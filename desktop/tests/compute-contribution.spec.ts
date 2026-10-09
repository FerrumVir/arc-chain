import { expect, test } from "@playwright/test";
import { seedMockOverrides, seedOnboarded } from "./helpers";

// Compute contribution is an explicit opt-in. The node only downloads the
// model and takes jobs after the user turns it on, and the dashboard shows
// what this computer actually did, read from its own node.
test.describe("Compute contribution opt-in", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("is off until the user turns it on, and keep-awake waits for it", async ({
    page,
  }) => {
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    const consent = page.getByTestId("compute-consent-toggle");
    const keepAwake = page.getByTestId("prevent-sleep-toggle");
    await expect(consent).not.toBeChecked();
    await expect(keepAwake).toBeDisabled();

    await consent.click();
    await expect(consent).toBeChecked();
    await expect(keepAwake).toBeEnabled();
    await keepAwake.click();
    await expect(keepAwake).toBeChecked();

    // Turning it off is one click and returns the node to observer mode.
    await consent.click();
    await expect(consent).not.toBeChecked();
    await expect(keepAwake).toBeDisabled();
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
