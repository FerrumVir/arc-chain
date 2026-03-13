// ─── @arc-chain/sdk — Utility Functions ───────────────────────
// Address validation, hash formatting, hex encoding/decoding.
// Pure functions with zero dependencies.

/**
 * ARC Chain addresses are 64-character hex strings (32 bytes, BLAKE3).
 * No `0x` prefix on the native API.
 */
const ADDRESS_LENGTH = 64;

/**
 * ARC Chain transaction hashes are 64-character hex strings (32 bytes, BLAKE3).
 */
const TX_HASH_LENGTH = 64;

/** Regex: 64 hex characters, case-insensitive. */
const HEX_64_RE = /^[0-9a-fA-F]{64}$/;

// ─── Validation ─────────────────────────────────────────────

/**
 * Check whether a string is a valid ARC Chain address.
 *
 * A valid address is exactly 64 lowercase/uppercase hex characters.
 * The `0x` prefix is stripped before validation if present.
 *
 * @example
 * ```ts
 * isValidAddress("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262");
 * // true
 *
 * isValidAddress("0xaf1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262");
 * // true (0x prefix is tolerated)
 *
 * isValidAddress("not-an-address");
 * // false
 * ```
 */
export function isValidAddress(addr: string): boolean {
  const clean = addr.startsWith("0x") || addr.startsWith("0X")
    ? addr.slice(2)
    : addr;
  return clean.length === ADDRESS_LENGTH && HEX_64_RE.test(clean);
}

/**
 * Check whether a string is a valid ARC Chain transaction hash.
 *
 * Same format as addresses — 64 hex characters (32-byte BLAKE3).
 *
 * @example
 * ```ts
 * isValidTxHash("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2");
 * // true
 * ```
 */
export function isValidTxHash(hash: string): boolean {
  const clean = hash.startsWith("0x") || hash.startsWith("0X")
    ? hash.slice(2)
    : hash;
  return clean.length === TX_HASH_LENGTH && HEX_64_RE.test(clean);
}

// ─── Formatting ─────────────────────────────────────────────

/**
 * Truncate a 64-char hex hash for display: `"a1b2c3...f6a1b2"`.
 *
 * @param hash - Full hex hash string.
 * @param prefixLen - Characters to keep at the start (default 8).
 * @param suffixLen - Characters to keep at the end (default 6).
 *
 * @example
 * ```ts
 * formatHash("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262");
 * // "af1349b9...e41f3262"
 * ```
 */
export function formatHash(
  hash: string,
  prefixLen: number = 8,
  suffixLen: number = 8,
): string {
  const clean = hash.startsWith("0x") || hash.startsWith("0X")
    ? hash.slice(2)
    : hash;

  if (clean.length <= prefixLen + suffixLen) {
    return clean;
  }

  return `${clean.slice(0, prefixLen)}...${clean.slice(-suffixLen)}`;
}

/**
 * Format a raw balance (smallest unit) to a human-readable ARC amount.
 *
 * ARC uses 6 decimal places by default.
 *
 * @param amount - Raw balance as a number.
 * @param decimals - Decimal places (default 6).
 *
 * @example
 * ```ts
 * formatArc(1_000_000); // "1.000000"
 * formatArc(500);       // "0.000500"
 * ```
 */
export function formatArc(amount: number, decimals: number = 6): string {
  const divisor = 10 ** decimals;
  const whole = Math.floor(amount / divisor);
  const frac = amount % divisor;
  return `${whole}.${String(frac).padStart(decimals, "0")}`;
}

// ─── Hex Encoding ───────────────────────────────────────────

/**
 * Convert a hex string to a `Uint8Array`.
 *
 * Accepts both `0x`-prefixed and raw hex strings.
 *
 * @throws {Error} if the input has an odd length or contains non-hex characters.
 *
 * @example
 * ```ts
 * hexToBytes("deadbeef"); // Uint8Array [0xde, 0xad, 0xbe, 0xef]
 * hexToBytes("0xCAFE");   // Uint8Array [0xca, 0xfe]
 * ```
 */
export function hexToBytes(hex: string): Uint8Array {
  const clean = hex.startsWith("0x") || hex.startsWith("0X")
    ? hex.slice(2)
    : hex;

  if (clean.length % 2 !== 0) {
    throw new Error(`Invalid hex string: odd length (${clean.length})`);
  }

  if (!/^[0-9a-fA-F]*$/.test(clean)) {
    throw new Error("Invalid hex string: contains non-hex characters");
  }

  const bytes = new Uint8Array(clean.length / 2);
  for (let i = 0; i < clean.length; i += 2) {
    bytes[i / 2] = parseInt(clean.substring(i, i + 2), 16);
  }
  return bytes;
}

/**
 * Convert a `Uint8Array` to a lowercase hex string (no `0x` prefix).
 *
 * @example
 * ```ts
 * bytesToHex(new Uint8Array([0xde, 0xad, 0xbe, 0xef])); // "deadbeef"
 * ```
 */
export function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/**
 * Strip the `0x` prefix from a hex string if present.
 * Returns the string unchanged if no prefix.
 */
export function stripHexPrefix(hex: string): string {
  if (hex.startsWith("0x") || hex.startsWith("0X")) {
    return hex.slice(2);
  }
  return hex;
}

/**
 * Ensure a hex string has a `0x` prefix.
 * Returns the string unchanged if already prefixed.
 */
export function addHexPrefix(hex: string): string {
  if (hex.startsWith("0x") || hex.startsWith("0X")) {
    return hex;
  }
  return `0x${hex}`;
}
