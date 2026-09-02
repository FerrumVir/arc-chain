"use strict";
/**
 * ARC Chain SDK - RPC client.
 *
 * Typed HTTP client for all ARC Chain RPC endpoints.
 * Uses the native `fetch` API (Node 18+, Deno, Bun, browsers).
 */
Object.defineProperty(exports, "__esModule", { value: true });
exports.ArcClient = exports.ArcTransactionError = exports.ArcConnectionError = exports.ArcError = void 0;
const u64_1 = require("./u64");
const MAX_TX_SUBMIT_BATCH_SIZE = 64;
// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------
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
/**
 * HTTP client for the ARC Chain RPC API.
 *
 * Usage:
 * ```ts
 * const client = new ArcClient("http://localhost:9090");
 * const info = await client.getChainInfo();
 * console.log(info.block_height);
 * ```
 */
class ArcClient {
    constructor(rpcUrl, options = {}) {
        this.rpcUrl = rpcUrl.replace(/\/+$/, "");
        this.timeout = options.timeout ?? 30000;
        this.headers = {
            "Content-Type": "application/json",
            ...options.headers,
        };
    }
    // -- Internal helpers --
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
    // -- Block endpoints --
    /**
     * GET /block/{height} -- Fetch a block by height.
     */
    async getBlock(height) {
        return this._get(`/block/${height}`);
    }
    /**
     * GET /blocks -- Paginated block listing.
     */
    async getBlocks(fromHeight = 0, toHeight, limit = 20) {
        const params = { from: fromHeight, limit };
        if (toHeight !== undefined)
            params.to = toHeight;
        return this._get(`/blocks`, params);
    }
    /**
     * GET /block/{height}/txs -- Paginated transaction listing for a block.
     */
    async getBlockTxs(height, offset = 0, limit = 100) {
        return this._get(`/block/${height}/txs`, { offset, limit });
    }
    /**
     * GET /block/{height}/proofs -- All Merkle proofs for transactions in a block.
     */
    async getBlockProofs(height) {
        return this._get(`/block/${height}/proofs`);
    }
    // -- Account endpoints --
    /**
     * GET /account/{address} -- Fetch an account by address.
     */
    async getAccount(address) {
        return this._get(`/account/${address}`);
    }
    /**
     * GET /account/{address}/txs -- Transaction hashes involving an account.
     */
    async getAccountTxs(address) {
        return this._get(`/account/${address}/txs`);
    }
    // -- Transaction endpoints --
    /**
     * POST /tx/submit -- Submit a transaction to the mempool.
     *
     * Accepts either a raw RPC-format object or a signed Transaction from
     * TransactionBuilder.sign().
     *
     * @returns Transaction hash string.
     */
    async submitTransaction(tx) {
        const payload = this._normalizeSignedTransfer(tx);
        await this._assertTransactionDomain(tx.transaction_domain);
        const data = await this._post("/tx/submit", payload);
        return data.tx_hash;
    }
    /**
     * GET /tx/{hash} -- Look up a transaction receipt by hash.
     */
    async getTransaction(txHash) {
        return this._get(`/tx/${txHash}`);
    }
    /**
     * GET /tx/{hash}/full -- Full transaction body with type-specific fields.
     */
    async getFullTransaction(txHash) {
        return this._get(`/tx/${txHash}/full`);
    }
    /**
     * GET /tx/{hash}/proof -- Merkle inclusion proof for a transaction.
     */
    async getTxProof(txHash) {
        return this._get(`/tx/${txHash}/proof`);
    }
    /**
     * POST /tx/submit_batch -- Submit multiple transactions.
     */
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
    /** Return the current v3 signing domain, or null for a pre-v3 node. */
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
    // -- Chain info & stats --
    /**
     * GET /info -- Chain information.
     */
    async getChainInfo() {
        return this._get("/info");
    }
    /**
     * GET /stats -- Chain statistics.
     */
    async getStats() {
        return this._get("/stats");
    }
    /**
     * GET /health -- Node health status.
     */
    async getHealth() {
        return this._get("/health");
    }
    /**
     * GET /node/info -- Validator node information.
     */
    async getNodeInfo() {
        return this._get("/node/info");
    }
    // -- Contract endpoints --
    /**
     * GET /contract/{address} -- Contract bytecode info.
     */
    async getContractInfo(address) {
        return this._get(`/contract/${address}`);
    }
    /**
     * POST /contract/{address}/call -- Read-only contract call.
     */
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
    // -- Light client & sync --
    /**
     * GET /light/snapshot -- Lightweight snapshot for light client bootstrapping.
     */
    async getLightSnapshot() {
        return this._get("/light/snapshot");
    }
    /**
     * GET /sync/snapshot/info -- Metadata about available state sync snapshot.
     */
    async getSyncSnapshotInfo() {
        return this._get("/sync/snapshot/info");
    }
    // -- ETH JSON-RPC --
    /**
     * POST /eth -- Send an ETH-compatible JSON-RPC request.
     *
     * Supports methods like eth_chainId, eth_blockNumber,
     * eth_getBalance, eth_call, eth_estimateGas, etc.
     */
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