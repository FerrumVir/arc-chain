import { expect, test } from "@playwright/test";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { describeNodeStrip, type NodeStripInput } from "../src/lib/node-strip";
import {
  describeWorkerStatus,
  isStrictPartialRange,
  PARTIAL_SHARD_MODE,
  PARTIAL_SHARD_PHASES,
  workerStateCopy,
} from "../src/lib/worker-status";
import type {
  PartialShardPhase,
  PartialShardRangeStatus,
  PartialShardWorkerStatus,
  WorkerStatus,
} from "../src/lib/types";

const NOW_MS = 1_760_000_000_000;

const VALID_RANGE: PartialShardRangeStatus = {
  startLayer: 0,
  endLayer: 16,
  totalLayers: 32,
};

function wholeWorker(overrides: Partial<WorkerStatus> = {}): WorkerStatus {
  return {
    running: true,
    unavailable: null,
    state: "polling",
    publicName: "oak-01",
    coordinatorsRegistered: 1,
    coordinatorsTotal: 1,
    jobsClaimed: 0,
    jobsCompleted: 0,
    jobsVerified: 0,
    jobsFailed: 0,
    jobsDeclined: 0,
    lastJobCompletedUnixMs: null,
    startedUnixMs: NOW_MS - 60_000,
    preventSleepDuringJobs: true,
    ...overrides,
  };
}

function partialShard(
  overrides: Partial<PartialShardWorkerStatus> = {},
): PartialShardWorkerStatus {
  return {
    phase: "ready",
    modelName: "arc-7b",
    modelId: "model-1",
    leaseSeq: 4,
    leaseEpoch: 2,
    assignedRange: VALID_RANGE,
    readyExpiresAtUnixMs: NOW_MS + 30_000,
    leaseLive: true,
    stepsExecuted: 0,
    stepsSubmitted: 0,
    submitRetries: 0,
    submitStaleConflicts: 0,
    cancellationsApplied: 0,
    rebalancesCompleted: 0,
    reconnectAttempts: 0,
    lastHeartbeatUnixMs: NOW_MS - 1_000,
    lastStepSubmittedUnixMs: null,
    lastError: null,
    rewardsEnabled: false,
    rewardEligible: false,
    ...overrides,
  };
}

function partialWorker(
  shardOverrides: Partial<PartialShardWorkerStatus> = {},
  workerOverrides: Partial<WorkerStatus> = {},
): WorkerStatus {
  return wholeWorker({
    mode: PARTIAL_SHARD_MODE,
    shard: partialShard(shardOverrides),
    ...workerOverrides,
  });
}

function stripInput(overrides: Partial<NodeStripInput> = {}): NodeStripInput {
  return {
    status: {
      running: true,
      pid: 4242,
      lastError: null,
      health: "live",
      coordinatorUrl: "https://140.82.16.112",
    },
    startBlock: null,
    startError: null,
    update: {
      phase: "available",
      canInstall: false,
      message: "",
      error: null,
      restartRequired: false,
    },
    modelProgress: null,
    sharing: true,
    sharingEligible: true,
    sharingPending: false,
    network: null,
    nowMs: NOW_MS,
    ...overrides,
  };
}

