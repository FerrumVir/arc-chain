import { useEffect, useRef, type CSSProperties } from "react";
import "../lib/aqueduct/engine.js";
import "../lib/aqueduct/archchain.js";
import type { ArchChainApi, ArchChainPhase, ArchChainStats } from "../lib/aqueduct/archchain";

/**
 * The chain drawn as an aqueduct under construction, engraved white on Arc blue. Every stone is a block (or a batch of
 * 2^n blocks when the chain runs fast, so stones are always set at a pace the eye can follow); an arch holds the exact
 * heights cut in its tablet; the timber centering under an arch is struck once its last block is final; water runs
 * over final arches, and it runs with the inference requests the node reports serving. It is driven only by what the
 * node reports: when the chain stops, the building stops and the next stone hangs where it is.
 */
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
  /** The newest block height the node reports. null while unknown. */
  height: number | null;
  /** The newest final height, when the chain reports one. */
  finalHeight?: number | null;
  /** Recent blocks with their transaction counts, newest last or first (order does not matter). */
  blocks?: { height: number; txCount: number | null }[] | null;
  /** Start-up phase while the node is starting; "live" (or undefined) once it is running. */
  phase?: ArchChainPhase;
  /** Real progress for the current phase, 0..1, or null when the node gives none. */
  progress?: number | null;
  /**
   * A running count of inference requests the node reports having served. The first value only marks the feed live
   * (still water); each increase after it is fed to the engine as that many requests. A count that goes down (the
   * node restarted and its counter began again) is taken as the new starting point, never as requests. Leave it
   * undefined when the node reports no such count: nothing is drawn for requests nobody reported.
   */
  served?: number | null;
  /** The lowest row of the drawing to keep in view (plate units); larger values leave ground under the aqueduct. */
  bot?: number;
  /** Called about once a second with what the picture is showing, for a readout beside it. */
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
    // the sky and the ground slide on their own compositor layers; the canvas holds only the aqueduct
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
    // the drawing's framing is fixed for the life of the canvas
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (phase) api.current?.phase(phase, progress ?? null);
  }, [phase, progress]);

  useEffect(() => {
    const a = api.current;
    if (!a || height == null) return;
    if (!seeded.current) {
      // start from where the chain is; while syncing there is no history to show yet
      a.init(height, finalHeight ?? height, { history: phase !== "sync" });
      seeded.current = true;
      fed.current = height;
      return;
    }
    // each block the node has told us about, with its transactions where we know them
    const known = new Map<number, number>();
    for (const b of blocks ?? []) if (b.height > fed.current) known.set(b.height, b.txCount ?? 0);
    const heights = Array.from(known.keys()).filter((h) => h <= height).sort((x, y) => x - y);
    for (const h of heights) a.block(h, known.get(h) ?? 0);
    if (height > fed.current) a.block(height, known.get(height) ?? 0);
    fed.current = Math.max(fed.current, height);
    // without a separate finality signal, a sealed block is treated as final (blocks are sealed from committed rounds)
    a.final(finalHeight ?? height);
  }, [height, finalHeight, blocks, phase]);

  // Inference water: only the requests served since this picture started watching, never a backlog.
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
