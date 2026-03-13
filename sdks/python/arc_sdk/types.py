"""
ARC Chain SDK — Data types.

Typed dataclasses matching the ARC Chain RPC response shapes.
All fields use snake_case to match the JSON keys returned by the node.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional


@dataclass
class Account:
    """An on-chain account."""

    address: str
    balance: int
    nonce: int
    code_hash: Optional[str] = None
    storage_root: Optional[str] = None

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> Account:
        return cls(
            address=data.get("address", ""),
            balance=data.get("balance", 0),
            nonce=data.get("nonce", 0),
            code_hash=data.get("code_hash"),
            storage_root=data.get("storage_root"),
        )


@dataclass
class BlockHeader:
    """Block header fields."""

    height: int
    hash: str
    parent_hash: str
    tx_root: str
    state_root: str
    tx_count: int
    timestamp: int
    producer: str

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> BlockHeader:
        header = data.get("header", data)
        return cls(
            height=header.get("height", 0),
            hash=data.get("hash", ""),
            parent_hash=header.get("parent_hash", ""),
            tx_root=header.get("tx_root", ""),
            state_root=header.get("state_root", ""),
            tx_count=header.get("tx_count", 0),
            timestamp=header.get("timestamp", 0),
            producer=header.get("producer", ""),
        )


@dataclass
class Block:
    """A block on the ARC Chain."""

    header: BlockHeader
    hash: str
    tx_hashes: List[str] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> Block:
        header = BlockHeader.from_dict(data)
        tx_hashes = [
            h if isinstance(h, str) else h.get("hash", "")
            for h in data.get("tx_hashes", [])
        ]
        return cls(
            header=header,
            hash=data.get("hash", ""),
            tx_hashes=tx_hashes,
        )


@dataclass
class EventLog:
    """An event log emitted during contract execution."""

    address: str
    topics: List[str]
    data: str
    block_height: int
    tx_hash: str
    log_index: int

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> EventLog:
        return cls(
            address=data.get("address", ""),
            topics=data.get("topics", []),
            data=data.get("data", ""),
            block_height=data.get("block_height", 0),
            tx_hash=data.get("tx_hash", ""),
            log_index=data.get("log_index", 0),
        )


@dataclass
class Receipt:
    """Transaction receipt (result of execution)."""

    tx_hash: str
    block_height: int
    block_hash: str
    index: int
    success: bool
    gas_used: int
    value_commitment: Optional[str] = None
    inclusion_proof: Optional[str] = None
    logs: List[EventLog] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> Receipt:
        logs = [EventLog.from_dict(log) for log in data.get("logs", [])]
        return cls(
            tx_hash=data.get("tx_hash", ""),
            block_height=data.get("block_height", 0),
            block_hash=data.get("block_hash", ""),
            index=data.get("index", 0),
            success=data.get("success", False),
            gas_used=data.get("gas_used", 0),
            value_commitment=data.get("value_commitment"),
            inclusion_proof=data.get("inclusion_proof"),
            logs=logs,
        )


@dataclass
class ChainInfo:
    """Chain information returned by /info."""

    chain: str
    version: str
    block_height: int
    account_count: int
    mempool_size: int
    gpu: Optional[Dict[str, Any]] = None

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> ChainInfo:
        return cls(
            chain=data.get("chain", ""),
            version=data.get("version", ""),
            block_height=data.get("block_height", 0),
            account_count=data.get("account_count", 0),
            mempool_size=data.get("mempool_size", 0),
            gpu=data.get("gpu"),
        )


@dataclass
class ChainStats:
    """Chain statistics returned by /stats."""

    chain: str
    version: str
    block_height: int
    total_accounts: int
    mempool_size: int
    total_transactions: int
    indexed_hashes: int
    indexed_receipts: int

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> ChainStats:
        return cls(
            chain=data.get("chain", ""),
            version=data.get("version", ""),
            block_height=data.get("block_height", 0),
            total_accounts=data.get("total_accounts", 0),
            mempool_size=data.get("mempool_size", 0),
            total_transactions=data.get("total_transactions", 0),
            indexed_hashes=data.get("indexed_hashes", 0),
            indexed_receipts=data.get("indexed_receipts", 0),
        )


@dataclass
class HealthInfo:
    """Node health returned by /health."""

    status: str
    version: str
    height: int
    peers: int
    uptime_secs: int

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> HealthInfo:
        return cls(
            status=data.get("status", ""),
            version=data.get("version", ""),
            height=data.get("height", 0),
            peers=data.get("peers", 0),
            uptime_secs=data.get("uptime_secs", 0),
        )


@dataclass
class NodeInfo:
    """Node info returned by /node/info."""

    validator: str
    stake: int
    tier: str
    height: int
    version: str
    mempool_size: int

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> NodeInfo:
        return cls(
            validator=data.get("validator", ""),
            stake=data.get("stake", 0),
            tier=data.get("tier", ""),
            height=data.get("height", 0),
            version=data.get("version", ""),
            mempool_size=data.get("mempool_size", 0),
        )


@dataclass
class SubmitResult:
    """Result of a transaction submission."""

    tx_hash: str
    status: str

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> SubmitResult:
        return cls(
            tx_hash=data.get("tx_hash", ""),
            status=data.get("status", ""),
        )


@dataclass
class BatchResult:
    """Result of a batch transaction submission."""

    accepted: int
    rejected: int
    tx_hashes: List[str]

    @classmethod
    def from_dict(cls, data: Dict[str, Any]) -> BatchResult:
        return cls(
            accepted=data.get("accepted", 0),
            rejected=data.get("rejected", 0),
            tx_hashes=data.get("tx_hashes", []),
        )
