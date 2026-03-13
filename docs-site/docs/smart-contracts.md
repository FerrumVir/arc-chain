---
title: "Smart Contracts"
sidebar_position: 7
slug: "/smart-contracts"
---
# Smart Contract Development

ARC Chain supports smart contracts through two paths: **WASM contracts** for on-chain logic executed by the ARC VM, and **Solidity contracts** compiled to EVM bytecode and deployed via the `DeployContract` transaction type. Four token standards are provided out of the box.

---

## Token Standards

ARC Chain includes production-ready Solidity implementations of three token standards plus a UUPS upgrade proxy, located in `contracts/standards/`:

| Contract | File | Description |
|---|---|---|
| **ARC-20** | `ARC20.sol` (7.6 KB) | Fungible token (ERC-20 compatible), mint/burn, owner-only |
| **ARC-721** | `ARC721.sol` (11.4 KB) | Non-fungible token (ERC-721 compatible), tokenURI, safeTransfer, ERC-165 |
| **ARC-1155** | `ARC1155.sol` (14.1 KB) | Multi-token (ERC-1155 compatible), batch operations, receiver hooks |
| **UUPSProxy** | `UUPSProxy.sol` (7.2 KB) | UUPS upgrade proxy with EIP-1967 storage slots |

All contracts target Solidity `0.8.24` with optimizer enabled (200 runs).

---

## WASM Contracts

### Overview

Contracts are compiled to WebAssembly and executed by the `ArcVM` runtime (`arc-vm` crate). Gas metering is enforced per operation. Storage is a key-value store scoped to the contract address.

### Contract Structure

A WASM contract exports named functions that can be called via `WasmCall` transactions (type `0x06`). The VM provides host imports for storage, balance queries, transfers, and event emission.

```rust
// Example: Simple counter contract (Rust -> WASM)
#[no_mangle]
pub fn increment() {
    let current = arc::storage_get(b"count");
    let value = u64::from_le_bytes(current.try_into().unwrap_or([0; 8]));
    arc::storage_set(b"count", &(value + 1).to_le_bytes());
    arc::emit_event(b"incremented", &(value + 1).to_le_bytes());
}

#[no_mangle]
pub fn get_count() -> u64 {
    let current = arc::storage_get(b"count");
    u64::from_le_bytes(current.try_into().unwrap_or([0; 8]))
}
```

### Host Imports

Contracts have access to these host functions provided by `ArcVM`:

| Function | Description |
|---|---|
| `storage_get(key)` | Read from contract storage |
| `storage_set(key, value)` | Write to contract storage |
| `storage_delete(key)` | Remove a storage entry |
| `balance_of(address)` | Query any account's balance |
| `transfer(to, amount)` | Send ARC tokens |
| `caller()` | Address of the transaction sender |
| `self_address()` | Address of this contract |
| `block_height()` | Current block height |
| `block_timestamp()` | Current block timestamp (ms) |
| `tx_value()` | ARC sent with this call |
| `gas_remaining()` | Remaining gas budget |
| `emit_event(topic, data)` | Emit an event log |

### Gas Costs for WASM Execution

| Operation | Gas |
|---|---|
| Contract call base | 21,000 |
| Contract deployment base | 53,000 |
| Storage read (SLOAD) | 200 |
| Storage write (SSTORE) | 5,000 |
| Event log (LOG) | 375 |
| Data byte | 16 |

### Compile and Deploy a WASM Contract

```bash
# Compile Rust to WASM
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/counter.wasm .
```

Deploy using the Python SDK:

```python
from arc_sdk import ArcClient, KeyPair, TransactionBuilder

client = ArcClient("http://localhost:9090")
kp = KeyPair.from_seed(b"my-secret-seed-32-bytes-long!!!!")

with open("counter.wasm", "rb") as f:
    bytecode = f.read()

tx = TransactionBuilder.deploy_contract(
    from_addr=kp.address(),
    code=bytecode,
    gas_limit=2_000_000,
    fee=50,
    nonce=0,
)
signed = TransactionBuilder.sign(tx, kp)
tx_hash = client.submit_transaction(signed)
print(f"Deploy tx: {tx_hash}")
```

### Call a Deployed Contract

**Write call** (modifies state, requires signing and gas):

```python
tx = TransactionBuilder.call_contract(
    from_addr=kp.address(),
    contract_addr="<contract_address>",
    function="increment",
    calldata=b"",
    gas_limit=500_000,
    fee=1,
    nonce=1,
)
signed = TransactionBuilder.sign(tx, kp)
tx_hash = client.submit_transaction(signed)
```

**Read-only call** (no transaction, no gas cost):

```python
result = client.call_contract(
    address="<contract_address>",
    function="get_count",
)
print(f"Count: {result['return_data']}")
print(f"Gas used: {result['gas_used']}")
```

Or via curl:

```bash
curl -X POST http://localhost:9090/contract/<address>/call \
  -H "Content-Type: application/json" \
  -d '{"function": "get_count", "gas_limit": 100000}'
```

---

## Solidity Contract Compilation

ARC Chain provides a compiler wrapper script that invokes `solc` and outputs EVM bytecode and ABI JSON.

### Using arc-compile

