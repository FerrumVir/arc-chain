import { expect, test, type Page } from "@playwright/test";
import { seedMockOverrides, seedOnboarded } from "./helpers";

// The node's memory guard (arc-chain #228) reports `paused_memory_guard` while it holds new
// jobs. The dashboard must say so honestly instead of falling back to "Running", by reason
// only when the node reported one, and with nothing about when or whether it resumes.
// Synthetic: every status below is a mocked `fetch_worker_status`; no node runs.

const BASE = {
  running: true,
  unavailable: null,
  publicName: "node-1a2b3c4d",
  coordinatorsRegistered: 5,
  coordinatorsTotal: 6,
  jobsClaimed: 9,
  jobsCompleted: 7,
  jobsVerified: 6,
  jobsFailed: 1,
  jobsDeclined: 1,
  lastJobCompletedUnixMs: null as number | null,
  startedUnixMs: null as number | null,
  preventSleepDuringJobs: false,
};

function status(state: string, guard?: { memoryGuardState?: string | null; memoryCheck?: string | null }) {
  return { ...BASE, state, ...(guard ?? {}) };
}

async function openWith(page: Page, workerStatus: unknown) {
  await seedOnboarded(page);
  await seedMockOverrides(page, { fetch_worker_status: workerStatus });
  await page.goto("/");
  return page.getByTestId("worker-jobs");
}

const NO_RESUME_PROMISE = /resum|automatic|back soon|protect|load/i;

test.describe("Memory guard pause on the dashboard", () => {
  for (const [guardState, check, copy] of [
    ["paused_low_memory", "ok", "Paused: not enough free memory for new jobs"],
    ["paused_memory_check_failed", "failed", "Paused: ARC can't check free memory"],
    ["paused_decline_backoff", "ok", "Paused: recent jobs needed more free memory"],
  ] as const) {
    test(`${guardState} reads as its own pause, never "Running"`, async ({ page }) => {
      const card = await openWith(page, status("paused_memory_guard", { memoryGuardState: guardState, memoryCheck: check }));
      const state = card.getByTestId("worker-jobs-state");
      await expect(state).toHaveText(copy);
      await expect(state).not.toHaveText(/running/i);
      await expect(state).not.toHaveText(NO_RESUME_PROMISE);
      const detail = card.getByTestId("worker-jobs-detail");
      await expect(detail).toContainText("While paused, this computer doesn't take new jobs.");
      await expect(detail).not.toContainText(NO_RESUME_PROMISE);
      // The counters still show what the node reported; nothing claims new work.
      await expect(card.getByTestId("worker-jobs-completed")).toHaveText("7");
    });
  }

  test("missing or unknown pause details fall back to a plain pause", async ({ page }) => {
    for (const guard of [undefined, { memoryGuardState: null, memoryCheck: null }, { memoryGuardState: "something_new" }]) {
      const card = await openWith(page, status("paused_memory_guard", guard));
      await expect(card.getByTestId("worker-jobs-state")).toHaveText("Paused: not taking new jobs");
      await expect(card.getByTestId("worker-jobs-detail")).toContainText("While paused");
    }
  });

  test("guard details never override a worker that is not paused", async ({ page }) => {
    // A failed memory check with the `proceed` policy keeps the worker polling.
    const card = await openWith(page, status("polling", { memoryGuardState: "ok", memoryCheck: "failed" }));
    await expect(card.getByTestId("worker-jobs-state")).toHaveText("Ready: waiting for a job");
    await expect(card.getByTestId("worker-jobs-detail")).not.toContainText("While paused");
  });

  test("returns to the polling copy when the node reports polling again", async ({ page }) => {
    const card = await openWith(page, status("paused_memory_guard", { memoryGuardState: "paused_low_memory" }));
    await expect(card.getByTestId("worker-jobs-state")).toHaveText("Paused: not enough free memory for new jobs");
    await page.evaluate((next) => {
      (window as unknown as { __ARC_MOCK__: Record<string, unknown> }).__ARC_MOCK__.fetch_worker_status = next;
    }, status("polling", { memoryGuardState: "ok", memoryCheck: "ok" }));
    // The card refetches every 5 s.
    await expect(card.getByTestId("worker-jobs-state")).toHaveText("Ready: waiting for a job", { timeout: 12_000 });
    await expect(card.getByTestId("worker-jobs-detail")).not.toContainText("While paused");
  });

  test("existing states are unchanged", async ({ page }) => {
    for (const [state, copy] of [
      ["polling", "Ready: waiting for a job"],
      ["computing", "Computing a job now"],
      ["reconnecting", "Reconnecting to coordinators"],
      ["some_future_state", "Running"],
    ] as const) {
      const card = await openWith(page, status(state));
      await expect(card.getByTestId("worker-jobs-state")).toHaveText(copy);
      await expect(card.getByTestId("worker-jobs-detail")).not.toContainText("While paused");
    }
  });
});

// Mocked preview for review at the desktop window's minimum (960×640), default (1180×780)
// and the test suite's (1280×820) sizes. Run:
//   npx playwright test tests/worker-guard-status.spec.ts -g "preview"
// Images go to test-results/worker-guard-preview/ (git-ignored).
test.describe("Memory guard pause preview", () => {
  for (const [width, height] of [
    [960, 640],
    [1180, 780],
    [1280, 820],
  ] as const) {
    for (const [name, guard] of [
      ["low-memory", { memoryGuardState: "paused_low_memory", memoryCheck: "ok" }],
      ["check-failed", { memoryGuardState: "paused_memory_check_failed", memoryCheck: "failed" }],
      ["no-details", undefined],
    ] as const) {
      test(`preview ${name} at ${width}x${height}`, async ({ page }) => {
        await page.setViewportSize({ width, height });
        const card = await openWith(page, status("paused_memory_guard", guard));
        await page.locator("#boot").waitFor({ state: "detached" });
        await card.scrollIntoViewIfNeeded();
        await expect(card.getByTestId("worker-jobs-state")).toBeVisible();
        await card.screenshot({ path: `test-results/worker-guard-preview/worker-guard-${name}-${width}x${height}.png` });
        await page.screenshot({ path: `test-results/worker-guard-preview/worker-guard-${name}-${width}x${height}-page.png` });
      });
    }
  }
});
