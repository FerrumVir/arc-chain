import { test, expect } from "@playwright/test";
import fixtures from "../../crates/arc-node/tests/fixtures/serving-stats-v2.json" with { type: "json" };
import { extendNetworkStatsV2, summarizeServing } from "../src/lib/network-stats/serving-v2";

const readings = () => fixtures.map((body, i) => ({ coordinator: `private-${i}.internal`, body: structuredClone(body) }));
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
  expect(summarizeServing(readings(), 90001).available).toBe(false);
  expect(summarizeServing(readings(), 59000).available).toBe(false);
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
  r[0].body.day_available = true;
  Object.assign(r[0].body.models[0], { input_tokens_last_day: 100, output_tokens_last_day: 1000 });
  r[1].body.models = [];
  expect(summarizeServing(r, 60000).per_model[0].output_tokens_last_day).toBeNull();
  r[1].body.day_available = true;
  expect(summarizeServing(r, 60000).per_model[0].output_tokens_last_day).toBe(1000);
});
