/* ARC aqueduct band: the chain drawn as an aqueduct under construction, engraved white on Arc blue, filling the page
   header behind the headline.

   The drawing is ./archchain.js on ./engine.js, byte-identical copies of desktop/src/lib/aqueduct/ (keep them in step
   with the desktop app). This file only decides what the drawing is told, and when.

   Every stone is a block this page read from the source it names; an arch is closed by its keystone and cut with the
   heights it holds; water runs over finished arches, and a glint in it is a transaction in a block the page read.
   The water along the top also runs with inference: every confirmed inference receipt the page reads that it had not
   read before, in a block above the ones it had already read, is one request, and each batch of them is a surge.
   Receipts already on the chain when the page started reading are never replayed as new work. Nothing else feeds it:
   the blocks and receipts of each page refresh and, while the band is on screen and the tab is visible, a light poll
   of the same source's latest block. It moves only when a new height arrives and holds still when blocks stop, the
   next stone hanging where it is. When the page withdraws its claim (source unreachable, maintenance interlock,
   checkpoint mismatch) the picture is cleared to an empty site instead of standing on old evidence. A change of
   source always starts a new picture: two sources are never drawn as one chain.

     var band = ArcAqueductBand.mount(root, { bot })   null when canvas or the drawing scripts are unavailable
     band.follow({ key, tone, title, detail, source, stalled, blocks, poll, inference })
     band.clear({ tone, title, detail, veil })
     band.destroy()

   blocks: [{ height, txCount, timestamp }]; poll: (signal) => Promise<blocks>; tone: good | warn | bad | neutral.
   inference: [{ id, height }], the confirmed inference receipts this read returned, or null when the read carries no
   inference evidence the page stands behind; then no water is fed and the line under the headline is withdrawn.
   The line counts the receipts that arrived in the last minute. It appears only once the page has read receipts
   without a gap for a whole minute, so it never reports a minute it did not see.
   The root holds its parts as [data-aq] elements: canvas; far and near, the sky and the ground, two layers that fill
   the canvas's box; stage and caption, shown once the drawing can run; veil, title, detail, tip, age, stone and
   queries. */
