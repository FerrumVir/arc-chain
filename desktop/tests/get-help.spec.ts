import { expect, test, type Page } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { SUPPORT_LINKS, buildSupportDetails, describeReportState, scrubForSharing } from "../src/lib/support";
import { SHARE_COPY } from "../src/components/ShareCopy";
import { seedMockOverrides, seedOnboarded } from "./helpers";

// "Get help": a person who hits a problem can copy scrubbed details and is
// told exactly where to post them. Reachable from a start failure, Node off and
// No connection (and Settings). No reply is promised anywhere.

const __dirnameHere = path.dirname(fileURLToPath(import.meta.url));
const REFUSED =
  "ARC refused to start the node because chain-data migration is not safely resolved: legacy-data migration preflight failed: refusing protocol-v3 migration through symbolic-link data directory /Users/alice/.arc. Restart ARC after repairing the reported path or permissions.";

/** Onboarded, with the node left off (no auto start). */
async function seedNodeOff(page: Page) {
  await seedOnboarded(page);
  await page.addInitScript(() => {
    const raw = localStorage.getItem("arc-desktop-state-v1");
    if (!raw) return;
    const state = JSON.parse(raw);
    state.config.autoStart = false;
    localStorage.setItem("arc-desktop-state-v1", JSON.stringify(state));
  });
}

const stoppedStatus = (health: "lite" | "offline") => ({
  running: false,
  pid: null,
  health,
  version: "0.8.16",
  peers: 0,
  round: 0,
  committed: 0,
  height: 0,
  uptimeSeconds: 0,
  address: "99".repeat(32),
  rpcPort: 9090,
  lastError: null,
  coordinatorUrl: health === "lite" ? "https://140.82.16.112" : null,
  chainHost: "http://140.82.16.112:9090",
  chainHeight: 123_469,
  chainRound: 9_596_644,
  chainBlockAgeSeconds: 400,
  workerThreads: null,
  cpuCores: 8,
});

test.describe("Get help: pure logic", () => {
  test("support details carry versions, state and the scrubbed native text only", () => {
    const text = buildSupportDetails({
      context: "start-failure",
      appVersion: "0.8.16",
      nodeVersion: "0.8.16",
      os: "macos",
      arch: "aarch64",
      title: PLAIN_COPY.startFailure.needsRepair.title,
      error: `${REFUSED} contact me at someone@example.com via https://10.0.0.7:9090`,
      privacy: { state: "ready", homeDir: "/Users/alice" },
    });
    expect(text).toContain("App: 0.8.16 · Node: 0.8.16 · System: macos aarch64");
    expect(text).toContain("State: Couldn't start");
    expect(text).toContain(`ARC says: ${PLAIN_COPY.startFailure.needsRepair.title}`);
    expect(text).toContain("symbolic-link data directory ~/.arc");
    for (const leak of ["alice", "someone@example.com", "10.0.0.7"]) expect(text).not.toContain(leak);
    expect(
      buildSupportDetails({ context: "off", appVersion: null, nodeVersion: "", os: null, arch: null, privacy: { state: "pending" } }),
    ).toContain("App: unknown · Node: unknown · System: unknown unknown");
    // Without privacy context the native text is left out, and versions that fail the shape check read unknown.
    const withheld = buildSupportDetails({
      context: "start-failure", appVersion: "0.8.16", nodeVersion: "0.8.16 /Users/alice", os: "macos", arch: "aarch64",
      title: PLAIN_COPY.startFailure.needsRepair.title, error: REFUSED, privacy: { state: "unavailable" },
    });
    expect(withheld).toContain("App: 0.8.16 · Node: unknown · System: macos aarch64");
    expect(withheld).toContain(PLAIN_COPY.share.omitted);
    expect(withheld).not.toContain("symbolic-link");
    expect(withheld).not.toContain("alice");
  });

  test("the report line says whether a report actually left this computer", () => {
    const base = { ready: true, enabled: true, endpointConfigured: true, recent: [] as never[] };
    expect(describeReportState({ ...base, ready: false }).kind).toBe("loading");
    expect(describeReportState({ ...base, enabled: false }).text).toBe(PLAIN_COPY.help.report.off);
    expect(describeReportState({ ...base, endpointConfigured: false }).text).toBe(PLAIN_COPY.help.report.notConfigured);
    expect(describeReportState(base).text).toBe(PLAIN_COPY.help.report.none);
    const at = Date.UTC(2026, 10, 4, 9, 30);
    const heartbeat = { kind: "heartbeat" as const, body: "{}", at: at + 1, delivery: "sent" as const };
    expect(
      describeReportState({ ...base, recent: [heartbeat, { kind: "report", body: "{}", at, delivery: "queued" }] }),
    ).toEqual({ kind: "queued", text: PLAIN_COPY.help.report.queued("2026-11-04 09:30 UTC") });
    expect(describeReportState({ ...base, recent: [{ kind: "report", body: "{}", at, delivery: "sent" }] }).text).toBe(
      PLAIN_COPY.help.report.sent("2026-11-04 09:30 UTC"),
    );
  });

  test("the report line never reads as a delivered help request", () => {
    for (const line of [PLAIN_COPY.help.report.sent("2026-11-04 09:30 UTC"), PLAIN_COPY.help.report.queued("2026-11-04 09:30 UTC")]) {
      expect(line).toContain("not a help request");
      expect(line).toContain("asks nobody to reply");
    }
    expect(PLAIN_COPY.help.report.sent("x")).toContain("doesn't mean a person has read it");
  });

  test("no help copy promises a reply or names a reader", () => {
    const all = JSON.stringify(PLAIN_COPY.help).toLowerCase();
    for (const promise of ["we'll reply", "we will reply", "someone will", "the team will", "within 24", "within 48", "guarantee", "reads every"]) {
      expect(all).not.toContain(promise);
    }
  });

  test("the destinations are the README's Community and help links (#222)", () => {
    const readme = fs.readFileSync(path.resolve(__dirnameHere, "../../README.md"), "utf8");
    expect(readme).toContain("## Community and help");
    for (const url of [SUPPORT_LINKS.discord, SUPPORT_LINKS.telegram, SUPPORT_LINKS.issues]) expect(readme).toContain(url);
    expect(SUPPORT_LINKS.docs).toBe("https://github.com/FerrumVir/arc-chain#community-and-help");
  });
});

