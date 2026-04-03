// Conway governance (CIP-1694)
//
// Copied from torsten-ledger/src/state/governance.rs
// Implements complete governance ratification, voting, and enactment logic

use super::primitives::*;
use super::state::{credential_to_hash, GovernanceState, LedgerState, ProposalState};
use std::collections::HashMap;
use std::sync::Arc;

impl LedgerState {
    /// Check if we're in Conway bootstrap phase (protocol version 9).
    /// During bootstrap, all DRep voting thresholds are 0.
    pub(crate) fn is_bootstrap_phase(&self) -> bool {
        self.protocol_params.protocol_version_major == 9
    }

    /// Process a new governance proposal submission.
    ///
    /// Validates the proposal and stores it with an expiration epoch.
    /// Per CIP-1694, proposals expire after `gov_action_lifetime` epochs.
    pub fn process_proposal(
        &mut self,
        action_id: &GovActionId,
        procedure: &ProposalProcedure,
    ) -> Result<(), String> {
        // Validate prev_action_id chain per Haskell `prevActionAsExpected`
        if !prev_action_as_expected(&procedure.gov_action, &self.governance) {
            return Err("Invalid prev_action_id chain".to_string());
        }

        // Check constitution guardrails (if present)
        if let Some(ref constitution) = self.governance.constitution {
            if let Some(ref script_hash) = constitution.script_hash {
                // TODO: Validate proposal against constitution script
                // For now, we accept all proposals (constitution script validation
                // requires Plutus evaluation, which is stubbed)
                tracing::debug!(
                    "Constitution guardrails check skipped (script_hash: {})",
                    hex::encode(script_hash)
                );
            }
        }

        // Compute expiration epoch
        let expires_epoch = EpochNo(
            self.epoch
                .0
                .saturating_add(self.protocol_params.gov_action_lifetime),
        );

        // Store proposal state
        let proposal_state = ProposalState {
            procedure: procedure.clone(),
            proposed_epoch: self.epoch,
            expires_epoch,
            yes_votes: 0,
            no_votes: 0,
            abstain_votes: 0,
        };

        Arc::make_mut(&mut self.governance)
            .proposals
            .insert(*action_id, proposal_state);

        tracing::debug!(
            action_id = %hex::encode(&action_id.tx_hash),
            index = action_id.index,
            expires = expires_epoch.0,
            "Governance proposal submitted"
        );

        Ok(())
    }

    /// Process a vote on a governance action.
    ///
    /// Updates vote tallies and tracks DRep activity.
    pub fn process_vote(
        &mut self,
        voter: &Voter,
        action_id: &GovActionId,
        procedure: &VotingProcedure,
    ) {
        // Check proposal exists (already ratified/expired proposals are removed from the map)
        if !self.governance.proposals.contains_key(action_id) {
            tracing::debug!(
                action_id = %hex::encode(&action_id.tx_hash),
                "Vote on already-ratified or unknown proposal, ignoring"
            );
            return;
        }

        // Update DRep activity if this is a DRep vote
        if let Voter::DRep(cred) = voter {
            let drep_hash = credential_to_hash(cred);
            if let Some(drep) = Arc::make_mut(&mut self.governance).dreps.get_mut(&drep_hash) {
                drep.last_active_epoch = self.epoch;
                drep.active = true;
            }
        }

        // Store vote indexed by action_id
        Arc::make_mut(&mut self.governance)
            .votes_by_action
            .entry(*action_id)
            .or_insert_with(Vec::new)
            .push((voter.clone(), procedure.clone()));
    }

    /// Ratify governance proposals at epoch boundary.
    ///
    /// Per Haskell Ratify.hs:
    /// 1. Sort proposals by priority (NoConfidence > UpdateCommittee > ...)
    /// 2. Process proposals sequentially with state threading
    /// 3. Check voting thresholds per action type
    /// 4. Validate prev_action_id chain (updated as proposals are enacted)
    /// 5. Enact ratified proposals
    /// 6. Stop at first "delaying action" (NoConfidence, HardFork, UpdateCommittee, NewConstitution)
    ///
    /// Returns the list of ratified enactments to be applied immediately.
    pub fn ratify_proposals(&mut self) -> Vec<PendingEnactment> {
        // Build DRep power cache once (O(n) instead of per-proposal O(n))
        let (drep_power_cache, no_confidence_stake, _abstain_stake) =
            self.build_drep_power_cache();

        let total_drep_stake = self.compute_total_drep_stake();

        // Sort proposals by priority (lower number = higher priority)
        // Clone proposals so we don't hold a borrow during iteration
        let mut proposals: Vec<(GovActionId, ProposalState)> = self
            .governance
            .proposals
            .iter()
            .map(|(id, state)| (*id, state.clone()))
            .collect();
        proposals.sort_by_key(|(_, state)| gov_action_priority(&state.procedure.gov_action));

        let mut ratified = Vec::new();
        let mut ratified_ids = Vec::new();
        let mut enactments = Vec::new();
        let mut delayed = false;

        for (action_id, state) in &proposals {
            // Stop if a delaying action was already ratified
            if delayed {
                break;
            }

            // Check ratification thresholds
            if self.check_ratification(
                action_id,
                state,
                total_drep_stake,
                &drep_power_cache,
                no_confidence_stake,
            ) {
                // Validate prev_action_id chain (using CURRENT enacted state)
                if !prev_action_as_expected(&state.procedure.gov_action, &self.governance) {
                    tracing::debug!(
                        action_id = %hex::encode(&action_id.tx_hash),
                        "Proposal ratified but prev_action_id chain broken — skipped"
                    );
                    continue;
                }

                // Update enacted roots to reflect this action (for within-epoch chain validation:
                // sequential proposals in the same RATIFY round can depend on each other's roots).
                self.update_enacted_roots(&action_id, &state.procedure.gov_action);

                // Build enactment for immediate application (same epoch transition).
                // Haskell's Conway EPOCH STS does RATIFY → ENACT in the same boundary.
                let return_cred_hash = match &state.procedure.return_addr {
                    Credential::Key(hash) => *hash,
                    Credential::Script(hash) => *hash,
                };
                enactments.push(PendingEnactment {
                    action_id: *action_id,
                    gov_action: state.procedure.gov_action.clone(),
                    return_cred_hash,
                    deposit: state.procedure.deposit,
                });

                ratified.push((*action_id, state.clone()));
                ratified_ids.push(*action_id);

                let action_type = match &state.procedure.gov_action {
                    GovernanceAction::ParameterChange { .. } => "ParameterChange",
                    GovernanceAction::HardForkInitiation { .. } => "HardForkInitiation",
                    GovernanceAction::TreasuryWithdrawals { .. } => "TreasuryWithdrawals",
                    GovernanceAction::NoConfidence { .. } => "NoConfidence",
                    GovernanceAction::UpdateCommittee { .. } => "UpdateCommittee",
                    GovernanceAction::NewConstitution { .. } => "NewConstitution",
                    GovernanceAction::InfoAction => "InfoAction",
                };
                tracing::info!(
                    action_id = %hex::encode(&action_id.tx_hash),
                    action_type,
                    deposit = state.procedure.deposit.0,
                    return_cred = %hex::encode(&return_cred_hash[..28]),
                    proposed_epoch = state.proposed_epoch.0,
                    expires_epoch = state.expires_epoch.0,
                    "Governance proposal ratified"
                );

                // Check if this is a delaying action
                if is_delaying_action(&state.procedure.gov_action) {
                    delayed = true;
                }
            }
        }

        // Remove ratified proposals from pending
        for action_id in &ratified_ids {
            Arc::make_mut(&mut self.governance).proposals.remove(action_id);
            Arc::make_mut(&mut self.governance)
                .votes_by_action
                .remove(action_id);
        }

        // Update governance state
        Arc::make_mut(&mut self.governance).last_ratified = ratified;
        Arc::make_mut(&mut self.governance).last_ratify_delayed = delayed;

        enactments
    }

