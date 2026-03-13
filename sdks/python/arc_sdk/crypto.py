"""
ARC Chain SDK — Cryptographic primitives.

Provides Ed25519 key pair generation, signing, verification, and
BLAKE3-based address derivation matching the ARC Chain protocol.
"""

from __future__ import annotations

import blake3
from nacl.signing import SigningKey, VerifyKey
from nacl.exceptions import BadSignatureError

from .errors import ArcCryptoError


class KeyPair:
    """
    Ed25519 key pair for ARC Chain transaction signing.

    Addresses are derived as the BLAKE3 hash of the Ed25519 public key,
    matching the Rust implementation: ``Hash256(blake3::hash(pubkey))``.
    """

    def __init__(self, signing_key: SigningKey):
        self._signing_key = signing_key
        self._verify_key: VerifyKey = signing_key.verify_key

    # -- Constructors --

    @classmethod
    def generate(cls) -> KeyPair:
        """Generate a random Ed25519 key pair."""
        return cls(SigningKey.generate())

    @classmethod
    def from_seed(cls, seed: bytes) -> KeyPair:
        """
        Create a deterministic key pair from a 32-byte seed.

        Args:
            seed: Exactly 32 bytes of seed material.

        Raises:
            ArcCryptoError: If the seed is not 32 bytes.
        """
        if len(seed) != 32:
            raise ArcCryptoError(
                f"Seed must be exactly 32 bytes, got {len(seed)}",
                operation="from_seed",
            )
        return cls(SigningKey(seed))

    @classmethod
    def from_private_key(cls, private_key_hex: str) -> KeyPair:
        """
        Import a key pair from a hex-encoded 32-byte private key (seed).

        Args:
            private_key_hex: 64-character hex string of the Ed25519 seed.
        """
        try:
            seed = bytes.fromhex(private_key_hex)
        except ValueError as e:
            raise ArcCryptoError(f"Invalid hex string: {e}", operation="from_private_key")
        return cls.from_seed(seed)

    # -- Signing --

    def sign(self, message: bytes) -> bytes:
        """
        Sign a message and return the 64-byte Ed25519 signature.

        Args:
            message: Arbitrary bytes to sign.

        Returns:
            64-byte signature.
        """
        try:
            signed = self._signing_key.sign(message)
            return signed.signature  # first 64 bytes
        except Exception as e:
            raise ArcCryptoError(f"Signing failed: {e}", operation="sign")

    def verify(self, message: bytes, signature: bytes) -> bool:
        """
        Verify a signature against a message using this key pair's public key.

        Args:
            message: The original message bytes.
            signature: 64-byte Ed25519 signature.

        Returns:
            True if valid, False otherwise.
        """
        try:
            self._verify_key.verify(message, signature)
            return True
        except BadSignatureError:
            return False
        except Exception:
            return False

    @staticmethod
    def verify_with_public_key(public_key: bytes, message: bytes, signature: bytes) -> bool:
        """
        Verify a signature given a raw public key (no key pair needed).

        Args:
            public_key: 32-byte Ed25519 public key.
            message: The original message bytes.
            signature: 64-byte Ed25519 signature.

        Returns:
            True if valid, False otherwise.
        """
        try:
            vk = VerifyKey(public_key)
            vk.verify(message, signature)
            return True
        except (BadSignatureError, Exception):
            return False

    # -- Address derivation --

    def address(self) -> str:
        """
        Derive the ARC Chain address from the public key.

        The address is the BLAKE3 hash of the 32-byte Ed25519 public key,
        returned as a 64-character lowercase hex string.

        Returns:
            64-char hex address string.
        """
        pubkey_bytes = bytes(self._verify_key)
        digest = blake3.blake3(pubkey_bytes).digest()
        return digest.hex()

    def public_key_hex(self) -> str:
        """Return the 32-byte public key as a 64-character hex string."""
        return bytes(self._verify_key).hex()

    def public_key_bytes(self) -> bytes:
        """Return the raw 32-byte public key."""
        return bytes(self._verify_key)

    def private_key_hex(self) -> str:
        """Return the 32-byte seed (private key) as a 64-character hex string."""
        # PyNaCl SigningKey stores seed + pubkey (64 bytes); first 32 = seed
        return bytes(self._signing_key).hex()[:64]

    # -- Dunder --

    def __repr__(self) -> str:
        return f"KeyPair(address={self.address()[:16]}...)"

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, KeyPair):
            return NotImplemented
        return self.public_key_hex() == other.public_key_hex()
