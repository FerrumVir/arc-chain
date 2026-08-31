import { useSyncExternalStore } from "react";
import {
  check as tauriCheckUpdate,
  type DownloadEvent,
} from "@tauri-apps/plugin-updater";
import { relaunch as tauriRelaunch } from "@tauri-apps/plugin-process";
import { api, isTauri } from "./tauri";
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
    const [update, policy] = await Promise.all([
      tauriCheckUpdate({ timeout: CHECK_TIMEOUT_MS }),
      api.updateInstallPolicy(),
    ]);
    if (!update) return null;

    const candidate: UpdateCandidate = {
      version: update.version,
      canInstall: policy.canInstall,
      installInstructions: policy.instructions,
      download: (onEvent) =>
        update.download(
          onEvent as ((event: DownloadEvent) => void) | undefined,
          { timeout: DOWNLOAD_TIMEOUT_MS },
        ),
      install: () => update.install(),
      close: () => update.close(),
    };
    return candidate;
  },
  prepareRelaunch: api.prepareUpdateRelaunch,
  abortRelaunch: api.abortUpdateRelaunch,
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
