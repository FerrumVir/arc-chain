(function (root) {
  "use strict";

  const POLL_MS = 4_000;
  const IDLE_MS = 12_000;
  const MAX_JUMP = 20_000;
  const REGRESSION = 256;
  const HISTORY = 13 * 17;
  const SETTLE_MS = [0, 700, 1600];
  const MINUTE_MS = 60_000;
  const GAP_MS = 75_000;

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

    function build() {
      if (s.api) s.api.destroy();
      const layers = ui.far && ui.near ? { far: ui.far, near: ui.near } : {};
      s.api = chain.mount(canvas, { reduced: s.reduced, manual: true, ...layers, ...(Number.isFinite(settings.bot) ? { bot: settings.bot } : {}) });
      s.used = false;
      if (s.mode !== "following") s.api.phase("survey");
    }
    function start() {
      if (s.used || !s.api) build();
      s.api.clock(now() / 1000);
      s.api.init(s.top, s.top, { history: s.top >= HISTORY });
      s.used = true;
      if (s.inf.live) s.api.queries(0);
    }

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
    function settle() {
      s.settle.forEach(clearTimeout);
      s.settle = SETTLE_MS.map((ms) => setTimeout(() => { if (!s.raf && s.api && !s.resizing) paint(10); }, ms));
    }
    function kick() {
      if (s.destroyed || !s.api) return;
      if (running()) { if (!s.raf) { s.lastT = null; s.raf = requestAnimationFrame(loop); } }
      else if (!s.raf) settle();
    }

    function feed(list) {
      const rows = rowsOf(list);
      if (!rows.length) return false;
      const newest = rows[rows.length - 1];
      if (s.top >= 0 && newest.height <= s.top && newest.height >= s.top - REGRESSION) {
        if (newest.height === s.top && newest.timestamp != null) s.tipAt = newest.timestamp;
        return false;
      }
      const fresh = s.top < 0 || newest.height < s.top || newest.height - s.top > MAX_JUMP;
      const previous = s.top;
      s.top = newest.height;
      s.tipAt = newest.timestamp;
      s.lastAdvance = now();
      if (fresh && previous >= 0) s.inf = noInference();
      if (!s.api) return true;
      if (fresh || !s.used) { start(); return true; }
      s.api.clock(now() / 1000);
      for (const row of rows) if (row.height > previous) s.api.block(row.height, row.txCount);
      s.api.final(s.top);
      return true;
    }

    function readReceipts(list) {
      const rows = receiptsOf(list);
      const t = Date.now();
      if (rows === null) { s.inf = noInference(); return; }
      const top = rows.reduce((max, row) => Math.max(max, row.height), -1);
      if (!s.inf.live || t - s.inf.last > GAP_MS) {
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
      try { rows = await poll(ctl.signal); } catch (_error) { rows = null; }
      if (s.pollCtl === ctl) s.pollCtl = null;
      if (ctl.signal.aborted || s.destroyed || key !== s.key || s.mode !== "following") return;
      if (rows && feed(rows)) { caption(); kick(); }
      schedulePoll();
    }

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
        if (s.used) build();
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
      if (s.used) build();
      else if (s.api) s.api.phase("survey");
      caption();
      kick();
    }

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
      document.fonts.load('500 16px "Hanken Grotesk"').then(() => { if (!s.raf) kick(); }, () => {});
    }

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
