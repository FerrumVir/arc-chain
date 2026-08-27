import { useMutation, useQuery } from "@tanstack/react-query";
import { AlertTriangle, Check, RefreshCw, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";
import { Card, CardHeader } from "../components/Card";
import { NotAvailable } from "../components/NotAvailable";
import { StatusPill } from "../components/StatusPill";
import { api } from "../lib/tauri";
import { formatInt } from "../lib/format";
import { useAppStore } from "../lib/store";
import { DEFAULT_NODE_CONFIG, type NodeConfig } from "../lib/types";
import { appUpdater, useUpdaterSnapshot } from "../lib/updater";

export function Settings() {
  const config = useAppStore((s) => s.config);
  const identity = useAppStore((s) => s.identity);
  const setOnboarded = useAppStore((s) => s.setOnboarded);
  const setConfig = useAppStore((s) => s.setConfig);
  const setIdentity = useAppStore((s) => s.setIdentity);
  // Defaults now match the real ones (types.ts DEFAULT_NODE_CONFIG and the
  // Rust NodeConfig::default). The RPC field used to default to 9944 while
  // onboarding wrote 9090 and the node bound 9090.
  const [rpcPort, setRpcPort] = useState(
    config?.rpcPort ?? DEFAULT_NODE_CONFIG.rpcPort,
  );
  const [p2pPort, setP2pPort] = useState(
    config?.p2pPort ?? DEFAULT_NODE_CONFIG.p2pPort,
  );
  const [autoUpdate, setAutoUpdate] = useState(config?.autoUpdate ?? true);
  const [autoStart, setAutoStart] = useState(config?.autoStart ?? true);
  const [saved, setSaved] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  // One app-wide updater state is shared by startup, periodic, and manual
  // checks. Settings only renders it; navigating here cannot start a second
  // polling loop or race a background check.
  const update = useUpdaterSnapshot();
  const updateInstallPolicy = useQuery({
    queryKey: ["update-install-policy"],
    queryFn: api.updateInstallPolicy,
    staleTime: Infinity,
  });
  const savedAutoUpdate =
    config?.autoUpdate ?? DEFAULT_NODE_CONFIG.autoUpdate;
  const autoUpdateHasUnsavedChange = autoUpdate !== savedAutoUpdate;

  // Native config hydration can complete after the first render. Keep the
  // form aligned with the persisted preference without overwriting a user's
  // in-progress toggle for unrelated config updates.
  useEffect(() => {
    setAutoUpdate(config?.autoUpdate ?? DEFAULT_NODE_CONFIG.autoUpdate);
  }, [config?.autoUpdate]);

  const save = async () => {
    // Previously `if (!config) return;` — so on a fresh install, where
    // config is null, Save silently did nothing and never showed the Saved
    // state. Fall back to defaults instead of no-oping, and surface errors.
    setSaveError(null);
    const next: NodeConfig = {
      ...(config ?? DEFAULT_NODE_CONFIG),
      rpcPort,
      p2pPort,
      autoUpdate,
      autoStart,
    };
    try {
      await api.saveConfig(next);
      setConfig(next);
      setSaved(true);
      // 2.5s, not 1.5s. A confirmation short enough to miss isn't a
      // confirmation - and this screen now also runs a polling status query
      // for the core slider, so on a loaded machine the old window could
      // close before the user (or a test) saw it.
      setTimeout(() => setSaved(false), 2500);
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div className="main-inner" data-testid="settings-screen">
      <div className="page-header">
        <div>
          <h1 className="page-title">Settings</h1>
          <p className="page-subtitle">Configure your node and app preferences.</p>
        </div>
      </div>

      <PersistenceCard />

      <Card style={{ marginBottom: "var(--space-6)" }}>
        <CardHeader title="Node" />

        <div
          style={{
            display: "grid",
            gap: "var(--space-4)",
          }}
        >
          <div className="field">
            <label className="field-label">RPC port</label>
            <input
              className="input input-mono"
              type="number"
              value={rpcPort}
              onChange={(e) => setRpcPort(parseInt(e.target.value, 10) || 0)}
              data-testid="input-rpc-port"
            />
            <span className="field-hint">
              Default 9090. HTTP, used by this app to talk to your node.
            </span>
          </div>

          <div className="field">
            <label className="field-label">P2P port</label>
            <input
              className="input input-mono"
              type="number"
              value={p2pPort}
              onChange={(e) => setP2pPort(parseInt(e.target.value, 10) || 0)}
              data-testid="input-p2p-port"
            />
            {/* The old hint claimed "P2P port is automatically RPC + 1".
                It is not — p2pPort is an independent stored field, so
                raising RPC to 9500 left P2P on 9091. It is now an explicit
                input rather than a false promise. */}
            <span className="field-hint">
              Default 9091. UDP/QUIC, used to reach other nodes. Set
              independently of the RPC port.
            </span>
          </div>

          <ComputeContribution />

          <label
            style={{
              display: "flex",
              alignItems: "center",
              gap: "var(--space-3)",
              cursor: "pointer",
            }}
          >
            <input
              type="checkbox"
              checked={autoStart}
              onChange={(e) => setAutoStart(e.target.checked)}
              data-testid="toggle-autostart"
            />
            <div>
              <div style={{ fontWeight: 500 }}>Start node on app launch</div>
              <div style={{ fontSize: "var(--text-sm)", color: "var(--text-muted)" }}>
                Automatically launch the node whenever ARC opens.
              </div>
            </div>
          </label>

          <label
            style={{
              display: "flex",
              alignItems: "center",
              gap: "var(--space-3)",
              cursor: "pointer",
            }}
          >
            <input
              type="checkbox"
              checked={autoUpdate}
              onChange={(e) => setAutoUpdate(e.target.checked)}
              data-testid="toggle-autoupdate"
            />
            <div>
              <div style={{ fontWeight: 500 }}>
                Check for app updates automatically
              </div>
              <div style={{ fontSize: "var(--text-sm)", color: "var(--text-muted)" }}>
                Check shortly after ARC starts and once every 24 hours. Updates
                are never installed without your confirmation.
              </div>
            </div>
          </label>

          <div>
            <button
              className="btn btn-primary"
              onClick={save}
              data-testid="btn-save-settings"
            >
              {saved ? (
                <>
                  <Check size={14} /> Saved
                </>
              ) : (
                "Save changes"
              )}
            </button>
            {saveError && (
              <p
                style={{
                  marginTop: "var(--space-2)",
                  fontSize: "var(--text-sm)",
                  color: "var(--danger)",
                }}
                data-testid="save-error"
              >
                Could not save: {saveError}
              </p>
            )}
            <p
              style={{
                marginTop: "var(--space-2)",
                fontSize: "var(--text-sm)",
                color: "var(--text-muted)",
              }}
            >
              Port changes take effect the next time the node restarts.
            </p>
          </div>
        </div>
      </Card>

      <Card style={{ marginBottom: "var(--space-6)" }}>
        <CardHeader title="Inference" />
        <InferenceModeToggle />
      </Card>

      <Card style={{ marginBottom: "var(--space-6)" }}>
        <CardHeader
          title="Updates"
          action={
            update.phase === "checking" ? (
              <StatusPill level="info" label="Checking" />
            ) : update.phase === "ready" ? (
              <StatusPill level="info" label="Ready" />
            ) : update.version ? (
              <StatusPill level="info" label={`v${update.version}`} />
            ) : null
          }
        />
        <p
          style={{
            fontSize: "var(--text-sm)",
            color: "var(--text-muted)",
            marginBottom: "var(--space-3)",
          }}
          data-testid="update-policy"
        >
          {autoUpdateHasUnsavedChange
            ? autoUpdate
              ? "Save settings to enable automatic background checks."
              : "Save settings to turn automatic background checks off."
            : autoUpdate
              ? "Automatic checks run after startup and every 24 hours. Background checks never download or install an update."
              : "Automatic background checks are off. You can still check manually below."}
        </p>
        <p
          style={{
            fontSize: "var(--text-sm)",
            color:
              update.phase === "error"
                ? "var(--danger)"
                : "var(--text-secondary)",
            marginBottom: "var(--space-3)",
          }}
          data-testid="update-status"
          data-update-phase={update.phase}
        >
          {update.message}
          {update.phase === "downloading" &&
          update.contentLength !== null &&
          update.contentLength > 0
            ? ` (${Math.min(100, Math.round((update.downloadedBytes / update.contentLength) * 100))}%)`
            : ""}
        </p>
        {update.error && (
          <p
            style={{
              fontSize: "var(--text-sm)",
              color:
                update.phase === "ready"
                  ? "var(--warning)"
                  : "var(--danger)",
              marginBottom: "var(--space-3)",
            }}
            data-testid="update-error"
          >
            {update.phase === "ready"
              ? `Relaunch failed: ${update.error}`
              : `Update error: ${update.error}`}
          </p>
        )}
        <p
          style={{
            fontSize: "var(--text-xs)",
            color: "var(--text-muted)",
            marginBottom: "var(--space-3)",
          }}
          data-testid="update-install-policy"
        >
          {updateInstallPolicy.data?.canInstall === false
            ? `Install policy: ${updateInstallPolicy.data.instructions}`
            : "Install policy: after you choose Install, ARC downloads and verifies the signed bundle, installs it, then immediately relaunches. If installation fails, ARC keeps running this version."}
        </p>
        <div style={{ display: "flex", gap: "var(--space-2)" }}>
          <button
            className="btn btn-secondary"
            onClick={() => void appUpdater.checkForUpdates("manual")}
            disabled={
              update.phase === "checking" ||
              update.phase === "downloading" ||
              update.phase === "ready"
            }
            data-testid="btn-check-update"
          >
            <RefreshCw
              size={14}
              style={
                update.phase === "checking"
                  ? { animation: "spin 1s linear infinite" }
                  : {}
              }
            />{" "}
            {update.phase === "checking" ? "Checking…" : "Check for updates"}
          </button>
          {update.canInstall && update.version && (
            <button
              className="btn btn-primary"
              onClick={() => void appUpdater.installAvailableUpdate()}
              disabled={update.phase === "downloading"}
              data-testid="btn-install-update"
            >
              {`Install v${update.version} & relaunch`}
            </button>
          )}
        </div>
      </Card>

      <Card>
        <CardHeader title="Identity" />
        {identity ? (
          <>
            <div
              style={{
                fontFamily: "var(--font-mono)",
                fontSize: "var(--text-sm)",
                color: "var(--text-secondary)",
                wordBreak: "break-all",
                marginBottom: "var(--space-4)",
                padding: "var(--space-3)",
                background: "var(--bg)",
                borderRadius: "var(--radius-sm)",
                border: "1px solid var(--border)",
              }}
            >
              {identity.address}
            </div>
            <div
              style={{
                padding: "var(--space-3) var(--space-4)",
                background: "var(--warning-bg)",
                border: "1px solid rgba(251, 191, 36, 0.2)",
                borderRadius: "var(--radius-sm)",
                display: "flex",
                gap: "var(--space-3)",
                alignItems: "flex-start",
              }}
            >
              <AlertTriangle size={16} style={{ color: "var(--warning)", flexShrink: 0, marginTop: 2 }} />
              <div>
                <div style={{ color: "var(--warning)", fontWeight: 500, marginBottom: 2 }}>
                  Keep your recovery phrase safe
                </div>
                <div style={{ fontSize: "var(--text-sm)", color: "var(--text-secondary)" }}>
                  The phrase you saw during setup is the only way to restore this identity.
                  We don't store it.
                </div>
              </div>
            </div>
          </>
        ) : (
          <p style={{ color: "var(--text-muted)", fontSize: "var(--text-sm)" }}>
            No identity configured.
          </p>
        )}

        <div className="divider" />

        <button
          className="btn btn-danger"
          onClick={() => {
            if (
              confirm(
                "Reset the app? This forgets your identity on this device. Funds stay on-chain.",
              )
            ) {
              // Full reset: clear identity + config + onboarded flag so the
              // wizard actually runs again. (The Rust store would still hold
              // the keys on disk - a full wipe requires `Uninstall` which
              // removes the app-data dir.)
              setIdentity(null);
              setConfig(null);
              setOnboarded(false);
            }
          }}
          data-testid="btn-reset"
        >
          <Trash2 size={14} /> Reset onboarding
        </button>
      </Card>

      <style>{`@keyframes spin { to { transform: rotate(360deg); } }`}</style>
    </div>
  );
}

/**
 * How many CPU cores the node contributes.
 *
 * There was previously no such control at any layer — not in the UI, not in
 * NodeConfig, not in the node's CLI — so rayon silently took every logical
 * core and there was no way to raise or lower it.
 *
 * Applying tries a live reconfigure first and only restarts if the node
 * can't do it in place; the result message says which happened, because
 * "applied live" and "restarted your node" are very different things to
 * have just done to someone mid-demo.
 */
function ComputeContribution() {
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);

  const { data: status } = useQuery({
    queryKey: ["status"],
    queryFn: api.nodeStatus,
    refetchInterval: 3000,
  });

  const maxCores = status?.cpuCores ?? 8;
  const active = status?.workerThreads ?? config?.workerThreads ?? maxCores;
  const [value, setValue] = useState<number | null>(null);
  const shown = value ?? active;

  const apply = useMutation({
    mutationFn: (n: number) => api.setWorkerThreads(n),
    onSuccess: (r) => {
      if (config) setConfig({ ...config, workerThreads: r.workerThreads });
      setValue(null);
    },
  });

  const dirty = value !== null && value !== active;

  return (
    <div className="field" data-testid="compute-contribution">
      <label className="field-label" htmlFor="worker-threads">
        Compute contribution
      </label>
      <div
        style={{ display: "flex", alignItems: "center", gap: "var(--space-3)" }}
      >
        <input
          id="worker-threads"
          type="range"
          min={1}
          max={maxCores}
          step={1}
          value={shown}
          onChange={(e) => setValue(parseInt(e.target.value, 10))}
          style={{ flex: 1 }}
          data-testid="slider-worker-threads"
          aria-valuemin={1}
          aria-valuemax={maxCores}
          aria-valuenow={shown}
        />
        <span
          className="mono"
          style={{ minWidth: 72, textAlign: "right" }}
          data-testid="worker-threads-value"
        >
          {shown} / {maxCores}
        </span>
        <button
          className="btn btn-secondary"
          onClick={() => apply.mutate(shown)}
          disabled={!dirty || apply.isPending}
          data-testid="btn-apply-threads"
        >
          {apply.isPending ? "Applying…" : "Apply"}
        </button>
      </div>
      {/* The mechanism, in one sentence, with no multiplier implied.
          This hint used to read "More cores means more work served — and more
          earnings", which states a causal link from cores to ARC that does not
          exist: the network routes work by its own scheduling, and a node with
          every core dedicated earns nothing if it is sent nothing. */}
      <span className="field-hint">
        Cores your node may use for inference and verification. More cores
        serve each hop faster, at the cost of responsiveness elsewhere on this
        machine. Cores do not multiply ARC: payment requires compatible assigned
        work, independent verification, validator authorization, and a
        successful mined <code>0x25</code> reward receipt.
      </span>

      <ActualContribution />

      {apply.data && (
        <p
          style={{
            marginTop: "var(--space-2)",
            fontSize: "var(--text-sm)",
            color: "var(--text-muted)",
          }}
          data-testid="threads-result"
        >
          {apply.data.message}
        </p>
      )}
      {apply.error && (
        <p
          style={{
            marginTop: "var(--space-2)",
            fontSize: "var(--text-sm)",
            color: "var(--danger)",
          }}
          data-testid="threads-error"
        >
          {String(apply.error)}
        </p>
      )}
    </div>
  );
}

/**
 * What this node is actually contributing, next to the slider that sets it.
 *
 * The slider on its own says what was requested. This says what happened —
 * which is a different thing, and the gap between them is the interesting part
 * (a node that is stopped, or was launched before the last change, contributes
 * what it was started with, not what the slider reads).
 *
 * Read from the LOCAL node. Every figure is a measurement or absent.
 */
function ActualContribution() {
  const { data: c } = useQuery({
    queryKey: ["node-contribution"],
    queryFn: api.fetchNodeContribution,
    refetchInterval: 10_000,
  });

  if (!c) return null;

  if (c.unavailable) {
    return (
      <div style={{ marginTop: "var(--space-3)" }}>
        <NotAvailable
          reason={c.unavailable}
          title="Currently contributing: unknown"
          testId="contribution-unavailable"
        />
      </div>
    );
  }

  const rows: Array<[string, string]> = [];
  if (c.threadsInUse != null) {
    rows.push([
      "Cores in use",
      c.threadsAvailable != null
        ? `${formatInt(c.threadsInUse)} of ${formatInt(c.threadsAvailable)}`
        : formatInt(c.threadsInUse),
    ]);
  }
  if (c.layersHeld) {
    // layerCount is a UNION of the layers held, not a sum over replicas, so
    // "6 of 32" is honest even when two ranges overlap.
    const held =
      c.layerCount != null
        ? c.totalLayers != null
          ? `${c.layersHeld} — ${formatInt(c.layerCount)} of ${formatInt(c.totalLayers)} layers`
          : `${c.layersHeld} (${formatInt(c.layerCount)} layers)`
        : c.layersHeld;
    rows.push(["Model layers held", held]);
  }
  if (c.runsServed != null) {
    rows.push(["Pipeline runs served", formatInt(c.runsServed)]);
  }
  // Kept separate from runs served: a cache hit is not work performed, and
  // summing the two would inflate the count of hops this node actually ran.
  if (c.cacheHits != null) {
    rows.push(["Served from cache", formatInt(c.cacheHits)]);
  }
  if (c.hopMsMean != null) {
    // The sample count is part of the claim: a mean over 2 samples and a mean
    // over 200 are not the same statement.
    rows.push([
      "Measured time per hop",
      c.hopSamples != null
        ? `${Math.round(c.hopMsMean)} ms (mean of ${formatInt(c.hopSamples)})`
        : `${Math.round(c.hopMsMean)} ms`,
    ]);
  }

  return (
    <div
      style={{
        marginTop: "var(--space-3)",
        padding: "var(--space-3) var(--space-4)",
        background: "var(--bg)",
        border: "1px solid var(--border)",
        borderRadius: "var(--radius-sm)",
      }}
      data-testid="actual-contribution"
    >
      <div
        className="stat-label"
        style={{ marginBottom: "var(--space-2)" }}
      >
        Currently contributing
      </div>
      {rows.length === 0 ? (
        <p
          style={{
            margin: 0,
            fontSize: "var(--text-sm)",
            color: "var(--text-muted)",
          }}
        >
          Your node answered, but reported none of these figures yet. They
          appear once it has served work.
        </p>
      ) : (
        <div className="kv">
          {rows.map(([k, v]) => (
            <div key={k} style={{ display: "contents" }}>
              <dt>{k}</dt>
              <dd data-testid={`contrib-${k.toLowerCase().replace(/[^a-z]+/g, "-")}`}>
                {v}
              </dd>
            </div>
          ))}
        </div>
      )}
      {/* The host's own reason for having no timing, shown verbatim rather
          than leaving the row silently missing. */}
      {c.hopMsMean == null && c.hopUnavailableReason && (
        <p
          style={{
            margin: "var(--space-2) 0 0",
            fontSize: "var(--text-xs)",
            color: "var(--text-muted)",
            lineHeight: 1.6,
          }}
          data-testid="contrib-hop-unavailable"
        >
          No per-hop timing: {c.hopUnavailableReason}
        </p>
      )}
      <p
        style={{
          margin: "var(--space-3) 0 0",
          fontSize: "var(--text-xs)",
          color: "var(--text-muted)",
          lineHeight: 1.6,
        }}
      >
        Read from your own node at {c.sourceHost}
        {c.source === "composed" && (
          <>
            {" "}
            via <code>/node/threads</code> and <code>/stats</code>, because it
            does not serve <code>/node/contribution</code>
          </>
        )}
        . Anything this node does not measure is left out rather than shown as
        zero.
      </p>
    </div>
  );
}

/**
 * Startup state, stated plainly — whether the app is configured to start the
 * node and whether the OS login item that can reopen the app is registered.
 *
 * It has to be truthful about what resumes, not just that something does.
 * Auto-start brings the process back, but the role depends on whether a model
 * path is configured. Neither process state nor role proves exact-artifact
 * eligibility, assignment, authorization, or payment.
 */
function PersistenceCard() {
  const config = useAppStore((s) => s.config);

  const { data: loginItem } = useQuery({
    queryKey: ["autostart"],
    queryFn: api.getAutostart,
    refetchInterval: 30_000,
  });
  const { data: status } = useQuery({
    queryKey: ["status"],
    queryFn: api.nodeStatus,
    refetchInterval: 5_000,
  });

  const autoStart = config?.autoStart ?? DEFAULT_NODE_CONFIG.autoStart;
  const hasModel = !!config?.modelPath;
  const running = !!status?.running;

  return (
    <Card style={{ marginBottom: "var(--space-6)" }} data-testid="persistence-card">
      <CardHeader
        title="Startup readiness"
        action={
          <StatusPill
            level={running ? "live" : "offline"}
            label={running ? "Node running" : "Node stopped"}
          />
        }
      />
      <div
        style={{
          fontSize: "var(--text-sm)",
          color: "var(--text-secondary)",
          lineHeight: 1.7,
        }}
      >
        <p style={{ marginTop: 0 }} data-testid="persistence-summary">
          {autoStart ? (
            <>
              <strong>
                ARC is configured to start the node when the app opens.
              </strong>{" "}
              {loginItem === true
                ? "The OS login item is registered, so ARC can reopen after login. Check “Right now” after a reboot; process startup does not prove peers, work, or payment."
                : loginItem === false
                  ? "No OS login item is registered, so this setting alone cannot reopen ARC after login. Open ARC manually or repair the login item."
                  : "OS login-item registration has not been verified yet. Until it is, do not assume ARC will reopen after login."}
            </>
          ) : (
            <>
              <strong>Your node does not start on its own.</strong>{" "}
              &ldquo;Start node on app launch&rdquo; is off, so after a reboot
              you have to start it yourself from the Dashboard. A stopped node
              cannot serve local inference; starting it still does not
              guarantee peers, assignment, or payment. Turn the setting on to
              have the process resume automatically.
            </>
          )}
        </p>

        <p data-testid="persistence-role">
          When it does resume, it comes back as{" "}
          {hasModel ? (
            <>
              a <strong>worker candidate</strong>: a model path is configured.
              The artifact must load completely and exactly match a requested
              model ID before the node advertises capacity. Assignment and a
              successful mined <code>0x25</code> reward remain separate gates.
            </>
          ) : (
            <>
              an <strong>observer/router</strong>: no model is configured, so it
              cannot execute local model inference. Downloading a complete
              compatible artifact can enable worker mode, but does not promise
              work or payment.
            </>
          )}
        </p>

        <div className="kv" style={{ marginTop: "var(--space-4)" }}>
          <dt>Start on app launch</dt>
          <dd data-testid="persistence-autostart">
            {autoStart ? "on" : "off"}
          </dd>
          <dt>Registered with the OS</dt>
          {/* The login item is the mechanism that survives a full reboot, and
              it is registered independently of the config flag. Reported
              separately so a disagreement between the two is visible rather
              than averaged into one reassuring line. */}
          <dd data-testid="persistence-login-item">
            {loginItem == null
              ? "—"
              : loginItem
                ? "yes — login item registered"
                : "no login item"}
          </dd>
          <dt>Right now</dt>
          <dd>{running ? "running" : "stopped"}</dd>
        </div>
      </div>
    </Card>
  );
}

function InferenceModeToggle() {
  const mode = useAppStore((s) => s.inferenceMode);
  const setMode = useAppStore((s) => s.setInferenceMode);
  return (
    <div style={{ display: "grid", gap: "var(--space-2)" }}>
      <label
        style={{
          display: "flex",
          alignItems: "center",
          gap: "var(--space-3)",
          cursor: "pointer",
        }}
      >
        <input
          type="radio"
          name="inferenceMode"
          checked={mode === "coordinator"}
          onChange={() => setMode("coordinator")}
          data-testid="inference-mode-coordinator"
        />
        <div>
          <div style={{ fontWeight: 500 }}>Coordinator</div>
          <div style={{ fontSize: "var(--text-sm)", color: "var(--text-muted)" }}>
            Tries your local node, then the selected coordinator path. The
            protocol-v3 worker path accepts community work only after
            authenticated 2-of-3 recomputation for every layer range and token.
            Older nodes may return less evidence, which the result screen labels.
          </div>
        </div>
      </label>
    </div>
  );
}
