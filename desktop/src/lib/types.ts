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

export interface NodeStatus {
  running: boolean;
  pid: number | null;
  health: HealthLevel;
  version: string;
  peers: number;
  chainParticipationEnabled?: boolean | null;
  round: number;
  committed: number;
  height: number;
  uptimeSeconds: number;
  address: string | null;
  rpcPort: number;
  lastError: string | null;
  coordinatorUrl?: string | null;

  chainHost?: string | null;
  chainHeight?: number | null;
  chainRound?: number | null;
  chainBlockAgeSeconds?: number | null;

  workerThreads?: number | null;
  cpuCores?: number | null;
  modelUnavailable?: ModelUnavailableReason | null;
  crashRetryInSecs?: number | null;
  modelDownloadWait?: ModelDownloadWait | null;
}

export type ModelUnavailableReason = "notFound" | "unreadable";

export interface ConfirmedRewardReceipt {
  txHash: string;
  jobId: string;
  blockHeight: number;
  blockHash: string;
  rewardBase: number;
  rewardArc: number;
  receiptUrl: string;
  recoveryEpoch: number | null;
  validatorSetId: number | null;
}

export interface Earnings {
  totalArc: number;
  todayArc: number | null;
  pendingArc: number | null;
  rank: number | null;
  attestations: number;
  lastPayoutAt: number | null;
  lastPayoutBlock: number | null;
  confirmedReceipts: ConfirmedRewardReceipt[];
  projectedDailyArc: number | null;
  projectedDailyUnavailableReason: string | null;
  recoveryEpoch: number | null;
  validatorSetId: number | null;
  unavailableReason: string | null;
  receiptSource: string | null;
  archiveMode: boolean | null;
  historyCompleteSinceRecovery: boolean | null;
  historyScope: string | null;
  fromChain: boolean;
}

export interface Attestation {
  txHash: string;
  inputPreview: string;
  outputHash: string;
  modelHash: string;
  tokens: number | null;
  latencyMs: number | null;
  timestamp: number | null;
  blockHeight: number | null;
  txType: string | null;
  recordKind: string | null;
  computed: boolean;
  paid: boolean;
  earned: boolean;
  from: string | null;
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
  autoInstallUpdates: boolean;
  dataDir: string;
  workerThreads: number | null;
  computeConsent?: boolean | null;
  preventSleepDuringJobs?: boolean | null;
  keepAwakeWhileSharing?: boolean | null;
  computeDefaultNoticePending?: boolean;
}

export function computeContributionEnabled(
  config: NodeConfig | null | undefined,
): boolean {
  if (!config) return false;
  if (config.computeConsent === true) return true;
  if (config.computeConsent === false) return false;
  return config.role === "worker" && !!config.modelPath;
}

export function defaultWorkerThreads(logicalCores: number): number {
  const cores = Number.isFinite(logicalCores) ? logicalCores : 0;
  return Math.max(1, Math.floor(cores / 4));
}

export const DEFAULT_NODE_CONFIG: NodeConfig = {
  role: "worker",
  modelPath: null,
  rpcPort: 9090,
  p2pPort: 9091,
  autoStart: true,
  autoUpdate: true,
  autoInstallUpdates: true,
  dataDir: "~/.arc/data-v3",
  workerThreads: null,
};

export interface DataMigrationNotice {
  legacyDataDir: string;
  activeDataDir: string;
  migratedAt: number;
  reason: string;
}

export interface PreviousInstallNotice {
  setAsideDataDir: string;
  activeDataDir: string;
  setAsideAt: number;
}

export interface LegacyComputeQuestion {
  previousRole: string;
  previousModelPath: string | null;
  detectedBy: string;
  recordedUnixMs: number;
}

export interface Identity {
  address: string;
  publicKey: string;
  createdAt: number;
}

export interface NetworkStats {
  totalNodes: number;
  totalInferences: number;
  avgTps: number;
  latestBlock: number | null;
}

export interface Unavailable {
  unavailable: string | null;
  sourceHost: string;
}

