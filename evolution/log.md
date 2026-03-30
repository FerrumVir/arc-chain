# Evolution Log

## Evolution 1 — 2026-03-29 19:55
**What:** Persistent earnings + SIGTERM graceful shutdown
- Created `crates/arc-node/src/earnings.rs`: `EarningsTracker` struct that wraps inference_count/inference_earned AtomicU64s with automatic disk persistence to `<data_dir>/earnings.json`. Atomic write (tmp + rename) prevents corruption on crash.
- Wired `EarningsTracker` into `ConsensusManager` and `NodeState` (RPC layer). Both code paths (direct RPC inference and P2P consensus inference) now persist earnings to disk on every inference.
- Added SIGTERM/SIGINT handler in `main.rs` that saves earnings before exit. Ensures no data loss on `kill`, `systemctl stop`, or Ctrl+C.
- Added `"persistence": true/false` field to `/worker/earnings` endpoint so dashboards can show persistence status.
- On startup, existing earnings are loaded from disk and counters resume from where they left off.

**Why:** inference_count and inference_earned were in-memory AtomicU64s initialized to 0. Every node restart wiped all earnings history — users lost proof of their work. This was the #1 UX pain point for community workers.

**Verified:**
- Local Mac worker: 16 inferences / 1600 ARC survived restart (pre-seeded, then auto-incremented to 17/1700)
- SIGTERM handler logged "saving earnings and shutting down" and persisted before exit
- Launchd auto-restart picked up persisted earnings correctly
- New `persistence` field appears in /worker/earnings response

**Rollback:** git checkout evolution-1

**Files:**
- `crates/arc-node/src/earnings.rs` (NEW — EarningsTracker)
- `crates/arc-node/src/lib.rs` (added `pub mod earnings`)
- `crates/arc-node/src/main.rs` (EarningsTracker init, SIGTERM handler, wiring)
- `crates/arc-node/src/rpc.rs` (EarningsTracker in NodeState, persist on inference)
- `crates/arc-node/src/consensus.rs` (EarningsTracker field, persist on P2P inference)
---
