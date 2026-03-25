/**
 * ARC Chain TypeScript SDK.
 *
 * A complete TypeScript client for interacting with the ARC Chain blockchain,
 * including transaction building, Ed25519 signing, and RPC communication.
 *
 * @example
 * ```ts
 * import { ArcClient, KeyPair, TransactionBuilder } from "@arc-chain/sdk";
 *
 * const client = new ArcClient("http://localhost:9090");
 * const kp = await KeyPair.generate();
 *
 * const tx = TransactionBuilder.transfer(kp.address(), "0".repeat(64), 1000);
 * const signed = await TransactionBuilder.sign(tx, kp);
 * const hash = await client.submitTransaction(signed);
 * ```
 */

export { ArcClient, ArcError, ArcConnectionError, ArcTransactionError } from "./client";
export type { ArcClientOptions } from "./client";

export { KeyPair } from "./crypto";

export { TransactionBuilder } from "./transaction";

export {
  encodeAbi,
  decodeAbi,
  encodeFunctionCall,
  decodeFunctionResult,
  decodeFunctionInput,
  functionSelector,
  keccak256,
} from "./abi";

export type {
  // Transaction types
  TxType,
  TxBody,
  TransferBody,
  DeployContractBody,
  WasmCallBody,
  StakeBody,
  SettleBody,
  Ed25519Signature,
  Transaction,

  // Chain types
  Account,
  Block,
  BlockHeader,
  BlockSummary,
  EventLog,
  Receipt,
  ChainInfo,
  ChainStats,
  HealthInfo,
  NodeInfo,

  // Submission results
  SubmitResult,
  BatchResult,

  // Contract types
  ContractInfo,
  ContractCallResult,

  // ETH compatibility
  EthRpcResponse,

  // Light client
  LightSnapshot,
  SyncSnapshotInfo,
} from "./types";
