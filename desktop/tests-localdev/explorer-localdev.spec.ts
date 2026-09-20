import { expect, test } from "@playwright/test";

// Local-development explorer coverage against a disposable node.
//
// Gate 5 was previously claimed on 214 mock-backed tests plus a shell script
// that called RPC endpoints with curl. Neither renders the product. This loads
// the real explorer page in a real browser and asserts on what a user would
// actually see, with the page talking to a real local node.
//
// It is NOT the recovered-production gate. That one needs an approved recovery
// checkpoint and all six production maintenance interlocks and is untouched.
//
// Requires ARC_LOCALDEV_RPC, e.g. http://127.0.0.1:9980 - the fixture prints
// its node URLs. Skips loudly rather than silently passing when unset.
const RPC = process.env.ARC_LOCALDEV_RPC;

test.describe("explorer against a local disposable node", () => {
  test.skip(
    !RPC,
    "set ARC_LOCALDEV_RPC to a running local node (scripts/arc-multinode-fixture.sh prints one)",
  );

  test.beforeEach(async ({ page }) => {
    // Supply a local-development network configuration in place of the
    // production one. The page, its config loader and app.js are unmodified;
    // only the configuration document differs, which is exactly the seam an
    // operator would use.
    const localConfig = {
      schema: "arc.frontend.network.v1",
      state: "recovered",
      updatedAt: new Date().toISOString(),
      network: { name: "ARC LOCAL DEV (disposable)", chainId: "0x415243" },
      checkpoint: null,
      sources: [
        {
          id: "localdev",
          name: "ARC local dev node",
          region: "local",
          kind: "v3",
          baseUrl: RPC,
          replicaGroup: "localdev",
          enabled: true,
        },
      ],
    };
    await page.route("**/arc-network.json", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify(localConfig),
      }),
    );
  });

  test("renders live chain status from a real node instead of staying unconfigured", async ({
    page,
  }) => {
    await page.goto("/explorer/index.html");

    // The page must leave its "no chain claim has been made yet" state. That
    // banner is the explorer's own admission that it has nothing, so a test
    // that passes while it is still showing is testing nothing.
    const label = page.locator("#network-label");
    await expect(label).not.toHaveText(/CONFIGURING/i, { timeout: 30_000 });
    await expect(page.locator("#banner-title")).not.toHaveText(
      /Loading canonical configuration/i,
      { timeout: 30_000 },
    );
  });

  test("renders real mined blocks in the block table", async ({ page }) => {
    await page.goto("/explorer/index.html");

    // The empty state names itself, so assert it is gone and that real rows
    // arrived rather than asserting on a row count that an empty cell satisfies.
    const emptyCell = page.locator("#blocks-body .empty-cell");
    await expect(emptyCell).toHaveCount(0, { timeout: 45_000 });

    const rows = page.locator("#blocks-body tr");
    await expect(rows.first()).toBeVisible({ timeout: 45_000 });

    // A height cell must hold a real number produced by the local chain.
    const firstHeight = await rows.first().locator("td").first().innerText();
    const parsed = Number.parseInt(firstHeight.replace(/[^0-9]/g, ""), 10);
    expect(Number.isFinite(parsed)).toBe(true);
    expect(parsed).toBeGreaterThan(0);
  });

  test("surfaces an unreachable node as an error instead of fabricating data", async ({
    page,
  }) => {
    // Point the page at a port nothing is listening on. The explorer must say
    // so; it must not render a plausible-looking chain.
    await page.route("**/arc-network.json", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          schema: "arc.frontend.network.v1",
          state: "degraded",
          updatedAt: new Date().toISOString(),
          network: { name: "ARC LOCAL DEV (unreachable)", chainId: "0x415243" },
          checkpoint: null,
          sources: [
            {
              id: "localdev-dead",
              name: "unreachable",
              region: "local",
              kind: "v3",
              baseUrl: "http://127.0.0.1:9",
              replicaGroup: "localdev",
              enabled: true,
            },
          ],
        }),
      }),
    );
    await page.goto("/explorer/index.html");

    // Either the banner reports a problem or the block table stays empty. What
    // must NOT happen is a populated table of blocks that do not exist.
    await page.waitForTimeout(8_000);
    const rows = await page.locator("#blocks-body tr td:not(.empty-cell)").count();
    expect(rows).toBe(0);
  });
});
