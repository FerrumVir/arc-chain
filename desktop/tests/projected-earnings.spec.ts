// Projected earnings — the app's only forward-looking number.
//
// Every assertion here checks FORMAT, PRESENCE or COPY. None checks a
// magnitude. A spec that required earnings to be non-zero could never pass
// against a real network where a node has served nothing, and a spec that
// pinned an exact figure would break the first time the mock's fixtures moved.
// What must never drift is the honesty of the degraded states, so those get the
// most coverage.

import { expect, test } from "@playwright/test";
import {
  ECONOMICS_404,
  ECONOMICS_NO_BALANCE,
  PROJECTION_404,
  PROJECTION_NO_HISTORY,
  seedMockOverrides,
  seedOnboarded,
} from "./helpers";

/** An ARC amount: thousands separators optional, exactly two decimals. */
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
    await expect(page.getByTestId("projection-card")).toBeVisible();
    // Format only. Whether the number is large, small or zero is the
    // network's business, not this spec's.
    await expect(page.getByTestId("projection-per-day")).toHaveText(ARC_AMOUNT);
    await expect(page.getByTestId("projection-per-week")).toHaveText(
      ARC_AMOUNT,
    );
  });

  test("states its assumptions: rate, reward per attestation, bond", async ({
    page,
  }) => {
    await gotoEarnings(page);
    const assumptions = page.getByTestId("projection-assumptions");
    await expect(assumptions).toBeVisible();
    // The three things a reader needs in order to judge the projection.
    await expect(assumptions).toContainText("attestations/day");
    await expect(assumptions).toContainText("measured on");
    await expect(assumptions).toContainText("per settled attestation");
    await expect(assumptions).toContainText("bond");
    // The rate must be attributed to a host, because the seeds are separate
    // chains and a rate from one says nothing about another.
    await expect(assumptions).toContainText(/LAX|\d+\.\d+\.\d+\.\d+/);
  });

  test("labels the funding as a testnet treasury transfer, not revenue", async ({
    page,
  }) => {
    await gotoEarnings(page);
    await expect(
      page.getByTestId("projection-funding-label").first(),
    ).toContainText("Testnet treasury transfer, not revenue");
  });

  test("surfaces the finite treasury as a network-wide ceiling", async ({
    page,
  }) => {
    await gotoEarnings(page);
    const treasury = page.getByTestId("treasury-remaining");
    await expect(treasury).toBeVisible();
    await expect(treasury).toContainText("finite");
    await expect(treasury).toContainText("network-wide");
    await expect(treasury).toContainText("ARC");
    // The anti-"unlimited payout" sentence.
    await expect(treasury).toContainText("Rewards stop when it is empty");
  });

  test("reports the treasury remainder as attestations, not as currency", async ({
    page,
  }) => {
    await gotoEarnings(page);
    const treasury = page.getByTestId("treasury-remaining");
    // `rewards_remaining` on the wire is a COUNT of attestations the treasury
    // can still pay for, NOT an ARC amount. Rendering it as currency would be
    // wrong by nine orders of magnitude and wrong in kind, so the copy has to
    // name the unit it actually is.
    await expect(treasury).toContainText("more");
    await expect(treasury).toContainText("settled attestations");
    await expect(treasury).toContainText("across the whole network");
  });

  test("attributes the bond-refund claim to the host and stays conservative", async ({
    page,
  }) => {
    await gotoEarnings(page);
    const assumptions = page.getByTestId("projection-assumptions");
    // The repo's own notes say the apply path locks the bond with no release,
    // while the endpoint reports a refund. The UI must not pick a side: it
    // attributes the claim and projects on the locked figure.
    await expect(assumptions).toContainText("this host reports");
    await expect(assumptions).toContainText("challenge period");
    await expect(assumptions).toContainText(
      "treats the bond as still locked",
    );
  });

  test("shows the host's own caveat about how the rate was derived", async ({
    page,
  }) => {
    await gotoEarnings(page);
    // Verbatim from the host - it knows its method, this build does not.
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
    // Compact variant: one figure, not two.
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

  test("shows what one attestation pays instead of a projection", async ({
    page,
  }) => {
    await gotoEarnings(page);
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
      "No per-day figure is shown: a rate has to be measured",
    );
    await expect(card).toContainText("no history to measure a rate from");
    // The whole point: zero attestations must not become a zero rate and a
    // zero projection. No per-day figure may be rendered at all.
    await expect(page.getByTestId("projection-per-day")).toHaveCount(0);
    await expect(page.getByTestId("projection-per-week")).toHaveCount(0);
  });

  test("still states its assumptions and the funding label", async ({
    page,
  }) => {
    await gotoEarnings(page);
    await expect(page.getByTestId("projection-assumptions")).toContainText(
      "per settled attestation",
    );
    await expect(
      page.getByTestId("projection-funding-label").first(),
    ).toContainText("Testnet treasury transfer, not revenue");
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
    // Names the host and what was observed - never speculates why.
    await expect(reason).toContainText("HTTP 404");
    await expect(reason).toContainText("140.82.16.112");
    await expect(reason).toContainText("Not available from this host");
    // No figure is invented to fill the gap.
    await expect(page.getByTestId("projection-per-day")).toHaveCount(0);
    await expect(page.getByTestId("projection-per-attestation")).toHaveCount(0);
  });

  test("a 404 from /economics/rewards hides the treasury but keeps the projection", async ({
    page,
  }) => {
    await seedMockOverrides(page, { fetch_reward_economics: ECONOMICS_404 });
    await gotoEarnings(page);
    // The two reads are independent: losing the treasury figure must not take
    // down a projection whose own inputs are intact.
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
    // The endpoint exists and answered - it just could not read the account.
    // That is a different state from a 404 and carries its own reason.
    const treasury = page.getByTestId("treasury-unavailable");
    await expect(treasury).toBeVisible();
    await expect(treasury).toContainText("not present in this host's state");
    await expect(page.getByTestId("treasury-remaining")).toHaveCount(0);
    // The bond IS known here, so it must still be netted out.
    await expect(page.getByTestId("projection-assumptions")).toContainText(
      "bond netted out",
    );
  });

  test("losing /economics/rewards also stops netting out a bond, and says so", async ({
    page,
  }) => {
    await seedMockOverrides(page, { fetch_reward_economics: ECONOMICS_404 });
    await gotoEarnings(page);
    // The bond comes from that endpoint, so its loss must be stated rather
    // than silently producing a gross figure labelled as net.
    await expect(page.getByTestId("projection-assumptions")).toContainText(
      "the bond could not be read from this host",
    );
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
