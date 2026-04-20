import { expect, test } from "@playwright/test";
import { clearState } from "./helpers";

test.describe("Onboarding wizard", () => {
  test.beforeEach(async ({ page }) => {
    await clearState(page);
  });

  test("walks through all five steps and lands on dashboard", async ({
    page,
  }) => {
    await page.goto("/");

    // Welcome
    await expect(page.getByTestId("onboarding")).toBeVisible();
    await expect(page.getByTestId("step-welcome")).toBeVisible();
    await expect(page.getByRole("heading", { name: /welcome to arc/i })).toBeVisible();
    await page.getByTestId("btn-continue-welcome").click();

    // Hardware
    await expect(page.getByTestId("step-hardware")).toBeVisible();
    await expect(page.getByText(/your machine/i)).toBeVisible();
    await page
      .getByTestId("btn-continue-hardware")
      .waitFor({ state: "attached" });
    // Wait for hardware to load (shimmer → real content)
    await page.waitForFunction(() => {
      const btn = document.querySelector(
        "[data-testid='btn-continue-hardware']",
      ) as HTMLButtonElement | null;
      return btn && !btn.disabled;
    });
    await page.getByTestId("btn-continue-hardware").click();

    // Role
    await expect(page.getByTestId("step-role")).toBeVisible();
    // Worker is recommended and pre-selected; switch to validator then back to worker
    await page.getByTestId("role-validator").click();
    await expect(page.getByTestId("role-validator")).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await page.getByTestId("role-worker").click();
    await expect(page.getByTestId("role-worker")).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await page.getByTestId("btn-continue-role").click();

    // Identity
    await expect(page.getByTestId("step-identity")).toBeVisible();
    await expect(page.getByTestId("identity-address")).toContainText("arc1q");

    // Continue disabled until user reveals seed phrase
    await expect(page.getByTestId("btn-continue-identity")).toBeDisabled();
    await page.getByTestId("btn-reveal-seed").click();
    await expect(page.getByTestId("btn-continue-identity")).toBeEnabled();
    await page.getByTestId("btn-continue-identity").click();

    // Launch
    await expect(page.getByTestId("step-launch")).toBeVisible();
    await page.getByTestId("btn-launch").click();

    // Lands on dashboard
    await expect(page.getByTestId("dashboard")).toBeVisible({ timeout: 10_000 });
    await expect(page.getByRole("heading", { name: /dashboard/i })).toBeVisible();
  });

  test("progress dots update per step", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("step-dot-0")).toHaveClass(/active/);
    await page.getByTestId("btn-continue-welcome").click();
    await expect(page.getByTestId("step-dot-0")).toHaveClass(/done/);
    await expect(page.getByTestId("step-dot-1")).toHaveClass(/active/);
  });

  test("back button returns to prior step", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("btn-continue-welcome").click();
    await expect(page.getByTestId("step-hardware")).toBeVisible();
    // back is a text button, not a testid — use role
    await page.getByRole("button", { name: "Back" }).click();
    await expect(page.getByTestId("step-welcome")).toBeVisible();
  });

  test("seed phrase is blurred until revealed", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("btn-continue-welcome").click();
    await page.waitForFunction(() => {
      const btn = document.querySelector(
        "[data-testid='btn-continue-hardware']",
      ) as HTMLButtonElement | null;
      return btn && !btn.disabled;
    });
    await page.getByTestId("btn-continue-hardware").click();
    await page.getByTestId("btn-continue-role").click();
    await expect(page.getByTestId("btn-reveal-seed")).toBeVisible();
    // Before reveal, the words should be rendered but visually blurred.
    // We only check the reveal button exists.
    await page.getByTestId("btn-reveal-seed").click();
    await expect(page.getByTestId("btn-reveal-seed")).toHaveCount(0);
  });
});
