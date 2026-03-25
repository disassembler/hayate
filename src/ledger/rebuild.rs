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
        // Base address: payment (28/32 bytes) + stake (28 bytes)
        // Type 0-3 encode whether payment/stake are key hash (28) or script hash (32)
        0x0 => {
            // Type 0: payment key hash (28) + stake key hash (28)
            // Address = [header(1)] [payment(28)] [stake(28)] = 57 bytes
            if address.len() >= 57 {
                let stake_bytes = &address[29..57]; // Skip header(1) + payment(28)
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(stake_bytes);
                Some(hash)
            } else {
                None
            }
        }
        0x1 => {
            // Type 1: payment script hash (32) + stake key hash (28)
            // Address = [header(1)] [payment(32)] [stake(28)] = 61 bytes
            if address.len() >= 61 {
                let stake_bytes = &address[33..61]; // Skip header(1) + payment(32)
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(stake_bytes);
                Some(hash)
            } else {
                None
            }
        }
        0x2 => {
            // Type 2: payment key hash (28) + stake script hash (32)
            // Address = [header(1)] [payment(28)] [stake(32)] = 61 bytes
            if address.len() >= 61 {
                let stake_bytes = &address[29..61]; // Skip header(1) + payment(28)
                let mut hash = [0u8; 32];
                hash.copy_from_slice(stake_bytes);
                Some(hash)
            } else {
                None
            }
        }
        0x3 => {
            // Type 3: payment script hash (32) + stake script hash (32)
            // Address = [header(1)] [payment(32)] [stake(32)] = 65 bytes
            if address.len() >= 65 {
                let stake_bytes = &address[33..65]; // Skip header(1) + payment(32)
                let mut hash = [0u8; 32];
                hash.copy_from_slice(stake_bytes);
                Some(hash)
            } else {
                None
            }
        }
        0xe => {
            // Type 14: Reward address with key hash (28 bytes)
            // Address = [header(1)] [stake(28)] = 29 bytes
            if address.len() >= 29 {
                let stake_bytes = &address[1..29];
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(stake_bytes);
                Some(hash)
            } else {
                None
            }
        }
        0xf => {
            // Type 15: Reward address with script hash (32 bytes)
            // Address = [header(1)] [stake(32)] = 33 bytes
            if address.len() >= 33 {
                let stake_bytes = &address[1..33];
                let mut hash = [0u8; 32];
                hash.copy_from_slice(stake_bytes);
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
    fn test_extract_stake_credential_enterprise() {
        // Enterprise address type 6: no stake credential
        let mut addr = vec![0x61];
        addr.extend_from_slice(&[0xaa; 28]);

        let cred = extract_stake_credential(&addr);
        assert!(cred.is_none());
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
