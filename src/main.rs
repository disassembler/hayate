// Hayate (疾風) - Swift Cardano Indexer with UTxORPC API

mod api;
mod chain_sync;
mod cli;
mod config;
mod genesis; // Genesis file parsing
mod gpg;
mod indexer;
mod keys;
mod ledger; // Ledger state management
mod mock_types;
mod node;
mod protocol_params;
mod rewards;
mod snapshot_manager;
mod wallet;
mod wallet_stats;

use chain_sync::HayateSync;
use clap::{CommandFactory, Parser};
use cli::Args;
use indexer::{HayateIndexer, Network};
use pallas_network::miniprotocols::chainsync::NextResponse;
use pallas_network::miniprotocols::Point;
use pallas_traverse::MultiEraBlock;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use tracing::trace;

/// Run chain sync from a node socket
async fn run_chain_sync(
    indexer: Arc<HayateIndexer>,
    network: Network,
    socket_path: String,
    tokens: Vec<config::TokenConfig>,
) -> anyhow::Result<()> {
    use indexer::block_processor::BlockProcessor;

    info!("🔄 Starting chain sync from socket: {}", socket_path);

    // Get wallet IDs for per-wallet tip tracking
    let wallet_ids = indexer.account_xpubs.read().await.clone();

    // Get storage handle
    let networks = indexer.networks.read().await;
    let storage_handle = networks
        .get(&network)
        .ok_or_else(|| anyhow::anyhow!("Network storage not found"))?
        .clone();
    drop(networks); // Release read lock

    // Get minimum wallet tip for resume point
    // This ensures newly added wallets are indexed from their last known tip (or origin)
    let start_point = if let Some(tip) = storage_handle
        .get_min_wallet_tip(wallet_ids.clone())
        .await?
    {
        info!(
            "Resuming from slot {} (minimum across {} wallets)",
            tip.slot,
            wallet_ids.len()
        );
        Point::Specific(tip.slot, tip.hash)
    } else {
        info!("Starting from origin (new wallets or empty database)");
        Point::Origin
    };

    // Connect to node via Unix socket
    let magic = network.magic();
    let mut sync = HayateSync::connect(&socket_path, magic, start_point).await?;
    info!("✅ Connected to Cardano node");

    // Create block processor with storage handle
    let system_start_ms = network.system_start_ms();
    let mut processor = BlockProcessor::new(storage_handle.clone(), system_start_ms).await?;

    // Add wallet IDs to processor for per-wallet tip tracking
    for wallet_id in &wallet_ids {
        processor.add_wallet_id(wallet_id.clone());
    }

    // Derive key hashes from account xpubs and populate wallet filter
    for wallet_xpub_str in &wallet_ids {
        match keys::parse_account_xpub(wallet_xpub_str) {
            Ok(account_xpub) => {
                // Derive wallet keys with gap limit
                match keys::derive_account_keys(account_xpub, indexer.gap_limit) {
                    Ok(wallet_account) => {
                        // Add all derived payment key hashes and stake key hash to filter
                        let (payment_hashes, stake_hash) =
                            keys::get_wallet_key_hashes(&wallet_account);

                        info!(
                            "Indexing wallet {} with {} payment addresses and stake key",
                            &wallet_xpub_str[..20],
                            payment_hashes.len()
                        );

                        // Add each payment key with the stake key to the filter
                        for payment_hash in payment_hashes {
                            processor.add_wallet(payment_hash, stake_hash);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to derive keys from xpub {}: {}",
                            wallet_xpub_str,
                            e
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to parse account xpub {}: {}", wallet_xpub_str, e);
            }
        }
    }

    // Add tracked tokens to processor
    for token in &tokens {
        processor.add_tracked_token(token.clone());
    }

    // Add tracked addresses to processor
    let tracked_addresses = indexer.tracked_addresses.read().await.clone();
    for address_str in &tracked_addresses {
        use pallas_addresses::Address;

        // Try bech32 first, then hex
        let address_bytes = match Address::from_bech32(address_str) {
            Ok(addr) => addr.to_vec(),
            Err(_) => {
                // Try parsing as hex
                match hex::decode(address_str) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        tracing::warn!(
                            "Failed to parse address {} (not bech32 or hex): {}",
                            address_str,
                            e
                        );
                        continue;
                    }
                }
            }
        };

        info!("Adding tracked address: {}", address_str);
        processor.add_tracked_address(address_bytes);
    }

    if !tokens.is_empty() {
        info!("Tracking {} native token(s)", tokens.len());
    }

    info!("🔄 Starting block processing...");

    // Subscribe to restart signals
    let mut restart_rx = indexer.subscribe_restart();

    // Process blocks - loop until shutdown signal or restart signal
    // Handle both SIGINT (Ctrl+C) and SIGTERM (systemd/docker stop)
    let mut shutdown = Box::pin(async {
        let ctrl_c = tokio::signal::ctrl_c();

        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("Failed to setup SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => info!("🛑 Received SIGINT (Ctrl+C)"),
                _ = sigterm.recv() => info!("🛑 Received SIGTERM"),
            }
        }

        #[cfg(not(unix))]
        {
            ctrl_c.await.expect("Failed to listen for Ctrl+C");
            info!("🛑 Received shutdown signal (Ctrl+C)");
        }
    });

    let mut is_caught_up = false;
    let result: anyhow::Result<()> = loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!("Shutting down gracefully...");
                break Ok(());
            }
            restart_slot = restart_rx.recv() => {
                match restart_slot {
                    Ok(slot) => {
                        info!("🔄 Restart signal received! Rewinding to slot {}", slot);
                        // Break out and restart the chain sync from the new slot
                        // We'll use a special error to signal restart
                        break Err(anyhow::anyhow!("RESTART:{}", slot));
                    }
                    Err(e) => {
                        tracing::warn!("Restart channel error: {}", e);
                    }
                }
            }
            next_result = async {
                // Check agency and call appropriate method
                if sync.has_agency() {
                    sync.request_next().await
                } else {
                    sync.await_next().await
                }
            } => {
                let next = next_result?;

        // Process the response
        match next {
            NextResponse::RollForward(block_bytes, tip) => {
                // Parse block to get slot and hash
                let block = MultiEraBlock::decode(&block_bytes)?;
                let slot = block.slot();
                let hash = block.hash();

                // Extract chain tip slot for snapshot logic
                let chain_tip_slot = match tip.0 {
                    Point::Specific(tip_slot, _) => Some(tip_slot),
                    Point::Origin => None,
                };

                // Log new blocks only when we were previously caught up (waiting at tip)
                // This means we only log blocks that extend the chain after we've been idle
                if is_caught_up {
                    info!("📦 New block at slot {} - {}", slot, hex::encode(&hash.as_ref()[..8]));
                }

                is_caught_up = false;

                // Process block
                match processor.process_block(&block_bytes, slot, hash.as_ref(), chain_tip_slot).await {
                    Ok(_stats) => {
                        // Broadcast block update
                        indexer.broadcast_block(indexer::BlockUpdate {
                            network: network.clone(),
                            height: 0, // TODO: track height
                            slot,
                            hash: hash.as_ref().to_vec(),
                            tx_hashes: Vec::new(), // TODO: collect tx hashes
                        });
                    }
                    Err(e) => {
                        tracing::error!("Error processing block at slot {}: {}", slot, e);
                        continue;
                    }
                }
            }
            NextResponse::RollBackward(point, _tip) => {
                is_caught_up = false;
                info!("⚠️  Rollback to {:?}", point);
                match point {
                    Point::Specific(slot, _) => {
                        match processor.rollback_to(slot).await {
                            Ok(count) => info!("✓ Rolled back {} blocks", count),
                            Err(e) => tracing::error!("Failed to rollback: {}", e),
                        }
                    }
                    Point::Origin => {
                        info!("Rollback to origin requested");
                        match processor.rollback_to(0).await {
                            Ok(count) => info!("✓ Rolled back {} blocks to origin", count),
                            Err(e) => tracing::error!("Failed to rollback to origin: {}", e),
                        }
                    }
                }
            }
            NextResponse::Await => {
                if !is_caught_up {
                    tracing::trace!("Caught up - waiting for new blocks");
                    is_caught_up = true;
                }
                // The protocol state machine requires us to call recv_while_must_reply()
                // after receiving Await, but that call should block until a message arrives.
                // However, to avoid busy-looping if it returns immediately, add a small delay.
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }
            }
        }
    };

    // Check if this is a restart - if so, skip final tip save to preserve the rewound tip
    let is_restart = result
        .as_ref()
        .err()
        .map(|e| e.to_string().starts_with("RESTART:"))
        .unwrap_or(false);

    if !is_restart {
        // Save tips before exiting (whether shutdown, error, or normal exit)
        info!("💾 Saving final chain tips...");
        processor.save_current_tips().await?;

        // Create final snapshot before exit for minimal data loss
        info!("📸 Creating final snapshot before shutdown...");
        let current_slot = processor.current_slot;
        if let Err(e) = processor.create_final_snapshot().await {
            tracing::error!("Failed to create final snapshot: {}", e);
            // Continue with shutdown even if snapshot fails
        } else {
            info!("✓ Final snapshot saved at slot {}", current_slot);
        }
    } else {
        info!("🔄 Restarting - skipping final save to preserve rewound tip");
    }

    result
}

