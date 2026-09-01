# ARC Node - Desktop App

A Tauri 2 desktop app that resolves the exact matching `arc-node` release
binary so a user can run an ARC observer or worker from a `.dmg`, NSIS/MSI
installer, or Linux package without a terminal.

> **Source-freeze release status (2026-08-31; tag-stable):** At this review
> cutoff, the source tree was the unreleased v0.8.0 recovery candidate. Public
> v0.7.11 did not contain this full updater, evidence, or node-download
> behavior, and the public seeds did not run v0.8.0. This is historical status,
> not a live probe. See [`../README.md`](../README.md) and require exact release
> plus rollout evidence before presenting a build as released, deployed,
> synced, or reward-producing.

## What's inside

```
desktop/
├── src/                 React + TypeScript frontend (Vite)
│   ├── screens/         Onboarding, Dashboard, Earnings, Network, Logs, Settings
│   ├── components/      PulseDot, StatusPill, NumberTicker, Card, Titlebar, Sidebar
│   ├── lib/             Typed Tauri IPC wrapper (with browser mock fallback), zustand store, formatters
│   └── styles/          Design tokens, reset, app-level CSS
├── src-tauri/           Rust backend
│   ├── src/
│   │   ├── commands.rs  Tauri #[command] entry points
│   │   ├── node_manager.rs  Spawns & supervises arc-node child process, ring-buffer log capture
│   │   ├── rpc_client.rs    HTTP client against the node's local RPC (127.0.0.1:9944)
│   │   ├── hardware.rs      Platform-specific GPU/RAM/CPU detection
│   │   ├── identity.rs      Generates an on-chain address + recovery phrase
│   │   └── store.rs         Persists identity & config to the OS app-data dir
│   ├── capabilities/    Tauri 2 permission manifest
│   └── tauri.conf.json  Bundle identifier, window, CSP
└── tests/               Playwright browser/evidence/resilience suites
```

## Quick start

```bash
cd desktop
npm ci
npx playwright install chromium
npm run build           # TypeScript + Vite → dist/
npm test                # Playwright (runs against the preview server)
```

## Development

### Frontend only (browser mock)

The `lib/tauri.ts` module auto-detects whether it's inside Tauri. In a browser,
every Tauri command returns synthetic mock data, which lets you iterate on the
UI without an arc-node running.

```bash
npm run dev             # http://localhost:1420
```

### Full desktop app (Tauri dev)

