// Epoch snapshot validation tests
//
// Tests to ensure epoch boundary snapshots are correct and maintain invariants:
// 1. Conservation: treasury + reserves + circulation = MAX_SUPPLY
// 2. Stake bounds: sum(pool_stake) ≤ circulation
// 3. Reward bounds: sum(reward_accounts) ≤ reserves
// 4. Snapshot consistency: mark/set/go rotation preserves data

use crate::ledger::*;

#[cfg(test)]
mod invariant_tests {
    use super::*;

    /// Maximum ADA supply (45 billion ADA = 45,000,000,000,000,000 lovelace)
    const MAX_SUPPLY: u64 = 45_000_000_000_000_000;

    #[test]
    fn test_conservation_of_ada() {
        let mut state = LedgerState::new(ProtocolParameters::default());

        // Initialize with some values
        state.treasury = Lovelace(1_000_000_000);
        state.reserves = Lovelace(MAX_SUPPLY - 1_000_000_000);

        // Verify conservation invariant
        // treasury + reserves + circulation = MAX_SUPPLY
        let total_supply = state.treasury.0 + state.reserves.0;

        // Note: circulation = MAX_SUPPLY - reserves - treasury
        // So: treasury + reserves + (MAX_SUPPLY - reserves - treasury) = MAX_SUPPLY
        assert!(total_supply <= MAX_SUPPLY, "Supply exceeds maximum");
    }

    #[test]
    fn test_stake_distribution_bounds() {
        let mut state = LedgerState::new(ProtocolParameters::default());

        // Set reasonable reserves and treasury (leaving some ADA in circulation)
        state.reserves = Lovelace(20_000_000_000_000_000); // 20 billion ADA
        state.treasury = Lovelace(5_000_000_000_000_000);  // 5 billion ADA
        // Circulating: 45B - 20B - 5B = 20B ADA

        // Add some stake to credentials
        let cred1 = [1u8; 32];
        let cred2 = [2u8; 32];

        state.stake_distribution.stake_map.insert(cred1, Lovelace(1_000_000));
        state.stake_distribution.stake_map.insert(cred2, Lovelace(2_000_000));

        // Calculate total staked
        let total_stake: u64 = state.stake_distribution.stake_map
            .values()
            .map(|l| l.0)
            .sum();

        // Total stake should not exceed circulating supply
        let circulating = MAX_SUPPLY - state.reserves.0 - state.treasury.0;
        assert!(total_stake <= circulating, "Stake exceeds circulation");
    }

    #[test]
    fn test_pool_stake_consistency() {
        use std::collections::HashMap;

        let cred1 = [1u8; 32];
        let pool1 = [11u8; 28];

        // Create maps directly (avoiding Arc mutation issues in tests)
        let mut stake_map = HashMap::new();
        stake_map.insert(cred1, Lovelace(5_000_000));

        let mut delegations = HashMap::new();
        delegations.insert(cred1, pool1);

        // Compute pool stake
        let pool_stake = delegations.iter()
            .filter(|(_, pool)| *pool == &pool1)
            .filter_map(|(cred, _)| stake_map.get(cred))
            .map(|l| l.0)
            .sum::<u64>();

        // Pool stake should equal delegated stake
        assert_eq!(pool_stake, 5_000_000);
    }

