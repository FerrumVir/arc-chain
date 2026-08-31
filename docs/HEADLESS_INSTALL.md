# ARC headless/server installation

This guide is for EC2, VPS, SSH-only, and other machines with no graphical
session. The desktop `.dmg`, `.exe`, AppImage, `.deb`, and `.rpm` packages are
GUI applications; they are not substitutes for the headless `arc-node` binary.

> **Source-freeze release status (2026-08-31; tag-stable) — do not skip:** At
> this review cutoff, v0.8.0 was an unreleased recovery candidate, the public
> v0.7.11 release was desktop-only, and v0.8.0 was not published or deployed.
> This is historical status, not a live probe. The commands below are valid
> only when GitHub shows that exact immutable release with `arc-node`,
> `arc-cli`, `install.sh`, `genesis.toml`, `testnet-seeds.txt`, `SHA256SUMS`,
> and `SHA256SUMS.sig` assets. Publication never proves that the public seeds
> were deployed; require the signed coordinated-rollout evidence separately.

## Candidate release targets and tested runners

| Operating system | Architecture | Asset | Installer |
|---|---:|---|---|
| Linux | x86_64 / amd64 | `arc-node-linux-x86_64` | Yes |
| Linux | ARM64 / aarch64 | `arc-node-linux-arm64` | Yes |
| macOS (boot-tested on macOS 15) | Apple Silicon | `arc-node-macos-arm64` | Yes |
| macOS (boot-tested on macOS 15 Intel) | Intel | `arc-node-macos-x86_64` | Yes |
| Windows (boot-tested on GitHub `windows-latest`) | x86_64 | `arc-node-windows-x86_64.exe` | Manual |

Every row also has a matching `arc-cli-*` asset. The Linux x86_64/amd64
artifact is built on the oldest supported Ubuntu baseline, 22.04, then both
the node and CLI are executed and the real node is booted in clean Ubuntu
22.04, 24.04, and 26.04 containers with `DISPLAY` unset before publication.
Linux ARM64 is built on Ubuntu 24.04 ARM and receives the same GUI-free checks
on Ubuntu 24.04 and 26.04. Other Linux distributions may work when their glibc
is compatible, but are not claimed by that runtime gate. There is no Linux
ARM64 desktop bundle and no Windows ARM64 release. The candidate asset matrix
proves those architectures and named CI runners; it does not claim an older
macOS or Windows-version floor.

## Linux and macOS

After the complete v0.8.0 release is published, download the installer from
the owner-created protected source tag and pin the same version when running
it. Keeping download and execution separate makes network errors visible and
lets you inspect the script first.

```bash
curl -fsSLO --proto '=https' --proto-redir '=https' --tlsv1.2 https://raw.githubusercontent.com/FerrumVir/arc-chain/v0.8.0/install.sh
ARC_INSTALL_SHA256=c699b59e0137230ef40d9505a4226d562c8f0d0eda8543de1a42be323d080d37
if command -v sha256sum >/dev/null 2>&1; then
  printf '%s  %s\n' "$ARC_INSTALL_SHA256" install.sh | sha256sum -c -
else
  printf '%s  %s\n' "$ARC_INSTALL_SHA256" install.sh | shasum -a 256 -c -
fi
bash install.sh --version 0.8.0
```

The branch above uses `sha256sum` on Linux and the standard `shasum` fallback
on macOS. A digest mismatch stops the command block before the installer runs.

That first script download is the bootstrap trust boundary: HTTPS reads it
from a semver tag whose creation is owner-only and whose update/deletion is
blocked for everyone. The script requires OpenSSH 8.1 or newer and verifies
the release's namespaced Ed25519 `SHA256SUMS.sig` against its embedded owner
public key before it downloads executable payloads or replaces any managed
file. An unsigned checksum file is never an authenticity boundary.

The installer does not resolve `releases/latest/download` for the programs. It
first reads one release's metadata, requires GitHub to report that release as
immutable, non-draft, and non-prerelease, requires the server-authenticated
release author to be `github-actions[bot]`, validates a strict
`vMAJOR.MINOR.PATCH` tag and protected source commit, and downloads from that
exact tag. It verifies the signature first, then the
current platform's node and CLI plus seeds and genesis against `SHA256SUMS`; if
auto-update is enabled, it also verifies the installer copy it retains. A
desktop-only or otherwise incomplete platform bundle is an error; it never
silently walks backward to an old version. The release publisher separately
requires every supported platform before it can publish anything.

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

