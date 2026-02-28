// ─── @arc-chain/sdk ───────────────────────────────────────────
// TypeScript SDK for building agents on ARC Chain
//
// Usage:
//   import { ArcAgent, ArcClient, ArcWallet, TxType } from "@arc-chain/sdk";
//
//   // Quick start — create agent with new wallet
//   const agent = ArcAgent.create("http://localhost:8545");
//   await agent.init();
//
//   // Send tokens
//   const receipt = await agent.transfer("0x...", 1000n);
//   console.log(`TX ${receipt.tx_hash} — ${receipt.success ? "OK" : "FAIL"}`);
//
//   // Listen for blocks
//   agent.onBlock((block) => console.log(`Block #${block.height}`));

export { ArcClient, ArcRpcError } from "./client";
export type { ArcClientConfig } from "./client";

export { ArcWallet } from "./wallet";

export { ArcAgent, AgentError } from "./agent";

export {
  TxType,
} from "./types";

export type {
  Hash256,
  Address,
  UnsignedTransaction,
  SignedTransaction,
  TxReceipt,
  BlockHeader,
  Block,
  AccountState,
  NodeInfo,
  MerkleProof,
  AgentConfig,
  EscrowParams,
  BatchResult,
} from "./types";
