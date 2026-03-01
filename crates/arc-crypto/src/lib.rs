pub mod blake3_commit;
pub mod pedersen;
pub mod merkle;
pub mod zk_aggregate;
pub mod hash;
pub mod signature;

pub use blake3_commit::{TransactionCommitment, commit_transaction, batch_commit_parallel};
pub use pedersen::{PedersenCommitment, PedersenProof, commit_value, verify_commitment, batch_verify};
pub use merkle::{MerkleTree, MerkleProof};
pub use zk_aggregate::{AggregateProof, aggregate_proofs, verify_aggregate};
pub use hash::{Hash256, hash_bytes, hash_pair};
pub use signature::{
    Signature, KeyPair, SignatureError,
    address_from_ed25519_pubkey, address_from_secp256k1_pubkey, address_from_ml_dsa_pubkey,
    batch_verify_ed25519, batch_verify_ml_dsa,
};