    /// Update enacted action roots after ratifying a proposal.
    fn update_enacted_roots(&mut self, action_id: &GovActionId, action: &GovernanceAction) {
        let gov = Arc::make_mut(&mut self.governance);
        match action {
            GovernanceAction::ParameterChange { .. } => {
                gov.enacted_pparam_update = Some(*action_id);
            }
            GovernanceAction::HardForkInitiation { .. } => {
                gov.enacted_hard_fork = Some(*action_id);
            }
            GovernanceAction::NoConfidence { .. } | GovernanceAction::UpdateCommittee { .. } => {
                gov.enacted_committee = Some(*action_id);
            }
            GovernanceAction::NewConstitution { .. } => {
                gov.enacted_constitution = Some(*action_id);
            }
            GovernanceAction::TreasuryWithdrawals { .. } | GovernanceAction::InfoAction => {
                // No enacted root for these action types
            }
        }
    }

    /// Check if a proposal meets the voting thresholds for ratification.
    ///
    /// During Conway bootstrap phase (protocol version 9), all DRep thresholds are 0.
    fn check_ratification(
        &self,
        action_id: &GovActionId,
        state: &ProposalState,
        _total_drep_stake: u64,
        drep_power_cache: &HashMap<Hash32, u64>,
        no_confidence_stake: u64,
    ) -> bool {
        // Count votes by voter type (uses pre-computed DRep power cache)
        // Per CIP-1694:
        // - DRep denominator = yes + no voted stake (abstain excluded)
        // - SPO denominator = total non-abstain SPO stake (accounts for default vote logic)
        let (drep_yes, drep_total, spo_yes, spo_effective_total, _cc_yes, _cc_total) = self
            .count_votes_by_type(
                action_id,
                &state.procedure.gov_action,
                drep_power_cache,
                no_confidence_stake,
            );

        let bootstrap = self.is_bootstrap_phase();

        match &state.procedure.gov_action {
            GovernanceAction::InfoAction => {
                // InfoAction can NEVER be ratified per Haskell.
                // Haskell's `votingDRepThresholdInternal` returns `NoVotingThreshold`
                // for InfoAction, which maps to `SNothing`, causing `dRepAccepted`
                // to return False unconditionally. InfoAction proposals can only expire.
                false
            }
            GovernanceAction::ParameterChange {
                update: protocol_param_update,
                ..
            } => {
                // Per CIP-1694: each affected DRep parameter group must independently
                // meet its own threshold. ALL affected group thresholds must be met.
                // SPO threshold = pvtPPSecurityGroup if any param is security-relevant
                // CC approval required
                let drep_met = if bootstrap {
                    true // All DRep thresholds are 0 during bootstrap
                } else {
                    pp_change_drep_all_groups_met(
                        protocol_param_update,
                        &self.protocol_params,
                        drep_yes,
                        drep_total,
                    )
                };
                let spo_met = if let Some(ref spo_threshold) =
                    pp_change_spo_threshold(protocol_param_update, &self.protocol_params)
                {
                    check_threshold(spo_yes, spo_effective_total, spo_threshold)
                } else {
                    true // No SPO vote required for non-security params
                };
                let cc_met = check_cc_approval(
                    action_id,
                    &self.governance,
                    self.epoch,
                    self.protocol_params.committee_min_size,
                    bootstrap,
                );
                drep_met && spo_met && cc_met
            }
            GovernanceAction::HardForkInitiation {
                protocol_version, ..
            } => {
                let rational_zero = Rational {
                    numerator: 0,
                    denominator: 1,
                };
                // DRep + SPO + CC all required
                let drep_threshold = if bootstrap {
                    rational_zero
                } else {
                    self.protocol_params.dvt_hard_fork.clone()
                };
                let spo_threshold = &self.protocol_params.pvt_hard_fork;
                let drep_met = check_threshold(drep_yes, drep_total, &drep_threshold);
                let spo_met = check_threshold(spo_yes, spo_effective_total, spo_threshold);
                let cc_met = check_cc_approval(
                    action_id,
                    &self.governance,
                    self.epoch,
                    self.protocol_params.committee_min_size,
                    bootstrap,
                );
                tracing::debug!(
                    action_id = %hex::encode(&action_id.tx_hash),
                    version = ?protocol_version,
                    bootstrap,
                    drep_yes, drep_total,
                    drep_threshold = drep_threshold.numerator as f64 / drep_threshold.denominator as f64,
                    drep_met,
                    spo_yes, spo_effective_total,
                    spo_threshold = spo_threshold.numerator as f64 / spo_threshold.denominator as f64,
                    spo_met,
                    cc_met,
                    "HardForkInitiation ratification check"
                );
                drep_met && spo_met && cc_met
            }
            GovernanceAction::NoConfidence { .. } => {
                let rational_zero = Rational {
                    numerator: 0,
                    denominator: 1,
                };
                // DRep + SPO, no CC (CC cannot vote on NoConfidence)
                let drep_threshold = if bootstrap {
                    rational_zero
                } else {
                    self.protocol_params.dvt_no_confidence.clone()
                };
                let spo_threshold = &self.protocol_params.pvt_motion_no_confidence;
                let drep_met = check_threshold(drep_yes, drep_total, &drep_threshold);
                let spo_met = check_threshold(spo_yes, spo_effective_total, spo_threshold);
                drep_met && spo_met
            }
            GovernanceAction::UpdateCommittee { .. } => {
                let rational_zero = Rational {
                    numerator: 0,
                    denominator: 1,
                };
                // DRep + SPO, no CC (CC cannot vote on UpdateCommittee)
                let (drep_threshold, spo_threshold) = if self.governance.no_confidence {
                    (
                        if bootstrap {
                            rational_zero
                        } else {
                            self.protocol_params.dvt_committee_no_confidence.clone()
                        },
                        &self.protocol_params.pvt_committee_no_confidence,
                    )
                } else {
                    (
                        if bootstrap {
                            rational_zero
                        } else {
                            self.protocol_params.dvt_committee_normal.clone()
                        },
                        &self.protocol_params.pvt_committee_normal,
                    )
                };
                let drep_met = check_threshold(drep_yes, drep_total, &drep_threshold);
                let spo_met = check_threshold(spo_yes, spo_effective_total, spo_threshold);
                drep_met && spo_met
            }
            GovernanceAction::NewConstitution { .. } => {
                let rational_zero = Rational {
                    numerator: 0,
                    denominator: 1,
                };
                // DRep + CC, no SPO
                let drep_threshold = if bootstrap {
                    rational_zero
                } else {
                    self.protocol_params.dvt_constitution.clone()
                };
                let drep_met = check_threshold(drep_yes, drep_total, &drep_threshold);
                let cc_met = check_cc_approval(
                    action_id,
                    &self.governance,
                    self.epoch,
                    self.protocol_params.committee_min_size,
                    bootstrap,
                );
                drep_met && cc_met
            }
            GovernanceAction::TreasuryWithdrawals { .. } => {
                let rational_zero = Rational {
                    numerator: 0,
                    denominator: 1,
                };
                // DRep + CC, no SPO
                let drep_threshold = if bootstrap {
                    rational_zero
                } else {
                    self.protocol_params.dvt_treasury_withdrawal.clone()
                };
                let drep_met = check_threshold(drep_yes, drep_total, &drep_threshold);
                let cc_met = check_cc_approval(
                    action_id,
                    &self.governance,
                    self.epoch,
                    self.protocol_params.committee_min_size,
                    bootstrap,
                );
                drep_met && cc_met
            }
        }
    }

