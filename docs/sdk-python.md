# Python SDK Reference

The ARC Chain Python SDK provides a typed HTTP client for all RPC endpoints, Ed25519 key management, transaction building and signing, and Ethereum-standard ABI encoding/decoding -- all with zero external dependencies for the ABI module.

**Version:** 0.1.0

---

## Installation

From source (in the arc-chain repository):

```bash
cd sdks/python
pip install -e .
```

Or when published:

```bash
pip install arc-sdk
```

**Requirements:** Python 3.10+. Dependencies: `httpx` (HTTP client), `pynacl` (Ed25519), `blake3` (hashing).

## Quick Start

```python
from arc_sdk import ArcClient, KeyPair, TransactionBuilder

# Connect to a local node
client = ArcClient("http://localhost:9090")

# Check node health
health = client.get_health()
print(health)  # {"status": "ok", "height": 1542, ...}

# Generate a new key pair
kp = KeyPair.generate()
print(f"Address: {kp.address()}")

# Build, sign, and submit a transfer
tx = TransactionBuilder.transfer(
    from_addr=kp.address(),
    to_addr="0" * 64,
    amount=1000,
)
signed_tx = TransactionBuilder.sign(tx, kp)
tx_hash = client.submit_transaction(signed_tx)
print(f"Submitted: {tx_hash}")
```

---

## ArcClient

The `ArcClient` class provides typed methods for all ARC Chain RPC endpoints. It uses `httpx` for connection pooling.

### Constructor

```python
client = ArcClient(
    rpc_url="http://localhost:9090",
    timeout=30.0,           # Request timeout in seconds
    headers={"X-Key": "v"}, # Optional extra HTTP headers
)
```

### Context Manager

```python
with ArcClient("http://localhost:9090") as client:
    stats = client.get_stats()
# Connection pool is closed automatically
```

### Chain Info Methods

```python
# Health check
health = client.get_health()           # -> dict

# Chain info (GPU, account count, mempool)
info = client.get_chain_info()          # -> dict

# Node-specific info (validator, stake, tier)
node = client.get_node_info()           # -> dict

# Aggregate statistics
stats = client.get_stats()             # -> dict
```

### Block Methods

```python
# Get block by height (returns raw dict)
block = client.get_block(42)

# Get block as typed object
block = client.get_block_typed(42)
print(block.header.height, block.header.tx_count)

# Paginated block listing
result = client.get_blocks(from_height=0, to_height=100, limit=20)
for b in result["blocks"]:
    print(f"Block {b['height']}: {b['tx_count']} txs")

# Transaction listing for a block
txs = client.get_block_txs(42, offset=0, limit=100)

# Merkle proofs for all transactions in a block
proofs = client.get_block_proofs(42)
```

### Account Methods

```python
# Get account (raw dict)
account = client.get_account("af1349b9...")

# Get account (typed)
account = client.get_account_typed("af1349b9...")
print(account.balance, account.nonce, account.staked_balance)

# Transaction history
txs = client.get_account_txs("af1349b9...")
print(txs["tx_hashes"])
```

### Transaction Methods

```python
# Submit a single transaction
tx_hash = client.submit_transaction({
    "from": "af1349...",
    "to": "2d3ade...",
    "amount": 1000,
    "nonce": 0,
})

# Submit a batch
result = client.submit_batch([tx1, tx2, tx3])
print(f"Accepted: {result['accepted']}, Rejected: {result['rejected']}")

# Get receipt
receipt = client.get_transaction("a1b2c3d4...")

# Get full transaction body
full = client.get_full_transaction("a1b2c3d4...")

# Get Merkle inclusion proof
proof = client.get_tx_proof("a1b2c3d4...")
```

### Contract Methods

```python
# Get contract info
info = client.get_contract_info("deadbeef...")

# Read-only contract call
result = client.call_contract(
    address="deadbeef...",
    function="get_count",
    calldata="",
    from_addr="af1349...",  # optional caller
    gas_limit=100_000,
)
print(result["return_data"], result["gas_used"])
```

### Light Client / Sync

```python
# Light client snapshot
snapshot = client.get_light_snapshot()

# Sync snapshot info
info = client.get_sync_snapshot_info()
```

---

## KeyPair

Ed25519 key generation, signing, and verification.

```python
from arc_sdk import KeyPair

# Generate a new random key pair
kp = KeyPair.generate()

# Derive from a seed (deterministic)
kp = KeyPair.from_seed(b"my-secret-seed-32-bytes-long!!!!")

# Get the 64-hex-char address (BLAKE3 hash of public key)
address = kp.address()  # "af1349b9..."

# Get the raw public key hex
pubkey = kp.public_key_hex()

# Sign arbitrary bytes
signature = kp.sign(b"hello world")

# Verify a signature
is_valid = kp.verify(b"hello world", signature)
```

---

## TransactionBuilder

Build unsigned transactions, then sign them with a `KeyPair`.

