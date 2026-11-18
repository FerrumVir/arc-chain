import { PLAIN_COPY } from "./plain-copy";
import { describeStartFailure, type StartFailureAction } from "./start-failure";
import { CONNECTING_LONG_SECS, isUnreachableMessage } from "./connection";
import { formatBytes, modelDownloadStatus } from "./format";
import type { ModelDownloadProgress, ModelDownloadWait, NodeStatus, WorkerStatus, WorkerNetwork } from "./types";
import type { UpdateSnapshot } from "./update-controller";

export type NodeStripAction =
  | "close-copy"
  | "update-arc"
  | "restart-arc"
  | "quit-arc"
  | "start-node"
  | "start-again"
  | "try-again"
  | "share-computer";

/**
 * ok: running and connected. busy: something is happening by itself.
 * attention: a problem with a fix. info: nothing wrong, something on offer
 * (an update). idle: the node is off by choice, nothing is wrong.
 */
export type NodeStripTone = "ok" | "busy" | "attention" | "info" | "idle";

export interface NodeStripView {
  sentence: string;
  /** At most one button. Null while something is already happening. */
  action: NodeStripAction | null;
  tone: NodeStripTone;
  /** The native text behind "Show details", when there is one. */
  details: string | null;
}

export interface NodeStripInput {
  /** `node_status`; undefined until its first answer. */
  status:
    | (Pick<NodeStatus, "running" | "pid" | "lastError"> &
        Partial<Pick<NodeStatus, "health" | "coordinatorUrl" | "crashRetryInSecs" | "modelDownloadWait">>)
    | undefined;
  /** What a Start would be told by the start fence now; null when starts are open. */
  startBlock: string | null;
  /** The error from the last Start pressed on the strip. */
  startError: string | null;
  update: Pick<UpdateSnapshot, "phase" | "canInstall" | "message" | "error" | "restartRequired">;
  modelProgress: ModelDownloadProgress | null;
  /** How long the running node has had nothing answering, in seconds; null or absent when not connecting. */
  connectingSeconds?: number | null;
  sharing?: boolean;
  /** Same recommended-tier check as Settings; undefined until confirmed. */
  sharingEligible?: boolean;
  sharingPending?: boolean;
  worker?: WorkerStatus;
  network?: WorkerNetwork | null;
}

/**
 * How long a model download may go without reporting before Home stops
 * saying it is downloading. A download that fails ends without a final event,
 * and a computer that slept through one resumes later, so the line must not
 * outlive the reports. The quietest gaps inside a live download are an idle
 * connection (60 s before it is dropped) and the wait before a retry (up to
 * 60 s), so this is well past both; the next report brings the line back.
 */
export const MODEL_PROGRESS_STALE_MS = 150_000;

/** The model is still on its way: the sentence for it, with no button. */
function downloadingModelView(progress: ModelDownloadProgress): NodeStripView {
  const amount = `${formatBytes(progress.downloadedBytes)} of ${formatBytes(progress.totalBytes)}`;
  return {
    sentence: PLAIN_COPY.nodeStrip.downloadingModel(amount),
    action: null,
    tone: "busy",
    details: null,
  };
}

/** Why the consented model download is waiting, on a running node; no button, it retries by itself. */
function modelWaitView(waiting: ModelDownloadWait): NodeStripView {
  const copy = PLAIN_COPY.nodeStrip;
  const sentence = waiting.reason === "onBattery" ? copy.modelWaitingBattery
    : waiting.reason === "diskFull" ? copy.modelWaitingSpace(formatBytes(waiting.neededBytes))
    : waiting.reason === "meteredNetwork" ? copy.modelWaitingMetered
    : copy.modelWaitingFailed;
  return { sentence, action: null, tone: "idle", details: null };
}

const FAILURE_ACTION: Record<StartFailureAction, NodeStripAction> = {
  "close-copy": "close-copy",
  "install-update": "update-arc",
  "restart-arc": "restart-arc",
  "quit-arc": "quit-arc",
  "try-again": "try-again",
};

/**
 * An update ARC couldn't finish. "ready" with an error is a relaunch that
 * failed or an install ARC could not confirm, and "error" with
 * restartRequired an abort that could not confirm the node resumed. The
 * update keeps the node until ARC exits, so Start and Restart are refused and
 * only quitting and reopening ARC clears it. Home's strip and the Dashboard
 * both offer that one step.
 */
export function isUnfinishedUpdate(
  update: Pick<UpdateSnapshot, "phase" | "error" | "restartRequired">,
): boolean {
  return (
    (update.phase === "ready" && update.error !== null) ||
    (update.phase === "error" && update.restartRequired)
  );
}

/**
 * The node's state in one sentence and at most one button. Failures use the
 * start-failure rules, so a blocked node offers the update when one is ready
 * (it installs while the node is blocked) and a restart otherwise. With the
 * node healthy or off, a ready update is the one thing to show.
 *
 * Only a real problem gets the attention tone. A node that is off, an update
 * on offer and a node still connecting are ordinary states, and read as such.
 */
