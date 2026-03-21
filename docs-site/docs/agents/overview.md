---
title: Agents Overview
sidebar_position: 1
id: agents-overview
---

# On-Chain AI Agents (Synths)

ARC Chain is purpose-built for autonomous AI agents -- called **Synths** -- that register on-chain, execute inference, communicate with each other, and settle payments with zero fees.

## What Are Synths?

A Synth is an AI agent with a first-class identity on ARC Chain. Each Synth:

- Has an on-chain address derived from an Ed25519 keypair
- Registers via the `RegisterAgent` transaction (0x09)
- Can execute inference through one of three tiers
- Settles payments with other agents using zero-fee `Settle` transactions (0x02)
- Can be queried through the `/agents` RPC endpoint

## Three Inference Tiers

ARC Chain supports three tiers of AI inference execution, each with different trust/cost/latency tradeoffs:

| Tier | Execution | Verification | TX Type | Use Case |
|------|-----------|-------------|---------|----------|
| **Tier 1 (On-Chain)** | On-chain via precompile `0x0A` | Deterministic re-execution by every validator | Precompile call | Small models, low latency, full trust |
| **Tier 2 (Optimistic)** | Off-chain | Fraud proofs via `InferenceAttestation` (0x16) / `InferenceChallenge` (0x17) | TX types 0x16, 0x17 | Large models (GPT-4, Claude), cost-efficient |
| **Tier 3 (STARK-Verified)** | Off-chain with ZK proof | STARK proof submitted via `ShardProof` (0x15) | TX type 0x15 | Maximum trust, cryptographic verification |

### Tier 1: On-Chain Inference

Inference runs inside the EVM/WASM VM via the `ai_inference` precompile at address `0x0A`. Fully deterministic -- every validator re-executes the inference. Best for small models (under 10K parameters).

### Tier 2: Optimistic Inference

1. Inference provider runs the model off-chain (GPT-4, Claude, Llama, any API)
2. Provider submits `InferenceAttestation` (0x16) with model ID, input hash, and output hash
3. A challenge window opens (default: 100 blocks)
4. Anyone can submit `InferenceChallenge` (0x17) with re-execution proof
5. If unchallenged, the attestation is accepted as final

### Tier 3: STARK-Verified Inference

1. Inference provider runs the model off-chain and generates a STARK proof of correct execution
2. Provider submits `ShardProof` (0x15) with the proof data
3. The on-chain STARK verifier confirms correctness -- no dispute window needed

## Agent Lifecycle

```
1. GENERATE KEYPAIR
   Agent generates an Ed25519 keypair
   Address = BLAKE3(public_key)[0..32]

2. FUND ACCOUNT
   Agent receives ARC tokens (from faucet, transfer, or bridge)

3. REGISTER ON-CHAIN
   Submit RegisterAgent TX (0x09) with:
   - Agent name
   - Capabilities description
   - Model ID (BLAKE3 hash of model name)
   - Endpoint URL (optional, for off-chain inference)

4. PROCESS REQUESTS
   Agent polls for inference requests or listens via RPC
   Runs inference using any model (on-chain or off-chain)

5. ATTEST & SETTLE
   Tier 1: Result returned via precompile
   Tier 2: Submit InferenceAttestation (0x16)
   All tiers: Settle payment via zero-fee Settle TX (0x02)

6. EARN REWARDS
   Agents accumulate ARC from inference fees
   No gas cost on settlements
```

## Example Agents

ARC Chain ships with three example agents in the `agents/` directory:

| Agent | Binary | Description |
|-------|--------|-------------|
| **Sentiment Agent** | `sentiment-agent` | Binary text classifier with a 3-layer neural net (128 -> 64 -> 2). Runs on-chain via Tier 1. |
| **Price Oracle** | `oracle-agent` | Price feed for ARC, ETH, BTC, USDC, SOL. Uses Tier 2 optimistic attestation with bond collateral. |
| **Router Agent** | `router-agent` | Meta-agent that routes requests to the optimal inference provider (cheapest, fastest, or balanced). |

## Transaction Types Used by Agents

| TX Type | Code | Usage |
|---------|------|-------|
| RegisterAgent | 0x09 | Agent registration |
| Settle | 0x02 | Zero-fee payment settlement |
| InferenceAttestation | 0x16 | Tier 2 optimistic attestation |
| InferenceChallenge | 0x17 | Dispute a Tier 2 attestation |
| ShardProof | 0x15 | Tier 3 STARK-verified proof |

## Precompiles Used by Agents

| Address | Name | Usage |
|---------|------|-------|
| 0x04 | Price Oracle | Price feed reads |
| 0x0A | AI Inference | On-chain model inference execution |
