/**
 * ARC Chain SDK -- Ethereum-standard ABI encoding/decoding.
 *
 * Provides functions for encoding and decoding function calls using the
 * Ethereum ABI specification, including function selectors via Keccak-256.
 *
 * @example
 * ```ts
 * import { encodeFunctionCall, decodeAbi } from "@arc-chain/sdk";
 *
 * const calldata = encodeFunctionCall(
 *   "transfer(address,uint256)",
 *   "0xdead000000000000000000000000000000000000",
 *   1000n,
 * );
 *
 * const [amount, flag] = decodeAbi(["uint256", "bool"], returnData);
 * ```
 */

import { keccak_256 } from "@noble/hashes/sha3";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const WORD_SIZE = 32;
const ZERO_WORD = new Uint8Array(WORD_SIZE);

/** Regex matchers for ABI types */
const UINT_RE = /^uint(\d+)$/;
const INT_RE = /^int(\d+)$/;
const BYTES_FIXED_RE = /^bytes(\d+)$/;
const ARRAY_FIXED_RE = /^(.+)\[(\d+)\]$/;
const ARRAY_DYN_RE = /^(.+)\[\]$/;
const TUPLE_RE = /^\((.+)\)$/;

/**
 * Concatenate multiple Uint8Arrays into one.
 */
function concat(...arrays: Uint8Array[]): Uint8Array {
  const total = arrays.reduce((sum, a) => sum + a.length, 0);
  const result = new Uint8Array(total);
  let offset = 0;
  for (const arr of arrays) {
    result.set(arr, offset);
    offset += arr.length;
  }
  return result;
}

/**
 * Convert a hex string (with or without 0x prefix) to Uint8Array.
 */
