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

export interface SupportDetailsInput {
  context: HelpContext;
  appVersion: string | null;
  nodeVersion: string | null;
  os: string | null;
  arch: string | null;
  /** The plain sentence the app shows, if any. */
  title?: string | null;
  /** The native error text, if any. Scrubbed here before it is shown or copied. */
  error?: string | null;
  homeDir?: string | null;
}

/**
 * The text "Copy details" puts on the clipboard. Only what helps someone
 * answer: versions, system, what the app shows and the native text, with home
 * folders, emails, hosts and keys scrubbed the same way Node Care reports are.
 */
export function buildSupportDetails(input: SupportDetailsInput): string {
  const copy = PLAIN_COPY.help;
  const value = (v: string | null | undefined) => (v && v.trim() ? v.trim() : "unknown");
  const lines = [
    copy.detailsHeader,
    `App: ${value(input.appVersion)} · Node: ${value(input.nodeVersion)} · System: ${value(input.os)} ${value(input.arch)}`,
    `State: ${copy.state[input.context]}`,
  ];
  if (input.title) lines.push(`ARC says: ${input.title}`);
  if (input.error) lines.push(`Details: ${scrubMessage(input.error, input.homeDir)}`);
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
