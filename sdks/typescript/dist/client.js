"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.ArcClient = exports.ArcTransactionError = exports.ArcConnectionError = exports.ArcError = void 0;
const u64_1 = require("./u64");
const MAX_TX_SUBMIT_BATCH_SIZE = 64;
class ArcError extends Error {
    constructor(message, statusCode, detail) {
        super(message);
        this.name = "ArcError";
        this.statusCode = statusCode;
        this.detail = detail;
    }
}
exports.ArcError = ArcError;
class ArcConnectionError extends ArcError {
    constructor(message, url, cause) {
        super(message);
        this.name = "ArcConnectionError";
        this.url = url;
        this.cause = cause;
    }
}
exports.ArcConnectionError = ArcConnectionError;
class ArcTransactionError extends ArcError {
    constructor(message, txHash, statusCode) {
        super(message, statusCode);
        this.name = "ArcTransactionError";
        this.txHash = txHash;
    }
}
exports.ArcTransactionError = ArcTransactionError;
class ArcClient {
    constructor(rpcUrl, options = {}) {
        this.rpcUrl = rpcUrl.replace(/\/+$/, "");
        this.timeout = options.timeout ?? 30000;
        this.headers = {
            "Content-Type": "application/json",
            ...options.headers,
        };
    }
    async _get(path, params) {
        let url = `${this.rpcUrl}${path}`;
        if (params) {
            const qs = new URLSearchParams();
            for (const [k, v] of Object.entries(params)) {
                if (v !== undefined && v !== null)
                    qs.set(k, String(v));
            }
            const qsStr = qs.toString();
            if (qsStr)
                url += `?${qsStr}`;
        }
        let resp;
        try {
            resp = await fetch(url, {
                method: "GET",
                headers: this.headers,
                signal: AbortSignal.timeout(this.timeout),
            });
        }
        catch (e) {
            throw new ArcConnectionError(`Failed to connect to ${url}`, url, e instanceof Error ? e : undefined);
        }
        if (resp.status === 404) {
            throw new ArcError(`Not found: ${path}`, 404);
        }
        if (resp.status === 400) {
            throw new ArcError(`Bad request: ${path}`, 400);
        }
        if (resp.status >= 400) {
            const text = await resp.text().catch(() => "");
            throw new ArcError(`RPC error ${resp.status}: ${path}`, resp.status, text.slice(0, 500));
        }
        return (0, u64_1.parseJsonWithBigInts)(await resp.text());
    }
    async _post(path, body) {
        const url = `${this.rpcUrl}${path}`;
        const serializedBody = (0, u64_1.stringifyJsonWithBigInts)(body);
        let resp;
        try {
            resp = await fetch(url, {
                method: "POST",
                headers: this.headers,
                body: serializedBody,
                signal: AbortSignal.timeout(this.timeout),
            });
        }
        catch (e) {
            throw new ArcConnectionError(`Failed to connect to ${url}`, url, e instanceof Error ? e : undefined);
        }
        if (resp.status === 409) {
            throw new ArcTransactionError("Transaction already exists (duplicate/conflict)", undefined, 409);
        }
        if (resp.status >= 400) {
            const text = await resp.text().catch(() => "");
            throw new ArcError(`RPC error ${resp.status}: ${path}`, resp.status, text.slice(0, 500));
        }
        return (0, u64_1.parseJsonWithBigInts)(await resp.text());
    }
    async getBlock(height) {
        return this._get(`/block/${height}`);
    }
    async getBlocks(fromHeight = 0, toHeight, limit = 20) {
        const params = { from: fromHeight, limit };
        if (toHeight !== undefined)
            params.to = toHeight;
        return this._get(`/blocks`, params);
    }
    async getBlockTxs(height, offset = 0, limit = 100) {
        return this._get(`/block/${height}/txs`, { offset, limit });
    }
    async getBlockProofs(height) {
        return this._get(`/block/${height}/proofs`);
    }
    async getAccount(address) {
        return this._get(`/account/${address}`);
    }
    async getAccountTxs(address) {
        return this._get(`/account/${address}/txs`);
    }
    async submitTransaction(tx) {
        const payload = this._normalizeSignedTransfer(tx);
        await this._assertTransactionDomain(tx.transaction_domain);
        const data = await this._post("/tx/submit", payload);
        return data.tx_hash;
    }
    async getTransaction(txHash) {
        return this._get(`/tx/${txHash}`);
    }
    async getFullTransaction(txHash) {
        return this._get(`/tx/${txHash}/full`);
    }
    async getTxProof(txHash) {
        return this._get(`/tx/${txHash}/proof`);
    }
    async submitBatch(txs) {
        if (txs.length > MAX_TX_SUBMIT_BATCH_SIZE) {
            throw new RangeError(`transaction batch exceeds the maximum of ${MAX_TX_SUBMIT_BATCH_SIZE} items`);
        }
        for (const tx of txs)
            this._assertSignedTransferU64Fields(tx);
        const advertised = await this.getTransactionDomain();
        const normalized = txs.map((tx) => {
            this._requireMatchingTransactionDomain(tx.transaction_domain, advertised);
            return this._normalizeSignedTransfer(tx);
        });
        return this._post("/tx/submit_batch", {
            transactions: normalized,
        });
    }
    async getTransactionDomain() {
        let value;
        try {
            value = await this._get("/network/info");
        }
        catch (error) {
            if (error instanceof ArcError && error.statusCode === 404)
                return null;
            throw error;
        }
        const raw = value.transaction_domain;
        const protocolMajor = Number.parseInt(String(value.protocol_version ?? "0").split(".")[0] ?? "0", 10);
        if (raw == null) {
            if (value.recovery_active === true || protocolMajor >= 3) {
                throw new ArcError("Node requires recovery-domain signatures but omitted transaction_domain");
            }
            return null;
        }
        if (typeof raw !== "string" || !/^0x[0-9a-fA-F]{64}$/.test(raw)) {
            throw new ArcError("Node returned a malformed 32-byte transaction_domain");
        }
        if (/^0x0{64}$/i.test(raw)) {
            throw new ArcError("Node returned an all-zero transaction_domain");
        }
        return raw.toLowerCase();
    }
    _normalizeSignedTransfer(tx) {
        this._assertSignedTransferU64Fields(tx);
        if ("body" in tx) {
            if (tx.tx_type !== "Transfer" || tx.body.type !== "Transfer") {
                throw new ArcTransactionError("POST /tx/submit only accepts signed Transfer transactions");
            }
            (0, u64_1.assertU64)(tx.gas_limit, "gas_limit");
            if ((0, u64_1.u64ToBigInt)(tx.gas_limit, "gas_limit") !== 0n) {
                throw new ArcTransactionError("gas_limit must be zero for the flat transfer RPC");
            }
            return {
                from: tx.from,
                to: tx.body.to,
                amount: tx.body.amount,
                nonce: tx.nonce,
                fee: tx.fee,
                tx_type: "Transfer",
                signature: tx.signature.Ed25519.signature,
                public_key: tx.signature.Ed25519.public_key,
            };
        }
        const { transaction_domain: _domain, ...wire } = tx;
        return wire;
    }
    _assertSignedTransferU64Fields(tx) {
        const amount = "body" in tx ? tx.body.amount : tx.amount;
        (0, u64_1.assertU64)(amount, "amount");
        (0, u64_1.assertU64)(tx.nonce, "nonce");
        (0, u64_1.assertU64)(tx.fee, "fee");
    }
    async _assertTransactionDomain(signedDomain) {
        const advertised = await this.getTransactionDomain();
        this._requireMatchingTransactionDomain(signedDomain, advertised);
    }
    _requireMatchingTransactionDomain(signedDomain, advertised) {
        let normalized = null;
        if (signedDomain !== null) {
            const raw = signedDomain.replace(/^0x/i, "");
            if (!/^[0-9a-fA-F]{64}$/.test(raw) || /^0{64}$/.test(raw)) {
                throw new ArcTransactionError("Transaction signature domain must be a non-zero 32-byte hex value");
            }
            normalized = `0x${raw.toLowerCase()}`;
        }
        if (normalized !== advertised) {
            throw new ArcTransactionError(`Transaction signature domain mismatch: signed for ${normalized ?? "legacy-v1"}, ` +
                `node requires ${advertised ?? "legacy-v1"}`);
        }
    }
    async getChainInfo() {
        return this._get("/info");
    }
    async getStats() {
        return this._get("/stats");
    }
    async getHealth() {
        return this._get("/health");
    }
    async getNodeInfo() {
        return this._get("/node/info");
    }
    async getContractInfo(address) {
        return this._get(`/contract/${address}`);
    }
    async callContract(address, func, calldata, fromAddr, gasLimit = 1000000) {
        (0, u64_1.assertU64)(gasLimit, "gas_limit");
        const body = {
            function: func,
            gas_limit: gasLimit,
        };
        if (calldata)
            body.calldata = calldata;
        if (fromAddr)
            body.from = fromAddr;
        return this._post(`/contract/${address}/call`, body);
    }
    async getLightSnapshot() {
        return this._get("/light/snapshot");
    }
    async getSyncSnapshotInfo() {
        return this._get("/sync/snapshot/info");
    }
    async ethCall(method, params = []) {
        return this._post("/eth", {
            jsonrpc: "2.0",
            method,
            params,
            id: 1,
        });
    }
}
exports.ArcClient = ArcClient;
//# sourceMappingURL=client.js.map