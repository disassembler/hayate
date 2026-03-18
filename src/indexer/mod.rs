// Core Hayate indexer implementation

#![allow(dead_code)]

pub mod block_processor;
pub mod storage_manager;
pub mod block_walker;

use cardano_lsm::{LsmTree, LsmConfig, MonoidalLsmTree, IncrementalMerkleTree, Key, Value, PersistentSnapshot};
use std::path::PathBuf;
use std::collections::HashMap;
use tokio::sync::{RwLock, broadcast};
use anyhow::Result;

#[allow(unused_imports)]
pub use block_processor::{BlockProcessor, WalletFilter, BlockStats};
pub use storage_manager::{StorageHandle, StorageManager};

// Import RewardsTracker from the rewards module
use crate::rewards::RewardsTracker;

/// Type alias for block metadata: (slot, timestamp, prev_hash)
type BlockMetadata = (u64, u64, Option<Vec<u8>>);

/// Block update broadcast to subscribers
#[derive(Debug, Clone)]
pub struct BlockUpdate {
    pub network: Network,
    pub height: u64,
    pub slot: u64,
    pub hash: Vec<u8>,
    pub tx_hashes: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Network {
    Mainnet,
    Preprod,
    Preview,
    SanchoNet,
    Custom(String),
}

impl Network {
    pub fn magic(&self) -> u64 {
        match self {
            Network::Mainnet => 764824073,
            Network::Preprod => 1,
            Network::Preview => 2,
            Network::SanchoNet => 4,
            Network::Custom(_) => 0,
        }
    }

    /// Get the system start time (genesis time) in Unix milliseconds
    pub fn system_start_ms(&self) -> u64 {
        match self {
            Network::Mainnet => 1591566291000,  // July 29, 2020 21:44:51 UTC
            Network::Preprod => 1654041600000,  // June 1, 2022 00:00:00 UTC
            Network::Preview => 1666656000000,  // October 25, 2022 00:00:00 UTC
            Network::SanchoNet => 1686790200000, // June 15, 2023 00:30:00 UTC
            Network::Custom(_) => 1591566291000, // Default to mainnet genesis
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Network::Mainnet => "mainnet",
            Network::Preprod => "preprod",
            Network::Preview => "preview",
            Network::SanchoNet => "sanchonet",
            Network::Custom(name) => name,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "mainnet" => Some(Network::Mainnet),
            "preprod" => Some(Network::Preprod),
            "preview" => Some(Network::Preview),
            "sanchonet" => Some(Network::SanchoNet),
            other => Some(Network::Custom(other.to_string())),
        }
    }
}

/// Storage for a single network
pub struct NetworkStorage {
    pub network: Network,

    // Base path for this network's storage
    network_path: PathBuf,

    pub utxo_tree: LsmTree,
    pub balance_tree: MonoidalLsmTree<u64>,
    pub governance_tree: LsmTree,
    pub governance_merkle: IncrementalMerkleTree,

    // Staking and rewards (cardano-wallet pattern)
    pub rewards_tracker: RewardsTracker,

    // Epoch nonce for leader schedule
    pub nonce_tree: LsmTree,

    // Chain tip tracking
    pub chain_tip_tree: LsmTree,

    // Address -> UTxO keys index for efficient queries
    // Key: address_hex, Value: JSON array of UTxO keys
    pub address_utxo_index: LsmTree,

    // Transaction history: address -> tx hashes
    // Key: address_hex, Value: JSON array of tx hashes
    pub address_tx_index: LsmTree,

    // Token transaction indexes
    // Policy -> tx hashes: policy_id_hex -> JSON array of tx hashes
    pub policy_tx_index: LsmTree,

    // Asset -> tx hashes: "policy_id_hex:asset_name_hex" -> JSON array of tx hashes
    pub asset_tx_index: LsmTree,

    // Spent UTxO tracking: utxo_key -> spend event JSON
    // Key: "tx_hash#index", Value: { spent_at_slot, spent_at_block_hash, spent_at_tx_index, spent_at_tx_hash, spent_at_block_timestamp, spent_by_address }
    pub spent_utxo_index: LsmTree,

