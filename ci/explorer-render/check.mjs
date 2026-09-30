#!/usr/bin/env node

import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

// build-public-site.sh publishes the dashboard at the root and the explorer here.
export const HOME = "https://ferrumvir.github.io/arc-chain/explorer/";
export const REWARD = {
  hash: "0xbcccf7ba9994984613af9fe4691cefda4d9bd8934404fdf17074be25af785720",
  block: 3794543,
  worker: "0xfb1ac4793b45c45020ae0d9b1113bf249fe3e489664a418f456abec21ffcfa4f",
};
export const REQUESTS = [
  {
    id: "0x0d5f8286303b47a0bb6c11e9209cb79b9f3df4359a39ab52b5a14fd5d3e04283",
    status: "Finalized",
    admissionHash: "0x5f04ec0d91b0bf62241f5e54a4bf8ccb918928bff112ce4eaa86d101bcc63c03",
    admissionBlock: 3827760,
  },
  { id: "0xbaf7456c91ee637f62bfa26636793fbd3f900599ee663b53819c4cbbb2892f7b", status: "Refunded" },
];
export const CHECKS = [
  { name: "home", kind: "home", url: HOME },
  { name: "reward-transaction", kind: "tx", url: `${HOME}#/tx/${REWARD.hash}` },
  { name: "reward-block", kind: "block", url: `${HOME}#/block/${REWARD.block}` },
  ...REQUESTS.map((request) => ({ name: `request-${request.status.toLowerCase()}`, kind: "request", url: `${HOME}#/request/${request.id}`, request })),
  { name: "deep-link-reload", kind: "request", url: `${HOME}#/request/${REQUESTS[1].id}`, request: REQUESTS[1], reload: true },
];

const bare = (value) => String(value ?? "").replace(/^0x/i, "").toLowerCase();
const integer = (value) => /^[\d,]+$/.test(value ?? "") ? Number(value.replaceAll(",", "")) : NaN;

export function validateObservation(check, observed) {
  assert.equal(observed.url, check.url, "browser must remain on the requested public route");
  if (check.kind === "home") {
    assert.ok(integer(observed.height) >= REQUESTS[0].admissionBlock, "live reported height must be rendered");
    assert.ok(integer(observed.storedHeight) >= REQUESTS[0].admissionBlock, "latest stored block must be rendered");
    assert.equal(integer(observed.validators), 6, "six validators must be rendered");
    assert.deepEqual(observed.sources.map((source) => source.id).sort(), ["v3-ams", "v3-lax", "v3-lhr", "v3-nrt", "v3-nyc", "v3-sgp"]);
    assert.equal(observed.bannerTitle, "Canonical recovery verified");
    assert.match(observed.bannerDetail, /6 v3 replica identities verified/);
    return;
  }
  if (check.kind === "tx") {
    assert.equal(observed.kicker, "Transaction / receipt");
    const card = observed.cards.find((entry) => entry.receipts.some((receipt) =>
      receipt.label === "Community reward receipt" && bare(receipt.tx_hash) === bare(REWARD.hash)));
    assert.ok(card, "full transaction hash must appear in a rendered reward receipt");
    assert.equal(integer(card.fields.Block), REWARD.block);
    assert.equal(card.fields.Reward, "Earned · successful mined receipt");
    const receipt = card.receipts.find((entry) => entry.label === "Community reward receipt");
    assert.equal(receipt.status, "mined_success");
    assert.equal(receipt.tx_type, "0x25");
    assert.equal(receipt.block_height, REWARD.block);
    assert.equal(bare(receipt.worker), bare(REWARD.worker));
    assert.equal(receipt.reward_arc, 2.5);
    assert.equal(receipt.reward_base, 2_500_000_000);
    return;
  }
  if (check.kind === "block") {
    assert.equal(observed.title, "Block #3,794,543");
    assert.ok(observed.transactionLinks.some((link) => bare(link.hash) === bare(REWARD.hash) && link.text), "transaction must be listed as a visible block transaction link");
    return;
  }
  assert.equal(check.kind, "request");
  assert.equal(observed.kicker, "Native request · per-source receipts");
  const cards = observed.cards.filter((card) => card.receipts.some((receipt) => bare(receipt.request_id) === bare(check.request.id)));
  assert.ok(cards.length > 0, "request identity must appear in a rendered receipt");
  for (const card of cards) {
    assert.equal(card.fields["Chain status"], check.request.status);
    const receipt = card.receipts.find((entry) => bare(entry.request_id) === bare(check.request.id));
    assert.equal(receipt.observed_status, check.request.status);
    if (check.request.admissionBlock) {
      assert.equal(integer(card.fields["Admitted at"]), check.request.admissionBlock);
      assert.equal(receipt.admission_transaction?.block_height, check.request.admissionBlock);
      assert.equal(bare(receipt.admission_transaction?.tx_hash), bare(check.request.admissionHash));
    }
  }
}

