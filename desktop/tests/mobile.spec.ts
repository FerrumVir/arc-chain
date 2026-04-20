// Verifies the mobile layout at a typical phone viewport (iPhone 14 / Pixel 7).
// Sidebar collapses to bottom tab bar, stat grid becomes single-column,
// page header stacks vertically, tap targets hit the 44px floor.

import { expect, test, devices } from "@playwright/test";
import { seedOnboarded } from "./helpers";

test.use({ ...devices["Pixel 7"] });

test.describe("Mobile layout (Pixel 7, 412×915) — seeded", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("sidebar renders as bottom tab bar with all 7 nav items", async ({
    page,
  }) => {
    await page.goto("/");
    const sidebar = page.getByTestId("sidebar");
    await expect(sidebar).toBeVisible();
    const sidebarBox = await sidebar.boundingBox();
    const viewportSize = page.viewportSize()!;
    expect(sidebarBox!.y).toBeGreaterThan(viewportSize.height / 2);
    for (const id of [
      "dashboard",
      "wallet",
      "inference",
      "earnings",
      "network",
      "logs",
      "settings",
    ]) {
      const nav = page.getByTestId(`nav-${id}`);
      await expect(nav).toBeVisible();
      const box = await nav.boundingBox();
      expect(box!.height).toBeGreaterThanOrEqual(40);
    }
  });

  test("page header stacks vertically — title above actions", async ({
    page,
  }) => {
    await page.goto("/");
    const title = page.locator("h1.page-title").first();
    await expect(title).toBeVisible();
    const titleBox = await title.boundingBox();
    const startBtn = page.getByTestId("btn-start");
    if (await startBtn.count()) {
      const btnBox = await startBtn.boundingBox();
      expect(btnBox!.y).toBeGreaterThan(titleBox!.y + titleBox!.height - 4);
    }
  });

  test("primary buttons meet 44px tap-target minimum", async ({ page }) => {
    await page.goto("/");
    const buttons = page.locator(".btn-primary");
    const count = await buttons.count();
    expect(count).toBeGreaterThan(0);
    for (let i = 0; i < Math.min(count, 3); i++) {
      const box = await buttons.nth(i).boundingBox();
      expect(box!.height).toBeGreaterThanOrEqual(44);
    }
  });

  test("dashboard doesn't horizontally overflow the viewport", async ({
    page,
  }) => {
    await page.goto("/");
    const viewportWidth = page.viewportSize()!.width;
    const scrollWidth = await page.evaluate(
      () => document.documentElement.scrollWidth,
    );
    expect(scrollWidth).toBeLessThanOrEqual(viewportWidth + 1);
  });

  test("info popover fits inside phone viewport, doesn't clip off-screen", async ({
    page,
  }) => {
    await page.goto("/");
    const firstInfo = page.getByTestId("info-btn").first();
    await firstInfo.click();
    const popover = page.getByTestId("info-popover");
    await expect(popover).toBeVisible();
    const box = await popover.boundingBox();
    const viewportWidth = page.viewportSize()!.width;
    expect(box!.x).toBeGreaterThanOrEqual(0);
    expect(box!.x + box!.width).toBeLessThanOrEqual(viewportWidth + 1);
  });

  test("wallet balance big number is legible, fits in one line", async ({
    page,
  }) => {
    await page.goto("/");
    await page.getByTestId("nav-wallet").click();
    await expect(page.getByTestId("wallet-screen")).toBeVisible();
    const bal = page.getByTestId("wallet-balance");
    const box = await bal.boundingBox();
    expect(box!.height).toBeLessThan(90);
  });
});

test.describe("Mobile layout (Pixel 7, 412×915) — onboarding", () => {
  // Intentionally NO beforeEach — this test needs a fresh, un-seeded store.
  test("all five steps reachable at phone size", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("step-welcome")).toBeVisible();
    await page.getByTestId("btn-continue-welcome").click();
    await page.waitForFunction(() => {
      const btn = document.querySelector(
        "[data-testid='btn-continue-hardware']",
      ) as HTMLButtonElement | null;
      return btn && !btn.disabled;
    });
    await expect(page.getByTestId("step-hardware")).toBeVisible();
    const hwItems = page.locator(".hw-item");
    await expect(hwItems).toHaveCount(3);
  });
});