The recovery candidate bundles the checkpoint-bound complete validator set:
six rotated public identities and reward activation at block 137146. A
stake-zero process uses that recovered network identity while keeping local
consensus and voting disabled; community work and chain reads use the six
reviewed HTTPS origins. The file does not prove that the public cutover is
live. Confirm the rollout checks in
[VALIDATOR-FLEET-ROLLOUT.md](VALIDATOR-FLEET-ROLLOUT.md) before describing the
node as connected to the repaired fleet.

The activation height is part of the semantic network hash. Consensus still
rejects reward tx `0x25` until that height, and the independent
`--enable-community-rewards-v1` switch cannot override a mismatched network or
missing validator approval quorum. Absence means consensus rejects reward tx
`0x25`; a local flag cannot manufacture network activation.

### Worker hardware and model

- An observer/router does not need a model or GPU. Size CPU, RAM, storage, and
  bandwidth from measured local usage before exposing it as a public service.
- The current full-model worker target is Llama-2-7B Q4_K_M (about 4 GB on
  disk). The release has not established a minimum-RAM figure; prove a complete
  model load and inference on the target host while leaving OS/chain headroom.
  More CPU cores can reduce latency. A GPU is optional and does not change
  correctness or reward eligibility.
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

#### Download and verify the exact worker model

The desktop app downloads and verifies this artifact automatically. A headless
operator must stage the same bytes explicitly. The supported artifact is
`llama-2-7b-chat.Q4_K_M.gguf`, exactly 4,081,004,224 bytes, with SHA-256
`08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa`.
The hash, not the filename or download host, is the trust boundary.

```bash
mkdir -p "$HOME/.arc-models"
curl --fail --location --retry 3 --continue-at - \
  --proto '=https' --proto-redir '=https' --tlsv1.2 \
  --output "$HOME/.arc-models/llama2-7b.gguf" \
  https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/191239b3e26b2882fb562ffccdd1cf0f65402adb/llama-2-7b-chat.Q4_K_M.gguf

test "$(wc -c < "$HOME/.arc-models/llama2-7b.gguf" | tr -d ' ')" = 4081004224
printf '%s  %s\n' \
  08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa \
  "$HOME/.arc-models/llama2-7b.gguf" | sha256sum -c -

bash install.sh --version 0.8.0 \
  --model "$HOME/.arc-models/llama2-7b.gguf"
```

On macOS, replace the `sha256sum -c -` pipeline with:

```bash
printf '%s  %s\n' \
  08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa \
  "$HOME/.arc-models/llama2-7b.gguf" | shasum -a 256 -c -
```

If either the byte count or digest differs, delete only the incomplete model
file and retry the download. Do not start a worker with unverified bytes. Model
verification proves artifact identity; it does not prove that the fleet is
ready, that a job will be assigned, or that a reward will be mined.

The installer never emits `--model ""`. It generates a persistent Ed25519 JSON
keyfile at `INSTALL_DIR/identity/validator-key.json`, mode `0600`, and passes
only its path to the node. Secret material never enters process arguments or
an environment file. Preserve that keyfile across restarts and back it up
privately; do not post it in a support channel. During the narrowly verified
v0.7 adoption, the file-only converter preserves the legacy public address and
keeps the old seed only in the protected legacy evidence archive.

`--data-dir` must name a normalized absolute directory dedicated to ARC. Paths
with `.`/`..`, repeated separators, symlinked components, or an overlap with
the program/identity tree are rejected before any directory or ownership
change. Do not pass a shared system location such as `/`, `/etc`, `/usr`,
`/var`, or `/var/lib`; use a dedicated descendant such as
`/var/lib/arc-chain/data`, `/srv/arc-data`, a mounted volume subdirectory, or
`$HOME/arc-chain-data`. For a system service, every existing parent of a custom
install or data directory must be root-owned and not group/world writable; the
final data directory remains owned by the node account.

The install root is different from the chain data marker described below. A
fresh install atomically creates `.arc-chain-install-root`, bound to the exact
normalized install path. Later installs and updates require it and never adopt
an arbitrary existing unmarked directory. There is one narrow bridge for the
affected community: exact default `~/.arc` and `/var/lib/arc-chain` roots may
be adopted only when ownership, every ancestor's write permissions, all path
components, the v0.7.x binary/version pair, ARC genesis, seeds, identity, data,
and optional updater/model match the recognized legacy layout. Custom roots
and incomplete or hostile lookalikes fail before downloads or mutation.

