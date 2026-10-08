//! Regional swarms: one model split across ordinary nodes, bit-exact.
//!
//! A Kimi-class model (about 600 GB at 4 bits) fits no single community
//! machine. The target is 40+ stages or expert groups on roughly 16 GB
//! community nodes, connected over regional public-internet links. Large
//! machines are optional members. The legacy `island` module/CLI names stay
//! compatible; they do not impose a LAN or large-machine requirement.
//!
//! * [`worker`]: a stage process holding layers `[a, b)`. It runs every item
//!   through its layers, commits the activation hash at each layer boundary,
//!   logs its inputs (audit log and recovery log in one), and forwards.
//! * [`coordinator`]: the ingress. Micro-batched pipeline scheduling with
//!   continuous admission; per-sequence [`commit::Ledger`]s with link checks.
//! * [`commit`]: the ledger and the re-execution audit of one stage.
//! * [`expert`]: expert parallelism inside a stage.
//! * [`transport`]: the pluggable [`transport::Transport`] (TCP first;
//!   in-memory for tests; a wide-area emulator for benchmarks).
//! * [`wire`]: the frames.
//! * [`process`]: islands of `arc-island stage` processes on one host (the
//!   multi-process tests and the benchmark).
//!
//! The topology is a ring: coordinator -> stage 0 -> ... -> stage S-1 ->
//! coordinator, so each token crosses S one-way hops plus the local hand-off
//! to the first stage. Nothing here changes the profile's arithmetic: every
//! stage calls `StageModel::forward`, and an island's tokens, logits hashes
//! and per-boundary digests equal `StageModel::generate` on the whole model
//! for any split, any process count and any schedule.

pub mod commit;
pub mod coordinator;
pub mod expert;
pub mod process;
pub mod replica;
pub mod transport;
pub mod wire;
pub mod worker;

#[cfg(test)]
mod tests;

/// Split `n_layers` into `stages` contiguous ranges as evenly as possible
/// (earlier stages take the remainder), as `[0, c1, ..., n_layers]`.
pub fn even_cuts(n_layers: usize, stages: usize) -> Vec<usize> {
    let stages = stages.clamp(1, n_layers.max(1));
    let (base, extra) = (n_layers / stages, n_layers % stages);
    let mut cuts = vec![0];
    for s in 0..stages {
        let last = *cuts.last().expect("non-empty");
        cuts.push(last + base + usize::from(s < extra));
    }
    cuts
}
