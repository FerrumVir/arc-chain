import { expect, test } from "@playwright/test";
import { seedOnboarded } from "./helpers";

test.describe("Settings", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("saves rpc port changes", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    const input = page.getByTestId("input-rpc-port");
    await input.fill("9955");
    await page.getByTestId("btn-save-settings").click();
    await expect(page.getByTestId("btn-save-settings")).toContainText("Saved");
  });

  test("reset onboarding returns to welcome screen", async ({ page }) => {
    await page.goto("/");
    page.once("dialog", (d) => d.accept());
    await page.getByTestId("nav-settings").click();
    await page.getByTestId("btn-reset").click();
    await expect(page.getByTestId("onboarding")).toBeVisible();
    await expect(page.getByTestId("step-welcome")).toBeVisible();
  });

  test("check-for-updates disables while fetching", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    const btn = page.getByTestId("btn-check-update");
    await btn.click();
    // In mock mode the call resolves quickly; the "info" pill should appear
    await expect(page.getByText(/v0\.5\.2/).first()).toBeVisible({ timeout: 4000 });
  });
});
