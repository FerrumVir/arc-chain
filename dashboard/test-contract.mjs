#!/usr/bin/env node

import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const app = require(join(here, "app.js"));
const network = require(join(here, "../shared/frontend/arc-network.js"));
const html = readFileSync(join(here, "index.html"), "utf8");
const source = readFileSync(join(here, "app.js"), "utf8");
const css = readFileSync(join(here, "app.css"), "utf8");
const defaultConfig = JSON.parse(readFileSync(join(here, "../shared/frontend/arc-network.json"), "utf8"));

const H = 500;
const hex = (char) => char.repeat(64);
function makeResolver() {
  return network.createCanonicalResolver({
    schema: "arc.frontend.network.v1",
    state: "recovered",
    network: { name: "ARC Testnet", chainId: "arc-testnet-v3" },
    checkpoint: {
      height: H,
      recoveryHeight: H + 1,
      blockHash: hex("a"),
      stateRoot: hex("b"),
      manifestHash: hex("c"),
      boundaryBlockHash: hex("d"),
      boundaryStateRoot: hex("e"),
      recoveryDomain: hex("f"),
      recoveryEpoch: 7,
      validatorSetId: 9,
      protocolVersion: "3.0.0",
      legacySourceId: "legacy",
      v3SourceId: "v3-a",
    },
    sources: [
      { id: "legacy", name: "Legacy", kind: "legacy-canonical", baseUrl: "https://legacy.example.test" },
      { id: "v3-a", name: "v3 A", kind: "v3", replicaGroup: "main", baseUrl: "https://v3-a.example.test" },
      { id: "v3-b", name: "v3 B", kind: "v3", replicaGroup: "main", baseUrl: "https://v3-b.example.test" },
      { id: "fork", name: "Fork", kind: "legacy-fork", baseUrl: "https://fork.example.test" },
    ],
  });
}

const response = (status, body) => ({ ok: status >= 200 && status < 300, status, json: async () => body });
function mockFetch(routes, calls = []) {
  return async (url, options) => {
    calls.push({ url, options });
    const parsed = new URL(url);
    const key = `${parsed.origin}${parsed.pathname}${parsed.search}`;
    const route = routes[key];
    return route ? response(route.status ?? 200, route.body) : response(404, { error: "not found" });
  };
}

function healthyFleetRoutes(options = {}) {
  const rootB = options.fork ? hex("9") : hex("2");
  return {
    "https://v3-a.example.test/health": { body: { height: 101, chain_advancing: true, last_block_age_secs: 4 } },
    "https://v3-a.example.test/info": { body: { block_height: 101, version: "v3" } },
    "https://v3-a.example.test/block/latest": { body: { header: { height: 101, hash: hex("1"), state_root: hex("2"), timestamp: 2_000 } } },
    "https://v3-b.example.test/health": { body: { height: 103, chain_advancing: true, last_block_age_secs: 5 } },
    "https://v3-b.example.test/info": { body: { block_height: 103, version: "v3" } },
    "https://v3-b.example.test/block/latest": { body: { header: { height: 103, hash: hex("3"), state_root: hex("4"), timestamp: 2_000 } } },
    "https://v3-a.example.test/block/101": { body: { header: { height: 101, hash: hex("1"), state_root: hex("2") } } },
    "https://v3-b.example.test/block/101": { body: { header: { height: 101, hash: hex("1"), state_root: rootB } } },
  };
}

function exactNetworkInfo(overrides = {}) {
  return {
    chain_id: "arc-testnet-v3",
    protocol_version: "3.0.0",
    recovery_active: true,
    recovery_epoch: 7,
    validator_set_id: 9,
    recovery_domain: hex("f"),
    checkpoint_manifest_hash: hex("c"),
    ...overrides,
  };
}

