import { expect, test } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import { CONNECTING_LONG_SECS } from "../src/lib/connection";
import {
  describeNodeStrip,
  isUnfinishedUpdate,
  type NodeStripInput,
} from "../src/lib/node-strip";
import {
  classifyStartFailure,
  cleanFailureDetails,
  describeStartFailure,
  isStartupReconciliationInProgress,
  NATIVE_RECONCILING,
  NATIVE_REFUSAL_ADVICE,
  NATIVE_REFUSAL_PREFIX,
  NATIVE_UPDATE_UNFINISHED,
} from "../src/lib/start-failure";

// Pure logic: no page. What a desktop that would not start says, and what it
// keeps behind "Show details".

const __dirnameHere = path.dirname(fileURLToPath(import.meta.url));

// The text v0.8.12 desktops showed on "Ready to set up" (field reports,
// October 2026): a refusal recorded inside another, framing and advice twice.
const V0812_FIELD_ERROR =
  "ARC refused to start the node because chain-data migration is not safely resolved: managed-node durability recovery failed: ARC refused to start the node because chain-data migration is not safely resolved: managed-node startup reconciliation is still in progress; binary replacement and node start are temporarily blocked. Restart ARC after repairing the reported path or permissions; do not point v0.8 at the preserved legacy directory.. Restart ARC after repairing the reported path or permissions; do not point v0.8 at the preserved legacy directory.";

const RECONCILING =
  "managed-node startup reconciliation is still in progress: ARC is checking how the previous node session ended before it starts or updates arc-node. Try again in a few seconds.";

