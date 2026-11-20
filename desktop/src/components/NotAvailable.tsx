import { HelpCircle } from "lucide-react";

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
