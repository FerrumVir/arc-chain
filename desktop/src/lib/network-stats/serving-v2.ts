/** Additive network-stats v2 adapter. v1's non-model sections are unchanged.
 * Read one model-stats/v2 alias per coordinator. No IO is performed here.
 * Rates use complete, identical UTC-aligned windows; missing/stale sources
 * invalidate the aggregate. Daily figures are counts, never scaled rates.
 * Pool raw samples: neither percentile averages nor median averages are valid.
 */
type ObjectValue = Record<string, unknown>;
const object = (v: unknown): ObjectValue | null =>
  v !== null && typeof v === "object" && !Array.isArray(v) ? v as ObjectValue : null;
const count = (v: unknown): v is number => Number.isSafeInteger(v) && (v as number) >= 0;
const nullableCount = (v: unknown): v is number | null => v === null || count(v);
const rates = (v: unknown, positive: boolean): v is number[] => Array.isArray(v)
  && v.length <= 1000 && v.every(x => typeof x === "number" && Number.isFinite(x) && (positive ? x > 0 : x >= 0));

export function percentiles(samples: readonly number[]) {
  const sorted = [...samples].sort((a, b) => a - b);
  const at = (p: number) => {
    if (!sorted.length) return null;
    const rank = (sorted.length - 1) * p;
    return sorted[Math.floor(rank)] + (sorted[Math.ceil(rank)] - sorted[Math.floor(rank)]) * (rank % 1);
  };
  return { samples: sorted.length, p5: at(0.05), p50: at(0.5), p95: at(0.95) };
}

type Hop = { hop: number; start_layer: number; end_layer: number; positions: number; wall_ms: number; compute_ms: number };
const timingKeys = ["untimed_answers_no_worker_timestamps", "short_answers", "zero_output_answers",
  "decode_missing_timestamps_answers", "ttft_missing_timestamps_answers", "invalid_decode_timing_answers",
  "speed_eligible_answers", "ttft_eligible_answers"] as const;
type TimingKey = typeof timingKeys[number];
type Model = Record<TimingKey, number> & {
  model_id: string; answers: number; served_tokens: number; verified_tokens: number;
  input_tokens: number | null; input_tokens_last_day: number | null; output_tokens_last_day: number | null;
  answer_tokens_per_second: number[]; ttft_samples_ms: number[]; hop_samples: Hop[];
  sampled_answers: number; cached_answers: number; omitted_hop_samples: number;
};
type Source = { start: number; end: number; dayComplete: boolean; models: Model[] };

