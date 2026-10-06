// The live network panel's poller: when to read what, from which validator,
// and how to back off.
//
// Gentle by design. Per validator, at most:
//   GET /health                     every 10 s (odd validators offset by 5 s,
//                                   so a fresh height arrives every 5 s);
//   GET /workers/scoreboard?limit=0 every 30 s;
//   GET /community/twin_stats       every 60 s, or every 10 min while it
//                                   answers 404.
// The block window is read every 15 s: one GET /finality/latest and,
// normally, one GET /blocks range of about 66 blocks. Reads rotate across the
// validators and fail over to the next one. Each failure doubles that read's
// wait, up to 5 minutes. Nothing is read while no panel is on screen or the
// window is hidden. The native command adds its own read budget on top
// (src-tauri/src/network_live.rs).
//
// No browser or Tauri imports: the timers and the read function are injected,
// so unit tests drive this with a fake clock and the website can reuse it.

import {
  NETWORK_STATS_SCHEMA,
  SOURCES,
  STALE_TIP_MS,
  WINDOW_LENGTH_MS,
  type NetworkStatsV1,
  type WindowStatusV1,
} from "./contract";
import {
  PUBLIC_VALIDATORS,
  livePath,
  readFailureReason,
  type LiveReadRequest,
  type LiveReadResult,
} from "./read";
import {
  appendBlocks,
  computeWindowStats,
  parseBlocksBody,
  planNextRead,
  prependBlocks,
  pruneWindow,
  shouldRestartWindow,
  type BlockRow,
} from "./window";
import {
  emptyNetworkStats,
  parseFinality,
  parseHealth,
  parseScoreboard,
  parseTwinStats,
  summarizeCommunity,
  summarizeModels,
  summarizeTwin,
  summarizeValidators,
  type HealthSample,
  type ScoreboardSample,
  type TwinReading,
  type ValidatorRef,
} from "./aggregate";

export interface Cadence {
  /** How often due work is checked. Nothing is read unless something is due. */
  tickMs: number;
  healthMs: number;
  /** Extra delay for odd validators after their first check. */
  healthStaggerMs: number;
  windowMs: number;
  communityMs: number;
  twinMs: number;
  /** Wait after a validator answers 404 for /community/twin_stats. */
  twinAbsentMs: number;
  /** The first community and twin reads wait this long after start, so start-up is not one burst. */
  communityStartMs: number;
  twinStartMs: number;
  maxBackoffMs: number;
  /** At most this many /blocks reads in one window read. */
  blockReadsPerWindow: number;
  /** Validators tried for one read before giving up on it. */
  failoverAttempts: number;
}

export const DEFAULT_CADENCE: Cadence = {
  tickMs: 1_000,
  healthMs: 10_000,
  healthStaggerMs: 5_000,
  windowMs: 15_000,
  communityMs: 30_000,
  twinMs: 60_000,
  twinAbsentMs: 600_000,
  communityStartMs: 2_000,
  twinStartMs: 4_000,
  maxBackoffMs: 300_000,
  blockReadsPerWindow: 10,
  failoverAttempts: 3,
};

/** The wait after `failures` consecutive failures: base × 2^failures, capped at `maxMs`. */
export function backoffDelayMs(baseMs: number, failures: number, maxMs: number): number {
  if (failures <= 0) return baseMs;
  return Math.min(maxMs, baseMs * 2 ** Math.min(failures, 20));
}

export interface PollerTimers {
  setInterval: (callback: () => void, delayMs: number) => unknown;
  clearInterval: (handle: unknown) => void;
}

const BROWSER_TIMERS: PollerTimers = {
  setInterval: (callback, delayMs) => globalThis.setInterval(callback, delayMs),
  clearInterval: (handle) =>
    globalThis.clearInterval(handle as ReturnType<typeof globalThis.setInterval>),
};

export interface PollerOptions {
  read: (request: LiveReadRequest) => Promise<LiveReadResult>;
  now?: () => number;
  timers?: PollerTimers;
  validators?: ReadonlyArray<ValidatorRef>;
  cadence?: Partial<Cadence>;
}

export interface NetworkStatsPoller {
  /** Start polling while at least one listener is subscribed. Stable reference. */
  subscribe: (listener: () => void) => () => void;
  /** The latest `arc.network-stats.v1` document. Same object until something changes. */
  getSnapshot: () => NetworkStatsV1;
  /** Pause while the app window is hidden. */
  setVisible: (visible: boolean) => void;
  /** Run whatever is due now and wait for it. The interval calls this; tests call it directly. */
  tick: () => Promise<void>;
}

