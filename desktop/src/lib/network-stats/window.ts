// The sliding window behind "blocks per second" and "transactions per
// second": pure functions over finalized block headers.
//
// The window is 60 seconds of chain time ending at the newest finalized block
// read (block E, timestamp T). Walking back from E, a block is in the window
// while its header timestamp is greater than T - 60 s. The first block at or
// before T - 60 s (the boundary block) ends the walk and is not counted; having
// read it proves that no block of the window is missing. Without it the window
// is incomplete and no rate is reported: a partial window is never scaled up.
//
//   blocks_per_second = blocks in the window / 60
//   tps               = sum of header tx_count over those blocks / 60
//
// Block headers come from `GET /blocks?from&to`, which returns the same
// header fields as `GET /block/{h}` (height, hash, parent_hash, timestamp,
// tx_count) for up to 100 blocks per request. Every block must name the block
// before it as its parent, so the window is one hash-linked chain segment even
// when its pieces were read from different validators.

import { WINDOW_LENGTH_MS } from "./contract";
import { MAX_BLOCKS_PER_READ } from "./read";

export interface BlockRow {
  height: number;
  /** 64 lowercase hex characters, no 0x. */
  hash: string;
  parent_hash: string;
  /** Header timestamp, unix ms (the producing validator's clock). */
  timestamp_ms: number;
  tx_count: number;
}

/**
 * When the finalized tip is further ahead than this, the window starts again
 * from the tip instead of replaying the gap (for example after the panel was
 * hidden for a while).
 */
export const MAX_FORWARD_GAP_BLOCKS = 600;

/** Never hold more than this many blocks, whatever the block rate. */
export const MAX_WINDOW_BLOCKS = 5_000;

const HASH = /^[0-9a-f]{64}$/;

function hash32(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const hex = value.trim().replace(/^0x/i, "").toLowerCase();
  return HASH.test(hex) ? hex : null;
}

