// Debug: Check delegations

use anyhow::Result;
use cardano_lsm::LsmTree;

fn main() -> Result<()> {
    let tree = LsmTree::open("./data/node/sanchonet/delegations", cardano_lsm::LsmConfig::default())?;

    println!("Delegation entries:\n");

    let mut count = 0;
    for (key, _value) in tree.iter() {
        let key_str = String::from_utf8_lossy(key.as_ref());
        if count < 10 {
            println!("Delegation: {}", key_str);
        }
        count += 1;
    }

    println!("\nTotal delegations: {}", count);

    Ok(())
}
