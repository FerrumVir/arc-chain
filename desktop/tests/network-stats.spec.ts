import { expect, test } from "@playwright/test";
import { readFileSync } from "node:fs";
import {
  parseFinality,
  parseHealth,
  parseModelStats,
  parseScoreboard,
  parseTwinStats,
  summarizeCommunity,
  summarizeModels,
  summarizeTwin,
  summarizeValidators,
} from "../src/lib/network-stats/aggregate";
import {
  formatMatchRate,
  formatRate,
  formatVersions,
  shouldAnimateCounter,
} from "../src/lib/network-stats/format";
import { mockBlockTimestamp, mockNetworkRead, mockTipHeight } from "../src/lib/network-stats/mock-chain";
import {
  backoffDelayMs,
  createNetworkStatsPoller,
  type PollerTimers,
} from "../src/lib/network-stats/poller";
import {
  PUBLIC_VALIDATORS,
  type LiveReadRequest,
  type LiveReadResult,
} from "../src/lib/network-stats/read";
import { formatRecordDate, measuredRecordsProblems } from "../src/lib/network-stats/records";
import {
  appendBlocks,
  computeWindowStats,
  parseBlocksBody,
  planNextRead,
  prependBlocks,
  type BlockRow,
} from "../src/lib/network-stats/window";

// Pure logic: no page. The numbers behind the live network panel and the
// arc.ai counter (docs/network-stats-contract.md).

const hashOf = (height: number) => height.toString(16).padStart(64, "0");

function chain(first: number, count: number, intervalMs: number, tx = (height: number) => height % 2): BlockRow[] {
  return Array.from({ length: count }, (_, i) => {
    const height = first + i;
    return {
      height,
      hash: hashOf(height),
      parent_hash: hashOf(height - 1),
      timestamp_ms: 1_790_000_000_000 + i * intervalMs,
      tx_count: tx(height),
    };
  });
}

function result(
  kind: string,
  validator: number,
  outcome: LiveReadResult["outcome"],
  body: unknown = null,
  extra: Partial<LiveReadResult> = {},
): LiveReadResult {
  const paths: Record<string, string> = {
    health: "/health",
    finality: "/finality/latest",
    scoreboard: "/workers/scoreboard?limit=0",
    twinStats: "/community/twin_stats",
  };
  return {
    validator,
    label: PUBLIC_VALIDATORS[validator].label,
    origin: PUBLIC_VALIDATORS[validator].origin,
    path: paths[kind] ?? kind,
    outcome,
    httpStatus: outcome === "ok" ? 200 : outcome === "notFound" ? 404 : null,
    body,
    detail: null,
    fetchedAtUnixMs: 1_790_000_000_000,
    elapsedMs: 5,
    ...extra,
  };
}

const TWIN_STATS = (matched: number, mismatched: number, tokens: number, since = 1_789_000_000_000) => ({
  schema: "arc.community.twin-stats.v1",
  since_unix_ms: since,
  counters: { groups_matched: matched, groups_mismatched: mismatched },
  throughput_last_hour: { window_secs: 3600, verified_jobs: 1, verified_tokens: tokens },
  twin_match_rate: matched / Math.max(1, matched + mismatched),
});

/** The synthetic chain behind a fake clock, recording every read. */
function fakeNetwork(scenario: Parameters<typeof mockNetworkRead>[2] = {}) {
  const state = { now: Date.UTC(2026, 9, 6, 16, 0, 0), chainNow: null as number | null };
  const calls: Array<LiveReadRequest & { at: number }> = [];
  const read = async (request: LiveReadRequest) => {
    calls.push({ ...request, at: state.now });
    return mockNetworkRead(request, state.chainNow ?? state.now, scenario);
  };
  return { state, calls, read };
}

async function runFor(
  net: ReturnType<typeof fakeNetwork>,
  poller: ReturnType<typeof createNetworkStatsPoller>,
  seconds: number,
) {
  const start = net.state.now;
  for (let s = 0; s <= seconds; s += 1) {
    net.state.now = start + s * 1000;
    await poller.tick();
  }
}