export function describeNodeStrip(input: NodeStripInput): NodeStripView {
  const copy = PLAIN_COPY.nodeStrip;
  const { update } = input;
  const updateReady = update.phase === "available" && update.canInstall;
  const ownershipFailure = input.startBlock ?? input.status?.lastError;
  if (ownershipFailure) {
    const view = describeStartFailure(ownershipFailure, { updateAvailable: false });
    if (view.kind === "other-desktop" || view.kind === "closing") {
      return { sentence: view.title, action: view.kind === "closing" ? null : "close-copy", tone: "attention", details: view.details };
    }
  }
  // Starts are refused until ARC is reopened.
  if (isUnfinishedUpdate(update)) {
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
  // The status's "nothing answered" text describes this computer's connection,
  // not a start that failed; it is read below with the node's off state.
  const unreachable = isUnreachableMessage(input.status?.lastError);
  const failure =
    input.startBlock ??
    input.startError ??
    (input.status?.lastError && !crashed && !unreachable && !input.status.running
      ? input.status.lastError
      : null);
  if (failure) {
    const view = describeStartFailure(failure, { updateAvailable: updateReady });
    if (view.kind === "reconciling" || view.kind === "closing") {
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
    return { sentence: copy.updateAvailable, action: "update-arc", tone: "info", details: null };
  }
  // Clearing the crash details does not cancel a native scheduled retry.
  if (input.status && !input.status.running && input.status.pid == null
      && input.status.crashRetryInSecs != null) {
    return {
      sentence: copy.crashRetry(Math.max(1, Math.ceil(input.status.crashRetryInSecs / 60))),
      action: "start-again", tone: "attention", details: input.status.lastError,
    };
  }
  if (input.status?.running) {
    // A consented install downloads its model after the node has started
    // (after setup, or after "Turn on"), so the node is running while it
    // does. It is the one thing worth saying: jobs wait for the model.
    if (input.sharingPending) {
      return {
        sentence: input.modelProgress ? modelDownloadStatus(input.modelProgress) : copy.enablingSharing,
        action: null, tone: "busy", details: null,
      };
    }
    if (input.modelProgress && !input.modelProgress.done) {
      return downloadingModelView(input.modelProgress);
    }
    const { health, coordinatorUrl } = input.status;
    const connected = health === "live" || health === "lite" || Boolean(coordinatorUrl);
    // A model wait replaces only the "all is well" sentences: a node that is
    // still connecting, or can't reach the network, says that first. A node
    // that is off or starting never reaches here.
    const waiting = input.status.modelDownloadWait;
    if (waiting && (connected || (health !== "syncing" && health !== "offline"))) {
      return modelWaitView(waiting);
    }
    if (connected) {
      if (input.sharing !== undefined && input.sharingEligible !== true) {
        return {
          sentence: (input.sharingEligible === false ? copy.sharingIneligible : copy.runningConnected)
            + networkSentence(input.network),
          action: null, tone: input.sharingEligible === false ? "idle" : "ok", details: null,
        };
      }
      if (input.sharing === undefined) {
        return { sentence: copy.runningConnected, action: null, tone: "ok", details: null };
      }
      const worker = input.worker;
      const sentence = !input.sharing ? copy.sharingOff
        : worker?.running && worker.state === "computing" ? copy.computing
        : worker?.running && worker.state === "reconnecting" ? copy.reconnectingWorker
        : worker?.running && worker.state === "polling"
          ? worker.jobsCompleted != null && worker.jobsCompleted > 0
            ? copy.answered(worker.jobsCompleted) : copy.ready
          : copy.preparing;
      return {
        sentence: sentence + networkSentence(input.network),
        action: input.sharing ? null : "share-computer",
        tone: input.sharing && (!worker?.running || worker.state !== "polling") ? "busy" : "ok",
        details: null,
      };
    }
    if (health === "syncing" || health === "offline") {
      // After the same wait the Dashboard uses, say so plainly; still no red and nothing to press.
      if ((input.connectingSeconds ?? 0) >= CONNECTING_LONG_SECS) {
        return { sentence: copy.runningUnreachable, action: null, tone: "idle", details: null };
      }
      return { sentence: copy.runningConnecting, action: null, tone: "busy", details: null };
    }
    return { sentence: copy.running, action: null, tone: "ok", details: null };
  }
  if (input.sharingPending) {
    return {
      sentence: input.modelProgress ? modelDownloadStatus(input.modelProgress) : copy.enablingSharing,
      action: null, tone: "busy", details: null,
    };
  }
  if (input.modelProgress && !input.modelProgress.done) {
    return downloadingModelView(input.modelProgress);
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
  if (unreachable) {
    return { sentence: copy.offUnreachable, action: "start-node", tone: "idle", details: null };
  }
  return { sentence: copy.off, action: "start-node", tone: "idle", details: null };
}

/** The registry includes stale workers; only equality proves every entry is fresh. */
export function onlineComputerCount(network: WorkerNetwork | null | undefined): number | null {
  return network && Number.isSafeInteger(network.count_total) && network.count_total >= 0
    && network.count_total === network.count_visible ? network.count_total : null;
}

function networkSentence(network: WorkerNetwork | null | undefined): string {
  if (!network || !Number.isSafeInteger(network.count_total) || network.count_total <= 0) return "";
  const online = onlineComputerCount(network);
  return " " + (online !== null ? PLAIN_COPY.nodeStrip.online(online) : PLAIN_COPY.nodeStrip.registered(network.count_total));
}
