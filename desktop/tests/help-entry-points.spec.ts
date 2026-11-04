import { expect, test, type Page } from "@playwright/test";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { SUPPORT_LINKS } from "../src/lib/support";
import { clearState, openOn, seedMockOverrides, seedOnboarded, walkToLaunch } from "./helpers";

// Get help from Home's node strip and from first-run setup (ARC-410, revision 2 §3-C), reusing #225's Get help:
// same destinations, same safe-summary policy, same wording, no reply or receipt promised. The privacy matrix
// (share-privacy.spec.ts) also covers the strip's Get help copy. Synthetic data only.

const PLATFORM = { endpointConfigured: false, appVersion: "0.8.16", os: "macos", arch: "aarch64", homeDir: "/Volumes/Data/entry-fixture-home" };
const REFUSAL = "ARC refused to start the node: cannot lock /Volumes/Data/entry-fixture-home/.arc/data-v3/.arc-node.lock (os error 35)";

const stopped = (health: "lite" | "offline") => ({
  running: false, pid: null, health, version: "0.8.16", peers: 0, round: 0, committed: 0, height: 0, uptimeSeconds: 0,
  address: "99".repeat(32), rpcPort: 9090, lastError: null, coordinatorUrl: health === "lite" ? "https://140.82.16.112" : null,
  chainHost: "http://140.82.16.112:9090", chainHeight: 123_469, chainRound: 9_596_644, chainBlockAgeSeconds: 400, workerThreads: null, cpuCores: 8,
});

async function home(page: Page, overrides: Record<string, unknown>) {
  await seedOnboarded(page);
  await openOn(page, "inference");
  await page.addInitScript(() => {
    const s = JSON.parse(localStorage.getItem("arc-desktop-state-v1")!);
    s.config.autoStart = false;
    localStorage.setItem("arc-desktop-state-v1", JSON.stringify(s));
  });
  await seedMockOverrides(page, { node_care_info: PLATFORM, ...overrides });
  await page.goto("/");
  await expect(page.getByTestId("node-strip")).toBeVisible();
}

/** Press Tab until the element has focus, as a keyboard user would find it. */
async function tabTo(page: Page, testId: string, max = 80) {
  for (let i = 0; i < max; i++) {
    await page.keyboard.press("Tab");
    if (await page.evaluate((id) => document.activeElement?.getAttribute("data-testid") === id, testId)) return;
  }
  throw new Error(`${testId} was not reached with Tab`);
}

test.describe("Get help on Home's node strip", () => {
  test("node off: reachable by keyboard, opens with Enter, says Node off", async ({ page }) => {
    await home(page, { node_status: stopped("lite") });
    await expect(page.getByTestId("node-strip-sentence")).toHaveText(PLAIN_COPY.nodeStrip.off);
    await tabTo(page, "node-strip-help-toggle");
    await page.keyboard.press("Enter");
    const details = page.getByTestId("node-strip-help-details");
    await expect(details).toContainText("State: Node off");
    await expect(details).toContainText(`ARC says: ${PLAIN_COPY.nodeStrip.off}`);
    await expect(page.getByTestId("node-strip-help-report")).toHaveText(PLAIN_COPY.help.report.notConfigured);
  });

  test("no connection: the help says so", async ({ page }) => {
    await home(page, { node_status: { ...stopped("offline"), lastError: "No node is answering on 127.0.0.1:9090 and no ARC coordinator answered." } });
    await expect(page.getByTestId("node-strip-sentence")).toHaveText(PLAIN_COPY.nodeStrip.offUnreachable);
    await page.getByTestId("node-strip-help-toggle").click();
    await expect(page.getByTestId("node-strip-help-details")).toContainText("State: No connection");
  });

  test("a refused start: scrubbed detail with the home folder, safe summary without it", async ({ page }) => {
    await home(page, { node_start_block: REFUSAL });
    await page.getByTestId("node-strip-help-toggle").click();
    const details = page.getByTestId("node-strip-help-details");
    await expect(details).toContainText("State: Problem shown on Home");
    await expect(details).toContainText("~/.arc/data-v3/.arc-node.lock");
    await expect(details).not.toContainText("entry-fixture-home");
  });

  test("a refused start with the home folder unavailable shares no native text", async ({ page }) => {
    await home(page, { node_start_block: REFUSAL, node_care_info: { ...PLATFORM, homeDir: "" } });
    await page.getByTestId("node-strip-help-toggle").click();
    const details = page.getByTestId("node-strip-help-details");
    await expect(details).toContainText(PLAIN_COPY.share.omitted);
    for (const native of ["entry-fixture-home", "cannot lock", "os error 35"]) await expect(details).not.toContainText(native);
  });

  test("a running, connected node shows no Get help on the strip", async ({ page }) => {
    await home(page, {});
    await page.getByTestId("node-strip-action").click(); // Start node (mock)
    await expect(page.getByTestId("node-strip")).toHaveAttribute("data-tone", "ok", { timeout: 15_000 });
    await expect(page.getByTestId("node-strip-help")).toHaveCount(0);
  });

  test("at the 960×640 minimum the panel is reachable, and failures stay honest", async ({ page }) => {
    await page.setViewportSize({ width: 960, height: 640 });
    await page.addInitScript(() => {
      Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: () => Promise.reject(new Error("denied")) } });
    });
    await home(page, { node_status: { ...stopped("offline"), lastError: "No node is answering on 127.0.0.1:9090 and no ARC coordinator answered." }, open_external: { __reject: "no browser in tests" } });
    await tabTo(page, "node-strip-help-toggle");
    await page.keyboard.press("Enter");
    const copy = page.getByTestId("node-strip-help-copy");
    await copy.scrollIntoViewIfNeeded();
    await expect(copy).toBeInViewport({ ratio: 1 });
    await copy.click();
    await expect(page.getByTestId("node-strip-help-copy-failed")).toHaveText(`${PLAIN_COPY.help.copyFailedFallback} ${PLAIN_COPY.share.review}`);
    const telegram = page.getByTestId("node-strip-help-telegram");
    await telegram.scrollIntoViewIfNeeded();
    await expect(telegram).toBeInViewport({ ratio: 1 });
    await telegram.click();
    await expect(page.getByTestId("node-strip-help-open-failed")).toHaveText(PLAIN_COPY.help.openFailed(SUPPORT_LINKS.telegram));
  });
});

