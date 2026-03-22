---
title: Python SDK
sidebar_position: 1
id: sdk-python
---

# Python SDK

The `arc_sdk` Python package provides a complete client for interacting with the ARC Chain blockchain, including transaction building, Ed25519 signing, ABI encoding, and an AI agent runner.

**Version**: 0.2.0

## Installation

```bash
pip install arc-sdk
```

Or install from source:

```bash
cd sdks/python
pip install -e .
```

## Quick Start

```python
from arc_sdk import ArcClient, KeyPair, TransactionBuilder

# Connect to a node
client = ArcClient("http://localhost:9090")

# Generate a key pair
kp = KeyPair.generate()
print(f"Address: {kp.address()}")

# Build and sign a transfer
tx = TransactionBuilder.transfer(
    from_addr=kp.address(),
    to_addr="0" * 64,
    amount=1000,
)
signed_tx = TransactionBuilder.sign(tx, kp)

# Submit
tx_hash = client.submit_transaction(signed_tx)
```

## ArcClient

The `ArcClient` class provides typed methods for all RPC endpoints.

```python
from arc_sdk import ArcClient

client = ArcClient("http://localhost:9090")

# Health check
health = client.health()

# Chain stats
stats = client.stats()

# Get account
account = client.get_account("abc123...def")

# Get block
block = client.get_block(100)
block = client.get_latest_block()

# Get transaction
tx = client.get_transaction("0xabcdef...")

# Submit transaction
tx_hash = client.submit_transaction(signed_tx)

# Get validators
validators = client.get_validators()

# Get registered agents
agents = client.get_agents()
```

## KeyPair

Ed25519 key generation and signing.

```python
from arc_sdk import KeyPair

# Generate a new random keypair
kp = KeyPair.generate()

# From a seed (deterministic)
import blake3
kp = KeyPair.from_seed(blake3.blake3(b"my-seed").digest())

# Get the address (BLAKE3 hash of public key)
address = kp.address()

# Sign a message
signature = kp.sign(b"hello")
```

## TransactionBuilder

Build and sign any of ARC Chain's 24 transaction types.

```python
from arc_sdk import TransactionBuilder, KeyPair

kp = KeyPair.generate()

# Transfer
tx = TransactionBuilder.transfer(
    from_addr=kp.address(),
    to_addr="recipient-address",
    amount=1000,
)

# Settle (zero-fee agent settlement)
tx = TransactionBuilder.settle(
    from_addr=kp.address(),
    to_addr="agent-address",
    amount=500,
)

# Stake
tx = TransactionBuilder.stake(
    from_addr=kp.address(),
    amount=500000,
)

# Deploy contract
tx = TransactionBuilder.deploy_contract(
    from_addr=kp.address(),
    bytecode=b"...",
)

# Register an AI agent
tx = TransactionBuilder.register_agent(
    from_addr=kp.address(),
    name="my-agent",
    capabilities="inference",
    model_id="0x...",
)

# Sign any transaction
signed = TransactionBuilder.sign(tx, kp)
```

## AgentRunner

Connect any AI model (GPT-4, Claude, Llama, Ollama, OpenClaw) to ARC Chain as an on-chain agent. See the [Deploy an Agent](../agents/deploy-agent.md) guide for full examples.

```python
from arc_sdk import ArcClient, KeyPair
from arc_sdk.agent_runner import AgentRunner

client = ArcClient("http://localhost:9090")
kp = KeyPair.generate()

async def my_inference(input_text: str, model_id: str) -> str:
    return f"Echo: {input_text}"

runner = AgentRunner(
    client=client,
    keypair=kp,
    name="echo-agent",
    inference_fn=my_inference,
    fee_per_request=100,      # ARC per inference
    challenge_period=100,     # blocks for Tier 2 disputes
    bond_amount=1000,         # collateral for attestations
)

await runner.start()
```

### Pre-Built Runner Factories

The SDK includes convenience factories for common AI providers:

```python
from arc_sdk.agent_runner import (
    openai_runner,
    anthropic_runner,
    ollama_runner,
    openclaw_runner,
)
```

## ABI Encoding

Encode and decode Solidity ABI data for smart contract interactions.

```python
from arc_sdk import (
    encode_abi,
    decode_abi,
    encode_function_call,
    decode_function_result,
    function_selector,
    keccak256,
)

# Encode function call
calldata = encode_function_call(
    "transfer(address,uint256)",
    ["0xrecipient...", 1000],
)

# Get function selector
selector = function_selector("transfer(address,uint256)")

# Decode return data
result = decode_function_result(
    "balanceOf(address)",
    return_data,
)
```

## Error Handling

The SDK provides typed exceptions:

```python
from arc_sdk.errors import (
    ArcError,              # Base exception
    ArcConnectionError,    # Network/connection failures
    ArcTransactionError,   # TX validation or execution errors
    ArcValidationError,    # Input validation errors
    ArcCryptoError,        # Signature/key errors
)

try:
    client.submit_transaction(signed_tx)
except ArcTransactionError as e:
    print(f"TX failed: {e}")
except ArcConnectionError as e:
    print(f"Cannot reach node: {e}")
```

## Data Types

The SDK includes typed dataclasses for all chain objects:

```python
from arc_sdk.types import (
    Account,       # Address, balance, nonce, contract status
    Block,         # Block with header and transactions
    BlockHeader,   # Height, timestamp, state root, parent hash
    Receipt,       # TX execution result (success/fail, gas used)
    ChainInfo,     # Chain ID, version
    ChainStats,    # TPS, height, total TX count
    EventLog,      # Smart contract event
    HealthInfo,    # Node health status
    NodeInfo,      # Node metadata
)
```

## Full API Reference

| Export | Type | Description |
|--------|------|-------------|
| `ArcClient` | class | RPC client with typed methods |
| `KeyPair` | class | Ed25519 key generation and signing |
| `TransactionBuilder` | class | Build and sign all 24 TX types |
| `AgentRunner` | class | Connect any AI model to ARC Chain |
| `openai_runner` | factory | Pre-configured OpenAI runner |
| `anthropic_runner` | factory | Pre-configured Anthropic runner |
| `ollama_runner` | factory | Pre-configured Ollama runner |
| `openclaw_runner` | factory | Pre-configured OpenClaw runner |
| `encode_abi` | function | Encode ABI parameters |
| `decode_abi` | function | Decode ABI return data |
| `encode_function_call` | function | Encode full function calldata |
| `decode_function_result` | function | Decode function return values |
| `function_selector` | function | Compute 4-byte selector |
| `keccak256` | function | Keccak-256 hash |
