import { expect, test, type Page } from "@playwright/test";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { openOn } from "./helpers";

// Every Copy details that shares diagnostics follows one policy (lib/support.ts sharePayload): with the home
// folder from node_care_info, the native text scrubbed; while that lookup is pending, after it fails, or when it
// has no home folder, only app-authored context and a line saying the details were left out. Checked on all six
// sharing controls, with the clipboard read back or the manual-copy fallback read, and the API working, failing
// or absent. All data is synthetic.

const HOME = "/Volumes/Data/review-fixture-home"; // a home folder outside the standard user directories
const DIAG =
  `cannot lock ${HOME}/.arc/data-v3/.arc-node.lock (os error 35); contact fixture.person@example.com or ` +
  "@fixture_handle on +1 (415) 555-0134 via 203.0.113.7:9090 on Fixture-MacBook-Pro.local; error code NODE_LOCK_HELD, app 0.8.16";
const LEAKS = [HOME, "review-fixture-home", "fixture.person", "fixture_handle", "555-0134", "203.0.113.7", "Fixture-MacBook-Pro"];
const NATIVE = ["cannot lock", "os error 35", "NODE_LOCK_HELD", ".arc-node.lock"];
const USEFUL = ["~/.arc/data-v3/.arc-node.lock", "(os error 35)", "NODE_LOCK_HELD", "app 0.8.16"];
const DELAY_MS = 5_000;

type Info = "available" | "delayed" | "rejected" | "empty";
type Clip = "ok" | "fail" | "absent";
type Control = "start-panel" | "start-help" | "home" | "home-help" | "crash" | "stop" | "answer";

async function setup(page: Page, control: Control, info: Info, clip: Clip) {
  await openOn(page, ["home", "home-help", "answer"].includes(control) ? "inference" : "dashboard");
  await page.addInitScript(
    ({ control, info, clip, diag, home, delay }) => {
      localStorage.setItem(
        "arc-desktop-state-v1",
        JSON.stringify({
          onboarded: true,
          identity: { address: "99".repeat(32), publicKey: "88".repeat(32), createdAt: 0 },
          config: { role: "worker", modelPath: null, rpcPort: 9090, p2pPort: 9091, autoStart: false, autoUpdate: false, autoInstallUpdates: false, dataDir: "~/.arc/data-v3", workerThreads: null },
          inferenceMode: "coordinator",
        }),
      );
      const running = control === "stop";
      const status = {
        running, pid: running ? 123 : null, health: running ? "live" : "lite", version: "0.8.16", peers: running ? 3 : 0,
        round: 0, committed: 0, height: 0, uptimeSeconds: running ? 60 : 0, address: "99".repeat(32), rpcPort: 9090,
        lastError: control === "crash" ? `arc-node exited unexpectedly: ${diag}` : null,
        coordinatorUrl: running ? null : "https://mock.arc.invalid", chainHost: "https://mock.arc.invalid",
        chainHeight: 123469, chainRound: 9596644, chainBlockAgeSeconds: 0, workerThreads: null, cpuCores: 8,
      };
      const mock: Record<string, unknown> = {
        node_status: status,
        start_node: { __reject: `ARC refused to start the node: ${diag}` },
        stop_node: { __reject: diag },
        run_inference: { __reject: diag },
        run_inference_via_coordinator: { __reject: diag },
        run_inference_via_coordinator_direct: { __reject: diag },
        open_external: { __reject: "external destinations disabled in tests" },
      };
      if (control === "home" || control === "home-help") mock.node_start_block = `ARC refused to start the node: ${diag}`;
      const platform = { endpointConfigured: false, appVersion: "0.8.16", os: "macos", arch: "aarch64", homeDir: info === "empty" ? "" : home };
      if (info === "rejected") {
        Object.defineProperty(mock, "node_care_info", { enumerable: true, get: () => Promise.reject(new Error("synthetic: unavailable")) });
      } else if (info === "delayed") {
        const ready = new Promise((resolve) => setTimeout(() => resolve(platform), delay));
        mock.node_care_info = ready;
      } else {
        mock.node_care_info = platform;
      }
      (window as unknown as { __ARC_MOCK__: unknown }).__ARC_MOCK__ = mock;
      if (clip === "fail") {
        Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: () => Promise.reject(new Error("denied")), readText: () => Promise.resolve("") } });
      } else if (clip === "absent") {
        Object.defineProperty(navigator, "clipboard", { configurable: true, value: undefined });
      }
    },
    { control, info, clip, diag: DIAG, home: HOME, delay: DELAY_MS },
  );
  if (clip === "ok") await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.goto("/");

  if (control === "start-panel" || control === "start-help") {
    await page.getByTestId("btn-start").click();
    await expect(page.getByTestId("mutation-error")).toBeVisible();
    if (control === "start-help") await page.getByTestId("start-failure-help-toggle").click();
  } else if (control === "home-help") {
    await page.getByTestId("node-strip-help-toggle").click();
  } else if (control === "stop") {
    await page.getByTestId("btn-stop").click();
    await expect(page.getByTestId("mutation-error")).toBeVisible();
  } else if (control === "answer") {
    await page.getByTestId("inference-prompt").fill("synthetic failing prompt");
    await page.getByTestId("btn-run-inference").click();
    await expect(page.getByTestId("inference-error")).toBeVisible();
  }
}

