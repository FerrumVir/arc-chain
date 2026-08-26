import { useSyncExternalStore } from "react";
import {
  check as tauriCheckUpdate,
  type DownloadEvent,
} from "@tauri-apps/plugin-updater";
import { relaunch as tauriRelaunch } from "@tauri-apps/plugin-process";
import { isTauri } from "./tauri";
import {
  createUpdateController,
  type UpdateCandidate,
  type UpdateDownloadEvent,
} from "./update-controller";

// Network operations are bounded without changing the configured endpoint,
// public key, TLS policy, or downgrade policy used by Tauri's updater.
const CHECK_TIMEOUT_MS = 30_000;
const DOWNLOAD_TIMEOUT_MS = 10 * 60_000;

export const appUpdater = createUpdateController({
  supported: isTauri,
  check: async () => {
    const update = await tauriCheckUpdate({ timeout: CHECK_TIMEOUT_MS });
    if (!update) return null;

    const candidate: UpdateCandidate = {
      version: update.version,
      downloadAndInstall: (onEvent) =>
        update.downloadAndInstall(
          onEvent as ((event: DownloadEvent) => void) | undefined,
          { timeout: DOWNLOAD_TIMEOUT_MS },
        ),
      close: () => update.close(),
    };
    return candidate;
  },
  relaunch: tauriRelaunch,
});

export function useUpdaterSnapshot() {
  return useSyncExternalStore(
    appUpdater.subscribe,
    appUpdater.getSnapshot,
    appUpdater.getSnapshot,
  );
}

export type { UpdateDownloadEvent };
