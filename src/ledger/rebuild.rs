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
            if let Some(cred_hash) = extract_stake_credential(&output.address, &self.ptr_map) {
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
            // Keys are 29 bytes: [hash(28)] [tag(1)] where tag=0x00 keyHash, 0x01 scriptHash
            if cred_bytes.len() >= 29 {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_bytes[..28]);
                hash[28] = cred_bytes[28]; // propagate key/script type tag
                *new_map.entry(hash).or_insert(Lovelace(0)) += Lovelace(amount);
            } else if cred_bytes.len() >= 28 {
                // Legacy 28-byte keys (no tag) — treat as keyHash (tag 0x00)
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

/// Decode a variable-length unsigned integer from a byte slice, returning
/// `(value, bytes_consumed)`.  Returns `None` on truncated / overflowing input.
///
/// Encoding: each byte contributes 7 payload bits; the MSB is 1 when more
/// bytes follow, 0 for the final byte.  Same as pallas-addresses' varuint.
fn decode_varuint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut output: u128 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        output = (output << 7) | (b & 0x7f) as u128;
        if output > u64::MAX as u128 {
            // Overflow – treat as u64::MAX (matches pallas behaviour for invalid
            // testnet addresses).
            return Some((u64::MAX, i + 1));
        }
        if (b & 0x80) == 0 {
            return Some((output as u64, i + 1));
        }
    }
    None // truncated
}

