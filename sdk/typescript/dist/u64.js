/** Largest value accepted by Rust's `u64`. */
export const U64_MAX = (1n << 64n) - 1n;
/** Convert a public u64 input to its exact bigint representation. */
export function u64ToBigInt(value, fieldName) {
    if (typeof value === "number") {
        if (!Number.isSafeInteger(value) || value < 0) {
            throw new RangeError(`${fieldName} must be a non-negative safe integer or bigint in the u64 range`);
        }
        return BigInt(value);
    }
    if (typeof value !== "bigint") {
        throw new TypeError(`${fieldName} must be a number or bigint`);
    }
    if (value < 0n || value > U64_MAX) {
        throw new RangeError(`${fieldName} must be in the u64 range (0..${U64_MAX})`);
    }
    return value;
}
/** Fail closed before an inexact or out-of-range u64 reaches the network. */
export function assertU64(value, fieldName) {
    u64ToBigInt(value, fieldName);
}
/**
 * JSON.stringify cannot encode bigint. This serializer emits bigint values as
 * exact, unquoted JSON integer tokens, which serde_json's Rust `u64` decoder
 * accepts. Other JSON values retain JSON.stringify-compatible behavior.
 */
export function stringifyJsonWithBigInts(value) {
    const serialized = serializeJsonValue(value, new Set());
    if (serialized === undefined) {
        throw new TypeError("ARC RPC request body is not JSON-serializable");
    }
    return serialized;
}
const U64_RESPONSE_FIELDS = new Set([
    "amount",
    "balance",
    "claim_amount",
    "fee",
    "gas_limit",
    "gas_used",
    "initial_stake",
    "max_fee",
    "max_reward",
    "new_stake",
    "nonce",
    "offer_amount",
    "receive_amount",
    "registration_fee",
    "stake",
    "staked_balance",
    "state_rent_deposit",
    "total_amount",
    "total_stake",
    "total_supply",
    "usage_units",
    "value",
]);
/**
 * Parse JSON while reading integer tokens directly from the response text.
 * This stays lossless on Node 18 and browsers that do not provide the newer
 * `JSON.parse` reviver `context.source` argument.
 */
