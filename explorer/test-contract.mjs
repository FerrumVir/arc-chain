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
const css = readFileSync(join(here, "styles.css"), "utf8");
const legacyHtml = readFileSync(join(here, "index-live.html"), "utf8");

const H = 88;
const hex = (char) => char.repeat(64);
const rewardActivity = (overrides = {}) => ({
  schema: "arc.inference.activity.v1",
  record_kind: "mined_community_inference_reward",
  source: "chain_receipt",
  mined: true,
  receipt_status: "success",
  success: true,
  computed: true,
  paid: true,
  earned: true,
  submitted: true,
  included: true,
  confirmed: true,
  tx_type: "CommunityInferenceReward",
  tx_type_code: "0x25",
  block_height: H + 2,
  block_hash: hex("8"),
  index: 0,
  tx_hash: hex("6"),
  job_id: hex("7"),
  worker: hex("9"),
  reward_base: 2_500_000_000,
  reward_arc: 2.5,
  receipt_url: `/community/reward_receipt/0x${hex("6")}`,
  payment: {
    status: "earned",
    receipt_backed: true,
    reward_base: 2_500_000_000,
    reward_arc: 2.5,
  },
  ...overrides,
});
const forkArchive = {
  schema: "arc.legacy-archive.source.v1",
  readOnly: true,
  classification: "valid_noncanonical_fork",
  captureId: hex("1"),
  node: "nyc",
  rolloutManifestSha256: hex("2"),
  archiveManifestSha256: hex("3"),
  completeSha256: hex("4"),
  bundleSha256: hex("5"),
  inventorySha256: hex("6"),
  bindingIndexSha256: hex("7"),
  bindingSha256: hex("8"),
  checkpointSha256: hex("9"),
  checkpointManifestHash: hex("a"),
  checkpointPayloadHash: hex("b"),
  canonicalCheckpointHeight: H,
  sourceHeight: H + 10,
  sourceBlockHash: hex("c"),
  sourceStateRoot: hex("d"),
  provenancePath: "/provenance",
};
const forkProvenance = {
  schema: "arc.legacy-archive.query.v1",
  read_only: true,
  classification: forkArchive.classification,
  capture_id: forkArchive.captureId,
  node: forkArchive.node,
  rollout_manifest_sha256: forkArchive.rolloutManifestSha256,
  archive_manifest_sha256: forkArchive.archiveManifestSha256,
  complete_sha256: forkArchive.completeSha256,
  bundle_sha256: forkArchive.bundleSha256,
  inventory_sha256: forkArchive.inventorySha256,
  binding_index_sha256: forkArchive.bindingIndexSha256,
  binding_sha256: forkArchive.bindingSha256,
  checkpoint_sha256: forkArchive.checkpointSha256,
  checkpoint_manifest_hash: forkArchive.checkpointManifestHash,
  checkpoint_payload_hash: forkArchive.checkpointPayloadHash,
  canonical_checkpoint_height: forkArchive.canonicalCheckpointHeight,
  source_height: forkArchive.sourceHeight,
  source_block_hash: forkArchive.sourceBlockHash,
  source_state_root: forkArchive.sourceStateRoot,
};
const resolver = network.createCanonicalResolver({
  schema: "arc.frontend.network.v1",
  state: "recovered",
  network: { name: "ARC Testnet", chainId: "arc-testnet-v3" },
  checkpoint: {
    height: H,
    recoveryHeight: H + 1,
    legacyPublicMaxHeight: H + 10,
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
    v3SourceId: "v3",
  },
  sources: [
    { id: "legacy", name: "Legacy", kind: "legacy-canonical", baseUrl: "https://legacy.example.test" },
    { id: "v3", name: "v3", kind: "v3", baseUrl: "https://v3.example.test" },
    { id: "fork", name: "Fork", kind: "legacy-fork", baseUrl: "https://fork.example.test", archive: forkArchive },
  ],
});

