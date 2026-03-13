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
export declare function encodeAbi(types: string[], values: any[]): Uint8Array;
/**
 * Decode ABI-encoded data according to the given types.
 *
 * @param types - ABI type strings matching the encoded data layout
 * @param data - The ABI-encoded bytes (without a function selector)
 * @returns Array of decoded values
 */
export declare function decodeAbi(types: string[], data: Uint8Array): any[];
/**
 * Compute the Keccak-256 hash.
 *
 * @param data - Bytes to hash
 * @returns 32-byte digest
 */
export declare function keccak256(data: Uint8Array): Uint8Array;
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
export declare function functionSelector(signature: string): Uint8Array;
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
export declare function encodeFunctionCall(signature: string, ...args: any[]): Uint8Array;
/**
 * Decode the return data from a function call.
 *
 * Convenience alias for {@link decodeAbi}.
 *
 * @param types - ABI type strings of the return values
 * @param data - Raw bytes returned by the function call
 * @returns Array of decoded values
 */
export declare function decodeFunctionResult(types: string[], data: Uint8Array): any[];
/**
 * Decode calldata (selector + arguments) given the expected signature.
 *
 * @param signature - Canonical signature, e.g. `"transfer(address,uint256)"`
 * @param data - Full calldata including the 4-byte selector
 * @returns Tuple of [functionName, decodedArgs]
 */
export declare function decodeFunctionInput(signature: string, data: Uint8Array): [string, any[]];
//# sourceMappingURL=abi.d.ts.map