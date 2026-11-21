import { expect, test } from "@playwright/test";
import { seedOnboarded } from "./helpers";

test.describe("InfoPopover (in-context explainers)", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
    await page.goto("/");
  });

  test("attestation explainer opens, shows content, closes on Esc", async ({
    page,
  }) => {
    const infoButtons = page.getByTestId("info-btn");
    await expect(infoButtons.first()).toBeVisible();

    await infoButtons.first().click();
    const popover = page.getByTestId("info-popover");
    await expect(popover).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(popover).toHaveCount(0);
  });

  test("claim popover separates recomputation evidence from payment", async ({
    page,
  }) => {
    const attestHeading = page
      .locator(".card-title", { hasText: /Recent inference activity/i })
      .first();
    await attestHeading.getByTestId("info-btn").click();
    const popover = page.getByTestId("info-popover");
    await expect(popover).toBeVisible();
    await expect(popover).toContainText(/exact model artifact/i);
    await expect(popover).toContainText(/2-of-3/i);
    await expect(popover).toContainText(/0x16/);
    await expect(popover).toContainText(/pay nothing/i);
    await expect(popover).toContainText(/0x25/);
    await expect(popover).toContainText(/five/i);
  });

  test("click outside closes the popover", async ({ page }) => {
    const btn = page.getByTestId("info-btn").first();
    await btn.click();
    await expect(page.getByTestId("info-popover")).toBeVisible();
    await page.getByTestId("dashboard").click({ position: { x: 10, y: 10 } });
    await expect(page.getByTestId("info-popover")).toHaveCount(0);
  });

  test("every dashboard stat tile has an info button", async ({ page }) => {
    const grid = page.getByTestId("stat-grid");
    const infoBtns = grid.getByTestId("info-btn");
    await expect(infoBtns).toHaveCount(4);
  });
});