async fn handle_wallet_command(wallet_cmd: &cli::WalletCommand, args: &Args) -> anyhow::Result<()> {
    // Determine wallet directory
    let wallet_dir = args
        .db_path
        .as_ref()
        .map(|p| PathBuf::from(p).join("wallets"))
        .unwrap_or_else(|| PathBuf::from("./hayate-wallets"));

    // Load config to get UTxORPC endpoint
    let config = if let Some(ref config_path) = args.config {
        config::HayateConfig::load(config_path)?
    } else {
        config::HayateConfig::default()
    };

    let utxorpc_endpoint = Some(format!("http://{}", config.api.bind));

    // Use the wallet CLI handler
    wallet::handle_wallet_command(wallet_cmd, wallet_dir, utxorpc_endpoint).await
}

async fn handle_query_command(query_cmd: &cli::QueryCommand) -> anyhow::Result<()> {
    match query_cmd {
        cli::QueryCommand::ProtocolParams {
            socket,
            magic,
            output,
            json,
        } => {
            info!("Querying protocol parameters from node...");
            info!("  Socket: {}", socket);
            info!("  Magic:  {}", magic);

            // Query protocol parameters using N2C
            use node::ProtocolParamQuery;
            let mut query = ProtocolParamQuery::new(socket.clone(), *magic);
            let params = query.query_current_params().await?;

            // Format output
            if *json {
                let json_output = serde_json::to_string_pretty(&params)?;

                if let Some(output_path) = output {
                    std::fs::write(output_path, &json_output)?;
                    info!("✅ Protocol parameters saved to: {}", output_path);
                } else {
                    println!("{}", json_output);
                }
            } else {
                // Human-readable format
                let output_text = format!(
                    r#"Protocol Parameters (Epoch {})

Fee Parameters:
  min_fee_a (linear):        {}
  min_fee_b (constant):      {} lovelace
  max_tx_size:               {} bytes
  max_block_body_size:       {} bytes

UTxO Parameters:
  utxo_cost_per_byte:        {} lovelace/byte

Stake Parameters:
  key_deposit:               {} lovelace ({} ADA)
  pool_deposit:              {} lovelace ({} ADA)
  min_pool_cost:             {} lovelace ({} ADA)

Plutus Parameters:
  price_memory:              {}
  price_steps:               {}
  max_tx_execution_units:    {:?}
  max_block_execution_units: {:?}
"#,
                    params.epoch,
                    params.min_fee_a,
                    params.min_fee_b,
                    params.max_tx_size,
                    params.max_block_body_size,
                    params.utxo_cost_per_byte,
                    params.key_deposit,
                    params.key_deposit / 1_000_000,
                    params.pool_deposit,
                    params.pool_deposit / 1_000_000,
                    params.min_pool_cost,
                    params.min_pool_cost / 1_000_000,
                    params
                        .price_memory
                        .as_ref()
                        .map(|r| format!("{}/{}", r.numerator, r.denominator))
                        .unwrap_or_else(|| "N/A".to_string()),
                    params
                        .price_steps
                        .as_ref()
                        .map(|r| format!("{}/{}", r.numerator, r.denominator))
                        .unwrap_or_else(|| "N/A".to_string()),
                    params.max_tx_execution_units,
                    params.max_block_execution_units,
                );

                if let Some(output_path) = output {
                    std::fs::write(output_path, &output_text)?;
                    info!("✅ Protocol parameters saved to: {}", output_path);
                } else {
                    println!("{}", output_text);
                }
            }

            Ok(())
        }
    }
}

