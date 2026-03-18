// Certificate processing (ledger state transitions)
//
// Copied from torsten-ledger/src/state/certificates.rs
// Handles all certificate types: stake, pool, governance, MIR

use super::primitives::*;
use super::state::{credential_to_hash, DRepRegistration, LedgerState, PoolRegistration};
use std::sync::Arc;

/// Returns true if the certificate is Conway-only and requires protocol version >= 9.
pub(crate) fn is_conway_only_certificate(cert: &Certificate) -> bool {
    matches!(
        cert,
        Certificate::RegDRep { .. }
            | Certificate::UnregDRep { .. }
            | Certificate::UpdateDRep { .. }
            | Certificate::VoteDelegation { .. }
            | Certificate::StakeVoteDelegation { .. }
            | Certificate::CommitteeHotAuth { .. }
            | Certificate::CommitteeColdResign { .. }
            | Certificate::RegStakeVoteDeleg { .. }
            | Certificate::VoteRegDeleg { .. }
            | Certificate::ConwayStakeRegistration { .. }
            | Certificate::ConwayStakeDeregistration { .. }
            | Certificate::RegStakeDeleg { .. }
    )
}

impl LedgerState {
    /// Process a certificate and update the ledger state accordingly.
    /// Conway-specific certificates are silently skipped if the protocol version < 9.
    pub fn process_certificate(&mut self, cert: &Certificate) {
        if is_conway_only_certificate(cert) && self.protocol_params.protocol_version_major < 9 {
            tracing::warn!(
                "Ignoring Conway-only certificate {:?} in pre-Conway era (protocol version {})",
                std::mem::discriminant(cert),
                self.protocol_params.protocol_version_major,
            );
            return;
        }
        match cert {
            Certificate::StakeRegistration(credential) => {
                let key = credential_to_hash(credential);
                self.stake_distribution
                    .stake_map
                    .entry(key)
                    .or_insert(Lovelace(0));
                Arc::make_mut(&mut self.reward_accounts)
                    .entry(key)
                    .or_insert(Lovelace(0));
                // Track script credentials for correct credential_type in queries
                if matches!(credential, Credential::Script(_)) {
                    self.script_stake_credentials.insert(key);
                }
                tracing::debug!("Stake key registered: {}", hex::encode(key));
            }
            Certificate::StakeDeregistration(credential) => {
                let key = credential_to_hash(credential);
                // Per Shelley ledger spec: deregistration is only valid if reward balance is zero.
                let balance = self
                    .reward_accounts
                    .get(&key)
                    .copied()
                    .unwrap_or(Lovelace(0));
                if balance.0 > 0 {
                    tracing::debug!(
                        "Stake deregistration rejected: non-zero reward balance (key={}, balance={})",
                        hex::encode(key),
                        balance.0,
                    );
                } else {
                    self.stake_distribution.stake_map.remove(&key);
                    Arc::make_mut(&mut self.delegations).remove(&key);
                    Arc::make_mut(&mut self.reward_accounts).remove(&key);
                    self.script_stake_credentials.remove(&key);
                    tracing::debug!("Stake key deregistered: {}", hex::encode(key));
                }
            }
            Certificate::ConwayStakeRegistration {
                credential,
                deposit: _,
            } => {
                // Conway cert tag 7: same behavior as StakeRegistration
                let key = credential_to_hash(credential);
                self.stake_distribution
                    .stake_map
                    .entry(key)
                    .or_insert(Lovelace(0));
                Arc::make_mut(&mut self.reward_accounts)
                    .entry(key)
                    .or_insert(Lovelace(0));
                if matches!(credential, Credential::Script(_)) {
                    self.script_stake_credentials.insert(key);
                }
                tracing::debug!("Stake key registered (Conway): {}", hex::encode(key));
            }
            Certificate::ConwayStakeDeregistration {
                credential,
                refund: _,
            } => {
                // Conway cert tag 8: deregistration returns remaining reward balance
                let key = credential_to_hash(credential);
                self.stake_distribution.stake_map.remove(&key);
                Arc::make_mut(&mut self.delegations).remove(&key);
                Arc::make_mut(&mut self.reward_accounts).remove(&key);
                self.script_stake_credentials.remove(&key);
                tracing::debug!("Stake key deregistered (Conway): {}", hex::encode(key));
            }
            Certificate::StakeDelegation {
                credential,
                pool_hash,
            } => {
                let key = credential_to_hash(credential);
                Arc::make_mut(&mut self.delegations).insert(key, *pool_hash);
                tracing::debug!("Stake delegated to pool: {}", hex::encode(pool_hash));
            }
            Certificate::PoolRegistration(params) => {
                let pool_reg = PoolRegistration {
                    pool_id: params.operator,
                    vrf_keyhash: params.vrf_keyhash,
                    pledge: params.pledge,
                    cost: params.cost,
                    margin_numerator: params.margin.numerator,
                    margin_denominator: params.margin.denominator,
                    reward_account: params.reward_account.clone(),
                    owners: params.pool_owners.clone(),
                    relays: params.relays.clone(),
                    metadata_url: params.pool_metadata.as_ref().map(|m| m.url.clone()),
                    metadata_hash: params.pool_metadata.as_ref().map(|m| m.hash),
                };
                // If pool is re-registering, cancel any pending retirement
                if self.pool_params.contains_key(&params.operator) {
                    for pools in self.pending_retirements.values_mut() {
                        pools.retain(|id| id != &params.operator);
                    }
                    self.pending_retirements
                        .retain(|_, pools| !pools.is_empty());
                    tracing::debug!(
                        "Pool re-registered (pending retirement cancelled): {}",
                        hex::encode(params.operator)
                    );
                } else {
                    tracing::debug!("Pool registered: {}", hex::encode(params.operator));
                }
                Arc::make_mut(&mut self.pool_params).insert(params.operator, pool_reg);
            }
            Certificate::PoolRetirement { pool_hash, epoch } => {
                // Validate: retirement epoch must be <= current_epoch + e_max
                let max_retirement_epoch = self.epoch.0.saturating_add(self.protocol_params.e_max);
                if *epoch > max_retirement_epoch {
                    tracing::warn!(
                        pool = %hex::encode(pool_hash),
                        retirement_epoch = epoch,
                        current_epoch = self.epoch.0,
                        e_max = self.protocol_params.e_max,
                        "Pool retirement epoch exceeds e_max bound, ignoring"
                    );
                } else {
                    tracing::debug!(
                        "Pool retirement scheduled at epoch {}: {}",
                        epoch,
                        hex::encode(pool_hash)
                    );
                    self.pending_retirements
                        .entry(EpochNo(*epoch))
                        .or_default()
                        .push(*pool_hash);
                }
            }
            Certificate::RegStakeDeleg {
                credential,
                pool_hash,
                ..
            } => {
                let key = credential_to_hash(credential);
                self.stake_distribution
                    .stake_map
                    .entry(key)
                    .or_insert(Lovelace(0));
                Arc::make_mut(&mut self.reward_accounts)
                    .entry(key)
                    .or_insert(Lovelace(0));
                Arc::make_mut(&mut self.delegations).insert(key, *pool_hash);
                if matches!(credential, Credential::Script(_)) {
                    self.script_stake_credentials.insert(key);
                }
            }
            Certificate::RegDRep {
                credential,
                deposit,
                anchor,
            } => {
                let key = credential_to_hash(credential);
                Arc::make_mut(&mut self.governance).dreps.insert(
                    key,
                    DRepRegistration {
                        credential: credential.clone(),
                        deposit: *deposit,
                        anchor: anchor.clone(),
                        registered_epoch: self.epoch,
                        last_active_epoch: self.epoch,
                        active: true,
                    },
                );
                Arc::make_mut(&mut self.governance).drep_registration_count += 1;
                tracing::debug!("DRep registered: {}", hex::encode(key));
            }
            Certificate::UnregDRep {
                credential,
                refund: _,
            } => {
                let key = credential_to_hash(credential);
                Arc::make_mut(&mut self.governance).dreps.remove(&key);
                tracing::debug!("DRep deregistered: {}", hex::encode(key));
            }
            Certificate::UpdateDRep { credential, anchor } => {
                let key = credential_to_hash(credential);
                if let Some(drep) = Arc::make_mut(&mut self.governance).dreps.get_mut(&key) {
                    drep.anchor = anchor.clone();
                    drep.last_active_epoch = self.epoch;
                    tracing::debug!("DRep updated: {}", hex::encode(key));
                }
            }
            Certificate::VoteDelegation { credential, drep } => {
                let key = credential_to_hash(credential);
                Arc::make_mut(&mut self.governance)
                    .vote_delegations
                    .insert(key, drep.clone());
                tracing::debug!("Vote delegated to {:?}", drep);
            }
            Certificate::StakeVoteDelegation {
                credential,
                pool_hash,
                drep,
            } => {
                let key = credential_to_hash(credential);
                // Stake delegation
                Arc::make_mut(&mut self.delegations).insert(key, *pool_hash);
                // Vote delegation
                Arc::make_mut(&mut self.governance)
                    .vote_delegations
                    .insert(key, drep.clone());
                tracing::debug!(
                    "Stake+vote delegated to pool {} and drep {:?}",
                    hex::encode(pool_hash),
                    drep
                );
            }
            Certificate::CommitteeHotAuth {
                cold_credential,
                hot_credential,
            } => {
                let cold_key = credential_to_hash(cold_credential);
                let hot_key = credential_to_hash(hot_credential);
                let gov = Arc::make_mut(&mut self.governance);
                gov.committee_hot_keys.insert(cold_key, hot_key);
                gov.committee_resigned.remove(&cold_key);
                if matches!(cold_credential, Credential::Script(_)) {
                    gov.script_committee_credentials.insert(cold_key);
                }
                tracing::debug!(
                    "Committee hot key authorized: {} -> {}",
                    hex::encode(cold_key),
                    hex::encode(hot_key)
                );
            }
            Certificate::CommitteeColdResign {
                cold_credential,
                anchor,
            } => {
                let cold_key = credential_to_hash(cold_credential);
                let gov = Arc::make_mut(&mut self.governance);
                gov.committee_resigned.insert(cold_key, anchor.clone());
                gov.committee_hot_keys.remove(&cold_key);
                if matches!(cold_credential, Credential::Script(_)) {
                    gov.script_committee_credentials.insert(cold_key);
                }
                tracing::debug!("Committee member resigned: {}", hex::encode(cold_key));
            }
            Certificate::RegStakeVoteDeleg {
                credential,
                pool_hash,
                drep,
                ..
            } => {
                let key = credential_to_hash(credential);
                // Register stake credential
                self.stake_distribution
                    .stake_map
                    .entry(key)
                    .or_insert(Lovelace(0));
                Arc::make_mut(&mut self.reward_accounts)
                    .entry(key)
                    .or_insert(Lovelace(0));
                // Stake delegation
                Arc::make_mut(&mut self.delegations).insert(key, *pool_hash);
                // Vote delegation
                Arc::make_mut(&mut self.governance)
                    .vote_delegations
                    .insert(key, drep.clone());
                if matches!(credential, Credential::Script(_)) {
                    self.script_stake_credentials.insert(key);
                }
                tracing::debug!(
                    "Reg+stake+vote delegated: pool={}, drep={:?}",
                    hex::encode(pool_hash),
                    drep
                );
            }
            Certificate::VoteRegDeleg {
                credential, drep, ..
            } => {
                let key = credential_to_hash(credential);
                // Register stake credential
                self.stake_distribution
                    .stake_map
                    .entry(key)
                    .or_insert(Lovelace(0));
                Arc::make_mut(&mut self.reward_accounts)
                    .entry(key)
                    .or_insert(Lovelace(0));
                // Vote delegation
                Arc::make_mut(&mut self.governance)
                    .vote_delegations
                    .insert(key, drep.clone());
                if matches!(credential, Credential::Script(_)) {
                    self.script_stake_credentials.insert(key);
                }
                tracing::debug!("Reg+vote delegated to {:?}", drep);
            }
            Certificate::GenesisKeyDelegation {
                genesis_hash,
                genesis_delegate_hash,
                vrf_keyhash,
            } => {
                // Genesis key delegation (Shelley-era governance by genesis keys)
                tracing::debug!(
                    "Genesis key delegation: {} -> delegate={}, vrf={}",
                    hex::encode(genesis_hash),
                    hex::encode(genesis_delegate_hash),
                    hex::encode(vrf_keyhash)
                );
            }
            Certificate::MoveInstantaneousRewardsCert { source, target } => {
                // MIR: transfer funds between reserves/treasury or distribute to stake credentials
                match target {
                    MIRTarget::StakeCredentials(creds) => {
                        let mut total_distributed: u64 = 0;
                        for (cred, amount) in creds {
                            let key = credential_to_hash(cred);
                            let entry = Arc::make_mut(&mut self.reward_accounts)
                                .entry(key)
                                .or_insert(Lovelace(0));
                            // Handle positive and negative amounts
                            entry.0 = entry.0.saturating_add(amount.0);
                            total_distributed = total_distributed.saturating_add(amount.0);
                            tracing::debug!(
                                "MIR: distributed {} lovelace from {:?} to {}",
                                amount.0,
                                source,
                                hex::encode(key)
                            );
                        }
                        // Debit the source pot
                        if total_distributed > 0 {
                            match source {
                                MIRSource::Reserves => {
                                    self.reserves.0 =
                                        self.reserves.0.saturating_sub(total_distributed);
                                }
                                MIRSource::Treasury => {
                                    self.treasury.0 =
                                        self.treasury.0.saturating_sub(total_distributed);
                                }
                            }
                        }
                    }
                    MIRTarget::OtherPot(coin) => {
                        // Transfer between reserves and treasury
                        match source {
                            MIRSource::Reserves => {
                                let actual = coin.0.min(self.reserves.0);
                                self.reserves.0 = self.reserves.0.saturating_sub(actual);
                                self.treasury.0 = self.treasury.0.saturating_add(actual);
                                tracing::debug!(
                                    "MIR: transferred {} lovelace from reserves to treasury",
                                    actual
                                );
                            }
                            MIRSource::Treasury => {
                                let actual = coin.0.min(self.treasury.0);
                                self.treasury.0 = self.treasury.0.saturating_sub(actual);
                                self.reserves.0 = self.reserves.0.saturating_add(actual);
                                tracing::debug!(
                                    "MIR: transferred {} lovelace from treasury to reserves",
                                    actual
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Process a withdrawal from a reward account.
    /// Per Cardano spec, the withdrawal amount must exactly match the reward balance.
    pub fn process_withdrawal(&mut self, reward_account: &[u8], amount: Lovelace) {
        let key = Self::reward_account_to_hash(reward_account);
        if let Some(balance) = Arc::make_mut(&mut self.reward_accounts).get_mut(&key) {
            if balance.0 != amount.0 {
                tracing::debug!(
                    account = %hex::encode(key),
                    balance = balance.0,
                    withdrawal = amount.0,
                    "Withdrawal amount does not match reward balance"
                );
            }
            // Always process the withdrawal: set balance to 0
            balance.0 = 0;
        }
    }
}