/// Extract stake credential hash from a Cardano address.
///
/// Cardano addresses encode the stake credential in different positions depending
/// on the address type. This function handles the common address formats.
///
/// Address types (per CIP-19):
/// - Type 0x00-0x03: Base address (payment + stake)
/// - Type 0x04-0x05: Pointer address (payment + pointer) — resolved via `ptr_map`
/// - Type 0x06-0x07: Enterprise address (payment only, no stake)
/// - Type 0x0e-0x0f: Reward address (stake only)
///
/// For base addresses (type 0-3), the stake credential is the second credential.
/// For pointer addresses (type 4-5), the pointer is decoded and resolved via `ptr_map`.
/// For reward addresses (type 14-15), the stake credential is the only credential.
///
/// Returns `None` if:
/// - Address is too short
/// - Address type has no stake credential (enterprise)
/// - Pointer address cannot be resolved (pointer not in map)
/// - Address format is invalid
fn extract_stake_credential(
    address: &[u8],
    ptr_map: &HashMap<(u64, u32, u32), [u8; 29]>,
) -> Option<Hash32> {
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
        //
        // Stake credential type tag (byte 28 of Hash32):
        //   types 0, 1 => keyHash   (tag 0x00)
        //   types 2, 3 => scriptHash (tag 0x01)
        0x0 | 0x1 | 0x2 | 0x3 => {
            // Base address: header(1) + payment(28) + stake(28) = 57 bytes
            if address.len() >= 57 {
                let stake_bytes = &address[29..57]; // Skip header(1) + payment(28)
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(stake_bytes);
                // Types 2, 3 have scriptHash stake credential
                if addr_type >= 2 {
                    hash[28] = 0x01; // scriptHash tag
                }
                // Types 0, 1: keyHash tag is 0x00 (already zero)
                Some(hash)
            } else {
                None
            }
        }
        // Pointer address types 4-5 (CIP-19):
        // [header(1)] [payment(28)] [slot(varint)] [tx_idx(varint)] [cert_idx(varint)]
        // Resolve the pointer via ptr_map to get the tagged credential.
        0x4 | 0x5 => {
            if address.len() < 30 {
                return None; // too short for header + payment + at least 1 byte of pointer
            }
            let pointer_bytes = &address[29..]; // after header(1) + payment(28)
            let (slot, n1) = decode_varuint(pointer_bytes)?;
            let (tx_idx, n2) = decode_varuint(&pointer_bytes[n1..])?;
            let (cert_idx, _n3) = decode_varuint(&pointer_bytes[n1 + n2..])?;

            ptr_map
                .get(&(slot, tx_idx as u32, cert_idx as u32))
                .map(|cred| {
                    let mut hash = [0u8; 32];
                    hash[..28].copy_from_slice(&cred[..28]);
                    hash[28] = cred[28]; // propagate key/script tag
                    hash
                })
        }
        // Reward address types 14-15 (CIP-19):
        // [header(1)] [stake(28)] = 29 bytes. Both key and script hashes are 28 bytes.
        // Type 0xe = keyHash, type 0xf = scriptHash
        0xe | 0xf => {
            if address.len() >= 29 {
                let stake_bytes = &address[1..29];
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(stake_bytes);
                if addr_type == 0xf {
                    hash[28] = 0x01; // scriptHash tag
                }
                Some(hash)
            } else {
                None
            }
        }
        // Enterprise (6-7), Byron (8), etc. - no stake credential
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

        let cred = extract_stake_credential(&addr, &HashMap::new()).unwrap();
        assert_eq!(&cred[..28], &[0xbb; 28]);
        assert_eq!(cred[28], 0x00); // keyHash tag
        assert_eq!(&cred[29..], &[0, 0, 0]); // remaining zero-padded
    }

    #[test]
    fn test_extract_stake_credential_base_address_type1() {
        // Base address type 1: payment script (28) + stake key (28)
        let mut addr = vec![0x11]; // type 1, testnet
        addr.extend_from_slice(&[0xaa; 28]); // payment script hash (28 bytes)
        addr.extend_from_slice(&[0xbb; 28]); // stake key hash (28 bytes)

        let cred = extract_stake_credential(&addr, &HashMap::new()).unwrap();
        assert_eq!(&cred[..28], &[0xbb; 28]);
        assert_eq!(cred[28], 0x00); // keyHash tag (types 0,1 have key stake)
    }

    #[test]
    fn test_extract_stake_credential_base_address_type2() {
        // Base address type 2: payment key (28) + stake script (28)
        let mut addr = vec![0x21]; // type 2, testnet
        addr.extend_from_slice(&[0xaa; 28]); // payment key hash (28 bytes)
        addr.extend_from_slice(&[0xcc; 28]); // stake script hash (28 bytes)

        let cred = extract_stake_credential(&addr, &HashMap::new()).unwrap();
        assert_eq!(&cred[..28], &[0xcc; 28]);
        assert_eq!(cred[28], 0x01); // scriptHash tag (types 2,3 have script stake)
    }

    #[test]
    fn test_extract_stake_credential_base_address_type3() {
        // Base address type 3: payment script (28) + stake script (28)
        let mut addr = vec![0x31]; // type 3, testnet
        addr.extend_from_slice(&[0xaa; 28]); // payment script hash (28 bytes)
        addr.extend_from_slice(&[0xdd; 28]); // stake script hash (28 bytes)

        let cred = extract_stake_credential(&addr, &HashMap::new()).unwrap();
        assert_eq!(&cred[..28], &[0xdd; 28]);
        assert_eq!(cred[28], 0x01); // scriptHash tag
    }

    #[test]
    fn test_extract_stake_credential_reward_address() {
        // Reward address type 14: stake key only
        let mut addr = vec![0xe1];
        addr.extend_from_slice(&[0xcc; 28]);

        let cred = extract_stake_credential(&addr, &HashMap::new()).unwrap();
        assert_eq!(&cred[..28], &[0xcc; 28]);
        assert_eq!(cred[28], 0x00); // keyHash tag (type 0xe)
    }

    #[test]
    fn test_extract_stake_credential_reward_address_script() {
        // Reward address type 15: stake script only (28 bytes per CIP-19)
        let mut addr = vec![0xf1]; // type 15, testnet
        addr.extend_from_slice(&[0xee; 28]);

        let cred = extract_stake_credential(&addr, &HashMap::new()).unwrap();
        assert_eq!(&cred[..28], &[0xee; 28]);
        assert_eq!(cred[28], 0x01); // scriptHash tag (type 0xf)
    }

    #[test]
    fn test_extract_stake_credential_enterprise() {
        // Enterprise address type 6: no stake credential
        let mut addr = vec![0x61];
        addr.extend_from_slice(&[0xaa; 28]);

        let cred = extract_stake_credential(&addr, &HashMap::new());
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
            let cred = extract_stake_credential(&addr, &HashMap::new());
            assert!(
                cred.is_some(),
                "type {:#04x} should extract stake credential",
                addr_type
            );
            // Types 0x20, 0x30 have script stake → tag 0x01; types 0x00, 0x10 have key stake → tag 0x00
            let expected_tag = if addr_type >= 0x20 { 0x01 } else { 0x00 };
            assert_eq!(
                cred.unwrap()[28], expected_tag,
                "type {:#04x} should have tag {:#04x}", addr_type, expected_tag
            );
        }
    }

    #[test]
    fn test_same_hash_different_type_produces_different_hash32() {
        // The core bug #14 test: same 28-byte hash at keyHash vs scriptHash address
        // must produce different Hash32 keys
        let hash_bytes = [0xab; 28];

        // Type 0: keyHash stake
        let mut addr_key = vec![0x01]; // type 0, testnet
        addr_key.extend_from_slice(&[0xaa; 28]); // payment
        addr_key.extend_from_slice(&hash_bytes); // stake

        // Type 3: scriptHash stake (same raw hash)
        let mut addr_script = vec![0x31]; // type 3, testnet
        addr_script.extend_from_slice(&[0xaa; 28]); // payment
        addr_script.extend_from_slice(&hash_bytes); // stake

        let cred_key = extract_stake_credential(&addr_key, &HashMap::new()).unwrap();
        let cred_script = extract_stake_credential(&addr_script, &HashMap::new()).unwrap();

        // Same raw hash bytes
        assert_eq!(&cred_key[..28], &cred_script[..28]);
        // Different type tag
        assert_eq!(cred_key[28], 0x00);   // keyHash
        assert_eq!(cred_script[28], 0x01); // scriptHash
        // Therefore different Hash32 values (the whole point of bug #14 fix)
        assert_ne!(cred_key, cred_script);
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
