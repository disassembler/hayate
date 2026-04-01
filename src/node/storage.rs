// Hayate-Node Storage
// Single UTxO LSM tree + plain bincode epoch files.
//
// Architecture:
//   data/node/<network>/
//   ├── utxos/
//   │   ├── active/          ← current UTxO SSTables (cardano-lsm managed)
//   │   ├── snapshots/
//   │   │   ├── epoch-0000000001/
//   │   │   └── ...
//   │   └── lock
//   └── epochs/
//       ├── epoch-0000000001.bin   ← bincode EpochSnapshot
//       └── ...
//
// Safety invariant: a `.bin` file is written LAST (after the UTxO snapshot hard-links
// are complete), so presence of `epoch-N.bin` implies a complete UTxO snapshot for N.
//
// UTxO restore uses `LsmTree::open_snapshot` (cardano-lsm ≥ 1.0.2), which hard-links
// snapshot files → active/ internally, leaving the snapshot directory intact.

use cardano_lsm::{LsmTree, LsmConfig, Key, Value};
use std::path::PathBuf;
use serde::{Serialize, Deserialize};
use anyhow::{Context, Result};
use std::collections::HashMap;

use crate::indexer::Network;
use crate::ledger::state::LedgerState;

/// A complete snapshot of ledger state at an epoch boundary.
/// Written as a plain bincode file: `epochs/epoch-{:010}.bin`.
#[derive(Serialize, Deserialize)]
pub struct EpochSnapshot {
    pub epoch: u64,
    pub slot:  u64,
    pub block_hash: [u8; 32],
    pub ledger_state: LedgerState,
}

/// A single UTxO output stored in the LSM tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct UtxoEntry {
    pub address: Vec<u8>,
    pub amount: u64,
    pub assets: HashMap<String, u64>, // policy_id.asset_name -> amount
    pub datum_hash: Option<Vec<u8>>,
    pub datum: Option<Vec<u8>>,       // Inline datum data (if present)
    pub script_ref: Option<Vec<u8>>,
    pub stake_credential: Option<Vec<u8>>,
}

/// Full node storage: UTxO LSM tree + epoch files.
pub struct NodeStorage {
    pub network: Network,

    /// The live UTxO set.
    pub utxo_tree: LsmTree,

    /// In-memory ADA-by-stake-credential, updated on insert/remove.
    /// Allows O(1) stake reads without scanning the full UTxO set.
    current_stake: HashMap<Vec<u8>, u64>,

    /// Which epoch the utxo_tree was last restored to, or None if never restored.
    utxo_epoch: Option<u64>,

    /// `.../node/<network>/` — parent of utxos/ and epochs/.
    base_path: PathBuf,

    /// `.../node/<network>/epochs/`
    epochs_dir: PathBuf,
}

impl NodeStorage {
    /// Open storage for `network` under `base_path`.
    ///
    /// Opens the UTxO tree from its current active/ state (no snapshot restore here).
    /// The caller must immediately call `restore_latest_snapshot()` (or
    /// `restore_epoch_snapshot(N)`) to roll the UTxO tree back to a consistent epoch.
    pub fn open(base_path: PathBuf, network: Network) -> Result<Self> {
        let net_path = base_path.join("node").join(network.as_str());
        let utxo_path = net_path.join("utxos");
        let epochs_dir = net_path.join("epochs");

        tracing::info!("Opening node storage for {} at {:?}", network.as_str(), net_path);

        std::fs::create_dir_all(utxo_path.join("active"))?;
        std::fs::create_dir_all(&epochs_dir)?;

        // Tune LSM for bulk-sync workload:
        //
        // - Large memtable (256 MB): prevents auto-flushes during block processing.
        //   Each epoch generates ~50-100 MB of UTxO changes; keeping them in memory
        //   avoids triggering L0 compaction mid-epoch.
        //
        // Compaction runs on a background thread (credit-based), so the default
        // level0_compaction_trigger (4) is used only as the emergency back-pressure
        // threshold and never causes write-path stalls.
        let lsm_config = LsmConfig {
            memtable_size: 256 * 1024 * 1024,
            ..LsmConfig::default()
        };
        let utxo_tree = LsmTree::open(utxo_path, lsm_config)
            .context("open utxo_tree")?;

        Ok(Self {
            network,
            utxo_tree,
            current_stake: HashMap::new(),
            utxo_epoch: None,
            base_path: net_path,
            epochs_dir,
        })
    }

