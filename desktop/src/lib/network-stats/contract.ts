// `arc.network-stats.v1`: the one definition of ARC's live network numbers.
//
// The desktop panel and the arc.ai public live counter both produce this
// document from the same public validator endpoints, with the same windows and
// the same aggregation rules, so the two show the same numbers. The full field
// reference, the rules and the test vectors are in
// docs/network-stats-contract.md.
//
// Every value is measured. A value that could not be measured is `null` with a
// reason next to it; it is never estimated, extrapolated, carried forward from
// an older reading as if current, or filled with zero.
//
// Keys are snake_case because the document is meant to be published as JSON.
// No browser or Tauri imports: unit tests and the website use this file as is.

export const NETWORK_STATS_SCHEMA = "arc.network-stats.v1";

/** The sliding window for block and transaction rates: 60 seconds of chain time. */
export const WINDOW_LENGTH_MS = 60_000;

/**
 * A window whose newest finalized block is older than this (by this
 * computer's clock) is reported as stalled on its first read. Later reads use
 * a clock-free test: the finalized tip did not move between two reads.
 */
export const STALE_TIP_MS = 120_000;

export const SOURCES = {
  validators: "GET /health on each public validator",
  chain: "GET /health (height)",
  window:
    "GET /finality/latest, then GET /blocks?from&to (block header timestamp and tx_count)",
  community: "GET /workers/scoreboard?limit=0 (eligible_inference_workers)",
  twinStats: "GET /community/twin_stats",
  twinScoreboard: "GET /workers/scoreboard (twin summary)",
} as const;

export interface ValidatorHealthV1 {
  /** City label, e.g. "LAX". */
  validator: string;
  origin: string;
  /** Answered GET /health with HTTP 200 and `status: "ok"` at its latest check. `null`: not checked yet. */
  online: boolean | null;
  version: string | null;
  /** The validator's committed height at its latest check. */
  height: number | null;
  checked_at_unix_ms: number | null;
  /** Why it is not online, as observed. */
  reason: string | null;
}

export interface VersionCountV1 {
  version: string;
  count: number;
}

/**
 * `waiting`: no window read has finished yet.
 * `measuring`: blocks were read, but they do not yet reach back a full window.
 * `live`: a complete window, and the chain is advancing.
 * `stalled`: a complete window, but no new finalized block arrived between
 *   two reads (or, on the first read, the newest one was already old).
 * `unavailable`: the latest window read failed on every validator tried.
 */
export type WindowStatusV1 = "waiting" | "measuring" | "live" | "stalled" | "unavailable";

export interface NetworkStatsV1 {
  schema: typeof NETWORK_STATS_SCHEMA;
  /** This computer's clock at the newest successful read in this document. */
  as_of_unix_ms: number | null;
  validators: {
    /** Public validators polled. */
    total: number;
    /** Validators whose latest GET /health answered HTTP 200 with `status: "ok"`. */
    online: number;
    /** Validators checked at least once. */
    checked: number;
    /** Versions among online validators, most common first. */
    versions: VersionCountV1[];
    per_validator: ValidatorHealthV1[];
    source: string;
  };
  chain: {
    /** Highest committed height among online validators' latest /health. */
    height: number | null;
    /** The validator that reported it. */
    height_validator: string | null;
    height_as_of_unix_ms: number | null;
    source: string;
  };
  window: {
    /** Always 60000. */
    length_ms: number;
    status: WindowStatusV1;
    /** The newest finalized block read: the window ends at this block. */
    end_height: number | null;
    /** That block's header timestamp (validator clock, unix ms). */
    end_timestamp_ms: number | null;
    /** end_timestamp_ms - length_ms. Blocks at or before this are outside the window. */
    start_exclusive_timestamp_ms: number | null;
    /** Finalized blocks in the window. Reported only when `status` is "live". */
    blocks: number | null;
    /** Sum of header tx_count over those blocks. Reported only when "live". */
    finalized_tx: number | null;
    /** blocks / 60. Reported only when "live". */
    blocks_per_second: number | null;
    /** finalized_tx / 60. Reported only when "live". */
    tps: number | null;
    /** `finalized_height` from the latest GET /finality/latest. */
    finalized_height: number | null;
    /** Whether end_height grew since the previous window read. `null` on the first read. */
    advancing: boolean | null;
    /** Validators that served the latest window read. */
    read_from: string[];
    as_of_unix_ms: number | null;
    /** Why the rates are null. */
    reason: string | null;
    source: string;
  };
  community: {
    /**
     * Community nodes online, idle and running the network's model, as
     * counted by the validators (`eligible_inference_workers`). Every node
     * registers with every validator, so this is the highest count any
     * validator reported, never a sum. A node busy with a job is not counted.
     */
    ready_workers: number | null;
    validators_reporting: number;
    per_validator: Array<{
      validator: string;
      eligible_inference_workers: number | null;
      checked_at_unix_ms: number | null;
      reason: string | null;
    }>;
    as_of_unix_ms: number | null;
    reason: string | null;
    source: string;
  };
  /**
   * Twin execution (PR #139, v0.8.11). `available` stays false, and every
   * figure null, until a validator serves the counters.
   */
  twin: {
    available: boolean;
    /** Sum over coordinators of verified tokens in each one's last hour, divided by 3600. */
    verified_tokens_per_second: number | null;
    verified_tokens_last_hour: number | null;
    /** Sum of groups_matched / sum of (groups_matched + groups_mismatched), 0..1. */
    match_rate: number | null;
    groups_matched: number | null;
    /** groups_matched + groups_mismatched. */
    groups_compared: number | null;
    coordinators_reporting: number;
    /** Earliest coordinator start among those reporting (their counters reset on restart). */
    since_unix_ms: number | null;
    source: string | null;
    as_of_unix_ms: number | null;
    reason: string | null;
  };
}
