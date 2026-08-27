(function (root, factory) {
  const network = root?.ArcNetwork || (typeof require === "function" ? require("../shared/frontend/arc-network.js") : null);
  const api = factory(network);
  if (typeof module === "object" && module.exports) module.exports = api;
  if (root) root.ArcExplorer = api;
  if (typeof document !== "undefined") document.addEventListener("DOMContentLoaded", api.boot, { once: true });
})(typeof globalThis !== "undefined" ? globalThis : this, function (network) {
  "use strict";

  if (!network) throw new Error("ARC network resolver did not load");

  const REFRESH_INTERVAL_MS = 30_000;
  const REQUEST_TIMEOUT_MS = 8_000;

  class RpcError extends Error {
    constructor(message, status, sourceId) {
      super(message);
      this.name = "RpcError";
      this.status = status || 0;
      this.sourceId = sourceId || null;
    }
  }

  function classifyLookup(raw, requestedKind) {
    const value = String(raw ?? "").trim();
    const kind = requestedKind || "auto";
    if (!value) return { error: "Enter a block height, transaction hash, or address." };
    if (kind === "block" || (kind === "auto" && /^\d+$/.test(value))) {
      if (!/^\d+$/.test(value)) return { error: "Block heights contain digits only." };
      const height = Number(value);
      if (!Number.isSafeInteger(height)) return { error: "Block height is outside the supported range." };
      return { kind: "block", value: String(height) };
    }
    const normalized = network.normalizeHex(value, 32);
    if (!normalized) return { error: "Transactions and addresses must be 32-byte hexadecimal values." };
    if (kind === "tx" || kind === "address") return { kind, value: normalized };
    return { kind: "lookup", value: normalized };
  }

  function extractBlocks(payload) {
    const candidates = Array.isArray(payload) ? payload : payload?.blocks ?? payload?.items ?? payload?.data?.blocks ?? [];
    return Array.isArray(candidates)
      ? [...candidates].filter(Boolean).sort((a, b) => (network.blockHeight(b) ?? -1) - (network.blockHeight(a) ?? -1))
      : [];
  }

  function extractRows(payload) {
    if (Array.isArray(payload)) return payload;
    for (const key of ["attestations", "transactions", "items", "records", "activity", "workers"]) {
      if (Array.isArray(payload?.[key])) return payload[key];
      if (Array.isArray(payload?.data?.[key])) return payload.data[key];
    }
    return [];
  }

  function numberOrNull(...values) {
    for (const value of values) {
      if (typeof value === "number" && Number.isFinite(value)) return value;
      if (typeof value === "string" && value.trim() && Number.isFinite(Number(value))) return Number(value);
    }
    return null;
  }

  function reportedHeight(snapshot) {
    const values = [
      snapshot?.health?.height,
      snapshot?.health?.block_height,
      snapshot?.info?.height,
      snapshot?.info?.block_height,
      snapshot?.stats?.height,
      snapshot?.stats?.block_height,
      network.blockHeight(snapshot?.latest),
    ].map((value) => numberOrNull(value)).filter((value) => value !== null);
    return values.length ? Math.max(...values) : null;
  }

  async function requestJson(fetchImpl, source, path, options) {
    const settings = options || {};
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort("timeout"), settings.timeoutMs || REQUEST_TIMEOUT_MS);
    const abort = () => controller.abort(settings.signal?.reason || "caller-abort");
    if (settings.signal) {
      if (settings.signal.aborted) abort();
      else settings.signal.addEventListener("abort", abort, { once: true });
    }
    try {
      const response = await fetchImpl(network.buildRpcUrl(source, path), {
        method: "GET",
        headers: { Accept: "application/json" },
        cache: "no-store",
        signal: controller.signal,
      });
      if (!response.ok) throw new RpcError(`RPC returned HTTP ${response.status}`, response.status, source.id);
      return await response.json();
    } catch (error) {
      if (error instanceof RpcError) throw error;
      if (controller.signal.aborted) throw new RpcError("RPC request timed out or was cancelled", 0, source.id);
      throw new RpcError(error?.message || "RPC request failed", 0, source.id);
    } finally {
      clearTimeout(timeout);
      if (settings.signal) settings.signal.removeEventListener("abort", abort);
    }
  }

  async function optionalRequest(fetchImpl, source, path, options) {
    try {
      return { ok: true, value: await requestJson(fetchImpl, source, path, options) };
    } catch (error) {
      return { ok: false, error };
    }
  }

  async function queryBlock(options) {
    const { resolver, fetchImpl, height, sourceId, signal } = options;
    const route = resolver.routeBlock(height, { sourceId });
    if (!route.ok) throw new RpcError(`Cannot resolve block: ${route.reason}`, 0, route.sourceId);
    const [blockResult, txsResult] = await Promise.all([
      optionalRequest(fetchImpl, route.source, `/block/${route.height}`, { signal }),
      optionalRequest(fetchImpl, route.source, `/block/${route.height}/txs?offset=0&limit=100`, { signal }),
    ]);
    if (!blockResult.ok) throw blockResult.error;
    return {
      route,
      block: blockResult.value,
      transactions: txsResult.ok ? txsResult.value : null,
      boundary: network.boundaryVerification(blockResult.value, resolver.config.checkpoint),
    };
  }

  async function queryTransaction(options) {
    const { resolver, fetchImpl, hash, sourceId, signal } = options;
    const planned = resolver.lookupSources({ sourceId });
    const attempts = await Promise.all(planned.map(async ({ source }) => {
      const [full, receipt] = await Promise.all([
        optionalRequest(fetchImpl, source, `/tx/${hash}/full`, { signal }),
        optionalRequest(fetchImpl, source, `/tx/${hash}`, { signal }),
      ]);
      if (!full.ok && !receipt.ok) return { source, found: false, errors: [full.error, receipt.error] };
      const fullValue = full.ok ? full.value : null;
      const receiptValue = receipt.ok ? receipt.value : null;
      const classification = network.classifyReceipt({
        tx: fullValue?.transaction ?? fullValue?.tx ?? fullValue,
        receipt: receiptValue?.receipt ?? receiptValue,
      });
      const provenance = classification.height === null
        ? { canonical: false, segment: "unverified", reason: "receipt-height-unavailable" }
        : resolver.classifyOccurrence(source.id, classification.height);
      return { source, found: true, full: fullValue, receipt: receiptValue, classification, provenance };
    }));
    return {
      occurrences: attempts.filter((attempt) => attempt.found),
      failures: attempts.filter((attempt) => !attempt.found),
      plannedSources: planned.map((entry) => entry.sourceId),
    };
  }

  async function queryAddress(options) {
    const { resolver, fetchImpl, address, sourceId, signal } = options;
    const planned = resolver.lookupSources({ sourceId });
    const attempts = await Promise.all(planned.map(async ({ source }) => {
      const [account, history] = await Promise.all([
        optionalRequest(fetchImpl, source, `/account/${address}`, { signal }),
        optionalRequest(fetchImpl, source, `/account/${address}/txs`, { signal }),
      ]);
      const historyValue = history.ok ? history.value : null;
      const txHashes = historyValue?.tx_hashes ?? historyValue?.transactions ?? [];
      const found = account.ok || (Array.isArray(txHashes) && txHashes.length > 0);
      return { source, found, account: account.ok ? account.value : null, history: historyValue };
    }));
    return { records: attempts.filter((attempt) => attempt.found), failures: attempts.filter((attempt) => !attempt.found) };
  }

  function boot() {
    const $ = (id) => document.getElementById(id);
    const elements = {
      networkLabel: $("network-label"), sourceSelect: $("source-select"), sourceDot: $("source-dot"), refreshButton: $("refresh-button"),
      banner: $("connection-banner"), bannerTitle: $("banner-title"), bannerDetail: $("banner-detail"), sourceName: $("source-name"), sourceEndpoint: $("source-endpoint"), lastRefreshed: $("last-refreshed"), sourceHelp: $("source-help"),
      recoveryTitle: $("recovery-title"), recoverySummary: $("recovery-summary"), checkpointHeight: $("checkpoint-height"), checkpointHash: $("checkpoint-hash"), boundaryHeight: $("boundary-height"), boundaryState: $("boundary-state"), continuationLabel: $("continuation-label"), manifestHash: $("manifest-hash"),
      metricHeight: $("metric-height"), metricHeightNote: $("metric-height-note"), metricStoredHeight: $("metric-stored-height"), metricStoredNote: $("metric-stored-note"), metricBlockAge: $("metric-block-age"), metricLivenessNote: $("metric-liveness-note"), metricTransactions: $("metric-transactions"), metricPeers: $("metric-peers"), metricValidators: $("metric-validators"), metricValidatorNote: $("metric-validator-note"),
      blocksStatus: $("blocks-status"), blocksBody: $("blocks-body"), sourceFacts: $("source-facts"), inferenceStatus: $("inference-status"), inferenceList: $("inference-list"), rewardsStatus: $("rewards-status"), rewardsList: $("rewards-list"),
      searchForm: $("search-form"), searchInput: $("search-input"), searchKind: $("search-kind"), searchError: $("search-error"), inspector: $("inspector"), inspectorKicker: $("inspector-kicker"), inspectorTitle: $("inspector-title"), inspectorClose: $("inspector-close"), inspectorContent: $("inspector-content"),
    };
    const state = { config: null, resolver: null, sourceId: "canonical", refreshController: null, lookupController: null, timer: null };

    const text = (node, value) => { if (node) node.textContent = value == null ? "" : String(value); };
    const clear = (node) => { if (node) node.replaceChildren(); };
    const create = (tag, className, content) => {
      const node = document.createElement(tag);
      if (className) node.className = className;
      if (content !== undefined && content !== null) node.textContent = String(content);
      return node;
    };
    const formatInteger = (value) => {
      const number = numberOrNull(value);
      return number === null ? "Unavailable" : new Intl.NumberFormat().format(number);
    };
    const formatTimestamp = (timestamp) => {
      const raw = numberOrNull(timestamp);
      if (raw === null) return "Unavailable";
      const date = new Date(raw < 10_000_000_000 ? raw * 1000 : raw);
      return Number.isNaN(date.getTime()) ? "Unavailable" : date.toLocaleString();
    };
    const sourceDisplay = (source) => `${source.name}${source.region ? ` · ${source.region}` : ""}`;
    const currentSource = () => state.sourceId === "canonical" ? state.resolver?.currentSource() : state.resolver?.source(state.sourceId);

    function setBanner(kind, title, detail) {
      elements.banner.className = `connection-banner ${kind}`;
      text(elements.bannerTitle, title);
      text(elements.bannerDetail, detail);
      elements.sourceDot.className = `status-dot ${kind === "online" ? "online" : kind === "degraded" ? "stalled" : kind === "error" ? "offline" : "unknown"}`;
    }

    function fact(label, value, title) {
      const row = create("div");
      row.append(create("dt", "", label));
      const dd = create("dd", "", value ?? "Unavailable");
      if (title) dd.title = title;
      row.append(dd);
      return row;
    }

    function renderFacts(source, snapshot, liveness) {
      clear(elements.sourceFacts);
      const latest = snapshot?.latest;
      elements.sourceFacts.append(
        fact("Source", source ? sourceDisplay(source) : "Unavailable"),
        fact("Node version", snapshot?.info?.version ?? snapshot?.health?.version ?? "Unavailable"),
        fact("Reachability", snapshot ? "RPC answered" : "Unreachable"),
        fact("Chain liveness", liveness?.state ?? "Unknown", liveness?.basis),
        fact("Latest block hash", network.formatHash(network.blockHash(latest)), network.blockHash(latest)),
        fact("Latest state root", network.formatHash(network.stateRoot(latest)), network.stateRoot(latest)),
      );
    }

    function renderRecovery(boundary) {
      const checkpoint = state.config?.checkpoint;
      if (!checkpoint) {
        text(elements.recoveryTitle, "Recovery checkpoint unavailable");
        text(elements.recoverySummary, "Canonical claims are paused. Legacy peers are not automatically treated as one chain.");
        return;
      }
      text(elements.recoveryTitle, `Signed checkpoint #${formatInteger(checkpoint.height)} → protocol v3`);
      text(elements.recoverySummary, "History through H is served by the approved legacy archive. H+1 begins the configured v3 continuation.");
      text(elements.checkpointHeight, `H ${formatInteger(checkpoint.height)}`);
      text(elements.checkpointHash, network.formatHash(checkpoint.blockHash, 10, 8));
      elements.checkpointHash.title = `0x${checkpoint.blockHash}`;
      text(elements.boundaryHeight, `H+1 ${formatInteger(checkpoint.recoveryHeight)}`);
      const messages = {
        verified: "Parent hash matches signed H",
        mismatch: "PARENT HASH MISMATCH",
        unknown: "Parent link unavailable",
        "not-boundary": "Boundary response unavailable",
      };
      text(elements.boundaryState, messages[boundary?.state] || "Parent link not checked");
      elements.boundaryState.className = boundary?.state === "verified" ? "truth-good" : boundary?.state === "mismatch" ? "truth-bad" : "truth-warn";
      text(elements.continuationLabel, `Continuation #${formatInteger(checkpoint.recoveryHeight + 1)}+`);
      text(elements.manifestHash, `Manifest ${network.formatHash(checkpoint.manifestHash, 8, 6)}`);
      elements.manifestHash.title = `0x${checkpoint.manifestHash}`;
    }

    function populateSources() {
      clear(elements.sourceSelect);
      const canonical = create("option", "", "Canonical timeline · automatic");
      canonical.value = "canonical";
      elements.sourceSelect.append(canonical);
      for (const source of state.config.sources.filter((item) => item.enabled)) {
        const prefix = source.kind === "legacy-fork" ? "NON-CANONICAL" : source.kind === "diagnostic" ? "DIAGNOSTIC" : source.kind === "legacy-canonical" ? "SIGNED ARCHIVE" : source.id === state.config.checkpoint?.v3SourceId ? "V3 CANONICAL" : "V3 REPLICA";
        const option = create("option", "", `${prefix} · ${sourceDisplay(source)}`);
        option.value = source.id;
        elements.sourceSelect.append(option);
      }
      elements.sourceSelect.value = "canonical";
    }

    function updateSourceChrome() {
      const source = currentSource();
      if (state.sourceId === "canonical") {
        text(elements.sourceName, "Canonical timeline");
        text(elements.sourceEndpoint, source ? `Height-routed · current ${source.name}` : "No canonical route configured");
        text(elements.sourceHelp, "Blocks resolve to the signed legacy archive through H and protocol v3 from H+1 onward.");
      } else {
        text(elements.sourceName, source ? sourceDisplay(source) : "Unavailable source");
        text(elements.sourceEndpoint, source?.baseUrl ?? "Unavailable");
        text(elements.sourceHelp, source?.id === state.config.checkpoint?.v3SourceId ? "Explicit canonical v3 source view." : "Explicit source view. Results are not promoted into the canonical timeline.");
      }
    }

    function resetMetrics() {
      for (const node of [elements.metricHeight, elements.metricStoredHeight, elements.metricBlockAge, elements.metricTransactions, elements.metricPeers, elements.metricValidators]) text(node, "—");
      text(elements.metricHeightNote, "Awaiting source evidence");
      text(elements.metricStoredNote, "No block header loaded");
      text(elements.metricLivenessNote, "Liveness unknown");
      text(elements.metricValidatorNote, "Availability unknown");
    }

    function blockTimestamp(block) {
      const header = network.blockHeader(block);
      return numberOrNull(header.timestamp, block?.timestamp);
    }

    function txCount(block) {
      const header = network.blockHeader(block);
      const explicit = numberOrNull(block?.tx_count, header.tx_count, block?.transactions_count);
      if (explicit !== null) return explicit;
      if (Array.isArray(block?.transactions)) return block.transactions.length;
      if (Array.isArray(block?.tx_hashes)) return block.tx_hashes.length;
      return null;
    }

    function renderBlocks(blocks, source) {
      clear(elements.blocksBody);
      if (!blocks.length) {
        const tr = create("tr");
        const td = create("td", "empty-cell", "No retained blocks were returned by this source.");
        td.colSpan = 5;
        tr.append(td);
        elements.blocksBody.append(tr);
        text(elements.blocksStatus, "Unavailable");
        return;
      }
      for (const block of blocks.slice(0, 12)) {
        const height = network.blockHeight(block);
        const canonical = height === null ? { canonical: false, segment: "unverified" } : state.resolver.classifyOccurrence(source.id, height);
        const tr = create("tr");
        const heightCell = create("td");
        const button = create("button", "table-link", height === null ? "Unknown" : `#${formatInteger(height)}`);
        button.type = "button";
        if (height !== null) button.addEventListener("click", () => navigate("block", String(height)));
        heightCell.append(button);
        const segment = canonical.canonical ? canonical.segment.replaceAll("-", " ") : "non-canonical / unverified";
        const stamp = blockTimestamp(block);
        const age = stamp === null ? null : Math.max(0, Math.round((Date.now() - (stamp < 10_000_000_000 ? stamp * 1000 : stamp)) / 1000));
        tr.append(heightCell, create("td", canonical.canonical ? "truth-good" : "truth-warn", segment), create("td", "", age === null ? "Unavailable" : network.formatDuration(age)), create("td", "", formatInteger(txCount(block))), create("td", "", network.formatHash(network.blockHash(block))));
        elements.blocksBody.append(tr);
      }
      text(elements.blocksStatus, `${Math.min(12, blocks.length)} shown · ${source.name}`);
    }

    function renderInference(payload, source) {
      clear(elements.inferenceList);
      const rows = extractRows(payload);
      const normalized = rows.map((row) => ({ row, receipt: network.classifyReceipt(row) }));
      const confirmed = normalized.filter(({ receipt }) => receipt.inferenceConfirmed && receipt.height !== null && state.resolver.classifyOccurrence(source.id, receipt.height).canonical);
      const excluded = rows.length - confirmed.length;
      if (!confirmed.length) elements.inferenceList.append(create("p", "empty-cell", rows.length ? `${rows.length} observation(s) returned, but none had a successful canonical mined receipt.` : "No inference activity was returned."));
      for (const { row, receipt } of confirmed.slice(0, 8)) {
        const card = create("article", "evidence-card");
        const heading = create("div", "evidence-heading");
        heading.append(create("strong", "", `Inference receipt · #${formatInteger(receipt.height)}`), create("span", "status-pill online", "MINED SUCCESS"));
        card.append(heading, create("code", "", receipt.txHash ? `0x${receipt.txHash}` : "Transaction hash unavailable"), create("small", "", `Source: ${source.name} · ${row.model_id ? `model ${network.formatHash(row.model_id)}` : "model unavailable"}`));
        if (receipt.txHash) card.addEventListener("click", () => navigate("tx", receipt.txHash));
        elements.inferenceList.append(card);
      }
      text(elements.inferenceStatus, `${confirmed.length} confirmed${excluded ? ` · ${excluded} excluded` : ""}`);
    }

    function economicValue(payload, keys) {
      for (const key of keys) {
        const value = key.split(".").reduce((current, part) => current?.[part], payload);
        if (value !== undefined && value !== null && value !== "") return value;
      }
      return null;
    }

    function renderRewards(payload) {
      clear(elements.rewardsList);
      const enabled = economicValue(payload, ["community_rewards_v1_enabled", "enabled", "issuance.enabled"]);
      const active = economicValue(payload, ["community_rewards_v1_protocol_active", "protocol_active", "issuance.protocol_active"]);
      const reward = economicValue(payload, ["attestation_reward_arc", "community_attestation_reward_arc", "reward_per_attestation_arc"]);
      const observed = economicValue(payload, ["attestations_per_day_observed", "observed.attestations_per_day"]);
      const readiness = enabled === true && active === true ? "Enabled and protocol-active" : enabled === false || active === false ? "Not issuing" : "Unavailable";
      elements.rewardsList.append(
        fact("Issuance", readiness),
        fact("Attestation reward", reward === null ? "Unavailable" : `${reward} ARC configured rate`),
        fact("Observed worker rate", observed === null ? "Unavailable" : `${observed}/day · backward-looking`),
        fact("Projected earnings", reward !== null && observed !== null ? `${Number(reward) * Number(observed)} ARC/day · projection, not earned` : "Unavailable without observed worker evidence"),
      );
      text(elements.rewardsStatus, payload ? "Current source report" : "Unavailable");
    }

    async function loadSnapshot(source, signal) {
      const requests = await Promise.all([
        optionalRequest(window.fetch.bind(window), source, "/health", { signal }),
        optionalRequest(window.fetch.bind(window), source, "/info", { signal }),
        optionalRequest(window.fetch.bind(window), source, "/stats", { signal }),
        optionalRequest(window.fetch.bind(window), source, "/validators", { signal }),
        optionalRequest(window.fetch.bind(window), source, "/block/latest", { signal }),
      ]);
      const [health, info, stats, validators, latest] = requests.map((result) => result.ok ? result.value : null);
      if (!requests.some((result) => result.ok)) throw requests[0].error;
      const height = network.blockHeight(latest) ?? numberOrNull(health?.height, info?.block_height, stats?.block_height);
      let blocks = latest ? [latest] : [];
      if (height !== null) {
        const from = Math.max(0, height - 11);
        const recent = await optionalRequest(window.fetch.bind(window), source, `/blocks?from=${from}&to=${height}&limit=12`, { signal });
        if (recent.ok && extractBlocks(recent.value).length) blocks = extractBlocks(recent.value);
      }
      return { health, info, stats, validators, latest, blocks };
    }

    async function refresh() {
      if (!state.resolver) return;
      state.refreshController?.abort();
      state.refreshController = new AbortController();
      const signal = state.refreshController.signal;
      elements.refreshButton.classList.add("spinning");
      const source = currentSource();
      updateSourceChrome();
      if (!source) {
        resetMetrics();
        renderFacts(null, null, null);
        renderRecovery(null);
        setBanner("degraded", "Canonical recovery is not configured", state.config.notices[0] || "No approved checkpoint and v3 source are available.");
        text(elements.blocksStatus, "Paused");
        text(elements.inferenceStatus, "Paused");
        text(elements.rewardsStatus, "Paused");
        elements.refreshButton.classList.remove("spinning");
        return;
      }
      const alternate = state.sourceId !== "canonical" && source.id !== state.config.checkpoint?.v3SourceId && source.id !== state.config.checkpoint?.legacySourceId;
      setBanner("loading", alternate ? "Loading explicit alternate source…" : "Loading canonical source…", sourceDisplay(source));
      try {
        const [snapshotResult, boundaryResult, inferenceResult, rewardsResult] = await Promise.all([
          loadSnapshot(source, signal).then((value) => ({ ok: true, value }), (error) => ({ ok: false, error })),
          state.config.checkpoint ? optionalRequest(window.fetch.bind(window), state.resolver.source(state.config.checkpoint.v3SourceId), `/block/${state.config.checkpoint.recoveryHeight}`, { signal }) : Promise.resolve({ ok: false }),
          optionalRequest(window.fetch.bind(window), source, "/inference/attestations?limit=20", { signal }),
          optionalRequest(window.fetch.bind(window), source, "/economics/rewards", { signal }),
        ]);
        if (signal.aborted) return;
        if (!snapshotResult.ok) throw snapshotResult.error;
        const snapshot = snapshotResult.value;
        const height = reportedHeight(snapshot);
        const liveness = network.evaluateLiveness(snapshot.health, snapshot.latest);
        text(elements.metricHeight, formatInteger(height));
        text(elements.metricHeightNote, state.sourceId === "canonical" ? "Protocol-v3 current source" : "Explicit source report");
        text(elements.metricStoredHeight, formatInteger(network.blockHeight(snapshot.latest)));
        text(elements.metricStoredNote, snapshot.latest ? formatTimestamp(blockTimestamp(snapshot.latest)) : "Header unavailable");
        text(elements.metricBlockAge, liveness.ageSecs === null ? "—" : network.formatDuration(liveness.ageSecs));
        text(elements.metricLivenessNote, `${liveness.state} · ${liveness.basis}`);
        text(elements.metricTransactions, formatInteger(numberOrNull(snapshot.stats?.total_transactions, snapshot.info?.total_transactions)));
        text(elements.metricPeers, formatInteger(numberOrNull(snapshot.health?.peers, snapshot.info?.peer_count, snapshot.stats?.connected_peers)));
        const validatorRows = Array.isArray(snapshot.validators) ? snapshot.validators : snapshot.validators?.validators;
        text(elements.metricValidators, formatInteger(numberOrNull(snapshot.stats?.validators, snapshot.info?.validator_count, validatorRows?.length)));
        text(elements.metricValidatorNote, validatorRows ? `${validatorRows.length} records returned` : "Validator records unavailable");
        renderFacts(source, snapshot, liveness);
        renderBlocks(snapshot.blocks, source);
        const boundary = boundaryResult.ok ? network.boundaryVerification(boundaryResult.value, state.config.checkpoint) : { state: "unknown" };
        renderRecovery(boundary);
        renderInference(inferenceResult.ok ? inferenceResult.value : null, source);
        renderRewards(rewardsResult.ok ? rewardsResult.value : null);
        text(elements.lastRefreshed, new Date().toLocaleTimeString());
        if (boundary.state === "mismatch") setBanner("error", "Recovery boundary mismatch", "H+1 does not reference the configured signed checkpoint. Do not treat this continuation as canonical.");
        else if (alternate) setBanner("degraded", "NON-CANONICAL source view", `${sourceDisplay(source)} is being queried explicitly and is not merged into canonical results.`);
        else if (liveness.state === "stalled") setBanner("degraded", "RPC reachable, chain appears stalled", `${sourceDisplay(source)} answered, but its newest block is stale.`);
        else setBanner("online", "Canonical source reachable", `${sourceDisplay(source)} · liveness ${liveness.state}`);
      } catch (error) {
        if (!signal.aborted) {
          resetMetrics();
          renderFacts(source, null, null);
          renderRecovery(null);
          setBanner("error", "Selected source is unreachable", `${sourceDisplay(source)}: ${error.message}`);
        }
      } finally {
        elements.refreshButton.classList.remove("spinning");
      }
    }

    function setInspector(kicker, title) {
      text(elements.inspectorKicker, kicker);
      text(elements.inspectorTitle, title);
      elements.inspectorClose.hidden = false;
      clear(elements.inspectorContent);
    }

    function inspectorLoading(kicker, title) {
      setInspector(kicker, title);
      const wrap = create("div", "inspector-empty");
      wrap.append(create("span", "loading-ring"), create("p", "", "Resolving source and loading evidence…"));
      elements.inspectorContent.append(wrap);
    }

    function inspectorError(kicker, title, message) {
      setInspector(kicker, title);
      const wrap = create("div", "error-state");
      wrap.append(create("strong", "", title), create("p", "", message));
      elements.inspectorContent.append(wrap);
    }

    function detailGrid(items) {
      const grid = create("dl", "detail-grid");
      for (const [label, value, wide] of items) {
        const row = create("div", `detail-item${wide ? " wide" : ""}`);
        row.append(create("dt", "", label), create("dd", "", value ?? "Unavailable"));
        grid.append(row);
      }
      return grid;
    }

    function rawSection(label, value) {
      const section = create("section", "detail-section");
      section.append(create("h3", "", label), create("pre", "raw-data", JSON.stringify(value, null, 2)));
      return section;
    }

    async function inspectBlock(value) {
      const parsed = classifyLookup(value, "block");
      if (parsed.error) return inspectorError("Block", "Invalid height", parsed.error);
      state.lookupController?.abort();
      state.lookupController = new AbortController();
      inspectorLoading("Block", `#${formatInteger(parsed.value)}`);
      try {
        const result = await queryBlock({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), height: parsed.value, sourceId: state.sourceId, signal: state.lookupController.signal });
        setInspector(result.route.canonical ? "Canonical block" : "NON-CANONICAL BLOCK", `Block #${formatInteger(result.route.height)}`);
        const warning = !result.route.canonical ? create("p", "inspector-note warning", result.route.warning || "This result is outside the configured canonical route.") : null;
        if (warning) elements.inspectorContent.append(warning);
        if (result.boundary.state === "verified") elements.inspectorContent.append(create("p", "inspector-note good", "Recovery boundary verified: H+1 parent hash matches the signed H checkpoint."));
        if (result.boundary.state === "mismatch") elements.inspectorContent.append(create("p", "inspector-note error", "Recovery boundary mismatch: this block cannot be presented as the configured continuation."));
        const header = network.blockHeader(result.block);
        elements.inspectorContent.append(detailGrid([
          ["Canonical status", result.route.canonical ? "Canonical" : "Alternate / non-canonical"],
          ["Segment", result.route.segment.replaceAll("-", " ")],
          ["Source", sourceDisplay(result.route.source)],
          ["Timestamp", formatTimestamp(header.timestamp)],
          ["Block hash", network.blockHash(result.block) ? `0x${network.blockHash(result.block)}` : "Unavailable", true],
          ["Parent hash", network.parentHash(result.block) ? `0x${network.parentHash(result.block)}` : "Unavailable", true],
          ["State root", network.stateRoot(result.block) ? `0x${network.stateRoot(result.block)}` : "Unavailable", true],
          ["Transactions", formatInteger(txCount(result.block))],
        ]), rawSection("Block response", result.block));
        if (result.transactions) elements.inspectorContent.append(rawSection("Transaction index response", result.transactions));
      } catch (error) {
        if (!state.lookupController.signal.aborted) inspectorError("Block", "Block unavailable", error.message);
      }
    }

    function occurrenceCard(occurrence) {
      const { source, classification, provenance } = occurrence;
      const card = create("article", `occurrence-card ${provenance.canonical ? "canonical" : "alternate"}`);
      const heading = create("div", "evidence-heading");
      heading.append(create("strong", "", sourceDisplay(source)), create("span", `status-pill ${provenance.canonical ? "online" : "degraded"}`, provenance.canonical ? "CANONICAL" : "NOT CANONICAL"));
      card.append(heading, detailGrid([
        ["Receipt", classification.receiptBacked ? classification.status : "Absent / unproven"],
        ["Category", classification.category],
        ["Block", formatInteger(classification.height)],
        ["Segment", provenance.segment?.replaceAll("-", " ")],
        ["Inference", classification.inferenceConfirmed ? "Confirmed mined receipt" : "Not confirmed"],
        ["Reward", classification.rewardEarned ? "Earned · successful mined receipt" : "Not counted as earned"],
      ]));
      if (occurrence.full) card.append(rawSection("Transaction", occurrence.full));
      if (occurrence.receipt) card.append(rawSection("Receipt", occurrence.receipt));
      return card;
    }

    async function inspectTransaction(hash) {
      state.lookupController?.abort();
      state.lookupController = new AbortController();
      inspectorLoading("Transaction / receipt", network.formatHash(hash, 14, 12));
      try {
        const result = await queryTransaction({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), hash, sourceId: state.sourceId, signal: state.lookupController.signal });
        if (!result.occurrences.length) return inspectorError("Transaction / receipt", "Transaction not found", `No record was returned by ${result.plannedSources.length} permitted source(s). Alternate forks were not searched unless explicitly selected.`);
        setInspector("Transaction / receipt", network.formatHash(hash, 14, 12));
        elements.inspectorContent.append(create("p", "inspector-note", "Each occurrence is classified independently. A transaction on an alternate source is never promoted to the canonical timeline."));
        for (const occurrence of result.occurrences) elements.inspectorContent.append(occurrenceCard(occurrence));
      } catch (error) {
        if (!state.lookupController.signal.aborted) inspectorError("Transaction / receipt", "Lookup failed", error.message);
      }
    }

    async function inspectAddress(address) {
      state.lookupController?.abort();
      state.lookupController = new AbortController();
      inspectorLoading("Address", network.formatHash(address, 14, 12));
      try {
        const result = await queryAddress({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), address, sourceId: state.sourceId, signal: state.lookupController.signal });
        if (!result.records.length) return inspectorError("Address", "Address unavailable", "No account or indexed history was returned by the permitted sources.");
        setInspector("Address · source-separated", network.formatHash(address, 14, 12));
        elements.inspectorContent.append(create("p", "inspector-note", "Balances and histories below remain source-scoped. They are not added together across the recovery boundary."));
        for (const record of result.records) {
          const card = create("article", "occurrence-card");
          card.append(create("h3", "", sourceDisplay(record.source)), detailGrid([
            ["Balance (raw)", formatInteger(record.account?.balance)], ["Nonce", formatInteger(record.account?.nonce)], ["Indexed transactions", formatInteger(record.history?.tx_count ?? record.history?.tx_hashes?.length)],
          ]));
          if (record.account) card.append(rawSection("Account response", record.account));
          if (record.history) card.append(rawSection("Address history", record.history));
          elements.inspectorContent.append(card);
        }
      } catch (error) {
        if (!state.lookupController.signal.aborted) inspectorError("Address", "Lookup failed", error.message);
      }
    }

    async function inspectAutoHash(hash) {
      const result = await queryTransaction({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), hash, sourceId: state.sourceId });
      if (result.occurrences.length) return inspectTransaction(hash);
      return inspectAddress(hash);
    }

    function parseRoute() {
      const raw = window.location.hash.replace(/^#\/?/, "");
      if (!raw) return null;
      const split = raw.indexOf("/");
      if (split < 0) return null;
      try { return { kind: raw.slice(0, split), value: decodeURIComponent(raw.slice(split + 1)) }; } catch (_error) { return null; }
    }

    function handleRoute() {
      const route = parseRoute();
      if (!route) {
        elements.inspectorClose.hidden = true;
        text(elements.inspectorKicker, "Lookup");
        text(elements.inspectorTitle, "Search canonical history or select an alternate source");
        clear(elements.inspectorContent);
        const empty = create("div", "inspector-empty");
        empty.append(create("span", "", "⌕"), create("p", "", "Block lookup resolves by checkpoint height. Transaction and address searches retain source provenance for every result."));
        elements.inspectorContent.append(empty);
        return;
      }
      if (!state.resolver) return inspectorError("Lookup", "Configuration unavailable", "The canonical resolver has not loaded.");
      if (route.kind === "block") inspectBlock(route.value);
      else if (route.kind === "tx") inspectTransaction(route.value);
      else if (route.kind === "address") inspectAddress(route.value);
      else if (route.kind === "lookup") inspectAutoHash(route.value).catch((error) => inspectorError("Lookup", "Lookup failed", error.message));
      else inspectorError("Lookup", "Unsupported route", "Use a block, transaction, or address search.");
    }

    function navigate(kind, value) {
      const hash = `#/${kind}/${encodeURIComponent(value)}`;
      if (window.location.hash === hash) handleRoute();
      else window.location.hash = hash;
    }

    elements.sourceSelect.addEventListener("change", () => {
      state.sourceId = elements.sourceSelect.value;
      updateSourceChrome();
      refresh();
      handleRoute();
    });
    elements.refreshButton.addEventListener("click", refresh);
    elements.searchForm.addEventListener("submit", (event) => {
      event.preventDefault();
      const parsed = classifyLookup(elements.searchInput.value, elements.searchKind.value);
      text(elements.searchError, parsed.error || "");
      if (!parsed.error) navigate(parsed.kind, parsed.value);
    });
    elements.inspectorClose.addEventListener("click", () => { window.location.hash = "#/"; });
    window.addEventListener("hashchange", handleRoute);
    document.addEventListener("visibilitychange", () => { if (!document.hidden) refresh(); });

    (async () => {
      try {
        const meta = document.querySelector('meta[name="arc-network-config"]');
        state.config = await network.loadConfig({
          injected: window.__ARC_NETWORK_CONFIG__,
          fetchImpl: window.fetch.bind(window),
          url: meta?.content || "../shared/frontend/arc-network.json",
        });
        state.resolver = network.createCanonicalResolver(state.config);
        text(elements.networkLabel, `${state.config.network.name} / ${state.config.state.toUpperCase()}`);
        populateSources();
        updateSourceChrome();
        renderRecovery(null);
        await refresh();
        handleRoute();
        state.timer = window.setInterval(() => { if (!document.hidden) refresh(); }, REFRESH_INTERVAL_MS);
      } catch (error) {
        resetMetrics();
        renderFacts(null, null, null);
        setBanner("error", "Explorer configuration rejected", error.message);
        inspectorError("Configuration", "No canonical chain view", "Publish a valid arc.frontend.network.v1 configuration. No legacy peer was selected automatically.");
      }
    })();
  }

  return Object.freeze({
    RpcError,
    classifyLookup,
    extractBlocks,
    extractRows,
    reportedHeight,
    requestJson,
    queryBlock,
    queryTransaction,
    queryAddress,
    boot,
  });
});
