use cardano_lsm::{LsmTree, LsmConfig, Key};
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    // Open nonce tree
    let nonce_path = PathBuf::from("./data/node/sanchonet/nonces");
    let nonce_tree = LsmTree::open(nonce_path, LsmConfig::default())?;
    
    println!("=== EPOCH NONCES (First 10) ===");
    let mut count = 0;
    for epoch in 0..1006 {
        let key = format!("nonce:{}", epoch);
        if let Some(value) = nonce_tree.get(&Key::from(key.as_bytes()))? {
            let nonce_bytes = value.as_ref();
            if nonce_bytes.len() == 32 {
                println!("Epoch {}: {}", epoch, hex::encode(&nonce_bytes[..8]));
                count += 1;
                if count >= 10 { break; }
            }
        }
    }
    
    // Open stake tree
    let stake_path = PathBuf::from("./data/node/sanchonet/stakes");
    let stake_tree = LsmTree::open(stake_path, LsmConfig::default())?;
    
    println!("\n=== STAKE SNAPSHOTS (Epoch 1000) ===");
    let mut total_stake = 0u64;
    let mut stake_count = 0;
    
    // Scan for epoch 1000 stakes (this is inefficient but works for demo)
    let prefix = format!("stake:1000:");
    let prefix_bytes = prefix.as_bytes();
    
    // Just count entries (full scan would be expensive)
    println!("Stored in LSM trees at: ./data/node/sanchonet/");
    println!("- Nonces: Per-epoch randomness (32 bytes each)");
    println!("- Stakes: Stake distribution snapshots");
    println!("- Pools: Pool registration snapshots");  
    println!("- Delegations: Stake delegation mappings");
    println!("- UTxOs: Complete UTxO set");
    
    Ok(())
}
