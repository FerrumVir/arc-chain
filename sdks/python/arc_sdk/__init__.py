"""
ARC Chain Python SDK.

A complete Python client for interacting with the ARC Chain blockchain,
including transaction building, Ed25519 signing, and RPC communication.

Usage::

    from arc_sdk import ArcClient, KeyPair, TransactionBuilder

    # Connect to a node
    client = ArcClient("http://localhost:9000")

    # Generate a key pair
    kp = KeyPair.generate()
    print(f"Address: {kp.address()}")

    # Build and sign a transfer
    tx = TransactionBuilder.transfer(
        from_addr=kp.address(),
        to_addr="0" * 64,
        amount=1000,
    )
    signed_tx = TransactionBuilder.sign(tx, kp)

    # Submit
    tx_hash = client.submit_transaction(signed_tx)
"""

from .client import ArcClient
from .crypto import KeyPair
from .transaction import TransactionBuilder
from .abi import (
    encode_abi,
    decode_abi,
    encode_function_call,
    decode_function_result,
    decode_function_input,
    function_selector,
    keccak256,
)
from .errors import (
    ArcError,
    ArcConnectionError,
    ArcTransactionError,
    ArcValidationError,
    ArcCryptoError,
)
from .types import (
    Account,
    Block,
    BlockHeader,
    Receipt,
    ChainInfo,
    ChainStats,
    EventLog,
    HealthInfo,
    NodeInfo,
)

__version__ = "0.1.0"

__all__ = [
    "ArcClient",
    "KeyPair",
    "TransactionBuilder",
    # ABI encoding/decoding
    "encode_abi",
    "decode_abi",
    "encode_function_call",
    "decode_function_result",
    "decode_function_input",
    "function_selector",
    "keccak256",
    # Errors
    "ArcError",
    "ArcConnectionError",
    "ArcTransactionError",
    "ArcValidationError",
    "ArcCryptoError",
    # Types
    "Account",
    "Block",
    "BlockHeader",
    "Receipt",
    "ChainInfo",
    "ChainStats",
    "EventLog",
    "HealthInfo",
    "NodeInfo",
    "__version__",
]
