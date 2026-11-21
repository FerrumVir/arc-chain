import { u64ToBigInt } from "./u64.js";
const ADDRESS_LENGTH = 64;
const TX_HASH_LENGTH = 64;
const HEX_64_RE = /^[0-9a-fA-F]{64}$/;
export function isValidAddress(addr) {
    const clean = addr.startsWith("0x") || addr.startsWith("0X")
        ? addr.slice(2)
        : addr;
    return clean.length === ADDRESS_LENGTH && HEX_64_RE.test(clean);
}
export function isValidTxHash(hash) {
    const clean = hash.startsWith("0x") || hash.startsWith("0X")
        ? hash.slice(2)
        : hash;
    return clean.length === TX_HASH_LENGTH && HEX_64_RE.test(clean);
}
export function formatHash(hash, prefixLen = 8, suffixLen = 8) {
    const clean = hash.startsWith("0x") || hash.startsWith("0X")
        ? hash.slice(2)
        : hash;
    if (clean.length <= prefixLen + suffixLen) {
        return clean;
    }
    return `${clean.slice(0, prefixLen)}...${clean.slice(-suffixLen)}`;
}
export function formatArc(amount, decimals = 6) {
    if (!Number.isSafeInteger(decimals) || decimals < 0 || decimals > 255) {
        throw new RangeError("decimals must be a safe integer from 0 through 255");
    }
    const exact = u64ToBigInt(amount, "amount");
    const divisor = 10n ** BigInt(decimals);
    const whole = exact / divisor;
    if (decimals === 0)
        return whole.toString(10);
    const fraction = (exact % divisor).toString(10).padStart(decimals, "0");
    return `${whole}.${fraction}`;
}
export function hexToBytes(hex) {
    const clean = hex.startsWith("0x") || hex.startsWith("0X")
        ? hex.slice(2)
        : hex;
    if (clean.length % 2 !== 0) {
        throw new Error(`Invalid hex string: odd length (${clean.length})`);
    }
    if (!/^[0-9a-fA-F]*$/.test(clean)) {
        throw new Error("Invalid hex string: contains non-hex characters");
    }
    const bytes = new Uint8Array(clean.length / 2);
    for (let i = 0; i < clean.length; i += 2) {
        bytes[i / 2] = parseInt(clean.substring(i, i + 2), 16);
    }
    return bytes;
}
export function bytesToHex(bytes) {
    return Array.from(bytes)
        .map((b) => b.toString(16).padStart(2, "0"))
        .join("");
}
export function stripHexPrefix(hex) {
    if (hex.startsWith("0x") || hex.startsWith("0X")) {
        return hex.slice(2);
    }
    return hex;
}
export function addHexPrefix(hex) {
    if (hex.startsWith("0x") || hex.startsWith("0X")) {
        return hex;
    }
    return `0x${hex}`;
}
//# sourceMappingURL=utils.js.map