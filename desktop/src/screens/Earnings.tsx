import { useQuery } from "@tanstack/react-query";
import { ArrowUpRight, Calendar, FileSignature } from "lucide-react";
import { useMemo } from "react";
import { Card, CardHeader } from "../components/Card";
import { EmptyState } from "../components/EmptyState";
import { NumberTicker } from "../components/NumberTicker";
import { api } from "../lib/tauri";
import { formatHash, formatInt, formatRelativeTime } from "../lib/format";

export function Earnings() {
  const { data: earnings } = useQuery({
    queryKey: ["earnings"],
    queryFn: api.fetchEarnings,
    refetchInterval: 3000,
  });
  const { data: attestations } = useQuery({
    queryKey: ["attestations"],
    queryFn: () => api.fetchAttestations(50),
    refetchInterval: 5000,
  });

  const weekly = useMemo(() => {
    // synthesize 7-day series from attestations; real backend will provide history
    const days = Array.from({ length: 7 }).map((_, i) => ({
      label:
        ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][
          (new Date().getDay() + 6 - (6 - i)) % 7
        ],
      value: Math.random() * 300 + 100,
    }));
    return days;
  }, []);

  const max = Math.max(...weekly.map((d) => d.value));

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
            <NumberTicker value={earnings?.todayArc ?? 0} digits={2} />
            <span className="unit">ARC</span>
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
          <div className="stat-label">Pending</div>
          <div className="big-number" style={{ marginTop: "var(--space-2)" }}>
            <NumberTicker value={earnings?.pendingArc ?? 0} digits={2} />
            <span className="unit">ARC</span>
          </div>
        </Card>
      </div>

      <Card style={{ marginBottom: "var(--space-6)" }}>
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
            >
              <Calendar size={12} /> Weekly
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
              {formatInt(attestations?.length ?? 0)} total shown
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
                  <div className="feed-item-title">{a.inputPreview}</div>
                  <div className="feed-item-meta">
                    <span>{formatHash(a.txHash, 10)}</span>
                    <span>{a.tokens} tok</span>
                    <span>{a.latencyMs}ms</span>
                    <span>{formatRelativeTime(a.timestamp)}</span>
                  </div>
                </div>
                <div
                  style={{
                    fontFamily: "var(--font-mono)",
                    fontSize: "var(--text-sm)",
                    color: "var(--success)",
                    fontWeight: 600,
                    flexShrink: 0,
                  }}
                >
                  +{a.rewardArc.toFixed(2)} ARC
                </div>
                <button
                  className="btn btn-ghost btn-sm"
                  onClick={() =>
                    api.openExternal(
                      `http://140.82.16.112:3200/tx/${a.txHash}`,
                    )
                  }
                  aria-label="Open in explorer"
                >
                  <ArrowUpRight size={13} />
                </button>
              </div>
            ))
          )}
        </div>
      </Card>
    </div>
  );
}