let count = 0;
async function test(name, fn) {
  await fn();
  count += 1;
  process.stdout.write(`ok ${count} - ${name}\n`);
}

const response = (status, payload) => ({ ok: status >= 200 && status < 300, status, json: async () => payload });
function mockFetch(routes, calls = []) {
  return async (url, options) => {
    calls.push({ url, options });
    const parsed = new URL(url);
    const key = `${parsed.origin}${parsed.pathname}${parsed.search}`;
    const route = routes[key];
    return route ? response(route.status ?? 200, route.body) : response(404, { error: "not found" });
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
    last_block_height: H + 11,
    ...overrides,
  };
}

const verifiedAudit = { state: "verified" };

await test("classifies decimal block heights", () => {
  assert.deepEqual(app.classifyLookup("00089", "auto"), { kind: "block", value: "89" });
  assert.match(app.classifyLookup("9007199254740992", "block").error, /outside the supported range/);
});

await test("rejects malformed transaction and address input", () => {
  assert.match(app.classifyLookup("not-a-hash", "tx").error, /32-byte/);
});

await test("keeps auto hash lookup intentionally ambiguous", () => {
  assert.deepEqual(app.classifyLookup(hex("d"), "auto"), { kind: "lookup", value: hex("d") });
});

await test("extracts and sorts block collections without mutating input", () => {
  const blocks = [{ header: { height: 2 } }, { header: { height: 9 } }, { header: { height: 4 } }];
  assert.deepEqual(app.extractBlocks({ blocks }).map(network.blockHeight), [9, 4, 2]);
  assert.deepEqual(blocks.map(network.blockHeight), [2, 9, 4]);
});

await test("inference activity consumes v3 0x25 activity rows before legacy attestations", () => {
  const paid = { record_kind: "mined_community_inference_reward", tx_type: "CommunityInferenceReward", tx_hash: hex("6") };
  assert.deepEqual(app.extractRows({ activities: [paid], attestations: [{ tx_hash: hex("7") }] }), [paid]);
  const classification = network.classifyReceipt(rewardActivity());
  assert.equal(classification.inferenceConfirmed, true);
  assert.equal(classification.paymentConfirmed, true);
  assert.equal(classification.rewardEarned, true);
});

await test("explorer activity rejects fuzzy reward names and absent or invalid transaction hashes", () => {
  const common = rewardActivity();
  for (const overrides of [
    { tx_type: "InferenceRewardBogus" },
    { tx_type: "CommunityRewardPreview" },
    { tx_hash: undefined },
    { tx_hash: "not-a-hash" },
    { worker: undefined },
    { job_id: "not-a-hash" },
    { block_hash: undefined },
    { index: -1 },
    { reward_base: 2_499_999_999 },
    { reward_arc: 2.49 },
  ]) {
    const classification = network.classifyReceipt({ ...common, ...overrides });
    assert.equal(classification.inferenceConfirmed, false);
    assert.equal(classification.paymentConfirmed, false);
    assert.equal(classification.rewardEarned, false);
  }
});

await test("reports the highest evidence within one source snapshot", () => {
  assert.equal(app.reportedHeight({ health: { height: 4 }, info: { block_height: 7 }, stats: { block_height: 6 } }), 7);
  assert.equal(app.reportedHeight({ health: { height: Number.MAX_SAFE_INTEGER + 1 }, info: { block_height: 7 } }), 7);
  assert.equal(app.reportedHeight({ health: { height: "9007199254740992" } }), null);
});

await test("raw u64 strings format exactly while unsafe JSON numbers fail closed", () => {
  assert.equal(app.formatExactInteger("18446744073709551615").replace(/\D/g, ""), "18446744073709551615");
  assert.equal(app.formatExactInteger(Number.MAX_SAFE_INTEGER + 1), "Unavailable");
  assert.equal(app.integerOrNull("9007199254740992"), null);
});

