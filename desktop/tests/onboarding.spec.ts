import { expect, test } from "@playwright/test";
import { clearState, wheelIntoView } from "./helpers";

test.describe("Onboarding wizard", () => {
  test.beforeEach(async ({ page }) => {
    await clearState(page);
  });

  test("walks through all four steps at the default 1180x780 window and lands on dashboard", async ({
    page,
  }) => {
    // The app opens at 1180x780 (tauri.conf.json). Each step's action must be
    // reachable by scrolling there, not only by Playwright's own scrolling.
    await page.setViewportSize({ width: 1180, height: 780 });
    await page.goto("/");

    // Welcome
    await expect(page.getByTestId("onboarding")).toBeVisible();
    await expect(page.getByTestId("step-welcome")).toBeVisible();
    await expect(
      page.getByRole("heading", { name: /welcome to arc/i }),
    ).toBeVisible();
    await wheelIntoView(page, page.getByTestId("btn-continue-welcome"));
    await page.getByTestId("btn-continue-welcome").click();

    // Identity (no role / hardware step - the role is derived later from
    // whether a model was downloaded)
    await expect(page.getByTestId("step-identity")).toBeVisible();
    await expect(page.getByTestId("identity-address")).toContainText("99".repeat(16));
    await expect(page.getByTestId("btn-continue-identity")).toBeDisabled();
    await page.getByTestId("btn-reveal-seed").click();
    await expect(page.getByTestId("btn-continue-identity")).toBeEnabled();
    await wheelIntoView(page, page.getByTestId("btn-continue-identity"));
    await page.getByTestId("btn-continue-identity").click();

    // Model picker. Added in v0.6.0 - this spec previously jumped straight
    // to launch and stalled here.
    await expect(page.getByTestId("step-model")).toBeVisible();
    // ARC-50 checklist 1.5: nothing opts in by default. Observer mode is
    // pre-selected (Continue is enabled on arrival), and what contributing
    // shares, its power use and keep-awake are explained before any choice.
    await expect(page.getByTestId("tier-skip")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("tier-standard")).toHaveAttribute("aria-pressed", "false");
    await expect(page.getByTestId("compute-choice-default")).toContainText(
      "Contributing compute is off until you select the ARC model.",
    );
    const disclosure = page.getByTestId("contribution-disclosure");
    await expect(disclosure).toBeVisible();
    const shared = disclosure.getByTestId("disclosure-shared");
    await expect(shared).toContainText("a public label made from that address");
    await expect(shared).toContainText("your operating system and processor type");
    await expect(shared).toContainText("it receives the prompt and sends back the generated text");
    const power = disclosure.getByTestId("disclosure-power");
    await expect(power).toContainText("every processor core");
    await expect(power).toContainText("battery drain");
    await expect(power).toContainText("after you log in while Start node on app launch is on");
    const keepAwake = disclosure.getByTestId("disclosure-keep-awake");
    await expect(keepAwake).toContainText("Off unless you turn it on in Settings.");
    await expect(keepAwake).toContainText(
      "does not sleep on its own while a job is computing",
    );
    await expect(keepAwake).toContainText(
      "on Linux a sleep you request is also blocked until the job ends",
    );
    await expect(keepAwake).toContainText("closing the lid can still put it to sleep");
    await expect(keepAwake).toContainText("applies the next time the node starts");
    await expect(page.getByTestId("btn-continue-model")).toBeEnabled();
    // Selecting the model is the explicit opt-in; Skip takes it back.
    await page.getByTestId("tier-standard").click();
    await expect(page.getByTestId("tier-standard")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("tier-skip")).toHaveAttribute("aria-pressed", "false");
    await page.getByTestId("tier-skip").click();
    await expect(page.getByTestId("tier-skip")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("btn-continue-model")).toBeEnabled();
    await wheelIntoView(page, page.getByTestId("btn-continue-model"));
    await page.getByTestId("btn-continue-model").click();

    // Launch
    await expect(page.getByTestId("step-launch")).toBeVisible();
    await expect(page.getByRole("button", { name: /set up this node/i })).toBeVisible();
    await wheelIntoView(page, page.getByTestId("btn-launch"));
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

  test("keeps setup visible while the binary download is pending or fails", async ({ page }) => {
    await page.addInitScript(() => {
      const state = window as unknown as {
        __ARC_MOCK__: Record<string, unknown>;
        rejectSetupDownload: () => void;
      };
      state.__ARC_MOCK__ = {
        ensure_binary: new Promise((_resolve, reject) => {
          state.rejectSetupDownload = () => reject(new Error("release checksum manifest returned HTTP 404"));
        }),
      };
    });
    await page.goto("/");
    await page.getByTestId("btn-continue-welcome").click();
    await page.getByTestId("btn-reveal-seed").click();
    await page.getByTestId("btn-continue-identity").click();
    await page.getByTestId("tier-skip").click();
    await page.getByTestId("btn-continue-model").click();
    await page.getByTestId("btn-launch").click();
    await expect(page.getByTestId("step-launch")).toBeVisible();
    await expect(page.getByTestId("dashboard")).toHaveCount(0);
    await page.evaluate(() => {
      (window as unknown as { rejectSetupDownload: () => void }).rejectSetupDownload();
    });
    await expect(page.getByText("release checksum manifest returned HTTP 404", { exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: /retry/i })).toBeEnabled();
    await expect(page.getByTestId("dashboard")).toHaveCount(0);
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

  test("at the 960x640 minimum window every step's action is reachable, and observer setup never claims a model download", async ({
    page,
  }) => {
    // The smallest window the app allows (tauri.conf.json minWidth/minHeight).
    // The compute step is taller than this, so it must scroll.
    await page.setViewportSize({ width: 960, height: 640 });
    await page.goto("/");
    await wheelIntoView(page, page.getByTestId("btn-continue-welcome"));
    await page.getByTestId("btn-continue-welcome").click();
    await page.getByTestId("btn-reveal-seed").click();
    await wheelIntoView(page, page.getByTestId("btn-continue-identity"));
    await page.getByTestId("btn-continue-identity").click();
    // No choice made: continuing keeps contribution off (ARC-50 1.5).
    await expect(page.getByTestId("tier-standard")).toBeVisible();
    await wheelIntoView(page, page.getByTestId("btn-continue-model"));
    await page.getByTestId("btn-continue-model").click();
    await wheelIntoView(page, page.getByTestId("btn-launch"));
    await page.getByTestId("btn-launch").click({ trial: true });

    const summary = page.getByTestId("launch-summary");
    await expect(summary).toContainText("observer/router");
    await expect(summary).toContainText("without local model execution");
    await expect(summary).not.toContainText("fetch the selected model");
  });
});
