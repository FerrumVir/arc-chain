"""
ARC Chain SDK -- Ethereum-standard ABI encoding/decoding.

Provides functions for encoding and decoding function calls using the
Ethereum ABI specification, including function selectors via Keccak-256.

Usage::

    from arc_sdk.abi import encode_function_call, decode_abi

    # Encode a transfer call
    calldata = encode_function_call(
        "transfer(address,uint256)",
        "0x1234567890abcdef1234567890abcdef12345678",
        1000,
    )

    # Decode return data
    values = decode_abi(["uint256", "bool"], return_bytes)
"""

from __future__ import annotations

import struct
import re
from typing import Any, List, Sequence, Tuple, Union


# ---------------------------------------------------------------------------
# Keccak-256 (pure Python, no external deps)
# ---------------------------------------------------------------------------
# Keccak-256 (NOT SHA3-256; they differ in padding).  This is the hash used
# by Ethereum for ABI function selectors.


_KECCAK_RC = [
    0x0000000000000001, 0x0000000000008082, 0x800000000000808A,
    0x8000000080008000, 0x000000000000808B, 0x0000000080000001,
    0x8000000080008081, 0x8000000000008009, 0x000000000000008A,
    0x0000000000000088, 0x0000000080008009, 0x000000008000000A,
    0x000000008000808B, 0x800000000000008B, 0x8000000000008089,
    0x8000000000008003, 0x8000000000008002, 0x8000000000000080,
    0x000000000000800A, 0x800000008000000A, 0x8000000080008081,
    0x8000000000008080, 0x0000000080000001, 0x8000000080008008,
]

_KECCAK_ROT = [
    [0, 36, 3, 41, 18],
    [1, 44, 10, 45, 2],
    [62, 6, 43, 15, 61],
    [28, 55, 25, 21, 56],
    [27, 20, 39, 8, 14],
]

_MASK64 = (1 << 64) - 1


def _rot64(x: int, n: int) -> int:
    return ((x << n) | (x >> (64 - n))) & _MASK64


def _keccak_f1600(state: list[int]) -> list[int]:
    """Apply the Keccak-f[1600] permutation on a 25-element state.

    The state is a flat list of 25 u64 lanes indexed as ``state[x + 5*y]``
    where *x* is the column and *y* is the row (Keccak spec convention).
    """
    # Expand to 2-D for correctness (the flat-indexing version is error-prone
    # due to column-major vs row-major confusion in rho/pi).
    A = [[0] * 5 for _ in range(5)]
    for x in range(5):
        for y in range(5):
            A[x][y] = state[x + 5 * y]

    for rc in _KECCAK_RC:
        # Theta
        C = [A[x][0] ^ A[x][1] ^ A[x][2] ^ A[x][3] ^ A[x][4] for x in range(5)]
        D = [C[(x - 1) % 5] ^ _rot64(C[(x + 1) % 5], 1) for x in range(5)]
        for x in range(5):
            for y in range(5):
                A[x][y] = (A[x][y] ^ D[x]) & _MASK64

        # Rho + Pi
        B = [[0] * 5 for _ in range(5)]
        for x in range(5):
            for y in range(5):
                B[y][(2 * x + 3 * y) % 5] = _rot64(A[x][y], _KECCAK_ROT[x][y])

        # Chi
        for x in range(5):
            for y in range(5):
                A[x][y] = (B[x][y] ^ ((~B[(x + 1) % 5][y]) & B[(x + 2) % 5][y])) & _MASK64

        # Iota
        A[0][0] = (A[0][0] ^ rc) & _MASK64

    # Flatten back to 1-D
    out = [0] * 25
    for x in range(5):
        for y in range(5):
            out[x + 5 * y] = A[x][y]
    return out


