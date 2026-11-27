import { expect, test } from "@playwright/test";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { seedMockOverrides, seedOnboarded } from "./helpers";

for (const phase of ["ready", "computing", "failed", "stopped"] as const) {
  test(`partial worker ${phase} displays steps without whole-job or reward claims`, async ({ page }, testInfo) => {
    await seedOnboarded(page);
    await page.addInitScript(() => {
      const raw = localStorage.getItem("arc-desktop-state-v1");
      if (!raw) throw new Error("onboarded fixture is missing");
      const state = JSON.parse(raw);
      state.config.computeConsent = true;
      state.config.modelPath = "/models/standard.gguf";
      localStorage.setItem("arc-desktop-state-v1", JSON.stringify(state));
    });
    await seedMockOverrides(page, {
      fetch_worker_status: {
        running: phase !== "failed" && phase !== "stopped",
        unavailable: null,
        state: "polling",
        publicName: "partial-node",
        coordinatorsRegistered: 1,
        coordinatorsTotal: 1,
        jobsClaimed: 999,
        jobsCompleted: 999,
        jobsVerified: 999,
        jobsFailed: 0,
        jobsDeclined: 0,
        lastJobCompletedUnixMs: null,
        startedUnixMs: Date.now(),
        preventSleepDuringJobs: false,
        mode: "partial_shard",
        shard: {
          phase,
          modelName: "fixture-model",
          modelId: "ab".repeat(32),
          leaseSeq: 1,
          leaseEpoch: 1,
          assignedRange: { startLayer: 0, endLayer: 6, totalLayers: 32 },
          readyExpiresAtUnixMs: Date.now() + 120_000,
          leaseLive: phase === "ready" || phase === "computing",
          stepsExecuted: 7,
          stepsSubmitted: 6,
          submitRetries: 0,
          submitStaleConflicts: 0,
          cancellationsApplied: 0,
          rebalancesCompleted: 0,
          reconnectAttempts: 0,
          lastHeartbeatUnixMs: Date.now(),
          lastStepSubmittedUnixMs: null,
          lastError: phase === "failed" ? "Coordinator authentication failed." : null,
          rewardsEnabled: false,
          rewardEligible: false,
        },
      },
    });
    await page.goto("/");
    const card = page.getByTestId("worker-jobs");
    await expect(card.getByTestId("worker-jobs-state")).toHaveText(PLAIN_COPY.workerJobs.partialStates[phase]);
    await expect(card.getByTestId("worker-jobs-steps-executed")).toHaveText("7");
    await expect(card.getByTestId("worker-jobs-steps-submitted")).toHaveText("6");
    await expect(card.getByTestId("worker-jobs-completed")).toHaveCount(0);
    await expect(card.getByTestId("worker-jobs-verified")).toHaveCount(0);
    await expect(card.getByTestId("worker-jobs-detail")).toContainText(PLAIN_COPY.workerJobs.partialNoRewards);
    await expect(card.getByTestId("worker-jobs-detail")).not.toContainText(PLAIN_COPY.workerJobs.partialNoStepsYet);
    await expect(card.getByTestId("worker-jobs-assigned-range")).toHaveText(phase === "ready" || phase === "computing" ? "6 of 32 layers" : "—");
    await expect(page.locator("#boot")).toHaveCount(0);
    await card.getByTestId("worker-jobs-steps-executed").click({ trial: true });
    await card.screenshot({ path: testInfo.outputPath(`partial-worker-${phase}.png`) });
  });
}
