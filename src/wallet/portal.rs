// MeshJS multisig portal REST client — air-gap friendly split flow

use anyhow::{Context, Result};
use ed25519_bip32::XPrv;
use pallas_addresses::{Address, Network, ShelleyDelegationPart};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::wallet::multisig::sign_cip8;

// ── Persistent credentials (live machine, used for fetch/submit) ─────────────

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct PortalConfig {
    pub portal_url: String,
    pub payment_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

impl PortalConfig {
    pub fn load(path: &str) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read portal credentials: {}", path))?;
        serde_json::from_str(&contents).context("Failed to parse portal credentials")
    }

    pub fn save(&self, path: &str) -> Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap())
            .with_context(|| format!("Failed to write portal credentials: {}", path))
    }

    pub fn require_bot_credentials(&self) -> Result<(&str, &str)> {
        let bot_key_id = self
            .bot_key_id
            .as_deref()
            .context("No bot credentials — run 'portal complete-setup' first")?;
        let secret = self
            .secret
            .as_deref()
            .context("No bot credentials — run 'portal complete-setup' first")?;
        Ok((bot_key_id, secret))
    }
}

// ── Step 1 output: live machine → USB → air-gap ──────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct PortalInitFile {
    pub portal_url: String,
    pub payment_address: String,
    pub name: String,
    pub pending_bot_id: String,
    pub claim_code: String,
    pub nonce: String,
}

impl PortalInitFile {
    pub fn load(path: &str) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read portal init file: {}", path))?;
        serde_json::from_str(&contents).context("Failed to parse portal init file")
    }

    pub fn save(&self, path: &str) -> Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap())
            .with_context(|| format!("Failed to write portal init file: {}", path))
    }
}

// ── Step 2 output: air-gap → USB → live machine ──────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct PortalSignedFile {
    pub portal_url: String,
    pub payment_address: String,
    pub pending_bot_id: String,
    pub claim_code: String,
    pub signature: String,
    pub key: String,
}

impl PortalSignedFile {
    pub fn load(path: &str) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read portal signed file: {}", path))?;
        serde_json::from_str(&contents).context("Failed to parse portal signed file")
    }

    pub fn save(&self, path: &str) -> Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap())
            .with_context(|| format!("Failed to write portal signed file: {}", path))
    }
}

// ── Internal response types ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct NonceResponse {
    pub nonce: String,
}

#[derive(Deserialize)]
struct AuthResponse {
    pub token: String,
}

#[derive(Deserialize)]
struct RegisterResponse {
    #[serde(rename = "pendingBotId")]
    pub pending_bot_id: String,
    #[serde(rename = "claimCode")]
    pub claim_code: String,
}

