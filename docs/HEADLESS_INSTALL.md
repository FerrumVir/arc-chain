# ARC headless/server installation

This guide is for EC2, VPS, SSH-only, and other machines with no graphical
session. The desktop `.dmg`, `.exe`, AppImage, `.deb`, and `.rpm` packages are
GUI applications; they are not substitutes for the headless `arc-node` binary.

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

Download the stable installer asset, then run it. Keeping the download and
execution as separate commands makes network errors visible and lets you
inspect the script first.

```bash
curl -fsSLO https://github.com/FerrumVir/arc-chain/releases/latest/download/install.sh
bash install.sh
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
bash install.sh

# Root/sudo Linux: root-owned programs in /var/lib/arc-chain and a system
# service whose node process/data/identity belong to the invoking sudo user.
# A direct root login intentionally runs the node as root.
sudo bash install.sh --system-service

# Custom install root, chain data volume, RPC, and P2P ports
bash install.sh \
  --install-dir "$HOME/.arc-custom" \
  --data-dir "$HOME/arc-chain-data" \
  --port 19090 \
  --p2p-port 19091

# Install and verify only. No service, background process, health request, or
# update schedule is created.
bash install.sh --no-service --no-auto-update

# Load a local model and become eligible to execute compatible inference work.
bash install.sh --model /absolute/path/to/model.gguf
```

The default node is deliberately `--stake 0 --community-mode`, with the EVM RPC
disabled. It can register as a community observer/router without a model. A
node without `--model` cannot execute local model inference, and installing a
model does not guarantee jobs or rewards; assignment and reward policy are
network behavior, not an installer promise.

The installer never emits `--model ""`. It also stores the validator seed in a
mode-`0600` environment file instead of process arguments, so `ps` does not
expose it. The seed at `INSTALL_DIR/identity/validator-seed` is created once and
preserved on installs and updates. Back it up privately; do not post it in a
support channel.

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

The RPC default is TCP `9944`; the QUIC P2P default is UDP `9945`. Custom
`--port` and `--p2p-port` values persist in `install.conf` and are reused by
auto-update. A cloud security group or host firewall must allow the traffic the
operator intends to expose. Do not expose RPC publicly unless you understand
the available methods and access policy. Community registration uses outbound
requests, but peer connectivity can still require outbound UDP.

Check local health on the configured RPC port:

```bash
curl --fail http://127.0.0.1:9944/health
```

## Updates and rollback behavior

The optional daily systemd timer/LaunchAgent runs the checksummed installer
copy from the installed release. It resolves a complete latest release, refuses
a version lower than the installed binary, verifies downloads, replaces files
atomically, restarts the same service scope, and checks the saved custom RPC
port. If a replacement service does not become healthy, it restores the
previous node binary and restarts it.

Run the same updater manually:

```bash
# User systemd / macOS install
"$HOME/.arc/bin/arc-installer" --update-only --install-dir "$HOME/.arc"

# Linux system install
sudo /var/lib/arc-chain/bin/arc-installer \
  --update-only --install-dir /var/lib/arc-chain --system-service
```

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
