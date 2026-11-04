import { useEffect, useId, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Check, Copy, ExternalLink, LifeBuoy } from "lucide-react";
import { api } from "../lib/tauri";
import { useNodeCareSnapshot } from "../lib/node-care-runtime";
import type { NodeStatus } from "../lib/types";
import { PLAIN_COPY } from "../lib/plain-copy";
import { SUPPORT_LINKS, buildSupportDetails, describeReportState, type HelpContext } from "../lib/support";
import { SHARE_COPY, usePrivacyContext } from "./ShareCopy";

const COPIED_RESET_MS = 2_500;

/**
 * "Get help": the way from a problem to a person. Copy the details (scrubbed),
 * then post them in the community chats the README names, plus one honest line
 * about whether Node Care's anonymous report actually left this computer.
 *
 * It promises no reply and names nobody as reading the chats: until an owner
 * is confirmed, the app only says where to post.
 */
export function GetHelp({
  context,
  title,
  error,
  defaultOpen = false,
  testId = "get-help",
}: {
  context: HelpContext;
  title?: string | null;
  error?: string | null;
  defaultOpen?: boolean;
  testId?: string;
}) {
  const copy = PLAIN_COPY.help;
  const [open, setOpen] = useState(defaultOpen);
  const [copied, setCopied] = useState<"idle" | "copied" | "failed">("idle");
  const [openFailed, setOpenFailed] = useState<string | null>(null);
  const panelId = useId();
  const queryClient = useQueryClient();
  const snapshot = useNodeCareSnapshot();
  const { privacy, platform, retry } = usePrivacyContext();
  const status = queryClient.getQueryData<NodeStatus>(["status"]);

  useEffect(() => {
    if (copied !== "copied") return;
    const timer = window.setTimeout(() => setCopied("idle"), COPIED_RESET_MS);
    return () => window.clearTimeout(timer);
  }, [copied]);

  const details = buildSupportDetails({
    context,
    appVersion: platform?.appVersion ?? null,
    nodeVersion: status?.version ?? null,
    os: platform?.os ?? null,
    arch: platform?.arch ?? null,
    title,
    error,
    privacy,
  });
  const report = describeReportState(snapshot);

  const copyDetails = () => {
    // Without the privacy context the native text is left out; ask again for a later copy.
    if (error && privacy.state !== "ready") retry();
    // Without a clipboard API the call throws before returning a promise.
    void Promise.resolve()
      .then(() => navigator.clipboard.writeText(details))
      .then(
        () => setCopied("copied"),
        () => setCopied("failed"),
      );
  };

  const openLink = (url: string) => {
    setOpenFailed(null);
    void Promise.resolve()
      .then(() => api.openExternal(url))
      .catch(() => setOpenFailed(url));
  };

  return (
    <div data-testid={testId} data-context={context} style={{ marginTop: "var(--space-3)" }}>
      <button
        type="button"
        className="btn btn-ghost btn-sm"
        onClick={() => setOpen(!open)}
        aria-expanded={open}
        aria-controls={panelId}
        data-testid={`${testId}-toggle`}
      >
        <LifeBuoy size={12} aria-hidden="true" /> {open ? copy.hide : copy.button}
      </button>
      {open && (
        <section
          id={panelId}
          aria-label={copy.title}
          data-testid={`${testId}-panel`}
          style={{
            marginTop: "var(--space-3)",
            padding: "var(--space-4)",
            background: "var(--surface)",
            border: "1px solid var(--border)",
            borderRadius: "var(--radius-md)",
            color: "var(--text)",
            fontSize: "var(--text-sm)",
            lineHeight: 1.5,
          }}
        >
          <p style={{ margin: 0 }}>{copy.stepCopy}</p>
          <pre
            data-testid={`${testId}-details`}
            style={{
              margin: "var(--space-2) 0",
              padding: "var(--space-3)",
              background: "var(--bg)",
              border: "1px solid var(--border)",
              borderRadius: "var(--radius-sm)",
              color: "var(--text-muted)",
              fontFamily: "var(--font-mono)",
              fontSize: "var(--text-xs)",
              whiteSpace: "pre-wrap",
              wordBreak: "break-word",
              userSelect: "text",
            }}
          >
            {details}
          </pre>
          <button type="button" className="btn btn-secondary btn-sm" onClick={copyDetails} data-testid={`${testId}-copy`}>
            {copied === "copied" ? <Check size={12} /> : <Copy size={12} />}{" "}
            {copied === "copied" ? PLAIN_COPY.actions.copied : PLAIN_COPY.actions.copyDetails}
          </button>
          {copied === "failed" && (
            <p role="alert" data-testid={`${testId}-copy-failed`} style={{ margin: "var(--space-2) 0 0", color: "var(--danger)" }}>
              {copy.copyFailedFallback} {SHARE_COPY.review}
            </p>
          )}
          <span className="sr-only" aria-live="polite">
            {copied === "copied" ? PLAIN_COPY.actions.copied : ""}
          </span>

          <p style={{ margin: "var(--space-4) 0 var(--space-2)" }}>{copy.stepPost}</p>
          <div style={{ display: "flex", flexWrap: "wrap", gap: "var(--space-2)" }}>
            <button type="button" className="btn btn-secondary btn-sm" onClick={() => openLink(SUPPORT_LINKS.discord)} data-testid={`${testId}-discord`}>
              <ExternalLink size={12} aria-hidden="true" /> {copy.openDiscord}
            </button>
            <button type="button" className="btn btn-secondary btn-sm" onClick={() => openLink(SUPPORT_LINKS.telegram)} data-testid={`${testId}-telegram`}>
              <ExternalLink size={12} aria-hidden="true" /> {copy.openTelegram}
            </button>
            <button type="button" className="btn btn-ghost btn-sm" onClick={() => openLink(SUPPORT_LINKS.issues)} data-testid={`${testId}-issue`}>
              <ExternalLink size={12} aria-hidden="true" /> {copy.openIssue}
            </button>
            <button type="button" className="btn btn-ghost btn-sm" onClick={() => openLink(SUPPORT_LINKS.docs)} data-testid={`${testId}-docs`}>
              <ExternalLink size={12} aria-hidden="true" /> {copy.openDocs}
            </button>
          </div>
          {openFailed && (
            <p role="alert" data-testid={`${testId}-open-failed`} style={{ margin: "var(--space-2) 0 0", color: "var(--danger)", userSelect: "text" }}>
              {copy.openFailed(openFailed)}
            </p>
          )}
          <p style={{ margin: "var(--space-3) 0 0", color: "var(--text-muted)" }}>{copy.safety}</p>
          <p data-testid={`${testId}-report`} data-report={report.kind} style={{ margin: "var(--space-2) 0 0", color: "var(--text-muted)" }}>
            {report.text}
          </p>
        </section>
      )}
    </div>
  );
}
