/* ArchChain: the chain as an aqueduct under construction, engraved in the house manner (white line on Arc blue).

   Every stone is a block. Seventeen stones close an arch, the keystone last, and the arch's heights are cut in the
   tablet over it. An arch stands on its timber centering until its last block is final; then the centering is struck.
   The channel over final arches carries water, and each transaction is a glint travelling in it. The treadwheel turns
   once for every arch. Nothing moves that the chain did not do: when blocks stop, the building stops, and the next
   stone hangs on the rope where it is.

   var ac = ArchChain.mount(canvas, { reduced: false });
   ac.init(height, finalHeight)   where the chain is when we start watching
   ac.block(height, txCount)      a new block (heights may jump: every height between becomes a stone)
   ac.final(height)               the latest final height
   ac.phase(name, progress)       node start-up: 'survey' | 'model' (0..1) | 'rpc' | 'peers' | 'sync' | 'live'
   ac.queries(n)                  n more inference requests served: the water in the channel runs with them, and each
                                  batch of them is a surge travelling along it (still water when none come)
   ac.stats()                     what the picture is showing, for the page's own readout
   ac.hit(x, y)                   the arch under a point of the canvas (CSS px), or -1
   ac.destroy()

   Chat mode, ArchChain.mount(canvas, { chat: true }): one arch per answer, the conversation as an aqueduct running out
   of a water house. Each call waits for the real event:
   ac.load([{ checked, height }])  answers already in the conversation (checked: true | false | null; height or null)
   ac.ask()                        a prompt was sent: the next pier rises and the centering is set; returns the arch's index
   ac.stream(tokens)               tokens received so far: stones go up as they arrive (the keystone waits for the end)
   ac.answered()                   the answer is complete: the keystone drops, the wall and channel rise, water flows in
   ac.checked(ok, i)               a second computer re-ran it: agreed strikes the centering; not agreed leaves it standing
   ac.recorded(height, i)          on chain: the block is cut into the arch's tablet
   ac.failed()                     no answer: the stones come down and the site waits for the next try
*/
(function () {
  'use strict';
  var E = window.EG, PI = Math.PI, clamp = E.clamp, sm = E.sm, mix = E.mix, fbm = E.fbm, noise = E.noise, hash = E.hash;
  var ARC = '#002dde';

  // ---------------------------------------------------------------- the aqueduct, in plate units (y down)
  var S = 120, P = 36, B = S + P, R = S / 2, TH = 18, RO = R + TH, KEY = 7, NV = 17;
  var YG = 240, YS = 130, YCB = 43, YCT = 37, YPT = 21, YWL = 24, YFL = 33, CPR = 3;
  var D = 48, OX = 0.3, OY = 0.3, WT = 6;
  var ORDER = [0, 16, 1, 15, 2, 14, 3, 13, 4, 12, 5, 11, 6, 10, 7, 9, 8];   // the k-th block of an arch sets stone ORDER[k]
  var TOP = -40, BOT = 300;                                                  // the rows the picture always shows
  function Q(x, y, z) { return [x + z * OX, y - z * OY]; }
  function onArc(cx, r, a) { return [cx + Math.cos(a) * r, YS - Math.sin(a) * r]; }
  function aOf(i) { return PI - i * PI / NV; }
  var LS = E.norm([-0.62, -0.58, 0.52]);
  function litS(nx, ny, nz) { return Math.max(0, nx * LS[0] + ny * LS[1] + nz * LS[2]); }
  function closed(p) { return p.concat([p[0]]); }
  function lerp2(a, b, t) { return [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]; }
  function fmt(n) { return String(n).replace(/\B(?=(\d{3})+(?!\d))/g, ','); }

  // ---------------------------------------------------------------- engraving at the size it will be shown
  var K = 1, DPR = 1;   // CSS px per unit, device px per CSS px (set when the sprites are cut)
  function hatch(pl, pt, dark, lenU, lenV, o) {
    o = o || {};
    pl.hatch(pt, dark, { lenU: Math.max(1, lenU), lenV: Math.max(1, lenV), sp: (o.sp || 1.2) / K, step: (o.step || 1.4) / K, angle: o.angle == null ? 55 : o.angle,
      t: o.t || [0.16, 0.46, 0.68, 0.86], w0: (o.w0 || 0.13) / K, w1: (o.w1 || 0.62) / K, layer: 3, families: o.families, jitter: o.jitter == null ? 0.25 : o.jitter });
  }
  function line(pl, pts, wpx, layer) { pl.line(pts, wpx / K, layer || 1); }
  function contour(pl, pts, fn, w0, w1) { pl.contour(pts, fn, w0 / K, w1 / K, 1); }
  // a face of dressed stone, pillowed like rustication: bright bevels to the upper left, dark to the lower right
  function pillow(u, v, e) {
    e = e || 0.14; var du = u < e ? (u - e) / e : u > 1 - e ? (u - 1 + e) / e : 0, dv = v < e ? (v - e) / e : v > 1 - e ? (v - 1 + e) / e : 0;
    var n = E.norm([du * 0.9, dv * 0.9, 1]); return 0.86 - litS(n[0], n[1], n[2]) * 1.05;
  }
  function inPoly(poly, x, y) { var c = false; for (var i = 0, j = poly.length - 1; i < poly.length; j = i++) { var a = poly[i], b = poly[j]; if ((a[1] > y) !== (b[1] > y) && x < (b[0] - a[0]) * (y - a[1]) / (b[1] - a[1]) + a[0]) c = !c; } return c; }
  // a free polygon hatched in a rotated frame
  function hatchPoly(pl, poly, dark, o) {
    o = o || {};
    var xs = poly.map(function (p) { return p[0]; }), ys = poly.map(function (p) { return p[1]; });
    var x0 = Math.min.apply(null, xs), x1 = Math.max.apply(null, xs), y0 = Math.min.apply(null, ys), y1 = Math.max.apply(null, ys);
    var rot = o.rot || 0, c = Math.cos(rot), s = Math.sin(rot), cx = (x0 + x1) / 2, cy = (y0 + y1) / 2, Rr = Math.hypot(x1 - x0, y1 - y0) / 2 + 1;
    pl.clip(poly);
    hatch(pl, function (u, v) { var a = (2 * u - 1) * Rr, b = (2 * v - 1) * Rr; return [cx + a * c - b * s, cy + a * s + b * c]; }, function (u, v) {
      var a = (2 * u - 1) * Rr, b = (2 * v - 1) * Rr, x = cx + a * c - b * s, y = cy + a * s + b * c; return dark(x, y); }, 2 * Rr, 2 * Rr, o);
    pl.unclip();
  }
  function bake(x0, y0, x1, y1, fn, exact) {
    var s = K * DPR, c = document.createElement('canvas'); c.width = Math.max(1, exact ? Math.round((x1 - x0) * s) : Math.ceil((x1 - x0) * s) + 2); c.height = Math.max(1, Math.ceil((y1 - y0) * s) + 2);
    var pl = new E.Plate(x1 - x0, y1 - y0); fn(pl);
    E.draw(c.getContext('2d'), pl, { x: -x0 * K, y: -y0 * K, s: K }, DPR);
    return { c: c, x0: x0, y0: y0 };
  }

  // noise that repeats every W units in x, so a tile meets its neighbour without a seam
  function pfbm(x, y, W, f) { var u = ((x % W) + W) % W / W; return fbm(u * f, y) * (1 - u) + fbm((u - 1) * f, y) * u; }
  // coursed ashlar over a region: fine bed and cross joints, each block its own weathering, stains in places
  var CH = 5.5, BL = 13;
  function masonry(pl, poly, x0, x1, y0, y1, o) {
    o = o || {};
    var base = o.base == null ? 0.26 : o.base, seed = o.seed || 1;
    pl.clip(poly);
    hatch(pl, function (u, v) { return [mix(x0, x1, u), mix(y0, y1, v)]; }, function (u, v) {
      var x = mix(x0, x1, u), y = mix(y0, y1, v), row = Math.floor((YG - y) / CH), fr = ((YG - y) / CH) - row;
      var xo = (x + (row % 2) * BL * 0.5 + hash(row, 7) * 3) / BL, fx = xo - Math.floor(xo), id = Math.floor(xo) * 31 + row * 7;
      var bed = sm(0.2, 0, fr) * 0.34 + sm(0.86, 1, fr) * 0.1, perp = (sm(0.06, 0, fx) + sm(0.94, 1, fx)) * 0.2;
      var weather = 0.2 * (fbm(x * 0.09 + seed, y * 0.16) - 0.45) + 0.1 * (hash(id, seed) - 0.5), stain = 0.18 * sm(0.62, 0.8, fbm(x * 0.03 + 7 + seed, y * 0.025));
      return clamp(base + bed + perp + weather + stain + (o.dark ? o.dark(x, y) : 0), 0, 1);
    }, x1 - x0, y1 - y0, { angle: 62, sp: 1.1, step: 1.3 });
    for (var row = Math.floor((YG - y1) / CH); row <= Math.ceil((YG - y0) / CH); row++) {
      var yy = YG - row * CH; if (yy <= y0 || yy >= y1) continue;
      line(pl, [[x0, yy], [x1, yy]], 0.3, 2);
      var off = (row % 2) * BL * 0.5 + hash(row, 7) * 3;
      for (var xj = Math.floor((x0 + off) / BL) * BL - off; xj < x1; xj += BL) if (xj > x0) line(pl, [[xj, yy - CH], [xj, yy]], 0.28, 2);
    }
    pl.unclip();
  }
  // ---------------------------------------------------------------- the parts, each cut once
  // a pier, centred on x = 0: its jamb (the right face, seen inside the next opening) in shadow, its face coursed ashlar,
  // an impost at the springing with the corbels the centering rests on, and a plinth at the ground
  var ROWS = 10, RH = (YG - YS) / ROWS;
  function pierPlate(pl, rows) {
    rows = rows == null ? ROWS : rows;
    var x0 = -P / 2, x1 = P / 2, yTop = YG - rows * RH;
    if (rows <= 0) return;
    var J = [[x1, yTop], [x1, YG], Q(x1, YG, D), Q(x1, yTop, D)];
    pl.mask(J);
    hatch(pl, function (u, v) { return Q(x1, mix(yTop, YG, v), u * D); }, function (u, v) {
      var y = mix(yTop, YG, v), fr = ((YG - y) / CH) % 1; return clamp(0.4 + 0.22 * sm(0.18, 0, fr) + 0.1 * (fbm(u * 3, y * 0.1) - 0.5) + 0.16 * u, 0, 1);
    }, D * 0.42, YG - yTop, { angle: 64, sp: 1.2 });
    for (var r = 1; r * CH < YG - yTop; r++) { var y = YG - r * CH; line(pl, [Q(x1, y, 0), Q(x1, y, D)], 0.26, 2); }
    line(pl, [Q(x1, yTop, D), Q(x1, YG, D)], 0.4);
    var face = [[x0, yTop], [x1, yTop], [x1, YG], [x0, YG]];
    pl.mask(face);
    masonry(pl, face, x0, x1, yTop, YG, { dark: function (x, y) { return 0.1 * sm(YG - 40, YG, y) + 0.06 * sm(x0 + 12, x1, x); } });
    line(pl, [[x0, yTop], [x0, YG]], 0.5); line(pl, [[x1, yTop], [x1, YG]], 1.0);
    if (rows < ROWS) { line(pl, [[x0, yTop], [x1, yTop]], 0.5); return; }
    // the plinth at the ground, projecting; its top catches the light
    var pb = [[x0 - 3, YG - 7], [x1 + 3, YG - 7], [x1 + 3, YG + 1], [x0 - 3, YG + 1]], pt = [[x0 - 3, YG - 7], [x1 + 3, YG - 7], Q(x1 + 3, YG - 7, 3), Q(x0 - 3, YG - 7, 3)];
    pl.mask(pt); pl.mask(pb);
    hatch(pl, function (u, v) { return [mix(x0 - 3, x1 + 3, u), mix(YG - 7, YG + 1, v)]; }, function (u, v) { return clamp(0.3 + 0.4 * v + 0.1 * (fbm(u * 6, v) - 0.5), 0, 1); }, P + 6, 8, { angle: 70 });
    line(pl, closed(pb), 0.5); line(pl, pt, 0.35);
    // the impost: a projecting band at the springing, and a corbel each side for the centering
    var im = [[x0 - 2.5, YS - 1], [x1 + 2.5, YS - 1], [x1 + 2.5, YS + 5], [x0 - 2.5, YS + 5]], imT = [[x0 - 2.5, YS - 1], [x1 + 2.5, YS - 1], Q(x1 + 2.5, YS - 1, 3), Q(x0 - 2.5, YS - 1, 3)];
    pl.mask(imT); pl.mask(im);
    hatch(pl, function (u, v) { return [mix(x0 - 2.5, x1 + 2.5, u), mix(YS - 1, YS + 5, v)]; }, function (u, v) { return 0.16 + 0.6 * sm(0.3, 1, v); }, P + 5, 6, { angle: 75 });
    line(pl, closed(im), 0.45); line(pl, imT, 0.3);
    [-1, 1].forEach(function (sd) {
      var cx0 = sd < 0 ? x0 - 7 : x1, cb = [[cx0, YS + 5], [cx0 + 7, YS + 5], [cx0 + 7, YS + 11], [cx0, YS + 11]];
      pl.mask(cb); hatch(pl, function (u, v) { return [mix(cx0, cx0 + 7, u), mix(YS + 5, YS + 11, v)]; }, function (u, v) { return clamp(0.3 + 0.5 * v + (sd > 0 ? 0.15 * u : 0), 0, 1); }, 7, 6, { angle: 70 });
      line(pl, closed(cb), 0.4);
    });
  }
  // the stones of an arch centred on x = 0
  function stonePoly(i, rOut) { var a0 = aOf(i), a1 = aOf(i + 1), pts = [], k; for (k = 0; k <= 6; k++) pts.push(onArc(0, R, mix(a0, a1, k / 6))); for (k = 6; k >= 0; k--) pts.push(onArc(0, rOut, mix(a0, a1, k / 6))); return pts; }
  function stonePlate(pl, i) {
    var key = i === 8, rOut = key ? RO + KEY : RO, a0 = aOf(i), a1 = aOf(i + 1), am = (a0 + a1) / 2, k;
    // its top face, seen from above until the wall is built over it
    if (am < 3 * PI / 4) {
      var tb = []; for (k = 0; k <= 6; k++) tb.push(onArc(0, rOut, mix(a0, a1, k / 6))); for (k = 6; k >= 0; k--) { var p = onArc(0, rOut, mix(a0, a1, k / 6)); tb.push(Q(p[0], p[1], D)); }
      pl.mask(tb);
      hatch(pl, function (u, v) { var p2 = onArc(0, rOut, mix(a0, a1, v)); return Q(p2[0], p2[1], u * D); }, function (u, v) { return clamp(0.1 + 0.25 * u + 0.12 * (fbm(u * 3 + i, v * 3) - 0.5), 0, 1); }, D * 0.42, (a0 - a1) * rOut, { angle: 30, families: 2 });
      var e1 = []; for (k = 0; k <= 6; k++) { var p3 = onArc(0, rOut, mix(a0, a1, k / 6)); e1.push(Q(p3[0], p3[1], D)); } line(pl, e1, 0.4);
      line(pl, [onArc(0, rOut, a0), Q(onArc(0, rOut, a0)[0], onArc(0, rOut, a0)[1], D)], 0.35); line(pl, [onArc(0, rOut, a1), Q(onArc(0, rOut, a1)[0], onArc(0, rOut, a1)[1], D)], 0.35);
    }
    var poly = stonePoly(i, rOut), rx = Math.cos(am), ry = -Math.sin(am), tx = Math.sin(am), ty = Math.cos(am);
    pl.mask(poly);
    hatch(pl, function (u, v) { return onArc(0, mix(R + 0.4, rOut - 0.4, u), mix(a0, a1, v)); }, function (u, v) {
      var e = 0.15, du = u < e ? (u - e) / e : u > 1 - e ? (u - 1 + e) / e : 0, dv = v < e ? (v - e) / e : v > 1 - e ? (v - 1 + e) / e : 0;
      var n = E.norm([du * 0.9 * rx + dv * 0.9 * tx, du * 0.9 * ry + dv * 0.9 * ty, 1]);
      var side = am < PI / 2 ? 0.1 + 0.12 * (1 - Math.sin(am)) : 0;
      return clamp(0.86 - litS(n[0], n[1], n[2]) * 1.05 + side + 0.13 * (fbm(u * 3 + i * 7, v * 3) - 0.5), 0, 1);
    }, rOut - R, (a0 - a1) * (R + rOut) / 2, { angle: 58 });
    contour(pl, closed(poly), function (p) { return sm(-0.4, 0.8, ((p[0] - onArc(0, (R + rOut) / 2, am)[0]) * 0.7 + (p[1] - onArc(0, (R + rOut) / 2, am)[1]) * 0.7) / 12); }, 0.4, 1.0);
    if (key) {
      // the keystone projects above the ring; a small sunk panel on its face
      var kp = [onArc(0, R + 6, mix(a0, a1, 0.2)), onArc(0, R + 6, mix(a0, a1, 0.8)), onArc(0, rOut - 5, mix(a0, a1, 0.78)), onArc(0, rOut - 5, mix(a0, a1, 0.22))];
      pl.mask(kp); hatchPoly(pl, kp, function (x, y) { return clamp(0.5 + 0.3 * sm(-3, 3, x), 0, 1); }, { angle: 80, families: 2 }); line(pl, closed(kp), 0.45);
    }
  }
  // the underside of the ring, where the eye can reach it: the lower left of each opening
  function soffitPlate(pl, i) {
    var a0 = aOf(i), a1 = Math.max(aOf(i + 1), 3 * PI / 4); if (a1 >= a0) return;
    var sp = [], k; for (k = 0; k <= 8; k++) sp.push(onArc(0, R, mix(a0, a1, k / 8))); for (k = 8; k >= 0; k--) { var p = onArc(0, R, mix(a0, a1, k / 8)); sp.push(Q(p[0], p[1], D)); }
    pl.mask(sp);
    hatch(pl, function (u, v) { var p2 = onArc(0, R, mix(a0, a1, v)); return Q(p2[0], p2[1], u * D); }, function (u, v) { return clamp(0.78 + 0.12 * u + 0.1 * (fbm(u * 4 + i, v * 4) - 0.5), 0, 1); }, D * 0.42, (a0 - a1) * R, { angle: 40 });
    line(pl, [onArc(0, R, a0), Q(onArc(0, R, a0)[0], onArc(0, R, a0)[1], D)], 0.35);
  }
  // the timber centering the arch is built on: a front rib and a back rib, lagging between, radial struts to a tie
  // beam on the corbels, and two props from the ground
  function timber(pl, a, b, w, tone, seed) {
    var dx = b[0] - a[0], dy = b[1] - a[1], L = Math.hypot(dx, dy) || 1, nx = -dy / L * w / 2, ny = dx / L * w / 2;
    var poly = [[a[0] + nx, a[1] + ny], [b[0] + nx, b[1] + ny], [b[0] - nx, b[1] - ny], [a[0] - nx, a[1] - ny]];
    pl.mask(poly);
    hatch(pl, function (u, v) { return [mix(a[0], b[0], v) + nx * (2 * u - 1), mix(a[1], b[1], v) + ny * (2 * u - 1)]; }, function (u, v) {
      return clamp(tone + 0.3 * sm(0.35, 1, u) + 0.16 * (noise(v * L * 0.25 + seed, u * 4) - 0.5), 0, 1); }, w, L, { angle: 8, families: 3, sp: 1.1 });
    line(pl, [poly[0], poly[1]], 0.35); line(pl, [poly[3], poly[2]], 0.7);
  }
  function centeringPlate(pl) {
    var k, rr = R - 0.4, ri = R - 7, n = 24, zb = D - 6;
    // back rib, glimpsed behind the front one
    var back = []; for (k = 0; k <= n; k++) { var p = onArc(0, rr, PI - k * PI / n); back.push(Q(p[0], p[1], zb)); }
    line(pl, back, 0.45);
    // lagging: boards across the top of the ribs, front to back (the stones will cover them)
    for (k = 0; k <= 34; k++) { var a = PI - k * PI / 34, p0 = onArc(0, rr, a); line(pl, [p0, Q(p0[0], p0[1], zb)], 0.3); }
    // the front rib: a curved beam in short segments
    var outer = [], inner = []; for (k = 0; k <= n; k++) { outer.push(onArc(0, rr, PI - k * PI / n)); inner.push(onArc(0, ri, PI - k * PI / n)); }
    var rib = outer.concat(inner.slice().reverse()); pl.mask(rib);
    hatch(pl, function (u, v) { return onArc(0, mix(ri, rr, u), PI - v * PI); }, function (u, v) { return clamp(0.34 + 0.3 * (1 - u) + 0.14 * (noise(v * 40, u * 3) - 0.5), 0, 1); }, rr - ri, PI * (rr + ri) / 2, { angle: 10, families: 3, sp: 1.1 });
    line(pl, outer, 0.5); line(pl, inner, 0.75);
    for (k = 1; k < 6; k++) { var aj = PI - k * PI / 6; line(pl, [onArc(0, ri, aj), onArc(0, rr, aj)], 0.5); }
    // tie beam on the corbels, king post and struts
    var yt = YS + 11, tie = [[-R - 6, yt], [R + 6, yt]];
    timber(pl, [-R + 1, YS + 8], [R - 1, YS + 8], 5, 0.4, 3);
    timber(pl, [0, YS + 6], [0, YS - ri + 3], 4.5, 0.35, 5);
    [30, 60, 120, 150].forEach(function (deg, j) { var aa = deg * PI / 180, tip = onArc(0, ri - 0.5, aa); timber(pl, [Math.cos(aa) * 8, YS + 5], tip, 3.4, 0.38, 7 + j); });
    // props down to the ground
    [-30, 30].forEach(function (x, j) { timber(pl, [x, YS + 10], [x + (j ? 4 : -4), YG], 4, 0.42, 11 + j); });
    [-30, 30].forEach(function (x, j) { var cb = [[x - 6, YG - 2], [x + 6, YG - 2], [x + 6, YG + 1], [x - 6, YG + 1]]; pl.mask(cb); line(pl, closed(cb), 0.45); });
    // the folding wedges on the tie beam, knocked out when the centering is struck
    [-R + 10, R - 10].forEach(function (x) { var w = [[x - 5, YS + 5], [x + 5, YS + 3.5], [x + 5, YS + 5.5], [x - 5, YS + 5.5]]; pl.mask(w); line(pl, closed(w), 0.5); });
  }
  // the wall over one arch (x from -B/2 to B/2): ashlar courses on from the piers, down to the ring's back
  function spandrelPlate(pl) {
    var x0 = -B / 2, x1 = B / 2, poly = [[x0, YCB], [x1, YCB], [x1, YS]], k;
    for (k = 0; k <= 36; k++) poly.push(onArc(0, RO, k * PI / 36));
    poly.push([x0, YS]);
    pl.mask(poly);
    masonry(pl, poly, x0, x1, YCB, YS, { base: 0.24, seed: 3, dark: function (x, y) { return 0.28 * sm(YCB + 6, YCB, y) + 0.05 * sm(0, x1, x); } });
    var ex = []; for (k = 0; k <= 36; k++) ex.push(onArc(0, RO + 0.2, k * PI / 36)); line(pl, ex, 0.6);
  }
  // the cornice, the parapet with its tablet, and the channel behind: its far wall's inner face, dry
  function channelPlate(pl, cap) {
    var x0 = -B / 2, x1 = B / 2, tw = 34;
    // far wall: the inner face (seen above the near parapet) and its top
    var bf = [Q(x0, YFL, D - WT), Q(x1, YFL, D - WT), Q(x1, YPT, D - WT), Q(x0, YPT, D - WT)], bt = [Q(x0, YPT, D - WT), Q(x1, YPT, D - WT), Q(x1, YPT, D), Q(x0, YPT, D)];
    pl.mask(bt); pl.mask(bf);
    hatch(pl, function (u, v) { return Q(mix(x0, x1, v), mix(YPT, YFL, u), D - WT); }, function (u, v) { return clamp(0.66 + 0.16 * u + 0.12 * (fbm(v * 12, u * 3) - 0.5), 0, 1); }, YFL - YPT, B, { angle: 60 });
    hatch(pl, function (u, v) { return Q(mix(x0, x1, v), YPT, mix(D - WT, D, u)); }, function (u) { return 0.12 + 0.2 * u; }, WT * 0.42, B, { angle: 20, families: 2 });
    line(pl, [bt[3], bt[2]], 0.45); line(pl, [bt[0], bt[1]], 0.4);
    // the near parapet: its top, then its face with the tablet
    var nt = [[x0, YPT], [x1, YPT], Q(x1, YPT, WT), Q(x0, YPT, WT)], nf = [[x0, YPT], [x1, YPT], [x1, YCT], [x0, YCT]];
    pl.mask(nt); pl.mask(nf);
    hatch(pl, function (u, v) { return Q(mix(x0, x1, v), YPT, u * WT); }, function (u) { return 0.1 + 0.18 * u; }, WT * 0.42, B, { angle: 20, families: 2 });
    var tab = [[-tw, YPT + 2.6], [tw, YPT + 2.6], [tw, YCT - 2.6], [-tw, YCT - 2.6]];
    pl.clipHoles([nf, tab]);
    hatch(pl, function (u, v) { return [mix(x0, x1, v), mix(YPT, YCT, u)]; }, function (u, v) { var x = mix(x0, x1, v), bl = ((x + 200) / 39) % 1; return clamp(0.3 + 0.2 * sm(0.93, 1, bl) + 0.14 * (fbm(x * 0.1, u * 3) - 0.5) + 0.12 * u, 0, 1); }, YCT - YPT, B, { angle: 58 });
    pl.unclip();
    for (var bx = x0 + 39 - ((x0 + 200) % 39); bx < x1; bx += 39) if (Math.abs(bx) > tw + 2) line(pl, [[bx, YPT], [bx, YCT]], 0.35);
    // the tablet: a sunk field, clean for the heights, a fine moulding round it
    line(pl, closed(tab), 0.6);
    line(pl, [[-tw + 1, YCT - 3.4], [tw - 1, YCT - 3.4]], 0.35); line(pl, [[tw - 0.9, YPT + 3.2], [tw - 0.9, YCT - 3.3]], 0.35);
    [-1, 1].forEach(function (sd) { var ex = sd * tw, ear = [[ex, YPT + 4.5], [ex + sd * 6, YPT + 3], [ex + sd * 6, YCT - 3], [ex, YCT - 4.5]]; line(pl, ear, 0.45); });
    line(pl, [[x0, YPT], [x1, YPT]], 0.6);
    // the cornice: a projecting band, its top lit, its face moulded, a shadow thrown on the wall under it
    var cf = [[x0, YCT], [x1, YCT], [x1, YCB], [x0, YCB]], ct = [[x0, YCT], [x1, YCT], Q(x1, YCT, -CPR), Q(x0, YCT, -CPR)];
    pl.mask(ct); pl.mask(cf);
    hatch(pl, function (u, v) { return [mix(x0, x1, v), mix(YCT, YCB, u)]; }, function (u) { return clamp(0.12 + 0.5 * sm(0.35, 0.55, u) + 0.2 * sm(0.75, 1, u), 0, 1); }, YCB - YCT, B, { angle: 80 });
    line(pl, [[x0, YCT], [x1, YCT]], 0.55); line(pl, [[x0, YCT + 2.3], [x1, YCT + 2.3]], 0.35); line(pl, [[x0, YCB], [x1, YCB]], 0.9);
    var sh = [[x0, YCB], [x1, YCB], [x1, YCB + 3], [x0, YCB + 3]]; pl.mask(sh);
    hatch(pl, function (u, v) { return [mix(x0, x1, v), mix(YCB, YCB + 3, u)]; }, function (u) { return 0.72 - 0.35 * u; }, 3, B, { angle: 70, families: 3 });
    if (cap) {
      // the end of the work so far: the channel's cut end, its right face in shadow
      var xe = x1, ef = [[xe, YPT], [xe, YCB], Q(xe, YCB, D), Q(xe, YPT, D)];
      pl.mask(ef); hatch(pl, function (u, v) { return Q(xe, mix(YPT, YCB, v), u * D); }, function () { return 0.72; }, D * 0.42, YCB - YPT, { angle: 60 }); line(pl, closed(ef), 0.6);
    }
  }
  // the sky, the hills and the plain beyond: a tile that repeats every W units
  var TILE = 4 * B, M = 24;
  function farPlate(pl, yTop) {
    var W = TILE, Y0 = TOP - 330, y0 = Math.max(Y0, yTop == null ? Y0 : yTop), hz = 198;
    var ridge = function (x) { var t = x / W * 2 * PI; return hz - 15 - 10 * Math.sin(t + 0.6) - 6 * Math.sin(2 * t + 2.1) - 3.5 * Math.sin(5 * t + 0.4) - 1.6 * Math.sin(11 * t + 1); };
    // banks of cloud: long, low, carried by the strokes themselves; lines gather along their shaded undersides
    var banks = [[0.2, 30, 0.22, 5], [0.7, 58, 0.18, 4]];
    function bank(x, y) { var best = 0; banks.forEach(function (b) { var dx = ((x - b[0] * W) % W + W * 1.5) % W - W / 2, nx = dx / (b[2] * W), dy = (y - b[1]) / b[3];
      var w = (1 - nx * nx) * Math.exp(-dy * dy * (dy > 0 ? 1.1 : 0.3)); if (dy > -0.4) best = Math.max(best, w * sm(-0.4, 0.7, dy)); }); return clamp(best, 0, 1); }
    hatch(pl, function (u, v) { return [mix(-M, W + M, v), mix(y0, hz, u)]; }, function (u, v) {
      var x = mix(-M, W + M, v), y = mix(y0, hz, u), uu = (y - Y0) / (hz - Y0); if (y > ridge(x) + 0.5) return 0;
      return clamp(0.18 + 0.14 * Math.pow(1 - uu, 1.3) - 0.04 * sm(0.8, 1, uu) + 0.22 * bank(x, y), 0, 1);
    }, hz - y0, W + 2 * M, { angle: 8, t: [0.1, 0.58, 0.8, 0.94], w0: 0.24, w1: 0.46, jitter: 0.2, families: 2, sp: 1.8, step: 4 });
    // the hills: their flanks hatched down the slope, the far side of each fold in shade
    var rp = []; for (var x = -M; x <= W + M; x += 2) rp.push([x, ridge(x)]);
    hatch(pl, function (u, v) { var xx = mix(-M, W + M, v); return [xx, mix(ridge(xx), hz + 1, u)]; }, function (u, v) {
      var xx = mix(-M, W + M, v), s = (ridge(xx + 1) - ridge(xx - 1)) / 2; return clamp(0.26 + 0.3 * sm(-0.1, 0.6, s) + 0.08 * (pfbm(xx, u * 3, W, 30) - 0.5) - 0.08 * u, 0, 1);
    }, 26, W + 2 * M, { angle: 20, t: [0.12, 0.56, 0.8, 0.95], w0: 0.1, w1: 0.3, sp: 1.5, families: 2 });
    line(pl, rp, 0.4, 2);
    // the plain, closing up toward us, a river winding through it
    var riv = function (x) { var t = x / W * 2 * PI; return 213 + 5 * Math.sin(t * 2 + 1) + 3 * Math.sin(t * 3); };
    hatch(pl, function (u, v) { return [mix(-M, W + M, v), mix(hz, YG, u)]; }, function (u, v) {
      var xx = mix(-M, W + M, v), y = mix(hz, YG, u), dr = Math.abs(y - riv(xx)); if (dr < 2) return 0.05;
      return clamp(0.16 + 0.26 * u + 0.08 * (pfbm(xx, y * 0.2, W, 50) - 0.5), 0, 1);
    }, YG - hz, W + 2 * M, { angle: 5, t: [0.12, 0.5, 0.78, 0.95], w0: 0.1, w1: 0.36, step: 3, families: 2, jitter: 0.6, sp: 1.5 });
    var rl = [], rr2 = []; for (x = -M; x <= W + M; x += 3) { rl.push([x, riv(x) - 2]); rr2.push([x, riv(x) + 2]); } line(pl, rl, 0.3, 2); line(pl, rr2, 0.45, 2);
    var r = E.rng(17); for (var i = 0; i < 26; i++) { var tx = r() * W, ty = hz + 4 + r() * 26; if (Math.abs(ty - riv(tx)) < 5) continue; for (var j = 0; j < 5; j++) { var a = r() * PI, l = 1 + r() * 2; pl.seg(tx + (r() - 0.5) * 5, ty - r() * 3, tx + (r() - 0.5) * 5 + Math.cos(a) * l, ty - r() * 3 - Math.sin(a) * l * 0.6, 0.26 / K, 4); } }
  }
  // the ground in front of the aqueduct: quiet, a worn track along it, a few stones; a tile repeating every 2 bays
  var NTILE = 2 * B;
  function nearPlate(pl, yBot) {
    var W = NTILE, y1 = Math.min(YG + 340, yBot == null ? YG + 340 : yBot);
    hatch(pl, function (u, v) { return [mix(-M, W + M, v), mix(YG, y1, u)]; }, function (u, v) {
      var x = mix(-M, W + M, v), y = mix(YG, y1, u), rut = Math.exp(-Math.pow((y - 258) / 2.2, 2)) + Math.exp(-Math.pow((y - 268) / 2.2, 2));
      return clamp(0.26 + 0.18 * sm(YG + 20, YG + 120, y) + 0.2 * sm(YG + 5, YG, y) + 0.22 * rut + 0.08 * (pfbm(x, y * 0.12, W, 24) - 0.5), 0, 1);
    }, y1 - YG, W + 2 * M, { angle: 4, t: [0.12, 0.5, 0.76, 0.92], w0: 0.1, w1: 0.42, step: 2.4, jitter: 0.7, families: 2, sp: 1.45 });
    var r = E.rng(29);
    for (var i = 0; i < 9; i++) {
      var x = 10 + r() * (W - 20), y = YG + 14 + r() * 50, rx = 2 + r() * 3, ry = rx * 0.5, st = [];
      for (var k = 0; k < 16; k++) { var a = k / 16 * 2 * PI; st.push([x + Math.cos(a) * rx, y + Math.sin(a) * ry * (1 + 0.15 * Math.sin(a * 3))]); }
      pl.mask(st); pl.clip(st);
      hatch(pl, function (u, v) { return [x - rx + 2 * rx * v, y - ry * 1.2 + 2.4 * ry * u]; }, (function (x, y, rx, ry) { return function (u, v) { var nx = 2 * v - 1, ny = 2 * u - 1; return clamp(0.2 + 0.6 * sm(-0.6, 1, nx * 0.6 + ny * 0.8), 0, 1); }; })(x, y, rx, ry), 2.4 * ry, 2 * rx, { angle: 50, families: 2 });
      pl.unclip(); contour(pl, closed(st), function (p) { return sm(-1, 1, (p[0] - x) * 0.4 + (p[1] - y) * 0.9); }, 0.3, 0.9);
      pl.seg(x - rx * 0.8, y + ry * 1.05, x + rx * 1.5, y + ry * 1.15, 0.45 / K, 4);
    }
    for (i = 0; i < 40; i++) { var gx = r() * W, gy = YG + 3 + r() * 64, gh = 2 + r() * 4; for (var g2 = 0; g2 < 3; g2++) { var bnd = (r() - 0.5) * 3; pl.line([[gx + g2 * 0.7, gy], [gx + g2 * 0.7 + bnd * 0.4, gy - gh * 0.6], [gx + g2 * 0.7 + bnd, gy - gh]], 0.28 / K, 4); } }
  }
  // the treadwheel crane: two masts leaning over the work, held by stays; a great wheel at their foot
  var CRANE = { wheel: 24, head: [-B * 1.02, YCB - 44] };
  function cranePlate(pl) {
    var head = CRANE.head, fa = [-10, YG], fb = [12, YG - 1], hb = [head[0] + 4, head[1] - 1];
    // two masts leaning over the work, a crossbar, and the stays that hold them from behind
    line(pl, [[head[0] + 3, head[1]], [140, YG + 2]], 0.4); line(pl, [[head[0] + 2, head[1] + 1], [104, YG + 5]], 0.35);
    timber(pl, fb, hb, 4.6, 0.5, 23);
    timber(pl, fa, head, 5.2, 0.4, 21);
    var mA = lerp2(fa, head, 0.42), mB = lerp2(fb, hb, 0.42); timber(pl, mA, mB, 3, 0.45, 31);
    var mC = lerp2(fa, head, 0.72), mD = lerp2(fb, hb, 0.72); timber(pl, mC, mD, 2.6, 0.45, 33);
    // the pulley block at the head
    var pb = [[head[0] - 4, head[1] - 3], [head[0] + 4, head[1] - 3], [head[0] + 4, head[1] + 8], [head[0] - 4, head[1] + 8]]; pl.mask(pb);
    hatchPoly(pl, pb, function () { return 0.5; }, { angle: 80, families: 2 }); line(pl, closed(pb), 0.55);
    pl.arc(head[0], head[1] + 2.5, 2.4, 0, 2 * PI, 0.45 / K, 1, 12); pl.arc(head[0], head[1] + 2.5, 0.8, 0, 2 * PI, 0.4 / K, 1, 8);
    // the rope down the mast to the drum on the wheel's axle
    line(pl, [[head[0] + 2, head[1] + 4], [4, YG - CRANE.wheel - 6]], 0.35);
    // the treadwheel's frame (the wheel itself turns, and is drawn live)
    timber(pl, [-CRANE.wheel - 4, YG], [-1, YG - CRANE.wheel - 6], 3.4, 0.45, 25);
    timber(pl, [CRANE.wheel + 4, YG], [1, YG - CRANE.wheel - 6], 3.4, 0.5, 27);
    timber(pl, [-CRANE.wheel - 8, YG - 1], [CRANE.wheel + 8, YG - 1], 3, 0.45, 29);
  }
  // a small cloud of stone dust
  function dustPlate(pl, seed) { var r = E.rng(seed); for (var i = 0; i < 90; i++) { var a = r() * 2 * PI, d = Math.pow(r(), 0.6) * 9, x = Math.cos(a) * d * 1.4, y = Math.sin(a) * d * 0.7; pl.seg(x, y, x + 0.4 + r() * 0.6, y + 0.2, (0.25 + r() * 0.3) / K, 4); } }

  // the water house at the head of a conversation's aqueduct (chat mode): a small temple front, a pediment over a round
  // headed door, its right face in shadow. Cut in two: the side and roof go behind the channel, the front in front of it.
  var TW = 64, TT = YPT - 18, TPK = 15;
  function towerSidePlate(pl) {
    var x0 = -TW / 2, x1 = TW / 2, r, k;
    var J = [[x1, TT], [x1, YG], Q(x1, YG, D), Q(x1, TT, D)];
    pl.mask(J);
    hatch(pl, function (u, v) { return Q(x1, mix(TT, YG, v), u * D); }, function (u, v) {
      var y = mix(TT, YG, v), fr = ((YG - y) / CH) % 1; return clamp(0.44 + 0.22 * sm(0.18, 0, fr) + 0.1 * (fbm(u * 3, y * 0.1) - 0.5) + 0.16 * u, 0, 1);
    }, D * 0.42, YG - TT, { angle: 64, sp: 1.2 });
    for (r = 1; r * CH < YG - TT; r++) { var y = YG - r * CH; line(pl, [Q(x1, y, 0), Q(x1, y, D)], 0.26, 2); }
    line(pl, [Q(x1, TT, D), Q(x1, YG, D)], 0.45);
    // the roof: its right slope running back from the pediment, tiled
    var ap = [0, TT - 1 - TPK], eR = [x1 + 3.5, TT - 1], zb = D + 3;
    var slope = [ap, eR, Q(eR[0], eR[1], zb), Q(ap[0], ap[1], zb)];
    pl.mask(slope);
    hatch(pl, function (u, v) { var p = lerp2(ap, eR, v); return Q(p[0], p[1], u * zb); }, function (u, v) { return clamp(0.3 + 0.2 * v + 0.12 * (fbm(u * 7, v * 5) - 0.5), 0, 1); },
      zb * 0.42, Math.hypot(eR[0] - ap[0], eR[1] - ap[1]), { angle: 84, families: 2 });
    for (k = 1; k < 14; k++) { var z = k / 14 * zb; line(pl, [Q(ap[0], ap[1], z), Q(eR[0], eR[1], z)], 0.26, 2); }
    line(pl, [Q(eR[0], eR[1], 0), Q(eR[0], eR[1], zb)], 0.5); line(pl, [Q(ap[0], ap[1], 0), Q(ap[0], ap[1], zb)], 0.9);
    line(pl, [Q(ap[0], ap[1], zb), Q(eR[0], eR[1], zb)], 0.4);
  }
  function towerFrontPlate(pl) {
    var x0 = -TW / 2, x1 = TW / 2, k;
    var face = [[x0, TT], [x1, TT], [x1, YG], [x0, YG]];
    pl.mask(face);
    masonry(pl, face, x0, x1, TT, YG, { base: 0.24, seed: 5, dark: function (x, y) { return 0.1 * sm(YG - 40, YG, y) + 0.07 * sm(x0 + 24, x1, x); } });
    line(pl, [[x0, TT], [x0, YG]], 0.5); line(pl, [[x1, TT], [x1, YG]], 1.0);
    // the plinth
    var pb = [[x0 - 3, YG - 7], [x1 + 3, YG - 7], [x1 + 3, YG + 1], [x0 - 3, YG + 1]], pt = [[x0 - 3, YG - 7], [x1 + 3, YG - 7], Q(x1 + 3, YG - 7, 3), Q(x0 - 3, YG - 7, 3)];
    pl.mask(pt); pl.mask(pb);
    hatch(pl, function (u, v) { return [mix(x0 - 3, x1 + 3, u), mix(YG - 7, YG + 1, v)]; }, function (u, v) { return clamp(0.3 + 0.4 * v + 0.1 * (fbm(u * 6, v) - 0.5), 0, 1); }, TW + 6, 8, { angle: 70 });
    line(pl, closed(pb), 0.5); line(pl, pt, 0.35);
    // a round-headed door, dark within, under a ring of voussoirs
    var dw = 10, dB = YG - 7, dS = YG - 38, door = [[-dw, dB], [-dw, dS]];
    for (k = 0; k <= 16; k++) door.push([-Math.cos(k / 16 * PI) * dw, dS - Math.sin(k / 16 * PI) * dw]);
    door.push([dw, dB]);
    var ring = []; for (k = 0; k <= 16; k++) ring.push([-Math.cos(k / 16 * PI) * (dw + 5), dS - Math.sin(k / 16 * PI) * (dw + 5)]);
    for (k = 16; k >= 0; k--) ring.push([-Math.cos(k / 16 * PI) * dw, dS - Math.sin(k / 16 * PI) * dw]);
    pl.mask(ring);
    hatchPoly(pl, ring, function (x, y) { return clamp(0.22 + 0.22 * sm(-6, 10, x) + 0.1 * sm(dS - 8, dS, y), 0, 1); }, { angle: 58 });
    for (k = 1; k < 7; k++) { var a = PI - k * PI / 7; line(pl, [[Math.cos(a) * dw, dS - Math.sin(a) * dw], [Math.cos(a) * (dw + 5), dS - Math.sin(a) * (dw + 5)]], 0.35); }
    line(pl, ring.slice(0, 17), 0.45);
    pl.mask(door);
    hatchPoly(pl, door, function (x, y) { return clamp(0.8 + 0.12 * sm(-dw, dw, x), 0, 1); }, { angle: 80, families: 3 });
    line(pl, door, 0.7);
    // a plain tablet over the door
    var tb = [[-15, TT + 12], [15, TT + 12], [15, TT + 23], [-15, TT + 23]];
    pl.mask(tb); line(pl, closed(tb), 0.55); line(pl, [[-13.6, TT + 21.8], [13.6, TT + 21.8]], 0.3); line(pl, [[13.8, TT + 13.4], [13.8, TT + 21.6]], 0.3);
    // the cornice, and the pediment over it with its raking cornice
    var cf = [[x0 - 3.5, TT - 1], [x1 + 3.5, TT - 1], [x1 + 3.5, TT + 5], [x0 - 3.5, TT + 5]];
    pl.mask(cf);
    hatch(pl, function (u, v) { return [mix(x0 - 3.5, x1 + 3.5, v), mix(TT - 1, TT + 5, u)]; }, function (u) { return clamp(0.12 + 0.5 * sm(0.35, 0.55, u) + 0.2 * sm(0.75, 1, u), 0, 1); }, 6, TW + 7, { angle: 80 });
    line(pl, [[x0 - 3.5, TT - 1], [x1 + 3.5, TT - 1]], 0.55); line(pl, [[x0 - 3.5, TT + 5], [x1 + 3.5, TT + 5]], 0.9);
    var sh = [[x0, TT + 5], [x1, TT + 5], [x1, TT + 8], [x0, TT + 8]]; pl.mask(sh);
    hatch(pl, function (u, v) { return [mix(x0, x1, v), mix(TT + 5, TT + 8, u)]; }, function (u) { return 0.7 - 0.35 * u; }, 3, TW, { angle: 70, families: 3 });
    var ped = [[x0 - 3.5, TT - 1], [x1 + 3.5, TT - 1], [0, TT - 1 - TPK]];
    pl.mask(ped);
    hatchPoly(pl, ped, function (x, y) { return clamp(0.22 + 0.16 * sm(TT - 1 - TPK, TT - 1, y) + 0.08 * sm(-10, 30, x), 0, 1); }, { angle: 4, families: 2 });
    var inset = [[x0 + 5, TT - 3.2], [x1 - 5, TT - 3.2], [0, TT - TPK + 3.4]];
    line(pl, closed(ped), 0.7); line(pl, closed(inset), 0.4);
    // a corbel on the right face at the springing, for the first arch's centering, and an impost band across the front
    var cb = [[x1, YS + 5], [x1 + 7, YS + 5], [x1 + 7, YS + 11], [x1, YS + 11]];
    pl.mask(cb); hatch(pl, function (u, v) { return [mix(x1, x1 + 7, u), mix(YS + 5, YS + 11, v)]; }, function (u, v) { return clamp(0.3 + 0.5 * v + 0.15 * u, 0, 1); }, 7, 6, { angle: 70 });
    line(pl, closed(cb), 0.4);
    var im = [[x0 - 2.5, YS - 1], [x1 + 2.5, YS - 1], [x1 + 2.5, YS + 5], [x0 - 2.5, YS + 5]];
    pl.mask(im); hatch(pl, function (u, v) { return [mix(x0 - 2.5, x1 + 2.5, u), mix(YS - 1, YS + 5, v)]; }, function (u, v) { return 0.16 + 0.6 * sm(0.3, 1, v); }, TW + 5, 6, { angle: 75 });
    line(pl, closed(im), 0.45);
  }

  // ---------------------------------------------------------------- the chain the picture follows
  /* Scale: a stone is a batch of 2^n blocks, n chosen from the measured block rate so that stones are set at a pace the
     eye can follow however fast the network runs (and from the backlog while catching up). An arch keeps the batch it
     was begun with, so its tablet always reads the exact heights it holds. The next stone is hoisted up the rope as its
     blocks arrive: a half-filled batch hangs half way. */
  // parts already cut on this page, by size, so a picture mounted again (a screen revisited) starts at once
  var CUTS = {}, CUTKEYS = [];
  function mount(canvas, opts) {
    opts = opts || {};
    var g = canvas.getContext('2d'), sprites = null, W = 1, H = 1, camX = 0, camY = TOP, viewW = 1, viewH = 1, running = true, raf = 0, lastT = null;
    var reduced = !!opts.reduced, PACE = opts.stonesPerSecond || 3, DBG = opts.debug || {};
    // chat mode: one arch per answer, running out of a water house; the rows shown are cropped to the aqueduct itself
    var CHAT = !!opts.chat;
    var VT = opts.top != null ? opts.top : CHAT ? TT - 1 - TPK - (D + 3) * OY - 6 : TOP, VB = opts.bot != null ? opts.bot : CHAT ? YG + 30 : BOT;
    var BAYS = opts.bays || 0, TFONT = opts.tabletFont || (CHAT ? 11 : 6);
    // The sky and the ground are big, quiet pictures that only slide. Given two elements behind the canvas (opts.far and
    // opts.near, each filling it), they become CSS backgrounds moved by transforms on the compositor, which costs next to
    // nothing per frame; the canvas then holds only the aqueduct. Without them they are drawn on the canvas.
    var LAYERS = opts.far && opts.near ? [{ el: opts.far, key: 'far', par: 0.35, period: TILE }, { el: opts.near, key: 'near', par: 1, period: NTILE }] : null;
    if (LAYERS) LAYERS.forEach(function (L) { L.el.style.overflow = 'hidden'; L.inner = document.createElement('div'); L.inner.style.cssText = 'position:absolute;left:0;top:0;height:100%;will-change:transform;background-repeat:repeat-x;background-position:0 0;'; L.el.appendChild(L.inner); });
    function layerImages() {
      if (!LAYERS) return;
      LAYERS.forEach(function (L) {
        var sp = sprites[L.key], ver = (L.ver || 0) + 1; L.ver = ver; L.ready = false;
        sp.c.toBlob(function (b) { if (L.ver !== ver || !b) return; if (L.url) URL.revokeObjectURL(L.url); L.url = URL.createObjectURL(b);
          L.inner.style.backgroundImage = 'url(' + L.url + ')'; L.inner.style.backgroundSize = (sp.c.width / myDPR) + 'px ' + (sp.c.height / myDPR) + 'px'; L.ready = true; if (typeof wake === 'function') wake(); });
      });
    }
    function slideLayers(t) {
      if (!LAYERS) return false;
      var ok = true;
      LAYERS.forEach(function (L) {
        if (!L.ready) { ok = false; return; }
        var sp = sprites[L.key], wcss = sp.c.width / DPR, ox = ((camX * L.par * K) % wcss + wcss) % wcss, y = (sp.y0 - camY) * K;
        L.inner.style.width = (W / DPR + 2 * wcss) + 'px'; L.inner.style.height = (sp.c.height / DPR) + 'px';
        L.inner.style.transform = 'translate3d(' + (-ox).toFixed(2) + 'px,' + y.toFixed(2) + 'px,0)';
      });
      return ok;
    }
    var st = { inited: false, top: -1, fin: -1, arches: [], tx: {}, hist: [], rate: 0, txRate: 0, lastBlock: 0, nextSet: 0, gb: 1,
      glints: [], dust: [], spawn: [], waterX: null, wheel: 0, wheelTarget: 0, catching: false, phase: 'live', progress: 1, clock: 0,
      q: { fed: false, total: 0, pend: 0, hist: [], rate: 0, qb: 1 }, surges: [], surgeAt: [], splashes: [], flow: 1, flowPh: 0, craneX: null, cur: null, slowFirst: null };
    function cur() { return CHAT ? st.cur : st.arches[st.arches.length - 1]; }
    function Xa(i) { return i * B; }
    function towerX() { return Xa(0) - B / 2 + P / 2 - TW / 2; }
    function waterStart() { return CHAT ? towerX() + TW / 2 : st.arches.length ? Xa(st.arches[0].i) - B / 2 : 0; }
    function site() { return CHAT ? (st.cur ? st.cur.i : st.arches.length) : cur().i; }
    // size: watched with a ResizeObserver rather than measured every frame (a layout read per frame costs, and forces
    // layout when the page has just changed); the device pixel ratio is capped at 2, and lowered on a machine where
    // frames run long
    var needSize = true, dprCap = 2, lastDpr = window.devicePixelRatio || 1;
    var ro = typeof ResizeObserver !== 'undefined' ? new ResizeObserver(function () { needSize = true; if (typeof wake === 'function') wake(); }) : null;
    if (ro) ro.observe(canvas);
    var myK = 1, myDPR = 1;
    function use() { K = myK; DPR = myDPR; }
    function resize() {
      var wd = window.devicePixelRatio || 1; if (wd !== lastDpr) { lastDpr = wd; needSize = true; }
      if (ro && !needSize && sprites) { use(); return; }
      needSize = false;
      var r = canvas.getBoundingClientRect(), dpr = Math.min(wd, dprCap);
      var w = Math.max(1, Math.round(r.width * dpr)), h = Math.max(1, Math.round(r.height * dpr));
      var k = Math.min(r.height / (VB - VT), r.width / ((BAYS || (r.width < 700 ? 2.5 : 5.2)) * B));
      if (w === canvas.width && h === canvas.height && sprites && Math.abs(k - myK) < 1e-3 && dpr === myDPR) { use(); return; }
      canvas.width = w; canvas.height = h; W = w; H = h; myDPR = dpr; myK = k; use();
      viewW = r.width / K; viewH = r.height / K;
      camY = mix(VT, VB, 0.52) - viewH / 2;
      cut();
    }
    function cut() {
      var t0 = performance.now();
      var cy = mix(VT, VB, 0.52) - viewH / 2, vy0 = cy - 30, vy1 = cy + viewH + 90;
      var key = [K.toFixed(4), DPR, CHAT ? 1 : 0, Math.round(vy0), Math.round(vy1)].join('|');
      if (CUTS[key]) { sprites = CUTS[key]; st.cutMs = 0; layerImages(); return; }
      sprites = {
        pier: bake(-P / 2 - 10, YS - 20, P / 2 + D * OX + 10, YG + 3, function (pl) { pierPlate(pl); }),
        rows: [], stones: [], soffits: [],
        centering: bake(-R - 12, YS - R - 4 - D * OY, R + 12 + D * OX, YG + 3, centeringPlate),
        spandrel: bake(-B / 2, YCB - 1, B / 2, YS + 1, spandrelPlate),
        channel: bake(-B / 2 - 1, YPT - D * OY - 2, B / 2 + D * OX + 1, YCB + 4, function (pl) { channelPlate(pl, false); }),
        cap: bake(-B / 2 - 1, YPT - D * OY - 2, B / 2 + D * OX + 1, YCB + 4, function (pl) { channelPlate(pl, true); }),
        far: bake(0, Math.max(TOP - 330, vy0), TILE, YG + 1, function (pl) { farPlate(pl, vy0); }, true),
        near: bake(0, YG - 1, NTILE, Math.min(YG + 340, vy1), function (pl) { nearPlate(pl, vy1); }, true),
        crane: bake(-B * 1.1, YCB - 56, 150, YG + 8, cranePlate),
        dust: [bake(-14, -8, 14, 8, function (pl) { dustPlate(pl, 1); }), bake(-14, -8, 14, 8, function (pl) { dustPlate(pl, 2); })]
      };
      if (CHAT) {
        sprites.towerSide = bake(-2, TT - 1 - TPK - (D + 3) * OY - 3, TW / 2 + 3.5 + (D + 3) * OX + 2, YG + 3, towerSidePlate);
        sprites.towerFront = bake(-TW / 2 - 5, TT - 1 - TPK - 2, TW / 2 + 9, YG + 3, towerFrontPlate);
      }
      for (var r = 1; r <= ROWS; r++) (function (r) { sprites.rows[r] = bake(-P / 2 - 10, YG - r * RH - 16, P / 2 + D * OX + 10, YG + 3, function (pl) { pierPlate(pl, r); }); })(r);
      for (var i = 0; i < NV; i++) (function (i) {
        var rOut = i === 8 ? RO + KEY : RO, bb = stonePoly(i, rOut).concat(stonePoly(i, rOut).map(function (p) { return Q(p[0], p[1], D); }));
        var xs = bb.map(function (p) { return p[0]; }), ys = bb.map(function (p) { return p[1]; });
        sprites.stones[i] = bake(Math.min.apply(null, xs) - 2, Math.min.apply(null, ys) - 2, Math.max.apply(null, xs) + 2, Math.max.apply(null, ys) + 2, function (pl) { stonePlate(pl, i); });
        sprites.soffits[i] = aOf(i) > 3 * PI / 4 + 0.01 ? bake(-R - 2, YS - R - 2, -R + 40, YS + 2, function (pl) { soffitPlate(pl, i); }) : null;
      })(i);
      st.cutMs = Math.round(performance.now() - t0);
      CUTS[key] = sprites; CUTKEYS.push(key); while (CUTKEYS.length > 3) delete CUTS[CUTKEYS.shift()];
      layerImages();
    }
    var rcX = 0, rcY = 0, shiftCss = '';
    function snapCam() {
      var s = K * DPR, px = camX * s, py = camY * s, ix = Math.floor(px), iy = Math.floor(py);
      rcX = ix / s; rcY = iy / s;
      var fx = (px - ix) / DPR, fy = (py - iy) / DPR, css = fx > 0.001 || fy > 0.001 ? 'translate3d(' + (-fx).toFixed(3) + 'px,' + (-fy).toFixed(3) + 'px,0)' : '';
      if (css !== shiftCss) { shiftCss = css; canvas.style.transform = css; }
    }
    function blit(sp, x, y, alpha) {
      if (!sp || alpha <= 0.003) return;
      var s = K * DPR, X = Math.round((sp.x0 + x - rcX) * s), Y = Math.round((sp.y0 + y - rcY) * s);
      if (X > W || Y > H || X + sp.c.width < 0 || Y + sp.c.height < 0) return;
      g.globalAlpha = alpha == null ? 1 : Math.min(1, alpha); g.drawImage(sp.c, X, Y); g.globalAlpha = 1;
    }
    // a repeating strip, stitched edge to edge in whole device pixels so no seam shows
    function tiles(sp, ox, period) {
      var s = K * DPR, w = sp.c.width, X0 = Math.round(-ox * s) % w; if (X0 > 0) X0 -= w;
      var Y = Math.round((sp.y0 - rcY) * s); for (var X = X0; X < W; X += w) g.drawImage(sp.c, X, Y);
    }
    function sx(x) { return (x - rcX) * K * DPR; } function sy(y) { return (y - rcY) * K * DPR; }
    // many thin strokes of one weight and one strength, stroked once
    function Batch(wpx, a) { this.w = wpx; this.a = a; this.p = new Path2D(); this.n = 0; }
    Batch.prototype.seg = function (x0, y0, x1, y1) { this.p.moveTo(sx(x0), sy(y0)); this.p.lineTo(sx(x1), sy(y1)); this.n++; };
    Batch.prototype.done = function () { if (!this.n || this.a <= 0.01) return; g.globalAlpha = Math.min(1, this.a); g.lineWidth = Math.max(0.5, this.w * DPR); g.stroke(this.p); g.globalAlpha = 1; };
    function seg(x0, y0, x1, y1, wpx, a) { if (a <= 0.01) return; g.globalAlpha = Math.min(1, a); g.lineWidth = Math.max(0.5, wpx * DPR); g.beginPath(); g.moveTo(sx(x0), sy(y0)); g.lineTo(sx(x1), sy(y1)); g.stroke(); }

    // ---- the feed
    function newArch(i, h0, b) { return { i: i, h0: h0, b: b, next0: h0, h1: null, set: 0, anim: [], raised: null, struck: null, begun: st.clock }; }
    function init(h, fin, o) {
      o = o || {};
      st.inited = true; st.top = h; st.fin = fin == null ? h : Math.min(fin, h); st.lastBlock = st.clock; st.hist = [[st.clock, h]];
      var b = o.batch || 1, h0 = h + 1 - ((h + 1) % (NV * b)), c = newArch(0, h0, b);
      c.set = Math.floor((h + 1 - h0) / b); c.next0 = h0 + c.set * b;
      st.arches = [];
      if (o.history !== false) for (var k = 12; k >= 1; k--) { var ha = newArch(-k, h0 - k * NV * b, b); ha.set = NV; ha.h1 = ha.h0 + NV * b - 1; ha.next0 = ha.h1 + 1; ha.raised = -1e9; if (ha.h1 <= st.fin) ha.struck = -1e9; st.arches.push(ha); }
      if (o.history === false) c.begun = -1e9; else camX = Xa(c.i) - viewW * (viewW < 520 ? 0.34 : 0.58);
      // starting from nothing, the first arch is set stone by stone however far behind the chain is: then the rest race in
      st.slowFirst = o.history === false ? c.i : null;
      st.arches.push(c); st.waterX = null;
    }
    function block(h, tx) {
      if (!st.inited) init(h - 1, h - 1);
      if (h <= st.top) return;
      if (h - st.top < 5000) for (var k = st.top + 1; k < h; k++) st.tx[k] = st.tx[k] || 0;
      st.tx[h] = (st.tx[h] || 0) + (tx || 0);
      st.top = h; st.lastBlock = st.clock; st.hist.push([st.clock, h]);
    }
    function final(h) { if (h > st.fin) st.fin = Math.min(h, st.top); }
    var ph = { name: null, t0: 0, p: 0 }, STARTS = ['survey', 'model', 'rpc', 'peers'];
    function phase(name, p) { if (name !== ph.name) { ph.name = name; ph.t0 = st.clock; } ph.p = p == null ? (name === 'model' ? null : 1) : clamp(p, 0, 1); st.phase = name; st.progress = ph.p; }
    // inference requests: counted, measured, and turned into surges of water
    function queries(n) {
      var q = st.q; if (!q.fed) { q.fed = true; q.t0 = st.clock; } n = Math.max(0, Math.floor(n || 0)); if (!n) return;
      q.total += n; q.pend += n; q.hist.push([st.clock, n]);
    }
    function measure() {
      if (!st.hist.length) { st.rate = 0; return; }
      while (st.hist.length > 2 && st.clock - st.hist[1][0] > 10) st.hist.shift();
      var a = st.hist[0], b = st.hist[st.hist.length - 1], span = Math.max(st.clock - a[0], 0.5);
      st.rate = (b[1] - a[1]) / span;
    }
    function chooseBatch(prev) {
      // catching up, the stones go faster (still one at a time, still followable): the chain racing in
      var c = cur(), behind = c ? st.top - (c.next0 - 1) : 0, eff = Math.max(st.rate, behind / 10), pace = st.phase === 'sync' || behind > 2000 ? PACE * 3.5 : PACE, want = Math.max(1, eff / pace), b = prev || 1;
      if (want > b * 2.5 || want < b / 2.5) b = Math.pow(2, Math.max(0, Math.round(Math.log(want) / Math.LN2)));
      return b;
    }

    // ---- chat: one arch per answer; each call below is a real event in the conversation
    var TAU = 36;   // tokens: stone k (of the sixteen before the keystone) is up once 16 (1 - e^(-tokens / TAU)) passes k + 1
    function chatArch(i) { return { i: i, set: 0, anim: [], raised: null, struck: null, begun: st.clock, tokens: 0, done: false, ok: null, rec: null, recT: null, failed: null, retry: null }; }
    function load(list) {
      st.inited = true; st.arches = []; st.cur = null; st.waterX = null;
      (list || []).forEach(function (a, i) {
        var c = chatArch(i); c.set = NV; c.done = true; c.raised = -1e9; c.begun = -1e9;
        c.ok = a && a.checked != null ? !!a.checked : null; if (c.ok) c.struck = -1e9;
        if (a && a.height != null) { c.rec = a.height; c.recT = -1e9; }
        st.arches.push(c);
      });
    }
    function ask() {
      if (!st.inited) load([]);
      var c = st.cur;
      if (c && c.failed == null) return c.i;
      if (c) { c.failed = null; c.set = 0; c.anim = []; c.tokens = 0; c.done = false; c.retry = st.clock; st.nextSet = st.clock + 0.6; return c.i; }
      c = chatArch(st.arches.length); st.arches.push(c); st.cur = c; st.nextSet = st.clock + 1.1;
      return c.i;
    }
    function stream(tokens) { var c = st.cur; if (c && c.failed == null) c.tokens = Math.max(c.tokens, tokens || 0); }
    function answered() { var c = st.cur; if (c && c.failed == null) c.done = true; }
    function archAt(i) { if (i == null) { for (var j = st.arches.length - 1; j >= 0; j--) if (st.arches[j].set === NV || st.arches[j] === st.cur) return st.arches[j]; return null; } for (var k = 0; k < st.arches.length; k++) if (st.arches[k].i === i) return st.arches[k]; return null; }
    function checked(ok, i) { var a = archAt(i); if (a) a.ok = !!ok; }
    function recorded(h, i) { var a = archAt(i); if (a && h != null) { a.rec = h; a.recT = st.clock; } }
    function failed() { var c = st.cur; if (c && c.failed == null) c.failed = st.clock; }
    function stonesFor(tok) { return Math.min(NV - 1, Math.floor((NV - 1) * (1 - Math.exp(-tok / TAU)))); }
    function tokensFor(k) { return k <= 0 ? 0 : k >= NV - 1 ? Infinity : -TAU * Math.log(1 - k / (NV - 1)); }
    function updateChat(t) {
      var c = st.cur;
      if (c && c.failed != null) { if (t - c.failed > 0.7 && c.set) { c.set = 0; c.anim = []; } }
      else if (c) {
        var want = c.done ? NV : stonesFor(c.tokens), ready = t - (c.retry != null ? c.retry : c.begun) > (c.retry != null ? 0.5 : 1.05);
        if (ready && c.set < want && t >= st.nextSet) {
          var k = c.set; c.anim[k] = reduced ? -1e9 : t; c.set++; st.wheelTarget += 1 / NV;
          if (!reduced) { var si = ORDER[k], m = onArc(0, (R + RO) / 2, (aOf(si) + aOf(si + 1)) / 2); st.dust.push({ x: Xa(c.i) + m[0], y: m[1], t: t, k: k % 2 }); }
          st.nextSet = t + (reduced ? 0 : c.done ? 0.085 : 0.16);
          // the keystone is in: the wall and the channel go up over it, and the answer runs down the channel into it
          if (c.set === NV) { c.raised = reduced ? -1e9 : t + 0.25; st.cur = null; st.surgeAt.push({ t: t + 0.25, v: 280, n: 1 }); }
        }
      }
      // an answer a second computer agreed with stands on its own: its centering is struck once the wall is up
      st.arches.forEach(function (a) { if (a.ok && a.struck == null && a.raised != null && t > a.raised + 0.9) a.struck = reduced ? -1e9 : t; });
      // the water runs while an answer is coming in, and lies still between answers
      if (!st.q.fed) { var want2 = st.cur && st.cur.failed == null && (st.cur.tokens > 0 || st.cur.done) ? 1 : st.surges.length ? 0.6 : 0.12; st.flow += (want2 - st.flow) * 0.05; }
    }

    // ---- the picture's own motion
    function updateChain(t, dt) {
      measure();
      var c = cur(); if (!c) return;
      // stones whose blocks all exist, not yet set
      // when the chain slows (a sync finishing, a quiet hour), the stone still filling may take fewer blocks: never more
      // than it was begun with, never fewer than have already come in; its exact range still goes on the tablet
      var want = chooseBatch(1); if (want < c.b) c.b = Math.max(want, Math.min(c.b, st.top - c.next0 + 1));
      var ready = Math.floor((st.top - c.next0 + 1) / c.b), far = ready > NV * 6, first = st.slowFirst === c.i;
      st.catching = ready > NV || st.rate / c.b > PACE * 3;
      if (first) far = false;
      var perSec = clamp(Math.max(st.rate / c.b * 1.05, ready / 4), 1, 14);
      var gap = reduced || far ? 0 : first ? 0.11 : 1 / perSec;
      var n = 0;
      while (ready > 0 && (gap === 0 || t >= st.nextSet) && n < (gap === 0 ? NV * 2 : 2)) {
        var k = c.set, e0 = c.next0, e1 = e0 + c.b - 1, txs = 0;
        for (var hh = e0; hh <= e1; hh++) { txs += st.tx[hh] || 0; delete st.tx[hh]; }
        c.anim[k] = reduced || gap === 0 ? -1e9 : t; c.set++; n++; ready--; st.setCount = (st.setCount || 0) + 1;
        var gl = Math.min(24, Math.round(txs / st.gb)); for (var q = 0; q < gl; q++) st.spawn.push(t + Math.random() * 1.2);
        if (!reduced && gap > 0) { var si = ORDER[k], m = onArc(0, (R + RO) / 2, (aOf(si) + aOf(si + 1)) / 2); st.dust.push({ x: Xa(c.i) + m[0], y: m[1], t: t, k: k % 2 }); }
        st.wheelTarget += 1 / NV;
        c.next0 = e1 + 1;
        if (c.set === NV) { c.h1 = e1; c.raised = reduced || gap === 0 ? -1e9 : t; st.arches.push(newArch(c.i + 1, c.h1 + 1, chooseBatch(c.b))); c = cur(); }
        else c.b = chooseBatch(c.b);
        ready = Math.floor((st.top - c.next0 + 1) / c.b);
        st.nextSet = t + gap;
      }
      // transactions: one glint for gb of them, gb chosen so the water never boils
      st.txRate = st.txRate * 0.97 + 0.03 * (st.rate * 20);
      st.gb = Math.pow(2, Math.max(0, Math.ceil(Math.log(Math.max(1, st.txRate / 24)) / Math.LN2)));
      // strike the centering under every finished arch whose last block is final
      st.arches.forEach(function (a) { if (a.set === NV && a.struck == null && a.h1 != null && a.h1 <= st.fin) a.struck = reduced || st.catching ? -1e9 : t + 0.15; });
      if (st.arches.length > 70) st.arches.splice(0, st.arches.length - 70);
    }
    function update(t, dt) {
      st.clock = t; if (!st.inited) return;
      if (CHAT) updateChat(t); else updateChain(t, dt);
      if (!CHAT && !cur()) return;
      // inference: the rate over the last half minute sets how fast the water runs and how many requests make a surge
      var q = st.q;
      if (q.fed) {
        while (q.hist.length && t - q.hist[0][0] > 30) q.hist.shift();
        var sum = 0; q.hist.forEach(function (h) { sum += h[1]; }); q.rate = sum / clamp(t - q.t0, 5, 30);
        q.qb = Math.pow(2, Math.max(0, Math.ceil(Math.log(Math.max(1, q.rate / 1.5)) / Math.LN2)));
        while (q.pend >= q.qb) { q.pend -= q.qb; st.surgeAt.push({ t: t + (reduced ? 0 : Math.random() * 0.5), v: 150 + Math.random() * 40, n: q.qb }); }
        var wantF = 0.12 + 0.88 * clamp(q.rate / 2, 0, 1); st.flow += (wantF - st.flow) * (1 - Math.exp(-dt * 0.8));
      }
      st.flowPh += dt * st.flow;
      // camera, water, wheel, crane
      var cy0 = mix(VT, VB, 0.52) - viewH / 2; camY = reduced ? cy0 : camY + (cy0 - camY) * (1 - Math.exp(-dt * 1.2));
      var target;
      if (CHAT) { var xL = towerX() - TW / 2 - 26, xR = Xa(site()) + B * 1.05 + 158; target = xR - xL <= viewW ? (xL + xR) / 2 - viewW / 2 : xR - viewW; }   // a short aqueduct sits in the middle; a long one follows its end
      else target = Xa(cur().i) + B * (cur().set / NV - 0.5) - viewW * (viewW < 520 ? 0.34 : 0.58);
      // a critically damped follow: the camera's speed changes smoothly, never in steps, however the target moves
      var dx = target - camX;
      if (reduced || st.camFresh) { camX = target; st.camV = 0; }
      else if (dt > 0) {
        var T = Math.abs(dx) > B * 2 ? 0.3 : 0.85, om = 2 / T, x = om * dt, ex = 1 / (1 + x + 0.48 * x * x + 0.235 * x * x * x), ch = -dx, tmp = ((st.camV || 0) + om * ch) * dt;
        st.camV = ((st.camV || 0) - om * tmp) * ex; camX = target + (ch + tmp) * ex;
      }
      st.camFresh = false;
      var wt = waterTarget(t); if (st.waterX == null || reduced) st.waterX = wt; else st.waterX += (wt - st.waterX) * (1 - Math.exp(-dt * 1.5));
      st.wheel += (st.wheelTarget - st.wheel) * (1 - Math.exp(-dt * 5));
      var cx = Xa(site()) + B * 1.05; st.craneX = st.craneX == null || reduced ? cx : st.craneX + (cx - st.craneX) * (1 - Math.exp(-dt * 2.2));
      st.spawn.sort(function (p, r) { return p - r; });
      while (st.spawn.length && st.spawn[0] <= t) { st.spawn.shift(); if (st.glints.length < 80) st.glints.push({ x: camX - 10 - Math.random() * 40, y: mix(12.6, 19.6, Math.random()), v: 50 + Math.random() * 40, t: t }); }
      var wx0 = waterStart();
      st.glints = st.glints.filter(function (gl) { if (gl.x < wx0) gl.x = wx0 + Math.random() * 20; gl.x += gl.v * dt; return gl.x < st.waterX - 1; });
      // surges: each starts where the water enters the picture and runs to the end of the water, where it breaks
      st.surgeAt.sort(function (p, r) { return p.t - r.t; });
      while (st.surgeAt.length && st.surgeAt[0].t <= t) {
        var sg = st.surgeAt.shift(), xs = Math.max(wx0 + 2, camX - 30);
        if (CHAT || xs < st.waterX - 12) { if (st.surges.length < 14) st.surges.push({ x: xs, v: sg.v, n: sg.n, len: 36 + 9 * Math.log(sg.n + 1) / Math.LN2, t: t }); }
      }
      st.surges = st.surges.filter(function (s) {
        if (reduced) return false;
        s.x += s.v * dt;
        if (s.x >= st.waterX - 1) {
          if (wt - st.waterX > 4) { s.x = st.waterX - 1; return true; }   // riding the front while the water runs on into a new span
          if (st.waterX > camX && st.splashes.length < 10) st.splashes.push({ x: st.waterX, t: t, n: s.n }); return false;
        }
        return true;
      });
      st.splashes = st.splashes.filter(function (s) { return t - s.t < 0.75; });
      st.dust = st.dust.filter(function (d) { return t - d.t < 0.7; });
    }
    function waterTarget(t) {
      var last = null;
      if (CHAT) { st.arches.forEach(function (a) { if (a.set === NV && a.raised != null && t > a.raised + 0.45) last = a; }); return last ? Xa(last.i) + B / 2 : waterStart(); }
      st.arches.forEach(function (a) { if (a.struck != null && a.set === NV) last = a; });
      return last ? Xa(last.i) + B / 2 : Xa(st.arches[0].i) - B / 2;
    }

    // ---- drawing
    function render(t) {
      g.setTransform(1, 0, 0, 1, 0, 0); g.globalAlpha = 1;
      g.lineCap = 'round'; g.strokeStyle = '#ffffff';
      snapCam();
      if (slideLayers(t)) g.clearRect(0, 0, W, H);
      else { g.fillStyle = ARC; g.fillRect(0, 0, W, H); if (!DBG.noTiles) { tiles(sprites.far, rcX * 0.35, TILE); tiles(sprites.near, rcX, NTILE); } }
      var i;
      var C = cur(), S = site(), iL = Math.floor((camX - B) / B), iR = Math.ceil((camX + viewW + B) / B);
      var vis = st.arches.filter(function (a) { return a.i >= iL && a.i <= iR; });
      // surveyor's stakes and ranging poles where the next piers will stand
      for (i = CHAT ? (C ? S + 1 : S) : C.i + 2; i <= iR + 1; i++) if (!CHAT || i >= 0) stakes(Xa(i) + B / 2);
      if (CHAT) blit(sprites.towerSide, towerX(), 0);
      // piers: all of them up to the far side of the arch being built; the next one rising as it goes up
      if (CHAT) vis.forEach(function (a) {
        if (a.i > 0) blit(sprites.pier, Xa(a.i) - B / 2, 0);
        if (a === C && a.begun > -1e8 && a.retry == null) { var rw = Math.max(1, Math.round(ROWS * sm(0, 0.6, t - a.begun))); if (rw >= ROWS) blit(sprites.pier, Xa(a.i) + B / 2, 0); else blit(sprites.rows[rw], Xa(a.i) + B / 2, 0); }
        else blit(sprites.pier, Xa(a.i) + B / 2, 0);
      });
      else {
        vis.forEach(function (a) { blit(sprites.pier, Xa(a.i) - B / 2, 0); blit(sprites.pier, Xa(a.i) + B / 2, 0); });
        var rows = Math.floor(C.set / NV * ROWS + 1e-4); if (rows > 0) blit(sprites.rows[rows], Xa(C.i + 1) + B / 2, 0);
      }
      // under each arch: the soffit stones the eye reaches, then the centering while it stands
      vis.forEach(function (a) {
        for (var k = 0; k < a.set; k++) { var si = ORDER[k]; if (sprites.soffits[si]) blit(sprites.soffits[si], Xa(a.i), 0, landed(a, k, t) * gone(a, t)); }
        var drop = 0, alpha = 1;
        if (a.struck != null) { var e = t - a.struck; if (a.struck < 0 || e > 1.4) return; drop = sm(0, 0.35, e) * 3 + sm(0.35, 1.4, e) * 10; alpha = 1 - sm(0.45, 1.4, e); }
        if (a === C && a.begun > 0 && !(CHAT && a.retry != null)) { var eb = t - a.begun - (CHAT ? 0.5 : 0); drop = Math.min(drop, -34 * (1 - sm(0, 0.55, eb))); alpha = Math.min(alpha, sm(0, 0.35, eb)); }
        blit(sprites.centering, Xa(a.i), drop, alpha);
      });
      // the stones, the newest dropping into place
      vis.forEach(function (a) {
        var ga = gone(a, t), gd = (1 - ga) * 8;
        for (var k = 0; k < a.set; k++) { if (a.set === NV && ORDER[k] === 8) continue; var e2 = a.anim[k] == null ? 1 : clamp((t - a.anim[k]) / 0.3, 0, 1);
          if (e2 >= 1 || !st.hook) { blit(sprites.stones[ORDER[k]], Xa(a.i), gd, ga); continue; }
          var si2 = ORDER[k], m2 = onArc(0, (R + RO) / 2, (aOf(si2) + aOf(si2 + 1)) / 2), f = 1 - Math.pow(1 - e2, 3), hx = st.hook[0] - (Xa(a.i) + m2[0]), hy = st.hook[1] - m2[1];
          blit(sprites.stones[si2], Xa(a.i) + hx * (1 - f), hy * (1 - f) - 10 * Math.sin(PI * f) + gd, ga); }
      });
      // the wall over each finished arch rising course by course, its keystone, its channel
      var capA = null;
      if (CHAT) st.arches.forEach(function (a) { if (a.set === NV) capA = a; }); else capA = st.arches[st.arches.length - 2];
      vis.forEach(function (a) {
        if (a.set < NV) return;
        var pr = a.raised == null || a.raised < 0 ? 1 : sm(0, 0.7, t - a.raised);
        if (CHAT && a.raised != null && t < a.raised) {   // the keystone's last drop, before the wall goes up
          var ek = clamp(1 - (a.raised - t) / 0.25, 0, 1); blit(sprites.stones[8], Xa(a.i), -14 * Math.pow(1 - ek, 2)); return;
        }
        if (pr < 1) { g.save(); g.beginPath(); g.rect(0, sy(mix(YS + 2, YCB - 30, pr)), W, H); g.clip(); }
        blit(sprites.spandrel, Xa(a.i), 0); blit(sprites.stones[8], Xa(a.i), 0);
        blit(a === capA ? sprites.cap : sprites.channel, Xa(a.i), 0);
        if (pr < 1) g.restore();
        if (CHAT && pr < 1) blit(sprites.stones[8], Xa(a.i), 0);
      });
      if (CHAT) blit(sprites.towerFront, towerX(), 0);
      if (!DBG.noWater) water(t);
      // what each finished arch holds, cut in its tablet: its heights, or in a conversation the block that recorded it
      g.fillStyle = '#ffffff'; g.textAlign = 'center'; g.textBaseline = 'middle';
      // numbers in the text face the pages use for every other number (Marcellus is kept for words)
      g.font = '500 ' + (TFONT * K * DPR).toFixed(1) + 'px "Hanken Grotesk", "Helvetica Neue", Arial, sans-serif'; if ('letterSpacing' in g) g.letterSpacing = (0.12 * K * DPR * TFONT / 6).toFixed(2) + 'px';
      vis.forEach(function (a) {
        if (a.set < NV) return; var pr2 = a.raised == null || a.raised < 0 ? 1 : sm(0.5, 0.9, t - a.raised); if (pr2 <= 0.01) return;
        var text = CHAT ? (a.rec != null ? fmt(a.rec) : '') : fmt(a.h0) + ' – ' + fmt(a.h1); if (!text) return;
        var ra = CHAT && a.recT != null && a.recT > 0 ? sm(0, 0.9, t - a.recT) : 1;
        g.globalAlpha = 0.94 * pr2 * ra; g.fillText(text, sx(Xa(a.i)), sy((YPT + YCT) / 2 + 0.3));
      });
      g.globalAlpha = 1;
      st.dust.forEach(function (d) { var e3 = (t - d.t) / 0.7; blit(sprites.dust[d.k], d.x, d.y + e3 * 3, 0.9 * (1 - e3)); });
      if (!DBG.noCrane) crane(t, C);
    }
    function gone(a, t) { return CHAT && a.failed != null ? 1 - sm(0, 0.6, t - a.failed) : 1; }
    function renderStart(t) {
      g.setTransform(1, 0, 0, 1, 0, 0); g.globalAlpha = 1;
      g.lineCap = 'round'; g.strokeStyle = '#ffffff';
      var k = STARTS.indexOf(ph.name); if (k < 0) k = 0;
      camX = Xa(0) - viewW * 0.45;
      // while there are only footings, look lower, at the ground where the work is; rise with the work
      var shift = Math.max(0, Math.min(viewH * 0.18, 60)) * [1, 0.85, 0.35, 0.15][k], dtS = st.lastRS == null ? 1 : Math.min(0.1, t - st.lastRS); st.lastRS = t;
      st.camY0 = mix(VT, VB, 0.52) - viewH / 2; var cyT = st.camY0 + shift;
      camY = st.startCamY == null || reduced ? cyT : camY + (cyT - camY) * (1 - Math.exp(-dtS * 2)); st.startCamY = camY;
      snapCam();
      if (slideLayers(t)) g.clearRect(0, 0, W, H);
      else { g.fillStyle = ARC; g.fillRect(0, 0, W, H); tiles(sprites.far, rcX * 0.35, TILE); tiles(sprites.near, rcX, NTILE); }
      for (var i = 1; i <= 5; i++) stakes(Xa(i) + B / 2);
      // checking files: the surveyor's stakes and poles for the first two piers
      if (k === 0) { stakes(Xa(0) - B / 2); stakes(Xa(0) + B / 2); }
      // loading the model: the first piers rise course by course with the real progress
      if (k >= 1) { var rows = k === 1 ? (ph.p == null ? 1 : Math.max(1, Math.floor(ph.p * ROWS + 1e-4))) : ROWS;
        if (k === 1 && ph.p == null && !reduced) [-1, 1].forEach(function (sd, j) { var e = (t * 0.7 + j * 0.5) % 1; blit(sprites.dust[j], Xa(0) + sd * B / 2 + (j ? 6 : -8), YG - RH - 2 - e * 4, 0.7 * Math.sin(e * PI)); });
        [-1, 1].forEach(function (sd) { var x = Xa(0) + sd * B / 2; if (rows >= ROWS) blit(sprites.pier, x, 0); else if (rows > 0) blit(sprites.rows[rows], x, 0); else stakes(x); }); }
      // opening the RPC port: the centering is lowered onto the corbels
      if (k >= 2) { var e = k === 2 ? t - ph.t0 : 9; blit(sprites.centering, Xa(0), -34 * (1 - sm(0, 0.8, e)), sm(0, 0.4, e)); }
      // finding peers: the crane is raised beside the work
      if (k >= 3) { var e2 = k === 3 ? t - ph.t0 : 9, a = sm(0, 0.8, e2); g.save(); g.globalAlpha = a; blit(sprites.crane, Xa(0) + B * 1.05, 26 * (1 - a), a); g.restore(); }
      st.craneX = Xa(0) + B * 1.05;
    }
    function landed(a, k, t) { if (a.anim[k] == null) return 1; return sm(0.15, 0.3, (t - a.anim[k]) / 0.26); }
    function stakes(px) {
      var a = new Batch(0.7, 0.85), b = new Batch(0.35, 0.6), c = new Batch(1.3, 0.85);
      a.seg(px - P / 2, YG + 1, px - P / 2, YG - 7); a.seg(px + P / 2, YG + 1, px + P / 2, YG - 7); a.seg(px, YG, px, YG - 60);
      b.seg(px - P / 2, YG - 5, px + P / 2, YG - 5); b.seg(px - P / 2 - 40, YG - 3, px - P / 2, YG - 5);
      for (var y = YG; y > YG - 60; y -= 6) c.seg(px, y, px, y - 3);
      a.done(); b.done(); c.done();
    }
    function water(t) {
      var x0 = Math.max(camX - 20, waterStart()), x1 = st.waterX; if (x1 == null || x1 <= x0 + 0.5) return;
      var top = 11.2, bot = 20.4;
      g.save(); g.beginPath();
      g.moveTo(sx(x0), sy(bot)); g.lineTo(sx(x1 + 1.8), sy(bot)); g.lineTo(sx(x1 + 12.6), sy(top)); g.lineTo(sx(x0), sy(top)); g.closePath();
      g.fillStyle = ARC; g.fill(); g.clip();
      // the surface catches the light a little, so the channel reads as water from far off
      var fl = clamp(st.flow, 0, 1); g.fillStyle = 'rgba(255,255,255,' + (0.12 + 0.12 * fl).toFixed(3) + ')'; g.fillRect(sx(x0) - 2, sy(top) - 2, sx(x1 + 14) - sx(x0) + 4, sy(bot) - sy(top) + 4);
      g.strokeStyle = '#ffffff';
      var wa = new Batch(0.34, 0.72 * (0.6 + 0.4 * fl)), wb = new Batch(0.46, 0.86 * (0.6 + 0.4 * fl)), gh = new Batch(0.7, 1), gv = new Batch(0.45, 0.8);
      var P0 = st.flowPh;
      for (var r = 0; r < 7; r++) {
        var y = mix(top + 0.8, bot - 0.6, r / 6), sp = 18 + r * 3, ph = (P0 * (22 + r * 4) + r * 37) % sp, pen = r % 2 ? wb : wa;
        for (var x = x0 - sp + ph; x < x1 + 14; x += sp) { var L = sp * (0.45 + 0.35 * hash(Math.floor((x - ph) / sp), r)); pen.seg(x, y, x + L, y); }
      }
      wa.done(); wb.done();
      st.glints.forEach(function (gl) { gh.seg(gl.x - 3.5, gl.y, gl.x + 3.5, gl.y); gv.seg(gl.x, gl.y - 1.6, gl.x, gl.y + 1.6); });
      gh.done(); gv.done();
      // surges: a run of brighter, closer water behind a leaning crest with a curl of foam on it
      if (st.surges.length) {
        var sb = new Batch(0.5, 0.95), sc = new Batch(1.05, 1), sf = new Batch(0.45, 0.9);
        // each surge a bright swell, brightest at its front, fading out behind
        st.surges.forEach(function (s) {
          var xa = s.x - s.len * 1.7, X0 = sx(xa), X1 = sx(s.x + 1.2); if (X1 - X0 < 1) return;
          var gr = g.createLinearGradient(X0, 0, X1, 0);
          gr.addColorStop(0, 'rgba(255,255,255,0)'); gr.addColorStop(0.5, 'rgba(255,255,255,0.36)'); gr.addColorStop(0.88, 'rgba(255,255,255,0.86)'); gr.addColorStop(1, 'rgba(255,255,255,1)');
          g.fillStyle = gr; g.fillRect(X0, sy(top) - 2, X1 - X0, sy(bot) - sy(top) + 4);
        });
        st.surges.forEach(function (s) {
          var L = s.len, xa = s.x - L;
          for (var r2 = 0; r2 < 5; r2++) { var y2 = mix(top + 1.1, bot - 0.9, r2 / 4);
            for (var k2 = 0; k2 < 6; k2++) { var u = (k2 + 0.5 * (r2 % 2)) / 6, xx = xa + u * L, l2 = L * 0.1 * (0.5 + u); if (xx + l2 < s.x - 1.5) sb.seg(xx, y2, xx + l2, y2); } }
          var c0 = [s.x - 3, bot - 0.4], c1 = [s.x - 0.6, mix(bot, top, 0.55)], c2 = [s.x + 1.2, top + 0.9];
          sc.seg(c0[0], c0[1], c1[0], c1[1]); sc.seg(c1[0], c1[1], c2[0], c2[1]);
          sf.seg(c2[0], c2[1], c2[0] + 1.8, top + 1.9); sf.seg(c2[0] + 1.8, top + 1.9, c2[0] + 2.1, top + 3.2);
          sf.seg(s.x - 4.2, bot - 0.6, s.x - 2.4, mix(bot, top, 0.5)); sf.seg(s.x - 5.6, bot - 0.8, s.x - 4.4, mix(bot, top, 0.4));
        });
        sb.done(); sc.done(); sf.done();
      }
      g.restore();
      var b0 = [x1 + 1.8, bot - 1.2], b1 = [x1 + 12.6, top - 1.2];
      seg(b0[0], b0[1], b1[0], b1[1], 1.4, 0.95); seg(b0[0] + 1, b0[1] - 2.2, b1[0] + 1, b1[1] - 2.2, 0.6, 0.8);
      for (var k = 0; k < 6; k++) { var q = lerp2(b0, b1, (k + 0.5) / 6); seg(q[0] - 1.2, q[1] + 0.5 + Math.sin(t * 6 + k) * 0.3, q[0] - 3.2, q[1] + 0.5, 0.35, 0.6); }
      // spray thrown up over each surge's crest, and where one breaks against the end of the water
      if (st.surges.length || st.splashes.length) {
        var dr = new Batch(0.55, 0.9);
        st.surges.forEach(function (s) { for (var j = 0; j < 3; j++) { var ph2 = (t * 3 + j * 0.37 + s.t) % 1, dx = 1 + j * 1.3 + ph2 * 2.5, dy = top - 0.6 - Math.sin(ph2 * PI) * (1.6 + j * 0.5); if (s.x + dx < x1) dr.seg(s.x + dx, dy, s.x + dx + 0.7, dy - 0.2); } });
        st.splashes.forEach(function (s) {
          var e = (t - s.t) / 0.75, a = 1 - e, n = Math.min(12, 6 + 2 * Math.log(s.n + 1) / Math.LN2);
          for (var j = 0; j < n; j++) { var an = mix(0.35, 1.45, hash(j, Math.floor(s.t * 10))) , v = 20 + 16 * hash(j + 7, 3), px = s.x + 6 + Math.cos(an) * v * e, py = top + 2 - Math.sin(an) * v * e + 34 * e * e;
            dr.seg(px, py, px + Math.cos(an) * 1.4 * a, py - Math.sin(an) * 1.4 * a + 0.8 * e); }
        });
        dr.a = 0.9; dr.done();
      }
    }
    function crane(t, C) {
      var cx = st.craneX == null ? Xa(site()) + B * 1.05 : st.craneX, head = [cx + CRANE.head[0], CRANE.head[1]], S = site();
      blit(sprites.crane, cx, 0);
      var wc = [cx, YG - CRANE.wheel - 5], Rw = CRANE.wheel, ang = st.wheel * 2 * PI;
      g.strokeStyle = '#ffffff';
      [Rw, Rw - 3].forEach(function (rr, j) { g.globalAlpha = 0.95; g.lineWidth = (j ? 0.6 : 1.1) * DPR; g.beginPath(); g.arc(sx(wc[0]), sy(wc[1]), rr * K * DPR, 0, 2 * PI); g.stroke(); });
      var spk = new Batch(0.7, 0.9), tth = new Batch(0.45, 0.7);
      for (var k = 0; k < 8; k++) { var a = ang + k * PI / 4; spk.seg(wc[0] + Math.cos(a) * 2.5, wc[1] + Math.sin(a) * 2.5, wc[0] + Math.cos(a) * (Rw - 3), wc[1] + Math.sin(a) * (Rw - 3)); }
      for (k = 0; k < 24; k++) { var a2 = ang + k * PI / 12; tth.seg(wc[0] + Math.cos(a2) * (Rw - 3), wc[1] + Math.sin(a2) * (Rw - 3), wc[0] + Math.cos(a2) * Rw, wc[1] + Math.sin(a2) * Rw); }
      spk.done(); tth.done();
      g.globalAlpha = 1; g.beginPath(); g.fillStyle = '#ffffff'; g.arc(sx(wc[0]), sy(wc[1]), 1.8 * K * DPR, 0, 2 * PI); g.fill();
      st.hook = [head[0], YCB - 12];
      // the next stone hangs straight under the pulley. Chain: it is hoisted as its blocks come in (a half-filled batch
      // hangs half way), and when they are all in it is swung onto its seat. Chat: while no computer has taken the prompt
      // it hangs low and sways; then it is hoisted as the words arrive; between answers the hook hangs empty.
      var si = -1, p = 1, swayA = 1.1;
      if (!CHAT) { var have = st.top - C.next0 + 1; si = ORDER[C.set % NV]; p = C.b > 1 ? clamp(have / C.b, 0, 1) : 1; }
      else if (C && C.failed == null && C.set < NV) {
        si = ORDER[C.set];
        if (C.done) p = 1; else if (C.tokens <= 0) { p = 0.22; swayA = 2.4; }
        else { var ta = tokensFor(C.set), tb = tokensFor(C.set + 1); p = tb === Infinity ? 0.9 : clamp((C.tokens - ta) / (tb - ta), 0, 1); }
      }
      var sway = Math.sin(t * 1.3) * swayA * (si < 0 ? 0.5 : Math.max(p, 0.5));
      if (si < 0) {
        var hk = [head[0] + sway, YCB - 26];
        seg(head[0], head[1] + 5, hk[0], hk[1], 0.55, 0.95);
        seg(hk[0] - 3, hk[1], hk[0] + 3, hk[1], 0.6, 0.9); seg(hk[0], hk[1], hk[0], hk[1] + 3, 0.6, 0.9); seg(hk[0], hk[1] + 3, hk[0] + 1.6, hk[1] + 4.2, 0.5, 0.85);
        return;
      }
      var mid = onArc(Xa(S), (R + RO) / 2, (aOf(si) + aOf(si + 1)) / 2);
      var hang = [head[0] + sway, mix(YG - 16, YCB - 12, sm(0, 1, p))];
      seg(head[0], head[1] + 5, hang[0], hang[1] - 8, 0.55, 0.95);
      seg(hang[0] - 3, hang[1] - 8, hang[0] + 3, hang[1] - 8, 0.6, 0.9); seg(hang[0] - 3, hang[1] - 8, hang[0] - 4, hang[1] - 2, 0.45, 0.8); seg(hang[0] + 3, hang[1] - 8, hang[0] + 4, hang[1] - 2, 0.45, 0.8);
      blit(sprites.stones[si], hang[0] - mid[0] + Xa(S), hang[1] - mid[1], 1);
    }
    function hit(x, y) {
      use();
      var ux = camX + x / K, uy = camY + y / K; if (uy < YPT - 24 || uy > YG + 6) return CHAT ? -1 : null;
      var i = Math.round(ux / B), a = null; st.arches.forEach(function (c) { if (c.i === i) a = c; });
      if (!a) return CHAT ? -1 : null;
      return CHAT ? a.i : a.h1 != null ? { from: a.h0, to: a.h1 } : null;
    }

    var onScreen = true, pageHidden = typeof document !== 'undefined' && !!document.hidden, cost = 0, costN = 0;
    var io = typeof IntersectionObserver !== 'undefined' ? new IntersectionObserver(function (es) { es.forEach(function (e) { onScreen = e.isIntersecting; }); kick(); }, { rootMargin: '80px' }) : null;
    if (io) io.observe(canvas);
    function onVis() { pageHidden = !!document.hidden; kick(); }
    document.addEventListener('visibilitychange', onVis);
    var asleep = false;
    function kick() { if (running && !opts.manual && onScreen && !pageHidden && !raf && !asleep) { lastT = null; raf = requestAnimationFrame(frame); } }
    function wake() { if (asleep) { asleep = false; calmSince = null; } kick(); }
    // Asleep: when nothing is under way (no stone moving or waiting, no water surging, the camera at rest) the last
    // frame stays on screen and no more are drawn until the page tells the picture something new. An idle node app
    // costs nothing, and nothing moves that did not happen.
    var calmSince = null;
    function settled(t) {
      if (!st.inited || st.catching) return false;
      if (st.surges.length || st.splashes.length || st.dust.length || st.glints.length || st.surgeAt.length || st.spawn.length) return false;
      if (Math.abs(st.camV || 0) > 0.3 || (st.craneX != null && Math.abs(st.craneX - (Xa(site()) + B * 1.05)) > 0.3)) return false;
      if (st.waterX != null && Math.abs(st.waterX - waterTarget(t)) > 0.3) return false;
      if (st.q.fed) { if (st.flow > 0.2) return false; } else if (!CHAT && st.clock - st.lastBlock < 20) return false;   // a stalled chain's water stills with it
      if (Math.abs(st.wheel - st.wheelTarget) > 0.002) return false;
      var c = cur();
      if (CHAT) { if (c && (c.failed == null || t - c.failed < 1)) return false; if (st.flow > 0.2) return false; }
      else if (c && st.top - c.next0 + 1 >= c.b) return false;
      for (var i = 0; i < st.arches.length; i++) {
        var a = st.arches[i];
        if (a.raised != null && a.raised > 0 && t - a.raised < 1.2) return false;
        if (a.struck != null && a.struck > 0 && t - a.struck < 1.6) return false;
        if (a.recT != null && a.recT > 0 && t - a.recT < 1.2) return false;
        if (a.set && a.anim[a.set - 1] != null && a.anim[a.set - 1] > 0 && t - a.anim[a.set - 1] < 0.5) return false;
      }
      return true;
    }
    function frame(ts) {
      raf = 0;
      if (!running || !onScreen || pageHidden) return;   // paused; kick() starts it again
      if (asleep) return;
      raf = requestAnimationFrame(frame);
      var t = ts / 1000, dt = lastT == null ? 0 : Math.min(0.1, t - lastT); lastT = t;
      var a0 = performance.now();
      resize(); st.clock = t; if (!st.inited) { if (ph.name) renderStart(t); return; }
      update(t, dt); render(t);
      if (settled(t)) { if (calmSince == null) calmSince = t; else if (t - calmSince > 1.5) { asleep = true; cancelAnimationFrame(raf); raf = 0; } } else calmSince = null;
      // a slow machine: after a few seconds of long frames, draw at a lower pixel density
      cost = cost * 0.94 + (performance.now() - a0) * 0.06;
      if (++costN > 120 && cost > 9 && dprCap > 1) { dprCap = dprCap > 1.5 ? 1.5 : 1; needSize = true; costN = 0; cost = 0; }
    }
    // the page's calls land between frames, and while paused: keep the clock current so rates and timings stay true
    function now() { if (!opts.manual) st.clock = performance.now() / 1000; wake(); }
    resize();
    if (CHAT) { load([]); st.camFresh = true; }
    kick();
    return {
      init: function (h, f, o) { now(); init(h, f, o); }, block: function (h, tx) { now(); block(h, tx); }, final: function (h) { now(); final(h); }, phase: function (n, p) { now(); phase(n, p); },
      queries: function (n) { now(); queries(n); }, hit: hit,
      load: function (list) { now(); load(list); st.camFresh = true; }, ask: function () { now(); return ask(); }, stream: function (n) { now(); stream(n); }, answered: function () { now(); answered(); },
      checked: function (ok, i) { now(); checked(ok, i); }, recorded: function (h, i) { now(); recorded(h, i); }, failed: function () { now(); failed(); },
      clock: function (t) { st.clock = t; },
      step: function (t, dt) { resize(); update(t, dt); },
      draw: function (t) { resize(); if (!st.inited) { if (ph.name) renderStart(t); return; } render(t); },
      resize: function () { needSize = true; resize(); },
      stats: function () {
        var c = cur(), q = st.q;
        return { top: st.top, final: st.fin, rate: st.rate, batch: c && c.b ? c.b : 1, txPerGlint: st.gb, catching: st.catching || st.phase === 'sync', since: st.clock - st.lastBlock,
          arches: st.arches.length, cutMs: st.cutMs, cam: camX, stonesSet: st.setCount || 0, dpr: DPR, queries: q.total, queryRate: q.rate, queriesPerSurge: q.qb, flow: st.flow,
          answers: CHAT ? st.arches.filter(function (a) { return a.set === NV; }).length : undefined };
      },
      destroy: function () { running = false; cancelAnimationFrame(raf); raf = 0; if (io) io.disconnect(); if (ro) ro.disconnect(); document.removeEventListener('visibilitychange', onVis); canvas.style.transform = ''; if (LAYERS) LAYERS.forEach(function (L) { if (L.url) URL.revokeObjectURL(L.url); if (L.inner.parentNode) L.inner.parentNode.removeChild(L.inner); }); }
    };
  }
  window.ArchChain = { mount: mount, NV: NV };
})();
