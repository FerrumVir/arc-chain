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

## Evolution 2 — 2026-03-29 20:11 (Cycle 2)
**Commit:** dedfc76
**Tag:** evolution-2
**What:** Mobile-responsive dashboard + animated earnings counter + strip special tokens from inference output

### Dashboard (dashboard/worker.html)
- Rewrote layout for mobile-first responsive design using Tailwind `sm:` breakpoints
- All grids stack vertically on phone screens (peers, activity, stats)
- Text scales: `text-[10px]` on mobile, `text-xs`/`text-sm` on desktop
- Addresses truncate properly; install command wraps on narrow screens
- Copy button goes full-width on mobile via `flex-col sm:flex-row`
- Added `animateCounter()`: smooth slot-machine style count-up using `requestAnimationFrame` with ease-out cubic easing. Fires on every 5-second refresh cycle when earnings change.
- Added `earning-glow` CSS animation: green `text-shadow` pulse (rgba(74,222,128)) on the earnings number when new ARC arrives
- Toast container constrained to `max-w-[90vw]` to prevent overflow on phones
- Peer IPs hidden on mobile (`hidden sm:inline`) to save space

### Special Token Stripping (rpc.rs + consensus.rs)
- Added `strip_special_tokens()` function in both files
- Strips: `</s>`, `<s>`, `<unk>`, `<pad>`, `[INST]`, `[/INST]`, `<<SYS>>`, `<</SYS>>`, `[SPEAK]`, `[/SPEAK]`
- Collapses runs of whitespace left by stripping, then trims
- Applied in direct inference path (rpc.rs:inference_run) after `model.decode()`
- Applied in P2P worker inference path (consensus.rs) in both candle and INT8 decode branches

**Why:** Dashboard was desktop-only -- unusable on phone screens where community workers actually check their earnings. The static number display felt lifeless; animated counters make earning feel rewarding. Special tokens leaked through the tokenizer on TinyLlama, making API output look broken.

**Verified:**
- Mac worker dashboard serves new version (5 `animateCounter` references in HTML)
- All 8 seed nodes serve new dashboard (LAX verified)
- Direct inference output: "Artificial intelligence (AI) is a field of computer science..." -- no special tokens
- Mac worker restarted cleanly with SIGTERM, earnings persisted (9700 ARC after restart)
- All 8 nodes restarted via rolling-deploy.sh, all reporting 8+ peers
- Binary hash: 6ae2e24a0dc55560636fe566ee91887c141826deadc8e990c0ac3faab49cc0b7

**Known issue (pre-existing, NOT from this cycle):**
- P2P inference from seeds to Mac worker times out intermittently after node restarts
- Root cause: QUIC connections cycle every ~70s due to VPS UDP timeout (handoff item #9)
- Self-healing recovers connections but creates windows where inference broadcasts miss the worker
- This was observed before evolution-2 as well

**Rollback:** `git checkout evolution-1` (revert to pre-cycle-2 state)

**Files changed:**
- `dashboard/worker.html` (responsive layout, animated counter, glow animation)
- `crates/arc-node/src/rpc.rs` (strip_special_tokens function + applied to inference_run)
- `crates/arc-node/src/consensus.rs` (strip_special_tokens function + applied to P2P inference)
---

## Evolution 3 — 2026-03-29 20:45
**Commit:** 70c3f00
**Tag:** evolution-3
**What:** Network leaderboard endpoint + dashboard leaderboard section

### Backend (crates/arc-node/src/rpc.rs)
- Added `GET /worker/leaderboard` endpoint
- Queries all 8 seed nodes' `/worker/earnings` endpoints in parallel using `tokio::spawn` + `reqwest` with 3s timeout
- Deduplicates by address (prevents double-counting if a node is both seed and local)
- Adds the local node to the list if not already present (community workers won't be seeds)
- Labels the local node "You" for easy identification
- Sorts by total_arc descending, then inferences descending as tiebreaker
- Returns ranked list with: rank, address, label, total_arc, inferences, uptime_hours, status, is_seed
- Response includes `your_rank`, `total_nodes`, `your_address`, `updated_at`
- Route registered at `/worker/leaderboard` in the main router

### Dashboard (dashboard/worker.html)
- Added leaderboard section between earnings hero and stats row (high visibility placement)
- Shows ranked list of all network nodes with ARC earned, inference count, status dot
- Top 3 get colored medal dots (gold, silver, bronze)
- "You" row highlighted with arc-500/10 background and arc-500/30 border
- Footer stats: Your Rank, Total Nodes, Network ARC (total across all nodes)
- Leaderboard refreshes every 15s (every 3rd refresh cycle) to avoid hammering seed nodes
- Country flags for seed nodes, status-colored dots (green=active, yellow=connected, gray=offline)
- Mobile responsive: inference count column hidden on small screens, text scaling matches existing pattern

**Why:** Leaderboards create competition and gamification — users want to see their rank and compare against others. This is the single highest-impact engagement feature for community workers. Seeing "Rank #1" or "Rank #42 of 200" creates retention and motivation to keep nodes running.

**Verified:**
- Mac worker: leaderboard shows 9 nodes, Mac at #1 with 13,400 ARC (134 inferences)
- NYC seed: leaderboard shows 8 seeds, labels itself "You" at #1
- AMS seed: leaderboard shows 8 seeds, labels itself "You" at #3
- All 8 seeds upgraded via rolling-deploy.sh, all report 8 peers
- Dashboard serves leaderboard section on both Mac worker and seeds (12 leaderboard references in HTML)
- Binary hash: 91073e31d2243d988914893431209acf4687ae955b08ce0578b66ce296933d48

**Rollback:** `git checkout evolution-2`

**Files changed:**
- `crates/arc-node/src/rpc.rs` (worker_leaderboard handler + route registration)
- `dashboard/worker.html` (leaderboard section HTML + updateLeaderboard() JS)
- `evolution/log.md` (added missing Evolution 2 entry + this Evolution 3 entry)
---