#[derive(Deserialize)]
struct PickupResponse {
    #[serde(rename = "botKeyId")]
    pub bot_key_id: String,
    pub secret: String,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn base(portal_url: &str) -> String {
    portal_url.trim_end_matches('/').to_string()
}

async fn get_nonce(client: &Client, portal_url: &str, address: &str) -> Result<String> {
    let url = format!("{}/api/v1/getNonce?address={}", base(portal_url), address);
    let resp = client.get(&url).send().await.context("Failed to reach portal")?;
    let status = resp.status();
    let text = resp.text().await.context("Failed to read response")?;
    if !status.is_success() {
        anyhow::bail!("getNonce failed ({}): {}", status, text);
    }
    let r: NonceResponse =
        serde_json::from_str(&text).context("Failed to parse nonce response")?;
    Ok(r.nonce)
}

async fn auth_signer(
    client: &Client,
    portal_url: &str,
    address: &str,
    signature_hex: &str,
    key_hex: &str,
) -> Result<String> {
    let url = format!("{}/api/v1/authSigner", base(portal_url));
    let body = serde_json::json!({
        "address": address,
        "signature": signature_hex,
        "key": key_hex,
    });
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("Failed to reach portal")?;
    let status = resp.status();
    let text = resp.text().await.context("Failed to read response")?;
    if !status.is_success() {
        anyhow::bail!("authSigner failed ({}): {}", status, text);
    }
    let r: AuthResponse =
        serde_json::from_str(&text).context("Failed to parse auth response")?;
    Ok(r.token)
}

// ── Step 1: portal init (live machine, needs network) ────────────────────────

/// Register the bot and fetch a nonce. Saves everything to `out_path` for
/// transfer to the air-gapped machine.
pub async fn portal_init(
    portal_url: &str,
    name: &str,
    address: &str,
    out_path: &str,
) -> Result<()> {
    let client = Client::new();

    println!("Registering bot with portal...");
    let url = format!("{}/api/v1/botRegister", base(portal_url));
    let body = serde_json::json!({
        "name": name,
        "paymentAddress": address,
        "requestedScopes": ["multisig:read", "multisig:sign", "multisig:create"],
    });
    let resp = client.post(&url).json(&body).send().await.context("Failed to reach portal")?;
    let status = resp.status();
    let text = resp.text().await.context("Failed to read response")?;
    if !status.is_success() {
        anyhow::bail!("Bot registration failed ({}): {}", status, text);
    }
    let reg: RegisterResponse =
        serde_json::from_str(&text).context("Failed to parse registration response")?;
    println!("  Pending bot ID: {}", reg.pending_bot_id);

    println!("Fetching nonce for CIP-8 signing...");
    let nonce = get_nonce(&client, portal_url, address).await?;
    println!("  Done.");

    let init = PortalInitFile {
        portal_url: portal_url.to_string(),
        payment_address: address.to_string(),
        name: name.to_string(),
        pending_bot_id: reg.pending_bot_id,
        claim_code: reg.claim_code,
        nonce,
    };
    init.save(out_path)?;

    println!("Init file saved to: {}", out_path);
    println!();
    println!("Next: copy '{}' to the air-gapped machine and run:", out_path);
    println!("  hayate wallet multisig portal sign-setup --wallet <name> --in {}", out_path);

    Ok(())
}

// ── Step 2: portal sign-setup (air-gapped machine, no network) ───────────────

/// Read the init file, sign the nonce with CIP-8, write the signed file.
/// Runs entirely offline — no network calls.
pub fn portal_sign_setup(
    init_path: &str,
    signing_key: &XPrv,
    out_path: &str,
) -> Result<()> {
    let init = PortalInitFile::load(init_path)?;

    let addr = Address::from_bech32(&init.payment_address)
        .with_context(|| format!("Invalid address: {}", init.payment_address))?;
    let address_bytes = addr.to_vec();

    println!("Signing nonce with CIP-8...");
    let (signature, key) = sign_cip8(&init.nonce, signing_key, &address_bytes)
        .context("CIP-8 signing failed")?;
    println!("  Done.");

    let signed = PortalSignedFile {
        portal_url: init.portal_url,
        payment_address: init.payment_address,
        pending_bot_id: init.pending_bot_id,
        claim_code: init.claim_code,
        signature,
        key,
    };
    signed.save(out_path)?;

    println!("Signed file saved to: {}", out_path);
    println!();
    println!("Next: copy '{}' back to the live machine and run:", out_path);
    println!("  hayate wallet multisig portal complete-setup --signed {} --creds <out>", out_path);

    Ok(())
}

// ── Step 3: portal complete-setup (live machine, needs network) ──────────────

/// Submit the signed nonce, claim the bot, pick up credentials. Saves the
/// final credentials file for use with fetch/submit-witness.
pub async fn portal_complete_setup(signed_path: &str, creds_path: &str) -> Result<()> {
    let signed = PortalSignedFile::load(signed_path)?;
    let client = Client::new();

    println!("Authenticating via CIP-8 signature...");
    let token = auth_signer(
        &client,
        &signed.portal_url,
        &signed.payment_address,
        &signed.signature,
        &signed.key,
    )
    .await?;
    println!("  Authenticated.");

    println!("Claiming bot...");
    let url = format!("{}/api/v1/botClaim", base(&signed.portal_url));
    let body = serde_json::json!({
        "pendingBotId": signed.pending_bot_id,
        "claimCode": signed.claim_code,
        "approvedScopes": ["multisig:read", "multisig:sign", "multisig:create"],
    });
    let resp = client
        .post(&url)
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await
        .context("Failed to reach portal")?;
    let status = resp.status();
    let text = resp.text().await.context("Failed to read response")?;
    if !status.is_success() {
        anyhow::bail!("Bot claim failed ({}): {}", status, text);
    }
    println!("  Claimed.");

    println!("Picking up bot credentials...");
    let url = format!(
        "{}/api/v1/botPickupSecret?pendingBotId={}",
        base(&signed.portal_url),
        signed.pending_bot_id
    );
    let resp = client.get(&url).send().await.context("Failed to reach portal")?;
    let status = resp.status();
    let text = resp.text().await.context("Failed to read response")?;
    if !status.is_success() {
        anyhow::bail!("Secret pickup failed ({}): {}", status, text);
    }
    let pickup: PickupResponse =
        serde_json::from_str(&text).context("Failed to parse pickup response")?;
    println!("  Done.");

    let creds = PortalConfig {
        portal_url: signed.portal_url,
        payment_address: signed.payment_address,
        bot_key_id: Some(pickup.bot_key_id.clone()),
        secret: Some(pickup.secret),
    };
    creds.save(creds_path)?;

    println!("Credentials saved to: {}", creds_path);
    println!("Bot key ID: {}", pickup.bot_key_id);

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract the bech32 stake address from a base address, or return empty string
/// for enterprise addresses (which carry no stake credential).
fn stake_address_from_payment_address(addr_bech32: &str) -> String {
    let addr = match Address::from_bech32(addr_bech32) {
        Ok(a) => a,
        Err(_) => return String::new(),
    };
    let shelley = match addr {
        Address::Shelley(s) => s,
        _ => return String::new(),
    };
    let hash_bytes: [u8; 28] = match shelley.delegation() {
        ShelleyDelegationPart::Key(hash) => {
            let b: &[u8] = hash.as_ref();
            match b.try_into() {
                Ok(arr) => arr,
                Err(_) => return String::new(),
            }
        }
        _ => return String::new(),
    };
    // Stake address raw bytes: header + 28-byte key hash.
    // Header 0xe1 = mainnet key hash, 0xe0 = testnet key hash.
    let header: u8 = match shelley.network() {
        Network::Mainnet => 0xe1,
        Network::Testnet => 0xe0,
        _ => 0xe0,
    };
    let raw: Vec<u8> = std::iter::once(header).chain(hash_bytes).collect();
    let hrp = if header == 0xe1 { "stake" } else { "stake_test" };
    bech32_encode(hrp, &raw)
}

fn bech32_encode(hrp: &str, data: &[u8]) -> String {
    const CHARSET: &[u8] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
    fn polymod(values: &[u8]) -> u32 {
        const GEN: [u32; 5] = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
        let mut chk: u32 = 1;
        for &v in values {
            let b = chk >> 25;
            chk = ((chk & 0x1ffffff) << 5) ^ u32::from(v);
            for (i, &g) in GEN.iter().enumerate() {
                if (b >> i) & 1 == 1 { chk ^= g; }
            }
        }
        chk
    }
    // convert 8-bit groups to 5-bit
    let mut data5: Vec<u8> = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in data {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            data5.push(((acc >> bits) & 0x1f) as u8);
        }
    }
    if bits > 0 {
        data5.push(((acc << (5 - bits)) & 0x1f) as u8);
    }
    // build values for checksum: hrp expand + data + 6 zeros
    let mut values: Vec<u8> = hrp.bytes().map(|c| c >> 5).collect();
    values.push(0);
    values.extend(hrp.bytes().map(|c| c & 0x1f));
    values.extend_from_slice(&data5);
    values.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    let chk = polymod(&values) ^ 1;
    let checksum: Vec<u8> = (0..6).map(|i| ((chk >> (5 * (5 - i))) & 0x1f) as u8).collect();
    let mut result = format!("{}1", hrp);
    for &v in data5.iter().chain(&checksum) {
        result.push(CHARSET[v as usize] as char);
    }
    result
}

// ── Authenticated portal client (live machine, uses stored bot creds) ─────────

pub struct PortalClient {
    client: Client,
    base_url: String,
    token: String,
    pub address: String,
}

impl PortalClient {
    pub async fn new(config: &PortalConfig) -> Result<Self> {
        let (bot_key_id, secret) = config.require_bot_credentials()?;
        let client = Client::new();
        let url = format!("{}/api/v1/botAuth", base(&config.portal_url));
        let body = serde_json::json!({
            "botKeyId": bot_key_id,
            "secret": secret,
            "paymentAddress": config.payment_address,
        });
        let resp = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Failed to reach portal")?;
        let status = resp.status();
        let text = resp.text().await.context("Failed to read response")?;
        if !status.is_success() {
            anyhow::bail!("Bot auth failed ({}): {}", status, text);
        }
        let auth: AuthResponse =
            serde_json::from_str(&text).context("Failed to parse auth response")?;
        Ok(Self {
            client,
            base_url: base(&config.portal_url),
            token: auth.token,
            address: config.payment_address.clone(),
        })
    }

