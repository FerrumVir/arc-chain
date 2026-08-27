#!/usr/bin/env node

import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";

const configTarget = process.env.ARC_LIVE_CONFIG;
if (!configTarget) {
  console.log("SKIP ARC dashboard live gate: set ARC_LIVE_CONFIG to an approved config path or HTTPS URL");
  process.exit(0);
}

const require = createRequire(import.meta.url);
const network = require("../shared/frontend/arc-network.js");
const dashboard = require("./app.js");

async function loadRawConfig(target) {
  if (/^https:\/\//i.test(target)) {
    const response = await fetch(target, { cache: "no-store" });
    assert.equal(response.ok, true, `config returned HTTP ${response.status}`);
    return response.json();
  }
  return JSON.parse(await readFile(resolve(target), "utf8"));
}

const config = network.normalizeConfig(await loadRawConfig(configTarget));
assert.ok(config.checkpoint, "live dashboard gate requires an approved recovery checkpoint");
const resolver = network.createCanonicalResolver(config);

const [boundary, fleet] = await Promise.all([
  dashboard.verifyRecoveryBoundary({ resolver, fetchImpl: fetch }),
  dashboard.collectFleetHealth({ resolver, fetchImpl: fetch }),
]);

assert.equal(boundary.state, "verified", `exact recovery checkpoint proof is ${boundary.state}: ${boundary.reason ?? "no reason"}`);
const inference = await dashboard.loadInferenceEvidence({ resolver, fetchImpl: fetch, checkpointAudit: boundary });
assert.notEqual(fleet.state, "fork", `v3 replicas disagree at common height ${fleet.commonHeight}`);
assert.notEqual(fleet.state, "offline", "all configured v3 replicas are offline");
assert.ok(fleet.current?.reachable, "checkpoint-selected v3 source must be reachable");
assert.notEqual(fleet.current.liveness.state, "stalled", "checkpoint-selected v3 source is stale");
assert.equal(inference.error, null, `inference evidence endpoint failed: ${inference.error}`);

console.log(`PASS ARC dashboard live gate: fleet=${fleet.state}, replicas=${fleet.reachable.length}/${fleet.replicaCount}, common_height=${fleet.commonHeight}, confirmed_inference=${inference.confirmed.length}, excluded=${inference.excluded}`);
