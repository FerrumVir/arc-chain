from __future__ import annotations

import re
from typing import Any, Tuple, Union


_KECCAK_RC = [
    0x0000000000000001,
    0x0000000000008082,
    0x800000000000808A,
    0x8000000080008000,
    0x000000000000808B,
    0x0000000080000001,
    0x8000000080008081,
    0x8000000000008009,
    0x000000000000008A,
    0x0000000000000088,
    0x0000000080008009,
    0x000000008000000A,
    0x000000008000808B,
    0x800000000000008B,
    0x8000000000008089,
    0x8000000000008003,
    0x8000000000008002,
    0x8000000000000080,
    0x000000000000800A,
    0x800000008000000A,
    0x8000000080008081,
    0x8000000000008080,
    0x0000000080000001,
    0x8000000080008008,
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
    A = [[0] * 5 for _ in range(5)]
    for x in range(5):
        for y in range(5):
            A[x][y] = state[x + 5 * y]

    for rc in _KECCAK_RC:
        C = [A[x][0] ^ A[x][1] ^ A[x][2] ^ A[x][3] ^ A[x][4] for x in range(5)]
        D = [C[(x - 1) % 5] ^ _rot64(C[(x + 1) % 5], 1) for x in range(5)]
        for x in range(5):
            for y in range(5):
                A[x][y] = (A[x][y] ^ D[x]) & _MASK64

        B = [[0] * 5 for _ in range(5)]
        for x in range(5):
            for y in range(5):
                B[y][(2 * x + 3 * y) % 5] = _rot64(A[x][y], _KECCAK_ROT[x][y])

        for x in range(5):
            for y in range(5):
                A[x][y] = (B[x][y] ^ ((~B[(x + 1) % 5][y]) & B[(x + 2) % 5][y])) & _MASK64

        A[0][0] = (A[0][0] ^ rc) & _MASK64

    out = [0] * 25
    for x in range(5):
        for y in range(5):
            out[x + 5 * y] = A[x][y]
    return out


def keccak256(data: bytes) -> bytes:
    rate = 136
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

    state = [0] * 25
    for offset in range(0, len(padded), rate):
        block = padded[offset : offset + rate]
        for i in range(rate // 8):
            lane = int.from_bytes(block[i * 8 : i * 8 + 8], "little")
            state[i] ^= lane
        state = _keccak_f1600(state)

    out = b""
    for i in range(4):
        out += state[i].to_bytes(8, "little")
    return out[:32]


_UINT_RE = re.compile(r"^uint(\d+)$")
_INT_RE = re.compile(r"^int(\d+)$")
_BYTES_FIXED_RE = re.compile(r"^bytes(\d+)$")
_ARRAY_FIXED_RE = re.compile(r"^(.+)\[(\d+)\]$")
_ARRAY_DYN_RE = re.compile(r"^(.+)\[\]$")
_TUPLE_RE = re.compile(r"^\((.+)\)$")


def _is_dynamic(typ: str) -> bool:
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
    paren = sig.index("(")
    name = sig[:paren].strip()
    inner = sig[paren + 1 : -1].strip()
    if not inner:
        return name, []
    return name, _split_tuple_types(inner)


def _encode_uint(value: int, bits: int) -> bytes:
    if value < 0:
        raise ValueError(f"uint{bits} cannot be negative, got {value}")
    max_val = (1 << bits) - 1
    if value > max_val:
        raise ValueError(f"Value {value} exceeds uint{bits} max ({max_val})")
    return value.to_bytes(32, "big")


def _encode_int(value: int, bits: int) -> bytes:
    min_val = -(1 << (bits - 1))
    max_val = (1 << (bits - 1)) - 1
    if value < min_val or value > max_val:
        raise ValueError(f"Value {value} out of range for int{bits} [{min_val}, {max_val}]")
    if value >= 0:
        return value.to_bytes(32, "big")
    twos = (1 << 256) + value
    return twos.to_bytes(32, "big")


def _encode_address(value: Union[str, int, bytes]) -> bytes:
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
    return (1 if value else 0).to_bytes(32, "big")


def _encode_bytes_fixed(value: bytes, size: int) -> bytes:
    if len(value) != size:
        raise ValueError(f"bytes{size} requires exactly {size} bytes, got {len(value)}")
    return value.ljust(32, b"\x00")


def _encode_bytes_dynamic(value: bytes) -> bytes:
    length_word = len(value).to_bytes(32, "big")
    padded_len = ((len(value) + 31) // 32) * 32
    padded_data = value.ljust(padded_len, b"\x00")
    return length_word + padded_data


def _encode_string(value: str) -> bytes:
    return _encode_bytes_dynamic(value.encode("utf-8"))


def _encode_single(typ: str, value: Any) -> bytes:
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

    m = _ARRAY_FIXED_RE.match(typ)
    if m:
        inner_type = m.group(1)
        n = int(m.group(2))
        if len(value) != n:
            raise ValueError(f"{typ} requires exactly {n} elements, got {len(value)}")
        return _encode_array_contents(inner_type, list(value))

    m = _ARRAY_DYN_RE.match(typ)
    if m:
        inner_type = m.group(1)
        items = list(value)
        length_word = len(items).to_bytes(32, "big")
        return length_word + _encode_array_contents(inner_type, items)

    m = _TUPLE_RE.match(typ)
    if m:
        subtypes = _split_tuple_types(m.group(1))
        return encode_abi(subtypes, list(value))

    raise ValueError(f"Unsupported ABI type: {typ}")


def _encode_array_contents(inner_type: str, items: list) -> bytes:
    return encode_abi([inner_type] * len(items), items)


def encode_abi(types: list[str], values: list) -> bytes:
    if len(types) != len(values):
        raise ValueError(f"types/values length mismatch: {len(types)} vs {len(values)}")

    dynamic_flags = [_is_dynamic(t) for t in types]

    head_size = 32 * len(types)

    head_parts: list[bytes] = []
    tail_parts: list[bytes] = []

    for typ, val, is_dyn in zip(types, values, dynamic_flags):
        if is_dyn:
            encoded_data = _encode_single(typ, val)
            offset = head_size + sum(len(t) for t in tail_parts)
            head_parts.append(offset.to_bytes(32, "big"))
            tail_parts.append(encoded_data)
        else:
            head_parts.append(_encode_single(typ, val))

    return b"".join(head_parts) + b"".join(tail_parts)


def _decode_uint(data: bytes, offset: int, bits: int) -> Tuple[int, int]:
    word = data[offset : offset + 32]
    val = int.from_bytes(word, "big")
    max_val = (1 << bits) - 1
    val &= max_val
    return val, offset + 32


def _decode_int(data: bytes, offset: int, bits: int) -> Tuple[int, int]:
    word = data[offset : offset + 32]
    val = int.from_bytes(word, "big")
    if val >= (1 << 255):
        val -= 1 << 256
    half = 1 << (bits - 1)
    if val < -half or val >= half:
        val = val % (1 << bits)
        if val >= half:
            val -= 1 << bits
    return val, offset + 32


def _decode_address(data: bytes, offset: int) -> Tuple[str, int]:
    word = data[offset : offset + 32]
    addr = "0x" + word[12:].hex()
    return addr, offset + 32


def _decode_bool(data: bytes, offset: int) -> Tuple[bool, int]:
    word = data[offset : offset + 32]
    val = int.from_bytes(word, "big")
    return val != 0, offset + 32


def _decode_bytes_fixed(data: bytes, offset: int, size: int) -> Tuple[bytes, int]:
    word = data[offset : offset + 32]
    return word[:size], offset + 32


def _decode_bytes_dynamic(data: bytes, offset: int) -> Tuple[bytes, int]:
    length = int.from_bytes(data[offset : offset + 32], "big")
    start = offset + 32
    raw = data[start : start + length]
    padded_len = ((length + 31) // 32) * 32
    return raw, start + padded_len


def _decode_string(data: bytes, offset: int) -> Tuple[str, int]:
    raw, end = _decode_bytes_dynamic(data, offset)
    return raw.decode("utf-8"), end


def _decode_single(typ: str, data: bytes, offset: int, base_offset: int) -> Tuple[Any, int]:
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

    if typ == "string":
        tail_offset = int.from_bytes(data[offset : offset + 32], "big")
        val, _ = _decode_string(data, base_offset + tail_offset)
        return val, offset + 32

    if typ == "bytes":
        tail_offset = int.from_bytes(data[offset : offset + 32], "big")
        val, _ = _decode_bytes_dynamic(data, base_offset + tail_offset)
        return val, offset + 32

    m = _ARRAY_DYN_RE.match(typ)
    if m:
        inner_type = m.group(1)
        tail_offset = int.from_bytes(data[offset : offset + 32], "big")
        abs_offset = base_offset + tail_offset
        count = int.from_bytes(data[abs_offset : abs_offset + 32], "big")
        items = _decode_array_contents(inner_type, count, data, abs_offset + 32, abs_offset + 32)
        return items, offset + 32

    m = _ARRAY_FIXED_RE.match(typ)
    if m:
        inner_type = m.group(1)
        n = int(m.group(2))
        if _is_dynamic(inner_type):
            tail_offset = int.from_bytes(data[offset : offset + 32], "big")
            abs_offset = base_offset + tail_offset
            items = _decode_array_contents(inner_type, n, data, abs_offset, abs_offset)
            return items, offset + 32
        else:
            items = _decode_array_contents(inner_type, n, data, offset, base_offset)
            return items, offset + 32 * n

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
    results: list = []
    cursor = offset
    for _ in range(count):
        val, cursor = _decode_single(inner_type, data, cursor, base_offset)
        results.append(val)
    return results


def _decode_tuple(types: list[str], data: bytes, offset: int, base_offset: int) -> list:
    results: list = []
    cursor = offset
    for typ in types:
        val, cursor = _decode_single(typ, data, cursor, base_offset)
        results.append(val)
    return results


def decode_abi(types: list[str], data: bytes) -> list:
    return _decode_tuple(types, data, 0, 0)


def function_selector(signature: str) -> bytes:
    return keccak256(signature.encode("utf-8"))[:4]


def encode_function_call(function_signature: str, *args: Any) -> bytes:
    _name, param_types = _parse_canonical_signature(function_signature)
    if len(args) != len(param_types):
        raise ValueError(
            f"Signature {function_signature} expects {len(param_types)} args, got {len(args)}"
        )
    selector = function_selector(function_signature)
    encoded_args = encode_abi(param_types, list(args))
    return selector + encoded_args


def decode_function_result(types: list[str], data: bytes) -> list:
    return decode_abi(types, data)


def decode_function_input(function_signature: str, data: bytes) -> Tuple[str, list]:
    expected_selector = function_selector(function_signature)
    actual_selector = data[:4]
    if expected_selector != actual_selector:
        raise ValueError(
            f"Selector mismatch: expected {expected_selector.hex()}, got {actual_selector.hex()}"
        )
    name, param_types = _parse_canonical_signature(function_signature)
    decoded = decode_abi(param_types, data[4:])
    return name, decoded
