# ARC Node - first run

> **Release status:** Use these steps only with a published v0.8 release whose
> assets include the complete normalized set and `SHA256SUMS`. A release being
> published does not by itself mean the public network is serving work.

The release's updater payloads carry Tauri update signatures, but its macOS
package is not Apple Developer ID signed/notarized and its Windows package is
not Authenticode signed. Those are different trust systems. Verify the
download against the exact release's `SHA256SUMS` before bypassing an operating
system warning; never bypass a warning for an unverified or unexpected file.

## macOS - "ARC Node" Not Opened / "the developer cannot be verified"

Verify the DMG against the exact release's `SHA256SUMS` first, then follow the
steps for your macOS version (Apple menu → **About This Mac**).

### macOS 15 Sequoia and later - Open Anyway in System Settings

macOS 15 removed the right-click (Control-click) → **Open** override; that menu
no longer opens an app macOS cannot verify. Approve it in System Settings:

1. Double-click **ARC Node** in Applications. macOS says *"ARC Node" Not
   Opened*: *Apple could not verify "ARC Node" is free of malware…*. Click
   **Done** (not **Move to Trash**).
2. Open Apple menu → **System Settings** → **Privacy & Security**.
3. Scroll down to **Security**. Next to *"ARC Node" was blocked to protect your
   Mac*, click **Open Anyway**. The button appears for about an hour after
   step 1; if it is missing, repeat step 1.
4. Enter your login password (or use Touch ID) when asked.
5. In the dialog that follows, click **Open Anyway** again.
6. macOS remembers this choice; double-clicking works normally from then on.

### macOS 14 Sonoma and earlier - right-click → Open

1. Right-click (or Control-click) **ARC Node** in Applications.
2. Choose **Open** from the menu.
3. The same warning appears, now with an **Open** button. Click it.
4. macOS remembers your choice; double-clicking works normally from then on.

### "ARC Node is damaged and can't be opened"

If the DMG matched `SHA256SUMS`, this is Gatekeeper refusing a quarantined app
it cannot verify, not a broken download, and **Open Anyway** is usually not
offered for it. Strip the quarantine flag from the installed app, then open it
normally:
```
xattr -cr /Applications/ARC\ Node.app
```
Never run this on an app whose download did not match `SHA256SUMS`.

## Windows - SmartScreen "Windows protected your PC"

1. Click **More info** on the warning dialog.
2. Click **Run anyway**.

## Linux - `.AppImage` / `.deb`

No Apple/Windows signing prompt applies. Use the release's normalized
filename; if the downloaded AppImage is not executable:
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
4. **Attempts community-worker registration** (if you picked the Worker role,
   or later turn on **Settings → Contribute compute**). ARC never runs jobs on
   your computer unless you choose one of those, and the same switch turns it
   off. Registration alone does not prove that the worker is eligible,
   reachable, receiving jobs, or earning rewards; those states must be visible
   in the app. The Dashboard's **Jobs on this computer** card shows the jobs
   this computer completed and the ones the network verified since the node
   last started. **Keep this computer awake while a job runs** (Settings) holds
   off idle sleep only while a job is computing.
5. **Submits** a testnet faucet request when onboarding reaches that step. A
   submission is not a balance credit; only a successful mined receipt on the
   selected chain confirms it. The current public fleet is divergent, and the
   checked-in observer genesis does not produce blocks.

## What to do if onboarding fails

- **"Couldn't start arc-node"** with a download error: GitHub releases
  may be rate-limiting. Wait a minute and click Retry.
- **The model download stopped** (Wi-Fi dropped, the laptop slept, or the app
  quit): the downloaded part stays in the `models` folder of the ARC data
  directory, and the next attempt resumes from it instead of starting over.
  Click Retry or reopen the app. The finished file is used only after it
  matches the pinned SHA-256.
- **"Not enough free disk space for the model"**: the model is a 3.80 GB
  download and the app keeps 0.5 GB spare. Free that much space and retry; the
  part already downloaded is kept.
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
