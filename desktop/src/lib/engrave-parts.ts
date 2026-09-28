// Parts the app's engravings share, cut in the site's manner (see lib/engrave.ts): coursed ashlar with
// pillowed blocks, hatched bands, arched openings, and the dashed lines of work not yet built.

import { Plate, clamp, fbm, hash, mix, pillow, rng, type Pt } from "./engrave";

export function rect(x0: number, y0: number, x1: number, y1: number): Pt[] {
  return [[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
}
export function closed(p: Pt[]): Pt[] {
  return p.concat([p[0]]);
}
// an arch-headed opening from the ground up
export function opening(L: number, R: number, spring: number, ground: number): Pt[] {
  const r = (R - L) / 2, c = (L + R) / 2, pts: Pt[] = [[L, ground], [L, spring]];
  for (let k = 1; k < 16; k++) { const a = Math.PI - (k / 16) * Math.PI; pts.push([c + Math.cos(a) * r, spring - Math.sin(a) * r]); }
  pts.push([R, spring], [R, ground]);
  return pts;
}
export function dashed(pl: Plate, pts: Pt[], w: number, on = 3, off = 3) {
  for (let i = 1; i < pts.length; i++) {
    const [ax, ay] = pts[i - 1], [bx, by] = pts[i], L = Math.hypot(bx - ax, by - ay);
    for (let s = 0; s < L; s += on + off) {
      const t0 = s / L, t1 = Math.min(1, (s + on) / L);
      pl.seg(mix(ax, bx, t0), mix(ay, by, t0), mix(ax, bx, t1), mix(ay, by, t1), w, 2);
    }
  }
}

/** Coursed ashlar over a box, clipped to an outline with holes: one hatch, each block pillowed. */
export function masonry(pl: Plate, outer: Pt[], holes: Pt[][], box: [number, number, number, number], o: { course?: number; block?: number; tone?: number; seed?: number } = {}) {
  const [x0, y0, x1, y1] = box, course = o.course ?? 5.4, block = o.block ?? 11, tone = o.tone ?? 0, seed = o.seed ?? 1;
  pl.maskHoles([outer, ...holes]);
  pl.clipHoles([outer, ...holes]);
  pl.hatch(
    (u, v) => [mix(x0, x1, v), mix(y0, y1, u)],
    (u, v) => {
      const x = mix(x0, x1, v), y = mix(y0, y1, u), up = (y1 - y) / course, row = Math.floor(up), fy = up - row;
      const off = (row % 2) * block * 0.5 + (hash(row, seed) - 0.5) * block * 0.3, bx = (x - x0 + off) / block, col = Math.floor(bx);
      return clamp(pillow(bx - col, 1 - fy, 0.22) * 0.9 + tone + 0.16 * (fbm(x * 0.09 + seed, y * 0.16) - 0.5) + 0.07 * (hash(col * 7 + row, seed) - 0.5), 0, 1);
    },
    { lenU: y1 - y0, lenV: x1 - x0, sp: 1.05, step: 1.1, angle: 62, t: [0.3, 0.56, 0.74, 0.9], w0: 0.12, w1: 0.52, layer: 3 },
  );
  for (let up = course, row = 1; y1 - up > y0 + 0.5; up += course, row++) {
    const y = y1 - up;
    pl.line([[x0, y], [x1, y]], 0.24, 2);
    const off = (row % 2) * block * 0.5 + (hash(row, seed) - 0.5) * block * 0.3;
    for (let x = x0 - off + block; x < x1; x += block) pl.seg(x, y, x, Math.min(y1, y + course), 0.22, 2);
  }
  pl.unclip();
}

export function band(pl: Plate, x0: number, x1: number, y0: number, y1: number, dark: (u: number, v: number) => number, angle = 80) {
  pl.mask(rect(x0, y0, x1, y1));
  pl.hatch((u, v) => [mix(x0, x1, v), mix(y0, y1, u)], dark, { lenU: y1 - y0, lenV: x1 - x0, sp: 0.95, step: 1.2, angle, w0: 0.12, w1: 0.55, layer: 3 });
}


/** An engraver's sky: long level strokes, sparse, thinning toward the horizon. */
export function sky(pl: Plate, W: number, y0: number, y1: number, seed = 5) {
  const rs = rng(seed);
  for (let y = y0; y < y1; y += 3.1) {
    const density = 0.5 - ((y - y0) / (y1 - y0)) * 0.42;
    for (let x = rs() * 24; x < W; ) {
      const L = 18 + rs() * 90, e = Math.min(W, x + L);
      if (rs() < density) pl.seg(x, y, e, y + 0.05, 0.14 + 0.1 * (1 - (y - y0) / (y1 - y0)), 6);
      x = e + 6 + rs() * 34;
    }
  }
}

/** Far hills along a horizon, lightly hatched down to the ground line. */
export function hills(pl: Plate, W: number, base: number, ground: number) {
  const hill = (x: number) => base - 7 * Math.sin((x / W) * 6.3 + 1) - 4 * Math.sin((x / W) * 17 + 2) - 2 * Math.sin((x / W) * 41);
  const hp: Pt[] = [];
  for (let x = 0; x <= W; x += 3) hp.push([x, hill(x)]);
  pl.hatch((u, v) => { const x = mix(0, W, v); return [x, mix(hill(x) + 0.6, ground - 1, u)]; }, (u) => 0.2 + 0.22 * u, { lenU: 16, lenV: W, sp: 1.7, step: 3, angle: 18, t: [0.14, 0.6, 0.85, 0.95], w0: 0.1, w1: 0.26, layer: 5, families: 1, jitter: 0.6 });
  pl.line(hp, 0.34, 5);
}

/** A ground line with a few strokes of earth under it. */
export function ground(pl: Plate, W: number, y: number, seed = 3) {
  pl.line([[0, y], [W, y]], 0.55, 2);
  const r = rng(seed);
  for (let i = 0; i < W / 14; i++) { const x = r() * W, yy = y + 1.5 + r() * 5; pl.seg(x, yy, x + 3 + r() * 9, yy, 0.26, 4); }
}

