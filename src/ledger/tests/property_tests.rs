// Property-based tests for Hayate ledger invariants.
//
// Tests mathematical properties that must hold for all valid inputs across all
// eras. Uses proptest 1.4 for automated test-case generation.
//
// Organised into four sections:
//   1. Rational arithmetic — Rat commutativity, floor bounds, GCD normalisation
//   2. Reward calculation — pot conservation, expansion bounds, treasury cut, pledge check
//   3. Epoch transition — snapshot rotation, epoch counter, ADA conservation
//   4. Deposit tracker — add/refund round-trip, voting vs staking stake ordering

use proptest::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;

use crate::ledger::primitives::*;
use crate::ledger::rational::Rat;
use crate::ledger::state::{
    DepositTracker, DepositType, LedgerState, PoolRegistration, StakeSnapshot, MAX_LOVELACE_SUPPLY,
};

// ── Shared generators ────────────────────────────────────────────────────────

/// A unit rational p/q where 0 ≤ p ≤ q and q ∈ [1, 1000].
fn arb_unit_rational() -> impl Strategy<Value = Rational> {
    (1u64..=1000u64).prop_flat_map(|d| {
        (0u64..=d).prop_map(move |n| Rational { numerator: n, denominator: d })
    })
}

fn arb_hash28() -> impl Strategy<Value = Hash28> {
    proptest::array::uniform28(any::<u8>())
}

fn arb_hash32() -> impl Strategy<Value = Hash32> {
    proptest::array::uniform32(any::<u8>())
}

/// ProtocolParameters with the reward-relevant fields set to arbitrary valid values.
/// All other fields are left at their defaults.
fn arb_protocol_params() -> impl Strategy<Value = ProtocolParameters> {
    (
        arb_unit_rational(), // rho  (monetary expansion rate)
        arb_unit_rational(), // tau  (treasury growth rate)
        arb_unit_rational(), // a0   (pledge influence)
        1u64..=500u64,       // n_opt (optimal pool count k)
        arb_unit_rational(), // decentralization (d parameter)
    )
    .prop_map(|(rho, tau, a0, n_opt, d)| {
        let mut pp = ProtocolParameters::default();
        pp.rho = rho;
        pp.tau = tau;
        pp.a0 = a0;
        pp.n_opt = n_opt;
        pp.decentralization = d;
        pp
    })
}

/// A go-snapshot with exactly one pool.
///
/// `owner_stake_pct` controls what fraction of `pool_stake` belongs to the
/// pool operator (as a value in 0..=100).  The remainder is assigned to a
/// separate delegator credential so the pledge check is well-defined.
prop_compose! {
    fn arb_single_pool_snapshot()(
        pool_id   in arb_hash28(),
        owner     in arb_hash28(),
        // Keep pledge small enough to be satisfiable from owner stake
        pledge    in 0u64..=10_000_000_000u64,       // 0–10 000 ADA
        cost      in 0u64..=500_000_000u64,           // 0–500 ADA
        margin    in arb_unit_rational(),
        pool_stake in 1u64..=1_000_000_000_000_000u64,
        blocks    in 0u64..=10_000u64,
        owner_pct in 0u64..=100u64,                   // owner's share of pool stake
    ) -> StakeSnapshot {
        let owner_stake = pool_stake * owner_pct / 100;
        let delegator_stake = pool_stake.saturating_sub(owner_stake);

        let owner_cred = owner.to_hash32_padded();
        // Delegator gets a deterministic credential distinct from the owner
        let delegator_cred: Hash32 = {
            let mut h = [0u8; 32];
            h[..28].copy_from_slice(&pool_id);
            h[0] ^= 0xFF; // flip to make it different from pool_id-based keys
            h
        };

        let mut reward_account = vec![0xE1u8]; // testnet reward address header
        reward_account.extend_from_slice(&owner[..]);

        let pool_reg = PoolRegistration {
            pool_id,
            vrf_keyhash: [0u8; 32],
            pledge: Lovelace(pledge),
            cost: Lovelace(cost),
            margin_numerator: margin.numerator,
            margin_denominator: margin.denominator.max(1),
            reward_account,
            owners: vec![owner],
            relays: vec![],
            metadata_url: None,
            metadata_hash: None,
        };

        let mut delegations = HashMap::new();
        delegations.insert(owner_cred, pool_id);
        if delegator_stake > 0 {
            delegations.insert(delegator_cred, pool_id);
        }

        let mut pool_stake_map = HashMap::new();
        pool_stake_map.insert(pool_id, Lovelace(pool_stake));

        let mut pool_params = HashMap::new();
        pool_params.insert(pool_id, pool_reg);

        let mut stake_dist = HashMap::new();
        stake_dist.insert(owner_cred, Lovelace(owner_stake));
        if delegator_stake > 0 {
            stake_dist.insert(delegator_cred, Lovelace(delegator_stake));
        }

        let mut blocks_map = HashMap::new();
        if blocks > 0 {
            blocks_map.insert(pool_id, blocks);
        }

        StakeSnapshot {
            epoch: EpochNo(0),
            delegations: Arc::new(delegations),
            pool_stake: pool_stake_map,
            pool_params: Arc::new(pool_params),
            stake_distribution: Arc::new(stake_dist),
            epoch_blocks_by_pool: Arc::new(blocks_map),
        }
    }
}

