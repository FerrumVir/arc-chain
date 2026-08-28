#!/usr/bin/env node

import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const network = require(join(here, "arc-network.js"));

const H = 4219;
const hex = (byte) => byte.repeat(64);
const recovered = {
  schema: "arc.frontend.network.v1",
  state: "recovered",
  updatedAt: "2026-08-27T12:00:00Z",
  network: { name: "ARC Testnet", chainId: "arc-testnet-v3" },
  checkpoint: {
    height: H,
    recoveryHeight: H + 1,
    legacyPublicMaxHeight: H + 100,
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
    { id: "legacy", name: "Signed legacy archive", kind: "legacy-canonical", baseUrl: "https://legacy.example.test" },
    { id: "v3-a", name: "v3 canonical A", kind: "v3", replicaGroup: "v3-main", baseUrl: "https://v3-a.example.test" },
    { id: "v3-b", name: "v3 canonical B", kind: "v3", replicaGroup: "v3-main", baseUrl: "https://v3-b.example.test" },
    { id: "fork", name: "Preserved fork", kind: "legacy-fork", baseUrl: "https://fork.example.test" },
  ],
};

let count = 0;
function test(name, fn) {
  fn();
  count += 1;
  process.stdout.write(`ok ${count} - ${name}\n`);
}

test("normalizes a signed recovery configuration", () => {
  const config = network.normalizeConfig(recovered);
  assert.equal(config.checkpoint.height, H);
  assert.equal(config.sourcesById.get("fork").kind, "legacy-fork");
});

test("checked-in operator example conforms to the production schema", () => {
  const example = JSON.parse(readFileSync(join(here, "arc-network.example.json"), "utf8"));
  const normalized = network.normalizeConfig(example);
  assert.equal(normalized.checkpoint.recoveryHeight, normalized.checkpoint.height + 1);
  assert.equal(normalized.sourcesById.get("legacy-fork-preserved").kind, "legacy-fork");
});

test("fails closed when recovered mode has no checkpoint", () => {
  assert.throws(() => network.normalizeConfig({ ...recovered, checkpoint: null }), /requires a recovery checkpoint/);
});

test("requires H+1 as the recovery height", () => {
  const invalid = { ...recovered, checkpoint: { ...recovered.checkpoint, recoveryHeight: H + 2 } };
  assert.throws(() => network.normalizeConfig(invalid), /must equal checkpoint.height \+ 1/);
});

test("requires the sealed legacy public maximum at or above H", () => {
  const missing = structuredClone(recovered);
  delete missing.checkpoint.legacyPublicMaxHeight;
  assert.throws(() => network.normalizeConfig(missing), /legacyPublicMaxHeight/);

  const belowSource = structuredClone(recovered);
  belowSource.checkpoint.legacyPublicMaxHeight = H - 1;
  assert.throws(() => network.normalizeConfig(belowSource), /at least checkpoint.height/);
});

test("recovered configuration requires exact boundary and recovery identity commitments", () => {
  for (const field of ["boundaryBlockHash", "boundaryStateRoot", "recoveryDomain", "recoveryEpoch", "validatorSetId", "protocolVersion"]) {
    const invalid = structuredClone(recovered);
    delete invalid.checkpoint[field];
    assert.throws(() => network.normalizeConfig(invalid), new RegExp(`checkpoint\\.${field}`));
  }
});

test("one recovered v3 source may serve both retained legacy history and continuation", () => {
  const shared = structuredClone(recovered);
  shared.checkpoint.legacySourceId = "v3-a";
  shared.sources = shared.sources.filter((source) => source.id !== "legacy");
  const normalized = network.normalizeConfig(shared);
  const sharedResolver = network.createCanonicalResolver(normalized);
  assert.equal(sharedResolver.routeBlock(H).sourceId, "v3-a");
  assert.equal(sharedResolver.routeBlock(H + 1).sourceId, "v3-a");
});