async fn handle_config_command(config_cmd: &cli::ConfigCommand) -> anyhow::Result<()> {
    match config_cmd {
        cli::ConfigCommand::Generate { output } => {
            info!("Generating default config at: {}", output);
            config::HayateConfig::generate_default(output)?;
            info!("✅ Config file created: {}", output);
            info!("Edit the file and run: hayate sync --config {}", output);
            Ok(())
        }
    }
}

fn handle_completions_command(shell: &cli::Shell) -> anyhow::Result<()> {
    use clap_complete::{generate, Shell as ClapShell};
    use std::io;

    let mut cmd = Args::command();
    let bin_name = cmd.get_name().to_string();

    let clap_shell = match shell {
        cli::Shell::Bash => ClapShell::Bash,
        cli::Shell::Zsh => ClapShell::Zsh,
        cli::Shell::Fish => ClapShell::Fish,
        cli::Shell::PowerShell => ClapShell::PowerShell,
        cli::Shell::Elvish => ClapShell::Elvish,
    };

    generate(clap_shell, &mut cmd, bin_name, &mut io::stdout());
    Ok(())
}

async fn handle_rollback_command(
    epoch: u64,
    network_str: Option<String>,
    db_path_str: Option<String>,
) -> anyhow::Result<()> {
    use indexer::block_processor::BlockProcessor;

    // Parse network
    let network = if let Some(net_str) = network_str {
        Network::parse(&net_str).ok_or_else(|| anyhow::anyhow!("Invalid network: {}", net_str))?
    } else {
        Network::Preview // Default
    };

    // Determine database path
    let db_path = if let Some(path) = db_path_str {
        PathBuf::from(path)
    } else {
        PathBuf::from("./.hayate")
    };

    info!("🔄 Rolling back {} to epoch {}", network.as_str(), epoch);

    // Calculate target slot from epoch
    let target_slot = epoch_to_slot(epoch, &network);
    info!("Target slot: {} (epoch {})", target_slot, epoch);

    // Open network storage
    let storage = indexer::NetworkStorage::open(db_path.clone(), network.clone())?;
    let (manager, handle) = indexer::StorageManager::new(storage);

    // Spawn storage manager
    tokio::spawn(async move {
        manager.run().await;
    });

    // Create block processor
    let system_start_ms = network.system_start_ms();
    let mut processor = BlockProcessor::new(handle.clone(), system_start_ms).await?;

    // Perform rollback
    let count = processor.rollback_to(target_slot).await?;

    info!("✅ Rolled back {} blocks", count);
    info!("💾 Saving chain tips...");

    // Save tips
    processor.save_current_tips().await?;

    info!("✅ Rollback complete");

    Ok(())
}

