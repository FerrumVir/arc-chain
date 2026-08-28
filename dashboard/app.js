(function (root, factory) {
  const network = root?.ArcNetwork || (typeof require === "function" ? require("../shared/frontend/arc-network.js") : null);
  const api = factory(network);
  if (typeof module === "object" && module.exports) module.exports = api;
  if (root) root.ArcDashboard = api;
  if (typeof document !== "undefined") document.addEventListener("DOMContentLoaded", api.boot, { once: true });
})(typeof globalThis !== "undefined" ? globalThis : this, function (network) {
  "use strict";

  if (!network) throw new Error("ARC network resolver did not load");
  const REQUEST_TIMEOUT_MS = 8_000;
  const REFRESH_INTERVAL_MS = 30_000;

  class RpcError extends Error {
    constructor(message, status, sourceId) {
      super(message);
      this.name = "RpcError";
      this.status = status || 0;
      this.sourceId = sourceId || null;
    }
  }

  function numberOrNull(...values) {
    for (const value of values) {
      if (typeof value === "number" && Number.isFinite(value)) return value;
      if (typeof value === "string" && value.trim() && Number.isFinite(Number(value))) return Number(value);
    }
    return null;
  }

  function integerOrNull(...values) {
    const value = numberOrNull(...values);
    return value !== null && Number.isSafeInteger(value) && value >= 0 ? value : null;
  }

  function extractRows(payload) {
    if (Array.isArray(payload)) return payload;
    for (const key of ["attestations", "transactions", "records", "items", "activity"]) {
      if (Array.isArray(payload?.[key])) return payload[key];
      if (Array.isArray(payload?.data?.[key])) return payload.data[key];
    }
    return [];
  }

  async function requestJson(fetchImpl, source, path, options) {
    const settings = options || {};
    const controller = new AbortController();
    const abort = () => controller.abort(settings.signal?.reason || "caller-abort");
    const timer = setTimeout(() => controller.abort("timeout"), settings.timeoutMs || REQUEST_TIMEOUT_MS);
    if (settings.signal) {
      if (settings.signal.aborted) abort();
      else settings.signal.addEventListener("abort", abort, { once: true });
    }
    try {
      const response = await fetchImpl(network.buildRpcUrl(source, path), {
        method: "GET",
        cache: "no-store",
        headers: { Accept: "application/json" },
        signal: controller.signal,
      });
      if (!response.ok) throw new RpcError(`HTTP ${response.status}`, response.status, source.id);
      return await response.json();
    } catch (error) {
      if (error instanceof RpcError) throw error;
      if (controller.signal.aborted) throw new RpcError("Request timed out or was cancelled", 0, source.id);
      throw new RpcError(error?.message || "RPC request failed", 0, source.id);
    } finally {
      clearTimeout(timer);
      if (settings.signal) settings.signal.removeEventListener("abort", abort);
    }
  }

  async function optionalRequest(fetchImpl, source, path, options) {
    try { return { ok: true, value: await requestJson(fetchImpl, source, path, options) }; }
    catch (error) { return { ok: false, error }; }
  }

  function snapshotHeight(snapshot) {
    const values = [
      snapshot?.health?.height,
      snapshot?.health?.block_height,
      snapshot?.info?.height,
      snapshot?.info?.block_height,
      network.blockHeight(snapshot?.latest),
    ].map((value) => integerOrNull(value)).filter((value) => value !== null);
    return values.length ? Math.max(...values) : null;
  }

  async function collectSourceSnapshot(fetchImpl, source, options) {
    const [health, info, latest] = await Promise.all([
      optionalRequest(fetchImpl, source, "/health", options),
      optionalRequest(fetchImpl, source, "/info", options),
      optionalRequest(fetchImpl, source, "/block/latest", options),
    ]);
    const reachable = health.ok || info.ok || latest.ok;
    const snapshot = {
      source,
      reachable,
      health: health.ok ? health.value : null,
      info: info.ok ? info.value : null,
      latest: latest.ok ? latest.value : null,
      errors: [health, info, latest].filter((result) => !result.ok).map((result) => result.error?.message),
    };
    snapshot.height = snapshotHeight(snapshot);
    snapshot.liveness = reachable ? network.evaluateLiveness(snapshot.health, snapshot.latest, options?.nowMs) : { state: "unknown", ageSecs: null, basis: "RPC unreachable" };
    return snapshot;
  }

  async function collectFleetHealth(options) {
    const { resolver, fetchImpl, signal, nowMs } = options;
    const replicas = resolver.v3Replicas();
    if (!replicas.length) return { state: "unconfigured", samples: [], reachable: [], commonAudit: { state: "unknown", reason: "no-v3-replicas" }, drift: null, commonHeight: null };
    const samples = await Promise.all(replicas.map((source) => collectSourceSnapshot(fetchImpl, source, { signal, nowMs })));
    const reachable = samples.filter((sample) => sample.reachable);
    const heightSamples = reachable.filter((sample) => sample.height !== null);
    const commonHeight = heightSamples.length ? Math.min(...heightSamples.map((sample) => sample.height)) : null;
    const maxHeight = heightSamples.length ? Math.max(...heightSamples.map((sample) => sample.height)) : null;
    const drift = commonHeight === null ? null : maxHeight - commonHeight;
    let commitments = [];
    if (commonHeight !== null) {
      commitments = await Promise.all(heightSamples.map(async (sample) => {
        const result = await optionalRequest(fetchImpl, sample.source, `/block/${commonHeight}`, { signal });
        return result.ok
          ? { sourceId: sample.source.id, ok: true, height: network.blockHeight(result.value), blockHash: network.blockHash(result.value), stateRoot: network.stateRoot(result.value), block: result.value }
          : { sourceId: sample.source.id, ok: false, height: commonHeight, error: result.error?.message };
      }));
    }
    const commonAudit = network.auditCommonHeight(commitments);
    const current = samples.find((sample) => sample.source.id === resolver.config.checkpoint?.v3SourceId) || null;
    const anyStalled = reachable.some((sample) => sample.liveness.state === "stalled");
    let state = "unknown";
    if (!reachable.length) state = "offline";
    else if (commonAudit.state === "fork") state = "fork";
    else if (replicas.length < 2 || !current?.reachable || reachable.length < replicas.length || anyStalled || (drift !== null && drift > 3)) state = "degraded";
    else if (commonAudit.state === "consistent") state = current?.liveness.state === "advancing" ? "healthy" : "unknown";
    return { state, samples, reachable, current, commonHeight, drift, commonAudit, commitments, replicaCount: replicas.length };
  }

  function activeFleetPublicationError(config, fleet) {
    if (!config || !["recovered", "degraded"].includes(config.state)) return "configuration is not an active recovery state";
    if (fleet?.replicaCount !== 6 || fleet?.samples?.length !== 6) return "active recovery must declare exactly six validator replicas";
    if (fleet.reachable?.length !== fleet.replicaCount) return "all six validator health snapshots must be reachable";
    if (fleet.commitments?.length !== fleet.replicaCount || fleet.commitments.some((entry) => !entry?.ok)) return "all six validators must return a common-height commitment";
    if (fleet.commonAudit?.state !== "consistent" || !Number.isSafeInteger(fleet.commonHeight)) return "all six current commitments must agree at one comparable height";
    if (!fleet.current?.reachable) return "checkpoint-selected validator must be reachable";
    if (fleet.current?.liveness?.state !== "advancing") return "checkpoint-selected validator must prove advancing liveness";
    if (config.state === "recovered" && fleet.state !== "healthy") return "recovered publication requires a healthy fleet";
    if (config.state === "degraded" && !["healthy", "degraded"].includes(fleet.state)) return "degraded publication still requires a consistent live fleet";
    return null;
  }

  async function verifyRecoveryBoundary(options) {
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
    return network.auditRecoveryCheckpoint({
      config: resolver.config,
      legacyBlock: legacy.ok ? legacy.value : null,
      replicas: replicaEvidence,
    });
  }

  async function loadInferenceEvidence(options) {
    const { resolver, fetchImpl, signal, checkpointAudit, limit = 50 } = options;
    const source = resolver.currentSource();
    if (!source) return { source: null, rows: [], confirmed: [], excluded: 0, error: "canonical-v3-source-unavailable" };
    const result = await optionalRequest(fetchImpl, source, `/inference/attestations?limit=${limit}`, { signal });
    if (!result.ok) return { source, rows: [], confirmed: [], excluded: 0, error: result.error?.message };
    const rows = extractRows(result.value);
    const classified = rows.map((row) => {
      const receipt = network.classifyReceipt(row);
      const configured = receipt.height === null ? { canonical: false, segment: "unverified" } : resolver.classifyOccurrence(source.id, receipt.height);
      const provenance = network.gateCanonical(configured, checkpointAudit);
      return { row, receipt, provenance };
    });
    const confirmed = classified.filter((entry) => entry.receipt.inferenceConfirmed && entry.provenance.canonical);
    return { source, rows, confirmed, excluded: rows.length - confirmed.length, error: null };
  }

  function economicValue(payload, keys) {
    for (const key of keys) {
      const value = key.split(".").reduce((current, part) => current?.[part], payload);
      if (value !== undefined && value !== null && value !== "") return value;
    }
    return null;
  }

  function normalizeWorkerEarnings(earnings, economics) {
    const balance = numberOrNull(earnings?.onchain_balance_arc, earnings?.balance_arc);
    const confirmedReceipts = Array.isArray(earnings?.confirmed_receipts) ? earnings.confirmed_receipts : null;
    const confirmedReceiptCount = integerOrNull(earnings?.confirmed_receipt_count);
    const confirmedGross = numberOrNull(earnings?.confirmed_gross_earnings_arc);
    const receiptEvidenceConsistent = confirmedReceipts !== null
      && confirmedReceiptCount !== null
      && confirmedReceipts.length === confirmedReceiptCount
      && confirmedGross !== null
      && confirmedGross >= 0;
    const totalRewards = receiptEvidenceConsistent ? confirmedReceiptCount : null;
    const observedRateValue = numberOrNull(earnings?.attestations_per_day_observed);
    const observedRate = observedRateValue !== null && observedRateValue >= 0 ? observedRateValue : null;
    const rewardValue = numberOrNull(earnings?.reward_per_attestation_arc);
    const rewardPerAttestation = rewardValue !== null && rewardValue >= 0 ? rewardValue : null;
    const enabled = earnings?.community_rewards_v1_enabled;
    const active = earnings?.community_rewards_v1_protocol_active;
    const approvals = earnings?.community_rewards_v1_approval_collection_ready;
    const projectionValue = numberOrNull(earnings?.projected_daily_arc);
    const projectionReason = typeof earnings?.projected_daily_unavailable_reason === "string" && earnings.projected_daily_unavailable_reason.trim()
      ? earnings.projected_daily_unavailable_reason.trim()
      : null;
    const projectedPerDay = projectionValue !== null && projectionValue >= 0 && projectionReason === null ? projectionValue : null;
    let readiness = "unknown";
    if (enabled === false || active === false || approvals === false) readiness = "blocked";
    else if (enabled === true && active === true && approvals === true) readiness = "ready";
    return { balance, totalRewards, confirmedGross: receiptEvidenceConsistent ? confirmedGross : null, confirmedReceipts, receiptEvidenceConsistent, observedRate, rewardPerAttestation, projectedPerDay, projectionReason, enabled, active, approvals, readiness, raw: earnings, economics };
  }

  function validateWorkerId(raw) {
    const value = network.normalizeHex(String(raw ?? "").trim(), 32);
    return value
      ? { value }
      : { error: "Enter the 32-byte ARC worker address reported by your node (64 hex characters, with optional 0x)." };
  }

  async function loadWorkerEarnings(options) {
    const { resolver, fetchImpl, workerId, signal, checkpointAudit } = options;
    const validated = validateWorkerId(workerId);
    if (validated.error) throw new Error(validated.error);
    if (checkpointAudit?.state !== "verified") throw new Error("Canonical recovery checkpoint is not fully verified; earnings are paused.");
    const source = resolver.currentSource();
    if (!source) throw new Error("Canonical v3 source is unavailable");
    const [earnings, economics] = await Promise.all([
      optionalRequest(fetchImpl, source, `/worker/earnings/${encodeURIComponent(validated.value)}`, { signal }),
      optionalRequest(fetchImpl, source, "/economics/rewards", { signal }),
    ]);
    if (!earnings.ok) throw earnings.error;
    return { workerId: validated.value, source, ...normalizeWorkerEarnings(earnings.value, economics.ok ? economics.value : null) };
  }

  function validateHash(raw) {
    const value = network.normalizeHex(raw, 32);
    return value ? { value } : { error: "Enter a 32-byte transaction hash." };
  }

  async function lookupTransaction(options) {
    const { resolver, fetchImpl, hash, signal, checkpointAudit } = options;
    const validated = validateHash(hash);
    if (validated.error) throw new Error(validated.error);
    const plans = resolver.lookupSources();
    const attempts = await Promise.all(plans.map(async ({ source }) => {
      const [full, receipt] = await Promise.all([
        optionalRequest(fetchImpl, source, `/tx/${validated.value}/full`, { signal }),
        optionalRequest(fetchImpl, source, `/tx/${validated.value}`, { signal }),
      ]);
      if (!full.ok && !receipt.ok) return { source, found: false };
      const fullValue = full.ok ? full.value : null;
      const receiptValue = receipt.ok ? receipt.value : null;
      const classification = network.classifyReceipt({ tx: fullValue?.transaction ?? fullValue?.tx ?? fullValue, receipt: receiptValue?.receipt ?? receiptValue });
      const configured = classification.height === null ? { canonical: false, segment: "unverified", reason: "receipt-height-unavailable" } : resolver.classifyOccurrence(source.id, classification.height);
      const provenance = network.gateCanonical(configured, checkpointAudit);
      return { source, found: true, full: fullValue, receipt: receiptValue, classification, provenance };
    }));
    return { hash: validated.value, occurrences: attempts.filter((attempt) => attempt.found), searched: plans.map((plan) => plan.sourceId) };
  }

  function boot() {
    const $ = (id) => document.getElementById(id);
    const elements = {
      networkName: $("network-name"), navDot: $("nav-dot"), navState: $("nav-state"), refresh: $("refresh"), truthBanner: $("truth-banner"), truthTitle: $("truth-title"), truthDetail: $("truth-detail"), lastUpdated: $("last-updated"),
      continuityTitle: $("continuity-title"), boundaryBadge: $("boundary-badge"), continuityCopy: $("continuity-copy"), manifestValue: $("manifest-value"), chainId: $("chain-id"), legacyRange: $("legacy-range"), legacyAnchor: $("legacy-anchor"), boundaryBlock: $("boundary-block"), boundaryProof: $("boundary-proof"), v3Range: $("v3-range"), v3Source: $("v3-source"),
      fleetBadge: $("fleet-badge"), canonicalHeight: $("canonical-height"), heightNote: $("height-note"), blockAge: $("block-age"), livenessNote: $("liveness-note"), replicaCount: $("replica-count"), replicaNote: $("replica-note"), commonHeight: $("common-height"), forkNote: $("fork-note"), sourceGrid: $("source-grid"),
      inferenceBadge: $("inference-badge"), inferenceSummary: $("inference-summary"), inferenceBody: $("inference-body"),
      workerForm: $("worker-form"), workerId: $("worker-id"), workerError: $("worker-error"), workerBalance: $("worker-balance"), workerRewards: $("worker-rewards"), workerRate: $("worker-rate"), workerProjection: $("worker-projection"), workerReadiness: $("worker-readiness"),
      receiptForm: $("receipt-form"), receiptHash: $("receipt-hash"), receiptError: $("receipt-error"), receiptResult: $("receipt-result"),
    };
    const state = { config: null, resolver: null, checkpointAudit: { state: "unknown", reason: "not-audited" }, controller: null, timer: null };
    const text = (node, value) => { if (node) node.textContent = value == null ? "" : String(value); };
    const clear = (node) => { if (node) node.replaceChildren(); };
    const create = (tag, className, content) => { const node = document.createElement(tag); if (className) node.className = className; if (content !== undefined && content !== null) node.textContent = String(content); return node; };
    const formatInteger = (value) => value === null || value === undefined ? "—" : new Intl.NumberFormat().format(value);
    const formatArc = (value) => value === null || value === undefined ? "—" : `${new Intl.NumberFormat(undefined, { maximumFractionDigits: 4 }).format(value)} ARC`;

    function setBadge(node, kind, label) { node.className = `badge ${kind}`; text(node, label); }
    function setTruth(kind, title, detail) {
      elements.truthBanner.className = `truth-banner ${kind}`;
      text(elements.truthTitle, title); text(elements.truthDetail, detail);
      const dotKind = kind === "good" ? "good" : kind === "warn" ? "warn" : kind === "bad" ? "bad" : "unknown";
      elements.navDot.className = `dot ${dotKind}`;
      text(elements.navState, title);
    }

    function renderContinuity(boundary) {
      const checkpoint = state.config?.checkpoint;
      text(elements.chainId, state.config?.network.chainId ?? "Unavailable");
      if (!checkpoint) {
        text(elements.continuityTitle, "Recovery metadata unavailable");
        text(elements.continuityCopy, "Canonical claims are paused until a signed legacy checkpoint and a protocol-v3 continuation source are published.");
        setBadge(elements.boundaryBadge, "unknown", "NOT VERIFIED");
        return;
      }
      text(elements.continuityTitle, `Signed checkpoint #${formatInteger(checkpoint.height)} → protocol v3`);
      text(elements.continuityCopy, "The configured composite history preserves blocks 0…H at the signed checkpoint source and begins protocol v3 at exactly H+1.");
      text(elements.manifestValue, `0x${checkpoint.manifestHash}`);
      elements.manifestValue.title = `0x${checkpoint.manifestHash}`;
      text(elements.legacyRange, `0…${formatInteger(checkpoint.height)}`);
      text(elements.legacyAnchor, network.formatHash(checkpoint.blockHash, 10, 8));
      text(elements.boundaryBlock, `#${formatInteger(checkpoint.recoveryHeight)}`);
      text(elements.v3Range, `#${formatInteger(checkpoint.recoveryHeight + 1)}…latest`);
      text(elements.v3Source, state.resolver.currentSource()?.name ?? "Source unavailable");
      if (boundary?.state === "verified") { setBadge(elements.boundaryBadge, "good", "CHECKPOINT VERIFIED"); text(elements.boundaryProof, "Exact H, H+1, and every v3 identity match"); }
      else if (boundary?.state === "mismatch") { setBadge(elements.boundaryBadge, "bad", "CHECKPOINT MISMATCH"); text(elements.boundaryProof, "A signed commitment or replica identity differs"); }
      else { setBadge(elements.boundaryBadge, "warn", "PROOF UNAVAILABLE"); text(elements.boundaryProof, boundary?.reason || "Exact checkpoint evidence unavailable"); }
    }

    function endpointLabel(source) {
      if (source.baseUrl.startsWith("/")) return source.baseUrl;
      try { return new URL(source.baseUrl).host; } catch (_error) { return "Configured endpoint"; }
    }

    function renderSources(fleet) {
      clear(elements.sourceGrid);
      if (!state.config.sources.length) return elements.sourceGrid.append(create("div", "empty", "No sources configured. Publish the signed recovery inventory before claiming network health."));
      for (const source of state.config.sources) {
        const sample = fleet?.samples.find((item) => item.source.id === source.id);
        const canonicalLegacy = source.id === state.config.checkpoint?.legacySourceId;
        const canonicalV3 = source.id === state.config.checkpoint?.v3SourceId;
        const canonical = canonicalLegacy || canonicalV3;
        const card = create("article", `source-card ${canonical ? "canonical" : "alternate"}`);
        const top = create("div", "card-top");
        top.append(create("h4", "", source.name));
        let statusText = canonicalLegacy ? "SIGNED ARCHIVE" : canonicalV3 ? "V3 CANONICAL" : "NON-CANONICAL";
        let statusClass = canonical ? "good" : "warn";
        if (sample) {
          statusText = !sample.reachable ? "UNREACHABLE" : sample.liveness.state === "stalled" ? "STALE" : `${sample.liveness.state} · #${formatInteger(sample.height)}`;
          statusClass = !sample.reachable ? "bad" : sample.liveness.state === "advancing" ? "good" : "warn";
        }
        top.append(create("span", `source-state ${statusClass}`, statusText));
        card.append(top, create("p", "", `${source.kind.replaceAll("-", " ")}${source.region ? ` · ${source.region}` : ""}. ${canonical ? "Eligible for its configured canonical segment." : "Queryable only as an explicit alternate view."}`), create("code", "", endpointLabel(source)));
        elements.sourceGrid.append(card);
      }
    }

    function renderFleet(fleet) {
      const current = fleet.current;
      text(elements.canonicalHeight, formatInteger(current?.height));
      text(elements.heightNote, current ? `Current source · ${current.source.name}` : "No canonical v3 evidence");
      text(elements.blockAge, current?.liveness.ageSecs == null ? "—" : network.formatDuration(current.liveness.ageSecs));
      text(elements.livenessNote, current?.liveness ? `${current.liveness.state} · ${current.liveness.basis}` : "Liveness unknown");
      text(elements.replicaCount, fleet.replicaCount ? `${fleet.reachable.length}/${fleet.replicaCount}` : "—");
      text(elements.replicaNote, fleet.replicaCount ? `${fleet.replicaCount} configured in canonical replica group` : "No v3 replicas configured");
      text(elements.commonHeight, formatInteger(fleet.commonHeight));
      text(elements.forkNote, fleet.commonAudit.state === "consistent" ? "Block hash and state root match" : fleet.commonAudit.state === "fork" ? "HASH OR STATE ROOT DISAGREEMENT" : "Insufficient same-height evidence");
      const states = { healthy: ["good", "CONSISTENT"], degraded: ["warn", "DEGRADED"], fork: ["bad", "FORK CONFIRMED"], offline: ["bad", "OFFLINE"], unconfigured: ["unknown", "UNCONFIGURED"], unknown: ["unknown", "UNKNOWN"] };
      setBadge(elements.fleetBadge, ...(states[fleet.state] || states.unknown));
      renderSources(fleet);
    }

    function renderInference(result) {
      clear(elements.inferenceBody);
      if (!result.confirmed.length) {
        const row = create("tr"); const cell = create("td", "empty", result.error ? `Inference evidence unavailable: ${result.error}` : result.rows.length ? `${result.rows.length} observation(s) excluded because successful canonical receipts were absent.` : "No confirmed inference receipts returned."); cell.colSpan = 5; row.append(cell); elements.inferenceBody.append(row);
      }
      for (const entry of result.confirmed.slice(0, 20)) {
        const row = create("tr");
        const hashCell = create("td");
        const link = create("a", "tx-link", network.formatHash(entry.receipt.txHash));
        if (entry.receipt.txHash) link.href = `../explorer/#/tx/${entry.receipt.txHash}`;
        hashCell.append(link);
        row.append(create("td", "", formatInteger(entry.receipt.height)), hashCell, create("td", "", network.formatHash(entry.row.model_id ?? entry.row.model)), create("td", "", network.formatHash(entry.row.worker_id ?? entry.row.worker)), create("td", "receipt-ok", "MINED SUCCESS"));
        elements.inferenceBody.append(row);
      }
      text(elements.inferenceSummary, `${result.confirmed.length} confirmed · ${result.excluded} unproven or non-canonical excluded · source ${result.source?.name ?? "unavailable"}`);
      setBadge(elements.inferenceBadge, result.confirmed.length ? "good" : result.error ? "warn" : "unknown", result.confirmed.length ? `${result.confirmed.length} CONFIRMED` : "NO RECEIPTS");
    }

    function renderWorker(result) {
      text(elements.workerBalance, formatArc(result.balance));
      text(elements.workerRewards, formatInteger(result.totalRewards));
      text(elements.workerRate, result.observedRate === null ? "—" : `${result.observedRate}/day`);
      text(elements.workerProjection, formatArc(result.projectedPerDay));
      const mapping = {
        ready: ["good", "good", "Reward issuance path reports ready", `Protocol active, issuance enabled, and validator approval collection ready on ${result.source.name}. ${result.projectedPerDay === null ? `Projection unavailable: ${result.projectionReason || "the earnings endpoint supplied no authoritative projection"}.` : "The shown projection is the endpoint's explicit non-guaranteed projection."}`],
        blocked: ["bad", "bad", "Reward issuance is blocked", `At least one required readiness flag is false. No pending amount is presented as earned.`],
        unknown: ["warn", "warn", "Reward readiness is incomplete", "One or more protocol, issuance, or validator-approval fields were unavailable."],
      };
      const [className, dot, title, detail] = mapping[result.readiness];
      elements.workerReadiness.className = `readiness ${className}`;
      clear(elements.workerReadiness);
      elements.workerReadiness.append(create("span", `dot ${dot}`));
      const copy = create("div"); copy.append(create("strong", "", title), create("p", "", detail)); elements.workerReadiness.append(copy);
    }

    function receiptField(label, value) { const field = create("div", "receipt-field"); field.append(create("small", "", label), create("strong", "", value ?? "Unavailable")); return field; }
    function renderReceipt(result) {
      clear(elements.receiptResult);
      if (!result.occurrences.length) return elements.receiptResult.append(create("div", "empty", `Not found on ${result.searched.length} canonical source(s). Preserved forks were not searched.`));
      for (const occurrence of result.occurrences) {
        const card = create("article", `receipt-card ${occurrence.provenance.canonical ? "canonical" : "alternate"}`);
        const header = create("header"); header.append(create("h3", "", occurrence.source.name)); setBadge(header.appendChild(create("span", "badge")), occurrence.provenance.canonical ? "good" : "warn", occurrence.provenance.canonical ? "CANONICAL" : "NOT CANONICAL"); card.append(header);
        const grid = create("div", "receipt-grid");
        grid.append(
          receiptField("RECEIPT", occurrence.classification.receiptBacked ? occurrence.classification.status : "Absent / unproven"),
          receiptField("BLOCK", formatInteger(occurrence.classification.height)),
          receiptField("SEGMENT", occurrence.provenance.segment?.replaceAll("-", " ")),
          receiptField("CATEGORY", occurrence.classification.category),
          receiptField("INFERENCE", occurrence.classification.inferenceConfirmed ? "Confirmed" : "Not confirmed"),
          receiptField("REWARD", occurrence.classification.rewardEarned ? "Earned · mined success" : "Not counted as earned"),
        );
        card.append(grid);
        const details = create("details"); details.append(create("summary", "", "Raw source response"), create("pre", "", JSON.stringify({ transaction: occurrence.full, receipt: occurrence.receipt }, null, 2))); card.append(details);
        elements.receiptResult.append(card);
      }
    }

    async function refresh() {
      if (!state.resolver) return;
      state.controller?.abort();
      state.controller = new AbortController();
      const signal = state.controller.signal;
      elements.refresh.classList.add("spinning"); elements.refresh.disabled = true;
      setTruth("loading", "Auditing configured canonical sources…", "Checking H+1 parent linkage, replica freshness, and one common-height commitment.");
      try {
        const [fleet, boundary] = await Promise.all([
          collectFleetHealth({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), signal }),
          verifyRecoveryBoundary({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), signal }),
        ]);
        if (signal.aborted) return;
        state.checkpointAudit = boundary;
        const inference = await loadInferenceEvidence({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), signal, checkpointAudit: boundary });
        if (signal.aborted) return;
        renderContinuity(boundary); renderFleet(fleet); renderInference(inference);
        if (boundary.state === "mismatch") setTruth("bad", "Recovery checkpoint mismatch", "Exact H, H+1, chain identity, recovery epoch, validator set, domain, or manifest differs on a configured replica. Canonical and earnings claims are paused.");
        else if (fleet.state === "fork") setTruth("bad", "COMMON-HEIGHT FORK CONFIRMED", `Configured v3 replicas disagree at #${formatInteger(fleet.commonHeight)}. Stop canonical and reward claims until recovery selection is resolved.`);
        else if (fleet.state === "healthy" && boundary.state === "verified") setTruth("good", "Canonical v3 continuation verified", `Signed H, exact H+1, and chain/recovery identity match on all ${boundary.replicas.length} configured v3 replica(s); ${fleet.reachable.length}/${fleet.replicaCount} are healthy.`);
        else if (fleet.state === "unconfigured") setTruth("warn", "Canonical recovery is not configured", state.config.notices[0] || "Publish signed checkpoint and endpoint metadata before using this console.");
        else setTruth("warn", "Canonical evidence is incomplete", `Fleet ${fleet.state}; boundary ${boundary.state}. Missing evidence is not treated as success.`);
        text(elements.lastUpdated, `Updated ${new Date().toLocaleTimeString()}`);
      } catch (error) {
        if (!signal.aborted) { renderContinuity({ state: "unknown", reason: error.message }); renderSources(null); setTruth("bad", "Dashboard audit failed", error.message); }
      } finally {
        elements.refresh.classList.remove("spinning"); elements.refresh.disabled = false;
      }
    }

    elements.refresh.addEventListener("click", refresh);
    elements.workerForm.addEventListener("submit", async (event) => {
      event.preventDefault(); text(elements.workerError, "");
      const button = elements.workerForm.querySelector("button"); button.disabled = true;
      try { renderWorker(await loadWorkerEarnings({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), workerId: elements.workerId.value, checkpointAudit: state.checkpointAudit })); }
      catch (error) { text(elements.workerError, error.message); }
      finally { button.disabled = false; }
    });
    elements.receiptForm.addEventListener("submit", async (event) => {
      event.preventDefault(); text(elements.receiptError, "");
      const button = elements.receiptForm.querySelector("button"); button.disabled = true; clear(elements.receiptResult); elements.receiptResult.append(create("div", "empty", "Searching canonical segments…"));
      try { renderReceipt(await lookupTransaction({ resolver: state.resolver, fetchImpl: window.fetch.bind(window), hash: elements.receiptHash.value, checkpointAudit: state.checkpointAudit })); }
      catch (error) { text(elements.receiptError, error.message); clear(elements.receiptResult); elements.receiptResult.append(create("div", "empty", "Receipt lookup did not run.")); }
      finally { button.disabled = false; }
    });
    document.addEventListener("visibilitychange", () => { if (!document.hidden) refresh(); });

    (async () => {
      try {
        const meta = document.querySelector('meta[name="arc-network-config"]');
        state.config = await network.loadConfig({ injected: window.__ARC_NETWORK_CONFIG__, fetchImpl: window.fetch.bind(window), url: meta?.content || "../shared/frontend/arc-network.json" });
        state.resolver = network.createCanonicalResolver(state.config);
        text(elements.networkName, state.config.network.name);
        text(elements.chainId, state.config.network.chainId);
        renderContinuity({ state: "unknown" }); renderSources(null);
        await refresh();
        state.timer = window.setInterval(() => { if (!document.hidden) refresh(); }, REFRESH_INTERVAL_MS);
      } catch (error) {
        setTruth("bad", "Dashboard configuration rejected", error.message);
        renderContinuity({ state: "unknown", reason: error.message });
      }
    })();
  }

  return Object.freeze({
    RpcError,
    numberOrNull,
    extractRows,
    requestJson,
    collectSourceSnapshot,
    collectFleetHealth,
    activeFleetPublicationError,
    verifyRecoveryBoundary,
    loadInferenceEvidence,
    normalizeWorkerEarnings,
    validateWorkerId,
    loadWorkerEarnings,
    validateHash,
    lookupTransaction,
    boot,
  });
});