export function parseJsonWithBigInts(text) {
    let index = 0;
    const syntaxError = (message) => {
        throw new SyntaxError(`${message} at JSON offset ${index}`);
    };
    const skipWhitespace = () => {
        while (index < text.length && /[\t\n\r ]/.test(text[index]))
            index += 1;
    };
    const parseString = () => {
        if (text[index] !== '"')
            return syntaxError("expected string");
        const start = index;
        index += 1;
        while (index < text.length) {
            const character = text[index];
            index += 1;
            if (character === '"') {
                return JSON.parse(text.slice(start, index));
            }
            if (character === "\\") {
                if (index >= text.length)
                    return syntaxError("unterminated escape");
                const escape = text[index];
                index += 1;
                if (escape === "u") {
                    const hex = text.slice(index, index + 4);
                    if (!/^[0-9a-fA-F]{4}$/.test(hex))
                        return syntaxError("invalid unicode escape");
                    index += 4;
                }
                else if (!/["\\/bfnrt]/.test(escape)) {
                    return syntaxError("invalid escape");
                }
            }
            else if (character.charCodeAt(0) < 0x20) {
                return syntaxError("unescaped control character");
            }
        }
        return syntaxError("unterminated string");
    };
    const parseNumber = (fieldName) => {
        const start = index;
        if (text[index] === "-")
            index += 1;
        if (text[index] === "0") {
            index += 1;
        }
        else if (/[1-9]/.test(text[index] ?? "")) {
            while (/[0-9]/.test(text[index] ?? ""))
                index += 1;
        }
        else {
            return syntaxError("invalid number");
        }
        if (text[index] === ".") {
            index += 1;
            if (!/[0-9]/.test(text[index] ?? ""))
                return syntaxError("invalid fraction");
            while (/[0-9]/.test(text[index] ?? ""))
                index += 1;
        }
        if (text[index] === "e" || text[index] === "E") {
            index += 1;
            if (text[index] === "+" || text[index] === "-")
                index += 1;
            if (!/[0-9]/.test(text[index] ?? ""))
                return syntaxError("invalid exponent");
            while (/[0-9]/.test(text[index] ?? ""))
                index += 1;
        }
        const token = text.slice(start, index);
        const isIntegerToken = !/[.eE]/.test(token);
        if (isIntegerToken) {
            if (token.startsWith("-")) {
                if (U64_RESPONSE_FIELDS.has(fieldName)) {
                    throw new RangeError(`${fieldName} was negative in the RPC response`);
                }
                const signed = Number(token);
                if (!Number.isSafeInteger(signed)) {
                    throw new RangeError(`${fieldName || "RPC value"} exceeds JavaScript's safe-integer range`);
                }
                return signed;
            }
            const exact = BigInt(token);
            if (exact <= BigInt(Number.MAX_SAFE_INTEGER))
                return Number(exact);
            if (!U64_RESPONSE_FIELDS.has(fieldName)) {
                throw new RangeError(`${fieldName || "RPC value"} exceeds JavaScript's safe-integer range`);
            }
            if (exact > U64_MAX)
                throw new RangeError(`${fieldName} exceeds the u64 range`);
            return exact;
        }
        const numeric = Number(token);
        if (!Number.isFinite(numeric) || (Number.isInteger(numeric) && !Number.isSafeInteger(numeric))) {
            throw new RangeError(`${fieldName || "RPC value"} was not an exact safe number in the RPC response`);
        }
        if (U64_RESPONSE_FIELDS.has(fieldName) &&
            (!Number.isSafeInteger(numeric) || numeric < 0)) {
            throw new RangeError(`${fieldName} was not a non-negative integer in the RPC response`);
        }
        return numeric;
    };
    const parseValue = (fieldName) => {
        skipWhitespace();
        const character = text[index];
        if (character === '"')
            return parseString();
        if (character === "-" || /[0-9]/.test(character ?? "")) {
            return parseNumber(fieldName);
        }
        if (text.startsWith("true", index)) {
            index += 4;
            return true;
        }
        if (text.startsWith("false", index)) {
            index += 5;
            return false;
        }
        if (text.startsWith("null", index)) {
            index += 4;
            return null;
        }
        if (character === "[") {
            index += 1;
            const array = [];
            skipWhitespace();
            if (text[index] === "]") {
                index += 1;
                return array;
            }
            while (true) {
                array.push(parseValue(""));
                skipWhitespace();
                if (text[index] === "]") {
                    index += 1;
                    return array;
                }
                if (text[index] !== ",")
                    return syntaxError("expected ',' or ']'");
                index += 1;
            }
        }
        if (character === "{") {
            index += 1;
            const object = {};
            skipWhitespace();
            if (text[index] === "}") {
                index += 1;
                return object;
            }
            while (true) {
                skipWhitespace();
                const key = parseString();
                skipWhitespace();
                if (text[index] !== ":")
                    return syntaxError("expected ':'");
                index += 1;
                const value = parseValue(key);
                Object.defineProperty(object, key, {
                    value,
                    enumerable: true,
                    configurable: true,
                    writable: true,
                });
                skipWhitespace();
                if (text[index] === "}") {
                    index += 1;
                    return object;
                }
                if (text[index] !== ",")
                    return syntaxError("expected ',' or '}'");
                index += 1;
            }
        }
        return syntaxError("unexpected token");
    };
    const result = parseValue("");
    skipWhitespace();
    if (index !== text.length)
        syntaxError("unexpected trailing input");
    return result;
}
function serializeJsonValue(input, ancestors) {
    let value = input;
    if (value !== null && typeof value === "object") {
        const toJSON = value.toJSON;
        if (typeof toJSON === "function") {
            value = toJSON.call(value);
        }
    }
    if (value === null)
        return "null";
    switch (typeof value) {
        case "string":
        case "boolean":
            return JSON.stringify(value);
        case "number":
            if (Number.isInteger(value) && !Number.isSafeInteger(value)) {
                throw new RangeError("JSON integer must be a safe number; use bigint for larger exact values");
            }
            return Number.isFinite(value) ? JSON.stringify(value) : "null";
        case "bigint":
            return u64ToBigInt(value, "JSON bigint").toString(10);
        case "undefined":
        case "function":
        case "symbol":
            return undefined;
        case "object": {
            const objectValue = value;
            if (ancestors.has(objectValue)) {
                throw new TypeError("Converting circular structure to JSON");
            }
            ancestors.add(objectValue);
            try {
                if (Array.isArray(value)) {
                    return `[${value
                        .map((item) => serializeJsonValue(item, ancestors) ?? "null")
                        .join(",")}]`;
                }
                const entries = [];
                for (const key of Object.keys(value)) {
                    const item = serializeJsonValue(value[key], ancestors);
                    if (item !== undefined) {
                        entries.push(`${JSON.stringify(key)}:${item}`);
                    }
                }
                return `{${entries.join(",")}}`;
            }
            finally {
                ancestors.delete(objectValue);
            }
        }
    }
}
//# sourceMappingURL=u64.js.map