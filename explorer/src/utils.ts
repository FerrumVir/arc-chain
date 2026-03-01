/**
 * Truncate a hex hash for display: 0x1234...abcd
 */
export function truncateHash(hash: string, prefixLen = 6, suffixLen = 4): string {
  if (!hash) return '';
  const clean = hash.startsWith('0x') ? hash : `0x${hash}`;
  if (clean.length <= prefixLen + suffixLen + 2) return clean;
  return `${clean.slice(0, prefixLen + 2)}...${clean.slice(-suffixLen)}`;
}

/**
 * Format a full hash with 0x prefix
 */
export function formatHash(hash: string): string {
  if (!hash) return '';
  return hash.startsWith('0x') ? hash : `0x${hash}`;
}

/**
 * Relative time string from a Unix timestamp (seconds)
 */
export function timeAgo(timestamp: number): string {
  if (!timestamp || timestamp === 0) return 'Genesis';
  const now = Math.floor(Date.now() / 1000);
  const diff = now - timestamp;

  if (diff < 0) return 'just now';
  if (diff < 5) return 'just now';
  if (diff < 60) return `${diff}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  if (diff < 2592000) return `${Math.floor(diff / 86400)}d ago`;
  return new Date(timestamp * 1000).toLocaleDateString();
}

/**
 * Format a Unix timestamp to locale string
 */
export function formatTimestamp(timestamp: number): string {
  if (!timestamp) return 'N/A';
  return new Date(timestamp * 1000).toLocaleString();
}

/**
 * Format a number with commas
 */
export function formatNumber(n: number): string {
  if (n === undefined || n === null) return '0';
  return n.toLocaleString();
}

/**
 * Detect search input type
 */
export function detectSearchType(
  input: string
): 'block' | 'tx' | 'account' | 'unknown' {
  const trimmed = input.trim();

  // Pure number = block height
  if (/^\d+$/.test(trimmed)) {
    return 'block';
  }

  // Remove 0x prefix for hex check
  const hex = trimmed.startsWith('0x') ? trimmed.slice(2) : trimmed;

  // 64-char hex = transaction hash
  if (/^[0-9a-fA-F]{64}$/.test(hex)) {
    return 'tx';
  }

  // Shorter hex strings = account address
  if (/^[0-9a-fA-F]{8,}$/.test(hex)) {
    return 'account';
  }

  return 'unknown';
}

/**
 * Copy text to clipboard
 */
export async function copyToClipboard(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}
