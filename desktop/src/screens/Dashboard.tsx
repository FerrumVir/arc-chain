import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowUpRight,
  CircleStop,
  ClipboardCheck,
  FileSignature,
  Loader2,
  Play,
  RotateCw,
  Sparkles,
  Users,
  Waypoints,
  Wifi,
  WifiOff,
  Zap,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Card, CardHeader } from "../components/Card";
import { CrashBanner } from "../components/CrashBanner";
import { EmptyState } from "../components/EmptyState";
import { InfoPopover } from "../components/InfoPopover";
import { NumberTicker } from "../components/NumberTicker";
import { ObserverUpgradeBanner } from "../components/ObserverUpgradeBanner";
import { ProjectedEarnings } from "../components/ProjectedEarnings";
import { StatusPill } from "../components/StatusPill";
import { api } from "../lib/tauri";
import {
  formatAddress,
  formatHash,
  formatInt,
  formatRelativeTime,
  formatUptime,
} from "../lib/format";
import { hostLabel } from "../lib/hosts";
import { useAppStore } from "../lib/store";
import { DEFAULT_NODE_CONFIG } from "../lib/types";

// Host labels live in lib/hosts.ts so every screen names a seed identically.
// "Which host" is load-bearing context for any chain number here, because the
// seeds are independent chains (CLAUDE.md rule 4).

