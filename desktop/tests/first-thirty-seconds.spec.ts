import { expect, test, type Page } from "@playwright/test";
import { describeNodeStrip, onlineComputerCount, type NodeStripInput } from "../src/lib/node-strip";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { CONNECTING_LONG_SECS } from "../src/lib/connection";
import type { WorkerStatus } from "../src/lib/types";
import { clearState, openOn, seedComputePrompt, seedMockOverrides, seedOnboarded } from "./helpers";

const worker: WorkerStatus = {
  running: true, state: "polling", unavailable: null, publicName: "test worker",
  jobsCompleted: 3, jobsVerified: 0, jobsClaimed: 3, jobsFailed: 0, jobsDeclined: 0,
  coordinatorsRegistered: 1, coordinatorsTotal: 1, lastJobCompletedUnixMs: Date.now() - 60_000,
  startedUnixMs: 1, preventSleepDuringJobs: false,
};
const status = {
  running: true, pid: 1234, health: "live", lastError: null, coordinatorUrl: null,
  version: "0.8.21", peers: 1, round: 1, committed: 1, height: 1,
  uptimeSeconds: 120, address: "99".repeat(32), rpcPort: 9090, cpuCores: 8, workerThreads: 2,
};
const network = { count_total: 1, count_visible: 1, eligible_inference_workers: 1 };
const base: NodeStripInput = {
  status: { running: true, pid: 1234, lastError: null, health: "live" },
  startBlock: null, startError: null, modelProgress: null,
  update: { phase: "idle", canInstall: true, message: "", error: null, restartRequired: false },
  worker, network, sharing: true, sharingEligible: true,
};

test("Home distinguishes idle, active, completed, not sharing, and unavailable data", () => {
  expect(describeNodeStrip(base).sentence).toBe("Your computer has answered 3 questions since it started. 1 computer is online.");
  expect(describeNodeStrip({ ...base, worker: { ...worker, jobsCompleted: 1 } }).sentence).toContain("1 question since");
  expect(describeNodeStrip({ ...base, worker: { ...worker, jobsCompleted: 0 } }).sentence).toContain(PLAIN_COPY.nodeStrip.ready);
  expect(describeNodeStrip({ ...base, worker: { ...worker, state: "computing" } }).sentence).toContain(PLAIN_COPY.nodeStrip.computing);
  expect(describeNodeStrip({ ...base, worker: { ...worker, state: "reconnecting" } }).sentence).toContain(PLAIN_COPY.nodeStrip.reconnectingWorker);
  const off = describeNodeStrip({ ...base, sharing: false });
  expect(off.sentence).toBe("Your computer is not sharing. 1 computer is online.");
  expect(off.action).toBe("share-computer");
  expect(describeNodeStrip({ ...base, network: null }).sentence).toBe(PLAIN_COPY.nodeStrip.answered(3));
  expect(describeNodeStrip({ ...base, worker: undefined }).sentence).toContain(PLAIN_COPY.nodeStrip.preparing);
  expect(describeNodeStrip({ ...base, worker: { ...worker, running: false } }).sentence).toBe(PLAIN_COPY.nodeStrip.preparing + " " + PLAIN_COPY.nodeStrip.online(1));
  expect(describeNodeStrip({ ...base, network: { ...network, count_total: 2, count_visible: 2 } }).sentence).toContain("2 computers are online.");
});

test("stale or truncated registries do not claim an online total", () => {
  for (const counters of [
    { ...network, count_total: 70, count_visible: 50 },
    { ...network, count_total: 3, count_visible: 1 },
    { ...network, count_visible: null },
  ]) {
    expect(onlineComputerCount(counters)).toBeNull();
    expect(describeNodeStrip({ ...base, network: counters }).sentence).toContain("registered.");
  }
  expect(onlineComputerCount({ ...network, count_total: -1, count_visible: -1 })).toBeNull();
  expect(describeNodeStrip({ ...base, network: { ...network, count_total: NaN } }).sentence).toBe(PLAIN_COPY.nodeStrip.answered(3));
  expect(onlineComputerCount({ ...network, count_total: 0, count_visible: 0 })).toBe(0);
  expect(describeNodeStrip({ ...base, network: { ...network, count_total: 0, count_visible: 0 } }).sentence).toBe(PLAIN_COPY.nodeStrip.answered(3));
});