function recoveryAuditRoutes(overrides = {}) {
  const boundary = { header: { height: H + 1, parent_hash: hex("a"), hash: hex("d"), state_root: hex("e") } };
  return {
    "https://legacy.example.test/block/500": { body: { header: { height: H, hash: hex("a"), state_root: hex("b") } } },
    "https://v3-a.example.test/block/501": { body: boundary },
    "https://v3-b.example.test/block/501": { body: boundary },
    "https://v3-a.example.test/network/info": { body: exactNetworkInfo() },
    "https://v3-b.example.test/network/info": { body: exactNetworkInfo() },
    ...overrides,
  };
}

let count = 0;
async function test(name, fn) {
  await fn();
  count += 1;
  process.stdout.write(`ok ${count} - ${name}\n`);
}

await test("same-height matching v3 commitments produce a healthy fleet", async () => {
  const result = await app.collectFleetHealth({ resolver: makeResolver(), fetchImpl: mockFetch(healthyFleetRoutes()), nowMs: 2_005_000 });
  assert.equal(result.state, "healthy");
  assert.equal(result.commonHeight, 101);
  assert.equal(result.drift, 2);
  assert.equal(result.commonAudit.state, "consistent");
});

await test("same-height state-root disagreement confirms a fork", async () => {
  const result = await app.collectFleetHealth({ resolver: makeResolver(), fetchImpl: mockFetch(healthyFleetRoutes({ fork: true })), nowMs: 2_005_000 });
  assert.equal(result.state, "fork");
  assert.equal(result.commonAudit.state, "fork");
});

await test("unreachable configured replica degrades but does not fabricate a fork", async () => {
  const routes = healthyFleetRoutes();
  for (const key of Object.keys(routes)) if (key.includes("v3-b.example.test")) delete routes[key];
  const result = await app.collectFleetHealth({ resolver: makeResolver(), fetchImpl: mockFetch(routes), nowMs: 2_005_000 });
  assert.equal(result.state, "degraded");
  assert.equal(result.reachable.length, 1);
  assert.equal(result.commonAudit.state, "unknown");
});

await test("stale canonical source is reported as degraded", async () => {
  const routes = healthyFleetRoutes();
  routes["https://v3-a.example.test/health"].body = { height: 101, chain_advancing: false, last_block_age_secs: 9000 };
  const result = await app.collectFleetHealth({ resolver: makeResolver(), fetchImpl: mockFetch(routes), nowMs: 2_005_000 });
  assert.equal(result.state, "degraded");
  assert.equal(result.current.liveness.state, "stalled");
});

await test("recovery audit verifies exact H, H+1, and every replica identity", async () => {
  const resolver = makeResolver();
  const fetchImpl = mockFetch(recoveryAuditRoutes());
  assert.equal((await app.verifyRecoveryBoundary({ resolver, fetchImpl })).state, "verified");
});

await test("any replica recovery identity mismatch stays blocking", async () => {
  const resolver = makeResolver();
  const fetchImpl = mockFetch(recoveryAuditRoutes({
    "https://v3-b.example.test/network/info": { body: exactNetworkInfo({ recovery_epoch: 8 }) },
  }));
  assert.equal((await app.verifyRecoveryBoundary({ resolver, fetchImpl })).state, "mismatch");
});

await test("an unreachable configured replica leaves checkpoint status unknown", async () => {
  const routes = recoveryAuditRoutes();
  delete routes["https://v3-b.example.test/network/info"];
  assert.equal((await app.verifyRecoveryBoundary({ resolver: makeResolver(), fetchImpl: mockFetch(routes) })).state, "unknown");
});

