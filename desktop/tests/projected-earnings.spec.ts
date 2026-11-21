import { expect, test } from "@playwright/test";
import {
  ECONOMICS_404,
  ECONOMICS_NO_BALANCE,
  PROJECTION_404,
  PROJECTION_NO_HISTORY,
  seedMockOverrides,
  seedOnboarded,
} from "./helpers";

const ARC_AMOUNT = /^[\d,]+\.\d{2}$/;

async function gotoEarnings(page: import("@playwright/test").Page) {
  await page.goto("/");
  await page.getByTestId("nav-earnings").click();
  await expect(page.getByTestId("earnings-screen")).toBeVisible();
}

test.describe("Projected earnings - populated", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("renders per-day and per-7-day figures as ARC amounts", async ({
    page,
  }) => {
    await gotoEarnings(page);
    const projection = page.getByTestId("projection-card");
    await expect(projection).toBeVisible();
    await expect(projection).toHaveAttribute("data-projection-state", "numeric");
    await expect(projection).toHaveAttribute(
      "data-source-host",
      "http://140.82.16.112:9090",
    );
    await expect(projection).toHaveAttribute(
      "data-economics-source-host",
      "http://140.82.16.112:9090",
    );
    await expect(projection).toHaveAttribute("data-projected-daily-arc", "108");
    await expect(projection).toHaveAttribute("data-unavailable-reason", "");
    await expect(projection).toHaveAttribute("data-reward-per-attestation", "2.5");
    await expect(projection).toHaveAttribute("data-reward-rate-source", "chain");
    await expect(projection).toHaveAttribute(
      "data-reward-policy-hash",
      "0xpreview-policy",
    );
    await expect(projection).toHaveAttribute("data-community-rewards-enabled", "true");
    await expect(projection).toHaveAttribute("data-issuance-ready-for-worker", "true");
    await expect(page.getByTestId("projection-per-day")).toHaveText(ARC_AMOUNT);
    await expect(page.getByTestId("projection-per-week")).toHaveText(
      ARC_AMOUNT,
    );
  });

  test("states its assumptions: rate, reward, and zero worker bond", async ({
    page,
  }) => {
    await gotoEarnings(page);
    const assumptions = page.getByTestId("projection-assumptions");
    await expect(assumptions).toBeVisible();
    await expect(assumptions).toContainText("mined reward receipts/day");
    await expect(assumptions).toContainText("measured on");
    await expect(assumptions).toContainText("successful mined 0x25");
    await expect(assumptions).toContainText("approval collection");
    await expect(assumptions).toContainText("no worker bond");
    await expect(assumptions).toContainText(/LAX|\d+\.\d+\.\d+\.\d+/);
  });

  test("labels the funding as a testnet treasury transfer, not revenue", async ({
    page,
  }) => {
    await gotoEarnings(page);
    await expect(
      page.getByTestId("projection-funding-label").first(),
    ).toContainText("Promotional testnet subsidy, not demand or revenue");
  });

  test("surfaces the finite treasury as a selected-host ceiling", async ({
    page,
  }) => {
    await gotoEarnings(page);
    const treasury = page.getByTestId("treasury-remaining");
    await expect(treasury).toBeVisible();
    await expect(treasury).toContainText("finite");
    await expect(treasury).toContainText("host-scoped");
    await expect(treasury).toContainText("ARC");
    await expect(treasury).toContainText("Rewards stop when it is empty");
    await expect(treasury).toContainText(
      "unless canonical agreement is independently verified",
    );
    await expect(treasury).not.toContainText("public fleet is divergent");
  });

  test("shows consensus global, worker, and coordinator promotional caps", async ({
    page,
  }) => {
    await gotoEarnings(page);
    const budget = page.getByTestId("projection-reward-budget");
    await expect(budget).toContainText("Promotional cap, epoch 2");
    await expect(budget).toContainText("31 global");
    await expect(budget).toContainText("7 for this worker");
    await expect(budget).toContainText("11 for this coordinator");
    await expect(budget).toContainText("consensus-sealed");
  });

  test("reports the treasury remainder as fundable receipts, not currency", async ({
    page,
  }) => {
    await gotoEarnings(page);
    const treasury = page.getByTestId("treasury-remaining");
    await expect(treasury).toContainText("more");
    await expect(treasury).toContainText("successful mined");
    await expect(treasury).toContainText("0x25");
    await expect(treasury).toContainText("on this host");
  });

  test("does not subtract the unrelated local-attestation bond", async ({
    page,
  }) => {
    await gotoEarnings(page);
    const assumptions = page.getByTestId("projection-assumptions");
    await expect(assumptions).toContainText("no worker bond");
    await expect(assumptions).not.toContainText("bond netted out");
  });

  test("uses only the backend-authoritative daily projection", async ({ page }) => {
    await seedMockOverrides(page, {
      fetch_earnings_projection: {
        ...PROJECTION_NO_HISTORY,
        attestationsPerDay: 999,
        rewardPerAttestation: 2.5,
        projectedDailyArc: 7.25,
        projectedDailyUnavailableReason: null,
      },
    });
    await gotoEarnings(page);
    await expect(page.getByTestId("projection-per-day")).toHaveText("7.25");
    await expect(page.getByTestId("projection-per-week")).toHaveText("50.75");
  });

  test("never reconstructs a forecast the backend withheld", async ({ page }) => {
    await seedMockOverrides(page, {
      fetch_earnings_projection: {
        ...PROJECTION_NO_HISTORY,
        attestationsPerDay: 999,
        rewardPerAttestation: 2.5,
        projectedDailyArc: null,
        projectedDailyUnavailableReason:
          "promotional reward budget is exhausted for this worker",
      },
    });
    await gotoEarnings(page);
    await expect(page.getByTestId("projection-per-day")).toHaveCount(0);
    await expect(page.getByTestId("projection-no-rate")).toContainText(
      "promotional reward budget is exhausted",
    );
  });

  test("two rollout canary receipts stay collecting data instead of annualizing", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      fetch_earnings_projection: {
        ...PROJECTION_NO_HISTORY,
        communityRewardsEnabled: true,
        attestationsTotal: 2,
        rewardPerAttestation: 2.5,
        projectedDailyArc: null,
        projectedDailyUnavailableReason:
          "collecting data: a projection needs at least 3 successful mined reward receipts spanning at least 24 hours, not the initial one or two rollout canaries",
        rateUnavailableReason:
          "collecting data: a projection needs at least 3 successful mined reward receipts spanning at least 24 hours, not the initial one or two rollout canaries",
        observedOverBlocks: 1,
      },
    });
    await gotoEarnings(page);

    const collecting = page.getByTestId("projection-no-rate");
    await expect(collecting).toContainText("collecting data");
    await expect(collecting).toContainText("at least 3");
    await expect(collecting).toContainText("at least 24 hours");
    await expect(page.getByTestId("projection-per-day")).toHaveCount(0);
    await expect(page.getByTestId("projection-per-week")).toHaveCount(0);
    await expect(
      page.getByTestId("projection-funding-label").first(),
    ).toContainText("Promotional testnet subsidy, not demand or revenue");
  });

  test("shows the host's own caveat about how the rate was derived", async ({
    page,
  }) => {
    await gotoEarnings(page);
    await expect(page.getByTestId("projection-rate-caveat")).toContainText(
      "offline for part of that window",
    );
  });

  test("never renders a fiat figure or a currency symbol", async ({ page }) => {
    await gotoEarnings(page);
    await expect(page.getByTestId("projection-card")).toBeVisible();
    const text = (await page.getByTestId("earnings-screen").innerText()) ?? "";
    expect(text).not.toContain("$");
    expect(text).not.toMatch(/\bUSD\b|\bEUR\b|\bGBP\b/);
  });

  test("the Dashboard tile shows a per-day figure but not the weekly one", async ({
    page,
  }) => {
    await page.goto("/");
    const tile = page.getByTestId("dashboard-projection");
    await expect(tile).toBeVisible();
    await expect(tile.getByTestId("projection-per-day")).toHaveText(ARC_AMOUNT);
    await expect(tile.getByTestId("projection-per-week")).toHaveCount(0);
    await expect(
      tile.getByTestId("projection-funding-label"),
    ).toBeVisible();
  });
});

