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
    ///  1. Apply pending reward update (applyRUpd — computed at PREVIOUS boundary)
    ///  2. SNAP: rotate snapshots (go=set, set=mark), build new mark
    ///  3. POOLREAP: process pool retirements
    ///  4. ENACT: apply previously ratified governance proposals
    ///  5. Apply queued pool re-registrations
    ///  6. PPUP: apply pre-Conway protocol parameter updates
    ///  7. RATIFY: ratify governance proposals
    ///  8. Expire governance proposals
    ///  9. Mark inactive DReps
    /// 10. Expire committee members
    /// 11. Snapshot DRep power distribution
    /// 12. Compute epoch nonce (TICKN rule)
    /// 13. createRUpd: compute new reward update -> store in pending_reward_update
    /// 14. Reset per-epoch accumulators, advance epoch number
    pub fn process_epoch_transition(&mut self, new_epoch: EpochNo) {
        tracing::debug!(target: "ChainDB.LedgerEvent", "Epoch transition: {} -> {}", self.epoch.0, new_epoch.0);

        // Capture prevPp BEFORE propagating new epoch's params.
        // This is the protocol parameters in effect during the epoch that just ended (epoch N
        // at the N→N+1 transition). Used by createRUpd as the `prevPp` argument: the params
        // under which the blocks in `self.epoch_blocks_by_pool` were produced.
        let prev_pp = self.protocol_params.clone();

        // Propagate Conway curPParams → protocol_params starting from the SECOND Conway epoch.
        // At the Conway genesis epoch (e.g. 492), protocol_params stays as Babbage params so
        // the epoch dump shows the correct "prevPParams = Babbage params" relationship.
        // From the next epoch onwards, protocol_params reflects the current Conway params.
        if let Some(genesis_epoch) = self.conway_genesis_epoch {
            if new_epoch.0 > genesis_epoch {
                if let Some(cur_params) = self.governance.conway_cur_params.as_deref().cloned() {
                    self.protocol_params = cur_params;
                    tracing::debug!(
                        target: "ChainDB.LedgerEvent",
                        "Propagated conway_cur_params → protocol_params at epoch {} (genesis epoch: {})",
                        new_epoch.0, genesis_epoch
                    );
                }
            }
        }

        // ===================================================================
        // Step 1: Apply the PREVIOUSLY computed reward update (deferred RUPD)
        // ===================================================================
        // Haskell's NEWEPOCH STS order:
        //   1. applyRUpd (apply rewards computed at the PREVIOUS epoch boundary)
        //   2. EPOCH sub-rule (SNAP, POOLREAP, ENACT, PPUP, RATIFY, ...)
        //   3. createRUpd (compute new rewards for deferred application at NEXT boundary)
        //
        // The pending_reward_update was computed at the PREVIOUS epoch boundary using
        // state from that point (correct reserves, correct reward_accounts registration).
        // Applying it here — before SNAP and before the new RUPD computation — matches
        // Haskell's timing exactly.
        if let Some(rupd) = self.pending_reward_update.take() {
            // Reserves: subtract monetary expansion (deltaR1), add back undistributed (rounding remainder).
            // Unregistered rewards are in rupd.rewards and flow to treasury via unreg_treasury below.
            self.reserves.0 = self.reserves.0.saturating_sub(rupd.delta_reserves);
            self.reserves.0 = self.reserves.0.saturating_add(rupd.undistributed);

            // Apply per-account rewards. Amounts for unregistered accounts become unregRU'
            // (Haskell Babbage+: computed unconditionally at createRUpd, redirected here).
            let mut unreg_treasury = 0u64;
            for (cred_hash, reward) in &rupd.rewards {
                if reward.0 > 0 {
                    let registered = self.reward_accounts.contains_key(cred_hash);
                    tracing::debug!(
                        target: "ChainDB.LedgerEvent",
                        epoch = self.epoch.0,
                        cred = hex::encode(&cred_hash[..28]),
                        reward = reward.0,
                        unregistered = !registered,
                        "RUPD: unregRU"
                    );
                    if let Some(balance) =
                        Arc::make_mut(&mut self.reward_accounts).get_mut(cred_hash)
                    {
                        *balance += *reward;
                    } else {
                        // Account not registered at application time → unregRU'
                        unreg_treasury += reward.0;
                    }
                }
            }

            // Treasury: deltaT1 + unregRU'.
            // Haskell: treasury' = treasury + Δt₁ + unregRU'
            self.treasury.0 = self.treasury.0
                .saturating_add(rupd.delta_treasury)
                .saturating_add(unreg_treasury);

            tracing::debug!(target: "ChainDB.LedgerEvent", "RUPD: unregRU′ = {}", unreg_treasury);
            tracing::debug!(
                target: "ChainDB.LedgerEvent",
                "RUPD: ending balances: reserves={}, treasury={}",
                self.reserves.0, self.treasury.0,
            );

            // Store for debugging/comparison — this is what hayate[N].rupd shows in dumps
            self.last_applied_rupd = Some(rupd);
        } else {
            // No pending RUPD (first epoch or fresh start)
            self.last_applied_rupd = None;
        }

        // ===================================================================
        // Step 2: SNAP — rotate snapshots (AFTER RUPD application)
        // ===================================================================
        // After applying rewards, reward_accounts reflects the new balances.
        // The mark snapshot built below includes these updated reward balances,
        // matching Haskell's SNAP which runs after applyRUpd.
        //
        // Standard 3-level rotation: go = old set, set = old mark, mark = new.
        // The "pay" snapshot (built below, after the new mark is created) combines
        // go's stake with the current epoch's blocks for RUPD calculation.
        self.snapshots.go = self.snapshots.set.take();
        self.snapshots.set = self.snapshots.mark.take();

        // Step 4: Rebuild stake distribution from the full UTxO set at epoch boundaries.
        // This ensures correctness and prevents incremental drift.
        // NOTE: In hayate, we'll need to query the utxo_tree LSM tree
        // For now, we skip this and assume stake_distribution is maintained incrementally
        // TODO: Implement rebuild_stake_distribution() that queries utxo_tree

        // Step 5b: Build new mark snapshot (SNAP runs BEFORE POOLREAP in Haskell's EPOCH STS)
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
            let total = utxo_stake + reward_balance;
            if total.0 > 0 {
                snapshot_stake.insert(*cred_hash, total);
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
            target: "ChainDB.LedgerEvent",
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
            // Capture current script credentials so dumps remain correct even after
            // credentials are later deregistered from the live ledger state.
            script_stake_credentials: self.script_stake_credentials.clone(),
        };

        // Store in mark for the 3-snapshot rotation pattern.
        // No bootstrap needed: Haskell starts all snapshots empty, so at epoch 2 go=empty
        // (activeStake=0) and at epoch 3 go=epoch0 snapshot.  Any pre-seeding of set at
        // epoch 0 would advance go by one epoch too early.
        self.snapshots.mark = Some(new_snapshot);

        // Step 5a: POOLREAP — retire pools after taking the mark snapshot.
        // Matches Haskell's EPOCH STS: SNAP runs before POOL (POOLREAP).
        // Haskell's POOLREAP retires pools where retire_epoch == new_epoch (exact match).
        // The mark snapshot above includes pools that are being retired in this same transition.
        {
            // Haskell POOLREAP step 1: activate future pool params BEFORE retirement.
            // This ensures re-registered pools get their updated params before we check for
            // retirement. Matches Haskell:
            //   psStakePools = Map.union psFutureStakePoolsL psStakePoolsL
            let future_params =
                std::mem::replace(Arc::make_mut(&mut self.future_pool_params), HashMap::new());
            if !future_params.is_empty() {
                let pool_map = Arc::make_mut(&mut self.pool_params);
                for (pool_id, reg) in future_params {
                    pool_map.insert(pool_id, reg);
                }
            }

            let pool_deposit = Lovelace(self.protocol_params.pool_deposit);
            // Haskell: retired = {k | (k, v) <- psRetiring, v == e}
            // pending_retirements is HashMap<Hash28, EpochNo> (pool_id → epoch)
            let retiring: Vec<Hash28> = self
                .pending_retirements
                .iter()
                .filter(|(_, epoch)| **epoch == new_epoch)
                .map(|(pool_id, _)| *pool_id)
                .collect();
            // Remove all retired pools from pending_retirements
            for pool_id in &retiring {
                self.pending_retirements.remove(pool_id);
            }

            // Collect the set of retired pool IDs for delegation removal
            let mut retired_set: std::collections::HashSet<Hash28> =
                std::collections::HashSet::with_capacity(retiring.len());

            for pool_id in &retiring {
                if let Some(pool_reg) = Arc::make_mut(&mut self.pool_params).remove(pool_id) {
                    retired_set.insert(*pool_id);
                    let op_key = Self::reward_account_to_hash(&pool_reg.reward_account);
                    let is_registered = self.reward_accounts.contains_key(&op_key)
                        || self.delegations.contains_key(&op_key);
                    if is_registered {
                        *Arc::make_mut(&mut self.reward_accounts)
                            .entry(op_key)
                            .or_insert(Lovelace(0)) += pool_deposit;
                    } else {
                        // Operator reward account is unregistered: deposit goes to treasury
                        // (Haskell POOLREAP: unclaimed deposits → treasury)
                        self.treasury += pool_deposit;
                    }
                    // Deposit was stored under pool_id (28 bytes, zero-padded to 32).
                    // Must refund with the same key, NOT op_key.
                    let pool_dep_key = {
                        let mut k = [0u8; 32];
                        k[..28].copy_from_slice(pool_id);
                        k
                    };
                    self.deposit_tracker
                        .refund_deposit(&pool_dep_key, super::state::DepositType::Pool);
                    tracing::info!(
                        target: "ChainDB.LedgerEvent",
                        "Pool retired at epoch→{}: {} (deposit {} refunded, registered={})",
                        new_epoch.0,
                        hex::encode(&pool_id[..8]),
                        pool_deposit.0,
                        is_registered,
                    );
                }
            }

            // Haskell POOLREAP: removeStakePoolDelegations
            // Remove pool delegations from all accounts delegating to retired pools.
            // Matches Haskell: certDStateL.accountsL %~ removeStakePoolDelegations retired
            if !retired_set.is_empty() {
                let delegations = Arc::make_mut(&mut self.delegations);
                let before = delegations.len();
                delegations.retain(|_, pool_id| !retired_set.contains(pool_id));
                let removed = before - delegations.len();
                if removed > 0 {
                    tracing::info!(
                        target: "ChainDB.LedgerEvent",
                        "POOLREAP: removed {} delegations to {} retired pool(s) at epoch→{}",
                        removed,
                        retired_set.len(),
                        new_epoch.0,
                    );
                }
            }
        }

        // ENACT: Apply proposals ratified at the PREVIOUS epoch boundary.
        //
        // Matches Haskell's NEWEPOCH STS order: EPOCH (→ SNAP) then ENACT then RATIFY.
        // ENACT runs AFTER the mark snapshot, so the deposit does NOT appear in this epoch's
        // mark. It will appear in the mark at the NEXT epoch boundary (one epoch later).
        //
        // Example: proposal passes RATIFY at epoch N → pending_enactments.
        //   Epoch N+1: mark taken (deposit NOT in reward_accounts) → ENACT (deposit added)
        //   Epoch N+2: mark taken (deposit IS in reward_accounts) ← matches Haskell
        let pending = std::mem::take(&mut self.pending_enactments);
        if !pending.is_empty() {
            tracing::info!(
                target: "ChainDB.LedgerEvent",
                epoch = new_epoch.0,
                n = pending.len(),
                "ENACT: applying {} proposal(s) ratified at previous epoch boundary",
                pending.len()
            );
            for enactment in pending {
                self.enact_gov_action(&enactment.gov_action);
                if enactment.deposit.0 > 0 {
                    *Arc::make_mut(&mut self.reward_accounts)
                        .entry(enactment.return_cred_hash)
                        .or_insert(Lovelace(0)) += enactment.deposit;
                    self.deposit_tracker.refund_deposit(
                        &enactment.return_cred_hash,
                        super::state::DepositType::Governance(enactment.action_id),
                    );
                    tracing::debug!(
                        target: "ChainDB.LedgerEvent",
                        action_id = %hex::encode(&enactment.action_id.tx_hash),
                        deposit = enactment.deposit.0,
                        return_cred = %hex::encode(&enactment.return_cred_hash[..28]),
                        "ENACT: deposit returned to reward_accounts"
                    );
                }
            }
        }

        // NOTE: Future pool re-registrations are now activated inside POOLREAP above,
        // matching Haskell where psStakePools merges psFutureStakePools before retirement.

        // Update current_epoch_fees (Haskell's ssFee) to fees from the epoch that just ended
        // This will be used by RUPD at the NEXT epoch boundary
        self.snapshots.current_epoch_fees = self.epoch_fees;

        // Step 6: Apply pre-Conway protocol parameter updates (PPUP rule)
        //
        // Proposals submitted in epoch N carry CBOR target_epoch=N in their Update field.
        // They take effect at the N→(N+1) boundary, producing epoch N+1 state.
        //
        // Concretely: at transition old→new (self.epoch=old, new_epoch=new),
        // apply proposals with key = old.  Example: at 1→2, apply epoch-1
        // proposals (target_epoch=1) → epoch 2 gets d=0.
        //
        // Haskell's `votedValue` semantics:
        //   1. The proposals map is keyed by genesis delegate hash (last-write-wins)
        //   2. Group the map VALUES by equality
        //   3. If any group has count >= quorum, apply that update
        //   4. If no group reaches quorum, discard all proposals
        let ppup_key = self.epoch;
        if let Some(proposals) = self.pending_pp_updates.remove(&ppup_key) {
            let quorum = self.update_quorum;
            let n_proposals = proposals.len() as u64;
            tracing::debug!(
                target: "ChainDB.LedgerEvent",
                epoch = new_epoch.0,
                n_proposals,
                quorum,
                "Evaluating pre-Conway protocol parameter update proposals"
            );

            // Implement votedValue: group identical proposals, find one with >= quorum votes.
            // Since ProtocolParamUpdate is Eq but not Ord, we count groups by linear scan.
            let values: Vec<&crate::ledger::primitives::ProtocolParamUpdate> =
                proposals.values().collect();
            let mut voted: Option<&crate::ledger::primitives::ProtocolParamUpdate> = None;
            let mut used = vec![false; values.len()];
            for i in 0..values.len() {
                if used[i] {
                    continue;
                }
                let mut count = 1u64;
                for j in (i + 1)..values.len() {
                    if !used[j] && values[j] == values[i] {
                        count += 1;
                        used[j] = true;
                    }
                }
                if count >= quorum {
                    voted = Some(values[i]);
                    break;
                }
            }

            if let Some(winner) = voted {
                let changes = winner.format_changes(&self.protocol_params);
                if let Err(e) = self.apply_protocol_param_update(winner) {
                    tracing::warn!(target: "ChainDB.LedgerEvent", epoch = new_epoch.0, error = %e, "PPUP: failed to apply protocol parameter update");
                } else {
                    self.ppup_enacted_log = Some(format!(
                        "PPUP: enacted protocol parameter update epoch={} changes=\"{}\"",
                        new_epoch.0, changes
                    ));
                }
            } else {
                tracing::info!(
                    target: "ChainDB.LedgerEvent",
                    epoch = new_epoch.0,
                    n_proposals,
                    quorum,
                    "PPUP: no proposal reached quorum, discarding"
                );
            }
        }
        // Discard proposals from epochs before the current (old) epoch.
        // Proposals with key = self.epoch are still needed for the next transition
        // (they will be applied at (self.epoch+1) → (self.epoch+2)).
        self.pending_pp_updates
            .retain(|epoch, _| epoch.0 >= self.epoch.0);

        // Record the current epoch's d for use in this epoch's createRUpd (step 13).
        // The RUPD uses blocks from self.epoch_blocks_by_pool (epoch N blocks), so eta's
        // d-threshold must use epoch N's d.  prev_pp.decentralization = epoch N's d (captured
        // before PPUP changed it to epoch N+1's d).
        self.prev_epoch_decentralization = prev_pp.decentralization;

        // Step 8: Ratify governance proposals per CIP-1694.
        // Only active in Conway era (protocol_version_major >= 9).
        // In pre-Conway eras the governance state is empty so this is a no-op.
        let _ratified = self.ratify_proposals();

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
                        target: "ChainDB.LedgerEvent",
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
                target: "ChainDB.LedgerEvent",
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
                    target: "ChainDB.LedgerEvent",
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
                target: "ChainDB.LedgerEvent",
                "Expired {} committee members at epoch {}",
                expired_members.len(),
                new_epoch.0
            );
        }

        // Snapshot DRep power distribution for use in next epoch's ratification.
        // Matches Haskell's `setFreshDRepPulsingState` at each epoch boundary.
        // Ratification at epoch N+1→N+2 uses the snapshot frozen at N→N+1.
        // Only meaningful in Conway era (vote_delegations will be empty otherwise).
        if !self.governance.vote_delegations.is_empty() {
            let (drep_power, no_confidence, abstain) =
                self.compute_drep_distribution_for_snapshot();
            let gov = Arc::make_mut(&mut self.governance);
            gov.drep_power_snapshot = drep_power;
            gov.drep_no_confidence_snapshot = no_confidence;
            gov.drep_abstain_snapshot = abstain;
        }

        // Step 12: Compute new epoch nonce per Haskell TICKN rule
        self.compute_epoch_nonce();

        // ===================================================================
        // Step 13: createRUpd — compute new reward update for DEFERRED application
        // ===================================================================
        // Matches Haskell's createRUpd / setFreshDRepPulsingState timing.
        // This RUPD will be stored in pending_reward_update and applied at the
        // NEXT epoch boundary (step 1 of the next process_epoch_transition call).
        //
        // Inputs (built into the "pay" snapshot):
        //   pay_snapshot = go's stake/delegations/pool_params (from 2 epochs ago)
        //                + self.epoch_blocks_by_pool (just-ended epoch N's blocks = nesBprev)
        //   curPp  = self.protocol_params (epoch N+1's params, after PPUP)
        //   feeSS  = self.snapshots.current_epoch_fees
        //
        // Why this is correct:
        //   - Haskell's createRUpd uses: go.stake (2 epochs ago) + nesBprev (epoch N blocks)
        //   - After 3-snapshot rotation: self.snapshots.go = old set = 2-epoch-old stake ✓
        //   - self.epoch_blocks_by_pool = blocks from epoch N (before clearing in step 14) ✓
        //   - Haskell's startStep uses the CURRENT (post-PPUP) PParams for hardforkBabbageForgoRewardPrefilter
        //     and η calculation (confirmed: epoch 3→4 uses d=0/pv=7, not prev_pp d=1/pv=6) ✓
        let fees_for_rewards = self.snapshots.current_epoch_fees;

        // Build "pay" snapshot: go's stake + current epoch's blocks (nesBprev equivalent).
        // This is NOT a standard rotation — it's a derived snapshot combining two different
        // time-points of data, exactly as Haskell's createRUpd does implicitly.
        self.snapshots.pay = self.snapshots.go.as_ref().map(|go| {
            let mut pay = go.clone();
            pay.epoch_blocks_by_pool = Arc::clone(&self.epoch_blocks_by_pool);
            pay
        });

        tracing::debug!(
            target: "ChainDB.LedgerEvent",
            "createRUpd: epoch={}, pay_stake_epoch={}, pay_blocks={}, fees={},",
            self.epoch.0,
            self.snapshots.pay.as_ref().map(|p| p.epoch.0).unwrap_or(0),
            self.snapshots.pay.as_ref().map(|p| p.epoch_blocks_by_pool.values().sum::<u64>()).unwrap_or(0),
            fees_for_rewards.0,
        );
        tracing::debug!(
            target: "ChainDB.LedgerEvent",
            "RUPD: starting balances: reserves={}, treasury={}",
            self.reserves.0, self.treasury.0,
        );

        // Haskell's startStep uses `prevPParamsEpochStateL` for ALL formula params including
        // the protocol version for hardforkBabbageForgoRewardPrefilter. prev_pp is captured
        // before PPUP fires, so prev_pp.pv = epoch N-1's protocol version.
        // E.g. epoch 3→4: prev_pp.pv=6 → prefilter=true → unregistered accounts excluded;
        //      epoch 4→5: prev_pp.pv=7 → prefilter=false → unregistered accounts included in rs.
        let new_rupd = if let Some(ref pay_snapshot) = self.snapshots.pay {
            self.calculate_rewards(pay_snapshot, fees_for_rewards, &prev_pp)
        } else {
            // Early epochs before go snapshot exists: no pools, no rewards
            let empty_snapshot = super::state::StakeSnapshot {
                epoch: self.epoch,
                delegations: Arc::new(HashMap::new()),
                pool_stake: HashMap::new(),
                pool_params: Arc::clone(&self.pool_params),
                stake_distribution: Arc::new(HashMap::new()),
                epoch_blocks_by_pool: Arc::new(HashMap::new()),
                script_stake_credentials: std::collections::HashSet::new(),
            };
            self.calculate_rewards(&empty_snapshot, fees_for_rewards, &prev_pp)
        };

        self.pending_reward_update = Some(new_rupd);

        // Step 14: Reset per-epoch accumulators
        self.epoch_fees = Lovelace(0);
        Arc::make_mut(&mut self.epoch_blocks_by_pool).clear();
        self.epoch_block_count = 0;

        self.epoch = new_epoch;
    }

    /// Compute DRep voting power distribution from live state for snapshotting.
    ///
    /// Uses Haskell's `computeDRepDistr` formula:
    ///   stake[cred] = instantStake[cred] + rewardBalance[cred] + proposalDeposits[cred]
    /// Only active registered DReps (KeyHash/ScriptHash) are included in the cache.
    /// Returns (drep_cache, no_confidence_stake, abstain_stake).
    pub(crate) fn compute_drep_distribution_for_snapshot(
        &self,
    ) -> (
        std::collections::HashMap<crate::ledger::primitives::Hash32, u64>,
        u64,
        u64,
    ) {
        let mut cache = std::collections::HashMap::new();
        let mut no_confidence = 0u64;
        let mut abstain = 0u64;
        for (stake_cred, drep) in &self.governance.vote_delegations {
            let utxo = self
                .stake_distribution
                .stake_map
                .get(stake_cred)
                .map(|l| l.0)
                .unwrap_or(0);
            let reward = self
                .reward_accounts
                .get(stake_cred)
                .map(|l| l.0)
                .unwrap_or(0);
            let gov_deps = self
                .deposit_tracker
                .governance_deposits_by_return_cred(stake_cred);
            let stake = utxo + reward + gov_deps;
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
                DRep::AlwaysNoConfidence => no_confidence += stake,
                DRep::AlwaysAbstain => abstain += stake,
            }
        }
        (cache, no_confidence, abstain)
    }

    /// Apply Conway genesis: initialize governance state from the Conway genesis configuration.
    ///
    /// Called when the first Conway-era block is detected, BEFORE the epoch transition.
    /// Sets up `conway_cur_params`, committee, constitution, and marks the genesis epoch.
    pub fn apply_conway_genesis(
        &mut self,
        genesis: &crate::genesis::ConwayGenesis,
        new_epoch: EpochNo,
    ) {
        if self.conway_genesis_epoch.is_some() {
            return; // Already applied
        }

        // Build conway_cur_params: copy current protocol_params and apply Conway overrides.
        // Protocol version is set to 9 (Conway).
        let mut cur_params = self.protocol_params.clone();
        cur_params.protocol_version_major = 9;
        cur_params.protocol_version_minor = 0;

        if let Some(v) = genesis.committee_max_term_length {
            cur_params.committee_max_term_length = v;
        }
        if let Some(v) = genesis.gov_action_lifetime {
            cur_params.gov_action_lifetime = v;
        }
        if let Some(v) = genesis.gov_action_deposit {
            cur_params.gov_action_deposit = v;
        }
        if let Some(v) = genesis.d_rep_deposit {
            cur_params.drep_deposit = v;
        }
        if let Some(v) = genesis.d_rep_activity {
            cur_params.drep_activity_period = v;
        }
        if let Some(v) = genesis.committee_min_size {
            cur_params.committee_min_size = v;
        }

        // Apply pool voting thresholds from genesis
        if let Some(pvt) = &genesis.pool_voting_thresholds {
            cur_params.pvt_motion_no_confidence = Self::f64_to_rational(pvt.motion_no_confidence);
            cur_params.pvt_committee_normal = Self::f64_to_rational(pvt.committee_normal);
            cur_params.pvt_committee_no_confidence =
                Self::f64_to_rational(pvt.committee_no_confidence);
            cur_params.pvt_hard_fork = Self::f64_to_rational(pvt.hard_fork_initiation);
            cur_params.pvt_pp_security_group = Self::f64_to_rational(pvt.pp_security_group);
        }

        // Apply DRep voting thresholds from genesis
        if let Some(dvt) = &genesis.d_rep_voting_thresholds {
            cur_params.dvt_motion_no_confidence = Self::f64_to_rational(dvt.motion_no_confidence);
            cur_params.dvt_committee_normal = Self::f64_to_rational(dvt.committee_normal);
            cur_params.dvt_committee_no_confidence =
                Self::f64_to_rational(dvt.committee_no_confidence);
            cur_params.dvt_constitution = Self::f64_to_rational(dvt.update_to_constitution);
            cur_params.dvt_hard_fork = Self::f64_to_rational(dvt.hard_fork_initiation);
            cur_params.dvt_pp_network_group = Self::f64_to_rational(dvt.pp_network_group);
            cur_params.dvt_pp_economic_group = Self::f64_to_rational(dvt.pp_economic_group);
            cur_params.dvt_pp_technical_group = Self::f64_to_rational(dvt.pp_technical_group);
            cur_params.dvt_pp_gov_group = Self::f64_to_rational(dvt.pp_gov_group);
            cur_params.dvt_treasury_withdrawal = Self::f64_to_rational(dvt.treasury_withdrawal);
        }

        Arc::make_mut(&mut self.governance).conway_cur_params = Some(Box::new(cur_params));

        // Initialize committee from genesis
        if let Some(committee) = &genesis.committee {
            let gov = Arc::make_mut(&mut self.governance);
            gov.committee_expiration.clear();
            for (cred_str, term_end) in &committee.members {
                if let Some(hash) = Self::parse_credential_key(cred_str) {
                    // Mark script credentials
                    if cred_str.starts_with("scriptHash-") {
                        gov.script_committee_credentials.insert(hash);
                    }
                    gov.committee_expiration.insert(hash, EpochNo(*term_end));
                }
            }
            if let Some(threshold) = &committee.threshold {
                gov.committee_threshold = Some(Rational {
                    numerator: threshold.numerator,
                    denominator: threshold.denominator,
                });
            }
        }

        // Initialize constitution from genesis
        if let Some(c) = &genesis.constitution {
            let anchor = c.anchor.as_ref().map(|a| {
                let mut hash = [0u8; 32];
                if let Ok(hash_bytes) = hex::decode(&a.data_hash) {
                    let n = hash_bytes.len().min(32);
                    hash[..n].copy_from_slice(&hash_bytes[..n]);
                }
                Anchor {
                    url: a.url.clone(),
                    hash,
                }
            });
            let script_hash = c.script.as_ref().and_then(|s| {
                let bytes = hex::decode(s).ok()?;
                if bytes.len() >= 28 {
                    let mut hash = [0u8; 32];
                    hash[..28].copy_from_slice(&bytes[..28]);
                    Some(hash)
                } else {
                    None
                }
            });
            Arc::make_mut(&mut self.governance).constitution = Some(Constitution {
                anchor,
                script_hash,
            });
        }

        self.conway_genesis_epoch = Some(new_epoch.0);

        tracing::info!(target: "ChainDB.LedgerEvent", epoch = new_epoch.0, "Applied Conway genesis");
    }

    /// Convert a floating-point threshold (e.g., 0.67) to a Rational with denominator 100.
    fn f64_to_rational(v: f64) -> Rational {
        let d = 100u64;
        let n = (v * d as f64).round() as u64;
        Rational {
            numerator: n,
            denominator: d,
        }
    }

    /// Parse a credential string like "keyHash-aabbcc..." or "scriptHash-aabbcc..."
    /// into a 32-byte padded hash (credential stored as Hash32 with 28 bytes of hash data).
    fn parse_credential_key(cred_str: &str) -> Option<Hash32> {
        let hex_part = cred_str
            .strip_prefix("keyHash-")
            .or_else(|| cred_str.strip_prefix("scriptHash-"))?;

        let bytes = hex::decode(hex_part).ok()?;
        if bytes.len() < 28 {
            return None;
        }
        let mut hash = [0u8; 32];
        hash[..28].copy_from_slice(&bytes[..28]);
        Some(hash)
    }

    /// Dump epoch state to JSON file for comparison with Haskell cardano-node
    pub fn dump_epoch_state(&self, dump_dir: &Path, slot: u64) -> Result<(), std::io::Error> {
        let filename = format!("{}-hayate.json", self.epoch.0);
        let filepath = dump_dir.join(filename);

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
                // Use the snapshot's own script credentials for correct type tagging,
                // even if credentials were later deregistered from the live ledger state.
                let snap_script_creds = &snap.script_stake_credentials;
                let snap_cred_tag = |k: &Hash32| -> String {
                    let hex = hex::encode(&k[..28]);
                    if snap_script_creds.contains(k) {
                        format!("scriptHash-{}", hex)
                    } else {
                        format!("keyHash-{}", hex)
                    }
                };
                let snap_stake: serde_json::Map<String, serde_json::Value> = snap
                    .stake_distribution
                    .iter()
                    .map(|(k, v)| (snap_cred_tag(k), json!(v.0)))
                    .collect();
                let snap_delegations: serde_json::Map<String, serde_json::Value> = snap
                    .delegations
                    .iter()
                    .map(|(k, v)| (snap_cred_tag(k), json!(hex::encode(v))))
                    .collect();
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
                    "stake": snap_stake,
                    "delegations": snap_delegations,
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
        let total_deposits = total_pool_deposits
            + total_stake_deposits
            + total_governance_deposits
            + total_drep_deposits;

        // Include RUPD intermediate values for comparison with Haskell.
        // `rupd` = the RUPD that was applied at this epoch boundary (computed at the previous
        //          epoch boundary).  Comparison: haskell[N-1].rupdNext == hayate[N].rupd
        // `rupdNext` = the RUPD computed at this epoch boundary, to be applied at the next.
        //              Comparison: haskell[N].rupdNext == hayate[N].rupdNext
        let rupd_values = if let Some(rupd) = &self.last_applied_rupd {
            tracing::debug!(target: "ChainDB.LedgerEvent", "Dumping RUPD: eta={}, deltaR1={}", rupd.eta, rupd.delta_r1);
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
            tracing::debug!(
                target: "ChainDB.LedgerEvent",
                "last_applied_rupd is None when dumping epoch {}",
                self.epoch.0
            );
            json!(null)
        };

        let rupd_next_values = if let Some(rupd) = &self.pending_reward_update {
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
            json!(null)
        };

        let pp = &self.protocol_params;
        let rho_f = pp.rho.numerator as f64 / pp.rho.denominator.max(1) as f64;
        let tau_f = pp.tau.numerator as f64 / pp.tau.denominator.max(1) as f64;
        let a0_f = pp.a0.numerator as f64 / pp.a0.denominator.max(1) as f64;
        let d_f =
            pp.decentralization.numerator as f64 / pp.decentralization.denominator.max(1) as f64;

        // Conway era = governance has conway_cur_params initialized (set at Conway genesis)
        let is_conway = self.governance.conway_cur_params.is_some();
        // Era name from protocol major version, matching Haskell's snapshotEraName field.
        let era_name = match self.protocol_params.protocol_version_major {
            0..=1 => "Byron",
            2 => "Shelley",
            3 => "Allegra",
            4 => "Mary",
            5..=6 => "Alonzo",
            7..=8 => "Babbage",
            9..=11 => "Conway",
            _ => "Dijkstra",
        };

        // activeStake = sum of all go snapshot stake (= delegated lovelace in go snapshot)
        // Matches Haskell's sumAllStake (ssStake goSnap)
        let active_stake: u64 = self
            .snapshots
            .go
            .as_ref()
            .map(|go| go.stake_distribution.values().map(|l| l.0).sum())
            .unwrap_or(0);

        // Helper: serialize a Rational as {numerator, denominator}
        let rat_json =
            |r: Rational| json!({"numerator": r.numerator, "denominator": r.denominator});

        // Helper: serialize full protocol parameters (both Babbage and Conway fields)
        let serialize_pp = |p: &ProtocolParameters| -> serde_json::Value {
            json!({
                "protocolVersion": {"major": p.protocol_version_major, "minor": p.protocol_version_minor},
                "txFeePerByte": p.min_fee_a,
                "txFeeFixed": p.min_fee_b,
                "maxBlockBodySize": p.max_block_body_size,
                "maxTxSize": p.max_transaction_size,
                "maxBlockHeaderSize": p.max_block_header_size,
                "stakeAddressDeposit": p.key_deposit,
                "stakePoolDeposit": p.pool_deposit,
                "poolRetireMaxEpoch": p.e_max,
                "stakePoolTargetNum": p.n_opt,
                "poolPledgeInfluence": rat_json(p.a0),
                "monetaryExpansion": rat_json(p.rho),
                "treasuryCut": rat_json(p.tau),
                "minPoolCost": p.min_pool_cost,
                "utxoCostPerByte": p.utxo_cost_per_byte,
                "maxValueSize": p.max_value_size,
                "collateralPercentage": p.collateral_percentage,
                "maxCollateralInputs": p.max_collateral_inputs,
                "executionUnitPrices": {
                    "priceMemory": rat_json(p.price_mem),
                    "priceSteps": rat_json(p.price_step),
                },
                "maxTxExecutionUnits": {
                    "memory": p.max_tx_execution_units_mem,
                    "steps": p.max_tx_execution_units_steps,
                },
                "maxBlockExecutionUnits": {
                    "memory": p.max_block_execution_units_mem,
                    "steps": p.max_block_execution_units_steps,
                },
                "minFeeRefScriptCostPerByte": p.min_fee_ref_script_cost_per_byte,
                // Conway-specific
                "dRepDeposit": p.drep_deposit,
                "dRepActivity": p.drep_activity_period,
                "govActionLifetime": p.gov_action_lifetime,
                "govActionDeposit": p.gov_action_deposit,
                "committeeMinSize": p.committee_min_size,
                "committeeMaxTermLength": p.committee_max_term_length,
                "dRepVotingThresholds": {
                    "motionNoConfidence": rat_json(p.dvt_motion_no_confidence),
                    "committeeNormal": rat_json(p.dvt_committee_normal),
                    "committeeNoConfidence": rat_json(p.dvt_committee_no_confidence),
                    "hardForkInitiation": rat_json(p.dvt_hard_fork),
                    "ppNetworkGroup": rat_json(p.dvt_pp_network_group),
                    "ppEconomicGroup": rat_json(p.dvt_pp_economic_group),
                    "ppTechnicalGroup": rat_json(p.dvt_pp_technical_group),
                    "ppGovGroup": rat_json(p.dvt_pp_gov_group),
                    "treasuryWithdrawal": rat_json(p.dvt_treasury_withdrawal),
                    "updateToConstitution": rat_json(p.dvt_constitution),
                    "noConfidence": rat_json(p.dvt_no_confidence),
                },
                "poolVotingThresholds": {
                    "motionNoConfidence": rat_json(p.pvt_motion_no_confidence),
                    "committeeNormal": rat_json(p.pvt_committee_normal),
                    "committeeNoConfidence": rat_json(p.pvt_committee_no_confidence),
                    "hardForkInitiation": rat_json(p.pvt_hard_fork),
                    "ppSecurityGroup": rat_json(p.pvt_pp_security_group),
                },
            })
        };

        // Compute conwayGov JSON (only populated in Conway era)
        let conway_gov = if is_conway {
            let gov = &*self.governance;

            // Committee members: cold_cred_hex -> expiration_epoch
            // Committee credentials are stored as Hash32 (28-byte hash padded to 32).
            // Output only the first 28 bytes (56 hex chars) to match Haskell format.
            let committee_members: serde_json::Map<String, serde_json::Value> = gov
                .committee_expiration
                .iter()
                .map(|(cold_hash, exp)| {
                    let key = if gov.script_committee_credentials.contains(cold_hash) {
                        format!("scriptHash-{}", hex::encode(&cold_hash[..28]))
                    } else {
                        format!("keyHash-{}", hex::encode(&cold_hash[..28]))
                    };
                    (key, json!(exp.0))
                })
                .collect();

            // Committee threshold
            let committee_threshold = gov
                .committee_threshold
                .as_ref()
                .map(|r| json!({"numerator": r.numerator, "denominator": r.denominator}))
                .unwrap_or(json!(null));

            // Committee state: cold_cred -> hot credential status
            let cs_creds: serde_json::Map<String, serde_json::Value> = {
                let mut map = serde_json::Map::new();
                // Hot key authorizations
                for (cold_hash, hot_hash) in &gov.committee_hot_keys {
                    let cold_key = if gov.script_committee_credentials.contains(cold_hash) {
                        format!("scriptHash-{}", hex::encode(&cold_hash[..28]))
                    } else {
                        format!("keyHash-{}", hex::encode(&cold_hash[..28]))
                    };
                    let hot_tag = if gov.script_committee_hot_credentials.contains(hot_hash) {
                        json!({"tag": "CommitteeHotCredential", "contents": {"scriptHash": hex::encode(&hot_hash[..28])}})
                    } else {
                        json!({"tag": "CommitteeHotCredential", "contents": {"keyHash": hex::encode(&hot_hash[..28])}})
                    };
                    map.insert(cold_key, hot_tag);
                }
                // Resignations
                for (cold_hash, _anchor) in &gov.committee_resigned {
                    let cold_key = if gov.script_committee_credentials.contains(cold_hash) {
                        format!("scriptHash-{}", hex::encode(&cold_hash[..28]))
                    } else {
                        format!("keyHash-{}", hex::encode(&cold_hash[..28]))
                    };
                    map.entry(cold_key)
                        .or_insert(json!({"tag": "MemberResigned", "contents": null}));
                }
                map
            };

            // Constitution
            let constitution_json = if let Some(c) = &gov.constitution {
                let anchor_json = if let Some(a) = &c.anchor {
                    json!({"url": a.url, "dataHash": hex::encode(a.hash)})
                } else {
                    json!(null)
                };
                // Script hash is 28 bytes stored in Hash32 (padded); output first 28 bytes
                let script_json = c
                    .script_hash
                    .map(|h| json!(hex::encode(&h[..28])))
                    .unwrap_or(json!(null));
                json!({"anchor": anchor_json, "script": script_json})
            } else {
                json!(null)
            };

            // DRep stake distribution: compute from live state per Haskell's computeDRepDistr.
            // Formula: stake = instantStake[cred] + rewardBalance[cred] + proposalDeposits[cred]
            // instantStake = UTxO-owning credentials (NOT pool-delegation-restricted).
            // This matches Haskell's `instantStakeCredentialsL` from the live UTxO map.
            let drep_distr: serde_json::Map<String, serde_json::Value> = {
                let mut distr: std::collections::BTreeMap<String, u64> =
                    std::collections::BTreeMap::new();
                for (stake_cred, drep) in &gov.vote_delegations {
                    let utxo = self
                        .stake_distribution
                        .stake_map
                        .get(stake_cred)
                        .map(|l| l.0)
                        .unwrap_or(0);
                    let reward = self
                        .reward_accounts
                        .get(stake_cred)
                        .map(|l| l.0)
                        .unwrap_or(0);
                    let gov_deps = self
                        .deposit_tracker
                        .governance_deposits_by_return_cred(stake_cred);
                    let stake = utxo + reward + gov_deps;
                    if stake == 0 {
                        continue;
                    }
                    let key = match drep {
                        DRep::KeyHash(h) => format!("drep-keyHash-{}", hex::encode(&h[..28])),
                        DRep::ScriptHash(h) => format!("drep-scriptHash-{}", hex::encode(&h[..28])),
                        DRep::AlwaysAbstain => "drep-alwaysAbstain".to_string(),
                        DRep::AlwaysNoConfidence => "drep-alwaysNoConfidence".to_string(),
                    };
                    *distr.entry(key).or_insert(0) += stake;
                }
                distr.into_iter().map(|(k, v)| (k, json!(v))).collect()
            };

            // prevGovActionIds: last enacted action ID per type
            let format_action_id = |opt: Option<&GovActionId>| -> serde_json::Value {
                match opt {
                    Some(id) => json!({"txId": hex::encode(id.tx_hash), "govActionIx": id.index}),
                    None => json!(null),
                }
            };
            let prev_gov_action_ids = json!({
                "Committee": format_action_id(gov.enacted_committee.as_ref()),
                "Constitution": format_action_id(gov.enacted_constitution.as_ref()),
                "HardFork": format_action_id(gov.enacted_hard_fork.as_ref()),
                "PParamUpdate": format_action_id(gov.enacted_pparam_update.as_ref()),
            });

            let prev_pp_json = self
                .prev_protocol_params
                .as_ref()
                .map(|p| serialize_pp(p))
                .unwrap_or_else(|| serialize_pp(pp));

            // curPParams = Conway governance's current params (initialized from Conway genesis,
            // updated by ParameterChange governance actions). Falls back to protocol_params.
            let conway_cur = self.governance.conway_cur_params.as_deref().unwrap_or(pp);

            json!({
                "committee": {
                    "members": committee_members,
                    "threshold": committee_threshold,
                },
                "committeeState": {
                    "csCommitteeCreds": cs_creds,
                },
                "constitution": constitution_json,
                "drepDistr": drep_distr,
                "nextEnactState": {
                    "committee": {
                        "members": committee_members,
                        "threshold": committee_threshold,
                    },
                    "constitution": constitution_json,
                    "curPParams": serialize_pp(conway_cur),
                    "prevPParams": prev_pp_json,
                    "prevGovActionIds": prev_gov_action_ids,
                },
            })
        } else {
            json!(null)
        };

        let json_output = json!({
            "epoch": self.epoch.0,
            "slot": slot,
            "snapshotEraName": era_name,
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
            "conwayGov": conway_gov,
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
            "rupdNext": rupd_next_values,
            "snapshots": {
                "mark": format_snapshot("mark", &self.snapshots.mark),
                "set": format_snapshot("set", &self.snapshots.set),
                "go": format_snapshot("go", &self.snapshots.go),
                "pay": format_snapshot("pay", &self.snapshots.pay),
            }
        });

        std::fs::write(&filepath, serde_json::to_string_pretty(&json_output)?)?;
        tracing::debug!(
            target: "ChainDB.LedgerEvent",
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
                script_stake_credentials: std::collections::HashSet::new(),
            }),
            set: Some(StakeSnapshot {
                epoch: EpochNo(99),
                delegations: Arc::new(HashMap::new()),
                pool_stake: HashMap::new(),
                pool_params: Arc::new(HashMap::new()),
                stake_distribution: Arc::new(HashMap::new()),
                epoch_blocks_by_pool: Arc::new(HashMap::new()),
                script_stake_credentials: std::collections::HashSet::new(),
            }),
            go: Some(StakeSnapshot {
                epoch: EpochNo(98),
                delegations: Arc::new(HashMap::new()),
                pool_stake: HashMap::new(),
                pool_params: Arc::new(HashMap::new()),
                stake_distribution: Arc::new(HashMap::new()),
                epoch_blocks_by_pool: Arc::new(HashMap::new()),
                script_stake_credentials: std::collections::HashSet::new(),
            }),
            pay: None,
            current_epoch_fees: Lovelace(0),
        };

        state.process_epoch_transition(EpochNo(101));

        // Verify rotation: go = old set, set = old mark, mark = new (epoch that ended)
        // pay = go's stake (epoch 99) + current blocks
        assert_eq!(state.snapshots.go.as_ref().unwrap().epoch.0, 99);
        assert_eq!(state.snapshots.set.as_ref().unwrap().epoch.0, 100);
        assert_eq!(state.snapshots.mark.as_ref().unwrap().epoch.0, 100);
        // pay = go after rotation (epoch 99) with updated blocks
        assert_eq!(state.snapshots.pay.as_ref().unwrap().epoch.0, 99);
    }
}
