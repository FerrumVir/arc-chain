// The one native read the live network panel makes, and how a failed read is
// phrased.
//
// The webview cannot reach the validators (the Tauri CSP allows only this
// machine and GitHub), so every read goes through the native command
// `network_live_read` (src-tauri/src/network_live.rs). That command builds the
// path from these typed fields itself, so the panel can issue exactly five
// read-only GETs and nothing else. In particular it can never reach
// `GET /community/list`, whose handler prunes the worker registry as a side
// effect, and it never sends a POST.
//
// This module has no browser or Tauri imports, so unit tests and the arc.ai
// website can use it as is.

/**
 * The six public validators, index-aligned with `PRODUCTION_RPC_ORIGINS` in
 * src-tauri/src/rpc_client.rs and `VALIDATOR_LABELS` in network_live.rs. The
 * native command echoes the label and origin back with every result, and the
 * panel displays those.
 */
export const PUBLIC_VALIDATORS: ReadonlyArray<{ label: string; origin: string }> = [
  { label: "NYC", origin: "https://149.28.32.76" },
  { label: "LAX", origin: "https://140.82.16.112" },
  { label: "AMS", origin: "https://136.244.109.1" },
  { label: "LHR", origin: "https://104.238.171.11" },
  { label: "NRT", origin: "https://202.182.107.41" },
  { label: "SGP", origin: "https://149.28.153.31" },
];

/** `GET /blocks` returns at most 100 blocks per call (arc-node caps `limit` at 100). */
export const MAX_BLOCKS_PER_READ = 100;

export type LiveReadRequest =
  | { kind: "health"; validator: number }
  | { kind: "finality"; validator: number }
  | { kind: "blocks"; validator: number; from: number; to: number }
  | { kind: "scoreboard"; validator: number }
  | { kind: "twinStats"; validator: number };

/**
 * What happened to one read. `notFound` (HTTP 404) is kept apart from a
 * failure: it says the validator does not serve that path, which is the
 * expected answer for endpoints newer than the deployed release.
 * `throttled` means the native read budget refused the read before any
 * network I/O.
 */
export type LiveReadOutcome =
  | "ok"
  | "notFound"
  | "badRequest"
  | "status"
  | "unreachable"
  | "unparseable"
  | "tooLarge"
  | "throttled";

/** Mirrors `LiveReadResult` in src-tauri/src/network_live.rs. */
export interface LiveReadResult {
  validator: number;
  /** City label of the validator, e.g. "LAX". */
  label: string;
  origin: string;
  /** The exact path requested, including its query string. */
  path: string;
  outcome: LiveReadOutcome;
  httpStatus: number | null;
  /** The parsed JSON body. Present only when `outcome` is "ok". */
  body: unknown;
  /** What went wrong, as observed. Never a guess at why. */
  detail: string | null;
  /** This computer's clock when the read finished. */
  fetchedAtUnixMs: number;
  elapsedMs: number;
}

/** The exact path the native command requests. Mirrors `LiveRead::path` in network_live.rs. */
export function livePath(request: LiveReadRequest): string {
  switch (request.kind) {
    case "health":
      return "/health";
    case "finality":
      return "/finality/latest";
    case "blocks":
      return `/blocks?from=${request.from}&to=${request.to}&limit=${request.to - request.from + 1}`;
    case "scoreboard":
      return "/workers/scoreboard?limit=0";
    case "twinStats":
      return "/community/twin_stats";
  }
}

/** The path without its query string, for sentences. */
function pathName(path: string): string {
  const query = path.indexOf("?");
  return query === -1 ? path : path.slice(0, query);
}

/**
 * One sentence naming the validator and what was observed. Like the rest of
 * the app (rpc_client.rs `unavailable_reason`), it states the observation and
 * never guesses a cause.
 */
export function readFailureReason(result: LiveReadResult): string {
  const path = pathName(result.path);
  switch (result.outcome) {
    case "ok":
      return "";
    case "notFound":
      return `${result.label} does not serve ${path} (HTTP 404).`;
    case "badRequest":
      return `${result.label} rejected ${path} as malformed (HTTP 400).`;
    case "status":
      return `${result.label} answered ${path} with HTTP ${result.httpStatus ?? "error"}.`;
    case "unreachable":
      return result.detail
        ? `Could not reach ${result.label} (${result.detail}).`
        : `Could not reach ${result.label}.`;
    case "unparseable":
      return `${result.label} answered ${path} with a response this app could not parse.`;
    case "tooLarge":
      return `${result.label} answered ${path} with a response larger than this app accepts.`;
    case "throttled":
      return `This app paused its read of ${path} from ${result.label} to stay within its read budget.`;
  }
}
