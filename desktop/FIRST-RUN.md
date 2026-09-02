# ARC Node - first run

> **Release status:** v0.8.0 is an unreleased recovery candidate and is not
> deployed to the public seeds. Use these steps only after the exact v0.8.0
> release contains the complete normalized asset set and `SHA256SUMS`.

The candidate's updater payloads carry Tauri update signatures, but its macOS
package is not Apple Developer ID signed/notarized and its Windows package is
not Authenticode signed. Those are different trust systems. Verify the
download against the exact release's `SHA256SUMS` before bypassing an operating
system warning; never bypass a warning for an unverified or unexpected file.

## macOS - "ARC Node cannot be opened because the developer cannot be verified"

Pick one of these, top-recommended first.

### Option 1 - Right-click → Open (30 seconds)
1. Right-click (or control-click) `ARC Node.app` in Applications.
2. Choose **Open** from the menu.
3. The same warning appears but with an **Open** button. Click it.
4. macOS remembers your choice; normal double-click works from then on.

### Option 2 - Terminal (one command)
If the right-click path doesn't give you an Open button on your macOS
version, first verify the downloaded DMG against `SHA256SUMS`, then strip the
quarantine flag from the installed app:
```
xattr -cr /Applications/ARC\ Node.app
```

## Windows - SmartScreen "Windows protected your PC"

1. Click **More info** on the warning dialog.
2. Click **Run anyway**.

## Linux - `.AppImage` / `.deb`

No Apple/Windows signing prompt applies. Use the normalized v0.8.0 filename;
if the downloaded AppImage is not executable:
```
chmod +x arc-desktop-linux-x86_64.AppImage
```

## What the app does on first launch

1. **Resolves** the `arc-node` binary from the exact release matching the
   desktop version and platform, verifies its `SHA256SUMS` entry and reported
   version, and fails closed instead of running a stale mismatched node.
2. **Generates** a fresh BIP-39 12-word recovery phrase and derives your
   on-chain address from it. The phrase is shown on the Identity step of
   onboarding - **save it somewhere safe**.
3. **Starts** arc-node, pointed at the 6 testnet seeds bundled with the
   app, using an app-owned private Ed25519 keyfile derived once from your
   recovery phrase. The keyfile preserves the address you just saw; the app
   never places the phrase or secret key in node arguments, environment, or
   logs and reuses the same protected keyfile across restarts.
4. **Attempts community-worker registration** (if you picked the Worker role).
   Registration alone does not prove that the worker is eligible, reachable,
   receiving jobs, or earning rewards; those states must be visible in the app.
5. **Submits** a testnet faucet request when onboarding reaches that step. A
   submission is not a balance credit; only a successful mined receipt on the
   selected chain confirms it. The current public fleet is divergent, and the
   checked-in observer genesis does not produce blocks.

## What to do if onboarding fails

- **"Couldn't start arc-node"** with a download error: GitHub releases
  may be rate-limiting. Wait a minute and click Retry.
- **"port 9090 busy"**: another arc-node (or Jupyter) is using the port.
  The app will auto-fall back to 9100, 9110, ...; the warning in the logs
  tells you which port it ended up on.
- **"no identity"**: you hit Launch without completing the Identity step.
  Restart onboarding.

If nothing works, open an issue at
[github.com/FerrumVir/arc-chain/issues](https://github.com/FerrumVir/arc-chain/issues)
and paste the log output from the `Logs` screen.

## Where your data lives

- macOS: `~/Library/Application Support/network.arc.desktop/store.json`
  (identity + config) and `~/.arc/data-v3/` (current arc-node WAL + state)
- Linux: `~/.local/share/network.arc.desktop/` and `~/.arc/data-v3/`
- Windows: `%APPDATA%\network.arc.desktop\` and
  `%USERPROFILE%\.arc\data-v3\`

Before the first v0.8 launch, fully quit the v0.7 desktop/node and its updater;
also stop any separately launched v0.7 `arc-node`. The v0.8
`.arc-node.lock` is a same-generation guard, and released v0.7 binaries do not
acquire it. A fresh path prevents WAL reuse but does not make overlapping old
and new processes safe.

When v0.8 first opens a v0.7 store that points at an unbound WAL in `~/.arc`,
it leaves those old block/WAL bytes untouched, preserves identity and model
selection, switches only the persisted data-directory pointer to a fresh
`~/.arc/data-v3*` child, and shows both paths in a dismissible migration notice.

Deleting the app-data directory removes the locally stored recovery phrase.
Deleting the ARC data directory removes local chain state. Back up the phrase
offline and stop the node before deleting either directory.