test.describe("network stats: the 60 s window", () => {
  test("reproduces every shared test vector", () => {
    const vectors = JSON.parse(
      readFileSync(new URL("../../docs/network-stats-vectors.json", import.meta.url), "utf8"),
    ) as {
      window_length_ms: number;
      vectors: Array<{
        name: string;
        first_height: number;
        first_timestamp_ms: number;
        segments: Array<{ blocks: number; interval_ms: number; tx: number[] }>;
        omit_heights: number[];
        expect: {
          complete: boolean;
          end_height: number | null;
          blocks: number | null;
          finalized_tx: number | null;
          blocks_per_second: number | null;
          tps: number | null;
        };
      }>;
    };
    expect(vectors.window_length_ms).toBe(60_000);
    expect(vectors.vectors.length).toBeGreaterThanOrEqual(7);
    for (const vector of vectors.vectors) {
      const blocks: BlockRow[] = [];
      let height = vector.first_height;
      let timestamp = vector.first_timestamp_ms;
      for (const segment of vector.segments) {
        for (let i = 0; i < segment.blocks; i += 1) {
          if (blocks.length > 0) timestamp += segment.interval_ms;
          blocks.push({
            height,
            hash: hashOf(height),
            parent_hash: hashOf(height - 1),
            timestamp_ms: timestamp,
            tx_count: segment.tx[i % segment.tx.length],
          });
          height += 1;
        }
      }
      const held = blocks.filter((b) => !vector.omit_heights.includes(b.height));
      const stats = computeWindowStats(held, vectors.window_length_ms);
      const why = vector.name;
      expect(stats.complete, why).toBe(vector.expect.complete);
      expect(stats.end_height, why).toBe(vector.expect.end_height);
      expect(stats.blocks, why).toBe(vector.expect.blocks);
      expect(stats.finalized_tx, why).toBe(vector.expect.finalized_tx);
      for (const key of ["blocks_per_second", "tps"] as const) {
        const want = vector.expect[key];
        if (want === null) expect(stats[key], why).toBeNull();
        else expect(stats[key], why).toBeCloseTo(want, 12);
      }
    }
  });

  test("never reports a rate for a partial window and says how much it covers", () => {
    const stats = computeWindowStats(chain(1, 100, 250));
    expect(stats.complete).toBe(false);
    expect(stats.blocks_per_second).toBeNull();
    expect(stats.tps).toBeNull();
    expect(stats.reason).toBe("the blocks read so far cover 24.8 s of the 60 s window");
    expect(computeWindowStats([]).reason).toBe("no finalized blocks have been read yet");
  });

  test("joins blocks only by hash, and keeps a gap apart from a fork", () => {
    const held = chain(100, 10, 250);
    expect(appendBlocks(held, chain(110, 5, 250))).toMatchObject({ ok: true });
    expect(prependBlocks(held, chain(90, 10, 250))).toMatchObject({ ok: true });

    const forked = chain(110, 5, 250).map((b, i) => (i === 0 ? { ...b, parent_hash: "f".repeat(64) } : b));
    expect(appendBlocks(held, forked)).toMatchObject({ ok: false, conflict: true });

    expect(appendBlocks(held, chain(112, 5, 250))).toMatchObject({ ok: false, conflict: false });
    expect(prependBlocks(held, chain(80, 10, 250))).toMatchObject({ ok: false, conflict: false });

    const brokenInside = chain(110, 5, 250).map((b, i) => (i === 3 ? { ...b, parent_hash: "e".repeat(64) } : b));
    expect(appendBlocks(held, brokenInside)).toMatchObject({ ok: false, conflict: true });
  });

  test("reads forward to the finalized tip, then back to a full window", () => {
    expect(planNextRead([], 6_105_642)).toEqual({ direction: "forward", from: 6_105_543, to: 6_105_642 });
    const held = chain(1_000, 100, 250);
    expect(planNextRead(held, 1_150)).toEqual({ direction: "forward", from: 1_100, to: 1_150 });
    expect(planNextRead(held, 1_500)).toEqual({ direction: "forward", from: 1_100, to: 1_199 });
    expect(planNextRead(held, 1_099)).toEqual({ direction: "backfill", from: 900, to: 999 });
    expect(planNextRead(chain(0, 10, 250), 9)).toBeNull();
    expect(planNextRead(chain(1_000, 300, 250), 1_299)).toBeNull();
  });

  test("parses /blocks bodies strictly", () => {
    const body = (heights: number[]) => ({
      blocks: heights.map((height) => ({
        height,
        hash: `0x${hashOf(height).toUpperCase()}`,
        parent_hash: hashOf(height - 1),
        timestamp: 1_790_000_000_000 + height,
        tx_count: 1,
      })),
    });
    const parsed = parseBlocksBody(body([10, 11, 12]), 10, 12);
    expect(parsed.ok).toBe(true);
    if (parsed.ok) expect(parsed.blocks[0].hash).toBe(hashOf(10));
    expect(parseBlocksBody(body([10, 11]), 10, 19)).toMatchObject({ ok: true });
    expect(parseBlocksBody(body([11, 12]), 10, 12)).toMatchObject({ ok: false });
    expect(parseBlocksBody(body([10, 12]), 10, 12)).toMatchObject({ ok: false });
    expect(parseBlocksBody(body([10, 11, 12, 13]), 10, 12)).toMatchObject({ ok: false });
    expect(parseBlocksBody({ blocks: [{ height: 10 }] }, 10, 10)).toMatchObject({ ok: false });
    expect(parseBlocksBody({}, 10, 10)).toMatchObject({ ok: false });
  });
});