### Transfer

```python
tx = TransactionBuilder.transfer(
    from_addr=kp.address(),
    to_addr="0" * 64,
    amount=1000,
    fee=0,
    nonce=0,
)
signed = TransactionBuilder.sign(tx, kp)
```

### Settle (Zero Fee)

```python
tx = TransactionBuilder.settle(
    from_addr=kp.address(),
    agent_id="abcdef01" * 8,
    service_hash="fedcba98" * 8,
    amount=500,
    usage_units=10,
    nonce=0,
)
```

### Deploy Contract

```python
with open("counter.wasm", "rb") as f:
    bytecode = f.read()

tx = TransactionBuilder.deploy_contract(
    from_addr=kp.address(),
    code=bytecode,
    gas_limit=2_000_000,
    fee=50,
    nonce=1,
)
```

### Call Contract

```python
tx = TransactionBuilder.call_contract(
    from_addr=kp.address(),
    contract_addr="deadbeef" * 8,
    function="increment",
    calldata=b"",
    gas_limit=500_000,
    fee=1,
    nonce=2,
)
```

---

## ABI Encoding/Decoding

The `arc_sdk.abi` module provides Ethereum-standard ABI encoding and decoding with a **pure-Python Keccak-256** implementation (zero external dependencies).

### Function Selectors

```python
from arc_sdk import function_selector

selector = function_selector("transfer(address,uint256)")
print(selector.hex())  # "a9059cbb"
```

### Encode a Function Call

```python
from arc_sdk import encode_function_call

calldata = encode_function_call(
    "transfer(address,uint256)",
    "0xdead000000000000000000000000000000000000",
    1000,
)
# calldata[:4] == b'\xa9\x05\x9c\xbb' (selector)
# calldata[4:] == ABI-encoded arguments
```

### Encode Raw ABI Data

```python
from arc_sdk import encode_abi

encoded = encode_abi(
    ["address", "uint256", "bool"],
    ["0xdead000000000000000000000000000000000000", 1000, True],
)
```

### Decode ABI Data

```python
from arc_sdk import decode_abi

values = decode_abi(
    ["uint256", "bool"],
    encoded_bytes,
)
print(values)  # [1000, True]
```

### Decode Function Input (calldata)

```python
from arc_sdk import decode_function_input

name, args = decode_function_input(
    "transfer(address,uint256)",
    calldata,
)
print(name)  # "transfer"
print(args)  # ["0xdead...", 1000]
```

### Supported ABI Types

| Type | Python Representation |
|---|---|
| `uint8` through `uint256` | `int` |
| `int8` through `int256` | `int` (two's complement) |
| `address` | `str` (hex, with or without `0x`) |
| `bool` | `bool` |
| `bytes` | `bytes` (dynamic) |
| `bytes1` through `bytes32` | `bytes` (fixed) |
| `string` | `str` (UTF-8 encoded) |
| `T[]` | `list` (dynamic array) |
| `T[N]` | `list` (fixed array) |
| `(T1,T2,...)` | `tuple` (nested tuples) |

### Keccak-256

The SDK includes a standalone pure-Python Keccak-256 (Ethereum flavor, NOT NIST SHA3-256):

```python
from arc_sdk import keccak256

digest = keccak256(b"hello")
print(digest.hex())
```

---

## Error Handling

```python
from arc_sdk import (
    ArcError,
    ArcConnectionError,
    ArcTransactionError,
    ArcValidationError,
    ArcCryptoError,
)

# Base error -- all SDK errors inherit from this
try:
    client.get_account("nonexistent")
except ArcError as e:
    print(f"Status {e.status_code}: {e}")

# Connection errors (timeout, unreachable)
try:
    client = ArcClient("http://unreachable:9090", timeout=5.0)
    client.get_health()
except ArcConnectionError as e:
    print(f"Failed to connect to {e.url}: {e.cause}")

# Transaction errors (conflict, rejection)
try:
    client.submit_transaction(duplicate_tx)
except ArcTransactionError as e:
    print(f"TX rejected: {e}")

# Validation errors (bad address format, invalid params)
try:
    TransactionBuilder.transfer("short", "0" * 64, 100)
except ArcValidationError as e:
    print(f"Validation failed on '{e.field}': {e}")
```

---

## Typed Response Objects

Query methods with `_typed` suffix return dataclass instances:

```python
from arc_sdk import Account, Block, BlockHeader, Receipt, ChainInfo, ChainStats, EventLog, HealthInfo, NodeInfo

account = client.get_account_typed("af1349b9...")
account.address   # str
account.balance   # int
account.nonce     # int

block = client.get_block_typed(42)
block.header.height     # int
block.header.tx_count   # int
block.header.timestamp  # int

receipt = client.get_transaction_typed("a1b2c3d4...")
receipt.success    # bool
receipt.gas_used   # int
receipt.logs       # list[EventLog]
```
