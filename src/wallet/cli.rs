// CLI handlers for wallet commands

use crate::cli::WalletCommand;
use crate::wallet::{
    WalletStorage,
    mnemonic::{generate_mnemonic, normalize_mnemonic},
    derivation::{Network, derive_payment_address},
    transaction::{TransactionBuilder, sign_transaction, write_tx_body, write_signed_tx},
    utxorpc_client::WalletUtxorpcClient,
};
use crate::gpg::Gpg;
use std::io::{self, Write};
use std::path::PathBuf;
use anyhow::{Result, Context};
use pallas_addresses::Address;

/// Handle wallet commands
pub async fn handle_wallet_command(
    wallet_cmd: &WalletCommand,
    wallet_dir: PathBuf,
    utxorpc_endpoint: Option<String>,
) -> Result<()> {
    // Create wallet storage
    let storage = WalletStorage::new(wallet_dir)?;

    match wallet_cmd {
        WalletCommand::Init { name, gpg_recipient, words, network } => {
            handle_init(&storage, name, gpg_recipient.as_deref(), *words, network)?;
        }

        WalletCommand::Add { name, mnemonic, mnemonic_file, gpg_recipient, network } => {
            handle_add(&storage, name, mnemonic.as_deref(), mnemonic_file.as_deref(), gpg_recipient.as_deref(), network)?;
        }

        WalletCommand::List => {
            handle_list(&storage)?;
        }

        WalletCommand::Show { name, count } => {
            handle_show(&storage, name, *count)?;
        }

        WalletCommand::Export { name } => {
            handle_export(&storage, name)?;
        }

        WalletCommand::Delete { name, yes } => {
            handle_delete(&storage, name, *yes)?;
        }

        WalletCommand::Stats { wallet: _ } => {
            println!("Stats command not yet implemented");
        }

        WalletCommand::Utxos { wallet: _ } => {
            println!("UTxOs command not yet implemented");
        }

        WalletCommand::Txs { wallet: _ } => {
            println!("Transactions command not yet implemented");
        }

        // Transaction commands
        WalletCommand::SendTx { wallet, account, address, amount, fee, socket, magic, out_file, multiasset, ttl, sign } => {
            handle_send_tx(&storage, wallet, *account, address, *amount, *fee, socket.as_deref(), *magic, out_file, *multiasset, *ttl, *sign, utxorpc_endpoint.as_deref()).await?;
        }

        WalletCommand::DrainTx { wallet, account, address, fee, socket, magic, out_file, multiasset, rewards, ttl, sign } => {
            handle_drain_tx(&storage, wallet, *account, address, *fee, socket.as_deref(), *magic, out_file, *multiasset, *rewards, *ttl, *sign, utxorpc_endpoint.as_deref()).await?;
        }

        WalletCommand::StakeRegistrationTx { wallet, account, fee, out_file, deposit, ttl, sign } => {
            handle_stake_registration(&storage, wallet, *account, *fee, out_file, *deposit, *ttl, *sign).await?;
        }

        WalletCommand::DelegatePoolTx { wallet, account, pool_id, fee, out_file, ttl, sign } => {
            handle_delegate_pool(&storage, wallet, *account, pool_id, *fee, out_file, *ttl, *sign).await?;
        }

        WalletCommand::SignTx { wallet, account, tx_body_file, out_file, stake } => {
            handle_sign_tx(&storage, wallet, *account, tx_body_file, out_file, *stake)?;
        }

        WalletCommand::WitnessTx { wallet, account, tx_body_file, out_file, role } => {
            handle_witness_tx(&storage, wallet, *account, tx_body_file, out_file, role)?;
        }

        WalletCommand::SignMsg { wallet, account, msg_file, out_file, stake, hashed } => {
            handle_sign_msg(&storage, wallet, *account, msg_file, out_file, *stake, *hashed)?;
        }

        WalletCommand::Multisig { multisig_cmd } => {
            crate::wallet::multisig_cli::handle_multisig_command(multisig_cmd, &storage).await?;
        }
    }

    Ok(())
}

