import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { chmod, mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";

import {
  DESKTOP_LIVE_SCHEMA,
  appBundleTreeSha256,
  blake3Short,
  buildDesktopLiveProductReceipt,
  canonicalJson,
  validatePlaywrightReport,
} from "../../scripts/release/build-desktop-live-product-receipt.mjs";

const ROOT = resolve(import.meta.dirname, "../..");
const WORKER = `0x${"99".repeat(32)}`;
const TX = `0x${"aa".repeat(32)}`;
const JOB = `0x${"01".repeat(32)}`;
const MODEL = `0x${"02".repeat(32)}`;
const INPUT = `0x${"03".repeat(32)}`;
const OUTPUT = `0x${"04".repeat(32)}`;
const ASSIGNMENT = `0x${"05".repeat(32)}`;
const DOMAIN = `0x${"06".repeat(32)}`;
const BLOCK = `0x${"07".repeat(32)}`;
const COMMITMENT = `0x${"08".repeat(32)}`;
const ROLLOUT = "09".repeat(32);
const TESTS = [
  "first-launch: welcome → onboarding → dashboard with real data",
  "dashboard shows real-node peers + committed blocks + attestations",
  "sealed 0x25 canary is visible in earnings and host-scoped explorer",
  "network screen reads real validator count + latest block",
];

const packageLock = JSON.parse(
  await readFile(join(ROOT, "desktop/package-lock.json"), "utf8"),
);
const playwrightVersion = packageLock.packages["node_modules/@playwright/test"].version;

function sha256(raw) {
  return createHash("sha256").update(raw).digest("hex");
}

async function writePrivateJson(path, value) {
  const raw = Buffer.from(canonicalJson(value));
  await writeFile(path, raw, { mode: 0o400 });
  await chmod(path, 0o400);
  return raw;
}

function report(testStatus = "expected", resultStatus = "passed") {
  return {
    config: {
      forbidOnly: true,
      fullyParallel: false,
      workers: 1,
      version: playwrightVersion,
      projects: [{ name: "chromium", retries: 0, testMatch: ["**/live.spec.ts"] }],
    },
    errors: [],
    suites: [{
      title: "live.spec.ts",
      file: "live.spec.ts",
      specs: TESTS.map((title) => ({
        title,
        file: "live.spec.ts",
        ok: testStatus === "expected" && resultStatus === "passed",
        tests: [{
          expectedStatus: "passed",
          status: testStatus,
          results: [{ status: resultStatus }],
        }],
      })),
      suites: [],
    }],
    stats: {
      duration: 1234,
      expected: testStatus === "expected" && resultStatus === "passed" ? 4 : 0,
      flaky: 0,
      skipped: testStatus === "skipped" ? 4 : 0,
      startTime: "2026-09-06T12:00:00.000Z",
      unexpected: resultStatus === "failed" ? 4 : 0,
    },
  };
}

function directReceipt(overrides = {}) {
  return {
    assignment_epoch: ASSIGNMENT,
    block_hash: BLOCK,
    block_height: 137_150,
    confirmed: true,
    evidence_source: "successful mined CommunityInferenceReward receipt",
    included: true,
    index: 2,
    input_hash: INPUT,
    job_id: JOB,
    model_id: MODEL,
    output_hash: OUTPUT,
    receipt_url: `/community/reward_receipt/${TX}`,
    recovery_epoch: 1,
    reward_arc: 2.5,
    reward_base: 2_500_000_000,
    status: "mined_success",
    submitted: true,
    success: true,
    transaction_domain: DOMAIN,
    tx_hash: TX,
    tx_type: "0x25",
    validator_approvals: 5,
    validator_set_commitment: COMMITMENT,
    validator_set_id: 1,
    worker: WORKER,
    ...overrides,
  };
}

function earningsReceipt(overrides = {}) {
  const direct = directReceipt();
  const {
    evidence_source: _evidenceSource,
    status: _status,
    validator_approvals: _approvals,
    validator_set_commitment: _commitment,
    ...row
  } = direct;
  return { ...row, ...overrides };
}

function earnings(overrides = {}) {
  return {
    address: WORKER,
    archive_mode: false,
    community_rewards_v1_approval_collection_ready: true,
    community_rewards_v1_enabled: true,
    community_rewards_v1_protocol_active: true,
    confirmed_gross_earnings_arc: 2.5,
    confirmed_gross_earnings_base: 2_500_000_000,
    confirmed_receipt_count: 1,
    confirmed_receipts: [earningsReceipt()],
    estimated_total_arc: 2.5,
    history_complete_since_recovery: false,
    history_domain:
      "all canonical 0x25 reward domains since the v3 recovery boundary; historical rows retain their own recovery_epoch, validator_set_id, and transaction_domain",
    history_scope: "this node's bounded retained reward-receipt window",
    issuance_ready_for_worker: true,
    projected_daily_arc: null,
    projected_daily_unavailable_reason: "collecting confirmed receipt history",
    recovery_epoch: 1,
    reward_per_attestation_arc: 2.5,
    reward_per_attestation_base: 2_500_000_000,
    source: "scan of this node's in-memory full_transactions map",
    stake_zero_eligible: true,
    total_rewards: 1,
    validator_set_commitment: COMMITMENT,
    validator_set_id: 1,
    worker_min_stake_base: 0,
    ...overrides,
  };
}

function blockReceipt(overrides = {}) {
  return {
    block_hash: BLOCK.slice(2),
    block_height: 137_150,
    gas_used: 50_000,
    inclusion_proof: null,
    index: 2,
    logs: [],
    success: true,
    tx_hash: TX.slice(2),
    value_commitment: null,
    ...overrides,
  };
}

function nativeNetwork(overrides = {}) {
  return {
    chainId: "arc-testnet-v3",
    dagCommitted: 137_151,
    dagRound: 137_151,
    declaresMainnet: false,
    height: 137_151,
    hostVersion: "0.8.0",
    isBlockProducing: true,
    isBlockProducingBasis: "fresh canonical block",
    lastBlockAgeSecs: 1,
    minActiveStake: 1,
    networkName: "ARC Testnet",
    networkNameUnavailableReason: null,
    peers: 5,
    sourceHost: "https://140.82.16.112",
    unavailable: null,
    validatorSplitDerived: false,
    validators: Array.from({ length: 6 }, (_, index) => ({
      active: true,
      address: `${String(index + 1).padStart(2, "0")}`.repeat(32),
      stake: 1,
    })),
    validatorsActive: 6,
    validatorsRegistered: 6,
    ...overrides,
  };
}

function nativeSettlement(overrides = {}) {
  return {
    assignmentEpoch: ASSIGNMENT,
    blockHash: BLOCK,
    blockHeight: 137_150,
    confirmed: true,
    evidenceSource: "successful mined CommunityInferenceReward receipt",
    included: true,
    index: 2,
    inputHash: INPUT,
    jobId: JOB,
    modelId: MODEL,
    outputHash: OUTPUT,
    receiptUrl: `/community/reward_receipt/${TX}`,
    recoveryEpoch: 1,
    rewardArc: 2.5,
    rewardBase: 2_500_000_000,
    status: "mined_success",
    submitted: true,
    success: true,
    transactionDomain: DOMAIN,
    txHash: TX,
    txType: "0x25",
    validatorApprovals: 5,
    validatorSetCommitment: COMMITMENT,
    validatorSetId: 1,
    worker: WORKER,
    ...overrides,
  };
}

function mockFetch(payloads) {
  return async (url) => {
    const path = new URL(url).pathname;
    const body = path.startsWith("/community/reward_receipt/")
      ? payloads.direct
      : path.startsWith("/worker/earnings/")
        ? payloads.earnings
        : payloads.block;
    return new Response(JSON.stringify(body), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
  };
}

async function fixture(overrides = {}) {
  const root = await mkdtemp(join(tmpdir(), "arc-desktop-live-receipt-test-"));
  const reportPath = join(root, "playwright.json");
  const configPath = join(root, "arc-network.json");
  const nodeArchive = join(root, "node.tar.xz");
  const npmCli = join(root, "npm-cli.js");
  const npmPackage = join(root, "npm-package.json");
  const sshExecutable = join(root, "fixture-ssh");
  const knownHosts = join(root, "known_hosts");
  const appArchive = join(root, "arc-desktop-macos-arm64.app.tar.gz");
  const appArchiveSignature = join(root, "arc-desktop-macos-arm64.app.tar.gz.sig");
  const dmg = join(root, "arc-desktop-macos-arm64.dmg");
  const archiveAppBundle = join(root, "archive", "ARC.app");
  const dmgAppBundle = join(root, "dmg", "ARC.app");
  const nativeInputPath = join(root, "DESKTOP-LIVE-INPUT.json");
  const nativeAttemptPath = join(root, "PACKAGED-NATIVE-DISPATCH-ATTEMPT.json");
  const nativeReceiptPath = join(root, "PACKAGED-NATIVE-ACCEPTANCE.json");
  const macosControllerAttemptPath = join(root, "MACOS-NATIVE-CONTROLLER-ATTEMPT.json");
  const macosPackageInspectionPath = join(root, "MACOS-PACKAGE-INSPECTION.json");
  const macosPackageProvenancePath = join(root, "MACOS-PACKAGE-PROVENANCE.json");
  const macosPackageProvenanceVerificationPath = join(root, "MACOS-PACKAGE-PROVENANCE-VERIFICATION.json");
  const macosUpdaterSignatureReceiptPath = join(root, "MACOS-UPDATER-SIGNATURE.json");
  const appImageReceiptPath = join(root, "PACKAGED-APPIMAGE-HOST.json");
  const output = join(root, "receipt.json");
  const nativeHome = join(root, "isolated-home");
  await mkdir(join(nativeHome, "tmp"), { recursive: true });
  await writeFile(reportPath, JSON.stringify(overrides.report ?? report()));
  const configRaw = Buffer.from(canonicalJson({ schema: "test-config" }));
  const archiveRaw = Buffer.from("fixture Node distribution archive\n");
  const npmCliRaw = Buffer.from("fixture npm CLI\n");
  const npmPackageRaw = Buffer.from(canonicalJson({ version: "fixture-npm" }));
  const sshExecutableRaw = Buffer.from("fixture SSH executable\n");
  const knownHostsRaw = Buffer.from("fixture exact known-hosts\n");
  const appArchiveRaw = Buffer.from("fixture exact app archive\n");
  const appArchiveSignatureRaw = Buffer.from("fixture updater signature\n");
  const dmgRaw = Buffer.from("fixture exact DMG\n");
  await writeFile(configPath, configRaw);
  await writeFile(nodeArchive, archiveRaw);
  await writeFile(npmCli, npmCliRaw);
  await writeFile(npmPackage, npmPackageRaw);
  await writeFile(sshExecutable, sshExecutableRaw, { mode: 0o700 });
  await chmod(sshExecutable, 0o700);
  await writeFile(knownHosts, knownHostsRaw);
  await writeFile(appArchive, appArchiveRaw);
  await writeFile(appArchiveSignature, appArchiveSignatureRaw);
  await writeFile(dmg, dmgRaw);
  for (const bundle of [archiveAppBundle, dmgAppBundle]) {
    await mkdir(join(bundle, "Contents", "MacOS"), { recursive: true });
    await chmod(join(bundle, "Contents"), 0o755);
    await chmod(join(bundle, "Contents", "MacOS"), 0o755);
    await writeFile(join(bundle, "Contents", "MacOS", "arc-desktop"), "fixture executable\n");
    await chmod(join(bundle, "Contents", "MacOS", "arc-desktop"), 0o755);
    await writeFile(join(bundle, "Contents", "Info.plist"), "fixture plist\n");
    await chmod(join(bundle, "Contents", "Info.plist"), 0o644);
  }
  const bundleTree = await appBundleTreeSha256(dmgAppBundle, "fixture DMG app");
  const bundleExecutableRaw = await readFile(join(dmgAppBundle, "Contents", "MacOS", "arc-desktop"));
  const sourceCommit = "ab".repeat(20);
  const now = Math.floor(Date.now() / 1000);
  const nativeAssets = {
    appArchive: { id: 101, name: "arc-desktop-macos-arm64.app.tar.gz", sha256: sha256(appArchiveRaw), size: appArchiveRaw.length },
    appArchiveSignature: { id: 102, name: "arc-desktop-macos-arm64.app.tar.gz.sig", sha256: sha256(appArchiveSignatureRaw), size: appArchiveSignatureRaw.length },
    dmg: { id: 103, name: "arc-desktop-macos-arm64.dmg", sha256: sha256(dmgRaw), size: dmgRaw.length },
  };
  const nativeBudgets = {
    dispatchMaxWaitMs: 3_960_000,
    earningsMaxPolls: 11,
    earningsMaxWaitMs: 30_000,
    finalReadMaxPolls: 11,
    finalReadMaxWaitMs: 30_000,
    preflightMaxWaitMs: 30_000,
    receiptMaxPolls: 61,
    receiptMaxWaitMs: 180_000,
    totalMaxWaitMs: 4_300_000,
  };
  const nativeInput = {
    assets: nativeAssets,
    budgets: nativeBudgets,
    challenge: "11".repeat(32),
    expectedActiveValidators: 6,
    expectedBundle: {
      appBundleTreeSha256: bundleTree,
      executableRelativePath: "Contents/MacOS/arc-desktop",
      executableSha256: sha256(bundleExecutableRaw),
      executableSize: bundleExecutableRaw.length,
    },
    expectedCoordinator: "https://140.82.16.112",
    expectedModelId: MODEL,
    expectedRegisteredValidators: 6,
    expiresAtUnix: now + 7200,
    frontendCommit: sourceCommit,
    frontendConfigSha256: sha256(configRaw),
    issuedAtUnix: now,
    maximumBlockAgeSeconds: 300,
    minimumPeers: 5,
    pagesOrigin: "https://ferrumvir.github.io/arc-chain",
    recoveryEpoch: 1,
    releaseId: 10,
    releaseRunAttempt: 1,
    releaseRunId: 11,
    releaseVersion: "0.8.0",
    repository: "FerrumVir/arc-chain",
    rolloutManifestSha256: ROLLOUT,
    schema: "arc.packaged-desktop-native-input.v1",
    sourceCommit,
    transactionDomain: DOMAIN,
    validatorApprovalsRequired: 5,
    validatorSetCommitment: COMMITMENT,
    validatorSetId: 1,
  };
  const inputRaw = Buffer.from(canonicalJson(nativeInput));
  await writeFile(nativeInputPath, inputRaw, { mode: 0o400 });
  await chmod(nativeInputPath, 0o400);
  const nativeAttempt = {
    armedAt: new Date().toISOString(),
    challenge: nativeInput.challenge,
    dispatchLimit: 1,
    executableSha256: nativeInput.expectedBundle.executableSha256,
    inputSha256: sha256(inputRaw),
    schema: "arc.packaged-desktop-native-dispatch-attempt.v1",
    sourceCommit,
    sourceHost: nativeInput.expectedCoordinator,
  };
  const attemptRaw = Buffer.from(canonicalJson(nativeAttempt));
  await writeFile(nativeAttemptPath, attemptRaw, { mode: 0o400 });
  await chmod(nativeAttemptPath, 0o400);
  const prompt = `ARC packaged v0.8.0 production acceptance challenge ${nativeInput.challenge} input ${sha256(inputRaw)}`;
  const promptInputHash = `0x${blake3Short(Buffer.from(prompt, "utf8"))}`;
  const nativeOutput = "accepted";
  const nativeOutputHash = `0x${blake3Short(Buffer.from(nativeOutput, "utf8"))}`;
  const settlement = nativeSettlement({ inputHash: promptInputHash, outputHash: nativeOutputHash });
  const provisionalSettlement = nativeSettlement({
    assignmentEpoch: ASSIGNMENT,
    blockHash: null,
    blockHeight: null,
    confirmed: false,
    evidenceSource: "coordinator mempool submission only; no mined receipt",
    included: false,
    index: null,
    inputHash: "",
    modelId: "",
    outputHash: "",
    recoveryEpoch: 1,
    rewardArc: null,
    rewardBase: null,
    status: "pending_mined_receipt",
    success: null,
    transactionDomain: DOMAIN,
    validatorApprovals: 5,
    validatorSetCommitment: COMMITMENT,
    validatorSetId: 1,
  });
  const inferenceResult = {
    coordinator: nativeInput.expectedCoordinator,
    deterministic: true,
    engine: "arc",
    executionProfile: "INT8 integer (per-row, cross-platform deterministic)",
    explorerUrl: "",
    inferenceMs: 1,
    input: prompt,
    modelHash: MODEL,
    output: nativeOutput,
    outputHash: nativeOutputHash,
    profileBound: true,
    quorumVerified: true,
    routedVia: `community:${WORKER}`,
    servedLocally: false,
    settlement: provisionalSettlement,
    tokensGenerated: 1,
    txHash: "",
  };
  const nativeEarningsValue = {
    archiveMode: false,
    attestations: 1,
    confirmedReceipts: [{
      blockHash: BLOCK,
      blockHeight: 137_150,
      jobId: JOB,
      receiptUrl: `/community/reward_receipt/${TX}`,
      recoveryEpoch: 1,
      rewardArc: 2.5,
      rewardBase: 2_500_000_000,
      txHash: TX,
      validatorSetId: 1,
    }],
    fromChain: true,
    historyCompleteSinceRecovery: false,
    historyScope: "this node's bounded retained reward-receipt window",
    lastPayoutAt: null,
    lastPayoutBlock: 137_150,
    pendingArc: null,
    projectedDailyArc: null,
    projectedDailyUnavailableReason: "needs more observations",
    rank: null,
    receiptSource: "scan of this node's in-memory full_transactions map",
    recoveryEpoch: 1,
    todayArc: null,
    totalArc: 2.5,
    unavailableReason: null,
    validatorSetId: 1,
  };
  const nativeReceipt = {
    assets: nativeAssets,
    budgets: nativeBudgets,
    bundle: nativeInput.expectedBundle,
    challenge: nativeInput.challenge,
    completedAt: new Date((now + 2) * 1000).toISOString(),
    dispatch: {
      chatTemplate: true,
      count: 1,
      elapsedMs: 1000,
      maxTokens: 2,
      maxWaitMs: 3_960_000,
      promptSha256: sha256(Buffer.from(prompt)),
      result: inferenceResult,
      sourceHost: nativeInput.expectedCoordinator,
    },
    dispatchAttemptSha256: sha256(attemptRaw),
    earnings: { attempts: 1, elapsedMs: 1, maxPolls: 11, maxWaitMs: 30_000, value: nativeEarningsValue },
    expectedCoordinator: nativeInput.expectedCoordinator,
    expectedModelId: nativeInput.expectedModelId,
    expiresAtUnix: nativeInput.expiresAtUnix,
    finalReadAttempts: 1,
    finalReadElapsedMs: 1,
    frontendCommit: sourceCommit,
    frontendConfigSha256: nativeInput.frontendConfigSha256,
    immutableSessionOrigin: true,
    inputSha256: sha256(inputRaw),
    issuedAtUnix: nativeInput.issuedAtUnix,
    negativeRouteChecks: { job: true, origin: true, receiptUrl: true, transaction: true, worker: true },
    network: nativeNetwork(),
    pagesOrigin: nativeInput.pagesOrigin,
    preflightNetwork: nativeNetwork({ height: 137_149 }),
    preflightModelId: nativeInput.expectedModelId,
    projection: {
      attestationsPerDay: null,
      attestationsTotal: 1,
      communityRewardsEnabled: true,
      coordinatorRewardsRemainingThisEpoch: 1,
      firstAttestationBlock: 137_150,
      issuanceReadyForWorker: true,
      observedOverBlocks: null,
      projectedDailyArc: null,
      projectedDailyUnavailableReason: "needs more observations",
      rateCaveat: null,
      rateUnavailableReason: "needs more observations",
      rewardBudgetEpoch: 1,
      rewardIsCustomerDemand: false,
      rewardPerAttestation: 2.5,
      rewardPolicyHash: `0x${"12".repeat(32)}`,
      rewardProgram: "protocol-capped testnet promotional compute subsidy",
      rewardRateSource: "chain",
      rewardsRemainingThisEpoch: 1,
      sourceHost: nativeInput.expectedCoordinator,
      unavailable: null,
      workerRewardsRemainingThisEpoch: 1,
    },
    receiptPoll: { attempts: 1, elapsedMs: 1, maxPolls: 61, maxWaitMs: 180_000, receipt: settlement },
    recentBlocks: {
      blocks: [{ hash: BLOCK.slice(2), height: 137_150, proposer: null, timestampMs: 1, txCount: 1 }],
      sourceHost: nativeInput.expectedCoordinator,
      unavailable: null,
    },
    releaseId: nativeInput.releaseId,
    releaseRunAttempt: nativeInput.releaseRunAttempt,
    releaseRunId: nativeInput.releaseRunId,
    releaseVersion: nativeInput.releaseVersion,
    repository: nativeInput.repository,
    rolloutManifestSha256: nativeInput.rolloutManifestSha256,
    runtime: {
      appDataRelativePath: "Library/Application Support/network.arc.desktop",
      appVersion: "0.8.0",
      architecture: "aarch64",
      buildSourceCommit: sourceCommit,
      environmentNames: ["HOME", "LANG", "LC_ALL", "PATH", "TMPDIR"],
      environmentSha256: sha256(Buffer.from([
        `HOME=${nativeHome}`,
        "LANG=C",
        "LC_ALL=C",
        "PATH=/usr/bin:/bin:/usr/sbin:/sbin",
        `TMPDIR=${join(nativeHome, "tmp")}`,
      ].sort().join("\0") + "\0")),
      ipcHandlersRegistered: false,
      isolatedHomeBasename: "isolated-home",
      operatingSystem: "macos",
      pluginsLoaded: false,
      tauriBuilderStarted: false,
      webviewsCreated: 0,
    },
    schema: "arc.packaged-desktop-native-acceptance.v1",
    sourceCommit,
    startedAt: new Date((now + 1) * 1000).toISOString(),
    transaction: {
      blockHash: BLOCK.slice(2),
      blockHeight: 137_150,
      gasUsed: 50_000,
      hash: TX.slice(2),
      sourceHost: nativeInput.expectedCoordinator,
      status: "mined",
      success: true,
      txIndex: 2,
      unavailable: null,
    },
    worker: WORKER,
  };
  overrides.mutateNative?.(nativeReceipt);
  const nativeReceiptRaw = await writePrivateJson(nativeReceiptPath, nativeReceipt);
  const controllerRaw = await readFile(join(ROOT, "scripts/recovery/build-macos-package-provenance.py"));
  const controllerSha256 = sha256(controllerRaw);
  const bindingSha256 = "44".repeat(32);
  const controllerSource = {
    commit: sourceCommit,
    path: "scripts/recovery/build-macos-package-provenance.py",
    sha256: controllerSha256,
    treeClean: true,
  };
  const macosRelease = { id: 10, runAttempt: 1, runId: 11, tag: "v0.8.0" };
  const codeSignature = {
    appleDeveloperIdSigned: false,
    authorities: [],
    designatedRequirement: 'designated => identifier "network.arc.desktop"',
    designatedRequirementKind: "identifier",
    displayStderrSha256: "45".repeat(32),
    displayStdoutSha256: "46".repeat(32),
    gatekeeperAssessed: false,
    hardenedRuntime: false,
    identifier: "network.arc.desktop",
    infoPlistSha256: "47".repeat(32),
    kind: "adhoc",
    notarizationAssessed: false,
    requirementsStderrSha256: "48".repeat(32),
    requirementsStdoutSha256: "49".repeat(32),
    teamIdentifier: null,
    verifier: {
      path: "/usr/bin/codesign",
      resolvedPath: "/usr/bin/codesign",
      sha256: "4a".repeat(32),
      size: 1,
    },
    verifyDeepStrict: true,
    verifyStderrSha256: "4b".repeat(32),
    verifyStdoutSha256: "4c".repeat(32),
  };
  const signatureGuest = {
    apt: {
      installStderrSha256: "4d".repeat(32),
      installStdoutSha256: "4e".repeat(32),
      snapshot: "20260321T235959Z",
      sourceSha256: "4bfd00f33ef52b3b75a41988c6362874422c54210d60022a73e57740e2d60b15",
      updateStderrSha256: "4f".repeat(32),
      updateStdoutSha256: "50".repeat(32),
    },
    archiveSha256: nativeAssets.appArchive.sha256,
    completedAt: "2026-09-06T12:00:00Z",
    helperSha256: controllerSha256,
    minisign: {
      binary: { path: "/usr/bin/minisign", resolvedPath: "/usr/bin/minisign", sha256: "4e".repeat(32), size: 1 },
      package: { architecture: "amd64", version: "0.11-1" },
    },
    publicKeySha256: "4f".repeat(32),
    schema: "arc.macos-updater-signature-guest.v1",
    signatureSha256: nativeAssets.appArchiveSignature.sha256,
    verification: { stderrSha256: "50".repeat(32), stdoutSha256: "51".repeat(32), verified: true },
  };
  const macosSignatureReceipt = {
    assets: { appArchive: nativeAssets.appArchive, appArchiveSignature: nativeAssets.appArchiveSignature },
    bindingSha256,
    completedAt: "2026-09-06T12:00:01Z",
    disposableVm: {
      configSha256: "865b35d8bb272aafd60b5f80838632eff2699eb2061d73ead8494cf49f657095",
      deletedAfterEvidenceRead: true,
      imageDigest: "sha256:5c3ddb00f60bc455dac0862fabe9d8bacec46c33ac1751143c5c3683404b110d",
      imageUrl: "https://cloud-images.ubuntu.com/releases/noble/release-20260321/ubuntu-24.04-server-cloudimg-amd64.img",
      mounts: [],
      name: `arc-macos-signature-v080-${sourceCommit.slice(0, 8)}-abc123`,
      preexistingInstancesUnchanged: true,
      recoveryEnclaveAccessed: false,
    },
    guest: signatureGuest,
    guestReceiptSha256: sha256(Buffer.from(canonicalJson(signatureGuest))),
    limactl: { path: "/opt/homebrew/bin/limactl", resolvedPath: "/opt/homebrew/bin/limactl", sha256: "53".repeat(32), size: 1 },
    release: macosRelease,
    repository: "FerrumVir/arc-chain",
    schema: "arc.macos-updater-signature-verification.v1",
    source: controllerSource,
    sourceCommit,
    updaterPublicKeySha256: signatureGuest.publicKeySha256,
    verified: true,
  };
  let macosSignatureRaw = Buffer.from(canonicalJson(macosSignatureReceipt));
  const makeMacosMount = (mountPointBasename) => ({
    attachPlistSha256: "54".repeat(32),
    device: "/dev/disk4s1",
    diskutilInfoPlistSha256: "55".repeat(32),
    filesystem: "apfs",
    hdiutilInfoPlistSha256: "56".repeat(32),
    imagePathMatched: true,
    imagePathSha256: sha256(Buffer.from(resolve(dmg))),
    mountFlags: { lineSha256: "57".repeat(32), options: ["read-only", "noowners", "nobrowse"] },
    mountPointBasename,
    nobrowse: true,
    noowners: true,
    readOnlyMedia: true,
    readOnlyVolume: true,
    statvfsReadOnly: true,
    tools: {
      codesign: { path: "/usr/bin/codesign", resolvedPath: "/usr/bin/codesign", sha256: "60".repeat(32), size: 1 },
      diskutil: { path: "/usr/sbin/diskutil", resolvedPath: "/usr/sbin/diskutil", sha256: "61".repeat(32), size: 1 },
      hdiutil: { path: "/usr/bin/hdiutil", resolvedPath: "/usr/bin/hdiutil", sha256: "62".repeat(32), size: 1 },
      mount: { path: "/sbin/mount", resolvedPath: "/sbin/mount", sha256: "63".repeat(32), size: 1 },
    },
    verifyStderrSha256: "58".repeat(32),
    verifyStdoutSha256: "59".repeat(32),
    writeable: false,
  });
  const detachEvidence = {
    detachStderrSha256: "5a".repeat(32),
    detachStdoutSha256: "5b".repeat(32),
    detached: true,
    mountPointEmpty: true,
    postDetachInfoPlistSha256: "5c".repeat(32),
  };
  const bundleTreeSummary = {
    entryCount: 3,
    sha256: nativeInput.expectedBundle.appBundleTreeSha256,
    totalRegularBytes: nativeInput.expectedBundle.executableSize + Buffer.byteLength("fixture plist\n"),
  };
  const macosInspection = {
    assets: nativeAssets,
    bindingSha256,
    bundle: nativeInput.expectedBundle,
    codeSignature,
    completedAt: "2026-09-06T12:00:02Z",
    dmg: {
      appBundleTree: bundleTreeSummary,
      attach: makeMacosMount("inspect-dmg-mount"),
      detach: detachEvidence,
      rootInventory: [{ kind: "directory", mode: 0o755, name: "ARC Node.app", target: null }],
      sha256AfterDetach: nativeAssets.dmg.sha256,
    },
    extraction: {
      appBundleRelativePath: "ARC Node.app",
      appBundleTreeSha256: nativeInput.expectedBundle.appBundleTreeSha256,
      archiveSha256AfterExtraction: nativeAssets.appArchive.sha256,
      implementation: "arc-openat-create-only-tar-extractor-v1",
      limits: {
        maxDepth: 33,
        maxEntries: 20_000,
        maxMemberBytes: 512 * 1024 * 1024,
        maxPathBytes: 4_096,
        maxTotalRegularBytes: 2 * 1024 * 1024 * 1024,
      },
      manifest: {
        directoryCount: 2,
        memberCount: 4,
        regularFileCount: 2,
        symlinkCount: 0,
        totalRegularBytes: nativeInput.expectedBundle.executableSize + Buffer.byteLength("fixture plist\n"),
      },
      safety: {
        absolutePathsRejected: true,
        caseAndNormalizationCollisionsRejected: true,
        descriptorRelativeCreateOnly: true,
        hardlinksAndSpecialEntriesRejected: true,
        parentTraversalRejected: true,
        setIdAndStickyModesRejected: true,
        symlinksResolvedInsideBundle: true,
      },
    },
    release: macosRelease,
    repository: "FerrumVir/arc-chain",
    schema: "arc.macos-package-inspection.v1",
    source: controllerSource,
    sourceCommit,
    updaterSignature: {
      receiptSha256: sha256(macosSignatureRaw),
      schema: "arc.macos-updater-signature-verification.v1",
      updaterPublicKeySha256: macosSignatureReceipt.updaterPublicKeySha256,
      verified: true,
    },
  };
  let macosInspectionRaw = Buffer.from(canonicalJson(macosInspection));
  const macosControllerAttempt = {
    armedAt: "2026-09-06T12:00:03Z",
    challenge: nativeInput.challenge,
    executableSha256: nativeInput.expectedBundle.executableSha256,
    inputSha256: sha256(inputRaw),
    inspectionSha256: sha256(macosInspectionRaw),
    outerTimeoutSeconds: 4_360,
    retryPermitted: false,
    schema: "arc.macos-native-controller-attempt.v1",
    sourceCommit,
    state: "armed-no-retry",
  };
  let macosControllerAttemptRaw = Buffer.from(canonicalJson(macosControllerAttempt));
  const nativeMount = makeMacosMount("native-dmg-mount");
  const macosProvenance = {
    assets: nativeAssets,
    bindingSha256,
    bundle: nativeInput.expectedBundle,
    codeSignature: { after: codeSignature, before: codeSignature, semanticIdentityUnchanged: true },
    completedAt: "2026-09-06T12:00:05Z",
    controllerAttemptSha256: sha256(macosControllerAttemptRaw),
    dmgExecution: {
      attach: nativeMount,
      bundleAfter: nativeInput.expectedBundle,
      bundleBefore: nativeInput.expectedBundle,
      detach: detachEvidence,
      treeAfter: bundleTreeSummary,
      treeBefore: bundleTreeSummary,
    },
    extractedArchiveBundleAfter: nativeInput.expectedBundle,
    inspectionSha256: sha256(macosInspectionRaw),
    nativeDispatchAttemptSha256: sha256(attemptRaw),
    nativeExecution: {
      completedAt: "2026-09-06T12:00:05Z",
      elapsedMs: 1_000,
      innerMaxSeconds: 4_300,
      outerTimeoutSeconds: 4_360,
      returnCode: 0,
      startedAt: "2026-09-06T12:00:04Z",
      stderrSha256: "5d".repeat(32),
      stderrSize: 0,
      stdoutSha256: "5e".repeat(32),
      stdoutSize: 1,
      timedOut: false,
    },
    nativeInputSha256: sha256(inputRaw),
    nativeReceiptSha256: sha256(nativeReceiptRaw),
    release: macosRelease,
    repository: "FerrumVir/arc-chain",
    schema: "arc.macos-packaged-native-provenance.v1",
    source: controllerSource,
    sourceCommit,
    truthScope: {
      appleDeveloperIdSigned: false,
      exactMountedDmgExecutableRan: true,
      gatekeeperAssessed: false,
      nativeCoreOnly: true,
      notarizationAssessed: false,
      shippedDebugOrWebdriverSurfaceAdded: false,
      uiToIpcCoveredByThisReceipt: false,
      updaterArchiveMinisignVerified: true,
    },
    updaterSignatureReceiptSha256: sha256(macosSignatureRaw),
  };
  let macosProvenanceRaw = Buffer.from(canonicalJson(macosProvenance));
  const macosVerification = {
    assets: nativeAssets,
    bindingSha256,
    bundle: nativeInput.expectedBundle,
    completedAt: "2026-09-06T12:00:06Z",
    controllerAttemptSha256: sha256(macosControllerAttemptRaw),
    inspectionSha256: sha256(macosInspectionRaw),
    nativeDispatchAttemptSha256: sha256(attemptRaw),
    nativeInputSha256: sha256(inputRaw),
    nativeReceiptSha256: sha256(nativeReceiptRaw),
    provenanceSha256: sha256(macosProvenanceRaw),
    release: macosRelease,
    repository: "FerrumVir/arc-chain",
    schema: "arc.macos-package-provenance-verification.v1",
    source: controllerSource,
    sourceCommit,
    updaterSignatureReceiptSha256: sha256(macosSignatureRaw),
    verified: true,
  };
  overrides.mutateMacosEvidence?.({
    controllerAttempt: macosControllerAttempt,
    inspection: macosInspection,
    provenance: macosProvenance,
    signature: macosSignatureReceipt,
    verification: macosVerification,
  });
  macosSignatureRaw = await writePrivateJson(macosUpdaterSignatureReceiptPath, macosSignatureReceipt);
  macosInspectionRaw = await writePrivateJson(macosPackageInspectionPath, macosInspection);
  macosControllerAttemptRaw = await writePrivateJson(macosControllerAttemptPath, macosControllerAttempt);
  macosProvenanceRaw = await writePrivateJson(macosPackageProvenancePath, macosProvenance);
  await writePrivateJson(macosPackageProvenanceVerificationPath, macosVerification);
  const nodeExecutable = process.execPath;
  const nodeRaw = await readFile(nodeExecutable);
  const appImageGateRaw = await readFile(join(ROOT, "scripts/release/packaged-appimage-live-gate.py"));
  const appImageRaw = Buffer.from("fixture AppImage bytes");
  const appImageSigRaw = Buffer.from("fixture AppImage signature");
  const appImageHostReceipt = {
    completed_at: "2026-09-06T12:00:00Z",
    disposable_vm: {
      config_sha256: "21".repeat(32),
      deleted_after_evidence_copy: true,
      image_digest: "sha256:5c3ddb00f60bc455dac0862fabe9d8bacec46c33ac1751143c5c3683404b110d",
      image_url: "https://cloud-images.ubuntu.com/releases/noble/release-20260321/ubuntu-24.04-server-cloudimg-amd64.img",
      mounts: [],
      name: `arc-packaged-live-v080-${sourceCommit.slice(0, 8)}-abc123`,
      preexisting_instances_unchanged: true,
      recovery_enclave_accessed: false,
    },
    guest_receipt: {
      path: "guest-evidence/receipt.json",
      schema: "arc.packaged-appimage-live-product.v1",
      sha256: "22".repeat(32),
    },
    host_runtime: {
      limactl: { path: "/opt/homebrew/bin/limactl", resolved_path: "/opt/homebrew/bin/limactl", sha256: "23".repeat(32), size: 1 },
      limactl_version: "limactl version 2.1.1",
      ssh: { path: "/usr/bin/ssh", resolved_path: "/usr/bin/ssh", sha256: "24".repeat(32), size: 1 },
    },
    inference_attempt: {
      path: "inference-attempt.json",
      plan_sha256: "25".repeat(32),
      sha256: "26".repeat(32),
      state: "armed-no-retry",
    },
    platform_claim: "published Linux x86_64 AppImage launched by WebKitWebDriver under WebKitGTK, exercising real bundled UI and Tauri IPC; this does not prove the macOS .app/WKWebView or Windows WebView2 packages",
    release: {
      assets: {
        "arc-desktop-linux-x86_64.AppImage": { id: 201, name: "arc-desktop-linux-x86_64.AppImage", sha256: sha256(appImageRaw), size: appImageRaw.length },
        "arc-desktop-linux-x86_64.AppImage.sig": { id: 202, name: "arc-desktop-linux-x86_64.AppImage.sig", sha256: sha256(appImageSigRaw), size: appImageSigRaw.length },
        "arc-node-linux-x86_64": { id: 203, name: "arc-node-linux-x86_64", sha256: "27".repeat(32), size: 1 },
      },
      binding_sha256: "28".repeat(32),
      commit: sourceCommit,
      release_id: 10,
      repository: "FerrumVir/arc-chain",
      tag: "v0.8.0",
    },
    result: "passed",
    schema: "arc.packaged-appimage-live-host.v1",
    source: {
      commit: sourceCommit,
      gate_path: "scripts/release/packaged-appimage-live-gate.py",
      gate_sha256: sha256(appImageGateRaw),
      tree_clean: true,
    },
    transport: {
      accepted_connect_count: 1,
      identity_sha256: "29".repeat(32),
      known_hosts_sha256: "2a".repeat(32),
      relay_log_sha256: "2b".repeat(32),
      relay_target: "140.82.16.112:443",
      relay_token_sha256: "2c".repeat(32),
      remote: "root@140.82.16.112:127.0.0.1:443",
      ssh_options_sha256: "2d".repeat(32),
      ssh_stderr_sha256: "2e".repeat(32),
      ssh_stdout_sha256: "2f".repeat(32),
    },
    updater_signature: {
      appimage_sha256: sha256(appImageRaw),
      minisign_binary: { sha256: "30".repeat(32), size: 1 },
      minisign_package: { architecture: "amd64", version: "0.11-1" },
      public_key_sha256: "31".repeat(32),
      schema: "arc.packaged-appimage-updater-signature.v1",
      signature_sha256: sha256(appImageSigRaw),
      stderr_sha256: "32".repeat(32),
      stdout_sha256: "33".repeat(32),
      updater_signature_verified: true,
    },
  };
  overrides.mutateAppImage?.(appImageHostReceipt);
  await writeFile(appImageReceiptPath, canonicalJson(appImageHostReceipt), { mode: 0o400 });
  await chmod(appImageReceiptPath, 0o400);
  const npmVersion = JSON.parse(npmPackageRaw.toString("utf8")).version;
  return {
    args: {
      appArchive,
      appArchiveSignature,
      appImageReceipt: appImageReceiptPath,
      dmg,
      expectedFrontendConfigSha256: sha256(configRaw),
      expectedNodeArchiveSha256: sha256(archiveRaw),
      expectedNodeExecutableSha256: sha256(nodeRaw),
      expectedNpmCliSha256: sha256(npmCliRaw),
      expectedNpmPackageSha256: sha256(npmPackageRaw),
      expectedSshExecutableSha256: sha256(sshExecutableRaw),
      expectedSshKnownHostsSha256: sha256(knownHostsRaw),
      fetchImpl: mockFetch({
        block: overrides.block ?? blockReceipt(),
        direct: overrides.direct ?? directReceipt(),
        earnings: overrides.earnings ?? earnings(),
      }),
      frontendConfig: configPath,
      nodeArchive,
      nodeExecutable,
      npmCli,
      npmPackage,
      macosControllerAttempt: macosControllerAttemptPath,
      macosPackageInspection: macosPackageInspectionPath,
      macosPackageProvenance: macosPackageProvenancePath,
      macosPackageProvenanceVerification: macosPackageProvenanceVerificationPath,
      macosUpdaterSignatureReceipt: macosUpdaterSignatureReceiptPath,
      nativeAttempt: nativeAttemptPath,
      nativeHome,
      nativeInput: nativeInputPath,
      nativeReceipt: nativeReceiptPath,
      output,
      playwrightReport: reportPath,
      port: 9090,
      repositoryRoot: ROOT,
      rewardTx: TX,
      rolloutManifestSha256: ROLLOUT,
      runtimeContract: {
        nodeArch: process.arch,
        nodePlatform: process.platform,
        nodeVersion: process.version,
        npmVersion,
      },
      sshExecutable,
      sshIdentitySha256: "10".repeat(32),
      sshKnownHosts: knownHosts,
      sourceCommit,
      validatorHost: "140.82.16.112",
      validatorName: "lax",
      validatorRpcSocket: `/run/arc-v3-rpc-lax-${ROLLOUT.slice(0, 16)}/rpc.sock`,
      worker: WORKER,
    },
    output,
  };
}

test("accepts only an exact clean Playwright live-suite pass", () => {
  assert.equal(validatePlaywrightReport(report(), packageLock).testCount, 4);
  assert.throws(
    () => validatePlaywrightReport(report("skipped"), packageLock),
    /did not pass|skipped, flaky, or unexpected|summary/,
  );
  assert.throws(
    () => validatePlaywrightReport(report("unexpected", "failed"), packageLock),
    /did not pass|skipped, flaky, or unexpected|summary/,
  );
  const retried = report();
  retried.suites[0].specs[0].tests[0].results.push({ status: "passed" });
  assert.throws(
    () => validatePlaywrightReport(retried, packageLock),
    /did not run exactly once/,
  );
});

test("reviewed short-input BLAKE3 verifier matches canonical vectors", () => {
  assert.equal(
    blake3Short(Buffer.alloc(0)),
    "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
  );
  assert.equal(
    blake3Short(Buffer.from("abc")),
    "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85",
  );
  assert.equal(
    blake3Short(Buffer.alloc(65)),
    "a6f791da7707e3a05a7742248eefe43f9ad4626fc21b63675367c3d1d69ec91c",
  );
  assert.throws(() => blake3Short(Buffer.alloc(1_025)), /at most one chunk/);
});

test("writes a canonical create-only receipt bound to exact live values", async () => {
  const { args, output } = await fixture();
  const result = await buildDesktopLiveProductReceipt(args);
  const raw = await readFile(output);
  const value = JSON.parse(raw);
  assert.equal(value.schema, DESKTOP_LIVE_SCHEMA);
  assert.equal(value.rewardTx, TX);
  assert.equal(value.worker, WORKER);
  assert.equal(value.rpcPort, 9090);
  assert.equal(value.forward.validatorName, "lax");
  assert.equal(value.forward.rolloutManifestSha256, ROLLOUT);
  assert.equal(value.runtime.nodeVersion, process.version);
  assert.equal(value.rewardReceipt.job_id, JOB);
  assert.equal(value.earnings.canaryReceipt.job_id, JOB);
  assert.equal(value.blockReceipt.txHash, TX);
  assert.deepEqual(value.pollContract, { maxPolls: 61, maxWaitMs: 180_000 });
  assert.equal(raw.toString("utf8"), canonicalJson(value));
  assert.match(result.sha256, /^[0-9a-f]{64}$/);
  await assert.rejects(() => buildDesktopLiveProductReceipt(args), /EEXIST/);
});

test("fails closed before output on malformed or mismatched direct receipt evidence", async () => {
  const mutations = [
    ["missing required key", (value) => { delete value.evidence_source; }],
    ["wrong transaction", (value) => { value.tx_hash = `0x${"97".repeat(32)}`; }],
    ["wrong job", (value) => { value.job_id = `0x${"96".repeat(32)}`; }],
    ["wrong worker", (value) => { value.worker = `0x${"98".repeat(32)}`; }],
    ["failed status", (value) => { value.status = "mined_failed"; }],
    ["wrong reward", (value) => { value.reward_base = 1; }],
    ["malformed confirmation", (value) => { value.confirmed = "true"; }],
  ];
  for (const [name, mutate] of mutations) {
    const direct = directReceipt();
    mutate(direct);
    const invalid = await fixture({ direct });
    await assert.rejects(
      () => buildDesktopLiveProductReceipt(invalid.args),
      undefined,
      name,
    );
    await assert.rejects(() => readFile(invalid.output), /ENOENT/, name);
  }
});

test("fails closed before output on cross-block evidence", async () => {
  const wrongBlock = await fixture({
    block: blockReceipt({ block_height: 137_151 }),
  });
  await assert.rejects(
    () => buildDesktopLiveProductReceipt(wrongBlock.args),
    /height differs/,
  );
  await assert.rejects(() => readFile(wrongBlock.output), /ENOENT/);
});

test("fails closed on malformed raw block-receipt shape", async () => {
  const mutations = [
    ["missing field", (value) => { delete value.logs; }],
    ["extra field", (value) => { value.reward_arc = 2.5; }],
    ["value commitment", (value) => { value.value_commitment = "00".repeat(32); }],
    ["inline proof", (value) => { value.inclusion_proof = []; }],
    ["execution logs", (value) => { value.logs = [{}]; }],
  ];
  for (const [name, mutate] of mutations) {
    const block = blockReceipt();
    mutate(block);
    const invalid = await fixture({ block });
    await assert.rejects(
      () => buildDesktopLiveProductReceipt(invalid.args),
      undefined,
      name,
    );
    await assert.rejects(() => readFile(invalid.output), /ENOENT/, name);
  }
});

test("fails closed when projection value and reason are both absent", async () => {
  const invalid = await fixture({
    earnings: earnings({
      projected_daily_arc: null,
      projected_daily_unavailable_reason: null,
    }),
  });
  await assert.rejects(
    () => buildDesktopLiveProductReceipt(invalid.args),
    /projection lacks an exact value\/reason XOR/,
  );
  await assert.rejects(() => readFile(invalid.output), /ENOENT/);
});

test("fails closed on packaged-native route, worker, projection, network, and tx mutations", async () => {
  const mutations = [
    ["route check", (value) => { value.negativeRouteChecks.origin = false; }],
    ["worker", (value) => { value.worker = `0x${"ee".repeat(32)}`; }],
    ["projection", (value) => { value.projection.projectedDailyUnavailableReason = null; }],
    ["network source", (value) => { value.network.sourceHost = "https://149.28.32.76"; }],
    ["validator duplicate", (value) => { value.network.validators[1].address = value.network.validators[0].address; }],
    ["transaction", (value) => { value.transaction.blockHeight += 1; }],
    ["dispatch count", (value) => { value.dispatch.count = 2; }],
    ["runtime webview", (value) => { value.runtime.webviewsCreated = 1; }],
    ["runtime IPC", (value) => { value.runtime.ipcHandlersRegistered = true; }],
    ["runtime plugin", (value) => { value.runtime.pluginsLoaded = true; }],
    ["runtime builder", (value) => { value.runtime.tauriBuilderStarted = true; }],
    ["runtime environment names", (value) => { value.runtime.environmentNames.push("HTTPS_PROXY"); }],
    ["runtime environment values", (value) => { value.runtime.environmentSha256 = "fe".repeat(32); }],
    ["result input", (value) => { value.dispatch.result.input += " mutated"; }],
    ["result model", (value) => { value.dispatch.result.modelHash = `0x${"ee".repeat(32)}`; }],
    ["result output", (value) => { value.dispatch.result.outputHash = `0x${"ed".repeat(32)}`; }],
    ["result output bytes", (value) => { value.dispatch.result.output += " mutated"; }],
    ["provisional tx", (value) => { value.dispatch.result.settlement.txHash = `0x${"ec".repeat(32)}`; }],
    ["provisional job", (value) => { value.dispatch.result.settlement.jobId = `0x${"eb".repeat(32)}`; }],
    ["provisional worker", (value) => { value.dispatch.result.settlement.worker = `0x${"ea".repeat(32)}`; }],
    ["provisional URL", (value) => { value.dispatch.result.settlement.receiptUrl = "/community/reward_receipt/wrong"; }],
    ["provisional type", (value) => { value.dispatch.result.settlement.txType = "0x16"; }],
    ["provisional submitted", (value) => { value.dispatch.result.settlement.submitted = false; }],
    ["provisional pending model", (value) => { value.dispatch.result.settlement.modelId = MODEL; }],
    ["provisional pending input", (value) => { value.dispatch.result.settlement.inputHash = INPUT; }],
    ["provisional pending output", (value) => { value.dispatch.result.settlement.outputHash = OUTPUT; }],
    ["provisional assignment", (value) => { value.dispatch.result.settlement.assignmentEpoch = `0x${"e7".repeat(32)}`; }],
    ["provisional missing assignment", (value) => { value.dispatch.result.settlement.assignmentEpoch = ""; }],
    ["provisional domain", (value) => { value.dispatch.result.settlement.transactionDomain = `0x${"e6".repeat(32)}`; }],
    ["provisional recovery", (value) => { value.dispatch.result.settlement.recoveryEpoch = 2; }],
    ["provisional validator set", (value) => { value.dispatch.result.settlement.validatorSetId = 2; }],
    ["provisional validator commitment", (value) => { value.dispatch.result.settlement.validatorSetCommitment = `0x${"e5".repeat(32)}`; }],
    ["provisional approvals", (value) => { value.dispatch.result.settlement.validatorApprovals = 6; }],
    ["provisional evidence", (value) => { value.dispatch.result.settlement.evidenceSource = "wrong"; }],
    ["terminal input", (value) => { value.receiptPoll.receipt.inputHash = `0x${"e9".repeat(32)}`; }],
    ["terminal worker", (value) => { value.receiptPoll.receipt.worker = `0x${"e8".repeat(32)}`; }],
    ["terminal URL", (value) => { value.receiptPoll.receipt.receiptUrl = "/community/reward_receipt/wrong"; }],
    ["terminal type", (value) => { value.receiptPoll.receipt.txType = "0x16"; }],
    ["terminal submitted", (value) => { value.receiptPoll.receipt.submitted = false; }],
  ];
  for (const [name, mutateNative] of mutations) {
    const invalid = await fixture({ mutateNative });
    await assert.rejects(
      () => buildDesktopLiveProductReceipt(invalid.args),
      undefined,
      name,
    );
    await assert.rejects(() => readFile(invalid.output), /ENOENT/, name);
  }
});

test("fails closed on macOS package provenance, signature, mount, and no-retry mutations", async () => {
  const mutations = [
    ["Developer ID overclaim", ({ provenance }) => { provenance.truthScope.appleDeveloperIdSigned = true; }],
    ["writable DMG", ({ provenance }) => { provenance.dmgExecution.attach.writeable = true; }],
    ["changed code identity", ({ provenance }) => { provenance.codeSignature.after.identifier = "attacker.bundle"; }],
    ["signature failure", ({ signature }) => { signature.verified = false; }],
    ["recovery-enclave access", ({ signature }) => { signature.disposableVm.recoveryEnclaveAccessed = true; }],
    ["missing extraction", ({ inspection }) => { delete inspection.extraction; }],
    ["unsafe extraction", ({ inspection }) => { inspection.extraction.safety.symlinksResolvedInsideBundle = false; }],
    ["wrong inspection mount", ({ inspection }) => { inspection.dmg.attach.mountPointBasename = "native-dmg-mount"; }],
    ["missing mount tools", ({ inspection }) => { inspection.dmg.attach.tools = {}; }],
    ["bad root inventory", ({ inspection }) => { inspection.dmg.rootInventory = []; }],
    ["incomplete detach", ({ inspection }) => { delete inspection.dmg.detach.postDetachInfoPlistSha256; }],
    ["changed inspected DMG", ({ inspection }) => { inspection.dmg.sha256AfterDetach = "ff".repeat(32); }],
    ["retryable controller", ({ controllerAttempt }) => { controllerAttempt.retryPermitted = true; }],
    ["unverified final provenance", ({ verification }) => { verification.verified = false; }],
  ];
  for (const [name, mutateMacosEvidence] of mutations) {
    const invalid = await fixture({ mutateMacosEvidence });
    await assert.rejects(
      () => buildDesktopLiveProductReceipt(invalid.args),
      undefined,
      name,
    );
    await assert.rejects(() => readFile(invalid.output), /ENOENT/, name);
  }
});

test("fails closed on packaged AppImage source, release, isolation, transport, and signature mutations", async () => {
  const mutations = [
    ["schema", (value) => { value.schema = "arc.packaged-appimage-live-host.v0"; }],
    ["result", (value) => { value.result = "failed"; }],
    ["source", (value) => { value.source.gate_sha256 = "ff".repeat(32); }],
    ["commit", (value) => { value.release.commit = "cd".repeat(20); }],
    ["VM deletion", (value) => { value.disposable_vm.deleted_after_evidence_copy = false; }],
    ["recovery enclave", (value) => { value.disposable_vm.recovery_enclave_accessed = true; }],
    ["extra mount", (value) => { value.disposable_vm.mounts = ["/Users"]; }],
    ["attempt state", (value) => { value.inference_attempt.state = "unarmed"; }],
    ["guest schema", (value) => { value.guest_receipt.schema = "wrong"; }],
    ["transport", (value) => { value.transport.relay_target = "149.28.32.76:443"; }],
    ["signature", (value) => { value.updater_signature.updater_signature_verified = false; }],
    ["asset signature", (value) => { value.updater_signature.signature_sha256 = "ee".repeat(32); }],
    ["duplicate asset IDs", (value) => {
      value.release.assets["arc-node-linux-x86_64"].id = value.release.assets["arc-desktop-linux-x86_64.AppImage"].id;
    }],
  ];
  for (const [name, mutateAppImage] of mutations) {
    const invalid = await fixture({ mutateAppImage });
    await assert.rejects(
      () => buildDesktopLiveProductReceipt(invalid.args),
      undefined,
      name,
    );
    await assert.rejects(() => readFile(invalid.output), /ENOENT/, name);
  }
});