    /// Count stake-weighted votes by voter type for a specific governance action.
    ///
    /// Per Haskell `dRepAcceptedRatio` / `spoAcceptedRatio`:
    /// - DRep denominator = total active DRep-delegated stake - abstain stake
    ///   (non-voting active DReps count as implicit No in denominator)
    /// - SPO: iterates ALL mark pools; non-voting SPOs get a default vote based on
    ///   the pool operator's DRep delegation (AlwaysAbstain → excluded from denominator,
    ///   AlwaysNoConfidence → Yes on NoConfidence else No, otherwise → No)
    /// - AlwaysNoConfidence stake counts as Yes for NoConfidence, No otherwise
    /// - AlwaysAbstain stake is excluded from both numerator and denominator
    /// - Inactive/expired DReps are excluded (handled by drep_power_cache)
    ///
    /// Returns (drep_yes, drep_total, spo_yes, spo_effective_total, cc_yes, cc_total)
    /// where spo_effective_total is the SPO denominator (total non-abstain SPO stake).
    pub(crate) fn count_votes_by_type(
        &self,
        action_id: &GovActionId,
        action: &GovernanceAction,
        drep_power_cache: &HashMap<Hash32, u64>,
        no_confidence_stake: u64,
    ) -> (u64, u64, u64, u64, u64, u64) {
        let mut cc_yes = 0u64;
        let mut cc_total = 0u64;

        let is_no_confidence = matches!(action, GovernanceAction::NoConfidence { .. });
        let is_hard_fork = matches!(action, GovernanceAction::HardForkInitiation { .. });
        let bootstrap = self.is_bootstrap_phase();

        // Build per-voter vote maps for this specific action
        let mut drep_votes: HashMap<Hash32, Vote> = HashMap::new();
        let mut spo_votes: HashMap<Hash28, Vote> = HashMap::new();

        let empty = vec![];
        let action_votes = self
            .governance
            .votes_by_action
            .get(action_id)
            .unwrap_or(&empty);

        for (voter, procedure) in action_votes {
            match voter {
                Voter::DRep(cred) => {
                    let drep_hash = credential_to_hash(cred);
                    drep_votes.insert(drep_hash, procedure.vote);
                }
                Voter::StakePool(pool_hash) => {
                    spo_votes.insert(*pool_hash, procedure.vote);
                }
                Voter::ConstitutionalCommittee(_) => {
                    cc_total += 1;
                    if procedure.vote == Vote::Yes {
                        cc_yes += 1;
                    }
                }
            }
        }

        // SPO vote counting: iterate ALL pools in the mark snapshot (not just voters).
        // Per Haskell `spoAccepted` / `spoVotingPower`:
        // - Voted pools: use their explicit vote.
        // - Bootstrap phase non-voted: Abstain (excluded from denominator).
        // - Non-bootstrap non-voted: check pool operator's DRep delegation for default vote.
        //   * AlwaysAbstain → Abstain (excluded from denominator)
        //   * AlwaysNoConfidence → Yes on NoConfidence, No on everything else
        //   * Other (or no delegation) → No (in denominator, not numerator)
        let mut spo_yes = 0u64;
        let mut spo_effective_total = 0u64; // denominator = total non-abstain SPO stake

        if let Some(ref mark) = self.snapshots.mark {
            for (pool_id, pool_stake) in &mark.pool_stake {
                let stake = pool_stake.0;
                let vote = if let Some(&v) = spo_votes.get(pool_id) {
                    v
                } else if is_hard_fork {
                    // Per Haskell spoAcceptedRatio: for HardForkInitiation, non-voting SPOs
                    // ALWAYS count as No regardless of bootstrap phase or DRep delegation.
                    // This differs from all other proposal types.
                    Vote::No
                } else if bootstrap {
                    // Bootstrap phase, non-HardFork: non-voting SPOs count as Abstain
                    Vote::Abstain
                } else {
                    // Post-bootstrap, non-HardFork: check pool operator's DRep delegation
                    let pool_reg = mark.pool_params.get(pool_id)
                        .or_else(|| self.pool_params.get(pool_id));
                    let op_cred = pool_reg.map(|p| Self::reward_account_to_hash(&p.reward_account));
                    match op_cred.and_then(|h| self.governance.vote_delegations.get(&h)) {
                        Some(DRep::AlwaysAbstain) => Vote::Abstain,
                        Some(DRep::AlwaysNoConfidence) => {
                            if is_no_confidence { Vote::Yes } else { Vote::No }
                        }
                        _ => Vote::No,
                    }
                };
                match vote {
                    Vote::Yes => {
                        spo_yes += stake;
                        spo_effective_total += stake;
                    }
                    Vote::No => {
                        spo_effective_total += stake;
                    }
                    Vote::Abstain => {
                        // Excluded from both numerator and denominator
                    }
                }
            }
        } else {
            // Fallback when mark snapshot not available: use only explicit voters
            for (pool_hash, vote) in &spo_votes {
                let pool_stake = self.compute_spo_voting_power(pool_hash);
                spo_effective_total += pool_stake;
                if *vote == Vote::Yes {
                    spo_yes += pool_stake;
                }
            }
        }

        // Compute DRep ratio per Haskell `dRepAcceptedRatio`:
        // Iterate ALL active DRep stake (from drep_power_cache), not just voters.
        // Non-voting DReps are implicit No (in denominator, not numerator).
        let mut drep_yes = 0u64;
        let mut drep_abstain = 0u64;
        let mut drep_total_all = 0u64;

        for (drep_hash, &power) in drep_power_cache {
            drep_total_all += power;
            match drep_votes.get(drep_hash) {
                Some(Vote::Yes) => {
                    drep_yes += power;
                }
                Some(Vote::Abstain) => {
                    drep_abstain += power;
                }
                Some(Vote::No) | None => {
                    // Voted No or didn't vote: implicit No (already in total)
                }
            }
        }

        // Handle AlwaysNoConfidence stake per CIP-1694:
        // - For NoConfidence actions: counts as Yes
        // - For all other actions: counts as No (in denominator, not numerator)
        // AlwaysNoConfidence is always in the denominator.
        if no_confidence_stake > 0 {
            drep_total_all += no_confidence_stake;
            if is_no_confidence {
                drep_yes += no_confidence_stake;
            }
        }

        // AlwaysAbstain: already excluded from drep_power_cache (handled in build_drep_power_cache)

        // DRep denominator = total active stake - abstain stake
        let drep_total = drep_total_all.saturating_sub(drep_abstain);

        (drep_yes, drep_total, spo_yes, spo_effective_total, cc_yes, cc_total)
    }