test.describe("Projected earnings - no measured rate", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
    await seedMockOverrides(page, {
      fetch_earnings_projection: PROJECTION_NO_HISTORY,
    });
  });

  test("shows the per-receipt amount instead of inventing a projection", async ({
    page,
  }) => {
    await gotoEarnings(page);
    const projection = page.getByTestId("projection-card");
    await expect(projection).toHaveAttribute("data-projection-state", "no_rate");
    await expect(projection).toHaveAttribute("data-projected-daily-arc", "");
    await expect(projection).toHaveAttribute(
      "data-unavailable-reason",
      PROJECTION_NO_HISTORY.projectedDailyUnavailableReason,
    );
    await expect(page.getByTestId("projection-no-rate")).toBeVisible();
    await expect(page.getByTestId("projection-per-attestation")).toHaveText(
      ARC_AMOUNT,
    );
  });

  test("refuses to project, and says a rate needs history", async ({
    page,
  }) => {
    await gotoEarnings(page);
    const card = page.getByTestId("projection-no-rate");
    await expect(card).toContainText(
      "No per-day figure is shown unless the coordinator explicitly supplies one",
    );
    await expect(card).toContainText("No successful mined reward receipts are retained");
    await expect(page.getByTestId("projection-per-day")).toHaveCount(0);
    await expect(page.getByTestId("projection-per-week")).toHaveCount(0);
  });

  test("still states its assumptions and the funding label", async ({
    page,
  }) => {
    await gotoEarnings(page);
    await expect(page.getByTestId("projection-assumptions")).toContainText(
      "per successful mined 0x25 community-reward receipt",
    );
    await expect(
      page.getByTestId("projection-funding-label").first(),
    ).toContainText("Promotional testnet subsidy, not demand or revenue");
  });
});

