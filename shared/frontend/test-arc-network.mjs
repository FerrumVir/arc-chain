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
const archiveSource = (overrides = {}) => ({
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
  sourceHeight: H + 100,
  sourceBlockHash: hex("c"),
  sourceStateRoot: hex("d"),
  provenancePath: "/provenance",
  ...overrides,
});
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
    { id: "fork", name: "Preserved fork", kind: "legacy-fork", baseUrl: "https://fork.example.test", archive: archiveSource() },
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

test("legacy fork sources require exact immutable archive pins", () => {
  const missing = structuredClone(recovered);
  delete missing.sources.find((source) => source.id === "fork").archive;
  assert.throws(() => network.normalizeConfig(missing), /archive is required/);

  const wrongBoundary = structuredClone(recovered);
  wrongBoundary.sources.find((source) => source.id === "fork").archive.canonicalCheckpointHeight = H - 1;
  assert.throws(() => network.normalizeConfig(wrongBoundary), /configured canonical checkpoint height/);
});

test("preserves valid noncanonical fork archives at and below canonical H", () => {
  for (const sourceHeight of [H, H - 100]) {
    const candidate = structuredClone(recovered);
    candidate.sources.find((source) => source.id === "fork").archive.sourceHeight = sourceHeight;
    assert.equal(
      network.normalizeConfig(candidate).sourcesById.get("fork").archive.sourceHeight,
      sourceHeight,
    );
  }
  const invalid = structuredClone(recovered);
  invalid.sources.find((source) => source.id === "fork").archive.sourceHeight = -1;
  assert.throws(() => network.normalizeConfig(invalid), /non-negative integer/);
});

const normalizedFork = network.normalizeConfig(recovered).sourcesById.get("fork");
const provenancePayload = {
  schema: "arc.legacy-archive.query.v1",
  read_only: true,
  classification: "valid_noncanonical_fork",
  capture_id: normalizedFork.archive.captureId,
  node: normalizedFork.archive.node,
  rollout_manifest_sha256: normalizedFork.archive.rolloutManifestSha256,
  archive_manifest_sha256: normalizedFork.archive.archiveManifestSha256,
  complete_sha256: normalizedFork.archive.completeSha256,
  bundle_sha256: normalizedFork.archive.bundleSha256,
  inventory_sha256: normalizedFork.archive.inventorySha256,
  binding_index_sha256: normalizedFork.archive.bindingIndexSha256,
  binding_sha256: normalizedFork.archive.bindingSha256,
  checkpoint_sha256: normalizedFork.archive.checkpointSha256,
  checkpoint_manifest_hash: normalizedFork.archive.checkpointManifestHash,
  checkpoint_payload_hash: normalizedFork.archive.checkpointPayloadHash,
  canonical_checkpoint_height: normalizedFork.archive.canonicalCheckpointHeight,
  source_height: normalizedFork.archive.sourceHeight,
  source_block_hash: normalizedFork.archive.sourceBlockHash,
  source_state_root: normalizedFork.archive.sourceStateRoot,
};
const verifiedArchive = await network.verifyLegacyArchiveSource({
  source: normalizedFork,
  fetchImpl: async (_url, options) => ({ ok: options.method === "GET", status: 200, json: async () => provenancePayload }),
});
assert.equal(verifiedArchive.state, "verified");
count += 1;
process.stdout.write(`ok ${count} - verifies every legacy archive provenance pin\n`);

const mismatchedArchive = await network.verifyLegacyArchiveSource({
  source: normalizedFork,
  fetchImpl: async () => ({ ok: true, status: 200, json: async () => ({ ...provenancePayload, checkpoint_sha256: hex("f") }) }),
});
assert.equal(mismatchedArchive.state, "mismatch");
assert.deepEqual(mismatchedArchive.mismatches, ["checkpointSha256"]);
count += 1;
process.stdout.write(`ok ${count} - fails closed on legacy archive provenance mismatch\n`);

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
    schema: "arc.inference.activity.v1",
    record_kind: "mined_community_inference_reward",
    source: "chain_receipt",
    mined: true,
    receipt_status: "success",
    success: true,
    computed: true,
    paid: true,
    earned: true,
    tx_type: "CommunityInferenceReward",
    tx_type_code: "0x25",
    tx_hash: hex("d"),
    block_height: H + 3,
  });
  assert.equal(result.rewardEarned, true);
  assert.equal(result.category, "reward");
  assert.equal(result.inferenceConfirmed, true);
  assert.equal(result.paymentConfirmed, true);
});

