//! Precompiled contracts for ARC Chain's VM.
//!
//! Built-in functions callable by smart contracts at fixed addresses in the
//! range `0x0000...0001` through `0x0000...00FF`. Analogous to Ethereum's
//! ecrecover (0x01) and friends, but tailored to ARC's cryptographic stack:
//!
//! | Address | Name              | Description                              |
//! |---------|-------------------|------------------------------------------|
//! | `0x01`  | BLAKE3            | Hash arbitrary input to 32 bytes         |
//! | `0x02`  | Ed25519 verify    | Verify an Ed25519 signature              |
//! | `0x03`  | VRF random        | Read the current block's VRF random      |
//! | `0x04`  | Price oracle      | Fetch latest USD price for a token       |
//! | `0x05`  | Merkle verify     | Verify a BLAKE3 Merkle inclusion proof   |
//! | `0x06`  | Block info        | Query block height / timestamp / etc.    |
//! | `0x07`  | Identity / DID    | Look up on-chain identity for an address |
//! | `0x08`  | Falcon-512 verify | Verify a post-quantum Falcon signature   |
//! | `0x09`  | ZK proof verify   | Verify a ZK proof against registered key |
//! | `0x0A`  | AI inference      | Run model inference (deterministic)      |
//! | `0x0B`  | BLS verify        | Verify a BLS12-381 signature             |

// Add to lib.rs: pub mod precompiles;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use arc_crypto::{Hash256, MerkleProof, MerkleTree};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Precompile address space: 0x0000...0001 through 0x0000...00FF.
pub type PrecompileAddress = [u8; 32];

/// Result of a precompile call.
#[derive(Debug, Clone)]
pub struct PrecompileResult {
    pub success: bool,
    pub output: Vec<u8>,
    pub gas_used: u64,
    pub error: Option<String>,
}

/// Oracle price feed data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceFeed {
    /// Token address (32 bytes).
    pub token: [u8; 32],
    /// Price in USD with 18 decimal places.
    pub price_usd: u128,
    /// Unix timestamp of the price update.
    pub timestamp: u64,
    /// Monotonically increasing round identifier.
    pub round_id: u64,
    /// Human-readable source name.
    pub source: String,
}

/// Oracle registry storing latest price feeds and VRF random outputs.
pub struct OracleRegistry {
    feeds: HashMap<[u8; 32], PriceFeed>,
    /// block_height -> VRF random output (32 bytes).
    vrf_outputs: HashMap<u64, [u8; 32]>,
}

impl OracleRegistry {
    /// Create an empty oracle registry.
    pub fn new() -> Self {
        Self {
            feeds: HashMap::new(),
            vrf_outputs: HashMap::new(),
        }
    }

    /// Insert or update a price feed.
    pub fn update_price(&mut self, feed: PriceFeed) {
        self.feeds.insert(feed.token, feed);
    }

    /// Get the latest price feed for a token.
    pub fn get_price(&self, token: &[u8; 32]) -> Option<&PriceFeed> {
        self.feeds.get(token)
    }

    /// Store a VRF random output for a given block height.
    pub fn set_vrf_random(&mut self, height: u64, random: [u8; 32]) {
        self.vrf_outputs.insert(height, random);
    }

    /// Retrieve the VRF random output for a given block height.
    pub fn get_vrf_random(&self, height: u64) -> Option<[u8; 32]> {
        self.vrf_outputs.get(&height).copied()
    }

    /// Number of price feeds in the registry.
    pub fn price_count(&self) -> usize {
        self.feeds.len()
    }
}

impl Default for OracleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Precompile registry
// ---------------------------------------------------------------------------

struct PrecompileEntry {
    name: String,
    base_gas: u64,
    per_word_gas: u64,
    handler: Box<dyn Fn(&[u8]) -> PrecompileResult + Send + Sync>,
}

/// Registry of precompiled contracts.
///
/// Each precompile is stored as a closure over its fixed address. State-dependent
/// precompiles (Oracle, VRF, Block info) read from an `Arc<RwLock<OracleRegistry>>`
/// that is shared with the block producer / execution engine.
pub struct PrecompileRegistry {
    precompiles: HashMap<PrecompileAddress, PrecompileEntry>,
}

