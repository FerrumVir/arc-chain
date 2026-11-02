import { expect, test } from "@playwright/test";
import {
  DAILY_REPORT_CAP,
  HEARTBEAT_INTERVAL_MS,
  HEARTBEAT_SCHEMA,
  MESSAGE_MAX_BYTES,
  NodeCare,
  NodeCareWatcher,
  QUEUE_MAX,
  REPORT_SCHEMA,
  SAME_REPORT_INTERVAL_MS,
  STUCK_AFTER_MS,
  heartbeatStatus,
  scrubMessage,
  type NodeCarePlatform,
  type PayloadKind,
  type ReportInput,
} from "../src/lib/node-care";

// Pure logic: no page. What Node Care sends, and when it must not.

const PLATFORM: NodeCarePlatform = {
  endpointConfigured: true,
  appVersion: "0.8.14",
  os: "macos",
  arch: "aarch64",
  homeDir: "/Users/alice",
};

function harness(options: { platform?: Partial<NodeCarePlatform>; sendOk?: boolean; stored?: string | null } = {}) {
  let now = Date.UTC(2026, 9, 22, 12, 0, 0);
  let stored: string | null = options.stored ?? null;
  const sent: { kind: PayloadKind; body: string }[] = [];
  const control = { sendOk: options.sendOk ?? true };
  const care = new NodeCare({
    loadState: async () => stored,
    saveState: async (json) => {
      stored = json;
    },
    send: async (kind, body) => {
      if (!control.sendOk) return false;
      sent.push({ kind, body });
      return true;
    },
    platform: async () => ({ ...PLATFORM, ...options.platform }),
    now: () => now,
    randomBytes: (n) => new Uint8Array(n).fill(0xab),
  });
  return {
    care,
    sent,
    control,
    stored: () => stored,
    advance: (ms: number) => {
      now += ms;
    },
  };
}

const START_FAILED: ReportInput = {
  event: "start_failed",
  stage: "start",
  errorCode: "reconciling",
  message: "Couldn't start arc-node: managed-node startup reconciliation is still in progress",
  nodeVersion: "0.8.14",
};

// The text users posted screenshots of.
const FIELD_MESSAGE =
  "Couldn't start arc-node: managed-node startup reconciliation is still in progress: ARC is checking how the previous node session ended before it starts or updates arc-node. Try again in a few seconds.";

test.describe("scrubbing", () => {
  test("the real start failure keeps its meaning", () => {
    expect(scrubMessage(FIELD_MESSAGE, "/Users/alice")).toBe(FIELD_MESSAGE);
  });

  test("home folders and user names in paths become ~", () => {
    expect(scrubMessage("cannot open /Users/alice/.arc/data-v3/wal", "/Users/alice")).toBe(
      "cannot open ~/.arc/data-v3/wal",
    );
    expect(scrubMessage("cannot open /Users/bob/.arc", null)).toBe("cannot open ~/.arc");
    expect(scrubMessage("denied: /home/carol/.local/share/network.arc.desktop")).toBe(
      "denied: ~/.local/share/network.arc.desktop",
    );
    expect(scrubMessage("C:\\Users\\Dave\\AppData\\Roaming\\arc failed")).toBe("~\\AppData\\Roaming\\arc failed");
    expect(scrubMessage("C:/Users/Dave/AppData failed")).toBe("~/AppData failed");
    expect(scrubMessage("open /srv/arc/home2/node: no such file", "/srv/arc/home2")).toBe(
      "open ~/node: no such file",
    );
    expect(scrubMessage("read-only file system: /root/.arc")).toBe("read-only file system: ~/.arc");
  });

  test("IP addresses are removed, times and Rust paths are not", () => {
    expect(scrubMessage("connect to 203.0.113.7:9090 refused")).toBe("connect to [redacted] refused");
    expect(scrubMessage("peer 198.51.100.5 dropped")).toBe("peer [redacted] dropped");
    expect(scrubMessage("bind [2001:db8:85a3::8a2e:370:7334]:9000 failed")).toBe("bind [redacted] failed");
    expect(scrubMessage("listen on fe80::1%en0 failed")).toBe("listen on [redacted] failed");
    expect(scrubMessage("addr 2001:0db8:0000:0000:0000:ff00:0042:8329 down")).toBe("addr [redacted] down");
    expect(scrubMessage("loopback ::1 refused")).toBe("loopback [redacted] refused");
    expect(scrubMessage("at 12:34:56 std::io::Error kind")).toBe("at 12:34:56 std::io::Error kind");
    expect(scrubMessage("GET https://203.0.113.7/health timed out")).toBe("GET https://[redacted]/health timed out");
  });

  test("keys, addresses, hashes and emails are removed", () => {
    const address = "0x" + "a1".repeat(32);
    expect(scrubMessage(`balance for ${address} unavailable`)).toBe("balance for [redacted] unavailable");
    expect(scrubMessage(`addr ${"0x" + "b2".repeat(20)} bad`)).toBe("addr [redacted] bad");
    expect(scrubMessage(`key ${"c3".repeat(32)} rejected`)).toBe("key [redacted] rejected");
    expect(scrubMessage("signer dGhpcyBpcyBhIHNlY3JldCBrZXkgMTIzNDU2Nzg5MA== bad")).toBe("signer [redacted] bad");
    expect(scrubMessage("mail ops@example.com about it")).toBe("mail [redacted] about it");
    expect(scrubMessage("https://user:pw@host.example/x")).toBe("https://[redacted]/x");
    // Ordinary long words survive.
    expect(scrubMessage("managed_node_startup_reconciliation_is_running")).toBe(
      "managed_node_startup_reconciliation_is_running",
    );
  });

  test("messages are capped at 2 KB", () => {
    const long = "é".repeat(5000);
    const out = scrubMessage(long);
    expect(new TextEncoder().encode(out).length).toBeLessThanOrEqual(MESSAGE_MAX_BYTES);
    expect(out.endsWith("…")).toBe(true);
  });
});

