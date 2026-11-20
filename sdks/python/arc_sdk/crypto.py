from __future__ import annotations

import blake3
from nacl.signing import SigningKey, VerifyKey
from nacl.exceptions import BadSignatureError

from .errors import ArcCryptoError


class KeyPair:
    def __init__(self, signing_key: SigningKey):
        self._signing_key = signing_key
        self._verify_key: VerifyKey = signing_key.verify_key

    @classmethod
    def generate(cls) -> KeyPair:
        return cls(SigningKey.generate())

    @classmethod
    def from_seed(cls, seed: bytes) -> KeyPair:
        if len(seed) != 32:
            raise ArcCryptoError(
                f"Seed must be exactly 32 bytes, got {len(seed)}",
                operation="from_seed",
            )
        return cls(SigningKey(seed))

    @classmethod
    def from_private_key(cls, private_key_hex: str) -> KeyPair:
        try:
            seed = bytes.fromhex(private_key_hex)
        except ValueError as e:
            raise ArcCryptoError(f"Invalid hex string: {e}", operation="from_private_key")
        return cls.from_seed(seed)

    def sign(self, message: bytes) -> bytes:
        try:
            signed = self._signing_key.sign(message)
            return signed.signature
        except Exception as e:
            raise ArcCryptoError(f"Signing failed: {e}", operation="sign")

    def verify(self, message: bytes, signature: bytes) -> bool:
        try:
            self._verify_key.verify(message, signature)
            return True
        except BadSignatureError:
            return False
        except Exception:
            return False

    @staticmethod
    def verify_with_public_key(public_key: bytes, message: bytes, signature: bytes) -> bool:
        try:
            vk = VerifyKey(public_key)
            vk.verify(message, signature)
            return True
        except (BadSignatureError, Exception):
            return False

    def address(self) -> str:
        pubkey_bytes = bytes(self._verify_key)
        digest = blake3.blake3(pubkey_bytes).digest()
        return digest.hex()

    def public_key_hex(self) -> str:
        return bytes(self._verify_key).hex()

    def public_key_bytes(self) -> bytes:
        return bytes(self._verify_key)

    def private_key_hex(self) -> str:
        return bytes(self._signing_key).hex()[:64]

    def __repr__(self) -> str:
        return f"KeyPair(address={self.address()[:16]}...)"

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, KeyPair):
            return NotImplemented
        return self.public_key_hex() == other.public_key_hex()
