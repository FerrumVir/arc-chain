import { expect, test, type Locator } from "@playwright/test";

// The CANONICAL explorer (/explorer/index.html, the page the public site
// publishes), through its deep-link routes, against a disposable protocol-4
// network - e.g. the soak harness's `--workload native` validators:
//   #/request/<id>  #/blocks/<start>  #/block/<h>  #/tx/<hash>  #/address/<a>
//
// A disposable chain has no production maintenance interlock, so the home
// panels stay paused (explorer-localdev.spec.ts pins that) and nothing below
// may be labelled canonical: the configured sources answer explicit lookups,
// but nothing verifies them as the canonical timeline. The labels are
// asserted, not assumed.
//
// What this adds over the node-only contract tests (explorer/test-contract.mjs)
// is the rendered page in a real browser: the routes, the DOM a user reads,
// the links between views and a reload. Every displayed value is compared
// with the node's own JSON, never with anything the page computed.
//
// The configuration document describes this throwaway chain only. Its
// checkpoint is one of the chain's own committed blocks, read from the node
// as in explorer-localdev.spec.ts, and it reaches the page through the
// page's existing configuration seam. It makes no recovery claim, and no
// interlock evidence is synthesized.
//
// ARC_LOCALDEV_NATIVE_RPC: one node's RPC base (http://127.0.0.1:9960)
// ARC_LOCALDEV_NATIVE_REPLICAS: the other nodes, comma-separated (optional)
const RPC = process.env.ARC_LOCALDEV_NATIVE_RPC;
const REPLICAS = (process.env.ARC_LOCALDEV_NATIVE_REPLICAS ?? "")
  .split(",")
  .map((entry) => entry.trim())
  .filter(Boolean);
const NODES = RPC ? [RPC, ...REPLICAS] : [];

type Json = Record<string, any>;

async function get(base: string, path: string): Promise<Json | null> {
  const response = await fetch(`${base}${path}`);
  return response.ok ? response.json() : null;
}

const bare = (value: unknown) => String(value ?? "").replace(/^0x/i, "").toLowerCase();
// The page's integer rule (explorer/app.js formatExactInteger): a safe
// non-negative integer is formatted, anything else reads "Unavailable".
const exact = (value: unknown) =>
  typeof value === "number" && Number.isSafeInteger(value) && value >= 0
    ? new Intl.NumberFormat("en-US").format(value)
    : "Unavailable";
const short = (hash: string) => `0x${hash.slice(0, 8)}…${hash.slice(-6)}`;
const sourceId = (base: string) => `node-${new URL(base).port}`;

/// The value cell of one labelled row of a detail grid.
const field = (scope: Locator, label: string) =>
  scope
    .locator("div.detail-item")
    .filter({ has: scope.page().getByText(label, { exact: true }) })
    .locator("dd");

/// A local-development configuration built from the live chain: the
/// checkpoint is the first node's own block two below its tip.
async function chainConfig(sources: string[]): Promise<Json> {
  const health = await get(RPC!, "/health");
  const height = Math.max(1, Number(health?.height ?? 0) - 2);
  const block = await get(RPC!, `/block/${height}`);
  const blockHash = bare(block?.hash);
  const stateRoot = bare(block?.header?.state_root);
  if (blockHash.length !== 64 || stateRoot.length !== 64) {
    throw new Error(`${RPC} returned no usable block identity at height ${height}`);
  }
  const ids = sources.map(sourceId);
  if (new Set(ids).size !== ids.length) throw new Error(`node ports must differ: ${ids.join(", ")}`);
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
      // A disposable chain has no recovery boundary or signed manifest. These
      // reuse the same real block identity and assert nothing about a
      // recovery that never happened.
      manifestHash: blockHash,
      boundaryBlockHash: blockHash,
      boundaryStateRoot: stateRoot,
      recoveryDomain: stateRoot,
      recoveryEpoch: 1,
      validatorSetId: 1,
      protocolVersion: "3.0.0",
      legacySourceId: sourceId(RPC!),
      v3SourceId: sourceId(RPC!),
    },
    sources: sources.map((base) => ({
      id: sourceId(base),
      name: `local node ${new URL(base).port}`,
      region: "local",
      kind: "v3",
      baseUrl: base,
      replicaGroup: "localdev",
      enabled: true,
    })),
  };
}

type Settled = { requestId: string; finalizeTx: string; height: number };

