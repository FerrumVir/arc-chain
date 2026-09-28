// The website's engraving engine (arc-ai-website/js/engrave/engine.js), copied
// into the app and typed. A plate is an ordered list of strokes (polylines with
// a width at every point) and masks (polygons filled with the ground colour, for
// occlusion). Hatching follows each form, swells in shadow and tapers into
// light, and crosses in up to four layers where it is darkest. White on Arc blue
// reads as a negative print: the more shadow, the more white.
//
// Kept deliberately close to the original so both stay recognisably one hand;
// the site's text items and slice-at-a-time drawing are left out.

export type Pt = [number, number];
export type Dark = (u: number, v: number) => number;
export type Map2 = (u: number, v: number) => Pt;

export interface HatchOptions {
  lenU: number;
  lenV: number;
  sp?: number;
  step?: number;
  angle?: number;
  t?: [number, number, number, number];
  w0?: number;
  w1?: number;
  layer?: number;
  jitter?: number;
  families?: number;
  minSp?: number;
}

interface Stroke { t: 0; p: Float32Array; w: Float32Array; l: number; b: [number, number, number, number] }
interface Mask { t: 1; p: Float32Array; c?: string }
interface Clip { t: 3; p: Float32Array }
interface Unclip { t: 4 }
interface Rings { t: 5 | 6; r: Float32Array[]; c?: string }
type Item = Stroke | Mask | Clip | Unclip | Rings;

export const clamp = (v: number, a: number, b: number) => (v < a ? a : v > b ? b : v);
export const sm = (a: number, b: number, v: number) => {
  const t = clamp((v - a) / (b - a), 0, 1);
  return t * t * (3 - 2 * t);
};
export const mix = (a: number, b: number, t: number) => a + (b - a) * t;
export function rng(seed: number) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
export function hash(i: number, j: number) {
  const s = Math.sin(i * 127.1 + j * 311.7) * 43758.5453;
  return s - Math.floor(s);
}
export function noise(x: number, y: number) {
  const xi = Math.floor(x), yi = Math.floor(y), xf = x - xi, yf = y - yi;
  const u = xf * xf * (3 - 2 * xf), v = yf * yf * (3 - 2 * yf);
  return (hash(xi, yi) * (1 - u) + hash(xi + 1, yi) * u) * (1 - v) + (hash(xi, yi + 1) * (1 - u) + hash(xi + 1, yi + 1) * u) * v;
}
export function fbm(x: number, y: number) {
  return noise(x, y) * 0.55 + noise(x * 2.1, y * 2.1) * 0.28 + noise(x * 4.3, y * 4.3) * 0.17;
}
export function norm(v: [number, number, number]): [number, number, number] {
  const l = Math.hypot(v[0], v[1], v[2]) || 1;
  return [v[0] / l, v[1] / l, v[2] / l];
}
/** Light from the upper left and a little in front, in plate space (x right, y down, z toward the viewer). */
const LS = norm([-0.62, -0.58, 0.52]);
export function lit(nx: number, ny: number, nz: number) {
  return Math.max(0, nx * LS[0] + ny * LS[1] + nz * LS[2]);
}
/** A face of dressed stone, pillowed: bright bevels to the upper left, dark ones to the lower right. */
export function pillow(u: number, v: number, e = 0.15) {
  const du = u < e ? (u - e) / e : u > 1 - e ? (u - 1 + e) / e : 0;
  const dv = v < e ? (v - e) / e : v > 1 - e ? (v - 1 + e) / e : 0;
  const n = norm([du * 0.9, dv * 0.9, 1]);
  return 0.86 - lit(n[0], n[1], n[2]) * 1.05;
}

function flat(pts: Pt[]) {
  const P = new Float32Array(pts.length * 2);
  for (let i = 0; i < pts.length; i++) { P[i * 2] = pts[i][0]; P[i * 2 + 1] = pts[i][1]; }
  return P;
}