test.describe("Get help: in the app", () => {
  test("a start failure offers Get help with copied, scrubbed details", async ({ page }) => {
    await seedNodeOff(page);
    await seedMockOverrides(page, { start_node: { __reject: REFUSED } });
    await page.goto("/");
    await page.getByTestId("btn-start").click();
    const failure = page.getByTestId("mutation-error");
    await expect(failure).toBeVisible();

    const help = failure.getByTestId("start-failure-help");
    await expect(help).toHaveAttribute("data-context", "start-failure");
    await help.getByTestId("start-failure-help-toggle").click();
    const details = help.getByTestId("start-failure-help-details");
    await expect(details).toContainText("State: Couldn't start");
    await expect(details).toContainText(PLAIN_COPY.startFailure.needsRepair.title);
    await expect(details).toContainText("~/.arc");
    await expect(details).not.toContainText("alice");

    await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
    await help.getByTestId("start-failure-help-copy").click();
    await expect(help.getByTestId("start-failure-help-copy")).toHaveText(PLAIN_COPY.actions.copied);
    const copied = await page.evaluate(() => navigator.clipboard.readText());
    expect(copied).toBe(await details.textContent());

    await expect(help).toContainText(PLAIN_COPY.help.stepPost);
    await expect(help).toContainText(PLAIN_COPY.help.safety);
    // The browser preview has no report address, and the line says so instead of implying a report went out.
    await expect(help.getByTestId("start-failure-help-report")).toHaveText(PLAIN_COPY.help.report.notConfigured);
  });

  test("Node off shows Get help on the Dashboard", async ({ page }) => {
    await seedNodeOff(page);
    await seedMockOverrides(page, { node_status: stoppedStatus("lite") });
    await page.goto("/");
    await expect(page.getByTestId("dashboard-subtitle")).toHaveText(PLAIN_COPY.dashboard.subtitleOff);
    const help = page.getByTestId("dashboard-help");
    await expect(help).toHaveAttribute("data-context", "off");
    await help.getByTestId("dashboard-help-toggle").click();
    await expect(help.getByTestId("dashboard-help-details")).toContainText("State: Node off");
    await expect(help.getByTestId("dashboard-help-details")).toContainText("Node: 0.8.16");
  });

  test("No connection shows Get help, and a failed copy says how to copy by hand", async ({ page }) => {
    await seedNodeOff(page);
    await seedMockOverrides(page, { node_status: stoppedStatus("offline") });
    await page.addInitScript(() => {
      Object.defineProperty(navigator, "clipboard", {
        configurable: true,
        value: { writeText: () => Promise.reject(new Error("denied")), readText: () => Promise.resolve("") },
      });
    });
    await page.goto("/");
    await expect(page.getByTestId("dashboard-subtitle")).toHaveText(PLAIN_COPY.connection.unreachable);
    const help = page.getByTestId("dashboard-help");
    await expect(help).toHaveAttribute("data-context", "unreachable");
    await help.getByTestId("dashboard-help-toggle").click();
    await expect(help.getByTestId("dashboard-help-details")).toContainText("State: No connection");
    await help.getByTestId("dashboard-help-copy").click();
    await expect(help.getByTestId("dashboard-help-copy-failed")).toHaveText(`${PLAIN_COPY.help.copyFailedFallback} ${PLAIN_COPY.share.review}`);
    await expect(help.getByTestId("dashboard-help-copy")).toHaveText(PLAIN_COPY.actions.copyDetails);
  });

  test("a link that cannot open says so and shows the address", async ({ page }) => {
    await seedNodeOff(page);
    await seedMockOverrides(page, { node_status: stoppedStatus("offline"), open_external: { __reject: "no browser" } });
    await page.goto("/");
    const help = page.getByTestId("dashboard-help");
    await help.getByTestId("dashboard-help-toggle").click();
    await help.getByTestId("dashboard-help-telegram").click();
    await expect(help.getByTestId("dashboard-help-open-failed")).toHaveText(PLAIN_COPY.help.openFailed(SUPPORT_LINKS.telegram));
    await help.getByTestId("dashboard-help-discord").click();
    await expect(help.getByTestId("dashboard-help-open-failed")).toHaveText(PLAIN_COPY.help.openFailed(SUPPORT_LINKS.discord));
  });

  test("a running, connected node shows no Get help on the Dashboard; Settings always has it", async ({ page }) => {
    await seedNodeOff(page);
    await page.goto("/");
    await expect(page.getByTestId("dashboard-help")).toBeVisible();
    await page.getByTestId("btn-start").click();
    await expect(page.getByTestId("dashboard-subtitle")).toHaveText(PLAIN_COPY.nodeStrip.runningConnected, { timeout: 15_000 });
    await expect(page.getByTestId("dashboard-help")).toHaveCount(0);

    await page.getByTestId("nav-settings").click();
    const help = page.getByTestId("settings-help");
    await help.getByTestId("settings-help-toggle").click();
    await expect(help.getByTestId("settings-help-details")).toContainText("State: Opened from Settings");
    // Turning anonymous reports off changes the line to say no report is sent.
    await page.getByTestId("toggle-node-care").uncheck();
    await expect(help.getByTestId("settings-help-report")).toHaveText(PLAIN_COPY.help.report.off);
  });
});