test("submitted rewards are not earnings", () => {
  const result = network.classifyReceipt({ tx: { tx_type: "CommunityInferenceReward" }, status: "submitted" });
  assert.equal(result.rewardEarned, false);
  assert.equal(result.receiptBacked, false);
  assert.equal(result.inferenceConfirmed, false);
});

test("reward-like names and incomplete canonical reward identities never prove payment", () => {
  const common = {
    schema: "arc.inference.activity.v1",
    record_kind: "mined_community_inference_reward",
    source: "chain_receipt",
    mined: true,
    receipt_status: "success",
    success: true,
    computed: true,
    paid: true,
    earned: true,
    tx_type_code: "0x25",
    tx_hash: hex("d"),
    block_height: H + 3,
  };
  for (const tx_type of ["InferenceRewardBogus", "CommunityRewardPreview"]) {
    const result = network.classifyReceipt({ ...common, tx_type });
    assert.equal(result.category, "transaction");
    assert.equal(result.rewardEarned, false);
    assert.equal(result.paymentConfirmed, false);
    assert.equal(result.inferenceConfirmed, false);
  }
  const missingRecordKind = network.classifyReceipt({ ...common, record_kind: undefined, tx_type: "CommunityInferenceReward" });
  assert.equal(missingRecordKind.paymentConfirmed, false);
});