    // ── UTxO operations ────────────────────────────────────────────────────────

    /// Encode a UTxO key as a compact 36-byte binary key: 32-byte tx hash || 4-byte big-endian index.
    /// This is ~45% smaller than the previous hex-string format ("txhash:N"), reducing index/filter
    /// file sizes and eliminating per-operation heap allocations for key formatting.
    fn utxo_key(tx_hash: &[u8], output_index: u32) -> [u8; 36] {
        let mut key = [0u8; 36];
        let len = tx_hash.len().min(32);
        key[..len].copy_from_slice(&tx_hash[..len]);
        key[32..36].copy_from_slice(&output_index.to_be_bytes());
        key
    }

    pub fn insert_utxo(&mut self, tx_hash: &[u8], output_index: u32, utxo: &UtxoEntry) -> Result<()> {
        let key = Self::utxo_key(tx_hash, output_index);
        let value = bincode::serialize(utxo)?;
        self.utxo_tree.insert(&Key::from(key.as_ref()), &Value::from(&value))?;

        if let Some(stake_cred) = &utxo.stake_credential {
            *self.current_stake.entry(stake_cred.clone()).or_insert(0) += utxo.amount;
        }
        Ok(())
    }

    pub fn remove_utxo(&mut self, tx_hash: &[u8], output_index: u32) -> Result<Option<UtxoEntry>> {
        let key = Self::utxo_key(tx_hash, output_index);
        let key_ref = Key::from(key.as_ref());

        let utxo: Option<UtxoEntry> = if let Some(value) = self.utxo_tree.get(&key_ref)? {
            if value.as_ref().is_empty() {
                // Tombstone — UTxO was already consumed
                None
            } else {
                Some(bincode::deserialize(value.as_ref())?)
            }
        } else {
            None
        };

        if let Some(ref entry) = utxo {
            if let Some(stake_cred) = &entry.stake_credential {
                if let Some(current) = self.current_stake.get_mut(stake_cred) {
                    *current = current.saturating_sub(entry.amount);
                    if *current == 0 {
                        self.current_stake.remove(stake_cred);
                    }
                }
            }
        }

        // Tombstone
        self.utxo_tree.insert(&key_ref, &Value::from(b"".as_ref()))?;
        Ok(utxo)
    }

    /// Remove a UTxO without reading its current value first.
    /// Only safe when the caller knows there is no stake credential to untrack
    /// (e.g. Byron-era UTxOs which never have stake credentials).
    pub fn remove_utxo_blind(&mut self, tx_hash: &[u8], output_index: u32) -> Result<()> {
        let key = Self::utxo_key(tx_hash, output_index);
        self.utxo_tree.insert(&Key::from(key.as_ref()), &Value::from(b"".as_ref()))?;
        Ok(())
    }

    /// Returns the incrementally-maintained UTxO stake map (credential bytes → lovelace).
    /// Updated on every insert_utxo / remove_utxo; use this instead of a full UTxO tree scan
    /// at epoch boundaries.
    pub fn current_stake(&self) -> &HashMap<Vec<u8>, u64> {
        &self.current_stake
    }

