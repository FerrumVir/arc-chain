import type { AutoUpdatePrepareResult } from "./types";
import { PLAIN_COPY } from "./plain-copy";
import { isStartupReconciliationInProgress } from "./start-failure";

export const UPDATE_STARTUP_DELAY_MS = 5_000;
export const UPDATE_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1_000;
export const UPDATE_RETRY_DELAYS_MS = [60_000, 5 * 60_000, 15 * 60_000] as const;
export const UPDATE_BUSY_RETRY_MS = 60 * 60 * 1_000;
export const UPDATE_SIGNAL_MIN_GAP_MS = 15 * 60_000;

export type UpdateSignal = "tick" | "wake" | "focus" | "shown";
export const UPDATE_RECONCILIATION_RETRY_MS = 60_000;

export const NATIVE_WRONG_LOCATION =
  "ARC can't update itself from where it's running. Quit ARC, drag ARC Node into your Applications folder, then open it from there.";

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

export interface UpdateCandidate {
  version: string;
  canInstall?: boolean;
  installInstructions?: string;
  download: (
    onEvent?: (event: UpdateDownloadEvent) => void,
  ) => Promise<void>;
  install: () => Promise<void>;
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
  prepareRelaunch: () => Promise<void>;
  beginHandoff: () => Promise<void>;
  abortRelaunch: () => Promise<void>;
  tryPrepareAutoUpdateRelaunch?: () => Promise<AutoUpdatePrepareResult>;
  relaunch: () => Promise<void>;
  now?: () => number;
}

export interface UpdateSchedule {
  startupDelayMs?: number;
  intervalMs?: number;
  retryDelaysMs?: readonly number[];
  busyRetryMs?: number;
  signalMinGapMs?: number;
}

type UpdateCheckSource = "automatic" | "manual" | "retry";

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

export class UpdateController {
  private snapshot: UpdateSnapshot = IDLE_SNAPSHOT;
  private readonly listeners = new Set<() => void>();
  private candidate: UpdateCandidate | null = null;
  private downloadedCandidate: UpdateCandidate | null = null;
  private checkInFlight: Promise<UpdateSnapshot> | null = null;
  private automaticCheckRequestedInFlight = false;
  private installInFlight: Promise<UpdateSnapshot> | null = null;
  private startupTimer: unknown | null = null;
  private intervalTimer: unknown | null = null;
  private retryTimer: unknown | null = null;
  private autoInstallRetryTimer: unknown | null = null;
  private intervalDueAt: number | null = null;
  private retryDueAt: number | null = null;
  private autoInstallRetryDueAt: number | null = null;
  private lastCheckStartedAt: number | null = null;
  private reconciliationRetryTimer: unknown | null = null;
  private reconciliationWaitManual = false;
  private automaticFailureCount = 0;
  private autoInstallDeferralCount = 0;
  private autoChecksEnabled = false;
  private autoInstallEnabled = false;
  private autoInstallGeneration = 0;
  private readonly startupDelayMs: number;
  private readonly intervalMs: number;
  private readonly retryDelaysMs: readonly number[];
  private readonly busyRetryMs: number;
  private readonly signalMinGapMs: number;

  constructor(
    private readonly runtime: UpdateRuntime,
    schedule: UpdateSchedule = {},
    private readonly timers: UpdateTimers = BROWSER_TIMERS,
  ) {
    this.startupDelayMs =
      schedule.startupDelayMs ?? UPDATE_STARTUP_DELAY_MS;
    this.intervalMs = schedule.intervalMs ?? UPDATE_CHECK_INTERVAL_MS;
    this.retryDelaysMs = schedule.retryDelaysMs ?? UPDATE_RETRY_DELAYS_MS;
    this.busyRetryMs = schedule.busyRetryMs ?? UPDATE_BUSY_RETRY_MS;
    this.signalMinGapMs = schedule.signalMinGapMs ?? UPDATE_SIGNAL_MIN_GAP_MS;
  }

  private now(): number {
    return (this.runtime.now ?? Date.now)();
  }

