// Turning individual validator answers into `arc.network-stats.v1` sections.
//
// Pure functions: each takes read results and returns plain data. The rules:
//   - a validator is online when GET /health answered HTTP 200 with
//     `status: "ok"`;
//   - chain height is the highest height an online validator reported;
//   - community nodes are the highest `eligible_inference_workers` any
//     validator reported, never a sum (every node registers with every
//     validator, so a sum would count each node up to six times);
//   - twin counters are summed across coordinators (each twin group is
//     recorded only by the coordinator that ran it) and the match rate is
//     recomputed from the summed counts.

import { NETWORK_STATS_SCHEMA, SOURCES, type NetworkStatsV1, type VersionCountV1 } from "./contract";
import { readFailureReason, type LiveReadResult } from "./read";

/** `GET /community/twin_stats` schema from PR #139. */
export const TWIN_STATS_SCHEMA = "arc.community.twin-stats.v1";

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function count(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() !== "" ? value.trim() : null;
}

// ── /health ─────────────────────────────────────────────────────────────

export interface HealthSample {
  online: boolean;
  version: string | null;
  height: number | null;
  reason: string | null;
  checkedAtUnixMs: number;
}

export function parseHealth(result: LiveReadResult): HealthSample {
  const checkedAtUnixMs = result.fetchedAtUnixMs;
  if (result.outcome !== "ok") {
    return { online: false, version: null, height: null, reason: readFailureReason(result), checkedAtUnixMs };
  }
  const body = record(result.body);
  const status = text(body?.status);
  const version = text(body?.version);
  const height = count(body?.height);
  if (status !== "ok") {
    return {
      online: false,
      version,
      height,
      reason: `${result.label} answered /health with status ${status === null ? "missing" : `"${status}"`}.`,
      checkedAtUnixMs,
    };
  }
  return { online: true, version, height, reason: null, checkedAtUnixMs };
}

// ── /finality/latest ────────────────────────────────────────────────────

export type FinalityReading = { ok: true; finalizedHeight: number } | { ok: false; reason: string };

export function parseFinality(result: LiveReadResult): FinalityReading {
  if (result.outcome !== "ok") return { ok: false, reason: readFailureReason(result) };
  const height = count(record(result.body)?.finalized_height);
  if (height === null) {
    return { ok: false, reason: `${result.label} reported no finalized height on /finality/latest.` };
  }
  return { ok: true, finalizedHeight: height };
}

// ── twin counters (PR #139) ─────────────────────────────────────────────

export interface TwinCounts {
  matched: number;
  mismatched: number;
  tokensLastHour: number;
  windowSecs: number;
  sinceUnixMs: number | null;
  source: "twinStats" | "scoreboard";
}

export type TwinReading =
  | { kind: "counts"; counts: TwinCounts }
  | { kind: "absent"; reason: string }
  | { kind: "error"; reason: string };

/** `GET /community/twin_stats`. A 404 means the validator runs a release without it. */
export function parseTwinStats(result: LiveReadResult): TwinReading {
  if (result.outcome === "notFound") return { kind: "absent", reason: readFailureReason(result) };
  if (result.outcome !== "ok") return { kind: "error", reason: readFailureReason(result) };
  const body = record(result.body);
  if (!body || body.schema !== TWIN_STATS_SCHEMA) {
    return {
      kind: "error",
      reason: `${result.label} answered /community/twin_stats without the ${TWIN_STATS_SCHEMA} schema.`,
    };
  }
  const counters = record(body.counters);
  const throughput = record(body.throughput_last_hour);
  const matched = count(counters?.groups_matched);
  const mismatched = count(counters?.groups_mismatched);
  const tokens = count(throughput?.verified_tokens);
  const windowSecs = count(throughput?.window_secs);
  if (matched === null || mismatched === null || tokens === null || windowSecs === null || windowSecs === 0) {
    return {
      kind: "error",
      reason: `${result.label} answered /community/twin_stats without its match or throughput counters.`,
    };
  }
  return {
    kind: "counts",
    counts: {
      matched,
      mismatched,
      tokensLastHour: tokens,
      windowSecs,
      sinceUnixMs: count(body.since_unix_ms),
      source: "twinStats",
    },
  };
}

