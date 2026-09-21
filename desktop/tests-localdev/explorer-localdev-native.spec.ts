import { expect, test } from "@playwright/test";

// LOCAL-DEVELOPMENT explorer: native paid inference (protocol 4).
//
// Against a disposable protocol-4 network - e.g. the soak harness's
// `--workload native` validators. It discovers a settled request from the
// chain itself (a NativeInferenceFinalize transaction in a recent block), so
// every assertion is about a real receipt, cross-checked against the node's
// own JSON rather than against anything the page computed.
//
// ARC_LOCALDEV_NATIVE_RPC: one node's RPC base (http://127.0.0.1:9960)
// ARC_LOCALDEV_NATIVE_REPLICAS: optional comma-separated other nodes
const RPC = process.env.ARC_LOCALDEV_NATIVE_RPC;
const REPLICAS = (process.env.ARC_LOCALDEV_NATIVE_REPLICAS ?? "").split(",").filter(Boolean);

type Json = Record<string, any>;
const get = async (path: string): Promise<Json | null> => {
  const response = await fetch(`${RPC}${path}`);
  return response.ok ? response.json() : null;
};

/// The newest settled native request in the last 2,000 blocks, with the block
/// that settled it.
async function settledRequest(): Promise<{ requestId: string; height: number } | null> {
  const health = await get("/health");
  const tip = Number(health?.height ?? 0);
  for (let from = Math.max(1, tip - 100); from > Math.max(0, tip - 2_000); from -= 100) {
    const page = await get(`/blocks?from=${from}&to=${from + 99}&limit=100`);
    const blocks = (page?.blocks ?? []).filter((b: Json) => b.tx_count > 0).reverse();
    for (const block of blocks) {
      const listing = await get(`/block/${block.height}/txs`);
      for (const tx of listing?.transactions ?? []) {
        const full = await get(`/tx/${tx.hash}/full`);
        if (full?.tx_type === "NativeInferenceFinalize" && full.success) {
          return { requestId: String(full.body.request_id), height: Number(block.height) };
        }
      }
    }
  }
  return null;
}

test.describe("local-development explorer: native inference", () => {
  test.skip(!RPC, "set ARC_LOCALDEV_NATIVE_RPC to a node of a protocol-4 network");

  test("a deep-linked request shows its canonical receipt, reconciled", async ({ page }) => {
    const found = await settledRequest();
    test.skip(!found, "no settled native request in the last 2,000 blocks");
    const receipt = (await get(`/native-inference/receipt/${found!.requestId}`))!;
    const replicas = REPLICAS.length ? `&replicas=${encodeURIComponent(REPLICAS.join(","))}` : "";
    await page.goto(
      `/explorer/localdev.html?rpc=${encodeURIComponent(RPC!)}&request=${found!.requestId}${replicas}`,
    );
    const status = page.locator("#localdev-native-status");
    await expect(status).toHaveText(receipt.observed_status, { timeout: 30_000 });
    const credited = (receipt.settlement_credits as Json[]).reduce((s, c) => s + Number(c.amount), 0);
    expect(credited).toBe(Number(receipt.reserved_max_payment));
    await expect(page.locator("#localdev-native-credited")).toHaveAttribute("data-reconciled", "true");
    await expect(page.locator("#localdev-native-settled")).toHaveText(
      String(receipt.terminal_transaction.block_height),
    );
    await expect(page.locator("#localdev-native-credits li")).toHaveCount(
      (receipt.settlement_credits as Json[]).length,
    );
    if (REPLICAS.length) {
      await expect(page.locator("#localdev-native-replicas")).toHaveAttribute("data-agree", "true");
    }
  });

  test("a native transaction in a block opens its request", async ({ page }) => {
    const found = await settledRequest();
    test.skip(!found, "no settled native request in the last 2,000 blocks");
    await page.goto(
      `/explorer/localdev.html?rpc=${encodeURIComponent(RPC!)}&height=${found!.height}`,
    );
    const item = page.locator("#localdev-tx-list .localdev-tx").first();
    await expect(item.locator(".localdev-tx-type")).toHaveText(/NativeInferenceFinalize/, {
      timeout: 30_000,
    });
    await item.locator(".localdev-native-open").click();
    await expect(page.locator("#localdev-native-input")).toHaveValue(found!.requestId);
    await expect(page.locator("#localdev-native-status")).toHaveText(/Finalized|Refunded/);
  });

  test("an unknown request is said to be unknown, not given a status", async ({ page }) => {
    await page.goto(
      `/explorer/localdev.html?rpc=${encodeURIComponent(RPC!)}&request=${"0".repeat(64)}`,
    );
    await expect(page.locator("#localdev-native-status")).toHaveText(/No receipt on this node/, {
      timeout: 30_000,
    });
  });
});
