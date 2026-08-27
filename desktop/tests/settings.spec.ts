import { expect, test } from "@playwright/test";
import {
  seedMockOverrides,
  seedOnboarded,
  seedOnboardedWithoutConfig,
} from "./helpers";

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
    // A browser preview cannot verify a native signed bundle. It must not
    // claim "latest" merely because the updater plugin is unavailable.
    await expect(page.getByTestId("update-status")).toHaveAttribute(
      "data-update-phase",
      "unsupported",
    );
    await expect(page.getByTestId("update-status")).toContainText(
      "installed ARC app",
    );
  });

  test("Linux package installs show package-manager update policy", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      update_install_policy: {
        canInstall: false,
        channel: "package-manager",
        instructions:
          "Install the new .deb or .rpm with the same package manager used for ARC.",
      },
    });
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    await expect(page.getByTestId("update-install-policy")).toContainText(
      "Install the new .deb or .rpm",
    );
  });

  test("auto-update preference persists and describes background behavior", async ({
    page,
  }) => {
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    await expect(page.getByTestId("update-policy")).toContainText(
      "after startup and every 24 hours",
    );

    await page.getByTestId("toggle-autoupdate").uncheck();
    await expect(page.getByTestId("update-policy")).toContainText(
      "Save settings to turn automatic background checks off",
    );
    await page.getByTestId("btn-save-settings").click();
    await expect(page.getByTestId("btn-save-settings")).toContainText("Saved");
    await expect(page.getByTestId("update-policy")).toContainText(
      "background checks are off",
    );

    // `seedOnboarded` is an init script and intentionally rewrites the fixture
    // on every reload, so inspect the same persisted blob the app will read on
    // its next real launch instead of having the fixture overwrite it first.
    const persistedAutoUpdate = await page.evaluate(() => {
      const raw = localStorage.getItem("arc-desktop-state-v1");
      return raw ? JSON.parse(raw).config?.autoUpdate : null;
    });
    expect(persistedAutoUpdate).toBe(false);
    await expect(page.getByTestId("toggle-autoupdate")).not.toBeChecked();
    await expect(page.getByTestId("btn-check-update")).toBeEnabled();
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
