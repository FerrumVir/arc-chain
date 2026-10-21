import { expect, test } from "@playwright/test";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { openOn, seedMockOverrides, seedOnboarded } from "./helpers";

// Home is "Ask ARC": the chat answers from the network whatever the node is
// doing, and a one-line strip says what the node needs and offers its one fix.

const REFUSAL =
  "ARC refused to start the node because chain-data migration is not safely resolved: managed-node durability recovery failed: recovery node exited before its WAL barrier. Restart ARC after repairing the reported path or permissions; do not point v0.8 at the preserved legacy directory.";
const RECONCILING =
  "managed-node startup reconciliation is still in progress: ARC is checking how the previous node session ended before it starts or updates arc-node. Try again in a few seconds.";

test.describe("Home: Ask ARC", () => {
  test("ARC opens on Ask ARC, and the chat answers while the node is off", async ({
    page,
  }) => {
    await seedOnboarded(page);
    // Not a screen name: the app falls back to its own default, Home.
    await openOn(page, "default");
    await page.goto("/");

    await expect(page.getByTestId("inference-screen")).toBeVisible();
    await expect(page.getByRole("heading", { level: 1 })).toHaveText(PLAIN_COPY.home.title);
    await expect(page.getByTestId("nav-inference")).toHaveText(PLAIN_COPY.home.navLabel);
    await expect(page.getByTestId("nav-inference")).toHaveAttribute("aria-current", "page");

    const strip = page.getByTestId("node-strip");
    await expect(strip.getByTestId("node-strip-sentence")).toHaveText(PLAIN_COPY.nodeStrip.off);
    await expect(strip.getByTestId("node-strip-action")).toHaveText(
      PLAIN_COPY.actions.startNode,
    );

    await page.getByTestId("inference-prompt").fill("The largest planet is");
    await page.getByTestId("btn-run-inference").click();
    await expect(page.getByTestId("inference-output")).toContainText("mock response");

    // The strip's one button starts the node, and the sentence follows.
    await strip.getByTestId("node-strip-action").click();
    await expect(strip.getByTestId("node-strip-sentence")).toHaveText(
      PLAIN_COPY.nodeStrip.running,
    );
    await expect(strip.getByTestId("node-strip-action")).toHaveCount(0);
  });

  test("a blocked node shows its one fix on Home, and the chat still answers", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await openOn(page, "inference");
    await seedMockOverrides(page, { node_start_block: REFUSAL });
    await page.goto("/");

    const strip = page.getByTestId("node-strip");
    await expect(strip.getByTestId("node-strip-sentence")).toHaveText(
      PLAIN_COPY.startFailure.blocked.title,
    );
    await expect(strip.getByTestId("node-strip-action")).toHaveText(
      PLAIN_COPY.actions.restartArc,
    );
    await expect(strip.getByTestId("node-strip-details")).toBeHidden();
    await strip.getByTestId("node-strip-details-toggle").click();
    await expect(strip.getByTestId("node-strip-details")).toContainText(
      "recovery node exited before its WAL barrier",
    );

    await page.getByTestId("inference-prompt").fill("Water boils at");
    await page.getByTestId("btn-run-inference").click();
    await expect(page.getByTestId("inference-output")).toContainText("mock response");
  });

  test("while ARC finishes the last session, the strip says so and offers nothing to press", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await openOn(page, "inference");
    await seedMockOverrides(page, { node_start_block: RECONCILING });
    await page.goto("/");

    const strip = page.getByTestId("node-strip");
    await expect(strip).toHaveAttribute("data-tone", "busy");
    await expect(strip.getByTestId("node-strip-sentence")).toHaveText(
      PLAIN_COPY.startFailure.reconciling.title,
    );
    await expect(strip.getByTestId("node-strip-action")).toHaveCount(0);
    await expect(page.getByTestId("inference-prompt")).toBeEnabled();
  });
});
