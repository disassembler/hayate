// Primitive types for ledger state
//
// Simple type aliases and newtype wrappers for clarity.
// Uses pallas types where available.

use serde::{Deserialize, Serialize};
use std::fmt;

/// 28-byte hash (pool ID, stake key hash)
pub type Hash28 = [u8; 28];

/// 32-byte hash (transaction hash, block hash, credential hash, etc.)
pub type Hash32 = [u8; 32];

/// Helper trait for Hash28 conversions
pub trait Hash28Ext {
    /// Convert Hash28 to Hash32 by right-padding with zeros
    fn to_hash32_padded(&self) -> Hash32;
    /// Convert to hex string
    fn to_hex(&self) -> String;
}

impl Hash28Ext for Hash28 {
    fn to_hash32_padded(&self) -> Hash32 {
        let mut result = [0u8; 32];
        result[..28].copy_from_slice(self);
        result
    }

    fn to_hex(&self) -> String {
        hex::encode(self)
    }
}

/// Helper trait for Hash32 conversions
pub trait Hash32Ext {
    /// Convert to hex string
    fn to_hex(&self) -> String;
}

impl Hash32Ext for Hash32 {
    fn to_hex(&self) -> String {
        hex::encode(self)
    }
}

/// Lovelace amount (1 ADA = 1,000,000 lovelace)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct Lovelace(pub u64);

impl std::ops::Add for Lovelace {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Lovelace(self.0.saturating_add(other.0))
    }
}

impl std::ops::AddAssign for Lovelace {
    fn add_assign(&mut self, other: Self) {
        self.0 = self.0.saturating_add(other.0);
    }
}

impl std::ops::Sub for Lovelace {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Lovelace(self.0.saturating_sub(other.0))
    }
}

impl std::ops::SubAssign for Lovelace {
    fn sub_assign(&mut self, other: Self) {
        self.0 = self.0.saturating_sub(other.0);
    }
}

impl fmt::Display for Lovelace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} lovelace", self.0)
    }
}

/// Epoch number
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct EpochNo(pub u64);

impl fmt::Display for EpochNo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Slot number
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct SlotNo(pub u64);

impl fmt::Display for SlotNo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Block number
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct BlockNo(pub u64);

impl fmt::Display for BlockNo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Cardano era
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Era {
    Byron,
    Shelley,
    Allegra,
    Mary,
    Alonzo,
    Babbage,
    Conway,
}

impl fmt::Display for Era {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Era::Byron => write!(f, "Byron"),
            Era::Shelley => write!(f, "Shelley"),
            Era::Allegra => write!(f, "Allegra"),
            Era::Mary => write!(f, "Mary"),
            Era::Alonzo => write!(f, "Alonzo"),
            Era::Babbage => write!(f, "Babbage"),
            Era::Conway => write!(f, "Conway"),
        }
    }
}

/// Stake credential (key hash or script hash)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Credential {
    Key(Hash32),
    Script(Hash32),
}

/// Governance action ID (transaction hash + index)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GovActionId {
    pub tx_hash: Hash32,
    pub index: u32,
}

/// DRep (Delegated Representative)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DRep {
    KeyHash(Hash32),
    ScriptHash(Hash32),
    AlwaysAbstain,
    AlwaysNoConfidence,
}

/// Voter type for governance
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Voter {
    ConstitutionalCommittee(Credential),
    DRep(Credential),
    StakePool(Hash28),
}

/// Anchor for metadata
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub url: String,
    pub hash: Hash32,
}

/// Voting procedure
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VotingProcedure {
    pub vote: Vote,
    pub anchor: Option<Anchor>,
}

/// Vote (Yes/No/Abstain)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Vote {
    Yes,
    No,
    Abstain,
}

/// Rational number (for protocol parameters)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rational {
    pub numerator: u64,
    pub denominator: u64,
}

