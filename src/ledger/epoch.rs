// Epoch transition logic (NEWEPOCH STS rule)
//
// Copied from torsten-ledger/src/state/epoch.rs
// This ties together all the ledger state components at epoch boundaries

use super::primitives::*;
use super::state::{LedgerState, PoolRegistration};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

impl LedgerState {
    /// Process an epoch transition
    ///
    /// Follows Haskell's NEWEPOCH STS rule ordering:
    /// 1. Apply pending reward update from the PREVIOUS epoch transition (RUPD)
    /// 2. Compute new reward update using go snapshot -> store as pending
    /// 3. Rotate snapshots: go = set, set = mark, mark = new
    /// 4. Rebuild stake distribution from UTxO set
    /// 5. Build new mark snapshot (with deposit accounting)
    /// 6. Process retirements
    /// 7. Expire governance proposals
    /// 8. Mark inactive DReps
    /// 9. Expire committee members
    /// 10. Compute epoch nonce (TICKN rule)
    /// 11. Reset per-epoch accumulators
    pub fn process_epoch_transition(&mut self, new_epoch: EpochNo) {
        tracing::debug!("Epoch transition: {} -> {}", self.epoch.0, new_epoch.0);

        // Step 1: Calculate rewards FIRST using existing snapshots
        // CRITICAL: Haskell's RUPD runs BEFORE SNAP, so we calculate rewards using:
        // - Stake distribution from ssStakeGo (existing go snapshot)
        // - Fees from current_epoch_fees (fees from the PREVIOUS epoch, set by SNAP)
        // This matches the Haskell order: RUPD then SNAP
        //
        // Timing of fees:
        // - At epoch 0→1: current_epoch_fees=0 (initial), fees from epoch 0 NOT used yet
        // - SNAP updates current_epoch_fees = fees from epoch 0
        // - At epoch 1→2: current_epoch_fees=fees from epoch 0, used by RUPD
        // - So fees from epoch N are used at epoch N+1→N+2
        // CRITICAL: Use blocks from the MARK snapshot
        // At epoch N→N+1 transition (BEFORE rotation):
        // - Epoch N just ended, we payout rewards for blocks made in epoch N-1
        // - The mark snapshot contains epoch N-1 data (created at end of epoch N-1)
        // - After payout, we rotate: go←set, set←mark, create new mark
        //
        // The d parameter from the protocol params will handle federated vs decentralized:
        // - d >= 0.8: eta = 1 (full expansion regardless of blocks)
        // - d < 0.8: eta = min(1, actual_blocks / expected_blocks)
        let blocks_for_rewards = if let Some(ref mark_snapshot) = self.snapshots.mark {
            let mark_total_blocks: u64 = mark_snapshot.epoch_blocks_by_pool.values().sum();
            tracing::info!(
                "Using blocks from mark snapshot: epoch={}, blocks={}",
                mark_snapshot.epoch.0,
                mark_total_blocks
            );
            Arc::clone(&mark_snapshot.epoch_blocks_by_pool)
        } else {
            tracing::info!("No mark snapshot available, using empty blocks");
            Arc::new(HashMap::new())
        };

        let fees_for_rewards = self.snapshots.current_epoch_fees;
        let rupd = if let (Some(ref go_snapshot), Some(ref mark_snapshot)) =
            (&self.snapshots.go, &self.snapshots.mark)
        {
            // Use GO snapshot for stake distribution (standard Cardano model)
            // but MARK snapshot blocks (epoch N-1 at transition N→N+1)
            let mut reward_snapshot = (*go_snapshot).clone();
            reward_snapshot.epoch_blocks_by_pool = blocks_for_rewards;
            self.calculate_rewards(&reward_snapshot, fees_for_rewards)
        } else if let Some(ref mark_snapshot) = self.snapshots.mark {
            // Fallback: use mark for both if go doesn't exist yet
            let mut reward_snapshot = (*mark_snapshot).clone();
            reward_snapshot.epoch_blocks_by_pool = blocks_for_rewards;
            self.calculate_rewards(&reward_snapshot, fees_for_rewards)
        } else {
            // Early epochs before set snapshot exists: create empty snapshot with appropriate blocks
            let empty_snapshot = super::state::StakeSnapshot {
                epoch: self.epoch,
                delegations: Arc::new(HashMap::new()),
                pool_stake: HashMap::new(),
                pool_params: Arc::clone(&self.pool_params),
                stake_distribution: Arc::new(HashMap::new()),
                epoch_blocks_by_pool: blocks_for_rewards,
            };
            self.calculate_rewards(&empty_snapshot, fees_for_rewards)
        };

        // Store for debugging/comparison
        self.last_applied_rupd = Some(rupd.clone());
        tracing::info!(
            "Stored last_applied_rupd with eta={}, deltaR1={}",
            rupd.eta,
            rupd.delta_r1
        );

        // Apply the RUPD immediately (not deferred)
        // Reserves: subtract expansion, add back undistributed rewards
        self.reserves.0 = self.reserves.0.saturating_sub(rupd.delta_reserves);
        self.reserves.0 = self.reserves.0.saturating_add(rupd.undistributed);

        // Treasury: add tau cut
        self.treasury.0 = self.treasury.0.saturating_add(rupd.delta_treasury);

        // Apply per-account rewards
        // CRITICAL: Only pay rewards to REGISTERED accounts (that already exist in reward_accounts)
        // Rewards to unregistered accounts should go to treasury (tracked separately)
        let mut unregistered_rewards = Lovelace(0);
        for (cred_hash, reward) in &rupd.rewards {
            if reward.0 > 0 {
                // Check if account is registered
                if Arc::make_mut(&mut self.reward_accounts).contains_key(cred_hash) {
                    *Arc::make_mut(&mut self.reward_accounts)
                        .get_mut(cred_hash)
                        .unwrap() += *reward;
                } else {
                    // Account not registered - reward goes to treasury
                    unregistered_rewards.0 += reward.0;
                }
            }
        }

        // Add unregistered account rewards to treasury
        // NOTE: Do NOT adjust reserves here. rupd.undistributed already accounts for all
        // computed rewards (including unregistered ones), so reserves were correctly adjusted
        // via the existing +rupd.undistributed above. Subtracting again would double-deduct.
        if unregistered_rewards.0 > 0 {
            tracing::info!(
                "Unregistered account rewards going to treasury: {} lovelace",
                unregistered_rewards.0
            );
            self.treasury.0 = self.treasury.0.saturating_add(unregistered_rewards.0);
        }

        tracing::debug!(
            "Applied epoch {} rewards: treasury +{}, reserves -{}",
            self.epoch.0,
            rupd.delta_treasury,
            rupd.delta_reserves
        );

        // CRITICAL: Update protocol parameters for new epoch AFTER rewards are calculated
        // Babbage era (epoch 2+) removes the decentralization parameter (d=0)
        // This must happen AFTER reward calculation because epoch N→N+1 transition
        // uses epoch N's parameters, not epoch N+1's parameters
        if new_epoch.0 >= 2 && self.protocol_params.decentralization.numerator != 0 {
            tracing::info!(
                "Babbage era: setting decentralization parameter d=0 (was {}/{})",
                self.protocol_params.decentralization.numerator,
                self.protocol_params.decentralization.denominator
            );
            self.protocol_params.decentralization = Rational {
                numerator: 0,
                denominator: 1,
            };
        }

        // Step 2: NOW rotate snapshots (AFTER reward calculation, matching Haskell SNAP rule)
        // This happens AFTER RUPD in Haskell's NEWEPOCH rule
        self.snapshots.go = self.snapshots.set.take();
        self.snapshots.set = self.snapshots.mark.take();

        // Step 4: Rebuild stake distribution from the full UTxO set at epoch boundaries.
        // This ensures correctness and prevents incremental drift.
        // NOTE: In hayate, we'll need to query the utxo_tree LSM tree
        // For now, we skip this and assume stake_distribution is maintained incrementally
        // TODO: Implement rebuild_stake_distribution() that queries utxo_tree

        // Step 5: Build new mark snapshot
        // Per Cardano spec, stake snapshot = UTxO-delegated stake + reward account balance.
        // Deposits are NOT included — they are tracked separately in esDeposited and do not
        // contribute to pool sigma or member reward calculations. Including deposits would
        // inflate sigma and over-distribute rewards.
        let mut pool_stake: HashMap<Hash28, Lovelace> =
            HashMap::with_capacity(self.pool_params.len());

        for (cred_hash, pool_id) in self.delegations.iter() {
            let utxo_stake = self
                .stake_distribution
                .stake_map
                .get(cred_hash)
                .copied()
                .unwrap_or(Lovelace(0));
            let reward_balance = self
                .reward_accounts
                .get(cred_hash)
                .copied()
                .unwrap_or(Lovelace(0));

            let total_stake = utxo_stake + reward_balance;
            *pool_stake.entry(*pool_id).or_insert(Lovelace(0)) += total_stake;
        }

        // Build per-credential stake for delegated credentials only.
        // Matches Haskell's spssStake: `domRestrictedMap (delegations ▷ dom poolParams) (utxo ∪+ rewards)`.
        // Only credentials that are delegated to a registered pool are included.
        // Undelegated (registered but not delegated) credentials are excluded.
        let mut snapshot_stake: HashMap<Hash32, Lovelace> =
            HashMap::with_capacity(self.delegations.len());
        for (cred_hash, pool_id) in self.delegations.iter() {
            if !self.pool_params.contains_key(pool_id) {
                continue; // pool retired or not yet registered
            }
            let utxo_stake = self
                .stake_distribution
                .stake_map
                .get(cred_hash)
                .copied()
                .unwrap_or(Lovelace(0));
            let reward_balance = self
                .reward_accounts
                .get(cred_hash)
                .copied()
                .unwrap_or(Lovelace(0));
            snapshot_stake.insert(*cred_hash, utxo_stake + reward_balance);
        }

        let total_utxo_stake: u64 = self
            .stake_distribution
            .stake_map
            .values()
            .fold(0u64, |acc, l| acc.saturating_add(l.0));
        let total_pool_stake: u64 = pool_stake
            .values()
            .fold(0u64, |acc, l| acc.saturating_add(l.0));

        tracing::debug!(
            epoch = new_epoch.0,
            credentials = self.stake_distribution.stake_map.len(),
            delegations = self.delegations.len(),
            pools = pool_stake.len(),
            total_utxo_stake_ada = total_utxo_stake / 1_000_000,
            total_pool_stake_ada = total_pool_stake / 1_000_000,
            "Epoch snapshot created"
        );

        use super::state::StakeSnapshot;
        // CRITICAL: Create new mark snapshot for the epoch that just ended
        let new_snapshot = StakeSnapshot {
            epoch: self.epoch, // CRITICAL: Snapshot records the epoch that ENDED, not new_epoch
            delegations: Arc::clone(&self.delegations),
            pool_stake,
            pool_params: Arc::clone(&self.pool_params),
            stake_distribution: Arc::new(snapshot_stake),
            epoch_blocks_by_pool: Arc::clone(&self.epoch_blocks_by_pool),
        };

        // Store in mark for the 3-snapshot rotation pattern
        self.snapshots.mark = Some(new_snapshot.clone());

        // Bootstrap: In epoch 0 only, populate set so that rotation at epoch 1→2 gives us go=epoch0
        // This ensures at epoch 2→3, we use go=epoch0 for reward calculation
        // Only bootstrap in epoch 0 (before first Babbage epoch)
        if self.epoch.0 == 0 {
            self.snapshots.set = Some(new_snapshot);
        }

        // Apply queued pool re-registrations AFTER taking the mark snapshot.
        // Matches Haskell's EPOCH STS ordering: SNAP runs before POOL.
        // The re-registered params will appear in the mark snapshot at the NEXT epoch boundary.
        let future_params = std::mem::replace(
            Arc::make_mut(&mut self.future_pool_params),
            HashMap::new(),
        );
        if !future_params.is_empty() {
            let pool_map = Arc::make_mut(&mut self.pool_params);
            for (pool_id, reg) in future_params {
                pool_map.insert(pool_id, reg);
            }
        }

        // Update current_epoch_fees (Haskell's ssFee) to fees from the epoch that just ended
        // This will be used by RUPD at the NEXT epoch boundary
        self.snapshots.current_epoch_fees = self.epoch_fees;

        // Step 6: Process pending pool retirements for this epoch
        if let Some(retiring_pools) = self.pending_retirements.remove(&new_epoch) {
            let pool_deposit = Lovelace(self.protocol_params.pool_deposit);
            for pool_id in &retiring_pools {
                // Refund pool deposit to operator's registered reward account
                if let Some(pool_reg) = Arc::make_mut(&mut self.pool_params).remove(pool_id) {
                    let op_key = Self::reward_account_to_hash(&pool_reg.reward_account);
                    *Arc::make_mut(&mut self.reward_accounts)
                        .entry(op_key)
                        .or_insert(Lovelace(0)) += pool_deposit;

                    // Also refund deposit from tracker
                    self.deposit_tracker
                        .refund_deposit(&op_key, super::state::DepositType::Pool);

                    tracing::debug!(
                        "Pool retired at epoch {}: {} (deposit {} refunded)",
                        new_epoch.0,
                        hex::encode(pool_id),
                        pool_deposit.0
                    );
                }
            }
        }

        // Clean up retirements from past epochs
        self.pending_retirements
            .retain(|epoch, _| *epoch >= new_epoch);

        // Step 7: Apply pre-Conway protocol parameter updates (PPUP rule)
        //
        // Proposals submitted in epoch N carry CBOR epoch=N in their Update field.
        // Haskell applies them two transitions later: the proposals from epoch N are
        // enacted at (N+1)→(N+2), producing epoch N+2 state.
        //
        // Concretely: at transition old→new (self.epoch=old, new_epoch=new),
        // we apply proposals with key = old - 1.  Example: at 2→3, apply epoch-1
        // proposals → epoch 3 gets nOpt=500.
        let ppup_key = EpochNo(self.epoch.0.saturating_sub(1));
        if let Some(proposals) = self.pending_pp_updates.remove(&ppup_key) {
            let quorum = self.update_quorum;
            let n_proposals = proposals.len() as u64;
            tracing::info!(
                epoch = new_epoch.0,
                n_proposals,
                quorum,
                "Evaluating pre-Conway protocol parameter update proposals"
            );

            if n_proposals >= quorum {
                // Merge all proposals: for each field, if any proposal sets it,
                // apply the first (all agreeing delegates propose the same values).
                // This is equivalent to Haskell's `votedValue` when quorum is met.
                let mut merged = crate::ledger::primitives::ProtocolParamUpdate::default();
                for (_genesis_hash, update) in &proposals {
                    if merged.min_fee_a.is_none() { merged.min_fee_a = update.min_fee_a; }
                    if merged.min_fee_b.is_none() { merged.min_fee_b = update.min_fee_b; }
                    if merged.max_block_body_size.is_none() { merged.max_block_body_size = update.max_block_body_size; }
                    if merged.max_transaction_size.is_none() { merged.max_transaction_size = update.max_transaction_size; }
                    if merged.max_block_header_size.is_none() { merged.max_block_header_size = update.max_block_header_size; }
                    if merged.protocol_version.is_none() { merged.protocol_version = update.protocol_version; }
                    if merged.key_deposit.is_none() { merged.key_deposit = update.key_deposit; }
                    if merged.pool_deposit.is_none() { merged.pool_deposit = update.pool_deposit; }
                    if merged.min_pool_cost.is_none() { merged.min_pool_cost = update.min_pool_cost; }
                    if merged.rho.is_none() { merged.rho = update.rho; }
                    if merged.tau.is_none() { merged.tau = update.tau; }
                    if merged.a0.is_none() { merged.a0 = update.a0; }
                    if merged.n_opt.is_none() { merged.n_opt = update.n_opt; }
                    if merged.e_max.is_none() { merged.e_max = update.e_max; }
                    if merged.decentralization.is_none() { merged.decentralization = update.decentralization; }
                }
                if let Err(e) = self.apply_protocol_param_update(&merged) {
                    tracing::warn!(epoch = new_epoch.0, error = %e, "Failed to apply protocol parameter update");
                } else {
                    tracing::info!(
                        epoch = new_epoch.0,
                        n_opt = ?merged.n_opt,
                        rho = ?merged.rho,
                        tau = ?merged.tau,
                        "Applied pre-Conway protocol parameter update"
                    );
                }
            } else {
                tracing::debug!(
                    epoch = new_epoch.0,
                    n_proposals,
                    quorum,
                    "Not enough proposals for quorum, skipping protocol parameter update"
                );
            }
        }
        // Discard proposals from epochs before the current (old) epoch.
        // Proposals with key = self.epoch are still needed for the next transition
        // (they will be applied at (self.epoch+1) → (self.epoch+2)).
        self.pending_pp_updates
            .retain(|epoch, _| epoch.0 >= self.epoch.0);

        // Step 8: Ratify governance proposals (stubbed for now, full implementation in task #6)
        // TODO: Implement full governance ratification in task #6
        self.ratify_proposals_stub();

        // Step 9: Expire governance proposals that have passed their lifetime
        let expired: Vec<GovActionId> = self
            .governance
            .proposals
            .iter()
            .filter(|(_, state)| state.expires_epoch < new_epoch)
            .map(|(id, _)| *id)
            .collect();

        if !expired.is_empty() {
            for action_id in &expired {
                if let Some(proposal_state) = Arc::make_mut(&mut self.governance)
                    .proposals
                    .remove(action_id)
                {
                    // Refund deposit to return address
                    let deposit = proposal_state.procedure.deposit;
                    if deposit.0 > 0 {
                        // Extract credential from return address
                        let return_cred_hash = match &proposal_state.procedure.return_addr {
                            Credential::Key(hash) => *hash,
                            Credential::Script(hash) => *hash,
                        };

                        *Arc::make_mut(&mut self.reward_accounts)
                            .entry(return_cred_hash)
                            .or_insert(Lovelace(0)) += deposit;

                        // Refund from deposit tracker
                        self.deposit_tracker.refund_deposit(
                            &return_cred_hash,
                            super::state::DepositType::Governance(*action_id),
                        );
                    }
                    tracing::debug!(
                        "Governance proposal expired at epoch {}: {:?} (deposit {} returned)",
                        new_epoch.0,
                        action_id,
                        deposit.0
                    );
                }
            }

            // Remove votes for expired proposals
            for id in &expired {
                Arc::make_mut(&mut self.governance)
                    .votes_by_action
                    .remove(id);
            }

            tracing::debug!(
                "Expired {} governance proposals at epoch {}",
                expired.len(),
                new_epoch.0
            );
        }

        Arc::make_mut(&mut self.governance).last_expired = expired;

        // Step 10: Mark inactive DReps per CIP-1694
        let drep_activity = self.protocol_params.drep_activity_period;
        if drep_activity > 0 {
            let mut newly_inactive = 0u64;
            let mut reactivated = 0u64;
            for drep in Arc::make_mut(&mut self.governance).dreps.values_mut() {
                let inactive = new_epoch.0.saturating_sub(drep.last_active_epoch.0) > drep_activity;
                if inactive && drep.active {
                    drep.active = false;
                    newly_inactive += 1;
                } else if !inactive && !drep.active {
                    drep.active = true;
                    reactivated += 1;
                }
            }
            if newly_inactive > 0 || reactivated > 0 {
                tracing::debug!(
                    "DRep activity update at epoch {}: {} newly inactive, {} reactivated (threshold: {} epochs)",
                    new_epoch.0,
                    newly_inactive,
                    reactivated,
                    drep_activity
                );
            }
        }

        // Step 11: Expire committee members that have passed their expiration epoch
        let expired_members: Vec<Hash32> = self
            .governance
            .committee_expiration
            .iter()
            .filter(|(_, exp_epoch)| **exp_epoch <= new_epoch)
            .map(|(hash, _)| *hash)
            .collect();

        if !expired_members.is_empty() {
            for hash in &expired_members {
                Arc::make_mut(&mut self.governance)
                    .committee_hot_keys
                    .remove(hash);
                Arc::make_mut(&mut self.governance)
                    .committee_expiration
                    .remove(hash);
            }
            tracing::debug!(
                "Expired {} committee members at epoch {}",
                expired_members.len(),
                new_epoch.0
            );
        }

        // Step 12: Compute new epoch nonce per Haskell TICKN rule
        self.compute_epoch_nonce();

        // Step 13: Reset per-epoch accumulators
        self.epoch_fees = Lovelace(0);
        Arc::make_mut(&mut self.epoch_blocks_by_pool).clear();
        self.epoch_block_count = 0;

        self.epoch = new_epoch;
    }

