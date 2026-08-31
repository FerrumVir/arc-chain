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
  const LEGACY_ARCHIVE_SOURCE_SCHEMA = "arc.legacy-archive.source.v1";
  const LEGACY_ARCHIVE_QUERY_SCHEMA = "arc.legacy-archive.query.v1";
  const MAINTENANCE_SERVICE_SCHEMA = "arc.frontend.maintenance-interlock.v1";
  const MAINTENANCE_STATUS_SCHEMA = "arc.recovery.legacy-late-fork-interlock-status.v2";
  const OFFICIAL_RETIRED_ORIGINS = Object.freeze([
    Object.freeze({ name: "nyc", origin: "http://149.28.32.76:9090" }),
    Object.freeze({ name: "lax", origin: "http://140.82.16.112:9090" }),
    Object.freeze({ name: "ams", origin: "http://136.244.109.1:9090" }),
    Object.freeze({ name: "lhr", origin: "http://104.238.171.11:9090" }),
    Object.freeze({ name: "nrt", origin: "http://202.182.107.41:9090" }),
    Object.freeze({ name: "sgp", origin: "http://149.28.153.31:9090" }),
  ]);

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

  function requiredHex(raw, field) {
    const value = normalizeHex(raw, 32);
    if (!value) throw new ConfigurationError(`${field} must be a 32-byte hex value`);
    return value;
  }

  function requiredExactHash(raw, field) {
    if (typeof raw !== "string" || !/^[0-9a-f]{64}$/.test(raw)) {
      throw new ConfigurationError(`${field} must be an exact lowercase 32-byte hex value`);
    }
    return raw;
  }

  function normalizeLegacyArchive(raw, field) {
    if (!isObject(raw)) throw new ConfigurationError(`${field} is required for a legacy-fork source`);
    const fields = [
      "schema", "readOnly", "classification", "captureId", "node", "rolloutManifestSha256",
      "archiveManifestSha256", "completeSha256", "bundleSha256", "inventorySha256",
      "bindingIndexSha256", "bindingSha256", "checkpointSha256", "checkpointManifestHash",
      "checkpointPayloadHash", "canonicalCheckpointHeight", "sourceHeight", "sourceBlockHash",
      "sourceStateRoot", "provenancePath",
    ];
    if (Object.keys(raw).length !== fields.length || !fields.every((key) => Object.hasOwn(raw, key))) {
      throw new ConfigurationError(`${field} fields must exactly match ${LEGACY_ARCHIVE_SOURCE_SCHEMA}`);
    }
    if (raw.schema !== LEGACY_ARCHIVE_SOURCE_SCHEMA) {
      throw new ConfigurationError(`${field}.schema must be ${LEGACY_ARCHIVE_SOURCE_SCHEMA}`);
    }
    if (raw.readOnly !== true || raw.classification !== "valid_noncanonical_fork") {
      throw new ConfigurationError(`${field} must identify a read-only valid_noncanonical_fork`);
    }
    const node = nonEmpty(raw.node, `${field}.node`);
    if (!/^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(node)) {
      throw new ConfigurationError(`${field}.node must be lowercase DNS-safe text`);
    }
    const canonicalCheckpointHeight = integer(raw.canonicalCheckpointHeight);
    const sourceHeight = integer(raw.sourceHeight);
    if (canonicalCheckpointHeight === null || canonicalCheckpointHeight < 0) {
      throw new ConfigurationError(`${field}.canonicalCheckpointHeight must be a non-negative integer`);
    }
    if (sourceHeight === null || sourceHeight < 0) {
      throw new ConfigurationError(`${field}.sourceHeight must be a non-negative integer`);
    }
    if (raw.provenancePath !== "/provenance") {
      throw new ConfigurationError(`${field}.provenancePath must be exactly /provenance`);
    }
    return Object.freeze({
      schema: LEGACY_ARCHIVE_SOURCE_SCHEMA,
      readOnly: true,
      classification: "valid_noncanonical_fork",
      captureId: requiredHex(raw.captureId, `${field}.captureId`),
      node,
      rolloutManifestSha256: requiredHex(raw.rolloutManifestSha256, `${field}.rolloutManifestSha256`),
      archiveManifestSha256: requiredHex(raw.archiveManifestSha256, `${field}.archiveManifestSha256`),
      completeSha256: requiredHex(raw.completeSha256, `${field}.completeSha256`),
      bundleSha256: requiredHex(raw.bundleSha256, `${field}.bundleSha256`),
      inventorySha256: requiredHex(raw.inventorySha256, `${field}.inventorySha256`),
      bindingIndexSha256: requiredHex(raw.bindingIndexSha256, `${field}.bindingIndexSha256`),
      bindingSha256: requiredHex(raw.bindingSha256, `${field}.bindingSha256`),
      checkpointSha256: requiredHex(raw.checkpointSha256, `${field}.checkpointSha256`),
      checkpointManifestHash: requiredHex(raw.checkpointManifestHash, `${field}.checkpointManifestHash`),
      checkpointPayloadHash: requiredHex(raw.checkpointPayloadHash, `${field}.checkpointPayloadHash`),
      canonicalCheckpointHeight,
      sourceHeight,
      sourceBlockHash: requiredHex(raw.sourceBlockHash, `${field}.sourceBlockHash`),
      sourceStateRoot: requiredHex(raw.sourceStateRoot, `${field}.sourceStateRoot`),
      provenancePath: "/provenance",
    });
  }

  function normalizeSource(raw, index) {
    if (!isObject(raw)) throw new ConfigurationError(`sources[${index}] must be an object`);
    const id = nonEmpty(raw.id, `sources[${index}].id`);
    if (!/^[a-z0-9][a-z0-9_-]{0,63}$/i.test(id)) {
      throw new ConfigurationError(`sources[${index}].id contains unsupported characters`);
    }
    const kind = nonEmpty(raw.kind, `sources[${index}].kind`);
    if (!SOURCE_KINDS.has(kind)) throw new ConfigurationError(`sources[${index}].kind is unsupported`);
    if (kind !== "legacy-fork" && raw.archive !== undefined) {
      throw new ConfigurationError(`sources[${index}].archive is permitted only for legacy-fork sources`);
    }
    return Object.freeze({
      id,
      name: typeof raw.name === "string" && raw.name.trim() ? raw.name.trim() : id,
      region: typeof raw.region === "string" ? raw.region.trim() : "",
      kind,
      baseUrl: validateEndpoint(raw.baseUrl, `sources[${index}].baseUrl`),
      enabled: raw.enabled !== false,
      replicaGroup: typeof raw.replicaGroup === "string" ? raw.replicaGroup.trim() : "",
      description: typeof raw.description === "string" ? raw.description.trim() : "",
      archive: kind === "legacy-fork" ? normalizeLegacyArchive(raw.archive, `sources[${index}].archive`) : null,
    });
  }

  function normalizeCheckpoint(raw, sourcesById) {
    if (raw === null || raw === undefined) return null;
    if (!isObject(raw)) throw new ConfigurationError("checkpoint must be an object or null");
    const height = integer(raw.height);
    const recoveryHeight = integer(raw.recoveryHeight);
    const legacyPublicMaxHeight = integer(raw.legacyPublicMaxHeight);
    if (height === null || height < 0) throw new ConfigurationError("checkpoint.height must be a non-negative integer");
    if (recoveryHeight !== height + 1) throw new ConfigurationError("checkpoint.recoveryHeight must equal checkpoint.height + 1");
    if (legacyPublicMaxHeight === null || legacyPublicMaxHeight < height) {
      throw new ConfigurationError("checkpoint.legacyPublicMaxHeight must be an integer at least checkpoint.height");
    }
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
      legacyPublicMaxHeight,
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

  function normalizeServices(raw) {
    const source = isObject(raw) ? raw : {};
    const result = { ...source };
    if (source.maintenanceInterlock === undefined) return Object.freeze(result);
    const service = source.maintenanceInterlock;
    const fields = [
      "schema", "path", "sourceSetSha256", "boundarySha256", "toolSha256",
      "sourceMainCommit", "observedCutoffHeight", "requiredHealthyReplicas",
      "maxStalenessSeconds",
    ];
    if (!isObject(service) || Object.keys(service).length !== fields.length || !fields.every((key) => Object.hasOwn(service, key))) {
      throw new ConfigurationError("services.maintenanceInterlock fields differ");
    }
    if (service.schema !== MAINTENANCE_SERVICE_SCHEMA || service.path !== "/maintenance/status") {
      throw new ConfigurationError("services.maintenanceInterlock identity/path differs");
    }
    if (
      integer(service.requiredHealthyReplicas) !== 6
      || integer(service.maxStalenessSeconds) !== 90
      || typeof service.sourceMainCommit !== "string"
      || !/^[0-9a-f]{40}$/.test(service.sourceMainCommit)
      || typeof service.observedCutoffHeight !== "number"
      || !Number.isSafeInteger(service.observedCutoffHeight)
      || service.observedCutoffHeight < 1
    ) {
      throw new ConfigurationError("services.maintenanceInterlock requires six healthy replicas and 90-second expiry");
    }
    result.maintenanceInterlock = Object.freeze({
      schema: MAINTENANCE_SERVICE_SCHEMA,
      path: "/maintenance/status",
      sourceSetSha256: requiredExactHash(service.sourceSetSha256, "services.maintenanceInterlock.sourceSetSha256"),
      boundarySha256: requiredExactHash(service.boundarySha256, "services.maintenanceInterlock.boundarySha256"),
      toolSha256: requiredExactHash(service.toolSha256, "services.maintenanceInterlock.toolSha256"),
      sourceMainCommit: service.sourceMainCommit,
      observedCutoffHeight: service.observedCutoffHeight,
      requiredHealthyReplicas: 6,
      maxStalenessSeconds: 90,
    });
    return Object.freeze(result);
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
    for (const source of sources.filter((item) => item.kind === "legacy-fork")) {
      if (!checkpoint || source.archive.canonicalCheckpointHeight !== checkpoint.height) {
        throw new ConfigurationError(`legacy-fork source ${source.id} must pin the configured canonical checkpoint height`);
      }
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
      services: normalizeServices(raw.services),
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

  function validateMaintenanceStatus(payload, service, nowMs) {
    const fields = [
      "schema", "source_main_commit", "boundary_sha256", "source_set_sha256",
      "tool_sha256", "sampled_at", "expires_at", "poll_interval_seconds",
      "max_staleness_seconds", "observations", "state", "gate_reason",
      "incident_sha256", "required_community_observations",
      "healthy_community_observations", "global_absence_claimed",
    ];
    if (!isObject(payload) || Object.keys(payload).length !== fields.length || !fields.every((key) => Object.hasOwn(payload, key))) {
      throw new ConfigurationError("maintenance status fields differ");
    }
    if (
      payload.schema !== MAINTENANCE_STATUS_SCHEMA
      || payload.boundary_sha256 !== service.boundarySha256
      || payload.source_set_sha256 !== service.sourceSetSha256
      || payload.tool_sha256 !== service.toolSha256
      || payload.poll_interval_seconds !== 30
      || payload.max_staleness_seconds !== service.maxStalenessSeconds
      || payload.global_absence_claimed !== false
    ) {
      throw new ConfigurationError("maintenance status identity/policy differs");
    }
    if (payload.source_main_commit !== service.sourceMainCommit) {
      throw new ConfigurationError("maintenance status source commit differs");
    }
    const exactUtc = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/;
    const sampledAt = typeof payload.sampled_at === "string" && exactUtc.test(payload.sampled_at)
      ? Date.parse(payload.sampled_at)
      : Number.NaN;
    const expiresAt = typeof payload.expires_at === "string" && exactUtc.test(payload.expires_at)
      ? Date.parse(payload.expires_at)
      : Number.NaN;
    const now = Number.isFinite(nowMs) ? nowMs : Date.now();
    if (
      !Number.isFinite(sampledAt)
      || !Number.isFinite(expiresAt)
      || new Date(sampledAt).toISOString().replace(".000Z", "Z") !== payload.sampled_at
      || new Date(expiresAt).toISOString().replace(".000Z", "Z") !== payload.expires_at
      || expiresAt - sampledAt !== service.maxStalenessSeconds * 1000
      || sampledAt > now + 30_000
      || now > expiresAt
    ) {
      throw new ConfigurationError("maintenance status is malformed, future-dated, or expired");
    }
    if (!Array.isArray(payload.observations) || !["HEALTHY", "MAINTENANCE"].includes(payload.state)) {
      throw new ConfigurationError("maintenance status observations/state differ");
    }

    const observationFields = [
      "name", "origin", "scope", "outcome", "height", "block_hash",
      "state_root", "response_sha256",
    ];
    const responseFields = ["info_before", "latest", "exact", "info_after"];
    const coordinates = new Set();
    let requiredCommunityObservations = 0;
    let healthyCommunityObservations = 0;
    let respondingRetiredOrigin = false;
    let postCutoffCommunityOrigin = false;
    for (const [index, row] of payload.observations.entries()) {
      if (!isObject(row) || Object.keys(row).length !== observationFields.length || !observationFields.every((key) => Object.hasOwn(row, key))) {
        throw new ConfigurationError("maintenance observation fields differ");
      }
      const originMatch = typeof row.origin === "string"
        ? row.origin.match(/^https?:\/\/(?:\[[0-9a-fA-F:]+\]|[A-Za-z0-9.-]+):(\d{1,5})$/)
        : null;
      const originPort = originMatch ? Number(originMatch[1]) : 0;
      if (
        typeof row.name !== "string"
        || !/^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(row.name)
        || !["retired", "community"].includes(row.scope)
        || !originMatch
        || originPort < 1
        || originPort > 65_535
        || (row.scope === "community" && !row.origin.startsWith("https://"))
        || !["observed", "inconsistent", "unreachable"].includes(row.outcome)
      ) {
        throw new ConfigurationError("maintenance observation identity/outcome differs");
      }
      const coordinate = `${row.scope}\u0000${row.name}\u0000${row.origin}`;
      if (coordinates.has(coordinate)) throw new ConfigurationError("maintenance observation coordinate is duplicated");
      coordinates.add(coordinate);

      if (row.outcome === "observed") {
        if (typeof row.height !== "number" || !Number.isSafeInteger(row.height) || row.height < 1 || !/^[0-9a-f]{64}$/.test(row.block_hash) || !/^[0-9a-f]{64}$/.test(row.state_root)) {
          throw new ConfigurationError("maintenance observed commitment differs");
        }
      } else if (row.height !== null || row.block_hash !== null || row.state_root !== null) {
        throw new ConfigurationError("maintenance unavailable observation carries a commitment");
      }

      if (row.outcome === "unreachable") {
        if (row.response_sha256 !== null) throw new ConfigurationError("maintenance unreachable observation carries response hashes");
      } else {
        const response = row.response_sha256;
        if (!isObject(response) || Object.keys(response).length !== responseFields.length || !responseFields.every((key) => Object.hasOwn(response, key) && typeof response[key] === "string" && /^[0-9a-f]{64}$/.test(response[key]))) {
          throw new ConfigurationError("maintenance response hashes differ");
        }
      }

      if (row.scope === "community") {
        requiredCommunityObservations += 1;
        if (row.outcome === "observed") {
          healthyCommunityObservations += 1;
          if (row.height > service.observedCutoffHeight) postCutoffCommunityOrigin = true;
        }
      } else if (row.outcome !== "unreachable") {
        respondingRetiredOrigin = true;
      }

      if (index < OFFICIAL_RETIRED_ORIGINS.length) {
        const official = OFFICIAL_RETIRED_ORIGINS[index];
        if (row.scope !== "retired" || row.name !== official.name || row.origin !== official.origin) {
          throw new ConfigurationError("maintenance status exact retired official inventory differs");
        }
      }
    }
    if (payload.observations.length < OFFICIAL_RETIRED_ORIGINS.length) {
      throw new ConfigurationError("maintenance status omits a retired official origin");
    }
    if (
      !Number.isSafeInteger(payload.required_community_observations)
      || !Number.isSafeInteger(payload.healthy_community_observations)
      || payload.required_community_observations !== requiredCommunityObservations
      || payload.healthy_community_observations !== healthyCommunityObservations
    ) {
      throw new ConfigurationError("maintenance community observation counts differ");
    }

    const incident = payload.incident_sha256;
    let expectedState;
    let expectedReason;
    if (incident !== null) {
      if (typeof incident !== "string" || !/^[0-9a-f]{64}$/.test(incident)) throw new ConfigurationError("maintenance incident hash differs");
      expectedState = "MAINTENANCE";
      expectedReason = "latched-legacy-source-incident";
    } else if (healthyCommunityObservations !== requiredCommunityObservations) {
      expectedState = "MAINTENANCE";
      expectedReason = "community-source-observation-unavailable";
    } else {
      expectedState = "HEALTHY";
      expectedReason = "capture-bound-retirement-tripwire-clear";
    }
    if ((respondingRetiredOrigin || postCutoffCommunityOrigin) && incident === null) {
      throw new ConfigurationError("legacy source candidate omitted its latched incident");
    }
    if (payload.state !== expectedState || payload.gate_reason !== expectedReason) {
      throw new ConfigurationError("maintenance status gate reason/state binding differs");
    }
    return Object.freeze({ ...payload, sampledAt, expiresAt });
  }

  async function auditMaintenanceInterlock(options) {
    const resolver = options?.resolver;
    const fetchImpl = options?.fetchImpl;
    const service = resolver?.config?.services?.maintenanceInterlock;
    if (!service) return { state: "unconfigured", reason: "maintenance-interlock-unconfigured", samples: [] };
    if (typeof fetchImpl !== "function") return { state: "unknown", reason: "fetch-unavailable", samples: [] };
    const replicas = resolver.v3Replicas();
    if (replicas.length !== service.requiredHealthyReplicas) {
      return { state: "maintenance", reason: "maintenance-interlock-replica-count-differs", samples: [] };
    }
    const samples = await Promise.all(replicas.map(async (source) => {
      try {
        const response = await fetchImpl(buildRpcUrl(source, service.path), {
          method: "GET",
          headers: { Accept: "application/json" },
          cache: "no-store",
          redirect: "error",
          signal: options.signal,
        });
        if (!response?.ok) return { sourceId: source.id, ok: false, reason: `maintenance-http-${response?.status ?? "unknown"}` };
        const status = validateMaintenanceStatus(await response.json(), service, options.nowMs);
        return { sourceId: source.id, ok: true, status };
      } catch (error) {
        return { sourceId: source.id, ok: false, reason: error?.message || "maintenance-request-failed" };
      }
    }));
    if (samples.some((sample) => !sample.ok)) {
      return { state: "maintenance", reason: "maintenance-interlock-evidence-incomplete", samples };
    }
    if (samples.some((sample) => sample.status.state !== "HEALTHY")) {
      return { state: "maintenance", reason: "late-fork-candidate-or-operator-maintenance", samples };
    }
    return { state: "healthy", reason: "all-six-fresh-healthy-interlocks", samples };
  }

  async function verifyLegacyArchiveSource(options) {
    const source = options?.source;
    if (!source || source.kind !== "legacy-fork" || !source.archive) {
      return { state: "mismatch", reason: "source-is-not-a-pinned-legacy-fork" };
    }
    if (typeof options.fetchImpl !== "function") {
      return { state: "unknown", reason: "fetch-unavailable", sourceId: source.id };
    }
    let response;
    let payload;
    try {
      response = await options.fetchImpl(buildRpcUrl(source, source.archive.provenancePath), {
        method: "GET",
        headers: { Accept: "application/json" },
        cache: "no-store",
        signal: options.signal,
      });
      if (!response?.ok) {
        return { state: "unknown", reason: `provenance-http-${response?.status ?? "unknown"}`, sourceId: source.id };
      }
      payload = await response.json();
    } catch (error) {
      return { state: "unknown", reason: "provenance-request-failed", detail: error?.message || String(error), sourceId: source.id };
    }
    if (!isObject(payload)) return { state: "mismatch", reason: "provenance-is-not-an-object", sourceId: source.id };
    const expected = source.archive;
    const actual = {
      schema: payload.schema,
      readOnly: payload.read_only,
      classification: payload.classification,
      captureId: normalizeHex(payload.capture_id, 32),
      node: payload.node,
      rolloutManifestSha256: normalizeHex(payload.rollout_manifest_sha256, 32),
      archiveManifestSha256: normalizeHex(payload.archive_manifest_sha256, 32),
      completeSha256: normalizeHex(payload.complete_sha256, 32),
      bundleSha256: normalizeHex(payload.bundle_sha256, 32),
      inventorySha256: normalizeHex(payload.inventory_sha256, 32),
      bindingIndexSha256: normalizeHex(payload.binding_index_sha256, 32),
      bindingSha256: normalizeHex(payload.binding_sha256, 32),
      checkpointSha256: normalizeHex(payload.checkpoint_sha256, 32),
      checkpointManifestHash: normalizeHex(payload.checkpoint_manifest_hash, 32),
      checkpointPayloadHash: normalizeHex(payload.checkpoint_payload_hash, 32),
      canonicalCheckpointHeight: integer(payload.canonical_checkpoint_height),
      sourceHeight: integer(payload.source_height),
      sourceBlockHash: normalizeHex(payload.source_block_hash, 32),
      sourceStateRoot: normalizeHex(payload.source_state_root, 32),
    };
    const comparisons = {
      schema: LEGACY_ARCHIVE_QUERY_SCHEMA,
      readOnly: true,
      classification: expected.classification,
      captureId: expected.captureId,
      node: expected.node,
      rolloutManifestSha256: expected.rolloutManifestSha256,
      archiveManifestSha256: expected.archiveManifestSha256,
      completeSha256: expected.completeSha256,
      bundleSha256: expected.bundleSha256,
      inventorySha256: expected.inventorySha256,
      bindingIndexSha256: expected.bindingIndexSha256,
      bindingSha256: expected.bindingSha256,
      checkpointSha256: expected.checkpointSha256,
      checkpointManifestHash: expected.checkpointManifestHash,
      checkpointPayloadHash: expected.checkpointPayloadHash,
      canonicalCheckpointHeight: expected.canonicalCheckpointHeight,
      sourceHeight: expected.sourceHeight,
      sourceBlockHash: expected.sourceBlockHash,
      sourceStateRoot: expected.sourceStateRoot,
    };
    const mismatches = Object.keys(comparisons).filter((key) => actual[key] !== comparisons[key]);
    return mismatches.length
      ? { state: "mismatch", reason: "archive-provenance-mismatch", sourceId: source.id, mismatches }
      : { state: "verified", reason: "exact-archive-provenance-match", sourceId: source.id, provenance: payload };
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
    const rawTypeCode = tx.tx_type_code ?? receipt?.tx_type_code ?? payload?.tx_type_code ?? null;
    const typeCode = typeof rawTypeCode === "number"
      ? rawTypeCode
      : (/^0x[0-9a-f]+$/i.test(String(rawTypeCode ?? ""))
          ? Number.parseInt(String(rawTypeCode), 16)
          : typeof rawType === "number"
            ? rawType
            : (/^0x[0-9a-f]+$/i.test(String(rawType)) ? Number.parseInt(String(rawType), 16) : null));
    const activityEnvelope = payload?.schema === "arc.inference.activity.v1"
      && payload?.source === "chain_receipt"
      && payload?.mined === true;
    const canonicalRewardIdentity = activityEnvelope
      && payload?.record_kind === "mined_community_inference_reward"
      && payload?.tx_type === "CommunityInferenceReward"
      && payload?.tx_type_code === "0x25";
    const canonicalInferenceIdentity = activityEnvelope
      && payload?.record_kind === "mined_inference_attestation"
      && payload?.tx_type === "InferenceAttestation"
      && payload?.tx_type_code === "0x16";
    // Standalone transaction lookups may expose an exact numeric transaction
    // type without the activity-record envelope. They may be categorized, but
    // only the canonical activity shape below can prove a reward was paid.
    const lookupRewardIdentity = !activityEnvelope && typeCode === 0x25;
    const lookupInferenceIdentity = !activityEnvelope
      && (typeCode === 0x16 || normalizedType === "inferenceattestation");
    const category = canonicalRewardIdentity || lookupRewardIdentity
      ? "reward"
      : canonicalInferenceIdentity || lookupInferenceIdentity
        ? "inference"
        : "transaction";
    const rawStatus = receipt?.status ?? receipt?.receipt_status ?? payload?.receipt_status ?? payload?.status ?? null;
    const status = typeof rawStatus === "string" ? rawStatus.trim().toLowerCase() : rawStatus === true ? "success" : rawStatus === false ? "failed" : "unknown";
    const explicitProvenance = activityEnvelope;
    const receiptBacked = (isObject(receipt) && (rawStatus !== null || receipt.block_height != null || receipt.block_hash != null || receipt.mined === true))
      || explicitProvenance;
    const explicitSuccess = receipt?.success ?? (explicitProvenance ? payload?.success : undefined);
    const success = receiptBacked && explicitSuccess !== false && (explicitSuccess === true || SUCCESS_STATES.has(status));
    const failed = receiptBacked && (explicitSuccess === false || FAILURE_STATES.has(status));
    const height = activityEnvelope
      ? integer(payload?.block_height)
      : integer(receipt?.block_height ?? receipt?.height ?? tx?.block_height ?? tx?.height ?? payload?.block_height ?? payload?.height);
    const txHash = activityEnvelope
      ? normalizeHex(payload?.tx_hash, 32)
      : normalizeHex(tx?.hash ?? tx?.tx_hash ?? payload?.hash ?? payload?.tx_hash, 32);
    const mined = receiptBacked && height !== null && txHash !== null;
    const canonicalPaidReward = canonicalRewardIdentity
      && payload?.computed === true
      && payload?.paid === true
      && payload?.earned === true;
    const canonicalComputation = (canonicalInferenceIdentity
      && payload?.computed === true
      && payload?.paid === false
      && payload?.earned === false)
      || canonicalPaidReward;
    const lookupComputation = lookupInferenceIdentity;
    return Object.freeze({
      category,
      type: rawType,
      status,
      receiptBacked,
      success,
      failed,
      mined,
      height,
      txHash,
      rewardEarned: canonicalPaidReward && success && mined,
      inferenceConfirmed: (canonicalComputation || lookupComputation) && success && mined,
      computationConfirmed: (canonicalComputation || lookupComputation) && success && mined,
      paymentConfirmed: canonicalPaidReward && success && mined,
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
    if (mismatches.length) return { state: "mismatch", reason: "network-identity-mismatch", mismatches };
    const lastBlockHeight = integer(info.last_block_height);
    if (lastBlockHeight === null) {
      return { state: "unknown", reason: "last-block-height-unavailable" };
    }
    if (lastBlockHeight <= checkpoint.legacyPublicMaxHeight) {
      return {
        state: "unknown",
        reason: "visible-height-regression-gate-pending",
        lastBlockHeight,
        requiredMinimumHeight: checkpoint.legacyPublicMaxHeight + 1,
      };
    }
    return { state: "verified", lastBlockHeight };
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
    MAINTENANCE_SERVICE_SCHEMA,
    MAINTENANCE_STATUS_SCHEMA,
    OFFICIAL_RETIRED_ORIGINS,
    ConfigurationError,
    normalizeHex,
    normalizeConfig,
    loadConfig,
    buildRpcUrl,
    validateMaintenanceStatus,
    auditMaintenanceInterlock,
    verifyLegacyArchiveSource,
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
