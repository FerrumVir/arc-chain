use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoseidonConfig {
    pub width: usize,
    pub full_rounds: usize,
    pub partial_rounds: usize,
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
    pub fn total_rounds(&self) -> usize {
        self.full_rounds + self.partial_rounds
    }
}

const MODULUS: u64 = (1u64 << 61) - 1;

#[inline]
fn mod_add(a: u64, b: u64) -> u64 {
    let sum = (a as u128) + (b as u128);
    (sum % MODULUS as u128) as u64
}

#[inline]
fn mod_mul(a: u64, b: u64) -> u64 {
    let prod = (a as u128) * (b as u128);
    (prod % MODULUS as u128) as u64
}

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

fn generate_round_constants(total: usize) -> Vec<u64> {
    (0..total)
        .map(|i| {
            let tag = format!("ARC_POSEIDON_RC_{i}");
            let h = blake3::hash(tag.as_bytes());
            let bytes = h.as_bytes();
            let raw = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            raw % MODULUS
        })
        .collect()
}

fn generate_mds_matrix(t: usize) -> Vec<Vec<u64>> {
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

#[derive(Clone, Debug)]
pub struct PoseidonState {
    pub elements: Vec<u64>,
}

impl PoseidonState {
    pub fn new(width: usize) -> Self {
        Self {
            elements: vec![0u64; width],
        }
    }

    pub fn permute(&mut self, config: &PoseidonConfig) {
        let t = config.width;
        let total = config.total_rounds();
        let rc = generate_round_constants(total * t);
        let mds = generate_mds_matrix(t);

        let half_full = config.full_rounds / 2;

        for r in 0..total {
            for i in 0..t {
                self.elements[i] = mod_add(self.elements[i], rc[r * t + i]);
            }

            let is_full = r < half_full || r >= half_full + config.partial_rounds;
            if is_full {
                for i in 0..t {
                    self.elements[i] = mod_pow(self.elements[i], config.alpha);
                }
            } else {
                self.elements[0] = mod_pow(self.elements[0], config.alpha);
            }

            let old = self.elements.clone();
            for (i, row) in mds.iter().enumerate().take(t) {
                let mut acc: u64 = 0;
                for (j, &o) in old.iter().enumerate().take(t) {
                    acc = mod_add(acc, mod_mul(row[j], o));
                }
                self.elements[i] = acc;
            }
        }
    }
}

pub fn poseidon_hash(inputs: &[u64]) -> u64 {
    let config = PoseidonConfig::default();
    poseidon_hash_with_config(inputs, &config)
}

pub fn poseidon_hash_with_config(inputs: &[u64], config: &PoseidonConfig) -> u64 {
    let rate = config.width - 1;
    let mut state = PoseidonState::new(config.width);

    state.elements[0] = inputs.len() as u64;

    for chunk in inputs.chunks(rate) {
        for (i, &v) in chunk.iter().enumerate() {
            state.elements[1 + i] = mod_add(state.elements[1 + i], v % MODULUS);
        }
        state.permute(config);
    }

    if inputs.is_empty() {
        state.permute(config);
    }

    state.elements[1]
}

pub fn poseidon_hash_bytes(data: &[u8]) -> [u8; 32] {
    let mut limbs: Vec<u64> = Vec::with_capacity(data.len().div_ceil(8));
    for chunk in data.chunks(8) {
        let mut buf = [0u8; 8];
        buf[..chunk.len()].copy_from_slice(chunk);
        limbs.push(u64::from_le_bytes(buf) % MODULUS);
    }

    let core = poseidon_hash(&limbs);

    let mut hasher = blake3::Hasher::new();
    hasher.update(&core.to_le_bytes());
    *hasher.finalize().as_bytes()
}

pub fn poseidon_merkle_hash(left: u64, right: u64) -> u64 {
    poseidon_hash(&[left, right])
}

#[derive(Clone, Debug)]
pub struct PoseidonSponge {
    config: PoseidonConfig,
    state: PoseidonState,
    absorb_buf: Vec<u64>,
    absorbed: usize,
    squeezing: bool,
}

impl PoseidonSponge {
    pub fn new() -> Self {
        Self::with_config(PoseidonConfig::default())
    }

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

    pub fn absorb(&mut self, inputs: &[u64]) {
        assert!(!self.squeezing, "cannot absorb after squeezing has begun");
        let rate = self.config.width - 1;
        self.absorb_buf.extend_from_slice(inputs);
        self.absorbed += inputs.len();

        while self.absorb_buf.len() >= rate {
            let remaining = self.absorb_buf.split_off(rate);
            let block = std::mem::replace(&mut self.absorb_buf, remaining);
            for (i, &v) in block.iter().enumerate() {
                self.state.elements[1 + i] = mod_add(self.state.elements[1 + i], v % MODULUS);
            }
            self.state.permute(&self.config);
        }
    }

    fn finalise(&mut self) {
        if self.squeezing {
            return;
        }
        self.state.elements[0] = mod_add(self.state.elements[0], self.absorbed as u64);

        let _rate = self.config.width - 1;
        for (i, &v) in self.absorb_buf.iter().enumerate() {
            self.state.elements[1 + i] = mod_add(self.state.elements[1 + i], v % MODULUS);
        }
        let pad_idx = 1 + self.absorb_buf.len();
        if pad_idx < self.config.width {
            self.state.elements[pad_idx] = mod_add(self.state.elements[pad_idx], 1);
        }
        self.absorb_buf.clear();
        self.state.permute(&self.config);
        self.squeezing = true;
    }

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
