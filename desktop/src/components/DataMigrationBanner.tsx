import { Archive, X } from "lucide-react";
import { useState } from "react";
import type { DataMigrationNotice } from "../lib/types";
import { api } from "../lib/tauri";

export function DataMigrationBanner({
  notice,
  onDismissed,
}: {
  notice: DataMigrationNotice;
  onDismissed: () => void;
}) {
  const [dismissing, setDismissing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const dismiss = async () => {
    setDismissing(true);
    setError(null);
    try {
      await api.dismissDataMigrationNotice();
      onDismissed();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setDismissing(false);
    }
  };

  return (
    <div
      role="status"
      data-testid="data-migration-banner"
      style={{
        display: "flex",
        alignItems: "flex-start",
        gap: "var(--space-3)",
        padding: "var(--space-4) var(--space-5)",
        margin: "0 auto var(--space-5)",
        maxWidth: 1100,
        background: "var(--warning-bg)",
        color: "var(--text)",
        border: "1px solid color-mix(in srgb, var(--warning) 35%, transparent)",
        borderRadius: "var(--radius-md)",
      }}
    >
      <Archive
        size={18}
        style={{ color: "var(--warning)", flexShrink: 0, marginTop: 2 }}
      />
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ fontWeight: 600, marginBottom: 4 }}>
          Protocol-v3 safety fence protected your old block history
        </div>
        <div
          style={{
            color: "var(--text-secondary)",
            fontSize: "var(--text-sm)",
            lineHeight: 1.5,
          }}
        >
          ARC found chain data that could not be safely replayed on the active
          network. It left every old WAL, binding, and block byte untouched,
          preserved your identity and model selection, and selected a fresh
          data directory for v0.8.
        </div>
        <div
          data-testid="data-migration-reason"
          style={{
            color: "var(--text-muted)",
            fontSize: "var(--text-xs)",
            marginTop: 6,
          }}
        >
          Safety reason: {notice.reason}
        </div>
        <dl
          style={{
            display: "grid",
            gridTemplateColumns: "max-content minmax(0, 1fr)",
            gap: "4px var(--space-3)",
            marginTop: "var(--space-3)",
            fontSize: "var(--text-xs)",
          }}
        >
          <dt style={{ color: "var(--text-muted)" }}>Preserved v0.7 data</dt>
          <dd
            data-testid="legacy-data-dir"
            style={{ fontFamily: "var(--font-mono)", overflowWrap: "anywhere" }}
          >
            {notice.legacyDataDir}
          </dd>
          <dt style={{ color: "var(--text-muted)" }}>Active v3 data</dt>
          <dd
            data-testid="active-v3-data-dir"
            style={{ fontFamily: "var(--font-mono)", overflowWrap: "anywhere" }}
          >
            {notice.activeDataDir}
          </dd>
        </dl>
        {error && (
          <div role="alert" style={{ color: "var(--danger)", marginTop: 6 }}>
            Could not dismiss this notice: {error}
          </div>
        )}
      </div>
      <button
        className="btn btn-ghost btn-sm"
        type="button"
        onClick={dismiss}
        disabled={dismissing}
        aria-label="Dismiss data migration notice"
        data-testid="dismiss-data-migration"
        style={{ padding: 6, flexShrink: 0 }}
      >
        <X size={13} />
      </button>
    </div>
  );
}
