# Cardano-Streamer: Conformance Testing Guide

## Overview

`cardano-streamer` (cstreamer) is a tool for replaying the Cardano blockchain using the same Haskell libraries as `cardano-node`. It's the **authoritative implementation** for validating hayate's ledger state calculations.

## Purpose

Since we can't query historical ledger state from `cardano-node` using `cardano-cli`, we use `cardano-streamer` to:
1. Replay the chain to specific slots/epochs
2. Dump ledger snapshots at those points
3. Compare with hayate's snapshots to verify correctness

## Command Structure

```bash
cardano-streamer <COMMAND> [OPTIONS]
```

### Commands

- `replay` - Replay the chain (default use case for conformance testing)
- `benchmark` - Replay with timing statistics
- `stats` - Calculate various chain statistics
- `rewards` - Calculate rewards and withdrawals per epoch

## Core Options for Conformance Testing

### Required Flags

```bash
--chain-dir CHAIN_DIR
```
Path to cardano-node's database directory (the `db/` folder)
- Example: `--chain-dir ~/work/iohk/midnight-playground/.run/sanchonet/cardano-node/db`

```bash
--config CONFIG
```
Path to the network configuration file
- Example: `--config ~/work/iohk/midnight-playground/.run/sanchonet/cardano-node/config.json`

### Snapshot Control

```bash
--start-slot SLOT (-r)
```
Start replaying from this slot number. Requires a snapshot at that slot to exist.
- Example: `--start-slot 0` (start from genesis)

```bash
--write-snapshot SLOT (-w)
```
Write a ledger snapshot at this slot number. Can be specified multiple times.
- Example: `--write-snapshot 86400` (snapshot at epoch 1 boundary)
- Example: `-w 86400 -w 172800 -w 259200` (multiple epochs)

```bash
--stop SLOT (-s)
```
Stop replaying at this slot number.
- Example: `--stop 86400` (stop at end of epoch 0)

```bash
--suffix SUFFIX
```
Optional suffix for snapshot filenames. Useful for organizing different test runs.
- Default: no suffix
- Example: `--suffix hayate_comparison`
- Snapshots saved as: `CHAIN_DIR/ledger/SLOT_NUMBER_SUFFIX`

### Output Control

```bash
--out-dir OUT_DIR
```
Directory where output files will be written.
- Example: `--out-dir ./conformance-snapshots`

### Validation Mode

```bash
--validate MODE
```
Set the validation level:
- `full` (default) - Full ledger validation
- `re` - Re-validation
- `none` - No validation

### Logging

```bash
--log-level LEVEL
```
Minimum log level: `debug|info|warn|error` (default: `info`)

```bash
--verbose (-v)
```
Enable verbose output

```bash
--debug (-d)
```
Enable debug output with source locations

## Sanchonet Epoch Calculations

Sanchonet uses **86,400 slots per epoch**:

| Epoch | Start Slot | First Block Slot | End Slot |
|-------|-----------|------------------|----------|
| 0     | 0         | ~0-20           | 86,399   |
| 1     | 86,400    | 86,400          | 172,799  |
| 2     | 172,800   | ~172,827        | 259,199  |
| 3     | 259,200   | ~259,216        | 345,599  |
| 4     | 345,600   | ~345,606        | 431,999  |
| 5     | 432,000   | ~432,063        | 518,399  |
| ...   | ...       | ...             | ...      |
| N     | N×86400   | N×86400+offset  | (N+1)×86400-1 |

**Note**: The first block of an epoch may not be at the exact epoch boundary due to slot leadership schedule.

## Conformance Testing Workflow

### Step 1: Dump Ledger Snapshots for First 10 Epochs

```bash
cd ~/work/iohk/cardano-streamer

# Use nix develop to get the build environment
nix develop -c cabal run cardano-streamer -- replay \
  --chain-dir ~/work/iohk/midnight-playground/.run/sanchonet/cardano-node/db \
  --config ~/work/iohk/midnight-playground/.run/sanchonet/cardano-node/config.json \
  --start-slot 0 \
  --stop 864000 \
  --write-snapshot 86400 \
  --write-snapshot 172800 \
  --write-snapshot 259200 \
  --write-snapshot 345600 \
  --write-snapshot 432000 \
  --write-snapshot 518400 \
  --write-snapshot 604800 \
  --write-snapshot 691200 \
  --write-snapshot 777600 \
  --write-snapshot 864000 \
  --out-dir ./conformance-snapshots \
  --suffix hayate_test
```

This will:
1. Start from genesis (slot 0)
2. Replay through slot 864000 (end of epoch 10)
3. Write snapshots at each epoch boundary (epochs 1-10)
4. Save them to `./conformance-snapshots/`

### Step 2: Compare Specific Values

Snapshots are binary files using cardano-ledger's native serialization. To extract values for comparison:

#### Option A: Use cardano-streamer's built-in tools
```bash
# Example: Calculate rewards at a specific epoch
cardano-streamer rewards \
  --address <bech32-address> \
  --chain-dir ... \
  --config ... \
  --start-slot 86400 \
  --stop 172800
```

#### Option B: Write a Haskell script to deserialize and print values
```haskell
-- Example: Load snapshot and print stake distribution
import Ouroboros.Consensus.Storage.LedgerDB.Snapshots
import Cardano.Ledger.Shelley.LedgerState

main = do
  snapshot <- readDiskSnapshot "path/to/snapshot"
  let stakeDistr = ... -- extract from snapshot
  print stakeDistr
```

#### Option C: Compare binary snapshots directly
If ledger states are identical, the binary snapshots should match byte-for-byte (modulo timestamp/randomness differences).

