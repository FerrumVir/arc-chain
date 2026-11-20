import { expect, test } from "@playwright/test";
import { PLAIN_COPY } from "../src/lib/plain-copy";
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
    const storage = page.getByTestId("recovery-phrase-storage");
    await expect(storage).toContainText("This app keeps a plain-text copy on this computer only");
    await expect(storage).toContainText("store.json");
    await expect(storage).toContainText("network.arc.desktop");
    await expect(storage).toContainText("never sends it over the network");
    await expect(storage).toContainText("Write the phrase down, keep it offline, and keep this computer secure.");
    await expect(page.getByText("We don't store it")).toHaveCount(0);
    await page.getByTestId("slider-worker-threads").fill("12");
    await page.getByTestId("btn-apply-threads").click();
    await expect(page.getByTestId("threads-result")).toContainText("12 cores");
    await page.getByTestId("btn-reset").click();
    await expect(page.getByTestId("onboarding")).toBeVisible();
    await expect(page.getByTestId("step-welcome")).toBeVisible();

    await page.getByTestId("btn-continue-welcome").click();
    await page.getByTestId("btn-reveal-seed").click();
    await page.getByTestId("btn-continue-identity").click();
    await expect(
      page.getByRole("switch", { name: PLAIN_COPY.setup.compute.control }),
    ).toBeChecked();
    await page.getByTestId("btn-continue-model").click();
    await page.getByTestId("btn-launch").click();
    await expect(page.getByTestId("inference-screen")).toBeVisible({ timeout: 15_000 });
    await page.getByTestId("nav-settings").click();
    await expect(page.getByTestId("compute-consent-toggle")).toBeChecked();
    await expect(page.getByTestId("worker-threads-value")).toHaveText("12 / 24");
    await expect(page.getByTestId("compute-share-note")).toHaveText(
      "Uses up to 12 of your 24 processor cores at low priority, and about 8 GB of memory while ARC Node is running. It keeps running on battery. Turn this off to stop ARC jobs and free that memory.",
    );
  });

  test("check-for-updates reports a result", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    await page.getByTestId("btn-check-update").click();
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
      "Save settings to apply automatic update preferences",
    );
    await page.getByTestId("btn-save-settings").click();
    await expect(page.getByTestId("btn-save-settings")).toContainText("Saved");
    await expect(page.getByTestId("update-policy")).toContainText(
      "background checks are off",
    );

    const persistedAutoUpdate = await page.evaluate(() => {
      const raw = localStorage.getItem("arc-desktop-state-v1");
      return raw ? JSON.parse(raw).config?.autoUpdate : null;
    });
    expect(persistedAutoUpdate).toBe(false);
    await expect(page.getByTestId("toggle-autoupdate")).not.toBeChecked();
    await expect(page.getByTestId("btn-check-update")).toBeEnabled();
  });

  test("unattended installs default on and can be turned off", async ({
    page,
  }) => {
    await page.addInitScript(() => {
      const raw = localStorage.getItem("arc-desktop-state-v1");
      if (!raw) return;
      const state = JSON.parse(raw);
      delete state.config?.autoInstallUpdates;
      localStorage.setItem("arc-desktop-state-v1", JSON.stringify(state));
    });
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    const autoInstall = page.getByTestId("toggle-autoinstall-updates");
    await expect(autoInstall).toBeChecked();
    await expect(page.getByTestId("update-policy")).toContainText(
      "may install and restart ARC automatically",
    );

    await autoInstall.uncheck();
    await expect(page.getByTestId("update-policy")).toContainText(
      "Save settings to apply automatic update preferences",
    );
    await page.getByTestId("btn-save-settings").click();
    await expect(page.getByTestId("btn-save-settings")).toContainText("Saved");
    await expect(page.getByTestId("update-policy")).toContainText(
      "Background checks never install an update",
    );

    const persistedAutoInstall = await page.evaluate(() => {
      const raw = localStorage.getItem("arc-desktop-state-v1");
      return raw ? JSON.parse(raw).config?.autoInstallUpdates : null;
    });
    expect(persistedAutoInstall).toBe(false);
    await expect(page.getByTestId("toggle-autoupdate")).toBeChecked();
    await expect(autoInstall).not.toBeChecked();
  });

  test("p2p port is its own field, not RPC + 1", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    await page.getByTestId("input-rpc-port").fill("9500");
    await expect(page.getByTestId("input-p2p-port")).toHaveValue("9091");
  });

  test("rpc port defaults to the port the node actually binds", async ({
    page,
  }) => {
    await seedOnboardedWithoutConfig(page);
    await page.goto("/");
    await page.getByTestId("nav-settings").click();
    await expect(page.getByTestId("input-rpc-port")).toHaveValue("9090");
    await expect(page.getByTestId("input-p2p-port")).toHaveValue("9091");
  });

  test("save works even when no config is stored", async ({ page }) => {
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
    await expect(slider).toHaveAttribute("max", "24");
    await slider.fill("6");
    await expect(page.getByTestId("worker-threads-value")).toHaveText("6 / 24");
    await page.getByTestId("btn-apply-threads").click();
    await expect(page.getByTestId("threads-result")).toContainText("6 cores");
  });
});
