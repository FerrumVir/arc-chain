import { UpdateDeadline } from "../src/lib/update-deadline";
import { expect, test } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import {
  createUpdateController,
  NATIVE_WRONG_LOCATION,
  UPDATE_BUSY_RETRY_MS,
  UPDATE_CHECK_INTERVAL_MS,
  UPDATE_RECONCILIATION_RETRY_MS,
  UPDATE_RETRY_DELAYS_MS,
  UPDATE_SIGNAL_MIN_GAP_MS,
  type UpdateCandidate,
  type UpdateDownloadEvent,
  type UpdateTimers,
} from "../src/lib/update-controller";

const __dirnameHere = path.dirname(fileURLToPath(import.meta.url));

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

class FakeTimers implements UpdateTimers {
  private nowMs = 0;
  private nextId = 1;
  private readonly tasks = new Map<
    number,
    { at: number; callback: () => void; everyMs: number | null }
  >();

  setTimeout = (callback: () => void, delayMs: number): number =>
    this.add(callback, delayMs, null);

  clearTimeout = (handle: unknown): void => {
    this.tasks.delete(handle as number);
  };

  setInterval = (callback: () => void, delayMs: number): number =>
    this.add(callback, delayMs, delayMs);

  now = (): number => this.nowMs;

  async skipWithoutFiring(ms: number): Promise<void> {
    this.nowMs += ms;
    for (const task of this.tasks.values()) task.at += ms;
    await this.flushPromises();
  }

  clearInterval = this.clearTimeout;

  async advanceBy(ms: number): Promise<void> {
    const target = this.nowMs + ms;

    for (;;) {
      const due = [...this.tasks.entries()]
        .filter(([, task]) => task.at <= target)
        .sort((a, b) => a[1].at - b[1].at || a[0] - b[0])[0];
      if (!due) break;

      const [id, task] = due;
      this.nowMs = task.at;
      if (task.everyMs === null) {
        this.tasks.delete(id);
      } else {
        task.at += task.everyMs;
      }
      task.callback();
      await this.flushPromises();
    }

    this.nowMs = target;
    await this.flushPromises();
  }

  private add(
    callback: () => void,
    delayMs: number,
    everyMs: number | null,
  ): number {
    const id = this.nextId++;
    this.tasks.set(id, {
      at: this.nowMs + Math.max(0, delayMs),
      callback,
      everyMs,
    });
    return id;
  }

  private async flushPromises(): Promise<void> {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  }
}

function updateCheckHarness() {
  const timers = new FakeTimers();
  const callbacks: Array<() => void> = [];
  let checks = 0;
  const controller = createUpdateController(
    {
      supported: true,
      now: timers.now,
      check: async () => {
        checks += 1;
        return null;
      },
      prepareRelaunch: async () => {},
      beginHandoff: async () => {},
      abortRelaunch: async () => {},
      relaunch: async () => {},
    },
    { startupDelayMs: 10, intervalMs: 10_000 },
    {
      setTimeout: (callback, delay) => {
        callbacks.push(callback);
        return timers.setTimeout(callback, delay);
      },
      clearTimeout: timers.clearTimeout,
      setInterval: timers.setInterval,
      clearInterval: timers.clearInterval,
    },
  );
  return { controller, timers, callbacks, checks: () => checks };
}

function retryDeadlineHarness(kind: "check" | "busy" | "reconciliation") {
  const timers = new FakeTimers();
  const callbacks: Array<{ callback: () => void; delay: number }> = [];
  let attempts = 0;
  const reconciling = "managed-node startup reconciliation is still in progress: ARC is checking how the previous node session ended before it starts or updates arc-node. Try again in a few seconds.";
  const prepare = async () => {
    attempts += 1;
    if (kind === "reconciliation") throw new Error(reconciling);
    return { status: "busy" as const, reason: "inference active", activeJobs: 1 };
  };
  const controller = createUpdateController(
    {
      supported: true,
      now: timers.now,
      check: async () => {
        if (kind === "check") {
          attempts += 1;
          throw new Error("offline");
        }
        return { version: "0.8.27", download: async () => {}, install: async () => {} };
      },
      tryPrepareAutoUpdateRelaunch: prepare,
      prepareRelaunch: async () => { await prepare(); },
      beginHandoff: async () => {},
      abortRelaunch: async () => {},
      relaunch: async () => {},
    },
    { startupDelayMs: 10, intervalMs: 1_000_000, retryDelaysMs: [20, 40, 80], busyRetryMs: 20 },
    {
      setTimeout: (callback, delay) => {
        callbacks.push({ callback, delay });
        return timers.setTimeout(callback, delay);
      },
      clearTimeout: timers.clearTimeout,
      setInterval: timers.setInterval,
      clearInterval: timers.clearInterval,
    },
  );
  const flush = async () => {
    await timers.advanceBy(0);
    await timers.advanceBy(0);
  };
  return {
    controller, timers, callbacks, flush, attempts: () => attempts,
    retryDelay: kind === "reconciliation" ? UPDATE_RECONCILIATION_RETRY_MS : 20,
    async start() {
      controller.setAutoChecksEnabled(true);
      if (kind !== "check") controller.setAutoInstallEnabled(true);
      await timers.advanceBy(10);
      await flush();
    },
  };
}