    /// Get the total stake for a credential: UTxO stake + reward account balance.
    pub(crate) fn credential_stake(&self, cred_hash: &Hash32) -> u64 {
        let utxo = self
            .stake_distribution
            .stake_map
            .get(cred_hash)
            .map(|s| s.0)
            .unwrap_or(0);
        let reward = self
            .reward_accounts
            .get(cred_hash)
            .map(|s| s.0)
            .unwrap_or(0);
        utxo + reward
    }

    /// Build a cache of DRep voting power for ratification.
    ///
    /// Returns the DRep power snapshot frozen at the PREVIOUS epoch boundary.
    /// This matches Haskell's `setFreshDRepPulsingState`: ratification at epoch N uses
    /// the drepDistr snapshotted at epoch N-1.
    ///
    /// Falls back to live computation on the first Conway epoch (no snapshot yet).
    /// Returns (drep_power_cache, always_no_confidence_stake, always_abstain_stake).
    pub(crate) fn build_drep_power_cache(&self) -> (HashMap<Hash32, u64>, u64, u64) {
        // Return stored snapshot if non-empty (set at previous epoch boundary)
        let snap = &self.governance.drep_power_snapshot;
        let no_conf = self.governance.drep_no_confidence_snapshot;
        let abstain = self.governance.drep_abstain_snapshot;
        if !snap.is_empty() || no_conf > 0 || abstain > 0 {
            return (snap.clone(), no_conf, abstain);
        }

        // Fallback: compute live for first Conway epoch (bootstrap — no snapshot yet).
        // Uses the full formula: UTxO stake + reward balance + governance proposal deposits.
        let mut cache: HashMap<Hash32, u64> = HashMap::new();
        let mut no_confidence_stake = 0u64;
        let mut abstain_stake = 0u64;
        for (stake_cred, drep) in &self.governance.vote_delegations {
            let stake = self.credential_drep_stake(stake_cred);
            match drep {
                DRep::KeyHash(h) => {
                    if self.governance.dreps.get(h).is_some_and(|d| d.active) {
                        *cache.entry(*h).or_default() += stake;
                    }
                }
                DRep::ScriptHash(h) => {
                    if self.governance.dreps.get(h).is_some_and(|d| d.active) {
                        *cache.entry(*h).or_default() += stake;
                    }
                }
                DRep::AlwaysNoConfidence => no_confidence_stake += stake,
                DRep::AlwaysAbstain => abstain_stake += stake,
            }
        }
        (cache, no_confidence_stake, abstain_stake)
    }

    /// Compute the full DRep stake for a credential: UTxO + rewards + governance proposal deposits.
    ///
    /// Matches Haskell's `computeDRepDistr` formula where:
    ///   stake[cred] = instantStake[cred] + rewardBalance[cred] + proposalDeposits[cred]
    pub(crate) fn credential_drep_stake(&self, cred_hash: &Hash32) -> u64 {
        let utxo = self.stake_distribution.stake_map.get(cred_hash).map(|s| s.0).unwrap_or(0);
        let reward = self.reward_accounts.get(cred_hash).map(|s| s.0).unwrap_or(0);
        let gov_deps = self.deposit_tracker.governance_deposits_by_return_cred(cred_hash);
        utxo + reward + gov_deps
    }

    /// Compute total active DRep-delegated stake across all DReps.
    /// Excludes stake delegated to inactive DReps.
    /// Includes stake delegated to Abstain and NoConfidence (they are part of total DRep ecosystem).
    pub(crate) fn compute_total_drep_stake(&self) -> u64 {
        let mut total = 0u64;
        for (stake_cred, drep) in &self.governance.vote_delegations {
            let stake = self.credential_stake(stake_cred);
            match drep {
                DRep::AlwaysAbstain | DRep::AlwaysNoConfidence => {
                    total += stake;
                }
                DRep::KeyHash(h) => {
                    if self.governance.dreps.get(h).is_some_and(|d| d.active) {
                        total += stake;
                    }
                }
                DRep::ScriptHash(h) => {
                    if self.governance.dreps.get(h).is_some_and(|d| d.active) {
                        total += stake;
                    }
                }
            }
        }
        total.max(1) // Ensure non-zero to avoid division by zero
    }

    /// Compute the voting power of a stake pool: total delegated stake.
    ///
    /// Per CIP-1694 and the Haskell Ratify.hs implementation, SPO voting power
    /// is measured against the **mark** snapshot (the stake distribution captured
    /// at the beginning of the current epoch, immediately before the epoch
    /// transition). Using `set` (two epochs prior) would delay the effect of new
    /// delegations by an extra epoch compared to the specification.
    ///
    /// Reference: Haskell `spoVotingPower` in `Cardano.Ledger.Conway.Governance.Procedures`
    /// uses `ssStakeMarkPoolDistr` (the mark pool distribution).
    pub(crate) fn compute_spo_voting_power(&self, pool_id: &Hash28) -> u64 {
        // Use the "mark" snapshot (current epoch stake) for voting power — CIP-1694 spec.
        if let Some(ref snapshot) = self.snapshots.mark {
            if let Some(stake) = snapshot.pool_stake.get(pool_id) {
                return stake.0;
            }
        }
        // Fallback: compute from current delegations (UTxO + rewards).
        // This path is taken during the first two epochs before snapshots are populated.
        tracing::debug!(
            "SPO voting power: falling back to O(n) delegation scan — snapshot not available"
        );
        let mut total = 0u64;
        for (stake_cred, delegated_pool) in self.delegations.iter() {
            if delegated_pool == pool_id {
                total += self.credential_stake(stake_cred);
            }
        }
        total
    }

