#!/usr/bin/env node

// Create-only evidence for the post-cutover desktop product gate. This helper
// runs only after `playwright test` succeeds (see desktop/package.json), then
// independently re-reads the exact canary from the loopback node. A skipped,
// flaky, partial, or prefix-only UI run cannot produce a receipt.

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { constants as fsConstants } from "node:fs";
import { open, readdir, readlink, lstat, realpath } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname, isAbsolute, relative, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const require = createRequire(import.meta.url);
const network = require("../../shared/frontend/arc-network.js");

export const DESKTOP_LIVE_SCHEMA = "arc.desktop-live-product-gate.v2";
export const PACKAGED_NATIVE_INPUT_SCHEMA = "arc.packaged-desktop-native-input.v1";
export const PACKAGED_NATIVE_SCHEMA = "arc.packaged-desktop-native-acceptance.v1";
export const PACKAGED_NATIVE_ATTEMPT_SCHEMA = "arc.packaged-desktop-native-dispatch-attempt.v1";
export const PACKAGED_APPIMAGE_HOST_SCHEMA = "arc.packaged-appimage-live-host.v1";
export const MACOS_PACKAGE_INSPECTION_SCHEMA = "arc.macos-package-inspection.v1";
export const MACOS_PACKAGE_PROVENANCE_SCHEMA = "arc.macos-packaged-native-provenance.v1";
export const MACOS_PACKAGE_PROVENANCE_VERIFICATION_SCHEMA =
  "arc.macos-package-provenance-verification.v1";
export const MACOS_UPDATER_SIGNATURE_SCHEMA =
  "arc.macos-updater-signature-verification.v1";
export const MAX_RECEIPT_POLLS = 61;
export const MAX_RECEIPT_WAIT_MS = 180_000;
const REWARD_BASE = 2_500_000_000;
const REWARD_ARC = 2.5;
const MAX_JSON_BYTES = 16 * 1024 * 1024;
const MAX_BUNDLE_ENTRIES = 20_000;
const MAX_BUNDLE_BYTES = 2 * 1024 * 1024 * 1024;
const MAX_BUNDLE_MEMBER_BYTES = 512 * 1024 * 1024;
const MAX_BUNDLE_DEPTH = 32;
const CANONICAL_REWARD_INFERENCE_PROFILE =
  "INT8 integer (per-row, cross-platform deterministic)";
const PACKAGED_APPIMAGE_PLATFORM_CLAIM =
  "published Linux x86_64 AppImage launched by WebKitWebDriver under "
  + "WebKitGTK, exercising real bundled UI and Tauri IPC; this does not prove "
  + "the macOS .app/WKWebView or Windows WebView2 packages";
const MACOS_PACKAGE_CONTROLLER_RELATIVE =
  "scripts/recovery/build-macos-package-provenance.py";
const MACOS_UPDATER_APT_SOURCE_SHA256 =
  "4bfd00f33ef52b3b75a41988c6362874422c54210d60022a73e57740e2d60b15";
const MACOS_UPDATER_LIMA_CONFIG_SHA256 =
  "865b35d8bb272aafd60b5f80838632eff2699eb2061d73ead8494cf49f657095";
const MACOS_PACKAGE_TRUTH_SCOPE = Object.freeze({
  appleDeveloperIdSigned: false,
  exactMountedDmgExecutableRan: true,
  gatekeeperAssessed: false,
  nativeCoreOnly: true,
  notarizationAssessed: false,
  shippedDebugOrWebdriverSurfaceAdded: false,
  uiToIpcCoveredByThisReceipt: false,
  updaterArchiveMinisignVerified: true,
});
const HASH_RE = /^[0-9a-f]{64}$/;
const COMMIT_RE = /^[0-9a-f]{40}$/;
const LIVE_VALIDATOR_NAME = "lax";
const LIVE_VALIDATOR_HOST = "140.82.16.112";
const FORWARD_KIND = "host-key-pinned-ssh-local-tcp-to-validator-unix-v1";
const HISTORY_DOMAIN =
  "all canonical 0x25 reward domains since the v3 recovery boundary; historical rows retain their own recovery_epoch, validator_set_id, and transaction_domain";
const RETAINED_SOURCE = "scan of this node's in-memory full_transactions map";
const RETAINED_SCOPE = "this node's bounded retained reward-receipt window";
const ARCHIVE_SCOPE = "complete canonical reward history since the v3 recovery boundary";
const EXPECTED_TESTS = Object.freeze([
  "first-launch: welcome → onboarding → dashboard with real data",
  "dashboard shows real-node peers + committed blocks + attestations",
  "sealed 0x25 canary is visible in earnings and host-scoped explorer",
  "network screen reads real validator count + latest block",
]);
const SUITE_FILES = Object.freeze([
  "desktop/index.html",
  "desktop/package.json",
  "desktop/playwright.config.ts",
  "desktop/playwright.live.config.ts",
  "desktop/tests/helpers.ts",
  "desktop/tests/live.spec.ts",
  "desktop/vite.config.ts",
]);
const DIRECT_RECEIPT_KEYS = Object.freeze([
  "assignment_epoch", "block_hash", "block_height", "confirmed",
  "evidence_source", "included", "index", "input_hash", "job_id",
  "model_id", "output_hash", "receipt_url", "recovery_epoch", "reward_arc",
  "reward_base", "status", "submitted", "success", "transaction_domain",
  "tx_hash", "tx_type", "validator_approvals", "validator_set_commitment",
  "validator_set_id", "worker",
]);
const EARNINGS_RECEIPT_KEYS = Object.freeze([
  "assignment_epoch", "block_hash", "block_height", "confirmed", "included",
  "index", "input_hash", "job_id", "model_id", "output_hash", "receipt_url",
  "recovery_epoch", "reward_arc", "reward_base", "submitted", "success",
  "transaction_domain", "tx_hash", "tx_type", "validator_set_id", "worker",
]);
const BLOCK_RECEIPT_KEYS = Object.freeze([
  "block_hash", "block_height", "gas_used", "inclusion_proof", "index",
  "logs", "success", "tx_hash", "value_commitment",
]);

// A deliberately small, independently reviewed BLAKE3 implementation for the
// sealed acceptance prompt.  The prompt is bounded far below one 1,024-byte
// BLAKE3 chunk, so rejecting larger inputs keeps this verifier compact and
// avoids silently exercising unreviewed tree-reduction code.  Known-answer
// tests below are also evaluated at module load; the implementation source
// digest is emitted in the receipt so downstream evidence binds the exact
// verifier that recomputed the on-chain input hash.
const BLAKE3_IV = Object.freeze([
  0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
  0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
]);
const BLAKE3_MSG_PERMUTATION = Object.freeze([
  2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8,
]);

function blake3RotateRight(value, shift) {
  return ((value >>> shift) | (value << (32 - shift))) >>> 0;
}

function blake3Compress(chainingValue, blockWords, blockLength, flags) {
  const state = new Uint32Array(16);
  state.set(chainingValue, 0);
  state.set(BLAKE3_IV.slice(0, 4), 8);
  // The short-input verifier always hashes chunk zero.
  state[12] = 0;
  state[13] = 0;
  state[14] = blockLength;
  state[15] = flags;
  let message = Array.from(blockWords);

  const mix = (a, b, c, d, x, y) => {
    state[a] = (state[a] + state[b] + x) >>> 0;
    state[d] = blake3RotateRight(state[d] ^ state[a], 16);
    state[c] = (state[c] + state[d]) >>> 0;
    state[b] = blake3RotateRight(state[b] ^ state[c], 12);
    state[a] = (state[a] + state[b] + y) >>> 0;
    state[d] = blake3RotateRight(state[d] ^ state[a], 8);
    state[c] = (state[c] + state[d]) >>> 0;
    state[b] = blake3RotateRight(state[b] ^ state[c], 7);
  };

  for (let round = 0; round < 7; round += 1) {
    mix(0, 4, 8, 12, message[0], message[1]);
    mix(1, 5, 9, 13, message[2], message[3]);
    mix(2, 6, 10, 14, message[4], message[5]);
    mix(3, 7, 11, 15, message[6], message[7]);
    mix(0, 5, 10, 15, message[8], message[9]);
    mix(1, 6, 11, 12, message[10], message[11]);
    mix(2, 7, 8, 13, message[12], message[13]);
    mix(3, 4, 9, 14, message[14], message[15]);
    message = BLAKE3_MSG_PERMUTATION.map((index) => message[index]);
  }

  const output = new Uint32Array(16);
  for (let index = 0; index < 8; index += 1) {
    output[index] = (state[index] ^ state[index + 8]) >>> 0;
    output[index + 8] = (state[index + 8] ^ chainingValue[index]) >>> 0;
  }
  return output;
}

export function blake3Short(raw) {
  const bytes = Buffer.isBuffer(raw) ? raw : Buffer.from(raw);
  assert.ok(bytes.length <= 1_024, "reviewed BLAKE3 verifier accepts at most one chunk");
  let chainingValue = Uint32Array.from(BLAKE3_IV);
  const blockCount = Math.max(1, Math.ceil(bytes.length / 64));
  for (let blockIndex = 0; blockIndex < blockCount; blockIndex += 1) {
    const block = bytes.subarray(blockIndex * 64, Math.min(bytes.length, (blockIndex + 1) * 64));
    const padded = Buffer.alloc(64);
    block.copy(padded);
    const words = new Uint32Array(16);
    for (let index = 0; index < 16; index += 1) words[index] = padded.readUInt32LE(index * 4);
    const isFinal = blockIndex + 1 === blockCount;
    const flags = (blockIndex === 0 ? 1 : 0) | (isFinal ? 2 | 8 : 0);
    const compressed = blake3Compress(chainingValue, words, block.length, flags);
    if (isFinal) {
      const digest = Buffer.alloc(32);
      for (let index = 0; index < 8; index += 1) digest.writeUInt32LE(compressed[index], index * 4);
      return digest.toString("hex");
    }
    chainingValue = compressed.slice(0, 8);
  }
  throw new Error("unreachable BLAKE3 state");
}

const BLAKE3_VERIFIER_SOURCE_SHA256 =
  "ec5d6c51a048e2535aa4790adbcf767ec76475115ccda439eac4b65c8de643d6";
assert.equal(
  sha256(Buffer.from(blake3Short.toString())),
  BLAKE3_VERIFIER_SOURCE_SHA256,
  "reviewed BLAKE3 verifier source changed",
);
assert.equal(blake3Short(Buffer.alloc(0)), "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262");
assert.equal(blake3Short(Buffer.from("abc")), "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85");

function sha256(raw) {
  return createHash("sha256").update(raw).digest("hex");
}

function sortJson(value) {
  if (Array.isArray(value)) return value.map(sortJson);
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0).map(
        ([key, child]) => [key, sortJson(child)],
      ),
    );
  }
  if (typeof value === "number") {
    assert.ok(Number.isFinite(value), "canonical JSON refuses non-finite numbers");
  }
  return value;
}

export function canonicalJson(value) {
  return `${JSON.stringify(sortJson(value))}\n`;
}

async function regularBytes(path, label, maximum = MAX_JSON_BYTES) {
  const pathBefore = await lstat(path);
  assert.ok(pathBefore.isFile() && !pathBefore.isSymbolicLink(), `${label} must be a non-symlink regular file`);
  const handle = await open(path, fsConstants.O_RDONLY | (fsConstants.O_NOFOLLOW ?? 0));
  try {
    const before = await handle.stat();
    assert.deepEqual([before.dev, before.ino], [pathBefore.dev, pathBefore.ino], `${label} path changed before it was opened`);
    assert.ok(before.isFile() && before.size > 0 && before.size <= maximum, `${label} has an unsupported size`);
    const raw = await handle.readFile();
    const [after, pathAfter] = await Promise.all([handle.stat(), lstat(path)]);
    assert.deepEqual(
      [after.dev, after.ino, after.size, after.mtimeMs, after.ctimeMs, after.nlink],
      [before.dev, before.ino, before.size, before.mtimeMs, before.ctimeMs, before.nlink],
      `${label} changed while its opened descriptor was read`,
    );
    assert.deepEqual(
      [pathAfter.dev, pathAfter.ino, pathAfter.size, pathAfter.mtimeMs, pathAfter.ctimeMs, pathAfter.nlink],
      [after.dev, after.ino, after.size, after.mtimeMs, after.ctimeMs, after.nlink],
      `${label} path no longer names the opened file`,
    );
    return raw;
  } finally {
    await handle.close();
  }
}

async function jsonFile(path, label) {
  const raw = await regularBytes(path, label);
  return { raw, value: JSON.parse(raw.toString("utf8")) };
}

async function privateJsonFile(path, label) {
  const value = await jsonFile(path, label);
  const info = await lstat(path);
  if (process.platform !== "win32") {
    assert.equal(info.mode & 0o777, 0o400, `${label} must be mode 0400`);
    assert.equal(info.nlink, 1, `${label} must have link-count one`);
  }
  return value;
}

function expectedHash(value, label) {
  assert.ok(typeof value === "string" && HASH_RE.test(value), `${label} must be a lowercase SHA-256`);
  return value;
}

function canonicalHash(value, label) {
  const normalized = network.normalizeHex(value, 32);
  assert.ok(normalized && value === `0x${normalized}`, `${label} must be canonical lowercase 0x-prefixed 32-byte hex`);
  return value;
}

function flexibleHash(value, label) {
  const normalized = network.normalizeHex(value, 32);
  assert.ok(normalized, `${label} must be an exact 32-byte hash`);
  return `0x${normalized}`;
}

function nonnegativeInteger(value, label) {
  assert.ok(Number.isSafeInteger(value) && value >= 0, `${label} must be a non-negative safe integer`);
  return value;
}

function exactKeys(value, expected, label) {
  assert.ok(value && typeof value === "object" && !Array.isArray(value), `${label} must be an object`);
  assert.deepEqual(Object.keys(value).sort(), [...expected].sort(), `${label} fields differ from the reviewed RPC contract`);
}

function collectSpecs(suites, output = []) {
  for (const suite of suites ?? []) {
    for (const spec of suite.specs ?? []) output.push(spec);
    collectSpecs(suite.suites, output);
  }
  return output;
}

