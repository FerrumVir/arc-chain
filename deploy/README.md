# ARC Chain — 4-Node Testnet Deployment

Deploy a production-grade ARC Chain testnet on Hetzner Cloud with 4 ARM64 validator nodes.

## Architecture

```
                    Internet
                       |
        +--------------+--------------+
        |              |              |
   arc-node-0     arc-node-1     arc-node-2     arc-node-3
   (boot node)    (validator)    (validator)    (validator)
   :9090 RPC      :9090 RPC      :9090 RPC      :9090 RPC
   :9091 P2P <--> :9091 P2P <--> :9091 P2P <--> :9091 P2P
```

Each node runs:
- **arc-node** binary as a systemd service
- **UFW firewall** allowing only ports 22 (SSH), 9090 (RPC), 9091 (P2P)
- **Ubuntu 24.04** on Hetzner CAX41 (ARM64, 16 vCPU, 32 GB RAM)

## Prerequisites

1. **Hetzner Cloud account** with API token
2. **hcloud CLI** installed and authenticated:
   ```bash
   brew install hcloud               # macOS
   hcloud context create arc-testnet # set API token
   ```
3. **SSH key** registered in Hetzner Cloud:
   ```bash
   hcloud ssh-key create --name default --public-key-from-file ~/.ssh/id_ed25519.pub
   ```
4. **jq** installed (`brew install jq`)
5. **arc-node binary** published to GitHub Releases (update the URL in `cloud-init.yml`)

## Quick Start

```bash
cd deploy/

# Provision all 4 nodes (takes ~2 minutes)
make setup

# Check status
make status

# Watch continuously
make watch

# SSH into a node
make ssh NODE=0

# View logs
make logs NODE=1

# Tear down everything
make teardown
```

## File Structure

```
deploy/
  README.md              # This file
  Makefile               # Convenience targets
  cloud-init.yml         # Hetzner VM bootstrap (packages, firewall, user)
  arc-node.service       # Systemd unit file (reference copy)
  setup-testnet.sh       # Provisioning script (creates VMs, deploys configs)
  monitor.sh             # Health check / monitoring script
  teardown.sh            # Cleanup script (deletes all VMs)
  .node-ips              # Auto-generated: one IP per line (gitignored)
  config/
    genesis.toml         # Genesis state (shared by all nodes)
    node-0.toml          # Validator 0 config (boot node)
    node-1.toml          # Validator 1 config
    node-2.toml          # Validator 2 config
    node-3.toml          # Validator 3 config
```

## Step-by-Step Guide

### 1. Publish the Binary

Before deploying, build and publish `arc-node` for linux-arm64:

```bash
# Cross-compile for ARM64 (from your dev machine)
cargo build --release --target aarch64-unknown-linux-gnu -p arc-node

# Or build on an ARM64 machine directly
cargo build --release -p arc-node
```

Upload the binary to GitHub Releases, then update the download URL in `cloud-init.yml`:

```yaml
BINARY_URL="https://github.com/FerrumVir/arc-chain/releases/download/v0.1.0/arc-node-linux-arm64"
```

### 2. Configure (Optional)

Default settings work out of the box. Customize if needed:

| Variable        | Default       | Description                         |
|-----------------|---------------|-------------------------------------|
| `SERVER_TYPE`   | `cax41`       | Hetzner server type (ARM64)         |
| `DATACENTER`    | `ash-dc1`     | Datacenter (Ashburn, VA)            |
| `IMAGE`         | `ubuntu-24.04`| OS image                            |
| `SSH_KEY_NAME`  | `default`     | SSH key name in Hetzner             |

Override via environment:

```bash
SERVER_TYPE=cax31 DATACENTER=fsn1-dc14 ./setup-testnet.sh
```

### 3. Provision

```bash
make setup
```

This will:
1. Create 4 CAX41 servers in Hetzner Cloud
2. Wait for cloud-init to install dependencies and configure firewalls
3. Deploy `config.toml` (with real peer IPs) and `genesis.toml` to each node
4. Start the `arc-node` systemd service on all nodes
5. Verify health of all nodes
6. Save node IPs to `.node-ips` for other scripts

### 4. Verify

```bash
# Quick health check
make status

# Detailed health for one node
make health NODE=0

# Direct curl
curl http://<NODE_IP>:9090/health | jq .
```

### 5. Monitor

```bash
# One-shot status table
make status

# Continuous monitoring (refreshes every 10s)
make watch

# JSON output (for scripting)
./monitor.sh --json
```

### 6. Operate

```bash
# SSH into any node
make ssh NODE=2

# Tail logs
make logs NODE=0

# Restart a single node
make restart NODE=1

# Restart all nodes
make restart-all
```

### 7. Tear Down

```bash
make teardown
```

This deletes all 4 servers and removes the local `.node-ips` file. Requires confirmation (use `--force` to skip).

## Genesis Configuration

The testnet genesis includes:

| Account     | Address (blake3)     | Balance         |
|-------------|---------------------|-----------------|
| Faucet      | `2d3aded...92e213`  | 1,000,000,000   |
| Validator 0 | `48fc721...88652b`  | 100,000         |
| Validator 1 | `ab13bed...5e25d0`  | 100,000         |
| Validator 2 | `e1e0e81...8d7505`  | 100,000         |
| Validator 3 | `0c389a7...09e7eb`  | 100,000         |

All 4 validators start with 5,000,000 stake.

## Network Ports

| Port | Protocol | Purpose            |
|------|----------|--------------------|
| 22   | TCP      | SSH access         |
| 9090 | TCP      | JSON-RPC API       |
| 8545 | TCP      | Ethereum JSON-RPC  |
| 9091 | TCP      | P2P gossip         |

Only 22, 9090, and 9091 are opened by the firewall. Port 8545 (Eth RPC) is bound but not exposed through UFW by default. To enable it:

```bash
ssh root@<IP> ufw allow 8545/tcp
```

## Troubleshooting

**Node won't start:**
```bash
ssh root@<IP> journalctl -u arc-node -n 50 --no-pager
ssh root@<IP> systemctl status arc-node
```

**Cloud-init failed:**
```bash
ssh root@<IP> cat /var/log/cloud-init-output.log
```

**Binary not found:**
```bash
ssh root@<IP> ls -la /usr/local/bin/arc-node
ssh root@<IP> file /usr/local/bin/arc-node  # verify architecture
```

**Peers not connecting:**
```bash
# Check firewall
ssh root@<IP> ufw status

# Check P2P port is listening
ssh root@<IP> ss -tlnp | grep 9091

# Verify peer IPs in config
ssh root@<IP> cat /etc/arc/config.toml
```

**Reset node state:**
```bash
ssh root@<IP> "systemctl stop arc-node && rm -rf /var/lib/arc/data/* && systemctl start arc-node"
```

## Cost

Hetzner CAX41 (ARM64, 16 vCPU, 32 GB RAM):
- ~15.90 EUR/month per server
- **~63.60 EUR/month** for the full 4-node testnet
- Billed hourly when servers exist (even if stopped)

Tear down when not in use to save costs.