  getSnapshot = (): UpdateSnapshot => this.snapshot;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  setAutoChecksEnabled(enabled: boolean): void {
    if (!enabled) this.setAutoInstallEnabled(false);
    if (enabled === this.autoChecksEnabled) return;

    this.clearTimers();
    this.autoChecksEnabled = enabled;
    this.automaticFailureCount = 0;

    if (!enabled || !this.runtime.supported) return;

    this.startupTimer = this.timers.setTimeout(() => {
      this.startupTimer = null;
      void this.checkForUpdates("automatic");
    }, this.startupDelayMs);

    this.scheduleIntervalCheck();
  }

  private scheduleIntervalCheck(): void {
    this.intervalDueAt = this.now() + this.intervalMs;
    this.intervalTimer = this.timers.setTimeout(() => {
      this.intervalTimer = null;
      this.runIntervalCheck();
    }, this.intervalMs);
  }

  private runIntervalCheck(): void {
    this.scheduleIntervalCheck();
    if (this.retryTimer !== null) return;
    this.automaticFailureCount = 0;
    void this.checkForUpdates("automatic");
  }

  signal(signal: UpdateSignal): void {
    if (!this.autoChecksEnabled || !this.runtime.supported) return;
    if (this.snapshot.restartRequired) return;
    const now = this.now();
    this.runOverdue(now);
    if (signal === "tick" || this.candidate !== null) return;
    if (
      this.lastCheckStartedAt !== null &&
      now - this.lastCheckStartedAt < this.signalMinGapMs
    ) {
      return;
    }
    void this.checkForUpdates("automatic");
  }

  private runOverdue(now: number): void {
    if (
      this.autoInstallRetryTimer !== null &&
      this.autoInstallRetryDueAt !== null &&
      this.autoInstallRetryDueAt <= now
    ) {
      const fire = this.autoInstallRetryFire;
      this.clearAutoInstallRetryTimer();
      fire?.();
    }
    if (
      this.retryTimer !== null &&
      this.retryDueAt !== null &&
      this.retryDueAt <= now
    ) {
      this.clearRetryTimer();
      void this.checkForUpdates("retry");
    }
    if (
      this.intervalTimer !== null &&
      this.intervalDueAt !== null &&
      this.intervalDueAt <= now
    ) {
      this.timers.clearTimeout(this.intervalTimer);
      this.intervalTimer = null;
      this.runIntervalCheck();
    }
  }

  setAutoInstallEnabled(enabled: boolean): void {
    const next = enabled && this.autoChecksEnabled && this.runtime.supported;
    if (next === this.autoInstallEnabled) return;

    this.autoInstallEnabled = next;
    this.autoInstallGeneration += 1;
    this.clearAutoInstallRetryTimer();
    this.autoInstallDeferralCount = 0;

    if (
      !next &&
      this.reconciliationRetryTimer !== null &&
      !this.reconciliationWaitManual
    ) {
      this.clearReconciliationRetryTimer();
      if (this.candidate && this.snapshot.phase === "available") {
        this.setSnapshot({
          ...this.snapshot,
          message: `Version ${this.candidate.version} is available.`,
        });
      }
    }

    if (
      next &&
      this.candidate &&
      this.candidate.canInstall !== false &&
      this.snapshot.phase === "available"
    ) {
      void this.installAutomatically(this.candidate);
    }
  }