    pub async fn create_wallet(
        &self,
        name: &str,
        signers: &[String],
        threshold: u32,
        network: u8,
        description: Option<&str>,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/api/v1/createWallet", self.base_url);
        let stake_keys: Vec<String> = signers
            .iter()
            .map(|a| stake_address_from_payment_address(a))
            .collect();
        let mut body = serde_json::json!({
            "name": name,
            "signersAddresses": signers,
            "signersStakeKeys": stake_keys,
            "numRequiredSigners": threshold,
            "scriptType": "atLeast",
            "network": network,
        });
        if let Some(desc) = description {
            body["description"] = serde_json::Value::String(desc.to_string());
        }
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .context("Failed to reach portal")?;
        let status = resp.status();
        let text = resp.text().await.context("Failed to read response")?;
        if !status.is_success() {
            anyhow::bail!("createWallet failed ({}): {}", status, text);
        }
        serde_json::from_str(&text).context("Failed to parse createWallet response")
    }

    pub async fn list_wallets(&self) -> Result<Vec<serde_json::Value>> {
        let url = format!(
            "{}/api/v1/walletIds?address={}",
            self.base_url, self.address
        );
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("Failed to reach portal")?;
        let status = resp.status();
        let text = resp.text().await.context("Failed to read response")?;
        if !status.is_success() {
            anyhow::bail!("walletIds failed ({}): {}", status, text);
        }
        serde_json::from_str(&text).context("Failed to parse walletIds response")
    }

