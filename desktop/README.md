# ARC Node Desktop App

**One-click access to the ARC Chain network.** No terminal, no config files, no technical knowledge required.

## What This App Does

- **Runs a validator node in the background** -- just click "Start Node" and you're participating in the network.
- **Shows real-time stats** -- block height, TPS, connected peers, finality time.
- **Manages staking** -- deposit or withdraw ARC directly from the app.
- **Tracks earnings** -- see your accumulated fee revenue per session.

## Who This Is For

Anyone who wants to join the ARC Chain network. You do not need to know how to use a terminal, compile code, or manage servers. Download the app, open it, and click Start.

## How It Works

Under the hood, the app:

1. Bundles the `arc-node` binary (or builds it on first run if needed).
2. Runs the node as a background process.
3. Communicates with the node over its local HTTP API (`localhost:9090`).
4. Displays network and validator stats in a clean dashboard UI.

## Building From Source

```bash
# Prerequisites:
#   - Rust toolchain (https://rustup.rs)
#   - Node.js >= 18
#   - Tauri CLI: cargo install tauri-cli

./build.sh
```

The built application will be in `src-tauri/target/release/bundle/`.

### Supported Platforms

| Platform | Format        |
|----------|---------------|
| macOS    | .dmg, .app    |
| Linux    | .deb, .AppImage |

Windows support is planned for a future release.

## Status

This is an **MVP scaffold**. It demonstrates the UX vision and app architecture. The goal is to have something people can download, run, and immediately understand what participating in ARC Chain looks and feels like.

Core functionality that is stubbed or pending:

- Automatic `arc-node` binary bundling in release builds
- Keypair generation and management UI
- Staking deposit/withdraw transactions
- Auto-update mechanism
- Windows builds
