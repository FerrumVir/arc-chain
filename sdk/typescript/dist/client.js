import { assertU64, parseJsonWithBigInts, stringifyJsonWithBigInts, u64ToBigInt, } from "./u64.js";
const MAX_TX_SUBMIT_BATCH_SIZE = 64;
export class ArcRpcError extends Error {
    statusCode;
    body;
    constructor(statusCode, body) {
        super(`ARC RPC Error (${statusCode}): ${body}`);
        this.name = "ArcRpcError";
        this.statusCode = statusCode;
        this.body = body;
    }
}
export class ArcClient {
    baseUrl;
    timeout;
    headers;
    constructor(rpcUrl, options) {
        this.baseUrl = rpcUrl.replace(/\/+$/, "");
        this.timeout = options?.timeout ?? 30_000;
        this.headers = {
            "Content-Type": "application/json",
            ...options?.headers,
        };
    }
    async getHealth() {
        return this._get("/health");
    }
    async getInfo() {
        return this._get("/info");
    }
    async getNodeInfo() {
        return this._get("/node/info");
    }
    async getStats() {
        return this._get("/stats");
    }
    async getBlock(height) {
        return this._get(`/block/${height}`);
    }
    async getBlocks(options) {
        const params = new URLSearchParams();
        if (options?.from !== undefined)
            params.set("from", String(options.from));
        if (options?.to !== undefined)
            params.set("to", String(options.to));
        if (options?.limit !== undefined)
            params.set("limit", String(options.limit));
        const qs = params.toString();
        return this._get(`/blocks${qs ? `?${qs}` : ""}`);
    }
    async getBlockTxs(height, options) {
        const params = new URLSearchParams();
        if (options?.offset !== undefined)
            params.set("offset", String(options.offset));
        if (options?.limit !== undefined)
            params.set("limit", String(options.limit));
        const qs = params.toString();
        return this._get(`/block/${height}/txs${qs ? `?${qs}` : ""}`);
    }
    async getBlockProofs(height) {
        return this._get(`/block/${height}/proofs`);
    }
    async getTx(hash) {
        return this._get(`/tx/${hash}`);
    }
    async getTxFull(hash) {
        return this._get(`/tx/${hash}/full`);
    }
    async getTxProof(hash) {
        return this._get(`/tx/${hash}/proof`);
    }
    async submitTx(tx) {
        assertSignedTransferU64Fields(tx);
        await this._assertTransactionDomain(tx.transaction_domain);
        const { transaction_domain: _domain, ...wire } = tx;
        return this._post("/tx/submit", wire);
    }
    async submitSignedTx(tx) {
        assertU64(tx.gas_limit, "gas_limit");
        if (u64ToBigInt(tx.gas_limit, "gas_limit") !== 0n) {
            throw new RangeError("gas_limit must be zero for the flat transfer RPC");
        }
        const signature = tx.signature.Ed25519;
        return this.submitTx({
            from: tx.from,
            to: tx.body.to,
            amount: tx.body.amount,
            nonce: tx.nonce,
            fee: tx.fee,
            tx_type: "Transfer",
            signature: signature.signature,
            public_key: signature.public_key,
            transaction_domain: tx.transaction_domain,
        });
    }
    async submitTxBatch(transactions) {
        if (transactions.length > MAX_TX_SUBMIT_BATCH_SIZE) {
            throw new RangeError(`transaction batch exceeds the maximum of ${MAX_TX_SUBMIT_BATCH_SIZE} items`);
        }
        for (const tx of transactions)
            assertSignedTransferU64Fields(tx);
        const domain = await this.getTransactionDomain();
        for (const tx of transactions) {
            this._requireMatchingTransactionDomain(tx.transaction_domain, domain);
        }
        return this._post("/tx/submit_batch", {
            transactions: transactions.map(({ transaction_domain: _domain, ...wire }) => wire),
        });
    }
    async getTransactionDomain() {
        let value;
        try {
            value = await this._get("/network/info");
        }
        catch (error) {
            if (error instanceof ArcRpcError && error.statusCode === 404)
                return null;
            throw error;
        }
        const raw = value.transaction_domain;
        const recoveryActive = value.recovery_active === true;
        const protocolMajor = Number.parseInt(String(value.protocol_version ?? "0").split(".")[0] ?? "0", 10);
        if (raw == null) {
            if (recoveryActive || protocolMajor >= 3) {
                throw new Error("node requires recovery-domain signatures but omitted transaction_domain");
            }
            return null;
        }
        if (typeof raw !== "string" || !/^0x[0-9a-fA-F]{64}$/.test(raw)) {
            throw new Error("node returned a malformed 32-byte transaction_domain");
        }
        if (/^0x0{64}$/i.test(raw)) {
            throw new Error("node returned an all-zero transaction_domain");
        }
        return raw.toLowerCase();
    }
    async _assertTransactionDomain(signedDomain) {
        const advertised = await this.getTransactionDomain();
        this._requireMatchingTransactionDomain(signedDomain, advertised);
    }
    _requireMatchingTransactionDomain(signedDomain, advertised) {
        let normalized = null;
        if (signedDomain !== null) {
            if (!/^0x[0-9a-fA-F]{64}$/.test(signedDomain) || /^0x0{64}$/i.test(signedDomain)) {
                throw new Error("transaction signature domain must be a non-zero 32-byte 0x-prefixed hex value");
            }
            normalized = signedDomain.toLowerCase();
        }
        if (normalized !== advertised) {
            throw new Error(`transaction signature domain mismatch: signed for ${normalized ?? "legacy-v1"}, ` +
                `node requires ${advertised ?? "legacy-v1"}`);
        }
    }
    async getAccount(address) {
        return this._get(`/account/${address}`);
    }
    async getAccountTxs(address) {
        return this._get(`/account/${address}/txs`);
    }
    async getBalance(address) {
        const account = await this.getAccount(address);
        return account.balance;
    }
    async getNonce(address) {
        const account = await this.getAccount(address);
        return account.nonce;
    }
    async getValidators() {
        return this._get("/validators");
    }
    async getContract(address) {
        return this._get(`/contract/${address}`);
    }
    async callContract(address, fn, options) {
        if (options?.gasLimit !== undefined) {
            assertU64(options.gasLimit, "gas_limit");
        }
        return this._post(`/contract/${address}/call`, {
            function: fn,
            calldata: options?.calldata,
            from: options?.from,
            gas_limit: options?.gasLimit,
        });
    }
    async getLightSnapshot() {
        return this._get("/light/snapshot");
    }
    async getSyncSnapshotInfo() {
        return this._get("/sync/snapshot/info");
    }
    async getSyncSnapshot() {
        const controller = new AbortController();
        const timer = setTimeout(() => controller.abort(), this.timeout * 10);
        try {
            const res = await fetch(`${this.baseUrl}/sync/snapshot`, {
                method: "GET",
                signal: controller.signal,
            });
            if (!res.ok) {
                throw new ArcRpcError(res.status, await res.text());
            }
            return res;
        }
        finally {
            clearTimeout(timer);
        }
    }
    async faucetClaim(address) {
        return this._post("/claim", { address });
    }
    async faucetStatus() {
        return this._get("/status");
    }
    async faucetHealth() {
        return this._get("/health");
    }
    async ethRpc(method, params = [], id = 1) {
        const request = {
            jsonrpc: "2.0",
            method,
            params,
            id,
        };
        const response = await this._post("/eth", request);
        if (response.error) {
            throw new ArcRpcError(response.error.code, response.error.message);
        }
        return response.result;
    }
    async ethChainId() {
        return this.ethRpc("eth_chainId");
    }
    async ethBlockNumber() {
        return this.ethRpc("eth_blockNumber");
    }
    async ethGetBalance(address, block = "latest") {
        return this.ethRpc("eth_getBalance", [address, block]);
    }
    async ethGetTransactionCount(address, block = "latest") {
        return this.ethRpc("eth_getTransactionCount", [address, block]);
    }
    async waitForTx(hash, options) {
        const timeout = options?.timeout ?? 60_000;
        const interval = options?.interval ?? 500;
        const deadline = Date.now() + timeout;
        while (Date.now() < deadline) {
            try {
                return await this.getTx(hash);
            }
            catch (err) {
                if (err instanceof ArcRpcError && err.statusCode === 404) {
                    await sleep(interval);
                    continue;
                }
                throw err;
            }
        }
        throw new Error(`Transaction ${hash} not confirmed within ${timeout}ms`);
    }
    onBlock(callback, interval = 1000) {
        let active = true;
        let lastHeight = -1;
        const poll = async () => {
            while (active) {
                try {
                    const info = await this.getInfo();
                    const tip = info.block_height;
                    if (lastHeight === -1) {
                        lastHeight = tip - 1;
                    }
                    for (let h = lastHeight + 1; h <= tip && active; h++) {
                        const block = await this.getBlock(h);
                        await callback(block);
                        lastHeight = h;
                    }
                }
                catch {
                }
                if (active) {
                    await sleep(interval);
                }
            }
        };
        poll();
        return {
            unsubscribe: () => {
                active = false;
            },
        };
    }
    async _get(path) {
        const controller = new AbortController();
        const timer = setTimeout(() => controller.abort(), this.timeout);
        try {
            const res = await fetch(`${this.baseUrl}${path}`, {
                method: "GET",
                headers: this.headers,
                signal: controller.signal,
            });
            if (!res.ok) {
                throw new ArcRpcError(res.status, await res.text());
            }
            return parseJsonWithBigInts(await res.text());
        }
        finally {
            clearTimeout(timer);
        }
    }
    async _post(path, body) {
        const controller = new AbortController();
        const timer = setTimeout(() => controller.abort(), this.timeout);
        const serializedBody = stringifyJsonWithBigInts(body);
        try {
            const res = await fetch(`${this.baseUrl}${path}`, {
                method: "POST",
                headers: this.headers,
                body: serializedBody,
                signal: controller.signal,
            });
            if (!res.ok) {
                throw new ArcRpcError(res.status, await res.text());
            }
            return parseJsonWithBigInts(await res.text());
        }
        finally {
            clearTimeout(timer);
        }
    }
}
function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
}
function assertSignedTransferU64Fields(tx) {
    assertU64(tx.amount, "amount");
    assertU64(tx.nonce, "nonce");
    assertU64(tx.fee, "fee");
}
//# sourceMappingURL=client.js.map