  checkForUpdates(
    source: UpdateCheckSource = "manual",
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

    if (this.snapshot.restartRequired) return Promise.resolve(this.snapshot);

    if (this.installInFlight) return this.installInFlight;
    if (this.checkInFlight) {
      if (source !== "manual") this.automaticCheckRequestedInFlight = true;
      return this.checkInFlight;
    }

    this.automaticCheckRequestedInFlight = source !== "manual";
    this.lastCheckStartedAt = this.now();

    this.setSnapshot({
      ...this.snapshot,
      phase: "checking",
      message: "Checking the signed release manifest…",
      error: null,
      canInstall: false,
      restartRequired: false,
    });

    const check = (async () => {
      let autoInstallCandidate: UpdateCandidate | null = null;
      try {
        const next = await this.runtime.check();
        this.automaticFailureCount = 0;
        this.clearRetryTimer();
        const previous = this.candidate;
        this.candidate = next;
        if (previous && previous !== next) {
          this.clearAutoInstallRetryTimer();
          this.clearReconciliationRetryTimer();
          this.autoInstallDeferralCount = 0;
          if (this.downloadedCandidate === previous) {
            this.downloadedCandidate = null;
          }
          void previous.close?.().catch(() => {});
        }

        const checkedAt = (this.runtime.now ?? Date.now)();
        if (next) {
          const canInstall = next.canInstall !== false;
          this.setSnapshot({
            phase: "available",
            version: next.version,
            message: canInstall
              ? `Version ${next.version} is available.`
              : (next.installInstructions ??
                `Version ${next.version} is available through your package manager.`),
            error: null,
            checkedAt,
            downloadedBytes: 0,
            contentLength: null,
            canInstall,
            restartRequired: false,
          });
          if (
            source !== "manual" &&
            this.autoInstallEnabled &&
            canInstall
          ) {
            autoInstallCandidate = next;
          }
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
          canInstall:
            this.candidate !== null && this.candidate.canInstall !== false,
          restartRequired: false,
        });
        if (source !== "manual" || this.automaticCheckRequestedInFlight) {
          this.scheduleAutomaticRetry();
        }
      } finally {
        this.checkInFlight = null;
        this.automaticCheckRequestedInFlight = false;
      }
      if (
        autoInstallCandidate &&
        this.autoInstallEnabled &&
        this.candidate === autoInstallCandidate
      ) {
        void this.installAutomatically(autoInstallCandidate);
      }
      return this.snapshot;
    })();

