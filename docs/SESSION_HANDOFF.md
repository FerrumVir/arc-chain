# Session Handoff — Tier 1 On-Chain Inference

**Last session:** 2026-05-16 (Windows machine, branch `fix/inference-timeout-and-connect-ux`)
**Next session:** continue on MacBook — clone fork, read this doc, resume.

## TL;DR

**Phase A of Tier 1 on-chain inference is DONE and tested.** ~3,500 LOC across 17 files modified + 4 new files. 13 tests passing across arc-types, arc-state, arc-node. Code compiles clean with MSVC toolchain on Windows.

**Phase B (production deploy to coordinators) is pending.** Code ready, but the 5 alive testnet coordinators (LAX, AMS, LHR, NRT, SGP) still run 0.7.0 which doesn't recognize the new `InferenceRequest` / `InferenceVote` / `InferenceFinalize` TxTypes (`0x22`–`0x24`). Mixed-network testing showed Tier 1 tx never commits to chain because public nodes reject the unknown variants. End-to-end UI verification requires either (a) Phase B deploy to upgrade all 5 coordinators simultaneously, or (b) an isolated local solo-validator chain (we tested that path, candle inference works once `tokio::task::spawn_blocking` was added).

## What's done (Phase A)

### Code

| File | Change | Test status |
|---|---|---|
| `crates/arc-types/src/transaction.rs` | 3 new TxType variants (0x22-0x24), 3 body structs, 6 bounds constants | 7 tests pass |
| `crates/arc-state/src/lib.rs` | 3 apply arms (Request/Vote/Finalize), pending index, snapshot helpers, status bytes | 5 tests pass |
| `crates/arc-state/src/block_stm.rs` | tx access set arms | covered |
| `crates/arc-consensus/src/lib.rs` | cross-shard match arms | covered |
| `crates/arc-node/src/inference_validator.rs` (NEW) | Background task: poll → committee derivation → candle generate (in `spawn_blocking`) → vote/finalize tx submission | covered by E2E |
| `crates/arc-node/src/main.rs` | Spawn validator task at boot | smoke-tested |
| `crates/arc-node/src/rpc.rs` | `/inference/onchain/submit` + `/inference/onchain/result/:id` handlers | smoke-tested |
| `crates/arc-node/src/pipeline.rs` | match arms for new tx types | covered |
| `crates/arc-node/src/lib.rs` | module declaration | covered |
| `crates/arc-node/tests/inference_onchain_e2e.rs` (NEW) | E2E integration test: validator task → state apply → finalize → payout | **1/1 pass** |
| `desktop/src-tauri/src/rpc_client.rs` | `tier1_submit` + `tier1_result` helpers + 3 response structs | manual |
| `desktop/src-tauri/src/commands.rs` | 2 new Tauri commands | manual |
| `desktop/src-tauri/src/lib.rs` | handler registration | manual |
| `desktop/src-tauri/Cargo.toml` | bumped to 0.7.1 (match workspace) | n/a |
| `desktop/src-tauri/rust-toolchain.toml` (NEW) | Pin stable-x86_64-pc-windows-msvc | n/a |
| `desktop/src/lib/types.ts` | TS types: `Tier1Submitted`, `Tier1Vote`, `Tier1Result`, `Tier1Status` | tsc clean |
| `desktop/src/lib/tauri.ts` | `api.tier1Submit` + `api.tier1Result` + live + mock paths | tsc clean |
| `desktop/src/lib/store.ts` | `inferenceMode: "coordinator" \| "onchain"` zustand field + localStorage persist | tsc clean |
| `desktop/src/screens/Settings.tsx` | `InferenceModeToggle` component + Card | tsc clean |
| `desktop/src/screens/Inference.tsx` | `tier1Run` mutation + polling effect + status panel (Open/Voting/Finalized/Refunded) | tsc clean |
| `desktop/src/screens/Dashboard.tsx` | onError handlers, error banner, isStarting state, "Detached" pill | tsc clean |
| `desktop/src/components/Sidebar.tsx` | Status chip "Starting" + "loading model..." subtitle | tsc clean |

### Tests verified passing

```bash
cargo +stable-x86_64-pc-windows-msvc test -p arc-types --lib tier1     # 7 pass
cargo +stable-x86_64-pc-windows-msvc test -p arc-state --lib tier1     # 5 pass
cargo +stable-x86_64-pc-windows-msvc test -p arc-node --features candle --test inference_onchain_e2e   # 1 pass
```

### Documentation

- [`docs/TIER1_ONCHAIN_INFERENCE_PLAN.md`](TIER1_ONCHAIN_INFERENCE_PLAN.md) — implementation plan with file-level breakdown, design decisions, alternatives rejected
- [`docs/TIER1_INFERENCE_UX.md`](TIER1_INFERENCE_UX.md) — user-facing flow walkthrough, UI states, edge cases, GPU optionality

## What's next

### Phase B — production deploy (your call)

Code is ready. Deployment requires SSH access to 5 alive testnet coordinators (LAX, AMS, LHR, NRT, SGP), and a coordinated maintenance window because of mixed-version backward compat (see "Known mixed-network issue" below).

Steps in `docs/TIER1_ONCHAIN_INFERENCE_PLAN.md` §"Phase B — production deployment".

