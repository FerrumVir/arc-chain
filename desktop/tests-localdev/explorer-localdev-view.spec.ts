import { expect, test } from "@playwright/test";

// LOCAL-DEVELOPMENT explorer view, against a disposable local network.
//
// The canonical explorer (/explorer/index.html) refuses to publish block data
// on its home panels until the production maintenance interlock is configured,
// and that refusal is covered by explorer-localdev.spec.ts. It stays as it is:
// a disposable chain has no interlock, and fabricating one would mean writing a
// safety attestation that nothing backs. Its deep-link routes still answer
// explicit lookups, labelled non-canonical; explorer-canonical-routes.spec.ts
// covers those.
//
// /explorer/localdev.html is a SEPARATE, loudly labelled route that reads one
// local node directly and never touches the canonical configuration path. This
// spec asserts it shows what the milestone asked for: real blocks, transaction
// detail, and balances.
//
// Requires ARC_LOCALDEV_RPC (scripts/arc-localdev-network.sh prints it).
const RPC = process.env.ARC_LOCALDEV_RPC;

test.describe("local-development explorer view", () => {
  test.skip(
    !RPC,
    "set ARC_LOCALDEV_RPC to a running local node (scripts/arc-localdev-network.sh prints one)",
  );

  const view = () => `/explorer/localdev.html?rpc=${encodeURIComponent(RPC!)}`;

  test("is unmistakably labelled as non-canonical local development data", async ({ page }) => {
    await page.goto(view());
    const banner = page.locator("#localdev-banner");
    await expect(banner).toBeVisible();
    await expect(banner).toContainText(/LOCAL DEVELOPMENT VIEW/i);
    await expect(banner).toContainText(/Not canonical/i);
    await expect(banner).toContainText(/Not production/i);
    // The canonical resolver must not be involved in this route at all.
    const scripts = await page.locator("script[src]").evaluateAll((nodes) =>
      nodes.map((n) => (n as HTMLScriptElement).getAttribute("src") ?? ""),
    );
    expect(scripts.some((src) => src.includes("arc-network.js"))).toBe(false);
  });

  test("renders real blocks committed by the local node", async ({ page }) => {
    await page.goto(view());

    // Height must come from the node and be a committed block, not a placeholder.
    const heightText = page.locator("#localdev-height");
    await expect(heightText).not.toHaveText("—", { timeout: 30_000 });
    const height = Number((await heightText.textContent())?.trim());
    expect(Number.isFinite(height)).toBe(true);
    expect(height).toBeGreaterThan(0);

    // Real rows, not an empty-state cell.
    await expect(page.locator("#localdev-blocks-body .empty-cell")).toHaveCount(0, {
      timeout: 30_000,
    });
    const rows = page.locator("#localdev-blocks-body tr");
    await expect(rows.first()).toBeVisible({ timeout: 30_000 });
    expect(await rows.count()).toBeGreaterThan(0);

    // The top row must name a block the node actually committed, so the table
    // is proven to be rendering chain data rather than anything invented
    // locally. The chain seals blocks continuously, so this asks the node about
    // that exact height rather than comparing two separately-timed listings.
    const topHeight = Number(
      (await page.locator("#localdev-blocks-body tr .localdev-block-height").first().textContent())?.trim(),
    );
    expect(topHeight).toBeGreaterThan(0);
    const nodeBlock = await fetch(`${RPC}/block/${topHeight}`).then((r) => r.json());
    expect(Number(nodeBlock.header.height)).toBe(topHeight);
    expect(String(nodeBlock.hash).replace(/^0x/, "")).toMatch(/^[0-9a-f]{64}$/);
    expect(topHeight).toBeLessThanOrEqual(height + 30);
  });

  test("shows the transactions of a block that actually carries one", async ({ page }) => {
    // The disposable chain seals blocks continuously, so by the time a browser
    // opens the page the block holding a transaction is long out of the
    // newest-15 window. Rather than assert on whatever happens to be on screen,
    // this puts a real transaction on the chain, asks the node which block it
    // landed in, and opens that block by height.
    const recipient = "2".repeat(64);
    const claim = await fetch(`${RPC}/faucet/claim`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ address: recipient }),
    }).then((r) => (r.ok ? r.json() : null));
    test.skip(!claim?.tx_hash, "the local node refused a faucet claim, so there is no transaction to show");

    let landed: { block_height: number; tx_hash: string } | null = null;
    for (let i = 0; i < 40 && !landed; i += 1) {
      const response = await fetch(`${RPC}/tx/${claim.tx_hash}`);
      if (response.ok) landed = await response.json();
      else await new Promise((resolve) => setTimeout(resolve, 500));
    }
    expect(landed, "the faucet transaction never appeared in a block").not.toBeNull();

    await page.goto(
      `/explorer/localdev.html?rpc=${encodeURIComponent(RPC!)}&height=${landed!.block_height}`,
    );

    await expect(page.locator("#localdev-detail-height")).toHaveText(
      String(landed!.block_height),
      { timeout: 30_000 },
    );
    const declared = Number((await page.locator("#localdev-detail-txcount").textContent())?.trim());
    expect(declared).toBeGreaterThan(0);

    const txs = page.locator("#localdev-tx-list .localdev-tx");
    await expect(txs.first()).toBeVisible({ timeout: 15_000 });
    expect(await txs.count()).toBe(declared);
    // The listed hash must be the transaction that was actually submitted.
    const shown = (await txs.first().locator("code").textContent()) ?? "";
    const [head, tail] = shown.trim().replace(/^0x/, "").split("\u2026");
    expect(landed!.tx_hash.startsWith(head)).toBe(true);
    expect(landed!.tx_hash.endsWith(tail)).toBe(true);
  });

  test("shows transaction detail for a selected block", async ({ page }) => {
    await page.goto(view());
    await expect(page.locator("#localdev-blocks-body .empty-cell")).toHaveCount(0, {
      timeout: 30_000,
    });

    // Prefer a block that actually carries transactions, so this test shows
    // transaction DETAIL rather than only an empty-block statement. Falls back
    // to the newest block when the disposable chain has only empty ones.
    const rows = page.locator("#localdev-blocks-body tr");
    const counts = await rows.locator(".localdev-block-txcount").allTextContents();
    const withTxs = counts.findIndex((c) => Number(c.trim()) > 0);
    await rows.nth(withTxs >= 0 ? withTxs : 0).click();
    const detail = page.locator("#localdev-block-detail");
    await expect(detail).toBeVisible();

    const detailHeight = Number((await page.locator("#localdev-detail-height").textContent())?.trim());
    expect(Number.isFinite(detailHeight)).toBe(true);

    // A 64-hex block hash for that exact height, cross-checked against the node.
    const hash = ((await page.locator("#localdev-detail-hash").textContent()) ?? "")
      .trim()
      .replace(/^0x/, "");
    expect(hash).toMatch(/^[0-9a-f]{64}$/);
    const nodeBlock = await fetch(`${RPC}/block/${detailHeight}`).then((r) => r.json());
    expect(String(nodeBlock.hash).replace(/^0x/, "")).toBe(hash);

    // The transaction pane must resolve to a definite statement: either listed
    // transactions or an explicit "no transactions" line. A silently empty pane
    // would pass a weaker assertion while showing the user nothing.
    const txItems = page.locator("#localdev-tx-list li");
    await expect(txItems.first()).toBeVisible({ timeout: 15_000 });
    const declaredTxCount = Number(
      (await page.locator("#localdev-detail-txcount").textContent())?.trim(),
    );
    if (declaredTxCount > 0) {
      await expect(page.locator("#localdev-tx-list .localdev-tx").first()).toBeVisible();
    } else {
      await expect(page.locator("#localdev-tx-list .localdev-no-txs")).toBeVisible();
    }
  });

  test("resolves a real account balance from local chain state", async ({ page }) => {
    await page.goto(view());

    // The page preloads the first genesis validator; assert on that real value.
    const value = page.locator("#localdev-balance-value");
    await expect(value).not.toHaveText("—", { timeout: 30_000 });
    const shown = (await value.textContent())?.trim() ?? "";
    expect(shown).toMatch(/^\d+$/);

    const address = (await page.locator("#localdev-account-input").inputValue())
      .trim()
      .replace(/^0x/, "");
    expect(address).toMatch(/^[0-9a-f]{64}$/);
    const account = await fetch(`${RPC}/account/${address}`).then((r) => r.json());
    expect(shown).toBe(String(account.balance));
    await expect(page.locator("#localdev-balance-nonce")).toHaveText(String(account.nonce));
  });
});
