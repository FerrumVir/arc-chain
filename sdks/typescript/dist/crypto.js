"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.KeyPair = void 0;
const ed25519_js_1 = require("@noble/curves/ed25519.js");
const blake3_1 = require("@noble/hashes/blake3");
const utils_1 = require("@noble/hashes/utils");
class KeyPair {
    constructor(privateKey, publicKey) {
        this._privateKey = privateKey;
        this._publicKey = publicKey;
    }
    static async generate() {
        const privateKey = ed25519_js_1.ed25519.utils.randomSecretKey();
        const publicKey = ed25519_js_1.ed25519.getPublicKey(privateKey);
        return new KeyPair(privateKey, publicKey);
    }
    static async fromSeed(seed) {
        if (seed.length !== 32) {
            throw new Error(`Seed must be exactly 32 bytes, got ${seed.length}`);
        }
        const publicKey = ed25519_js_1.ed25519.getPublicKey(seed);
        return new KeyPair(seed, publicKey);
    }
    static async fromPrivateKeyHex(hex) {
        const seed = (0, utils_1.hexToBytes)(hex);
        return KeyPair.fromSeed(seed);
    }
    async sign(message) {
        return ed25519_js_1.ed25519.sign(message, this._privateKey);
    }
    async verify(message, signature) {
        try {
            return ed25519_js_1.ed25519.verify(signature, message, this._publicKey);
        }
        catch {
            return false;
        }
    }
    static async verifyWithPublicKey(publicKey, message, signature) {
        try {
            return ed25519_js_1.ed25519.verify(signature, message, publicKey);
        }
        catch {
            return false;
        }
    }
    address() {
        const digest = (0, blake3_1.blake3)(this._publicKey);
        return (0, utils_1.bytesToHex)(digest);
    }
    publicKeyHex() {
        return (0, utils_1.bytesToHex)(this._publicKey);
    }
    publicKeyBytes() {
        return this._publicKey;
    }
    privateKeyHex() {
        return (0, utils_1.bytesToHex)(this._privateKey);
    }
}
exports.KeyPair = KeyPair;
//# sourceMappingURL=crypto.js.map