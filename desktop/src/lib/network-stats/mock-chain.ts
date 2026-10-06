// A synthetic chain for the browser preview and Playwright. The native app
// never uses it: `mockInvoke` in lib/tauri.ts is unreachable inside Tauri, and
// a production bundle refuses to mock at all.
//
// Unlike the fixed fixtures elsewhere in the mock, this chain advances with
// the clock, because the panel's whole job is a moving window. It is
// deterministic per height: block h has timestamp EPOCH + (h - FIRST) × 250 ms
// and two transactions when h is a multiple of 4. Every 60 s window therefore
// holds exactly 240 blocks and 120 transactions (4.00 blocks/s, 2.00 tx/s),
// whenever a test runs.

import {
  MAX_BLOCKS_PER_READ,
  PUBLIC_VALIDATORS,
  livePath,
  type LiveReadRequest,
  type LiveReadResult,
} from "./read";

export interface MockNetworkScenario {
  /** Validator indexes whose reads fail as unreachable. */
  offline?: number[];
  /** `false`: every validator answers /finality/latest with HTTP 404. */
  finality?: boolean;
  /** /community/twin_stats bodies by validator index. Absent: HTTP 404, as on v0.8.10. */
  twinStats?: Record<number, unknown>;
  /** /health version by validator index (default "0.8.10"). */
  versions?: Record<number, string>;
  /** `eligible_inference_workers` by validator index (default 3). */
  readyWorkers?: Record<number, number>;
}

export const MOCK_BLOCK_INTERVAL_MS = 250;
export const MOCK_CHAIN_EPOCH_MS = Date.UTC(2026, 0, 1);
export const MOCK_FIRST_HEIGHT = 1_000_000;

export function mockTipHeight(nowMs: number): number {
  return (
    MOCK_FIRST_HEIGHT +
    Math.max(0, Math.floor((nowMs - MOCK_CHAIN_EPOCH_MS) / MOCK_BLOCK_INTERVAL_MS))
  );
}

export function mockBlockTimestamp(height: number): number {
  return MOCK_CHAIN_EPOCH_MS + (height - MOCK_FIRST_HEIGHT) * MOCK_BLOCK_INTERVAL_MS;
}

export function mockTxCount(height: number): number {
  return height % 4 === 0 ? 2 : 0;
}

export function mockBlockHash(height: number): string {
  return height.toString(16).padStart(16, "0").repeat(4);
}

export function mockNetworkRead(
  request: LiveReadRequest,
  nowMs: number,
  scenario: MockNetworkScenario = {},
): LiveReadResult {
  const validator = PUBLIC_VALIDATORS[request.validator] ?? {
    label: `validator ${request.validator}`,
    origin: "",
  };
  const base = {
    validator: request.validator,
    label: validator.label,
    origin: validator.origin,
    path: livePath(request),
    fetchedAtUnixMs: nowMs,
    elapsedMs: 0,
  };
  const ok = (body: unknown): LiveReadResult => ({
    ...base,
    outcome: "ok",
    httpStatus: 200,
    body,
    detail: null,
  });
  const notFound = (): LiveReadResult => ({
    ...base,
    outcome: "notFound",
    httpStatus: 404,
    body: null,
    detail: null,
  });
  if (scenario.offline?.includes(request.validator)) {
    return {
      ...base,
      outcome: "unreachable",
      httpStatus: null,
      body: null,
      detail: "synthetic outage",
    };
  }
  const tip = mockTipHeight(nowMs);
  switch (request.kind) {
    case "health":
      return ok({
        status: "ok",
        version: scenario.versions?.[request.validator] ?? "0.8.10",
        height: tip - (request.validator % 2),
        peers: 5,
        validators: 6,
        chain_advancing: true,
        last_block_age_secs: 0,
      });
    case "finality":
      return scenario.finality === false
        ? notFound()
        : ok({ finalized_height: tip - 1, committed_height: tip, finality_lag: 1 });
    case "blocks": {
      if (request.to < request.from || request.to - request.from >= MAX_BLOCKS_PER_READ) {
        return { ...base, outcome: "badRequest", httpStatus: 400, body: null, detail: null };
      }
      const blocks: Array<Record<string, unknown>> = [];
      for (let height = request.from; height <= Math.min(request.to, tip); height += 1) {
        blocks.push({
          height,
          hash: mockBlockHash(height),
          parent_hash: mockBlockHash(height - 1),
          timestamp: mockBlockTimestamp(height),
          tx_count: mockTxCount(height),
          tx_root: "0".repeat(64),
          producer: mockBlockHash(request.validator + 1),
        });
      }
      return ok({
        from: request.from,
        to: request.to,
        limit: request.to - request.from + 1,
        count: blocks.length,
        blocks,
      });
    }
    case "scoreboard":
      return ok({
        workers: [],
        count_visible: 0,
        count_total: 3,
        eligible_inference_workers: scenario.readyWorkers?.[request.validator] ?? 3,
      });
    case "twinStats": {
      const body = scenario.twinStats?.[request.validator];
      return body === undefined ? notFound() : ok(body);
    }
  }
}
