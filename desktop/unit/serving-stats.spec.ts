import { readFileSync } from "node:fs";
import ts from "typescript";
import unavailableV1 from "../../crates/arc-node/tests/fixtures/serving-stats-v1-unavailable.json" with { type: "json" };
import { test, expect } from "@playwright/test";
import fixtures from "../../crates/arc-node/tests/fixtures/serving-stats-v2.json" with { type: "json" };
import { extendNetworkStatsV2, summarizeServing } from "../src/lib/network-stats/serving-v2";

const readings = () => fixtures.map((body, i) => ({ coordinator: `private-${i}.internal`, body: { ...structuredClone(body), day_window_start_unix_ms: body.day_window_start_unix_ms as number | null } }));
test("shared vectors pool rates rather than medians and retain v1 fields", () => {
  const v1 = { schema: "arc.network-stats.v1" as const, models: { available: false }, chain: { height: 17 }, window: { tps: 2 } };
  const result = extendNetworkStatsV2(v1, readings(), 60000);
  expect(result.schema).toBe("arc.network-stats.v2");
  expect(result.chain).toEqual(v1.chain);
  expect(result.window).toEqual(v1.window);
  expect(v1.models).toEqual({ available: false });
  const m = result.models.per_model[0];
  expect(m.served_tokens).toBe(87);
  expect(m.served_tokens_per_second).toBe(87 / 60);
  expect(m.input_tokens_per_second).toBe(16 / 60);
  expect(m.median_answer_tokens_per_second).toBe(11.5);
  expect(m.answer_tokens_per_second_percentiles.p95).toBeCloseTo(44.3);
  expect(m.time_to_first_token_ms.p50).toBe(200);
  expect(m.output_tokens_last_day).toBeNull();
  expect(m.hop_latency[0].mean_wall_ms).toBe(16);
});

test("windows, missing sources, restart coverage and duplicate aliases fail closed", () => {
  for (const mutation of [
    (r: ReturnType<typeof readings>) => { r[1].coordinator = r[0].coordinator; },
    (r: ReturnType<typeof readings>) => { r[1].body.available = false; },
    (r: ReturnType<typeof readings>) => { r[1].body.window_start_unix_ms += 1000; r[1].body.window_end_unix_ms += 1000; },
    (r: ReturnType<typeof readings>) => { r[1].body.models[0].verified_tokens = 9999; },
    (r: ReturnType<typeof readings>) => { r[1].body.models[0].answer_tokens_per_second = [Infinity]; },
    (r: ReturnType<typeof readings>) => { r[1].body.models.push(r[1].body.models[0]); },
  ]) {
    const r = readings(); mutation(r);
    expect(summarizeServing(r, 61000).available).toBe(false);
  }
  expect(summarizeServing(readings(), 122001).available).toBe(false);
  expect(summarizeServing(readings(), 57999).available).toBe(false);
  expect(summarizeServing([], 60000).available).toBe(false);
  expect(summarizeServing([{ coordinator: "x", body: null }], 60000).available).toBe(false);
});

test("public output allowlists source fields and excludes addresses", () => {
  const r = readings();
  Object.assign(r[0].body, { hostname: "node.internal", ip: "203.0.113.5" });
  Object.assign(r[0].body.models[0], { model_name: "node.internal", socket: "203.0.113.5:9944" });
  Object.assign(r[0].body.models[0].hop_samples[0], { served_by: "worker.internal", address: "2001:db8::1" });
  const serialized = JSON.stringify(summarizeServing(r, 60000));
  for (const text of [".internal", "203.0.113", "2001:db8", "hostname", "socket", "served_by"])
    expect(serialized).not.toContain(text);
});


test("daily totals require coverage from every coordinator, even one with no model row", () => {
  const r = readings();
  for (const {body} of r) {
    body.window_start_unix_ms += 86400000; body.window_end_unix_ms += 86400000;
    body.day_window_end_unix_ms = body.window_end_unix_ms;
  }
  r[0].body.day_window_start_unix_ms = 60000;
  r[0].body.day_available = true;
  Object.assign(r[0].body.models[0], { input_tokens_last_day: 100, output_tokens_last_day: 1000 });
  r[1].body.models = [];
  expect(summarizeServing(r, 86460000).per_model[0].output_tokens_last_day).toBeNull();
  r[1].body.day_available = true;
  r[1].body.day_window_start_unix_ms = 60000;
  expect(summarizeServing(r, 86460000).per_model[0].output_tokens_last_day).toBe(1000);
});


