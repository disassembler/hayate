// Nonce state machine (TICKN rule)
//
// Copied from torsten-ledger/src/state/epoch.rs
// Implements Cardano's nonce evolution and epoch nonce computation

use super::primitives::*;
use super::state::LedgerState;
use cryptoxide::{blake2b::Blake2b, digest::Digest};

/// Blake2b-256 hash function
pub fn blake2b_256(data: &[u8]) -> Hash32 {
    let mut hasher = Blake2b::new(32);
    hasher.input(data);
    let mut result = [0u8; 32];
    hasher.result(&mut result);
    result
}

impl LedgerState {
    /// Compute new epoch nonce per Haskell TICKN rule.
    ///
    /// Called at epoch boundaries. Uses the OLD last_epoch_block_nonce first,
    /// then updates it to the current lab_nonce.
    ///
    /// From Haskell:
    ///   TRC (TicknEnv extraEntropy ηc ηph, TicknState _ ηh, newEpoch)
    ///   epochNonce'    = ηc ⭒ ηh ⭒ extraEntropy   (uses OLD prevHashNonce)
    ///   prevHashNonce' = ηph                         (THEN updates to current labNonce)
    ///
    /// Nonce combine (⭒) with NeutralNonce (ZERO) as identity:
    ///   NeutralNonce ⭒ x = x;  x ⭒ NeutralNonce = x
    ///   Nonce(a) ⭒ Nonce(b) = Nonce(blake2b_256(a || b))
    pub fn compute_epoch_nonce(&mut self) {
        let candidate = self.candidate_nonce;
        let prev_hash_nonce = self.last_epoch_block_nonce; // OLD value

        tracing::debug!(
            epoch = self.epoch.0,
            candidate = %hex::encode(candidate),
            prev_hash_nonce = %hex::encode(prev_hash_nonce),
            block_count = self.epoch_block_count,
            "Epoch nonce inputs"
        );

        // Step 1: Compute new epoch nonce using OLD prevHashNonce
        // Uses Haskell's Nonce combine (⭒) with NeutralNonce (ZERO) as identity
        // extraEntropy is NeutralNonce on all real networks, so omitted
        let zero = [0u8; 32];
        self.epoch_nonce = if candidate == zero && prev_hash_nonce == zero {
            zero
        } else if candidate == zero {
            prev_hash_nonce
        } else if prev_hash_nonce == zero {
            candidate // identity: candidate ⭒ NeutralNonce = candidate
        } else {
            let mut nonce_input = Vec::with_capacity(64);
            nonce_input.extend_from_slice(&candidate);
            nonce_input.extend_from_slice(&prev_hash_nonce);
            blake2b_256(&nonce_input)
        };

        // Step 2: NOW update prevHashNonce to current labNonce for NEXT epoch
        self.last_epoch_block_nonce = self.lab_nonce;

        tracing::debug!(
            epoch = self.epoch.0,
            nonce = %hex::encode(self.epoch_nonce),
            "Epoch nonce computed"
        );

        // evolving_nonce and candidate_nonce carry forward unchanged
        // (they are NOT reset at epoch boundaries)
    }

    /// Update the evolving nonce with a pre-computed nonce VRF contribution (eta).
    ///
    /// evolving_nonce = blake2b_256(evolving_nonce || eta)
    ///
    /// The `nonce_eta` argument is the era-specific nonce contribution:
    ///
    /// - Shelley/Allegra/Mary/Alonzo (TPraos): eta = blake2b_256(nonce_vrf_cert.0)
    /// - Babbage/Conway (Praos): eta = blake2b_256("N" || vrf_result.0)
    ///
    /// This function does NOT do any additional hashing of the input — the caller
    /// (block application) is responsible for computing eta correctly per era.
    ///
    /// Matches Haskell's reupdateChainDepState:
    ///   eta = vrfNonceValue block
    ///   evolving_nonce' = updateNonce evolving_nonce eta
    ///   where updateNonce n e = hash (n <> e)
    pub fn update_evolving_nonce(&mut self, nonce_eta: &[u8]) {
        // ALWAYS hash the input — matching pallas's generate_rolling_nonce exactly.
        // DO NOT use a pass-through for 32-byte inputs — this was verified to produce
        // wrong nonces. The hash step is required for both TPraos and Praos.
        let eta_hash = blake2b_256(nonce_eta);
        let mut data = Vec::with_capacity(64);
        data.extend_from_slice(&self.evolving_nonce);
        data.extend_from_slice(&eta_hash);
        self.evolving_nonce = blake2b_256(&data);
    }

