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

  test("shows earnings card with gradient number", async ({ page }) => {
    await page.goto("/");
    const earnings = page.getByTestId("earnings-total");
    await expect(earnings).toBeVisible();
    // Wait for the ticker to animate in — at least a non-zero value shows up.
    await expect(earnings).not.toHaveText(/^0\.00/);
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

  test("attestation feed renders at least 3 mock attestations", async ({
    page,
  }) => {
    await page.goto("/");
    const feed = page.getByTestId("attestation-feed");
    await expect(feed).toBeVisible();
    await expect(feed.locator(".feed-item")).toHaveCount(3, { timeout: 8000 });
  });

  test("copy address button shows confirmation", async ({ page, context }) => {
    await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.goto("/");
    const copyBtn = page.getByTestId("btn-copy-address");
    await copyBtn.click();
    await expect(copyBtn).toContainText("Copied");
  });
});