test.describe("network stats: aggregation and missing endpoints", () => {
  test("counts validators online from /health, with versions and the highest height", () => {
    const samples = [
      parseHealth(result("health", 0, "ok", { status: "ok", version: "0.8.10", height: 100 })),
      parseHealth(result("health", 1, "ok", { status: "ok", version: "0.8.11", height: 103 })),
      parseHealth(result("health", 2, "ok", { status: "ok", version: "0.8.10", height: 101 })),
      parseHealth(result("health", 3, "ok", { status: "degraded", version: "0.8.10", height: 999 })),
      parseHealth(result("health", 4, "unreachable", null, { detail: "timed out" })),
      null,
    ];
    const { validators, chain: tip } = summarizeValidators(PUBLIC_VALIDATORS, samples);
    expect(validators.total).toBe(6);
    expect(validators.online).toBe(3);
    expect(validators.checked).toBe(5);
    expect(validators.versions).toEqual([
      { version: "0.8.10", count: 2 },
      { version: "0.8.11", count: 1 },
    ]);
    expect(validators.per_validator[3].reason).toBe('LHR answered /health with status "degraded".');
    expect(validators.per_validator[4].reason).toBe("Could not reach NRT (timed out).");
    expect(validators.per_validator[5].online).toBeNull();
    // An offline validator's height is never the chain height.
    expect(tip.height).toBe(103);
    expect(tip.height_validator).toBe("LAX");
  });

  test("shows the highest community count, never a sum", () => {
    const samples = [3, 3, 2, null, 3, 3].map((n, i) =>
      n === null
        ? parseScoreboard(result("scoreboard", i, "notFound"))
        : parseScoreboard(result("scoreboard", i, "ok", { eligible_inference_workers: n, workers: [] })),
    );
    const community = summarizeCommunity(PUBLIC_VALIDATORS, samples);
    expect(community.ready_workers).toBe(3);
    expect(community.validators_reporting).toBe(5);
    expect(community.per_validator[3].reason).toBe("LHR does not serve /workers/scoreboard (HTTP 404).");
  });

  test("hides twin metrics until a validator serves /community/twin_stats", () => {
    const absent = summarizeTwin(
      PUBLIC_VALIDATORS.map((_, i) => ({
        stats: parseTwinStats(result("twinStats", i, "notFound")),
        scoreboard: null,
        asOfUnixMs: null,
      })),
    );
    expect(absent.available).toBe(false);
    expect(absent.verified_tokens_per_second).toBeNull();
    expect(absent.match_rate).toBeNull();
    expect(absent.reason).toContain("v0.8.11");
    // A body without the #139 schema is not read as counters.
    expect(parseTwinStats(result("twinStats", 0, "ok", { counters: {} }))).toMatchObject({ kind: "error" });
  });

  test("sums twin counters across coordinators and recomputes the match rate", () => {
    const twin = summarizeTwin([
      { stats: parseTwinStats(result("twinStats", 0, "ok", TWIN_STATS(98, 1, 7_200))), scoreboard: null, asOfUnixMs: 10 },
      {
        stats: parseTwinStats(result("twinStats", 1, "notFound")),
        scoreboard: parseScoreboard(
          result("scoreboard", 1, "ok", {
            eligible_inference_workers: 3,
            twin: { groups_matched: 99, groups_mismatched: 0, verified_tokens_last_hour: 3_600 },
          }),
        ).twin,
        asOfUnixMs: 20,
      },
      { stats: parseTwinStats(result("twinStats", 2, "notFound")), scoreboard: null, asOfUnixMs: null },
    ]);
    expect(twin.available).toBe(true);
    expect(twin.coordinators_reporting).toBe(2);
    expect(twin.verified_tokens_last_hour).toBe(10_800);
    expect(twin.verified_tokens_per_second).toBeCloseTo(3, 12);
    expect(twin.groups_matched).toBe(197);
    expect(twin.groups_compared).toBe(198);
    expect(twin.match_rate).toBeCloseTo(197 / 198, 12);
    expect(twin.source).toBe("GET /community/twin_stats + GET /workers/scoreboard (twin summary)");
    expect(twin.as_of_unix_ms).toBe(20);
  });

  test("keeps per-model stats empty until reported, then sums tokens and pools answers", () => {
    const empty = summarizeModels([]);
    expect(empty).toMatchObject({ available: false, window_ms: null, per_model: [] });
    expect(empty.reason).toBe("No validator reports per-model serving stats yet.");

    // Test fixtures for the proposed arc.community.model-stats.v1 source.
    const body = (windowSecs: number, served: number, verified: number, rates: number[]) => ({
      schema: "arc.community.model-stats.v1",
      window_secs: windowSecs,
      models: [
        {
          model_id: "0xAB",
          model_name: "fixture-model",
          answers: rates.length,
          served_tokens: served,
          verified_tokens: verified,
          answer_tokens_per_second: rates,
        },
      ],
    });
    const read = (validator: number, payload: unknown) => {
      const parsed = parseModelStats(result("modelStats", validator, "ok", payload));
      if (parsed.kind !== "counts") throw new Error(`fixture did not parse: ${parsed.reason}`);
      return parsed.reading;
    };
    const models = summarizeModels([read(0, body(3600, 7_200, 7_000, [10, 11, 12])), read(1, body(3600, 3_600, 3_600, [50])), null]);
    expect(models.available).toBe(true);
    expect(models.window_ms).toBe(3_600_000);
    expect(models.per_model).toHaveLength(1);
    const [model] = models.per_model;
    expect(model).toMatchObject({
      model_id: "0xab",
      model_name: "fixture-model",
      served_tokens: 10_800,
      verified_tokens: 10_600,
      answers: 4,
      answer_samples: 4,
      coordinators_reporting: 2,
    });
    expect(model.served_tokens_per_second).toBeCloseTo(3, 12);
    expect(model.verified_share).toBeCloseTo(10_600 / 10_800, 12);
    // The median of the pooled answers (10, 11, 12, 50) is 11.5; averaging the
    // coordinators' medians (11 and 50) would claim 30.5.
    expect(model.median_answer_tokens_per_second).toBe(11.5);

    // Different windows are never combined.
    const mixed = summarizeModels([read(0, body(3600, 1, 1, [5])), read(1, body(60, 1, 1, [5]))]);
    expect(mixed.available).toBe(false);
    expect(mixed.reason).toContain("different windows");
    // A missing endpoint is absent, not zero; inconsistent counters are refused.
    expect(parseModelStats(result("modelStats", 2, "notFound"))).toMatchObject({ kind: "absent" });
    expect(parseModelStats(result("modelStats", 2, "ok", body(3600, 10, 11, [5])))).toMatchObject({ kind: "error" });
  });

  test("states a missing endpoint instead of showing zero", async () => {
    expect(parseFinality(result("finality", 1, "notFound"))).toEqual({
      ok: false,
      reason: "LAX does not serve /finality/latest (HTTP 404).",
    });
    expect(parseFinality(result("finality", 1, "ok", { finalized_height: null }))).toMatchObject({ ok: false });
    expect(parseScoreboard(result("scoreboard", 0, "ok", {})).eligible).toBeNull();
    const community = summarizeCommunity(
      PUBLIC_VALIDATORS,
      PUBLIC_VALIDATORS.map((_, i) => parseScoreboard(result("scoreboard", i, "notFound"))),
    );
    expect(community.ready_workers).toBeNull();
    expect(community.reason).toBe("NYC does not serve /workers/scoreboard (HTTP 404).");

    // End to end through the poller: no validator serves /finality/latest.
    const net = fakeNetwork({ finality: false });
    const poller = createNetworkStatsPoller({ read: net.read, now: () => net.state.now });
    await poller.tick();
    const stats = poller.getSnapshot();
    expect(stats.window.status).toBe("unavailable");
    expect(stats.window.reason).toContain("does not serve /finality/latest (HTTP 404)");
    expect(stats.window.blocks_per_second).toBeNull();
    expect(stats.window.tps).toBeNull();
    expect(stats.validators.online).toBe(6);
  });
});