// Synthetic sensitive diagnostics: every kind of personal detail the sharing scrubber must remove, next to the
// parts that make an error useful (wording, codes, versions, paths under the home folder, times, heights).
const SENSITIVE =
  "ARC refused to start the node: cannot lock /Volumes/Data/synthetic-home/.arc/data-v3/.arc-node.lock (os error 35). " +
  "Also seen in /Users/synthetic/.arc, /home/synthetic/.arc, C:\\Users\\Synthetic\\.arc and C:/Users/Synthetic/.arc and /root/.arc. " +
  "Contact synthetic.person@example.com or @synthetic_handle on +1 (415) 555-0134 / +44 20 7946 0958. " +
  "Coordinator https://user:pw@coordinator.example:9443/v1 via 203.0.113.7:9090 and [2001:db8::7]:9090 on Synthetic-MacBook-Pro.local. " +
  "Error code NODE_LOCK_HELD at 12:30:45 on 2026-11-04, block 1234567890, app 0.8.16, npm @types/node.";
const LEAKS = [
  "synthetic-home", "/Users/synthetic", "/home/synthetic", "Synthetic\\.arc", "Users/Synthetic", "/root/",
  "synthetic.person", "example.com", "synthetic_handle", "555-0134", "7946", "user:pw", "coordinator.example",
  "203.0.113.7", "2001:db8", "Synthetic-MacBook-Pro",
];
const KEPT = [
  "ARC refused to start the node", "~/.arc/data-v3/.arc-node.lock", "(os error 35)", "NODE_LOCK_HELD", "12:30:45",
  "2026-11-04", "block 1234567890", "app 0.8.16", "@types/node", "https://[redacted]",
];
const PLATFORM = { endpointConfigured: false, appVersion: "0.8.16", os: "macos", arch: "aarch64", homeDir: "/Volumes/Data/synthetic-home" };

