// The Network screen — "check the chain any time".
//
// Assertions are on format, presence and copy, never on magnitude. The two
// things most worth locking down are the stall warning (because `/health`
// cannot give it, and four of six live seeds are stopped while reporting ok)
// and the not-found-vs-invalid distinction in the lookup (because calling a
// pending attestation "invalid" would send a user hunting a bug that isn't
// there).

import { expect, test } from "@playwright/test";
import { seedMockOverrides, seedOnboarded } from "./helpers";

/** The mock's overview, as a base for targeted overrides. */
const OVERVIEW = {
  sourceHost: "http://140.82.16.112:9090",
  unavailable: null,
  networkName: "arc-testnet-1",
  networkNameUnavailableReason: null,
  chainId: "arc-testnet-1",
  declaresMainnet: false,
  isBlockProducing: true,
  isBlockProducingBasis: "sealed a block within the last 120s",
  hostVersion: "0.7.9",
  height: 123_469,
  lastBlockAgeSecs: 400,
  dagRound: 9_596_644,
  dagCommitted: 9_596_640,
  peers: 8,
  validatorsActive: 10,
  validatorsRegistered: 14,
  minActiveStake: 500_000,
  validatorSplitDerived: false,
  validators: [
    { address: "a".repeat(64), stake: 500_000_000_000_000, active: true },
    { address: "b".repeat(64), stake: 0, active: false },
  ],
};

async function gotoNetwork(page: import("@playwright/test").Page) {
  await page.goto("/");
  await page.getByTestId("nav-network").click();
  await expect(page.getByTestId("network-screen")).toBeVisible();
}

test.describe("Network screen - identity and attribution", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("names the network and the host it was read from", async ({ page }) => {
    await gotoNetwork(page);
    const identity = page.getByTestId("network-identity");
    await expect(identity).toContainText("arc-testnet-1");
    // Attribution: every figure on this screen belongs to one pinned host.
    await expect(identity).toContainText("LAX");
  });

  test("says the name is unknown when the host does not report one", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      fetch_network_overview: {
        ...OVERVIEW,
        networkName: null,
        chainId: null,
        networkNameUnavailableReason:
          "this node was started without --genesis, so it has no network name",
      },
    });
    await gotoNetwork(page);
    const identity = page.getByTestId("network-identity");
    await expect(identity).toContainText("Network name unknown");
    // It must still say which host it is reading.
    await expect(identity).toContainText("140.82.16.112");
  });

  test("never claims to be mainnet, named or unnamed", async ({ page }) => {
    await seedMockOverrides(page, {
      fetch_network_overview: {
        ...OVERVIEW,
        networkName: null,
        declaresMainnet: null,
        networkNameUnavailableReason: "no genesis file was loaded",
      },
    });
    await gotoNetwork(page);
    const text = await page.getByTestId("network-screen").innerText();
    expect(text).not.toMatch(/mainnet/i);
  });

  test("the external link is named for what it actually opens", async ({
    page,
  }) => {
    await gotoNetwork(page);
    const btn = page.getByTestId("btn-open-raw-json");
    await expect(btn).toBeVisible();
    // Not "explorer" — it opens a block header as JSON, and says so.
    await expect(btn).toContainText("Raw block JSON");
    await expect(btn).not.toContainText(/explorer/i);
  });

  test("offers the canonical composite explorer without blending host data", async ({
    page,
  }) => {
    await gotoNetwork(page);
    const btn = page.getByTestId("btn-open-composite-explorer");
    await expect(btn).toBeVisible();
    await expect(btn).toHaveText(/Composite explorer/i);
    await expect(page.getByTestId("tx-lookup")).toContainText(
      "this host-scoped lookup never blends sources",
    );
  });
});