await test("inference feed includes only successful canonical mined receipts", async () => {
  const resolver = makeResolver();
  const rows = [
    { schema: "arc.inference.activity.v1", record_kind: "mined_inference_attestation", source: "chain_receipt", mined: true, receipt_status: "success", success: true, tx_type: "InferenceAttestation", block_height: H + 8, tx_hash: hex("7") },
    { schema: "arc.inference.activity.v1", record_kind: "inference_observation", source: "local", mined: false, receipt_status: "absent", tx_type: "InferenceAttestation", block_height: H + 9, tx_hash: hex("8") },
    { schema: "arc.inference.activity.v1", record_kind: "mined_inference_attestation", source: "chain_receipt", mined: true, receipt_status: "success", success: true, tx_type: "InferenceAttestation", block_height: H - 1, tx_hash: hex("9") },
  ];
  const fetchImpl = mockFetch({ "https://v3-a.example.test/inference/attestations?limit=50": { body: { attestations: rows } } });
  const result = await app.loadInferenceEvidence({ resolver, fetchImpl, checkpointAudit: { state: "verified" } });
  assert.equal(result.confirmed.length, 1);
  assert.equal(result.confirmed[0].receipt.txHash, hex("7"));
  assert.equal(result.excluded, 2);
});

await test("missing worker earnings fields remain unavailable, never zero", () => {
  const normalized = app.normalizeWorkerEarnings({}, {});
  assert.equal(normalized.balance, null);
  assert.equal(normalized.totalRewards, null);
  assert.equal(normalized.projectedPerDay, null);
  assert.equal(normalized.readiness, "unknown");
});

await test("projection is accepted only from the authoritative earnings response", () => {
  const normalized = app.normalizeWorkerEarnings(
    { onchain_balance_arc: 12.5, confirmed_receipt_count: 1, confirmed_receipts: [{ tx_hash: hex("1") }], confirmed_gross_earnings_arc: 2.5, attestations_per_day_observed: 3, projected_daily_arc: 7.5, projected_daily_unavailable_reason: null, reward_per_attestation_arc: 2.5, community_rewards_v1_enabled: true, community_rewards_v1_protocol_active: true, community_rewards_v1_approval_collection_ready: true },
    { attestation_reward_arc: 2.5 },
  );
  assert.equal(normalized.balance, 12.5);
  assert.equal(normalized.totalRewards, 1);
  assert.equal(normalized.projectedPerDay, 7.5);
  assert.equal(normalized.readiness, "ready");
});

await test("observed rate times reward is never synthesized into a projection", () => {
  const normalized = app.normalizeWorkerEarnings({
    confirmed_receipt_count: 0,
    confirmed_receipts: [],
    confirmed_gross_earnings_arc: 0,
    attestations_per_day_observed: 3,
    reward_per_attestation_arc: 2.5,
    projected_daily_arc: null,
    projected_daily_unavailable_reason: "treasury proof unavailable",
  }, {});
  assert.equal(normalized.projectedPerDay, null);
  assert.equal(normalized.projectionReason, "treasury proof unavailable");
});

await test("worker lookup is pinned to canonical v3 source and read-only endpoints", async () => {
  const calls = [];
  const fetchImpl = mockFetch({
    "https://v3-a.example.test/worker/earnings/worker-1": { body: { onchain_balance_arc: 4, confirmed_receipt_count: 2, confirmed_receipts: [{}, {}], confirmed_gross_earnings_arc: 5, projected_daily_arc: null, projected_daily_unavailable_reason: "insufficient observations" } },
    "https://v3-a.example.test/economics/rewards": { body: { attestation_reward_arc: 2.5 } },
  }, calls);
  const result = await app.loadWorkerEarnings({ resolver: makeResolver(), fetchImpl, workerId: "worker-1", checkpointAudit: { state: "verified" } });
  assert.equal(result.source.id, "v3-a");
  assert.equal(result.balance, 4);
  assert.ok(calls.every((call) => call.options.method === "GET"));
  assert.ok(calls.every((call) => call.url.startsWith("https://v3-a.example.test/")));
});

await test("worker IDs are validated before path construction", () => {
  assert.match(app.validateWorkerId("../../health").error, /may contain/);
  assert.deepEqual(app.validateWorkerId("worker:abc_1"), { value: "worker:abc_1" });
});

