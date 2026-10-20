import { PLAIN_COPY } from "./plain-copy";

/**
 * Native wording this module recognises. None of it is shown as the
 * sentence: these are the stable phrases start_fence.rs, the node manager and
 * the release download put in their errors, and start-failure.spec.ts pins
 * them against the Rust source.
 */
export const NATIVE_REFUSAL_PREFIX =
  "ARC refused to start the node because chain-data migration is not safely resolved: ";
export const NATIVE_REFUSAL_ADVICE =
  "Restart ARC after repairing the reported path or permissions; do not point v0.8 at the preserved legacy directory.";
export const NATIVE_RECONCILING = "startup reconciliation is still in progress";

const BLOCKED_PHRASES = [
  NATIVE_REFUSAL_PREFIX,
  "startup reconciliation failed",
  "durability recovery",
  "shutdown receipt",
  "refusing Stop/update boundary",
  "legacy desktop node reconciliation",
];
// The statuses commands.rs retries as transient; any other status is not the
// user's connection.
const NETWORK_PATTERN =
  /transport failed|returned HTTP (?:408|429|5\d\d)\b|timed out|error sending request|dns error|connection (?:refused|reset)/i;

export type StartFailureKind = "reconciling" | "blocked" | "network" | "unknown";
export type StartFailureAction = "install-update" | "restart-arc" | "try-again";

export interface StartFailureView {
  kind: StartFailureKind;
  title: string;
  body: string;
  /** The one primary action. */
  action: StartFailureAction;
  /** The native text, nested framing collapsed, for "Show details". */
  details: string;
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
  if (BLOCKED_PHRASES.some((phrase) => raw.includes(phrase))) return "blocked";
  if (NETWORK_PATTERN.test(raw)) return "network";
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
 * One plain sentence and one action for a node that would not start. A wait
 * or a download failure asks for another try. Anything else offers the
 * update when one is ready, since the update installs even while the node is
 * blocked, and otherwise a restart, which runs launch reconciliation again.
 */
export function describeStartFailure(
  raw: string,
  options: { updateAvailable: boolean },
): StartFailureView {
  const kind = classifyStartFailure(raw);
  const copy = PLAIN_COPY.startFailure[kind];
  const action: StartFailureAction =
    kind === "reconciling" || kind === "network"
      ? "try-again"
      : options.updateAvailable
        ? "install-update"
        : "restart-arc";
  return {
    kind,
    title: copy.title,
    body: copy.body,
    action,
    details: cleanFailureDetails(raw),
  };
}