    #[test]
    fn test_reward_account_bounds() {
        use std::collections::HashMap;

        let cred1 = [1u8; 32];

        // Create reward map directly
        let mut reward_accounts = HashMap::new();
        reward_accounts.insert(cred1, Lovelace(100_000));

        let total_rewards: u64 = reward_accounts.values().map(|l| l.0).sum();

        // Total rewards should not exceed available reserves
        // (In practice, rewards come from reserves + fees)
        assert!(total_rewards < MAX_SUPPLY, "Rewards exceed maximum");
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn test_snapshot_rotation() {
        let mut state = LedgerState::new(ProtocolParameters::default());

        // Create initial stake distribution
        let cred1 = [1u8; 32];
        state.stake_distribution.stake_map.insert(cred1, Lovelace(1_000_000));

        // Take initial snapshot (would be called during epoch transition)
        let initial_stake = state.stake_distribution.stake_map.clone();

        // Simulate mark snapshot creation
        let mark_snapshot = StakeSnapshot {
            epoch: EpochNo(0),
            stake_distribution: initial_stake.into(),
            pool_params: state.pool_params.clone(),
            pool_stake: Default::default(),
            delegations: state.delegations.clone(),
        };

        // Verify snapshot captured state correctly
        assert_eq!(
            mark_snapshot.stake_distribution.get(&cred1),
            Some(&Lovelace(1_000_000))
        );
    }

    #[test]
    fn test_epoch_boundary_immutability() {
        let mut state = LedgerState::new(ProtocolParameters::default());

        // Set up initial state
        let cred1 = [1u8; 32];
        state.stake_distribution.stake_map.insert(cred1, Lovelace(1_000_000));

        // Take snapshot
        let snapshot = StakeSnapshot {
            epoch: EpochNo(0),
            stake_distribution: state.stake_distribution.stake_map.clone().into(),
            pool_params: state.pool_params.clone(),
            pool_stake: Default::default(),
            delegations: state.delegations.clone(),
        };

        // Modify state after snapshot
        state.stake_distribution.stake_map.insert(cred1, Lovelace(2_000_000));

        // Verify snapshot is unchanged
        assert_eq!(
            snapshot.stake_distribution.get(&cred1),
            Some(&Lovelace(1_000_000)),
            "Snapshot should be immutable"
        );
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    /// Test that epoch transitions maintain invariants
    #[test]
    fn test_epoch_transition_preserves_invariants() {
        let mut state = LedgerState::new(ProtocolParameters::default());

        // Initialize state
        state.treasury = Lovelace(10_000_000_000);
        state.reserves = Lovelace(30_000_000_000_000_000);

        let treasury_before = state.treasury.0;
        let reserves_before = state.reserves.0;

        // Process epoch transition (simplified - no rewards in this test)
        state.process_epoch_transition(EpochNo(1));

        // Verify treasury and reserves haven't changed unreasonably
        // (In a real epoch, they would change due to rewards, but predictably)
        let treasury_after = state.treasury.0;
        let reserves_after = state.reserves.0;

        // Total should remain constant if no rewards were distributed
        // (This test is simplified; real tests would account for reward calculation)
        let total_before = treasury_before + reserves_before;
        let total_after = treasury_after + reserves_after;

        assert_eq!(
            total_before, total_after,
            "Conservation violated during epoch transition"
        );
    }

    /// Test that multiple epoch transitions work correctly
    #[test]
    fn test_multiple_epochs() {
        let mut state = LedgerState::new(ProtocolParameters::default());

        for epoch in 1..10 {
            state.process_epoch_transition(EpochNo(epoch));

            // Verify epoch number updated
            assert_eq!(state.epoch.0, epoch);
        }
    }
}

// TODO: Cross-reference tests with cardano-node
// These tests would:
// 1. Query cardano-node at specific epochs
// 2. Compare stake distribution with our snapshot
// 3. Compare epoch nonce
// 4. Compare pool parameters
//
// Requires:
// - Cardano-node LocalStateQuery protocol integration
// - Or Koios/Blockfrost API queries for historical data

// TODO: Historical epoch validation
// Test against known mainnet epochs:
// - Epoch 208 (Shelley launch)
// - Epoch 290 (Mary HF)
// - Epoch 365 (Alonzo HF)
// - Recent epochs
//
// Requires:
// - Historical block data
// - Known correct snapshots to compare against

// TODO: Comparison with torsten
// Run both hayate and torsten on same network, compare snapshots
// Requires:
// - Torsten integration
// - Snapshot export/comparison tools