test.describe("payloads", () => {
  test("a report has exactly the published fields", async () => {
    const h = harness();
    await h.care.init();
    await h.care.markNoticeShown();
    expect(
      await h.care.report({ ...START_FAILED, message: "open /Users/alice/.arc at 192.0.2.33 failed", uptimeS: 12.7 }),
    ).toBe("sent");
    const body = JSON.parse(h.sent[0].body);
    expect(h.sent[0].kind).toBe("report");
    expect(Object.keys(body).sort()).toEqual(
      [
        "app_version",
        "arch",
        "error_code",
        "event",
        "install_id",
        "message",
        "node_version",
        "occurred_at",
        "os",
        "schema",
        "stage",
        "uptime_s",
      ].sort(),
    );
    expect(body).toEqual({
      schema: REPORT_SCHEMA,
      install_id: "ab".repeat(16),
      event: "start_failed",
      app_version: "0.8.14",
      node_version: "0.8.14",
      os: "macos",
      arch: "aarch64",
      stage: "start",
      error_code: "reconciling",
      message: "open ~/.arc at [redacted] failed",
      uptime_s: 12,
      occurred_at: "2026-10-22T12:00:00Z",
    });
    expect(h.sent[0].body).not.toContain("alice");
    expect(h.sent[0].body).not.toContain("192.0.2.33");
  });

  test("a heartbeat has exactly the published fields, once a day", async () => {
    const h = harness();
    await h.care.init();
    await h.care.markNoticeShown();
    expect(await h.care.heartbeat("earning", "0.8.14")).toBe("sent");
    expect(JSON.parse(h.sent[0].body)).toEqual({
      schema: HEARTBEAT_SCHEMA,
      install_id: "ab".repeat(16),
      app_version: "0.8.14",
      node_version: "0.8.14",
      status: "earning",
      os: "macos",
    });
    expect(await h.care.heartbeat("earning", "0.8.14")).toBe("rate-limited");
    h.advance(HEARTBEAT_INTERVAL_MS);
    expect(await h.care.heartbeat("syncing", "0.8.14")).toBe("sent");
    expect(h.sent).toHaveLength(2);
  });

  test("heartbeat status follows the node", () => {
    const base = { running: true, pid: 1, health: "live" as const, lastError: null };
    expect(heartbeatStatus(base, null)).toBe("earning");
    expect(heartbeatStatus({ ...base, health: "syncing" }, null)).toBe("syncing");
    expect(heartbeatStatus({ ...base, running: false }, null)).toBe("starting");
    expect(heartbeatStatus({ ...base, running: false, pid: null, lastError: "x" }, null)).toBe("error");
    expect(heartbeatStatus({ ...base, running: false, pid: null }, "blocked")).toBe("error");
    expect(heartbeatStatus({ ...base, running: false, pid: null }, null)).toBeNull();
  });

  test("a stored install ID is kept and can be reset", async () => {
    const h = harness({ stored: JSON.stringify({ installId: "0123456789abcdef0123456789abcdef", enabled: true }) });
    await h.care.init();
    expect(h.care.getSnapshot().installId).toBe("0123456789abcdef0123456789abcdef");
    await h.care.resetInstallId();
    expect(h.care.getSnapshot().installId).toBe("ab".repeat(16));
    expect(JSON.parse(h.stored()!).installId).toBe("ab".repeat(16));
  });
});

