# ARC headless/server installation

This guide is for EC2, VPS, SSH-only, and other machines with no graphical
session. The desktop `.dmg`, `.exe`, AppImage, `.deb`, and `.rpm` packages are
GUI applications; they are not substitutes for the headless `arc-node` binary.

> **Release status — do not skip:** v0.8.0 is an unreleased recovery
> candidate. The current public v0.7.11 release is desktop-only and cannot
> install an SSH/headless node. The v0.8.0 commands below become valid only
> after GitHub shows that exact release with `arc-node`, `arc-cli`, `install.sh`,
> `genesis.toml`, `testnet-seeds.txt`, and `SHA256SUMS` assets. Nothing here
> claims that v0.8.0 is already deployed to the public seeds. v0.8.0 is not published or deployed.

## Supported release targets

| Operating system | Architecture | Asset | Installer |
|---|---:|---|---|
| Linux | x86_64 / amd64 | `arc-node-linux-x86_64` | Yes |
| Linux | ARM64 / aarch64 | `arc-node-linux-arm64` | Yes |
| macOS 11+ | Apple Silicon | `arc-node-macos-arm64` | Yes |
| macOS 11+ | Intel | `arc-node-macos-x86_64` | Yes |
| Windows / Windows Server | x86_64 | `arc-node-windows-x86_64.exe` | Manual |

Every row also has a matching `arc-cli-*` asset. The release pipeline executes
both Linux binaries in clean Ubuntu 24.04 and Ubuntu 26.04 containers with
`DISPLAY` unset before it can publish. Other Linux distributions may work when
their glibc is compatible, but are not claimed by that runtime gate. There is no
Linux ARM64 desktop bundle and no Windows ARM64 release.

## Linux and macOS

After the complete v0.8.0 release is published, download that exact installer
asset and pin the same version when running it. Keeping download and execution
separate makes network errors visible and lets you inspect the script first.

```bash
curl -fsSLO https://github.com/FerrumVir/arc-chain/releases/download/v0.8.0/install.sh
bash install.sh --version 0.8.0
```

The installer does not resolve `releases/latest/download` for the programs. It
first reads one release's metadata, validates a strict `vMAJOR.MINOR.PATCH`
tag, checks that every required platform asset exists, and then downloads from
that exact tag. It verifies the node, CLI, seeds, genesis, and updater installer
against `SHA256SUMS`. A desktop-only or otherwise incomplete release is an
error; it never silently walks backward to an old version.

### Common server setups

```bash
# Non-root Linux: ~/.arc plus a systemd user service
bash install.sh --version 0.8.0

# Root/sudo Linux: root-owned programs in /var/lib/arc-chain and a system
# service whose node process/data/identity belong to the invoking sudo user.
# A direct root login intentionally runs the node as root.
sudo bash install.sh --version 0.8.0 --system-service

# Custom install root, chain data volume, RPC, and P2P ports
bash install.sh \
  --version 0.8.0 \
  --install-dir "$HOME/.arc-custom" \
  --data-dir "$HOME/arc-chain-data" \
  --port 19090 \
  --p2p-port 19091

# Install and verify only. No service, background process, health request, or
# update schedule is created.
bash install.sh --version 0.8.0 --no-service --no-auto-update

# Load a local model and become eligible to execute compatible inference work.
bash install.sh --version 0.8.0 --model /absolute/path/to/model.gguf
```

The default node is deliberately `--stake 0 --community-mode`, with the EVM RPC
disabled. It can register as a community observer/router without a model. A
node without `--model` cannot execute local model inference, and installing a
model does not guarantee jobs or rewards; assignment and reward policy are
network behavior, not an installer promise.

When `--model` is present, the managed runner adds
`--full-integer-worker`. This loads the complete deterministic integer model
required for independent verification but does not announce the residential
machine as a validator layer shard. Never substitute `--shard-range 0:32` for
that flag: doing so publishes an overlapping shard that is normally
unreachable behind NAT.

