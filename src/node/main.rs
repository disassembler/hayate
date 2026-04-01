// Hayate-Node (疾風ノード) - Full Cardano node with ledger state snapshots

use anyhow::{Context, Result};
use clap::Parser;
use hermod::forwarder::{ForwarderConfig, TraceForwarder};
use hermod::tracer::TracerBuilder;
use std::path::PathBuf;
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::prelude::*;

// Import from lib
use hayate::chain_sync::HayateSync;
use hayate::genesis::{ByronGenesis, ConwayGenesis, ShelleyGenesis};
use hayate::indexer::Network;
use hayate::ledger::{
    primitives::{EpochNo, ProtocolParameters},
    state::LedgerState,
};
use hayate::node::storage::{NodeStorage, UtxoEntry};
use pallas_crypto::hash::Hash;
use pallas_crypto::nonce::generate_rolling_nonce;
use pallas_hardano::storage::immutable as imm_db;
use pallas_network::miniprotocols::chainsync::NextResponse;
use pallas_network::miniprotocols::Point;

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

    /// Directory to dump epoch state JSON files (for comparison with Haskell cardano-node)
    #[arg(long)]
    dump_epoch_dir: Option<PathBuf>,

    /// Directory containing Haskell cardano-node epoch reference dumps.
    /// When set, Hayate compares its epoch dump against the reference after each transition.
    /// If treasury, reserves, or activeStake diverge the node writes the dump and exits.
    #[arg(long)]
    haskell_epoch_dir: Option<PathBuf>,

    /// Wipe all ledger state and re-sync from genesis.
    /// Use this when genesis parameters have changed or state is known to be corrupt.
    #[arg(long, default_value_t = false)]
    reset_genesis: bool,

    /// Restore ledger state from a specific epoch snapshot and re-sync from that point.
    ///
    /// Use this to roll back after a divergence is detected.  Typical workflow:
    ///   1. Run compare-epoch-dumps and find the first epoch with any WARNING.
    ///   2. Subtract 2:  TARGET = first_warning_epoch - 2
    ///   3. Delete hayate dump files for epochs >= TARGET so they regenerate.
    ///   4. Restart with --restore-from-epoch TARGET
    ///
    /// Snapshots are kept for the last 5 epochs.  Hard-fork boundary snapshots
    /// (e.g. epoch 492 on sanchonet) are never pruned so they are always available.
    #[arg(long)]
    restore_from_epoch: Option<u64>,

    /// Path to a Cardano node immutable database directory (offline sync).
    ///
    /// When set, Hayate reads blocks directly from the immutable DB files on disk
    /// instead of (or before) connecting to a live node socket.  A --config file
    /// is still required for genesis initialisation when starting from scratch.
    /// After exhausting the immutable DB, Hayate connects to --socket if one is
    /// given, or exits otherwise.
    #[arg(long)]
    immutable_db: Option<PathBuf>,

    /// Unix socket path for forwarding traces to cardano-tracer.
    ///
    /// When set, structured trace objects are forwarded to cardano-tracer in addition
    /// to the normal stdout log output.  If not set, only stdout logging is active.
    #[arg(long)]
    tracer_socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    // RUST_LOG overrides the defaults; if unset, fall back to info for both crates.
    let env_filter = std::env::var("RUST_LOG")
        .map(|_| tracing_subscriber::EnvFilter::from_default_env())
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("hayate_node=info,hayate=info"));

    // Optionally forward traces to cardano-tracer via a Unix socket.
    // None = no-op layer (only stdout logging active).
    let tracer_layer = args.tracer_socket.as_ref().map(|socket| {
        let fwd_config = ForwarderConfig {
            socket_path: socket.clone(),
            ..Default::default()
        };
        let (layer, _fwd_task) = TracerBuilder::new(TraceForwarder::new(fwd_config))
            .with_namespace_prefix(vec!["Hayate".into()])
            .build();
        // _fwd_task is a JoinHandle; dropping it detaches (task keeps running)
        layer
    });

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .with(tracer_layer)
        .init();

    info!("疾風ノード Hayate-Node starting...");

    // Parse network
    let network = Network::parse(&args.network)
        .ok_or_else(|| anyhow::anyhow!("Invalid network: {}", args.network))?;

    info!("Network: {}", network.as_str());
    info!("Database: {}", args.db_path);

    // Wipe state if --reset-genesis was requested
    if args.reset_genesis {
        let network_db = PathBuf::from(&args.db_path)
            .join("node")
            .join(network.as_str());
        if network_db.exists() {
            info!(
                "⚠️  --reset-genesis: wiping ledger state at {}",
                network_db.display()
            );
            std::fs::remove_dir_all(&network_db)
                .with_context(|| format!("Failed to remove {}", network_db.display()))?;
            info!("✅ Ledger state wiped. Re-syncing from genesis.");
        } else {
            info!(
                "--reset-genesis: no existing state at {}, nothing to wipe",
                network_db.display()
            );
        }
    }

    // Open node storage
    let mut storage = NodeStorage::open(PathBuf::from(&args.db_path), network.clone())?;

    // Determine magic
    let magic = args.magic.unwrap_or_else(|| network.magic());

    info!("Network magic: {}", magic);

    // Initialize or restore ledger state and determine chain-sync start point.
    //
    // Priority (highest first):
    //   1. --restore-from-epoch N  — roll back to a specific epoch snapshot
    //   2. latest snapshot          — resume from the most recent epoch boundary
    //   3. genesis                  — first run or after --reset-genesis
    let ((mut ledger_state, conway_genesis), start_point, resume_slot_hash) =
        if let Some(target_epoch) = args.restore_from_epoch {
            let (state, slot, hash) = storage
                .restore_epoch_snapshot(target_epoch)
                .with_context(|| format!("Failed to restore from epoch {}", target_epoch))?;
            let epoch = state.epoch.0;
            info!(
                "⏪ Rolled back to epoch {} snapshot (slot {}). \
                 Re-syncing from that point forward.",
                epoch, slot
            );
            if let Some(ref dump_dir) = args.dump_epoch_dir {
                info!(
                    "  Tip: delete dump files for epochs >= {} in {} so they regenerate.",
                    epoch,
                    dump_dir.display()
                );
            }
            let conway = load_conway_genesis(args.config.as_ref());
            (
                (state, conway),
                Point::Specific(slot, hash.to_vec()),
                Some((slot, hash)),
            )
        } else {
            match storage.restore_latest_snapshot()? {
                Some((state, slot, hash)) => {
                    let epoch = state.epoch.0;
                    info!(
                        "Restored ledger state from epoch {} (resuming from slot {})",
                        epoch, slot
                    );
                    let conway = load_conway_genesis(args.config.as_ref());
                    (
                        (state, conway),
                        Point::Specific(slot, hash.to_vec()),
                        Some((slot, hash)),
                    )
                }
                None => {
                    info!("🆕 No epoch snapshot found, initializing from genesis");
                    let result = load_genesis_and_init_ledger(args.config.as_ref(), &network)?;
                    // Seed genesis UTxOs into the LSM tree (fresh run only).
                    // This must happen after storage is open but before any blocks are processed.
                    seed_genesis_utxos_into_storage(args.config.as_ref(), &mut storage)?;
                    (result, Point::Origin, None)
                }
            }
        };

    // Shared processing state — used by both immutable-DB and network sync loops.
    let mut blocks_processed = 0u64;
    let mut current_epoch = ledger_state.epoch.0;
    let mut epoch_tx_count: u64 = 0;
    let mut epoch_update_proposals: Vec<(u16, u16, u8)> = Vec::new();

    // Track the last block processed. At epoch transitions, last_slot/last_hash hold
    // the slot and hash of the final block of the ending epoch — these are the correct
    // resume point for save_epoch_snapshot (the current block hasn't been processed yet).
    let (mut last_slot, mut last_hash): (u64, [u8; 32]) =
        resume_slot_hash.unwrap_or((0, [0u8; 32]));

    // Rolling nonce for epoch nonce calculation
    let mut rolling_nonce: Option<Hash<32>> = None;

    info!(
        "🔄 Starting block processing from epoch {}...",
        current_epoch
    );

    // ── Immutable DB sync ────────────────────────────────────────────────────
    if let Some(ref imm_dir) = args.immutable_db {
        info!("📂 Syncing from immutable DB at {}", imm_dir.display());

        match imm_db::get_tip(imm_dir) {
            Ok(Some(Point::Specific(tip_slot, _))) => {
                info!("📂 Immutable DB tip: slot {}", tip_slot)
            }
            Ok(_) => info!("📂 Immutable DB appears empty"),
            Err(e) => return Err(anyhow::anyhow!("Cannot read immutable DB tip: {e}")),
        }

        let mut iter = imm_db::read_blocks_from_point(imm_dir, start_point.clone())
            .context("Failed to open immutable DB iterator")?;

        // read_blocks_from_point is inclusive: the first block returned IS the
        // start_point block, which the restored ledger state has already processed.
        // Skip it to match socket chainsync semantics (find_intersect positions
        // the cursor AFTER the intersection block).
        if matches!(start_point, Point::Specific(_, _)) {
            iter.next(); // discard the already-processed start block
        }

        for block_result in iter {
            let block_bytes = block_result.context("Error reading block from immutable DB")?;
            process_block_bytes(
                &block_bytes,
                &mut storage,
                &mut ledger_state,
                &conway_genesis,
                args.dump_epoch_dir.as_deref(),
                args.haskell_epoch_dir.as_deref(),
                &mut rolling_nonce,
                &mut current_epoch,
                &mut epoch_tx_count,
                &mut epoch_update_proposals,
                &mut last_slot,
                &mut last_hash,
                &mut blocks_processed,
            )
            .await;
        }

        info!(
            "✅ Immutable DB sync complete — epoch {}, {} blocks processed",
            current_epoch, blocks_processed
        );
    }

    // ── Network sync (optional after immutable DB, or standalone) ───────────
    // Resolve socket path; if not available and no network sync is needed, exit.
    let socket_path_opt: Option<String> = if let Some(socket) = args.socket {
        Some(socket)
    } else {
        match network {
            Network::Preview | Network::Mainnet => Some(
                std::env::var("CARDANO_NODE_SOCKET_PATH")
                    .unwrap_or_else(|_| "./cardano-node/node.socket".to_string()),
            ),
            _ => None,
        }
    };

    let socket_path = match socket_path_opt {
        Some(s) => s,
        None => {
            if args.immutable_db.is_some() {
                // Finished immutable DB sync with no socket to continue from — done.
                return Ok(());
            }
            return Err(anyhow::anyhow!(
                "Please specify --socket for network {}",
                network.as_str()
            ));
        }
    };

    // The network sync resumes from wherever we currently are (either the
    // original start_point if no immutable DB was synced, or the tip of the
    // immutable DB if we just processed it).
    let network_start_point = if last_slot > 0 {
        Point::Specific(last_slot, last_hash.to_vec())
    } else {
        start_point
    };

    info!("Connecting to node socket: {}", socket_path);
    info!("Connecting to chain sync...");
    let mut sync = HayateSync::connect(&socket_path, magic, network_start_point).await?;
    info!("✅ Connected to Cardano node via chain sync");

    let mut awaiting = false;
    loop {
        if awaiting {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            continue;
        }

        match sync.request_next().await? {
            NextResponse::RollForward(block_bytes, _tip) => {
                awaiting = false;
                process_block_bytes(
                    &block_bytes,
                    &mut storage,
                    &mut ledger_state,
                    &conway_genesis,
                    args.dump_epoch_dir.as_deref(),
                    args.haskell_epoch_dir.as_deref(),
                    &mut rolling_nonce,
                    &mut current_epoch,
                    &mut epoch_tx_count,
                    &mut epoch_update_proposals,
                    &mut last_slot,
                    &mut last_hash,
                    &mut blocks_processed,
                )
                .await;
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

/// Process a single raw block through the full ledger pipeline.
///
/// Handles nonce evolution, epoch transitions, epoch snapshots/dumps, and
/// block application.  Shared by both the immutable-DB and network-sync paths.
#[allow(clippy::too_many_arguments)]
async fn process_block_bytes(
    block_bytes: &[u8],
    storage: &mut NodeStorage,
    ledger_state: &mut LedgerState,
    conway_genesis: &Option<ConwayGenesis>,
    dump_epoch_dir: Option<&std::path::Path>,
    haskell_epoch_dir: Option<&std::path::Path>,
    rolling_nonce: &mut Option<Hash<32>>,
    current_epoch: &mut u64,
    epoch_tx_count: &mut u64,
    epoch_update_proposals: &mut Vec<(u16, u16, u8)>,
    last_slot: &mut u64,
    last_hash: &mut [u8; 32],
    blocks_processed: &mut u64,
) {
    match parse_block_with_nonce(block_bytes) {
        Ok(parsed) => {
            // EBBs carry no transactions, no VRF, and are not chain-sync resume
            // points — skip all ledger work.
            if parsed.is_ebb {
                return;
            }

            let slot = parsed.slot;
            let block_hash = parsed.hash;
            let tx_count = parsed.tx_count;
            let t_block = std::time::Instant::now();

            // Update rolling nonce if VRF is present
            if let Some(vrf_output) = parsed.vrf_output {
                if vrf_output.len() >= 32 {
                    match rolling_nonce {
                        Some(prev_nonce) => {
                            *rolling_nonce =
                                Some(generate_rolling_nonce(*prev_nonce, &vrf_output[..32]));
                        }
                        None => {
                            let init_nonce = Hash::<32>::from([0u8; 32]);
                            *rolling_nonce =
                                Some(generate_rolling_nonce(init_nonce, &vrf_output[..32]));
                            info!(
                                "🔐 Initializing rolling nonce from first VRF at slot {}",
                                slot
                            );
                        }
                    }
                } else {
                    warn!("VRF output too short: {} bytes", vrf_output.len());
                }
            }

            // Accumulate Byron update proposals and detect the Shelley HF proposal.
            if let Some(bv) = parsed.byron_update {
                epoch_update_proposals.push(bv);
                if bv == (2, 0, 0) {
                    ledger_state.record_shelley_hf_proposal(*current_epoch);
                    info!(
                        "🔀 Shelley HF update proposal seen in epoch {} — \
                           transition at epoch {}",
                        *current_epoch,
                        *current_epoch + 1
                    );
                }
            }

            // Check for epoch transition BEFORE processing the block.
            // This ensures the mark snapshot captures only the previous epoch's
            // blocks. If the block were counted first, the snapshot would include
            // the first block of the new epoch — causing a 1-block misattribution.
            let epoch = ledger_state.epoch_of_slot(slot);

            // Detect first Conway block and apply Conway genesis BEFORE the epoch
            // transition. This ensures the epoch dump shows Conway state.
            if parsed.is_conway && ledger_state.conway_genesis_epoch.is_none() {
                if let Some(ref cg) = conway_genesis {
                    ledger_state.apply_conway_genesis(cg, EpochNo(epoch));
                    info!("🌅 Conway genesis applied at epoch {} slot {}", epoch, slot);
                } else {
                    warn!("⚠️  First Conway block detected but no Conway genesis loaded!");
                }
            }

            // When we transition to a new epoch, snapshot the previous epoch
            if epoch > *current_epoch {
                if ledger_state.is_shelley_plus_epoch(epoch) {
                    let epoch_fees_ada = ledger_state.epoch_fees.0 / 1_000_000;
                    let old_era = era_name(ledger_state.protocol_params.protocol_version_major);

                    // Capture enactments before process_epoch_transition consumes them
                    let pending_enactments: Vec<_> = ledger_state
                        .pending_enactments
                        .iter()
                        .map(|e| describe_gov_action(&e.gov_action, &ledger_state.protocol_params))
                        .collect();

                    // 1. Rebuild stake distribution from the incrementally-maintained stake map.
                    // NodeStorage::current_stake is updated on every insert/remove, so this is
                    // O(credentials) rather than O(all_utxos). The full UTxO scan is only needed
                    // once after snapshot restoration (where current_stake is cleared).
                    let t_stake = std::time::Instant::now();
                    ledger_state.rebuild_stake_from_current_stake(storage.current_stake());
                    let stake_ms = t_stake.elapsed().as_millis() as u64;

                    // 1a. At the Byron→Shelley boundary, recalibrate reserves from the
                    // actual UTxO total.  Our genesis seed used the initial lovelace
                    // allocation; Byron fees reduce the UTxO set, so reserves must be
                    // recomputed as (maxLovelaceSupply - totalUTxOValue).
                    if !ledger_state.is_shelley_plus_epoch(*current_epoch) {
                        if let Err(e) =
                            ledger_state.recalibrate_reserves_from_utxo_tree(&storage.utxo_tree)
                        {
                            error!("Failed to recalibrate reserves at Shelley HF: {}", e);
                        }
                    }

                    // 2. Process epoch transition in ledger state
                    let t_ledger = std::time::Instant::now();
                    ledger_state.process_epoch_transition(EpochNo(epoch));
                    let ledger_ms = t_ledger.elapsed().as_millis() as u64;

                    // 2a. Dump epoch state to JSON for comparison with Haskell
                    if let Some(dump_dir) = dump_epoch_dir {
                        if let Err(e) = ledger_state.dump_epoch_state(dump_dir, slot) {
                            error!("Failed to dump epoch state: {}", e);
                        }

                        // Mismatch detection: compare against Haskell reference if provided
                        if let Some(haskell_dir) = haskell_epoch_dir {
                            let epoch_num = ledger_state.epoch.0;
                            let hayate_dump = dump_dir.join(format!("{}-hayate.json", epoch_num));
                            if let Some(haskell_dump) = find_haskell_dump(haskell_dir, epoch_num) {
                                match compare_epoch_dumps(
                                    &hayate_dump,
                                    &haskell_dump,
                                    haskell_dir,
                                    epoch_num,
                                ) {
                                    Ok(true) => {
                                        debug!("Epoch {} matches Haskell reference", epoch_num)
                                    }
                                    Ok(false) => {
                                        error!("EPOCH {} DIVERGED", epoch_num);
                                        std::process::exit(1);
                                    }
                                    Err(e) => {
                                        warn!("Could not compare epoch {} dumps: {}", epoch_num, e)
                                    }
                                }
                            }
                        }
                    }

                    // Two-line epoch boundary log:
                    //   Line 1: summary of the epoch that just ended (fees, txs)
                    //   Line 2: opening state of the new epoch (treasury, reserves, rewards)
                    let new_era = era_name(ledger_state.protocol_params.protocol_version_major);
                    let treasury_ada = ledger_state.treasury.0 / 1_000_000;
                    let reserves_ada = ledger_state.reserves.0 / 1_000_000;
                    let rewards_ada = ledger_state
                        .last_applied_rupd
                        .as_ref()
                        .map(|r| r.total_distributed / 1_000_000)
                        .unwrap_or(0);
                    if ledger_state.is_shelley_plus_epoch(*current_epoch) {
                        info!(
                            "Epoch {} ({}) ended  txs: {}  fees: {} ADA",
                            current_epoch, old_era, epoch_tx_count, epoch_fees_ada
                        );
                    } else {
                        info!(
                            "Epoch {} (Byron) ended  txs: {}",
                            current_epoch, epoch_tx_count
                        );
                    }
                    for desc in &pending_enactments {
                        info!("  enacted: {desc}");
                    }
                    info!(
                        "Epoch {} ({}) treasury={} ADA  reserves={} ADA  rewards={} ADA",
                        epoch, new_era, treasury_ada, reserves_ada, rewards_ada
                    );
                    if let Some(msg) = ledger_state.ppup_enacted_log.take() {
                        info!("{}", msg);
                    }
                    info!(
                        epoch = *current_epoch,
                        stake_ms,
                        ledger_ms,
                        "epoch transition timing"
                    );
                } else {
                    // Byron epoch summary — no ledger transition, just log.
                    // Use `epoch - 1` (the epoch that just ended) for the label,
                    // which is correct regardless of what current_epoch holds on
                    // resume from an old Byron snapshot.
                    info!(
                        "Epoch {} (Byron) slot={}  txs: {}",
                        epoch.saturating_sub(1),
                        slot,
                        epoch_tx_count
                    );
                    for &(mj, mn, pat) in epoch_update_proposals.iter() {
                        if (mj, mn, pat) == (2, 0, 0) {
                            info!("  Update proposal: Shelley HF ({}.{}.{})", mj, mn, pat);
                        } else {
                            info!("  Update proposal: noop ({}.{}.{})", mj, mn, pat);
                        }
                    }
                    // Keep ledger_state.epoch in sync so that restore gives the
                    // right current_epoch without needing process_epoch_transition.
                    ledger_state.epoch = EpochNo(epoch);
                }

                // Save epoch snapshot (UTxO hard-links + bincode file).
                // last_slot/last_hash = final block of the ending epoch
                // (the current block hasn't been processed yet at this point).
                if let Err(e) =
                    storage.save_epoch_snapshot(epoch, *last_slot, *last_hash, ledger_state)
                {
                    error!("Failed to save epoch {} snapshot: {}", epoch, e);
                }

                *epoch_tx_count = 0;
                epoch_update_proposals.clear();
                *current_epoch = epoch;
            }

            // Process block AFTER epoch transition so the first block of a new
            // epoch is counted towards the new epoch, not the previous one.
            // Byron regular blocks are also processed here to track UTxOs.
            let (decode_us, remove_us, insert_us, certs_us) =
                match process_block_simple(storage, ledger_state, slot, &block_hash, block_bytes).await {
                    Ok(t) => t,
                    Err(e) => {
                        error!("Error processing block at slot {}: {}", slot, e);
                        return;
                    }
                };

            *blocks_processed += 1;
            *epoch_tx_count += tx_count as u64;

            if *blocks_processed % 1000 == 0 {
                info!(
                    target: "Hayate.Sync",
                    slot,
                    epoch = *current_epoch,
                    blocks_total = *blocks_processed,
                    "sync.progress"
                );
            }

            let (slot_off, epoch_len) = ledger_state.slot_within_epoch(slot);
            debug!(slot, elapsed_us = t_block.elapsed().as_micros() as u64, decode_us, remove_us, insert_us, certs_us, "block");
            trace!(
                "block slot={} ({}/{}) epoch={} txs={}",
                slot,
                slot_off,
                epoch_len,
                epoch,
                tx_count
            );

            // Track last processed block for epoch snapshot resume point.
            *last_slot = slot;
            let n = block_hash.len().min(32);
            last_hash[..n].copy_from_slice(&block_hash[..n]);
        }
        Err(e) => {
            error!("Failed to parse block: {}", e);
        }
    }
}

fn era_name(protocol_version_major: u64) -> &'static str {
    match protocol_version_major {
        0 | 1 => "Byron",
        2 => "Shelley",
        3 => "Allegra",
        4 => "Mary",
        5 | 6 => "Alonzo",
        7 | 8 => "Babbage",
        9..=11 => "Conway",
        12 => "Dijkstra",
        _ => "Unknown",
    }
}

fn describe_gov_action(
    action: &hayate::ledger::primitives::GovernanceAction,
    params: &hayate::ledger::primitives::ProtocolParameters,
) -> String {
    use hayate::ledger::primitives::{GovernanceAction::*, Rational};

    fn fmt_rat(r: Rational) -> String {
        format!("{}/{}", r.numerator, r.denominator)
    }

    match action {
        HardForkInitiation {
            protocol_version: (major, minor),
            ..
        } => {
            let from_era = era_name(params.protocol_version_major);
            let to_era = era_name(*major);
            if from_era != to_era {
                format!("HardFork {from_era}→{to_era}")
            } else {
                format!(
                    "HardFork {from_era} v{}.{}→v{major}.{minor}",
                    params.protocol_version_major, params.protocol_version_minor
                )
            }
        }
        ParameterChange { update, .. } => {
            let mut changes: Vec<String> = Vec::new();

            macro_rules! chg_u64 {
                ($upd:expr, $cur:expr, $name:literal) => {
                    if let Some(new) = $upd {
                        if new != $cur {
                            changes.push(format!("{}: {}→{}", $name, $cur, new));
                        }
                    }
                };
            }
            macro_rules! chg_rat {
                ($upd:expr, $cur:expr, $name:literal) => {
                    if let Some(new) = $upd { if new != $cur { changes.push(format!("{}: {}→{}", $name, fmt_rat($cur), fmt_rat(new))); } }
                }
            }

            chg_u64!(update.min_fee_a, params.min_fee_a, "minFeeA");
            chg_u64!(update.min_fee_b, params.min_fee_b, "minFeeB");
            chg_u64!(update.n_opt, params.n_opt, "nOpt");
            chg_u64!(update.key_deposit, params.key_deposit, "keyDeposit");
            chg_u64!(update.pool_deposit, params.pool_deposit, "poolDeposit");
            chg_u64!(update.e_max, params.e_max, "eMax");
            chg_u64!(update.min_pool_cost, params.min_pool_cost, "minPoolCost");
            chg_rat!(update.rho, params.rho, "rho");
            chg_rat!(update.tau, params.tau, "tau");
            chg_rat!(update.a0, params.a0, "a0");
            chg_u64!(update.drep_deposit, params.drep_deposit, "drepDeposit");
            chg_u64!(
                update.drep_activity,
                params.drep_activity_period,
                "drepActivity"
            );
            chg_u64!(
                update.gov_action_lifetime,
                params.gov_action_lifetime,
                "govActionLifetime"
            );
            chg_u64!(
                update.gov_action_deposit,
                params.gov_action_deposit,
                "govActionDeposit"
            );
            chg_u64!(
                update.committee_min_size,
                params.committee_min_size,
                "committeeMinSize"
            );
            chg_u64!(
                update.committee_max_term_length,
                params.committee_max_term_length,
                "committeeMaxTermLength"
            );
            if let Some((mj, mn)) = update.protocol_version {
                if mj != params.protocol_version_major || mn != params.protocol_version_minor {
                    changes.push(format!(
                        "protocolVersion: v{}.{}→v{}.{}",
                        params.protocol_version_major, params.protocol_version_minor, mj, mn
                    ));
                }
            }

            // Count threshold changes without listing each one
            let threshold_fields: &[(Option<Rational>, Rational)] = &[
                (
                    update.dvt_motion_no_confidence,
                    params.dvt_motion_no_confidence,
                ),
                (update.dvt_committee_normal, params.dvt_committee_normal),
                (
                    update.dvt_committee_no_confidence,
                    params.dvt_committee_no_confidence,
                ),
                (update.dvt_hard_fork_initiation, params.dvt_hard_fork),
                (update.dvt_pp_network_group, params.dvt_pp_network_group),
                (update.dvt_pp_economic_group, params.dvt_pp_economic_group),
                (update.dvt_pp_technical_group, params.dvt_pp_technical_group),
                (update.dvt_pp_gov_group, params.dvt_pp_gov_group),
                (
                    update.dvt_treasury_withdrawal,
                    params.dvt_treasury_withdrawal,
                ),
                (
                    update.pvt_motion_no_confidence,
                    params.pvt_motion_no_confidence,
                ),
                (update.pvt_committee_normal, params.pvt_committee_normal),
                (
                    update.pvt_committee_no_confidence,
                    params.pvt_committee_no_confidence,
                ),
                (update.pvt_hard_fork_initiation, params.pvt_hard_fork),
                (update.pvt_pp_security_group, params.pvt_pp_security_group),
            ];
            let n_threshold_changes = threshold_fields
                .iter()
                .filter(|(new, old)| new.is_some_and(|v| v != *old))
                .count();
            if n_threshold_changes > 0 {
                changes.push(format!("{n_threshold_changes} voting threshold(s)"));
            }

            if changes.is_empty() {
                "ParameterChange (no fields changed)".to_string()
            } else {
                format!("ParameterChange: {}", changes.join(", "))
            }
        }
        TreasuryWithdrawals { withdrawals, .. } => {
            let total_ada: u64 = withdrawals.iter().map(|(_, l)| l.0).sum::<u64>() / 1_000_000;
            format!(
                "TreasuryWithdrawals ({} recipients, {} ADA)",
                withdrawals.len(),
                total_ada
            )
        }
        NoConfidence { .. } => "NoConfidence".to_string(),
        UpdateCommittee {
            members_to_add,
            members_to_remove,
            ..
        } => format!(
            "UpdateCommittee (+{} -{} members)",
            members_to_add.len(),
            members_to_remove.len()
        ),
        NewConstitution { .. } => "NewConstitution".to_string(),
        InfoAction => "InfoAction".to_string(),
    }
}

/// Find the Haskell reference dump file for a given epoch.
///
/// Haskell dumps are named `"{epoch}-{slot}.json"` (e.g. `"492-12345678.json"`).
fn find_haskell_dump(dir: &std::path::Path, epoch: u64) -> Option<PathBuf> {
    let prefix = format!("{}-", epoch);
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(&prefix) && n.ends_with(".json"))
                .unwrap_or(false)
        })
}