/** The `twin` summary PR #139 adds to `/workers/scoreboard` while twin execution is on. */
function scoreboardTwin(body: Record<string, unknown> | null): TwinCounts | null {
  const twin = record(body?.twin);
  if (!twin) return null;
  const matched = count(twin.groups_matched);
  const mismatched = count(twin.groups_mismatched);
  const tokens = count(twin.verified_tokens_last_hour);
  if (matched === null || mismatched === null || tokens === null) return null;
  return {
    matched,
    mismatched,
    tokensLastHour: tokens,
    windowSecs: 3_600,
    sinceUnixMs: null,
    source: "scoreboard",
  };
}

// ── /workers/scoreboard?limit=0 ─────────────────────────────────────────

export interface ScoreboardSample {
  eligible: number | null;
  twin: TwinCounts | null;
  reason: string | null;
  checkedAtUnixMs: number;
}

export function parseScoreboard(result: LiveReadResult): ScoreboardSample {
  const checkedAtUnixMs = result.fetchedAtUnixMs;
  if (result.outcome !== "ok") {
    return { eligible: null, twin: null, reason: readFailureReason(result), checkedAtUnixMs };
  }
  const body = record(result.body);
  const eligible = count(body?.eligible_inference_workers);
  return {
    eligible,
    twin: scoreboardTwin(body),
    reason:
      eligible === null
        ? `${result.label} answered /workers/scoreboard without eligible_inference_workers.`
        : null,
    checkedAtUnixMs,
  };
}

// ── sections ────────────────────────────────────────────────────────────

export interface ValidatorRef {
  label: string;
  origin: string;
}

export function summarizeValidators(
  validators: ReadonlyArray<ValidatorRef>,
  samples: ReadonlyArray<HealthSample | null>,
): { validators: NetworkStatsV1["validators"]; chain: NetworkStatsV1["chain"] } {
  const perValidator = validators.map((validator, index) => {
    const sample = samples[index] ?? null;
    return {
      validator: validator.label,
      online: sample ? sample.online : null,
      version: sample ? sample.version : null,
      height: sample ? sample.height : null,
      checked_at_unix_ms: sample ? sample.checkedAtUnixMs : null,
      reason: sample ? sample.reason : null,
    };
  });
  const online = perValidator.filter((v) => v.online === true);
  const tally = new Map<string, number>();
  for (const v of online) {
    if (v.version) tally.set(v.version, (tally.get(v.version) ?? 0) + 1);
  }
  const versions: VersionCountV1[] = [...tally.entries()]
    .map(([version, n]) => ({ version, count: n }))
    .sort((a, b) => b.count - a.count || a.version.localeCompare(b.version));

  let best: (typeof perValidator)[number] | null = null;
  for (const v of online) {
    if (v.height !== null && (best === null || best.height === null || v.height > best.height)) best = v;
  }
  return {
    validators: {
      total: validators.length,
      online: online.length,
      checked: perValidator.filter((v) => v.online !== null).length,
      versions,
      per_validator: perValidator,
      source: SOURCES.validators,
    },
    chain: {
      height: best?.height ?? null,
      height_validator: best?.validator ?? null,
      height_as_of_unix_ms: best?.checked_at_unix_ms ?? null,
      source: SOURCES.chain,
    },
  };
}

export function summarizeCommunity(
  validators: ReadonlyArray<ValidatorRef>,
  samples: ReadonlyArray<ScoreboardSample | null>,
): NetworkStatsV1["community"] {
  const perValidator = validators.map((validator, index) => {
    const sample = samples[index] ?? null;
    return {
      validator: validator.label,
      eligible_inference_workers: sample ? sample.eligible : null,
      checked_at_unix_ms: sample ? sample.checkedAtUnixMs : null,
      reason: sample ? sample.reason : null,
    };
  });
  const reporting = perValidator.filter((v) => v.eligible_inference_workers !== null);
  const ready = reporting.reduce<number | null>(
    (max, v) => Math.max(max ?? 0, v.eligible_inference_workers ?? 0),
    null,
  );
  const asOf = reporting.reduce<number | null>(
    (newest, v) => Math.max(newest ?? 0, v.checked_at_unix_ms ?? 0),
    null,
  );
  const firstReason = perValidator.find((v) => v.reason !== null)?.reason ?? null;
  return {
    ready_workers: ready,
    validators_reporting: reporting.length,
    per_validator: perValidator,
    as_of_unix_ms: reporting.length > 0 ? asOf : null,
    reason:
      reporting.length > 0
        ? null
        : (firstReason ?? "No validator has been asked for its community count yet."),
    source: SOURCES.community,
  };
}

export interface TwinSlotReading {
  /** Latest GET /community/twin_stats outcome for this validator; null before the first read. */
  stats: TwinReading | null;
  /** Latest scoreboard twin summary for this validator, if any. */
  scoreboard: TwinCounts | null;
  asOfUnixMs: number | null;
}

