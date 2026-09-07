#!/usr/bin/env node

import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { verifyPostcutoverProductSurfaces } from "../../scripts/release/verify-postcutover-product-surfaces.mjs";

const require = createRequire(import.meta.url);
const productNetwork = require("../../shared/frontend/arc-network.js");

const hex = (character) => character.repeat(64);
const worker = hex("e");
const canaries = [
  { txHash: hex("a"), jobId: hex("c"), worker },
  { txHash: hex("b"), jobId: hex("d"), worker },
];
const rewardEvidence = {
  schema: "arc.recovery.reward-evidence.v3",
  earnings_baseline: {
    worker: `0x${worker}`,
    confirmed_receipt_count: 0,
    confirmed_gross_earnings_base: 0,
  },
  receipts: canaries.map((row) => ({
    tx_hash: `0x${row.txHash}`,
    job_id: `0x${row.jobId}`,
    worker: `0x${row.worker}`,
  })),
};

const network = {
  normalizeHex(value, bytes) {
    const normalized = typeof value === "string" ? value.replace(/^0x/i, "").toLowerCase() : "";
    return new RegExp(`^[0-9a-f]{${bytes * 2}}$`).test(normalized) ? normalized : null;
  },
  normalizeConfig(value) { return value; },
  createCanonicalResolver(config) {
    return { config, currentSource: () => ({ id: "v3-nyc", kind: "v3", name: "ARC v3 NYC" }) };
  },
  async auditMaintenanceInterlock() {
    return { state: "healthy", samples: Array.from({ length: 6 }, (_, index) => ({ sourceId: `v3-${index}` })) };
  },
};

const rewardRpcReceipt = (row, overrides = {}) => ({
  status: "mined_success",
  tx_type: "0x25",
  tx_hash: `0x${row.txHash}`,
  job_id: `0x${row.jobId}`,
  worker: `0x${row.worker}`,
  model_id: `0x${hex("1")}`,
  input_hash: `0x${hex("2")}`,
  output_hash: `0x${hex("3")}`,
  assignment_epoch: `0x${hex("4")}`,
  recovery_epoch: 1,
  validator_set_id: 1,
  validator_set_commitment: `0x${hex("5")}`,
  transaction_domain: `0x${hex("6")}`,
  validator_approvals: 5,
  submitted: true,
  included: true,
  confirmed: true,
  success: true,
  block_height: row.txHash === canaries[0].txHash ? 101 : 102,
  block_hash: `0x${row.txHash}`,
  index: 0,
  reward_base: 2_500_000_000,
  reward_arc: 2.5,
  receipt_url: `/community/reward_receipt/0x${row.txHash}`,
  evidence_source: "successful mined CommunityInferenceReward receipt",
  ...overrides,
});
const classification = (row) => {
  const result = productNetwork.classifyCommunityRewardReceipt(
    rewardRpcReceipt(row),
    row.txHash,
  );
  assert.ok(result, "real direct reward RPC fixture must classify");
  return result;
};
const occurrence = (row) => ({
  source: { id: "v3-nyc" },
  rewardEvidenceBound: true,
  provenance: { canonical: true },
  classification: classification(row),
});
const confirmedReceipt = (row, index) => ({
  tx_type: "0x25",
  tx_hash: `0x${row.txHash}`,
  job_id: `0x${row.jobId}`,
  worker: `0x${row.worker}`,
  block_height: 101 + index,
  block_hash: `0x${row.txHash}`,
  submitted: true,
  included: true,
  confirmed: true,
  success: true,
  reward_base: 2_500_000_000,
  reward_arc: 2.5,
  receipt_url: `/community/reward_receipt/0x${row.txHash}`,
});
const earningsResult = (overrides = {}) => ({
  receiptEvidenceConsistent: true,
  historyContractConsistent: true,
  archiveMode: true,
  readiness: "ready",
  totalRewards: 2,
  confirmedGross: 5,
  confirmedReceipts: canaries.map(confirmedReceipt),
  projectedPerDay: null,
  projectionReason: "collecting data: two rollout canaries do not span 24 hours",
  ...overrides,
});