const IDS: Record<Control, { button: string; fallback: string; title: string }> = {
  "start-panel": { button: "btn-start-failure-copy", fallback: "start-failure-copy-fallback-text", title: "ARC says:" },
  "start-help": { button: "start-failure-help-copy", fallback: "start-failure-help-details", title: "ARC says:" },
  home: { button: "node-strip-copy", fallback: "node-strip-copy-fallback-text", title: "ARC says:" },
  "home-help": { button: "node-strip-help-copy", fallback: "node-strip-help-details", title: "ARC says:" },
  crash: { button: "crash-banner-copy", fallback: "crash-banner-copy-fallback-text", title: `ARC says: ${PLAIN_COPY.nodeStrip.crashed}` },
  stop: { button: "mutation-error-copy", fallback: "mutation-error-copy-fallback-text", title: `ARC says: ${PLAIN_COPY.dashboard.stopFailed}` },
  answer: { button: "inference-error-copy", fallback: "inference-error-copy-fallback-text", title: "ARC says:" },
};

/** Click Copy details and return what was shared: the clipboard, or the manual-copy fallback when copying failed. */
async function share(page: Page, control: Control, clip: Clip): Promise<string> {
  const ids = IDS[control];
  if (clip === "ok") await page.evaluate(() => navigator.clipboard.writeText(""));
  await page.getByTestId(ids.button).click();
  if (clip === "ok") {
    await expect(page.getByTestId(ids.button)).toHaveText(PLAIN_COPY.actions.copied);
    return page.evaluate(() => navigator.clipboard.readText());
  }
  if (control === "start-help" || control === "home-help") {
    await expect(page.getByTestId(`${ids.button}-failed`)).toContainText(PLAIN_COPY.share.review);
  } else {
    const alert = page.getByTestId(ids.fallback.replace(/-text$/, ""));
    await expect(alert).toContainText(PLAIN_COPY.share.fallback);
    await expect(alert).toContainText(PLAIN_COPY.share.review);
  }
  const fallback = page.getByTestId(ids.fallback);
  expect(await fallback.evaluate((el) => getComputedStyle(el).userSelect)).toBe("text");
  return (await fallback.textContent()) ?? "";
}

function expectNoLeak(text: string) {
  for (const leak of LEAKS) expect(text, `leaked ${leak}`).not.toContain(leak);
}
function expectSummary(text: string, control: Control) {
  expectNoLeak(text);
  for (const native of NATIVE) expect(text, `native text ${native}`).not.toContain(native);
  expect(text).toContain(PLAIN_COPY.share.omitted);
  expect(text).toContain(IDS[control].title);
}
function expectDetailed(text: string) {
  expectNoLeak(text);
  for (const useful of USEFUL) expect(text, `kept ${useful}`).toContain(useful);
  expect(text).not.toContain(PLAIN_COPY.share.omitted);
}

const CASES: [Info, Clip][] = [
  ["available", "ok"], ["available", "absent"],
  ["delayed", "ok"], ["delayed", "fail"],
  ["rejected", "ok"], ["rejected", "fail"],
  ["empty", "ok"], ["empty", "absent"],
];
const CONTROLS: Control[] = ["start-panel", "start-help", "home", "home-help", "crash", "stop", "answer"];

test.describe.configure({ mode: "parallel" });

for (const control of CONTROLS) {
  test.describe(`Copy details privacy: ${control}`, () => {
    for (const [info, clip] of CASES) {
      test(`${info} home information, clipboard ${clip}`, async ({ page }) => {
        test.setTimeout(45_000);
        await setup(page, control, info, clip);
        const first = await share(page, control, clip);
        if (info === "available") {
          expectDetailed(first);
        } else {
          expectSummary(first, control);
        }
        if (info === "delayed") {
          // Once the lookup answers, the next Copy details shares the scrubbed detail; the fallback shows the
          // current payload, never the earlier one, and never the raw text.
          await page.waitForTimeout(DELAY_MS + 500);
          if (clip !== "ok") expectDetailed((await page.getByTestId(IDS[control].fallback).textContent()) ?? "");
          expectDetailed(await share(page, control, clip));
        }
        if (info === "rejected" || info === "empty") {
          // Trying again does not fall back to best-effort sharing.
          await page.waitForTimeout(600);
          expectSummary(await share(page, control, clip), control);
        }
        // No surface claims the shared text is fully anonymised.
        await expect(page.locator("body")).not.toContainText("already removed");
      });
    }
  });
}

test.describe("raw local text is labelled on every surface", () => {
  for (const control of ["start-panel", "home", "crash", "stop", "answer"] as Control[]) {
    test(control, async ({ page }) => {
      await setup(page, control, "available", "ok");
      const toggle = {
        "start-panel": "btn-start-failure-details",
        home: "node-strip-details-toggle",
        crash: "crash-banner-details-toggle",
        stop: "mutation-error-details-toggle",
        answer: "inference-error-details-toggle",
      }[control as Exclude<Control, "start-help" | "home-help">];
      const note = {
        "start-panel": "start-failure-details-note",
        home: "node-strip-details-note",
        crash: "crash-banner-details-note",
        stop: "mutation-error-details-note",
        answer: "inference-error-details-note",
      }[control as Exclude<Control, "start-help" | "home-help">];
      const button = page.getByTestId(toggle);
      if ((await button.getAttribute("aria-expanded")) !== "true") await button.click();
      await expect(page.getByTestId(note)).toHaveText(PLAIN_COPY.share.localOnly);
    });
  }
});
