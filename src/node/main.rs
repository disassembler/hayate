// Hayate-Node (疾風ノード) - Full Cardano node with ledger state snapshots

use clap::Parser;
use tracing::{info, error, warn};
use std::path::PathBuf;
use anyhow::{Result, Context};

// Import from lib
use hayate::node::storage::{NodeStorage, UtxoEntry, slot_to_epoch};
use hayate::indexer::Network;
use hayate::chain_sync::HayateSync;
use hayate::ledger::{
    state::LedgerState,
    primitives::{ProtocolParameters, EpochNo},
};
use hayate::genesis::{ByronGenesis, ShelleyGenesis};
use pallas_network::miniprotocols::Point;
use pallas_network::miniprotocols::chainsync::NextResponse;
use pallas_crypto::nonce::generate_rolling_nonce;
use pallas_crypto::hash::Hash;

#[derive(Parser, Debug)]
#[command(name = "hayate-node")]
#[command(about = "疾風ノード Hayate-Node - Full Cardano node with ledger state snapshots", long_about = None)]
struct Args {
    /// Database directory
    #[arg(short, long, default_value = "./data")]
    db_path: String,

    /// Network (mainnet, preprod, preview, sanchonet)
    #[arg(short, long, default_value = "preview")]
    network: String,

    /// Node socket path (for syncing blocks)
    #[arg(short, long)]
    socket: Option<String>,

    /// Start from slot
    #[arg(long)]
    from_slot: Option<u64>,

    /// Magic number (network ID)
    #[arg(long)]
    magic: Option<u64>,

