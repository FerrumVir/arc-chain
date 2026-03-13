/**
 * ARC Chain SDK — Cryptographic primitives.
 *
 * Ed25519 key pair generation, signing, verification, and BLAKE3 address
 * derivation using @noble/ed25519 and @noble/hashes.
 */
/**
 * Ed25519 key pair for ARC Chain transaction signing.
 *
 * Addresses are derived as the BLAKE3 hash of the Ed25519 public key,
 * matching the Rust implementation.
 */
export declare class KeyPair {
    private readonly _privateKey;
    private readonly _publicKey;
    private constructor();
    /**
     * Generate a random Ed25519 key pair.
     */
    static generate(): Promise<KeyPair>;
    /**
     * Create a deterministic key pair from a 32-byte seed.
     */
    static fromSeed(seed: Uint8Array): Promise<KeyPair>;
    /**
     * Import from a hex-encoded 32-byte private key.
     */
    static fromPrivateKeyHex(hex: string): Promise<KeyPair>;
    /**
     * Sign a message and return the 64-byte Ed25519 signature.
     */
    sign(message: Uint8Array): Promise<Uint8Array>;
    /**
     * Verify a signature against a message using this key pair's public key.
     */
    verify(message: Uint8Array, signature: Uint8Array): Promise<boolean>;
    /**
     * Verify a signature given a raw public key (static, no key pair needed).
     */
    static verifyWithPublicKey(publicKey: Uint8Array, message: Uint8Array, signature: Uint8Array): Promise<boolean>;
    /**
     * Derive the ARC Chain address from the public key.
     *
     * The address is the BLAKE3 hash of the 32-byte Ed25519 public key,
     * returned as a 64-character lowercase hex string.
     */
    address(): string;
    /**
     * Return the 32-byte public key as a 64-character hex string.
     */
    publicKeyHex(): string;
    /**
     * Return the raw 32-byte public key.
     */
    publicKeyBytes(): Uint8Array;
    /**
     * Return the 32-byte private key (seed) as a 64-character hex string.
     */
    privateKeyHex(): string;
}
//# sourceMappingURL=crypto.d.ts.map