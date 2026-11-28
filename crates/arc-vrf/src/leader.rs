//! The VRF input (`alpha`) for leader election and committee sampling.
//!
//! RFC 9381 leaves `alpha` to the application. ARC fixes one byte layout so
//! that every consumer (proposer, verifier, light client, explorer) hashes the
//! same bytes, and so that a proof made for one purpose can never be replayed
//! as a proof for another:
//!
//! ```text
//! alpha = len(domain) as u8 || domain || chain_id (32) || epoch (u64 LE) || slot (u64 LE) || epoch_seed (32)
//! ```
//!
//! - `domain` is an ASCII tag per [`Purpose`]. It names the ciphersuite too, so
//!   a future suite change also changes every input.
//! - `chain_id` separates networks; the genesis block hash is the intended value.
//! - `epoch` and `slot` are fixed-width, so the encoding is injective: no two
//!   distinct inputs share an `alpha`.
//! - `epoch_seed` is the per-epoch randomness fixed before the epoch starts
//!   (lookahead), so nobody can grind the input after seeing the committee.
//!
//! The public key is the try-and-increment salt (RFC 9381 Section 5.5), so two
//! validators evaluating the same `alpha` still hash to different curve points.
//!
//! Nothing in consensus consumes this yet. The wiring PR owns the lottery math
//! (how `beta` becomes a stake-weighted decision); this module only fixes what
//! gets hashed.

/// What a VRF evaluation is for. Each purpose has its own domain tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// Leader (block proposer) election for one slot.
    LeaderElection,
    /// Committee sampling for one slot, or for an epoch with `slot = 0`.
    CommitteeSampling,
    /// A validator's contribution to the next epoch seed.
    EpochSeedContribution,
}

impl Purpose {
    /// The ASCII domain tag hashed in front of every input of this purpose.
    pub const fn domain(self) -> &'static [u8] {
        match self {
            Purpose::LeaderElection => b"ARC-ECVRF-EDWARDS25519-SHA512-TAI/leader-election/v1",
            Purpose::CommitteeSampling => {
                b"ARC-ECVRF-EDWARDS25519-SHA512-TAI/committee-sampling/v1"
            }
            Purpose::EpochSeedContribution => {
                b"ARC-ECVRF-EDWARDS25519-SHA512-TAI/epoch-seed-contribution/v1"
            }
        }
    }

    /// Every purpose, for tests and tooling.
    pub const ALL: [Purpose; 3] = [
        Purpose::LeaderElection,
        Purpose::CommitteeSampling,
        Purpose::EpochSeedContribution,
    ];
}

/// A fully specified VRF input. [`VrfInput::alpha`] is the only byte layout ARC hashes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VrfInput {
    /// What the evaluation is for; selects the domain tag.
    pub purpose: Purpose,
    /// Network identifier; the genesis block hash is the intended value.
    pub chain_id: [u8; 32],
    /// Epoch number.
    pub epoch: u64,
    /// Slot (round) within the epoch; `0` for per-epoch purposes.
    pub slot: u64,
    /// The epoch seed fixed before the epoch began (lookahead).
    pub epoch_seed: [u8; 32],
}

impl VrfInput {
    /// Length of `alpha` after the length-prefixed domain tag.
    const FIXED_SUFFIX_LENGTH: usize = 32 + 8 + 8 + 32;

    /// Encodes the input as the `alpha_string` passed to `prove` and `verify`.
    pub fn alpha(&self) -> Vec<u8> {
        let domain = self.purpose.domain();
        let domain_len =
            u8::try_from(domain.len()).expect("domain tags are shorter than 256 bytes");
        let mut alpha = Vec::with_capacity(1 + domain.len() + Self::FIXED_SUFFIX_LENGTH);
        alpha.push(domain_len);
        alpha.extend_from_slice(domain);
        alpha.extend_from_slice(&self.chain_id);
        alpha.extend_from_slice(&self.epoch.to_le_bytes());
        alpha.extend_from_slice(&self.slot.to_le_bytes());
        alpha.extend_from_slice(&self.epoch_seed);
        alpha
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(purpose: Purpose, epoch: u64, slot: u64) -> VrfInput {
        VrfInput {
            purpose,
            chain_id: [0xAA; 32],
            epoch,
            slot,
            epoch_seed: [0x55; 32],
        }
    }

    #[test]
    fn alpha_is_length_prefixed_domain_then_fixed_width_fields() {
        let alpha = input(Purpose::LeaderElection, 0x0102_0304_0506_0708, 9).alpha();
        let domain = Purpose::LeaderElection.domain();
        assert_eq!(
            alpha.len(),
            1 + domain.len() + VrfInput::FIXED_SUFFIX_LENGTH
        );
        assert_eq!(alpha[0] as usize, domain.len());
        assert_eq!(&alpha[1..=domain.len()], domain);
        let rest = &alpha[1 + domain.len()..];
        assert_eq!(&rest[..32], &[0xAA; 32]);
        assert_eq!(
            &rest[32..40],
            &[8u8, 7, 6, 5, 4, 3, 2, 1],
            "epoch is little-endian"
        );
        assert_eq!(
            &rest[40..48],
            &[9u8, 0, 0, 0, 0, 0, 0, 0],
            "slot is little-endian"
        );
        assert_eq!(&rest[48..], &[0x55; 32]);
    }

    #[test]
    fn purposes_have_distinct_domains_and_distinct_alphas() {
        for (i, a) in Purpose::ALL.iter().enumerate() {
            for (j, b) in Purpose::ALL.iter().enumerate() {
                if i != j {
                    assert_ne!(a.domain(), b.domain());
                    assert_ne!(input(*a, 1, 1).alpha(), input(*b, 1, 1).alpha());
                }
            }
            assert!(a.domain().len() < 256);
            assert!(a.domain().is_ascii());
        }
    }

    #[test]
    fn every_field_changes_alpha() {
        let base = input(Purpose::LeaderElection, 1, 1);
        let mut other_chain = base;
        other_chain.chain_id[0] ^= 1;
        let mut other_seed = base;
        other_seed.epoch_seed[31] ^= 1;
        let variants = [
            input(Purpose::LeaderElection, 2, 1),
            input(Purpose::LeaderElection, 1, 2),
            input(Purpose::CommitteeSampling, 1, 1),
            other_chain,
            other_seed,
        ];
        for variant in variants {
            assert_ne!(variant.alpha(), base.alpha());
        }
        assert_eq!(base.alpha(), input(Purpose::LeaderElection, 1, 1).alpha());
    }
}
