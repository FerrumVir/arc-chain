// Small engraved pictures for empty states, in place of stock icons when the
// engraved theme is on. Each says what is missing, plainly:
//
//   tablet  no claims yet: a tablet on its plinth, its field not yet cut; the chisel beside it
//   leaves  no logs yet: an open writing tablet with nothing written, and its stylus
//   stones  no blocks: dressed stones waiting on the ground under the outline of an arch not yet built

import { Plate, clamp, mix, type Pt } from "../lib/engrave";
import { band, closed, dashed, ground, masonry, rect, sky } from "../lib/engrave-parts";
import { Engraving } from "./Engraving";

export type VignetteKind = "tablet" | "leaves" | "stones";

const W = 190, H = 92, GY = H - 10;

function tablet(pl: Plate) {
  const cx = W / 2, x0 = cx - 38, x1 = cx + 38, y0 = 22, y1 = 58, ear = 10;
  // plinth
  masonry(pl, rect(cx - 30, y1 + 2, cx + 30, GY), [], [cx - 30, y1 + 2, cx + 30, GY], { block: 12, course: 6, seed: 3 });
  pl.line(closed(rect(cx - 30, y1 + 2, cx + 30, GY)), 0.5, 1);
  // the tabula ansata: dovetail handles, a sunk field left clean
  const out: Pt[] = [[x0, y0], [x1, y0], [x1, y0 + 9], [x1 + ear, y0 + 4], [x1 + ear, y1 - 4], [x1, y1 - 9], [x1, y1], [x0, y1], [x0, y1 - 9], [x0 - ear, y1 - 4], [x0 - ear, y0 + 4], [x0, y0 + 9]];
  pl.mask(out);
  const field = rect(x0 + 4, y0 + 4, x1 - 4, y1 - 4);
  pl.clipHoles([out, field]);
  pl.hatch((u, v) => [mix(x0 - ear, x1 + ear, v), mix(y0, y1, u)], (u, v) => clamp(0.3 + 0.35 * u + 0.25 * v, 0, 1), { lenU: y1 - y0, lenV: x1 - x0 + 2 * ear, sp: 1.05, step: 1.1, angle: 55, t: [0.3, 0.56, 0.74, 0.9], w0: 0.12, w1: 0.5, layer: 3 });
  pl.unclip();
  pl.line(closed(out), 0.7, 1);
  pl.line(closed(field), 0.45, 1);
  pl.seg(x0 + 5, y1 - 5, x1 - 5, y1 - 5, 0.5, 2);
  pl.seg(x1 - 5, y0 + 5, x1 - 5, y1 - 5, 0.5, 2);
  // chisel and mallet on the ground
  pl.line([[cx + 44, GY - 2], [cx + 66, GY - 6]], 1.1, 1);
  pl.line([[cx + 66, GY - 6], [cx + 70, GY - 7]], 0.5, 1);
  const m: Pt[] = [[cx - 70, GY - 1], [cx - 58, GY - 1], [cx - 58, GY - 8], [cx - 70, GY - 8]];
  pl.mask(m);
  pl.hatch((u, v) => [mix(cx - 70, cx - 58, v), mix(GY - 8, GY - 1, u)], (u) => 0.3 + 0.5 * u, { lenU: 7, lenV: 12, sp: 1, step: 1, angle: 70, w0: 0.12, w1: 0.45, layer: 3, families: 2 });
  pl.line(closed(m), 0.5, 1);
  pl.line([[cx - 58, GY - 4.5], [cx - 44, GY - 4]], 0.9, 1);
}

function leaves(pl: Plate) {
  const cx = W / 2, top = 26, bot = GY - 6, w = 40;
  // two hinged leaves lying open, their wax fields smooth and empty
  [[cx - w - 2, cx - 2], [cx + 2, cx + w + 2]].forEach(([a, b], k) => {
    const skew = k ? -3 : 3, poly: Pt[] = [[a + skew, top], [b + skew, top], [b, bot], [a, bot]];
    pl.mask(poly);
    band(pl, Math.min(a, a + skew), Math.max(b, b + skew), top, top + 3, () => 0.5, 85);
    const f: Pt[] = [[a + skew * 0.9 + 4, top + 5], [b + skew * 0.9 - 4, top + 5], [b - 4, bot - 4], [a + 4, bot - 4]];
    pl.clipHoles([poly, f]);
    pl.hatch((u, v) => [mix(a - 4, b + 4, v), mix(top, bot, u)], (u) => 0.4 + 0.3 * u, { lenU: bot - top, lenV: b - a + 8, sp: 1, step: 1.1, angle: 50, t: [0.3, 0.56, 0.74, 0.9], w0: 0.12, w1: 0.46, layer: 3 });
    pl.unclip();
    pl.line(closed(poly), 0.65, 1);
    pl.line(closed(f), 0.4, 1);
  });
  pl.seg(cx, top + 2, cx, bot, 0.8, 1);
  // the stylus
  pl.line([[cx + w + 12, bot - 2], [cx + w + 36, top + 16]], 0.9, 1);
  pl.line([[cx + w + 36, top + 16], [cx + w + 40, top + 12]], 0.4, 1);
}

function stones(pl: Plate) {
  const cx = W / 2, spring = 52, r = 30;
  // the arch that is not built yet: only its outline, dashed, over two stakes
  const outline: Pt[] = [];
  for (let k = 0; k <= 24; k++) { const a = Math.PI - (k * Math.PI) / 24; outline.push([cx + Math.cos(a) * r, spring - Math.sin(a) * r]); }
  dashed(pl, outline, 0.4, 2.5, 3);
  const outer: Pt[] = [];
  for (let k = 0; k <= 24; k++) { const a = Math.PI - (k * Math.PI) / 24; outer.push([cx + Math.cos(a) * (r + 9), spring - Math.sin(a) * (r + 9)]); }
  dashed(pl, outer, 0.3, 2, 3);
  [cx - r - 5, cx + r + 5].forEach((x) => { pl.seg(x, spring, x, GY + 1, 0.6, 2); dashed(pl, [[x - 5, spring], [x + 5, spring]], 0.35, 2, 2); });
  // the dressed stones on the ground, waiting
  [[cx - 20, 0, 0.2], [cx - 4, 0, 0.35], [cx + 12, 0, 0.1], [cx - 12, -9, 0.25]].forEach(([x, dy, tone], k) => {
    const s: Pt[] = [[x - 7, GY + dy], [x + 7, GY + dy], [x + 5.5, GY + dy - 9], [x - 5.5, GY + dy - 9]];
    pl.mask(s);
    pl.hatch((u, v) => [mix(x - 7, x + 7, v), mix(GY + dy - 9, GY + dy, u)], (u, v) => clamp(tone + 0.4 * u + 0.25 * v, 0, 1), { lenU: 9, lenV: 14, sp: 1, step: 1, angle: 60, t: [0.3, 0.56, 0.74, 0.9], w0: 0.12, w1: 0.45, layer: 3 });
    pl.line(closed(s), k === 3 ? 0.7 : 0.55, 1);
  });
}

const DRAW: Record<VignetteKind, (pl: Plate) => void> = { tablet, leaves, stones };

export function Vignette({ kind }: { kind: VignetteKind }) {
  return (
    <div className="vignette" style={{ width: W, maxWidth: "100%" }} aria-hidden="true">
      <Engraving
        height={H}
        cacheKey={`vignette:${kind}`}
        draw={(pl, w) => {
          sky(pl, w, 3, 40, kind.length);
          ground(pl, w, GY, kind.length + 2);
          DRAW[kind](pl);
        }}
      />
    </div>
  );
}