export class Plate {
  items: Item[] = [];
  add(pts: Pt[], ws: number | number[], layer = 0) {
    const n = pts.length;
    if (n < 2) return;
    const P = new Float32Array(n * 2), W = new Float32Array(n);
    let x0 = 1e9, y0 = 1e9, x1 = -1e9, y1 = -1e9;
    for (let i = 0; i < n; i++) {
      const [x, y] = pts[i];
      P[i * 2] = x; P[i * 2 + 1] = y; W[i] = typeof ws === "number" ? ws : ws[i];
      if (x < x0) x0 = x; if (x > x1) x1 = x; if (y < y0) y0 = y; if (y > y1) y1 = y;
    }
    this.items.push({ t: 0, p: P, w: W, l: layer, b: [x0, y0, x1, y1] });
  }
  line(pts: Pt[], w: number, layer = 1) { this.add(pts, w, layer); }
  seg(x0: number, y0: number, x1: number, y1: number, w: number, layer = 1) { this.add([[x0, y0], [x1, y1]], w, layer); }
  mask(pts: Pt[], colour?: string) { this.items.push(colour ? { t: 1, p: flat(pts), c: colour } : { t: 1, p: flat(pts) }); }
  maskHoles(list: Pt[][], colour?: string) { this.items.push({ t: 5, r: list.map(flat), c: colour }); }
  clip(pts: Pt[]) { this.items.push({ t: 3, p: flat(pts) }); }
  clipHoles(list: Pt[][]) { this.items.push({ t: 6, r: list.map(flat) }); }
  unclip() { this.items.push({ t: 4 }); }
  arc(cx: number, cy: number, r: number, a0: number, a1: number, w: number, layer = 1, n?: number) {
    const pts: Pt[] = [], k = n || Math.max(8, Math.round((Math.abs(a1 - a0) * r) / 2.5));
    for (let i = 0; i <= k; i++) { const a = a0 + ((a1 - a0) * i) / k; pts.push([cx + Math.cos(a) * r, cy - Math.sin(a) * r]); }
    this.add(pts, w, layer);
  }
  /** A contour whose weight follows the shadow: heavier where the form turns away from the light. */
  contour(pts: Pt[], darkAt: (p: Pt) => number, w0: number, w1: number, layer = 1) {
    this.add(pts, pts.map((p) => mix(w0, w1, clamp(darkAt(p), 0, 1))), layer);
  }
  /** Hatch a patch. pt(u, v) maps the unit square onto the drawing; dark(u, v) is 0 (light) .. 1 (black).
   *  Family one runs along v, spaced along u; three crossing families enter at rising darkness. */
  hatch(pt: Map2, dark: Dark, o: HatchOptions) {
    const sp = o.sp || 1.5, nu = Math.max(2, Math.round(o.lenU / sp)), nv = Math.max(2, Math.round(o.lenV / (o.step || 1.4)));
    const T = o.t || [0.14, 0.42, 0.64, 0.84], w0 = o.w0 ?? 0.16, w1 = o.w1 ?? 0.72, layer = o.layer ?? 3;
    const sl = Math.min(6, (o.lenV / o.lenU) * Math.tan(((o.angle ?? 48) * Math.PI) / 180)), jit = o.jitter ?? 0.25;
    let fams = [{ thr: T[0], k: 0, gap: 1 }, { thr: T[1], k: 1, gap: 1.25 }, { thr: T[2], k: -0.75, gap: 1.5 }, { thr: T[3], k: 2.4, gap: 1.2 }];
    if (o.families) fams = fams.slice(0, o.families);
    fams.forEach((fm, fi) => {
      const du = (1 / nu) * fm.gap, span = Math.abs(fm.k * sl) + 0.001;
      for (let c = fm.k === 0 ? 0 : -span; c <= 1 + span + 1e-9; c += du) {
        let pts: Pt[] = [], ws: number[] = [];
        const flush = () => {
          if (pts.length > 1) {
            const n = pts.length;
            for (let q = 0; q < n; q++) { const e = Math.min(q, n - 1 - q); ws[q] *= e < 3 ? 0.35 + 0.22 * e : 1; }
            this.add(pts, ws, layer + fi * 0.25);
          }
          pts = []; ws = [];
        };
        for (let j = 0; j <= nv; j++) {
          const v = j / nv, u = c + fm.k * sl * v;
          if (u < 0 || u > 1) { flush(); continue; }
          const d = dark(u, v);
          if (d > fm.thr) {
            const p = pt(u, v), jj = (hash(Math.round(c * 997), j) - 0.5) * jit;
            pts.push([p[0] + jj * 0.3, p[1] + jj * 0.3]);
            ws.push(w0 + (w1 - w0) * Math.pow(clamp((d - fm.thr) / (1 - fm.thr), 0, 1), 0.75) * (0.85 + 0.3 * hash(j, Math.round(c * 331))));
          } else flush();
        }
        flush();
      }
    });
  }
  /** Short strokes and dots scattered where dark(x, y) says, for grain and spray. */
  stipple(x0: number, y0: number, x1: number, y1: number, dark: (x: number, y: number) => number, count: number, seed = 1, w = 0.35, layer = 4) {
    const r = rng(seed);
    for (let i = 0; i < count; i++) {
      const x = x0 + r() * (x1 - x0), y = y0 + r() * (y1 - y0), d = dark(x, y);
      if (r() > d) continue;
      const a = r() * Math.PI, l = 0.4 + r() * 0.9;
      this.add([[x, y], [x + Math.cos(a) * l, y + Math.sin(a) * l]], w * (0.7 + d * 0.6), layer);
    }
  }
}

