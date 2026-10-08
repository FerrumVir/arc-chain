//! Stage-to-stage networking for sharded inference (ENG-9).
//!
//! Per-answer speed across a sharded model is set by one token's trip
//! through every stage, so this module is about cutting that trip:
//!
//! * [`wire`]: the framed message format and the transport trait
//!   ([`wire::StageSink`] / [`wire::StageSource`]) with persistent tuned TCP
//!   and in-process implementations;
//! * [`codec`]: exact activation compression — zigzag bit-packing of the
//!   `i64` residual stream, lossless, with commitments over the canonical
//!   bytes so digests never change;
//! * [`shaper`]: WAN simulation (per-hop RTT, jitter, bounded uplink);
//! * [`pipeline`]: a micro-batched ring of stages that overlaps compute with
//!   transfer and records a per-hop serialize / transfer / compute breakdown;
//! * [`cost`]: research-7's latency model;
//! * [`placement`]: the placement optimizer (regional cells, fewest fastest
//!   stages, exact ring order, layer and expert slices).
//!
//! Nothing here touches consensus, validators or the public RPC pipeline.
//! The benchmark binary is `arc_wan_bench`.

pub mod codec;
pub mod cost;
pub mod pipeline;
pub mod placement;
pub mod shaper;
pub mod wire;
