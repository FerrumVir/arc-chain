/* Round 6: an engraving engine. A plate is an ordered list of strokes (polylines with a width at every point) and
   masks (polygons filled with the ground colour, for occlusion). Forms are modelled under one light; hatching
   follows each form, swells in shadow and tapers into light, and crosses in up to four layers where it is darkest.
   White on Arc blue reads as a negative print: the more shadow, the more white. */
(function () {
  'use strict';
  var PI = Math.PI;
  function clamp(v, a, b) { return v < a ? a : v > b ? b : v; }
  function sm(a, b, v) { var t = clamp((v - a) / (b - a), 0, 1); return t * t * (3 - 2 * t); }
  function mix(a, b, t) { return a + (b - a) * t; }
  function rng(seed) { var a = seed >>> 0; return function () { a = (a + 0x6D2B79F5) | 0; var t = Math.imul(a ^ (a >>> 15), 1 | a); t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t; return ((t ^ (t >>> 14)) >>> 0) / 4294967296; }; }
  function hash(i, j) { var s = Math.sin(i * 127.1 + j * 311.7) * 43758.5453; return s - Math.floor(s); }
  function noise(x, y) { var xi = Math.floor(x), yi = Math.floor(y), xf = x - xi, yf = y - yi, u = xf * xf * (3 - 2 * xf), v = yf * yf * (3 - 2 * yf);
    return (hash(xi, yi) * (1 - u) + hash(xi + 1, yi) * u) * (1 - v) + (hash(xi, yi + 1) * (1 - u) + hash(xi + 1, yi + 1) * u) * v; }
  function fbm(x, y) { return noise(x, y) * 0.55 + noise(x * 2.1, y * 2.1) * 0.28 + noise(x * 4.3, y * 4.3) * 0.17; }
  function norm(v) { var l = Math.hypot(v[0], v[1], v[2]) || 1; return [v[0] / l, v[1] / l, v[2] / l]; }
  var LIGHT = norm([-0.58, 0.6, 0.55]);
  function lit(n) { return Math.max(0, n[0] * LIGHT[0] + n[1] * LIGHT[1] + n[2] * LIGHT[2]); }
  // darkness of a surface with normal n (x right, y up, z toward the viewer), with a floor of ambient light
  function shade(n, amb) { return clamp(1 - (amb == null ? 0.12 : amb) - lit(n) * 1.05, 0, 1); }

  function Plate(w, h) { this.w = w; this.h = h; this.items = []; this.nPts = 0; }
  Plate.prototype.add = function (pts, ws, layer) {
    var n = pts.length; if (n < 2) return;
    var P = new Float32Array(n * 2), W = new Float32Array(n), x0 = 1e9, y0 = 1e9, x1 = -1e9, y1 = -1e9, len = 0;
    for (var i = 0; i < n; i++) {
      var x = pts[i][0], y = pts[i][1]; P[i * 2] = x; P[i * 2 + 1] = y; W[i] = typeof ws === 'number' ? ws : ws[i];
      if (x < x0) x0 = x; if (x > x1) x1 = x; if (y < y0) y0 = y; if (y > y1) y1 = y;
      if (i) len += Math.hypot(x - pts[i - 1][0], y - pts[i - 1][1]);
    }
    this.items.push({ t: 0, p: P, w: W, l: layer || 0, b: [x0, y0, x1, y1], len: len });
    this.nPts += n;
  };
  Plate.prototype.line = function (pts, w, layer) { this.add(pts, w, layer); };
  Plate.prototype.seg = function (x0, y0, x1, y1, w, layer) { this.add([[x0, y0], [x1, y1]], w, layer); };
  // colour: a fill other than the ground, for a surface that has its own tone (a screen's glass)
  Plate.prototype.mask = function (pts, colour) {
    var P = new Float32Array(pts.length * 2); for (var i = 0; i < pts.length; i++) { P[i * 2] = pts[i][0]; P[i * 2 + 1] = pts[i][1]; }
    this.items.push(colour ? { t: 1, p: P, c: colour } : { t: 1, p: P });
  };
  function rings(list) { return list.map(function (pts) { var P = new Float32Array(pts.length * 2); for (var i = 0; i < pts.length; i++) { P[i * 2] = pts[i][0]; P[i * 2 + 1] = pts[i][1]; } return P; }); }
  // a mask or a clip with holes: the first ring is the outline, the rest are cut out of it
  Plate.prototype.maskHoles = function (list, colour) { this.items.push(colour ? { t: 5, r: rings(list), c: colour } : { t: 5, r: rings(list) }); };
  Plate.prototype.clipHoles = function (list) { this.items.push({ t: 6, r: rings(list) }); };
  Plate.prototype.clip = function (pts) { var P = new Float32Array(pts.length * 2); for (var i = 0; i < pts.length; i++) { P[i * 2] = pts[i][0]; P[i * 2 + 1] = pts[i][1]; } this.items.push({ t: 3, p: P }); };
  Plate.prototype.unclip = function () { this.items.push({ t: 4 }); };
  Plate.prototype.text = function (str, x, y, size, font, align, alpha, weight, spacing) { this.items.push({ t: 2, s: str, x: x, y: y, z: size, f: font || 'Marcellus, serif', a: align || 'center', o: alpha == null ? 1 : alpha, wt: weight || '', ls: spacing || 0 }); };
  Plate.prototype.arc = function (cx, cy, r, a0, a1, w, layer, n) {
    var pts = [], k = n || Math.max(8, Math.round(Math.abs(a1 - a0) * r / 2.5));
    for (var i = 0; i <= k; i++) { var a = a0 + (a1 - a0) * i / k; pts.push([cx + Math.cos(a) * r, cy - Math.sin(a) * r]); }
    this.add(pts, w, layer);
  };
  // a contour whose weight follows the shadow: thicker where the form turns away from the light
  Plate.prototype.contour = function (pts, darkAt, w0, w1, layer) {
    var ws = pts.map(function (p, i) { return mix(w0, w1, clamp(darkAt(p, i), 0, 1)); }); this.add(pts, ws, layer);
  };
  /* hatch a patch. pt(u, v) -> [x, y] maps the unit square onto the drawing; dark(u, v) -> 0 (light) .. 1 (black).
     o.lenU, o.lenV: its size in drawing units. Family one runs along v and is spaced along u; three crossing families
     enter at rising darkness. Strokes swell with darkness and taper where they end. */
  Plate.prototype.hatch = function (pt, dark, o) {
    var self = this, sp = o.sp || 1.5, nu = Math.max(2, Math.round(o.lenU / sp)), nv = Math.max(2, Math.round(o.lenV / (o.step || 1.4)));
    var T = o.t || [0.14, 0.42, 0.64, 0.84], w0 = o.w0 == null ? 0.16 : o.w0, w1 = o.w1 == null ? 0.72 : o.w1, layer = o.layer == null ? 3 : o.layer;
    var sl = Math.min(6, (o.lenV / o.lenU) * Math.tan((o.angle == null ? 48 : o.angle) * PI / 180)), jit = o.jitter == null ? 0.25 : o.jitter;
    var fams = [{ thr: T[0], k: 0, gap: 1 }, { thr: T[1], k: 1, gap: 1.25 }, { thr: T[2], k: -0.75, gap: 1.5 }, { thr: T[3], k: 2.4, gap: 1.2 }];
    if (o.families) fams = fams.slice(0, o.families);
    var minSp = o.minSp || 0;
    fams.forEach(function (fm, fi) {
      var du = (1 / nu) * fm.gap, span = Math.abs(fm.k * sl) + 0.001, idx = -1;
      for (var c = (fm.k === 0 ? 0 : -span); c <= 1 + span + 1e-9; c += du) {
        idx++;
        var pts = [], ws = [];
        var flush = function () {
          if (pts.length > 1) {
            var n = pts.length;
            for (var q = 0; q < n; q++) { var e = Math.min(q, n - 1 - q), tp = e < 3 ? 0.35 + 0.22 * e : 1; ws[q] *= tp; }
            self.add(pts, ws, layer + fi * 0.25);
          }
          pts = []; ws = [];
        };
        for (var j = 0; j <= nv; j++) {
          var v = j / nv, u = c + fm.k * sl * v;
          if (u < 0 || u > 1) { flush(); continue; }
          var d = dark(u, v);
          if (d > fm.thr && minSp) {
            var pa = pt(u, v), pb = pt(u + (u + du <= 1 ? du : -du), v), dsp = Math.hypot(pb[0] - pa[0], pb[1] - pa[1]);
            if (dsp < minSp) { var lvl = Math.ceil(Math.log(minSp / Math.max(dsp, 1e-4)) / Math.LN2); if (lvl > 12 || idx % (1 << lvl) !== 0) { flush(); continue; } }
          }
          if (d > fm.thr) {
            var p = pt(u, v), jj = (hash(Math.round(c * 997), j) - 0.5) * jit;
            pts.push([p[0] + jj * 0.3, p[1] + jj * 0.3]);
            ws.push(w0 + (w1 - w0) * Math.pow(clamp((d - fm.thr) / (1 - fm.thr), 0, 1), 0.75) * (0.85 + 0.3 * hash(j, Math.round(c * 331))));
          } else flush();
        }
        flush();
      }
    });
  };
  // stipple: short strokes and dots scattered where dark, for grain, earth and foliage
  Plate.prototype.stipple = function (x0, y0, x1, y1, dark, count, seed, w, layer) {
    var r = rng(seed || 1);
    for (var i = 0; i < count; i++) { var x = x0 + r() * (x1 - x0), y = y0 + r() * (y1 - y0), d = dark(x, y); if (r() > d) continue;
      var a = r() * PI, l = 0.4 + r() * 0.9; this.add([[x, y], [x + Math.cos(a) * l, y + Math.sin(a) * l]], (w || 0.35) * (0.7 + d * 0.6), layer == null ? 4 : layer); }
  };

  /* draw a plate. T: { x, y, s } maps plate units to CSS px; the canvas is in device px (dpr).
     opt.reveal 0..1 draws each stroke up to that share of its length (outlines first); opt.wz scales widths with zoom. */
  var BINS = [0.26, 0.38, 0.52, 0.7, 0.95, 1.3, 1.8, 2.6];
  function draw(g, plate, T, dpr, opt) {
    opt = opt || {};
    var s = T.s * dpr, ox = T.x * dpr, oy = T.y * dpr, CW = g.canvas.width, CH = g.canvas.height;
    var wz = (opt.baseS ? Math.pow(T.s / opt.baseS, opt.grow == null ? 0.4 : opt.grow) * opt.baseS : T.s) * dpr;
    var rev = opt.reveal == null ? 1 : opt.reveal, alpha = opt.alpha == null ? 1 : opt.alpha, minW = opt.minW == null ? 0.18 : opt.minW;
    var paths = BINS.map(function () { return new Path2D(); }), used = BINS.map(function () { return false; });
    function flush() {
      for (var k = 0; k < BINS.length; k++) if (used[k]) {
        g.lineWidth = BINS[k]; g.strokeStyle = 'rgba(255,255,255,' + (alpha * Math.min(1, 0.55 + BINS[k] * 0.55)).toFixed(3) + ')'; g.stroke(paths[k]);
        paths[k] = new Path2D(); used[k] = false;
      }
    }
    g.lineCap = 'round'; g.lineJoin = 'round';
    var items = plate.items, cl = -20, cr = CW + 20, ct = -20, cb = CH + 20, dl = opt.deadline || 0;
    for (var i = opt.from || 0; i < items.length; i++) {
      // resumable: when drawing into a cached picture a slice at a time, stop at the deadline and say where to go on
      if (dl && (i & 127) === 0 && i > (opt.from || 0) && performance.now() > dl) { flush(); return i; }
      var it = items[i], P = it.p;
      if (it.t === 3) { flush(); g.save(); g.beginPath(); for (var m3 = 0; m3 < P.length; m3 += 2) { var X3 = ox + P[m3] * s, Y3 = oy + P[m3 + 1] * s; if (m3) g.lineTo(X3, Y3); else g.moveTo(X3, Y3); } g.closePath(); g.clip(); continue; }
      if (it.t === 4) { flush(); g.restore(); continue; }
      if (it.t === 5 || it.t === 6) {
        flush(); if (it.t === 6) g.save(); g.beginPath();
        it.r.forEach(function (R) { for (var m5 = 0; m5 < R.length; m5 += 2) { var X5 = ox + R[m5] * s, Y5 = oy + R[m5 + 1] * s; if (m5) g.lineTo(X5, Y5); else g.moveTo(X5, Y5); } g.closePath(); });
        if (it.t === 5) { g.fillStyle = it.c || opt.ground || '#002dde'; g.fill('evenodd'); } else g.clip('evenodd');
        continue;
      }
      if (it.t === 2) { if (rev < 0.6) continue; flush(); g.save(); g.globalAlpha = alpha * it.o * clamp((rev - 0.6) / 0.3, 0, 1); g.fillStyle = '#ffffff'; g.font = (it.wt ? it.wt + ' ' : '') + (it.z * s).toFixed(1) + 'px ' + it.f; if ('letterSpacing' in g) g.letterSpacing = (it.ls * s).toFixed(1) + 'px'; g.textAlign = it.a; g.textBaseline = 'alphabetic'; g.fillText(it.s, ox + it.x * s, oy + it.y * s); g.restore(); continue; }
      if (it.t === 1) {
        flush(); g.beginPath();
        for (var m = 0; m < P.length; m += 2) { var X = ox + P[m] * s, Y = oy + P[m + 1] * s; if (m) g.lineTo(X, Y); else g.moveTo(X, Y); }
        g.closePath(); g.fillStyle = it.c || opt.ground || '#002dde'; g.fill(); continue;
      }
      var b = it.b; if (ox + b[2] * s < cl || ox + b[0] * s > cr || oy + b[3] * s < ct || oy + b[1] * s > cb) continue;
      var lim = 1;
      if (rev < 1) { var start = Math.min(0.85, it.l * 0.14), f = clamp((rev - start) / 0.3, 0, 1); if (f <= 0) continue; lim = f; }
      var W = it.w, n = W.length, stop = lim >= 1 ? n - 1 : Math.max(1, Math.floor((n - 1) * lim));
      for (var q = 0; q < stop; q++) {
        var wd = (W[q] + W[q + 1]) * 0.5 * wz; if (wd < minW) continue;
        var k2 = 0; while (k2 < BINS.length - 1 && wd > (BINS[k2] + BINS[k2 + 1]) * 0.5) k2++;
        var x0 = ox + P[q * 2] * s, y0 = oy + P[q * 2 + 1] * s, x1 = ox + P[q * 2 + 2] * s, y1 = oy + P[q * 2 + 3] * s;
        if ((x0 < cl && x1 < cl) || (x0 > cr && x1 > cr) || (y0 < ct && y1 < ct) || (y0 > cb && y1 > cb)) continue;
        paths[k2].moveTo(x0, y0); paths[k2].lineTo(x1, y1); used[k2] = true;
      }
    }
    flush();
    return items.length;
  }

  window.EG = { Plate: Plate, draw: draw, PI: PI, clamp: clamp, sm: sm, mix: mix, rng: rng, hash: hash, noise: noise, fbm: fbm, norm: norm, lit: lit, shade: shade, LIGHT: LIGHT };
})();
