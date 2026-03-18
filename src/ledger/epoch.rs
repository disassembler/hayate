// Epoch transition logic (NEWEPOCH STS rule)
//
// Copied from torsten-ledger/src/state/epoch.rs
// This ties together all the ledger state components at epoch boundaries

use super::primitives::*;
use super::state::LedgerState;
use std::collections::HashMap;
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
        let fees_for_rewards = self.snapshots.current_epoch_fees;
        let rupd = if let Some(ref go_snapshot) = self.snapshots.go {
            self.calculate_rewards(go_snapshot, fees_for_rewards)
        } else {
            // Early epochs before go snapshot exists: create empty snapshot
            // This allows reward calculation to run, with all rewards undistributed
            let empty_snapshot = super::state::StakeSnapshot {
                epoch: self.epoch,
                delegations: Arc::new(HashMap::new()),
                pool_stake: HashMap::new(),
                pool_params: Arc::clone(&self.pool_params),
                stake_distribution: Arc::new(HashMap::new()),
            };
            self.calculate_rewards(&empty_snapshot, fees_for_rewards)
        };

        // Apply the RUPD immediately (not deferred)
        // Note: delta_reserves represents the NET decrease in reserves
        // When all rewards are undistributed and fees > treasury_cut, reserves actually INCREASE
        // In that case, delta_reserves will be 0 (clamped from negative)
        // The actual increase happens via the reward pot flowing back
        self.reserves.0 = self.reserves.0.saturating_sub(rupd.delta_reserves);
        self.treasury.0 = self.treasury.0.saturating_add(rupd.delta_treasury);

        // Apply per-account rewards
        for (cred_hash, reward) in &rupd.rewards {
            if reward.0 > 0 {
                *Arc::make_mut(&mut self.reward_accounts)
                    .entry(*cred_hash)
                    .or_insert(Lovelace(0)) += *reward;
            }
        }

        tracing::debug!(
            "Applied epoch {} rewards: treasury +{}, reserves -{}",
            self.epoch.0,
            rupd.delta_treasury,
            rupd.delta_reserves
        );

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
        // Per Cardano spec, total stake = UTxO-delegated stake + reward account balance + deposits
        // CRITICAL: Account for deposits correctly (voting vs staking stake)
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

            // CRITICAL: Use staking_stake (excludes governance/DRep deposits)
            // NOT voting_stake (includes all deposits)
            let deposit_stake = self.deposit_tracker.get_staking_stake(cred_hash);

            let total_stake = utxo_stake + reward_balance + deposit_stake;
            *pool_stake.entry(*pool_id).or_insert(Lovelace(0)) += total_stake;
        }

        // Build per-credential stake including reward balances AND deposits
        let mut snapshot_stake = self.stake_distribution.stake_map.clone();
        for (cred_hash, reward) in self.reward_accounts.iter() {
            if reward.0 > 0 {
                *snapshot_stake.entry(*cred_hash).or_insert(Lovelace(0)) += *reward;
            }
        }
        // Add voting stake (includes ALL deposits)
        for (cred_hash, deposits) in &self.deposit_tracker.deposits {
            let voting_deposit = deposits.voting_stake();
            if voting_deposit.0 > 0 {
                *snapshot_stake.entry(*cred_hash).or_insert(Lovelace(0)) += voting_deposit;
            }
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
        };

        // Store in mark for the 3-snapshot rotation pattern
        self.snapshots.mark = Some(new_snapshot.clone());

        // ALSO store in set so it becomes `go` next epoch (used for rewards at N+1)
        // This matches Haskell's behavior where fees are used 1 epoch later, not 2
        if self.snapshots.set.is_none() {
            self.snapshots.set = Some(new_snapshot);
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
                    self.deposit_tracker.refund_deposit(&op_key, super::state::DepositType::Pool);

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
        // Simplified version - full implementation would merge proposals
        if let Some(_proposals) = self.pending_pp_updates.remove(&self.epoch) {
            // TODO: Implement protocol parameter update merging and application
            tracing::debug!(
                epoch = new_epoch.0,
                "Pre-Conway protocol parameter updates (stubbed)"
            );
        }
        self.pending_pp_updates
            .retain(|epoch, _| *epoch >= new_epoch);

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
                if let Some(proposal_state) =
                    Arc::make_mut(&mut self.governance).proposals.remove(action_id)
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
                        self.deposit_tracker
                            .refund_deposit(&return_cred_hash, super::state::DepositType::Governance(*action_id));
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
                Arc::make_mut(&mut self.governance).votes_by_action.remove(id);
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
                let inactive =
                    new_epoch.0.saturating_sub(drep.last_active_epoch.0) > drep_activity;
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
            }),
            set: Some(StakeSnapshot {
                epoch: EpochNo(99),
                delegations: Arc::new(HashMap::new()),
                pool_stake: HashMap::new(),
                pool_params: Arc::new(HashMap::new()),
                stake_distribution: Arc::new(HashMap::new()),
            }),
            go: Some(StakeSnapshot {
                epoch: EpochNo(98),
                delegations: Arc::new(HashMap::new()),
                pool_stake: HashMap::new(),
                pool_params: Arc::new(HashMap::new()),
                stake_distribution: Arc::new(HashMap::new()),
            }),
            current_epoch_fees: Lovelace(0),
        };

        state.process_epoch_transition(EpochNo(101));

        // Verify rotation: go = old set, set = old mark, mark = new
        assert_eq!(state.snapshots.go.as_ref().unwrap().epoch.0, 99);
        assert_eq!(state.snapshots.set.as_ref().unwrap().epoch.0, 100);
        assert_eq!(state.snapshots.mark.as_ref().unwrap().epoch.0, 101);
    }
}
