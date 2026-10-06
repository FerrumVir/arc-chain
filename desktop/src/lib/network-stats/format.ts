// How the live numbers are written on screen. The arc.ai counter uses the same
// rules (docs/network-stats-contract.md, "Display rules").

import type { VersionCountV1, WindowStatusV1 } from "./contract";

/** A per-second rate: one decimal under 10 ("4.4"), whole numbers from 10 up ("169"), as on arc.ai. */
export function formatRate(value: number): string {
  if (value < 10) {
    return value.toLocaleString("en-US", { minimumFractionDigits: 1, maximumFractionDigits: 1 });
  }
  return Math.round(value).toLocaleString("en-US");
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

/**
 * The title dot pulses only while real data shows a live chain. Fixture data
 * (the browser preview) never pulses, whatever it says.
 */
export function shouldPulse(status: WindowStatusV1, syntheticPreview: boolean): boolean {
  return !syntheticPreview && status === "live";
}

/** "16:05:12" in the viewer's local time. */
export function formatClock(unixMs: number): string {
  return new Date(unixMs).toLocaleTimeString("en-US", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hourCycle: "h23",
  });
}

/** "Updated just now", "Updated 3 s ago", "Updated 2 min ago", then "Updated at 16:05:12". */
export function formatUpdatedAgo(asOfUnixMs: number, nowUnixMs: number): string {
  const seconds = Math.max(0, Math.floor((nowUnixMs - asOfUnixMs) / 1000));
  if (seconds < 1) return "Updated just now";
  if (seconds < 60) return `Updated ${seconds} s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `Updated ${minutes} min ago`;
  return `Updated at ${formatClock(asOfUnixMs)}`;
}
