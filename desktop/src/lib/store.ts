import { create } from "zustand";
import type { Identity, NodeConfig } from "./types";

export type Route =
  | "dashboard"
  | "wallet"
  | "inference"
  | "earnings"
  | "network"
  | "settings"
  | "logs";

/// Selects which inference flow the user gets when they submit a prompt.
/// `coordinator` = legacy off-chain coordinator path (run_consensus +
/// direct fallback). `onchain` = Tier 1 VRF committee voting on-chain.
/// See `arc-chain-docs/TIER1_ONCHAIN_INFERENCE_PLAN.md`.
export type InferenceMode = "coordinator" | "onchain";

interface AppState {
  onboarded: boolean;
  identity: Identity | null;
  config: NodeConfig | null;
  route: Route;
  inferenceMode: InferenceMode;
  /**
   * A tx/attestation hash to prefill into the Network screen's lookup box.
   *
   * This is how "view this attestation" works now. It used to be an
   * `openExternal` to `http://140.82.16.112:3200/tx/<hash>` — a hardcoded LAX
   * IP, pointing at a network dashboard that is not a block explorer, on a
   * host that is usually not the seed this session is actually reading. A
   * lookup against the pinned chain host is the only view that can answer
   * "is my attestation in a block" truthfully.
   */
  pendingLookup: string | null;

  setOnboarded: (v: boolean) => void;
  setIdentity: (i: Identity | null) => void;
  setConfig: (c: NodeConfig | null) => void;
  setRoute: (r: Route) => void;
  setInferenceMode: (m: InferenceMode) => void;
  /** Jump to the Network screen with `hash` loaded into the lookup box. */
  lookupHash: (hash: string) => void;
  /** Consume the prefill so a later visit to Network starts empty. */
  clearPendingLookup: () => void;
}

const STORAGE_KEY = "arc-desktop-state-v1";

/**
 * The only inference flow the UI still offers.
 *
 * The On-chain (Tier 1) radio was removed, but every default here stayed
 * `"onchain"` — so on a fresh install Settings rendered a lone, unselected
 * radio button. Anything persisted as `"onchain"` is coerced on load.
 */
const DEFAULT_INFERENCE_MODE: InferenceMode = "coordinator";

function coerceMode(v: unknown): InferenceMode {
  return v === "coordinator" ? "coordinator" : DEFAULT_INFERENCE_MODE;
}

/**
 * Drop the BIP-39 phrase from anything read out of localStorage.
 *
 * Older builds persisted the full identity — recovery phrase included — in
 * plaintext under this key, where DevTools, any injected script, or anything
 * that can read the WebView profile directory could take it. The backend no
 * longer sends the phrase across the IPC boundary at all, but copies written
 * by previous versions are still sitting on disk, so scrub on load and let
 * the next `persist()` overwrite the stored record without it.
 */
function scrubIdentity(raw: unknown): Identity | null {
  if (!raw || typeof raw !== "object") return null;
  const { address, publicKey, createdAt } = raw as Record<string, unknown>;
  if (typeof address !== "string") return null;
  return {
    address,
    publicKey: typeof publicKey === "string" ? publicKey : "",
    createdAt: typeof createdAt === "number" ? createdAt : 0,
  };
}

const EMPTY = {
  onboarded: false,
  identity: null,
  config: null,
  inferenceMode: DEFAULT_INFERENCE_MODE,
} as const;

function loadInitial(): Pick<
  AppState,
  "onboarded" | "identity" | "config" | "inferenceMode"
> {
  if (typeof localStorage === "undefined") return { ...EMPTY };
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { ...EMPTY };
    const parsed = JSON.parse(raw);
    const identity = scrubIdentity(parsed.identity);
    const loaded = {
      onboarded: !!parsed.onboarded,
      identity,
      config: parsed.config ?? null,
      inferenceMode: coerceMode(parsed.inferenceMode),
    };
    // If the stored blob carried a seed phrase, rewrite it immediately
    // rather than waiting for the next state change to evict it.
    if (parsed.identity && "seedPhrase" in parsed.identity) {
      writeStorage(loaded);
    }
    return loaded;
  } catch {
    return { ...EMPTY };
  }
}

function writeStorage(state: {
  onboarded: boolean;
  identity: Identity | null;
  config: NodeConfig | null;
  inferenceMode: InferenceMode;
}) {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({
        onboarded: state.onboarded,
        // scrubIdentity guarantees no seedPhrase field, but go through it
        // again so a future caller can't reintroduce one by accident.
        identity: state.identity ? scrubIdentity(state.identity) : null,
        config: state.config,
        inferenceMode: state.inferenceMode,
      }),
    );
  } catch {
    /* ignore quota errors */
  }
}

function persist(state: AppState) {
  writeStorage(state);
}

export const useAppStore = create<AppState>((set, get) => ({
  ...loadInitial(),
  route: "dashboard" as Route,
  // Deliberately NOT persisted: a hash the user clicked three days ago is not
  // something to restore into a search box on next launch.
  pendingLookup: null,

  setOnboarded: (v) => {
    set({ onboarded: v });
    persist(get());
  },
  setIdentity: (i) => {
    set({ identity: i });
    persist(get());
  },
  setConfig: (c) => {
    set({ config: c });
    persist(get());
  },
  setRoute: (r) => set({ route: r }),
  setInferenceMode: (m) => {
    set({ inferenceMode: m });
    persist(get());
  },
  lookupHash: (hash) => set({ pendingLookup: hash, route: "network" }),
  clearPendingLookup: () => set({ pendingLookup: null }),
}));