### Phase C — desktop default flip

After 1-2 weeks dual-running (coordinator vs onchain) once Phase B is stable, flip desktop default `inferenceMode` from `coordinator` to `onchain`. Deprecate `/inference/run_consensus` endpoint.

### Polish items deferred

| Item | Priority | Reason deferred |
|---|---|---|
| Disable "Start node" button when external arc-node detected | Medium | User feedback from session — UI lets user click Start even when an external arc-node owns port 9090, silently killing it. Need to disable Start/Restart (keep Stop available) when `status.running == true && status.pid == null`. See `desktop/src/screens/Dashboard.tsx` button group. |
| spawn_blocking inside `compute_output` for candle path | Done in Phase A.3b fix | Originally synchronous, blocked tokio workers. Fixed before E2E test passed. |
| Multi-validator integration test | Low | Single-node E2E + arc-state unit tests cover state machine. Multi-validator needs test harness setup. |
| Real candle path in E2E test | Low | E2E uses stub mode (no model load). Real candle exercised manually in interactive testing. |
| Auto-slash on disagreement | Low | User instruction "follow existing" — current bond + InferenceChallenge mechanic suffices for Phase A. Revisit after Phase C. |

## Known mixed-network issue

Adding new TxType variants without a migration shim means **all validators must upgrade simultaneously**. Testnet coordinators are currently on 0.7.0 and will:

1. Drop `InferenceRequest`/`Vote`/`Finalize` txs from mempool (bincode deserialize fails on unknown variant)
2. If a 0.7.1 validator proposes a block including these, 0.7.0 validators fail to validate the block → no quorum → chain stall or fork

Until Phase B coordinated upgrade lands, Tier 1 mode in the desktop will only work against an isolated single-validator local chain (which we built and tested manually — script and genesis files were wiped at session end).

## How to resume on MacBook

### Prerequisites on Mac

- Xcode Command Line Tools (`xcode-select --install`)
- Rust via rustup (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- Node.js 20+ via nvm or fnm
- Tauri prerequisites (`brew install pkg-config`)

### Setup

```bash
# 1. Clone your fork
git clone https://github.com/arisarcmarket/arc-chain.git
cd arc-chain
git checkout fix/inference-timeout-and-connect-ux

# 2. Build arc-node with Tier 1 + candle
cargo build -p arc-node --features candle
# MSVC toolchain pin in desktop/src-tauri/rust-toolchain.toml is Windows-specific.
# On Mac it falls back to the default toolchain (stable-aarch64-apple-darwin
# or stable-x86_64-apple-darwin). Should compile clean.

# 3. Copy binary to canonical path
mkdir -p ~/.arc/bin ~/.arc/models
cp target/debug/arc-node ~/.arc/bin/arc-node
~/.arc/bin/arc-node --version   # should print: arc-node 0.7.1

# 4. Download TinyLlama model
curl -L \
  "https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf" \
  -o ~/.arc/models/tiny.gguf

# 5. Install desktop deps + start Tauri dev
cd desktop
npm install
npm run tauri:dev
```

Once Tauri opens, onboarding flows from step 1 (no existing state to load). Generate a fresh identity, save the seed phrase securely, complete onboarding, click Start node in Dashboard, wait for model load.

### Resume the conversation with Claude

If using Claude Code on MacBook:

1. Open the cloned repo (`cd arc-chain && claude`)
2. Reference this doc: "read docs/SESSION_HANDOFF.md and continue Phase B planning" — or whatever your next intent is
3. Memory files are local to each device (Claude Code stores them in `~/.claude/projects/.../memory/`). The one feedback memory from the prior session was: *user prefers disabled UI controls (with explanatory tooltip) over clickable controls that lead to wrong state*. This doc captures everything else worth carrying over.

## Running an isolated local solo-validator chain (if you need to demo Tier 1 end-to-end)

The script + genesis file from the prior session were wiped. To recreate:

```bash
# Generate the local genesis (replace the seed with your actual BIP39 phrase)
SEED="<your 12-word BIP39 phrase>"
ADDRESS="$(echo -n "$SEED" | <derive address>)"  # see desktop/src-tauri/src/identity.rs:38-55
cat > ~/.arc/genesis-local.toml <<EOF
[chain]
name = "arc-local-tier1"
chain_id = "0x415243"

[[accounts]]
address = "$ADDRESS"
balance = 1_000_000_000

[[validators]]
seed = "$SEED"
stake = 5_000_000
EOF

# Spawn solo arc-node in a dedicated terminal (replaces Tauri's spawn)
~/.arc/bin/arc-node \
  --rpc 127.0.0.1:9090 \
  --p2p-port 9091 \
  --data-dir ~/.arc/data-local \
  --model ~/.arc/models/tiny.gguf \
  --genesis ~/.arc/genesis-local.toml \
  --validator-seed "$SEED" \
  --eth-rpc-port 0
```

In Tauri: Settings → On-chain (Tier 1) → Inference tab → Submit on-chain → ~15-30 sec to Finalized status with coherent TinyLlama output.

**Caveat**: Tauri's "Start node" button in Dashboard will kill this manual spawn and respawn a testnet-joined arc-node. Don't click it while the manual instance is running. The "disable Start button when external arc-node detected" UX fix listed under Polish above would prevent this.
