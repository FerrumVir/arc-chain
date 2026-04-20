# Production readiness — honest gaps

Status: **closed-alpha ready** (v0.2 pass addressed #4, #6, #7, #8, #13).
Public beta still gated on code-signing + key derivation.

This document lists everything that stands between "works for TJ on this Mac"
and "TJ's mom installs it from arcnetwork.ai and starts earning".

---

## ✅ Resolved in the alpha-hardening pass

| Gap | Fix |
|---|---|
| #4 Mock-strip | `vite.config.ts` now sets `__ARC_PROD_TAURI__=true` only inside production Tauri bundles. `mockInvoke()` refuses to run when the bundle is served outside Tauri (DevTools attack / static-host leak). |
| #6 Crash recovery | `NodeManager` gained a `try_reap_if_crashed()` path called on every `node_status` poll. When our owned child exits without `stop()` having been called, a `CrashInfo` is recorded and surfaced as `status.lastError`. Dashboard shows a `CrashBanner` with Relaunch + Dismiss. |
| #7 Port conflicts | `choose_port_pair()` probes the preferred RPC/P2P ports and falls back in +10 increments up to 5 tries. The chosen port is logged and recorded — `node_status.rpc_port` reflects reality, not the config. |
| #8 Error boundary | `src/components/ErrorBoundary.tsx` wraps the app. Render exceptions show a branded recovery screen with "Restart view" (reset boundary) and "Reload app" (window.reload). |
| #13 Auto-updater | `tauri-plugin-updater` wired into the Rust Builder. `tauri.conf.json` gains a `plugins.updater` block pointing at `latest.json` on GitHub releases. **TODO**: generate signing keypair + paste pubkey before first release. |

---

## 🚫 Blockers — cannot ship to public without these

### 1. Not code-signed or notarized

- **macOS**: Gatekeeper will block the installer by default. "app can't be opened because it is from an unidentified developer" → >90% bounce rate.
  - Need: Apple Developer ID ($99/yr), `codesign` + `xcrun notarytool` wired into `tauri build`. Stub exists in `src-tauri/tauri.conf.json` (`bundle.macOS.signingIdentity: null`).
- **Windows**: SmartScreen warning → similar bounce. Need EV code-signing cert (~$400/yr).
- **Linux**: `.AppImage` / `.deb` don't require signing.

### 2. Identity / key derivation is not real

**File**: `src-tauri/src/identity.rs`

- Seed phrase uses a **36-word custom wordlist**, not BIP-39 (2048 words). Current entropy ≈ 62 bits — an attacker can brute-force it.
- Address bytes come from `OsRng` directly, with **no derivation from the seed phrase**. Restoring from the phrase on another device would produce a different address — i.e. the phrase is a lie.
- Needs: `bip39` crate for phrase, `hmac-sha512` + `slip10` for derivation, `ed25519-dalek` for keygen, `BLAKE3(pk)` for address.

### 3. Private keys stored in plain JSON

**File**: `src-tauri/src/store.rs`

- Identity, seed phrase, and (eventually) private key all land in `~/Library/Application Support/network.ARC.ARC-Node/store.json` as JSON.
- Any malicious process with read access to `$HOME` can steal the seed.
- Needs: macOS Keychain (`security-framework` crate), Windows Credential Manager, libsecret on Linux. Seed phrase should never touch disk — only the derived pubkey/address.

### ~~4. Mock IPC layer ships in production builds~~ → RESOLVED

### 5. Sending ARC is not implemented

**File**: `src/screens/Wallet.tsx`

- UI shows "Send — coming in v0.2". Truthful but a showstopper.
- Blocker: needs real ed25519 signing (#2) first.

---

## ⚠️ Stability — will bite within a week of public use

### ~~6. Node-crash recovery~~ → RESOLVED

### ~~7. Port conflicts~~ → RESOLVED

### ~~8. No error boundary in React~~ → RESOLVED

### 9. Tauri webview security relies on dev-only CSP

- `tauri.conf.json` CSP allows `connect-src: http://127.0.0.1:*` — fine for a local node, but also allows `http://localhost:*`. No TLS. Fine on localhost, but if we ever expose RPC on the LAN it's a footgun.

---

## 📉 Quality — will embarrass us

### 10. No Rust unit tests

- All 48 tests are Playwright UI tests. Rust commands (`node_manager`, `rpc_client`, `identity`) have zero coverage.
- Needs: at minimum, shape-adapter tests for `rpc_client.rs` (snapshot real node responses, assert we parse them correctly).

### 11. No CI

- Tests pass on TJ's Mac. They will break on another architecture and we won't know.
- Needs: GitHub Actions matrix {macOS-arm64, macOS-x86_64, ubuntu-22.04, windows-latest} running `npm ci && npm run build && npx playwright test`.

### 12. Dev-mode detritus in screenshots + copy

- The titlebar shows "Preview mode" when running in browser — leak path. Remove in prod builds.
- Example prompts in Inference screen reference arc-chain jargon the user may not know.

### ~~13. No auto-updater wired~~ → PARTIALLY RESOLVED

- Plugin wired, endpoint pointed at `github.com/FerrumVir/arc-chain/releases/latest/download/latest.json`.
- **TODO before first release**: `npx @tauri-apps/cli signer generate -w ~/.tauri/arc-desktop-key`, paste the public key into `tauri.conf.json > plugins.updater.pubkey`, and commit signed update artifacts with each release (`tauri build` respects `createUpdaterArtifacts: true`).

### 14. Accessibility gaps

- Color-only status signaling: live/syncing/offline are green/yellow/red pulse dots. Colorblind users get no secondary cue beyond the text label. Acceptable, but the pulse-dot could use a shape variant.
- Info popovers don't trap focus. Tab escapes them before Esc is tried.

### 15. No analytics or crash reporting

- Zero visibility into what users actually do. First few thousand users will give us free telemetry if we ask — need Sentry (or self-hosted) + a minimal opt-in prompt.

---

## 🧪 Technical honesty — the attestation story

This app previously called attestations "cryptographic receipts of off-chain compute". After auditing the chain (`distributed.rs`, `committee.rs`, `lib.rs:3782–3875`), the correct story — now in the popover — is:

- **Inference is executed by the network, pipeline-parallel across nodes.** Each node holds a range of transformer layers and forwards hidden state downstream. No single node re-runs the whole model. This IS on-chain inference in the consensus-participating sense.
- **The attestation commits the result to consensus.** `InferenceAttestation` tx carries `(input_hash, output_hash, model_hash)` + a 1,000 ARC bond. Included in the DAG block like any other tx.
- **Verification is challenge-response, not every-validator re-execution.** A 7-validator VRF committee re-runs; ≥5/7 agreement finalizes. Disputes trigger on-chain re-execution via precompile 0x0A.
- **Determinism is claimed, not independently audited.** INT16 fixed-point, pure integer math, BLAKE3 on activations. The chain's own tests pass, but we have no third-party attestation that hashes match bit-for-bit across x86/ARM/GPU under all loads.

Mainnet should require a cross-platform determinism audit and a published reproduction kit.

---

## ✅ What's actually done right

- Typed IPC layer (Rust `#[command]` ↔ TS `api` object) with matching mocks — easy to test
- All 48 tests pass (44 mock + 4 live against real testnet node on `127.0.0.1:9090`)
- External-node detection: the app recognizes when a node is managed by launchd/systemd and shows "External · read-only" instead of a dead Stop button
- Brand tokens live in `src/styles/tokens.css` — when the signed pack drops, 12 vars change, everything re-colors
- Icon generator (`src-tauri/icons/gen_icons.py`) is deterministic and dependency-free, so CI can regenerate icons on any platform
- Rust side cleanly compiled under `cargo check` on the first attempt after every iteration

---

## Target milestones

| Milestone | Gates | Status |
|---|---|---|
| **Closed alpha** (10 node operators) | #4 mock-strip, #6 crash recovery, #7 port conflicts, #8 error boundary | ✅ **READY** |
| **Public testnet beta** (~1k users) | #1 signing + notarization, #13 updater pubkey | 2 weeks incl. Apple enrolment |
| **Mainnet launch** | #2 real BIP-39 + key derivation, #3 keychain, #5 send flow, cross-platform determinism audit | 6–8 weeks |

The UI is genuinely ready. Crash handling is now honest. Cryptography and
signed delivery are still the remaining work.