/// Protocol parameters (simplified - we'll expand this as needed)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolParameters {
    pub protocol_version_major: u64,
    pub protocol_version_minor: u64,
    pub min_fee_a: u64,
    pub min_fee_b: u64,
    pub max_block_body_size: u64,
    pub max_transaction_size: u64,
    pub max_block_header_size: u64,
    pub key_deposit: u64,
    pub pool_deposit: u64,
    pub min_pool_cost: u64,
    pub price_mem: Rational,
    pub price_step: Rational,
    pub max_tx_execution_units_mem: u64,
    pub max_tx_execution_units_steps: u64,
    pub max_block_execution_units_mem: u64,
    pub max_block_execution_units_steps: u64,
    pub max_value_size: u64,
    pub collateral_percentage: u64,
    pub max_collateral_inputs: u64,
    // Monetary policy
    pub rho: Rational,  // monetary expansion rate
    pub tau: Rational,  // treasury growth rate
    pub decentralization: Rational,
    pub extra_entropy: Option<Hash32>,
    pub active_slot_coefficient: Rational,  // f (fraction of slots expected to have blocks)
    // Pool parameters
    pub min_pool_cost_lovelace: u64,
    pub n_opt: u64,  // optimal pool count
    pub a0: Rational,  // pool pledge influence
    pub e_max: u64,  // maximum epoch for pool retirement (epochs in future)
    // Governance (Conway)
    pub drep_deposit: u64,
    pub drep_activity_period: u64,
    pub gov_action_lifetime: u64,
    pub gov_action_deposit: u64,
    pub committee_min_size: u64,
    // DRep voting thresholds
    pub dvt_motion_no_confidence: Rational,
    pub dvt_committee_normal: Rational,
    pub dvt_committee_no_confidence: Rational,
    pub dvt_hard_fork: Rational,
    pub dvt_pp_network_group: Rational,
    pub dvt_pp_economic_group: Rational,
    pub dvt_pp_technical_group: Rational,
    pub dvt_pp_gov_group: Rational,
    pub dvt_treasury_withdrawal: Rational,
    pub dvt_constitution: Rational,
    pub dvt_no_confidence: Rational,
    // SPO voting thresholds
    pub pvt_motion_no_confidence: Rational,
    pub pvt_committee_normal: Rational,
    pub pvt_committee_no_confidence: Rational,
    pub pvt_hard_fork: Rational,
    pub pvt_pp_security_group: Rational,
}

impl ProtocolParameters {
    /// Get active slot coefficient as f64
    pub fn active_slot_coeff(&self) -> f64 {
        self.active_slot_coefficient.numerator as f64
            / self.active_slot_coefficient.denominator as f64
    }
}