impl PrecompileRegistry {
    /// Register all default precompiles.
    ///
    /// `oracle` is shared state that is updated by the block producer before
    /// executing transactions. `current_height`, `current_timestamp`,
    /// `current_proposer`, and `current_state_root` supply the block context
    /// for the block-info precompile (0x06).
    pub fn new() -> Self {
        // Shared oracle with mock data — real integration wires in the actual
        // registries later.
        let oracle = Arc::new(RwLock::new(OracleRegistry::new()));

        // Mock block context values for standalone use. The execution engine
        // will call `new_with_context()` for real block parameters.
        Self::new_with_context(
            oracle,
            0,                // height
            0,                // timestamp
            [0u8; 32],        // proposer
            [0u8; 32],        // state_root
        )
    }

    /// Create a registry with explicit block context and oracle state.
    pub fn new_with_context(
        oracle: Arc<RwLock<OracleRegistry>>,
        current_height: u64,
        current_timestamp: u64,
        current_proposer: [u8; 32],
        current_state_root: [u8; 32],
    ) -> Self {
        let mut precompiles = HashMap::new();

        // ── 0x01: BLAKE3 hash ──────────────────────────────────────────
        {
            let mut addr = [0u8; 32];
            addr[31] = 0x01;
            precompiles.insert(addr, PrecompileEntry {
                name: "blake3".into(),
                base_gas: 60,
                per_word_gas: 12,
                handler: Box::new(|input: &[u8]| {
                    let hash = blake3::hash(input);
                    PrecompileResult {
                        success: true,
                        output: hash.as_bytes().to_vec(),
                        gas_used: 60 + (input.len() as u64 / 32) * 12,
                        error: None,
                    }
                }),
            });
        }

        // ── 0x02: Ed25519 verify ───────────────────────────────────────
        // Input layout: 32B pubkey ‖ 64B signature ‖ msg (variable)
        // Output: 1 byte (1 = valid, 0 = invalid)
        {
            let mut addr = [0u8; 32];
            addr[31] = 0x02;
            precompiles.insert(addr, PrecompileEntry {
                name: "ed25519_verify".into(),
                base_gas: 3000,
                per_word_gas: 0,
                handler: Box::new(|input: &[u8]| {
                    if input.len() < 96 {
                        return PrecompileResult {
                            success: true,
                            output: vec![0],
                            gas_used: 3000,
                            error: Some("input too short: need 32B pubkey + 64B sig + msg".into()),
                        };
                    }

                    let pubkey_bytes: [u8; 32] = input[..32].try_into().unwrap();
                    let sig_bytes: [u8; 64] = input[32..96].try_into().unwrap();
                    let msg = &input[96..];

                    let valid = ed25519_verify_raw(&pubkey_bytes, &sig_bytes, msg);
                    PrecompileResult {
                        success: true,
                        output: vec![if valid { 1 } else { 0 }],
                        gas_used: 3000,
                        error: None,
                    }
                }),
            });
        }

        // ── 0x03: VRF random ──────────────────────────────────────────
        // Input: empty (reads current block's VRF output)
        // Output: 32 bytes of randomness
        {
            let mut addr = [0u8; 32];
            addr[31] = 0x03;
            let oracle_ref = Arc::clone(&oracle);
            let height = current_height;
            precompiles.insert(addr, PrecompileEntry {
                name: "vrf_random".into(),
                base_gas: 100,
                per_word_gas: 0,
                handler: Box::new(move |_input: &[u8]| {
                    let guard = oracle_ref.read().unwrap();
                    match guard.get_vrf_random(height) {
                        Some(random) => PrecompileResult {
                            success: true,
                            output: random.to_vec(),
                            gas_used: 100,
                            error: None,
                        },
                        None => {
                            // Return a deterministic fallback: BLAKE3("arc-vrf-fallback" || height)
                            let mut hasher = blake3::Hasher::new_derive_key("arc-vrf-fallback-v1");
                            hasher.update(&height.to_le_bytes());
                            let hash = hasher.finalize();
                            PrecompileResult {
                                success: true,
                                output: hash.as_bytes().to_vec(),
                                gas_used: 100,
                                error: None,
                            }
                        }
                    }
                }),
            });
        }

        // ── 0x04: Price oracle ─────────────────────────────────────────
        // Input: 32B token address
        // Output: 16B price (u128 little-endian)
        {
            let mut addr = [0u8; 32];
            addr[31] = 0x04;
            let oracle_ref = Arc::clone(&oracle);
            precompiles.insert(addr, PrecompileEntry {
                name: "price_oracle".into(),
                base_gas: 200,
                per_word_gas: 0,
                handler: Box::new(move |input: &[u8]| {
                    if input.len() < 32 {
                        return PrecompileResult {
                            success: false,
                            output: vec![],
                            gas_used: 200,
                            error: Some("input must be 32-byte token address".into()),
                        };
                    }

                    let token: [u8; 32] = input[..32].try_into().unwrap();
                    let guard = oracle_ref.read().unwrap();
                    match guard.get_price(&token) {
                        Some(feed) => PrecompileResult {
                            success: true,
                            output: feed.price_usd.to_le_bytes().to_vec(),
                            gas_used: 200,
                            error: None,
                        },
                        None => PrecompileResult {
                            success: false,
                            output: vec![],
                            gas_used: 200,
                            error: Some("no price feed for token".into()),
                        },
                    }
                }),
            });
        }

        // ── 0x05: BLAKE3 Merkle verify ────────────────────────────────
        // Input: 32B root ‖ 32B leaf ‖ proof (variable: n * (32B sibling + 1B is_left))
        // Output: 1 byte (1 = valid, 0 = invalid)
        {
            let mut addr = [0u8; 32];
            addr[31] = 0x05;
            precompiles.insert(addr, PrecompileEntry {
                name: "merkle_verify".into(),
                base_gas: 500,
                per_word_gas: 50,
                handler: Box::new(|input: &[u8]| {
                    if input.len() < 64 {
                        return PrecompileResult {
                            success: true,
                            output: vec![0],
                            gas_used: 500,
                            error: Some("input too short: need root + leaf + proof".into()),
                        };
                    }

                    let root = Hash256(input[..32].try_into().unwrap());
                    let leaf = Hash256(input[32..64].try_into().unwrap());
                    let proof_data = &input[64..];

                    // Each sibling entry = 32 bytes hash + 1 byte is_left flag.
                    let entry_size = 33;
                    if proof_data.len() % entry_size != 0 {
                        return PrecompileResult {
                            success: true,
                            output: vec![0],
                            gas_used: 500,
                            error: Some("malformed proof: each entry must be 33 bytes".into()),
                        };
                    }

                    let mut siblings = Vec::new();
                    for chunk in proof_data.chunks_exact(entry_size) {
                        let hash = Hash256(chunk[..32].try_into().unwrap());
                        let is_left = chunk[32] != 0;
                        siblings.push((hash, is_left));
                    }

                    let proof = MerkleProof {
                        leaf,
                        index: 0, // Index is not needed for verification recomputation
                        siblings,
                        root,
                    };

                    let valid = MerkleTree::verify_proof(&proof);
                    let n_siblings = proof_data.len() / entry_size;
                    PrecompileResult {
                        success: true,
                        output: vec![if valid { 1 } else { 0 }],
                        gas_used: 500 + (n_siblings as u64) * 50,
                        error: None,
                    }
                }),
            });
        }

        // ── 0x06: Block info ──────────────────────────────────────────
        // Input: 1B selector
        //   0 → block height (8B le)
        //   1 → block timestamp (8B le)
        //   2 → block proposer (32B)
        //   3 → state root (32B)
        {
            let mut addr = [0u8; 32];
            addr[31] = 0x06;
            let height = current_height;
            let timestamp = current_timestamp;
            let proposer = current_proposer;
            let state_root = current_state_root;
            precompiles.insert(addr, PrecompileEntry {
                name: "block_info".into(),
                base_gas: 50,
                per_word_gas: 0,
                handler: Box::new(move |input: &[u8]| {
                    if input.is_empty() {
                        return PrecompileResult {
                            success: false,
                            output: vec![],
                            gas_used: 50,
                            error: Some("input must be a 1-byte selector".into()),
                        };
                    }

                    let selector = input[0];
                    match selector {
                        0 => PrecompileResult {
                            success: true,
                            output: height.to_le_bytes().to_vec(),
                            gas_used: 50,
                            error: None,
                        },
                        1 => PrecompileResult {
                            success: true,
                            output: timestamp.to_le_bytes().to_vec(),
                            gas_used: 50,
                            error: None,
                        },
                        2 => PrecompileResult {
                            success: true,
                            output: proposer.to_vec(),
                            gas_used: 50,
                            error: None,
                        },
                        3 => PrecompileResult {
                            success: true,
                            output: state_root.to_vec(),
                            gas_used: 50,
                            error: None,
                        },
                        _ => PrecompileResult {
                            success: false,
                            output: vec![],
                            gas_used: 50,
                            error: Some(format!("unknown selector: {}", selector)),
                        },
                    }
                }),
            });
        }

        // ── 0x07: Identity / DID lookup ───────────────────────────────
        // Input: 32B address
        // Output: 1B identity_level + variable data
        //   level 0 = Anonymous, 1 = Basic, 2 = Verified, 3 = Institutional
        // Returns [0] for unknown addresses (Anonymous, no data).
        {
            let mut addr = [0u8; 32];
            addr[31] = 0x07;
            precompiles.insert(addr, PrecompileEntry {
                name: "identity_lookup".into(),
                base_gas: 400,
                per_word_gas: 0,
                handler: Box::new(|input: &[u8]| {
                    if input.len() < 32 {
                        return PrecompileResult {
                            success: false,
                            output: vec![],
                            gas_used: 400,
                            error: Some("input must be 32-byte address".into()),
                        };
                    }

                    // Stub: all addresses are Anonymous until the real identity
                    // registry is wired in.
                    PrecompileResult {
                        success: true,
                        output: vec![0], // IdentityLevel::Anonymous
                        gas_used: 400,
                        error: None,
                    }
                }),
            });
        }

        // ── 0x08: Falcon-512 verify ───────────────────────────────────
        // Input: 897B pubkey ‖ sig (variable, up to 752B) ‖ msg (variable)
        // We use a length prefix for the signature: 2B little-endian sig_len,
        // then sig_len bytes of signature, then the message.
        // Full layout: 897B pubkey ‖ 2B sig_len ‖ sig_len B sig ‖ msg
        // Output: 1 byte (1 = valid, 0 = invalid)
        {
            let mut addr = [0u8; 32];
            addr[31] = 0x08;
            precompiles.insert(addr, PrecompileEntry {
                name: "falcon512_verify".into(),
                base_gas: 5000,
                per_word_gas: 0,
                handler: Box::new(|input: &[u8]| {
                    // Minimum: 897 (pk) + 2 (sig_len) + 1 (min sig) = 900
                    if input.len() < 900 {
                        return PrecompileResult {
                            success: true,
                            output: vec![0],
                            gas_used: 5000,
                            error: Some("input too short for Falcon-512 verify".into()),
                        };
                    }

                    let pubkey = &input[..897];
                    let sig_len = u16::from_le_bytes([input[897], input[898]]) as usize;

                    // Reject unreasonable signature lengths (Falcon-512 max is 752 bytes)
                    if sig_len == 0 || sig_len > 1024 {
                        return PrecompileResult {
                            success: true,
                            output: vec![0],
                            gas_used: 5000,
                            error: Some(format!("invalid Falcon signature length: {}", sig_len)),
                        };
                    }

                    if input.len() < 899 + sig_len {
                        return PrecompileResult {
                            success: true,
                            output: vec![0],
                            gas_used: 5000,
                            error: Some("input too short for declared signature length".into()),
                        };
                    }

                    let sig = &input[899..899 + sig_len];
                    let msg = &input[899 + sig_len..];

                    let valid = arc_crypto::falcon_verify(pubkey, msg, sig);
                    PrecompileResult {
                        success: true,
                        output: vec![if valid { 1 } else { 0 }],
                        gas_used: 5000,
                        error: None,
                    }
                }),
            });
        }

        // ── 0x09: ZK proof verify ──────────────────────────────────
        // Input: 32B circuit_id ‖ 1B public_inputs_count ‖ (count * 8B LE) public_inputs ‖ proof_data
        // Output: 1 byte (1 = valid, 0 = invalid)
        {
            use crate::zk_precompile::{ZkVerifierRegistry, ZkProofInput};
            let mut addr = [0u8; 32];
            addr[31] = 0x09;
            let zk_registry = Arc::new(RwLock::new(ZkVerifierRegistry::new(100_000)));
            let zk_ref = Arc::clone(&zk_registry);
            precompiles.insert(addr, PrecompileEntry {
                name: "zk_verify".into(),
                base_gas: 100_000,
                per_word_gas: 0,
                handler: Box::new(move |input: &[u8]| {
                    if input.len() < 33 {
                        return PrecompileResult {
                            success: false,
                            output: vec![],
                            gas_used: 100_000,
                            error: Some("input too short: need 32B circuit_id + data".into()),
                        };
                    }

                    let circuit_id: [u8; 32] = input[..32].try_into().unwrap();
                    let count = input[32] as usize;
                    let pi_end = match 33usize.checked_add(count.checked_mul(8).unwrap_or(usize::MAX)) {
                        Some(v) => v,
                        None => return PrecompileResult {
                            success: false,
                            output: vec![],
                            gas_used: 100_000,
                            error: Some("public input count overflow".into()),
                        },
                    };
                    if input.len() < pi_end {
                        return PrecompileResult {
                            success: false,
                            output: vec![],
                            gas_used: 100_000,
                            error: Some("input too short for declared public inputs".into()),
                        };
                    }
                    let mut public_inputs = Vec::with_capacity(count);
                    for i in 0..count {
                        let off = 33 + i * 8;
                        let val = u64::from_le_bytes(input[off..off + 8].try_into().unwrap());
                        public_inputs.push(val);
                    }
                    let proof_data = input[pi_end..].to_vec();

                    let mut guard = zk_ref.write().unwrap();
                    let result = guard.verify_proof(&ZkProofInput {
                        circuit_id,
                        proof_data,
                        public_inputs,
                    });
                    PrecompileResult {
                        success: true,
                        output: vec![if result.valid { 1 } else { 0 }],
                        gas_used: result.gas_used,
                        error: result.error,
                    }
                }),
            });
        }

        // ── 0x0A: AI inference ──────────────────────────────────────
        // Input: 32B model_id ‖ msg (UTF-8)
        // Output: inference result (UTF-8 bytes)
        {
            use crate::inference::{InferenceEngine, InferenceConfig, InferenceRequest, InferenceInput, InferenceParams};
            let mut addr = [0u8; 32];
            addr[31] = 0x0A;
            let engine = Arc::new(RwLock::new(InferenceEngine::new(InferenceConfig {
                max_loaded_models: 16,
                default_timeout_ms: 5_000,
                max_tokens: 1024,
                temperature: 0.7,
            })));
            let eng_ref = Arc::clone(&engine);
            precompiles.insert(addr, PrecompileEntry {
                name: "ai_inference".into(),
                base_gas: 500_000,
                per_word_gas: 1_000,
                handler: Box::new(move |input: &[u8]| {
                    if input.len() < 33 {
                        return PrecompileResult {
                            success: false,
                            output: vec![],
                            gas_used: 500_000,
                            error: Some("input too short: need 32B model_id + data".into()),
                        };
                    }
                    let model_id: [u8; 32] = input[..32].try_into().unwrap();
                    let text = String::from_utf8_lossy(&input[32..]).to_string();
                    let request = InferenceRequest {
                        model_id,
                        input: InferenceInput::Text(text),
                        params: InferenceParams {
                            max_tokens: 256,
                            temperature: 0.7,
                            top_p: 0.9,
                            stop_sequences: vec![],
                        },
                    };
                    let mut guard = eng_ref.write().unwrap();
                    match guard.run_inference(&request) {
                        Ok(resp) => {
                            let output_bytes = match &resp.output {
                                crate::inference::InferenceOutput::Text(s) => s.as_bytes().to_vec(),
                                crate::inference::InferenceOutput::Tokens(t) => {
                                    t.iter().flat_map(|v| v.to_le_bytes()).collect()
                                }
                                crate::inference::InferenceOutput::Embedding(e) => {
                                    e.iter().flat_map(|v| v.to_le_bytes()).collect()
                                }
                                crate::inference::InferenceOutput::Classification(classes) => {
                                    classes.first()
                                        .map(|(label, _)| label.as_bytes().to_vec())
                                        .unwrap_or_default()
                                }
                            };
                            PrecompileResult {
                                success: true,
                                output: output_bytes,
                                gas_used: 500_000 + resp.tokens_used * 1_000,
                                error: None,
                            }
                        }
                        Err(e) => PrecompileResult {
                            success: false,
                            output: vec![],
                            gas_used: 500_000,
                            error: Some(e),
                        },
                    }
                }),
            });
        }

        // ── 0x0B: BLS verify ─────────────────────────────────────────
        // Input: 48B pubkey ‖ 96B signature ‖ msg (variable)
        // Output: 1 byte (1 = valid, 0 = invalid)
        {
            let mut addr = [0u8; 32];
            addr[31] = 0x0B;
            precompiles.insert(addr, PrecompileEntry {
                name: "bls_verify".into(),
                base_gas: 10_000,
                per_word_gas: 0,
                handler: Box::new(|input: &[u8]| {
                    if input.len() < 145 {
                        return PrecompileResult {
                            success: true,
                            output: vec![0],
                            gas_used: 10_000,
                            error: Some("input too short: need 48B pk + 96B sig + msg".into()),
                        };
                    }
                    let mut pk_bytes = [0u8; 48];
                    pk_bytes.copy_from_slice(&input[..48]);
                    let pk = arc_crypto::bls::BlsPublicKey(pk_bytes);

                    let mut sig_bytes = [0u8; 96];
                    sig_bytes.copy_from_slice(&input[48..144]);
                    let sig = arc_crypto::bls::BlsSignature(sig_bytes);

                    let msg = &input[144..];
                    let valid = arc_crypto::bls::bls_verify(&pk, msg, &sig);
                    PrecompileResult {
                        success: true,
                        output: vec![if valid { 1 } else { 0 }],
                        gas_used: 10_000,
                        error: None,
                    }
                }),
            });
        }

        Self { precompiles }
    }