function parse(body: unknown, now: number): Source | null {
  const b = object(body);
  if (!b || b.schema !== "arc.community.model-stats.v2" || b.available !== true
    || b.scope !== "completed_public_answers_on_this_coordinator" || b.window_secs !== 60
    || !count(b.window_start_unix_ms) || !count(b.window_end_unix_ms)
    || b.window_end_unix_ms - b.window_start_unix_ms !== 60000
    || b.window_end_unix_ms % 60000 !== 0
    || b.window_end_unix_ms - now > 2000 || now - b.window_end_unix_ms > 62000
    || typeof b.day_available !== "boolean" || b.rejected_model_answers !== 0 || !Array.isArray(b.models) || b.models.length > 32) return null;
  if (b.day_window_end_unix_ms !== b.window_end_unix_ms
    || (b.day_available ? !count(b.day_window_start_unix_ms)
      || b.day_window_start_unix_ms !== b.window_end_unix_ms - 86400000
      : b.day_window_start_unix_ms !== null)) return null;
  const models: Model[] = [];
  const seen = new Set<string>();
  for (const raw of b.models) {
    const m = object(raw);
    if (!m || typeof m.model_id !== "string" || !/^0x[0-9a-f]{64}$/.test(m.model_id)
      || seen.has(m.model_id) || !count(m.answers) || !count(m.served_tokens)
      || !count(m.verified_tokens) || m.verified_tokens > m.served_tokens
      || !nullableCount(m.input_tokens) || !nullableCount(m.input_tokens_last_day)
      || !nullableCount(m.output_tokens_last_day) || m.day_window_secs !== 86400
      || !rates(m.answer_tokens_per_second, true) || !rates(m.ttft_samples_ms, false)
      || !count(m.sampled_answers) || m.sampled_answers > m.answers || m.sampled_answers > 1000
      || m.answer_tokens_per_second.length > m.sampled_answers || m.ttft_samples_ms.length > m.sampled_answers
      || !count(m.cached_answers) || m.cached_answers > m.answers || !count(m.omitted_hop_samples)
      || !Array.isArray(m.hop_samples) || m.hop_samples.length > 128) return null;
    if (timingKeys.some(key => !count(m[key]) || (m[key] as number) > (m.answers as number))) return null;
    const timing = Object.fromEntries(timingKeys.map(key => [key, m[key]])) as Record<TimingKey, number>;
    if (m.cached_answers + timing.short_answers + timing.decode_missing_timestamps_answers
        + timing.invalid_decode_timing_answers + timing.speed_eligible_answers !== m.answers
      || m.cached_answers + timing.zero_output_answers + timing.ttft_missing_timestamps_answers
        + timing.ttft_eligible_answers !== m.answers
      || timing.zero_output_answers > timing.short_answers
      || m.answer_tokens_per_second.length > timing.speed_eligible_answers
      || m.ttft_samples_ms.length > timing.ttft_eligible_answers) return null;
    const hops: Hop[] = [];
    for (const rawHop of m.hop_samples) {
      const h = object(rawHop);
      if (!h || !count(h.hop) || !count(h.start_layer) || !count(h.end_layer)
        || h.start_layer >= h.end_layer || !count(h.positions) || !count(h.wall_ms) || !count(h.compute_ms)) return null;
      // Explicit allowlist: no hostname/socket/model display name gets copied.
      hops.push({ hop: h.hop, start_layer: h.start_layer, end_layer: h.end_layer,
        positions: h.positions, wall_ms: h.wall_ms, compute_ms: h.compute_ms });
    }
    seen.add(m.model_id);
    models.push({ ...timing, model_id: m.model_id, answers: m.answers, served_tokens: m.served_tokens,
      verified_tokens: m.verified_tokens, input_tokens: m.input_tokens,
      input_tokens_last_day: m.input_tokens_last_day, output_tokens_last_day: m.output_tokens_last_day,
      answer_tokens_per_second: m.answer_tokens_per_second, ttft_samples_ms: m.ttft_samples_ms,
      hop_samples: hops, sampled_answers: m.sampled_answers, cached_answers: m.cached_answers,
      omitted_hop_samples: m.omitted_hop_samples });
  }
  return { start: b.window_start_unix_ms, end: b.window_end_unix_ms, dayComplete: b.day_available, models };
}

export type CoordinatorReading = {
  /** Internal deduplication identity; never included in the public document. */
  coordinator: string;
  body: unknown;
};