(function (root) {
  "use strict";

  const POLL_MS = 4_000;        // one latest-block request every 4 s, only while the band is seen
  const IDLE_MS = 12_000;       // no new block for this long: the picture holds still until one arrives
  const MAX_JUMP = 20_000;      // a wider gap than this starts a fresh picture at the new height
  const REGRESSION = 256;       // a tip this far below the drawn one is a source that went back: start again there
  const HISTORY = 13 * 17;      // heights below the tip needed to draw the finished arches behind it
  const SETTLE_MS = [0, 700, 1600];
  const MINUTE_MS = 60_000;     // the window the inference line reports
  const GAP_MS = 75_000;        // receipt reads further apart than this leave a hole: counting starts again

  const formatInteger = (value) => new Intl.NumberFormat().format(value);
  function formatAge(seconds) {
    const own = root.ArcNetwork && root.ArcNetwork.formatDuration;
    if (typeof own === "function") return own(seconds);
    if (seconds < 60) return `${seconds}s`;
    if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
    return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
  }
  const integer = (value) => (typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null);
  function stampMs(value) {
    const n = Number(value);
    if (value === null || value === undefined || !Number.isFinite(n) || n <= 0) return null;
    return n < 10_000_000_000 ? n * 1000 : n;
  }
  // the blocks a page read, by height, oldest first; an unknown transaction count draws no glints
  function rowsOf(list) {
    const byHeight = new Map();
    for (const row of Array.isArray(list) ? list : []) {
      const height = integer(row && row.height);
      if (height === null) continue;
      const tx = integer(row.txCount);
      byHeight.set(height, { height, txCount: tx === null ? 0 : Math.min(tx, 1_000_000), timestamp: stampMs(row.timestamp) });
    }
    return [...byHeight.values()].sort((a, b) => a.height - b.height);
  }
  // the inference receipts a page read: an identity and the height of the block that holds it, or nothing
  function receiptsOf(list) {
    if (!Array.isArray(list)) return null;
    const rows = [];
    for (const row of list) {
      const id = row && typeof row.id === "string" ? row.id : "";
      const height = integer(row && row.height);
      if (id && height !== null) rows.push({ id, height });
    }
    return rows;
  }
  const noInference = () => ({ live: false, since: 0, last: 0, floor: -1, seen: new Set(), batches: [] });

  function mount(figure, options) {
    const chain = root.ArchChain;
    if (!figure || !chain || typeof chain.mount !== "function" || !root.EG) return null;
    const settings = options || {};
    const part = (name) => figure.querySelector(`[data-aq="${name}"]`);
    const canvas = part("canvas");
    if (!canvas || typeof canvas.getContext !== "function" || !canvas.getContext("2d")) return null;
    const ui = {
      title: part("title"), detail: part("detail"), tip: part("tip"), age: part("age"), stone: part("stone"), veil: part("veil"),
      queries: part("queries"), stage: part("stage"), caption: part("caption"), far: part("far"), near: part("near"),
    };
    const motion = typeof root.matchMedia === "function" ? root.matchMedia("(prefers-reduced-motion: reduce)") : null;
    const now = () => performance.now();

    const s = {
      api: null, used: false, mode: "waiting", key: null,
      tone: "neutral", title: "Awaiting chain evidence", detail: "", veil: "Awaiting chain evidence", source: "",
      stalled: false, top: -1, tipAt: null, lastAdvance: 0,
      poll: null, pollTimer: 0, pollCtl: null,
      raf: 0, lastT: null, settle: [], ticker: 0, resizing: 0,
      onScreen: false, reduced: !!(motion && motion.matches), destroyed: false,
      inf: noInference(),
    };
    s.detail = ui.detail ? ui.detail.textContent : "";

    // ---- the drawing instance, created when the band first comes on screen
    function build() {
      if (s.api) s.api.destroy();
      // the sky and the ground slide as two layers behind the canvas; the canvas holds only the aqueduct
      const layers = ui.far && ui.near ? { far: ui.far, near: ui.near } : {};
      s.api = chain.mount(canvas, { reduced: s.reduced, manual: true, ...layers, ...(Number.isFinite(settings.bot) ? { bot: settings.bot } : {}) });
      s.used = false;
      if (s.mode !== "following") s.api.phase("survey");
    }
    // a fresh picture standing at the tip: the finished arches behind it, the current arch part-built
    function start() {
      if (s.used || !s.api) build();
      s.api.clock(now() / 1000);
      s.api.init(s.top, s.top, { history: s.top >= HISTORY });
      s.used = true;
      // the water runs only with requests read from here on: the feed is live, and still until one arrives
      if (s.inf.live) s.api.queries(0);
    }

    // ---- frames
    function paint(dt) {
      if (!s.api) return;
      const t = now() / 1000;
      s.api.step(t, dt);
      s.api.draw(t);
    }
    function running() {
      return !!s.api && s.used && s.mode === "following" && !s.stalled && !s.reduced && s.onScreen
        && !document.hidden && !s.resizing && now() - s.lastAdvance < IDLE_MS;
    }
    function loop(ts) {
      s.raf = 0;
      if (s.destroyed || !s.api) return;
      const t = ts / 1000, dt = s.lastT == null ? 1 / 60 : Math.min(0.1, Math.max(0, t - s.lastT));
      s.lastT = t;
      s.api.step(t, dt);
      s.api.draw(t);
      if (running()) s.raf = requestAnimationFrame(loop);
      else { s.lastT = null; caption(); }
    }
    function stop() {
      if (s.raf) cancelAnimationFrame(s.raf);
      s.raf = 0;
      s.lastT = null;
    }
    // a still picture: a few frames with the easing run to rest, and nothing between them
    function settle() {
      s.settle.forEach(clearTimeout);
      s.settle = SETTLE_MS.map((ms) => setTimeout(() => { if (!s.raf && s.api && !s.resizing) paint(10); }, ms));
    }
    function kick() {
      if (s.destroyed || !s.api) return;
      if (running()) { if (!s.raf) { s.lastT = null; s.raf = requestAnimationFrame(loop); } }
      else if (!s.raf) settle();
    }

    // ---- the feed: only heights the page read, each new one a stone
    function feed(list) {
      const rows = rowsOf(list);
      if (!rows.length) return false;
      const newest = rows[rows.length - 1];
      if (s.top >= 0 && newest.height <= s.top && newest.height >= s.top - REGRESSION) {
        if (newest.height === s.top && newest.timestamp != null) s.tipAt = newest.timestamp;
        return false;   // nothing new (or a refresh that began before the last poll landed)
      }
      const fresh = s.top < 0 || newest.height < s.top || newest.height - s.top > MAX_JUMP;
      const previous = s.top;
      s.top = newest.height;
      s.tipAt = newest.timestamp;
      s.lastAdvance = now();
      if (fresh && previous >= 0) s.inf = noInference();   // a source that went back or leapt: count from here again
      if (!s.api) return true;                  // drawn when the band comes on screen
      if (fresh || !s.used) { start(); return true; }
      s.api.clock(now() / 1000);
      for (const row of rows) if (row.height > previous) s.api.block(row.height, row.txCount);
      // ARC commits a block two rounds after it is proposed; a block a source serves is already final
      s.api.final(s.top);
      return true;
    }

    // ---- inference: receipts the page read, each new one a request in the water
    function readReceipts(list) {
      const rows = receiptsOf(list);
      const t = Date.now();
      if (rows === null) { s.inf = noInference(); return; }          // no evidence this read: the line is withdrawn
      const top = rows.reduce((max, row) => Math.max(max, row.height), -1);
      if (!s.inf.live || t - s.inf.last > GAP_MS) {
        // the first read, or the first after a hole: what it holds, and anything at or below the block the page has
        // read, happened before the page was watching
        s.inf = { live: true, since: t, last: t, floor: Math.max(top, s.top), seen: new Set(rows.map((row) => row.id)), batches: [] };
        if (s.api && s.used) s.api.queries(0);
        return;
      }
      const fresh = rows.filter((row) => !s.inf.seen.has(row.id) && row.height > s.inf.floor);
      s.inf.seen = new Set(rows.map((row) => row.id));
      s.inf.floor = Math.max(s.inf.floor, top);
      s.inf.last = t;
      s.inf.batches = s.inf.batches.filter(([at]) => t - at <= MINUTE_MS);
      if (!fresh.length) return;
      s.inf.batches.push([t, fresh.length]);
      if (s.api && s.used) s.api.queries(fresh.length);
    }
    function inferenceLine() {
      const t = Date.now();
      if (s.mode !== "following" || !s.inf.live || t - s.inf.since < MINUTE_MS || t - s.inf.last > GAP_MS) return "";
      const count = s.inf.batches.reduce((sum, [at, n]) => (t - at <= MINUTE_MS ? sum + n : sum), 0);
      return `Inference on the network · ${formatInteger(count)} ${count === 1 ? "query" : "queries"} in the last minute`;
    }

    // ---- the light poll of the same source's latest block
    function stopPoll() {
      clearTimeout(s.pollTimer);
      s.pollTimer = 0;
      if (s.pollCtl) s.pollCtl.abort();
      s.pollCtl = null;
    }
    function polling() {
      return !!s.poll && s.mode === "following" && !s.stalled && s.onScreen && !document.hidden;
    }
    function schedulePoll(delay) {
      clearTimeout(s.pollTimer);
      s.pollTimer = 0;
      if (!polling() || s.pollCtl) return;
      s.pollTimer = setTimeout(pollOnce, delay == null ? POLL_MS : delay);
    }
    async function pollOnce() {
      s.pollTimer = 0;
      if (!polling()) return;
      const key = s.key, poll = s.poll, ctl = new AbortController();
      s.pollCtl = ctl;
      let rows = null;
      try { rows = await poll(ctl.signal); } catch (_error) { rows = null; }   // no evidence this time: nothing moves
      if (s.pollCtl === ctl) s.pollCtl = null;
      if (ctl.signal.aborted || s.destroyed || key !== s.key || s.mode !== "following") return;
      if (rows && feed(rows)) { caption(); kick(); }
      schedulePoll();
    }

    // ---- words
    function put(node, value) { if (node && node.textContent !== value) node.textContent = value; }
    function idle() { return s.mode === "following" && !s.stalled && s.top >= 0 && now() - s.lastAdvance >= IDLE_MS; }
    const sentence = (text) => String(text || "").replace(/[.\s]+$/, "");
    function label(waiting) {
      if (s.mode !== "following" || s.top < 0) return `An empty building site where the aqueduct will stand; no blocks are drawn. ${sentence(s.title)}.`;
      const where = s.source ? ` from ${s.source}` : "";
      const why = s.stalled ? " Building has stopped because the chain appears stalled." : waiting ? " Building has stopped until a new block arrives." : "";
      return `The chain drawn as an aqueduct under construction, at block ${formatInteger(s.top)}${where}. Each stone is a block the page read; each finished arch is cut with the heights it holds.${why}`;
    }
    function caption() {
      if (s.destroyed) return;
      const waiting = idle();
      put(ui.title, waiting ? "Waiting for the next block" : s.title);
      put(ui.detail, waiting
        ? `No new block has arrived from ${s.source || "the source"} since #${formatInteger(s.top)}. Nothing is built until one does.`
        : s.detail);
      put(ui.tip, s.mode === "following" && s.top >= 0 ? `#${formatInteger(s.top)}` : "—");
      put(ui.age, s.mode === "following" && s.tipAt != null ? formatAge(Math.max(0, Math.round((Date.now() - s.tipAt) / 1000))) : "—");
      const batch = s.api && s.used && s.mode === "following" ? s.api.stats().batch : null;
      put(ui.stone, batch == null ? "—" : batch === 1 ? "1 block" : `${formatInteger(batch)} blocks`);
      put(ui.queries, inferenceLine());
      figure.dataset.tone = waiting ? "neutral" : s.tone;
      figure.dataset.state = s.mode !== "following" ? s.mode : s.stalled ? "stalled" : waiting ? "idle" : "live";
      const aria = label(waiting);
      if (canvas.getAttribute("aria-label") !== aria) canvas.setAttribute("aria-label", aria);
      if (ui.veil) {
        const show = s.mode !== "following" && !!s.veil;
        if (ui.veil.hidden === show) ui.veil.hidden = !show;
        put(ui.veil, s.veil);
      }
    }
    function tick() {
      clearInterval(s.ticker);
      s.ticker = s.onScreen && !document.hidden ? setInterval(() => { caption(); if (!running() && s.raf) stop(); }, 1000) : 0;
    }

    // ---- what the page says
    function follow(o) {
      if (s.destroyed) return;
      const opts = o || {};
      const key = String(opts.key);
      if (s.mode !== "following" || key !== s.key) {
        stopPoll();
        stop();
        s.mode = "following";
        s.key = key;
        s.top = -1;
        s.tipAt = null;
        s.lastAdvance = 0;
        s.inf = noInference();
        if (s.used) build();   // a new source is a new picture: nothing carries over
      }
      s.tone = opts.tone || "good";
      s.title = opts.title || "";
      s.detail = opts.detail || "";
      s.source = opts.source || "";
      s.veil = "";
      s.stalled = !!opts.stalled;
      s.poll = typeof opts.poll === "function" ? opts.poll : null;
      if (s.stalled) stopPoll();
      feed(opts.blocks);
      readReceipts(opts.inference);
      caption();
      kick();
      schedulePoll();
    }
    function clear(o) {
      if (s.destroyed) return;
      const opts = o || {};
      stopPoll();
      stop();
      s.mode = opts.tone === "neutral" ? "waiting" : "cleared";
      s.key = null;
      s.top = -1;
      s.tipAt = null;
      s.stalled = false;
      s.poll = null;
      s.inf = noInference();
      s.tone = opts.tone || "bad";
      s.title = opts.title || "";
      s.detail = opts.detail || "";
      s.veil = opts.veil || opts.title || "";
      if (s.used) build();     // the old picture is withdrawn; the fresh one shows only the surveyor's stakes
      else if (s.api) s.api.phase("survey");
      caption();
      kick();
    }

    // ---- when to work: on screen, visible, at the size it is shown
    function onScreen(on) {
      if (on === s.onScreen) return;
      s.onScreen = on;
      if (on) {
        if (!s.api) { build(); if (s.mode === "following" && s.top >= 0) start(); }
        caption();
        kick();
        schedulePoll(0);
      } else {
        stop();
        stopPoll();
      }
      tick();
    }
    const io = typeof IntersectionObserver === "function"
      ? new IntersectionObserver((entries) => onScreen(entries.some((entry) => entry.isIntersecting)), { rootMargin: "160px 0px" })
      : null;
    const ro = typeof ResizeObserver === "function"
      ? new ResizeObserver(() => {
        if (!s.api) return;
        clearTimeout(s.resizing);
        stop();
        s.resizing = setTimeout(() => { s.resizing = 0; kick(); }, 160);
      })
      : null;
    function onVisibility() {
      if (document.hidden) { stop(); stopPoll(); }
      else { caption(); kick(); schedulePoll(0); }
      tick();
    }
    function onMotion() {
      s.reduced = !!(motion && motion.matches);
      if (!s.api) return;
      stop();
      build();
      if (s.mode === "following" && s.top >= 0) start();
      kick();
    }
    document.addEventListener("visibilitychange", onVisibility);
    if (motion && typeof motion.addEventListener === "function") motion.addEventListener("change", onMotion);
    if (document.fonts && typeof document.fonts.load === "function") {
      // the tablets are cut in Hanken Grotesk; redraw a still picture once the face is in
      document.fonts.load('500 16px "Hanken Grotesk"').then(() => { if (!s.raf) kick(); }, () => {});
    }

    // the drawing and its caption stay hidden until the drawing can run: a page without it says nothing about it
    const shown = ui.stage || figure;
    shown.hidden = false;
    if (ui.caption) ui.caption.hidden = false;
    caption();
    if (io) io.observe(shown);
    else onScreen(true);
    if (ro) ro.observe(canvas);

    function destroy() {
      if (s.destroyed) return;
      s.destroyed = true;
      stop();
      stopPoll();
      s.settle.forEach(clearTimeout);
      clearInterval(s.ticker);
      clearTimeout(s.resizing);
      if (io) io.disconnect();
      if (ro) ro.disconnect();
      document.removeEventListener("visibilitychange", onVisibility);
      if (motion && typeof motion.removeEventListener === "function") motion.removeEventListener("change", onMotion);
      if (s.api) s.api.destroy();
      s.api = null;
    }

    return Object.freeze({ follow, clear, destroy });
  }

  root.ArcAqueductBand = Object.freeze({ mount });
})(typeof window !== "undefined" ? window : globalThis);
