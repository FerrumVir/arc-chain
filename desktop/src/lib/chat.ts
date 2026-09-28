// The Inference conversation. Kept in memory only: prompts and answers are not
// written to disk, and the thread clears when the app quits. It survives moving
// between screens, so a person can follow a receipt to the Network screen and
// come back to the same thread.
//
// Each turn records only what actually happened: what was sent, which route
// this app itself tried (its own dispatch decisions, not node progress), and
// the node's returned result or error.

import { create } from "zustand";
import type { InferenceResult } from "./types";

/** The route this app last dispatched to, as decided in runInferenceSmart. */
export type DispatchRoute = "local" | "direct" | "consensus";

export interface ChatTurn {
  id: string;
  prompt: string;
  maxTokens: number;
  sentAt: number;
  status: "pending" | "answered" | "failed";
  route: DispatchRoute | null;
  /** The person stopped waiting; a late answer is still attached, and says so. */
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

/**
 * An earlier answered turn with the same prompt and token budget, if any: the
 * only fair basis for comparing output fingerprints within this conversation.
 */
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
