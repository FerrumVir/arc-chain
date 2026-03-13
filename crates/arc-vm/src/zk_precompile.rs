//! ZK Proof Verification Precompile
//! Allows smart contracts to verify ZK proofs on-chain.

use std::collections::HashMap;

/// Verification key registered on-chain
#[derive(Debug, Clone)]
pub struct VerificationKey {
    pub circuit_id: [u8; 32],
    pub key_data: Vec<u8>,
    pub circuit_name: String,
    pub registered_at: u64,
    pub registrar: [u8; 32],
}

/// ZK proof submitted for verification
#[derive(Debug, Clone)]
pub struct ZkProofInput {
    pub circuit_id: [u8; 32],
    pub proof_data: Vec<u8>,
    pub public_inputs: Vec<u64>,
}

/// Result of ZK proof verification
#[derive(Debug, Clone)]
pub struct ZkVerifyResult {
    pub valid: bool,
    pub circuit_id: [u8; 32],
    pub gas_used: u64,
    pub error: Option<String>,
}

/// Registry for verification keys and proof verification.
///
/// Maintains a set of registered verification keys identified by circuit ID
/// and provides mock ZK proof verification using BLAKE3 hashing.
pub struct ZkVerifierRegistry {
    keys: HashMap<[u8; 32], VerificationKey>,
    verification_count: u64,
    gas_per_proof: u64,
}

impl ZkVerifierRegistry {
    /// Create a new empty registry with the given gas cost per proof verification.
    pub fn new(gas_per_proof: u64) -> Self {
        Self {
            keys: HashMap::new(),
            verification_count: 0,
            gas_per_proof,
        }
    }

    /// Register a verification key for a circuit.
    ///
    /// Returns an error if a key for the same circuit ID is already registered,
    /// or if the key data is empty.
    pub fn register_key(&mut self, key: VerificationKey) -> Result<(), String> {
        if key.key_data.is_empty() {
            return Err("Verification key data must not be empty".to_string());
        }
        if key.circuit_name.is_empty() {
            return Err("Circuit name must not be empty".to_string());
        }
        if self.keys.contains_key(&key.circuit_id) {
            return Err(format!(
                "Verification key already registered for circuit {:?}",
                &key.circuit_id[..4]
            ));
        }
        self.keys.insert(key.circuit_id, key);
        Ok(())
    }

    /// Verify a ZK proof against a registered verification key.
    ///
    /// Mock implementation: computes BLAKE3 hash of (circuit_id || public_inputs)
    /// and checks that proof_data starts with the first 8 bytes of that hash.
    pub fn verify_proof(&mut self, input: &ZkProofInput) -> ZkVerifyResult {
        self.verification_count += 1;

        // Check that a verification key is registered for this circuit
        if !self.keys.contains_key(&input.circuit_id) {
            return ZkVerifyResult {
                valid: false,
                circuit_id: input.circuit_id,
                gas_used: self.gas_per_proof,
                error: Some("No verification key registered for circuit".to_string()),
            };
        }

        if input.proof_data.is_empty() {
            return ZkVerifyResult {
                valid: false,
                circuit_id: input.circuit_id,
                gas_used: self.gas_per_proof,
                error: Some("Proof data must not be empty".to_string()),
            };
        }

        // Compute expected prefix: BLAKE3(circuit_id || public_inputs as le bytes)
        let expected_prefix = Self::compute_expected_prefix(&input.circuit_id, &input.public_inputs);

        // Proof is valid if its data starts with the expected 8-byte prefix
        let valid = input.proof_data.len() >= 8
            && input.proof_data[..8] == expected_prefix[..8];

        ZkVerifyResult {
            valid,
            circuit_id: input.circuit_id,
            gas_used: self.gas_per_proof,
            error: if valid {
                None
            } else {
                Some("Proof verification failed: prefix mismatch".to_string())
            },
        }
    }

