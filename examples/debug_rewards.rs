// Debug: Check reward account balances

use anyhow::Result;
use cardano_lsm::LsmTree;

fn main() -> Result<()> {
    let tree = LsmTree::open("./data/node/sanchonet/rewards", cardano_lsm::LsmConfig::default())?;

    println!("Reward account entries:\n");

    let mut count = 0;
    let mut total = 0u64;
    for (key, value) in tree.iter() {
        let key_str = String::from_utf8_lossy(key.as_ref());
        let value_bytes: &[u8] = value.as_ref();

        if value_bytes.len() >= 8 {
            let amount = u64::from_le_bytes(value_bytes[0..8].try_into().unwrap());
            if count < 10 {
                println!("Key: {}, Amount: {} lovelace ({} ADA)",
                    key_str, amount, amount / 1_000_000);
            }
            total += amount;
            count += 1;
        }
    }

    println!("\nTotal reward accounts: {}", count);
    println!("Total rewards: {} lovelace ({} ADA)", total, total / 1_000_000);

    Ok(())
}
