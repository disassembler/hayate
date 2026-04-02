// Reward calculation logic
//
// Copied from torsten-ledger/src/state/rewards.rs
// Implements the Cardano reward formula matching cardano-ledger-shelley

use super::primitives::*;
use super::rational::Rat;
use super::state::{LedgerState, PendingRewardUpdate, StakeSnapshot, MAX_LOVELACE_SUPPLY};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

impl LedgerState {
    /// Apply a pending reward update to the ledger state.
    ///
    /// This is called at the BEGINNING of an epoch transition to apply rewards
    /// computed during the previous epoch transition, matching Haskell's RUPD
    /// deferred application pattern.
    pub fn apply_pending_reward_update(&mut self) {
        if let Some(rupd) = self.pending_reward_update.take() {
            // Reserves: subtract expansion (deltaR1), add back undistributed (deltaR2).
            self.reserves.0 = self.reserves.0.saturating_sub(rupd.delta_reserves);
            self.reserves.0 = self.reserves.0.saturating_add(rupd.undistributed);

            // Apply per-account rewards; rewards for deregistered accounts go to treasury (unregRU').
            let mut unreg_treasury = 0u64;
            for (cred_hash, reward) in &rupd.rewards {
                if reward.0 > 0 {
                    if let Some(balance) =
                        Arc::make_mut(&mut self.reward_accounts).get_mut(cred_hash)
                    {
                        *balance += *reward;
                    } else {
                        unreg_treasury += reward.0;
                    }
                }
            }

            // Treasury: tau cut (deltaT1) + unregRU' (accounts deregistered at application).
            // Haskell: treasury_5 = treasury_4 + Δt₁ + unregRU'
            self.treasury.0 = self
                .treasury
                .0
                .saturating_add(rupd.delta_treasury)
                .saturating_add(unreg_treasury);
        }
    }