    /// Cardano node config file path (for genesis files)
    /// If not specified, will try to load from default paths based on network
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("hayate_node=info".parse()?)
                .add_directive("hayate=info".parse()?)
        )
        .init();

    let args = Args::parse();

    info!("疾風ノード Hayate-Node starting...");

    // Parse network
    let network = Network::parse(&args.network)
        .ok_or_else(|| anyhow::anyhow!("Invalid network: {}", args.network))?;

    info!("Network: {}", network.as_str());
    info!("Database: {}", args.db_path);

    // Open node storage
    let mut storage = NodeStorage::open(PathBuf::from(&args.db_path), network.clone())?;

    // Check for existing tip
    if let Some((tip_slot, tip_hash)) = storage.get_chain_tip()? {
        info!("Resuming from slot {}, hash: {}", tip_slot, hex::encode(&tip_hash));
    } else {
        info!("Starting fresh sync");
    }

    // Determine socket path
    let socket_path = if let Some(socket) = args.socket {
        socket
    } else {
        // Default socket paths
        match network {
            Network::Preview => {
                std::env::var("CARDANO_NODE_SOCKET_PATH")
                    .unwrap_or_else(|_| "./cardano-node/node.socket".to_string())
            },
            Network::Mainnet => {
                std::env::var("CARDANO_NODE_SOCKET_PATH")
                    .unwrap_or_else(|_| "./cardano-node/node.socket".to_string())
            },
            _ => {
                return Err(anyhow::anyhow!("Please specify --socket for network {}", network.as_str()));
            }
        }
    };

    info!("Connecting to node socket: {}", socket_path);

    // Determine magic
    let magic = args.magic.unwrap_or_else(|| network.magic());

    info!("Network magic: {}", magic);

    // Connect to node via chain sync
    let start_point = if let Some((tip_slot, tip_hash)) = storage.get_chain_tip()? {
        info!("Resuming from slot {}", tip_slot);
        Point::Specific(tip_slot, tip_hash)
    } else {
        info!("Starting from origin");
        Point::Origin
    };

    info!("Connecting to chain sync...");
    let mut sync = HayateSync::connect(&socket_path, magic, start_point).await?;
    info!("✅ Connected to Cardano node via chain sync");

    // Initialize or restore ledger state
    let mut ledger_state = if let Some((restored_epoch, state)) = storage.restore_latest_ledger_state()? {
        info!("✅ Restored ledger state from epoch {}", restored_epoch);
        state
    } else {
        info!("🆕 Initializing fresh ledger state");
        load_genesis_and_init_ledger(args.config.as_ref(), &network)?
    };

    // Start processing blocks
    let mut blocks_processed = 0u64;
    let mut current_epoch = ledger_state.epoch.0;

    // Initialize rolling nonce with Shelley genesis nonce for Preview network
    // This is the starting point for nonce evolution
    let mut rolling_nonce: Option<Hash<32>> = None;

    info!("🔄 Starting block processing from epoch {}...", current_epoch);

    let mut awaiting = false;
    loop {
        if awaiting {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            continue;
        }

        match sync.request_next().await? {
            NextResponse::RollForward(block_bytes, _tip) => {
                awaiting = false;
                // Parse block using pallas
                match parse_block_with_nonce(&block_bytes) {
                    Ok((slot, block_hash, tx_count, vrf_output_opt)) => {
                        // Update rolling nonce if VRF is present
                        if let Some(vrf_output) = vrf_output_opt {
                            // VRF output should be at least 32 bytes
                            if vrf_output.len() >= 32 {
                                match rolling_nonce {
                                    Some(prev_nonce) => {
                                        // generate_rolling_nonce(prev: Hash<32>, vrf: &[u8]) -> Hash<32>
                                        rolling_nonce = Some(generate_rolling_nonce(prev_nonce, &vrf_output[..32]));
                                    }
                                    None => {
                                        // Initialize with Preview network genesis nonce
                                        // For now, using zero hash - TODO: use actual genesis nonce
                                        let init_nonce = Hash::<32>::from([0u8; 32]);
                                        rolling_nonce = Some(generate_rolling_nonce(init_nonce, &vrf_output[..32]));
                                        info!("🔐 Initializing rolling nonce from first VRF at slot {}", slot);
                                    }
                                }
                            } else {
                                warn!("VRF output too short: {} bytes", vrf_output.len());
                            }
                        }

                        // Process block
                        if let Err(e) = process_block_simple(&mut storage, &mut ledger_state, slot, &block_hash, &block_bytes).await {
                            error!("Error processing block at slot {}: {}", slot, e);
                            continue;
                        }

                        blocks_processed += 1;

                        // Check for epoch transition
                        let epoch = slot_to_epoch(slot, &network);

                        // When we transition to a new epoch, snapshot the previous epoch
                        if epoch > current_epoch {
                            info!("🎯 Epoch transition detected: epoch {} → {} at slot {}",
                                current_epoch, epoch, slot);
                            info!("💰 Epoch {} accumulated fees: {} lovelace ({} ADA)",
                                current_epoch,
                                ledger_state.epoch_fees.0,
                                ledger_state.epoch_fees.0 / 1_000_000);

                            // 1. Rebuild stake distribution from UTxO set
                            info!("🔄 Rebuilding stake distribution from UTxO tree...");
                            if let Err(e) = ledger_state.rebuild_from_utxo_tree(&storage.utxo_tree) {
                                error!("❌ Failed to rebuild stake distribution: {}", e);
                            }

                            // 2. Process epoch transition in ledger state
                            info!("⚙️  Processing epoch transition in ledger state...");
                            ledger_state.process_epoch_transition(EpochNo(epoch));

                            // 3. Snapshot complete ledger state to disk
                            info!("💾 Snapshotting complete ledger state for epoch {} at slot {}...", epoch, slot);
                            match storage.snapshot_full_ledger_state(epoch, slot, &ledger_state) {
                                Ok(()) => {
                                    info!("✅ Complete ledger state snapshot saved");

                                    // Log summary statistics
                                    let stake_count = ledger_state.stake_distribution.stake_map.len();
                                    let total_stake: u64 = ledger_state.stake_distribution.stake_map.values()
                                        .map(|l| l.0).sum();
                                    let pool_count = ledger_state.pool_params.len();
                                    let reward_accounts = ledger_state.reward_accounts.len();

                                    info!("📊 Ledger State Summary:");
                                    info!("   • Stake credentials: {}", stake_count);
                                    info!("   • Total staked: {} ADA", total_stake / 1_000_000);
                                    info!("   • Active pools: {}", pool_count);
                                    info!("   • Reward accounts: {}", reward_accounts);
                                    info!("   • Treasury: {} ADA", ledger_state.treasury.0 / 1_000_000);
                                    info!("   • Reserves: {} ADA", ledger_state.reserves.0 / 1_000_000);
                                }
                                Err(e) => {
                                    error!("❌ Failed to snapshot ledger state: {}", e);
                                }
                            }

                            // 3. Store epoch nonce
                            if let Some(nonce) = &rolling_nonce {
                                let nonce_slice: &[u8] = nonce.as_ref();
                                if nonce_slice.len() == 32 {
                                    let mut nonce_bytes = [0u8; 32];
                                    nonce_bytes.copy_from_slice(nonce_slice);
                                    match storage.store_nonce(epoch, &nonce_bytes) {
                                        Ok(()) => {
                                            info!("🔐 Stored epoch nonce for epoch {}: {}",
                                                epoch, hex::encode(&nonce_bytes[..8]));
                                        }
                                        Err(e) => {
                                            error!("Failed to store epoch nonce: {}", e);
                                        }
                                    }
                                } else {
                                    error!("Invalid nonce length: {}", nonce_slice.len());
                                }
                            } else {
                                warn!("No rolling nonce available at epoch boundary");
                            }

                            current_epoch = epoch;
                        }

                        // Update chain tip
                        storage.store_chain_tip(slot, &block_hash)?;

                        // Log progress
                        if blocks_processed % 1000 == 0 {
                            info!("Processed {} blocks, slot: {}, epoch: {}, txs: {}",
                                blocks_processed, slot, epoch, tx_count);
                        }
                    }
                    Err(e) => {
                        error!("Failed to parse block: {}", e);
                        continue;
                    }
                }
            }
            NextResponse::RollBackward(point, _tip) => {
                awaiting = false;
                info!("⚠️  Rollback to {:?}", point);
                // TODO: Implement rollback logic
            }
            NextResponse::Await => {
                info!("Caught up, waiting for new blocks...");
                awaiting = true;
            }
        }
    }
}

