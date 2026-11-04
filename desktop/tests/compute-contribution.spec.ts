import { expect, test, type Page } from "@playwright/test";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { defaultWorkerThreads } from "../src/lib/types";
import { seedComputePrompt, seedMockOverrides, seedOnboarded } from "./helpers";

/** The node config the app saved, read from its persisted UI state. */
async function savedConfig(page: Page) {
  return page.evaluate(() => {
    const raw = localStorage.getItem("arc-desktop-state-v1");
    return raw ? JSON.parse(raw).config : null;
  });
}

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
    const quarter =
      "Uses up to a quarter of your processor at low priority, and about 8 GB of memory while ARC Node is running. It keeps running on battery. Turn this off to stop ARC jobs and free that memory.";
    // The other caps change the share and nothing else: the node starts at
    // low priority and holds the model whatever its width.
    const rest =
      " at low priority, and about 8 GB of memory while ARC Node is running. It keeps running on battery. Turn this off to stop ARC jobs and free that memory.";
    const whole = `Uses up to your whole processor${rest}`;
    expect(share(6, 24)).toBe(quarter);
    expect(share(3, 12)).toBe(quarter);
    expect(share(1, 16)).toBe(quarter);
    expect(share(7, 24)).toBe(`Uses up to 7 of your 24 processor cores${rest}`);
    // One core of two is more than a quarter, so it is not called one.
    expect(share(1, 2)).toBe(`Uses up to 1 of your 2 processor cores${rest}`);
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

// What the setup step and the Settings note say is what this release does:
// the node starts at low operating-system priority, jobs use at most a
// quarter of the logical cores, the model holds about 8 GB, nothing pauses
// the node on battery, and it runs on after the window closes (the tray keeps
// ARC up). No rule for idle time, heat or free memory exists yet, so none is
// promised. Pure logic: no page.
test.describe("Compute copy states this release's facts", () => {
  test("the setup step and every Settings note say priority, memory and battery", () => {
    const { body, belowMemoryFloor } = PLAIN_COPY.setup.compute;
    expect(body).toBe(
      "ARC Node can run AI jobs for the ARC network on this computer. It uses up to a quarter of your processor, at low priority so your own apps come first, and about 8 GB of memory while it is ready for jobs. It keeps running on battery. First it downloads the ARC model, about 4 GB, in the background. You can turn this off here or at any time in Settings.",
    );
    // Below the memory requirement the step states the same facts.
    for (const fact of [
      "a quarter of your processor",
      "at low priority so your own apps come first",
      "about 8 GB of memory",
      "It keeps running on battery.",
    ]) {
      expect(belowMemoryFloor).toContain(fact);
    }
    const share = PLAIN_COPY.settings.computeShare;
    for (const note of [share(6, 24), share(7, 24), share(24, 24), share(null, 24)]) {
      expect(note).toContain("at low priority");
      expect(note).toContain("about 8 GB of memory while ARC Node is running");
      expect(note).toContain("It keeps running on battery.");
      expect(note).toContain("Turn this off to stop ARC jobs and free that memory.");
      // The node runs on from the tray after the window closes.
      expect(note).not.toContain("while the app is open");
    }
    expect(body).not.toContain("while the app is open");
    expect(belowMemoryFloor).not.toContain("while the app is open");
    for (const copy of [body, belowMemoryFloor, share(6, 24)]) {
      expect(copy).not.toMatch(/\bidle\b|\bheat\b|\bthrottl|\bpause/i);
    }
  });
});

// An install that finished setup before compute started on by default is
// asked once, on the first launch after the update, in the setup step's own
// words. Native code decides and records that it asked before it answers; the
// browser preview stands in for that with `seedComputePrompt`.
test.describe("The one-time compute question", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
    await seedComputePrompt(page);
  });

  test("asks once, in the setup step's words, and Not now leaves compute off", async ({
    page,
  }) => {
    await page.goto("/");
    const dialog = page.getByTestId("compute-prompt-dialog");
    await expect(dialog).toBeVisible();
    await expect(
      dialog.getByRole("heading", { name: PLAIN_COPY.setup.compute.title }),
    ).toBeVisible();
    await expect(dialog.getByTestId("compute-prompt-body")).toHaveText(
      PLAIN_COPY.setup.compute.body,
    );
    await expect(dialog.getByTestId("compute-prompt-turn-on")).toHaveText("Turn on");
    await expect(dialog.getByTestId("compute-prompt-not-now")).toHaveText("Not now");

    await dialog.getByTestId("compute-prompt-not-now").click();
    await expect(dialog).toBeHidden();
    await page.getByTestId("nav-settings").click();
    await expect(page.getByTestId("compute-consent-toggle")).not.toBeChecked();
    // Not now changed nothing: no consent, no core count.
    const saved = await savedConfig(page);
    expect(saved?.computeConsent ?? null).toBeNull();
    expect(saved?.workerThreads ?? null).toBeNull();

    // Never again: the next launch does not ask. The preview answers after a
    // short delay, so give a repeated question time to show before checking.
    await page.reload();
    await expect(page.getByTestId("app-shell")).toBeVisible();
    await page.waitForTimeout(600);
    await expect(page.getByTestId("compute-prompt-dialog")).toHaveCount(0);
  });

  test("Turn on stores consent and a quarter of the cores, and downloads in the background", async ({
    page,
  }) => {
    await page.goto("/");
    const dialog = page.getByTestId("compute-prompt-dialog");
    await dialog.getByTestId("compute-prompt-turn-on").click();
    await expect(dialog).toBeHidden();

    // The model follows in the background, the call setup makes with compute
    // on: the node becomes a worker with the model. (This install is already
    // "worker" with no model, so the model is what to wait for.)
    await expect
      .poll(async () => (await savedConfig(page))?.modelPath)
      .toBe("/mock/.arc/models/standard.gguf");
    const saved = await savedConfig(page);
    expect(saved?.role).toBe("worker");
    expect(saved?.computeConsent).toBe(true);
    // A quarter of the preview's 24 logical cores.
    expect(saved?.workerThreads).toBe(6);

    await page.getByTestId("nav-settings").click();
    await expect(page.getByTestId("compute-consent-toggle")).toBeChecked();
    await expect(page.getByTestId("compute-share-note")).toHaveText(
      "Uses up to a quarter of your processor at low priority, and about 8 GB of memory while ARC Node is running. It keeps running on battery. Turn this off to stop ARC jobs and free that memory.",
    );
  });

  test("Escape closes it without changing anything", async ({ page }) => {
    await page.goto("/");
    const dialog = page.getByTestId("compute-prompt-dialog");
    await expect(dialog).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
    await page.getByTestId("nav-settings").click();
    await expect(page.getByTestId("compute-consent-toggle")).not.toBeChecked();
    const saved = await savedConfig(page);
    expect(saved?.computeConsent ?? null).toBeNull();
  });

  test("a Turn on that fails says why, stays open, and can still be dismissed", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      accept_compute_prompt: { __reject: "The settings could not be saved." },
    });
    await page.goto("/");
    const dialog = page.getByTestId("compute-prompt-dialog");
    await dialog.getByTestId("compute-prompt-turn-on").click();
    await expect(dialog.getByTestId("compute-prompt-error")).toHaveText(
      "The settings could not be saved.",
    );
    await expect(dialog).toBeVisible();
    await expect(dialog.getByTestId("compute-prompt-turn-on")).toBeEnabled();
    await dialog.getByTestId("compute-prompt-not-now").click();
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