    /// Update candidate nonce if not in the stabilization window.
    ///
    /// Candidate nonce tracks evolving nonce UNLESS the block is within the
    /// last randomness_stabilisation_window (4k/f) slots of the epoch,
    /// in which case the candidate freezes so the epoch nonce is stable.
    pub fn update_candidate_nonce(&mut self, current_slot: u64) {
        // Calculate first slot of next epoch
        let first_slot_of_next_epoch = self.first_slot_of_next_epoch();

        // Candidate nonce tracks evolving nonce OUTSIDE the stability window
        if current_slot
            .saturating_add(self.randomness_stabilisation_window)
            < first_slot_of_next_epoch
        {
            self.candidate_nonce = self.evolving_nonce;
        }
    }

    /// Update LAB (last applied block) nonce.
    ///
    /// LAB nonce is just the prev_hash of the current block (type-cast, no hashing).
    /// Matches Haskell's praosStateLabNonce.
    pub fn update_lab_nonce(&mut self, block_prev_hash: Hash32) {
        self.lab_nonce = block_prev_hash;
    }

    /// Get first slot of next epoch
    fn first_slot_of_next_epoch(&self) -> u64 {
        self.first_slot_of_epoch(self.epoch.0.saturating_add(1))
    }

    /// Get first slot of a given epoch
    fn first_slot_of_epoch(&self, epoch: u64) -> u64 {
        if epoch < self.shelley_transition_epoch {
            // Byron epoch
            epoch * self.byron_epoch_length
        } else {
            // Shelley+ epoch
            let shelley_epochs_passed = epoch.saturating_sub(self.shelley_transition_epoch);
            let byron_slots = self.shelley_transition_epoch * self.byron_epoch_length;
            let shelley_slots = shelley_epochs_passed * self.epoch_length;
            byron_slots + shelley_slots
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blake2b_256() {
        let data = b"test";
        let hash = blake2b_256(data);
        assert_eq!(hash.len(), 32);
        // Verify it's deterministic
        let hash2 = blake2b_256(data);
        assert_eq!(hash, hash2);
    }

    #[test]
    fn test_epoch_nonce_neutral() {
        let mut state = LedgerState::new(ProtocolParameters::default());
        state.candidate_nonce = [0u8; 32];
        state.last_epoch_block_nonce = [0u8; 32];
        state.compute_epoch_nonce();
        // Zero ⭒ Zero = Zero
        assert_eq!(state.epoch_nonce, [0u8; 32]);
    }

    #[test]
    fn test_epoch_nonce_identity() {
        let mut state = LedgerState::new(ProtocolParameters::default());
        let test_nonce = [0xaa; 32];
        state.candidate_nonce = test_nonce;
        state.last_epoch_block_nonce = [0u8; 32];
        state.compute_epoch_nonce();
        // candidate ⭒ Zero = candidate
        assert_eq!(state.epoch_nonce, test_nonce);
    }

    #[test]
    fn test_candidate_nonce_freezing() {
        let mut state = LedgerState::new(ProtocolParameters::default());
        state.epoch_length = 100;
        state.shelley_transition_epoch = 0;
        state.byron_epoch_length = 100;
        state.randomness_stabilisation_window = 20;

        let test_evolving = [0xbb; 32];
        state.evolving_nonce = test_evolving;

        // Slot 50: well before freeze window (100 - 20 = 80)
        state.update_candidate_nonce(50);
        assert_eq!(state.candidate_nonce, test_evolving);

        // Slot 79: still before freeze
        state.update_candidate_nonce(79);
        assert_eq!(state.candidate_nonce, test_evolving);

        // Change evolving nonce
        let new_evolving = [0xcc; 32];
        state.evolving_nonce = new_evolving;

        // Slot 80: exactly at freeze window start - should freeze
        let frozen_value = state.candidate_nonce;
        state.update_candidate_nonce(80);
        assert_eq!(state.candidate_nonce, frozen_value); // Should NOT update

        // Slot 90: in freeze window - should stay frozen
        state.update_candidate_nonce(90);
        assert_eq!(state.candidate_nonce, frozen_value); // Should NOT update
    }
}