export function validatePlaywrightReport(report, packageLock) {
  assert.ok(Array.isArray(report?.errors) && report.errors.length === 0, "Playwright reported top-level errors");
  assert.equal(report?.config?.fullyParallel, false, "live suite must be sequential");
  assert.equal(report?.config?.forbidOnly, true, "live suite must reject focused tests");
  assert.equal(report?.config?.workers, 1, "live suite must use exactly one worker");
  assert.equal(report?.config?.projects?.length, 1, "live suite must use exactly one browser project");
  const project = report.config.projects[0];
  assert.equal(project.name, "chromium", "live suite must use Chromium");
  assert.equal(project.retries, 0, "live suite must not hide failures behind retries");
  assert.deepEqual(project.testMatch, ["**/live.spec.ts"], "live suite selected another test set");
  const lockedVersion = packageLock?.packages?.["node_modules/@playwright/test"]?.version;
  assert.equal(report.config.version, lockedVersion, "Playwright runtime differs from package-lock.json");

  const specs = collectSpecs(report.suites);
  assert.deepEqual(
    specs.map((spec) => spec.title).sort(),
    [...EXPECTED_TESTS].sort(),
    "Playwright report does not contain the exact reviewed live tests",
  );
  for (const spec of specs) {
    assert.equal(spec.file, "live.spec.ts", `${spec.title} came from another file`);
    assert.equal(spec.ok, true, `${spec.title} did not pass`);
    assert.equal(spec.tests?.length, 1, `${spec.title} must execute once`);
    const test = spec.tests[0];
    assert.equal(test.expectedStatus, "passed", `${spec.title} expected a non-pass result`);
    assert.equal(test.status, "expected", `${spec.title} was skipped, flaky, or unexpected`);
    assert.equal(test.results?.length, 1, `${spec.title} did not run exactly once`);
    assert.equal(test.results[0].status, "passed", `${spec.title} did not finish passed`);
  }
  assert.deepEqual(
    {
      expected: report?.stats?.expected,
      skipped: report?.stats?.skipped,
      unexpected: report?.stats?.unexpected,
      flaky: report?.stats?.flaky,
    },
    { expected: EXPECTED_TESTS.length, skipped: 0, unexpected: 0, flaky: 0 },
    "Playwright summary is not an exact clean live-suite pass",
  );
  assert.ok(typeof report.stats.startTime === "string" && !Number.isNaN(Date.parse(report.stats.startTime)), "Playwright start time is invalid");
  assert.ok(Number.isFinite(report.stats.duration) && report.stats.duration >= 0, "Playwright duration is invalid");
  return {
    durationMs: report.stats.duration,
    playwrightVersion: report.config.version,
    startedAt: new Date(report.stats.startTime).toISOString(),
    testCount: EXPECTED_TESTS.length,
  };
}

function validateDirectReceipt(payload, expectedTx, expectedWorker) {
  exactKeys(payload, DIRECT_RECEIPT_KEYS, "direct reward receipt");
  const classification = network.classifyCommunityRewardReceipt(payload, expectedTx);
  assert.ok(classification, "direct reward receipt failed the strict shared product parser");
  assert.equal(classification.status, "mined_success", "canary reward receipt is not mined_success");
  assert.equal(classification.paymentConfirmed, true, "canary payment is not confirmed");
  assert.equal(payload.tx_hash, expectedTx, "direct reward receipt returned another transaction");
  assert.equal(payload.worker, expectedWorker, "direct reward receipt returned another worker");
  return payload;
}

function validateEarningsReceipt(row, index) {
  exactKeys(row, EARNINGS_RECEIPT_KEYS, `earnings receipt ${index}`);
  for (const field of [
    "tx_hash", "job_id", "worker", "model_id", "input_hash", "output_hash",
    "assignment_epoch", "transaction_domain", "block_hash",
  ]) canonicalHash(row[field], `earnings receipt ${index}.${field}`);
  assert.equal(row.tx_type, "0x25", `earnings receipt ${index} is not type 0x25`);
  for (const field of ["recovery_epoch", "validator_set_id", "block_height", "index"]) {
    nonnegativeInteger(row[field], `earnings receipt ${index}.${field}`);
  }
  assert.equal(row.submitted, true, `earnings receipt ${index} was not submitted`);
  assert.equal(row.included, true, `earnings receipt ${index} was not included`);
  assert.equal(row.confirmed, true, `earnings receipt ${index} was not confirmed`);
  assert.equal(row.success, true, `earnings receipt ${index} did not succeed`);
  assert.equal(row.reward_base, REWARD_BASE, `earnings receipt ${index} has another base reward`);
  assert.equal(row.reward_arc, REWARD_ARC, `earnings receipt ${index} has another ARC reward`);
  assert.equal(row.receipt_url, `/community/reward_receipt/${row.tx_hash}`, `earnings receipt ${index} URL is not self-bound`);
  return row;
}

function validateEarnings(payload, expectedTx, expectedWorker, direct) {
  assert.ok(payload && typeof payload === "object" && !Array.isArray(payload), "worker earnings must be an object");
  assert.equal(canonicalHash(payload.address, "earnings.address"), expectedWorker, "earnings returned another worker");
  const rows = payload.confirmed_receipts;
  assert.ok(Array.isArray(rows), "earnings confirmed_receipts must be an array");
  const validated = rows.map(validateEarningsReceipt);
  const txs = validated.map((row) => row.tx_hash);
  const jobs = validated.map((row) => row.job_id);
  assert.equal(new Set(txs).size, txs.length, "earnings contains duplicate reward transactions");
  assert.equal(new Set(jobs).size, jobs.length, "earnings contains duplicate reward jobs");
  const count = nonnegativeInteger(payload.confirmed_receipt_count, "earnings.confirmed_receipt_count");
  assert.equal(payload.total_rewards, count, "earnings total_rewards differs from confirmed count");
  assert.equal(rows.length, count, "earnings row count differs from confirmed count");
  assert.equal(payload.confirmed_gross_earnings_base, count * REWARD_BASE, "earnings base total does not reconcile to receipts");
  assert.equal(payload.confirmed_gross_earnings_arc, count * REWARD_ARC, "earnings ARC total does not reconcile to receipts");
  assert.equal(payload.estimated_total_arc, count * REWARD_ARC, "earnings estimate does not reconcile to receipts");
  assert.equal(payload.reward_per_attestation_base, REWARD_BASE, "earnings base reward policy differs");
  assert.equal(payload.reward_per_attestation_arc, REWARD_ARC, "earnings ARC reward policy differs");
  assert.equal(payload.source, RETAINED_SOURCE, "earnings source is not the canonical retained receipt index");
  assert.equal(payload.history_domain, HISTORY_DOMAIN, "earnings history domain differs");
  assert.equal(typeof payload.archive_mode, "boolean", "earnings archive_mode must be boolean");
  assert.equal(typeof payload.history_complete_since_recovery, "boolean", "earnings history completeness must be boolean");
  assert.equal(
    payload.history_scope,
    payload.archive_mode ? ARCHIVE_SCOPE : RETAINED_SCOPE,
    "earnings history scope differs from archive mode",
  );
  assert.equal(payload.history_complete_since_recovery, payload.archive_mode, "earnings history completeness differs from archive mode");
  assert.equal(payload.community_rewards_v1_enabled, true, "worker reward issuance is not ready");
  assert.equal(payload.community_rewards_v1_protocol_active, true, "worker reward protocol is inactive");
  assert.equal(payload.community_rewards_v1_approval_collection_ready, true, "validator approval collection is not ready");
  assert.equal(payload.issuance_ready_for_worker, true, "worker is not issuance-ready");
  assert.equal(payload.worker_min_stake_base, 0, "worker minimum stake is not zero");
  assert.equal(payload.stake_zero_eligible, true, "stake-zero worker is not eligible");
  assert.equal(payload.recovery_epoch, direct.recovery_epoch, "earnings recovery epoch differs from direct receipt");
  assert.equal(payload.validator_set_id, direct.validator_set_id, "earnings validator set differs from direct receipt");
  assert.equal(payload.validator_set_commitment, direct.validator_set_commitment, "earnings validator commitment differs from direct receipt");

  const projectionAvailable = typeof payload.projected_daily_arc === "number"
    && Number.isFinite(payload.projected_daily_arc)
    && payload.projected_daily_arc >= 0
    && payload.projected_daily_unavailable_reason === null;
  const projectionUnavailable = payload.projected_daily_arc === null
    && typeof payload.projected_daily_unavailable_reason === "string"
    && payload.projected_daily_unavailable_reason.trim().length > 0;
  assert.ok(projectionAvailable || projectionUnavailable, "earnings projection lacks an exact value/reason XOR");

  const canaries = validated.filter((row) => row.tx_hash === expectedTx);
  assert.equal(canaries.length, 1, "earnings does not contain the exact canary once");
  const canary = canaries[0];
  for (const [earningsField, directField = earningsField] of [
    ["tx_hash"], ["job_id"], ["worker"], ["model_id"], ["input_hash"],
    ["output_hash"], ["assignment_epoch"], ["transaction_domain"],
    ["recovery_epoch"], ["validator_set_id"], ["block_height"], ["block_hash"],
    ["index"], ["reward_base"], ["reward_arc"], ["receipt_url"],
  ]) {
    assert.equal(canary[earningsField], direct[directField], `earnings canary ${earningsField} differs from direct receipt`);
  }
  return {
    address: payload.address,
    archiveMode: payload.archive_mode,
    canaryReceipt: canary,
    communityRewardsV1ApprovalCollectionReady:
      payload.community_rewards_v1_approval_collection_ready,
    communityRewardsV1Enabled: payload.community_rewards_v1_enabled,
    communityRewardsV1ProtocolActive: payload.community_rewards_v1_protocol_active,
    confirmedGrossEarningsArc: payload.confirmed_gross_earnings_arc,
    confirmedGrossEarningsBase: payload.confirmed_gross_earnings_base,
    confirmedReceiptCount: count,
    historyCompleteSinceRecovery: payload.history_complete_since_recovery,
    historyDomain: payload.history_domain,
    historyScope: payload.history_scope,
    issuanceReadyForWorker: payload.issuance_ready_for_worker,
    projectedDailyArc: payload.projected_daily_arc,
    projectedDailyUnavailableReason: payload.projected_daily_unavailable_reason,
    recoveryEpoch: payload.recovery_epoch,
    rewardPerAttestationArc: payload.reward_per_attestation_arc,
    rewardPerAttestationBase: payload.reward_per_attestation_base,
    source: payload.source,
    stakeZeroEligible: payload.stake_zero_eligible,
    validatorSetCommitment: payload.validator_set_commitment,
    validatorSetId: payload.validator_set_id,
    workerMinStakeBase: payload.worker_min_stake_base,
  };
}

function validateBlockReceipt(payload, expectedTx, direct) {
  exactKeys(payload, BLOCK_RECEIPT_KEYS, "block receipt");
  const txHash = flexibleHash(payload.tx_hash, "block receipt transaction");
  const blockHash = flexibleHash(payload.block_hash, "block receipt block hash");
  assert.equal(txHash, expectedTx, "block receipt returned another transaction");
  assert.equal(blockHash, direct.block_hash, "block receipt hash differs from direct reward receipt");
  for (const field of ["block_height", "index", "gas_used"]) {
    nonnegativeInteger(payload[field], `block receipt.${field}`);
  }
  assert.equal(payload.block_height, direct.block_height, "block receipt height differs from direct reward receipt");
  assert.equal(payload.index, direct.index, "block receipt index differs from direct reward receipt");
  assert.equal(payload.success, true, "block receipt did not succeed");
  assert.equal(payload.value_commitment, null, "reward block receipt must not carry a value commitment");
  assert.equal(payload.inclusion_proof, null, "reward block receipt must use the production null inline-proof contract");
  assert.deepEqual(payload.logs, [], "reward block receipt must not claim execution logs");
  return {
    blockHash,
    blockHeight: payload.block_height,
    gasUsed: payload.gas_used,
    index: payload.index,
    success: true,
    txHash,
  };
}

async function sourceTreeHash(root) {
  const sourceRoot = resolve(root, "desktop/src");
  const sourceInfo = await lstat(sourceRoot);
  assert.ok(sourceInfo.isDirectory() && !sourceInfo.isSymbolicLink(), "desktop app source root must be a non-symlink directory");
  const entries = [];
  async function walk(directory) {
    const children = await readdir(directory, { withFileTypes: true });
    children.sort((a, b) => a.name < b.name ? -1 : a.name > b.name ? 1 : 0);
    for (const child of children) {
      const path = resolve(directory, child.name);
      const info = await lstat(path);
      assert.ok(!info.isSymbolicLink(), `desktop app source contains symlink ${path}`);
      if (info.isDirectory()) await walk(path);
      else {
        assert.ok(info.isFile(), `desktop app source contains unsupported entry ${path}`);
        const name = relative(root, path).split(sep).join("/");
        entries.push({ path: name, sha256: sha256(await regularBytes(path, name, 4 * 1024 * 1024)) });
      }
    }
  }
  await walk(sourceRoot);
  assert.ok(entries.length > 0, "desktop app source tree is empty");
  return sha256(Buffer.from(canonicalJson(entries)));
}

