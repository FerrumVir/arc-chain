import { useQuery } from "@tanstack/react-query";
import clsx from "clsx";
import { ArrowDown, ArrowUpRight } from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { Card } from "./Card";
import { InfoPopover } from "./InfoPopover";
import { Skeleton } from "./Skeleton";
import { StatusPill } from "./StatusPill";
import { prefersReducedMotion } from "./Engraving";
import type { DotLevel } from "./PulseDot";
import { api, isSyntheticPreview } from "../lib/tauri";
import { formatInt } from "../lib/format";
import type { NetworkStatsV1 } from "../lib/network-stats/contract";
import {
  formatClock,
  formatMatchRate,
  formatRate,
  formatUpdatedAgo,
  formatVersions,
  shouldAnimateCounter,
  shouldPulse,
} from "../lib/network-stats/format";
import recordsJson from "../lib/network-stats/measured-records.json";
import {
  formatRecordDate,
  pullRequestUrl,
  type MeasuredRecords,
} from "../lib/network-stats/records";
import { useNetworkStats } from "../lib/network-stats/use-network-stats";

/**
 * The network, live: what ARC's six public validators report right now, and
 * this computer's part in it.
 *
 * Every figure is read while the panel is on screen (lib/network-stats) and
 * follows `arc.network-stats.v1` (docs/network-stats-contract.md), the same
 * definitions and wording as the arc.ai live counter. Nothing is estimated: a
 * figure that could not be read is a dash with the reason under it. Rates are
 * counts over the last minute of finalized blocks, never a projection.
 */
