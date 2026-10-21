import { PLAIN_COPY } from "./plain-copy";
import { describeStartFailure, type StartFailureAction } from "./start-failure";
import { formatBytes } from "./format";
import type { ModelDownloadProgress, NodeStatus } from "./types";
import type { UpdateSnapshot } from "./update-controller";

export type NodeStripAction =
  | "update-arc"
  | "restart-arc"
  | "quit-arc"
  | "start-node"
  | "start-again"
  | "try-again";

export interface NodeStripView {
  sentence: string;
  /** At most one button. Null while something is already happening. */
  action: NodeStripAction | null;
  tone: "ok" | "busy" | "attention";
  /** The native text behind "Show details", when there is one. */
  details: string | null;
}

export interface NodeStripInput {
  /** `node_status`; undefined until its first answer. */
  status: Pick<NodeStatus, "running" | "pid" | "lastError"> | undefined;
  /** What a Start would be told by the start fence now; null when starts are open. */
  startBlock: string | null;
  /** The error from the last Start pressed on the strip. */
  startError: string | null;
  update: Pick<UpdateSnapshot, "phase" | "canInstall" | "message" | "error" | "restartRequired">;
  modelProgress: ModelDownloadProgress | null;
}

const FAILURE_ACTION: Record<StartFailureAction, NodeStripAction> = {
  "install-update": "update-arc",
  "restart-arc": "restart-arc",
  "try-again": "try-again",
};

/**
 * The node's state in one sentence and at most one button. Failures use the
 * start-failure rules, so a blocked node offers the update when one is ready
 * (it installs while the node is blocked) and a restart otherwise. With the
 * node healthy or off, a ready update is the one thing to show.
 */
export function describeNodeStrip(input: NodeStripInput): NodeStripView {
  const copy = PLAIN_COPY.nodeStrip;
  const { update } = input;
  const updateReady = update.phase === "available" && update.canInstall;
  // "ready" with an error is a relaunch that failed or an install ARC could
  // not confirm, and "error" with restartRequired an abort that could not
  // confirm the node resumed. Starts are refused until ARC is reopened.
  if (
    (update.phase === "ready" && update.error !== null) ||
    (update.phase === "error" && update.restartRequired)
  ) {
    return {
      sentence: copy.updateFailed,
      action: "quit-arc",
      tone: "attention",
      details: [update.message, update.error].filter(Boolean).join("\n"),
    };
  }
  if (update.phase === "downloading" || update.phase === "ready") {
    return { sentence: copy.updating, action: null, tone: "busy", details: null };
  }

  const crashed = input.status?.lastError?.includes("exited unexpectedly") === true;
  const failure =
    input.startBlock ??
    input.startError ??
    (input.status?.lastError && !crashed && !input.status.running
      ? input.status.lastError
      : null);
  if (failure) {
    const view = describeStartFailure(failure, { updateAvailable: updateReady });
    if (view.kind === "reconciling") {
      // It clears by itself; the strip polls, so there is nothing to press.
      return { sentence: view.title, action: null, tone: "busy", details: view.details };
    }
    return {
      sentence: view.title,
      action: FAILURE_ACTION[view.action],
      tone: "attention",
      details: view.details,
    };
  }

  if (updateReady) {
    return { sentence: copy.updateAvailable, action: "update-arc", tone: "attention", details: null };
  }
  if (input.status?.running) {
    return { sentence: copy.running, action: null, tone: "ok", details: null };
  }
  if (input.modelProgress && !input.modelProgress.done) {
    const { downloadedBytes, totalBytes } = input.modelProgress;
    const amount = `${formatBytes(downloadedBytes)} of ${formatBytes(totalBytes)}`;
    return { sentence: copy.downloadingModel(amount), action: null, tone: "busy", details: null };
  }
  if (input.status === undefined) {
    return { sentence: copy.checking, action: null, tone: "busy", details: null };
  }
  if (input.status.pid != null) {
    return { sentence: copy.starting, action: null, tone: "busy", details: null };
  }
  if (crashed) {
    return {
      sentence: copy.crashed,
      action: "start-again",
      tone: "attention",
      details: input.status.lastError,
    };
  }
  return { sentence: copy.off, action: "start-node", tone: "attention", details: null };
}
