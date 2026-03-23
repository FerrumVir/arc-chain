# ARC Chain Testnet

## Quick Join (Non-Technical)

Download and run the ARC Node desktop app (coming soon) or:

```bash
curl -sSf https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-node.sh | bash
```

## Manual Join

```bash
# Clone and build
git clone https://github.com/FerrumVir/arc-chain.git
cd arc-chain && cargo build --release

# Start with testnet seeds
cargo run --release -p arc-node -- \
    --seeds-file testnet/seeds.txt \
    --validator-seed "$(openssl rand -hex 16)"
```

## Create Your Own Testnet

```bash
# Generate a 4-validator testnet
bash scripts/create-testnet.sh 4

# Start seed node
cargo run --release -p arc-node -- --config testnet/validator-0.toml

# On other machines, start validators pointing to seed node
cargo run --release -p arc-node -- --config testnet/validator-1.toml --peers SEED_IP:9091
```

## Monitor

```bash
bash scripts/monitor-testnet.sh localhost:9090 localhost:9190 localhost:9290 localhost:9390
```

## Testnet Goals

| Day | Target | How to Verify |
|-----|--------|--------------|
| 1 | 4+ nodes connected, producing blocks | `curl localhost:9090/health` shows peers > 0 |
| 2 | Sustained 10K+ TPS across network | `curl localhost:9090/stats` shows tps > 10000 |
| 3 | AI agent deployed and running inference | Run sentiment-agent against testnet RPC |
| 7 | No crashes, no forks, no stalls for 7 days | Monitor dashboard shows continuous operation |
| 14 | 10+ community nodes joined | `/validators` endpoint shows 10+ validators |
| 30 | Bridge tested (ETH testnet ↔ ARC testnet) | Bridge relayer processes test locks |

## Testnet Parameters

| Parameter | Value |
|-----------|-------|
| Chain ID | 0x415243 ("ARC") |
| Block time | ~12ms/round (finality ~24ms, 2-round DAG) |
| Consensus | DAG (Mysticeti-inspired), 2-round finality |
| Min stake (Observer) | 50,000 ARC |
| Min stake (Verifier) | 500,000 ARC |
| Min stake (Proposer) | 5,000,000 ARC |
| Faucet | Available at node /faucet endpoint |
| Explorer | http://localhost:3100 (run from /explorer) |
