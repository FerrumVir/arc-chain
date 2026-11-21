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

export type InferenceMode = "coordinator";

interface AppState {
  onboarded: boolean;
  identity: Identity | null;
  config: NodeConfig | null;
  route: Route;
  inferenceMode: InferenceMode;
  pendingLookup: string | null;

  setOnboarded: (v: boolean) => void;
  setIdentity: (i: Identity | null) => void;
  setConfig: (c: NodeConfig | null) => void;
  setRoute: (r: Route) => void;
  setInferenceMode: (m: InferenceMode) => void;
  lookupHash: (hash: string) => void;
  clearPendingLookup: () => void;
}

const STORAGE_KEY = "arc-desktop-state-v1";

const DEFAULT_INFERENCE_MODE: InferenceMode = "coordinator";

function coerceMode(v: unknown): InferenceMode {
  return v === "coordinator" ? "coordinator" : DEFAULT_INFERENCE_MODE;
}

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
        identity: state.identity ? scrubIdentity(state.identity) : null,
        config: state.config,
        inferenceMode: state.inferenceMode,
      }),
    );
  } catch {
  }
}

function persist(state: AppState) {
  writeStorage(state);
}

const ROUTES: readonly Route[] = [
  "dashboard",
  "wallet",
  "inference",
  "earnings",
  "network",
  "logs",
  "settings",
];

function startRoute(): Route {
  const requested =
    typeof window === "undefined"
      ? undefined
      : (window as Window & { __ARC_START_ROUTE__?: unknown }).__ARC_START_ROUTE__;
  return ROUTES.find((route) => route === requested) ?? "inference";
}

export const useAppStore = create<AppState>((set, get) => ({
  ...loadInitial(),
  route: startRoute(),
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
