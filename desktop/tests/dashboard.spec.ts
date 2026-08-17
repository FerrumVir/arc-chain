import { expect, test } from "@playwright/test";
import { seedOnboarded } from "./helpers";

test.describe("Dashboard", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("renders the app shell with titlebar, sidebar, and main", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.getByTestId("app-shell")).toBeVisible();
    await expect(page.getByTestId("titlebar")).toBeVisible();
    await expect(page.getByTestId("sidebar")).toBeVisible();
    await expect(page.getByTestId("main")).toBeVisible();
    await expect(page.getByTestId("dashboard")).toBeVisible();
  });

  test("shows earnings card formatted as an ARC amount", async ({ page }) => {
    await page.goto("/");
    const earnings = page.getByTestId("earnings-total");
    await expect(earnings).toBeVisible();
    // Assert the FORMAT, not the magnitude.
    //
    // This used to assert `not.toHaveText(/^0\.00/)`, i.e. that earnings are
    // non-zero. That is a property of the mock fixture, not of the app: zero
    // is the correct and expected reading for a fresh identity on the real
    // network, so the assertion would fail against live data while telling us
    // nothing about whether the card renders.
    await expect(earnings).toHaveText(/[\d,]+\.\d{2}\s*ARC total/);
  });

  test("attestations not credited to this user are not shown as earnings", async ({
    page,
  }) => {
    await page.goto("/");
    const feed = page.getByTestId("attestation-feed");
    await expect(feed).toBeVisible();
    // The mock includes one attestation from another validator. It must
    // render as "network", never as a "+2.50" credit - showing other
    // validators' work as the user's income was the original bug.
    await expect(feed.getByText("network", { exact: true })).toHaveCount(1);
  });

  test("unknown telemetry renders as 'recent', not fabricated zeros", async ({
    page,
  }) => {
    await page.goto("/");
    const feed = page.getByTestId("attestation-feed");
    // The flat-shaped attestation carries no tokens, latency or timestamp.
    await expect(feed.getByText("recent", { exact: true })).toHaveCount(1);
    await expect(feed.getByText("0 tokens")).toHaveCount(0);
    await expect(feed.getByText("0ms")).toHaveCount(0);
  });

  test("last payout renders a block height as a block, not a date", async ({
    page,
  }) => {
    await page.goto("/");
    const payout = page.getByTestId("last-payout");
    await expect(payout).toBeVisible();
    // Regression guard for the "20770d ago" bug: a block height must never
    // reach the relative-time formatter.
    await expect(payout).not.toHaveText(/\d{3,}d ago/);
  });

  test("start / stop controls toggle node state", async ({ page }) => {
    await page.goto("/");
    // Initial: mock state shows stopped (mockStartedAt null)
    const startBtn = page.getByTestId("btn-start");
    await expect(startBtn).toBeVisible();
    await startBtn.click();
    // After start, stop button appears
    await expect(page.getByTestId("btn-stop")).toBeVisible({ timeout: 4000 });
    // Sidebar status chip should flip to "Running"
    await expect(page.getByTestId("sidebar-status")).toContainText("Running");
    await page.getByTestId("btn-stop").click();
    await expect(page.getByTestId("btn-start")).toBeVisible({ timeout: 4000 });
  });

  test("stats grid has four cards", async ({ page }) => {
    await page.goto("/");
    const grid = page.getByTestId("stat-grid");
    await expect(grid).toBeVisible();
    const tiles = grid.locator(".stat-tile");
    await expect(tiles).toHaveCount(4);
  });

  test("attestation feed renders the mock attestations", async ({ page }) => {
    await page.goto("/");
    const feed = page.getByTestId("attestation-feed");
    await expect(feed).toBeVisible();
    await expect(feed.locator(".feed-item")).toHaveCount(3, { timeout: 8000 });
  });

  test("shows the node's compute width", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("btn-start").click();
    // "add two cores" is only demonstrable if the current width is visible.
    await expect(page.getByTestId("compute-width")).toHaveText(
      /\d+|all/,
      { timeout: 8000 },
    );
  });

  test("copy address button shows confirmation", async ({ page, context }) => {
    await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.goto("/");
    const copyBtn = page.getByTestId("btn-copy-address");
    await copyBtn.click();
    await expect(copyBtn).toContainText("Copied");
  });
});