fn epoch_to_slot(epoch: u64, network: &Network) -> u64 {
    let epoch_length = match network {
        Network::Mainnet | Network::Preprod => 432_000,
        Network::Preview => 86_400,
        Network::SanchoNet => 86_400,
        Network::Custom(_) => 432_000,
    };
    epoch * epoch_length
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // Default to info if RUST_LOG not set, but allow RUST_LOG to override
                tracing_subscriber::EnvFilter::new("hayate=info,h2=warn")
            }),
        )
        .init();

    let args = Args::parse();

    // Handle commands
    match &args.command {
        Some(cli::Command::Wallet { wallet_cmd }) => {
            return handle_wallet_command(wallet_cmd, &args).await;
        }
        Some(cli::Command::Query { query_cmd }) => {
            return handle_query_command(query_cmd).await;
        }
        Some(cli::Command::Config { config_cmd }) => {
            return handle_config_command(config_cmd).await;
        }
        Some(cli::Command::Rollback {
            epoch,
            network,
            db_path,
        }) => {
            return handle_rollback_command(*epoch, network.clone(), db_path.clone()).await;
        }
        Some(cli::Command::Completions { shell }) => {
            return handle_completions_command(shell);
        }
        Some(cli::Command::Sync { .. }) | None => {
            // Continue with sync (default behavior)
        }
    }

    // Load configuration
    let mut config = if let Some(config_path) = args.config {
        info!("Loading config from: {}", config_path);
        config::HayateConfig::load(&config_path)?
    } else {
        info!("Using default configuration");
        config::HayateConfig::default()
    };

    // Apply CLI overrides from global args
    if let Some(db_path) = &args.db_path {
        config.data_dir = PathBuf::from(db_path);
    }

    // Apply overrides from Sync command if present
    if let Some(cli::Command::Sync {
        gap_limit,
        api_bind,
        ..
    }) = &args.command
    {
        if let Some(gap_limit) = gap_limit {
            config.gap_limit = *gap_limit;
        }
        if let Some(api_bind) = api_bind {
            config.api.bind = api_bind.clone();
        }
    }

    info!("疾風 Hayate starting...");
    info!("Database: {:?}", config.data_dir);
    info!("UTxORPC API: {}", config.api.bind);
    info!("Gap limit: {}", config.gap_limit);

    // Wrap config in Arc for sharing
    let config = Arc::new(config);

    // Create indexer
    let indexer = Arc::new(HayateIndexer::new(
        config.data_dir.clone(),
        config.gap_limit,
    )?);

    // Determine network to use
    let socket = if let Some(cli::Command::Sync { socket, .. }) = &args.command {
        socket.as_ref()
    } else {
        None
    };

    let network = if let Some(network_str) = &args.network {
        Network::parse(network_str)
            .ok_or_else(|| anyhow::anyhow!("Invalid network: {}", network_str))?
    } else if socket.is_some() {
        return Err(anyhow::anyhow!("--network is required when using --socket"));
    } else {
        // From config file - use first enabled network
        config
            .networks
            .iter()
            .find(|(_, cfg)| cfg.enabled)
            .and_then(|(name, _)| Network::parse(name))
            .ok_or_else(|| anyhow::anyhow!("No network enabled in config"))?
    };

    info!("Network: {}", network.as_str());
    info!("Magic: {}", network.magic());

    // Add network storage
    indexer
        .add_network(network.clone(), config.data_dir.clone())
        .await?;

    // Load wallets, addresses, and tokens from config
    if config.wallets.is_empty() && config.tokens.is_empty() && config.addresses.is_empty() {
        return Err(anyhow::anyhow!(
            "Nothing configured to index. Please configure at least one of:\n\
             - Wallet xpubs (in 'wallets' array)\n\
             - Addresses to track (in 'addresses' array)\n\
             - Native tokens to track (in 'tokens' array)\n\n\
             Generate a default config with: hayate config generate config.toml"
        ));
    }

    for wallet_xpub in &config.wallets {
        info!("Loading wallet: {}", wallet_xpub);
        indexer.add_account(wallet_xpub.clone()).await?;
    }

    info!("Loaded {} wallet(s)", config.wallets.len());

    for address in &config.addresses {
        info!("Loading address: {}", address);
        indexer.add_address(address.clone()).await?;
        // Debug: show the address bytes that will be stored
        use pallas_addresses::Address;
        if let Ok(addr) = Address::from_bech32(address) {
            info!("  Address bytes: {}", hex::encode(addr.to_vec()));
        }
    }

    info!("Loaded {} address(es)", config.addresses.len());

    // If socket is provided, start API in background and run chain sync in foreground
    if let Some(socket_path) = socket {
        info!("Socket: {}", socket_path);
        info!("Starting chain sync with UTxORPC API");

        // Spawn API server in background task
        let indexer_clone = Arc::clone(&indexer);
        let config_clone = Arc::clone(&config);
        let api_bind = config.api.bind.clone();
        let socket_clone = socket_path.clone();
        let network_clone = network.clone();
        tokio::spawn(async move {
            info!("🚀 Starting UTxORPC server on {}...", api_bind);
            if let Err(e) = api::start_utxorpc_server(
                indexer_clone,
                config_clone,
                api_bind,
                network_clone,
                Some(socket_clone),
            )
            .await
            {
                tracing::error!("API server error: {}", e);
            }
        });

        // Run chain sync in main task (this will block)
        // Wrap in loop to handle restarts from AddAddressAndRollback
        loop {
            match run_chain_sync(
                indexer.clone(),
                network.clone(),
                socket_path.clone(),
                config.tokens.clone(),
            )
            .await
            {
                Ok(()) => {
                    // Normal shutdown
                    return Ok(());
                }
                Err(e) => {
                    // Check if this is a restart signal
                    if let Some(slot_str) = e.to_string().strip_prefix("RESTART:") {
                        if let Ok(_slot) = slot_str.parse::<u64>() {
                            info!("🔄 Restarting chain sync to replay from rewound position...");
                            // Loop will restart run_chain_sync, which will read the new tip from storage
                            continue;
                        }
                    }
                    // Real error - propagate it
                    return Err(e);
                }
            }
        }
    }

    // No socket provided - run in API-only mode
    info!("Running in API-only mode (reading from existing DB)");

    // Get socket path from network config (for GetBlockByHash support)
    let socket_path = config
        .networks
        .get(network.as_str())
        .and_then(|cfg| cfg.socket_path.as_ref())
        .map(|p| p.to_string_lossy().to_string());

    if socket_path.is_none() {
        info!(
            "⚠️  No socket_path configured for {} - GetBlockByHash will not be available",
            network.as_str()
        );
        info!("💡 Add socket_path to network config to enable block-by-hash queries");
    }

    info!("🚀 Starting UTxORPC server on {}...", config.api.bind);
    let api_bind = config.api.bind.clone();
    api::start_utxorpc_server(indexer, config, api_bind, network, socket_path).await?;

    Ok(())
}
