import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowUpRight,
  CircleStop,
  ClipboardCheck,
  FileSignature,
  Play,
  RotateCw,
  Sparkles,
  Users,
  Waypoints,
  Wifi,
  WifiOff,
  Zap,
} from "lucide-react";
import { useState } from "react";
import { Card, CardHeader } from "../components/Card";
import { CrashBanner } from "../components/CrashBanner";
import { EmptyState } from "../components/EmptyState";
import { InfoPopover } from "../components/InfoPopover";
import { NumberTicker } from "../components/NumberTicker";
import { ObserverUpgradeBanner } from "../components/ObserverUpgradeBanner";
import { StatusPill } from "../components/StatusPill";
import { api } from "../lib/tauri";
import {
  formatAddress,
  formatHash,
  formatInt,
  formatRelativeTime,
  formatUptime,
} from "../lib/format";
import { useAppStore } from "../lib/store";

const COORDINATOR_LABELS: Record<string, string> = {
  "149.28.32.76": "NYC",
  "140.82.16.112": "LAX",
  "136.244.109.1": "AMS",
  "104.238.171.11": "LHR",
  "202.182.107.41": "NRT",
  "149.28.153.31": "SGP",
};

function coordinatorLabel(url: string): string {
  for (const [ip, label] of Object.entries(COORDINATOR_LABELS)) {
    if (url.includes(ip)) return label;
  }
  return url;
}