    /// Compute the expected proof prefix from circuit_id and public inputs.
    ///
    /// This is the mock verification logic: BLAKE3(circuit_id || public_inputs_bytes).
    /// Returns the first 8 bytes of the hash.
    pub fn compute_expected_prefix(circuit_id: &[u8; 32], public_inputs: &[u64]) -> [u8; 8] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(circuit_id);
        for &val in public_inputs {
            hasher.update(&val.to_le_bytes());
        }
        let hash = hasher.finalize();
        let mut prefix = [0u8; 8];
        prefix.copy_from_slice(&hash.as_bytes()[..8]);
        prefix
    }

    /// Look up a registered verification key by circuit ID.
    pub fn get_key(&self, circuit_id: &[u8; 32]) -> Option<&VerificationKey> {
        self.keys.get(circuit_id)
    }

    /// Remove a verification key from the registry.
    ///
    /// Returns true if the key was found and removed, false otherwise.
    pub fn deregister_key(&mut self, circuit_id: &[u8; 32]) -> bool {
        self.keys.remove(circuit_id).is_some()
    }

    /// Return statistics: (total verification count, number of registered keys).
    pub fn stats(&self) -> (u64, usize) {
        (self.verification_count, self.keys.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_circuit_id(seed: u8) -> [u8; 32] {
        let mut id = [0u8; 32];
        id[0] = seed;
        id
    }

    fn make_registrar(seed: u8) -> [u8; 32] {
        let mut r = [0u8; 32];
        r[0] = seed;
        r
    }

    fn make_key(seed: u8) -> VerificationKey {
        VerificationKey {
            circuit_id: make_circuit_id(seed),
            key_data: vec![1, 2, 3, 4],
            circuit_name: format!("circuit_{}", seed),
            registered_at: 1000,
            registrar: make_registrar(seed),
        }
    }

    #[test]
    fn test_register_and_get_key() {
        let mut registry = ZkVerifierRegistry::new(100_000);
        let key = make_key(1);
        assert!(registry.register_key(key.clone()).is_ok());

        let retrieved = registry.get_key(&make_circuit_id(1)).unwrap();
        assert_eq!(retrieved.circuit_name, "circuit_1");
        assert_eq!(retrieved.key_data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_register_duplicate_key_fails() {
        let mut registry = ZkVerifierRegistry::new(100_000);
        let key = make_key(2);
        assert!(registry.register_key(key.clone()).is_ok());
        let result = registry.register_key(key);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already registered"));
    }

    #[test]
    fn test_register_empty_key_data_fails() {
        let mut registry = ZkVerifierRegistry::new(100_000);
        let mut key = make_key(3);
        key.key_data = vec![];
        let result = registry.register_key(key);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must not be empty"));
    }

    #[test]
    fn test_register_empty_circuit_name_fails() {
        let mut registry = ZkVerifierRegistry::new(100_000);
        let mut key = make_key(4);
        key.circuit_name = String::new();
        let result = registry.register_key(key);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Circuit name"));
    }

    #[test]
    fn test_verify_valid_proof() {
        let mut registry = ZkVerifierRegistry::new(50_000);
        let key = make_key(5);
        let circuit_id = key.circuit_id;
        registry.register_key(key).unwrap();

        let public_inputs = vec![42u64, 100u64];
        let prefix = ZkVerifierRegistry::compute_expected_prefix(&circuit_id, &public_inputs);

        let mut proof_data = prefix.to_vec();
        proof_data.extend_from_slice(&[0xAA; 64]); // padding

        let input = ZkProofInput {
            circuit_id,
            proof_data,
            public_inputs,
        };

        let result = registry.verify_proof(&input);
        assert!(result.valid);
        assert_eq!(result.gas_used, 50_000);
        assert!(result.error.is_none());
    }

    #[test]
    fn test_verify_invalid_proof() {
        let mut registry = ZkVerifierRegistry::new(50_000);
        let key = make_key(6);
        let circuit_id = key.circuit_id;
        registry.register_key(key).unwrap();

        let input = ZkProofInput {
            circuit_id,
            proof_data: vec![0xFF; 64], // wrong prefix
            public_inputs: vec![1, 2, 3],
        };

        let result = registry.verify_proof(&input);
        assert!(!result.valid);
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("prefix mismatch"));
    }

    #[test]
    fn test_verify_unregistered_circuit_fails() {
        let mut registry = ZkVerifierRegistry::new(50_000);

        let input = ZkProofInput {
            circuit_id: make_circuit_id(99),
            proof_data: vec![1; 32],
            public_inputs: vec![1],
        };

        let result = registry.verify_proof(&input);
        assert!(!result.valid);
        assert!(result.error.unwrap().contains("No verification key"));
    }

    #[test]
    fn test_verify_empty_proof_data_fails() {
        let mut registry = ZkVerifierRegistry::new(50_000);
        let key = make_key(7);
        let circuit_id = key.circuit_id;
        registry.register_key(key).unwrap();

        let input = ZkProofInput {
            circuit_id,
            proof_data: vec![],
            public_inputs: vec![1],
        };

        let result = registry.verify_proof(&input);
        assert!(!result.valid);
        assert!(result.error.unwrap().contains("must not be empty"));
    }

    #[test]
    fn test_deregister_key() {
        let mut registry = ZkVerifierRegistry::new(50_000);
        let key = make_key(8);
        let circuit_id = key.circuit_id;
        registry.register_key(key).unwrap();

        assert!(registry.deregister_key(&circuit_id));
        assert!(registry.get_key(&circuit_id).is_none());
        // Deregister again returns false
        assert!(!registry.deregister_key(&circuit_id));
    }

    #[test]
    fn test_stats() {
        let mut registry = ZkVerifierRegistry::new(50_000);
        assert_eq!(registry.stats(), (0, 0));

        registry.register_key(make_key(10)).unwrap();
        registry.register_key(make_key(11)).unwrap();
        assert_eq!(registry.stats(), (0, 2));

        // Run a verification to increment count
        let input = ZkProofInput {
            circuit_id: make_circuit_id(10),
            proof_data: vec![0; 32],
            public_inputs: vec![],
        };
        registry.verify_proof(&input);
        assert_eq!(registry.stats(), (1, 2));

        registry.deregister_key(&make_circuit_id(10));
        assert_eq!(registry.stats(), (1, 1));
    }

    #[test]
    fn test_proof_with_different_public_inputs_produces_different_prefix() {
        let circuit_id = make_circuit_id(12);
        let prefix_a = ZkVerifierRegistry::compute_expected_prefix(&circuit_id, &[1, 2, 3]);
        let prefix_b = ZkVerifierRegistry::compute_expected_prefix(&circuit_id, &[4, 5, 6]);
        assert_ne!(prefix_a, prefix_b);
    }
}
