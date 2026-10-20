import { PLAIN_COPY } from "./plain-copy";

/**
 * Native wording this module recognises. None of it is shown as the
 * sentence: these are the stable phrases start_fence.rs, the node manager and
 * the release download put in their errors, and start-failure.spec.ts pins
 * the start-fence ones against the Rust source.
 */
export const NATIVE_REFUSAL_PREFIX =
  "ARC refused to start the node because chain-data migration is not safely resolved: ";
export const NATIVE_REFUSAL_ADVICE =
  "Restart ARC after repairing the reported path or permissions; do not point v0.8 at the preserved legacy directory.";
export const NATIVE_RECONCILING = "startup reconciliation is still in progress";

// Something on this computer needs fixing, and the native text says what: a
// path, a permission, a migration preflight, another desktop holding the
// node, an arc-node too old for this desktop. Checked before the durability
// phrases: a permission error during recovery is still a repair, and the
// outdated-binary text mentions the WAL barrier it cannot prove.
const REPAIR_PATTERN =
  /migration preflight|could not be persisted|legacy desktop node reconciliation|legacy node reconciliation context|permission|symbolic[- ]link|not a directory|not a regular|must already exist|another ARC desktop|read-only file system|no space left|update (?:the managed node|arc-node) before/i;
// The previous session did not close cleanly and ARC could not finish
// closing it.
const DURABILITY_PATTERN =
  /durability recovery|shutdown receipt|shutdown boundary|\bWAL\b|crash recovery|startup reconciliation failed/;
// The statuses commands.rs retries as transient; any other status is not the
// user's connection.
const NETWORK_PATTERN =
  /transport failed|returned HTTP (?:408|429|5\d\d)\b|timed out|error sending request|dns error|connection (?:refused|reset)/i;

export type StartFailureKind =
  | "reconciling"
  | "blocked"
  | "needs-repair"
  | "network"
  | "unknown";
export type StartFailureAction = "install-update" | "restart-arc" | "try-again";

export interface StartFailureView {
  kind: StartFailureKind;
  title: string;
  body: string;
  /** The one primary action. */
  action: StartFailureAction;
  /** The native text, nested framing collapsed, for "Show details". */
  details: string;
  /** Open when the native text is the actionable part. */
  detailsOpen: boolean;
}

/**
 * Launch-time reconciliation still owns the node. Through v0.8.12 the same
 * phrase also appeared nested inside a refusal; that is a refusal, not a wait.
 */
export function isStartupReconciliationInProgress(raw: string): boolean {
  return raw.includes(NATIVE_RECONCILING) && !raw.includes(NATIVE_REFUSAL_PREFIX);
}

export function classifyStartFailure(raw: string): StartFailureKind {
  if (isStartupReconciliationInProgress(raw)) return "reconciling";
  // Every refusal carries the same advice, which names paths and
  // permissions. Only the reason decides the kind.
  const reason = raw.split(NATIVE_REFUSAL_ADVICE).join(" ");
  if (REPAIR_PATTERN.test(reason)) return "needs-repair";
  if (DURABILITY_PATTERN.test(reason)) return "blocked";
  // Any other refusal is a migration refusal, which names what to repair.
  if (reason.includes(NATIVE_REFUSAL_PREFIX)) return "needs-repair";
  if (NETWORK_PATTERN.test(reason)) return "network";
  return "unknown";
}

/**
 * v0.8.12 recorded a refusal inside another and showed the framing and its
 * advice twice. Keep the first framing and one copy of the advice; leave
 * every other character as the native side wrote it, paths included.
 */
export function cleanFailureDetails(raw: string): string {
  let text = raw.trim();
  const first = text.indexOf(NATIVE_REFUSAL_PREFIX);
  if (first >= 0) {
    const end = first + NATIVE_REFUSAL_PREFIX.length;
    text =
      text.slice(0, end) + text.slice(end).split(NATIVE_REFUSAL_PREFIX).join("");
  }
  if (text.includes(NATIVE_REFUSAL_ADVICE)) {
    const rest = text
      .split(NATIVE_REFUSAL_ADVICE)
      .map((part) => part.trim())
      .filter(Boolean)
      .join(" ")
      .replace(/[.\s]+$/, "");
    text = rest ? `${rest}. ${NATIVE_REFUSAL_ADVICE}` : NATIVE_REFUSAL_ADVICE;
  }
  return text;
}

/**
 * One plain sentence and one action for a node that would not start.
 * - A wait or a download failure asks for another try.
 * - An unclean last session offers the update when one is ready (it installs
 *   even while the node is blocked), otherwise a restart, which runs launch
 *   reconciliation again.
 * - A repair says so, opens the details, and offers the step that follows
 *   the repair: a restart for a launch refusal, which holds until ARC
 *   restarts; otherwise another try.
 * - Anything else opens the details and offers the update or another try,
 *   never a restart that cannot help.
 */
export function describeStartFailure(
  raw: string,
  options: { updateAvailable: boolean },
): StartFailureView {
  const kind = classifyStartFailure(raw);
  const details = cleanFailureDetails(raw);
  const copy = PLAIN_COPY.startFailure;
  switch (kind) {
    case "reconciling":
      return { kind, ...copy.reconciling, action: "try-again", details, detailsOpen: false };
    case "network":
      return { kind, ...copy.network, action: "try-again", details, detailsOpen: false };
    case "blocked":
      return {
        kind,
        ...copy.blocked,
        action: options.updateAvailable ? "install-update" : "restart-arc",
        details,
        detailsOpen: false,
      };
    case "needs-repair": {
      const launchRefusal = raw.includes(NATIVE_REFUSAL_PREFIX);
      return {
        kind,
        title: copy.needsRepair.title,
        body: launchRefusal
          ? copy.needsRepair.bodyThenRestart
          : copy.needsRepair.bodyThenRetry,
        action: launchRefusal ? "restart-arc" : "try-again",
        details,
        detailsOpen: true,
      };
    }
    case "unknown":
      return {
        kind,
        ...copy.unknown,
        action: options.updateAvailable ? "install-update" : "try-again",
        details,
        detailsOpen: true,
      };
  }
}
