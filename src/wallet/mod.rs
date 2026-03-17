// Wallet module for Hayate
// Implements pure Rust wallet functionality with GPG encrypted mnemonics

pub mod mnemonic;
pub mod storage;
pub mod derivation;
pub mod transaction;
pub mod utxorpc_client;
pub mod cli;
#[allow(dead_code)]
pub mod plutus;
// TODO: Migrate away from pallas-txbuilder (deprecated in pallas 1.0)
#[allow(dead_code)]
pub mod tx_builder;
#[allow(clippy::module_inception)]
pub mod wallet;
#[allow(dead_code)]
pub mod unified_tx;
pub mod simulator;
#[cfg(test)]
pub mod test_validators;

pub use storage::*;
pub use cli::handle_wallet_command;
pub use wallet::{Wallet, ed25519_secret_to_privatekey, ed25519_secret_to_extended_privatekey};
#[allow(unused_imports)]
pub use derivation::{Network, DerivationError, DerivationResult};
