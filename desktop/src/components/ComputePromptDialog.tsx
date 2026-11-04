import { Cpu } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api } from "../lib/tauri";
import { useAppStore } from "../lib/store";
import { PLAIN_COPY } from "../lib/plain-copy";

/**
 * Asked once, on the first launch after the update that made setup start with
 * Contribute compute on, of an install that finished setup before that and
 * has compute off on a computer that can run the ARC model. Native code
 * decides that and records that it asked before this shows
 * (`claim_compute_prompt`), so closing ARC with the dialog up never asks
 * again. It never shows to an install that has just finished setup, which
 * was asked the same question there.
 *
 * It says what the setup step says, word for word: the same title and body.
 * "Turn on" stores what a setup that leaves compute on stores and starts the
 * model download in the background, the same call setup makes. "Not now"
 * and Escape close it and change nothing; compute stays off, and Settings
 * turns it on at any time.
 */
export function ComputePromptDialog({ onClose }: { onClose: () => void }) {
  const setConfig = useAppStore((s) => s.setConfig);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const notNowRef = useRef<HTMLButtonElement>(null);
  const copy = PLAIN_COPY.setup.compute;

  // Not now is the answer that changes nothing, so it starts with the focus.
  useEffect(() => {
    notNowRef.current?.focus();
  }, []);

  useEffect(() => {
    if (busy) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [busy, onClose]);

  const turnOn = async () => {
    setBusy(true);
    setError(null);
    try {
      const saved = await api.acceptComputePrompt();
      setConfig(saved);
      onClose();
      // The model downloads in the background and the node becomes a worker
      // when it is ready: the call setup makes with compute on. A failure
      // leaves compute on, and the next launch tries again.
      void api.promoteConsentedInstall().then(
        (promoted) => setConfig(promoted),
        () => undefined,
      );
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setBusy(false);
    }
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
          {copy.title}
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
          {copy.body}
        </p>
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
            {copy.notNow}
          </button>
          <button
            type="button"
            className="btn btn-primary"
            onClick={() => void turnOn()}
            disabled={busy}
            aria-busy={busy || undefined}
            data-testid="compute-prompt-turn-on"
          >
            {copy.turnOn}
          </button>
        </div>
      </div>
    </div>
  );
}
