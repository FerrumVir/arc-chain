"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.TransactionBuilder = void 0;
const blake3_1 = require("@noble/hashes/blake3");
const utils_1 = require("@noble/hashes/utils");
const u64_1 = require("./u64");
const TX_DOMAIN = "ARC-chain-tx-v1";
const TX_DOMAIN_V3 = "ARC-chain-tx-v3";
function encodeU64(value, fieldName) {
    const exact = (0, u64_1.u64ToBigInt)(value, fieldName);
    const buf = new ArrayBuffer(8);
    const view = new DataView(buf);
    view.setBigUint64(0, exact, true);
    return new Uint8Array(buf);
}
function encodeU32(value) {
    const buf = new ArrayBuffer(4);
    new DataView(buf).setUint32(0, value, true);
    return new Uint8Array(buf);
}
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
function normalizeTransactionDomain(value) {
    if (value === null)
        return null;
    const normalized = value.replace(/^0x/i, "");
    if (!/^[0-9a-fA-F]{64}$/.test(normalized) || /^0{64}$/.test(normalized)) {
        throw new Error("transactionDomain must be a non-zero 32-byte hex value");
    }
    return `0x${normalized.toLowerCase()}`;
}
function txTypeByteForSigning(txType) {
    switch (txType) {
        case "Transfer":
            return 0x01;
        case "Settle":
            return 0x02;
        case "Stake":
            return 0x05;
        case "WasmCall":
            return 0x06;
        case "DeployContract":
            return 0x08;
        default:
            throw new Error(`canonical signing codec is unavailable for ${txType}`);
    }
}
function encodeBody(body) {
    const parts = [];
    switch (body.type) {
        case "Transfer": {
            parts.push(encodeU32(0));
            parts.push((0, utils_1.hexToBytes)(body.to));
            parts.push(encodeU64(body.amount, "amount"));
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
            parts.push(encodeU32(7));
            const code = (0, utils_1.hexToBytes)(body.bytecode);
            parts.push(encodeU64(code.length, "bytecode length"));
            parts.push(code);
            const ctor = (0, utils_1.hexToBytes)(body.constructor_args);
            parts.push(encodeU64(ctor.length, "constructor_args length"));
            parts.push(ctor);
            parts.push(encodeU64(body.state_rent_deposit, "state_rent_deposit"));
            break;
        }
        case "WasmCall": {
            parts.push(encodeU32(5));
            parts.push((0, utils_1.hexToBytes)(body.contract));
            const func = new TextEncoder().encode(body.function);
            parts.push(encodeU64(func.length, "function length"));
            parts.push(func);
            const calldata = (0, utils_1.hexToBytes)(body.calldata);
            parts.push(encodeU64(calldata.length, "calldata length"));
            parts.push(calldata);
            parts.push(encodeU64(body.value, "value"));
            parts.push(encodeU64(body.gas_limit, "body.gas_limit"));
            break;
        }
        case "Stake": {
            parts.push(encodeU32(4));
            parts.push(encodeU64(body.amount, "amount"));
            parts.push(new Uint8Array([body.is_stake ? 0x01 : 0x00]));
            parts.push((0, utils_1.hexToBytes)(body.validator));
            break;
        }
        case "Settle": {
            parts.push(encodeU32(1));
            parts.push((0, utils_1.hexToBytes)(body.agent_id));
            parts.push((0, utils_1.hexToBytes)(body.service_hash));
            parts.push(encodeU64(body.amount, "amount"));
            parts.push(encodeU64(body.usage_units, "usage_units"));
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
            const json = new TextEncoder().encode(JSON.stringify(body));
            parts.push(json);
        }
    }
    return concat(...parts);
}
function computeHash(txTypeByte, fromAddr, nonce, body, fee, gasLimit, transactionDomain = null) {
    let context = TX_DOMAIN;
    const prefix = [];
    if (transactionDomain !== null) {
        const normalized = normalizeTransactionDomain(transactionDomain).slice(2);
        context = TX_DOMAIN_V3;
        prefix.push((0, utils_1.hexToBytes)(normalized));
    }
    const data = concat(...prefix, new Uint8Array([txTypeByte]), (0, utils_1.hexToBytes)(fromAddr), encodeU64(nonce, "nonce"), encodeBody(body), encodeU64(fee, "fee"), encodeU64(gasLimit, "gas_limit"));
    const digest = (0, blake3_1.blake3)(data, { context });
    return (0, utils_1.bytesToHex)(digest);
}
function validateAddress(address, fieldName) {
    if (!address) {
        throw new Error(`${fieldName} is required`);
    }
    if (address.length !== 64) {
        throw new Error(`${fieldName} must be 64 hex characters, got ${address.length}`);
    }
    if (!/^[0-9a-fA-F]{64}$/.test(address)) {
        throw new Error(`${fieldName} is not valid hex`);
    }
}
class TransactionBuilder {
    static transfer(fromAddr, toAddr, amount, fee = 1, nonce = 0, transactionDomain = null) {
        validateAddress(fromAddr, "fromAddr");
        validateAddress(toAddr, "toAddr");
        if ((0, u64_1.u64ToBigInt)(amount, "amount") === 0n) {
            throw new Error("Amount must be positive");
        }
        const body = {
            type: "Transfer",
            to: toAddr,
            amount,
            amount_commitment: null,
        };
        const hash = computeHash(0x01, fromAddr, nonce, body, fee, 0, transactionDomain);
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
            transaction_domain: normalizeTransactionDomain(transactionDomain),
        };
    }
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
            transaction_domain: null,
        };
    }
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
            transaction_domain: null,
        };
    }
    static stake(fromAddr, amount, isStake = true, validator, fee = 1, nonce = 0) {
        validateAddress(fromAddr, "fromAddr");
        if ((0, u64_1.u64ToBigInt)(amount, "amount") === 0n) {
            throw new Error("Stake amount must be positive");
        }
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
            transaction_domain: null,
        };
    }
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
            transaction_domain: null,
        };
    }
    static async sign(tx, keypair) {
        const kpAddr = keypair.address();
        if (tx.from && tx.from !== kpAddr) {
            throw new Error(`KeyPair address ${kpAddr.slice(0, 16)}... does not match tx sender ${tx.from.slice(0, 16)}...`);
        }
        const txTypeByte = txTypeByteForSigning(tx.tx_type);
        const hash = computeHash(txTypeByte, tx.from, tx.nonce, tx.body, tx.fee, tx.gas_limit, tx.transaction_domain);
        const hashBytes = (0, utils_1.hexToBytes)(hash);
        const signature = await keypair.sign(hashBytes);
        return {
            ...tx,
            hash,
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