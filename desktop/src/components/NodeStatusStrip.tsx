import { useEffect, useId, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Check, CheckCircle2, CircleAlert, Copy, Download, Loader2, Power } from "lucide-react";
import { exit, relaunch } from "@tauri-apps/plugin-process";
import { api, isTauri } from "../lib/tauri";
import { useAppStore } from "../lib/store";
import { appUpdater, useUpdaterSnapshot } from "../lib/updater";
import { describeNodeStrip, type NodeStripAction, type NodeStripTone } from "../lib/node-strip";
import { PLAIN_COPY } from "../lib/plain-copy";
import { DEFAULT_NODE_CONFIG, type ModelDownloadProgress } from "../lib/types";

const COPIED_RESET_MS = 2_500;

const ACTION_LABEL: Record<NodeStripAction, string> = {
  "update-arc": PLAIN_COPY.actions.updateArc,
  "restart-arc": PLAIN_COPY.actions.restartArc,
  "quit-arc": PLAIN_COPY.actions.quitArc,
  "start-node": PLAIN_COPY.actions.startNode,
  "start-again": PLAIN_COPY.actions.startAgain,
  "try-again": PLAIN_COPY.actions.tryAgain,
};

// Only the attention tone is red. Off and an update on offer are ordinary
// states and take the muted and info colours; connected is green.
const TONE_ICON: Record<NodeStripTone, typeof CheckCircle2> = {
  ok: CheckCircle2,
  busy: Loader2,
  attention: CircleAlert,
  info: Download,
  idle: Power,
};
const TONE_COLOR: Record<NodeStripTone, string> = {
  ok: "var(--success)",
  busy: "var(--text-muted)",
  attention: "var(--danger)",
  info: "var(--info)",
  idle: "var(--text-muted)",
};

/**
 * Home's one-line node strip: the node's state in one sentence and its one
 * fix. It reads only things that answer while the node is down, blocked,
 * starting or updating (the start fence, the node status, the updater), so
 * the chat below stays usable and this line says what to do about the node.
 */
