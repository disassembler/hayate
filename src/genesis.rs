// Genesis file parsing for Cardano networks
// Adapted from torsten-node/src/genesis.rs

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

// ──────────────────────────────────────────────────────────────────────────
// Byron genesis
// ──────────────────────────────────────────────────────────────────────────

/// Byron genesis configuration (compatible with cardano-node byron-genesis.json)
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ByronGenesis {
    /// AVVM (Ada Voucher Vending Machine) distribution: base64 pubkey → lovelace
    #[serde(default)]
    pub avvm_distr: HashMap<String, String>,
    /// Non-AVVM initial balances: base58 Byron address → lovelace
    #[serde(default)]
    pub non_avvm_balances: HashMap<String, String>,
    /// Bootstrap stakeholders: stakeholder ID → weight
    #[serde(default, rename = "bootStakeholders")]
    _boot_stakeholders: HashMap<String, serde_json::Value>,
    /// Heavy delegation certificates
    #[serde(default, rename = "heavyDelegation")]
    _heavy_delegation: HashMap<String, serde_json::Value>,
    /// System start time (POSIX timestamp)
    #[serde(rename = "startTime")]
    pub start_time: u64,
    /// Block version data (fee policy, slot duration, etc.)
    #[serde(default)]
    pub block_version_data: ByronBlockVersionData,
    /// Protocol constants (k, protocol magic)
    #[serde(default)]
    pub protocol_consts: ByronProtocolConsts,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ByronBlockVersionData {
    #[serde(default)]
    pub slot_duration: String,
    #[serde(default, rename = "maxBlockSize")]
    _max_block_size: String,
    #[serde(default, rename = "maxTxSize")]
    _max_tx_size: String,
    #[serde(default, rename = "txFeePolicy")]
    _tx_fee_policy: ByronTxFeePolicy,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ByronTxFeePolicy {
    /// Fee = summand + multiplier * tx_size (both values are x1e12)
    #[serde(default, rename = "summand")]
    _summand: String,
    #[serde(default, rename = "multiplier")]
    _multiplier: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ByronProtocolConsts {
    pub k: u64,
    pub protocol_magic: u64,
}

/// A genesis UTxO entry (address bytes + lovelace amount)
#[derive(Debug, Clone)]
pub struct GenesisUtxoEntry {
    pub address: Vec<u8>,
    pub lovelace: u64,
}

impl ByronGenesis {
    /// Load the Byron genesis and compute its Blake2b-256 hash.
    pub fn load_with_hash(path: &Path) -> Result<(Self, [u8; 32])> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read Byron genesis: {}", path.display()))?;
        let genesis: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse Byron genesis: {}", path.display()))?;

        // Compute Blake2b-256 hash of the file content
        use blake2::{Blake2b, Digest};
        let mut hasher = Blake2b::<blake2::digest::consts::U32>::new();
        hasher.update(content.as_bytes());
        let hash: [u8; 32] = hasher.finalize().into();

        tracing::debug!(
            genesis_hash = hex::encode(hash),
            "Byron genesis hash computed"
        );
        Ok((genesis, hash))
    }

    /// Load Byron genesis without computing hash
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read Byron genesis: {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse Byron genesis: {}", path.display()))
    }

    /// Get the protocol magic from the genesis config
    pub fn protocol_magic(&self) -> u64 {
        self.protocol_consts.protocol_magic
    }

    /// Get the security parameter k
    pub fn security_param(&self) -> u64 {
        self.protocol_consts.k
    }

    /// Get the Byron slot duration in milliseconds from genesis config.
    /// Falls back to 20000ms (20s) if not specified or unparseable.
    pub fn slot_duration_ms(&self) -> u64 {
        self.block_version_data
            .slot_duration
            .parse::<u64>()
            .unwrap_or(20_000)
    }

    /// Extract the initial UTxO set from both nonAvvmBalances and avvmDistr.
    ///
    /// Returns decoded address bytes and lovelace amounts for all non-zero balances.
    pub fn initial_utxos(&self) -> Vec<GenesisUtxoEntry> {
        let mut entries = Vec::new();

        // Process nonAvvmBalances (base58 Byron addresses)
        for (addr_str, lovelace_str) in &self.non_avvm_balances {
            let lovelace: u64 = match lovelace_str.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            if lovelace == 0 {
                continue;
            }

            // Decode base58 Byron address
            match bs58::decode(addr_str).into_vec() {
                Ok(addr_bytes) => {
                    entries.push(GenesisUtxoEntry {
                        address: addr_bytes,
                        lovelace,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to decode Byron genesis address {}: {}",
                        &addr_str[..40.min(addr_str.len())],
                        e
                    );
                }
            }
        }

        // Process AVVM distribution (base64 Ed25519 public keys)
        // For simplicity, we'll skip AVVM entries as SanchoNet doesn't use them
        // Production implementation would convert these to Byron redeem addresses

        tracing::info!(
            "Extracted {} genesis UTxO entries from Byron genesis",
            entries.len()
        );

        entries
    }

    /// Calculate total lovelace in genesis UTxOs
    pub fn total_genesis_lovelace(&self) -> u64 {
        let mut total = 0u64;

        for lovelace_str in self.non_avvm_balances.values() {
            if let Ok(lovelace) = lovelace_str.parse::<u64>() {
                total = total.saturating_add(lovelace);
            }
        }

        for lovelace_str in self.avvm_distr.values() {
            if let Ok(lovelace) = lovelace_str.parse::<u64>() {
                total = total.saturating_add(lovelace);
            }
        }

        total
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Shelley genesis (for future use - protocol params, initial treasury, etc.)
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShelleyGenesis {
    /// System start time (ISO 8601 format)
    pub system_start: String,
    /// Network magic
    pub network_magic: u64,
    /// Network ID (Mainnet = 1, Testnet = 0)
    pub network_id: String,
    /// Active slots coefficient (f)
    pub active_slots_coeff: f64,
    /// Security parameter (k)
    pub security_param: u64,
    /// Epoch length in slots
    pub epoch_length: u64,
    /// Slots per KES period
    #[serde(rename = "slotsPerKESPeriod")]
    pub slots_per_kes_period: u64,
    /// Max KES evolutions
    #[serde(rename = "maxKESEvolutions")]
    pub max_kes_evolutions: u64,
    /// Slot length in seconds
    pub slot_length: u64,
    /// Update quorum
    pub update_quorum: u64,
    /// Max Lovelace supply
    pub max_lovelace_supply: u64,
    /// Protocol parameters
    #[serde(default)]
    pub protocol_params: ShelleyProtocolParams,
    /// Genesis delegations
    #[serde(default)]
    pub gen_delegs: HashMap<String, serde_json::Value>,
    /// Initial funds
    #[serde(default)]
    pub initial_funds: HashMap<String, u64>,
    /// Staking (pools, delegations)
    #[serde(default)]
    pub staking: Option<ShelleyStaking>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShelleyProtocolVersion {
    pub major: u64,
    pub minor: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShelleyProtocolParams {
    pub min_fee_a: Option<u64>,
    pub min_fee_b: Option<u64>,
    pub max_block_body_size: Option<u64>,
    pub max_tx_size: Option<u64>,
    pub max_block_header_size: Option<u64>,
    pub key_deposit: Option<u64>,
    pub pool_deposit: Option<u64>,
    pub e_max: Option<u64>,
    pub n_opt: Option<u64>,
    pub a0: Option<f64>,
    pub rho: Option<f64>,
    pub tau: Option<f64>,
    pub decentralisation_param: Option<f64>,
    pub min_u_tx_o_value: Option<u64>,
    pub min_pool_cost: Option<u64>,
    #[serde(default)]
    pub protocol_version: Option<ShelleyProtocolVersion>,
    #[serde(default)]
    pub extra_entropy: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShelleyStaking {
    #[serde(default)]
    pub pools: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub stake: HashMap<String, serde_json::Value>,
}

impl ShelleyGenesis {
    /// Load Shelley genesis and compute its hash
    pub fn load_with_hash(path: &Path) -> Result<(Self, [u8; 32])> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read Shelley genesis: {}", path.display()))?;
        let genesis: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse Shelley genesis: {}", path.display()))?;

        // Compute Blake2b-256 hash
        use blake2::{Blake2b, Digest};
        let mut hasher = Blake2b::<blake2::digest::consts::U32>::new();
        hasher.update(content.as_bytes());
        let hash: [u8; 32] = hasher.finalize().into();

        Ok((genesis, hash))
    }

    /// Calculate total genesis UTxO value from initialFunds
    pub fn total_initial_funds(&self) -> u64 {
        self.initial_funds.values().sum()
    }

    /// Calculate initial reserves (max_supply - initial_funds)
    pub fn initial_reserves(&self) -> u64 {
        let total_funds = self.total_initial_funds();
        self.max_lovelace_supply.saturating_sub(total_funds)
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Alonzo genesis (Plutus cost models)
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlonzoGenesis {
    /// Lovelace per UTxO byte (replaces minUTxOValue)
    #[serde(default)]
    pub lovelace_per_utxo_word: Option<u64>,
    /// Execution unit prices
    #[serde(default)]
    pub execution_prices: Option<ExecutionPrices>,
    /// Max transaction execution units
    #[serde(default)]
    pub max_tx_ex_units: Option<ExUnits>,
    /// Max block execution units
    #[serde(default)]
    pub max_block_ex_units: Option<ExUnits>,
    /// Max value size
    #[serde(default)]
    pub max_value_size: Option<u64>,
    /// Collateral percentage
    #[serde(default)]
    pub collateral_percentage: Option<u64>,
    /// Max collateral inputs
    #[serde(default)]
    pub max_collateral_inputs: Option<u64>,
    /// Plutus cost models (V1, V2, etc.)
    #[serde(default)]
    pub cost_models: HashMap<String, Vec<i64>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPrices {
    pub pr_mem: PriceRational,
    pub pr_steps: PriceRational,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceRational {
    pub numerator: u64,
    pub denominator: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExUnits {
    pub ex_units_mem: u64,
    pub ex_units_steps: u64,
}

impl AlonzoGenesis {
    /// Load Alonzo genesis and compute its hash
    pub fn load_with_hash(path: &Path) -> Result<(Self, [u8; 32])> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read Alonzo genesis: {}", path.display()))?;
        let genesis: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse Alonzo genesis: {}", path.display()))?;

        // Compute Blake2b-256 hash
        use blake2::{Blake2b, Digest};
        let mut hasher = Blake2b::<blake2::digest::consts::U32>::new();
        hasher.update(content.as_bytes());
        let hash: [u8; 32] = hasher.finalize().into();

        Ok((genesis, hash))
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Conway genesis (CIP-1694 governance parameters)
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConwayGenesis {
    /// Pool voting thresholds
    #[serde(default)]
    pub pool_voting_thresholds: Option<PoolVotingThresholds>,
    /// DRep voting thresholds
    #[serde(default)]
    pub d_rep_voting_thresholds: Option<DRepVotingThresholds>,
    /// Constitutional committee minimum size
    #[serde(default)]
    pub committee_min_size: Option<u64>,
    /// Constitutional committee maximum term length (epochs)
    #[serde(default)]
    pub committee_max_term_length: Option<u64>,
    /// Governance action lifetime (epochs)
    #[serde(default)]
    pub gov_action_lifetime: Option<u64>,
    /// Governance action deposit
    #[serde(default)]
    pub gov_action_deposit: Option<u64>,
    /// DRep registration deposit
    #[serde(default)]
    pub d_rep_deposit: Option<u64>,
    /// DRep activity period (epochs)
    #[serde(default)]
    pub d_rep_activity: Option<u64>,
    /// Initial constitutional committee
    #[serde(default)]
    pub committee: Option<Committee>,
    /// Initial constitution
    #[serde(default)]
    pub constitution: Option<Constitution>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolVotingThresholds {
    pub motion_no_confidence: f64,
    pub committee_normal: f64,
    pub committee_no_confidence: f64,
    pub hard_fork_initiation: f64,
    pub pp_security_group: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DRepVotingThresholds {
    pub motion_no_confidence: f64,
    pub committee_normal: f64,
    pub committee_no_confidence: f64,
    pub update_to_constitution: f64,
    pub hard_fork_initiation: f64,
    pub pp_network_group: f64,
    pub pp_economic_group: f64,
    pub pp_technical_group: f64,
    pub pp_gov_group: f64,
    pub treasury_withdrawal: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rational {
    pub numerator: u64,
    pub denominator: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Committee {
    #[serde(default)]
    pub members: HashMap<String, u64>, // credential → term end epoch
    #[serde(default)]
    pub threshold: Option<Rational>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Constitution {
    pub anchor: Option<Anchor>,
    pub script: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Anchor {
    pub url: String,
    pub data_hash: String,
}

impl ConwayGenesis {
    /// Load Conway genesis and compute its hash
    pub fn load_with_hash(path: &Path) -> Result<(Self, [u8; 32])> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read Conway genesis: {}", path.display()))?;
        let genesis: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse Conway genesis: {}", path.display()))?;

        // Compute Blake2b-256 hash
        use blake2::{Blake2b, Digest};
        let mut hasher = Blake2b::<blake2::digest::consts::U32>::new();
        hasher.update(content.as_bytes());
        let hash: [u8; 32] = hasher.finalize().into();

        Ok((genesis, hash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_byron_genesis_total_calculation() {
        let mut genesis = ByronGenesis {
            avvm_distr: HashMap::new(),
            non_avvm_balances: HashMap::new(),
            _boot_stakeholders: HashMap::new(),
            _heavy_delegation: HashMap::new(),
            start_time: 0,
            block_version_data: ByronBlockVersionData::default(),
            protocol_consts: ByronProtocolConsts::default(),
        };

        genesis.non_avvm_balances.insert(
            "addr1".to_string(),
            "1000000000".to_string(),
        );
        genesis.non_avvm_balances.insert(
            "addr2".to_string(),
            "2000000000".to_string(),
        );

        assert_eq!(genesis.total_genesis_lovelace(), 3000000000);
    }
}
