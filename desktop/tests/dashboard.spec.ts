import { expect, test } from "@playwright/test";
import { seedOnboarded } from "./helpers";

async function useLiveEarningsBody(
  page: import("@playwright/test").Page,
  earningsBody: Record<string, unknown>,
) {
  await page.addInitScript(() => {
    (window as unknown as { __ARC_LIVE__: number }).__ARC_LIVE__ = 9090;
  });
  await page.route("http://127.0.0.1:9090/**", (route) => {
    const path = new URL(route.request().url()).pathname;
    const json = (body: unknown) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify(body),
      });
    if (path === "/health") {
      return json({
        status: "ok",
        version: "test",
        peers: 1,
        height: 10,
        dag_round: 10,
        dag_committed: 10,
        uptime_secs: 60,
        validators: 1,
      });
    }
    if (path.startsWith("/worker/earnings/")) return json(earningsBody);
    if (path === "/inference/results") return json({ count: 99, results: [] });
    if (path === "/inference/attestations") {
      return json({ count: 0, attestations: [] });
    }
    return route.fulfill({ status: 404, body: "not found" });
  });
}

function candidateEarningsBody(overrides: Record<string, unknown> = {}) {
  return {
    total_rewards: 2,
    estimated_total_arc: 5,
    confirmed_receipt_count: 2,
    confirmed_gross_earnings_base: 5_000_000_000,
    confirmed_gross_earnings_arc: 5,
    confirmed_receipts: [
      {
        tx_type: "0x25",
        tx_hash: `0x${"aa".repeat(32)}`,
        job_id: `0x${"01".repeat(32)}`,
        block_height: 123_461,
        block_hash: `0x${"10".repeat(32)}`,
        success: true,
        reward_base: 2_500_000_000,
        reward_arc: 2.5,
        recovery_epoch: 1,
        validator_set_id: 7,
      },
      {
        tx_type: "0x25",
        tx_hash: `0x${"ab".repeat(32)}`,
        job_id: `0x${"02".repeat(32)}`,
        block_height: 123_462,
        block_hash: `0x${"11".repeat(32)}`,
        success: true,
        reward_base: 2_500_000_000,
        reward_arc: 2.5,
        recovery_epoch: 1,
        validator_set_id: 7,
      },
    ],
    estimated_total_arc_note:
      "retained-window gross rewards = successful CommunityInferenceReward receipts × reward_per_attestation_arc",
    today_arc: null,
    projected_daily_arc: null,
    projected_daily_unavailable_reason:
      "a single receipt window cannot establish a forecast",
    recovery_epoch: 1,
    validator_set_id: 7,
    community_rewards_v1_enabled: true,
    community_rewards_v1_protocol_active: true,
    community_rewards_v1_approval_collection_ready: true,
    last_reward_block: 123_462,
    last_reward_tx_hash: `0x${"ab".repeat(32)}`,
    ...overrides,
  };
}

