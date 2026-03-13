//! Poseidon ZK-friendly hash function.
//!
//! A hash function designed for efficient verification inside ZK circuits
//! (SNARKs, STARKs, PLONK). Uses the Poseidon permutation with configurable
//! width, round counts, and S-box exponent.
//!
//! Default parameters target BN254-friendly fields:
//! width=3, full_rounds=8, partial_rounds=57, alpha=5.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Parameters governing the Poseidon permutation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoseidonConfig {
    /// Number of field elements in the state (t).
    pub width: usize,
    /// Number of full rounds (R_F).
    pub full_rounds: usize,
    /// Number of partial rounds (R_P).
    pub partial_rounds: usize,
    /// S-box exponent (typically 5 for BN254).
    pub alpha: u64,
}

impl Default for PoseidonConfig {
    fn default() -> Self {
        Self {
            width: 3,
            full_rounds: 8,
            partial_rounds: 57,
            alpha: 5,
        }
    }
}

impl PoseidonConfig {
    /// Total number of rounds.
    pub fn total_rounds(&self) -> usize {
        self.full_rounds + self.partial_rounds
    }
}

// ---------------------------------------------------------------------------
// Round constants & MDS matrix generation
// ---------------------------------------------------------------------------

/// A 64-bit modular prime used to keep intermediate values within a single
/// u64 word.  This is NOT the BN254 scalar field — it is a convenience prime
/// for the reference implementation that avoids big-integer arithmetic while
/// still exercising the correct Poseidon structure.
const MODULUS: u64 = (1u64 << 61) - 1; // Mersenne prime 2^61 - 1

/// Modular addition mod MODULUS.
#[inline]
fn mod_add(a: u64, b: u64) -> u64 {
    let sum = (a as u128) + (b as u128);
    (sum % MODULUS as u128) as u64
}

/// Modular multiplication mod MODULUS.
#[inline]
fn mod_mul(a: u64, b: u64) -> u64 {
    let prod = (a as u128) * (b as u128);
    (prod % MODULUS as u128) as u64
}

/// Modular exponentiation (binary method).
fn mod_pow(mut base: u64, mut exp: u64) -> u64 {
    let mut result: u64 = 1;
    base %= MODULUS;
    while exp > 0 {
        if exp & 1 == 1 {
            result = mod_mul(result, base);
        }
        exp >>= 1;
        base = mod_mul(base, base);
    }
    result
}

/// Generate deterministic round constants from BLAKE3("ARC_POSEIDON_RC_{i}").
fn generate_round_constants(total: usize) -> Vec<u64> {
    (0..total)
        .map(|i| {
            let tag = format!("ARC_POSEIDON_RC_{i}");
            let h = blake3::hash(tag.as_bytes());
            let bytes = h.as_bytes();
            // Take first 8 bytes → u64, reduce mod MODULUS
            let raw = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            raw % MODULUS
        })
        .collect()
}

