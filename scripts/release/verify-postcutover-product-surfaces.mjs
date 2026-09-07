#!/usr/bin/env node

// Read-only post-cutover acceptance for the product surfaces users actually
// rely on. The recovery verifier proves consensus and the two real canaries;
// this verifier takes those sealed identities and requires the dashboard and
// explorer readers to expose the same mined 0x25 receipts and earnings.

import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const require = createRequire(import.meta.url);
const defaultNetwork = require("../../shared/frontend/arc-network.js");
const defaultDashboard = require("../../dashboard/app.js");
const defaultExplorer = require("../../explorer/app.js");

const REWARD_SCHEMA = "arc.recovery.reward-evidence.v3";
const REWARD_BASE = 2_500_000_000;
const BASE_UNITS_PER_ARC = 1_000_000_000;

function exactIdentity(network, value, label) {
  const normalized = network.normalizeHex(value, 32);
  assert.ok(normalized, `${label} must be an exact 32-byte hexadecimal identity`);
  return normalized;
}

function exactNonNegativeInteger(value, label) {
  assert.ok(Number.isSafeInteger(value) && value >= 0, `${label} must be a non-negative safe integer`);
  return value;
}

function expectedCanaries(network, rewardEvidence) {
  assert.equal(rewardEvidence?.schema, REWARD_SCHEMA, "unsupported reward-evidence schema");
  assert.ok(Array.isArray(rewardEvidence.receipts), "reward evidence receipts must be an array");
  assert.equal(rewardEvidence.receipts.length, 2, "exactly two real rollout canaries are required");
  const baseline = rewardEvidence.earnings_baseline;
  assert.ok(baseline && typeof baseline === "object" && !Array.isArray(baseline), "reward evidence earnings baseline is required");
  const worker = exactIdentity(network, baseline.worker, "earnings baseline worker");
  const receipts = rewardEvidence.receipts.map((row, index) => {
    assert.ok(row && typeof row === "object" && !Array.isArray(row), `reward receipt ${index} must be an object`);
    const receiptWorker = exactIdentity(network, row.worker, `reward receipt ${index} worker`);
    assert.equal(receiptWorker, worker, `reward receipt ${index} belongs to another worker`);
    return {
      txHash: exactIdentity(network, row.tx_hash, `reward receipt ${index} transaction`),
      jobId: exactIdentity(network, row.job_id, `reward receipt ${index} job`),
      worker,
    };
  });
  assert.equal(new Set(receipts.map((row) => row.txHash)).size, 2, "canary transaction identities must be distinct");
  assert.equal(new Set(receipts.map((row) => row.jobId)).size, 2, "canary job identities must be distinct");
  return {
    worker,
    receipts,
    baselineCount: exactNonNegativeInteger(baseline.confirmed_receipt_count, "baseline receipt count"),
    baselineGrossBase: exactNonNegativeInteger(baseline.confirmed_gross_earnings_base, "baseline gross earnings"),
  };
}

function requireCanonicalRewardOccurrence(result, sourceId, expected, label) {
  const matches = result.occurrences.filter((row) => row.source?.id === sourceId);
  assert.equal(matches.length, 1, `${label} must expose the canary exactly once on the canonical v3 source`);
  const occurrence = matches[0];
  assert.equal(occurrence.rewardEvidenceBound, true, `${label} did not bind the authoritative community reward receipt to the requested hash`);
  assert.equal(occurrence.provenance?.canonical, true, `${label} did not classify the canary as canonical`);
  assert.equal(occurrence.classification?.txHash, expected.txHash, `${label} returned another transaction`);
  assert.equal(occurrence.classification?.rewardJob, expected.jobId, `${label} returned another inference job`);
  assert.equal(occurrence.classification?.rewardWorker, expected.worker, `${label} returned another worker`);
  assert.equal(occurrence.classification?.rewardEarned, true, `${label} did not show a successful mined 0x25 reward`);
  assert.equal(occurrence.classification?.inferenceConfirmed, true, `${label} did not show confirmed computation`);
  assert.equal(occurrence.classification?.paymentConfirmed, true, `${label} did not show confirmed payment`);
}

