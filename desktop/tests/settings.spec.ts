import { expect, test } from "@playwright/test";
import { seedOnboarded, seedOnboardedWithoutConfig } from "./helpers";

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

  test("check-for-updates reports a result", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    await page.getByTestId("btn-check-update").click();
    // Update state now comes solely from the Tauri updater plugin, which
    // doesn't exist in the browser preview - so outside the native shell
    // the honest answer is "you're on the latest". The old assertion looked
    // for a hardcoded "v0.5.2" from the removed GitHub-API command.
    await expect(
      page.getByText(/You're running the latest version/),
    ).toBeVisible({ timeout: 4000 });
  });

  test("p2p port is its own field, not RPC + 1", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    await page.getByTestId("input-rpc-port").fill("9500");
    // The old hint promised P2P tracked RPC automatically. It never did.
    await expect(page.getByTestId("input-p2p-port")).toHaveValue("9091");
  });

  test("rpc port defaults to the port the node actually binds", async ({
    page,
  }) => {
    // Onboarded but with no stored config - the fallback path. (Clearing
    // localStorage outright would also clear `onboarded`, so the app would
    // render the wizard and Settings would never mount.)
    await seedOnboardedWithoutConfig(page);
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    // Must be 9090, the port the node actually binds - not the old 9944.
    await expect(page.getByTestId("input-rpc-port")).toHaveValue("9090");
    await expect(page.getByTestId("input-p2p-port")).toHaveValue("9091");
  });

  test("save works even when no config is stored", async ({ page }) => {
    // `save()` used to `if (!config) return;` - so on this exact state the
    // button did nothing and never showed its Saved confirmation.
    await seedOnboardedWithoutConfig(page);
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    await page.getByTestId("btn-save-settings").click();
    await expect(page.getByTestId("btn-save-settings")).toContainText("Saved");
  });

  test("core-count slider applies a new compute width", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    const slider = page.getByTestId("slider-worker-threads");
    await expect(slider).toBeVisible();
    // Bounded by the machine's detected core count (24 in the mock).
    await expect(slider).toHaveAttribute("max", "24");
    await slider.fill("6");
    await expect(page.getByTestId("worker-threads-value")).toHaveText("6 / 24");
    await page.getByTestId("btn-apply-threads").click();
    await expect(page.getByTestId("threads-result")).toContainText("6 cores");
  });
});
