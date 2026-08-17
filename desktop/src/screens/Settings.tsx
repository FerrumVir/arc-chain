import { useMutation, useQuery } from "@tanstack/react-query";
import { AlertTriangle, Check, RefreshCw, Trash2 } from "lucide-react";
import { useState } from "react";
import { check as tauriCheckUpdate } from "@tauri-apps/plugin-updater";
import { relaunch as tauriRelaunch } from "@tauri-apps/plugin-process";
import { Card, CardHeader } from "../components/Card";
import { StatusPill } from "../components/StatusPill";
import { api, isTauri } from "../lib/tauri";
import { useAppStore } from "../lib/store";
import { DEFAULT_NODE_CONFIG, type NodeConfig } from "../lib/types";

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

  // A single source of update truth: the Tauri updater plugin, which reads
  // the signed manifest. The old `api.checkForUpdate` hit the GitHub
  // releases API instead, so the badge and the Install button could — and
  // routinely did — disagree.
  const {
    data: update,
    refetch: checkUpdate,
    isFetching,
  } = useQuery({
    queryKey: ["update-check"],
    queryFn: async () => {
      // The updater plugin only exists inside the native shell. In the
      // browser preview say so plainly rather than throwing.
      if (!isTauri) return { hasUpdate: false, version: null };
      const u = await tauriCheckUpdate();
      return { hasUpdate: !!u, version: u?.version ?? null };
    },
    enabled: false,
  });
  const [installing, setInstalling] = useState(false);
  const [installError, setInstallError] = useState<string | null>(null);

  // Tauri auto-update: download the new bundle then relaunch the app.
  // Without the relaunch() call the installer overwrites the .app/.exe but
  // the existing process exits without reopening — user sees "auto-update
  // ran but the window never came back". The Rust-side updater plugin is
  // already registered in lib.rs; this is the missing JS-side trigger.
  const installUpdate = async () => {
    setInstalling(true);
    setInstallError(null);
    try {
      const u = await tauriCheckUpdate();
      if (!u) {
        setInstallError("No update available.");
        setInstalling(false);
        return;
      }
      await u.downloadAndInstall();
      await tauriRelaunch();
      // relaunch() never returns — process is replaced.
    } catch (e) {
      setInstallError(e instanceof Error ? e.message : String(e));
      setInstalling(false);
    }
  };

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
              <div style={{ fontWeight: 500 }}>Keep node up to date</div>
              <div style={{ fontSize: "var(--text-sm)", color: "var(--text-muted)" }}>
                Check GitHub for new releases daily and upgrade automatically.
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
            update?.version ? (
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
        >
          {update === undefined
            ? "Check for a new signed release."
            : update.hasUpdate
              ? `Version ${update.version} is available. Click below to download, install, and relaunch.`
              : "You're running the latest version."}
        </p>
        {installError && (
          <p
            style={{
              fontSize: "var(--text-sm)",
              color: "var(--danger)",
              marginBottom: "var(--space-3)",
            }}
            data-testid="update-error"
          >
            Update failed: {installError}
          </p>
        )}
        <div style={{ display: "flex", gap: "var(--space-2)" }}>
          <button
            className="btn btn-secondary"
            onClick={() => checkUpdate()}
            disabled={isFetching || installing}
            data-testid="btn-check-update"
          >
            <RefreshCw
              size={14}
              style={isFetching ? { animation: "spin 1s linear infinite" } : {}}
            />{" "}
            Check for updates
          </button>
          {update?.hasUpdate && (
            <button
              className="btn btn-primary"
              onClick={installUpdate}
              disabled={installing}
              data-testid="btn-install-update"
            >
              {installing ? "Installing…" : `Install v${update.version} & relaunch`}
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
      <span className="field-hint">
        Cores your node may use for inference and verification. More cores
        means more work served — and more earnings — at the cost of
        responsiveness elsewhere on this machine.
      </span>
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
            Routes inference through the seed coordinator network. Results are
            verified by k-of-n consensus across validator replicas.
          </div>
        </div>
      </label>
    </div>
  );
}
