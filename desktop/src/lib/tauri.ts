// Thin Tauri IPC wrapper with a mock-mode fallback used in the browser (dev + Playwright).
// Production: invoke the real Tauri command. Mock: return synthetic data so every screen
// is visible without a running node.

import type {
  AccountBalance,
  Attestation,
  BinaryStatus,
  BlockTxs,
  Earnings,
  EarningsProjection,
  FaucetResult,
  HardwareInfo,
  Identity,
  InferenceResult,
  LogEntry,
  ModelTierInfo,
  NetworkOverview,
  NetworkStats,
  NodeConfig,
  NodeContribution,
  NodeStatus,
  PaidInferenceResult,
  RecentBlocks,
  ResetPeerStateResult,
  RewardEconomics,
  SavedLogs,
  ThreadsApplied,
  Tier1Result,
  Tier1Submitted,
  Tier1Vote,
  TxLookup,
  UpdateInstallPolicy,
  WalletTxResult,
} from "./types";

const IS_TAURI =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

// Set by vite.config.ts: true only inside the production Tauri bundle. A
// production build served off a web host has this true but no Tauri
// internals - in that case mockInvoke() refuses to run to prevent a user
// from opening DevTools and fabricating node state.
declare const __ARC_PROD_TAURI__: boolean;
const IS_PROD_TAURI_BUNDLE =
  typeof __ARC_PROD_TAURI__ !== "undefined" && __ARC_PROD_TAURI__;

// Live mode - used by E2E tests against a real running arc-node (typically on
// 127.0.0.1:9090 from the community installer). When `window.__ARC_LIVE__` is
// truthy, the browser bypasses the mock layer and calls the node's HTTP RPC
// directly, adapting the shapes the same way src-tauri/src/rpc_client.rs does.
function liveBase(): string | null {
  if (typeof window === "undefined") return null;
  const port = (window as Window & { __ARC_LIVE__?: number | string }).__ARC_LIVE__;
  if (!port) return null;
  return `http://127.0.0.1:${port}`;
}

/** Browser-preview fixture amount for a successful mined 0x25 receipt. */
const MOCK_REWARD_PER_RECEIPT = 2.5;

const SETTLEMENT_WRITE_UNAVAILABLE =
  "is unavailable before any transaction is signed or submitted: exact model-artifact binding, validator-authenticated authorization, and settlement are not ready on the selected path. VRF selection and server-derived replica labels are not validator approval. Unpaid inference remains available, but running it does not earn ARC.";

function settlementWriteUnavailable(flow: string): Error {
  return new Error(`${flow} ${SETTLEMENT_WRITE_UNAVAILABLE}`);
}

function emptyEarnings(): Earnings {
  return {
    totalArc: 0,
    todayArc: null,
    pendingArc: null,
    rank: null,
    attestations: 0,
    lastPayoutAt: null,
    lastPayoutBlock: null,
    confirmedReceipts: [],
    projectedDailyArc: null,
    projectedDailyUnavailableReason:
      "confirmed mined reward receipts are unavailable",
    recoveryEpoch: null,
    validatorSetId: null,
    fromChain: false,
  };
}

/**
 * Parse only the candidate's mined-0x25 receipt/readiness contract.
 * Public v2 returns HTTP 200 for this path too, but its body is raw-0x16
 * count × constant display arithmetic. HTTP status is not a semantics gate.
 */
function confirmedEarningsFromBody(body: unknown): Earnings | null {
  if (!body || typeof body !== "object") return null;
  const o = body as Record<string, unknown>;
  const totalRewards = o.total_rewards;
  const totalArc = o.confirmed_gross_earnings_arc;
  const confirmedCount = o.confirmed_receipt_count;
  const confirmedBase = o.confirmed_gross_earnings_base;
  const rows = o.confirmed_receipts;
  const note = o.estimated_total_arc_note;
  const effective = o.community_rewards_v1_enabled;
  const protocolActive = o.community_rewards_v1_protocol_active;
  const approvalReady = o.community_rewards_v1_approval_collection_ready;
  if (
    typeof totalRewards !== "number" ||
    !Number.isSafeInteger(totalRewards) ||
    totalRewards < 0 ||
    typeof totalArc !== "number" ||
    !Number.isFinite(totalArc) ||
    totalArc < 0 ||
    o.today_arc !== null ||
    confirmedCount !== totalRewards ||
    !Number.isSafeInteger(confirmedBase) ||
    (confirmedBase as number) < 0 ||
    !Array.isArray(rows) ||
    rows.length !== totalRewards ||
    typeof note !== "string" ||
    !note.includes("CommunityInferenceReward") ||
    typeof effective !== "boolean" ||
    typeof protocolActive !== "boolean" ||
    typeof approvalReady !== "boolean"
  ) {
    return null;
  }
  if (effective && (!protocolActive || !approvalReady)) return null;
  let receiptBaseSum = 0;
  let receiptArcSum = 0;
  const confirmedReceipts = [] as Earnings["confirmedReceipts"];
  for (const value of rows as unknown[]) {
    if (!value || typeof value !== "object") return null;
    const receipt = value as Record<string, unknown>;
    if (
      receipt.tx_type !== "0x25" ||
      receipt.success !== true ||
      typeof receipt.tx_hash !== "string" ||
      typeof receipt.job_id !== "string" ||
      !Number.isSafeInteger(receipt.block_height) ||
      typeof receipt.block_hash !== "string" ||
      !Number.isSafeInteger(receipt.reward_base) ||
      (receipt.reward_base as number) < 0 ||
      typeof receipt.reward_arc !== "number" ||
      !Number.isFinite(receipt.reward_arc) ||
      receipt.reward_arc < 0
    ) {
      return null;
    }
    receiptBaseSum += receipt.reward_base as number;
    receiptArcSum += receipt.reward_arc;
    if (!Number.isSafeInteger(receiptBaseSum)) return null;
    confirmedReceipts.push({
      txHash: receipt.tx_hash,
      jobId: receipt.job_id,
      blockHeight: receipt.block_height as number,
      blockHash: receipt.block_hash,
      rewardBase: receipt.reward_base as number,
      rewardArc: receipt.reward_arc,
      recoveryEpoch:
        typeof receipt.recovery_epoch === "number"
          ? receipt.recovery_epoch
          : null,
      validatorSetId:
        typeof receipt.validator_set_id === "number"
          ? receipt.validator_set_id
          : null,
    });
  }
  if (receiptBaseSum !== confirmedBase) return null;
  if (!Number.isFinite(receiptArcSum) || Math.abs(receiptArcSum - totalArc) > 1e-9) {
    return null;
  }
  const projection = o.projected_daily_arc;
  if (
    projection !== null &&
    (typeof projection !== "number" ||
      !Number.isFinite(projection) ||
      projection < 0)
  ) return null;
  const projectionReason = o.projected_daily_unavailable_reason;
  if (
    projectionReason !== null &&
    (typeof projectionReason !== "string" || projectionReason.trim().length === 0)
  ) return null;
  const projectedDailyArc = projection as number | null;
  const projectedDailyUnavailableReason = projectionReason as string | null;
  if ((projectedDailyArc === null) === (projectedDailyUnavailableReason === null)) return null;
  if (
    !Object.prototype.hasOwnProperty.call(o, "last_reward_block") ||
    !Object.prototype.hasOwnProperty.call(o, "last_reward_tx_hash")
  ) {
    return null;
  }
  const lastBlock =
    typeof o.last_reward_block === "number" &&
    Number.isSafeInteger(o.last_reward_block) &&
    o.last_reward_block >= 0
      ? o.last_reward_block
      : null;
  const lastHash =
    typeof o.last_reward_tx_hash === "string" &&
    o.last_reward_tx_hash.trim().length > 0
      ? o.last_reward_tx_hash
      : null;
  if (totalRewards > 0 && (lastBlock === null || lastHash === null)) return null;

  return {
    totalArc,
    todayArc: null,
    pendingArc: null,
    rank: null,
    attestations: totalRewards,
    lastPayoutAt:
      typeof o.last_reward_at === "number" &&
      Number.isFinite(o.last_reward_at)
        ? o.last_reward_at
        : null,
    lastPayoutBlock: lastBlock,
    confirmedReceipts,
    projectedDailyArc,
    projectedDailyUnavailableReason,
    recoveryEpoch:
      typeof o.recovery_epoch === "number" ? o.recovery_epoch : null,
    validatorSetId:
      typeof o.validator_set_id === "number" ? o.validator_set_id : null,
    fromChain: true,
  };
}

/** Mirrors commands.rs::COORDINATOR_HOSTS, NYC included. */
const COORDINATOR_HOSTS = [
  "https://149-28-32-76.nip.io", // NYC
  "https://140-82-16-112.nip.io", // LAX
  "https://136-244-109-1.nip.io", // AMS
  "https://104-238-171-11.nip.io", // LHR
  "https://202-182-107-41.nip.io", // NRT
  "https://149-28-153-31.nip.io", // SGP
];

/**
 * Per-command mock overrides, for tests only.
 *
 * The endpoints behind the projection and Network screens are newer than the
 * deployed seeds, so their real-world behaviour today is a 404 that degrades
 * to a stated reason. Both the populated path and each degraded path have to
 * be exercisable, and a test cannot reach into a Rust process to make a seed
 * 404 on demand.
 *
 * This seam lives inside `mockInvoke` only. It is unreachable in the Tauri app
 * (which never calls the mock) and unreachable from a production bundle opened
 * outside Tauri (which refuses to mock at all — see the guard in
 * `mockInvoke`). Setting it cannot make the real app show a fabricated number.
 */
type MockOverrides = Record<string, unknown>;
function mockOverride<T>(cmd: string): T | undefined {
  if (typeof window === "undefined") return undefined;
  const o = (window as Window & { __ARC_MOCK__?: MockOverrides }).__ARC_MOCK__;
  if (!o || !(cmd in o)) return undefined;
  return o[cmd] as T;
}

/** Strip an optional `0x` and lowercase. Mirrors rpc_client.rs::strip_0x. */
function strip0x(s: string): string {
  return s.trim().replace(/^0[xX]/, "").toLowerCase();
}

/** ARC base units per whole ARC. Mirrors rpc_client.rs::ARC_BASE_UNITS. */
const ARC_BASE_UNITS = 1_000_000_000;

