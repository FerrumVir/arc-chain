import { expect, test } from "@playwright/test";
import { clearState } from "./helpers";

test.describe("Onboarding wizard", () => {
  test.beforeEach(async ({ page }) => {
    await clearState(page);
  });

  test("walks through all four steps and lands on dashboard", async ({
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

    // Identity (no role / hardware step - the role is derived later from
    // whether a model was downloaded)
    await expect(page.getByTestId("step-identity")).toBeVisible();
    await expect(page.getByTestId("identity-address")).toContainText("arc1q");
    await expect(page.getByTestId("btn-continue-identity")).toBeDisabled();
    await page.getByTestId("btn-reveal-seed").click();
    await expect(page.getByTestId("btn-continue-identity")).toBeEnabled();
    await page.getByTestId("btn-continue-identity").click();

    // Model picker. Added in v0.6.0 - this spec previously jumped straight
    // to launch and stalled here.
    await expect(page.getByTestId("step-model")).toBeVisible();
    // The recommended tier is pre-selected, so the user can keep clicking
    // through without choosing anything - Continue is enabled on arrival.
    await expect(page.getByTestId("btn-continue-model")).toBeEnabled();
    // Opting out is still available and keeps Continue usable.
    await page.getByTestId("tier-skip").click();
    await expect(page.getByTestId("btn-continue-model")).toBeEnabled();
    await page.getByTestId("btn-continue-model").click();

    // Launch
    await expect(page.getByTestId("step-launch")).toBeVisible();
    await expect(page.getByRole("button", { name: /set up this node/i })).toBeVisible();
    await page.getByTestId("btn-launch").click();

    // Lands on dashboard (mock mode resolves startNode + faucetClaim fast)
    await expect(page.getByTestId("dashboard")).toBeVisible({ timeout: 15_000 });
    await expect(page.getByRole("heading", { name: /dashboard/i })).toBeVisible();
  });

  test("progress dots update per step (4 dots, one per wizard step)", async ({
    page,
  }) => {
    await page.goto("/");
    // 4 dots - welcome, identity, model, launch. This asserted 3 and that
    // dot 3 was absent, which stopped being true when the model step landed.
    await expect(page.getByTestId("step-dot-0")).toHaveClass(/active/);
    await expect(page.getByTestId("step-dot-3")).toBeVisible();
    await expect(page.getByTestId("step-dot-4")).toHaveCount(0);
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

  test("does not promise model size, setup, or automatic updates will earn rewards", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.getByText(/confirm every download and install/i)).toBeVisible();
    await expect(page.getByText(/you start earning/i)).toHaveCount(0);
    await page.getByTestId("btn-continue-welcome").click();
    await page.getByTestId("btn-reveal-seed").click();
    await page.getByTestId("btn-continue-identity").click();
    await expect(page.getByTestId("step-model")).toContainText(
      /exact artifact ID/i,
    );
    await expect(page.getByTestId("step-model")).toContainText(
      /do not multiply rewards or guarantee demand/i,
    );
  });
});
