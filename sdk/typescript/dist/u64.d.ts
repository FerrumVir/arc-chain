/**
 * A Rust `u64` represented without precision loss.
 *
 * Existing callers may keep using `number` within JavaScript's safe-integer
 * range. Values above that range must be supplied as `bigint`.
 */
export type U64 = number | bigint;
/** Largest value accepted by Rust's `u64`. */
export declare const U64_MAX: bigint;
/** Convert a public u64 input to its exact bigint representation. */
export declare function u64ToBigInt(value: U64, fieldName: string): bigint;
/** Fail closed before an inexact or out-of-range u64 reaches the network. */
export declare function assertU64(value: U64, fieldName: string): void;
/**
 * JSON.stringify cannot encode bigint. This serializer emits bigint values as
 * exact, unquoted JSON integer tokens, which serde_json's Rust `u64` decoder
 * accepts. Other JSON values retain JSON.stringify-compatible behavior.
 */
export declare function stringifyJsonWithBigInts(value: unknown): string;
/**
 * Parse JSON while reading integer tokens directly from the response text.
 * This stays lossless on Node 18 and browsers that do not provide the newer
 * `JSON.parse` reviver `context.source` argument.
 */
export declare function parseJsonWithBigInts<T>(text: string): T;
//# sourceMappingURL=u64.d.ts.map