await test("canonical block lookup routes H to legacy", async () => {
  const calls = [];
  const fetchImpl = mockFetch({
    "https://legacy.example.test/block/88": { body: { header: { height: H, hash: hex("a"), state_root: hex("b") } } },
    "https://legacy.example.test/block/88/txs?offset=0&limit=100": { body: { tx_hashes: [] } },
  }, calls);
  const result = await app.queryBlock({ resolver, fetchImpl, height: H, sourceId: "canonical", checkpointAudit: verifiedAudit });
  assert.equal(result.route.sourceId, "legacy");
  assert.equal(result.route.segment, "signed-checkpoint");
  assert.ok(calls.every((call) => call.url.startsWith("https://legacy.example.test/")));
});

await test("canonical block lookup verifies the H+1 parent link on v3", async () => {
  const fetchImpl = mockFetch({
    "https://v3.example.test/block/89": { body: { header: { height: H + 1, hash: hex("d"), parent_hash: hex("a"), state_root: hex("e") } } },
    "https://v3.example.test/block/89/txs?offset=0&limit=100": { body: { tx_hashes: [] } },
  });
  const result = await app.queryBlock({ resolver, fetchImpl, height: H + 1, sourceId: "canonical", checkpointAudit: verifiedAudit });
  assert.equal(result.route.segment, "recovery-boundary");
  assert.equal(result.boundary.state, "verified");
});

await test("explicit fork block lookup stays non-canonical", async () => {
  const fetchImpl = mockFetch({
    "https://fork.example.test/provenance": { body: forkProvenance },
    "https://fork.example.test/block/80": { body: { header: { height: 80, hash: hex("f"), state_root: hex("1") } } },
  });
  const result = await app.queryBlock({ resolver, fetchImpl, height: 80, sourceId: "fork" });
  assert.equal(result.route.canonical, false);
  assert.equal(result.route.expectedCanonicalSourceId, "legacy");
  assert.equal(result.archiveVerification.state, "verified");
});

await test("explicit fork queries fail closed on archive provenance mismatch", async () => {
  const fetchImpl = mockFetch({
    "https://fork.example.test/provenance": { body: { ...forkProvenance, checkpoint_sha256: hex("f") } },
    "https://fork.example.test/block/80": { body: { header: { height: 80, hash: hex("f"), state_root: hex("1") } } },
  });
  await assert.rejects(
    app.queryBlock({ resolver, fetchImpl, height: 80, sourceId: "fork" }),
    /provenance rejected/,
  );
});

await test("canonical labels fail closed without a complete checkpoint audit", async () => {
  const fetchImpl = mockFetch({
    "https://legacy.example.test/block/88": { body: { header: { height: H, hash: hex("a"), state_root: hex("b") } } },
  });
  const result = await app.queryBlock({ resolver, fetchImpl, height: H, sourceId: "canonical" });
  assert.equal(result.route.canonical, false);
  assert.equal(result.route.configuredCanonical, true);
});

await test("full explorer checkpoint audit checks H, H+1, and network identity", async () => {
  const routes = {
    "https://legacy.example.test/block/88": { body: { header: { height: H, hash: hex("a"), state_root: hex("b") } } },
    "https://v3.example.test/block/89": { body: { header: { height: H + 1, parent_hash: hex("a"), hash: hex("d"), state_root: hex("e") } } },
    "https://v3.example.test/network/info": { body: exactNetworkInfo() },
  };
  assert.equal((await app.verifyRecoveryCheckpoint({ resolver, fetchImpl: mockFetch(routes) })).state, "verified");
  routes["https://v3.example.test/network/info"] = { body: exactNetworkInfo({ checkpoint_manifest_hash: hex("0") }) };
  assert.equal((await app.verifyRecoveryCheckpoint({ resolver, fetchImpl: mockFetch(routes) })).state, "mismatch");
});