/** Twin metrics across coordinators. Hidden (available: false) until any validator serves them. */
export function summarizeTwin(readings: ReadonlyArray<TwinSlotReading>): NetworkStatsV1["twin"] {
  const used: TwinCounts[] = [];
  let asOf: number | null = null;
  let absentReason: string | null = null;
  let errorReason: string | null = null;
  for (const reading of readings) {
    const counts =
      reading.stats?.kind === "counts" ? reading.stats.counts : (reading.scoreboard ?? null);
    if (reading.stats?.kind === "absent") absentReason ??= reading.stats.reason;
    if (reading.stats?.kind === "error") errorReason ??= reading.stats.reason;
    if (!counts) continue;
    used.push(counts);
    if (reading.asOfUnixMs !== null) asOf = Math.max(asOf ?? 0, reading.asOfUnixMs);
  }
  if (used.length === 0) {
    return {
      available: false,
      verified_tokens_per_second: null,
      verified_tokens_last_hour: null,
      match_rate: null,
      groups_matched: null,
      groups_compared: null,
      coordinators_reporting: 0,
      since_unix_ms: null,
      source: null,
      as_of_unix_ms: null,
      reason:
        absentReason !== null
          ? "No validator serves /community/twin_stats yet; it arrives with v0.8.11 (PR #139)."
          : (errorReason ?? "Twin execution counters have not been read yet."),
    };
  }
  const matched = used.reduce((sum, c) => sum + c.matched, 0);
  const compared = used.reduce((sum, c) => sum + c.matched + c.mismatched, 0);
  const tokens = used.reduce((sum, c) => sum + c.tokensLastHour, 0);
  const tokensPerSecond = used.reduce((sum, c) => sum + c.tokensLastHour / c.windowSecs, 0);
  const starts = used.map((c) => c.sinceUnixMs).filter((t): t is number => t !== null);
  const sources = new Set(used.map((c) => (c.source === "twinStats" ? SOURCES.twinStats : SOURCES.twinScoreboard)));
  return {
    available: true,
    verified_tokens_per_second: tokensPerSecond,
    verified_tokens_last_hour: tokens,
    match_rate: compared > 0 ? matched / compared : null,
    groups_matched: matched,
    groups_compared: compared,
    coordinators_reporting: used.length,
    since_unix_ms: starts.length > 0 ? Math.min(...starts) : null,
    source: [...sources].join(" + "),
    as_of_unix_ms: asOf,
    reason: compared > 0 ? null : "No twin comparisons have finished yet.",
  };
}

// ── per-model live stats (proposed source) ──────────────────────────────

/**
 * Proposed schema for a validator's per-model serving counters. No validator
 * serves it yet; the desktop does not request it. It is defined so the
 * figures in `models` have one meaning before the endpoint is built (see
 * "Per-model live stats" in docs/network-stats-contract.md).
 */
export const MODEL_STATS_SCHEMA = "arc.community.model-stats.v1";

export interface ModelCounts {
  modelId: string;
  modelName: string | null;
  answers: number;
  servedTokens: number;
  verifiedTokens: number;
  /** One decode rate per answer, as the coordinator reported them. */
  answerRates: number[];
}

/** One coordinator's per-model counters over its trailing window. */
export interface ModelStatsReading {
  windowSecs: number;
  models: ModelCounts[];
  asOfUnixMs: number | null;
}

export type ModelStatsParse =
  | { kind: "counts"; reading: ModelStatsReading }
  | { kind: "absent"; reason: string }
  | { kind: "error"; reason: string };