export function Dashboard() {
  const queryClient = useQueryClient();
  const identity = useAppStore((s) => s.identity);
  const config = useAppStore((s) => s.config);
  const setRoute = useAppStore((s) => s.setRoute);

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
  // Whether the OS will bring the node back by itself. Polled rather than
  // assumed from the config flag: the login item is registered separately, so
  // the two can disagree and that disagreement is worth seeing.
  const { data: loginItem } = useQuery({
    queryKey: ["autostart"],
    queryFn: api.getAutostart,
    refetchInterval: 30_000,
  });
  const inferenceActivity = (attestations ?? []).filter((row) =>
    row.txType == null
    || row.txType === "Inference"
    || row.txType === "InferenceAttestation"
    || row.txType === "CommunityInferenceReward");

  const startMutation = useMutation({
    mutationFn: () =>
      api.startNode(config ?? DEFAULT_NODE_CONFIG),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["status"] });
    },
    onError: (err) => {
      console.error("start_node failed:", err);
    },
  });

  const stopMutation = useMutation({
    mutationFn: () => api.stopNode(),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["status"] });
    },
    onError: (err) => {
      console.error("stop_node failed:", err);
    },
  });

  const restartMutation = useMutation({
    mutationFn: () => api.restartNode(),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["status"] });
    },
    onError: (err) => {
      console.error("restart_node failed:", err);
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
  // The node is up but not peered — the state "Reset peer state" exists to
  // fix. Covers both "lite" (a seed is reachable over HTTP) and "syncing"
  // (nothing is).
  const stuckWithoutPeers = running && (status?.peers ?? 0) === 0;
  const isCrashed =
    !!status?.lastError && status.lastError.includes("exited unexpectedly");
  // Spawned but RPC not yet bound. arc-node spends most of its startup time
  // loading the GGUF model into memory (especially in debug builds), during
  // which /health doesn't respond, so status.running stays false even though
  // the child process is alive and working. Without surfacing this, the
  // Start button appears unresponsive for minutes.
  const isStarting =
    (!running && status?.pid != null) || startMutation.isPending;

  const [startedAt, setStartedAt] = useState<number | null>(null);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    if (isStarting && startedAt == null) setStartedAt(Date.now());
    if (running) setStartedAt(null);
  }, [isStarting, running, startedAt]);
  useEffect(() => {
    if (!isStarting) return;
    const id = setInterval(() => setTick((t) => t + 1), 1000);
    return () => clearInterval(id);
  }, [isStarting]);
  const startingElapsedSec = startedAt
    ? Math.floor((Date.now() - startedAt) / 1000)
    : 0;
  void tick;

  const [addressCopied, setAddressCopied] = useState(false);

  const [syncElapsed, setSyncElapsed] = useState(0);
  const syncTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);

  useEffect(() => {
    const isSyncing = !!(status?.running && status?.health === "syncing");
    if (isSyncing && !syncTimerRef.current) {
      setSyncElapsed(0);
      syncTimerRef.current = setInterval(() => setSyncElapsed((s) => s + 1), 1000);
    } else if (!isSyncing && syncTimerRef.current) {
      clearInterval(syncTimerRef.current);
      syncTimerRef.current = null;
      setSyncElapsed(0);
    }
  }, [status?.running, status?.health]);

  useEffect(
    () => () => {
      if (syncTimerRef.current) clearInterval(syncTimerRef.current);
    },
    [],
  );

  return (
    <div className="main-inner" data-testid="dashboard">
      <div className="page-header">
        <div>
          <h1 className="page-title">Dashboard</h1>
          <p className="page-subtitle">
            {running
              ? isExternal
                ? "The node process is running under launchd or systemd. Peering, compatible work, and reward settlement are separate checks."
                : "The node process is running. Peering, compatible work, and reward settlement are separate checks."
              : isStarting
                ? "Starting node — loading model and binding RPC. This can take a few minutes on first run."
                : "Your node is stopped. Start it to sync and make configured compute available."}
          </p>
        </div>
        <div style={{ display: "flex", gap: "var(--space-2)" }}>
          {running ? (
            <>
              {isExternal && (
                <span
                  title="Desktop lost track of the child handle (likely a Tauri rebuild while arc-node kept running). Restart will respawn from this Tauri process."
                  className="status-pill info"
                  style={{ padding: "8px 14px" }}
                  data-testid="external-pill"
                >
                  Detached
                </span>
              )}
              <button
                className="btn btn-secondary"
                onClick={() => restartMutation.mutate()}
                disabled={restartMutation.isPending || isExternal}
                title={isExternal ? "Node is managed externally — stop it first before restarting via desktop" : undefined}
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
          ) : isStarting ? (
            <button
              className="btn btn-primary"
              disabled
              data-testid="btn-starting"
              style={{ opacity: 0.85 }}
            >
              <RotateCw
                size={14}
                style={{ animation: "spin 1s linear infinite" }}
              /> Starting
              {startedAt ? `… ${startingElapsedSec}s` : "…"}
            </button>
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

      {isStarting && (
        <div
          role="status"
          data-testid="starting-banner"
          style={{
            margin: "var(--space-3) 0",
            padding: "12px 16px",
            border: "1px solid rgba(99, 102, 241, 0.4)",
            background: "rgba(99, 102, 241, 0.08)",
            color: "#c7d2fe",
            borderRadius: 8,
            fontSize: 13,
            display: "flex",
            alignItems: "center",
            gap: 12,
          }}
        >
          <RotateCw
            size={16}
            style={{ animation: "spin 1s linear infinite" }}
          />
          <div>
            <strong>Node is starting</strong>
            {startedAt ? ` (${startingElapsedSec}s elapsed)` : ""} — arc-node is
            loading its configured model before binding RPC. Startup time
            depends on the artifact, storage, memory, and CPU. Switch to Logs
            for measured progress from this process.
          </div>
        </div>
      )}

      {isCrashed && status?.lastError && (
        <CrashBanner message={status.lastError} />
      )}

      {(startMutation.error || stopMutation.error || restartMutation.error) && (
        <div
          role="alert"
          data-testid="mutation-error"
          style={{
            margin: "var(--space-3) 0",
            padding: "12px 16px",
            border: "1px solid #ef4444",
            background: "rgba(239, 68, 68, 0.08)",
            color: "#fca5a5",
            borderRadius: 8,
            fontFamily: "ui-monospace, monospace",
            fontSize: 13,
            whiteSpace: "pre-wrap",
          }}
        >
          {String(
            startMutation.error ??
              stopMutation.error ??
              restartMutation.error,
          )}
        </div>
      )}

      {status?.running && status?.health === "syncing" && (
        <div
          className="syncing-banner"
          role="status"
          data-testid="syncing-banner"
        >
          <div
            style={{
              display: "flex",
              alignItems: "center",
              gap: "var(--space-2)",
              marginBottom: "var(--space-1)",
            }}
          >
            <Loader2 size={13} className="spin" />
            <strong>Connecting to peers</strong>
            <span
              style={{
                fontFamily: "var(--font-mono)",
                fontSize: "var(--text-xs)",
                opacity: 0.7,
              }}
            >
              {syncElapsed}s
            </span>
          </div>
          {syncElapsed < 10
            ? "Handshaking with seed nodes…"
            : syncElapsed < 25
              ? "Waiting for QUIC peers to respond…"
              : syncElapsed < 45
                ? "Still connecting — trying all 6 data centers…"
                : "Taking longer than usual. If this persists, try resetting peer state below."}
        </div>
      )}

      {/* Recovery is gated on the actual problem — the node is up but has
          no peers — rather than on the "lite" health level specifically.
          "Reset peer state" is the documented fix for "stuck at 0 peers",
          but it lived inside the lite-mode banner, which was unreachable in
          the shipped app, so the one recovery action the docs point at could
          never be clicked. It now also covers "syncing", which is the state
          a genuinely stuck node sits in. */}
      {stuckWithoutPeers && (
        <div
          className="lite-banner"
          data-testid={
            status?.health === "lite" ? "lite-mode-banner" : "no-peers-banner"
          }
          role="status"
        >
          {status?.coordinatorUrl ? (
            <>
              <strong>Client mode</strong> — your node has 0 peers, so reads and
              inference requests are using {hostLabel(status.coordinatorUrl)}.
              Any balance, faucet response, or transaction is scoped to that
              host. Use the composite explorer to audit canonical agreement and
              preserved forks. This process cannot receive peer-routed community
              work in its current state.
            </>
          ) : (
            <>
              <strong>No peers yet</strong> — your node is running but has not
              completed a handshake with a configured seed. It cannot receive
              peer-routed community work in this state. A peer connection alone
              still does not guarantee assignment or payment.
            </>
          )}{" "}
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
              className="btn btn-secondary"
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
          title="Mined rewards"
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
            <div className="big-number gradient" data-testid="earnings-total">
              {earnings?.fromChain === true ? (
                <>
                  <NumberTicker value={earnings.totalArc} digits={2} />
                  <span className="unit">ARC confirmed</span>
                </>
              ) : (
                <span
                  style={{ color: "var(--text-muted)" }}
                  title="This host did not provide the mined community-reward receipt index."
                >
                  — <span className="unit">not confirmed</span>
                </span>
              )}
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
              {/* Rendered only when the chain actually reports it. It used
                  to fall back to `?? 0`, which showed a confident "+0.00
                  today" for a number nobody knew. */}
              {earnings?.fromChain === true && earnings.todayArc != null && (
                <div>
                  <span style={{ color: "var(--success)" }}>+</span>{" "}
                  <NumberTicker
                    value={earnings.todayArc}
                    digits={2}
                    suffix=" today"
                  />
                </div>
              )}
              {earnings?.fromChain === true &&
                earnings.pendingArc != null &&
                earnings.pendingArc > 0 && (
                <div>
                  <NumberTicker
                    value={earnings.pendingArc}
                    digits={2}
                    suffix=" pending"
                  />
                </div>
              )}
              {earnings && !earnings.fromChain && (
                <div
                  data-testid="dashboard-earnings-unavailable"
                  title={earnings.unavailableReason
                    ?? "Recent inference claims are not evidence of payment."}
                >
                  reward receipt index unavailable — no zero inferred
                </div>
              )}
            </div>
          </div>
          <div>
            <div className="kv">
              <dt>Reward receipts</dt>
              <dd>
                {earnings?.fromChain === true
                  ? formatInt(earnings.attestations)
                  : "—"}
              </dd>
              <dt>Last payout</dt>
              {/* A block height is not a timestamp. Passing
                  `last_attestation_block` (~123,462) to formatRelativeTime
                  rendered "20770d ago" the moment the account earned
                  anything. The two are now separate fields and rendered
                  differently. */}
              <dd data-testid="last-payout">
                {earnings?.lastPayoutAt
                  ? formatRelativeTime(earnings.lastPayoutAt)
                  : earnings?.lastPayoutBlock
                    ? `block #${formatInt(earnings.lastPayoutBlock)}`
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

      {/* Compact projection. The full version, with the treasury ceiling and
          the complete assumptions line, lives on the Earnings screen. */}
      <div style={{ marginBottom: "var(--space-6)" }} data-testid="dashboard-projection">
        <ProjectedEarnings variant="tile" />
      </div>

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
                Peers can improve reachability and fault tolerance, but a peer
                count is not proof of sync, assignment, verification, or
                reward eligibility.
              </p>
            ),
          }}
        />
        <StatTile
          icon={Users}
          label="Host validator records"
          value={formatInt(network?.totalNodes ?? 0)}
          info={{
            title: "Host validator records",
            children: (
              <p>
                Validator records reported by the one chain host selected for
                this session. Treat this as host-scoped unless canonical fleet
                agreement is independently verified.
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
                  This is the selected host&rsquo;s local DAG protocol round.
                  It can advance even while that host stops sealing blocks.
                </p>
                <p>
                  Round progress does not prove block finality or agreement
                  with other sources. Check block age, hash, state root, and the
                  composite recovery boundary before describing the chain as
                  healthy.
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
                How long this node process has run without restart. Uptime is
                operational evidence only; this client does not claim an
                uptime-to-work or uptime-to-reward multiplier.
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
                Recent inference activity
                <InfoPopover title="What this evidence proves">
                  <p>
                    On the protocol-v3 path, a worker is eligible only after it
                    fully loads the exact model artifact requested. The
                    coordinator independently recomputes every token through
                    authenticated 2-of-3 agreement for each layer range.
                  </p>
                  <p>
                    An <code>InferenceAttestation</code> (<code>0x16</code>)
                    carrying{" "}
                    <code>(input_hash, output_hash, model_hash)</code> is
                    a computation claim. A successful block receipt proves
                    inclusion on this host; it does not itself prove payment.
                  </p>
                  <p>
                    Payment is a separate <code>CommunityInferenceReward</code>
                    transaction (<code>0x25</code>). It requires the signed
                    worker certificate, active reward protocol and approval
                    collection, strict greater-than-two-thirds validator
                    identity and stake approval, and a successful mined
                    receipt. With six equal validators, that means five.
                  </p>
                  <p style={{ color: "var(--text-muted)", fontSize: 11 }}>
                    Raw <code>0x16</code> attestations pay nothing. Any
                    policy-reported ARC reward applies only to a successful
                    mined <code>0x25</code> receipt. Settlement fails closed
                    while approval collection is unavailable.
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
                selected host
              </span>
            }
          />
          <div className="feed" data-testid="attestation-feed">
            {inferenceActivity.length === 0 ? (
              <EmptyState
                icon={FileSignature}
                title="No inference claims on this host"
                description="A raw 0x16 claim appears here after submission; payment requires a separate successful mined 0x25 reward receipt."
              />
            ) : (
              inferenceActivity.slice(0, 6).map((a) => (
                <div
                  key={a.txHash}
                  className="feed-item"
                  data-testid={`attestation-${a.txHash.slice(0, 10)}`}
                >
                  <div className="feed-item-icon">
                    <Zap />
                  </div>
                  <div className="feed-item-body">
                    {/* The live seeds return flat tx records with no prompt
                        text. Say so rather than rendering an empty title. */}
                    <div className="feed-item-title">
                      {a.inputPreview || (
                        <span style={{ color: "var(--text-muted)" }}>
                          Inference activity
                        </span>
                      )}
                    </div>
                    <div className="feed-item-meta">
                      {/* Each of these is omitted when unknown, instead of
                          being printed as a confident "0 tokens / 0ms". */}
                      {a.tokens != null && <span>{a.tokens} tokens</span>}
                      {a.latencyMs != null && <span>{a.latencyMs}ms</span>}
                      <span>{formatHash(a.txHash, 8)}</span>
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
                      textAlign: "right",
                      fontFamily: "var(--font-mono)",
                      fontSize: "var(--text-sm)",
                      color: a.mine ? "var(--success)" : "var(--text-muted)",
                      fontWeight: 600,
                      whiteSpace: "nowrap",
                    }}
                    data-testid={`activity-status-${a.txHash.slice(0, 10)}`}
                    title={a.paid
                      ? "Successful mined CommunityInferenceReward (0x25) receipt."
                      : "Successful computation claim; this row is not a payment."}
                  >
                    {a.paid
                      ? "COMPUTED + PAID"
                      : `COMPUTED · NOT PAYMENT · ${a.mine ? "yours" : "network"}`}
                  </div>
                </div>
              ))
            )}
          </div>
        </Card>

        <Card>
          {/* Everything in this card is YOUR node, read from
              127.0.0.1:<rpcPort>. It used to be a remote seed's numbers. */}
          <CardHeader title="Your node" />
          <div className="kv">
            <dt>Health</dt>
            <dd>
              <StatusPill level={status?.health ?? "offline"} />
            </dd>
            <dt>Version</dt>
            <dd>{status?.running ? `v${status.version}` : "-"}</dd>
            <dt>Block height</dt>
            <dd>{status?.running ? formatInt(status.committed) : "-"}</dd>
            <dt>Round</dt>
            <dd>{status?.running ? formatInt(status.round) : "-"}</dd>
            <dt>Cores</dt>
            <dd data-testid="compute-width">
              {status?.running
                ? status.workerThreads != null
                  ? `${status.workerThreads} of ${status.cpuCores ?? "?"}`
                  : `all${status.cpuCores ? ` (${status.cpuCores})` : ""}`
                : "-"}
            </dd>
            <dt>RPC port</dt>
            <dd className="mono">:{status?.rpcPort ?? "-"}</dd>
            <dt>PID</dt>
            <dd>{status?.pid ?? "-"}</dd>
            {/* The owner's first question: does this survive a restart on its
                own? Answered here from the two things that decide it — the
                config flag and the OS login item — rather than implied. */}
            <dt>Starts with OS</dt>
            <dd data-testid="dashboard-persistence">
              {(config?.autoStart ?? DEFAULT_NODE_CONFIG.autoStart)
                ? loginItem === true
                  ? "yes"
                  : loginItem === false
                    ? "set, but no login item"
                    : "not verified"
                : "no"}
            </dd>
          </div>
          <p
            style={{
              marginTop: "var(--space-3)",
              marginBottom: 0,
              fontSize: "var(--text-xs)",
              color: "var(--text-muted)",
              lineHeight: 1.6,
            }}
            data-testid="dashboard-persistence-note"
          >
            {(config?.autoStart ?? DEFAULT_NODE_CONFIG.autoStart) &&
            loginItem === true ? (
              <>
                The OS login item is registered and ARC is configured to start
                the node when the app opens. After login it resumes as{" "}
                {config?.modelPath ? "a worker candidate" : "an observer"}
                {config?.modelPath
                  ? " — it advertises compute only after the exact model artifact loads completely; assignment and payment remain separate."
                  : " — no model is configured, so it cannot execute local model inference."}{" "}
                Verify process and peer state after every restart.
              </>
            ) : (config?.autoStart ?? DEFAULT_NODE_CONFIG.autoStart) ? (
              loginItem === false ? (
                <>
                  Start-on-app-launch is enabled, but no OS login item is
                  registered. ARC will not reopen automatically after login;
                  open it manually or repair the login item in Settings.
                </>
              ) : (
                <>
                  Start-on-app-launch is enabled, but OS registration has not
                  been verified. Do not assume this node resumes after login.
                </>
              )
            ) : (
              <>
                Auto-start is off, so nothing resumes after a reboot until you
                press Start. Turn it on in Settings.
              </>
            )}
          </p>
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
            <h2 style={{ fontSize: "var(--text-base)" }}>
              Network
              {status?.chainHost && (
                <span
                  style={{
                    marginLeft: 8,
                    fontSize: "var(--text-xs)",
                    color: "var(--text-muted)",
                    fontWeight: 400,
                  }}
                >
                  via {hostLabel(status.chainHost)}
                </span>
              )}
            </h2>
          </div>
          <div className="kv">
            <dt>Host inference records</dt>
            <dd>
              {network?.totalInferences != null
                ? formatInt(network.totalInferences)
                : "—"}
            </dd>
            <dt>Host-reported TPS</dt>
            <dd>
              {network?.avgTps != null ? formatInt(network.avgTps) : "—"}
            </dd>
            <dt>Host block height</dt>
            <dd>
              {network?.latestBlock != null
                ? formatInt(network.latestBlock)
                : "—"}
            </dd>
            {/* Block production has been stalled on most seeds for days.
                `/health` still reports "ok" because DAG rounds keep
                advancing, so without this the network looks healthy. */}
            {status?.chainBlockAgeSeconds != null && (
              <>
                <dt>Last block</dt>
                <dd
                  data-testid="chain-block-age"
                  style={{
                    color:
                      status.chainBlockAgeSeconds > 3600
                        ? "var(--warning)"
                        : undefined,
                  }}
                >
                  {formatUptime(status.chainBlockAgeSeconds)} ago
                </dd>
              </>
            )}
          </div>

          {/* Was `openExternal("http://140.82.16.112:3200")` labelled "Open
              network explorer". Three things were wrong with that: the IP was
              hardcoded to LAX rather than the seed this session actually reads,
              :3200 is a network dashboard and not a block explorer, and the
              deployed page carries dead tiles and stale copy. The in-app
              Network screen reads the pinned host and can be trusted. */}
          <button
            className="btn btn-secondary"
            style={{
              width: "100%",
              marginTop: "var(--space-4)",
              justifyContent: "center",
            }}
            onClick={() => setRoute("network")}
            data-testid="btn-open-network"
          >
            <ArrowUpRight size={14} /> Check the chain
            {status?.chainHost && <> ({hostLabel(status.chainHost)})</>}
          </button>
        </Card>
      </div>

      <style>{`@keyframes spin { to { transform: rotate(360deg); } }`}</style>
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
