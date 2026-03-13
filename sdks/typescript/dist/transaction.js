"use strict";
/**
 * ARC Chain SDK — Transaction builder.
 *
 * Constructs unsigned transaction objects matching the ARC Chain RPC format,
 * then signs them with Ed25519 and computes the BLAKE3 transaction hash.
 */
Object.defineProperty(exports, "__esModule", { value: true });
exports.TransactionBuilder = void 0;
const blake3_1 = require("@noble/hashes/blake3");
const utils_1 = require("@noble/hashes/utils");
/** Domain separation context matching the Rust implementation. */
const TX_DOMAIN = "ARC-chain-tx-v1";
// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------
/** Encode a u64 as 8 little-endian bytes. */
function encodeU64(value) {
    const buf = new ArrayBuffer(8);
    const view = new DataView(buf);
    // JS numbers are safe up to 2^53; for blockchain amounts this is fine.
    view.setUint32(0, value & 0xffffffff, true);
    view.setUint32(4, Math.floor(value / 0x100000000) & 0xffffffff, true);
    return new Uint8Array(buf);
}
/** Concatenate multiple Uint8Arrays. */
function concat(...arrays) {
    let totalLen = 0;
    for (const a of arrays)
        totalLen += a.length;
    const result = new Uint8Array(totalLen);
    let offset = 0;
    for (const a of arrays) {
        result.set(a, offset);
        offset += a.length;
    }
    return result;
}
/** Encode a transaction body to bytes for hashing. */
function encodeBody(body) {
    const parts = [];
    switch (body.type) {
        case "Transfer": {
            parts.push(new Uint8Array([0x00])); // variant tag
            parts.push((0, utils_1.hexToBytes)(body.to));
            parts.push(encodeU64(body.amount));
            // amount_commitment: Option<[u8;32]>
            if (body.amount_commitment) {
                parts.push(new Uint8Array([0x01]));
                parts.push((0, utils_1.hexToBytes)(body.amount_commitment));
            }
            else {
                parts.push(new Uint8Array([0x00]));
            }
            break;
        }
        case "DeployContract": {
            parts.push(new Uint8Array([0x07]));
            const code = (0, utils_1.hexToBytes)(body.bytecode);
            parts.push(encodeU64(code.length));
            parts.push(code);
            const ctor = (0, utils_1.hexToBytes)(body.constructor_args);
            parts.push(encodeU64(ctor.length));
            parts.push(ctor);
            parts.push(encodeU64(body.state_rent_deposit));
            break;
        }
        case "WasmCall": {
            parts.push(new Uint8Array([0x05]));
            parts.push((0, utils_1.hexToBytes)(body.contract));
            const func = new TextEncoder().encode(body.function);
            parts.push(encodeU64(func.length));
            parts.push(func);
            const calldata = (0, utils_1.hexToBytes)(body.calldata);
            parts.push(encodeU64(calldata.length));
            parts.push(calldata);
            parts.push(encodeU64(body.value));
            parts.push(encodeU64(body.gas_limit));
            break;
        }
        case "Stake": {
            parts.push(new Uint8Array([0x04]));
            parts.push(encodeU64(body.amount));
            parts.push(new Uint8Array([body.is_stake ? 0x01 : 0x00]));
            parts.push((0, utils_1.hexToBytes)(body.validator));
            break;
        }
        case "Settle": {
            parts.push(new Uint8Array([0x01]));
            parts.push((0, utils_1.hexToBytes)(body.agent_id));
            parts.push((0, utils_1.hexToBytes)(body.service_hash));
            parts.push(encodeU64(body.amount));
            parts.push(encodeU64(body.usage_units));
            if (body.amount_commitment) {
                parts.push(new Uint8Array([0x01]));
                parts.push((0, utils_1.hexToBytes)(body.amount_commitment));
            }
            else {
                parts.push(new Uint8Array([0x00]));
            }
            break;
        }
        default: {
            // Fallback: JSON-serialize unknown body types
            const json = new TextEncoder().encode(JSON.stringify(body));
            parts.push(json);
        }
    }
    return concat(...parts);
}
/**
 * Compute the BLAKE3 signing hash for a transaction.
 *
 * Matches the Rust `Transaction::compute_hash()`:
 * `tx_type || from || nonce || body || fee || gas_limit`
 */
