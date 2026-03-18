//! Validation tests against Haskell cardano-ledger ground truth
//!
//! These tests compare hayate's ledger calculations against known-correct values
//! from the Haskell cardano-ledger implementation running on SanchoNet.

use crate::ledger::primitives::{EpochNo, Lovelace, ProtocolParameters, Rational};
use crate::ledger::state::LedgerState;
use std::collections::HashMap;

/// Expected ledger state at an epoch boundary from Haskell cardano-ledger
#[derive(Debug, Clone)]
struct HaskellEpochState {
    epoch: u64,
    treasury_lovelace: u64,
    reserves_lovelace: u64,
    total_rewards_distributed: u64,
    reward_accounts: HashMap<String, u64>, // hex stake key hash -> lovelace
}

/// Ground truth values from Haskell cardano-ledger on SanchoNet
///
/// To populate these values:
/// 1. Run cardano-node on sanchonet
/// 2. Query ledger state at each epoch boundary using cardano-cli
/// 3. Record treasury, reserves, and reward account balances
fn haskell_ground_truth() -> Vec<HaskellEpochState> {
    vec![
        // Epoch 1: First epoch after genesis
        HaskellEpochState {
            epoch: 1,
            treasury_lovelace: 9_000_000_000_000, // 9M ADA (from user's earlier message)
            reserves_lovelace: 14_991_000_000_000_000, // 15B - 9M ADA
            total_rewards_distributed: 0, // TODO: get from Haskell
            reward_accounts: HashMap::new(), // TODO: populate from Haskell
        },

        // Epoch 2: Second epoch
        HaskellEpochState {
            epoch: 2,
            treasury_lovelace: 0, // TODO: user said "17994600" ADA approx, need exact lovelace
            reserves_lovelace: 0, // TODO: get from Haskell
            total_rewards_distributed: 0,
            reward_accounts: HashMap::new(),
        },

        // Epoch 3
        HaskellEpochState {
            epoch: 3,
            treasury_lovelace: 0, // TODO: get from Haskell
            reserves_lovelace: 0,
            total_rewards_distributed: 0,
            reward_accounts: HashMap::new(),
        },

        // Epoch 4
        HaskellEpochState {
            epoch: 4,
            treasury_lovelace: 0, // TODO: get from Haskell
            reserves_lovelace: 0,
            total_rewards_distributed: 0,
            reward_accounts: HashMap::new(),
        },

        // Epoch 5
        HaskellEpochState {
            epoch: 5,
            treasury_lovelace: 0, // TODO: get from Haskell
            reserves_lovelace: 0,
            total_rewards_distributed: 0,
            reward_accounts: HashMap::new(),
        },
    ]
}

#[test]
#[ignore] // Remove this when we have real Haskell values
fn test_epoch_transitions_match_haskell() {
    // Initialize ledger state from SanchoNet genesis
    let params = ProtocolParameters {
        rho: Rational { numerator: 3, denominator: 1000 },
        tau: Rational { numerator: 1, denominator: 5 },
        decentralization: Rational { numerator: 1, denominator: 1 }, // d = 1.0
        a0: Rational { numerator: 3, denominator: 10 },
        n_opt: 500,
        min_fee_a: 44,
        min_fee_b: 155381,
        pool_deposit: 500_000_000,
        key_deposit: 2_000_000,
        min_pool_cost: 340_000_000,
        ..ProtocolParameters::default()
    };
    let mut ledger = LedgerState::new(params);

    // Set SanchoNet parameters
    ledger.set_epoch_length(86400); // 1 day epochs

    // Seed genesis UTxOs (30M ADA per the genesis file)
    let genesis_utxo_total = 30_000_000_000_000_000u64; // 30M ADA in lovelace
    ledger.seed_genesis_utxos(genesis_utxo_total);

    // Expected initial state
    assert_eq!(ledger.reserves.0, 15_000_000_000_000_000); // 45M - 30M = 15M ADA
    assert_eq!(ledger.treasury.0, 0);

    let ground_truth = haskell_ground_truth();

    for expected in ground_truth.iter() {
        // TODO: Process blocks for this epoch
        // This would require replaying actual blocks from the chain
        // For now, we'll test the reward calculation logic directly

        println!("\n=== Testing Epoch {} ===", expected.epoch);

        // Process epoch transition
        ledger.process_epoch_transition(EpochNo(expected.epoch));

        // Compare treasury (in lovelace for exact precision)
        assert_eq!(
            ledger.treasury.0,
            expected.treasury_lovelace,
            "Epoch {} treasury mismatch: hayate={} lovelace, haskell={} lovelace, diff={} lovelace",
            expected.epoch,
            ledger.treasury.0,
            expected.treasury_lovelace,
            (ledger.treasury.0 as i128 - expected.treasury_lovelace as i128).abs()
        );

        // Compare reserves (in lovelace for exact precision)
        assert_eq!(
            ledger.reserves.0,
            expected.reserves_lovelace,
            "Epoch {} reserves mismatch: hayate={} lovelace, haskell={} lovelace, diff={} lovelace",
            expected.epoch,
            ledger.reserves.0,
            expected.reserves_lovelace,
            (ledger.reserves.0 as i128 - expected.reserves_lovelace as i128).abs()
        );

        // Verify ADA conservation: reserves + treasury + circulation = 45M ADA
        let total = ledger.reserves.0 + ledger.treasury.0 + genesis_utxo_total;
        assert_eq!(
            total,
            45_000_000_000_000_000,
            "Epoch {} ADA conservation violated: total={} lovelace",
            expected.epoch,
            total
        );

        println!("✓ Epoch {} matches Haskell exactly", expected.epoch);
    }
}