    pub fn get_utxo(&self, tx_hash: &[u8], output_index: u32) -> Result<Option<UtxoEntry>> {
        let key = Self::utxo_key(tx_hash, output_index);
        if let Some(value) = self.utxo_tree.get(&Key::from(key.as_ref()))? {
            if value.as_ref().is_empty() {
                // Tombstone — UTxO was already consumed
                Ok(None)
            } else {
                Ok(Some(bincode::deserialize(value.as_ref())?))
            }
        } else {
            Ok(None)
        }
    }

    // ── Epoch snapshot operations ──────────────────────────────────────────────

    /// Save epoch snapshot: UTxO hard-links first, then bincode file (atomic).
    ///
    /// Writing the `.bin` file last ensures that its presence always implies a
    /// complete UTxO snapshot (crash-safe: a partial write leaves only a `.bin.tmp`
    /// which is ignored by `find_latest_consistent_epoch`).
    pub fn save_epoch_snapshot(
        &mut self,
        epoch: u64,
        slot: u64,
        block_hash: [u8; 32],
        state: &LedgerState,
    ) -> Result<()> {
        let snap_name = format!("epoch-{:010}", epoch);

        // If a UTxO snapshot for this epoch already exists (re-sync after rollback),
        // delete it before creating a fresh one.
        let snap_dir = self.base_path.join("utxos").join("snapshots").join(&snap_name);
        if snap_dir.exists() {
            if let Err(e) = self.utxo_tree.delete_snapshot(&snap_name) {
                tracing::warn!("Could not delete old UTxO snapshot {}: {}", snap_name, e);
            }
        }

        // Hard-link active/ → snapshots/epoch-N/  (safe: snapshot dir is never read by lsm)
        let t_lsm = std::time::Instant::now();
        self.utxo_tree.save_snapshot(&snap_name, &format!("UTxO epoch {} slot {}", epoch, slot))?;
        let lsm_ms = t_lsm.elapsed().as_millis() as u64;

        // Write bincode epoch file atomically: tmp → rename
        let t_bin = std::time::Instant::now();
        let record = EpochSnapshot { epoch, slot, block_hash, ledger_state: state.clone() };
        let bytes = bincode::serialize(&record)?;
        let tmp  = self.epochs_dir.join(format!("epoch-{:010}.bin.tmp", epoch));
        let dest = self.epochs_dir.join(format!("epoch-{:010}.bin", epoch));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &dest)?;
        let bin_ms = t_bin.elapsed().as_millis() as u64;

