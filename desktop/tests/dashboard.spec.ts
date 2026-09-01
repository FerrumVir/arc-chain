import { expect, test } from "@playwright/test";
import { seedOnboarded } from "./helpers";

const TEST_WORKER = "99".repeat(32);

async function useLiveEarningsBody(
  page: import("@playwright/test").Page,
  earningsBody: Record<string, unknown>,
  earningsStatus = 200,
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
    if (path.startsWith("/worker/earnings/")) {
      return route.fulfill({
        status: earningsStatus,
        contentType: "application/json",
        body: JSON.stringify(earningsBody),
      });
    }
    if (path === "/inference/results") return json({ count: 99, results: [] });
    if (path === "/inference/attestations") {
      return json({ count: 0, attestations: [] });
    }
    return route.fulfill({ status: 404, body: "not found" });
  });
}

function candidateEarningsBody(overrides: Record<string, unknown> = {}) {
  return {
    address: `0x${TEST_WORKER}`,
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
        worker: `0x${TEST_WORKER}`,
        block_height: 123_461,
        block_hash: `0x${"10".repeat(32)}`,
        submitted: true,
        included: true,
        confirmed: true,
        success: true,
        receipt_url: `/community/reward_receipt/0x${"aa".repeat(32)}`,
        reward_base: 2_500_000_000,
        reward_arc: 2.5,
        recovery_epoch: 1,
        validator_set_id: 7,
      },
      {
        tx_type: "0x25",
        tx_hash: `0x${"ab".repeat(32)}`,
        job_id: `0x${"02".repeat(32)}`,
        worker: `0x${TEST_WORKER}`,
        block_height: 123_462,
        block_hash: `0x${"11".repeat(32)}`,
        submitted: true,
        included: true,
        confirmed: true,
        success: true,
        receipt_url: `/community/reward_receipt/0x${"ab".repeat(32)}`,
        reward_base: 2_500_000_000,
        reward_arc: 2.5,
        recovery_epoch: 1,
        validator_set_id: 7,
      },
    ],
    estimated_total_arc_note:
      "retained-window gross rewards = successful CommunityInferenceReward receipts × reward_per_attestation_arc",
    source: "scan of this node's in-memory full_transactions map",
    archive_mode: false,
    history_complete_since_recovery: false,
    history_scope: "this node's bounded retained reward-receipt window",
    history_domain:
      "all canonical 0x25 reward domains since the v3 recovery boundary; historical rows retain their own recovery_epoch, validator_set_id, and transaction_domain",
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