    /// Calculate rewards and return a PendingRewardUpdate for deferred application.
    ///
    /// Implements the formula from cardano-ledger-shelley:
    ///   - maxPool'(a0, nOpt, R, sigma, p) for pledge-influenced pool rewards
    ///   - mkApparentPerformance for beta/sigma performance calculation
    ///   - Pledge verification (pool gets zero if owner stake < declared pledge)
    ///   - Operator reward includes self-delegation share (margin + proportional)
    ///   - Operator reward goes to pool's registered reward account
    ///
    /// Returns a `PendingRewardUpdate` that should be stored and applied at the
    /// NEXT epoch boundary, matching Haskell's RUPD timing.
    pub fn calculate_rewards(
        &self,
        go_snapshot: &StakeSnapshot,
        fees: Lovelace,
        pp: &ProtocolParameters,
    ) -> PendingRewardUpdate {
        let rho_num = pp.rho.numerator as i128;
        let rho_den = pp.rho.denominator.max(1) as i128;
        let tau_num = pp.tau.numerator as i128;
        let tau_den = pp.tau.denominator.max(1) as i128;

        let fees_for_rewards = fees.0;

        // hardforkBabbageForgoRewardPrefilter: pvMajor > 6
        // Pre-Babbage (pv <= 6): filter rewards at createRUpd time — unregistered accounts
        //   are excluded from `rs`; their share stays in deltaR2 → returns to reserves.
        // Post-Babbage (pv >= 7): skip pre-filter — all rewards go into `rs`; unregistered
        //   accounts are caught at applyRUpd time as unregRU' → treasury.
        // Haskell's hardforkBabbageForgoRewardPrefilter uses prevPParamsEpochStateL.pv,
        // i.e. the PRE-PPUP protocol version (pp = prev_pp passed by caller).
        // At epoch N→N+1: prev_pp.pv=6 → prefilter enabled; prev_pp.pv≥7 → prefilter disabled.
        let babbage_forgo_prefilter = pp.protocol_version_major > 6;

        let d_num = pp.decentralization.numerator as i128;
        let d_den = pp.decentralization.denominator.max(1) as i128;
        let d_value = d_num as f64 / d_den as f64;

        let total_stake_pool_blocks: u64 = go_snapshot.epoch_blocks_by_pool.values().sum();

        // Expected blocks: floor((1 - d) * activeSlotCoeff * epochLength)
        let raw_expected_blocks =
            ((1.0 - d_value) * pp.active_slot_coeff() * self.epoch_length as f64).floor()
                as u64;
        let expected_blocks = raw_expected_blocks.max(1);

        // η = 1 when d ≥ 0.8 (federated); η = min(actual, expected) / expected otherwise
        let rho = Rat::from_i128(rho_num, rho_den);
        let (eta, eta_rat) = if d_value >= 0.8 {
            (1.0f64, Rat::from_i128(1, 1))
        } else {
            let effective_blocks = total_stake_pool_blocks.min(expected_blocks);
            let eta_f64 = if expected_blocks == 0 {
                1.0
            } else {
                effective_blocks as f64 / expected_blocks as f64
            };
            let eta_rat = Rat::from_i128(effective_blocks as i128, expected_blocks.max(1) as i128);
            (eta_f64, eta_rat)
        };

        tracing::debug!(
            "RUPD: protocol parameters: ρ = {}/{}, τ = {}/{}, d = {}/{}",
            rho_num,
            rho_den,
            tau_num,
            tau_den,
            d_num,
            d_den
        );
        tracing::debug!(
            "RUPD: performance: n = pay_blocks / expected_blocks = {}/{}",
            total_stake_pool_blocks,
            expected_blocks
        );

        let expansion = eta_rat
            .mul(&rho)
            .mul(&Rat::from_i128(self.reserves.0 as i128, 1))
            .floor_u64();
        let total_rewards_available = expansion + fees_for_rewards;
        tracing::debug!(
            "RUPD: deltaR1 = floor(min(1,η) · ρ · reserves) = {}",
            expansion
        );
        tracing::debug!("RUPD: rPot = fees + deltaR1 = {}", total_rewards_available);

        if total_rewards_available == 0 {
            return PendingRewardUpdate {
                rewards: HashMap::new(),
                delta_treasury: 0,
                delta_reserves: 0,
                undistributed: 0,
                eta,
                delta_r1: expansion,
                r_pot: total_rewards_available,
                delta_t1: 0,
                reward_pot_after_treasury: 0,
                total_distributed: 0,
            };
        }

        // Treasury cut: floor(tau * total_rewards)
        let tau = Rat::from_i128(tau_num, tau_den);
        let treasury_cut = tau
            .mul(&Rat::from_i128(total_rewards_available as i128, 1))
            .floor_u64();

        let reward_pot = total_rewards_available - treasury_cut;
        tracing::debug!("RUPD: deltaT1 = floor(τ · rPot) = {}", treasury_cut);

        // Total stake = circulation = maxSupply - reserves (per Haskell PulsingReward.hs)
        let total_stake = MAX_LOVELACE_SUPPLY.saturating_sub(self.reserves.0);
        if total_stake == 0 {
            return PendingRewardUpdate {
                delta_reserves: expansion,
                delta_treasury: treasury_cut,
                rewards: HashMap::new(),
                undistributed: reward_pot,
                eta,
                delta_r1: expansion,
                r_pot: total_rewards_available,
                delta_t1: treasury_cut,
                reward_pot_after_treasury: reward_pot,
                total_distributed: 0,
            };
        }

        // Total active stake (for apparent performance denominator only)
        let total_active_stake: u64 = go_snapshot
            .pool_stake
            .values()
            .fold(0u64, |acc, s| acc.saturating_add(s.0));
        if total_active_stake == 0 {
            return PendingRewardUpdate {
                delta_reserves: expansion,
                delta_treasury: treasury_cut,
                rewards: HashMap::new(),
                undistributed: reward_pot,
                eta,
                delta_r1: expansion,
                r_pot: total_rewards_available,
                delta_t1: treasury_cut,
                reward_pot_after_treasury: reward_pot,
                total_distributed: 0,
            };
        }

        let total_blocks_for_performance = total_stake_pool_blocks.max(1);
        let n_opt = pp.n_opt.max(1);

        let mut total_distributed: u64 = 0;
        let mut reward_map: HashMap<Hash32, Lovelace> = HashMap::new();

        // Build delegators-by-pool index
        let mut delegators_by_pool: HashMap<Hash28, Vec<Hash32>> = HashMap::new();
        for (cred_hash, pool_id) in go_snapshot.delegations.iter() {
            delegators_by_pool
                .entry(*pool_id)
                .or_default()
                .push(*cred_hash);
        }

        // Build owner-delegated-stake per pool for pledge check
        let mut owner_stake_by_pool: HashMap<Hash28, u64> = HashMap::new();
        for (pool_id, pool_reg) in go_snapshot.pool_params.iter() {
            let mut owner_stake = 0u64;
            for owner in &pool_reg.owners {
                let owner_key = owner.to_hash32_padded();
                if go_snapshot.delegations.get(&owner_key) == Some(pool_id) {
                    owner_stake += go_snapshot
                        .stake_distribution
                        .get(&owner_key)
                        .map(|l| l.0)
                        .unwrap_or(0);
                }
            }
            owner_stake_by_pool.insert(*pool_id, owner_stake);
        }

        // Calculate rewards per pool
        for (pool_id, pool_active_stake) in &go_snapshot.pool_stake {
            let pool_reg = match go_snapshot.pool_params.get(pool_id) {
                Some(reg) => reg,
                None => continue,
            };

            // Pledge check: if owner-delegated stake < declared pledge, pool gets zero
            let self_delegated = owner_stake_by_pool.get(pool_id).copied().unwrap_or(0);
            if self_delegated < pool_reg.pledge.0 {
                continue;
            }

            // maxPool'(a0, nOpt, R, sigma, p)
            let a0_r = Rat::from_i128(
                pp.a0.numerator as i128,
                pp.a0.denominator.max(1) as i128,
            );
            let z0 = Rat::from_i128(1, n_opt as i128);
            let sigma_raw = Rat::from_i128(pool_active_stake.0 as i128, total_stake as i128);
            let p_raw = Rat::from_i128(pool_reg.pledge.0 as i128, total_stake as i128);
            let sigma = sigma_raw.min_rat(&z0);
            let p = p_raw.min_rat(&z0);

            let f4 = z0.sub(&sigma).div(&z0);
            let f3 = sigma.sub(&p.mul(&f4)).div(&z0);
            let f2 = sigma.add(&p.mul(&a0_r).mul(&f3));
            let f1 = Rat::from_i128(reward_pot as i128, 1).div(&Rat::from_i128(1, 1).add(&a0_r));
            let max_pool = f1.mul(&f2).floor_u64();

            let blocks_made = go_snapshot
                .epoch_blocks_by_pool
                .get(pool_id)
                .copied()
                .unwrap_or(0);
            let pool_reward = if blocks_made == 0 || pool_active_stake.0 == 0 {
                0u64
            } else {
                let perf =
                    Rat::from_i128(blocks_made as i128, total_blocks_for_performance as i128).mul(
                        &Rat::from_i128(total_active_stake as i128, pool_active_stake.0 as i128),
                    );
                perf.mul(&Rat::from_i128(max_pool as i128, 1)).floor_u64()
            };

            if pool_reward == 0 {
                continue;
            }

            let cost = pool_reg.cost.0;
            let margin_num = pool_reg.margin_numerator as i128;
            let margin_den = pool_reg.margin_denominator.max(1) as i128;

            let operator_reward = if pool_reward <= cost {
                pool_reward
            } else {
                let remainder = pool_reward - cost;
                let margin = Rat::from_i128(margin_num, margin_den);
                let one_minus_margin = Rat::from_i128(margin_den - margin_num, margin_den);
                let s_over_sigma =
                    Rat::from_i128(self_delegated as i128, pool_active_stake.0 as i128);
                let share = margin.add(&one_minus_margin.mul(&s_over_sigma));
                let op_extra = share.mul(&Rat::from_i128(remainder as i128, 1)).floor_u64();
                cost + op_extra
            };

            let owner_set: HashSet<Hash32> = pool_reg
                .owners
                .iter()
                .map(|o| o.to_hash32_padded())
                .collect();

            if let Some(delegators) = delegators_by_pool.get(pool_id) {
                for cred_hash in delegators {
                    if owner_set.contains(cred_hash) {
                        continue;
                    }

                    let member_stake = go_snapshot
                        .stake_distribution
                        .get(cred_hash)
                        .copied()
                        .unwrap_or(Lovelace(0))
                        .0;

                    if member_stake == 0 || pool_active_stake.0 == 0 {
                        continue;
                    }

                    let member_share = if pool_reward <= cost {
                        0u64
                    } else {
                        let remainder = pool_reward - cost;
                        let one_minus_margin = Rat::from_i128(margin_den - margin_num, margin_den);
                        let member_frac =
                            Rat::from_i128(member_stake as i128, pool_active_stake.0 as i128);
                        Rat::from_i128(remainder as i128, 1)
                            .mul(&one_minus_margin)
                            .mul(&member_frac)
                            .floor_u64()
                    };

                    if member_share > 0 {
                        *reward_map.entry(*cred_hash).or_insert(Lovelace(0)) +=
                            Lovelace(member_share);
                        total_distributed += member_share;
                    }
                }
            }

            // Operator reward: gated by hardforkBabbageForgoRewardPrefilter (pvMajor > 6).
            // Pre-Babbage (pv <= 6): only add to rs if the reward account is registered.
            //   Unregistered operator rewards stay in deltaR2 → reserves.
            // Post-Babbage (pv >= 7): always add to rs regardless of registration.
            //   At applyRUpd time, unregistered accounts become unregRU' → treasury.
            if operator_reward > 0 {
                let op_key = Self::reward_account_to_hash(&pool_reg.reward_account);
                if babbage_forgo_prefilter || self.reward_accounts.contains_key(&op_key) {
                    *reward_map.entry(op_key).or_insert(Lovelace(0)) += Lovelace(operator_reward);
                    total_distributed += operator_reward;
                }
            }
        }

        let undistributed = reward_pot.saturating_sub(total_distributed);
        for (cred, reward) in &reward_map {
            tracing::debug!(
                epoch = self.epoch.0,
                cred = hex::encode(&cred[..28]),
                reward = reward.0,
                "RUPD: rs"
            );
        }
        tracing::debug!("RUPD: Σrs = {}", total_distributed);
        tracing::debug!("RUPD: deltaR2 = R - Σrs = {}", undistributed);

        PendingRewardUpdate {
            rewards: reward_map,
            delta_treasury: treasury_cut,
            delta_reserves: expansion,
            undistributed,
            eta,
            delta_r1: expansion,
            r_pot: total_rewards_available,
            delta_t1: treasury_cut,
            reward_pot_after_treasury: reward_pot,
            total_distributed,
        }
    }

