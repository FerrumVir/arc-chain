import { useQuery } from "@tanstack/react-query";
import { Activity, ArrowUpRight, Globe, Server, Zap } from "lucide-react";
import { Card, CardHeader } from "../components/Card";
import { NumberTicker } from "../components/NumberTicker";
import { api } from "../lib/tauri";
import { formatInt } from "../lib/format";

export function Network() {
  const { data: network } = useQuery({
    queryKey: ["network"],
    queryFn: api.fetchNetworkStats,
    refetchInterval: 5000,
  });
  const { data: status } = useQuery({
    queryKey: ["status"],
    queryFn: api.nodeStatus,
    refetchInterval: 2000,
  });

  return (
    <div className="main-inner" data-testid="network-screen">
      <div className="page-header">
        <div>
          <h1 className="page-title">Network</h1>
          <p className="page-subtitle">
            You're one of {formatInt(network?.totalNodes ?? 0)} nodes keeping
            ARC alive.
          </p>
        </div>
        <button
          className="btn btn-secondary"
          onClick={() => api.openExternal("http://140.82.16.112:3200")}
          data-testid="btn-open-explorer-network"
        >
          <ArrowUpRight size={14} /> Explorer
        </button>
      </div>

      <div className="grid-stats" style={{ marginBottom: "var(--space-6)" }}>
        <Card>
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              marginBottom: "var(--space-3)",
            }}
          >
            <span className="stat-label">Total nodes</span>
            <Server size={14} style={{ color: "var(--text-muted)" }} />
          </div>
          <div className="stat-value">
            <NumberTicker value={network?.totalNodes ?? 0} digits={0} />
          </div>
        </Card>
        <Card>
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              marginBottom: "var(--space-3)",
            }}
          >
            <span className="stat-label">Inferences served</span>
            <Zap size={14} style={{ color: "var(--text-muted)" }} />
          </div>
          <div className="stat-value">
            <NumberTicker
              value={network?.totalInferences ?? 0}
              digits={0}
            />
          </div>
        </Card>
        <Card>
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              marginBottom: "var(--space-3)",
            }}
          >
            <span className="stat-label">Average TPS</span>
            <Activity size={14} style={{ color: "var(--text-muted)" }} />
          </div>
          <div className="stat-value">
            <NumberTicker value={network?.avgTps ?? 0} digits={0} />
          </div>
        </Card>
        <Card>
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              marginBottom: "var(--space-3)",
            }}
          >
            <span className="stat-label">Latest block</span>
            <Globe size={14} style={{ color: "var(--text-muted)" }} />
          </div>
          <div className="stat-value">
            <NumberTicker value={network?.latestBlock ?? 0} digits={0} />
          </div>
        </Card>
      </div>

      <Card>
        <CardHeader title="Your position" />
        <div
          style={{
            display: "grid",
            gridTemplateColumns: "1fr 1fr",
            gap: "var(--space-6)",
          }}
        >
          <div>
            <div className="stat-label">Peers connected</div>
            <div
              className="big-number"
              style={{ marginTop: "var(--space-2)" }}
            >
              <NumberTicker value={status?.peers ?? 0} digits={0} />
              <span className="unit">
                / {formatInt((network?.totalNodes ?? 1) - 1)}
              </span>
            </div>
            <div
              className="progress"
              style={{ marginTop: "var(--space-3)" }}
            >
              <div
                className="progress-fill"
                style={{
                  width: `${Math.min(
                    100,
                    ((status?.peers ?? 0) / Math.max(1, network?.totalNodes ?? 1)) *
                      100 *
                      20,
                  )}%`,
                }}
              />
            </div>
          </div>
          <div>
            <div className="stat-label">Sync</div>
            <div
              className="big-number"
              style={{ marginTop: "var(--space-2)" }}
            >
              {status?.committed === network?.latestBlock ? "100" : "—"}
              <span className="unit">%</span>
            </div>
            <div
              style={{
                marginTop: "var(--space-3)",
                fontSize: "var(--text-sm)",
                color: "var(--text-muted)",
              }}
            >
              Round {formatInt(status?.round ?? 0)} · committed{" "}
              {formatInt(status?.committed ?? 0)}
            </div>
          </div>
        </div>
      </Card>
    </div>
  );
}
