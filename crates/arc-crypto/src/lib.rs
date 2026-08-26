// Clippy-lint policy is workspace-wide - see `[workspace.lints.clippy]` in
// the root Cargo.toml. The rationale is that rewriting crypto hot-paths to
// satisfy stylistic lints risks a one-bit divergence in on-chain attestation
// verification between old and new nodes, which would fracture consensus.

pub mod batch_prover;
pub mod blake3_commit;
pub mod bls;
pub mod circuits;
pub mod hash;
pub mod inference_proof;
pub mod merkle;
pub mod pedersen;
pub mod poseidon;
pub mod proof_compress;
pub mod signature;
pub mod stark;
#[cfg(feature = "stwo-prover")]
pub mod stwo_air;
pub mod threshold;
pub mod vrf;
pub mod zk_aggregate;
pub mod zk_rollup;

pub use batch_prover::{
    BatchConfig, BatchProver, ProveResult, ProveStatus, ProveTask, ProverStats,
    circuit_id_from_name, task_id_from_seed, verify_mock_proof,
};
pub use blake3_commit::{TransactionCommitment, batch_commit_parallel, commit_transaction};
pub use circuits::{
    Circuit, CircuitBuilder, CircuitError, CircuitEvaluator, Gate, StateTransitionCircuit,
    TransferCircuit, TransferResult, Wire,
};
pub use hash::{Hash256, hash_bytes, hash_pair};
pub use merkle::{IncrementalMerkle, MerkleProof, MerkleTree};
pub use pedersen::{
    PedersenCommitment, PedersenProof, batch_verify, commit_value, verify_commitment,
};
pub use poseidon::{
    PoseidonConfig, PoseidonSponge, PoseidonState, poseidon_hash, poseidon_hash_bytes,
    poseidon_hash_with_config, poseidon_merkle_hash,
};
pub use proof_compress::{
    AggregatedProof as CompressedAggregatedProof, BatchCompressor, CompressedProof,
    CompressionStats, CompressionType, ProofAggregator, compress_proof, decompress_proof,
};
pub use signature::{
    FALCON_PK_LEN, FALCON_SIG_MAX_LEN, FALCON_SK_LEN, KeyPair, Signature, SignatureError,
    address_from_ed25519_pubkey, address_from_falcon_pubkey, address_from_ml_dsa_pubkey,
    address_from_secp256k1_pubkey, batch_verify_ed25519, batch_verify_falcon512,
    batch_verify_ml_dsa, benchmark_address, benchmark_keypair, falcon_batch_verify, falcon_keygen,
    falcon_sign, falcon_verify,
};
pub use threshold::{
    KeyGeneration, PartialSignature, SecretShare, ShareVerification, THRESHOLD_TAG_LEN,
    ThresholdEncryption, ThresholdError, ThresholdScheme, ThresholdSignature, ThresholdSigner,
    derive_public_key,
};
pub use vrf::{VrfOutput, VrfProof, vrf_prove, vrf_verify};
pub use zk_aggregate::{AggregateProof, aggregate_proofs, verify_aggregate};
pub use zk_rollup::{
    BatchSubmission, DisputeResolution, FraudProof, RollupBatch, RollupConfig, RollupProof,
    RollupProofType, RollupSequencer, RollupState, RollupTx, RollupVerifier,
};
