export const SEED_LABELS: Record<string, string> = {
  "149.28.32.76": "NYC",
  "140.82.16.112": "LAX",
  "136.244.109.1": "AMS",
  "104.238.171.11": "LHR",
  "202.182.107.41": "NRT",
  "149.28.153.31": "SGP",
};

const SEED_ORIGIN_LABELS: Record<string, string> = SEED_LABELS;

function hostnameOf(origin: string): string | null {
  try {
    return new URL(
      origin.includes("://") ? origin : `http://${origin}`,
    ).hostname.toLowerCase();
  } catch {
    return null;
  }
}

export function hostLabel(url: string | null | undefined): string {
  if (!url) return "unknown host";
  const hostname = hostnameOf(url);
  if (hostname && SEED_ORIGIN_LABELS[hostname]) {
    return SEED_ORIGIN_LABELS[hostname];
  }
  return url.replace(/^https?:\/\//, "");
}

export function hostLabelVerbose(url: string | null | undefined): string {
  if (!url) return "unknown host";
  const short = hostLabel(url);
  const bare = url.replace(/^https?:\/\//, "");
  return short === bare ? bare : `${short} (${bare})`;
}
