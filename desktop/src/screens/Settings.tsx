import { useQuery } from "@tanstack/react-query";
import { AlertTriangle, Check, RefreshCw, Trash2 } from "lucide-react";
import { useState } from "react";
import { check as tauriCheckUpdate } from "@tauri-apps/plugin-updater";
import { relaunch as tauriRelaunch } from "@tauri-apps/plugin-process";
import { Card, CardHeader } from "../components/Card";
import { StatusPill } from "../components/StatusPill";
import { api } from "../lib/tauri";
import { useAppStore } from "../lib/store";

export function Settings() {
  const config = useAppStore((s) => s.config);
  const identity = useAppStore((s) => s.identity);
  const setOnboarded = useAppStore((s) => s.setOnboarded);
  const setConfig = useAppStore((s) => s.setConfig);
  const setIdentity = useAppStore((s) => s.setIdentity);
  const [rpcPort, setRpcPort] = useState(config?.rpcPort ?? 9944);
  const [autoUpdate, setAutoUpdate] = useState(config?.autoUpdate ?? true);
  const [autoStart, setAutoStart] = useState(config?.autoStart ?? true);
  const [saved, setSaved] = useState(false);

  const { data: update, refetch: checkUpdate, isFetching } = useQuery({
    queryKey: ["update-check"],
    queryFn: api.checkForUpdate,
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
    if (!config) return;
    const next = { ...config, rpcPort, autoUpdate, autoStart };
    await api.saveConfig(next);
    setConfig(next);
    setSaved(true);
    setTimeout(() => setSaved(false), 1500);
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
              Default 9944. P2P port is automatically RPC + 1.
            </span>
          </div>

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
          action={update ? <StatusPill level="info" label={`v${update.version}`} /> : null}
        />
        <p
          style={{
            fontSize: "var(--text-sm)",
            color: "var(--text-muted)",
            marginBottom: "var(--space-3)",
          }}
        >
          {update?.hasUpdate
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
