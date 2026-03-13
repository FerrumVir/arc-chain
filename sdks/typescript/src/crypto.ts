/**
 * ARC Chain SDK — Cryptographic primitives.
 *
 * Ed25519 key pair generation, signing, verification, and BLAKE3 address
 * derivation using @noble/ed25519 and @noble/hashes.
 */

import * as ed from "@noble/ed25519";
import { blake3 } from "@noble/hashes/blake3";
import { bytesToHex, hexToBytes } from "@noble/hashes/utils";

/**
 * Ed25519 key pair for ARC Chain transaction signing.
 *
 * Addresses are derived as the BLAKE3 hash of the Ed25519 public key,
 * matching the Rust implementation.
 */
export class KeyPair {
  private readonly _privateKey: Uint8Array;
  private readonly _publicKey: Uint8Array;

  private constructor(privateKey: Uint8Array, publicKey: Uint8Array) {
    this._privateKey = privateKey;
    this._publicKey = publicKey;
  }

  // -- Constructors --

  /**
   * Generate a random Ed25519 key pair.
   */
  static async generate(): Promise<KeyPair> {
    const privateKey = ed.utils.randomPrivateKey();
    const publicKey = await ed.getPublicKeyAsync(privateKey);
    return new KeyPair(privateKey, publicKey);
  }

  /**
   * Create a deterministic key pair from a 32-byte seed.
   */
  static async fromSeed(seed: Uint8Array): Promise<KeyPair> {
    if (seed.length !== 32) {
      throw new Error(`Seed must be exactly 32 bytes, got ${seed.length}`);
    }
    const publicKey = await ed.getPublicKeyAsync(seed);
    return new KeyPair(seed, publicKey);
  }

  /**
   * Import from a hex-encoded 32-byte private key.
   */
  static async fromPrivateKeyHex(hex: string): Promise<KeyPair> {
    const seed = hexToBytes(hex);
    return KeyPair.fromSeed(seed);
  }

  // -- Signing --

  /**
   * Sign a message and return the 64-byte Ed25519 signature.
   */
  async sign(message: Uint8Array): Promise<Uint8Array> {
    return ed.signAsync(message, this._privateKey);
  }

  /**
   * Verify a signature against a message using this key pair's public key.
   */
  async verify(message: Uint8Array, signature: Uint8Array): Promise<boolean> {
    try {
      return await ed.verifyAsync(signature, message, this._publicKey);
    } catch {
      return false;
    }
  }

  /**
   * Verify a signature given a raw public key (static, no key pair needed).
   */
  static async verifyWithPublicKey(
    publicKey: Uint8Array,
    message: Uint8Array,
    signature: Uint8Array
  ): Promise<boolean> {
    try {
      return await ed.verifyAsync(signature, message, publicKey);
    } catch {
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
  address(): string {
    const digest = blake3(this._publicKey);
    return bytesToHex(digest);
  }

  /**
   * Return the 32-byte public key as a 64-character hex string.
   */
  publicKeyHex(): string {
    return bytesToHex(this._publicKey);
  }

  /**
   * Return the raw 32-byte public key.
   */
  publicKeyBytes(): Uint8Array {
    return this._publicKey;
  }

  /**
   * Return the 32-byte private key (seed) as a 64-character hex string.
   */
  privateKeyHex(): string {
    return bytesToHex(this._privateKey);
  }
}
