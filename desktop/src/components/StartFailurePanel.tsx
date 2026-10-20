import { useEffect, useId, useState } from "react";
import { AlertTriangle, Check, Copy, Download, RotateCw } from "lucide-react";
import { relaunch } from "@tauri-apps/plugin-process";
import { isTauri } from "../lib/tauri";
import { appUpdater, useUpdaterSnapshot } from "../lib/updater";
import { PLAIN_COPY } from "../lib/plain-copy";
import { describeStartFailure } from "../lib/start-failure";

const COPIED_RESET_MS = 2_500;

/**
 * A node that would not start, in one plain sentence with one action. The
 * native error chain stays one click away for support, behind "Show details",
 * instead of being the message; when that text is the actionable part (a
 * repair, or an error nobody anticipated) it starts open. Through v0.8.13 the
 * chain was the message, and a stuck desktop showed a paragraph that repeated
 * itself.
 *
 * `onTryAgain` is for screens without their own retry button.
 */
export function StartFailurePanel({
  error,
  onTryAgain,
  testId,
}: {
  error: string;
  onTryAgain?: () => void;
  testId?: string;
}) {
  const update = useUpdaterSnapshot();
  const updateAvailable = update.phase === "available" && update.canInstall;
  const installing = update.phase === "downloading" || update.phase === "ready";
  const view = describeStartFailure(error, { updateAvailable });
  // A toggle applies to the error it was made on; a new error starts from
  // its own default.
  const [toggled, setToggled] = useState<{ error: string; open: boolean } | null>(null);
  const open = toggled?.error === error ? toggled.open : view.detailsOpen;
  const [copied, setCopied] = useState<"idle" | "copied" | "failed">("idle");
  const detailsId = useId();

  useEffect(() => {
    if (copied === "idle") return;
    const timer = window.setTimeout(() => setCopied("idle"), COPIED_RESET_MS);
    return () => window.clearTimeout(timer);
  }, [copied]);

  const restartArc = () => {
    // Relaunching runs launch-time reconciliation again. Outside the native
    // shell (browser previews) a reload is the closest equivalent.
    if (isTauri) void relaunch();
    else window.location.reload();
  };

  const copyDetails = () => {
    // Without a clipboard API the call throws before returning a promise.
    void Promise.resolve()
      .then(() => navigator.clipboard.writeText(view.details))
      .then(
        () => setCopied("copied"),
        () => setCopied("failed"),
      );
  };

  const waiting = view.kind === "reconciling";
  return (
    <div
      data-testid={testId}
      data-failure-kind={view.kind}
      style={{
        padding: "var(--space-4)",
        background: waiting ? "var(--surface)" : "rgba(248, 113, 113, 0.08)",
        border: waiting
          ? "1px solid var(--border)"
          : "1px solid rgba(248, 113, 113, 0.3)",
        borderRadius: "var(--radius-md)",
        marginBottom: "var(--space-4)",
        color: "var(--text)",
        fontSize: "var(--text-sm)",
      }}
    >
      <div style={{ display: "flex", gap: "var(--space-3)", alignItems: "flex-start" }}>
        {waiting ? (
          <RotateCw size={16} className="spin" style={{ flexShrink: 0, marginTop: 2 }} />
        ) : (
          <AlertTriangle
            size={16}
            style={{ color: "var(--danger)", flexShrink: 0, marginTop: 2 }}
          />
        )}
        <div style={{ flex: 1, minWidth: 0 }}>
          {/* Only the sentence is announced. Buttons, details and progress
              below change without re-reading it. */}
          <div role={waiting ? "status" : "alert"}>
            <div style={{ fontWeight: 600, marginBottom: 4 }} data-testid="start-failure-title">
              {view.title}
            </div>
            <div style={{ color: "var(--text-muted)" }}>
              {view.body}
              {view.action === "install-update" &&
                ` ${PLAIN_COPY.startFailure.updateReady}`}
            </div>
          </div>

          <div
            style={{
              display: "flex",
              flexWrap: "wrap",
              gap: "var(--space-2)",
              marginTop: "var(--space-3)",
            }}
          >
            {view.action === "install-update" ? (
              <button
                type="button"
                className="btn btn-primary btn-sm"
                onClick={() => void appUpdater.installAvailableUpdate()}
                disabled={installing}
                aria-busy={installing || undefined}
                data-testid="btn-start-failure-install"
              >
                <Download size={12} />{" "}
                {installing
                  ? PLAIN_COPY.actions.installing
                  : PLAIN_COPY.actions.installUpdate}
              </button>
            ) : view.action === "restart-arc" ? (
              <button
                type="button"
                className="btn btn-primary btn-sm"
                onClick={restartArc}
                data-testid="btn-start-failure-restart"
              >
                <RotateCw size={12} /> {PLAIN_COPY.actions.restartArc}
              </button>
            ) : (
              onTryAgain && (
                <button
                  type="button"
                  className="btn btn-primary btn-sm"
                  onClick={onTryAgain}
                  data-testid="btn-start-failure-retry"
                >
                  <RotateCw size={12} /> {PLAIN_COPY.actions.tryAgain}
                </button>
              )
            )}
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={() => setToggled({ error, open: !open })}
              aria-expanded={open}
              aria-controls={detailsId}
              data-testid="btn-start-failure-details"
            >
              {open ? PLAIN_COPY.actions.hideDetails : PLAIN_COPY.actions.showDetails}
            </button>
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={copyDetails}
              data-testid="btn-start-failure-copy"
            >
              {copied === "copied" ? <Check size={12} /> : <Copy size={12} />}{" "}
              {copied === "copied"
                ? PLAIN_COPY.actions.copied
                : copied === "failed"
                  ? PLAIN_COPY.actions.copyFailed
                  : PLAIN_COPY.actions.copyDetails}
            </button>
          </div>

          {/* The copy result, announced on its own. */}
          <span className="sr-only" aria-live="polite">
            {copied === "copied"
              ? PLAIN_COPY.actions.copied
              : copied === "failed"
                ? PLAIN_COPY.actions.copyFailed
                : ""}
          </span>

          {installing && update.message && (
            <div style={{ marginTop: "var(--space-2)", color: "var(--text-muted)" }}>
              {update.message}
            </div>
          )}

          {/* Rendered while hidden so aria-controls always names a real element. */}
          <pre
            id={detailsId}
            hidden={!open}
            data-testid="start-failure-details"
            style={{
              marginTop: "var(--space-3)",
              marginBottom: 0,
              padding: "var(--space-3)",
              background: "var(--surface)",
              border: "1px solid var(--border)",
              borderRadius: "var(--radius-sm)",
              color: "var(--text-muted)",
              fontFamily: "var(--font-mono)",
              fontSize: "var(--text-xs)",
              whiteSpace: "pre-wrap",
              wordBreak: "break-word",
            }}
          >
            {view.details}
          </pre>
        </div>
      </div>
    </div>
  );
}
