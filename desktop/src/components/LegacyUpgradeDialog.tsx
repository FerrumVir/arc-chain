import { Cpu } from "lucide-react";
import { useState } from "react";
import { api } from "../lib/tauri";
import { useAppStore } from "../lib/store";
import type { LegacyComputeQuestion } from "../lib/types";

/**
 * Asked once, on the first launch after a v0.7 install reached this app. The
 * node stays an observer without a model until the user answers; the answer
 * is recorded exactly like the Settings "Let ARC run inference jobs on this
 * computer" switch, so it can be changed there at any time.
 */
export function LegacyUpgradeDialog({
  question,
  onAnswered,
}: {
  question: LegacyComputeQuestion;
  onAnswered: () => void;
}) {
  const setConfig = useAppStore((s) => s.setConfig);
  const [busy, setBusy] = useState<"yes" | "no" | null>(null);
  const [error, setError] = useState<string | null>(null);

  const answer = async (contribute: boolean) => {
    setBusy(contribute ? "yes" : "no");
    setError(null);
    try {
      const saved = await api.answerLegacyComputeQuestion(contribute);
      setConfig(saved);
      onAnswered();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setBusy(null);
    }
  };

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-labelledby="legacy-upgrade-title"
      aria-describedby="legacy-upgrade-question"
      data-testid="legacy-upgrade-dialog"
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
          id="legacy-upgrade-title"
          style={{
            fontSize: "var(--text-xl)",
            fontWeight: 600,
            color: "var(--text)",
            marginBottom: "var(--space-2)",
          }}
        >
          Your ARC node is upgrading to the new network.
        </h2>
        <p
          id="legacy-upgrade-question"
          style={{ fontSize: "var(--text-lg)", color: "var(--text)", marginBottom: "var(--space-3)" }}
        >
          Keep contributing compute?
        </p>
        <p
          style={{
            color: "var(--text-muted)",
            fontSize: "var(--text-sm)",
            lineHeight: 1.5,
            marginBottom: "var(--space-5)",
          }}
        >
          Yes downloads the ARC model if it is not on this computer yet (3.80 GB,
          checked against its pinned SHA-256) and lets ARC run inference jobs
          here. Not now keeps your node on the network as an observer, with no
          compute. You can change this any time in Settings.
          {question.previousRole === "worker" &&
            " Before the upgrade, this computer ran inference jobs."}
        </p>
        {error && (
          <div
            role="alert"
            data-testid="legacy-upgrade-error"
            style={{ color: "var(--danger)", fontSize: "var(--text-sm)", marginBottom: "var(--space-4)" }}
          >
            {error}
          </div>
        )}
        <div style={{ display: "flex", justifyContent: "flex-end", gap: "var(--space-3)" }}>
          <button
            type="button"
            className="btn btn-secondary"
            onClick={() => answer(false)}
            disabled={busy !== null}
            data-testid="legacy-upgrade-not-now"
          >
            {busy === "no" ? "Saving…" : "Not now"}
          </button>
          <button
            type="button"
            className="btn btn-primary"
            onClick={() => answer(true)}
            disabled={busy !== null}
            data-testid="legacy-upgrade-yes"
          >
            {busy === "yes" ? "Starting…" : "Yes, keep contributing"}
          </button>
        </div>
      </div>
    </div>
  );
}
