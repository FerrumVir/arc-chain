import { useEffect, useId, useState, type ReactNode } from "react";
import { AlertTriangle, Check, Copy, Info } from "lucide-react";
import { PLAIN_COPY } from "../lib/plain-copy";

const COPIED_RESET_MS = 2_500;

export function ProblemNotice({
  title,
  body,
  details,
  detailsOpen = false,
  tone = "attention",
  actions,
  onDismiss,
  dismissLabel = "Dismiss",
  dismissTestId,
  testId,
}: {
  title: string;
  body?: ReactNode;
  details?: string | null;
  detailsOpen?: boolean;
  tone?: "attention" | "status";
  actions?: ReactNode;
  onDismiss?: () => void;
  dismissLabel?: string;
  dismissTestId?: string;
  testId?: string;
}) {
  const [toggled, setToggled] = useState<{ details: string; open: boolean } | null>(null);
  const open = toggled !== null && toggled.details === details ? toggled.open : detailsOpen;
  const [copied, setCopied] = useState<"idle" | "copied" | "failed">("idle");
  const detailsId = useId();

  useEffect(() => {
    if (copied === "idle") return;
    const timer = window.setTimeout(() => setCopied("idle"), COPIED_RESET_MS);
    return () => window.clearTimeout(timer);
  }, [copied]);

  const copyDetails = () => {
    if (!details) return;
    void Promise.resolve()
      .then(() => navigator.clipboard.writeText(details))
      .then(
        () => setCopied("copied"),
        () => setCopied("failed"),
      );
  };

  const attention = tone === "attention";
  const Icon = attention ? AlertTriangle : Info;
  return (
    <div
      role={attention ? "alert" : "status"}
      data-testid={testId}
      data-tone={tone}
      style={{
        display: "flex",
        gap: "var(--space-3)",
        alignItems: "flex-start",
        padding: "var(--space-4) var(--space-5)",
        background: attention ? "var(--danger-bg)" : "var(--surface)",
        border: attention
          ? "1px solid rgba(240, 115, 115, 0.3)"
          : "1px solid var(--border)",
        borderRadius: "var(--radius-md)",
        marginBottom: "var(--space-5)",
        color: "var(--text)",
        fontSize: "var(--text-sm)",
      }}
    >
      <Icon
        size={18}
        aria-hidden="true"
        style={{ color: attention ? "var(--danger)" : "var(--text-muted)", flexShrink: 0, marginTop: 1 }}
      />
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ fontWeight: 600, marginBottom: 2 }} data-testid={testId ? `${testId}-title` : undefined}>
          {title}
        </div>
        {body && <div style={{ color: "var(--text-muted)", lineHeight: 1.5 }}>{body}</div>}
        <div style={{ display: "flex", flexWrap: "wrap", gap: "var(--space-2)", marginTop: "var(--space-3)" }}>
          {actions}
          {details && (
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={() => setToggled({ details, open: !open })}
              aria-expanded={open}
              aria-controls={detailsId}
              data-testid={testId ? `${testId}-details-toggle` : undefined}
            >
              {open ? PLAIN_COPY.actions.hideDetails : PLAIN_COPY.actions.showDetails}
            </button>
          )}
          {details && (
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={copyDetails}
              data-testid={testId ? `${testId}-copy` : undefined}
            >
              {copied === "copied" ? <Check size={12} /> : <Copy size={12} />}{" "}
              {copied === "copied"
                ? PLAIN_COPY.actions.copied
                : copied === "failed"
                  ? PLAIN_COPY.actions.copyFailed
                  : PLAIN_COPY.actions.copyDetails}
            </button>
          )}
        </div>
        <span className="sr-only" aria-live="polite">
          {copied === "copied"
            ? PLAIN_COPY.actions.copied
            : copied === "failed"
              ? PLAIN_COPY.actions.copyFailed
              : ""}
        </span>
        {details && (
          <pre
            id={detailsId}
            hidden={!open}
            data-testid={testId ? `${testId}-details` : undefined}
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
            {details}
          </pre>
        )}
      </div>
      {onDismiss && (
        <button
          type="button"
          className="btn btn-ghost btn-sm"
          onClick={onDismiss}
          aria-label={dismissLabel}
          data-testid={dismissTestId ?? (testId ? `${testId}-dismiss` : undefined)}
          style={{ padding: 6, flexShrink: 0 }}
        >
          ×
        </button>
      )}
    </div>
  );
}
