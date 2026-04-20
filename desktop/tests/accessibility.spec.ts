import { expect, test } from "@playwright/test";
import { seedOnboarded } from "./helpers";

test.describe("Accessibility basics", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("dashboard has a single h1 with 'Dashboard'", async ({ page }) => {
    await page.goto("/");
    const h1s = page.locator("h1");
    await expect(h1s).toHaveCount(1);
    await expect(h1s.first()).toHaveText(/dashboard/i);
  });

  test("nav buttons are reachable via keyboard (tab focus)", async ({
    page,
  }) => {
    await page.goto("/");
    // Focus the first nav item
    await page.getByTestId("nav-dashboard").focus();
    await expect(page.getByTestId("nav-dashboard")).toBeFocused();
    // Tab forward and confirm another button gets focus
    await page.keyboard.press("Tab");
    const focused = await page.evaluate(
      () => document.activeElement?.getAttribute("data-testid") ?? "",
    );
    expect(focused).toMatch(/nav-|btn-|copy/);
  });

  test("preview-mode chip is present in browser (non-Tauri)", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.getByTestId("preview-mode")).toBeVisible();
  });
});