export async function verifyPostcutoverProductSurfaces(options) {
  const {
    config: rawConfig,
    rewardEvidence,
    fetchImpl = globalThis.fetch,
    network = defaultNetwork,
    dashboard = defaultDashboard,
    explorer = defaultExplorer,
  } = options;
  assert.equal(typeof fetchImpl, "function", "a fetch implementation is required");
  const config = network.normalizeConfig(rawConfig);
  assert.ok(["recovered", "degraded"].includes(config.state), "post-cutover product acceptance requires an active recovery config");
  assert.ok(config.checkpoint, "post-cutover product acceptance requires the approved checkpoint");
  const resolver = network.createCanonicalResolver(config);
  const source = resolver.currentSource();
  assert.equal(source?.kind, "v3", "the canonical current source must be protocol v3");
  const canaries = expectedCanaries(network, rewardEvidence);

  const [dashboardBoundary, explorerBoundary, fleet, maintenanceAudit] = await Promise.all([
    dashboard.verifyRecoveryBoundary({ resolver, fetchImpl }),
    explorer.verifyRecoveryCheckpoint({ resolver, fetchImpl }),
    dashboard.collectFleetHealth({ resolver, fetchImpl }),
    network.auditMaintenanceInterlock({ resolver, fetchImpl }),
  ]);
  assert.equal(dashboardBoundary.state, "verified", `dashboard recovery proof is ${dashboardBoundary.state}: ${dashboardBoundary.reason ?? "no reason"}`);
  assert.equal(explorerBoundary.state, "verified", `explorer recovery proof is ${explorerBoundary.state}: ${explorerBoundary.reason ?? "no reason"}`);
  const fleetError = dashboard.activeFleetPublicationError(config, fleet, maintenanceAudit);
  assert.equal(fleetError, null, `dashboard live publication gate failed: ${fleetError}`);

  const [inference, earnings, ...lookups] = await Promise.all([
    dashboard.loadInferenceEvidence({ resolver, fetchImpl, checkpointAudit: dashboardBoundary, limit: 100 }),
    dashboard.loadWorkerEarnings({ resolver, fetchImpl, workerId: canaries.worker, checkpointAudit: dashboardBoundary }),
    ...canaries.receipts.flatMap((receipt) => [
      dashboard.lookupTransaction({ resolver, fetchImpl, hash: receipt.txHash, checkpointAudit: dashboardBoundary }),
      explorer.queryTransaction({ resolver, fetchImpl, hash: receipt.txHash, sourceId: "canonical", checkpointAudit: explorerBoundary }),
    ]),
  ]);

  assert.equal(inference.error, null, `dashboard inference feed failed: ${inference.error}`);
  for (const expected of canaries.receipts) {
    const visible = inference.confirmed.filter((row) => row.receipt?.txHash === expected.txHash);
    assert.equal(visible.length, 1, `dashboard inference feed does not expose canary 0x${expected.txHash}`);
    assert.equal(visible[0].receipt.rewardEarned, true, `dashboard inference feed does not show canary 0x${expected.txHash} as paid`);
    assert.equal(visible[0].receipt.rewardWorker, expected.worker, `dashboard inference feed attributes canary 0x${expected.txHash} to another worker`);
    assert.equal(visible[0].receipt.rewardJob, expected.jobId, `dashboard inference feed attributes canary 0x${expected.txHash} to another job`);
  }

  assert.equal(earnings.receiptEvidenceConsistent, true, "dashboard rejected the canonical earnings receipt contract");
  assert.equal(earnings.historyContractConsistent, true, "dashboard rejected the canonical earnings history scope");
  assert.equal(earnings.archiveMode, true, "post-cutover earnings must come from the archive-backed canonical history");
  assert.equal(earnings.readiness, "ready", "community reward issuance is not ready on the canonical source");
  assert.ok(
    earnings.totalRewards >= canaries.baselineCount + canaries.receipts.length,
    "dashboard earnings count does not include baseline plus both real canaries",
  );
  const minimumGrossArc = (canaries.baselineGrossBase + REWARD_BASE * canaries.receipts.length) / BASE_UNITS_PER_ARC;
  assert.ok(
    earnings.confirmedGross + Number.EPSILON >= minimumGrossArc,
    "dashboard gross earnings do not include baseline plus both 2.5 ARC canaries",
  );
  const earningsByTx = new Map(earnings.confirmedReceipts.map((row) => [
    exactIdentity(network, row.tx_hash, "earnings receipt transaction"),
    row,
  ]));
  for (const expected of canaries.receipts) {
    const row = earningsByTx.get(expected.txHash);
    assert.ok(row, `dashboard earnings does not retain canary 0x${expected.txHash}`);
    assert.equal(exactIdentity(network, row.job_id, "earnings receipt job"), expected.jobId, "earnings receipt has another job identity");
    assert.equal(exactIdentity(network, row.worker, "earnings receipt worker"), expected.worker, "earnings receipt has another worker identity");
    assert.equal(row.reward_base, REWARD_BASE, "earnings receipt does not carry the exact 2.5 ARC base reward");
    assert.equal(row.reward_arc, 2.5, "earnings receipt does not carry the exact 2.5 ARC display reward");
  }

  const projectionAvailable = typeof earnings.projectedPerDay === "number"
    && Number.isFinite(earnings.projectedPerDay)
    && earnings.projectedPerDay >= 0
    && earnings.projectionReason === null;
  const projectionUnavailable = earnings.projectedPerDay === null
    && typeof earnings.projectionReason === "string"
    && earnings.projectionReason.trim().length > 0;
  assert.ok(
    projectionAvailable || projectionUnavailable,
    "dashboard projection must be either a finite non-negative authoritative value with no reason or unavailable with a nonempty reason",
  );

  canaries.receipts.forEach((receipt, index) => {
    requireCanonicalRewardOccurrence(lookups[index * 2], source.id, receipt, "dashboard transaction lookup");
    requireCanonicalRewardOccurrence(lookups[index * 2 + 1], source.id, receipt, "explorer transaction lookup");
  });

  return Object.freeze({
    canonicalSourceId: source.id,
    fleetState: fleet.state,
    replicaCount: fleet.replicaCount,
    commonHeight: fleet.commonHeight,
    worker: `0x${canaries.worker}`,
    canaryReceiptCount: canaries.receipts.length,
    confirmedReceiptCount: earnings.totalRewards,
    confirmedGrossArc: earnings.confirmedGross,
    projectedDailyArc: earnings.projectedPerDay,
    projectionState: projectionUnavailable ? "unavailable-with-reason" : "authoritative-endpoint-value",
  });
}

