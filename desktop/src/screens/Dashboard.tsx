import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowUpRight,
  CircleStop,
  ClipboardCheck,
  FileSignature,
  Loader2,
  Play,
  Power,
  RotateCw,
  Sparkles,
  Users,
  Waypoints,
  Wifi,
  Zap,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { Card, CardHeader } from "../components/Card";
import { StartStage } from "../components/StartStage";
import { Skeleton } from "../components/Skeleton";
import { EASE_OUT } from "../lib/motion";
import { prefersReducedMotion } from "../components/Engraving";
import type { ArchChainPhase } from "../lib/aqueduct/archchain";
import { CrashBanner } from "../components/CrashBanner";
import { ProblemNotice } from "../components/ProblemNotice";
import { StartFailurePanel } from "../components/StartFailurePanel";
import { EmptyState } from "../components/EmptyState";
import { InfoPopover } from "../components/InfoPopover";
import { ModelUnavailableBanner } from "../components/ModelUnavailableBanner";
import { NumberTicker } from "../components/NumberTicker";
import { ObserverUpgradeBanner } from "../components/ObserverUpgradeBanner";
import { WorkerJobsCard } from "../components/WorkerJobs";
import { ProjectedEarnings } from "../components/ProjectedEarnings";
import { StatusPill } from "../components/StatusPill";
import { api } from "../lib/tauri";
import { CONNECTING_LONG_SECS, describeConnection, isUnreachableMessage } from "../lib/connection";
import {
  formatAddress,
  formatHash,
  formatInt,
  formatRelativeTime,
  formatUptime,
} from "../lib/format";
import { hostLabel } from "../lib/hosts";
import { isUnfinishedUpdate } from "../lib/node-strip";
import { PLAIN_COPY } from "../lib/plain-copy";
import { quitArc } from "../lib/quit";
import { classifyStartFailure } from "../lib/start-failure";
import { useAppStore } from "../lib/store";
import { DEFAULT_NODE_CONFIG } from "../lib/types";
import { useUpdaterSnapshot } from "../lib/updater";

// Host labels live in lib/hosts.ts so every screen names a seed identically.
// "Which host" is load-bearing context for any chain number here, because the
// seeds are independent chains (CLAUDE.md rule 4).