test.describe("UpdateController", () => {
  for (const kind of ["check", "busy", "reconciliation"] as const) {
    test(`${kind} retry deadline recovers once and ignores stale delivery`, async () => {
      const h = retryDeadlineHarness(kind);
      await h.start();
      expect(h.attempts()).toBe(1);
      const original = h.callbacks.find((item) => item.delay === h.retryDelay)!.callback;
      await h.timers.skipWithoutFiring(h.retryDelay);
      h.controller.signal("tick");
      await h.flush();
      expect(h.attempts()).toBe(2);
      original();
      await h.flush();
      expect(h.attempts()).toBe(2);
      await h.timers.skipWithoutFiring(kind === "check" ? 40 : h.retryDelay);
      h.controller.signal("tick");
      await h.flush();
      expect(h.attempts()).toBe(3);
      h.controller.dispose();
    });

    test(`${kind} retry deadline cannot replace a reenabled wait`, async () => {
      const h = retryDeadlineHarness(kind);
      await h.start();
      const original = h.callbacks.find((item) => item.delay === h.retryDelay)!.callback;
      if (kind === "check") {
        h.controller.setAutoChecksEnabled(false);
        h.controller.setAutoChecksEnabled(true);
        await h.timers.advanceBy(10);
      } else {
        h.controller.setAutoInstallEnabled(false);
        h.controller.setAutoInstallEnabled(true);
      }
      await h.flush();
      expect(h.attempts()).toBe(2);
      original();
      await h.flush();
      expect(h.attempts()).toBe(2);
      await h.timers.skipWithoutFiring(h.retryDelay);
      h.controller.signal("tick");
      await h.flush();
      expect(h.attempts()).toBe(3);
      h.controller.dispose();
    });
  }

  for (const stop of ["disable", "dispose"] as const) {
    test(`a failed-check retry cannot run after ${stop}`, async () => {
      const h = retryDeadlineHarness("check");
      await h.start();
      const original = h.callbacks.find((item) => item.delay === h.retryDelay)!.callback;
      if (stop === "disable") h.controller.setAutoChecksEnabled(false);
      else h.controller.dispose();
      original();
      await h.flush();
      expect(h.attempts()).toBe(1);
      h.controller.dispose();
    });
  }

  test("a manual reconciliation deadline recovers while automatic checks are off", async () => {
    const h = retryDeadlineHarness("reconciliation");
    await h.controller.checkForUpdates("manual");
    await h.controller.installAvailableUpdate();
    expect(h.attempts()).toBe(1);
    const original = h.callbacks.find((item) => item.delay === h.retryDelay)!.callback;
    await h.timers.skipWithoutFiring(h.retryDelay);
    h.controller.signal("tick");
    await h.flush();
    expect(h.attempts()).toBe(2);
    original();
    await h.flush();
    expect(h.attempts()).toBe(2);
    h.controller.dispose();
  });

  test("native tick recovers a suspended startup check exactly once", async () => {
    const h = updateCheckHarness();
    h.controller.setAutoChecksEnabled(true);
    const originalStartup = h.callbacks[0];
    await h.timers.skipWithoutFiring(11);
    h.controller.signal("tick");
    await h.timers.advanceBy(0);
    expect(h.checks()).toBe(1);

    originalStartup();
    await h.timers.advanceBy(10);
    expect(h.checks()).toBe(1);
    h.controller.dispose();
  });

  for (const stop of ["disable", "dispose"] as const) {
    test(`a queued startup callback cannot check after ${stop}`, async () => {
      const h = updateCheckHarness();
      h.controller.setAutoChecksEnabled(true);
      const originalStartup = h.callbacks[0];
      if (stop === "disable") h.controller.setAutoChecksEnabled(false);
      else h.controller.dispose();
      originalStartup();
      await h.timers.advanceBy(11);
      h.controller.signal("tick");
      expect(h.checks()).toBe(0);
      h.controller.dispose();
    });
  }

  test("a previous startup callback cannot consume a reenabled deadline", async () => {
    const h = updateCheckHarness();
    h.controller.setAutoChecksEnabled(true);
    const originalStartup = h.callbacks[0];
    h.controller.setAutoChecksEnabled(false);
    h.controller.setAutoChecksEnabled(true);
    originalStartup();
    await h.timers.advanceBy(0);
    expect(h.checks()).toBe(0);
    await h.timers.advanceBy(10);
    expect(h.checks()).toBe(1);
    h.controller.dispose();
  });

  test("an overdue interval ignores its late callback after native tick recovery", async () => {
    const h = updateCheckHarness();
    h.controller.setAutoChecksEnabled(true);
    const originalInterval = h.callbacks[1];
    await h.timers.advanceBy(10);
    await h.timers.skipWithoutFiring(10_000);
    h.controller.signal("tick");
    await h.timers.advanceBy(0);
    expect(h.checks()).toBe(2);
    originalInterval();
    await h.timers.advanceBy(0);
    expect(h.checks()).toBe(2);
    h.controller.dispose();
  });

  for (const stop of ["disable", "dispose"] as const) {
    test(`a queued interval callback cannot check after ${stop}`, async () => {
      const h = updateCheckHarness();
      h.controller.setAutoChecksEnabled(true);
      const originalInterval = h.callbacks[1];
      if (stop === "disable") h.controller.setAutoChecksEnabled(false);
      else h.controller.dispose();
      originalInterval();
      await h.timers.advanceBy(20_000);
      expect(h.checks()).toBe(0);
      h.controller.dispose();
    });
  }

  test("a previous interval callback cannot replace a reenabled deadline", async () => {
    const h = updateCheckHarness();
    h.controller.setAutoChecksEnabled(true);
    const originalInterval = h.callbacks[1];
    h.controller.setAutoChecksEnabled(false);
    h.controller.setAutoChecksEnabled(true);
    originalInterval();
    await h.timers.advanceBy(0);
    expect(h.checks()).toBe(0);
    await h.timers.advanceBy(10_000);
    expect(h.checks()).toBe(2);
    h.controller.dispose();
  });

  test("checks after startup and periodically only while enabled", async () => {
    const timers = new FakeTimers();
    let checkCalls = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => {
          checkCalls += 1;
          return null;
        },
        prepareRelaunch: async () => {},
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
      },
      { startupDelayMs: 1_000, intervalMs: 10_000 },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    controller.setAutoChecksEnabled(true);
    await timers.advanceBy(999);
    expect(checkCalls).toBe(0);

    await timers.advanceBy(1);
    expect(checkCalls).toBe(1);
    expect(controller.getSnapshot().phase).toBe("up-to-date");

    await timers.advanceBy(9_000);
    expect(checkCalls).toBe(2);

    controller.setAutoChecksEnabled(false);
    await timers.advanceBy(30_000);
    expect(checkCalls).toBe(2);

    await controller.checkForUpdates("manual");
    expect(checkCalls).toBe(3);
    controller.dispose();
  });

  test("automatic checks remain check-only unless installs are separately opted in", async () => {
    const timers = new FakeTimers();
    let checks = 0;
    let downloads = 0;
    let installs = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => {
          checks += 1;
          return {
            version: "0.8.23",
            download: async () => {
              downloads += 1;
            },
            install: async () => {
              installs += 1;
            },
          };
        },
        prepareRelaunch: async () => {},
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
      },
      { startupDelayMs: 10, intervalMs: 1_000 },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    await timers.advanceBy(10);

    expect(checks).toBe(1);
    expect(downloads).toBe(0);
    expect(installs).toBe(0);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      version: "0.8.23",
      canInstall: true,
    });
    controller.dispose();
  });

  test("auto install waits for native worker quiescence and retries a busy candidate", async () => {
    const timers = new FakeTimers();
    const order: string[] = [];
    let prepareAttempts = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => ({
          version: "0.8.23",
          download: async () => {
            order.push("download");
          },
          install: async () => {
            order.push("install");
          },
        }),
        tryPrepareAutoUpdateRelaunch: async () => {
          order.push("try-prepare");
          prepareAttempts += 1;
          return prepareAttempts === 1
            ? { status: "busy", reason: "inference active", activeJobs: 1 }
            : { status: "prepared" };
        },
        prepareRelaunch: async () => {
          order.push("manual-prepare");
        },
        beginHandoff: async () => {
          order.push("handoff");
        },
        abortRelaunch: async () => {
          order.push("abort");
        },
        relaunch: async () => {
          order.push("relaunch");
        },
      },
      {
        startupDelayMs: 10,
        intervalMs: 1_000,
        retryDelaysMs: [500],
        busyRetryMs: 20,
      },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    controller.setAutoInstallEnabled(true);
    await timers.advanceBy(10);
    expect(order).toEqual(["download", "try-prepare"]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      canInstall: true,
      restartRequired: false,
    });
    expect(controller.getSnapshot().message).toContain("inference active");

    await timers.advanceBy(20);
    expect(order).toEqual([
      "download",
      "try-prepare",
      "try-prepare",
      "handoff",
      "install",
      "relaunch",
    ]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "ready",
      restartRequired: true,
    });
    controller.dispose();
  });

  test("the default cadence is a check every 6 hours, with busy installs retried hourly", () => {
    expect(UPDATE_CHECK_INTERVAL_MS).toBe(6 * 60 * 60 * 1_000);
    expect(UPDATE_BUSY_RETRY_MS).toBe(60 * 60 * 1_000);
    expect(UPDATE_RETRY_DELAYS_MS).toEqual([60_000, 5 * 60_000, 15 * 60_000]);
    expect(UPDATE_SIGNAL_MIN_GAP_MS).toBe(15 * 60_000);
  });

  test("checks every 6 hours by default", async () => {
    const timers = new FakeTimers();
    let checkCalls = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => {
          checkCalls += 1;
          return null;
        },
        prepareRelaunch: async () => {},
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
        now: timers.now,
      },
      {},
      timers,
    );
    controller.setAutoChecksEnabled(true);
    await timers.advanceBy(5_000);
    expect(checkCalls).toBe(1);
    await timers.advanceBy(6 * 60 * 60 * 1_000 - 5_000 - 1);
    expect(checkCalls).toBe(1);
    await timers.advanceBy(1);
    expect(checkCalls).toBe(2);
    await timers.advanceBy(6 * 60 * 60 * 1_000);
    expect(checkCalls).toBe(3);
    controller.dispose();
  });

  test("checks when the window is shown or focused and after a wake, at most once per gap", async () => {
    const timers = new FakeTimers();
    let checkCalls = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => {
          checkCalls += 1;
          return null;
        },
        prepareRelaunch: async () => {},
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
        now: timers.now,
      },
      { startupDelayMs: 10, intervalMs: 100_000, signalMinGapMs: 1_000 },
      timers,
    );

    controller.signal("focus");
    await timers.advanceBy(0);
    expect(checkCalls).toBe(0);

    controller.setAutoChecksEnabled(true);
    await timers.advanceBy(10);
    expect(checkCalls).toBe(1);

    controller.signal("focus");
    await timers.advanceBy(0);
    expect(checkCalls).toBe(1);

    await timers.advanceBy(1_000);
    controller.signal("shown");
    await timers.advanceBy(0);
    expect(checkCalls).toBe(2);

    await timers.advanceBy(1_000);
    controller.signal("focus");
    await timers.advanceBy(0);
    expect(checkCalls).toBe(3);

    await timers.advanceBy(1_000);
    controller.signal("tick");
    await timers.advanceBy(0);
    expect(checkCalls).toBe(3);

    await timers.skipWithoutFiring(50_000);
    controller.signal("wake");
    await timers.advanceBy(0);
    expect(checkCalls).toBe(4);

    controller.setAutoChecksEnabled(false);
    await timers.advanceBy(5_000);
    controller.signal("wake");
    await timers.advanceBy(0);
    expect(checkCalls).toBe(4);
    controller.dispose();
  });

  test("a native tick runs the 6-hour check that throttled WebView timers held back", async () => {
    const timers = new FakeTimers();
    let checkCalls = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => {
          checkCalls += 1;
          return null;
        },
        prepareRelaunch: async () => {},
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
        now: timers.now,
      },
      { startupDelayMs: 10, intervalMs: 1_000 },
      timers,
    );
    controller.setAutoChecksEnabled(true);
    await timers.advanceBy(10);
    expect(checkCalls).toBe(1);

    await timers.skipWithoutFiring(5_000);
    expect(checkCalls).toBe(1);
    controller.signal("tick");
    await timers.advanceBy(0);
    expect(checkCalls).toBe(2);

    await timers.advanceBy(999);
    expect(checkCalls).toBe(2);
    await timers.advanceBy(1);
    expect(checkCalls).toBe(3);
    controller.dispose();
  });

  test("an install deferred by running inference retries every hour while it stays busy", async () => {
    const timers = new FakeTimers();
    const order: string[] = [];
    let prepareAttempts = 0;
    const hour = 60 * 60 * 1_000;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => ({
          version: "0.8.23",
          download: async () => {
            order.push("download");
          },
          install: async () => {
            order.push("install");
          },
        }),
        tryPrepareAutoUpdateRelaunch: async () => {
          prepareAttempts += 1;
          order.push("try-prepare");
          return prepareAttempts <= 5
            ? { status: "busy", reason: "inference active", activeJobs: 1 }
            : { status: "prepared" };
        },
        prepareRelaunch: async () => {},
        beginHandoff: async () => {
          order.push("handoff");
        },
        abortRelaunch: async () => {},
        relaunch: async () => {
          order.push("relaunch");
        },
        now: timers.now,
      },
      { startupDelayMs: 10, intervalMs: 100 * hour },
      timers,
    );
    controller.setAutoChecksEnabled(true);
    controller.setAutoInstallEnabled(true);
    await timers.advanceBy(10);
    expect(prepareAttempts).toBe(1);

    await timers.advanceBy(15 * 60_000);
    expect(prepareAttempts).toBe(1);
    await timers.advanceBy(hour - 15 * 60_000 - 1);
    expect(prepareAttempts).toBe(1);
    await timers.advanceBy(1);
    expect(prepareAttempts).toBe(2);
    for (let attempt = 3; attempt <= 5; attempt += 1) {
      await timers.advanceBy(hour);
      expect(prepareAttempts).toBe(attempt);
      expect(controller.getSnapshot().message).toContain("waiting for inference to finish");
    }
    expect(order.filter((step) => step === "download")).toEqual(["download"]);

    await timers.skipWithoutFiring(hour);
    controller.signal("tick");
    await timers.advanceBy(0);
    expect(prepareAttempts).toBe(6);
    expect(order.slice(-3)).toEqual(["handoff", "install", "relaunch"]);
    controller.dispose();
  });

  test("a signal does not re-check while a found update waits to install", async () => {
    const timers = new FakeTimers();
    let checkCalls = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => {
          checkCalls += 1;
          return {
            version: "0.8.23",
            download: async () => {},
            install: async () => {},
          };
        },
        tryPrepareAutoUpdateRelaunch: async () => ({
          status: "busy",
          reason: "inference active",
          activeJobs: 1,
        }),
        prepareRelaunch: async () => {},
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
        now: timers.now,
      },
      { startupDelayMs: 10, intervalMs: 100_000_000, signalMinGapMs: 1_000 },
      timers,
    );
    controller.setAutoChecksEnabled(true);
    controller.setAutoInstallEnabled(true);
    await timers.advanceBy(10);
    expect(checkCalls).toBe(1);
    await timers.advanceBy(10_000);
    controller.signal("focus");
    controller.signal("wake");
    await timers.advanceBy(0);
    expect(checkCalls, "the update already found is not downloaded again").toBe(1);
    controller.dispose();
  });

  test("failed checks still retry at 1, 5 and 15 minutes by default", async () => {
    const timers = new FakeTimers();
    let checkCalls = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => {
          checkCalls += 1;
          throw new Error("offline");
        },
        prepareRelaunch: async () => {},
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
        now: timers.now,
      },
      { startupDelayMs: 10 },
      timers,
    );
    controller.setAutoChecksEnabled(true);
    await timers.advanceBy(10);
    expect(checkCalls).toBe(1);
    await timers.advanceBy(60_000);
    expect(checkCalls).toBe(2);
    await timers.advanceBy(5 * 60_000);
    expect(checkCalls).toBe(3);
    await timers.advanceBy(15 * 60_000);
    expect(checkCalls).toBe(4);
    await timers.advanceBy(60 * 60_000);
    expect(checkCalls).toBe(4);
    controller.dispose();
  });

  test("a failed update download still retries at the failure delays", async () => {
    const timers = new FakeTimers();
    let downloads = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => ({
          version: "0.8.23",
          download: async () => {
            downloads += 1;
            throw new Error("connection reset");
          },
          install: async () => {},
        }),
        tryPrepareAutoUpdateRelaunch: async () => ({ status: "prepared" }),
        prepareRelaunch: async () => {},
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
        now: timers.now,
      },
      {
        startupDelayMs: 10,
        intervalMs: 100_000_000,
        retryDelaysMs: [20, 40],
        busyRetryMs: 5,
      },
      timers,
    );
    controller.setAutoChecksEnabled(true);
    controller.setAutoInstallEnabled(true);
    await timers.advanceBy(10);
    expect(downloads).toBe(1);
    await timers.advanceBy(19);
    expect(downloads).toBe(1);
    await timers.advanceBy(1);
    expect(downloads).toBe(2);
    await timers.advanceBy(40);
    expect(downloads).toBe(3);
    await timers.advanceBy(1_000);
    expect(downloads).toBe(3);
    controller.dispose();
  });

  test("turning auto install off during download prevents native preparation", async () => {
    const timers = new FakeTimers();
    const download = deferred<void>();
    let prepareAttempts = 0;
    let installCalls = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => ({
          version: "0.8.23",
          download: () => download.promise,
          install: async () => {
            installCalls += 1;
          },
        }),
        tryPrepareAutoUpdateRelaunch: async () => {
          prepareAttempts += 1;
          return { status: "prepared" };
        },
        prepareRelaunch: async () => {},
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
      },
      { startupDelayMs: 10, intervalMs: 1_000 },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    controller.setAutoInstallEnabled(true);
    await timers.advanceBy(10);
    controller.setAutoInstallEnabled(false);
    download.resolve();
    await timers.advanceBy(0);

    expect(prepareAttempts).toBe(0);
    expect(installCalls).toBe(0);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      canInstall: true,
      restartRequired: false,
    });
    expect(controller.getSnapshot().message).toContain("turned off");
    controller.dispose();
  });

  test("turning auto install off during native drain aborts before handoff", async () => {
    const timers = new FakeTimers();
    const prepare = deferred<{ status: "prepared" }>();
    const order: string[] = [];
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => ({
          version: "0.8.23",
          download: async () => {
            order.push("download");
          },
          install: async () => {
            order.push("install");
          },
        }),
        tryPrepareAutoUpdateRelaunch: () => {
          order.push("try-prepare");
          return prepare.promise;
        },
        prepareRelaunch: async () => {},
        beginHandoff: async () => {
          order.push("handoff");
        },
        abortRelaunch: async () => {
          order.push("abort");
        },
        relaunch: async () => {},
      },
      { startupDelayMs: 10, intervalMs: 1_000 },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    controller.setAutoInstallEnabled(true);
    await timers.advanceBy(10);
    expect(order).toEqual(["download", "try-prepare"]);
    controller.setAutoInstallEnabled(false);
    prepare.resolve({ status: "prepared" });
    await timers.advanceBy(0);

    expect(order).toEqual(["download", "try-prepare", "abort"]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      canInstall: true,
      restartRequired: false,
    });
    expect(controller.getSnapshot().message).toContain("turned off");
    controller.dispose();
  });

  test("missing native auto safety support never falls back to manual preparation", async () => {
    const timers = new FakeTimers();
    const order: string[] = [];
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => ({
          version: "0.8.23",
          download: async () => {
            order.push("download");
          },
          install: async () => {
            order.push("install");
          },
        }),
        prepareRelaunch: async () => {
          order.push("manual-prepare");
        },
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
      },
      { startupDelayMs: 10, intervalMs: 1_000 },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    controller.setAutoInstallEnabled(true);
    await timers.advanceBy(10);

    expect(order).toEqual([]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      canInstall: true,
      restartRequired: false,
    });
    expect(controller.getSnapshot().message).toContain(
      "requires the native worker safety check",
    );
    controller.dispose();
  });

  test("native auto safety command errors abort pre-handoff and fail closed", async () => {
    const timers = new FakeTimers();
    const order: string[] = [];
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => ({
          version: "0.8.23",
          download: async () => {
            order.push("download");
          },
          install: async () => {
            order.push("install");
          },
        }),
        tryPrepareAutoUpdateRelaunch: async () => {
          order.push("try-prepare");
          throw new Error("unknown command");
        },
        prepareRelaunch: async () => {
          order.push("manual-prepare");
        },
        beginHandoff: async () => {
          order.push("handoff");
        },
        abortRelaunch: async () => {
          order.push("abort");
        },
        relaunch: async () => {},
      },
      { startupDelayMs: 10, intervalMs: 1_000 },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    controller.setAutoInstallEnabled(true);
    await timers.advanceBy(10);

    expect(order).toEqual(["download", "try-prepare", "abort"]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      canInstall: true,
      restartRequired: false,
    });
    expect(controller.getSnapshot().error).toBe("unknown command");
    controller.dispose();
  });

  test("retries failed automatic checks with bounded backoff and cancels retry on disable", async () => {
    const timers = new FakeTimers();
    let checkCalls = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => {
          checkCalls += 1;
          if (checkCalls < 4) throw new Error("offline");
          return null;
        },
        prepareRelaunch: async () => {},
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
      },
      { startupDelayMs: 10, intervalMs: 1_000, retryDelaysMs: [20, 40, 80] },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    await timers.advanceBy(10);
    expect(checkCalls).toBe(1);
    await timers.advanceBy(19);
    expect(checkCalls).toBe(1);
    await timers.advanceBy(1);
    expect(checkCalls).toBe(2);
    await timers.advanceBy(39);
    expect(checkCalls).toBe(2);
    await timers.advanceBy(1);
    expect(checkCalls).toBe(3);
    await timers.advanceBy(80);
    expect(checkCalls).toBe(4);
    expect(controller.getSnapshot().phase).toBe("up-to-date");

    controller.setAutoChecksEnabled(false);
    await timers.advanceBy(100);
    expect(checkCalls).toBe(4);
    controller.dispose();
  });

  test("cancels a pending automatic retry when checks are disabled", async () => {
    const timers = new FakeTimers();
    let checkCalls = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => {
          checkCalls += 1;
          throw new Error("offline");
        },
        prepareRelaunch: async () => {},
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
      },
      { startupDelayMs: 10, intervalMs: 1_000, retryDelaysMs: [20] },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    await timers.advanceBy(10);
    expect(checkCalls).toBe(1);
    controller.setAutoChecksEnabled(false);
    await timers.advanceBy(100);
    expect(checkCalls).toBe(1);

    controller.setAutoChecksEnabled(true);
    await timers.advanceBy(10);
    expect(checkCalls).toBe(2);
    controller.dispose();
    await timers.advanceBy(100);
    expect(checkCalls).toBe(2);
  });

  test("coalesces startup, periodic, and manual checks into one flight", async () => {
    const timers = new FakeTimers();
    const pending = deferred<UpdateCandidate | null>();
    let checkCalls = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: () => {
          checkCalls += 1;
          return pending.promise;
        },
        prepareRelaunch: async () => {},
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
      },
      { startupDelayMs: 10, intervalMs: 20 },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    const first = controller.checkForUpdates("manual");
    const second = controller.checkForUpdates("manual");
    expect(first).toBe(second);
    await timers.advanceBy(20);

    expect(checkCalls).toBe(1);
    expect(controller.getSnapshot().phase).toBe("checking");

    pending.resolve({
      version: "0.8.0",
      download: async () => {},
      install: async () => {},
    });
    await first;
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      version: "0.8.0",
      canInstall: true,
    });
    controller.dispose();
  });

  test("reports download progress, then marks ready and relaunches", async () => {
    const installDone = deferred<void>();
    const relaunchDone = deferred<void>();
    const restartOrder: string[] = [];
    const download = async (
      onEvent?: (event: UpdateDownloadEvent) => void,
    ) => {
      restartOrder.push("download");
      onEvent?.({ event: "Started", data: { contentLength: 100 } });
      onEvent?.({ event: "Progress", data: { chunkLength: 35 } });
      onEvent?.({ event: "Finished" });
    };
    const install = async () => {
      restartOrder.push("install");
      await installDone.promise;
    };
    const controller = createUpdateController({
      supported: true,
      check: async () => ({ version: "0.8.0", download, install }),
      prepareRelaunch: async () => {
        restartOrder.push("stop-node");
      },
      beginHandoff: async () => {
        restartOrder.push("handoff");
      },
      abortRelaunch: async () => {
        restartOrder.push("abort");
      },
      relaunch: () => {
        restartOrder.push("relaunch");
        return relaunchDone.promise;
      },
    });

    await controller.checkForUpdates("manual");
    const installing = controller.installAvailableUpdate();
    await Promise.resolve();
    expect(controller.getSnapshot()).toMatchObject({
      phase: "downloading",
      downloadedBytes: 35,
      contentLength: 100,
      restartRequired: false,
    });

    installDone.resolve();
    while (!restartOrder.includes("relaunch")) await Promise.resolve();
    expect(restartOrder).toEqual([
      "download",
      "stop-node",
      "handoff",
      "install",
      "relaunch",
    ]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "ready",
      version: "0.8.0",
      restartRequired: true,
      error: null,
    });

    relaunchDone.resolve();
    await installing;
    controller.dispose();
  });

  test("a failed verified download leaves the running node untouched", async () => {
    let relaunchCalls = 0;
    let installCalls = 0;
    const order: string[] = [];
    const controller = createUpdateController({
      supported: true,
      check: async () => ({
        version: "0.8.0",
        download: async () => {
          order.push("download");
          throw new Error("signature verification failed");
        },
        install: async () => {
          installCalls += 1;
        },
      }),
      prepareRelaunch: async () => {
        order.push("prepare");
      },
      beginHandoff: async () => {
        order.push("handoff");
      },
      abortRelaunch: async () => {
        order.push("abort");
      },
      relaunch: async () => {
        relaunchCalls += 1;
      },
    });

    await controller.checkForUpdates("manual");
    await controller.installAvailableUpdate();

    expect(controller.getSnapshot()).toMatchObject({
      phase: "error",
      restartRequired: false,
      canInstall: true,
      error: "signature verification failed",
    });
    expect(controller.getSnapshot().message).toContain("was not installed");
    expect(installCalls).toBe(0);
    expect(relaunchCalls).toBe(0);
    expect(order).toEqual(["download"]);
    controller.dispose();
  });

  test("a cancelled download does not enter the native update fence", async () => {
    const order: string[] = [];
    const controller = createUpdateController({
      supported: true,
      check: async () => ({
        version: "0.8.0",
        download: async () => {
          order.push("download-cancelled");
          throw new Error("download cancelled");
        },
        install: async () => {
          order.push("install");
        },
      }),
      prepareRelaunch: async () => {
        order.push("prepare-stop");
      },
      beginHandoff: async () => {
        order.push("handoff");
      },
      abortRelaunch: async () => {
        order.push("abort-resume");
      },
      relaunch: async () => {
        order.push("app-relaunch");
      },
    });

    await controller.checkForUpdates("manual");
    const snapshot = await controller.installAvailableUpdate();
    expect(order).toEqual(["download-cancelled"]);
    expect(snapshot).toMatchObject({
      phase: "error",
      restartRequired: false,
      error: "download cancelled",
    });
    controller.dispose();
  });

  test("does not install when the node cannot be safely prepared after download", async () => {
    let installCalls = 0;
    let downloadCalls = 0;
    let prepareCalls = 0;
    const order: string[] = [];
    const controller = createUpdateController({
      supported: true,
      check: async () => ({
        version: "0.8.0",
        download: async () => {
          downloadCalls += 1;
          order.push("download");
        },
        install: async () => {
          installCalls += 1;
          order.push("install");
        },
      }),
      prepareRelaunch: async () => {
        prepareCalls += 1;
        order.push("prepare");
        if (prepareCalls === 1) throw new Error("node has pending work");
      },
      beginHandoff: async () => {
        order.push("handoff");
      },
      abortRelaunch: async () => {
        order.push("abort");
      },
      relaunch: async () => {},
    });

    await controller.checkForUpdates("manual");
    await controller.installAvailableUpdate();

    expect(downloadCalls).toBe(1);
    expect(installCalls).toBe(0);
    expect(order).toEqual(["download", "prepare"]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "error",
      restartRequired: false,
      canInstall: true,
    });
    expect(controller.getSnapshot().error).toContain("pending work");

    const retry = await controller.installAvailableUpdate();
    expect(retry.phase).toBe("ready");
    expect(downloadCalls).toBe(1);
    expect(installCalls).toBe(1);
    expect(order).toEqual([
      "download",
      "prepare",
      "prepare",
      "handoff",
      "install",
    ]);
    controller.dispose();
  });

  test("keeps the native fence after install invocation rejects", async () => {
    const order: string[] = [];
    const controller = createUpdateController({
      supported: true,
      check: async () => ({
        version: "0.8.0",
        download: async () => {
          order.push("download");
        },
        install: async () => {
          order.push("install");
          throw new Error("bundle rename failed after mutation began");
        },
      }),
      prepareRelaunch: async () => {
        order.push("prepare");
      },
      beginHandoff: async () => {
        order.push("handoff");
      },
      abortRelaunch: async () => {
        order.push("abort");
        throw new Error("install rejection must never abort the fence");
      },
      relaunch: async () => {
        order.push("relaunch");
      },
    });

    await controller.checkForUpdates("manual");
    await controller.installAvailableUpdate();

    expect(order).toEqual(["download", "prepare", "handoff", "install"]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "ready",
      restartRequired: true,
      canInstall: false,
      error: "bundle rename failed after mutation began",
    });
    expect(controller.getSnapshot().message).toContain("node remains safely stopped");
    controller.dispose();
  });

  test("never aborts or resumes after native updater handoff begins", async () => {
    const order: string[] = [];
    const controller = createUpdateController({
      supported: true,
      check: async () => ({
        version: "0.8.0",
        download: async () => {
          order.push("download");
        },
        install: async () => {
          order.push("install");
        },
      }),
      prepareRelaunch: async () => {
        order.push("prepare");
      },
      beginHandoff: async () => {
        order.push("handoff");
        throw new Error("updater IPC disconnected after native commit");
      },
      abortRelaunch: async () => {
        order.push("abort-resume");
      },
      relaunch: async () => {
        order.push("relaunch");
      },
    });

    await controller.checkForUpdates("manual");
    await controller.installAvailableUpdate();

    expect(order).toEqual(["download", "prepare", "handoff"]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "ready",
      restartRequired: true,
      canInstall: false,
      error: "updater IPC disconnected after native commit",
    });
    controller.dispose();
  });

  test("downloads the update but blocks install when the old node cannot be stopped safely", async () => {
    let downloadCalls = 0;
    let installCalls = 0;
    let relaunchCalls = 0;
    const controller = createUpdateController({
      supported: true,
      check: async () => ({
        version: "0.8.0",
        download: async () => {
          downloadCalls += 1;
        },
        install: async () => {
          installCalls += 1;
        },
      }),
      prepareRelaunch: async () => {
        throw new Error("old arc-node pid 4312 did not exit");
      },
      beginHandoff: async () => {
        throw new Error("handoff must not run when prepare failed");
      },
      abortRelaunch: async () => {
        throw new Error("abort must not run when prepare failed");
      },
      relaunch: async () => {
        relaunchCalls += 1;
      },
    });

    await controller.checkForUpdates("manual");
    await controller.installAvailableUpdate();

    expect(downloadCalls).toBe(1);
    expect(installCalls).toBe(0);
    expect(relaunchCalls).toBe(0);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "error",
      version: "0.8.0",
      restartRequired: false,
      canInstall: true,
      error: "old arc-node pid 4312 did not exit",
    });
    expect(controller.getSnapshot().message).toContain("was not installed");
    controller.dispose();
  });

  test("native Linux packages can check but cannot invoke in-app install", async () => {
    let installCalls = 0;
    const instructions =
      "Install the new .deb or .rpm with the same package manager used for ARC.";
    const controller = createUpdateController({
      supported: true,
      check: async () => ({
        version: "0.8.23",
        canInstall: false,
        installInstructions: instructions,
        download: async () => {
          installCalls += 1;
        },
        install: async () => {
          installCalls += 1;
        },
      }),
      prepareRelaunch: async () => {},
      beginHandoff: async () => {},
      abortRelaunch: async () => {},
      relaunch: async () => {},
    });

    await controller.checkForUpdates("manual");
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      version: "0.8.23",
      canInstall: false,
      message: instructions,
    });

    await controller.installAvailableUpdate();
    expect(installCalls).toBe(0);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      canInstall: false,
      message: instructions,
    });
    controller.dispose();
  });

  test("keeps an installed update ready if relaunch fails", async () => {
    let checkCalls = 0;
    const controller = createUpdateController({
      supported: true,
      check: async () => {
        checkCalls += 1;
        return {
          version: "0.8.0",
          download: async () => {},
          install: async () => {},
        };
      },
      prepareRelaunch: async () => {},
      beginHandoff: async () => {},
      abortRelaunch: async () => {
        throw new Error("successful install must keep its fence");
      },
      relaunch: async () => {
        throw new Error("restart denied");
      },
    });

    await controller.checkForUpdates("manual");
    await controller.installAvailableUpdate();

    expect(controller.getSnapshot()).toMatchObject({
      phase: "ready",
      restartRequired: true,
      canInstall: false,
      error: "restart denied",
    });
    expect(controller.getSnapshot().message).toContain("installed");

    await controller.checkForUpdates("automatic");
    expect(checkCalls).toBe(1);
    expect(controller.getSnapshot().phase).toBe("ready");
    controller.dispose();
  });

  test("does not overlap install with a candidate-replacing check", async () => {
    let oldInstalls = 0;
    let newInstalls = 0;
    let prepareCalls = 0;
    let handoffCalls = 0;
    const nextCheck = deferred<UpdateCandidate | null>();
    let checkCalls = 0;
    const controller = createUpdateController({
      supported: true,
      check: async () => {
        checkCalls += 1;
        if (checkCalls === 1) {
          return {
            version: "0.8.0",
            download: async () => {},
            install: async () => {
              oldInstalls += 1;
            },
          };
        }
        return nextCheck.promise;
      },
      prepareRelaunch: async () => {
        prepareCalls += 1;
      },
      beginHandoff: async () => {
        handoffCalls += 1;
      },
      abortRelaunch: async () => {},
      relaunch: async () => {},
    });

    await controller.checkForUpdates("manual");
    const checking = controller.checkForUpdates("manual");
    const staleInstall = controller.installAvailableUpdate();
    expect(staleInstall).toBe(checking);
    expect(oldInstalls).toBe(0);

    nextCheck.resolve({
      version: "0.7.13",
      download: async () => {},
      install: async () => {
        newInstalls += 1;
      },
    });
    await checking;
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      version: "0.7.13",
    });
    expect(oldInstalls).toBe(0);

    const firstInstall = controller.installAvailableUpdate();
    const secondInstall = controller.installAvailableUpdate();
    expect(firstInstall).toBe(secondInstall);
    await firstInstall;
    expect(newInstalls).toBe(1);
    expect(prepareCalls).toBe(1);
    expect(handoffCalls).toBe(1);
    controller.dispose();
  });

  test("does not schedule or call the updater outside the native shell", async () => {
    const timers = new FakeTimers();
    let checkCalls = 0;
    const controller = createUpdateController(
      {
        supported: false,
        check: async () => {
          checkCalls += 1;
          return null;
        },
        prepareRelaunch: async () => {},
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
      },
      { startupDelayMs: 1, intervalMs: 2 },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    await timers.advanceBy(100);
    expect(checkCalls).toBe(0);

    await controller.checkForUpdates("manual");
    expect(checkCalls).toBe(0);
    expect(controller.getSnapshot().phase).toBe("unsupported");
    controller.dispose();
  });

  const RECONCILING =
    "managed-node startup reconciliation is still in progress: ARC is checking how the previous node session ended before it starts or updates arc-node. Try again in a few seconds.";

  test("a manual install waits for the last session to close, then installs", async () => {
    const timers = new FakeTimers();
    const order: string[] = [];
    let prepareAttempts = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => ({
          version: "0.8.23",
          download: async () => {
            order.push("download");
          },
          install: async () => {
            order.push("install");
          },
        }),
        prepareRelaunch: async () => {
          order.push("prepare");
          prepareAttempts += 1;
          if (prepareAttempts === 1) throw new Error(RECONCILING);
        },
        beginHandoff: async () => {
          order.push("handoff");
        },
        abortRelaunch: async () => {
          order.push("abort");
        },
        relaunch: async () => {
          order.push("relaunch");
        },
      },
      { startupDelayMs: 10, intervalMs: 1_000 },
      timers,
    );

    await controller.checkForUpdates("manual");
    await controller.installAvailableUpdate();
    expect(order).toEqual(["download", "prepare"]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      version: "0.8.23",
      error: null,
      canInstall: true,
      restartRequired: false,
    });
    expect(controller.getSnapshot().message).toContain("last session");

    await timers.advanceBy(UPDATE_RECONCILIATION_RETRY_MS - 1);
    expect(order).toEqual(["download", "prepare"]);
    await timers.advanceBy(1);
    await timers.advanceBy(0);
    expect(order).toEqual([
      "download",
      "prepare",
      "prepare",
      "handoff",
      "install",
      "relaunch",
    ]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "ready",
      restartRequired: true,
    });
    controller.dispose();
  });

  test("an automatic install waits for the last session instead of giving up", async () => {
    const timers = new FakeTimers();
    const order: string[] = [];
    let prepareAttempts = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => ({
          version: "0.8.23",
          download: async () => {
            order.push("download");
          },
          install: async () => {
            order.push("install");
          },
        }),
        tryPrepareAutoUpdateRelaunch: async () => {
          order.push("try-prepare");
          prepareAttempts += 1;
          if (prepareAttempts === 1) throw new Error(RECONCILING);
          return { status: "prepared" };
        },
        prepareRelaunch: async () => {
          order.push("manual-prepare");
        },
        beginHandoff: async () => {
          order.push("handoff");
        },
        abortRelaunch: async () => {
          order.push("abort");
        },
        relaunch: async () => {
          order.push("relaunch");
        },
      },
      { startupDelayMs: 10, intervalMs: 24 * 60 * 60 * 1_000, retryDelaysMs: [] },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    controller.setAutoInstallEnabled(true);
    await timers.advanceBy(10);
    expect(order).toEqual(["download", "try-prepare", "abort"]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      error: null,
      canInstall: true,
      restartRequired: false,
    });
    expect(controller.getSnapshot().message).toContain("last session");

    await timers.advanceBy(UPDATE_RECONCILIATION_RETRY_MS);
    await timers.advanceBy(0);
    expect(order).toEqual([
      "download",
      "try-prepare",
      "abort",
      "try-prepare",
      "handoff",
      "install",
      "relaunch",
    ]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "ready",
      restartRequired: true,
    });
    controller.dispose();
  });

  test("turning unattended installs off ends an automatic wait for the last session", async () => {
    const timers = new FakeTimers();
    const order: string[] = [];
    const abort = deferred<void>();
    let prepareAttempts = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => ({
          version: "0.8.23",
          download: async () => {
            order.push("download");
          },
          install: async () => {
            order.push("install");
          },
        }),
        tryPrepareAutoUpdateRelaunch: async () => {
          order.push("try-prepare");
          prepareAttempts += 1;
          throw new Error(RECONCILING);
        },
        prepareRelaunch: async () => {
          order.push("manual-prepare");
        },
        beginHandoff: async () => {},
        abortRelaunch: () => {
          order.push("abort");
          return abort.promise;
        },
        relaunch: async () => {},
      },
      { startupDelayMs: 10, intervalMs: 24 * 60 * 60 * 1_000, retryDelaysMs: [] },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    controller.setAutoInstallEnabled(true);
    await timers.advanceBy(10);
    expect(order).toEqual(["download", "try-prepare", "abort"]);
    expect(controller.getSnapshot().message).toContain("last session");
    abort.resolve();
    await timers.advanceBy(0);
    expect(controller.getSnapshot().message).toContain("last session");

    controller.setAutoInstallEnabled(false);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      message: "Version 0.8.23 is available.",
      canInstall: true,
    });
    await timers.advanceBy(UPDATE_RECONCILIATION_RETRY_MS * 2);
    expect(prepareAttempts).toBe(1);
    controller.dispose();
  });

  test("an Install press during an automatic wait survives unattended installs turning off", async () => {
    const timers = new FakeTimers();
    const order: string[] = [];
    let manualAttempts = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => ({
          version: "0.8.23",
          download: async () => {
            order.push("download");
          },
          install: async () => {
            order.push("install");
          },
        }),
        tryPrepareAutoUpdateRelaunch: async () => {
          order.push("try-prepare");
          throw new Error(RECONCILING);
        },
        prepareRelaunch: async () => {
          order.push("prepare");
          manualAttempts += 1;
          if (manualAttempts === 1) throw new Error(RECONCILING);
        },
        beginHandoff: async () => {
          order.push("handoff");
        },
        abortRelaunch: async () => {
          order.push("abort");
        },
        relaunch: async () => {
          order.push("relaunch");
        },
      },
      { startupDelayMs: 10, intervalMs: 24 * 60 * 60 * 1_000, retryDelaysMs: [] },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    controller.setAutoInstallEnabled(true);
    await timers.advanceBy(10);
    await timers.advanceBy(0);
    expect(order).toEqual(["download", "try-prepare", "abort"]);

    await controller.installAvailableUpdate();
    expect(order).toEqual(["download", "try-prepare", "abort", "prepare"]);
    controller.setAutoInstallEnabled(false);
    expect(controller.getSnapshot().message).toContain("last session");

    await timers.advanceBy(UPDATE_RECONCILIATION_RETRY_MS);
    await timers.advanceBy(0);
    expect(order).toEqual([
      "download",
      "try-prepare",
      "abort",
      "prepare",
      "prepare",
      "handoff",
      "install",
      "relaunch",
    ]);
    controller.dispose();
  });

  test("a newer release or dispose ends a wait for the last session", async () => {
    const timers = new FakeTimers();
    let checks = 0;
    let prepareAttempts = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => {
          checks += 1;
          return {
            version: checks === 1 ? "0.8.14" : "0.8.15",
            download: async () => {},
            install: async () => {},
          };
        },
        prepareRelaunch: async () => {
          prepareAttempts += 1;
          throw new Error(RECONCILING);
        },
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
      },
      { startupDelayMs: 10, intervalMs: 24 * 60 * 60 * 1_000 },
      timers,
    );

    await controller.checkForUpdates("manual");
    await controller.installAvailableUpdate();
    expect(prepareAttempts).toBe(1);

    await controller.checkForUpdates("manual");
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      version: "0.8.15",
    });
    await timers.advanceBy(UPDATE_RECONCILIATION_RETRY_MS * 2);
    expect(prepareAttempts).toBe(1);

    await controller.installAvailableUpdate();
    expect(prepareAttempts).toBe(2);
    controller.dispose();
    await timers.advanceBy(UPDATE_RECONCILIATION_RETRY_MS * 2);
    expect(prepareAttempts).toBe(2);
  });

  test("a legacy refusal points at the manual install, not at Settings", async () => {
    const timers = new FakeTimers();
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => ({
          version: "0.8.23",
          download: async () => {},
          install: async () => {},
        }),
        tryPrepareAutoUpdateRelaunch: async () => {
          throw new Error(
            "automatic update preparation requires a safe abort: legacy desktop node reconciliation is unresolved; refusing the update boundary: legacy-data migration preflight failed",
          );
        },
        prepareRelaunch: async () => {},
        beginHandoff: async () => {},
        abortRelaunch: async () => {},
        relaunch: async () => {},
      },
      { startupDelayMs: 10, intervalMs: 24 * 60 * 60 * 1_000, retryDelaysMs: [] },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    controller.setAutoInstallEnabled(true);
    await timers.advanceBy(10);
    await timers.advanceBy(0);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      message: PLAIN_COPY.updates.manualInstallOnly("0.8.23"),
    });
    controller.dispose();
  });

  test("a refusal from native preparation is still reported as an error", async () => {
    const controller = createUpdateController({
      supported: true,
      check: async () => ({
        version: "0.8.23",
        download: async () => {},
        install: async () => {},
      }),
      prepareRelaunch: async () => {
        throw new Error(
          "could not establish the native update lifecycle fence: a prior desktop update lifecycle transaction is unresolved",
        );
      },
      beginHandoff: async () => {},
      abortRelaunch: async () => {},
      relaunch: async () => {},
    });

    await controller.checkForUpdates("manual");
    await controller.installAvailableUpdate();
    expect(controller.getSnapshot()).toMatchObject({
      phase: "error",
      canInstall: true,
    });
    expect(controller.getSnapshot().message).toContain("Update was not installed");
    controller.dispose();
  });

  test("a manual install from where ARC can't update itself stays available and stops nothing", async () => {
    const order: string[] = [];
    const controller = createUpdateController({
      supported: true,
      check: async () => ({
        version: "0.8.16",
        download: async () => {
          order.push("download");
        },
        install: async () => {
          order.push("install");
        },
      }),
      prepareRelaunch: async () => {
        order.push("prepare");
        throw new Error(NATIVE_WRONG_LOCATION);
      },
      beginHandoff: async () => {
        order.push("handoff");
      },
      abortRelaunch: async () => {
        order.push("abort");
      },
      relaunch: async () => {
        order.push("relaunch");
      },
    });

    await controller.checkForUpdates("manual");
    await controller.installAvailableUpdate();
    expect(order).toEqual(["download", "prepare"]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      version: "0.8.16",
      message: PLAIN_COPY.updates.wrongLocation,
      error: null,
      canInstall: true,
      restartRequired: false,
    });

    await controller.installAvailableUpdate();
    expect(order).toEqual(["download", "prepare", "prepare"]);
    expect(controller.getSnapshot().message).toBe(PLAIN_COPY.updates.wrongLocation);
    controller.dispose();
  });

  test("an automatic install from where ARC can't update itself is deferred with the same sentence", async () => {
    const timers = new FakeTimers();
    const order: string[] = [];
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => ({
          version: "0.8.16",
          download: async () => {
            order.push("download");
          },
          install: async () => {
            order.push("install");
          },
        }),
        tryPrepareAutoUpdateRelaunch: async () => {
          order.push("try-prepare");
          throw new Error(NATIVE_WRONG_LOCATION);
        },
        prepareRelaunch: async () => {
          order.push("manual-prepare");
        },
        beginHandoff: async () => {
          order.push("handoff");
        },
        abortRelaunch: async () => {
          order.push("abort");
        },
        relaunch: async () => {},
      },
      { startupDelayMs: 10, intervalMs: 24 * 60 * 60 * 1_000, retryDelaysMs: [60_000] },
      timers,
    );

    controller.setAutoChecksEnabled(true);
    controller.setAutoInstallEnabled(true);
    await timers.advanceBy(10);
    await timers.advanceBy(0);
    expect(order).toEqual(["download", "try-prepare", "abort"]);
    expect(controller.getSnapshot()).toMatchObject({
      phase: "available",
      version: "0.8.16",
      message: PLAIN_COPY.updates.wrongLocation,
      error: null,
      canInstall: true,
      restartRequired: false,
    });

    await timers.advanceBy(60_000 * 2);
    expect(order).toEqual(["download", "try-prepare", "abort"]);
    controller.dispose();
  });

  test("the location refusal it recognises is the sentence update_location.rs writes", () => {
    const source = fs.readFileSync(
      path.resolve(__dirnameHere, "..", "src-tauri", "src", "update_location.rs"),
      "utf8",
    );
    expect(source).toContain(`"${NATIVE_WRONG_LOCATION}"`);
    expect(PLAIN_COPY.updates.wrongLocation).toMatch(/^ARC can't update itself/);
    expect(PLAIN_COPY.updates.wrongLocation).toContain("Quit ARC");
    expect(PLAIN_COPY.updates.wrongLocation).toContain("Applications folder");
  });
});