test.describe("Network screen - stalled block production", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("warns that the host is not sealing blocks when the newest is days old", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      fetch_network_overview: {
        ...OVERVIEW,
        lastBlockAgeSecs: 6 * 24 * 3600,
        isBlockProducing: false,
        isBlockProducingBasis:
          "no block sealed within block_production_fresh_secs (120s)",
      },
    });
    await gotoNetwork(page);
    const banner = page.getByTestId("not-sealing-banner");
    await expect(banner).toBeVisible();
    await expect(banner).toContainText("not sealing blocks");
    // The reason `/health` cannot be trusted here has to be stated, or the
    // user will believe the green pill over this banner.
    await expect(banner).toContainText("DAG round keeps advancing");
    await expect(banner).toContainText(
      "cannot mine a new claim or reward receipt",
    );
  });

  test("warns more mildly when blocks lag but the host still claims to produce", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      // 400s is past the warn threshold and short of the stall threshold, and
      // this host has NOT declared itself stopped. Both conditions matter: the
      // host's own verdict alone is enough to escalate to "not sealing".
      fetch_network_overview: {
        ...OVERVIEW,
        lastBlockAgeSecs: 400,
        isBlockProducing: true,
      },
    });
    await gotoNetwork(page);
    await expect(page.getByTestId("block-lag-banner")).toBeVisible();
    await expect(page.getByTestId("not-sealing-banner")).toHaveCount(0);
  });

  test("escalates to 'not sealing' on the host's own verdict, not just age", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      // Only 400s old — well short of the hour-long stall threshold — but the
      // host says it is not producing. Its word is enough.
      fetch_network_overview: {
        ...OVERVIEW,
        lastBlockAgeSecs: 400,
        isBlockProducing: false,
        isBlockProducingBasis:
          "no block sealed within block_production_fresh_secs (120s)",
      },
    });
    await gotoNetwork(page);
    const banner = page.getByTestId("not-sealing-banner");
    await expect(banner).toBeVisible();
    // The host's basis is quoted rather than paraphrased.
    await expect(banner).toContainText("block_production_fresh_secs");
  });

  test("shows no warning at all on a healthy block age", async ({ page }) => {
    await seedMockOverrides(page, {
      fetch_network_overview: {
        ...OVERVIEW,
        lastBlockAgeSecs: 3,
        isBlockProducing: true,
      },
    });
    await gotoNetwork(page);
    await expect(page.getByTestId("net-stat-last-block")).toBeVisible();
    await expect(page.getByTestId("not-sealing-banner")).toHaveCount(0);
    await expect(page.getByTestId("block-lag-banner")).toHaveCount(0);
  });
});

test.describe("Network screen - validators, height, peers", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("reports active vs registered validators as a ratio", async ({
    page,
  }) => {
    await gotoNetwork(page);
    // Format, not magnitude: "N / M".
    await expect(page.getByTestId("net-stat-validators")).toHaveText(
      /^[\d,]+ \/ [\d,]+$/,
    );
  });

  test("explains that zero-stake validators inflate the reported set", async ({
    page,
  }) => {
    await gotoNetwork(page);
    const split = page.getByTestId("validator-split");
    await expect(split).toContainText("hold stake above zero");
    await expect(split).toContainText("cannot lead a round");
    // The default fixture has the host reporting the split itself, so the copy
    // must attribute it to the host and name the threshold it used.
    await expect(split).toContainText("Reported by this host");
    await expect(split).toContainText("stake or more");
  });

  test("says so when the split had to be derived locally instead", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      // An older host: /network/info absent, so the active count is counted
      // from /validators by taking stake > 0 — an approximation of the real
      // min_active_stake threshold, and it must not be passed off as reported.
      fetch_network_overview: {
        ...OVERVIEW,
        validatorSplitDerived: true,
        minActiveStake: null,
      },
    });
    await gotoNetwork(page);
    const split = page.getByTestId("validator-split");
    await expect(split).toContainText("derived here by counting stake");
    await expect(split).not.toContainText("Reported by this host");
  });

  test("the validator list expands on request", async ({ page }) => {
    await gotoNetwork(page);
    await expect(page.getByTestId("validator-list")).toHaveCount(0);
    await page.getByTestId("btn-toggle-validators").click();
    const list = page.getByTestId("validator-list");
    await expect(list).toBeVisible();
    await expect(list.getByText("zero-stake").first()).toBeVisible();
  });

  test("height and DAG round render as integers", async ({ page }) => {
    await gotoNetwork(page);
    await expect(page.getByTestId("net-stat-block-height")).toHaveText(
      /^[\d,]+$/,
    );
    await expect(page.getByTestId("net-stat-dag-round")).toHaveText(/^[\d,]+$/);
  });

  test("an unreadable chain degrades to a reason and em dashes, not zeros", async ({
    page,
  }) => {
    await seedMockOverrides(page, {
      fetch_network_overview: {
        ...OVERVIEW,
        unavailable:
          "Could not reach http://140.82.16.112:9090 — connection refused",
        networkName: null,
        height: null,
        lastBlockAgeSecs: null,
        dagRound: null,
        dagCommitted: null,
        peers: null,
        validatorsActive: null,
        validatorsRegistered: null,
        validators: [],
      },
    });
    await gotoNetwork(page);
    await expect(page.getByTestId("overview-unavailable")).toContainText(
      "Could not reach",
    );
    // A host that cannot answer is not a host reporting zero.
    await expect(page.getByTestId("net-stat-block-height")).toHaveText("—");
    await expect(page.getByTestId("net-stat-peers")).toHaveText("—");
  });
});

