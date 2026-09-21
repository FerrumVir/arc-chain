"""Minimal GGUF (v2/v3) header reader and writer. Never reads tensor data.

The reader stops at the end of the tensor-info table, so inspecting a
multi-gigabyte artifact costs a few megabytes of I/O. The writer exists for
tests: it produces small, valid files for the manifest and loader tests.
"""

from __future__ import annotations

import struct
from dataclasses import dataclass, field

MAGIC = b"GGUF"

# Metadata value types.
U8, I8, U16, I16, U32, I32, F32, BOOL, STRING, ARRAY, U64, I64, F64 = range(13)
_SCALAR = {U8: "<B", I8: "<b", U16: "<H", I16: "<h", U32: "<I", I32: "<i",
           F32: "<f", BOOL: "<?", U64: "<Q", I64: "<q", F64: "<d"}

# ggml tensor types (the ones a manifest names; unknown ids are kept numeric).
GGML_TYPES = {0: "F32", 1: "F16", 2: "Q4_0", 3: "Q4_1", 6: "Q5_0", 7: "Q5_1",
              8: "Q8_0", 9: "Q8_1", 10: "Q2_K", 11: "Q3_K", 12: "Q4_K",
              13: "Q5_K", 14: "Q6_K", 15: "Q8_K", 24: "I8", 25: "I16",
              26: "I32", 27: "I64", 28: "F64", 30: "BF16"}

MAX_STRING = 1 << 24      # 16 MiB: no legitimate key or token is larger
MAX_ARRAY = 1 << 24       # 16 Mi entries
MAX_TENSORS = 1 << 16
MAX_DIMS = 4


class GgufError(ValueError):
    pass


@dataclass
class TensorInfo:
    name: str
    dims: list[int]           # GGUF order: innermost (ne0) first
    ggml_type: int
    offset: int

    @property
    def type_name(self) -> str:
        return GGML_TYPES.get(self.ggml_type, f"type{self.ggml_type}")

    @property
    def shape(self) -> list[int]:
        """Row-major shape, outermost first (how candle and ARC see it)."""
        return list(reversed(self.dims))


@dataclass
class Header:
    version: int
    metadata: dict = field(default_factory=dict)
    metadata_types: dict = field(default_factory=dict)
    tensors: list[TensorInfo] = field(default_factory=list)
    header_bytes: int = 0


class _Reader:
    def __init__(self, f, version):
        self.f, self.version = f, version

    def take(self, n: int) -> bytes:
        data = self.f.read(n)
        if len(data) != n:
            raise GgufError("truncated GGUF header")
        return data

    def unpack(self, fmt: str):
        return struct.unpack(fmt, self.take(struct.calcsize(fmt)))[0]

    def count(self) -> int:
        return self.unpack("<I" if self.version == 1 else "<Q")

    def string(self) -> str:
        n = self.count()
        if n > MAX_STRING:
            raise GgufError(f"string of {n} bytes exceeds the reader bound")
        return self.take(n).decode("utf-8")

    def value(self, vtype: int):
        if vtype in _SCALAR:
            return self.unpack(_SCALAR[vtype])
        if vtype == STRING:
            return self.string()
        if vtype == ARRAY:
            etype = self.unpack("<I")
            n = self.count()
            if n > MAX_ARRAY:
                raise GgufError(f"array of {n} entries exceeds the reader bound")
            if etype == ARRAY:
                raise GgufError("nested arrays are not supported")
            return (etype, [self.value(etype) for _ in range(n)])
        raise GgufError(f"unknown metadata value type {vtype}")


def read_header(path) -> Header:
    with open(path, "rb") as f:
        if f.read(4) != MAGIC:
            raise GgufError("not a GGUF file")
        version = struct.unpack("<I", f.read(4))[0]
        if version not in (2, 3):
            raise GgufError(f"unsupported GGUF version {version}")
        r = _Reader(f, version)
        n_tensors, n_kv = r.count(), r.count()
        if n_tensors > MAX_TENSORS:
            raise GgufError(f"{n_tensors} tensors exceeds the reader bound")
        header = Header(version=version)
        for _ in range(n_kv):
            key = r.string()
            vtype = r.unpack("<I")
            value = r.value(vtype)
            if key in header.metadata:
                raise GgufError(f"duplicate metadata key {key}")
            header.metadata[key] = value[1] if vtype == ARRAY else value
            header.metadata_types[key] = (vtype, value[0]) if vtype == ARRAY else (vtype, None)
        names = set()
        for _ in range(n_tensors):
            name = r.string()
            n_dims = r.unpack("<I")
            if not 1 <= n_dims <= MAX_DIMS:
                raise GgufError(f"{name}: {n_dims} dimensions")
            dims = [r.unpack("<Q") for _ in range(n_dims)]
            ggml_type = r.unpack("<I")
            offset = r.unpack("<Q")
            if name in names:
                raise GgufError(f"duplicate tensor {name}")
            names.add(name)
            header.tensors.append(TensorInfo(name, dims, ggml_type, offset))
        header.header_bytes = f.tell()
    return header


# --- writer (tests only) --------------------------------------------------------

def _pack_string(text: str) -> bytes:
    data = text.encode("utf-8")
    return struct.pack("<Q", len(data)) + data


def _pack_value(vtype: int, value, etype=None) -> bytes:
    if vtype in _SCALAR:
        return struct.pack(_SCALAR[vtype], value)
    if vtype == STRING:
        return _pack_string(value)
    if vtype == ARRAY:
        body = b"".join(_pack_value(etype, v) for v in value)
        return struct.pack("<IQ", etype, len(value)) + body
    raise GgufError(f"cannot write value type {vtype}")


def write(path, metadata: list[tuple], tensors: list[tuple], alignment: int = 32) -> None:
    """metadata: (key, vtype, value[, etype]); tensors: (name, shape, f32 values)."""
    out = bytearray(MAGIC + struct.pack("<IQQ", 3, len(tensors), len(metadata)))
    for item in metadata:
        key, vtype, value = item[:3]
        etype = item[3] if len(item) > 3 else None
        out += _pack_string(key) + struct.pack("<I", vtype) + _pack_value(vtype, value, etype)
    blobs, offset = [], 0
    for name, shape, values in tensors:
        dims = list(reversed(shape))
        out += _pack_string(name) + struct.pack("<I", len(dims))
        out += b"".join(struct.pack("<Q", d) for d in dims)
        out += struct.pack("<IQ", 0, offset)  # F32
        blob = struct.pack(f"<{len(values)}f", *values)
        blobs.append(blob)
        offset += len(blob) + (-len(blob)) % alignment
    out += b"\0" * ((-len(out)) % alignment)
    for blob in blobs:
        out += blob + b"\0" * ((-len(blob)) % alignment)
    with open(path, "wb") as f:
        f.write(out)
