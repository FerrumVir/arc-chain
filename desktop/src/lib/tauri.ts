// Thin Tauri IPC wrapper with a mock-mode fallback used in the browser (dev + Playwright).
// Production: invoke the real Tauri command. Mock: return synthetic data so every screen
// is visible without a running node.

import type {
  AccountBalance,
  Attestation,
  BinaryStatus,
  Earnings,
  FaucetResult,
  HardwareInfo,
  Identity,
  InferenceResult,
  LogEntry,
  ModelTierInfo,
  NetworkStats,
  NodeConfig,
  NodeStatus,
  PaidInferenceResult,
  ResetPeerStateResult,
  SavedLogs,
  ThreadsApplied,
  Tier1Result,
  Tier1Submitted,
  Tier1Vote,
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

const REWARD_PER_ATTESTATION = 2.5;

/** Mirrors commands.rs::COORDINATOR_HOSTS, NYC included. */
const COORDINATOR_HOSTS = [
  "http://149.28.32.76:9090", // NYC
  "http://140.82.16.112:9090", // LAX
  "http://136.244.109.1:9090", // AMS
  "http://104.238.171.11:9090", // LHR
  "http://202.182.107.41:9090", // NRT
  "http://149.28.153.31:9090", // SGP
];

async function liveInvoke<T>(cmd: string, args?: unknown): Promise<T> {
  const base = liveBase()!;
  const fetchJson = async (path: string) => {
    const r = await fetch(`${base}${path}`);
    if (!r.ok) throw new Error(`${path} → ${r.status}`);
    return r.json();
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
        estimatedDailyArc: 180,
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
      // No synthesized "today" (was total × 12%) and no invented "pending"
      // (was a flat 2.5). This path genuinely does not know either, and
      // null renders as "—" rather than a confident number.
      try {
        const r = await fetchJson("/inference/results?limit=1");
        const count = r.count ?? 0;
        return {
          totalArc: count * REWARD_PER_ATTESTATION,
          todayArc: null,
          pendingArc: null,
          rank: null,
          attestations: count,
          lastPayoutAt: null,
          lastPayoutBlock: null,
          fromChain: false,
        } as T;
      } catch {
        return {
          totalArc: 0,
          todayArc: null,
          pendingArc: null,
          rank: null,
          attestations: 0,
          lastPayoutAt: null,
          lastPayoutBlock: null,
          fromChain: false,
        } as T;
      }
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
              rewardArc: mine ? REWARD_PER_ATTESTATION : null,
              timestamp: num(v, "timestamp"),
              blockHeight: num(v, "block_height"),
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
          balance: 0,
          nonce: 0,
          stakedBalance: 0,
        } as T;
      }
      try {
        const r = await fetch(`${base}/account/${addr}`);
        if (r.status === 404) {
          return { address: addr, balance: 0, nonce: 0, stakedBalance: 0 } as T;
        }
        const v = await r.json();
        return {
          address: v.address ?? addr,
          balance: v.balance ?? 0,
          nonce: v.nonce ?? 0,
          stakedBalance: v.staked_balance ?? 0,
        } as T;
      } catch {
        return { address: addr, balance: 0, nonce: 0, stakedBalance: 0 } as T;
      }
    }
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
      return {
        txHash: v.tx_hash,
        amount: v.amount,
        message: v.message,
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
      // Paid-inference signs and submits an on-chain tx; the live browser
      // mode doesn't carry the payer's private key and can't represent
      // that flow honestly. Surface a clear error rather than synthesize
      // fake tx hashes the user might take as real.
      throw new Error(
        "run_paid_inference requires the Tauri native app (signing + tx submission)",
      );
    }
    case "tier1_submit": {
      const { prompt, maxTokens, maxReward, deadlineBlocks, committeeSize } =
        args as {
          prompt: string;
          maxTokens?: number;
          maxReward?: number;
          deadlineBlocks?: number;
          committeeSize?: number;
        };
      const r = await fetch(`${base}/inference/onchain/submit`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          input: prompt,
          max_tokens: maxTokens ?? 32,
          max_reward: maxReward ?? 10,
          deadline_blocks: deadlineBlocks ?? 20,
          committee_size: committeeSize ?? 5,
        }),
      });
      if (!r.ok) throw new Error(`tier1_submit → HTTP ${r.status}`);
      const v = await r.json();
      return {
        requestId: v.request_id,
        txHash: v.tx_hash,
        anchorHeight: v.anchor_height,
        committeeSize: v.committee_size,
        deadlineBlocks: v.deadline_blocks,
        maxReward: v.max_reward,
      } as T;
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
    case "list_model_tiers":
      return [
        {
          id: "tiny",
          displayName: "TinyLlama 1.1B (Q4_K_M)",
          sizeBytes: 669_262_336,
          url: "https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf",
        },
        {
          id: "standard",
          displayName: "Llama-2 7B Chat (Q4_K_M)",
          sizeBytes: 4_081_004_544,
          url: "https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/main/llama-2-7b-chat.Q4_K_M.gguf",
        },
        {
          id: "big",
          displayName: "Llama-2 13B Chat (Q4_K_M)",
          sizeBytes: 7_866_070_016,
          url: "https://huggingface.co/TheBloke/Llama-2-13B-chat-GGUF/resolve/main/llama-2-13b-chat.Q4_K_M.gguf",
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
let mockEarnings: Earnings = {
  totalArc: 12_847.5,
  todayArc: 247.12,
  pendingArc: 18.4,
  rank: 147,
  attestations: 1283,
  lastPayoutAt: Date.now() - 1000 * 60 * 12,
  lastPayoutBlock: 123_462,
  fromChain: true,
};

// The mock deliberately exercises BOTH attestation shapes the UI must
// survive: rows that are the user's own (with a reward, tokens and a real
// timestamp) and a row from another validator with no telemetry — which is
// what the live seeds actually return today.
const MOCK_ADDRESS = "arc1qxywa87m9v3kz8n2p5nc4z8y7dv4q3lns8z3p";

const mockAttestations: Attestation[] = [
  {
    txHash: "0xe0c73bb8a4446f23a62033001cb22e1e9298d5ce1cfea8111762c1ca2833f67d",
    inputPreview: "What is the largest planet in our solar system?",
    outputHash: "0xe0c73bb8a4446f23a62033001cb22e1e",
    modelHash: "0xabec2d582beb97a876c21d7ccc5e8e48",
    tokens: 42,
    latencyMs: 147,
    rewardArc: REWARD_PER_ATTESTATION,
    timestamp: Date.now() - 1000 * 34,
    blockHeight: 123_462,
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
    rewardArc: REWARD_PER_ATTESTATION,
    timestamp: Date.now() - 1000 * 89,
    blockHeight: 123_455,
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
    rewardArc: null,
    timestamp: null,
    blockHeight: 123_401,
    from: "0cda729e004c87fd15efc6b859ab567bbaba82ba95bdcf5f026082e0865e938e",
    mine: false,
    verified: true,
  },
];

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
  recommendedModel: "Llama-2-13B Q4_K_M (7.3 GB)",
  recommendedRole: "worker",
  estimatedDailyArc: 420,
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
        version: "0.7.11",
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
      if (mockStartedAt) {
        const elapsed = (Date.now() - mockStartedAt) / 1000;
        mockEarnings = {
          ...mockEarnings,
          todayArc: 247.12 + elapsed * 0.05,
          totalArc: 12_847.5 + elapsed * 0.05,
        };
      }
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
    case "open_external":
      return undefined as T;
    case "fetch_balance":
      return {
        address: "fakehex0000000000000000000000000000000000000000000000000000000000",
        balance: 28_500,
        nonce: 3,
        stakedBalance: 0,
      } as T;
    case "faucet_claim":
      return {
        txHash:
          "0x8f31fe12aab4c7d2e44a88b1f91023abfe23bb8a4446f23a62033001cb22e1e9",
        amount: 10_000,
        message: "Sent 10000 ARC to fakehex…",
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
          "  Mock coordinator response - browser preview. In Tauri + live testnet, this is served by one of the 6 seed nodes via /inference/run_consensus with k=3 majority verification.",
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
          "  Mock direct-coordinator response - browser preview. In Tauri + live testnet, this hits a single coordinator's /inference/run as a fallback when the sharded /inference/run_consensus path is degraded.",
        outputHash:
          "0xe5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5",
        modelHash:
          "0xabec2d582beb97a876c21d7ccc5e8e4833e8fd34aee0cb5b64e9f14f5ea57fdb",
        tokensGenerated: 28,
        inferenceMs: 7_800,
        txHash: "0xfafafafafafafafafafafafafafafafafafafafafafafafafafafafafafafafafa",
        deterministic: true,
        engine: "INT8 integer (cross-platform deterministic)",
        explorerUrl: "/tx/0xfafafafafafafafafafafafafafafafafafafafafafafafafafafafafafafafafa",
        consensus: undefined,
        coordinator: "http://140.82.16.112:9090",
        servedLocally: false,
      } as T;
    }
    case "tier1_submit": {
      await new Promise((r) => setTimeout(r, 300));
      const requestId =
        "0x" +
        [...crypto.getRandomValues(new Uint8Array(32))]
          .map((b) => b.toString(16).padStart(2, "0"))
          .join("");
      return {
        requestId,
        txHash:
          "0x" +
          [...crypto.getRandomValues(new Uint8Array(32))]
            .map((b) => b.toString(16).padStart(2, "0"))
            .join(""),
        anchorHeight: 12345,
        committeeSize: 5,
        deadlineBlocks: 20,
        maxReward: 10,
      } as T;
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
      const { prompt, maxFee } = args as {
        prompt: string;
        maxTokens?: number;
        maxFee?: number;
      };
      await new Promise((r) => setTimeout(r, 1500));
      return {
        input: prompt,
        output:
          "  Mock paid-inference response - browser preview. In the native app, this flow: signs an InferenceEscrowOpen tx locally, POSTs it to a coordinator, waits for commit, then runs /inference/run_consensus which auto-submits the release.",
        outputHash:
          "0xd3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3",
        tokensGenerated: 28,
        inferenceMs: 22_100,
        coordinator: "http://149.28.32.76:9090",
        consensus: {
          k: 3,
          votesTotal: 48,
          unanimous: 48,
          majority: 0,
          split: 0,
          divergentReplicaCount: 0,
        },
        payerAddress:
          "0xaabbccddeeff00112233445566778899aabbccddeeff00112233445566778899",
        maxFee: maxFee ?? 10_000,
        openTxHash:
          "0x0open111111111111111111111111111111111111111111111111111111111111",
        releaseTxHash:
          "0x0release1111111111111111111111111111111111111111111111111111111",
      } as T;
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
    case "list_model_tiers":
      return [
        {
          id: "tiny",
          displayName: "TinyLlama 1.1B (Q4_K_M)",
          sizeBytes: 669_262_336,
          url: "https://huggingface.co/TheBloke/TinyLlama-1.1B-Chat-v1.0-GGUF/resolve/main/tinyllama-1.1b-chat-v1.0.Q4_K_M.gguf",
        },
        {
          id: "standard",
          displayName: "Llama-2 7B Chat (Q4_K_M)",
          sizeBytes: 4_081_004_544,
          url: "https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/main/llama-2-7b-chat.Q4_K_M.gguf",
        },
        {
          id: "big",
          displayName: "Llama-2 13B Chat (Q4_K_M)",
          sizeBytes: 7_866_070_016,
          url: "https://huggingface.co/TheBloke/Llama-2-13B-chat-GGUF/resolve/main/llama-2-13b-chat.Q4_K_M.gguf",
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
  fetchBalance: () => invoke<AccountBalance>("fetch_balance"),
  faucetClaim: () => invoke<FaucetResult>("faucet_claim"),
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
  // ── Tier 1 on-chain inference ────────────────────────────────────────────
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
  listModelTiers: () => invoke<ModelTierInfo[]>("list_model_tiers"),
  recommendedTier: () => invoke<string>("recommended_tier"),
  existingModelForTier: (tier: string) =>
    invoke<string | null>("existing_model_for_tier", { tier }),
  downloadModel: (tier: string) => invoke<string>("download_model", { tier }),
  removeModel: (tier: string) => invoke<void>("remove_model", { tier }),
};

export const isTauri = IS_TAURI;
