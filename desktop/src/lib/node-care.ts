import { classifyStartFailure } from "./start-failure";
import type { NodeStatus } from "./types";
import type { UpdateSnapshot } from "./update-controller";

/**
 * Node Care: opt-out anonymous error reports and a daily heartbeat.
 *
 * When the node will not start, crashes, gets stuck or an update fails, ARC
 * sends one small scrubbed report so the problem can be fixed without a
 * screenshot. Once a day a running app sends a heartbeat so working nodes can
 * be counted. desktop/PRIVACY.md lists every field.
 *
 * Everything here is pure logic with injected I/O, so the unit specs drive it
 * without a native host. Native code (src-tauri/src/node_care.rs) only keeps
 * the state file and POSTs a payload to the endpoint baked in at build time;
 * a build without one never sends anything.
 */

export const REPORT_SCHEMA = "arc.node-care.report.v1";
export const HEARTBEAT_SCHEMA = "arc.node-care.heartbeat.v1";

export const MESSAGE_MAX_BYTES = 2048;
export const QUEUE_MAX = 50;
/** The same problem is reported at most once an hour. */
export const SAME_REPORT_INTERVAL_MS = 60 * 60_000;
/** And no install sends more than this many reports a day. */
export const DAILY_REPORT_CAP = 20;
export const HEARTBEAT_INTERVAL_MS = 24 * 60 * 60_000;
/** Queued items older than this are dropped rather than retried. */
export const QUEUE_MAX_AGE_MS = 7 * 24 * 60 * 60_000;
/** How many recent payloads "See what's sent" keeps. */
export const RECENT_MAX = 5;
/** A node starting, or a last session closing, for this long is stuck. */
export const STUCK_AFTER_MS = 10 * 60_000;

export type NodeCareEvent =
  | "start_failed"
  | "crash"
  | "update_failed"
  | "stuck"
  | "recovered";

export type HeartbeatStatus = "earning" | "syncing" | "starting" | "error";

export interface NodeCareReport {
  schema: typeof REPORT_SCHEMA;
  install_id: string;
  event: NodeCareEvent;
  app_version: string;
  node_version: string;
  os: string;
  arch: string;
  stage: string;
  error_code: string;
  message: string;
  uptime_s: number | null;
  occurred_at: string;
}

export interface NodeCareHeartbeat {
  schema: typeof HEARTBEAT_SCHEMA;
  install_id: string;
  app_version: string;
  node_version: string;
  status: HeartbeatStatus;
  os: string;
}

export type PayloadKind = "report" | "heartbeat";

/** What native `node_care_info` returns. `homeDir` never leaves the machine. */
export interface NodeCarePlatform {
  endpointConfigured: boolean;
  appVersion: string;
  os: string;
  arch: string;
  homeDir: string;
}

export interface NodeCareDeps {
  loadState: () => Promise<string | null>;
  saveState: (json: string) => Promise<void>;
  /** Resolves true only when the endpoint accepted the payload. */
  send: (kind: PayloadKind, body: string) => Promise<boolean>;
  platform: () => Promise<NodeCarePlatform>;
  now?: () => number;
  randomBytes?: (length: number) => Uint8Array;
}

export interface QueuedItem {
  kind: PayloadKind;
  body: string;
  queuedAt: number;
}

export type Delivery = "sent" | "queued" | "not-configured";

export interface RecentItem {
  kind: PayloadKind;
  /** The exact JSON that was (or will be) sent. */
  body: string;
  at: number;
  delivery: Delivery;
}

export interface NodeCareState {
  v: 1;
  installId: string;
  enabled: boolean;
  noticeShownAt: number | null;
  queue: QueuedItem[];
  /** Report fingerprint -> when it was last reported. */
  reportedAt: Record<string, number>;
  lastHeartbeatAt: number | null;
  recent: RecentItem[];
}

export interface NodeCareSnapshot {
  ready: boolean;
  enabled: boolean;
  endpointConfigured: boolean;
  noticeShown: boolean;
  installId: string;
  queued: number;
  recent: readonly RecentItem[];
}

