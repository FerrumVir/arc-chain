export function truncateHash(hash: string, chars: number = 6): string {
  if (hash.length <= chars * 2 + 2) return hash;
  return `${hash.slice(0, chars + 2)}...${hash.slice(-chars)}`;
}

export function formatNumber(n: number): string {
  if (n >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(2)}B`;
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toLocaleString();
}

export function formatTPS(tps: number): string {
  if (tps >= 1_000_000) return `${(tps / 1_000_000).toFixed(1)}M`;
  if (tps >= 1_000) return `${(tps / 1_000).toFixed(0)}K`;
  return tps.toString();
}

export function formatARC(amount: number): string {
  return `${amount.toLocaleString(undefined, { maximumFractionDigits: 2 })} ARC`;
}

export function timeAgo(timestamp: number): string {
  const seconds = Math.floor((Date.now() - timestamp) / 1000);
  if (seconds < 5) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

export function txTypeColor(txType: string): string {
  const colors: Record<string, string> = {
    Transfer: "text-[var(--shield-green)]",
    Settle: "text-[var(--accent)]",
    Swap: "text-[var(--accent-soft)]",
    Escrow: "text-[var(--shield-yellow)]",
    Stake: "text-[var(--accent-mid)]",
    WasmCall: "text-[var(--code-highlight-2)]",
    MultiSig: "text-[var(--code-highlight-1)]",
  };
  return colors[txType] || "text-[var(--text-secondary)]";
}

export function txTypeBg(txType: string): string {
  const colors: Record<string, string> = {
    Transfer: "bg-[var(--shield-green-bg)]",
    Settle: "bg-[var(--accent-light)]",
    Swap: "bg-[var(--accent-light)]",
    Escrow: "bg-[var(--shield-yellow-bg)]",
    Stake: "bg-[var(--accent-light)]",
    WasmCall: "bg-[var(--accent-light)]",
    MultiSig: "bg-[var(--accent-light)]",
  };
  return colors[txType] || "bg-[var(--bg-secondary)]";
}
