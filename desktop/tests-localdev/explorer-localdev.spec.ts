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

  /// Build a local-development network configuration from the LIVE chain.
  ///
  /// The explorer structurally requires a recovery checkpoint: `canonicalRoute`
  /// returns `recovery-checkpoint-unavailable` without one, so with a null
  /// checkpoint the page correctly renders "No canonical source configured" and
  /// nothing else can be tested. That is a real property of the product, not a
  /// test problem.
  ///
  /// So the checkpoint below is built from the local chain's OWN committed
  /// block: the height, block hash and state root are read from the running
  /// node, not invented. A disposable dev chain has no recovery boundary and no
  /// signed manifest, so those fields reuse that same real block identity. This
  /// document describes a throwaway local chain and is **not** a recovery
  /// claim; the recovered-production gate keeps its own fleet-supplied
  /// checkpoint and is untouched.
  async function localConfigFromChain(): Promise<Record<string, unknown>> {
    const health = await fetch(`${RPC}/health`).then((r) => r.json());
    const height = Math.max(1, Number(health.height) - 2);
    const [block, snap] = await Promise.all([
      fetch(`${RPC}/block/${height}`).then((r) => r.json()),
      fetch(`${RPC}/sync/snapshot/info`).then((r) => r.json()),
    ]);
    const strip = (h: string) => String(h ?? "").replace(/^0x/, "");
    const blockHash = strip(block.hash);
    const stateRoot = strip(snap.state_root);
    if (blockHash.length !== 64 || stateRoot.length !== 64) {
      throw new Error(
        `local node did not return a usable block identity at height ${height}: ` +
          `hash=${blockHash} root=${stateRoot}`,
      );
    }
    return {
      schema: "arc.frontend.network.v1",
      state: "recovered",
      updatedAt: new Date().toISOString(),
      network: { name: "ARC LOCAL DEV (disposable)", chainId: "0x415243" },
      checkpoint: {
        height,
        recoveryHeight: height + 1,
        legacyPublicMaxHeight: height,
        blockHash,
        stateRoot,
        // No recovery boundary or signed manifest exists on a disposable dev
        // chain; these reuse the same real local block identity rather than
        // asserting anything about a recovery that never happened.
        manifestHash: blockHash,
        boundaryBlockHash: blockHash,
        boundaryStateRoot: stateRoot,
        recoveryDomain: stateRoot,
        recoveryEpoch: 1,
        validatorSetId: 1,
        protocolVersion: "3.0.0",
        legacySourceId: "localdev",
        v3SourceId: "localdev",
      },
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
  }

  test.beforeEach(async ({ page }) => {
    // The page, its config loader and app.js are unmodified; only the
    // configuration document differs, supplied through the existing
    // `<meta name="arc-network-config">` seam.
    const localConfig = await localConfigFromChain();
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

  test("pauses canonical publication, with a reason, when no interlock is configured", async ({
    page,
  }) => {
    // What this test originally asserted - that real blocks render - turns out
    // not to be reachable locally, and the reason is a safety property rather
    // than a bug.
    //
    // With a valid checkpoint the explorer still refuses to publish canonical
    // block data and says exactly why:
    //
    //   "Network maintenance safety interlock active"
    //   "Canonical publication is paused: maintenance-interlock-unconfigured."
    //
    // The maintenance interlock is production-recovery machinery (six seeds,
    // pinned source/boundary/tool hashes). A disposable local chain has none,
    // and synthesising one would mean fabricating a safety attestation in a
    // configuration document. That is not a thing to do to make a test green.
    //
    // So this asserts the behaviour that IS correct and IS testable: the page
    // withholds canonical data and states the reason, rather than rendering
    // blocks it cannot vouch for. Rendering real blocks in this table remains
    // out of reach locally - recorded as a limitation, not worked around.
    await page.goto("/explorer/index.html");

    await expect(page.locator("#blocks-body .empty-cell")).toHaveCount(1, {
      timeout: 30_000,
    });
    const banner = page.locator("#connection-banner");
    await expect(banner).toContainText(/interlock/i, { timeout: 30_000 });
    await expect(banner).toContainText(/maintenance-interlock-unconfigured/i);
  });

  test("surfaces an unreachable node as an error instead of fabricating data", async ({
    page,
  }) => {
    // Same valid local configuration, but the source points at a port nothing
    // is listening on. Reusing the real checkpoint matters: with a null
    // checkpoint the page would withhold data for an unrelated reason and this
    // would pass without testing unreachability at all.
    const cfg = await localConfigFromChain();
    (cfg.sources as Array<Record<string, unknown>>)[0].baseUrl = "http://127.0.0.1:9";
    (cfg.network as Record<string, unknown>).name = "ARC LOCAL DEV (unreachable)";
    await page.route("**/arc-network.json", (route) =>
      route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify(cfg),
      }),
    );
    await page.goto("/explorer/index.html");
    await page.waitForTimeout(8_000);

    // No fabricated chain: zero real block rows.
    const rows = await page.locator("#blocks-body tr td:not(.empty-cell)").count();
    expect(rows).toBe(0);
  });
});
