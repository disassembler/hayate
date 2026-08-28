// CLI handlers for MeshJS multisig portal integration

use crate::cli::PortalCommand;
use crate::wallet::{
    WalletStorage,
    multisig::{derive_multisig_payment_key, embed_vkey_witness, encode_native_script_n_of_k},
    portal::{portal_complete_setup, portal_init, portal_sign_setup, PortalClient, PortalConfig},
    tx_builder::{PlutusTransactionBuilder, PlutusInput, PlutusOutput},
    utxorpc_client::UtxoData,
    plutus::Network as PlutusNetwork,
};
use anyhow::{Context, Result};

pub async fn handle_portal_command(
    cmd: &PortalCommand,
    storage: &WalletStorage,
) -> Result<()> {
    match cmd {
        PortalCommand::Init { name, address, portal_url, out } => {
            portal_init(portal_url, name, address, out).await
        }

        PortalCommand::SignSetup {
            wallet,
            wallet_account,
            wallet_key_index,
            in_file,
            out,
        } => handle_sign_setup(wallet, *wallet_account, *wallet_key_index, in_file, out, storage),

        PortalCommand::CompleteSetup { signed, creds } => {
            portal_complete_setup(signed, creds).await
        }

        PortalCommand::CreateWallet { name, signers, threshold, network, description, creds } => {
            handle_create_wallet(name, signers, *threshold, network, description.as_deref(), creds).await
        }

        PortalCommand::Wallets { creds } => {
            handle_wallets(creds).await
        }

        PortalCommand::BuildTx {
            utxos, to, amount, change, fee, policy_file, ttl, network, out,
        } => handle_build_tx(utxos, to, *amount, change, *fee, policy_file, *ttl, network, out),

        PortalCommand::ProposeTx { tx_file, witness_file, wallet_id, description, creds } => {
            handle_propose_tx(tx_file, witness_file, wallet_id, description.as_deref(), creds).await
        }

        PortalCommand::Fetch { wallet_id, creds, out_dir } => {
            handle_fetch(wallet_id, creds, out_dir.as_deref()).await
        }

        PortalCommand::SubmitWitness {
            wallet_id,
            transaction_id,
            witness_file,
            creds,
            no_broadcast,
        } => handle_submit_witness(wallet_id, transaction_id, witness_file, creds, !no_broadcast).await,

    }
}

fn handle_sign_setup(
    wallet_name: &str,
    wallet_account: u32,
    wallet_key_index: u32,
    in_file: &str,
    out_file: &str,
    storage: &WalletStorage,
) -> Result<()> {
    let root = storage
        .derive_root(wallet_name)
        .with_context(|| format!("Failed to derive root key from wallet '{}'", wallet_name))?;
    let signing_key = derive_multisig_payment_key(&root, wallet_account, wallet_key_index);
    portal_sign_setup(in_file, &signing_key, out_file)
}

const WALLETS_CACHE: &str = "portal-wallets.json";

fn load_wallets_cache() -> serde_json::Value {
    std::fs::read_to_string(WALLETS_CACHE)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::json!({}))
}

fn save_wallets_cache(cache: &serde_json::Value) {
    if let Ok(s) = serde_json::to_string_pretty(cache) {
        let _ = std::fs::write(WALLETS_CACHE, s);
    }
}

async fn handle_create_wallet(
    name: &str,
    signers: &[String],
    threshold: u32,
    network_str: &str,
    description: Option<&str>,
    creds_path: &str,
) -> Result<()> {
    let network_byte: u8 = match network_str.to_lowercase().as_str() {
        "mainnet" => 1,
        _ => 0,
    };

    let cfg = PortalConfig::load(creds_path)?;
    let portal = PortalClient::new(&cfg).await?;

    let result = portal
        .create_wallet(name, signers, threshold, network_byte, description)
        .await?;

    let wallet_id = result["walletId"].as_str().unwrap_or("?");
    let address = result["address"].as_str().unwrap_or("?");

    let mut cache = load_wallets_cache();
    cache[wallet_id] = serde_json::json!({ "name": name, "address": address });
    save_wallets_cache(&cache);

    println!("Wallet created.");
    println!("Wallet ID: {}", wallet_id);
    println!("Address:   {}", address);

    Ok(())
}

