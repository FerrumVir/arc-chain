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
  NetworkStats,
  NodeConfig,
  NodeStatus,
} from "./types";

const IS_TAURI =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

// Set by vite.config.ts: true only inside the production Tauri bundle. A
// production build served off a web host has this true but no Tauri
// internals — in that case mockInvoke() refuses to run to prevent a user
// from opening DevTools and fabricating node state.
declare const __ARC_PROD_TAURI__: boolean;
const IS_PROD_TAURI_BUNDLE =
  typeof __ARC_PROD_TAURI__ !== "undefined" && __ARC_PROD_TAURI__;

// Live mode — used by E2E tests against a real running arc-node (typically on
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
        seedPhrase:
          "galaxy stellar quantum horizon crystal ember aurora silent mirror ocean celestial fragment",
        createdAt: Date.now(),
      } as T;
    case "load_identity":
      return null as T;
    case "save_config":
      return undefined as T;
    case "load_config":
      return null as T;
    case "node_status": {
      try {
        const h = await fetchJson("/health");
        const peers = h.peers ?? 0;
        const uptime = h.uptime_secs ?? 0;
        const health =
          peers === 0 || uptime < 8 ? "syncing" : "live";
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
          rpcPort: Number((window as Window & { __ARC_LIVE__?: number }).__ARC_LIVE__),
          lastError: null,
        } as T;
      } catch {
        return {
          running: false,
          pid: null,
          health: "offline",
          version: "unknown",
          peers: 0,
          round: 0,
          committed: 0,
          height: 0,
          uptimeSeconds: 0,
          address: null,
          rpcPort: Number((window as Window & { __ARC_LIVE__?: number }).__ARC_LIVE__),
          lastError: "No response",
        } as T;
      }
    }
    case "fetch_earnings": {
      try {
        const r = await fetchJson("/inference/results?limit=1");
        const count = r.count ?? 0;
        return {
          totalArc: count * REWARD_PER_ATTESTATION,
          todayArc: Math.round(count * 0.12) * REWARD_PER_ATTESTATION,
          pendingArc: 2.5,
          rank: null,
          attestations: count,
          lastPayoutAt: Date.now() - 60_000,
        } as T;
      } catch {
        return {
          totalArc: 0,
          todayArc: 0,
          pendingArc: 0,
          rank: null,
          attestations: 0,
          lastPayoutAt: null,
        } as T;
      }
    }
    case "fetch_attestations": {
      try {
        const limit = (args as { limit?: number } | undefined)?.limit ?? 20;
        const r = await fetchJson(`/inference/attestations?limit=${limit}`);
        const arr = (r.attestations ?? []) as Array<{
          tx_hash: string;
          success: boolean;
          inference: {
            input: string;
            output_hash: string;
            model_hash: string;
            tokens_generated: number;
            ms_per_token: number;
          };
        }>;
        const now = Date.now();
        return arr.map((v, i) => ({
          txHash: v.tx_hash,
          inputPreview: (v.inference?.input ?? "")
            .replace("[INST] ", "")
            .replace(" [/INST]", "")
            .slice(0, 140),
          outputHash: v.inference?.output_hash ?? "",
          modelHash: v.inference?.model_hash ?? "",
          tokens: v.inference?.tokens_generated ?? 0,
          latencyMs:
            (v.inference?.tokens_generated ?? 0) *
            (v.inference?.ms_per_token ?? 0),
          rewardArc: REWARD_PER_ATTESTATION,
          timestamp: now - i * 30_000,
          verified: !!v.success,
        })) as T;
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
      const { prompt, maxTokens } = args as {
        prompt: string;
        maxTokens?: number;
      };
      const wrapped = prompt.includes("[INST]")
        ? prompt
        : `[INST] ${prompt} [/INST]`;
      const r = await fetch(`${base}/inference/run`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ input: wrapped, max_tokens: maxTokens ?? 32 }),
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
      } as T;
    }
    case "open_external":
      window.open((args as { url: string }).url, "_blank");
      return undefined as T;
    case "clear_crash":
      return undefined as T;
    case "check_for_update":
      return { hasUpdate: false, version: "0.5.2" } as T;
    case "ensure_binary":
      // Browser (live mode) can't install a native binary — pretend it's
      // already installed so the UI doesn't block onboarding.
      return {
        path: "/browser-live-mode",
        downloadedBytes: 0,
        totalBytes: 0,
        alreadyInstalled: true,
      } as T;
    default:
      throw new Error(`Unhandled live command: ${cmd}`);
  }
}

// Mock state — only used in browser preview. Tauri env always hits the real backend.
let mockStartedAt: number | null = null;
const mockLogs: LogEntry[] = [];
let mockEarnings: Earnings = {
  totalArc: 12_847.5,
  todayArc: 247.12,
  pendingArc: 18.4,
  rank: 147,
  attestations: 1283,
  lastPayoutAt: Date.now() - 1000 * 60 * 12,
};

