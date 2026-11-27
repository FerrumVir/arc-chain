import { PLAIN_COPY } from "./plain-copy";
import type {
  PartialShardPhase,
  PartialShardRangeStatus,
  PartialShardWorkerStatus,
  WorkerStatus,
} from "./types";

export const PARTIAL_SHARD_MODE = "partial_shard" as const;

export const PARTIAL_SHARD_PHASES = [
  "starting",
  "loading",
  "ready",
  "computing",
  "reconnecting",
  "failed",
  "stopped",
] as const satisfies readonly PartialShardPhase[];

export type WorkerStatusInvalidReason =
  | "unknown_mode"
  | "contradictory_shard"
  | "missing_shard"
  | "malformed_shard"
  | "unknown_phase"
  | "contradictory_running";

export interface WholeWorkerView {
  kind: "whole";
  running: boolean;
  state: string | null;
  stateLabel: string;
}

export interface PartialShardWorkerView {
  kind: "partial_shard";
  reportedPhase: PartialShardPhase;
  phase: PartialShardPhase;
  downgradedToReconnecting: boolean;
  leaseExpired: boolean;
  stateLabel: string;
  summary: string;
  modelName: string | null;
  modelId: string | null;
  leaseLive: boolean;
  leaseEpoch: number | null;
  leaseSeq: number | null;
  assignedRange: PartialShardRangeStatus | null;
  assignedRangeLabel: string | null;
  readyExpiresAtUnixMs: number | null;
  stepsExecuted: number | null;
  stepsSubmitted: number | null;
  lastStepSubmittedUnixMs: number | null;
  lastError: string | null;
  rewardsEnabled: false;
  rewardEligible: false;
}

export interface InvalidWorkerView {
  kind: "invalid";
  reason: WorkerStatusInvalidReason;
  stateLabel: string;
  summary: string;
  rewardsEnabled: false;
  rewardEligible: false;
}

export type WorkerStatusView =
  | WholeWorkerView
  | PartialShardWorkerView
  | InvalidWorkerView;

const WHOLE_STATE_COPY: Record<string, string> = {
  polling: PLAIN_COPY.workerJobs.wholeStates.polling,
  computing: PLAIN_COPY.workerJobs.wholeStates.computing,
  reconnecting: PLAIN_COPY.workerJobs.wholeStates.reconnecting,
};

function isPlainRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isNonNegativeSafeInt(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function isPositiveSafeInt(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0;
}

function isOptionalNullableNumber(value: unknown): boolean {
  return value === undefined || value === null || typeof value === "number";
}

function isOptionalNonNegativeSafeInt(value: unknown): boolean {
  return value === undefined || value === null || isNonNegativeSafeInt(value);
}

function isOptionalNullableString(value: unknown): boolean {
  return value === undefined || value === null || typeof value === "string";
}

function isPartialShardPhase(value: unknown): value is PartialShardPhase {
  return (
    typeof value === "string" &&
    (PARTIAL_SHARD_PHASES as readonly string[]).includes(value)
  );
}

function isValidOptionalRangeShape(value: unknown): boolean {
  if (value === undefined || value === null) return true;
  if (!isPlainRecord(value)) return false;
  return (
    typeof value.startLayer === "number" &&
    typeof value.endLayer === "number" &&
    typeof value.totalLayers === "number"
  );
}

function trimNonEmptyString(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
}

function hasValidShardShape(shard: Record<string, unknown>): boolean {
  return (
    isOptionalNullableString(shard.modelName) &&
    isOptionalNullableString(shard.modelId) &&
    isOptionalNullableString(shard.lastError) &&
    typeof shard.leaseLive === "boolean" &&
    typeof shard.rewardsEnabled === "boolean" &&
    typeof shard.rewardEligible === "boolean" &&
    isOptionalNullableNumber(shard.leaseSeq) &&
    isOptionalNullableNumber(shard.leaseEpoch) &&
    isOptionalNullableNumber(shard.readyExpiresAtUnixMs) &&
    isValidOptionalRangeShape(shard.assignedRange) &&
    isOptionalNonNegativeSafeInt(shard.stepsExecuted) &&
    isOptionalNonNegativeSafeInt(shard.stepsSubmitted) &&
    isOptionalNonNegativeSafeInt(shard.submitRetries) &&
    isOptionalNonNegativeSafeInt(shard.submitStaleConflicts) &&
    isOptionalNonNegativeSafeInt(shard.cancellationsApplied) &&
    isOptionalNonNegativeSafeInt(shard.rebalancesCompleted) &&
    isOptionalNonNegativeSafeInt(shard.reconnectAttempts) &&
    isOptionalNonNegativeSafeInt(shard.lastHeartbeatUnixMs) &&
    isOptionalNonNegativeSafeInt(shard.lastStepSubmittedUnixMs)
  );
}

function invalidView(reason: WorkerStatusInvalidReason): InvalidWorkerView {
  return {
    kind: "invalid",
    reason,
    stateLabel: PLAIN_COPY.workerJobs.invalidState,
    summary:
      reason === "unknown_mode"
        ? PLAIN_COPY.workerJobs.unknownModeSummary
        : PLAIN_COPY.workerJobs.invalidSummary,
    rewardsEnabled: false,
    rewardEligible: false,
  };
}

function formatAssignedRange(range: PartialShardRangeStatus): string {
  return PLAIN_COPY.workerJobs.partialAssignedPart(
    range.endLayer - range.startLayer,
    range.totalLayers,
  );
}

export function isStrictPartialRange(range: unknown): range is PartialShardRangeStatus {
  if (!isPlainRecord(range)) return false;
  const { startLayer, endLayer, totalLayers } = range;
  if (
    !isNonNegativeSafeInt(startLayer) ||
    !isNonNegativeSafeInt(endLayer) ||
    !isPositiveSafeInt(totalLayers)
  ) {
    return false;
  }
  return startLayer < endLayer && endLayer <= totalLayers && endLayer - startLayer < totalLayers;
}

function evaluateActiveLease(
  shard: PartialShardWorkerStatus,
  nowMs: number,
): { valid: boolean; expired: boolean } {
  const expiry = shard.readyExpiresAtUnixMs;
  const hasValidExpiry = isPositiveSafeInt(expiry);
  const expired =
    hasValidExpiry && Number.isFinite(nowMs) && nowMs >= expiry;
  if (!shard.leaseLive) return { valid: false, expired };
  if (!isPositiveSafeInt(shard.leaseEpoch) || !isPositiveSafeInt(shard.leaseSeq)) {
    return { valid: false, expired };
  }
  if (!isStrictPartialRange(shard.assignedRange)) {
    return { valid: false, expired };
  }
  if (!hasValidExpiry || !Number.isFinite(nowMs) || expiry <= nowMs) {
    return { valid: false, expired };
  }
  return { valid: true, expired: false };
}

export function describeWorkerStatus(
  status: WorkerStatus,
  nowMs: number = Date.now(),
): WorkerStatusView {
  const rawMode: unknown = status.mode;
  const rawShard: unknown = status.shard;
  const hasMode = rawMode !== undefined && rawMode !== null;
  const hasShard = rawShard !== undefined && rawShard !== null;

  if (!hasMode) {
    if (hasShard) return invalidView("contradictory_shard");
    const stateLabel = !status.running
      ? PLAIN_COPY.workerJobs.wholeStates.notTakingJobs
      : (status.state && WHOLE_STATE_COPY[status.state]) ||
        PLAIN_COPY.workerJobs.wholeStates.running;
    return {
      kind: "whole",
      running: status.running,
      state: status.state,
      stateLabel,
    };
  }

  if (rawMode !== PARTIAL_SHARD_MODE) {
    return invalidView("unknown_mode");
  }
  if (!hasShard) {
    return invalidView("missing_shard");
  }
  if (!isPlainRecord(rawShard)) {
    return invalidView("malformed_shard");
  }
  if (!isPartialShardPhase(rawShard.phase)) {
    return invalidView("unknown_phase");
  }
  if (!hasValidShardShape(rawShard)) {
    return invalidView("malformed_shard");
  }

  const shard = rawShard as unknown as PartialShardWorkerStatus;
  const reportedPhase = rawShard.phase;

  if (!status.running && reportedPhase !== "failed" && reportedPhase !== "stopped") {
    return invalidView("contradictory_running");
  }

  let phase: PartialShardPhase = reportedPhase;
  let downgradedToReconnecting = false;
  let leaseExpired = false;
  let leaseLive = false;
  let leaseEpoch: number | null = null;
  let leaseSeq: number | null = null;
  let assignedRange: PartialShardRangeStatus | null = null;
  let readyExpiresAtUnixMs: number | null = null;

  if (reportedPhase === "ready" || reportedPhase === "computing") {
    const lease = evaluateActiveLease(shard, nowMs);
    if (lease.valid && shard.assignedRange !== null) {
      leaseLive = true;
      leaseEpoch = shard.leaseEpoch;
      leaseSeq = shard.leaseSeq;
      assignedRange = shard.assignedRange;
      readyExpiresAtUnixMs = shard.readyExpiresAtUnixMs;
    } else {
      phase = "reconnecting";
      downgradedToReconnecting = true;
      leaseExpired = lease.expired;
    }
  } else if (reportedPhase === "loading" && isStrictPartialRange(shard.assignedRange)) {
    assignedRange = shard.assignedRange;
  }

  const summary =
    phase === "reconnecting" && leaseExpired
      ? PLAIN_COPY.workerJobs.partialSummaries.reconnectingExpired
      : PLAIN_COPY.workerJobs.partialSummaries[phase];

  return {
    kind: "partial_shard",
    reportedPhase,
    phase,
    downgradedToReconnecting,
    leaseExpired,
    stateLabel: PLAIN_COPY.workerJobs.partialStates[phase],
    summary,
    modelName: trimNonEmptyString(shard.modelName),
    modelId: trimNonEmptyString(shard.modelId),
    leaseLive,
    leaseEpoch,
    leaseSeq,
    assignedRange,
    assignedRangeLabel: assignedRange ? formatAssignedRange(assignedRange) : null,
    readyExpiresAtUnixMs,
    stepsExecuted: shard.stepsExecuted ?? null,
    stepsSubmitted: shard.stepsSubmitted ?? null,
    lastStepSubmittedUnixMs: shard.lastStepSubmittedUnixMs ?? null,
    lastError: trimNonEmptyString(shard.lastError),
    rewardsEnabled: false,
    rewardEligible: false,
  };
}

export function workerStateCopy(status: WorkerStatus, nowMs: number = Date.now()): string {
  return describeWorkerStatus(status, nowMs).stateLabel;
}
