export const UPDATE_STARTUP_DELAY_MS = 5_000;
export const UPDATE_CHECK_INTERVAL_MS = 24 * 60 * 60 * 1_000;

export type UpdatePhase =
  | "idle"
  | "checking"
  | "up-to-date"
  | "available"
  | "downloading"
  | "ready"
  | "error"
  | "unsupported";

export type UpdateDownloadEvent =
  | { event: "Started"; data: { contentLength?: number } }
  | { event: "Progress"; data: { chunkLength: number } }
  | { event: "Finished" };

/**
 * The small surface we use from Tauri's signed updater resource.
 * Keeping it structural makes the scheduler deterministic to unit-test
 * without replacing or weakening the native updater implementation.
 */
export interface UpdateCandidate {
  version: string;
  downloadAndInstall: (
    onEvent?: (event: UpdateDownloadEvent) => void,
  ) => Promise<void>;
  close?: () => Promise<void>;
}

export interface UpdateSnapshot {
  phase: UpdatePhase;
  version: string | null;
  message: string;
  error: string | null;
  checkedAt: number | null;
  downloadedBytes: number;
  contentLength: number | null;
  canInstall: boolean;
  restartRequired: boolean;
}

export interface UpdateRuntime {
  supported: boolean;
  check: () => Promise<UpdateCandidate | null>;
  relaunch: () => Promise<void>;
  now?: () => number;
}

export interface UpdateSchedule {
  startupDelayMs?: number;
  intervalMs?: number;
}

/**
 * Injectable timer surface. Production uses the browser timers below; tests
 * provide a deterministic clock so cadence and cancellation do not depend on
 * wall-clock sleeps.
 */
export interface UpdateTimers {
  setTimeout: (callback: () => void, delayMs: number) => unknown;
  clearTimeout: (handle: unknown) => void;
  setInterval: (callback: () => void, delayMs: number) => unknown;
  clearInterval: (handle: unknown) => void;
}

const BROWSER_TIMERS: UpdateTimers = {
  setTimeout: (callback, delayMs) =>
    globalThis.setTimeout(callback, delayMs),
  clearTimeout: (handle) =>
    globalThis.clearTimeout(
      handle as ReturnType<typeof globalThis.setTimeout>,
    ),
  setInterval: (callback, delayMs) =>
    globalThis.setInterval(callback, delayMs),
  clearInterval: (handle) =>
    globalThis.clearInterval(
      handle as ReturnType<typeof globalThis.setInterval>,
    ),
};

const IDLE_SNAPSHOT: UpdateSnapshot = {
  phase: "idle",
  version: null,
  message: "No update check has run yet.",
  error: null,
  checkedAt: null,
  downloadedBytes: 0,
  contentLength: null,
  canInstall: false,
  restartRequired: false,
};

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Owns the updater lifecycle for the whole app.
 *
 * There is intentionally one controller rather than one query per screen:
 * startup, periodic, and manual checks all share the same in-flight promise,
 * so navigation and React renders cannot create overlapping network checks.
 */
export class UpdateController {
  private snapshot: UpdateSnapshot = IDLE_SNAPSHOT;
  private readonly listeners = new Set<() => void>();
  private candidate: UpdateCandidate | null = null;
  private checkInFlight: Promise<UpdateSnapshot> | null = null;
  private installInFlight: Promise<UpdateSnapshot> | null = null;
  private startupTimer: unknown | null = null;
  private intervalTimer: unknown | null = null;
  private autoChecksEnabled = false;
  private readonly startupDelayMs: number;
  private readonly intervalMs: number;

  constructor(
    private readonly runtime: UpdateRuntime,
    schedule: UpdateSchedule = {},
    private readonly timers: UpdateTimers = BROWSER_TIMERS,
  ) {
    this.startupDelayMs =
      schedule.startupDelayMs ?? UPDATE_STARTUP_DELAY_MS;
    this.intervalMs = schedule.intervalMs ?? UPDATE_CHECK_INTERVAL_MS;
  }

  getSnapshot = (): UpdateSnapshot => this.snapshot;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  /** Enable or cancel background checks. Safe to call repeatedly. */
  setAutoChecksEnabled(enabled: boolean): void {
    if (enabled === this.autoChecksEnabled) return;

    this.clearTimers();
    this.autoChecksEnabled = enabled;

    // Browser previews and tests without the native shell keep the manual
    // unsupported state, but must never schedule calls to a missing plugin.
    if (!enabled || !this.runtime.supported) return;

    this.startupTimer = this.timers.setTimeout(() => {
      this.startupTimer = null;
      void this.checkForUpdates("automatic");
    }, this.startupDelayMs);

    this.intervalTimer = this.timers.setInterval(() => {
      void this.checkForUpdates("automatic");
    }, this.intervalMs);
  }