test.describe("start failure wording", () => {
  test("the v0.8.12 field error becomes one plain sentence and one action", () => {
    const view = describeStartFailure(V0812_FIELD_ERROR, { updateAvailable: false });
    expect(view.kind).toBe("blocked");
    expect(view.title).toBe(PLAIN_COPY.startFailure.blocked.title);
    expect(view.body).toBe(PLAIN_COPY.startFailure.blocked.body);
    expect(view.action).toBe("restart-arc");
    expect(view.detailsOpen).toBe(false);
    for (const jargon of ["migration", "reconciliation", "WAL", "legacy directory"]) {
      expect(`${view.title} ${view.body}`).not.toContain(jargon);
    }
  });

  test("a repair says so, keeps the native text open, and offers the next step", () => {
    // A launch refusal holds until ARC restarts, so the step after the
    // repair is a restart.
    const migration = `${NATIVE_REFUSAL_PREFIX}legacy-data migration preflight failed: refusing protocol-v3 migration through symbolic-link data directory /Users/me/.arc. ${NATIVE_REFUSAL_ADVICE}`;
    const refused = describeStartFailure(migration, { updateAvailable: true });
    expect(refused.kind).toBe("needs-repair");
    expect(refused.detailsOpen).toBe(true);
    expect(refused.action).toBe("restart-arc");
    expect(refused.body).toBe(PLAIN_COPY.startFailure.needsRepair.bodyThenRestart);
    expect(`${refused.title} ${refused.body}`).not.toMatch(/cleanly|last session/);

    // Outside a launch refusal a restart can't help; another try can, once
    // the repair is made.
    for (const raw of [
      "another ARC desktop currently owns the managed node lifecycle for /Users/me/.arc/data-v3: Resource temporarily unavailable (os error 35)",
      "managed data-directory parent /Volumes/arc must already exist; the installer or operator must create it before node start",
      "cannot durably secure desktop node data directory /Users/me/.arc/data-v3: Permission denied (os error 13)",
    ]) {
      const view = describeStartFailure(raw, { updateAvailable: true });
      expect(view.kind, raw).toBe("needs-repair");
      expect(view.action, raw).toBe("try-again");
      expect(view.body, raw).toBe(PLAIN_COPY.startFailure.needsRepair.bodyThenRetry);
      expect(view.detailsOpen, raw).toBe(true);
    }
  });

  test("the advice every refusal carries does not make it a repair", () => {
    // The advice names paths and permissions; only the reason decides.
    expect(classifyStartFailure(V0812_FIELD_ERROR)).toBe("blocked");
    expect(
      classifyStartFailure(
        `${NATIVE_REFUSAL_PREFIX}managed-node durability recovery failed: recovery node exited before its WAL barrier. ${NATIVE_REFUSAL_ADVICE}`,
      ),
    ).toBe("blocked");
    // "WAL" is a word, not the start of "wallet".
    expect(classifyStartFailure("wallet operation failed")).toBe("unknown");
    // An arc-node too old for this desktop names the WAL barrier it cannot
    // prove. That is not an unclean last session.
    const outdated = describeStartFailure(
      "arc-node at /Users/me/.arc/bin/arc-node lacks the authenticated durable desktop shutdown protocol; update the managed node before starting so Stop, Update, and rollback can prove its final WAL barrier",
      { updateAvailable: false },
    );
    expect(outdated.kind).toBe("needs-repair");
    expect(outdated.detailsOpen).toBe(true);
  });

  test("a ready update is the primary action, since it installs while blocked", () => {
    const view = describeStartFailure(V0812_FIELD_ERROR, { updateAvailable: true });
    expect(view.action).toBe("install-update");
  });

  test("details keep the native text once, without the nested framing", () => {
    const details = cleanFailureDetails(V0812_FIELD_ERROR);
    expect(details.split(NATIVE_REFUSAL_PREFIX)).toHaveLength(2);
    expect(details.split(NATIVE_REFUSAL_ADVICE)).toHaveLength(2);
    expect(details.endsWith(NATIVE_REFUSAL_ADVICE)).toBe(true);
    expect(details).not.toContain("..");
    expect(details).toContain("managed-node durability recovery failed: ");
    expect(details).toContain(
      "binary replacement and node start are temporarily blocked",
    );
  });

  test("details never rewrite anything else", () => {
    for (const raw of [
      "cannot open /Users/me/../shared/arc/model.gguf: permission denied",
      "release asset returned HTTP 404 Not Found for v0.8.14",
      NATIVE_REFUSAL_PREFIX + "legacy data directory is a symbolic link. " + NATIVE_REFUSAL_ADVICE,
    ]) {
      expect(cleanFailureDetails(raw)).toBe(raw);
    }
  });

  test("launch reconciliation is a wait, not a failure", () => {
    expect(isStartupReconciliationInProgress(RECONCILING)).toBe(true);
    // In v0.8.12 the same phrase sat inside a refusal. That one is a refusal.
    expect(isStartupReconciliationInProgress(V0812_FIELD_ERROR)).toBe(false);
    const view = describeStartFailure(RECONCILING, { updateAvailable: true });
    expect(view.kind).toBe("reconciling");
    expect(view.action).toBe("try-again");
  });

  test("download failures ask for another try", () => {
    expect(
      classifyStartFailure(
        "release asset transport failed: error sending request for url (https://github.com/…)",
      ),
    ).toBe("network");
    expect(
      classifyStartFailure(
        "release manifest returned HTTP 503 Service Unavailable for v0.8.14",
      ),
    ).toBe("network");
    expect(
      describeStartFailure("model download timed out", { updateAvailable: true }).action,
    ).toBe("try-again");
    // A missing release file is not the user's connection.
    expect(
      classifyStartFailure("release checksum manifest returned HTTP 404"),
    ).toBe("unknown");
  });

  test("an unrecognised failure keeps its text open and never offers a restart", () => {
    const view = describeStartFailure("something nobody anticipated", {
      updateAvailable: false,
    });
    expect(view.kind).toBe("unknown");
    expect(view.title).toBe(PLAIN_COPY.startFailure.unknown.title);
    expect(view.action).toBe("try-again");
    expect(view.detailsOpen).toBe(true);
    expect(view.details).toBe("something nobody anticipated");
    // A newer release may carry the fix for a failure nobody anticipated.
    expect(
      describeStartFailure("something nobody anticipated", { updateAvailable: true })
        .action,
    ).toBe("install-update");
  });

  test("the native wording it recognises is the wording the Rust side writes", () => {
    const startFence = fs.readFileSync(
      path.resolve(__dirnameHere, "..", "src-tauri", "src", "start_fence.rs"),
      "utf8",
    );
    expect(startFence).toContain(`"${NATIVE_REFUSAL_PREFIX}"`);
    expect(startFence).toContain(`"${NATIVE_REFUSAL_ADVICE}"`);
    expect(startFence).toContain(
      `"managed-node ${NATIVE_RECONCILING}: ARC is checking how the previous node session ended`,
    );
    const nodeManager = fs.readFileSync(
      path.resolve(__dirnameHere, "..", "src-tauri", "src", "node_manager.rs"),
      "utf8",
    );
    expect(nodeManager).toContain(`"${NATIVE_UPDATE_UNFINISHED}"`);
  });

  // An install that failed after its handoff keeps the update's fence until
  // ARC exits. Start and Restart used to fail against that fence as
  // "another ARC desktop currently owns the managed node lifecycle" and
  // offered another try; they are refused before it now.
  test("a Start refused while an unfinished update holds the node says to quit ARC", () => {
    for (const updateAvailable of [false, true]) {
      const view = describeStartFailure(NATIVE_UPDATE_UNFINISHED, { updateAvailable });
      expect(view).toMatchObject({
        kind: "update-unfinished",
        title: PLAIN_COPY.startFailure.updateUnfinished.title,
        body: PLAIN_COPY.startFailure.updateUnfinished.body,
        action: "quit-arc",
        detailsOpen: false,
      });
    }
    // The same sentence Home shows from the updater's own state.
    expect(PLAIN_COPY.startFailure.updateUnfinished.title).toBe(PLAIN_COPY.nodeStrip.updateFailed);
    // The text a Start got before, from the update's own lock, still reads as a repair.
    expect(
      classifyStartFailure(
        "another ARC desktop currently owns the managed node lifecycle for /Users/me/.arc/data-v3: Resource temporarily unavailable (os error 35)",
      ),
    ).toBe("needs-repair");
  });
});