test.describe("Projected earnings - endpoint 404s", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("a 404 from /worker/earnings degrades to a stated reason, no numbers", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      fetch_earnings_projection: PROJECTION_404,
    });
    await gotoEarnings(page);
    const reason = page.getByTestId("projection-unavailable");
    await expect(reason).toBeVisible();
    await expect(reason).toContainText("HTTP 404");
    await expect(reason).toContainText("140.82.16.112");
    await expect(reason).toContainText("Not available from this host");
    await expect(page.getByTestId("projection-per-day")).toHaveCount(0);
    await expect(page.getByTestId("projection-per-attestation")).toHaveCount(0);
    const projection = page.getByTestId("projection-card");
    await expect(projection).toHaveAttribute("data-projection-state", "unavailable");
    await expect(projection).toHaveAttribute("data-projected-daily-arc", "");
    await expect(projection).toHaveAttribute(
      "data-unavailable-reason",
      PROJECTION_404.unavailable,
    );
  });

  test("a 404 from /economics/rewards hides the treasury but keeps the projection", async ({
    page,
  }) => {
    await seedMockOverrides(page, { fetch_reward_economics: ECONOMICS_404 });
    await gotoEarnings(page);
    await expect(page.getByTestId("projection-per-day")).toHaveText(ARC_AMOUNT);
    const treasury = page.getByTestId("treasury-unavailable");
    await expect(treasury).toBeVisible();
    await expect(treasury).toContainText("Remaining treasury unknown");
    await expect(treasury).toContainText("HTTP 404");
    await expect(page.getByTestId("treasury-remaining")).toHaveCount(0);
  });

  test("both endpoints 404: a reason for each, and no figure anywhere", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      fetch_earnings_projection: PROJECTION_404,
      fetch_reward_economics: ECONOMICS_404,
    });
    await gotoEarnings(page);
    await expect(page.getByTestId("projection-unavailable")).toBeVisible();
    await expect(page.getByTestId("treasury-unavailable")).toBeVisible();
    await expect(page.getByTestId("projection-figures")).toHaveCount(0);
    const text = await page.getByTestId("projection-card").innerText();
    expect(text).not.toContain("$");
  });

  test("a treasury the host cannot read states the host's reason", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      fetch_reward_economics: ECONOMICS_NO_BALANCE,
    });
    await gotoEarnings(page);
    const treasury = page.getByTestId("treasury-unavailable");
    await expect(treasury).toBeVisible();
    await expect(treasury).toContainText("not present in this host's state");
    await expect(page.getByTestId("treasury-remaining")).toHaveCount(0);
    await expect(page.getByTestId("projection-assumptions")).toContainText(
      "no worker bond",
    );
  });

  test("losing /economics/rewards makes worker bond terms unavailable", async ({
    page,
  }) => {
    await seedMockOverrides(page, { fetch_reward_economics: ECONOMICS_404 });
    await gotoEarnings(page);
    await expect(page.getByTestId("projection-assumptions")).toContainText(
      "worker certificate bond terms could not be read",
    );
  });

  test("an inactive reward rollout never shows a forward earnings figure", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      fetch_earnings_projection: {
        ...PROJECTION_NO_HISTORY,
        communityRewardsEnabled: false,
        attestationsPerDay: 43.2,
      },
    });
    await gotoEarnings(page);
    await expect(page.getByTestId("projection-rollout-inactive")).toContainText(
      "reward settlement is inactive",
    );
    await expect(page.getByTestId("projection-rollout-inactive")).toContainText(
      "approval collection",
    );
    await expect(page.getByTestId("projection-per-day")).toHaveCount(0);
  });

  test("no identity on the device is explained, not shown as zero", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      fetch_earnings_projection: {
        ...PROJECTION_404,
        unavailable:
          "No identity on this device yet, so there is nothing to project.",
      },
    });
    await gotoEarnings(page);
    await expect(page.getByTestId("projection-unavailable")).toContainText(
      "nothing to project",
    );
    await expect(page.getByTestId("projection-per-day")).toHaveCount(0);
  });
});