export function LiveNetworkPanel() {
  const stats = useNetworkStats();
  // Shares the Jobs card's query (same key), so no extra read of the local node.
  const { data: worker } = useQuery({
    queryKey: ["worker-status"],
    queryFn: api.fetchWorkerStatus,
    refetchInterval: 5000,
  });
  const { validators, chain, window: win, community, twin } = stats;
  const firstLoad = validators.checked === 0;
  const pill = panelPill(stats);
  const pulsing = shouldPulse(win.status, isSyntheticPreview);
  const reduceMotion = prefersReducedMotion();

  return (
    <Card
      className="live-network"
      data-testid="live-network"
      role="region"
      aria-labelledby="live-network-title"
    >
      <div className="live-network-head">
        <div className="live-network-title-row">
          <span
            className={clsx("live-dot", `state-${dotState(stats)}`, pulsing && "pulsing")}
            data-testid="live-network-dot"
            data-pulsing={pulsing ? "true" : "false"}
            aria-hidden="true"
          />
          <h2 className="live-network-title" id="live-network-title">
            Network, live
          </h2>
          <StatusPill level={pill.level} label={pill.label} />
        </div>
        <div className="live-network-meta">
          <UpdatedAgo asOf={stats.as_of_unix_ms} />
          <InfoPopover
            title="How these numbers are measured"
            ariaLabel="How the live network numbers are measured"
          >
            <p>
              Read from ARC&rsquo;s {validators.total} public validators while this screen
              is open. Nothing here is estimated or projected; a number that could
              not be read shows as a dash, with the reason.
            </p>
            <p>
              <strong>Validators online</strong>: answered <code>GET /health</code> with
              status ok at their latest check (each checked every 10 s). The Arc team
              runs all of them today.
            </p>
            <p>
              <strong>Blocks and transactions a second</strong>: the finalized blocks
              in the 60 s ending at the newest finalized block, and the transactions
              in them, each divided by 60. Read every 15 s from block headers; each
              block is linked to the one before by hash.
            </p>
            {win.status === "live" && win.end_height !== null && (
              <p data-testid="live-network-window-detail">
                The last count covers the 60 s ending at block {formatInt(win.end_height)}:{" "}
                {formatInt(win.blocks ?? 0)} blocks and {formatInt(win.finalized_tx ?? 0)}{" "}
                transactions
                {win.read_from.length > 0 && <>, read from {joinLabels(win.read_from)}</>}.
              </p>
            )}
            <p>
              <strong>Community nodes ready</strong>: computers that are online, idle
              and running the network&rsquo;s model, as the validators count them. A node
              busy with a job is not counted. Every node registers with every
              validator, so the highest count is shown, never a sum.
            </p>
            <p style={{ color: "var(--text-muted)", fontSize: 11 }}>
              Definitions: <code>arc.network-stats.v1</code>, shared with the arc.ai
              live counter.
            </p>
          </InfoPopover>
          <button
            type="button"
            className="live-link"
            data-testid="live-network-records-link"
            onClick={() =>
              document
                .getElementById("measured-records")
                ?.scrollIntoView({ block: "start", behavior: reduceMotion ? "auto" : "smooth" })
            }
          >
            <ArrowDown size={12} aria-hidden="true" /> Measured records
          </button>
        </div>
      </div>

      <div className="live-network-grid">
        <LiveStat
          id="validators"
          label="Validators online"
          loading={firstLoad}
          note={
            <>
              <span className="live-note-line">run by the Arc team today</span>
              <span className="live-note-line">
                {formatVersions(validators.versions, validators.online)}
              </span>
              <ValidatorDots validators={validators.per_validator} />
            </>
          }
        >
          {validators.online} of {validators.total}
        </LiveStat>

        <LiveStat
          id="height"
          label="Chain height"
          loading={firstLoad}
          note={
            chain.height_validator
              ? `newest block, reported by ${chain.height_validator}`
              : "no validator reported a height"
          }
        >
          {chain.height !== null ? (
            <LiveCounter id="height" value={chain.height} format={formatWhole} />
          ) : (
            <Missing />
          )}
        </LiveStat>

        <LiveStat
          id="blocks-per-second"
          label="Blocks a second"
          loading={win.status === "waiting"}
          note={
            win.blocks !== null
              ? `${formatInt(win.blocks)} in the last minute`
              : windowShortReason(win)
          }
        >
          {win.blocks_per_second !== null ? (
            <LiveCounter id="blocks-per-second" value={win.blocks_per_second} format={formatRate} />
          ) : (
            <Missing />
          )}
        </LiveStat>

        <LiveStat
          id="tps"
          label="Transactions a second"
          loading={win.status === "waiting"}
          note={
            win.finalized_tx !== null
              ? `${formatInt(win.finalized_tx)} in the last minute`
              : windowShortReason(win)
          }
        >
          {win.tps !== null ? (
            <LiveCounter id="tps" value={win.tps} format={formatRate} />
          ) : (
            <Missing />
          )}
        </LiveStat>

        <LiveStat
          id="community"
          label="Community nodes ready"
          loading={
            community.validators_reporting === 0 &&
            community.per_validator.every((v) => v.checked_at_unix_ms === null)
          }
          note={community.ready_workers !== null ? "ready for AI work" : (community.reason ?? undefined)}
        >
          {community.ready_workers !== null ? (
            <LiveCounter id="community" value={community.ready_workers} format={formatWhole} />
          ) : (
            <Missing />
          )}
        </LiveStat>
      </div>

      {twin.available && (
        <section
          className="live-twin"
          data-testid="live-twin"
          aria-labelledby="live-twin-title"
        >
          <h3 className="live-twin-title" id="live-twin-title">
            AI work, checked by twins
          </h3>
          <div className="live-twin-grid">
            <LiveStat
              id="verified-tokens"
              label="Verified tokens a second"
              note={`average over the last hour · ${twin.coordinators_reporting} of ${validators.total} coordinators`}
            >
              {twin.verified_tokens_per_second !== null ? (
                <LiveCounter
                  id="verified-tokens"
                  value={twin.verified_tokens_per_second}
                  format={formatRate}
                />
              ) : (
                <Missing />
              )}
            </LiveStat>
            <LiveStat
              id="twin-match"
              label="Twin match rate"
              note={
                twin.groups_compared
                  ? `${formatInt(twin.groups_matched ?? 0)} of ${formatInt(twin.groups_compared)} twin pairs gave the same answer`
                  : (twin.reason ?? undefined)
              }
            >
              {twin.match_rate !== null ? formatMatchRate(twin.match_rate) : <Missing />}
            </LiveStat>
          </div>
        </section>
      )}

      <p className="live-network-window" data-testid="live-network-window">
        {windowLine(win)}
      </p>

      {worker?.running && (
        <div className="live-network-you" data-testid="live-contribution">
          <span className="live-label">Your computer</span>
          <span>
            <strong data-testid="live-contribution-completed">
              {worker.jobsCompleted !== null ? formatInt(worker.jobsCompleted) : "—"}
            </strong>{" "}
            jobs completed
          </span>
          <span>
            <strong data-testid="live-contribution-verified">
              {worker.jobsVerified !== null ? formatInt(worker.jobsVerified) : "—"}
            </strong>{" "}
            verified by the network
          </span>
          <span className="live-muted">since your node started</span>
        </div>
      )}
    </Card>
  );
}

