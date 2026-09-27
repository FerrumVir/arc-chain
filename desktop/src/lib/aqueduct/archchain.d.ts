/** The engraved aqueduct (archchain.js). Every stone is a block; see the header of archchain.js. */
export type ArchChainPhase = "survey" | "model" | "rpc" | "peers" | "sync" | "live";
export interface ArchChainStats {
  top: number; final: number; rate: number; batch: number; txPerGlint: number;
  catching: boolean; since: number; arches: number; cutMs: number;
  /** Inference requests fed through queries(), their rate per second over the last half minute, and how many make one surge. */
  queries: number; queryRate: number; queriesPerSurge: number; flow: number;
  /** Chat mode: finished answers (arches). */
  answers?: number;
}
/** Chat mode: an answer already in the conversation. checked: a second computer agreed (true), disagreed (false), or never checked (null). */
export interface ArchChainAnswer { checked: boolean | null; height: number | null }
export interface ArchChainApi {
  init(height: number, finalHeight?: number, opts?: { history?: boolean; batch?: number }): void;
  block(height: number, txCount?: number): void;
  final(height: number): void;
  phase(name: ArchChainPhase, progress?: number | null): void;
  /** n more inference requests served since the last call: the channel's water runs with them, each batch a surge. Call with 0 to mark the feed live while idle. */
  queries(n: number): void;
  /** Chain mode: the heights of the arch under a canvas point (CSS px), or null. Chat mode: the answer index, or -1. */
  hit(x: number, y: number): { from: number; to: number } | number | null;
  // chat mode (mount with { chat: true }): one arch per answer
  load(answers: ArchChainAnswer[]): void;
  /** A prompt was sent: returns the index of the arch that will hold its answer. */
  ask(): number;
  /** Tokens received so far, when the node streams (stones follow them; the keystone waits for answered()). */
  stream(tokens: number): void;
  answered(): void;
  checked(ok: boolean, index?: number): void;
  recorded(height: number, index?: number): void;
  failed(): void;
  stats(): ArchChainStats;
  /** Re-measure the canvas now (it is otherwise watched with a ResizeObserver). */
  resize(): void;
  destroy(): void;
}
export interface ArchChainOptions {
  reduced?: boolean; stonesPerSecond?: number; far?: HTMLElement; near?: HTMLElement;
  /** One arch per answer, running out of a water house, cropped to the aqueduct. */
  chat?: boolean;
  /** Rows of the drawing to fit in the canvas height (defaults suit each mode), bays across the width, tablet font size. */
  top?: number; bot?: number; bays?: number; tabletFont?: number;
  manual?: boolean;
}
declare global {
  interface Window {
    ArchChain: { mount(canvas: HTMLCanvasElement, opts?: ArchChainOptions): ArchChainApi; NV: number };
  }
}
