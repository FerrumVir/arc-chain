# ARC Node - first run

This binary is unsigned - the testnet release skips Apple / Windows code
signing. Your operating system will warn you on first launch. The app is
fine to run; you just need to tell the OS that once.

## macOS - "ARC Node cannot be opened because the developer cannot be verified"

Pick one of these, top-recommended first.

### Option 1 - Right-click → Open (30 seconds)
1. Right-click (or control-click) `ARC Node.app` in Applications.
2. Choose **Open** from the menu.
3. The same warning appears but with an **Open** button. Click it.
4. macOS remembers your choice; normal double-click works from then on.

### Option 2 - Terminal (one command)
If the right-click path doesn't give you an Open button on your macOS
version, strip the quarantine flag:
```
xattr -cr /Applications/ARC\ Node.app
```

## Windows - SmartScreen "Windows protected your PC"

1. Click **More info** on the warning dialog.
2. Click **Run anyway**.

## Linux - `.AppImage` / `.deb`

No signing-related prompts. If the downloaded file isn't executable:
```
chmod +x arc-node-desktop.AppImage
```

## What the app does on first launch

1. **Downloads** the `arc-node` binary (~45 MB) from
   [github.com/FerrumVir/arc-chain/releases](https://github.com/FerrumVir/arc-chain/releases).
2. **Generates** a fresh BIP-39 12-word recovery phrase and derives your
   on-chain address from it. The phrase is shown on the Identity step of
   onboarding - **save it somewhere safe**.
3. **Starts** arc-node, pointed at the 6 testnet seeds bundled with the
   app, using your recovery phrase as the validator seed so your node's
   address matches the one you just saw.
4. **Registers as a community worker** (if you picked the Worker role)
   so your compute contributes to the network.
5. **Claims testnet faucet tokens** after your first heartbeat - you get
   testnet ARC for trying the network out.

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
  (identity + config) and `~/.arc/` (arc-node WAL + state)
- Linux: `~/.local/share/network.arc.desktop/` and `~/.arc/`
- Windows: `%APPDATA%\network.arc.desktop\` and `%USERPROFILE%\.arc\`

Delete either directory to start fresh.