/// Compare critical fields between a Hayate dump and a Haskell reference dump.
/// Returns `Ok(true)` if all fields match, `Ok(false)` if any diverge, `Err` if
/// either file cannot be read or parsed.
fn compare_epoch_dumps(
    hayate_path: &std::path::Path,
    haskell_path: &std::path::Path,
    haskell_dir: &std::path::Path,
    epoch_num: u64,
) -> anyhow::Result<bool> {
    use serde_json::Value;

    let hayate: Value = serde_json::from_str(&std::fs::read_to_string(hayate_path)?)?;
    let haskell: Value = serde_json::from_str(&std::fs::read_to_string(haskell_path)?)?;

    let mut criticals: Vec<String> = Vec::new();

    // Helper: compare u64 field with ADA display
    let mut cmp_u64 = |key: &str, hv: Option<u64>, rv: Option<u64>| match (hv, rv) {
        (Some(h), Some(r)) if h != r => {
            let diff = r as i128 - h as i128;
            criticals.push(format!(
                "{key}: haskell={h} hayate={r} diff={diff} ({:.3} ADA)",
                diff as f64 / 1_000_000.0
            ));
        }
        (Some(h), None) => criticals.push(format!("{key}: haskell={h} hayate=missing")),
        (None, Some(r)) => criticals.push(format!("{key}: haskell=missing hayate={r}")),
        _ => {}
    };

    let get_u64 = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_u64());

    cmp_u64(
        "treasury",
        get_u64(&haskell, "treasury"),
        get_u64(&hayate, "treasury"),
    );
    cmp_u64(
        "reserves",
        get_u64(&haskell, "reserves"),
        get_u64(&hayate, "reserves"),
    );
    cmp_u64(
        "activeStake",
        get_u64(&haskell, "activeStake"),
        get_u64(&hayate, "activeStake"),
    );

    // drepDistr total stake
    let drep_total = |v: &Value| -> u64 {
        v.pointer("/conwayGov/drepDistr")
            .and_then(|d| d.as_object())
            .map(|m| m.values().filter_map(|x| x.as_u64()).sum())
            .unwrap_or(0)
    };
    let (hd, rd) = (drep_total(&haskell), drep_total(&hayate));
    cmp_u64("conwayGov.drepDistr total stake", Some(hd), Some(rd));

    // Snapshots: compare mark/set/go per-credential stake and totalStake
    {
        fn normalize_cred(s: &str) -> String {
            let hex_part = s.strip_prefix("keyHash-").unwrap_or(s);
            format!("keyHash-{}", &hex_part[..56.min(hex_part.len())])
        }
        fn snapshot_stake_total(snap: &Value) -> u64 {
            snap.get("stake")
                .and_then(|s| s.as_object())
                .map(|m| m.values().filter_map(|v| v.as_u64()).sum())
                .unwrap_or(0)
        }
        for sname in &["mark", "set", "go"] {
            let hs = haskell.get("snapshots").and_then(|s| s.get(sname));
            let rs = hayate.get("snapshots").and_then(|s| s.get(sname));
            match (hs, rs) {
                (Some(h), Some(r)) if !h.is_null() && !r.is_null() => {
                    let ht = snapshot_stake_total(h);
                    let rt = snapshot_stake_total(r);
                    if ht != rt {
                        let diff = rt as i128 - ht as i128;
                        criticals.push(format!(
                            "snapshots.{sname}.totalStake: haskell={ht} hayate={rt} diff={diff} ({:.3} ADA)",
                            diff as f64 / 1_000_000.0
                        ));
                    }
                    let hstake: std::collections::HashMap<String, u64> = h
                        .get("stake")
                        .and_then(|s| s.as_object())
                        .map(|m| {
                            m.iter()
                                .map(|(k, v)| (normalize_cred(k), v.as_u64().unwrap_or(0)))
                                .collect()
                        })
                        .unwrap_or_default();
                    let rstake: std::collections::HashMap<String, u64> = r
                        .get("stake")
                        .and_then(|s| s.as_object())
                        .map(|m| {
                            m.iter()
                                .map(|(k, v)| (normalize_cred(k), v.as_u64().unwrap_or(0)))
                                .collect()
                        })
                        .unwrap_or_default();
                    for (cred, hv) in &hstake {
                        let rv = rstake.get(cred).copied().unwrap_or(0);
                        if *hv != rv {
                            criticals.push(format!(
                                "snapshots.{sname}.stake[{cred}]: haskell={hv} hayate={rv}"
                            ));
                        }
                    }
                    for (cred, rv) in &rstake {
                        if !hstake.contains_key(cred) {
                            criticals.push(format!(
                                "snapshots.{sname}.stake[{cred}]: missing in haskell, hayate={rv}"
                            ));
                        }
                    }
                }
                (Some(_), None) => criticals.push(format!("snapshots.{sname}: missing in hayate")),
                _ => {}
            }
        }
    }

    // RUPD: compare haskell[N-1].rupdNext vs hayate[N].rupd
    if epoch_num > 0 {
        if let Some(prev_haskell_path) = find_haskell_dump(haskell_dir, epoch_num - 1) {
            if let Ok(prev_haskell) = std::fs::read_to_string(&prev_haskell_path)
                .and_then(|s| Ok(serde_json::from_str::<Value>(&s)?))
            {
                let hr = prev_haskell.get("rupdNext").filter(|v| !v.is_null());
                let rr = hayate.get("rupd").filter(|v| !v.is_null());
                if let (Some(hr), Some(rr)) = (hr, rr) {
                    let prefix = format!(
                        "rupdNext (haskell[{}] vs hayate[{epoch_num}])",
                        epoch_num - 1
                    );
                    for key in &[
                        "deltaR1",
                        "deltaR2",
                        "deltaT1",
                        "rPot",
                        "rewardPot",
                        "totalDistributed",
                    ] {
                        let hv = hr.get(key).and_then(|v| v.as_u64());
                        let rv = rr.get(key).and_then(|v| v.as_u64());
                        match (hv, rv) {
                            (Some(h), Some(r)) if h != r => {
                                let diff = r as i128 - h as i128;
                                criticals.push(format!(
                                    "{prefix}.{key}: haskell={h} hayate={r} diff={diff} ({:.3} ADA)",
                                    diff as f64 / 1_000_000.0
                                ));
                            }
                            (None, Some(r)) => criticals
                                .push(format!("{prefix}.{key}: missing in haskell, hayate={r}")),
                            (Some(h), None) => criticals
                                .push(format!("{prefix}.{key}: haskell={h}, missing in hayate")),
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    for msg in &criticals {
        error!("  CRITICAL: {msg}");
    }

    Ok(criticals.is_empty())
}

struct ParsedBlock {
    slot: u64,
    hash: Vec<u8>,
    tx_count: usize,
    vrf_output: Option<Vec<u8>>,
    is_conway: bool,
    /// True for Byron Epoch Boundary Blocks (EBBs) — no txs, no VRF, skip ledger work.
    is_ebb: bool,
    /// Proposed block version from a Byron update proposal, if present.
    byron_update: Option<(u16, u16, u8)>,
}

/// Parse a raw block and extract fields needed by the processing pipeline.
fn parse_block_with_nonce(block_bytes: &[u8]) -> Result<ParsedBlock> {
    use pallas_traverse::MultiEraBlock;

    let block = MultiEraBlock::decode(block_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to decode block: {}", e))?;

    // Epoch Boundary Blocks carry no transactions, no VRF, and are not
    // meaningful resume points — return early with a sentinel.
    if matches!(block, MultiEraBlock::EpochBoundary(_)) {
        return Ok(ParsedBlock {
            slot: block.slot(),
            hash: block.hash().to_vec(),
            tx_count: 0,
            vrf_output: None,
            is_conway: false,
            is_ebb: true,
            byron_update: None,
        });
    }

    let slot = block.slot();
    let hash = block.hash().to_vec();
    let tx_count = block.txs().len();

    if tx_count > 0 {
        tracing::debug!(
            "Found block with {} transactions at slot {} (block_size={} bytes)",
            tx_count,
            slot,
            block_bytes.len()
        );
    }

    let is_conway = block.as_conway().is_some();

    // Extract proposed block version from a Byron update proposal, if present.
    let byron_update = block
        .update()
        .and_then(|u| u.byron_proposed_block_version());

    // Extract VRF output from block header
    let vrf_output: Option<Vec<u8>> = if let Some(conway_block) = block.as_conway() {
        let vrf_cert = &conway_block.header.header_body.vrf_result;
        Some(vrf_cert.0.to_vec())
    } else if let Some(babbage_block) = block.as_babbage() {
        let vrf_cert = &babbage_block.header.header_body.vrf_result;
        Some(vrf_cert.0.to_vec())
    } else if let Some(alonzo_block) = block.as_alonzo() {
        let vrf_cert = &alonzo_block.header.header_body.nonce_vrf;
        Some(vrf_cert.0.to_vec())
    } else {
        // Byron regular blocks don't have VRF
        None
    };

    Ok(ParsedBlock {
        slot,
        hash,
        tx_count,
        vrf_output,
        is_conway,
        is_ebb: false,
        byron_update,
    })
}

async fn process_block_simple(
    storage: &mut NodeStorage,
    ledger_state: &mut LedgerState,
    slot: u64,
    _block_hash: &[u8],
    block_bytes: &[u8],
) -> Result<(u64, u64, u64, u64)> {
    use pallas_traverse::MultiEraBlock;

    use std::sync::Arc;

    let t_decode = std::time::Instant::now();
    let block = MultiEraBlock::decode(block_bytes)?;
    let decode_us = t_decode.elapsed().as_micros() as u64;

    // Track total block count for epoch (needed for monetary expansion calculation)
    ledger_state.epoch_block_count += 1;

    // ── Byron fast-path ───────────────────────────────────────────────────────
    // Byron blocks have no pools, no certificates, no multi-assets, no datums,
    // and no stake credentials — only plain ADA UTxOs.  Skip all Shelley+ work.
    if block.as_byron().is_some() {
        for tx in block.txs() {
            let tx_hash = tx.hash();
            for input in tx.inputs() {
                storage.remove_utxo_blind(input.hash().as_ref(), input.index() as u32)?;
            }
            for (idx, output) in tx.outputs().into_iter().enumerate() {
                let address_bytes = output.address()?.to_vec();
                let amount = output.value().coin();
                let utxo_entry = UtxoEntry {
                    address: address_bytes,
                    amount,
                    assets: std::collections::HashMap::new(),
                    datum_hash: None,
                    datum: None,
                    script_ref: None,
                    stake_credential: None,
                };
                storage.insert_utxo(tx_hash.as_ref(), idx as u32, &utxo_entry)?;
            }
            ledger_state.epoch_fees.0 += tx.fee().unwrap_or(0);
        }
        return Ok((decode_us, 0, 0, 0));
    }

    // Track block for leader schedule (pool that produced this block).
    // In Haskell, nesBlocksCur accumulates ALL blocks from ANY stake pool, including recently
    // retired ones. The registration check is done only during reward distribution, not here.
    // Counting only registered pools would miss blocks from pools that retired mid-epoch,
    // causing the eta (active slot coefficient) to be under-counted.
    if let Some(pool_id) = extract_pool_id_from_block(&block) {
        *Arc::make_mut(&mut ledger_state.epoch_blocks_by_pool).entry(pool_id).or_insert(0) += 1;
    }

    // Process each transaction
    let txs = block.txs();
    if !txs.is_empty() {
        tracing::debug!(
            "Processing {} transactions in block at slot {}",
            txs.len(),
            slot
        );
    }
    let mut remove_us: u64 = 0;
    let mut insert_us: u64 = 0;
    let mut certs_us: u64 = 0;
    for tx in txs {
        let tx_hash = tx.hash();
        let tx_valid = tx.is_valid();

        if !tx_valid {
            tracing::info!(
                "Phase-2 script failure: tx {} at slot {} (is_valid=false), using collateral",
                hex::encode(tx_hash.as_ref()),
                slot
            );
        }

        // Process consumed UTxOs.
        // For valid txs, consumes() returns inputs(); for invalid txs, it returns collateral().
        // Track consumed values for invalid txs to compute collateral fee.
        let mut consumed_value: u64 = 0;
        for input in tx.consumes() {
            let input_hash = input.hash();
            let input_index = input.index();

            let t = std::time::Instant::now();
            let removed = storage.remove_utxo(input_hash.as_ref(), input_index as u32)?;
            remove_us += t.elapsed().as_micros() as u64;

            if !tx_valid {
                if let Some(entry) = &removed {
                    consumed_value += entry.amount;
                }
            }
        }

        // Process produced UTxOs.
        // For valid txs, produces() returns enumerate(outputs()); for invalid txs,
        // it returns collateral_return (at index = outputs().len()) or empty.
        for (output_index, output) in tx.produces() {
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
                        pallas_traverse::MultiEraAsset::AlonzoCompatibleOutput(_, _, amt) => {
                            amt as u64
                        }
                        pallas_traverse::MultiEraAsset::ConwayOutput(_, _, amt) => u64::from(amt),
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

            let t = std::time::Instant::now();
            storage.insert_utxo(tx_hash.as_ref(), output_index as u32, &utxo_entry)?;
            insert_us += t.elapsed().as_micros() as u64;
        }

        // For valid transactions, process certificates, governance, withdrawals.
        // For invalid transactions (phase-2 script failure), these are all skipped —
        // only UTxO changes (collateral consumed, collateral return produced) and
        // the collateral fee are applied.
        if tx_valid {
            // Process certificates (delegations, pool registrations, etc.)
            let t = std::time::Instant::now();
            for cert in tx.certs() {
                process_certificate(cert, ledger_state, slot)?;
            }
            certs_us += t.elapsed().as_micros() as u64;

            // Extract pre-Conway protocol parameter update proposals (Shelley through Babbage)
            if let Some(update) = tx.update() {
                process_ppup_proposal(update, ledger_state, slot);
            }

            // Process Conway governance proposals (proposal_procedures in tx body)
            // Each submitted proposal pays a deposit; the deposit is refunded when the proposal
            // expires (epoch.rs) to the return_addr's reward account.
            for (proposal_index, proposal) in tx.gov_proposals().into_iter().enumerate() {
                if let Some(conway_proposal) = proposal.as_conway() {
                    let tx_hash_bytes = tx_hash.as_ref();
                    let mut tx_hash_arr = [0u8; 32];
                    let len = tx_hash_bytes.len().min(32);
                    tx_hash_arr[..len].copy_from_slice(&tx_hash_bytes[..len]);

                    let action_id = hayate::ledger::primitives::GovActionId {
                        tx_hash: tx_hash_arr,
                        index: proposal_index as u32,
                    };

                    // Parse return_addr from RewardAccount bytes (header + 28-byte cred hash)
                    let reward_account_bytes: &[u8] = conway_proposal.reward_account.as_ref();
                    let return_addr = if reward_account_bytes.len() >= 29 {
                        let header = reward_account_bytes[0];
                        let is_script = (header & 0x10) != 0;
                        let mut hash = [0u8; 32];
                        hash[..28].copy_from_slice(&reward_account_bytes[1..29]);
                        if is_script {
                            hayate::ledger::primitives::Credential::Script(hash)
                        } else {
                            hayate::ledger::primitives::Credential::Key(hash)
                        }
                    } else {
                        continue;
                    };

                    let deposit = hayate::ledger::primitives::Lovelace(conway_proposal.deposit);

                    // Track deposit for accounting
                    let return_hash = match &return_addr {
                        hayate::ledger::primitives::Credential::Key(h) => *h,
                        hayate::ledger::primitives::Credential::Script(h) => *h,
                    };
                    ledger_state.deposit_tracker.add_deposit(
                        return_hash,
                        hayate::ledger::state::DepositType::Governance(action_id),
                        deposit,
                    );

                    // Map pallas GovAction to hayate GovernanceAction (full parse).
                    let gov_action = {
                        use pallas_primitives::conway::GovAction as PGA;
                        // Convert Option<pallas GovActionId> → Option<hayate GovActionId>
                        let convert_action_id =
                            |opt: &Option<pallas_primitives::conway::GovActionId>| {
                                opt.as_ref().map(|id| {
                                    let mut arr = [0u8; 32];
                                    let b = id.transaction_id.as_ref();
                                    arr[..b.len().min(32)].copy_from_slice(&b[..b.len().min(32)]);
                                    hayate::ledger::primitives::GovActionId {
                                        tx_hash: arr,
                                        index: id.action_index,
                                    }
                                })
                            };
                        match &conway_proposal.gov_action {
                            PGA::ParameterChange(prev_id, update, _guardrail) => {
                                let hayate_update = pallas_ppu_to_hayate(update.as_ref());
                                hayate::ledger::primitives::GovernanceAction::ParameterChange {
                                    prev_action_id: convert_action_id(prev_id),
                                    update: hayate_update,
                                    guardrails_hash: None,
                                }
                            }
                            PGA::HardForkInitiation(prev_id, version) => {
                                hayate::ledger::primitives::GovernanceAction::HardForkInitiation {
                                    prev_action_id: convert_action_id(prev_id),
                                    protocol_version: (version.0, version.1),
                                }
                            }
                            PGA::TreasuryWithdrawals(withdrawals, _guardrail) => {
                                let converted = withdrawals
                                    .iter()
                                    .filter_map(|(acct, amount)| {
                                        let bytes: &[u8] = acct.as_ref();
                                        if bytes.len() >= 29 {
                                            let is_script = (bytes[0] & 0x10) != 0;
                                            let mut hash = [0u8; 32];
                                            hash[..28].copy_from_slice(&bytes[1..29]);
                                            let cred = if is_script {
                                                hayate::ledger::primitives::Credential::Script(hash)
                                            } else {
                                                hayate::ledger::primitives::Credential::Key(hash)
                                            };
                                            Some((cred, hayate::ledger::primitives::Lovelace(*amount)))
                                        } else {
                                            None
                                        }
                                    })
                                    .collect();
                                hayate::ledger::primitives::GovernanceAction::TreasuryWithdrawals {
                                    withdrawals: converted,
                                    guardrails_hash: None,
                                }
                            }
                            PGA::NoConfidence(prev_id) => {
                                hayate::ledger::primitives::GovernanceAction::NoConfidence {
                                    prev_action_id: convert_action_id(prev_id),
                                }
                            }
                            PGA::UpdateCommittee(prev_id, remove, add, threshold) => {
                                let members_to_remove = remove
                                    .iter()
                                    .filter_map(|cred| {
                                        stake_credential_to_hash28(cred).map(|h28| {
                                            let mut hash = [0u8; 32];
                                            hash[..28].copy_from_slice(&h28);
                                            if matches!(
                                                cred,
                                                pallas_primitives::StakeCredential::ScriptHash(_)
                                            ) {
                                                hayate::ledger::primitives::Credential::Script(hash)
                                            } else {
                                                hayate::ledger::primitives::Credential::Key(hash)
                                            }
                                        })
                                    })
                                    .collect();
                                let members_to_add = add
                                    .iter()
                                    .filter_map(|(cred, epoch)| {
                                        stake_credential_to_hash28(cred).map(|h28| {
                                            let mut hash = [0u8; 32];
                                            hash[..28].copy_from_slice(&h28);
                                            let credential = if matches!(
                                                cred,
                                                pallas_primitives::StakeCredential::ScriptHash(_)
                                            ) {
                                                hayate::ledger::primitives::Credential::Script(hash)
                                            } else {
                                                hayate::ledger::primitives::Credential::Key(hash)
                                            };
                                            (credential, hayate::ledger::primitives::EpochNo(*epoch))
                                        })
                                    })
                                    .collect();
                                hayate::ledger::primitives::GovernanceAction::UpdateCommittee {
                                    prev_action_id: convert_action_id(prev_id),
                                    members_to_remove,
                                    members_to_add,
                                    quorum: hayate::ledger::primitives::Rational {
                                        numerator: threshold.numerator,
                                        denominator: threshold.denominator,
                                    },
                                }
                            }
                            PGA::NewConstitution(prev_id, constitution) => {
                                let anchor = Some(hayate::ledger::primitives::Anchor {
                                    url: constitution.anchor.url.clone(),
                                    hash: {
                                        let mut h = [0u8; 32];
                                        let b = constitution.anchor.content_hash.as_ref();
                                        h[..b.len().min(32)].copy_from_slice(&b[..b.len().min(32)]);
                                        h
                                    },
                                });
                                let script_hash = constitution.guardrail_script.map(|h| {
                                    let mut hash = [0u8; 32];
                                    hash[..28].copy_from_slice(h.as_ref());
                                    hash
                                });
                                hayate::ledger::primitives::GovernanceAction::NewConstitution {
                                    prev_action_id: convert_action_id(prev_id),
                                    constitution: hayate::ledger::primitives::Constitution {
                                        anchor,
                                        script_hash,
                                    },
                                }
                            }
                            PGA::Information => {
                                hayate::ledger::primitives::GovernanceAction::InfoAction
                            }
                        }
                    };

                    let procedure = hayate::ledger::primitives::ProposalProcedure {
                        deposit,
                        return_addr,
                        gov_action,
                        anchor: None,
                    };

                    if let Err(e) = ledger_state.process_proposal(&action_id, &procedure) {
                        tracing::debug!("Governance proposal rejected: {}", e);
                    } else {
                        tracing::debug!(
                            "Governance proposal stored: tx={} idx={} deposit={}",
                            hex::encode(&tx_hash_arr[..8]),
                            proposal_index,
                            deposit.0
                        );
                    }
                }
            }

            // Process Conway governance votes and treasury donations (Conway tx body only)
            if let pallas_traverse::MultiEraTx::Conway(conway_tx) = &tx {
                if let Some(voting_procedures) = &conway_tx.transaction_body.voting_procedures {
                    for (pallas_voter, action_votes) in voting_procedures {
                        let hayate_voter = match pallas_voter {
                            pallas_primitives::conway::Voter::ConstitutionalCommitteeKey(h) => {
                                let mut hash = [0u8; 32];
                                hash[..28].copy_from_slice(h.as_ref());
                                hayate::ledger::primitives::Voter::ConstitutionalCommittee(
                                    hayate::ledger::primitives::Credential::Key(hash),
                                )
                            }
                            pallas_primitives::conway::Voter::ConstitutionalCommitteeScript(h) => {
                                let mut hash = [0u8; 32];
                                hash[..28].copy_from_slice(h.as_ref());
                                hayate::ledger::primitives::Voter::ConstitutionalCommittee(
                                    hayate::ledger::primitives::Credential::Script(hash),
                                )
                            }
                            pallas_primitives::conway::Voter::DRepKey(h) => {
                                let mut hash = [0u8; 32];
                                hash[..28].copy_from_slice(h.as_ref());
                                hayate::ledger::primitives::Voter::DRep(
                                    hayate::ledger::primitives::Credential::Key(hash),
                                )
                            }
                            pallas_primitives::conway::Voter::DRepScript(h) => {
                                let mut hash = [0u8; 32];
                                hash[..28].copy_from_slice(h.as_ref());
                                hayate::ledger::primitives::Voter::DRep(
                                    hayate::ledger::primitives::Credential::Script(hash),
                                )
                            }
                            pallas_primitives::conway::Voter::StakePoolKey(h) => {
                                let mut pool_id = [0u8; 28];
                                pool_id.copy_from_slice(h.as_ref());
                                hayate::ledger::primitives::Voter::StakePool(pool_id)
                            }
                        };
                        for (pallas_action_id, procedure) in action_votes {
                            let mut id_arr = [0u8; 32];
                            let b = pallas_action_id.transaction_id.as_ref();
                            id_arr[..b.len().min(32)].copy_from_slice(&b[..b.len().min(32)]);
                            let hayate_action_id = hayate::ledger::primitives::GovActionId {
                                tx_hash: id_arr,
                                index: pallas_action_id.action_index,
                            };
                            let vote = match procedure.vote {
                                pallas_primitives::conway::Vote::Yes => {
                                    hayate::ledger::primitives::Vote::Yes
                                }
                                pallas_primitives::conway::Vote::No => {
                                    hayate::ledger::primitives::Vote::No
                                }
                                pallas_primitives::conway::Vote::Abstain => {
                                    hayate::ledger::primitives::Vote::Abstain
                                }
                            };
                            ledger_state.process_vote(
                                &hayate_voter,
                                &hayate_action_id,
                                &hayate::ledger::primitives::VotingProcedure { vote, anchor: None },
                            );
                        }
                    }
                }
                if let Some(donation) = conway_tx.transaction_body.donation {
                    ledger_state.treasury.0 =
                        ledger_state.treasury.0.saturating_add(u64::from(donation));
                }
            }

            // Process withdrawals (reward account withdrawals).
            // When a TX withdraws from a reward account, the full balance moves into a UTxO
            // output (already added to utxo_tree above). We must zero out the reward_accounts
            // entry, otherwise snapshot_stake will double-count: UTxO stake (which now includes
            // the withdrawn funds) + the still-non-zero reward_accounts balance.
            {
                // Bind to a local so the temporary outlives the borrow in collect().
                let raw_withdrawals = tx.withdrawals();
                let withdrawals: Vec<(&[u8], u64)> = raw_withdrawals.collect();
                if !withdrawals.is_empty() {
                    let reward_accounts = Arc::make_mut(&mut ledger_state.reward_accounts);
                    for (reward_addr_bytes, _amount) in &withdrawals {
                        if reward_addr_bytes.len() < 29 {
                            continue;
                        }
                        let mut cred_hash = [0u8; 32];
                        cred_hash[..28].copy_from_slice(&reward_addr_bytes[1..29]);
                        // Zero the balance; do NOT remove the entry. Removing would make the
                        // credential appear "unregistered" to the next RUPD application, causing
                        // its rewards to be redirected to treasury (unregRU'). In Haskell,
                        // withdrawal sets balance=0 but keeps the credential registered.
                        if let Some(balance) = reward_accounts.get_mut(&cred_hash) {
                            *balance = hayate::ledger::primitives::Lovelace(0);
                        }
                    }
                }
            }
        } // end if tx_valid

        // Track transaction fees.
        // For valid txs: fee is the declared tx fee.
        // For invalid txs (phase-2 script failure): fee is total_collateral, or if absent,
        // the sum of consumed collateral input values minus collateral return value.
        let tx_fee = if tx_valid {
            tx.fee().unwrap_or(0)
        } else {
            // Babbage/Conway: use explicit total_collateral if present.
            // Otherwise (Alonzo-style or Babbage without the optional field):
            // fee = sum(collateral_input_values) - collateral_return_value.
            // consumed_value was computed above during UTxO removal; produced_value
            // comes from the collateral_return output (already inserted into storage).
            if let Some(tc) = tx.total_collateral() {
                tc
            } else {
                let return_value = tx.collateral_return()
                    .map(|r| r.value().coin())
                    .unwrap_or(0);
                consumed_value.saturating_sub(return_value)
            }
        };
        ledger_state.epoch_fees.0 += tx_fee;
        if tx_fee > 0 {
            tracing::debug!(
                "fee extracted: tx_fee={}, total_epoch_fees={} lovelace (slot {}){}",
                tx_fee,
                ledger_state.epoch_fees.0,
                slot,
                if tx_valid { "" } else { " [COLLATERAL]" }
            );
        }
    }

    Ok((decode_us, remove_us, insert_us, certs_us))
}

fn extract_stake_credential(address: &[u8]) -> Result<Option<Vec<u8>>> {
    use pallas_addresses::{Address, ShelleyDelegationPart};

    let addr = Address::from_bytes(address)
        .map_err(|e| anyhow::anyhow!("Failed to parse address: {}", e))?;

    match addr {
        Address::Shelley(shelley_addr) => match shelley_addr.delegation() {
            ShelleyDelegationPart::Key(key_hash) => Ok(Some(key_hash.to_vec())),
            ShelleyDelegationPart::Script(script_hash) => Ok(Some(script_hash.to_vec())),
            ShelleyDelegationPart::Null => Ok(None),
            _ => Ok(None),
        },
        _ => Ok(None), // Byron addresses don't have stake credentials
    }
}

fn extract_pool_id_from_block(
    block: &pallas_traverse::MultiEraBlock,
) -> Option<hayate::ledger::primitives::Hash28> {
    use blake2::{Blake2b, Digest};

    // The pool ID is the blake2b-224 hash (28 bytes) of the cold vkey
    // The block header contains the full vkey (32 bytes), so we need to hash it
    if let Some(conway_block) = block.as_conway() {
        let issuer_vkey: &[u8] = conway_block.header.header_body.issuer_vkey.as_ref();
        if issuer_vkey.len() == 32 {
            let mut hasher = Blake2b::<blake2::digest::consts::U28>::new();
            hasher.update(issuer_vkey);
            let hash_result = hasher.finalize();
            let mut pool_id = [0u8; 28];
            pool_id.copy_from_slice(&hash_result);
            return Some(pool_id);
        }
    } else if let Some(babbage_block) = block.as_babbage() {
        let issuer_vkey: &[u8] = babbage_block.header.header_body.issuer_vkey.as_ref();
        if issuer_vkey.len() == 32 {
            let mut hasher = Blake2b::<blake2::digest::consts::U28>::new();
            hasher.update(issuer_vkey);
            let hash_result = hasher.finalize();
            let mut pool_id = [0u8; 28];
            pool_id.copy_from_slice(&hash_result);
            return Some(pool_id);
        }
    } else if let Some(alonzo_block) = block.as_alonzo() {
        let issuer_vkey: &[u8] = alonzo_block.header.header_body.issuer_vkey.as_ref();
        if issuer_vkey.len() == 32 {
            let mut hasher = Blake2b::<blake2::digest::consts::U28>::new();
            hasher.update(issuer_vkey);
            let hash_result = hasher.finalize();
            let mut pool_id = [0u8; 28];
            pool_id.copy_from_slice(&hash_result);
            return Some(pool_id);
        }
    }
    None
}

/// Extract a pre-Conway protocol parameter update proposal from a transaction
/// and record it in `pending_pp_updates` for application at the target epoch.
///
/// In Shelley through Babbage, transaction bodies may contain an `Update` field
/// with proposed protocol parameter changes keyed by genesis delegate hash.
/// When quorum (from Shelley genesis `updateQuorum`) delegates have proposed
/// the same update for the same target epoch, those parameters are enacted
/// at the epoch boundary.
fn process_ppup_proposal(
    update: pallas_traverse::MultiEraUpdate<'_>,
    ledger_state: &mut LedgerState,
    slot: u64,
) {
    use hayate::ledger::primitives::{EpochNo, ProtocolParamUpdate, Rational};

    let target_epoch = EpochNo(update.epoch());

    // Extract proposals from Alonzo-compatible (Shelley/Allegra/Mary/Alonzo) or Babbage updates.
    // For each genesis delegate hash and its proposed parameters, build our ProtocolParamUpdate.
    let proposals: Vec<(hayate::ledger::primitives::Hash32, ProtocolParamUpdate)> =
        if let Some(alonzo_update) = update.as_alonzo() {
            alonzo_update
                .proposed_protocol_parameter_updates
                .iter()
                .map(|(genesis_hash, ppu)| {
                    let mut update = ProtocolParamUpdate::default();
                    update.min_fee_a = ppu.minfee_a.map(|v| v as u64);
                    update.min_fee_b = ppu.minfee_b.map(|v| v as u64);
                    update.max_block_body_size = ppu.max_block_body_size.map(|v| v as u64);
                    update.max_transaction_size = ppu.max_transaction_size.map(|v| v as u64);
                    update.max_block_header_size = ppu.max_block_header_size.map(|v| v as u64);
                    update.key_deposit = ppu.key_deposit;
                    update.pool_deposit = ppu.pool_deposit;
                    update.e_max = ppu.maximum_epoch;
                    update.n_opt = ppu.desired_number_of_stake_pools.map(|v| v as u64);
                    update.a0 = ppu.pool_pledge_influence.as_ref().map(|r| Rational {
                        numerator: r.numerator,
                        denominator: r.denominator,
                    });
                    update.rho = ppu.expansion_rate.as_ref().map(|r| Rational {
                        numerator: r.numerator,
                        denominator: r.denominator,
                    });
                    update.tau = ppu.treasury_growth_rate.as_ref().map(|r| Rational {
                        numerator: r.numerator,
                        denominator: r.denominator,
                    });
                    update.decentralization =
                        ppu.decentralization_constant.as_ref().map(|r| Rational {
                            numerator: r.numerator,
                            denominator: r.denominator,
                        });
                    update.protocol_version = ppu
                        .protocol_version
                        .map(|(major, minor)| (major as u64, minor as u64));
                    update.min_pool_cost = ppu.min_pool_cost;
                    let src: &[u8] = genesis_hash.as_ref();
                    let mut hash = [0u8; 32];
                    let n = src.len().min(32);
                    hash[..n].copy_from_slice(&src[..n]);
                    (hash, update)
                })
                .collect()
        } else if let Some(babbage_update) = update.as_babbage() {
            babbage_update
                .proposed_protocol_parameter_updates
                .iter()
                .map(|(genesis_hash, ppu)| {
                    let mut update = ProtocolParamUpdate::default();
                    update.min_fee_a = ppu.minfee_a.map(|v| v as u64);
                    update.min_fee_b = ppu.minfee_b.map(|v| v as u64);
                    update.max_block_body_size = ppu.max_block_body_size.map(|v| v as u64);
                    update.max_transaction_size = ppu.max_transaction_size.map(|v| v as u64);
                    update.max_block_header_size = ppu.max_block_header_size.map(|v| v as u64);
                    update.key_deposit = ppu.key_deposit;
                    update.pool_deposit = ppu.pool_deposit;
                    update.e_max = ppu.maximum_epoch;
                    update.n_opt = ppu.desired_number_of_stake_pools.map(|v| v as u64);
                    update.a0 = ppu.pool_pledge_influence.as_ref().map(|r| Rational {
                        numerator: r.numerator,
                        denominator: r.denominator,
                    });
                    update.rho = ppu.expansion_rate.as_ref().map(|r| Rational {
                        numerator: r.numerator,
                        denominator: r.denominator,
                    });
                    update.tau = ppu.treasury_growth_rate.as_ref().map(|r| Rational {
                        numerator: r.numerator,
                        denominator: r.denominator,
                    });
                    update.protocol_version = ppu
                        .protocol_version
                        .map(|(major, minor)| (major as u64, minor as u64));
                    update.min_pool_cost = ppu.min_pool_cost;
                    let src: &[u8] = genesis_hash.as_ref();
                    let mut hash = [0u8; 32];
                    let n = src.len().min(32);
                    hash[..n].copy_from_slice(&src[..n]);
                    (hash, update)
                })
                .collect()
        } else {
            return;
        };

    if proposals.is_empty() {
        return;
    }

    // Log at info level so param updates are visible in normal operation.
    // Use the first proposal's changes against current params (delegates propose identical values).
    let changes = proposals
        .first()
        .map(|(_, ppu)| ppu.format_changes(&ledger_state.protocol_params))
        .unwrap_or_default();
    let slot_in_epoch = slot % ledger_state.epoch_length as u64;
    info!(
        "PPUP: received protocol parameter update proposal votes={} epoch={} slot_in_epoch={} changes=\"{}\"",
        proposals.len(), target_epoch.0, slot_in_epoch, changes
    );

    let entry = ledger_state
        .pending_pp_updates
        .entry(target_epoch)
        .or_insert_with(std::collections::BTreeMap::new);
    for (hash, ppu) in proposals {
        let individual_changes = ppu.format_changes(&ledger_state.protocol_params);
        let overwritten = entry.contains_key(&hash);
        tracing::debug!(
            "PPUP: delegate={} overwritten={} changes=\"{}\"",
            hex::encode(&hash[..8]),
            overwritten,
            individual_changes,
        );
        // Last-write-wins: if the same genesis delegate submits multiple
        // proposals, only the latest one counts (matches Haskell Map semantics).
        entry.insert(hash, ppu);
    }
}

fn process_certificate(
    cert: pallas_traverse::MultiEraCert,
    ledger_state: &mut LedgerState,
    slot: u64,
) -> Result<()> {
    // MultiEraCert is an enum with AlonzoCompatible and Conway variants
    // Each contains the era-specific Certificate enum
    match cert {
        pallas_traverse::MultiEraCert::AlonzoCompatible(cert_box) => {
            process_alonzo_certificate(&cert_box, ledger_state, slot)?;
        }
        pallas_traverse::MultiEraCert::Conway(cert_box) => {
            process_conway_certificate(&cert_box, ledger_state, slot)?;
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
    slot: u64,
) -> Result<()> {
    use hayate::ledger::primitives::{EpochNo, Lovelace};
    use hayate::ledger::state::{DepositType, PoolRegistration};
    use pallas_primitives::alonzo::Certificate;
    use std::sync::Arc;

    let epoch = ledger_state.epoch_of_slot(slot);
    let (slot_off, _) = ledger_state.slot_within_epoch(slot);

    match cert {
        Certificate::StakeRegistration(stake_cred) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);

                let is_new = Arc::make_mut(&mut ledger_state.reward_accounts).insert(hash, Lovelace(0)).is_none();

                // Track deposit only on new registrations (not re-registrations)
                if is_new {
                    let key_deposit = Lovelace(ledger_state.protocol_params.key_deposit);
                    ledger_state
                        .deposit_tracker
                        .add_deposit(hash, DepositType::Stake, key_deposit);
                }

                // Track script credentials for correct type tag in dumps
                if matches!(stake_cred, pallas_primitives::StakeCredential::ScriptHash(_)) {
                    ledger_state.script_stake_credentials.insert(hash);
                }

                tracing::debug!("e={} s={} off={} | Stake registered: {}", epoch, slot, slot_off, hex::encode(&hash[..8]));
            }
        }

        Certificate::StakeDeregistration(stake_cred) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);

                Arc::make_mut(&mut ledger_state.delegations).remove(&hash);
                Arc::make_mut(&mut ledger_state.reward_accounts).remove(&hash);

                ledger_state
                    .deposit_tracker
                    .refund_deposit(&hash, DepositType::Stake);

                ledger_state.script_stake_credentials.remove(&hash);

                tracing::debug!("e={} s={} off={} | Stake deregistered: {}", epoch, slot, slot_off, hex::encode(&hash[..8]));
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

                    Arc::make_mut(&mut ledger_state.delegations).insert(stake_hash, pool_id);

                    tracing::debug!(
                        "e={} s={} off={} | Delegation: {} -> {}",
                        epoch, slot, slot_off,
                        hex::encode(&stake_hash[..8]),
                        hex::encode(&pool_id[..8])
                    );
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

                // New registrations go directly to pool_params; re-registrations go to
                // future_pool_params so updated params appear in the NEXT mark snapshot.
                // Note: re-registration does NOT cancel a pending retirement — both Haskell and
                // Hayate leave psRetiring unchanged when a RegPool cert is processed.
                let is_new_pool = if ledger_state.pool_params.contains_key(&pool_id) {
                    Arc::make_mut(&mut ledger_state.future_pool_params).insert(pool_id, pool_reg);
                    false
                } else {
                    Arc::make_mut(&mut ledger_state.pool_params).insert(pool_id, pool_reg);
                    true
                };

                // Track deposit only on new pool registrations (not updates/re-registrations)
                if is_new_pool {
                    let pool_dep_key = {
                        let mut k = [0u8; 32];
                        k[..28].copy_from_slice(&pool_id);
                        k
                    };
                    let pool_deposit = Lovelace(ledger_state.protocol_params.pool_deposit);
                    ledger_state.deposit_tracker.add_deposit(
                        pool_dep_key,
                        DepositType::Pool,
                        pool_deposit,
                    );
                }

                let ra_bytes: &[u8] = reward_account.as_ref();
                let ra_short = if ra_bytes.len() >= 9 {
                    hex::encode(&ra_bytes[1..9])
                } else {
                    hex::encode(ra_bytes)
                };
                tracing::debug!(
                    "e={} s={} off={} | Pool registered: {} reward_acct={} (pledge: {} ADA)",
                    epoch, slot, slot_off,
                    hex::encode(&pool_id[..8]),
                    ra_short,
                    pledge / 1_000_000
                );
            }
        }

        Certificate::PoolRetirement(pool_hash, retirement_epoch) => {
            let pool_bytes = pool_hash.as_ref();
            if pool_bytes.len() >= 28 {
                let mut pool_id = [0u8; 28];
                pool_id.copy_from_slice(&pool_bytes[..28]);

                ledger_state
                    .pending_retirements
                    .entry(EpochNo(*retirement_epoch))
                    .or_insert_with(Vec::new)
                    .push(pool_id);

                tracing::debug!(
                    "e={} s={} off={} | Pool retirement scheduled: {} at epoch {}",
                    epoch, slot, slot_off,
                    hex::encode(&pool_id[..8]),
                    retirement_epoch
                );
            }
        }

        Certificate::GenesisKeyDelegation(_, _, _)
        | Certificate::MoveInstantaneousRewardsCert(_) => {
            tracing::debug!("e={} s={} off={} | Legacy certificate (genesis/MIR)", epoch, slot, slot_off);
        }
    }

    Ok(())
}

fn pallas_drep_to_hayate(
    drep: &pallas_primitives::conway::DRep,
) -> hayate::ledger::primitives::DRep {
    use hayate::ledger::primitives::DRep;
    match drep {
        pallas_primitives::conway::DRep::Key(hash) => {
            let bytes = hash.as_ref();
            let mut h = [0u8; 32];
            let len = bytes.len().min(28);
            h[..len].copy_from_slice(&bytes[..len]);
            DRep::KeyHash(h)
        }
        pallas_primitives::conway::DRep::Script(hash) => {
            let bytes = hash.as_ref();
            let mut h = [0u8; 32];
            let len = bytes.len().min(28);
            h[..len].copy_from_slice(&bytes[..len]);
            DRep::ScriptHash(h)
        }
        pallas_primitives::conway::DRep::Abstain => DRep::AlwaysAbstain,
        pallas_primitives::conway::DRep::NoConfidence => DRep::AlwaysNoConfidence,
    }
}

/// Convert a pallas Conway ProtocolParamUpdate to a hayate ProtocolParamUpdate.
///
/// Only Conway-era fields that have hayate equivalents are mapped.
fn pallas_ppu_to_hayate(
    ppu: &pallas_primitives::conway::ProtocolParamUpdate,
) -> hayate::ledger::primitives::ProtocolParamUpdate {
    use hayate::ledger::primitives::{ProtocolParamUpdate, Rational};

    // Helper: convert pallas RationalNumber → hayate Rational
    let rat = |r: &pallas_primitives::RationalNumber| Rational {
        numerator: r.numerator,
        denominator: r.denominator,
    };

    ProtocolParamUpdate {
        min_fee_a: ppu.minfee_a,
        min_fee_b: ppu.minfee_b,
        max_block_body_size: ppu.max_block_body_size,
        max_transaction_size: ppu.max_transaction_size,
        max_block_header_size: ppu.max_block_header_size,
        protocol_version: None, // protocol_version not in Conway PPU
        key_deposit: ppu.key_deposit,
        pool_deposit: ppu.pool_deposit,
        min_pool_cost: ppu.min_pool_cost,
        rho: ppu.expansion_rate.as_ref().map(&rat),
        tau: ppu.treasury_growth_rate.as_ref().map(&rat),
        a0: ppu.pool_pledge_influence.as_ref().map(&rat),
        n_opt: ppu.desired_number_of_stake_pools,
        e_max: ppu.maximum_epoch,
        decentralization: None,
        drep_deposit: ppu.drep_deposit,
        drep_activity: ppu.drep_inactivity_period,
        gov_action_lifetime: ppu.governance_action_validity_period,
        gov_action_deposit: ppu.governance_action_deposit,
        committee_min_size: ppu.min_committee_size,
        committee_max_term_length: ppu.committee_term_limit,
        min_fee_ref_script_cost_per_byte: ppu.minfee_refscript_cost_per_byte.as_ref().map(&rat),
        dvt_motion_no_confidence: ppu
            .drep_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.motion_no_confidence)),
        dvt_committee_normal: ppu
            .drep_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.committee_normal)),
        dvt_committee_no_confidence: ppu
            .drep_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.committee_no_confidence)),
        dvt_update_to_constitution: ppu
            .drep_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.update_constitution)),
        dvt_hard_fork_initiation: ppu
            .drep_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.hard_fork_initiation)),
        dvt_pp_network_group: ppu
            .drep_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.pp_network_group)),
        dvt_pp_economic_group: ppu
            .drep_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.pp_economic_group)),
        dvt_pp_technical_group: ppu
            .drep_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.pp_technical_group)),
        dvt_pp_gov_group: ppu
            .drep_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.pp_governance_group)),
        dvt_treasury_withdrawal: ppu
            .drep_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.treasury_withdrawal)),
        pvt_motion_no_confidence: ppu
            .pool_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.motion_no_confidence)),
        pvt_committee_normal: ppu
            .pool_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.committee_normal)),
        pvt_committee_no_confidence: ppu
            .pool_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.committee_no_confidence)),
        pvt_hard_fork_initiation: ppu
            .pool_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.hard_fork_initiation)),
        pvt_pp_security_group: ppu
            .pool_voting_thresholds
            .as_ref()
            .map(|t| rat(&t.security_voting_threshold)),
    }
}