    /// Compute total active SPO stake across all pools.
    /// Used as the denominator for SPO voting thresholds.
    ///
    /// Per CIP-1694, the denominator is derived from the **mark** snapshot
    /// (same snapshot used for individual pool voting power) to keep the
    /// ratio consistent. Haskell uses `ssStakeMarkPoolDistr` for both the
    /// numerator (per-pool power) and this denominator.
    #[allow(dead_code)]
    fn compute_total_spo_stake(&self) -> u64 {
        // Use "mark" snapshot if available (current epoch), else fall back.
        if let Some(ref snapshot) = self.snapshots.mark {
            let total: u64 = snapshot
                .pool_stake
                .values()
                .fold(0u64, |acc, s| acc.saturating_add(s.0));
            return total.max(1);
        }
        // Fallback: sum all pool stake from current delegations (UTxO + rewards).
        // This path is taken during the first two epochs before snapshots are populated.
        let mut total = 0u64;
        for stake_cred in self.delegations.keys() {
            total = total.saturating_add(self.credential_stake(stake_cred));
        }
        total.max(1)
    }

    /// Enact a ratified governance action by applying its effects
    pub(crate) fn enact_gov_action(&mut self, action: &GovernanceAction) {
        match action {
            GovernanceAction::ParameterChange { update, .. } => {
                let is_conway = self.governance.conway_cur_params.is_some();
                if is_conway {
                    // In Conway era: save old conway_cur_params as prevPParams, apply to protocol_params,
                    // then sync conway_cur_params to reflect the update.
                    self.prev_protocol_params = self.governance.conway_cur_params.as_deref().cloned();
                    if let Err(e) = self.apply_protocol_param_update(update) {
                        tracing::warn!(
                            error = %e,
                            "Governance protocol parameter update rejected"
                        );
                        self.prev_protocol_params = None;
                    } else {
                        // Sync conway_cur_params to the updated protocol_params
                        Arc::make_mut(&mut self.governance).conway_cur_params =
                            Some(Box::new(self.protocol_params.clone()));
                        tracing::debug!("Governance protocol parameters updated (Conway)");
                    }
                } else {
                    // Babbage era: update protocol_params directly
                    self.prev_protocol_params = Some(self.protocol_params.clone());
                    if let Err(e) = self.apply_protocol_param_update(update) {
                        tracing::warn!(
                            error = %e,
                            "Governance protocol parameter update rejected"
                        );
                        self.prev_protocol_params = None;
                    } else {
                        tracing::debug!("Governance protocol parameters updated");
                    }
                }
            }
            GovernanceAction::HardForkInitiation {
                protocol_version, ..
            } => {
                self.protocol_params.protocol_version_major = protocol_version.0;
                self.protocol_params.protocol_version_minor = protocol_version.1;
                // Also update conway_cur_params so curPParams in dumps reflects the new version
                if let Some(ref mut cp) = Arc::make_mut(&mut self.governance).conway_cur_params {
                    cp.protocol_version_major = protocol_version.0;
                    cp.protocol_version_minor = protocol_version.1;
                }
                tracing::debug!(
                    "Governance hard fork initiated (protocol version {}.{})",
                    protocol_version.0,
                    protocol_version.1
                );
            }
            GovernanceAction::TreasuryWithdrawals { withdrawals, .. } => {
                // Compute total first and cap at available treasury
                let requested: u64 = withdrawals
                    .iter()
                    .fold(0u64, |acc, (_, a)| acc.saturating_add(a.0));
                let available = self.treasury.0;
                if requested > available {
                    tracing::warn!(
                        "Treasury withdrawal capped: requested {} but only {} available",
                        requested,
                        available
                    );
                }
                let mut total = 0u64;
                for (cred, amount) in withdrawals {
                    let actual = amount.0.min(self.treasury.0);
                    self.treasury.0 = self.treasury.0.saturating_sub(actual);
                    total += actual;
                    // Credit the withdrawal to the recipient's reward account
                    if actual > 0 {
                        let key = match cred {
                            Credential::Key(hash) => *hash,
                            Credential::Script(hash) => *hash,
                        };
                        *Arc::make_mut(&mut self.reward_accounts)
                            .entry(key)
                            .or_insert(Lovelace(0)) += Lovelace(actual);
                    }
                }
                tracing::debug!(
                    "Governance treasury withdrawal: {} lovelace to {} accounts",
                    total,
                    withdrawals.len()
                );
            }
            GovernanceAction::NoConfidence { .. } => {
                // No confidence motion: dissolve the committee entirely.
                // Per Haskell: `ensCommittee = SNothing` — committee is set to Nothing.
                let gov = Arc::make_mut(&mut self.governance);
                gov.committee_hot_keys.clear();
                gov.committee_expiration.clear();
                gov.committee_threshold = None; // Match Haskell SNothing
                gov.no_confidence = true;
                tracing::debug!("Governance no confidence motion enacted, committee disbanded");
            }
            GovernanceAction::UpdateCommittee {
                members_to_remove,
                members_to_add,
                quorum,
                ..
            } => {
                // Remove specified members
                for cred in members_to_remove {
                    let key = match cred {
                        Credential::Key(hash) => *hash,
                        Credential::Script(hash) => *hash,
                    };
                    Arc::make_mut(&mut self.governance)
                        .committee_hot_keys
                        .remove(&key);
                    Arc::make_mut(&mut self.governance)
                        .committee_expiration
                        .remove(&key);
                    Arc::make_mut(&mut self.governance)
                        .committee_resigned
                        .remove(&key);
                }
                // Add new members with expiration epochs
                for (cred, expiration_epoch) in members_to_add {
                    let key = match cred {
                        Credential::Key(hash) => *hash,
                        Credential::Script(hash) => *hash,
                    };
                    Arc::make_mut(&mut self.governance)
                        .committee_expiration
                        .insert(key, *expiration_epoch);
                    // Hot key auth comes via CommitteeHotAuth certificates
                }
                // Store the new committee quorum threshold
                Arc::make_mut(&mut self.governance).committee_threshold = Some(quorum.clone());
                // UpdateCommittee restores confidence
                Arc::make_mut(&mut self.governance).no_confidence = false;
                tracing::debug!(
                    "Governance committee updated: {} removed, {} added, threshold={}/{}",
                    members_to_remove.len(),
                    members_to_add.len(),
                    quorum.numerator,
                    quorum.denominator,
                );
            }
            GovernanceAction::NewConstitution { constitution, .. } => {
                Arc::make_mut(&mut self.governance).constitution = Some(constitution.clone());
                tracing::debug!(
                    "Governance new constitution enacted (script_hash: {:?})",
                    constitution.script_hash.as_ref().map(hex::encode)
                );
            }
            GovernanceAction::InfoAction => {
                // Info actions have no on-chain effect
                tracing::debug!("Info action ratified (no on-chain effect)");
            }
        }
    }