function fixture(overrides = {}) {
  const dashboard = {
    async verifyRecoveryBoundary() { return { state: "verified" }; },
    async collectFleetHealth() {
      return { state: "healthy", replicaCount: 6, commonHeight: 102 };
    },
    activeFleetPublicationError() { return null; },
    async loadInferenceEvidence() {
      return {
        error: null,
        confirmed: canaries.map((row) => ({
          receipt: classification(row),
          provenance: { canonical: true },
        })),
      };
    },
    async loadWorkerEarnings() {
      return earningsResult();
    },
    async lookupTransaction({ hash }) {
      return { occurrences: [occurrence(canaries.find((row) => row.txHash === hash))] };
    },
    ...overrides.dashboard,
  };
  const explorer = {
    async verifyRecoveryCheckpoint() { return { state: "verified" }; },
    async queryTransaction({ hash }) {
      return { occurrences: [occurrence(canaries.find((row) => row.txHash === hash))] };
    },
    ...overrides.explorer,
  };
  return {
    config: { state: "recovered", checkpoint: {}, network: {}, sources: [] },
    rewardEvidence,
    fetchImpl: async () => { throw new Error("stub APIs must consume fetch"); },
    network,
    dashboard,
    explorer,
  };
}

const accepted = await verifyPostcutoverProductSurfaces(fixture());
assert.deepEqual(accepted, {
  canonicalSourceId: "v3-nyc",
  fleetState: "healthy",
  replicaCount: 6,
  commonHeight: 102,
  worker: `0x${worker}`,
  canaryReceiptCount: 2,
  confirmedReceiptCount: 2,
  confirmedGrossArc: 5,
  projectedDailyArc: null,
  projectionState: "unavailable-with-reason",
});

assert.equal(
  productNetwork.classifyCommunityRewardReceipt(
    rewardRpcReceipt(canaries[0], { evidence_source: "unbound response" }),
    canaries[0].txHash,
  ),
  null,
);

const acceptedProjection = await verifyPostcutoverProductSurfaces(fixture({
  dashboard: {
    async loadWorkerEarnings() {
      return earningsResult({ projectedPerDay: 7.5, projectionReason: null });
    },
  },
}));
assert.equal(acceptedProjection.projectedDailyArc, 7.5);
assert.equal(acceptedProjection.projectionState, "authoritative-endpoint-value");

await assert.rejects(
  verifyPostcutoverProductSurfaces(fixture({
    dashboard: {
      async loadWorkerEarnings() {
        return earningsResult({ projectedPerDay: null, projectionReason: null });
      },
    },
  })),
  /unavailable with a nonempty reason/,
);

await assert.rejects(
  verifyPostcutoverProductSurfaces(fixture({
    dashboard: {
      async loadWorkerEarnings() {
        return earningsResult({ projectedPerDay: 7.5, projectionReason: "conflicting reason" });
      },
    },
  })),
  /authoritative value with no reason/,
);

await assert.rejects(
  verifyPostcutoverProductSurfaces(fixture({
    dashboard: {
      async lookupTransaction({ hash }) {
        const row = canaries.find((candidate) => candidate.txHash === hash);
        return { occurrences: [{ ...occurrence(row), rewardEvidenceBound: false }] };
      },
    },
  })),
  /did not bind the authoritative community reward receipt/,
);

await assert.rejects(
  verifyPostcutoverProductSurfaces(fixture({
    dashboard: {
      async loadInferenceEvidence() {
        return { error: null, confirmed: [{ receipt: classification(canaries[0]) }] };
      },
    },
  })),
  /inference feed does not expose canary/,
);

await assert.rejects(
  verifyPostcutoverProductSurfaces(fixture({
    dashboard: {
      async loadWorkerEarnings() {
        return {
          receiptEvidenceConsistent: true,
          historyContractConsistent: true,
          archiveMode: true,
          readiness: "ready",
          totalRewards: 1,
          confirmedGross: 2.5,
          confirmedReceipts: [confirmedReceipt(canaries[0], 0)],
          projectedPerDay: null,
          projectionReason: "collecting data",
        };
      },
    },
  })),
  /baseline plus both real canaries/,
);

console.log("ARC post-cutover product-surface verifier: 8/8 checks passed");
