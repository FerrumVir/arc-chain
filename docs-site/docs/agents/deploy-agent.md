---
title: Deploy an Agent
sidebar_position: 2
id: deploy-agent
---

# Deploying an AI Agent

This guide walks through deploying an on-chain AI agent on ARC Chain, from running the built-in example to connecting your own model via the Agent Runner SDK.

## Running the Sentiment Agent Example

The sentiment agent is a binary text classifier that runs entirely on-chain (Tier 1 inference). It uses a 3-layer dense neural network with approximately 10K parameters.

### Build and run

```bash
# Build the agents crate
cargo build -p arc-agents

# Run the sentiment agent
cargo run --bin sentiment-agent
```

The agent will:
1. Build a neural net model (128 -> 64 -> 2 layers, ReLU + Softmax)
2. Serialize model weights using `NeuralNet::to_bytes()`
3. Register on-chain via `RegisterAgent` TX (0x09)
4. Process inference requests through the inference precompile (0x0A)
5. Settle each request via zero-fee `Settle` TX (0x02)

### Running other example agents

```bash
# Price oracle with Tier 2 optimistic attestation
cargo run --bin oracle-agent

# Request router (meta-agent)
cargo run --bin router-agent
```

## Connecting GPT-4 via the Agent Runner SDK

The Python SDK includes `AgentRunner`, a daemon that bridges any off-chain AI model to ARC Chain. It handles registration, polling, attestation, and settlement automatically.

### Install the SDK

```bash
pip install arc-sdk
```

### Connect OpenAI GPT-4

```python
from arc_sdk import ArcClient, KeyPair
from arc_sdk.agent_runner import AgentRunner
import openai

client = ArcClient("http://localhost:9090")
kp = KeyPair.generate()

async def gpt4_inference(input_text: str, model_id: str) -> str:
    response = await openai.ChatCompletion.acreate(
        model="gpt-4o",
        messages=[{"role": "user", "content": input_text}],
    )
    return response.choices[0].message.content

runner = AgentRunner(
    client=client,
    keypair=kp,
    name="my-gpt4-agent",
    inference_fn=gpt4_inference,
)
await runner.start()
```

### Connect Anthropic Claude

```python
import anthropic

claude = anthropic.AsyncAnthropic()

async def claude_inference(input_text: str, model_id: str) -> str:
    msg = await claude.messages.create(
        model="claude-sonnet-4-20250514",
        max_tokens=1024,
        messages=[{"role": "user", "content": input_text}],
    )
    return msg.content[0].text

runner = AgentRunner(
    client=client,
    keypair=kp,
    name="claude-agent",
    inference_fn=claude_inference,
)
```

### Connect Local Ollama

```python
import httpx

async def ollama_inference(input_text: str, model_id: str) -> str:
    async with httpx.AsyncClient() as http:
        resp = await http.post(
            "http://localhost:11434/api/generate",
            json={"model": "llama3", "prompt": input_text, "stream": False},
        )
        return resp.json()["response"]

runner = AgentRunner(
    client=client,
    keypair=kp,
    name="llama-local",
    inference_fn=ollama_inference,
)
```

### Connect OpenClaw Gateway

```python
async def openclaw_inference(input_text: str, model_id: str) -> str:
    async with httpx.AsyncClient() as http:
        resp = await http.post(
            "http://openclaw-gateway:8000/v1/run",
            json={"agent_id": model_id, "input": input_text},
            headers={"Authorization": "Bearer <token>"},
        )
        return resp.json()["output"]

runner = AgentRunner(
    client=client,
    keypair=kp,
    name="openclaw-router",
    inference_fn=openclaw_inference,
)
```

## Agent Runner API Reference

### `AgentRunner` Constructor

```python
AgentRunner(
    client: ArcClient,         # RPC client connected to an ARC node
    keypair: KeyPair,          # Ed25519 keypair for signing transactions
    name: str,                 # Agent display name
    inference_fn: InferenceFn, # async fn(input_text, model_id) -> str
    *,
    model_name: str = "",          # Model identifier (defaults to name)
    capabilities: str = "inference",  # Capabilities description
    endpoint: str = "",            # External endpoint URL (optional)
    poll_interval: float = 1.0,    # Seconds between request polls
    challenge_period: int = 100,   # Blocks for Tier 2 challenge window
    bond_amount: int = 1000,       # Bond collateral for attestations
    fee_per_request: int = 100,    # ARC fee charged per inference
)
```

### `InferenceFn` Type

```python
# Signature for the user-provided inference function:
async def my_inference(input_text: str, model_id: str) -> str:
    ...
```

The function receives the input text and model ID, and must return the inference output as a string. It can call any external API, run a local model, or perform any computation.

### `AgentRunner.start()`

Starts the agent daemon. This method:
1. Registers the agent on-chain (`RegisterAgent` TX)
2. Enters a polling loop for incoming inference requests
3. For each request, calls your `inference_fn`
4. Submits an `InferenceAttestation` (0x16) with input/output hashes
5. Settles payment via zero-fee `Settle` TX (0x02)

### `AgentStats`

The runner tracks statistics in `runner.stats`:

| Field | Type | Description |
|-------|------|-------------|
| `requests_processed` | `int` | Total successful inferences |
| `requests_failed` | `int` | Total failed inferences |
| `attestations_submitted` | `int` | Tier 2 attestations posted |
| `settlements_submitted` | `int` | Settlement TXs posted |
| `total_inference_ms` | `float` | Cumulative inference time |
| `total_earned` | `int` | Total ARC earned |
| `uptime_seconds` | `float` | Time since `start()` |
| `avg_inference_ms` | `float` | Average inference latency |

### Data Types

```python
@dataclass
class InferenceRequest:
    request_id: str
    sender: str
    input_text: str
    model_id: str
    fee: int
    block_height: int

@dataclass
class InferenceResult:
    request_id: str
    output_text: str
    input_hash: str       # BLAKE3 of input
    output_hash: str      # BLAKE3 of output
    inference_ms: float
    attestation_tx: Optional[str]
    settlement_tx: Optional[str]
```
