import { create } from "zustand";
import type { InferenceResult } from "./types";

export type DispatchRoute = "local" | "direct" | "consensus";

export interface ChatTurn {
  id: string;
  prompt: string;
  maxTokens: number;
  sentAt: number;
  status: "pending" | "answered" | "failed";
  route: DispatchRoute | null;
  detached: boolean;
  result?: InferenceResult;
  error?: string;
  settledAt?: number;
}

interface ChatState {
  turns: ChatTurn[];
  addTurn: (turn: ChatTurn) => void;
  updateTurn: (id: string, patch: Partial<ChatTurn>) => void;
  clear: () => void;
}

export const useChatStore = create<ChatState>((set) => ({
  turns: [],
  addTurn: (turn) => set((s) => ({ turns: [...s.turns, turn] })),
  updateTurn: (id, patch) =>
    set((s) => ({ turns: s.turns.map((t) => (t.id === id ? { ...t, ...patch } : t)) })),
  clear: () => set({ turns: [] }),
}));

let seq = 0;
export function newTurnId(): string {
  seq += 1;
  return `turn-${Date.now().toString(36)}-${seq}`;
}

export function earlierSameAsk(turns: ChatTurn[], turn: ChatTurn): ChatTurn | null {
  const index = turns.findIndex((t) => t.id === turn.id);
  for (let i = index - 1; i >= 0; i--) {
    const t = turns[i];
    if (
      t.status === "answered" &&
      t.result?.outputHash &&
      t.prompt === turn.prompt &&
      t.maxTokens === turn.maxTokens
    ) {
      return t;
    }
  }
  return null;
}
