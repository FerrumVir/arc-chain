import { useSyncExternalStore } from "react";
import type { QueryClient } from "@tanstack/react-query";
import { invoke, isBlockedProductionBrowser, onStartNodeFailure } from "./tauri";
import { appUpdater } from "./updater";
import {
  NodeCare,
  NodeCareWatcher,
  type NodeCarePlatform,
  type PayloadKind,
  type WatchedStatus,
} from "./node-care";

/** How often the queue is retried while ARC is open. */
const FLUSH_INTERVAL_MS = 10 * 60_000;

export const nodeCare = new NodeCare({
  loadState: () => invoke<string | null>("node_care_load_state"),
  saveState: (json: string) => invoke<void>("node_care_save_state", { json }),
  send: (kind: PayloadKind, body: string) => invoke<boolean>("node_care_send", { kind, body }),
  platform: () => invoke<NodeCarePlatform>("node_care_info"),
});

const watcher = new NodeCareWatcher(nodeCare);

/**
 * Watch what the app already polls (the `status` and `start-block` queries
 * and the updater) and report from it. Adds no polling of its own and
 * nothing on the start path waits for it. Returns a cleanup.
 */
export function startNodeCare(queryClient: QueryClient): () => void {
  if (isBlockedProductionBrowser) return () => {};
  void nodeCare.init();

  let lastStatus: unknown;
  let lastBlock: unknown;
  const observe = () => {
    const status = queryClient.getQueryData<WatchedStatus>(["status"]);
    const block = queryClient.getQueryData<string | null>(["start-block"]) ?? null;
    if (status === lastStatus && block === lastBlock) return;
    lastStatus = status;
    lastBlock = block;
    try {
      watcher.observeStatus(status, block);
    } catch {
      /* never let reporting break the query cache */
    }
  };
  const unsubscribeCache = queryClient.getQueryCache().subscribe((event) => {
    if (event.type !== "updated") return;
    const key = event.query.queryKey[0];
    if (key === "status" || key === "start-block") observe();
  });

  const unsubscribeUpdater = appUpdater.subscribe(() => {
    try {
      watcher.observeUpdate(appUpdater.getSnapshot());
    } catch {
      /* ignore */
    }
  });

  onStartNodeFailure((error, stage) => watcher.observeStartError(error, stage));

  const timer = setInterval(() => void nodeCare.flush(), FLUSH_INTERVAL_MS);

  return () => {
    unsubscribeCache();
    unsubscribeUpdater();
    onStartNodeFailure(null);
    clearInterval(timer);
  };
}

export function useNodeCareSnapshot() {
  return useSyncExternalStore(nodeCare.subscribe, nodeCare.getSnapshot, nodeCare.getSnapshot);
}