test.describe("the switch", () => {
  test("on by default", async () => {
    const h = harness();
    await h.care.init();
    expect(h.care.getSnapshot().enabled).toBe(true);
    expect(JSON.parse(h.stored()!).enabled).toBe(true);
  });

  test("off means nothing is sent, and the queue is dropped at once", async () => {
    const h = harness({ sendOk: false });
    await h.care.init();
    await h.care.markNoticeShown();
    expect(await h.care.report(START_FAILED)).toBe("queued");
    expect(h.care.getSnapshot().queued).toBe(1);

    await h.care.setEnabled(false);
    expect(h.care.getSnapshot().queued).toBe(0);
    expect(JSON.parse(h.stored()!).enabled).toBe(false);

    h.control.sendOk = true;
    expect(await h.care.report({ ...START_FAILED, errorCode: "blocked" })).toBe("off");
    expect(await h.care.heartbeat("earning", "0.8.14")).toBe("off");
    await h.care.flush();
    expect(h.sent).toHaveLength(0);
  });

  test("a stored off survives a restart", async () => {
    const h = harness({ stored: JSON.stringify({ enabled: false }) });
    expect(await h.care.report(START_FAILED)).toBe("off");
    expect(h.sent).toHaveLength(0);
  });

  test("nothing is sent before the first-run notice; queued reports go after it", async () => {
    const h = harness();
    expect(await h.care.report(START_FAILED)).toBe("queued");
    expect(h.sent).toHaveLength(0);
    await h.care.markNoticeShown();
    await h.care.flush();
    expect(h.sent).toHaveLength(1);
    expect(h.care.getSnapshot().queued).toBe(0);
  });

  test("a build without an endpoint sends nothing and queues nothing", async () => {
    const h = harness({ platform: { endpointConfigured: false } });
    await h.care.init();
    await h.care.markNoticeShown();
    expect(await h.care.report(START_FAILED)).toBe("not-configured");
    expect(await h.care.heartbeat("earning", "0.8.14")).toBe("not-configured");
    expect(h.sent).toHaveLength(0);
    expect(h.care.getSnapshot().queued).toBe(0);
    // "See what's sent" still shows the payload, marked not sent.
    expect(h.care.getSnapshot().recent[0].delivery).toBe("not-configured");
  });
});

test.describe("queue and limits", () => {
  test("the queue keeps at most 50 items, newest kept", async () => {
    const h = harness({ sendOk: false });
    await h.care.init();
    await h.care.markNoticeShown();
    for (let i = 0; i < QUEUE_MAX + 15; i++) {
      // Distinct problems; skip the daily cap by moving a day each time.
      h.advance(HEARTBEAT_INTERVAL_MS + 1);
      await h.care.report({ ...START_FAILED, errorCode: `code-${i}` });
    }
    const state = h.care.peekState()!;
    expect(state.queue).toHaveLength(QUEUE_MAX);
    expect(JSON.parse(state.queue[QUEUE_MAX - 1].body).error_code).toBe(`code-${QUEUE_MAX + 14}`);
    expect(JSON.parse(state.queue[0].body).error_code).toBe("code-15");

    h.control.sendOk = true;
    await h.care.flush();
    // Items older than a week were dropped; the rest went.
    expect(h.care.getSnapshot().queued).toBe(0);
    expect(h.sent.length).toBeGreaterThan(0);
    expect(h.sent.length).toBeLessThanOrEqual(QUEUE_MAX);
  });

  test("the same problem is reported at most once an hour", async () => {
    const h = harness();
    await h.care.init();
    await h.care.markNoticeShown();
    expect(await h.care.report(START_FAILED)).toBe("sent");
    h.advance(60_000);
    expect(await h.care.report(START_FAILED)).toBe("rate-limited");
    // Numbers in the message do not make it a new problem.
    expect(
      await h.care.report({ ...START_FAILED, message: START_FAILED.message + " (attempt 2)" }),
    ).toBe("sent");
    expect(
      await h.care.report({ ...START_FAILED, message: START_FAILED.message + " (attempt 3)" }),
    ).toBe("rate-limited");
    h.advance(SAME_REPORT_INTERVAL_MS);
    expect(await h.care.report(START_FAILED)).toBe("sent");
    expect(h.sent).toHaveLength(3);
  });

  test("no more than the daily cap of distinct reports", async () => {
    const h = harness();
    await h.care.init();
    await h.care.markNoticeShown();
    for (let i = 0; i < DAILY_REPORT_CAP + 5; i++) {
      await h.care.report({ ...START_FAILED, errorCode: `code-${i}` });
    }
    expect(h.sent).toHaveLength(DAILY_REPORT_CAP);
  });

  test("a failed send is queued, retried later, and never throws", async () => {
    const h = harness({ sendOk: false });
    await h.care.init();
    await h.care.markNoticeShown();
    expect(await h.care.report(START_FAILED)).toBe("queued");
    await h.care.flush();
    expect(h.care.getSnapshot().queued).toBe(1);
    h.control.sendOk = true;
    await h.care.flush();
    expect(h.sent).toHaveLength(1);
    expect(h.care.getSnapshot().recent[0].delivery).toBe("sent");
  });

  test("broken storage and a throwing transport stay silent", async () => {
    const care = new NodeCare({
      loadState: async () => {
        throw new Error("no store");
      },
      saveState: async () => {
        throw new Error("disk full");
      },
      send: async () => {
        throw new Error("offline");
      },
      platform: async () => PLATFORM,
    });
    await care.markNoticeShown();
    expect(await care.report(START_FAILED)).toBe("queued");
  });
});

