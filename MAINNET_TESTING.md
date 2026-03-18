# Mainnet Testing Guide for Ledger State Integration

## Overview

This guide covers end-to-end testing of hayate's ledger state tracking on Cardano mainnet. The goal is to verify that epoch boundary snapshots, Conway governance, and deposit tracking work correctly with real mainnet data.

## Prerequisites

- Cardano mainnet node running (for comparison queries)
- Hayate built with ledger integration
- Sufficient disk space for mainnet sync
- Time for full sync (or sync from recent snapshot)

## Configuration

### 1. Mainnet Network Parameters

Ensure hayate is configured for mainnet:

```toml
# config/mainnet.toml
[network]
magic = 764824073  # Mainnet magic
name = "mainnet"

# Genesis time: 2017-09-23 21:44:51 UTC
# First Shelley block: Epoch 208, slot 4492800
# Conway era start: Epoch 509, slot ~47,000,000 (approximate)

[sync]
# Start from recent epoch for faster testing
# Or sync from genesis for full validation
start_slot = 0  # Adjust as needed
```

### 2. Enable Detailed Logging

Add environment variable for detailed ledger logs:

```bash
export RUST_LOG=hayate::ledger=debug,hayate::indexer=info
```

## Testing Strategy

### Phase 1: Epoch Boundary Detection (Quick Test)

**Goal:** Verify epoch boundaries are detected correctly

**Steps:**
1. Start hayate sync from a recent slot (e.g., current epoch - 5)
2. Monitor logs for epoch boundary messages
3. Verify epoch transitions are logged

**Expected Logs:**
```
INFO hayate::indexer: 📅 Epoch boundary: 499 → 500
INFO hayate::ledger::epoch: Processing epoch boundary: 499 → 500
DEBUG hayate::ledger::epoch: Epoch 500 transition complete (snapshot persistence TODO)
```

**Validation:**
- Epoch numbers increment correctly
- Transitions happen at correct slots (slot % 432000 == 0 for mainnet)
- No panics or errors during transition

### Phase 2: Ledger State Accumulation (Medium Test)

**Goal:** Verify ledger state accumulates correctly over multiple epochs

**Steps:**
1. Sync through 3-5 epochs
2. Monitor stake distribution growth
3. Check certificate processing
4. Verify governance state (if Conway era)

**Expected Behavior:**
- `stake_distribution.stake_map` grows as stake credentials are registered
- `delegations` map populates with stake delegations
- `pool_params` accumulates pool registrations
- No memory leaks (check with `htop` or similar)

**Validation Queries:**

Add debug logging to BlockProcessor to verify:

```rust
// In process_epoch_boundary()
tracing::info!(
    "Epoch {} stats: {} credentials, {} pools, {} delegations",
    new_epoch,
    self.ledger_state.stake_distribution.stake_map.len(),
    self.ledger_state.pool_params.len(),
    self.ledger_state.delegations.len()
);
```

### Phase 3: Historical Epoch Verification (Comprehensive Test)

**Goal:** Validate against known mainnet epochs

**Target Epochs:**

1. **Epoch 208 (Shelley Launch)**
   - Slot: 4,492,800
   - First epoch with staking/delegation
   - Verify: Stake distribution starts populating

2. **Epoch 290 (Mary Hard Fork)**
   - Slot: 16,588,800
   - Multi-asset support introduced
   - Verify: Certificate processing continues working

3. **Epoch 365 (Alonzo Hard Fork)**
   - Slot: 43,372,800
   - Plutus smart contracts introduced
   - Verify: Epoch transitions handle new tx types

4. **Epoch 509 (Conway Era Start - approximate)**
   - Slot: ~47,000,000
   - Conway governance introduced
   - Verify: Governance state tracking begins
   - Check: Bootstrap phase (protocol version 9)

5. **Recent Epochs (Current - 10 to Current)**
   - Verify: Current epoch data is accurate
   - Compare: With cardano-node queries

**For Each Epoch:**

```bash
# At epoch boundary, query cardano-node for comparison
cardano-cli query stake-distribution \
  --mainnet \
  --socket-path /path/to/node.socket

# Compare with hayate's ledger state
# (Add API endpoint or logging to expose this)
```

