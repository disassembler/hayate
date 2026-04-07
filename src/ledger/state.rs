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
    /// Number of Byron epochs before Shelley hard fork.
    /// `u64::MAX` is the sentinel meaning "Byron ongoing, HF proposal not yet seen".
    #[serde(default)]
    pub shelley_transition_epoch: u64,
    /// Byron epoch length in slots (10 * k)
    #[serde(default)]
    pub byron_epoch_length: u64,
    /// True when this chain has a Byron era (mainnet, preprod).
    /// Old snapshots lacking this field deserialise to `false`, preserving
    /// existing sanchonet / preview behaviour unchanged.
    #[serde(default)]
    pub has_byron: bool,

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

    /// Pool retirements: pool_id → retirement epoch.
    /// Matches Haskell's `psRetiring :: Map (KeyHash 'StakePool) EpochNo`.
    /// Re-registration (RegPool for an existing pool) cancels a pending retirement
    /// by removing the pool from this map.
    pub pending_retirements: HashMap<Hash28, EpochNo>,

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

    /// RUPD stability window: ceiling(2k/f) — the slot offset within an epoch
    /// at which Haskell's `createRUpd` / reward pulsing starts.
    /// Hayate computes the RUPD at the first block whose `slot_in_epoch >= stability_window_2kf`.
    #[serde(default = "default_stability_window_2kf")]
    pub stability_window_2kf: u64,

    /// Shelley genesis hash (used for initial nonce state)
    pub genesis_hash: Hash32,

    // ===== Protocol parameter updates (pre-Conway) =====

    /// Pending protocol parameter update proposals (pre-Conway):
    /// Maps target_epoch -> { genesis_delegate_hash -> proposed_update }.
    /// Using a BTreeMap keyed by genesis hash ensures that if the same delegate
    /// submits multiple proposals for the same epoch, only the last one counts
    /// (matching Haskell's Map-based semantics).
    pub pending_pp_updates: BTreeMap<EpochNo, BTreeMap<Hash32, ProtocolParamUpdate>>,

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

    /// PPUP enacted message to emit after epoch summary lines (set by epoch.rs, consumed by main.rs)
    #[serde(skip)]
    pub ppup_enacted_log: Option<String>,

    // ===== Script credentials =====

    /// Script-type stake credentials (for N2C queries)
    #[serde(default)]
    pub script_stake_credentials: HashSet<Hash32>,

    /// Protocol parameters from before the last ParameterChange governance action was enacted.
    /// Used for nextEnactState.prevPParams in Conway epoch dumps.
    #[serde(default)]
    pub prev_protocol_params: Option<ProtocolParameters>,

    /// Epoch when Conway genesis was applied (the first Conway-era epoch number).
    /// Used to determine when to propagate conway_cur_params → protocol_params.
    #[serde(default)]
    pub conway_genesis_epoch: Option<u64>,

    /// Legacy field: previously used to defer governance enactments by one epoch.
    ///
    /// Now RATIFY and ENACT happen in the same epoch transition (matching Haskell).
    /// This field is retained only for backward compatibility with existing snapshots;
    /// any stale entries are discarded at the first epoch transition after restore.
    #[serde(default)]
    pub pending_enactments: Vec<PendingEnactment>,

    /// Pointer address resolution map: (slot, tx_idx, cert_idx) → 28-byte credential hash.
    ///
    /// Populated by stake registration certificates. Used to resolve CIP-19 pointer
    /// addresses (types 4-5) which encode a stake reference as a chain pointer to the
    /// registration certificate rather than embedding the credential hash directly.
    ///
    /// Matches Haskell's `UMap.umPtrs :: Map Ptr (Credential 'Staking)`.
    #[serde(default)]
    /// Tagged credential: bytes 0..28 = hash, byte 28 = type tag (0=key, 1=script).
    pub ptr_map: HashMap<(u64, u32, u32), [u8; 29]>,

    /// The decentralization parameter (d) from the PREVIOUS epoch.
    ///
    /// Haskell's RUPD computes eta using `d` from the epoch whose blocks are being measured
    /// (`nesBprev`'s epoch), not the current epoch's `d`. When d changes (e.g. epoch 1 d=1
    /// → epoch 2 d=0), the eta d-threshold must use the OLD value (d=1) to correctly classify
    /// federated blocks as eta=1, not d=0 which would give eta=0.
    ///
    /// Default is 1/1 (fully federated), appropriate for chain start.
    #[serde(default = "default_prev_epoch_decentralization")]
    pub prev_epoch_decentralization: Rational,
}

fn default_update_quorum() -> u64 {
    5 // Mainnet default: 5 out of 7 genesis delegates
}

fn default_prev_epoch_decentralization() -> Rational {
    Rational { numerator: 1, denominator: 1 }
}

