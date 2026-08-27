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

export interface ConfirmedRewardReceipt {
  txHash: string;
  jobId: string;
  blockHeight: number;
  blockHash: string;
  rewardBase: number;
  rewardArc: number;
  recoveryEpoch: number | null;
  validatorSetId: number | null;
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
  /** Successful mined 0x25 rows that alone reconcile to totalArc. */
  confirmedReceipts: ConfirmedRewardReceipt[];
  projectedDailyArc: number | null;
  projectedDailyUnavailableReason: string | null;
  recoveryEpoch: number | null;
  validatorSetId: number | null;
  /** True only for the candidate's mined-0x25 receipt/readiness contract. */
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
  /** null = "recent, exact time unknown". */
  timestamp: number | null;
  blockHeight: number | null;
  /**
   * Transaction type as the host labelled it.
   *
   * Current builds emit `"Inference"` on every row. Older deployed seeds
   * padded `/inference/attestations` with unrelated transactions tagged
   * `"Other"` once real rows ran out — at `limit=500` some seeds returned 500
   * padding rows and zero real ones. The Network screen filters on this so a
   * chain view is not half transfers presented as inference evidence.
   */
  txType: string | null;
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

/**
 * The convention every type below follows.
 *
 * `unavailable` carries a human-readable reason the data could not be read —
 * a 404 from a seed that predates the endpoint, a connection failure, a host
 * that answered with a shape we don't recognise. When it is set, the numeric
 * fields are `null` and the UI renders the reason instead of a figure. It is
 * never "0", because a host that cannot answer is not a host reporting zero.
 *
 * `sourceHost` is always the pinned chain host (see CLAUDE.md rule 4 — chain
 * reads stay on ONE elected seed for the session). It is displayed next to
 * anything derived from it so a number is always attributable.
 */
export interface Unavailable {
  unavailable: string | null;
  sourceHost: string;
}

/**
 * `GET /economics/rewards` — the finite testnet reward treasury.
 *
 * Surfacing the ceiling is the point of this type. A per-day earnings
 * projection with no ceiling implies an unlimited payout, which is the
 * dishonest version of the feature.
 *
 * Two wire names are easy to misread, and both were misread once:
 *
   * - **`rewards_remaining` is a COUNT of fundable reward receipts, not an ARC
   *   amount** — the treasury balance divided by the per-receipt reward. Rendering it as
 *   currency is wrong by nine orders of magnitude *and* wrong in kind. It is
 *   carried here as `attestationsRemaining` so the name cannot be confused.
 * - The treasury balance is `treasury_balance_arc` / `_base`. There is no
 *   `treasury_total` and no `rewards_paid`.
 */
export interface RewardEconomics extends Unavailable {
  /** ARC paid for one settled attestation. */
  rewardPerAttestation: number | null;
  /** ARC left in the reward treasury. */
  treasuryBalanceArc: number | null;
  treasuryBalanceUnavailableReason: string | null;
  /**
   * How many MORE successful reward receipts the treasury can fund. A count, not
   * currency. This is the honest form of "how much is left": it is denominated
   * in the thing a worker actually produces.
   */
  attestationsRemaining: number | null;
  attestationsRemainingUnavailableReason: string | null;
  /** The host states outright that the treasury is bounded. */
  treasuryIsFinite: boolean | null;
  /**
   * ARC bonded by a community worker reward certificate. This is deliberately
   * separate from the coordinator's local-attestation bond.
   */
  bondPerAttestation: number | null;
  /** Reserved for a future community-certificate challenge period. */
  challengePeriodBlocks: number | null;
  /** Reserved for a future community-certificate bond refund contract. */
  bondRefundedAfterChallengePeriod: boolean | null;
  /** Where the money comes from, in the host's own words. */
  fundingDetail: string | null;
}

/** Where a displayed reward rate came from. Never "assumed". */
export type RateSource = "chain" | "constant" | "unknown";

/**
 * `GET /worker/earnings/{addr}` — the inputs a projection needs.
 *
 * Kept separate from `Earnings` (which is lifetime-to-date) because a
 * projection has a completely different honesty burden: it is the one number
 * in the app that describes something that has not happened yet.
 */
export interface EarningsProjection extends Unavailable {
  /** ARC per successful mined community-reward receipt. */
  rewardPerAttestation: number | null;
  /** Whether the rate above is chain-reported or a local named constant. */
  rewardRateSource: RateSource;
  /**
   * Exact reward rollout gate reported by the selected coordinator. A
   * projection is shown only when this is true.
   */
  communityRewardsEnabled: boolean | null;
  /** Successful mined reward receipts retained for this address. */
  attestationsTotal: number;
  /** Block containing the first retained successful reward receipt. */
  firstAttestationBlock: number | null;
  /**
   * Reward receipts per day, MEASURED over the address's retained history.
   *
   * null with `rateUnavailableReason` set when there is no history to measure
   * — which is the common case. It is never extrapolated from zero: an
   * account with no retained receipts has no rate, not a rate of zero, and
   * certainly not a projection.
   */
  attestationsPerDay: number | null;
  /** Why `attestationsPerDay` is null, in words, for display. */
  rateUnavailableReason: string | null;
  /**
   * Blocks the rate was observed across (`blocks_observed` on the wire).
   * Named in the assumptions line so the rate can be judged.
   */
  observedOverBlocks: number | null;
  /** The host's own caveat about how it derived the rate, shown verbatim. */
  rateCaveat: string | null;
}

/**
 * What this machine is actually contributing, as opposed to what the slider
 * is set to.
 *
 * Read from the LOCAL node (127.0.0.1), not a seed — this describes the
 * user's own hardware. Prefers `GET /node/contribution`; where that endpoint
 * is absent it is composed from `GET /node/threads` and `GET /stats`, which
 * are present on the shipped binary. Every field is still a measurement
 * either way; `composed` just says which endpoints answered.
 */
export interface NodeContribution extends Unavailable {
  /** "contribution" = the dedicated endpoint; "composed" = threads + stats. */
  source: "contribution" | "composed" | "none";
  /** Threads the node is actually working with (`threads.in_use`). */
  threadsInUse: number | null;
  /** Logical cores the node can see (`threads.available_parallelism`). */
  threadsAvailable: number | null;
  /** Layer ranges rendered for display, e.g. "0..6, 12..18". */
  layersHeld: string | null;
  /** Distinct layers held — a UNION, not a sum over replicas. */
  layerCount: number | null;
  /** Layers in the whole model, for "6 of 32". */
  totalLayers: number | null;
  /**
   * Real sharded pipeline walks this node served. Deliberately NOT summed
   * with cache hits — the node counts those separately and a cache hit is
   * not work performed.
   */
  runsServed: number | null;
  /** Pipeline walks answered from cache, kept apart from `runsServed`. */
  cacheHits: number | null;
  /** Measured mean of this node's own compute per hop. null = never measured. */
  hopMsMean: number | null;
  /** Samples the mean rests on. A mean over 2 and over 200 differ in weight. */
  hopSamples: number | null;
  /** The host's reason for having no timing, shown verbatim. */
  hopUnavailableReason: string | null;
}

/** One validator as the chain host reports it. */
export interface ValidatorInfo {
  address: string;
  stake: number;
  /** Stake > 0. Zero-stake entries are counted by `/health` but cannot lead. */
  active: boolean;
}

/**
 * The Network screen's chain view, all from the ONE pinned host.
 *
 * Nothing here compares two hosts. The seeds are independent chains with
 * different heights and different block hashes at the same height, so a
 * side-by-side would be reporting a disagreement as if it were a fault.
 */
export interface NetworkOverview extends Unavailable {
  /**
   * Network name as `/network/info` reports it. null when the endpoint is
   * absent — in which case the UI says the name is unknown and names the
   * host, rather than picking a name.
   */
  networkName: string | null;
  /** The host's reason for not naming its network, shown verbatim. */
  networkNameUnavailableReason: string | null;
  chainId: string | null;
  /**
   * Whether the host's genesis DECLARES itself mainnet.
   *
   * null means the host did not say, and the UI then says nothing about
   * mainnet either. This is the only input allowed to make the app describe a
   * network as mainnet — `/info`'s `chain` field is the constant string
   * "ARC Chain" everywhere and distinguishes nothing.
   */
  declaresMainnet: boolean | null;
  /** The host's own verdict on whether it is producing blocks, plus its basis. */
  isBlockProducing: boolean | null;
  isBlockProducingBasis: string | null;
  /** arc-node version the pinned host is running. */
  hostVersion: string | null;
  height: number | null;
  /**
   * Age of the newest block this host knows about. The one number that
   * distinguishes a live chain from a stalled one — `/health` reports `ok`
   * either way, because DAG rounds keep advancing after blocks stop.
   */
  lastBlockAgeSecs: number | null;
  dagRound: number | null;
  dagCommitted: number | null;
  peers: number | null;
  /**
   * Validators that can actually lead a round.
   *
   * From `/network/info` when it answers, which applies the real
   * `minActiveStake` threshold; otherwise derived by counting stake > 0, which
   * only approximates it. `validatorSplitDerived` says which happened.
   */
  validatorsActive: number | null;
  /** Every validator in the set, zero-stake entries included. */
  validatorsRegistered: number | null;
  /** Minimum stake for an active validator, when the host reports it. */
  minActiveStake: number | null;
  /** True when the split was counted locally rather than reported. */
  validatorSplitDerived: boolean;
  validators: ValidatorInfo[];
}

/** One block in the recent-blocks list. */
export interface BlockSummary {
  height: number;
  hash: string;
  /** Epoch millis from the block header. */
  timestampMs: number | null;
  txCount: number | null;
  proposer: string | null;
}

export interface RecentBlocks extends Unavailable {
  blocks: BlockSummary[];
}

/**
 * One transaction inside a block, from `GET /block/{h}/txs`.
 *
 * Only `index` and `hash` are guaranteed — normal blocks return exactly those
 * two. The rest appear on reconstructed benchmark blocks only, so they stay
 * optional instead of being defaulted into existence.
 */
export interface BlockTx {
  index: number;
  hash: string;
  txType: string | null;
  from: string | null;
}

export interface BlockTxs extends Unavailable {
  height: number;
  /** Total in the block, which can exceed `txs.length` when paginated. */
  txCount: number | null;
  txs: BlockTx[];
}

/** Outcome of a `GET /tx/{hash}` lookup. */
export type TxLookupStatus =
  /** A receipt exists: it is in a block on the pinned host's chain. */
  | "mined"
  /**
   * HTTP 404 — the host has no receipt for it. On this chain that is ALSO
   * exactly what a pending attestation looks like: `/tx/{hash}` is a receipt
   * lookup, and a tx sitting in the mempool has no receipt yet. So this is
   * rendered as "not in a block yet", never as "invalid" or "no such hash".
   */
  | "not_found"
  /**
   * HTTP 400 — not 64 hex characters. Genuinely different from `not_found`:
   * this one really is a bad paste, and saying so saves the user waiting for
   * a tx that was never submitted.
   */
  | "invalid_hash"
  /** The lookup itself failed (host unreachable, unparseable response). */
  | "error";

/**
 * A `GET /tx/{hash}` result.
 *
 * Deliberately narrow: the endpoint returns a `TxReceipt`, which carries no
 * `tx_type` and no `from`. Those are only on `/tx/{hash}/full`. Rather than
 * fetch two endpoints and risk showing half a record, this shows what a
 * receipt actually proves — that the tx is in a block, at what height, and
 * whether it succeeded.
 */
export interface TxLookup extends Unavailable {
  /** Echo of what was searched, `0x` stripped. */
  hash: string;
  status: TxLookupStatus;
  blockHeight: number | null;
  blockHash: string | null;
  /** Position within the block. */
  txIndex: number | null;
  success: boolean | null;
  gasUsed: number | null;
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

// Historical Tier 1 read/result shapes. New request writes are disabled in the
// recovery candidate; these remain for IPC compatibility and old-ID inspection.
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

/** Legacy paid-inference response shape, retained only for IPC compatibility.
 *  The v0.7.12 recovery candidate rejects new escrow writes before signing. */
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
