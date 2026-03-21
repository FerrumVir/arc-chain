---
title: Quickstart
sidebar_position: 2
id: quickstart
---

# Join ARC Chain

Two ways to join — pick whichever is easier for you.

## Option 1: Desktop App (Easiest)

Download the ARC Node app. Open it. Click "Launch Node." Done.

| Platform | Download |
|----------|----------|
| macOS (Apple Silicon) | Coming soon |
| macOS (Intel) | Coming soon |
| Windows | Coming soon |
| Linux | Coming soon |

The app handles everything — keypair generation, peer discovery, syncing, and earning rewards. No terminal needed.

## Option 2: Terminal (One Command)

```bash
curl -sSf https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-node.sh | bash
```

This installs Rust (if needed), builds the node, generates your validator keypair, and starts it as a background service. Works on macOS and Linux, Intel and ARM.

Your node is running. That's it.

## Verify Your Node

Open ARC Scan to see your node on the network:

**[ARC Scan →](https://dist-three-amber-39.vercel.app)**

Or check from the terminal:

```bash
curl http://localhost:9090/health
```

## Get Test Tokens

Your node comes pre-funded with genesis tokens. To get more:

```bash
curl -X POST http://localhost:9090/faucet/claim \
  -H "Content-Type: application/json" \
  -d '{"address": "YOUR_VALIDATOR_ADDRESS"}'
```

The faucet sends 10,000 test ARC to your address.

## Run an AI Agent

Deploy a sentiment analysis agent that does real on-chain inference:

```bash
cargo run --release -p arc-agents --bin sentiment-agent
```

Or connect GPT-4 to ARC Chain:

```python
from arc_sdk import ArcClient, KeyPair
from arc_sdk.agent_runner import openai_runner

client = ArcClient("http://localhost:9090")
kp = KeyPair.generate()
runner = openai_runner(client, kp, model="gpt-4o")
result = await runner.infer("Analyze this market data...")
```

## Next Steps

- [Architecture](./architecture.md) — how ARC Chain works under the hood
- [Deploy an AI Agent](./agents/deploy-agent.md) — connect any model to ARC Chain
- [RPC API](./rpc-api.md) — all available endpoints
- [Tokenomics](./tokenomics.md) — how rewards work