The recovery candidate currently bundles an explicit incomplete validator-set
placeholder. With that file, a stake-zero process starts in HTTP-only migration
observer mode: community inference can be tested, but chain P2P, consensus,
voting, and mined rewards remain disabled. Operators must publish the approved
public-address genesis/checkpoint described in
[VALIDATOR-FLEET-ROLLOUT.md](VALIDATOR-FLEET-ROLLOUT.md) before describing the
node as joined to the repaired chain.

The checked-in placeholder also omits
`[chain].community_rewards_v1_activation_height`. Absence means consensus
rejects reward tx `0x25`; `--enable-community-rewards-v1` cannot override it
and startup rejects that flag/schedule mismatch. An activation may be added
only to a complete approved validator genesis, where it becomes part of the
network identity hash.

### Worker hardware and model

- An observer/router does not need a model or GPU. Size CPU, RAM, storage, and
  bandwidth from measured local usage before exposing it as a public service.
- The current full-model worker target is Llama-2-7B Q4_K_M (about 4 GB on
  disk). Use at least 16 GB system RAM for the expanded integer weights plus
  OS/chain headroom. More CPU cores reduce latency. A GPU is optional and does
  not change correctness or reward eligibility.
- Pass the GGUF by absolute path. Capacity is advertised only after every model
  layer loads, and the request/coordinator must carry the streamed BLAKE3 ID of
  those exact artifact bytes. A matching filename or architecture shape is not
  proof of identical weights. The coordinator independently recomputes every
  accepted community result.
- No hardware tier guarantees work. Promotional jobs depend on coordinator
  availability and policy, exact-artifact capacity, assignment, and independent
  verification. Validator recomputation proves output agreement, not customer
  demand.
  Payment additionally requires reward-protocol activation, validator-approval
  collection, strict greater-than-two-thirds identity and active-stake
  authorization, treasury funding, remaining block/epoch/worker/coordinator
  promotional budget, and a successful mined `0x25` receipt.

The installer never emits `--model ""`. It also stores the validator seed in a
mode-`0600` environment file instead of process arguments, so `ps` does not
expose it. The seed at `INSTALL_DIR/identity/validator-seed` is created once and
preserved on installs and updates. Back it up privately; do not post it in a
support channel.

### v2 data directories are not upgrade inputs

v0.8.0 binds persisted state to the exact network identity. On first use of a
fresh data directory it writes `genesis.network-hash`. Startup fails closed if
an existing WAL has no marker or if the stored hash differs from the selected
genesis. A reachable HTTP process is not permission to bypass that check.

Do not point v0.8.0 at a v0.7.11-or-earlier data directory. For a stake-zero
observer, stop the old service, back up its identity and data for forensics, and
install with a fresh data directory (for example a new `--data-dir` path). Do
not copy the old WAL into it. A validator may move state only through the
approved canonical checkpoint migration in the coordinated fleet runbook; a
unilateral reset or reuse of an old WAL is not a recovery procedure.

For a Linux system service, the default install root is
`/var/lib/arc-chain`. Its programs and updater are root-owned because the daily
timer executes the updater as root; allowing the node account to replace that
script would be a privilege-escalation bug. The identity and data directories
remain owned by the normalized invoking user. A custom system install root is
accepted only beneath a root-owned, non-writable parent and never beneath the
user's home or a temporary directory.

### Service commands

Linux system service:

```bash
sudo systemctl status arc-node
sudo journalctl -u arc-node -f
sudo systemctl restart arc-node
```

Linux user service:

```bash
systemctl --user status arc-node
journalctl --user -u arc-node -f
systemctl --user restart arc-node
```

A user service is enabled for future logins. If it must start at boot before
that user logs in, an administrator can opt in explicitly:

```bash
sudo loginctl enable-linger "$USER"
```

macOS:

