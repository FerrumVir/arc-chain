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

export type NodeStripTone = "ok" | "busy" | "attention" | "info" | "idle";

export interface NodeStripView {
  sentence: string;
  action: NodeStripAction | null;
  tone: NodeStripTone;
  details: string | null;
}

export interface NodeStripInput {
  status:
    | (Pick<NodeStatus, "running" | "pid" | "lastError"> &
        Partial<Pick<NodeStatus, "health" | "coordinatorUrl" | "crashRetryInSecs" | "modelDownloadWait">>)
    | undefined;
  startBlock: string | null;
  startError: string | null;
  update: Pick<UpdateSnapshot, "phase" | "canInstall" | "message" | "error" | "restartRequired">;
  modelProgress: ModelDownloadProgress | null;
  connectingSeconds?: number | null;
  sharing?: boolean;
  sharingEligible?: boolean;
  sharingPending?: boolean;
  worker?: WorkerStatus;
  network?: WorkerNetwork | null;
}

export const MODEL_PROGRESS_STALE_MS = 150_000;

function downloadingModelView(progress: ModelDownloadProgress): NodeStripView {
  const amount = `${formatBytes(progress.downloadedBytes)} of ${formatBytes(progress.totalBytes)}`;
  return {
    sentence: PLAIN_COPY.nodeStrip.downloadingModel(amount),
    action: null,
    tone: "busy",
    details: null,
  };
}

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

export function isUnfinishedUpdate(
  update: Pick<UpdateSnapshot, "phase" | "error" | "restartRequired">,
): boolean {
  return (
    (update.phase === "ready" && update.error !== null) ||
    (update.phase === "error" && update.restartRequired)
  );
}

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
  if (input.status && !input.status.running && input.status.pid == null
      && input.status.crashRetryInSecs != null) {
    return {
      sentence: copy.crashRetry(Math.max(1, Math.ceil(input.status.crashRetryInSecs / 60))),
      action: "start-again", tone: "attention", details: input.status.lastError,
    };
  }
  if (input.status?.running) {
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

export function onlineComputerCount(network: WorkerNetwork | null | undefined): number | null {
  return network && Number.isSafeInteger(network.count_total) && network.count_total >= 0
    && network.count_total === network.count_visible ? network.count_total : null;
}

function networkSentence(network: WorkerNetwork | null | undefined): string {
  if (!network || !Number.isSafeInteger(network.count_total) || network.count_total <= 0) return "";
  const online = onlineComputerCount(network);
  return " " + (online !== null ? PLAIN_COPY.nodeStrip.online(online) : PLAIN_COPY.nodeStrip.registered(network.count_total));
}