export function NodeStatusStrip() {
  const queryClient = useQueryClient();
  const config = useAppStore((s) => s.config);
  const update = useUpdaterSnapshot();
  const [startError, setStartError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [detailsOpen, setDetailsOpen] = useState(false);
  const [modelProgress, setModelProgress] = useState<ModelDownloadProgress | null>(null);
  const [copied, setCopied] = useState<"idle" | "copied" | "failed">("idle");
  const detailsId = useId();

  useEffect(() => {
    if (copied === "idle") return;
    const timer = window.setTimeout(() => setCopied("idle"), COPIED_RESET_MS);
    return () => window.clearTimeout(timer);
  }, [copied]);

  const { data: status } = useQuery({
    queryKey: ["status"],
    queryFn: api.nodeStatus,
    refetchInterval: 2000,
  });
  const { data: startBlock } = useQuery({
    queryKey: ["start-block"],
    queryFn: api.nodeStartBlock,
    refetchInterval: 5000,
  });

  // A consented install can still be downloading its model after setup.
  useEffect(() => {
    if (!isTauri) return;
    let unlisten: (() => void) | undefined;
    let active = true;
    (async () => {
      const { listen } = await import("@tauri-apps/api/event");
      const handle = await listen<ModelDownloadProgress>(
        "model-download-progress",
        (event) => setModelProgress(event.payload),
      );
      if (active) unlisten = handle;
      else handle();
    })();
    return () => {
      active = false;
      unlisten?.();
    };
  }, []);

  // How long the running node has had nothing answering. The strip re-renders
  // once a second while that lasts, so the sentence can change at the mark.
  const connecting =
    status?.running === true &&
    (status.health === "syncing" || status.health === "offline") &&
    !status.coordinatorUrl;
  const [connectingSince, setConnectingSince] = useState<number | null>(null);
  const [, setTick] = useState(0);
  useEffect(() => {
    if (!connecting) {
      setConnectingSince(null);
      return;
    }
    setConnectingSince((since) => since ?? Date.now());
    const timer = window.setInterval(() => setTick((n) => n + 1), 1000);
    return () => window.clearInterval(timer);
  }, [connecting]);
  const connectingSeconds =
    connecting && connectingSince !== null ? Math.floor((Date.now() - connectingSince) / 1000) : null;

  const view = describeNodeStrip({
    status,
    startBlock: startBlock ?? null,
    startError,
    update,
    modelProgress,
    connectingSeconds,
  });

  // The native text, so a person can paste it into a report as it is.
  const copyDetails = (details: string) => {
    // Without a clipboard API the call throws before returning a promise.
    void Promise.resolve()
      .then(() => navigator.clipboard.writeText(details))
      .then(
        () => setCopied("copied"),
        () => setCopied("failed"),
      );
  };

  const act = async (action: NodeStripAction) => {
    if (action === "update-arc") {
      void appUpdater.installAvailableUpdate();
      return;
    }
    if (action === "restart-arc") {
      // Relaunching runs launch reconciliation again, which a refusal needs.
      if (isTauri) void relaunch();
      else window.location.reload();
      return;
    }
    if (action === "quit-arc") {
      if (!isTauri) {
        window.location.reload();
        return;
      }
      // The tray's Quit: stop the node, then exit. While an update holds the
      // node, the stop is refused at once and ARC still exits.
      setBusy(true);
      await api.stopNode().catch(() => {});
      await exit(0).catch(() => {});
      setBusy(false);
      return;
    }
    setBusy(true);
    setStartError(null);
    try {
      if (action === "start-again") await api.clearCrash();
      await api.startNode(config ?? DEFAULT_NODE_CONFIG);
    } catch (error) {
      setStartError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
      void queryClient.invalidateQueries({ queryKey: ["status"] });
      void queryClient.invalidateQueries({ queryKey: ["start-block"] });
    }
  };

  const Icon = TONE_ICON[view.tone];
  return (
    <div
      className="node-strip"
      data-testid="node-strip"
      data-tone={view.tone}
      style={{
        display: "flex",
        flexWrap: "wrap",
        alignItems: "center",
        gap: "var(--space-2) var(--space-3)",
        padding: "var(--space-2) var(--space-4)",
        marginBottom: "var(--space-4)",
        border: "1px solid var(--border)",
        borderRadius: "var(--radius-md)",
        background: "var(--surface)",
        fontSize: "var(--text-sm)",
      }}
    >
      <Icon
        size={15}
        aria-hidden="true"
        className={view.tone === "busy" ? "spin" : undefined}
        style={{ flexShrink: 0, color: TONE_COLOR[view.tone] }}
      />
      <span role="status" style={{ flex: 1, minWidth: 0 }} data-testid="node-strip-sentence">
        {view.sentence}
      </span>
      {view.action && (
        <button
          type="button"
          className={view.tone === "attention" || view.tone === "info" ? "btn btn-primary btn-sm" : "btn btn-secondary btn-sm"}
          onClick={() => void act(view.action!)}
          disabled={busy}
          aria-busy={busy || undefined}
          data-testid="node-strip-action"
        >
          {ACTION_LABEL[view.action]}
        </button>
      )}
      {view.details && (
        <button
          type="button"
          className="btn btn-ghost btn-sm"
          onClick={() => setDetailsOpen((open) => !open)}
          aria-expanded={detailsOpen}
          aria-controls={detailsId}
          data-testid="node-strip-details-toggle"
        >
          {detailsOpen ? PLAIN_COPY.actions.hideDetails : PLAIN_COPY.actions.showDetails}
        </button>
      )}
      {view.details && (
        <button
          type="button"
          className="btn btn-ghost btn-sm"
          onClick={() => copyDetails(view.details!)}
          data-testid="node-strip-copy"
        >
          {copied === "copied" ? <Check size={12} /> : <Copy size={12} />}{" "}
          {copied === "copied"
            ? PLAIN_COPY.actions.copied
            : copied === "failed"
              ? PLAIN_COPY.actions.copyFailed
              : PLAIN_COPY.actions.copyDetails}
        </button>
      )}
      {/* The copy result, announced on its own. */}
      <span className="sr-only" aria-live="polite">
        {copied === "copied"
          ? PLAIN_COPY.actions.copied
          : copied === "failed"
            ? PLAIN_COPY.actions.copyFailed
            : ""}
      </span>
      {view.details && (
        <pre
          id={detailsId}
          hidden={!detailsOpen}
          data-testid="node-strip-details"
          style={{
            flexBasis: "100%",
            margin: 0,
            padding: "var(--space-2) var(--space-3)",
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
      )}
    </div>
  );
}
