// End-to-end tests against a REAL running arc-node on 127.0.0.1:9090.
// Requires: the community installer's launchd daemon OR a manual
//   ~/.arc/bin/arc-node --rpc 0.0.0.0:9090 --seeds-file ~/.arc/seeds.txt --genesis ~/.arc/genesis.toml ...
//
// Run:  npx playwright test live.spec.ts
//
// Each test sets window.__ARC_LIVE__ = 9090 before page load so the app bypasses
// the mock layer and hits the node's real HTTP endpoints.

import { expect, test } from "@playwright/test";
import { clearState, seedOnboarded } from "./helpers";

const LIVE_PORT = 9090;

async function injectLive(page: import("@playwright/test").Page) {
  await page.addInitScript((port) => {
    (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = port;
  }, LIVE_PORT);
}

test.describe("Live node (port 9090) — real data", () => {
  test.beforeEach(async () => {
    // Only run if the node is actually reachable
    try {
      const r = await fetch(`http://127.0.0.1:${LIVE_PORT}/health`);
      if (!r.ok) test.skip();
    } catch {
      test.skip();
    }
  });

  test("first-launch: welcome → onboarding → dashboard with real data", async ({
    page,
  }) => {
    await clearState(page);
    await injectLive(page);

    await page.goto("/");
    await expect(page.getByTestId("step-welcome")).toBeVisible();
    await page.screenshot({ path: "screenshots/live-01-welcome.png" });

    await page.getByTestId("btn-continue-welcome").click();
    await page.waitForFunction(() => {
      const btn = document.querySelector(
        "[data-testid='btn-continue-hardware']",
      ) as HTMLButtonElement | null;
      return btn && !btn.disabled;
    });
    await page.screenshot({ path: "screenshots/live-02-hardware.png" });
    await page.getByTestId("btn-continue-hardware").click();

    await expect(page.getByTestId("step-role")).toBeVisible();
    await page.screenshot({ path: "screenshots/live-03-role.png" });
    await page.getByTestId("btn-continue-role").click();

    await expect(page.getByTestId("step-identity")).toBeVisible();
    await page.getByTestId("btn-reveal-seed").click();
    await page.screenshot({ path: "screenshots/live-04-identity.png" });
    await page.getByTestId("btn-continue-identity").click();

    await expect(page.getByTestId("step-launch")).toBeVisible();
    await page.screenshot({ path: "screenshots/live-05-launch.png" });
    await page.getByTestId("btn-launch").click();

    // Land on dashboard — should show real data
    await expect(page.getByTestId("dashboard")).toBeVisible({ timeout: 10_000 });
    await page.waitForTimeout(2_000); // let queries populate
    await page.screenshot({
      path: "screenshots/live-06-dashboard-running.png",
      fullPage: false,
    });
  });

  test("dashboard shows real-node peers + committed blocks + attestations", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await injectLive(page);
    await page.goto("/");
    await page.waitForTimeout(1_500);

    // Health endpoint says peers = 4, dag_committed > 3500 on the real node
    // (values drift; we assert non-zero)
    const committedText = await page
      .locator('[data-testid="dashboard"] dd')
      .nth(2) // Block height row (Health, Version, Block height, ...)
      .textContent();
    expect(committedText).toBeTruthy();
    expect(committedText).not.toBe("0");

    const statTile = page.getByTestId("stat-peers");
    await expect(statTile).toBeVisible();

    // Attestation feed should have at least one real inference
    const feed = page.getByTestId("attestation-feed");
    await expect(feed).toBeVisible();
    await expect(feed.locator(".feed-item")).toHaveCount(6, { timeout: 6_000 });

    // Real tx hashes look like 0x606664ec... from the live node
    const firstHash = feed
      .locator(".feed-item .feed-item-meta span")
      .nth(2)
      .first();
    const txText = await firstHash.textContent();
    expect(txText).toMatch(/^0x[0-9a-f]{8}…$/);

    await page.screenshot({ path: "screenshots/live-dashboard-full.png" });
  });

  test("earnings screen reads real attestation count", async ({ page }) => {
    await seedOnboarded(page);
    await injectLive(page);
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();
    await page.waitForTimeout(2_000);

    // Lifetime total should be (attestations * 2.5 ARC) — non-zero
    const lifetime = page.locator(".big-number.gradient").first();
    await expect(lifetime).toBeVisible();
    const lifetimeText = await lifetime.textContent();
    const digits = (lifetimeText ?? "").replace(/[^\d]/g, "");
    expect(Number(digits)).toBeGreaterThan(0);

    await page.screenshot({
      path: "screenshots/live-earnings.png",
      fullPage: true,
    });
  });

  test("network screen reads real validator count + latest block", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await injectLive(page);
    await page.goto("/");
    await page.getByTestId("nav-network").click();
    await page.waitForTimeout(1_500);

    // Latest block ≈ dag_committed — > 1000 on this node
    const latestBlock = page
      .locator('[data-testid="network-screen"] .stat-value')
      .last();
    const text = await latestBlock.textContent();
    const digits = (text ?? "").replace(/[^\d]/g, "");
    expect(Number(digits)).toBeGreaterThan(100);

    await page.screenshot({ path: "screenshots/live-network.png" });
  });
});
