# Getting Started with ARC Node

> **Recovery notice (2026-08-26):** v0.8.0 is an unreleased recovery
> candidate; the public v0.7.11 release is desktop-only, and the public seeds
> still run older split/stalled chain state. This guide predates that recovery
> and is not proof that community work or rewards are live. For an SSH/VPS
> node use [`HEADLESS_INSTALL.md`](HEADLESS_INSTALL.md), and do not record an
> earnings walkthrough until
> [`VALIDATOR-FLEET-ROLLOUT.md`](VALIDATOR-FLEET-ROLLOUT.md) is complete.

You downloaded a published ARC desktop package. Now what?

This historical GUI tour explains the controls; it does not promise a first
earned token. Reading time: ~5 minutes. Hands-on time: ~3 minutes.

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

> **Apple Silicon vs Intel?** Apple menu → *About This Mac*. If the chip name
> starts with “Apple,” use the Apple Silicon `.dmg`; if it says “Intel,” use the
> Intel `.dmg`.

### Windows 10 / 11

1. Run the `.exe` you downloaded.
2. Windows SmartScreen may say *"Windows protected your PC"*. That's normal for early releases.
   - Click **More info**.
   - Click **Run anyway**.
3. The installer launches. Click **Next → Install → Finish**.
4. ARC Node opens automatically.

### Linux (Ubuntu / Debian)

After the complete v0.8.0 release is published, use its normalized desktop
asset name:

```bash
sudo apt install ./arc-desktop-linux-x86_64.deb
arc-node-desktop
```

### Linux (Fedora / RHEL)

```bash
sudo rpm -i ./arc-desktop-linux-x86_64.rpm
arc-node-desktop
```

### Linux (any distro, AppImage)

```bash
chmod +x ./arc-desktop-linux-x86_64.AppImage
./arc-desktop-linux-x86_64.AppImage
```

---

## 2. First-launch onboarding (3 clicks)

When ARC Node opens for the first time, you'll see a 3-screen welcome flow.

### Screen 1: Welcome

Brief overview. Click **Continue**.

### Screen 2: Identity

ARC Node generates a fresh **BIP-39 seed phrase** (12 words) and the current
64-hex-character ARC address derived from its Ed25519 public key.

> **Save the seed phrase offline or in a trusted password manager.** Do not take
> a screenshot, paste it into chat, or store it in an unencrypted note. You'll
> need it if you ever reinstall or move to a new machine.

The seed phrase is stored in the native app's private local `store.json` so the
node can sign after restart; it is excluded from frontend `localStorage` and is
never sent to a server by the identity flow. On Unix the directory/file modes
are `0700`/`0600`, but v0.8.0 does not yet use an OS keychain. Save a separate
offline backup: ARC Node has no “forgot password” recovery.

Click **Continue**.

### Screen 3: Configure this node

Choose whether to download a model:

- **Worker candidate**: the node can advertise compute only after the complete
  artifact loads. It is eligible only for requests carrying that exact artifact
  ID. This does not guarantee peers, assignment, verification, or payment.
- **Observer/router**: no local model execution. It can still query the selected
  coordinator and follow whatever chain services the approved rollout enables.

Click **Set up this node**. The app downloads the chosen model and node build,
starts the process, attempts a configured coordinator connection, and requests
testnet faucet credit. Each result is reported separately.

---

## 3. What happens next

After setup:

- The Dashboard distinguishes a running process from peer connectivity and
  selected-host chain health. Do not treat a `live` health string or advancing
  DAG round as proof of block production or cross-seed agreement.
- The **Network** tab attributes every chain number to one pinned host. The
  public seeds are divergent, so it intentionally does not blend them.
- The tray icon (🟣 dot, top-right of your menu bar / system tray) tells you
  the desktop process is present when the window is closed. Use Dashboard health
  and peer fields to check the node process and network state separately.
- If **Start node on app launch** is enabled and the OS login item is actually
  registered, ARC opens and starts the process after login. Settings reports
  the config flag and registration separately.

**Close the window any time.** The node keeps running in the tray. Closing the window does not stop the node. To actually stop, click the tray icon → **Quit**.

---

## 4. Try the features

### Run an AI inference

1. Click the **Inference** tab in the sidebar.
2. Type a prompt. Example: `What is the capital of France?`
3. Click **Run inference**.

The result states which host says it served the request and exactly what
agreement evidence came back. A raw `InferenceAttestation` (`0x16`) is only a
computation claim; even a successful mined `0x16` receipt is not a worker
payment.

4. If the selected path succeeds, the response can include output, input/output
   commitments, route metadata, and a claim transaction hash. Each field is
   reported independently. A missing `tx_hash`, a host-reported agreement
   summary, or an HTTP success must not be upgraded into proof of mining,
   independent signatures, community assignment, or payment.

**How long this took in the historical public-path sample.** The old path was
roughly **10 seconds per token**, so a 16-token response often took **1–3
minutes**. One cold run spent 14.5 s and 16.5 s on two individual ranges. These
are dated observations, not a latency promise for the recovery candidate.
Millisecond figures in the README are single-node local measurements. A fast
repeat can be a cache hit rather than recomputation; inspect the returned
evidence instead of inferring it from timing.

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
3. Read the returned transaction status, then verify a successful mined receipt
   and the balance on the same selected host. A submitted or pending claim is
   not yet a balance change.