export async function appBundleTreeSha256(root, label = "app bundle") {
  const canonicalRoot = await realpath(root);
  const rootInfo = await lstat(root);
  assert.ok(rootInfo.isDirectory() && !rootInfo.isSymbolicLink(), `${label} root must be a real directory`);
  const entries = [];
  let totalBytes = 0;
  async function walk(directory, depth) {
    assert.ok(depth <= MAX_BUNDLE_DEPTH, `${label} exceeds the reviewed traversal depth`);
    const children = await readdir(directory, { withFileTypes: true });
    children.sort((a, b) => a.name < b.name ? -1 : a.name > b.name ? 1 : 0);
    for (const child of children) {
      assert.ok(entries.length < MAX_BUNDLE_ENTRIES, `${label} exceeds the reviewed member count`);
      const path = resolve(directory, child.name);
      const info = await lstat(path);
      const member = relative(canonicalRoot, path).split(sep).join("/");
      assert.ok(member && !member.startsWith("../") && !member.includes("\t") && !member.includes("\n") && !member.includes("\r"), `${label} has an invalid member path`);
      const mode = info.mode & 0o7777;
      if (info.isSymbolicLink()) {
        const target = await readlink(path);
        assert.ok(!isAbsolute(target) && !target.includes("\t") && !target.includes("\n") && !target.includes("\r"), `${label} has an unsafe symlink target`);
        const resolved = await realpath(path);
        const resolvedMember = relative(canonicalRoot, resolved);
        assert.ok(resolvedMember && !resolvedMember.startsWith(`..${sep}`) && !isAbsolute(resolvedMember), `${label} symlink escapes the app bundle`);
        entries.push({ kind: "symlink", mode, path: member, sha256: null, size: null, target });
      } else if (info.isDirectory()) {
        entries.push({ kind: "directory", mode, path: member, sha256: null, size: null, target: null });
        await walk(path, depth + 1);
      } else {
        assert.ok(info.isFile(), `${label} has a special filesystem member`);
        assert.ok(info.size <= MAX_BUNDLE_MEMBER_BYTES, `${label} member is too large`);
        totalBytes += info.size;
        assert.ok(totalBytes <= MAX_BUNDLE_BYTES, `${label} exceeds the reviewed total byte count`);
        const raw = await regularBytes(path, `${label} ${member}`, MAX_BUNDLE_MEMBER_BYTES);
        entries.push({ kind: "file", mode, path: member, sha256: sha256(raw), size: raw.length, target: null });
      }
    }
  }
  await walk(canonicalRoot, 0);
  assert.ok(entries.length > 0, `${label} is empty`);
  return sha256(Buffer.from(canonicalJson(entries)));
}

async function validatePackagedAppImage(options, sourceCommit, repositoryRoot) {
  assert.ok(
    typeof options.appImageReceipt === "string" && isAbsolute(options.appImageReceipt),
    "packaged AppImage host receipt path must be absolute",
  );
  const hostFile = await privateJsonFile(
    options.appImageReceipt,
    "packaged AppImage host receipt",
  );
  assert.equal(
    hostFile.raw.toString("utf8"),
    canonicalJson(hostFile.value),
    "packaged AppImage host receipt is not canonical JSON",
  );
  const receipt = hostFile.value;
  exactKeys(receipt, [
    "completed_at", "disposable_vm", "guest_receipt", "host_runtime",
    "inference_attempt", "platform_claim", "release", "result", "schema",
    "source", "transport", "updater_signature",
  ], "packaged AppImage host receipt");
  assert.equal(receipt.schema, PACKAGED_APPIMAGE_HOST_SCHEMA);
  assert.equal(receipt.result, "passed");
  assert.equal(receipt.platform_claim, PACKAGED_APPIMAGE_PLATFORM_CLAIM);
  assert.ok(Number.isFinite(Date.parse(receipt.completed_at)), "packaged AppImage completion time is invalid");

  exactKeys(receipt.release, ["assets", "binding_sha256", "commit", "release_id", "repository", "tag"], "packaged AppImage release");
  assert.equal(receipt.release.repository, "FerrumVir/arc-chain");
  assert.equal(receipt.release.tag, "v0.8.0");
  assert.equal(receipt.release.commit, sourceCommit);
  assert.ok(Number.isSafeInteger(receipt.release.release_id) && receipt.release.release_id > 0);
  expectedHash(receipt.release.binding_sha256, "packaged AppImage release binding");
  const appImageAssets = receipt.release.assets;
  exactKeys(appImageAssets, [
    "arc-desktop-linux-x86_64.AppImage",
    "arc-desktop-linux-x86_64.AppImage.sig",
    "arc-node-linux-x86_64",
  ], "packaged AppImage assets");
  const appImageAssetIds = new Set();
  for (const [name, asset] of Object.entries(appImageAssets)) {
    exactKeys(asset, ["id", "name", "sha256", "size"], `packaged AppImage asset ${name}`);
    assert.equal(asset.name, name);
    assert.ok(Number.isSafeInteger(asset.id) && asset.id > 0);
    assert.ok(!appImageAssetIds.has(asset.id), "packaged AppImage asset IDs are not distinct");
    appImageAssetIds.add(asset.id);
    assert.ok(Number.isSafeInteger(asset.size) && asset.size > 0);
    expectedHash(asset.sha256, `packaged AppImage asset ${name}`);
  }

  exactKeys(receipt.source, ["commit", "gate_path", "gate_sha256", "tree_clean"], "packaged AppImage source");
  assert.equal(receipt.source.commit, sourceCommit);
  assert.equal(receipt.source.gate_path, "scripts/release/packaged-appimage-live-gate.py");
  assert.equal(receipt.source.tree_clean, true);
  const gateRaw = await regularBytes(
    resolve(repositoryRoot, receipt.source.gate_path),
    "packaged AppImage gate source",
    4 * 1024 * 1024,
  );
  assert.equal(receipt.source.gate_sha256, sha256(gateRaw), "packaged AppImage gate source differs");

  exactKeys(receipt.inference_attempt, ["path", "plan_sha256", "sha256", "state"], "packaged AppImage attempt");
  assert.equal(receipt.inference_attempt.path, "inference-attempt.json");
  assert.equal(receipt.inference_attempt.state, "armed-no-retry");
  expectedHash(receipt.inference_attempt.plan_sha256, "packaged AppImage inference plan");
  expectedHash(receipt.inference_attempt.sha256, "packaged AppImage inference attempt");
  exactKeys(receipt.guest_receipt, ["path", "schema", "sha256"], "packaged AppImage guest receipt");
  assert.equal(receipt.guest_receipt.schema, "arc.packaged-appimage-live-product.v1");
  assert.ok(
    typeof receipt.guest_receipt.path === "string"
      && !receipt.guest_receipt.path.startsWith("/")
      && !receipt.guest_receipt.path.split("/").includes(".."),
    "packaged AppImage guest receipt path is unsafe",
  );
  expectedHash(receipt.guest_receipt.sha256, "packaged AppImage guest receipt");

  exactKeys(receipt.disposable_vm, [
    "config_sha256", "deleted_after_evidence_copy", "image_digest", "image_url",
    "mounts", "name", "preexisting_instances_unchanged", "recovery_enclave_accessed",
  ], "packaged AppImage disposable VM");
  assert.equal(receipt.disposable_vm.deleted_after_evidence_copy, true);
  assert.equal(receipt.disposable_vm.preexisting_instances_unchanged, true);
  assert.equal(receipt.disposable_vm.recovery_enclave_accessed, false);
  assert.deepEqual(receipt.disposable_vm.mounts, []);
  assert.equal(receipt.disposable_vm.image_digest, "sha256:5c3ddb00f60bc455dac0862fabe9d8bacec46c33ac1751143c5c3683404b110d");
  assert.equal(receipt.disposable_vm.image_url, "https://cloud-images.ubuntu.com/releases/noble/release-20260321/ubuntu-24.04-server-cloudimg-amd64.img");
  assert.match(receipt.disposable_vm.name, /^arc-packaged-live-v080-[0-9a-f]{8}-[a-z0-9]{6}$/);
  expectedHash(receipt.disposable_vm.config_sha256, "packaged AppImage VM config");

  exactKeys(receipt.host_runtime, ["limactl", "limactl_version", "ssh"], "packaged AppImage host runtime");
  assert.equal(receipt.host_runtime.limactl_version, "limactl version 2.1.1");
  for (const [name, identity] of [["limactl", receipt.host_runtime.limactl], ["ssh", receipt.host_runtime.ssh]]) {
    exactKeys(identity, ["path", "resolved_path", "sha256", "size"], `packaged AppImage ${name} identity`);
    assert.ok(typeof identity.path === "string" && isAbsolute(identity.path));
    assert.ok(typeof identity.resolved_path === "string" && isAbsolute(identity.resolved_path));
    assert.ok(Number.isSafeInteger(identity.size) && identity.size > 0);
    expectedHash(identity.sha256, `packaged AppImage ${name}`);
  }

  exactKeys(receipt.transport, [
    "accepted_connect_count", "identity_sha256", "known_hosts_sha256",
    "relay_log_sha256", "relay_target", "relay_token_sha256", "remote",
    "ssh_options_sha256", "ssh_stderr_sha256", "ssh_stdout_sha256",
  ], "packaged AppImage transport");
  assert.equal(receipt.transport.relay_target, "140.82.16.112:443");
  assert.equal(receipt.transport.remote, "root@140.82.16.112:127.0.0.1:443");
  assert.ok(Number.isSafeInteger(receipt.transport.accepted_connect_count) && receipt.transport.accepted_connect_count > 0);
  for (const field of [
    "identity_sha256", "known_hosts_sha256", "relay_log_sha256",
    "relay_token_sha256", "ssh_options_sha256", "ssh_stderr_sha256",
    "ssh_stdout_sha256",
  ]) expectedHash(receipt.transport[field], `packaged AppImage transport ${field}`);

  exactKeys(receipt.updater_signature, [
    "appimage_sha256", "minisign_binary", "minisign_package",
    "public_key_sha256", "schema", "signature_sha256", "stderr_sha256",
    "stdout_sha256", "updater_signature_verified",
  ], "packaged AppImage updater signature");
  assert.equal(receipt.updater_signature.schema, "arc.packaged-appimage-updater-signature.v1");
  assert.equal(receipt.updater_signature.updater_signature_verified, true);
  assert.equal(receipt.updater_signature.appimage_sha256, appImageAssets["arc-desktop-linux-x86_64.AppImage"].sha256);
  assert.equal(receipt.updater_signature.signature_sha256, appImageAssets["arc-desktop-linux-x86_64.AppImage.sig"].sha256);
  for (const field of ["public_key_sha256", "stderr_sha256", "stdout_sha256"]) {
    expectedHash(receipt.updater_signature[field], `packaged AppImage signature ${field}`);
  }
  exactKeys(receipt.updater_signature.minisign_binary, ["sha256", "size"], "packaged AppImage minisign binary");
  expectedHash(receipt.updater_signature.minisign_binary.sha256, "packaged AppImage minisign binary");
  assert.ok(Number.isSafeInteger(receipt.updater_signature.minisign_binary.size) && receipt.updater_signature.minisign_binary.size > 0);
  exactKeys(receipt.updater_signature.minisign_package, ["architecture", "version"], "packaged AppImage minisign package");
  assert.equal(receipt.updater_signature.minisign_package.architecture, "amd64");
  assert.ok(typeof receipt.updater_signature.minisign_package.version === "string" && receipt.updater_signature.minisign_package.version.trim());

  return {
    gatePath: receipt.source.gate_path,
    gateSha256: receipt.source.gate_sha256,
    receipt,
    receiptSha256: sha256(hostFile.raw),
    scope: "linux-x86_64-packaged-ui-to-tauri-ipc-under-external-webkit-automation",
  };
}

function validateNativeNetwork(networkValue, expectedSource, minimumHeight, maximumAge) {
  exactKeys(networkValue, [
    "chainId", "dagCommitted", "dagRound", "declaresMainnet", "height",
    "hostVersion", "isBlockProducing", "isBlockProducingBasis",
    "lastBlockAgeSecs", "minActiveStake", "networkName",
    "networkNameUnavailableReason", "peers", "sourceHost", "unavailable",
    "validatorSplitDerived", "validators", "validatorsActive",
    "validatorsRegistered",
  ], "packaged native network");
  assert.equal(networkValue.sourceHost, expectedSource, "native network crossed coordinator origins");
  assert.equal(networkValue.unavailable, null, "native network is unavailable");
  assert.equal(networkValue.hostVersion, "0.8.0", "native network host version differs");
  assert.ok(Number.isSafeInteger(networkValue.height) && networkValue.height >= minimumHeight, "native network tip is below the accepted receipt");
  assert.ok(Number.isSafeInteger(networkValue.lastBlockAgeSecs) && networkValue.lastBlockAgeSecs <= maximumAge, "native network tip is stale");
  assert.equal(networkValue.isBlockProducing, true, "native network is not producing blocks");
  assert.equal(networkValue.validatorsActive, 6, "native network active validator count differs");
  assert.equal(networkValue.validatorsRegistered, 6, "native network registered validator count differs");
  assert.ok(Number.isSafeInteger(networkValue.peers) && networkValue.peers >= 5, "native network has too few peers");
  assert.ok(Array.isArray(networkValue.validators) && networkValue.validators.length === 6, "native network lacks the exact validator set");
  const identities = new Set();
  for (const validator of networkValue.validators) {
    exactKeys(validator, ["active", "address", "stake"], "packaged native validator");
    assert.ok(typeof validator.address === "string" && HASH_RE.test(validator.address), "native validator identity is malformed");
    assert.equal(validator.active, true, "native validator is inactive");
    assert.ok(Number.isSafeInteger(validator.stake) && validator.stake > 0, "native validator has no active stake");
    identities.add(validator.address);
  }
  assert.equal(identities.size, 6, "native validator identities are duplicated");
}

function validateMacosAssetMap(value, expected, label) {
  exactKeys(value, ["appArchive", "appArchiveSignature", "dmg"], label);
  assert.deepEqual(value, expected, `${label} differs from the native input assets`);
  const assetIds = new Set();
  for (const [name, asset] of Object.entries(value)) {
    exactKeys(asset, ["id", "name", "sha256", "size"], `${label} ${name}`);
    assert.ok(Number.isSafeInteger(asset.id) && asset.id > 0, `${label} ${name} ID is invalid`);
    assert.ok(!assetIds.has(asset.id), `${label} asset IDs are not distinct`);
    assetIds.add(asset.id);
    assert.ok(Number.isSafeInteger(asset.size) && asset.size > 0, `${label} ${name} size is invalid`);
    expectedHash(asset.sha256, `${label} ${name} hash`);
  }
}

function validateMacosBundle(value, expected, label) {
  exactKeys(value, [
    "appBundleTreeSha256", "executableRelativePath", "executableSha256", "executableSize",
  ], label);
  assert.deepEqual(value, expected, `${label} differs from the sealed native bundle`);
  expectedHash(value.appBundleTreeSha256, `${label} tree hash`);
  expectedHash(value.executableSha256, `${label} executable hash`);
  assert.equal(value.executableRelativePath, "Contents/MacOS/arc-desktop", `${label} executable path differs`);
  assert.ok(Number.isSafeInteger(value.executableSize) && value.executableSize > 0, `${label} executable size is invalid`);
}

