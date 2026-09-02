import { HelpCircle } from "lucide-react";

/**
 * The one way this app says "we don't know".
 *
 * Every chain read in the projection and Network screens can come back
 * unavailable — most of the endpoints behind them are newer than the deployed
 * seed binaries, so a 404 is the expected path, not an exceptional one. The
 * rule that makes that safe is: state the reason, name the host, and show no
 * number at all. A zero, a dash, or a stale cached figure would each read as a
 * measurement.
 *
 * `reason` is always a sentence produced by the fetch layer, which knows what
 * it observed (404 vs. unreachable vs. unparseable) and never guesses why.
 */
export function NotAvailable({
  reason,
  title = "Not available from this host",
  testId,
}: {
  reason: string;
  title?: string;
  testId?: string;
}) {
  return (
    <div
      data-testid={testId}
      role="status"
      style={{
        display: "flex",
        gap: "var(--space-3)",
        alignItems: "flex-start",
        padding: "var(--space-3) var(--space-4)",
        background: "var(--bg)",
        border: "1px dashed var(--border)",
        borderRadius: "var(--radius-sm)",
        fontSize: "var(--text-sm)",
        lineHeight: 1.6,
      }}
    >
      <HelpCircle
        size={15}
        style={{ color: "var(--text-muted)", flexShrink: 0, marginTop: 2 }}
      />
      <div>
        <div style={{ color: "var(--text-secondary)", fontWeight: 500 }}>
          {title}
        </div>
        <div style={{ color: "var(--text-muted)", wordBreak: "break-word" }}>
          {reason}
        </div>
      </div>
    </div>
  );
}
