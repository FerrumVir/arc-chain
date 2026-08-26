pub mod benchmark;
pub mod block_stm;
pub mod chunk_cache;
pub mod coalesce;
pub mod consensus;
pub mod inference_validator;
pub mod pipeline;
pub mod planner;
pub mod producer;
pub mod rpc;
pub mod state_sync;
pub mod vrf;

/// The live validator set — `(address, stake)` — shared between the consensus
/// loop (which updates it on peer connect/disconnect) and the RPC layer (which
/// reads it for `/validators` and `/health`).
pub type SharedValidators = std::sync::Arc<parking_lot::RwLock<Vec<(arc_crypto::Hash256, u64)>>>;