### Step 3: Hayate Snapshots

Hayate creates slot-labeled snapshots:
```
./data/node/sanchonet/rewards/snapshots/slot-000086400/
./data/node/sanchonet/rewards/snapshots/slot-000172800/
...
```

Extract values from hayate's LSM snapshots:
```rust
// Example: Read stake distribution from hayate snapshot
use cardano_lsm::LsmTree;

let tree = LsmTree::open_snapshot(
    "./data/node/sanchonet/rewards",
    "slot-000086400"
)?;

for (key, value) in tree.iter() {
    // key = credential hash
    // value = reward balance (u64 lovelace)
    println!("{} -> {}", hex::encode(key), u64::from_le_bytes(&value));
}
```

### Step 4: Comparison Checklist

For each epoch boundary, verify:

| Component | Cardano-Streamer | Hayate | Match? |
|-----------|------------------|---------|--------|
| **Treasury** | Extract from snapshot | `ledger_state.treasury.0` | ☐ |
| **Reserves** | Extract from snapshot | `ledger_state.reserves.0` | ☐ |
| **Stake Distribution** | Credential → Lovelace map | `stake_distribution.stake_map` | ☐ |
| **Pool Parameters** | Pool registrations | `ledger_state.pool_params` | ☐ |
| **Reward Accounts** | Reward balances | `ledger_state.reward_accounts` | ☐ |
| **Epoch Nonce** | Snapshot nonce state | `ledger_state.epoch_nonce` | ☐ |
| **Delegations** | Credential → PoolId map | `ledger_state.delegations` | ☐ |

## Common Use Cases

### Test a Single Epoch

```bash
# Replay just epoch 1 and dump its snapshot
cardano-streamer replay \
  --chain-dir <db> \
  --config <config> \
  --start-slot 0 \
  --stop 86400 \
  --write-snapshot 86400
```

### Resume from a Snapshot

```bash
# Start from epoch 5 snapshot and replay to epoch 10
cardano-streamer replay \
  --chain-dir <db> \
  --config <config> \
  --start-slot 432000 \
  --stop 864000 \
  --write-snapshot 864000 \
  --suffix epoch5_to_10
```

### Dump Multiple Snapshots in One Run

```bash
# Efficient: Single replay writing multiple snapshots
cardano-streamer replay \
  --chain-dir <db> \
  --config <config> \
  -w 86400 -w 172800 -w 259200 -w 345600 -w 432000
```

## Snapshot File Locations

By default, snapshots are written to:
```
<CHAIN_DIR>/ledger/<SLOT_NUMBER>
<CHAIN_DIR>/ledger/<SLOT_NUMBER>_<SUFFIX>  # if --suffix provided
```

With `--out-dir`, they go to:
```
<OUT_DIR>/ledger/<SLOT_NUMBER>
<OUT_DIR>/ledger/<SLOT_NUMBER>_<SUFFIX>
```

## Expected Differences

Some ledger state components may differ between cardano-streamer and hayate:

### Acceptable Differences
- **Internal timestamps**: Ledger state may include timestamps from when it was computed
- **Serialization format**: Binary layout differs (CBOR vs bincode/LSM)
- **Floating point**: Minor differences in rational/decimal representations

### Must Match Exactly
- **Treasury amount** (lovelace)
- **Reserves amount** (lovelace)
- **Total stake** (sum of all stake distribution)
- **Epoch nonce** (32-byte hash)
- **Reward account balances** (per credential)
- **Pool parameters** (pledge, margin, cost, etc.)

## Troubleshooting

### "Chain directory does not exist"
Ensure you're pointing to the actual `db/` directory, not the parent directory.

### "Failed to load snapshot"
If resuming from a snapshot:
1. Verify the snapshot exists at `<chain-dir>/ledger/<slot>`
2. Check the `--suffix` matches if you used one when creating it

### "No snapshot at slot X"
Snapshots can only be created at slots that actually have blocks. The tool will fail if you request a snapshot at an empty slot.

### Build failures
If `nix develop` fails:
1. Ensure you're on the correct git branch (10.6.2 or integration)
2. Check `cabal.project` has correct CHaP index-state (2025-02-15T18:39:38Z)
3. Try `nix develop --impure` if needed

## References

- **cardano-streamer repo**: https://github.com/input-output-hk/cardano-streamer (internal)
- **Ledger state spec**: Cardano ledger specification (Shelley, Alonzo, Conway)
- **Consensus docs**: ouroboros-consensus documentation

## Example: Complete Epoch 1 Verification

```bash
# 1. Dump cardano-node's ledger state at epoch 1
cd ~/work/iohk/cardano-streamer
nix develop -c cabal run cardano-streamer -- replay \
  --chain-dir ~/work/iohk/midnight-playground/.run/sanchonet/cardano-node/db \
  --config ~/work/iohk/midnight-playground/.run/sanchonet/cardano-node/config.json \
  --start-slot 0 \
  --stop 86400 \
  --write-snapshot 86400 \
  --out-dir ./epoch1-test

# 2. Hayate will have created: ./data/node/sanchonet/rewards/snapshots/slot-000086400/

# 3. Compare treasury/reserves (extract from cstreamer snapshot with Haskell)
# 4. Compare stake distribution (iterate both and diff)
# 5. Compare nonce values
# 6. Verify total stake matches

# Expected for epoch 1 on sanchonet:
# - 3 initial credentials with 100M ADA each (300M total in early epochs)
# - Treasury: ~0 ADA (early epochs)
# - Reserves: ~45B ADA (max supply minus genesis allocation)
```
