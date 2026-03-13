"use strict";
/**
 * ARC Chain SDK — RPC client.
 *
 * Typed HTTP client for all ARC Chain RPC endpoints.
 * Uses the native `fetch` API (Node 18+, Deno, Bun, browsers).
 */
Object.defineProperty(exports, "__esModule", { value: true });
exports.ArcClient = exports.ArcTransactionError = exports.ArcConnectionError = exports.ArcError = void 0;
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
 * const client = new ArcClient("http://localhost:9000");
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
        return resp.json();
    }
    async _post(path, body) {
        const url = `${this.rpcUrl}${path}`;
        let resp;
        try {
            resp = await fetch(url, {
                method: "POST",
                headers: this.headers,
                body: JSON.stringify(body),
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
        return resp.json();
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
        let payload;
        // Normalize TransactionBuilder-style tx
        if ("body" in tx && "tx_type" in tx) {
            const body = tx.body;
            payload = {
                from: tx.from,
                to: body.to ?? "0".repeat(64),
                amount: body.amount ?? 0,
                nonce: tx.nonce ?? 0,
                tx_type: tx.tx_type,
            };
        }
        else {
            payload = tx;
        }
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
        const normalized = txs.map((tx) => {
            if ("body" in tx && "tx_type" in tx) {
                const body = tx.body;
                return {
                    from: tx.from,
                    to: body.to ?? "0".repeat(64),
                    amount: body.amount ?? 0,
                    nonce: tx.nonce ?? 0,
                };
            }
            return tx;
        });
        return this._post("/tx/submit_batch", {
            transactions: normalized,
        });
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