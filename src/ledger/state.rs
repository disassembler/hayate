// Ledger state structures
//
// Copied from torsten-ledger/src/state/mod.rs with adaptations for hayate:
// - Removed `utxo_set` field (hayate tracks UTxOs in LSM trees)
// - Using hayate's primitive types instead of torsten-primitives
// - Kept all governance, deposit tracking, nonce state, etc.

use super::primitives::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// Total ADA supply (45 billion ADA = 45 * 10^15 lovelace)
pub const MAX_LOVELACE_SUPPLY: u64 = 45_000_000_000_000_000;

/// Convert a Credential to Hash32.
/// For Key credentials, the hash is already 32 bytes.
/// For Script credentials, the hash is also 32 bytes.
pub(crate) fn credential_to_hash(cred: &Credential) -> Hash32 {
    match cred {
        Credential::Key(hash) => *hash,
        Credential::Script(hash) => *hash,
    }
}

/// Controls whether block application includes validation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockValidationMode {
    /// Full Phase-1 + Phase-2 Plutus evaluation
    ValidateAll,
    /// Trust the block producer's validation (for replay/import)
    ApplyOnly,
}

/// The complete ledger state.
///
/// Large collections are wrapped in `Arc` for copy-on-write semantics.
/// Cloning a `LedgerState` is cheap (just reference count bumps).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerState {
    // NOTE: utxo_set removed - hayate tracks this in LSM trees
    // We'll add a method to query the LSM tree instead

    /// Current epoch
    pub epoch: EpochNo,
    /// Shelley epoch length in slots
    pub epoch_length: u64,
    /// Number of Byron epochs before Shelley hard fork
    #[serde(default)]
    pub shelley_transition_epoch: u64,
    /// Byron epoch length in slots (10 * k)
    #[serde(default)]
    pub byron_epoch_length: u64,

    /// Current protocol parameters
    pub protocol_params: ProtocolParameters,

    /// Stake distribution
    pub stake_distribution: StakeDistributionState,

    /// Treasury balance
    pub treasury: Lovelace,
    /// Reserves balance (ADA not yet in circulation)
    pub reserves: Lovelace,

    /// Delegation state: credential_hash -> pool_id (Arc for copy-on-write)
    pub delegations: Arc<HashMap<Hash32, Hash28>>,

    /// Pool registrations: pool_id -> pool registration (Arc for copy-on-write)
    pub pool_params: Arc<HashMap<Hash28, PoolRegistration>>,

    /// Future pool params from re-registrations during the current epoch.
    /// In Haskell's POOL STS rule, re-registrations go into psFutureStakePoolParams
    /// and are only merged into psStakePoolParams AFTER the mark snapshot is taken
    /// at the epoch boundary (SNAP runs before POOL in the EPOCH STS rule).
    /// New registrations go directly to pool_params; only re-registrations are queued here.
    #[serde(default)]
    pub future_pool_params: Arc<HashMap<Hash28, PoolRegistration>>,

    /// Pool retirements pending at a given epoch
    pub pending_retirements: BTreeMap<EpochNo, Vec<Hash28>>,

    /// Stake snapshots for the Cardano "mark/set/go" snapshot model
    pub snapshots: EpochSnapshots,

    /// Reward accounts: stake credential hash -> accumulated rewards (Arc for copy-on-write)
    pub reward_accounts: Arc<HashMap<Hash32, Lovelace>>,

    /// Fees collected in the current epoch
    pub epoch_fees: Lovelace,

    /// Number of blocks produced by each pool in the current epoch (Arc for copy-on-write)
    pub epoch_blocks_by_pool: Arc<HashMap<Hash28, u64>>,

    /// Total blocks in the current epoch
    pub epoch_block_count: u64,

    // ===== Nonce state machine (TICKN rule) =====

    /// Evolving nonce (eta_v): accumulated hash of ALL VRF outputs (never reset)
    pub evolving_nonce: Hash32,

    /// Candidate nonce: snapshot of evolving_nonce that freezes in the last
    /// randomness_stabilisation_window (4k/f) slots of each epoch
    pub candidate_nonce: Hash32,

    /// Current epoch nonce: hash(candidate_nonce || last_epoch_block_nonce) at epoch boundary
    pub epoch_nonce: Hash32,

    /// LAB nonce: prev_hash of the most recent block (type-cast, no hashing)
    pub lab_nonce: Hash32,

    /// Snapshot of lab_nonce at epoch boundary
    pub last_epoch_block_nonce: Hash32,

    /// Randomness stabilisation window: ceiling(4k/f) for Conway+
    pub randomness_stabilisation_window: u64,

    /// Stability window: ceiling(3k/f) for Alonzo/Babbage
    #[serde(default)]
    pub stability_window_3kf: u64,

    /// Shelley genesis hash (used for initial nonce state)
    pub genesis_hash: Hash32,

    // ===== Protocol parameter updates (pre-Conway) =====

    /// Pending protocol parameter update proposals (pre-Conway):
    /// Maps target_epoch -> [(genesis_delegate_hash, proposed_update)]
    pub pending_pp_updates: BTreeMap<EpochNo, Vec<(Hash32, ProtocolParamUpdate)>>,

    /// Quorum for pre-Conway protocol parameter updates (from Shelley genesis)
    #[serde(default = "default_update_quorum")]
    pub update_quorum: u64,

    // ===== Conway governance (CIP-1694) =====

    /// Conway governance state (Arc for copy-on-write)
    pub governance: Arc<GovernanceState>,

    // ===== Deposit tracking =====

    /// Deposit tracker for all deposit types (pool, stake, governance, DRep)
    /// CRITICAL: Governance deposits are voting stake but NOT staking stake
    #[serde(default)]
    pub deposit_tracker: DepositTracker,

    // ===== Rewards =====

    /// Pending reward update computed at one epoch boundary and applied at the
    /// next, matching Haskell's RUPD (Reward UPDate) / pulsing reward scheme.
    ///
    /// At boundary E -> E+1:
    ///   1. Apply `pending_reward_update` (computed at E-1 -> E boundary)
    ///   2. Rotate snapshots, build new mark snapshot
    ///   3. Compute new rewards using go snapshot -> store in `pending_reward_update`
    #[serde(default)]
    pub pending_reward_update: Option<PendingRewardUpdate>,

    /// Last applied RUPD (for debugging/comparison with Haskell)
    #[serde(default)]
    pub last_applied_rupd: Option<PendingRewardUpdate>,

    // ===== Script credentials =====

    /// Script-type stake credentials (for N2C queries)
    #[serde(default)]
    pub script_stake_credentials: HashSet<Hash32>,
}

