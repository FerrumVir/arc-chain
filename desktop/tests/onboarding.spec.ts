import { expect, test, type Page } from "@playwright/test";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { clearState, walkToLaunch, wheelIntoView } from "./helpers";

/** The node config the app saved, read from its persisted UI state. */
async function savedConfig(page: Page) {
  return page.evaluate(() => {
    const raw = localStorage.getItem("arc-desktop-state-v1");
    return raw ? JSON.parse(raw).config : null;
  });
}

test.describe("Onboarding wizard", () => {
  test.beforeEach(async ({ page }) => {
    await clearState(page);
  });

  test("walks through all four steps and lands on Home", async ({
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
    // Nothing opts in by default, even where the model is recommended.
    // Observer mode is pre-selected (Continue is enabled on arrival), and
    // what contributing runs, when, its power use and keep-awake are
    // explained above the choices.
    await expect(page.getByTestId("tier-skip")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("tier-standard")).toHaveAttribute("aria-pressed", "false");
    await expect(page.getByTestId("tier-standard")).toContainText("Recommended");
    await expect(page.getByTestId("compute-choice-default")).toContainText(
      "Contributing compute is off until you select the ARC model.",
    );
    await expect(page.getByTestId("step-model")).not.toContainText("pre-select");
    await expect(page.getByTestId("step-model")).not.toContainText("while the app is open");
    const disclosure = page.getByTestId("contribution-disclosure");
    await expect(disclosure).toBeVisible();
    const runs = disclosure.getByTestId("disclosure-runs");
    await expect(runs).toContainText("your node runs AI jobs for the ARC network on this computer");
    await expect(runs).toContainText("it receives a prompt and sends back the generated text");
    const when = disclosure.getByTestId("disclosure-when");
    await expect(when).toContainText("from the menu bar or system tray after you close the window");
    await expect(when).toContainText("after you log in while Start node on app launch is on");
    const power = disclosure.getByTestId("disclosure-power");
    await expect(power).toContainText("every processor core");
    await expect(power).toContainText("battery drain");
    const keepAwake = disclosure.getByTestId("disclosure-keep-awake");
    await expect(keepAwake).toContainText("Off unless you turn it on in Settings.");
    await expect(keepAwake).toContainText("does not sleep on its own while a job is computing");
    await expect(keepAwake).toContainText("on Linux a sleep you request is also blocked until the job ends");
    await expect(keepAwake).toContainText("closing the lid can still put it to sleep");
    await expect(page.getByTestId("btn-continue-model")).toBeEnabled();
    // Selecting the model is the explicit opt-in; Skip takes it back, and
    // selecting it again opts in again.
    await page.getByTestId("tier-standard").click();
    await expect(page.getByTestId("tier-standard")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("tier-skip")).toHaveAttribute("aria-pressed", "false");
    await page.getByTestId("tier-skip").click();
    await expect(page.getByTestId("tier-skip")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("btn-continue-model")).toBeEnabled();
    await page.getByTestId("tier-standard").click();
    await expect(page.getByTestId("tier-standard")).toHaveAttribute("aria-pressed", "true");
    await wheelIntoView(page, page.getByTestId("btn-continue-model"));
    await page.getByTestId("btn-continue-model").click();

    // Launch
    await expect(page.getByTestId("step-launch")).toBeVisible();
    await expect(page.getByTestId("launch-summary")).toContainText("downloads its AI model");
    await expect(page.getByRole("button", { name: /start my node/i })).toBeVisible();
    await expect(page.getByTestId("launch-testnet-note")).toHaveText(
      PLAIN_COPY.setup.testnet,
    );
    await wheelIntoView(page, page.getByTestId("btn-launch"));
    await page.getByTestId("btn-launch").click();

    // Lands on Home: Ask ARC, with the node's one-line strip.
    await expect(page.getByTestId("inference-screen")).toBeVisible({ timeout: 15_000 });
    await expect(
      page.getByRole("heading", { name: PLAIN_COPY.home.title, level: 1 }),
    ).toBeVisible();
    await expect(page.getByTestId("node-strip")).toBeVisible();
    // The explicit selection is what recorded consent.
    const saved = await savedConfig(page);
    expect(saved?.computeConsent).toBe(true);
    expect(saved?.role).toBe("worker");
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
    // A plain sentence, with the native text left open: for a failure the
    // panel does not recognise, that text is what says what went wrong.
    const failure = page.getByTestId("launch-error");
    await expect(failure.getByTestId("start-failure-title")).toBeVisible();
    await expect(failure.getByTestId("start-failure-details")).toHaveText(
      "release checksum manifest returned HTTP 404",
    );
    // Setup never traps: Continue to ARC leads, Try again follows.
    await expect(failure.getByTestId("btn-start-failure-restart")).toHaveCount(0);
    await expect(page.getByTestId("btn-continue-to-arc")).toHaveText(
      PLAIN_COPY.actions.continueToArc,
    );
    await expect(page.getByRole("button", { name: /try again/i })).toBeEnabled();
    await failure.getByTestId("btn-start-failure-details").click();
    await expect(failure.getByTestId("start-failure-details")).toBeHidden();
    await expect(page.getByTestId("dashboard")).toHaveCount(0);
  });

  test("a node that will not start lets setup finish on Home, where the strip has the fix", async ({
    page,
  }) => {
    // The refusal v0.8.12 desktops showed in the field, nested framing and all.
    const refusal =
      "ARC refused to start the node because chain-data migration is not safely resolved: managed-node durability recovery failed: ARC refused to start the node because chain-data migration is not safely resolved: managed-node startup reconciliation is still in progress; binary replacement and node start are temporarily blocked. Restart ARC after repairing the reported path or permissions; do not point v0.8 at the preserved legacy directory.. Restart ARC after repairing the reported path or permissions; do not point v0.8 at the preserved legacy directory.";
    await page.addInitScript((message) => {
      (window as unknown as { __ARC_MOCK__: Record<string, unknown> }).__ARC_MOCK__ = {
        // A thenable rejects only when the mock returns it, like the native
        // command's string error.
        start_node: {
          then: (_resolve: unknown, reject: (reason: unknown) => void) => reject(message),
        },
        // The start fence still refuses on Home, as it would natively.
        node_start_block: message,
      };
    }, refusal);
    await page.goto("/");
    await walkToLaunch(page);
    await page.getByTestId("btn-launch").click();

    const failure = page.getByTestId("launch-error");
    await expect(failure.getByTestId("start-failure-title")).toHaveText(
      PLAIN_COPY.startFailure.blocked.title,
    );
    // The panel offers no fix of its own here: setup's buttons lead.
    await expect(failure.getByTestId("btn-start-failure-restart")).toHaveCount(0);
    await expect(failure.getByTestId("start-failure-details")).toBeHidden();
    await expect(page.getByTestId("launch-continue-hint")).toHaveText(
      PLAIN_COPY.setup.failedHint,
    );

    await failure.getByTestId("btn-start-failure-details").click();
    const details = failure.getByTestId("start-failure-details");
    await expect(details).toBeVisible();
    const text = (await details.textContent()) ?? "";
    expect(text.split("ARC refused to start the node")).toHaveLength(2);
    expect(text.split("Restart ARC after repairing")).toHaveLength(2);
    await expect(failure.getByTestId("btn-start-failure-copy")).toBeVisible();
    // The error can be selected and copied by hand too; the buttons cannot.
    for (const [testId, expected] of [
      ["start-failure-title", "text"],
      ["start-failure-details", "text"],
      ["btn-start-failure-copy", "none"],
    ] as const) {
      const value = await failure
        .getByTestId(testId)
        .evaluate((element) => getComputedStyle(element).userSelect);
      expect(value, testId).toBe(expected);
    }
    await expect(page.getByTestId("dashboard")).toHaveCount(0);

    // Continue to ARC: Home opens without a running node, and the strip
    // carries the node's one fix.
    await page.getByTestId("btn-continue-to-arc").click();
    await expect(page.getByTestId("inference-screen")).toBeVisible();
    const strip = page.getByTestId("node-strip");
    await expect(strip.getByTestId("node-strip-sentence")).toHaveText(
      PLAIN_COPY.startFailure.blocked.title,
    );
    await expect(strip.getByTestId("node-strip-action")).toHaveText(
      PLAIN_COPY.actions.restartArc,
    );
    await expect(page.getByTestId("inference-prompt")).toBeEnabled();
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
    await expect(page.getByText(PLAIN_COPY.setup.alwaysReady)).toBeVisible();
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

  test("a .deb or .rpm install isn't promised automatic updates", async ({ page }) => {
    await page.addInitScript(() => {
      (window as unknown as { __ARC_MOCK__: Record<string, unknown> }).__ARC_MOCK__ = {
        update_install_policy: {
          canInstall: false,
          channel: "package-manager",
          instructions:
            "A signed update is available. Install the new .deb or .rpm with the same package manager used for this ARC installation.",
        },
      };
    });
    await page.goto("/");
    await expect(page.getByText(PLAIN_COPY.setup.alwaysReadyPackageManaged)).toBeVisible();
    await expect(page.getByText(/updates automatically/i)).toHaveCount(0);
  });

  test("observer setup never claims that a model will be downloaded", async ({
    page,
  }) => {
    // The smallest window the app allows (tauri.conf.json minWidth and
    // minHeight). The compute step is taller than this, so it must scroll.
    await page.setViewportSize({ width: 960, height: 640 });
    await page.goto("/");
    await wheelIntoView(page, page.getByTestId("btn-continue-welcome"));
    await page.getByTestId("btn-continue-welcome").click();
    await page.getByTestId("btn-reveal-seed").click();
    await wheelIntoView(page, page.getByTestId("btn-continue-identity"));
    await page.getByTestId("btn-continue-identity").click();
    // No choice made, on a machine where the model is recommended:
    // continuing keeps contribution off.
    await expect(page.getByTestId("tier-standard")).toContainText("Recommended");
    await expect(page.getByTestId("tier-standard")).toHaveAttribute("aria-pressed", "false");
    await wheelIntoView(page, page.getByTestId("btn-continue-model"));
    await page.getByTestId("btn-continue-model").click();

    const summary = page.getByTestId("launch-summary");
    await expect(summary).toContainText(PLAIN_COPY.setup.readyWithoutModel);
    await expect(summary).toContainText("doesn't run AI jobs");
    await expect(summary).not.toContainText("downloads its AI model");

    await wheelIntoView(page, page.getByTestId("btn-launch"));
    await page.getByTestId("btn-launch").click();
    await expect(page.getByTestId("inference-screen")).toBeVisible({ timeout: 15_000 });
    // No consent was recorded from the recommendation.
    const saved = await savedConfig(page);
    expect(saved?.computeConsent).toBe(false);
    expect(saved?.role).toBe("observer");
    expect(saved?.modelPath).toBeNull();
  });
});
