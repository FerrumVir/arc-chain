"""
ARC Chain SDK — RPC client.

Provides ``ArcClient``, a typed HTTP client for all ARC Chain RPC endpoints.
Uses httpx for connection pooling and async support.
"""

from __future__ import annotations

from typing import Any, Dict, List, Optional

import httpx

from .errors import ArcConnectionError, ArcError, ArcTransactionError
from .types import (
    Account,
    BatchResult,
    Block,
    ChainInfo,
    ChainStats,
    HealthInfo,
    NodeInfo,
    Receipt,
)


class ArcClient:
    """
    Synchronous HTTP client for the ARC Chain RPC API.

    Usage::

        client = ArcClient("http://localhost:9944")
        info = client.get_chain_info()
        print(info["block_height"])
    """

    def __init__(
        self,
        rpc_url: str,
        *,
        timeout: float = 30.0,
        headers: Optional[Dict[str, str]] = None,
    ):
        """
        Initialize the ARC client.

        Args:
            rpc_url: Base URL of the ARC Chain RPC node (e.g. http://localhost:9944).
            timeout: Request timeout in seconds (default 30).
            headers: Optional extra HTTP headers.
        """
        self.rpc_url = rpc_url.rstrip("/")
        self._client = httpx.Client(
            base_url=self.rpc_url,
            timeout=timeout,
            headers=headers or {},
        )

    def close(self) -> None:
        """Close the underlying HTTP client."""
        self._client.close()

    def __enter__(self) -> ArcClient:
        return self

    def __exit__(self, *args: Any) -> None:
        self.close()

    # -- Internal helpers --

    def _get(self, path: str, **params: Any) -> Any:
        """Send a GET request and return the parsed JSON response."""
        try:
            resp = self._client.get(path, params=params or None)
        except httpx.ConnectError as e:
            raise ArcConnectionError(
                f"Failed to connect to {self.rpc_url}{path}",
                url=f"{self.rpc_url}{path}",
                cause=e,
            )
        except httpx.TimeoutException as e:
            raise ArcConnectionError(
                f"Request timed out: {self.rpc_url}{path}",
                url=f"{self.rpc_url}{path}",
                cause=e,
            )
        if resp.status_code == 404:
            raise ArcError(f"Not found: {path}", status_code=404)
        if resp.status_code == 400:
            raise ArcError(f"Bad request: {path}", status_code=400)
        if resp.status_code >= 400:
            raise ArcError(
                f"RPC error {resp.status_code}: {path}",
                status_code=resp.status_code,
                detail=resp.text[:500],
            )
        return resp.json()

    def _post(self, path: str, json_body: Any) -> Any:
        """Send a POST request with a JSON body and return the parsed JSON response."""
        try:
            resp = self._client.post(path, json=json_body)
        except httpx.ConnectError as e:
            raise ArcConnectionError(
                f"Failed to connect to {self.rpc_url}{path}",
                url=f"{self.rpc_url}{path}",
                cause=e,
            )
        except httpx.TimeoutException as e:
            raise ArcConnectionError(
                f"Request timed out: {self.rpc_url}{path}",
                url=f"{self.rpc_url}{path}",
                cause=e,
            )
        if resp.status_code == 409:
            raise ArcTransactionError(
                "Transaction already exists (duplicate/conflict)",
                status_code=409,
            )
        if resp.status_code >= 400:
            raise ArcError(
                f"RPC error {resp.status_code}: {path}",
                status_code=resp.status_code,
                detail=resp.text[:500],
            )
        return resp.json()

    # -- Block endpoints --

    def get_block(self, height: int) -> dict:
        """
        GET /block/{height} -- Fetch a block by height.

        Args:
            height: Block number.

        Returns:
            Raw block dict from the RPC node.

        Raises:
            ArcError: If the block is not found (404).
        """
        return self._get(f"/block/{height}")

    def get_block_typed(self, height: int) -> Block:
        """Fetch a block and parse it into a typed Block object."""
        data = self.get_block(height)
        return Block.from_dict(data)

    def get_blocks(
        self,
        from_height: int = 0,
        to_height: Optional[int] = None,
        limit: int = 20,
    ) -> dict:
        """
        GET /blocks -- Paginated block listing.

        Args:
            from_height: Start height (inclusive).
            to_height: End height (inclusive, defaults to chain tip).
            limit: Max blocks to return (capped at 100 server-side).

        Returns:
            Dict with blocks list, from, to, count.
        """
        params: Dict[str, Any] = {"from": from_height, "limit": limit}
        if to_height is not None:
            params["to"] = to_height
        return self._get("/blocks", **params)

    def get_block_txs(self, height: int, offset: int = 0, limit: int = 100) -> dict:
        """
        GET /block/{height}/txs -- Paginated transaction listing for a block.

        Args:
            height: Block number.
            offset: Start offset.
            limit: Max transactions to return (capped at 1000).

        Returns:
            Dict with transactions list and pagination metadata.
        """
        return self._get(f"/block/{height}/txs", offset=offset, limit=limit)

    def get_block_proofs(self, height: int) -> dict:
        """
        GET /block/{height}/proofs -- All Merkle proofs for transactions in a block.
        """
        return self._get(f"/block/{height}/proofs")

    # -- Account endpoints --

    def get_account(self, address: str) -> dict:
        """
        GET /account/{address} -- Fetch an account by address.

        Args:
            address: 64-char hex address.

        Returns:
            Raw account dict.

        Raises:
            ArcError: If the account is not found (404).
        """
        return self._get(f"/account/{address}")

    def get_account_typed(self, address: str) -> Account:
        """Fetch an account and parse it into a typed Account object."""
        data = self.get_account(address)
        return Account.from_dict(data)

    def get_account_txs(self, address: str) -> dict:
        """
        GET /account/{address}/txs -- Transaction hashes involving an account.

        Args:
            address: 64-char hex address.

        Returns:
            Dict with tx_hashes list.
        """
        return self._get(f"/account/{address}/txs")

    # -- Transaction endpoints --

    def submit_transaction(self, tx: dict) -> str:
        """
        POST /tx/submit -- Submit a transaction to the mempool.

        Accepts only a signed transfer from ``TransactionBuilder.sign()`` or
        the exact flat signed-transfer wire shape. Unsigned writes and nested
        non-transfer read projections fail locally before any POST.

        Args:
            tx: Signed transfer dict carrying the exact signing domain.

        Returns:
            Transaction hash string.

        Raises:
            ArcTransactionError: On submission failure or conflict.
        """
        payload: Dict[str, Any]
        if "body" in tx and "tx_type" in tx:
            body = tx["body"]
            if tx.get("tx_type") != "Transfer" or body.get("type") != "Transfer":
                raise ArcTransactionError(
                    "POST /tx/submit only accepts signed Transfer transactions"
                )
            signature = tx.get("signature")
            if not isinstance(signature, dict):
                raise ArcTransactionError("Signed transfer requires an Ed25519 signature")
            sig_data = signature.get("Ed25519")
            if not isinstance(sig_data, dict):
                raise ArcTransactionError("Signed transfer requires an Ed25519 signature")
            payload = {
                "from": tx["from"],
                "to": body["to"],
                "amount": body["amount"],
                "nonce": tx["nonce"],
                "fee": tx["fee"],
                "tx_type": "Transfer",
                "signature": sig_data.get("signature"),
                "public_key": sig_data.get("public_key"),
            }
        else:
            required = {
                "from",
                "to",
                "amount",
                "nonce",
                "fee",
                "signature",
                "public_key",
                "transaction_domain",
            }
            if not required.issubset(tx):
                raise ArcTransactionError(
                    "Signed transfer requires from, to, amount, nonce, fee, "
                    "signature, public_key, and transaction_domain"
                )
            payload = {key: value for key, value in tx.items() if key != "transaction_domain"}

        self._validate_signed_transfer_wire(payload)
        self._require_matching_transaction_domain(
            tx.get("transaction_domain"), self.get_transaction_domain()
        )

        data = self._post("/tx/submit", payload)
        return data.get("tx_hash", "")

    def get_transaction(self, tx_hash: str) -> dict:
        """
        GET /tx/{hash} -- Look up a transaction receipt by hash.

        Args:
            tx_hash: 64-char hex transaction hash.

        Returns:
            Transaction receipt dict.
        """
        return self._get(f"/tx/{tx_hash}")

    def get_transaction_typed(self, tx_hash: str) -> Receipt:
        """Fetch a transaction receipt and parse into a typed Receipt."""
        data = self.get_transaction(tx_hash)
        return Receipt.from_dict(data)

    def get_full_transaction(self, tx_hash: str) -> dict:
        """
        GET /tx/{hash}/full -- Full transaction body with type-specific fields.
        """
        return self._get(f"/tx/{tx_hash}/full")

    def get_tx_proof(self, tx_hash: str) -> dict:
        """
        GET /tx/{hash}/proof -- Merkle inclusion proof for a transaction.
        """
        return self._get(f"/tx/{tx_hash}/proof")

    def submit_batch(self, txs: List[dict]) -> dict:
        """
        POST /tx/submit_batch -- Submit multiple transactions.

        Args:
            txs: List of transaction dicts (same format as submit_transaction).

        Returns:
            Dict with accepted, rejected, tx_hashes.
        """
        advertised = self.get_transaction_domain()
        normalized: List[Dict[str, Any]] = []
        for tx in txs:
            self._require_matching_transaction_domain(tx.get("transaction_domain"), advertised)
            if "body" in tx:
                if tx.get("tx_type") != "Transfer" or tx["body"].get("type") != "Transfer":
                    raise ArcTransactionError(
                        "POST /tx/submit_batch only accepts signed Transfer transactions"
                    )
                sig_data = (tx.get("signature") or {}).get("Ed25519")
                if not isinstance(sig_data, dict):
                    raise ArcTransactionError("Signed transfer requires an Ed25519 signature")
                payload = {
                    "from": tx["from"],
                    "to": tx["body"]["to"],
                    "amount": tx["body"]["amount"],
                    "nonce": tx["nonce"],
                    "fee": tx["fee"],
                    "tx_type": "Transfer",
                    "signature": sig_data.get("signature"),
                    "public_key": sig_data.get("public_key"),
                }
            else:
                payload = {key: value for key, value in tx.items() if key != "transaction_domain"}
            self._validate_signed_transfer_wire(payload)
            normalized.append(payload)

        return self._post("/tx/submit_batch", {"transactions": normalized})

    def get_transaction_domain(self) -> Optional[str]:
        """Return the node's exact v3 signing domain, or ``None`` on legacy v1."""
        try:
            value = self._get("/network/info")
        except ArcError as error:
            if error.status_code == 404:
                return None
            raise
        raw = value.get("transaction_domain")
        try:
            protocol_major = int(str(value.get("protocol_version") or "0").split(".")[0])
        except ValueError:
            protocol_major = 0
        if raw is None:
            if value.get("recovery_active") is True or protocol_major >= 3:
                raise ArcTransactionError(
                    "Node requires recovery-domain signatures but omitted transaction_domain"
                )
            return None
        return self._normalize_transaction_domain(raw)

    @staticmethod
    def _normalize_transaction_domain(value: Any) -> str:
        if not isinstance(value, str):
            raise ArcTransactionError(
                "Transaction signature domain must be a non-zero 32-byte hex value"
            )
        raw = value[2:] if value.lower().startswith("0x") else value
        try:
            encoded = bytes.fromhex(raw)
        except ValueError as error:
            raise ArcTransactionError(
                "Transaction signature domain must be a non-zero 32-byte hex value"
            ) from error
        if len(encoded) != 32 or not any(encoded):
            raise ArcTransactionError(
                "Transaction signature domain must be a non-zero 32-byte hex value"
            )
        return f"0x{raw.lower()}"

    @classmethod
    def _require_matching_transaction_domain(
        cls, signed_domain: Any, advertised: Optional[str]
    ) -> None:
        normalized = (
            None if signed_domain is None else cls._normalize_transaction_domain(signed_domain)
        )
        if normalized != advertised:
            raise ArcTransactionError(
                "Transaction signature domain mismatch: signed for "
                f"{normalized or 'legacy-v1'}, node requires {advertised or 'legacy-v1'}"
            )

    @staticmethod
    def _validate_signed_transfer_wire(payload: Dict[str, Any]) -> None:
        for field in ("signature", "public_key"):
            value = payload.get(field)
            expected = 128 if field == "signature" else 64
            if not isinstance(value, str) or len(value) != expected:
                raise ArcTransactionError(
                    f"Signed transfer {field} must be {expected} hexadecimal characters"
                )
            try:
                bytes.fromhex(value)
            except ValueError as error:
                raise ArcTransactionError(f"Signed transfer {field} must be hexadecimal") from error

    def submit_batch_typed(self, txs: List[dict]) -> BatchResult:
        """Submit a batch and parse the result into a typed BatchResult."""
        data = self.submit_batch(txs)
        return BatchResult.from_dict(data)

    # -- Chain info & stats --

    def get_chain_info(self) -> dict:
        """GET /info -- Chain information (height, account count, GPU, etc)."""
        return self._get("/info")

    def get_chain_info_typed(self) -> ChainInfo:
        """Fetch chain info and parse into a typed ChainInfo."""
        data = self.get_chain_info()
        return ChainInfo.from_dict(data)

    def get_stats(self) -> dict:
        """GET /stats -- Chain statistics (tx count, indexed receipts, etc)."""
        return self._get("/stats")

    def get_stats_typed(self) -> ChainStats:
        """Fetch stats and parse into a typed ChainStats."""
        data = self.get_stats()
        return ChainStats.from_dict(data)

    def get_health(self) -> dict:
        """GET /health -- Node health status."""
        return self._get("/health")

    def get_health_typed(self) -> HealthInfo:
        """Fetch health info and parse into a typed HealthInfo."""
        data = self.get_health()
        return HealthInfo.from_dict(data)

    def get_node_info(self) -> dict:
        """GET /node/info -- Validator node information."""
        return self._get("/node/info")

    def get_node_info_typed(self) -> NodeInfo:
        """Fetch node info and parse into a typed NodeInfo."""
        data = self.get_node_info()
        return NodeInfo.from_dict(data)

    # -- Contract endpoints --

    def get_contract_info(self, address: str) -> dict:
        """GET /contract/{address} -- Contract bytecode info."""
        return self._get(f"/contract/{address}")

    def call_contract(
        self,
        address: str,
        function: str,
        calldata: str = "",
        from_addr: Optional[str] = None,
        gas_limit: int = 1_000_000,
    ) -> dict:
        """
        POST /contract/{address}/call -- Read-only contract call.

        Args:
            address: Contract address.
            function: Function name.
            calldata: Hex-encoded calldata.
            from_addr: Optional caller address.
            gas_limit: Gas limit for the call.

        Returns:
            Execution result dict with success, return_data, gas_used.
        """
        body: Dict[str, Any] = {
            "function": function,
            "gas_limit": gas_limit,
        }
        if calldata:
            body["calldata"] = calldata
        if from_addr:
            body["from"] = from_addr
        return self._post(f"/contract/{address}/call", body)

    # -- Light client & sync --

    def get_light_snapshot(self) -> dict:
        """GET /light/snapshot -- Lightweight snapshot for light client bootstrapping."""
        return self._get("/light/snapshot")

    def get_sync_snapshot_info(self) -> dict:
        """GET /sync/snapshot/info -- Metadata about available state sync snapshot."""
        return self._get("/sync/snapshot/info")

    # -- ETH JSON-RPC --

    def eth_call(self, method: str, params: Optional[List[Any]] = None) -> dict:
        """
        POST /eth -- Send an ETH-compatible JSON-RPC request.

        Supports methods like eth_chainId, eth_blockNumber,
        eth_getBalance, eth_call, eth_estimateGas, etc.

        Args:
            method: JSON-RPC method name (e.g. "eth_blockNumber").
            params: JSON-RPC params array (default empty).

        Returns:
            Full JSON-RPC response dict with jsonrpc, id, and
            either result or error.
        """
        payload = {
            "jsonrpc": "2.0",
            "method": method,
            "params": params or [],
            "id": 1,
        }
        return self._post("/eth", payload)
