// ─── @arc-chain/sdk ───────────────────────────────────────────
// TypeScript SDK for the ARC Chain RPC API.
//
// Usage:
//   import { ArcClient } from "@arc-chain/sdk";
//
//   const client = new ArcClient("http://localhost:9944");
//   const health = await client.getHealth();
//   const block  = await client.getBlock(42);
//   const tx     = await client.getTxFull(block.tx_hashes[0]);
// ─── Client ─────────────────────────────────────────────────
export { ArcClient, ArcRpcError } from "./client.js";
// ─── Utilities ──────────────────────────────────────────────
export { isValidAddress, isValidTxHash, formatHash, formatArc, hexToBytes, bytesToHex, stripHexPrefix, addHexPrefix, } from "./utils.js";
//# sourceMappingURL=index.js.map