    // Block events for range queries (CREATE and SPEND events)
    // Key: "slot#{slot:020}#{event_index:010}", Value: JSON event data
    // This enables efficient slot range queries for ReadUtxoEvents RPC
    pub block_events_tree: LsmTree,

    // Block hash index for block-by-hash queries
    // Key: block_hash (32 bytes), Value: JSON { slot, timestamp }
    pub block_hash_index: LsmTree,

    // Track which epoch we started indexing from
    pub indexing_start_epoch: u64,

    // SPDD (Stake Pool Distribution Data) snapshot trees
    // These store epoch boundary snapshots for rewards and stake distribution

    // Stake distribution: "stake:{epoch}:{cred_hash_hex}" -> u64 (lovelace)
    pub stake_distribution_tree: LsmTree,

    // Pool parameters: "pool:{epoch}:{pool_id_hex}" -> JSON (PoolRegistration)
    pub pool_params_tree: LsmTree,

    // Pool stake totals: "pool_stake:{epoch}:{pool_id_hex}" -> u64 (lovelace)
    pub pool_stake_tree: LsmTree,

    // Delegations: "delegation:{epoch}:{cred_hash_hex}" -> pool_id (28 bytes)
    pub delegations_tree: LsmTree,
}

/// Open an LSM tree, restoring from latest valid snapshot if available
///
/// This function implements graceful degradation:
/// 1. Lists all snapshots in reverse chronological order
/// 2. Validates each snapshot (checks file existence and checksums)
/// 3. Deletes invalid/corrupted snapshots
/// 4. Falls back to previous snapshots if the latest is corrupted
/// 5. Falls back to empty tree if no valid snapshots exist
fn open_lsm_tree_with_snapshot(tree_path: std::path::PathBuf) -> Result<LsmTree> {
    let tree_name = tree_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    // List all available snapshots
    let temp_tree = LsmTree::open(&tree_path, LsmConfig::default())?;
    let mut snapshots = temp_tree.list_snapshots()?;
    drop(temp_tree); // Release lock before attempting to open snapshots

    if snapshots.is_empty() {
        tracing::info!("No snapshots found for {}, starting from empty state", tree_name);
        return Ok(LsmTree::open(tree_path, LsmConfig::default())?);
    }

    // Sort in reverse chronological order (newest first)
    // Snapshots are named "slot-{:020}" so reverse lexicographic sort works
    snapshots.sort();
    snapshots.reverse();

    tracing::info!("Found {} snapshot(s) for {}, attempting to restore from latest valid snapshot",
                   snapshots.len(), tree_name);

    let mut attempted_snapshots = Vec::new();
    let mut deleted_snapshots = Vec::new();

    // Try each snapshot in reverse chronological order
    for snapshot_name in snapshots {
        attempted_snapshots.push(snapshot_name.clone());

        tracing::debug!("Attempting to load snapshot {} for {}", snapshot_name, tree_name);

        // First, validate the snapshot before attempting to open it
        match PersistentSnapshot::load(&tree_path, &snapshot_name) {
            Ok(snapshot) => {
                match snapshot.validate() {
                    Ok(()) => {
                        // Snapshot is valid, try to open it
                        tracing::info!("Snapshot {} validated successfully, opening {} from this snapshot",
                                      snapshot_name, tree_name);

                        match LsmTree::open_snapshot(&tree_path, &snapshot_name) {
                            Ok(tree) => {
                                if !deleted_snapshots.is_empty() {
                                    tracing::warn!("Deleted {} corrupted snapshot(s) for {}: {}",
                                                  deleted_snapshots.len(),
                                                  tree_name,
                                                  deleted_snapshots.join(", "));
                                }
                                tracing::info!("Successfully restored {} from snapshot {}", tree_name, snapshot_name);
                                return Ok(tree);
                            }
                            Err(e) => {
                                tracing::error!("Failed to open validated snapshot {} for {}: {}",
                                              snapshot_name, tree_name, e);
                                tracing::warn!("Deleting snapshot {} and trying previous snapshot", snapshot_name);

                                // Delete the corrupted snapshot
                                if let Err(delete_err) = snapshot.delete() {
                                    tracing::error!("Failed to delete corrupted snapshot {}: {}",
                                                  snapshot_name, delete_err);
                                } else {
                                    deleted_snapshots.push(snapshot_name.clone());
                                }
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Snapshot {} validation failed for {}: {}", snapshot_name, tree_name, e);
                        tracing::info!("Deleting corrupted snapshot {} and trying previous snapshot", snapshot_name);

                        // Delete the corrupted snapshot
                        if let Err(delete_err) = snapshot.delete() {
                            tracing::error!("Failed to delete corrupted snapshot {}: {}", snapshot_name, delete_err);
                        } else {
                            deleted_snapshots.push(snapshot_name.clone());
                        }
                        continue;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to load snapshot metadata for {} ({}): {}",
                              snapshot_name, tree_name, e);
                tracing::info!("Attempting to delete corrupted snapshot directory {}", snapshot_name);

                // Try to delete the corrupted snapshot directory directly
                let snapshot_path = tree_path.join("snapshots").join(&snapshot_name);
                if let Err(delete_err) = std::fs::remove_dir_all(&snapshot_path) {
                    tracing::error!("Failed to delete corrupted snapshot directory {}: {}",
                                  snapshot_name, delete_err);
                } else {
                    deleted_snapshots.push(snapshot_name.clone());
                }
                continue;
            }
        }
    }

    // No valid snapshots found
    if !deleted_snapshots.is_empty() {
        tracing::error!(
            "All {} snapshot(s) for {} were corrupted and have been deleted: {}",
            deleted_snapshots.len(),
            tree_name,
            deleted_snapshots.join(", ")
        );
    }

    tracing::warn!(
        "No valid snapshots found for {} after checking {} snapshot(s). Starting from empty state. \
         This will require a full resync from the network.",
        tree_name,
        attempted_snapshots.len()
    );

    Ok(LsmTree::open(tree_path, LsmConfig::default())?)
}

impl NetworkStorage {
    pub fn open(base_path: PathBuf, network: Network) -> Result<Self> {
        let network_path = base_path.join(network.as_str());

        tracing::info!("Opening storage for {} at {:?}", network.as_str(), network_path);

        let utxo_tree = open_lsm_tree_with_snapshot(network_path.join("utxos"))?;
        let balance_tree = MonoidalLsmTree::open(network_path.join("balances"), LsmConfig::default())?;
        let governance_tree = open_lsm_tree_with_snapshot(network_path.join("governance"))?;
        let governance_merkle = IncrementalMerkleTree::new(32);
        let nonce_tree = open_lsm_tree_with_snapshot(network_path.join("nonces"))?;
        let chain_tip_tree = open_lsm_tree_with_snapshot(network_path.join("chain_tip"))?;
        let address_utxo_index = open_lsm_tree_with_snapshot(network_path.join("address_utxos"))?;
        let address_tx_index = open_lsm_tree_with_snapshot(network_path.join("address_txs"))?;
        let policy_tx_index = open_lsm_tree_with_snapshot(network_path.join("policy_txs"))?;
        let asset_tx_index = open_lsm_tree_with_snapshot(network_path.join("asset_txs"))?;
        let spent_utxo_index = open_lsm_tree_with_snapshot(network_path.join("spent_utxos"))?;
        let block_events_tree = open_lsm_tree_with_snapshot(network_path.join("block_events"))?;
        let block_hash_index = open_lsm_tree_with_snapshot(network_path.join("block_hash_index"))?;

        let start_epoch = 0;
        let rewards_tracker = RewardsTracker::open(network_path.join("rewards"), start_epoch)?;

        // SPDD snapshot trees
        let stake_distribution_tree = open_lsm_tree_with_snapshot(network_path.join("stake_distribution"))?;
        let pool_params_tree = open_lsm_tree_with_snapshot(network_path.join("pool_params"))?;
        let pool_stake_tree = open_lsm_tree_with_snapshot(network_path.join("pool_stake"))?;
        let delegations_tree = open_lsm_tree_with_snapshot(network_path.join("delegations"))?;

        Ok(Self {
            network,
            network_path: network_path.clone(),
            utxo_tree,
            balance_tree,
            governance_tree,
            governance_merkle,
            rewards_tracker,
            nonce_tree,
            chain_tip_tree,
            address_utxo_index,
            address_tx_index,
            policy_tx_index,
            asset_tx_index,
            spent_utxo_index,
            block_events_tree,
            block_hash_index,
            indexing_start_epoch: start_epoch,
            stake_distribution_tree,
            pool_params_tree,
            pool_stake_tree,
            delegations_tree,
        })
    }
    
    pub fn store_nonce(&mut self, epoch: u64, nonce: &[u8]) -> Result<()> {
        let key = format!("nonce:{}", epoch);
        self.nonce_tree.insert(
            &Key::from(key.as_bytes()),
            &Value::from(nonce)
        )?;
        
        tracing::debug!("Stored nonce for epoch {}", epoch);
        Ok(())
    }
    
    pub fn get_nonce(&self, epoch: u64) -> Result<Option<Vec<u8>>> {
        let key = format!("nonce:{}", epoch);

        if let Some(value) = self.nonce_tree.get(&Key::from(key.as_bytes()))? {
            Ok(Some(value.as_ref().to_vec()))
        } else {
            Ok(None)
        }
    }

    /// Store chain tip
    pub fn store_chain_tip(&mut self, slot: u64, hash: &[u8], timestamp: u64) -> Result<()> {
        let tip_data = serde_json::json!({
            "slot": slot,
            "hash": hex::encode(hash),
            "timestamp": timestamp,
        });

        self.chain_tip_tree.insert(
            &Key::from(b"current_tip"),
            &Value::from(&serde_json::to_vec(&tip_data)?),
        )?;

        Ok(())
    }

    /// Get current chain tip
    pub fn get_chain_tip(&self) -> Result<Option<ChainTip>> {
        if let Some(value) = self.chain_tip_tree.get(&Key::from(b"current_tip"))? {
            let tip_data: serde_json::Value = serde_json::from_slice(value.as_ref())?;

            Ok(Some(ChainTip {
                height: 0, // TODO: track height
                slot: tip_data["slot"].as_u64().unwrap_or(0),
                hash: hex::decode(tip_data["hash"].as_str().unwrap_or("")).unwrap_or_default(),
                timestamp: tip_data["timestamp"].as_u64().unwrap_or(0),
            }))
        } else {
            Ok(None)
        }
    }

    /// Store wallet-specific chain tip
    /// Uses wallet identifier (e.g., xpub hash or stake key) as key
    pub fn store_wallet_tip(&mut self, wallet_id: &str, slot: u64, hash: &[u8], timestamp: u64) -> Result<()> {
        let tip_data = serde_json::json!({
            "slot": slot,
            "hash": hex::encode(hash),
            "timestamp": timestamp,
        });

        let key = format!("wallet_tip:{}", wallet_id);
        self.chain_tip_tree.insert(
            &Key::from(key.as_bytes()),
            &Value::from(&serde_json::to_vec(&tip_data)?),
        )?;

        Ok(())
    }

    /// Get wallet-specific chain tip
    pub fn get_wallet_tip(&self, wallet_id: &str) -> Result<Option<ChainTip>> {
        let key = format!("wallet_tip:{}", wallet_id);

        if let Some(value) = self.chain_tip_tree.get(&Key::from(key.as_bytes()))? {
            let tip_data: serde_json::Value = serde_json::from_slice(value.as_ref())?;

            Ok(Some(ChainTip {
                height: 0,
                slot: tip_data["slot"].as_u64().unwrap_or(0),
                hash: hex::decode(tip_data["hash"].as_str().unwrap_or("")).unwrap_or_default(),
                timestamp: tip_data["timestamp"].as_u64().unwrap_or(0),
            }))
        } else {
            Ok(None)
        }
    }

    /// Get the minimum wallet tip across all wallets
    /// This determines where to resume chain sync from
    pub fn get_min_wallet_tip(&self, wallet_ids: &[String]) -> Result<Option<ChainTip>> {
        if wallet_ids.is_empty() {
            // No wallets configured, use global chain tip
            return self.get_chain_tip();
        }

        let mut min_tip: Option<ChainTip> = None;

        for wallet_id in wallet_ids {
            match self.get_wallet_tip(wallet_id)? {
                Some(tip) => {
                    match &min_tip {
                        Some(current_min) => {
                            if tip.slot < current_min.slot {
                                min_tip = Some(tip);
                            }
                        }
                        None => min_tip = Some(tip),
                    }
                }
                None => {
                    // If any wallet has no tip, start from origin
                    return Ok(None);
                }
            }
        }

        Ok(min_tip)
    }

    /// Store block metadata in the block hash index
    pub fn store_block_metadata(&mut self, block_hash: &[u8], slot: u64, timestamp: u64, prev_hash: Option<Vec<u8>>) -> Result<()> {
        let metadata = serde_json::json!({
            "slot": slot,
            "timestamp": timestamp,
            "prev_hash": prev_hash.map(hex::encode),
        });

        self.block_hash_index.insert(
            &Key::from(block_hash),
            &Value::from(&serde_json::to_vec(&metadata)?),
        )?;

        Ok(())
    }

    /// Get block metadata by hash - returns (slot, timestamp, prev_hash)
    pub fn get_block_metadata(&self, block_hash: &[u8]) -> Result<Option<BlockMetadata>> {
        if let Some(value) = self.block_hash_index.get(&Key::from(block_hash))? {
            let metadata: serde_json::Value = serde_json::from_slice(value.as_ref())?;
            let slot = metadata["slot"].as_u64().unwrap_or(0);
            let timestamp = metadata["timestamp"].as_u64().unwrap_or(0);
            let prev_hash = metadata["prev_hash"]
                .as_str()
                .and_then(|s| hex::decode(s).ok());
            Ok(Some((slot, timestamp, prev_hash)))
        } else {
            Ok(None)
        }
    }

    /// Delete block metadata by hash
    pub fn delete_block_metadata(&mut self, block_hash: &[u8]) -> Result<()> {
        self.block_hash_index.delete(&Key::from(block_hash))?;
        Ok(())
    }

    /// Add a UTxO to the address index
    /// Uses individual keys for O(1) operations instead of JSON arrays
    pub fn add_utxo_to_address_index(&mut self, address_hex: &str, utxo_key: &str) -> Result<()> {
        // Key format: address:utxo_key
        let index_key = format!("{}:{}", address_hex, utxo_key);
        let key = Key::from(index_key.as_bytes());

        // Store a marker (just 1 byte) - the key itself contains the information
        self.address_utxo_index.insert(&key, &Value::from([1u8]))?;

        Ok(())
    }

    /// Remove a UTxO from the address index
    /// Uses individual keys for O(1) operations
    pub fn remove_utxo_from_address_index(&mut self, address_hex: &str, utxo_key: &str) -> Result<()> {
        // Key format: address:utxo_key
        let index_key = format!("{}:{}", address_hex, utxo_key);
        let key = Key::from(index_key.as_bytes());

        // Simply delete the key - O(1) operation
        self.address_utxo_index.delete(&key)?;

        Ok(())
    }

    /// Get all UTxO keys for an address
    /// Uses prefix scan for efficient retrieval
    pub fn get_utxos_for_address(&self, address_hex: &str) -> Result<Vec<String>> {
        let prefix = format!("{}:", address_hex);
        let mut utxo_keys = Vec::new();

        // Scan all keys with this address prefix
        for (key, _value) in self.address_utxo_index.scan_prefix(prefix.as_bytes()) {
            // Key format is "address:utxo_key", extract the utxo_key part
            if let Ok(key_str) = std::str::from_utf8(key.as_ref()) {
                if let Some(utxo_key) = key_str.strip_prefix(&prefix) {
                    utxo_keys.push(utxo_key.to_string());
                }
            }
        }

        Ok(utxo_keys)
    }

    /// Add a transaction to the address history
    /// Uses individual keys for O(1) operations
    pub fn add_tx_to_address_history(&mut self, address_hex: &str, tx_hash_hex: &str) -> Result<()> {
        // Key format: address:tx_hash
        let index_key = format!("{}:{}", address_hex, tx_hash_hex);
        let key = Key::from(index_key.as_bytes());

        // Store a marker (just 1 byte)
        self.address_tx_index.insert(&key, &Value::from([1u8]))?;

        Ok(())
    }

    /// Get transaction history for an address
    /// Uses prefix scan for efficient retrieval
    pub fn get_tx_history_for_address(&self, address_hex: &str) -> Result<Vec<String>> {
        let prefix = format!("{}:", address_hex);
        let mut tx_hashes = Vec::new();

        // Scan all keys with this address prefix
        for (key, _value) in self.address_tx_index.scan_prefix(prefix.as_bytes()) {
            // Key format is "address:tx_hash", extract the tx_hash part
            if let Ok(key_str) = std::str::from_utf8(key.as_ref()) {
                if let Some(tx_hash) = key_str.strip_prefix(&prefix) {
                    tx_hashes.push(tx_hash.to_string());
                }
            }
        }

        Ok(tx_hashes)
    }

    /// Add a transaction to the policy index
    /// Tracks all transactions that include tokens from this policy
    pub fn add_tx_to_policy_index(&mut self, policy_id_hex: &str, tx_hash_hex: &str) -> Result<()> {
        // Key format: policy_id:tx_hash
        let index_key = format!("{}:{}", policy_id_hex, tx_hash_hex);
        let key = Key::from(index_key.as_bytes());

        // Store a marker (just 1 byte)
        self.policy_tx_index.insert(&key, &Value::from([1u8]))?;

        Ok(())
    }

    /// Get all transactions for a policy ID
    pub fn get_txs_for_policy(&self, policy_id_hex: &str) -> Result<Vec<String>> {
        let prefix = format!("{}:", policy_id_hex);
        let mut tx_hashes = Vec::new();

        // Scan all keys with this policy prefix
        for (key, _value) in self.policy_tx_index.scan_prefix(prefix.as_bytes()) {
            if let Ok(key_str) = std::str::from_utf8(key.as_ref()) {
                if let Some(tx_hash) = key_str.strip_prefix(&prefix) {
                    tx_hashes.push(tx_hash.to_string());
                }
            }
        }

        Ok(tx_hashes)
    }

    /// Add a transaction to the asset index
    /// Tracks all transactions that include a specific asset (policy + name)
    pub fn add_tx_to_asset_index(&mut self, policy_id_hex: &str, asset_name_hex: &str, tx_hash_hex: &str) -> Result<()> {
        // Key format: policy_id:asset_name:tx_hash
        let index_key = format!("{}:{}:{}", policy_id_hex, asset_name_hex, tx_hash_hex);
        let key = Key::from(index_key.as_bytes());

        // Store a marker (just 1 byte)
        self.asset_tx_index.insert(&key, &Value::from([1u8]))?;

        Ok(())
    }

    /// Get all transactions for a specific asset (policy + name)
    pub fn get_txs_for_asset(&self, policy_id_hex: &str, asset_name_hex: &str) -> Result<Vec<String>> {
        let prefix = format!("{}:{}:", policy_id_hex, asset_name_hex);
        let mut tx_hashes = Vec::new();

        // Scan all keys with this asset prefix
        for (key, _value) in self.asset_tx_index.scan_prefix(prefix.as_bytes()) {
            if let Ok(key_str) = std::str::from_utf8(key.as_ref()) {
                if let Some(tx_hash) = key_str.strip_prefix(&prefix) {
                    tx_hashes.push(tx_hash.to_string());
                }
            }
        }

        Ok(tx_hashes)
    }

    /// Save snapshots of all LSM trees
    ///
    /// This creates a consistent snapshot across all storage trees at the given slot.
    /// All snapshots use the same name (based on slot) for easy restoration.
    pub fn save_all_snapshots(&mut self, slot: u64) -> Result<()> {
        let snapshot_name = format!("slot-{:020}", slot);
        let label = format!("Slot {}", slot);

        tracing::debug!("Saving snapshots for all trees at slot {} ({})", slot, snapshot_name);

        // Save all regular LSM trees
        self.utxo_tree.save_snapshot(&snapshot_name, &label)?;
        // TODO: balance_tree is MonoidalLsmTree which doesn't expose save_snapshot yet
        // It's derived from utxo_tree so can be reconstructed on restore
        // self.balance_tree.save_snapshot(&snapshot_name, &label)?;
        self.governance_tree.save_snapshot(&snapshot_name, &label)?;
        self.nonce_tree.save_snapshot(&snapshot_name, &label)?;
        self.chain_tip_tree.save_snapshot(&snapshot_name, &label)?;
        self.address_utxo_index.save_snapshot(&snapshot_name, &label)?;
        self.address_tx_index.save_snapshot(&snapshot_name, &label)?;
        self.policy_tx_index.save_snapshot(&snapshot_name, &label)?;
        self.asset_tx_index.save_snapshot(&snapshot_name, &label)?;
        self.spent_utxo_index.save_snapshot(&snapshot_name, &label)?;
        self.block_events_tree.save_snapshot(&snapshot_name, &label)?;
        self.block_hash_index.save_snapshot(&snapshot_name, &label)?;

        // Save rewards tracker snapshots (3 more trees)
        self.rewards_tracker.save_snapshot(slot)?;

        tracing::info!("Saved snapshots for all trees at slot {}", slot);
        Ok(())
    }

    /// Cleanup old snapshots from all LSM trees, keeping only the N most recent
    ///
    /// This should be called after save_all_snapshots() to prevent unbounded
    /// accumulation of snapshots which severely impacts snapshot creation performance.
    ///
    /// # Arguments
    /// * `keep_latest` - Number of snapshots to keep (default: 10)
    pub fn cleanup_all_snapshots(&mut self, keep_latest: Option<usize>) -> Result<()> {
        use crate::snapshot_manager::SnapshotManager;

        let keep = keep_latest.unwrap_or(10);
        let snapshot_manager = SnapshotManager::default();

        tracing::debug!("Cleaning up old snapshots, keeping {} most recent", keep);

        // Track cleanup stats
        let mut total_cleaned = 0;
        let mut errors = Vec::new();

        // Cleanup function for a single tree
        let mut cleanup_tree = |tree_subdir: &str, tree_name: &str| {
            let tree_path = self.network_path.join(tree_subdir);
            match snapshot_manager.cleanup_old_snapshots(&tree_path, Some(keep)) {
                Ok(()) => {
                    // Count how many were deleted by checking directory
                    if let Ok(snapshots_dir) = std::fs::read_dir(tree_path.join("snapshots")) {
                        let remaining = snapshots_dir.filter(|e| e.is_ok()).count();
                        let deleted = remaining.saturating_sub(keep);
                        if deleted > 0 {
                            total_cleaned += deleted;
                            tracing::debug!("Cleaned {} old snapshot(s) from {}", deleted, tree_name);
                        }
                    }
                }
                Err(e) => {
                    errors.push(format!("{}: {}", tree_name, e));
                }
            }
        };

        // Cleanup all LSM trees
        cleanup_tree("utxos", "utxos");
        cleanup_tree("governance", "governance");
        cleanup_tree("nonces", "nonces");
        cleanup_tree("chain_tip", "chain_tip");
        cleanup_tree("address_utxos", "address_utxos");
        cleanup_tree("address_txs", "address_txs");
        cleanup_tree("policy_txs", "policy_txs");
        cleanup_tree("asset_txs", "asset_txs");
        cleanup_tree("spent_utxos", "spent_utxos");
        cleanup_tree("block_events", "block_events");
        cleanup_tree("block_hash_index", "block_hash_index");

        // Cleanup rewards tracker snapshots (3 trees)
        cleanup_tree("rewards/pool_stake", "rewards/pool_stake");
        cleanup_tree("rewards/wallet_rewards", "rewards/wallet_rewards");
        cleanup_tree("rewards/reward_snapshots", "rewards/reward_snapshots");

        if !errors.is_empty() {
            tracing::warn!("Snapshot cleanup encountered {} error(s): {}", errors.len(), errors.join("; "));
        }

        if total_cleaned > 0 {
            tracing::info!("Cleaned up {} old snapshot(s) across all trees", total_cleaned);
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ChainTip {
    pub height: u64,
    pub slot: u64,
    pub hash: Vec<u8>,
    pub timestamp: u64,  // Unix milliseconds
}

pub struct HayateIndexer {
    pub networks: RwLock<HashMap<Network, StorageHandle>>,
    pub account_xpubs: RwLock<Vec<String>>,
    pub tracked_addresses: RwLock<Vec<String>>,
    pub gap_limit: u32,
    block_updates: broadcast::Sender<BlockUpdate>,
    restart_signal: broadcast::Sender<u64>, // Signal to restart chain sync from specific slot
}

impl HayateIndexer {
    pub fn new(_base_path: PathBuf, gap_limit: u32) -> Result<Self> {
        // Create broadcast channel with capacity for 1000 block updates
        let (block_updates, _) = broadcast::channel(1000);
        // Create broadcast channel for restart signals
        let (restart_signal, _) = broadcast::channel(10);

        Ok(Self {
            networks: RwLock::new(HashMap::new()),
            account_xpubs: RwLock::new(Vec::new()),
            tracked_addresses: RwLock::new(Vec::new()),
            gap_limit,
            block_updates,
            restart_signal,
        })
    }

    /// Subscribe to block updates
    pub fn subscribe_blocks(&self) -> broadcast::Receiver<BlockUpdate> {
        self.block_updates.subscribe()
    }

    /// Broadcast a new block update to all subscribers
    pub fn broadcast_block(&self, update: BlockUpdate) {
        // Ignore send errors (no subscribers is okay)
        let _ = self.block_updates.send(update);
    }

    /// Subscribe to chain sync restart signals
    pub fn subscribe_restart(&self) -> broadcast::Receiver<u64> {
        self.restart_signal.subscribe()
    }

    /// Signal chain sync to restart from a specific slot
    pub fn signal_restart(&self, slot: u64) {
        tracing::info!("📢 Signaling chain sync to restart from slot {}", slot);
        let _ = self.restart_signal.send(slot);
    }

    pub async fn add_network(&self, network: Network, base_path: PathBuf) -> Result<()> {
        let storage = NetworkStorage::open(base_path, network.clone())?;

        // Create storage manager and spawn its task
        let (manager, handle) = StorageManager::new(storage);
        tokio::spawn(async move {
            manager.run().await;
        });

        self.networks.write().await.insert(network, handle);
        Ok(())
    }
    
    pub async fn add_account(&self, xpub: String) -> Result<()> {
        self.account_xpubs.write().await.push(xpub);
        Ok(())
    }

    pub async fn add_address(&self, address: String) -> Result<()> {
        self.tracked_addresses.write().await.push(address);
        Ok(())
    }
    
    pub async fn get_chain_tip(&self) -> Result<ChainTip> {
        Ok(ChainTip {
            height: 0,
            slot: 0,
            hash: vec![],
            timestamp: 0,
        })
    }
}
