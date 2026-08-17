export type NodeRole = "observer" | "worker" | "validator" | "verifier";

export type HealthLevel = "live" | "lite" | "syncing" | "offline";

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

/**
 * Everything NOT prefixed `chain` describes the node on THIS machine, read
 * from `http://127.0.0.1:<rpcPort>`. The `chain*` fields describe the public
 * network, read from whichever seed is currently freshest. Keeping them apart
 * is deliberate: these used to be the same numbers, sourced from a remote
 * seed, which is how the Dashboard came to report a datacenter's uptime and
 * peer count as the user's own.
 */
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
  /**
   * If the local node has no peers but a public seed coordinator's `/health`
   * responded, this is its origin (e.g. `http://140.82.16.112:9090`). The UI
   * shows "Client mode (via LAX)" and the onboarding gate lets the user
   * through even when residential UDP P2P is blocked.
   */
  coordinatorUrl?: string | null;

  /** Seed these chain numbers came from, for attribution in the UI. */
  chainHost?: string | null;
  chainHeight?: number | null;
  chainRound?: number | null;
  /** Age of the newest block the chosen seed knows about. */
  chainBlockAgeSeconds?: number | null;

  /** Cores the running node was launched with. null = unconstrained. */
  workerThreads?: number | null;
  /** Logical cores on this machine (upper bound for the slider). */
  cpuCores?: number | null;
}

export interface Earnings {
  totalArc: number;
  /** null = the chain does not report it. Not the same as zero. */
  todayArc: number | null;
  /** null = not exposed by the chain yet. Never invented client-side. */
  pendingArc: number | null;
  rank: number | null;
  attestations: number;
  /** Epoch millis. Only ever a real timestamp. */
  lastPayoutAt: number | null;
  /** Block height of the last attestation — NOT a timestamp. */
  lastPayoutBlock: number | null;
  /** False = synthesized locally; label it as an estimate. */
  fromChain: boolean;
}

export interface Attestation {
  txHash: string;
  inputPreview: string;
  outputHash: string;
  modelHash: string;
  /** null when the record carries no count — render nothing, not "0". */
  tokens: number | null;
  latencyMs: number | null;
  /** Only set for attestations credited to this user. */
  rewardArc: number | null;
  /** null = "recent, exact time unknown". */
  timestamp: number | null;
  blockHeight: number | null;
  from: string | null;
  /** `from` matches the user's address. */
  mine: boolean;
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
  /** Cores the node may use. null = every logical core. */
  workerThreads: number | null;
}

/**
 * The single source of truth for a fresh node config. Onboarding, the
 * Dashboard's Start button and Settings all built their own literal, which
 * is how the Settings RPC-port field came to default to 9944 while
 * onboarding wrote 9090 and the Rust default was 9090.
 */
export const DEFAULT_NODE_CONFIG: NodeConfig = {
  role: "worker",
  modelPath: null,
  rpcPort: 9090,
  p2pPort: 9091,
  autoStart: true,
  autoUpdate: true,
  dataDir: "~/.arc",
  workerThreads: null,
};

/**
 * The identity as the UI sees it — no `seedPhrase`.
 *
 * The phrase is the signing key. It used to be returned here and persisted
 * into localStorage, which put it in reach of DevTools, of any injected
 * script, and of anything able to read the WebView profile directory. It now
 * stays in the Rust process; `api.revealSeedPhrase()` fetches it on demand
 * for the backup screen and the result must never be stored.
 */
export interface Identity {
  address: string;
  publicKey: string;
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

export interface ResetPeerStateResult {
  removedPath: string;
  wasPresent: boolean;
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

/** One shard in the pipeline that served an inference. */
export interface InferenceHop {
  hop: number;
  node: string;
  /** Layer range, e.g. "0..6". */
  layers: string;
  computeMs: number;
  wallMs: number;
  isTerminal: boolean;
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
  /** Origin URL of the coordinator that served the request. */
  coordinator?: string;
  /** Per-shard pipeline trace, when the serving node reported one. */
  trace?: InferenceHop[];
  /** Served by the arc-node on this machine. */
  servedLocally: boolean;
}

/** Outcome of a compute-contribution change. */
export interface ThreadsApplied {
  workerThreads: number;
  /** True = the node was restarted; false = applied live or saved for next start. */
  restarted: boolean;
  message: string;
}

/** Where `saveLogs` wrote the file; path is null if the user cancelled. */
export interface SavedLogs {
  path: string | null;
  lines: number;
}

// ── Tier 1 on-chain inference (VRF committee voting) ───────────────────────
// See `arc-chain-docs/TIER1_ONCHAIN_INFERENCE_PLAN.md`.
export interface Tier1Submitted {
  requestId: string; // 0x-prefixed 32-byte hex
  txHash: string;
  anchorHeight: number;
  committeeSize: number;
  deadlineBlocks: number;
  maxReward: number;
}

export interface Tier1Vote {
  voter: string;
  outputHash: string;
}

export type Tier1Status =
  | "Open"
  | "Voting"
  | "Finalized"
  | "Refunded"
  | "Unknown";

export interface Tier1Result {
  requestId: string;
  status: Tier1Status;
  voteCount: number;
  committeeSize: number;
  anchorHeight: number;
  deadlineBlocks: number;
  votes: Tier1Vote[];
  /** Set once consensus is reached. */
  outputHash: string | null;
  /** UTF-8 decode of the first voter's attached output, when present. */
  outputBlob: string | null;
  /** Tokenizer-decoded text of the output blob, when the node has the
   *  tokenizer loaded. Preferred over outputBlob for display. */
  outputText: string | null;
  maxReward: number;
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

/** A model tier the desktop will auto-download from a stable HF mirror. */
export interface ModelTierInfo {
  /** Stable id used everywhere (`tiny` | `standard` | `big`). */
  id: string;
  /** Human label e.g. "Llama-2 7B Chat (Q4_K_M)". */
  displayName: string;
  /** Size on disk after download. */
  sizeBytes: number;
  /** HF resolve URL the desktop streams from. */
  url: string;
}

/** Streamed event emitted on the `model-download-progress` channel during
 *  an active model download. `done = true` is the terminal event. */
export interface ModelDownloadProgress {
  tier: string;
  downloadedBytes: number;
  totalBytes: number;
  done: boolean;
}
