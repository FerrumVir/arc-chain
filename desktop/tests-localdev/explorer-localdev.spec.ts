import { expect, test } from "@playwright/test";

const RPC = process.env.ARC_LOCALDEV_RPC;

test.describe("explorer against a local disposable node", () => {
  test.skip(
    !RPC,
    "set ARC_LOCALDEV_RPC to a running local node (scripts/arc-multinode-fixture.sh prints one)",
  );

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

    const rows = await page.locator("#blocks-body tr td:not(.empty-cell)").count();
    expect(rows).toBe(0);
  });
});
