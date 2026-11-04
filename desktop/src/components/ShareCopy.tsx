import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { invoke } from "../lib/tauri";
import type { NodeCarePlatform } from "../lib/node-care";
import { scrubForSharing } from "../lib/support";

const COPIED_RESET_MS = 2_500;

export const SHARE_COPY = {
  fallback: "Couldn't copy. Select the text below and copy it yourself. Personal details are already removed.",
  localOnly: "Shown only on this computer, exactly as ARC recorded it. Copy details removes personal details first.",
} as const;

/** The home folder the native side reports, so it can be scrubbed; never shown or sent. */
export function useHomeDir(): string | null {
  const platform = useQuery({
    queryKey: ["node-care-info"],
    queryFn: () => invoke<NodeCarePlatform>("node_care_info"),
    staleTime: Infinity,
  });
  return platform.data?.homeDir ?? null;
}

/**
 * Copy details for sharing: what reaches the clipboard is always the scrubbed
 * text, never the raw diagnostics. "failed" stays until the next attempt, so
 * the manual-copy fallback (also scrubbed) does not vanish under the reader.
 */
export function useShareCopy(raw: string | null | undefined) {
  const homeDir = useHomeDir();
  const shareText = raw ? scrubForSharing(raw, homeDir) : "";
  const [copied, setCopied] = useState<"idle" | "copied" | "failed">("idle");
  useEffect(() => {
    if (copied !== "copied") return;
    const timer = window.setTimeout(() => setCopied("idle"), COPIED_RESET_MS);
    return () => window.clearTimeout(timer);
  }, [copied]);
  const copy = () => {
    if (!shareText) return;
    // Without a clipboard API the call throws before returning a promise.
    void Promise.resolve()
      .then(() => navigator.clipboard.writeText(shareText))
      .then(
        () => setCopied("copied"),
        () => setCopied("failed"),
      );
  };
  return { shareText, copied, copy };
}

/** After a failed copy: the scrubbed text, selectable, with what to do. */
export function ShareCopyFallback({ text, testId }: { text: string; testId?: string }) {
  return (
    <div role="alert" data-testid={testId} style={{ marginTop: "var(--space-3)" }}>
      <p style={{ margin: 0, color: "var(--danger)" }}>{SHARE_COPY.fallback}</p>
      <pre
        data-testid={testId ? `${testId}-text` : undefined}
        style={{
          marginTop: "var(--space-2)",
          marginBottom: 0,
          padding: "var(--space-3)",
          background: "var(--surface)",
          border: "1px solid var(--border)",
          borderRadius: "var(--radius-sm)",
          color: "var(--text)",
          fontFamily: "var(--font-mono)",
          fontSize: "var(--text-xs)",
          whiteSpace: "pre-wrap",
          wordBreak: "break-word",
          userSelect: "text",
        }}
      >
        {text}
      </pre>
    </div>
  );
}
