#!/usr/bin/env node

import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const explorer = require(join(here, "app.js"));
const html = readFileSync(join(here, "index.html"), "utf8");
const legacyHtml = readFileSync(join(here, "index-live.html"), "utf8");
const appSource = readFileSync(join(here, "app.js"), "utf8");

let checks = 0;
function check(description, callback) {
  callback();
  checks += 1;
  process.stdout.write(`ok ${checks} - ${description}\n`);
}

const expectedSources = [
  ["nyc", "http://149.28.32.76:9090"],
  ["lax", "http://140.82.16.112:9090"],
  ["ams", "http://136.244.109.1:9090"],
  ["lhr", "http://104.238.171.11:9090"],
  ["nrt", "http://202.182.107.41:9090"],
  ["sgp", "http://149.28.153.31:9090"],
];

check("ships exactly the six current seed sources", () => {
  assert.deepEqual(explorer.SOURCES.map(({ id, baseUrl }) => [id, baseUrl]), expectedSources);
});

check("does not retain the retired Africa seed", () => {
  assert.doesNotMatch(`${html}\n${appSource}`, /139\.84\.237\.49/);
});

check("does not retain the retired South America seed", () => {
  assert.doesNotMatch(`${html}\n${appSource}`, /216\.238\.120\.27/);
});

check("uses a source-relative URL builder", () => {
  assert.equal(explorer.buildRpcUrl("lhr", "/block/42"), "http://104.238.171.11:9090/block/42");
});

check("URL builder rejects cross-source absolute URLs", () => {
  assert.throws(() => explorer.buildRpcUrl("nyc", "http://example.test/health"), /source-relative/);
  assert.throws(() => explorer.buildRpcUrl("nyc", "//example.test/health"), /source-relative/);
});

check("URL builder rejects unknown sources", () => {
  assert.throws(() => explorer.buildRpcUrl("retired", "/health"), /Unknown ARC RPC source/);
});

check("normalizes prefixed hashes", () => {
  assert.equal(explorer.normalizeHex(`0x${"AB".repeat(32)}`), "ab".repeat(32));
});

check("rejects short and non-hex hashes", () => {
  assert.equal(explorer.normalizeHex("ab12"), null);
  assert.equal(explorer.normalizeHex("z".repeat(64)), null);
});

check("auto-classifies a decimal block height", () => {
  assert.deepEqual(explorer.classifyLookup("00123", "auto"), { kind: "block", value: "123" });
});

check("explicit block lookup rejects hash input", () => {
  assert.match(explorer.classifyLookup("ab".repeat(32), "block").error, /digits only/);
});

check("explicit transaction lookup classifies a hash", () => {
  assert.deepEqual(explorer.classifyLookup("ab".repeat(32), "tx"), { kind: "tx", value: "ab".repeat(32) });
});

check("explicit address lookup classifies a hash", () => {
  assert.deepEqual(explorer.classifyLookup("cd".repeat(32), "address"), { kind: "address", value: "cd".repeat(32) });
});

check("auto hash lookup remains intentionally ambiguous", () => {
  assert.deepEqual(explorer.classifyLookup("ef".repeat(32), "auto"), { kind: "lookup", value: "ef".repeat(32) });
});

check("empty lookup returns useful validation", () => {
  assert.match(explorer.classifyLookup("", "auto").error, /Enter a block height/);
});

check("fresh retained block is advancing", () => {
  const now = 2_000_000;
  assert.deepEqual(
    explorer.evaluateLiveness({}, { header: { timestamp: now - 15_000 } }, now),
    { state: "advancing", ageSecs: 15, basis: "selected source block timestamp (1800s freshness window)" },
  );
});

check("stale retained block is stalled", () => {
  const now = 10_000_000;
  assert.equal(explorer.evaluateLiveness({}, { header: { timestamp: now - 1_801_000 } }, now).state, "stalled");
});

check("future retained block makes liveness unknown", () => {
  const now = 10_000_000;
  assert.equal(explorer.evaluateLiveness({}, { header: { timestamp: now + 61_000 } }, now).state, "unknown");
});

check("missing timestamp makes liveness unknown", () => {
  assert.equal(explorer.evaluateLiveness({}, null, 10_000_000).state, "unknown");
});