def keccak256(data: bytes) -> bytes:
    """
    Compute the Keccak-256 hash of *data*.

    This is the Ethereum-flavour Keccak (padding 0x01) -- **not** NIST SHA3-256
    (which uses padding 0x06).

    Args:
        data: Arbitrary bytes to hash.

    Returns:
        32-byte digest.
    """
    rate = 136  # (1600 - 2*256) / 8 = 136 bytes
    capacity_bytes = 64  # 512 bits / 8

    # Pad: Keccak uses multi-rate padding  10*1
    padding_len = rate - (len(data) % rate)
    if padding_len == 0:
        padding_len = rate
    padded = bytearray(data)
    if padding_len == 1:
        padded.append(0x81)
    else:
        padded.append(0x01)
        padded.extend(b"\x00" * (padding_len - 2))
        padded.append(0x80)

    # Absorb
    state = [0] * 25
    for offset in range(0, len(padded), rate):
        block = padded[offset : offset + rate]
        for i in range(rate // 8):
            lane = int.from_bytes(block[i * 8 : i * 8 + 8], "little")
            state[i] ^= lane
        state = _keccak_f1600(state)

    # Squeeze (only need 32 bytes = 4 lanes)
    out = b""
    for i in range(4):
        out += state[i].to_bytes(8, "little")
    return out[:32]


# ---------------------------------------------------------------------------
# ABI type helpers
# ---------------------------------------------------------------------------

# Regex for parsing type strings
_UINT_RE = re.compile(r"^uint(\d+)$")
_INT_RE = re.compile(r"^int(\d+)$")
_BYTES_FIXED_RE = re.compile(r"^bytes(\d+)$")
_ARRAY_FIXED_RE = re.compile(r"^(.+)\[(\d+)\]$")
_ARRAY_DYN_RE = re.compile(r"^(.+)\[\]$")
_TUPLE_RE = re.compile(r"^\((.+)\)$")


def _is_dynamic(typ: str) -> bool:
    """Return True if the ABI type is dynamically sized."""
    if typ in ("bytes", "string"):
        return True
    m = _ARRAY_DYN_RE.match(typ)
    if m:
        return True
    m = _ARRAY_FIXED_RE.match(typ)
    if m:
        return _is_dynamic(m.group(1))
    m = _TUPLE_RE.match(typ)
    if m:
        subtypes = _split_tuple_types(m.group(1))
        return any(_is_dynamic(t) for t in subtypes)
    return False


def _split_tuple_types(inner: str) -> list[str]:
    """Split comma-separated types inside a tuple, respecting nesting."""
    result: list[str] = []
    depth = 0
    current: list[str] = []
    for ch in inner:
        if ch == "(":
            depth += 1
            current.append(ch)
        elif ch == ")":
            depth -= 1
            current.append(ch)
        elif ch == "," and depth == 0:
            result.append("".join(current).strip())
            current = []
        else:
            current.append(ch)
    if current:
        result.append("".join(current).strip())
    return result


def _parse_canonical_signature(sig: str) -> Tuple[str, list[str]]:
    """
    Parse a canonical function signature like ``transfer(address,uint256)``
    into (function_name, [param_types]).
    """
    paren = sig.index("(")
    name = sig[:paren].strip()
    inner = sig[paren + 1 : -1].strip()
    if not inner:
        return name, []
    return name, _split_tuple_types(inner)


# ---------------------------------------------------------------------------
# Encoding
# ---------------------------------------------------------------------------

def _encode_uint(value: int, bits: int) -> bytes:
    """Encode an unsigned integer, left-padded to 32 bytes."""
    if value < 0:
        raise ValueError(f"uint{bits} cannot be negative, got {value}")
    max_val = (1 << bits) - 1
    if value > max_val:
        raise ValueError(f"Value {value} exceeds uint{bits} max ({max_val})")
    return value.to_bytes(32, "big")


def _encode_int(value: int, bits: int) -> bytes:
    """Encode a signed integer (two's complement), left-padded to 32 bytes."""
    min_val = -(1 << (bits - 1))
    max_val = (1 << (bits - 1)) - 1
    if value < min_val or value > max_val:
        raise ValueError(f"Value {value} out of range for int{bits} [{min_val}, {max_val}]")
    if value >= 0:
        return value.to_bytes(32, "big")
    # Two's complement for negative values in 256 bits
    twos = (1 << 256) + value
    return twos.to_bytes(32, "big")


def _encode_address(value: Union[str, int, bytes]) -> bytes:
    """Encode an Ethereum address (20 bytes, left-padded to 32)."""
    if isinstance(value, str):
        value = value.lower().replace("0x", "")
        addr_bytes = bytes.fromhex(value)
    elif isinstance(value, int):
        addr_bytes = value.to_bytes(20, "big")
    elif isinstance(value, bytes):
        addr_bytes = value
    else:
        raise TypeError(f"Cannot encode address from {type(value)}")
    if len(addr_bytes) > 20:
        raise ValueError(f"Address must be <= 20 bytes, got {len(addr_bytes)}")
    return addr_bytes.rjust(32, b"\x00")


def _encode_bool(value: Any) -> bytes:
    """Encode a boolean as uint256."""
    return (1 if value else 0).to_bytes(32, "big")


def _encode_bytes_fixed(value: bytes, size: int) -> bytes:
    """Encode bytesN (1..32), right-padded to 32 bytes."""
    if len(value) != size:
        raise ValueError(f"bytes{size} requires exactly {size} bytes, got {len(value)}")
    return value.ljust(32, b"\x00")


def _encode_bytes_dynamic(value: bytes) -> bytes:
    """Encode dynamic ``bytes`` -- length prefix + right-padded data."""
    length_word = len(value).to_bytes(32, "big")
    padded_len = ((len(value) + 31) // 32) * 32
    padded_data = value.ljust(padded_len, b"\x00")
    return length_word + padded_data


def _encode_string(value: str) -> bytes:
    """Encode a dynamic ``string`` (UTF-8 encoded bytes)."""
    return _encode_bytes_dynamic(value.encode("utf-8"))


def _encode_single(typ: str, value: Any) -> bytes:
    """
    Encode a single value for a head-only (static) type.
    For dynamic types, this returns the *tail* portion (data after offset).
    """
    if typ == "address":
        return _encode_address(value)
    if typ == "bool":
        return _encode_bool(value)
    if typ == "string":
        return _encode_string(value)
    if typ == "bytes":
        if isinstance(value, str):
            value = bytes.fromhex(value.replace("0x", ""))
        return _encode_bytes_dynamic(value)

    m = _UINT_RE.match(typ)
    if m:
        return _encode_uint(int(value), int(m.group(1)))

    m = _INT_RE.match(typ)
    if m:
        return _encode_int(int(value), int(m.group(1)))

    m = _BYTES_FIXED_RE.match(typ)
    if m:
        size = int(m.group(1))
        if isinstance(value, str):
            value = bytes.fromhex(value.replace("0x", ""))
        return _encode_bytes_fixed(value, size)

    # Fixed-size array: T[N]
    m = _ARRAY_FIXED_RE.match(typ)
    if m:
        inner_type = m.group(1)
        n = int(m.group(2))
        if len(value) != n:
            raise ValueError(f"{typ} requires exactly {n} elements, got {len(value)}")
        return _encode_array_contents(inner_type, list(value))

    # Dynamic array: T[]
    m = _ARRAY_DYN_RE.match(typ)
    if m:
        inner_type = m.group(1)
        items = list(value)
        length_word = len(items).to_bytes(32, "big")
        return length_word + _encode_array_contents(inner_type, items)

    # Tuple: (T1,T2,...)
    m = _TUPLE_RE.match(typ)
    if m:
        subtypes = _split_tuple_types(m.group(1))
        return encode_abi(subtypes, list(value))

    raise ValueError(f"Unsupported ABI type: {typ}")


def _encode_array_contents(inner_type: str, items: list) -> bytes:
    """Encode the contents of an array (shared by fixed and dynamic arrays)."""
    return encode_abi([inner_type] * len(items), items)


def encode_abi(types: list[str], values: list) -> bytes:
    """
    Encode a list of values according to the given ABI types.

    Follows Ethereum ABI encoding rules: static types are encoded inline
    (head section), dynamic types get an offset in the head section pointing
    to their data in the tail section.

    Args:
        types: ABI type strings, e.g. ``["address", "uint256", "bytes"]``.
        values: Corresponding Python values.

    Returns:
        ABI-encoded bytes.

    Raises:
        ValueError: If types and values lengths differ or a type is unsupported.
    """
    if len(types) != len(values):
        raise ValueError(f"types/values length mismatch: {len(types)} vs {len(values)}")

    # Determine which params are dynamic
    dynamic_flags = [_is_dynamic(t) for t in types]

    # Head size = 32 bytes per parameter (offset or inline value)
    head_size = 32 * len(types)

    # Build head and tail
    head_parts: list[bytes] = []
    tail_parts: list[bytes] = []

    for typ, val, is_dyn in zip(types, values, dynamic_flags):
        if is_dyn:
            # Encode the data
            encoded_data = _encode_single(typ, val)
            # Head: offset to the tail data
            offset = head_size + sum(len(t) for t in tail_parts)
            head_parts.append(offset.to_bytes(32, "big"))
            tail_parts.append(encoded_data)
        else:
            # Static: inline in head
            head_parts.append(_encode_single(typ, val))

    return b"".join(head_parts) + b"".join(tail_parts)


# ---------------------------------------------------------------------------
# Decoding
# ---------------------------------------------------------------------------

def _decode_uint(data: bytes, offset: int, bits: int) -> Tuple[int, int]:
    """Decode a uint from 32 bytes at the given offset."""
    word = data[offset : offset + 32]
    val = int.from_bytes(word, "big")
    max_val = (1 << bits) - 1
    val &= max_val
    return val, offset + 32


def _decode_int(data: bytes, offset: int, bits: int) -> Tuple[int, int]:
    """Decode a signed int from 32 bytes at the given offset."""
    word = data[offset : offset + 32]
    val = int.from_bytes(word, "big")
    # Interpret as two's complement
    if val >= (1 << 255):
        val -= 1 << 256
    # Mask to bit width
    half = 1 << (bits - 1)
    if val < -half or val >= half:
        # Wrap into range
        val = val % (1 << bits)
        if val >= half:
            val -= 1 << bits
    return val, offset + 32


def _decode_address(data: bytes, offset: int) -> Tuple[str, int]:
    """Decode an address (last 20 bytes of a 32-byte word)."""
    word = data[offset : offset + 32]
    addr = "0x" + word[12:].hex()
    return addr, offset + 32


def _decode_bool(data: bytes, offset: int) -> Tuple[bool, int]:
    """Decode a bool from a 32-byte word."""
    word = data[offset : offset + 32]
    val = int.from_bytes(word, "big")
    return val != 0, offset + 32


def _decode_bytes_fixed(data: bytes, offset: int, size: int) -> Tuple[bytes, int]:
    """Decode bytesN from 32 bytes (right-padded)."""
    word = data[offset : offset + 32]
    return word[:size], offset + 32


def _decode_bytes_dynamic(data: bytes, offset: int) -> Tuple[bytes, int]:
    """Decode dynamic bytes at the given offset (length-prefixed)."""
    length = int.from_bytes(data[offset : offset + 32], "big")
    start = offset + 32
    raw = data[start : start + length]
    padded_len = ((length + 31) // 32) * 32
    return raw, start + padded_len


def _decode_string(data: bytes, offset: int) -> Tuple[str, int]:
    """Decode a dynamic string at the given offset."""
    raw, end = _decode_bytes_dynamic(data, offset)
    return raw.decode("utf-8"), end


def _decode_single(typ: str, data: bytes, offset: int, base_offset: int) -> Tuple[Any, int]:
    """
    Decode a single ABI-encoded value.

    For dynamic types, *offset* points to the head word (an offset pointer),
    and we resolve the tail location relative to *base_offset*.

    Returns (decoded_value, new_offset_past_head_word).
    """
    if typ == "address":
        return _decode_address(data, offset)
    if typ == "bool":
        return _decode_bool(data, offset)

    m = _UINT_RE.match(typ)
    if m:
        return _decode_uint(data, offset, int(m.group(1)))

    m = _INT_RE.match(typ)
    if m:
        return _decode_int(data, offset, int(m.group(1)))

    m = _BYTES_FIXED_RE.match(typ)
    if m:
        return _decode_bytes_fixed(data, offset, int(m.group(1)))

    # Dynamic types: the head word is an offset pointer
    if typ == "string":
        tail_offset = int.from_bytes(data[offset : offset + 32], "big")
        val, _ = _decode_string(data, base_offset + tail_offset)
        return val, offset + 32

    if typ == "bytes":
        tail_offset = int.from_bytes(data[offset : offset + 32], "big")
        val, _ = _decode_bytes_dynamic(data, base_offset + tail_offset)
        return val, offset + 32

    # Dynamic array: T[]
    m = _ARRAY_DYN_RE.match(typ)
    if m:
        inner_type = m.group(1)
        tail_offset = int.from_bytes(data[offset : offset + 32], "big")
        abs_offset = base_offset + tail_offset
        count = int.from_bytes(data[abs_offset : abs_offset + 32], "big")
        items = _decode_array_contents(inner_type, count, data, abs_offset + 32, abs_offset + 32)
        return items, offset + 32

    # Fixed array: T[N]
    m = _ARRAY_FIXED_RE.match(typ)
    if m:
        inner_type = m.group(1)
        n = int(m.group(2))
        if _is_dynamic(inner_type):
            # Head word is an offset
            tail_offset = int.from_bytes(data[offset : offset + 32], "big")
            abs_offset = base_offset + tail_offset
            items = _decode_array_contents(inner_type, n, data, abs_offset, abs_offset)
            return items, offset + 32
        else:
            items = _decode_array_contents(inner_type, n, data, offset, base_offset)
            return items, offset + 32 * n

    # Tuple: (T1,T2,...)
    m = _TUPLE_RE.match(typ)
    if m:
        subtypes = _split_tuple_types(m.group(1))
        is_tuple_dynamic = any(_is_dynamic(t) for t in subtypes)
        if is_tuple_dynamic:
            tail_offset = int.from_bytes(data[offset : offset + 32], "big")
            abs_offset = base_offset + tail_offset
            vals = _decode_tuple(subtypes, data, abs_offset, abs_offset)
            return tuple(vals), offset + 32
        else:
            vals = _decode_tuple(subtypes, data, offset, base_offset)
            return tuple(vals), offset + 32 * len(subtypes)

    raise ValueError(f"Unsupported ABI type: {typ}")


def _decode_array_contents(
    inner_type: str, count: int, data: bytes, offset: int, base_offset: int
) -> list:
    """Decode *count* elements of *inner_type* starting at *offset*."""
    results: list = []
    cursor = offset
    for _ in range(count):
        val, cursor = _decode_single(inner_type, data, cursor, base_offset)
        results.append(val)
    return results


def _decode_tuple(
    types: list[str], data: bytes, offset: int, base_offset: int
) -> list:
    """Decode a tuple of types starting at *offset*."""
    results: list = []
    cursor = offset
    for typ in types:
        val, cursor = _decode_single(typ, data, cursor, base_offset)
        results.append(val)
    return results


def decode_abi(types: list[str], data: bytes) -> list:
    """
    Decode ABI-encoded data according to the given types.

    Args:
        types: ABI type strings matching the encoded data layout.
        data: The ABI-encoded bytes (without a function selector).

    Returns:
        List of decoded Python values.

    Raises:
        ValueError: If a type is unsupported or data is malformed.
    """
    return _decode_tuple(types, data, 0, 0)


# ---------------------------------------------------------------------------
# Function selectors and call encoding
# ---------------------------------------------------------------------------

def function_selector(signature: str) -> bytes:
    """
    Compute the 4-byte function selector for a canonical signature.

    The selector is the first 4 bytes of the Keccak-256 hash of the
    canonical function signature (no spaces, no parameter names).

    Args:
        signature: Canonical signature, e.g. ``"transfer(address,uint256)"``.

    Returns:
        4-byte selector.

    Example::

        >>> function_selector("transfer(address,uint256)").hex()
        'a9059cbb'
    """
    return keccak256(signature.encode("utf-8"))[:4]


def encode_function_call(function_signature: str, *args: Any) -> bytes:
    """
    Encode a complete function call (selector + ABI-encoded arguments).

    Args:
        function_signature: Canonical function signature, e.g.
            ``"transfer(address,uint256)"``.
        *args: Argument values matching the parameter types in the signature.

    Returns:
        4-byte selector followed by ABI-encoded arguments.

    Example::

        >>> data = encode_function_call(
        ...     "transfer(address,uint256)",
        ...     "0xdead000000000000000000000000000000000000",
        ...     1000,
        ... )
        >>> data[:4].hex()
        'a9059cbb'
    """
    _name, param_types = _parse_canonical_signature(function_signature)
    if len(args) != len(param_types):
        raise ValueError(
            f"Signature {function_signature} expects {len(param_types)} args, got {len(args)}"
        )
    selector = function_selector(function_signature)
    encoded_args = encode_abi(param_types, list(args))
    return selector + encoded_args


def decode_function_result(types: list[str], data: bytes) -> list:
    """
    Decode the return data from a function call.

    Convenience alias for :func:`decode_abi`.

    Args:
        types: ABI type strings of the return values.
        data: Raw bytes returned by the function call.

    Returns:
        List of decoded Python values.
    """
    return decode_abi(types, data)


def decode_function_input(
    function_signature: str, data: bytes
) -> Tuple[str, list]:
    """
    Decode calldata (selector + arguments) given the expected signature.

    Args:
        function_signature: Canonical signature, e.g. ``"transfer(address,uint256)"``.
        data: Full calldata including the 4-byte selector.

    Returns:
        Tuple of (function_name, [decoded_args]).

    Raises:
        ValueError: If the selector does not match.
    """
    expected_selector = function_selector(function_signature)
    actual_selector = data[:4]
    if expected_selector != actual_selector:
        raise ValueError(
            f"Selector mismatch: expected {expected_selector.hex()}, "
            f"got {actual_selector.hex()}"
        )
    name, param_types = _parse_canonical_signature(function_signature)
    decoded = decode_abi(param_types, data[4:])
    return name, decoded
