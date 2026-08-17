# Getting Started with ARC Node

You downloaded ARC Node. Now what?

This guide walks you from a fresh install through your first earned ARC token. Reading time: ~5 minutes. Hands-on time: ~3 minutes.

---

## 1. Install

### macOS

1. Open the `.dmg` you downloaded.
2. Drag **ARC Node** into your Applications folder.
3. Eject the `.dmg`.
4. Open **Applications → ARC Node** for the first time.
5. macOS will say *"ARC Node can't be opened because it is from an unidentified developer."* That's normal for early releases.
   - **Right-click** ARC Node in Applications.
   - Choose **Open** from the context menu.
   - Click **Open** in the dialog that appears.
   - You only do this once. After the first launch, double-clicking works normally.

> **Got *"ARC Node is damaged and can't be opened. You should move it to the Trash"* instead?**
> That's the same Gatekeeper rejection in disguise — it isn't actually damaged. macOS just refuses to verify a signature that doesn't exist on early builds. Two fixes:
>
> - **Easy:** open Terminal and run `xattr -cr "/Applications/ARC Node.app"`, then double-click ARC Node again. The command strips the quarantine flag macOS adds to anything downloaded from a browser.
> - **Or:** delete the .app, re-download the .dmg, and use the right-click → Open flow above *before* double-clicking.
>
> Permanent fix is on the roadmap — once the project is signed + notarized with an Apple Developer ID, this dialog goes away for everyone.

> **Apple Silicon vs Intel?** Apple menu → *About This Mac*. If you see "Apple M1/M2/M3/M4" → use the Apple Silicon `.dmg`. If "Intel" → use the Intel `.dmg`.

### Windows 10 / 11

1. Run the `.exe` you downloaded.
2. Windows SmartScreen may say *"Windows protected your PC"*. That's normal for early releases.
   - Click **More info**.
   - Click **Run anyway**.
3. The installer launches. Click **Next → Install → Finish**.
4. ARC Node opens automatically.

### Linux (Ubuntu / Debian)

```bash
sudo apt install ./ARC.Node_0.7.11_amd64.deb
arc-node-desktop
```

### Linux (Fedora / RHEL)

```bash
sudo rpm -i ARC.Node-0.7.11-1.x86_64.rpm
arc-node-desktop
```

### Linux (any distro, AppImage)

```bash
chmod +x ARC.Node_0.7.11_amd64.AppImage
./ARC.Node_0.7.11_amd64.AppImage
```

---

## 2. First-launch onboarding (3 clicks)

When ARC Node opens for the first time, you'll see a 3-screen welcome flow.

### Screen 1: Welcome

Brief overview. Click **Continue**.

### Screen 2: Identity

ARC Node generates a fresh **BIP39 seed phrase** (12 words) and an **ARC address** (`arc1q...`).

> **Save the seed phrase.** It's the only thing that proves the address is yours. Take a screenshot or copy it into a password manager. You'll need it if you ever reinstall or move to a new machine.

The seed phrase is stored locally on your machine. It is never sent to any server. ARC Node has no "forgot password" recovery — the seed phrase IS the recovery.

Click **Continue**.

### Screen 3: Join the network

Pick your role:

- **Worker** (default, recommended): your machine serves AI inference and earns ARC.
- **Observer**: read-only, no earnings. For people who want to verify the chain without participating.

Click **Start node**.

---

## 3. What happens next

Within ~30 seconds:

- The bottom-left status pill turns from `connecting` → `syncing` → `live`.
- The **Network** tab shows the 6 testnet seeds you've connected to.
- The tray icon (🟣 dot, top-right of your menu bar / system tray) tells you the node is alive even when the window is closed.
- ARC Node registers a launchd (macOS) / systemd user service (Linux) / startup task (Windows) so the node starts automatically next time you log in.

**Close the window any time.** The node keeps running in the tray. Closing the window does not stop the node. To actually stop, click the tray icon → **Quit**.

---

## 4. Try the features

### Run an AI inference

1. Click the **Inference** tab in the sidebar.
2. Type a prompt. Example: `What is the capital of France?`
3. Click **Run inference**.
4. The network will:
   - Route your prompt through the live shard pipeline (Llama-2-7B split into 6 layer ranges across the 6 seeds, each range replicated 3×).
   - Run it through the pure-integer engine.
   - Return the output, the BLAKE3 input/output hashes, and an attestation `tx_hash`.

