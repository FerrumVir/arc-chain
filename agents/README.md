# ARC Chain Example AI Agents

Demonstration agents showcasing ARC Chain's AI-native capabilities: on-chain model deployment, inference via precompiles, agent-to-agent communication, and zero-fee settlements.

## Agents

### 1. Sentiment Agent (`sentiment_agent.rs`)

Binary text sentiment classifier running entirely on-chain.

- Builds a 3-layer Dense neural net (128 -> 64 -> 2) with ReLU + Softmax
- Serializes model weights using `NeuralNet::to_bytes()` binary format
- Registers on-chain via `RegisterAgent` TX (0x09)
- Processes inference requests through the inference precompile (0x0A)
- Settles each request via zero-fee `Settle` TX (0x02)

**Model:** ~10K parameters (128*64 + 64 + 64*2 + 2 = 8,386 weights/biases)

```
cargo run --bin sentiment-agent
```

### 2. Price Oracle Agent (`oracle_agent.rs`)

Price feed oracle with Tier 2 optimistic attestation.

- Simulates external price data for ARC, ETH, BTC, USDC, SOL
- Updates the `OracleRegistry` (precompile 0x04)
- Attests to prices via `InferenceAttestation` TX (0x16) with bond collateral
- Supports fraud-proof challenges via `InferenceChallenge` TX (0x17)

```
cargo run --bin oracle-agent
```

### 3. Router Agent (`router_agent.rs`)

Meta-agent that routes inference requests to the optimal provider.

- Maintains a routing table of available inference providers
- Supports three strategies: Cheapest, Fastest, Balanced
- Collects a routing fee (10 ARC) per request via `Settle` TX
- Forwards provider fees via agent-to-agent `Settle` TXs
- Demonstrates multi-agent coordination patterns

```
cargo run --bin router-agent
```

## Transaction Types Used

| TX Type | Code | Usage |
|---------|------|-------|
| RegisterAgent | 0x09 | Agent registration |
| Settle | 0x02 | Zero-fee payment settlement |
| InferenceAttestation | 0x16 | Tier 2 optimistic price attestation |

## Precompiles Used

| Address | Name | Usage |
|---------|------|-------|
| 0x04 | Price Oracle | Price feed reads |
| 0x0A | AI Inference | Model inference execution |

## Building

```bash
cargo build -p arc-agents
```

## Running All Agents

```bash
cargo run --bin sentiment-agent
cargo run --bin oracle-agent
cargo run --bin router-agent
```
