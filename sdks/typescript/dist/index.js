"use strict";
/**
 * ARC Chain TypeScript SDK.
 *
 * A complete TypeScript client for interacting with the ARC Chain blockchain,
 * including transaction building, Ed25519 signing, and RPC communication.
 *
 * @example
 * ```ts
 * import { ArcClient, KeyPair, TransactionBuilder } from "@arc-chain/sdk";
 *
 * const client = new ArcClient("http://localhost:9000");
 * const kp = await KeyPair.generate();
 *
 * const tx = TransactionBuilder.transfer(kp.address(), "0".repeat(64), 1000);
 * const signed = await TransactionBuilder.sign(tx, kp);
 * const hash = await client.submitTransaction(signed);
 * ```
 */
Object.defineProperty(exports, "__esModule", { value: true });
exports.keccak256 = exports.functionSelector = exports.decodeFunctionInput = exports.decodeFunctionResult = exports.encodeFunctionCall = exports.decodeAbi = exports.encodeAbi = exports.TransactionBuilder = exports.KeyPair = exports.ArcTransactionError = exports.ArcConnectionError = exports.ArcError = exports.ArcClient = void 0;
var client_1 = require("./client");
Object.defineProperty(exports, "ArcClient", { enumerable: true, get: function () { return client_1.ArcClient; } });
Object.defineProperty(exports, "ArcError", { enumerable: true, get: function () { return client_1.ArcError; } });
Object.defineProperty(exports, "ArcConnectionError", { enumerable: true, get: function () { return client_1.ArcConnectionError; } });
Object.defineProperty(exports, "ArcTransactionError", { enumerable: true, get: function () { return client_1.ArcTransactionError; } });
var crypto_1 = require("./crypto");
Object.defineProperty(exports, "KeyPair", { enumerable: true, get: function () { return crypto_1.KeyPair; } });
var transaction_1 = require("./transaction");
Object.defineProperty(exports, "TransactionBuilder", { enumerable: true, get: function () { return transaction_1.TransactionBuilder; } });
var abi_1 = require("./abi");
Object.defineProperty(exports, "encodeAbi", { enumerable: true, get: function () { return abi_1.encodeAbi; } });
Object.defineProperty(exports, "decodeAbi", { enumerable: true, get: function () { return abi_1.decodeAbi; } });
Object.defineProperty(exports, "encodeFunctionCall", { enumerable: true, get: function () { return abi_1.encodeFunctionCall; } });
Object.defineProperty(exports, "decodeFunctionResult", { enumerable: true, get: function () { return abi_1.decodeFunctionResult; } });
Object.defineProperty(exports, "decodeFunctionInput", { enumerable: true, get: function () { return abi_1.decodeFunctionInput; } });
Object.defineProperty(exports, "functionSelector", { enumerable: true, get: function () { return abi_1.functionSelector; } });
Object.defineProperty(exports, "keccak256", { enumerable: true, get: function () { return abi_1.keccak256; } });
//# sourceMappingURL=index.js.map