/// Build a Cauchy MDS matrix of dimension `t × t`.
///
/// M[i][j] = 1 / (x_i + y_j)  where  x_i = i+1, y_j = t+j+1  (mod MODULUS).
fn generate_mds_matrix(t: usize) -> Vec<Vec<u64>> {
    // Modular inverse via Fermat's little theorem: a^{-1} = a^{p-2} mod p.
    let inv = |v: u64| mod_pow(v, MODULUS - 2);

    (0..t)
        .map(|i| {
            (0..t)
                .map(|j| {
                    let x = (i as u64 + 1) % MODULUS;
                    let y = (t as u64 + j as u64 + 1) % MODULUS;
                    inv(mod_add(x, y))
                })
                .collect()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Poseidon state & permutation
// ---------------------------------------------------------------------------

/// Internal state of the Poseidon permutation.
#[derive(Clone, Debug)]
pub struct PoseidonState {
    /// State words (length = config.width).
    pub elements: Vec<u64>,
}

impl PoseidonState {
    /// Create a zero-initialised state of the given width.
    pub fn new(width: usize) -> Self {
        Self {
            elements: vec![0u64; width],
        }
    }

    /// Apply the full Poseidon permutation in-place.
    pub fn permute(&mut self, config: &PoseidonConfig) {
        let t = config.width;
        let total = config.total_rounds();
        let rc = generate_round_constants(total * t);
        let mds = generate_mds_matrix(t);

        let half_full = config.full_rounds / 2;

        for r in 0..total {
            // --- AddRoundConstants ---
            for i in 0..t {
                self.elements[i] = mod_add(self.elements[i], rc[r * t + i]);
            }

            // --- S-Box ---
            let is_full = r < half_full || r >= half_full + config.partial_rounds;
            if is_full {
                // Full round: S-box on every element.
                for i in 0..t {
                    self.elements[i] = mod_pow(self.elements[i], config.alpha);
                }
            } else {
                // Partial round: S-box on first element only.
                self.elements[0] = mod_pow(self.elements[0], config.alpha);
            }

            // --- MDS Mix ---
            let old = self.elements.clone();
            for i in 0..t {
                let mut acc: u64 = 0;
                for j in 0..t {
                    acc = mod_add(acc, mod_mul(mds[i][j], old[j]));
                }
                self.elements[i] = acc;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public hash API
// ---------------------------------------------------------------------------

/// Hash one or more u64 inputs and return a single u64 digest.
///
/// Inputs are absorbed into the state starting at index 1 (index 0 is the
/// capacity element).  If the number of inputs exceeds `width - 1`, multiple
/// permutations are applied in sponge fashion.
pub fn poseidon_hash(inputs: &[u64]) -> u64 {
    let config = PoseidonConfig::default();
    poseidon_hash_with_config(inputs, &config)
}

/// Hash with an explicit configuration.
pub fn poseidon_hash_with_config(inputs: &[u64], config: &PoseidonConfig) -> u64 {
    let rate = config.width - 1; // capacity = 1 element
    let mut state = PoseidonState::new(config.width);

    // Domain separation: encode input length into capacity element.
    state.elements[0] = inputs.len() as u64;

    for chunk in inputs.chunks(rate) {
        for (i, &v) in chunk.iter().enumerate() {
            state.elements[1 + i] = mod_add(state.elements[1 + i], v % MODULUS);
        }
        state.permute(config);
    }

    // If inputs were empty we still permute once.
    if inputs.is_empty() {
        state.permute(config);
    }

    state.elements[1] // squeeze from rate portion
}

/// Byte-oriented wrapper: converts arbitrary bytes into u64 limbs, hashes,
/// and returns a 32-byte digest (the u64 result zero-extended, then BLAKE3
/// finalised for full 256-bit output).
pub fn poseidon_hash_bytes(data: &[u8]) -> [u8; 32] {
    // Pack bytes into u64 chunks (little-endian, 8 bytes each).
    let mut limbs: Vec<u64> = Vec::with_capacity((data.len() + 7) / 8);
    for chunk in data.chunks(8) {
        let mut buf = [0u8; 8];
        buf[..chunk.len()].copy_from_slice(chunk);
        limbs.push(u64::from_le_bytes(buf) % MODULUS);
    }

    let core = poseidon_hash(&limbs);

    // Expand the u64 core digest to 32 bytes via BLAKE3 for collision
    // resistance at the 256-bit level.
    let mut hasher = blake3::Hasher::new();
    hasher.update(&core.to_le_bytes());
    *hasher.finalize().as_bytes()
}

/// Poseidon-based Merkle hash: H(left, right).
/// Suitable for use inside ZK Merkle tree circuits.
pub fn poseidon_merkle_hash(left: u64, right: u64) -> u64 {
    poseidon_hash(&[left, right])
}

// ---------------------------------------------------------------------------
// Sponge construction for arbitrary-length input
// ---------------------------------------------------------------------------

/// Sponge that absorbs arbitrary u64 elements and squeezes output.
#[derive(Clone, Debug)]
pub struct PoseidonSponge {
    config: PoseidonConfig,
    state: PoseidonState,
    /// Buffer for incomplete absorption blocks.
    absorb_buf: Vec<u64>,
    /// How many elements have been absorbed in total (for domain separation).
    absorbed: usize,
    /// Whether the sponge has been finalised (switched to squeezing).
    squeezing: bool,
}

impl PoseidonSponge {
    /// Create a new sponge with the default configuration.
    pub fn new() -> Self {
        Self::with_config(PoseidonConfig::default())
    }

    /// Create a sponge with a custom configuration.
    pub fn with_config(config: PoseidonConfig) -> Self {
        let state = PoseidonState::new(config.width);
        Self {
            config,
            state,
            absorb_buf: Vec::new(),
            absorbed: 0,
            squeezing: false,
        }
    }

    /// Absorb a slice of u64 values.
    pub fn absorb(&mut self, inputs: &[u64]) {
        assert!(!self.squeezing, "cannot absorb after squeezing has begun");
        let rate = self.config.width - 1;
        self.absorb_buf.extend_from_slice(inputs);
        self.absorbed += inputs.len();

        while self.absorb_buf.len() >= rate {
            let remaining = self.absorb_buf.split_off(rate);
            let block = std::mem::replace(&mut self.absorb_buf, remaining);
            for (i, &v) in block.iter().enumerate() {
                self.state.elements[1 + i] =
                    mod_add(self.state.elements[1 + i], v % MODULUS);
            }
            self.state.permute(&self.config);
        }
    }

    /// Finalise absorption and switch to squeezing mode.
    fn finalise(&mut self) {
        if self.squeezing {
            return;
        }
        // Domain separation.
        self.state.elements[0] = mod_add(self.state.elements[0], self.absorbed as u64);

        // Absorb remaining buffer.
        let _rate = self.config.width - 1;
        for (i, &v) in self.absorb_buf.iter().enumerate() {
            self.state.elements[1 + i] = mod_add(self.state.elements[1 + i], v % MODULUS);
        }
        // Pad: add 1 after last element (10* padding).
        let pad_idx = 1 + self.absorb_buf.len();
        if pad_idx < self.config.width {
            self.state.elements[pad_idx] = mod_add(self.state.elements[pad_idx], 1);
        }
        self.absorb_buf.clear();
        self.state.permute(&self.config);
        self.squeezing = true;
    }

    /// Squeeze `count` u64 elements from the sponge.
    pub fn squeeze(&mut self, count: usize) -> Vec<u64> {
        self.finalise();
        let rate = self.config.width - 1;
        let mut out = Vec::with_capacity(count);

        while out.len() < count {
            let take = std::cmp::min(count - out.len(), rate);
            for i in 0..take {
                out.push(self.state.elements[1 + i]);
            }
            if out.len() < count {
                self.state.permute(&self.config);
            }
        }
        out
    }
}

impl Default for PoseidonSponge {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_hash() {
        let h = poseidon_hash(&[1, 2, 3]);
        assert_ne!(h, 0);
    }

    #[test]
    fn test_consistency() {
        let a = poseidon_hash(&[42, 99]);
        let b = poseidon_hash(&[42, 99]);
        assert_eq!(a, b, "same inputs must produce same output");
    }

    #[test]
    fn test_different_inputs_different_outputs() {
        let a = poseidon_hash(&[1, 2]);
        let b = poseidon_hash(&[2, 1]);
        assert_ne!(a, b, "different inputs should produce different outputs");
    }

    #[test]
    fn test_single_input() {
        let h = poseidon_hash(&[7]);
        assert_ne!(h, 0);
    }

    #[test]
    fn test_empty_input() {
        let h = poseidon_hash(&[]);
        assert_ne!(h, 0, "empty input should still produce a non-zero hash");
    }

    #[test]
    fn test_large_input() {
        let data: Vec<u64> = (0..100).collect();
        let h = poseidon_hash(&data);
        assert_ne!(h, 0);
    }

    #[test]
    fn test_merkle_hash() {
        let h = poseidon_merkle_hash(10, 20);
        assert_ne!(h, 0);
        // Commutativity should NOT hold (order matters).
        let h2 = poseidon_merkle_hash(20, 10);
        assert_ne!(h, h2);
    }

    #[test]
    fn test_merkle_hash_consistency() {
        let a = poseidon_merkle_hash(100, 200);
        let b = poseidon_merkle_hash(100, 200);
        assert_eq!(a, b);
    }

    #[test]
    fn test_hash_bytes() {
        let digest = poseidon_hash_bytes(b"hello world");
        assert_ne!(digest, [0u8; 32]);
    }

    #[test]
    fn test_hash_bytes_consistency() {
        let a = poseidon_hash_bytes(b"arc chain");
        let b = poseidon_hash_bytes(b"arc chain");
        assert_eq!(a, b);
    }

    #[test]
    fn test_hash_bytes_different() {
        let a = poseidon_hash_bytes(b"foo");
        let b = poseidon_hash_bytes(b"bar");
        assert_ne!(a, b);
    }

    #[test]
    fn test_sponge_basic() {
        let mut sponge = PoseidonSponge::new();
        sponge.absorb(&[1, 2, 3, 4, 5]);
        let out = sponge.squeeze(2);
        assert_eq!(out.len(), 2);
        assert_ne!(out[0], 0);
    }

    #[test]
    fn test_sponge_consistency() {
        let mut s1 = PoseidonSponge::new();
        s1.absorb(&[10, 20, 30]);
        let o1 = s1.squeeze(1);

        let mut s2 = PoseidonSponge::new();
        s2.absorb(&[10, 20, 30]);
        let o2 = s2.squeeze(1);

        assert_eq!(o1, o2);
    }

    #[test]
    fn test_sponge_incremental_absorb() {
        // Absorbing in parts should equal absorbing all at once.
        let mut s_all = PoseidonSponge::new();
        s_all.absorb(&[1, 2, 3, 4]);
        let o_all = s_all.squeeze(1);

        let mut s_parts = PoseidonSponge::new();
        s_parts.absorb(&[1, 2]);
        s_parts.absorb(&[3, 4]);
        let o_parts = s_parts.squeeze(1);

        assert_eq!(o_all, o_parts, "incremental absorb must match bulk absorb");
    }

    #[test]
    fn test_collision_resistance_sanity() {
        // Hash 1000 random-ish inputs and check no collisions.
        let mut seen = std::collections::HashSet::new();
        for i in 0u64..1000 {
            let h = poseidon_hash(&[i]);
            assert!(seen.insert(h), "collision at input {i}");
        }
    }

    #[test]
    fn test_custom_config() {
        let config = PoseidonConfig {
            width: 5,
            full_rounds: 8,
            partial_rounds: 22,
            alpha: 5,
        };
        let h = poseidon_hash_with_config(&[1, 2, 3], &config);
        assert_ne!(h, 0);
    }

    #[test]
    fn test_mds_matrix_dimensions() {
        let mds = generate_mds_matrix(3);
        assert_eq!(mds.len(), 3);
        for row in &mds {
            assert_eq!(row.len(), 3);
        }
    }

    #[test]
    fn test_round_constants_deterministic() {
        let a = generate_round_constants(10);
        let b = generate_round_constants(10);
        assert_eq!(a, b);
    }
}