function exactBaseUnits(value: unknown, field: string): string {
  if (typeof value === "string" && /^\d+$/.test(value)) return value;
  if (
    typeof value === "number" &&
    Number.isSafeInteger(value) &&
    value >= 0
  ) {
    return String(value);
  }
  throw new Error(`${field} is not an exact base-unit integer`);
}

function arcFromBaseUnits(value: string): string {
  const padded = value.padStart(10, "0");
  const whole = padded.slice(0, -9).replace(/^0+(?=\d)/, "");
  const fraction = padded.slice(-9).replace(/0+$/, "");
  return fraction ? `${whole}.${fraction}` : whole;
}

async function liveInvoke<T>(cmd: string, args?: unknown): Promise<T> {
  const base = liveBase()!;
  const fetchJson = async (path: string) => {
    const r = await fetch(`${base}${path}`);
    if (!r.ok) throw new Error(`${path} → ${r.status}`);
    return r.json();
  };

  // Mirrors rpc_client.rs::get_detailed — keeps "this host has no such
  // endpoint" (404) apart from "this host is unreachable", because the two
  // degrade to different sentences in the UI.
  type Detailed =
    | { kind: "ok"; body: Record<string, unknown> }
    | { kind: "notFound" }
    | { kind: "badRequest" }
    | { kind: "status"; code: number }
    | { kind: "unreachable"; error: string }
    | { kind: "unparseable" };
  const getDetailed = async (path: string): Promise<Detailed> => {
    try {
      const r = await fetch(`${base}${path}`);
      if (r.ok) {
        try {
          return { kind: "ok", body: await r.json() };
        } catch {
          return { kind: "unparseable" };
        }
      }
      if (r.status === 404) return { kind: "notFound" };
      if (r.status === 400) return { kind: "badRequest" };
      return { kind: "status", code: r.status };
    } catch (e) {
      return { kind: "unreachable", error: String(e) };
    }
  };
  const reason = (path: string, d: Detailed): string => {
    switch (d.kind) {
      case "notFound":
        return `${base} does not serve ${path} (HTTP 404).`;
      case "badRequest":
        return `${base} rejected ${path} as malformed (HTTP 400).`;
      case "status":
        return `${base} answered ${path} with HTTP ${d.code}.`;
      case "unreachable":
        return `Could not reach ${base} — ${d.error}`;
      default:
        return `${base} answered ${path} with a response this build could not parse.`;
    }
  };
  const numOf = (
    o: Record<string, unknown>,
    keys: string[],
  ): number | null => {
    for (const k of keys) {
      const v = o[k];
      if (typeof v === "number" && Number.isFinite(v)) return v;
    }
    return null;
  };
  // `/node/contribution` and `/network/info` nest their figures, so a flat
  // lookup for "threads" returns the OBJECT and reads as absent.
  const at = (o: unknown, path: string): unknown => {
    let cur: unknown = o;
    for (const seg of path.split(".")) {
      if (cur === null || typeof cur !== "object") return undefined;
      cur = (cur as Record<string, unknown>)[seg];
    }
    return cur;
  };
  const nNum = (o: unknown, path: string): number | null => {
    const v = at(o, path);
    return typeof v === "number" && Number.isFinite(v) ? v : null;
  };
  const nStr = (o: unknown, path: string): string | null => {
    const v = at(o, path);
    return typeof v === "string" && v.length > 0 ? v : null;
  };
  const nBool = (o: unknown, path: string): boolean | null => {
    const v = at(o, path);
    return typeof v === "boolean" ? v : null;
  };
  const strOf = (
    o: Record<string, unknown>,
    keys: string[],
  ): string | null => {
    for (const k of keys) {
      const v = o[k];
      if (typeof v === "string" && v.length > 0) return v;
    }
    return null;
  };

  switch (cmd) {
    case "detect_hardware":
      return {
        platform: "macOS",
        arch: "arm64",
        cpuModel: "Apple Silicon",
        cpuCores: navigator.hardwareConcurrency ?? 8,
        ramGb: (navigator as Navigator & { deviceMemory?: number }).deviceMemory
          ? (navigator as Navigator & { deviceMemory?: number }).deviceMemory! * 8
          : 16,
        gpuName: "Apple GPU",
        gpuVramGb: 16,
        recommendedModel: "Llama-2-7B Q4_K_M (3.8 GB)",
        recommendedRole: "worker",
      } as T;
    case "generate_identity":
      return {
        address:
          "arc1q" +
          [...crypto.getRandomValues(new Uint8Array(20))]
            .map((b) => b.toString(16).padStart(2, "0"))
            .join(""),
        publicKey:
          "0x" +
          [...crypto.getRandomValues(new Uint8Array(32))]
            .map((b) => b.toString(16).padStart(2, "0"))
            .join(""),
        createdAt: Date.now(),
      } as T;
    case "reveal_seed_phrase":
      return "galaxy stellar quantum horizon crystal ember aurora silent mirror ocean celestial fragment" as T;
    case "load_identity":
      return null as T;
    case "save_config":
      return undefined as T;
    case "load_config":
      return null as T;
    case "node_status": {
      // Mirror desktop/src-tauri/src/rpc_client.rs::fetch_status:
      // if local /health is missing peers, probe the public seed
      // coordinators in order. First 200 → set coordinatorUrl + flip
      // health to "lite". This is what unlocks the v0.7.0 Client-mode
      // banner in the dashboard. Pre-v0.7 the JS mock hardcoded
      // coordinatorUrl: null so the banner was untestable in live mode.
      // Probed concurrently, matching rpc_client.rs::probe_coordinator.
      const probeCoordinator = async (): Promise<string | null> => {
        const attempts = COORDINATOR_HOSTS.map(async (origin) => {
          const r = await fetch(`${origin}/health`, {
            method: "GET",
            signal: AbortSignal.timeout(2000),
          });
          if (!r.ok) throw new Error(`${origin} → ${r.status}`);
          return origin;
        });
        try {
          return await Promise.any(attempts);
        } catch {
          return null;
        }
      };

      const rpcPort = Number(
        (window as Window & { __ARC_LIVE__?: number }).__ARC_LIVE__,
      );
      try {
        const h = await fetchJson("/health");
        const peers = h.peers ?? 0;
        const uptime = h.uptime_secs ?? 0;
        const coordinatorUrl =
          peers === 0 ? await probeCoordinator() : null;
        const health =
          peers >= 1 && uptime >= 8
            ? "live"
            : coordinatorUrl
              ? "lite"
              : "syncing";
        return {
          running: true,
          pid: null,
          health,
          version: h.version ?? "unknown",
          peers,
          round: h.dag_round ?? 0,
          committed: h.dag_committed ?? 0,
          height: h.height ?? 0,
          uptimeSeconds: uptime,
          address: null,
          rpcPort,
          lastError: null,
          coordinatorUrl,
          chainHost: coordinatorUrl,
          chainHeight: null,
          chainRound: null,
          chainBlockAgeSeconds: null,
          workerThreads: null,
          cpuCores: navigator.hardwareConcurrency ?? null,
        } as T;
      } catch {
        const coordinatorUrl = await probeCoordinator();
        return {
          running: false,
          pid: null,
          health: coordinatorUrl ? "lite" : "offline",
          version: "unknown",
          peers: 0,
          round: 0,
          committed: 0,
          height: 0,
          uptimeSeconds: 0,
          address: null,
          rpcPort,
          lastError: coordinatorUrl ? null : "No response",
          coordinatorUrl,
          chainHost: coordinatorUrl,
          chainHeight: null,
          chainRound: null,
          chainBlockAgeSeconds: null,
          workerThreads: null,
          cpuCores: navigator.hardwareConcurrency ?? null,
        } as T;
      }
    }
    case "fetch_earnings": {
      const stored = localStorage.getItem("arc-desktop-state-v1");
      const addr = stored
        ? (JSON.parse(stored).identity?.address as string | undefined)
        : undefined;
      if (!addr) return emptyEarnings() as T;
      const path = `/worker/earnings/${strip0x(addr)}`;
      const response = await getDetailed(path);
      if (response.kind !== "ok") return emptyEarnings() as T;
      return (confirmedEarningsFromBody(response.body) ?? emptyEarnings()) as T;
    }
    case "fetch_attestations": {
      // Shape-tolerant, matching rpc_client.rs::fetch_attestations. The live
      // seeds return flat tx records with no nested `inference` object, so
      // reading only the nested shape produced blank rows with "0 tokens",
      // "0ms" and a hardcoded "+2.50".
      try {
        const limit = (args as { limit?: number } | undefined)?.limit ?? 20;
        const r = await fetchJson(`/inference/attestations?limit=${limit}`);
        type Raw = Record<string, unknown>;
        const arr = (r.attestations ?? []) as Raw[];
        const stored = localStorage.getItem("arc-desktop-state-v1");
        const mineAddr = stored
          ? (JSON.parse(stored).identity?.address as string | undefined)
              ?.replace(/^0x/, "")
              .toLowerCase()
          : undefined;

        const num = (o: Raw, k: string): number | null => {
          const v = o[k];
          return typeof v === "number" && v > 0 ? v : null;
        };

        return arr
          .map((v) => {
            const inf = (v.inference as Raw | undefined) ?? v;
            const txHash = (v.tx_hash as string) ?? "";
            if (!txHash) return null;
            const tokens = num(inf, "tokens_generated");
            const msPerTok = num(inf, "ms_per_token");
            const from = ((v.from as string) ?? "")
              .replace(/^0x/, "")
              .toLowerCase();
            const mine = !!mineAddr && !!from && from === mineAddr;
            return {
              txHash,
              inputPreview: ((inf.input as string) ?? "")
                .replace("[INST] ", "")
                .replace(" [/INST]", "")
                .slice(0, 140),
              outputHash: (inf.output_hash as string) ?? "",
              modelHash: (inf.model_hash as string) ?? "",
              tokens,
              latencyMs:
                tokens !== null && msPerTok !== null
                  ? tokens * msPerTok
                  : num(inf, "inference_ms"),
              timestamp: num(v, "timestamp"),
              blockHeight: num(v, "block_height"),
              txType: (v.tx_type as string) ?? null,
              from: from || null,
              mine,
              verified: !!v.success,
            };
          })
          .filter((x): x is NonNullable<typeof x> => x !== null)
          .sort((a, b) => (b.blockHeight ?? 0) - (a.blockHeight ?? 0)) as T;
      } catch {
        return [] as T;
      }
    }
    case "fetch_logs":
      return [] as T;
    case "fetch_network_stats": {
      try {
        const [h, r] = await Promise.all([
          fetchJson("/health"),
          fetchJson("/inference/results?limit=1"),
        ]);
        const uptime = Math.max(1, h.uptime_secs ?? 1);
        return {
          totalNodes: Math.max(1, h.validators ?? 0),
          totalInferences: r.count ?? 0,
          avgTps: Math.floor(((h.dag_round ?? 0) * 4) / uptime),
          latestBlock: h.dag_committed ?? 0,
        } as T;
      } catch {
        return {
          totalNodes: 0,
          totalInferences: 0,
          avgTps: 0,
          latestBlock: 0,
        } as T;
      }
    }
    case "start_node":
    case "stop_node":
    case "restart_node":
      return undefined as T;
    case "reset_peer_state":
      return {
        removedPath: "/mock/.arc/data/known_peers.json",
        wasPresent: true,
        message: "Cleared cached peer list. Rebootstrapping from testnet seeds.",
      } as T;
    case "fetch_balance": {
      // Identity address is stored in localStorage zustand under `arc-desktop-state-v1`.
      const stored = localStorage.getItem("arc-desktop-state-v1");
      const addr = stored
        ? (JSON.parse(stored).identity?.address as string | undefined)
        : undefined;
      if (!addr) {
        return {
          address: "",
          balanceBase: "0",
          balanceArc: "0",
          nonce: 0,
          stakedBalanceBase: "0",
          stakedBalanceArc: "0",
        } as T;
      }
      try {
        const r = await fetch(`${base}/account/${addr}`);
        if (r.status === 404) {
          return {
            address: addr,
            balanceBase: "0",
            balanceArc: "0",
            nonce: 0,
            stakedBalanceBase: "0",
            stakedBalanceArc: "0",
          } as T;
        }
        const v = await r.json();
        const balanceBase = exactBaseUnits(v.balance, "balance");
        const stakedBalanceBase = exactBaseUnits(
          v.staked_balance,
          "staked_balance",
        );
        return {
          address: v.address ?? addr,
          balanceBase,
          balanceArc: arcFromBaseUnits(balanceBase),
          nonce: v.nonce ?? 0,
          stakedBalanceBase,
          stakedBalanceArc: arcFromBaseUnits(stakedBalanceBase),
        } as T;
      } catch (error) {
        throw new Error(
          `could not read an exact wallet balance: ${error instanceof Error ? error.message : String(error)}`,
        );
      }
    }
    case "send_arc":
      // Browser-live mode intentionally has no signing material. Never add a
      // seed argument here: native Rust signing is the security boundary.
      throw new Error(
        "Sending ARC requires the native desktop app so signing stays in Rust.",
      );
    case "faucet_claim": {
      const stored = localStorage.getItem("arc-desktop-state-v1");
      const addr = stored
        ? (JSON.parse(stored).identity?.address as string | undefined)
        : undefined;
      if (!addr) throw new Error("no identity");
      const r = await fetch(`${base}/faucet/claim`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ address: addr }),
      });
      const v = await r.json();
      if (!r.ok)
        throw new Error(v.error ?? `faucet error ${r.status}`);
      if (v.status !== "pending") {
        throw new Error("faucet did not acknowledge the claim as pending");
      }
      const txHash = strip0x(String(v.tx_hash ?? ""));
      if (!/^[0-9a-f]{64}$/.test(txHash)) {
        throw new Error("faucet returned no valid transaction hash");
      }
      const amountBase = exactBaseUnits(v.amount, "faucet amount");
      const receipt = await getDetailed(`/tx/${txHash}`);
      const receiptBody = receipt.kind === "ok" ? receipt.body : null;
      const mined = receiptBody !== null;
      const success =
        receiptBody && typeof receiptBody.success === "boolean"
          ? receiptBody.success
          : null;
      const receiptStatus = mined
        ? success === true
          ? "mined_success"
          : success === false
            ? "mined_failed"
            : "receipt_unavailable"
        : receipt.kind === "notFound"
          ? "pending"
          : "receipt_unavailable";
      return {
        txHash,
        amountBase,
        amountArc: arcFromBaseUnits(amountBase),
        receiptStatus,
        mined,
        success,
        blockHeight:
          receiptBody && typeof receiptBody.block_height === "number"
            ? receiptBody.block_height
            : null,
        blockHash:
          receiptBody && typeof receiptBody.block_hash === "string"
            ? strip0x(receiptBody.block_hash)
            : null,
        sourceHost: base,
        unavailable:
          receiptStatus === "receipt_unavailable"
            ? "the receipt lookup could not establish transaction success"
            : null,
        message:
          receiptStatus === "mined_success"
            ? "Faucet claim has a successful mined receipt."
            : receiptStatus === "mined_failed"
              ? "Faucet claim was mined but failed."
              : "Faucet claim was accepted and is waiting for a mined receipt.",
      } as T;
    }
    case "run_inference": {
      const { prompt, maxTokens, chatTemplate } = args as {
        prompt: string;
        maxTokens?: number;
        chatTemplate?: boolean;
      };
      const r = await fetch(`${base}/inference/run`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          input: prompt,
          max_tokens: maxTokens ?? 32,
          chat_template: chatTemplate ?? true,
        }),
      });
      if (!r.ok) throw new Error(`inference error ${r.status}`);
      const v = await r.json();
      return {
        input: v.inference?.input ?? "",
        output: v.inference?.output ?? "",
        outputHash: v.inference?.output_hash ?? "",
        modelHash: v.inference?.model_hash ?? "",
        tokensGenerated: v.inference?.tokens_generated ?? 0,
        inferenceMs: v.inference?.inference_ms ?? 0,
        txHash: v.attestation?.tx_hash ?? "",
        deterministic: v.inference?.deterministic ?? false,
        engine: v.inference?.engine ?? "",
        explorerUrl: v.explorer_url ?? "",
        // liveBase() is 127.0.0.1 - this IS the local node.
        servedLocally: true,
        trace: v.shard_trace ?? undefined,
      } as T;
    }
    case "run_inference_via_coordinator": {
      const { prompt, maxTokens, k, chatTemplate } = args as {
        prompt: string;
        maxTokens?: number;
        k?: number;
        chatTemplate?: boolean;
      };
      // Live mode iterates the same seed list the Rust side uses so the
      // browser E2E path exercises the coordinator fallback against a
      // real chain host.
      let lastErr = "";
      for (const host of COORDINATOR_HOSTS) {
        try {
          const r = await fetch(`${host}/inference/run_consensus`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({
              input: prompt,
              max_tokens: maxTokens ?? 32,
              k: k ?? 3,
              chat_template: chatTemplate ?? true,
            }),
          });
          if (!r.ok) {
            lastErr = `${host} → HTTP ${r.status}`;
            continue;
          }
          const v = await r.json();
          const c = v.consensus ?? {};
          return {
            input: v.input ?? "",
            output: v.output ?? "",
            outputHash: v.output_hash ?? "",
            modelHash: "",
            tokensGenerated: v.tokens_generated ?? 0,
            inferenceMs: v.total_ms ?? 0,
            txHash: "",
            deterministic: true,
            engine: "consensus",
            explorerUrl: "",
            consensus: {
              k: c.k ?? 0,
              votesTotal: c.votes_total ?? 0,
              unanimous: c.unanimous ?? 0,
              majority: c.majority ?? 0,
              split: c.split ?? 0,
              divergentReplicaCount: c.divergent_replicas
                ? Object.keys(c.divergent_replicas).length
                : 0,
            },
            coordinator: host,
          } as T;
        } catch (e) {
          lastErr = `${host} → ${String(e)}`;
        }
      }
      throw new Error(`all coordinators failed; last: ${lastErr}`);
    }
    case "run_inference_via_coordinator_direct": {
      const { prompt, maxTokens, chatTemplate } = args as {
        prompt: string;
        maxTokens?: number;
        chatTemplate?: boolean;
      };
      let lastErr = "";
      for (const host of COORDINATOR_HOSTS) {
        try {
          const r = await fetch(`${host}/inference/run`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({
              input: prompt,
              max_tokens: maxTokens ?? 32,
              chat_template: chatTemplate ?? true,
            }),
          });
          if (!r.ok) {
            lastErr = `${host} → HTTP ${r.status}`;
            continue;
          }
          const v = await r.json();
          const inf = v.inference ?? {};
          const att = v.attestation ?? {};
          return {
            input: inf.input ?? "",
            output: inf.output ?? "",
            outputHash: inf.output_hash ?? "",
            modelHash: inf.model_hash ?? "",
            tokensGenerated: inf.tokens_generated ?? 0,
            inferenceMs: inf.inference_ms ?? 0,
            txHash: att.tx_hash ?? "",
            deterministic: inf.deterministic ?? false,
            engine: inf.engine ?? "",
            explorerUrl: v.explorer_url ?? "",
            consensus: undefined,
            coordinator: host,
            servedLocally: false,
            trace: v.shard_trace ?? undefined,
          } as T;
        } catch (e) {
          lastErr = `${host} → ${String(e)}`;
        }
      }
      throw new Error(`all coordinators failed (direct path); last: ${lastErr}`);
    }
    case "run_paid_inference": {
      throw settlementWriteUnavailable("Paid inference escrow");
    }
    case "tier1_submit": {
      throw settlementWriteUnavailable("Tier 1 on-chain inference");
    }
    case "tier1_result": {
      const { requestId } = args as { requestId: string };
      const r = await fetch(
        `${base}/inference/onchain/result/${encodeURIComponent(requestId)}`,
      );
      if (!r.ok) throw new Error(`tier1_result → HTTP ${r.status}`);
      const v = await r.json();
      return {
        requestId: v.request_id,
        status: v.status,
        voteCount: v.vote_count,
        committeeSize: v.committee_size,
        anchorHeight: v.anchor_height,
        deadlineBlocks: v.deadline_blocks,
        votes: (v.votes ?? []).map((x: { voter: string; output_hash: string }) => ({
          voter: x.voter,
          outputHash: x.output_hash,
        })),
        outputHash: v.output_hash ?? null,
        outputBlob: v.output_blob ?? null,
        outputText: v.output_text ?? null,
        maxReward: v.max_reward,
      } as T;
    }
    // ── Chain visibility + projection ────────────────────────────────────
    // These mirror rpc_client.rs exactly. In live browser mode `base` is the
    // local node, which is also the chain host being read, so every number
    // below is attributable to one host — the same invariant the Rust side
    // holds (CLAUDE.md rule 4).
    case "fetch_reward_economics": {
      const path = "/economics/rewards";
      const d = await getDetailed(path);
      const shell = {
        sourceHost: base,
        rewardPerAttestation: null,
        treasuryBalanceArc: null,
        treasuryBalanceUnavailableReason: null,
        attestationsRemaining: null,
        attestationsRemainingUnavailableReason: null,
        treasuryIsFinite: null,
        bondPerAttestation: null,
        challengePeriodBlocks: null,
        bondRefundedAfterChallengePeriod: null,
        fundingDetail: null,
      };
      if (d.kind !== "ok") {
        return { ...shell, unavailable: reason(path, d) } as T;
      }
      // Prefer the exact `_base` integer and divide; the `_arc` floats are
      // produced by dividing by 1e9 and carry rounding.
      const baseOrArc = (baseKey: string, arcKey: string) => {
        const b = numOf(d.body, [baseKey]);
        if (b !== null) return b / ARC_BASE_UNITS;
        return numOf(d.body, [arcKey]);
      };
      return {
        ...shell,
        unavailable: null,
        rewardPerAttestation: baseOrArc(
          "reward_per_attestation_base",
          "reward_per_attestation_arc",
        ),
        treasuryBalanceArc: baseOrArc(
          "treasury_balance_base",
          "treasury_balance_arc",
        ),
        treasuryBalanceUnavailableReason: strOf(d.body, [
          "treasury_balance_unavailable_reason",
        ]),
        // `rewards_remaining` is a COUNT of fundable reward receipts, NOT an
        // ARC amount. Renamed on ingest so no call site can mistake it for
        // currency.
        attestationsRemaining: numOf(d.body, ["rewards_remaining"]),
        attestationsRemainingUnavailableReason: strOf(d.body, [
          "rewards_remaining_unavailable_reason",
        ]),
        treasuryIsFinite: nBool(d.body, "treasury_is_finite"),
        bondPerAttestation: baseOrArc(
          "community_worker_certificate_bond_base",
          "community_worker_certificate_bond_arc",
        ),
        challengePeriodBlocks: null,
        bondRefundedAfterChallengePeriod: null,
        fundingDetail: strOf(d.body, ["funding_detail", "funding"]),
      } as T;
    }
    case "fetch_earnings_projection": {
      const stored = localStorage.getItem("arc-desktop-state-v1");
      const addr = stored
        ? (JSON.parse(stored).identity?.address as string | undefined)
        : undefined;
      const empty = {
        sourceHost: base,
        rewardPerAttestation: null,
        rewardRateSource: "unknown" as const,
        communityRewardsEnabled: null,
        projectedDailyArc: null,
        projectedDailyUnavailableReason: null,
        rewardPolicyHash: null,
        rewardBudgetEpoch: null,
        rewardsRemainingThisEpoch: null,
        workerRewardsRemainingThisEpoch: null,
        coordinatorRewardsRemainingThisEpoch: null,
        issuanceReadyForWorker: null,
        rewardProgram: null,
        rewardIsCustomerDemand: null,
        attestationsTotal: 0,
        firstAttestationBlock: null,
        attestationsPerDay: null,
        rateUnavailableReason: null,
        observedOverBlocks: null,
        rateCaveat: null,
      };
      if (!addr) {
        return {
          ...empty,
          unavailable:
            "No identity on this device yet, so there is nothing to project.",
        } as T;
      }
      const path = `/worker/earnings/${strip0x(addr)}`;
      const d = await getDetailed(path);
      if (d.kind !== "ok") {
        return { ...empty, unavailable: reason(path, d) } as T;
      }
      const confirmed = confirmedEarningsFromBody(d.body);
      if (!confirmed) {
        return {
          ...empty,
          unavailable: `${base} answered ${path}, but did not provide the candidate mined-0x25 receipt and reward-readiness contract. Legacy inference-count arithmetic is not projected as earnings.`,
        } as T;
      }
      const rateBase = numOf(d.body, ["reward_per_attestation_base"]);
      const chainRate =
        rateBase !== null
          ? rateBase / ARC_BASE_UNITS
          : numOf(d.body, ["reward_per_attestation_arc"]);
      const attestationsTotal = numOf(d.body, ["total_rewards"]) ?? 0;
      const payableRate =
        chainRate !== null && chainRate >= 0 ? chainRate : null;
      const observedPerDay = numOf(d.body, ["attestations_per_day_observed"]);
      const perDay =
        observedPerDay !== null && observedPerDay >= 0
          ? observedPerDay
          : null;
      const rewardBudget =
        d.body.reward_budget !== null &&
        typeof d.body.reward_budget === "object" &&
        !Array.isArray(d.body.reward_budget)
          ? (d.body.reward_budget as Record<string, unknown>)
          : {};
      return {
        sourceHost: base,
        unavailable: null,
        // The selected coordinator must report the payable amount. A local
        // constant cannot prove remote rollout alignment.
        rewardPerAttestation: payableRate,
        rewardRateSource: payableRate !== null ? "chain" : "unknown",
        communityRewardsEnabled: nBool(
          d.body,
          "community_rewards_v1_enabled",
        ),
        projectedDailyArc: confirmed.projectedDailyArc,
        projectedDailyUnavailableReason:
          confirmed.projectedDailyUnavailableReason,
        rewardPolicyHash: strOf(d.body, ["reward_issuance_policy_hash"]),
        rewardBudgetEpoch: numOf(rewardBudget, ["epoch"]),
        rewardsRemainingThisEpoch: numOf(rewardBudget, [
          "remaining_this_epoch",
        ]),
        workerRewardsRemainingThisEpoch: numOf(rewardBudget, [
          "worker_remaining_this_epoch",
        ]),
        coordinatorRewardsRemainingThisEpoch: numOf(rewardBudget, [
          "coordinator_remaining_this_epoch",
        ]),
        issuanceReadyForWorker: nBool(d.body, "issuance_ready_for_worker"),
        rewardProgram: strOf(d.body, ["reward_program"]),
        rewardIsCustomerDemand: nBool(d.body, "reward_is_customer_demand"),
        attestationsTotal,
        firstAttestationBlock: numOf(d.body, ["first_attestation_block"]),
        attestationsPerDay: perDay,
        rateUnavailableReason:
          perDay !== null
            ? null
            : (strOf(d.body, ["attestations_per_day_unavailable_reason"]) ??
              (attestationsTotal === 0
                ? "No successful mined reward receipts are retained for this address, so there is no history to measure a rate from."
                : `${base} reports ${attestationsTotal} successful mined reward receipt(s) for this address but no observed rate, so a per-day figure cannot be measured here.`)),
        // `blocks_observed` on the wire.
        observedOverBlocks: numOf(d.body, ["blocks_observed"]),
        // Shown verbatim: the host knows its own method, this build does not.
        rateCaveat: strOf(d.body, ["attestations_per_day_caveat"]),
        // The bond is NOT on this endpoint — it comes from /economics/rewards.
      } as T;
    }
    case "fetch_node_contribution": {
      const cores = navigator.hardwareConcurrency ?? null;
      const shell = {
        sourceHost: base,
        layersHeld: null as string | null,
        layerCount: null as number | null,
        totalLayers: null as number | null,
        hopMsMean: null as number | null,
        hopSamples: null as number | null,
        hopUnavailableReason: null as string | null,
      };
      const direct = await getDetailed("/node/contribution");
      if (direct.kind === "ok") {
        // Nested: `threads`, `shards` and `own_compute_ms` are objects.
        const ranges =
          (at(direct.body, "shards.ranges") as
            | Array<Record<string, unknown>>
            | undefined) ?? [];
        const rendered = ranges
          .map((r) =>
            typeof r.start_layer === "number" && typeof r.end_layer === "number"
              ? `${r.start_layer}..${r.end_layer}`
              : null,
          )
          .filter((x): x is string => x !== null);
        const avail = nNum(direct.body, "threads.available_parallelism");
        return {
          ...shell,
          unavailable: null,
          source: "contribution",
          threadsInUse: nNum(direct.body, "threads.in_use"),
          // The host reports 0 when it could not read the core count. Zero
          // cores is not a measurement, so fall back to ours.
          threadsAvailable: avail !== null && avail > 0 ? avail : cores,
          layersHeld: rendered.length > 0 ? rendered.join(", ") : null,
          // A UNION of layers held, which the host computes. Summing the
          // ranges would double-count replicated layers.
          layerCount: nNum(direct.body, "shards.layers_held"),
          totalLayers: nNum(direct.body, "shards.total_layers"),
          runsServed: numOf(direct.body, ["sharded_runs_total"]),
          // `sharded_cache_hits` — no `_total` suffix here, unlike
          // `sharded_runs_total` and `sharded_bytes_total`.
          cacheHits: numOf(direct.body, ["sharded_cache_hits"]),
          hopMsMean: nNum(direct.body, "own_compute_ms.mean_ms"),
          hopSamples: nNum(direct.body, "own_compute_ms.samples"),
          hopUnavailableReason: nStr(
            direct.body,
            "own_compute_ms.unavailable_reason",
          ),
        } as T;
      }
      const [threads, stats] = await Promise.all([
        getDetailed("/node/threads"),
        getDetailed("/stats"),
      ]);
      const threadsInUse =
        threads.kind === "ok"
          ? numOf(threads.body, ["threads", "threads_in_use", "worker_threads"])
          : null;
      const runsServed =
        stats.kind === "ok" ? numOf(stats.body, ["sharded_runs_total"]) : null;
      if (threadsInUse === null && runsServed === null) {
        return {
          ...shell,
          unavailable:
            "Your node did not answer /node/contribution, /node/threads or /stats, so what it is contributing cannot be read right now.",
          source: "none",
          threadsInUse: null,
          threadsAvailable: cores,
          runsServed: null,
          cacheHits: null,
        } as T;
      }
      return {
        ...shell,
        unavailable: null,
        source: "composed",
        threadsInUse,
        threadsAvailable:
          (threads.kind === "ok"
            ? numOf(threads.body, ["available", "cpu_cores", "max_threads"])
            : null) ?? cores,
        runsServed,
        // /stats spells it with `_total`; /node/contribution does not.
        cacheHits:
          stats.kind === "ok"
            ? numOf(stats.body, [
                "sharded_cache_hits_total",
                "sharded_cache_hits",
              ])
            : null,
      } as T;
    }
    case "fetch_network_overview": {
      const [info, health, latest, validators] = await Promise.all([
        getDetailed("/network/info"),
        getDetailed("/health"),
        getDetailed("/block/latest"),
        getDetailed("/validators"),
      ]);
      const iv = info.kind === "ok" ? info.body : null;
      const h = health.kind === "ok" ? health.body : null;

      // Only /network/info may name the network. `/info` is deliberately not
      // consulted: its `chain` field is the constant "ARC Chain" everywhere,
      // so it cannot tell a testnet from a mainnet.
      const networkName = iv ? strOf(iv, ["network"]) : null;

      const rawValidators =
        validators.kind === "ok"
          ? ((validators.body.validators as Array<Record<string, unknown>>) ??
            [])
          : [];
      const list = rawValidators
        .map((v) => {
          const address = typeof v.address === "string" ? v.address : null;
          if (!address) return null;
          const stake = typeof v.stake === "number" ? v.stake : 0;
          return { address: strip0x(address), stake, active: stake > 0 };
        })
        .filter((x): x is NonNullable<typeof x> => x !== null);

      // /network/info applies the real min_active_stake threshold; counting
      // stake > 0 only approximates it. Prefer the reported figures and record
      // which was used, so an approximation is never shown as the host's own.
      const reportedActive = iv ? numOf(iv, ["validators_active"]) : null;
      const reportedRegistered = iv
        ? numOf(iv, ["validators_registered"])
        : null;

      const header =
        latest.kind === "ok"
          ? (latest.body.header as Record<string, unknown> | undefined)
          : undefined;
      const tsMs =
        header && typeof header.timestamp === "number" && header.timestamp > 0
          ? header.timestamp
          : null;
      const height = h ? numOf(h, ["height", "block_height"]) : null;
      // Prefer the host's own age; fall back to computing it from the header.
      const lastBlockAgeSecs =
        (iv ? numOf(iv, ["last_block_age_secs"]) : null) ??
        (tsMs !== null ? Math.max(0, Math.floor((Date.now() - tsMs) / 1000)) : null);

      return {
        sourceHost: base,
        unavailable:
          height === null && lastBlockAgeSecs === null && list.length === 0
            ? reason("/health", health)
            : null,
        networkName,
        networkNameUnavailableReason:
          (iv ? strOf(iv, ["network_unavailable_reason"]) : null) ??
          (info.kind === "ok" ? null : reason("/network/info", info)),
        chainId: iv ? strOf(iv, ["chain_id"]) : null,
        // The ONLY input allowed to make this app describe a network as mainnet.
        declaresMainnet: nBool(iv ?? {}, "declares_mainnet"),
        isBlockProducing: nBool(iv ?? {}, "is_block_producing"),
        isBlockProducingBasis: iv
          ? strOf(iv, ["is_block_producing_basis"])
          : null,
        hostVersion: h ? strOf(h, ["version"]) : null,
        height,
        lastBlockAgeSecs,
        dagRound: h ? numOf(h, ["dag_round"]) : null,
        dagCommitted: h ? numOf(h, ["dag_committed"]) : null,
        peers: h ? numOf(h, ["peers", "connected_peers"]) : null,
        validatorsActive:
          reportedActive ??
          (validators.kind === "ok"
            ? list.filter((v) => v.active).length
            : null),
        validatorsRegistered:
          reportedRegistered ??
          (validators.kind === "ok"
            ? (numOf(validators.body, ["count"]) ?? list.length)
            : null),
        minActiveStake: iv ? numOf(iv, ["min_active_stake"]) : null,
        validatorSplitDerived: reportedActive === null,
        validators: list,
      } as T;
    }
    case "fetch_recent_blocks": {
      const limit = Math.min(
        100,
        Math.max(1, (args as { limit?: number } | undefined)?.limit ?? 10),
      );
      // The range must be computed. `/blocks` defaults `from` to 0, so
      // `?limit=10` returns the ten OLDEST blocks starting at genesis — not
      // the newest ten. Verified against the live NYC seed, which answered
      // `?limit=2` with height 0. Anchor the window to the tip instead.
      const health = await getDetailed("/health");
      const tip =
        health.kind === "ok" ? numOf(health.body, ["height", "block_height"]) : null;
      if (tip === null) {
        return {
          sourceHost: base,
          unavailable: `Could not read the current height from ${base}, so the newest blocks cannot be located.`,
          blocks: [],
        } as T;
      }
      const from = Math.max(0, tip - limit + 1);
      const path = `/blocks?from=${from}&to=${tip}&limit=${limit}`;
      const d = await getDetailed(path);
      if (d.kind !== "ok") {
        return {
          sourceHost: base,
          unavailable: reason(path, d),
          blocks: [],
        } as T;
      }
      const arr = (d.body.blocks as Array<Record<string, unknown>>) ?? [];
      return {
        sourceHost: base,
        unavailable: null,
        blocks: arr
          .map((b) => {
            const height = typeof b.height === "number" ? b.height : null;
            if (height === null) return null;
            // A zero timestamp is not a time: genesis carries `timestamp: 0`,
            // and the UI's relative-time formatter renders that as "20770d
            // ago". Same reasoning for an all-zero producer, which is a
            // placeholder rather than an address.
            const ts = numOf(b, ["timestamp"]);
            const producer =
              typeof b.producer === "string" ? strip0x(b.producer) : null;
            return {
              height,
              hash: typeof b.hash === "string" ? strip0x(b.hash) : "",
              timestampMs: ts !== null && ts > 0 ? ts : null,
              txCount: numOf(b, ["tx_count"]),
              proposer:
                producer && /[^0]/.test(producer) ? producer : null,
            };
          })
          .filter((x): x is NonNullable<typeof x> => x !== null)
          .sort((a, b) => b.height - a.height),
      } as T;
    }
    case "fetch_block_txs": {
      const { height, limit } = args as { height: number; limit?: number };
      const path = `/block/${height}/txs?limit=${Math.min(1000, Math.max(1, limit ?? 50))}`;
      const d = await getDetailed(path);
      if (d.kind !== "ok") {
        return {
          sourceHost: base,
          unavailable: reason(path, d),
          height,
          txCount: null,
          txs: [],
        } as T;
      }
      const rows =
        (d.body.transactions as Array<Record<string, unknown>>) ?? [];
      return {
        sourceHost: base,
        unavailable: null,
        height,
        txCount: numOf(d.body, ["tx_count"]),
        txs: rows
          .map((t) => {
            const hash = typeof t.hash === "string" ? strip0x(t.hash) : null;
            if (!hash) return null;
            return {
              index: numOf(t, ["index"]) ?? 0,
              hash,
              txType: strOf(t, ["tx_type"]),
              from: typeof t.from === "string" ? strip0x(t.from) : null,
            };
          })
          .filter((x): x is NonNullable<typeof x> => x !== null),
      } as T;
    }
    case "lookup_tx": {
      const raw = (args as { hash: string }).hash;
      const hash = strip0x(raw);
      const shell = {
        sourceHost: base,
        hash,
        blockHeight: null,
        blockHash: null,
        txIndex: null,
        success: null,
        gasUsed: null,
      };
      if (hash.length !== 64 || !/^[0-9a-f]+$/.test(hash)) {
        return {
          ...shell,
          status: "invalid_hash",
          unavailable: `A transaction hash is 64 hex characters (an optional 0x prefix is fine). That one is ${hash.length}.`,
        } as T;
      }
      const path = `/tx/${hash}`;
      const d = await getDetailed(path);
      if (d.kind === "ok") {
        return {
          ...shell,
          unavailable: null,
          status: "mined",
          blockHeight: numOf(d.body, ["block_height"]),
          blockHash:
            typeof d.body.block_hash === "string"
              ? strip0x(d.body.block_hash)
              : null,
          txIndex: numOf(d.body, ["index"]),
          success:
            typeof d.body.success === "boolean" ? d.body.success : null,
          gasUsed: numOf(d.body, ["gas_used"]),
        } as T;
      }
      // A 404 is ALSO what a pending attestation looks like — /tx/{hash} is a
      // receipt lookup and a mempool tx has no receipt. Never "invalid".
      if (d.kind === "notFound") {
        return { ...shell, unavailable: null, status: "not_found" } as T;
      }
      if (d.kind === "badRequest") {
        return {
          ...shell,
          status: "invalid_hash",
          unavailable: `${base} rejected that hash as malformed.`,
        } as T;
      }
      return {
        ...shell,
        status: "error",
        unavailable: reason(path, d),
      } as T;
    }
    case "open_external":
      window.open((args as { url: string }).url, "_blank");
      return undefined as T;
    case "clear_crash":
      return undefined as T;
    case "ensure_binary":
      // Browser (live mode) can't install a native binary - pretend it's
      // already installed so the UI doesn't block onboarding.
      return {
        path: "/browser-live-mode",
        downloadedBytes: 0,
        totalBytes: 0,
        alreadyInstalled: true,
      } as T;
    case "get_autostart":
      return false as T;
    case "update_install_policy":
      return {
        canInstall: false,
        channel: "package-manager",
        instructions: "Browser previews cannot install application updates.",
      } as T;
    case "list_model_tiers":
      return [
        {
          id: "standard",
          displayName: "Llama-2 7B Chat (Q4_K_M) — ARC compatible",
          sizeBytes: 4_081_004_224,
          url: "https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/main/llama-2-7b-chat.Q4_K_M.gguf",
        },
      ] as T;
    case "recommended_tier":
      return "standard" as T;
    case "existing_model_for_tier":
      return null as T;
    case "download_model":
      return "/browser-live-mode/.arc/models/standard.gguf" as T;
    case "remove_model":
      return undefined as T;
    default:
      throw new Error(`Unhandled live command: ${cmd}`);
  }
}