/// Parse block and extract VRF nonce for epoch nonce calculation
fn parse_block_with_nonce(block_bytes: &[u8]) -> Result<(u64, Vec<u8>, usize, Option<Vec<u8>>)> {
    use pallas_traverse::MultiEraBlock;

    let block = MultiEraBlock::decode(block_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to decode block: {}", e))?;

    let slot = block.slot();
    let hash = block.hash().to_vec();
    let tx_count = block.txs().len();

    // Extract VRF output from block header
    let vrf_output: Option<Vec<u8>> = if let Some(babbage_block) = block.as_babbage() {
        // Babbage era blocks have vrf_result in header_body
        let vrf_cert = &babbage_block.header.header_body.vrf_result;
        // VrfCert is (output, proof) - we need the output (field .0)
        Some(vrf_cert.0.to_vec())
    } else if let Some(alonzo_block) = block.as_alonzo() {
        // Alonzo era blocks have nonce_vrf field
        let vrf_cert = &alonzo_block.header.header_body.nonce_vrf;
        Some(vrf_cert.0.to_vec())
    } else {
        // Byron and epoch boundary blocks don't have VRF
        None
    };

    Ok((slot, hash, tx_count, vrf_output))
}

async fn process_block_simple(
    storage: &mut NodeStorage,
    ledger_state: &mut LedgerState,
    slot: u64,
    _block_hash: &[u8],
    block_bytes: &[u8],
) -> Result<()> {
    use pallas_traverse::MultiEraBlock;
    use hayate::ledger::primitives::{Lovelace, Hash32, Hash28};
    use std::sync::Arc;

    let block = MultiEraBlock::decode(block_bytes)?;

    // Track total block count for epoch (needed for monetary expansion calculation)
    ledger_state.epoch_block_count += 1;

    // Track block for leader schedule (pool that produced this block)
    if let Some(pool_id) = extract_pool_id_from_block(&block) {
        let mut epoch_blocks = (*ledger_state.epoch_blocks_by_pool).clone();
        *epoch_blocks.entry(pool_id).or_insert(0) += 1;
        ledger_state.epoch_blocks_by_pool = Arc::new(epoch_blocks);
    }

    // Process each transaction
    let txs = block.txs();
    if !txs.is_empty() {
        tracing::debug!("Processing {} transactions in block at slot {}", txs.len(), slot);
    }
    for tx in txs {
        let tx_hash = tx.hash();

        // Process inputs (remove UTxOs)
        for input in tx.inputs() {
            let input_hash = input.hash();
            let input_index = input.index();

            storage.remove_utxo(input_hash.as_ref(), input_index as u32)?;
        }

        // Process outputs (add UTxOs)
        for (output_index, output) in tx.outputs().into_iter().enumerate() {
            let address_bytes = output.address()?.to_vec();
            let amount = output.value().coin();

            // Extract stake credential from address
            let stake_credential = extract_stake_credential(&address_bytes)?;

            // Parse multi-assets
            let mut assets = std::collections::HashMap::new();
            for policy_assets in output.value().assets() {
                let policy_id = hex::encode(policy_assets.policy().as_ref());

                for asset in policy_assets.assets() {
                    let asset_name = hex::encode(asset.name());
                    let asset_key = format!("{}.{}", policy_id, asset_name);
                    let amount = match asset {
                        pallas_traverse::MultiEraAsset::AlonzoCompatibleOutput(_, _, amt) => amt as u64,
                        pallas_traverse::MultiEraAsset::ConwayOutput(_, _, amt) => {
                            u64::from(amt)
                        }
                        _ => 0, // Shouldn't happen for outputs
                    };
                    assets.insert(asset_key, amount);
                }
            }

            // Parse datum and datum hash
            let (datum_hash, datum) = match output.datum() {
                Some(datum_option) => {
                    use pallas_primitives::conway::DatumOption;
                    match datum_option {
                        DatumOption::Hash(hash) => {
                            // Datum hash only (datum is stored separately on-chain)
                            (Some(hash.to_vec()), None)
                        }
                        DatumOption::Data(inline_datum) => {
                            // Inline datum (post-Babbage)
                            
                            

                            // Get the raw bytes from the KeepRaw wrapper
                            let datum_bytes = inline_datum.raw_cbor().to_vec();

                            // Compute datum hash using Blake2b256
                            let mut hasher = pallas_crypto::hash::Hasher::<256>::new();
                            hasher.input(&datum_bytes);
                            let hash = hasher.finalize();

                            (Some(hash.to_vec()), Some(datum_bytes))
                        }
                    }
                }
                None => (None, None),
            };

            let utxo_entry = UtxoEntry {
                address: address_bytes,
                amount,
                assets,
                datum_hash,
                datum,
                script_ref: None, // TODO: Parse script ref
                stake_credential,
            };

            storage.insert_utxo(tx_hash.as_ref(), output_index as u32, &utxo_entry)?;
        }

        // Process certificates (delegations, pool registrations, etc.)
        for cert in tx.certs() {
            process_certificate(cert, ledger_state)?;
        }

        // Process withdrawals (reward account withdrawals)
        // Note: MultiEraWithdrawals needs special handling, simplified for now
        // TODO: Extract withdrawals from transaction and update reward accounts

        // Track transaction fees
        let tx_fee = tx.fee().unwrap_or(0);
        ledger_state.epoch_fees.0 += tx_fee;
        if tx_fee > 0 {
            tracing::debug!(
                "Accumulated fee: tx_fee={}, total_epoch_fees={} lovelace",
                tx_fee,
                ledger_state.epoch_fees.0
            );
        }
    }

    Ok(())
}

fn extract_stake_credential(address: &[u8]) -> Result<Option<Vec<u8>>> {
    use pallas_addresses::{Address, ShelleyDelegationPart};

    let addr = Address::from_bytes(address)
        .map_err(|e| anyhow::anyhow!("Failed to parse address: {}", e))?;

    match addr {
        Address::Shelley(shelley_addr) => {
            match shelley_addr.delegation() {
                ShelleyDelegationPart::Key(key_hash) => {
                    Ok(Some(key_hash.to_vec()))
                }
                ShelleyDelegationPart::Script(script_hash) => {
                    Ok(Some(script_hash.to_vec()))
                }
                ShelleyDelegationPart::Null => {
                    Ok(None)
                }
                _ => Ok(None),
            }
        }
        _ => Ok(None), // Byron addresses don't have stake credentials
    }
}

fn extract_pool_id_from_block(block: &pallas_traverse::MultiEraBlock) -> Option<hayate::ledger::primitives::Hash28> {
    // Try to get pool ID from block header
    if let Some(babbage_block) = block.as_babbage() {
        let pool_id_bytes: &[u8] = babbage_block.header.header_body.issuer_vkey.as_ref();
        if pool_id_bytes.len() >= 28 {
            let mut pool_id = [0u8; 28];
            pool_id.copy_from_slice(&pool_id_bytes[..28]);
            return Some(pool_id);
        }
    } else if let Some(alonzo_block) = block.as_alonzo() {
        let pool_id_bytes: &[u8] = alonzo_block.header.header_body.issuer_vkey.as_ref();
        if pool_id_bytes.len() >= 28 {
            let mut pool_id = [0u8; 28];
            pool_id.copy_from_slice(&pool_id_bytes[..28]);
            return Some(pool_id);
        }
    }
    None
}

fn process_certificate(
    cert: pallas_traverse::MultiEraCert,
    ledger_state: &mut LedgerState,
) -> Result<()> {
    use hayate::ledger::primitives::{Hash32, Hash28, Lovelace, EpochNo};
    use hayate::ledger::state::PoolRegistration;
    use std::sync::Arc;
    use pallas_primitives::{alonzo, conway};

    // MultiEraCert is an enum with AlonzoCompatible and Conway variants
    // Each contains the era-specific Certificate enum
    match cert {
        pallas_traverse::MultiEraCert::AlonzoCompatible(cert_box) => {
            process_alonzo_certificate(&cert_box, ledger_state)?;
        }
        pallas_traverse::MultiEraCert::Conway(cert_box) => {
            process_conway_certificate(&cert_box, ledger_state)?;
        }
        pallas_traverse::MultiEraCert::NotApplicable => {
            // Byron era - no certificates
        }
        _ => {
            // Handle any other unmatched variants
        }
    }

    Ok(())
}

fn stake_credential_to_hash28(cred: &pallas_primitives::StakeCredential) -> Option<[u8; 28]> {
    match cred {
        pallas_primitives::StakeCredential::AddrKeyhash(hash) => {
            let bytes = hash.as_ref();
            if bytes.len() >= 28 {
                let mut result = [0u8; 28];
                result.copy_from_slice(&bytes[..28]);
                Some(result)
            } else {
                None
            }
        }
        pallas_primitives::StakeCredential::ScriptHash(hash) => {
            let bytes = hash.as_ref();
            if bytes.len() >= 28 {
                let mut result = [0u8; 28];
                result.copy_from_slice(&bytes[..28]);
                Some(result)
            } else {
                None
            }
        }
    }
}

fn process_alonzo_certificate(
    cert: &pallas_primitives::alonzo::Certificate,
    ledger_state: &mut LedgerState,
) -> Result<()> {
    use hayate::ledger::primitives::{Hash32, Lovelace, EpochNo};
    use hayate::ledger::state::PoolRegistration;
    use std::sync::Arc;
    use pallas_primitives::alonzo::Certificate;

    match cert {
        Certificate::StakeRegistration(stake_cred) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);

                let mut reward_accounts = (*ledger_state.reward_accounts).clone();
                reward_accounts.entry(hash).or_insert(Lovelace(0));
                ledger_state.reward_accounts = Arc::new(reward_accounts);

                tracing::debug!("Stake registered: {}", hex::encode(&hash[..8]));
            }
        }

        Certificate::StakeDeregistration(stake_cred) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);

                let mut delegations = (*ledger_state.delegations).clone();
                delegations.remove(&hash);
                ledger_state.delegations = Arc::new(delegations);

                let mut reward_accounts = (*ledger_state.reward_accounts).clone();
                reward_accounts.remove(&hash);
                ledger_state.reward_accounts = Arc::new(reward_accounts);

                tracing::debug!("Stake deregistered: {}", hex::encode(&hash[..8]));
            }
        }

        Certificate::StakeDelegation(stake_cred, pool_hash) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let pool_bytes = pool_hash.as_ref();

                if pool_bytes.len() >= 28 {
                    let mut stake_hash = [0u8; 32];
                    stake_hash[..28].copy_from_slice(&cred_hash);

                    let mut pool_id = [0u8; 28];
                    pool_id.copy_from_slice(&pool_bytes[..28]);

                    let mut delegations = (*ledger_state.delegations).clone();
                    delegations.insert(stake_hash, pool_id);
                    ledger_state.delegations = Arc::new(delegations);

                    tracing::debug!("Delegation: {} -> {}",
                        hex::encode(&stake_hash[..8]),
                        hex::encode(&pool_id[..8]));
                }
            }
        }

        Certificate::PoolRegistration {
            operator,
            vrf_keyhash,
            pledge,
            cost,
            margin,
            reward_account,
            pool_owners,
            relays: _,
            pool_metadata: _,
        } => {
            let operator_bytes = operator.as_ref();
            if operator_bytes.len() >= 28 {
                let mut pool_id = [0u8; 28];
                pool_id.copy_from_slice(&operator_bytes[..28]);

                let mut vrf_hash = [0u8; 32];
                let vrf_bytes = vrf_keyhash.as_ref();
                if vrf_bytes.len() >= 32 {
                    vrf_hash.copy_from_slice(&vrf_bytes[..32]);
                }

                let mut owners = Vec::new();
                for owner in pool_owners {
                    let owner_bytes = owner.as_ref();
                    if owner_bytes.len() >= 28 {
                        let mut owner_hash = [0u8; 28];
                        owner_hash.copy_from_slice(&owner_bytes[..28]);
                        owners.push(owner_hash);
                    }
                }

                let pool_reg = PoolRegistration {
                    pool_id,
                    vrf_keyhash: vrf_hash,
                    pledge: Lovelace(*pledge),
                    cost: Lovelace(*cost),
                    margin_numerator: margin.numerator,
                    margin_denominator: margin.denominator,
                    reward_account: reward_account.to_vec(),
                    owners,
                    relays: vec![],
                    metadata_url: None,
                    metadata_hash: None,
                };

                let mut pool_params = (*ledger_state.pool_params).clone();
                pool_params.insert(pool_id, pool_reg);
                ledger_state.pool_params = Arc::new(pool_params);

                tracing::info!("Pool registered: {} (pledge: {} ADA)",
                    hex::encode(&pool_id[..8]),
                    pledge / 1_000_000);
            }
        }

        Certificate::PoolRetirement(pool_hash, retirement_epoch) => {
            let pool_bytes = pool_hash.as_ref();
            if pool_bytes.len() >= 28 {
                let mut pool_id = [0u8; 28];
                pool_id.copy_from_slice(&pool_bytes[..28]);

                ledger_state.pending_retirements.insert(EpochNo(*retirement_epoch), vec![pool_id]);

                tracing::info!("Pool retirement scheduled: {} at epoch {}",
                    hex::encode(&pool_id[..8]),
                    retirement_epoch);
            }
        }

        Certificate::GenesisKeyDelegation(_, _, _) |
        Certificate::MoveInstantaneousRewardsCert(_) => {
            tracing::debug!("Legacy certificate (genesis/MIR)");
        }
    }

    Ok(())
}

