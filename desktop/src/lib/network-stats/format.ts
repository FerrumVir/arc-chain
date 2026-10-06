// How the live numbers are written on screen. The arc.ai counter uses the same
// rules (docs/network-stats-contract.md, "Display rules").

import type { VersionCountV1 } from "./contract";

/** A per-second rate: two decimals under 10, one under 100, whole numbers above. */
export function formatRate(value: number): string {
  const digits = value < 10 ? 2 : value < 100 ? 1 : 0;
  return value.toLocaleString("en-US", {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });
}

/**
 * A share as a percentage with one decimal, truncated rather than rounded so
 * that a rate below 100% is never shown as 100.0%. 199 matches out of 200 is
 * 99.5%; 1,999 out of 2,000 is 99.9%, not 100.0%.
 */
export function formatMatchRate(rate: number): string {
  if (rate >= 1) return "100%";
  const tenths = Math.floor(rate * 1000 + 1e-9) / 10;
  return `${tenths.toFixed(1)}%`;
}

/** "v0.8.10 on all 6", or "v0.8.10 on 5 · v0.8.11 on 1". */
export function formatVersions(versions: readonly VersionCountV1[], online: number): string {
  if (versions.length === 0) return online > 0 ? "version not reported" : "no validator answering";
  if (versions.length === 1 && versions[0].count === online) {
    return `v${versions[0].version} on ${online === 1 ? "the only one" : `all ${online}`}`;
  }
  return versions.map((v) => `v${v.version} on ${v.count}`).join(" · ");
}

/** Whether a counter should animate from `from` to `to`: only on a real change, never with reduced motion. */
export function shouldAnimateCounter(from: number, to: number, reducedMotion: boolean): boolean {
  return !reducedMotion && Number.isFinite(from) && Number.isFinite(to) && from !== to;
}

/** "16:05:12" in the viewer's local time, for "as of" lines. */
export function formatClock(unixMs: number): string {
  return new Date(unixMs).toLocaleTimeString("en-US", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hourCycle: "h23",
  });
}