fn default_update_quorum() -> u64 {
    5 // Mainnet default: 5 out of 7 genesis delegates
}

impl LedgerState {
    pub fn new(params: ProtocolParameters) -> Self {
        LedgerState {
            epoch: EpochNo(0),
            epoch_length: 432000,          // mainnet default
            shelley_transition_epoch: 208, // mainnet default
            byron_epoch_length: 21600,     // mainnet default (10 * 2160)
            protocol_params: params,
            stake_distribution: StakeDistributionState::default(),
            treasury: Lovelace(0),
            reserves: Lovelace(MAX_LOVELACE_SUPPLY),
            delegations: Arc::new(HashMap::new()),
            pool_params: Arc::new(HashMap::new()),
            future_pool_params: Arc::new(HashMap::new()),
            pending_retirements: BTreeMap::new(),
            snapshots: EpochSnapshots::default(),
            reward_accounts: Arc::new(HashMap::new()),
            epoch_fees: Lovelace(0),
            epoch_blocks_by_pool: Arc::new(HashMap::new()),
            epoch_block_count: 0,
            evolving_nonce: [0u8; 32],
            candidate_nonce: [0u8; 32],
            epoch_nonce: [0u8; 32],
            lab_nonce: [0u8; 32],
            last_epoch_block_nonce: [0u8; 32],
            randomness_stabilisation_window: 172800, // 4k/f on mainnet
            stability_window_3kf: 129600,            // 3k/f on mainnet
            genesis_hash: [0u8; 32],
            pending_pp_updates: BTreeMap::new(),
            update_quorum: default_update_quorum(),
            governance: Arc::new(GovernanceState::default()),
            deposit_tracker: DepositTracker::default(),
            pending_reward_update: None,
            last_applied_rupd: None,
            script_stake_credentials: HashSet::new(),
        }
    }