async function loadJson(target, label) {
  if (/^https:\/\//i.test(target)) {
    const response = await fetch(target, { cache: "no-store", redirect: "error" });
    assert.equal(response.ok, true, `${label} returned HTTP ${response.status}`);
    return response.json();
  }
  return JSON.parse(await readFile(resolve(target), "utf8"));
}

async function main(argv) {
  const values = new Map();
  for (let index = 0; index < argv.length; index += 2) {
    const flag = argv[index];
    const value = argv[index + 1];
    assert.ok(["--config", "--reward-evidence"].includes(flag) && value, "usage: verify-postcutover-product-surfaces.mjs --config PATH_OR_HTTPS_URL --reward-evidence PATH");
    assert.ok(!values.has(flag), `${flag} may be supplied only once`);
    values.set(flag, value);
  }
  assert.equal(values.size, 2, "both --config and --reward-evidence are required");
  const result = await verifyPostcutoverProductSurfaces({
    config: await loadJson(values.get("--config"), "frontend config"),
    rewardEvidence: await loadJson(values.get("--reward-evidence"), "reward evidence"),
  });
  console.log(`VERIFIED ARC post-cutover product surfaces ${JSON.stringify(result)}`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main(process.argv.slice(2)).catch((error) => {
    console.error(`post-cutover product-surface verification failed: ${error?.message ?? error}`);
    process.exitCode = 1;
  });
}