test("Home reports download amounts and retries while sharing is being enabled", () => {
  const pending = { ...base, sharing: false, sharingPending: true };
  expect(describeNodeStrip(pending)).toMatchObject({ sentence: PLAIN_COPY.nodeStrip.enablingSharing, action: null, tone: "busy" });
  const progress = {
    tier: "standard", downloadedBytes: 1024 ** 3, totalBytes: 4_081_004_224,
    done: false, stage: "downloading" as const,
  };
  expect(describeNodeStrip({ ...pending, modelProgress: progress }).sentence).toContain("1.00 GB of 3.80 GB (26%)");
  expect(describeNodeStrip({ ...pending, modelProgress: { ...progress, stage: "retrying", retryInSecs: 8 } }).sentence).toContain("Retrying in 8 s");
});

for (const sharingEligible of [false, undefined]) {
  for (const health of ["offline", "syncing"] as const) {
    test(`${health} remains visible when eligibility is ${sharingEligible}`, () => {
      const input = { ...base, sharing: false, sharingEligible, status: { ...status, health } };
      expect(describeNodeStrip({ ...input, connectingSeconds: 0 })).toMatchObject({
        sentence: PLAIN_COPY.nodeStrip.runningConnecting, tone: "busy", action: null,
      });
      expect(describeNodeStrip({ ...input, connectingSeconds: CONNECTING_LONG_SECS })).toMatchObject({
        sentence: PLAIN_COPY.nodeStrip.runningUnreachable, tone: "idle", action: null,
      });
    });
  }
  test(`model download remains visible when eligibility is ${sharingEligible}`, () => {
    expect(describeNodeStrip({
      ...base, sharing: false, sharingEligible,
      modelProgress: { tier: "standard", downloadedBytes: 1024 ** 3, totalBytes: 4_081_004_224, done: false },
    })).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.downloadingModel("1.00 GB of 3.80 GB"), tone: "busy", action: null,
    });
  });
}

test.beforeEach(async ({ page, baseURL }) => {
  await page.route("**/*", (route) => new URL(route.request().url()).origin === baseURL
    ? route.continue() : route.abort());
});

async function seedSharing(page: Page, consent = true) {
  await seedOnboarded(page);
  await page.addInitScript((sharing) => {
    const stored = JSON.parse(localStorage.getItem("arc-desktop-state-v1")!);
    stored.config.computeConsent = sharing;
    stored.config.modelPath = sharing ? "/mock/model.gguf" : null;
    localStorage.setItem("arc-desktop-state-v1", JSON.stringify(stored));
  }, consent);
}

async function shot(page: Page, name: string) {
  if (!process.env.ARC_DESKTOP_SHOTS) return;
  await page.locator("#boot").waitFor({ state: "detached" });
  await page.screenshot({ path: `${process.env.ARC_DESKTOP_SHOTS}/${name}-${page.viewportSize()!.width}.png`, animations: "disabled" });
}

test("Home sharing action uses the existing consent flow; unknown network stays quiet", async ({ page }) => {
  await seedSharing(page, false);
  await openOn(page, "inference");
  await seedMockOverrides(page, { node_status: status, fetch_worker_network: null });
  await page.goto("/?slow=700");
  await expect(page.getByTestId("node-strip-sentence")).toHaveText(PLAIN_COPY.nodeStrip.sharingOff);
  await expect(page.getByTestId("node-strip-share-note")).toContainText("3.80 GB");
  await page.getByRole("button", { name: PLAIN_COPY.actions.shareComputer }).click();
  await expect(page.getByTestId("node-strip-sentence")).toHaveText(PLAIN_COPY.nodeStrip.enablingSharing);
  await expect(page.getByTestId("node-strip-action")).toHaveCount(0);
  await expect.poll(() => page.evaluate(() => JSON.parse(localStorage.getItem("arc-desktop-state-v1")!).config.computeConsent)).toBe(true);
  await expect(page.getByTestId("node-strip-sentence")).not.toContainText("online");
});

