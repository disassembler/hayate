//! Validation tests against Haskell cardano-ledger ground truth
//!
//! These tests compare hayate's ledger calculations against known-correct values
//! from the Haskell cardano-ledger implementation running on SanchoNet.

use crate::ledger::primitives::{EpochNo, Hash28Ext, Lovelace, ProtocolParameters, Rational};
use crate::ledger::state::{LedgerState, PoolRegistration, StakeSnapshot};
use std::collections::HashMap;
use std::sync::Arc;

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
#[ignore]
fn test_epoch_0_to_1_calculation() {
    // Test the specific calculation for epoch 0→1 that the user identified.
    //
    // With deferred RUPD (matching Haskell), the RUPD computed at the 0→1
    // transition is stored in pending_reward_update and only applied at the
    // 1→2 transition.  So we must call process_epoch_transition twice.
    //
    // After the 0→1 transition:
    //   - pending_reward_update contains the RUPD
    //   - reserves/treasury are unchanged (no previous RUPD to apply)
    //
    // After the 1→2 transition:
    //   - The 0→1 RUPD is applied: treasury/reserves change
    //   - A new RUPD for epoch 1→2 is computed and stored

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

    // Process epoch 0→1: computes RUPD and stores it (deferred).
    // No previous RUPD to apply, so reserves/treasury stay the same.
    ledger.process_epoch_transition(EpochNo(1));

    // After 0→1: RUPD is pending, not yet applied
    assert_eq!(ledger.treasury.0, 0, "Treasury should be 0 after epoch 0→1 (RUPD is deferred)");
    assert_eq!(ledger.reserves.0, reserves_initial, "Reserves should be unchanged after epoch 0→1");
    assert!(ledger.pending_reward_update.is_some(), "pending_reward_update should be set");

    // Verify the pending RUPD has correct expansion.
    // Note: the RUPD also includes feeSS (epoch 0 fees = 438,057) in rPot,
    // so deltaT1 = floor(tau * (expansion + fees)) rather than floor(tau * expansion).
    let rupd = ledger.pending_reward_update.as_ref().unwrap();
    assert_eq!(rupd.delta_r1, expansion, "pending RUPD deltaR1 should equal expansion");

    // Process epoch 1→2: applies the epoch 0→1 RUPD, then computes a new one.
    ledger.process_epoch_transition(EpochNo(2));

    // The exact values depend on fees (438,057 lovelace from epoch 0):
    //   rPot = expansion + feeSS = 45,000,000,000,000 + 438,057 = 45,000,000,438,057
    //   deltaT1 = floor(1/5 * rPot) = 9,000,000,087,611
    //   rewardPot = rPot - deltaT1 = 35,999,999,912,389 + 438,057 = 36,000,000,350,446
    //   (no pools → totalDistributed = 0 → undistributed = rewardPot)
    //   reserves' = 15T - expansion + undistributed
    //   treasury' = 0 + deltaT1
    let expected_exact_treasury = 9_000_000_087_611u64;
    let expected_exact_reserves = reserves_initial - expansion + (expansion + 438_057 - expected_exact_treasury);

    println!("\nActual results (after epoch 1→2 applies the deferred RUPD):");
    println!("  treasury: {} lovelace", ledger.treasury.0);
    println!("  reserves: {} lovelace", ledger.reserves.0);
    println!("\nExpected (exact calculation):");
    println!("  treasury: {} lovelace", expected_exact_treasury);
    println!("  reserves: {} lovelace", expected_exact_reserves);

    assert_eq!(ledger.treasury.0, expected_exact_treasury, "Treasury mismatch");
    assert_eq!(ledger.reserves.0, expected_exact_reserves, "Reserves mismatch");
}