test.describe("node strip on Home", () => {
  const settled = { canInstall: false, message: "", error: null, restartRequired: false };
  const idle = { ...settled, phase: "idle" } as const;
  const ready = { ...settled, phase: "available", canInstall: true } as const;
  const off = { running: false, pid: null, lastError: null };
  const running = { running: true, pid: 4242, lastError: null };
  const strip = (input: Partial<NodeStripInput>) =>
    describeNodeStrip({
      status: off,
      startBlock: null,
      startError: null,
      update: idle,
      modelProgress: null,
      ...input,
    });

  test("each node state is one sentence with at most one button", () => {
    expect(strip({ status: undefined })).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.checking,
      action: null,
    });
    expect(strip({})).toMatchObject({ sentence: PLAIN_COPY.nodeStrip.off, action: "start-node" });
    expect(strip({ status: running })).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.running,
      action: null,
      tone: "ok",
    });
    expect(strip({ status: { running: false, pid: 4242, lastError: null } })).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.starting,
      action: null,
    });
    expect(
      strip({
        status: { running: false, pid: null, lastError: "arc-node exited unexpectedly (code 1)" },
      }),
    ).toMatchObject({ sentence: PLAIN_COPY.nodeStrip.crashed, action: "start-again" });
    expect(
      strip({
        modelProgress: {
          tier: "standard",
          downloadedBytes: 1_200_000_000,
          totalBytes: 3_800_000_000,
          done: false,
        },
      }).sentence,
    ).toMatch(/^Your node is getting ready: downloading the AI model \(.+ of .+\)\.$/);
  });

  test("a ready update shows on Home, and installing it says so", () => {
    expect(strip({ status: running, update: ready })).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.updateAvailable,
      action: "update-arc",
    });
    expect(strip({ update: ready })).toMatchObject({ action: "update-arc" });
    expect(strip({ update: { ...settled, phase: "downloading" } })).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.updating,
      action: null,
    });
    const relaunching = {
      ...settled,
      phase: "ready",
      message: "Version 0.8.14 installed. Restarting ARC now…",
      restartRequired: true,
    } as const;
    expect(strip({ status: running, update: relaunching })).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.updating,
      action: null,
      tone: "busy",
    });
  });

  test("an update that couldn't finish says to quit ARC, not that it restarts by itself", () => {
    const failures = [
      {
        phase: "ready",
        message:
          "Version 0.8.14 is installed, but ARC could not safely stop the old node and relaunch. Quit ARC from the tray, then reopen it to finish updating.",
        error: "relaunch failed",
      },
      {
        phase: "ready",
        message:
          "ARC could not verify that update 0.8.14 finished installing. The node remains safely stopped. Quit ARC from the tray, then reopen it; reinstall from the verified release if ARC does not reopen.",
        error: "installer IPC disconnected",
      },
      {
        phase: "error",
        message:
          "Automatic installation did not begin, but ARC could not confirm that its node resumed. Quit and reopen ARC before continuing.",
        error: "prepare failed; resume failed: abort IPC disconnected",
      },
    ] as const;
    for (const failure of failures) {
      const update = { ...settled, ...failure, restartRequired: true };
      // It outranks the node's own state and a refused Start: only a reopen clears it.
      for (const input of [{ status: running }, {}, { startBlock: V0812_FIELD_ERROR }]) {
        const view = strip({ ...input, update });
        expect(view).toMatchObject({
          sentence: PLAIN_COPY.nodeStrip.updateFailed,
          action: "quit-arc",
          tone: "attention",
        });
        expect(view.details).toBe(`${failure.message}\n${failure.error}`);
      }
    }
    // An update that failed without needing a reopen leaves the node's state.
    const retryable = {
      ...settled,
      phase: "error",
      message: "Update was not installed: network down",
      error: "network down",
    } as const;
    expect(strip({ status: running, update: retryable })).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.running,
      action: null,
    });
    expect(PLAIN_COPY.nodeStrip.updateFailed).toMatch(/Quit ARC and open it again\.$/);
    expect(PLAIN_COPY.nodeStrip.updateFailed).not.toMatch(/by itself/);
  });

  test("a Start refused for an unfinished update offers Quit ARC, as the update's own state does", () => {
    const refused = strip({ startError: NATIVE_UPDATE_UNFINISHED });
    expect(refused).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.updateFailed,
      action: "quit-arc",
      tone: "attention",
    });
    // An update on offer does not outrank it: nothing installs until ARC reopens.
    expect(strip({ startError: NATIVE_UPDATE_UNFINISHED, update: ready }).action).toBe("quit-arc");
    // The Dashboard reads the updater's state through the same rule.
    expect(isUnfinishedUpdate({ phase: "ready", error: "installer IPC disconnected", restartRequired: true })).toBe(true);
    expect(isUnfinishedUpdate({ phase: "error", error: "resume failed", restartRequired: true })).toBe(true);
    expect(isUnfinishedUpdate({ phase: "ready", error: null, restartRequired: true })).toBe(false);
    expect(isUnfinishedUpdate({ phase: "error", error: "network down", restartRequired: false })).toBe(false);
    expect(isUnfinishedUpdate({ phase: "available", error: null, restartRequired: false })).toBe(false);
  });

  test("a blocked node takes the start-failure sentence and its fix", () => {
    const blocked = strip({ startBlock: V0812_FIELD_ERROR });
    expect(blocked).toMatchObject({
      sentence: PLAIN_COPY.startFailure.blocked.title,
      action: "restart-arc",
      tone: "attention",
    });
    expect(blocked.details?.split(NATIVE_REFUSAL_PREFIX)).toHaveLength(2);
    // The update installs while the node is blocked, so it is the fix.
    expect(strip({ startBlock: V0812_FIELD_ERROR, update: ready }).action).toBe("update-arc");
    // Launch reconciliation clears by itself; the strip polls.
    expect(strip({ startBlock: RECONCILING })).toMatchObject({
      sentence: PLAIN_COPY.startFailure.reconciling.title,
      action: null,
      tone: "busy",
    });
    // A Start pressed on the strip that fails reads the same way.
    expect(
      strip({ startError: "another ARC desktop currently owns the managed node lifecycle for /x" }),
    ).toMatchObject({ sentence: PLAIN_COPY.startFailure.needsRepair.title, action: "try-again" });
  });

  test("a running node reads as connected once a peer or a coordinator answered, and ordinary states are never red", () => {
    // A community node: no peers by design, a coordinator answering.
    const connected = strip({
      status: { ...running, health: "lite", coordinatorUrl: "https://140.82.16.112" },
    });
    expect(connected).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.runningConnected,
      action: null,
      tone: "ok",
    });
    expect(strip({ status: { ...running, health: "live" } })).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.runningConnected,
      tone: "ok",
    });
    // Nothing has answered yet: a wait, not a warning.
    expect(strip({ status: { ...running, health: "syncing" } })).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.runningConnecting,
      action: null,
      tone: "busy",
    });
    // After the same wait the Dashboard uses, Home says to check the connection: calm, nothing to press.
    expect(
      strip({ status: { ...running, health: "syncing" }, connectingSeconds: CONNECTING_LONG_SECS - 1 }),
    ).toMatchObject({ sentence: PLAIN_COPY.nodeStrip.runningConnecting, tone: "busy" });
    expect(
      strip({ status: { ...running, health: "syncing" }, connectingSeconds: CONNECTING_LONG_SECS }),
    ).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.runningUnreachable,
      action: null,
      tone: "idle",
      details: null,
    });
    // The sentences never claim the node itself reached the network through a coordinator.
    for (const sentence of [PLAIN_COPY.nodeStrip.runningConnected, PLAIN_COPY.connection.connectedCoordinator("LAX")]) {
      expect(sentence).toMatch(/ARC is connected/);
      expect(sentence).not.toMatch(/node is (?:running and )?connected/);
    }
    // Off by choice is idle, and an update on offer is information.
    expect(strip({})).toMatchObject({ tone: "idle" });
    expect(strip({ update: ready }).tone).toBe("info");
    // The status's "nothing answered" text is this computer's connection, not a failed start.
    const offline = strip({
      status: {
        running: false,
        pid: null,
        lastError:
          "No node is answering on http://127.0.0.1:9090 and every public seed is unreachable. Start your node, or check your internet connection and firewall.",
      },
    });
    expect(offline).toMatchObject({
      sentence: PLAIN_COPY.nodeStrip.offUnreachable,
      action: "start-node",
      tone: "idle",
      details: null,
    });
    for (const view of [connected, offline]) {
      expect(view.sentence).not.toMatch(/lite|syncing|degraded|0 peers|client mode/i);
    }
  });
});