fn process_conway_certificate(
    cert: &pallas_primitives::conway::Certificate,
    ledger_state: &mut LedgerState,
    slot: u64,
) -> Result<()> {
    use hayate::ledger::primitives::{EpochNo, Lovelace};
    use hayate::ledger::state::{DepositType, PoolRegistration};

    let epoch = ledger_state.epoch_of_slot(slot);
    let (slot_off, _) = ledger_state.slot_within_epoch(slot);
    use pallas_primitives::conway::Certificate;
    use std::sync::Arc;

    match cert {
        // Basic stake certificates (same as Alonzo)
        Certificate::StakeRegistration(stake_cred) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);

                let is_new = Arc::make_mut(&mut ledger_state.reward_accounts).insert(hash, Lovelace(0)).is_none();

                // Track deposit only on new registrations (not re-registrations)
                if is_new {
                    let key_deposit = Lovelace(ledger_state.protocol_params.key_deposit);
                    ledger_state
                        .deposit_tracker
                        .add_deposit(hash, DepositType::Stake, key_deposit);
                }

                // Track script credentials for correct type tag in dumps
                if matches!(stake_cred, pallas_primitives::StakeCredential::ScriptHash(_)) {
                    ledger_state.script_stake_credentials.insert(hash);
                }

                tracing::debug!("e={} s={} off={} | Stake registered: {}", epoch, slot, slot_off, hex::encode(&hash[..8]));
            }
        }

        Certificate::StakeDeregistration(stake_cred) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);

                Arc::make_mut(&mut ledger_state.delegations).remove(&hash);
                Arc::make_mut(&mut ledger_state.reward_accounts).remove(&hash);

                ledger_state
                    .deposit_tracker
                    .refund_deposit(&hash, DepositType::Stake);

                ledger_state.script_stake_credentials.remove(&hash);

                tracing::debug!("e={} s={} off={} | Stake deregistered: {}", epoch, slot, slot_off, hex::encode(&hash[..8]));
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

                    Arc::make_mut(&mut ledger_state.delegations).insert(stake_hash, pool_id);

                    tracing::debug!(
                        "e={} s={} off={} | Delegation: {} -> {}",
                        epoch, slot, slot_off,
                        hex::encode(&stake_hash[..8]),
                        hex::encode(&pool_id[..8])
                    );
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

                // New registrations go directly to pool_params; re-registrations go to
                // future_pool_params so updated params appear in the NEXT mark snapshot.
                // Note: re-registration does NOT cancel a pending retirement.
                let is_new_pool = if ledger_state.pool_params.contains_key(&pool_id) {
                    Arc::make_mut(&mut ledger_state.future_pool_params).insert(pool_id, pool_reg);
                    false
                } else {
                    Arc::make_mut(&mut ledger_state.pool_params).insert(pool_id, pool_reg);
                    true
                };

                // Track deposit only on new pool registrations (not updates/re-registrations)
                if is_new_pool {
                    let pool_dep_key = {
                        let mut k = [0u8; 32];
                        k[..28].copy_from_slice(&pool_id);
                        k
                    };
                    let pool_deposit = Lovelace(ledger_state.protocol_params.pool_deposit);
                    ledger_state.deposit_tracker.add_deposit(
                        pool_dep_key,
                        DepositType::Pool,
                        pool_deposit,
                    );
                }

                let ra_bytes: &[u8] = reward_account.as_ref();
                let ra_short = if ra_bytes.len() >= 9 {
                    hex::encode(&ra_bytes[1..9])
                } else {
                    hex::encode(ra_bytes)
                };
                tracing::debug!(
                    "e={} s={} off={} | Pool registered: {} reward_acct={} (pledge: {} ADA)",
                    epoch, slot, slot_off,
                    hex::encode(&pool_id[..8]),
                    ra_short,
                    pledge / 1_000_000
                );
            }
        }

        Certificate::PoolRetirement(pool_hash, retirement_epoch) => {
            let pool_bytes = pool_hash.as_ref();
            if pool_bytes.len() >= 28 {
                let mut pool_id = [0u8; 28];
                pool_id.copy_from_slice(&pool_bytes[..28]);

                ledger_state
                    .pending_retirements
                    .entry(EpochNo(*retirement_epoch))
                    .or_insert_with(Vec::new)
                    .push(pool_id);

                tracing::debug!(
                    "e={} s={} off={} | Pool retirement scheduled: {} at epoch {}",
                    epoch, slot, slot_off,
                    hex::encode(&pool_id[..8]),
                    retirement_epoch
                );
            }
        }

        // Conway-specific governance certificates
        Certificate::Reg(stake_cred, deposit) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);

                let is_new = Arc::make_mut(&mut ledger_state.reward_accounts).insert(hash, Lovelace(0)).is_none();

                if is_new {
                    // Use the explicit deposit amount from the certificate
                    ledger_state.deposit_tracker.add_deposit(
                        hash,
                        DepositType::Stake,
                        Lovelace(*deposit),
                    );
                }

                // Track script credentials for correct type tag in dumps
                if matches!(stake_cred, pallas_primitives::StakeCredential::ScriptHash(_)) {
                    ledger_state.script_stake_credentials.insert(hash);
                }

                tracing::debug!("Conway stake registered: {}", hex::encode(&cred_hash[..8]));
            }
        }
        Certificate::UnReg(stake_cred, _deposit) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);

                Arc::make_mut(&mut ledger_state.delegations).remove(&hash);
                Arc::make_mut(&mut ledger_state.reward_accounts).remove(&hash);

                ledger_state
                    .deposit_tracker
                    .refund_deposit(&hash, DepositType::Stake);

                ledger_state.script_stake_credentials.remove(&hash);

                tracing::debug!(
                    "Conway stake deregistered: {}",
                    hex::encode(&cred_hash[..8])
                );
            }
        }

        Certificate::VoteDeleg(stake_cred, drep) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);
                let hayate_drep = pallas_drep_to_hayate(drep);
                Arc::make_mut(&mut ledger_state.governance)
                    .vote_delegations
                    .insert(hash, hayate_drep);
                tracing::debug!("Vote delegation: {}", hex::encode(&cred_hash[..8]));
            }
        }

        Certificate::StakeVoteDeleg(stake_cred, pool_keyhash, drep) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);
                let pool_bytes = pool_keyhash.as_ref();
                if pool_bytes.len() >= 28 {
                    let mut pool_id = [0u8; 28];
                    pool_id.copy_from_slice(&pool_bytes[..28]);
                    Arc::make_mut(&mut ledger_state.delegations).insert(hash, pool_id);
                }
                let hayate_drep = pallas_drep_to_hayate(drep);
                Arc::make_mut(&mut ledger_state.governance)
                    .vote_delegations
                    .insert(hash, hayate_drep);
                tracing::debug!("Stake+vote delegation: {}", hex::encode(&cred_hash[..8]));
            }
        }

        Certificate::StakeRegDeleg(stake_cred, pool_keyhash, deposit) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);
                let is_new = Arc::make_mut(&mut ledger_state.reward_accounts).insert(hash, Lovelace(0)).is_none();
                if is_new {
                    ledger_state.deposit_tracker.add_deposit(
                        hash,
                        DepositType::Stake,
                        Lovelace(*deposit),
                    );
                }
                if matches!(stake_cred, pallas_primitives::StakeCredential::ScriptHash(_)) {
                    ledger_state.script_stake_credentials.insert(hash);
                }
                let pool_bytes = pool_keyhash.as_ref();
                if pool_bytes.len() >= 28 {
                    let mut pool_id = [0u8; 28];
                    pool_id.copy_from_slice(&pool_bytes[..28]);
                    Arc::make_mut(&mut ledger_state.delegations).insert(hash, pool_id);
                }
                tracing::debug!("Stake reg+delegate: {}", hex::encode(&cred_hash[..8]));
            }
        }

        Certificate::VoteRegDeleg(stake_cred, drep, deposit) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);
                let is_new = Arc::make_mut(&mut ledger_state.reward_accounts).insert(hash, Lovelace(0)).is_none();
                if is_new {
                    ledger_state.deposit_tracker.add_deposit(
                        hash,
                        DepositType::Stake,
                        Lovelace(*deposit),
                    );
                }
                if matches!(stake_cred, pallas_primitives::StakeCredential::ScriptHash(_)) {
                    ledger_state.script_stake_credentials.insert(hash);
                }
                let hayate_drep = pallas_drep_to_hayate(drep);
                Arc::make_mut(&mut ledger_state.governance)
                    .vote_delegations
                    .insert(hash, hayate_drep);
                tracing::debug!("Vote reg+delegate: {}", hex::encode(&cred_hash[..8]));
            }
        }

        Certificate::StakeVoteRegDeleg(stake_cred, pool_keyhash, drep, deposit) => {
            if let Some(cred_hash) = stake_credential_to_hash28(stake_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);
                let is_new = Arc::make_mut(&mut ledger_state.reward_accounts).insert(hash, Lovelace(0)).is_none();
                if is_new {
                    ledger_state.deposit_tracker.add_deposit(
                        hash,
                        DepositType::Stake,
                        Lovelace(*deposit),
                    );
                }
                if matches!(stake_cred, pallas_primitives::StakeCredential::ScriptHash(_)) {
                    ledger_state.script_stake_credentials.insert(hash);
                }
                let pool_bytes = pool_keyhash.as_ref();
                if pool_bytes.len() >= 28 {
                    let mut pool_id = [0u8; 28];
                    pool_id.copy_from_slice(&pool_bytes[..28]);
                    Arc::make_mut(&mut ledger_state.delegations).insert(hash, pool_id);
                }
                let hayate_drep = pallas_drep_to_hayate(drep);
                Arc::make_mut(&mut ledger_state.governance)
                    .vote_delegations
                    .insert(hash, hayate_drep);
                tracing::debug!("Stake+vote reg+delegate: {}", hex::encode(&cred_hash[..8]));
            }
        }

        Certificate::AuthCommitteeHot(cold_cred, hot_cred) => {
            if let (Some(cold_28), Some(hot_28)) = (
                stake_credential_to_hash28(cold_cred),
                stake_credential_to_hash28(hot_cred),
            ) {
                let mut cold_hash = [0u8; 32];
                cold_hash[..28].copy_from_slice(&cold_28);
                let mut hot_hash = [0u8; 32];
                hot_hash[..28].copy_from_slice(&hot_28);
                let gov = Arc::make_mut(&mut ledger_state.governance);
                gov.committee_hot_keys.insert(cold_hash, hot_hash);
                gov.committee_resigned.remove(&cold_hash);
                if matches!(cold_cred, pallas_primitives::StakeCredential::ScriptHash(_)) {
                    gov.script_committee_credentials.insert(cold_hash);
                }
                if matches!(hot_cred, pallas_primitives::StakeCredential::ScriptHash(_)) {
                    gov.script_committee_hot_credentials.insert(hot_hash);
                }
                tracing::debug!(
                    "Committee hot key authorized: {} -> {}",
                    hex::encode(&cold_28[..8]),
                    hex::encode(&hot_28[..8])
                );
            }
        }

        Certificate::ResignCommitteeCold(cold_cred, _anchor) => {
            if let Some(cold_28) = stake_credential_to_hash28(cold_cred) {
                let mut cold_hash = [0u8; 32];
                cold_hash[..28].copy_from_slice(&cold_28);
                let gov = Arc::make_mut(&mut ledger_state.governance);
                gov.committee_resigned.insert(cold_hash, None);
                gov.committee_hot_keys.remove(&cold_hash);
                if matches!(cold_cred, pallas_primitives::StakeCredential::ScriptHash(_)) {
                    gov.script_committee_credentials.insert(cold_hash);
                }
                tracing::debug!("Committee member resigned: {}", hex::encode(&cold_28[..8]));
            }
        }

        Certificate::RegDRepCert(drep_cred, deposit, anchor) => {
            if let Some(cred_hash) = stake_credential_to_hash28(drep_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);
                ledger_state.deposit_tracker.add_deposit(
                    hash,
                    DepositType::DRep,
                    Lovelace(*deposit),
                );
                // Issue 5: distinguish Key vs Script credentials for DRep registration
                let is_script =
                    matches!(drep_cred, pallas_primitives::StakeCredential::ScriptHash(_));
                let credential = if is_script {
                    hayate::ledger::primitives::Credential::Script(hash)
                } else {
                    hayate::ledger::primitives::Credential::Key(hash)
                };
                // Issue 6: store anchor from registration
                let hayate_anchor = anchor.as_ref().map(|a| {
                    let mut hash_bytes = [0u8; 32];
                    let b = a.content_hash.as_ref();
                    hash_bytes[..b.len().min(32)].copy_from_slice(&b[..b.len().min(32)]);
                    hayate::ledger::primitives::Anchor {
                        url: a.url.clone(),
                        hash: hash_bytes,
                    }
                });
                let gov = Arc::make_mut(&mut ledger_state.governance);
                let current_epoch = ledger_state.epoch;
                gov.dreps.insert(
                    hash,
                    hayate::ledger::state::DRepRegistration {
                        credential,
                        deposit: Lovelace(*deposit),
                        anchor: hayate_anchor,
                        registered_epoch: current_epoch,
                        last_active_epoch: current_epoch,
                        active: true,
                    },
                );
                gov.drep_registration_count += 1;
                tracing::debug!(
                    "DRep registered: {} deposit={}",
                    hex::encode(&cred_hash[..8]),
                    deposit
                );
            }
        }

        Certificate::UnRegDRepCert(drep_cred, _deposit) => {
            if let Some(cred_hash) = stake_credential_to_hash28(drep_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);
                if let Some(refund) = ledger_state
                    .deposit_tracker
                    .refund_deposit(&hash, DepositType::DRep)
                {
                    *Arc::make_mut(&mut ledger_state.reward_accounts)
                        .entry(hash)
                        .or_insert(Lovelace(0)) += refund;
                }
                Arc::make_mut(&mut ledger_state.governance)
                    .dreps
                    .remove(&hash);
                tracing::debug!("DRep deregistered: {}", hex::encode(&cred_hash[..8]));
            }
        }

        Certificate::UpdateDRepCert(drep_cred, anchor) => {
            if let Some(cred_hash) = stake_credential_to_hash28(drep_cred) {
                let mut hash = [0u8; 32];
                hash[..28].copy_from_slice(&cred_hash);
                let current_epoch = ledger_state.epoch;
                // Issue 6: store updated anchor
                let hayate_anchor = anchor.as_ref().map(|a| {
                    let mut hash_bytes = [0u8; 32];
                    let b = a.content_hash.as_ref();
                    hash_bytes[..b.len().min(32)].copy_from_slice(&b[..b.len().min(32)]);
                    hayate::ledger::primitives::Anchor {
                        url: a.url.clone(),
                        hash: hash_bytes,
                    }
                });
                let gov = Arc::make_mut(&mut ledger_state.governance);
                if let Some(reg) = gov.dreps.get_mut(&hash) {
                    reg.last_active_epoch = current_epoch;
                    reg.anchor = hayate_anchor;
                }
                tracing::debug!("DRep updated: {}", hex::encode(&cred_hash[..8]));
            }
        }
    }

    Ok(())
}

