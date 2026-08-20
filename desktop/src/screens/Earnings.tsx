import { useQuery } from "@tanstack/react-query";
import { Calendar, FileSignature, Search } from "lucide-react";
import { useMemo } from "react";
import { Card, CardHeader } from "../components/Card";
import { EmptyState } from "../components/EmptyState";
import { NumberTicker } from "../components/NumberTicker";
import { ProjectedEarnings } from "../components/ProjectedEarnings";
import { api } from "../lib/tauri";
import { formatHash, formatInt, formatRelativeTime } from "../lib/format";
import { useAppStore } from "../lib/store";

/** Testnet flat rate per settled attestation. Mirrors rpc_client.rs. */
const REWARD_PER_ATTESTATION = 2.5;

export function Earnings() {
  const lookupHash = useAppStore((s) => s.lookupHash);
  const { data: earnings } = useQuery({
    queryKey: ["earnings"],
    queryFn: api.fetchEarnings,
    refetchInterval: 3000,
  });
  const { data: attestations } = useQuery({
    queryKey: ["attestations"],
    queryFn: () => api.fetchAttestations(200),
    refetchInterval: 5000,
  });

  // 7-day bucketed earnings from real attestation timestamps.
  //
  // Only attestations that are BOTH the user's own AND carry a genuine
  // timestamp can be bucketed. That is frequently none of them: the live
  // seeds return flat tx records with no timestamp, and the previous
  // synthetic `now - i * 30s` series made every attestation land in today's
  // bucket, producing a chart that looked like a week of activity from a
  // single afternoon's data. When nothing is bucketable the chart is not
  // rendered at all — see `chartIsMeaningful`.
  const { weekly, datedCount } = useMemo(() => {
    const WEEK_LABELS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const now = new Date();
    const todayStart = new Date(
      Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate()),
    ).getTime();
    const DAY_MS = 86_400_000;
    const buckets: Array<{ label: string; value: number; dayStart: number }> =
      [];
    for (let i = 6; i >= 0; i--) {
      const start = todayStart - i * DAY_MS;
      const date = new Date(start);
      buckets.push({
        label: WEEK_LABELS[date.getUTCDay()],
        value: 0,
        dayStart: start,
      });
    }
    let dated = 0;
    for (const a of attestations ?? []) {
      if (a.timestamp == null || a.rewardArc == null) continue;
      const ts = a.timestamp;
      const bucketIdx = buckets.findIndex(
        (b) => ts >= b.dayStart && ts < b.dayStart + DAY_MS,
      );
      if (bucketIdx !== -1) {
        buckets[bucketIdx].value += a.rewardArc;
        dated++;
      }
    }
    return { weekly: buckets, datedCount: dated };
  }, [attestations]);

  const chartIsMeaningful = datedCount > 0;
  const mineCount = (attestations ?? []).filter((a) => a.mine).length;
  const hasEarned = (earnings?.totalArc ?? 0) > 0 || mineCount > 0;

  // Floor at 1 so all-zero weeks still render visible (flat) bars instead
  // of dividing by zero.
  const max = Math.max(1, ...weekly.map((d) => d.value));

  return (
    <div className="main-inner" data-testid="earnings-screen">
      <div className="page-header">
        <div>
          <h1 className="page-title">Earnings</h1>
          <p className="page-subtitle">
            Your share of network rewards, paid in ARC.
          </p>
        </div>
      </div>

      {/* Primary home for the projection. Placed above lifetime totals
          because "what will this earn me" is the question people arrive with,
          and it is the one figure that must never be guessed. */}
      <div style={{ marginBottom: "var(--space-6)" }} data-testid="earnings-projection">
        <ProjectedEarnings />
      </div>

      {/* When nothing has been earned, three cards reading "0.00 ARC" next
          to a "Last 7 days" chart of empty bars told the user nothing and
          implied something had gone wrong. Explain the mechanism instead. */}
      {!hasEarned ? (
        <Card style={{ marginBottom: "var(--space-6)" }} data-testid="earnings-empty">
          <CardHeader title="No earnings yet" />
          <div
            style={{
              color: "var(--text-secondary)",
              fontSize: "var(--text-sm)",
              lineHeight: 1.7,
            }}
          >
            <p style={{ marginTop: 0 }}>
              You earn <strong>{REWARD_PER_ATTESTATION.toFixed(2)} ARC</strong>{" "}
              each time your node serves a slice of an inference request and
              the resulting attestation settles on-chain.
            </p>
            <p>Three things have to be true before that can happen:</p>
            <ol style={{ paddingLeft: "1.2em" }}>
              <li>
                Your node is <strong>running</strong> and has at least one
                peer — check the Dashboard.
              </li>
              <li>
                You downloaded a <strong>model</strong> during setup. Without
                one your node validates consensus but is never sent inference
                work.
              </li>
              <li>
                The network <strong>routed a request to you</strong>. Requests
                are distributed across all workers, so a quiet network means
                quiet earnings.
              </li>
            </ol>
            <p style={{ marginBottom: 0, color: "var(--text-muted)" }}>
              Run a prompt yourself from the Inference tab to put work through
              the network.
            </p>
          </div>
        </Card>
      ) : (
        <div
          style={{
            display: "grid",
            gridTemplateColumns: "repeat(3, 1fr)",
            gap: "var(--space-4)",
            marginBottom: "var(--space-6)",
          }}
        >
          <Card>
            <div className="stat-label">Today</div>
            <div className="big-number" style={{ marginTop: "var(--space-2)" }}>
              {/* null ≠ 0. The chain not reporting a daily figure is not the
                  same as having earned nothing today. */}
              {earnings?.todayArc != null ? (
                <>
                  <NumberTicker value={earnings.todayArc} digits={2} />
                  <span className="unit">ARC</span>
                </>
              ) : (
                <span
                  style={{ color: "var(--text-muted)" }}
                  title="This chain host doesn't report a daily breakdown."
                >
                  —
                </span>
              )}
            </div>
          </Card>
          <Card>
            <div className="stat-label">Lifetime</div>
            <div
              className="big-number gradient"
              style={{ marginTop: "var(--space-2)" }}
            >
              <NumberTicker value={earnings?.totalArc ?? 0} digits={2} />
              <span className="unit">ARC</span>
            </div>
          </Card>
          <Card>
            <div className="stat-label">Last attestation</div>
            {/* Replaces the "Pending" card. Pending was invented client-side
                (min(results,5) × 2.5 / 2) — the chain exposes no such
                figure. The last attestation's block height is real. */}
            <div className="big-number" style={{ marginTop: "var(--space-2)" }}>
              {earnings?.lastPayoutBlock != null ? (
                <span style={{ fontSize: "0.7em" }}>
                  block #{formatInt(earnings.lastPayoutBlock)}
                </span>
              ) : earnings?.lastPayoutAt != null ? (
                <span style={{ fontSize: "0.7em" }}>
                  {formatRelativeTime(earnings.lastPayoutAt)}
                </span>
              ) : (
                <span style={{ color: "var(--text-muted)" }}>—</span>
              )}
            </div>
          </Card>
        </div>
      )}

      {/* Rendered only when at least one of the user's own attestations
          carries a real timestamp. Otherwise there is nothing to plot, and a
          row of empty bars under a "Last 7 days" heading reads as "you
          earned nothing all week" rather than "this data doesn't exist". */}
      {chartIsMeaningful && (
      <Card style={{ marginBottom: "var(--space-6)" }} data-testid="weekly-chart">
        <CardHeader
          title="Last 7 days"
          action={
            <span
              style={{
                display: "inline-flex",
                alignItems: "center",
                gap: 6,
                fontSize: "var(--text-xs)",
                color: "var(--text-muted)",
              }}
              title={`Built from ${datedCount} timestamped attestation${datedCount === 1 ? "" : "s"} credited to you.`}
            >
              <Calendar size={12} /> {datedCount} dated
            </span>
          }
        />
        <div
          style={{
            display: "flex",
            alignItems: "stretch",
            gap: "var(--space-3)",
            height: 200,
            padding: "var(--space-2) 0",
          }}
        >
          {weekly.map((d, i) => {
            const barHeight = Math.max(6, (d.value / max) * 140);
            return (
              <div
                key={i}
                style={{
                  flex: 1,
                  display: "flex",
                  flexDirection: "column",
                  alignItems: "center",
                  justifyContent: "flex-end",
                  gap: "var(--space-2)",
                }}
              >
                <div
                  style={{
                    fontSize: 10,
                    color: "var(--text-muted)",
                    fontVariantNumeric: "tabular-nums",
                    marginBottom: 4,
                  }}
                >
                  {d.value.toFixed(0)}
                </div>
                <div
                  style={{
                    width: "100%",
                    maxWidth: 40,
                    height: barHeight,
                    background: "var(--gradient-earnings)",
                    borderRadius: "var(--radius-sm)",
                    transition: "height 0.6s var(--ease-out)",
                    boxShadow: "0 4px 14px rgba(232, 93, 47, 0.22)",
                  }}
                />
                <div
                  style={{
                    fontSize: 11,
                    color: "var(--text-muted)",
                    letterSpacing: "var(--tracking-wide)",
                    textTransform: "uppercase",
                    fontWeight: 500,
                  }}
                >
                  {d.label}
                </div>
              </div>
            );
          })}
        </div>
      </Card>
      )}

      <Card>
        <CardHeader
          title="All attestations"
          action={
            <span
              style={{
                fontSize: "var(--text-xs)",
                color: "var(--text-muted)",
              }}
            >
              {/* The feed is network-wide; only some rows are the user's.
                  Saying "N total shown" alone implied all of them were. */}
              {formatInt(mineCount)} yours ·{" "}
              {formatInt(attestations?.length ?? 0)} shown
            </span>
          }
        />
        <div className="feed" data-testid="all-attestations">
          {!attestations || attestations.length === 0 ? (
            <EmptyState
              icon={FileSignature}
              title="No attestations yet"
              description="Run inference to start earning."
            />
          ) : (
            attestations.map((a) => (
              <div key={a.txHash} className="feed-item">
                <div className="feed-item-icon">
                  <FileSignature />
                </div>
                <div className="feed-item-body">
                  <div className="feed-item-title">
                    {a.inputPreview || (
                      <span style={{ color: "var(--text-muted)" }}>
                        Inference attestation
                      </span>
                    )}
                  </div>
                  <div className="feed-item-meta">
                    <span>{formatHash(a.txHash, 10)}</span>
                    {a.tokens != null && <span>{a.tokens} tok</span>}
                    {a.latencyMs != null && <span>{a.latencyMs}ms</span>}
                    {a.blockHeight != null && (
                      <span>#{formatInt(a.blockHeight)}</span>
                    )}
                    <span>
                      {a.timestamp != null
                        ? formatRelativeTime(a.timestamp)
                        : "recent"}
                    </span>
                  </div>
                </div>
                <div
                  style={{
                    fontFamily: "var(--font-mono)",
                    fontSize: "var(--text-sm)",
                    color: a.mine ? "var(--success)" : "var(--text-muted)",
                    fontWeight: 600,
                    flexShrink: 0,
                    whiteSpace: "nowrap",
                  }}
                  title={
                    a.mine
                      ? "Credited to your address."
                      : "Submitted by another validator - not your earnings."
                  }
                >
                  {a.rewardArc != null
                    ? `+${a.rewardArc.toFixed(2)} ARC`
                    : "network"}
                </div>
                {/* Was openExternal to a hardcoded LAX :3200 URL — the wrong
                    host for this session and not a block explorer. Resolves
                    against the pinned chain host instead, which is the only
                    place this hash can be confirmed. */}
                <button
                  className="btn btn-ghost btn-sm"
                  onClick={() => lookupHash(a.txHash)}
                  data-testid={`btn-lookup-earnings-${a.txHash.slice(0, 10)}`}
                  aria-label="Look up on the pinned chain host"
                >
                  <Search size={13} />
                </button>
              </div>
            ))
          )}
        </div>
      </Card>
    </div>
  );
}
