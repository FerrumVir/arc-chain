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

## Evolution 4 — 2026-03-30 02:15
**Commit:** d8b7835
**Tag:** evolution-4
**What:** Earnings history chart — time-series tracking + dashboard visualization

### Backend (crates/arc-node/src/earnings.rs)
- Added `EarningsHistoryPoint` struct: timestamp, epoch_secs, total_arc, total_inferences
- Added `EarningsHistory` struct for on-disk persistence
- `EarningsTracker` now maintains an in-memory `Vec<EarningsHistoryPoint>` protected by `Mutex`
- Every `record_inference()` appends a new timestamped data point
- History persisted to `<data_dir>/earnings_history.json` using atomic write (tmp + rename)
- Loaded from disk on startup (survives restarts)
- Auto-downsampling: when history exceeds 1000 points, older half is merged into 5-minute buckets
  - Recent points kept at full resolution, older points compressed
  - Prevents unbounded file growth on high-throughput workers
- New `get_history()` method returns clone of history for API consumption

### API (crates/arc-node/src/rpc.rs)
- `GET /worker/earnings/history` — returns timestamped earnings data for charting
- Response: `{ "history": [{timestamp, epoch_secs, total_arc, total_inferences}], "count": N }`
- Registered at `/worker/earnings/history` in the router

### Dashboard (dashboard/worker.html)
- Added SVG-based earnings chart between hero section and leaderboard (high visibility)
- No external chart libraries — pure inline SVG keeps it dependency-free and fast
- Time range buttons: 1H / 6H / 24H / All (default=All)
- Gradient fill under the line (arc-500 color with opacity fade)
- Interactive hover: tooltip shows exact ARC, inference count, and time
- 3 horizontal grid lines + auto-scaled Y-axis labels
- X-axis shows start/end timestamps, format adapts to range (time only vs date+time)
- Empty state: "No earnings data yet — complete an inference to start tracking"
- Refreshes every 15s (matches leaderboard cadence to avoid hammering API)
- Mobile responsive: chart scales with viewBox, labels use tiny fonts on small screens

**Why:** The dashboard showed a single number for total earnings — no sense of progress over time. Adding a time-series chart gives users a visual reward loop: they can see their earnings curve grow with each inference. This is the visual equivalent of watching a stock ticker go up — addictive engagement. Combined with the animated counter and leaderboard, the dashboard now has three distinct reward feedback mechanisms.