function validateMacosControllerSource(value, sourceCommit, controllerSha256, label) {
  exactKeys(value, ["commit", "path", "sha256", "treeClean"], label);
  assert.deepEqual(value, {
    commit: sourceCommit,
    path: MACOS_PACKAGE_CONTROLLER_RELATIVE,
    sha256: controllerSha256,
    treeClean: true,
  }, `${label} differs from the exact checked-in controller`);
}

function validateMacosRelease(value, input, label) {
  exactKeys(value, ["id", "runAttempt", "runId", "tag"], label);
  assert.deepEqual(value, {
    id: input.releaseId,
    runAttempt: input.releaseRunAttempt,
    runId: input.releaseRunId,
    tag: "v0.8.0",
  }, `${label} differs from the native release binding`);
}

function validateMacosCodeSignature(value, label) {
  exactKeys(value, [
    "appleDeveloperIdSigned", "authorities", "designatedRequirement",
    "designatedRequirementKind", "displayStderrSha256", "displayStdoutSha256",
    "gatekeeperAssessed", "hardenedRuntime", "identifier", "infoPlistSha256",
    "kind", "notarizationAssessed", "requirementsStderrSha256",
    "requirementsStdoutSha256", "teamIdentifier", "verifier",
    "verifyDeepStrict", "verifyStderrSha256", "verifyStdoutSha256",
  ], label);
  assert.equal(value.appleDeveloperIdSigned, false, `${label} falsely claims Developer ID signing`);
  assert.deepEqual(value.authorities, [], `${label} must not invent a signing authority`);
  assert.equal(value.gatekeeperAssessed, false, `${label} falsely claims Gatekeeper assessment`);
  assert.equal(value.identifier, "network.arc.desktop", `${label} bundle identifier differs`);
  assert.equal(value.kind, "adhoc", `${label} must truthfully report the ad-hoc code seal`);
  assert.equal(value.notarizationAssessed, false, `${label} falsely claims notarization assessment`);
  assert.equal(value.teamIdentifier, null, `${label} must not invent an Apple team identity`);
  assert.equal(value.verifyDeepStrict, true, `${label} did not pass codesign --deep --strict`);
  assert.ok(typeof value.hardenedRuntime === "boolean", `${label} hardened-runtime observation is invalid`);
  assert.ok(
    typeof value.designatedRequirement === "string"
      && (value.designatedRequirement.includes('identifier "network.arc.desktop"')
        || /^designated => cdhash H"[0-9a-f]{40}"(?: or cdhash H"[0-9a-f]{40}")*$/.test(value.designatedRequirement)),
    `${label} designated requirement differs`,
  );
  assert.ok(["identifier", "cdhash"].includes(value.designatedRequirementKind), `${label} designated requirement kind differs`);
  for (const field of [
    "displayStderrSha256", "displayStdoutSha256", "infoPlistSha256",
    "requirementsStderrSha256", "requirementsStdoutSha256",
    "verifyStderrSha256", "verifyStdoutSha256",
  ]) expectedHash(value[field], `${label} ${field}`);
  exactKeys(value.verifier, ["path", "resolvedPath", "sha256", "size"], `${label} verifier`);
  assert.equal(value.verifier.path, "/usr/bin/codesign", `${label} used another verifier path`);
  assert.equal(value.verifier.resolvedPath, "/usr/bin/codesign", `${label} verifier resolved elsewhere`);
  expectedHash(value.verifier.sha256, `${label} verifier hash`);
  assert.ok(Number.isSafeInteger(value.verifier.size) && value.verifier.size > 0, `${label} verifier size is invalid`);
  return Object.fromEntries([
    "appleDeveloperIdSigned", "authorities", "designatedRequirement",
    "designatedRequirementKind", "gatekeeperAssessed", "hardenedRuntime",
    "identifier", "infoPlistSha256", "kind", "notarizationAssessed",
    "teamIdentifier", "verifyDeepStrict",
  ].map((field) => [field, value[field]]));
}

function validateMacosMount(value, label, expectedBasename = "native-dmg-mount") {
  exactKeys(value, [
    "attachPlistSha256", "device", "diskutilInfoPlistSha256", "filesystem",
    "hdiutilInfoPlistSha256", "imagePathMatched", "imagePathSha256",
    "mountFlags", "mountPointBasename", "nobrowse", "noowners",
    "readOnlyMedia", "readOnlyVolume", "statvfsReadOnly", "tools",
    "verifyStderrSha256", "verifyStdoutSha256", "writeable",
  ], label);
  assert.equal(value.mountPointBasename, expectedBasename, `${label} mount point differs`);
  assert.equal(value.imagePathMatched, true, `${label} did not bind the exact DMG path`);
  assert.equal(value.nobrowse, true, `${label} was browser-visible`);
  assert.equal(value.noowners, true, `${label} enabled filesystem ownership`);
  assert.equal(value.readOnlyMedia, true, `${label} media is not read-only`);
  assert.equal(value.readOnlyVolume, true, `${label} volume is not read-only`);
  assert.equal(value.statvfsReadOnly, true, `${label} statvfs is not read-only`);
  assert.equal(value.writeable, false, `${label} source was writable`);
  assert.match(value.device, /^\/dev\/disk[0-9]+(?:s[0-9]+)*$/, `${label} device differs`);
  assert.ok(typeof value.filesystem === "string" && value.filesystem, `${label} filesystem is absent`);
  exactKeys(value.mountFlags, ["lineSha256", "options"], `${label} flags`);
  assert.ok(
    Array.isArray(value.mountFlags.options)
      && ["read-only", "noowners", "nobrowse"].every((flag) => value.mountFlags.options.includes(flag))
      && !value.mountFlags.options.includes("read-write"),
    `${label} flags are not the reviewed read-only set`,
  );
  for (const field of [
    "attachPlistSha256", "diskutilInfoPlistSha256", "hdiutilInfoPlistSha256",
    "imagePathSha256", "verifyStderrSha256", "verifyStdoutSha256",
  ]) expectedHash(value[field], `${label} ${field}`);
  exactKeys(value.tools, ["codesign", "diskutil", "hdiutil", "mount"], `${label} tools`);
  const expectedTools = {
    codesign: "/usr/bin/codesign",
    diskutil: "/usr/sbin/diskutil",
    hdiutil: "/usr/bin/hdiutil",
    mount: "/sbin/mount",
  };
  for (const [name, tool] of Object.entries(value.tools)) {
    exactKeys(tool, ["path", "resolvedPath", "sha256", "size"], `${label} ${name}`);
    assert.equal(tool.path, expectedTools[name], `${label} ${name} path differs`);
    assert.equal(tool.resolvedPath, expectedTools[name], `${label} ${name} resolved elsewhere`);
    expectedHash(tool.sha256, `${label} ${name} hash`);
    assert.ok(Number.isSafeInteger(tool.size) && tool.size > 0, `${label} ${name} size is invalid`);
  }
}

function validateMacosDetach(value, label) {
  exactKeys(value, [
    "detachStderrSha256", "detachStdoutSha256", "detached",
    "mountPointEmpty", "postDetachInfoPlistSha256",
  ], label);
  assert.equal(value.detached, true, `${label} did not detach`);
  assert.equal(value.mountPointEmpty, true, `${label} left a mounted entry`);
  for (const field of [
    "detachStderrSha256", "detachStdoutSha256", "postDetachInfoPlistSha256",
  ]) expectedHash(value[field], `${label} ${field}`);
}

function validateMacosTree(value, expectedTreeSha256, label) {
  exactKeys(value, ["entryCount", "sha256", "totalRegularBytes"], label);
  assert.ok(Number.isSafeInteger(value.entryCount) && value.entryCount > 0, `${label} entry count is invalid`);
  assert.ok(Number.isSafeInteger(value.totalRegularBytes) && value.totalRegularBytes > 0, `${label} byte count is invalid`);
  assert.equal(value.sha256, expectedTreeSha256, `${label} differs from the complete bundle tree`);
}

function validateMacosRootInventory(value, label) {
  assert.ok(Array.isArray(value) && value.length >= 1 && value.length <= 64, `${label} size differs`);
  const names = [];
  let appCount = 0;
  for (const [index, row] of value.entries()) {
    exactKeys(row, ["kind", "mode", "name", "target"], `${label} ${index}`);
    assert.ok(
      typeof row.name === "string"
        && /^[\x20-\x7e]+$/.test(row.name)
        && !/[\r\n\t/]/.test(row.name)
        && Buffer.byteLength(row.name, "ascii") <= 255
        && !names.includes(row.name),
      `${label} ${index} name is unsafe or duplicated`,
    );
    assert.ok(["directory", "file", "symlink"].includes(row.kind), `${label} ${index} kind differs`);
    assert.ok(Number.isSafeInteger(row.mode) && row.mode >= 0 && row.mode <= 0o7777, `${label} ${index} mode differs`);
    assert.ok(row.target === null || typeof row.target === "string", `${label} ${index} target differs`);
    if (row.name.endsWith(".app")) {
      appCount += 1;
      assert.equal(row.name, "ARC Node.app", `${label} contains another app`);
      assert.equal(row.kind, "directory", `${label} app is not a directory`);
    }
    if (row.name === "Applications") assert.ok([null, "/Applications"].includes(row.target), `${label} Applications target differs`);
    names.push(row.name);
  }
  assert.deepEqual(names, [...names].sort(), `${label} is not canonically sorted`);
  assert.equal(appCount, 1, `${label} must contain exactly one ARC Node.app`);
}

function validateMacosExtraction(value, input, label) {
  exactKeys(value, [
    "appBundleRelativePath", "appBundleTreeSha256", "archiveSha256AfterExtraction",
    "implementation", "limits", "manifest", "safety",
  ], label);
  assert.equal(value.appBundleRelativePath, "ARC Node.app");
  assert.equal(value.appBundleTreeSha256, input.expectedBundle.appBundleTreeSha256);
  assert.equal(value.archiveSha256AfterExtraction, input.assets.appArchive.sha256);
  assert.equal(value.implementation, "arc-openat-create-only-tar-extractor-v1");
  assert.deepEqual(value.limits, {
    maxDepth: 33,
    maxEntries: 20_000,
    maxMemberBytes: 512 * 1024 * 1024,
    maxPathBytes: 4_096,
    maxTotalRegularBytes: 2 * 1024 * 1024 * 1024,
  }, `${label} limits differ`);
  exactKeys(value.manifest, [
    "directoryCount", "memberCount", "regularFileCount", "symlinkCount", "totalRegularBytes",
  ], `${label} manifest`);
  for (const field of ["directoryCount", "memberCount", "regularFileCount", "symlinkCount", "totalRegularBytes"]) {
    assert.ok(Number.isSafeInteger(value.manifest[field]) && value.manifest[field] >= 0, `${label} ${field} is invalid`);
  }
  assert.ok(value.manifest.memberCount > 0 && value.manifest.regularFileCount > 0 && value.manifest.totalRegularBytes > 0, `${label} manifest is empty`);
  assert.ok(value.manifest.memberCount <= value.limits.maxEntries, `${label} manifest exceeds its member limit`);
  assert.ok(value.manifest.totalRegularBytes <= value.limits.maxTotalRegularBytes, `${label} manifest exceeds its byte limit`);
  assert.deepEqual(value.safety, {
    absolutePathsRejected: true,
    caseAndNormalizationCollisionsRejected: true,
    descriptorRelativeCreateOnly: true,
    hardlinksAndSpecialEntriesRejected: true,
    parentTraversalRejected: true,
    setIdAndStickyModesRejected: true,
    symlinksResolvedInsideBundle: true,
  }, `${label} safety contract differs`);
}