const RECORDS = (recordsJson as unknown as MeasuredRecords).records;

/**
 * One-time measurements with receipts. Kept in its own dashed card, marked
 * "Not live", so a lab figure is never read as what the network is doing now.
 * Each record is a plain headline and one line; the method is behind "How we
 * measured".
 */
export function MeasuredRecordsCard() {
  return (
    <Card
      className="measured-records"
      data-testid="measured-records"
      id="measured-records"
      role="region"
      aria-labelledby="measured-records-title"
    >
      <div className="card-header">
        <h3 className="card-title" id="measured-records-title">
          Measured records
        </h3>
        <span className="status-pill info" data-testid="measured-records-not-live">
          Not live
        </span>
      </div>
      <p className="measured-records-intro">
        One-time measurements, each with a public receipt and a date. They are not live
        network numbers.
      </p>
      <ul className="measured-records-list">
        {RECORDS.map((record) => (
          <li
            key={record.id}
            className="measured-record"
            data-testid={`measured-record-${record.id}`}
          >
            <div className="measured-record-headline">{record.headline}</div>
            <p className="measured-record-summary">{record.summary}</p>
            <div className="measured-record-meta">
              <span data-testid="measured-record-date">
                Measured {formatRecordDate(record.measured_on)}
              </span>
              {record.prs.map((pr) => (
                <button
                  key={pr}
                  type="button"
                  className="live-link"
                  onClick={() => api.openExternal(pullRequestUrl(pr))}
                >
                  PR #{pr}
                </button>
              ))}
              {record.receipts.map((receipt) => (
                <button
                  key={receipt.url}
                  type="button"
                  className="live-link"
                  data-testid="measured-record-receipt"
                  data-url={receipt.url}
                  onClick={() => api.openExternal(receipt.url)}
                >
                  <ArrowUpRight size={12} aria-hidden="true" />
                  {receipt.label}
                </button>
              ))}
            </div>
            <details className="measured-record-method" data-testid="measured-record-method">
              <summary>How we measured</summary>
              <p>{record.method}</p>
              {record.model && (
                <p data-testid="measured-record-model">Model: {record.model}</p>
              )}
              {record.hardware && (
                <p data-testid="measured-record-hardware">Hardware: {record.hardware}</p>
              )}
            </details>
          </li>
        ))}
      </ul>
    </Card>
  );
}

function panelPill(stats: NetworkStatsV1): { level: DotLevel | "info"; label: string } {
  // Never say "Live" over fixture data.
  if (isSyntheticPreview) return { level: "info", label: "Synthetic preview" };
  if (stats.validators.checked > 0 && stats.validators.online === 0) {
    return { level: "offline", label: "Unreachable" };
  }
  switch (stats.window.status) {
    case "live":
      return { level: "live", label: "Live" };
    case "stalled":
      return { level: "syncing", label: "Stalled" };
    case "unavailable":
      return { level: "offline", label: "No block data" };
    default:
      return { level: "checking", label: "Measuring" };
  }
}

/** The title dot's colour: green when live, amber when stalled, red when down, grey otherwise or in the preview. */
function dotState(stats: NetworkStatsV1): "live" | "stalled" | "down" | "idle" {
  if (isSyntheticPreview) return "idle";
  if (stats.validators.checked > 0 && stats.validators.online === 0) return "down";
  switch (stats.window.status) {
    case "live":
      return "live";
    case "stalled":
      return "stalled";
    case "unavailable":
      return "down";
    default:
      return "idle";
  }
}

function windowShortReason(win: NetworkStatsV1["window"]): string {
  switch (win.status) {
    case "waiting":
      return "reading finalized blocks";
    case "measuring":
      return "measuring the first minute";
    case "stalled":
      return "no new finalized block";
    case "unavailable":
      return "could not read finalized blocks";
    case "live":
      return "in the last minute";
  }
}

/** One plain line under the numbers; the detail is behind the ⓘ. A reason replaces it when rates are hidden. */
function windowLine(win: NetworkStatsV1["window"]): string {
  switch (win.status) {
    case "live":
      return "Counted from the last minute of finalized blocks.";
    case "waiting":
      return "Reading finalized blocks from the validators…";
    case "measuring":
      return `Measuring: ${win.reason ?? "less than a minute of finalized blocks read so far"}. No rate is shown until a full minute is read.`;
    case "stalled":
    case "unavailable":
      return win.reason ?? "No finalized blocks could be read.";
  }
}

