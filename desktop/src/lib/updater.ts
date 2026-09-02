import { useSyncExternalStore } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Update as TauriUpdate,
  type DownloadEvent,
} from "@tauri-apps/plugin-updater";
import { relaunch as tauriRelaunch } from "@tauri-apps/plugin-process";
import { api, isTauri } from "./tauri";
import {
  createUpdateController,
  type UpdateCandidate,
  type UpdateDownloadEvent,
} from "./update-controller";

// Native release discovery and manifest checks are bounded to 30 seconds;
// payload downloads get a separate bounded window below.
const DOWNLOAD_TIMEOUT_MS = 10 * 60_000;

interface NativeUpdateMetadata {
  rid: number;
  currentVersion: string;
  version: string;
  date?: string;
  body?: string;
  rawJson: Record<string, unknown>;
}

/**
 * Resolve the ARC v0.8+ desktop channel natively. The Rust command selects an
 * immutable exact-tag GitHub release and creates Tauri's own Update resource;
 * download/install below therefore retains Tauri's embedded-key signature
 * verification and never depends on GitHub's legacy global `latest` pointer.
 */
async function checkArcUpdate(): Promise<TauriUpdate | null> {
  const metadata = await invoke<NativeUpdateMetadata | null>(
    "check_arc_update",
  );
  return metadata ? new TauriUpdate(metadata) : null;
}

export const appUpdater = createUpdateController({
  supported: isTauri,
  check: async () => {
    // Resolve the local install policy first. If that native command fails,
    // no Tauri Update resource has been allocated and therefore no orphaned
    // resource ID can survive a rejected parallel branch.
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
  beginHandoff: api.beginUpdateHandoff,
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
