// ARC Chain — Client-side BLAKE3 + Merkle proof verification engine
//
// Matches Rust hashing EXACTLY:
//   - Tx hash:    blake3::new_derive_key("ARC-chain-tx-v1")    + pre_hash bytes
//   - Block hash: blake3::new_derive_key("ARC-chain-block-v1") + pre_hash bytes
//   - Merkle:     blake3::new()                                 + left(32) + right(32)
//
// Uses @noble/hashes — pure JS, zero WASM, audited by Ethereum Foundation.
// No async loading required. Works in SSR and browser identically.

import { blake3 } from "@noble/hashes/blake3.js";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface VerificationBundle {
  /** Hex-encoded pre-hash bytes (what Rust feeds into the hasher) */
  preHashHex: string;
  /** Hex-encoded expected hash output */
  claimedHash: string;
  /** Domain separation context string (e.g. "ARC-chain-tx-v1") */
  domain: string;
}

export interface MerkleVerificationBundle {
  /** Hex-encoded leaf hash (the tx hash sitting at `index`) */
  leafHash: string;
  /** Zero-based index of the leaf in the tree */
  index: number;
  /** Sibling hashes from leaf to root, with position flags */
  siblings: { hash: string; isLeft: boolean }[];
  /** Hex-encoded expected Merkle root */
  expectedRoot: string;
}

export interface VerificationResult {
  valid: boolean;
  computedHash: string;
  claimedHash: string;
  /** Verification wall-clock time in milliseconds */
  timeMs: number;
}

export interface MerkleVerificationResult {
  valid: boolean;
  computedRoot: string;
  expectedRoot: string;
  pathLength: number;
  /** Verification wall-clock time in milliseconds */
  timeMs: number;
}

// ---------------------------------------------------------------------------
// Tx type → byte mapping (must match Rust TxType discriminant)
// ---------------------------------------------------------------------------

export const TX_TYPE_BYTE: Record<string, number> = {
  Transfer: 0x01,
  Settle:   0x02,
  Swap:     0x03,
  Escrow:   0x04,
  Stake:    0x05,
  WasmCall: 0x06,
  MultiSig: 0x07,
};

// ---------------------------------------------------------------------------
// Byte / hex utilities
// ---------------------------------------------------------------------------

const HEX_CHARS = "0123456789abcdef";

/**
 * Decode a hex string (with or without "0x" prefix) into a Uint8Array.
 * Throws on invalid input.
 */