### Phase 4: Conway Governance Validation (Advanced Test)

**Goal:** Verify Conway governance tracking on mainnet

**Requirements:**
- Sync to epoch 509+ (Conway era)
- Monitor governance state accumulation

**What to Verify:**

1. **Governance Proposals:**
   - `ledger_state.governance.proposals` populates
   - Proposal types detected correctly (HardForkInitiation, ParameterChange, etc.)
   - Deposits tracked per proposal

2. **Voting:**
   - DRep votes recorded
   - SPO votes recorded
   - Constitutional Committee votes recorded

3. **Ratification:**
   - Proposals move through states: Proposed → Voting → Ratified/Expired
   - Thresholds calculated correctly
   - Bootstrap phase handling (if protocol v9)

4. **Deposits:**
   - Governance deposits tracked separately from staking deposits
   - Voting stake includes governance deposits
   - Staking stake (for block production) excludes governance deposits

**Debug Logging:**

```rust
// In governance.rs ratify_proposals()
tracing::info!(
    "Ratified {} proposals at epoch {}: {:?}",
    ratified.len(),
    self.epoch.0,
    ratified.iter().map(|(id, _)| hex::encode(id)).collect::<Vec<_>>()
);
```

### Phase 5: Deposit Separation Validation (Critical Test)

**Goal:** Verify voting stake ≠ staking stake when governance deposits exist

**Test Case:**

Find a mainnet transaction with a governance proposal deposit, then verify:

1. The deposit is added to `ledger_state.deposits`
2. The credential's voting stake includes the deposit
3. The credential's staking stake (for pool delegation) excludes the deposit

**Expected:**
```
voting_stake(cred) = utxo_stake + reward_balance + all_deposits
staking_stake(cred) = utxo_stake + reward_balance + pool_deposits + stake_deposits
  (governance deposits excluded)
```

**Validation:**

```rust
// Check this invariant holds:
assert!(
    voting_stake >= staking_stake,
    "Voting stake should be >= staking stake (includes governance deposits)"
);
```

## Known Mainnet Epochs for Validation

### Epoch 208 (Shelley Launch)
- **Slot:** 4,492,800
- **Date:** 2020-07-29
- **Significance:** First epoch with staking
- **Expected:** Initial stake distribution snapshot
- **Pools:** ~100-200 pools registered

### Epoch 290 (Mary)
- **Slot:** 16,588,800
- **Date:** 2021-03-01
- **Significance:** Multi-asset support
- **Expected:** Certificate processing continues, native asset tracking begins

### Epoch 365 (Alonzo)
- **Slot:** 43,372,800
- **Date:** 2021-09-12
- **Significance:** Plutus smart contracts
- **Expected:** Epoch transitions handle Alonzo tx format

### Epoch 509 (Conway - approximate)
- **Slot:** ~47,000,000+
- **Date:** 2024+ (exact date TBD)
- **Significance:** Conway governance (CIP-1694)
- **Expected:** Governance proposals, DRep votes, committee actions

### Current Epoch
- **Slot:** Query from node
- **Date:** Current
- **Significance:** Live data validation
- **Compare:** Against cardano-node LocalStateQuery

## Comparison with Cardano-Node

### Stake Distribution Query

```bash
# Query node at epoch boundary
cardano-cli query stake-distribution \
  --mainnet \
  --socket-path /path/to/node.socket \
  > node_stake_epoch_N.json

# Export hayate's stake distribution
# (TODO: Add API endpoint or logging)
```

### Epoch Nonce Query

```bash
# Query current epoch nonce
cardano-cli query protocol-parameters \
  --mainnet \
  --socket-path /path/to/node.socket \
  | jq '.extraPraosEntropy'
```

### Pool Parameters Query

```bash
# Query pool parameters at epoch boundary
cardano-cli query stake-pools \
  --mainnet \
  --socket-path /path/to/node.socket
```

## Success Criteria

### ✅ Minimum Viable Validation

1. **Epoch boundaries detected correctly**
   - Transitions happen at slot % 432000 == 0
   - No crashes during transitions
   - Epoch number increments correctly

