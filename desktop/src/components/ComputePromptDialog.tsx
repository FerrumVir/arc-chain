import { Cpu } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api } from "../lib/tauri";
import { useAppStore } from "../lib/store";

/** Visible acknowledgement is separate from claiming the pending notice. */
export function ComputePromptDialog({ onClose }: { onClose: () => void }) {
  const setConfig = useAppStore((s) => s.setConfig);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const notNowRef = useRef<HTMLButtonElement>(null);
  const savedKeepAwake = useAppStore((s) => s.config?.keepAwakeWhileSharing === true);
  const [keepAwake, setKeepAwake] = useState(savedKeepAwake);

  // Dismissing the notice leaves sharing and the independent opt-in intact.
  useEffect(() => {
    notNowRef.current?.focus();
    return api.whenMainWindowSeen(() => {
      void api.acknowledgeComputeNotice().catch(() => undefined);
    });
  }, []);

  useEffect(() => {
    if (busy) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [busy, onClose]);

  const turnOff = async () => {
    setBusy(true);
    setError(null);
    try {
      const saved = await api.setComputeContribution(false);
      setConfig(saved);
      onClose();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setBusy(false);
    }
  };

  const toggleAwake = async (enabled: boolean) => {
    setBusy(true);
    setError(null);
    try {
      const saved = await api.setKeepAwakeWhileSharing(enabled);
      setConfig(saved);
      setKeepAwake(saved.keepAwakeWhileSharing === true);
    } catch (cause) {
      setError(String(cause));
    } finally { setBusy(false); }
  };

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="compute-prompt-title"
      aria-describedby="compute-prompt-body"
      data-testid="compute-prompt-dialog"
      style={{
        position: "fixed",
        inset: 0,
        background: "rgba(0,0,0,0.5)",
        display: "grid",
        placeItems: "center",
        zIndex: 60,
      }}
    >
      <div
        style={{
          width: "min(520px, 90vw)",
          background: "var(--bg)",
          border: "1px solid var(--border)",
          borderRadius: "var(--radius-lg)",
          padding: "var(--space-6)",
          boxShadow: "var(--shadow-lg)",
        }}
      >
        <Cpu size={22} style={{ color: "var(--info)", marginBottom: "var(--space-3)" }} />
        <h2
          id="compute-prompt-title"
          style={{
            fontSize: "var(--text-xl)",
            fontWeight: 600,
            color: "var(--text)",
            marginBottom: "var(--space-2)",
          }}
        >
          ARC now shares this computer’s spare compute
        </h2>
        <p
          id="compute-prompt-body"
          data-testid="compute-prompt-body"
          style={{
            color: "var(--text-muted)",
            fontSize: "var(--text-sm)",
            lineHeight: 1.5,
            marginBottom: "var(--space-5)",
          }}
        >
          Turn it off any time in Settings. The model downloads in the background when this computer is plugged in and the network allows it.
        </p>
        <label style={{ display: "flex", gap: "var(--space-3)", marginBottom: "var(--space-4)" }}>
          <input type="checkbox" checked={keepAwake} disabled={busy}
            onChange={(event) => void toggleAwake(event.target.checked)}
            data-testid="compute-notice-keep-awake" />
          <span>Keep this computer awake while plugged in so it can take jobs.</span>
        </label>
        {error && (
          <div
            role="alert"
            data-testid="compute-prompt-error"
            style={{ color: "var(--danger)", fontSize: "var(--text-sm)", marginBottom: "var(--space-4)" }}
          >
            {error}
          </div>
        )}
        <div style={{ display: "flex", justifyContent: "flex-end", gap: "var(--space-3)" }}>
          <button
            type="button"
            className="btn btn-secondary"
            ref={notNowRef}
            onClick={onClose}
            disabled={busy}
            data-testid="compute-prompt-not-now"
          >
            Got it
          </button>
          <button
            type="button"
            className="btn btn-primary"
            onClick={() => void turnOff()}
            disabled={busy}
            aria-busy={busy || undefined}
            data-testid="compute-prompt-turn-off"
          >
            Turn off
          </button>
        </div>
      </div>
    </div>
  );
}