function computeHash(txTypeByte, fromAddr, nonce, body, fee, gasLimit) {
    const data = concat(new Uint8Array([txTypeByte]), (0, utils_1.hexToBytes)(fromAddr), encodeU64(nonce), encodeBody(body), encodeU64(fee), encodeU64(gasLimit));
    // BLAKE3 with derive_key context
    const digest = (0, blake3_1.blake3)(data, { context: TX_DOMAIN });
    return (0, utils_1.bytesToHex)(digest);
}
/** Validate that an address is a 64-character hex string. */
function validateAddress(address, fieldName) {
    if (!address) {
        throw new Error(`${fieldName} is required`);
    }
    if (address.length !== 64) {
        throw new Error(`${fieldName} must be 64 hex characters, got ${address.length}`);
    }
    // Check valid hex
    if (!/^[0-9a-fA-F]{64}$/.test(address)) {
        throw new Error(`${fieldName} is not valid hex`);
    }
}
// ---------------------------------------------------------------------------
// TransactionBuilder
// ---------------------------------------------------------------------------
/**
 * Build unsigned ARC Chain transactions.
 *
 * All methods return a Transaction object that can be signed with
 * `TransactionBuilder.sign()` and submitted via `ArcClient.submitTransaction()`.
 */
class TransactionBuilder {
    // -- Transfer --
    /**
     * Build an unsigned transfer transaction.
     *
     * @param fromAddr - 64-char hex sender address
     * @param toAddr - 64-char hex recipient address
     * @param amount - Amount in ARC tokens (smallest unit)
     * @param fee - Transaction fee (default 1)
     * @param nonce - Sender nonce for replay protection
     */
    static transfer(fromAddr, toAddr, amount, fee = 1, nonce = 0) {
        validateAddress(fromAddr, "fromAddr");
        validateAddress(toAddr, "toAddr");
        if (amount <= 0)
            throw new Error("Amount must be positive");
        const body = {
            type: "Transfer",
            to: toAddr,
            amount,
            amount_commitment: null,
        };
        const hash = computeHash(0x01, fromAddr, nonce, body, fee, 0);
        return {
            tx_type: "Transfer",
            from: fromAddr,
            to: toAddr,
            amount,
            nonce,
            fee,
            gas_limit: 0,
            body,
            hash,
            signature: null,
        };
    }
    // -- Deploy Contract --
    /**
     * Build an unsigned contract deployment transaction.
     *
     * @param fromAddr - 64-char hex sender address
     * @param code - WASM bytecode as Uint8Array
     * @param gasLimit - Maximum gas for deployment
     * @param fee - Transaction fee
     * @param nonce - Sender nonce
     * @param constructorArgs - ABI-encoded constructor arguments
     * @param stateRentDeposit - Pre-paid state rent
     */
    static deployContract(fromAddr, code, gasLimit = 1000000, fee = 50, nonce = 0, constructorArgs = new Uint8Array(0), stateRentDeposit = 0) {
        validateAddress(fromAddr, "fromAddr");
        if (code.length === 0)
            throw new Error("Bytecode must not be empty");
        const body = {
            type: "DeployContract",
            bytecode: (0, utils_1.bytesToHex)(code),
            constructor_args: (0, utils_1.bytesToHex)(constructorArgs),
            state_rent_deposit: stateRentDeposit,
        };
        const hash = computeHash(0x08, fromAddr, nonce, body, fee, gasLimit);
        return {
            tx_type: "DeployContract",
            from: fromAddr,
            nonce,
            fee,
            gas_limit: gasLimit,
            body,
            hash,
            signature: null,
        };
    }
    // -- Call Contract --
    /**
     * Build an unsigned WASM contract call transaction.
     *
     * @param fromAddr - 64-char hex sender address
     * @param contractAddr - 64-char hex contract address
     * @param calldata - ABI-encoded call data as Uint8Array
     * @param value - ARC tokens to send with the call
     * @param gasLimit - Maximum gas for execution
     * @param func - Function name to call
     * @param fee - Transaction fee
     * @param nonce - Sender nonce
     */
    static callContract(fromAddr, contractAddr, calldata, value = 0, gasLimit = 1000000, func = "", fee = 1, nonce = 0) {
        validateAddress(fromAddr, "fromAddr");
        validateAddress(contractAddr, "contractAddr");
        const body = {
            type: "WasmCall",
            contract: contractAddr,
            function: func,
            calldata: (0, utils_1.bytesToHex)(calldata),
            value,
            gas_limit: gasLimit,
        };
        const hash = computeHash(0x06, fromAddr, nonce, body, fee, gasLimit);
        return {
            tx_type: "WasmCall",
            from: fromAddr,
            nonce,
            fee,
            gas_limit: gasLimit,
            body,
            hash,
            signature: null,
        };
    }
    // -- Stake --
    /**
     * Build an unsigned stake/unstake transaction.
     *
     * @param fromAddr - 64-char hex sender address
     * @param amount - Amount to stake or unstake
     * @param isStake - True to stake, false to unstake
     * @param validator - Validator address (defaults to self)
     * @param fee - Transaction fee
     * @param nonce - Sender nonce
     */
    static stake(fromAddr, amount, isStake = true, validator, fee = 1, nonce = 0) {
        validateAddress(fromAddr, "fromAddr");
        if (amount <= 0)
            throw new Error("Stake amount must be positive");
        const validatorAddr = validator ?? fromAddr;
        validateAddress(validatorAddr, "validator");
        const body = {
            type: "Stake",
            amount,
            is_stake: isStake,
            validator: validatorAddr,
        };
        const hash = computeHash(0x05, fromAddr, nonce, body, fee, 0);
        return {
            tx_type: "Stake",
            from: fromAddr,
            nonce,
            fee,
            gas_limit: 0,
            body,
            hash,
            signature: null,
        };
    }
    // -- Settle --
    /**
     * Build an unsigned settlement transaction (zero fee).
     *
     * @param fromAddr - 64-char hex sender address
     * @param agentId - 64-char hex agent address
     * @param serviceHash - 64-char hex service hash
     * @param amount - Settlement amount
     * @param usageUnits - Usage units consumed
     * @param nonce - Sender nonce
     */
    static settle(fromAddr, agentId, serviceHash, amount, usageUnits, nonce = 0) {
        validateAddress(fromAddr, "fromAddr");
        validateAddress(agentId, "agentId");
        const body = {
            type: "Settle",
            agent_id: agentId,
            service_hash: serviceHash,
            amount,
            usage_units: usageUnits,
            amount_commitment: null,
        };
        const hash = computeHash(0x02, fromAddr, nonce, body, 0, 0);
        return {
            tx_type: "Settle",
            from: fromAddr,
            nonce,
            fee: 0,
            gas_limit: 0,
            body,
            hash,
            signature: null,
        };
    }
    // -- Signing --
    /**
     * Sign a transaction with the given key pair.
     *
     * @param tx - Unsigned transaction from any build method
     * @param keypair - Ed25519 key pair whose address matches tx.from
     * @returns A new signed transaction (original is not modified)
     */
    static async sign(tx, keypair) {
        const kpAddr = keypair.address();
        if (tx.from && tx.from !== kpAddr) {
            throw new Error(`KeyPair address ${kpAddr.slice(0, 16)}... does not match tx sender ${tx.from.slice(0, 16)}...`);
        }
        const hashBytes = (0, utils_1.hexToBytes)(tx.hash);
        const signature = await keypair.sign(hashBytes);
        return {
            ...tx,
            signature: {
                Ed25519: {
                    public_key: keypair.publicKeyHex(),
                    signature: (0, utils_1.bytesToHex)(signature),
                },
            },
        };
    }
}
exports.TransactionBuilder = TransactionBuilder;
//# sourceMappingURL=transaction.js.map