**How long this takes, honestly.** Roughly **10 seconds per token** on the
public testnet, so the default 16-token response takes **1–3 minutes**. A first
request against a shard that has not served recently is slower still — a
measured cold run spent 14.5 s and 16.5 s on two individual layer ranges. The
millisecond figures in the README are single-node local measurements, not
network ones. Repeating the *exact* same prompt returns in microseconds because
it is served from a result cache, not recomputed.

**About that `tx_hash`.** It is real, and the attestation genuinely enters the
mempool — but four of the six seeds have not sealed a block in about six days,
so it will most likely **not be mined**. Looking it up returns
`block_height: null`. That is the current state of the testnet, not a bug in
your node.

**Prompt quality.** The INT16 engine on this build degrades badly on some
prompts. Prompts phrased as `Explain <topic>` reliably return newline spam.
`What is …?` and `How does … work?` phrasings behave. See
`INFERENCE_DETERMINISM.md` for the underlying quantization defect.

### Claim from the testnet faucet

1. Click the **Wallet** tab.
2. Click **Claim from faucet** (top right).
3. Within ~10 seconds, your balance will show **10,000 testnet ARC**.

There is a **60-second cooldown** per address. Check the live figures yourself:

```bash
curl http://140.82.16.112:9090/faucet/status
# {"claim_amount":10000,"rate_limit_secs":60,...}
```

This is testnet ARC — no real-world value.

> **Read your balance from one seed only.** The six seeds are independent
> chains today, not replicas. A faucet credit on one seed does not appear on
> another, so a balance that "disappears" is almost always a different seed
> answering, not a lost transaction.

### See your earnings

1. Click the **Earnings** tab.
2. Attestations your node helped serve appear here, valued at 2.5 ARC each.

**What that number is.** It is display arithmetic — an attestation count
multiplied by a 2.5 ARC constant. It does not read an on-chain balance and will
not reconcile against your wallet balance. It is also computed from an
in-memory transaction map that gets pruned, so the lifetime figure can go
*down* between refreshes and resets to zero if the node restarts. Attestation
rewards are not actually paid out on the current chain.

Expect this tab to read **0.0 ARC** today: `/worker/earnings` returns
`total_attestations: 0` for every address on every seed, because attestations
are not being mined (see above).

If you're a fresh worker node, you may not see earnings for the first few minutes — the network has to assign you a shard range and announce you to the coordinators. After that, attestations roll in passively.

### Check the dashboard

The 6 testnet seeds publish a live network dashboard at <http://140.82.16.112:3200>. You can paste your ARC address into it to see your node's view of the world from outside.

---

## 5. Common questions

**Does ARC Node use GPU?**

If you have a Metal-capable Mac (M1/M2/M3/M4) or a CUDA GPU on Linux/Windows, yes. Otherwise CPU. The integer-fixed-point engine is deterministic across both — same prompt → identical output hash regardless of hardware.

**How much will I earn?**

On testnet: nothing real. Testnet ARC is a placeholder for measuring throughput and proving the economic model. Mainnet earnings depend on demand, your hardware, and your chosen shard range. The Earnings tab shows you the testnet-equivalent rate.

**Will it slow down my computer?**

The node runs at ~5-15% CPU when idle, spiking briefly during inference. RAM usage is ~3-4 GB (model weights). Storage: ~5 GB total. If you want to throttle, **Settings → Resource limits**.

**How do I update?**

Auto. When a version newer than `v0.7.11` ships, ARC Node downloads the signed update and applies it on next launch. You'll see a small badge in the status pill. No action needed. The signing key is pinned to the GitHub release pipeline — you can verify by reading `desktop/src-tauri/tauri.conf.json` in the repo.

**How do I uninstall?**

- **macOS**: drag ARC Node from Applications to Trash. Then `~/Library/LaunchAgents/ARC Node.plist` and `~/Library/Application Support/network.arc.desktop/`.
- **Windows**: Settings → Apps → ARC Node → Uninstall.
- **Linux**: `sudo apt remove arc-node-desktop` (or rpm equivalent), then `~/.config/network.arc.desktop/`.

The seed phrase lives in your config dir, so deleting it without backup means losing the address.

**Where do I report bugs?**

GitHub Issues: <https://github.com/FerrumVir/arc-chain/issues>

---

## 6. Want to go deeper?

- **Run from CLI instead of the desktop app?** See [README.md](../README.md) "Or run from the command line".
- **Verify a past attestation from scratch?** `curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-verify.sh | bash -s -- --latest`
- **Read the paper:** *On the Foundations of Trustworthy Artificial Intelligence* (in the repo root).
- **Architecture deep-dive:** [ARCHITECTURE.md](../ARCHITECTURE.md).

Welcome to ARC.