```bash
launchctl print "gui/$(id -u)/network.arc.node" \
  || launchctl print "user/$(id -u)/network.arc.node"
tail -f "$HOME/.arc/node.log"
```

Do not run the macOS installer through `sudo`; doing so would create the
LaunchAgent and private files for the wrong account, so the installer refuses.

### Ports and EC2/VPS firewalls

The RPC default is TCP `9944` bound only to `127.0.0.1`; the QUIC P2P default
is UDP `9945`. Custom `--port` and `--p2p-port` values persist in
`install.conf` and are reused by auto-update, but `--port` does not make RPC
public. Managed stake-zero community nodes are outbound-only by default, and
there is no supported installer option that binds unauthenticated RPC to every
interface. A future public-RPC mode requires a separate security review. A
cloud security group or host firewall must still allow any intended peer
traffic; peer connectivity can require outbound UDP.

Check local health on the configured RPC port:

```bash
curl --fail http://127.0.0.1:9944/health
```

Treat the JSON status literally. `ok` means the node's current liveness checks
pass; `degraded` means the process and RPC are reachable but one or more chain
readiness checks do not. The installer accepts both as proof that the new
binary booted, but it prints a warning for `degraded` and never calls that
state healthy.

## Updates and rollback behavior

The optional daily systemd timer/LaunchAgent runs the checksummed installer
copy from the installed release. It resolves a complete latest release, refuses
a version lower than the installed binary, verifies downloads, replaces files
atomically, restarts the same service scope, and checks the saved custom RPC
port. Before the first replacement it snapshots every managed binary, network
file, runner, install config, identity file, service definition, and the active
and enabled service/timer state. A copy, service-manager, or health failure
restores that complete snapshot (or removes a newly introduced managed file)
and returns the service and updater timer to their prior state. Existing model
and chain-data contents are never replacement targets.

That transaction also catches an attempted unsafe in-place upgrade whose old
WAL lacks the network marker or has the wrong genesis hash. Rollback is not
state migration, so the operator must still select a fresh observer data
directory or an approved validator checkpoint migration before retrying.

Run the same updater manually:

```bash
# User systemd / macOS install
"$HOME/.arc/bin/arc-installer" --update-only --install-dir "$HOME/.arc"

# Linux system install
sudo /var/lib/arc-chain/bin/arc-installer --update-only --install-dir /var/lib/arc-chain --system-service
```

Those commands intentionally do not pin v0.8.0: update mode resolves the
newest complete release and then refuses equality or downgrade. They are
available only when the original install kept auto-update enabled; an install
made with `--no-auto-update` does not install `arc-installer`.

Pinning is deterministic and never means “nearest available version”:

```bash
bash install.sh --version 0.8.0
```

If `v0.8.0` lacks any required asset or checksum, installation fails with the
missing filename. If a newer version is already installed, the command refuses
to downgrade it.

## Windows Server manual verification

PowerShell does not use this Bash installer. Download these three files from
the same release:

- `arc-node-windows-x86_64.exe`
- `arc-cli-windows-x86_64.exe`
- `SHA256SUMS`

Then compare PowerShell's digest with the corresponding manifest line:

```powershell
Get-FileHash .\arc-node-windows-x86_64.exe -Algorithm SHA256
Get-Content .\SHA256SUMS | Select-String 'arc-node-windows-x86_64.exe'
```

Run `arc-node-windows-x86_64.exe --help` to construct a service command for the
Windows service manager of your choice. The repository currently publishes the
binary but does not claim a supported automatic Windows Server service setup.

## Uninstall

```bash
# Remove service definitions and programs; preserve identity and chain data.
bash install.sh --uninstall

# System-service install:
sudo bash install.sh --uninstall --system-service

# Explicitly remove everything under the selected install directory.
bash install.sh --uninstall --purge
```

`--purge` is destructive. Back up the identity first if the node address must
be retained.