    /// Seed the ledger with genesis UTxOs and adjust reserves.
    ///
    /// This must be called after creating the ledger state to:
    /// 1. Subtract genesis UTxO value from reserves per Shelley spec
    /// 2. Optionally populate the UTxO tree (not implemented in hayate, handled externally)
    ///
    /// Without this, monetary expansion (rho * reserves) is computed on too large a reserves value,
    /// draining reserves too fast and overfilling the treasury.
    ///
    /// The genesis UTxOs don't need to be added to hayate's UTxO tree since they will appear
    /// as outputs in genesis/Byron blocks when syncing from genesis.
    pub fn seed_genesis_utxos(&mut self, total_genesis_lovelace: u64) {
        // Deduct seeded lovelace from reserves per Shelley spec:
        // reserves = maxLovelaceSupply - totalBalance(initialUTxO) - treasury
        self.reserves.0 = self.reserves.0.saturating_sub(total_genesis_lovelace);

        tracing::info!(
            genesis_utxo_total = total_genesis_lovelace,
            reserves_after = self.reserves.0,
            "Seeded genesis UTxOs, adjusted reserves"
        );
    }

    /// Set the genesis hash (for epoch nonce initialization)
    pub fn set_genesis_hash(&mut self, hash: [u8; 32]) {
        self.genesis_hash = hash;
        tracing::debug!(hash = hex::encode(hash), "Set genesis hash");
    }

    /// Set epoch length from Shelley genesis
    pub fn set_epoch_length(&mut self, length: u64) {
        self.epoch_length = length;
    }

    /// Set Shelley transition epoch
    pub fn set_shelley_transition(&mut self, epoch: u64, byron_epoch_length: u64) {
        self.shelley_transition_epoch = epoch;
        self.byron_epoch_length = byron_epoch_length;
    }

    /// Set update quorum from Shelley genesis
    pub fn set_update_quorum(&mut self, quorum: u64) {
        self.update_quorum = quorum;
    }