    /// Apply a protocol parameter update.
    ///
    /// Validates and applies parameter changes from a governance action or
    /// a pre-Conway PPUP proposal.
    pub fn apply_protocol_param_update(&mut self, update: &ProtocolParamUpdate) -> Result<(), String> {
        // Network group
        if let Some(v) = update.min_fee_a { self.protocol_params.min_fee_a = v; }
        if let Some(v) = update.min_fee_b { self.protocol_params.min_fee_b = v; }
        if let Some(v) = update.max_block_body_size { self.protocol_params.max_block_body_size = v; }
        if let Some(v) = update.max_transaction_size { self.protocol_params.max_transaction_size = v; }
        if let Some(v) = update.max_block_header_size { self.protocol_params.max_block_header_size = v; }
        if let Some((major, minor)) = update.protocol_version {
            self.protocol_params.protocol_version_major = major;
            self.protocol_params.protocol_version_minor = minor;
            // Babbage hard fork (6→7): d is removed from the protocol parameter type.
            // If d is still non-zero when Babbage takes effect, force it to 0.
            if major >= 7 && self.protocol_params.decentralization.numerator != 0 {
                tracing::info!(
                    "Babbage HF (protocol version {}→{}.{}): forcing d=0 (was {}/{})",
                    major - 1, major, minor,
                    self.protocol_params.decentralization.numerator,
                    self.protocol_params.decentralization.denominator
                );
                self.protocol_params.decentralization = Rational { numerator: 0, denominator: 1 };
            }
        }
        // Economic group
        if let Some(v) = update.key_deposit { self.protocol_params.key_deposit = v; }
        if let Some(v) = update.pool_deposit { self.protocol_params.pool_deposit = v; }
        if let Some(v) = update.min_pool_cost {
            self.protocol_params.min_pool_cost = v;
            self.protocol_params.min_pool_cost_lovelace = v;
        }
        if let Some(v) = update.rho { self.protocol_params.rho = v; }
        if let Some(v) = update.tau { self.protocol_params.tau = v; }
        if let Some(v) = update.a0 { self.protocol_params.a0 = v; }
        // Technical group
        if let Some(v) = update.n_opt { self.protocol_params.n_opt = v; }
        if let Some(v) = update.e_max { self.protocol_params.e_max = v; }
        if let Some(v) = update.decentralization { self.protocol_params.decentralization = v; }
        // Conway governance group
        if let Some(v) = update.drep_deposit { self.protocol_params.drep_deposit = v; }
        if let Some(v) = update.drep_activity { self.protocol_params.drep_activity_period = v; }
        if let Some(v) = update.gov_action_lifetime { self.protocol_params.gov_action_lifetime = v; }
        if let Some(v) = update.gov_action_deposit { self.protocol_params.gov_action_deposit = v; }
        if let Some(v) = update.committee_min_size { self.protocol_params.committee_min_size = v; }
        if let Some(v) = update.committee_max_term_length { self.protocol_params.committee_max_term_length = v; }
        if let Some(v) = update.min_fee_ref_script_cost_per_byte.clone() { self.protocol_params.min_fee_ref_script_cost_per_byte = v.numerator / v.denominator.max(1); }
        // DRep voting thresholds
        if let Some(v) = update.dvt_motion_no_confidence.clone() { self.protocol_params.dvt_motion_no_confidence = v; }
        if let Some(v) = update.dvt_committee_normal.clone() { self.protocol_params.dvt_committee_normal = v; }
        if let Some(v) = update.dvt_committee_no_confidence.clone() { self.protocol_params.dvt_committee_no_confidence = v; }
        if let Some(v) = update.dvt_update_to_constitution.clone() { self.protocol_params.dvt_constitution = v; }
        if let Some(v) = update.dvt_hard_fork_initiation.clone() { self.protocol_params.dvt_hard_fork = v; }
        if let Some(v) = update.dvt_pp_network_group.clone() { self.protocol_params.dvt_pp_network_group = v; }
        if let Some(v) = update.dvt_pp_economic_group.clone() { self.protocol_params.dvt_pp_economic_group = v; }
        if let Some(v) = update.dvt_pp_technical_group.clone() { self.protocol_params.dvt_pp_technical_group = v; }
        if let Some(v) = update.dvt_pp_gov_group.clone() { self.protocol_params.dvt_pp_gov_group = v; }
        if let Some(v) = update.dvt_treasury_withdrawal.clone() { self.protocol_params.dvt_treasury_withdrawal = v; }
        // SPO voting thresholds
        if let Some(v) = update.pvt_motion_no_confidence.clone() { self.protocol_params.pvt_motion_no_confidence = v; }
        if let Some(v) = update.pvt_committee_normal.clone() { self.protocol_params.pvt_committee_normal = v; }
        if let Some(v) = update.pvt_committee_no_confidence.clone() { self.protocol_params.pvt_committee_no_confidence = v; }
        if let Some(v) = update.pvt_hard_fork_initiation.clone() { self.protocol_params.pvt_hard_fork = v; }
        if let Some(v) = update.pvt_pp_security_group.clone() { self.protocol_params.pvt_pp_security_group = v; }

        tracing::debug!("Protocol parameters updated");
        Ok(())
    }
}

/// DRep voting group for protocol parameter classification per CIP-1694.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DRepPPGroup {
    Network,
    Economic,
    Technical,
    Gov,
}

/// Whether SPOs can vote on a parameter change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum StakePoolPPGroup {
    Security,
    NoVote,
}

/// Classification of a protocol parameter: (DRepPPGroup, StakePoolPPGroup).
/// Matches Haskell cardano-ledger Conway `PPGroups` exactly.
pub(crate) type PPGroup = (DRepPPGroup, StakePoolPPGroup);

