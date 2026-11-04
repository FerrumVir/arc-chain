import { expect, test, type Page } from "@playwright/test";
import { clearState, openOn, seedOnboarded } from "./helpers";
import { installSetupRecoveryFixture } from "./fixtures/setup-recovery";
import { PLAIN_COPY } from "../src/lib/plain-copy";

type Control = { calls: () => number; pending: () => number; writes: () => string[]; mode: (v: string) => void; recover: () => void; reject: () => void };
declare global { interface Window { __ARC_SETUP_RECOVERY__: Control } }
const calls = (page: Page) => page.evaluate(() => window.__ARC_SETUP_RECOVERY__.calls());
const writes = (page: Page) => page.evaluate(() => window.__ARC_SETUP_RECOVERY__.writes());

async function setup(page: Page, options: Parameters<typeof installSetupRecoveryFixture>[0] = {}) {
  if (options.diagnostic) {
    await seedOnboarded(page); await openOn(page, "inference");
    await page.addInitScript(() => {
      const state = JSON.parse(localStorage.getItem("arc-desktop-state-v1")!);
      state.config.autoStart = false;
      localStorage.setItem("arc-desktop-state-v1", JSON.stringify(state));
    });
  } else await clearState(page);
  await page.addInitScript(installSetupRecoveryFixture, options);
  await page.route("**/*", (route) => new URL(route.request().url()).hostname === "localhost" ? route.continue() : route.abort());
  await page.goto("/");
  const id = options.diagnostic ? "node-strip-help" : "onboarding-help";
  await page.getByTestId(`${id}-toggle`).click();
  await expect(page.getByTestId(`${id}-details`)).toContainText("App: unknown");
  // The parent inherits one query retry; let it finish for the baseline reproduction too.
  if (!options.pending) await page.waitForTimeout(1_500);
  return { details: page.getByTestId(`${id}-details`), copy: page.getByTestId(`${id}-copy`), failed: page.getByTestId(`${id}-copy-failed`) };
}

test("setup without an error recovers on Copy details while mounted, copying the current safe summary", async ({ page }) => {
  const ui = await setup(page);
  const safe = await ui.details.textContent();
  expect(safe).not.toContain("Details:");
  await page.getByTestId("step-welcome").evaluate((el) => el.setAttribute("data-mounted-proof", "kept"));
  const initial = await calls(page);
  await page.evaluate(() => window.__ARC_SETUP_RECOVERY__.mode("pending"));
  await ui.copy.click();
  await expect.poll(() => calls(page)).toBe(initial + 1);
  await expect.poll(() => writes(page)).toEqual([safe]);
  // Same-turn clicks exercise stale observer state as well as ordinary pending clicks.
  await ui.copy.evaluate((el: HTMLButtonElement) => { el.click(); el.click(); });
  await expect.poll(() => writes(page)).toEqual([safe, safe, safe]);
  expect(await calls(page)).toBe(initial + 1);
  await expect(ui.details).toHaveText(safe!);
  await page.evaluate(() => window.__ARC_SETUP_RECOVERY__.recover());
  await expect(ui.details).toContainText("App: 0.8.16");
  expect(await writes(page), "recovery never overwrites the current clipboard").toEqual([safe, safe, safe]);
  await ui.copy.click();
  await expect.poll(() => writes(page)).toEqual([safe, safe, safe, await ui.details.textContent()]);
  expect(await calls(page), "ready copies do not fetch again").toBe(initial + 1);
  await expect(page.getByTestId("step-welcome")).toHaveAttribute("data-mounted-proof", "kept");
  await expect(ui.details).not.toContainText("setup-recovery-fixture");
});

