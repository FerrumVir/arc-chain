"""
ARC Chain SDK — Transaction builder.

Constructs unsigned transaction dicts matching the ARC Chain RPC submission
format, then signs them with Ed25519 and computes the BLAKE3 transaction hash.
"""

from __future__ import annotations

import struct
from typing import Any, Dict, List, Optional

import blake3

from .crypto import KeyPair
from .errors import ArcValidationError

# Domain separation key matching the Rust implementation
_TX_DOMAIN = "ARC-chain-tx-v1"


def _encode_body(body: Dict[str, Any]) -> bytes:
    """
    Encode a transaction body to bytes for hashing.

    This produces a deterministic byte representation used for computing
    the BLAKE3 signing hash. The encoding mirrors the Rust bincode
    serialization order.
    """
    parts: list[bytes] = []
    tx_body_type = body.get("type", "")

    if tx_body_type == "Transfer":
        parts.append(b"\x00")  # variant tag
        parts.append(bytes.fromhex(body["to"]))
        parts.append(struct.pack("<Q", body["amount"]))
        # amount_commitment: Option<[u8;32]>
        if body.get("amount_commitment"):
            parts.append(b"\x01")
            parts.append(bytes.fromhex(body["amount_commitment"]))
        else:
            parts.append(b"\x00")

    elif tx_body_type == "DeployContract":
        parts.append(b"\x07")  # variant tag for DeployContract
        code = body.get("bytecode", b"")
        if isinstance(code, str):
            code = bytes.fromhex(code)
        parts.append(struct.pack("<Q", len(code)))
        parts.append(code)
        ctor = body.get("constructor_args", b"")
        if isinstance(ctor, str):
            ctor = bytes.fromhex(ctor)
        parts.append(struct.pack("<Q", len(ctor)))
        parts.append(ctor)
        parts.append(struct.pack("<Q", body.get("state_rent_deposit", 0)))

    elif tx_body_type == "WasmCall":
        parts.append(b"\x05")  # variant tag for WasmCall
        parts.append(bytes.fromhex(body["contract"]))
        func = body["function"].encode("utf-8")
        parts.append(struct.pack("<Q", len(func)))
        parts.append(func)
        calldata = body.get("calldata", b"")
        if isinstance(calldata, str):
            calldata = bytes.fromhex(calldata)
        parts.append(struct.pack("<Q", len(calldata)))
        parts.append(calldata)
        parts.append(struct.pack("<Q", body.get("value", 0)))
        parts.append(struct.pack("<Q", body.get("gas_limit", 1_000_000)))

    elif tx_body_type == "Stake":
        parts.append(b"\x04")  # variant tag for Stake
        parts.append(struct.pack("<Q", body["amount"]))
        parts.append(b"\x01" if body.get("is_stake", True) else b"\x00")
        parts.append(bytes.fromhex(body["validator"]))

    elif tx_body_type == "Settle":
        parts.append(b"\x01")  # variant tag for Settle
        parts.append(bytes.fromhex(body["agent_id"]))
        parts.append(bytes.fromhex(body["service_hash"]))
        parts.append(struct.pack("<Q", body["amount"]))
        parts.append(struct.pack("<Q", body["usage_units"]))
        if body.get("amount_commitment"):
            parts.append(b"\x01")
            parts.append(bytes.fromhex(body["amount_commitment"]))
        else:
            parts.append(b"\x00")

    else:
        # Fallback: serialize as-is for unknown types
        import json
        parts.append(json.dumps(body, sort_keys=True).encode("utf-8"))

    return b"".join(parts)


def _compute_hash(
    tx_type_byte: int, from_addr: str, nonce: int,
    body: Dict[str, Any], fee: int, gas_limit: int,
) -> str:
    """
    Compute the BLAKE3 signing hash for a transaction.

    Matches the Rust ``Transaction::compute_hash()`` method:
    ``tx_type || from || nonce || body || fee || gas_limit``
    """
    h = blake3.blake3(derive_key_context=_TX_DOMAIN)
    h.update(bytes([tx_type_byte]))
    h.update(bytes.fromhex(from_addr))
    h.update(struct.pack("<Q", nonce))
    h.update(_encode_body(body))
    h.update(struct.pack("<Q", fee))
    h.update(struct.pack("<Q", gas_limit))
    return h.hexdigest()


