// Hayate library - UTxORPC Cardano indexer

pub mod cli;
pub mod mock_types;
pub mod wallet;
pub mod gpg;
pub mod chain_sync;
pub mod keys;
pub mod rewards;  // Must come before indexer
// pub mod rewards_calculation;  // Rewards calculation logic (deprecated - now in ledger/rewards.rs)
pub mod snapshot_manager;  // LSM snapshot management
pub mod genesis;  // Genesis file parsing (Byron, Shelley, Alonzo, Conway)
pub mod ledger;  // Full ledger state management (epoch transitions, governance, validation)
pub mod indexer;
pub mod api;
pub mod config;
pub mod storage;
pub mod node;  // Full node with ledger state snapshots
pub mod protocol_params;  // Protocol parameter querying and management

pub use indexer::{HayateIndexer, Network, NetworkStorage, ChainTip, BlockProcessor, BlockStats};
pub use config::HayateConfig;
pub use chain_sync::HayateSync;
pub use wallet::{WalletStorage, WalletMetadata};
