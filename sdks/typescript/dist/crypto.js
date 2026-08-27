"use strict";
/**
 * ARC Chain SDK - Cryptographic primitives.
 *
 * Ed25519 key pair generation, signing, verification, and BLAKE3 address
 * derivation using @noble/ed25519 and @noble/hashes.
 */
Object.defineProperty(exports, "__esModule", { value: true });
exports.KeyPair = void 0;
const ed25519_js_1 = require("@noble/curves/ed25519.js");
const blake3_1 = require("@noble/hashes/blake3");
const utils_1 = require("@noble/hashes/utils");
/**
 * Ed25519 key pair for ARC Chain transaction signing.
 *
 * Addresses are derived as the BLAKE3 hash of the Ed25519 public key,
 * matching the Rust implementation.
 */
class KeyPair {
    constructor(privateKey, publicKey) {
        this._privateKey = privateKey;
        this._publicKey = publicKey;
    }
    // -- Constructors --
    /**
     * Generate a random Ed25519 key pair.
     */
    static async generate() {
        const privateKey = ed25519_js_1.ed25519.utils.randomSecretKey();
        const publicKey = ed25519_js_1.ed25519.getPublicKey(privateKey);
        return new KeyPair(privateKey, publicKey);
    }
    /**
     * Create a deterministic key pair from a 32-byte seed.
     */
    static async fromSeed(seed) {
        if (seed.length !== 32) {
            throw new Error(`Seed must be exactly 32 bytes, got ${seed.length}`);
        }
        const publicKey = ed25519_js_1.ed25519.getPublicKey(seed);
        return new KeyPair(seed, publicKey);
    }
    /**
     * Import from a hex-encoded 32-byte private key.
     */
    static async fromPrivateKeyHex(hex) {
        const seed = (0, utils_1.hexToBytes)(hex);
        return KeyPair.fromSeed(seed);
    }
    // -- Signing --
    /**
     * Sign a message and return the 64-byte Ed25519 signature.
     */
    async sign(message) {
        return ed25519_js_1.ed25519.sign(message, this._privateKey);
    }
    /**
     * Verify a signature against a message using this key pair's public key.
     */
    async verify(message, signature) {
        try {
            return ed25519_js_1.ed25519.verify(signature, message, this._publicKey);
        }
        catch {
            return false;
        }
    }
    /**
     * Verify a signature given a raw public key (static, no key pair needed).
     */
    static async verifyWithPublicKey(publicKey, message, signature) {
        try {
            return ed25519_js_1.ed25519.verify(signature, message, publicKey);
        }
        catch {
            return false;
        }
    }
    // -- Address derivation --
    /**
     * Derive the ARC Chain address from the public key.
     *
     * The address is the BLAKE3 hash of the 32-byte Ed25519 public key,
     * returned as a 64-character lowercase hex string.
     */
    address() {
        const digest = (0, blake3_1.blake3)(this._publicKey);
        return (0, utils_1.bytesToHex)(digest);
    }
    /**
     * Return the 32-byte public key as a 64-character hex string.
     */
    publicKeyHex() {
        return (0, utils_1.bytesToHex)(this._publicKey);
    }
    /**
     * Return the raw 32-byte public key.
     */
    publicKeyBytes() {
        return this._publicKey;
    }
    /**
     * Return the 32-byte private key (seed) as a 64-character hex string.
     */
    privateKeyHex() {
        return (0, utils_1.bytesToHex)(this._privateKey);
    }
}
exports.KeyPair = KeyPair;
//# sourceMappingURL=crypto.js.map