    this.checkInFlight = check;
    return check;
  }

  installAvailableUpdate(): Promise<UpdateSnapshot> {
    return this.startInstall(null);
  }

  private installAutomatically(
    candidate: UpdateCandidate,
  ): Promise<UpdateSnapshot> {
    if (
      !this.autoInstallEnabled ||
      !this.autoChecksEnabled ||
      this.candidate !== candidate ||
      candidate.canInstall === false ||
      this.snapshot.restartRequired ||
      this.checkInFlight ||
      this.installInFlight
    ) {
      return Promise.resolve(this.snapshot);
    }
    return this.startInstall(this.autoInstallGeneration);
  }

  private startInstall(
    autoInstallGeneration: number | null,
  ): Promise<UpdateSnapshot> {
    const automatic = autoInstallGeneration !== null;
    if (
      automatic &&
      !this.isAutoInstallCurrent(autoInstallGeneration)
    ) {
      return Promise.resolve(this.snapshot);
    }

    if (this.installInFlight) return this.installInFlight;

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

    if (this.candidate.canInstall === false) {
      this.setSnapshot({
        ...this.snapshot,
        phase: "available",
        message:
          this.candidate.installInstructions ??
          `Version ${this.candidate.version} is available through your package manager.`,
        error: null,
        canInstall: false,
        restartRequired: false,
      });
      return Promise.resolve(this.snapshot);
    }

    const candidate = this.candidate;
    const version = candidate.version;
    const alreadyDownloaded = this.downloadedCandidate === candidate;
    if (automatic && !this.runtime.tryPrepareAutoUpdateRelaunch) {
      this.setSnapshot({
        ...this.snapshot,
        phase: "available",
        version,
        message: `Version ${version} is available. Automatic installation requires the native worker safety check; install it manually from Settings.`,
        error: null,
        canInstall: true,
        restartRequired: false,
      });
      return Promise.resolve(this.snapshot);
    }
    this.setSnapshot({
      ...this.snapshot,
      phase: "downloading",
      version,
      message: alreadyDownloaded
        ? `Preparing verified update ${version}…`
        : `Downloading signed update ${version}…`,
      error: null,
      downloadedBytes: 0,
      contentLength: null,
      canInstall: false,
      restartRequired: false,
    });

    const install = (async () => {
      let handoffStarted = false;
      let verifiedDownloaded = alreadyDownloaded;
      try {
        if (!alreadyDownloaded) await candidate.download((event) => {
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
        this.downloadedCandidate = candidate;
        verifiedDownloaded = true;

        if (automatic && !this.isAutoInstallCurrent(autoInstallGeneration)) {
          this.setSnapshot({
            ...this.snapshot,
            phase: "available",
            message: `Version ${version} was downloaded and verified. Automatic installation was turned off; install it manually from Settings if you choose.`,
            error: null,
            canInstall: true,
            restartRequired: false,
          });
          return this.snapshot;
        }

        if (automatic) {
          let outcome: AutoUpdatePrepareResult;
          try {
            outcome = await this.runtime.tryPrepareAutoUpdateRelaunch!();
          } catch (error) {
            const detail = errorMessage(error);
            const lastSession = isStartupReconciliationInProgress(detail);
            if (lastSession) this.showWaitingForLastSession(version);
            try {
              await this.runtime.abortRelaunch();
            } catch (abortError) {
              this.setSnapshot({
                ...this.snapshot,
                phase: "error",
                message: "Automatic installation did not begin, but ARC could not confirm that its node resumed. Quit and reopen ARC before continuing.",
                error: `${detail}; resume failed: ${errorMessage(abortError)}`,
                canInstall: false,
                restartRequired: true,
              });
              return this.snapshot;
            }
            if (lastSession) {
              if (!this.isAutoInstallCurrent(autoInstallGeneration)) {
                this.setSnapshot({
                  ...this.snapshot,
                  phase: "available",
                  message: `Version ${version} is available.`,
                  error: null,
                  canInstall: true,
                  restartRequired: false,
                });
                return this.snapshot;
              }
              return this.waitForLastSession(candidate, version, true);
            }
            const wrongLocation = detail.includes(NATIVE_WRONG_LOCATION);
            this.setSnapshot({
              ...this.snapshot,
              phase: "available",
              message: wrongLocation
                ? PLAIN_COPY.updates.wrongLocation
                : /legacy desktop node reconciliation is unresolved/.test(detail)
                  ? PLAIN_COPY.updates.manualInstallOnly(version)
                  : `Automatic installation of ${version} was deferred because native worker safety could not be confirmed. Install it manually from Settings if you choose.`,
              error: wrongLocation ? null : detail,
              canInstall: true,
              restartRequired: false,
            });
            return this.snapshot;
          }

          if (outcome.status === "busy") {
            const detail = outcome.reason || "inference work is still active";
            this.setSnapshot({
              ...this.snapshot,
              phase: "available",
              message: `Automatic installation of ${version} is waiting for inference to finish: ${detail} (${outcome.activeJobs} active).`,
              error: null,
              canInstall: true,
              restartRequired: false,
            });
            this.scheduleBusyInstallRetry(candidate);
            return this.snapshot;
          }

          if (outcome.status !== "prepared") {
            try {
              await this.runtime.abortRelaunch();
            } catch (error) {
              this.setSnapshot({
                ...this.snapshot,
                phase: "error",
                message: "ARC received an invalid worker safety result and could not confirm that its node resumed. Quit and reopen ARC before continuing.",
                error: errorMessage(error),
                canInstall: false,
                restartRequired: true,
              });
              return this.snapshot;
            }
            this.setSnapshot({
              ...this.snapshot,
              phase: "available",
              message: `Automatic installation of ${version} was deferred because native worker safety returned an unrecognized result. Install it manually from Settings if you choose.`,
              error: "Unrecognized automatic update preparation result.",
              canInstall: true,
              restartRequired: false,
            });
            return this.snapshot;
          }

          if (!this.isAutoInstallCurrent(autoInstallGeneration)) {
            try {
              await this.runtime.abortRelaunch();
            } catch (error) {
              const detail = errorMessage(error);
              this.setSnapshot({
                ...this.snapshot,
                phase: "error",
                message: `No app update was started, but ARC could not confirm that its node resumed. Quit and reopen ARC before continuing.`,
                error: detail,
                canInstall: false,
                restartRequired: true,
              });
              return this.snapshot;
            }
            this.setSnapshot({
              ...this.snapshot,
              phase: "available",
              message: `Automatic installation of ${version} was turned off before handoff. The node resumed; install manually from Settings if you choose.`,
              error: null,
              canInstall: true,
              restartRequired: false,
            });
            return this.snapshot;
          }
        } else {
          await this.runtime.prepareRelaunch();
        }

        handoffStarted = true;
        await this.runtime.beginHandoff();
        await candidate.install();

        this.candidate = null;
        this.downloadedCandidate = null;
        void candidate.close?.().catch(() => {});
        this.setSnapshot({
          ...this.snapshot,
          phase: "ready",
          version,
          message: `Version ${version} installed. Safely stopping the node before restart…`,
          error: null,
          canInstall: false,
          restartRequired: true,
        });

        try {
          this.setSnapshot({
            ...this.snapshot,
            message: `Version ${version} installed. Restarting ARC now…`,
          });
          await this.runtime.relaunch();
        } catch (error) {
          const detail = errorMessage(error);
          this.setSnapshot({
            ...this.snapshot,
            phase: "ready",
            message: `Version ${version} is installed, but ARC could not safely stop the old node and relaunch. Quit ARC from the tray, then reopen it to finish updating.`,
            error: detail,
            canInstall: false,
            restartRequired: true,
          });
        }
      } catch (error) {
        const detail = errorMessage(error);
        if (automatic && !handoffStarted) {
          this.setSnapshot({
            ...this.snapshot,
            phase: "available",
            message: `Automatic installation of ${version} was deferred: ${detail}. Install it manually from Settings if you choose.`,
            error: detail,
            canInstall: true,
            restartRequired: false,
          });
          if (!verifiedDownloaded) this.scheduleAutoInstallRetry(candidate);
          return this.snapshot;
        }
        if (handoffStarted) {
          this.candidate = null;
          this.downloadedCandidate = null;
          void candidate.close?.().catch(() => {});
          this.setSnapshot({
            ...this.snapshot,
            phase: "ready",
            version,
            message: `ARC could not verify that update ${version} finished installing. The node remains safely stopped. Quit ARC from the tray, then reopen it; reinstall from the verified release if ARC does not reopen.`,
            error: detail,
            canInstall: false,
            restartRequired: true,
          });
          return this.snapshot;
        }
        if (isStartupReconciliationInProgress(detail)) {
          return this.waitForLastSession(candidate, version, false);
        }
        if (detail.includes(NATIVE_WRONG_LOCATION)) {
          this.setSnapshot({
            ...this.snapshot,
            phase: "available",
            version,
            message: PLAIN_COPY.updates.wrongLocation,
            error: null,
            canInstall: true,
            restartRequired: false,
          });
          return this.snapshot;
        }
        this.setSnapshot({
          ...this.snapshot,
          phase: "error",
          version,
          message: `Update was not installed: ${detail}`,
          error: detail,
          canInstall: candidate.canInstall !== false,
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

  dispose(): void {
    this.setAutoInstallEnabled(false);
    this.clearTimers();
    this.clearReconciliationRetryTimer();
    this.autoChecksEnabled = false;
    const candidate = this.candidate;
    this.candidate = null;
    this.downloadedCandidate = null;
    if (candidate) void candidate.close?.().catch(() => {});
    this.listeners.clear();
  }

  private clearTimers(): void {
    if (this.startupTimer !== null) {
      this.timers.clearTimeout(this.startupTimer);
      this.startupTimer = null;
    }
    if (this.intervalTimer !== null) {
      this.timers.clearTimeout(this.intervalTimer);
      this.intervalTimer = null;
    }
    this.intervalDueAt = null;
    this.clearRetryTimer();
    this.clearAutoInstallRetryTimer();
  }

  private clearRetryTimer(): void {
    if (this.retryTimer !== null) {
      this.timers.clearTimeout(this.retryTimer);
      this.retryTimer = null;
    }
    this.retryDueAt = null;
  }

  private clearAutoInstallRetryTimer(): void {
    if (this.autoInstallRetryTimer !== null) {
      this.timers.clearTimeout(this.autoInstallRetryTimer);
      this.autoInstallRetryTimer = null;
    }
    this.autoInstallRetryDueAt = null;
    this.autoInstallRetryFire = null;
  }

  private autoInstallRetryFire: (() => void) | null = null;

  private setAutoInstallRetry(fire: () => void, delayMs: number): void {
    this.autoInstallRetryDueAt = this.now() + delayMs;
    this.autoInstallRetryFire = fire;
    this.autoInstallRetryTimer = this.timers.setTimeout(() => {
      this.autoInstallRetryTimer = null;
      this.autoInstallRetryDueAt = null;
      this.autoInstallRetryFire = null;
      fire();
    }, delayMs);
  }

  private clearReconciliationRetryTimer(): void {
    if (this.reconciliationRetryTimer !== null) {
      this.timers.clearTimeout(this.reconciliationRetryTimer);
      this.reconciliationRetryTimer = null;
    }
    this.reconciliationWaitManual = false;
  }

  private showWaitingForLastSession(version: string): void {
    this.setSnapshot({
      ...this.snapshot,
      phase: "available",
      version,
      message: PLAIN_COPY.updates.waitingForLastSession(version),
      error: null,
      canInstall: true,
      restartRequired: false,
    });
  }

  private waitForLastSession(
    candidate: UpdateCandidate,
    version: string,
    automatic: boolean,
  ): UpdateSnapshot {
    this.showWaitingForLastSession(version);
    if (this.reconciliationRetryTimer === null) {
      this.reconciliationWaitManual = !automatic;
      this.reconciliationRetryTimer = this.timers.setTimeout(() => {
        const manual = this.reconciliationWaitManual;
        this.reconciliationRetryTimer = null;
        this.reconciliationWaitManual = false;
        if (this.candidate !== candidate) return;
        if (manual) void this.startInstall(null);
        else void this.installAutomatically(candidate);
      }, UPDATE_RECONCILIATION_RETRY_MS);
    } else if (!automatic) {
      this.reconciliationWaitManual = true;
    }
    return this.snapshot;
  }

  private isAutoInstallCurrent(generation: number | null): boolean {
    return (
      generation !== null &&
      this.autoInstallEnabled &&
      this.autoChecksEnabled &&
      generation === this.autoInstallGeneration
    );
  }

  private scheduleAutoInstallRetry(candidate: UpdateCandidate): void {
    if (
      !this.autoInstallEnabled ||
      this.autoInstallRetryTimer !== null ||
      this.candidate !== candidate
    ) {
      return;
    }
    const delayMs = this.retryDelaysMs[this.autoInstallDeferralCount];
    if (delayMs === undefined) return;
    this.autoInstallDeferralCount += 1;
    this.setAutoInstallRetry(() => {
      void this.installAutomatically(candidate);
    }, delayMs);
  }

  private scheduleBusyInstallRetry(candidate: UpdateCandidate): void {
    if (
      !this.autoInstallEnabled ||
      this.autoInstallRetryTimer !== null ||
      this.candidate !== candidate
    ) {
      return;
    }
    this.setAutoInstallRetry(() => {
      void this.installAutomatically(candidate);
    }, this.busyRetryMs);
  }

  private scheduleAutomaticRetry(): void {
    if (!this.autoChecksEnabled || this.retryTimer !== null) return;
    const delayMs = this.retryDelaysMs[this.automaticFailureCount];
    if (delayMs === undefined) return;
    this.automaticFailureCount += 1;
    this.retryDueAt = this.now() + delayMs;
    this.retryTimer = this.timers.setTimeout(() => {
      this.retryTimer = null;
      this.retryDueAt = null;
      void this.checkForUpdates("retry");
    }, delayMs);
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
