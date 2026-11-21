export declare class KeyPair {
    private readonly _privateKey;
    private readonly _publicKey;
    private constructor();
    static generate(): Promise<KeyPair>;
    static fromSeed(seed: Uint8Array): Promise<KeyPair>;
    static fromPrivateKeyHex(hex: string): Promise<KeyPair>;
    sign(message: Uint8Array): Promise<Uint8Array>;
    verify(message: Uint8Array, signature: Uint8Array): Promise<boolean>;
    static verifyWithPublicKey(publicKey: Uint8Array, message: Uint8Array, signature: Uint8Array): Promise<boolean>;
    address(): string;
    publicKeyHex(): string;
    publicKeyBytes(): Uint8Array;
    privateKeyHex(): string;
}
//# sourceMappingURL=crypto.d.ts.map