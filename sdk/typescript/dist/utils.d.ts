import type { U64 } from "./u64.js";
export declare function isValidAddress(addr: string): boolean;
export declare function isValidTxHash(hash: string): boolean;
export declare function formatHash(hash: string, prefixLen?: number, suffixLen?: number): string;
export declare function formatArc(amount: U64, decimals?: number): string;
export declare function hexToBytes(hex: string): Uint8Array;
export declare function bytesToHex(bytes: Uint8Array): string;
export declare function stripHexPrefix(hex: string): string;
export declare function addHexPrefix(hex: string): string;
//# sourceMappingURL=utils.d.ts.map