/// Determine which PP groups are modified by a ProtocolParamUpdate.
///
/// Each parameter belongs to exactly one (DRepPPGroup, StakePoolPPGroup) pair.
/// Classification matches Haskell cardano-ledger Conway ConwayPParams field tags.
///
/// Determine which PP groups are modified by a ProtocolParamUpdate.
///
/// Each parameter belongs to exactly one (DRepPPGroup, StakePoolPPGroup) pair.
/// Classification matches Haskell cardano-ledger Conway ConwayPParams field tags.
pub(crate) fn modified_pp_groups(ppu: &ProtocolParamUpdate) -> Vec<PPGroup> {
    use DRepPPGroup::*;
    use StakePoolPPGroup::*;

    let mut groups = Vec::new();

    // Network + Security
    if ppu.max_block_body_size.is_some() { groups.push((Network, Security)); }
    if ppu.max_transaction_size.is_some() { groups.push((Network, Security)); }
    if ppu.max_block_header_size.is_some() { groups.push((Network, Security)); }
    if ppu.protocol_version.is_some() { groups.push((Network, Security)); }

    // Economic + Security
    if ppu.min_fee_a.is_some() { groups.push((Economic, Security)); }
    if ppu.min_fee_b.is_some() { groups.push((Economic, Security)); }
    if ppu.key_deposit.is_some() { groups.push((Economic, NoVote)); }
    if ppu.pool_deposit.is_some() { groups.push((Economic, NoVote)); }
    if ppu.min_pool_cost.is_some() { groups.push((Economic, NoVote)); }
    if ppu.rho.is_some() { groups.push((Economic, NoVote)); }
    if ppu.tau.is_some() { groups.push((Economic, NoVote)); }
    if ppu.a0.is_some() { groups.push((Economic, NoVote)); }

    // Technical + Security
    if ppu.n_opt.is_some() { groups.push((Technical, Security)); }
    if ppu.e_max.is_some() { groups.push((Technical, NoVote)); }
    if ppu.decentralization.is_some() { groups.push((Technical, Security)); }

    // Economic + NoVote (Conway)
    if ppu.min_fee_ref_script_cost_per_byte.is_some() { groups.push((Economic, NoVote)); }

    // Governance group (Conway) — NoVote for SPOs
    if ppu.drep_deposit.is_some() { groups.push((Gov, NoVote)); }
    if ppu.drep_activity.is_some() { groups.push((Gov, NoVote)); }
    if ppu.gov_action_lifetime.is_some() { groups.push((Gov, NoVote)); }
    if ppu.gov_action_deposit.is_some() { groups.push((Gov, NoVote)); }
    if ppu.committee_min_size.is_some() { groups.push((Gov, NoVote)); }
    if ppu.committee_max_term_length.is_some() { groups.push((Gov, NoVote)); }
    if ppu.dvt_motion_no_confidence.is_some() { groups.push((Gov, NoVote)); }
    if ppu.dvt_committee_normal.is_some() { groups.push((Gov, NoVote)); }
    if ppu.dvt_committee_no_confidence.is_some() { groups.push((Gov, NoVote)); }
    if ppu.dvt_update_to_constitution.is_some() { groups.push((Gov, NoVote)); }
    if ppu.dvt_hard_fork_initiation.is_some() { groups.push((Gov, NoVote)); }
    if ppu.dvt_pp_network_group.is_some() { groups.push((Gov, NoVote)); }
    if ppu.dvt_pp_economic_group.is_some() { groups.push((Gov, NoVote)); }
    if ppu.dvt_pp_technical_group.is_some() { groups.push((Gov, NoVote)); }
    if ppu.dvt_pp_gov_group.is_some() { groups.push((Gov, NoVote)); }
    if ppu.dvt_treasury_withdrawal.is_some() { groups.push((Gov, NoVote)); }
    if ppu.pvt_motion_no_confidence.is_some() { groups.push((Gov, NoVote)); }
    if ppu.pvt_committee_normal.is_some() { groups.push((Gov, NoVote)); }
    if ppu.pvt_committee_no_confidence.is_some() { groups.push((Gov, NoVote)); }
    if ppu.pvt_hard_fork_initiation.is_some() { groups.push((Gov, NoVote)); }
    if ppu.pvt_pp_security_group.is_some() { groups.push((Gov, NoVote)); }

    groups
}

/// Check that ALL affected DRep parameter group thresholds are independently met.
///
/// Per CIP-1694 / Haskell `pparamsUpdateThreshold`: each affected parameter group
/// has its own DRep voting threshold. A ParameterChange is ratified only if the
/// DRep vote ratio meets the threshold for EVERY affected group independently.
pub(crate) fn pp_change_drep_all_groups_met(
    ppu: &ProtocolParamUpdate,
    params: &ProtocolParameters,
    drep_yes: u64,
    drep_total: u64,
) -> bool {
    let groups = modified_pp_groups(ppu);
    // Collect unique DRep groups (avoid checking the same group multiple times)
    let mut seen = std::collections::HashSet::new();
    for (drep_group, _) in &groups {
        if !seen.insert(*drep_group) {
            continue;
        }
        let threshold = match drep_group {
            DRepPPGroup::Network => &params.dvt_pp_network_group,
            DRepPPGroup::Economic => &params.dvt_pp_economic_group,
            DRepPPGroup::Technical => &params.dvt_pp_technical_group,
            DRepPPGroup::Gov => &params.dvt_pp_gov_group,
        };
        if !check_threshold(drep_yes, drep_total, threshold) {
            return false;
        }
    }
    true
}

/// Determine if SPOs can vote on a ParameterChange, and if so, return the threshold.
///
/// Per Haskell `votingStakePoolThresholdInternal`: SPOs vote with pvtPPSecurityGroup
/// if ANY modified parameter is tagged SecurityGroup. Otherwise SPOs cannot vote.
pub(crate) fn pp_change_spo_threshold(
    ppu: &ProtocolParamUpdate,
    params: &ProtocolParameters,
) -> Option<Rational> {
    let groups = modified_pp_groups(ppu);
    let has_security = groups
        .iter()
        .any(|(_, spo)| *spo == StakePoolPPGroup::Security);
    if has_security {
        Some(params.pvt_pp_security_group.clone())
    } else {
        None
    }
}

pub(crate) fn check_threshold(yes: u64, total: u64, threshold: &Rational) -> bool {
    // A zero threshold always passes (e.g., DRep thresholds during Conway bootstrap)
    if threshold.numerator == 0 {
        return true;
    }
    if total == 0 {
        return false;
    }
    // Exact integer comparison: yes/total >= numerator/denominator
    // ⟺ yes * denominator >= numerator * total (using u128 to avoid overflow)
    let lhs = (yes as u128) * (threshold.denominator as u128);
    let rhs = (threshold.numerator as u128) * (total as u128);
    lhs >= rhs
}