// A running node that reaches nothing for this long steps out of the start-up stage, so the dashboard and its
// connection line take over instead of the stage holding the whole screen.
const PEER_WAIT_MS = 45_000;
// How long the stage shows "Your node is live" when start-up finishes, before it steps aside.
const LIVE_MOMENT_MS = 2_600;

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
  // Share Network's host overview so chain height and block age come from the
  // same current source. `nodeStatus` only has the age from the first host
  // election; using it here lets that timestamp grow stale while height moves.
  const {
    data: networkOverview,
    isError: networkOverviewError,
  } = useQuery({
    queryKey: ["network-overview"],
    queryFn: api.fetchNetworkOverview,
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
  const chainHeight = networkOverviewError
    ? "Unavailable"
    : networkOverview?.height != null
      ? formatInt(networkOverview.height)
      : "—";
  const chainBlockAge = networkOverviewError
    ? "Unavailable"
    : networkOverview
      ? networkOverview.lastBlockAgeSecs != null
        ? `${formatUptime(networkOverview.lastBlockAgeSecs)} ago`
        : "Unavailable"
      : "—";
  const chainBlockAgeIsStale =
    !networkOverviewError &&
    networkOverview?.lastBlockAgeSecs != null &&
    networkOverview.lastBlockAgeSecs > 3600;
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

  // The crash banner's "Start again": clear the crash, then start. The error
  // lives here, not in the banner, because clearing the crash unmounts the
  // banner; a refused start is shown below as the start-failure panel, the
  // way Home's strip keeps its Start errors.
  const startAgainMutation = useMutation({
    mutationFn: async () => {
      await api.clearCrash();
      await api.startNode(config ?? DEFAULT_NODE_CONFIG);
    },
    onSettled: () => {
      queryClient.invalidateQueries({ queryKey: ["status"] });
    },
  });

  const running = !!status?.running;
  // A start error is moot once the node runs, and a stop error once it has exited.
  useEffect(() => {
    if (running) {
      startMutation.reset();
      restartMutation.reset();
      startAgainMutation.reset();
    } else {
      stopMutation.reset();
    }
    // The reset functions are stable; only the node's state decides.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [running]);
  // The one start or restart that failed, if any, in the order the buttons sit on this screen.
  const startFailure = startMutation.error ?? restartMutation.error ?? startAgainMutation.error ?? null;
  // An update ARC couldn't finish keeps the node until ARC exits, and Start and Restart are refused, so this screen
  // offers what Home's node strip does: Quit ARC. The updater's own state says so, or a refused Start does when this
  // window no longer has that state (after a reload).
  const update = useUpdaterSnapshot();
  const updateUnfinished =
    isUnfinishedUpdate(update) ||
    (startFailure !== null && classifyStartFailure(String(startFailure)) === "update-unfinished");
  const [quitting, setQuitting] = useState(false);
  const isExternal = running && status?.pid == null;
  // Nothing is known until the first status arrives: the node may be running, or auto-starting in the background, so
  // the screen says "Checking" rather than "stopped" and offers no Start button yet.
  const statusKnown = status !== undefined;
  // How the node reaches the network, in plain words (lib/connection.ts). A community node has no peers by
  // design (arc-net admits only the validators), so zero peers with a coordinator answering is connected, not
  // stuck. Through v0.8.14 that state carried three banners: "HTTP client mode", "Client mode" with a
  // "Reset peer state" button, and "Connecting to peers" counting up.
  const connection = describeConnection(status);
  const connecting = running && connection.state === "connecting";
  const connectedViaCoordinator =
    running && connection.state === "connected" && (status?.peers ?? 0) === 0;
  const isCrashed =
    !!status?.lastError && status.lastError.includes("exited unexpectedly");
  // The status's own "nothing answered" text is this computer's connection, told by the subtitle; any other
  // lastError that is not a crash is a start the app made by itself that failed, told like a refused Start.
  const reportedStartFailure =
    !running && !isCrashed && !isUnreachableMessage(status?.lastError) ? status?.lastError ?? null : null;
  // Spawned but RPC not yet bound. arc-node spends most of its startup time
  // loading the GGUF model into memory (especially in debug builds), during
  // which /health doesn't respond, so status.running stays false even though
  // the child process is alive and working. Without surfacing this, the
  // Start button appears unresponsive for minutes.
  const isStarting =
    (!running && status?.pid != null) || startMutation.isPending || startAgainMutation.isPending;

  // The node's start, drawn as the first arch of an aqueduct: every step is one the node actually reports. No model-load
  // percentage exists, so that step shows work under way without claiming progress; catching up is drawn from the real
  // local and chain heights. Once caught up, the Network screen carries the live aqueduct.
  const localHeight = status?.height ?? null;
  // the chain's tip as the node reports it (the display string chainHeight above is the network overview's)
  const tipHeight = status?.chainHeight ?? null;
  const catchingUp =
    running && localHeight != null && tipHeight != null && tipHeight - localHeight > 50;
  // "running" means the node's RPC already answers, so a running node that has reached nothing yet is connecting,
  // not opening its port. With no model configured (an observer) there is no model to load: that process is
  // opening its port.
  const findingPeers = running && (status?.peers ?? 0) === 0 && !catchingUp;
  const [peerWaitOver, setPeerWaitOver] = useState(false);
  useEffect(() => {
    if (!findingPeers) {
      setPeerWaitOver(false);
      return;
    }
    const timer = setTimeout(() => setPeerWaitOver(true), PEER_WAIT_MS);
    return () => clearTimeout(timer);
  }, [findingPeers]);
  const startupPhase: ArchChainPhase | null =
    startMutation.isPending && status?.pid == null
      ? "survey"
      : isStarting
        ? config?.modelPath && !status?.modelUnavailable
          ? "model"
          : "rpc"
        : findingPeers && !peerWaitOver && !status?.coordinatorUrl
          ? "peers"
          : catchingUp
            ? "sync"
            : null;
  // When start-up finishes connected (a peer, or a coordinator for a community node), the stage says so for a
  // moment ("Your node is live"), then steps aside.
  const [liveUntil, setLiveUntil] = useState<number | null>(null);
  const prevStartupPhase = useRef<ArchChainPhase | null>(null);
  const connected = running && connection.state === "connected";
  useEffect(() => {
    const was = prevStartupPhase.current;
    prevStartupPhase.current = startupPhase;
    if (was && !startupPhase && connected) {
      setLiveUntil(Date.now() + LIVE_MOMENT_MS);
    }
  }, [startupPhase, connected]);
  useEffect(() => {
    if (liveUntil == null) return;
    const timer = setTimeout(() => setLiveUntil(null), Math.max(0, liveUntil - Date.now()));
    return () => clearTimeout(timer);
  }, [liveUntil]);
  const startPhase: ArchChainPhase | null =
    startupPhase ?? (liveUntil != null && running ? "live" : null);

  const detailsRef = useRef<HTMLDivElement>(null);
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

  // How long the running node has been reaching for the network without an answer.
  const [syncElapsed, setSyncElapsed] = useState(0);
  const syncTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);

  useEffect(() => {
    if (connecting && !syncTimerRef.current) {
      setSyncElapsed(0);
      syncTimerRef.current = setInterval(() => setSyncElapsed((s) => s + 1), 1000);
    } else if (!connecting && syncTimerRef.current) {
      clearInterval(syncTimerRef.current);
      syncTimerRef.current = null;
      setSyncElapsed(0);
    }
  }, [connecting]);

  useEffect(
    () => () => {
      if (syncTimerRef.current) clearInterval(syncTimerRef.current);
    },
    [],
  );

  const controls = (
        <div style={{ display: "flex", gap: "var(--space-2)" }}>
          {updateUnfinished ? (
            <button
              className="btn btn-primary"
              onClick={() => {
                setQuitting(true);
                void quitArc().finally(() => setQuitting(false));
              }}
              disabled={quitting}
              aria-busy={quitting || undefined}
              data-testid="btn-quit-arc"
            >
              <Power size={14} /> {PLAIN_COPY.actions.quitArc}
            </button>
          ) : !statusKnown ? (
            <button className="btn btn-secondary" disabled aria-busy="true" data-testid="btn-checking">
              <RotateCw size={14} className="spin" /> Checking…
            </button>
          ) : running ? (
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
                aria-busy={restartMutation.isPending || undefined}
              >
                <RotateCw size={14} className={restartMutation.isPending ? "spin" : undefined} />
                {restartMutation.isPending ? " Restarting…" : " Restart"}
              </button>
              <button
                className="btn btn-danger"
                onClick={() => stopMutation.mutate()}
                disabled={stopMutation.isPending}
                data-testid="btn-stop"
                aria-busy={stopMutation.isPending || undefined}
              >
                {stopMutation.isPending ? (
                  <>
                    <RotateCw size={14} className="spin" /> Stopping…
                  </>
                ) : (
                  <>
                    <CircleStop size={14} /> Stop
                  </>
                )}
              </button>
            </>
          ) : isStarting ? (
            <button
              className="btn btn-primary"
              disabled
              data-testid="btn-starting"
              style={{ opacity: 0.85 }}
            >
              <RotateCw size={14} className="spin" /> Starting
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
  );

  return (
    <div className="main-inner" data-testid="dashboard">
      <AnimatePresence initial={false}>
        {startPhase && (
          <motion.div
            key="start-stage"
            style={{ overflow: "hidden" }}
            initial={{ opacity: 0, height: 0 }}
            animate={{ opacity: 1, height: "auto", transition: { duration: 0.42, ease: EASE_OUT } }}
            exit={{ opacity: 0, height: 0, transition: { duration: 0.42, ease: EASE_OUT } }}
          >
            <StartStage
              phase={startPhase}
              localHeight={localHeight}
              chainHeight={tipHeight}
              actions={controls}
              onDetails={() => detailsRef.current?.scrollIntoView({ block: "start", behavior: prefersReducedMotion() ? "auto" : "smooth" })}
            />
          </motion.div>
        )}
      </AnimatePresence>
      <div className="page-header" ref={detailsRef}>
        <div>
          <h1 className="page-title">Dashboard</h1>
          <p className="page-subtitle" data-testid="dashboard-subtitle">
            {updateUnfinished
              ? PLAIN_COPY.nodeStrip.updateFailed
              : !statusKnown
                ? PLAIN_COPY.dashboard.subtitleChecking
                : running
                  ? isExternal
                    ? PLAIN_COPY.dashboard.subtitleExternal
                    : connection.state === "connected"
                      ? PLAIN_COPY.nodeStrip.runningConnected
                      : PLAIN_COPY.nodeStrip.runningConnecting
                  : isStarting
                    ? PLAIN_COPY.dashboard.subtitleStarting
                    : connection.state === "unreachable"
                      ? PLAIN_COPY.connection.unreachable
                      : PLAIN_COPY.dashboard.subtitleOff}
          </p>
        </div>
        {!startPhase && controls}
      </div>

      {isStarting && (
        <div className="connection-notice" role="status" data-testid="starting-banner">
          <RotateCw size={16} className="spin" aria-hidden="true" />
          <span>
            {PLAIN_COPY.dashboard.startingBanner(startedAt ? ` (${startingElapsedSec}s)` : "")}
          </span>
        </div>
      )}

      {isCrashed && status?.lastError && (
        <CrashBanner
          message={status.lastError}
          onStartAgain={() => startAgainMutation.mutate()}
          starting={startAgainMutation.isPending}
        />
      )}

      {status?.modelUnavailable && (
        <ModelUnavailableBanner reason={status.modelUnavailable} />
      )}

      {/* The header carries an update ARC couldn't finish, as one sentence and Quit ARC. */}
      {startFailure && !updateUnfinished && (
        <StartFailurePanel
          error={String(startFailure)}
          onTryAgain={() =>
            startMutation.error
              ? startMutation.mutate()
              : restartMutation.error
                ? restartMutation.mutate()
                : startAgainMutation.mutate()
          }
          testId="mutation-error"
        />
      )}

      {/* A start the app made by itself (at launch, after a crash) that failed reads like a refused Start: one
          sentence, one action, the native text behind Show details. Not shown twice when a pressed Start failed
          the same way. */}
      {reportedStartFailure && !startFailure && !updateUnfinished && (
        <StartFailurePanel
          error={reportedStartFailure}
          onTryAgain={() => startMutation.mutate()}
          testId="last-error"
        />
      )}

      {/* Only while the node is still running: once it has exited, a failed Stop is moot (and reset above). */}
      {stopMutation.error && running && !startFailure && (
        <ProblemNotice
          testId="mutation-error"
          title={PLAIN_COPY.dashboard.stopFailed}
          body={PLAIN_COPY.dashboard.stopFailedBody}
          details={String(stopMutation.error)}
          actions={
            <button
              type="button"
              className="btn btn-primary btn-sm"
              onClick={() => stopMutation.mutate()}
              disabled={stopMutation.isPending}
              data-testid="btn-stop-again"
            >
              {PLAIN_COPY.actions.tryAgain}
            </button>
          }
        />
      )}

      {/* The connection, as status. A community node connects through a coordinator and has no peers, which is
          its normal state; while nothing has answered yet the line counts the wait, and after a while it says to
          check the internet connection. Nothing here is a fault, so nothing here is red. */}
      {connectedViaCoordinator && (
        <div
          className="connection-notice"
          role="status"
          data-testid="connection-notice"
          data-state="connected"
        >
          <Wifi size={15} aria-hidden="true" />
          <span>{connection.sentence}</span>
        </div>
      )}

      {connecting && (
        <div
          className="connection-notice"
          role="status"
          data-testid="connection-notice"
          data-state="connecting"
        >
          <Loader2 size={15} className="spin" aria-hidden="true" />
          <strong>{PLAIN_COPY.connection.labels.connecting} to the ARC network</strong>
          <span className="connection-elapsed">{syncElapsed}s</span>
          <span className="connection-body">
            {syncElapsed < CONNECTING_LONG_SECS
              ? PLAIN_COPY.connection.connecting
              : PLAIN_COPY.connection.connectingLong}
          </span>
        </div>
      )}

      <ObserverUpgradeBanner />

      <WorkerJobsCard />


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
              {earnings === undefined ? (
                <Skeleton width="6ch" height="0.8em" />
              ) : earnings.fromChain === true ? (
                <>
                  <NumberTicker value={earnings.totalArc} digits={2} />
                  <span className="unit">
                    {earnings.historyCompleteSinceRecovery === true
                      ? "ARC confirmed since recovery"
                      : "ARC confirmed in retained window"}
                  </span>
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
                {earnings === undefined ? (
                  <Skeleton width="3ch" />
                ) : earnings.fromChain === true ? (
                  formatInt(earnings.attestations)
                ) : (
                  "—"
                )}
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
          icon={Wifi}
          label="Peers"
          value={status ? formatInt(status.peers) : <Skeleton width="2.5ch" height="0.8em" />}
          info={{
            title: "Peers",
            children: <p>{PLAIN_COPY.connection.peersInfo}</p>,
          }}
        />
        <StatTile
          icon={Users}
          label="Host validator records"
          value={network ? formatInt(network.totalNodes) : <Skeleton width="2.5ch" height="0.8em" />}
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
          value={status ? formatInt(status.round) : <Skeleton width="4ch" height="0.8em" />}
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
            {attestations === undefined ? (
              <div className="feed-loading" aria-busy="true" data-testid="attestation-feed-loading">
                {[0, 1, 2].map((i) => (
                  <div key={i} className="feed-skeleton-row">
                    <Skeleton width={i === 1 ? "48%" : "62%"} />
                    <Skeleton width="18%" />
                  </div>
                ))}
              </div>
            ) : inferenceActivity.length === 0 ? (
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
                  data-tx-hash={a.txHash}
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
            <dt>Connection</dt>
            <dd>
              <StatusPill level={connection.level} label={connection.label} />
            </dd>
            <dt>Version</dt>
            <dd>{status?.running ? `v${status.version}` : "-"}</dd>
            <dt>Block height</dt>
            <dd data-testid="node-block-height">
              {status?.running ? formatInt(status.committed) : "-"}
            </dd>
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
                    : loginItem === undefined
                      ? <Skeleton width="6ch" />
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

          <div className="divider" />

          <div className="section-heading">
            <h2 style={{ fontSize: "var(--text-base)" }}>
              Network
              {networkOverview?.sourceHost && (
                <span
                  style={{
                    marginLeft: 8,
                    fontSize: "var(--text-xs)",
                    color: "var(--text-muted)",
                    fontWeight: 400,
                  }}
                >
                  via {hostLabel(networkOverview.sourceHost)}
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
            <dt>Host block height</dt>
            <dd data-testid="dashboard-chain-height">{chainHeight}</dd>
            {/* DAG rounds and health can advance without sealed blocks.
                Height and age both come from the fresh host overview. */}
            <dt>Last block</dt>
            <dd
              data-testid="chain-block-age"
              style={{
                color: chainBlockAgeIsStale ? "var(--warning)" : undefined,
              }}
            >
              {chainBlockAge}
            </dd>
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
            {networkOverview?.sourceHost && (
              <> ({hostLabel(networkOverview.sourceHost)})</>
            )}
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
  value: React.ReactNode;
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
