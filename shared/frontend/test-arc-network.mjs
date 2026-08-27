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
    blockHash: hex("a"),
    stateRoot: hex("b"),
    manifestHash: hex("c"),
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
    { header: { height: H + 1, parent_hash: recovered.checkpoint.blockHash } },
    config.checkpoint,
  );
  assert.equal(result.state, "verified");
});

test("H+1 parent mismatch is never hidden", () => {
  const result = network.boundaryVerification(
    { header: { height: H + 1, parent_hash: hex("f") } },
    config.checkpoint,
  );
  assert.equal(result.state, "mismatch");
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
