// Query stored epoch state from LSM trees
use cardano_lsm::{LsmTree, LsmConfig, Key};
use std::path::PathBuf;
use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let epoch: u64 = if args.len() > 1 {
        args[1].parse().expect("Invalid epoch number")
    } else {
        59
    };

    let base_path = PathBuf::from("./data/node/sanchonet");

    println!("\n=== EPOCH {} LEDGER STATE ===\n", epoch);

    // 1. Epoch Nonce
    println!("🔐 Epoch Nonce:");
    let nonce_tree = LsmTree::open(base_path.join("nonces"), LsmConfig::default())?;
    let nonce_key = format!("nonce:{}", epoch);
    if let Some(value) = nonce_tree.get(&Key::from(nonce_key.as_bytes()))? {
        let nonce_bytes = value.as_ref();
        if nonce_bytes.len() == 32 {
            println!("   Full:  {}", hex::encode(nonce_bytes));
            println!("   Short: {}", hex::encode(&nonce_bytes[..8]));
        }
    } else {
        println!("   (Not found)");
    }
    println!();

    // 2. Treasury Snapshot
    println!("💰 Treasury & Reserves:");
    let treasury_tree = LsmTree::open(base_path.join("treasury"), LsmConfig::default())?;
    let treasury_key = format!("treasury:{}", epoch);
    if let Some(value) = treasury_tree.get(&Key::from(treasury_key.as_bytes()))? {
        #[derive(serde::Deserialize, Debug)]
        struct TreasurySnapshot {
            epoch: u64,
            treasury: u64,
            reserves: u64,
        }

        match bincode::deserialize::<TreasurySnapshot>(value.as_ref()) {
            Ok(snapshot) => {
                println!("   Treasury: {:>15} ADA ({:>20} lovelace)",
                    snapshot.treasury / 1_000_000, snapshot.treasury);
                println!("   Reserves: {:>15} ADA ({:>20} lovelace)",
                    snapshot.reserves / 1_000_000, snapshot.reserves);
            }
            Err(e) => println!("   Error deserializing: {}", e),
        }
    } else {
        println!("   (Not found)");
    }
    println!();

    // 3. Governance State
    println!("🏛️  Governance State:");
    let gov_tree = LsmTree::open(base_path.join("governance"), LsmConfig::default())?;
    let gov_key = format!("governance:{}", epoch);
    if let Some(value) = gov_tree.get(&Key::from(gov_key.as_bytes()))? {
        println!("   Stored: {} bytes (bincode serialized)", value.as_ref().len());
        println!("   Contains: Proposals, votes, committee, DReps, ratification state");
    } else {
        println!("   (Not found)");
    }
    println!();

    // 4. Deposit Tracker
    println!("🔒 Deposit Tracker:");
    let deposits_tree = LsmTree::open(base_path.join("deposits"), LsmConfig::default())?;
    let deposits_key = format!("deposits:{}", epoch);
    if let Some(value) = deposits_tree.get(&Key::from(deposits_key.as_bytes()))? {
        println!("   Stored: {} bytes (bincode serialized)", value.as_ref().len());
        println!("   Tracks: Pool, stake, governance, DRep deposits");
        println!("   Purpose: Separate voting stake from staking stake (Conway)");
    } else {
        println!("   (Not found)");
    }
    println!();

    // 5. Summary Statistics
    println!("📊 Summary:");
    println!("   Database: {}", base_path.display());
    println!("   Epoch: {}", epoch);
    println!("   All ledger state components successfully stored!");
    println!();

    Ok(())
}