for (const consent of [false, true]) {
  test(`Home blocks sharing below 16 GB even with saved consent=${consent}`, async ({ page }) => {
    await seedSharing(page, consent);
    await openOn(page, "inference");
    await seedMockOverrides(page, { node_status: status, recommended_tier: "none", fetch_worker_network: network });
    await page.goto("/");
    await expect(page.getByTestId("node-strip-sentence")).toHaveText(PLAIN_COPY.nodeStrip.sharingIneligible + " " + PLAIN_COPY.nodeStrip.online(1));
    await expect(page.getByTestId("node-strip")).toHaveAttribute("data-tone", "idle");
    await expect(page.getByTestId("node-strip-action")).toHaveCount(0);
    await page.getByTestId("nav-settings").click();
    await expect(page.getByTestId("compute-consent-toggle")).toBeDisabled();
    await expect(page.getByTestId("compute-consent-toggle")).not.toBeChecked();
    expect(await page.evaluate(() => JSON.parse(localStorage.getItem("arc-desktop-state-v1")!).config.computeConsent)).toBe(consent);
  });
}

test("Home cannot offer sharing before eligibility is known", async ({ page }) => {
  await seedSharing(page, false);
  await openOn(page, "inference");
  await seedMockOverrides(page, { node_status: status, recommended_tier: { __reject: "Hardware unavailable" }, fetch_worker_network: network });
  await page.goto("/");
  await expect(page.getByTestId("node-strip-sentence")).toHaveText(PLAIN_COPY.nodeStrip.runningConnected + " " + PLAIN_COPY.nodeStrip.online(1));
  await expect(page.getByTestId("node-strip-action")).toHaveCount(0);
});

const result = {
  input: "Largest planet?", output: "Jupiter is the largest planet in our solar system.",
  outputHash: `0x${"aa".repeat(32)}`, modelHash: `0x${"bb".repeat(32)}`,
  tokensGenerated: 12, inferenceMs: 800, txHash: "", deterministic: true,
  profileBound: true, quorumVerified: true, executionProfile: "test", engine: "test",
  explorerUrl: "", servedLocally: false, routedVia: `community:0x${"99".repeat(32)}`,
};

for (const [name, patch, expected] of [
  ["own worker", {}, PLAIN_COPY.answer.own],
  ["another worker", { routedVia: `community:0x${"11".repeat(32)}` }, PLAIN_COPY.answer.network],
  ["local answer", { routedVia: "local", servedLocally: true }, PLAIN_COPY.answer.own],
  ["seed-local answer", { routedVia: "local", servedLocally: false, coordinator: "http://seed.invalid:9090" }, PLAIN_COPY.answer.arc],
  ["ARC seed pipeline through local endpoint", { routedVia: "sharded_seed_pipeline", servedLocally: true }, PLAIN_COPY.answer.arc],
  ["remote replicas through local endpoint", { routedVia: "sharded_consensus", servedLocally: true }, PLAIN_COPY.answer.network],
  ["missing routing evidence on local endpoint", { routedVia: undefined, servedLocally: true }, PLAIN_COPY.answer.network],
  ["coordinator without routing evidence", { routedVia: undefined, coordinator: "http://localhost:9090" }, PLAIN_COPY.answer.network],
  ["no route", { routedVia: "none", servedLocally: true }, PLAIN_COPY.answer.network],
  ["unknown route", { routedVia: "future_route", servedLocally: true }, PLAIN_COPY.answer.network],
] as const) {
  test(`answer provenance: ${name}; proof opens with one click`, async ({ page }) => {
    await seedSharing(page);
    await openOn(page, "inference");
    await seedMockOverrides(page, { node_status: status, run_inference: { ...result, ...patch }, run_inference_via_coordinator_direct: { ...result, ...patch } });
    await page.goto("/");
    await page.getByTestId("inference-prompt").fill("Largest planet?");
    await page.getByTestId("btn-run-inference").click();
    await expect(page.locator(".answer-by")).toHaveText(expected);
    const served = expected === PLAIN_COPY.answer.own ? "your computer"
      : expected === PLAIN_COPY.answer.arc ? "ARC's computers" : "the ARC network";
    await expect(page.getByRole("list", { name: "What happened to this prompt" }).locator("li").nth(1).locator(".stage-detail")).toContainText(served + " · ");
    if (name === "seed-local answer") await shot(page, "seed-local-answer");
    await expect(page.getByText(PLAIN_COPY.answer.checked, { exact: true })).toBeVisible();
    const proof = page.locator("details.evidence");
    await expect(proof).not.toHaveAttribute("open");
    await expect(page.getByTestId("inference-consensus")).toBeHidden();
    await proof.locator("summary").click();
    await expect(page.getByTestId("inference-consensus")).toBeVisible();
    await expect(page.getByTestId("inference-result")).toHaveAttribute("data-output-hash", result.outputHash);
  });
}

