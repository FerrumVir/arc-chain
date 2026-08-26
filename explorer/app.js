(function arcExplorerModule(factory) {
  "use strict";

  const api = factory();
  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  }
  if (typeof window !== "undefined" && typeof document !== "undefined") {
    window.ArcExplorer = api;
    window.addEventListener("DOMContentLoaded", api.boot, { once: true });
  }
})(function arcExplorerFactory() {
  "use strict";

  const SOURCES = Object.freeze([
    Object.freeze({ id: "nyc", name: "NYC", region: "US East", baseUrl: "http://149.28.32.76:9090" }),
    Object.freeze({ id: "lax", name: "LAX", region: "US West", baseUrl: "http://140.82.16.112:9090" }),
    Object.freeze({ id: "ams", name: "AMS", region: "Europe", baseUrl: "http://136.244.109.1:9090" }),
    Object.freeze({ id: "lhr", name: "LHR", region: "Europe", baseUrl: "http://104.238.171.11:9090" }),
    Object.freeze({ id: "nrt", name: "NRT", region: "Asia", baseUrl: "http://202.182.107.41:9090" }),
    Object.freeze({ id: "sgp", name: "SGP", region: "Asia", baseUrl: "http://149.28.153.31:9090" }),
  ]);

  const SOURCE_BY_ID = new Map(SOURCES.map((source) => [source.id, source]));
  const HASH_PATTERN = /^[0-9a-f]{64}$/i;
  const HEIGHT_PATTERN = /^\d+$/;
  const LIVENESS_FRESH_SECS = 30 * 60;
  const REQUEST_TIMEOUT_MS = 7000;
  const MAX_RESPONSE_CHARS = 5_000_000;
  const REFRESH_INTERVAL_MS = 20_000;

  class RpcError extends Error {
    constructor(message, status, details) {
      super(message);
      this.name = "RpcError";
      this.status = status || 0;
      this.details = details || null;
      this.aborted = Boolean(details && details.aborted);
    }
  }

  function sourceFor(sourceId) {
    return SOURCE_BY_ID.get(String(sourceId || "").toLowerCase()) || null;
  }

  function buildRpcUrl(sourceId, path) {
    const source = sourceFor(sourceId);
    if (!source) {
      throw new Error(`Unknown ARC RPC source: ${sourceId}`);
    }
    if (typeof path !== "string" || !path.startsWith("/") || path.startsWith("//") || path.includes("://")) {
      throw new Error("RPC path must be a source-relative absolute path");
    }
    return `${source.baseUrl}${path}`;
  }

  function normalizeHex(value) {
    const normalized = String(value || "").trim().replace(/^0x/i, "").toLowerCase();
    return HASH_PATTERN.test(normalized) ? normalized : null;
  }

  function classifyLookup(rawValue, requestedKind) {
    const value = String(rawValue || "").trim();
    const kind = requestedKind || "auto";

    if (!value) {
      return { error: "Enter a block height, transaction hash, or address." };
    }

    if (kind === "block" || (kind === "auto" && HEIGHT_PATTERN.test(value))) {
      if (!HEIGHT_PATTERN.test(value)) {
        return { error: "Block heights contain digits only." };
      }
      const height = Number(value);
      if (!Number.isSafeInteger(height) || height < 0) {
        return { error: "That block height is outside the safe lookup range." };
      }
      return { kind: "block", value: String(height) };
    }

    const hash = normalizeHex(value);
    if (!hash) {
      return { error: "Hashes and ARC addresses must be exactly 32 bytes (64 hex characters)." };
    }

    if (kind === "tx") return { kind: "tx", value: hash };
    if (kind === "address") return { kind: "address", value: hash };
    return { kind: "lookup", value: hash };
  }

  function finiteNonNegative(value) {
    const number = typeof value === "number" ? value : Number(value);
    return Number.isFinite(number) && number >= 0 ? number : null;
  }

  function reportedHeightFrom(payloads) {
    const candidates = [
      payloads.network && payloads.network.height,
      payloads.info && payloads.info.block_height,
      payloads.stats && payloads.stats.block_height,
      payloads.health && payloads.health.height,
    ]
      .map(finiteNonNegative)
      .filter((value) => value !== null);
    return candidates.length ? Math.max(...candidates) : null;
  }

  function evaluateLiveness(health, latestBlock, nowMs) {
    const currentTime = finiteNonNegative(nowMs) === null ? Date.now() : Number(nowMs);
    const explicitAge = health ? finiteNonNegative(health.last_block_age_secs) : null;

    if (health && typeof health.chain_advancing === "boolean" && explicitAge !== null) {
      return {
        state: health.chain_advancing ? "advancing" : "stalled",
        ageSecs: Math.floor(explicitAge),
        basis: "selected node /health block-liveness fields",
      };
    }

    const timestamp = latestBlock && latestBlock.header
      ? finiteNonNegative(latestBlock.header.timestamp)
      : null;
    if (timestamp === null || timestamp === 0) {
      return { state: "unknown", ageSecs: null, basis: "no readable retained block timestamp" };
    }
    if (timestamp > currentTime + 60_000) {
      return { state: "unknown", ageSecs: null, basis: "retained block timestamp is ahead of this browser clock" };
    }

    const ageSecs = Math.max(0, Math.floor((currentTime - timestamp) / 1000));
    return {
      state: ageSecs <= LIVENESS_FRESH_SECS ? "advancing" : "stalled",
      ageSecs,
      basis: `selected source block timestamp (${LIVENESS_FRESH_SECS}s freshness window)`,
    };
  }

  function sortBlocksNewestFirst(blocks) {
    if (!Array.isArray(blocks)) return [];
    return blocks
      .filter((block) => block && finiteNonNegative(block.height ?? (block.header && block.header.height)) !== null)
      .slice()
      .sort((left, right) => {
        const leftHeight = finiteNonNegative(left.height ?? (left.header && left.header.height));
        const rightHeight = finiteNonNegative(right.height ?? (right.header && right.header.height));
        return rightHeight - leftHeight;
      });
  }

  function boundedItems(items, limit) {
    const sourceItems = Array.isArray(items) ? items : [];
    const requestedLimit = Number.isSafeInteger(limit) && limit >= 0 ? limit : 0;
    return {
      visible: sourceItems.slice(0, requestedLimit),
      total: sourceItems.length,
      truncated: sourceItems.length > requestedLimit,
    };
  }

  function formatInteger(value) {
    if (value === null || value === undefined || value === "") return "Unknown";
    if (typeof value === "number" && Number.isFinite(value)) return Math.trunc(value).toLocaleString("en-US");
    const text = String(value);
    if (/^\d+$/.test(text)) {
      try {
        return BigInt(text).toLocaleString("en-US");
      } catch (_) {
        return text;
      }
    }
    return text;
  }

  function formatDuration(seconds) {
    const value = finiteNonNegative(seconds);
    if (value === null) return "Unknown";
    const whole = Math.floor(value);
    if (whole < 5) return "just now";
    if (whole < 60) return `${whole}s`;
    if (whole < 3600) return `${Math.floor(whole / 60)}m ${whole % 60}s`;
    if (whole < 86400) return `${Math.floor(whole / 3600)}h ${Math.floor((whole % 3600) / 60)}m`;
    return `${Math.floor(whole / 86400)}d ${Math.floor((whole % 86400) / 3600)}h`;
  }

  function formatHash(value, leading, trailing) {
    const text = String(value || "").replace(/^0x/i, "");
    if (!text) return "Unknown";
    const start = leading === undefined ? 10 : leading;
    const end = trailing === undefined ? 8 : trailing;
    return text.length > start + end + 1 ? `0x${text.slice(0, start)}…${text.slice(-end)}` : `0x${text}`;
  }

  function formatTimestamp(timestampMs) {
    const value = finiteNonNegative(timestampMs);
    if (value === null || value === 0) return "Unknown";
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? "Unknown" : date.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "medium" });
  }

  function errorMessage(error, resource) {
    const noun = resource || "resource";
    if (error && error.status === 404) {
      return `The selected node does not have this ${noun} in its retained or indexed history.`;
    }
    if (error && error.status === 400) {
      return `The selected node rejected this ${noun} lookup as invalid.`;
    }
    if (error && error.aborted) return "Lookup canceled.";
    return error && error.message ? error.message : `Unable to load this ${noun} from the selected node.`;
  }

  async function requestJson(sourceId, path, options) {
    const opts = options || {};
    const controller = new AbortController();
    const externalSignal = opts.signal;
    const abortFromExternal = () => controller.abort();
    if (externalSignal) {
      if (externalSignal.aborted) controller.abort();
      else externalSignal.addEventListener("abort", abortFromExternal, { once: true });
    }
    const timeout = setTimeout(() => controller.abort(), opts.timeoutMs || REQUEST_TIMEOUT_MS);

    try {
      const response = await fetch(buildRpcUrl(sourceId, path), {
        method: "GET",
        headers: { Accept: "application/json" },
        cache: "no-store",
        signal: controller.signal,
      });
      const text = await response.text();
      if (text.length > MAX_RESPONSE_CHARS) {
        throw new RpcError("RPC response exceeded the explorer safety limit.", response.status);
      }
      let payload = null;
      if (text) {
        try {
          payload = JSON.parse(text);
        } catch (_) {
          throw new RpcError("RPC returned a non-JSON response.", response.status, { responseText: text.slice(0, 160) });
        }
      }
      if (!response.ok) {
        const detail = payload && (payload.error || payload.message);
        throw new RpcError(detail || `RPC returned HTTP ${response.status}.`, response.status, payload);
      }
      return payload;
    } catch (error) {
      if (error instanceof RpcError) throw error;
      if (controller.signal.aborted) {
        if (externalSignal && externalSignal.aborted) {
          throw new RpcError("Request canceled.", 0, { aborted: true });
        }
        throw new RpcError(`RPC request timed out after ${opts.timeoutMs || REQUEST_TIMEOUT_MS}ms.`, 0);
      }
      throw new RpcError(error && error.message ? error.message : "RPC request failed.", 0);
    } finally {
      clearTimeout(timeout);
      if (externalSignal) externalSignal.removeEventListener("abort", abortFromExternal);
    }
  }

  function boot() {
    const byId = (id) => document.getElementById(id);
    const elements = {
      sourceSelect: byId("source-select"),
      sourceDot: byId("source-dot"),
      refreshButton: byId("refresh-button"),
      sourceName: byId("source-name"),
      sourceEndpoint: byId("source-endpoint"),
      lastRefreshed: byId("last-refreshed"),
      banner: byId("connection-banner"),
      bannerTitle: byId("banner-title"),
      bannerDetail: byId("banner-detail"),
      metricHeight: byId("metric-height"),
      metricHeightNote: byId("metric-height-note"),
      metricStoredHeight: byId("metric-stored-height"),
      metricStoredNote: byId("metric-stored-note"),
      metricBlockAge: byId("metric-block-age"),
      metricLivenessNote: byId("metric-liveness-note"),
      metricTransactions: byId("metric-transactions"),
      metricPeers: byId("metric-peers"),
      metricValidators: byId("metric-validators"),
      metricValidatorNote: byId("metric-validator-note"),
      blocksStatus: byId("blocks-status"),
      blocksBody: byId("blocks-body"),
      sourceFacts: byId("source-facts"),
      searchForm: byId("search-form"),
      searchInput: byId("search-input"),
      searchKind: byId("search-kind"),
      searchError: byId("search-error"),
      inspector: byId("inspector"),
      inspectorKicker: byId("inspector-kicker"),
      inspectorTitle: byId("inspector-title"),
      inspectorClose: byId("inspector-close"),
      inspectorContent: byId("inspector-content"),
    };

    const state = {
      sourceId: "nyc",
      refreshController: null,
      lookupController: null,
      refreshGeneration: 0,
      lookupGeneration: 0,
      timer: null,
    };

    function text(element, value) {
      if (element) element.textContent = value === null || value === undefined ? "—" : String(value);
    }

    function create(tagName, className, value) {
      const element = document.createElement(tagName);
      if (className) element.className = className;
      if (value !== undefined && value !== null) element.textContent = String(value);
      return element;
    }

    function clear(element) {
      if (element) element.replaceChildren();
    }

    function selectedSource() {
      return sourceFor(state.sourceId) || SOURCES[0];
    }

    function savedSourceId() {
      const params = new URLSearchParams(window.location.search);
      const querySource = params.get("source");
      if (sourceFor(querySource)) return querySource.toLowerCase();
      try {
        const stored = window.localStorage.getItem("arc-explorer-source");
        if (sourceFor(stored)) return stored.toLowerCase();
      } catch (_) {
        // Storage can be disabled. The default remains deterministic.
      }
      return SOURCES[0].id;
    }

    function persistSourceId(sourceId) {
      try {
        window.localStorage.setItem("arc-explorer-source", sourceId);
      } catch (_) {
        // The URL still records the source when storage is unavailable.
      }
      const url = new URL(window.location.href);
      url.searchParams.set("source", sourceId);
      window.history.replaceState(null, "", `${url.pathname}${url.search}${url.hash}`);
    }

    function populateSources() {
      clear(elements.sourceSelect);
      for (const source of SOURCES) {
        const option = create("option", "", `${source.name} · ${source.region}`);
        option.value = source.id;
        elements.sourceSelect.append(option);
      }
    }

    function updateSourceChrome() {
      const source = selectedSource();
      elements.sourceSelect.value = source.id;
      text(elements.sourceName, `${source.name} · ${source.region}`);
      text(elements.sourceEndpoint, source.baseUrl);
    }

    function setSourceDot(mode) {
      elements.sourceDot.className = `status-dot ${mode || "unknown"}`;
    }

    function setBanner(mode, title, detail) {
      elements.banner.className = `connection-banner ${mode}`;
      text(elements.bannerTitle, title);
      text(elements.bannerDetail, detail);
    }

    function setRefreshBusy(busy) {
      elements.refreshButton.classList.toggle("spinning", busy);
      elements.refreshButton.disabled = busy;
    }

    function setMetricPlaceholders() {
      text(elements.metricHeight, "—");
      text(elements.metricHeightNote, "Waiting for selected node");
      text(elements.metricStoredHeight, "—");
      text(elements.metricStoredNote, "Waiting for block header");
      text(elements.metricBlockAge, "—");
      text(elements.metricLivenessNote, "Liveness unknown");
      text(elements.metricTransactions, "—");
      text(elements.metricPeers, "—");
      text(elements.metricValidators, "—");
      text(elements.metricValidatorNote, "Active split may be unavailable");
    }

    function fulfilled(result) {
      return result && result.status === "fulfilled" ? result.value : null;
    }

    function renderFacts(facts) {
      clear(elements.sourceFacts);
      for (const [label, value, title] of facts) {
        const row = create("div");
        const term = create("dt", "", label);
        const detail = create("dd", "", value === null || value === undefined ? "Unknown" : value);
        if (title) detail.title = title;
        row.append(term, detail);
        elements.sourceFacts.append(row);
      }
    }

    function blockHeight(block) {
      return finiteNonNegative(block && (block.height ?? (block.header && block.header.height)));
    }

    function blockHash(block) {
      return block && (block.hash || block.block_hash);
    }

    function blockHeader(block) {
      return (block && block.header) || block || {};
    }

    function renderBlocks(blocks) {
      const sorted = sortBlocksNewestFirst(blocks).slice(0, 12);
      clear(elements.blocksBody);
      if (!sorted.length) {
        const row = create("tr");
        const cell = create("td", "empty-cell", "No retained blocks were returned by this source for the requested range.");
        cell.colSpan = 5;
        row.append(cell);
        elements.blocksBody.append(row);
        text(elements.blocksStatus, "No retained data");
        return;
      }

      for (const block of sorted) {
        const header = blockHeader(block);
        const height = blockHeight(block);
        const row = create("tr", "data-row");

        const heightCell = create("td");
        const heightButton = create("button", "table-link height-link", `#${formatInteger(height)}`);
        heightButton.type = "button";
        heightButton.addEventListener("click", () => navigate("block", String(height)));
        heightCell.append(heightButton);

        const timestamp = finiteNonNegative(header.timestamp);
        const age = timestamp && timestamp <= Date.now() + 60_000 ? Math.max(0, Math.floor((Date.now() - timestamp) / 1000)) : null;
        const ageCell = create("td", "", formatDuration(age));
        if (timestamp) ageCell.title = formatTimestamp(timestamp);

        const txCell = create("td", "", formatInteger(header.tx_count ?? (block.tx_hashes && block.tx_hashes.length)));
        const producerCell = create("td", "", formatHash(header.producer, 6, 5));
        producerCell.title = String(header.producer || "Unknown");

        const hashCell = create("td");
        const hashButton = create("button", "table-link", formatHash(blockHash(block)));
        hashButton.type = "button";
        hashButton.title = String(blockHash(block) || "Unknown");
        hashButton.addEventListener("click", () => navigate("block", String(height)));
        hashCell.append(hashButton);

        row.append(heightCell, ageCell, txCell, producerCell, hashCell);
        elements.blocksBody.append(row);
      }
      text(elements.blocksStatus, `${sorted.length} retained`);
    }

    function factsFrom(payloads, latest) {
      const header = blockHeader(latest);
      const version = (payloads.network && payloads.network.node_version)
        || (payloads.info && payloads.info.version)
        || (payloads.health && payloads.health.version)
        || (payloads.stats && payloads.stats.version);
      const chain = (payloads.network && payloads.network.network)
        || (payloads.info && payloads.info.chain)
        || (payloads.stats && payloads.stats.chain);
      const healthStatus = payloads.health && payloads.health.status;
      const mempool = (payloads.info && payloads.info.mempool_size) ?? (payloads.stats && payloads.stats.mempool_size);
      return [
        ["Node version", version || "Unknown"],
        ["Chain", chain || "Undeclared / unknown"],
        ["Health response", healthStatus || "Endpoint unavailable"],
        ["Mempool", mempool === undefined ? "Unknown" : formatInteger(mempool)],
        ["Latest block hash", formatHash(blockHash(latest)), blockHash(latest)],
        ["Latest state root", formatHash(header.state_root), header.state_root],
      ];
    }

    async function refresh(options) {
      const opts = options || {};
      const source = selectedSource();
      const generation = ++state.refreshGeneration;
      if (state.refreshController) state.refreshController.abort();
      state.refreshController = new AbortController();
      const signal = state.refreshController.signal;

      setRefreshBusy(true);
      setSourceDot("unknown");
      setBanner("loading", `Connecting to ${source.name}…`, `All panels are pinned to ${source.baseUrl}.`);
      text(elements.blocksStatus, "Loading");
      if (!opts.keepMetrics) setMetricPlaceholders();

      const paths = ["/health", "/info", "/stats", "/validators", "/network/info", "/block/latest"];
      const results = await Promise.allSettled(paths.map((path) => requestJson(source.id, path, { signal })));
      if (generation !== state.refreshGeneration || source.id !== state.sourceId) return;

      const payloads = {
        health: fulfilled(results[0]),
        info: fulfilled(results[1]),
        stats: fulfilled(results[2]),
        validators: fulfilled(results[3]),
        network: fulfilled(results[4]),
      };
      let latest = fulfilled(results[5]);
      const reachable = results.some((result) => result.status === "fulfilled");

      if (!reachable) {
        setRefreshBusy(false);
        setSourceDot("offline");
        const mixedContent = window.location.protocol === "https:" && source.baseUrl.startsWith("http:");
        const detail = mixedContent
          ? "These seed RPCs currently expose HTTP only; an HTTPS page cannot read them without an HTTPS reverse proxy."
          : `No read endpoint answered at ${source.baseUrl}. Try another source or check network access.`;
        setBanner("error", `${source.name} is unreachable`, detail);
        renderBlocks([]);
        renderFacts(factsFrom(payloads, null));
        return;
      }

      const reportedHeight = reportedHeightFrom(payloads);
      let blocks = [];
      if (reportedHeight !== null) {
        const from = Math.max(0, Math.floor(reportedHeight) - 15);
        try {
          const range = await requestJson(source.id, `/blocks?from=${from}&to=${Math.floor(reportedHeight)}&limit=16`, { signal });
          blocks = range && Array.isArray(range.blocks) ? range.blocks : [];
        } catch (error) {
          if (error.aborted) return;
        }
      }
      if (!latest && blocks.length) {
        latest = sortBlocksNewestFirst(blocks)[0];
      }
      if (latest && !blocks.some((block) => blockHeight(block) === blockHeight(latest))) {
        blocks.push(latest);
      }

      const liveness = evaluateLiveness(payloads.health, latest, Date.now());
      const storedHeight = blockHeight(latest);
      const lag = reportedHeight !== null && storedHeight !== null ? Math.max(0, Math.floor(reportedHeight - storedHeight)) : null;
      const transactionCount = payloads.stats && payloads.stats.total_transactions;
      const peers = (payloads.health && payloads.health.peers) ?? (payloads.stats && payloads.stats.connected_peers);
      const registeredValidators = (payloads.network && payloads.network.validators_registered)
        ?? (payloads.validators && payloads.validators.count)
        ?? (payloads.health && payloads.health.validators)
        ?? (payloads.stats && payloads.stats.validators);
      const activeValidators = payloads.network && payloads.network.validators_active;

      text(elements.metricHeight, reportedHeight === null ? "Unknown" : formatInteger(reportedHeight));
      text(elements.metricHeightNote, "Highest height reported by this source during refresh");
      text(elements.metricStoredHeight, storedHeight === null ? "Unknown" : formatInteger(storedHeight));
      text(elements.metricStoredNote, lag === null ? "No retained header available" : lag === 0 ? "Matches reported height" : `${formatInteger(lag)} behind reported height`);
      text(elements.metricBlockAge, formatDuration(liveness.ageSecs));
      text(elements.metricLivenessNote, liveness.state === "advancing" ? "Block timestamp is fresh" : liveness.state === "stalled" ? "Block production appears stalled" : "Cannot infer block liveness");
      text(elements.metricTransactions, transactionCount === undefined ? "Unknown" : formatInteger(transactionCount));
      text(elements.metricPeers, peers === undefined ? "Unknown" : formatInteger(peers));
      text(elements.metricValidators, registeredValidators === undefined ? "Unknown" : formatInteger(registeredValidators));
      text(elements.metricValidatorNote, activeValidators === undefined || activeValidators === null ? "Reported set; active split unavailable" : `${formatInteger(activeValidators)} active by this node's rule`);

      renderBlocks(blocks);
      renderFacts(factsFrom(payloads, latest));
      text(elements.lastRefreshed, new Date().toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit", second: "2-digit" }));

      if (liveness.state === "advancing") {
        setSourceDot("online");
        setBanner("online", `${source.name} RPC reachable`, `Newest retained block #${formatInteger(storedHeight)} is ${formatDuration(liveness.ageSecs)} old.`);
      } else if (liveness.state === "stalled") {
        setSourceDot("stalled");
        setBanner("degraded", `${source.name} reachable · block production stale`, `Newest retained block is ${formatDuration(liveness.ageSecs)} old; RPC availability is not proof of chain progress.`);
      } else {
        setSourceDot("unknown");
        setBanner("degraded", `${source.name} RPC reachable · liveness unknown`, liveness.basis);
      }

      setRefreshBusy(false);
      if (opts.rerunLookup !== false) handleRoute();
    }

    function setInspectorHeading(kicker, title, closable) {
      text(elements.inspectorKicker, kicker);
      text(elements.inspectorTitle, title);
      elements.inspectorClose.hidden = !closable;
    }

    function renderInspectorEmpty() {
      setInspectorHeading("Lookup", "Select a block or search the chain", false);
      clear(elements.inspectorContent);
      const wrapper = create("div", "inspector-empty");
      wrapper.append(create("span", "", "⌕"), create("p", "", "Block, transaction, and address details stay pinned to the source above."));
      elements.inspectorContent.append(wrapper);
    }

    function renderInspectorLoading(kind, value) {
      setInspectorHeading(kind, `Loading ${value}`, true);
      clear(elements.inspectorContent);
      const wrapper = create("div", "loading-state");
      wrapper.append(create("div", "loading-ring"), create("p", "", `Querying ${selectedSource().name}; no other source will be used as a fallback.`));
      elements.inspectorContent.append(wrapper);
    }

    function renderInspectorError(kind, error, resource) {
      setInspectorHeading(kind, `${resource} unavailable`, true);
      clear(elements.inspectorContent);
      const wrapper = create("div", "error-state");
      wrapper.append(create("strong", "", error && error.status ? `HTTP ${error.status}` : "RPC unavailable"));
      wrapper.append(create("p", "", errorMessage(error, String(resource).toLowerCase())));
      wrapper.append(create("p", "", `Source: ${selectedSource().baseUrl}. ARC nodes can prune old blocks and indexes, so a 404 is source-specific.`));
      elements.inspectorContent.append(wrapper);
    }

    function appendDetailGrid(parent, fields) {
      const list = create("dl", "detail-grid");
      for (const field of fields) {
        if (field.value === undefined) continue;
        const item = create("div", `detail-item${field.wide ? " wide" : ""}`);
        const term = create("dt", "", field.label);
        const detail = create("dd");
        if (field.action && field.value !== null) {
          const button = create("button", "hash-link", field.display || String(field.value));
          button.type = "button";
          button.addEventListener("click", field.action);
          detail.append(button);
        } else {
          detail.textContent = field.display || (field.value === null ? "Unknown" : String(field.value));
        }
        if (field.title) detail.title = String(field.title);
        item.append(term, detail);
        list.append(item);
      }
      parent.append(list);
    }

    function appendRawSection(parent, title, payload) {
      const section = create("section", "detail-section");
      section.append(create("h3", "", title));
      const pre = create("pre", "raw-data");
      pre.textContent = JSON.stringify(payload, null, 2);
      section.append(pre);
      parent.append(section);
    }

    function appendTransactionChips(parent, hashes, limit) {
      const section = create("section", "detail-section");
      section.append(create("h3", "", "Transactions visible from this source"));
      const bounded = boundedItems(hashes, limit === undefined ? 100 : limit);
      if (!bounded.total) {
        section.append(create("p", "", "No transaction hashes were returned for this record."));
      } else {
        if (bounded.truncated) {
          section.append(create("p", "inspector-note", `Showing the first ${formatInteger(bounded.visible.length)} of ${formatInteger(bounded.total)} hashes returned by this source. The address-history RPC is currently unpaginated.`));
        }
        const list = create("ul", "chip-list");
        for (const hashValue of bounded.visible) {
          const hash = normalizeHex(hashValue);
          if (!hash) continue;
          const item = create("li");
          const button = create("button", "", formatHash(hash, 12, 10));
          button.type = "button";
          button.title = `0x${hash}`;
          button.addEventListener("click", () => navigate("tx", hash));
          item.append(button);
          list.append(item);
        }
        section.append(list);
      }
      parent.append(section);
    }

    function renderBlockDetail(block, txPayload) {
      const header = blockHeader(block);
      const height = blockHeight(block);
      const hash = blockHash(block);
      setInspectorHeading("Block", `Block #${formatInteger(height)}`, true);
      clear(elements.inspectorContent);
      const root = create("div");
      appendDetailGrid(root, [
        { label: "Height", value: height, display: formatInteger(height) },
        { label: "Timestamp", value: header.timestamp, display: formatTimestamp(header.timestamp) },
        { label: "Transactions", value: header.tx_count, display: formatInteger(header.tx_count) },
        { label: "Block hash", value: hash, display: formatHash(hash, 18, 14), title: hash, wide: true },
        { label: "Producer", value: header.producer, display: formatHash(header.producer, 18, 14), title: header.producer, wide: true },
        { label: "Parent hash", value: header.parent_hash, display: formatHash(header.parent_hash, 18, 14), title: header.parent_hash, wide: true },
        { label: "Transaction root", value: header.tx_root, display: formatHash(header.tx_root, 18, 14), title: header.tx_root, wide: true },
        { label: "State root", value: header.state_root, display: formatHash(header.state_root, 18, 14), title: header.state_root, wide: true },
        { label: "Protocol", value: header.protocol_version, display: header.protocol_version ? `${header.protocol_version.major}.${header.protocol_version.minor}.${header.protocol_version.patch}` : "Unknown" },
      ]);

      const returned = txPayload && Array.isArray(txPayload.transactions) ? txPayload.transactions : [];
      const hashes = returned.map((transaction) => transaction.hash).filter(Boolean);
      if (!hashes.length && Array.isArray(block.tx_hashes)) hashes.push(...block.tx_hashes);
      appendTransactionChips(root, hashes, 100);

      const nav = create("section", "detail-section");
      nav.append(create("h3", "", "Adjacent heights on this source"));
      const buttons = create("ul", "chip-list");
      if (height > 0) {
        const previousItem = create("li");
        const previous = create("button", "", `← Block #${formatInteger(height - 1)}`);
        previous.type = "button";
        previous.addEventListener("click", () => navigate("block", String(height - 1)));
        previousItem.append(previous);
        buttons.append(previousItem);
      }
      const nextItem = create("li");
      const next = create("button", "", `Block #${formatInteger(height + 1)} →`);
      next.type = "button";
      next.addEventListener("click", () => navigate("block", String(height + 1)));
      nextItem.append(next);
      buttons.append(nextItem);
      nav.append(buttons);
      root.append(nav);
      appendRawSection(root, "Raw source response", block);
      elements.inspectorContent.append(root);
    }

    function renderTransactionDetail(hash, full, receipt, note) {
      const body = full && full.body;
      const success = full && typeof full.success === "boolean" ? full.success : receipt && receipt.success;
      setInspectorHeading("Transaction", formatHash(hash, 14, 12), true);
      clear(elements.inspectorContent);
      const root = create("div");
      if (note) root.append(create("p", "inspector-note", note));

      appendDetailGrid(root, [
        { label: "Status", value: success, display: success === true ? "Success" : success === false ? "Failed" : "Receipt status unavailable" },
        { label: "Type", value: full && full.tx_type, display: (full && full.tx_type) || "Body unavailable" },
        { label: "Block", value: (full && full.block_height) ?? (receipt && receipt.block_height), display: formatInteger((full && full.block_height) ?? (receipt && receipt.block_height)) },
        { label: "Hash", value: hash, display: `0x${hash}`, wide: true },
        {
          label: "From",
          value: full && full.from,
          display: full && full.from ? formatHash(full.from, 18, 14) : "Unavailable in receipt",
          title: full && full.from,
          wide: true,
          action: full && normalizeHex(full.from) ? () => navigate("address", normalizeHex(full.from)) : null,
        },
        {
          label: "To",
          value: body && body.to,
          display: body && body.to ? formatHash(body.to, 18, 14) : "Not a transfer / unavailable",
          title: body && body.to,
          wide: true,
          action: body && normalizeHex(body.to) ? () => navigate("address", normalizeHex(body.to)) : null,
        },
        { label: "Nonce", value: full && full.nonce, display: full && full.nonce !== undefined ? formatInteger(full.nonce) : "Unavailable" },
        { label: "Fee (raw)", value: full && full.fee, display: full && full.fee !== undefined ? formatInteger(full.fee) : "Unavailable" },
        { label: "Gas used", value: (full && full.gas_used) ?? (receipt && receipt.gas_used), display: formatInteger((full && full.gas_used) ?? (receipt && receipt.gas_used)) },
      ]);

      if (body) appendRawSection(root, "Transaction body", body);
      appendRawSection(root, "Raw source response", full || receipt);
      elements.inspectorContent.append(root);
    }

    function renderAddressDetail(address, account, history) {
      const hashes = history && Array.isArray(history.tx_hashes) ? history.tx_hashes : [];
      setInspectorHeading("Address", formatHash(address, 14, 12), true);
      clear(elements.inspectorContent);
      const root = create("div");
      if (!account) {
        root.append(create("p", "inspector-note", "This source returned no current account record. Any history below is the node's local address index, not proof of a current balance."));
      }
      appendDetailGrid(root, [
        { label: "Address", value: address, display: `0x${address}`, wide: true },
        { label: "Balance (raw units)", value: account && account.balance, display: account && account.balance !== undefined ? formatInteger(account.balance) : "Account record unavailable" },
        { label: "Nonce", value: account && account.nonce, display: account && account.nonce !== undefined ? formatInteger(account.nonce) : "Account record unavailable" },
        { label: "Indexed transactions", value: history && history.tx_count, display: history && history.tx_count !== undefined ? formatInteger(history.tx_count) : "History endpoint unavailable" },
        { label: "Code hash", value: account && account.code_hash, display: account && account.code_hash ? formatHash(account.code_hash, 18, 14) : "None / unavailable", title: account && account.code_hash, wide: true },
        { label: "Storage root", value: account && account.storage_root, display: account && account.storage_root ? formatHash(account.storage_root, 18, 14) : "None / unavailable", title: account && account.storage_root, wide: true },
      ]);
      const boundedHistory = boundedItems(hashes, 50);
      appendTransactionChips(root, hashes, 50);
      if (account) appendRawSection(root, "Raw account response", account);
      if (history) {
        appendRawSection(root, "Address-history response preview", {
          ...history,
          tx_hashes: boundedHistory.visible,
          explorer_preview_truncated: boundedHistory.truncated,
          explorer_preview_count: boundedHistory.visible.length,
        });
      }
      elements.inspectorContent.append(root);
    }

    function beginLookup(kind, value) {
      if (state.lookupController) state.lookupController.abort();
      state.lookupController = new AbortController();
      const generation = ++state.lookupGeneration;
      const sourceId = state.sourceId;
      renderInspectorLoading(kind, value);
      return {
        generation,
        sourceId,
        signal: state.lookupController.signal,
        current() {
          return generation === state.lookupGeneration && sourceId === state.sourceId;
        },
      };
    }

    async function inspectBlock(value) {
      const parsed = classifyLookup(value, "block");
      if (parsed.error) {
        renderInspectorError("Block", new RpcError(parsed.error, 400), "Block");
        return;
      }
      const lookup = beginLookup("Block", `#${formatInteger(parsed.value)}`);
      const [blockResult, txResult] = await Promise.allSettled([
        requestJson(lookup.sourceId, `/block/${parsed.value}`, { signal: lookup.signal }),
        requestJson(lookup.sourceId, `/block/${parsed.value}/txs?offset=0&limit=100`, { signal: lookup.signal }),
      ]);
      if (!lookup.current()) return;
      if (blockResult.status === "rejected") {
        if (!blockResult.reason.aborted) renderInspectorError("Block", blockResult.reason, "Block");
        return;
      }
      renderBlockDetail(blockResult.value, fulfilled(txResult));
      elements.inspector.scrollIntoView({ behavior: "smooth", block: "start" });
    }

    async function inspectTransaction(value) {
      const hash = normalizeHex(value);
      if (!hash) {
        renderInspectorError("Transaction", new RpcError("Invalid transaction hash.", 400), "Transaction");
        return;
      }
      const lookup = beginLookup("Transaction", formatHash(hash, 14, 12));
      const [fullResult, receiptResult] = await Promise.allSettled([
        requestJson(lookup.sourceId, `/tx/${hash}/full`, { signal: lookup.signal }),
        requestJson(lookup.sourceId, `/tx/${hash}`, { signal: lookup.signal }),
      ]);
      if (!lookup.current()) return;
      const full = fulfilled(fullResult);
      const receipt = fulfilled(receiptResult);
      if (!full && !receipt) {
        const error = fullResult.status === "rejected" ? fullResult.reason : receiptResult.reason;
        if (!error.aborted) renderInspectorError("Transaction", error, "Transaction");
        return;
      }
      const note = full ? null : "This source returned a receipt but not the full transaction body. The explorer is showing only fields the receipt proves.";
      renderTransactionDetail(hash, full, receipt, note);
      elements.inspector.scrollIntoView({ behavior: "smooth", block: "start" });
    }

    async function inspectAddress(value) {
      const address = normalizeHex(value);
      if (!address) {
        renderInspectorError("Address", new RpcError("Invalid ARC address.", 400), "Address");
        return;
      }
      const lookup = beginLookup("Address", formatHash(address, 14, 12));
      const [accountResult, historyResult] = await Promise.allSettled([
        requestJson(lookup.sourceId, `/account/${address}`, { signal: lookup.signal }),
        requestJson(lookup.sourceId, `/account/${address}/txs`, { signal: lookup.signal }),
      ]);
      if (!lookup.current()) return;
      const account = fulfilled(accountResult);
      const history = fulfilled(historyResult);
      const hasHistory = history && Array.isArray(history.tx_hashes) && history.tx_hashes.length > 0;
      if (!account && !hasHistory) {
        const error = accountResult.status === "rejected" ? accountResult.reason : new RpcError("Address not found.", 404);
        if (!error.aborted) renderInspectorError("Address", error, "Address");
        return;
      }
      renderAddressDetail(address, account, history);
      elements.inspector.scrollIntoView({ behavior: "smooth", block: "start" });
    }

    async function inspectAutoHash(value) {
      const hash = normalizeHex(value);
      if (!hash) {
        renderInspectorError("Lookup", new RpcError("Invalid 32-byte value.", 400), "Lookup");
        return;
      }
      const lookup = beginLookup("Auto lookup", formatHash(hash, 14, 12));
      let full = null;
      try {
        full = await requestJson(lookup.sourceId, `/tx/${hash}/full`, { signal: lookup.signal });
      } catch (error) {
        if (error.aborted || !lookup.current()) return;
        if (error.status !== 404) {
          renderInspectorError("Auto lookup", error, "Transaction");
          return;
        }
      }
      if (!lookup.current()) return;
      if (full) {
        let receipt = null;
        try {
          receipt = await requestJson(lookup.sourceId, `/tx/${hash}`, { signal: lookup.signal });
        } catch (_) {
          // The full record already proves the transaction exists.
        }
        if (lookup.current()) renderTransactionDetail(hash, full, receipt, null);
        return;
      }

      const [accountResult, historyResult] = await Promise.allSettled([
        requestJson(lookup.sourceId, `/account/${hash}`, { signal: lookup.signal }),
        requestJson(lookup.sourceId, `/account/${hash}/txs`, { signal: lookup.signal }),
      ]);
      if (!lookup.current()) return;
      const account = fulfilled(accountResult);
      const history = fulfilled(historyResult);
      const hasHistory = history && Array.isArray(history.tx_hashes) && history.tx_hashes.length > 0;
      if (account || hasHistory) {
        renderAddressDetail(hash, account, history);
      } else {
        renderInspectorError("Auto lookup", new RpcError("No transaction or address record was found on this source.", 404), "Lookup");
      }
      elements.inspector.scrollIntoView({ behavior: "smooth", block: "start" });
    }

    function parseRoute() {
      const raw = window.location.hash.replace(/^#\/?/, "");
      if (!raw) return null;
      const separator = raw.indexOf("/");
      if (separator === -1) return null;
      try {
        return { kind: raw.slice(0, separator), value: decodeURIComponent(raw.slice(separator + 1)) };
      } catch (_) {
        return null;
      }
    }

    function handleRoute() {
      const route = parseRoute();
      if (!route) {
        if (state.lookupController) state.lookupController.abort();
        state.lookupGeneration += 1;
        renderInspectorEmpty();
        return;
      }
      if (route.kind === "block") inspectBlock(route.value);
      else if (route.kind === "tx") inspectTransaction(route.value);
      else if (route.kind === "address") inspectAddress(route.value);
      else if (route.kind === "lookup") inspectAutoHash(route.value);
      else renderInspectorError("Lookup", new RpcError("Unsupported explorer route.", 400), "Lookup");
    }

    function navigate(kind, value) {
      const nextHash = `#/${kind}/${encodeURIComponent(value)}`;
      if (window.location.hash === nextHash) handleRoute();
      else window.location.hash = nextHash;
    }

    function selectSource(sourceId) {
      if (!sourceFor(sourceId)) return;
      state.sourceId = sourceId;
      if (state.lookupController) state.lookupController.abort();
      state.lookupGeneration += 1;
      renderInspectorEmpty();
      persistSourceId(sourceId);
      updateSourceChrome();
      refresh({ keepMetrics: false, rerunLookup: true });
    }

    elements.sourceSelect.addEventListener("change", (event) => selectSource(event.target.value));
    elements.refreshButton.addEventListener("click", () => refresh({ keepMetrics: true, rerunLookup: true }));
    elements.inspectorClose.addEventListener("click", () => { window.location.hash = "#/"; });
    elements.searchForm.addEventListener("submit", (event) => {
      event.preventDefault();
      const lookup = classifyLookup(elements.searchInput.value, elements.searchKind.value);
      text(elements.searchError, lookup.error || "");
      if (!lookup.error) navigate(lookup.kind, lookup.value);
    });
    window.addEventListener("hashchange", handleRoute);
    document.addEventListener("visibilitychange", () => {
      if (!document.hidden) refresh({ keepMetrics: true, rerunLookup: false });
    });

    populateSources();
    state.sourceId = savedSourceId();
    updateSourceChrome();
    refresh({ keepMetrics: false, rerunLookup: true });
    state.timer = window.setInterval(() => {
      if (!document.hidden) refresh({ keepMetrics: true, rerunLookup: false });
    }, REFRESH_INTERVAL_MS);
  }

  return Object.freeze({
    SOURCES,
    RpcError,
    boundedItems,
    buildRpcUrl,
    classifyLookup,
    evaluateLiveness,
    formatDuration,
    formatHash,
    normalizeHex,
    reportedHeightFrom,
    sortBlocksNewestFirst,
    boot,
  });
});