    /// Update protocol parameters from Shelley genesis
    pub fn update_protocol_params_from_genesis(
        &mut self,
        rho: Option<f64>,
        tau: Option<f64>,
        decentralization: Option<f64>,
        a0: Option<f64>,
        n_opt: Option<u64>,
        min_fee_a: Option<u64>,
        min_fee_b: Option<u64>,
        pool_deposit: Option<u64>,
        key_deposit: Option<u64>,
        min_pool_cost: Option<u64>,
        active_slot_coeff: Option<f64>,
        protocol_version: Option<(u64, u64)>,
    ) {
        // Helper to convert f64 to exact rational by parsing decimal string
        // This avoids floating point precision loss
        fn f64_to_rational(f: f64) -> Rational {
            // Format as string with sufficient precision
            let s = format!("{:.18}", f);

            // Parse decimal: find decimal point position
            if let Some(dot_pos) = s.find('.') {
                let int_part = &s[..dot_pos];
                let frac_part = &s[dot_pos + 1..].trim_end_matches('0');

                if frac_part.is_empty() {
                    // Integer value
                    let n: u64 = int_part.parse().unwrap_or(0);
                    return Rational { numerator: n, denominator: 1 };
                }

                // Compute numerator and denominator
                let decimals = frac_part.len();
                let denominator = 10u64.pow(decimals as u32);
                let int_val: u64 = int_part.parse().unwrap_or(0);
                let frac_val: u64 = frac_part.parse().unwrap_or(0);
                let numerator = int_val * denominator + frac_val;

                // Simplify by finding GCD
                fn gcd(mut a: u64, mut b: u64) -> u64 {
                    while b != 0 {
                        let t = b;
                        b = a % b;
                        a = t;
                    }
                    a
                }

                let g = gcd(numerator, denominator);
                Rational {
                    numerator: numerator / g,
                    denominator: denominator / g,
                }
            } else {
                // Integer value
                let n: u64 = s.parse().unwrap_or(0);
                Rational { numerator: n, denominator: 1 }
            }
        }

        // Update monetary policy parameters
        if let Some(r) = rho {
            let rat = f64_to_rational(r);
            tracing::debug!(
                "rho: {} = {}/{}",
                r,
                rat.numerator,
                rat.denominator
            );
            self.protocol_params.rho = rat;
        }

        if let Some(t) = tau {
            let rat = f64_to_rational(t);
            tracing::debug!(
                "tau: {} = {}/{}",
                t,
                rat.numerator,
                rat.denominator
            );
            self.protocol_params.tau = rat;
        }

        if let Some(d) = decentralization {
            let rat = f64_to_rational(d);
            tracing::debug!(
                "d: {} = {}/{}",
                d,
                rat.numerator,
                rat.denominator
            );
            self.protocol_params.decentralization = rat;
        }

        if let Some(a) = a0 {
            let rat = f64_to_rational(a);
            tracing::debug!(
                "a0: {} = {}/{}",
                a,
                rat.numerator,
                rat.denominator
            );
            self.protocol_params.a0 = rat;
        }

        if let Some(n) = n_opt {
            self.protocol_params.n_opt = n;
        }

        if let Some(a) = min_fee_a {
            self.protocol_params.min_fee_a = a;
        }

        if let Some(b) = min_fee_b {
            self.protocol_params.min_fee_b = b;
        }

        if let Some(d) = pool_deposit {
            self.protocol_params.pool_deposit = d;
        }

        if let Some(d) = key_deposit {
            self.protocol_params.key_deposit = d;
        }

        if let Some(c) = min_pool_cost {
            self.protocol_params.min_pool_cost = c;
            self.protocol_params.min_pool_cost_lovelace = c;
        }

        if let Some(f) = active_slot_coeff {
            self.protocol_params.active_slot_coefficient = f64_to_rational(f);
        }

        if let Some((major, minor)) = protocol_version {
            self.protocol_params.protocol_version_major = major;
            self.protocol_params.protocol_version_minor = minor;
        }
    }
}

/// Pending reward update matching Haskell's RUPD structure.
///
/// Computed at one epoch boundary and applied at the next. Contains:
/// - Per-account rewards to credit
/// - Treasury increase (tau cut + undistributed)
/// - Reserves decrease (monetary expansion)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PendingRewardUpdate {
    /// Rewards to add to each registered stake credential's reward account
    pub rewards: HashMap<Hash32, Lovelace>,
    /// Total treasury increase (tau cut)
    pub delta_treasury: u64,
    /// Total reserves decrease (monetary expansion)
    pub delta_reserves: u64,
    /// Undistributed rewards (returned to reserves)
    pub undistributed: u64,

    // === Intermediate values for debugging (match Haskell PulsingReward.hs) ===
    /// Effectiveness parameter: eta = blocks_produced / expected_blocks (capped at 1)
    #[serde(default)]
    pub eta: f64,
    /// Monetary expansion: deltaR1 = floor(min(eta, 1) * rho * reserves)
    #[serde(default)]
    pub delta_r1: u64,
    /// Reward pot before treasury cut: rPot = deltaR1 + fees
    #[serde(default)]
    pub r_pot: u64,
    /// Treasury cut: deltaT1 = floor(tau * rPot)
    #[serde(default)]
    pub delta_t1: u64,
    /// Reward pot after treasury cut: _R = rPot - deltaT1
    #[serde(default)]
    pub reward_pot_after_treasury: u64,
    /// Total distributed rewards
    #[serde(default)]
    pub total_distributed: u64,
}

