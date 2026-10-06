import { expect, test, type Page } from "@playwright/test";
import { seedMockOverrides, seedOnboarded } from "./helpers";

// The Dashboard's "Network, live" panel against the synthetic chain in
// lib/network-stats/mock-chain.ts: 4 blocks per second and 2 transactions in
// every fourth block, so every 60 s window holds 240 blocks and 120
// transactions whenever the test runs. `window.__ARC_MOCK_NETWORK__` steers
// outages, missing endpoints and the v0.8.11 twin counters.

async function seedNetwork(page: Page, scenario: Record<string, unknown>) {
  await page.addInitScript((value) => {
    (window as unknown as { __ARC_MOCK_NETWORK__?: unknown }).__ARC_MOCK_NETWORK__ = value;
  }, scenario);
}

/** Record which counters ever start moving (data-animating="true"). */
async function watchCounters(page: Page) {
  await page.evaluate(() => {
    const seen: Record<string, boolean> = {};
    (window as unknown as { __movedCounters: Record<string, boolean> }).__movedCounters = seen;
    new MutationObserver((records) => {
      for (const record of records) {
        const element = record.target as HTMLElement;
        if (element.dataset.animating === "true") seen[element.dataset.testid ?? "?"] = true;
      }
    }).observe(document.body, { subtree: true, attributes: true, attributeFilter: ["data-animating"] });
  });
}

const movedCounters = (page: Page) =>
  page.evaluate(
    () => (window as unknown as { __movedCounters: Record<string, boolean> }).__movedCounters,
  );

const panelOf = (page: Page) => page.getByRole("region", { name: "Network, live" });

