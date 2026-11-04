import type { NodeCareSnapshot } from "./node-care";
import { scrubMessage } from "./node-care";
import { PLAIN_COPY } from "./plain-copy";

/**
 * Where a person can get help, as the README's "Community and help" section
 * (#222) and docs/GETTING_STARTED.md give it. The documentation contract test
 * checks that every URL here is in the README, so the app and the docs cannot
 * drift apart.
 */
export const SUPPORT_LINKS = {
  discord: "https://discord.com/invite/arcreactorai",
  telegram: "https://t.me/JoinTheARC",
  issues: "https://github.com/FerrumVir/arc-chain/issues",
  docs: "https://github.com/FerrumVir/arc-chain#community-and-help",
} as const;

export type HelpContext = keyof typeof PLAIN_COPY.help.state;

const REDACTED = "[redacted]";
// Device names on a home or office network ("Toms-MacBook-Pro.local").
const LOCAL_HOSTNAME = /\b[A-Za-z0-9][A-Za-z0-9-]{0,62}\.(?:local|lan|home|internal|localdomain)\b/gi;
// @handles, not npm scopes ("@types/node") or emails (already removed). No lookbehind: older macOS WebViews can't parse it.
const HANDLE = /(^|[^\w@./])@[A-Za-z0-9_]{3,32}(?![\w/])/g;
// Phone numbers: 9+ digits written with a +, spaces, dashes or brackets. Dotted versions, clock
// times ("12:30"), dates ("2026-11-04") and bare counts or heights never match.
const PHONE = /(^|[^\w.+])(\+?\(?\d{1,4}\)?(?:[ -]?\(?\d{2,5}\)?){2,5})(?!\w|:|\.\d)/g;

/**
 * The one scrubber for anything a person copies to share: Node Care's report
 * scrub (home folders on macOS, Linux and Windows, emails, URL hosts, IP
 * addresses, keys, addresses and long hex), plus local device hostnames,
 * @handles and phone numbers. Error wording, codes, versions and paths under
 * the home folder stay, so the text is still useful to whoever answers.
 */
export function scrubForSharing(raw: string, homeDir?: string | null): string {
  return scrubMessage(raw, homeDir)
    .replace(LOCAL_HOSTNAME, REDACTED)
    .replace(HANDLE, (_m, prefix: string) => `${prefix}@${REDACTED}`)
    .replace(PHONE, (match, prefix: string, number: string) => {
      const digits = number.replace(/\D/g, "").length;
      const written = /[+()\s-]/.test(number.trim());
      return digits >= 9 && written ? `${prefix}${REDACTED}` : match;
    });
}

/**
 * Whether ARC has what it needs to scrub native text before it is shared:
 * the home folder the native side reports. Anything else (still loading, the
 * lookup failed, no home folder) is not ready, and native text is withheld.
 */
export type PrivacyContext = { state: "ready"; homeDir: string } | { state: "pending" | "unavailable" };

export function privacyContextFrom(lookup: { status: "pending" | "error" | "success"; homeDir?: string | null }): PrivacyContext {
  if (lookup.status === "pending") return { state: "pending" };
  const home = String(lookup.homeDir ?? "").trim().replace(/[\\/]+$/, "");
  if (lookup.status !== "success" || home.length < 2) return { state: "unavailable" };
  return { state: "ready", homeDir: home };
}

/**
 * The one policy for anything Copy details shares. With privacy context, the
 * native text scrubbed by scrubForSharing. Without it, only app-authored
 * context (the sentence ARC shows, which callers take from PLAIN_COPY) and a
 * line saying the detailed diagnostics were left out: never the native text,
 * however harmless it looks.
 */
export function sharePayload(input: { raw: string; title?: string | null; privacy: PrivacyContext }): { text: string; detailed: boolean } {
  if (input.privacy.state === "ready") return { text: scrubForSharing(input.raw, input.privacy.homeDir), detailed: true };
  const lines = input.title ? [`ARC says: ${input.title}`] : [];
  lines.push(PLAIN_COPY.share.omitted);
  return { text: lines.join("\n"), detailed: false };
}

// Version and system fields are app data, but they still pass a shape check before they are shared.
const VERSION_SHAPE = /^[0-9A-Za-z][0-9A-Za-z.+-]{0,31}$/;
const SYSTEM_SHAPE = /^[a-z0-9_]{1,16}$/;
const shaped = (v: string | null | undefined, shape: RegExp) => {
  const t = String(v ?? "").trim();
  return shape.test(t) ? t : "unknown";
};

export interface SupportDetailsInput {
  context: HelpContext;
  appVersion: string | null;
  nodeVersion: string | null;
  os: string | null;
  arch: string | null;
  /** The plain sentence the app shows, if any. */
  title?: string | null;
  /** The native error text, if any. Shared only scrubbed, and only when `privacy` is ready. */
  error?: string | null;
  privacy: PrivacyContext;
}

/**
 * The text "Copy details" puts on the clipboard. Only what helps someone
 * answer: versions, system, what the app shows and the native text, with home
 * folders, emails, hosts and keys scrubbed the same way Node Care reports are.
 */
export function buildSupportDetails(input: SupportDetailsInput): string {
  const copy = PLAIN_COPY.help;
  const lines = [
    copy.detailsHeader,
    `App: ${shaped(input.appVersion, VERSION_SHAPE)} · Node: ${shaped(input.nodeVersion, VERSION_SHAPE)} · System: ${shaped(input.os, SYSTEM_SHAPE)} ${shaped(input.arch, SYSTEM_SHAPE)}`,
    `State: ${copy.state[input.context]}`,
  ];
  if (input.title) lines.push(`ARC says: ${input.title}`);
  if (input.error) {
    const shared = sharePayload({ raw: input.error, privacy: input.privacy });
    lines.push(shared.detailed ? `Details: ${shared.text}` : shared.text);
  }
  return lines.join("\n");
}

const timeLabel = (ms: number) => new Date(ms).toISOString().replace("T", " ").slice(0, 16) + " UTC";

/**
 * One honest line about Node Care, the app's automatic report path: whether a
 * problem report actually left this computer, is still queued, or was never
 * sent (reports off, or a build with no report address).
 */
export function describeReportState(snapshot: Pick<NodeCareSnapshot, "ready" | "enabled" | "endpointConfigured" | "recent">): {
  kind: "loading" | "off" | "not-configured" | "none" | "sent" | "queued";
  text: string;
} {
  const copy = PLAIN_COPY.help.report;
  if (!snapshot.ready) return { kind: "loading", text: copy.loading };
  if (!snapshot.enabled) return { kind: "off", text: copy.off };
  if (!snapshot.endpointConfigured) return { kind: "not-configured", text: copy.notConfigured };
  const latest = snapshot.recent.find((r) => r.kind === "report");
  if (!latest) return { kind: "none", text: copy.none };
  if (latest.delivery === "sent") return { kind: "sent", text: copy.sent(timeLabel(latest.at)) };
  if (latest.delivery === "queued") return { kind: "queued", text: copy.queued(timeLabel(latest.at)) };
  return { kind: "not-configured", text: copy.notConfigured };
}