const BINS = [0.26, 0.38, 0.52, 0.7, 0.95, 1.3, 1.8, 2.6];

/** Draw a plate. T maps plate units to CSS px; the canvas is in device px (dpr). */
export function drawPlate(
  g: CanvasRenderingContext2D,
  plate: Plate,
  T: { x: number; y: number; s: number },
  dpr: number,
  opt: { ground?: string; ink?: string; alpha?: number; minW?: number } = {},
) {
  const s = T.s * dpr, ox = T.x * dpr, oy = T.y * dpr, CW = g.canvas.width, CH = g.canvas.height;
  const wz = T.s * dpr, alpha = opt.alpha ?? 1, minW = opt.minW ?? 0.18, ground = opt.ground || "#002dde", ink = opt.ink || "255,255,255";
  let paths = BINS.map(() => new Path2D());
  const used = BINS.map(() => false);
  const flush = () => {
    for (let k = 0; k < BINS.length; k++) if (used[k]) {
      g.lineWidth = BINS[k];
      g.strokeStyle = `rgba(${ink},${(alpha * Math.min(1, 0.55 + BINS[k] * 0.55)).toFixed(3)})`;
      g.stroke(paths[k]);
      paths[k] = new Path2D(); used[k] = false;
    }
  };
  const path = (P: Float32Array) => {
    for (let m = 0; m < P.length; m += 2) { const X = ox + P[m] * s, Y = oy + P[m + 1] * s; if (m) g.lineTo(X, Y); else g.moveTo(X, Y); }
    g.closePath();
  };
  g.lineCap = "round"; g.lineJoin = "round";
  const cl = -20, cr = CW + 20, ct = -20, cb = CH + 20;
  for (const it of plate.items) {
    if (it.t === 3) { flush(); g.save(); g.beginPath(); path(it.p); g.clip(); continue; }
    if (it.t === 4) { flush(); g.restore(); continue; }
    if (it.t === 5 || it.t === 6) {
      flush(); if (it.t === 6) g.save(); g.beginPath();
      it.r.forEach(path);
      if (it.t === 5) { g.fillStyle = it.c || ground; g.fill("evenodd"); } else g.clip("evenodd");
      continue;
    }
    if (it.t === 1) { flush(); g.beginPath(); path(it.p); g.fillStyle = it.c || ground; g.fill(); continue; }
    if (it.t !== 0) continue;
    const b = it.b;
    if (ox + b[2] * s < cl || ox + b[0] * s > cr || oy + b[3] * s < ct || oy + b[1] * s > cb) continue;
    const W = it.w, P = it.p, n = W.length;
    for (let q = 0; q < n - 1; q++) {
      const wd = (W[q] + W[q + 1]) * 0.5 * wz;
      if (wd < minW) continue;
      let k2 = 0; while (k2 < BINS.length - 1 && wd > (BINS[k2] + BINS[k2 + 1]) * 0.5) k2++;
      const x0 = ox + P[q * 2] * s, y0 = oy + P[q * 2 + 1] * s, x1 = ox + P[q * 2 + 2] * s, y1 = oy + P[q * 2 + 3] * s;
      if ((x0 < cl && x1 < cl) || (x0 > cr && x1 > cr) || (y0 < ct && y1 < ct) || (y0 > cb && y1 > cb)) continue;
      paths[k2].moveTo(x0, y0); paths[k2].lineTo(x1, y1); used[k2] = true;
    }
  }
  flush();
  paths = [];
}