test("rejects non-loopback clear-text RPCs", () => {
  const invalid = structuredClone(recovered);
  invalid.sources[0].baseUrl = "http://192.0.2.4:9090";
  assert.throws(() => network.normalizeConfig(invalid), /refuses clear-text/);
});

test("rejects duplicate endpoints that would fake independent sources", () => {
  const invalid = structuredClone(recovered);
  invalid.sources[1].baseUrl = invalid.sources[0].baseUrl;
  assert.throws(() => network.normalizeConfig(invalid), /duplicate source endpoint/);
});

test("allows loopback HTTP for local live-gated testing", () => {
  const local = structuredClone(recovered);
  local.sources[0].baseUrl = "http://127.0.0.1:9090";
  assert.equal(network.normalizeConfig(local).sources[0].baseUrl, "http://127.0.0.1:9090");
});

test("same-origin root endpoint builds a same-origin RPC path", () => {
  assert.equal(network.buildRpcUrl({ id: "gateway", baseUrl: "/" }, "/health"), "/health");
});

const config = network.normalizeConfig(recovered);
const resolver = network.createCanonicalResolver(config);

test("routes pre-checkpoint history to the signed legacy archive", () => {
  assert.deepEqual(
    (({ sourceId, segment, canonical }) => ({ sourceId, segment, canonical }))(resolver.routeBlock(H - 1)),
    { sourceId: "legacy", segment: "legacy-history", canonical: true },
  );
});

test("labels the exact signed checkpoint", () => {
  assert.equal(resolver.routeBlock(H).segment, "signed-checkpoint");
});

test("routes H+1 to v3 and exposes the recovery boundary", () => {
  const route = resolver.routeBlock(H + 1);
  assert.equal(route.sourceId, "v3-a");
  assert.equal(route.segment, "recovery-boundary");
});

test("routes later blocks to the v3 continuation", () => {
  assert.equal(resolver.routeBlock(H + 80).segment, "v3-continuation");
});

test("explicit fork queries remain non-canonical", () => {
  const route = resolver.routeBlock(H - 2, { sourceId: "fork" });
  assert.equal(route.ok, true);
  assert.equal(route.canonical, false);
  assert.equal(route.segment, "alternate-source");
  assert.match(route.warning, /not part of.*canonical/i);
});

test("canonical transaction lookups search v3 then signed legacy only", () => {
  assert.deepEqual(resolver.lookupSources().map((entry) => entry.sourceId), ["v3-a", "legacy"]);
  assert.ok(!resolver.lookupSources().some((entry) => entry.sourceId === "fork"));
});

test("an explicit alternate lookup queries only that source", () => {
  assert.deepEqual(resolver.lookupSources({ sourceId: "fork" }).map((entry) => entry.sourceId), ["fork"]);
});

test("canonical occurrence classification follows the block boundary", () => {
  assert.equal(resolver.classifyOccurrence("legacy", H).canonical, true);
  assert.equal(resolver.classifyOccurrence("legacy", H + 1).canonical, false);
  assert.equal(resolver.classifyOccurrence("v3-a", H + 1).canonical, true);
});

test("same-height identical commitments are consistent", () => {
  const audit = network.auditCommonHeight([
    { sourceId: "a", height: 10, blockHash: hex("1"), stateRoot: hex("2") },
    { sourceId: "b", height: 10, blockHash: hex("1"), stateRoot: hex("2") },
  ]);
  assert.equal(audit.state, "consistent");
});

test("same-height commitment disagreement is a fork", () => {
  const audit = network.auditCommonHeight([
    { sourceId: "a", height: 10, blockHash: hex("1"), stateRoot: hex("2") },
    { sourceId: "b", height: 10, blockHash: hex("3"), stateRoot: hex("2") },
  ]);
  assert.equal(audit.state, "fork");
});

test("different-height samples cannot be called consistent", () => {
  const audit = network.auditCommonHeight([
    { sourceId: "a", height: 10, blockHash: hex("1"), stateRoot: hex("2") },
    { sourceId: "b", height: 11, blockHash: hex("1"), stateRoot: hex("2") },
  ]);
  assert.equal(audit.state, "unknown");
});