fn process_conway_certificate(
    cert: &pallas_primitives::conway::Certificate,
    ledger_state: &mut LedgerState,
) -> Result<()> {
    use hayate::ledger::primitives::{Hash32, Lovelace, EpochNo};
    use hayate::ledger::state::PoolRegistration;
    use std::sync::Arc;
    use pallas_primitives::conway::Certificate;

    match cert {
        // Basic stake certificates (same as Alonzo)
        Certificate::StakeRegistration(stake_cred) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);

                let mut reward_accounts = (*ledger_state.reward_accounts).clone();
                reward_accounts.entry(hash).or_insert(Lovelace(0));
                ledger_state.reward_accounts = Arc::new(reward_accounts);

                tracing::debug!("Stake registered: {}", hex::encode(&hash[..8]));
            }
        }

        Certificate::StakeDeregistration(stake_cred) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);

                let mut delegations = (*ledger_state.delegations).clone();
                delegations.remove(&hash);
                ledger_state.delegations = Arc::new(delegations);

                let mut reward_accounts = (*ledger_state.reward_accounts).clone();
                reward_accounts.remove(&hash);
                ledger_state.reward_accounts = Arc::new(reward_accounts);

                tracing::debug!("Stake deregistered: {}", hex::encode(&hash[..8]));
            }
        }

        Certificate::StakeDelegation(stake_cred, pool_hash) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let pool_bytes = pool_hash.as_ref();

                if pool_bytes.len() >= 28 {
                    let mut stake_hash = [0u8; 32];
                    stake_hash[..28].copy_from_slice(&cred_hash);

                    let mut pool_id = [0u8; 28];
                    pool_id.copy_from_slice(&pool_bytes[..28]);

                    let mut delegations = (*ledger_state.delegations).clone();
                    delegations.insert(stake_hash, pool_id);
                    ledger_state.delegations = Arc::new(delegations);

                    tracing::debug!("Delegation: {} -> {}",
                        hex::encode(&stake_hash[..8]),
                        hex::encode(&pool_id[..8]));
                }
            }
        }

        Certificate::PoolRegistration {
            operator,
            vrf_keyhash,
            pledge,
            cost,
            margin,
            reward_account,
            pool_owners,
            relays: _,
            pool_metadata: _,
        } => {
            let operator_bytes = operator.as_ref();
            if operator_bytes.len() >= 28 {
                let mut pool_id = [0u8; 28];
                pool_id.copy_from_slice(&operator_bytes[..28]);

                let mut vrf_hash = [0u8; 32];
                let vrf_bytes = vrf_keyhash.as_ref();
                if vrf_bytes.len() >= 32 {
                    vrf_hash.copy_from_slice(&vrf_bytes[..32]);
                }

                let mut owners = Vec::new();
                for owner in pool_owners.iter() {
                    let owner_bytes = owner.as_ref();
                    if owner_bytes.len() >= 28 {
                        let mut owner_hash = [0u8; 28];
                        owner_hash.copy_from_slice(&owner_bytes[..28]);
                        owners.push(owner_hash);
                    }
                }

                let pool_reg = PoolRegistration {
                    pool_id,
                    vrf_keyhash: vrf_hash,
                    pledge: Lovelace(*pledge),
                    cost: Lovelace(*cost),
                    margin_numerator: margin.numerator,
                    margin_denominator: margin.denominator,
                    reward_account: reward_account.to_vec(),
                    owners,
                    relays: vec![],
                    metadata_url: None,
                    metadata_hash: None,
                };

                let mut pool_params = (*ledger_state.pool_params).clone();
                pool_params.insert(pool_id, pool_reg);
                ledger_state.pool_params = Arc::new(pool_params);

                tracing::info!("Pool registered: {} (pledge: {} ADA)",
                    hex::encode(&pool_id[..8]),
                    pledge / 1_000_000);
            }
        }

        Certificate::PoolRetirement(pool_hash, retirement_epoch) => {
            let pool_bytes = pool_hash.as_ref();
            if pool_bytes.len() >= 28 {
                let mut pool_id = [0u8; 28];
                pool_id.copy_from_slice(&pool_bytes[..28]);

                ledger_state.pending_retirements.insert(EpochNo(*retirement_epoch), vec![pool_id]);

                tracing::info!("Pool retirement scheduled: {} at epoch {}",
                    hex::encode(&pool_id[..8]),
                    retirement_epoch);
            }
        }

        // Conway-specific governance certificates
        Certificate::Reg(stake_cred, _deposit) |
        Certificate::UnReg(stake_cred, _deposit) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                tracing::debug!("Conway stake registration/deregistration with deposit: {}", hex::encode(&cred_hash[..8]));
            }
            // TODO: Track deposits in DepositTracker
        }

        Certificate::VoteDeleg(_, _) |
        Certificate::StakeVoteDeleg(_, _, _) |
        Certificate::StakeRegDeleg(_, _, _) |
        Certificate::VoteRegDeleg(_, _, _) |
        Certificate::StakeVoteRegDeleg(_, _, _, _) => {
            tracing::debug!("Conway delegation certificate");
            // TODO: Track vote delegations separately from stake delegations
        }

        Certificate::AuthCommitteeHot(_, _) |
        Certificate::ResignCommitteeCold(_, _) => {
            tracing::debug!("Conway committee certificate");
            // TODO: Update governance state
        }

        Certificate::RegDRepCert(_, _, _) |
        Certificate::UnRegDRepCert(_, _) |
        Certificate::UpdateDRepCert(_, _) => {
            tracing::debug!("Conway DRep certificate");
            // TODO: Track DRep registrations and updates
        }

        _ => {}
    }

    Ok(())
}