test.describe("network stats: the poller", () => {
  test("reads gently: /health every 10 s, blocks every 15 s, community and twin less often", async () => {
    const net = fakeNetwork();
    const poller = createNetworkStatsPoller({ read: net.read, now: () => net.state.now });
    const start = net.state.now;
    await runFor(net, poller, 60);

    const reads = (kind: LiveReadRequest["kind"], validator?: number) =>
      net.calls.filter((c) => c.kind === kind && (validator === undefined || c.validator === validator));
    const gaps = (times: number[]) => times.slice(1).map((t, i) => t - times[i]);

    for (let v = 0; v < 6; v += 1) {
      const health = reads("health", v).map((c) => c.at - start);
      expect(health[0]).toBe(0);
      expect(Math.min(...gaps(health))).toBeGreaterThanOrEqual(10_000);
      expect(health.length).toBe(v % 2 === 0 ? 7 : 6);
      expect(gaps(reads("scoreboard", v).map((c) => c.at)).every((g) => g >= 30_000)).toBe(true);
    }
    // Odd validators are offset by 5 s, so a fresh height arrives every 5 s.
    expect(reads("health", 1).map((c) => c.at - start)).toEqual([0, 15_000, 25_000, 35_000, 45_000, 55_000]);

    const finality = reads("finality").map((c) => c.at - start);
    expect(finality).toEqual([0, 15_000, 30_000, 45_000, 60_000]);
    const blocks = reads("blocks");
    // Three ranges to fill the first window, then one range per poll.
    expect(blocks.length).toBe(7);
    for (const b of blocks) {
      if (b.kind === "blocks") expect(b.to - b.from).toBeLessThan(100);
    }
    expect(reads("scoreboard").length).toBe(12);
    // /community/twin_stats answers 404 today: once per validator, then every 10 min.
    expect(reads("twinStats").length).toBe(6);

    const stats = poller.getSnapshot();
    expect(stats.schema).toBe("arc.network-stats.v1");
    expect(stats.validators.online).toBe(6);
    expect(stats.validators.versions).toEqual([{ version: "0.8.10", count: 6 }]);
    expect(stats.chain.height).toBe(mockTipHeight(net.state.now));
    expect(stats.window).toMatchObject({
      status: "live",
      length_ms: 60_000,
      blocks: 240,
      finalized_tx: 120,
      blocks_per_second: 4,
      tps: 2,
      advancing: true,
      reason: null,
    });
    expect(stats.window.end_height).toBe(mockTipHeight(net.state.now) - 1);
    expect(stats.window.end_timestamp_ms).toBe(mockBlockTimestamp(stats.window.end_height!));
    expect(stats.community.ready_workers).toBe(3);
    expect(stats.twin.available).toBe(false);
  });

  test("backs off a failing validator exponentially and fails over to the next", async () => {
    expect([1, 2, 3, 5].map((f) => backoffDelayMs(10_000, f, 300_000))).toEqual([20_000, 40_000, 80_000, 300_000]);
    expect(backoffDelayMs(10_000, 0, 300_000)).toBe(10_000);

    const net = fakeNetwork({ offline: [0] });
    const poller = createNetworkStatsPoller({ read: net.read, now: () => net.state.now });
    const start = net.state.now;
    await runFor(net, poller, 200);
    const nyc = net.calls.filter((c) => c.kind === "health" && c.validator === 0).map((c) => c.at - start);
    expect(nyc).toEqual([0, 20_000, 60_000, 140_000]);

    // The first window read tried NYC, failed, and moved on.
    const firstFinality = net.calls.filter((c) => c.kind === "finality" && c.at === start);
    expect(firstFinality.map((c) => c.validator)).toEqual([0, 1]);
    const stats = poller.getSnapshot();
    expect(stats.validators.online).toBe(5);
    expect(stats.validators.per_validator[0]).toMatchObject({ online: false, reason: "Could not reach NYC (synthetic outage)." });
    expect(stats.window.status).toBe("live");
    // Once NYC is known to be down, window reads go elsewhere first.
    const later = net.calls.filter((c) => (c.kind === "finality" || c.kind === "blocks") && c.at > start);
    expect(later.some((c) => c.validator === 0)).toBe(false);
  });

  test("reports a stalled chain when the finalized height stops moving", async () => {
    const net = fakeNetwork();
    net.state.chainNow = net.state.now;
    const poller = createNetworkStatsPoller({ read: net.read, now: () => net.state.now });
    await poller.tick();
    expect(poller.getSnapshot().window.status).toBe("live");

    net.state.now += 15_000;
    await poller.tick();
    const stalled = poller.getSnapshot().window;
    expect(stalled.status).toBe("stalled");
    expect(stalled.advancing).toBe(false);
    expect(stalled.blocks_per_second).toBeNull();
    expect(stalled.tps).toBeNull();
    expect(stalled.reason).toContain("did not move");
    // It asked three validators before calling the chain stalled.
    expect(net.calls.filter((c) => c.kind === "finality" && c.at === net.state.now).length).toBe(3);

    // A first read that finds the newest finalized block 10 minutes old is stalled too.
    const old = fakeNetwork();
    old.state.chainNow = old.state.now - 600_000;
    const fresh = createNetworkStatsPoller({ read: old.read, now: () => old.state.now });
    await fresh.tick();
    expect(fresh.getSnapshot().window.status).toBe("stalled");
    expect(fresh.getSnapshot().window.reason).toContain("s old by this computer's clock");
  });

  test("polls only while subscribed and visible", async () => {
    const intervals: Array<{ id: number; ms: number }> = [];
    const cleared: number[] = [];
    let next = 1;
    const timers: PollerTimers = {
      setInterval: (_callback, ms) => {
        intervals.push({ id: next, ms });
        next += 1;
        return next - 1;
      },
      clearInterval: (handle) => {
        cleared.push(handle as number);
      },
    };
    const net = fakeNetwork();
    const poller = createNetworkStatsPoller({ read: net.read, now: () => net.state.now, timers });
    const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

    await flush();
    expect(intervals).toEqual([]);
    expect(net.calls).toEqual([]);

    let updates = 0;
    const unsubscribe = poller.subscribe(() => {
      updates += 1;
    });
    await flush();
    expect(intervals).toEqual([{ id: 1, ms: 1_000 }]);
    expect(net.calls.length).toBeGreaterThan(0);
    expect(updates).toBeGreaterThan(0);

    poller.setVisible(false);
    expect(cleared).toEqual([1]);
    poller.setVisible(true);
    expect(intervals.map((i) => i.id)).toEqual([1, 2]);

    unsubscribe();
    expect(cleared).toEqual([1, 2]);
  });
});

