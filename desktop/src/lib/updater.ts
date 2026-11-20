import { useSyncExternalStore } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Update as TauriUpdate,
  type DownloadEvent,
} from "@tauri-apps/plugin-updater";
import { relaunch as tauriRelaunch } from "@tauri-apps/plugin-process";
import { api, isTauri, listenForAppSignals } from "./tauri";
import {
  createUpdateController,
  type UpdateCandidate,
  type UpdateDownloadEvent,
} from "./update-controller";

const DOWNLOAD_TIMEOUT_MS = 10 * 60_000;

interface NativeUpdateMetadata {
  rid: number;
  currentVersion: string;
  version: string;
  date?: string;
  body?: string;
  rawJson: Record<string, unknown>;
}

async function checkArcUpdate(): Promise<TauriUpdate | null> {
  const metadata = await invoke<NativeUpdateMetadata | null>(
    "check_arc_update",
  );
  return metadata ? new TauriUpdate(metadata) : null;
}

export const appUpdater = createUpdateController({
  supported: isTauri,
  check: async () => {
    const policy = await api.updateInstallPolicy();
    const update = await checkArcUpdate();
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
  tryPrepareAutoUpdateRelaunch: api.tryPrepareAutoUpdateRelaunch,
  beginHandoff: api.beginUpdateHandoff,
  abortRelaunch: api.abortUpdateRelaunch,
  relaunch: tauriRelaunch,
});

if (isTauri) {
  void listenForAppSignals((signal) => appUpdater.signal(signal));
}

export function useUpdaterSnapshot() {
  return useSyncExternalStore(
    appUpdater.subscribe,
    appUpdater.getSnapshot,
    appUpdater.getSnapshot,
  );
}

export type { UpdateDownloadEvent };