/// Convenience: build a minimal LedgerState with arbitrary protocol params and reserves.
fn make_state(pp: ProtocolParameters, reserves: u64) -> LedgerState {
    let mut state = LedgerState::new(pp);
    state.epoch = EpochNo(10);
    state.epoch_length = 86_400;
    state.reserves = Lovelace(reserves);
    state
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 1 — Rational arithmetic
// ═══════════════════════════════════════════════════════════════════════════

proptest! {
    // Multiplication is commutative: a/b * c/d == c/d * a/b
    #[test]
    fn prop_rat_mul_commutative(
        a in 0i128..=10_000i128,
        b in 1i128..=10_000i128,
        c in 0i128..=10_000i128,
        d in 1i128..=10_000i128,
    ) {
        let lhs = Rat::from_i128(a, b).mul(&Rat::from_i128(c, d));
        let rhs = Rat::from_i128(c, d).mul(&Rat::from_i128(a, b));
        prop_assert_eq!(lhs.n, rhs.n, "mul commutativity: numerators differ");
        prop_assert_eq!(lhs.d, rhs.d, "mul commutativity: denominators differ");
    }

    // Addition is commutative: a/b + c/d == c/d + a/b
    #[test]
    fn prop_rat_add_commutative(
        a in 0i128..=10_000i128,
        b in 1i128..=10_000i128,
        c in 0i128..=10_000i128,
        d in 1i128..=10_000i128,
    ) {
        let lhs = Rat::from_i128(a, b).add(&Rat::from_i128(c, d));
        let rhs = Rat::from_i128(c, d).add(&Rat::from_i128(a, b));
        prop_assert_eq!(lhs.n, rhs.n);
        prop_assert_eq!(lhs.d, rhs.d);
    }

    // floor(a/b) <= a/b:  i.e. floor * b <= a  (all positive)
    #[test]
    fn prop_rat_floor_does_not_exceed_value(
        a in 0i128..=1_000_000_000_000i128,
        b in 1i128..=1_000_000_000_000i128,
    ) {
        let r = Rat::from_i128(a, b);
        let f = r.floor_u64() as i128;
        // f * b <= a
        prop_assert!(f * b <= a, "floor({}/{}) = {} but {} * {} = {} > {}", a, b, f, f, b, f*b, a);
    }

    // floor(a/b) is the largest integer n such that n <= a/b
    // Equivalently: (floor + 1) * b > a  (floor+1 is strictly above a/b)
    #[test]
    fn prop_rat_floor_is_tight(
        a in 0i128..=1_000_000_000_000i128,
        b in 1i128..=1_000_000_000_000i128,
    ) {
        let r = Rat::from_i128(a, b);
        let f = r.floor_u64() as i128;
        // (f + 1) * b > a
        prop_assert!((f + 1) * b > a,
            "floor({}/{}) = {} but ({} + 1) * {} = {} <= {}",
            a, b, f, f, b, (f+1)*b, a);
    }

    // GCD normalisation: 2n / 2d should equal n / d
    #[test]
    fn prop_rat_gcd_normalises(
        n in 0i128..=100_000i128,
        d in 1i128..=100_000i128,
    ) {
        let r1 = Rat::from_i128(n, d);
        let r2 = Rat::from_i128(2 * n, 2 * d);
        prop_assert_eq!(r1.n, r2.n, "GCD normalisation failed (numerators differ)");
        prop_assert_eq!(r1.d, r2.d, "GCD normalisation failed (denominators differ)");
    }

    // mul then div by same value is identity (for non-zero values):
    //   floor((a/b * c/1) / (c/1)) == floor(a/b)   when c > 0
    #[test]
    fn prop_rat_mul_div_identity(
        a in 0i128..=1_000_000i128,
        b in 1i128..=1_000_000i128,
        c in 1i128..=1_000i128,
    ) {
        let r = Rat::from_i128(a, b);
        let scaled = r.mul(&Rat::from_i128(c, 1));
        let restored = scaled.div(&Rat::from_i128(c, 1));
        prop_assert_eq!(
            r.floor_u64(), restored.floor_u64(),
            "floor({}/{}) != floor(({}/{}*{}/1) / ({}/1))",
            a, b, a, b, c, c
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 2 — Reward calculation invariants (all eras)
// ═══════════════════════════════════════════════════════════════════════════
//
// The key conservation law (Haskell RUPD spec):
//   r_pot         = delta_r1 + fees
//   r_pot         = reward_pot_after_treasury + delta_t1
//   total_dist    + undistributed == reward_pot_after_treasury
//
// Together: total_dist + undistributed + delta_t1 = delta_r1 + fees
//
// Era coverage through the `decentralization` parameter:
//   d  > 0.8  →  federated (Shelley early): eta = 1, full expansion
//   0 < d < 0.8  →  transitional: eta = min(1, actual/expected)
//   d = 0    →  Babbage/Conway: eta = min(1, actual/expected) with full decentralisation

proptest! {
    // The reward pot is always correctly split: distributed + undistributed == pot_after_treasury
    #[test]
    fn prop_reward_pot_splits_correctly(
        pp        in arb_protocol_params(),
        reserves  in 1u64..MAX_LOVELACE_SUPPLY,
        fees      in 0u64..1_000_000_000_000u64,
        snapshot  in arb_single_pool_snapshot(),
    ) {
        let state = make_state(pp, reserves);
        let upd = state.calculate_rewards(&snapshot, Lovelace(fees), &state.protocol_params.clone());

        prop_assert_eq!(
            upd.total_distributed.saturating_add(upd.undistributed),
            upd.reward_pot_after_treasury,
            "distributed={} + undistributed={} != pot_after_treasury={}",
            upd.total_distributed, upd.undistributed, upd.reward_pot_after_treasury
        );
    }

    // r_pot == delta_r1 + fees  (expansion plus fees equals total available before treasury cut)
    #[test]
    fn prop_reward_r_pot_equals_expansion_plus_fees(
        pp       in arb_protocol_params(),
        reserves in 1u64..MAX_LOVELACE_SUPPLY,
        fees     in 0u64..1_000_000_000_000u64,
        snapshot in arb_single_pool_snapshot(),
    ) {
        let state = make_state(pp, reserves);
        let upd = state.calculate_rewards(&snapshot, Lovelace(fees), &state.protocol_params.clone());

        prop_assert_eq!(
            upd.r_pot,
            upd.delta_r1.saturating_add(fees),
            "r_pot={} != delta_r1={} + fees={}",
            upd.r_pot, upd.delta_r1, fees
        );
    }

    // r_pot == reward_pot_after_treasury + delta_t1  (treasury cut + rest == total)
    #[test]
    fn prop_reward_treasury_split(
        pp       in arb_protocol_params(),
        reserves in 1u64..MAX_LOVELACE_SUPPLY,
        fees     in 0u64..1_000_000_000_000u64,
        snapshot in arb_single_pool_snapshot(),
    ) {
        let state = make_state(pp, reserves);
        let upd = state.calculate_rewards(&snapshot, Lovelace(fees), &state.protocol_params.clone());

        prop_assert_eq!(
            upd.r_pot,
            upd.reward_pot_after_treasury.saturating_add(upd.delta_t1),
            "r_pot={} != pot_after_treasury={} + delta_t1={}",
            upd.r_pot, upd.reward_pot_after_treasury, upd.delta_t1
        );
    }

    // Monetary expansion never exceeds reserves (rho <= 1, eta <= 1)
    #[test]
    fn prop_expansion_bounded_by_reserves(
        pp       in arb_protocol_params(),
        reserves in 1u64..MAX_LOVELACE_SUPPLY,
        fees     in 0u64..1_000_000_000_000u64,
        snapshot in arb_single_pool_snapshot(),
    ) {
        let rho = pp.rho;
        let d = pp.decentralization;
        let state = make_state(pp, reserves);
        let upd = state.calculate_rewards(&snapshot, Lovelace(fees), &state.protocol_params.clone());

        prop_assert!(
            upd.delta_r1 <= reserves,
            "delta_r1={} > reserves={} (rho={}/{}, d={}/{})",
            upd.delta_r1, reserves,
            rho.numerator, rho.denominator,
            d.numerator, d.denominator
        );
    }

    // Treasury cut never exceeds total rewards available (tau <= 1)
    #[test]
    fn prop_treasury_cut_bounded_by_r_pot(
        pp       in arb_protocol_params(),
        reserves in 1u64..MAX_LOVELACE_SUPPLY,
        fees     in 0u64..1_000_000_000_000u64,
        snapshot in arb_single_pool_snapshot(),
    ) {
        let tau = pp.tau;
        let state = make_state(pp, reserves);
        let upd = state.calculate_rewards(&snapshot, Lovelace(fees), &state.protocol_params.clone());

        prop_assert!(
            upd.delta_t1 <= upd.r_pot,
            "delta_t1={} > r_pot={} (tau={}/{})",
            upd.delta_t1, upd.r_pot,
            tau.numerator, tau.denominator
        );
    }

    // A pool that produced 0 blocks distributes 0 rewards, regardless of fees or parameters.
    //
    // Covers Babbage/Conway (d=0) where eta depends on actual blocks.  When the pool
    // has no blocks in the snapshot, pool_reward = 0 for that pool, so no member or
    // operator rewards are generated from it.  With exactly one pool, total_distributed = 0.
    #[test]
    fn prop_zero_blocks_zero_distributed(
        rho  in arb_unit_rational(),
        tau  in arb_unit_rational(),
        a0   in arb_unit_rational(),
        n_opt in 1u64..=500u64,
        // d < 0.8 so eta is NOT forced to 1.0
        d_num in 0u64..=79u64,
        reserves in 1u64..MAX_LOVELACE_SUPPLY,
        pool_id   in arb_hash28(),
        owner     in arb_hash28(),
        pool_stake in 1u64..=1_000_000_000_000_000u64,
    ) {
        let mut pp = ProtocolParameters::default();
        pp.rho = rho;
        pp.tau = tau;
        pp.a0 = a0;
        pp.n_opt = n_opt;
        pp.decentralization = Rational { numerator: d_num, denominator: 100 };

        let state = make_state(pp, reserves);

        // Build a snapshot where this pool has 0 blocks
        let owner_cred = owner.to_hash32_padded();
        let delegator_cred: Hash32 = { let mut h = [0u8; 32]; h[..28].copy_from_slice(&pool_id); h[0] ^= 0xAB; h };
        let mut reward_account = vec![0xE1u8];
        reward_account.extend_from_slice(&owner[..]);

        let pool_reg = PoolRegistration {
            pool_id,
            vrf_keyhash: [0u8; 32],
            pledge: Lovelace(0),
            cost: Lovelace(0),
            margin_numerator: 0,
            margin_denominator: 1,
            reward_account,
            owners: vec![owner],
            relays: vec![],
            metadata_url: None,
            metadata_hash: None,
        };

        let half = pool_stake / 2;
        let mut delegations = HashMap::new();
        delegations.insert(owner_cred, pool_id);
        delegations.insert(delegator_cred, pool_id);
        let mut pool_stake_map = HashMap::new();
        pool_stake_map.insert(pool_id, Lovelace(pool_stake));
        let mut pool_params_map = HashMap::new();
        pool_params_map.insert(pool_id, pool_reg);
        let mut stake_dist = HashMap::new();
        stake_dist.insert(owner_cred, Lovelace(half));
        stake_dist.insert(delegator_cred, Lovelace(pool_stake - half));

        let snapshot = StakeSnapshot {
            epoch: EpochNo(0),
            delegations: Arc::new(delegations),
            pool_stake: pool_stake_map,
            pool_params: Arc::new(pool_params_map),
            stake_distribution: Arc::new(stake_dist),
            epoch_blocks_by_pool: Arc::new(HashMap::new()), // 0 blocks
        };

        let upd = state.calculate_rewards(&snapshot, Lovelace(0), &state.protocol_params.clone());
        prop_assert_eq!(
            upd.total_distributed, 0,
            "expected 0 distributed (0 blocks, 0 fees), got {} (expansion={})",
            upd.total_distributed, upd.delta_r1
        );
    }

    // A pool whose owner-delegated stake < declared pledge gets 0 reward.
    #[test]
    fn prop_pledge_failure_gives_zero_reward(
        pp       in arb_protocol_params(),
        reserves in 1u64..MAX_LOVELACE_SUPPLY,
        pool_id  in arb_hash28(),
        owner    in arb_hash28(),
        pledge   in 1u64..=1_000_000_000_000u64,
        blocks   in 1u64..=10_000u64,
    ) {
        let state = make_state(pp, reserves);

        let owner_cred = owner.to_hash32_padded();
        let mut reward_account = vec![0xE1u8];
        reward_account.extend_from_slice(&owner[..]);

        let pool_reg = PoolRegistration {
            pool_id,
            vrf_keyhash: [0u8; 32],
            pledge: Lovelace(pledge),
            cost: Lovelace(0),
            margin_numerator: 0,
            margin_denominator: 1,
            reward_account,
            owners: vec![owner],
            relays: vec![],
            metadata_url: None,
            metadata_hash: None,
        };

        // Owner contributes 0 stake — pledge check will fail regardless of pledge value
        let mut delegations = HashMap::new();
        delegations.insert(owner_cred, pool_id);
        let mut pool_stake_map = HashMap::new();
        pool_stake_map.insert(pool_id, Lovelace(pledge));
        let mut pool_params_map = HashMap::new();
        pool_params_map.insert(pool_id, pool_reg);
        let mut stake_dist = HashMap::new();
        stake_dist.insert(owner_cred, Lovelace(0)); // owner has 0 stake → pledge fails

        let mut blocks_map = HashMap::new();
        blocks_map.insert(pool_id, blocks); // pool produced blocks but pledge fails

        let snapshot = StakeSnapshot {
            epoch: EpochNo(0),
            delegations: Arc::new(delegations),
            pool_stake: pool_stake_map,
            pool_params: Arc::new(pool_params_map),
            stake_distribution: Arc::new(stake_dist),
            epoch_blocks_by_pool: Arc::new(blocks_map),
        };

        let upd = state.calculate_rewards(&snapshot, Lovelace(0), &state.protocol_params.clone());
        prop_assert_eq!(
            upd.total_distributed, 0,
            "pool should get 0 reward when pledge fails (owner_stake=0 < pledge={}), got {}",
            pledge, upd.total_distributed
        );
    }

    // Federated phase (d >= 0.8): expansion = floor(rho * reserves), independent of blocks.
    // Any two calls with the same reserves/params but different blocks should give the same
    // delta_r1 (when d >= 0.8, eta is fixed at 1.0).
    #[test]
    fn prop_federated_expansion_independent_of_blocks(
        rho  in arb_unit_rational(),
        tau  in arb_unit_rational(),
        a0   in arb_unit_rational(),
        n_opt in 1u64..=500u64,
        // d >= 0.8 (federated)
        d_num in 80u64..=100u64,
        reserves  in 1u64..MAX_LOVELACE_SUPPLY,
        blocks_a  in 0u64..=10_000u64,
        blocks_b  in 0u64..=10_000u64,
        pool_id   in arb_hash28(),
    ) {
        let mut pp = ProtocolParameters::default();
        pp.rho = rho;
        pp.tau = tau;
        pp.a0 = a0;
        pp.n_opt = n_opt;
        pp.decentralization = Rational { numerator: d_num, denominator: 100 };

        let state = make_state(pp, reserves);

        let make_snapshot = |blocks: u64| -> StakeSnapshot {
            let mut blocks_map = HashMap::new();
            if blocks > 0 { blocks_map.insert(pool_id, blocks); }
            StakeSnapshot {
                epoch: EpochNo(0),
                delegations: Arc::new(HashMap::new()),
                pool_stake: HashMap::new(),
                pool_params: Arc::new(HashMap::new()),
                stake_distribution: Arc::new(HashMap::new()),
                epoch_blocks_by_pool: Arc::new(blocks_map),
            }
        };

        let upd_a = state.calculate_rewards(&make_snapshot(blocks_a), Lovelace(0), &state.protocol_params.clone());
        let upd_b = state.calculate_rewards(&make_snapshot(blocks_b), Lovelace(0), &state.protocol_params.clone());

        prop_assert_eq!(
            upd_a.delta_r1, upd_b.delta_r1,
            "federated phase: delta_r1 should not depend on blocks \
             (blocks_a={}, blocks_b={}, d={}/100, rho={}/{})",
            blocks_a, blocks_b, d_num, rho.numerator, rho.denominator
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 3 — Epoch transition invariants
// ═══════════════════════════════════════════════════════════════════════════

/// Build a minimal snapshot with a given epoch tag (used only for identity checks).
fn make_stub_snapshot(epoch: u64) -> StakeSnapshot {
    StakeSnapshot {
        epoch: EpochNo(epoch),
        delegations: Arc::new(HashMap::new()),
        pool_stake: HashMap::new(),
        pool_params: Arc::new(HashMap::new()),
        stake_distribution: Arc::new(HashMap::new()),
        epoch_blocks_by_pool: Arc::new(HashMap::new()),
    }
}

proptest! {
    // After N epoch transitions from epoch E, the current epoch is E + N.
    #[test]
    fn prop_epoch_counter_increments(
        start_epoch in 0u64..=100u64,
        n_transitions in 1usize..=10usize,
    ) {
        let mut state = LedgerState::new(ProtocolParameters::default());
        state.epoch = EpochNo(start_epoch);
        state.epoch_length = 86_400;

        for i in 0..n_transitions {
            state.process_epoch_transition(EpochNo(start_epoch + 1 + i as u64));
        }

        prop_assert_eq!(
            state.epoch.0,
            start_epoch + n_transitions as u64,
            "after {} transitions from epoch {}, expected epoch {} but got {}",
            n_transitions, start_epoch,
            start_epoch + n_transitions as u64, state.epoch.0
        );
    }

    // Snapshot rotation: after a transition the old mark becomes the new set,
    // and the old set becomes the new go.  We track this via the snapshot epoch numbers.
    #[test]
    fn prop_snapshot_rotation(
        start_epoch in 5u64..=100u64,
        mark_epoch  in 0u64..=4u64,
        set_epoch   in 0u64..=4u64,
        go_epoch    in 0u64..=4u64,
    ) {
        let mut state = LedgerState::new(ProtocolParameters::default());
        state.epoch = EpochNo(start_epoch);
        state.epoch_length = 86_400;

        state.snapshots.mark = Some(make_stub_snapshot(mark_epoch));
        state.snapshots.set  = Some(make_stub_snapshot(set_epoch));
        state.snapshots.go   = Some(make_stub_snapshot(go_epoch));

        state.process_epoch_transition(EpochNo(start_epoch + 1));

        // Old mark → new set
        let new_set_epoch = state.snapshots.set.as_ref().map(|s| s.epoch.0);
        prop_assert_eq!(
            new_set_epoch, Some(mark_epoch),
            "new set epoch {:?} != old mark epoch {}", new_set_epoch, mark_epoch
        );

        // Old set → new go
        let new_go_epoch = state.snapshots.go.as_ref().map(|s| s.epoch.0);
        prop_assert_eq!(
            new_go_epoch, Some(set_epoch),
            "new go epoch {:?} != old set epoch {}", new_go_epoch, set_epoch
        );
    }

    // After an epoch transition the treasury + reserves can only decrease or stay the
    // same relative to (treasury + reserves + fees) before the transition.
    // (Rewards move value from reserves into reward_accounts; treasury grows by the tau cut.)
    #[test]
    fn prop_treasury_plus_reserves_never_grows(
        rho in arb_unit_rational(),
        tau in arb_unit_rational(),
        treasury_initial in 0u64..=1_000_000_000_000_000u64,
        reserves_initial in 0u64..=14_000_000_000_000_000u64,
        fees in 0u64..=1_000_000_000_000u64,
    ) {
        let mut pp = ProtocolParameters::default();
        pp.rho = rho;
        pp.tau = tau;
        // Use d > 0.8 so eta = 1.0 (simplest case: no block-counting path)
        pp.decentralization = Rational { numerator: 90, denominator: 100 };

        let mut state = LedgerState::new(pp);
        state.epoch = EpochNo(0);
        state.epoch_length = 86_400;
        state.treasury = Lovelace(treasury_initial);
        state.reserves = Lovelace(reserves_initial);
        // Pre-load fees into the snapshot accumulator the way the real node does
        state.snapshots.current_epoch_fees = Lovelace(fees);

        let before = treasury_initial.saturating_add(reserves_initial);
        state.process_epoch_transition(EpochNo(1));
        let after = state.treasury.0.saturating_add(state.reserves.0);

        // Net change in treasury+reserves = fees - registered_rewards_distributed.
        // Fees that are not distributed to stake holders flow into reserves.
        // Therefore: after <= before + fees  (fees represent value entering the system
        // from the epoch-fee accumulator, which sits outside treasury+reserves).
        prop_assert!(
            after <= before.saturating_add(fees),
            "treasury+reserves grew by more than fees: before={} fees={} after={} excess={}",
            before, fees, after, after.saturating_sub(before.saturating_add(fees))
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Section 4 — Deposit tracker
// ═══════════════════════════════════════════════════════════════════════════

proptest! {
    // add_deposit then refund_deposit is a round-trip: the balance afterwards
    // equals the balance beforehand (both for total and staking stake).
    #[test]
    fn prop_deposit_add_refund_roundtrip(
        cred   in arb_hash32(),
        pool   in 0u64..=500_000_000_000u64,
        stake  in 0u64..=2_000_000_000u64,
        drep   in 0u64..=500_000_000_000u64,
    ) {
        let mut tracker = DepositTracker::new();

        // Add all four deposit types
        if pool > 0 {
            tracker.add_deposit(cred, DepositType::Pool, Lovelace(pool));
        }
        if stake > 0 {
            tracker.add_deposit(cred, DepositType::Stake, Lovelace(stake));
        }
        let gov_id = GovActionId { tx_hash: [0xAAu8; 32], index: 0 };
        let gov_amount = 100_000_000_000u64;
        tracker.add_deposit(cred, DepositType::Governance(gov_id), Lovelace(gov_amount));
        if drep > 0 {
            tracker.add_deposit(cred, DepositType::DRep, Lovelace(drep));
        }

        let total_before = tracker.get_total_deposits(&cred).0;
        let staking_before = tracker.get_staking_stake(&cred).0;

        // Refund the governance deposit only
        let refunded = tracker.refund_deposit(&cred, DepositType::Governance(gov_id));
        prop_assert_eq!(refunded, Some(Lovelace(gov_amount)), "wrong refund amount");

        let total_after  = tracker.get_total_deposits(&cred).0;
        let staking_after = tracker.get_staking_stake(&cred).0;

        // Total decreases by exactly gov_amount
        prop_assert_eq!(
            total_before.saturating_sub(total_after), gov_amount,
            "total did not decrease by governance deposit amount"
        );
        // Staking stake is unchanged (gov deposits are voting-only)
        prop_assert_eq!(
            staking_before, staking_after,
            "staking stake changed after refunding a governance deposit"
        );
    }

    // staking_stake <= voting_stake for any deposit combination.
    // (Governance and DRep deposits count for voting but not staking.)
    #[test]
    fn prop_staking_stake_leq_voting_stake(
        cred  in arb_hash32(),
        pool  in 0u64..=500_000_000_000u64,
        stake in 0u64..=2_000_000_000u64,
        gov   in 0u64..=100_000_000_000u64,
        drep  in 0u64..=500_000_000_000u64,
    ) {
        let mut tracker = DepositTracker::new();

        if pool > 0  { tracker.add_deposit(cred, DepositType::Pool,  Lovelace(pool));  }
        if stake > 0 { tracker.add_deposit(cred, DepositType::Stake, Lovelace(stake)); }
        if gov > 0 {
            let id = GovActionId { tx_hash: [0xBBu8; 32], index: 0 };
            tracker.add_deposit(cred, DepositType::Governance(id), Lovelace(gov));
        }
        if drep > 0  { tracker.add_deposit(cred, DepositType::DRep,  Lovelace(drep));  }

        let voting  = tracker.get_voting_stake(&cred).0;
        let staking = tracker.get_staking_stake(&cred).0;

        prop_assert!(
            staking <= voting,
            "staking_stake={} > voting_stake={} (pool={}, stake={}, gov={}, drep={})",
            staking, voting, pool, stake, gov, drep
        );
    }

    // The sum of all individual deposit fields equals get_total_deposits().
    #[test]
    fn prop_total_deposits_equals_sum(
        cred  in arb_hash32(),
        pool  in 0u64..=500_000_000_000u64,
        stake in 0u64..=2_000_000_000u64,
        gov1  in 0u64..=50_000_000_000u64,
        gov2  in 0u64..=50_000_000_000u64,
        drep  in 0u64..=500_000_000_000u64,
    ) {
        let mut tracker = DepositTracker::new();

        if pool  > 0 { tracker.add_deposit(cred, DepositType::Pool,  Lovelace(pool));  }
        if stake > 0 { tracker.add_deposit(cred, DepositType::Stake, Lovelace(stake)); }
        if gov1  > 0 {
            let id = GovActionId { tx_hash: [0xCCu8; 32], index: 0 };
            tracker.add_deposit(cred, DepositType::Governance(id), Lovelace(gov1));
        }
        if gov2  > 0 {
            let id = GovActionId { tx_hash: [0xCCu8; 32], index: 1 };
            tracker.add_deposit(cred, DepositType::Governance(id), Lovelace(gov2));
        }
        if drep  > 0 { tracker.add_deposit(cred, DepositType::DRep,  Lovelace(drep));  }

        let expected = pool.saturating_add(stake)
            .saturating_add(gov1)
            .saturating_add(gov2)
            .saturating_add(drep);
        let actual = tracker.get_total_deposits(&cred).0;

        prop_assert_eq!(actual, expected,
            "total_deposits mismatch: actual={} expected={}", actual, expected);
    }

    // Deposits for different credentials are independent: adding a deposit for
    // credential A does not change the balance for credential B.
    #[test]
    fn prop_deposits_per_credential_are_independent(
        cred_a in arb_hash32(),
        cred_b in arb_hash32(),
        amount in 1u64..=500_000_000_000u64,
    ) {
        prop_assume!(cred_a != cred_b);

        let mut tracker = DepositTracker::new();
        let before_b = tracker.get_total_deposits(&cred_b).0;

        tracker.add_deposit(cred_a, DepositType::Pool, Lovelace(amount));

        let after_b = tracker.get_total_deposits(&cred_b).0;
        prop_assert_eq!(before_b, after_b,
            "deposit for cred_a changed balance for cred_b");
    }
}