/// Check if the constitutional committee has approved a governance action.
///
/// Per Haskell `committeeAccepted` / `committeeAcceptedRatio`:
/// - Iterate ALL committee members (from committee_expiration, which tracks membership)
/// - Expired members: excluded (treated as abstain)
/// - Members without hot keys (unregistered): excluded (treated as abstain)
/// - Resigned members: excluded (treated as abstain)
/// - Active members who didn't vote: counted as NO
/// - Active members who voted Abstain: excluded from ratio
/// - Active members who voted Yes: yes / Active members who voted No: no
/// - Ratio = yes_count / (yes_count + no_count) compared against committee_threshold
///
/// During bootstrap (protocol version 9), committeeMinSize check is skipped.
/// Post-bootstrap, if active_size < committeeMinSize, CC blocks ratification.
pub(crate) fn check_cc_approval(
    action_id: &GovActionId,
    governance: &GovernanceState,
    current_epoch: EpochNo,
    committee_min_size: u64,
    bootstrap: bool,
) -> bool {
    // Get committee quorum threshold
    let threshold = match &governance.committee_threshold {
        Some(t) => t,
        None => {
            // No committee exists — CC vote fails (blocks ratification)
            return false;
        }
    };

    // If threshold is 0, auto-approve
    if threshold.numerator == 0 {
        return true;
    }

    // Collect CC votes for this action indexed by hot credential
    let mut cc_votes: HashMap<Hash32, Vote> = HashMap::new();
    let empty = vec![];
    let action_votes = governance.votes_by_action.get(action_id).unwrap_or(&empty);
    for (voter, procedure) in action_votes {
        if let Voter::ConstitutionalCommittee(cred) = voter {
            let hot_key = match cred {
                Credential::Key(hash) => *hash,
                Credential::Script(hash) => *hash,
            };
            cc_votes.insert(hot_key, procedure.vote);
        }
    }

    // Iterate all committee members and compute the ratio
    let mut yes_count = 0u64;
    let mut total_excluding_abstain = 0u64;
    let mut active_size = 0u64;

    for (cold_key, expiry) in &governance.committee_expiration {
        // Expired members: excluded (treated as abstain)
        // Per Haskell: `currentEpoch > validUntil` means expired.
        // Members are active through their expiry epoch (inclusive).
        if current_epoch > *expiry {
            continue;
        }

        // Check if member has a registered hot key
        let hot_key = match governance.committee_hot_keys.get(cold_key) {
            Some(hk) => hk,
            None => continue, // No hot key: excluded (treated as abstain)
        };

        // Resigned members: excluded (treated as abstain)
        if governance.committee_resigned.contains_key(cold_key) {
            continue;
        }

        active_size += 1;

        // Look up vote by hot credential
        match cc_votes.get(hot_key) {
            Some(Vote::Yes) => {
                yes_count += 1;
                total_excluding_abstain += 1;
            }
            Some(Vote::Abstain) => {
                // Abstain: excluded from ratio
            }
            Some(Vote::No) | None => {
                // Voted No or didn't vote: counts as No
                total_excluding_abstain += 1;
            }
        }
    }

    // Check committeeMinSize (skipped during bootstrap per Haskell spec)
    if !bootstrap && active_size < committee_min_size {
        return false;
    }

    // If no committee members exist at all
    if active_size == 0 {
        return false;
    }

    // If all active members abstained, ratio is 0
    if total_excluding_abstain == 0 {
        tracing::debug!(
            action_id = %hex::encode(&action_id.tx_hash),
            active_size, yes_count, total_excluding_abstain,
            threshold = threshold.numerator as f64 / threshold.denominator as f64,
            cc_voters = cc_votes.len(),
            committee_members = governance.committee_expiration.len(),
            hot_keys = governance.committee_hot_keys.len(),
            "CC approval check: all active members abstained"
        );
        return false;
    }

    // Exact comparison: yes_count / total_excluding_abstain >= threshold
    let result = check_threshold(yes_count, total_excluding_abstain, threshold);
    if !result {
        tracing::debug!(
            action_id = %hex::encode(&action_id.tx_hash),
            active_size, yes_count, total_excluding_abstain,
            threshold = threshold.numerator as f64 / threshold.denominator as f64,
            ratio = yes_count as f64 / total_excluding_abstain as f64,
            result,
            cc_voters = cc_votes.len(),
            committee_members = governance.committee_expiration.len(),
            hot_keys = governance.committee_hot_keys.len(),
            "CC approval check failed"
        );
    }
    result
}

/// Check that a proposal's `prev_action_id` matches the last enacted action of the same
/// governance purpose. Per Haskell `prevActionAsExpected` in Ratify.hs.
///
/// NoConfidence and UpdateCommittee share the `Committee` purpose.
/// TreasuryWithdrawals and InfoAction have no prev_action_id chain (always pass).
pub(crate) fn prev_action_as_expected(
    action: &GovernanceAction,
    governance: &GovernanceState,
) -> bool {
    match action {
        GovernanceAction::ParameterChange { prev_action_id, .. } => {
            *prev_action_id == governance.enacted_pparam_update
        }
        GovernanceAction::HardForkInitiation { prev_action_id, .. } => {
            *prev_action_id == governance.enacted_hard_fork
        }
        GovernanceAction::NoConfidence { prev_action_id } => {
            *prev_action_id == governance.enacted_committee
        }
        GovernanceAction::UpdateCommittee { prev_action_id, .. } => {
            *prev_action_id == governance.enacted_committee
        }
        GovernanceAction::NewConstitution { prev_action_id, .. } => {
            *prev_action_id == governance.enacted_constitution
        }
        // TreasuryWithdrawals and InfoAction have no chain requirement
        GovernanceAction::TreasuryWithdrawals { .. } | GovernanceAction::InfoAction => true,
    }
}

/// Returns the governance action priority for ratification ordering.
/// Lower number = higher priority, per Haskell's `actionPriority`.
pub(crate) fn gov_action_priority(action: &GovernanceAction) -> u8 {
    match action {
        GovernanceAction::NoConfidence { .. } => 0,
        GovernanceAction::UpdateCommittee { .. } => 1,
        GovernanceAction::NewConstitution { .. } => 2,
        GovernanceAction::HardForkInitiation { .. } => 3,
        GovernanceAction::ParameterChange { .. } => 4,
        GovernanceAction::TreasuryWithdrawals { .. } => 5,
        GovernanceAction::InfoAction => 6,
    }
}

/// Whether enacting this action should delay all further ratification for this epoch.
/// Per Haskell `delayingAction`: NoConfidence, HardFork, UpdateCommittee, NewConstitution.
pub(crate) fn is_delaying_action(action: &GovernanceAction) -> bool {
    matches!(
        action,
        GovernanceAction::NoConfidence { .. }
            | GovernanceAction::HardForkInitiation { .. }
            | GovernanceAction::UpdateCommittee { .. }
            | GovernanceAction::NewConstitution { .. }
    )
}