Before touching v0.7 files, that bridge fsyncs a pending marker bound to the
exact install path, source version, fresh v0.8 data path, service scope,
supervisor kind, RPC/P2P ports, and model path. It preserves the old
configuration and supervisor definition under `legacy-v0.7-preserved/`, copies
the exact identity to the protected v0.8 seed, leaves `data/` and the model
unchanged, retains verified custom ports/model selection, and uses
`data-v0.8/` for fresh state. A crash resumes only the bound scope and
configuration. The final purge-authorizing marker is promoted only after the
complete v0.8 install transaction commits; a pending migration cannot purge
the directory.

Supervisor adoption is deliberately exact. Linux accepts the historical
root-owned `/etc/systemd/system/arc-node.service` and either both old
`arc-updater` unit files or neither. macOS accepts `com.arc.inference` and the
optional `com.arc.updater` LaunchAgent. A detached install requires a live
`node.pid` whose inspected command is the recognized v0.7 invocation. Wrong
users, extra lifecycle directives, partial updater pairs, changed labels or
executables, ambiguous supervisors, stale/reused PIDs, symlinks, and
group/world-writable ancestors fail before reservation or download.

For the common Linux `~/.arc` topology, the one-time migration uses sudo to
retire the old global units transactionally. It replaces them with root-owned
managed units, but both the node and signed updater execute as the original
community user. The old unsigned `arc-auto-update.sh` and `arc-updater` timer
are stopped and removed before the node binary changes, so they cannot later
overwrite v0.8. Scheduled updates need no recurring sudo: the user-owned,
checksummed installer replaces only user-owned ARC files and signals the exact
owned node PID; systemd's root-owned `Restart=always` unit relaunches it. If
any copy, service, or health step fails, rollback restores the exact v0.7
binary/unit files and each prior active/enabled service and timer state.

macOS uses the same durable drain contract. The managed node LaunchAgent sets
`ExitTimeOut=4420`, covering the 4,000-second public inference window, the
300-second late-submit grace, and 120 seconds for task joins and WAL fsync.
During the first upgrade from an older plist that lacks that key, the installer
disables the exact verified label, sends SIGTERM to its inspected node PID, and
waits for that process to exit before `bootout`; it therefore never relies on
launchd's shorter system-defined default. A scheduled `network.arc.update` job
is not unloaded while it may be running the installer itself. The updated
plist remains at the same path and is picked up on the next login/bootstrap.

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

The adopted v0.7 Linux `~/.arc` bridge is intentionally different from a new
root system install: its global unit files are root-owned, but programs,
identity, data, node process, and updater process remain owned by the original
community user. `install.conf` records the internal `system-user` scope so
every later update follows that same boundary. During an update that
unprivileged bridge sends SIGTERM only to its verified node PID, then follows
the systemd unit through the complete graceful drain and `Restart=always`
transition. The wait covers the node's 4,000-second inference window plus its
300-second crash/late-submit grace for already-owned community work, its
writer/fsync barrier, and restart allowance; an inactive, failed, unexpected,
or timed-out unit rolls the file transaction back instead of claiming success.

### Service commands

Linux system service:

```bash
sudo systemctl status arc-node
sudo journalctl -u arc-node -f
sudo systemctl restart arc-node
```

The same commands inspect an adopted v0.7 Linux `system-user` bridge. Do not
use `systemctl --user` for that migrated topology; `install.conf` identifies
it as `service_scope=system-user`.

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

### Troubleshooting and permissions

First identify which installation scope owns the node. Do not alternate
between a user install and a sudo/system install while trying random fixes:

```bash
# User scope
test -f "$HOME/.arc/install.conf" && sed -n '1,20p' "$HOME/.arc/install.conf"
systemctl --user --no-pager status arc-node

# System scope
sudo test -f /var/lib/arc-chain/install.conf \
  && sudo sed -n '1,20p' /var/lib/arc-chain/install.conf
sudo systemctl --no-pager status arc-node
```

`install.conf` contains paths, ports, and service scope, not private identity
material. Do not paste `identity/validator-key.json`, any legacy evidence,
process arguments, environment contents, or a recovery phrase into a support
channel. Do not use
`chmod -R 777`, `chown -R`, or delete the data directory to make an error go
away; those actions can weaken the identity boundary or destroy the evidence
needed to diagnose a failed recovery.

