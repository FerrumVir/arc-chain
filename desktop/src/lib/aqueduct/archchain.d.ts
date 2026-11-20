export type ArchChainPhase = "survey" | "model" | "rpc" | "peers" | "sync" | "live";
export interface ArchChainStats {
  top: number; final: number; rate: number; batch: number; txPerGlint: number;
  catching: boolean; since: number; arches: number; cutMs: number;
  queries: number; queryRate: number; queriesPerSurge: number; flow: number;
  answers?: number;
}
export interface ArchChainAnswer { checked: boolean | null; height: number | null }
export interface ArchChainApi {
  init(height: number, finalHeight?: number, opts?: { history?: boolean; batch?: number }): void;
  block(height: number, txCount?: number): void;
  final(height: number): void;
  phase(name: ArchChainPhase, progress?: number | null): void;
  queries(n: number): void;
  hit(x: number, y: number): { from: number; to: number } | number | null;
  load(answers: ArchChainAnswer[]): void;
  ask(): number;
  stream(tokens: number): void;
  answered(): void;
  checked(ok: boolean, index?: number): void;
  recorded(height: number, index?: number): void;
  failed(): void;
  stats(): ArchChainStats;
  resize(): void;
  destroy(): void;
}
export interface ArchChainOptions {
  reduced?: boolean; stonesPerSecond?: number; far?: HTMLElement; near?: HTMLElement;
  chat?: boolean;
  top?: number; bot?: number; bays?: number; tabletFont?: number;
  manual?: boolean;
}
declare global {
  interface Window {
    ArchChain: { mount(canvas: HTMLCanvasElement, opts?: ArchChainOptions): ArchChainApi; NV: number };
  }
}