/// Conway-era governance state (CIP-1694)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GovernanceState {
    /// Registered DReps: credential -> DRepState
    pub dreps: HashMap<Hash32, DRepRegistration>,

    /// Vote delegations: stake credential hash -> DRep
    pub vote_delegations: HashMap<Hash32, DRep>,

    /// Constitutional committee: cold credential -> hot credential
    pub committee_hot_keys: HashMap<Hash32, Hash32>,

    /// Committee member expiration epochs (cold credential -> expiration epoch)
    pub committee_expiration: HashMap<Hash32, EpochNo>,

    /// Resigned committee members
    pub committee_resigned: HashMap<Hash32, Option<Anchor>>,

    /// Script-type cold committee credentials (for N2C queries)
    #[serde(default)]
    pub script_committee_credentials: HashSet<Hash32>,

    /// Active governance proposals indexed by GovActionId
    pub proposals: BTreeMap<GovActionId, ProposalState>,

    /// Votes cast, indexed by action ID for efficient ratification lookup
    pub votes_by_action: BTreeMap<GovActionId, Vec<(Voter, VotingProcedure)>>,

    /// Total DRep registrations count (including deregistered)
    pub drep_registration_count: u64,

    /// Total proposals submitted
    pub proposal_count: u64,

    /// Current constitution (set by NewConstitution governance action)
    pub constitution: Option<Constitution>,

    /// Whether the committee is in a no-confidence state
    #[serde(default)]
    pub no_confidence: bool,

    /// Committee quorum threshold
    #[serde(default)]
    pub committee_threshold: Option<Rational>,

    /// Last enacted governance action IDs per purpose (for prev_action_id chain validation)
    #[serde(default)]
    pub enacted_pparam_update: Option<GovActionId>,
    #[serde(default)]
    pub enacted_hard_fork: Option<GovActionId>,
    #[serde(default)]
    pub enacted_committee: Option<GovActionId>,
    #[serde(default)]
    pub enacted_constitution: Option<GovActionId>,

    /// Last ratification results (from most recent epoch transition)
    #[serde(default)]
    pub last_ratified: Vec<(GovActionId, ProposalState)>,
    #[serde(default)]
    pub last_expired: Vec<GovActionId>,
    #[serde(default)]
    pub last_ratify_delayed: bool,
}

/// Registration state for a DRep
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DRepRegistration {
    pub credential: Credential,
    pub deposit: Lovelace,
    pub anchor: Option<Anchor>,
    pub registered_epoch: EpochNo,
    /// Last epoch in which this DRep voted or updated (for activity tracking)
    pub last_active_epoch: EpochNo,
    /// Whether this DRep is currently active (per CIP-1694 activity tracking)
    #[serde(default = "default_drep_active")]
    pub active: bool,
}

fn default_drep_active() -> bool {
    true
}

/// State of a governance proposal
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalState {
    pub procedure: ProposalProcedure,
    pub proposed_epoch: EpochNo,
    pub expires_epoch: EpochNo,
    pub yes_votes: u64,
    pub no_votes: u64,
    pub abstain_votes: u64,
}

/// Stake distribution state
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StakeDistributionState {
    pub stake_map: HashMap<Hash32, Lovelace>,
}

