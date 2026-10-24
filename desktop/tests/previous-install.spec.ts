import { expect, test } from "@playwright/test";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { clearState, seedMockOverrides, seedOnboarded } from "./helpers";

// A Windows reinstall whose uninstaller deleted the app data leaves the
// node's data directory behind. Launch sets that directory aside (renamed,
// never deleted) and says so in one sentence: during setup, where the user
// lands with no identity, and on every screen after it, until dismissed.

const NOTICE = {
  setAsideDataDir:
    "C:\\Users\\community\\.arc\\data-v3.previous-install-20261024T141500Z",
  activeDataDir: "C:\\Users\\community\\.arc\\data-v3",
  setAsideAt: 1_792_851_300_000,
};

test.describe("node data from an earlier install", () => {
  test("setup says the data was set aside and how a wallet comes back", async ({
    page,
  }) => {
    await clearState(page);
    await seedMockOverrides(page, { load_previous_install_notice: NOTICE });
    await page.goto("/");

    await expect(page.getByTestId("step-welcome")).toBeVisible();
    const notice = page.getByTestId("previous-install-notice");
    await expect(notice).toBeVisible();
    await expect(notice.getByTestId("previous-install-sentence")).toHaveText(
      PLAIN_COPY.previousInstall.sentence,
    );
    await expect(notice.getByTestId("previous-install-path")).toHaveText(
      NOTICE.setAsideDataDir,
    );
    // Calm: a status, not an alert, in plain words.
    await expect(notice).toHaveAttribute("role", "status");
    for (const jargon of [
      "migration",
      "reconciliation",
      "WAL",
      "namespace",
      "receipt",
      "refused",
      "lifecycle",
    ]) {
      expect(PLAIN_COPY.previousInstall.sentence).not.toContain(jargon);
    }
    expect(PLAIN_COPY.previousInstall.sentence).toContain("12 words");

    // Setup goes on underneath it.
    await expect(page.getByTestId("btn-continue-welcome")).toBeEnabled();
    await notice.getByTestId("dismiss-previous-install").click();
    await expect(notice).toHaveCount(0);
    await expect(page.getByTestId("step-welcome")).toBeVisible();
  });

  test("Home carries the same sentence, and the node strip stays calm", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await seedMockOverrides(page, { load_previous_install_notice: NOTICE });
    await page.goto("/");

    const notice = page.getByTestId("previous-install-notice");
    await expect(notice).toBeVisible();
    await expect(notice.getByTestId("previous-install-sentence")).toHaveText(
      PLAIN_COPY.previousInstall.sentence,
    );
    // The node is simply off: nothing red, and Start is still the one action.
    const strip = page.getByTestId("node-strip");
    await expect(strip.getByTestId("node-strip-sentence")).toHaveText(
      PLAIN_COPY.nodeStrip.off,
    );
    await expect(strip).toHaveAttribute("data-tone", "idle");

    await notice.getByTestId("dismiss-previous-install").click();
    await expect(notice).toHaveCount(0);
  });

  test("a dismissal the native side cannot record keeps the notice and says why", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await seedMockOverrides(page, {
      load_previous_install_notice: NOTICE,
      dismiss_previous_install_notice: {
        __reject: "existing identity store failed private owner/permission validation",
      },
    });
    await page.goto("/");

    const notice = page.getByTestId("previous-install-notice");
    await expect(notice).toBeVisible();
    await notice.getByTestId("dismiss-previous-install").click();
    await expect(notice.getByRole("alert")).toContainText(
      "existing identity store failed private owner/permission validation",
    );
    await expect(notice.getByTestId("previous-install-sentence")).toHaveText(
      PLAIN_COPY.previousInstall.sentence,
    );
    await expect(notice.getByTestId("dismiss-previous-install")).toBeEnabled();
  });
});