/// The newest request settled by a NativeInferenceFinalize at least 20
/// blocks below the tip, so every replica has applied it.
async function settledRequest(): Promise<Settled | null> {
  const health = await get(RPC!, "/health");
  const tip = Number(health?.height ?? 0);
  for (let to = tip - 20; to > Math.max(0, tip - 2_000); to -= 100) {
    const from = Math.max(1, to - 99);
    const page = await get(RPC!, `/blocks?from=${from}&to=${to}&limit=100`);
    const blocks = ((page?.blocks ?? []) as Json[]).filter((b) => b.tx_count > 0).reverse();
    for (const block of blocks) {
      const listing = await get(RPC!, `/block/${block.height}/txs?offset=0&limit=100`);
      for (const tx of (listing?.transactions ?? []) as Json[]) {
        const full = await get(RPC!, `/tx/${bare(tx.hash)}/full`);
        if (full?.tx_type === "NativeInferenceFinalize" && full.success) {
          return {
            requestId: bare(full.body.request_id),
            finalizeTx: bare(full.tx_hash),
            height: Number(block.height),
          };
        }
      }
    }
  }
  return null;
}

test.describe("canonical explorer routes against a local protocol-4 network", () => {
  test.skip(!RPC, "set ARC_LOCALDEV_NATIVE_RPC to a node of a protocol-4 network");
  test.use({ locale: "en-US" });

  let found: Settled | null = null;
  let config: Json = {};

  test.beforeAll(async () => {
    if (!RPC) return;
    test.setTimeout(180_000);
    found = await settledRequest();
    config = await chainConfig(NODES);
  });

  test.beforeEach(async ({ page }) => {
    await page.route("**/arc-network.json", (route) =>
      route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(config) }),
    );
  });

  test("a deep-linked request shows every node's receipt in agreement, and survives a reload", async ({
    page,
  }) => {
    test.skip(!found, "no settled native request between 20 and 2,000 blocks below the tip");
    const id = found!.requestId;
    const receipts = await Promise.all(NODES.map((base) => get(base, `/native-inference/receipt/${id}`)));
    expect(receipts.map(Boolean), "every node holds the settled request's receipt").toEqual(
      NODES.map(() => true),
    );
    // A chain property first: honest replicas record one settlement, and its
    // credits add up to the reservation.
    const settlement = (r: Json) =>
      JSON.stringify([
        r.observed_status,
        r.output_hash,
        r.terminal_transaction?.block_height,
        [...(r.settlement_credits as Json[])].map((c) => `${c.payee}:${c.amount}`).sort(),
      ]);
    expect(new Set(receipts.map((r) => settlement(r!))).size, "the nodes disagree").toBe(1);

    await page.goto(`/explorer/index.html#/request/${id}`);
    const content = page.locator("#inspector-content");
    for (const pass of ["deep link", "after reload"]) {
      await expect(page.locator("#inspector-kicker"), pass).toHaveText(
        "Native request · per-source receipts",
        { timeout: 30_000 },
      );
      await expect(content.locator("p.inspector-note.good")).toHaveText(
        `${NODES.length} of ${NODES.length} permitted source(s) returned a receipt, and all of them record the same settlement.`,
      );
      await expect(content.locator("p.inspector-note.error")).toHaveCount(0);
      const cards = content.locator("article.occurrence-card");
      await expect(cards).toHaveCount(NODES.length);
      for (const [index, base] of NODES.entries()) {
        const r = receipts[index]!;
        const credits = r.settlement_credits as Json[];
        const credited = credits.reduce((sum, c) => sum + Number(c.amount), 0);
        expect(credited, `${base}: credits equal the reservation`).toBe(Number(r.reserved_max_payment));
        const card = cards.filter({ has: page.getByRole("heading", { name: sourceId(base), exact: true }) });
        await expect(card).toHaveCount(1);
        await expect(field(card, "Chain status")).toHaveText(String(r.observed_status));
        await expect(field(card, "Settlement")).toHaveText("Credits equal the reservation");
        await expect(field(card, "Price / reserved")).toHaveText(
          `${exact(r.execution_price)} / ${exact(r.reserved_max_payment)}`,
        );
        await expect(field(card, "Credited")).toHaveText(exact(credited));
        await expect(field(card, "Certificate votes")).toHaveText(exact(r.certificate_votes));
        await expect(field(card, "Output hash")).toHaveText(`0x${bare(r.output_hash)}`);
        await expect(field(card, "Admitted at")).toHaveText(
          exact(r.admission_transaction?.block_height ?? r.admission_height),
        );
        await expect(field(card, "Terminal at")).toHaveText(exact(r.terminal_transaction?.block_height));
        // Fields a node reports only once upgraded: shown when served, and
        // never invented when not.
        await expect(field(card, "Expires at")).toHaveText(
          typeof r.expires_at === "number" ? `Height ${exact(r.expires_at)}` : "Unavailable from this node",
        );
        await expect(field(card, "Requester")).toHaveText(
          r.requester ? `0x${bare(r.requester)}` : "Unavailable",
        );
        if (typeof r.output_hex === "string") {
          await expect(field(card, "Certified output (hex)")).toHaveText(
            r.output_hex || "Empty · not finalized yet",
          );
        }
      }
      if (pass === "deep link") await page.reload();
    }
  });

  test("the blocks page lists the node's own blocks and links to a block and its transaction", async ({
    page,
  }) => {
    test.skip(!found, "no settled native request between 20 and 2,000 blocks below the tip");
    const start = found!.height;
    const from = Math.max(0, start - 19);
    const listed = ((await get(RPC!, `/blocks?from=${from}&to=${start}&limit=20`))?.blocks ?? []) as Json[];
    const newestFirst = [...listed].sort((a, b) => b.height - a.height);

    await page.goto(`/explorer/index.html#/blocks/${start}`);
    await expect(page.locator("#inspector-title")).toHaveText(`Heights #${exact(from)} – #${exact(start)}`, {
      timeout: 30_000,
    });
    const content = page.locator("#inspector-content");
    const rows = content.locator("tbody tr");
    await expect(rows).toHaveCount(newestFirst.length);
    for (const [index, block] of newestFirst.entries()) {
      const cells = rows.nth(index).locator("td");
      await expect(cells.nth(0)).toHaveText(`#${exact(block.height)}`);
      await expect(cells.nth(1), "no row is labelled canonical").toHaveText("non-canonical / unverified");
      await expect(cells.nth(2)).toHaveText(exact(block.tx_count));
      await expect(cells.nth(3)).toHaveText(short(bare(block.hash)));
    }

    // Into the block that settled the request: the newest row of the page.
    expect(newestFirst[0]?.height).toBe(start);
    await rows.first().locator("button.table-link").click();
    await expect(page).toHaveURL(new RegExp(`#/block/${start}$`));
    await expect(page.locator("#inspector-kicker")).toHaveText("NON-CANONICAL BLOCK", { timeout: 30_000 });
    const block = (await get(RPC!, `/block/${start}`))!;
    await expect(field(content, "Canonical status")).toHaveText("Alternate / non-canonical");
    await expect(field(content, "Block hash")).toHaveText(`0x${bare(block.hash)}`);
    await expect(field(content, "Parent hash")).toHaveText(`0x${bare(block.header.parent_hash)}`);
    await expect(field(content, "State root")).toHaveText(`0x${bare(block.header.state_root)}`);
    await expect(field(content, "Transactions")).toHaveText(exact(block.header.tx_count));

    // Its transaction list links the finalize transaction.
    await content.locator(`ul.chip-list button[title="0x${found!.finalizeTx}"]`).click();
    await expect(page).toHaveURL(new RegExp(`#/tx/${found!.finalizeTx}$`));
    const full = (await get(RPC!, `/tx/${found!.finalizeTx}/full`))!;
    // The checkpoint's legacy and current sources are the same node, which
    // is asked once.
    const card = content.locator("article.occurrence-card");
    await expect(card).toHaveCount(1, { timeout: 30_000 });
    await expect(card.locator(".status-pill")).toHaveText("NOT CANONICAL");
    await expect(field(card, "Block")).toHaveText(exact(full.block_height));
  });

  test("an address shows the node's own balance, nonce and indexed history, newest first", async ({
    page,
  }) => {
    test.skip(!found, "no settled native request between 20 and 2,000 blocks below the tip");
    const receipt = (await get(RPC!, `/native-inference/receipt/${found!.requestId}`))!;
    let requester = bare(receipt.requester);
    if (requester.length !== 64) {
      // A node that does not report the requester yet: the admission
      // transaction's signer is the requester.
      const admission = await get(RPC!, `/tx/${bare(receipt.admission_transaction?.tx_hash)}/full`);
      requester = bare(admission?.from);
    }
    expect(requester).toHaveLength(64);

    await page.goto(`/explorer/index.html#/address/${requester}`);
    const content = page.locator("#inspector-content");
    // The requester keeps paying for requests, so each attempt compares one
    // render with the node's state read right after it, until they match.
    await expect(async () => {
      await page.reload();
      await expect(page.locator("#inspector-kicker")).toHaveText("Address · source-separated", {
        timeout: 15_000,
      });
      const card = content.locator("article.occurrence-card");
      await expect(card).toHaveCount(1);
      const chips = card.locator("ul.chip-list button");
      const shown = {
        balance: await field(card, "Balance (raw)").innerText(),
        nonce: await field(card, "Nonce").innerText(),
        indexed: await card.locator("h3", { hasText: /^Indexed transactions/ }).innerText(),
        newest: (await chips.count()) ? await chips.first().getAttribute("title") : null,
      };
      const [account, history] = await Promise.all([
        get(RPC!, `/account/${requester}`),
        get(RPC!, `/account/${requester}/txs`),
      ]);
      const hashes = (history?.tx_hashes ?? []) as string[];
      expect(shown).toEqual({
        balance: exact(account?.balance),
        nonce: exact(account?.nonce),
        indexed: `Indexed transactions (${exact(hashes.length)})`,
        newest: hashes.length ? `0x${bare(hashes[hashes.length - 1])}` : null,
      });
    }).toPass({ timeout: 90_000 });
  });

  test("malformed input is refused before any request, and an unknown request is not given a status", async ({
    page,
  }) => {
    const asked: string[] = [];
    page.on("request", (request) => {
      const path = new URL(request.url()).pathname;
      if (/^\/(tx|account|native-inference)\//.test(path)) asked.push(path);
    });
    await page.goto("/explorer/index.html");
    await expect(page.locator("#network-label")).not.toHaveText(/CONFIGURING/i, { timeout: 30_000 });

    await page.locator("#search-input").fill("not-a-hash");
    await page.locator("#search-input").press("Enter");
    await expect(page.locator("#search-error")).toHaveText(
      "Transactions, native requests and addresses must be 32-byte hexadecimal values.",
    );
    await page.goto("/explorer/index.html#/tx/not-a-hash");
    await expect(page.locator("#inspector-kicker")).toHaveText("Transaction / receipt", { timeout: 30_000 });
    await expect(page.locator("#inspector-title")).toHaveText("Lookup failed");
    await page.goto("/explorer/index.html#/address/..%2F..%2Fadmin");
    await expect(page.locator("#inspector-kicker")).toHaveText("Address", { timeout: 30_000 });
    await expect(page.locator("#inspector-title")).toHaveText("Lookup failed");
    expect(asked, "nothing malformed reached a node").toEqual([]);

    const unknown = "0".repeat(64);
    await page.goto(`/explorer/index.html#/request/${unknown}`);
    await expect(page.locator("#inspector-title")).toHaveText("Request not found", { timeout: 30_000 });
    await expect(page.locator("#inspector-content .error-state p")).toHaveText(
      `None of ${NODES.length} permitted source(s) holds a receipt for this request id.`,
    );
    expect(asked).toEqual(NODES.map(() => `/native-inference/receipt/${unknown}`));
  });

  test("a replica that cannot be reached is named on the request, not counted as agreeing", async ({
    page,
  }) => {
    test.skip(!found, "no settled native request between 20 and 2,000 blocks below the tip");
    // Port 9 has no listener: the same local configuration plus one replica
    // that cannot be asked. This route overrides the one set in beforeEach.
    const withDeadReplica = await chainConfig([...NODES, "http://127.0.0.1:9"]);
    await page.route("**/arc-network.json", (route) =>
      route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(withDeadReplica) }),
    );
    await page.goto(`/explorer/index.html#/request/${found!.requestId}`);
    const content = page.locator("#inspector-content");
    await expect(content.locator("p.inspector-note.good")).toHaveText(
      `${NODES.length} of ${NODES.length + 1} permitted source(s) returned a receipt, and all of them record the same settlement.`,
      { timeout: 30_000 },
    );
    await expect(content.locator("p.inspector-note.error")).toHaveText(
      /^node-9: could not be asked \(.+\), so its receipt is unknown\.$/,
    );
    await expect(content.locator("article.occurrence-card")).toHaveCount(NODES.length);
  });
});
