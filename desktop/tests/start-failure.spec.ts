import { expect, test } from "@playwright/test";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { PLAIN_COPY } from "../src/lib/plain-copy";
import {
  classifyStartFailure,
  cleanFailureDetails,
  describeStartFailure,
  isStartupReconciliationInProgress,
  NATIVE_RECONCILING,
  NATIVE_REFUSAL_ADVICE,
  NATIVE_REFUSAL_PREFIX,
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
    for (const jargon of ["migration", "reconciliation", "WAL", "legacy directory"]) {
      expect(`${view.title} ${view.body}`).not.toContain(jargon);
    }
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

  test("an unrecognised failure still gets a plain sentence and an action", () => {
    const view = describeStartFailure("something nobody anticipated", {
      updateAvailable: false,
    });
    expect(view.kind).toBe("unknown");
    expect(view.title).toBe(PLAIN_COPY.startFailure.unknown.title);
    expect(view.action).toBe("restart-arc");
    expect(view.details).toBe("something nobody anticipated");
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
  });
});
