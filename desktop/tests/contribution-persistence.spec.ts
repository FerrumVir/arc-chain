// The compute slider's real effect, and whether the node persists.
//
// Both exist to answer questions the owner asked directly: "can users set how
// much compute they dedicate" and "will mining auto-persist even when they turn
// it off and back on". The copy is the deliverable here, so the copy is what is
// asserted — including the absence of a cores-to-ARC multiplier claim.

import { expect, test } from "@playwright/test";
import { seedMockOverrides, seedOnboarded, seedOnboardedWithoutConfig } from "./helpers";

async function gotoSettings(page: import("@playwright/test").Page) {
  await page.goto("/");
  await page.getByTestId("nav-settings").click();
  await expect(page.getByTestId("settings-screen")).toBeVisible();
}

test.describe("Compute contribution", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("the slider still sets a core count", async ({ page }) => {
    await gotoSettings(page);
    const slider = page.getByTestId("slider-worker-threads");
    await expect(slider).toBeVisible();
    await expect(page.getByTestId("worker-threads-value")).toHaveText(
      /^\d+ \/ \d+$/,
    );
  });

  test("explains the mechanism without claiming a cores-to-ARC multiplier", async ({
    page,
  }) => {
    await gotoSettings(page);
    const hint = page.getByTestId("compute-contribution");
    await expect(hint).toContainText("serve each hop faster");
    // The honest causal chain: attestations pay, cores do not.
    await expect(hint).toContainText(
      "Earnings follow the attestations you actually serve, not the cores you own",
    );
    await expect(hint).toContainText("no multiplier from cores to ARC");
    // The previous copy promised "more cores means more work served - and more
    // earnings". That claim must not come back.
    await expect(hint).not.toContainText("more cores means more work served");
  });

  test("shows what the node is actually contributing, next to the slider", async ({
    page,
  }) => {
    await gotoSettings(page);
    const actual = page.getByTestId("actual-contribution");
    await expect(actual).toBeVisible();
    await expect(actual).toContainText("Currently contributing");
    // Threads in use vs available, as a ratio - format not magnitude.
    await expect(page.getByTestId("contrib-cores-in-use")).toHaveText(
      /^\d+ of \d+$/,
    );
    // Attributed to the user's own node, not a seed.
    await expect(actual).toContainText("127.0.0.1");
    await expect(actual).toContainText("your own node");
  });

  test("keeps cache hits separate from real pipeline runs", async ({ page }) => {
    await gotoSettings(page);
    const actual = page.getByTestId("actual-contribution");
    await expect(actual).toContainText("Pipeline runs served");
    // Summing these two would overstate the work this node performed.
    await expect(actual).toContainText("Served from cache");
  });

  test("a node that answers nothing degrades to a stated reason", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      fetch_node_contribution: {
        sourceHost: "http://127.0.0.1:9090",
        unavailable:
          "Your node did not answer /node/contribution, /node/threads or /stats, so what it is contributing cannot be read right now.",
        source: "none",
        threadsInUse: null,
        threadsAvailable: 24,
        layersHeld: null,
        layerCount: null,
        totalLayers: null,
        runsServed: null,
        cacheHits: null,
        hopMsMean: null,
        hopSamples: null,
        hopUnavailableReason: null,
      },
    });
    await gotoSettings(page);
    const reason = page.getByTestId("contribution-unavailable");
    await expect(reason).toBeVisible();
    await expect(reason).toContainText("cannot be read right now");
    // No fabricated substitute figures.
    await expect(page.getByTestId("contrib-cores-in-use")).toHaveCount(0);
  });

  test("says so when the figures were composed from fallback endpoints", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      fetch_node_contribution: {
        sourceHost: "http://127.0.0.1:9090",
        unavailable: null,
        source: "composed",
        threadsInUse: 4,
        threadsAvailable: 24,
        layersHeld: null,
        layerCount: null,
        totalLayers: null,
        runsServed: 15,
        cacheHits: 3,
        hopMsMean: null,
        hopSamples: null,
        hopUnavailableReason:
          "no sharded hop has been served on this node yet, so there is nothing to average",
      },
    });
    await gotoSettings(page);
    const actual = page.getByTestId("actual-contribution");
    await expect(actual).toContainText("does not serve");
    await expect(actual).toContainText("/node/contribution");
    // Anything unmeasured is omitted, not zeroed.
    await expect(
      page.getByTestId("contrib-measured-time-per-hop"),
    ).toHaveCount(0);
  });
});