test.describe("Network screen - transaction lookup", () => {
  // Mined in the mock: the first mock attestation's hash.
  const MINED = "0xe0c73bb8a4446f23a62033001cb22e1e9298d5ce1cfea8111762c1ca2833f67d";
  const WELL_FORMED_UNKNOWN = `0x${"9".repeat(64)}`;

  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("a mined hash reports its block and position", async ({ page }) => {
    await gotoNetwork(page);
    await page.getByTestId("tx-lookup-input").fill(MINED);
    await page.getByTestId("tx-lookup-submit").click();
    await expect(page.getByTestId("tx-status-mined")).toBeVisible();
    const result = page.getByTestId("tx-lookup-result");
    await expect(result).toContainText("In a block");
    await expect(result).toContainText("Position in block");
    await expect(result).toContainText("does not expose transaction type");
    await expect(result).toContainText("0x25");
    await expect(result).toContainText("0x16");
    await expect(result).toContainText("pays nothing");
  });

  test("an unknown but well-formed hash is 'not in a block yet', never invalid", async ({
    page,
  }) => {
    await gotoNetwork(page);
    await page.getByTestId("tx-lookup-input").fill(WELL_FORMED_UNKNOWN);
    await page.getByTestId("tx-lookup-submit").click();
    const notFound = page.getByTestId("tx-status-not-found");
    await expect(notFound).toBeVisible();
    await expect(notFound).toContainText("Not in a block yet");
    // The distinction that matters: a pending attestation returns not-found,
    // so this must not be presented as a bad hash.
    await expect(notFound).toContainText("waiting in the mempool");
    await expect(notFound).toContainText("does not mean the hash is invalid");
    await expect(page.getByTestId("tx-status-invalid")).toHaveCount(0);
  });

  test("a malformed hash IS called out as malformed", async ({ page }) => {
    await gotoNetwork(page);
    await page.getByTestId("tx-lookup-input").fill("not-a-hash");
    await page.getByTestId("tx-lookup-submit").click();
    const invalid = page.getByTestId("tx-status-invalid");
    await expect(invalid).toBeVisible();
    await expect(invalid).toContainText("64 hex characters");
    await expect(page.getByTestId("tx-status-not-found")).toHaveCount(0);
  });

  test("routes cross-boundary and preserved-fork questions to the composite explorer", async ({
    page,
  }) => {
    await gotoNetwork(page);
    await expect(page.getByTestId("tx-lookup")).toContainText(
      "For checkpoint-spanning history, replica agreement, or a preserved fork",
    );
    await expect(page.getByTestId("tx-lookup")).not.toContainText(
      "the seeds are separate chains",
    );
  });
});

test.describe("Network screen - blocks and attestations", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("lists recent blocks newest-first with heights and tx counts", async ({
    page,
  }) => {
    await gotoNetwork(page);
    const list = page.getByTestId("block-list");
    await expect(list).toBeVisible();
    const items = list.locator(".feed-item");
    expect(await items.count()).toBeGreaterThan(0);
    // Format only.
    await expect(items.first()).toContainText(/Block #[\d,]+/);
    // Newest first.
    const heights = await list.locator(".feed-item-title").allInnerTexts();
    const nums = heights.map((h) => Number(h.replace(/[^\d]/g, "")));
    const sorted = [...nums].sort((a, b) => b - a);
    expect(nums).toEqual(sorted);
  });

  test("a block expands to its transactions on demand", async ({ page }) => {
    await gotoNetwork(page);
    // The first mock block that reports a non-zero tx_count.
    const expander = page.getByTestId("btn-expand-block-123469");
    await expect(expander).toBeVisible();
    await expander.click();
    await expect(page.getByTestId("block-txs-123469")).toBeVisible();
  });

  test("old-seed padding is filtered out of the attestation list", async ({
    page,
  }) => {
    await gotoNetwork(page);
    const note = page.getByTestId("padding-filtered-note");
    await expect(note).toBeVisible();
    // The mock carries exactly one `tx_type: "Other"` padding row.
    await expect(note).toContainText("1 row from LAX was not an inference record");
    // Five of six fixtures are real inference records: three mined 0x16
    // computations and two distinct mined 0x25 reward receipts.
    await expect(
      page.getByTestId("recent-inference").locator(".feed-item"),
    ).toHaveCount(5);
    await expect(page.getByTestId("recent-inference")).toContainText(
      "COMPUTED + PAID",
    );
    await expect(
      page.getByTestId("recent-inference").getByText("COMPUTED + PAID", { exact: true }),
    ).toHaveCount(2);
  });
});

test.describe("Network screen - reached from elsewhere", () => {
  test.beforeEach(async ({ page }) => {
    await seedOnboarded(page);
  });

  test("the Dashboard's chain button opens the in-app Network screen", async ({
    page,
  }) => {
    await page.goto("/");
    const btn = page.getByTestId("btn-open-network");
    await expect(btn).toBeVisible();
    await expect(btn).toContainText("Check the chain");
    await btn.click();
    await expect(page.getByTestId("network-screen")).toBeVisible();
  });

  test("an attestation row on Earnings looks the hash up on the pinned host", async ({
    page,
  }) => {
    await page.goto("/");
    await page.getByTestId("nav-earnings").click();
    const lookupBtn = page
      .getByTestId(/^btn-lookup-earnings-/)
      .first();
    await expect(lookupBtn).toBeVisible();
    await lookupBtn.click();
    // Navigates to Network AND runs the lookup, prefilled.
    await expect(page.getByTestId("network-screen")).toBeVisible();
    await expect(page.getByTestId("tx-lookup-input")).not.toHaveValue("");
    await expect(page.getByTestId("tx-lookup-result")).toBeVisible();
  });
});