/// Load only the Conway genesis (does not require full ledger init)
fn load_conway_genesis(config_path: Option<&PathBuf>) -> Option<ConwayGenesis> {
    let config_file = config_path?;
    let config_content = std::fs::read_to_string(config_file).ok()?;
    let config: serde_json::Value = serde_json::from_str(&config_content).ok()?;
    let config_dir = config_file.parent().unwrap_or(std::path::Path::new("./"));
    let conway_path_str = config.get("ConwayGenesisFile")?.as_str()?;
    let full_path = config_dir.join(conway_path_str);
    match ConwayGenesis::load_with_hash(&full_path) {
        Ok((genesis, _)) => {
            info!("✅ Conway genesis loaded from {}", full_path.display());
            Some(genesis)
        }
        Err(e) => {
            warn!("Failed to load Conway genesis: {}", e);
            None
        }
    }
}

/// Load genesis files and initialize ledger state with correct reserves
fn load_genesis_and_init_ledger(
    config_path: Option<&PathBuf>,
    _network: &Network,
) -> Result<(LedgerState, Option<ConwayGenesis>)> {
    let mut ledger_state = LedgerState::new(ProtocolParameters::default());

    // Try to load genesis files if config is provided or we can find default paths
    if let Some(config_file) = config_path {
        info!("Loading genesis from config: {}", config_file.display());

        // Read the cardano-node config file to get genesis file paths
        let config_dir = config_file.parent().unwrap_or(std::path::Path::new("./"));
        let config_content = std::fs::read_to_string(config_file)
            .with_context(|| format!("Failed to read config file: {}", config_file.display()))?;

        let config: serde_json::Value =
            serde_json::from_str(&config_content).with_context(|| "Failed to parse config JSON")?;

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
                        Some(shelley_genesis.active_slots_coeff),
                        pp.protocol_version.as_ref().map(|v| (v.major, v.minor)),
                    );

                    // Compute stability windows from k and f.
                    // Haskell: stabilityWindow = ceiling(2k/f)
                    // Also update 3k/f and 4k/f while we're at it.
                    let k = shelley_genesis.security_param;
                    let f = shelley_genesis.active_slots_coeff;
                    if f > 0.0 {
                        ledger_state.stability_window_2kf =
                            ((2.0 * k as f64 / f).ceil()) as u64;
                        ledger_state.stability_window_3kf =
                            ((3.0 * k as f64 / f).ceil()) as u64;
                        ledger_state.randomness_stabilisation_window =
                            ((4.0 * k as f64 / f).ceil()) as u64;
                    }

                    info!(
                        "✅ Shelley genesis loaded: epoch_length={}, k={}, d={}, stabilityWindow(2k/f)={}",
                        shelley_genesis.epoch_length,
                        shelley_genesis.security_param,
                        pp.decentralisation_param.unwrap_or(0.0),
                        ledger_state.stability_window_2kf,
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

        // Determine how the Shelley hard fork is triggered.
        let test_shelley_epoch = config
            .get("TestShelleyHardForkAtEpoch")
            .and_then(|v| v.as_u64());

        match test_shelley_epoch {
            Some(0) => {
                // Chain starts in Shelley — no Byron era (e.g. sanchonet, preview).
                // has_byron stays false, shelley_transition_epoch stays 0.
                info!("TestShelleyHardForkAtEpoch = 0 — no Byron era, starting in Shelley");
            }
            Some(n) => {
                // Forced transition at a fixed epoch (custom testnet with Byron prefix).
                ledger_state.has_byron = true;
                ledger_state.shelley_transition_epoch = n;
                info!(
                    "TestShelleyHardForkAtEpoch = {} — forced Shelley transition",
                    n
                );
            }
            None => {
                // No forced fork — transition via on-chain update proposal (mainnet, preprod).
                if ledger_state.byron_epoch_length > 0 {
                    ledger_state.has_byron = true;
                    ledger_state.shelley_transition_epoch = u64::MAX; // sentinel: pending
                    info!(
                        "No TestShelleyHardForkAtEpoch — will transition to Shelley via \
                           on-chain update proposal (2.0.0)"
                    );
                }
            }
        }

        // Seed initial RUPD (reward update) for Shelley-from-genesis chains.
        //
        // Haskell's `mkShelleyNewEpochState` calls `createRUpd` at genesis to seed
        // `nesRu`, which is then applied at the very first epoch transition (0→1).
        // Without this, the 0→1 boundary has no RUPD to apply, leaving treasury=0
        // and reserves=15T instead of the correct treasury=9T.
        //
        // Only relevant when the chain starts in Shelley (no Byron era).  For
        // Byron-first chains, the initial RUPD is seeded at the Shelley HF.
        if !ledger_state.has_byron {
            let prev_pp = ledger_state.protocol_params.clone();
            let empty_snapshot = hayate::ledger::state::StakeSnapshot {
                epoch: EpochNo(0),
                delegations: std::sync::Arc::new(std::collections::HashMap::new()),
                pool_stake: std::collections::HashMap::new(),
                pool_params: std::sync::Arc::new(std::collections::HashMap::new()),
                stake_distribution: std::sync::Arc::new(std::collections::HashMap::new()),
                epoch_blocks_by_pool: std::sync::Arc::new(std::collections::HashMap::new()),
                script_stake_credentials: std::collections::HashSet::new(),
            };
            let initial_rupd = ledger_state.calculate_rewards(
                &empty_snapshot,
                hayate::ledger::primitives::Lovelace(0),
                &prev_pp,
            );
            info!(
                "Seeded initial RUPD: deltaR1={}, deltaT1={}, to be applied at 0→1",
                initial_rupd.delta_r1, initial_rupd.delta_t1
            );
            ledger_state.pending_reward_update = Some(initial_rupd);
        }

        // Load Conway genesis for governance parameters (applied at era transition)
        let conway_genesis = load_conway_genesis(config_path);

        return Ok((ledger_state, conway_genesis));
    }

    warn!("No config file provided - using default mainnet values");
    warn!("⚠️  Treasury and reserves will be incorrect!");
    warn!("⚠️  Provide --config <path> to load proper genesis values");

    Ok((ledger_state, None))
}