export interface ReportInput {
  event: NodeCareEvent;
  stage: string;
  errorCode: string;
  message: string;
  nodeVersion?: string | null;
  uptimeS?: number | null;
}

export type ReportOutcome =
  | "sent"
  | "queued"
  | "off"
  | "not-configured"
  | "rate-limited";

// ── Scrubbing ────────────────────────────────────────────────────────────

const REDACTED = "[redacted]";

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Truncate to at most `maxBytes` of UTF-8 without splitting a character.
 */
export function truncateUtf8(text: string, maxBytes: number): string {
  const encoder = new TextEncoder();
  if (encoder.encode(text).length <= maxBytes) return text;
  const ellipsis = "…";
  const budget = maxBytes - encoder.encode(ellipsis).length;
  let used = 0;
  let out = "";
  for (const char of text) {
    const size = encoder.encode(char).length;
    if (used + size > budget) break;
    out += char;
    used += size;
  }
  return out + ellipsis;
}

/**
 * Remove anything that could identify a person or a wallet from an error
 * message, then cap it at 2 KB. The home folder and any user folder become
 * `~`; emails, IPv4/IPv6 addresses, 0x-hex of 40+ digits, bare hex of 32+
 * digits and long base64-looking keys become `[redacted]`.
 *
 * No lookbehind: the WebView on older macOS cannot parse it.
 */
