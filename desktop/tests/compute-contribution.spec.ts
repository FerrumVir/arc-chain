import { expect, test } from "@playwright/test";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { defaultWorkerThreads } from "../src/lib/types";
import { seedMockOverrides, seedOnboarded } from "./helpers";

// Compute contribution is the user's answer. New installs start with it on
// at setup where the model can run (onboarding.spec.ts); an install from
// before that keeps its answer, here off. The node only downloads the model
// and takes jobs while it is on, and the dashboard shows what this computer
// actually did, read from its own node.
test.describe("Compute contribution opt-in", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("is off until the user turns it on, and keep-awake waits for it", async ({
    page,
  }) => {
    // The core cap setup stores for a compute-on install (mirrored from
    // commands::default_worker_threads), and the share Settings states.
    for (const [cores, cap] of [
      [1, 1],
      [2, 1],
      [3, 1],
      [4, 1],
      [7, 1],
      [8, 2],
      [12, 3],
      [16, 4],
      [24, 6],
      [64, 16],
    ] as const) {
      expect(defaultWorkerThreads(cores), `${cores} logical cores`).toBe(cap);
    }
    const share = PLAIN_COPY.settings.computeShare;
    const quarter = "Uses up to a quarter of your processor while ARC Node is open.";
    const whole = "Uses up to your whole processor while ARC Node is open.";
    expect(share(6, 24)).toBe(quarter);
    expect(share(3, 12)).toBe(quarter);
    expect(share(1, 16)).toBe(quarter);
    expect(share(7, 24)).toBe("Uses up to 7 of your 24 processor cores while ARC Node is open.");
    // One core of two is more than a quarter, so it is not called one.
    expect(share(1, 2)).toBe("Uses up to 1 of your 2 processor cores while ARC Node is open.");
    expect(share(24, 24)).toBe(whole);
    expect(share(null, 24)).toBe(whole);

    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    const consent = page.getByTestId("compute-consent-toggle");
    const keepAwake = page.getByTestId("prevent-sleep-toggle");
    await expect(consent).not.toBeChecked();
    await expect(keepAwake).toBeDisabled();
    // Off, so nothing is said about processor use.
    await expect(page.getByTestId("compute-share-note")).toHaveCount(0);

    await consent.click();
    await expect(consent).toBeChecked();
    // This install never had a core cap. Turning compute on here keeps that
    // (only setup stores the quarter), and the note says what it means.
    await expect(page.getByTestId("compute-share-note")).toHaveText(whole);
    await expect(keepAwake).toBeEnabled();
    await keepAwake.click();
    await expect(keepAwake).toBeChecked();

    // Turning it off is one click and returns the node to observer mode.
    await consent.click();
    await expect(consent).not.toBeChecked();
    await expect(keepAwake).toBeDisabled();
    await expect(page.getByTestId("compute-share-note")).toHaveCount(0);
  });

  test("the dashboard shows jobs completed and verified from the local node", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      fetch_worker_status: {
        running: true,
        unavailable: null,
        state: "computing",
        publicName: "node-1a2b3c4d",
        coordinatorsRegistered: 5,
        coordinatorsTotal: 6,
        jobsClaimed: 9,
        jobsCompleted: 7,
        jobsVerified: 6,
        jobsFailed: 1,
        jobsDeclined: 1,
        lastJobCompletedUnixMs: Date.now() - 120_000,
        startedUnixMs: Date.now() - 3_600_000,
        preventSleepDuringJobs: false,
      },
    });
    await page.goto("/");
    const card = page.getByTestId("worker-jobs");
    await expect(card.getByTestId("worker-jobs-completed")).toHaveText("7");
    await expect(card.getByTestId("worker-jobs-verified")).toHaveText("6");
    await expect(card.getByTestId("worker-jobs-coordinators")).toHaveText("5 of 6");
    await expect(card.getByTestId("worker-jobs-state")).toHaveText("Computing a job now");
    await expect(card.getByTestId("worker-jobs-detail")).toContainText("node-1a2b3c4d");
  });

  test("the dashboard says contribution is off instead of showing zero jobs", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.getByTestId("worker-jobs-off")).toContainText(
      "Compute contribution is off",
    );
    await expect(page.getByTestId("worker-jobs-completed")).toHaveCount(0);
  });
});

