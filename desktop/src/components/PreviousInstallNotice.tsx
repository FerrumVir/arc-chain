import { Archive, X } from "lucide-react";
import { useState } from "react";
import type { PreviousInstallNotice as Notice } from "../lib/types";
import { PLAIN_COPY } from "../lib/plain-copy";
import { api } from "../lib/tauri";

/**
 * One calm sentence: launch found node data from an earlier ARC install whose
 * app data was gone, and set it aside (renamed beside itself, never deleted).
 * Shown during setup and on every screen after it until dismissed. A status,
 * not a problem: the node starts fresh, and a wallet comes back from its 12
 * words.
 */
export function PreviousInstallNotice({
  notice,
  onDismissed,
}: {
  notice: Notice;
  onDismissed: () => void;
}) {
  const [dismissing, setDismissing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const dismiss = async () => {
    setDismissing(true);
    setError(null);
    try {
      await api.dismissPreviousInstallNotice();
      onDismissed();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setDismissing(false);
    }
  };

  return (
    <div
      role="status"
      data-testid="previous-install-notice"
      style={{
        display: "flex",
        alignItems: "flex-start",
        gap: "var(--space-3)",
        padding: "var(--space-4) var(--space-5)",
        margin: "0 auto var(--space-5)",
        maxWidth: 1100,
        width: "100%",
        background: "var(--surface)",
        color: "var(--text)",
        border: "1px solid var(--border)",
        borderRadius: "var(--radius-md)",
        textAlign: "left",
      }}
    >
      <Archive
        size={18}
        style={{ color: "var(--text-muted)", flexShrink: 0, marginTop: 2 }}
      />
      <div style={{ flex: 1, minWidth: 0 }}>
        <div data-testid="previous-install-sentence" style={{ lineHeight: 1.5 }}>
          {PLAIN_COPY.previousInstall.sentence}
        </div>
        <div
          data-testid="previous-install-path"
          style={{
            color: "var(--text-muted)",
            fontSize: "var(--text-xs)",
            fontFamily: "var(--font-mono)",
            overflowWrap: "anywhere",
            marginTop: 6,
          }}
        >
          {notice.setAsideDataDir}
        </div>
        {error && (
          <div role="alert" style={{ color: "var(--danger)", marginTop: 6 }}>
            {PLAIN_COPY.previousInstall.dismissFailed} {error}
          </div>
        )}
      </div>
      <button
        className="btn btn-ghost btn-sm"
        type="button"
        onClick={dismiss}
        disabled={dismissing}
        aria-label={PLAIN_COPY.previousInstall.dismiss}
        data-testid="dismiss-previous-install"
        style={{ padding: 6, flexShrink: 0 }}
      >
        <X size={13} />
      </button>
    </div>
  );
}
