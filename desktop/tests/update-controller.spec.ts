import { expect, test } from "@playwright/test";
import {
  createUpdateController,
  type UpdateCandidate,
  type UpdateDownloadEvent,
  type UpdateTimers,
} from "../src/lib/update-controller";

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
    // The controller and async runtime mock each add a microtask boundary.
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  }
}

test.describe("UpdateController", () => {
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
            version: "0.8.1",
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
      version: "0.8.1",
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
          version: "0.8.1",
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
        retryDelaysMs: [20],
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

  test("turning auto install off during download prevents native preparation", async () => {
    const timers = new FakeTimers();
    const download = deferred<void>();
    let prepareAttempts = 0;
    let installCalls = 0;
    const controller = createUpdateController(
      {
        supported: true,
        check: async () => ({
          version: "0.8.1",
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
          version: "0.8.1",
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
          version: "0.8.1",
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
          version: "0.8.1",
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
        version: "0.8.1",
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
      version: "0.8.1",
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
});