await test("canonical transaction lookup searches both canonical segments but not forks", async () => {
  const calls = [];
  const hash = hex("2");
  const fetchImpl = mockFetch({
    [`https://legacy.example.test/tx/${hash}/full`]: { body: { tx_type: "Transfer", hash } },
    [`https://legacy.example.test/tx/${hash}`]: { body: { status: "success", block_height: H } },
  }, calls);
  const result = await app.queryTransaction({ resolver, fetchImpl, hash, sourceId: "canonical", checkpointAudit: verifiedAudit });
  assert.deepEqual(result.plannedSources, ["v3", "legacy"]);
  assert.equal(result.occurrences.length, 1);
  assert.equal(result.occurrences[0].provenance.canonical, true);
  assert.ok(!calls.some((call) => call.url.includes("fork.example.test")));
});

await test("a legacy source occurrence above H is shown as non-canonical", async () => {
  const hash = hex("3");
  const fetchImpl = mockFetch({
    [`https://legacy.example.test/tx/${hash}`]: { body: { status: "success", block_height: H + 3 } },
  });
  const result = await app.queryTransaction({ resolver, fetchImpl, hash, sourceId: "legacy", checkpointAudit: verifiedAudit });
  assert.equal(result.occurrences[0].provenance.canonical, false);
  assert.equal(result.occurrences[0].provenance.expectedCanonicalSourceId, "v3");
});

await test("a preserved fork exposes block inclusion when transaction details were pruned", async () => {
  const hash = hex("7");
  const fetchImpl = mockFetch({
    "https://fork.example.test/provenance": { body: forkProvenance },
    [`https://fork.example.test/tx/${hash}/occurrences`]: {
      body: {
        schema: "arc.legacy-archive.transaction-occurrences.v1",
        tx_hash: hash,
        unique_occurrence: true,
        receipt_retained: false,
        full_transaction_retained: false,
        occurrences: [{ block_height: H + 5, block_hash: hex("e"), index: 0 }],
      },
    },
  });
  const result = await app.queryTransaction({ resolver, fetchImpl, hash, sourceId: "fork", checkpointAudit: verifiedAudit });
  assert.equal(result.occurrences.length, 1);
  assert.equal(result.occurrences[0].classification.receiptBacked, false);
  assert.equal(result.occurrences[0].classification.rewardEarned, false);
  assert.equal(result.occurrences[0].occurrence.occurrences[0].block_height, H + 5);
  assert.equal(result.occurrences[0].provenance.canonical, false);
});

await test("pending reward submissions never appear earned", async () => {
  const hash = hex("4");
  const fetchImpl = mockFetch({
    [`https://v3.example.test/tx/${hash}/full`]: { body: { tx_type: "CommunityInferenceReward", hash, status: "submitted" } },
  });
  const result = await app.queryTransaction({ resolver, fetchImpl, hash, sourceId: "canonical", checkpointAudit: verifiedAudit });
  assert.equal(result.occurrences[0].classification.rewardEarned, false);
  assert.equal(result.occurrences[0].classification.receiptBacked, false);
});

await test("successful mined inference receipts expose canonical provenance", async () => {
  const hash = hex("5");
  const fetchImpl = mockFetch({
    [`https://v3.example.test/tx/${hash}/full`]: { body: { type: "InferenceAttestation", hash } },
    [`https://v3.example.test/tx/${hash}`]: { body: { receipt_status: "success", height: H + 9 } },
  });
  const result = await app.queryTransaction({ resolver, fetchImpl, hash, sourceId: "canonical", checkpointAudit: verifiedAudit });
  assert.equal(result.occurrences[0].classification.inferenceConfirmed, true);
  assert.equal(result.occurrences[0].provenance.canonical, true);
});

await test("address responses stay separated by source", async () => {
  const address = hex("6");
  const fetchImpl = mockFetch({
    [`https://v3.example.test/account/${address}`]: { body: { balance: 7, nonce: 1 } },
    [`https://legacy.example.test/account/${address}`]: { body: { balance: 4, nonce: 0 } },
  });
  const result = await app.queryAddress({ resolver, fetchImpl, address, sourceId: "canonical", checkpointAudit: verifiedAudit });
  assert.equal(result.records.length, 2);
  assert.deepEqual(result.records.map((record) => record.account.balance), [7, 4]);
});