| Symptom | Safe check and action |
|---|---|
| `sudo authorization failed` | Install as the login user with `bash install.sh --version 0.8.0 --user-service`, or deliberately use `sudo bash install.sh --version 0.8.0 --system-service`. Do not mix the two scopes. |
| `No systemd user manager is reachable` over SSH | Use the explicit system service command above, or install with `--no-service` and run the printed command under your own supervisor. `sudo loginctl enable-linger "$USER"` is an optional administrator decision for a user service that must start before login. |
| Model file is unreadable | As the account that will run the node, use `test -r /absolute/path/to/model.gguf`. Move the model to a directory that account can traverse or correct only the specific file/directory ownership; do not make the model or identity world-writable. |
| Port is already in use | On Linux inspect `ss -ltnp` for RPC and `ss -lunp` for P2P, then choose unused explicit values with `--port` and `--p2p-port`. Do not kill an unidentified process. |
| Service starts but health is `degraded` | Read the matching user/system journal above, confirm peers and the selected genesis/checkpoint, and run `bash scripts/arc-diagnose.sh` from a reviewed checkout. `degraded` is not fixed by exposing RPC publicly or reusing an old WAL. |
| Update says no existing installation | Run the updater from the same install scope and exact install directory recorded in `install.conf`. A `--no-auto-update` install intentionally has no retained updater. |
| Legacy adoption rejects a service or PID | Do not rename or weaken files to bypass the check. Confirm that the service is the unmodified v0.7 default, that both `arc-updater` units are present or absent together, that `node.pid` still names the live ARC process, and that `~/.arc` plus its ancestors are not symlinks or group/world writable. Stop and preserve an unrecognized custom layout for manual review. |
| Installer reports an incomplete release or checksum/signature failure | Stop. Confirm the exact v0.8.0 release contains every required platform asset and signed manifest. Never fall back to a desktop package, a moving URL, or an unsigned older binary. |

For a support report, share the operating system/architecture, release version,
service scope, redacted journal error, local `/health` response, and the output
of `scripts/arc-diagnose.sh`. That diagnostic intentionally excludes process
arguments and validator material and compares same-height hashes/state roots;
an advancing DAG round alone is not a healthy-chain result.

## Updates and rollback behavior

The optional daily systemd timer/LaunchAgent runs the checksummed installer
copy from the installed release. It resolves the latest immutable, non-draft,
non-prerelease release, requires its complete bundle for the installed platform,
refuses a version lower than the installed binary, verifies downloads, replaces
files atomically, restarts the same service scope, and checks the saved custom
RPC port. Before the first replacement it snapshots every managed binary, network
file, runner, install config, identity file, service definition, and the active
and enabled service/timer state. A copy, service-manager, or health failure
restores that complete snapshot (or removes a newly introduced managed file)
and returns the service and updater timer to their prior state. Existing model
and chain-data contents are never replacement targets.

That transaction also catches an attempted unsafe in-place upgrade whose old
WAL lacks the network marker or has the wrong genesis hash. Rollback is not
state migration, so the operator must still select a fresh stake-zero data
directory or an approved validator checkpoint migration before retrying.

Run the same updater manually:

```bash
# User systemd / macOS install
"$HOME/.arc/bin/arc-installer" --update-only --install-dir "$HOME/.arc"

# Adopted v0.7 Linux system-user bridge (runs unprivileged; no sudo)
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

PowerShell does not use this Bash installer. Download these four files from
the same release and the allowed-signers file from the protected source tag:

- `arc-node-windows-x86_64.exe`
- `arc-cli-windows-x86_64.exe`
- `SHA256SUMS`
- `SHA256SUMS.sig`
- `release/arc-release-allowed-signers` from tag `v0.8.0`

Windows OpenSSH 8.1 or newer is required; `ssh -V` prints the installed
version. First use that client to authenticate the exact manifest, then compare
PowerShell's digest with the corresponding signed manifest line. The blocking
cross-OS preflight exercises this exact `cmd.exe` verification path on Windows
before the release tag is created:

```powershell
cmd.exe /d /c "ssh-keygen -Y verify -f arc-release-allowed-signers -I arc-release -n arc-release-manifest-v1 -s SHA256SUMS.sig < SHA256SUMS"
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

`--purge` is destructive, but it is intentionally narrow. A fresh installer
creates `.arc-chain-install-root` with the exact normalized install path.
Install, update, and every uninstall refuse a pre-existing directory without
that final marker; a pending adoption marker cannot authorize removal. Purge
revalidates the final marker's regular-file type, exact contents, owner, and
permissions immediately before recursion. Broad roots, path traversal, and
symlinked roots/markers are rejected. Purge removes only the marked install
root; a custom data directory outside it is preserved and reported. Back up
the identity first if the node address must be retained.