test.describe("Sharing scrub", () => {
  test("removes home folders on every platform, emails, hosts, handles and phones, and keeps what helps", () => {
    const out = scrubForSharing(SENSITIVE, PLATFORM.homeDir);
    for (const leak of LEAKS) expect(out, leak).not.toContain(leak);
    for (const keep of KEPT) expect(out, keep).toContain(keep);
    // Without the native home folder, the generic user-folder rules still apply.
    const generic = scrubForSharing("open /Users/synthetic/.arc and /home/synthetic/x and C:\\Users\\Synthetic\\y");
    expect(generic).toBe("open ~/.arc and ~/x and ~\\y");
  });
});

test.describe("Start failure: both Copy details actions share scrubbed text", () => {
  test("the panel's Copy details and Get help's Copy details both put scrubbed text on the clipboard", async ({ page }) => {
    await seedNodeOff(page);
    await seedMockOverrides(page, { start_node: { __reject: SENSITIVE }, node_care_info: PLATFORM });
    await page.goto("/");
    await page.getByTestId("btn-start").click();
    const failure = page.getByTestId("mutation-error");
    await expect(failure).toBeVisible();
    await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);

    // The exact native text stays on screen for repair, labelled as local-only.
    const toggle = failure.getByTestId("btn-start-failure-details");
    if ((await toggle.getAttribute("aria-expanded")) !== "true") await toggle.click();
    await expect(failure.getByTestId("start-failure-details")).toContainText("/Users/synthetic/.arc");
    await expect(failure.getByTestId("start-failure-details-note")).toHaveText(SHARE_COPY.localOnly);

    // 1. The panel's own Copy details (the one that copied the raw text before).
    await failure.getByTestId("btn-start-failure-copy").click();
    await expect(failure.getByTestId("btn-start-failure-copy")).toHaveText(PLAIN_COPY.actions.copied);
    const panelCopy = await page.evaluate(() => navigator.clipboard.readText());
    for (const leak of LEAKS) expect(panelCopy, leak).not.toContain(leak);
    for (const keep of KEPT) expect(panelCopy, keep).toContain(keep);

    // 2. Get help's Copy details.
    await page.evaluate(() => navigator.clipboard.writeText(""));
    const help = failure.getByTestId("start-failure-help");
    await help.getByTestId("start-failure-help-toggle").click();
    await help.getByTestId("start-failure-help-copy").click();
    await expect(help.getByTestId("start-failure-help-copy")).toHaveText(PLAIN_COPY.actions.copied);
    const helpCopy = await page.evaluate(() => navigator.clipboard.readText());
    for (const leak of LEAKS) expect(helpCopy, leak).not.toContain(leak);
    expect(helpCopy).toContain("App: 0.8.16 · Node: 0.8.16 · System: macos aarch64");
    expect(helpCopy).toContain("~/.arc/data-v3/.arc-node.lock");
  });

  test("when the clipboard fails, both actions point to scrubbed text to copy by hand", async ({ page }) => {
    await seedNodeOff(page);
    await seedMockOverrides(page, { start_node: { __reject: SENSITIVE }, node_care_info: PLATFORM });
    await page.addInitScript(() => {
      Object.defineProperty(navigator, "clipboard", {
        configurable: true,
        value: { writeText: () => Promise.reject(new Error("denied")), readText: () => Promise.resolve("") },
      });
    });
    await page.goto("/");
    await page.getByTestId("btn-start").click();
    const failure = page.getByTestId("mutation-error");

    await failure.getByTestId("btn-start-failure-copy").click();
    await expect(failure.getByTestId("btn-start-failure-copy")).toHaveText(PLAIN_COPY.actions.copyFailed);
    const fallback = failure.getByTestId("start-failure-copy-fallback");
    await expect(fallback).toContainText(SHARE_COPY.fallback);
    const manual = (await fallback.getByTestId("start-failure-copy-fallback-text").textContent()) ?? "";
    for (const leak of LEAKS) expect(manual, leak).not.toContain(leak);
    for (const keep of KEPT) expect(manual, keep).toContain(keep);
    // It stays until the next attempt instead of vanishing while someone selects it.
    await page.waitForTimeout(3_000);
    await expect(fallback).toBeVisible();
    expect(await fallback.evaluate((el) => getComputedStyle(el.querySelector("pre")!).userSelect)).toBe("text");

    const help = failure.getByTestId("start-failure-help");
    await help.getByTestId("start-failure-help-toggle").click();
    await help.getByTestId("start-failure-help-copy").click();
    await expect(help.getByTestId("start-failure-help-copy-failed")).toHaveText(`${PLAIN_COPY.help.copyFailedFallback} ${PLAIN_COPY.share.review}`);
    const helpManual = (await help.getByTestId("start-failure-help-details").textContent()) ?? "";
    for (const leak of LEAKS) expect(helpManual, leak).not.toContain(leak);
  });
});
