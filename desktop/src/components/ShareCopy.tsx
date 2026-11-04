import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { invoke } from "../lib/tauri";
import type { NodeCarePlatform } from "../lib/node-care";
import { PLAIN_COPY } from "../lib/plain-copy";
import { privacyContextFrom, sharePayload, type PrivacyContext } from "../lib/support";

const COPIED_RESET_MS = 2_500;

export const SHARE_COPY = PLAIN_COPY.share;

/**
 * The platform the native side reports (versions, system, home folder) and
 * the privacy context it gives: ready only once a home folder has arrived.
 * `retry` asks again after a failed or empty lookup; a later answer makes the
 * detailed (scrubbed) payload available without anything stale left behind,
 * because every payload is derived from the current context on render.
 */
export function usePrivacyContext(): { privacy: PrivacyContext; platform: NodeCarePlatform | null; retry: () => void } {
  const lookup = useQuery({
    queryKey: ["node-care-info"],
    queryFn: () => invoke<NodeCarePlatform>("node_care_info"),
    staleTime: Infinity,
    // One lookup, then explicit Copy details retries. No background recovery loop.
    retry: false,
    retryOnMount: false,
    refetchOnReconnect: false,
  });
  const platform = lookup.status === "success" ? lookup.data : null;
  const privacy = privacyContextFrom({ status: lookup.status, homeDir: platform?.homeDir });
  const retry = () => {
    // Also deduplicate same-turn clicks/observers before isFetching has rerendered.
    if (privacy.state !== "ready" && !lookup.isFetching) void lookup.refetch({ cancelRefetch: false });
  };
  return { privacy, platform, retry };
}

/**
 * Copy details for sharing, under one policy (lib/support.ts sharePayload):
 * the clipboard and the manual-copy fallback always get the same payload,
 * scrubbed native text when the privacy context is ready and a safe summary
 * when it isn't. A failed copy belongs to the text it was made for: when the
 * diagnostics change, the fallback goes away instead of showing another
 * error's text.
 *
 * `title` must be app-authored copy (PLAIN_COPY): it is the only context the
 * summary keeps.
 */
export function useShareCopy(raw: string | null | undefined, title?: string | null) {
  const { privacy, retry } = usePrivacyContext();
  const payload = raw ? sharePayload({ raw, title, privacy }) : null;
  const [attempt, setAttempt] = useState<{ status: "copied" | "failed"; raw: string } | null>(null);
  const copied = attempt && raw && attempt.raw === raw ? attempt.status : "idle";

  useEffect(() => {
    if (attempt?.status !== "copied") return;
    const timer = window.setTimeout(() => setAttempt(null), COPIED_RESET_MS);
    return () => window.clearTimeout(timer);
  }, [attempt]);

  const copy = () => {
    if (!raw || !payload) return;
    // Without the context, ask again so a later Copy details can share the detailed text.
    if (!payload.detailed) retry();
    const text = payload.text;
    // Without a clipboard API the call throws before returning a promise.
    void Promise.resolve()
      .then(() => navigator.clipboard.writeText(text))
      .then(
        () => setAttempt({ status: "copied", raw }),
        () => setAttempt({ status: "failed", raw }),
      );
  };
  return { shareText: payload?.text ?? "", detailed: payload?.detailed ?? false, copied, copy };
}

/** After a failed copy: the payload Copy details would have shared, selectable, and what to check. */
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
      <p style={{ margin: "var(--space-2) 0 0", color: "var(--text-muted)" }}>{SHARE_COPY.review}</p>
    </div>
  );
}

/** Under a raw native text that stays on this computer. */
export function LocalOnlyNote({ testId }: { testId?: string }) {
  return (
    <p data-testid={testId} style={{ flexBasis: "100%", margin: "var(--space-2) 0 0", color: "var(--text-muted)", fontSize: "var(--text-xs)" }}>
      {SHARE_COPY.localOnly}
    </p>
  );
}