/// Test the epoch 3→4 RUPD against preview network Haskell dumps.
///
/// Source data: haskell-dump-configs/3-259215.json and 4-345612.json
///
/// All three pools share the same reward account (keyHash-b96fcef3...) which is
/// NOT registered as a stake key.  With margin=1, member rewards=0 and all leader
/// rewards target that unregistered account, so `calculate_rewards` produces no
/// payouts.  The entire `rewardPot` is returned as `undistributed` and flows back
/// to reserves via `applyRUpd`; only the τ·rPot treasury cut actually leaves reserves.
///
/// createRUpd verification:
///   deltaR1  = floor(3/1000 × 14_973_016_197_275_303) = 44_919_048_591_825
///   feeSS    = 205_981
///   rPot     = deltaR1 + feeSS                         = 44_919_048_797_806
///   deltaT1  = floor(1/5 × rPot)                       =  8_983_809_759_561
///   rewardPot = rPot − deltaT1                         = 35_935_239_038_245
///   totalDistributed = 0  (reward acct unregistered)
///   deltaR2  = rewardPot − 0                           = 35_935_239_038_245
///
/// applyRUpd verification:
///   reserves' = 14_973_016_197_275_303 − 44_919_048_591_825 + 35_935_239_038_245
///             = 14_964_032_387_721_723  ✓ (matches 4-345612.json)
///   treasury' = 26_983_803_369_087 + 8_983_809_759_561
///             = 35_967_613_128_648      ✓ (matches 4-345612.json)
#[test]
#[ignore]
fn test_epoch3_to_4_rupd_unregistered_reward_accounts() {
    fn hex28(s: &str) -> [u8; 28] {
        let b = hex::decode(s).unwrap();
        let mut h = [0u8; 28];
        h.copy_from_slice(&b);
        h
    }

    // Protocol params from epoch 3 dump (prevPp used by createRUpd)
    let mut params = ProtocolParameters::default();
    params.rho = Rational { numerator: 3, denominator: 1000 };
    params.tau = Rational { numerator: 1, denominator: 5 };
    params.a0  = Rational { numerator: 3, denominator: 10 };
    params.n_opt = 150;
    params.decentralization = Rational { numerator: 0, denominator: 1 };
    params.active_slot_coefficient = Rational { numerator: 1, denominator: 20 };

    let mut state = LedgerState::new(params);
    state.epoch        = EpochNo(3);
    state.epoch_length = 86400;
    state.reserves     = Lovelace(14_973_016_197_275_303);
    state.treasury     = Lovelace(26_983_803_369_087);
    // d=0 at epoch 3: eta = min(blocks_produced, expected) / expected
    state.prev_epoch_decentralization = Rational { numerator: 0, denominator: 1 };
    // IMPORTANT: pool reward account keyHash-b96fcef3... is intentionally NOT
    // inserted into state.reward_accounts — it is unregistered.

    // Pool IDs and owner keys from the epoch 3 go snapshot
    let pool_38f4 = hex28("38f4a58aaf3fec84f3410520c70ad75321fb651ada7ca026373ce486");
    let pool_40d8 = hex28("40d806d73c8d2a0c8d9b1e95ccb9f380e40cb4d4b23ff6e403ae1456");
    let pool_d5cf = hex28("d5cfc42cf67f6b637688d19fa50a4342658f63370b9e2c9e3eaf4dfe");

    let own_788c = hex28("788cf0519348fefaf3c721c5f5bd60b195b444fa0d8fb4512dc259be");
    let own_ba14 = hex28("ba149e2e2379097e65f0c03f2733d3103151e7f100d36dfdb01a0b22");
    let own_f631 = hex28("f631370cc87882bf5e14ab72534caf2655d0a2a50a9a8a3820bb6f4a");

    // All three pools share one unregistered reward account
    let mut reward_account = vec![0xe0u8]; // testnet key reward address header
    reward_account.extend_from_slice(&hex28("b96fcef3b9351af6834bd850e3a97859d7bd5b729d24bf3646aeaccf"));

    let make_pool = |pool_id: [u8; 28], owner: [u8; 28]| PoolRegistration {
        pool_id,
        vrf_keyhash: [0u8; 32],
        pledge: Lovelace(100_000_000_000_000),
        cost: Lovelace(500_000_000),
        margin_numerator: 1,   // 100% margin → member rewards = 0
        margin_denominator: 1,
        reward_account: reward_account.clone(),
        owners: vec![owner],
        relays: vec![],
        metadata_url: None,
        metadata_hash: None,
    };

    // Delegator credentials (owner key padded to Hash32)
    let cred_788c = own_788c.to_hash32_padded();
    let cred_ba14 = own_ba14.to_hash32_padded();
    let cred_f631 = own_f631.to_hash32_padded();

    let mut delegations = HashMap::new();
    delegations.insert(cred_788c, pool_38f4);
    delegations.insert(cred_ba14, pool_d5cf);
    delegations.insert(cred_f631, pool_40d8);

    let mut pool_stake = HashMap::new();
    pool_stake.insert(pool_38f4, Lovelace(100_000_000_000_000));
    pool_stake.insert(pool_40d8, Lovelace(100_000_000_000_000));
    pool_stake.insert(pool_d5cf, Lovelace(100_000_000_000_000));

    let mut pool_params = HashMap::new();
    pool_params.insert(pool_38f4, make_pool(pool_38f4, own_788c));
    pool_params.insert(pool_40d8, make_pool(pool_40d8, own_f631));
    pool_params.insert(pool_d5cf, make_pool(pool_d5cf, own_ba14));

    // Per-delegator stake (needed for pledge verification: owner_stake >= pledge)
    let mut stake_distribution = HashMap::new();
    stake_distribution.insert(cred_788c, Lovelace(100_000_000_000_000));
    stake_distribution.insert(cred_ba14, Lovelace(100_000_000_000_000));
    stake_distribution.insert(cred_f631, Lovelace(100_000_000_000_000));

    // Block counts from epoch 3's go snapshot (total 4372 > expected 4320 → eta=1)
    let mut blocks = HashMap::new();
    blocks.insert(pool_38f4, 1514u64);
    blocks.insert(pool_40d8, 1415u64);
    blocks.insert(pool_d5cf, 1443u64);

    let go_snapshot = StakeSnapshot {
        epoch: EpochNo(1), // go = set→mark from 2 epochs back
        delegations: Arc::new(delegations),
        pool_stake,
        pool_params: Arc::new(pool_params),
        stake_distribution: Arc::new(stake_distribution),
        epoch_blocks_by_pool: Arc::new(blocks),
        script_stake_credentials: Default::default(),
    };

    // === createRUpd ===
    let fees = Lovelace(205_981); // feeSS from epoch 3 snapshot
    let rupd = state.calculate_rewards(&go_snapshot, fees, &state.protocol_params.clone());

    // Verify each RUPD field against the rupdNext values in 3-259215.json
    assert_eq!(rupd.delta_r1,                  44_919_048_591_825, "deltaR1");
    assert_eq!(rupd.r_pot,                      44_919_048_797_806, "rPot");
    assert_eq!(rupd.delta_t1,                    8_983_809_759_561, "deltaT1");
    assert_eq!(rupd.reward_pot_after_treasury,  35_935_239_038_245, "rewardPot");
    assert_eq!(rupd.total_distributed,                           0, "totalDistributed");
    assert!(rupd.rewards.is_empty(), "rewards must be empty: all leader rewards target an unregistered account; member rewards are 0 (margin=1)");
    assert_eq!(rupd.undistributed, rupd.reward_pot_after_treasury,  "undistributed == rewardPot (nothing distributed)");

    // === applyRUpd ===
    // Mirror the inline application in epoch.rs::process_epoch_transition
    state.reserves.0 -= rupd.delta_reserves; // subtract monetary expansion
    state.reserves.0 += rupd.undistributed;  // add back undistributed reward pot
    state.treasury.0 += rupd.delta_treasury; // treasury receives tau cut only
    // rupd.rewards is empty so no per-account credits needed

    // Verify final state against epoch 4 dump (4-345612.json)
    assert_eq!(state.reserves.0, 14_964_032_387_721_723, "epoch 4 reserves");
    assert_eq!(state.treasury.0, 35_967_613_128_648,     "epoch 4 treasury");
}