export function summarizeServing(readings: readonly CoordinatorReading[], now = Date.now()) {
  const unavailable = (reason: string) => ({ available: false, window_ms: null,
    per_model: [], source: null, as_of_unix_ms: null, reason });
  if (!readings.length) return unavailable("No coordinator serving stats are available.");
  if (new Set(readings.map(r => r.coordinator)).size !== readings.length)
    return unavailable("Duplicate coordinator sources cannot be summed.");
  const parsed = readings.map(r => parse(r.body, now));
  if (parsed.some(s => !s)) return unavailable("A coordinator source is incomplete, stale, or invalid.");
  const sources = parsed as Source[];
  if (sources.some(s => s.start !== sources[0].start || s.end !== sources[0].end))
    return unavailable("Coordinator windows differ; counts cannot be combined.");
  const grouped = new Map<string, { model: Model; coordinator: number }[]>();
  sources.forEach((s, coordinator) => s.models.forEach(model => {
    const group = grouped.get(model.model_id) ?? [];
    group.push({ model, coordinator }); grouped.set(model.model_id, group);
  }));
  const per_model = [...grouped].sort(([a], [b]) => a.localeCompare(b)).map(([model_id, entries]) => {
    const models = entries.map(e => e.model);
    const sum = (key: "answers" | "served_tokens" | "verified_tokens" | "input_tokens" | "input_tokens_last_day" | "output_tokens_last_day" | "cached_answers" | TimingKey | "sampled_answers") => {
      if ((key === "input_tokens_last_day" || key === "output_tokens_last_day") && !sources.every(s => s.dayComplete)) return null;
      let total = 0;
      for (const m of models) { if (m[key] === null) return null; total += m[key]; }
      return Number.isSafeInteger(total) ? total : null;
    };
    const output = sum("served_tokens"), input = sum("input_tokens"), verified = sum("verified_tokens");
    const speed = percentiles(models.flatMap(m => m.answer_tokens_per_second));
    const ttft = percentiles(models.flatMap(m => m.ttft_samples_ms));
    const answers = sum("answers");
    const coverage = (eligible: number | null, samples: number, exclusions: [string, number | null][]) => {
      const reasons = exclusions.filter(([, n]) => n === null || n > 0).map(([reason]) => reason);
      if (eligible === null) reasons.push("count_overflow");
      else if (samples < eligible) reasons.push("sample_limit");
      return { eligible_answers: eligible, sampled_answers: samples, total_answers: answers,
        partial: answers === null || samples < answers, reasons };
    };
    const timingCoverage = {
      speed: coverage(sum("speed_eligible_answers"), speed.samples, [
        ["cached_answers", sum("cached_answers")], ["short_answers", sum("short_answers")],
        ["missing_token_timestamps", sum("decode_missing_timestamps_answers")],
        ["invalid_decode_interval", sum("invalid_decode_timing_answers")]]),
      ttft: coverage(sum("ttft_eligible_answers"), ttft.samples, [
        ["cached_answers", sum("cached_answers")], ["zero_output_answers", sum("zero_output_answers")],
        ["missing_first_token_timestamp", sum("ttft_missing_timestamps_answers")]]),
    };
    return { model_id, model_name: null, answers: sum("answers"), served_tokens: output,
      served_tokens_per_second: output === null ? null : output / 60,
      verified_tokens: verified, verified_share: output && verified !== null ? verified / output : null,
      median_answer_tokens_per_second: speed.p50, answer_samples: speed.samples,
      coordinators_reporting: entries.length, input_tokens: input, output_tokens: output,
      input_tokens_per_second: input === null ? null : input / 60,
      output_tokens_per_second: output === null ? null : output / 60,
      input_tokens_last_day: sum("input_tokens_last_day"), output_tokens_last_day: sum("output_tokens_last_day"),
      day_window_ms: 86400000, cached_answers: sum("cached_answers"),
      ...Object.fromEntries(timingKeys.map(key => [key, sum(key)])) as Record<TimingKey, number | null>,
      sampled_answers: sum("sampled_answers"), timing_coverage: timingCoverage,
      answer_tokens_per_second_percentiles: speed, time_to_first_token_ms: ttft,
      hop_latency: entries.flatMap(({ model, coordinator }) => model.hop_samples.map(h => ({
        coordinator_index: coordinator, ...h,
        mean_wall_ms: h.positions ? h.wall_ms / h.positions : null,
        mean_compute_ms: h.positions ? h.compute_ms / h.positions : null,
      }))),
      hop_samples_truncated: models.some(m => m.omitted_hop_samples > 0),
      reason: timingCoverage.speed.partial || timingCoverage.ttft.partial
        ? "Timing covers only part of the answer population; see timing_coverage counts and reasons." : null,
      day_reason: sources.every(s => s.dayComplete) ? null : "A coordinator lacks a complete day window.",
      input_reason: input === null || sum("input_tokens_last_day") === null
        ? "Input counts are unavailable where tokenization or day coverage is missing." : null,
    };
  });
  return { available: true, window_ms: 60000, per_model,
    window_start_unix_ms: sources[0].start, window_end_unix_ms: sources[0].end,
    source: "GET /community/model_stats/v2 (one alias per coordinator)",
    day_window_start_unix_ms: sources.every(s => s.dayComplete) ? sources[0].end - 86400000 : null,
    day_window_end_unix_ms: sources[0].end,
    as_of_unix_ms: sources[0].end, reason: null };
}

/** The caller supplies its existing, already privacy-filtered v1 document.
 * Retain v1 for old clients; opt-in v2 keeps every non-model field unchanged.
 */
export function extendNetworkStatsV2<T extends { schema: "arc.network-stats.v1"; models: unknown }>(
  v1: T, readings: readonly CoordinatorReading[], now = Date.now(),
): Omit<T, "schema" | "models"> & { schema: "arc.network-stats.v2"; models: ReturnType<typeof summarizeServing> } {
  return { ...v1, schema: "arc.network-stats.v2" as const, models: summarizeServing(readings, now) };
}