test("reward earnings require a successful mined receipt", () => {
  const result = network.classifyReceipt({
    tx: { tx_type: "CommunityInferenceReward", hash: hex("d") },
    receipt: { status: "success", block_height: H + 3 },
  });
  assert.equal(result.rewardEarned, true);
  assert.equal(result.category, "reward");
});

test("submitted rewards are not earnings", () => {
  const result = network.classifyReceipt({ tx: { tx_type: "CommunityInferenceReward" }, status: "submitted" });
  assert.equal(result.rewardEarned, false);
  assert.equal(result.receiptBacked, false);
});

test("successful inference receipts are confirmed activity", () => {
  const result = network.classifyReceipt({
    transaction: { type: "InferenceAttestation", hash: hex("e") },
    receipt: { receipt_status: "success", height: H + 8 },
  });
  assert.equal(result.inferenceConfirmed, true);
});

test("H+1 parent linkage is independently verified", () => {
  const result = network.boundaryVerification(
    { header: { height: H + 1, parent_hash: recovered.checkpoint.blockHash, hash: recovered.checkpoint.boundaryBlockHash, state_root: recovered.checkpoint.boundaryStateRoot } },
    config.checkpoint,
  );
  assert.equal(result.state, "verified");
});

test("H+1 parent mismatch is never hidden", () => {
  const result = network.boundaryVerification(
    { header: { height: H + 1, parent_hash: hex("0"), hash: recovered.checkpoint.boundaryBlockHash, state_root: recovered.checkpoint.boundaryStateRoot } },
    config.checkpoint,
  );
  assert.equal(result.state, "mismatch");
});

test("signed H requires exact height, block hash, and state root", () => {
  assert.equal(network.checkpointVerification(
    { header: { height: H, hash: recovered.checkpoint.blockHash, state_root: recovered.checkpoint.stateRoot } },
    config.checkpoint,
  ).state, "verified");
  assert.equal(network.checkpointVerification(
    { header: { height: H, hash: recovered.checkpoint.blockHash, state_root: hex("0") } },
    config.checkpoint,
  ).state, "mismatch");
});

function exactNetworkInfo(overrides = {}) {
  return {
    chain_id: recovered.network.chainId,
    protocol_version: recovered.checkpoint.protocolVersion,
    recovery_active: true,
    recovery_epoch: recovered.checkpoint.recoveryEpoch,
    validator_set_id: recovered.checkpoint.validatorSetId,
    recovery_domain: recovered.checkpoint.recoveryDomain,
    checkpoint_manifest_hash: recovered.checkpoint.manifestHash,
    last_block_height: recovered.checkpoint.legacyPublicMaxHeight + 1,
    ...overrides,
  };
}

test("network identity requires exact chain, v3, epoch, set, domain, and manifest", () => {
  assert.equal(network.networkInfoVerification(exactNetworkInfo(), config).state, "verified");
  for (const [field, value] of [["chain_id", "wrong"], ["protocol_version", "3.0.1"], ["recovery_active", false], ["recovery_epoch", 8], ["validator_set_id", 10], ["recovery_domain", hex("0")], ["checkpoint_manifest_hash", hex("1")]]) {
    const result = network.networkInfoVerification(exactNetworkInfo({ [field]: value }), config);
    assert.equal(result.state, "mismatch", field);
  }
});

test("network identity stays unverified until the reported height is strictly above the legacy public maximum", () => {
  const missing = network.networkInfoVerification(exactNetworkInfo({ last_block_height: undefined }), config);
  assert.equal(missing.state, "unknown");
  assert.equal(missing.reason, "last-block-height-unavailable");

  const equal = network.networkInfoVerification(
    exactNetworkInfo({ last_block_height: recovered.checkpoint.legacyPublicMaxHeight }),
    config,
  );
  assert.equal(equal.state, "unknown");
  assert.equal(equal.reason, "visible-height-regression-gate-pending");
  assert.equal(equal.requiredMinimumHeight, recovered.checkpoint.legacyPublicMaxHeight + 1);

  const above = network.networkInfoVerification(
    exactNetworkInfo({ last_block_height: recovered.checkpoint.legacyPublicMaxHeight + 1 }),
    config,
  );
  assert.equal(above.state, "verified");
});

