// CLI handlers for CIP-1854 multisig wallet commands

use crate::cli::MultisigCommand;
use crate::wallet::{
    WalletStorage,
    derivation::Network,
    multisig::{
        derive_multisig_payment_key, encode_key_witness,
        extract_tx_body, key_enterprise_address, payment_keyhash_from_address, sign_multisig_tx, vkey_hash,
    },
    portal_cli::handle_portal_command,
};
use anyhow::{Context, Result};
use serde_json::json;

pub async fn handle_multisig_command(
    cmd: &MultisigCommand,
    storage: &WalletStorage,
) -> Result<()> {
    match cmd {
        MultisigCommand::PaymentAddress { wallet, wallet_account, wallet_key_index, network } => {
            let network = parse_network(network)?;
            let root = storage
                .derive_root(wallet)
                .with_context(|| format!("Failed to derive root key from wallet '{}'", wallet))?;
            let key = derive_multisig_payment_key(&root, *wallet_account, *wallet_key_index);
            let hash = vkey_hash(&key.public().public_key());
            let addr = key_enterprise_address(&hash, network)?;
            println!("{}", addr);
            Ok(())
        }

        MultisigCommand::CreateAddress {
            wallets,
            wallet_accounts,
            wallet_key_indices,
            addresses,
            threshold,
            network,
            policy_file,
        } => handle_create_address(
            storage,
            wallets,
            wallet_accounts,
            wallet_key_indices,
            addresses,
            *threshold,
            network,
            policy_file,
        ),

        MultisigCommand::Sign {
            wallet,
            wallet_account,
            wallet_key_index,
            tx,
            tx_cbor,
            out_file,
        } => handle_sign(
            storage,
            wallet,
            *wallet_account,
            *wallet_key_index,
            tx.as_deref(),
            tx_cbor.as_deref(),
            out_file,
        ),

        MultisigCommand::Portal { portal_cmd } => handle_portal_command(portal_cmd, storage).await,
    }
}

fn parse_network(s: &str) -> Result<Network> {
    match s.to_lowercase().as_str() {
        "mainnet" => Ok(Network::Mainnet),
        "testnet" | "preprod" | "preview" | "sanchonet" => Ok(Network::Testnet),
        _ => anyhow::bail!("Invalid network '{}': expected mainnet, testnet, preprod, preview, or sanchonet", s),
    }
}

fn handle_create_address(
    storage: &WalletStorage,
    wallets: &[String],
    wallet_accounts: &[u32],
    wallet_key_indices: &[u32],
    addresses: &[String],
    threshold: u32,
    network_str: &str,
    policy_file: &str,
) -> Result<()> {
    let network = parse_network(network_str)?;

    // Derive a CIP-1854 payment key for each local wallet, zipping accounts/indices (default 0).
    let mut all_hashes: Vec<[u8; 28]> = Vec::new();
    for (i, wallet_name) in wallets.iter().enumerate() {
        let account_index = wallet_accounts.get(i).copied().unwrap_or(0);
        let key_index = wallet_key_indices.get(i).copied().unwrap_or(0);

        let root = storage
            .derive_root(wallet_name)
            .with_context(|| format!("Failed to derive root key from wallet '{}'", wallet_name))?;

        let key = derive_multisig_payment_key(&root, account_index, key_index);
        all_hashes.push(vkey_hash(&key.public().public_key()));
    }

    // External cosigners supplied as payment addresses — extract key hash directly.
    for (i, addr) in addresses.iter().enumerate() {
        let hash = payment_keyhash_from_address(addr)
            .with_context(|| format!("Invalid --address at position {}: {}", i, addr))?;
        all_hashes.push(hash);
    }

    let n = all_hashes.len();
    if n == 0 {
        anyhow::bail!("At least one --wallet or --address is required");
    }
    if threshold == 0 || threshold as usize > n {
        anyhow::bail!(
            "Invalid threshold: M={} must be >= 1 and <= N={}",
            threshold,
            n
        );
    }
    let script_cbor =
        crate::wallet::multisig::encode_native_script_n_of_k(threshold, &all_hashes)
            .context("Failed to encode native script")?;
    let script_hash = crate::wallet::multisig::native_script_hash(&script_cbor);
    let address =
        crate::wallet::multisig::multisig_enterprise_address(&script_hash, network)
            .context("Failed to build address")?;

    // Build cardano-cli native script JSON
    let scripts: Vec<serde_json::Value> = all_hashes
        .iter()
        .map(|h| json!({ "type": "sig", "keyHash": hex::encode(h) }))
        .collect();

    let policy_json = json!({
        "type": "atLeast",
        "required": threshold,
        "scripts": scripts,
    });

    std::fs::write(
        policy_file,
        serde_json::to_string_pretty(&policy_json).unwrap(),
    )
    .with_context(|| format!("Failed to write policy file: {}", policy_file))?;

    println!("Multisig address ({}-of-{} enterprise):", threshold, n);
    println!("  {}", address);
    println!();
    println!("Script hash:");
    println!("  {}", hex::encode(script_hash));
    println!();

    println!("Individual signer addresses (for portal registration):");
    for (i, hash) in all_hashes.iter().enumerate() {
        let label = if i < wallets.len() {
            format!("wallet '{}' account={} key={}", wallets[i],
                wallet_accounts.get(i).copied().unwrap_or(0),
                wallet_key_indices.get(i).copied().unwrap_or(0))
        } else {
            format!("external vkey[{}]", i - wallets.len())
        };
        let signer_addr = key_enterprise_address(hash, network)
            .with_context(|| format!("Failed to build signer address for {}", label))?;
        println!("  [{}] {}: {}", i, label, signer_addr);
    }
    println!();

    println!("Policy written to: {}", policy_file);

    Ok(())
}