function count(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

export type ParsedBlocks = { ok: true; blocks: BlockRow[] } | { ok: false; reason: string };

/**
 * Parse a `GET /blocks?from&to` body. The blocks must start at `from`, be
 * contiguous and ascending, and not pass `to`. They may stop short of `to`
 * when the validator has not finalized that far yet.
 */
export function parseBlocksBody(body: unknown, from: number, to: number): ParsedBlocks {
  const list =
    body !== null && typeof body === "object" ? (body as { blocks?: unknown }).blocks : undefined;
  if (!Array.isArray(list)) return { ok: false, reason: "the response has no blocks list" };
  const blocks: BlockRow[] = [];
  for (const raw of list) {
    const entry: Record<string, unknown> =
      raw !== null && typeof raw === "object" ? (raw as Record<string, unknown>) : {};
    const height = count(entry.height);
    const timestamp = count(entry.timestamp);
    const txCount = count(entry.tx_count);
    const hash = hash32(entry.hash);
    const parent = hash32(entry.parent_hash);
    if (height === null || timestamp === null || txCount === null || hash === null || parent === null) {
      return {
        ok: false,
        reason: `a block in ${from}-${to} is missing its height, hash, parent hash, timestamp or tx_count`,
      };
    }
    blocks.push({ height, hash, parent_hash: parent, timestamp_ms: timestamp, tx_count: txCount });
  }
  for (let i = 0; i < blocks.length; i += 1) {
    if (blocks[i].height !== from + i) {
      return { ok: false, reason: `expected block ${from + i} but got block ${blocks[i].height}` };
    }
  }
  if (blocks.length > 0 && blocks[blocks.length - 1].height > to) {
    return { ok: false, reason: `the response runs past block ${to}` };
  }
  return { ok: true, blocks };
}

/**
 * `conflict` is true when two blocks disagree about the chain (a parent hash
 * that does not match). The window must then start again. A plain gap (a
 * validator returned fewer blocks than asked) is not a conflict.
 */
export type MergeResult =
  | { ok: true; blocks: BlockRow[] }
  | { ok: false; conflict: boolean; reason: string };

function checkLinks(blocks: readonly BlockRow[]): MergeResult {
  for (let i = 1; i < blocks.length; i += 1) {
    const before = blocks[i - 1];
    const block = blocks[i];
    if (block.height !== before.height + 1) {
      return { ok: false, conflict: false, reason: `block ${before.height + 1} is missing` };
    }
    if (block.parent_hash !== before.hash) {
      return {
        ok: false,
        conflict: true,
        reason: `block ${block.height} does not name block ${before.height} as its parent`,
      };
    }
  }
  return { ok: true, blocks: [...blocks] };
}

/** Add blocks that continue the window at its tip. */
export function appendBlocks(held: readonly BlockRow[], next: readonly BlockRow[]): MergeResult {
  const linked = checkLinks(next);
  if (!linked.ok) return linked;
  if (next.length === 0) return { ok: true, blocks: [...held] };
  if (held.length === 0) return { ok: true, blocks: [...next] };
  const tip = held[held.length - 1];
  if (next[0].height !== tip.height + 1) {
    return {
      ok: false,
      conflict: false,
      reason: `expected block ${tip.height + 1} after block ${tip.height}, got block ${next[0].height}`,
    };
  }
  if (next[0].parent_hash !== tip.hash) {
    return {
      ok: false,
      conflict: true,
      reason: `block ${next[0].height} does not name block ${tip.height} as its parent`,
    };
  }
  return { ok: true, blocks: [...held, ...next] };
}

/** Add older blocks that end just below the window's lowest block. */
export function prependBlocks(held: readonly BlockRow[], older: readonly BlockRow[]): MergeResult {
  const linked = checkLinks(older);
  if (!linked.ok) return linked;
  if (older.length === 0) return { ok: true, blocks: [...held] };
  if (held.length === 0) return { ok: true, blocks: [...older] };
  const lowest = held[0];
  const last = older[older.length - 1];
  if (last.height !== lowest.height - 1) {
    return {
      ok: false,
      conflict: false,
      reason: `expected block ${lowest.height - 1} below block ${lowest.height}, got up to block ${last.height}`,
    };
  }
  if (lowest.parent_hash !== last.hash) {
    return {
      ok: false,
      conflict: true,
      reason: `block ${lowest.height} does not name block ${last.height} as its parent`,
    };
  }
  return { ok: true, blocks: [...older, ...held] };
}

export interface WindowStats {
  complete: boolean;
  end_height: number | null;
  end_timestamp_ms: number | null;
  start_exclusive_timestamp_ms: number | null;
  /** Blocks in the window; null unless complete. */
  blocks: number | null;
  /** Sum of tx_count over them; null unless complete. */
  finalized_tx: number | null;
  blocks_per_second: number | null;
  tps: number | null;
  /** Index (into the input) of the boundary block, when one was read. */
  boundary_index: number | null;
  /** Why the window is incomplete. */
  reason: string | null;
}

const NO_STATS: WindowStats = {
  complete: false,
  end_height: null,
  end_timestamp_ms: null,
  start_exclusive_timestamp_ms: null,
  blocks: null,
  finalized_tx: null,
  blocks_per_second: null,
  tps: null,
  boundary_index: null,
  reason: null,
};

/**
 * Count the window that ends at the last block of `blocks` (ascending,
 * contiguous). See the module comment for the definition.
 */
export function computeWindowStats(
  blocks: readonly BlockRow[],
  lengthMs: number = WINDOW_LENGTH_MS,
): WindowStats {
  if (blocks.length === 0) return { ...NO_STATS, reason: "no finalized blocks have been read yet" };
  for (let i = 1; i < blocks.length; i += 1) {
    if (blocks[i].height !== blocks[i - 1].height + 1) {
      return { ...NO_STATS, reason: `block ${blocks[i - 1].height + 1} is missing` };
    }
  }
  const end = blocks[blocks.length - 1];
  const startExclusive = end.timestamp_ms - lengthMs;
  let inWindow = 0;
  let txs = 0;
  let boundary: number | null = null;
  for (let i = blocks.length - 1; i >= 0; i -= 1) {
    if (blocks[i].timestamp_ms <= startExclusive) {
      boundary = i;
      break;
    }
    inWindow += 1;
    txs += blocks[i].tx_count;
  }
  const where = {
    end_height: end.height,
    end_timestamp_ms: end.timestamp_ms,
    start_exclusive_timestamp_ms: startExclusive,
  };
  if (boundary === null) {
    const coveredSeconds = Math.max(0, end.timestamp_ms - blocks[0].timestamp_ms) / 1000;
    return {
      ...NO_STATS,
      ...where,
      reason: `the blocks read so far cover ${coveredSeconds.toFixed(1)} s of the ${lengthMs / 1000} s window`,
    };
  }
  const seconds = lengthMs / 1000;
  return {
    ...where,
    complete: true,
    blocks: inWindow,
    finalized_tx: txs,
    blocks_per_second: inWindow / seconds,
    tps: txs / seconds,
    boundary_index: boundary,
    reason: null,
  };
}

/**
 * Drop blocks older than the window's boundary block: no later window can
 * need them. Also caps the number of blocks held.
 */
export function pruneWindow(
  blocks: readonly BlockRow[],
  lengthMs: number = WINDOW_LENGTH_MS,
): BlockRow[] {
  const stats = computeWindowStats(blocks, lengthMs);
  const kept =
    stats.boundary_index !== null && stats.boundary_index > 0
      ? blocks.slice(stats.boundary_index)
      : [...blocks];
  return kept.length > MAX_WINDOW_BLOCKS ? kept.slice(kept.length - MAX_WINDOW_BLOCKS) : kept;
}

export interface PlannedRead {
  direction: "forward" | "backfill";
  from: number;
  to: number;
}

/**
 * The next `/blocks` range to read so that the window ends at
 * `finalizedHeight` and reaches back a full window: first forward to the
 * finalized tip, then backwards until the boundary block is held. `null` when
 * nothing is left to read.
 */
export function planNextRead(
  blocks: readonly BlockRow[],
  finalizedHeight: number,
  lengthMs: number = WINDOW_LENGTH_MS,
): PlannedRead | null {
  if (blocks.length === 0) {
    return {
      direction: "forward",
      from: Math.max(0, finalizedHeight - (MAX_BLOCKS_PER_READ - 1)),
      to: finalizedHeight,
    };
  }
  const tip = blocks[blocks.length - 1].height;
  if (finalizedHeight > tip) {
    return {
      direction: "forward",
      from: tip + 1,
      to: Math.min(finalizedHeight, tip + MAX_BLOCKS_PER_READ),
    };
  }
  if (computeWindowStats(blocks, lengthMs).complete) return null;
  const lowest = blocks[0].height;
  if (lowest === 0) return null;
  return {
    direction: "backfill",
    from: Math.max(0, lowest - MAX_BLOCKS_PER_READ),
    to: lowest - 1,
  };
}

/** Whether to start the window again rather than read up to `finalizedHeight`. */
export function shouldRestartWindow(blocks: readonly BlockRow[], finalizedHeight: number): boolean {
  if (blocks.length === 0) return false;
  return finalizedHeight - blocks[blocks.length - 1].height > MAX_FORWARD_GAP_BLOCKS;
}