interface Slot {
  dueAt: number;
  failures: number;
  inFlight: boolean;
}

interface TwinSlot {
  slot: Slot;
  reading: TwinReading | null;
  asOfUnixMs: number | null;
}

interface WindowState {
  slot: Slot;
  blocks: BlockRow[];
  status: WindowStatusV1;
  reason: string | null;
  finalizedHeight: number | null;
  advancing: boolean | null;
  readFrom: string[];
  asOfUnixMs: number | null;
  rotation: number;
}

function newSlot(): Slot {
  return { dueAt: 0, failures: 0, inFlight: false };
}

function tipOf(blocks: readonly BlockRow[]): number | null {
  return blocks.length > 0 ? blocks[blocks.length - 1].height : null;
}

export function createNetworkStatsPoller(options: PollerOptions): NetworkStatsPoller {
  const cadence: Cadence = { ...DEFAULT_CADENCE, ...options.cadence };
  const now = options.now ?? (() => Date.now());
  const timers = options.timers ?? BROWSER_TIMERS;
  const validators = options.validators ?? PUBLIC_VALIDATORS;
  const count = validators.length;

  const health = validators.map(() => ({ slot: newSlot(), sample: null as HealthSample | null }));
  const community = validators.map(() => ({
    slot: newSlot(),
    sample: null as ScoreboardSample | null,
  }));
  const twin: TwinSlot[] = validators.map(() => ({ slot: newSlot(), reading: null, asOfUnixMs: null }));
  const win: WindowState = {
    slot: newSlot(),
    blocks: [],
    status: "waiting",
    reason: "No finalized blocks have been read yet.",
    finalizedHeight: null,
    advancing: null,
    readFrom: [],
    asOfUnixMs: null,
    rotation: 0,
  };

  let snapshot: NetworkStatsV1 = emptyNetworkStats(validators);
  const listeners = new Set<() => void>();
  let handle: unknown = null;
  let visible = true;
  let started = false;

  function settle(slot: Slot, ok: boolean, successDelayMs: number, baseMs: number): void {
    if (ok) {
      slot.failures = 0;
      slot.dueAt = now() + successDelayMs;
    } else {
      slot.failures += 1;
      slot.dueAt = now() + backoffDelayMs(baseMs, slot.failures, cadence.maxBackoffMs);
    }
  }

  async function safeRead(request: LiveReadRequest): Promise<LiveReadResult> {
    try {
      return await options.read(request);
    } catch (error) {
      const validator = validators[request.validator];
      return {
        validator: request.validator,
        label: validator?.label ?? `validator ${request.validator}`,
        origin: validator?.origin ?? "",
        path: livePath(request),
        outcome: "unreachable",
        httpStatus: null,
        body: null,
        detail: error instanceof Error ? error.message : String(error),
        fetchedAtUnixMs: now(),
        elapsedMs: 0,
      };
    }
  }

  /** Validators to try, starting from `start`: online ones first, then unchecked, then offline. */
  function preferredOrder(start: number): number[] {
    const rank = (index: number) => {
      const sample = health[index].sample;
      if (sample === null) return 1;
      return sample.online ? 0 : 2;
    };
    return Array.from({ length: count }, (_, k) => (start + k) % count).sort(
      (a, b) => rank(a) - rank(b),
    );
  }

  function windowSection(): NetworkStatsV1["window"] {
    const stats = computeWindowStats(win.blocks);
    const live = win.status === "live";
    return {
      length_ms: WINDOW_LENGTH_MS,
      status: win.status,
      end_height: stats.end_height,
      end_timestamp_ms: stats.end_timestamp_ms,
      start_exclusive_timestamp_ms: stats.start_exclusive_timestamp_ms,
      blocks: live ? stats.blocks : null,
      finalized_tx: live ? stats.finalized_tx : null,
      blocks_per_second: live ? stats.blocks_per_second : null,
      tps: live ? stats.tps : null,
      finalized_height: win.finalizedHeight,
      advancing: win.advancing,
      read_from: [...win.readFrom],
      as_of_unix_ms: win.asOfUnixMs,
      reason: live ? null : win.reason,
      source: SOURCES.window,
    };
  }

  function rebuild(): void {
    const { validators: validatorSection, chain } = summarizeValidators(
      validators,
      health.map((entry) => entry.sample),
    );
    const communitySection = summarizeCommunity(
      validators,
      community.map((entry) => entry.sample),
    );
    const twinSection = summarizeTwin(
      twin.map((entry, index) => {
        const scoreboard = community[index].sample;
        return {
          stats: entry.reading,
          scoreboard: scoreboard?.twin ?? null,
          asOfUnixMs:
            entry.reading?.kind === "counts"
              ? entry.asOfUnixMs
              : scoreboard?.twin
                ? scoreboard.checkedAtUnixMs
                : null,
        };
      }),
    );
    const windowPart = windowSection();
    const times = [
      chain.height_as_of_unix_ms,
      communitySection.as_of_unix_ms,
      twinSection.as_of_unix_ms,
      windowPart.as_of_unix_ms,
    ].filter((t): t is number => t !== null);
    snapshot = {
      schema: NETWORK_STATS_SCHEMA,
      as_of_unix_ms: times.length > 0 ? Math.max(...times) : null,
      validators: validatorSection,
      chain,
      window: windowPart,
      community: communitySection,
      twin: twinSection,
      // No validator reports per-model serving stats yet, so nothing is read
      // for them; the section stays empty and unavailable until one does.
      models: summarizeModels([]),
    };
    for (const listener of listeners) listener();
  }

  async function runHealth(index: number): Promise<void> {
    const entry = health[index];
    entry.slot.inFlight = true;
    try {
      const sample = parseHealth(await safeRead({ kind: "health", validator: index }));
      const first = entry.sample === null;
      entry.sample = sample;
      const stagger = first && index % 2 === 1 ? cadence.healthStaggerMs : 0;
      settle(entry.slot, sample.online, cadence.healthMs + stagger, cadence.healthMs);
    } finally {
      entry.slot.inFlight = false;
      rebuild();
    }
  }

  async function runCommunity(index: number): Promise<void> {
    const entry = community[index];
    entry.slot.inFlight = true;
    try {
      const sample = parseScoreboard(await safeRead({ kind: "scoreboard", validator: index }));
      entry.sample = sample;
      settle(entry.slot, sample.eligible !== null, cadence.communityMs, cadence.communityMs);
    } finally {
      entry.slot.inFlight = false;
      rebuild();
    }
  }

  async function runTwin(index: number): Promise<void> {
    const entry = twin[index];
    entry.slot.inFlight = true;
    try {
      const result = await safeRead({ kind: "twinStats", validator: index });
      const reading = parseTwinStats(result);
      entry.reading = reading;
      if (reading.kind === "counts") {
        entry.asOfUnixMs = result.fetchedAtUnixMs;
        settle(entry.slot, true, cadence.twinMs, cadence.twinMs);
      } else if (reading.kind === "absent") {
        settle(entry.slot, true, cadence.twinAbsentMs, cadence.twinMs);
      } else {
        settle(entry.slot, false, cadence.twinMs, cadence.twinMs);
      }
    } finally {
      entry.slot.inFlight = false;
      rebuild();
    }
  }

  async function runWindow(): Promise<void> {
    win.slot.inFlight = true;
    try {
      await readWindow();
    } finally {
      win.slot.inFlight = false;
      rebuild();
    }
  }

  async function readWindow(): Promise<void> {
    const order = preferredOrder(win.rotation);
    win.rotation = (win.rotation + 1) % count;
    const tipBefore = tipOf(win.blocks);
    const readFrom = new Set<string>();

    // 1. The finalized height. Ask the next validator when one is behind the
    //    blocks already held, so one lagging validator cannot look like a
    //    stalled chain.
    let best: { height: number; index: number; label: string } | null = null;
    let failure: string | null = null;
    for (const index of order.slice(0, cadence.failoverAttempts)) {
      const result = await safeRead({ kind: "finality", validator: index });
      const reading = parseFinality(result);
      if (!reading.ok) {
        failure = reading.reason;
        continue;
      }
      if (best === null || reading.finalizedHeight > best.height) {
        best = { height: reading.finalizedHeight, index, label: result.label };
      }
      if (tipBefore === null || reading.finalizedHeight > tipBefore) break;
    }
    if (best === null) {
      win.status = "unavailable";
      win.reason = failure ?? "No validator answered /finality/latest.";
      win.readFrom = [];
      settle(win.slot, false, cadence.windowMs, cadence.windowMs);
      return;
    }
    readFrom.add(best.label);

    // 2. Blocks up to the finalized height, then back until a full window is held.
    let blocks: BlockRow[] = shouldRestartWindow(win.blocks, best.height) ? [] : [...win.blocks];
    const heldTip = tipOf(blocks);
    const target = heldTip !== null && heldTip > best.height ? heldTip : best.height;
    let reads = 0;
    let cursor = Math.max(0, order.indexOf(best.index));
    let readFailure: string | null = null;
    while (reads < cadence.blockReadsPerWindow) {
      const plan = planNextRead(blocks, target);
      if (plan === null) break;
      let got: BlockRow[] | null = null;
      for (
        let attempt = 0;
        attempt < cadence.failoverAttempts && reads < cadence.blockReadsPerWindow;
        attempt += 1
      ) {
        const index = order[cursor % order.length];
        cursor += 1;
        reads += 1;
        const result = await safeRead({ kind: "blocks", validator: index, from: plan.from, to: plan.to });
        if (result.outcome !== "ok") {
          readFailure = readFailureReason(result);
          continue;
        }
        const parsed = parseBlocksBody(result.body, plan.from, plan.to);
        if (!parsed.ok) {
          readFailure = `${result.label} answered /blocks, but ${parsed.reason}.`;
          continue;
        }
        if (parsed.blocks.length === 0) {
          readFailure = `${result.label} returned no blocks for ${plan.from}-${plan.to}.`;
          continue;
        }
        got = parsed.blocks;
        readFrom.add(result.label);
        break;
      }
      if (got === null) break;
      const merged =
        plan.direction === "forward" ? appendBlocks(blocks, got) : prependBlocks(blocks, got);
      if (!merged.ok) {
        readFailure = `The blocks read did not form one chain: ${merged.reason}.`;
        // Two validators disagree about a finalized block: drop everything
        // and read the window afresh next time rather than count either.
        if (merged.conflict) blocks = [];
        break;
      }
      blocks = merged.blocks;
      readFailure = null;
    }

    // 3. Count, and decide whether the count may be shown.
    blocks = pruneWindow(blocks);
    const stats = computeWindowStats(blocks);
    const tipAfter = tipOf(blocks);
    const advancing = tipBefore === null ? null : best.height > tipBefore;
    win.blocks = blocks;
    win.finalizedHeight = best.height;
    win.advancing = advancing;
    win.readFrom = [...readFrom];
    const at = now();

    if (tipAfter === null || tipAfter < target) {
      win.status = "unavailable";
      win.reason = readFailure ?? `Could not read the blocks up to finalized block ${target}.`;
      settle(win.slot, false, cadence.windowMs, cadence.windowMs);
      return;
    }
    win.asOfUnixMs = at;
    settle(win.slot, true, cadence.windowMs, cadence.windowMs);
    if (!stats.complete) {
      win.status = "measuring";
      win.reason = readFailure ?? stats.reason;
    } else if (advancing === false) {
      win.status = "stalled";
      win.reason = `No new finalized block since block ${tipAfter}: the finalized height did not move between two reads.`;
    } else if (
      advancing === null &&
      stats.end_timestamp_ms !== null &&
      at - stats.end_timestamp_ms > STALE_TIP_MS
    ) {
      win.status = "stalled";
      win.reason = `The newest finalized block, ${tipAfter}, is ${Math.round(
        (at - stats.end_timestamp_ms) / 1000,
      )} s old by this computer's clock.`;
    } else {
      win.status = "live";
      win.reason = null;
    }
  }

  function isDue(slot: Slot, at: number): boolean {
    return !slot.inFlight && at >= slot.dueAt;
  }

  async function tick(): Promise<void> {
    const at = now();
    if (!started) {
      started = true;
      for (const entry of community) entry.slot.dueAt = at + cadence.communityStartMs;
      for (const entry of twin) entry.slot.dueAt = at + cadence.twinStartMs;
    }
    const jobs: Array<Promise<void>> = [];
    health.forEach((entry, index) => {
      if (isDue(entry.slot, at)) jobs.push(runHealth(index));
    });
    community.forEach((entry, index) => {
      if (isDue(entry.slot, at)) jobs.push(runCommunity(index));
    });
    twin.forEach((entry, index) => {
      if (isDue(entry.slot, at)) jobs.push(runTwin(index));
    });
    if (isDue(win.slot, at)) jobs.push(runWindow());
    await Promise.all(jobs);
  }

  function startTimer(): void {
    if (handle !== null || listeners.size === 0 || !visible) return;
    handle = timers.setInterval(() => {
      void tick();
    }, cadence.tickMs);
    void tick();
  }

  function stopTimer(): void {
    if (handle === null) return;
    timers.clearInterval(handle);
    handle = null;
  }

  const subscribe = (listener: () => void) => {
    listeners.add(listener);
    startTimer();
    return () => {
      listeners.delete(listener);
      if (listeners.size === 0) stopTimer();
    };
  };

  return {
    subscribe,
    getSnapshot: () => snapshot,
    setVisible: (next: boolean) => {
      visible = next;
      if (next) startTimer();
      else stopTimer();
    },
    tick,
  };
}
