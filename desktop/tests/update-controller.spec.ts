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
      downloadAndInstall: async () => {},
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
    let relaunchCalls = 0;
    const downloadAndInstall = async (
      onEvent?: (event: UpdateDownloadEvent) => void,
    ) => {
      onEvent?.({ event: "Started", data: { contentLength: 100 } });
      onEvent?.({ event: "Progress", data: { chunkLength: 35 } });
      onEvent?.({ event: "Finished" });
      await installDone.promise;
    };
    const controller = createUpdateController({
      supported: true,
      check: async () => ({ version: "0.8.0", downloadAndInstall }),
      relaunch: () => {
        relaunchCalls += 1;
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
    while (relaunchCalls === 0) await Promise.resolve();
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

  test("never claims installation when the signed updater fails", async () => {
    let relaunchCalls = 0;
    const controller = createUpdateController({
      supported: true,
      check: async () => ({
        version: "0.8.0",
        downloadAndInstall: async () => {
          throw new Error("signature verification failed");
        },
      }),
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
    expect(relaunchCalls).toBe(0);
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
        downloadAndInstall: async () => {
          installCalls += 1;
        },
      }),
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
          downloadAndInstall: async () => {},
        };
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
    const nextCheck = deferred<UpdateCandidate | null>();
    let checkCalls = 0;
    const controller = createUpdateController({
      supported: true,
      check: async () => {
        checkCalls += 1;
        if (checkCalls === 1) {
          return {
            version: "0.8.0",
            downloadAndInstall: async () => {
              oldInstalls += 1;
            },
          };
        }
        return nextCheck.promise;
      },
      relaunch: async () => {},
    });

    await controller.checkForUpdates("manual");
    const checking = controller.checkForUpdates("manual");
    const staleInstall = controller.installAvailableUpdate();
    expect(staleInstall).toBe(checking);
    expect(oldInstalls).toBe(0);

    nextCheck.resolve({
      version: "0.7.13",
      downloadAndInstall: async () => {
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