check("new health liveness fields override fallback inference", () => {
  const result = explorer.evaluateLiveness(
    { chain_advancing: false, last_block_age_secs: 4000 },
    { header: { timestamp: 9_999_000 } },
    10_000_000,
  );
  assert.deepEqual(result, {
    state: "stalled",
    ageSecs: 4000,
    basis: "selected node /health block-liveness fields",
  });
});

check("reported height stays within one source snapshot", () => {
  assert.equal(explorer.reportedHeightFrom({ health: { height: 9 }, info: { block_height: 11 }, stats: { block_height: 10 } }), 11);
});

check("missing height stays unknown", () => {
  assert.equal(explorer.reportedHeightFrom({ health: {}, info: {}, stats: {} }), null);
});

check("recent blocks sort newest first without mutating input", () => {
  const input = [{ height: 2 }, { height: 9 }, { height: 4 }];
  assert.deepEqual(explorer.sortBlocksNewestFirst(input).map(({ height }) => height), [9, 4, 2]);
  assert.deepEqual(input.map(({ height }) => height), [2, 9, 4]);
});

check("block sort supports full block header shape", () => {
  assert.deepEqual(
    explorer.sortBlocksNewestFirst([{ header: { height: 3 } }, { header: { height: 8 } }]).map(({ header }) => header.height),
    [8, 3],
  );
});

check("large unpaginated address histories are deterministically bounded", () => {
  const bounded = explorer.boundedItems(["a", "b", "c", "d"], 2);
  assert.deepEqual(bounded, { visible: ["a", "b"], total: 4, truncated: true });
});

check("short histories are not marked truncated", () => {
  assert.deepEqual(explorer.boundedItems(["a"], 50), { visible: ["a"], total: 1, truncated: false });
});

check("duration formatter makes long stalls legible", () => {
  assert.equal(explorer.formatDuration(176_400), "2d 1h");
});

check("hash formatter never invents content", () => {
  assert.equal(explorer.formatHash("ab".repeat(32), 4, 4), `0xabab…abab`);
  assert.equal(explorer.formatHash(""), "Unknown");
});

check("entry page is standalone and has no missing TypeScript build entry", () => {
  assert.match(html, /\.\/app\.js/);
  assert.match(html, /\.\/styles\.css/);
  assert.doesNotMatch(html, /src\/main\.tsx|id="root"/);
});

check("entry page declares a restrictive content security policy", () => {
  assert.match(html, /Content-Security-Policy/);
  assert.match(html, /object-src 'none'/);
  assert.match(html, /connect-src http:\/\/149\.28\.32\.76:9090/);
});

check("all six seed endpoints are allowed by the CSP", () => {
  for (const [, baseUrl] of expectedSources) assert.match(html, new RegExp(baseUrl.replaceAll(".", "\\.")));
});

check("remote data renderer never uses HTML injection sinks", () => {
  assert.doesNotMatch(appSource, /\.innerHTML\s*=|insertAdjacentHTML|document\.write\s*\(|\.outerHTML\s*=/);
});

check("remote data renderer uses textContent and node replacement", () => {
  assert.match(appSource, /\.textContent\s*=/);
  assert.match(appSource, /\.replaceChildren\(\)/);
});

check("markup has no inline event handlers", () => {
  assert.doesNotMatch(html, /\son[a-z]+\s*=/i);
});

check("UI contains explicit block, transaction, and address lookup modes", () => {
  assert.match(html, /value="block"/);
  assert.match(html, /value="tx"/);
  assert.match(html, /value="address"/);
});

check("UI explicitly states that sources are not blended", () => {
  assert.match(html, /heights and state roots are never blended/i);
  assert.match(html, /No cross-node aggregation/i);
});

check("UI makes unknown and offline states first-class", () => {
  assert.match(appSource, /liveness unknown/i);
  assert.match(appSource, /is unreachable/i);
  assert.match(appSource, /retained or indexed history/i);
});

check("legacy explorer path redirects instead of running unsafe old code", () => {
  assert.match(legacyHtml, /url=\.\/index\.html/);
  assert.doesNotMatch(legacyHtml, /innerHTML|RPC_BENCH|bench-latest/);
});

process.stdout.write(`\nARC explorer contract: ${checks}/${checks} checks passed\n`);