test("one lagging replica keeps the full recovery audit fail closed", () => {
  const boundaryBlock = { header: { height: H + 1, parent_hash: recovered.checkpoint.blockHash, hash: recovered.checkpoint.boundaryBlockHash, state_root: recovered.checkpoint.boundaryStateRoot } };
  const result = network.auditRecoveryCheckpoint({
    config,
    legacyBlock: { header: { height: H, hash: recovered.checkpoint.blockHash, state_root: recovered.checkpoint.stateRoot } },
    replicas: [
      { sourceId: "v3-a", boundaryBlock, networkInfo: exactNetworkInfo() },
      { sourceId: "v3-b", boundaryBlock, networkInfo: exactNetworkInfo({ last_block_height: recovered.checkpoint.legacyPublicMaxHeight }) },
    ],
  });
  assert.equal(result.state, "unknown");
  assert.equal(result.replicas.find((entry) => entry.sourceId === "v3-b").networkInfo.reason, "visible-height-regression-gate-pending");
});

test("full checkpoint audit verifies signed H plus every configured v3 replica", () => {
  const boundaryBlock = { header: { height: H + 1, parent_hash: recovered.checkpoint.blockHash, hash: recovered.checkpoint.boundaryBlockHash, state_root: recovered.checkpoint.boundaryStateRoot } };
  const result = network.auditRecoveryCheckpoint({
    config,
    legacyBlock: { header: { height: H, hash: recovered.checkpoint.blockHash, state_root: recovered.checkpoint.stateRoot } },
    replicas: ["v3-a", "v3-b"].map((sourceId) => ({ sourceId, boundaryBlock, networkInfo: exactNetworkInfo() })),
  });
  assert.equal(result.state, "verified");
  assert.equal(result.replicas.length, 2);
});

test("missing replica or wrong H never receives canonical status", () => {
  const boundaryBlock = { header: { height: H + 1, parent_hash: recovered.checkpoint.blockHash, hash: recovered.checkpoint.boundaryBlockHash, state_root: recovered.checkpoint.boundaryStateRoot } };
  const incomplete = network.auditRecoveryCheckpoint({
    config,
    legacyBlock: { header: { height: H, hash: recovered.checkpoint.blockHash, state_root: recovered.checkpoint.stateRoot } },
    replicas: [{ sourceId: "v3-a", boundaryBlock, networkInfo: exactNetworkInfo() }],
  });
  assert.equal(incomplete.state, "unknown");
  const configured = resolver.classifyOccurrence("v3-a", H + 2);
  assert.equal(network.gateCanonical(configured, incomplete).canonical, false);
  assert.equal(network.gateCanonical(configured, { state: "verified" }).canonical, true);
});

test("stale blocks are reported as stalled", () => {
  const result = network.evaluateLiveness({}, { header: { timestamp: 1_000 } }, 3_000_000, 1200);
  assert.equal(result.state, "stalled");
});

test("configuration can be injected without a network request", async () => {
  let fetched = false;
  const loaded = await network.loadConfig({ injected: recovered, fetchImpl: async () => { fetched = true; } });
  assert.equal(loaded.state, "recovered");
  assert.equal(fetched, false);
});

test("configuration fetch failures remain explicit", async () => {
  await assert.rejects(
    network.loadConfig({ fetchImpl: async () => ({ ok: false, status: 503 }) }),
    /configuration request failed \(503\)/,
  );
});

process.stdout.write(`\nARC frontend network contract: ${count}/${count} checks passed\n`);