test.describe("Dashboard", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("renders the app shell with titlebar, sidebar, and main", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.getByTestId("app-shell")).toBeVisible();
    await expect(page.getByTestId("titlebar")).toBeVisible();
    await expect(page.getByTestId("sidebar")).toBeVisible();
    await expect(page.getByTestId("main")).toBeVisible();
    await expect(page.getByTestId("dashboard")).toBeVisible();
  });

  test("shows only host-confirmed mined rewards as an ARC amount", async ({ page }) => {
    await page.goto("/");
    const earnings = page.getByTestId("earnings-total");
    await expect(earnings).toBeVisible();
    // Assert the FORMAT, not the magnitude.
    //
    // This used to assert `not.toHaveText(/^0\.00/)`, i.e. that earnings are
    // non-zero. That is a property of the mock fixture, not of the app: zero
    // is the correct and expected reading for a fresh identity on the real
    // network, so the assertion would fail against live data while telling us
    // nothing about whether the card renders.
    await expect(earnings).toHaveText(/[\d,]+\.\d{2}\s*ARC confirmed/);
  });

  test("public-v2 count-times-constant earnings fail closed even on HTTP 200", async ({
    page,
  }) => {
    await useLiveEarningsBody(page, {
      total_attestations: 7,
      total_arc: 17.5,
      today_arc: 2.5,
      last_attestation_block: 9,
    });
    await page.goto("/");
    const earnings = page.getByTestId("earnings-total");
    await expect(earnings).toContainText("not confirmed");
    await expect(earnings).not.toContainText("17.50");
    // The old `/inference/results` fallback returned count=99 here. It must
    // not become 247.50 ARC either.
    await expect(earnings).not.toContainText("247.50");
    await page.getByTestId("nav-earnings").click();
    await expect(page.getByTestId("projection-unavailable")).toContainText(
      "Legacy inference-count arithmetic is not projected as earnings",
    );
    await expect(page.getByTestId("earnings-empty")).toBeVisible();
  });

  test("candidate receipt/readiness shape can render confirmed mined rewards", async ({
    page,
  }) => {
    await useLiveEarningsBody(page, candidateEarningsBody());
    await page.goto("/");
    await expect(page.getByTestId("earnings-total")).toHaveText(
      /5\.00\s*ARC confirmed/,
    );
  });

  test("candidate-shaped negative reward totals fail closed", async ({ page }) => {
    await useLiveEarningsBody(
      page,
      candidateEarningsBody({
        estimated_total_arc: -5,
        confirmed_gross_earnings_arc: -5,
      }),
    );
    await page.goto("/");
    await expect(page.getByTestId("earnings-total")).toContainText(
      "not confirmed",
    );
    await expect(page.getByTestId("earnings-total")).not.toContainText("-5.00");
  });

  test("attestations not credited to this user are not shown as earnings", async ({
    page,
  }) => {
    await page.goto("/");
    const feed = page.getByTestId("attestation-feed");
    await expect(feed).toBeVisible();
    // The mock includes two rows that are not the user's: another
    // validator's attestation, and one old-seed padding row. Every raw 0x16
    // row must render as a claim, never as a "+2.50" credit.
    await expect(feed.getByText("network claim", { exact: true })).toHaveCount(2);
    await expect(feed.getByText("your claim", { exact: true })).toHaveCount(2);
    await expect(feed.getByText(/\+2\.50/)).toHaveCount(0);
  });

  test("unknown telemetry renders as 'recent', not fabricated zeros", async ({
    page,
  }) => {
    await page.goto("/");
    const feed = page.getByTestId("attestation-feed");
    // Two rows carry no tokens, latency or timestamp: the flat-shaped
    // attestation and the padding row. Neither may invent them.
    await expect(feed.getByText("recent", { exact: true })).toHaveCount(2);
    await expect(feed.getByText("0 tokens")).toHaveCount(0);
    await expect(feed.getByText("0ms")).toHaveCount(0);
  });

  test("last payout renders a block height as a block, not a date", async ({
    page,
  }) => {
    await page.goto("/");
    const payout = page.getByTestId("last-payout");
    await expect(payout).toBeVisible();
    // Regression guard for the "20770d ago" bug: a block height must never
    // reach the relative-time formatter.
    await expect(payout).not.toHaveText(/\d{3,}d ago/);
  });

  test("start / stop controls toggle node state", async ({ page }) => {
    await page.goto("/");
    // Initial: mock state shows stopped (mockStartedAt null)
    const startBtn = page.getByTestId("btn-start");
    await expect(startBtn).toBeVisible();
    await startBtn.click();
    // After start, stop button appears
    await expect(page.getByTestId("btn-stop")).toBeVisible({ timeout: 4000 });
    // Sidebar status chip should flip to "Running"
    await expect(page.getByTestId("sidebar-status")).toContainText("Running");
    await page.getByTestId("btn-stop").click();
    await expect(page.getByTestId("btn-start")).toBeVisible({ timeout: 4000 });
  });

  test("stats grid has four cards", async ({ page }) => {
    await page.goto("/");
    const grid = page.getByTestId("stat-grid");
    await expect(grid).toBeVisible();
    const tiles = grid.locator(".stat-tile");
    await expect(tiles).toHaveCount(4);
  });

  test("attestation feed renders the mock attestations", async ({ page }) => {
    await page.goto("/");
    const feed = page.getByTestId("attestation-feed");
    await expect(feed).toBeVisible();
    // Four fixtures: two of the user's, one other validator's, one old-seed
    // padding row (kept so the Network screen's filter is demonstrable).
    await expect(feed.locator(".feed-item")).toHaveCount(4, { timeout: 8000 });
  });

  test("shows the node's compute width", async ({ page }) => {
    await page.goto("/");
    await page.getByTestId("btn-start").click();
    // "add two cores" is only demonstrable if the current width is visible.
    await expect(page.getByTestId("compute-width")).toHaveText(
      /\d+|all/,
      { timeout: 8000 },
    );
  });

  test("copy address button shows confirmation", async ({ page, context }) => {
    await context.grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.goto("/");
    const copyBtn = page.getByTestId("btn-copy-address");
    await copyBtn.click();
    await expect(copyBtn).toContainText("Copied");
  });
});