export interface RewardEconomics extends Unavailable {
  rewardPerAttestation: number | null;
  treasuryBalanceArc: number | null;
  treasuryBalanceUnavailableReason: string | null;
  attestationsRemaining: number | null;
  attestationsRemainingUnavailableReason: string | null;
  treasuryIsFinite: boolean | null;
  bondPerAttestation: number | null;
  challengePeriodBlocks: number | null;
  bondRefundedAfterChallengePeriod: boolean | null;
  fundingDetail: string | null;
}

export type RateSource = "chain" | "constant" | "unknown";

export interface EarningsProjection extends Unavailable {
  rewardPerAttestation: number | null;
  rewardRateSource: RateSource;
  communityRewardsEnabled: boolean | null;
  projectedDailyArc: number | null;
  projectedDailyUnavailableReason: string | null;
  rewardPolicyHash: string | null;
  rewardBudgetEpoch: number | null;
  rewardsRemainingThisEpoch: number | null;
  workerRewardsRemainingThisEpoch: number | null;
  coordinatorRewardsRemainingThisEpoch: number | null;
  issuanceReadyForWorker: boolean | null;
  rewardProgram: string | null;
  rewardIsCustomerDemand: boolean | null;
  attestationsTotal: number;
  firstAttestationBlock: number | null;
  attestationsPerDay: number | null;
  rateUnavailableReason: string | null;
  observedOverBlocks: number | null;
  rateCaveat: string | null;
}

export interface NodeContribution extends Unavailable {
  source: "contribution" | "composed" | "none";
  threadsInUse: number | null;
  threadsAvailable: number | null;
  layersHeld: string | null;
  layerCount: number | null;
  totalLayers: number | null;
  runsServed: number | null;
  cacheHits: number | null;
  hopMsMean: number | null;
  hopSamples: number | null;
  hopUnavailableReason: string | null;
}

export interface ValidatorInfo {
  address: string;
  stake: number;
  active: boolean;
}

export interface NetworkOverview extends Unavailable {
  networkName: string | null;
  networkNameUnavailableReason: string | null;
  chainId: string | null;
  declaresMainnet: boolean | null;
  isBlockProducing: boolean | null;
  isBlockProducingBasis: string | null;
  hostVersion: string | null;
  height: number | null;
  lastBlockAgeSecs: number | null;
  dagRound: number | null;
  dagCommitted: number | null;
  peers: number | null;
  validatorsActive: number | null;
  validatorsRegistered: number | null;
  minActiveStake: number | null;
  validatorSplitDerived: boolean;
  validators: ValidatorInfo[];
}

export interface BlockSummary {
  height: number;
  hash: string;
  timestampMs: number | null;
  txCount: number | null;
  proposer: string | null;
}

export interface RecentBlocks extends Unavailable {
  blocks: BlockSummary[];
}

export interface BlockTx {
  index: number;
  hash: string;
  txType: string | null;
  from: string | null;
}

export interface BlockTxs extends Unavailable {
  height: number;
  txCount: number | null;
  txs: BlockTx[];
}

export type TxLookupStatus =
  | "mined"
  | "not_found"
  | "invalid_hash"
  | "error";

export interface TxLookup extends Unavailable {
  hash: string;
  status: TxLookupStatus;
  blockHeight: number | null;
  blockHash: string | null;
  txIndex: number | null;
  success: boolean | null;
  gasUsed: number | null;
}

export interface AccountBalance {
  address: string;
  balanceBase: string;
  balanceArc: string;
  nonce: number;
  stakedBalanceBase: string;
  stakedBalanceArc: string;
}

export type WalletReceiptStatus =
  | "pending"
  | "mined_success"
  | "mined_failed"
  | "receipt_unavailable";

export interface WalletTxResult {
  txHash: string;
  amountBase: string;
  amountArc: string;
  receiptStatus: WalletReceiptStatus;
  mined: boolean;
  success: boolean | null;
  blockHeight: number | null;
  blockHash: string | null;
  sourceHost: string;
  unavailable: string | null;
  message: string;
}

export type FaucetResult = WalletTxResult;

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

export interface InferenceHop {
  hop: number;
  node: string;
  layers: string;
  computeMs: number;
  wallMs: number;
  isTerminal: boolean;
}

