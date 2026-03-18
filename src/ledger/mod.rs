// Ledger state management for Hayate
//
// This module implements full Cardano ledger state tracking including:
// - Epoch boundary transitions (NEWEPOCH STS rule)
// - Reward calculations (BigInt-based for accuracy)
// - Conway governance (CIP-1694)
// - Deposit tracking (voting vs staking stake separation)
// - Nonce state machine (TICKN rule)
// - Optional transaction validation

pub mod primitives;
pub mod rational;
pub mod state;
pub mod rewards;
pub mod nonce;
pub mod epoch;
pub mod governance;
pub mod certificates;
pub mod validation;
pub mod snapshot;
pub mod rebuild;
pub mod eras;

#[cfg(test)]
mod tests;

// Re-export commonly used types
pub use primitives::*;
pub use rational::Rat;
pub use state::*;
pub use nonce::blake2b_256;