async function validateMacosPackageEvidence(
  options,
  inputFile,
  nativeFile,
  attemptFile,
  sourceCommit,
  repositoryRoot,
) {
  for (const [path, label] of [
    [options.macosPackageProvenance, "macOS package provenance"],
    [options.macosPackageProvenanceVerification, "macOS package provenance verification"],
    [options.macosPackageInspection, "macOS package inspection"],
    [options.macosUpdaterSignatureReceipt, "macOS updater-signature receipt"],
    [options.macosControllerAttempt, "macOS native controller attempt"],
  ]) assert.ok(typeof path === "string" && isAbsolute(path), `${label} path must be absolute`);
  const [provenanceFile, verificationFile, inspectionFile, signatureFile, controllerAttemptFile, controllerRaw] = await Promise.all([
    privateJsonFile(options.macosPackageProvenance, "macOS package provenance"),
    privateJsonFile(options.macosPackageProvenanceVerification, "macOS package provenance verification"),
    privateJsonFile(options.macosPackageInspection, "macOS package inspection"),
    privateJsonFile(options.macosUpdaterSignatureReceipt, "macOS updater-signature receipt"),
    privateJsonFile(options.macosControllerAttempt, "macOS native controller attempt"),
    regularBytes(resolve(repositoryRoot, MACOS_PACKAGE_CONTROLLER_RELATIVE), "macOS package controller", 4 * 1024 * 1024),
  ]);
  for (const [file, label] of [
    [provenanceFile, "macOS package provenance"],
    [verificationFile, "macOS package provenance verification"],
    [inspectionFile, "macOS package inspection"],
    [signatureFile, "macOS updater-signature receipt"],
    [controllerAttemptFile, "macOS native controller attempt"],
  ]) assert.equal(file.raw.toString("utf8"), canonicalJson(file.value), `${label} is not canonical JSON`);

  const controllerSha256 = sha256(controllerRaw);
  const input = inputFile.value;
  const native = nativeFile.value;
  const provenance = provenanceFile.value;
  exactKeys(provenance, [
    "assets", "bindingSha256", "bundle", "codeSignature", "completedAt",
    "controllerAttemptSha256", "dmgExecution", "extractedArchiveBundleAfter",
    "inspectionSha256", "nativeDispatchAttemptSha256", "nativeExecution",
    "nativeInputSha256", "nativeReceiptSha256", "release", "repository",
    "schema", "source", "sourceCommit", "truthScope",
    "updaterSignatureReceiptSha256",
  ], "macOS package provenance");
  assert.equal(provenance.schema, MACOS_PACKAGE_PROVENANCE_SCHEMA);
  assert.equal(provenance.repository, "FerrumVir/arc-chain");
  assert.equal(provenance.sourceCommit, sourceCommit);
  expectedHash(provenance.bindingSha256, "macOS package binding");
  validateMacosControllerSource(provenance.source, sourceCommit, controllerSha256, "macOS provenance source");
  validateMacosAssetMap(provenance.assets, input.assets, "macOS provenance assets");
  validateMacosBundle(provenance.bundle, input.expectedBundle, "macOS provenance bundle");
  validateMacosBundle(provenance.extractedArchiveBundleAfter, input.expectedBundle, "macOS extracted archive bundle");
  validateMacosRelease(provenance.release, input, "macOS provenance release");
  assert.deepEqual(provenance.truthScope, MACOS_PACKAGE_TRUTH_SCOPE, "macOS provenance truth scope differs");
  assert.equal(provenance.nativeInputSha256, sha256(inputFile.raw), "macOS provenance input hash differs");
  assert.equal(provenance.nativeReceiptSha256, sha256(nativeFile.raw), "macOS provenance native receipt hash differs");
  assert.equal(provenance.nativeDispatchAttemptSha256, sha256(attemptFile.raw), "macOS provenance native attempt hash differs");
  assert.equal(provenance.inspectionSha256, sha256(inspectionFile.raw), "macOS provenance inspection hash differs");
  assert.equal(provenance.updaterSignatureReceiptSha256, sha256(signatureFile.raw), "macOS provenance updater-signature hash differs");
  assert.equal(provenance.controllerAttemptSha256, sha256(controllerAttemptFile.raw), "macOS provenance controller-attempt hash differs");

  exactKeys(provenance.codeSignature, ["after", "before", "semanticIdentityUnchanged"], "macOS provenance code signature");
  assert.equal(provenance.codeSignature.semanticIdentityUnchanged, true);
  const codeBefore = validateMacosCodeSignature(provenance.codeSignature.before, "macOS pre-run code signature");
  const codeAfter = validateMacosCodeSignature(provenance.codeSignature.after, "macOS post-run code signature");
  assert.deepEqual(codeAfter, codeBefore, "macOS code-signature identity changed during execution");

  exactKeys(provenance.dmgExecution, ["attach", "bundleAfter", "bundleBefore", "detach", "treeAfter", "treeBefore"], "macOS DMG execution");
  validateMacosMount(provenance.dmgExecution.attach, "macOS native DMG mount");
  validateMacosBundle(provenance.dmgExecution.bundleBefore, input.expectedBundle, "macOS pre-run mounted bundle");
  validateMacosBundle(provenance.dmgExecution.bundleAfter, input.expectedBundle, "macOS post-run mounted bundle");
  assert.deepEqual(provenance.dmgExecution.treeAfter, provenance.dmgExecution.treeBefore, "macOS mounted tree changed during native execution");
  validateMacosTree(provenance.dmgExecution.treeBefore, input.expectedBundle.appBundleTreeSha256, "macOS mounted tree");
  validateMacosDetach(provenance.dmgExecution.detach, "macOS DMG detach");

  exactKeys(provenance.nativeExecution, [
    "completedAt", "elapsedMs", "innerMaxSeconds", "outerTimeoutSeconds",
    "returnCode", "startedAt", "stderrSha256", "stderrSize", "stdoutSha256",
    "stdoutSize", "timedOut",
  ], "macOS native execution");
  assert.equal(provenance.nativeExecution.innerMaxSeconds, 4_300);
  assert.equal(provenance.nativeExecution.outerTimeoutSeconds, 4_360);
  assert.equal(provenance.nativeExecution.returnCode, 0);
  assert.equal(provenance.nativeExecution.timedOut, false);
  assert.ok(Number.isSafeInteger(provenance.nativeExecution.elapsedMs) && provenance.nativeExecution.elapsedMs >= 0 && provenance.nativeExecution.elapsedMs <= 4_360_000);
  for (const field of ["stderrSha256", "stdoutSha256"]) expectedHash(provenance.nativeExecution[field], `macOS native execution ${field}`);
  for (const field of ["stderrSize", "stdoutSize"]) assert.ok(Number.isSafeInteger(provenance.nativeExecution[field]) && provenance.nativeExecution[field] >= 0 && provenance.nativeExecution[field] <= MAX_JSON_BYTES, `macOS native execution ${field} is invalid`);

  const controllerAttempt = controllerAttemptFile.value;
  exactKeys(controllerAttempt, [
    "armedAt", "challenge", "executableSha256", "inputSha256",
    "inspectionSha256", "outerTimeoutSeconds", "retryPermitted", "schema",
    "sourceCommit", "state",
  ], "macOS native controller attempt");
  assert.equal(controllerAttempt.schema, "arc.macos-native-controller-attempt.v1");
  assert.equal(controllerAttempt.challenge, input.challenge);
  assert.equal(controllerAttempt.executableSha256, input.expectedBundle.executableSha256);
  assert.equal(controllerAttempt.inputSha256, sha256(inputFile.raw));
  assert.equal(controllerAttempt.inspectionSha256, sha256(inspectionFile.raw));
  assert.equal(controllerAttempt.outerTimeoutSeconds, 4_360);
  assert.equal(controllerAttempt.retryPermitted, false);
  assert.equal(controllerAttempt.sourceCommit, sourceCommit);
  assert.equal(controllerAttempt.state, "armed-no-retry");

  const inspection = inspectionFile.value;
  exactKeys(inspection, [
    "assets", "bindingSha256", "bundle", "codeSignature", "completedAt",
    "dmg", "extraction", "release", "repository", "schema", "source",
    "sourceCommit", "updaterSignature",
  ], "macOS package inspection");
  assert.equal(inspection.schema, MACOS_PACKAGE_INSPECTION_SCHEMA);
  assert.equal(inspection.repository, "FerrumVir/arc-chain");
  assert.equal(inspection.sourceCommit, sourceCommit);
  assert.equal(inspection.bindingSha256, provenance.bindingSha256);
  assert.deepEqual(inspection.source, provenance.source);
  validateMacosAssetMap(inspection.assets, input.assets, "macOS inspection assets");
  validateMacosBundle(inspection.bundle, input.expectedBundle, "macOS inspection bundle");
  validateMacosRelease(inspection.release, input, "macOS inspection release");
  assert.deepEqual(validateMacosCodeSignature(inspection.codeSignature, "macOS inspected code signature"), codeBefore);
  validateMacosExtraction(inspection.extraction, input, "macOS package extraction");
  exactKeys(inspection.dmg, [
    "appBundleTree", "attach", "detach", "rootInventory", "sha256AfterDetach",
  ], "macOS inspection DMG");
  assert.equal(inspection.dmg.sha256AfterDetach, input.assets.dmg.sha256, "macOS inspection changed the DMG bytes");
  validateMacosMount(inspection.dmg.attach, "macOS inspection DMG mount", "inspect-dmg-mount");
  validateMacosTree(inspection.dmg.appBundleTree, input.expectedBundle.appBundleTreeSha256, "macOS inspection mounted tree");
  validateMacosDetach(inspection.dmg.detach, "macOS inspection DMG detach");
  validateMacosRootInventory(inspection.dmg.rootInventory, "macOS inspection DMG root inventory");
  exactKeys(inspection.updaterSignature, ["receiptSha256", "schema", "updaterPublicKeySha256", "verified"], "macOS inspection updater signature");
  assert.equal(inspection.updaterSignature.receiptSha256, sha256(signatureFile.raw));
  assert.equal(inspection.updaterSignature.schema, MACOS_UPDATER_SIGNATURE_SCHEMA);
  assert.equal(inspection.updaterSignature.verified, true);
  expectedHash(inspection.updaterSignature.updaterPublicKeySha256, "macOS updater public key");

  const signature = signatureFile.value;
  exactKeys(signature, [
    "assets", "bindingSha256", "completedAt", "disposableVm", "guest",
    "guestReceiptSha256", "limactl", "release", "repository", "schema",
    "source", "sourceCommit", "updaterPublicKeySha256", "verified",
  ], "macOS updater-signature receipt");
  assert.equal(signature.schema, MACOS_UPDATER_SIGNATURE_SCHEMA);
  assert.equal(signature.repository, "FerrumVir/arc-chain");
  assert.equal(signature.sourceCommit, sourceCommit);
  assert.equal(signature.bindingSha256, provenance.bindingSha256);
  assert.equal(signature.updaterPublicKeySha256, inspection.updaterSignature.updaterPublicKeySha256);
  assert.equal(signature.verified, true);
  assert.deepEqual(signature.source, provenance.source);
  validateMacosRelease(signature.release, input, "macOS updater-signature release");
  exactKeys(signature.assets, ["appArchive", "appArchiveSignature"], "macOS updater-signature assets");
  assert.deepEqual(signature.assets, {
    appArchive: input.assets.appArchive,
    appArchiveSignature: input.assets.appArchiveSignature,
  });
  exactKeys(signature.disposableVm, [
    "configSha256", "deletedAfterEvidenceRead", "imageDigest", "imageUrl",
    "mounts", "name", "preexistingInstancesUnchanged", "recoveryEnclaveAccessed",
  ], "macOS updater-signature disposable VM");
  assert.equal(signature.disposableVm.deletedAfterEvidenceRead, true);
  assert.equal(signature.disposableVm.preexistingInstancesUnchanged, true);
  assert.equal(signature.disposableVm.recoveryEnclaveAccessed, false);
  assert.deepEqual(signature.disposableVm.mounts, []);
  assert.equal(signature.disposableVm.configSha256, MACOS_UPDATER_LIMA_CONFIG_SHA256);
  assert.equal(signature.disposableVm.imageDigest, "sha256:5c3ddb00f60bc455dac0862fabe9d8bacec46c33ac1751143c5c3683404b110d");
  assert.equal(signature.disposableVm.imageUrl, "https://cloud-images.ubuntu.com/releases/noble/release-20260321/ubuntu-24.04-server-cloudimg-amd64.img");
  const guest = signature.guest;
  exactKeys(guest, [
    "apt", "archiveSha256", "completedAt", "helperSha256", "minisign",
    "publicKeySha256", "schema", "signatureSha256", "verification",
  ], "macOS updater-signature guest");
  assert.equal(guest.schema, "arc.macos-updater-signature-guest.v1");
  assert.equal(guest.archiveSha256, input.assets.appArchive.sha256);
  assert.equal(guest.signatureSha256, input.assets.appArchiveSignature.sha256);
  assert.equal(guest.publicKeySha256, signature.updaterPublicKeySha256);
  assert.equal(guest.helperSha256, controllerSha256);
  exactKeys(guest.minisign, ["binary", "package"], "macOS updater minisign");
  exactKeys(guest.minisign.binary, ["path", "resolvedPath", "sha256", "size"], "macOS updater minisign binary");
  assert.equal(guest.minisign.binary.path, "/usr/bin/minisign");
  assert.equal(guest.minisign.binary.resolvedPath, "/usr/bin/minisign");
  expectedHash(guest.minisign.binary.sha256, "macOS updater minisign binary");
  assert.ok(Number.isSafeInteger(guest.minisign.binary.size) && guest.minisign.binary.size > 0);
  exactKeys(guest.minisign.package, ["architecture", "version"], "macOS updater minisign package");
  assert.equal(guest.minisign.package.architecture, "amd64");
  assert.ok(typeof guest.minisign.package.version === "string" && guest.minisign.package.version.trim());
  exactKeys(guest.apt, [
    "installStderrSha256", "installStdoutSha256", "snapshot", "sourceSha256",
    "updateStderrSha256", "updateStdoutSha256",
  ], "macOS updater APT evidence");
  assert.equal(guest.apt.snapshot, "20260321T235959Z");
  assert.equal(guest.apt.sourceSha256, MACOS_UPDATER_APT_SOURCE_SHA256);
  for (const field of ["installStderrSha256", "installStdoutSha256", "updateStderrSha256", "updateStdoutSha256"]) {
    expectedHash(guest.apt[field], `macOS updater APT ${field}`);
  }
  exactKeys(guest.verification, ["stderrSha256", "stdoutSha256", "verified"], "macOS updater minisign result");
  assert.equal(guest.verification.verified, true);
  assert.equal(signature.guestReceiptSha256, sha256(Buffer.from(canonicalJson(guest))));
  exactKeys(signature.limactl, ["path", "resolvedPath", "sha256", "size"], "macOS updater limactl");
  expectedHash(signature.limactl.sha256, "macOS updater limactl hash");
  assert.ok(Number.isSafeInteger(signature.limactl.size) && signature.limactl.size > 0);

  const verification = verificationFile.value;
  exactKeys(verification, [
    "assets", "bindingSha256", "bundle", "completedAt", "controllerAttemptSha256",
    "inspectionSha256", "nativeDispatchAttemptSha256", "nativeInputSha256",
    "nativeReceiptSha256", "provenanceSha256", "release", "repository",
    "schema", "source", "sourceCommit", "updaterSignatureReceiptSha256",
    "verified",
  ], "macOS package provenance verification");
  assert.equal(verification.schema, MACOS_PACKAGE_PROVENANCE_VERIFICATION_SCHEMA);
  assert.equal(verification.repository, "FerrumVir/arc-chain");
  assert.equal(verification.sourceCommit, sourceCommit);
  assert.equal(verification.verified, true);
  assert.deepEqual(verification.source, provenance.source);
  validateMacosAssetMap(verification.assets, input.assets, "macOS verification assets");
  validateMacosBundle(verification.bundle, input.expectedBundle, "macOS verification bundle");
  validateMacosRelease(verification.release, input, "macOS verification release");
  for (const [field, expected] of [
    ["bindingSha256", provenance.bindingSha256],
    ["controllerAttemptSha256", sha256(controllerAttemptFile.raw)],
    ["inspectionSha256", sha256(inspectionFile.raw)],
    ["nativeDispatchAttemptSha256", sha256(attemptFile.raw)],
    ["nativeInputSha256", sha256(inputFile.raw)],
    ["nativeReceiptSha256", sha256(nativeFile.raw)],
    ["provenanceSha256", sha256(provenanceFile.raw)],
    ["updaterSignatureReceiptSha256", sha256(signatureFile.raw)],
  ]) assert.equal(verification[field], expected, `macOS verification ${field} differs`);
  assert.equal(native.inputSha256, verification.nativeInputSha256);

  return {
    controllerPath: MACOS_PACKAGE_CONTROLLER_RELATIVE,
    controllerSha256,
    inspectionReceipt: inspection,
    inspectionReceiptSha256: sha256(inspectionFile.raw),
    provenanceReceipt: provenance,
    provenanceReceiptSha256: sha256(provenanceFile.raw),
    signatureReceipt: signature,
    signatureReceiptSha256: sha256(signatureFile.raw),
    verificationReceipt: verification,
    verificationReceiptSha256: sha256(verificationFile.raw),
  };
}