await test("request helper performs only source-relative GET requests", async () => {
  const calls = [];
  const sourceConfig = resolver.source("v3");
  await app.requestJson(mockFetch({ "https://v3.example.test/health": { body: { ok: true } } }, calls), sourceConfig, "/health");
  assert.equal(calls[0].options.method, "GET");
  assert.equal(calls[0].options.cache, "no-store");
  await assert.rejects(app.requestJson(mockFetch({}), sourceConfig, "https://evil.example/health"), /source-relative/);
});

await test("lookup failures use their own abort controller and render the intended error", () => {
  assert.equal((source.match(/state\.lookupController = controller;/g) || []).length, 3);
  assert.doesNotMatch(source, /signal: state\.lookupController\.signal/);
  assert.doesNotMatch(source, /if \(!signal\.aborted\) inspectorError\("Block"/);
  assert.match(source, /if \(!controller\.signal\.aborted\) inspectorError\("Block", "Block unavailable", error\.message\)/);
});

await test("entry page loads shared resolver before explorer application", () => {
  assert.ok(html.indexOf("../shared/frontend/arc-network.js") < html.indexOf("./app.js"));
});

await test("entry page loads production network config from one meta declaration", () => {
  assert.match(html, /name="arc-network-config" content="\.\.\/shared\/frontend\/arc-network\.json"/);
  assert.equal((html.match(/name="arc-network-config"/g) || []).length, 1);
});

await test("content policy permits configurable HTTPS and loopback development only", () => {
  assert.match(html, /connect-src 'self' https: http:\/\/localhost:\* http:\/\/127\.0\.0\.1:\*/);
  assert.match(html, /object-src 'none'/);
});

await test("retired and raw seed endpoints are absent", () => {
  const combined = `${html}\n${source}`;
  assert.doesNotMatch(combined, /(?:149\.28\.32\.76|140\.82\.16\.112|136\.244\.109\.1|104\.238\.171\.11|202\.182\.107\.41|149\.28\.153\.31|139\.84\.237\.49|216\.238\.120\.27)/);
  assert.doesNotMatch(combined, /\/community\/list|:10000|:3001/);
});

await test("UI explicitly discloses canonical, boundary, fork, receipt, and reward semantics", () => {
  for (const phrase of ["H+1 recovery link", "non-canonical views", "Receipt-backed only", "not an earning", "No fork blending"]) assert.ok(html.toLowerCase().includes(phrase.toLowerCase()), `missing disclosure: ${phrase}`);
  assert.match(source, /at least 3 exact successful mined 0x25 receipts spanning 24 hours/);
  assert.doesNotMatch(source, /numberOrNull\(payload\?\.projected_daily_arc\)/);
});

await test("remote values are rendered without HTML injection sinks", () => {
  assert.doesNotMatch(source, /\.innerHTML\s*=|insertAdjacentHTML|document\.write\s*\(|\.outerHTML\s*=/);
  assert.match(source, /\.textContent\s*=/);
  assert.match(source, /\.replaceChildren\(\)/);
  assert.doesNotMatch(html, /\son[a-z]+\s*=/i);
});

await test("new continuity and evidence UI has explicit styling", () => {
  for (const selector of [".recovery-ribbon", ".continuity-track", ".activity-grid", ".occurrence-card", ".truth-bad"]) assert.ok(css.includes(selector));
});

await test("legacy explorer entry redirects to the composite explorer", () => {
  assert.match(legacyHtml, /url=\.\/index\.html/);
  assert.doesNotMatch(legacyHtml, /innerHTML|RPC_BENCH|bench-latest/);
});

process.stdout.write(`\nARC composite explorer contract: ${count}/${count} checks passed\n`);
