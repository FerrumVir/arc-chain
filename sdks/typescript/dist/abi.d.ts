export declare function encodeAbi(types: string[], values: any[]): Uint8Array;
export declare function decodeAbi(types: string[], data: Uint8Array): any[];
export declare function keccak256(data: Uint8Array): Uint8Array;
export declare function functionSelector(signature: string): Uint8Array;
export declare function encodeFunctionCall(signature: string, ...args: any[]): Uint8Array;
export declare function decodeFunctionResult(types: string[], data: Uint8Array): any[];
export declare function decodeFunctionInput(signature: string, data: Uint8Array): [string, any[]];
//# sourceMappingURL=abi.d.ts.map