async fn handle_wallets(creds_path: &str) -> Result<()> {
    let cfg = PortalConfig::load(creds_path)?;
    let portal = PortalClient::new(&cfg).await?;

    let wallets = portal.list_wallets().await?;

    if wallets.is_empty() {
        println!("No wallets found for this bot.");
        return Ok(());
    }

    let cache = load_wallets_cache();

    println!("{} wallet(s):", wallets.len());
    for w in &wallets {
        let id = w["walletId"].as_str().unwrap_or("?");
        let name = w["walletName"].as_str().unwrap_or("?");
        let address = cache[id]["address"].as_str().unwrap_or("(run create-wallet to cache address)");
        println!("  {} — {}", id, name);
        println!("    {}", address);
    }

    Ok(())
}

fn build_tx_cbor(
    utxo_strs: &[String],
    to: &str,
    amount: u64,
    change_addr: &str,
    fee: u64,
    policy_file: &str,
    ttl: Option<u64>,
    network_str: &str,
) -> Result<(Vec<u8>, Vec<u8>, serde_json::Value)> {
    let policy_json: serde_json::Value = {
        let text = std::fs::read_to_string(policy_file)
            .with_context(|| format!("Failed to read policy file: {}", policy_file))?;
        serde_json::from_str(&text).context("Failed to parse policy file")?
    };
    let threshold = policy_json["required"]
        .as_u64()
        .context("Policy file missing 'required' field")? as u32;
    let key_hashes: Vec<[u8; 28]> = policy_json["scripts"]
        .as_array()
        .context("Policy file missing 'scripts' array")?
        .iter()
        .map(|s| {
            let hex_str = s["keyHash"].as_str().context("Script missing keyHash")?;
            let bytes = hex::decode(hex_str).context("Invalid keyHash hex")?;
            bytes.try_into().map_err(|_| anyhow::anyhow!("keyHash must be 28 bytes"))
        })
        .collect::<Result<_>>()?;
    let native_script_cbor =
        encode_native_script_n_of_k(threshold, &key_hashes).context("Failed to encode native script")?;

    let mut total_input: u64 = 0;
    let mut utxo_inputs: Vec<UtxoData> = Vec::new();
    for s in utxo_strs {
        let (txref, lovelace_str) = s.rsplit_once(':')
            .with_context(|| format!("Invalid --utxo format (expected txid#index:lovelace): {}", s))?;
        let (txid_hex, index_str) = txref.split_once('#')
            .with_context(|| format!("Invalid --utxo format (expected txid#index:lovelace): {}", s))?;
        let tx_hash = hex::decode(txid_hex)
            .with_context(|| format!("Invalid tx hash hex in --utxo: {}", txid_hex))?;
        let output_index: u32 = index_str.parse()
            .with_context(|| format!("Invalid output index in --utxo: {}", index_str))?;
        let lovelace: u64 = lovelace_str.parse()
            .with_context(|| format!("Invalid lovelace in --utxo: {}", lovelace_str))?;
        total_input += lovelace;
        utxo_inputs.push(UtxoData {
            tx_hash,
            output_index,
            address: vec![],
            coin: lovelace,
            assets: vec![],
            datum_hash: None,
            datum: None,
        });
    }

    let change_amount = total_input
        .checked_sub(amount + fee)
        .with_context(|| format!(
            "Inputs ({} lovelace) insufficient to cover amount ({}) + fee ({})",
            total_input, amount, fee
        ))?;

    let to_bytes = pallas_addresses::Address::from_bech32(to)
        .with_context(|| format!("Invalid recipient address: {}", to))?
        .to_vec();
    let change_bytes = pallas_addresses::Address::from_bech32(change_addr)
        .with_context(|| format!("Invalid change address: {}", change_addr))?
        .to_vec();

    let network = match network_str.to_lowercase().as_str() {
        "mainnet" => PlutusNetwork::Mainnet,
        _ => PlutusNetwork::Testnet,
    };

    let mut builder = PlutusTransactionBuilder::new(network, change_bytes.clone());
    for utxo in &utxo_inputs {
        builder.add_input(&PlutusInput::regular(utxo.clone())).context("Failed to add input")?;
    }
    builder.add_output(&PlutusOutput::new(to_bytes, amount)).context("Failed to add output")?;
    if change_amount > 0 {
        builder.add_output(&PlutusOutput::new(change_bytes, change_amount))
            .context("Failed to add change output")?;
    }
    builder.add_native_script(native_script_cbor).context("Failed to add native script")?;
    builder.set_fee(fee).set_network_id();
    if let Some(t) = ttl { builder.set_ttl(t); }

    let (tx_bytes, tx_hash) = builder.build().context("Failed to build transaction")?;
    let tx_hash_hex = hex::encode(&tx_hash);

    // MeshJS MeshTxBuilderBody format so the portal's transaction renderer works.
    let mesh_inputs: Vec<serde_json::Value> = utxo_strs.iter().zip(utxo_inputs.iter())
        .map(|(s, utxo)| {
            let txid_hex = hex::encode(&utxo.tx_hash);
            let _ = s; // utxo_strs only needed for error messages above
            serde_json::json!({
                "type": "Script",
                "txIn": {
                    "txHash": txid_hex,
                    "txIndex": utxo.output_index,
                    "amount": [{"unit": "lovelace", "quantity": utxo.coin.to_string()}],
                    "address": change_addr,
                }
            })
        })
        .collect();

    let mut mesh_outputs: Vec<serde_json::Value> = vec![
        serde_json::json!({
            "address": to,
            "amount": [{"unit": "lovelace", "quantity": amount.to_string()}],
        }),
    ];
    if change_amount > 0 {
        mesh_outputs.push(serde_json::json!({
            "address": change_addr,
            "amount": [{"unit": "lovelace", "quantity": change_amount.to_string()}],
        }));
    }

    let tx_json = serde_json::json!({
        "inputs": mesh_inputs,
        "outputs": mesh_outputs,
        "fee": fee.to_string(),
        "changeAddress": change_addr,
    });

    Ok((tx_bytes, tx_hash, tx_json))
}