    /// Execute a precompile at the given address.
    pub fn call(&self, address: &PrecompileAddress, input: &[u8]) -> PrecompileResult {
        match self.precompiles.get(address) {
            Some(entry) => (entry.handler)(input),
            None => PrecompileResult {
                success: false,
                output: vec![],
                gas_used: 0,
                error: Some(format!("no precompile at address 0x{}", hex::encode(address))),
            },
        }
    }

    /// Check whether an address hosts a precompile.
    pub fn is_precompile(&self, address: &PrecompileAddress) -> bool {
        self.precompiles.contains_key(address)
    }

    /// List all registered precompiles: (address, name, base_gas).
    pub fn list_precompiles(&self) -> Vec<(PrecompileAddress, String, u64)> {
        let mut list: Vec<_> = self
            .precompiles
            .iter()
            .map(|(addr, entry)| (*addr, entry.name.clone(), entry.base_gas))
            .collect();
        // Sort by address for deterministic output.
        list.sort_by_key(|(addr, _, _)| *addr);
        list
    }

    /// Compute the gas cost for calling a precompile with the given input length.
    pub fn gas_cost(&self, address: &PrecompileAddress, input_len: usize) -> u64 {
        match self.precompiles.get(address) {
            Some(entry) => {
                let words = (input_len as u64 + 31) / 32; // ceiling division
                entry.base_gas + words * entry.per_word_gas
            }
            None => 0,
        }
    }
}

