import { PLAIN_COPY } from "./plain-copy";

export const NATIVE_REFUSAL_PREFIX =
  "ARC refused to start the node because chain-data migration is not safely resolved: ";
export const NATIVE_REFUSAL_ADVICE =
  "Restart ARC after repairing the reported path or permissions; do not point v0.8 at the preserved legacy directory.";
export const NATIVE_RECONCILING = "startup reconciliation is still in progress";
export const NATIVE_OTHER_DESKTOP = "ARC is already open on this computer.";
export const NATIVE_UPDATE_UNFINISHED =
  "ARC couldn't finish an update. Quit ARC and open it again.";

const REPAIR_PATTERN =
  /migration preflight|could not be persisted|legacy desktop node reconciliation|legacy node reconciliation context|permission|symbolic[- ]link|not a directory|not a regular|must already exist|read-only file system|no space left|update (?:the managed node|arc-node) before/i;
const DURABILITY_PATTERN =
  /durability recovery|shutdown receipt|shutdown boundary|\bWAL\b|crash recovery|startup reconciliation failed/;
const NETWORK_PATTERN =
  /transport failed|returned HTTP (?:408|429|5\d\d)\b|timed out|error sending request|dns error|connection (?:refused|reset)/i;

export type StartFailureKind =
  | "other-desktop"
  | "closing"
  | "reconciling"
  | "update-unfinished"
  | "blocked"
  | "needs-repair"
  | "network"
  | "unknown";
export type StartFailureAction = "close-copy" | "install-update" | "restart-arc" | "quit-arc" | "try-again";

export interface StartFailureView {
  kind: StartFailureKind;
  title: string;
  body: string;
  action: StartFailureAction;
  details: string;
  detailsOpen: boolean;
}

export function isStartupReconciliationInProgress(raw: string): boolean {
  return raw.includes(NATIVE_RECONCILING) && !raw.includes(NATIVE_REFUSAL_PREFIX);
}

export function classifyStartFailure(raw: string): StartFailureKind {
  if (raw.includes(NATIVE_OTHER_DESKTOP) || raw.includes("another ARC desktop currently owns the managed node lifecycle")) return "other-desktop";
  if (raw === "ARC is closing") return "closing";
  if (isStartupReconciliationInProgress(raw)) return "reconciling";
  if (raw.includes(NATIVE_UPDATE_UNFINISHED)) return "update-unfinished";
  const reason = raw.split(NATIVE_REFUSAL_ADVICE).join(" ");
  if (REPAIR_PATTERN.test(reason)) return "needs-repair";
  if (DURABILITY_PATTERN.test(reason)) return "blocked";
  if (reason.includes(NATIVE_REFUSAL_PREFIX)) return "needs-repair";
  if (NETWORK_PATTERN.test(reason)) return "network";
  return "unknown";
}

export function cleanFailureDetails(raw: string): string {
  const boundary = String.raw`(?=[\\/]|[.,;:)\]}!?](?:\s|$)|\s|$)`;
  const user = String.raw`[^\\/\r\n:;,()"']+?`;
  let text = raw.trim();
  const roots: [string, string][] = [
    [String.raw`(?:\\\\\?\\|//\?/)?[A-Za-z]:[\\/](?:Users|Profiles|Documents and Settings)[\\/]`, "gi"],
    [String.raw`/(?:Users|(?:var/|usr/|export/)?home)/`, "g"],
  ];
  for (const [root, flags] of roots) {
    text = text.replace(new RegExp(`${root}[^\\\\/\\r\\n]+(?=[\\\\/])`, flags), "~")
      .replace(new RegExp(`${root}${user}${boundary}`, flags), "~");
  }
  text = text.replace(new RegExp(String.raw`/(?:var/)?root${boundary}`, "g"), "~");
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

export function describeStartFailure(
  raw: string,
  options: { updateAvailable: boolean },
): StartFailureView {
  const kind = classifyStartFailure(raw);
  const details = cleanFailureDetails(raw);
  const copy = PLAIN_COPY.startFailure;
  switch (kind) {
    case "other-desktop":
      return { kind, ...copy.otherDesktop, action: "close-copy", details, detailsOpen: false };
    case "closing":
      return { kind, ...copy.closing, action: "try-again", details, detailsOpen: false };
    case "reconciling":
      return { kind, ...copy.reconciling, action: "try-again", details, detailsOpen: false };
    case "update-unfinished":
      return { kind, ...copy.updateUnfinished, action: "quit-arc", details, detailsOpen: false };
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