export function hexToBytes(hex: string): Uint8Array {
  const cleaned = hex.startsWith("0x") || hex.startsWith("0X")
    ? hex.slice(2)
    : hex;

  if (cleaned.length % 2 !== 0) {
    throw new Error(`hexToBytes: odd-length hex string (${cleaned.length} chars)`);
  }
  if (!/^[0-9a-fA-F]*$/.test(cleaned)) {
    throw new Error("hexToBytes: non-hex characters in input");
  }

  const bytes = new Uint8Array(cleaned.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(cleaned.substring(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

/**
 * Encode a Uint8Array into a lowercase hex string (no "0x" prefix).
 */
export function bytesToHex(bytes: Uint8Array): string {
  let hex = "";
  for (let i = 0; i < bytes.length; i++) {
    hex += HEX_CHARS[bytes[i] >> 4];
    hex += HEX_CHARS[bytes[i] & 0x0f];
  }
  return hex;
}

/**
 * Concatenate multiple Uint8Arrays into a single contiguous buffer.
 */
export function concatBytes(...arrays: Uint8Array[]): Uint8Array {
  let totalLen = 0;
  for (const arr of arrays) totalLen += arr.length;
  const result = new Uint8Array(totalLen);
  let offset = 0;
  for (const arr of arrays) {
    result.set(arr, offset);
    offset += arr.length;
  }
  return result;
}

/**
 * Encode a u64 as 8 bytes in little-endian order (matches Rust's `.to_le_bytes()`).
 * Accepts `bigint` for full 64-bit range.
 */
export function uint64ToLeBytes(value: bigint): Uint8Array {
  if (value < BigInt(0)) {
    throw new Error("uint64ToLeBytes: value must be non-negative");
  }
  if (value > BigInt("18446744073709551615")) {
    throw new Error("uint64ToLeBytes: value exceeds u64 range");
  }
  const buf = new Uint8Array(8);
  let v = value;
  for (let i = 0; i < 8; i++) {
    buf[i] = Number(v & BigInt(0xFF));
    v >>= BigInt(8);
  }
  return buf;
}

// ---------------------------------------------------------------------------
// Internal hashing primitives (synchronous — pure JS, no WASM)
// ---------------------------------------------------------------------------

/**
 * Domain-separated BLAKE3 hash (matches Rust `Hasher::new_derive_key(ctx)`).
 *
 * @noble/hashes blake3() with `{ context }` option = derive_key mode.
 */
const encoder = new TextEncoder();

function blake3DeriveKey(domain: string, data: Uint8Array): Uint8Array {
  return blake3(data, { context: encoder.encode(domain) });
}

/**
 * Plain BLAKE3 hash (matches Rust `Hasher::new()`).
 */
function blake3Hash(data: Uint8Array): Uint8Array {
  return blake3(data);
}

// ---------------------------------------------------------------------------
// Public API — hash computation
// ---------------------------------------------------------------------------

/**
 * Compute a domain-separated BLAKE3 hash from raw pre-hash bytes.
 *
 * This matches the Rust path where the RPC provides `pre_hash_hex` and a domain
 * string, and we simply run `blake3::new_derive_key(domain).update(pre_hash_bytes).finalize()`.
 *
 * @param preHashHex  Hex-encoded raw bytes to hash
 * @param domain      Domain separation string (e.g. "ARC-chain-tx-v1")
 * @returns           Lowercase hex hash (64 chars, no "0x" prefix)
 */
export function computeBlake3Hash(
  preHashHex: string,
  domain: string,
): string {
  const data = hexToBytes(preHashHex);
  const digest = blake3DeriveKey(domain, data);
  return bytesToHex(digest);
}

/**
 * Compute the BLAKE3 hash of a CompactTransfer from its individual fields.
 *
 * Matches the Rust layout:
 *   `[tx_type:1] + [from:32] + [nonce:8 LE] + [to:32] + [amount:8 LE]`
 * with domain `"ARC-chain-tx-v1"`.
 *
 * @param txType   Transaction type name (e.g. "Transfer")
 * @param fromHex  Sender address as 32-byte hex
 * @param nonce    Sender nonce
 * @param toHex    Recipient address as 32-byte hex
 * @param amount   Transfer amount in smallest units
 * @returns        Lowercase hex hash (64 chars, no "0x" prefix)
 */
export function computeCompactTransferHash(
  txType: string,
  fromHex: string,
  nonce: bigint,
  toHex: string,
  amount: bigint,
): string {
  const typeByte = TX_TYPE_BYTE[txType];
  if (typeByte === undefined) {
    throw new Error(`Unknown transaction type: "${txType}"`);
  }

  const fromBytes = hexToBytes(fromHex);
  const toBytes = hexToBytes(toHex);

  if (fromBytes.length !== 32) {
    throw new Error(`"from" address must be 32 bytes, got ${fromBytes.length}`);
  }
  if (toBytes.length !== 32) {
    throw new Error(`"to" address must be 32 bytes, got ${toBytes.length}`);
  }

  const preImage = concatBytes(
    new Uint8Array([typeByte]),  // tx_type as u8  — 1 byte
    fromBytes,                   // from           — 32 bytes
    uint64ToLeBytes(nonce),      // nonce (LE)     — 8 bytes
    toBytes,                     // to             — 32 bytes
    uint64ToLeBytes(amount),     // amount (LE)    — 8 bytes
  );
  // Total: 81 bytes

  const digest = blake3DeriveKey("ARC-chain-tx-v1", preImage);
  return bytesToHex(digest);
}

// ---------------------------------------------------------------------------
// Public API — verification
// ---------------------------------------------------------------------------

/**
 * Verify a domain-separated BLAKE3 hash against a claimed value.
 *
 * Works for both full transaction hashes and block hashes — the caller
 * provides the appropriate `domain` string via the bundle.
 *
 * @param bundle  Contains preHashHex, claimedHash, and domain
 * @returns       Verification result with timing
 */
export function verifyBlake3Hash(
  bundle: VerificationBundle,
): VerificationResult {
  const t0 = performance.now();
  const computedHash = computeBlake3Hash(bundle.preHashHex, bundle.domain);
  const t1 = performance.now();

  const claimedNorm = normalizeHex(bundle.claimedHash);

  return {
    valid: computedHash === claimedNorm,
    computedHash,
    claimedHash: claimedNorm,
    timeMs: round3(t1 - t0),
  };
}

/**
 * Verify a CompactTransfer hash by recomputing it from individual fields.
 *
 * This is the "full transparency" path where the explorer reconstructs the
 * pre-image from display fields and checks it matches the on-chain hash.
 *
 * @param txType      Transaction type name
 * @param fromHex     Sender address (32-byte hex)
 * @param nonce       Sender nonce
 * @param toHex       Recipient address (32-byte hex)
 * @param amount      Transfer amount (smallest units)
 * @param claimedHash Expected hash to verify against
 * @returns           Verification result with timing
 */
export function verifyCompactTransferHash(
  txType: string,
  fromHex: string,
  nonce: bigint,
  toHex: string,
  amount: bigint,
  claimedHash: string,
): VerificationResult {
  const t0 = performance.now();
  const computedHash = computeCompactTransferHash(
    txType,
    fromHex,
    nonce,
    toHex,
    amount,
  );
  const t1 = performance.now();

  const claimedNorm = normalizeHex(claimedHash);

  return {
    valid: computedHash === claimedNorm,
    computedHash,
    claimedHash: claimedNorm,
    timeMs: round3(t1 - t0),
  };
}

/**
 * Verify a Merkle inclusion proof.
 *
 * Walks from the leaf hash up to the root, combining sibling hashes at each
 * level using plain BLAKE3 (NO domain separation), matching the Rust Merkle
 * tree implementation:
 *
 *   `blake3::Hasher::new().update(left).update(right).finalize()`
 *
 * Sibling ordering is determined by the `isLeft` flag on each sibling:
 *   - `isLeft: true`  → sibling is the LEFT child, our current hash is RIGHT
 *   - `isLeft: false` → sibling is the RIGHT child, our current hash is LEFT
 *
 * @param bundle  Leaf hash, sibling path, index, and expected root
 * @returns       Verification result with timing and path length
 */
export function verifyMerkleProof(
  bundle: MerkleVerificationBundle,
): MerkleVerificationResult {
  const t0 = performance.now();

  let current = hexToBytes(bundle.leafHash);

  if (current.length !== 32) {
    throw new Error(`Leaf hash must be 32 bytes, got ${current.length}`);
  }

  for (const sibling of bundle.siblings) {
    const siblingBytes = hexToBytes(sibling.hash);
    if (siblingBytes.length !== 32) {
      throw new Error(`Sibling hash must be 32 bytes, got ${siblingBytes.length}`);
    }

    // Combine: the sibling tells us its position.
    // If sibling isLeft, then pair = [sibling | current].
    // If sibling is right, then pair = [current | sibling].
    const pair = sibling.isLeft
      ? concatBytes(siblingBytes, current)
      : concatBytes(current, siblingBytes);

    current = blake3Hash(pair);
  }

  const t1 = performance.now();

  const computedRoot = bytesToHex(current);
  const expectedNorm = normalizeHex(bundle.expectedRoot);

  return {
    valid: computedRoot === expectedNorm,
    computedRoot,
    expectedRoot: expectedNorm,
    pathLength: bundle.siblings.length,
    timeMs: round3(t1 - t0),
  };
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/**
 * Normalize a hex string to lowercase without "0x" prefix.
 */
function normalizeHex(hex: string): string {
  const stripped = hex.startsWith("0x") || hex.startsWith("0X")
    ? hex.slice(2)
    : hex;
  return stripped.toLowerCase();
}

/**
 * Round a number to 3 decimal places (for sub-ms timing precision).
 */
function round3(n: number): number {
  return Math.round(n * 1000) / 1000;
}
