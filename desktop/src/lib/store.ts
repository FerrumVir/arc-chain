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

  setOnboarded: (v: boolean) => void;
  setIdentity: (i: Identity | null) => void;
  setConfig: (c: NodeConfig | null) => void;
  setRoute: (r: Route) => void;
  setInferenceMode: (m: InferenceMode) => void;
}

const STORAGE_KEY = "arc-desktop-state-v1";

function loadInitial(): Pick<
  AppState,
  "onboarded" | "identity" | "config" | "inferenceMode"
> {
  if (typeof localStorage === "undefined") {
    return {
      onboarded: false,
      identity: null,
      config: null,
      inferenceMode: "coordinator",
    };
  }
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw)
      return {
        onboarded: false,
        identity: null,
        config: null,
        inferenceMode: "coordinator",
      };
    const parsed = JSON.parse(raw);
    return {
      onboarded: !!parsed.onboarded,
      identity: parsed.identity ?? null,
      config: parsed.config ?? null,
      inferenceMode:
        parsed.inferenceMode === "onchain" ? "onchain" : "coordinator",
    };
  } catch {
    return {
      onboarded: false,
      identity: null,
      config: null,
      inferenceMode: "coordinator",
    };
  }
}

function persist(state: AppState) {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({
        onboarded: state.onboarded,
        identity: state.identity,
        config: state.config,
        inferenceMode: state.inferenceMode,
      }),
    );
  } catch {
    /* ignore quota errors */
  }
}

export const useAppStore = create<AppState>((set, get) => ({
  ...loadInitial(),
  route: "dashboard" as Route,

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
}));