const mockAttestations: Attestation[] = [
  {
    txHash: "0xe0c73bb8a4446f23a62033001cb22e1e9298d5ce1cfea8111762c1ca2833f67d",
    inputPreview: "What is the largest planet in our solar system?",
    outputHash: "0xe0c73bb8a4446f23a62033001cb22e1e",
    modelHash: "0xabec2d582beb97a876c21d7ccc5e8e48",
    tokens: 42,
    latencyMs: 147,
    rewardArc: 12.5,
    timestamp: Date.now() - 1000 * 34,
    verified: true,
  },
  {
    txHash: "0xa9fe23bb8a4446f23a62033001cb22e1e9298d5ce1cfea8111762c1ca28336de",
    inputPreview: "Write a Rust function to compute BLAKE3 of a file",
    outputHash: "0x7c31fe12aab4c7d2e44a88b1f91023ab",
    modelHash: "0xabec2d582beb97a876c21d7ccc5e8e48",
    tokens: 128,
    latencyMs: 412,
    rewardArc: 34.8,
    timestamp: Date.now() - 1000 * 89,
    verified: true,
  },
  {
    txHash: "0x14ab23bb8a4446f23a62033001cb22e1e9298d5ce1cfea8111762c1ca28335f2",
    inputPreview: "Explain zero-knowledge proofs to a 10 year old",
    outputHash: "0xbe91fe12aab4c7d2e44a88b1f91023c8",
    modelHash: "0xabec2d582beb97a876c21d7ccc5e8e48",
    tokens: 256,
    latencyMs: 783,
    rewardArc: 67.2,
    timestamp: Date.now() - 1000 * 214,
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
        address: "arc1qxywa87m9v3kz8n2p5nc4z8y7dv4q3lns8z3p",
        publicKey:
          "0x7c31fe12aab4c7d2e44a88b1f91023abfe23bb8a4446f23a62033001cb22e1e9",
        seedPhrase:
          "galaxy stellar quantum horizon crystal ember aurora silent mirror ocean celestial fragment",
        createdAt: Date.now(),
      } as T;
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
        version: "0.5.2",
        peers: running ? 8 : 0,
        round: running ? 43_821 + Math.floor(uptime / 4) : 0,
        committed: running ? 43_820 + Math.floor(uptime / 4) : 0,
        height: running ? 43_820 + Math.floor(uptime / 4) : 0,
        uptimeSeconds: uptime,
        address: "arc1qxywa87m9v3kz8n2p5nc4z8y7dv4q3lns8z3p",
        rpcPort: 9944,
        lastError: null,
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
        input: `[INST] ${prompt} [/INST]`,
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
      } as T;
    }
    case "clear_crash":
      return undefined as T;
    case "check_for_update":
      return { hasUpdate: false, version: "0.5.2" } as T;
    case "ensure_binary":
      // Mock path — no real download. Pretend it completed instantly.
      return {
        path: "/mock/.arc/bin/arc-node",
        downloadedBytes: 45_000_000,
        totalBytes: 45_000_000,
        alreadyInstalled: false,
      } as T;
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

// Typed wrappers — call these, not invoke() directly.

export const api = {
  detectHardware: () => invoke<HardwareInfo>("detect_hardware"),
  generateIdentity: () => invoke<Identity>("generate_identity"),
  loadIdentity: () => invoke<Identity | null>("load_identity"),
  saveConfig: (config: NodeConfig) => invoke<void>("save_config", { config }),
  loadConfig: () => invoke<NodeConfig | null>("load_config"),
  startNode: (config: NodeConfig) => invoke<void>("start_node", { config }),
  stopNode: () => invoke<void>("stop_node"),
  restartNode: () => invoke<void>("restart_node"),
  nodeStatus: () => invoke<NodeStatus>("node_status"),
  fetchEarnings: () => invoke<Earnings>("fetch_earnings"),
  fetchAttestations: (limit = 20) =>
    invoke<Attestation[]>("fetch_attestations", { limit }),
  fetchLogs: (limit = 200) => invoke<LogEntry[]>("fetch_logs", { limit }),
  fetchNetworkStats: () => invoke<NetworkStats>("fetch_network_stats"),
  fetchBalance: () => invoke<AccountBalance>("fetch_balance"),
  faucetClaim: () => invoke<FaucetResult>("faucet_claim"),
  runInference: (prompt: string, maxTokens = 32) =>
    invoke<InferenceResult>("run_inference", { prompt, maxTokens }),
  clearCrash: () => invoke<void>("clear_crash"),
  openExternal: (url: string) => invoke<void>("open_external", { url }),
  checkForUpdate: () =>
    invoke<{ hasUpdate: boolean; version: string }>("check_for_update"),
  ensureBinary: () => invoke<BinaryStatus>("ensure_binary"),
};

export const isTauri = IS_TAURI;