// Mock state - only used in browser preview. Tauri env always hits the real backend.
let mockStartedAt: number | null = null;
let mockWorkerThreads: number | null = null;
const mockLogs: LogEntry[] = [];
// Explicit browser-preview fixture for a hypothetical host whose candidate
// receipt contract is ready. These are static layout values, never a claim
// about the public fleet and never increased merely because a process runs.
const mockEarnings: Earnings = {
  totalArc: 12_847.5,
  todayArc: null,
  pendingArc: null,
  rank: null,
  attestations: 1283,
  lastPayoutAt: null,
  lastPayoutBlock: 123_462,
  confirmedReceipts: [],
  projectedDailyArc: null,
  projectedDailyUnavailableReason: "browser preview fixture",
  recoveryEpoch: 1,
  validatorSetId: 1,
  fromChain: true,
};

// The mock deliberately exercises BOTH attestation shapes the UI must
// survive: raw 0x16 rows submitted by the user's address (with tokens and a
// timestamp, but no reward) and a row from another address with no telemetry.
const MOCK_ADDRESS = "arc1qxywa87m9v3kz8n2p5nc4z8y7dv4q3lns8z3p";

const mockAttestations: Attestation[] = [
  {
    txHash: "0xe0c73bb8a4446f23a62033001cb22e1e9298d5ce1cfea8111762c1ca2833f67d",
    inputPreview: "What is the largest planet in our solar system?",
    outputHash: "0xe0c73bb8a4446f23a62033001cb22e1e",
    modelHash: "0xabec2d582beb97a876c21d7ccc5e8e48",
    tokens: 42,
    latencyMs: 147,
    timestamp: Date.now() - 1000 * 34,
    blockHeight: 123_462,
    txType: "Inference",
    from: MOCK_ADDRESS,
    mine: true,
    verified: true,
  },
  {
    txHash: "0xa9fe23bb8a4446f23a62033001cb22e1e9298d5ce1cfea8111762c1ca28336de",
    inputPreview: "Write a Rust function to compute BLAKE3 of a file",
    outputHash: "0x7c31fe12aab4c7d2e44a88b1f91023ab",
    modelHash: "0xabec2d582beb97a876c21d7ccc5e8e48",
    tokens: 128,
    latencyMs: 412,
    timestamp: Date.now() - 1000 * 89,
    blockHeight: 123_455,
    txType: "Inference",
    from: MOCK_ADDRESS,
    mine: true,
    verified: true,
  },
  {
    // Someone else's work, flat shape, no telemetry: no reward, no token
    // count, no timestamp. The UI must render this without inventing any of
    // the three.
    txHash: "0x14ab23bb8a4446f23a62033001cb22e1e9298d5ce1cfea8111762c1ca28335f2",
    inputPreview: "",
    outputHash: "",
    modelHash: "",
    tokens: null,
    latencyMs: null,
    timestamp: null,
    blockHeight: 123_401,
    txType: "Inference",
    from: "0cda729e004c87fd15efc6b859ab567bbaba82ba95bdcf5f026082e0865e938e",
    mine: false,
    verified: true,
  },
  {
    // Old-seed PADDING. `/inference/attestations` on the deployed v0.7.9 seeds
    // tops its list up with unrelated transactions tagged `tx_type: "Other"`
    // once genuine attestation rows run out — at limit=500 some seeds returned
    // 500 of these and zero real ones. The Network screen filters them out;
    // this row is here so that filter is demonstrably doing something.
    txHash: "0x77cc23bb8a4446f23a62033001cb22e1e9298d5ce1cfea8111762c1ca2833aa1",
    inputPreview: "",
    outputHash: "",
    modelHash: "",
    tokens: null,
    latencyMs: null,
    timestamp: null,
    blockHeight: 123_390,
    txType: "Other",
    from: "0cda729e004c87fd15efc6b859ab567bbaba82ba95bdcf5f026082e0865e938e",
    mine: false,
    verified: true,
  },
];