    /// Extract stake credential hash from reward account bytes.
    /// Reward accounts are 29 bytes: [header(1)][28-byte hash].
    /// Header high nibble: 0xe = keyHash, 0xf = scriptHash.
    /// Returns Hash32 with byte 28 tagged: 0x00 = keyHash, 0x01 = scriptHash.
    pub(crate) fn reward_account_to_hash(reward_account: &[u8]) -> Hash32 {
        if reward_account.len() >= 29 {
            let mut hash = [0u8; 32];
            hash[..28].copy_from_slice(&reward_account[1..29]);
            // Tag byte 28 based on reward address header
            let header = reward_account[0];
            if (header & 0xf0) == 0xf0 {
                hash[28] = 0x01; // scriptHash
            }
            // keyHash (0xe_) leaves byte 28 as 0x00
            hash
        } else {
            [0u8; 32]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::primitives::Hash28Ext;
    use crate::ledger::state::{PoolRegistration, StakeSnapshot};

    fn hex28(s: &str) -> Hash28 {
        let b = hex::decode(s).unwrap();
        let mut h = [0u8; 28];
        h.copy_from_slice(&b[..28]);
        h
    }

    /// Build the go_snapshot as used for the epoch 12→13 RUPD:
    /// - stake: go snapshot (epoch 9) — 1T each pool
    /// - blocks: mark snapshot (epoch 11) — {8fc7:1432, bf94:1422, d9ea:1511}
    /// Each pool gets a unique reward account (AA/BB/CC) so we can track per-pool rewards.
    fn make_epoch12_reward_snapshot() -> StakeSnapshot {
        let pool_8fc7 = hex28("8fc7a730bba533f2f6f0c4ce0df1783bd002ee9d923ae941728b2830");
        let pool_bf94 = hex28("bf94a435bab1bc756f2f61443cc26d857b2e227903d9f2f5f2e7b686");
        let pool_d9ea = hex28("d9eaf1f9770c8bb35dd18df8d8a0b2b324cdbdeeb38e5e7ceb7ea734");

        let own_b2f1 = hex28("b2f1e813fb3108fe6926717ec6409bc369ae81b485aa2120dc1fa1d7");
        let own_2f5f = hex28("2f5fde105530993a262d744e534847794d477d2cc5a457312b2014b1");
        let own_f535 = hex28("f535d2bfc700116bb07ad9f9a1159d45577e85fc3c472d692780d996");

        let cred_b2f1 = own_b2f1.to_hash32_padded();
        let cred_2f5f = own_2f5f.to_hash32_padded();
        let cred_f535 = own_f535.to_hash32_padded();

        let make_reward_acct = |tag: u8| {
            let mut acct = vec![0xE0u8];
            acct.extend_from_slice(&[tag; 28]);
            acct
        };

        let pool_reg = |pool_id: Hash28, owner: Hash28, reward_account: Vec<u8>| PoolRegistration {
            pool_id,
            vrf_keyhash: [0u8; 32],
            pledge: Lovelace(1_000_000_000_000),
            cost: Lovelace(500_000_000),
            margin_numerator: 1,
            margin_denominator: 1,
            reward_account,
            owners: vec![owner],
            relays: vec![],
            metadata_url: None,
            metadata_hash: None,
        };

        let mut delegations = HashMap::new();
        delegations.insert(cred_b2f1, pool_8fc7);
        delegations.insert(cred_2f5f, pool_bf94);
        delegations.insert(cred_f535, pool_d9ea);

        let mut pool_stake = HashMap::new();
        pool_stake.insert(pool_8fc7, Lovelace(1_000_000_000_000));
        pool_stake.insert(pool_bf94, Lovelace(1_000_000_000_000));
        pool_stake.insert(pool_d9ea, Lovelace(1_000_000_000_000));

        let mut pool_params = HashMap::new();
        pool_params.insert(
            pool_8fc7,
            pool_reg(pool_8fc7, own_b2f1, make_reward_acct(0xAA)),
        );
        pool_params.insert(
            pool_bf94,
            pool_reg(pool_bf94, own_2f5f, make_reward_acct(0xBB)),
        );
        pool_params.insert(
            pool_d9ea,
            pool_reg(pool_d9ea, own_f535, make_reward_acct(0xCC)),
        );

        let mut stake_distribution = HashMap::new();
        stake_distribution.insert(cred_b2f1, Lovelace(1_000_000_000_000));
        stake_distribution.insert(cred_2f5f, Lovelace(1_000_000_000_000));
        stake_distribution.insert(cred_f535, Lovelace(1_000_000_000_000));

        let mut blocks = HashMap::new();
        blocks.insert(pool_8fc7, 1433u64);
        blocks.insert(pool_bf94, 1422u64);
        blocks.insert(pool_d9ea, 1510u64);

        StakeSnapshot {
            epoch: EpochNo(9),
            delegations: Arc::new(delegations),
            pool_stake,
            pool_params: Arc::new(pool_params),
            stake_distribution: Arc::new(stake_distribution),
            epoch_blocks_by_pool: Arc::new(blocks),
            script_stake_credentials: Default::default(),
        }
    }

    #[test]
    fn test_epoch12_rupd_total_distributed() {
        use crate::ledger::primitives::{ProtocolParameters, Rational};
        use crate::ledger::state::LedgerState;

        let mut params = ProtocolParameters::default();
        params.rho = Rational {
            numerator: 3,
            denominator: 1000,
        };
        params.tau = Rational {
            numerator: 1,
            denominator: 5,
        };
        params.a0 = Rational {
            numerator: 3,
            denominator: 10,
        };
        params.n_opt = 500;
        params.decentralization = Rational {
            numerator: 0,
            denominator: 1,
        };
        params.active_slot_coefficient = Rational {
            numerator: 1,
            denominator: 20,
        };

        let mut state = LedgerState::new(params);
        state.epoch = EpochNo(12);
        state.epoch_length = 86400;
        state.reserves = Lovelace(14_901_365_998_049_648);

        let key_8fc7: Hash32 = {
            let mut k = [0xAAu8; 32];
            k[28..].fill(0);
            k
        };
        let key_bf94: Hash32 = {
            let mut k = [0xBBu8; 32];
            k[28..].fill(0);
            k
        };
        let key_d9ea: Hash32 = {
            let mut k = [0xCCu8; 32];
            k[28..].fill(0);
            k
        };
        {
            let ra = std::sync::Arc::make_mut(&mut state.reward_accounts);
            ra.insert(key_8fc7, Lovelace(0));
            ra.insert(key_bf94, Lovelace(0));
            ra.insert(key_d9ea, Lovelace(0));
        }

        let go_snapshot = make_epoch12_reward_snapshot();
        let rupd =
            state.calculate_rewards(&go_snapshot, Lovelace(0), &state.protocol_params.clone());

        let r_8fc7 = rupd.rewards.get(&key_8fc7).map(|l| l.0).unwrap_or(0);
        let r_bf94 = rupd.rewards.get(&key_bf94).map(|l| l.0).unwrap_or(0);
        let r_d9ea = rupd.rewards.get(&key_d9ea).map(|l| l.0).unwrap_or(0);

        println!("=== Epoch 12→13 RUPD ===");
        println!("deltaR1 (expansion):    {}", rupd.delta_r1);
        println!("deltaT1 (treasury):     {}", rupd.delta_t1);
        println!("rewardPot:              {}", rupd.reward_pot_after_treasury);
        println!("totalDistributed:       {}", rupd.total_distributed);
        println!("undistributed (deltaR2):{}", rupd.undistributed);
        println!("eta:                    {}", rupd.eta);
        println!("--- Per-pool rewards (blocks 1433/1422/1510) ---");
        println!("8fc7 (1433 blocks):     {}", r_8fc7);
        println!("bf94 (1422 blocks):     {}", r_bf94);
        println!("d9ea (1510 blocks):     {}", r_d9ea);
        println!("sum:                    {}", r_8fc7 + r_bf94 + r_d9ea);

        assert_eq!(rupd.delta_r1, 44_704_097_994_148, "deltaR1 mismatch");
        assert_eq!(rupd.delta_t1, 8_940_819_598_829, "deltaT1 mismatch");
        assert_eq!(
            rupd.reward_pot_after_treasury, 35_763_278_395_319,
            "rewardPot mismatch"
        );
        assert_eq!(
            rupd.total_distributed, 2_742_233_249,
            "totalDistributed mismatch"
        );
    }

    #[test]
    fn test_reward_account_to_hash() {
        let mut account = vec![0xe1];
        account.extend_from_slice(&[0xaa; 28]);

        let hash = LedgerState::reward_account_to_hash(&account);
        assert_eq!(&hash[..28], &[0xaa; 28]);
        assert_eq!(&hash[28..], &[0, 0, 0, 0]);
    }
}