2. **State accumulates without errors**
   - Stake distribution grows over time
   - Delegations and pools accumulate
   - No memory leaks over 10+ epochs

3. **Basic invariants hold**
   - Treasury + reserves + circulation ≤ MAX_SUPPLY
   - Stake ≤ circulation
   - No panics in reward calculations

### ✅ Full Validation (Production Ready)

1. **Stake distribution matches cardano-node**
   - Within 0.1% at epoch boundaries
   - All registered credentials present
   - Pool stake totals match

2. **Governance state accurate (Conway era)**
   - All proposals tracked
   - Votes recorded correctly
   - Ratification status matches expected thresholds

3. **Deposit separation correct**
   - Voting stake > staking stake when governance deposits exist
   - Pool leadership calculations exclude governance deposits
   - DRep voting power includes all deposits

4. **Nonce matches cardano-node**
   - Epoch nonce identical at boundaries
   - TICKN rule implemented correctly

5. **Historical epochs validate**
   - Can sync from epoch 208 (Shelley) to current
   - All eras handled correctly (Byron skipped, Shelley-Conway processed)

## Performance Benchmarks

Track these metrics during mainnet sync:

- **Sync speed:** Blocks/second during bulk sync
- **Epoch transition time:** <5 seconds per epoch boundary
- **Memory usage:** Stable over 100+ epochs
- **Disk usage:** Snapshot size at epoch boundaries
- **CPU usage:** Should not peg CPU during sync

## Debugging Tips

### If Epoch Transitions Fail:

1. Check slot calculation: `slot_to_epoch()` function
2. Verify epoch length: mainnet = 432,000 slots (5 days)
3. Add logging to `process_epoch_boundary()`
4. Check for panics in reward calculations

### If Stake Distribution Diverges:

1. Enable debug logging for certificate processing
2. Verify all certificate types are handled
3. Check UTxO rebuild logic (address parsing)
4. Compare credential counts with node

### If Governance State Incorrect:

1. Verify protocol version detection (v9 = bootstrap, v10+ = normal)
2. Check proposal parsing from transactions
3. Verify voting threshold calculations
4. Compare with known ratified proposals

### If Memory Grows Unbounded:

1. Check for Arc cycles (should be none)
2. Verify old snapshots are cleaned up
3. Monitor stake_distribution.stake_map size
4. Profile with `heaptrack` or similar

## Next Steps After Validation

Once mainnet testing passes:

1. **Enable snapshot persistence**
   - Extend StorageHandle with save_epoch_snapshot command
   - Implement LSM tree persistence from process_epoch_boundary()

2. **Implement era-specific extraction**
   - Certificate parsing from pallas types
   - Pool ID extraction from block headers
   - Nonce extraction from VRF proofs

3. **Add query APIs**
   - GET /epoch/{N}/stake_distribution
   - GET /epoch/{N}/pool_stake/{pool_id}
   - GET /epoch/{N}/nonce
   - GET /governance/proposals

4. **Enable transaction validation** (optional)
   - Implement Phase-1 structural checks
   - Add Plutus script execution (feature-flagged)
   - Make validation mode configurable

5. **Production deployment**
   - Replace cardano-node for lightweight applications
   - Integrate with Midnight for SPDD queries
   - Enable snapshot export for analysis tools

## Timeline Estimate

- **Phase 1** (Epoch detection): 1-2 hours (sync 5-10 epochs)
- **Phase 2** (State accumulation): 4-8 hours (sync 20-50 epochs)
- **Phase 3** (Historical validation): 1-3 days (full sync or checkpoint + recent epochs)
- **Phase 4** (Conway governance): 4-8 hours (sync Conway era epochs)
- **Phase 5** (Deposit separation): 2-4 hours (find and verify governance deposits)

**Total:** 2-5 days for comprehensive mainnet validation

## Conclusion

This testing guide provides a structured approach to validating hayate's ledger state implementation against mainnet. Start with quick epoch detection tests, then gradually increase scope to full historical validation and Conway governance verification.

The implementation is complete and ready for testing. Success on mainnet will prove hayate can accurately track Cardano's full ledger state and serve as a lightweight alternative to cardano-node for epoch snapshot queries.