test("canonical payment evidence requires a valid 32-byte transaction hash", () => {
  const common = {
    schema: "arc.inference.activity.v1",
    record_kind: "mined_community_inference_reward",
    source: "chain_receipt",
    mined: true,
    receipt_status: "success",
    success: true,
    computed: true,
    paid: true,
    earned: true,
    tx_type: "CommunityInferenceReward",
    tx_type_code: "0x25",
    block_height: H + 3,
  };
  for (const tx_hash of [undefined, "not-a-hash", hex("d").slice(2)]) {
    const result = network.classifyReceipt({ ...common, tx_hash });
    assert.equal(result.txHash, null);
    assert.equal(result.mined, false);
    assert.equal(result.rewardEarned, false);
    assert.equal(result.paymentConfirmed, false);
    assert.equal(result.inferenceConfirmed, false);
  }
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

const maintenanceConfig = structuredClone(recovered);
maintenanceConfig.sources = maintenanceConfig.sources.filter((source) => !source.id.startsWith("v3-"));
maintenanceConfig.sources.push(...Array.from({ length: 6 }, (_, index) => ({
  id: `v3-${index + 1}`,
  name: `v3 ${index + 1}`,
  kind: "v3",
  replicaGroup: "v3-main",
  baseUrl: `https://v3-${index + 1}.example.test`,
})));
maintenanceConfig.checkpoint.v3SourceId = "v3-1";
maintenanceConfig.services = {
  maintenanceInterlock: {
    schema: network.MAINTENANCE_SERVICE_SCHEMA,
    path: "/maintenance/status",
    sourceSetSha256: hex("1"),
    boundarySha256: hex("2"),
    toolSha256: hex("3"),
    sourceMainCommit: "4".repeat(40),
    observedCutoffHeight: H + 100,
    requiredHealthyReplicas: 6,
    maxStalenessSeconds: 90,
  },
};
const maintenanceResolver = network.createCanonicalResolver(maintenanceConfig);
const maintenanceNow = Date.parse("2026-08-31T18:00:30Z");
const maintenanceResponseHashes = {
  info_before: hex("6"),
  latest: hex("7"),
  exact: hex("8"),
  info_after: hex("9"),
};
const officialRetiredUnreachable = network.OFFICIAL_RETIRED_ORIGINS.map(
  ({ name, origin }) => ({
    name,
    origin,
    scope: "retired",
    outcome: "unreachable",
    height: null,
    block_hash: null,
    state_root: null,
    response_sha256: null,
  }),
);
const retiredUnreachable = officialRetiredUnreachable[0];
const communityObserved = {
  name: "community-one",
  origin: "https://community.example.test:9443",
  scope: "community",
  outcome: "observed",
  height: H,
  block_hash: hex("a"),
  state_root: hex("b"),
  response_sha256: maintenanceResponseHashes,
};
const healthyStatus = {
  schema: network.MAINTENANCE_STATUS_SCHEMA,
  source_main_commit: "4".repeat(40),
  boundary_sha256: hex("2"),
  source_set_sha256: hex("1"),
  tool_sha256: hex("3"),
  sampled_at: "2026-08-31T18:00:00Z",
  expires_at: "2026-08-31T18:01:30Z",
  poll_interval_seconds: 30,
  max_staleness_seconds: 90,
  observations: [...officialRetiredUnreachable, communityObserved],
  state: "HEALTHY",
  gate_reason: "capture-bound-retirement-tripwire-clear",
  incident_sha256: null,
  required_community_observations: 1,
  healthy_community_observations: 1,
  global_absence_claimed: false,
};
const maintenanceHealthy = await network.auditMaintenanceInterlock({
  resolver: maintenanceResolver,
  nowMs: maintenanceNow,
  fetchImpl: async () => ({ ok: true, status: 200, json: async () => structuredClone(healthyStatus) }),
});
assert.equal(maintenanceHealthy.state, "healthy");
assert.equal(maintenanceHealthy.samples.length, 6);
count += 1;
process.stdout.write(`ok ${count} - requires six fresh healthy maintenance interlocks\n`);

let maintenanceCall = 0;
const maintenanceTripped = await network.auditMaintenanceInterlock({
  resolver: maintenanceResolver,
  nowMs: maintenanceNow,
  fetchImpl: async () => {
    maintenanceCall += 1;
    const call = maintenanceCall;
    return {
      ok: true,
      status: 200,
      json: async () => call === 4
        ? {
            ...healthyStatus,
            state: "MAINTENANCE",
            gate_reason: "latched-legacy-source-incident",
            incident_sha256: hex("5"),
          }
        : structuredClone(healthyStatus),
    };
  },
});
assert.equal(maintenanceTripped.state, "maintenance");
count += 1;
process.stdout.write(`ok ${count} - one tripped maintenance interlock fails publication closed\n`);

const maintenanceExpired = await network.auditMaintenanceInterlock({
  resolver: maintenanceResolver,
  nowMs: Date.parse("2026-08-31T18:01:31Z"),
  fetchImpl: async () => ({ ok: true, status: 200, json: async () => structuredClone(healthyStatus) }),
});
assert.equal(maintenanceExpired.state, "maintenance");
assert.equal(maintenanceExpired.reason, "maintenance-interlock-evidence-incomplete");
count += 1;
process.stdout.write(`ok ${count} - expired maintenance status fails publication closed\n`);

const transientStatus = {
  ...healthyStatus,
  observations: [
    ...officialRetiredUnreachable,
    {
      ...communityObserved,
      outcome: "unreachable",
      height: null,
      block_hash: null,
      state_root: null,
      response_sha256: null,
    },
  ],
  state: "MAINTENANCE",
  gate_reason: "community-source-observation-unavailable",
  incident_sha256: null,
  healthy_community_observations: 0,
};
assert.equal(
  network.validateMaintenanceStatus(
    transientStatus,
    maintenanceResolver.config.services.maintenanceInterlock,
    maintenanceNow,
  ).gate_reason,
  "community-source-observation-unavailable",
);
count += 1;
process.stdout.write(`ok ${count} - v2 accepts transient community-source maintenance without inventing an incident\n`);

test("v1 and unknown maintenance status schemas fail closed", () => {
  for (const schema of [
    "arc.recovery.legacy-late-fork-interlock-status.v1",
    "arc.recovery.legacy-late-fork-interlock-status.v3",
  ]) {
    assert.throws(
      () => network.validateMaintenanceStatus(
        { ...healthyStatus, schema },
        maintenanceResolver.config.services.maintenanceInterlock,
        maintenanceNow,
      ),
      /identity\/policy differs/,
    );
  }
});

test("v2 gate reason, incident, and community counts are exact", () => {
  const invalid = [
    { ...healthyStatus, gate_reason: "legacy-clear" },
    { ...healthyStatus, incident_sha256: hex("5") },
    { ...healthyStatus, required_community_observations: 2 },
    { ...transientStatus, state: "HEALTHY" },
    { ...transientStatus, incident_sha256: hex("5") },
  ];
  for (const status of invalid) {
    assert.throws(
      () => network.validateMaintenanceStatus(
        status,
        maintenanceResolver.config.services.maintenanceInterlock,
        maintenanceNow,
      ),
      /maintenance/,
    );
  }
});

test("a responding retired origin requires the immutable latched incident", () => {
  const retiredObserved = {
    ...communityObserved,
    name: "nyc",
    origin: "http://149.28.32.76:9090",
    scope: "retired",
  };
  assert.throws(
    () => network.validateMaintenanceStatus(
      {
        ...healthyStatus,
        observations: [
          retiredObserved,
          ...officialRetiredUnreachable.slice(1),
          communityObserved,
        ],
      },
      maintenanceResolver.config.services.maintenanceInterlock,
      maintenanceNow,
    ),
    /legacy source candidate omitted its latched incident/,
  );
});

test("a post-cutoff community observation requires the immutable latched incident", () => {
  assert.throws(
    () => network.validateMaintenanceStatus(
      {
        ...healthyStatus,
        observations: [
          ...officialRetiredUnreachable,
          { ...communityObserved, height: H + 101 },
        ],
      },
      maintenanceResolver.config.services.maintenanceInterlock,
      maintenanceNow,
    ),
    /legacy source candidate omitted its latched incident/,
  );
});

test("v2 requires the exact six retired official coordinates", () => {
  const omitted = {
    ...healthyStatus,
    observations: [
      ...officialRetiredUnreachable.slice(0, -1),
      communityObserved,
    ],
  };
  const substituted = structuredClone(healthyStatus);
  substituted.observations[2].origin = "http://192.0.2.10:9090";
  for (const status of [omitted, substituted]) {
    assert.throws(
      () => network.validateMaintenanceStatus(
        status,
        maintenanceResolver.config.services.maintenanceInterlock,
        maintenanceNow,
      ),
      /retired official/,
    );
  }
});

test("v2 requires exact source commit, canonical UTC, and lowercase hashes", () => {
  const invalid = [
    { ...healthyStatus, source_main_commit: "5".repeat(40) },
    { ...healthyStatus, sampled_at: "2026-08-31T18:00:00.000Z" },
    { ...healthyStatus, sampled_at: "2026-02-30T18:00:00Z" },
    { ...healthyStatus, boundary_sha256: `0x${hex("2")}` },
    { ...healthyStatus, incident_sha256: hex("A") },
  ];
  for (const status of invalid) {
    assert.throws(
      () => network.validateMaintenanceStatus(
        status,
        maintenanceResolver.config.services.maintenanceInterlock,
        maintenanceNow,
      ),
      /maintenance/,
    );
  }
});

test("maintenance observations reject unsafe origins and incoherent evidence shapes", () => {
  const invalidRows = [
    { ...communityObserved, origin: "http://community.example.test:9443" },
    { ...communityObserved, origin: "https://user:pass@community.example.test:9443" },
    { ...communityObserved, height: "4219" },
    { ...communityObserved, response_sha256: null },
    { ...retiredUnreachable, response_sha256: maintenanceResponseHashes },
  ];
  for (const row of invalidRows) {
    assert.throws(
      () => network.validateMaintenanceStatus(
        { ...healthyStatus, observations: [...officialRetiredUnreachable, row] },
        maintenanceResolver.config.services.maintenanceInterlock,
        maintenanceNow,
      ),
      /maintenance/,
    );
  }
});

process.stdout.write(`\nARC frontend network contract: ${count}/${count} checks passed\n`);