await test("transaction lookup searches canonical segments and excludes preserved fork", async () => {
  const calls = [];
  const hash = hex("d");
  const fetchImpl = mockFetch({
    [`https://v3-a.example.test/tx/${hash}/full`]: { body: { tx_type: "Transfer", hash } },
    [`https://v3-a.example.test/tx/${hash}`]: { body: { status: "success", block_height: H + 4 } },
  }, calls);
  const result = await app.lookupTransaction({ resolver: makeResolver(), fetchImpl, hash, checkpointAudit: { state: "verified" } });
  assert.deepEqual(result.searched, ["v3-a", "legacy"]);
  assert.equal(result.occurrences[0].provenance.canonical, true);
  assert.ok(!calls.some((call) => call.url.includes("fork.example.test")));
});

await test("pending reward transaction is never counted as earned", async () => {
  const hash = hex("e");
  const fetchImpl = mockFetch({ [`https://v3-a.example.test/tx/${hash}/full`]: { body: { tx_type: "CommunityInferenceReward", hash, status: "pending" } } });
  const result = await app.lookupTransaction({ resolver: makeResolver(), fetchImpl, hash, checkpointAudit: { state: "verified" } });
  assert.equal(result.occurrences[0].classification.rewardEarned, false);
});

await test("dashboard loads shared resolver and external application code", () => {
  assert.ok(html.indexOf("../shared/frontend/arc-network.js") < html.indexOf("./app.js"));
  assert.match(html, /href="\.\/tailwind\.css"/);
  assert.match(html, /href="\.\/app\.css/);
  assert.equal((html.match(/<script(?![^>]*\bsrc=)/gi) || []).length, 0);
});

await test("production network config has one same-origin declaration", () => {
  assert.match(html, /name="arc-network-config" content="\.\.\/shared\/frontend\/arc-network\.json"/);
  assert.equal((html.match(/name="arc-network-config"/g) || []).length, 1);
});

await test("default checked-in config fails closed pending approved recovery metadata", () => {
  assert.equal(defaultConfig.state, "maintenance");
  assert.equal(defaultConfig.checkpoint, null);
  assert.deepEqual(defaultConfig.sources, []);
});

await test("retired, raw, and mutating inference endpoints are absent", () => {
  const combined = `${html}\n${source}`;
  assert.doesNotMatch(combined, /(?:149\.28\.32\.76|140\.82\.16\.112|136\.244\.109\.1|104\.238\.171\.11|202\.182\.107\.41|149\.28\.153\.31|139\.84\.237\.49|216\.238\.120\.27)/);
  assert.doesNotMatch(combined, /\/community\/list|\/inference\/run(?:_consensus)?|:10000|:3001/);
  assert.doesNotMatch(source, /method:\s*["']POST["']/);
});

await test("copy distinguishes reachability, fork agreement, observation, receipt, earnings, and projection", () => {
  for (const phrase of ["Reachability, freshness", "same-height commitment", "Local observations", "successful mined reward receipt", "Projection, not earned ARC", "Pending is never earned"]) assert.ok(html.includes(phrase), `missing disclosure: ${phrase}`);
});

await test("remote and user values never enter HTML injection sinks", () => {
  assert.doesNotMatch(source, /\.innerHTML\s*=|insertAdjacentHTML|document\.write\s*\(|\.outerHTML\s*=/);
  assert.match(source, /\.textContent\s*=/);
  assert.match(source, /\.replaceChildren\(\)/);
  assert.doesNotMatch(html, /\son[a-z]+\s*=/i);
});

await test("document has no duplicate IDs", () => {
  const ids = [...html.matchAll(/\sid="([^"]+)"/g)].map((match) => match[1]);
  const duplicates = [...new Set(ids.filter((id, index) => ids.indexOf(id) !== index))];
  assert.deepEqual(duplicates, []);
});

await test("production UI styles health, recovery, inference, earnings, and receipt states", () => {
  for (const selector of [".truth-banner.bad", ".timeline", ".source-grid", ".proof-panel", ".earnings-grid", ".receipt-card.canonical"]) assert.ok(css.includes(selector), `missing CSS selector ${selector}`);
});

process.stdout.write(`\nARC production dashboard contract: ${count}/${count} checks passed\n`);