class TransactionBuilder:
    """
    Build unsigned ARC Chain transactions.

    All build methods return a transaction dict that can be signed
    with ``TransactionBuilder.sign()`` and then submitted via
    ``ArcClient.submit_transaction()``.
    """

    # -- Transfer --

    @staticmethod
    def transfer(
        from_addr: str,
        to_addr: str,
        amount: int,
        fee: int = 1,
        nonce: int = 0,
    ) -> Dict[str, Any]:
        """
        Build an unsigned transfer transaction.

        Args:
            from_addr: 64-char hex sender address.
            to_addr: 64-char hex recipient address.
            amount: Amount in ARC tokens (smallest unit).
            fee: Transaction fee (default 1).
            nonce: Sender nonce for replay protection.

        Returns:
            Unsigned transaction dict.
        """
        _validate_address(from_addr, "from_addr")
        _validate_address(to_addr, "to_addr")
        if amount <= 0:
            raise ArcValidationError("Amount must be positive", field="amount")

        body = {
            "type": "Transfer",
            "to": to_addr,
            "amount": amount,
            "amount_commitment": None,
        }
        tx_hash = _compute_hash(0x01, from_addr, nonce, body, fee, 0)

        return {
            "tx_type": "Transfer",
            "from": from_addr,
            "to": to_addr,
            "amount": amount,
            "nonce": nonce,
            "fee": fee,
            "gas_limit": 0,
            "body": body,
            "hash": tx_hash,
            "signature": None,
        }

    # -- Deploy Contract --

    @staticmethod
    def deploy_contract(
        from_addr: str,
        code: bytes,
        gas_limit: int = 1_000_000,
        fee: int = 50,
        nonce: int = 0,
        constructor_args: bytes = b"",
        state_rent_deposit: int = 0,
    ) -> Dict[str, Any]:
        """
        Build an unsigned contract deployment transaction.

        Args:
            from_addr: 64-char hex sender address.
            code: WASM bytecode to deploy.
            gas_limit: Maximum gas for deployment.
            fee: Transaction fee.
            nonce: Sender nonce.
            constructor_args: ABI-encoded constructor arguments.
            state_rent_deposit: Pre-paid state rent.

        Returns:
            Unsigned transaction dict.
        """
        _validate_address(from_addr, "from_addr")
        if not code:
            raise ArcValidationError("Bytecode must not be empty", field="code")

        body = {
            "type": "DeployContract",
            "bytecode": code.hex(),
            "constructor_args": constructor_args.hex(),
            "state_rent_deposit": state_rent_deposit,
        }
        tx_hash = _compute_hash(0x08, from_addr, nonce, body, fee, gas_limit)

        return {
            "tx_type": "DeployContract",
            "from": from_addr,
            "nonce": nonce,
            "fee": fee,
            "gas_limit": gas_limit,
            "body": body,
            "hash": tx_hash,
            "signature": None,
        }

    # -- Call Contract --

    @staticmethod
    def call_contract(
        from_addr: str,
        contract_addr: str,
        calldata: bytes,
        value: int = 0,
        gas_limit: int = 1_000_000,
        function: str = "",
        fee: int = 1,
        nonce: int = 0,
    ) -> Dict[str, Any]:
        """
        Build an unsigned WASM contract call transaction.

        Args:
            from_addr: 64-char hex sender address.
            contract_addr: 64-char hex contract address.
            calldata: ABI-encoded call data.
            value: ARC tokens to send with the call.
            gas_limit: Maximum gas for execution.
            function: Function name to call.
            fee: Transaction fee.
            nonce: Sender nonce.

        Returns:
            Unsigned transaction dict.
        """
        _validate_address(from_addr, "from_addr")
        _validate_address(contract_addr, "contract_addr")

        body = {
            "type": "WasmCall",
            "contract": contract_addr,
            "function": function,
            "calldata": calldata.hex(),
            "value": value,
            "gas_limit": gas_limit,
        }
        tx_hash = _compute_hash(0x06, from_addr, nonce, body, fee, gas_limit)

        return {
            "tx_type": "WasmCall",
            "from": from_addr,
            "nonce": nonce,
            "fee": fee,
            "gas_limit": gas_limit,
            "body": body,
            "hash": tx_hash,
            "signature": None,
        }

    # -- Stake --

    @staticmethod
    def stake(
        from_addr: str,
        amount: int,
        validator: Optional[str] = None,
        is_stake: bool = True,
        fee: int = 1,
        nonce: int = 0,
    ) -> Dict[str, Any]:
        """
        Build an unsigned stake/unstake transaction.

        Args:
            from_addr: 64-char hex sender address.
            amount: Amount to stake or unstake.
            validator: Validator address (defaults to self).
            is_stake: True to stake, False to unstake.
            fee: Transaction fee.
            nonce: Sender nonce.

        Returns:
            Unsigned transaction dict.
        """
        _validate_address(from_addr, "from_addr")
        if amount <= 0:
            raise ArcValidationError("Stake amount must be positive", field="amount")

        validator_addr = validator or from_addr
        _validate_address(validator_addr, "validator")

        body = {
            "type": "Stake",
            "amount": amount,
            "is_stake": is_stake,
            "validator": validator_addr,
        }
        tx_hash = _compute_hash(0x05, from_addr, nonce, body, fee, 0)

        return {
            "tx_type": "Stake",
            "from": from_addr,
            "nonce": nonce,
            "fee": fee,
            "gas_limit": 0,
            "body": body,
            "hash": tx_hash,
            "signature": None,
        }

    # -- Settle --

    @staticmethod
    def settle(
        from_addr: str,
        agent_id: str,
        service_hash: str,
        amount: int,
        usage_units: int,
        nonce: int = 0,
    ) -> Dict[str, Any]:
        """
        Build an unsigned settlement transaction (zero fee).

        Args:
            from_addr: 64-char hex sender address.
            agent_id: 64-char hex agent address.
            service_hash: 64-char hex service hash.
            amount: Settlement amount.
            usage_units: Number of usage units consumed.
            nonce: Sender nonce.

        Returns:
            Unsigned transaction dict.
        """
        _validate_address(from_addr, "from_addr")
        _validate_address(agent_id, "agent_id")

        body = {
            "type": "Settle",
            "agent_id": agent_id,
            "service_hash": service_hash,
            "amount": amount,
            "usage_units": usage_units,
            "amount_commitment": None,
        }
        tx_hash = _compute_hash(0x02, from_addr, nonce, body, 0, 0)

        return {
            "tx_type": "Settle",
            "from": from_addr,
            "nonce": nonce,
            "fee": 0,
            "gas_limit": 0,
            "body": body,
            "hash": tx_hash,
            "signature": None,
        }

    # -- Signing --

    @staticmethod
    def sign(tx: Dict[str, Any], keypair: KeyPair) -> Dict[str, Any]:
        """
        Sign a transaction dict with the given key pair.

        Computes the Ed25519 signature over the transaction hash and attaches
        both the signature and the public key to the transaction.

        Args:
            tx: Unsigned transaction dict (from any build method).
            keypair: Ed25519 key pair whose address matches tx["from"].

        Returns:
            Signed transaction dict (new copy, original is not modified).

        Raises:
            ArcValidationError: If the key pair address does not match tx["from"].
        """
        sender = tx.get("from", "")
        kp_addr = keypair.address()
        if sender and sender != kp_addr:
            raise ArcValidationError(
                f"KeyPair address {kp_addr[:16]}... does not match tx sender {sender[:16]}...",
                field="from",
            )

        tx_hash_bytes = bytes.fromhex(tx["hash"])
        signature = keypair.sign(tx_hash_bytes)

        signed = dict(tx)
        signed["signature"] = {
            "Ed25519": {
                "public_key": keypair.public_key_hex(),
                "signature": signature.hex(),
            }
        }
        return signed


def _validate_address(address: str, field_name: str) -> None:
    """Validate that an address is a 64-character hex string."""
    if not address:
        raise ArcValidationError(f"{field_name} is required", field=field_name)
    if len(address) != 64:
        raise ArcValidationError(
            f"{field_name} must be 64 hex characters, got {len(address)}",
            field=field_name,
        )
    try:
        bytes.fromhex(address)
    except ValueError:
        raise ArcValidationError(f"{field_name} is not valid hex", field=field_name)