export interface InferenceSettlement {
  status: string;
  txType: string;
  txHash: string;
  jobId: string;
  worker: string;
  submitted: boolean;
  included: boolean;
  confirmed: boolean;
  success: boolean | null;
  blockHeight: number | null;
  blockHash: string | null;
  index: number | null;
  rewardBase: number | null;
  rewardArc: number | null;
  receiptUrl: string;
}

export interface InferenceResult {
  input: string;
  output: string;
  outputHash: string;
  modelHash: string;
  tokensGenerated: number;
  inferenceMs: number;
  attestationStatus?: string;
  attestationHash?: string;
  txHash: string;
  deterministic: boolean;
  profileBound: boolean;
  quorumVerified: boolean;
  executionProfile: string;
  engine: string;
  explorerUrl: string;
  routedVia?: string;
  settlement?: InferenceSettlement;
  consensus?: InferenceConsensus;
  coordinator?: string;
  trace?: InferenceHop[];
  servedLocally: boolean;
}

export interface ThreadsApplied {
  workerThreads: number;
  restarted: boolean;
  message: string;
}

export interface SavedLogs {
  path: string | null;
  lines: number;
}

export interface Tier1Submitted {
  requestId: string;
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
  outputHash: string | null;
  outputBlob: string | null;
  outputText: string | null;
  maxReward: number;
}

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
  path: string;
  downloadedBytes: number;
  totalBytes: number;
  alreadyInstalled: boolean;
}

export interface UpdateInstallPolicy {
  canInstall: boolean;
  channel: "appimage" | "native" | "package-manager";
  instructions: string;
}

export type AutoUpdatePrepareResult =
  | { status: "prepared" }
  | { status: "busy"; reason: string; activeJobs: number };

export interface ModelTierInfo {
  id: string;
  displayName: string;
  sizeBytes: number;
  url: string;
}

export interface ModelDownloadProgress {
  tier: string;
  downloadedBytes: number;
  totalBytes: number;
  done: boolean;
  stage?: string;
  resumedFromBytes?: number;
  attempt?: number;
  retryInSecs?: number | null;
  message?: string | null;
}

export type PartialShardPhase =
  | "starting"
  | "loading"
  | "ready"
  | "computing"
  | "reconnecting"
  | "failed"
  | "stopped";

export interface PartialShardRangeStatus {
  startLayer: number;
  endLayer: number;
  totalLayers: number;
}

export interface PartialShardWorkerStatus {
  phase: string | null;
  modelName: string | null;
  modelId: string | null;
  leaseSeq: number | null;
  leaseEpoch: number | null;
  assignedRange: PartialShardRangeStatus | null;
  readyExpiresAtUnixMs: number | null;
  leaseLive: boolean;
  stepsExecuted: number | null;
  stepsSubmitted: number | null;
  submitRetries: number | null;
  submitStaleConflicts: number | null;
  cancellationsApplied: number | null;
  rebalancesCompleted: number | null;
  reconnectAttempts: number | null;
  lastHeartbeatUnixMs: number | null;
  lastStepSubmittedUnixMs: number | null;
  lastError: string | null;
  rewardsEnabled: boolean;
  rewardEligible: boolean;
}

export interface WorkerStatus {
  running: boolean;
  unavailable: string | null;
  state: string | null;
  publicName: string | null;
  coordinatorsRegistered: number | null;
  coordinatorsTotal: number | null;
  jobsClaimed: number | null;
  jobsCompleted: number | null;
  jobsVerified: number | null;
  jobsFailed: number | null;
  jobsDeclined: number | null;
  lastJobCompletedUnixMs: number | null;
  startedUnixMs: number | null;
  preventSleepDuringJobs: boolean | null;
  mode?: string | null;
  shard?: PartialShardWorkerStatus | null;
}

export type ModelDownloadWait =
  | { reason: "onBattery" | "meteredNetwork" | "failed" }
  | { reason: "diskFull"; neededBytes: number };
export interface WorkerNetwork {
  count_total: number;
  count_visible: number | null;
  eligible_inference_workers: number | null;
}