  /**
   * Check the signed Tauri update manifest. Concurrent callers share one
   * promise; checks never run while an install is active.
   */
  checkForUpdates(
    _source: "automatic" | "manual" = "manual",
  ): Promise<UpdateSnapshot> {
    if (!this.runtime.supported) {
      this.setSnapshot({
        ...this.snapshot,
        phase: "unsupported",
        message: "Update checks are available in the installed ARC app.",
        error: null,
        canInstall: false,
        restartRequired: false,
      });
      return Promise.resolve(this.snapshot);
    }

    // Once installation succeeded, this running process is deliberately
    // frozen on the ready state until it exits. Re-checking from the old
    // binary could rediscover and reinstall the same release after a failed
    // relaunch.
    if (this.snapshot.restartRequired) return Promise.resolve(this.snapshot);

    if (this.installInFlight) return this.installInFlight;
    if (this.checkInFlight) return this.checkInFlight;

    this.setSnapshot({
      ...this.snapshot,
      phase: "checking",
      message: "Checking the signed release manifest…",
      error: null,
      canInstall: false,
      restartRequired: false,
    });

    const check = (async () => {
      try {
        const next = await this.runtime.check();
        const previous = this.candidate;
        this.candidate = next;
        if (previous && previous !== next) void previous.close?.().catch(() => {});

        const checkedAt = (this.runtime.now ?? Date.now)();
        if (next) {
          this.setSnapshot({
            phase: "available",
            version: next.version,
            message: `Version ${next.version} is available.`,
            error: null,
            checkedAt,
            downloadedBytes: 0,
            contentLength: null,
            canInstall: true,
            restartRequired: false,
          });
        } else {
          this.setSnapshot({
            phase: "up-to-date",
            version: null,
            message: "You're running the latest version.",
            error: null,
            checkedAt,
            downloadedBytes: 0,
            contentLength: null,
            canInstall: false,
            restartRequired: false,
          });
        }
      } catch (error) {
        const detail = errorMessage(error);
        this.setSnapshot({
          ...this.snapshot,
          phase: "error",
          message: `Update check failed: ${detail}`,
          error: detail,
          checkedAt: (this.runtime.now ?? Date.now)(),
          canInstall: this.candidate !== null,
          restartRequired: false,
        });
      } finally {
        this.checkInFlight = null;
      }
      return this.snapshot;
    })();

    this.checkInFlight = check;
    return check;
  }

  /**
   * Download and install only after an explicit user action. A successful
   * install is marked ready before requesting an immediate app relaunch.
   */
  installAvailableUpdate(): Promise<UpdateSnapshot> {
    if (this.installInFlight) return this.installInFlight;

    // The UI disables Install during a check, but retain the invariant at the
    // controller boundary too. A stale/programmatic click must not race a
    // candidate replacement; it simply observes the check result and the user
    // can confirm the newly reported version.
    if (this.checkInFlight) return this.checkInFlight;

    if (!this.candidate) {
      this.setSnapshot({
        ...this.snapshot,
        phase: "error",
        message: "No checked update is ready to install.",
        error: "Check for updates before installing.",
        canInstall: false,
        restartRequired: false,
      });
      return Promise.resolve(this.snapshot);
    }

    const candidate = this.candidate;
    const version = candidate.version;
    this.setSnapshot({
      ...this.snapshot,
      phase: "downloading",
      version,
      message: `Downloading signed update ${version}…`,
      error: null,
      downloadedBytes: 0,
      contentLength: null,
      canInstall: false,
      restartRequired: false,
    });

    const install = (async () => {
      try {
        await candidate.downloadAndInstall((event) => {
          if (event.event === "Started") {
            this.setSnapshot({
              ...this.snapshot,
              phase: "downloading",
              message: `Downloading signed update ${version}…`,
              downloadedBytes: 0,
              contentLength: event.data.contentLength ?? null,
            });
          } else if (event.event === "Progress") {
            const downloadedBytes =
              this.snapshot.downloadedBytes + event.data.chunkLength;
            this.setSnapshot({
              ...this.snapshot,
              phase: "downloading",
              message: `Downloading signed update ${version}…`,
              downloadedBytes,
            });
          } else {
            this.setSnapshot({
              ...this.snapshot,
              phase: "downloading",
              message: `Download complete. Installing verified update ${version}…`,
            });
          }
        });

        this.candidate = null;
        void candidate.close?.().catch(() => {});
        this.setSnapshot({
          ...this.snapshot,
          phase: "ready",
          version,
          message: `Version ${version} installed. Restarting ARC now…`,
          error: null,
          canInstall: false,
          restartRequired: true,
        });

        try {
          await this.runtime.relaunch();
        } catch (error) {
          const detail = errorMessage(error);
          this.setSnapshot({
            ...this.snapshot,
            phase: "ready",
            message: `Version ${version} is installed, but ARC could not relaunch. Close and reopen the app to finish updating.`,
            error: detail,
            canInstall: false,
            restartRequired: true,
          });
        }
      } catch (error) {
        const detail = errorMessage(error);
        this.setSnapshot({
          ...this.snapshot,
          phase: "error",
          version,
          message: `Update was not installed: ${detail}`,
          error: detail,
          canInstall: true,
          restartRequired: false,
        });
      } finally {
        this.installInFlight = null;
      }
      return this.snapshot;
    })();

    this.installInFlight = install;
    return install;
  }

  /** Test/app teardown: stop timers and release any native update handle. */
  dispose(): void {
    this.clearTimers();
    this.autoChecksEnabled = false;
    const candidate = this.candidate;
    this.candidate = null;
    if (candidate) void candidate.close?.().catch(() => {});
    this.listeners.clear();
  }

  private clearTimers(): void {
    if (this.startupTimer !== null) {
      this.timers.clearTimeout(this.startupTimer);
      this.startupTimer = null;
    }
    if (this.intervalTimer !== null) {
      this.timers.clearInterval(this.intervalTimer);
      this.intervalTimer = null;
    }
  }

  private setSnapshot(next: UpdateSnapshot): void {
    this.snapshot = next;
    for (const listener of this.listeners) listener();
  }
}

export function createUpdateController(
  runtime: UpdateRuntime,
  schedule?: UpdateSchedule,
  timers?: UpdateTimers,
): UpdateController {
  return new UpdateController(runtime, schedule, timers);
}
