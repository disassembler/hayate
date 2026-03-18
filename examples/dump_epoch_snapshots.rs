use anyhow::Result;
use cardano_lsm::LsmTree;
use std::path::PathBuf;

const EPOCH_LENGTH: u64 = 86400; // slots per epoch on sanchonet

fn main() -> Result<()> {
    let base_path = PathBuf::from("./data/node/sanchonet");

    // Check which epochs we have
    let snapshots_path = base_path.join("rewards/snapshots");
    let mut epochs: Vec<u32> = std::fs::read_dir(&snapshots_path)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("epoch-") {
                name.strip_prefix("epoch-")?
                    .parse::<u32>().ok()
            } else {
                None
            }
        })
        .collect();

    epochs.sort();

    println!("Found {} epoch snapshots", epochs.len());
    println!();

    for epoch in &epochs {
        if *epoch > 10 {
            break; // Only process first 10 epochs
        }

        let slot = (*epoch as u64) * EPOCH_LENGTH;
        println!("=== Epoch {} (slot {}) ===", epoch, slot);

        // Open the snapshot trees
        let snapshot_name = format!("epoch-{:05}", epoch);

        // Treasury
        if let Ok(tree) = LsmTree::open_snapshot(
            base_path.join("treasury"),
            &snapshot_name,
        ) {
            let mut total_treasury = 0u64;
            for item in tree.iter() {
                let (_key, value) = item?;
                if value.len() >= 8 {
                    total_treasury = u64::from_le_bytes(value[0..8].try_into().unwrap());
                }
            }
            println!("  Treasury: {} lovelace", total_treasury);
        }

        // Governance (contains reserves)
        if let Ok(tree) = LsmTree::open_snapshot(
            base_path.join("governance"),
            &snapshot_name,
        ) {
            let mut total_reserves = 0u64;
            for item in tree.iter() {
                let (key, value) = item?;
                if key == b"reserves" && value.len() >= 8 {
                    total_reserves = u64::from_le_bytes(value[0..8].try_into().unwrap());
                }
            }
            println!("  Reserves: {} lovelace", total_reserves);
        }

        // Rewards
        if let Ok(tree) = LsmTree::open_snapshot(
            base_path.join("rewards"),
            &snapshot_name,
        ) {
            let mut reward_count = 0;
            let mut total_rewards = 0u64;
            for item in tree.iter() {
                let (_key, value) = item?;
                if value.len() >= 8 {
                    reward_count += 1;
                    let amount = u64::from_le_bytes(value[0..8].try_into().unwrap());
                    total_rewards += amount;
                }
            }
            println!("  Reward accounts: {}", reward_count);
            println!("  Total rewards: {} lovelace", total_rewards);
        }

        // Pools
        if let Ok(tree) = LsmTree::open_snapshot(
            base_path.join("pools"),
            &snapshot_name,
        ) {
            let pool_count = tree.iter().count();
            println!("  Pools: {}", pool_count);
        }

        println!();
    }

    Ok(())
}