fn handle_build_tx(
    utxo_strs: &[String],
    to: &str,
    amount: u64,
    change_addr: &str,
    fee: u64,
    policy_file: &str,
    ttl: Option<u64>,
    network_str: &str,
    out_path: &str,
) -> Result<()> {
    let (tx_bytes, tx_hash, tx_json) =
        build_tx_cbor(utxo_strs, to, amount, change_addr, fee, policy_file, ttl, network_str)?;

    let tx_hash_hex = hex::encode(&tx_hash);
    let out = serde_json::json!({
        "txCbor": hex::encode(&tx_bytes),
        "txHash": tx_hash_hex,
        "txJson": tx_json,
    });
    std::fs::write(out_path, serde_json::to_string_pretty(&out).unwrap())
        .with_context(|| format!("Failed to write {}", out_path))?;

    println!("Unsigned tx written to: {}", out_path);
    println!("Tx hash: {}", tx_hash_hex);
    println!();
    println!("Next: transfer '{}' to the air-gapped machine and run:", out_path);
    println!("  hayate wallet multisig sign --wallet <name> --wallet-account <n> \\");
    println!("    --tx-cbor <txCbor> --out-file witness-1.json");
    println!("Then bring witness-1.json back and run: portal propose-tx");

    Ok(())
}