test("a reported check without authenticated agreement stays unconfirmed", async ({ page }) => {
  await seedSharing(page);
  await openOn(page, "inference");
  const reported = {
    ...result, profileBound: false, quorumVerified: true,
    consensus: { k: 2, unanimous: 2, votesTotal: 2, split: 0, majority: 0, divergentReplicaCount: 0 },
  };
  await seedMockOverrides(page, { run_inference: reported, run_inference_via_coordinator_direct: reported });
  await page.goto("/");
  await page.getByTestId("inference-prompt").fill("Largest planet?");
  await page.getByTestId("btn-run-inference").click();
  await expect(page.getByText(PLAIN_COPY.answer.reported, { exact: true })).toBeVisible();
  await expect(page.getByText(PLAIN_COPY.answer.checked, { exact: true })).toHaveCount(0);
});

for (const profileBound of [false, true]) {
  test(`unchecked answers do not claim a second computer agreed (bound=${profileBound})`, async ({ page }) => {
    await seedOnboarded(page);
    await openOn(page, "inference");
    const unchecked = { ...result, profileBound, quorumVerified: false };
    await seedMockOverrides(page, { run_inference_via_coordinator_direct: unchecked, run_inference: unchecked });
    await page.goto("/");
    await page.getByTestId("inference-prompt").fill("Largest planet?");
    await page.getByTestId("btn-run-inference").click();
    await expect(page.getByText(PLAIN_COPY.answer.unchecked, { exact: true })).toBeVisible();
    await expect(page.getByText(PLAIN_COPY.answer.checked, { exact: true })).toHaveCount(0);
  });
}