/// Load genesis files and initialize ledger state with correct reserves
fn load_genesis_and_init_ledger(
    config_path: Option<&PathBuf>,
    network: &Network,
) -> Result<LedgerState> {
    let mut ledger_state = LedgerState::new(ProtocolParameters::default());

    // Try to load genesis files if config is provided or we can find default paths
    if let Some(config_file) = config_path {
        info!("Loading genesis from config: {}", config_file.display());

        // Read the cardano-node config file to get genesis file paths
        let config_dir = config_file.parent().unwrap_or(std::path::Path::new("./"));
        let config_content = std::fs::read_to_string(config_file)
            .with_context(|| format!("Failed to read config file: {}", config_file.display()))?;

        let config: serde_json::Value = serde_json::from_str(&config_content)
            .with_context(|| "Failed to parse config JSON")?;

        // Load Byron genesis (for initial UTxO distribution)
        if let Some(byron_path) = config.get("ByronGenesisFile").and_then(|v| v.as_str()) {
            let full_path = config_dir.join(byron_path);
            match ByronGenesis::load_with_hash(&full_path) {
                Ok((byron_genesis, genesis_hash)) => {
                    let total_lovelace = byron_genesis.total_genesis_lovelace();
                    let k = byron_genesis.security_param();
                    let byron_epoch_length = 10 * k;

                    info!(
                        "✅ Byron genesis loaded: {} lovelace in genesis UTxOs",
                        total_lovelace
                    );

                    // Seed the ledger with genesis UTxOs (subtracts from reserves)
                    ledger_state.seed_genesis_utxos(total_lovelace);
                    ledger_state.set_genesis_hash(genesis_hash);
                    ledger_state.byron_epoch_length = byron_epoch_length;
                }
                Err(e) => {
                    warn!("Failed to load Byron genesis: {}", e);
                }
            }
        }

        // Load Shelley genesis (for epoch length, protocol params, etc.)
        if let Some(shelley_path) = config.get("ShelleyGenesisFile").and_then(|v| v.as_str()) {
            let full_path = config_dir.join(shelley_path);
            match ShelleyGenesis::load_with_hash(&full_path) {
                Ok((shelley_genesis, _hash)) => {
                    ledger_state.set_epoch_length(shelley_genesis.epoch_length);
                    ledger_state.set_update_quorum(shelley_genesis.update_quorum);

                    // Apply protocol parameters from Shelley genesis
                    let pp = &shelley_genesis.protocol_params;
                    ledger_state.update_protocol_params_from_genesis(
                        pp.rho,
                        pp.tau,
                        pp.decentralisation_param,
                        pp.a0,
                        pp.n_opt,
                        pp.min_fee_a,
                        pp.min_fee_b,
                        pp.pool_deposit,
                        pp.key_deposit,
                        pp.min_pool_cost,
                    );

                    info!(
                        "✅ Shelley genesis loaded: epoch_length={}, k={}, d={}",
                        shelley_genesis.epoch_length,
                        shelley_genesis.security_param,
                        pp.decentralisation_param.unwrap_or(0.0)
                    );
                }
                Err(e) => {
                    warn!("Failed to load Shelley genesis: {:?}", e);
                    // Print full error chain for debugging
                    let mut source = e.source();
                    while let Some(err) = source {
                        warn!("  caused by: {}", err);
                        source = err.source();
                    }
                }
            }
        }

        // Alonzo and Conway genesis can be loaded here in the future for:
        // - Plutus cost models (Alonzo)
        // - Governance parameters (Conway)

    } else {
        warn!("No config file provided - using default mainnet values");
        warn!("⚠️  Treasury and reserves will be incorrect!");
        warn!("⚠️  Provide --config <path> to load proper genesis values");
    }

    Ok(ledger_state)
}