test.describe("network stats: display", () => {
  test("never rounds a match rate up to 100%, and formats rates and versions", () => {
    expect(formatMatchRate(1_999 / 2_000)).toBe("99.9%");
    expect(formatMatchRate(197 / 198)).toBe("99.4%");
    expect(formatMatchRate(0.995)).toBe("99.5%");
    expect(formatMatchRate(1)).toBe("100%");
    expect(formatMatchRate(0)).toBe("0.0%");
    expect([0, 2, 4.404, 12.345, 168.6].map(formatRate)).toEqual(["0.00", "2.00", "4.40", "12.3", "169"]);
    expect(formatVersions([{ version: "0.8.10", count: 6 }], 6)).toBe("v0.8.10 on all 6");
    expect(
      formatVersions(
        [
          { version: "0.8.10", count: 5 },
          { version: "0.8.11", count: 1 },
        ],
        6,
      ),
    ).toBe("v0.8.10 on 5 · v0.8.11 on 1");
    expect(formatVersions([], 0)).toBe("no validator answering");
  });

  test("moves a counter only when its value changes, and never with reduced motion", () => {
    expect(shouldAnimateCounter(1, 2, false)).toBe(true);
    expect(shouldAnimateCounter(2, 2, false)).toBe(false);
    expect(shouldAnimateCounter(1, 2, true)).toBe(false);
    expect(shouldAnimateCounter(Number.NaN, 2, false)).toBe(false);
  });
});