    /// Stub for governance ratification (full implementation in task #6)
    fn ratify_proposals_stub(&mut self) {
        // TODO: Implement full CIP-1694 ratification logic in task #6
        // This includes:
        // - Checking voting thresholds per action type
        // - DRep + SPO + Committee voting
        // - Enacting ratified actions
        // - Updating enacted_* roots
        // For now, this is a placeholder
        Arc::make_mut(&mut self.governance).last_ratified.clear();
        Arc::make_mut(&mut self.governance).last_ratify_delayed = false;
    }

    /// Dump epoch state to JSON file for comparison with Haskell cardano-node
    pub fn dump_epoch_state(&self, dump_dir: &Path, slot: u64) -> Result<(), std::io::Error> {
        let filename = format!("{}-hayate.json", self.epoch.0);
        let filepath = dump_dir.join(filename);

        // Helper to format stake map to match Haskell format
        let format_stake = |stake: &HashMap<Hash32, Lovelace>| {
            stake
                .iter()
                .map(|(k, v)| (format!("keyHash-{}", hex::encode(k)), json!(v.0)))
                .collect::<serde_json::Map<String, serde_json::Value>>()
        };

        // Helper to format delegations to match Haskell format
        let format_delegations = |delegations: &HashMap<Hash32, Hash28>| {
            delegations
                .iter()
                .map(|(k, v)| (format!("keyHash-{}", hex::encode(k)), json!(hex::encode(v))))
                .collect::<serde_json::Map<String, serde_json::Value>>()
        };

        // Helper to format pool params (pledge, cost, margin for reward diagnosis)
        let format_pool_params = |pool_params: &HashMap<Hash28, PoolRegistration>| {
            pool_params
                .iter()
                .map(|(pool_id, reg)| {
                    let margin = if reg.margin_denominator > 0 {
                        reg.margin_numerator as f64 / reg.margin_denominator as f64
                    } else {
                        0.0
                    };
                    (
                        hex::encode(pool_id),
                        json!({
                            "poolId": hex::encode(pool_id),
                            "pledge": reg.pledge.0,
                            "cost": reg.cost.0,
                            "margin": margin,
                            "owners": reg.owners.iter().map(hex::encode).collect::<Vec<_>>(),
                        }),
                    )
                })
                .collect::<serde_json::Map<String, serde_json::Value>>()
        };

        // Helper to create snapshot JSON (includes pool_stake and blocks for reward diagnosis)
        let format_snapshot = |name: &str, snapshot: &Option<super::state::StakeSnapshot>| {
            if let Some(snap) = snapshot {
                let pool_stake: serde_json::Map<String, serde_json::Value> = snap
                    .pool_stake
                    .iter()
                    .map(|(pool_id, stake)| (hex::encode(pool_id), json!(stake.0)))
                    .collect();
                let blocks: serde_json::Map<String, serde_json::Value> = snap
                    .epoch_blocks_by_pool
                    .iter()
                    .map(|(pool_id, count)| (hex::encode(pool_id), json!(count)))
                    .collect();
                json!({
                    "name": name,
                    "epoch": snap.epoch.0,
                    "stake": format_stake(&snap.stake_distribution),
                    "delegations": format_delegations(&snap.delegations),
                    "poolParams": format_pool_params(&snap.pool_params),
                    "poolStake": pool_stake,
                    "blocks": blocks,
                })
            } else {
                json!(null)
            }
        };

        // Calculate pool distribution for comparison
        let pool_distribution: Vec<_> = self
            .snapshots
            .mark
            .as_ref()
            .map(|mark| {
                let total_stake: u64 = mark.pool_stake.values().map(|l| l.0).sum();
                mark.pool_stake
                    .iter()
                    .map(|(pool_id, stake)| {
                        let stake_rational = if total_stake > 0 {
                            stake.0 as f64 / total_stake as f64
                        } else {
                            0.0
                        };
                        json!({
                            "poolId": hex::encode(pool_id),
                            "stake": stake_rational,
                            "stakePercent": stake_rational * 100.0,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Aggregate deposits by type for comparison with Haskell
        let mut total_pool_deposits: u64 = 0;
        let mut total_stake_deposits: u64 = 0;
        let mut total_governance_deposits: u64 = 0;
        let mut total_drep_deposits: u64 = 0;
        for deposits in self.deposit_tracker.deposits.values() {
            if let Some(d) = deposits.pool {
                total_pool_deposits += d.0;
            }
            if let Some(d) = deposits.stake {
                total_stake_deposits += d.0;
            }
            for (_, d) in &deposits.governance {
                total_governance_deposits += d.0;
            }
            if let Some(d) = deposits.drep {
                total_drep_deposits += d.0;
            }
        }
        let total_deposits =
            total_pool_deposits + total_stake_deposits + total_governance_deposits + total_drep_deposits;

        // Include RUPD intermediate values for comparison with Haskell
        let rupd_values = if let Some(rupd) = &self.last_applied_rupd {
            tracing::info!("Dumping RUPD: eta={}, deltaR1={}", rupd.eta, rupd.delta_r1);
            json!({
                "eta": rupd.eta,
                "deltaR1": rupd.delta_r1,
                "rPot": rupd.r_pot,
                "deltaT1": rupd.delta_t1,
                "rewardPot": rupd.reward_pot_after_treasury,
                "totalDistributed": rupd.total_distributed,
                "deltaR2": rupd.undistributed,
            })
        } else {
            tracing::info!(
                "last_applied_rupd is None when dumping epoch {}",
                self.epoch.0
            );
            json!(null)
        };

        let pp = &self.protocol_params;
        let rho_f = pp.rho.numerator as f64 / pp.rho.denominator.max(1) as f64;
        let tau_f = pp.tau.numerator as f64 / pp.tau.denominator.max(1) as f64;
        let a0_f = pp.a0.numerator as f64 / pp.a0.denominator.max(1) as f64;
        let d_f = pp.decentralization.numerator as f64 / pp.decentralization.denominator.max(1) as f64;

        // activeStake = sum of all go snapshot stake (= delegated lovelace in go snapshot)
        // Matches Haskell's sumAllStake (ssStake goSnap)
        let active_stake: u64 = self.snapshots.go.as_ref()
            .map(|go| go.stake_distribution.values().map(|l| l.0).sum())
            .unwrap_or(0);

        let json_output = json!({
            "epoch": self.epoch.0,
            "slot": slot,
            "snapshotEraName": "Babbage", // TODO: Track actual era
            "treasury": self.treasury.0,
            "reserves": self.reserves.0,
            "activeStake": active_stake,
            "protocolParams": {
                "nOpt": pp.n_opt,
                "a0": a0_f,
                "rho": rho_f,
                "tau": tau_f,
                "d": d_f,
                "minPoolCost": pp.min_pool_cost,
                "protocolVersion": {
                    "major": pp.protocol_version_major,
                    "minor": pp.protocol_version_minor,
                },
            },
            "totalPools": self.pool_params.len(),
            "totalStake": pool_distribution.iter().map(|p| p["stake"].as_f64().unwrap_or(0.0)).sum::<f64>(),
            "poolDistribution": pool_distribution,
            "epochFees": self.snapshots.current_epoch_fees.0,
            "deposits": {
                "pool": total_pool_deposits,
                "stakeKey": total_stake_deposits,
                "proposal": total_governance_deposits,
                "dRep": total_drep_deposits,
                "total": total_deposits,
            },
            "rupd": rupd_values,
            "snapshots": {
                "mark": format_snapshot("mark", &self.snapshots.mark),
                "set": format_snapshot("set", &self.snapshots.set),
                "go": format_snapshot("go", &self.snapshots.go),
            }
        });

        std::fs::write(&filepath, serde_json::to_string_pretty(&json_output)?)?;
        tracing::info!(
            "📝 Dumped epoch {} state to {}",
            self.epoch.0,
            filepath.display()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_epoch_transition_basic() {
        let mut state = LedgerState::new(ProtocolParameters::default());
        state.epoch = EpochNo(100);

        state.process_epoch_transition(EpochNo(101));

        assert_eq!(state.epoch.0, 101);
        assert_eq!(state.epoch_fees.0, 0);
        assert_eq!(state.epoch_block_count, 0);
    }

    #[test]
    fn test_snapshot_rotation() {
        let mut state = LedgerState::new(ProtocolParameters::default());
        state.epoch = EpochNo(100);

        // Create initial snapshots
        use super::super::state::{EpochSnapshots, StakeSnapshot};
        state.snapshots = EpochSnapshots {
            mark: Some(StakeSnapshot {
                epoch: EpochNo(100),
                delegations: Arc::new(HashMap::new()),
                pool_stake: HashMap::new(),
                pool_params: Arc::new(HashMap::new()),
                stake_distribution: Arc::new(HashMap::new()),
                epoch_blocks_by_pool: Arc::new(HashMap::new()),
            }),
            set: Some(StakeSnapshot {
                epoch: EpochNo(99),
                delegations: Arc::new(HashMap::new()),
                pool_stake: HashMap::new(),
                pool_params: Arc::new(HashMap::new()),
                stake_distribution: Arc::new(HashMap::new()),
                epoch_blocks_by_pool: Arc::new(HashMap::new()),
            }),
            go: Some(StakeSnapshot {
                epoch: EpochNo(98),
                delegations: Arc::new(HashMap::new()),
                pool_stake: HashMap::new(),
                pool_params: Arc::new(HashMap::new()),
                stake_distribution: Arc::new(HashMap::new()),
                epoch_blocks_by_pool: Arc::new(HashMap::new()),
            }),
            current_epoch_fees: Lovelace(0),
        };

        state.process_epoch_transition(EpochNo(101));

        // Verify rotation: go = old set, set = old mark, mark = new
        // The new mark snapshot records the epoch that ENDED (100), not the new epoch (101).
        assert_eq!(state.snapshots.go.as_ref().unwrap().epoch.0, 99);
        assert_eq!(state.snapshots.set.as_ref().unwrap().epoch.0, 100);
        assert_eq!(state.snapshots.mark.as_ref().unwrap().epoch.0, 100);
    }
}