/** The seed the mock pretends to have pinned. Mirrors mock `node_status`. */
const MOCK_CHAIN_HOST = "http://140.82.16.112:9090";

/**
 * Fourteen validators, four of them at stake 0.
 *
 * This fixture preserves an older host response in which four registered
 * entries had zero stake. It exists to keep the Network screen's active versus
 * registered distinction testable; it is not a current fleet claim.
 */
const MOCK_VALIDATORS = Array.from({ length: 14 }, (_, i) => ({
  address: `${(i + 1).toString(16).padStart(2, "0")}cda729e004c87fd15efc6b859ab567bbaba82ba95bdcf5f026082e0865e93${(i + 16).toString(16)}`,
  stake: i < 10 ? 500_000 * ARC_BASE_UNITS : 0,
  active: i < 10,
}));

/**
 * Recent blocks, descending. Heights and gaps are fixed literals rather than
 * derived from `Date.now()`: a fabricated "seconds ago" ladder that shifts on
 * every poll is exactly the class of invention this app removed.
 */
const MOCK_BLOCKS = [
  { height: 123_469, gapSecs: 400, txCount: 2 },
  { height: 123_468, gapSecs: 407, txCount: 0 },
  { height: 123_467, gapSecs: 415, txCount: 1 },
  { height: 123_466, gapSecs: 422, txCount: 0 },
  { height: 123_465, gapSecs: 430, txCount: 3 },
  { height: 123_464, gapSecs: 438, txCount: 0 },
  { height: 123_463, gapSecs: 445, txCount: 0 },
  { height: 123_462, gapSecs: 453, txCount: 1 },
  { height: 123_461, gapSecs: 460, txCount: 0 },
  { height: 123_460, gapSecs: 468, txCount: 2 },
].map((b) => ({
  height: b.height,
  hash: `${b.height.toString(16)}c41ab77e0d5c3b8a16e94f20d7a5589cc31be4470a2e6d1f8039b5ca7e4`
    .padEnd(64, "0")
    .slice(0, 64),
  timestampMs: Date.now() - b.gapSecs * 1000,
  txCount: b.txCount,
  proposer: "0cda729e004c87fd15efc6b859ab567bbaba82ba95bdcf5f026082e0865e938e",
}));

