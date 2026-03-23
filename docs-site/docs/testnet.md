---
title: Testnet
sidebar_position: 6
id: testnet
---

# ARC Chain Testnet

The testnet is live. Join with the desktop app or one terminal command.

## Join (Desktop App)

Download the ARC Node app, open it, click "Launch Node." Your node connects to the testnet automatically.

| Platform | Download |
|----------|----------|
| macOS (Apple Silicon) | Coming soon |
| macOS (Intel) | Coming soon |
| Windows | Coming soon |
| Linux | Coming soon |

## Join (Terminal)

```bash
curl -sSf https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-node.sh | bash
```

Done. Your node is running, connected to testnet seed nodes, and earning rewards.

## What Your Node Does

- Validates transactions and earns ARC rewards
- Helps secure the network
- Uses about 2 MB/s bandwidth, 200 MB RAM, 5% CPU or less
- Works on any Mac, PC, or Linux machine

## View the Network

**[ARC Scan →](https://dist-three-amber-39.vercel.app)** — live block explorer showing all testnet activity

## Get Test Tokens

```bash
curl -X POST http://localhost:9090/faucet/claim \
  -H "Content-Type: application/json" \
  -d '{"address": "YOUR_ADDRESS"}'
```

## Hardware Requirements

| Role | Hardware | Stake |
|------|----------|-------|
| Observer | Any Mac, PC, or Raspberry Pi | 50,000 ARC |
| Verifier | Mac Mini or desktop PC | 500,000 ARC |
| Proposer | Server with GPU | 5,000,000 ARC |

Most community members run as Observers. The desktop app handles this automatically.

## Testnet Parameters

| Parameter | Value |
|-----------|-------|
| Chain ID | 0x415243 ("ARC") |
| Finality | ~24ms (2-round DAG commit, ~12ms/round) |
| Consensus | DAG (Mysticeti-inspired) |
| Block explorer | [ARC Scan](https://dist-three-amber-39.vercel.app) |
| Faucet | Built into every node at `/faucet/claim` |

## For Developers

If you want to build on ARC Chain (deploy contracts, run agents, use the SDK), see:

- [Deploy an AI Agent](./agents/deploy-agent.md)
- [RPC API Reference](./rpc-api.md)
- [Python SDK](./sdk/python.md)
