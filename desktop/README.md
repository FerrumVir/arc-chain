# ARC Node - Desktop App

A Tauri 2 desktop app that wraps the `arc-node` binary so anyone can run an ARC node by double-clicking a `.dmg`, `.msi`, or `.AppImage`. No terminal, no curl-pipe-bash.

Built to remove the single biggest onboarding barrier for "every user is a node": unsigned installers, Windows support, and a clear earnings dashboard.

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
└── tests/               Playwright E2E suite (26 tests)
```

## Quick start

```bash
cd desktop
npm install
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

The Rust side expects the node binary at `~/.arc/bin/arc-node` (matches the
community installer). Override with `ARC_NODE_BINARY=/path/to/arc-node`.

## Production build

```bash
npm run tauri:build
```

Outputs to `src-tauri/target/release/bundle/`:

- **macOS**: `.app`, `.dmg` (ships signed + notarized once identity is configured)
- **Windows**: `.msi`, `.exe` (Authenticode-signed when cert is configured)
- **Linux**: `.AppImage`, `.deb`, `.rpm`

### Code signing - the single biggest UX investment

Without signing, Gatekeeper (macOS) and SmartScreen (Windows) block the installer
by default. This is the reason most "run from source" apps see <10% conversion.

**macOS:**

```bash
# Env vars for tauri.conf.json → bundle.macOS.signingIdentity
export APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)"
export APPLE_ID="your-apple-id@example.com"
export APPLE_PASSWORD="@keychain:AC_PASSWORD"   # app-specific password
export APPLE_TEAM_ID="TEAMID"

npm run tauri:build
# Tauri runs codesign + xcrun notarytool submit + stapler automatically.
```

**Windows:**

```bash
# Put an EV or OV code-signing cert in the Windows cert store and set:
export WINDOWS_CERTIFICATE_THUMBPRINT="<sha1>"
npm run tauri:build
```

**Linux:**

No signing required; the `.AppImage` / `.deb` run as-is.

## Testing

```bash
npm test                # all suites, headless
npm run test:ui         # Playwright UI mode (time-travel debugger)
```

**Coverage**: 26 tests across 6 suites:

- `onboarding.spec.ts` - 4 tests: five-step flow, progress dots, back button, seed-reveal gate
- `dashboard.spec.ts` - 6 tests: shell, earnings ticker, start/stop toggle, stat grid, attestation feed, clipboard
- `navigation.spec.ts` - 6 tests: each of 5 routes + aria-current
- `settings.spec.ts` - 3 tests: save, reset, update check
- `accessibility.spec.ts` - 3 tests: heading structure, keyboard focus, preview badge
- `visual.spec.ts` - 4 tests: gradient logo, gradient earnings text, live pulse animation, active-nav glow

There's also a `screenshots.spec.ts` that captures a 10-screen design gallery into `screenshots/`.

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
  (e.g. `~/Library/Application Support/ARC Node/store.json` on macOS)
- Recovery phrase is **never** stored on disk - user is forced to reveal and save it during onboarding

### Design tokens

Every color, font size, spacing, and animation duration comes from CSS custom
properties in `src/styles/tokens.css`. No hardcoded hex in components.

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

## Next steps (not in this PR)

- **Auto-update** via `tauri-plugin-updater` - wire to the same GitHub release feed the CLI installer uses
- **Tray icon** for quiet background mode (macOS `NSStatusItem`, Windows tray)
- **Model management** screen: list available GGUF models, download progress, per-model earnings
- **Hardware wallet signing** via Tauri's WebUSB bridge (Ledger Nano)
- **Deeplink handler** for `arc://` URIs (opens attestations in the app)