        tracing::info!(epoch, lsm_ms, bin_ms, "epoch snapshot saved");
        Ok(())
    }

    /// Restore UTxO tree to the state at `epoch`.
    ///
    /// `open_snapshot` (cardano-lsm ≥ 1.0.2) hard-links snapshot files → active/
    /// internally, leaving the snapshot directory intact.
    fn restore_utxo_to_epoch(&mut self, epoch: u64) -> Result<()> {
        if self.utxo_epoch == Some(epoch) {
            return Ok(());
        }

        let snap_name = format!("epoch-{:010}", epoch);
        let utxo_path = self.base_path.join("utxos");

        // Release the session lock on utxos/ by swapping in a placeholder tree,
        // then drop the old tree so open_snapshot can acquire the lock.
        let ph_path = self.base_path.join("utxos_restore_placeholder");
        std::fs::create_dir_all(&ph_path)?;
        let placeholder = LsmTree::open(ph_path.clone(), LsmConfig::default())
            .context("open placeholder for utxo restore")?;
        drop(std::mem::replace(&mut self.utxo_tree, placeholder));

        self.utxo_tree = LsmTree::open_snapshot(&utxo_path, &snap_name)
            .with_context(|| format!("open_snapshot epoch-{:010}", epoch))?;
        let _ = std::fs::remove_dir_all(&ph_path);

        self.current_stake.clear(); // rebuilt by caller via rebuild_from_utxo_tree
        self.utxo_epoch = Some(epoch);
        tracing::info!("🔄 UTxO tree restored to epoch {}", epoch);
        Ok(())
    }

    /// Restore from a specific epoch snapshot.
    ///
    /// Used for `--restore-from-epoch N` (targeted rollback after divergence).
    pub fn restore_epoch_snapshot(&mut self, epoch: u64)
        -> Result<(LedgerState, u64, [u8; 32])>
    {
        let path = self.epochs_dir.join(format!("epoch-{:010}.bin", epoch));
        anyhow::ensure!(path.exists(), "No epoch snapshot for epoch {}", epoch);
        let record: EpochSnapshot = bincode::deserialize(&std::fs::read(&path)?)?;
        self.restore_utxo_to_epoch(epoch)?;
        Ok((record.ledger_state, record.slot, record.block_hash))
    }

    /// Restore from the latest available consistent epoch snapshot.
    ///
    /// A snapshot is "consistent" when both `epoch-N.bin` and the matching UTxO
    /// snapshot metadata exist. Returns `None` on the first-ever run.
    pub fn restore_latest_snapshot(&mut self)
        -> Result<Option<(LedgerState, u64, [u8; 32])>>
    {
        match self.find_latest_consistent_epoch()? {
            None        => Ok(None),
            Some(epoch) => Ok(Some(self.restore_epoch_snapshot(epoch)?)),
        }
    }

    /// Scan `epochs/` for the highest epoch-N.bin that also has a UTxO snapshot.
    fn find_latest_consistent_epoch(&self) -> Result<Option<u64>> {
        let mut best: Option<u64> = None;

        let read_dir = match std::fs::read_dir(&self.epochs_dir) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        for entry in read_dir {
            let name = entry?.file_name();
            let s = name.to_string_lossy();
            if s.ends_with(".bin") && s.starts_with("epoch-") {
                if let Ok(ep) = s
                    .trim_start_matches("epoch-")
                    .trim_end_matches(".bin")
                    .parse::<u64>()
                {
                    let snap_meta = self.base_path
                        .join("utxos")
                        .join("snapshots")
                        .join(format!("epoch-{:010}", ep))
                        .join("metadata");
                    if snap_meta.exists() {
                        best = Some(best.map_or(ep, |b: u64| b.max(ep)));
                    }
                }
            }
        }
        Ok(best)
    }
}

// ── Helper functions for epoch/slot arithmetic ─────────────────────────────────

#[allow(dead_code)]
pub fn slot_to_epoch(slot: u64, network: &Network) -> u64 {
    let epoch_length = match network {
        Network::Mainnet | Network::Preprod => 432_000,
        Network::Preview => 86_400,
        Network::SanchoNet => 86_400,
        Network::Custom(_) => 432_000,
    };
    slot / epoch_length
}

#[allow(dead_code)]
pub fn is_epoch_boundary(slot: u64, network: &Network) -> bool {
    let epoch_length = match network {
        Network::Mainnet | Network::Preprod => 432_000,
        Network::Preview => 86_400,
        Network::SanchoNet => 86_400,
        Network::Custom(_) => 432_000,
    };
    (slot + 1).is_multiple_of(epoch_length)
}