async fn handle_propose_tx(
    tx_file: &str,
    witness_file: &str,
    wallet_id: &str,
    description: Option<&str>,
    creds_path: &str,
) -> Result<()> {
    let tx_envelope: serde_json::Value = {
        let text = std::fs::read_to_string(tx_file)
            .with_context(|| format!("Failed to read tx file: {}", tx_file))?;
        serde_json::from_str(&text).context("Failed to parse tx file")?
    };
    let tx_cbor_hex = tx_envelope["txCbor"].as_str().context("Missing txCbor in tx file")?;
    let tx_json = tx_envelope["txJson"].clone();
    let tx_cbor = hex::decode(tx_cbor_hex).context("Failed to decode txCbor")?;

    let witness: serde_json::Value = {
        let text = std::fs::read_to_string(witness_file)
            .with_context(|| format!("Failed to read witness file: {}", witness_file))?;
        serde_json::from_str(&text).context("Failed to parse witness file")?
    };
    let key_hex = witness["key"].as_str()
        .context("Missing 'key' field in witness file — sign with 'wallet multisig sign'")?;
    let sig_hex = witness["signature"].as_str()
        .context("Missing 'signature' field in witness file")?;
    let vkey = hex::decode(key_hex).context("Invalid key hex in witness")?;
    let sig = hex::decode(sig_hex).context("Invalid signature hex in witness")?;

    let signed_cbor = embed_vkey_witness(&tx_cbor, &vkey, &sig)
        .context("Failed to embed witness into tx")?;
    let signed_hex = hex::encode(&signed_cbor);

    let cfg = PortalConfig::load(creds_path)?;
    let portal = PortalClient::new(&cfg).await?;
    let result = portal.add_transaction(wallet_id, &signed_hex, tx_json, description).await?;

    let tx_id = result["id"].as_str().unwrap_or("?");
    println!("Transaction proposed.");
    println!("Transaction ID: {}", tx_id);
    println!("Tx hash:        {}", tx_envelope["txHash"].as_str().unwrap_or("?"));

    Ok(())
}

async fn handle_fetch(
    wallet_id: &str,
    creds_path: &str,
    out_dir: Option<&str>,
) -> Result<()> {
    let cfg = PortalConfig::load(creds_path)?;
    let portal = PortalClient::new(&cfg).await?;

    let txs = portal.fetch_pending(wallet_id).await?;

    if txs.is_empty() {
        println!("No pending transactions for wallet {}", wallet_id);
        return Ok(());
    }

    println!("{} pending transaction(s) for wallet {}:", txs.len(), wallet_id);

    for tx in &txs {
        let id = tx["id"].as_str().unwrap_or("?");
        let signed = tx["signedAddresses"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        let tx_cbor = tx["txCbor"].as_str().unwrap_or("");
        let preview = &tx_cbor[..tx_cbor.len().min(32)];

        println!();
        println!("  Transaction ID: {}", id);
        println!("  Signed by {} address(es)", signed);
        println!("  txCbor: {}{}", preview, if tx_cbor.len() > 32 { "..." } else { "" });

        if let Some(dir) = out_dir {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("Failed to create output dir: {}", dir))?;
            let path = format!("{}/{}.json", dir.trim_end_matches('/'), id);
            std::fs::write(&path, serde_json::to_string_pretty(tx).unwrap())
                .with_context(|| format!("Failed to write tx file: {}", path))?;
            println!("  Saved to: {}", path);
        }
    }

    if out_dir.is_none() {
        println!();
        println!("Tip: use --out-dir <dir> to save each transaction as a JSON file.");
        println!("     The txCbor field can be passed to 'wallet multisig sign --tx-cbor <cbor>'");
    }

    Ok(())
}

async fn handle_submit_witness(
    wallet_id: &str,
    transaction_id: &str,
    witness_file: &str,
    creds_path: &str,
    broadcast: bool,
) -> Result<()> {
    let contents = std::fs::read_to_string(witness_file)
        .with_context(|| format!("Failed to read witness file: {}", witness_file))?;
    let witness: serde_json::Value =
        serde_json::from_str(&contents).context("Failed to parse witness file as JSON")?;

    let key_hex = witness["key"]
        .as_str()
        .context("Missing 'key' field in witness file — sign with 'wallet multisig sign'")?;
    let sig_hex = witness["signature"]
        .as_str()
        .context("Missing 'signature' field in witness file")?;

    let cfg = PortalConfig::load(creds_path)?;
    let portal = PortalClient::new(&cfg).await?;

    let result = portal
        .submit_witness(wallet_id, transaction_id, key_hex, sig_hex, broadcast)
        .await?;

    let submitted = result["submitted"].as_bool().unwrap_or(false);
    let tx_hash = result["txHash"].as_str().unwrap_or("");

    if submitted {
        println!("Transaction submitted to the network.");
        println!("Tx hash: {}", tx_hash);
    } else {
        println!("Witness recorded. Waiting for other signers.");
    }

    if let Some(err) = result["submissionError"].as_str() {
        println!("Submission error: {}", err);
    }

    Ok(())
}