Requires Rust + platform prerequisites (https://tauri.app/start/prerequisites).

```bash
npm run tauri:dev       # Opens the native window, connects to ~/.arc/bin/arc-node
```

The Rust side first honors `ARC_NODE_BIN=/absolute/path/to/arc-node` (the
legacy `ARC_NODE_BINARY` alias remains accepted). Otherwise it reuses an exact
version match or downloads the current platform's node from the exact
`v{desktop-version}` release, verifies `SHA256SUMS`, verifies `--version`, and
installs it at `~/.arc/bin/arc-node` (`arc-node.exe` on Windows).

## Production build

```bash
npm run tauri:build
```

Local outputs land under `src-tauri/target/.../release/bundle/`. The unified
publisher normalizes them to:

- **macOS arm64/x86_64**: `.dmg`; signed `.app.tar.gz` updater payload
- **Windows x86_64**: NSIS `-setup.exe`, `.msi`; signed NSIS updater payload
- **Linux x86_64**: `.AppImage`, `.deb`, `.rpm`; signed AppImage updater payload

Linux ARM64 is headless-only. Tauri updater signatures are required and
cryptographically checked by the release workflow. They are not Apple
notarization or Windows Authenticode signatures: v0.8.0 does not claim either
OS trust signature, so Gatekeeper/SmartScreen approval may still be required.

### Operating-system code signing

The checked-in macOS bundle uses ad-hoc signing (`signingIdentity: "-"`) and
the Windows certificate thumbprint is null. A future OS-signed release must add
owner-controlled Developer ID/notarization and Authenticode configuration to
the protected release environment, then verify the produced signatures before
changing user-facing copy. Do not paste signing secrets into this file or the
repository. Linux packages do not use either OS trust system.

## Testing

```bash
npm test                # all suites, headless
npm run test:ui         # Playwright UI mode (time-travel debugger)
```

At this audited tree state, `npx playwright test --list` enumerates 225 tests in
20 files. Native test inventory is intentionally not hard-coded: run
`cargo test --manifest-path src-tauri/Cargo.toml -- --list`, and treat only a
successful compiling listing as evidence. The suites cover onboarding,
dashboard/evidence semantics, earnings,
inference, updates, peer recovery, persistence, accessibility, resilience,
navigation, wallet behavior, and visual/screenshots contracts.

The tests run against the Vite preview server (`npm run build && npm run preview`) using the same frontend the native app ships - just with mock Tauri IPC. Native integration (child-process spawning, file I/O) is covered by Rust unit tests.

## Architecture

### How the app talks to the node

```
┌────────────────────────┐     invoke()      ┌────────────────────────┐
│   React UI             │──────────────────▶│   Rust #[command]s    │
│   (src/)               │                    │   (src-tauri/)        │
└────────────────────────┘                    └──────────┬────────────┘
                                                         │
                                    spawn + pipe stdout  │
                                                         ▼
                                            ┌────────────────────────┐
                                            │   arc-node child       │
                                            │   (~/.arc/bin/arc-node)│
                                            └──────────┬─────────────┘
                                                       │
                                            HTTP 127.0.0.1:9944
                                                       │
                                                       ▼
                                            ┌────────────────────────┐
                                            │  /health, /worker/…,   │
                                            │  /inference/…          │
                                            └────────────────────────┘
```

The React layer never talks to the node directly. Every bit of state flows
through typed Tauri commands, so mocking is trivial (used for browser dev and
every Playwright test).

### State persistence

- **zustand** + `localStorage` for UI-only state (route, onboarded flag)
- **Rust `store.rs`** writes identity + config to the OS app-data dir
  (for example `~/Library/Application Support/network.arc.desktop/store.json`
  on macOS). The recovery phrase is present in this native store for
  backup/restoration and to
  verify or recreate the persistent node keyfile; the node itself receives
  only that keyfile path. The app-data directory and store are owner-validated
  through open handles (`0700`/`0600` on Unix and a protected DACL on Windows),
  writes are atomic, and symlink/reparse destinations are refused. v0.8.0 does
  not yet use an OS keychain, so users still need a protected offline backup.
- Frontend state is scrubbed of the recovery phrase; an explicit native IPC
  call reveals it only on the backup screen.

### Design tokens

The shared theme, typography, spacing, and animation scales live in
`src/styles/tokens.css`. A small number of diagnostic-state overrides remain
local to screens; use the tokens for new general UI styling.

- Background gradients drift subtly (20s cycle, pauses on reduced-motion)
- Pulse indicators use ring-expansion + fade, not color-blinking
- Number changes animate through `NumberTicker` (requestAnimationFrame + easeOutQuart)
- Route transitions use `framer-motion` with a 220ms custom cubic-bezier

### Accessibility

- `color-scheme: dark` + respected `prefers-reduced-motion`
- All nav items expose `aria-current="page"` on the active route
- Status pills have `role="status"` with proper labels
- Focus rings use a brand-consistent 2px indigo ring, never removed
- Keyboard-reachable: tab through nav, space/enter to activate buttons

## Remaining distribution boundaries

- Apple Developer ID signing/notarization and Windows Authenticode are not
  configured in the v0.8.0 release contract.
- Recovery material is protected as a private local file, not by the native OS
  keychain/credential store.
- The app and earnings UI can report only selected-host and mined-receipt
  evidence. The bundled recovered genesis schedules reward activation at block
  137146, but issuance and earnings still fail closed until the public-v3
  cutover and independent runtime gate are proven live.
- `.deb` and `.rpm` installs remain package-manager updates; only macOS,
  Windows NSIS, and AppImage consume the in-app signed updater payload.