export function scrubMessage(raw: string, homeDir?: string | null): string {
  let text = String(raw ?? "");

  // The home folder as the native side reports it, both slash styles.
  const home = (homeDir ?? "").replace(/[\\/]+$/, "");
  if (home.length > 1) {
    for (const variant of new Set([home, home.replace(/\\/g, "/"), home.replace(/\//g, "\\")])) {
      text = text.replace(new RegExp(escapeRegExp(variant), "gi"), "~");
    }
  }
  // Any other user folder: macOS, Linux, Windows (either slash), root.
  text = text
    .replace(/\b[A-Za-z]:[\\/]+(?:Users|Documents and Settings)[\\/]+[^\\/\s:'"]+/gi, "~")
    .replace(/\/(?:Users|home)\/[^/\\\s:'"]+/g, "~")
    .replace(/(^|[\s'"(=:])\/root(?=[/\s'")]|$)/g, "$1~");

  // Emails before anything that could split them.
  text = text.replace(/[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+/g, REDACTED);
  // URLs keep their scheme, lose credentials and host.
  text = text.replace(/\b([a-z][a-z0-9+.-]*:\/\/)[^\s/'"]+/gi, `$1${REDACTED}`);
  // 0x-hex of 40+ digits (addresses, keys, hashes).
  text = text.replace(/\b0x[0-9a-fA-F]{40,}\b/g, REDACTED);
  // Bare hex of 32+ digits (addresses and keys without a prefix).
  text = text.replace(/\b[0-9a-fA-F]{32,}\b/g, REDACTED);
  // IPv4, with an optional port.
  text = text.replace(/\b(?:\d{1,3}\.){3}\d{1,3}(?::\d{1,5})?\b/g, REDACTED);
  // IPv6: a run of hex groups and colons, standing alone, with "::" or at
  // least 5 colons (so 12:34:56 and std::io stay as they are).
  text = text.replace(
    /(^|[^\w:.])(\[?[0-9a-fA-F]{0,4}(?::[0-9a-fA-F]{0,4}){2,7}(?:%[\w.]+)?\]?(?::\d{1,5})?)(?![\w:])/g,
    (match, prefix: string, candidate: string) => {
      const bare = candidate.replace(/^\[|\](?::\d+)?$/g, "");
      const colons = (bare.match(/:/g) ?? []).length;
      const isIpv6 = (bare.includes("::") && /[0-9a-fA-F]/.test(bare)) || colons >= 5;
      return isIpv6 ? `${prefix}${REDACTED}` : match;
    },
  );
  // Long base64 / key material: 32+ characters mixing digits and both cases.
  text = text.replace(/[A-Za-z0-9+/_-]{32,}={0,2}/g, (token) =>
    /\d/.test(token) && /[a-z]/.test(token) && /[A-Z]/.test(token) ? REDACTED : token,
  );

  return truncateUtf8(text, MESSAGE_MAX_BYTES);
}

// ── Payloads ─────────────────────────────────────────────────────────────

function isoSeconds(ms: number): string {
  return new Date(Math.floor(ms / 1000) * 1000).toISOString().replace(".000Z", "Z");
}

/** Short tokens only: anything else in a code field becomes "unknown". */
function code(value: string | null | undefined): string {
  const text = String(value ?? "").trim();
  return /^[A-Za-z0-9._-]{1,64}$/.test(text) ? text : "unknown";
}

export function buildReport(
  input: ReportInput,
  context: { installId: string; platform: NodeCarePlatform; now: number },
): NodeCareReport {
  const uptime =
    typeof input.uptimeS === "number" && Number.isFinite(input.uptimeS) && input.uptimeS >= 0
      ? Math.floor(input.uptimeS)
      : null;
  return {
    schema: REPORT_SCHEMA,
    install_id: context.installId,
    event: input.event,
    app_version: code(context.platform.appVersion),
    node_version: code(input.nodeVersion ?? ""),
    os: code(context.platform.os),
    arch: code(context.platform.arch),
    stage: code(input.stage),
    error_code: code(input.errorCode),
    message: scrubMessage(input.message, context.platform.homeDir),
    uptime_s: uptime,
    occurred_at: isoSeconds(context.now),
  };
}

export function buildHeartbeat(
  status: HeartbeatStatus,
  nodeVersion: string | null | undefined,
  context: { installId: string; platform: NodeCarePlatform },
): NodeCareHeartbeat {
  return {
    schema: HEARTBEAT_SCHEMA,
    install_id: context.installId,
    app_version: code(context.platform.appVersion),
    node_version: code(nodeVersion ?? ""),
    status,
    os: code(context.platform.os),
  };
}

/**
 * The same problem twice: same event, stage and code, and the same message
 * once numbers (exit codes, ports, counts, times) are ignored.
 */
export function reportFingerprint(report: Pick<NodeCareReport, "event" | "stage" | "error_code" | "message">): string {
  const key = [report.event, report.stage, report.error_code, report.message.replace(/\d+/g, "#")].join("|");
  // FNV-1a, 32-bit: a local dedupe key, never sent.
  let hash = 0x811c9dc5;
  for (let i = 0; i < key.length; i++) {
    hash ^= key.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash.toString(16).padStart(8, "0");
}

/** The heartbeat status for a node status, or null when the node is off by choice. */
export function heartbeatStatus(
  status: Pick<NodeStatus, "running" | "pid" | "health" | "lastError"> | undefined,
  startBlock: string | null,
): HeartbeatStatus | null {
  if (!status) return null;
  if (status.running) {
    return status.health === "live" || status.health === "lite" ? "earning" : "syncing";
  }
  if (startBlock || status.lastError) return "error";
  if (status.pid != null) return "starting";
  return null;
}

// ── State ────────────────────────────────────────────────────────────────

function randomHex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

function defaultRandomBytes(length: number): Uint8Array {
  const bytes = new Uint8Array(length);
  globalThis.crypto.getRandomValues(bytes);
  return bytes;
}

const INSTALL_ID = /^[0-9a-f]{32}$/;

/** Read a stored state, keeping only well-formed parts. */
export function parseState(raw: string | null, fresh: () => NodeCareState): NodeCareState {
  const base = fresh();
  if (!raw) return base;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return base;
  }
  if (!parsed || typeof parsed !== "object") return base;
  const p = parsed as Record<string, unknown>;
  const kinds: PayloadKind[] = ["report", "heartbeat"];
  const queue = Array.isArray(p.queue)
    ? (p.queue as unknown[])
        .filter(
          (item): item is QueuedItem =>
            !!item &&
            typeof item === "object" &&
            kinds.includes((item as QueuedItem).kind) &&
            typeof (item as QueuedItem).body === "string" &&
            typeof (item as QueuedItem).queuedAt === "number",
        )
        .slice(-QUEUE_MAX)
    : [];
  const recent = Array.isArray(p.recent)
    ? (p.recent as unknown[])
        .filter(
          (item): item is RecentItem =>
            !!item &&
            typeof item === "object" &&
            kinds.includes((item as RecentItem).kind) &&
            typeof (item as RecentItem).body === "string" &&
            typeof (item as RecentItem).at === "number",
        )
        .slice(0, RECENT_MAX)
    : [];
  const reportedAt: Record<string, number> = {};
  if (p.reportedAt && typeof p.reportedAt === "object") {
    for (const [key, value] of Object.entries(p.reportedAt as Record<string, unknown>)) {
      if (typeof value === "number") reportedAt[key] = value;
    }
  }
  return {
    v: 1,
    installId: typeof p.installId === "string" && INSTALL_ID.test(p.installId) ? p.installId : base.installId,
    // Only an explicit false is off: the switch defaults on.
    enabled: p.enabled !== false,
    noticeShownAt: typeof p.noticeShownAt === "number" ? p.noticeShownAt : null,
    queue,
    reportedAt,
    lastHeartbeatAt: typeof p.lastHeartbeatAt === "number" ? p.lastHeartbeatAt : null,
    recent,
  };
}

// ── Controller ───────────────────────────────────────────────────────────

/**
 * Owns the switch, the install ID, the queue and the limits. Every method is
 * safe to call at any time and never throws: a failure to save or send is
 * silent, and nothing here is awaited on the start path.
 */
export class NodeCare {
  private state: NodeCareState | null = null;
  private platform: NodeCarePlatform | null = null;
  private initPromise: Promise<void> | null = null;
  private flushing: Promise<void> | null = null;
  private listeners = new Set<() => void>();
  private snapshot: NodeCareSnapshot = {
    ready: false,
    enabled: true,
    endpointConfigured: false,
    noticeShown: false,
    installId: "",
    queued: 0,
    recent: [],
  };
  private readonly now: () => number;
  private readonly randomBytes: (length: number) => Uint8Array;

  constructor(private readonly deps: NodeCareDeps) {
    this.now = deps.now ?? Date.now;
    this.randomBytes = deps.randomBytes ?? defaultRandomBytes;
  }

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = (): NodeCareSnapshot => this.snapshot;

  /** Load (or create) the state. Later calls reuse the first. */
  init(): Promise<void> {
    if (!this.initPromise) {
      this.initPromise = (async () => {
        let raw: string | null = null;
        try {
          raw = await this.deps.loadState();
        } catch {
          raw = null;
        }
        try {
          this.platform = await this.deps.platform();
        } catch {
          this.platform = null;
        }
        this.state = parseState(raw, () => this.freshState());
        await this.persist();
        this.emit();
      })();
      void this.initPromise.then(() => this.flush());
    }
    return this.initPromise;
  }

  /** Respected at once: the queue is dropped and nothing more is sent. */
  async setEnabled(enabled: boolean): Promise<void> {
    await this.init();
    const state = this.state!;
    state.enabled = enabled;
    if (!enabled) state.queue = [];
    await this.persist();
    this.emit();
    if (enabled) void this.flush();
  }

  /** A new random install ID; the old one is never sent again. */
  async resetInstallId(): Promise<void> {
    await this.init();
    const state = this.state!;
    state.installId = randomHex(this.randomBytes(16));
    // Queued payloads carry the old ID.
    state.queue = [];
    state.lastHeartbeatAt = null;
    await this.persist();
    this.emit();
  }

  /** The first-run notice has been on screen. Queued reports may now go. */
  async markNoticeShown(): Promise<void> {
    await this.init();
    const state = this.state!;
    if (state.noticeShownAt !== null) return;
    state.noticeShownAt = this.now();
    await this.persist();
    this.emit();
    void this.flush();
  }

  async report(input: ReportInput): Promise<ReportOutcome> {
    try {
      await this.init();
      const state = this.state!;
      const platform = this.platform;
      if (!state.enabled) return "off";
      const now = this.now();
      const report = buildReport(input, {
        installId: state.installId,
        platform: platform ?? {
          endpointConfigured: false,
          appVersion: "",
          os: "",
          arch: "",
          homeDir: "",
        },
        now,
      });
      const body = JSON.stringify(report);

      // Limits, counted per attempt so a failing network cannot build a pile.
      for (const [key, at] of Object.entries(state.reportedAt)) {
        if (now - at > HEARTBEAT_INTERVAL_MS) delete state.reportedAt[key];
      }
      const fingerprint = reportFingerprint(report);
      const last = state.reportedAt[fingerprint];
      if (last !== undefined && now - last < SAME_REPORT_INTERVAL_MS) return "rate-limited";
      if (Object.keys(state.reportedAt).length >= DAILY_REPORT_CAP) return "rate-limited";
      state.reportedAt[fingerprint] = now;

      if (!platform?.endpointConfigured) {
        this.remember("report", body, "not-configured");
        await this.persist();
        this.emit();
        return "not-configured";
      }
      return await this.deliver("report", body);
    } catch {
      return "queued";
    }
  }

  /** At most once every 24 hours. */
  async heartbeat(status: HeartbeatStatus, nodeVersion: string | null | undefined): Promise<ReportOutcome> {
    try {
      // Cheap in-memory check first: this runs on every status poll.
      const current = this.state;
      const now = this.now();
      if (current && current.lastHeartbeatAt !== null && now - current.lastHeartbeatAt < HEARTBEAT_INTERVAL_MS) {
        return "rate-limited";
      }
      await this.init();
      const state = this.state!;
      if (!state.enabled) return "off";
      if (!this.platform?.endpointConfigured) return "not-configured";
      if (state.noticeShownAt === null) return "rate-limited";
      if (state.lastHeartbeatAt !== null && now - state.lastHeartbeatAt < HEARTBEAT_INTERVAL_MS) {
        return "rate-limited";
      }
      state.lastHeartbeatAt = now;
      const body = JSON.stringify(
        buildHeartbeat(status, nodeVersion, { installId: state.installId, platform: this.platform }),
      );
      return await this.deliver("heartbeat", body);
    } catch {
      return "queued";
    }
  }

  /** Retry the queue, oldest first, stopping at the first failure. */
  flush(): Promise<void> {
    if (this.flushing) return this.flushing;
    const run = (async () => {
      const state = this.state;
      if (!state || !state.enabled || state.noticeShownAt === null) return;
      if (!this.platform?.endpointConfigured) return;
      const now = this.now();
      const before = state.queue.length;
      state.queue = state.queue.filter((item) => now - item.queuedAt <= QUEUE_MAX_AGE_MS);
      let changed = state.queue.length !== before;
      while (state.queue.length > 0 && state.enabled) {
        const item = state.queue[0];
        let ok = false;
        try {
          ok = await this.deps.send(item.kind, item.body);
        } catch {
          ok = false;
        }
        if (!ok) break;
        // Switched off while sending: the queue was already dropped.
        if (state.queue[0] === item) state.queue.shift();
        this.markSent(item.body);
        changed = true;
      }
      if (changed) {
        await this.persist();
        this.emit();
      }
    })();
    // Cleared after the assignment: a run that returns before its first
    // await must not leave a settled promise blocking every later flush.
    const flushing = run.catch(() => {}).finally(() => {
      if (this.flushing === flushing) this.flushing = null;
    });
    this.flushing = flushing;
    return flushing;
  }

  /** Test seam: the stored state as JSON. */
  peekState(): NodeCareState | null {
    return this.state ? (JSON.parse(JSON.stringify(this.state)) as NodeCareState) : null;
  }

  private freshState(): NodeCareState {
    return {
      v: 1,
      installId: randomHex(this.randomBytes(16)),
      enabled: true,
      noticeShownAt: null,
      queue: [],
      reportedAt: {},
      lastHeartbeatAt: null,
      recent: [],
    };
  }

  private async deliver(kind: PayloadKind, body: string): Promise<ReportOutcome> {
    const state = this.state!;
    let ok = false;
    // Before the first-run notice has been seen, everything waits.
    if (state.noticeShownAt !== null) {
      try {
        ok = await this.deps.send(kind, body);
      } catch {
        ok = false;
      }
    }
    if (!ok && state.enabled) this.enqueue({ kind, body, queuedAt: this.now() });
    this.remember(kind, body, ok ? "sent" : "queued");
    await this.persist();
    this.emit();
    return ok ? "sent" : "queued";
  }

  private enqueue(item: QueuedItem) {
    const state = this.state!;
    state.queue.push(item);
    while (state.queue.length > QUEUE_MAX) state.queue.shift();
  }

  private remember(kind: PayloadKind, body: string, delivery: Delivery) {
    const state = this.state!;
    state.recent = [{ kind, body, at: this.now(), delivery }, ...state.recent].slice(0, RECENT_MAX);
  }

  private markSent(body: string) {
    const state = this.state!;
    const entry = state.recent.find((item) => item.body === body);
    if (entry) entry.delivery = "sent";
  }

  private async persist() {
    if (!this.state) return;
    try {
      await this.deps.saveState(JSON.stringify(this.state));
    } catch {
      /* silent: reporting must never surface an error */
    }
  }

  private emit() {
    const state = this.state;
    this.snapshot = {
      ready: state !== null,
      enabled: state?.enabled ?? true,
      endpointConfigured: this.platform?.endpointConfigured ?? false,
      noticeShown: state?.noticeShownAt != null,
      installId: state?.installId ?? "",
      queued: state?.queue.length ?? 0,
      recent: state ? state.recent.map((item) => ({ ...item })) : [],
    };
    for (const listener of this.listeners) listener();
  }
}

// ── Watching the app ─────────────────────────────────────────────────────

export type WatchedStatus = Pick<
  NodeStatus,
  "running" | "pid" | "health" | "version" | "uptimeSeconds" | "lastError"
>;

interface Sink {
  report: (input: ReportInput) => Promise<unknown>;
  heartbeat: (status: HeartbeatStatus, nodeVersion: string | null | undefined) => Promise<unknown>;
}

const CRASH_TEXT = "exited unexpectedly";

/**
 * Turns what the app already observes (the node status poll, the start
 * fence, Start errors, the updater) into reports. Each distinct problem is
 * reported once while it lasts; the first healthy status after one is a
 * `recovered` report.
 */
export class NodeCareWatcher {
  private problem: { stage: string; errorCode: string } | null = null;
  private crashKey: string | null = null;
  private statusErrorKey: string | null = null;
  private blockKey: string | null = null;
  private updateErrorKey: string | null = null;
  private startingSince: number | null = null;
  private reconcilingSince: number | null = null;
  private stuckReported = false;
  private lastUptime: number | null = null;
  private nodeVersion: string | null = null;

  constructor(
    private readonly sink: Sink,
    private readonly now: () => number = Date.now,
  ) {}

  observeStatus(status: WatchedStatus | undefined, startBlock: string | null) {
    if (!status) return;
    const now = this.now();
    if (status.version) this.nodeVersion = status.version;
    const lastError = status.lastError ?? null;
    const crashed = !status.running && !!lastError && lastError.includes(CRASH_TEXT);

    if (crashed) {
      if (lastError !== this.crashKey) {
        this.crashKey = lastError;
        this.raise({
          event: "crash",
          stage: "running",
          errorCode: "exited-unexpectedly",
          message: lastError!,
          uptimeS: this.lastUptime,
        });
      }
    } else if (!status.running && lastError && status.pid == null) {
      // A start the app made by itself (launch auto-start, crash restart).
      if (lastError !== this.statusErrorKey) {
        this.statusErrorKey = lastError;
        this.raise({
          event: "start_failed",
          stage: "auto-start",
          errorCode: classifyStartFailure(lastError),
          message: lastError,
        });
      }
    }

    if (startBlock) {
      const kind = classifyStartFailure(startBlock);
      if (kind === "reconciling") {
        this.reconcilingSince ??= now;
        if (!this.stuckReported && now - this.reconcilingSince >= STUCK_AFTER_MS) {
          this.stuckReported = true;
          this.raise({
            event: "stuck",
            stage: "reconciliation",
            errorCode: "reconciling-too-long",
            message: startBlock,
          });
        }
      } else if (startBlock !== this.blockKey) {
        this.blockKey = startBlock;
        this.raise({ event: "start_failed", stage: "start-fence", errorCode: kind, message: startBlock });
      }
    } else {
      this.reconcilingSince = null;
      this.blockKey = null;
    }

    if (!status.running && status.pid != null) {
      this.startingSince ??= now;
      if (!this.stuckReported && now - this.startingSince >= STUCK_AFTER_MS) {
        this.stuckReported = true;
        this.raise({
          event: "stuck",
          stage: "starting",
          errorCode: "starting-too-long",
          message: lastError ?? "",
        });
      }
    } else {
      this.startingSince = null;
    }

    if (status.running) {
      this.lastUptime = status.uptimeSeconds;
      if (status.health !== "offline") {
        if (this.problem) {
          const { stage, errorCode } = this.problem;
          this.problem = null;
          void this.sink
            .report({
              event: "recovered",
              stage,
              errorCode,
              message: "",
              nodeVersion: this.nodeVersion,
              uptimeS: status.uptimeSeconds,
            })
            .catch(() => {});
        }
        this.crashKey = null;
        this.statusErrorKey = null;
        if (!this.reconcilingSince && !this.startingSince) this.stuckReported = false;
      }
    }

    const beat = heartbeatStatus(status, startBlock);
    if (beat) void this.sink.heartbeat(beat, this.nodeVersion).catch(() => {});
  }

  /** A Start or Restart the user pressed was refused. */
  observeStartError(error: unknown, stage: string = "start") {
    const message = error instanceof Error ? error.message : String(error);
    // The status poll may show the same text next; one report is enough.
    this.statusErrorKey = message;
    this.raise({ event: "start_failed", stage, errorCode: classifyStartFailure(message), message });
  }

  observeUpdate(snapshot: Pick<UpdateSnapshot, "phase" | "message" | "error" | "restartRequired">) {
    if (!snapshot.error) {
      this.updateErrorKey = null;
      return;
    }
    const key = `${snapshot.phase}|${snapshot.error}`;
    if (key === this.updateErrorKey) return;
    this.updateErrorKey = key;
    const stage =
      snapshot.phase === "error" && snapshot.message.startsWith("Update check failed")
        ? "update-check"
        : snapshot.phase === "ready"
          ? "update-relaunch"
          : snapshot.phase === "available"
            ? "update-auto-install"
            : "update-install";
    const errorCode = snapshot.restartRequired
      ? "restart-required"
      : stage === "update-check"
        ? "check-failed"
        : stage === "update-auto-install"
          ? "deferred"
          : "install-failed";
    void this.sink
      .report({ event: "update_failed", stage, errorCode, message: snapshot.error, nodeVersion: this.nodeVersion })
      .catch(() => {});
  }

  private raise(input: Omit<ReportInput, "nodeVersion">) {
    this.problem = { stage: input.stage, errorCode: input.errorCode };
    void this.sink.report({ ...input, nodeVersion: this.nodeVersion }).catch(() => {});
  }
}
