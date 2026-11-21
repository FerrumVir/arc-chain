from __future__ import annotations

import struct
from typing import Any, Dict, Optional

import blake3

from .crypto import KeyPair
from .errors import ArcValidationError

_TX_DOMAIN = "ARC-chain-tx-v1"
_TX_DOMAIN_V3 = "ARC-chain-tx-v3"


def _normalize_transaction_domain(value: Optional[str]) -> Optional[str]:
    if value is None:
        return None
    raw = value[2:] if value.lower().startswith("0x") else value
    try:
        encoded = bytes.fromhex(raw)
    except ValueError as error:
        raise ArcValidationError(
            "transaction_domain must be a non-zero 32-byte hex value",
            field="transaction_domain",
        ) from error
    if len(encoded) != 32 or not any(encoded):
        raise ArcValidationError(
            "transaction_domain must be a non-zero 32-byte hex value",
            field="transaction_domain",
        )
    return f"0x{raw.lower()}"


def _encode_body(body: Dict[str, Any]) -> bytes:
    parts: list[bytes] = []
    tx_body_type = body.get("type", "")

    if tx_body_type == "Transfer":
        parts.append(struct.pack("<I", 0))
        parts.append(bytes.fromhex(body["to"]))
        parts.append(struct.pack("<Q", body["amount"]))
        if body.get("amount_commitment"):
            parts.append(b"\x01")
            parts.append(bytes.fromhex(body["amount_commitment"]))
        else:
            parts.append(b"\x00")

    elif tx_body_type == "DeployContract":
        parts.append(struct.pack("<I", 7))
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
        parts.append(struct.pack("<I", 5))
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
        parts.append(struct.pack("<I", 4))
        parts.append(struct.pack("<Q", body["amount"]))
        parts.append(b"\x01" if body.get("is_stake", True) else b"\x00")
        parts.append(bytes.fromhex(body["validator"]))

    elif tx_body_type == "Settle":
        parts.append(struct.pack("<I", 1))
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
        import json

        parts.append(json.dumps(body, sort_keys=True).encode("utf-8"))

    return b"".join(parts)


def _compute_hash(
    tx_type_byte: int,
    from_addr: str,
    nonce: int,
    body: Dict[str, Any],
    fee: int,
    gas_limit: int,
    transaction_domain: Optional[str] = None,
) -> str:
    normalized_domain = _normalize_transaction_domain(transaction_domain)
    h = blake3.blake3(
        derive_key_context=_TX_DOMAIN_V3 if normalized_domain is not None else _TX_DOMAIN
    )
    if normalized_domain is not None:
        h.update(bytes.fromhex(normalized_domain[2:]))
    h.update(bytes([tx_type_byte]))
    h.update(bytes.fromhex(from_addr))
    h.update(struct.pack("<Q", nonce))
    h.update(_encode_body(body))
    h.update(struct.pack("<Q", fee))
    h.update(struct.pack("<Q", gas_limit))
    return h.hexdigest()


class TransactionBuilder:
    @staticmethod
    def transfer(
        from_addr: str,
        to_addr: str,
        amount: int,
        fee: int = 1,
        nonce: int = 0,
        transaction_domain: Optional[str] = None,
    ) -> Dict[str, Any]:
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
        normalized_domain = _normalize_transaction_domain(transaction_domain)
        tx_hash = _compute_hash(
            0x01,
            from_addr,
            nonce,
            body,
            fee,
            0,
            normalized_domain,
        )

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
            "transaction_domain": normalized_domain,
        }

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
            "transaction_domain": None,
        }

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
            "transaction_domain": None,
        }

    @staticmethod
    def stake(
        from_addr: str,
        amount: int,
        validator: Optional[str] = None,
        is_stake: bool = True,
        fee: int = 1,
        nonce: int = 0,
    ) -> Dict[str, Any]:
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
            "transaction_domain": None,
        }

    @staticmethod
    def settle(
        from_addr: str,
        agent_id: str,
        service_hash: str,
        amount: int,
        usage_units: int,
        nonce: int = 0,
    ) -> Dict[str, Any]:
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
            "transaction_domain": None,
        }

    @staticmethod
    def sign(tx: Dict[str, Any], keypair: KeyPair) -> Dict[str, Any]:
        sender = tx.get("from", "")
        kp_addr = keypair.address()
        if sender and sender != kp_addr:
            raise ArcValidationError(
                f"KeyPair address {kp_addr[:16]}... does not match tx sender {sender[:16]}...",
                field="from",
            )

        tx_type_bytes = {
            "Transfer": 0x01,
            "Settle": 0x02,
            "Stake": 0x05,
            "WasmCall": 0x06,
            "DeployContract": 0x08,
        }
        tx_type = tx.get("tx_type")
        if tx_type not in tx_type_bytes:
            raise ArcValidationError(
                f"canonical signing codec is unavailable for {tx_type}",
                field="tx_type",
            )
        tx_hash = _compute_hash(
            tx_type_bytes[tx_type],
            sender,
            tx.get("nonce", 0),
            tx["body"],
            tx.get("fee", 0),
            tx.get("gas_limit", 0),
            tx.get("transaction_domain"),
        )
        tx_hash_bytes = bytes.fromhex(tx_hash)
        signature = keypair.sign(tx_hash_bytes)

        signed = dict(tx)
        signed["hash"] = tx_hash
        signed["signature"] = {
            "Ed25519": {
                "public_key": keypair.public_key_hex(),
                "signature": signature.hex(),
            }
        }
        return signed


def _validate_address(address: str, field_name: str) -> None:
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