function seedMockLogs() {
  if (mockLogs.length > 0) return;
  const now = Date.now();
  const entries: Array<[LogEntry["level"], string, number]> = [
    ["info", "arc-node v0.5.2 starting", 12_000],
    ["info", "Loaded identity arc1qxy...8z3p", 11_800],
    ["info", "Connecting to 8 testnet seeds", 11_500],
    ["ok", "Handshake complete with 149.28.32.76", 10_200],
    ["ok", "Handshake complete with 140.82.16.112", 10_100],
    ["ok", "Handshake complete with 136.244.109.1", 10_000],
    ["info", "Joining DAG consensus at round 43,821", 9_500],
    ["ok", "Synced to current round. committed=43,820", 4_200],
    ["info", "Model loaded: llama-2-7b-chat.Q4_K_M.gguf (3.8 GB)", 3_400],
    ["info", "Serving inference on :9944", 2_100],
    ["ok", "Attestation submitted tx=0xe0c73bb8...", 34_000],
    ["ok", "Attestation submitted tx=0xa9fe23bb...", 89_000],
    ["ok", "Attestation submitted tx=0x14ab23bb...", 214_000],
  ];
  entries.forEach(([level, message, ago], i) => {
    mockLogs.push({
      id: `log-${i}`,
      level,
      message,
      timestamp: now - ago,
    });
  });
}

