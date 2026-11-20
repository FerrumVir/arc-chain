import { expect, test } from "@playwright/test";
import { clearState, seedOnboarded } from "./helpers";

const rawPort = process.env.ARC_LIVE_PORT;
const LIVE_PORT = rawPort && /^\d+$/.test(rawPort) ? Number(rawPort) : null;
const LIVE_WORKER = (process.env.ARC_LIVE_WORKER ?? "").replace(/^0x/i, "").toLowerCase();
const LIVE_REWARD_TX = (process.env.ARC_LIVE_REWARD_TX ?? "").replace(/^0x/i, "").toLowerCase();
const LIVE_REQUIRED = process.env.ARC_LIVE_REQUIRED === "1";
const CANARY_WORKER = `0x${LIVE_WORKER}`;
const CANARY_TX = `0x${LIVE_REWARD_TX}`;
const CANONICAL_HASH = /^0x[0-9a-f]{64}$/;

test.setTimeout(240_000);

async function injectLive(page: import("@playwright/test").Page) {
  if (LIVE_PORT === null) throw new Error("ARC_LIVE_PORT is required for a live test");
  await page.addInitScript((port) => {
    (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = port;
  }, LIVE_PORT);
}

test.describe("Authenticated production validator forward - real data", () => {
  test.beforeEach(async () => {
    test.skip(LIVE_PORT === null, "set ARC_LIVE_PORT or use npm run test:live");
    try {
      const r = await fetch(`http://127.0.0.1:${LIVE_PORT}/health`);
      if (!r.ok) throw new Error(`health returned HTTP ${r.status}`);
    } catch (error) {
      if (LIVE_REQUIRED) {
        throw new Error(`required live node on 127.0.0.1:${LIVE_PORT} is unavailable: ${String(error)}`);
      }
      test.skip(true, `optional live node is unavailable: ${String(error)}`);
    }
  });

  test("first-launch: welcome → onboarding → dashboard with real data", async ({
    page,
  }, testInfo) => {
    await clearState(page);
    await injectLive(page);

    await page.goto("/");
    await expect(page.getByTestId("step-welcome")).toBeVisible();
    await page.screenshot({ path: testInfo.outputPath("live-01-welcome.png") });

    await page.getByTestId("btn-continue-welcome").click();
    await expect(page.getByTestId("step-identity")).toBeVisible();
    await page.screenshot({ path: testInfo.outputPath("live-02-identity.png") });
    await page.getByTestId("btn-reveal-seed").click();
    await page.getByTestId("btn-continue-identity").click();

    await expect(page.getByTestId("step-model")).toBeVisible();
    await page.getByTestId("tier-skip").click();
    await page.getByTestId("btn-continue-model").click();

    await expect(page.getByTestId("step-launch")).toBeVisible();
    await page.screenshot({ path: testInfo.outputPath("live-03-launch.png") });
    await page.getByTestId("btn-launch").click();

    await expect(page.getByTestId("inference-screen")).toBeVisible({ timeout: 10_000 });
    await page.getByTestId("nav-dashboard").click();
    await expect(page.getByTestId("dashboard")).toBeVisible({ timeout: 10_000 });
    await expect.poll(async () => {
      const value = await page.getByTestId("node-block-height").textContent();
      return Number((value ?? "").replace(/[^\d]/g, ""));
    }, { timeout: 60_000 }).toBeGreaterThan(0);
    await expect.poll(async () => {
      const value = await page
        .getByTestId("stat-peers")
        .locator(".stat-value")
        .textContent();
      return Number((value ?? "").replace(/[^\d]/g, ""));
    }, { timeout: 60_000 }).toBeGreaterThan(0);
    await page.screenshot({
      path: testInfo.outputPath("live-06-dashboard-running.png"),
      fullPage: false,
    });
  });

  test("dashboard shows real-node peers + committed blocks + attestations", async ({
    page,
  }, testInfo) => {
    await seedOnboarded(page);
    await injectLive(page);
    await page.goto("/");

    const committed = page.getByTestId("node-block-height");
    await expect.poll(async () => {
      const digits = (await committed.textContent() ?? "").replace(/[^\d]/g, "");
      return Number(digits);
    }, { timeout: 30_000 }).toBeGreaterThan(0);

    const statTile = page.getByTestId("stat-peers");
    await expect.poll(async () => {
      const digits = (await statTile.locator(".stat-value").textContent() ?? "")
        .replace(/[^\d]/g, "");
      return Number(digits);
    }, { timeout: 60_000 }).toBeGreaterThan(0);

    const feed = page.getByTestId("attestation-feed");
    await expect(feed).toBeVisible();
    await expect.poll(() => feed.locator("[data-tx-hash]").count(), {
      timeout: 30_000,
    }).toBeGreaterThan(0);
    const recentTx = await feed.locator("[data-tx-hash]").first().getAttribute("data-tx-hash");
    expect(recentTx).toMatch(CANONICAL_HASH);

    await page.screenshot({ path: testInfo.outputPath("live-dashboard-full.png") });
  });

  test("sealed 0x25 canary is visible in earnings and host-scoped explorer", async ({ page }, testInfo) => {
    await seedOnboarded(page, LIVE_WORKER);
    await injectLive(page);
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();

    await expect(page.getByTestId("earnings-unavailable")).toHaveCount(0);
    await expect(page.getByTestId("earnings-empty")).toHaveCount(0);
    const canary = page
      .getByTestId("confirmed-reward-receipts")
      .locator(`[data-tx-hash="${CANARY_TX}"]`);
    await expect(canary).toHaveCount(1, { timeout: 30_000 });
    await expect(canary).toHaveAttribute("data-worker", CANARY_WORKER);
    await expect(canary).toHaveAttribute("data-job-id", CANONICAL_HASH);
    await expect(canary).toHaveAttribute("data-block-height", /^\d+$/);
    await expect(canary).toHaveAttribute("data-block-hash", CANONICAL_HASH);
    await expect(canary).toHaveAttribute("data-reward-base", "2500000000");
    await expect(canary).toHaveAttribute("data-reward-arc", "2.5");
    await expect(canary).toHaveAttribute(
      "data-receipt-url",
      `/community/reward_receipt/${CANARY_TX}`,
    );
    await expect(canary).toContainText("+2.50 ARC · COMPUTED + PAID");
    await canary.getByRole("button", {
      name: "Look up confirmed reward on the pinned chain host",
    }).click();
    await expect(page.getByTestId("tx-lookup-result")).toBeVisible({ timeout: 10_000 });
    await expect(page.getByTestId("tx-lookup-result")).toHaveAttribute(
      "data-tx-hash",
      CANARY_TX,
    );
    await expect(page.getByTestId("tx-lookup-input")).toHaveValue(CANARY_TX);
    await expect(page.getByTestId("tx-status-mined")).toBeVisible();

    await page.screenshot({
      path: testInfo.outputPath("live-earnings.png"),
      fullPage: true,
    });
  });

  test("network screen reads real validator count + latest block", async ({
    page,
  }, testInfo) => {
    await seedOnboarded(page);
    await injectLive(page);
    await page.goto("/");
    await page.getByTestId("nav-network").click();

    await expect(page.getByTestId("net-stat-validators")).toHaveText("6 / 6", {
      timeout: 30_000,
    });
    const latestBlock = page.getByTestId("net-stat-block-height");
    await expect.poll(async () => {
      const digits = (await latestBlock.textContent() ?? "").replace(/[^\d]/g, "");
      return Number(digits);
    }, { timeout: 30_000 }).toBeGreaterThan(100);

    await page.screenshot({ path: testInfo.outputPath("live-network.png") });
  });
});