for (const width of [1280, 960]) {
  test.describe(`first screens at ${width}`, () => {
    test.use({ viewport: { width, height: width === 960 ? 640 : 820 } });
    for (const tier of ["standard", "none"]) {
      test(`Home sharing off keeps its explanation clear (tier=${tier})`, async ({ page }) => {
        await seedSharing(page, false);
        await openOn(page, "inference");
        await seedMockOverrides(page, { node_status: status, recommended_tier: tier, fetch_worker_network: network });
        await page.goto("/");
        await expect(page.getByTestId("node-strip-sentence")).toContainText(tier === "none"
          ? PLAIN_COPY.nodeStrip.sharingIneligible : PLAIN_COPY.nodeStrip.sharingOff);
        if (tier === "standard") await expect(page.getByTestId("node-strip-share-note")).toBeVisible();
        if (width === 960) {
          await expect(page.getByTestId("inference-prompt")).toBeInViewport({ ratio: 1 });
          await expect(page.getByTestId("btn-run-inference")).toBeInViewport({ ratio: 1 });
          expect(await page.getByTestId("btn-run-inference").evaluate((element) => {
            const r = element.getBoundingClientRect();
            const top = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
            return top === element || element.contains(top);
          })).toBe(true);
          await shot(page, tier === "none" ? "home-ineligible-initial" : "home-sharing-off-initial");
        }
        const note = page.locator(".chat-empty-note");
        await note.scrollIntoViewIfNeeded();
        await expect(note).toBeInViewport({ ratio: 1 });
        expect(await note.evaluate((element) => {
          const rect = element.getBoundingClientRect();
          const composer = document.querySelector(".composer")!.getBoundingClientRect();
          const foreground = document.elementFromPoint(rect.left + rect.width / 2, rect.bottom - 1);
          const separated = rect.bottom <= composer.top || rect.top >= composer.bottom;
          return separated && (foreground === element || element.contains(foreground));
        })).toBe(true);
        await shot(page, tier === "none" ? "home-ineligible" : "home-sharing-off");
        await page.getByTestId("inference-prompt").fill("Largest planet?");
        await page.getByTestId("btn-run-inference").click();
        await expect(page.getByTestId("inference-output")).toBeVisible();
      });
    }
    test("Welcome explains ARC", async ({ page }) => {
      await clearState(page);
      await page.goto("/");
      await expect(page.getByText(PLAIN_COPY.home.promise)).toBeVisible();
      await expect(page.getByText(PLAIN_COPY.home.shareBody)).toBeVisible();
      await shot(page, "welcome");
    });
    test("Home, answer, Settings and Dashboard", async ({ page }) => {
      await seedSharing(page);
      await openOn(page, "inference");
      await seedMockOverrides(page, {
        node_status: status, fetch_worker_status: worker, fetch_worker_network: network,
        run_inference: result, run_inference_via_coordinator_direct: result,
      });
      await page.goto("/");
      await expect(page.getByTestId("node-strip-sentence")).toContainText("3 questions");
      await expect(page.getByLabel(PLAIN_COPY.ask.answerLength)).toHaveValue("16");
      await shot(page, "home");
      await page.getByTestId("inference-prompt").fill("Largest planet?");
      await page.getByTestId("btn-run-inference").click();
      await expect(page.locator(".answer-by")).toHaveText(PLAIN_COPY.answer.own);
      await page.locator("details.evidence summary").scrollIntoViewIfNeeded();
      await shot(page, "answer");
      await page.locator("details.evidence summary").click();
      await expect(page.getByTestId("inference-consensus")).toBeVisible();
      await page.getByTestId("nav-settings").click();
      await page.getByTestId("persistence-card").scrollIntoViewIfNeeded();
      await expect(page.getByTestId("persistence-summary")).toHaveText(PLAIN_COPY.settings.startupOn);
      await shot(page, "settings");
      await page.getByTestId("nav-dashboard").click();
      const grid = page.getByTestId("stat-grid");
      await expect(grid).toContainText(PLAIN_COPY.dashboard.questions);
      await expect(grid.locator(".stat-tile").nth(1).locator(".stat-value")).toHaveText("—");
      await expect(page.getByTestId("dashboard-advanced")).not.toHaveAttribute("open");
      await expect(page.getByTestId("start-stage")).toHaveCount(0, { timeout: 10000 });
      await page.getByRole("heading", { name: "Dashboard", exact: true }).scrollIntoViewIfNeeded();
      await shot(page, "dashboard");
    });
    test("sharing notice keeps both choices reachable and preserves their actions", async ({ page }) => {
      await seedOnboarded(page);
      await seedComputePrompt(page);
      await page.goto("/");
      const gotIt = page.getByTestId("compute-prompt-not-now");
      const turnOff = page.getByTestId("compute-prompt-turn-off");
      await expect(gotIt).toHaveClass(/btn-primary/);
      await expect(turnOff).toHaveClass(/btn-secondary/);
      await expect(gotIt).toBeInViewport({ ratio: 1 });
      await expect(turnOff).toBeInViewport({ ratio: 1 });
      await shot(page, "sharing-notice");
      await gotIt.click();
      await expect(page.getByTestId("compute-prompt-dialog")).toHaveCount(0);
      expect(await page.evaluate(() => JSON.parse(localStorage.getItem("arc-desktop-state-v1")!).config.computeConsent)).toBe(true);
    });
  });
}