fn default_stability_window_2kf() -> u64 {
    86400 // 2k/f on mainnet: 2*2160/0.05 = 86400
}

impl LedgerState {
    pub fn new(params: ProtocolParameters) -> Self {
        LedgerState {
            epoch: EpochNo(0),
            epoch_length: 432000,
            shelley_transition_epoch: 0,
            byron_epoch_length: 0,
            has_byron: false,
            protocol_params: params,
            stake_distribution: StakeDistributionState::default(),
            treasury: Lovelace(0),
            reserves: Lovelace(MAX_LOVELACE_SUPPLY),
            delegations: Arc::new(HashMap::new()),
            pool_params: Arc::new(HashMap::new()),
            future_pool_params: Arc::new(HashMap::new()),
            pending_retirements: HashMap::new(),
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
            stability_window_2kf: default_stability_window_2kf(), // 2k/f on mainnet
            genesis_hash: [0u8; 32],
            pending_pp_updates: BTreeMap::new(),
            update_quorum: default_update_quorum(),
            governance: Arc::new(GovernanceState::default()),
            deposit_tracker: DepositTracker::default(),
            pending_reward_update: None,
            last_applied_rupd: None,
            script_stake_credentials: HashSet::new(),
            prev_protocol_params: None,
            conway_genesis_epoch: None,
            pending_enactments: Vec::new(),
            ptr_map: HashMap::new(),
            ppup_enacted_log: None,
            prev_epoch_decentralization: default_prev_epoch_decentralization(),
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

    /// Convert an absolute slot to its epoch number, honouring the Byron/Shelley
    /// hard-fork boundary.
    ///
    /// | `has_byron` | `shelley_transition_epoch` | behaviour |
    /// |-------------|---------------------------|-----------|
    /// | `false`     | (any)                     | pure Shelley: `slot / epoch_length` |
    /// | `true`      | `u64::MAX` (sentinel)     | still in Byron: `slot / byron_epoch_length` |
    /// | `true`      | `n`                       | HFC: Byron for slots < n*byron_epoch_length, Shelley after |
    pub fn epoch_of_slot(&self, slot: u64) -> u64 {
        match (self.has_byron, self.shelley_transition_epoch) {
            (false, _) => slot / self.epoch_length.max(1),
            (true, u64::MAX) => slot / self.byron_epoch_length.max(1),
            (true, n) => {
                let byron_slots = self.byron_epoch_length.saturating_mul(n);
                if slot < byron_slots {
                    slot / self.byron_epoch_length.max(1)
                } else {
                    n.saturating_add((slot - byron_slots) / self.epoch_length.max(1))
                }
            }
        }
    }

    /// Returns `(slot_offset_within_epoch, epoch_length)` for display purposes.
    pub fn slot_within_epoch(&self, slot: u64) -> (u64, u64) {
        match (self.has_byron, self.shelley_transition_epoch) {
            (false, _) => {
                let len = self.epoch_length.max(1);
                (slot % len, len)
            }
            (true, u64::MAX) => {
                let len = self.byron_epoch_length.max(1);
                (slot % len, len)
            }
            (true, n) => {
                let byron_slots = self.byron_epoch_length.saturating_mul(n);
                if slot < byron_slots {
                    let len = self.byron_epoch_length.max(1);
                    (slot % len, len)
                } else {
                    let len = self.epoch_length.max(1);
                    ((slot - byron_slots) % len, len)
                }
            }
        }
    }

    /// Returns `true` when the given epoch is Shelley or later (i.e. the Shelley
    /// hard fork has occurred and `process_epoch_transition` should run).
    pub fn is_shelley_plus_epoch(&self, epoch: u64) -> bool {
        match (self.has_byron, self.shelley_transition_epoch) {
            (false, _) => true,
            (true, u64::MAX) => false,
            (true, n) => epoch >= n,
        }
    }

    /// Record that a Shelley hard-fork update proposal (version 2.0.0) was seen
    /// on-chain.  Only acts when the transition epoch is still the sentinel.
    pub fn record_shelley_hf_proposal(&mut self, proposal_epoch: u64) {
        if self.has_byron && self.shelley_transition_epoch == u64::MAX {
            self.shelley_transition_epoch = proposal_epoch + 1;
        }
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

    /// Script-type hot committee credentials (needed to emit correct type tag in dumps)
    #[serde(default)]
    pub script_committee_hot_credentials: HashSet<Hash32>,

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

    /// Conway current protocol parameters (initialized from Conway genesis at era transition,
    /// updated by ParameterChange governance actions). None in Babbage era.
    ///
    /// This is separate from LedgerState.protocol_params because:
    /// - At the Conway genesis epoch: protocol_params still shows Babbage params (for that epoch's rewards)
    /// - From the next epoch onwards: protocol_params is updated to match conway_cur_params
    #[serde(default)]
    pub conway_cur_params: Option<Box<ProtocolParameters>>,

    /// DRep voting power snapshot taken at the END of each epoch transition.
    ///
    /// Maps DRep credential hash → total delegated stake (UTxO + rewards + govDeposits).
    /// Ratification at epoch N+1 uses this snapshot (frozen at epoch N boundary).
    /// Matches Haskell's `setFreshDRepPulsingState` / `DRepPulser` behaviour.
    /// Empty until the first Conway epoch transition completes.
    #[serde(default)]
    pub drep_power_snapshot: HashMap<Hash32, u64>,

    /// AlwaysNoConfidence stake in the DRep power snapshot
    #[serde(default)]
    pub drep_no_confidence_snapshot: u64,

    /// AlwaysAbstain stake in the DRep power snapshot
    #[serde(default)]
    pub drep_abstain_snapshot: u64,
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
    /// Set of staking credentials delegated to this DRep (Haskell: drepDelegs).
    ///
    /// This is the reverse mapping of vote_delegations. It is maintained alongside
    /// vote_delegations and is critical for clearDRepDelegations on UnRegDRepCert.
    ///
    /// NOTE: Due to PV9 bug (Haskell #4772), when a credential re-delegates from
    /// DRep A to DRep B (where B is registered), the credential is NOT removed from
    /// A's delegs set. This means a credential can appear in MULTIPLE DReps' delegs
    /// sets. On UnRegDRepCert for A, clearDRepDelegations iterates A.delegs and sets
    /// each credential's casDRepDelegation to Nothing -- even if the credential has
    /// since re-delegated to B. This is the correct (bug-compatible) behavior for PV9.
    ///
    /// NOTE: This field MUST be serialized (not skipped) because the PV9 stale entries
    /// cannot be reconstructed from vote_delegations alone. On snapshot restore, the
    /// startup code only rebuilds from current vote_delegations (1:1 mapping), losing
    /// the stale multi-DRep entries that Haskell preserves in its UMap.
    #[serde(default)]
    pub delegs: HashSet<Hash32>,
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

/// Cardano uses a "mark / set / go / pay" snapshot model:
/// - "mark" is the snapshot taken at the current epoch boundary
/// - "set" is the snapshot from the previous epoch (used for leader election)
/// - "go" is the snapshot from two epochs ago (used for reward calculation stake)
/// - "pay" is a derived snapshot: go's stake + current epoch's blocks (nesBprev).
///   This combines the correct 2-epoch-old stake with the just-ended epoch's blocks
///   for RUPD (Reward UPDate) calculation, matching Haskell's createRUpd inputs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EpochSnapshots {
    /// Snapshot from the most recent epoch boundary ("mark")
    pub mark: Option<StakeSnapshot>,
    /// Snapshot from one epoch ago ("set") — used for leader election
    pub set: Option<StakeSnapshot>,
    /// Snapshot from two epochs ago ("go") — stake basis for reward calculation
    pub go: Option<StakeSnapshot>,
    /// Reward calculation snapshot ("pay"): go's stake + current epoch's blocks.
    ///
    /// Built at each epoch boundary as a copy of `go` with `epoch_blocks_by_pool`
    /// replaced by the just-ended epoch's block counts (`self.epoch_blocks_by_pool`).
    /// This is the direct input to `calculate_rewards` / createRUpd.
    #[serde(default)]
    pub pay: Option<StakeSnapshot>,
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
    /// Script stake credentials at snapshot time.
    /// Stored here so dumps use the correct type tag (scriptHash vs keyHash) even after
    /// a credential is later deregistered from the live ledger state.
    #[serde(default)]
    pub script_stake_credentials: HashSet<Hash32>,
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

    /// Sum all governance proposal deposits for a given return credential.
    ///
    /// The deposit_tracker indexes governance deposits by the proposal's `return_addr` credential.
    /// This returns the total governance deposit amount attributed to `cred` as a return address.
    pub fn governance_deposits_by_return_cred(&self, cred: &Hash32) -> u64 {
        self.deposits
            .get(cred)
            .map(|d| d.governance.iter().map(|(_, amount)| amount.0).sum())
            .unwrap_or(0)
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

    #[test]
    fn test_ledger_state_bincode_roundtrip() {
        let state = LedgerState::new(crate::ledger::primitives::ProtocolParameters::default());
        let bytes = bincode::serialize(&state).expect("serialize");
        let restored: LedgerState = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(state.epoch, restored.epoch);
        assert_eq!(state.treasury, restored.treasury);
        assert_eq!(state.reserves, restored.reserves);
        assert_eq!(state.delegations.len(), restored.delegations.len());
        assert_eq!(state.reward_accounts.len(), restored.reward_accounts.len());
    }
}