seedMockLogs();

const DEFAULT_HARDWARE: HardwareInfo = {
  platform: "macOS",
  arch: "arm64",
  cpuModel: "Apple M2 Ultra",
  cpuCores: 24,
  ramGb: 64,
  gpuName: "Apple M2 Ultra (76-core)",
  gpuVramGb: 64,
  recommendedModel: "Llama-2-7B Q4_K_M (3.8 GB, ARC compatible)",
  recommendedRole: "worker",
};

async function mockInvoke<T>(cmd: string, args?: unknown): Promise<T> {
  // Refuse to serve fake data from a production build. A user who managed
  // to load the production bundle outside Tauri (opened dist/ in Safari,
  // loaded it off a CDN, etc.) would otherwise see a fabricated dashboard.
  if (IS_PROD_TAURI_BUNDLE && !IS_TAURI && !liveBase()) {
    throw new Error(
      "ARC desktop is running outside its native host. Open the arc app, not the HTML bundle.",
    );
  }
  // Test seam (see `mockOverride`). Checked after the production guard above,
  // so it cannot fabricate anything in a real bundle.
  const override = mockOverride<T>(cmd);
  if (override !== undefined) {
    await new Promise((r) => setTimeout(r, 20));
    return override;
  }
  await new Promise((r) => setTimeout(r, 120));
  switch (cmd) {
    case "detect_hardware":
      return DEFAULT_HARDWARE as T;
    case "generate_identity":
      return {
        address: MOCK_ADDRESS,
        publicKey:
          "0x7c31fe12aab4c7d2e44a88b1f91023abfe23bb8a4446f23a62033001cb22e1e9",
        createdAt: Date.now(),
      } as T;
    case "reveal_seed_phrase":
      return "galaxy stellar quantum horizon crystal ember aurora silent mirror ocean celestial fragment" as T;
    case "load_identity":
      return null as T;
    case "save_config":
      return undefined as T;
    case "load_config":
      return null as T;
    case "node_status": {
      const running = mockStartedAt !== null;
      const uptime = running
        ? Math.floor((Date.now() - mockStartedAt!) / 1000)
        : 0;
      return {
        running,
        pid: running ? 42_731 : null,
        health: running ? (uptime < 8 ? "syncing" : "live") : "offline",
        version: "0.8.0",
        peers: running ? 8 : 0,
        round: running ? 43_821 + Math.floor(uptime / 4) : 0,
        committed: running ? 43_820 + Math.floor(uptime / 4) : 0,
        height: running ? 43_820 + Math.floor(uptime / 4) : 0,
        uptimeSeconds: uptime,
        address: MOCK_ADDRESS,
        rpcPort: 9090,
        lastError: null,
        coordinatorUrl: null,
        // Chain numbers are the network's, not this node's - and the mock
        // reflects the real testnet's stalled block production.
        chainHost: "http://140.82.16.112:9090",
        chainHeight: 123_469,
        chainRound: 9_596_644,
        chainBlockAgeSeconds: 400,
        workerThreads: running ? mockWorkerThreads : null,
        cpuCores: 24,
      } as T;
    }
    case "start_node":
      mockStartedAt = Date.now();
      return undefined as T;
    case "stop_node":
      mockStartedAt = null;
      return undefined as T;
    case "restart_node":
      mockStartedAt = Date.now();
      return undefined as T;
    case "reset_peer_state":
      mockStartedAt = Date.now();
      return {
        removedPath: "/mock/.arc/data/known_peers.json",
        wasPresent: true,
        message: "Cleared cached peer list. Rebootstrapping from testnet seeds.",
      } as T;
    case "fetch_earnings": {
      return mockEarnings as T;
    }
    case "fetch_attestations":
      return mockAttestations as T;
    case "fetch_logs":
      return mockLogs as T;
    case "fetch_network_stats":
      return {
        totalNodes: 1_283,
        totalInferences: 4_812_392,
        avgTps: 33_221,
        latestBlock: 43_821,
      } as T;
    // ── Chain visibility + projection ────────────────────────────────────
    // Populated, so browser preview and the screenshot suite show the real
    // layout. The 404 / no-history / degraded paths are reached by tests via
    // `window.__ARC_MOCK__` — see `mockOverride`.
    case "fetch_reward_economics":
      return {
        sourceHost: MOCK_CHAIN_HOST,
        unavailable: null,
        rewardPerAttestation: MOCK_REWARD_PER_RECEIPT,
        treasuryBalanceArc: 4_182_500,
        treasuryBalanceUnavailableReason: null,
        // A COUNT of successful reward receipts the treasury can still fund:
        // 4,182,500 ARC / 2.5 ARC = 1,673,000.
        attestationsRemaining: 1_673_000,
        attestationsRemainingUnavailableReason: null,
        treasuryIsFinite: true,
        // Verified community reward certificates carry no worker bond.
        bondPerAttestation: 0,
        challengePeriodBlocks: null,
        bondRefundedAfterChallengePeriod: null,
        fundingDetail:
          "Transferred from a pre-funded testnet treasury account. Not an emission and not revenue share.",
      } as T;
    case "fetch_earnings_projection":
      return {
        sourceHost: MOCK_CHAIN_HOST,
        unavailable: null,
        rewardPerAttestation: MOCK_REWARD_PER_RECEIPT,
        rewardRateSource: "chain",
        communityRewardsEnabled: true,
        projectedDailyArc: 108,
        projectedDailyUnavailableReason: null,
        rewardPolicyHash: "0xpreview-policy",
        rewardBudgetEpoch: 2,
        rewardsRemainingThisEpoch: 31,
        workerRewardsRemainingThisEpoch: 7,
        coordinatorRewardsRemainingThisEpoch: 11,
        issuanceReadyForWorker: true,
        rewardProgram: "protocol-capped testnet promotional compute subsidy",
        rewardIsCustomerDemand: false,
        attestationsTotal: 1_283,
        firstAttestationBlock: 118_011,
        attestationsPerDay: 43.2,
        rateUnavailableReason: null,
        observedOverBlocks: 5_458,
        rateCaveat:
          "Derived from the first and last attestation timestamps in this node's scan window; a node offline for part of that window will read low.",
      } as T;
    case "fetch_node_contribution":
      return {
        sourceHost: "http://127.0.0.1:9090",
        unavailable: null,
        source: "contribution",
        threadsInUse: mockWorkerThreads ?? 24,
        threadsAvailable: 24,
        layersHeld: "0..6",
        layerCount: 6,
        totalLayers: 32,
        runsServed: 15,
        cacheHits: 3,
        hopMsMean: 182,
        hopSamples: 15,
        hopUnavailableReason: null,
      } as T;
    case "fetch_network_overview":
      return {
        sourceHost: MOCK_CHAIN_HOST,
        unavailable: null,
        networkName: "arc-testnet-1",
        networkNameUnavailableReason: null,
        chainId: "arc-testnet-1",
        // The host declares itself NOT mainnet. The UI must never say mainnet
        // unless this is explicitly true.
        declaresMainnet: false,
        isBlockProducing: false,
        isBlockProducingBasis:
          "no block sealed within block_production_fresh_secs (120s)",
        hostVersion: "0.7.9",
        height: 123_469,
        // Matches the mock node_status: the real testnet's block production
        // is stalled, and the mock reflects that rather than a healthy fiction.
        lastBlockAgeSecs: 400,
        dagRound: 9_596_644,
        dagCommitted: 9_596_640,
        peers: 8,
        validatorsActive: 10,
        validatorsRegistered: 14,
        minActiveStake: 500_000,
        validatorSplitDerived: false,
        validators: MOCK_VALIDATORS,
      } as T;
    case "fetch_recent_blocks": {
      const limit = (args as { limit?: number } | undefined)?.limit ?? 10;
      return {
        sourceHost: MOCK_CHAIN_HOST,
        unavailable: null,
        blocks: MOCK_BLOCKS.slice(0, limit),
      } as T;
    }
    case "fetch_block_txs": {
      const { height } = args as { height: number };
      const block = MOCK_BLOCKS.find((b) => b.height === height);
      const n = block?.txCount ?? 0;
      return {
        sourceHost: MOCK_CHAIN_HOST,
        unavailable: null,
        height,
        txCount: n,
        // Derived from the block's own tx_count so an expanded block never
        // shows more rows than the list said it had.
        txs: Array.from({ length: n }, (_, i) => ({
          index: i,
          hash: strip0x(
            mockAttestations[i % mockAttestations.length].txHash,
          ),
          txType: i === 0 ? "Inference" : "Transfer",
          from: MOCK_ADDRESS,
        })),
      } as T;
    }
    case "lookup_tx": {
      const hash = strip0x((args as { hash: string }).hash);
      const shell = {
        sourceHost: MOCK_CHAIN_HOST,
        hash,
        blockHeight: null,
        blockHash: null,
        txIndex: null,
        success: null,
        gasUsed: null,
      };
      if (hash.length !== 64 || !/^[0-9a-f]+$/.test(hash)) {
        return {
          ...shell,
          status: "invalid_hash",
          unavailable: `A transaction hash is 64 hex characters (an optional 0x prefix is fine). That one is ${hash.length}.`,
        } as T;
      }
      // The first mock attestation is mined; anything else well-formed is
      // treated as not yet in a block, which is the honest answer for a hash
      // this host has no receipt for.
      const mined = strip0x(mockAttestations[0].txHash);
      if (hash === mined) {
        return {
          ...shell,
          unavailable: null,
          status: "mined",
          blockHeight: 123_462,
          blockHash:
            "9f2c41ab77e0d5c3b8a16e94f20d7a5589cc31be4470a2e6d1f8039b5ca7e412",
          txIndex: 0,
          success: true,
          gasUsed: 21_000,
        } as T;
      }
      return { ...shell, unavailable: null, status: "not_found" } as T;
    }
    case "open_external":
      return undefined as T;
    case "fetch_balance":
      return {
        address: "fakehex0000000000000000000000000000000000000000000000000000000000",
        balanceBase: "28500000000000",
        balanceArc: "28500",
        nonce: 3,
        stakedBalanceBase: "0",
        stakedBalanceArc: "0",
      } as T;
    case "faucet_claim":
      return {
        txHash:
          "8f31fe12aab4c7d2e44a88b1f91023abfe23bb8a4446f23a62033001cb22e1e9",
        amountBase: "1000000000",
        amountArc: "1",
        receiptStatus: "pending",
        mined: false,
        success: null,
        blockHeight: null,
        blockHash: null,
        sourceHost: MOCK_CHAIN_HOST,
        unavailable: null,
        message: "Faucet claim was accepted and is waiting for a mined receipt.",
      } as T;
    case "send_arc":
      return {
        txHash:
          "8e31fe12aab4c7d2e44a88b1f91023abfe23bb8a4446f23a62033001cb22e1e9",
        amountBase: "1250000000",
        amountArc: "1.25",
        receiptStatus: "pending",
        mined: false,
        success: null,
        blockHeight: null,
        blockHash: null,
        sourceHost: MOCK_CHAIN_HOST,
        unavailable: null,
        message: "Transfer was accepted and is waiting for a mined receipt.",
      } as T;
    case "run_inference": {
      const { prompt } = args as { prompt: string };
      await new Promise((r) => setTimeout(r, 900));
      return {
        input: prompt,
        output: "  This is a mock response for local preview mode.",
        outputHash:
          "0xbe91fe12aab4c7d2e44a88b1f91023c811112222333344445555666677778888",
        modelHash:
          "0xabec2d582beb97a876c21d7ccc5e8e4833e8fd34aee0cb5b64e9f14f5ea57fdb",
        tokensGenerated: 15,
        inferenceMs: 820,
        txHash:
          "0x1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b",
        deterministic: true,
        engine: "mock",
        explorerUrl: "/tx/0x1a2b3c4d5e6f7a8b",
        servedLocally: true,
      } as T;
    }
    case "run_inference_via_coordinator": {
      const { prompt } = args as { prompt: string };
      await new Promise((r) => setTimeout(r, 1200));
      return {
        input: prompt,
        output:
          "  Mock coordinator response — browser preview only. An installed build asks the selected coordinator for its agreement evidence; a host-reported quorum is not proof of payment or a healthy shared public chain.",
        outputHash:
          "0xd3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3",
        modelHash: "",
        tokensGenerated: 28,
        inferenceMs: 18_400,
        txHash: "",
        deterministic: true,
        engine: "consensus",
        explorerUrl: "",
        servedLocally: false,
        consensus: {
          k: 3,
          votesTotal: 48,
          unanimous: 48,
          majority: 0,
          split: 0,
          divergentReplicaCount: 0,
        },
        coordinator: "http://149.28.32.76:9090",
      } as T;
    }
    case "run_inference_via_coordinator_direct": {
      const { prompt } = args as { prompt: string };
      await new Promise((r) => setTimeout(r, 800));
      return {
        input: prompt,
        output:
          "  Mock direct-coordinator response — browser preview only. An installed build may ask one coordinator directly, but that response alone does not prove independent recomputation, community assignment, mining, or payment.",
        outputHash:
          "0xe5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5",
        modelHash:
          "0xabec2d582beb97a876c21d7ccc5e8e4833e8fd34aee0cb5b64e9f14f5ea57fdb",
        tokensGenerated: 28,
        inferenceMs: 7_800,
        txHash: "0xfafafafafafafafafafafafafafafafafafafafafafafafafafafafafafafafa",
        deterministic: true,
        engine: "INT8 integer (cross-platform deterministic)",
        explorerUrl: "/tx/0xfafafafafafafafafafafafafafafafafafafafafafafafafafafafafafafafa",
        consensus: undefined,
        coordinator: "http://140.82.16.112:9090",
        servedLocally: false,
      } as T;
    }
    case "tier1_submit": {
      throw settlementWriteUnavailable("Tier 1 on-chain inference");
    }
    case "tier1_result": {
      const { requestId } = args as { requestId: string };
      // Walk through Open → Voting → Finalized over ~6 polls so the UI
      // exercises every state. mockInvoke is stateless; we derive the
      // "step" from a counter on globalThis to make it look animated.
      const w = globalThis as unknown as { __arcTier1Tick?: Record<string, number> };
      if (!w.__arcTier1Tick) w.__arcTier1Tick = {};
      const counter = (w.__arcTier1Tick[requestId] ?? 0) + 1;
      w.__arcTier1Tick[requestId] = counter;
      const voteCount = Math.min(5, Math.max(0, counter - 1));
      const status =
        counter === 1 ? "Open" : counter < 5 ? "Voting" : "Finalized";
      const votes: Tier1Vote[] = Array.from({ length: voteCount }, (_, i) => ({
        voter:
          "0xv" +
          (i + 1).toString().padStart(63, "0"),
        outputHash:
          "0xe598" + "0".repeat(60),
      }));
      return {
        requestId,
        status,
        voteCount,
        committeeSize: 5,
        anchorHeight: 12345,
        deadlineBlocks: 20,
        votes,
        outputHash: status === "Finalized" ? "0xe598" + "0".repeat(60) : null,
        outputBlob:
          status === "Finalized"
            ? "A zero-knowledge proof lets one party prove they know a secret without revealing the secret itself."
            : null,
        maxReward: 10,
      } as T;
    }
    case "run_paid_inference": {
      throw settlementWriteUnavailable("Paid inference escrow");
    }
    case "clear_crash":
      return undefined as T;
    case "save_logs":
      return {
        path: "/mock/Downloads/arc-node-20260817-120000.log",
        lines: mockLogs.length,
      } as T;
    case "set_worker_threads": {
      const { threads } = args as { threads: number };
      mockWorkerThreads = threads;
      // Mirrors the real fallback: no /node/threads endpoint exists yet, so
      // applying a new width restarts the node.
      if (mockStartedAt !== null) mockStartedAt = Date.now();
      return {
        workerThreads: threads,
        restarted: mockStartedAt !== null,
        message:
          mockStartedAt !== null
            ? `Restarted the node with ${threads} cores.`
            : `Saved. The node will use ${threads} cores when it starts.`,
      } as T;
    }
    case "ensure_binary":
      // Mock path - no real download. Pretend it completed instantly.
      return {
        path: "/mock/.arc/bin/arc-node",
        downloadedBytes: 45_000_000,
        totalBytes: 45_000_000,
        alreadyInstalled: false,
      } as T;
    case "get_autostart":
      return true as T;
    case "update_install_policy":
      return {
        canInstall: true,
        channel: "native",
        instructions: "ARC can install this signed update in place.",
      } as T;
    case "list_model_tiers":
      return [
        {
          id: "standard",
          displayName: "Llama-2 7B Chat (Q4_K_M) — ARC compatible",
          sizeBytes: 4_081_004_224,
          url: "https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/main/llama-2-7b-chat.Q4_K_M.gguf",
        },
      ] as T;
    case "recommended_tier":
      return "standard" as T;
    case "existing_model_for_tier":
      return null as T;
    case "download_model":
      // In mock mode pretend the download finishes instantly. The real
      // backend streams progress events; the mock skips that for speed.
      return "/mock/.arc/models/standard.gguf" as T;
    case "remove_model":
      return undefined as T;
    default:
      throw new Error(`Unmocked Tauri command: ${cmd}`);
  }
}