export function Dashboard() {
  const queryClient = useQueryClient();
  const identity = useAppStore((s) => s.identity);
  const config = useAppStore((s) => s.config);

  const { data: status } = useQuery({
    queryKey: ["status"],
    queryFn: api.nodeStatus,
    refetchInterval: 1500,
  });
  const { data: earnings } = useQuery({
    queryKey: ["earnings"],
    queryFn: api.fetchEarnings,
    refetchInterval: 3000,
  });
  const { data: attestations } = useQuery({
    queryKey: ["attestations"],
    queryFn: () => api.fetchAttestations(10),
    refetchInterval: 5000,
  });
  const { data: network } = useQuery({
    queryKey: ["network"],
    queryFn: api.fetchNetworkStats,
    refetchInterval: 10_000,
  });

  const startMutation = useMutation({
    mutationFn: () =>
      api.startNode(
        config ?? {
          role: "worker",
          modelPath: null,
          rpcPort: 9090,
          p2pPort: 9091,
          autoStart: true,
          autoUpdate: true,
          dataDir: "~/.arc",
        },
      ),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["status"] });
    },
  });

  const stopMutation = useMutation({
    mutationFn: () => api.stopNode(),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["status"] });
    },
  });

  const restartMutation = useMutation({
    mutationFn: () => api.restartNode(),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["status"] });
    },
  });

  // Wipes <data_dir>/known_peers.json and restarts the node. Most common
  // cause of "I had peers, then I restarted, now I'm stuck" is a stale
  // peer cache pinning to dead seeds. After wiping, the node falls back
  // to the bundled testnet seeds and re-bootstraps cleanly.
  const resetPeersMutation = useMutation({
    mutationFn: () => api.resetPeerState(),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["status"] });
    },
  });

  const running = !!status?.running;
  const isExternal = running && status?.pid == null;
  const isCrashed =
    !!status?.lastError && status.lastError.includes("exited unexpectedly");
  const [addressCopied, setAddressCopied] = useState(false);

  return (
    <div className="main-inner" data-testid="dashboard">
      <div className="page-header">
        <div>
          <h1 className="page-title">Dashboard</h1>
          <p className="page-subtitle">
            {running
              ? isExternal
                ? "Your node is live - managed by the system (launchd / systemd)."
                : "Your node is live. Earnings update every few seconds."
              : "Your node is stopped. Start it to begin earning."}
          </p>
        </div>
        <div style={{ display: "flex", gap: "var(--space-2)" }}>
          {running ? (
            isExternal ? (
              <span
                title="This node is managed by the system. Use `launchctl` (macOS) or `systemctl` (Linux) to stop it."
                className="status-pill info"
                style={{ padding: "8px 14px" }}
                data-testid="external-pill"
              >
                External · read-only
              </span>
            ) : (
              <>
                <button
                  className="btn btn-secondary"
                  onClick={() => restartMutation.mutate()}
                  disabled={restartMutation.isPending}
                  data-testid="btn-restart"
                >
                  <RotateCw size={14} /> Restart
                </button>
                <button
                  className="btn btn-danger"
                  onClick={() => stopMutation.mutate()}
                  disabled={stopMutation.isPending}
                  data-testid="btn-stop"
                >
                  <CircleStop size={14} /> Stop
                </button>
              </>
            )
          ) : (
            <button
              className="btn btn-primary"
              onClick={() => startMutation.mutate()}
              disabled={startMutation.isPending}
              data-testid="btn-start"
            >
              <Play size={14} /> Start node
            </button>
          )}
        </div>
      </div>

      {isCrashed && status?.lastError && (
        <CrashBanner message={status.lastError} />
      )}

      {status?.health === "lite" && status?.coordinatorUrl && (
        <div
          className="lite-banner"
          data-testid="lite-mode-banner"
          role="status"
        >
          <strong>Client mode</strong> — your node has 0 peers, so the app is
          using the public network through{" "}
          {coordinatorLabel(status.coordinatorUrl)}. You can faucet, send, and
          run inference, but{" "}
          <strong>you won&rsquo;t earn ARC until you have at least one peer</strong>.
          The most common cause is a stale peer cache from a prior session
          pinning to seeds that have rotated.
          <div
            style={{
              marginTop: "var(--space-3)",
              display: "flex",
              gap: "var(--space-3)",
              alignItems: "center",
              flexWrap: "wrap",
            }}
          >
            <button
              type="button"
              className="btn-secondary"
              data-testid="reset-peer-state-btn"
              onClick={() => resetPeersMutation.mutate()}
              disabled={resetPeersMutation.isPending}
            >
              {resetPeersMutation.isPending
                ? "Resetting…"
                : "Reset peer state & rebootstrap"}
            </button>
            {resetPeersMutation.data && (
              <span
                style={{
                  color: "var(--text-muted)",
                  fontSize: "var(--text-sm)",
                }}
                data-testid="reset-peer-state-result"
              >
                {resetPeersMutation.data.message}
              </span>
            )}
            {resetPeersMutation.error && (
              <span
                style={{
                  color: "var(--danger)",
                  fontSize: "var(--text-sm)",
                }}
              >
                {String(resetPeersMutation.error)}
              </span>
            )}
          </div>
        </div>
      )}

      <ObserverUpgradeBanner />


      <Card featured style={{ marginBottom: "var(--space-6)" }}>
        <CardHeader
          title="Earnings"
          action={
            earnings?.rank ? (
              <span
                className="status-pill info"
                data-testid="rank-pill"
              >
                Rank #{earnings.rank}
              </span>
            ) : null
          }
        />
        <div
          style={{
            display: "grid",
            gridTemplateColumns: "1fr 1fr",
            gap: "var(--space-6)",
            alignItems: "flex-end",
          }}
        >
          <div>
            <div
              className="big-number gradient"
              data-testid="earnings-total"
            >
              <NumberTicker value={earnings?.totalArc ?? 0} digits={2} />
              <span className="unit">ARC total</span>
            </div>
            <div
              style={{
                marginTop: "var(--space-3)",
                display: "flex",
                gap: "var(--space-5)",
                color: "var(--text-muted)",
                fontSize: "var(--text-sm)",
              }}
            >
              <div>
                <span style={{ color: "var(--success)" }}>+</span>{" "}
                <NumberTicker
                  value={earnings?.todayArc ?? 0}
                  digits={2}
                  suffix=" today"
                />
              </div>
              {!!earnings?.pendingArc && (
                <div>
                  <NumberTicker
                    value={earnings.pendingArc}
                    digits={2}
                    suffix=" pending"
                  />
                </div>
              )}
            </div>
          </div>
          <div>
            <div className="kv">
              <dt>Attestations</dt>
              <dd>{formatInt(earnings?.attestations ?? 0)}</dd>
              <dt>Last payout</dt>
              <dd>
                {earnings?.lastPayoutAt
                  ? formatRelativeTime(earnings.lastPayoutAt)
                  : "-"}
              </dd>
              <dt>Address</dt>
              <dd>
                <button
                  className="btn btn-ghost btn-sm"
                  style={{ padding: "2px 8px" }}
                  onClick={async () => {
                    if (!identity?.address) return;
                    await navigator.clipboard.writeText(identity.address);
                    setAddressCopied(true);
                    setTimeout(() => setAddressCopied(false), 1500);
                  }}
                  data-testid="btn-copy-address"
                >
                  {addressCopied ? (
                    <>
                      <ClipboardCheck size={11} /> Copied
                    </>
                  ) : (
                    <>{identity ? formatAddress(identity.address) : "-"}</>
                  )}
                </button>
              </dd>
            </div>
          </div>
        </div>
      </Card>

      <div
        className="grid-stats"
        style={{ marginBottom: "var(--space-6)" }}
        data-testid="stat-grid"
      >
        <StatTile
          icon={running ? Wifi : WifiOff}
          label="Peers"
          value={formatInt(status?.peers ?? 0)}
          info={{
            title: "Peers",
            children: (
              <p>
                Other nodes your node has an active QUIC connection to.
                More peers means faster sync and better fault tolerance.
                You need at least <code>2</code> peers to serve inference.
              </p>
            ),
          }}
        />
        <StatTile
          icon={Users}
          label="Network nodes"
          value={formatInt(network?.totalNodes ?? 0)}
          info={{
            title: "Network nodes",
            children: (
              <p>
                All validators currently participating in consensus
                network-wide. This is the total size of the ARC testnet.
                Your node is one of them.
              </p>
            ),
          }}
        />
        <StatTile
          icon={Waypoints}
          label="DAG round"
          value={formatInt(status?.round ?? 0)}
          info={{
            title: "DAG round",
            children: (
              <>
                <p>
                  ARC uses a DAG (directed acyclic graph) consensus instead
                  of a single chain. Every round, validators propose blocks
                  in parallel and reach agreement in ~1&thinsp;s.
                </p>
                <p>
                  This number is the current round being voted on - it
                  ticks up continuously as long as the network is live.
                </p>
              </>
            ),
          }}
        />
        <StatTile
          icon={Sparkles}
          label="Uptime"
          value={
            status?.running
              ? formatUptime(status.uptimeSeconds)
              : "-"
          }
          info={{
            title: "Uptime",
            children: (
              <p>
                How long this node has been connected without restart.
                Higher uptime builds your reputation score and increases
                your share of inference requests.
              </p>
            ),
          }}
        />
      </div>

      <div className="grid-main">
        <Card>
          <CardHeader
            title={
              <span style={{ display: "inline-flex", alignItems: "center" }}>
                Recent attestations
                <InfoPopover title="How verifiable inference works">
                  <p>
                    When someone sends a prompt to arc, the network <strong>executes
                    the model end-to-end</strong> - sharded across nodes,
                    each holding a range of transformer layers. Your
                    node runs its slice and passes the hidden state on.
                  </p>
                  <p>
                    The final output is hashed (BLAKE3) with the model
                    weights. A <strong>bonded attestation</strong> carrying{" "}
                    <code>(input_hash, output_hash, model_hash)</code> is
                    posted to the chain - this is what shows up here.
                  </p>
                  <p>
                    A VRF-selected committee of 7 validators re-runs the
                    same input. If ≥5 agree on the hash, the attestation
                    is final. If not, the bond is slashable and dispute
                    resolution runs the inference on-chain via precompile
                    0x0A.
                  </p>
                  <p style={{ color: "var(--text-muted)", fontSize: 11 }}>
                    Inference is deterministic by construction: INT16
                    fixed-point, pure integer math, bit-identical on any
                    chip. On testnet, each settled attestation pays{" "}
                    <code>2.5 ARC</code>.
                  </p>
                </InfoPopover>
              </span>
            }
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
                <span
                  style={{
                    width: 6,
                    height: 6,
                    borderRadius: 999,
                    background: "var(--success)",
                    boxShadow: "0 0 6px var(--success)",
                  }}
                />
                Live
              </span>
            }
          />
          <div className="feed" data-testid="attestation-feed">
            {!attestations || attestations.length === 0 ? (
              <EmptyState
                icon={FileSignature}
                title="No attestations yet"
                description="When your node serves inference, signed receipts appear here."
              />
            ) : (
              attestations.slice(0, 6).map((a) => (
                <div
                  key={a.txHash}
                  className="feed-item"
                  data-testid={`attestation-${a.txHash.slice(0, 10)}`}
                >
                  <div className="feed-item-icon">
                    <Zap />
                  </div>
                  <div className="feed-item-body">
                    <div className="feed-item-title">{a.inputPreview}</div>
                    <div className="feed-item-meta">
                      <span>{a.tokens} tokens</span>
                      <span>{a.latencyMs}ms</span>
                      <span>{formatHash(a.txHash, 8)}</span>
                      <span>{formatRelativeTime(a.timestamp)}</span>
                    </div>
                  </div>
                  <div
                    style={{
                      textAlign: "right",
                      fontFamily: "var(--font-mono)",
                      fontSize: "var(--text-sm)",
                      color: "var(--success)",
                      fontWeight: 600,
                    }}
                  >
                    +{a.rewardArc.toFixed(2)}
                  </div>
                </div>
              ))
            )}
          </div>
        </Card>

        <Card>
          <CardHeader title="Node status" />
          <div className="kv">
            <dt>Health</dt>
            <dd>
              <StatusPill level={status?.health ?? "offline"} />
            </dd>
            <dt>Version</dt>
            <dd>v{status?.version ?? "-"}</dd>
            <dt>Block height</dt>
            <dd>{formatInt(status?.committed ?? 0)}</dd>
            <dt>Round</dt>
            <dd>{formatInt(status?.round ?? 0)}</dd>
            <dt>RPC port</dt>
            <dd className="mono">:{status?.rpcPort ?? "-"}</dd>
            <dt>PID</dt>
            <dd>{status?.pid ?? "-"}</dd>
          </div>
          {status?.lastError && (
            <div
              style={{
                marginTop: "var(--space-4)",
                padding: "var(--space-3) var(--space-4)",
                background: "var(--danger-bg)",
                color: "var(--danger)",
                borderRadius: "var(--radius-sm)",
                fontSize: "var(--text-sm)",
                border: "1px solid rgba(248, 113, 113, 0.2)",
              }}
              data-testid="last-error"
            >
              {status.lastError}
            </div>
          )}

          <div className="divider" />

          <div className="section-heading">
            <h2 style={{ fontSize: "var(--text-base)" }}>Network</h2>
          </div>
          <div className="kv">
            <dt>Total inferences</dt>
            <dd>{formatInt(network?.totalInferences ?? 0)}</dd>
            <dt>Average TPS</dt>
            <dd>{formatInt(network?.avgTps ?? 0)}</dd>
            <dt>Latest block</dt>
            <dd>{formatInt(network?.latestBlock ?? 0)}</dd>
          </div>

          <button
            className="btn btn-secondary"
            style={{
              width: "100%",
              marginTop: "var(--space-4)",
              justifyContent: "center",
            }}
            onClick={() =>
              api.openExternal("http://140.82.16.112:3200")
            }
            data-testid="btn-open-explorer"
          >
            <ArrowUpRight size={14} /> Open network explorer
          </button>
        </Card>
      </div>
    </div>
  );
}

function StatTile({
  icon: Icon,
  label,
  value,
  info,
}: {
  icon: typeof Wifi;
  label: string;
  value: string;
  info?: { title: string; children: React.ReactNode };
}) {
  return (
    <Card hoverable className="stat-tile" data-testid={`stat-${label.toLowerCase().replace(/\s/g, "-")}`}>
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          marginBottom: "var(--space-3)",
        }}
      >
        <span
          className="stat-label"
          style={{ display: "inline-flex", alignItems: "center" }}
        >
          {label}
          {info && (
            <InfoPopover title={info.title}>{info.children}</InfoPopover>
          )}
        </span>
        <Icon size={14} style={{ color: "var(--text-muted)" }} />
      </div>
      <div className="stat-value">{value}</div>
    </Card>
  );
}
