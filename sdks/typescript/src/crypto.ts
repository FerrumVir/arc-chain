import { ed25519 } from "@noble/curves/ed25519.js";
import { blake3 } from "@noble/hashes/blake3";
import { bytesToHex, hexToBytes } from "@noble/hashes/utils";

export class KeyPair {
  private readonly _privateKey: Uint8Array;
  private readonly _publicKey: Uint8Array;

  private constructor(privateKey: Uint8Array, publicKey: Uint8Array) {
    this._privateKey = privateKey;
    this._publicKey = publicKey;
  }

  static async generate(): Promise<KeyPair> {
    const privateKey = ed25519.utils.randomSecretKey();
    const publicKey = ed25519.getPublicKey(privateKey);
    return new KeyPair(privateKey, publicKey);
  }

  static async fromSeed(seed: Uint8Array): Promise<KeyPair> {
    if (seed.length !== 32) {
      throw new Error(`Seed must be exactly 32 bytes, got ${seed.length}`);
    }
    const publicKey = ed25519.getPublicKey(seed);
    return new KeyPair(seed, publicKey);
  }

  static async fromPrivateKeyHex(hex: string): Promise<KeyPair> {
    const seed = hexToBytes(hex);
    return KeyPair.fromSeed(seed);
  }

  async sign(message: Uint8Array): Promise<Uint8Array> {
    return ed25519.sign(message, this._privateKey);
  }

  async verify(message: Uint8Array, signature: Uint8Array): Promise<boolean> {
    try {
      return ed25519.verify(signature, message, this._publicKey);
    } catch {
      return false;
    }
  }

  static async verifyWithPublicKey(
    publicKey: Uint8Array,
    message: Uint8Array,
    signature: Uint8Array
  ): Promise<boolean> {
    try {
      return ed25519.verify(signature, message, publicKey);
    } catch {
      return false;
    }
  }

  address(): string {
    const digest = blake3(this._publicKey);
    return bytesToHex(digest);
  }

  publicKeyHex(): string {
    return bytesToHex(this._publicKey);
  }

  publicKeyBytes(): Uint8Array {
    return this._publicKey;
  }

  privateKeyHex(): string {
    return bytesToHex(this._privateKey);
  }
}
