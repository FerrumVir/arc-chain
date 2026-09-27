(function (root, factory) {
  const network = root?.ArcNetwork || (typeof require === "function" ? require("../shared/frontend/arc-network.js") : null);
  const receipts = root?.ArcNativeReceipts || (typeof require === "function" ? require("./native-receipts.js") : null);
  const api = factory(network, receipts);
  if (typeof module === "object" && module.exports) module.exports = api;
  if (root) root.ArcExplorer = api;
  if (typeof document !== "undefined") document.addEventListener("DOMContentLoaded", api.boot, { once: true });
})(typeof globalThis !== "undefined" ? globalThis : this, function (network, nativeReceipts) {
  "use strict";

  if (!network) throw new Error("ARC network resolver did not load");
  if (!nativeReceipts) throw new Error("ARC native receipt rules did not load");

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
    if (!normalized) return { error: "Transactions, native requests and addresses must be 32-byte hexadecimal values." };
    if (kind === "tx" || kind === "address" || kind === "request") return { kind, value: normalized };
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
    for (const key of ["activities", "attestations", "transactions", "items", "records", "activity", "workers"]) {
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

  function integerOrNull(...values) {
    for (const value of values) {
      if (typeof value === "number" && Number.isSafeInteger(value) && value >= 0) return value;
      if (typeof value === "string" && /^\d+$/.test(value.trim())) {
        const parsed = Number(value);
        if (Number.isSafeInteger(parsed)) return parsed;
      }
    }
    return null;
  }

  function formatExactInteger(value) {
    if (typeof value === "number") {
      return Number.isSafeInteger(value) && value >= 0
        ? new Intl.NumberFormat().format(value)
        : "Unavailable";
    }
    if (typeof value === "string" && /^\d+$/.test(value.trim())) {
      try { return new Intl.NumberFormat().format(BigInt(value.trim())); }
      catch (_) { return "Unavailable"; }
    }
    return "Unavailable";
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
    ].map((value) => integerOrNull(value)).filter((value) => value !== null);
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

  // Why a permitted source added nothing to a lookup. A 404 on every request
  // it was sent means it answered and holds no record. Any other status, no
  // response or a timeout leaves its record unknown. The inspector names each
  // such source: an outage is never read as agreement, and a missing record
  // never as an outage.
  function describeSilentSource(label, errors, noun) {
    const item = noun || "record";
    const seen = (Array.isArray(errors) ? errors : [errors]).filter(Boolean);
    const failure = seen.find((error) => error.status !== 404);
    if (seen.length && !failure) {
      return { label, state: "no-record", text: `${label}: answered, and holds no ${item} for this lookup.` };
    }
    if (failure?.status) {
      return { label, state: "unknown", text: `${label}: returned HTTP ${failure.status}, so its ${item} is unknown.` };
    }
    return { label, state: "unknown", text: `${label}: could not be asked (${failure?.message || "no usable answer"}), so its ${item} is unknown.` };
  }

  // Some deployed v0.8 gateways return 404 for the /block/latest alias even
  // though the source RPC implements it. Resolve the current height through
  // read-only status endpoints, then fetch the same canonical block shape.
  async function requestLatestBlock(fetchImpl, source, options) {
    const direct = await optionalRequest(fetchImpl, source, "/block/latest", options);
    if (direct.ok) return direct.value;
    if (direct.error?.status !== 404) throw direct.error;
    const [info, stats, health] = await Promise.all([
      optionalRequest(fetchImpl, source, "/info", options),
      optionalRequest(fetchImpl, source, "/stats", options),
      optionalRequest(fetchImpl, source, "/health", options),
    ]);
    const height = reportedHeight({ info: info.ok ? info.value : null, stats: stats.ok ? stats.value : null, health: health.ok ? health.value : null });
    if (height === null) throw direct.error;
    const block = await requestJson(fetchImpl, source, `/block/${height}`, options);
    if (network.blockHeight(block) !== height) throw new RpcError("latest block height did not match the advertised height", 0, source.id);
    return block;
  }

  async function verifyRecoveryCheckpoint(options) {
    const { resolver, fetchImpl, signal } = options;
    const checkpoint = resolver.config.checkpoint;
    const legacySource = checkpoint ? resolver.source(checkpoint.legacySourceId) : null;
    const replicas = resolver.v3Replicas();
    if (!checkpoint || !legacySource || !replicas.length) return { state: "unknown", reason: "checkpoint-sources-unavailable", legacy: { state: "unknown" }, replicas: [] };
    const [legacy, replicaEvidence] = await Promise.all([
      optionalRequest(fetchImpl, legacySource, `/block/${checkpoint.height}`, { signal }),
      Promise.all(replicas.map(async (source) => {
        const [boundary, info] = await Promise.all([
          optionalRequest(fetchImpl, source, `/block/${checkpoint.recoveryHeight}`, { signal }),
          optionalRequest(fetchImpl, source, "/network/info", { signal }),
        ]);
        return boundary.ok && info.ok
          ? { sourceId: source.id, boundaryBlock: boundary.value, networkInfo: info.value }
          : { sourceId: source.id, error: [boundary, info].filter((entry) => !entry.ok).map((entry) => entry.error?.message).join("; ") || "replica-evidence-unavailable" };
      })),
    ]);
    return network.auditRecoveryCheckpoint({ config: resolver.config, legacyBlock: legacy.ok ? legacy.value : null, replicas: replicaEvidence });
  }

  function downgradeRoute(route, reason, warning) {
    if (!route.canonical) return route;
    return { ...route, canonical: false, configuredCanonical: true, reason, warning };
  }

  async function requireLegacyArchiveProvenance(source, fetchImpl, signal) {
    if (source?.kind !== "legacy-fork") return null;
    const verification = await network.verifyLegacyArchiveSource({ source, fetchImpl, signal });
    if (verification.state !== "verified") {
      throw new RpcError(
        `Legacy archive provenance rejected (${verification.reason || verification.state})`,
        0,
        source.id,
      );
    }
    return verification;
  }

  async function queryBlock(options) {
    const { resolver, fetchImpl, height, sourceId, signal, checkpointAudit } = options;
    const configuredRoute = resolver.routeBlock(height, { sourceId });
    if (!configuredRoute.ok) throw new RpcError(`Cannot resolve block: ${configuredRoute.reason}`, 0, configuredRoute.sourceId);
    const archiveVerification = await requireLegacyArchiveProvenance(configuredRoute.source, fetchImpl, signal);
    const [blockResult, txsResult] = await Promise.all([
      optionalRequest(fetchImpl, configuredRoute.source, `/block/${configuredRoute.height}`, { signal }),
      optionalRequest(fetchImpl, configuredRoute.source, `/block/${configuredRoute.height}/txs?offset=0&limit=100`, { signal }),
    ]);
    if (!blockResult.ok) throw blockResult.error;
    const checkpoint = resolver.config.checkpoint;
    const boundary = network.boundaryVerification(blockResult.value, checkpoint);
    let route = network.gateCanonical(configuredRoute, checkpointAudit);
    if (configuredRoute.canonical && network.blockHeight(blockResult.value) !== configuredRoute.height) {
      route = downgradeRoute(route.canonical ? route : configuredRoute, "queried-height-mismatch", "The source returned a different block height than requested; this result is not canonical.");
    }
    if (configuredRoute.canonical && configuredRoute.height === checkpoint?.height) {
      const signed = network.checkpointVerification(blockResult.value, checkpoint);
      if (signed.state !== "verified") route = downgradeRoute(route.canonical ? route : configuredRoute, `signed-checkpoint-${signed.state}`, "The returned H block does not exactly match the signed hash and state root.");
    }
    if (configuredRoute.canonical && configuredRoute.height === checkpoint?.recoveryHeight && boundary.state !== "verified") {
      route = downgradeRoute(route.canonical ? route : configuredRoute, `recovery-boundary-${boundary.state}`, "The returned H+1 block does not exactly match the approved parent, block hash, and state root.");
    }
    return {
      route,
      block: blockResult.value,
      transactions: txsResult.ok ? txsResult.value : null,
      boundary,
      archiveVerification,
    };
  }

  const BLOCKS_PAGE_SIZE = 20;

  // A bounded, paged window of recent blocks, newest end first. Canonical mode
  // routes each side of the page the same way a single block does
  // (`resolver.routeBlock`); a page that straddles the signed checkpoint is
  // split into its legacy and v3 segments and each is fetched from its own
  // configured source, never blended into one request. An explicit source
  // bypasses that split entirely and serves the whole page, like
  // `queryTransaction`/`queryAddress` do.
  async function queryBlocksPage(options) {
    const { resolver, fetchImpl, startHeight, sourceId, signal } = options;
    if (!Number.isSafeInteger(startHeight) || startHeight < 0) {
      throw new RpcError("Block height is outside the supported range.", 0, null);
    }
    const from = Math.max(0, startHeight - BLOCKS_PAGE_SIZE + 1);
    const to = startHeight;
    const explicitId = sourceId && sourceId !== "canonical" ? sourceId : null;
    let segments;
    if (explicitId) {
      const selected = resolver.source(explicitId);
      if (!selected) throw new RpcError("Cannot resolve blocks: selected-source-unavailable", 0, explicitId);
      segments = [{ from, to, source: selected }];
    } else {
      const topRoute = resolver.routeBlock(to, {});
      if (!topRoute.ok) throw new RpcError(`Cannot resolve blocks: ${topRoute.reason}`, 0, null);
      const checkpoint = resolver.config.checkpoint;
      const crossesBoundary = checkpoint && from <= checkpoint.height && to > checkpoint.height;
      if (!crossesBoundary) {
        segments = [{ from, to, source: topRoute.source }];
      } else {
        const bottomRoute = resolver.routeBlock(checkpoint.height, {});
        if (!bottomRoute.ok) throw new RpcError(`Cannot resolve blocks: ${bottomRoute.reason}`, 0, null);
        segments = [
          { from, to: checkpoint.height, source: bottomRoute.source },
          { from: checkpoint.height + 1, to, source: topRoute.source },
        ];
      }
    }
    const fetched = await Promise.all(segments.map(async (segment) => {
      // Same rule as every other query: an explicitly selected preserved fork
      // is trusted only after its own provenance verifies, never on request
      // shape alone. A no-op for the legacy-canonical/v3 sources the
      // canonical split always uses.
      await requireLegacyArchiveProvenance(segment.source, fetchImpl, signal);
      const limit = segment.to - segment.from + 1;
      const result = await optionalRequest(fetchImpl, segment.source, `/blocks?from=${segment.from}&to=${segment.to}&limit=${limit}`, { signal });
      if (!result.ok) throw result.error;
      return extractBlocks(result.value).map((block) => ({ block, source: segment.source }));
    }));
    const rows = fetched.flat().sort((a, b) => (network.blockHeight(b.block) ?? -1) - (network.blockHeight(a.block) ?? -1));
    return { from, to, pageSize: BLOCKS_PAGE_SIZE, rows };
  }

  async function queryTransaction(options) {
    const { resolver, fetchImpl, hash, sourceId, signal, checkpointAudit } = options;
    // The hash reaches this function straight from the URL fragment, so it is
    // normalized before it is ever interpolated into an RPC path. Without this
    // an unvalidated fragment escapes /tx/ and queries arbitrary node paths.
    const txHash = network.normalizeHex(hash, 32);
    if (!txHash) throw new RpcError("Transactions must be 32-byte hexadecimal values", 0, null);
    const planned = resolver.lookupSources({ sourceId });
    const attempts = await Promise.all(planned.map(async ({ source }) => {
      const archiveVerification = await requireLegacyArchiveProvenance(source, fetchImpl, signal);
      const [full, receipt, rewardEvidence, occurrence] = await Promise.all([
        optionalRequest(fetchImpl, source, `/tx/${txHash}/full`, { signal }),
        optionalRequest(fetchImpl, source, `/tx/${txHash}`, { signal }),
        source.kind === "v3"
          ? optionalRequest(fetchImpl, source, `/community/reward_receipt/0x${txHash}`, { signal })
          : Promise.resolve({ ok: false, error: null }),
        source.kind === "legacy-fork"
          ? optionalRequest(fetchImpl, source, `/tx/${txHash}/occurrences`, { signal })
          : Promise.resolve({ ok: false, error: null }),
      ]);
      if (!full.ok && !receipt.ok && !rewardEvidence.ok && !occurrence.ok) {
        return { source, found: false, errors: [full.error, receipt.error, rewardEvidence.error, occurrence.error].filter(Boolean) };
      }
      const fullValue = full.ok ? full.value : null;
      const receiptValue = receipt.ok ? receipt.value : null;
      const rewardValue = rewardEvidence.ok ? rewardEvidence.value : null;
      const occurrenceValue = occurrence.ok ? occurrence.value : null;
      const rewardClassification = rewardValue
        ? network.classifyCommunityRewardReceipt(rewardValue, txHash)
        : null;
      const rewardEvidenceBound = rewardClassification !== null;
      const classification = rewardEvidenceBound
        ? rewardClassification
        : network.classifyReceipt({
          tx: fullValue?.transaction ?? fullValue?.tx ?? fullValue,
          receipt: receiptValue?.receipt ?? receiptValue,
        });
      const boundClassification = Object.freeze({
        ...classification,
        transactionHashMatches: classification.txHash === txHash
          && (classification.nativeInferenceStage
            ? classification.receiptTxHash === txHash
            : classification.receiptTxHash === undefined
              || classification.receiptTxHash === null
              || classification.receiptTxHash === txHash),
      });
      const occurrenceRows = Array.isArray(occurrenceValue?.occurrences) ? occurrenceValue.occurrences : [];
      const occurrenceHeight = occurrenceValue?.unique_occurrence === true && occurrenceRows.length === 1
        ? integerOrNull(occurrenceRows[0]?.block_height)
        : null;
      const evidenceHeight = boundClassification.nativeInferenceStage
        ? (boundClassification.transactionHashMatches ? boundClassification.height ?? occurrenceHeight : occurrenceHeight)
        : boundClassification.height ?? occurrenceHeight;
      const configured = evidenceHeight === null
        ? { canonical: false, segment: "unverified", reason: "receipt-height-unavailable" }
        : resolver.classifyOccurrence(source.id, evidenceHeight);
      const provenance = network.gateCanonical(configured, checkpointAudit);
      return {
        source,
        found: true,
        full: fullValue,
        receipt: receiptValue,
        rewardEvidence: rewardValue,
        rewardEvidenceBound,
        occurrence: occurrenceValue,
        classification: boundClassification,
        provenance,
        archiveVerification,
      };
    }));
    return {
      occurrences: attempts.filter((attempt) => attempt.found),
      failures: attempts.filter((attempt) => !attempt.found),
      plannedSources: planned.map((entry) => entry.sourceId),
    };
  }

  async function queryAddress(options) {
    const { resolver, fetchImpl, address, sourceId, signal, checkpointAudit } = options;
    // Same fragment-supplied input as queryTransaction: normalize before it can
    // reach an RPC path.
    const accountAddress = network.normalizeHex(address, 32);
    if (!accountAddress) throw new RpcError("Addresses must be 32-byte hexadecimal values", 0, null);
    const planned = resolver.lookupSources({ sourceId });
    const attempts = await Promise.all(planned.map(async ({ source }) => {
      const archiveVerification = await requireLegacyArchiveProvenance(source, fetchImpl, signal);
      const [account, history] = await Promise.all([
        optionalRequest(fetchImpl, source, `/account/${accountAddress}`, { signal }),
        optionalRequest(fetchImpl, source, `/account/${accountAddress}/txs`, { signal }),
      ]);
      const historyValue = history.ok ? history.value : null;
      const txHashes = Array.isArray(historyValue?.tx_hashes) ? historyValue.tx_hashes : [];
      const found = account.ok || txHashes.length > 0;
      // `/account/{address}/txs` never 404s for a well-formed address on a node
      // that implements it (an unused address just gets an empty list back),
      // so a 404 here means the route itself is absent - an older node, not
      // "no history". Any other failure is a genuine reachability problem.
      // Each gets its own message; none is silently treated as "no history".
      const historyState = history.ok ? "served" : history.error?.status === 404 ? "not-served" : history.error?.status ? "error" : "unreachable";
      const configuredCanonical = sourceId === "canonical"
        && (source.id === resolver.config.checkpoint?.v3SourceId || source.id === resolver.config.checkpoint?.legacySourceId);
      const provenance = network.gateCanonical(
        configuredCanonical ? { canonical: true, segment: "canonical-segment-source" } : { canonical: false, segment: "alternate-source" },
        checkpointAudit,
      );
      return {
        source, found,
        account: account.ok ? account.value : null,
        history: historyValue,
        historyState,
        historyError: history.ok ? null : (history.error?.message || null),
        errors: account.ok ? [] : [account.error, ...(history.ok ? [] : [history.error])],
        txHashes,
        provenance,
        archiveVerification,
      };
    }));
    return { records: attempts.filter((attempt) => attempt.found), failures: attempts.filter((attempt) => !attempt.found) };
  }

  // A native paid-inference request is identified by its request
  // id; its canonical record is the receipt each validator serves. Every
  // permitted source is asked, and the page shows whether the ones that
  // answer record the same settlement - a disagreement is shown, never
  // averaged away - and whether each settlement's credits reconcile.
  async function queryNativeRequest(options) {
    const { resolver, fetchImpl, requestId, sourceId, signal } = options;
    const id = network.normalizeHex(requestId, 32);
    if (!id) throw new RpcError("Native request ids must be 32-byte hexadecimal values", 0, null);
    // Canonical view: every enabled replica of the current network, since the
    // point is whether they agree. An explicitly selected source: that one.
    const planned = sourceId && sourceId !== "canonical"
      ? resolver.lookupSources({ sourceId })
      : resolver.v3Replicas().map((source) => ({ source, sourceId: source.id }));
    const answers = await Promise.all(planned.map(async ({ source }) => {
      // `/health` is fetched from the same source as the receipt, never a
      // different one: whether a Pending request has passed its expiry is a
      // claim about that source's own view of the chain, not a claim merged
      // across sources.
      const [result, health] = await Promise.all([
        optionalRequest(fetchImpl, source, `/native-inference/receipt/${id}`, { signal }),
        optionalRequest(fetchImpl, source, "/health", { signal }),
      ]);
      const returnedId = result.ok ? network.normalizeHex(result.value?.request_id, 32) : null;
      const receiptMatchesRequest = result.ok && returnedId === id;
      return {
        source,
        receipt: receiptMatchesRequest ? result.value : null,
        error: receiptMatchesRequest
          ? null
          : result.ok
            ? new RpcError("native receipt response does not match the requested request id", 0, source.id)
            : result.error,
        height: health.ok ? integerOrNull(health.value?.height) : null,
      };
    }));
    const answered = answers.filter((answer) => answer.receipt);
    return {
      requestId: id,
      answers,
      comparison: nativeReceipts.compareReplicas(
        answered.map((answer) => ({ source: answer.source.id, receipt: answer.receipt })),
      ),
      plannedSources: planned.map((entry) => entry.sourceId),
    };
  }

  function boot() {
    const $ = (id) => document.getElementById(id);
    const elements = {
      networkLabel: $("network-label"), sourceSelect: $("source-select"), sourceDot: $("source-dot"), refreshButton: $("refresh-button"),
      banner: $("connection-banner"), bannerTitle: $("banner-title"), bannerDetail: $("banner-detail"), sourceName: $("source-name"), sourceEndpoint: $("source-endpoint"), lastRefreshed: $("last-refreshed"), sourceHelp: $("source-help"),
      recoveryTitle: $("recovery-title"), recoverySummary: $("recovery-summary"), checkpointHeight: $("checkpoint-height"), checkpointHash: $("checkpoint-hash"), boundaryHeight: $("boundary-height"), boundaryState: $("boundary-state"), continuationLabel: $("continuation-label"), manifestHash: $("manifest-hash"),
      metricHeight: $("metric-height"), metricHeightNote: $("metric-height-note"), metricStoredHeight: $("metric-stored-height"), metricStoredNote: $("metric-stored-note"), metricBlockAge: $("metric-block-age"), metricLivenessNote: $("metric-liveness-note"), metricTransactions: $("metric-transactions"), metricPeers: $("metric-peers"), metricValidators: $("metric-validators"), metricValidatorNote: $("metric-validator-note"),
      blocksStatus: $("blocks-status"), blocksBody: $("blocks-body"), blocksPageLink: $("blocks-page-link"), sourceFacts: $("source-facts"), inferenceStatus: $("inference-status"), inferenceList: $("inference-list"), rewardsStatus: $("rewards-status"), rewardsList: $("rewards-list"),
      searchForm: $("search-form"), searchInput: $("search-input"), searchKind: $("search-kind"), searchError: $("search-error"), inspector: $("inspector"), inspectorKicker: $("inspector-kicker"), inspectorTitle: $("inspector-title"), inspectorClose: $("inspector-close"), inspectorContent: $("inspector-content"),
    };
    const state = { config: null, resolver: null, sourceId: "canonical", checkpointAudit: { state: "unknown", reason: "not-audited" }, refreshController: null, lookupController: null, timer: null, lastKnownHeight: null };

    const text = (node, value) => { if (node) node.textContent = value == null ? "" : String(value); };
    const clear = (node) => { if (node) node.replaceChildren(); };
    const create = (tag, className, content) => {
      const node = document.createElement(tag);
      if (className) node.className = className;
      if (content !== undefined && content !== null) node.textContent = String(content);
      return node;
    };
    const formatInteger = formatExactInteger;
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

    // The rows a `/block/{height}/txs` response actually carries, reduced to
    // just what is needed to link each one to its own transaction lookup.
    function transactionEntries(payload) {
      const rows = Array.isArray(payload?.transactions) ? payload.transactions : [];
      return rows
        .map((row) => ({ index: integerOrNull(row?.index), hash: network.normalizeHex(row?.hash, 32) }))
        .filter((row) => row.hash);
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
        const configured = height === null ? { canonical: false, segment: "unverified" } : state.resolver.classifyOccurrence(source.id, height);
        const canonical = network.gateCanonical(configured, state.checkpointAudit);
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
      const confirmed = normalized.filter(({ receipt }) => {
        if (!receipt.inferenceConfirmed || receipt.height === null) return false;
        return network.gateCanonical(state.resolver.classifyOccurrence(source.id, receipt.height), state.checkpointAudit).canonical;
      });
      const excluded = rows.length - confirmed.length;
      if (!confirmed.length) elements.inferenceList.append(create("p", "empty-cell", rows.length ? `${rows.length} observation(s) returned, but none had a successful canonical mined receipt.` : "No inference activity was returned."));
      for (const { row, receipt } of confirmed.slice(0, 8)) {
        const card = create("article", "evidence-card");
        const heading = create("div", "evidence-heading");
        heading.append(create("strong", "", `Inference receipt · #${formatInteger(receipt.height)}`), create("span", "status-pill online", receipt.paymentConfirmed ? "COMPUTED + PAID" : "COMPUTED · NOT PAYMENT"));
        const model = row.inference?.model_hash ?? row.model_id;
        card.append(heading, create("code", "", receipt.txHash ? `0x${receipt.txHash}` : "Transaction hash unavailable"), create("small", "", `Source: ${source.name} · ${model ? `model ${network.formatHash(model)}` : "model unavailable"}`));
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
      const projectionReason = typeof payload?.projected_daily_unavailable_reason === "string" && payload.projected_daily_unavailable_reason.trim()
        ? payload.projected_daily_unavailable_reason.trim()
        : "worker-specific authoritative projection is available only from /worker/earnings/{worker} after at least 3 exact successful mined 0x25 receipts spanning 24 hours";
      const readiness = enabled === true && active === true ? "Enabled and protocol-active" : enabled === false || active === false ? "Not issuing" : "Unavailable";
      elements.rewardsList.append(
        fact("Issuance", readiness),
        fact("Attestation reward", reward === null ? "Unavailable" : `${reward} ARC configured rate`),
        fact("Observed worker rate", observed === null ? "Unavailable" : `${observed}/day · backward-looking`),
        // `/economics/rewards` has no worker identity or exact receipt rows.
        // A numeric field on this fleet-wide endpoint can never establish a
        // worker projection, so the explorer always withholds it.
        fact("Projected earnings", `Unavailable · ${projectionReason}`),
      );
      text(elements.rewardsStatus, payload ? "Current source report" : "Unavailable");
    }

    async function loadSnapshot(source, signal) {
      const requests = await Promise.all([
        optionalRequest(window.fetch.bind(window), source, "/health", { signal }),
        optionalRequest(window.fetch.bind(window), source, "/info", { signal }),
        optionalRequest(window.fetch.bind(window), source, "/stats", { signal }),
        optionalRequest(window.fetch.bind(window), source, "/validators", { signal }),
        requestLatestBlock(window.fetch.bind(window), source, { signal }).then((value) => ({ ok: true, value })).catch((error) => ({ ok: false, error })),
      ]);
      const [health, info, stats, validators, latest] = requests.map((result) => result.ok ? result.value : null);
      if (!requests.some((result) => result.ok)) throw requests[0].error;
      const height = network.blockHeight(latest) ?? integerOrNull(health?.height, info?.block_height, stats?.block_height);
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
        await requireLegacyArchiveProvenance(source, window.fetch.bind(window), signal);
        const [snapshotResult, checkpointAudit, inferenceResult, rewardsResult, maintenanceAudit] = await Promise.all([
          loadSnapshot(source, signal).then((value) => ({ ok: true, value }), (error) => ({ ok: false, error })),
          verifyRecoveryCheckpoint({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), signal }),
          optionalRequest(window.fetch.bind(window), source, "/inference/attestations?limit=20", { signal }),
          optionalRequest(window.fetch.bind(window), source, "/economics/rewards", { signal }),
          network.auditMaintenanceInterlock({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), signal }),
        ]);
        if (signal.aborted) return;
        if (!alternate && maintenanceAudit.state !== "healthy") {
          state.checkpointAudit = { state: "unknown", reason: maintenanceAudit.reason || "maintenance-interlock-not-healthy" };
          resetMetrics();
          renderFacts(source, null, null);
          renderRecovery(state.checkpointAudit);
          renderBlocks([], source);
          renderInference(null, source);
          renderRewards(null);
          text(elements.blocksStatus, "Paused by maintenance interlock");
          text(elements.inferenceStatus, "Paused by maintenance interlock");
          text(elements.rewardsStatus, "Paused by maintenance interlock");
          setBanner("error", "Network maintenance safety interlock active", `Canonical publication is paused: ${maintenanceAudit.reason || "fresh six-validator maintenance evidence is unavailable"}. Preserved alternate archives remain explicitly queryable.`);
          return;
        }
        if (!snapshotResult.ok) throw snapshotResult.error;
        state.checkpointAudit = checkpointAudit;
        const snapshot = snapshotResult.value;
        const height = reportedHeight(snapshot);
        if (height !== null) state.lastKnownHeight = height;
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
        text(elements.metricValidatorNote, Array.isArray(validatorRows) ? `${validatorRows.length} records returned` : "Validator records unavailable");
        renderFacts(source, snapshot, liveness);
        renderBlocks(snapshot.blocks, source);
        renderRecovery(checkpointAudit);
        renderInference(inferenceResult.ok ? inferenceResult.value : null, source);
        renderRewards(rewardsResult.ok ? rewardsResult.value : null);
        text(elements.lastRefreshed, new Date().toLocaleTimeString());
        if (checkpointAudit.state === "mismatch") setBanner("error", "Recovery checkpoint mismatch", "Exact H, H+1, chain identity, epoch, validator set, domain, or manifest differs. Canonical labels are paused.");
        else if (alternate) setBanner("degraded", "NON-CANONICAL source view", `${sourceDisplay(source)} is being queried explicitly and is not merged into canonical results.`);
        else if (liveness.state === "stalled") setBanner("degraded", "RPC reachable, chain appears stalled", `${sourceDisplay(source)} answered, but its newest block is stale.`);
        else if (checkpointAudit.state === "verified") setBanner("online", "Canonical recovery verified", `${sourceDisplay(source)} · exact checkpoint and ${checkpointAudit.replicas.length} v3 replica identities verified · liveness ${liveness.state}`);
        else setBanner("degraded", "Canonical evidence incomplete", `${sourceDisplay(source)} is reachable, but exact checkpoint evidence is unavailable. No result is labeled canonical.`);
      } catch (error) {
        if (!signal.aborted) {
          // Evidence panels are cleared with the metrics. Leaving the previous
          // block, inference, and reward renders on screen would present stale
          // rows - possibly from a different source - as current evidence while
          // the banner reports the selected source as unreachable.
          resetMetrics();
          renderFacts(source, null, null);
          renderRecovery(null);
          renderBlocks([], source);
          renderInference(null, source);
          renderRewards(null);
          text(elements.blocksStatus, "Unavailable");
          text(elements.inferenceStatus, "Unavailable");
          text(elements.rewardsStatus, "Unavailable");
          text(elements.lastRefreshed, "Refresh failed");
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

    // One note per permitted source that contributed nothing. A source with
    // no record is listed only where every source should hold one (the
    // replicas of a native request); across the recovery boundary a record
    // on one segment only is expected. A source whose record is unknown is
    // always listed.
    function appendSilentSources(silent, includeNoRecord) {
      for (const entry of silent) {
        if (entry.state === "no-record" && !includeNoRecord) continue;
        elements.inspectorContent.append(create("p", `inspector-note${entry.state === "unknown" ? " error" : ""}`, entry.text));
      }
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
      const controller = new AbortController();
      state.lookupController = controller;
      inspectorLoading("Block", `#${formatInteger(parsed.value)}`);
      try {
        const result = await queryBlock({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), height: parsed.value, sourceId: state.sourceId, signal: controller.signal, checkpointAudit: state.checkpointAudit });
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
        if (result.transactions) {
          const entries = transactionEntries(result.transactions);
          const section = create("section", "detail-section");
          const totalKnown = integerOrNull(result.transactions?.tx_count);
          section.append(create("h3", "", `Transactions in this block${entries.length ? ` (${formatInteger(entries.length)}${totalKnown !== null && totalKnown > entries.length ? ` of ${formatInteger(totalKnown)}` : ""})` : ""}`));
          if (entries.length) {
            const list = create("ul", "chip-list");
            for (const entry of entries) {
              const li = create("li");
              const button = create("button", "", network.formatHash(entry.hash, 10, 8));
              button.type = "button";
              button.title = `0x${entry.hash}`;
              button.addEventListener("click", () => navigate("tx", entry.hash));
              li.append(button);
              list.append(li);
            }
            section.append(list);
          } else {
            section.append(create("p", "", "This block's transaction index returned no linkable entries."));
          }
          elements.inspectorContent.append(section, rawSection("Transaction index response", result.transactions));
        }
      } catch (error) {
        if (!controller.signal.aborted) inspectorError("Block", "Block unavailable", error.message);
      }
    }

    async function inspectBlocksPage(rawStart) {
      const parsed = classifyLookup(rawStart, "block");
      if (parsed.error) return inspectorError("Blocks", "Invalid start height", parsed.error);
      const startHeight = Number(parsed.value);
      state.lookupController?.abort();
      const controller = new AbortController();
      state.lookupController = controller;
      inspectorLoading("Blocks", `Heights up to #${formatInteger(startHeight)}`);
      try {
        const result = await queryBlocksPage({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), startHeight, sourceId: state.sourceId, signal: controller.signal });
        setInspector("Blocks · paged history", `Heights #${formatInteger(result.from)} – #${formatInteger(result.to)}`);
        const nav = create("div", "history-nav");
        const olderButton = create("button", "text-button", "← Older");
        olderButton.type = "button";
        olderButton.disabled = result.from === 0;
        olderButton.addEventListener("click", () => navigate("blocks", String(Math.max(0, result.from - 1))));
        const newerButton = create("button", "text-button", "Newer →");
        newerButton.type = "button";
        newerButton.addEventListener("click", () => navigate("blocks", String(result.to + result.pageSize)));
        nav.append(olderButton, create("span", "quiet-pill", `${formatInteger(result.rows.length)} block(s) · page size ${result.pageSize}`), newerButton);
        elements.inspectorContent.append(nav);
        if (!result.rows.length) {
          elements.inspectorContent.append(create("p", "empty-cell", "No retained blocks were returned in this range."));
          return;
        }
        const tableWrap = create("div", "table-scroll");
        const table = create("table");
        const thead = create("thead");
        const headRow = create("tr");
        for (const label of ["Height", "Segment", "Transactions", "Block hash", "Source"]) headRow.append(create("th", "", label));
        thead.append(headRow);
        const tbody = create("tbody");
        for (const row of result.rows) {
          const height = network.blockHeight(row.block);
          const configured = height === null ? { canonical: false, segment: "unverified" } : state.resolver.classifyOccurrence(row.source.id, height);
          const canonical = network.gateCanonical(configured, state.checkpointAudit);
          const tr = create("tr");
          const heightCell = create("td");
          const button = create("button", "table-link", height === null ? "Unknown" : `#${formatInteger(height)}`);
          button.type = "button";
          if (height !== null) button.addEventListener("click", () => navigate("block", String(height)));
          heightCell.append(button);
          const segment = canonical.canonical ? canonical.segment.replaceAll("-", " ") : "non-canonical / unverified";
          tr.append(
            heightCell,
            create("td", canonical.canonical ? "truth-good" : "truth-warn", segment),
            create("td", "", formatInteger(txCount(row.block))),
            create("td", "", network.formatHash(network.blockHash(row.block))),
            create("td", "", sourceDisplay(row.source)),
          );
          tbody.append(tr);
        }
        table.append(thead, tbody);
        tableWrap.append(table);
        elements.inspectorContent.append(tableWrap);
      } catch (error) {
        if (!controller.signal.aborted) inspectorError("Blocks", "Blocks unavailable", error.message);
      }
    }

    function occurrenceCard(occurrence) {
      const { source, classification, provenance } = occurrence;
      const nativeEvidence = nativeReceipts.nativeEvidenceLabels(classification, provenance.canonical);
      const archiveOccurrences = Array.isArray(occurrence.occurrence?.occurrences)
        ? occurrence.occurrence.occurrences
        : [];
      const preservedHeight = occurrence.occurrence?.unique_occurrence === true && archiveOccurrences.length === 1
        ? integerOrNull(archiveOccurrences[0]?.block_height)
        : null;
      const card = create("article", `occurrence-card ${provenance.canonical ? "canonical" : "alternate"}`);
      const heading = create("div", "evidence-heading");
      heading.append(create("strong", "", sourceDisplay(source)), create("span", `status-pill ${provenance.canonical ? "online" : "degraded"}`, provenance.canonical ? "CANONICAL" : "NOT CANONICAL"));
      card.append(heading, detailGrid([
        ["Receipt", classification.receiptBacked ? classification.status : preservedHeight === null ? "Absent / unproven" : "Pruned · block inclusion preserved"],
        ["Category", classification.category],
        ...(nativeEvidence ? [["Native stage", nativeEvidence.stage], ["Mined outcome", nativeEvidence.outcome]] : []),
        ["Block", formatInteger(classification.height ?? preservedHeight)],
        ["Segment", provenance.segment?.replaceAll("-", " ")],
        ["Inference", nativeEvidence?.inference ?? (classification.inferenceConfirmed ? "Confirmed mined receipt" : "Not confirmed")],
        ["Reward", classification.rewardEarned ? "Earned · successful mined receipt" : "Not counted as earned"],
      ]));
      if (occurrence.full) card.append(rawSection("Transaction", occurrence.full));
      if (occurrence.receipt) card.append(rawSection("Receipt", occurrence.receipt));
      if (occurrence.rewardEvidence) card.append(rawSection("Community reward receipt", occurrence.rewardEvidence));
      if (occurrence.occurrence) card.append(rawSection("Preserved block occurrence", occurrence.occurrence));
      const nativeTransaction = nativeReceipts.describeTransaction(occurrence.full);
      if (nativeTransaction.native && nativeTransaction.requestId) {
        const requestLink = create("button", "table-link", "View native request receipts");
        requestLink.type = "button";
        requestLink.addEventListener("click", () => navigate("request", nativeTransaction.requestId));
        card.append(requestLink);
      }
      return card;
    }

    async function inspectTransaction(hash) {
      state.lookupController?.abort();
      const controller = new AbortController();
      state.lookupController = controller;
      inspectorLoading("Transaction / receipt", network.formatHash(hash, 14, 12));
      try {
        const result = await queryTransaction({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), hash, sourceId: state.sourceId, signal: controller.signal, checkpointAudit: state.checkpointAudit });
        const silent = result.failures.map((failure) => describeSilentSource(sourceDisplay(failure.source), failure.errors));
        if (!result.occurrences.length) {
          if (!result.plannedSources.length) return inspectorError("Transaction / receipt", "No source configured", "No permitted source is configured for this lookup, so nothing was asked.");
          if (silent.every((entry) => entry.state === "no-record")) return inspectorError("Transaction / receipt", "Transaction not found", `No record was returned by ${result.plannedSources.length} permitted source(s). Alternate forks were not searched unless explicitly selected.`);
          return inspectorError("Transaction / receipt", "Transaction status unknown", `Not every permitted source could be asked, so this transaction may still exist. ${silent.map((entry) => entry.text).join(" ")}`);
        }
        setInspector("Transaction / receipt", network.formatHash(hash, 14, 12));
        elements.inspectorContent.append(create("p", "inspector-note", "Each occurrence is classified independently. A transaction on an alternate source is never promoted to the canonical timeline."));
        appendSilentSources(silent, false);
        for (const occurrence of result.occurrences) elements.inspectorContent.append(occurrenceCard(occurrence));
      } catch (error) {
        if (!controller.signal.aborted) inspectorError("Transaction / receipt", "Lookup failed", error.message);
      }
    }

    // A bounded, paged, client-side view over the tx hashes a source already
    // returned in full: `/account/{address}/txs` has no offset/limit of its
    // own, so pagination here is display-only - it never issues another
    // request. Newest first, since the node appends hashes as it applies them.
    function buildHashPager(hashes) {
      const pageSize = 20;
      const wrap = create("div", "detail-section");
      wrap.append(create("h3", "", `Indexed transactions (${formatInteger(hashes.length)})`));
      if (!hashes.length) {
        wrap.append(create("p", "", "No indexed transactions for this address."));
        return wrap;
      }
      const newestFirst = hashes.slice().reverse();
      const pageCount = Math.max(1, Math.ceil(newestFirst.length / pageSize));
      let page = 0;
      const nav = create("div", "history-nav");
      const newerButton = create("button", "text-button", "Newer →");
      const status = create("span", "quiet-pill");
      const olderButton = create("button", "text-button", "← Older");
      newerButton.type = "button";
      olderButton.type = "button";
      const list = create("ul", "chip-list");
      function renderPage() {
        clear(list);
        const start = page * pageSize;
        for (const hash of newestFirst.slice(start, start + pageSize)) {
          const li = create("li");
          const button = create("button", "", network.formatHash(hash, 10, 8));
          button.type = "button";
          button.title = `0x${hash}`;
          button.addEventListener("click", () => navigate("tx", hash));
          li.append(button);
          list.append(li);
        }
        text(status, `Page ${page + 1} of ${pageCount} · newest first`);
        newerButton.disabled = page <= 0;
        olderButton.disabled = page >= pageCount - 1;
      }
      newerButton.addEventListener("click", () => { if (page > 0) { page -= 1; renderPage(); } });
      olderButton.addEventListener("click", () => { if (page < pageCount - 1) { page += 1; renderPage(); } });
      nav.append(newerButton, status, olderButton);
      renderPage();
      wrap.append(nav, list);
      return wrap;
    }

    async function inspectAddress(address) {
      state.lookupController?.abort();
      const controller = new AbortController();
      state.lookupController = controller;
      inspectorLoading("Address", network.formatHash(address, 14, 12));
      try {
        const result = await queryAddress({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), address, sourceId: state.sourceId, signal: controller.signal, checkpointAudit: state.checkpointAudit });
        const silent = result.failures.map((failure) => describeSilentSource(sourceDisplay(failure.source), failure.errors));
        if (!result.records.length) {
          if (!silent.length) return inspectorError("Address", "No source configured", "No permitted source is configured for this lookup, so nothing was asked.");
          if (silent.every((entry) => entry.state === "no-record")) return inspectorError("Address", "No account or history", `None of ${silent.length} permitted source(s) holds an account or indexed history for this address.`);
          return inspectorError("Address", "Address unavailable", `No account or indexed history was returned, and not every permitted source could be asked. ${silent.map((entry) => entry.text).join(" ")}`);
        }
        setInspector("Address · source-separated", network.formatHash(address, 14, 12));
        elements.inspectorContent.append(create("p", "inspector-note", "Balances and histories below remain source-scoped. They are not added together across the recovery boundary."));
        appendSilentSources(silent, false);
        for (const record of result.records) {
          const card = create("article", "occurrence-card");
          card.append(create("h3", "", sourceDisplay(record.source)), detailGrid([
            ["Balance (raw)", formatInteger(record.account?.balance)],
            ["Nonce", formatInteger(record.account?.nonce)],
          ]));
          if (record.historyState === "served") {
            card.append(buildHashPager(record.txHashes));
          } else if (record.historyState === "not-served") {
            card.append(create("p", "inspector-note", "Transaction history is not served by this node (no /account/{address}/txs route)."));
          } else if (record.historyState === "unreachable") {
            card.append(create("p", "inspector-note error", `Transaction history unreachable: ${record.historyError || "no response"}.`));
          } else {
            card.append(create("p", "inspector-note error", `Transaction history request failed: ${record.historyError || "unexpected response"}.`));
          }
          if (record.account) card.append(rawSection("Account response", record.account));
          if (record.history) card.append(rawSection("Address history", record.history));
          elements.inspectorContent.append(card);
        }
      } catch (error) {
        if (!controller.signal.aborted) inspectorError("Address", "Lookup failed", error.message);
      }
    }

    async function inspectNativeRequest(requestId) {
      state.lookupController?.abort();
      const controller = new AbortController();
      state.lookupController = controller;
      inspectorLoading("Native request", network.formatHash(requestId, 14, 12));
      try {
        const result = await queryNativeRequest({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), requestId, sourceId: state.sourceId, signal: controller.signal });
        const { comparison } = result;
        const silent = result.answers
          .filter((answer) => !answer.receipt)
          .map((answer) => describeSilentSource(answer.source.id, answer.error, "receipt"));
        if (!comparison.answered) {
          if (!result.plannedSources.length) return inspectorError("Native request", "No source configured", "No replica is configured for native requests, so nothing was asked.");
          if (silent.every((entry) => entry.state === "no-record")) return inspectorError("Native request", "Request not found", `None of ${result.plannedSources.length} permitted source(s) holds a receipt for this request id.`);
          return inspectorError("Native request", "Request status unknown", `No permitted source returned a receipt, and not every one could be asked. ${silent.map((entry) => entry.text).join(" ")}`);
        }
        setInspector("Native request · per-source receipts", network.formatHash(requestId, 14, 12));
        elements.inspectorContent.append(create(
          "p",
          `inspector-note ${comparison.agree ? "good" : "error"}`,
          comparison.agree
            ? `${comparison.answered} of ${result.plannedSources.length} permitted source(s) returned a receipt, and all of them record the same settlement.`
            : `Sources DISAGREE about this request (${comparison.answered} of ${result.plannedSources.length} returned a receipt). Nothing is averaged; each record is shown as served.`,
        ));
        appendSilentSources(silent, true);
        for (const row of comparison.rows) {
          const summary = row.summary;
          const answer = result.answers.find((item) => item.source.id === row.source);
          const expiry = nativeReceipts.classifyExpiry(summary, answer?.height ?? null);
          const card = create("article", "occurrence-card");
          const reconciled = summary.reconciled === null ? "Not settled yet" : summary.reconciled ? "Credits equal the reservation" : "Credits do NOT equal the reservation";
          card.append(create("h3", "", row.source), detailGrid([
            ["Chain status", summary.status],
            ["Settlement", reconciled],
            ["Price / reserved", `${formatInteger(summary.price)} / ${formatInteger(summary.reserved)}`],
            ["Credited", formatInteger(summary.credited)],
            ["Certificate votes", formatInteger(summary.votes)],
            ["Output hash", summary.outputHash ? `0x${summary.outputHash}` : "None", true],
            ["Admitted at", formatInteger(summary.admissionHeight)],
            ["Terminal at", formatInteger(summary.terminalHeight)],
            ["Expires at", summary.expiresAt === null ? "Unavailable from this node" : `Height ${formatInteger(summary.expiresAt)}`],
            ["Requester", summary.requester ? `0x${summary.requester}` : "Unavailable", true],
            ["Certified output (hex)", summary.outputCertified ? summary.outputHex : "Empty · not finalized yet", true],
            ["Display text — NOT certified", summary.outputText !== null ? summary.outputText : "Unavailable · display-only, never part of the certificate", true],
          ]));
          if (expiry.applicable && expiry.expired === true) {
            card.append(create("p", "inspector-note error", "Expired without a certificate: refundable by a refund transaction. No refund has happened yet - the chain only refunds once that transaction is mined."));
          } else if (expiry.applicable && expiry.expired === null) {
            card.append(create("p", "inspector-note", `${row.source}'s current height is unavailable, so expiry cannot be evaluated against it.`));
          }
          if (answer?.receipt) card.append(rawSection("Receipt", answer.receipt));
          elements.inspectorContent.append(card);
        }
      } catch (error) {
        if (!controller.signal.aborted) inspectorError("Native request", "Lookup failed", error.message);
      }
    }

    async function inspectAutoHash(hash) {
      const result = await queryTransaction({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), hash, sourceId: state.sourceId, checkpointAudit: state.checkpointAudit });
      if (result.occurrences.length) return inspectTransaction(hash);
      const native = await queryNativeRequest({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), requestId: hash, sourceId: state.sourceId });
      if (native.comparison.answered) return inspectNativeRequest(hash);
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
      else if (route.kind === "blocks") inspectBlocksPage(route.value);
      else if (route.kind === "tx") inspectTransaction(route.value);
      else if (route.kind === "address") inspectAddress(route.value);
      else if (route.kind === "request") inspectNativeRequest(route.value);
      else if (route.kind === "lookup") inspectAutoHash(route.value).catch((error) => inspectorError("Lookup", "Lookup failed", error.message));
      else inspectorError("Lookup", "Unsupported route", "Use a block, transaction, native request, or address search.");
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
    elements.blocksPageLink.addEventListener("click", () => navigate("blocks", String(state.lastKnownHeight ?? 0)));
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
    integerOrNull,
    formatExactInteger,
    describeSilentSource,
    reportedHeight,
    requestJson,
    requestLatestBlock,
    verifyRecoveryCheckpoint,
    queryBlock,
    queryBlocksPage,
    queryTransaction,
    queryAddress,
    queryNativeRequest,
    boot,
  });
});
