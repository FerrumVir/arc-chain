import { useEffect, useState } from "react";
import { Loader2, Sparkles, X } from "lucide-react";
import { api, isTauri } from "../lib/tauri";
import { useAppStore } from "../lib/store";
import type {
  ModelDownloadProgress,
  ModelTierInfo,
  NodeConfig,
} from "../lib/types";

function formatBytes(bytes: number): string {
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(0)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(2)} GB`;
}

/**
 * One-time CTA for users who onboarded on a v0.5.x desktop (which defaulted
 * everyone to `role: "observer"` with no model). Without this, that cohort
 * stays passive validators forever — peers ≥ 1, attestations always 0,
 * earnings always 0 — because the chain only dispatches inference to nodes
 * that announced a model via `--community-mode`. v0.6.0 onboards new users
 * as workers from the start; this component is the migration path for
 * everyone already on the network.
 *
 * Triggers when the loaded config has `role: "observer"` OR `modelPath` is
 * null. User picks a tier (or dismisses), we download the GGUF, swap
 * config to worker, restart arc-node so the new model is picked up.
 */
export function ObserverUpgradeBanner() {
  const config = useAppStore((s) => s.config);
  const setStoreConfig = useAppStore((s) => s.setConfig);

  const [dismissed, setDismissed] = useState(false);
  const [open, setOpen] = useState(false);
  const [tiers, setTiers] = useState<ModelTierInfo[]>([]);
  const [recommendedTier, setRecommendedTier] = useState<string>("standard");
  const [selectedTier, setSelectedTier] = useState<string | null>(null);

  const [busy, setBusy] = useState(false);
  const [stage, setStage] = useState<
    "idle" | "downloading" | "restarting"
  >("idle");
  const [progress, setProgress] = useState<ModelDownloadProgress | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Show the banner only when the loaded config exists AND it's an
  // observer-style config. Don't render anything during the brief load gap.
  const isObserverConfig =
    !!config && (config.role === "observer" || config.modelPath == null);

  useEffect(() => {
    if (!open || tiers.length > 0) return;
    let cancelled = false;
    Promise.all([api.listModelTiers(), api.recommendedTier()]).then(
      ([loadedTiers, rec]) => {
        if (cancelled) return;
        setTiers(loadedTiers);
        const safeRec = rec === "none" ? "tiny" : rec;
        setRecommendedTier(safeRec);
        setSelectedTier(safeRec);
      },
    );
    return () => {
      cancelled = true;
    };
  }, [open, tiers.length]);

  useEffect(() => {
    if (!isTauri || !open) return;
    let unlisten: (() => void) | undefined;
    (async () => {
      const { listen } = await import("@tauri-apps/api/event");
      const handle = await listen<ModelDownloadProgress>(
        "model-download-progress",
        (event) => {
          setProgress(event.payload);
        },
      );
      unlisten = handle;
    })();
    return () => {
      unlisten?.();
    };
  }, [open]);

  if (!isObserverConfig || dismissed) return null;

  const upgrade = async () => {
    if (!selectedTier || !config) return;
    setBusy(true);
    setError(null);
    try {
      setStage("downloading");
      // Same idempotency as onboarding: reuse an existing matched download.
      const existing = await api.existingModelForTier(selectedTier);
      const modelPath = existing ?? (await api.downloadModel(selectedTier));

      const updated: NodeConfig = {
        ...config,
        role: "worker",
        modelPath,
      };
      await api.saveConfig(updated);
      setStoreConfig(updated);

      // Restart the node so arc-node re-reads the new --model + --community-mode.
      // node_manager already version-checks the binary on every restart so an
      // older arc-node also gets refreshed.
      setStage("restarting");
      await api.restartNode();

      setOpen(false);
      setBusy(false);
      setStage("idle");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setBusy(false);
      setStage("idle");
    }
  };

  return (
    <>
      <div
        data-testid="observer-upgrade-banner"
        style={{
          padding: "var(--space-4) var(--space-5)",
          background: "rgba(99, 102, 241, 0.08)",
          border: "1px solid rgba(99, 102, 241, 0.3)",
          borderRadius: "var(--radius-md)",
          marginBottom: "var(--space-6)",
          display: "flex",
          alignItems: "center",
          gap: "var(--space-4)",
        }}
      >
        <div
          style={{
            width: 36,
            height: 36,
            borderRadius: "var(--radius-sm)",
            background: "var(--arc-gradient)",
            color: "white",
            display: "grid",
            placeItems: "center",
            flexShrink: 0,
          }}
        >
          <Sparkles size={18} />
        </div>
        <div style={{ flex: 1 }}>
          <div style={{ fontWeight: 500, color: "var(--text)", marginBottom: 2 }}>
            Make compatible compute available
          </div>
          <div
            style={{
              fontSize: "var(--text-sm)",
              color: "var(--text-muted)",
              lineHeight: 1.5,
            }}
          >
            This observer does not execute local model inference. Downloading a
            complete model can make it a worker candidate, but work is assigned
            only when the requested artifact ID matches exactly. A model, a
            peer connection, or worker mode alone never guarantees a reward.
          </div>
        </div>
        <button
          className="btn btn-primary"
          onClick={() => setOpen(true)}
          data-testid="btn-open-upgrade"
        >
          Choose model <Sparkles size={14} />
        </button>
        <button
          className="btn btn-ghost btn-sm"
          onClick={() => setDismissed(true)}
          aria-label="Dismiss"
          data-testid="btn-dismiss-upgrade"
        >
          <X size={14} />
        </button>
      </div>

      {open && (
        <div
          role="dialog"
          aria-modal="true"
          data-testid="upgrade-dialog"
          style={{
            position: "fixed",
            inset: 0,
            background: "rgba(0,0,0,0.5)",
            display: "grid",
            placeItems: "center",
            zIndex: 50,
          }}
          onClick={() => {
            if (!busy) setOpen(false);
          }}
        >
          <div
            style={{
              width: "min(560px, 90vw)",
              maxHeight: "85vh",
              overflowY: "auto",
              background: "var(--bg)",
              border: "1px solid var(--border)",
              borderRadius: "var(--radius-lg)",
              padding: "var(--space-6)",
              boxShadow: "var(--shadow-lg)",
            }}
            onClick={(e) => e.stopPropagation()}
          >
            <h2
              style={{
                fontSize: "var(--text-xl)",
                fontWeight: 600,
                color: "var(--text)",
                marginBottom: "var(--space-2)",
              }}
            >
              Pick a model tier
            </h2>
            <p
              style={{
                color: "var(--text-muted)",
                fontSize: "var(--text-sm)",
                lineHeight: 1.5,
                marginBottom: "var(--space-5)",
              }}
            >
              We've pre-selected the tier that fits your machine. Only a model
              that loads completely and exactly matches a requested artifact is
              eligible for that work. Model size changes disk and RAM use, not
              the reward paid for a successful receipt.
            </p>

            {!busy && (
              <div
                style={{ display: "grid", gap: "var(--space-3)", marginBottom: "var(--space-5)" }}
              >
                {tiers.length === 0 && (
                  <div className="shimmer" style={{ height: 200 }} />
                )}
                {tiers.map((tier) => {
                  const isSelected = selectedTier === tier.id;
                  const isRec = recommendedTier === tier.id;
                  return (
                    <button
                      type="button"
                      key={tier.id}
                      onClick={() => setSelectedTier(tier.id)}
                      data-testid={`upgrade-tier-${tier.id}`}
                      style={{
                        textAlign: "left",
                        padding: "var(--space-4)",
                        background: isSelected
                          ? "rgba(99, 102, 241, 0.12)"
                          : "var(--surface)",
                        border: `1px solid ${isSelected ? "var(--indigo-400)" : "var(--border)"}`,
                        borderRadius: "var(--radius-md)",
                        cursor: "pointer",
                        color: "var(--text)",
                      }}
                    >
                      <div
                        style={{
                          display: "flex",
                          gap: "var(--space-2)",
                          alignItems: "center",
                          marginBottom: 2,
                          fontWeight: 500,
                        }}
                      >
                        {tier.displayName}
                        {isRec && (
                          <span
                            style={{
                              fontSize: "var(--text-xs)",
                              color: "var(--indigo-300)",
                              fontWeight: 600,
                              letterSpacing: "var(--tracking-wider)",
                              textTransform: "uppercase",
                            }}
                          >
                            Recommended
                          </span>
                        )}
                      </div>
                      <div
                        style={{
                          fontSize: "var(--text-sm)",
                          color: "var(--text-muted)",
                        }}
                      >
                        {formatBytes(tier.sizeBytes)} download · one-time
                      </div>
                    </button>
                  );
                })}
              </div>
            )}

            {busy && (
              <div
                style={{
                  textAlign: "center",
                  padding: "var(--space-6) 0",
                }}
              >
                <Loader2
                  size={28}
                  style={{
                    animation: "spin 1s linear infinite",
                    color: "var(--indigo-300)",
                    marginBottom: "var(--space-3)",
                  }}
                />
                <div style={{ fontWeight: 500, marginBottom: 4 }}>
                  {stage === "downloading"
                    ? "Downloading model"
                    : stage === "restarting"
                      ? "Restarting your node"
                      : "Working..."}
                </div>
                <div
                  style={{
                    fontSize: "var(--text-sm)",
                    color: "var(--text-muted)",
                  }}
                >
                  {stage === "downloading" && progress
                    ? `${formatBytes(progress.downloadedBytes)} of ${formatBytes(progress.totalBytes)} (${progress.totalBytes > 0 ? Math.floor((progress.downloadedBytes / progress.totalBytes) * 100) : 0}%)`
                    : stage === "downloading"
                      ? "Connecting to Hugging Face..."
                      : "arc-node is reloading with --model and --community-mode."}
                </div>
                {stage === "downloading" && progress && (
                  <div
                    style={{
                      width: "100%",
                      height: 8,
                      borderRadius: 4,
                      background: "var(--surface)",
                      border: "1px solid var(--border)",
                      overflow: "hidden",
                      marginTop: "var(--space-4)",
                    }}
                  >
                    <div
                      style={{
                        width:
                          progress.totalBytes > 0
                            ? `${Math.min(100, (progress.downloadedBytes / progress.totalBytes) * 100)}%`
                            : "0%",
                        height: "100%",
                        background: "var(--arc-gradient)",
                        transition: "width 0.25s linear",
                      }}
                    />
                  </div>
                )}
              </div>
            )}

            {error && (
              <div
                style={{
                  padding: "var(--space-4)",
                  background: "rgba(248, 113, 113, 0.08)",
                  border: "1px solid rgba(248, 113, 113, 0.3)",
                  borderRadius: "var(--radius-md)",
                  marginBottom: "var(--space-4)",
                  color: "var(--text)",
                  fontSize: "var(--text-sm)",
                }}
                data-testid="upgrade-error"
              >
                <div style={{ fontWeight: 600, marginBottom: 4 }}>
                  Something went wrong
                </div>
                <div
                  style={{
                    color: "var(--text-muted)",
                    whiteSpace: "pre-wrap",
                  }}
                >
                  {error}
                </div>
              </div>
            )}

            {!busy && (
              <div
                style={{
                  display: "flex",
                  justifyContent: "flex-end",
                  gap: "var(--space-2)",
                }}
              >
                <button
                  className="btn btn-ghost"
                  onClick={() => setOpen(false)}
                >
                  Cancel
                </button>
                <button
                  className="btn btn-primary"
                  onClick={upgrade}
                  disabled={!selectedTier}
                  data-testid="btn-confirm-upgrade"
                >
                  {error ? "Retry" : "Download & enable worker mode"}{" "}
                  <Sparkles size={14} />
                </button>
              </div>
            )}
          </div>
        </div>
      )}
    </>
  );
}
