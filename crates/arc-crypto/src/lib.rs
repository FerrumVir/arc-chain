pub mod blake3_commit;
pub mod bls;
pub mod pedersen;
pub mod merkle;
pub mod zk_aggregate;
pub mod hash;
pub mod signature;
pub mod stark;
pub mod vrf;
pub mod poseidon;
pub mod zk_rollup;
pub mod proof_compress;
pub mod threshold;
pub mod circuits;
pub mod batch_prover;
pub mod inference_proof;
#[cfg(feature = "stwo-prover")]
pub mod stwo_air;

pub use blake3_commit::{TransactionCommitment, commit_transaction, batch_commit_parallel};
pub use pedersen::{PedersenCommitment, PedersenProof, commit_value, verify_commitment, batch_verify};
pub use merkle::{MerkleTree, MerkleProof, IncrementalMerkle};
pub use zk_aggregate::{AggregateProof, aggregate_proofs, verify_aggregate};
pub use hash::{Hash256, hash_bytes, hash_pair};
pub use signature::{
    Signature, KeyPair, SignatureError,
    address_from_ed25519_pubkey, address_from_secp256k1_pubkey,
    address_from_ml_dsa_pubkey, address_from_falcon_pubkey,
    batch_verify_ed25519, batch_verify_ml_dsa, batch_verify_falcon512,
    falcon_keygen, falcon_sign, falcon_verify, falcon_batch_verify,
    benchmark_keypair, benchmark_address,
    FALCON_PK_LEN, FALCON_SK_LEN, FALCON_SIG_MAX_LEN,
};
pub use vrf::{VrfProof, VrfOutput, vrf_prove, vrf_verify};
pub use poseidon::{
    PoseidonConfig, PoseidonState, PoseidonSponge,
    poseidon_hash, poseidon_hash_with_config, poseidon_hash_bytes, poseidon_merkle_hash,
};
pub use zk_rollup::{
    RollupBatch, RollupTx, RollupProof, RollupProofType, RollupState, RollupConfig,
    RollupVerifier, RollupSequencer, BatchSubmission, FraudProof, DisputeResolution,
};
pub use proof_compress::{
    CompressedProof, CompressionType, CompressionStats, AggregatedProof as CompressedAggregatedProof,
    ProofAggregator, BatchCompressor, compress_proof, decompress_proof,
};
pub use threshold::{
    ThresholdScheme, SecretShare, ShareVerification, ThresholdSignature,
    PartialSignature, ThresholdSigner, ThresholdError, KeyGeneration,
    derive_public_key, ThresholdEncryption, THRESHOLD_TAG_LEN,
};
pub use circuits::{
    Circuit, Gate, Wire, CircuitBuilder, CircuitEvaluator, CircuitError,
    TransferCircuit, TransferResult, StateTransitionCircuit,
};
pub use batch_prover::{
    BatchProver, BatchConfig, ProveTask, ProveResult, ProveStatus, ProverStats,
    verify_mock_proof, task_id_from_seed, circuit_id_from_name,
};
