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
    await expect(hint).toContainText("Cores do not multiply ARC");
    await expect(hint).toContainText("successful mined");
    await expect(hint).toContainText("0x25");
    await expect(hint).not.toContainText("more cores means more work served");
  });

  test("shows what the node is actually contributing, next to the slider", async ({
    page,
  }) => {
    await gotoSettings(page);
    const actual = page.getByTestId("actual-contribution");
    await expect(actual).toBeVisible();
    await expect(actual).toContainText("Currently contributing");
    await expect(page.getByTestId("contrib-cores-in-use")).toHaveText(
      /^\d+ of \d+$/,
    );
    await expect(actual).toContainText("127.0.0.1");
    await expect(actual).toContainText("your own node");
  });

  test("keeps cache hits separate from real pipeline runs", async ({ page }) => {
    await gotoSettings(page);
    const actual = page.getByTestId("actual-contribution");
    await expect(actual).toContainText("Pipeline runs served");
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
    await expect(
      page.getByTestId("contrib-measured-time-per-hop"),
    ).toHaveCount(0);
  });
});

test.describe("Persistence - auto-start on", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("separates app-launch configuration from verified OS registration", async ({
    page,
  }) => {
    await gotoSettings(page);
    const summary = page.getByTestId("persistence-summary");
    await expect(summary).toBeVisible();
    await expect(summary).toHaveText("ARC opens when you log in and starts your node.");
  });

  test("reports the config flag and the OS login item separately", async ({
    page,
  }) => {
    await gotoSettings(page);
    await expect(page.getByTestId("persistence-autostart")).toHaveText("on");
    await expect(page.getByTestId("persistence-login-item")).toContainText(
      /login item/,
    );
  });

  test("is truthful that a model-less node resumes without local inference", async ({
    page,
  }) => {
    await gotoSettings(page);
    const role = page.getByTestId("persistence-role");
    await expect(role).toHaveText("Your node needs a model before it can answer questions.");
  });

  test("says worker candidate without promising work or payment", async ({
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
    await expect(role).toHaveText("Your node can answer questions after its model is ready.");
  });

  test("the Dashboard also answers whether it starts with the OS", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.getByTestId("dashboard-persistence")).toHaveText(
      /^(yes|no|set, but no login item|not verified)$/,
    );
    await expect(page.getByTestId("dashboard-persistence-note")).toContainText(
      "OS login item is registered",
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
    await expect(summary).toHaveText("ARC does not start your node by itself. You can turn that on below.");
    await expect(page.getByTestId("persistence-autostart")).toHaveText("off");
  });
});

test.describe("Persistence - no stored config", () => {
  test("falls back to the real default rather than rendering blank", async ({
    page,
  }) => {
    await seedOnboardedWithoutConfig(page);
    await gotoSettings(page);
    await expect(page.getByTestId("persistence-card")).toBeVisible();
    await expect(page.getByTestId("persistence-autostart")).toHaveText("on");
  });
});