    pub async fn fetch_pending(&self, wallet_id: &str) -> Result<Vec<serde_json::Value>> {
        let url = format!(
            "{}/api/v1/pendingTransactions?walletId={}&address={}",
            self.base_url, wallet_id, self.address
        );
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("Failed to reach portal")?;
        let status = resp.status();
        let text = resp.text().await.context("Failed to read response")?;
        if !status.is_success() {
            anyhow::bail!("Fetch pending transactions failed ({}): {}", status, text);
        }
        serde_json::from_str(&text).context("Failed to parse transactions response")
    }

    pub async fn add_transaction(
        &self,
        wallet_id: &str,
        tx_cbor_hex: &str,
        tx_json: serde_json::Value,
        description: Option<&str>,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/api/v1/addTransaction", self.base_url);
        let body = serde_json::json!({
            "walletId": wallet_id,
            "address": self.address,
            "txCbor": tx_cbor_hex,
            "txJson": tx_json,
            "description": description.unwrap_or("External Tx"),
        });
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .context("Failed to reach portal")?;
        let status = resp.status();
        let text = resp.text().await.context("Failed to read response")?;
        if !status.is_success() {
            anyhow::bail!("addTransaction failed ({}): {}", status, text);
        }
        serde_json::from_str(&text).context("Failed to parse addTransaction response")
    }

    pub async fn submit_witness(
        &self,
        wallet_id: &str,
        transaction_id: &str,
        key_hex: &str,
        sig_hex: &str,
        broadcast: bool,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/api/v1/signTransaction", self.base_url);
        let body = serde_json::json!({
            "walletId": wallet_id,
            "transactionId": transaction_id,
            "address": self.address,
            "key": key_hex,
            "signature": sig_hex,
            "broadcast": broadcast,
        });
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .context("Failed to reach portal")?;
        let status = resp.status();
        let text = resp.text().await.context("Failed to read response")?;
        if !status.is_success() {
            anyhow::bail!("Submit witness failed ({}): {}", status, text);
        }
        serde_json::from_str(&text).context("Failed to parse response")
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stake_address_extracted_from_base_address() {
        let base = "addr_test1qrnj3jult9fnemf5y75rwhzarhvft27087zjqnz792vw2hkn8g7epxk6kuqydg07gtqpgv6m4vsd5zvrvdwj77jkggzs64gamz";
        assert_eq!(
            stake_address_from_payment_address(base),
            "stake_test1urfn50vsntdtwqzx58ly9sq5xdd6kgx6pxpkxhf00ftyypgdtq3u2"
        );
    }

    #[test]
    fn stake_address_empty_for_enterprise_address() {
        let enterprise = "addr_test1vrnj3jult9fnemf5y75rwhzarhvft27087zjqnz792vw2hs786tk2";
        assert_eq!(stake_address_from_payment_address(enterprise), "");
    }
}