// A stranded v0.7 install reaches this app through the v0.7 legacy bridge release.
// The first launch asks once, in the user's words, before any compute runs.
test.describe("First launch after a v0.7 upgrade", () => {
  const pendingQuestion = {
    load_legacy_compute_question: {
      previousRole: "worker",
      previousModelPath: "/Users/ada/.arc/models/standard.gguf",
      detectedBy: "the v0.7 legacy bridge release ran on this computer",
      recordedUnixMs: 1_759_766_400_000,
    },
  };

  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
    await seedMockOverrides(page, pendingQuestion);
  });

  test("Yes, keep contributing records consent like the Settings switch", async ({
    page,
  }) => {
    await page.goto("/");
    const dialog = page.getByTestId("legacy-upgrade-dialog");
    await expect(dialog).toContainText("Your ARC node is upgrading to the new network.");
    await expect(dialog).toContainText("Keep contributing compute?");
    await expect(dialog.getByTestId("legacy-upgrade-yes")).toHaveText("Yes, keep contributing");
    await expect(dialog.getByTestId("legacy-upgrade-not-now")).toHaveText("Not now");
    await dialog.getByTestId("legacy-upgrade-yes").click();
    await expect(dialog).toBeHidden();
    await page.getByTestId("nav-settings").click();
    await expect(page.getByTestId("compute-consent-toggle")).toBeChecked();
  });

  test("Not now keeps the upgraded node an observer with compute off", async ({
    page,
  }) => {
    await page.goto("/");
    const dialog = page.getByTestId("legacy-upgrade-dialog");
    await dialog.getByTestId("legacy-upgrade-not-now").click();
    await expect(dialog).toBeHidden();
    await page.getByTestId("nav-settings").click();
    await expect(page.getByTestId("compute-consent-toggle")).not.toBeChecked();
  });
});

// Community PR #132: when the configured model file is missing or unreadable,
// the native side starts the node as an observer without it and reports why.
// The app says local compute is off, calmly, and leads to the re-download.
test.describe("Model file unavailable", () => {
  const runningStatus = {
    running: true,
    pid: 42_731,
    health: "live",
    version: "0.8.16",
    peers: 8,
    round: 43_821,
    committed: 43_820,
    height: 43_820,
    uptimeSeconds: 120,
    address: "99".repeat(32),
    rpcPort: 9090,
    lastError: null,
    coordinatorUrl: null,
    chainHost: "http://140.82.16.112:9090",
    // Caught up, so no start-up stage is drawn above the banners.
    chainHeight: 43_830,
    chainRound: 43_831,
    chainBlockAgeSeconds: 4,
    workerThreads: null,
    cpuCores: 24,
  };

  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("a missing model says compute is off and leads to the re-download", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      node_status: { ...runningStatus, modelUnavailable: "notFound" },
    });
    await page.goto("/");
    const banner = page.getByTestId("model-unavailable-banner");
    await expect(banner).toBeVisible();
    await expect(banner).toHaveAttribute("role", "status");
    await expect(banner).toContainText("Model file not found");
    await expect(banner).toContainText("local compute is off");
    await expect(banner).toContainText("re-download the model in Settings");
    // A calm status, not the crash banner.
    await expect(page.getByTestId("crash-banner")).toHaveCount(0);

    await banner.getByTestId("btn-model-unavailable-settings").click();
    await expect(page.getByTestId("settings-screen")).toBeVisible();
    await page.getByTestId("compute-consent-toggle").click();
    await expect(page.getByTestId("compute-consent-toggle")).toBeChecked();
    const unavailable = page.getByTestId("compute-consent-model-unavailable");
    await expect(unavailable).toContainText("model file was not found");
    await unavailable.getByTestId("btn-redownload-model").click();
    await expect(page.getByTestId("compute-consent-error")).toHaveCount(0);
  });

  test("an unreadable model is reported as unreadable, not missing", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      node_status: { ...runningStatus, modelUnavailable: "unreadable" },
    });
    await page.goto("/");
    const banner = page.getByTestId("model-unavailable-banner");
    await expect(banner).toContainText("Model file can't be read");
    await expect(banner).toContainText("permissions");
    await expect(banner).not.toContainText("not found");
  });

  test("no banner while the configured model is in use", async ({ page }) => {
    await seedMockOverrides(page, {
      node_status: { ...runningStatus, modelUnavailable: null },
    });
    await page.goto("/");
    await expect(page.getByTestId("dashboard")).toBeVisible();
    await expect(page.getByTestId("model-unavailable-banner")).toHaveCount(0);
  });
});