for (const [login, copy] of [[false, PLAIN_COPY.settings.startupUnregistered], [null, PLAIN_COPY.settings.startupUnknown]] as const) {
  test(`Settings explains login registration ${login}`, async ({ page }) => {
    await seedOnboarded(page);
    await openOn(page, "settings");
    await seedMockOverrides(page, { get_autostart: login });
    await page.goto("/");
    await expect(page.getByTestId("persistence-summary")).toHaveText(copy);
    await expect(page.getByTestId("persistence-card").locator(".kv dt")).toHaveCount(3);
  });
}

test("a sharing choice waiting for its model does not get the observer upgrade banner", async ({ page }) => {
  await seedSharing(page);
  await page.addInitScript(() => {
    const stored = JSON.parse(localStorage.getItem("arc-desktop-state-v1")!);
    stored.config.role = "observer";
    stored.config.modelPath = null;
    localStorage.setItem("arc-desktop-state-v1", JSON.stringify(stored));
  });
  await page.goto("/");
  await expect(page.getByTestId("dashboard")).toBeVisible();
  await expect(page.getByTestId("observer-upgrade-banner")).toHaveCount(0);
});

test("package-managed installs explain how to update without claiming automatic installs", async ({ page }) => {
  await seedOnboarded(page);
  await openOn(page, "settings");
  await seedMockOverrides(page, { update_install_policy: {
    canInstall: false, channel: "package-manager", instructions: "Use your package manager.",
  } });
  await page.goto("/");
  await expect(page.getByTestId("update-policy")).toHaveText(PLAIN_COPY.settings.packageUpdates);
});


test("model waits and slow recovery stay visible with sharing eligibility and network counts", () => {
  for (const sharingEligible of [true, false, undefined]) {
    for (const sharing of [true, false]) {
      const input = { ...base, sharing, sharingEligible };
      expect(describeNodeStrip({ ...input, status: {
        ...base.status!, modelDownloadWait: { reason: "onBattery" },
      } })).toMatchObject({ sentence: PLAIN_COPY.nodeStrip.modelWaitingBattery, action: null });
      expect(describeNodeStrip({ ...input, status: {
        ...base.status!, running: false, pid: null, crashRetryInSecs: 600,
      } })).toMatchObject({ sentence: PLAIN_COPY.nodeStrip.crashRetry(10), action: "start-again" });
    }
  }
});

test("a refused or closing copy never offers sharing even with a healthy worker", () => {
  for (const startBlock of ["ARC is already open on this computer.", "ARC is closing"]) {
    const view = describeNodeStrip({ ...base, sharing: false, startBlock,
      status: { ...base.status!, crashRetryInSecs: 600 },
    });
    expect(view.action).toBe(startBlock === "ARC is closing" ? null : "close-copy");
    expect(view.sentence).not.toContain("computer is online");
  }
});


test("Home closes a refused copy without enabling sharing below the memory floor", async ({ page }) => {
  await seedSharing(page, false);
  await openOn(page, "inference");
  await seedMockOverrides(page, {
    node_status: { ...status, running: false, pid: null, crashRetryInSecs: 600 },
    node_start_block: "ARC is already open on this computer.",
    recommended_tier: "none", fetch_worker_network: network,
  });
  await page.goto("/");
  const close = page.getByTestId("node-strip-action");
  await expect(close).toHaveText(PLAIN_COPY.actions.closeCopy);
  await expect(page.getByRole("button", { name: PLAIN_COPY.actions.shareComputer })).toHaveCount(0);
  await Promise.all([page.waitForEvent("load"), close.click()]);
  expect(await page.evaluate(() => JSON.parse(localStorage.getItem("arc-desktop-state-v1")!).config.computeConsent)).toBe(false);
});
