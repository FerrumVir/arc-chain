// React binding for the live network poller. One poller serves every panel
// on screen, so mounting the panel twice never doubles the reads, and it
// keeps its window across screen changes (it only pauses).

import { useEffect, useSyncExternalStore } from "react";
import { api } from "../tauri";
import type { NetworkStatsV1 } from "./contract";
import { createNetworkStatsPoller, type NetworkStatsPoller } from "./poller";

let shared: NetworkStatsPoller | null = null;

function sharedPoller(): NetworkStatsPoller {
  if (shared === null) {
    shared = createNetworkStatsPoller({ read: (request) => api.networkLiveRead(request) });
  }
  return shared;
}

/** The latest `arc.network-stats.v1` document. Polls only while a caller is mounted and the window is visible. */
export function useNetworkStats(): NetworkStatsV1 {
  const poller = sharedPoller();
  useEffect(() => {
    const update = () => poller.setVisible(document.visibilityState !== "hidden");
    update();
    document.addEventListener("visibilitychange", update);
    return () => document.removeEventListener("visibilitychange", update);
  }, [poller]);
  return useSyncExternalStore(poller.subscribe, poller.getSnapshot, poller.getSnapshot);
}
