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
const HISTORY_DOMAIN = "all canonical 0x25 reward domains since the v3 recovery boundary; historical rows retain their own recovery_epoch, validator_set_id, and transaction_domain";
const rewardReceipt = (char, overrides = {}) => ({
  tx_type: "0x25",
  tx_hash: hex(char),
  job_id: hex(char === "f" ? "e" : (parseInt(char, 16) + 1).toString(16)),
  worker: hex("a"),
  block_height: H + parseInt(char, 16),
  block_hash: hex(char === "e" ? "d" : (parseInt(char, 16) + 1).toString(16)),
  submitted: true,
  included: true,
  confirmed: true,
  success: true,
  receipt_url: `/community/reward_receipt/0x${hex(char)}`,
  reward_base: 2_500_000_000,
  reward_arc: 2.5,
  ...overrides,
});
const projectionWindow = {
  observed_window_first_timestamp_ms: 1_700_000_000_000,
  observed_window_last_timestamp_ms: 1_700_086_400_000,
};
const readyProjectionBody = (count = 3, overrides = {}) => {
  const address = overrides.address ?? hex("a");
  const confirmed_receipts = ["1", "3", "5"].slice(0, count).map((char) => rewardReceipt(char, { worker: address }));
  return {
    address,
    confirmed_receipt_count: count,
    confirmed_receipts,
    confirmed_gross_earnings_arc: count * 2.5,
    projected_daily_arc: 7.5,
    projected_daily_unavailable_reason: null,
    community_rewards_v1_enabled: true,
    community_rewards_v1_protocol_active: true,
    community_rewards_v1_approval_collection_ready: true,
    archive_mode: true,
    history_complete_since_recovery: true,
    history_scope: "complete canonical reward history since the v3 recovery boundary",
    history_domain: HISTORY_DOMAIN,
    ...projectionWindow,
    ...overrides,
  };
};
const forkArchive = {
  schema: "arc.legacy-archive.source.v1", readOnly: true,
  classification: "valid_noncanonical_fork", captureId: hex("1"), node: "nyc",
  rolloutManifestSha256: hex("2"), archiveManifestSha256: hex("3"),
  completeSha256: hex("4"), bundleSha256: hex("5"), inventorySha256: hex("6"),
  bindingIndexSha256: hex("7"), bindingSha256: hex("8"), checkpointSha256: hex("9"),
  checkpointManifestHash: hex("a"), checkpointPayloadHash: hex("b"),
  canonicalCheckpointHeight: H, sourceHeight: H + 10,
  sourceBlockHash: hex("c"), sourceStateRoot: hex("d"), provenancePath: "/provenance",
};
function makeResolver() {
  return network.createCanonicalResolver({
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
      v3SourceId: "v3-a",
    },
    sources: [
      { id: "legacy", name: "Legacy", kind: "legacy-canonical", baseUrl: "https://legacy.example.test" },
      { id: "v3-a", name: "v3 A", kind: "v3", replicaGroup: "main", baseUrl: "https://v3-a.example.test" },
      { id: "v3-b", name: "v3 B", kind: "v3", replicaGroup: "main", baseUrl: "https://v3-b.example.test" },
      { id: "fork", name: "Fork", kind: "legacy-fork", baseUrl: "https://fork.example.test", archive: forkArchive },
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
    last_block_height: H + 11,
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

await test("active publication requires six reachable agreeing commitments and advancing liveness", () => {
  const maintenance = { state: "healthy", samples: Array.from({ length: 6 }, () => ({ ok: true })) };
  const samples = Array.from({ length: 6 }, (_, index) => ({ source: { id: `v3-${index + 1}` }, reachable: true }));
  const commitments = samples.map((sample) => ({ sourceId: sample.source.id, ok: true, height: 700, blockHash: hex("7"), stateRoot: hex("8") }));
  const fleet = {
    state: "healthy",
    samples,
    reachable: samples,
    current: { reachable: true, liveness: { state: "advancing" } },
    commonHeight: 700,
    commonAudit: network.auditCommonHeight(commitments),
    commitments,
    replicaCount: 6,
  };
  assert.equal(fleet.commonAudit.samples.length, 6);
  assert.equal(app.activeFleetPublicationError({ state: "recovered" }, fleet, maintenance), null);
  assert.match(app.activeFleetPublicationError({ state: "recovered" }, fleet, { state: "maintenance", samples: [] }), /maintenance interlocks/);

  const unknownCommitment = { ...fleet, commonAudit: { state: "unknown" } };
  assert.match(app.activeFleetPublicationError({ state: "recovered" }, unknownCommitment, maintenance), /commitments must agree/);

  const unknownLiveness = { ...fleet, current: { reachable: true, liveness: { state: "unknown" } } };
  assert.match(app.activeFleetPublicationError({ state: "recovered" }, unknownLiveness, maintenance), /advancing liveness/);

  const missingReplica = { ...fleet, reachable: fleet.reachable.slice(1) };
  assert.match(app.activeFleetPublicationError({ state: "recovered" }, missingReplica, maintenance), /all six validator health snapshots/);

  assert.equal(app.activeFleetPublicationError({ state: "degraded" }, { ...fleet, state: "degraded" }, maintenance), null);
  assert.match(app.activeFleetPublicationError({ state: "recovered" }, { ...fleet, state: "degraded" }, maintenance), /healthy fleet/);
});

await test("a commitment audit that compared fewer than six replicas cannot publish", async () => {
  const ids = ["v3-1", "v3-2", "v3-3", "v3-4", "v3-5", "v3-6"];
  const resolver = network.createCanonicalResolver({
    schema: "arc.frontend.network.v1",
    state: "recovered",
    network: { name: "ARC Testnet", chainId: "arc-testnet-v3" },
    checkpoint: {
      height: H, recoveryHeight: H + 1, legacyPublicMaxHeight: H + 10,
      blockHash: hex("a"), stateRoot: hex("b"), manifestHash: hex("c"),
      boundaryBlockHash: hex("d"), boundaryStateRoot: hex("e"), recoveryDomain: hex("f"),
      recoveryEpoch: 7, validatorSetId: 9, protocolVersion: "3.0.0",
      legacySourceId: "legacy", v3SourceId: "v3-1",
    },
    sources: [
      { id: "legacy", name: "Legacy", kind: "legacy-canonical", baseUrl: "https://legacy.example.test" },
      ...ids.map((id) => ({ id, name: id, kind: "v3", replicaGroup: "main", baseUrl: `https://${id}.example.test` })),
    ],
  });

  // Every replica is reachable and advancing and answers /block/<common> with
  // HTTP 200, but only two serve a body that carries a comparable commitment.
  const routes = {};
  for (const [index, id] of ids.entries()) {
    routes[`https://${id}.example.test/health`] = { body: { height: 101, chain_advancing: true, last_block_age_secs: 4 } };
    routes[`https://${id}.example.test/info`] = { body: { block_height: 101 } };
    routes[`https://${id}.example.test/block/latest`] = { body: { header: { height: 101, hash: hex("1"), state_root: hex("2"), timestamp: 2_000 } } };
    routes[`https://${id}.example.test/block/101`] = index < 2
      ? { body: { header: { height: 101, hash: hex("1"), state_root: hex("2") } } }
      : { body: { note: "block body without a commitment" } };
  }

  const fleet = await app.collectFleetHealth({ resolver, fetchImpl: mockFetch(routes), nowMs: 2_005_000 });
  assert.equal(fleet.commitments.length, 6);
  assert.ok(fleet.commitments.every((entry) => entry.ok), "every replica answered the common-height query");
  assert.equal(fleet.commonAudit.state, "consistent");
  assert.equal(fleet.commonAudit.samples.length, 2, "only two commitments were actually comparable");

  const maintenance = { state: "healthy", samples: ids.map(() => ({ ok: true })) };
  assert.match(
    app.activeFleetPublicationError(resolver.config, fleet, maintenance),
    /commitments must agree/,
    "a six-validator agreement claim must not be published from a two-replica comparison",
  );
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
    { schema: "arc.inference.activity.v1", record_kind: "mined_inference_attestation", source: "chain_receipt", mined: true, receipt_status: "success", success: true, computed: true, paid: false, earned: false, tx_type: "InferenceAttestation", tx_type_code: "0x16", block_height: H + 8, block_hash: hex("8"), index: 0, tx_hash: hex("7") },
    { schema: "arc.inference.activity.v1", record_kind: "mined_community_inference_reward", source: "chain_receipt", mined: true, receipt_status: "success", submitted: true, included: true, confirmed: true, success: true, computed: true, paid: true, earned: true, tx_type: "CommunityInferenceReward", tx_type_code: "0x25", worker: hex("6"), job_id: hex("b"), block_height: H + 7, block_hash: hex("c"), index: 0, tx_hash: hex("6"), reward_base: 2_500_000_000, reward_arc: 2.5, receipt_url: `/community/reward_receipt/0x${hex("6")}`, payment: { status: "earned", receipt_backed: true, reward_base: 2_500_000_000, reward_arc: 2.5 } },
    { schema: "arc.inference.activity.v1", record_kind: "mined_community_inference_reward", source: "chain_receipt", mined: true, receipt_status: "failed", submitted: true, included: true, confirmed: false, success: false, computed: false, paid: false, earned: false, tx_type: "CommunityInferenceReward", tx_type_code: "0x25", worker: hex("6"), block_height: H + 6, tx_hash: hex("5"), receipt_url: `/community/reward_receipt/0x${hex("5")}` },
    { schema: "arc.inference.activity.v1", record_kind: "mined_community_inference_reward", source: "chain_receipt", mined: true, receipt_status: "success", submitted: true, included: true, confirmed: false, success: true, computed: true, paid: true, earned: true, tx_type: "CommunityInferenceReward", tx_type_code: "0x25", worker: hex("6"), block_height: H + 6, tx_hash: hex("a"), receipt_url: `/community/reward_receipt/0x${hex("a")}` },
    { schema: "arc.inference.activity.v1", record_kind: "inference_observation", source: "local", mined: false, receipt_status: "absent", tx_type: "InferenceAttestation", block_height: H + 9, tx_hash: hex("8") },
    { schema: "arc.inference.activity.v1", record_kind: "mined_inference_attestation", source: "chain_receipt", mined: true, receipt_status: "success", success: true, computed: true, paid: false, earned: false, tx_type: "InferenceAttestation", tx_type_code: "0x16", block_height: H - 1, tx_hash: hex("9") },
    { schema: "arc.inference.activity.v1", record_kind: "mined_community_inference_reward", source: "chain_receipt", mined: true, receipt_status: "success", success: true, computed: true, paid: true, earned: true, tx_type: "InferenceRewardBogus", tx_type_code: "0x25", block_height: H + 5, tx_hash: hex("4") },
    { schema: "arc.inference.activity.v1", record_kind: "mined_community_inference_reward", source: "chain_receipt", mined: true, receipt_status: "success", success: true, computed: true, paid: true, earned: true, tx_type: "CommunityRewardPreview", tx_type_code: "0x25", block_height: H + 4, tx_hash: hex("3") },
    { schema: "arc.inference.activity.v1", record_kind: "mined_community_inference_reward", source: "chain_receipt", mined: true, receipt_status: "success", success: true, computed: true, paid: true, earned: true, tx_type: "CommunityInferenceReward", tx_type_code: "0x25", block_height: H + 3 },
    { schema: "arc.inference.activity.v1", record_kind: "mined_community_inference_reward", source: "chain_receipt", mined: true, receipt_status: "success", success: true, computed: true, paid: true, earned: true, tx_type: "CommunityInferenceReward", tx_type_code: "0x25", block_height: H + 2, tx_hash: "not-a-hash" },
  ];
  const fetchImpl = mockFetch({ "https://v3-a.example.test/inference/attestations?limit=50": { body: { activities: rows, attestations: [] } } });
  const result = await app.loadInferenceEvidence({ resolver, fetchImpl, checkpointAudit: { state: "verified" } });
  assert.equal(result.confirmed.length, 2);
  assert.equal(result.confirmed[0].receipt.txHash, hex("7"));
  assert.equal(result.confirmed[1].receipt.paymentConfirmed, true);
  assert.equal(result.excluded, 8);
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
    { ...readyProjectionBody(), onchain_balance_arc: 12.5, attestations_per_day_observed: 3, reward_per_attestation_arc: 2.5 },
    { attestation_reward_arc: 2.5 },
  );
  assert.equal(normalized.balance, 12.5);
  assert.equal(normalized.totalRewards, 3);
  assert.equal(normalized.confirmedGross, 7.5);
  assert.equal(normalized.projectedPerDay, 7.5);
  assert.equal(normalized.readiness, "ready");
});

await test("mined reward ARC and projection fail closed without exact successful 0x25 receipts", () => {
  const base = readyProjectionBody();
  const valid = app.normalizeWorkerEarnings(base, {});
  assert.equal(valid.confirmedGross, 7.5);
  assert.equal(valid.projectedPerDay, 7.5);

  for (const confirmed_receipts of [
    [rewardReceipt("1"), rewardReceipt("3", { success: false }), rewardReceipt("5")],
    [rewardReceipt("1"), rewardReceipt("3", { worker: hex("b") }), rewardReceipt("5")],
    [rewardReceipt("1"), rewardReceipt("3", { submitted: false }), rewardReceipt("5")],
    [rewardReceipt("1"), rewardReceipt("3", { included: false }), rewardReceipt("5")],
    [rewardReceipt("1"), rewardReceipt("3", { confirmed: false }), rewardReceipt("5")],
    [rewardReceipt("1"), rewardReceipt("3", { receipt_url: `/community/reward_receipt/0x${hex("9")}` }), rewardReceipt("5")],
    [rewardReceipt("1"), rewardReceipt("3", { tx_type: "0x16" }), rewardReceipt("5")],
    [rewardReceipt("1"), rewardReceipt("3", { reward_base: 2_499_999_999 }), rewardReceipt("5")],
    [rewardReceipt("1"), rewardReceipt("3", { reward_arc: 2 }), rewardReceipt("5")],
    [rewardReceipt("1"), rewardReceipt("3", { tx_hash: hex("1") }), rewardReceipt("5")],
    [rewardReceipt("1"), rewardReceipt("3", { job_id: rewardReceipt("1").job_id }), rewardReceipt("5")],
    [rewardReceipt("1"), rewardReceipt("3", { block_hash: "not-a-hash" }), rewardReceipt("5")],
    [rewardReceipt("1"), rewardReceipt("3", { block_height: Number.MAX_SAFE_INTEGER + 1 }), rewardReceipt("5")],
  ]) {
    const rejected = app.normalizeWorkerEarnings({ ...base, confirmed_receipts }, {});
    assert.equal(rejected.totalRewards, null);
    assert.equal(rejected.confirmedGross, null);
    assert.equal(rejected.projectedPerDay, null);
  }

  const forgedButInternallyConsistent = app.normalizeWorkerEarnings({
    ...base,
    confirmed_receipt_count: 2,
    confirmed_receipts: [
      rewardReceipt("1", { reward_base: 999, reward_arc: 999 }),
      rewardReceipt("3", { reward_base: 999, reward_arc: 999 }),
    ],
    confirmed_gross_earnings_arc: 1_998,
    projected_daily_arc: 12_345,
  }, {});
  assert.equal(forgedButInternallyConsistent.totalRewards, null);
  assert.equal(forgedButInternallyConsistent.confirmedGross, null);
  assert.equal(forgedButInternallyConsistent.projectedPerDay, null);
});

await test("archive labels require the exact canonical recovery-history contract", () => {
  const valid = app.normalizeWorkerEarnings(readyProjectionBody(), {});
  assert.equal(valid.archiveMode, true);
  assert.equal(valid.historyContractConsistent, true);
  for (const overrides of [
    { history_complete_since_recovery: false },
    { history_scope: "retained window" },
    { archive_mode: true, history_scope: undefined },
    { history_domain: "current recovery epoch only" },
    { history_domain: undefined },
  ]) {
    const degraded = app.normalizeWorkerEarnings(readyProjectionBody(3, overrides), {});
    assert.equal(degraded.archiveMode, false);
    assert.equal(degraded.historyContractConsistent, false);
    assert.equal(degraded.projectedPerDay, null);
    assert.match(degraded.projectionReason, /history-scope contract/);
  }
});

await test("archive lifetime earnings preserve exact receipts across recovery epochs", () => {
  const body = readyProjectionBody();
  body.confirmed_receipts[0].recovery_epoch = 1;
  body.confirmed_receipts[0].validator_set_id = 3;
  body.confirmed_receipts[1].recovery_epoch = 7;
  body.confirmed_receipts[1].validator_set_id = 9;
  const normalized = app.normalizeWorkerEarnings(body, {});
  assert.equal(normalized.totalRewards, 3);
  assert.equal(normalized.confirmedGross, 7.5);
});

await test("numeric projections require issuance, protocol, and approval readiness", () => {
  const ready = readyProjectionBody();
  for (const overrides of [
    { community_rewards_v1_enabled: false },
    { community_rewards_v1_protocol_active: false },
    { community_rewards_v1_approval_collection_ready: false },
    { community_rewards_v1_approval_collection_ready: undefined },
  ]) {
    const rejected = app.normalizeWorkerEarnings({ ...ready, ...overrides }, {});
    assert.equal(rejected.projectedPerDay, null);
    assert.notEqual(rejected.readiness, "ready");
  }
});

await test("numeric projections stay null at zero, one, or two exact receipts", () => {
  for (const count of [0, 1, 2]) {
    const normalized = app.normalizeWorkerEarnings(readyProjectionBody(count), {});
    assert.equal(normalized.receiptEvidenceConsistent, true);
    assert.equal(normalized.totalRewards, count);
    assert.equal(normalized.projectedPerDay, null);
    assert.match(normalized.projectionReason, /at least 3 successful mined reward receipts/);
  }
});

await test("omitted or optimistic receipt summaries cannot unlock a projection", () => {
  const omitted = readyProjectionBody();
  delete omitted.confirmed_receipt_count;
  const omittedResult = app.normalizeWorkerEarnings(omitted, {});
  assert.equal(omittedResult.projectedPerDay, null);
  assert.match(omittedResult.projectionReason, /receipt evidence is unavailable or internally inconsistent/);

  const optimistic = app.normalizeWorkerEarnings({
    ...readyProjectionBody(2),
    confirmed_receipt_count: 3,
    confirmed_gross_earnings_arc: 7.5,
  }, {});
  assert.equal(optimistic.projectedPerDay, null);
  assert.match(optimistic.projectionReason, /receipt evidence is unavailable or internally inconsistent/);
});

await test("numeric projections require three exact receipts spanning a full day", () => {
  for (const overrides of [
    { observed_window_first_timestamp_ms: undefined },
    { observed_window_last_timestamp_ms: undefined },
    { observed_window_last_timestamp_ms: projectionWindow.observed_window_first_timestamp_ms + 86_399_999 },
    { observed_window_first_timestamp_ms: Number.MAX_SAFE_INTEGER + 1 },
  ]) {
    const normalized = app.normalizeWorkerEarnings(readyProjectionBody(3, overrides), {});
    assert.equal(normalized.projectedPerDay, null);
    assert.match(normalized.projectionReason, /valid confirmed-receipt window spanning at least 24 hours/);
  }
  assert.equal(app.normalizeWorkerEarnings(readyProjectionBody(), {}).projectedPerDay, 7.5);
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
  const worker = hex("1");
  const fetchImpl = mockFetch({
    [`https://v3-a.example.test/worker/earnings/${worker}`]: { body: { ...readyProjectionBody(2, { address: worker }), onchain_balance_arc: 4, total_rewards: 2, confirmed_gross_earnings_base: 5_000_000_000, confirmed_gross_earnings_arc: 5, projected_daily_arc: null, projected_daily_unavailable_reason: "insufficient observations" } },
    "https://v3-a.example.test/economics/rewards": { body: { attestation_reward_arc: 2.5 } },
  }, calls);
  const result = await app.loadWorkerEarnings({ resolver: makeResolver(), fetchImpl, workerId: `0x${worker}`, checkpointAudit: { state: "verified" } });
  assert.equal(result.source.id, "v3-a");
  assert.equal(result.balance, 4);
  assert.equal(result.totalRewards, 2);
  assert.equal(result.confirmedGross, 5);
  assert.ok(calls.every((call) => call.options.method === "GET"));
  assert.ok(calls.every((call) => call.url.startsWith("https://v3-a.example.test/")));
});

await test("worker lookup rejects a missing or mismatched response identity", async () => {
  const worker = hex("1");
  for (const address of [undefined, hex("2")]) {
    const body = readyProjectionBody(2, { address: address ?? worker });
    if (address === undefined) delete body.address;
    const fetchImpl = mockFetch({
      [`https://v3-a.example.test/worker/earnings/${worker}`]: { body },
      "https://v3-a.example.test/economics/rewards": { body: {} },
    });
    await assert.rejects(
      app.loadWorkerEarnings({ resolver: makeResolver(), fetchImpl, workerId: worker, checkpointAudit: { state: "verified" } }),
      /not bound to the requested worker address/,
    );
  }
});

await test("worker IDs are validated before path construction", () => {
  assert.match(app.validateWorkerId("../../health").error, /32-byte ARC worker address/);
  assert.deepEqual(app.validateWorkerId(`0x${hex("a")}`), { value: hex("a") });
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

await test("checked-in config is honest maintenance or a complete active recovery inventory", () => {
  const normalized = network.normalizeConfig(defaultConfig);
  if (normalized.state === "maintenance") {
    assert.equal(normalized.checkpoint, null);
    assert.deepEqual(normalized.sources, []);
    return;
  }

  assert.ok(["recovered", "degraded"].includes(normalized.state));
  assert.ok(normalized.checkpoint, "active config must bind the recovery checkpoint");
  const replicas = network.createCanonicalResolver(normalized).v3Replicas();
  assert.equal(replicas.length, 6, "active config must publish the complete six-validator v3 fleet");
  assert.equal(new Set(replicas.map((source) => source.baseUrl)).size, 6);
});

await test("retired, raw, and mutating inference endpoints are absent", () => {
  const combined = `${html}\n${source}`;
  assert.doesNotMatch(combined, /(?:149\.28\.32\.76|140\.82\.16\.112|136\.244\.109\.1|104\.238\.171\.11|202\.182\.107\.41|149\.28\.153\.31|139\.84\.237\.49|216\.238\.120\.27)/);
  assert.doesNotMatch(combined, /\/community\/list|\/inference\/run(?:_consensus)?|:10000|:3001/);
  assert.doesNotMatch(source, /method:\s*["']POST["']/);
});

await test("copy distinguishes reachability, fork agreement, observation, receipt, earnings, and projection", () => {
  for (const phrase of ["Reachability, freshness", "same-height commitment", "Local observations", "successful mined reward receipt", "MINED REWARD ARC", "Successful retained 0x25 receipts only", "Projection, not earned ARC", "Pending is never earned"]) assert.ok(html.includes(phrase), `missing disclosure: ${phrase}`);
  assert.match(source, /formatArc\(result\.confirmedGross\)/);
  assert.match(source, /result\.totalRewards/);
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