/// Insert all Byron genesis UTxOs into the LSM tree.
///
/// Called once on a fresh run (before any blocks are processed).  This ensures
/// that when Byron transactions spend genesis outputs the tombstones hit real
/// LSM entries, and that `recalibrate_reserves_from_utxo_tree` at the
/// Byron→Shelley hard fork sees the correct total UTxO value.
///
/// Genesis UTxOs have no stake credential (Byron addresses carry none).
fn seed_genesis_utxos_into_storage(
    config_path: Option<&PathBuf>,
    storage: &mut NodeStorage,
) -> Result<()> {
    let config_file = match config_path {
        Some(p) => p,
        None => {
            warn!("No config file — skipping genesis UTxO seeding");
            return Ok(());
        }
    };

    let config_dir = config_file.parent().unwrap_or(std::path::Path::new("./"));
    let config_content = std::fs::read_to_string(config_file)
        .with_context(|| format!("Failed to read config: {}", config_file.display()))?;
    let config: serde_json::Value =
        serde_json::from_str(&config_content).context("Failed to parse config JSON")?;

    let byron_path = match config.get("ByronGenesisFile").and_then(|v| v.as_str()) {
        Some(p) => config_dir.join(p),
        None => {
            warn!("ByronGenesisFile not in config — skipping genesis UTxO seeding");
            return Ok(());
        }
    };

    let entries = hayate::genesis::ByronGenesis::initial_utxos_from_path(&byron_path);
    if entries.is_empty() {
        warn!("No genesis UTxOs loaded — storage will be seeded empty");
        return Ok(());
    }

    info!("Seeding {} genesis UTxOs into LSM tree...", entries.len());
    let mut count = 0u64;
    let mut total_lovelace = 0u64;

    for entry in &entries {
        let utxo = UtxoEntry {
            address: entry.address.clone(),
            amount: entry.lovelace,
            assets: std::collections::HashMap::new(),
            datum_hash: None,
            datum: None,
            script_ref: None,
            stake_credential: None, // Byron addresses have no stake credential
        };
        storage.insert_utxo(&entry.txid, 0, &utxo)?;
        count += 1;
        total_lovelace = total_lovelace.saturating_add(entry.lovelace);
    }

    info!(
        "✅ Seeded {} genesis UTxOs ({} ADA) into LSM tree",
        count,
        total_lovelace / 1_000_000,
    );
    Ok(())
}