test.describe("Live network panel", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("measures the network from the validators and states the window", async ({ page }, testInfo) => {
    await page.goto("/");
    const panel = panelOf(page);
    await expect(panel).toBeVisible();
    await expect(panel.getByTestId("live-value-validators")).toHaveText("6 of 6");
    await expect(panel.getByTestId("live-note-validators")).toContainText("run by the Arc team today");
    await expect(panel.getByTestId("live-note-validators")).toContainText("v0.8.10 on all 6");
    await expect(panel.getByTestId("live-counter-height")).toHaveText(/^\d{1,3}(,\d{3})+$/);
    // One decimal for rates, as on arc.ai.
    await expect(panel.getByTestId("live-counter-blocks-per-second")).toHaveText("4.0");
    await expect(panel.getByTestId("live-note-blocks-per-second")).toHaveText("240 in the last minute");
    await expect(panel.getByTestId("live-counter-tps")).toHaveText("2.0");
    await expect(panel.getByTestId("live-note-tps")).toHaveText("120 in the last minute");
    await expect(panel.getByTestId("live-counter-community")).toHaveText("3");
    await expect(panel.getByTestId("live-note-community")).toHaveText("ready for AI work");
    for (const label of ["Validators online", "Chain height", "Blocks a second", "Transactions a second", "Community nodes ready"]) {
      await expect(panel.getByText(label, { exact: true })).toBeVisible();
    }
    // One plain line under the numbers; the detail is behind the explainer.
    await expect(panel.getByTestId("live-network-window")).toHaveText(
      "Counted from the last minute of finalized blocks.",
    );
    await expect(panel.getByTestId("live-network-updated")).toHaveText(/^Updated (just now|\d+ s ago)$/);
    // Fixture data is never called live, and the title dot does not pulse over it.
    await expect(panel).toContainText("Synthetic preview");
    await expect(panel).not.toContainText(/\bLive\b/);
    await expect(panel.getByTestId("live-network-dot")).toHaveAttribute("data-pulsing", "false");
    // Twin figures stay hidden until a validator serves them (v0.8.11).
    await expect(panel.getByTestId("live-twin")).toHaveCount(0);
    // Not contributing compute: no contribution line, no zeros.
    await expect(page.getByTestId("live-contribution")).toHaveCount(0);
    await testInfo.attach("live-network-panel", {
      body: await panel.screenshot(),
      contentType: "image/png",
    });
  });

  test("lights up verified tokens per second and the twin match rate once validators serve them", async ({
    page,
  }, testInfo) => {
    const twinStats = (matched: number, mismatched: number, tokens: number) => ({
      schema: "arc.community.twin-stats.v1",
      since_unix_ms: Date.UTC(2026, 9, 6),
      counters: { groups_matched: matched, groups_mismatched: mismatched },
      throughput_last_hour: {
        window_secs: 3600,
        verified_jobs: 12,
        verified_tokens: tokens,
        verified_tokens_per_second: tokens / 3600,
      },
      twin_match_rate: matched / (matched + mismatched),
    });
    await seedNetwork(page, { twinStats: { 0: twinStats(98, 1, 7_200), 1: twinStats(99, 0, 3_600) } });
    await page.goto("/");
    const panel = panelOf(page);
    const band = panel.getByRole("region", { name: "AI work, checked by twins" });
    await expect(band.getByTestId("live-counter-verified-tokens")).toHaveText("3.0", {
      timeout: 15_000,
    });
    await expect(band.getByTestId("live-note-verified-tokens")).toHaveText(
      "average over the last hour · 2 of 6 coordinators",
    );
    // 197 of 198 is 99.49%: shown as 99.4%, never rounded up.
    await expect(band.getByTestId("live-value-twin-match")).toHaveText("99.4%");
    await expect(band.getByTestId("live-note-twin-match")).toHaveText(
      "197 of 198 twin pairs gave the same answer",
    );
    await testInfo.attach("live-network-panel-with-twin", {
      body: await panel.screenshot(),
      contentType: "image/png",
    });
  });

  test("shows what it could not read instead of a number", async ({ page }) => {
    await seedNetwork(page, { offline: [4, 5], finality: false });
    await page.goto("/");
    const panel = panelOf(page);
    await expect(panel.getByTestId("live-value-validators")).toHaveText("4 of 6");
    await expect(panel.getByTestId("live-validator-NRT")).toHaveAttribute("data-state", "offline");
    await expect(panel.getByTestId("live-validator-LAX")).toHaveAttribute("data-state", "online");
    await expect(panel.getByTestId("live-value-blocks-per-second")).toHaveText("—");
    await expect(panel.getByTestId("live-value-tps")).toHaveText("—");
    await expect(panel.getByTestId("live-network-window")).toContainText(
      "does not serve /finality/latest (HTTP 404)",
    );
    await expect(panel).not.toContainText("0.0 ");
    await expect(panel.getByTestId("live-counter-tps")).toHaveCount(0);
  });

  test("shows this computer's jobs next to the network", async ({ page }) => {
    await seedMockOverrides(page, {
      fetch_worker_status: {
        running: true,
        unavailable: null,
        state: "polling",
        publicName: "node-1a2b3c4d",
        coordinatorsRegistered: 6,
        coordinatorsTotal: 6,
        jobsClaimed: 9,
        jobsCompleted: 7,
        jobsVerified: 6,
        jobsFailed: 1,
        jobsDeclined: 1,
        lastJobCompletedUnixMs: Date.now() - 60_000,
        startedUnixMs: Date.now() - 3_600_000,
        preventSleepDuringJobs: false,
      },
    });
    await page.goto("/");
    const you = panelOf(page).getByTestId("live-contribution");
    await expect(you.getByTestId("live-contribution-completed")).toHaveText("7");
    await expect(you.getByTestId("live-contribution-verified")).toHaveText("6");
    await expect(you).toContainText("since your node started");
  });

  test("keeps measured records apart from live numbers, each with a date and a receipt", async ({
    page,
  }, testInfo) => {
    await page.goto("/");
    const records = page.getByRole("region", { name: "Measured records" });
    await expect(records).toBeVisible();
    await expect(records.getByTestId("measured-records-not-live")).toHaveText("Not live");
    await expect(panelOf(page).getByTestId("measured-records")).toHaveCount(0);
    // A plain headline and one line; a lab figure says so in its headline.
    const lab = records.getByTestId("measured-record-lab-payments-per-second");
    await expect(lab.getByText("169 payments a second, in our test lab", { exact: true })).toBeVisible();
    await expect(lab).toContainText("not on the live network yet");
    await expect(records).not.toContainText("byte-identical");
    await expect(records.getByTestId("measured-record-same-answer-bit-for-bit")).toContainText(
      "the same output",
    );
    await expect(records.getByTestId("measured-record-date")).toHaveText([
      "Measured 6 Oct 2026",
      "Measured 4 Oct 2026",
    ]);
    // The method is behind a collapsed "How we measured".
    const method = lab.getByTestId("measured-record-method");
    await expect(method.getByText(/168\.6 transfers a second/)).toBeHidden();
    await method.getByText("How we measured", { exact: true }).click();
    await expect(method.getByText(/168\.6 transfers a second/)).toBeVisible();
    await expect(method.getByTestId("measured-record-hardware")).toContainText("4-vCPU");
    const receipts = records.getByTestId("measured-record-receipt");
    await expect(receipts).toHaveCount(4);
    const urls = await receipts.evaluateAll((elements) =>
      elements.map((element) => element.getAttribute("data-url")),
    );
    for (const url of urls) {
      expect(url).toMatch(/^https:\/\/github\.com\/FerrumVir\/arc-chain\/actions\/runs\/\d+$/);
    }
    await page.getByTestId("live-network-records-link").click();
    await expect(records).toBeInViewport();
    await testInfo.attach("measured-records", {
      body: await records.screenshot(),
      contentType: "image/png",
    });
  });

  test("a counter moves only when its value changes", async ({ page }) => {
    await page.goto("/");
    const height = panelOf(page).getByTestId("live-counter-height");
    await expect(height).toBeVisible();
    await watchCounters(page);
    const first = await height.getAttribute("data-value");
    // Validators are re-read every 10 s; the synthetic chain grows meanwhile.
    await expect.poll(() => height.getAttribute("data-value"), { timeout: 20_000 }).not.toBe(first);
    await expect.poll(async () => (await movedCounters(page))["live-counter-height"], { timeout: 5_000 }).toBe(true);
    const moved = await movedCounters(page);
    for (const unchanged of ["live-counter-blocks-per-second", "live-counter-tps", "live-counter-community"]) {
      expect(moved[unchanged]).toBeUndefined();
    }
  });

  test("with reduced motion, counters change without moving", async ({ page }) => {
    await page.emulateMedia({ reducedMotion: "reduce" });
    await page.goto("/");
    const height = panelOf(page).getByTestId("live-counter-height");
    await expect(height).toBeVisible();
    await watchCounters(page);
    const first = await height.getAttribute("data-value");
    await expect.poll(() => height.getAttribute("data-value"), { timeout: 20_000 }).not.toBe(first);
    await expect(height).toHaveText(/^\d{1,3}(,\d{3})+$/);
    expect(await movedCounters(page)).toEqual({});
  });

  test("is a labelled region whose explainer opens and closes", async ({ page }) => {
    await page.goto("/");
    const panel = panelOf(page);
    await panel.getByRole("button", { name: "How the live network numbers are measured" }).click();
    const explainer = page.getByRole("dialog", { name: "How these numbers are measured" });
    await expect(explainer).toContainText("arc.network-stats.v1");
    await expect(explainer).toContainText("never a sum");
    await expect(explainer).toContainText("linked to the one before by hash");
    await expect(explainer.getByTestId("live-network-window-detail")).toContainText("60 s ending at block");
    await page.keyboard.press("Escape");
    await expect(explainer).toHaveCount(0);
    await expect(panel.getByRole("list", { name: "Validators" }).getByRole("listitem")).toHaveCount(6);
  });
});