test("staggered reads and two-second clock skew accept only identical complete minutes", () => {
  // Production snapshots at 70s and 72s both end at 60s, including with +/-1s skew.
  for (const now of [60000, 70000, 72000, 119999, 58000, 122000]) {
    expect(summarizeServing(readings(), now).available).toBe(true);
  }
  const r = readings();
  // A read straddling a minute boundary must retry; do not mix the two intervals.
  r[1].body.window_start_unix_ms += 60000;
  r[1].body.window_end_unix_ms += 60000;
  r[1].body.day_window_end_unix_ms += 60000;
  const mixed = summarizeServing(r, 120001);
  expect(mixed.available).toBe(false);
  expect(mixed.reason).toContain("windows differ");
  expect(summarizeServing(readings(), 122001).available).toBe(false);
  expect(summarizeServing(readings(), 57999).available).toBe(false);
});

test("mixed worker, cached, short and timed answers expose distinct speed and TTFT coverage", () => {
  const r = readings();
  // 99 remote untimed answers (including one short), a cache hit and one timed short.
  const m = r[1].body.models[0];
  Object.assign(m, { answers: 102, served_tokens: 1053, verified_tokens: 1053,
    sampled_answers: 102, cached_answers: 1, short_answers: 2,
    untimed_answers_no_worker_timestamps: 99, decode_missing_timestamps_answers: 98,
    ttft_missing_timestamps_answers: 99, ttft_eligible_answers: 2,
    ttft_samples_ms: [200, 300] });
  const result = summarizeServing(r, 72000);
  expect(result.available).toBe(true);
  const model = result.per_model[0];
  expect(model.answers).toBe(105);
  expect(model.untimed_answers_no_worker_timestamps).toBe(99);
  expect(model.cached_answers).toBe(1);
  expect(model.short_answers).toBe(2);
  expect(model.timing_coverage.speed).toEqual({ eligible_answers: 4, sampled_answers: 4,
    total_answers: 105, partial: true, reasons: ["cached_answers", "short_answers", "missing_token_timestamps"] });
  expect(model.timing_coverage.ttft).toEqual({ eligible_answers: 5, sampled_answers: 5,
    total_answers: 105, partial: true, reasons: ["cached_answers", "missing_first_token_timestamp"] });
  expect(model.reason).toContain("only part");
  expect(model.median_answer_tokens_per_second).toBe(11.5);
  // Retention loss is separate from timing eligibility.
  m.ttft_samples_ms = [200];
  expect(summarizeServing(r, 72000).per_model[0].timing_coverage.ttft.reasons).toContain("sample_limit");
  m.speed_eligible_answers = 200;
  expect(summarizeServing(r, 72000).available).toBe(false);
});

test("fully timed and empty measured populations have no partial timing reason", () => {
  expect(summarizeServing(readings(), 60000).per_model[0].reason).toBeNull();
  const r = readings();
  r.forEach(reading => { reading.body.models = []; });
  expect(summarizeServing(r, 60000)).toMatchObject({ available: true, per_model: [] });
});


test("existing v1 consumer rejects startup/cap 503 responses instead of measuring zero", () => {
  const code = readFileSync(new URL("./fixtures/v1-model-consumer.ts.txt", import.meta.url), "utf8");
  const exports = {} as {
    parseModelStats: (result: Record<string, unknown>) => { kind: string; reading?: unknown };
    summarizeModels: (readings: unknown[]) => { available: boolean };
  };
  new Function("exports", "SOURCES", ts.transpileModule(code, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
  }).outputText)(exports, { modelStats: "test source" });
  for (const alias of ["/community/model_stats", "/inference/model_stats"]) {
    for (const response of unavailableV1) {
      // network_live.rs maps HTTP 503 to outcome=status, HTTP 404 to notFound.
      const result = { label: "synthetic", path: alias, outcome: "status",
        httpStatus: response.status, body: response.body, fetchedAtUnixMs: 60000 };
      const parsed = exports.parseModelStats(result);
      expect(parsed.kind).toBe("error");
      expect(exports.summarizeModels([parsed.reading ?? null]).available).toBe(false);
      expect(exports.parseModelStats({ ...result, outcome: "notFound", httpStatus: 404 }).kind).toBe("absent");
      // Pins the original bug: ignoring HTTP failure would interpret [] as measured zero.
      const unsafe = exports.parseModelStats({ ...result, outcome: "ok", httpStatus: 200 });
      expect(unsafe.kind).toBe("counts");
      expect(exports.summarizeModels([unsafe.reading]).available).toBe(true);
    }
  }
});
