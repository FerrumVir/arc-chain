![Rust](https://img.shields.io/badge/Rust-99%2C000%2B_LOC-orange)
![Tests](https://img.shields.io/badge/tests-1%2C231_passing-brightgreen)
![License](https://img.shields.io/badge/license-BUSL--1.1-blue)
![Inference](https://img.shields.io/badge/inference-76ms%2Ftok_deterministic-purple)
![Testnet](https://img.shields.io/badge/testnet-live-green)

# ARC Chain - Testnet

**The blockchain for the agentic economy.**

On-chain AI inference was previously thought impossible because floating-point arithmetic produces different results on different hardware. We solved it. The ARC engine uses pure integer arithmetic to achieve bitwise identical inference outputs across every chip, every architecture, every device on earth. This means AI outputs can be cryptographically verified for the first time. Agents can trust each other. Inference can be proven. The agentic economy can be built.

Read the paper: [On the Foundations of Trustworthy Artificial Intelligence](papers/foundations-trustworthy-ai.pdf)

99,000+ lines of Rust. Built from scratch.

---

---

## Why Build on ARC

| Feature | ARC | Everyone Else |
|---------|-----|---------------|
| **On-chain AI inference** | 76 ms/token, deterministic, identical on every chip on earth | Does not exist. Previously thought impossible. |
| **Verified inference** | Cryptographic proof that a specific model produced a specific output. Proven at 7B parameters, 700x larger than any prior ZK-verified model. Attested through multi-node DAG consensus. | No chain can verify AI inference. |
| **Agent settlements** | Zero fees forever. Agents are first-class citizens with dedicated transaction types. | No chain offers zero-fee agent transactions. |
| **Smart contracts** | Both EVM (Solidity) and WASM (Rust, C, Go) natively. Pick your stack. | One or the other, not both. |
| **Quantum resistant** | Falcon-512 + ML-DSA implemented and shipping. Not a roadmap item. | No production chain has post-quantum signatures. |
| **Multi-node TPS** | 33,230 measured with real DAG consensus over real QUIC networking. Throughput increases with more validators (DAG consensus scales horizontally). | Ethereum: ~15 TPS. Solana: ~4,000 non-vote TPS sustained. |
| **Finality** | ~200ms, 2-round DAG commit | Ethereum: ~12 min. Solana: ~400ms. |
| **MEV protection** | BLS threshold encrypted mempool. Transactions encrypted until block is committed. | Exposed or partially mitigated. |
| **Signatures** | 5 algorithms: Ed25519, Falcon-512, BLS12-381, ML-DSA, secp256k1 | 1 or 2 options. |
| **ZK proofs** | Circle STARKs (Stwo). No trusted setup. Post-quantum secure. Verified at 700x the scale of any prior ZK-ML system. | SNARKs requiring trusted setup, limited to small models. |

---

## Inference Speed

The ARC engine runs neural network inference in pure integer arithmetic. No floating-point operations. The output is bitwise identical on every CPU, GPU, and architecture on earth.

| Backend | Speed | Deterministic | Verified |
|---------|-------|---------------|----------|
| ARC engine (GPU) | **76 ms/token** | Yes, all platforms | Hash + STARK |
| ARC engine (CPU) | **139 ms/token** | Yes, all platforms | Hash + STARK |
| Standard float (candle Q4) | 175 ms/token | No | No |

The deterministic engine is **2.3x faster** than floating-point on GPU. Not slower. Faster.

Every inference produces an on-chain `InferenceAttestation` with the model hash, input hash, and output hash. Anyone can independently verify by re-executing on any hardware and comparing hashes.

Read the paper: [On the Foundations of Trustworthy Artificial Intelligence](papers/foundations-trustworthy-ai.pdf)

---

## Measured Performance

| Metric | Value | Conditions |
|--------|-------|------------|
| Single-node peak TPS | **183,000** | CPU verify + sequential exec, M2 Ultra |
| Multi-node sustained TPS | **33,230** | 2 validators, real QUIC, real DAG consensus |
| Peak TPS | **350,000** | 1-second burst window |
| Commit rate | **100%** | 500K/500K transactions committed |
| GPU Ed25519 verify | **379,000/sec** | Metal compute shader |
| Inference (GPU) | **76 ms/token** | Deterministic INT8, M2 Ultra |
| Inference (CPU) | **139 ms/token** | Deterministic INT8, M2 Ultra |
| DAG finality | **~200ms** | 2-round commit rule |

All numbers measured on Apple M2 Ultra (24 cores, 64 GB).

---

## Quick Start

### Prerequisites

- Rust nightly (`rustup default nightly`)
- ~2 GB disk for build, ~4 GB with model

### See it live right now (zero install)

The testnet is running across 4 nodes on 2 continents. Try it:

```bash
# Chain stats from a live node
curl http://140.82.16.112:9090/stats

# Node health, peers, uptime
curl http://140.82.16.112:9090/health

# Chain info with GPU status
curl http://140.82.16.112:9090/info
```

### Join the testnet

```bash
git clone https://github.com/FerrumVir/arc-chain.git
cd arc-chain
./scripts/join-testnet.sh
```

Or use the Makefile:

```bash
make join          # Join testnet
make inference     # Join with inference (downloads TinyLlama 1.1B)
make stats         # Check live chain stats
make health        # Check live node health
make test          # Run 1,231 tests
make explorer      # Open block explorer
make faucet        # Run testnet faucet
```

### What you'll see

```bash
# Live chain stats
curl http://localhost:9090/stats
# {"block_height":245,"total_accounts":100,"total_transactions":356}

# Run deterministic inference
curl -X POST http://localhost:9090/inference/run \
  -H 'Content-Type: application/json' \
  -d '{"input":"[INST] What is 2+2? [/INST]","max_tokens":16}'
# {"output":"Sure! The answer is 2+2 = 4.","output_hash":"0x...","ms_per_token":76}
# That output_hash is identical on ARM, x86, and GPU. Verify it yourself.

# View all inference attestations on-chain
curl http://localhost:9090/inference/attestations
```

### Get testnet tokens

```bash
curl -X POST http://localhost:9090/faucet/claim \
  -H 'Content-Type: application/json' \
  -d '{"address":"0x<your-address>"}'

# Or run the faucet with a web UI
cd faucet && cargo run --release
```

### Deploy a smart contract

Write Solidity (EVM via revm 19) or Rust/C/Go (WASM via Wasmer 6.0). Both VMs run natively. Choose whichever fits your stack.

### Run AI agents

Three agent types ship with the chain. All agent settlements are zero-fee.

```bash
cd agents && cargo run --release
```

- **Oracle agent** - submits inference attestations with economic bonds
- **Router agent** - routes inference requests to capable nodes
- **Sentiment agent** - on-chain sentiment analysis via deterministic inference

Agents register on-chain via `RegisterAgent` (0x07) and settle via `Settle` (0x06) at zero cost. ARC is built for agents.

---

## Dual VM: EVM + WASM

Deploy in whichever runtime fits your project:

| Runtime | Language | Engine | Use Case |
|---------|----------|--------|----------|
| **EVM** | Solidity, Vyper | revm 19 | Ethereum-compatible dApps, DeFi, existing tooling |
| **WASM** | Rust, C, C++, Go, AssemblyScript | Wasmer 6.0 | High-performance compute, custom logic, ML models |

Both VMs have access to 11 native precompiles: BLAKE3, Ed25519, VRF, Oracle, Merkle proofs, BlockInfo, Identity, Falcon-512, ZK-verify, AI-inference (0x0A), BLS-verify.

---

## Transaction Types (24)

| Type | Code | Description |
|------|------|-------------|
| Transfer | `0x01` | Send ARC between accounts |
| Stake | `0x02` | Stake ARC to become a validator |
| Unstake | `0x03` | Begin unstaking with cooldown |
| Deploy | `0x04` | Deploy WASM or EVM smart contract |
| Call | `0x05` | Call a deployed contract |
| **Settle** | **`0x06`** | **Zero-fee AI agent settlement** |
| **RegisterAgent** | **`0x07`** | **Register an AI agent on-chain** |
| Governance | `0x08` | Submit or vote on governance proposal |
| Bridge Lock/Unlock | `0x09-0x0B` | Cross-chain bridge operations |
| Channel Open/Close | `0x0C-0x0E` | Payment channel lifecycle |
| ShardProof | `0x15` | Submit STARK proof of computation |
| **InferenceAttestation** | **`0x16`** | **Attest to inference result with bond** |
| **InferenceChallenge** | **`0x17`** | **Challenge an attestation (dispute)** |
| InferenceRegister | `0x18` | Register validator inference capabilities |
| + 10 more | | Batch, social recovery, state rent, etc. |

Agent transactions (bold) are unique to ARC.

## Cryptographic Signatures

Five signature algorithms, production ready:

| Algorithm | Use | Speed |
|-----------|-----|-------|
| **Ed25519** | Primary signing | 118K sigs/sec |
| **Falcon-512** | Post-quantum (NIST) | Production |
| **BLS12-381** | Aggregate N sigs into 1 verify | Production |
| **ML-DSA** | Post-quantum (NIST Dilithium) | Production |
| **ECDSA secp256k1** | Ethereum compatibility | Production |

Your contracts and agents can use any of these. Post-quantum ready today.

## Smart Contract Standards

| Standard | Description |
|----------|-------------|
| **ARC20** | Fungible token (ERC-20 equivalent) |
| **ARC721** | NFT (ERC-721 equivalent) |
| **ARC1155** | Multi-token (ERC-1155 equivalent) |
| **UUPSProxy** | Upgradeable proxy pattern |
| **ARCStaking** | Staking with tier system |
| **ArcBridge** | Cross-chain bridge |
| **ArcStateRoot** | State root commitments for rollups/L2s |

## Inference Tiers

Three tiers of AI inference, each with different trust/cost tradeoffs:

| Tier | Execution | Verification | Use Case |
|------|-----------|-------------|----------|
| **Tier 1** | On-chain (precompile 0x0A) | Every validator re-executes | Small models, full trust |
| **Tier 2** | Off-chain, optimistic | Fraud proofs + economic bonds | Large models, fast |
| **Tier 3** | Off-chain, STARK-proven | Cryptographic proof | Maximum trust |

---

## ARC Token

ARC exists today as an ERC-20 on Ethereum: [`0x672fdba7055bddfa8fd6bd45b1455ce5eb97f499`](https://etherscan.io/token/0x672fdba7055bddfa8fd6bd45b1455ce5eb97f499)

When ARC Chain mainnet launches, ERC-20 holders will migrate to native ARC tokens via a bridge contract. Fixed supply of 1.03B ARC. No tokens are ever burned. No inflation.

On testnet, use the faucet to get test tokens and start building now.

---

## RPC API (34 endpoints)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Node health, peers, uptime |
| GET | `/stats` | Block height, TPS, total transactions |
| GET | `/info` | Chain info, GPU status |
| GET | `/block/latest` | Latest block |
| GET | `/block/{height}` | Block by height |
| GET | `/blocks?from=&to=&limit=` | Paginated block list |
| GET | `/account/{address}` | Account state |
| GET | `/account/{address}/txs` | Transaction history |
| POST | `/tx/submit` | Submit signed transaction |
| POST | `/tx/submit_batch` | Batch submission |
| GET | `/tx/{hash}` | Transaction with receipt |
| GET | `/tx/{hash}/proof` | Merkle inclusion proof |
| GET | `/validators` | Current validator set |
| GET | `/agents` | Registered AI agents |
| POST | `/inference/run` | Run inference (returns output + hash + ms/token) |
| GET | `/inference/attestations` | All on-chain attestations |
| POST | `/faucet/claim` | Claim testnet tokens |
| GET | `/faucet/status` | Faucet status |
| GET | `/sync/snapshot` | State sync for new nodes |
| POST | `/contract/{address}/call` | Call a smart contract |
| GET | `/channel/{id}/state` | Payment channel state |
| POST | `/eth` | ETH JSON-RPC (blockNumber, getBalance, call, estimateGas, getLogs) |

---

## Codebase

**99,600+ lines of Rust** across 14 crates with **1,231 tests**.

| Crate | LOC | Tests | What It Does |
|-------|-----|-------|-------------|
| `arc-types` | 14,490 | 264 | 24 transaction types, blocks, accounts, governance, staking, bridge, inference |
| `arc-state` | 13,203 | 147 | DashMap state, Jellyfish Merkle Tree, WAL, BlockSTM parallel execution, GPU cache |
| `arc-crypto` | 11,680 | 220 | Ed25519, secp256k1, BLS, BLAKE3, Falcon-512, ML-DSA, VRF, STARK prover |
| `arc-vm` | 8,439 | 145 | Wasmer WASM + revm EVM, gas metering, 11 precompiles, AI inference oracle |
| `arc-node` | 8,424 | 61 | Block production, RPC (34 endpoints), consensus manager, STARK proofs |
| `arc-consensus` | 7,971 | 137 | DAG consensus, 2-round finality, slashing, VRF, epoch transitions |
| `arc-bench` | 5,336 | - | 10 benchmark binaries |
| `arc-gpu` | 5,250 | 45 | Metal/WGSL Ed25519 batch verify (379K/sec), GPU memory, buffer pool |
| `arc-net` | 2,355 | 26 | QUIC transport, shred propagation, FEC, gossip, peer exchange |
| `arc-mempool` | 876 | 17 | Lock-free queue, deduplication, BLS threshold encrypted mempool |
| `arc-inference` | 620 | 17 | INT4 runtime, VRF committee selection, EIP-1559 inference gas lane |
| `arc-channel` | 480 | 10 | Off-chain payment channels, BLAKE3 state commitments |
| `arc-cli` | 660 | - | CLI: keygen, RPC, transaction submission |

Plus: Python SDK (2,688 LOC), TypeScript SDK (2,011 LOC), Solidity contracts (1,944 LOC), block explorer.

---

## Staking (Coming to Mainnet)

Staking is implemented in the protocol but not yet active on testnet. Right now, anyone can:
- Run a node and join the testnet
- Deploy smart contracts (EVM or WASM)
- Run deterministic inference
- Test all 24 transaction types
- Run AI agents with zero-fee settlements
- Use the faucet for test tokens

---

## Disclaimer

ARC Chain is in active development. This is a testnet. Do not use real funds. The software is provided as-is with no warranty. Smart contracts deployed on testnet may not persist across upgrades. The ARC token economics described here reflect current design and may change before mainnet.

---

## License

Open source in spirit. All source code is public. Read it, learn from it, build with it.

**What you can do:**

- Use ARC for any project if your org is under $10M revenue. Full production rights. No approval needed.
- Build anything on the ARC chain at any scale, any revenue. Contracts, tokens, agents, L2s, rollups, subnets. If it runs on ARC, it's free forever.
- Join the ARC ecosystem. Crypto projects of any size, any market cap. If you want to build on ARC, deploy on ARC, or integrate with ARC, you are welcome. We want you here.
- Run a validator, node, or inference provider. Always free.
- Use it for research, education, personal projects. Always free.
- Fork it, modify it, experiment with it.

If your org is over $10M revenue and you want to use the code outside the ARC ecosystem, reach out for a commercial license (starts at $50K/year). We're friendly about it: tj@arc.ai

**What you can't do:**

- Fork this codebase and launch a competing L1 blockchain
- Extract components (consensus, inference, crypto) to use in a competing network
- Repackage or rebrand this code as your own chain

I built this solo from scratch, every line. I just don't want to see it taken and passed off as someone else's work. Everything else is fair game. If you want to work together on something, I'm open to it: tj@arc.ai

Becomes fully open source (Apache 2.0) on March 25, 2030. See [LICENSE](LICENSE) for details.