test.describe("worker status and partial shard honesty", () => {
  test("covers every partial shard phase with plain copy, tones, and non-earning flags", () => {
    const cases: Array<{
      phase: PartialShardPhase;
      expectedTone: "ok" | "busy" | "attention" | "idle";
      expectedStrip: string;
      expectsLiveLease: boolean;
      expectedAssignedRange: PartialShardRangeStatus | null;
      lastError?: string | null;
    }> = [
      {
        phase: "starting",
        expectedTone: "busy",
        expectedStrip: PLAIN_COPY.nodeStrip.partialStarting,
        expectsLiveLease: false,
        expectedAssignedRange: null,
      },
      {
        phase: "loading",
        expectedTone: "busy",
        expectedStrip: PLAIN_COPY.nodeStrip.partialLoading,
        expectsLiveLease: false,
        expectedAssignedRange: VALID_RANGE,
      },
      {
        phase: "ready",
        expectedTone: "ok",
        expectedStrip: PLAIN_COPY.nodeStrip.partialReady,
        expectsLiveLease: true,
        expectedAssignedRange: VALID_RANGE,
      },
      {
        phase: "computing",
        expectedTone: "busy",
        expectedStrip: PLAIN_COPY.nodeStrip.partialComputing,
        expectsLiveLease: true,
        expectedAssignedRange: VALID_RANGE,
      },
      {
        phase: "reconnecting",
        expectedTone: "busy",
        expectedStrip: PLAIN_COPY.nodeStrip.partialReconnecting,
        expectsLiveLease: false,
        expectedAssignedRange: null,
      },
      {
        phase: "failed",
        expectedTone: "attention",
        expectedStrip: PLAIN_COPY.nodeStrip.partialFailed,
        expectsLiveLease: false,
        expectedAssignedRange: null,
        lastError: "model partition crashed",
      },
      {
        phase: "stopped",
        expectedTone: "idle",
        expectedStrip: PLAIN_COPY.nodeStrip.partialStopped,
        expectsLiveLease: false,
        expectedAssignedRange: null,
        lastError: "stopped by host",
      },
    ];

    expect(cases.map((c) => c.phase)).toEqual([...PARTIAL_SHARD_PHASES]);

    for (const item of cases) {
      const worker = partialWorker({
        phase: item.phase,
        lastError: item.lastError ?? null,
      });
      const view = describeWorkerStatus(worker, NOW_MS);
      expect(view.kind).toBe("partial_shard");
      if (view.kind !== "partial_shard") continue;
      expect(view.phase).toBe(item.phase);
      expect(view.downgradedToReconnecting).toBe(false);
      expect(view.stateLabel).toBe(PLAIN_COPY.workerJobs.partialStates[item.phase]);
      expect(view.summary).toBe(PLAIN_COPY.workerJobs.partialSummaries[item.phase]);
      expect(view.leaseLive).toBe(item.expectsLiveLease);
      expect(view.assignedRange).toEqual(item.expectedAssignedRange);
      expect(view.rewardsEnabled).toBe(false);
      expect(view.rewardEligible).toBe(false);
      expect(workerStateCopy(worker, NOW_MS)).toBe(
        PLAIN_COPY.workerJobs.partialStates[item.phase],
      );

      const strip = describeNodeStrip(stripInput({ worker }));
      expect(strip.sentence).toBe(item.expectedStrip);
      expect(strip.tone).toBe(item.expectedTone);
      expect(strip.details).toBe(item.lastError ?? null);
      expect(strip.sentence.toLowerCase()).not.toContain("answered");
    }
  });

  test("ready and computing require positive lease epoch/seq, strict partial range, live lease, and unexpired TTL else reconnecting", () => {
    expect(isStrictPartialRange({ startLayer: 0, endLayer: 16, totalLayers: 32 })).toBe(true);
    expect(isStrictPartialRange({ startLayer: 16, endLayer: 32, totalLayers: 32 })).toBe(true);
    expect(isStrictPartialRange({ startLayer: 5, endLayer: 6, totalLayers: 32 })).toBe(true);
    expect(isStrictPartialRange({ startLayer: 0, endLayer: 32, totalLayers: 32 })).toBe(false);
    expect(isStrictPartialRange({ startLayer: 0, endLayer: 1, totalLayers: 1 })).toBe(false);
    expect(isStrictPartialRange({ startLayer: 8, endLayer: 8, totalLayers: 32 })).toBe(false);
    expect(isStrictPartialRange({ startLayer: 16, endLayer: 8, totalLayers: 32 })).toBe(false);
    expect(isStrictPartialRange({ startLayer: 16, endLayer: 33, totalLayers: 32 })).toBe(false);
    expect(isStrictPartialRange({ startLayer: -1, endLayer: 8, totalLayers: 32 })).toBe(false);

    const justBeforeExpiry = partialWorker({
      phase: "computing",
      readyExpiresAtUnixMs: NOW_MS + 1,
    });
    const activeView = describeWorkerStatus(justBeforeExpiry, NOW_MS);
    expect(activeView).toMatchObject({
      kind: "partial_shard",
      phase: "computing",
      leaseLive: true,
      leaseEpoch: 2,
      leaseSeq: 4,
      assignedRange: VALID_RANGE,
      assignedRangeLabel: "16 of 32 layers",
      rewardsEnabled: false,
      rewardEligible: false,
    });

    const invalidLeaseCases: Array<{
      label: string;
      overrides: Partial<PartialShardWorkerStatus>;
      expectExpired: boolean;
    }> = [
      { label: "exact TTL boundary", overrides: { readyExpiresAtUnixMs: NOW_MS }, expectExpired: true },
      { label: "past TTL", overrides: { readyExpiresAtUnixMs: NOW_MS - 1 }, expectExpired: true },
      { label: "missing TTL", overrides: { readyExpiresAtUnixMs: null }, expectExpired: false },
      { label: "zero TTL", overrides: { readyExpiresAtUnixMs: 0 }, expectExpired: false },
      { label: "leaseLive false", overrides: { leaseLive: false }, expectExpired: false },
      { label: "zero epoch", overrides: { leaseEpoch: 0 }, expectExpired: false },
      { label: "null epoch", overrides: { leaseEpoch: null }, expectExpired: false },
      { label: "negative epoch", overrides: { leaseEpoch: -1 }, expectExpired: false },
      { label: "zero seq", overrides: { leaseSeq: 0 }, expectExpired: false },
      { label: "null seq", overrides: { leaseSeq: null }, expectExpired: false },
      { label: "float seq", overrides: { leaseSeq: 1.5 }, expectExpired: false },
      { label: "null range", overrides: { assignedRange: null }, expectExpired: false },
      {
        label: "whole model range",
        overrides: { assignedRange: { startLayer: 0, endLayer: 32, totalLayers: 32 } },
        expectExpired: false,
      },
      {
        label: "empty range",
        overrides: { assignedRange: { startLayer: 8, endLayer: 8, totalLayers: 32 } },
        expectExpired: false,
      },
      {
        label: "inverted range",
        overrides: { assignedRange: { startLayer: 20, endLayer: 10, totalLayers: 32 } },
        expectExpired: false,
      },
      {
        label: "out of bounds range",
        overrides: { assignedRange: { startLayer: 16, endLayer: 33, totalLayers: 32 } },
        expectExpired: false,
      },
    ];

    for (const reportedPhase of ["ready", "computing"] as const) {
      for (const testCase of invalidLeaseCases) {
        const worker = partialWorker({ phase: reportedPhase, ...testCase.overrides });
        const view = describeWorkerStatus(worker, NOW_MS);
        expect(view, `${reportedPhase}: ${testCase.label}`).toMatchObject({
          kind: "partial_shard",
          reportedPhase,
          phase: "reconnecting",
          downgradedToReconnecting: true,
          leaseExpired: testCase.expectExpired,
          leaseLive: false,
          leaseEpoch: null,
          leaseSeq: null,
          assignedRange: null,
          assignedRangeLabel: null,
          readyExpiresAtUnixMs: null,
          rewardsEnabled: false,
          rewardEligible: false,
        });
        const strip = describeNodeStrip(stripInput({ worker }));
        expect(strip.sentence).toBe(PLAIN_COPY.nodeStrip.partialReconnecting);
        expect(strip.tone).toBe("busy");
      }
    }
  });

  test("failed and stopped stay honest despite healthy NodeStatus and regardless of worker.running", () => {
    const healthyNodeVariants: Array<NodeStripInput["status"]> = [
      { running: true, pid: 4242, lastError: null, health: "live", coordinatorUrl: "https://140.82.16.112" },
      { running: true, pid: 4242, lastError: null, health: "lite", coordinatorUrl: "https://140.82.16.112" },
      { running: true, pid: 4242, lastError: null, coordinatorUrl: null },
    ];

    for (const phase of ["failed", "stopped"] as const) {
      for (const running of [true, false]) {
        const worker = partialWorker(
          {
            phase,
            leaseLive: true,
            lastError: phase === "failed" ? "backend stopped responding" : null,
          },
          {
            running,
            state: "polling",
            jobsCompleted: 9,
          },
        );
        const view = describeWorkerStatus(worker, NOW_MS);
        expect(view).toMatchObject({
          kind: "partial_shard",
          phase,
          leaseLive: false,
          leaseEpoch: null,
          leaseSeq: null,
          assignedRange: null,
          readyExpiresAtUnixMs: null,
          rewardsEnabled: false,
          rewardEligible: false,
        });

        for (const status of healthyNodeVariants) {
          const strip = describeNodeStrip(stripInput({ status, worker }));
          expect(strip.sentence).toBe(
            phase === "failed"
              ? PLAIN_COPY.nodeStrip.partialFailed
              : PLAIN_COPY.nodeStrip.partialStopped,
          );
          expect(strip.tone).toBe(phase === "failed" ? "attention" : "idle");
          expect(strip.details).toBe(phase === "failed" ? "backend stopped responding" : null);
        }
      }
    }
  });

  test("unknown modes, contradictory fields, and malformed shards fail closed and never fall back to whole worker", () => {
    const invalidPayloads: Array<{ label: string; worker: WorkerStatus; expectedReason: string }> = [
      {
        label: "explicit whole token is not valid",
        worker: wholeWorker({ mode: "whole", jobsCompleted: 5 }),
        expectedReason: "unknown_mode",
      },
      {
        label: "unknown future partial mode",
        worker: wholeWorker({ mode: "pipeline_v2", jobsCompleted: 5 }),
        expectedReason: "unknown_mode",
      },
      {
        label: "empty string mode",
        worker: wholeWorker({ mode: "", jobsCompleted: 5 }),
        expectedReason: "unknown_mode",
      },
      {
        label: "non-string mode",
        worker: wholeWorker({ mode: 123 as unknown as string }),
        expectedReason: "unknown_mode",
      },
      {
        label: "shard present without mode",
        worker: wholeWorker({ shard: partialShard(), jobsCompleted: 5 }),
        expectedReason: "contradictory_shard",
      },
      {
        label: "partial_shard without shard payload",
        worker: wholeWorker({ mode: PARTIAL_SHARD_MODE, shard: null, jobsCompleted: 5 }),
        expectedReason: "missing_shard",
      },
      {
        label: "partial_shard with non-object shard",
        worker: wholeWorker({
          mode: PARTIAL_SHARD_MODE,
          shard: "bad" as unknown as PartialShardWorkerStatus,
        }),
        expectedReason: "malformed_shard",
      },
      {
        label: "unknown shard phase",
        worker: partialWorker({ phase: "polling" }),
        expectedReason: "unknown_phase",
      },
      {
        label: "null shard phase",
        worker: partialWorker({ phase: null }),
        expectedReason: "unknown_phase",
      },
      {
        label: "non-terminal phase with running false",
        worker: partialWorker({ phase: "ready" }, { running: false }),
        expectedReason: "contradictory_running",
      },
      {
        label: "computing phase with running false",
        worker: partialWorker({ phase: "computing" }, { running: false }),
        expectedReason: "contradictory_running",
      },
      {
        label: "malformed negative counter",
        worker: partialWorker({ stepsSubmitted: -1 }),
        expectedReason: "malformed_shard",
      },
      {
        label: "malformed NaN counter",
        worker: partialWorker({ stepsExecuted: Number.NaN }),
        expectedReason: "malformed_shard",
      },
      {
        label: "malformed assignedRange non-object",
        worker: partialWorker({
          assignedRange: "0..16" as unknown as PartialShardRangeStatus,
        }),
        expectedReason: "malformed_shard",
      },
      {
        label: "malformed non-boolean leaseLive",
        worker: partialWorker({ leaseLive: "yes" as unknown as boolean }),
        expectedReason: "malformed_shard",
      },
      {
        label: "malformed non-boolean rewardsEnabled",
        worker: partialWorker({ rewardsEnabled: 1 as unknown as boolean }),
        expectedReason: "malformed_shard",
      },
    ];

    for (const item of invalidPayloads) {
      const view = describeWorkerStatus(item.worker, NOW_MS);
      expect(view, item.label).toMatchObject({
        kind: "invalid",
        reason: item.expectedReason,
        stateLabel: PLAIN_COPY.workerJobs.invalidState,
        rewardsEnabled: false,
        rewardEligible: false,
      });
      expect(workerStateCopy(item.worker, NOW_MS)).toBe(PLAIN_COPY.workerJobs.invalidState);

      const strip = describeNodeStrip(stripInput({ worker: item.worker }));
      expect(strip.sentence).toBe(PLAIN_COPY.nodeStrip.workerStatusUnavailable);
      expect(strip.tone).toBe("attention");
      expect(strip.sentence.toLowerCase()).not.toContain("answered");
    }
  });

  test("never substitutes whole-job counts, forces reward flags false, and never says answered for partial steps", () => {
    const worker = partialWorker(
      {
        phase: "ready",
        stepsExecuted: 4,
        stepsSubmitted: 3,
        lastStepSubmittedUnixMs: null,
        rewardsEnabled: true,
        rewardEligible: true,
      },
      {
        state: "polling",
        jobsCompleted: 42,
        jobsVerified: 40,
        lastJobCompletedUnixMs: NOW_MS - 5_000,
      },
    );

    const view = describeWorkerStatus(worker, NOW_MS);
    expect(view).toMatchObject({
      kind: "partial_shard",
      phase: "ready",
      stepsExecuted: 4,
      stepsSubmitted: 3,
      lastStepSubmittedUnixMs: null,
      rewardsEnabled: false,
      rewardEligible: false,
    });

    const strip = describeNodeStrip(stripInput({ worker }));
    expect(strip.sentence).toBe(PLAIN_COPY.nodeStrip.partialSteps(3));
    expect(strip.sentence).not.toContain("42");
    expect(strip.sentence.toLowerCase()).not.toContain("answered");
    expect(strip.tone).toBe("ok");

    const singleStepStrip = describeNodeStrip(
      stripInput({ worker: partialWorker({ phase: "ready", stepsSubmitted: 1 }, { jobsCompleted: 11 }) }),
    );
    expect(singleStepStrip.sentence).toBe(PLAIN_COPY.nodeStrip.partialSteps(1));
    expect(singleStepStrip.sentence).toContain("1 shared AI step");
    expect(singleStepStrip.sentence.toLowerCase()).not.toContain("answered");

    const jargon =
      /rpc|quic|ttl|lease|epoch|seq|shard|telemetry|tensor|activation|grpc|serde|partial_shard|lite|sync|offline|degraded|peer/i;
    const plainCopyStrings = [
      PLAIN_COPY.nodeStrip.partialStarting,
      PLAIN_COPY.nodeStrip.partialLoading,
      PLAIN_COPY.nodeStrip.partialReady,
      PLAIN_COPY.nodeStrip.partialSteps(1),
      PLAIN_COPY.nodeStrip.partialSteps(3),
      PLAIN_COPY.nodeStrip.partialComputing,
      PLAIN_COPY.nodeStrip.partialReconnecting,
      PLAIN_COPY.nodeStrip.partialFailed,
      PLAIN_COPY.nodeStrip.partialStopped,
      PLAIN_COPY.nodeStrip.workerStatusUnavailable,
      ...Object.values(PLAIN_COPY.workerJobs.partialStates),
      ...Object.values(PLAIN_COPY.workerJobs.partialSummaries),
      PLAIN_COPY.workerJobs.invalidState,
      PLAIN_COPY.workerJobs.invalidSummary,
      PLAIN_COPY.workerJobs.unknownModeSummary,
      PLAIN_COPY.workerJobs.partialNoRewards,
      PLAIN_COPY.workerJobs.partialNoStepsYet,
    ];
    for (const text of plainCopyStrings) {
      expect(text).not.toMatch(jargon);
    }
  });

  test("preserves legacy whole-worker behavior and existing sharing eligibility and consent gates", () => {
    expect(workerStateCopy(wholeWorker({ running: false }))).toBe("Not taking jobs");
    expect(workerStateCopy(wholeWorker({ state: "polling" }))).toBe("Ready: waiting for a job");
    expect(workerStateCopy(wholeWorker({ state: "computing" }))).toBe("Computing a job now");
    expect(workerStateCopy(wholeWorker({ state: "reconnecting" }))).toBe(
      "Reconnecting to coordinators",
    );
    expect(workerStateCopy(wholeWorker({ state: null }))).toBe("Running");

    const wholeReady = describeNodeStrip(
      stripInput({ worker: wholeWorker({ state: "polling", jobsCompleted: 0 }) }),
    );
    expect(wholeReady).toMatchObject({ sentence: PLAIN_COPY.nodeStrip.ready, tone: "ok" });

    const wholeAnswered = describeNodeStrip(
      stripInput({ worker: wholeWorker({ state: "polling", jobsCompleted: 3 }) }),
    );
    expect(wholeAnswered).toMatchObject({ sentence: PLAIN_COPY.nodeStrip.answered(3), tone: "ok" });

    const wholeComputing = describeNodeStrip(
      stripInput({ worker: wholeWorker({ state: "computing" }) }),
    );
    expect(wholeComputing).toMatchObject({ sentence: PLAIN_COPY.nodeStrip.computing, tone: "busy" });

    const wholeReconnecting = describeNodeStrip(
      stripInput({ worker: wholeWorker({ state: "reconnecting" }) }),
    );
    expect(wholeReconnecting).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.reconnectingWorker,
      tone: "busy",
    });

    const wholeNotRunning = describeNodeStrip(
      stripInput({ worker: wholeWorker({ running: false }) }),
    );
    expect(wholeNotRunning).toMatchObject({ sentence: PLAIN_COPY.nodeStrip.preparing, tone: "busy" });

    const ineligiblePartial = describeNodeStrip(
      stripInput({ sharingEligible: false, worker: partialWorker({ phase: "ready" }) }),
    );
    expect(ineligiblePartial).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.sharingIneligible,
      action: null,
      tone: "idle",
    });

    const unconsentedPartial = describeNodeStrip(
      stripInput({ sharing: false, worker: partialWorker({ phase: "ready" }) }),
    );
    expect(unconsentedPartial).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.sharingOff,
      action: "share-computer",
      tone: "ok",
    });
  });
});
