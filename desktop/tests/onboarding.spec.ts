import { expect, test } from "@playwright/test";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { clearState, walkToLaunch } from "./helpers";

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
    await expect(page.getByTestId("identity-address")).toContainText("99".repeat(16));
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
    // No restart is offered where it can't help; the wizard's Retry stays.
    await expect(failure.getByTestId("btn-start-failure-restart")).toHaveCount(0);
    await expect(page.getByRole("button", { name: /retry/i })).toBeEnabled();
    await failure.getByTestId("btn-start-failure-details").click();
    await expect(failure.getByTestId("start-failure-details")).toBeHidden();
    await expect(page.getByTestId("dashboard")).toHaveCount(0);
  });

  test("a node that will not start says so plainly and offers one action", async ({
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
      };
    }, refusal);
    await page.goto("/");
    await walkToLaunch(page);
    await page.getByTestId("btn-launch").click();

    const failure = page.getByTestId("launch-error");
    await expect(failure.getByTestId("start-failure-title")).toHaveText(
      PLAIN_COPY.startFailure.blocked.title,
    );
    await expect(failure.getByTestId("btn-start-failure-restart")).toBeVisible();
    await expect(failure.getByTestId("start-failure-details")).toBeHidden();

    await failure.getByTestId("btn-start-failure-details").click();
    const details = failure.getByTestId("start-failure-details");
    await expect(details).toBeVisible();
    const text = (await details.textContent()) ?? "";
    expect(text.split("ARC refused to start the node")).toHaveLength(2);
    expect(text.split("Restart ARC after repairing")).toHaveLength(2);
    await expect(failure.getByTestId("btn-start-failure-copy")).toBeVisible();
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

  test("observer setup never claims that a model will be downloaded", async ({
    page,
  }) => {
    await page.goto("/");
    await page.getByTestId("btn-continue-welcome").click();
    await page.getByTestId("btn-reveal-seed").click();
    await page.getByTestId("btn-continue-identity").click();
    await page.getByTestId("tier-skip").click();
    await page.getByTestId("btn-continue-model").click();

    const summary = page.getByTestId("launch-summary");
    await expect(summary).toContainText("observer/router");
    await expect(summary).toContainText("without local model execution");
    await expect(summary).not.toContainText("fetch the selected model");
  });
});