fn handle_sign(
    storage: &WalletStorage,
    wallet: &str,
    wallet_account: u32,
    wallet_key_index: u32,
    tx_file: Option<&str>,
    tx_cbor_hex: Option<&str>,
    out_file: &str,
) -> Result<()> {
    let tx_body_cbor = if let Some(file) = tx_file {
        let contents = std::fs::read_to_string(file)
            .with_context(|| format!("Failed to read tx file: {}", file))?;
        let envelope: serde_json::Value =
            serde_json::from_str(&contents).context("Failed to parse tx file as JSON")?;

        let cbor_hex = if let Some(hex) = envelope["txCbor"].as_str() {
            // portal build-tx format: { txCbor, txHash, txJson }
            hex.to_string()
        } else {
            // cardano-cli envelope format: { type, cborHex }
            let tx_type = envelope["type"]
                .as_str()
                .context("Missing 'type' or 'txCbor' field in tx file")?;
            if tx_type != "Tx ConwayEra" && tx_type != "TxBody ConwayEra" {
                anyhow::bail!(
                    "Unsupported tx type '{}': expected 'Tx ConwayEra' or 'TxBody ConwayEra'",
                    tx_type
                );
            }
            envelope["cborHex"]
                .as_str()
                .context("Missing 'cborHex' field in tx envelope")?
                .to_string()
        };

        let tx_cbor = hex::decode(cbor_hex.trim()).context("Failed to hex-decode tx CBOR")?;
        extract_tx_body(&tx_cbor).context("Failed to extract tx body from CBOR")?
    } else if let Some(hex_str) = tx_cbor_hex {
        // Raw CBOR hex (e.g. from MeshJS .complete())
        let tx_cbor = hex::decode(hex_str.trim()).context("Failed to hex-decode --tx-cbor")?;
        extract_tx_body(&tx_cbor).context("Failed to extract tx body from CBOR")?
    } else {
        anyhow::bail!("One of --tx or --tx-cbor is required");
    };

    let root = storage
        .derive_root(wallet)
        .context("Failed to derive root key from wallet")?;

    let signing_key = derive_multisig_payment_key(&root, wallet_account, wallet_key_index);

    let (vkey_32, sig_64) = sign_multisig_tx(&tx_body_cbor, &signing_key);

    let witness_cbor =
        encode_key_witness(&vkey_32, &sig_64).context("Failed to encode key witness")?;

    let witness_json = json!({
        "type": "TxWitness ConwayEra",
        "description": "",
        "cborHex": hex::encode(&witness_cbor),
        // Raw key and signature for submitting to APIs like MeshJS multisig portal
        // (POST /api/v1/signTransaction expects separate "key" and "signature" fields)
        "key": hex::encode(&vkey_32),
        "signature": hex::encode(&sig_64),
    });

    std::fs::write(
        out_file,
        serde_json::to_string_pretty(&witness_json).unwrap(),
    )
    .with_context(|| format!("Failed to write witness file: {}", out_file))?;

    let key_hash = vkey_hash(&vkey_32.try_into().unwrap());

    println!("Witness written to: {}", out_file);
    println!("Key hash: {}", hex::encode(key_hash));

    Ok(())
}