test.describe("Persistence - auto-start on", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("states plainly that the node starts with the computer", async ({
    page,
  }) => {
    await gotoSettings(page);
    const summary = page.getByTestId("persistence-summary");
    await expect(summary).toBeVisible();
    await expect(summary).toContainText(
      "starts with this computer and keeps contributing",
    );
    // The owner's actual question: off and on again must not reset it.
    await expect(summary).toContainText(
      "turning it off and on again does not reset anything",
    );
    // And what governs it.
    await expect(summary).toContainText("Start node on app launch");
  });

  test("reports the config flag and the OS login item separately", async ({
    page,
  }) => {
    await gotoSettings(page);
    await expect(page.getByTestId("persistence-autostart")).toHaveText("on");
    // Registered independently of the flag, so a disagreement stays visible.
    await expect(page.getByTestId("persistence-login-item")).toContainText(
      /login item/,
    );
  });

  test("is truthful that a model-less node resumes as an earning-nothing observer", async ({
    page,
  }) => {
    // seedOnboarded writes modelPath: null.
    await gotoSettings(page);
    const role = page.getByTestId("persistence-role");
    await expect(role).toContainText("observer");
    await expect(role).toContainText("never sent inference work");
    await expect(role).toContainText("an observer earns nothing");
  });

  test("says worker, and that it can earn, once a model is configured", async ({
    page,
  }) => {
    await page.addInitScript(() => {
      const raw = localStorage.getItem("arc-desktop-state-v1");
      if (!raw) return;
      const s = JSON.parse(raw);
      s.config.modelPath = "/mock/.arc/models/standard.gguf";
      localStorage.setItem("arc-desktop-state-v1", JSON.stringify(s));
    });
    await gotoSettings(page);
    const role = page.getByTestId("persistence-role");
    await expect(role).toContainText("worker");
    await expect(role).toContainText("can earn attestations");
  });

  test("the Dashboard also answers whether it starts with the OS", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.getByTestId("dashboard-persistence")).toHaveText(
      /^(yes|no|set, but no login item)$/,
    );
    await expect(page.getByTestId("dashboard-persistence-note")).toContainText(
      "starts with this computer",
    );
  });
});

test.describe("Persistence - auto-start off", () => {
  test("says nothing resumes, and what to do about it", async ({ page }) => {
    await seedOnboarded(page);
    await page.addInitScript(() => {
      const raw = localStorage.getItem("arc-desktop-state-v1");
      if (!raw) return;
      const s = JSON.parse(raw);
      s.config.autoStart = false;
      localStorage.setItem("arc-desktop-state-v1", JSON.stringify(s));
    });
    await gotoSettings(page);
    const summary = page.getByTestId("persistence-summary");
    await expect(summary).toContainText("does not start on its own");
    await expect(summary).toContainText("earns nothing until you do");
    await expect(page.getByTestId("persistence-autostart")).toHaveText("off");
  });
});

test.describe("Persistence - no stored config", () => {
  test("falls back to the real default rather than rendering blank", async ({
    page,
  }) => {
    // An install whose store.json predates the config block.
    await seedOnboardedWithoutConfig(page);
    await gotoSettings(page);
    await expect(page.getByTestId("persistence-card")).toBeVisible();
    // DEFAULT_NODE_CONFIG.autoStart is true.
    await expect(page.getByTestId("persistence-autostart")).toHaveText("on");
  });
});