```bash
# Compile a single contract
./scripts/arc-compile.sh contracts/standards/ARC20.sol

# With custom output directory
./scripts/arc-compile.sh contracts/standards/ARC20.sol -o build/

# With custom optimizer runs
./scripts/arc-compile.sh contracts/standards/ARC20.sol -r 1000

# Without optimization
./scripts/arc-compile.sh contracts/standards/ARC20.sol --no-optimize
```

Output is written to `build/` next to the source file (by default), producing:
- `&lt;ContractName&gt;.bin` -- EVM bytecode (hex)
- `&lt;ContractName&gt;.abi` -- ABI JSON

### Prerequisites

Install `solc` version 0.8.24 or later:

```bash
# macOS
brew install solidity

# Linux
sudo add-apt-repository ppa:ethereum/ethereum
sudo apt-get update
sudo apt-get install solc
```

---

## Foundry Integration

ARC Chain ships a `foundry.toml` configuration for using Foundry (forge, cast, anvil) with ARC Chain's ETH JSON-RPC endpoint.

### Configuration

```toml
[profile.default]
src = "contracts"
out = "build"
libs = ["lib"]
optimizer = true
optimizer_runs = 200
evm_version = "shanghai"
solc_version = "0.8.24"

[profile.default.rpc_endpoints]
arc_local = "http://localhost:9090/eth"
arc_testnet = "http://testnet.arc.ai:9090/eth"

[etherscan]
arc = { key = "", chain = 42069, url = "http://localhost:3000/api/verify" }
```

**Chain ID:** `42069` (configured in `foundry.toml`) / `0x415243` (4,281,923 in the ETH JSON-RPC layer -- "ARC" in ASCII)

### Using Forge

```bash
# Build contracts
forge build

# Run tests
forge test

# Deploy to local ARC Chain
forge create contracts/standards/ARC20.sol:ARC20 \
  --rpc-url arc_local \
  --private-key <your-private-key> \
  --constructor-args "ARC Token" "ARC" 18

# Interact with a contract
cast call <contract_address> "totalSupply()(uint256)" \
  --rpc-url arc_local

cast send <contract_address> "transfer(address,uint256)" \
  <recipient> 1000 \
  --rpc-url arc_local \
  --private-key <key>
```

---

## UUPS Proxy (Upgradable Contracts)

The `UUPSProxy.sol` contract implements the Universal Upgradeable Proxy Standard with EIP-1967 storage slots:

- **`fallback()`**: Delegates all calls to the implementation contract
- **`upgradeTo(address)`**: Admin-only upgrade with contract validation
- **`upgradeToAndCall(address, bytes)`**: Upgrade and initialize in one transaction
- EIP-1967 slots for implementation address and admin address

### Deploy an Upgradable Contract

```bash
# 1. Deploy the implementation
forge create contracts/standards/ARC20.sol:ARC20 \
  --rpc-url arc_local --private-key <key>

# 2. Deploy the proxy pointing to the implementation
forge create contracts/standards/UUPSProxy.sol:UUPSProxy \
  --rpc-url arc_local --private-key <key> \
  --constructor-args <implementation_address> <admin_address> "0x"

# 3. Interact with the proxy (it delegates to implementation)
cast call <proxy_address> "totalSupply()(uint256)" --rpc-url arc_local
```

---

## ABI Encoding with SDKs

Both the Python and TypeScript SDKs provide full Ethereum-standard ABI encoding for interacting with Solidity contracts.

### Python

```python
from arc_sdk import encode_function_call, decode_abi, function_selector

# Encode a transfer call
calldata = encode_function_call(
    "transfer(address,uint256)",
    "0xdead000000000000000000000000000000000000",
    1000,
)

# Get function selector
sel = function_selector("transfer(address,uint256)")
print(sel.hex())  # "a9059cbb"

# Decode return data
values = decode_abi(["uint256"], return_bytes)
```

### TypeScript

```typescript
import { encodeFunctionCall, decodeAbi, functionSelector } from "@arc-chain/sdk";

const calldata = encodeFunctionCall(
  "transfer(address,uint256)",
  "0xdead000000000000000000000000000000000000",
  1000n,
);

const sel = functionSelector("transfer(address,uint256)");
// Uint8Array [0xa9, 0x05, 0x9c, 0xbb]

const [amount] = decodeAbi(["uint256"], returnBytes);
```

---

## DeployContract Transaction

The `DeployContract` transaction type (`0x08`, gas cost 53,000) deploys a contract to ARC Chain:

```rust
pub struct DeployBody {
    pub bytecode: Vec<u8>,          // WASM or EVM bytecode
    pub constructor_args: Vec<u8>,  // ABI-encoded constructor arguments
    pub state_rent_deposit: u64,    // Pre-paid state rent in ARC
}
```

The contract address is deterministically derived from the deployer address and nonce.

## WasmCall Transaction

The `WasmCall` transaction type (`0x06`, gas cost 21,000 + execution) calls a deployed contract:

```rust
pub struct WasmCallBody {
    pub contract: Address,     // Contract address
    pub function: String,      // Function name to call
    pub calldata: Vec<u8>,     // ABI-encoded arguments
    pub value: u64,            // ARC to send with the call
    pub gas_limit: u64,        // Gas limit for execution
}
```
