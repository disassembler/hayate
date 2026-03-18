// Debug: Print first 20 UTxOs from the tree to see actual amounts

use anyhow::Result;
use cardano_lsm::LsmTree;
use serde::{Serialize, Deserialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtxoEntry {
    pub address: Vec<u8>,
    pub amount: u64,
    pub assets: HashMap<String, u64>,
    pub datum_hash: Option<Vec<u8>>,
    pub datum: Option<Vec<u8>>,
    pub script_ref: Option<Vec<u8>>,
    pub stake_credential: Option<Vec<u8>>,
}

fn main() -> Result<()> {
    let tree = LsmTree::open("./data/node/sanchonet/utxos", cardano_lsm::LsmConfig::default())?;

    println!("First 20 UTxOs with stake credentials:\n");

    let mut count = 0;
    for (_key, value) in tree.iter() {
        let value_bytes: &[u8] = value.as_ref();
        if value_bytes.is_empty() {
            continue; // Skip tombstones
        }

        if let Ok(utxo) = bincode::deserialize::<UtxoEntry>(value_bytes) {
            if utxo.stake_credential.is_some() {
                println!("UTxO #{}: amount = {} lovelace ({} ADA)",
                    count + 1,
                    utxo.amount,
                    utxo.amount / 1_000_000
                );

                count += 1;
                if count >= 20 {
                    break;
                }
            }
        }
    }

    println!("\nTotal UTxOs scanned: {}", count);

    Ok(())
}
