// Directly check LSM tree data
use cardano_lsm::{LsmTree, LsmConfig, Key};
use std::path::PathBuf;
use anyhow::Result;

fn main() -> Result<()> {
    let base_path = PathBuf::from("./data/node/sanchonet");

    println!("\n=== Checking LSM Trees ===\n");

    // Check treasury tree for any epochs
    println!("📊 Treasury tree:");
    let treasury_tree = LsmTree::open(base_path.join("treasury"), LsmConfig::default())?;
    let mut found_epochs = Vec::new();
    for epoch in 0..100 {
        let key = format!("treasury:{}", epoch);
        if treasury_tree.get(&Key::from(key.as_bytes()))?.is_some() {
            found_epochs.push(epoch);
        }
    }
    println!("  Found epochs: {:?}", found_epochs);
    println!();

    // Check rewards tree
    println!("💰 Rewards tree:");
    let rewards_tree = LsmTree::open(base_path.join("rewards"), LsmConfig::default())?;
    for epoch in found_epochs.iter().take(3) {
        let prefix = format!("rewards:{}:", epoch);
        println!("  Checking epoch {}...", epoch);
        // Try to get first key
        let test_key = format!("rewards:{}:00", epoch);
        if rewards_tree.get(&Key::from(test_key.as_bytes()))?.is_some() {
            println!("    Has reward data!");
        }
    }
    println!();

    // Check delegations
    println!("🔗 Delegations tree:");
    let del_tree = LsmTree::open(base_path.join("delegations"), LsmConfig::default())?;
    let test_key = b"delegation:latest";
    match del_tree.get(&Key::from(test_key))? {
        Some(_) => println!("  Has delegation data"),
        None => println!("  No delegation data found"),
    }

    Ok(())
}