/// Cardano uses a "mark / set / go" snapshot model:
/// - "mark" is the snapshot taken at the current epoch boundary
/// - "set" is the snapshot from the previous epoch (used for leader election)
/// - "go" is the snapshot from two epochs ago (used for reward calculation)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EpochSnapshots {
    /// Snapshot from the most recent epoch boundary ("mark")
    pub mark: Option<StakeSnapshot>,
    /// Snapshot from one epoch ago ("set") — used for leader election
    pub set: Option<StakeSnapshot>,
    /// Snapshot from two epochs ago ("go") — used for reward distribution
    pub go: Option<StakeSnapshot>,
    /// Fees from the current epoch (Haskell's ssFee)
    /// This is updated by SNAP at each epoch boundary to the fees from the epoch that just ended.
    /// RUPD uses this value at the next epoch transition.
    /// This means fees from epoch N are used for rewards at epoch N+1→N+2.
    #[serde(default)]
    pub current_epoch_fees: Lovelace,
}

/// A snapshot of the stake distribution at an epoch boundary.
/// Uses `Arc` for large HashMaps to avoid deep-cloning during epoch rotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StakeSnapshot {
    pub epoch: EpochNo,
    /// stake credential hash -> pool_id delegation
    pub delegations: Arc<HashMap<Hash32, Hash28>>,
    /// pool_id -> total active stake delegated to that pool
    pub pool_stake: HashMap<Hash28, Lovelace>,
    /// pool_id -> pool parameters at snapshot time
    pub pool_params: Arc<HashMap<Hash28, PoolRegistration>>,
    /// Individual stake per credential (for reward distribution and pledge verification)
    #[serde(default)]
    pub stake_distribution: Arc<HashMap<Hash32, Lovelace>>,
    /// Blocks produced by each pool during this epoch (for reward calculation)
    /// CRITICAL: Must be stored in snapshot so rewards use blocks from the correct epoch
    #[serde(default)]
    pub epoch_blocks_by_pool: Arc<HashMap<Hash28, u64>>,
}

/// Pool registration information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolRegistration {
    pub pool_id: Hash28,
    pub vrf_keyhash: Hash32,
    pub pledge: Lovelace,
    pub cost: Lovelace,
    pub margin_numerator: u64,
    pub margin_denominator: u64,
    /// Reward account for pool operator rewards
    #[serde(default)]
    pub reward_account: Vec<u8>,
    /// Pool owner stake key hashes
    #[serde(default)]
    pub owners: Vec<Hash28>,
    /// Relay endpoints declared by the pool operator
    #[serde(default)]
    pub relays: Vec<Relay>,
    /// Pool metadata URL
    #[serde(default)]
    pub metadata_url: Option<String>,
    /// Pool metadata hash
    #[serde(default)]
    pub metadata_hash: Option<Hash32>,
}

/// Deposit tracking for all deposit types
///
/// CRITICAL: Governance and DRep deposits are voting stake but NOT staking stake.
/// This distinction is essential for correct SPDD calculations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DepositTracker {
    /// Map: credential -> deposits by type
    pub deposits: HashMap<Hash32, DepositsByType>,
}

/// Deposits organized by type
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DepositsByType {
    /// Pool registration deposit (500 ADA)
    /// Counts as both voting and staking stake
    pub pool: Option<Lovelace>,

    /// Stake key registration deposit (2 ADA)
    /// Counts as both voting and staking stake
    pub stake: Option<Lovelace>,

    /// Governance proposal deposits (variable amount)
    /// Counts as voting stake ONLY, NOT staking stake
    pub governance: Vec<(GovActionId, Lovelace)>,

    /// DRep registration deposit (500 ADA)
    /// Counts as voting stake ONLY, NOT staking stake
    pub drep: Option<Lovelace>,
}

impl DepositsByType {
    /// Total deposits (all types)
    pub fn total(&self) -> Lovelace {
        let mut sum = Lovelace(0);
        if let Some(pool) = self.pool {
            sum += pool;
        }
        if let Some(stake) = self.stake {
            sum += stake;
        }
        for (_, amount) in &self.governance {
            sum += *amount;
        }
        if let Some(drep) = self.drep {
            sum += drep;
        }
        sum
    }

