import { expect, test } from "@playwright/test";
import { clearState } from "./helpers";

test.describe("Onboarding wizard", () => {
  test.beforeEach(async ({ page }) => {
    await clearState(page);
  });

  test("walks through all three steps and lands on dashboard", async ({
    page,
  }) => {
    await page.goto("/");

    // Welcome
    await expect(page.getByTestId("onboarding")).toBeVisible();
    await expect(page.getByTestId("step-welcome")).toBeVisible();
    await expect(
      page.getByRole("heading", { name: /welcome to arc/i }),
    ).toBeVisible();
    await page.getByTestId("btn-continue-welcome").click();

    // Identity (no role / hardware step anymore - one-click join)
    await expect(page.getByTestId("step-identity")).toBeVisible();
    await expect(page.getByTestId("identity-address")).toContainText("arc1q");
    await expect(page.getByTestId("btn-continue-identity")).toBeDisabled();
    await page.getByTestId("btn-reveal-seed").click();
    await expect(page.getByTestId("btn-continue-identity")).toBeEnabled();
    await page.getByTestId("btn-continue-identity").click();

    // Launch
    await expect(page.getByTestId("step-launch")).toBeVisible();
    await expect(page.getByRole("button", { name: /join the network/i })).toBeVisible();
    await page.getByTestId("btn-launch").click();

    // Lands on dashboard (mock mode resolves startNode + faucetClaim fast)
    await expect(page.getByTestId("dashboard")).toBeVisible({ timeout: 15_000 });
    await expect(page.getByRole("heading", { name: /dashboard/i })).toBeVisible();
  });

  test("progress dots update per step (3 dots, not 5)", async ({ page }) => {
    await page.goto("/");
    // 3 dots total - dot 3 should NOT exist.
    await expect(page.getByTestId("step-dot-0")).toHaveClass(/active/);
    await expect(page.getByTestId("step-dot-2")).toBeVisible();
    await expect(page.getByTestId("step-dot-3")).toHaveCount(0);
    await page.getByTestId("btn-continue-welcome").click();
    await expect(page.getByTestId("step-dot-0")).toHaveClass(/done/);
    await expect(page.getByTestId("step-dot-1")).toHaveClass(/active/);
  });

  test("back button returns to prior step", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("btn-continue-welcome").click();
    await expect(page.getByTestId("step-identity")).toBeVisible();
    await page.getByRole("button", { name: "Back" }).click();
    await expect(page.getByTestId("step-welcome")).toBeVisible();
  });

  test("seed phrase is blurred until revealed", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("btn-continue-welcome").click();
    await expect(page.getByTestId("btn-reveal-seed")).toBeVisible();
    await page.getByTestId("btn-reveal-seed").click();
    await expect(page.getByTestId("btn-reveal-seed")).toHaveCount(0);
  });

  test("no role or hardware step exists anymore", async ({ page }) => {
    await page.goto("/");
    // Even after clicking through, step-hardware + step-role must never appear.
    await page.getByTestId("btn-continue-welcome").click();
    await expect(page.getByTestId("step-hardware")).toHaveCount(0);
    await expect(page.getByTestId("step-role")).toHaveCount(0);
  });
});