test.describe("Get help in first-run setup", () => {
  test("every setup step offers Get help, reachable by keyboard", async ({ page }) => {
    await clearState(page);
    await seedMockOverrides(page, { node_care_info: PLATFORM });
    await page.goto("/");
    await expect(page.getByTestId("step-welcome")).toBeVisible();
    await tabTo(page, "onboarding-help-toggle");
    await page.keyboard.press("Enter");
    const details = page.getByTestId("onboarding-help-details");
    await expect(details).toContainText("State: Setting up ARC");
    await expect(details).toContainText("App: 0.8.16");
    // Nothing native is shared during setup unless a launch failed (that panel carries its own Get help).
    await expect(details).not.toContainText("Details:");
    await expect(page.getByTestId("onboarding-help-report")).toHaveText(PLAIN_COPY.help.report.notConfigured);
    await page.getByTestId("onboarding-help-toggle").click();

    await page.getByTestId("btn-continue-welcome").click();
    await expect(page.getByTestId("step-identity")).toBeVisible();
    await expect(page.getByTestId("onboarding-help")).toBeVisible();
    await page.getByTestId("btn-reveal-seed").click();
    await page.getByTestId("btn-continue-identity").click();
    await expect(page.getByTestId("onboarding-help")).toBeVisible();
    await page.getByTestId("tier-skip").click();
    await page.getByTestId("btn-continue-model").click();
    await expect(page.getByTestId("step-launch")).toBeVisible();
    await expect(page.getByTestId("onboarding-help")).toBeVisible();
  });

  test("at the 960×640 minimum the setup help opens, copies and links honestly", async ({ page }) => {
    await page.setViewportSize({ width: 960, height: 640 });
    await clearState(page);
    await seedMockOverrides(page, { node_care_info: PLATFORM, open_external: { __reject: "no browser in tests" } });
    await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.goto("/");
    await expect(page.getByTestId("step-welcome")).toBeVisible();
    await tabTo(page, "onboarding-help-toggle");
    await page.keyboard.press("Enter");
    const copy = page.getByTestId("onboarding-help-copy");
    await copy.scrollIntoViewIfNeeded();
    await expect(copy).toBeInViewport({ ratio: 1 });
    await copy.click();
    await expect(copy).toHaveText(PLAIN_COPY.actions.copied);
    const clip = await page.evaluate(() => navigator.clipboard.readText());
    expect(clip).toBe(await page.getByTestId("onboarding-help-details").textContent());
    const discord = page.getByTestId("onboarding-help-discord");
    await discord.scrollIntoViewIfNeeded();
    await discord.click();
    await expect(page.getByTestId("onboarding-help-open-failed")).toHaveText(PLAIN_COPY.help.openFailed(SUPPORT_LINKS.discord));
  });

  test("a failed launch shows its own Get help with the error, and the general entry steps aside", async ({ page }) => {
    await clearState(page);
    await seedMockOverrides(page, { node_care_info: { ...PLATFORM, homeDir: "" }, start_node: { __reject: REFUSAL } });
    await page.goto("/");
    await walkToLaunch(page);
    await page.getByTestId("btn-launch").click();
    const failure = page.getByTestId("launch-error");
    await expect(failure).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId("onboarding-help")).toHaveCount(0);
    await failure.getByTestId("start-failure-help-toggle").click();
    const details = failure.getByTestId("start-failure-help-details");
    await expect(details).toContainText("State: Couldn't start");
    // No home folder here, so the native text is left out.
    await expect(details).toContainText(PLAIN_COPY.share.omitted);
    await expect(details).not.toContainText("entry-fixture-home");
  });
});
