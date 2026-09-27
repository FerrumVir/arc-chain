// A canvas that prints an engraving (lib/engrave.ts) at the size it is shown,
// sharp at any device pixel ratio. The picture is cut once per size, theme and
// state, and cut pictures are cached, so a thread of answers that share a state
// costs one engraving, not one each.

import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { Plate, drawPlate } from "../lib/engrave";
import { useTheme } from "../lib/theme";

const cache = new Map<string, HTMLCanvasElement>();
const CACHE_LIMIT = 40;

export function useElementWidth<T extends HTMLElement>() {
  const ref = useRef<T>(null);
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    setWidth(Math.round(el.getBoundingClientRect().width));
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver((entries) => {
      const w = Math.round(entries[0]?.contentRect.width ?? 0);
      setWidth((prev) => (prev === w ? prev : w));
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  return [ref, width] as const;
}

export function Engraving({
  height,
  draw,
  cacheKey,
  className,
  children,
}: {
  height: number;
  /** Engrave into pl, in CSS pixels, for a picture w x h. */
  draw: (pl: Plate, w: number, h: number) => void;
  /** Everything the picture depends on besides its size and the theme. */
  cacheKey: string;
  className?: string;
  children?: ReactNode;
}) {
  const [wrap, width] = useElementWidth<HTMLDivElement>();
  const canvas = useRef<HTMLCanvasElement>(null);
  const theme = useTheme();

  useEffect(() => {
    const c = canvas.current, el = wrap.current;
    if (!c || !el || width < 40) return;
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const ground = getComputedStyle(el).getPropertyValue("--engrave-ground").trim() || "#002dde";
    const key = `${cacheKey}|${width}|${height}|${dpr}|${ground}|${theme}`;
    c.width = Math.round(width * dpr);
    c.height = Math.round(height * dpr);
    const g = c.getContext("2d");
    if (!g) return;
    g.clearRect(0, 0, c.width, c.height);
    let cut = cache.get(key);
    if (!cut) {
      cut = document.createElement("canvas");
      cut.width = c.width;
      cut.height = c.height;
      const cg = cut.getContext("2d");
      if (!cg) return;
      const pl = new Plate();
      draw(pl, width, height);
      drawPlate(cg, pl, { x: 0, y: 0, s: 1 }, dpr, { ground });
      cache.set(key, cut);
      if (cache.size > CACHE_LIMIT) cache.delete(cache.keys().next().value as string);
    }
    g.drawImage(cut, 0, 0);
    // draw is a pure function of cacheKey, size and theme
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cacheKey, width, height, theme]);

  return (
    <div ref={wrap} className={className} style={{ position: "relative", height }}>
      <canvas ref={canvas} aria-hidden="true" style={{ display: "block", width: "100%", height }} />
      {children}
    </div>
  );
}

export function prefersReducedMotion(): boolean {
  return typeof window !== "undefined" && typeof window.matchMedia === "function"
    ? window.matchMedia("(prefers-reduced-motion: reduce)").matches
    : false;
}