**Verified:**
- Mac worker: history endpoint returns 13 data points after 13 new inferences (147 total, 14,700 ARC)
- Mac worker: `earnings_history.json` persisted to disk (468 bytes)
- Mac worker: SIGTERM + restart preserves history (launchd auto-restart verified)
- Dashboard serves chart section: "Earnings Over Time" header + SVG chart + range buttons
- All 8 seeds upgraded via rolling-deploy.sh
- NYC seed: history endpoint works (empty because seeds don't do inference)
- AMS seed: dashboard has chart section
- LHR seed: leaderboard still functional (8 nodes)
- Mac worker: leaderboard shows #1 of 9, 8 peers connected
- Binary hash: 3d22d00fceb56210edb03a145b3c4b1b89f9ab292e74435e3fad5ca1a168c238

**Known issue (pre-existing, NOT from this cycle):**
- QUIC peer connections cycle every ~70s due to VPS UDP timeout (handoff item #9)
- After rolling restart, peer_count field shows 0 transiently while connections re-establish
- Consensus was actively running (round 204+) during deploy verification

**Rollback:** `git checkout evolution-3`

**Files changed:**
- `crates/arc-node/src/earnings.rs` (EarningsHistoryPoint, persistence, downsampling, get_history)
- `crates/arc-node/src/rpc.rs` (worker_earnings_history handler + route registration)
- `dashboard/worker.html` (SVG chart section + updateChart/renderChart JS + range buttons)
- `evolution/log.md` (this entry)
---

## Evolution 5 — 2026-03-30 03:10
**Commit:** 772c005
**Tag:** evolution-5
**What:** README update documenting all new features + installer hardening

### README (README.md)
- Updated "Your dashboard shows" section: now lists animated earnings counter with glow, earnings-over-time chart (SVG with time range toggles), network leaderboard with medal icons, mobile responsiveness
- Added "Earnings persist across restarts" callout
- Added curl examples for `/worker/earnings/history` and `/worker/leaderboard`
- Added "Installer options" section documenting `--tiny`, `--skip-model`, `--model PATH`, `--cpu-limit`
- Added 4 new worker endpoints to RPC API table (38 total): `/worker/dashboard`, `/worker/earnings`, `/worker/earnings/history`, `/worker/leaderboard`
- Added 5 new rows to "What's Done" table: Community worker mode, Worker dashboard, Persistent earnings, Earnings chart, Network leaderboard

### Installer hardening (scripts/arc-community.sh)
- Added `--help` / `-h` flag with full usage documentation
- Added pre-flight checks section before Step 1:
  - `curl` installed check (with OS-specific install instructions)
  - `git` installed check (skipped if binary already cached)
  - Disk space check: requires ~5 GB (or ~2 GB with `--skip-model`), reports available space
  - Port 9090 conflict detection: uses `lsof` (Mac/Linux) or `ss` (Linux fallback). Distinguishes between an existing arc-node (warns, will restart) vs another process (fails with PID and process name)
- Model download hardening: `curl --fail` flag prevents silent HTTP error pages being saved as the model file. Post-download file size check (must be >100 MB) catches truncated downloads. Cleanup of partial `.tmp` file on failure.
- Git clone error handling: shows tail of clone output on failure instead of silently swallowing errors
- Python3-free fallbacks: health check and faucet claim sections now use `grep`-based JSON parsing when `python3` is not available
- Health check timeout: reports warning if node doesn't respond after 30s with log file path
- OS-appropriate stop/restart commands in summary: `launchctl` on Mac, `systemctl` on Linux, `kill` fallback

**Why:** The README documented zero of the features built in Evolutions 1-4. A new user reading it would miss persistent earnings, the earnings chart, the leaderboard, mobile responsiveness, and worker mode entirely. The installer failed silently on missing git/curl, had no help flag, didn't check disk space before a 4 GB download, and couldn't detect port conflicts. Both are high-surface-area user touchpoints that were lagging behind the actual capabilities of the live network.

**Verified:**
- Mac worker: earnings endpoint returns 15,800 ARC, 158 inferences, 8 peers, persistence=true
- Mac worker: dashboard serves HTML with all Evolution 1-4 features
- Mac worker: inference produces clean output ("Blockchain is a distributed ledger...")
- Mac worker: leaderboard shows rank #1 of 9 nodes
- Mac worker: earnings history returns 15+ data points
- Installer `--help` works on Mac (local) and both NYC and AMS seeds (remote)
- Installer syntax check passes (`bash -n`)
- All 8 seeds upgraded via rolling-deploy.sh
- NYC seed: leaderboard returns 8 nodes
- LAX seed: dashboard serves HTML
- Binary hash unchanged (no Rust code changes): 3d22d00fceb56210edb03a145b3c4b1b89f9ab292e74435e3fad5ca1a168c238

**Known issue (pre-existing, NOT from this cycle):**
- `peer_count` in `/health` shows 0 transiently after rolling restart due to QUIC UDP timeout cycling (~70s)
- Inference still flows correctly during this period (verified in node logs)
- Self-healing reconnection confirmed in NYC logs (new inbound connections replacing stale ones)

**Rollback:** `git checkout evolution-4`

**Files changed:**
- `README.md` (documented all Evolution 1-4 features, added worker endpoints to API table, added installer options section, updated What's Done table)
- `scripts/arc-community.sh` (--help flag, pre-flight checks, download validation, python3-free fallbacks, OS-appropriate stop/restart commands)
- `evolution/log.md` (this entry)
---

## Evolution 6 — 2026-03-30 04:00
**Commit:** a33b7fc
**Tag:** evolution-6
**What:** Unit tests for earnings tracker, strip_special_tokens, and downsampling

### earnings.rs — 16 tests
- `new_tracker_starts_at_zero` — fresh tracker has 0 count, 0 earned, empty history
- `record_inference_increments_count_and_earned` — atomic counter correctness after 1 and 2 calls
- `record_inference_appends_history_point` — each call adds a timestamped point with cumulative totals
- `record_inference_with_custom_reward` — non-100 reward amounts accumulate correctly
- `save_creates_earnings_json_on_disk` — earnings.json exists and deserializes correctly after record_inference
- `save_creates_history_json_on_disk` — earnings_history.json written with correct point count
- `load_restores_counters_from_disk` — drop + recreate tracker restores count and earned from disk
- `load_restores_history_from_disk` — drop + recreate tracker restores history points from disk
- `load_from_empty_dir_returns_zero` — no panic, returns zeros when no files exist
- `load_from_corrupt_file_returns_zero` — graceful fallback when earnings files contain garbage
- `downsample_noop_when_under_target` — points returned as-is when count < target
- `downsample_reduces_point_count` — 100 points compressed to fewer after downsample
- `downsample_preserves_recent_points` — newest half of points preserved at full resolution
- `history_points_have_increasing_epoch` — epoch_secs monotonically non-decreasing
- `no_tmp_file_left_after_save` — atomic write leaves no .json.tmp residue

### rpc.rs strip_special_tokens — 16 tests
- Individual token removal: `</s>`, `<s>`, `<unk>`, `<pad>`, `[INST]`/`[/INST]`, `<<SYS>>`/`<</SYS>>`, `[SPEAK]`/`[/SPEAK]`
- Real-world: TinyLlama output format, Llama2-chat system prompt format
- Whitespace: collapses multiple spaces to one, trims leading/trailing
- Edge cases: empty string, all-tokens input, clean text passthrough, HTML tags preserved, angle brackets preserved, multiple consecutive occurrences

### consensus.rs strip_special_tokens — 4 tests
- All 10 known tokens stripped in a combined string
- Empty input returns empty
- Clean text passes through unchanged
- Whitespace collapsed and trimmed after stripping

### Infrastructure
- Added `tempfile = "3.10"` as workspace dev-dependency for disk persistence tests
- Added `[dev-dependencies] tempfile.workspace = true` to arc-node Cargo.toml

**Why:** Evolutions 1-4 added ~500 lines of new functionality (earnings tracker, history, downsampling, strip_special_tokens) with zero test coverage. Any future evolution that touches earnings persistence or token stripping could silently break core UX (lost earnings, garbled output) with no safety net. These 36 tests lock down the correctness of all new code paths.

**Verified:**
- `cargo test --lib -p arc-node` — 97 passed, 0 failed (36 new + 61 pre-existing)
- Mac worker: earnings endpoint returns 18,600+ ARC, 186+ inferences, persistence=true
- Mac worker: inference returns clean output ("Hello! It's nice to meet you") — no special tokens
- All 8 seeds upgraded via rolling-deploy.sh
- Binary hash: d757a5ed6ebe0d453953967a2b158633ca47320feb84378b361fe0a6202b8329

**Rollback:** `git checkout evolution-5`

**Files changed:**
- `crates/arc-node/src/earnings.rs` (added #[cfg(test)] mod tests — 16 tests)
- `crates/arc-node/src/rpc.rs` (added #[cfg(test)] mod tests — 16 tests for strip_special_tokens)
- `crates/arc-node/src/consensus.rs` (added 4 strip_special_tokens tests to existing test module)
- `Cargo.toml` (tempfile workspace dependency)
- `crates/arc-node/Cargo.toml` (tempfile dev-dependency)
- `evolution/log.md` (this entry)
---
