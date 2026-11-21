import { useEffect, useRef, type CSSProperties } from "react";
import "../lib/aqueduct/engine.js";
import "../lib/aqueduct/archchain.js";
import type { ArchChainApi, ArchChainPhase, ArchChainStats } from "../lib/aqueduct/archchain";

export function Aqueduct({
  height,
  finalHeight,
  blocks,
  phase,
  progress,
  served,
  bot,
  onStats,
  label = "The chain drawn as an aqueduct under construction: each stone is a block; each finished arch shows the heights it holds.",
  className,
  style,
}: {
  height: number | null;
  finalHeight?: number | null;
  blocks?: { height: number; txCount: number | null }[] | null;
  phase?: ArchChainPhase;
  progress?: number | null;
  served?: number | null;
  bot?: number;
  onStats?: (stats: ArchChainStats) => void;
  label?: string;
  className?: string;
  style?: CSSProperties;
}) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const far = useRef<HTMLDivElement>(null);
  const near = useRef<HTMLDivElement>(null);
  const api = useRef<ArchChainApi | null>(null);
  const seeded = useRef(false);
  const fed = useRef(-1);
  const servedSeen = useRef<number | null>(null);
  const statsListener = useRef(onStats);
  statsListener.current = onStats;

  useEffect(() => {
    if (!canvas.current) return;
    const reduced = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;
    api.current = window.ArchChain.mount(canvas.current, {
      reduced,
      far: far.current ?? undefined,
      near: near.current ?? undefined,
      ...(bot != null ? { bot } : {}),
    });
    const timer = window.setInterval(() => {
      const a = api.current;
      if (a && statsListener.current) statsListener.current(a.stats());
    }, 1000);
    return () => {
      window.clearInterval(timer);
      api.current?.destroy();
      api.current = null;
      seeded.current = false;
      fed.current = -1;
      servedSeen.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (phase) api.current?.phase(phase, progress ?? null);
  }, [phase, progress]);

  useEffect(() => {
    const a = api.current;
    if (!a || height == null) return;
    if (!seeded.current) {
      a.init(height, finalHeight ?? height, { history: phase !== "sync" });
      seeded.current = true;
      fed.current = height;
      return;
    }
    const known = new Map<number, number>();
    for (const b of blocks ?? []) if (b.height > fed.current) known.set(b.height, b.txCount ?? 0);
    const heights = Array.from(known.keys()).filter((h) => h <= height).sort((x, y) => x - y);
    for (const h of heights) a.block(h, known.get(h) ?? 0);
    if (height > fed.current) a.block(height, known.get(height) ?? 0);
    fed.current = Math.max(fed.current, height);
    a.final(finalHeight ?? height);
  }, [height, finalHeight, blocks, phase]);

  useEffect(() => {
    const a = api.current;
    if (!a || served == null || !Number.isFinite(served)) return;
    const before = servedSeen.current;
    servedSeen.current = served;
    if (before == null) a.queries(0);
    else if (served > before) a.queries(served - before);
  }, [served]);

  const fill: CSSProperties = { position: "absolute", inset: 0, width: "100%", height: "100%" };
  return (
    <div className={className} style={{ position: "relative", overflow: "hidden", background: "#002dde", ...style }}>
      <div ref={far} style={{ ...fill, pointerEvents: "none" }} aria-hidden="true" />
      <div ref={near} style={{ ...fill, pointerEvents: "none" }} aria-hidden="true" />
      <canvas ref={canvas} style={{ ...fill, display: "block" }} role="img" aria-label={label} />
    </div>
  );
}
