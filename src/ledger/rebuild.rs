// UTxO-based stake distribution rebuild
//
// Copied from torsten-ledger/src/state/epoch.rs rebuild_stake_distribution
// Extended to account for deposits and adapted for hayate's LSM-based UTxO storage

use super::primitives::*;
use super::state::LedgerState;
use std::collections::HashMap;

/// UTxO output representation for stake calculation
/// This is a minimal view needed to extract stake credential and value
#[derive(Debug, Clone)]
pub struct UtxoOutput {
    pub address: Vec<u8>,
    pub coin: u64,
}

impl LedgerState {
    /// Rebuild stake distribution from UTxO set at epoch boundaries.
    ///
    /// This function recomputes the stake_distribution.stake_map from scratch
    /// by scanning all UTxOs and extracting staked value. This prevents
    /// incremental drift that could accumulate from block-by-block updates.
    ///
    /// Per Cardano spec, this should be called at EVERY epoch boundary during
    /// live sync to ensure correctness.
    ///
    /// # Arguments
    /// - `utxos`: Iterator of (address, coin) pairs from the UTxO set
    ///
    /// # Stake Calculation
    /// For each credential:
    /// 1. UTxO stake: sum of all UTxO values at addresses with this stake credential
    /// 2. Reward balance: current reward account balance
    /// 3. Deposits: voting stake (includes governance deposits, pool deposits, etc.)
    ///
    /// Note: The snapshot building in epoch.rs adds reward balances and deposits
    /// separately, so this function only needs to rebuild the UTxO component.
    pub fn rebuild_stake_distribution<I>(&mut self, utxos: I)
    where
        I: Iterator<Item = UtxoOutput>,
    {
        // Pre-size to the current credential count to minimize rehashing
        let mut new_map: HashMap<Hash32, Lovelace> =
            HashMap::with_capacity(self.stake_distribution.stake_map.len());

        // Scan UTxO set and accumulate stake per credential
        for output in utxos {
            if let Some(cred_hash) = extract_stake_credential(&output.address) {
                *new_map.entry(cred_hash).or_insert(Lovelace(0)) += Lovelace(output.coin);
            }
        }

        // Ensure all registered stake credentials have entries (even with 0 stake)
        // This is important for delegations where the credential is registered but
        // has no UTxO stake currently
        for cred_hash in self.delegations.keys() {
            new_map.entry(*cred_hash).or_insert(Lovelace(0));
        }

        // Also ensure reward account holders are included (they may have 0 UTxO stake
        // but still have reward balance)
        for cred_hash in self.reward_accounts.keys() {
            new_map.entry(*cred_hash).or_insert(Lovelace(0));
        }

        self.stake_distribution.stake_map = new_map;

        tracing::debug!(
            "Rebuilt stake distribution: {} credentials",
            self.stake_distribution.stake_map.len()
        );
    }

    /// Rebuild stake distribution from the incrementally-maintained `current_stake` map
    /// in `NodeStorage`. This is O(registered_credentials) rather than O(all_utxos),
    /// and should be used at every epoch boundary during normal sync.
    ///
    /// `utxo_stake`: the `NodeStorage::current_stake()` map — credential bytes → lovelace.
    pub fn rebuild_stake_from_current_stake(&mut self, utxo_stake: &HashMap<Vec<u8>, u64>) {
        let mut new_map: HashMap<Hash32, Lovelace> =
            HashMap::with_capacity(utxo_stake.len().max(self.stake_distribution.stake_map.len()));

        for (cred_bytes, &amount) in utxo_stake {
            if cred_bytes.len() >= 28 {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_bytes[..28]);
                *new_map.entry(hash).or_insert(Lovelace(0)) += Lovelace(amount);
            }
        }

        // Ensure all registered delegators/reward accounts have entries
        for cred_hash in self.delegations.keys() {
            new_map.entry(*cred_hash).or_insert(Lovelace(0));
        }
        for cred_hash in self.reward_accounts.keys() {
            new_map.entry(*cred_hash).or_insert(Lovelace(0));
        }

