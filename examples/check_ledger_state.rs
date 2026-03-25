// Check ledger state from storage
use hayate::node::storage::NodeStorage;
use hayate::indexer::Network;
use std::path::PathBuf;
use anyhow::Result;

fn main() -> Result<()> {
    let mut storage = NodeStorage::open(PathBuf::from("./data"), Network::SanchoNet)?;

    if let Some((ledger_state, slot, _hash)) = storage.restore_latest_snapshot()? {
        let epoch = ledger_state.epoch.0;
        println!("\n=== LEDGER STATE (Epoch {}, resume slot {}) ===\n", epoch, slot);

        println!("📊 Reward Accounts: {}", ledger_state.reward_accounts.len());
        println!("🔗 Delegations: {}", ledger_state.delegations.len());
        println!("🏊 Pool Params: {}", ledger_state.pool_params.len());
        println!("📈 Stake Distribution: {}", ledger_state.stake_distribution.stake_map.len());
        println!("💰 Treasury: {} ADA", ledger_state.treasury.0 / 1_000_000);
        println!();

        if ledger_state.delegations.len() > 0 {
            println!("Sample delegations (first 5):");
            for (i, (cred, pool)) in ledger_state.delegations.iter().enumerate().take(5) {
                println!("  {} -> {}", hex::encode(&cred[..8]), hex::encode(&pool[..8]));
            }
            println!();
        }

        if ledger_state.pool_params.len() > 0 {
            println!("Registered pools:");
            for (pool_id, params) in ledger_state.pool_params.iter() {
                println!("  Pool {}: pledge={} ADA, cost={} ADA",
                    hex::encode(&pool_id[..8]),
                    params.pledge.0 / 1_000_000,
                    params.cost.0 / 1_000_000);
            }
        }
    } else {
        println!("No ledger state found");
    }

    Ok(())
}
