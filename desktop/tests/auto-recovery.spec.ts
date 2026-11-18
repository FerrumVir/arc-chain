import { expect, test } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { CONNECTING_LONG_SECS } from "../src/lib/connection";
import { describeNodeStrip, type NodeStripInput } from "../src/lib/node-strip";
import { PLAIN_COPY } from "../src/lib/plain-copy";

const here = path.dirname(fileURLToPath(import.meta.url));

const base: NodeStripInput = {
  status: { running: false, pid: null, lastError: "arc-node exited unexpectedly" },
  startBlock: null, startError: null, modelProgress: null,
  update: { phase: "idle", canInstall: false, message: "", error: null, restartRequired: false },
};

test("Home explains a slow crash retry and keeps Start again", () => {
  const view = describeNodeStrip({ ...base, status: { ...base.status!, crashRetryInSecs: 600 } });
  expect(view.sentence).toBe("Your node stopped unexpectedly; ARC will try again in 10 minutes.");
  expect(view.action).toBe("start-again");
  expect(describeNodeStrip({ ...base, status: { ...base.status!, crashRetryInSecs: 1 } }).sentence)
    .toBe("Your node stopped unexpectedly; ARC will try again in 1 minute.");
});

test("no pending retry makes no promise to restart", () => {
  expect(describeNodeStrip(base).sentence).toBe("Your node stopped unexpectedly.");
  expect(describeNodeStrip(base).action).toBe("start-again");
});

for (const [wait, sentence] of [
  [{ reason: "onBattery" }, "The AI model download is waiting; plug this computer in and ARC will try again."],
  [{ reason: "diskFull", neededBytes: 2 * 1024 ** 3 }, "The AI model download needs 2.00 GB of free space; ARC will check again automatically."],
  [{ reason: "failed" }, "The last AI model download failed; ARC will try again automatically."],
  [{ reason: "meteredNetwork" }, "The AI model download is waiting for a connection without a data limit; ARC will check again automatically."],
] as const) {
  test(`Home explains model wait: ${wait.reason}`, () => {
    const view = describeNodeStrip({ ...base, status: {
      running: true, pid: 123, lastError: null, health: "lite", modelDownloadWait: wait,
    } });
    expect(view.sentence).toBe(sentence);
    expect(view.action).toBeNull();
    expect(view.tone).toBe("idle");
  });
}

test("active model progress and crash recovery take precedence over a stale wait", () => {
  expect(describeNodeStrip({ ...base, status: {
    ...base.status!, modelDownloadWait: { reason: "failed" }, crashRetryInSecs: 600,
  } }).sentence).toContain("ARC will try again in 10 minutes");
  expect(describeNodeStrip({ ...base, status: {
    running: true, pid: 123, lastError: null, modelDownloadWait: { reason: "failed" },
  }, modelProgress: { tier: "standard", downloadedBytes: 1, totalBytes: 100, done: false } }).sentence)
    .toContain("downloading the AI model");
});

test("dismissing crash details does not hide a pending automatic retry", () => {
  const view = describeNodeStrip({ ...base, status: {
    running: false, pid: null, lastError: null, crashRetryInSecs: 1200,
  } });
  expect(view.sentence).toBe("Your node stopped unexpectedly; ARC will try again in 20 minutes.");
  expect(view.action).toBe("start-again");
});

