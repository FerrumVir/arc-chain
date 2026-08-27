(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  if (root) root.ArcNetwork = api;
})(typeof globalThis !== "undefined" ? globalThis : this, function () {
  "use strict";

  const CONFIG_SCHEMA = "arc.frontend.network.v1";
  const SOURCE_KINDS = new Set(["legacy-canonical", "v3", "legacy-fork", "diagnostic"]);
  const NETWORK_STATES = new Set(["maintenance", "recovered", "degraded"]);
  const SUCCESS_STATES = new Set(["success", "succeeded", "mined", "included", "confirmed", "ok"]);
  const FAILURE_STATES = new Set(["failed", "failure", "rejected", "reverted", "invalid", "dropped"]);

  class ConfigurationError extends Error {
    constructor(message) {
      super(message);
      this.name = "ConfigurationError";
    }
  }

  function isObject(value) {
    return value !== null && typeof value === "object" && !Array.isArray(value);
  }

  function integer(value) {
    if (typeof value === "number" && Number.isSafeInteger(value)) return value;
    if (typeof value === "string" && /^\d+$/.test(value.trim())) {
      const parsed = Number(value);
      return Number.isSafeInteger(parsed) ? parsed : null;
    }
    return null;
  }

  function nonEmpty(value, field) {
    if (typeof value !== "string" || !value.trim()) throw new ConfigurationError(`${field} is required`);
    return value.trim();
  }

  function normalizeHex(value, bytes) {
    if (typeof value !== "string") return null;
    const normalized = value.trim().replace(/^0x/i, "").toLowerCase();
    if (!/^[0-9a-f]+$/.test(normalized)) return null;
    if (bytes && normalized.length !== bytes * 2) return null;
    return normalized;
  }

  function validateEndpoint(raw, field) {
    const value = nonEmpty(raw, field).replace(/\/+$/, "");
    if (value.startsWith("/")) {
      if (value.startsWith("//")) throw new ConfigurationError(`${field} must not be protocol-relative`);
      return value || "/";
    }

    let url;
    try {
      url = new URL(value);
    } catch (_error) {
      throw new ConfigurationError(`${field} must be an HTTPS URL, a loopback HTTP URL, or a same-origin absolute path`);
    }
    const loopback = url.hostname === "localhost" || url.hostname === "127.0.0.1" || url.hostname === "[::1]";
    if (url.protocol !== "https:" && !(url.protocol === "http:" && loopback)) {
      throw new ConfigurationError(`${field} refuses clear-text non-loopback RPC endpoints`);
    }
    if (url.username || url.password || url.search || url.hash) {
      throw new ConfigurationError(`${field} must not contain credentials, a query, or a fragment`);
    }
    return url.toString().replace(/\/+$/, "");
  }

  function normalizeSource(raw, index) {
    if (!isObject(raw)) throw new ConfigurationError(`sources[${index}] must be an object`);
    const id = nonEmpty(raw.id, `sources[${index}].id`);
    if (!/^[a-z0-9][a-z0-9_-]{0,63}$/i.test(id)) {
      throw new ConfigurationError(`sources[${index}].id contains unsupported characters`);
    }
    const kind = nonEmpty(raw.kind, `sources[${index}].kind`);
    if (!SOURCE_KINDS.has(kind)) throw new ConfigurationError(`sources[${index}].kind is unsupported`);
    return Object.freeze({
      id,
      name: typeof raw.name === "string" && raw.name.trim() ? raw.name.trim() : id,
      region: typeof raw.region === "string" ? raw.region.trim() : "",
      kind,
      baseUrl: validateEndpoint(raw.baseUrl, `sources[${index}].baseUrl`),
      enabled: raw.enabled !== false,
      replicaGroup: typeof raw.replicaGroup === "string" ? raw.replicaGroup.trim() : "",
      description: typeof raw.description === "string" ? raw.description.trim() : "",
    });
  }

  function normalizeCheckpoint(raw, sourcesById) {
    if (raw === null || raw === undefined) return null;
    if (!isObject(raw)) throw new ConfigurationError("checkpoint must be an object or null");
    const height = integer(raw.height);
    const recoveryHeight = integer(raw.recoveryHeight);
    if (height === null || height < 0) throw new ConfigurationError("checkpoint.height must be a non-negative integer");
    if (recoveryHeight !== height + 1) throw new ConfigurationError("checkpoint.recoveryHeight must equal checkpoint.height + 1");
    const blockHash = normalizeHex(raw.blockHash, 32);
    const stateRoot = normalizeHex(raw.stateRoot, 32);
    const manifestHash = normalizeHex(raw.manifestHash, 32);
    if (!blockHash) throw new ConfigurationError("checkpoint.blockHash must be a 32-byte hex value");
    if (!stateRoot) throw new ConfigurationError("checkpoint.stateRoot must be a 32-byte hex value");
    if (!manifestHash) throw new ConfigurationError("checkpoint.manifestHash must be a 32-byte hex value");
    const legacySourceId = nonEmpty(raw.legacySourceId, "checkpoint.legacySourceId");
    const v3SourceId = nonEmpty(raw.v3SourceId, "checkpoint.v3SourceId");
    const legacy = sourcesById.get(legacySourceId);
    const v3 = sourcesById.get(v3SourceId);
    const sharedRecoveredSource = legacySourceId === v3SourceId && legacy?.kind === "v3";
    if (!legacy || (legacy.kind !== "legacy-canonical" && !sharedRecoveredSource)) {
      throw new ConfigurationError("checkpoint.legacySourceId must reference a legacy-canonical source or the retained-history v3 source");
    }
    if (!v3 || v3.kind !== "v3") throw new ConfigurationError("checkpoint.v3SourceId must reference a v3 source");
    if (!legacy.enabled || !v3.enabled) throw new ConfigurationError("checkpoint sources must be enabled");
    const boundaryBlockHash = normalizeHex(raw.boundaryBlockHash, 32);
    const boundaryStateRoot = normalizeHex(raw.boundaryStateRoot, 32);
    const recoveryDomain = normalizeHex(raw.recoveryDomain, 32);
    const recoveryEpoch = integer(raw.recoveryEpoch);
    const validatorSetId = integer(raw.validatorSetId);
    const protocolVersion = typeof raw.protocolVersion === "string" ? raw.protocolVersion.trim() : "";
    if (!boundaryBlockHash) throw new ConfigurationError("checkpoint.boundaryBlockHash must be a 32-byte hex value");
    if (!boundaryStateRoot) throw new ConfigurationError("checkpoint.boundaryStateRoot must be a 32-byte hex value");
    if (!recoveryDomain) throw new ConfigurationError("checkpoint.recoveryDomain must be a 32-byte hex value");
    if (recoveryEpoch === null || recoveryEpoch < 1) throw new ConfigurationError("checkpoint.recoveryEpoch must be a positive integer");
    if (validatorSetId === null || validatorSetId < 1) throw new ConfigurationError("checkpoint.validatorSetId must be a positive integer");
    if (!/^3\.\d+\.\d+$/.test(protocolVersion)) throw new ConfigurationError("checkpoint.protocolVersion must be a protocol-v3 semantic version");
    return Object.freeze({
      height,
      recoveryHeight,
      blockHash,
      stateRoot,
      manifestHash,
      boundaryBlockHash,
      boundaryStateRoot,
      recoveryDomain,
      recoveryEpoch,
      validatorSetId,
      protocolVersion,
      legacySourceId,
      v3SourceId,
      createdAt: typeof raw.createdAt === "string" ? raw.createdAt : null,
    });
  }

  function normalizeConfig(raw) {
    if (!isObject(raw)) throw new ConfigurationError("network configuration must be an object");
    if (raw.schema !== CONFIG_SCHEMA) throw new ConfigurationError(`configuration schema must be ${CONFIG_SCHEMA}`);
    const state = raw.state || "maintenance";
    if (!NETWORK_STATES.has(state)) throw new ConfigurationError("configuration state is unsupported");
    if (!isObject(raw.network)) throw new ConfigurationError("network metadata is required");
    const sources = Object.freeze((Array.isArray(raw.sources) ? raw.sources : []).map(normalizeSource));
    const sourcesById = new Map();
    const endpoints = new Set();
    for (const source of sources) {
      if (sourcesById.has(source.id)) throw new ConfigurationError(`duplicate source id: ${source.id}`);
      if (endpoints.has(source.baseUrl)) throw new ConfigurationError(`duplicate source endpoint: ${source.baseUrl}`);
      sourcesById.set(source.id, source);
      endpoints.add(source.baseUrl);
    }
    const checkpoint = normalizeCheckpoint(raw.checkpoint, sourcesById);
    if ((state === "recovered" || state === "degraded") && !checkpoint) {
      throw new ConfigurationError(`${state} configuration requires a recovery checkpoint`);
    }
    const updatedAt = typeof raw.updatedAt === "string" && Number.isFinite(Date.parse(raw.updatedAt))
      ? raw.updatedAt
      : null;
    return Object.freeze({
      schema: CONFIG_SCHEMA,
      state,
      updatedAt,
      network: Object.freeze({
        name: nonEmpty(raw.network.name, "network.name"),
        chainId: nonEmpty(raw.network.chainId, "network.chainId"),
      }),
      checkpoint,
      sources,
      sourcesById,
      services: Object.freeze(isObject(raw.services) ? { ...raw.services } : {}),
      notices: Object.freeze(Array.isArray(raw.notices) ? raw.notices.filter((item) => typeof item === "string") : []),
    });
  }

  async function loadConfig(options) {
    const settings = options || {};
    if (settings.injected) return normalizeConfig(settings.injected);
    if (typeof settings.fetchImpl !== "function") throw new ConfigurationError("a fetch implementation is required");
    const url = settings.url || "./arc-network.json";
    const response = await settings.fetchImpl(url, { cache: "no-store", credentials: "same-origin" });
    if (!response || !response.ok) throw new ConfigurationError(`network configuration request failed (${response?.status ?? "unknown"})`);
    return normalizeConfig(await response.json());
  }

  function buildRpcUrl(source, path) {
    if (!source || typeof source.baseUrl !== "string") throw new Error("A configured ARC source is required");
    if (typeof path !== "string" || !path.startsWith("/") || path.startsWith("//")) {
      throw new Error("RPC paths must be source-relative absolute paths");
    }
    if (/^\/\s*https?:/i.test(path)) throw new Error("RPC paths must be source-relative absolute paths");
    return source.baseUrl === "/" ? path : `${source.baseUrl}${path}`;
  }

  function createCanonicalResolver(input) {
    const config = input && input.sourcesById instanceof Map ? input : normalizeConfig(input);

    function source(id) {
      const found = config.sourcesById.get(id);
      if (!found || !found.enabled) return null;
      return found;
    }

    function canonicalRoute(heightInput) {
      const height = integer(heightInput);
      if (height === null || height < 0) return { ok: false, reason: "invalid-height", canonical: false };
      const checkpoint = config.checkpoint;
      if (!checkpoint) return { ok: false, reason: "recovery-checkpoint-unavailable", height, canonical: false };
      const legacy = height <= checkpoint.height;
      const selected = source(legacy ? checkpoint.legacySourceId : checkpoint.v3SourceId);
      if (!selected) return { ok: false, reason: "canonical-source-unavailable", height, canonical: false };
      let segment = "v3-continuation";
      if (height < checkpoint.height) segment = "legacy-history";
      else if (height === checkpoint.height) segment = "signed-checkpoint";
      else if (height === checkpoint.recoveryHeight) segment = "recovery-boundary";
      return { ok: true, height, source: selected, sourceId: selected.id, canonical: true, segment };
    }

    function routeBlock(heightInput, options) {
      const canonical = canonicalRoute(heightInput);
      const selectedId = options && options.sourceId && options.sourceId !== "canonical" ? options.sourceId : null;
      if (!selectedId) return canonical;
      const selected = source(selectedId);
      if (!selected) return { ok: false, reason: "selected-source-unavailable", height: integer(heightInput), canonical: false };
      const isCanonical = canonical.ok && canonical.sourceId === selected.id;
      return {
        ok: true,
        height: integer(heightInput),
        source: selected,
        sourceId: selected.id,
        canonical: isCanonical,
        segment: isCanonical ? canonical.segment : "alternate-source",
        expectedCanonicalSourceId: canonical.ok ? canonical.sourceId : null,
        warning: isCanonical ? null : "Explicit alternate-source result; not part of the configured canonical timeline.",
      };
    }

    function lookupSources(options) {
      const selectedId = options && options.sourceId && options.sourceId !== "canonical" ? options.sourceId : null;
      if (selectedId) {
        const selected = source(selectedId);
        return selected ? [{ source: selected, sourceId: selected.id, selected: true }] : [];
      }
      if (!config.checkpoint) return [];
      const ids = [config.checkpoint.v3SourceId, config.checkpoint.legacySourceId];
      return [...new Set(ids)].map(source).filter(Boolean).map((item) => ({ source: item, sourceId: item.id, selected: false }));
    }

    function classifyOccurrence(sourceId, heightInput) {
      const route = canonicalRoute(heightInput);
      if (!route.ok) return { canonical: false, segment: "unverified", reason: route.reason };
      if (route.sourceId !== sourceId) {
        return { canonical: false, segment: "alternate-source", expectedCanonicalSourceId: route.sourceId };
      }
      return { canonical: true, segment: route.segment, expectedCanonicalSourceId: route.sourceId };
    }

    function currentSource() {
      return config.checkpoint ? source(config.checkpoint.v3SourceId) : null;
    }

    function v3Replicas() {
      const canonical = currentSource();
      if (!canonical) return [];
      const group = canonical.replicaGroup;
      return config.sources.filter((item) => item.enabled && item.kind === "v3" && (!group || item.replicaGroup === group));
    }

    return Object.freeze({ config, source, canonicalRoute, routeBlock, lookupSources, classifyOccurrence, currentSource, v3Replicas });
  }

  function blockHeader(block) {
    if (!isObject(block)) return {};
    if (isObject(block.header)) return block.header;
    if (isObject(block.block) && isObject(block.block.header)) return block.block.header;
    return block;
  }

  function blockHeight(block) {
    const header = blockHeader(block);
    return integer(header.height ?? header.block_height ?? block?.height ?? block?.block_height);
  }

  function blockHash(block) {
    const header = blockHeader(block);
    return normalizeHex(header.hash ?? header.block_hash ?? block?.hash ?? block?.block_hash, 32);
  }

  function stateRoot(block) {
    const header = blockHeader(block);
    return normalizeHex(header.state_root ?? header.stateRoot ?? block?.state_root ?? block?.stateRoot, 32);
  }

  function parentHash(block) {
    const header = blockHeader(block);
    return normalizeHex(header.parent_hash ?? header.parentHash ?? block?.parent_hash ?? block?.parentHash, 32);
  }

  function evaluateLiveness(health, latestBlock, nowMs, staleAfterSecs) {
    const current = Number.isFinite(nowMs) ? nowMs : Date.now();
    const threshold = Number.isFinite(staleAfterSecs) ? staleAfterSecs : 1800;
    const explicitAge = Number(health?.last_block_age_secs);
    const explicit = typeof health?.chain_advancing === "boolean";
    if (explicit && Number.isFinite(explicitAge) && explicitAge >= 0) {
      return {
        state: health.chain_advancing ? "advancing" : "stalled",
        ageSecs: Math.round(explicitAge),
        basis: "RPC /health chain-liveness fields",
      };
    }
    const header = blockHeader(latestBlock);
    const rawTimestamp = Number(header.timestamp ?? latestBlock?.timestamp);
    if (!Number.isFinite(rawTimestamp) || rawTimestamp <= 0) return { state: "unknown", ageSecs: null, basis: "no valid block timestamp" };
    const timestampMs = rawTimestamp < 10_000_000_000 ? rawTimestamp * 1000 : rawTimestamp;
    const ageSecs = Math.round((current - timestampMs) / 1000);
    if (ageSecs < -60) return { state: "unknown", ageSecs, basis: "block timestamp is in the future" };
    return {
      state: ageSecs <= threshold ? "advancing" : "stalled",
      ageSecs: Math.max(0, ageSecs),
      basis: `latest retained block timestamp (${threshold}s freshness window)`,
    };
  }

  function auditCommonHeight(samples) {
    const valid = (Array.isArray(samples) ? samples : []).filter((sample) => {
      return sample && sample.ok !== false && integer(sample.height) !== null && normalizeHex(sample.blockHash, 32) && normalizeHex(sample.stateRoot, 32);
    }).map((sample) => ({
      sourceId: sample.sourceId,
      height: integer(sample.height),
      blockHash: normalizeHex(sample.blockHash, 32),
      stateRoot: normalizeHex(sample.stateRoot, 32),
    }));
    if (valid.length < 2) return { state: "unknown", samples: valid, reason: "fewer-than-two-comparable-replicas" };
    const heights = new Set(valid.map((sample) => sample.height));
    if (heights.size !== 1) return { state: "unknown", samples: valid, reason: "samples-are-not-at-one-height" };
    const commitments = new Set(valid.map((sample) => `${sample.blockHash}:${sample.stateRoot}`));
    return {
      state: commitments.size === 1 ? "consistent" : "fork",
      height: valid[0].height,
      samples: valid,
      commitments: commitments.size,
      reason: commitments.size === 1 ? "matching-hash-and-state-root" : "hash-or-state-root-disagreement",
    };
  }

  function unwrapTransaction(payload) {
    if (!isObject(payload)) return {};
    return payload.transaction || payload.tx || payload.data?.transaction || payload.data?.tx || payload;
  }

  function unwrapReceipt(payload) {
    if (!isObject(payload)) return null;
    return payload.receipt || payload.transaction?.receipt || payload.tx?.receipt || payload.data?.receipt || null;
  }

  function classifyReceipt(payload) {
    const tx = unwrapTransaction(payload);
    const receipt = unwrapReceipt(payload);
    const rawType = tx.type ?? tx.tx_type ?? receipt?.type ?? receipt?.tx_type ?? "unknown";
    const normalizedType = String(rawType).trim().toLowerCase();
    const typeCode = typeof rawType === "number" ? rawType : (/^0x[0-9a-f]+$/i.test(String(rawType)) ? Number.parseInt(String(rawType), 16) : null);
    const category = typeCode === 0x25 || /community.*reward|inference.*reward/.test(normalizedType)
      ? "reward"
      : typeCode === 0x16 || /inference.*attestation|inference/.test(normalizedType)
        ? "inference"
        : "transaction";
    const rawStatus = receipt?.status ?? receipt?.receipt_status ?? payload?.receipt_status ?? payload?.status ?? null;
    const status = typeof rawStatus === "string" ? rawStatus.trim().toLowerCase() : rawStatus === true ? "success" : rawStatus === false ? "failed" : "unknown";
    const explicitProvenance = payload?.schema === "arc.inference.activity.v1"
      && payload?.source === "chain_receipt"
      && payload?.mined === true;
    const receiptBacked = (isObject(receipt) && (rawStatus !== null || receipt.block_height != null || receipt.block_hash != null || receipt.mined === true))
      || explicitProvenance;
    const explicitSuccess = receipt?.success ?? (explicitProvenance ? payload?.success : undefined);
    const success = receiptBacked && explicitSuccess !== false && (explicitSuccess === true || SUCCESS_STATES.has(status));
    const failed = receiptBacked && (explicitSuccess === false || FAILURE_STATES.has(status));
    const height = integer(receipt?.block_height ?? receipt?.height ?? tx?.block_height ?? tx?.height ?? payload?.block_height ?? payload?.height);
    const txHash = normalizeHex(tx?.hash ?? tx?.tx_hash ?? payload?.hash ?? payload?.tx_hash, 32);
    return Object.freeze({
      category,
      type: rawType,
      status,
      receiptBacked,
      success,
      failed,
      mined: receiptBacked && height !== null,
      height,
      txHash,
      rewardEarned: category === "reward" && receiptBacked && success && height !== null,
      inferenceConfirmed: category === "inference" && receiptBacked && success && height !== null,
    });
  }

  function checkpointVerification(block, checkpoint) {
    if (!checkpoint) return { state: "unknown", reason: "checkpoint-unavailable" };
    const actual = {
      height: blockHeight(block),
      blockHash: blockHash(block),
      stateRoot: stateRoot(block),
    };
    const missing = Object.entries(actual).filter(([, value]) => value === null).map(([key]) => key);
    if (missing.length) return { state: "unknown", reason: `checkpoint-${missing.join("-")}-unavailable`, ...actual };
    const mismatches = [];
    if (actual.height !== checkpoint.height) mismatches.push("height");
    if (actual.blockHash !== checkpoint.blockHash) mismatches.push("blockHash");
    if (actual.stateRoot !== checkpoint.stateRoot) mismatches.push("stateRoot");
    return mismatches.length
      ? { state: "mismatch", reason: "checkpoint-commitment-mismatch", mismatches, ...actual }
      : { state: "verified", ...actual };
  }

  function boundaryVerification(block, checkpoint) {
    if (!checkpoint) return { state: "unknown", reason: "checkpoint-unavailable" };
    const height = blockHeight(block);
    if (height !== checkpoint.recoveryHeight) return { state: "not-boundary", height };
    const parent = parentHash(block);
    const hash = blockHash(block);
    const root = stateRoot(block);
    const missing = [!parent && "parentHash", !hash && "blockHash", !root && "stateRoot"].filter(Boolean);
    if (missing.length) return { state: "unknown", height, reason: `boundary-${missing.join("-")}-unavailable` };
    const mismatches = [];
    if (parent !== checkpoint.blockHash) mismatches.push("parentHash");
    if (hash !== checkpoint.boundaryBlockHash) mismatches.push("blockHash");
    if (root !== checkpoint.boundaryStateRoot) mismatches.push("stateRoot");
    return mismatches.length
      ? {
          state: "mismatch",
          height,
          parentHash: parent,
          blockHash: hash,
          stateRoot: root,
          mismatches,
          expectedParentHash: checkpoint.blockHash,
          expectedBlockHash: checkpoint.boundaryBlockHash,
          expectedStateRoot: checkpoint.boundaryStateRoot,
        }
      : { state: "verified", height, parentHash: parent, blockHash: hash, stateRoot: root };
  }

  function networkInfoVerification(info, configInput) {
    const config = configInput?.sourcesById instanceof Map ? configInput : normalizeConfig(configInput);
    const checkpoint = config.checkpoint;
    if (!checkpoint) return { state: "unknown", reason: "checkpoint-unavailable" };
    if (!isObject(info)) return { state: "unknown", reason: "network-info-unavailable" };
    const expected = {
      chain_id: config.network.chainId,
      protocol_version: checkpoint.protocolVersion,
      recovery_active: true,
      recovery_epoch: checkpoint.recoveryEpoch,
      validator_set_id: checkpoint.validatorSetId,
    };
    const missing = Object.keys(expected).filter((key) => info[key] === null || info[key] === undefined);
    for (const key of ["recovery_domain", "checkpoint_manifest_hash"]) {
      if (!normalizeHex(info[key], 32)) missing.push(key);
    }
    if (missing.length) return { state: "unknown", reason: "network-info-fields-unavailable", missing };
    const mismatches = Object.entries(expected)
      .filter(([key, value]) => info[key] !== value)
      .map(([key]) => key);
    if (normalizeHex(info.recovery_domain, 32) !== checkpoint.recoveryDomain) mismatches.push("recovery_domain");
    if (normalizeHex(info.checkpoint_manifest_hash, 32) !== checkpoint.manifestHash) mismatches.push("checkpoint_manifest_hash");
    return mismatches.length
      ? { state: "mismatch", reason: "network-identity-mismatch", mismatches }
      : { state: "verified" };
  }

  function auditRecoveryCheckpoint(input) {
    const config = input?.config?.sourcesById instanceof Map ? input.config : normalizeConfig(input?.config);
    const resolver = createCanonicalResolver(config);
    const checkpoint = config.checkpoint;
    if (!checkpoint) return { state: "unknown", reason: "checkpoint-unavailable", legacy: { state: "unknown" }, replicas: [] };
    const legacy = checkpointVerification(input?.legacyBlock, checkpoint);
    const evidence = Array.isArray(input?.replicas) ? input.replicas : [];
    const evidenceById = new Map();
    let duplicateSource = false;
    for (const entry of evidence) {
      if (!entry || typeof entry.sourceId !== "string") continue;
      if (evidenceById.has(entry.sourceId)) duplicateSource = true;
      else evidenceById.set(entry.sourceId, entry);
    }
    const replicas = resolver.v3Replicas().map((source) => {
      const entry = evidenceById.get(source.id);
      if (!entry || entry.error) {
        return { sourceId: source.id, state: "unknown", reason: entry?.error || "replica-evidence-unavailable" };
      }
      const boundary = boundaryVerification(entry.boundaryBlock, checkpoint);
      const networkInfo = networkInfoVerification(entry.networkInfo, config);
      const state = boundary.state === "mismatch" || networkInfo.state === "mismatch"
        ? "mismatch"
        : boundary.state === "verified" && networkInfo.state === "verified"
          ? "verified"
          : "unknown";
      return { sourceId: source.id, state, boundary, networkInfo };
    });
    const hasMismatch = duplicateSource || legacy.state === "mismatch" || replicas.some((entry) => entry.state === "mismatch");
    const allVerified = legacy.state === "verified" && replicas.length > 0 && replicas.every((entry) => entry.state === "verified");
    return {
      state: hasMismatch ? "mismatch" : allVerified ? "verified" : "unknown",
      reason: duplicateSource
        ? "duplicate-replica-evidence"
        : hasMismatch
          ? "checkpoint-or-replica-mismatch"
          : allVerified
            ? "exact-checkpoint-and-replica-identity-match"
            : "checkpoint-evidence-incomplete",
      legacy,
      replicas,
    };
  }

  function gateCanonical(provenance, audit) {
    if (!provenance?.canonical) return provenance;
    if (audit?.state === "verified") return { ...provenance, checkpointVerified: true };
    return {
      ...provenance,
      canonical: false,
      configuredCanonical: true,
      checkpointVerified: false,
      reason: audit?.state === "mismatch" ? "checkpoint-audit-mismatch" : "checkpoint-audit-unavailable",
      warning: "This source is configured for the canonical timeline, but the exact recovery checkpoint and every v3 replica identity have not been verified.",
    };
  }

  function formatHash(value, left, right) {
    const normalized = normalizeHex(value);
    if (!normalized) return "Unavailable";
    const head = Number.isInteger(left) ? left : 8;
    const tail = Number.isInteger(right) ? right : 6;
    if (normalized.length <= head + tail) return `0x${normalized}`;
    return `0x${normalized.slice(0, head)}…${normalized.slice(-tail)}`;
  }

  function formatDuration(seconds) {
    const value = Number(seconds);
    if (!Number.isFinite(value) || value < 0) return "Unavailable";
    if (value < 60) return `${Math.round(value)}s`;
    if (value < 3600) return `${Math.floor(value / 60)}m ${Math.round(value % 60)}s`;
    if (value < 86400) return `${Math.floor(value / 3600)}h ${Math.floor((value % 3600) / 60)}m`;
    return `${Math.floor(value / 86400)}d ${Math.floor((value % 86400) / 3600)}h`;
  }

  return Object.freeze({
    CONFIG_SCHEMA,
    ConfigurationError,
    normalizeHex,
    normalizeConfig,
    loadConfig,
    buildRpcUrl,
    createCanonicalResolver,
    blockHeader,
    blockHeight,
    blockHash,
    stateRoot,
    parentHash,
    evaluateLiveness,
    auditCommonHeight,
    classifyReceipt,
    checkpointVerification,
    boundaryVerification,
    networkInfoVerification,
    auditRecoveryCheckpoint,
    gateCanonical,
    formatHash,
    formatDuration,
  });
});
