# Ledger Lead Memory

## Confirmed Bug Fixes

### 1. Double-subtract reserves (epoch.rs)
`self.reserves.0 -= unregistered_rewards.0` was wrong. `rupd.undistributed` already accounts for
unregistered rewards. The extra subtraction caused reserves to be too low.

### 2. Wrong apparent performance denominator (rewards.rs:~303)
Was using `expected_blocks` as denominator. Should use `total_stake_pool_blocks.max(1)` (actual blocks
produced by all pools in the go snapshot's epoch_blocks_by_pool).

### 3. Epoch-boundary block counting (main.rs)
Epoch transition check MUST happen BEFORE `process_block_simple`. First block of new epoch must not
be counted in the previous epoch's snapshot.

### 4. Conway block ID extraction (main.rs)
`extract_pool_id_from_block` was missing the Conway branch. Conway blocks use same `issuer_vkey`
structure as Babbage. Without this, all Conway blocks had pool_id = None → eta = 0 → zero rewards.

### 5. Pool retirement timing (epoch.rs + main.rs)
- Retirement cert epoch N = pool is active THROUGH epoch N, retires at N→(N+1) boundary
- In epoch.rs: use `self.pending_retirements.remove(&self.epoch)` NOT `&new_epoch`
- POOL retirement MUST run BEFORE SNAP (mark snapshot)
- In main.rs: use `.entry().or_insert_with(Vec::new).push(pool_id)` NOT `.insert(..., vec![pool_id])`

## Key Invariants

- `self.epoch` = epoch that just ended at each transition
- `new_epoch` = `self.epoch + 1` = the epoch we're entering
- Mark snapshot captures: blocks from `self.epoch`, stake as of transition, pools after retirement
- RUPD uses: mark_snapshot.epoch_blocks_by_pool (actual blocks), go_snapshot stake
- DRep deposits: DepositType::DRep, stored in deposit_tracker.deposits[cred].drep
- vote_delegations live in GovernanceState, NOT in LedgerState directly

## Conway Cert Handlers (main.rs)
All implemented as of 2026-03:
- RegDRepCert, UnRegDRepCert, UpdateDRepCert
- VoteDeleg, StakeVoteDeleg, StakeRegDeleg, VoteRegDeleg, StakeVoteRegDeleg

## Files
- `src/ledger/epoch.rs` — NEWEPOCH/EPOCH STS, mark snapshot, retirements
- `src/ledger/rewards.rs` — RUPD/PulsingReward, pool reward calc
- `src/ledger/state.rs` — LedgerState, GovernanceState, DepositTracker, StakeSnapshot
- `src/ledger/governance.rs` — ratification, voting power, DRep activity
- `src/node/main.rs` — cert processing, block counting, epoch dumps