impl Default for ProtocolParameters {
    fn default() -> Self {
        // Mainnet Conway defaults
        Self {
            protocol_version_major: 10,
            protocol_version_minor: 0,
            min_fee_a: 44,
            min_fee_b: 155381,
            max_block_body_size: 90112,
            max_transaction_size: 16384,
            max_block_header_size: 1100,
            key_deposit: 2_000_000,
            pool_deposit: 500_000_000,
            min_pool_cost: 340_000_000,
            price_mem: Rational { numerator: 577, denominator: 10000 },
            price_step: Rational { numerator: 721, denominator: 10000000 },
            max_tx_execution_units_mem: 14000000,
            max_tx_execution_units_steps: 10000000000,
            max_block_execution_units_mem: 62000000,
            max_block_execution_units_steps: 40000000000,
            max_value_size: 5000,
            collateral_percentage: 150,
            max_collateral_inputs: 3,
            rho: Rational { numerator: 3, denominator: 1000 },  // 0.003
            tau: Rational { numerator: 2, denominator: 10 },    // 0.2
            decentralization: Rational { numerator: 0, denominator: 1 },
            extra_entropy: None,
            active_slot_coefficient: Rational { numerator: 1, denominator: 20 },  // f = 0.05
            min_pool_cost_lovelace: 340_000_000,
            n_opt: 500,
            a0: Rational { numerator: 3, denominator: 10 },  // 0.3
            e_max: 18,  // Pool retirement can be scheduled max 18 epochs in future
            drep_deposit: 500_000_000,
            drep_activity_period: 20,  // epochs
            gov_action_lifetime: 6,    // epochs
            gov_action_deposit: 100_000_000_000,  // 100k ADA
            committee_min_size: 7,
            // DRep voting thresholds (mainnet Conway defaults)
            dvt_motion_no_confidence: Rational { numerator: 51, denominator: 100 },  // 51%
            dvt_committee_normal: Rational { numerator: 51, denominator: 100 },  // 51%
            dvt_committee_no_confidence: Rational { numerator: 60, denominator: 100 },  // 60%
            dvt_hard_fork: Rational { numerator: 60, denominator: 100 },  // 60%
            dvt_pp_network_group: Rational { numerator: 51, denominator: 100 },  // 51%
            dvt_pp_economic_group: Rational { numerator: 51, denominator: 100 },  // 51%
            dvt_pp_technical_group: Rational { numerator: 51, denominator: 100 },  // 51%
            dvt_pp_gov_group: Rational { numerator: 75, denominator: 100 },  // 75%
            dvt_treasury_withdrawal: Rational { numerator: 51, denominator: 100 },  // 51%
            dvt_constitution: Rational { numerator: 75, denominator: 100 },  // 75%
            dvt_no_confidence: Rational { numerator: 51, denominator: 100 },  // 51%
            // SPO voting thresholds (mainnet Conway defaults)
            pvt_motion_no_confidence: Rational { numerator: 51, denominator: 100 },  // 51%
            pvt_committee_normal: Rational { numerator: 51, denominator: 100 },  // 51%
            pvt_committee_no_confidence: Rational { numerator: 51, denominator: 100 },  // 51%
            pvt_hard_fork: Rational { numerator: 60, denominator: 100 },  // 60%
            pvt_pp_security_group: Rational { numerator: 51, denominator: 100 },  // 51%
        }
    }
}

/// Relay endpoint for pool
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Relay {
    SingleHostAddr {
        port: Option<u16>,
        ipv4: Option<std::net::Ipv4Addr>,
        ipv6: Option<std::net::Ipv6Addr>,
    },
    SingleHostName {
        port: Option<u16>,
        dns_name: String,
    },
    MultiHostName {
        dns_name: String,
    },
}

/// Proposal procedure for governance
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalProcedure {
    pub deposit: Lovelace,
    pub return_addr: Credential,
    pub gov_action: GovernanceAction,
    pub anchor: Option<Anchor>,
}

/// Governance action types (CIP-1694)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GovernanceAction {
    ParameterChange {
        prev_action_id: Option<GovActionId>,
        update: ProtocolParamUpdate,
        guardrails_hash: Option<Hash32>,
    },
    HardForkInitiation {
        prev_action_id: Option<GovActionId>,
        protocol_version: (u64, u64),
    },
    TreasuryWithdrawals {
        withdrawals: Vec<(Credential, Lovelace)>,
        guardrails_hash: Option<Hash32>,
    },
    NoConfidence {
        prev_action_id: Option<GovActionId>,
    },
    UpdateCommittee {
        prev_action_id: Option<GovActionId>,
        members_to_remove: Vec<Credential>,
        members_to_add: Vec<(Credential, EpochNo)>,
        quorum: Rational,
    },
    NewConstitution {
        prev_action_id: Option<GovActionId>,
        constitution: Constitution,
    },
    InfoAction,
}