// Read the rendered DOM, including the explorer's visible raw receipt panels.
// No RPC client, fixture injection, or replacement network configuration is used.
export function readRenderedPage() {
  const visible = (element) => element && element.getClientRects().length > 0 && getComputedStyle(element).visibility !== "hidden";
  const text = (selector, scope = document) => {
    const element = scope.querySelector(selector);
    return visible(element) ? element.innerText.trim() : "";
  };
  const cards = [...document.querySelectorAll("#inspector-content .occurrence-card")].filter(visible).map((card) => ({
    fields: Object.fromEntries([...card.querySelectorAll(".detail-item")].filter(visible).map((row) => [text("dt", row), text("dd", row)])),
    receipts: [...card.querySelectorAll(".detail-section")].filter(visible).flatMap((section) => {
      const label = text("h3", section);
      if (!["Receipt", "Community reward receipt"].includes(label)) return [];
      const excerpt = text("pre", section);
      try {
        const receipt = JSON.parse(excerpt);
        return [{ label, excerpt: excerpt.slice(0, 2000), ...Object.fromEntries([
          "tx_hash", "tx_type", "status", "block_height", "worker", "reward_arc", "reward_base",
          "request_id", "observed_status", "admission_transaction",
        ].filter((key) => key in receipt).map((key) => [key, receipt[key]])) }];
      } catch {
        return [{ label, excerpt: excerpt.slice(0, 2000) }];
      }
    }),
  }));
  return {
    url: location.href,
    height: text("#metric-height"),
    storedHeight: text("#metric-stored-height"),
    validators: text("#metric-validators"),
    validatorNote: text("#metric-validator-note"),
    sources: [...document.querySelectorAll('#source-select option[value^="v3-"]')].map((option) => ({ id: option.value, text: option.textContent.trim() })),
    sourceText: text(".source-ribbon"),
    bannerTitle: text("#banner-title"),
    bannerDetail: text("#banner-detail"),
    sourceFacts: text("#source-facts"),
    kicker: text("#inspector-kicker"),
    title: text("#inspector-title"),
    inspectorExcerpt: text("#inspector-content").slice(0, 4000),
    agreementText: [...document.querySelectorAll("#inspector-content > .inspector-note")].filter(visible).map((element) => element.innerText.trim()),
    transactionLinks: [...document.querySelectorAll("#inspector-content .chip-list button")].filter(visible).map((element) => ({ hash: element.title, text: element.innerText.trim() })),
    cards,
  };
}

export async function runCheck(page, check, outputDir, timeoutMs = 180_000) {
  const result = { name: check.name, url: check.url, pass: false, observed: {} };
  try {
    if (check.reload) {
      assert.equal(page.url(), check.url, "reload must start on the deep link");
      await page.reload({ waitUntil: "domcontentloaded", timeout: 60_000 });
    } else {
      await page.goto(check.url, { waitUntil: "domcontentloaded", timeout: 60_000 });
    }
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      result.observed = await page.evaluate(readRenderedPage);
      try {
        validateObservation(check, result.observed);
        break;
      } catch (error) {
        if (Date.now() >= deadline) throw error;
        await delay(1000);
      }
    }
    result.pass = true;
  } catch (error) {
    result.error = error.message;
    try { result.observed = await page.evaluate(readRenderedPage); } catch { /* Retain the last readable DOM. */ }
  }
  try {
    const screenshot = `${check.name}.png`;
    await page.screenshot({ path: join(outputDir, screenshot), fullPage: true, timeout: 20_000 });
    result.screenshot = screenshot;
  } catch (error) {
    result.pass = false;
    result.screenshotError = error.message;
  }
  return result;
}

async function main() {
  const outputDir = join(dirname(fileURLToPath(import.meta.url)), "results");
  await mkdir(outputDir, { recursive: true });
  const report = {
    startedAt: new Date().toISOString(),
    pass: false,
    nativeRequestRoute: "#/request/<id> is implemented by explorer/app.js using native-receipts.js",
    checks: CHECKS.map((check) => ({ name: check.name, url: check.url, pass: false, error: "Not run", observed: {} })),
    browserErrors: [],
  };
  const save = () => writeFile(join(outputDir, "result.json"), `${JSON.stringify(report, null, 2)}\n`);
  await save();
  let browser;
  try {
    const { chromium } = await import("playwright");
    browser = await chromium.launch({ headless: true });
    const page = await browser.newPage({ viewport: { width: 1440, height: 1000 }, locale: "en-US" });
    page.on("pageerror", (error) => {
      if (report.browserErrors.length < 50) report.browserErrors.push({ url: page.url(), message: error.message });
    });
    for (const [index, check] of CHECKS.entries()) {
      report.checks[index] = await runCheck(page, check, outputDir);
      await save();
      console.log(`${report.checks[index].pass ? "PASS" : "FAIL"} ${check.name}: ${report.checks[index].error ?? check.url}`);
    }
    report.pass = report.checks.every((check) => check.pass);
  } catch (error) {
    report.error = error.message;
  } finally {
    try { await browser?.close(); } catch (error) { report.error = error.message; report.pass = false; }
    report.finishedAt = new Date().toISOString();
    await save();
  }
  if (!report.pass) process.exitCode = 1;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await main();
}