        self.stake_distribution.stake_map = new_map;
    }

    /// Rebuild stake distribution from hayate's UTxO LSM tree.
    ///
    /// This is a convenience wrapper that queries the NetworkStorage's utxo_tree
    /// and calls rebuild_stake_distribution with the results.
    ///
    /// TODO: Implement this when integrating with BlockProcessor.
    /// For now, this is a placeholder that shows the intended API.
    ///
    /// # Example
    /// ```ignore
    /// ledger_state.rebuild_from_storage(&network_storage)?;
    /// ```
    #[allow(dead_code)]
    pub fn rebuild_from_utxo_tree(
        &mut self,
        utxo_tree: &cardano_lsm::LsmTree,
    ) -> anyhow::Result<()> {
        use crate::node::storage::UtxoEntry;

        tracing::debug!("Rebuilding stake distribution from UTxO tree...");

        // Collect UTxOs by iterating the entire tree
        let mut utxos = Vec::new();
        let mut count = 0;
        let mut total_scanned = 0;
        let mut amount_samples = Vec::new(); // Sample first 10 amounts

        for (_key, value) in utxo_tree.iter() {
            total_scanned += 1;
            let value_bytes: &[u8] = value.as_ref();

            // Skip tombstone entries (empty values)
            if value_bytes.is_empty() {
                continue;
            }

            // Deserialize the UTxO entry
            match bincode::deserialize::<UtxoEntry>(value_bytes) {
                Ok(utxo_entry) => {
                    // Check if this UTxO has a stake credential
                    if let Some(ref stake_cred) = utxo_entry.stake_credential {
                        // Sample first 10 amounts for debugging
                        if amount_samples.len() < 10 {
                            amount_samples.push(utxo_entry.amount);
                        }

                        // Convert to Hash32 format
                        if stake_cred.len() == 32 {
                            let mut hash = [0u8; 32];
                            hash.copy_from_slice(stake_cred);

                            utxos.push(UtxoOutput {
                                address: utxo_entry.address.clone(),
                                coin: utxo_entry.amount,
                            });
                            count += 1;
                        } else if stake_cred.len() == 28 {
                            // 28-byte key hash needs to be padded to 32 bytes
                            let mut hash = [0u8; 32];
                            hash[..28].copy_from_slice(stake_cred);

                            utxos.push(UtxoOutput {
                                address: utxo_entry.address.clone(),
                                coin: utxo_entry.amount,
                            });
                            count += 1;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to decode UTxO: {}", e);
                    continue;
                }
            }
        }

        tracing::debug!("Scanned {} total entries, {} UTxOs with stake credentials", total_scanned, count);
        tracing::debug!("Sample UTxO amounts (first 10): {:?}", amount_samples);

        // Rebuild stake distribution from collected UTxOs
        self.rebuild_stake_distribution(utxos.into_iter());

        tracing::debug!("✓ Rebuilt stake distribution: {} credentials, {} total stake",
            self.stake_distribution.stake_map.len(),
            self.stake_distribution.stake_map.values().map(|l| l.0).sum::<u64>() / 1_000_000
        );

        Ok(())
    }

    /// Compute the total lovelace value of all live UTxOs in the tree.
    ///
    /// Used at the Byron→Shelley transition to reset reserves to the correct
    /// value: `reserves = maxLovelaceSupply - total_utxo_value`.  This accounts
    /// for Byron transaction fees which reduce the UTxO set (and therefore
    /// increase reserves relative to the genesis seed value).
    pub fn recalibrate_reserves_from_utxo_tree(
        &mut self,
        utxo_tree: &cardano_lsm::LsmTree,
    ) -> anyhow::Result<()> {
        use crate::node::storage::UtxoEntry;

        let mut total: u64 = 0;
        for (_key, value) in utxo_tree.iter() {
            let value_bytes: &[u8] = value.as_ref();
            if value_bytes.is_empty() {
                continue; // tombstone
            }
            match bincode::deserialize::<UtxoEntry>(value_bytes) {
                Ok(entry) => total = total.saturating_add(entry.amount),
                Err(e) => tracing::warn!("Failed to decode UTxO during reserve recalibration: {}", e),
            }
        }

        const MAX_LOVELACE: u64 = 45_000_000_000_000_000;
        let new_reserves = MAX_LOVELACE.saturating_sub(total);
        tracing::info!(
            "🔄 Byron→Shelley reserve recalibration: total_utxo={} lovelace, \
             old_reserves={} → new_reserves={}",
            total, self.reserves.0, new_reserves
        );
        self.reserves.0 = new_reserves;
        Ok(())
    }
}

/// Extract stake credential hash from a Cardano address.
///
/// Cardano addresses encode the stake credential in different positions depending
/// on the address type. This function handles the common address formats.
///
/// Address types (per CIP-19):
/// - Type 0x00-0x03: Base address (payment + stake)
/// - Type 0x04-0x05: Pointer address (payment + pointer)
/// - Type 0x06-0x07: Enterprise address (payment only, no stake)
/// - Type 0x0e-0x0f: Reward address (stake only)
///
/// For base addresses (type 0-3), the stake credential is the second credential.
/// For reward addresses (type 14-15), the stake credential is the only credential.
///
/// Returns `None` if:
/// - Address is too short
/// - Address type has no stake credential (enterprise, pointer)
/// - Address format is invalid
fn extract_stake_credential(address: &[u8]) -> Option<Hash32> {
    if address.is_empty() {
        return None;
    }

    let addr_type = address[0] >> 4;

    match addr_type {
        // Base address types 0-3 (CIP-19):
        // ALL credential hashes (key and script) are blake2b-224 = 28 bytes.
        // ALL base addresses are: [header(1)] [payment(28)] [stake(28)] = 57 bytes.
        // The type nibble encodes whether each credential is key or script, but
        // the on-chain representation is always 28 bytes regardless.
        0x0 | 0x1 | 0x2 | 0x3 => {
            // Base address: header(1) + payment(28) + stake(28) = 57 bytes
            if address.len() >= 57 {
                let stake_bytes = &address[29..57]; // Skip header(1) + payment(28)
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(stake_bytes);
                Some(hash)
            } else {
                None
            }
        }
        // Reward address types 14-15 (CIP-19):
        // [header(1)] [stake(28)] = 29 bytes. Both key and script hashes are 28 bytes.
        0xe | 0xf => {
            if address.len() >= 29 {
                let stake_bytes = &address[1..29];
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(stake_bytes);
                Some(hash)
            } else {
                None
            }
        }
        // Enterprise (6-7), Pointer (4-5), Byron (8), etc. - no stake credential
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_stake_credential_base_address_type0() {
        // Base address type 0: payment key + stake key
        // Header 0x01 (testnet, type 0), payment (28 bytes), stake (28 bytes)
        let mut addr = vec![0x01];
        addr.extend_from_slice(&[0xaa; 28]); // payment
        addr.extend_from_slice(&[0xbb; 28]); // stake

        let cred = extract_stake_credential(&addr).unwrap();
        assert_eq!(&cred[..28], &[0xbb; 28]);
        assert_eq!(&cred[28..], &[0, 0, 0, 0]); // Zero-padded
    }

    #[test]
    fn test_extract_stake_credential_base_address_type1() {
        // Base address type 1: payment script (28) + stake key (28)
        // Per CIP-19, script hashes are blake2b-224 = 28 bytes
        let mut addr = vec![0x11]; // type 1, testnet
        addr.extend_from_slice(&[0xaa; 28]); // payment script hash (28 bytes)
        addr.extend_from_slice(&[0xbb; 28]); // stake key hash (28 bytes)

        let cred = extract_stake_credential(&addr).unwrap();
        assert_eq!(&cred[..28], &[0xbb; 28]);
        assert_eq!(&cred[28..], &[0, 0, 0, 0]);
    }

    #[test]
    fn test_extract_stake_credential_base_address_type2() {
        // Base address type 2: payment key (28) + stake script (28)
        // This is the address type for script-based stake credentials
        let mut addr = vec![0x21]; // type 2, testnet
        addr.extend_from_slice(&[0xaa; 28]); // payment key hash (28 bytes)
        addr.extend_from_slice(&[0xcc; 28]); // stake script hash (28 bytes)

        let cred = extract_stake_credential(&addr).unwrap();
        assert_eq!(&cred[..28], &[0xcc; 28]);
        assert_eq!(&cred[28..], &[0, 0, 0, 0]);
    }

    #[test]
    fn test_extract_stake_credential_base_address_type3() {
        // Base address type 3: payment script (28) + stake script (28)
        let mut addr = vec![0x31]; // type 3, testnet
        addr.extend_from_slice(&[0xaa; 28]); // payment script hash (28 bytes)
        addr.extend_from_slice(&[0xdd; 28]); // stake script hash (28 bytes)

        let cred = extract_stake_credential(&addr).unwrap();
        assert_eq!(&cred[..28], &[0xdd; 28]);
        assert_eq!(&cred[28..], &[0, 0, 0, 0]);
    }

    #[test]
    fn test_extract_stake_credential_reward_address() {
        // Reward address type 14: stake key only
        // Header 0xe1 (testnet, type 14), stake (28 bytes)
        let mut addr = vec![0xe1];
        addr.extend_from_slice(&[0xcc; 28]);

        let cred = extract_stake_credential(&addr).unwrap();
        assert_eq!(&cred[..28], &[0xcc; 28]);
        assert_eq!(&cred[28..], &[0, 0, 0, 0]);
    }

    #[test]
    fn test_extract_stake_credential_reward_address_script() {
        // Reward address type 15: stake script only (28 bytes per CIP-19)
        let mut addr = vec![0xf1]; // type 15, testnet
        addr.extend_from_slice(&[0xee; 28]);

        let cred = extract_stake_credential(&addr).unwrap();
        assert_eq!(&cred[..28], &[0xee; 28]);
        assert_eq!(&cred[28..], &[0, 0, 0, 0]);
    }

    #[test]
    fn test_extract_stake_credential_enterprise() {
        // Enterprise address type 6: no stake credential
        let mut addr = vec![0x61];
        addr.extend_from_slice(&[0xaa; 28]);

        let cred = extract_stake_credential(&addr);
        assert!(cred.is_none());
    }

    #[test]
    fn test_all_base_address_types_are_57_bytes() {
        // Per CIP-19, ALL base addresses are 1 + 28 + 28 = 57 bytes
        for addr_type in [0x00u8, 0x10, 0x20, 0x30] {
            let mut addr = vec![addr_type | 0x01]; // testnet
            addr.extend_from_slice(&[0xaa; 28]); // payment
            addr.extend_from_slice(&[0xbb; 28]); // stake
            assert_eq!(addr.len(), 57, "type {:#04x} address should be 57 bytes", addr_type);
            assert!(
                extract_stake_credential(&addr).is_some(),
                "type {:#04x} should extract stake credential",
                addr_type
            );
        }
    }

    #[test]
    fn test_rebuild_stake_distribution() {
        let mut state = LedgerState::new(ProtocolParameters::default());

        // Create some test UTxOs
        let utxos = vec![
            UtxoOutput {
                address: {
                    let mut addr = vec![0x01]; // Type 0 base address
                    addr.extend_from_slice(&[0xaa; 28]); // payment
                    addr.extend_from_slice(&[0xbb; 28]); // stake
                    addr
                },
                coin: 1_000_000,
            },
            UtxoOutput {
                address: {
                    let mut addr = vec![0x01];
                    addr.extend_from_slice(&[0xaa; 28]);
                    addr.extend_from_slice(&[0xbb; 28]); // Same stake cred
                    addr
                },
                coin: 2_000_000,
            },
        ];

        state.rebuild_stake_distribution(utxos.into_iter());

        // Should have accumulated 3M lovelace for the stake credential
        let mut expected_cred = [0u8; 32];
        expected_cred[..28].copy_from_slice(&[0xbb; 28]);

        assert_eq!(
            state.stake_distribution.stake_map.get(&expected_cred),
            Some(&Lovelace(3_000_000))
        );
    }
}