test.describe("watching the app", () => {
  function watch() {
    let now = 0;
    const reports: ReportInput[] = [];
    const beats: string[] = [];
    const watcher = new NodeCareWatcher(
      {
        report: async (input) => {
          reports.push(input);
        },
        heartbeat: async (status) => {
          beats.push(status);
        },
      },
      () => now,
    );
    return { watcher, reports, beats, advance: (ms: number) => (now += ms) };
  }
  const running = { running: true, pid: 7, health: "live" as const, version: "0.8.14", uptimeSeconds: 300, lastError: null };

  test("a crash is reported once with the uptime before it, then recovery", () => {
    const w = watch();
    w.watcher.observeStatus(running, null);
    const crashed = { ...running, running: false, pid: null, uptimeSeconds: 0, lastError: "arc-node exited unexpectedly (code 101)" };
    w.watcher.observeStatus(crashed, null);
    w.watcher.observeStatus(crashed, null);
    expect(w.reports).toHaveLength(1);
    expect(w.reports[0]).toMatchObject({ event: "crash", errorCode: "exited-unexpectedly", uptimeS: 300, nodeVersion: "0.8.14" });
    w.watcher.observeStatus({ ...running, uptimeSeconds: 5 }, null);
    expect(w.reports[1]).toMatchObject({ event: "recovered", stage: "running", errorCode: "exited-unexpectedly" });
    expect(w.beats[0]).toBe("earning");
  });

  test("a start fence refusal is a start failure; a long reconciliation is stuck", () => {
    const w = watch();
    const off = { ...running, running: false, pid: null };
    w.watcher.observeStatus(off, FIELD_MESSAGE);
    expect(w.reports).toHaveLength(0);
    w.advance(STUCK_AFTER_MS);
    w.watcher.observeStatus(off, FIELD_MESSAGE);
    w.watcher.observeStatus(off, FIELD_MESSAGE);
    expect(w.reports).toHaveLength(1);
    expect(w.reports[0]).toMatchObject({ event: "stuck", stage: "reconciliation" });

    const blocked = "managed-node durability recovery failed: shutdown receipt missing";
    w.watcher.observeStatus(off, blocked);
    expect(w.reports[1]).toMatchObject({ event: "start_failed", stage: "start-fence", errorCode: "blocked" });
  });

  test("a refused Start and a failed update are reported", () => {
    const w = watch();
    w.watcher.observeStartError(new Error(FIELD_MESSAGE));
    expect(w.reports[0]).toMatchObject({ event: "start_failed", stage: "start", errorCode: "reconciling" });
    const failed = { phase: "error" as const, message: "Update check failed: timed out", error: "timed out", restartRequired: false };
    w.watcher.observeUpdate(failed);
    w.watcher.observeUpdate(failed);
    expect(w.reports).toHaveLength(2);
    expect(w.reports[1]).toMatchObject({ event: "update_failed", stage: "update-check", errorCode: "check-failed" });
  });
});