async function realInvoke<T>(cmd: string, args?: unknown): Promise<T> {
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(cmd, args as Record<string, unknown>);
}

export async function invoke<T>(cmd: string, args?: unknown): Promise<T> {
  if (IS_TAURI) return realInvoke<T>(cmd, args);
  if (liveBase()) return liveInvoke<T>(cmd, args);
  return mockInvoke<T>(cmd, args);
}

// Typed wrappers - call these, not invoke() directly.

export const api = {
  detectHardware: () => invoke<HardwareInfo>("detect_hardware"),
  generateIdentity: () => invoke<Identity>("generate_identity"),
  loadIdentity: () => invoke<Identity | null>("load_identity"),
  /**
   * Fetch the BIP-39 recovery phrase for the backup screen.
   *
   * The result MUST NOT be persisted, logged, or put into app state that
   * gets serialized. It exists only for as long as the user is looking at
   * it. See `IdentityPublic` in the Rust types for why.
   */
  revealSeedPhrase: () => invoke<string>("reveal_seed_phrase"),
  saveConfig: (config: NodeConfig) => invoke<void>("save_config", { config }),
  loadConfig: () => invoke<NodeConfig | null>("load_config"),
  startNode: (config: NodeConfig) => invoke<void>("start_node", { config }),
  stopNode: () => invoke<void>("stop_node"),
  restartNode: () => invoke<void>("restart_node"),
  resetPeerState: () => invoke<ResetPeerStateResult>("reset_peer_state"),
  nodeStatus: () => invoke<NodeStatus>("node_status"),
  fetchEarnings: () => invoke<Earnings>("fetch_earnings"),
  fetchAttestations: (limit = 20) =>
    invoke<Attestation[]>("fetch_attestations", { limit }),
  fetchLogs: (limit = 200) => invoke<LogEntry[]>("fetch_logs", { limit }),
  fetchNetworkStats: () => invoke<NetworkStats>("fetch_network_stats"),

  // ── Chain visibility + projection ──────────────────────────────────────
  // Each of these resolves to a struct carrying `unavailable` rather than
  // rejecting, because "this host does not serve that endpoint" is a fact to
  // display, not an exception to swallow. Callers render the reason.
  /** The finite reward treasury — the ceiling on any projection. */
  fetchRewardEconomics: () =>
    invoke<RewardEconomics>("fetch_reward_economics"),
  /** Measured inputs for the earnings projection. */
  fetchEarningsProjection: () =>
    invoke<EarningsProjection>("fetch_earnings_projection"),
  /** What the node on THIS machine is contributing. */
  fetchNodeContribution: () =>
    invoke<NodeContribution>("fetch_node_contribution"),
  /** Height, block age, validator split and peers for the pinned host. */
  fetchNetworkOverview: () =>
    invoke<NetworkOverview>("fetch_network_overview"),
  fetchRecentBlocks: (limit = 10) =>
    invoke<RecentBlocks>("fetch_recent_blocks", { limit }),
  /** Transactions in one block. Called on expand, never on the poll path. */
  fetchBlockTxs: (height: number, limit = 50) =>
    invoke<BlockTxs>("fetch_block_txs", { height, limit }),
  /** Resolve one tx/attestation hash against the pinned host. */
  lookupTx: (hash: string) => invoke<TxLookup>("lookup_tx", { hash }),
  fetchBalance: () => invoke<AccountBalance>("fetch_balance"),
  faucetClaim: () => invoke<FaucetResult>("faucet_claim"),
  sendArc: (to: string, amountArc: string) =>
    invoke<WalletTxResult>("send_arc", { to, amountArc }),
  // `chatTemplate` asks the serving node to apply the loaded model's own
  // chat template. The client no longer wraps prompts in Llama-2's
  // `[INST] ... [/INST]` tags, which were wrong for other architectures and
  // got double-applied when the node templated too.
  runInference: (prompt: string, maxTokens = 32, chatTemplate = true) =>
    invoke<InferenceResult>("run_inference", { prompt, maxTokens, chatTemplate }),
  runInferenceViaCoordinator: (
    prompt: string,
    maxTokens = 32,
    k = 3,
    chatTemplate = true,
  ) =>
    invoke<InferenceResult>("run_inference_via_coordinator", {
      prompt,
      maxTokens,
      k,
      chatTemplate,
    }),
  runInferenceViaCoordinatorDirect: (
    prompt: string,
    maxTokens = 32,
    chatTemplate = true,
  ) =>
    invoke<InferenceResult>("run_inference_via_coordinator_direct", {
      prompt,
      maxTokens,
      chatTemplate,
    }),
  // Write commands remain in the IPC surface for compatibility, but every
  // native/browser implementation rejects them before signing or network I/O.
  // `tier1Result` is read-only inspection for IDs created by older builds.
  tier1Submit: (
    prompt: string,
    maxTokens = 32,
    maxReward = 10,
    deadlineBlocks = 20,
    committeeSize = 1, // TEMP: solo-chain testing; production should be 3-5
  ) =>
    invoke<Tier1Submitted>("tier1_submit", {
      prompt,
      maxTokens,
      maxReward,
      deadlineBlocks,
      committeeSize,
    }),
  tier1Result: (requestId: string) =>
    invoke<Tier1Result>("tier1_result", { requestId }),
  runPaidInference: (
    prompt: string,
    maxTokens = 32,
    maxFee = 10_000,
    k = 3,
  ) =>
    invoke<PaidInferenceResult>("run_paid_inference", {
      prompt,
      maxTokens,
      maxFee,
      k,
    }),
  clearCrash: () => invoke<void>("clear_crash"),
  openExternal: (url: string) => invoke<void>("open_external", { url }),
  /**
   * Write the log ring to a file via a native save dialog. Replaces a
   * `Blob` + `<a download>` click, which WKWebView silently ignores — so
   * the button did nothing at all on macOS.
   */
  saveLogs: () => invoke<SavedLogs>("save_logs"),
  /** Change how many cores the node contributes. */
  setWorkerThreads: (threads: number) =>
    invoke<ThreadsApplied>("set_worker_threads", { threads }),
  ensureBinary: () => invoke<BinaryStatus>("ensure_binary"),
  getAutostart: () => invoke<boolean>("get_autostart"),
  updateInstallPolicy: () =>
    invoke<UpdateInstallPolicy>("update_install_policy"),
  listModelTiers: () => invoke<ModelTierInfo[]>("list_model_tiers"),
  recommendedTier: () => invoke<string>("recommended_tier"),
  existingModelForTier: (tier: string) =>
    invoke<string | null>("existing_model_for_tier", { tier }),
  downloadModel: (tier: string) => invoke<string>("download_model", { tier }),
  removeModel: (tier: string) => invoke<void>("remove_model", { tier }),
};

export const isTauri = IS_TAURI;
