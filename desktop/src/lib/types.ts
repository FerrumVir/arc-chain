export type NodeRole = "observer" | "worker" | "validator" | "verifier";

export type HealthLevel = "live" | "syncing" | "offline";

export interface HardwareInfo {
  platform: string;
  arch: string;
  cpuModel: string;
  cpuCores: number;
  ramGb: number;
  gpuName: string | null;
  gpuVramGb: number | null;
  recommendedModel: string;
  recommendedRole: NodeRole;
  estimatedDailyArc: number;
}

export interface NodeStatus {
  running: boolean;
  pid: number | null;
  health: HealthLevel;
  version: string;
  peers: number;
  round: number;
  committed: number;
  height: number;
  uptimeSeconds: number;
  address: string | null;
  rpcPort: number;
  lastError: string | null;
}

export interface Earnings {
  totalArc: number;
  todayArc: number;
  pendingArc: number;
  rank: number | null;
  attestations: number;
  lastPayoutAt: number | null;
}

export interface Attestation {
  txHash: string;
  inputPreview: string;
  outputHash: string;
  modelHash: string;
  tokens: number;
  latencyMs: number;
  rewardArc: number;
  timestamp: number;
  verified: boolean;
}

export interface LogEntry {
  id: string;
  timestamp: number;
  level: "info" | "warn" | "error" | "ok";
  message: string;
}

export interface NodeConfig {
  role: NodeRole;
  modelPath: string | null;
  rpcPort: number;
  p2pPort: number;
  autoStart: boolean;
  autoUpdate: boolean;
  dataDir: string;
}

export interface Identity {
  address: string;
  publicKey: string;
  seedPhrase: string;
  createdAt: number;
}

export interface NetworkStats {
  totalNodes: number;
  totalInferences: number;
  avgTps: number;
  latestBlock: number;
}

export interface AccountBalance {
  address: string;
  balance: number;
  nonce: number;
  stakedBalance: number;
}

export interface FaucetResult {
  txHash: string;
  amount: number;
  message: string;
}

export interface InferenceConsensus {
  k: number;
  votesTotal: number;
  unanimous: number;
  majority: number;
  split: number;
  divergentReplicaCount: number;
}

export interface InferenceResult {
  input: string;
  output: string;
  outputHash: string;
  modelHash: string;
  tokensGenerated: number;
  inferenceMs: number;
  txHash: string;
  deterministic: boolean;
  engine: string;
  explorerUrl: string;
  /** Present when the inference was served by a coordinator (seed RPC). */
  consensus?: InferenceConsensus;
  /** Origin URL of the coordinator that served the request; None = local. */
  coordinator?: string;
}

/** Milestone B (#36): paid-inference receipt - includes on-chain tx
 *  hashes for the escrow-open + escrow-release, plus payer bookkeeping. */
export interface PaidInferenceResult {
  input: string;
  output: string;
  outputHash: string;
  tokensGenerated: number;
  inferenceMs: number;
  coordinator: string;
  consensus: InferenceConsensus;
  payerAddress: string;
  maxFee: number;
  openTxHash: string;
  releaseTxHash: string;
}

export interface BinaryStatus {
  /** Absolute path to the arc-node binary on disk. */
  path: string;
  /** Bytes downloaded on this call (0 if already installed). */
  downloadedBytes: number;
  /** Total announced by the server (0 if unknown). */
  totalBytes: number;
  /** True when the binary was already present - nothing was fetched. */
  alreadyInstalled: boolean;
}