test.describe("UpdateDeadline", () => {
  test("matches a reference model through delayed, repeated and cancelled callbacks", () => {
    for (let seed = 1; seed <= 128; seed += 1) {
      let random = seed;
      const next = () => { random = (Math.imul(random, 1664525) + 1013904223) >>> 0; return random; };
      let now = 0;
      let nextId = 0;
      let active: { id: number; dueAt: number } | null = null;
      const delivered: number[] = [];
      const expected: number[] = [];
      const callbacks: Array<{ id: number; callback: () => void }> = [];
      const deadline = new UpdateDeadline({
        setTimeout: (callback) => {
          callbacks.push({ id: nextId, callback });
          return nextId;
        },
        clearTimeout: () => {},
      }, () => now);
      for (let step = 0; step < 64; step += 1) {
        const action = next() % 5;
        if (action === 0 || callbacks.length === 0) {
          const id = ++nextId;
          active = { id, dueAt: now + 3 };
          deadline.schedule(() => delivered.push(id), 3);
        } else if (action === 1) {
          active = null;
          deadline.clear();
        } else if (action === 2) {
          now += 4;
          if (active && active.dueAt <= now) {
            expected.push(active.id);
            active = null;
          }
          deadline.runOverdue(now);
        } else {
          const captured = callbacks[next() % callbacks.length];
          if (active?.id === captured.id) {
            expected.push(active.id);
            active = null;
          }
          captured.callback();
        }
        expect(delivered, `seed=${seed}, step=${step}`).toEqual(expected);
        expect(deadline.pending).toBe(active !== null);
      }
    }
  });

  test("a callback may schedule its successor without a stale delivery clearing it", () => {
    let now = 0;
    const callbacks: Array<() => void> = [];
    const delivered: number[] = [];
    const deadline = new UpdateDeadline({
      setTimeout: (callback) => { callbacks.push(callback); return callbacks.length; },
      clearTimeout: () => {},
    }, () => now);
    deadline.schedule(() => {
      delivered.push(1);
      deadline.schedule(() => delivered.push(2), 5);
    }, 5);
    now = 5;
    deadline.runOverdue(now);
    callbacks[0]();
    expect(delivered).toEqual([1]);
    expect(deadline.pending).toBe(true);
    now = 10;
    deadline.runOverdue(now);
    callbacks[1]();
    expect(delivered).toEqual([1, 2]);
    expect(deadline.pending).toBe(false);
  });

  test("controller scheduling stays inside the deadline wrapper", () => {
    const source = fs.readFileSync(path.resolve(__dirnameHere, "../src/lib/update-controller.ts"), "utf8");
    const controller = source.slice(source.indexOf("export class UpdateController"));
    expect(controller, "Use UpdateDeadline so cancellation fences and native ticks cover every timer.").not.toMatch(/\.(?:setTimeout|setInterval|clearTimeout|clearInterval)\s*\(/);
  });
});
