import { expect, test, type Locator, type Page } from "@playwright/test";
import { seedComputePrompt, seedMockOverrides, seedOnboarded } from "./helpers";

const noticeId = "compute-prompt-dialog";
const saved = (page: Page) => page.evaluate(() =>
  JSON.parse(localStorage.getItem("arc-desktop-state-v1")!).config,
);

async function expectFocusInside(page: Page, target: Locator) {
  await expect(target).toBeFocused();
  expect(await page.getByTestId(noticeId).evaluate((dialog) =>
    dialog.contains(document.activeElement),
  )).toBe(true);
}

async function step(page: Page, key: string, target: Locator) {
  await page.keyboard.press(key);
  await expectFocusInside(page, target);
  console.log(key, await target.getAttribute("data-testid"));
}

async function screenshot(page: Page, name: string) {
  if (process.env.ARC623_SHOTS) {
    await page.screenshot({ path: `${process.env.ARC623_SHOTS}/${name}.png`, fullPage: true });
  }
}

for (const viewport of [{ width: 1280, height: 820 }, { width: 960, height: 640 }]) {
  test.describe(`Sharing notice keyboard ${viewport.width}x${viewport.height}`, () => {
    test.use({ viewport });

    test.beforeEach(async ({ page }) => {
      await page.route("**/*", (route) =>
        new URL(route.request().url()).origin === "http://localhost:4173"
          ? route.continue() : route.abort(),
      );
      await seedOnboarded(page);
      await seedComputePrompt(page);
    });

    test("complete Tab and Shift+Tab cycles never reach the background", async ({ page }) => {
      await page.goto("/");
      const gotIt = page.getByTestId("compute-prompt-not-now");
      const turnOff = page.getByTestId("compute-prompt-turn-off");
      const checkbox = page.getByTestId("compute-notice-keep-awake");
      await expectFocusInside(page, gotIt);
      await expect(checkbox).not.toBeChecked();
      // Two full cycles per direction: every enabled control, both wrap points.
      for (let cycle = 0; cycle < 2; cycle++) {
        for (const target of [turnOff, checkbox, gotIt]) await step(page, "Tab", target);
        for (const target of [checkbox, turnOff, gotIt]) await step(page, "Shift+Tab", target);
      }
      await step(page, "Tab", turnOff);
      await step(page, "Tab", checkbox);
      await screenshot(page, `focus-forward-wrap-${viewport.width}`);
      await step(page, "Shift+Tab", turnOff);
      await screenshot(page, `focus-reverse-wrap-${viewport.width}`);
      // The modal also rejects focus attempts from background app code.
      await page.getByTestId("btn-start").evaluate((button: HTMLElement) => button.focus());
      await expectFocusInside(page, turnOff);
      await page.keyboard.press("Escape");
      await expect(page.getByTestId(noticeId)).toHaveCount(0);
      await expect(page.getByTestId("nav-dashboard")).toBeFocused();
      expect((await saved(page)).computeConsent).toBe(true);
    });

    test("Space saves the checkbox, Enter dismisses, and focus returns to navigation", async ({ page }) => {
      await page.goto("/");
      const gotIt = page.getByTestId("compute-prompt-not-now");
      const checkbox = page.getByTestId("compute-notice-keep-awake");
      await expectFocusInside(page, gotIt);
      await step(page, "Shift+Tab", checkbox);
      await page.keyboard.press("Space");
      await expect(checkbox).toBeChecked();
      await expect(checkbox).toBeEnabled();
      await expectFocusInside(page, checkbox);
      expect((await saved(page)).keepAwakeWhileSharing).toBe(true);
      expect((await saved(page)).preventSleepDuringJobs ?? false).toBe(false);
      await step(page, "Tab", gotIt);
      await page.keyboard.press("Enter");
      await expect(page.getByTestId(noticeId)).toHaveCount(0);
      await expect(page.getByTestId("nav-dashboard")).toBeFocused();
      await screenshot(page, `focus-dismissed-${viewport.width}`);
      await page.getByTestId("nav-settings").click();
      const setting = page.getByTestId("sharing-keep-awake-toggle");
      await expect(setting).toBeChecked();
      await setting.click();
      await expect(setting).not.toBeChecked();
      expect((await saved(page)).keepAwakeWhileSharing).toBe(false);
      await page.reload();
      await expect(page.getByTestId("app-shell")).toBeVisible();
      await expect(page.getByTestId(noticeId)).toHaveCount(0);
    });

    test("Enter on Turn off withdraws consent and restores focus", async ({ page }) => {
      await page.goto("/");
      await expectFocusInside(page, page.getByTestId("compute-prompt-not-now"));
      await step(page, "Tab", page.getByTestId("compute-prompt-turn-off"));
      await page.keyboard.press("Enter");
      await expect(page.getByTestId(noticeId)).toHaveCount(0);
      await expect(page.getByTestId("nav-dashboard")).toBeFocused();
      const config = await saved(page);
      expect(config.computeConsent).toBe(false);
      expect(config.role).toBe("observer");
      expect(config.modelPath).toBeNull();
      await page.getByTestId("nav-settings").click();
      await expect(page.getByTestId("compute-consent-toggle")).not.toBeChecked();
    });

    test("Escape restores a control focused before a delayed notice", async ({ page }) => {
      await page.addInitScript(() => {
        (window as unknown as { __ARC_MOCK_WINDOW_HIDDEN__: boolean }).__ARC_MOCK_WINDOW_HIDDEN__ = true;
      });
      await page.goto("/");
      const previous = page.getByTestId("nav-settings");
      await expect(previous).toBeVisible();
      await expect(page.locator("#boot")).toHaveCount(0);
      await expect(page.getByTestId(noticeId)).toHaveCount(0);
      // Focus without changing route, while the notice is still deferred.
      await previous.focus();
      await page.evaluate(() => {
        (window as unknown as { __ARC_MOCK_WINDOW_HIDDEN__: boolean }).__ARC_MOCK_WINDOW_HIDDEN__ = false;
        window.dispatchEvent(new Event("focus"));
      });
      await expectFocusInside(page, page.getByTestId("compute-prompt-not-now"));
      await page.keyboard.press("Escape");
      await expect(page.getByTestId(noticeId)).toHaveCount(0);
      await expect(previous).toBeFocused();
    });

    test("pending saves keep focus inside with all controls disabled", async ({ page }) => {
      await page.goto("/");
      const dialog = page.getByTestId(noticeId);
      const checkbox = page.getByTestId("compute-notice-keep-awake");
      await expectFocusInside(page, page.getByTestId("compute-prompt-not-now"));
      await step(page, "Shift+Tab", checkbox);
      // Delay only subsequent mock IPC, without slowing the app's boot.
      await page.evaluate(() => history.replaceState(null, "", "/?slow=1500"));
      await page.keyboard.press("Space");
      await expect(checkbox).toBeDisabled();
      await expectFocusInside(page, dialog);
      await step(page, "Tab", dialog);
      await step(page, "Shift+Tab", dialog);
      await page.keyboard.press("Escape");
      await expect(dialog).toBeVisible();
      await expect(checkbox).toBeChecked();
      await expect(checkbox).toBeEnabled();
      await expectFocusInside(page, checkbox);
      await page.evaluate(() => history.replaceState(null, "", "/"));
      await step(page, "Shift+Tab", page.getByTestId("compute-prompt-turn-off"));
    });

    test("a rejected keyboard save restores its control and keeps the cycle contained", async ({ page }) => {
      await seedMockOverrides(page, {
        set_compute_contribution: { __reject: "The settings could not be saved." },
      });
      await page.goto("/");
      const turnOff = page.getByTestId("compute-prompt-turn-off");
      const checkbox = page.getByTestId("compute-notice-keep-awake");
      const gotIt = page.getByTestId("compute-prompt-not-now");
      await expectFocusInside(page, gotIt);
      await step(page, "Tab", turnOff);
      await page.keyboard.press("Enter");
      await expect(page.getByTestId("compute-prompt-error")).toHaveText("The settings could not be saved.");
      await expect(turnOff).toBeEnabled();
      await expectFocusInside(page, turnOff);
      for (const target of [checkbox, gotIt, turnOff]) await step(page, "Tab", target);
      for (const target of [gotIt, checkbox, turnOff]) await step(page, "Shift+Tab", target);
      expect((await saved(page)).computeConsent).toBe(true);
    });
  });
}
