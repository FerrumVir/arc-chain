import { useIsMutating, useMutation, useQuery } from "@tanstack/react-query";
import { Skeleton, SkeletonLines } from "../components/Skeleton";
import { AlertTriangle, Check, RefreshCw, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";
import { Card, CardHeader } from "../components/Card";
import { NotAvailable } from "../components/NotAvailable";
import { NodeCareSettings } from "../components/NodeCare";
import { StartFailurePanel } from "../components/StartFailurePanel";
import { classifyStartFailure } from "../lib/start-failure";
import { StatusPill } from "../components/StatusPill";
import { api, isTauri } from "../lib/tauri";
import { describeConnection } from "../lib/connection";
import { formatInt, modelDownloadStatus } from "../lib/format";
import { useAppStore } from "../lib/store";
import {
  computeContributionEnabled,
  DEFAULT_NODE_CONFIG,
  type ModelDownloadProgress,
  type NodeConfig,
} from "../lib/types";
import { appUpdater, useUpdaterSnapshot } from "../lib/updater";
import { PLAIN_COPY } from "../lib/plain-copy";
import { setTheme, useTheme, type Theme } from "../lib/theme";

const COMPUTE_CONTRIBUTION_MUTATION_KEY = ["compute-contribution"];

export function Settings() {
  const { data: startBlock } = useQuery({ queryKey: ["start-block"], queryFn: api.nodeStartBlock, refetchInterval: 1000 });
  if (startBlock && ["other-desktop", "closing"].includes(classifyStartFailure(startBlock))) {
    return <div className="main-inner" data-testid="settings-screen"><StartFailurePanel error={startBlock} /></div>;
  }
  return <SettingsForm />;
}

function SettingsForm() {
  const config = useAppStore((s) => s.config);
  const identity = useAppStore((s) => s.identity);
  const setOnboarded = useAppStore((s) => s.setOnboarded);
  const setConfig = useAppStore((s) => s.setConfig);
  const setIdentity = useAppStore((s) => s.setIdentity);
  const contributionPending =
    useIsMutating({ mutationKey: COMPUTE_CONTRIBUTION_MUTATION_KEY }) > 0;
  const [rpcPort, setRpcPort] = useState(
    config?.rpcPort ?? DEFAULT_NODE_CONFIG.rpcPort,
  );
  const [p2pPort, setP2pPort] = useState(
    config?.p2pPort ?? DEFAULT_NODE_CONFIG.p2pPort,
  );
  const [autoUpdate, setAutoUpdate] = useState(config?.autoUpdate ?? true);
  const [autoInstallUpdates, setAutoInstallUpdates] = useState(
    config?.autoInstallUpdates ?? DEFAULT_NODE_CONFIG.autoInstallUpdates,
  );
  const [autoStart, setAutoStart] = useState(config?.autoStart ?? true);
  const [saved, setSaved] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const update = useUpdaterSnapshot();
  const updateInstallPolicy = useQuery({
    queryKey: ["update-install-policy"],
    queryFn: api.updateInstallPolicy,
    staleTime: Infinity,
  });
  const savedAutoUpdate =
    config?.autoUpdate ?? DEFAULT_NODE_CONFIG.autoUpdate;
  const savedAutoInstallUpdates =
    config?.autoInstallUpdates ?? DEFAULT_NODE_CONFIG.autoInstallUpdates;
  const updatePreferencesHaveUnsavedChange =
    autoUpdate !== savedAutoUpdate ||
    autoInstallUpdates !== savedAutoInstallUpdates;

  useEffect(() => {
    setAutoUpdate(config?.autoUpdate ?? DEFAULT_NODE_CONFIG.autoUpdate);
  }, [config?.autoUpdate]);

  useEffect(() => {
    setAutoInstallUpdates(
      config?.autoInstallUpdates ?? DEFAULT_NODE_CONFIG.autoInstallUpdates,
    );
  }, [config?.autoInstallUpdates]);

  const save = async () => {
    setSaveError(null);
    setSaving(true);
    const next: NodeConfig = {
      ...(config ?? DEFAULT_NODE_CONFIG),
      rpcPort,
      p2pPort,
      autoUpdate,
      autoInstallUpdates,
      autoStart,
    };
    try {
      await api.saveConfig(next);
      setConfig(next);
      setSaved(true);
      setTimeout(() => setSaved(false), 2500);
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
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
            <span className="field-hint">
              Default 9091. UDP/QUIC, used to reach other nodes. Set
              independently of the RPC port.
            </span>
          </div>

          <ComputeConsent />

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
                Check shortly after ARC starts and once every 24 hours. This
                setting only checks for signed releases.
              </div>
            </div>
          </label>

          <label
            style={{
              display: "flex",
              alignItems: "flex-start",
              gap: "var(--space-3)",
              cursor: "pointer",
            }}
          >
            <input
              type="checkbox"
              checked={autoInstallUpdates}
              onChange={(e) => setAutoInstallUpdates(e.target.checked)}
              data-testid="toggle-autoinstall-updates"
            />
            <div>
              <div style={{ fontWeight: 500 }}>
                Install signed app updates automatically when idle
              </div>
              <div style={{ fontSize: "var(--text-sm)", color: "var(--text-muted)" }}>
                {PLAIN_COPY.updates.autoInstallDescription}
              </div>
            </div>
          </label>

          <div>
            <button
              className="btn btn-primary"
              onClick={save}
              disabled={saving || contributionPending}
              aria-busy={saving || undefined}
              data-testid="btn-save-settings"
            >
              {saving ? (
                <>
                  <RefreshCw size={14} className="spin" /> Saving…
                </>
              ) : saved ? (
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

      <Card style={{ marginBottom: "var(--space-6)" }} data-testid="appearance-card">
        <CardHeader title="Appearance" />
        <AppearanceToggle />
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
          {updateInstallPolicy.data?.channel === "package-manager"
            ? PLAIN_COPY.settings.packageUpdates
            : updatePreferencesHaveUnsavedChange
            ? "Save settings to apply automatic update preferences."
            : !autoUpdate
              ? autoInstallUpdates
                ? "Automatic checks are off, so unattended installation is inactive. You can still check and install manually below."
                : "Automatic background checks are off. You can still check and install manually below."
              : autoInstallUpdates
                ? "Automatic checks run after startup and every 24 hours. Signed app updates may install and restart ARC automatically only after native worker safety confirms inference has drained."
                : "Automatic checks run after startup and every 24 hours. Background checks never install an update."}
        </p>
        <p
          style={{
            fontSize: "var(--text-sm)",
            color: "var(--text-secondary)",
            marginBottom: "var(--space-3)",
          }}
          data-testid="update-status"
          data-update-phase={update.phase}
        >
          {update.phase === "error"
            ? update.restartRequired
              ? PLAIN_COPY.nodeStrip.updateFailed
              : update.message.startsWith("Update check failed")
                ? PLAIN_COPY.updates.checkFailed
                : PLAIN_COPY.updates.installFailed
            : update.phase === "ready" && update.error
              ? PLAIN_COPY.nodeStrip.updateFailed
              : update.message}
          {update.phase === "downloading" &&
          update.contentLength !== null &&
          update.contentLength > 0
            ? ` (${Math.min(100, Math.round((update.downloadedBytes / update.contentLength) * 100))}%)`
            : ""}
        </p>
        {update.error && (
          <details
            className="plain-details"
            style={{ marginTop: 0, marginBottom: "var(--space-3)" }}
            data-testid="update-error"
          >
            <summary>{PLAIN_COPY.actions.showDetails}</summary>
            <pre>{[update.message, update.error].filter(Boolean).join("\n")}</pre>
          </details>
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
            : "Install policy: after you choose Install, ARC downloads and verifies the signed bundle before installation starts. Once bundle installation begins, the node stays stopped until ARC exits; if completion cannot be verified, quit and reopen ARC (or reinstall from the verified release)."}
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

      <Card style={{ marginBottom: "var(--space-6)" }} data-testid="node-care-card">
        <CardHeader title="Error reports" />
        <NodeCareSettings />
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
                <div
                  style={{ fontSize: "var(--text-sm)", color: "var(--text-secondary)" }}
                  data-testid="recovery-phrase-storage"
                >
                  The phrase you saw during setup is the only way to restore this
                  identity on another computer, or here if this app's data is
                  deleted. This app keeps a plain-text copy on this computer only,
                  in <code>store.json</code> in its <code>network.arc.desktop</code> data
                  folder, and never sends it over the network. Outside the app, your
                  system can still copy it: backups can include that folder, a Windows
                  roaming profile can sync it to a server, and if you used Copy during
                  setup, your clipboard may have kept it or synced it to your other
                  devices. Write the phrase down, keep it offline, and keep this
                  computer secure.
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

function ComputeConsent() {
  const config = useAppStore((s) => s.config);
  const setConfig = useAppStore((s) => s.setConfig);
  const contributionChosen = computeContributionEnabled(config);
  const keepAwake = config?.preventSleepDuringJobs === true;
  const { data: tier } = useQuery({
    queryKey: ["recommended-tier"],
    queryFn: api.recommendedTier,
  });
  const ineligible = tier === "none";
  const enabled = contributionChosen && !ineligible;
  const [progress, setProgress] = useState<ModelDownloadProgress | null>(null);
  const { data: status } = useQuery({
    queryKey: ["status"],
    queryFn: api.nodeStatus,
    refetchInterval: 3000,
  });
  const modelUnavailable = status?.modelUnavailable ?? null;
  const coreCap = status?.workerThreads ?? config?.workerThreads ?? null;
  const logicalCores = status?.cpuCores ?? null;

  const toggle = useMutation({
    mutationKey: COMPUTE_CONTRIBUTION_MUTATION_KEY,
    mutationFn: (next: boolean) => api.setComputeContribution(next),
    onMutate: () => setProgress(null),
    onSuccess: (saved) => setConfig(saved),
  });
  const redownload = useMutation({
    mutationKey: COMPUTE_CONTRIBUTION_MUTATION_KEY,
    mutationFn: () => api.promoteConsentedInstall(),
    onMutate: () => setProgress(null),
    onSuccess: (saved) => setConfig(saved),
  });
  const awake = useMutation({
    mutationFn: (next: boolean) => api.setPreventSleepDuringJobs(next),
    onSuccess: (saved) => setConfig(saved),
  });
  const sharingAwake = useMutation({
    mutationFn: (next: boolean) => api.setKeepAwakeWhileSharing(next),
    onSuccess: (saved) => setConfig(saved),
  });
  const downloading = toggle.isPending || redownload.isPending;

  useEffect(() => {
    if (!isTauri || !downloading) return;
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    (async () => {
      const { listen } = await import("@tauri-apps/api/event");
      const handle = await listen<ModelDownloadProgress>(
        "model-download-progress",
        (event) => setProgress(event.payload),
      );
      if (cancelled) handle();
      else unlisten = handle;
    })();
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [downloading]);

  return (
    <div className="field" data-testid="compute-consent">
      <span className="field-label">Contribute compute</span>
      <label
        htmlFor="compute-consent-toggle"
        style={{ display: "flex", alignItems: "center", gap: "var(--space-3)", cursor: "pointer" }}
      >
        <input
          id="compute-consent-toggle"
          type="checkbox"
          checked={enabled}
          disabled={downloading || (!enabled && ineligible)}
          onChange={(e) => toggle.mutate(e.target.checked)}
          data-testid="compute-consent-toggle"
        />
        <span>Let ARC run inference jobs on this computer</span>
      </label>
      {enabled && logicalCores !== null && (
        <span className="field-hint" data-testid="compute-share-note">
          {PLAIN_COPY.settings.computeShare(coreCap, logicalCores)}
        </span>
      )}
      <span className="field-hint">
        Sharing starts on unless you turned it off, when the computer can run the ARC
        model. When on, the app downloads the ARC model once
        (3.80 GB, checked against its pinned SHA-256), switches your node to
        worker mode, and takes jobs from the network while ARC Node is
        running. Turning it off returns the node to observer mode right away.
      </span>
      {ineligible && !enabled && (
        <span className="field-hint" data-testid="compute-consent-ineligible">
          This computer has less than 16 GB of memory, so it cannot run the ARC
          model. It stays an observer, which still relays and verifies.
        </span>
      )}
      {enabled && modelUnavailable && !downloading && (
        <div
          role="status"
          style={{
            display: "flex",
            flexDirection: "column",
            alignItems: "flex-start",
            gap: "var(--space-2)",
          }}
          data-testid="compute-consent-model-unavailable"
        >
          <span className="field-hint">
            {modelUnavailable === "notFound"
              ? "The model file was not found, so local compute is off and your node is running as an observer. Restore the file, or re-download the model."
              : "The model file can't be read, so local compute is off and your node is running as an observer. Check the file's permissions, or re-download the model."}
          </span>
          <button
            type="button"
            className="btn btn-secondary"
            onClick={() => redownload.mutate()}
            data-testid="btn-redownload-model"
          >
            Re-download model
          </button>
        </div>
      )}
      {downloading && (
        <p
          style={{ marginTop: "var(--space-2)", fontSize: "var(--text-sm)", color: "var(--text-muted)" }}
          data-testid="compute-consent-progress"
        >
          {progress
            ? modelDownloadStatus(progress)
            : redownload.isPending
              ? "Checking for the model…"
              : enabled
                ? "Switching to observer mode…"
                : "Preparing worker mode…"}
        </p>
      )}
      {(toggle.error || redownload.error) && (
        <p
          role="alert"
          style={{ marginTop: "var(--space-2)", fontSize: "var(--text-sm)", color: "var(--danger)" }}
          data-testid="compute-consent-error"
        >
          {String(toggle.error ?? redownload.error)}
        </p>
      )}

      <label style={{ display: "flex", gap: "var(--space-3)", marginTop: "var(--space-3)" }}>
        <input type="checkbox" checked={config?.keepAwakeWhileSharing === true}
          disabled={sharingAwake.isPending}
          onChange={(event) => sharingAwake.mutate(event.target.checked)}
          data-testid="sharing-keep-awake-toggle" />
        <span>Keep this computer awake while plugged in so it can take jobs</span>
      </label>
      <span className="field-hint">Only while sharing is on and the node is running as a worker. The display can still sleep.</span>
      {sharingAwake.error && <p role="alert">{String(sharingAwake.error)}</p>}

      <label
        htmlFor="prevent-sleep-toggle"
        style={{
          display: "flex",
          alignItems: "center",
          gap: "var(--space-3)",
          cursor: enabled ? "pointer" : "default",
          marginTop: "var(--space-3)",
        }}
      >
        <input
          id="prevent-sleep-toggle"
          type="checkbox"
          checked={keepAwake}
          disabled={!enabled || awake.isPending}
          onChange={(e) => awake.mutate(e.target.checked)}
          data-testid="prevent-sleep-toggle"
        />
        <span>Keep this computer awake while a job runs</span>
      </label>
      <span className="field-hint">
        Holds off idle sleep only while a job is computing, so it is not lost
        halfway. The computer still sleeps between jobs and when the lid
        closes. Takes effect the next time the node starts.
      </span>
      {awake.error && (
        <p
          role="alert"
          style={{ marginTop: "var(--space-2)", fontSize: "var(--text-sm)", color: "var(--danger)" }}
          data-testid="prevent-sleep-error"
        >
          {String(awake.error)}
        </p>
      )}
    </div>
  );
}

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
          disabled={status === undefined}
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
          {status === undefined ? <Skeleton width="5ch" /> : <>{shown} / {maxCores}</>}
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

function ActualContribution() {
  const { data: c } = useQuery({
    queryKey: ["node-contribution"],
    queryFn: api.fetchNodeContribution,
    refetchInterval: 10_000,
  });

  if (!c) {
    return (
      <div style={{ marginTop: "var(--space-3)" }} aria-busy="true" data-testid="contribution-loading">
        <SkeletonLines lines={3} gap={8} />
      </div>
    );
  }

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
  if (c.cacheHits != null) {
    rows.push(["Served from cache", formatInt(c.cacheHits)]);
  }
  if (c.hopMsMean != null) {
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
  const connection = describeConnection(status);

  return (
    <Card style={{ marginBottom: "var(--space-6)" }} data-testid="persistence-card">
      <CardHeader
        title={PLAIN_COPY.settings.startupTitle}
        action={
          <StatusPill
            level={running ? "live" : connection.level}
            label={running ? "Node running" : connection.label}
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
          {!autoStart ? PLAIN_COPY.settings.startupOff
            : loginItem === true ? PLAIN_COPY.settings.startupOn
            : loginItem === false ? PLAIN_COPY.settings.startupUnregistered
            : loginItem === undefined ? PLAIN_COPY.settings.startupChecking
            : PLAIN_COPY.settings.startupUnknown}
        </p>
        <p data-testid="persistence-role">
          {hasModel ? PLAIN_COPY.settings.startupModel : PLAIN_COPY.settings.startupNoModel}
        </p>

        <div className="kv" style={{ marginTop: "var(--space-4)" }}>
          <dt>Start on app launch</dt>
          <dd data-testid="persistence-autostart">
            {autoStart ? "on" : "off"}
          </dd>
          <dt>Registered with the OS</dt>
          <dd data-testid="persistence-login-item">
            {loginItem === undefined
              ? <Skeleton width="10ch" />
              : loginItem === null
              ? "—"
              : loginItem
                ? "yes — login item registered"
                : "no login item"}
          </dd>
          <dt>Right now</dt>
          <dd>{status === undefined ? <Skeleton width="6ch" /> : running ? "running" : "stopped"}</dd>
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
            protocol-v3 worker path accepts community work only after two or
            more validators recompute every layer range and token and return
            the same authenticated result (2-of-3 when a range has three or
            more live validators, 2-of-2 when it has two). Older nodes may
            return less evidence, which the result screen labels.
          </div>
        </div>
      </label>
    </div>
  );
}

function AppearanceToggle() {
  const theme = useTheme();
  const options: { value: Theme; title: string; body: string }[] = [
    { value: "engraved", title: "Engraved", body: "The website's language: white line on Arc blue, with engraved illustrations." },
    { value: "classic", title: "Classic", body: "The original dark theme." },
  ];
  return (
    <div style={{ display: "grid", gap: "var(--space-3)" }} role="radiogroup" aria-label="Appearance">
      {options.map((o) => (
        <label key={o.value} style={{ display: "flex", alignItems: "center", gap: "var(--space-3)", cursor: "pointer" }}>
          <input type="radio" name="appearance" checked={theme === o.value} onChange={() => setTheme(o.value)} data-testid={`theme-${o.value}`} />
          <div>
            <div style={{ fontWeight: 500 }}>{o.title}</div>
            <div style={{ fontSize: "var(--text-sm)", color: "var(--text-muted)" }}>{o.body} Saved on this device only.</div>
          </div>
        </label>
      ))}
    </div>
  );
}
