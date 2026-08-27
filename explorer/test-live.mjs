#!/usr/bin/env node

import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";

const configTarget = process.env.ARC_LIVE_CONFIG;
if (!configTarget) {
  console.log("SKIP ARC explorer live gate: set ARC_LIVE_CONFIG to an approved config path or HTTPS URL");
  process.exit(0);
}

const require = createRequire(import.meta.url);
const network = require("../shared/frontend/arc-network.js");
const explorer = require("./app.js");

async function loadRawConfig(target) {
  if (/^https:\/\//i.test(target)) {
    const response = await fetch(target, { cache: "no-store" });
    assert.equal(response.ok, true, `config returned HTTP ${response.status}`);
    return response.json();
  }
  return JSON.parse(await readFile(resolve(target), "utf8"));
}

const config = network.normalizeConfig(await loadRawConfig(configTarget));
assert.ok(config.checkpoint, "live explorer gate requires an approved recovery checkpoint");
const resolver = network.createCanonicalResolver(config);
const checkpoint = config.checkpoint;
const checkpointAudit = await explorer.verifyRecoveryCheckpoint({ resolver, fetchImpl: fetch });
assert.equal(checkpointAudit.state, "verified", `exact recovery checkpoint proof is ${checkpointAudit.state}: ${checkpointAudit.reason ?? "no reason"}`);

const legacy = await explorer.queryBlock({ resolver, fetchImpl: fetch, height: checkpoint.height, sourceId: "canonical", checkpointAudit });
assert.equal(legacy.route.sourceId, checkpoint.legacySourceId);
assert.equal(legacy.route.canonical, true, "signed H must be canonical only after the full replica audit");
assert.equal(network.blockHash(legacy.block), checkpoint.blockHash, "legacy H hash must match configured signed checkpoint");
assert.equal(network.stateRoot(legacy.block), checkpoint.stateRoot, "legacy H state root must match configured signed checkpoint");

const boundary = await explorer.queryBlock({ resolver, fetchImpl: fetch, height: checkpoint.recoveryHeight, sourceId: "canonical", checkpointAudit });
assert.equal(boundary.route.sourceId, checkpoint.v3SourceId);
assert.equal(boundary.route.canonical, true, "H+1 must be canonical only after the full replica audit");
assert.equal(boundary.boundary.state, "verified", "v3 H+1 parent must link to signed H");
assert.equal(network.blockHash(boundary.block), checkpoint.boundaryBlockHash, "H+1 hash must match configured boundary hash");
assert.equal(network.stateRoot(boundary.block), checkpoint.boundaryStateRoot, "H+1 state root must match configured boundary root");

const current = resolver.currentSource();
const [health, latest] = await Promise.all([
  explorer.requestJson(fetch, current, "/health"),
  explorer.requestJson(fetch, current, "/block/latest"),
]);
assert.ok(network.blockHeight(latest) >= checkpoint.recoveryHeight, "latest v3 block must not precede H+1");
const liveness = network.evaluateLiveness(health, latest);
assert.notEqual(liveness.state, "stalled", `canonical v3 source is stale (${liveness.ageSecs}s)`);

console.log(`PASS ARC explorer live gate: signed H=${checkpoint.height}, verified H+1=${checkpoint.recoveryHeight}, latest=${network.blockHeight(latest)}`);