export async function validatePackagedNative(
  options,
  sourceCommit,
  frontendConfigSha256,
  repositoryRoot,
) {
  for (const [path, label] of [
    [options.nativeInput, "packaged native input"],
    [options.nativeReceipt, "packaged native receipt"],
    [options.nativeAttempt, "packaged native dispatch attempt"],
    [options.appArchive, "macOS app archive"],
    [options.appArchiveSignature, "macOS app archive signature"],
    [options.dmg, "macOS DMG"],
    [options.nativeHome, "packaged native isolated HOME"],
  ]) assert.ok(typeof path === "string" && isAbsolute(path), `${label} path must be absolute`);

  const [inputFile, nativeFile, attemptFile, appArchiveRaw, signatureRaw, dmgRaw] = await Promise.all([
    privateJsonFile(options.nativeInput, "packaged native input"),
    privateJsonFile(options.nativeReceipt, "packaged native receipt"),
    privateJsonFile(options.nativeAttempt, "packaged native dispatch attempt"),
    regularBytes(options.appArchive, "macOS app archive", 2 * 1024 * 1024 * 1024),
    regularBytes(options.appArchiveSignature, "macOS app archive signature", 1024 * 1024),
    regularBytes(options.dmg, "macOS DMG", 2 * 1024 * 1024 * 1024),
  ]);
  for (const [file, label] of [[inputFile, "packaged native input"], [nativeFile, "packaged native receipt"], [attemptFile, "packaged native dispatch attempt"]]) {
    assert.equal(file.raw.toString("utf8"), canonicalJson(file.value), `${label} is not canonical JSON`);
  }
  const input = inputFile.value;
  exactKeys(input, [
    "assets", "budgets", "challenge", "expectedActiveValidators",
    "expectedBundle", "expectedCoordinator", "expectedModelId",
    "expectedRegisteredValidators",
    "expiresAtUnix", "frontendCommit",
    "frontendConfigSha256", "issuedAtUnix", "maximumBlockAgeSeconds",
    "minimumPeers", "pagesOrigin", "recoveryEpoch", "releaseId",
    "releaseRunAttempt", "releaseRunId", "releaseVersion", "repository",
    "rolloutManifestSha256", "schema", "sourceCommit", "transactionDomain",
    "validatorApprovalsRequired", "validatorSetCommitment", "validatorSetId",
  ], "packaged native input");
  assert.equal(input.schema, PACKAGED_NATIVE_INPUT_SCHEMA, "packaged native input schema differs");
  assert.equal(input.repository, "FerrumVir/arc-chain", "packaged native input repository differs");
  assert.equal(input.sourceCommit, sourceCommit, "packaged native input source differs");
  assert.equal(input.frontendConfigSha256, frontendConfigSha256, "packaged native input frontend config differs");
  assert.equal(input.expectedCoordinator, "https://140.82.16.112", "packaged native input selected another coordinator");
  canonicalHash(input.expectedModelId, "packaged native expected model");
  assert.equal(input.releaseVersion, "0.8.0", "packaged native release version differs");
  assert.ok(Number.isSafeInteger(input.releaseId) && input.releaseId > 0, "packaged native release ID is invalid");
  assert.ok(Number.isSafeInteger(input.releaseRunId) && input.releaseRunId > 0, "packaged native release run ID is invalid");
  assert.ok(Number.isSafeInteger(input.releaseRunAttempt) && input.releaseRunAttempt > 0, "packaged native release attempt is invalid");
  canonicalHash(input.transactionDomain, "packaged native transaction domain");
  canonicalHash(input.validatorSetCommitment, "packaged native validator set commitment");
  expectedHash(input.challenge, "packaged native challenge");
  expectedHash(input.rolloutManifestSha256, "packaged native rollout manifest");
  assert.deepEqual(input.budgets, {
    dispatchMaxWaitMs: 3_960_000,
    earningsMaxPolls: 11,
    earningsMaxWaitMs: 30_000,
    finalReadMaxPolls: 11,
    finalReadMaxWaitMs: 30_000,
    preflightMaxWaitMs: 30_000,
    receiptMaxPolls: 61,
    receiptMaxWaitMs: 180_000,
    totalMaxWaitMs: 4_300_000,
  }, "packaged native budgets differ from the shipped executable");
  assert.equal(input.expectedActiveValidators, 6);
  assert.equal(input.expectedRegisteredValidators, 6);
  assert.ok(input.minimumPeers >= 5);
  assert.ok(input.maximumBlockAgeSeconds > 0 && input.maximumBlockAgeSeconds <= 300);
  for (const field of ["issuedAtUnix", "expiresAtUnix"]) {
    assert.ok(Number.isSafeInteger(input[field]), `packaged native ${field} is invalid`);
  }
  assert.ok(input.expiresAtUnix > input.issuedAtUnix);
  assert.ok(input.expiresAtUnix - input.issuedAtUnix <= 21_600);

  exactKeys(input.assets, ["appArchive", "appArchiveSignature", "dmg"], "packaged native assets");
  const assetRows = [
    [input.assets.appArchive, "arc-desktop-macos-arm64.app.tar.gz", appArchiveRaw],
    [input.assets.appArchiveSignature, "arc-desktop-macos-arm64.app.tar.gz.sig", signatureRaw],
    [input.assets.dmg, "arc-desktop-macos-arm64.dmg", dmgRaw],
  ];
  for (const [binding, expectedName, raw] of assetRows) {
    exactKeys(binding, ["id", "name", "sha256", "size"], `packaged native asset ${expectedName}`);
    assert.ok(Number.isSafeInteger(binding.id) && binding.id > 0, `${expectedName} asset ID is invalid`);
    assert.equal(binding.name, expectedName, `${expectedName} asset name differs`);
    assert.equal(binding.size, raw.length, `${expectedName} asset size differs`);
    assert.equal(binding.sha256, sha256(raw), `${expectedName} asset hash differs`);
  }

  validateMacosBundle(input.expectedBundle, input.expectedBundle, "expected app bundle");
  const packageEvidence = await validateMacosPackageEvidence(
    options,
    inputFile,
    nativeFile,
    attemptFile,
    sourceCommit,
    repositoryRoot,
  );

  const attempt = attemptFile.value;
  exactKeys(attempt, ["armedAt", "challenge", "dispatchLimit", "executableSha256", "inputSha256", "schema", "sourceCommit", "sourceHost"], "packaged native attempt");
  assert.equal(attempt.schema, PACKAGED_NATIVE_ATTEMPT_SCHEMA);
  assert.equal(attempt.challenge, input.challenge);
  assert.equal(attempt.dispatchLimit, 1);
  assert.equal(attempt.executableSha256, input.expectedBundle.executableSha256);
  assert.equal(attempt.inputSha256, sha256(inputFile.raw));
  assert.equal(attempt.sourceCommit, sourceCommit);
  assert.equal(attempt.sourceHost, input.expectedCoordinator);

  const receipt = nativeFile.value;
  exactKeys(receipt, [
    "assets", "budgets", "bundle", "challenge", "completedAt", "dispatch",
    "dispatchAttemptSha256", "earnings", "expectedCoordinator", "expectedModelId",
    "expiresAtUnix", "finalReadAttempts", "finalReadElapsedMs", "frontendCommit",
    "frontendConfigSha256", "immutableSessionOrigin", "inputSha256",
    "issuedAtUnix", "negativeRouteChecks", "network", "pagesOrigin",
    "preflightModelId", "preflightNetwork", "projection", "receiptPoll", "recentBlocks",
    "releaseId", "releaseRunAttempt", "releaseRunId", "releaseVersion",
    "repository", "rolloutManifestSha256", "runtime", "schema",
    "sourceCommit", "startedAt", "transaction", "worker",
  ], "packaged native receipt");
  assert.equal(receipt.schema, PACKAGED_NATIVE_SCHEMA, "packaged native receipt schema differs");
  for (const field of ["repository", "sourceCommit", "releaseVersion", "releaseId", "releaseRunId", "releaseRunAttempt", "frontendCommit", "pagesOrigin", "frontendConfigSha256", "rolloutManifestSha256", "expectedCoordinator", "expectedModelId", "challenge", "issuedAtUnix", "expiresAtUnix", "assets", "budgets"]) {
    assert.deepEqual(receipt[field], input[field], `packaged native receipt ${field} differs from input`);
  }
  assert.deepEqual(receipt.bundle, input.expectedBundle, "running bundle differs from the sealed package identity");
  assert.equal(receipt.inputSha256, sha256(inputFile.raw));
  assert.equal(receipt.dispatchAttemptSha256, sha256(attemptFile.raw));
  assert.equal(receipt.immutableSessionOrigin, true);
  assert.ok(Number.isSafeInteger(receipt.finalReadAttempts) && receipt.finalReadAttempts >= 1 && receipt.finalReadAttempts <= input.budgets.finalReadMaxPolls);
  assert.ok(Number.isSafeInteger(receipt.finalReadElapsedMs) && receipt.finalReadElapsedMs <= input.budgets.finalReadMaxWaitMs);
  assert.equal(receipt.preflightModelId, input.expectedModelId, "native model preflight differs from the sealed model");
  const startedAtMs = Date.parse(receipt.startedAt);
  const completedAtMs = Date.parse(receipt.completedAt);
  assert.ok(Number.isFinite(startedAtMs) && Number.isFinite(completedAtMs) && completedAtMs >= startedAtMs);
  assert.ok(input.expiresAtUnix - Math.floor(startedAtMs / 1000) >= 4_360, "native attempt was armed without its complete runtime window");
  assert.ok(Math.floor(completedAtMs / 1000) <= input.expiresAtUnix, "native receipt completed after plan expiry");
  const nativeWorker = canonicalHash(receipt.worker, "packaged native selected worker");
  exactKeys(receipt.runtime, [
    "appDataRelativePath", "appVersion", "architecture", "buildSourceCommit",
    "environmentNames", "environmentSha256", "ipcHandlersRegistered",
    "isolatedHomeBasename", "operatingSystem", "pluginsLoaded",
    "tauriBuilderStarted", "webviewsCreated",
  ], "packaged native runtime");
  assert.equal(receipt.runtime.appDataRelativePath, "Library/Application Support/network.arc.desktop");
  assert.equal(receipt.runtime.appVersion, "0.8.0");
  assert.equal(receipt.runtime.architecture, "aarch64");
  assert.equal(receipt.runtime.operatingSystem, "macos");
  assert.equal(receipt.runtime.buildSourceCommit, sourceCommit);
  assert.equal(receipt.runtime.isolatedHomeBasename, "isolated-home");
  assert.deepEqual(
    receipt.runtime.environmentNames,
    ["HOME", "LANG", "LC_ALL", "PATH", "TMPDIR"],
    "packaged native runtime did not use the exact env -i allowlist",
  );
  const reviewedEnvironment = [
    `HOME=${options.nativeHome}`,
    "LANG=C",
    "LC_ALL=C",
    "PATH=/usr/bin:/bin:/usr/sbin:/sbin",
    `TMPDIR=${resolve(options.nativeHome, "tmp")}`,
  ].sort();
  const reviewedEnvironmentRaw = Buffer.from(`${reviewedEnvironment.join("\0")}\0`);
  assert.equal(
    receipt.runtime.environmentSha256,
    sha256(reviewedEnvironmentRaw),
    "packaged native runtime environment differs from the exact sanitized launch",
  );
  assert.equal(receipt.runtime.ipcHandlersRegistered, false);
  assert.equal(receipt.runtime.pluginsLoaded, false);
  assert.equal(receipt.runtime.tauriBuilderStarted, false);
  assert.equal(receipt.runtime.webviewsCreated, 0);

  validateNativeNetwork(receipt.preflightNetwork, input.expectedCoordinator, 1, input.maximumBlockAgeSeconds);
  exactKeys(receipt.dispatch, ["chatTemplate", "count", "elapsedMs", "maxTokens", "maxWaitMs", "promptSha256", "result", "sourceHost"], "packaged native dispatch");
  assert.equal(receipt.dispatch.chatTemplate, true);
  assert.equal(receipt.dispatch.count, 1);
  assert.equal(receipt.dispatch.maxTokens, 2);
  assert.equal(receipt.dispatch.maxWaitMs, input.budgets.dispatchMaxWaitMs);
  assert.ok(Number.isSafeInteger(receipt.dispatch.elapsedMs) && receipt.dispatch.elapsedMs <= receipt.dispatch.maxWaitMs);
  assert.equal(receipt.dispatch.sourceHost, input.expectedCoordinator);
  const inputSha256 = sha256(inputFile.raw);
  const prompt = `ARC packaged v0.8.0 production acceptance challenge ${input.challenge} input ${inputSha256}`;
  assert.equal(receipt.dispatch.promptSha256, sha256(Buffer.from(prompt)), "native dispatch prompt differs from challenge");
  const promptBlake3 = `0x${blake3Short(Buffer.from(prompt, "utf8"))}`;
  const result = receipt.dispatch.result;
  exactKeys(result, [
    "coordinator", "deterministic", "engine", "executionProfile",
    "explorerUrl", "inferenceMs", "input", "modelHash", "output",
    "outputHash", "profileBound", "quorumVerified", "routedVia",
    "servedLocally", "settlement", "tokensGenerated", "txHash",
  ], "packaged native inference result");
  assert.equal(result.input, prompt, "native inference input differs from challenge");
  assert.ok(typeof result.output === "string" && result.output.trim(), "native inference output is empty");
  assert.equal(result.deterministic, true);
  assert.equal(result.profileBound, true);
  assert.equal(result.quorumVerified, true);
  assert.equal(result.executionProfile, CANONICAL_REWARD_INFERENCE_PROFILE);
  assert.equal(result.servedLocally, false);
  assert.equal(result.coordinator, input.expectedCoordinator);
  assert.equal(result.routedVia, `community:${nativeWorker}`);
  canonicalHash(result.outputHash, "native inference output hash");
  assert.equal(
    result.outputHash,
    `0x${blake3Short(Buffer.from(result.output, "utf8"))}`,
    "native inference output hash is not BLAKE3-bound to exact output bytes",
  );
  assert.equal(canonicalHash(result.modelHash, "native inference model hash"), input.expectedModelId);

  assert.deepEqual(receipt.negativeRouteChecks, { job: true, origin: true, receiptUrl: true, transaction: true, worker: true });
  exactKeys(receipt.receiptPoll, ["attempts", "elapsedMs", "maxPolls", "maxWaitMs", "receipt"], "packaged native receipt poll");
  assert.ok(Number.isSafeInteger(receipt.receiptPoll.attempts) && receipt.receiptPoll.attempts >= 1 && receipt.receiptPoll.attempts <= 61);
  assert.ok(Number.isSafeInteger(receipt.receiptPoll.elapsedMs) && receipt.receiptPoll.elapsedMs <= 180_000);
  assert.equal(receipt.receiptPoll.maxPolls, 61);
  assert.equal(receipt.receiptPoll.maxWaitMs, 180_000);
  const terminal = receipt.receiptPoll.receipt;
  const settlementKeys = [
    "assignmentEpoch", "blockHash", "blockHeight", "confirmed",
    "evidenceSource", "included", "index", "inputHash", "jobId",
    "modelId", "outputHash", "receiptUrl", "recoveryEpoch", "rewardArc",
    "rewardBase", "status", "submitted", "success", "transactionDomain",
    "txHash", "txType", "validatorApprovals", "validatorSetCommitment",
    "validatorSetId", "worker",
  ];
  exactKeys(result.settlement, settlementKeys, "packaged native provisional settlement");
  exactKeys(terminal, settlementKeys, "packaged native terminal settlement");
  assert.equal(terminal.status, "mined_success");
  assert.equal(terminal.txType, "0x25");
  assert.equal(terminal.worker, nativeWorker);
  assert.equal(terminal.submitted, true);
  assert.equal(terminal.included, true);
  assert.equal(terminal.confirmed, true);
  assert.equal(terminal.success, true);
  assert.equal(terminal.rewardBase, REWARD_BASE);
  assert.equal(terminal.rewardArc, REWARD_ARC);
  assert.ok(terminal.validatorApprovals >= input.validatorApprovalsRequired);
  assert.equal(terminal.validatorSetCommitment, input.validatorSetCommitment);
  assert.equal(terminal.transactionDomain, input.transactionDomain);
  assert.equal(terminal.recoveryEpoch, input.recoveryEpoch);
  assert.equal(terminal.validatorSetId, input.validatorSetId);
  assert.equal(terminal.receiptUrl, `/community/reward_receipt/${terminal.txHash}`);
  for (const field of ["txHash", "jobId", "worker", "modelId", "inputHash", "outputHash", "assignmentEpoch", "transactionDomain", "validatorSetCommitment", "blockHash"]) canonicalHash(terminal[field], `packaged native terminal ${field}`);
  const provisional = result.settlement;
  assert.ok(
    provisional.status === "pending_mined_receipt" || provisional.status === "mined_success",
    "packaged native provisional settlement has an unsupported state",
  );
  assert.equal(provisional.txType, "0x25");
  assert.equal(provisional.submitted, true);
  assert.equal(provisional.txHash, terminal.txHash);
  assert.equal(provisional.jobId, terminal.jobId);
  assert.equal(provisional.worker, terminal.worker);
  assert.equal(provisional.receiptUrl, terminal.receiptUrl);
  if (provisional.status === "pending_mined_receipt") {
    assert.equal(provisional.included, false);
    assert.equal(provisional.confirmed, false);
    assert.equal(provisional.success, null);
    assert.equal(provisional.blockHeight, null);
    assert.equal(provisional.blockHash, null);
    assert.equal(provisional.index, null);
    assert.equal(provisional.rewardBase, null);
    assert.equal(provisional.rewardArc, null);
    for (const field of ["modelId", "inputHash", "outputHash"]) {
      assert.equal(provisional[field], "", `pending provisional ${field} must be absent`);
    }
    canonicalHash(provisional.assignmentEpoch, "pending provisional assignment epoch");
    assert.equal(provisional.assignmentEpoch, terminal.assignmentEpoch);
    assert.equal(provisional.transactionDomain, input.transactionDomain);
    assert.equal(provisional.recoveryEpoch, input.recoveryEpoch);
    assert.equal(provisional.validatorSetId, input.validatorSetId);
    assert.equal(provisional.validatorSetCommitment, input.validatorSetCommitment);
    assert.ok(
      Number.isSafeInteger(provisional.validatorApprovals)
        && provisional.validatorApprovals >= 0
        && provisional.validatorApprovals <= input.validatorApprovalsRequired,
      "pending provisional validator approval count differs from protocol",
    );
    assert.equal(
      provisional.evidenceSource,
      "coordinator mempool submission only; no mined receipt",
    );
  } else {
    assert.deepEqual(provisional, terminal, "immediately mined provisional receipt differs from terminal receipt");
  }
  assert.equal(terminal.modelId, result.modelHash);
  assert.equal(terminal.modelId, input.expectedModelId);
  assert.equal(terminal.inputHash, promptBlake3, "native mined receipt is not BLAKE3-bound to the exact sealed prompt");
  assert.equal(terminal.outputHash, result.outputHash);

  exactKeys(receipt.earnings, ["attempts", "elapsedMs", "maxPolls", "maxWaitMs", "value"], "packaged native earnings poll");
  assert.ok(receipt.earnings.attempts >= 1 && receipt.earnings.attempts <= 11);
  assert.ok(receipt.earnings.elapsedMs <= 30_000);
  assert.equal(receipt.earnings.maxPolls, 11);
  assert.equal(receipt.earnings.maxWaitMs, 30_000);
  const earningsValue = receipt.earnings.value;
  exactKeys(earningsValue, [
    "archiveMode", "attestations", "confirmedReceipts", "fromChain",
    "historyCompleteSinceRecovery", "historyScope", "lastPayoutAt",
    "lastPayoutBlock", "pendingArc", "projectedDailyArc",
    "projectedDailyUnavailableReason", "rank", "receiptSource",
    "recoveryEpoch", "todayArc", "totalArc", "unavailableReason",
    "validatorSetId",
  ], "packaged native earnings value");
  assert.equal(earningsValue.fromChain, true);
  assert.equal(earningsValue.unavailableReason, null);
  assert.equal(earningsValue.recoveryEpoch, input.recoveryEpoch);
  assert.equal(earningsValue.validatorSetId, input.validatorSetId);
  const canaries = earningsValue.confirmedReceipts.filter((row) => row.txHash === terminal.txHash);
  assert.equal(canaries.length, 1, "native earnings lacks the exact fresh reward once");
  exactKeys(canaries[0], ["blockHash", "blockHeight", "jobId", "receiptUrl", "recoveryEpoch", "rewardArc", "rewardBase", "txHash", "validatorSetId"], "packaged native earnings receipt");
  assert.equal(canaries[0].jobId, terminal.jobId);
  assert.equal(canaries[0].blockHeight, terminal.blockHeight);
  assert.equal(canaries[0].blockHash, terminal.blockHash);
  assert.equal(canaries[0].rewardBase, REWARD_BASE);
  assert.equal(canaries[0].rewardArc, REWARD_ARC);

  const projection = receipt.projection;
  exactKeys(projection, [
    "attestationsPerDay", "attestationsTotal", "communityRewardsEnabled",
    "coordinatorRewardsRemainingThisEpoch", "firstAttestationBlock",
    "issuanceReadyForWorker", "observedOverBlocks", "projectedDailyArc",
    "projectedDailyUnavailableReason", "rateCaveat", "rateUnavailableReason",
    "rewardBudgetEpoch", "rewardIsCustomerDemand", "rewardPerAttestation",
    "rewardPolicyHash", "rewardProgram", "rewardRateSource",
    "rewardsRemainingThisEpoch", "sourceHost", "unavailable",
    "workerRewardsRemainingThisEpoch",
  ], "packaged native projection");
  assert.equal(projection.sourceHost, input.expectedCoordinator);
  assert.equal(projection.unavailable, null);
  assert.equal(projection.rewardPerAttestation, REWARD_ARC);
  assert.equal(projection.communityRewardsEnabled, true);
  assert.equal(projection.issuanceReadyForWorker, true);
  const projectionValue = typeof projection.projectedDailyArc === "number" && Number.isFinite(projection.projectedDailyArc) && projection.projectedDailyArc >= 0 && projection.projectedDailyUnavailableReason === null;
  const projectionReason = projection.projectedDailyArc === null && typeof projection.projectedDailyUnavailableReason === "string" && projection.projectedDailyUnavailableReason.trim();
  assert.ok(Boolean(projectionValue) !== Boolean(projectionReason), "native projection lacks an exact value/reason XOR");
  assert.ok(projection.attestationsTotal >= earningsValue.attestations);
  assert.equal(projection.rewardRateSource, "chain");
  assert.equal(projection.rewardProgram, "protocol-capped testnet promotional compute subsidy");
  assert.equal(projection.rewardIsCustomerDemand, false);
  canonicalHash(projection.rewardPolicyHash, "packaged native reward policy");

  validateNativeNetwork(receipt.network, input.expectedCoordinator, terminal.blockHeight, input.maximumBlockAgeSeconds);
  assert.equal(receipt.recentBlocks.sourceHost, input.expectedCoordinator);
  exactKeys(receipt.recentBlocks, ["blocks", "sourceHost", "unavailable"], "packaged native recent blocks");
  assert.equal(receipt.recentBlocks.unavailable, null);
  assert.ok(Array.isArray(receipt.recentBlocks.blocks) && receipt.recentBlocks.blocks.length > 0);
  for (const block of receipt.recentBlocks.blocks) {
    exactKeys(block, ["hash", "height", "proposer", "timestampMs", "txCount"], "packaged native recent block");
    assert.ok(typeof block.hash === "string" && HASH_RE.test(block.hash), "packaged native recent block hash is malformed");
    assert.ok(Number.isSafeInteger(block.height) && block.height > 0);
  }
  assert.ok(
    receipt.recentBlocks.blocks.some(
      (block) => block.height === terminal.blockHeight && `0x${block.hash}` === terminal.blockHash,
    ),
    "packaged native recent blocks omit the exact reward block",
  );
  const transaction = receipt.transaction;
  exactKeys(transaction, ["blockHash", "blockHeight", "gasUsed", "hash", "sourceHost", "status", "success", "txIndex", "unavailable"], "packaged native transaction lookup");
  assert.equal(transaction.sourceHost, input.expectedCoordinator);
  assert.equal(transaction.unavailable, null);
  assert.equal(transaction.status, "mined");
  assert.equal(`0x${transaction.hash}`, terminal.txHash);
  assert.equal(transaction.blockHeight, terminal.blockHeight);
  assert.equal(`0x${transaction.blockHash}`, terminal.blockHash);
  assert.equal(transaction.txIndex, terminal.index);
  assert.equal(transaction.success, true);

  return {
    appArchiveSha256: sha256(appArchiveRaw),
    appArchiveSignatureSha256: sha256(signatureRaw),
    appBundleTreeSha256: input.expectedBundle.appBundleTreeSha256,
    challenge: input.challenge,
    dispatchAttemptSha256: sha256(attemptFile.raw),
    dmgSha256: sha256(dmgRaw),
    executableSha256: input.expectedBundle.executableSha256,
    inputHashVerification: {
      algorithm: "BLAKE3-256",
      implementation: "arc-reviewed-js-blake3-one-chunk-v1",
      implementationSourceSha256: BLAKE3_VERIFIER_SOURCE_SHA256,
      maximumInputBytes: 1_024,
      promptHash: promptBlake3,
    },
    inputSha256,
    packageEvidence,
    receipt,
    receiptSha256: sha256(nativeFile.raw),
    scope: "macos-arm64-packaged-native-core",
  };
}