// Review finding 1: a slow crash wait must not hide or block a ready update.
test("a ready update during a slow crash wait offers Update, and Update installs", () => {
  const view = describeNodeStrip({
    ...base,
    status: { ...base.status!, crashRetryInSecs: 1800 },
    update: { phase: "available", canInstall: true, message: "", error: null, restartRequired: false },
  });
  expect(view.sentence).toBe(PLAIN_COPY.nodeStrip.updateAvailable);
  expect(view.action).toBe("update-arc");
  // The button goes straight to the updater, whose native Prepare now takes
  // over the slow wait (Rust: home_update_prepares_during_a_slow_crash_wait).
  const strip = fs.readFileSync(path.join(here, "../src/components/NodeStatusStrip.tsx"), "utf8");
  expect(strip).toMatch(/if \(action === "update-arc"\) \{\s*void appUpdater\.installAvailableUpdate\(\);/);
});

// Review finding 2: a model wait only replaces the "all is well" sentences.
test.describe("a model wait never masks the node's own state", () => {
  const wait = { reason: "failed" } as const;
  const copy = PLAIN_COPY.nodeStrip;

  test("an off node says it is off", () => {
    const view = describeNodeStrip({ ...base, status: {
      running: false, pid: null, lastError: null, modelDownloadWait: wait,
    } });
    expect(view.sentence).toBe(copy.off);
    expect(view.action).toBe("start-node");
  });

  test("an off node with no network says to check the connection", () => {
    const view = describeNodeStrip({ ...base, status: {
      running: false, pid: null, lastError: "No node is answering on 127.0.0.1:9090", modelDownloadWait: wait,
    } });
    expect(view.sentence).toBe(copy.offUnreachable);
    expect(view.action).toBe("start-node");
  });

  test("a starting node says it is starting", () => {
    const view = describeNodeStrip({ ...base, status: {
      running: false, pid: 4242, lastError: null, modelDownloadWait: wait,
    } });
    expect(view.sentence).toBe(copy.starting);
    expect(view.action).toBeNull();
  });

  test("a running node still connecting, or unreachable, says so first", () => {
    const status = { running: true, pid: 4242, lastError: null, health: "syncing", modelDownloadWait: wait } as const;
    expect(describeNodeStrip({ ...base, status }).sentence).toBe(copy.runningConnecting);
    expect(describeNodeStrip({ ...base, status, connectingSeconds: CONNECTING_LONG_SECS }).sentence)
      .toBe(copy.runningUnreachable);
    expect(describeNodeStrip({ ...base, status: { ...status, health: "offline" } }).sentence)
      .toBe(copy.runningConnecting);
  });

  test("a connected or plainly running node explains the wait", () => {
    for (const status of [
      { running: true, pid: 4242, lastError: null, health: "live", modelDownloadWait: wait },
      { running: true, pid: 4242, lastError: null, coordinatorUrl: "https://coordinator.example", modelDownloadWait: wait },
      { running: true, pid: 4242, lastError: null, modelDownloadWait: wait },
    ] as const) {
      const view = describeNodeStrip({ ...base, status });
      expect(view.sentence).toBe(copy.modelWaitingFailed);
      expect(view.action).toBeNull();
    }
  });

  test("every wait sentence says ARC tries again by itself", () => {
    for (const sentence of [
      copy.modelWaitingBattery, copy.modelWaitingSpace("2.00 GB"), copy.modelWaitingFailed, copy.modelWaitingMetered,
    ]) {
      expect(sentence).toMatch(/ARC will (try|check) again/);
    }
  });
});


test("desktop ownership and closing take precedence over slow recovery and updates", () => {
  for (const error of ["ARC is already open on this computer.", "ARC is closing"]) {
    for (const source of ["startBlock", "lastError"] as const) {
      for (const updateReady of [false, true]) {
        const input: NodeStripInput = {
          ...base,
          startBlock: source === "startBlock" ? error : null,
          status: {
            running: false, pid: null,
            lastError: source === "lastError" ? error : null,
            crashRetryInSecs: 600,
            modelDownloadWait: { reason: "failed" },
          },
          update: { ...base.update, phase: updateReady ? "available" : "idle", canInstall: updateReady },
        };
        const refused = describeNodeStrip(input);
        expect(refused.action).toBe(error === "ARC is closing" ? null : "close-copy");
        expect(refused.sentence).not.toContain("try again in");
        // Once ownership is restored, the very same retry/update data is
        // visible again. Refusal must hide it without deleting recovery.
        const restored = describeNodeStrip({ ...input, startBlock: null,
          status: { ...input.status!, lastError: null } });
        expect(restored.action).toBe(updateReady ? "update-arc" : "start-again");
      }
    }
  }
});