test.describe("measured records", () => {
  test("every record has a date, its pull requests and a public CI receipt", () => {
    const raw = JSON.parse(
      readFileSync(new URL("../src/lib/network-stats/measured-records.json", import.meta.url), "utf8"),
    ) as { records: Array<{ setting: string; headline: string; receipts: Array<{ url: string }> }> };
    expect(measuredRecordsProblems(raw)).toEqual([]);
    for (const record of raw.records) {
      expect(["lab", "ci"]).toContain(record.setting);
      expect(record.headline.toLowerCase()).not.toContain("live");
      for (const receipt of record.receipts) {
        expect(receipt.url).toMatch(/^https:\/\/github\.com\/FerrumVir\/arc-chain\/actions\/runs\/\d+$/);
      }
    }
    // A record without a receipt is refused.
    const missing = structuredClone(raw) as { records: Array<{ receipts: unknown[] }> };
    missing.records[0].receipts = [];
    expect(measuredRecordsProblems(missing).join("\n")).toContain("at least one receipt is required");
    expect(formatRecordDate("2026-10-06")).toBe("6 Oct 2026");

    // Ready for future model-speed records. A test fixture, not a claim.
    const fixture = {
      id: "fixture-model-speed",
      headline: "Fixture model at 12.5 tokens per second per answer",
      detail: "Test fixture only.",
      setting: "testnet",
      measured_on: "2026-12-01",
      prs: [1],
      model: "fixture-model",
      hardware: "fixture hardware",
      metric: "answer_tokens_per_second",
      value: 12.5,
      unit: "tok/s",
      receipts: [
        {
          label: "evidence file",
          url: `https://github.com/FerrumVir/arc-chain/blob/${"a".repeat(40)}/docs/evidence.json`,
        },
      ],
    };
    const check = (patch: Record<string, unknown>) =>
      measuredRecordsProblems({
        schema: "arc.measured-records.v1",
        records: [{ ...fixture, ...patch }],
      }).join("\n");
    expect(check({})).toBe("");
    expect(check({ setting: "projection" })).toContain("a projection is not a record");
    expect(check({ unit: undefined })).toContain("unit is missing");
    expect(check({ hardware: undefined })).toContain("must name the exact model and the hardware");
    expect(
      check({ receipts: [{ label: "moving branch", url: "https://github.com/FerrumVir/arc-chain/blob/main/x.json" }] }),
    ).toContain("commit-pinned");
  });
});