test("an initially pending lookup is shared, with no duplicate request from copies", async ({ page }) => {
  const ui = await setup(page, { pending: true });
  const initial = await calls(page); // Node Care runtime + the shared privacy query are separate consumers.
  expect(initial).toBe(2);
  const safe = await ui.details.textContent();
  await ui.copy.evaluate((el: HTMLButtonElement) => { el.click(); el.click(); });
  await expect.poll(() => writes(page)).toEqual([safe, safe]);
  expect(await calls(page)).toBe(initial);
  await page.evaluate(() => window.__ARC_SETUP_RECOVERY__.recover());
  await expect(ui.details).toContainText("System: macos aarch64");
  expect(await writes(page)).toEqual([safe, safe]);
});

test("rejected and empty recovery attempts are bounded; another copy can recover later", async ({ page }) => {
  const ui = await setup(page);
  const initial = await calls(page);
  expect(initial, "one runtime lookup and one privacy lookup, no automatic retry").toBe(2);
  await ui.copy.click(); // explicit retry also rejects
  await expect.poll(() => calls(page)).toBe(initial + 1);
  await page.waitForTimeout(1_500);
  expect(await calls(page), "no automatic retry after rejection").toBe(initial + 1);
  await expect(ui.details).toContainText("App: unknown");
  await page.evaluate(() => window.__ARC_SETUP_RECOVERY__.mode("empty"));
  await ui.copy.click();
  await expect(ui.details).toContainText("App: 0.8.16");
  expect(await calls(page)).toBe(initial + 2);
  await page.evaluate(() => window.__ARC_SETUP_RECOVERY__.mode("pending"));
  await ui.copy.evaluate((el: HTMLButtonElement) => { el.click(); el.click(); });
  await expect.poll(() => calls(page)).toBe(initial + 3);
  await page.waitForTimeout(200);
  expect(await calls(page), "cached empty data must not cancel/restart an in-flight retry").toBe(initial + 3);
  await page.evaluate(() => window.__ARC_SETUP_RECOVERY__.recover());
  await ui.copy.click();
  expect(await calls(page)).toBe(initial + 3);
});

for (const clipboard of ["fail", "absent"] as const) {
  test(`setup metadata recovery with clipboard ${clipboard} retains the selectable fallback`, async ({ page }) => {
    const ui = await setup(page, { clipboard });
    const safe = await ui.details.textContent();
    const initial = await calls(page);
    await page.evaluate(() => window.__ARC_SETUP_RECOVERY__.mode("pending"));
    await ui.copy.click();
    await expect(ui.failed).toContainText(PLAIN_COPY.share.review);
    await expect.poll(() => calls(page)).toBe(initial + 1);
    await expect(ui.details).toHaveText(safe!);
    expect(await ui.details.evaluate((el) => getComputedStyle(el).userSelect)).toBe("text");
    expect(await writes(page)).toEqual(clipboard === "fail" ? [safe] : []);
    await page.evaluate(() => window.__ARC_SETUP_RECOVERY__.recover());
    await expect(ui.details).toContainText("App: 0.8.16");
    await ui.copy.click();
    await expect(ui.failed).toBeVisible();
    expect(await calls(page)).toBe(initial + 1);
    await expect(ui.details).not.toContainText("setup-recovery-fixture");
  });
}

test("error-bearing Get help keeps native details omitted during recovery, then scrubs them", async ({ page }) => {
  const ui = await setup(page, { diagnostic: true });
  const safe = await ui.details.textContent();
  expect(safe).toContain(PLAIN_COPY.share.omitted);
  expect(safe).not.toContain("cannot lock");
  await page.evaluate(() => window.__ARC_SETUP_RECOVERY__.mode("pending"));
  await ui.copy.click();
  await expect.poll(() => writes(page)).toEqual([safe]);
  await page.evaluate(() => window.__ARC_SETUP_RECOVERY__.recover());
  await expect(ui.details).toContainText("~/.arc/data-v3/.arc-node.lock");
  await expect(ui.details).not.toContainText("setup-recovery-fixture");
  expect(await writes(page)).toEqual([safe]);
  await ui.copy.click();
  await expect.poll(() => writes(page)).toEqual([safe, await ui.details.textContent()]);
});
