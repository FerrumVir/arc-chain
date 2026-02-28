// ─── ARC Chain Wallet ─────────────────────────────────────────
// Key management, transaction signing, nonce tracking

import type {
  Address,
  Hash256,
  UnsignedTransaction,
  SignedTransaction,
  TxType,
} from "./types";
import { ArcClient } from "./client";

/** Simple in-memory wallet for agents */
export class ArcWallet {
  private _address: Address;
  private _privateKey: Uint8Array;
  private _nonce: number = 0;
  private _nonceInitialized: boolean = false;

  private constructor(address: Address, privateKey: Uint8Array) {
    this._address = address;
    this._privateKey = privateKey;
  }

  /** Create a new random wallet */
  static create(): ArcWallet {
    const privateKey = new Uint8Array(32);
    if (typeof crypto !== "undefined" && crypto.getRandomValues) {
      crypto.getRandomValues(privateKey);
    } else {
      for (let i = 0; i < 32; i++) {
        privateKey[i] = Math.floor(Math.random() * 256);
      }
    }

    // Derive address from private key (simplified — BLAKE3 hash, take first 20 bytes)
    const address = deriveAddress(privateKey);
    return new ArcWallet(address, privateKey);
  }

  /** Import wallet from hex private key */
  static fromPrivateKey(hexKey: string): ArcWallet {
    const clean = hexKey.startsWith("0x") ? hexKey.slice(2) : hexKey;
    const privateKey = hexToBytes(clean);
    const address = deriveAddress(privateKey);
    return new ArcWallet(address, privateKey);
  }

  /** Get wallet address */
  get address(): Address {
    return this._address;
  }

  /** Export private key as hex */
  get privateKeyHex(): string {
    return "0x" + bytesToHex(this._privateKey);
  }

  /** Sync nonce from chain */
  async syncNonce(client: ArcClient): Promise<void> {
    this._nonce = await client.getNonce(this._address);
    this._nonceInitialized = true;
  }

  /** Get current nonce (auto-incrementing) */
  get nonce(): number {
    return this._nonce;
  }

  /** Sign a transaction */
  sign(tx: UnsignedTransaction): SignedTransaction {
    // Compute transaction hash (simplified BLAKE3 — full impl needs WASM)
    const txBytes = serializeTx(tx);
    const hash = simpleHash(txBytes);

    // Sign with private key (simplified — real impl uses Ed25519 or ECDSA)
    const signature = simpleSign(hash, this._privateKey);

    const signed: SignedTransaction = {
      ...tx,
      hash: "0x" + bytesToHex(hash),
      signature: "0x" + bytesToHex(signature),
    };

    // Auto-increment nonce
    this._nonce++;

    return signed;
  }

  /** Create and sign a transfer transaction */
  transfer(to: Address, amount: bigint): SignedTransaction {
    return this.sign({
      tx_type: "Transfer" as TxType,
      from: this._address,
      to,
      amount,
      nonce: this._nonce,
    });
  }

  /** Create and sign a settle transaction (agent settlement) */
  settle(to: Address, amount: bigint, data?: Uint8Array): SignedTransaction {
    return this.sign({
      tx_type: "Settle" as TxType,
      from: this._address,
      to,
      amount,
      nonce: this._nonce,
      data,
    });
  }

  /** Create and sign a swap transaction */
  swap(to: Address, amount: bigint): SignedTransaction {
    return this.sign({
      tx_type: "Swap" as TxType,
      from: this._address,
      to,
      amount,
      nonce: this._nonce,
    });
  }

  /** Create and sign a stake transaction */
  stake(amount: bigint): SignedTransaction {
    return this.sign({
      tx_type: "Stake" as TxType,
      from: this._address,
      to: this._address,
      amount,
      nonce: this._nonce,
    });
  }

  /** Create and sign a WASM contract call */
  callContract(
    contract: Address,
    data: Uint8Array,
    gasLimit: number = 1_000_000
  ): SignedTransaction {
    return this.sign({
      tx_type: "WasmCall" as TxType,
      from: this._address,
      to: contract,
      amount: 0n,
      nonce: this._nonce,
      data,
      gas_limit: gasLimit,
    });
  }
}

// ─── Helpers ────────────────────────────────────────────────

function deriveAddress(privateKey: Uint8Array): Address {
  // Simplified address derivation — hash private key, take first 20 bytes
  const hash = simpleHash(privateKey);
  return "0x" + bytesToHex(hash.slice(0, 20));
}

function serializeTx(tx: UnsignedTransaction): Uint8Array {
  const encoder = new TextEncoder();
  const parts = [
    encoder.encode(tx.tx_type),
    hexToBytes(tx.from.slice(2)),
    hexToBytes(tx.to.slice(2)),
    bigintToBytes(tx.amount),
    new Uint8Array([tx.nonce & 0xff, (tx.nonce >> 8) & 0xff, (tx.nonce >> 16) & 0xff, (tx.nonce >> 24) & 0xff]),
  ];
  if (tx.data) parts.push(tx.data);

  const totalLen = parts.reduce((sum, p) => sum + p.length, 0);
  const result = new Uint8Array(totalLen);
  let offset = 0;
  for (const part of parts) {
    result.set(part, offset);
    offset += part.length;
  }
  return result;
}

/** Simplified BLAKE3-like hash (for SDK without WASM dependency) */
function simpleHash(data: Uint8Array): Uint8Array {
  const hash = new Uint8Array(32);
  // FNV-1a style mixing (placeholder — real SDK will use WASM BLAKE3)
  let h = 0x811c9dc5;
  for (let i = 0; i < data.length; i++) {
    h ^= data[i];
    h = Math.imul(h, 0x01000193);
  }
  for (let i = 0; i < 32; i++) {
    h ^= data[i % data.length] ^ i;
    h = Math.imul(h, 0x01000193);
    hash[i] = h & 0xff;
  }
  return hash;
}

/** Simplified signing (placeholder — real SDK will use Ed25519) */
function simpleSign(hash: Uint8Array, key: Uint8Array): Uint8Array {
  const sig = new Uint8Array(64);
  for (let i = 0; i < 64; i++) {
    sig[i] = hash[i % 32] ^ key[i % 32] ^ (i & 0xff);
  }
  return sig;
}

function hexToBytes(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < hex.length; i += 2) {
    bytes[i / 2] = parseInt(hex.substr(i, 2), 16);
  }
  return bytes;
}

function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

function bigintToBytes(n: bigint): Uint8Array {
  const bytes: number[] = [];
  let val = n < 0n ? -n : n;
  if (val === 0n) return new Uint8Array([0]);
  while (val > 0n) {
    bytes.push(Number(val & 0xffn));
    val >>= 8n;
  }
  return new Uint8Array(bytes);
}