/// Protocol parameter update (pre-Conway and Conway governance)
///
/// Fields correspond to the CDDL protocol_param_update map (Shelley through Babbage).
/// All fields are optional; only set fields are applied when an update is enacted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProtocolParamUpdate {
    // Network group
    pub min_fee_a: Option<u64>,
    pub min_fee_b: Option<u64>,
    pub max_block_body_size: Option<u64>,
    pub max_transaction_size: Option<u64>,
    pub max_block_header_size: Option<u64>,
    pub protocol_version: Option<(u64, u64)>,
    // Economic group
    pub key_deposit: Option<u64>,
    pub pool_deposit: Option<u64>,
    pub min_pool_cost: Option<u64>,
    pub rho: Option<Rational>,      // monetary expansion rate
    pub tau: Option<Rational>,      // treasury growth rate
    pub a0: Option<Rational>,       // pool pledge influence
    // Technical group
    pub n_opt: Option<u64>,         // optimal pool count (k)
    pub e_max: Option<u64>,         // max pool retirement epoch
    pub decentralization: Option<Rational>, // d parameter
}

/// Constitution
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constitution {
    pub anchor: Option<Anchor>,
    pub script_hash: Option<Hash32>,
}

/// Pool registration parameters
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolParams {
    pub operator: Hash28,  // pool ID
    pub vrf_keyhash: Hash32,
    pub pledge: Lovelace,
    pub cost: Lovelace,
    pub margin: Rational,
    pub reward_account: Vec<u8>,  // 29-byte reward address
    pub pool_owners: Vec<Hash28>,
    pub relays: Vec<Relay>,
    pub pool_metadata: Option<PoolMetadata>,
}

/// Pool metadata
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolMetadata {
    pub url: String,
    pub hash: Hash32,
}

/// MIR (Move Instantaneous Rewards) source
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MIRSource {
    Reserves,
    Treasury,
}

/// MIR (Move Instantaneous Rewards) target
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MIRTarget {
    StakeCredentials(Vec<(Credential, Lovelace)>),
    OtherPot(Lovelace),
}

/// Certificate (ledger state transitions)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Certificate {
    // Pre-Conway stake certificates
    StakeRegistration(Credential),
    StakeDeregistration(Credential),
    StakeDelegation {
        credential: Credential,
        pool_hash: Hash28,
    },

    // Pool certificates
    PoolRegistration(PoolParams),
    PoolRetirement {
        pool_hash: Hash28,
        epoch: u64,
    },

    // Genesis delegation (Byron/Shelley era)
    GenesisKeyDelegation {
        genesis_hash: Hash32,
        genesis_delegate_hash: Hash32,
        vrf_keyhash: Hash32,
    },

    // MIR certificates (Shelley era treasury/reserves moves)
    MoveInstantaneousRewardsCert {
        source: MIRSource,
        target: MIRTarget,
    },

    // Conway stake certificates (tag 7, 8)
    ConwayStakeRegistration {
        credential: Credential,
        deposit: Lovelace,
    },
    ConwayStakeDeregistration {
        credential: Credential,
        refund: Lovelace,
    },

    // Conway combined registration + delegation
    RegStakeDeleg {
        credential: Credential,
        pool_hash: Hash28,
        deposit: Lovelace,
    },

    // Conway DRep certificates
    RegDRep {
        credential: Credential,
        deposit: Lovelace,
        anchor: Option<Anchor>,
    },
    UnregDRep {
        credential: Credential,
        refund: Lovelace,
    },
    UpdateDRep {
        credential: Credential,
        anchor: Option<Anchor>,
    },

    // Conway vote delegation
    VoteDelegation {
        credential: Credential,
        drep: DRep,
    },

    // Conway stake + vote delegation
    StakeVoteDelegation {
        credential: Credential,
        pool_hash: Hash28,
        drep: DRep,
    },

    // Conway registration + stake + vote delegation
    RegStakeVoteDeleg {
        credential: Credential,
        pool_hash: Hash28,
        drep: DRep,
        deposit: Lovelace,
    },

    // Conway registration + vote delegation
    VoteRegDeleg {
        credential: Credential,
        drep: DRep,
        deposit: Lovelace,
    },

    // Conway Constitutional Committee certificates
    CommitteeHotAuth {
        cold_credential: Credential,
        hot_credential: Credential,
    },
    CommitteeColdResign {
        cold_credential: Credential,
        anchor: Option<Anchor>,
    },
}
