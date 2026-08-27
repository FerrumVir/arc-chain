/**
 * Human labels for the six testnet seeds.
 *
 * Shared so that every screen naming a host names it the same way. The seeds
 * are independent chains, so "which host" is load-bearing context for any
 * number, not decoration — a height or a balance is only meaningful next to
 * the host it was read from (CLAUDE.md rule 4).
 */
export const SEED_LABELS: Record<string, string> = {
  "149.28.32.76": "NYC",
  "140.82.16.112": "LAX",
  "136.244.109.1": "AMS",
  "104.238.171.11": "LHR",
  "202.182.107.41": "NRT",
  "149.28.153.31": "SGP",
};

/**
 * HTTPS seed origins use nip.io names because the desktop talks to them over
 * TLS. Keep those aliases tied to the exact configured IPs: a custom nip.io
 * host must not inherit a city merely because its text happens to resemble a
 * seed address.
 */
const SEED_ORIGIN_LABELS: Record<string, string> = Object.fromEntries(
  Object.entries(SEED_LABELS).flatMap(([ip, label]) => [
    [ip, label],
    [`${ip.replaceAll(".", "-")}.nip.io`, label],
  ]),
);

function hostnameOf(origin: string): string | null {
  try {
    return new URL(
      origin.includes("://") ? origin : `http://${origin}`,
    ).hostname.toLowerCase();
  } catch {
    return null;
  }
}

/**
 * A short label for a host URL: the seed's city when we recognise it, else the
 * bare host:port. Never a guess — an unrecognised host is shown verbatim so a
 * local devnet or a custom `ARC_WALLET_HOST` is still identifiable.
 */
export function hostLabel(url: string | null | undefined): string {
  if (!url) return "unknown host";
  const hostname = hostnameOf(url);
  if (hostname && SEED_ORIGIN_LABELS[hostname]) {
    return SEED_ORIGIN_LABELS[hostname];
  }
  return url.replace(/^https?:\/\//, "");
}

/** `hostLabel` plus the raw origin, for tooltips: "LAX (140.82.16.112:9090)". */
export function hostLabelVerbose(url: string | null | undefined): string {
  if (!url) return "unknown host";
  const short = hostLabel(url);
  const bare = url.replace(/^https?:\/\//, "");
  return short === bare ? bare : `${short} (${bare})`;
}