async function validateRuntime(options) {
  const requiredRuntime = options.runtimeContract ?? {
    nodeArch: "arm64",
    nodePlatform: "darwin",
    nodeVersion: "v24.20.0",
    npmVersion: "11.19.0",
  };
  for (const [path, label] of [
    [options.nodeArchive, "Node distribution archive"],
    [options.nodeExecutable, "Node executable"],
    [options.npmCli, "npm CLI"],
    [options.npmPackage, "npm package metadata"],
  ]) {
    assert.ok(typeof path === "string" && isAbsolute(path), `${label} path must be absolute`);
  }
  const processNode = await realpath(process.execPath);
  const selectedNode = await realpath(options.nodeExecutable);
  assert.equal(selectedNode, processNode, "receipt helper is not running under the selected Node executable");
  const [archiveRaw, nodeRaw, npmCliRaw, npmPackageFile] = await Promise.all([
    regularBytes(options.nodeArchive, "Node distribution archive", 128 * 1024 * 1024),
    regularBytes(options.nodeExecutable, "Node executable", 128 * 1024 * 1024),
    regularBytes(options.npmCli, "npm CLI", 4 * 1024 * 1024),
    jsonFile(options.npmPackage, "npm package metadata"),
  ]);
  const identities = {
    nodeDistributionArchiveSha256: sha256(archiveRaw),
    nodeExecutableSha256: sha256(nodeRaw),
    npmCliSha256: sha256(npmCliRaw),
    npmPackageSha256: sha256(npmPackageFile.raw),
  };
  for (const [field, expectedField, label] of [
    ["nodeDistributionArchiveSha256", "expectedNodeArchiveSha256", "Node distribution archive"],
    ["nodeExecutableSha256", "expectedNodeExecutableSha256", "Node executable"],
    ["npmCliSha256", "expectedNpmCliSha256", "npm CLI"],
    ["npmPackageSha256", "expectedNpmPackageSha256", "npm package metadata"],
  ]) {
    assert.equal(identities[field], expectedHash(options[expectedField], `${label} expected hash`), `${label} differs from its reviewed hash`);
  }
  assert.equal(process.version, requiredRuntime.nodeVersion, `desktop live gate requires Node ${requiredRuntime.nodeVersion}`);
  assert.equal(process.platform, requiredRuntime.nodePlatform, `desktop live gate must run on ${requiredRuntime.nodePlatform}`);
  assert.equal(process.arch, requiredRuntime.nodeArch, `desktop live gate requires ${requiredRuntime.nodeArch}`);
  assert.equal(npmPackageFile.value?.version, requiredRuntime.npmVersion, `desktop live gate requires npm ${requiredRuntime.npmVersion}`);
  return {
    ...identities,
    nodeArch: process.arch,
    nodePlatform: process.platform,
    nodeVersion: process.version,
    npmVersion: npmPackageFile.value.version,
  };
}

