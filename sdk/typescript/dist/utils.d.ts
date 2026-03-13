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
export declare function isValidAddress(addr: string): boolean;
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
export declare function isValidTxHash(hash: string): boolean;
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
export declare function formatHash(hash: string, prefixLen?: number, suffixLen?: number): string;
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
export declare function formatArc(amount: number, decimals?: number): string;
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
export declare function hexToBytes(hex: string): Uint8Array;
/**
 * Convert a `Uint8Array` to a lowercase hex string (no `0x` prefix).
 *
 * @example
 * ```ts
 * bytesToHex(new Uint8Array([0xde, 0xad, 0xbe, 0xef])); // "deadbeef"
 * ```
 */
export declare function bytesToHex(bytes: Uint8Array): string;
/**
 * Strip the `0x` prefix from a hex string if present.
 * Returns the string unchanged if no prefix.
 */
export declare function stripHexPrefix(hex: string): string;
/**
 * Ensure a hex string has a `0x` prefix.
 * Returns the string unchanged if already prefixed.
 */
export declare function addHexPrefix(hex: string): string;
//# sourceMappingURL=utils.d.ts.map