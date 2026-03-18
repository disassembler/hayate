// Debug: Check deposits tree

use anyhow::Result;
use cardano_lsm::LsmTree;

fn main() -> Result<()> {
    let tree = LsmTree::open("./data/node/sanchonet/deposits", cardano_lsm::LsmConfig::default())?;

    println!("Deposit entries:\n");

    let mut count = 0;
    for (key, value) in tree.iter() {
        let key_str = String::from_utf8_lossy(key.as_ref());
        let value_bytes: &[u8] = value.as_ref();

        if count < 10 {
            println!("Key: {}, Value length: {} bytes",
                key_str, value_bytes.len());
        }
        count += 1;
    }

    println!("\nTotal deposit entries: {}", count);

    Ok(())
}