function candidateProjectionBody(receiptCount: 0 | 1 | 2 | 3) {
  const body = candidateEarningsBody();
  const rows = [...body.confirmed_receipts];
  rows.push({
    tx_type: "0x25",
    tx_hash: `0x${"ac".repeat(32)}`,
    job_id: `0x${"03".repeat(32)}`,
    worker: `0x${TEST_WORKER}`,
    block_height: 123_463,
    block_hash: `0x${"12".repeat(32)}`,
    submitted: true,
    included: true,
    confirmed: true,
    success: true,
    receipt_url: `/community/reward_receipt/0x${"ac".repeat(32)}`,
    reward_base: 2_500_000_000,
    reward_arc: 2.5,
    recovery_epoch: 1,
    validator_set_id: 7,
  });
  const retained = rows.slice(0, receiptCount);
  const last = retained.at(-1);
  return candidateEarningsBody({
    total_rewards: receiptCount,
    estimated_total_arc: receiptCount * 2.5,
    confirmed_receipt_count: receiptCount,
    confirmed_gross_earnings_base: receiptCount * 2_500_000_000,
    confirmed_gross_earnings_arc: receiptCount * 2.5,
    confirmed_receipts: retained,
    last_reward_block: last?.block_height ?? null,
    last_reward_tx_hash: last?.tx_hash ?? null,
    projected_daily_arc: 7.5,
    projected_daily_unavailable_reason: null,
    observed_window_first_timestamp_ms: 1_700_000_000_000,
    observed_window_last_timestamp_ms: 1_700_086_400_000,
    reward_per_attestation_arc: 2.5,
    attestations_per_day_observed: 3,
  });
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
      address: `0x${TEST_WORKER}`,
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
    await expect(page.getByTestId("earnings-unavailable")).toContainText(
      "Legacy or malformed inference-count arithmetic is not earnings",
    );
    await expect(page.getByTestId("earnings-empty")).toHaveCount(0);
  });

  test("an unavailable earnings RPC is not rendered as a confirmed zero", async ({ page }) => {
    await useLiveEarningsBody(page, { error: "maintenance" }, 503);
    await page.goto("/");
    await expect(page.getByTestId("dashboard-earnings-unavailable")).toContainText(
      "no zero inferred",
    );
    await page.getByTestId("nav-earnings").click();
    await expect(page.getByTestId("earnings-unavailable")).toContainText("HTTP 503");
    await expect(page.getByTestId("earnings-unavailable")).toContainText(
      "No zero balance or zero earnings claim is inferred",
    );
    await expect(page.getByTestId("earnings-empty")).toHaveCount(0);
  });

  test("a valid candidate zero is labelled as a retained-window zero", async ({ page }) => {
    await useLiveEarningsBody(page, candidateEarningsBody({
      total_rewards: 0,
      estimated_total_arc: 0,
      confirmed_receipt_count: 0,
      confirmed_gross_earnings_base: 0,
      confirmed_gross_earnings_arc: 0,
      confirmed_receipts: [],
      last_reward_block: null,
      last_reward_tx_hash: null,
    }));
    await page.goto("/");
    await expect(page.getByTestId("earnings-total")).toHaveText(
      /0\.00\s*ARC confirmed/,
    );
    await page.getByTestId("nav-earnings").click();
    await expect(page.getByTestId("earnings-empty")).toContainText(
      "confirmed zero in the selected host's current retained receipt window",
    );
    await expect(page.getByTestId("earnings-unavailable")).toHaveCount(0);
  });

  test("candidate receipt/readiness shape can render confirmed mined rewards", async ({
    page,
  }) => {
    await useLiveEarningsBody(page, candidateEarningsBody());
    await page.goto("/");
    await expect(page.getByTestId("earnings-total")).toHaveText(
      /5\.00\s*ARC confirmed/,
    );
    await page.getByTestId("nav-earnings").click();
    await expect(page.getByTestId("earnings-retained-window-note")).toContainText(
      "not lifetime earnings",
    );
  });

  for (const receiptCount of [0, 1, 2] as const) {
    test(`a host projection with ${receiptCount} exact receipt(s) fails closed locally`, async ({
      page,
    }) => {
      await useLiveEarningsBody(page, candidateProjectionBody(receiptCount));
      await page.goto("/");
      await page.getByTestId("nav-earnings").click();
      await expect(page.getByTestId("projection-per-day")).toHaveCount(0);
      await expect(page.getByTestId("projection-no-rate")).toContainText(
        "at least 3 successful mined reward receipts",
      );
      await expect(page.getByTestId("projection-no-rate")).toContainText(
        "at least 24 hours",
      );
    });
  }

  test("an omitted receipt count cannot unlock a numeric host projection", async ({
    page,
  }) => {
    const body = candidateProjectionBody(3) as Record<string, unknown>;
    delete body.confirmed_receipt_count;
    await useLiveEarningsBody(page, body);
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();
    await expect(page.getByTestId("projection-per-day")).toHaveCount(0);
    await expect(page.getByTestId("projection-unavailable")).toContainText(
      "did not provide the candidate mined-0x25 receipt",
    );
  });

  test("an optimistic summary count cannot override the exact receipt rows", async ({
    page,
  }) => {
    const body = candidateProjectionBody(2) as Record<string, unknown>;
    body.total_rewards = 3;
    body.confirmed_receipt_count = 3;
    body.confirmed_gross_earnings_base = 7_500_000_000;
    body.confirmed_gross_earnings_arc = 7.5;
    await useLiveEarningsBody(page, body);
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();
    await expect(page.getByTestId("projection-per-day")).toHaveCount(0);
    await expect(page.getByTestId("projection-unavailable")).toContainText(
      "did not provide the candidate mined-0x25 receipt",
    );
  });

  test("three exact receipts over less than 24 hours remain unavailable", async ({
    page,
  }) => {
    await useLiveEarningsBody(page, candidateEarningsBody({
      ...candidateProjectionBody(3),
      observed_window_last_timestamp_ms: 1_700_003_600_000,
    }));
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();
    await expect(page.getByTestId("projection-per-day")).toHaveCount(0);
    await expect(page.getByTestId("projection-no-rate")).toContainText(
      "valid confirmed-receipt window spanning at least 24 hours",
    );
  });

  test("three exact receipts spanning a full day can show the host projection", async ({
    page,
  }) => {
    await useLiveEarningsBody(page, candidateProjectionBody(3));
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();
    await expect(page.getByTestId("projection-per-day")).toHaveText("7.50");
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

  test("matching sums cannot disguise a non-protocol reward amount", async ({ page }) => {
    const body = candidateEarningsBody();
    const rows = body.confirmed_receipts as Array<Record<string, unknown>>;
    rows[0].reward_base = 2_499_999_999;
    rows[0].reward_arc = 2.49;
    body.confirmed_gross_earnings_base = 4_999_999_999;
    body.confirmed_gross_earnings_arc = 4.99;
    await useLiveEarningsBody(page, body);
    await page.goto("/");
    await expect(page.getByTestId("earnings-total")).toContainText("not confirmed");
  });

  test("malformed receipt identities fail closed", async ({ page }) => {
    const body = candidateEarningsBody();
    const rows = body.confirmed_receipts as Array<Record<string, unknown>>;
    rows[0].job_id = "0xdeadbeef";
    await useLiveEarningsBody(page, body);
    await page.goto("/");
    await expect(page.getByTestId("earnings-total")).toContainText("not confirmed");
  });

  test("earnings and projection reject a response bound to another worker", async ({ page }) => {
    const other = "98".repeat(32);
    const body = candidateProjectionBody(3);
    body.address = `0x${other}`;
    for (const row of body.confirmed_receipts) row.worker = `0x${other}`;
    await useLiveEarningsBody(page, body);
    await page.goto("/");
    await expect(page.getByTestId("earnings-total")).toContainText("not confirmed");
    await page.getByTestId("nav-earnings").click();
    await expect(page.getByTestId("projection-per-day")).toHaveCount(0);
    await expect(page.getByTestId("projection-unavailable")).toContainText(
      "not bound to the requested worker address",
    );
  });

  test("earnings reject a receipt attributed to another worker", async ({ page }) => {
    const body = candidateEarningsBody();
    body.confirmed_receipts[0].worker = `0x${"98".repeat(32)}`;
    await useLiveEarningsBody(page, body);
    await page.goto("/");
    await expect(page.getByTestId("earnings-total")).toContainText("not confirmed");
  });

  test("incomplete receipt truth flags fail closed", async ({ page }) => {
    const body = candidateEarningsBody();
    const rows = body.confirmed_receipts as Array<Record<string, unknown>>;
    rows[0].included = false;
    await useLiveEarningsBody(page, body);
    await page.goto("/");
    await expect(page.getByTestId("earnings-total")).toContainText("not confirmed");
  });

  test("archive copy requires the explicit canonical recovery-history scope", async ({ page }) => {
    await useLiveEarningsBody(page, candidateEarningsBody({
      archive_mode: true,
      history_complete_since_recovery: false,
      history_scope: "this node's bounded retained reward-receipt window",
    }));
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();
    await expect(page.getByTestId("earnings-unavailable")).toContainText(
      "did not provide the candidate mined-0x25 retained-receipt contract",
    );
    await expect(page.getByText("Gross mined rewards · canonical v3")).toHaveCount(0);
  });

  test("earnings reject a current-epoch-only history-domain claim", async ({ page }) => {
    await useLiveEarningsBody(page, candidateEarningsBody({
      history_domain: "current recovery epoch only",
    }));
    await page.goto("/");
    await expect(page.getByTestId("earnings-total")).toContainText("not confirmed");
  });

  test("attestations not credited to this user are not shown as earnings", async ({
    page,
  }) => {
    await page.goto("/");
    const feed = page.getByTestId("attestation-feed");
    await expect(feed).toBeVisible();
    // The other worker's raw 0x16 computation remains explicitly unpaid;
    // only the two mined 0x25 rows receive the paid label.
    await expect(
      feed.getByText("COMPUTED · NOT PAYMENT · network", { exact: true }),
    ).toHaveCount(1);
    await expect(
      feed.getByText("COMPUTED · NOT PAYMENT · yours", { exact: true }),
    ).toHaveCount(2);
    await expect(feed.getByText("COMPUTED + PAID", { exact: true })).toHaveCount(2);
    await expect(feed.getByText(/\+2\.50/)).toHaveCount(0);
  });

  test("unknown telemetry renders as 'recent', not fabricated zeros", async ({
    page,
  }) => {
    await page.goto("/");
    const feed = page.getByTestId("attestation-feed");
    // The other worker's flat-shaped activity carries no timestamp. The
    // padding row is excluded, and neither may cause invented telemetry.
    await expect(feed.getByText("recent", { exact: true })).toHaveCount(1);
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
    // Five real activities: three computations plus two distinct mined 0x25
    // reward receipts. The old-seed padding row is excluded.
    await expect(feed.locator(".feed-item")).toHaveCount(5, { timeout: 8000 });
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
