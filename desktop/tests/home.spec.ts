import { expect, test } from "@playwright/test";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { openOn, seedMockOverrides, seedOnboarded } from "./helpers";

const REFUSAL =
  "ARC refused to start the node because chain-data migration is not safely resolved: managed-node durability recovery failed: recovery node exited before its WAL barrier. Restart ARC after repairing the reported path or permissions; do not point v0.8 at the preserved legacy directory.";
const RECONCILING =
  "managed-node startup reconciliation is still in progress: ARC is checking how the previous node session ended before it starts or updates arc-node. Try again in a few seconds.";

test.describe("Home: Ask ARC", () => {
  test("ARC opens on Ask ARC, and the chat answers while the node is off", async ({
    page,
  }) => {
    await seedOnboarded(page);
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
    await expect(strip).toHaveAttribute("data-tone", "idle");
    await expect(page.getByTestId("update-location-notice")).toHaveCount(0);

    const home = page.getByTestId("inference-screen");
    await expect(home).toContainText(PLAIN_COPY.home.proofBody);
    await expect(home).not.toContainText("Your own node first");
    await page.getByRole("button", { name: "What is how this works?" }).click();
    const routeHelp = page.getByTestId("inference-route-help");
    await expect(routeHelp).toContainText("Your prompt goes to the ARC coordinator this app reads the chain from");
    await expect(routeHelp).toContainText("takes the prompt only if no coordinator has answered the app yet this session");
    await expect(routeHelp).not.toContainText("your own node first");
    await page.keyboard.press("Escape");
    await expect(routeHelp).toHaveCount(0);

    await page.getByTestId("inference-prompt").fill("The largest planet is");
    await page.getByTestId("btn-run-inference").click();
    await expect(page.getByTestId("inference-output")).toContainText("mock response");

    await strip.getByTestId("node-strip-action").click();
    await expect(strip.getByTestId("node-strip-sentence")).toHaveText(
      PLAIN_COPY.nodeStrip.runningConnecting,
    );
    await expect(strip).toHaveAttribute("data-tone", "busy");
    await expect(strip.getByTestId("node-strip-action")).toHaveCount(0);
    await expect(strip.getByTestId("node-strip-sentence")).toHaveText(
      PLAIN_COPY.nodeStrip.sharingOff + " " + PLAIN_COPY.nodeStrip.online(6),
      { timeout: 15_000 },
    );
    await expect(strip).toHaveAttribute("data-tone", "ok");
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

    const userSelect = (testId: string) =>
      strip
        .getByTestId(testId)
        .evaluate((element) => getComputedStyle(element).userSelect);
    expect(await userSelect("node-strip-sentence")).toBe("text");
    expect(await userSelect("node-strip-details")).toBe("text");
    expect(await userSelect("node-strip-action")).toBe("none");
    await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
    await strip.getByTestId("node-strip-copy").click();
    await expect(strip.getByTestId("node-strip-copy")).toHaveText(PLAIN_COPY.actions.copied);
    const copied = await page.evaluate(() => navigator.clipboard.readText());
    expect(copied).toContain("recovery node exited before its WAL barrier");
    expect(copied.split("ARC refused to start the node")).toHaveLength(2);

    await page.getByTestId("inference-prompt").fill("Water boils at");
    await page.getByTestId("btn-run-inference").click();
    await expect(page.getByTestId("inference-output")).toContainText("mock response");
  });

  test("a copy ARC can't update in place says so in one calm line, and ARC stays usable", async ({
    page,
  }) => {
    await seedOnboarded(page);
    await openOn(page, "inference");
    await seedMockOverrides(page, {
      update_location_block:
        "ARC can't update itself from where it's running. Quit ARC, drag ARC Node into your Applications folder, then open it from there.",
    });
    await page.goto("/");

    const notice = page.getByTestId("update-location-notice");
    await expect(notice).toHaveText(PLAIN_COPY.updates.wrongLocation);
    await expect(notice).toHaveAttribute("role", "status");
    const colours = await notice.evaluate((element) => {
      const probe = document.createElement("span");
      probe.style.color = "var(--danger)";
      element.parentElement!.append(probe);
      const danger = getComputedStyle(probe).color;
      probe.remove();
      const icon = element.querySelector("svg");
      return {
        danger,
        used: [
          getComputedStyle(element).color,
          getComputedStyle(element).borderTopColor,
          icon ? getComputedStyle(icon).color : "",
        ],
      };
    });
    for (const colour of colours.used) expect(colour).not.toBe(colours.danger);

    const strip = page.getByTestId("node-strip");
    await expect(strip.getByTestId("node-strip-sentence")).toHaveText(PLAIN_COPY.nodeStrip.off);
    await strip.getByTestId("node-strip-action").click();
    await expect(strip.getByTestId("node-strip-sentence")).toHaveText(
      PLAIN_COPY.nodeStrip.runningConnecting,
    );
    await page.getByTestId("inference-prompt").fill("The sun is a");
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