The Aug 17 public endpoint reported a **60-second cooldown** and a 10,000 ARC
testnet claim amount. Treat those as host-reported snapshot values and check the
selected host before quoting them:

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
2. Treat only a successful mined `CommunityInferenceReward` (`0x25`) receipt as
   payment. Raw `0x16` rows are shown separately as unpaid inference claims.

The unreleased v0.8.0 candidate configures 2.5 testnet ARC per successful
`0x25` receipt, but issuance also requires exact-artifact work assignment,
authenticated recomputation, a signed worker certificate, active genesis
protocol activation, validator approval collection, strict
greater-than-two-thirds identity and active-stake approval, a funded treasury,
and successful block inclusion. With six equally staked validators, five must
approve. The source candidate implements approval collection, and the
checkpoint-bound recovered genesis schedules activation at block 137146, but
neither is available on the current v2 public fleet. Until the coordinated
cutover proves that path and enables the independent runtime switch, the
candidate fails closed and shows no forward reward projection.

The deployed v2 seeds expose legacy count × constant display arithmetic. That
does not reconcile to payment and must not be described as earnings. The Aug 26
read-only snapshot showed community `total_work_completed: 0` across all
workers and no successful community reward receipt.

### Check the legacy diagnostic dashboard

The old v2 dashboard at <http://140.82.16.112:3200> is a legacy diagnostic
view, not the corrected product surface and not proof that the fleet is one
healthy chain. On Aug 26, all six reachable seeds returned different block
hashes and state roots at the same audited height. Pin one source, show its
version and block age, and stop any reward demo if the common-height fork
warning appears.

### Verify a block, transaction, or reward in the explorer

The corrected static explorer is built but is not a supported public service
until the coordinated v3 cutover passes its live gates. It will remain in
maintenance mode rather than inventing a canonical chain from the current six
forks.

After a proven cutover, **Canonical timeline** retains blocks `0..H` from the
signed checkpoint and continues with the exact parent-linked v3 block at
`H+1`. Search a reward transaction from the same selected host and require a
successful mined `0x25` receipt. Explicit alternate-source views remain
labelled non-canonical and are never added into balances, earnings, or the
canonical history. Until that recovered configuration is published, there is
no supported explorer URL to use in a walkthrough.

---

## 5. Common questions

**Does ARC Node use GPU?**

A GPU is optional. The candidate's blocking cross-architecture known-answer
test covers the CPU I8/I16 whole-model and three-way-shard paths on ARM and x86.
It does not yet prove a GPU backend or production 7B GGUF, so do not claim
cross-GPU bit identity from the current test gate.

**How much will I earn?**

No amount is guaranteed. Testnet ARC has no monetary value. The Earnings tab
shows a forward figure only when the selected coordinator confirms reward
protocol and approval readiness and has enough successful mined `0x25` history
to measure an address-specific rate. Hardware size is not a reward multiplier.

**Will it slow down my computer?**

Measure this on your own machine; the app does not promise a universal CPU or
RAM figure. An observer needs no model. The current full-model worker target is
about 4 GB on disk; use at least 16 GB system RAM for expanded integer weights
plus OS and chain headroom. **Settings → Compute contribution** controls the
worker thread ceiling, not a reward multiplier.

**How do I update?**

When **Check for app updates automatically** is enabled, the candidate checks
shortly after startup and every 24 hours. Background checks do not download or
install anything; the user must confirm installation. The macOS, Windows NSIS,
and Linux AppImage builds consume the signed Tauri
`latest.json` manifest. The updater refuses an artifact whose signature does
not match the public key pinned in `desktop/src-tauri/tauri.conf.json`. Linux
`.deb` and `.rpm` installs remain owned by their package managers, so update
those by downloading and installing the new package; the app must not overwrite
package-managed files. Headless/server updates are separate and documented in
[`HEADLESS_INSTALL.md`](HEADLESS_INSTALL.md).

**How do I uninstall?**

- **macOS**: drag ARC Node from Applications to Trash. Then `~/Library/LaunchAgents/ARC Node.plist` and `~/Library/Application Support/network.arc.desktop/`.
- **Windows**: Settings → Apps → ARC Node → Uninstall.
- **Linux**: `sudo apt remove arc-node-desktop` (or rpm equivalent), then `~/.config/network.arc.desktop/`.

Keep the recovery phrase you saved during setup. The native app retains it in
its private local store so the node can restart, but that local copy is not a
substitute for an offline backup and is removed if you delete the app-data
directory. Do not reset or uninstall until you have verified your backup.

**Where do I report bugs?**

GitHub Issues: <https://github.com/FerrumVir/arc-chain/issues>

---

## 6. Want to go deeper?

- **Run from CLI instead of the desktop app?** Use the current
  [headless/server guide](HEADLESS_INSTALL.md); archived command-line demos are
  not installation instructions.
- **Inspect a past attestation from a reviewed checkout?** Run `ARC_COORDINATOR=http://127.0.0.1:9944 bash scripts/arc-verify.sh --latest` from the repository root against a controlled candidate. Its hash comparison is not payment proof or exact-artifact recomputation unless that coordinator supplies those fields.
- **Read the paper:** *On the Foundations of Trustworthy Artificial Intelligence* (in the repo root).
- **Architecture deep-dive:** [ARCHITECTURE.md](../ARCHITECTURE.md).

Welcome to ARC.