function hexToBytes(hex: string): Uint8Array {
  hex = hex.replace(/^0x/i, "");
  if (hex.length % 2 !== 0) hex = "0" + hex;
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(hex.substring(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

/**
 * Convert Uint8Array to hex string (no 0x prefix).
 */
function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/**
 * Create a 32-byte word from a bigint (unsigned, big-endian, left-padded).
 */
function uint256ToBytes(value: bigint): Uint8Array {
  const result = new Uint8Array(WORD_SIZE);
  let v = value;
  for (let i = WORD_SIZE - 1; i >= 0; i--) {
    result[i] = Number(v & 0xffn);
    v >>= 8n;
  }
  return result;
}

/**
 * Read a bigint from 32 bytes (big-endian, unsigned).
 */
function bytesToUint256(data: Uint8Array, offset: number): bigint {
  let val = 0n;
  for (let i = 0; i < WORD_SIZE; i++) {
    val = (val << 8n) | BigInt(data[offset + i]);
  }
  return val;
}

/**
 * Read a bigint from 32 bytes (big-endian, signed two's complement).
 */
function bytesToInt256(data: Uint8Array, offset: number): bigint {
  const unsigned = bytesToUint256(data, offset);
  const MAX_INT256 = (1n << 255n) - 1n;
  if (unsigned > MAX_INT256) {
    return unsigned - (1n << 256n);
  }
  return unsigned;
}

/**
 * Convert a value to bigint, accepting number, string, or bigint.
 */
function toBigInt(value: any): bigint {
  if (typeof value === "bigint") return value;
  if (typeof value === "number") return BigInt(value);
  if (typeof value === "string") {
    if (value.startsWith("0x") || value.startsWith("0X")) {
      return BigInt(value);
    }
    return BigInt(value);
  }
  throw new Error(`Cannot convert ${typeof value} to bigint`);
}

// ---------------------------------------------------------------------------
// Type parsing
// ---------------------------------------------------------------------------

/**
 * Split comma-separated types inside a tuple, respecting nesting depth.
 */
function splitTupleTypes(inner: string): string[] {
  const result: string[] = [];
  let depth = 0;
  let current = "";
  for (const ch of inner) {
    if (ch === "(") {
      depth++;
      current += ch;
    } else if (ch === ")") {
      depth--;
      current += ch;
    } else if (ch === "," && depth === 0) {
      result.push(current.trim());
      current = "";
    } else {
      current += ch;
    }
  }
  if (current.trim()) {
    result.push(current.trim());
  }
  return result;
}

/**
 * Parse a canonical function signature into (name, paramTypes).
 */
function parseSignature(sig: string): [string, string[]] {
  const parenIdx = sig.indexOf("(");
  const name = sig.substring(0, parenIdx).trim();
  const inner = sig.substring(parenIdx + 1, sig.length - 1).trim();
  if (!inner) return [name, []];
  return [name, splitTupleTypes(inner)];
}

/**
 * Check whether an ABI type is dynamically sized.
 */
function isDynamic(typ: string): boolean {
  if (typ === "bytes" || typ === "string") return true;

  const dynArr = ARRAY_DYN_RE.exec(typ);
  if (dynArr) return true;

  const fixArr = ARRAY_FIXED_RE.exec(typ);
  if (fixArr) return isDynamic(fixArr[1]);

  const tuple = TUPLE_RE.exec(typ);
  if (tuple) {
    const subtypes = splitTupleTypes(tuple[1]);
    return subtypes.some(isDynamic);
  }

  return false;
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

function encodeUint(value: any, bits: number): Uint8Array {
  const v = toBigInt(value);
  if (v < 0n) throw new Error(`uint${bits} cannot be negative, got ${v}`);
  const maxVal = (1n << BigInt(bits)) - 1n;
  if (v > maxVal)
    throw new Error(`Value ${v} exceeds uint${bits} max (${maxVal})`);
  return uint256ToBytes(v);
}

function encodeInt(value: any, bits: number): Uint8Array {
  const v = toBigInt(value);
  const half = 1n << BigInt(bits - 1);
  if (v < -half || v >= half) {
    throw new Error(
      `Value ${v} out of range for int${bits} [${-half}, ${half - 1n}]`
    );
  }
  if (v >= 0n) return uint256ToBytes(v);
  // Two's complement
  return uint256ToBytes((1n << 256n) + v);
}

function encodeAddress(value: any): Uint8Array {
  let addrBytes: Uint8Array;
  if (typeof value === "string") {
    addrBytes = hexToBytes(value);
  } else if (value instanceof Uint8Array) {
    addrBytes = value;
  } else {
    throw new Error(`Cannot encode address from ${typeof value}`);
  }
  if (addrBytes.length > 20) {
    throw new Error(
      `Address must be <= 20 bytes, got ${addrBytes.length}`
    );
  }
  // Left-pad to 32 bytes
  const result = new Uint8Array(WORD_SIZE);
  result.set(addrBytes, WORD_SIZE - addrBytes.length);
  return result;
}

function encodeBool(value: any): Uint8Array {
  return uint256ToBytes(value ? 1n : 0n);
}

function encodeBytesFixed(value: Uint8Array | string, size: number): Uint8Array {
  let bytes: Uint8Array;
  if (typeof value === "string") {
    bytes = hexToBytes(value);
  } else {
    bytes = value;
  }
  if (bytes.length !== size) {
    throw new Error(
      `bytes${size} requires exactly ${size} bytes, got ${bytes.length}`
    );
  }
  // Right-pad to 32 bytes
  const result = new Uint8Array(WORD_SIZE);
  result.set(bytes, 0);
  return result;
}

function encodeBytesDynamic(value: Uint8Array | string): Uint8Array {
  let bytes: Uint8Array;
  if (typeof value === "string") {
    bytes = hexToBytes(value);
  } else {
    bytes = value;
  }
  const lengthWord = uint256ToBytes(BigInt(bytes.length));
  const paddedLen = Math.ceil(bytes.length / 32) * 32;
  const padded = new Uint8Array(paddedLen);
  padded.set(bytes, 0);
  return concat(lengthWord, padded);
}

function encodeString(value: string): Uint8Array {
  const encoder = new TextEncoder();
  const bytes = encoder.encode(value);
  return encodeBytesDynamic(bytes);
}

/**
 * Encode a single value for a given ABI type.
 * For dynamic types, returns the complete tail data (length + padded content).
 */
function encodeSingle(typ: string, value: any): Uint8Array {
  if (typ === "address") return encodeAddress(value);
  if (typ === "bool") return encodeBool(value);
  if (typ === "string") return encodeString(value);
  if (typ === "bytes") return encodeBytesDynamic(value);

  let m = UINT_RE.exec(typ);
  if (m) return encodeUint(value, parseInt(m[1], 10));

  m = INT_RE.exec(typ);
  if (m) return encodeInt(value, parseInt(m[1], 10));

  m = BYTES_FIXED_RE.exec(typ);
  if (m) return encodeBytesFixed(value, parseInt(m[1], 10));

  // Fixed-size array: T[N]
  m = ARRAY_FIXED_RE.exec(typ);
  if (m) {
    const innerType = m[1];
    const n = parseInt(m[2], 10);
    const arr = value as any[];
    if (arr.length !== n) {
      throw new Error(
        `${typ} requires exactly ${n} elements, got ${arr.length}`
      );
    }
    return encodeArrayContents(innerType, arr);
  }

  // Dynamic array: T[]
  m = ARRAY_DYN_RE.exec(typ);
  if (m) {
    const innerType = m[1];
    const arr = value as any[];
    const lengthWord = uint256ToBytes(BigInt(arr.length));
    return concat(lengthWord, encodeArrayContents(innerType, arr));
  }

  // Tuple: (T1,T2,...)
  m = TUPLE_RE.exec(typ);
  if (m) {
    const subtypes = splitTupleTypes(m[1]);
    return encodeAbi(subtypes, value as any[]);
  }

  throw new Error(`Unsupported ABI type: ${typ}`);
}

function encodeArrayContents(innerType: string, items: any[]): Uint8Array {
  const types = items.map(() => innerType);
  return encodeAbi(types, items);
}

/**
 * Encode a list of values according to the given ABI types.
 *
 * Follows Ethereum ABI encoding rules: static types are encoded inline
 * in the head section; dynamic types get an offset pointer in the head
 * pointing to their data in the tail section.
 *
 * @param types - ABI type strings, e.g. `["address", "uint256", "bytes"]`
 * @param values - Corresponding values to encode
 * @returns ABI-encoded bytes
 */
export function encodeAbi(types: string[], values: any[]): Uint8Array {
  if (types.length !== values.length) {
    throw new Error(
      `types/values length mismatch: ${types.length} vs ${values.length}`
    );
  }

  const dynamicFlags = types.map(isDynamic);
  const headSize = WORD_SIZE * types.length;

  const headParts: Uint8Array[] = [];
  const tailParts: Uint8Array[] = [];

  for (let i = 0; i < types.length; i++) {
    if (dynamicFlags[i]) {
      const encodedData = encodeSingle(types[i], values[i]);
      const tailOffset =
        headSize + tailParts.reduce((sum, t) => sum + t.length, 0);
      headParts.push(uint256ToBytes(BigInt(tailOffset)));
      tailParts.push(encodedData);
    } else {
      headParts.push(encodeSingle(types[i], values[i]));
    }
  }

  return concat(...headParts, ...tailParts);
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

interface DecodeResult {
  value: any;
  nextOffset: number;
}

function decodeUint(
  data: Uint8Array,
  offset: number,
  bits: number
): DecodeResult {
  const val = bytesToUint256(data, offset);
  const mask = (1n << BigInt(bits)) - 1n;
  return { value: val & mask, nextOffset: offset + WORD_SIZE };
}

function decodeInt(
  data: Uint8Array,
  offset: number,
  bits: number
): DecodeResult {
  const raw = bytesToInt256(data, offset);
  const half = 1n << BigInt(bits - 1);
  const mod = 1n << BigInt(bits);
  let val = ((raw % mod) + mod) % mod;
  if (val >= half) val -= mod;
  return { value: val, nextOffset: offset + WORD_SIZE };
}

function decodeAddress(data: Uint8Array, offset: number): DecodeResult {
  const word = data.slice(offset, offset + WORD_SIZE);
  const addr = "0x" + bytesToHex(word.slice(12));
  return { value: addr, nextOffset: offset + WORD_SIZE };
}

function decodeBool(data: Uint8Array, offset: number): DecodeResult {
  const val = bytesToUint256(data, offset);
  return { value: val !== 0n, nextOffset: offset + WORD_SIZE };
}

function decodeBytesFixed(
  data: Uint8Array,
  offset: number,
  size: number
): DecodeResult {
  const result = data.slice(offset, offset + size);
  return { value: result, nextOffset: offset + WORD_SIZE };
}

function decodeBytesDynamic(
  data: Uint8Array,
  offset: number
): DecodeResult {
  const length = Number(bytesToUint256(data, offset));
  const start = offset + WORD_SIZE;
  const raw = data.slice(start, start + length);
  const paddedLen = Math.ceil(length / 32) * 32;
  return { value: raw, nextOffset: start + paddedLen };
}

function decodeString(data: Uint8Array, offset: number): DecodeResult {
  const { value: raw, nextOffset } = decodeBytesDynamic(data, offset);
  const decoder = new TextDecoder();
  return { value: decoder.decode(raw), nextOffset };
}

function decodeSingle(
  typ: string,
  data: Uint8Array,
  offset: number,
  baseOffset: number
): DecodeResult {
  if (typ === "address") return decodeAddress(data, offset);
  if (typ === "bool") return decodeBool(data, offset);

  let m = UINT_RE.exec(typ);
  if (m) return decodeUint(data, offset, parseInt(m[1], 10));

  m = INT_RE.exec(typ);
  if (m) return decodeInt(data, offset, parseInt(m[1], 10));

  m = BYTES_FIXED_RE.exec(typ);
  if (m) return decodeBytesFixed(data, offset, parseInt(m[1], 10));

  // Dynamic types: head word is an offset pointer
  if (typ === "string") {
    const tailOffset = Number(bytesToUint256(data, offset));
    const { value } = decodeString(data, baseOffset + tailOffset);
    return { value, nextOffset: offset + WORD_SIZE };
  }

  if (typ === "bytes") {
    const tailOffset = Number(bytesToUint256(data, offset));
    const { value } = decodeBytesDynamic(data, baseOffset + tailOffset);
    return { value, nextOffset: offset + WORD_SIZE };
  }

  // Dynamic array: T[]
  m = ARRAY_DYN_RE.exec(typ);
  if (m) {
    const innerType = m[1];
    const tailOffset = Number(bytesToUint256(data, offset));
    const absOffset = baseOffset + tailOffset;
    const count = Number(bytesToUint256(data, absOffset));
    const items = decodeArrayContents(
      innerType,
      count,
      data,
      absOffset + WORD_SIZE,
      absOffset + WORD_SIZE
    );
    return { value: items, nextOffset: offset + WORD_SIZE };
  }

  // Fixed array: T[N]
  m = ARRAY_FIXED_RE.exec(typ);
  if (m) {
    const innerType = m[1];
    const n = parseInt(m[2], 10);
    if (isDynamic(innerType)) {
      const tailOffset = Number(bytesToUint256(data, offset));
      const absOffset = baseOffset + tailOffset;
      const items = decodeArrayContents(
        innerType,
        n,
        data,
        absOffset,
        absOffset
      );
      return { value: items, nextOffset: offset + WORD_SIZE };
    } else {
      const items = decodeArrayContents(
        innerType,
        n,
        data,
        offset,
        baseOffset
      );
      return { value: items, nextOffset: offset + WORD_SIZE * n };
    }
  }

  // Tuple: (T1,T2,...)
  m = TUPLE_RE.exec(typ);
  if (m) {
    const subtypes = splitTupleTypes(m[1]);
    const isTupleDynamic = subtypes.some(isDynamic);
    if (isTupleDynamic) {
      const tailOffset = Number(bytesToUint256(data, offset));
      const absOffset = baseOffset + tailOffset;
      const vals = decodeTuple(subtypes, data, absOffset, absOffset);
      return { value: vals, nextOffset: offset + WORD_SIZE };
    } else {
      const vals = decodeTuple(subtypes, data, offset, baseOffset);
      return {
        value: vals,
        nextOffset: offset + WORD_SIZE * subtypes.length,
      };
    }
  }

  throw new Error(`Unsupported ABI type: ${typ}`);
}

function decodeArrayContents(
  innerType: string,
  count: number,
  data: Uint8Array,
  offset: number,
  baseOffset: number
): any[] {
  const results: any[] = [];
  let cursor = offset;
  for (let i = 0; i < count; i++) {
    const { value, nextOffset } = decodeSingle(
      innerType,
      data,
      cursor,
      baseOffset
    );
    results.push(value);
    cursor = nextOffset;
  }
  return results;
}

function decodeTuple(
  types: string[],
  data: Uint8Array,
  offset: number,
  baseOffset: number
): any[] {
  const results: any[] = [];
  let cursor = offset;
  for (const typ of types) {
    const { value, nextOffset } = decodeSingle(typ, data, cursor, baseOffset);
    results.push(value);
    cursor = nextOffset;
  }
  return results;
}

/**
 * Decode ABI-encoded data according to the given types.
 *
 * @param types - ABI type strings matching the encoded data layout
 * @param data - The ABI-encoded bytes (without a function selector)
 * @returns Array of decoded values
 */
export function decodeAbi(types: string[], data: Uint8Array): any[] {
  return decodeTuple(types, data, 0, 0);
}

// ---------------------------------------------------------------------------
// Function selectors and call encoding
// ---------------------------------------------------------------------------

/**
 * Compute the Keccak-256 hash.
 *
 * @param data - Bytes to hash
 * @returns 32-byte digest
 */
export function keccak256(data: Uint8Array): Uint8Array {
  return keccak_256(data);
}

/**
 * Compute the 4-byte function selector for a canonical signature.
 *
 * The selector is the first 4 bytes of the Keccak-256 hash of the
 * canonical function signature (no spaces, no parameter names).
 *
 * @param signature - Canonical signature, e.g. `"transfer(address,uint256)"`
 * @returns 4-byte selector
 *
 * @example
 * ```ts
 * const sel = functionSelector("transfer(address,uint256)");
 * // sel.hex() === "a9059cbb"
 * ```
 */
export function functionSelector(signature: string): Uint8Array {
  const encoder = new TextEncoder();
  const hash = keccak256(encoder.encode(signature));
  return hash.slice(0, 4);
}

/**
 * Encode a complete function call (selector + ABI-encoded arguments).
 *
 * @param signature - Canonical function signature, e.g. `"transfer(address,uint256)"`
 * @param args - Argument values matching the parameter types in the signature
 * @returns 4-byte selector followed by ABI-encoded arguments
 *
 * @example
 * ```ts
 * const data = encodeFunctionCall(
 *   "transfer(address,uint256)",
 *   "0xdead000000000000000000000000000000000000",
 *   1000n,
 * );
 * ```
 */
export function encodeFunctionCall(
  signature: string,
  ...args: any[]
): Uint8Array {
  const [_name, paramTypes] = parseSignature(signature);
  if (args.length !== paramTypes.length) {
    throw new Error(
      `Signature ${signature} expects ${paramTypes.length} args, got ${args.length}`
    );
  }
  const selector = functionSelector(signature);
  const encodedArgs = encodeAbi(paramTypes, args);
  return concat(selector, encodedArgs);
}

/**
 * Decode the return data from a function call.
 *
 * Convenience alias for {@link decodeAbi}.
 *
 * @param types - ABI type strings of the return values
 * @param data - Raw bytes returned by the function call
 * @returns Array of decoded values
 */
export function decodeFunctionResult(
  types: string[],
  data: Uint8Array
): any[] {
  return decodeAbi(types, data);
}

/**
 * Decode calldata (selector + arguments) given the expected signature.
 *
 * @param signature - Canonical signature, e.g. `"transfer(address,uint256)"`
 * @param data - Full calldata including the 4-byte selector
 * @returns Tuple of [functionName, decodedArgs]
 */
export function decodeFunctionInput(
  signature: string,
  data: Uint8Array
): [string, any[]] {
  const expectedSelector = functionSelector(signature);
  const actualSelector = data.slice(0, 4);
  if (bytesToHex(expectedSelector) !== bytesToHex(actualSelector)) {
    throw new Error(
      `Selector mismatch: expected ${bytesToHex(expectedSelector)}, got ${bytesToHex(actualSelector)}`
    );
  }
  const [name, paramTypes] = parseSignature(signature);
  const decoded = decodeAbi(paramTypes, data.slice(4));
  return [name, decoded];
}