async function validateForward(options, port) {
  for (const [path, label] of [
    [options.sshExecutable, "SSH executable"],
    [options.sshKnownHosts, "SSH known-hosts"],
  ]) {
    assert.ok(typeof path === "string" && isAbsolute(path), `${label} path must be absolute`);
  }
  const rolloutManifestSha256 = expectedHash(
    options.rolloutManifestSha256,
    "rollout manifest hash",
  );
  assert.equal(options.validatorName, LIVE_VALIDATOR_NAME, "desktop live gate selected another validator");
  assert.equal(options.validatorHost, LIVE_VALIDATOR_HOST, "desktop live gate selected another validator host");
  const expectedSocket = `/run/arc-v3-rpc-${LIVE_VALIDATOR_NAME}-${rolloutManifestSha256.slice(0, 16)}/rpc.sock`;
  assert.equal(options.validatorRpcSocket, expectedSocket, "validator Unix RPC socket is not bound to the rollout manifest");
  const [sshRaw, knownHostsRaw] = await Promise.all([
    regularBytes(options.sshExecutable, "SSH executable", 16 * 1024 * 1024),
    regularBytes(options.sshKnownHosts, "SSH known-hosts", 1024 * 1024),
  ]);
  const sshExecutableSha256 = sha256(sshRaw);
  const sshKnownHostsSha256 = sha256(knownHostsRaw);
  assert.equal(
    sshExecutableSha256,
    expectedHash(options.expectedSshExecutableSha256, "SSH executable expected hash"),
    "SSH executable differs from its reviewed hash",
  );
  assert.equal(
    sshKnownHostsSha256,
    expectedHash(options.expectedSshKnownHostsSha256, "SSH known-hosts expected hash"),
    "SSH known-hosts differs from its reviewed hash",
  );
  const sshIdentitySha256 = expectedHash(
    options.sshIdentitySha256,
    "SSH identity hash",
  );
  return {
    kind: FORWARD_KIND,
    localPort: port,
    rolloutManifestSha256,
    sshExecutableSha256,
    sshIdentitySha256,
    sshKnownHostsSha256,
    validatorHost: LIVE_VALIDATOR_HOST,
    validatorName: LIVE_VALIDATOR_NAME,
    validatorRpcSocket: expectedSocket,
  };
}

async function fetchJson(fetchImpl, url, label) {
  const response = await fetchImpl(url, {
    cache: "no-store",
    headers: { accept: "application/json" },
    redirect: "error",
    signal: AbortSignal.timeout(15_000),
  });
  assert.equal(response.ok, true, `${label} returned HTTP ${response.status}`);
  const text = await response.text();
  assert.ok(Buffer.byteLength(text) > 0 && Buffer.byteLength(text) <= MAX_JSON_BYTES, `${label} has an unsupported size`);
  return JSON.parse(text);
}

export async function buildDesktopLiveProductReceipt(options) {
  const root = resolve(options.repositoryRoot ?? dirname(dirname(dirname(fileURLToPath(import.meta.url)))));
  const reportPath = resolve(options.playwrightReport);
  const configPath = resolve(options.frontendConfig);
  const outputPath = resolve(options.output);
  assert.ok(isAbsolute(options.playwrightReport) && isAbsolute(options.frontendConfig) && isAbsolute(options.output), "report, config, and output paths must be absolute");
  assert.ok(COMMIT_RE.test(options.sourceCommit), "source commit must be a full lowercase Git commit");
  const port = Number(options.port);
  assert.ok(Number.isSafeInteger(port) && port >= 1 && port <= 65_535, "live port must be between 1 and 65535");
  const worker = canonicalHash(options.worker, "live worker");
  const rewardTx = canonicalHash(options.rewardTx, "live reward transaction");

  const [{ raw: reportRaw, value: report }, { raw: lockRaw, value: packageLock }, configRaw, inferenceRaw] = await Promise.all([
    jsonFile(reportPath, "Playwright JSON report"),
    jsonFile(resolve(root, "desktop/package-lock.json"), "desktop package lock"),
    regularBytes(configPath, "frontend config"),
    regularBytes(resolve(root, "desktop/src/screens/Inference.tsx"), "desktop inference screen", 4 * 1024 * 1024),
  ]);
  const suiteResult = validatePlaywrightReport(report, packageLock);
  const configSha256 = sha256(configRaw);
  assert.equal(
    configSha256,
    expectedHash(options.expectedFrontendConfigSha256, "frontend config expected hash"),
    "frontend config differs from the Pages-verified hash",
  );
  const [runtime, forward] = await Promise.all([
    validateRuntime(options),
    validateForward(options, port),
  ]);
  const packagedNative = await validatePackagedNative(
    options,
    options.sourceCommit,
    configSha256,
    root,
  );
  const packagedAppImage = await validatePackagedAppImage(
    options,
    options.sourceCommit,
    root,
  );
  const inferenceText = inferenceRaw.toString("utf8");
  assert.match(inferenceText, /const MAX_RECEIPT_POLLS = 61;/, "desktop receipt poll count differs from 61");
  assert.match(inferenceText, /const MAX_RECEIPT_POLL_MS = 180_000;/, "desktop receipt wait differs from 180 seconds");

  const suiteSha256 = {};
  for (const name of SUITE_FILES) {
    suiteSha256[name] = sha256(await regularBytes(resolve(root, name), name, 4 * 1024 * 1024));
  }
  const base = `http://127.0.0.1:${port}`;
  const fetchImpl = options.fetchImpl ?? globalThis.fetch;
  assert.equal(typeof fetchImpl, "function", "fetch implementation is required");
  const [directPayload, earningsPayload, blockPayload] = await Promise.all([
    fetchJson(fetchImpl, `${base}/community/reward_receipt/${rewardTx}`, "direct reward receipt"),
    fetchJson(fetchImpl, `${base}/worker/earnings/${worker}`, "worker earnings"),
    fetchJson(fetchImpl, `${base}/tx/${rewardTx}`, "block receipt"),
  ]);
  const direct = validateDirectReceipt(directPayload, rewardTx, worker);
  const earnings = validateEarnings(earningsPayload, rewardTx, worker, direct);
  const blockReceipt = validateBlockReceipt(blockPayload, rewardTx, direct);
  const receipt = {
    appSourceTreeSha256: await sourceTreeHash(root),
    blockReceipt,
    earnings,
    forward,
    frontendConfigSha256: configSha256,
    packageLockSha256: sha256(lockRaw),
    packagedAppImage,
    packagedNative,
    playwrightReportSha256: sha256(reportRaw),
    pollContract: { maxPolls: MAX_RECEIPT_POLLS, maxWaitMs: MAX_RECEIPT_WAIT_MS },
    repository: "FerrumVir/arc-chain",
    rewardReceipt: direct,
    rewardTx,
    rpcOrigin: base,
    rpcPort: port,
    runtime,
    schema: DESKTOP_LIVE_SCHEMA,
    sourceCommit: options.sourceCommit,
    suite: { ...suiteResult, fileSha256: suiteSha256 },
    worker,
  };
  const raw = Buffer.from(canonicalJson(receipt));
  const outputParent = dirname(outputPath);
  const parentPathBefore = await lstat(outputParent);
  assert.ok(parentPathBefore.isDirectory() && !parentPathBefore.isSymbolicLink(), "receipt output parent must be a real directory");
  const parentHandle = await open(
    outputParent,
    fsConstants.O_RDONLY | (fsConstants.O_DIRECTORY ?? 0) | (fsConstants.O_NOFOLLOW ?? 0),
  );
  let handle;
  try {
    const parentFdBefore = await parentHandle.stat();
    assert.deepEqual(
      [parentFdBefore.dev, parentFdBefore.ino],
      [parentPathBefore.dev, parentPathBefore.ino],
      "receipt output parent changed before create",
    );
    handle = await open(
      outputPath,
      fsConstants.O_WRONLY | fsConstants.O_CREAT | fsConstants.O_EXCL | (fsConstants.O_NOFOLLOW ?? 0),
      0o400,
    );
    try {
      await handle.writeFile(raw);
      await handle.sync();
      await handle.chmod(0o400);
      const [fileFd, filePath, parentPathAfter] = await Promise.all([
        handle.stat(),
        lstat(outputPath),
        lstat(outputParent),
      ]);
      assert.deepEqual(
        [filePath.dev, filePath.ino, filePath.size, filePath.nlink],
        [fileFd.dev, fileFd.ino, fileFd.size, fileFd.nlink],
        "receipt output path no longer names the created file",
      );
      assert.equal(fileFd.nlink, 1, "receipt output must have link-count one");
      assert.deepEqual(
        [parentPathAfter.dev, parentPathAfter.ino],
        [parentFdBefore.dev, parentFdBefore.ino],
        "receipt output parent changed during create",
      );
      await parentHandle.sync();
    } finally {
      await handle.close();
    }
  } finally {
    await parentHandle.close();
  }
  return { path: outputPath, receipt, sha256: sha256(raw) };
}

function requiredEnv(name) {
  const value = process.env[name];
  assert.ok(value, `${name} is required`);
  return value;
}

async function main() {
  const result = await buildDesktopLiveProductReceipt({
    expectedFrontendConfigSha256: requiredEnv("ARC_LIVE_CONFIG_SHA256"),
    expectedNodeArchiveSha256: requiredEnv("ARC_LIVE_NODE_ARCHIVE_SHA256"),
    expectedNodeExecutableSha256: requiredEnv("ARC_LIVE_NODE_SHA256"),
    expectedNpmCliSha256: requiredEnv("ARC_LIVE_NPM_CLI_SHA256"),
    expectedNpmPackageSha256: requiredEnv("ARC_LIVE_NPM_PACKAGE_SHA256"),
    expectedSshExecutableSha256: requiredEnv("ARC_LIVE_SSH_SHA256"),
    expectedSshKnownHostsSha256: requiredEnv("ARC_LIVE_SSH_KNOWN_HOSTS_SHA256"),
    appArchive: requiredEnv("ARC_LIVE_APP_ARCHIVE"),
    appArchiveSignature: requiredEnv("ARC_LIVE_APP_ARCHIVE_SIGNATURE"),
    appImageReceipt: requiredEnv("ARC_LIVE_APPIMAGE_RECEIPT"),
    dmg: requiredEnv("ARC_LIVE_DMG"),
    frontendConfig: requiredEnv("ARC_LIVE_CONFIG"),
    nodeArchive: requiredEnv("ARC_LIVE_NODE_ARCHIVE"),
    nodeExecutable: requiredEnv("ARC_LIVE_NODE_PATH"),
    npmCli: requiredEnv("ARC_LIVE_NPM_CLI"),
    npmPackage: requiredEnv("ARC_LIVE_NPM_PACKAGE"),
    macosControllerAttempt: requiredEnv("ARC_LIVE_MACOS_CONTROLLER_ATTEMPT"),
    macosPackageInspection: requiredEnv("ARC_LIVE_MACOS_PACKAGE_INSPECTION"),
    macosPackageProvenance: requiredEnv("ARC_LIVE_MACOS_PACKAGE_PROVENANCE"),
    macosPackageProvenanceVerification: requiredEnv("ARC_LIVE_MACOS_PACKAGE_PROVENANCE_VERIFICATION"),
    macosUpdaterSignatureReceipt: requiredEnv("ARC_LIVE_MACOS_UPDATER_SIGNATURE_RECEIPT"),
    nativeAttempt: requiredEnv("ARC_LIVE_NATIVE_ATTEMPT"),
    nativeHome: requiredEnv("ARC_LIVE_NATIVE_HOME"),
    nativeInput: requiredEnv("ARC_LIVE_NATIVE_INPUT"),
    nativeReceipt: requiredEnv("ARC_LIVE_NATIVE_RECEIPT"),
    output: requiredEnv("ARC_LIVE_RECEIPT_OUTPUT"),
    playwrightReport: requiredEnv("ARC_LIVE_PLAYWRIGHT_REPORT"),
    port: requiredEnv("ARC_LIVE_PORT"),
    rewardTx: `0x${requiredEnv("ARC_LIVE_REWARD_TX").replace(/^0x/i, "").toLowerCase()}`,
    rolloutManifestSha256: requiredEnv("ARC_LIVE_ROLLOUT_MANIFEST_SHA256"),
    sshExecutable: requiredEnv("ARC_LIVE_SSH_PATH"),
    sshIdentitySha256: requiredEnv("ARC_LIVE_SSH_IDENTITY_SHA256"),
    sshKnownHosts: requiredEnv("ARC_LIVE_SSH_KNOWN_HOSTS"),
    sourceCommit: requiredEnv("ARC_LIVE_SOURCE_COMMIT"),
    worker: `0x${requiredEnv("ARC_LIVE_WORKER").replace(/^0x/i, "").toLowerCase()}`,
    validatorHost: requiredEnv("ARC_LIVE_VALIDATOR_HOST"),
    validatorName: requiredEnv("ARC_LIVE_VALIDATOR_NAME"),
    validatorRpcSocket: requiredEnv("ARC_LIVE_VALIDATOR_RPC_SOCKET"),
  });
  console.log(`VERIFIED ARC desktop live product gate sha256=${result.sha256} receipt=${result.path}`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch((error) => {
    console.error(`desktop live product gate failed: ${error?.message ?? error}`);
    process.exitCode = 1;
  });
}