export function parseModelStats(result: LiveReadResult): ModelStatsParse {
  if (result.outcome === "notFound") return { kind: "absent", reason: readFailureReason(result) };
  if (result.outcome !== "ok") return { kind: "error", reason: readFailureReason(result) };
  const body = record(result.body);
  const refuse = (what: string): ModelStatsParse => ({
    kind: "error",
    reason: `${result.label} answered with per-model stats that ${what}.`,
  });
  if (!body || body.schema !== MODEL_STATS_SCHEMA) return refuse(`lack the ${MODEL_STATS_SCHEMA} schema`);
  const windowSecs = count(body.window_secs);
  if (windowSecs === null || windowSecs === 0) return refuse("have no window");
  if (!Array.isArray(body.models)) return refuse("have no models list");
  const models: ModelCounts[] = [];
  for (const raw of body.models as unknown[]) {
    const model = record(raw);
    const modelId = text(model?.model_id);
    const answers = count(model?.answers);
    const servedTokens = count(model?.served_tokens);
    const verifiedTokens = count(model?.verified_tokens);
    const rates = model?.answer_tokens_per_second;
    if (
      modelId === null ||
      answers === null ||
      servedTokens === null ||
      verifiedTokens === null ||
      verifiedTokens > servedTokens ||
      !Array.isArray(rates) ||
      !rates.every((r: unknown) => typeof r === "number" && Number.isFinite(r) && r > 0)
    ) {
      return refuse("are incomplete or inconsistent");
    }
    models.push({
      modelId: modelId.toLowerCase(),
      modelName: text(model?.model_name),
      answers,
      servedTokens,
      verifiedTokens,
      answerRates: rates as number[],
    });
  }
  return { kind: "counts", reading: { windowSecs, models, asOfUnixMs: result.fetchedAtUnixMs } };
}

/** The median of a sample; null when empty. */
export function median(values: readonly number[]): number | null {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 1 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

/**
 * Per-model figures across coordinators: token counts are summed (each answer
 * is recorded by the one coordinator that served it), the median is taken over
 * the pooled per-answer rates, and readings over different windows are never
 * mixed.
 */
export function summarizeModels(
  readings: ReadonlyArray<ModelStatsReading | null>,
): NetworkStatsV1["models"] {
  const present = readings.filter((r): r is ModelStatsReading => r !== null);
  const none = {
    available: false,
    window_ms: null,
    per_model: [],
    source: null,
    as_of_unix_ms: null,
  };
  if (present.length === 0) {
    return { ...none, reason: "No validator reports per-model serving stats yet." };
  }
  const windowSecs = present[0].windowSecs;
  if (present.some((r) => r.windowSecs !== windowSecs)) {
    return {
      ...none,
      reason: "Coordinators report per-model stats over different windows, so they are not combined.",
    };
  }
  const byModel = new Map<string, { name: string | null; counts: ModelCounts[] }>();
  for (const reading of present) {
    for (const model of reading.models) {
      const entry = byModel.get(model.modelId) ?? { name: model.modelName, counts: [] };
      entry.name ??= model.modelName;
      entry.counts.push(model);
      byModel.set(model.modelId, entry);
    }
  }
  const perModel = [...byModel.entries()].map(([modelId, entry]) => {
    const served = entry.counts.reduce((sum, c) => sum + c.servedTokens, 0);
    const verified = entry.counts.reduce((sum, c) => sum + c.verifiedTokens, 0);
    const answers = entry.counts.reduce((sum, c) => sum + c.answers, 0);
    const rates = entry.counts.flatMap((c) => c.answerRates);
    const middle = median(rates);
    return {
      model_id: modelId,
      model_name: entry.name,
      served_tokens: served,
      served_tokens_per_second: served / windowSecs,
      verified_tokens: verified,
      verified_share: served > 0 ? verified / served : null,
      answers,
      median_answer_tokens_per_second: middle,
      answer_samples: rates.length,
      coordinators_reporting: entry.counts.length,
      reason: middle === null ? "No per-answer rates were reported in this window." : null,
    };
  });
  const asOf = present.reduce<number | null>(
    (newest, r) => (r.asOfUnixMs === null ? newest : Math.max(newest ?? 0, r.asOfUnixMs)),
    null,
  );
  return {
    available: true,
    window_ms: windowSecs * 1000,
    per_model: perModel,
    source: SOURCES.modelStats,
    as_of_unix_ms: asOf,
    reason: null,
  };
}

/** An empty document: every figure unknown. */
export function emptyNetworkStats(validators: ReadonlyArray<ValidatorRef>): NetworkStatsV1 {
  const { validators: v, chain } = summarizeValidators(validators, []);
  return {
    schema: NETWORK_STATS_SCHEMA,
    as_of_unix_ms: null,
    validators: v,
    chain,
    window: {
      length_ms: 60_000,
      status: "waiting",
      end_height: null,
      end_timestamp_ms: null,
      start_exclusive_timestamp_ms: null,
      blocks: null,
      finalized_tx: null,
      blocks_per_second: null,
      tps: null,
      finalized_height: null,
      advancing: null,
      read_from: [],
      as_of_unix_ms: null,
      reason: "No finalized blocks have been read yet.",
      source: SOURCES.window,
    },
    community: summarizeCommunity(validators, []),
    twin: summarizeTwin([]),
    models: summarizeModels([]),
  };
}