function joinLabels(labels: string[]): string {
  if (labels.length <= 1) return labels.join("");
  return `${labels.slice(0, -1).join(", ")} and ${labels[labels.length - 1]}`;
}

function formatWhole(value: number): string {
  return formatInt(Math.round(value));
}

function Missing() {
  return <span className="live-missing">—</span>;
}

/** "Updated 3 s ago", ticking once a second; the exact time is in the tooltip. */
function UpdatedAgo({ asOf }: { asOf: number | null }) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);
  if (asOf === null) {
    return <span data-testid="live-network-updated">Reading the validators…</span>;
  }
  return (
    <span data-testid="live-network-updated" title={`Last read at ${formatClock(asOf)}`}>
      {formatUpdatedAgo(asOf, now)}
    </span>
  );
}

function LiveStat({
  id,
  label,
  note,
  loading = false,
  children,
}: {
  id: string;
  label: string;
  note?: ReactNode;
  loading?: boolean;
  children: ReactNode;
}) {
  // The value carries the testid, not the tile, so a test about a number is
  // not a test about the label and note around it.
  return (
    <div className="live-stat" data-testid={`live-stat-${id}`}>
      <div className="live-label">{label}</div>
      <div className="live-stat-value" data-testid={`live-value-${id}`}>
        {loading ? <Skeleton width="3ch" height="0.8em" /> : children}
      </div>
      {!loading && note && (
        <div className="live-stat-note" data-testid={`live-note-${id}`}>
          {note}
        </div>
      )}
    </div>
  );
}

function ValidatorDots({ validators }: { validators: NetworkStatsV1["validators"]["per_validator"] }) {
  return (
    <ul className="live-validators" aria-label="Validators">
      {validators.map((v) => {
        const state = v.online === null ? "unchecked" : v.online ? "online" : "offline";
        const detail =
          v.online === null
            ? "not checked yet"
            : v.online
              ? `online${v.version ? `, v${v.version}` : ""}`
              : (v.reason ?? "offline");
        return (
          <li
            key={v.validator}
            data-testid={`live-validator-${v.validator}`}
            data-state={state}
            title={`${v.validator}: ${detail}`}
            aria-label={`${v.validator}: ${detail}`}
          >
            <span className={`dot ${state}`} aria-hidden="true" />
            {v.validator}
          </li>
        );
      })}
    </ul>
  );
}

const COUNTER_MS = 600;

/**
 * A number that moves to its new value only when the value changes, and not
 * at all with the OS "reduce motion" setting. While it moves, screen readers
 * get the final value, not the passing ones.
 */
function LiveCounter({
  id,
  value,
  format,
}: {
  id: string;
  value: number;
  format: (value: number) => string;
}) {
  // The in-between value while the counter moves; null at rest, when the
  // counter shows `value` itself. With "reduce motion" it is never set, so a
  // new value is on screen in the same render that receives it.
  const [frame, setFrame] = useState<number | null>(null);
  const onScreen = useRef(value);

  // A layout effect runs before the browser paints, so a moving counter
  // never flashes its new value first.
  useLayoutEffect(() => {
    const from = onScreen.current;
    if (!shouldAnimateCounter(from, value, prefersReducedMotion())) {
      onScreen.current = value;
      setFrame(null);
      return;
    }
    let raf = 0;
    const start = performance.now();
    setFrame(from);
    const step = (time: number) => {
      // A frame's timestamp can precede `start`; never move backwards.
      const progress = Math.max(0, Math.min(1, (time - start) / COUNTER_MS));
      if (progress >= 1) {
        onScreen.current = value;
        setFrame(null);
        return;
      }
      const next = from + (value - from) * (1 - (1 - progress) ** 3);
      onScreen.current = next;
      setFrame(next);
      raf = requestAnimationFrame(step);
    };
    raf = requestAnimationFrame(step);
    return () => cancelAnimationFrame(raf);
  }, [value]);

  return (
    <span
      className="live-counter"
      data-testid={`live-counter-${id}`}
      data-value={value}
      data-animating={frame !== null ? "true" : "false"}
    >
      {frame !== null ? (
        <>
          <span aria-hidden="true">{format(frame)}</span>
          <span className="sr-only">{format(value)}</span>
        </>
      ) : (
        format(value)
      )}
    </span>
  );
}
