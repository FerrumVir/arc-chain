# Legacy v0.7 bridge

Roughly 130 ARC installs still run v0.7.x. They update only from GitHub's
global "Latest" release, which is still the desktop-only v0.7.11, and v0.8
releases are deliberately never "Latest". These installs are stranded: the
headless ones fail their daily update with a 404, and v0.7.10/v0.7.11 desktops
cannot even start their node.

The legacy bridge is a v0.7.12 release built for exactly those updaters. Once
the owner marks it "Latest", every v0.7 install moves itself onto the current
network. Each install runs the pinned, signature-verified v0.8 node at stake
0, in a fresh data directory, with compute off until its owner opts in. The
v0.7 data stays where it is, unchanged.

Nothing here publishes anything. Publication is a separate owner action, in
stages, described in [Rollout](#rollout).

Why it matters: these returning machines are the start of the community
test lab and verifier fleet that ARC's verified-inference plan relies on.
Once their owners consent, they hold the pinned model, take community jobs,
and re-check answers through the twin execution in PR #139. Proof Kit
fingerprint reports can be added later through the same consent. The bridge
holds four rules: a fresh data directory, stake 0, compute off until
consent, and no publication without TJ's go.

## What released v0.7 updaters actually do

All citations are to tag `v0.7.11` unless marked otherwise.

| Updater | When it runs | What it fetches | Verification | What it runs afterwards |
|---|---|---|---|---|
| Desktop app update (Tauri) | Only when the user clicks Settings > Check for updates, then Install (`desktop/src/screens/Settings.tsx:22-52`). `config.autoUpdate` is stored but never acted on. | `releases/latest/download/latest.json` (`desktop/src-tauri/tauri.conf.json:58-66`) and the bundle URL it names | minisign signature of the bundle, key `9A970CAACE56473B` (same key in every desktop from v0.5.4 through v0.7.11 and in v0.8.10) | Replaces the app (`.app.tar.gz`, NSIS `-setup.exe`, AppImage) and relaunches |
| Desktop node binary (`ensure_binary`) | **Automatically, before every node start and restart** (`desktop/src-tauri/src/commands.rs:110`, `:140`), in every desktop from v0.6.0 on | `releases/latest/download/arc-node-{macos-arm64,macos-x86_64,windows-x86_64.exe,linux-x86_64}` whenever `~/.arc/bin/arc-node --version`'s second token differs from the app version (`commands.rs:974-1075`, `:1097-1128`) | None | Spawns it with `--rpc 127.0.0.1:<p> --p2p-port <q> --data-dir ~/.arc`, the wallet's recovery phrase as its identity seed argument, and `--eth-rpc-port 0 --seeds-file … --genesis … [--community-mode] [--model …]`. It never passes `--stake` (`desktop/src-tauri/src/node_manager.rs:126-211`) |
| Headless daily updater (`arc-auto-update.sh`, written by `scripts/install-community-node.sh:218-283`) | Daily at 04:17 local: launchd `StartCalendarInterval` (`:362-366`) or the systemd timer, which has `Persistent=true` (`:436-446`) | `api.github.com/repos/FerrumVir/arc-chain/releases/latest`, `tag_name` taken with `grep -m1 '"tag_name"' \| sed` that requires a bare `vX.Y.Z` (`:237-239`), then `releases/download/v<X.Y.Z>/arc-node-{macos-arm64,macos-x86_64,linux-x86_64,linux-aarch64}` (`:247`) | None. Any tag different from `version.txt` is installed (`:241-245`) | Copies the old binary to `arc-node.prev`, moves the new one into `bin/arc-node`, writes `version.txt`, restarts the service (`:249-261`), then rolls back if neither `localhost:9944/health` nor `:9090/health` answers within 30 s (`:262-281`). The service runs `--rpc 0.0.0.0:9944 --p2p-port 9945 --seeds-file … --genesis …`, the installer's seed as the identity seed argument, and `--stake 0 --min-stake 0 --eth-rpc-port 0 --data-dir ~/.arc/data [--model …] --community-mode` (`:306-341`, `:389-421`) |
| Older daemon (`scripts/auto-update.sh`) | Every 600 s while someone keeps it running (`:20`) | Same `releases/latest` parse; `releases/download/v<ver>/arc-node-<platform>` (`:73-101`) when the version is newer (`:180`) | None | Kills the matching `arc-node` process and restarts the new binary with the same arguments (`:103-157`) |

What a "Latest" release must therefore contain:

- a tag and release name of exactly `vX.Y.Z` (the headless `sed` parse);
- `arc-node-macos-arm64`, `arc-node-macos-x86_64`, `arc-node-linux-x86_64`,
  `arc-node-linux-aarch64` and `arc-node-windows-x86_64.exe`, all executables
  that are safe to start with the v0.7 command lines above;
- `latest.json` whose `version` is newer than the app and whose four platform
  bundles carry minisign signatures by key `9A970CAACE56473B`.

Three facts make "just mark v0.8 Latest" unsafe:

1. v0.8.10 ships assets under the same names as v0.7 (`arc-node-macos-arm64`
   and so on) plus a `latest.json` signed with the same key. Marking it Latest
   would hot-swap a raw v0.8 binary into every v0.7 supervisor, which then
   starts it with the v0.7 arguments and the v0.7 data directory. v0.8.10
   refuses a seed-derived identity outside its insecure development mode
   (`crates/arc-node/src/main.rs:5908` at `v0.8.10`). So those nodes would
   crash-loop and roll back daily rather than join anything.
2. The v0.7 desktop never passes `--stake`, and v0.7.11's default stake was
   5,000,000 (`crates/arc-node/src/main.rs:29-31`). v0.7 desktops therefore
   ran as staked validators. Nothing about their old invocation may carry
   over.
3. `release.yml:1616` and `:1987-1993` keep every v0.8 release
   `make_latest=false` on purpose. Every v0.8 consumer ignores "Latest": the
   desktop's `updater_channel.rs` selects the newest immutable bot-published
   release of v0.8.0 or later, and `install.sh` resolves exact tags.

### Why the existing legacy path does not help these installs

`install.sh` can adopt a recognized v0.7 layout (`docs/HEADLESS_INSTALL.md`,
"one narrow bridge for the affected community"), but only when an operator
runs it by hand, with sudo on Linux. It also needs a `cutover-v1` release that
carries the signed cutover boundary, checkpoint descriptor and policy assets.
No published release carries them. Every update-profile release (v0.8.6 and
later) deliberately refuses legacy migration (`install.sh:3127`). The
post-release acceptance proves that refusal with real v0.7.7 bytes
(`.github/workflows/post-release-acceptance.yml:356-600`; PRs #128 and #131).
An unattended v0.7 updater can only swap one executable, so it can never run
that path.

## Design

### The release

| Asset | What it is |
|---|---|
| `arc-node-linux-x86_64`, `arc-node-linux-aarch64` | The launcher (`crates/arc-legacy-bridge`), statically linked (musl) |
| `arc-node-macos-arm64`, `arc-node-macos-x86_64` | The launcher for macOS |
| `arc-node-windows-x86_64.exe` | The launcher for Windows (desktop only; v0.7 had no Windows headless installer) |
| `latest.json` | A byte-for-byte copy of a qualified v0.8 desktop release's `latest.json`. Its URLs and signatures point at that exact immutable v0.8 release. |
| `SHA256SUMS` | Digests of the above, for people. v0.7 updaters verify nothing. |

The tag is `v0.7.12`:

- every v0.8 consumer ignores releases below 0.8.0, so the bridge can be
  "Latest" without entering the v0.8 channel;
- the headless `sed` needs a bare `vX.Y.Z`;
- the old daemon needs a version above the v0.7.7 binaries it runs.

The version is set in `crates/arc-legacy-bridge/Cargo.toml` and
`pins/active.json`, and every test reads it from there. `v0.7.12` was the
working name of the recovery candidate that became v0.8.0 on 2026-08-27. That
candidate was never tagged or published, so the name is free. Any later v0.7.x
works the same way if a different number is preferred.

### The launcher

Released v0.7 supervisors keep starting `bin/arc-node` with their v0.7 command
line. The launcher sits in that slot permanently and on every start:

1. **Recognizes the invocation** (`src/argv.rs`). It accepts exactly the v0.7
   desktop and headless command lines above. Anything else exits 64 without
   touching the machine. An explicit `--stake` other than 0 is a validator
   and exits 78. The v0.7 identity seed argument is consumed and dropped, never
   stored, logged, transformed or forwarded.
2. **Resolves the layout** (`src/layout.rs`). The launcher must be
   `<ARC dir>/bin/arc-node`. A headless data directory must be
   `<ARC dir>/data`. A desktop data directory is `~/.arc` (or `.arc` relative
   to the app on Windows builds without `HOME`). The bridge owns only
   `<ARC dir>/legacy-bridge/`, created 0700 and refused if it is a link or
   writable by others.
3. **Refuses to run beside v0.7.** On Linux and macOS it refuses (exit 75,
   retried by the supervisor) while any v0.7 `arc-node` with
   a seed argument still runs on that data directory.
4. **Archives in place** (`src/archive.rs`). It records a stat-only manifest
   (path, type, size, mtime; links recorded, never followed) of the v0.7 data
   directory as `v0.7-data-archive-0001.json`. It writes a new generation only
   if the tree has changed, for example after a rollback ran v0.7 again. The
   bridge never opens a v0.7 file for writing, renaming or deletion.
5. **Fetches and verifies the pinned release** (`src/release.rs`,
   `src/sshsig.rs`, `src/manifest.rs`, `src/fetch.rs`). It downloads
   `SHA256SUMS` and `SHA256SUMS.sig` first and requires their pinned digests.
   It verifies the SSHSIG with the release key, principal `arc-release` and
   namespace `arc-release-manifest-v1`, as `install.sh:3055` does. It checks
   the four header lines (schema, repository, tag, commit) and that the
   signed digest of every used asset equals the compiled-in pin. Only then
   does it download this platform's `arc-node` and `arc-cli`, `genesis.toml`
   and `testnet-seeds.txt`, resumably, size-capped, with SHA-256 checked
   before an atomic rename. A cached file that no longer matches is deleted
   and fetched again, never executed.
6. **Probes** that both binaries run here and report the pinned version.
   Otherwise it exits 78, which, for example, catches a C library that is too
   old for the v0.8 Linux build.
7. **Creates a fresh identity** with the pinned `arc-cli keygen`, which
   writes the keyfile owner-only (mode 0600, or a protected DACL on Windows).
   It does not reuse the v0.7 identity: a v0.7 headless seed is
   `community-<hostname>-<8 hex>` (`install-community-node.sh:209-214`), about
   32 bits of secret, so that key could be brute-forced from its public
   address. The upgraded desktop app restores its BIP-39 identity itself.
8. **Decides compute** (`src/consent.rs`). Compute is always off under a v0.7
   desktop, because the upgraded app asks the question. A headless node
   computes only with an operator-recorded `yes` and a recorded,
   SHA-256-verified model. Released v0.7 has no opt-in signal to inherit:
   every v0.7 community installer hard-coded `--community-mode`, and `--model`
   named a file, not a choice about the new network. A pin whose node build
   would publish the computer's hostname (`main.rs:8749-8826` at `v0.8.10`,
   fixed by PR #134) runs with `--no-community` and no compute.
9. **Starts the node exactly as `install.sh:4578-4595` would.**
   - **Network and stake:** `--rpc 127.0.0.1:<v0.7 port> --p2p-port <v0.7 port>`
     with the release's seeds and genesis, and `--stake 0 --min-stake 0
     --eth-rpc-port 0`.
   - **Fresh state:** `--data-dir <ARC dir>/legacy-bridge/nodes/<kind>-<id>/data`
     and the fresh keyfile.
   - **Community:** `--community-mode` with the six HTTPS origins, or
     `--no-community`.
   - **Worker flags:** `--model <verified> --full-integer-worker` only with
     consent.
   - **Model auto-discovery:** a stake-0 v0.8 node without `--model` loads any
     canonical GGUF it finds (`main.rs:4725` and `:6670` at `v0.8.10`). So the
     node runs with a private working directory and `HOME`, and the bridge
     refuses (exit 78) while `/opt/arc/llama2-7b.gguf` or
     `/var/lib/arc/llama2-7b.gguf` exists and compute is off.
   - **Process handling:** on Linux and macOS the launcher `exec`s the node,
     so the supervisor's PID, signals and stdio are the node's. On Windows the
     node runs in a kill-on-close job object, so the v0.7 desktop's "stop"
     still stops it.

A stake-0 v0.8.10 node never joins consensus
(`chain_participation_allowed`, `main.rs:5734` at `v0.8.10`). Its `/health`
reports `chain_participation_enabled: false` and `/node/info` reports
`stake: 0`. The acceptance asserts both.

### Desktop path

1. **Interim observer.** On its next node start, a v0.7 desktop downloads the
   launcher through `ensure_binary`. The node then runs as a stake-0 v0.8
   observer with compute off. The app's Logs screen shows "Your ARC node is
   upgrading to the new network" and the next step.
2. **App update.** Settings > Check for updates > Install installs the
   desktop release named by the bridge's `latest.json`.
3. **First launch of the new app.** It fences the v0.7 WAL (existing v0.8
   behavior) and asks once: **"Your ARC node is upgrading to the new network.
   Keep contributing compute?"** with **Yes, keep contributing** and **Not
   now** (`desktop/src/components/LegacyUpgradeDialog.tsx`,
   `desktop/src-tauri/src/legacy_upgrade.rs`). Until the user answers, the
   node auto-starts as an observer without a model, even if v0.7 ran it as a
   worker. The answer goes through `set_compute_contribution`, the same path
   as the Settings switch from PR #138. Yes downloads and verifies the model
   if needed and promotes the node; Not now keeps it an observer.

### Recovery

| Situation | What happens |
|---|---|
| Supervisor restarts the node | The launcher re-verifies the cache and `exec`s the same node; same identity and data |
| Updater runs again | `version.txt` equals the Latest tag: nothing to do |
| Cached binary corrupted or tampered with | Detected by digest, deleted, and fetched again before any start |
| Download fails or the network is down | Exit 69; v0.7 data untouched; the supervisor retries. Within the first 30 s the v0.7 headless updater rolls back to `arc-node.prev` by itself, and the next daily run resumes the partial download |
| Machine cannot run the pinned build | Exit 78 at the probe; the v0.7 updater rolls back to `arc-node.prev` |
| Operator wants v0.7 back (headless) | `~/.arc/bin/arc-node --legacy-bridge-rollback` restores the pre-bridge binary, which is kept under `legacy-bridge/preserved/`; restart the service |
| Operator wants the bridge again | `cp ~/.arc/legacy-bridge/arc-node-bridge-<version> ~/.arc/bin/arc-node.new && mv ~/.arc/bin/arc-node.new ~/.arc/bin/arc-node`, then restart |
| Start over | Remove `~/.arc/legacy-bridge/` and restart. A new identity and fresh data are created; v0.7 data is untouched |

Operator commands (all read the bridge's own files):

```bash
~/.arc/bin/arc-node --legacy-bridge-status
~/.arc/bin/arc-node --legacy-bridge-compute on --download-model     # or --model /path/to/model.gguf
~/.arc/bin/arc-node --legacy-bridge-compute off
~/.arc/bin/arc-node --legacy-bridge-verify-archive --hash
~/.arc/bin/arc-node --legacy-bridge-rollback
```

### Safety invariants

- The bridge never runs v0.7 code and never writes, renames, or deletes v0.7
  data.
- Bridged nodes are stake 0 and never validators. An explicit validator stake
  is refused, and `--shard-range`, validator seeds, and insecure development
  flags are never passed.
- Only bytes pinned at compile time are executed, after the owner-signed
  manifest verifies.
- No compute without consent.
- No registration from a node build that publishes hostnames.
- v0.7 and v0.8 never run side by side on the same data directory.
- No v0.8 release is ever marked "Latest".

## Evidence (CI)

`.github/workflows/legacy-bridge.yml` runs on every push to a non-main branch
that touches the bridge. Every acceptance job rejects all six live-network
addresses for its whole run, so nothing registers with or writes to the
public testnet. The jobs read only public release assets.

| Job | What it proves |
|---|---|
| bridge unit tests (Linux, Windows) | The real v0.8.10 `SHA256SUMS` signature verifies with the release key, and any byte change, wrong key or namespace fails. Every pin equals the signed manifest. Downloads resume after a cut and reject tampered or oversized bodies against a local HTTP server. Exact v0.7 command lines are accepted, validator stakes refused, and seeds never retained. The observer and worker command lines are correct. Auto-discovery hazards are refused. Archive generations and the stat-only manifest work. |
| headless acceptance (systemd, real updater) | The **unmodified v0.7.11 `install-community-node.sh`** installs the pinned v0.7.7 binary under systemd, and v0.7.7 writes real state. Its own **`arc-auto-update.sh`, run as `arc-updater.service`**, consumes a simulated v0.7.12 Latest and passes its 30-second health check. The service's process is the pinned v0.8.10 binary with `--stake 0 --min-stake 0`, a fresh data directory, `--no-community` and no `--model`. `/node/info` reports stake 0 and `/health` reports `chain_participation_enabled: false`. The v0.7 data is byte-identical to the moment systemd stopped v0.7, captured by an `ExecStartPre` hook. No seed appears anywhere the bridge wrote. A second updater run is a no-op, restarts reuse the cache, and a corrupted cache self-heals. Compute and validator command lines are refused. Rollback runs v0.7.7 again on its untouched data, and reinstall bridges again with the same identity. |
| desktop acceptance (Linux, Windows) | A line-for-line harness of the v0.7.11 app's `ensure_binary` and `NodeManager::start`, checked against the tag's source, reproduces today's stranding (HTTP 404 from the desktop-only Latest). With the bridge as Latest, the next start installs the launcher and runs the pinned node at stake 0 in a fresh directory, with the v0.7 `~/.arc` state byte-identical. Stopping the app's child stops the node (job object on Windows), and a restart reuses the verified cache. |
| Tauri updater check (Linux job) | The v0.8 desktop `latest.json` and all four platform bundles verify with the key embedded in v0.7.11, with a version above 0.7.11. A v0.7 desktop will install it. |
| `legacy-bridge-release-assets.yml` | `pin-release.py --check` re-derives every pin from the live release and its owner signature. The workflow builds the five launchers, verifies the desktop `latest.json`, and assembles the owner handoff with an `eligible_for_latest` verdict. |

## Rollout

Each stage needs TJ's explicit go. Every command below is an owner action.
None is automated.

### Stage 0: prerequisites (merges and a normal v0.8 release)

1. Merge PRs #134 (privacy-safe worker names), #135, #138 (consent switch)
   and this PR.
2. Cut the next v0.8 release (the release captain's v0.8.11 or v0.8.12,
   carrying the privacy, joining, compute, and twin-verification fixes)
   through the normal `release.yml` pipeline. It stays non-latest
   automatically.
3. Re-pin the bridge to it, in a reviewed PR:
   `python3 scripts/legacy-bridge/pin-release.py --tag v0.8.11 --write`.
   The generator refuses unless the release is immutable, bot-published and
   owner-signed. It sets `worker_names_privacy_safe` from the tag's source.
4. Run **Legacy bridge release handoff** (`workflow_dispatch`) with
   `desktop_tag=v0.8.11`. Proceed only if `legacy-bridge-provenance.json`
   says `"eligible_for_latest": true`. That requires the pinned node to keep
   hostnames private and the desktop release to contain the first-launch
   question.

### Stage 1: canary as a non-latest tagged release

1. Create the protected tag on the merge commit that produced the handoff:
   `git tag v0.7.12 <commit> && git push origin v0.7.12`. The push also starts
   `release.yml`, which stops at its first step because the workspace version
   is not 0.7.12. That failed run creates nothing and is expected.
2. From the downloaded `legacy-bridge-release-handoff` artifact directory,
   publish it **not** as Latest, uploading exactly the handoff files:

   ```bash
   gh release create v0.7.12 --repo FerrumVir/arc-chain --verify-tag --latest=false \
     --title v0.7.12 --notes-file RELEASE-NOTES.md v0.7.12/*
   ```

   The release title must be exactly `v0.7.12`.
3. Confirm Latest did not move. This must still print `v0.7.11`:
   `gh api repos/FerrumVir/arc-chain/releases/latest --jq .tag_name`.
4. Compare the uploaded digests with the handoff `SHA256SUMS`:
   `gh api repos/FerrumVir/arc-chain/releases/tags/v0.7.12 --jq '.assets[] | "\(.digest) \(.name)"'`.

### Stage 2: manual opt-in test on one machine

- **Headless (one v0.7 Linux or macOS node):** first run the dry run, which
  prints the plan, then apply it:

  ```bash
  bash scripts/legacy-bridge/canary-consume.sh --tag v0.7.12 --expect-sha256 <digest from SHA256SUMS>
  bash scripts/legacy-bridge/canary-consume.sh --tag v0.7.12 --expect-sha256 <digest> --apply
  ```

  These are exactly the v0.7 updater's steps, plus a digest check.
- **Then check on that machine:**
  - `~/.arc/bin/arc-node --legacy-bridge-status` shows stake 0;
  - `curl -s localhost:9944/node/info` shows `"stake":0` and the pinned
    version;
  - `~/.arc/bin/arc-node --legacy-bridge-verify-archive --hash` reports
    "unchanged";
  - the node appears on `/workers/scoreboard` under a `node-…` name, not a
    hostname;
  - optionally `--legacy-bridge-compute on --download-model`, a restart, and
    jobs on the scoreboard.
- **Desktop (one v0.7.11 Mac or PC):** install the v0.8.11 desktop app from
  its exact release over the v0.7.11 app (same bundle identifier, as the
  Tauri update would). Check that the first launch asks "Your ARC node is
  upgrading to the new network. Keep contributing compute?", that "Not now"
  leaves Settings > Contribute compute off, and that the v0.7 `~/.arc` files
  are untouched.

### Stage 3: mark Latest (TJ's explicit go)

```bash
gh release edit v0.7.12 --repo FerrumVir/arc-chain --latest
gh api repos/FerrumVir/arc-chain/releases/latest --jq .tag_name   # v0.7.12
```

From then on:

- headless v0.7 nodes bridge at their next 04:17 run;
- v0.7 desktops bridge at their next node start, and get the app on
  Settings > Check for updates.

Future v0.8 releases keep `make_latest=false`, which leaves Latest on the
bridge.

### Monitoring (first 72 hours, then weekly)

- **Downloads:** `gh api repos/FerrumVir/arc-chain/releases/tags/v0.7.12
  --jq '.assets[] | "\(.download_count) \(.name)"'`. Launcher downloads show
  updaters arriving; desktop launcher counts repeat because a v0.7 app
  re-fetches at every node start until it is updated. `latest.json`
  downloads show desktop update clicks.
- **Fleet impact:** the read-only public `/workers/scoreboard` and `/health`
  of each validator. Bridged nodes appear as community observers (and as
  workers after consent), and validator health and latency should not move.
- **Reports:** issues, and support messages that quote `bridge.log` or
  exit codes 64, 69, 75 or 78 (see `--help` and the table above).

### Rollback

- **Stop new bridges:** `gh release edit v0.7.11 --repo FerrumVir/arc-chain
  --latest`. Already-bridged headless nodes keep running the pinned v0.8 node,
  because v0.7.11 has no node assets for their updater to fetch. Unbridged
  v0.7 installs return to today's stranded state.
- **Fix a launcher bug:** publish v0.7.13 with the fix through Stages 1-3. v0.7
  updaters install any newer tag, and the launcher reuses each node's
  identity and data.
- **Per node:** `--legacy-bridge-rollback` (headless), or reinstall the v0.7
  app (desktop).
- **Never** mark a v0.8 release Latest, publish a handoff whose
  `eligible_for_latest` is false as Latest, or delete the v0.7.11 release.

## Known limits

- **v0.7 desktops:** they reach the new app only when the user clicks Check
  for updates (v0.7 had no automatic app update). Until then they run the
  interim observer with compute off, under an interim identity.
- **Linux headless nodes without passwordless sudo:** the v0.7 updater cannot
  restart the service (`sudo systemctl restart` fails quietly), so the bridge
  takes effect at the next service restart or reboot.
- **v0.7 app crash or force-quit:** if the v0.7 app crashes or is force-quit
  while the bridged node runs, the node keeps running in the background, as a
  v0.7 node did. The app's next start then reports that the node's data
  directory is locked. The tray's "Quit ARC Node" stops the node cleanly
  (`tray.rs:53-63`); otherwise, restart the computer.
- **Custom layouts are refused (exit 64 or 78), not guessed:** source builds
  driven by the old daemon, custom data directories, and explicit stakes.
- **`arc-node-linux-aarch64`:** this asset was never published for v0.7, so
  no ARM64 Linux v0.7 install can have a working updater. It is included for
  completeness.
- **Old Linux C libraries:** the pinned v0.8 Linux node needs a C library at
  least as new as Ubuntu 22.04's. Older hosts stay on v0.7 through the
  updater's rollback.
- **Desktop observer auto-discovery:** a v0.8 desktop observer with a
  canonical GGUF at `$HOME/.arc-models/llama2-7b.gguf` could load it through
  model auto-discovery. The bridge's launcher hides those paths, but the
  desktop app does not; this is worth a follow-up in the consent work.
