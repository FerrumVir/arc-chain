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

interface AppState {
  onboarded: boolean;
  identity: Identity | null;
  config: NodeConfig | null;
  route: Route;

  setOnboarded: (v: boolean) => void;
  setIdentity: (i: Identity | null) => void;
  setConfig: (c: NodeConfig | null) => void;
  setRoute: (r: Route) => void;
}

const STORAGE_KEY = "arc-desktop-state-v1";

function loadInitial(): Pick<AppState, "onboarded" | "identity" | "config"> {
  if (typeof localStorage === "undefined") {
    return { onboarded: false, identity: null, config: null };
  }
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { onboarded: false, identity: null, config: null };
    const parsed = JSON.parse(raw);
    return {
      onboarded: !!parsed.onboarded,
      identity: parsed.identity ?? null,
      config: parsed.config ?? null,
    };
  } catch {
    return { onboarded: false, identity: null, config: null };
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
}));