#[test]
fn test_epoch_0_to_1_calculation() {
    // Test the specific calculation for epoch 0→1 that the user identified

    let params = ProtocolParameters {
        rho: Rational { numerator: 3, denominator: 1000 }, // 0.003
        tau: Rational { numerator: 1, denominator: 5 },     // 0.2
        decentralization: Rational { numerator: 1, denominator: 1 }, // 1.0
        a0: Rational { numerator: 3, denominator: 10 },
        n_opt: 500,
        min_fee_a: 44,
        min_fee_b: 155381,
        pool_deposit: 500_000_000,
        key_deposit: 2_000_000,
        min_pool_cost: 340_000_000,
        ..ProtocolParameters::default()
    };
    let mut ledger = LedgerState::new(params);

    ledger.set_epoch_length(86400);
    ledger.seed_genesis_utxos(30_000_000_000_000_000);

    // Epoch 0 state (before transition to epoch 1)
    // SanchoNet starts in Alonzo era, so epoch 0 has real blocks/transactions
    // From Haskell dumps: epoch 0 accumulated 438,057 lovelace in fees
    ledger.epoch_fees = Lovelace(438_057);
    ledger.epoch_block_count = 4320; // Expected blocks in epoch 0 (86400 * 0.05)

    // Calculate expected values
    let reserves_initial = 15_000_000_000_000_000u64;
    let expansion = ((reserves_initial as f64) * 0.003).floor() as u64;
    let fees = 0u64; // NO fees in epoch 0
    let total_rewards = expansion + fees;
    let treasury_cut = ((total_rewards as f64) * 0.2).floor() as u64;

    println!("\nExpected calculation for epoch 0→1:");
    println!("  reserves_initial: {} lovelace", reserves_initial);
    println!("  expansion (rho × reserves): {} lovelace", expansion);
    println!("  fees: {} lovelace (NO fees in epoch 0!)", fees);
    println!("  total_rewards (expansion + fees): {} lovelace", total_rewards);
    println!("  treasury_cut (tau × total_rewards): {} lovelace", treasury_cut);

    // User said Haskell has treasury = 9M ADA at epoch 1
    let expected_treasury = 9_000_000_000_000u64;
    let expected_reserves = 14_991_000_000_000_000u64; // 15B - 9M

    // Process transition
    ledger.process_epoch_transition(EpochNo(1));

    println!("\nActual results:");
    println!("  treasury: {} lovelace", ledger.treasury.0);
    println!("  reserves: {} lovelace", ledger.reserves.0);
    println!("\nExpected (from Haskell):");
    println!("  treasury: {} lovelace", expected_treasury);
    println!("  reserves: {} lovelace", expected_reserves);
    println!("\nDifference:");
    println!("  treasury diff: {} lovelace", (ledger.treasury.0 as i128 - expected_treasury as i128).abs());
    println!("  reserves diff: {} lovelace", (ledger.reserves.0 as i128 - expected_reserves as i128).abs());

    assert_eq!(ledger.treasury.0, expected_treasury, "Treasury mismatch");
    assert_eq!(ledger.reserves.0, expected_reserves, "Reserves mismatch");
}