fn handle_init(
    storage: &WalletStorage,
    name: &str,
    gpg_recipient: Option<&str>,
    words: usize,
    network_str: &str,
) -> Result<()> {
    // Check if GPG is required
    if let Some(recipient) = gpg_recipient {
        if !Gpg::is_available() {
            anyhow::bail!("GPG is not available. Please install gnupg or remove --gpg-recipient flag.");
        }
        println!("🔐 GPG encryption enabled with recipient: {}", recipient);
    } else {
        println!("⚠️  WARNING: Mnemonic will be stored UNENCRYPTED!");
        println!("   For production use, specify --gpg-recipient <email-or-key-id>");
        print!("   Continue without encryption? [y/N]: ");
        io::stdout().flush()?;

        let mut response = String::new();
        io::stdin().read_line(&mut response)?;

        if !response.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    // Parse network
    let network = parse_network(network_str)?;

    // Generate mnemonic
    println!("🎲 Generating {}-word mnemonic...", words);
    let mnemonic = generate_mnemonic(words)
        .context("Failed to generate mnemonic")?;

    println!("\n⚠️  IMPORTANT: Write down your recovery phrase and store it safely!");
    println!("════════════════════════════════════════════════════════════════");
    println!("{}", mnemonic);
    println!("════════════════════════════════════════════════════════════════");
    println!("\nPress Enter after you have securely saved your recovery phrase...");

    let mut _confirmation = String::new();
    io::stdin().read_line(&mut _confirmation)?;

    // Create wallet
    let metadata = storage.create_wallet(name, &mnemonic, network, gpg_recipient)
        .context("Failed to create wallet")?;

    println!("✅ Wallet '{}' created successfully!", metadata.name);
    println!("   Network: {}", metadata.network);
    println!("   Encrypted: {}", metadata.encrypted);
    println!("   Created: {}", format_timestamp(metadata.created_at));

    Ok(())
}

fn handle_add(
    storage: &WalletStorage,
    name: &str,
    mnemonic: Option<&str>,
    mnemonic_file: Option<&std::path::Path>,
    gpg_recipient: Option<&str>,
    network_str: &str,
) -> Result<()> {
    // Check for conflicting mnemonic sources
    if mnemonic.is_some() && mnemonic_file.is_some() {
        anyhow::bail!("Cannot specify both --mnemonic and --mnemonic-file");
    }

    // Check if GPG is required
    if let Some(recipient) = gpg_recipient {
        if !Gpg::is_available() {
            anyhow::bail!("GPG is not available. Please install gnupg or remove --gpg-recipient flag.");
        }
        println!("🔐 GPG encryption enabled with recipient: {}", recipient);
    }

    // Parse network
    let network = parse_network(network_str)?;

    // Get mnemonic from file, command line, or user input
    let mnemonic_phrase = if let Some(file) = mnemonic_file {
        // Read from file with GPG support
        read_mnemonic_from_file(file)?
    } else if let Some(m) = mnemonic {
        m.to_string()
    } else {
        println!("Enter your recovery phrase:");
        print!("> ");
        io::stdout().flush()?;

        let mut phrase = String::new();
        io::stdin().read_line(&mut phrase)?;
        phrase.trim().to_string()
    };

    // Normalize and validate
    let normalized = normalize_mnemonic(&mnemonic_phrase);

    // Create wallet
    let metadata = storage.create_wallet(name, &normalized, network, gpg_recipient)
        .context("Failed to add wallet")?;

    println!("✅ Wallet '{}' added successfully!", metadata.name);
    println!("   Network: {}", metadata.network);
    println!("   Encrypted: {}", metadata.encrypted);
    println!("   Created: {}", format_timestamp(metadata.created_at));

    Ok(())
}

fn handle_list(storage: &WalletStorage) -> Result<()> {
    let wallets = storage.list_wallets()?;

    if wallets.is_empty() {
        println!("No wallets found.");
        println!("\nCreate a new wallet with: hayate wallet init <name>");
        return Ok(());
    }

    println!("Wallets:");
    println!("═══════════════════════════════════════════════════════════════");

    for wallet in wallets {
        let encrypted_status = if wallet.encrypted { "🔐 encrypted" } else { "⚠️  plaintext" };
        println!("  {} ({}) - {}", wallet.name, wallet.network, encrypted_status);
        println!("    Created: {}", format_timestamp(wallet.created_at));
    }

    Ok(())
}

fn handle_show(storage: &WalletStorage, name: &str, count: u32) -> Result<()> {
    let metadata = storage.load_metadata(name)
        .context("Failed to load wallet")?;

    println!("Wallet: {}", metadata.name);
    println!("═══════════════════════════════════════════════════════════════");
    println!("Network:   {}", metadata.network);
    println!("Encrypted: {}", metadata.encrypted);
    println!("Created:   {}", format_timestamp(metadata.created_at));

    // Parse network
    let network = parse_network(&metadata.network)?;

    println!("\nDeriving addresses...");

    // Derive account
    let account = storage.derive_account(name, 0)
        .context("Failed to derive account")?;

    // Show addresses
    println!("\nReceiving addresses (first {} addresses):", count);
    println!("───────────────────────────────────────────────────────────────");

    for i in 0..count {
        let address = derive_payment_address(
            &account.payment_key,
            i,
            &account.stake_key,
            network,
        ).context("Failed to derive address")?;

        println!("  {}: {}", i, address);
    }

    Ok(())
}

fn handle_export(storage: &WalletStorage, name: &str) -> Result<()> {
    println!("⚠️  WARNING: This will display your recovery phrase in plaintext!");
    print!("   Continue? [y/N]: ");
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;

    if !response.trim().eq_ignore_ascii_case("y") {
        println!("Cancelled.");
        return Ok(());
    }

    let mnemonic = storage.export_mnemonic(name)
        .context("Failed to export mnemonic")?;

    println!("\n════════════════════════════════════════════════════════════════");
    println!("{}", mnemonic);
    println!("════════════════════════════════════════════════════════════════");

    Ok(())
}

fn handle_delete(storage: &WalletStorage, name: &str, yes: bool) -> Result<()> {
    if !yes {
        println!("⚠️  WARNING: This will permanently delete wallet '{}'!", name);
        println!("   Make sure you have backed up your recovery phrase!");
        print!("   Type 'YES' to confirm: ");
        io::stdout().flush()?;

        let mut response = String::new();
        io::stdin().read_line(&mut response)?;

        if response.trim() != "YES" {
            println!("Cancelled.");
            return Ok(());
        }
    }

    storage.delete_wallet(name)
        .context("Failed to delete wallet")?;

    println!("✅ Wallet '{}' deleted.", name);

    Ok(())
}

fn parse_network(network_str: &str) -> Result<Network> {
    match network_str.to_lowercase().as_str() {
        "mainnet" => Ok(Network::Mainnet),
        "testnet" | "preprod" | "preview" | "sanchonet" => Ok(Network::Testnet),
        _ => anyhow::bail!("Invalid network: {}", network_str),
    }
}

fn format_timestamp(timestamp: u64) -> String {
    use std::time::{UNIX_EPOCH, Duration};
    let datetime = UNIX_EPOCH + Duration::from_secs(timestamp);

    // Simple formatting - just show the timestamp for now
    // In production, you might want to use chrono for better formatting
    format!("{:?}", datetime)
}

/// Read mnemonic from a file (supports GPG encryption)
fn read_mnemonic_from_file(path: &std::path::Path) -> Result<String> {
    // Check if file exists
    if !path.exists() {
        anyhow::bail!("Mnemonic file not found: {}", path.display());
    }

    // Check if file is GPG encrypted
    let contents = if Gpg::is_encrypted(path) {
        // Decrypt GPG file
        println!("🔓 Decrypting GPG-encrypted mnemonic file...");
        Gpg::decrypt_file(path)
            .context("Failed to decrypt GPG file")?
    } else {
        // Read plain text file
        std::fs::read_to_string(path)
            .context("Failed to read mnemonic file")?
    };

    Ok(contents.trim().to_string())
}

// Transaction command handlers

async fn handle_send_tx(
    storage: &WalletStorage,
    wallet: &str,
    account: u32,
    address: &str,
    amount: u64,
    fee: Option<u64>,
    socket: Option<&str>,
    magic: Option<u64>,
    out_file: &str,
    _multiasset: bool,
    ttl: Option<u64>,
    sign: bool,
    utxorpc_endpoint: Option<&str>,
) -> Result<()> {
    println!("🔨 Building transaction...");

    // Get UTxORPC endpoint
    let endpoint = utxorpc_endpoint
        .map(|s| s.to_string())
        .unwrap_or_else(|| "http://127.0.0.1:50051".to_string());

    // Connect to UTxORPC
    let mut client = WalletUtxorpcClient::connect(endpoint)
        .await
        .context("Failed to connect to UTxORPC endpoint")?;

    // Parse recipient address
    let recipient_addr = Address::from_bech32(address)
        .context("Invalid recipient address")?;
    let recipient_bytes = recipient_addr.to_vec();

    // Calculate fee if not provided
    let fee = match fee {
        Some(f) => {
            println!("📊 Using provided fee: {} lovelace", f);
            f
        }
        None => {
            println!("📊 Calculating fee automatically...");

            // Require socket and magic for automatic fee calculation
            let socket_path = socket.context("--socket required for automatic fee calculation")?;
            let network_magic = magic.context("--magic required for automatic fee calculation")?;

            // Query protocol parameters
            use crate::node::ProtocolParamQuery;
            let mut query = ProtocolParamQuery::new(socket_path.to_string(), network_magic);
            let params = query.query_current_params().await
                .context("Failed to query protocol parameters")?;

            // Estimate transaction size (rough estimate)
            // A simple send transaction is approximately 300-400 bytes
            let estimated_tx_size = 350u64;
            let calculated_fee = params.calculate_min_fee(estimated_tx_size);

            println!("   Estimated tx size: {} bytes", estimated_tx_size);
            println!("   Calculated fee:    {} lovelace ({:.3} ADA)",
                calculated_fee, calculated_fee as f64 / 1_000_000.0);
            println!("   (minFeeA={}, minFeeB={})", params.min_fee_a, params.min_fee_b);

            calculated_fee
        }
    };

    // Load wallet metadata and derive account
    let metadata = storage.load_metadata(wallet)
        .context("Failed to load wallet")?;
    let network = parse_network(&metadata.network)?;
    let account_keys = storage.derive_account(wallet, account)
        .context("Failed to derive account keys")?;

    // Derive first 20 payment addresses to check for UTxOs
    let mut payment_addresses = Vec::new();
    for i in 0..20 {
        let addr = derive_payment_address(
            &account_keys.payment_key,
            i,
            &account_keys.stake_key,
            network,
        ).context("Failed to derive payment address")?;

        let parsed_addr = Address::from_bech32(&addr)
            .context("Failed to parse derived address")?;
        payment_addresses.push(parsed_addr.to_vec());
    }

    println!("📡 Querying UTxOs from {} addresses...", payment_addresses.len());

    // Query UTxOs
    let utxos = client.query_utxos(payment_addresses.clone())
        .await
        .context("Failed to query UTxOs")?;

    if utxos.is_empty() {
        anyhow::bail!("No UTxOs found for this wallet");
    }

    println!("✅ Found {} UTxO(s)", utxos.len());

    // Build transaction
    let mut builder = TransactionBuilder::new(payment_addresses[0].clone());
    builder.add_utxos(utxos);
    builder.add_output(recipient_bytes, amount, Vec::new());
    builder.set_fee(fee);
    if let Some(ttl_value) = ttl {
        builder.set_ttl(ttl_value);
    }

    let (tx_body_cbor, selected_utxos) = builder.build()
        .context("Failed to build transaction")?;

    println!("✅ Transaction built successfully");
    println!("   Selected {} UTxO(s) as inputs", selected_utxos.len());

    // Write transaction body
    if sign {
        // Sign the transaction
        println!("✍️  Signing transaction...");
        let signed_tx = sign_transaction(&tx_body_cbor, &account_keys, false)
            .context("Failed to sign transaction")?;

        write_signed_tx(&signed_tx, out_file)
            .context("Failed to write signed transaction")?;

        println!("✅ Signed transaction written to: {}", out_file);
        println!("   Submit with: cardano-cli transaction submit --tx-file {}", out_file);
    } else {
        write_tx_body(&tx_body_cbor, out_file)
            .context("Failed to write transaction body")?;

        println!("✅ Transaction body written to: {}", out_file);
        println!("   Sign with: hayate wallet sign-tx --wallet {} --tx-body-file {} --out-file {}.signed",
                 wallet, out_file, out_file);
    }

    Ok(())
}

async fn handle_drain_tx(
    storage: &WalletStorage,
    wallet: &str,
    account: u32,
    address: &str,
    fee: Option<u64>,
    socket: Option<&str>,
    magic: Option<u64>,
    out_file: &str,
    _multiasset: bool,
    _rewards: bool,
    ttl: Option<u64>,
    sign: bool,
    utxorpc_endpoint: Option<&str>,
) -> Result<()> {
    println!("🔨 Building drain transaction...");

    // Get UTxORPC endpoint
    let endpoint = utxorpc_endpoint
        .map(|s| s.to_string())
        .unwrap_or_else(|| "http://127.0.0.1:50051".to_string());

    // Connect to UTxORPC
    let mut client = WalletUtxorpcClient::connect(endpoint)
        .await
        .context("Failed to connect to UTxORPC endpoint")?;

    // Parse recipient address
    let recipient_addr = Address::from_bech32(address)
        .context("Invalid recipient address")?;
    let recipient_bytes = recipient_addr.to_vec();

    // Calculate fee if not provided
    let fee = match fee {
        Some(f) => {
            println!("📊 Using provided fee: {} lovelace", f);
            f
        }
        None => {
            println!("📊 Calculating fee automatically...");

            // Require socket and magic for automatic fee calculation
            let socket_path = socket.context("--socket required for automatic fee calculation")?;
            let network_magic = magic.context("--magic required for automatic fee calculation")?;

            // Query protocol parameters
            use crate::node::ProtocolParamQuery;
            let mut query = ProtocolParamQuery::new(socket_path.to_string(), network_magic);
            let params = query.query_current_params().await
                .context("Failed to query protocol parameters")?;

            // Estimate transaction size (drain tx is similar to send tx)
            let estimated_tx_size = 350u64;
            let calculated_fee = params.calculate_min_fee(estimated_tx_size);

            println!("   Estimated tx size: {} bytes", estimated_tx_size);
            println!("   Calculated fee:    {} lovelace ({:.3} ADA)",
                calculated_fee, calculated_fee as f64 / 1_000_000.0);
            println!("   (minFeeA={}, minFeeB={})", params.min_fee_a, params.min_fee_b);

            calculated_fee
        }
    };

    // Load wallet metadata and derive account
    let metadata = storage.load_metadata(wallet)
        .context("Failed to load wallet")?;
    let network = parse_network(&metadata.network)?;
    let account_keys = storage.derive_account(wallet, account)
        .context("Failed to derive account keys")?;

    // Derive first 20 payment addresses
    let mut payment_addresses = Vec::new();
    for i in 0..20 {
        let addr = derive_payment_address(
            &account_keys.payment_key,
            i,
            &account_keys.stake_key,
            network,
        ).context("Failed to derive payment address")?;

        let parsed_addr = Address::from_bech32(&addr)
            .context("Failed to parse derived address")?;
        payment_addresses.push(parsed_addr.to_vec());
    }

    println!("📡 Querying UTxOs from {} addresses...", payment_addresses.len());

    // Query UTxOs
    let utxos = client.query_utxos(payment_addresses.clone())
        .await
        .context("Failed to query UTxOs")?;

    if utxos.is_empty() {
        anyhow::bail!("No UTxOs found for this wallet");
    }

    // Calculate total balance
    let total_lovelace: u64 = utxos.iter().map(|u| u.coin).sum();
    let drain_amount = total_lovelace.saturating_sub(fee);

    println!("✅ Found {} UTxO(s)", utxos.len());
    println!("   Total balance: {} lovelace", total_lovelace);
    println!("   Fee: {} lovelace", fee);
    println!("   Draining: {} lovelace", drain_amount);

    if drain_amount == 0 {
        anyhow::bail!("Insufficient funds to cover fee");
    }

    // Build transaction (use all UTxOs, send everything minus fee)
    let mut builder = TransactionBuilder::new(payment_addresses[0].clone());
    builder.add_utxos(utxos);
    builder.add_output(recipient_bytes, drain_amount, Vec::new());
    builder.set_fee(fee);
    if let Some(ttl_value) = ttl {
        builder.set_ttl(ttl_value);
    }

    let (tx_body_cbor, selected_utxos) = builder.build()
        .context("Failed to build transaction")?;

    println!("✅ Drain transaction built successfully");
    println!("   Using {} UTxO(s) as inputs", selected_utxos.len());

    // Write or sign transaction
    if sign {
        println!("✍️  Signing transaction...");
        let signed_tx = sign_transaction(&tx_body_cbor, &account_keys, false)
            .context("Failed to sign transaction")?;

        write_signed_tx(&signed_tx, out_file)
            .context("Failed to write signed transaction")?;

        println!("✅ Signed transaction written to: {}", out_file);
        println!("   Submit with: cardano-cli transaction submit --tx-file {}", out_file);
    } else {
        write_tx_body(&tx_body_cbor, out_file)
            .context("Failed to write transaction body")?;

        println!("✅ Transaction body written to: {}", out_file);
        println!("   Sign with: hayate wallet sign-tx --wallet {} --tx-body-file {} --out-file {}.signed",
                 wallet, out_file, out_file);
    }

    Ok(())
}

async fn handle_stake_registration(
    _storage: &WalletStorage,
    _wallet: &str,
    _account: u32,
    _fee: u64,
    _out_file: &str,
    _deposit: u64,
    _ttl: Option<u64>,
    _sign: bool,
) -> Result<()> {
    println!("⚠️  stake-registration-tx command not yet implemented");
    println!("   Requires certificate encoding in transaction body");
    println!("   This will be implemented in the next phase");
    Ok(())
}

async fn handle_delegate_pool(
    _storage: &WalletStorage,
    _wallet: &str,
    _account: u32,
    _pool_id: &str,
    _fee: u64,
    _out_file: &str,
    _ttl: Option<u64>,
    _sign: bool,
) -> Result<()> {
    println!("⚠️  delegate-pool-tx command not yet implemented");
    println!("   Requires certificate encoding in transaction body");
    println!("   This will be implemented in the next phase");
    Ok(())
}

fn handle_sign_tx(
    storage: &WalletStorage,
    wallet: &str,
    account: u32,
    tx_body_file: &str,
    out_file: &str,
    stake: bool,
) -> Result<()> {
    println!("✍️  Signing transaction...");

    // Read transaction body from file (hex-encoded)
    let tx_body_hex = std::fs::read_to_string(tx_body_file)
        .context("Failed to read transaction body file")?;
    let tx_body_cbor = hex::decode(tx_body_hex.trim())
        .context("Failed to decode hex transaction body")?;

    println!("📄 Read transaction body: {} bytes", tx_body_cbor.len());

    // Load account keys
    let account_keys = storage.derive_account(wallet, account)
        .context("Failed to derive account keys")?;

    // Sign transaction
    let signed_tx = sign_transaction(&tx_body_cbor, &account_keys, stake)
        .context("Failed to sign transaction")?;

    println!("✅ Transaction signed successfully");
    if stake {
        println!("   Signed with both payment and stake keys");
    } else {
        println!("   Signed with payment key only");
    }

    // Write signed transaction
    write_signed_tx(&signed_tx, out_file)
        .context("Failed to write signed transaction")?;

    println!("✅ Signed transaction written to: {}", out_file);
    println!("   Submit with: cardano-cli transaction submit --tx-file {}", out_file);

    Ok(())
}

fn handle_witness_tx(
    _storage: &WalletStorage,
    _wallet: &str,
    _account: u32,
    _tx_body_file: &str,
    _out_file: &str,
    role: &str,
) -> Result<()> {
    println!("📝 Creating witness for transaction...");

    // Validate role
    if role != "payment" && role != "stake" {
        anyhow::bail!("Role must be 'payment' or 'stake'");
    }

    println!("⚠️  witness-tx command requires witness set generation");
    println!("   This will be fully implemented in the next phase");
    println!("   For now, use sign-tx to create a fully signed transaction");

    Ok(())
}

fn handle_sign_msg(
    _storage: &WalletStorage,
    _wallet: &str,
    _account: u32,
    _msg_file: &str,
    _out_file: &str,
    _stake: bool,
    _hashed: bool,
) -> Result<()> {
    println!("⚠️  sign-msg command requires CIP-8 message signing implementation");
    println!("   This will be implemented in the next phase");
    Ok(())
}