#[allow(dead_code)]
pub fn epoch_to_slot(epoch: u64, network: &Network) -> u64 {
    let epoch_length = match network {
        Network::Mainnet | Network::Preprod => 432_000,
        Network::Preview => 86_400,
        Network::SanchoNet => 86_400,
        Network::Custom(_) => 432_000,
    };
    epoch * epoch_length
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use crate::ledger::primitives::{EpochNo, Lovelace, ProtocolParameters};
    use crate::ledger::state::LedgerState;

    fn make_storage(dir: &TempDir) -> NodeStorage {
        NodeStorage::open(dir.path().to_path_buf(), Network::SanchoNet).unwrap()
    }

    fn make_state(epoch: u64, treasury: u64, reserves: u64) -> LedgerState {
        let mut s = LedgerState::new(ProtocolParameters::default());
        s.epoch = EpochNo(epoch);
        s.treasury = Lovelace(treasury);
        s.reserves = Lovelace(reserves);
        s
    }

    /// Test 1: basic round-trip — save then restore_latest_snapshot.
    #[test]
    fn test_basic_round_trip() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        let block_hash = [0xFFu8; 32];

        // Write
        {
            let mut storage = make_storage(&dir);
            let state = make_state(5, 100_000_000, 900_000_000);
            storage.save_epoch_snapshot(5, 12_345, block_hash, &state)?;
        }

        // Restore
        {
            let mut storage = make_storage(&dir);
            let (state, slot, hash) = storage
                .restore_latest_snapshot()?
                .expect("snapshot should exist");
            assert_eq!(state.epoch.0, 5);
            assert_eq!(state.treasury.0, 100_000_000);
            assert_eq!(state.reserves.0, 900_000_000);
            assert_eq!(slot, 12_345);
            assert_eq!(hash, block_hash);
        }
        Ok(())
    }

    /// Test 2: multi-epoch — restore picks the latest.
    #[test]
    fn test_multi_epoch_latest() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        {
            let mut storage = make_storage(&dir);
            for ep in 1u64..=10 {
                storage.save_epoch_snapshot(ep, ep * 1000, [ep as u8; 32], &make_state(ep, ep, ep))?;
            }
        }
        {
            let mut storage = make_storage(&dir);
            let (state, slot, _) = storage.restore_latest_snapshot()?.expect("snapshot");
            assert_eq!(state.epoch.0, 10);
            assert_eq!(slot, 10_000);
        }
        Ok(())
    }

    /// Test 2b: restore_epoch_snapshot(5) returns epoch 5 state even when later epochs exist.
    #[test]
    fn test_restore_specific_epoch() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        {
            let mut storage = make_storage(&dir);
            for ep in 1u64..=10 {
                storage.save_epoch_snapshot(ep, ep * 1000, [ep as u8; 32], &make_state(ep, ep * 1000, 0))?;
            }
        }
        {
            let mut storage = make_storage(&dir);
            let (state, slot, _) = storage.restore_epoch_snapshot(5)?;
            assert_eq!(state.epoch.0, 5);
            assert_eq!(state.treasury.0, 5_000);
            assert_eq!(slot, 5_000);
            assert_eq!(storage.utxo_epoch, Some(5));
        }
        Ok(())
    }

    /// Test 3: snapshot files survive multiple restores (regression for open_snapshot destruction).
    #[test]
    fn test_snapshot_survives_multiple_restores() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        {
            let mut storage = make_storage(&dir);
            storage.save_epoch_snapshot(5, 1000, [5u8; 32], &make_state(5, 42, 99))?;
        }

        let snap_dir = dir.path()
            .join("node/sanchonet/utxos/snapshots/epoch-0000000005");

        // The snapshot may be empty (no SSTables) on a fresh tree, but metadata must exist
        let meta = snap_dir.join("metadata");
        assert!(meta.exists(), "snapshot metadata should exist: {:?}", meta);

        // Restore three times and verify metadata still present after each.
        for i in 0..3 {
            let mut storage = make_storage(&dir);
            let _ = storage.restore_latest_snapshot()?;
            assert!(meta.exists(), "metadata missing after restore #{}", i + 1);
        }
        Ok(())
    }

    /// Test 4: UTxO data survives a save→restore cycle.
    #[test]
    fn test_utxo_survives_round_trip() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        let tx_hash = [0x01u8; 32];
        let cred = [0xAAu8; 28];

        {
            let mut storage = make_storage(&dir);
            let utxo = UtxoEntry {
                address: {
                    let mut a = vec![0xE0u8];
                    a.extend_from_slice(&cred);
                    a
                },
                amount: 5_000_000,
                assets: HashMap::new(),
                datum_hash: None,
                datum: None,
                script_ref: None,
                stake_credential: Some(cred.to_vec()),
            };
            storage.insert_utxo(&tx_hash, 0, &utxo)?;
            storage.save_epoch_snapshot(1, 100, [1u8; 32], &make_state(1, 0, 0))?;
        }

        {
            let mut storage = make_storage(&dir);
            let _ = storage.restore_latest_snapshot()?;
            let entry = storage.get_utxo(&tx_hash, 0)?.expect("UTxO should survive");
            assert_eq!(entry.amount, 5_000_000);
            assert_eq!(entry.stake_credential, Some(cred.to_vec()));
        }
        Ok(())
    }

    /// Test 5: re-sync overwrites an existing snapshot for the same epoch.
    #[test]
    fn test_resync_overwrites_snapshot() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        // Save epoch 5 with data A
        {
            let mut storage = make_storage(&dir);
            storage.save_epoch_snapshot(5, 1000, [5u8; 32], &make_state(5, 111, 222))?;
        }
        // Save epoch 5 again with data B (simulates rollback to epoch 3 then re-sync)
        {
            let mut storage = make_storage(&dir);
            storage.save_epoch_snapshot(5, 2000, [6u8; 32], &make_state(5, 333, 444))?;
        }
        // Restore should return data B
        {
            let mut storage = make_storage(&dir);
            let (state, slot, _) = storage.restore_epoch_snapshot(5)?;
            assert_eq!(state.treasury.0, 333, "should have data B");
            assert_eq!(state.reserves.0, 444, "should have data B");
            assert_eq!(slot, 2000);
        }
        Ok(())
    }

    /// Test 6: find_latest_consistent_epoch skips orphaned .bin files (no UTxO snapshot).
    #[test]
    fn test_skips_orphaned_bin_files() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        // Save epochs 1..4 properly
        {
            let mut storage = make_storage(&dir);
            for ep in 1u64..=4 {
                storage.save_epoch_snapshot(ep, ep * 100, [ep as u8; 32], &make_state(ep, ep, ep))?;
            }
        }
        // Write epoch-5.bin manually WITHOUT a matching UTxO snapshot
        let epochs_dir = dir.path().join("node/sanchonet/epochs");
        let orphan_bin = epochs_dir.join("epoch-0000000005.bin");
        let fake_record = EpochSnapshot {
            epoch: 5, slot: 500, block_hash: [5u8; 32],
            ledger_state: make_state(5, 5, 5),
        };
        std::fs::write(&orphan_bin, bincode::serialize(&fake_record)?)?;

        {
            let storage = make_storage(&dir);
            let best = storage.find_latest_consistent_epoch()?;
            assert_eq!(best, Some(4), "orphaned epoch 5 .bin should be skipped");
        }
        Ok(())
    }

    /// Test 7: crash safety — a leftover .bin.tmp is ignored.
    #[test]
    fn test_crash_safety_tmp_ignored() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        // Save epoch 4 cleanly
        {
            let mut storage = make_storage(&dir);
            storage.save_epoch_snapshot(4, 400, [4u8; 32], &make_state(4, 4, 4))?;
        }
        // Simulate a crash after writing epoch-5.bin.tmp (before rename)
        let epochs_dir = dir.path().join("node/sanchonet/epochs");
        std::fs::write(
            epochs_dir.join("epoch-0000000005.bin.tmp"),
            b"garbage",
        )?;

        {
            let mut storage = make_storage(&dir);
            let result = storage.restore_latest_snapshot()?;
            let (state, _, _) = result.expect("epoch 4 should restore");
            assert_eq!(state.epoch.0, 4, "should restore epoch 4, not see crashed epoch 5");
        }
        Ok(())
    }

    /// Test 8: empty fresh start — restore_latest_snapshot returns None.
    #[test]
    fn test_empty_fresh_start() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        let mut storage = make_storage(&dir);
        let result = storage.restore_latest_snapshot()?;
        assert!(result.is_none(), "fresh storage should have no snapshot");
        Ok(())
    }
}