    /// Voting stake (all deposits - used for DRep voting power)
    pub fn voting_stake(&self) -> Lovelace {
        self.total()
    }

    /// Staking stake (pool + stake deposits only, excludes governance + DRep)
    /// Used for block production / pool stake calculations
    pub fn staking_stake(&self) -> Lovelace {
        let mut sum = Lovelace(0);
        if let Some(pool) = self.pool {
            sum += pool;
        }
        if let Some(stake) = self.stake {
            sum += stake;
        }
        sum
    }
}

impl DepositTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a deposit
    pub fn add_deposit(&mut self, cred: Hash32, deposit_type: DepositType, amount: Lovelace) {
        let entry = self.deposits.entry(cred).or_default();
        match deposit_type {
            DepositType::Pool => entry.pool = Some(amount),
            DepositType::Stake => entry.stake = Some(amount),
            DepositType::Governance(action_id) => entry.governance.push((action_id, amount)),
            DepositType::DRep => entry.drep = Some(amount),
        }
    }

    /// Refund a deposit (returns the amount refunded)
    pub fn refund_deposit(&mut self, cred: &Hash32, deposit_type: DepositType) -> Option<Lovelace> {
        let entry = self.deposits.get_mut(cred)?;
        match deposit_type {
            DepositType::Pool => entry.pool.take(),
            DepositType::Stake => entry.stake.take(),
            DepositType::Governance(action_id) => {
                let pos = entry.governance.iter().position(|(id, _)| *id == action_id)?;
                Some(entry.governance.remove(pos).1)
            }
            DepositType::DRep => entry.drep.take(),
        }
    }

    /// Get total deposits for a credential
    pub fn get_total_deposits(&self, cred: &Hash32) -> Lovelace {
        self.deposits.get(cred).map(|d| d.total()).unwrap_or(Lovelace(0))
    }

    /// Get voting stake for a credential (includes all deposits)
    pub fn get_voting_stake(&self, cred: &Hash32) -> Lovelace {
        self.deposits.get(cred).map(|d| d.voting_stake()).unwrap_or(Lovelace(0))
    }

    /// Get staking stake for a credential (excludes governance + DRep deposits)
    pub fn get_staking_stake(&self, cred: &Hash32) -> Lovelace {
        self.deposits.get(cred).map(|d| d.staking_stake()).unwrap_or(Lovelace(0))
    }
}

/// Deposit type enum for tracking different deposit kinds
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepositType {
    Pool,
    Stake,
    Governance(GovActionId),
    DRep,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deposits_voting_vs_staking() {
        let mut tracker = DepositTracker::new();
        let cred = [1u8; 32];
        let gov_action = GovActionId {
            tx_hash: [2u8; 32],
            index: 0,
        };

        // Add pool deposit (counts for both)
        tracker.add_deposit(cred, DepositType::Pool, Lovelace(500_000_000));

        // Add stake deposit (counts for both)
        tracker.add_deposit(cred, DepositType::Stake, Lovelace(2_000_000));

        // Add governance deposit (voting only)
        tracker.add_deposit(cred, DepositType::Governance(gov_action), Lovelace(100_000_000_000));

        // Add DRep deposit (voting only)
        tracker.add_deposit(cred, DepositType::DRep, Lovelace(500_000_000));

        let voting = tracker.get_voting_stake(&cred);
        let staking = tracker.get_staking_stake(&cred);

        // Voting stake includes everything
        assert_eq!(voting.0, 500_000_000 + 2_000_000 + 100_000_000_000 + 500_000_000);

        // Staking stake excludes governance and DRep
        assert_eq!(staking.0, 500_000_000 + 2_000_000);

        // This is the critical property: voting > staking when governance deposits exist
        assert!(voting.0 > staking.0);
    }
}
