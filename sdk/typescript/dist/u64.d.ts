export type U64 = number | bigint;
export declare const U64_MAX: bigint;
export declare function u64ToBigInt(value: U64, fieldName: string): bigint;
export declare function assertU64(value: U64, fieldName: string): void;
export declare function stringifyJsonWithBigInts(value: unknown): string;
export declare function parseJsonWithBigInts<T>(text: string): T;
//# sourceMappingURL=u64.d.ts.map