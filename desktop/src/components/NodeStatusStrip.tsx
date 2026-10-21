import { useEffect, useId, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { CheckCircle2, CircleAlert, Loader2 } from "lucide-react";
import { relaunch } from "@tauri-apps/plugin-process";
import { api, isTauri } from "../lib/tauri";
import { useAppStore } from "../lib/store";
import { appUpdater, useUpdaterSnapshot } from "../lib/updater";
import { describeNodeStrip, type NodeStripAction } from "../lib/node-strip";
import { PLAIN_COPY } from "../lib/plain-copy";
import { DEFAULT_NODE_CONFIG, type ModelDownloadProgress } from "../lib/types";

const ACTION_LABEL: Record<NodeStripAction, string> = {
  "update-arc": PLAIN_COPY.actions.updateArc,
  "restart-arc": PLAIN_COPY.actions.restartArc,
  "start-node": PLAIN_COPY.actions.startNode,
  "start-again": PLAIN_COPY.actions.startAgain,
  "try-again": PLAIN_COPY.actions.tryAgain,
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
  const detailsId = useId();

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

  const view = describeNodeStrip({
    status,
    startBlock: startBlock ?? null,
    startError,
    update,
    modelProgress,
  });

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

  const Icon =
    view.tone === "ok" ? CheckCircle2 : view.tone === "busy" ? Loader2 : CircleAlert;
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
        style={{
          flexShrink: 0,
          color: view.tone === "attention" ? "var(--danger)" : "var(--text-muted)",
        }}
      />
      <span role="status" style={{ flex: 1, minWidth: 0 }} data-testid="node-strip-sentence">
        {view.sentence}
      </span>
      {view.action && (
        <button
          type="button"
          className="btn btn-primary btn-sm"
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
