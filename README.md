# ARC Chain

High-performance L1 blockchain with verifiable block explorer.

**4.3M+ TPS** on Apple M4 — BLAKE3 hashing, Ristretto commitments, GPU-accelerated execution, WASM smart contracts.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  ARC Chain Node  (Rust)                          :9090      │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌───────────┐  │
│  │arc-crypto│  │ arc-state │  │  arc-vm  │  │  arc-gpu  │  │
│  │ BLAKE3   │  │ StateDB  │  │  Wasmer  │  │   wgpu    │  │
│  │ Ristretto│  │ Merkle   │  │  WASM RT │  │  compute  │  │
│  └──────────┘  └──────────┘  └──────────┘  └───────────┘  │
│  ┌──────────┐  ┌──────────┐  ┌──────────────────────────┐  │
│  │arc-mempl │  │arc-types │  │  arc-node (Axum RPC)     │  │
│  │ tx pool  │  │ core defs│  │  /health /blocks /tx ... │  │
│  └──────────┘  └──────────┘  └──────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
                          │
                   HTTP (port 9090)
                          │
┌─────────────────────────────────────────────────────────────┐
│  ARC Scan Explorer  (Next.js)                    :3100      │
│  ┌──────────────────────────────────────────────────────┐  │
│  │ Client-side BLAKE3 verification  (@noble/hashes)     │  │
│  │ Merkle proof verification   (pure JS, zero WASM)     │  │
│  │ API proxy (/api/chain/*) → Rust node                 │  │
│  │ Mock fallback when node offline                      │  │
│  └──────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

## Crates

| Crate | Purpose |
|-------|---------|
| `arc-crypto` | BLAKE3 hashing, Ristretto commitments, Pedersen proofs |
| `arc-types` | Core types — Transaction, Block, TxBody, Hash256 |
| `arc-state` | StateDB with parallel execution, Merkle trees, proof storage |
| `arc-vm` | Wasmer-based WASM virtual machine |
| `arc-gpu` | wgpu compute shaders for GPU-accelerated operations |
| `arc-mempool` | Transaction pool with priority ordering |
| `arc-node` | Axum HTTP node with RPC endpoints |
| `arc-bench` | Benchmark suite (4.3M TPS on M4) |

## Explorer Features

- **Live block feed** — real-time blocks with tx counts and timing
- **Transaction detail** — every field displayed with cryptographic proof
- **Independent verification** — BLAKE3 recomputation runs in your browser
- **Merkle proof checker** — walk the inclusion proof from leaf to root
- **Standalone verify tool** — paste any hash or proof, verify independently
- **Auto data source detection** — seamless switch between live node and demo data

---

## Quick Start (Development)

### Prerequisites

- Rust 1.85+ (edition 2024)
- Node.js 22+
- Vulkan SDK (for GPU crate)

### Run the node

```bash
cargo run -p arc-node
# Node RPC at http://localhost:9090
# Health check: curl http://localhost:9090/health
```

### Run the explorer

```bash
cd explorer
npm install
npm run dev
# Explorer at http://localhost:3100
```

### Run benchmarks

```bash
cargo run --release -p arc-bench
# Outputs TPS metrics for all transaction types
```

---

## Deployment

### Option A: Docker Compose (Recommended)

The fastest way to deploy. Builds both the Rust node and Next.js explorer in isolated containers.

```bash
# Clone and deploy
git clone <your-repo> arc-chain
cd arc-chain

# Build and start everything
docker compose up -d --build

# Verify
curl http://localhost:9090/health    # Node RPC
open http://localhost:3100           # Explorer

# View logs
docker compose logs -f arc-node
docker compose logs -f explorer

# Run benchmark (separate container)
docker compose run arc-bench
```

**Services:**

| Service | Port | Description |
|---------|------|-------------|
| `arc-node` | 9090 | Rust blockchain node (RPC) |
| `explorer` | 3100 | Next.js block explorer |
| `arc-bench` | — | Benchmark (run manually) |

### Option B: Bare Metal (Linux Server)

Direct install on Ubuntu 22.04/24.04 or Debian 12. Creates systemd services for automatic restart.

```bash
# SSH into your server
ssh root@your-server

# Clone the repo
git clone <your-repo> /opt/arc-chain
cd /opt/arc-chain

# Run the deployment script
bash deploy.sh
```

The script will:
1. Install system dependencies (build-essential, cmake, Vulkan)
2. Install Rust 1.85+ (if not present)
3. Build `arc-node` and `arc-bench` in release mode
4. Install Node.js 22 (if not present)
5. Build the Next.js explorer (production)
6. Create and start systemd services

**After deployment:**

```
Node RPC:    http://your-server:9090
Explorer:    http://your-server:3100
Verify page: http://your-server:3100/verify
```

**Manage services:**

```bash
systemctl status arc-node
systemctl status arc-explorer
journalctl -u arc-node -f
journalctl -u arc-explorer -f
systemctl restart arc-node
systemctl restart arc-explorer
```

### Adding HTTPS (Production)

After deploying, add nginx + Let's Encrypt for HTTPS:

```bash
# Install nginx and certbot
apt install -y nginx certbot python3-certbot-nginx

# Copy the provided nginx config
cp deploy/nginx.conf /etc/nginx/sites-available/arc-chain
ln -s /etc/nginx/sites-available/arc-chain /etc/nginx/sites-enabled/

# Edit the config — replace "your-domain.com" with your actual domain
nano /etc/nginx/sites-available/arc-chain

# Test and reload
nginx -t
systemctl reload nginx

# Get SSL certificate
certbot --nginx -d your-domain.com

# Certbot auto-renews via systemd timer
```

The nginx config (`deploy/nginx.conf`) includes:
- Reverse proxy for explorer (/) and RPC (/rpc/)
- Security headers (X-Frame-Options, CSP, etc.)
- Gzip compression
- Static asset caching (365d for `/_next/static/`)
- Rate limiting zone for public RPC

---

## RPC Endpoints

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Node health check |
| GET | `/stats` | Live stats (TPS, height, total txs) |
| GET | `/block/latest` | Latest block |
| GET | `/block/{height}` | Block by height |
| GET | `/blocks?from=X&to=Y&limit=N` | Paginated block list |
| GET | `/tx/{hash}` | Transaction + receipt + pre_hash_hex |
| GET | `/tx/{hash}/proof` | Full verification bundle |
| GET | `/block/{height}/proofs` | Merkle proofs for all txs in block |
| GET | `/account/{address}/txs` | Transaction history |

### Verification Bundle (`/tx/{hash}/proof`)

```json
{
  "tx_hash": "abc123...",
  "pre_hash_hex": "01deadbeef...",
  "blake3_domain": "ARC-chain-tx-v1",
  "merkle_proof": {
    "leaf": "abc123...",
    "index": 42,
    "siblings": [
      { "hash": "...", "is_left": true },
      { "hash": "...", "is_left": false }
    ],
    "root": "def456..."
  },
  "block_height": 18420003
}
```

---

## Verification Engine

The explorer includes a client-side verification engine (`explorer/src/lib/verify.ts`) that independently recomputes cryptographic hashes in the user's browser.

**How it works:**

1. Fetch a transaction and its `pre_hash_hex` from the RPC
2. Run BLAKE3 derive_key with domain `"ARC-chain-tx-v1"` on the raw bytes
3. Compare the computed hash to the on-chain hash
4. If they match → the data is authentic, not fabricated

**Supported verifications:**

- **BLAKE3 hash** — domain-separated hash verification
- **Compact transfer hash** — reconstruct pre-image from individual fields
- **Merkle inclusion proof** — walk sibling path from leaf to root

**Library:** Uses `@noble/hashes` — pure JavaScript, zero WASM, audited by the Ethereum Foundation. Works identically in SSR and browser.

---

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `ARC_NODE_URL` | `http://127.0.0.1:9090` | Rust node RPC URL (explorer uses this) |
| `PORT` | `3100` | Explorer HTTP port |
| `RUST_LOG` | `info` | Rust node log level |
| `NODE_ENV` | `development` | Next.js environment |

---

## Project Structure

```
arc-chain/
├── crates/
│   ├── arc-bench/          # Benchmark suite
│   ├── arc-crypto/         # BLAKE3, Ristretto, Pedersen
│   ├── arc-gpu/            # wgpu compute shaders
│   ├── arc-mempool/        # Transaction pool
│   ├── arc-node/           # Axum RPC server
│   ├── arc-state/          # StateDB, Merkle trees
│   ├── arc-types/          # Core type definitions
│   └── arc-vm/             # Wasmer WASM runtime
├── explorer/
│   ├── src/
│   │   ├── app/
│   │   │   ├── page.tsx            # Homepage (live feed + verify)
│   │   │   ├── verify/page.tsx     # Standalone verification tool
│   │   │   ├── tx/[hash]/page.tsx  # Transaction detail
│   │   │   ├── block/[height]/     # Block detail
│   │   │   └── api/chain/[...path] # RPC proxy
│   │   ├── components/
│   │   │   ├── Header.tsx          # Nav + data source badge
│   │   │   ├── VerifyButton.tsx    # Reusable verify button
│   │   │   ├── VerificationPanel.tsx
│   │   │   └── DataSourceBadge.tsx # Live/Demo indicator
│   │   └── lib/
│   │       ├── verify.ts           # BLAKE3 + Merkle verification
│   │       ├── chain-client.ts     # RPC client + mock fallback
│   │       └── mock-data.ts        # Demo data generators
│   ├── Dockerfile
│   └── package.json
├── deploy/
│   └── nginx.conf          # Production nginx config
├── Dockerfile.node          # Rust node Docker image
├── docker-compose.yml       # Full stack orchestration
├── deploy.sh               # Bare metal deployment script
└── Cargo.toml              # Workspace manifest
```

---

## License

BUSL-1.1 — Business Source License 1.1