impl Default for PrecompileRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helper: raw Ed25519 verification (no address check)
// ---------------------------------------------------------------------------

/// Verify an Ed25519 signature over a raw message (not a hash).
/// Used by the Ed25519 precompile to verify arbitrary messages.
fn ed25519_verify_raw(pubkey: &[u8; 32], sig: &[u8; 64], msg: &[u8]) -> bool {
    use ed25519_dalek::Verifier;

    let vk = match ed25519_dalek::VerifyingKey::from_bytes(pubkey) {
        Ok(vk) => vk,
        Err(_) => return false,
    };
    let signature = ed25519_dalek::Signature::from_bytes(sig);
    vk.verify(msg, &signature).is_ok()
}

// ---------------------------------------------------------------------------
// Convenience: make a precompile address from a single byte
// ---------------------------------------------------------------------------

/// Create a precompile address from a single-byte identifier (1-255).
pub fn precompile_address(id: u8) -> PrecompileAddress {
    let mut addr = [0u8; 32];
    addr[31] = id;
    addr
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── 1. BLAKE3 precompile ───────────────────────────────────────────

    #[test]
    fn test_blake3_precompile() {
        let registry = PrecompileRegistry::new();
        let addr = precompile_address(0x01);
        let input = b"hello world";

        let result = registry.call(&addr, input);

        assert!(result.success);
        assert_eq!(result.output.len(), 32);
        assert!(result.error.is_none());

        // Verify it matches a direct BLAKE3 hash.
        let expected = blake3::hash(input);
        assert_eq!(&result.output[..], expected.as_bytes());
    }

    // ── 2. BLAKE3 empty input ──────────────────────────────────────────

    #[test]
    fn test_blake3_precompile_empty() {
        let registry = PrecompileRegistry::new();
        let addr = precompile_address(0x01);

        let result = registry.call(&addr, &[]);

        assert!(result.success);
        assert_eq!(result.output.len(), 32);

        let expected = blake3::hash(&[]);
        assert_eq!(&result.output[..], expected.as_bytes());
    }

    // ── 3. Ed25519 verify — valid signature ────────────────────────────

    #[test]
    fn test_ed25519_verify_precompile() {
        use ed25519_dalek::{Signer, SigningKey};

        let registry = PrecompileRegistry::new();
        let addr = precompile_address(0x02);

        // Generate a keypair and sign a message.
        let signing_key = SigningKey::generate(&mut rand::thread_rng());
        let verifying_key = signing_key.verifying_key();
        let msg = b"test message for ed25519 precompile";
        let sig = signing_key.sign(msg);

        // Construct input: pubkey(32) ‖ sig(64) ‖ msg
        let mut input = Vec::new();
        input.extend_from_slice(verifying_key.as_bytes());
        input.extend_from_slice(&sig.to_bytes());
        input.extend_from_slice(msg);

        let result = registry.call(&addr, &input);

        assert!(result.success);
        assert_eq!(result.output, vec![1]);
        assert!(result.error.is_none());
    }

    // ── 4. Ed25519 verify — invalid signature ──────────────────────────

    #[test]
    fn test_ed25519_verify_invalid() {
        use ed25519_dalek::{Signer, SigningKey};

        let registry = PrecompileRegistry::new();
        let addr = precompile_address(0x02);

        let signing_key = SigningKey::generate(&mut rand::thread_rng());
        let verifying_key = signing_key.verifying_key();
        let msg = b"original message";

        // Use a different message's signature with the original message.
        let wrong_sig = signing_key.sign(b"different message");

        let mut input = Vec::new();
        input.extend_from_slice(verifying_key.as_bytes());
        input.extend_from_slice(&wrong_sig.to_bytes());
        input.extend_from_slice(msg);

        let result = registry.call(&addr, &input);

        assert!(result.success);
        assert_eq!(result.output, vec![0]); // Invalid
    }

    // ── 5. Gas cost calculation ────────────────────────────────────────

    #[test]
    fn test_precompile_gas_cost() {
        let registry = PrecompileRegistry::new();

        // BLAKE3: base=60, per_word=12
        let blake3_addr = precompile_address(0x01);
        // 64 bytes = 2 words → 60 + 2*12 = 84
        assert_eq!(registry.gas_cost(&blake3_addr, 64), 84);
        // 0 bytes = 0 words → 60 + 0 = 60
        assert_eq!(registry.gas_cost(&blake3_addr, 0), 60);
        // 33 bytes = ceil(33/32) = 2 words → 60 + 2*12 = 84
        assert_eq!(registry.gas_cost(&blake3_addr, 33), 84);

        // Ed25519: base=3000, per_word=0
        let ed25519_addr = precompile_address(0x02);
        assert_eq!(registry.gas_cost(&ed25519_addr, 200), 3000);

        // Unknown address: 0
        let unknown = precompile_address(0xFF);
        assert_eq!(registry.gas_cost(&unknown, 100), 0);
    }

    // ── 6. Unknown precompile ──────────────────────────────────────────

    #[test]
    fn test_unknown_precompile() {
        let registry = PrecompileRegistry::new();
        let unknown_addr = precompile_address(0xAB);

        let result = registry.call(&unknown_addr, &[]);

        assert!(!result.success);
        assert!(result.output.is_empty());
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("no precompile"));
    }

    // ── 7. List precompiles ────────────────────────────────────────────

    #[test]
    fn test_list_precompiles() {
        let registry = PrecompileRegistry::new();
        let list = registry.list_precompiles();

        // We registered 11 precompiles (0x01 through 0x0B).
        assert_eq!(list.len(), 11);

        // Check that they are sorted by address.
        for i in 1..list.len() {
            assert!(list[i - 1].0 < list[i].0);
        }

        // Verify names of first and last.
        assert_eq!(list[0].1, "blake3");
        assert_eq!(list[0].0[31], 0x01);
        assert_eq!(list[10].1, "bls_verify");
        assert_eq!(list[10].0[31], 0x0B);
    }

    // ── 8. Oracle registry — update and read price ─────────────────────

    #[test]
    fn test_oracle_registry_update() {
        let mut oracle = OracleRegistry::new();
        assert_eq!(oracle.price_count(), 0);

        let token = [0x42u8; 32];
        let feed = PriceFeed {
            token,
            price_usd: 1_500_000_000_000_000_000_000, // $1500 with 18 decimals
            timestamp: 1700000000,
            round_id: 1,
            source: "test-oracle".into(),
        };

        oracle.update_price(feed.clone());
        assert_eq!(oracle.price_count(), 1);

        let fetched = oracle.get_price(&token).unwrap();
        assert_eq!(fetched.price_usd, 1_500_000_000_000_000_000_000);
        assert_eq!(fetched.round_id, 1);
        assert_eq!(fetched.source, "test-oracle");

        // Update the price.
        oracle.update_price(PriceFeed {
            token,
            price_usd: 2_000_000_000_000_000_000_000,
            timestamp: 1700000060,
            round_id: 2,
            source: "test-oracle".into(),
        });
        assert_eq!(oracle.price_count(), 1); // Still 1 feed, just updated.
        let updated = oracle.get_price(&token).unwrap();
        assert_eq!(updated.price_usd, 2_000_000_000_000_000_000_000);
        assert_eq!(updated.round_id, 2);

        // Non-existent token returns None.
        assert!(oracle.get_price(&[0xFFu8; 32]).is_none());
    }

    // ── 9. Oracle VRF random ───────────────────────────────────────────

    #[test]
    fn test_oracle_vrf_random() {
        let mut oracle = OracleRegistry::new();
        let random = [0xABu8; 32];

        assert!(oracle.get_vrf_random(100).is_none());

        oracle.set_vrf_random(100, random);
        assert_eq!(oracle.get_vrf_random(100), Some(random));

        // Different height returns None.
        assert!(oracle.get_vrf_random(101).is_none());
    }

    // ── 10. Block info precompile ──────────────────────────────────────

    #[test]
    fn test_block_info_precompile() {
        let oracle = Arc::new(RwLock::new(OracleRegistry::new()));
        let height = 42u64;
        let timestamp = 1700000000u64;
        let proposer = [0x11u8; 32];
        let state_root = [0x22u8; 32];

        let registry = PrecompileRegistry::new_with_context(
            oracle, height, timestamp, proposer, state_root,
        );
        let addr = precompile_address(0x06);

        // Selector 0: block height.
        let result = registry.call(&addr, &[0]);
        assert!(result.success);
        assert_eq!(result.output, height.to_le_bytes().to_vec());

        // Selector 1: timestamp.
        let result = registry.call(&addr, &[1]);
        assert!(result.success);
        assert_eq!(result.output, timestamp.to_le_bytes().to_vec());

        // Selector 2: proposer.
        let result = registry.call(&addr, &[2]);
        assert!(result.success);
        assert_eq!(result.output, proposer.to_vec());

        // Selector 3: state root.
        let result = registry.call(&addr, &[3]);
        assert!(result.success);
        assert_eq!(result.output, state_root.to_vec());

        // Unknown selector.
        let result = registry.call(&addr, &[4]);
        assert!(!result.success);
        assert!(result.error.unwrap().contains("unknown selector"));

        // Empty input.
        let result = registry.call(&addr, &[]);
        assert!(!result.success);
    }

    // ── 11. is_precompile detection ────────────────────────────────────

    #[test]
    fn test_is_precompile() {
        let registry = PrecompileRegistry::new();

        // Registered addresses (0x01 through 0x0B).
        for id in 1..=0x0Bu8 {
            assert!(
                registry.is_precompile(&precompile_address(id)),
                "0x{:02x} should be a precompile",
                id,
            );
        }

        // Unregistered addresses.
        assert!(!registry.is_precompile(&precompile_address(0x00)));
        assert!(!registry.is_precompile(&precompile_address(0x0C)));
        assert!(!registry.is_precompile(&precompile_address(0xFF)));
        assert!(!registry.is_precompile(&[0xFF; 32]));
    }
}
