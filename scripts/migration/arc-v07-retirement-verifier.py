#!/usr/bin/env python3
"""Fail-closed retirement evidence for a stopped ARC v0.7 community node.

This tool is deliberately local-only.  It never contacts an ARC peer, never
opens a legacy file writable, and never signals a process.  ``create-intent``
either pins a running, explicit ``--stake 0 --min-stake 0`` v0.7 process for an
external TERM-only supervisor or proves an already-stopped install remains
offline.  ``finalize`` verifies independently sealed stop evidence, proves the
process and all legacy listeners remain absent, preserves the exact old tree,
and publishes an atomic create-only receipt.  Local canonical replay is an
optional stronger classification; forensic-only retirement remains valid for
community nodes whose old data belongs to a noncanonical v0.7 fork.

The receipt intentionally does *not* call the v0.7 exit clean.  v0.7 did not
durably journal claimed inference work, so the only honest disposition is
``expired_noncanonical_at_cutover``.  Chain/checkpoint preservation is proven
separately from that inference-job disposition.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Mapping, NoReturn, Protocol, Sequence


INTENT_SCHEMA = "arc.migration.legacy-v07-community-retirement-intent.v1"
RECEIPT_SCHEMA = "arc.migration.legacy-v07-community-retirement-receipt.v1"
STOP_EVIDENCE_SCHEMA = "arc.migration.legacy-v07-term-only-stop-evidence.v1"
PREEXISTING_OFFLINE_EVIDENCE_SCHEMA = (
    "arc.migration.legacy-v07-preexisting-offline-evidence.v1"
)
RELEASE_SCHEMA = "arc.release-manifest-handoff.v1"
INSTALLER_BINDING_SCHEMA = "arc.release-installer-binding.v1"
BOUNDARY_SCHEMA = "arc.recovery.legacy-maintenance-boundary.v1"
CHECKPOINT_DESCRIPTOR_SCHEMA = "arc-recovery-checkpoint-descriptor/v1"
LEGACY_BLOCK_INSPECTION_SCHEMA = "arc.recovery.legacy-block-inspection.v1"
JOBS_DISPOSITION = "expired_noncanonical_at_cutover"
REPOSITORY = "FerrumVir/arc-chain"
CUTOVER_POLICY_SCHEMA = "arc-cutover-policy/v1"
CUTOVER_POLICY_ASSET = "arc-cutover-policy.json"
BOUNDARY_ASSET = "arc-legacy-maintenance-boundary.json"
CHECKPOINT_DESCRIPTOR_ASSET = "arc-recovery-checkpoint-descriptor.json"
CANONICAL_BOUNDARY_HEIGHT = 137_145
REQUIRED_POST_CUTOVER_MIN_HEIGHT = 137_146
RECOVERY_CHAIN_ID = "0x415243"
PRODUCTION_FLEET = (
    ("nyc", "149.28.32.76"),
    ("lax", "140.82.16.112"),
    ("ams", "136.244.109.1"),
    ("lhr", "104.238.171.11"),
    ("nrt", "202.182.107.41"),
    ("sgp", "149.28.153.31"),
)
MAX_JSON_BYTES = 16 * 1024 * 1024
MAX_TREE_ENTRIES = 250_000
MAX_TREE_BYTES = 4 * 1024 * 1024 * 1024 * 1024
HASH_RE = re.compile(r"^[0-9a-f]{64}$")
SIGNATURE_RE = re.compile(r"^[0-9a-f]{128}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
TAG_RE = re.compile(r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
LEGACY_VERSION_RE = re.compile(r"^0\.7\.(0|[1-9][0-9]*)$")
UTC_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")
BOOT_ID_RE = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
)


class RetirementError(RuntimeError):
    """An input or observed host state failed the retirement contract."""


def fail(message: str) -> NoReturn:
    raise RetirementError(message)


def canonical_bytes(value: Any) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


# The compact checkpoint descriptor carries the complete Ed25519 approval
# certificate.  Python's standard library has neither Ed25519 nor BLAKE3, so
# this deliberately small verifier implements only the exact primitives ARC's
# certificate needs: single-block BLAKE3/derive-key inputs and strict Ed25519
# verification.  It is an independent verifier, not a shell-out to the binary
# whose output produced the descriptor.
_U32_MASK = (1 << 32) - 1
_BLAKE3_IV = (
    0x6A09E667,
    0xBB67AE85,
    0x3C6EF372,
    0xA54FF53A,
    0x510E527F,
    0x9B05688C,
    0x1F83D9AB,
    0x5BE0CD19,
)
_BLAKE3_PERMUTATION = (2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8)
_BLAKE3_CHUNK_START = 1
_BLAKE3_CHUNK_END = 2
_BLAKE3_ROOT = 8
_BLAKE3_DERIVE_KEY_CONTEXT = 32
_BLAKE3_DERIVE_KEY_MATERIAL = 64
_RECOVERY_APPROVAL_CONTEXT = b"ARCCHKPT-validator-approval-v1"


def _rotate_right_32(value: int, count: int) -> int:
    return ((value >> count) | (value << (32 - count))) & _U32_MASK


def _blake3_g(
    state: list[int], a: int, b: int, c: int, d: int, first: int, second: int
) -> None:
    state[a] = (state[a] + state[b] + first) & _U32_MASK
    state[d] = _rotate_right_32(state[d] ^ state[a], 16)
    state[c] = (state[c] + state[d]) & _U32_MASK
    state[b] = _rotate_right_32(state[b] ^ state[c], 12)
    state[a] = (state[a] + state[b] + second) & _U32_MASK
    state[d] = _rotate_right_32(state[d] ^ state[a], 8)
    state[c] = (state[c] + state[d]) & _U32_MASK
    state[b] = _rotate_right_32(state[b] ^ state[c], 7)


def _blake3_compress(
    chaining_value: Sequence[int],
    block_words: Sequence[int],
    counter: int,
    block_length: int,
    flags: int,
) -> tuple[int, ...]:
    state = list(chaining_value) + list(_BLAKE3_IV[:4]) + [
        counter & _U32_MASK,
        (counter >> 32) & _U32_MASK,
        block_length,
        flags,
    ]
    message = list(block_words)
    for _round in range(7):
        _blake3_g(state, 0, 4, 8, 12, message[0], message[1])
        _blake3_g(state, 1, 5, 9, 13, message[2], message[3])
        _blake3_g(state, 2, 6, 10, 14, message[4], message[5])
        _blake3_g(state, 3, 7, 11, 15, message[6], message[7])
        _blake3_g(state, 0, 5, 10, 15, message[8], message[9])
        _blake3_g(state, 1, 6, 11, 12, message[10], message[11])
        _blake3_g(state, 2, 7, 8, 13, message[12], message[13])
        _blake3_g(state, 3, 4, 9, 14, message[14], message[15])
        message = [message[index] for index in _BLAKE3_PERMUTATION]
    return tuple(
        [state[index] ^ state[index + 8] for index in range(8)]
        + [state[index + 8] ^ chaining_value[index] for index in range(8)]
    )


def _blake3_short(
    value: bytes, *, key_words: Sequence[int] = _BLAKE3_IV, flags: int = 0
) -> bytes:
    """Return a BLAKE3 digest for the certificate's bounded one-block inputs."""

    if len(value) > 64 or len(key_words) != 8:
        fail("internal BLAKE3 certificate input exceeds the one-block contract")
    padded = value + bytes(64 - len(value))
    words = tuple(
        int.from_bytes(padded[offset : offset + 4], "little")
        for offset in range(0, 64, 4)
    )
    output = _blake3_compress(
        key_words,
        words,
        0,
        len(value),
        flags | _BLAKE3_CHUNK_START | _BLAKE3_CHUNK_END | _BLAKE3_ROOT,
    )
    return b"".join(word.to_bytes(4, "little") for word in output[:8])


def _blake3_derive_key(context: bytes, material: bytes) -> bytes:
    context_key = _blake3_short(context, flags=_BLAKE3_DERIVE_KEY_CONTEXT)
    key_words = tuple(
        int.from_bytes(context_key[offset : offset + 4], "little")
        for offset in range(0, 32, 4)
    )
    return _blake3_short(
        material,
        key_words=key_words,
        flags=_BLAKE3_DERIVE_KEY_MATERIAL,
    )


_ED25519_FIELD = 2**255 - 19
_ED25519_ORDER = 2**252 + 27742317777372353535851937790883648493
_ED25519_D = (-121665 * pow(121666, _ED25519_FIELD - 2, _ED25519_FIELD)) % _ED25519_FIELD
_ED25519_SQRT_M1 = pow(2, (_ED25519_FIELD - 1) // 4, _ED25519_FIELD)
_ED25519_IDENTITY = (0, 1, 1, 0)


def _ed25519_recover_x(y: int, sign: int) -> int:
    numerator = (y * y - 1) % _ED25519_FIELD
    denominator = (_ED25519_D * y * y + 1) % _ED25519_FIELD
    x_squared = numerator * pow(denominator, _ED25519_FIELD - 2, _ED25519_FIELD)
    x_squared %= _ED25519_FIELD
    x = pow(x_squared, (_ED25519_FIELD + 3) // 8, _ED25519_FIELD)
    if (x * x - x_squared) % _ED25519_FIELD != 0:
        x = x * _ED25519_SQRT_M1 % _ED25519_FIELD
    if (x * x - x_squared) % _ED25519_FIELD != 0:
        fail("checkpoint certificate contains an invalid Ed25519 point")
    if (x & 1) != sign:
        x = _ED25519_FIELD - x
    if x == 0 and sign:
        fail("checkpoint certificate contains a non-canonical Ed25519 point")
    return x


def _ed25519_decode(encoded: bytes) -> tuple[int, int, int, int]:
    if len(encoded) != 32:
        fail("checkpoint certificate Ed25519 point is not 32 bytes")
    raw = int.from_bytes(encoded, "little")
    sign = raw >> 255
    y = raw & ((1 << 255) - 1)
    if y >= _ED25519_FIELD:
        fail("checkpoint certificate contains a non-canonical Ed25519 point")
    x = _ed25519_recover_x(y, sign)
    return (x, y, 1, x * y % _ED25519_FIELD)


def _ed25519_add(
    left: tuple[int, int, int, int], right: tuple[int, int, int, int]
) -> tuple[int, int, int, int]:
    x1, y1, z1, t1 = left
    x2, y2, z2, t2 = right
    a = (y1 - x1) * (y2 - x2) % _ED25519_FIELD
    b = (y1 + x1) * (y2 + x2) % _ED25519_FIELD
    c = 2 * _ED25519_D * t1 * t2 % _ED25519_FIELD
    d = 2 * z1 * z2 % _ED25519_FIELD
    e, f, g, h = b - a, d - c, d + c, b + a
    return (
        e * f % _ED25519_FIELD,
        g * h % _ED25519_FIELD,
        f * g % _ED25519_FIELD,
        e * h % _ED25519_FIELD,
    )


def _ed25519_scalar_multiply(
    point: tuple[int, int, int, int], scalar: int
) -> tuple[int, int, int, int]:
    result = _ED25519_IDENTITY
    addend = point
    while scalar:
        if scalar & 1:
            result = _ed25519_add(result, addend)
        addend = _ed25519_add(addend, addend)
        scalar >>= 1
    return result


_ED25519_BASE_Y = 4 * pow(5, _ED25519_FIELD - 2, _ED25519_FIELD) % _ED25519_FIELD
_ED25519_BASE_X = _ed25519_recover_x(_ED25519_BASE_Y, 0)
_ED25519_BASE = (
    _ED25519_BASE_X,
    _ED25519_BASE_Y,
    1,
    _ED25519_BASE_X * _ED25519_BASE_Y % _ED25519_FIELD,
)


def _ed25519_encode(point: tuple[int, int, int, int]) -> bytes:
    x, y, z, _t = point
    inverse = pow(z, _ED25519_FIELD - 2, _ED25519_FIELD)
    affine_x = x * inverse % _ED25519_FIELD
    affine_y = y * inverse % _ED25519_FIELD
    return (affine_y | ((affine_x & 1) << 255)).to_bytes(32, "little")


def _ed25519_verify(public_key: bytes, message: bytes, signature: bytes) -> bool:
    if len(public_key) != 32 or len(signature) != 64:
        return False
    try:
        authority = _ed25519_decode(public_key)
        encoded_r = signature[:32]
        r_point = _ed25519_decode(encoded_r)
    except RetirementError:
        return False
    scalar = int.from_bytes(signature[32:], "little")
    if scalar >= _ED25519_ORDER:
        return False
    # Reject weak/small-order authority keys and R points.  The on-chain
    # verifier accepts only normal validator keys generated by ed25519-dalek.
    if (
        _ed25519_encode(_ed25519_scalar_multiply(authority, 8))
        == _ed25519_encode(_ED25519_IDENTITY)
        or _ed25519_encode(_ed25519_scalar_multiply(r_point, 8))
        == _ed25519_encode(_ED25519_IDENTITY)
    ):
        return False
    challenge = int.from_bytes(
        hashlib.sha512(encoded_r + public_key + message).digest(), "little"
    ) % _ED25519_ORDER
    left = _ed25519_scalar_multiply(_ED25519_BASE, scalar)
    right = _ed25519_add(
        r_point, _ed25519_scalar_multiply(authority, challenge)
    )
    return _ed25519_encode(left) == _ed25519_encode(right)


def require_hash(value: Any, label: str) -> str:
    if not isinstance(value, str) or HASH_RE.fullmatch(value) is None:
        fail(f"{label} must be 64 lowercase hexadecimal characters")
    return value


def require_uint(value: Any, label: str, *, positive: bool = False) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < (1 if positive else 0):
        qualifier = "positive" if positive else "non-negative"
        fail(f"{label} must be a {qualifier} integer")
    return value


def require_utc(value: Any, label: str) -> str:
    if not isinstance(value, str) or UTC_RE.fullmatch(value) is None:
        fail(f"{label} must be canonical UTC seconds")
    try:
        dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError as error:
        fail(f"{label} is invalid: {error}")
    return value


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0).strftime("%Y-%m-%dT%H:%M:%SZ")


def canonical_absolute(path: Path, label: str, *, must_exist: bool = True) -> Path:
    raw = os.fspath(path)
    if not path.is_absolute() or os.path.normpath(raw) != raw or any(
        part in {".", ".."} for part in path.parts[1:]
    ):
        fail(f"{label} must be a canonical absolute path")
    if must_exist and not path.exists():
        fail(f"{label} does not exist: {path}")
    return path


def projection(metadata: os.stat_result) -> dict[str, int]:
    return {
        "device": metadata.st_dev,
        "inode": metadata.st_ino,
        "mode": metadata.st_mode,
        "uid": metadata.st_uid,
        "gid": metadata.st_gid,
        "nlink": metadata.st_nlink,
        "size": metadata.st_size,
        "mtime_ns": metadata.st_mtime_ns,
        "ctime_ns": metadata.st_ctime_ns,
    }


def stable_file(
    path: Path,
    label: str,
    *,
    expected_sha256: str | None = None,
    maximum: int = MAX_TREE_BYTES,
    require_single_link: bool = True,
) -> tuple[bytes, dict[str, Any]]:
    canonical_absolute(path, label)
    expected = require_hash(expected_sha256, f"{label} sha256") if expected_sha256 else None
    before_path = os.lstat(path)
    if not stat.S_ISREG(before_path.st_mode) or stat.S_ISLNK(before_path.st_mode):
        fail(f"{label} must be a non-symlink regular file")
    if before_path.st_mode & 0o022:
        fail(f"{label} must not be group/world writable")
    if require_single_link and before_path.st_nlink != 1:
        fail(f"{label} must have exactly one hard link")
    if before_path.st_size <= 0 or before_path.st_size > maximum:
        fail(f"{label} size is outside the bounded contract")
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(path, flags)
    try:
        before = os.fstat(descriptor)
        if projection(before) != projection(before_path):
            fail(f"{label} pathname changed before its no-follow open")
        chunks: list[bytes] = []
        total = 0
        digest = hashlib.sha256()
        while True:
            chunk = os.read(descriptor, min(1024 * 1024, maximum + 1 - total))
            if not chunk:
                break
            total += len(chunk)
            if total > maximum:
                fail(f"{label} exceeds {maximum} bytes")
            digest.update(chunk)
            chunks.append(chunk)
        after = os.fstat(descriptor)
        if projection(after) != projection(before):
            fail(f"{label} changed while it was read")
    finally:
        os.close(descriptor)
    actual = digest.hexdigest()
    if expected is not None and actual != expected:
        fail(f"{label} sha256 differs from the selected trust root")
    record: dict[str, Any] = projection(before)
    record["sha256"] = actual
    record["path"] = os.fspath(path)
    return b"".join(chunks), record


def load_canonical_json(
    path: Path,
    label: str,
    *,
    expected_sha256: str | None = None,
    maximum: int = MAX_JSON_BYTES,
) -> tuple[dict[str, Any], bytes, dict[str, Any]]:
    raw, record = stable_file(
        path,
        label,
        expected_sha256=expected_sha256,
        maximum=maximum,
    )
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"{label} is invalid JSON: {error}")
    if not isinstance(value, dict) or canonical_bytes(value) != raw:
        fail(f"{label} must be one canonical JSON object")
    return value, raw, record


def safe_output(path: Path, label: str, *, forbidden_root: Path | None = None) -> Path:
    canonical_absolute(path, label, must_exist=False)
    parent = canonical_absolute(path.parent, f"{label} parent")
    parent_stat = os.lstat(parent)
    if not stat.S_ISDIR(parent_stat.st_mode) or stat.S_ISLNK(parent_stat.st_mode):
        fail(f"{label} parent must be a non-symlink directory")
    if parent_stat.st_uid not in {0, os.geteuid()} or parent_stat.st_mode & 0o022:
        fail(f"{label} parent ownership/mode is unsafe")
    if forbidden_root is not None:
        root = canonical_absolute(forbidden_root, "legacy data directory")
        resolved_parent = parent.resolve(strict=True)
        resolved_root = root.resolve(strict=True)
        if resolved_parent == resolved_root or resolved_parent.is_relative_to(resolved_root):
            fail(f"{label} must be outside the legacy data tree")
    return path


def require_fresh_v08_path(path: Path, old_data_dir: Path) -> Path:
    canonical_absolute(path, "v0.8 data directory", must_exist=False)
    canonical_absolute(path.parent, "v0.8 data-directory parent")
    if os.path.lexists(path):
        fail("v0.8 data directory must remain absent until the retirement receipt is sealed")
    parent_stat = os.lstat(path.parent)
    if (
        not stat.S_ISDIR(parent_stat.st_mode)
        or stat.S_ISLNK(parent_stat.st_mode)
        or parent_stat.st_uid not in {0, os.geteuid()}
        or parent_stat.st_mode & 0o022
    ):
        fail("v0.8 data-directory parent ownership/mode is unsafe")
    selected = path.parent.resolve(strict=True) / path.name
    old = canonical_absolute(old_data_dir, "legacy data directory").resolve(strict=True)
    if selected == old or selected.is_relative_to(old) or old.is_relative_to(selected):
        fail("fresh v0.8 data directory must be disjoint from the legacy data tree")
    return path


def publish_create_only_atomic(
    path: Path,
    value: Mapping[str, Any],
    label: str,
    *,
    fault: Callable[[str], None] | None = None,
) -> str:
    """Atomically publish complete bytes without ever replacing PATH.

    A fully fsynced private inode is hard-linked to the final name.  A crash
    before the link leaves no final file; a crash after it leaves complete,
    valid final bytes.  Retrying identical bytes is therefore idempotent.
    """

    payload = canonical_bytes(value)
    parent = path.parent
    parent_fd = os.open(
        parent,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    temporary = f".{path.name}.publish-{uuid.uuid4().hex}.tmp"
    descriptor = -1
    linked = False
    try:
        try:
            descriptor = os.open(
                temporary,
                os.O_WRONLY
                | os.O_CREAT
                | os.O_EXCL
                | getattr(os, "O_CLOEXEC", 0)
                | getattr(os, "O_NOFOLLOW", 0),
                0o400,
                dir_fd=parent_fd,
            )
            offset = 0
            while offset < len(payload):
                written = os.write(descriptor, payload[offset:])
                if written <= 0:
                    fail(f"{label} publication made no progress")
                offset += written
            os.fsync(descriptor)
            os.fchmod(descriptor, 0o400)
            if fault:
                fault("after_file_fsync")
            try:
                os.link(
                    temporary,
                    path.name,
                    src_dir_fd=parent_fd,
                    dst_dir_fd=parent_fd,
                    follow_symlinks=False,
                )
                linked = True
                os.fsync(parent_fd)
                if fault:
                    fault("after_link")
            except FileExistsError:
                existing, _record = stable_file(
                    path,
                    f"existing {label}",
                    maximum=len(payload),
                    require_single_link=False,
                )
                if existing != payload:
                    fail(f"existing {label} differs; refusing replacement")
        finally:
            if descriptor >= 0:
                os.close(descriptor)
            try:
                os.unlink(temporary, dir_fd=parent_fd)
                os.fsync(parent_fd)
            except FileNotFoundError:
                pass
    finally:
        os.close(parent_fd)
    if not linked:
        existing, _record = stable_file(
            path,
            f"existing {label}",
            maximum=len(payload),
            require_single_link=False,
        )
        if existing != payload:
            fail(f"existing {label} differs after publication")
    return sha256_bytes(payload)


def wal_prefix_record(path: Path, label: str = "legacy state WAL") -> dict[str, Any]:
    """Pin the current append-only prefix while tolerating concurrent appends."""

    canonical_absolute(path, label)
    path_before = os.lstat(path)
    if path_before.st_nlink != 1:
        fail(f"{label} must have exactly one hard link")
    if (
        not stat.S_ISREG(path_before.st_mode)
        or stat.S_ISLNK(path_before.st_mode)
        or path_before.st_mode & 0o022
        or path_before.st_size <= 0
    ):
        fail(f"{label} identity/mode is unsafe")
    prefix_bytes = path_before.st_size
    descriptor = os.open(
        path,
        os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    try:
        opened = os.fstat(descriptor)
        stable_identity_fields = ("device", "inode", "mode", "uid", "gid", "nlink")
        path_projection = projection(path_before)
        open_projection = projection(opened)
        if any(path_projection[field] != open_projection[field] for field in stable_identity_fields):
            fail(f"{label} pathname changed before its no-follow open")
        digest = hashlib.sha256()
        remaining = prefix_bytes
        while remaining:
            chunk = os.read(descriptor, min(1024 * 1024, remaining))
            if not chunk:
                fail(f"{label} shrank while its prefix was read")
            digest.update(chunk)
            remaining -= len(chunk)
        after = os.fstat(descriptor)
        after_projection = projection(after)
        if any(open_projection[field] != after_projection[field] for field in stable_identity_fields):
            fail(f"{label} identity changed while its prefix was read")
        if after.st_size < prefix_bytes:
            fail(f"{label} shrank while its prefix was read")
    finally:
        os.close(descriptor)
    return {
        "path": os.fspath(path),
        "device": open_projection["device"],
        "inode": open_projection["inode"],
        "mode": open_projection["mode"],
        "uid": open_projection["uid"],
        "gid": open_projection["gid"],
        "nlink": open_projection["nlink"],
        "observed_prefix_bytes": prefix_bytes,
        "observed_prefix_sha256": digest.hexdigest(),
    }


def verify_wal_prefix(path: Path, expected: Mapping[str, Any]) -> dict[str, Any]:
    current = wal_prefix_record(path)
    for field in ("path", "device", "inode", "mode", "uid", "gid", "nlink"):
        if current.get(field) != expected.get(field):
            fail(f"legacy state WAL {field} differs from the retirement intent")
    expected_bytes = require_uint(expected.get("observed_prefix_bytes"), "intent WAL prefix bytes", positive=True)
    expected_hash = require_hash(expected.get("observed_prefix_sha256"), "intent WAL prefix sha256")
    if current["observed_prefix_bytes"] < expected_bytes:
        fail("legacy state WAL shrank after retirement intent")
    descriptor = os.open(
        path,
        os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    try:
        digest = hashlib.sha256()
        remaining = expected_bytes
        while remaining:
            chunk = os.read(descriptor, min(1024 * 1024, remaining))
            if not chunk:
                fail("legacy state WAL shrank while verifying its intent prefix")
            digest.update(chunk)
            remaining -= len(chunk)
    finally:
        os.close(descriptor)
    if digest.hexdigest() != expected_hash:
        fail("legacy state WAL changed inside the prefix sealed by the retirement intent")
    return current


def tree_snapshot(root: Path) -> dict[str, Any]:
    """Hash a legacy tree through no-follow descriptors without modifying it."""

    canonical_absolute(root, "legacy data directory")
    root_path_stat = os.lstat(root)
    if not stat.S_ISDIR(root_path_stat.st_mode) or stat.S_ISLNK(root_path_stat.st_mode):
        fail("legacy data directory must be a non-symlink directory")
    if root_path_stat.st_mode & 0o022:
        fail("legacy data directory must not be group/world writable")
    records: list[dict[str, Any]] = []
    total_bytes = 0

    def walk(directory_fd: int, relative: str) -> None:
        nonlocal total_bytes
        try:
            names = sorted(os.listdir(directory_fd))
        except OSError as error:
            fail(f"cannot enumerate legacy data tree {relative or '.'}: {error}")
        for name in names:
            if name in {"", ".", ".."} or "/" in name or "\x00" in name:
                fail("legacy data tree contains an unsafe entry name")
            rel = f"{relative}/{name}" if relative else name
            if len(records) >= MAX_TREE_ENTRIES:
                fail(f"legacy data tree exceeds {MAX_TREE_ENTRIES} entries")
            before = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
            if stat.S_ISLNK(before.st_mode):
                fail(f"legacy data tree contains a symlink: {rel}")
            if before.st_mode & 0o022:
                fail(f"legacy data tree entry is group/world writable: {rel}")
            base: dict[str, Any] = {
                "path": rel,
                "device": before.st_dev,
                "inode": before.st_ino,
                "mode": before.st_mode,
                "uid": before.st_uid,
                "gid": before.st_gid,
                "nlink": before.st_nlink,
                "mtime_ns": before.st_mtime_ns,
                "ctime_ns": before.st_ctime_ns,
            }
            if stat.S_ISDIR(before.st_mode):
                child_fd = os.open(
                    name,
                    os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
                    dir_fd=directory_fd,
                )
                try:
                    opened = os.fstat(child_fd)
                    if projection(opened) != projection(before):
                        fail(f"legacy directory changed before its no-follow open: {rel}")
                    base["kind"] = "directory"
                    records.append(base)
                    walk(child_fd, rel)
                    after = os.fstat(child_fd)
                    if projection(after) != projection(opened):
                        fail(f"legacy directory changed during tree inspection: {rel}")
                finally:
                    os.close(child_fd)
            elif stat.S_ISREG(before.st_mode):
                if before.st_nlink != 1:
                    fail(f"legacy data file must have exactly one hard link: {rel}")
                file_fd = os.open(
                    name,
                    os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0),
                    dir_fd=directory_fd,
                )
                try:
                    opened = os.fstat(file_fd)
                    if projection(opened) != projection(before):
                        fail(f"legacy file changed before its no-follow open: {rel}")
                    digest = hashlib.sha256()
                    observed = 0
                    while True:
                        chunk = os.read(file_fd, 1024 * 1024)
                        if not chunk:
                            break
                        observed += len(chunk)
                        total_bytes += len(chunk)
                        if total_bytes > MAX_TREE_BYTES:
                            fail("legacy data tree exceeds its aggregate byte bound")
                        digest.update(chunk)
                    after = os.fstat(file_fd)
                    if projection(after) != projection(opened) or observed != opened.st_size:
                        fail(f"legacy file changed while it was hashed: {rel}")
                finally:
                    os.close(file_fd)
                base.update({"kind": "file", "size": observed, "sha256": digest.hexdigest()})
                records.append(base)
            else:
                fail(f"legacy data tree contains a non-file/non-directory entry: {rel}")

    root_fd = os.open(
        root,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    try:
        root_open = os.fstat(root_fd)
        if projection(root_open) != projection(root_path_stat):
            fail("legacy data directory changed before its no-follow open")
        walk(root_fd, "")
        root_after = os.fstat(root_fd)
        if projection(root_after) != projection(root_open):
            fail("legacy data directory changed during tree inspection")
    finally:
        os.close(root_fd)
    root_record = projection(root_open)
    semantic = {
        "schema": "arc.migration.legacy-v07-data-tree.v1",
        "root": {"path": os.fspath(root), **root_record},
        "entries": records,
        "entry_count": len(records),
        "total_file_bytes": total_bytes,
    }
    return {
        "root_sha256": sha256_bytes(canonical_bytes(semantic)),
        "root": semantic["root"],
        "entry_count": len(records),
        "total_file_bytes": total_bytes,
        "entries": records,
    }


@dataclass(frozen=True)
class ProcessObservation:
    pid: int
    boot_id: str
    start_ticks: int
    uid: int
    gid: int
    executable: Mapping[str, Any]
    argv: tuple[str, ...]
    cwd: str
    listeners: tuple[Mapping[str, Any], ...]


class Runtime(Protocol):
    def observe_process(self, pid: int) -> ProcessObservation | None: ...

    def matching_processes(self, data_dir: str, executable_sha256: str) -> list[ProcessObservation]: ...

    def active_listener_endpoints(self) -> set[tuple[str, str, int]]: ...


def _parse_proc_stat(raw: str) -> int:
    close = raw.rfind(")")
    if close < 2:
        fail("process stat record is malformed")
    fields = raw[close + 2 :].split()
    if len(fields) <= 19:
        fail("process stat record omits start ticks")
    try:
        value = int(fields[19], 10)
    except ValueError:
        fail("process start ticks are malformed")
    if value <= 0:
        fail("process start ticks must be positive")
    return value


def _parse_tcp_table(path: Path, family: str) -> list[dict[str, Any]]:
    try:
        lines = path.read_text(encoding="ascii").splitlines()
    except (OSError, UnicodeError) as error:
        fail(f"cannot read {path}: {error}")
    rows: list[dict[str, Any]] = []
    for line in lines[1:]:
        fields = line.split()
        if len(fields) < 10 or fields[3] != "0A":
            continue
        try:
            address, port_hex = fields[1].split(":", 1)
            port = int(port_hex, 16)
            inode = int(fields[9], 10)
        except (ValueError, IndexError):
            fail(f"kernel listener row is malformed in {path}")
        rows.append({"family": family, "address_hex": address.upper(), "port": port, "inode": inode})
    return rows


def stable_proc_executable(path: Path, label: str) -> dict[str, Any]:
    """Hash the regular executable reached by Linux's intentional /proc symlink."""

    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_CLOEXEC", 0))
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_size <= 0 or before.st_size > 1024 * 1024 * 1024:
            fail(f"{label} target is not a bounded regular file")
        digest = hashlib.sha256()
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
        after = os.fstat(descriptor)
        if projection(after) != projection(before):
            fail(f"{label} changed while it was hashed")
    finally:
        os.close(descriptor)
    value: dict[str, Any] = projection(before)
    value.update({"path": os.readlink(path), "sha256": digest.hexdigest()})
    return value


class LinuxProcRuntime:
    def __init__(self, proc_root: Path = Path("/proc")) -> None:
        self.proc_root = proc_root
        if sys.platform != "linux" or proc_root != Path("/proc"):
            # Alternate roots are intentionally available only to direct unit tests.
            if proc_root == Path("/proc"):
                fail("v0.7 process retirement verification currently requires Linux /proc")

    def _boot_id(self) -> str:
        try:
            value = (self.proc_root / "sys/kernel/random/boot_id").read_text(encoding="ascii").strip()
        except (OSError, UnicodeError) as error:
            fail(f"cannot read kernel boot identity: {error}")
        if BOOT_ID_RE.fullmatch(value) is None:
            fail("kernel boot identity is malformed")
        return value

    def _listeners(self) -> list[dict[str, Any]]:
        rows = _parse_tcp_table(self.proc_root / "net/tcp", "tcp4")
        tcp6 = self.proc_root / "net/tcp6"
        if tcp6.exists():
            rows.extend(_parse_tcp_table(tcp6, "tcp6"))
        return rows

    def observe_process(self, pid: int) -> ProcessObservation | None:
        if isinstance(pid, bool) or pid <= 1:
            fail("legacy PID must be greater than 1")
        process_root = self.proc_root / str(pid)
        try:
            start_before = _parse_proc_stat((process_root / "stat").read_text(encoding="utf-8"))
        except FileNotFoundError:
            return None
        except (OSError, UnicodeError) as error:
            fail(f"cannot inspect legacy PID {pid}: {error}")
        try:
            raw_cmdline = (process_root / "cmdline").read_bytes()
            argv_bytes = raw_cmdline.split(b"\0")
            if argv_bytes and argv_bytes[-1] == b"":
                argv_bytes.pop()
            argv = tuple(part.decode("utf-8", "strict") for part in argv_bytes)
            if not argv:
                fail(f"legacy PID {pid} has no command line")
            cwd = os.readlink(process_root / "cwd")
            process_stat = os.stat(process_root)
            executable = stable_proc_executable(
                process_root / "exe", f"legacy PID {pid} executable"
            )
            socket_inodes: set[int] = set()
            for fd_name in os.listdir(process_root / "fd"):
                try:
                    target = os.readlink(process_root / "fd" / fd_name)
                except FileNotFoundError:
                    continue
                match = re.fullmatch(r"socket:\[([0-9]+)\]", target)
                if match:
                    socket_inodes.add(int(match.group(1)))
            listeners = tuple(
                sorted(
                    (row for row in self._listeners() if row["inode"] in socket_inodes),
                    key=lambda row: (row["family"], row["address_hex"], row["port"], row["inode"]),
                )
            )
            start_after = _parse_proc_stat((process_root / "stat").read_text(encoding="utf-8"))
        except FileNotFoundError:
            fail(f"legacy PID {pid} exited during retirement-intent inspection")
        except (OSError, UnicodeError) as error:
            fail(f"cannot inspect legacy PID {pid}: {error}")
        if start_after != start_before:
            fail(f"legacy PID {pid} identity changed during inspection")
        return ProcessObservation(
            pid=pid,
            boot_id=self._boot_id(),
            start_ticks=start_before,
            uid=process_stat.st_uid,
            gid=process_stat.st_gid,
            executable=executable,
            argv=argv,
            cwd=cwd,
            listeners=listeners,
        )

    def matching_processes(self, data_dir: str, executable_sha256: str) -> list[ProcessObservation]:
        matches: list[ProcessObservation] = []
        try:
            pids = sorted(int(name) for name in os.listdir(self.proc_root) if name.isdigit())
        except OSError as error:
            fail(f"cannot enumerate processes: {error}")
        for pid in pids:
            if pid <= 1:
                continue
            try:
                observed = self.observe_process(pid)
            except RetirementError:
                # Permission-denied or transient unrelated processes cannot be
                # used as affirmative absence proof.  Only tolerate a process
                # that disappeared between enumeration and observation.
                if not (self.proc_root / str(pid)).exists():
                    continue
                raise
            if observed is None:
                continue
            argv_hash = observed.executable.get("sha256")
            semantic_match = False
            try:
                parsed = parse_stake_zero_argv(observed.argv, Path(data_dir))
                semantic_match = parsed["data_dir"] == data_dir
            except RetirementError:
                pass
            if argv_hash == executable_sha256 or semantic_match:
                matches.append(observed)
        return matches

    def active_listener_endpoints(self) -> set[tuple[str, str, int]]:
        return {(row["family"], row["address_hex"], row["port"]) for row in self._listeners()}


def parse_stake_zero_argv(argv: Sequence[str], expected_data_dir: Path) -> dict[str, Any]:
    if not argv:
        fail("legacy process command line is empty")
    stakes: list[str] = []
    minimums: list[str] = []
    data_dirs: list[str] = []
    community_flag = False
    index = 1
    while index < len(argv):
        token = argv[index]
        if token in {"--config", "-c"} or token.startswith("--config="):
            fail("legacy retirement requires explicit CLI state; config-file overrides are forbidden")
        if token in {"--benchmark", "--proposer-mode"}:
            fail(f"legacy stake-zero retirement rejects active role {token}")
        if token == "--community-mode":
            community_flag = True
            index += 1
            continue
        matched = False
        for name, destination in (
            ("--stake", stakes),
            ("--min-stake", minimums),
            ("--data-dir", data_dirs),
        ):
            if token == name:
                if index + 1 >= len(argv):
                    fail(f"legacy process {name} has no value")
                destination.append(argv[index + 1])
                index += 2
                matched = True
                break
            prefix = name + "="
            if token.startswith(prefix):
                destination.append(token[len(prefix) :])
                index += 1
                matched = True
                break
        if not matched:
            index += 1
    if stakes != ["0"] or minimums != ["0"]:
        fail("legacy process must explicitly contain exactly one --stake 0 and --min-stake 0")
    if len(data_dirs) != 1:
        fail("legacy process must explicitly contain exactly one --data-dir")
    supplied = Path(data_dirs[0])
    if not supplied.is_absolute() or os.path.normpath(os.fspath(supplied)) != os.fspath(supplied):
        fail("legacy process --data-dir must be canonical absolute")
    if supplied != expected_data_dir:
        fail("legacy process --data-dir differs from the selected old data tree")
    return {
        "stake": 0,
        "minimum_stake": 0,
        "data_dir": os.fspath(supplied),
        "community_mode_explicit": community_flag,
        "community_mode_effective": True,
    }


def validate_supervisor_binding(
    value: Mapping[str, Any],
    *,
    data_dir: Path,
    legacy_executable: Path,
    legacy_executable_sha256: str,
) -> tuple[dict[str, Any], dict[str, Any]]:
    if set(value) != {
        "schema",
        "kind",
        "source_path",
        "source_sha256",
        "executable_path",
        "executable_sha256",
        "argv",
    } or value.get("schema") != "arc.migration.legacy-v07-supervisor-binding.v1":
        fail("legacy supervisor binding has missing, unknown, or unsupported fields")
    if value.get("kind") not in {"systemd", "launchd", "manual"}:
        fail("legacy supervisor kind is unsupported")
    if value.get("executable_path") != os.fspath(legacy_executable) or value.get(
        "executable_sha256"
    ) != legacy_executable_sha256:
        fail("legacy supervisor executable binding differs")
    argv = value.get("argv")
    if not isinstance(argv, list) or not argv or not all(isinstance(item, str) for item in argv):
        fail("legacy supervisor argv is malformed")
    semantic = parse_stake_zero_argv(argv, data_dir)
    source_path_raw = value.get("source_path")
    if not isinstance(source_path_raw, str):
        fail("legacy supervisor source path is missing")
    source_path = canonical_absolute(Path(source_path_raw), "legacy supervisor source")
    _source_raw, source_record = stable_file(
        source_path,
        "legacy supervisor source",
        expected_sha256=require_hash(value.get("source_sha256"), "legacy supervisor source sha256"),
    )
    return semantic, source_record


def validate_release(
    value: Mapping[str, Any],
    inspector_asset: str | None,
    inspector_sha: str | None,
    cutover_policy_sha: str,
    boundary_sha: str,
    checkpoint_descriptor_sha: str,
) -> dict[str, Any]:
    schema = value.get("schema")
    if schema == RELEASE_SCHEMA:
        required = {
            "schema",
            "sealed",
            "repository",
            "commit",
            "tag",
            "workflow_run_id",
            "workflow_run_attempt",
            "files",
            "modes",
            "manifest_sha256",
        }
        if set(value) != required or value.get("sealed") is not True:
            fail("target release handoff has missing, unknown, or unsealed fields")
        modes = value.get("modes")
        manifest_sha = value.get("manifest_sha256")
        signature_sha: str | None = None
        require_uint(value.get("workflow_run_id"), "target release workflow run id", positive=True)
        require_uint(value.get("workflow_run_attempt"), "target release workflow attempt", positive=True)
    elif schema == INSTALLER_BINDING_SCHEMA:
        required = {
            "schema",
            "repository",
            "tag",
            "commit",
            "signed_manifest_sha256",
            "manifest_signature_sha256",
            "files",
        }
        if set(value) != required:
            fail("installer release binding has missing or unknown fields")
        modes = None
        manifest_sha = value.get("signed_manifest_sha256")
        signature_sha = require_hash(
            value.get("manifest_signature_sha256"), "release manifest signature sha256"
        )
    else:
        fail("target release binding schema is unsupported")
    if value.get("repository") != REPOSITORY:
        fail("target release handoff repository differs")
    commit = value.get("commit")
    if not isinstance(commit, str) or COMMIT_RE.fullmatch(commit) is None:
        fail("target release commit is not one full lowercase Git SHA")
    tag = value.get("tag")
    match = TAG_RE.fullmatch(tag) if isinstance(tag, str) else None
    if match is None or tuple(int(match.group(i)) for i in (1, 2, 3)) < (0, 8, 0):
        fail("target release must be a strict immutable v0.8.0-or-newer release")
    files = value.get("files")
    if not isinstance(files, dict):
        fail("target release files map is missing")
    if modes is not None and (not isinstance(modes, dict) or set(files) != set(modes)):
        fail("target release file/mode maps differ")
    for name, digest in files.items():
        if not isinstance(name, str) or not name or "/" in name or require_hash(digest, f"release file {name}") != digest:
            fail("target release contains an unsafe file record")
        if modes is not None:
            require_uint(modes[name], f"target release mode {name}")
    if (inspector_asset is None) != (inspector_sha is None):
        fail("optional local inspector asset/hash must be supplied together")
    if inspector_asset is not None and inspector_sha is not None:
        if files.get(inspector_asset) != require_hash(inspector_sha, "inspector binary sha256"):
            fail("inspector binary is not the exact selected target-release asset")
        if modes is not None and modes.get(inspector_asset) != 0o755:
            fail("target release inspector mode differs from the executable contract")
    if files.get(CUTOVER_POLICY_ASSET) != require_hash(
        cutover_policy_sha, "cutover policy sha256"
    ):
        fail("cutover policy is not the exact selected target-release asset")
    if modes is not None and modes.get(CUTOVER_POLICY_ASSET) != 0o644:
        fail("target release cutover-policy mode differs")
    for asset, expected in (
        (BOUNDARY_ASSET, require_hash(boundary_sha, "maintenance boundary sha256")),
        (
            CHECKPOINT_DESCRIPTOR_ASSET,
            require_hash(checkpoint_descriptor_sha, "recovery checkpoint descriptor sha256"),
        ),
    ):
        if files.get(asset) != expected:
            fail(f"{asset} is not the exact selected target-release asset")
        if modes is not None and modes.get(asset) != 0o644:
            fail(f"target release mode differs for {asset}")
    manifest_sha = require_hash(manifest_sha, "target release manifest sha256")
    result = {
        "binding_schema": schema,
        "repository": REPOSITORY,
        "tag": tag,
        "commit": commit,
        "manifest_sha256": manifest_sha,
        "inspector_asset": inspector_asset,
        "inspector_sha256": inspector_sha,
        "files": dict(files),
    }
    if signature_sha is not None:
        result["manifest_signature_sha256"] = signature_sha
    return result


def validate_boundary(value: Mapping[str, Any]) -> dict[str, Any]:
    if value.get("schema") != BOUNDARY_SCHEMA:
        fail("legacy maintenance boundary schema differs")
    source_commit = value.get("source_main_commit")
    if not isinstance(source_commit, str) or re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", source_commit) is None:
        fail("legacy maintenance boundary source commit is malformed")
    cutoff = require_uint(value.get("observed_cutoff_height"), "legacy observed cutoff height")
    margin = require_uint(value.get("continuity_safety_margin"), "legacy continuity margin", positive=True)
    public_maximum = require_uint(value.get("legacy_public_max_height"), "legacy public maximum height")
    if public_maximum != cutoff + margin:
        fail("legacy public maximum height is not cutoff plus the continuity margin")
    freeze_plan_sha256 = require_hash(
        value.get("freeze_plan_sha256"), "legacy maintenance freeze-plan sha256"
    )
    capture_id = require_hash(value.get("capture_id"), "legacy maintenance capture id")
    first_quarantine_started_at = require_utc(
        value.get("first_quarantine_started_at"), "legacy first-quarantine timestamp"
    )
    all_controlled_stopped_at = require_utc(
        value.get("all_controlled_stopped_at"), "legacy all-controlled-stopped timestamp"
    )
    if all_controlled_stopped_at < first_quarantine_started_at:
        fail("legacy all-controlled-stopped time predates first quarantine")
    if value.get("global_absence_claimed") is not False:
        fail("legacy maintenance boundary must honestly disclaim global absence")
    origin_scope = value.get("official_origin_scope")
    if not isinstance(origin_scope, dict) or origin_scope.get("global_absence_claimed") is not False:
        fail("legacy maintenance boundary official-origin scope is dishonest")
    threat_model = value.get("threat_model")
    if not isinstance(threat_model, dict) or threat_model.get("hostile_root_containment_claimed") is not False:
        fail("legacy maintenance boundary threat model is unsupported")
    return {
        "source_main_commit": source_commit,
        "observed_cutoff_height": cutoff,
        "continuity_safety_margin": margin,
        "legacy_public_max_height": public_maximum,
        "freeze_plan_sha256": freeze_plan_sha256,
        "capture_id": capture_id,
        "first_quarantine_started_at": first_quarantine_started_at,
        "all_controlled_stopped_at": all_controlled_stopped_at,
        "global_absence_claimed": False,
    }


def validate_checkpoint_identity(value: Mapping[str, Any], boundary: Mapping[str, Any]) -> dict[str, Any]:
    if set(value) != {
        "format_version",
        "chain_id",
        "manifest_hash",
        "payload_hash",
        "network_genesis_hash",
        "full_state_root",
        "source_height",
        "source_consensus_round",
        "created_at_unix_ms",
        "source_block_hash",
        "source_state_root",
        "transition_height",
        "transition_block_hash",
        "recovery_domain",
        "recovery_epoch",
        "validator_set_id",
        "protocol_version",
        "validator_count",
        "community_rewards_v1_activation_height",
    }:
        fail("checkpoint descriptor canonical inspection fields differ")
    required_hashes = (
        "manifest_hash", "payload_hash", "full_state_root", "network_genesis_hash", "source_block_hash",
        "source_state_root", "transition_block_hash", "recovery_domain",
    )
    for field in required_hashes:
        raw = value.get(field)
        require_hash(raw, f"checkpoint inspection {field}")
    source_height = require_uint(value.get("source_height"), "checkpoint source height")
    transition_height = require_uint(value.get("transition_height"), "checkpoint transition height", positive=True)
    if transition_height != source_height + 1:
        fail("checkpoint transition height is not exactly source height plus one")
    if (
        source_height != boundary["legacy_public_max_height"]
        or source_height != CANONICAL_BOUNDARY_HEIGHT
        or transition_height != REQUIRED_POST_CUTOVER_MIN_HEIGHT
    ):
        fail("checkpoint does not bind the canonical H=137145 to H+1=137146 cutover")
    if require_uint(value.get("format_version"), "checkpoint format version", positive=True) != 1:
        fail("checkpoint descriptor format version is not ARCCHKPT v1")
    if value.get("chain_id") != RECOVERY_CHAIN_ID:
        fail("checkpoint descriptor chain ID differs from ARC production")
    if value.get("protocol_version") != "3.0.0":
        fail("checkpoint descriptor is not exact recovery protocol 3.0.0")
    if value.get("validator_count") != 6:
        fail("checkpoint descriptor does not bind exactly six validators")
    if value.get("community_rewards_v1_activation_height") != REQUIRED_POST_CUTOVER_MIN_HEIGHT:
        fail("checkpoint descriptor community rewards activation differs from H+1")
    return {
        "format_version": 1,
        "chain_id": RECOVERY_CHAIN_ID,
        "manifest_hash": value["manifest_hash"],
        "payload_hash": value["payload_hash"],
        "source_height": source_height,
        "source_block_hash": value["source_block_hash"],
        "source_state_root": value["source_state_root"],
        "transition_height": transition_height,
        "transition_block_hash": value["transition_block_hash"],
        "full_state_root": value["full_state_root"],
        "network_genesis_hash": value["network_genesis_hash"],
        "recovery_domain": value["recovery_domain"],
        "recovery_epoch": require_uint(value.get("recovery_epoch"), "checkpoint recovery epoch", positive=True),
        "validator_set_id": require_uint(value.get("validator_set_id"), "checkpoint validator set id", positive=True),
        "source_consensus_round": require_uint(
            value.get("source_consensus_round"), "checkpoint source consensus round"
        ),
        "created_at_unix_ms": require_uint(
            value.get("created_at_unix_ms"), "checkpoint creation timestamp", positive=True
        ),
        "protocol_version": value["protocol_version"],
        "validator_count": 6,
        "community_rewards_v1_activation_height": REQUIRED_POST_CUTOVER_MIN_HEIGHT,
    }


def validate_checkpoint_descriptor(
    value: Mapping[str, Any],
    *,
    release_binding: Mapping[str, Any],
    boundary: Mapping[str, Any],
) -> dict[str, Any]:
    if set(value) != {
        "schema_version", "repository", "release_tag", "release_commit",
        "recovery_manifest_sha256", "freeze_plan_sha256", "capture_id",
        "inspector_binary_sha256", "checkpoint_file", "canonical_inspection",
        "checkpoint_certificate", "approved_validators", "verified_quorum",
    } or value.get("schema_version") != CHECKPOINT_DESCRIPTOR_SCHEMA:
        fail("checkpoint descriptor has missing, unknown, or unsupported fields")
    if (
        value.get("repository") != REPOSITORY
        or value.get("release_tag") != release_binding["tag"]
        or value.get("release_commit") != release_binding["commit"]
    ):
        fail("checkpoint descriptor release identity differs")
    require_hash(value.get("recovery_manifest_sha256"), "descriptor recovery manifest sha256")
    if value.get("freeze_plan_sha256") != boundary.get("freeze_plan_sha256") or value.get(
        "capture_id"
    ) != boundary.get("capture_id"):
        fail("checkpoint descriptor freeze/capture identity differs from the boundary")
    inspector_sha = require_hash(
        value.get("inspector_binary_sha256"), "descriptor inspector binary sha256"
    )
    release_files = release_binding.get("files")
    if not isinstance(release_files, dict) or release_files.get("arc-node-linux-x86_64") != inspector_sha:
        fail("checkpoint descriptor inspector is not the exact release Linux verifier")
    checkpoint_file = value.get("checkpoint_file")
    if not isinstance(checkpoint_file, dict) or set(checkpoint_file) != {"filename", "size_bytes", "sha256"}:
        fail("checkpoint descriptor full-file record is malformed")
    if checkpoint_file.get("filename") != "recovery.arcchkpt":
        fail("checkpoint descriptor full-file name differs")
    require_uint(checkpoint_file.get("size_bytes"), "full checkpoint size", positive=True)
    require_hash(checkpoint_file.get("sha256"), "full checkpoint sha256")
    identity_raw = value.get("canonical_inspection")
    if not isinstance(identity_raw, dict):
        fail("checkpoint descriptor canonical inspection is missing")
    identity = validate_checkpoint_identity(identity_raw, boundary)
    validators = value.get("approved_validators")
    if not isinstance(validators, list) or len(validators) != 6:
        fail("checkpoint descriptor must bind six approved validators")
    approved: list[dict[str, Any]] = []
    seen: set[str] = set()
    for (name, host), row in zip(PRODUCTION_FLEET, validators):
        if not isinstance(row, dict) or set(row) != {
            "name", "host", "origin", "address", "stake"
        }:
            fail("checkpoint descriptor approved-validator row is malformed")
        address = require_hash(row.get("address"), f"descriptor validator {name} address")
        stake = require_uint(row.get("stake"), f"descriptor validator {name} stake", positive=True)
        if stake > 2**64 - 1:
            fail(f"descriptor validator {name} stake exceeds u64")
        expected = {
            "name": name,
            "host": host,
            "origin": f"http://{host}:9090",
            "address": address,
            "stake": stake,
        }
        if row != expected or address in seen:
            fail(f"checkpoint descriptor validator {name} identity differs")
        seen.add(address)
        approved.append(expected)

    certificate = value.get("checkpoint_certificate")
    if not isinstance(certificate, dict) or set(certificate) != {
        "signing_hash", "validators", "signatures"
    }:
        fail("checkpoint descriptor certificate is malformed")
    signing_hash = require_hash(
        certificate.get("signing_hash"), "checkpoint certificate signing hash"
    )
    manifest_hash = _bare_hash(identity["manifest_hash"], "checkpoint manifest hash")
    derived_signing_hash = _blake3_derive_key(
        _RECOVERY_APPROVAL_CONTEXT, bytes.fromhex(manifest_hash)
    ).hex()
    if signing_hash != derived_signing_hash:
        fail("checkpoint certificate signing hash is not derived from the manifest hash")
    certificate_validators_raw = certificate.get("validators")
    if not isinstance(certificate_validators_raw, list) or len(certificate_validators_raw) != 6:
        fail("checkpoint certificate must contain exactly six validators")
    certificate_validators: list[dict[str, Any]] = []
    authority_by_address: dict[str, dict[str, Any]] = {}
    approved_stake_by_address = {row["address"]: row["stake"] for row in approved}
    previous_address = ""
    for index, row in enumerate(certificate_validators_raw):
        if not isinstance(row, dict) or set(row) != {"address", "public_key", "stake"}:
            fail(f"checkpoint certificate validator {index} is malformed")
        address = require_hash(row.get("address"), f"certificate validator {index} address")
        public_key = require_hash(
            row.get("public_key"), f"certificate validator {index} public key"
        )
        stake = require_uint(row.get("stake"), f"certificate validator {index} stake", positive=True)
        if stake > 2**64 - 1:
            fail(f"checkpoint certificate validator {index} stake exceeds u64")
        normalized = {"address": address, "public_key": public_key, "stake": stake}
        if (
            approved_stake_by_address.get(address) != stake
            or _blake3_short(bytes.fromhex(public_key)).hex() != address
            or address in authority_by_address
            or address <= previous_address
        ):
            fail(f"checkpoint certificate validator {index} differs from the sealed authority set")
        certificate_validators.append(normalized)
        authority_by_address[address] = normalized
        previous_address = address
    if set(authority_by_address) != set(approved_stake_by_address):
        fail("checkpoint certificate authority membership differs from approved validators")

    signatures_raw = certificate.get("signatures")
    if not isinstance(signatures_raw, list) or not 5 <= len(signatures_raw) <= 6:
        fail("checkpoint certificate does not contain an exact 5-of-6 identity quorum")
    signatures: list[dict[str, str]] = []
    signed_addresses: list[str] = []
    signed_stake = 0
    previous_signer = ""
    for index, row in enumerate(signatures_raw):
        if not isinstance(row, dict) or set(row) != {"validator", "public_key", "signature"}:
            fail(f"checkpoint certificate signature {index} is malformed")
        validator = require_hash(
            row.get("validator"), f"checkpoint certificate signer {index}"
        )
        public_key = require_hash(
            row.get("public_key"), f"checkpoint certificate signer {index} public key"
        )
        signature = row.get("signature")
        authority = authority_by_address.get(validator)
        if (
            not isinstance(signature, str)
            or SIGNATURE_RE.fullmatch(signature) is None
            or authority is None
            or public_key != authority["public_key"]
            or validator in signed_addresses
            or validator <= previous_signer
            or not _ed25519_verify(
                bytes.fromhex(public_key),
                bytes.fromhex(signing_hash),
                bytes.fromhex(signature),
            )
        ):
            fail(f"checkpoint certificate signature {index} is invalid")
        normalized_signature = {
            "validator": validator,
            "public_key": public_key,
            "signature": signature,
        }
        signatures.append(normalized_signature)
        signed_addresses.append(validator)
        signed_stake += authority["stake"]
        previous_signer = validator
    total_stake = sum(row["stake"] for row in certificate_validators)
    if total_stake > 2**64 - 1:
        fail("checkpoint certificate total stake exceeds u64")
    if signed_stake * 3 <= total_stake * 2:
        fail("checkpoint certificate lacks a strict two-thirds stake quorum")

    quorum = value.get("verified_quorum")
    if not isinstance(quorum, dict) or set(quorum) != {
        "status", "required_signatures", "verified_signature_count",
        "validator_count", "signed_validator_addresses", "signed_stake", "total_stake",
    }:
        fail("checkpoint descriptor quorum is malformed")
    verified = quorum.get("verified_signature_count")
    if (
        quorum.get("status") != "VERIFIED_QUORUM"
        or quorum.get("required_signatures") != 5
        or isinstance(verified, bool)
        or not isinstance(verified, int)
        or not 5 <= verified <= 6
        or verified != len(signatures)
        or quorum.get("validator_count") != 6
        or quorum.get("signed_validator_addresses") != signed_addresses
        or quorum.get("signed_stake") != signed_stake
        or quorum.get("total_stake") != total_stake
    ):
        fail("checkpoint descriptor quorum/validator identities differ")
    return {
        **identity,
        "descriptor_schema": CHECKPOINT_DESCRIPTOR_SCHEMA,
        "recovery_manifest_sha256": value["recovery_manifest_sha256"],
        "inspector_binary_sha256": inspector_sha,
        "checkpoint_file": dict(checkpoint_file),
        "approved_validators": approved,
        "checkpoint_certificate": {
            "signing_hash": signing_hash,
            "validators": certificate_validators,
            "signatures": signatures,
        },
        "certificate_cryptographically_verified": True,
        "verified_quorum": dict(quorum),
    }


def _bare_hash(value: Any, label: str) -> str:
    if not isinstance(value, str):
        fail(f"{label} must be a hash")
    return require_hash(value.removeprefix("0x"), label)


def validate_cutover_policy(
    value: Mapping[str, Any],
    *,
    release: Mapping[str, Any],
    release_binding: Mapping[str, Any],
    boundary: Mapping[str, Any],
    boundary_sha256: str,
    checkpoint: Mapping[str, Any],
    checkpoint_descriptor_sha256: str,
) -> dict[str, Any]:
    required = {
        "schema_version",
        "repository",
        "release_tag",
        "release_commit",
        "recovery_manifest_sha256",
        "legacy_maintenance_boundary_sha256",
        "recovery_checkpoint_descriptor_sha256",
        "recovery_checkpoint_file_sha256",
        "freeze_plan_sha256",
        "capture_id",
        "first_quarantine_started_at",
        "all_controlled_stopped_at",
        "legacy_admission_cutoff_utc",
        "canonical_boundary_height",
        "required_post_cutover_min_height",
        "required_recovery_epoch",
        "required_validator_set_id",
        "required_validator_count",
        "checkpoint_format_version",
        "chain_id",
        "protocol_version",
        "payload_hash",
        "community_rewards_v1_activation_height",
        "network_genesis_hash",
        "source_block_hash",
        "source_state_root",
        "transition_block_hash",
        "full_state_root",
        "recovery_domain",
        "checkpoint_manifest_hash",
        "checkpoint_source_consensus_round",
        "checkpoint_created_at_unix_ms",
        "checkpoint_quorum",
        "legacy_validators",
        "legacy_worker_rpc",
        "uncompleted_job_disposition",
        "legacy_exit_clean_claimed",
        "legacy_restart_allowed",
        "global_legacy_absence_claimed",
        "offline_retirement_receipt_required",
        "v08_start_requires_offline_receipt",
    }
    if set(value) != required or value.get("schema_version") != CUTOVER_POLICY_SCHEMA:
        fail("cutover policy has missing, unknown, or unsupported fields")
    if (
        value.get("repository") != REPOSITORY
        or value.get("release_tag") != release_binding["tag"]
        or value.get("release_commit") != release_binding["commit"]
        or release.get("tag") != release_binding["tag"]
        or release.get("commit") != release_binding["commit"]
    ):
        fail("cutover policy release identity differs from the sealed release")
    recovery_manifest_sha = require_hash(
        value.get("recovery_manifest_sha256"), "cutover recovery manifest sha256"
    )
    if recovery_manifest_sha != checkpoint["recovery_manifest_sha256"]:
        fail("cutover policy recovery-manifest hash differs from checkpoint descriptor")
    if value.get("legacy_maintenance_boundary_sha256") != boundary_sha256:
        fail("cutover policy maintenance-boundary hash differs")
    if value.get("recovery_checkpoint_descriptor_sha256") != checkpoint_descriptor_sha256:
        fail("cutover policy checkpoint-descriptor hash differs")
    if value.get("recovery_checkpoint_file_sha256") != checkpoint["checkpoint_file"]["sha256"]:
        fail("cutover policy full checkpoint-file hash differs")
    for field in (
        "freeze_plan_sha256",
        "capture_id",
        "first_quarantine_started_at",
        "all_controlled_stopped_at",
    ):
        if value.get(field) != boundary.get(field):
            fail(f"cutover policy {field} differs from the maintenance boundary")
    require_hash(value.get("freeze_plan_sha256"), "cutover freeze-plan sha256")
    require_hash(value.get("capture_id"), "cutover capture id")
    require_utc(value.get("first_quarantine_started_at"), "cutover first quarantine timestamp")
    stopped = require_utc(value.get("all_controlled_stopped_at"), "cutover all-stopped timestamp")
    if value.get("legacy_admission_cutoff_utc") != stopped:
        fail("cutover admission cutoff differs from the controlled-stop boundary")
    for field in (
        "canonical_boundary_height",
        "required_post_cutover_min_height",
        "required_recovery_epoch",
        "required_validator_set_id",
        "required_validator_count",
        "checkpoint_format_version",
        "community_rewards_v1_activation_height",
    ):
        require_uint(value.get(field), f"cutover policy {field}", positive=True)
    if (
        value.get("canonical_boundary_height") != CANONICAL_BOUNDARY_HEIGHT
        or value.get("required_post_cutover_min_height") != REQUIRED_POST_CUTOVER_MIN_HEIGHT
        or value.get("required_recovery_epoch") != 1
        or value.get("required_validator_set_id") != 1
        or value.get("required_validator_count") != 6
    ):
        fail("cutover policy height/epoch/validator constants differ")
    if checkpoint["recovery_epoch"] != 1 or checkpoint["validator_set_id"] != 1:
        fail("inspected checkpoint recovery epoch/validator-set id differs from cutover policy")
    comparisons = {
        "checkpoint_format_version": checkpoint["format_version"],
        "chain_id": checkpoint["chain_id"],
        "protocol_version": checkpoint["protocol_version"],
        "payload_hash": _bare_hash(checkpoint["payload_hash"], "checkpoint payload hash"),
        "community_rewards_v1_activation_height": checkpoint[
            "community_rewards_v1_activation_height"
        ],
        "network_genesis_hash": _bare_hash(
            checkpoint["network_genesis_hash"], "checkpoint network genesis hash"
        ),
        "source_block_hash": _bare_hash(checkpoint["source_block_hash"], "checkpoint source block hash"),
        "source_state_root": _bare_hash(checkpoint["source_state_root"], "checkpoint source state root"),
        "transition_block_hash": _bare_hash(
            checkpoint["transition_block_hash"], "checkpoint transition block hash"
        ),
        "full_state_root": _bare_hash(checkpoint["full_state_root"], "checkpoint full state root"),
        "recovery_domain": _bare_hash(checkpoint["recovery_domain"], "checkpoint recovery domain"),
        "checkpoint_manifest_hash": _bare_hash(
            checkpoint["manifest_hash"], "checkpoint manifest hash"
        ),
        "checkpoint_source_consensus_round": checkpoint["source_consensus_round"],
        "checkpoint_created_at_unix_ms": checkpoint["created_at_unix_ms"],
    }
    for field, expected in comparisons.items():
        if value.get(field) != expected:
            fail(f"cutover policy {field} differs from the inspected checkpoint")
    quorum = value.get("checkpoint_quorum")
    descriptor_quorum = checkpoint["verified_quorum"]
    if (
        not isinstance(quorum, dict)
        or set(quorum) != {
            "status", "required_signatures", "verified_signature_count",
            "validator_count", "signed_validator_addresses", "signed_stake", "total_stake",
        }
        or quorum != descriptor_quorum
    ):
        fail("cutover policy quorum differs from checkpoint descriptor")
    validators = value.get("legacy_validators")
    if not isinstance(validators, list) or len(validators) != len(PRODUCTION_FLEET):
        fail("cutover policy must bind exactly six legacy validator origins")
    addresses: set[str] = set()
    expected_validators: list[dict[str, Any]] = []
    for index, ((name, host), row) in enumerate(zip(PRODUCTION_FLEET, validators)):
        if not isinstance(row, dict) or set(row) != {
            "name", "host", "origin", "address", "stake"
        }:
            fail(f"cutover policy validator {index} is malformed")
        address = require_hash(row.get("address"), f"cutover validator {name} address")
        stake = require_uint(row.get("stake"), f"cutover validator {name} stake", positive=True)
        if stake > 2**64 - 1:
            fail(f"cutover validator {name} stake exceeds u64")
        expected = {
            "name": name,
            "host": host,
            "origin": f"http://{host}:9090",
            "address": address,
            "stake": stake,
        }
        if row != expected or address in addresses:
            fail(f"cutover policy validator {name} identity/origin/address differs")
        addresses.add(address)
        expected_validators.append(expected)
    if expected_validators != checkpoint["approved_validators"]:
        fail("cutover policy validator identities differ from checkpoint descriptor")
    worker_rpc = value.get("legacy_worker_rpc")
    if worker_rpc != {
        "claim_path": "/community/claim_work",
        "submit_path": "/community/submit_work",
        "listener_ports": [9090, 3001],
    }:
        fail("cutover policy legacy claim/submit paths or listener ports differ")
    if (
        value.get("uncompleted_job_disposition") != JOBS_DISPOSITION
        or value.get("legacy_exit_clean_claimed") is not False
        or value.get("legacy_restart_allowed") is not False
        or value.get("global_legacy_absence_claimed") is not False
        or value.get("offline_retirement_receipt_required") is not True
        or value.get("v08_start_requires_offline_receipt") is not True
    ):
        fail("cutover policy retirement/start requirements are dishonest")
    return {
        "schema_version": CUTOVER_POLICY_SCHEMA,
        "repository": REPOSITORY,
        "release_tag": release_binding["tag"],
        "release_commit": release_binding["commit"],
        "recovery_manifest_sha256": value["recovery_manifest_sha256"],
        "legacy_maintenance_boundary_sha256": boundary_sha256,
        "recovery_checkpoint_descriptor_sha256": checkpoint_descriptor_sha256,
        "recovery_checkpoint_file_sha256": checkpoint["checkpoint_file"]["sha256"],
        "canonical_boundary_height": CANONICAL_BOUNDARY_HEIGHT,
        "required_post_cutover_min_height": REQUIRED_POST_CUTOVER_MIN_HEIGHT,
        "required_recovery_epoch": 1,
        "required_validator_set_id": 1,
        "required_validator_count": 6,
        "checkpoint_format_version": 1,
        "chain_id": RECOVERY_CHAIN_ID,
        "payload_hash": checkpoint["payload_hash"],
        "community_rewards_v1_activation_height": REQUIRED_POST_CUTOVER_MIN_HEIGHT,
        "legacy_validators": expected_validators,
        "legacy_worker_rpc": worker_rpc,
        "uncompleted_job_disposition": JOBS_DISPOSITION,
        "legacy_exit_clean_claimed": False,
        "legacy_restart_allowed": False,
        "global_legacy_absence_claimed": False,
        "offline_retirement_receipt_required": True,
        "v08_start_requires_offline_receipt": True,
    }


class InspectorRunner(Protocol):
    def __call__(
        self,
        binary: Path,
        expected_sha256: str,
        argv: Sequence[str],
        work_parent: Path,
    ) -> tuple[dict[str, Any], str]: ...


def run_exact_inspector(
    binary: Path,
    expected_sha256: str,
    argv: Sequence[str],
    work_parent: Path,
) -> tuple[dict[str, Any], str]:
    """Execute an immutable private copy of exact-hash-checked inspector bytes."""

    source, _record = stable_file(
        binary,
        "target release inspector binary",
        expected_sha256=expected_sha256,
        maximum=1024 * 1024 * 1024,
    )
    with tempfile.TemporaryDirectory(prefix=".arc-v07-retirement-inspector-", dir=work_parent) as raw_dir:
        temporary = Path(raw_dir)
        os.chmod(temporary, 0o700)
        staged = temporary / "arc-node"
        descriptor = os.open(
            staged,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
            0o500,
        )
        try:
            offset = 0
            while offset < len(source):
                written = os.write(descriptor, source[offset:])
                if written <= 0:
                    fail("inspector staging write made no progress")
                offset += written
            os.fsync(descriptor)
            os.fchmod(descriptor, 0o500)
        finally:
            os.close(descriptor)
        staged_raw, _staged_record = stable_file(
            staged,
            "staged inspector binary",
            expected_sha256=expected_sha256,
            maximum=1024 * 1024 * 1024,
        )
        del staged_raw
        environment = {
            "PATH": "/usr/local/bin:/usr/bin:/bin",
            "LANG": "C",
            "LC_ALL": "C",
            "TZ": "UTC",
        }
        try:
            result = subprocess.run(
                [os.fspath(staged), *argv],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=False,
                cwd="/",
                env=environment,
                timeout=3600,
                check=False,
                start_new_session=True,
                close_fds=True,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            fail(f"exact ARC recovery inspector could not execute safely: {error}")
        if result.returncode != 0:
            stderr = result.stderr[:4096].decode("utf-8", "replace")
            fail(f"exact ARC recovery inspector failed ({result.returncode}): {stderr}")
        if len(result.stdout) > MAX_JSON_BYTES:
            fail("exact ARC recovery inspector output exceeds its bound")
        try:
            value = json.loads(result.stdout)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            fail(f"exact ARC recovery inspector returned invalid JSON: {error}")
        if not isinstance(value, dict) or canonical_bytes(value) != result.stdout:
            fail("exact ARC recovery inspector output is not one canonical JSON object")
        source_after, _after_record = stable_file(
            binary,
            "target release inspector binary after execution",
            expected_sha256=expected_sha256,
            maximum=1024 * 1024 * 1024,
        )
        del source_after
        return value, sha256_bytes(result.stdout)


@dataclass(frozen=True)
class PrepareRequest:
    intent_output: Path
    target_release: Path
    target_release_sha256: str
    maintenance_boundary: Path
    maintenance_boundary_sha256: str
    cutover_policy: Path
    cutover_policy_sha256: str
    checkpoint: Path
    checkpoint_sha256: str
    inspector_binary: Path | None
    inspector_asset: str | None
    inspector_sha256: str | None
    retirement_mode: str
    legacy_pid: int | None
    legacy_version: str
    legacy_executable: Path
    legacy_executable_sha256: str
    supervisor_definition: Path
    supervisor_definition_sha256: str
    data_dir: Path
    v08_data_dir: Path
    replay_mode: str
    snapshot: Path | None = None
    snapshot_sha256: str | None = None
    genesis: Path | None = None
    genesis_sha256: str | None = None
    legacy_validator_set: Path | None = None
    legacy_validator_set_sha256: str | None = None
    allow_unbound_legacy_wal: bool | None = None


def process_record(observed: ProcessObservation, semantic: Mapping[str, Any]) -> dict[str, Any]:
    return {
        "retirement_mode": "term_only",
        "pid": observed.pid,
        "boot_id": observed.boot_id,
        "start_ticks": observed.start_ticks,
        "uid": observed.uid,
        "gid": observed.gid,
        "executable": dict(observed.executable),
        "argv_sha256": sha256_bytes(b"\0".join(part.encode("utf-8") for part in observed.argv) + b"\0"),
        "cwd": observed.cwd,
        "stake_zero_semantics": dict(semantic),
        "listeners": [dict(row) for row in observed.listeners],
        "required_absent_listener_ports": [9090, 3001],
    }


def validate_intent(value: Mapping[str, Any]) -> None:
    required = {
        "schema",
        "protocol_id",
        "created_at",
        "scope",
        "target_release",
        "maintenance_boundary",
        "cutover_policy",
        "checkpoint",
        "inspector",
        "legacy_release",
        "old_process",
        "old_data",
        "v08_start",
        "replay_inputs",
        "retirement_policy",
    }
    if set(value) != required or value.get("schema") != INTENT_SCHEMA:
        fail("retirement intent has missing, unknown, or unsupported fields")
    require_hash(value.get("protocol_id"), "retirement protocol id")
    require_utc(value.get("created_at"), "retirement intent created_at")
    if value.get("scope") != "v0.7-stake-zero-community-worker":
        fail("retirement intent scope differs")
    policy = value.get("retirement_policy")
    if policy != {
        "network_access": "forbidden",
        "legacy_data_writes": "forbidden",
        "stop_signal_policy": "external-supervisor-term-only-no-sigkill",
        "legacy_exit_clean_claimed": False,
        "legacy_jobs_disposition": JOBS_DISPOSITION,
    }:
        fail("retirement intent policy differs")
    old_process = value.get("old_process")
    if not isinstance(old_process, dict):
        fail("retirement intent old process is missing")
    if old_process.get("retirement_mode") == "term_only":
        require_uint(old_process.get("pid"), "intent PID", positive=True)
        require_uint(old_process.get("start_ticks"), "intent process start ticks", positive=True)
        if not isinstance(old_process.get("boot_id"), str) or BOOT_ID_RE.fullmatch(old_process["boot_id"]) is None:
            fail("intent process boot identity is malformed")
        if not isinstance(old_process.get("listeners"), list):
            fail("TERM-only intent listener binding is malformed")
    elif old_process.get("retirement_mode") == "preexisting_offline":
        if any(old_process.get(field) is not None for field in (
            "pid", "boot_id", "start_ticks", "uid", "gid", "argv_sha256", "cwd"
        )):
            fail("preexisting-offline intent must not invent a process identity")
        if old_process.get("listeners") != []:
            fail("preexisting-offline intent must not invent listener inodes")
    else:
        fail("intent retirement mode is unsupported")
    executable = old_process.get("executable")
    if not isinstance(executable, dict):
        fail("intent process executable record is missing")
    require_hash(executable.get("sha256"), "intent process executable sha256")
    semantics = old_process.get("stake_zero_semantics")
    if not isinstance(semantics, dict) or semantics.get("stake") != 0 or semantics.get("minimum_stake") != 0:
        fail("intent process is not explicitly stake-zero")
    listener_ports = old_process.get("required_absent_listener_ports")
    if listener_ports != [9090, 3001]:
        fail("intent required legacy listener ports differ from cutover policy")
    listeners = old_process.get("listeners")
    if not isinstance(listeners, list):
        fail("intent listener records are malformed")
    seen_endpoints: set[tuple[str, str, int]] = set()
    for row in listeners:
        if not isinstance(row, dict) or set(row) != {"family", "address_hex", "port", "inode"}:
            fail("intent listener record is malformed")
        if row["family"] not in {"tcp4", "tcp6"} or not isinstance(row["address_hex"], str):
            fail("intent listener family/address is malformed")
        port = require_uint(row["port"], "intent listener port", positive=True)
        if port > 65535:
            fail("intent listener port exceeds 65535")
        require_uint(row["inode"], "intent listener inode", positive=True)
        endpoint = (row["family"], row["address_hex"], port)
        if endpoint in seen_endpoints:
            fail("intent repeats a listener endpoint")
        seen_endpoints.add(endpoint)
    old_data = value.get("old_data")
    if not isinstance(old_data, dict) or set(old_data) != {"root_anchor", "wal_prefix"}:
        fail("intent old-data binding is malformed")
    root = old_data["root_anchor"]
    wal = old_data["wal_prefix"]
    if not isinstance(root, dict) or not isinstance(root.get("path"), str):
        fail("intent data root anchor is malformed")
    if not isinstance(wal, dict):
        fail("intent WAL prefix is malformed")
    require_hash(wal.get("observed_prefix_sha256"), "intent WAL prefix sha256")
    require_uint(wal.get("observed_prefix_bytes"), "intent WAL prefix bytes", positive=True)
    v08_start = value.get("v08_start")
    if not isinstance(v08_start, dict) or v08_start != {
        "data_dir": v08_start.get("data_dir"),
        "must_be_absent_until_receipt": True,
        "canonical_history_source": "signed_recovery_checkpoint",
        "old_wal_migration_allowed": False,
    } or not isinstance(v08_start.get("data_dir"), str):
        fail("intent v0.8 fresh-data start policy is malformed")
    replay = value.get("replay_inputs")
    if not isinstance(replay, dict) or set(replay) != {
        "mode",
        "snapshot",
        "genesis",
        "legacy_validator_set",
        "allow_unbound_legacy_wal",
    }:
        fail("intent replay-input binding is malformed")
    if replay["mode"] == "forensic-only":
        if any(replay[field] is not None for field in (
            "snapshot", "genesis", "legacy_validator_set", "allow_unbound_legacy_wal"
        )):
            fail("forensic-only intent must not imply unavailable replay inputs")
    elif replay["mode"] == "canonical-replay":
        for label in ("snapshot", "genesis", "legacy_validator_set"):
            if not isinstance(replay[label], dict):
                fail(f"intent replay input {label} is malformed")
            require_hash(replay[label].get("sha256"), f"intent replay input {label} sha256")
        if not isinstance(replay["allow_unbound_legacy_wal"], bool):
            fail("intent legacy WAL binding policy is malformed")
    else:
        fail("intent replay mode is unsupported")


def _existing_intent_matches_request(value: Mapping[str, Any], request: PrepareRequest) -> None:
    expected = {
        "target_release": ("binding_sha256", request.target_release_sha256),
        "maintenance_boundary": ("sha256", request.maintenance_boundary_sha256),
        "cutover_policy": ("sha256", request.cutover_policy_sha256),
        "checkpoint": ("descriptor_sha256", request.checkpoint_sha256),
        "legacy_release": ("executable_sha256", request.legacy_executable_sha256),
    }
    for section, (field, selected) in expected.items():
        row = value.get(section)
        if not isinstance(row, dict) or row.get(field) != require_hash(selected, f"selected {section} hash"):
            fail(f"existing retirement intent {section} differs from this request")
    expected_inspector = request.inspector_sha256
    if value.get("inspector", {}).get("sha256") != expected_inspector:
        fail("existing retirement intent local inspector differs from this request")
    if value["legacy_release"].get("version") != request.legacy_version:
        fail("existing retirement intent legacy version differs from this request")
    if (
        value["old_process"].get("pid") != request.legacy_pid
        or value["old_process"].get("retirement_mode") != request.retirement_mode
    ):
        fail("existing retirement intent PID differs from this request")
    legacy_release = value["legacy_release"]
    if (
        legacy_release.get("executable", {}).get("path") != os.fspath(request.legacy_executable)
        or legacy_release.get("supervisor_definition", {}).get("path")
        != os.fspath(request.supervisor_definition)
        or legacy_release.get("supervisor_definition", {}).get("sha256")
        != request.supervisor_definition_sha256
    ):
        fail("existing retirement intent legacy binary/supervisor binding differs")
    if value["old_data"]["root_anchor"].get("path") != os.fspath(request.data_dir):
        fail("existing retirement intent data directory differs from this request")
    if value["v08_start"].get("data_dir") != os.fspath(request.v08_data_dir):
        fail("existing retirement intent v0.8 data directory differs from this request")
    replay = value["replay_inputs"]
    if replay.get("mode") != request.replay_mode:
        fail("existing retirement intent replay mode differs from this request")
    if request.replay_mode == "canonical-replay":
        for name, path, digest in (
            ("snapshot", request.snapshot, request.snapshot_sha256),
            ("genesis", request.genesis, request.genesis_sha256),
            ("legacy_validator_set", request.legacy_validator_set, request.legacy_validator_set_sha256),
        ):
            if path is None or replay[name].get("path") != os.fspath(path) or replay[name].get("sha256") != digest:
                fail(f"existing retirement intent {name} differs from this request")


def prepare_intent(
    request: PrepareRequest,
    *,
    runtime: Runtime,
    now: Callable[[], str] = utc_now,
    offline_stability_seconds: float = 10.0,
    offline_samples: int = 3,
    sleep: Callable[[float], None] = time.sleep,
) -> tuple[dict[str, Any], str]:
    safe_output(request.intent_output, "retirement intent output", forbidden_root=request.data_dir)
    if request.intent_output.exists():
        existing, raw, _record = load_canonical_json(
            request.intent_output,
            "existing retirement intent",
        )
        validate_intent(existing)
        _existing_intent_matches_request(existing, request)
        return existing, sha256_bytes(raw)
    require_fresh_v08_path(request.v08_data_dir, request.data_dir)
    for path, label in (
        (request.target_release, "target release binding"),
        (request.maintenance_boundary, "legacy maintenance boundary"),
        (request.cutover_policy, "cutover policy"),
        (request.checkpoint, "recovery checkpoint descriptor"),
        (request.legacy_executable, "legacy v0.7 executable"),
        (request.supervisor_definition, "legacy supervisor definition"),
        (request.data_dir, "legacy data directory"),
    ):
        canonical_absolute(path, label)
    if request.replay_mode not in {"forensic-only", "canonical-replay"}:
        fail("replay mode must be forensic-only or canonical-replay")
    replay_arguments = (
        request.snapshot,
        request.snapshot_sha256,
        request.genesis,
        request.genesis_sha256,
        request.legacy_validator_set,
        request.legacy_validator_set_sha256,
        request.allow_unbound_legacy_wal,
    )
    if request.replay_mode == "forensic-only":
        if any(value is not None for value in replay_arguments):
            fail("forensic-only mode forbids implied canonical replay inputs")
    else:
        if any(value is None for value in replay_arguments):
            fail("canonical-replay mode requires every snapshot/genesis/validator/WAL-policy input")
        assert request.snapshot is not None
        assert request.genesis is not None
        assert request.legacy_validator_set is not None
        for path, label in (
            (request.snapshot, "legacy replay snapshot"),
            (request.genesis, "recovery genesis"),
            (request.legacy_validator_set, "legacy validator set"),
        ):
            canonical_absolute(path, label)
        if request.inspector_binary is None or request.inspector_asset is None or request.inspector_sha256 is None:
            fail("canonical-replay mode requires an exact local inspector binary/asset/hash")
        canonical_absolute(request.inspector_binary, "target release inspector binary")
    if request.replay_mode == "forensic-only" and any(
        value is not None
        for value in (request.inspector_binary, request.inspector_asset, request.inspector_sha256)
    ):
        fail("forensic-only mode does not execute or bind an unnecessary local inspector")
    if LEGACY_VERSION_RE.fullmatch(request.legacy_version) is None:
        fail("legacy version must be strict 0.7.PATCH")
    release, release_raw, _release_record = load_canonical_json(
        request.target_release,
        "target release binding",
        expected_sha256=request.target_release_sha256,
    )
    inspector_record: dict[str, Any] | None = None
    if request.inspector_binary is not None and request.inspector_sha256 is not None:
        _inspector_raw, inspector_record = stable_file(
            request.inspector_binary,
            "target release inspector binary",
            expected_sha256=request.inspector_sha256,
            maximum=1024 * 1024 * 1024,
        )
    release_binding = validate_release(
        release,
        request.inspector_asset,
        request.inspector_sha256,
        request.cutover_policy_sha256,
        request.maintenance_boundary_sha256,
        request.checkpoint_sha256,
    )
    boundary, boundary_raw, _boundary_record = load_canonical_json(
        request.maintenance_boundary,
        "legacy maintenance boundary",
        expected_sha256=request.maintenance_boundary_sha256,
    )
    boundary_binding = validate_boundary(boundary)
    checkpoint_descriptor, checkpoint_raw, checkpoint_record = load_canonical_json(
        request.checkpoint,
        "recovery checkpoint descriptor",
        expected_sha256=request.checkpoint_sha256,
        maximum=1024 * 1024,
    )
    checkpoint_binding = validate_checkpoint_descriptor(
        checkpoint_descriptor,
        release_binding=release_binding,
        boundary=boundary_binding,
    )
    policy, policy_raw, _policy_record = load_canonical_json(
        request.cutover_policy,
        "cutover policy",
        expected_sha256=request.cutover_policy_sha256,
    )
    policy_binding = validate_cutover_policy(
        policy,
        release=release,
        release_binding=release_binding,
        boundary=boundary_binding,
        boundary_sha256=request.maintenance_boundary_sha256,
        checkpoint=checkpoint_binding,
        checkpoint_descriptor_sha256=request.checkpoint_sha256,
    )
    _legacy_executable_raw, legacy_executable_record = stable_file(
        request.legacy_executable,
        "legacy v0.7 executable",
        expected_sha256=request.legacy_executable_sha256,
        maximum=1024 * 1024 * 1024,
    )
    supervisor, _supervisor_raw, supervisor_record = load_canonical_json(
        request.supervisor_definition,
        "legacy supervisor binding",
        expected_sha256=request.supervisor_definition_sha256,
    )
    semantics, supervisor_source_record = validate_supervisor_binding(
        supervisor,
        data_dir=request.data_dir,
        legacy_executable=request.legacy_executable,
        legacy_executable_sha256=request.legacy_executable_sha256,
    )
    if request.retirement_mode == "term_only":
        if request.legacy_pid is None:
            fail("TERM-only retirement requires an exact legacy PID")
        observed = runtime.observe_process(request.legacy_pid)
        if observed is None:
            fail("legacy process is not running; use --already-offline instead")
        if observed.executable.get("sha256") != request.legacy_executable_sha256:
            fail("running legacy process executable differs from the selected v0.7 bytes")
        if tuple(supervisor["argv"]) != observed.argv:
            fail("running legacy process argv differs from the verified supervisor binding")
        old_process = process_record(observed, semantics)
    elif request.retirement_mode == "preexisting_offline":
        if request.legacy_pid is not None:
            fail("preexisting-offline retirement must not select a PID")
        old_process = {
            "retirement_mode": "preexisting_offline",
            "pid": None,
            "boot_id": None,
            "start_ticks": None,
            "uid": None,
            "gid": None,
            "executable": legacy_executable_record,
            "argv_sha256": None,
            "cwd": None,
            "stake_zero_semantics": semantics,
            "listeners": [],
            "required_absent_listener_ports": [9090, 3001],
        }
        # The same proof is repeated during finalize.  Intent creation proves
        # a previously broken/stopped install need not be restarted merely to
        # acquire a PID identity.
        prove_stably_offline(
            {"old_process": old_process, "old_data": {"root_anchor": {"path": os.fspath(request.data_dir)}}},
            runtime,
            stability_seconds=offline_stability_seconds,
            samples=offline_samples,
            sleep=sleep,
        )
    else:
        fail("retirement mode must be term_only or preexisting_offline")
    data_metadata = os.lstat(request.data_dir)
    if not stat.S_ISDIR(data_metadata.st_mode) or stat.S_ISLNK(data_metadata.st_mode):
        fail("legacy data directory is not a non-symlink directory")
    if data_metadata.st_mode & 0o022:
        fail("legacy data directory must not be group/world writable")
    wal = wal_prefix_record(request.data_dir / "state.wal")
    snapshot_record: dict[str, Any] | None = None
    genesis_record: dict[str, Any] | None = None
    legacy_set_record: dict[str, Any] | None = None
    if request.replay_mode == "canonical-replay":
        assert request.snapshot is not None and request.snapshot_sha256 is not None
        assert request.genesis is not None and request.genesis_sha256 is not None
        assert request.legacy_validator_set is not None and request.legacy_validator_set_sha256 is not None
        _snapshot_raw, snapshot_record = stable_file(
            request.snapshot,
            "legacy replay snapshot",
            expected_sha256=request.snapshot_sha256,
            maximum=1024 * 1024 * 1024,
        )
        _genesis_raw, genesis_record = stable_file(
            request.genesis,
            "recovery genesis",
            expected_sha256=request.genesis_sha256,
        )
        _legacy_set_raw, legacy_set_record = stable_file(
            request.legacy_validator_set,
            "legacy validator set",
            expected_sha256=request.legacy_validator_set_sha256,
        )
    roots = {
        "release_binding_sha256": sha256_bytes(release_raw),
        "maintenance_boundary_sha256": sha256_bytes(boundary_raw),
        "cutover_policy_sha256": sha256_bytes(policy_raw),
        "checkpoint_descriptor_sha256": checkpoint_record["sha256"],
        "checkpoint_file_sha256": checkpoint_binding["checkpoint_file"]["sha256"],
        "checkpoint_manifest_hash": checkpoint_binding["manifest_hash"],
        "local_inspector_sha256": inspector_record["sha256"] if inspector_record else None,
        "legacy_executable_sha256": legacy_executable_record["sha256"],
        "retirement_mode": request.retirement_mode,
        "process_boot_id": old_process["boot_id"],
        "process_pid": old_process["pid"],
        "process_start_ticks": old_process["start_ticks"],
        "data_directory_device": data_metadata.st_dev,
        "data_directory_inode": data_metadata.st_ino,
        "wal_prefix_sha256": wal["observed_prefix_sha256"],
        "v08_data_dir": os.fspath(request.v08_data_dir),
        "replay_mode": request.replay_mode,
    }
    protocol_id = sha256_bytes(
        b"ARC-v0.7-stake-zero-retirement-intent-v1\0" + canonical_bytes(roots)
    )
    intent: dict[str, Any] = {
        "schema": INTENT_SCHEMA,
        "protocol_id": protocol_id,
        "created_at": now(),
        "scope": "v0.7-stake-zero-community-worker",
        "target_release": {
            **release_binding,
            "binding_path": os.fspath(request.target_release),
            "binding_sha256": request.target_release_sha256,
        },
        "maintenance_boundary": {
            **boundary_binding,
            "path": os.fspath(request.maintenance_boundary),
            "sha256": request.maintenance_boundary_sha256,
        },
        "cutover_policy": {
            **policy_binding,
            "path": os.fspath(request.cutover_policy),
            "sha256": request.cutover_policy_sha256,
        },
        "checkpoint": {
            **checkpoint_binding,
            "path": os.fspath(request.checkpoint),
            "descriptor_sha256": request.checkpoint_sha256,
        },
        "inspector": {
            "path": os.fspath(request.inspector_binary) if request.inspector_binary else None,
            "asset": request.inspector_asset,
            "sha256": request.inspector_sha256,
        },
        "legacy_release": {
            "version": request.legacy_version,
            "executable_sha256": request.legacy_executable_sha256,
            "executable": legacy_executable_record,
            "supervisor_definition": supervisor_record,
            "supervisor_source": supervisor_source_record,
        },
        "old_process": old_process,
        "old_data": {
            "root_anchor": {
                "path": os.fspath(request.data_dir),
                **projection(data_metadata),
            },
            "wal_prefix": wal,
        },
        "v08_start": {
            "data_dir": os.fspath(request.v08_data_dir),
            "must_be_absent_until_receipt": True,
            "canonical_history_source": "signed_recovery_checkpoint",
            "old_wal_migration_allowed": False,
        },
        "replay_inputs": {
            "mode": request.replay_mode,
            "snapshot": snapshot_record,
            "genesis": genesis_record,
            "legacy_validator_set": legacy_set_record,
            "allow_unbound_legacy_wal": request.allow_unbound_legacy_wal,
        },
        "retirement_policy": {
            "network_access": "forbidden",
            "legacy_data_writes": "forbidden",
            "stop_signal_policy": "external-supervisor-term-only-no-sigkill",
            "legacy_exit_clean_claimed": False,
            "legacy_jobs_disposition": JOBS_DISPOSITION,
        },
    }
    validate_intent(intent)
    digest = publish_create_only_atomic(request.intent_output, intent, "retirement intent")
    return intent, digest


def validate_stop_evidence(value: Mapping[str, Any], intent: Mapping[str, Any], intent_sha: str) -> None:
    required = {
        "schema",
        "intent_sha256",
        "process_identity",
        "supervisor",
        "observation_started_at",
        "offline_observed_at",
        "legacy_exit_clean_claimed",
    }
    mode = intent["old_process"]["retirement_mode"]
    expected_schema = (
        STOP_EVIDENCE_SCHEMA if mode == "term_only" else PREEXISTING_OFFLINE_EVIDENCE_SCHEMA
    )
    if set(value) != required or value.get("schema") != expected_schema:
        fail("offline evidence has missing, unknown, or unsupported fields")
    if value.get("intent_sha256") != require_hash(intent_sha, "retirement intent sha256"):
        fail("TERM-only stop evidence is not bound to the exact retirement intent")
    process = intent["old_process"]
    expected_process_identity = (
        {"boot_id": process["boot_id"], "pid": process["pid"], "start_ticks": process["start_ticks"]}
        if mode == "term_only"
        else None
    )
    if value.get("process_identity") != expected_process_identity:
        fail("offline evidence process identity differs from the intent")
    supervisor = value.get("supervisor")
    if not isinstance(supervisor, dict) or set(supervisor) != {
        "mechanism",
        "signals_sent",
        "send_sigkill_configured",
        "sigkill_sent",
        "escalation_used",
        "exit_status_observed",
    }:
        fail("TERM-only stop evidence supervisor record is malformed")
    allowed_mechanisms = (
        {"systemd-send-sigkill-no", "launchd-term-only", "direct-term-only"}
        if mode == "term_only"
        else {"preexisting-offline-verified-supervisor"}
    )
    if supervisor.get("mechanism") not in allowed_mechanisms:
        fail("TERM-only stop evidence supervisor mechanism is unsupported")
    expected_signals = ["SIGTERM"] if mode == "term_only" else []
    if supervisor.get("signals_sent") != expected_signals:
        fail("offline evidence signal sequence differs from the retirement mode")
    if (
        supervisor.get("send_sigkill_configured") is not False
        or supervisor.get("sigkill_sent") is not False
        or supervisor.get("escalation_used") is not False
    ):
        fail("retirement refuses any configured, sent, or escalated SIGKILL path")
    if not isinstance(supervisor.get("exit_status_observed"), bool):
        fail("TERM-only stop evidence exit-status observation must be boolean")
    if value.get("legacy_exit_clean_claimed") is not False:
        fail("v0.7 retirement must not claim a clean legacy exit")
    requested = require_utc(value.get("observation_started_at"), "offline evidence observation_started_at")
    offline = require_utc(value.get("offline_observed_at"), "stop evidence offline_observed_at")
    requested_time = dt.datetime.strptime(requested, "%Y-%m-%dT%H:%M:%SZ")
    offline_time = dt.datetime.strptime(offline, "%Y-%m-%dT%H:%M:%SZ")
    if offline_time < requested_time:
        fail("stop evidence observed offline state before requesting TERM")


def _same_process(observed: ProcessObservation, expected: Mapping[str, Any]) -> bool:
    return (
        observed.pid == expected["pid"]
        and observed.boot_id == expected["boot_id"]
        and observed.start_ticks == expected["start_ticks"]
    )


def prove_stably_offline(
    intent: Mapping[str, Any],
    runtime: Runtime,
    *,
    stability_seconds: float,
    samples: int,
    sleep: Callable[[float], None],
) -> dict[str, Any]:
    if isinstance(samples, bool) or samples < 3:
        fail("offline proof requires at least three samples")
    if stability_seconds < 0:
        fail("offline stability duration cannot be negative")
    process = intent["old_process"]
    data_dir = intent["old_data"]["root_anchor"]["path"]
    executable_sha = process["executable"]["sha256"]
    expected_endpoints = {
        (row["family"], row["address_hex"], row["port"]) for row in process["listeners"]
    }
    required_ports = set(process["required_absent_listener_ports"])
    interval = stability_seconds / (samples - 1)
    for index in range(samples):
        if process["pid"] is not None:
            observed = runtime.observe_process(process["pid"])
            if observed is not None and _same_process(observed, process):
                fail("the exact legacy process identity is still running")
        replacements = runtime.matching_processes(data_dir, executable_sha)
        if replacements:
            identities = ", ".join(
                f"{row.pid}/{row.start_ticks}" for row in sorted(replacements, key=lambda row: row.pid)
            )
            fail(f"a legacy executable/data-tree writer is still running: {identities}")
        active = runtime.active_listener_endpoints()
        occupied = sorted(expected_endpoints & active)
        if occupied:
            fail(f"an old legacy listener endpoint is still active: {occupied}")
        occupied_ports = sorted({port for _family, _address, port in active} & required_ports)
        if occupied_ports:
            fail(f"a required-absent legacy listener port is still active: {occupied_ports}")
        if index + 1 < samples and interval:
            sleep(interval)
    return {
        "sample_count": samples,
        "stability_seconds": stability_seconds,
        "exact_process_identity_absent": True,
        "replacement_legacy_writer_absent": True,
        "recorded_listener_endpoints_absent": True,
        "listener_endpoints": [
            {"family": family, "address_hex": address, "port": port}
            for family, address, port in sorted(expected_endpoints)
        ],
        "required_absent_listener_ports": sorted(required_ports),
    }


def _same_file_binding(actual: Mapping[str, Any], expected: Mapping[str, Any], label: str) -> None:
    for field in ("path", "device", "inode", "mode", "uid", "gid", "nlink", "size", "sha256"):
        if actual.get(field) != expected.get(field):
            fail(f"{label} {field} differs from the retirement intent")


def validate_legacy_block_inspection(
    value: Mapping[str, Any],
    intent: Mapping[str, Any],
    tree: Mapping[str, Any],
) -> dict[str, Any]:
    checkpoint = intent["checkpoint"]
    if value.get("schema") != LEGACY_BLOCK_INSPECTION_SCHEMA:
        fail("legacy block inspector returned an unsupported schema")
    if value.get("height") != checkpoint["source_height"]:
        fail("legacy block inspector returned the wrong source height")
    if value.get("block_hash") != checkpoint["source_block_hash"]:
        fail("legacy data tree does not contain the checkpoint source block hash")
    if value.get("state_root") != checkpoint["source_state_root"]:
        fail("legacy data tree does not contain the checkpoint source state root")
    roots = value.get("input_roots")
    if not isinstance(roots, dict) or set(roots) != {
        "data_dir",
        "state_wal",
        "snapshot",
        "genesis",
        "legacy_validator_set",
    }:
        fail("legacy block inspection input-root set differs")
    entries = tree["entries"]
    wal_entries = [row for row in entries if row.get("path") == "state.wal" and row.get("kind") == "file"]
    if len(wal_entries) != 1 or roots["state_wal"].get("sha256") != wal_entries[0].get("sha256"):
        fail("legacy block inspection state-WAL root differs from the stable data tree")
    replay = intent["replay_inputs"]
    for name in ("snapshot", "genesis", "legacy_validator_set"):
        if not isinstance(roots[name], dict) or roots[name].get("sha256") != replay[name]["sha256"]:
            fail(f"legacy block inspection {name} root differs")
    data_root = roots["data_dir"]
    selected_root = intent["old_data"]["root_anchor"]
    for field in ("device", "inode", "mode", "uid", "gid", "nlink", "mtime_ns", "ctime_ns"):
        if data_root.get(field) != selected_root.get(field):
            fail(f"legacy block inspection data-directory {field} differs")
    return {
        "schema": LEGACY_BLOCK_INSPECTION_SCHEMA,
        "height": value["height"],
        "block_hash": value["block_hash"],
        "state_root": value["state_root"],
        "state_wal_sha256": roots["state_wal"]["sha256"],
        "snapshot_sha256": roots["snapshot"]["sha256"],
        "genesis_sha256": roots["genesis"]["sha256"],
        "legacy_validator_set_sha256": roots["legacy_validator_set"]["sha256"],
    }


def validate_receipt(value: Mapping[str, Any]) -> None:
    required = {
        "schema",
        "verified_at",
        "intent_sha256",
        "stop_evidence_sha256",
        "protocol_id",
        "scope",
        "target_release",
        "maintenance_boundary",
        "cutover_policy",
        "checkpoint",
        "old_process",
        "offline_stability",
        "old_data_tree",
        "v08_start",
        "local_legacy_replay",
        "retirement_result",
    }
    if set(value) != required or value.get("schema") != RECEIPT_SCHEMA:
        fail("retirement receipt has missing, unknown, or unsupported fields")
    require_utc(value.get("verified_at"), "retirement receipt verified_at")
    require_hash(value.get("intent_sha256"), "receipt intent sha256")
    require_hash(value.get("stop_evidence_sha256"), "receipt stop-evidence sha256")
    require_hash(value.get("protocol_id"), "receipt protocol id")
    if value.get("scope") != "v0.7-stake-zero-community-worker":
        fail("retirement receipt scope differs")
    old_process = value.get("old_process")
    if not isinstance(old_process, dict) or old_process.get("retirement_mode") not in {
        "term_only", "preexisting_offline"
    }:
        fail("retirement receipt process mode is malformed")
    expected_signals = ["SIGTERM"] if old_process["retirement_mode"] == "term_only" else []
    if old_process.get("signals_sent") != expected_signals:
        fail("retirement receipt signal sequence differs from its process mode")
    checkpoint = value.get("checkpoint")
    if not isinstance(checkpoint, dict) or set(checkpoint) != {
        "descriptor_sha256", "full_file_sha256", "full_file_size_bytes",
        "format_version", "chain_id", "manifest_hash", "payload_hash",
        "community_rewards_v1_activation_height", "certificate_signing_hash",
        "certificate_cryptographically_verified", "verified_signature_count",
        "signed_validator_addresses", "signed_stake", "total_stake",
        "source_height", "source_block_hash", "source_state_root",
        "transition_height", "transition_block_hash", "canonical_history_source",
    }:
        fail("retirement receipt checkpoint binding is malformed")
    for field in (
        "descriptor_sha256", "full_file_sha256", "manifest_hash", "payload_hash",
        "certificate_signing_hash", "source_block_hash", "source_state_root",
        "transition_block_hash",
    ):
        require_hash(checkpoint.get(field), f"receipt checkpoint {field}")
    signers = checkpoint.get("signed_validator_addresses")
    verified_count = checkpoint.get("verified_signature_count")
    signed_stake = require_uint(checkpoint.get("signed_stake"), "receipt signed stake", positive=True)
    total_stake = require_uint(checkpoint.get("total_stake"), "receipt total stake", positive=True)
    if (
        checkpoint.get("certificate_cryptographically_verified") is not True
        or isinstance(verified_count, bool)
        or not isinstance(verified_count, int)
        or not 5 <= verified_count <= 6
        or not isinstance(signers, list)
        or len(signers) != verified_count
        or len(set(signers)) != verified_count
        or any(not isinstance(item, str) or HASH_RE.fullmatch(item) is None for item in signers)
        or signed_stake * 3 <= total_stake * 2
        or checkpoint.get("format_version") != 1
        or checkpoint.get("chain_id") != RECOVERY_CHAIN_ID
        or checkpoint.get("community_rewards_v1_activation_height")
        != REQUIRED_POST_CUTOVER_MIN_HEIGHT
        or checkpoint.get("source_height") != CANONICAL_BOUNDARY_HEIGHT
        or checkpoint.get("transition_height") != REQUIRED_POST_CUTOVER_MIN_HEIGHT
        or checkpoint.get("canonical_history_source") != "signed_recovery_checkpoint"
    ):
        fail("retirement receipt checkpoint certificate/quorum binding differs")
    result = value.get("retirement_result")
    if result != {
        "retired": True,
        "stake": 0,
        "legacy_process_stably_absent": True,
        "legacy_listeners_stably_absent": True,
        "sigkill_sent": False,
        "legacy_exit_clean_claimed": False,
        "legacy_jobs_disposition": JOBS_DISPOSITION,
        "legacy_data_opened_writable_by_verifier": False,
        "legacy_data_changed_during_verification": False,
        "legacy_data_disposition": result.get("legacy_data_disposition"),
        "canonical_history_source": "signed_recovery_checkpoint",
        "old_wal_copied_to_v08": False,
        "v08_data_dir_fresh_at_receipt": True,
        "canonical_chain_history_rewritten": False,
    }:
        fail("retirement receipt result is dishonest or unsupported")
    if result.get("legacy_data_disposition") not in {
        "preserved_noncanonical_forensic_not_migrated",
        "preserved_local_canonical_boundary_verified_not_migrated",
    }:
        fail("retirement receipt legacy-data disposition is unsupported")
    replay = value.get("local_legacy_replay")
    if not isinstance(replay, dict) or replay.get("canonical_history_source") != "signed_recovery_checkpoint":
        fail("retirement receipt local-replay classification is malformed")
    if result["legacy_data_disposition"] == "preserved_noncanonical_forensic_not_migrated":
        if replay != {
            "performed": False,
            "classification": "preserved_noncanonical_forensic_not_migrated",
            "canonical_history_source": "signed_recovery_checkpoint",
            "inspection": None,
        }:
            fail("forensic-only retirement receipt overclaims local replay")
    elif replay.get("performed") is not True or not isinstance(replay.get("inspection"), dict):
        fail("canonical-replay retirement receipt omits its exact inspection")


def validate_existing_receipt_bindings(
    receipt: Mapping[str, Any],
    intent: Mapping[str, Any],
    stop: Mapping[str, Any],
    intent_sha256: str,
    stop_sha256: str,
) -> None:
    """Reject a pre-created receipt that merely copies the two public hashes."""

    if (
        receipt.get("intent_sha256") != intent_sha256
        or receipt.get("stop_evidence_sha256") != stop_sha256
        or receipt.get("protocol_id") != intent["protocol_id"]
        or receipt.get("scope") != intent["scope"]
    ):
        fail("existing retirement receipt belongs to another protocol execution")
    expected_target = {
        "tag": intent["target_release"]["tag"],
        "commit": intent["target_release"]["commit"],
        "binding_schema": intent["target_release"]["binding_schema"],
        "binding_sha256": intent["target_release"]["binding_sha256"],
        "manifest_sha256": intent["target_release"]["manifest_sha256"],
        "manifest_signature_sha256": intent["target_release"].get(
            "manifest_signature_sha256"
        ),
        "local_inspector_sha256": intent["inspector"]["sha256"],
    }
    if receipt.get("target_release") != expected_target:
        fail("existing retirement receipt release binding differs from its intent")
    expected_boundary = {
        "sha256": intent["maintenance_boundary"]["sha256"],
        "observed_cutoff_height": intent["maintenance_boundary"]["observed_cutoff_height"],
        "legacy_public_max_height": intent["maintenance_boundary"]["legacy_public_max_height"],
        "global_absence_claimed": False,
    }
    if receipt.get("maintenance_boundary") != expected_boundary:
        fail("existing retirement receipt maintenance boundary differs from its intent")
    expected_policy = {
        "sha256": intent["cutover_policy"]["sha256"],
        "uncompleted_job_disposition": JOBS_DISPOSITION,
        "legacy_exit_clean_claimed": False,
        "legacy_restart_allowed": False,
        "global_legacy_absence_claimed": False,
        "offline_retirement_receipt_required": True,
        "v08_start_requires_offline_receipt": True,
    }
    if receipt.get("cutover_policy") != expected_policy:
        fail("existing retirement receipt cutover policy differs from its intent")
    expected_checkpoint = {
        "descriptor_sha256": intent["checkpoint"]["descriptor_sha256"],
        "full_file_sha256": intent["checkpoint"]["checkpoint_file"]["sha256"],
        "full_file_size_bytes": intent["checkpoint"]["checkpoint_file"]["size_bytes"],
        "format_version": intent["checkpoint"]["format_version"],
        "chain_id": intent["checkpoint"]["chain_id"],
        "manifest_hash": intent["checkpoint"]["manifest_hash"],
        "payload_hash": intent["checkpoint"]["payload_hash"],
        "community_rewards_v1_activation_height": intent["checkpoint"][
            "community_rewards_v1_activation_height"
        ],
        "certificate_signing_hash": intent["checkpoint"]["checkpoint_certificate"][
            "signing_hash"
        ],
        "certificate_cryptographically_verified": True,
        "verified_signature_count": intent["checkpoint"]["verified_quorum"][
            "verified_signature_count"
        ],
        "signed_validator_addresses": intent["checkpoint"]["verified_quorum"][
            "signed_validator_addresses"
        ],
        "signed_stake": intent["checkpoint"]["verified_quorum"]["signed_stake"],
        "total_stake": intent["checkpoint"]["verified_quorum"]["total_stake"],
        "source_height": intent["checkpoint"]["source_height"],
        "source_block_hash": intent["checkpoint"]["source_block_hash"],
        "source_state_root": intent["checkpoint"]["source_state_root"],
        "transition_height": intent["checkpoint"]["transition_height"],
        "transition_block_hash": intent["checkpoint"]["transition_block_hash"],
        "canonical_history_source": "signed_recovery_checkpoint",
    }
    if receipt.get("checkpoint") != expected_checkpoint:
        fail("existing retirement receipt checkpoint binding differs from its intent")
    expected_process = {
        "retirement_mode": intent["old_process"]["retirement_mode"],
        "pid": intent["old_process"]["pid"],
        "boot_id": intent["old_process"]["boot_id"],
        "start_ticks": intent["old_process"]["start_ticks"],
        "legacy_version": intent["legacy_release"]["version"],
        "executable_sha256": intent["legacy_release"]["executable_sha256"],
        "signals_sent": stop["supervisor"]["signals_sent"],
        "exit_status_observed": stop["supervisor"]["exit_status_observed"],
    }
    if receipt.get("old_process") != expected_process:
        fail("existing retirement receipt process binding differs from its evidence")
    old_tree = receipt.get("old_data_tree")
    if (
        not isinstance(old_tree, dict)
        or set(old_tree) != {
            "path", "root_sha256", "entry_count", "total_file_bytes", "state_wal_sha256",
            "intent_wal_prefix_bytes", "intent_wal_prefix_sha256",
        }
        or old_tree.get("path") != intent["old_data"]["root_anchor"]["path"]
        or old_tree.get("intent_wal_prefix_bytes")
        != intent["old_data"]["wal_prefix"]["observed_prefix_bytes"]
        or old_tree.get("intent_wal_prefix_sha256")
        != intent["old_data"]["wal_prefix"]["observed_prefix_sha256"]
    ):
        fail("existing retirement receipt old-data binding differs from its intent")
    require_hash(old_tree.get("root_sha256"), "existing receipt old-tree root")
    require_hash(old_tree.get("state_wal_sha256"), "existing receipt state WAL")
    require_uint(old_tree.get("entry_count"), "existing receipt old-tree entry count", positive=True)
    require_uint(old_tree.get("total_file_bytes"), "existing receipt old-tree bytes", positive=True)
    if receipt.get("v08_start") != {
        "data_dir": intent["v08_start"]["data_dir"],
        "data_dir_fresh_and_absent": True,
        "canonical_history_source": "signed_recovery_checkpoint",
        "old_wal_migration_allowed": False,
    }:
        fail("existing retirement receipt v0.8 start binding differs from its intent")
    replay = receipt.get("local_legacy_replay")
    if intent["replay_inputs"]["mode"] == "forensic-only":
        if not isinstance(replay, dict) or replay.get("performed") is not False:
            fail("existing retirement receipt overclaims local canonical replay")
    elif not isinstance(replay, dict) or replay.get("performed") is not True:
        fail("existing retirement receipt omits required local canonical replay")


def finalize(
    *,
    intent_path: Path,
    expected_intent_sha256: str,
    stop_evidence_path: Path,
    expected_stop_evidence_sha256: str,
    receipt_output: Path,
    runtime: Runtime,
    runner: InspectorRunner = run_exact_inspector,
    stability_seconds: float = 10.0,
    samples: int = 3,
    sleep: Callable[[float], None] = time.sleep,
    now: Callable[[], str] = utc_now,
) -> tuple[dict[str, Any], str]:
    intent, intent_raw, _intent_record = load_canonical_json(
        intent_path,
        "retirement intent",
        expected_sha256=expected_intent_sha256,
    )
    validate_intent(intent)
    data_dir = Path(intent["old_data"]["root_anchor"]["path"])
    safe_output(receipt_output, "retirement receipt output", forbidden_root=data_dir)
    if receipt_output.exists():
        existing, existing_raw, _existing_record = load_canonical_json(
            receipt_output, "existing retirement receipt"
        )
        validate_receipt(existing)
        stop, _stop_raw, _stop_record = load_canonical_json(
            stop_evidence_path,
            "offline stop evidence",
            expected_sha256=expected_stop_evidence_sha256,
        )
        validate_stop_evidence(stop, intent, expected_intent_sha256)
        validate_existing_receipt_bindings(
            existing,
            intent,
            stop,
            expected_intent_sha256,
            expected_stop_evidence_sha256,
        )
        return existing, sha256_bytes(existing_raw)
    require_fresh_v08_path(Path(intent["v08_start"]["data_dir"]), data_dir)
    stop, stop_raw, _stop_record = load_canonical_json(
        stop_evidence_path,
        "TERM-only stop evidence",
        expected_sha256=expected_stop_evidence_sha256,
    )
    validate_stop_evidence(stop, intent, expected_intent_sha256)

    legacy_release = intent["legacy_release"]
    legacy_executable = legacy_release["executable"]
    _legacy_raw, observed_legacy_executable = stable_file(
        Path(legacy_executable["path"]),
        "legacy v0.7 executable",
        expected_sha256=legacy_release["executable_sha256"],
        maximum=1024 * 1024 * 1024,
    )
    _same_file_binding(
        observed_legacy_executable, legacy_executable, "legacy v0.7 executable"
    )
    supervisor_selected = legacy_release["supervisor_definition"]
    supervisor, _supervisor_raw, observed_supervisor = load_canonical_json(
        Path(supervisor_selected["path"]),
        "legacy supervisor binding",
        expected_sha256=supervisor_selected["sha256"],
    )
    _same_file_binding(observed_supervisor, supervisor_selected, "legacy supervisor binding")
    _semantics, observed_source = validate_supervisor_binding(
        supervisor,
        data_dir=data_dir,
        legacy_executable=Path(legacy_executable["path"]),
        legacy_executable_sha256=legacy_release["executable_sha256"],
    )
    _same_file_binding(
        observed_source, legacy_release["supervisor_source"], "legacy supervisor source"
    )

    inspector = intent["inspector"]
    inspector_path: Path | None = None
    inspector_record: dict[str, Any] | None = None
    if intent["replay_inputs"]["mode"] == "canonical-replay":
        if not isinstance(inspector.get("path"), str) or not isinstance(inspector.get("sha256"), str):
            fail("canonical-replay intent omits its exact local inspector")
        inspector_path = Path(inspector["path"])
        _inspector_raw, inspector_record = stable_file(
            inspector_path,
            "target release inspector binary",
            expected_sha256=inspector["sha256"],
            maximum=1024 * 1024 * 1024,
        )
    release_path = Path(intent["target_release"]["binding_path"])
    release, release_raw, _release_record = load_canonical_json(
        release_path,
        "target release binding",
        expected_sha256=intent["target_release"]["binding_sha256"],
    )
    release_binding = validate_release(
        release,
        inspector.get("asset"),
        inspector.get("sha256"),
        intent["cutover_policy"]["sha256"],
        intent["maintenance_boundary"]["sha256"],
        intent["checkpoint"]["descriptor_sha256"],
    )
    for field, expected in release_binding.items():
        if intent["target_release"].get(field) != expected:
            fail(f"target release {field} differs from the retirement intent")
    boundary_path = Path(intent["maintenance_boundary"]["path"])
    boundary, boundary_raw, _boundary_record = load_canonical_json(
        boundary_path,
        "legacy maintenance boundary",
        expected_sha256=intent["maintenance_boundary"]["sha256"],
    )
    boundary_binding = validate_boundary(boundary)
    for field, expected in boundary_binding.items():
        if intent["maintenance_boundary"].get(field) != expected:
            fail(f"legacy maintenance boundary {field} differs from the retirement intent")
    checkpoint_path = Path(intent["checkpoint"]["path"])
    checkpoint_descriptor, checkpoint_raw, checkpoint_record = load_canonical_json(
        checkpoint_path,
        "recovery checkpoint descriptor",
        expected_sha256=intent["checkpoint"]["descriptor_sha256"],
        maximum=1024 * 1024,
    )
    current_checkpoint = validate_checkpoint_descriptor(
        checkpoint_descriptor,
        release_binding=release_binding,
        boundary=boundary_binding,
    )
    for field, expected in current_checkpoint.items():
        if intent["checkpoint"].get(field) != expected:
            fail(f"checkpoint descriptor {field} differs from the retirement intent")
    policy_path = Path(intent["cutover_policy"]["path"])
    policy, policy_raw, _policy_record = load_canonical_json(
        policy_path,
        "cutover policy",
        expected_sha256=intent["cutover_policy"]["sha256"],
    )
    policy_binding = validate_cutover_policy(
        policy,
        release=release,
        release_binding=release_binding,
        boundary=boundary_binding,
        boundary_sha256=intent["maintenance_boundary"]["sha256"],
        checkpoint=current_checkpoint,
        checkpoint_descriptor_sha256=intent["checkpoint"]["descriptor_sha256"],
    )
    for field, expected in policy_binding.items():
        if intent["cutover_policy"].get(field) != expected:
            fail(f"cutover policy {field} differs from the retirement intent")
    cutoff_time = dt.datetime.strptime(
        require_utc(policy["legacy_admission_cutoff_utc"], "legacy admission cutoff"),
        "%Y-%m-%dT%H:%M:%SZ",
    )
    offline_time = dt.datetime.strptime(
        require_utc(stop["offline_observed_at"], "offline evidence completion"),
        "%Y-%m-%dT%H:%M:%SZ",
    )
    if offline_time < cutoff_time:
        fail("offline evidence predates the global legacy-admission cutoff")
    replay_records: dict[str, dict[str, Any]] = {}
    if intent["replay_inputs"]["mode"] == "canonical-replay":
        for name in ("snapshot", "genesis", "legacy_validator_set"):
            selected = intent["replay_inputs"][name]
            _raw, observed_record = stable_file(
                Path(selected["path"]),
                f"legacy replay {name}",
                expected_sha256=selected["sha256"],
                maximum=1024 * 1024 * 1024,
            )
            _same_file_binding(observed_record, selected, f"legacy replay {name}")
            replay_records[name] = observed_record

    root_now = os.lstat(data_dir)
    root_anchor = intent["old_data"]["root_anchor"]
    for field in ("device", "inode", "mode", "uid", "gid", "nlink"):
        if projection(root_now)[field] != root_anchor[field]:
            fail(f"legacy data-directory {field} differs from the retirement intent")
    verify_wal_prefix(data_dir / "state.wal", intent["old_data"]["wal_prefix"])
    offline = prove_stably_offline(
        intent,
        runtime,
        stability_seconds=stability_seconds,
        samples=samples,
        sleep=sleep,
    )
    tree_before = tree_snapshot(data_dir)

    wal_entries = [
        row for row in tree_before["entries"]
        if row.get("path") == "state.wal" and row.get("kind") == "file"
    ]
    if len(wal_entries) != 1:
        fail("stable legacy data tree must contain exactly one top-level state.wal")
    state_wal_sha = wal_entries[0]["sha256"]
    block_binding: dict[str, Any] | None = None
    block_stdout_sha: str | None = None
    if intent["replay_inputs"]["mode"] == "canonical-replay":
        assert inspector_path is not None and inspector.get("sha256") is not None
        block_argv = [
            "recovery", "inspect-legacy-block", "--data-dir", os.fspath(data_dir),
            "--snapshot", intent["replay_inputs"]["snapshot"]["path"],
            "--genesis", intent["replay_inputs"]["genesis"]["path"],
            "--legacy-validator-set", intent["replay_inputs"]["legacy_validator_set"]["path"],
            "--height", str(intent["checkpoint"]["source_height"]),
            "--expected-state-wal-sha256", state_wal_sha,
            "--expected-snapshot-sha256", intent["replay_inputs"]["snapshot"]["sha256"],
            "--expected-genesis-sha256", intent["replay_inputs"]["genesis"]["sha256"],
            "--expected-legacy-validator-set-sha256",
            intent["replay_inputs"]["legacy_validator_set"]["sha256"],
        ]
        if intent["replay_inputs"]["allow_unbound_legacy_wal"]:
            block_argv.append("--allow-unbound-legacy-wal")
        block_inspection, block_stdout_sha = runner(
            inspector_path, inspector["sha256"], block_argv, receipt_output.parent
        )
        block_binding = validate_legacy_block_inspection(block_inspection, intent, tree_before)
    tree_after = tree_snapshot(data_dir)
    if tree_after != tree_before:
        fail("legacy data tree changed during strict offline replay verification")
    for name, selected in replay_records.items():
        _raw, observed_record = stable_file(
            Path(selected["path"]),
            f"legacy replay {name} after inspection",
            expected_sha256=selected["sha256"],
            maximum=1024 * 1024 * 1024,
        )
        _same_file_binding(observed_record, selected, f"legacy replay {name} after inspection")
    offline_after = prove_stably_offline(
        intent,
        runtime,
        stability_seconds=stability_seconds,
        samples=samples,
        sleep=sleep,
    )
    if offline_after != offline:
        fail("offline process/listener proof changed during verification")
    require_fresh_v08_path(Path(intent["v08_start"]["data_dir"]), data_dir)

    stop_supervisor = stop["supervisor"]
    receipt: dict[str, Any] = {
        "schema": RECEIPT_SCHEMA,
        "verified_at": now(),
        "intent_sha256": expected_intent_sha256,
        "stop_evidence_sha256": expected_stop_evidence_sha256,
        "protocol_id": intent["protocol_id"],
        "scope": intent["scope"],
        "target_release": {
            "tag": intent["target_release"]["tag"],
            "commit": intent["target_release"]["commit"],
            "binding_schema": intent["target_release"]["binding_schema"],
            "binding_sha256": sha256_bytes(release_raw),
            "manifest_sha256": intent["target_release"]["manifest_sha256"],
            "manifest_signature_sha256": intent["target_release"].get(
                "manifest_signature_sha256"
            ),
            "local_inspector_sha256": inspector_record["sha256"] if inspector_record else None,
        },
        "maintenance_boundary": {
            "sha256": sha256_bytes(boundary_raw),
            "observed_cutoff_height": intent["maintenance_boundary"]["observed_cutoff_height"],
            "legacy_public_max_height": intent["maintenance_boundary"]["legacy_public_max_height"],
            "global_absence_claimed": False,
        },
        "cutover_policy": {
            "sha256": sha256_bytes(policy_raw),
            "uncompleted_job_disposition": JOBS_DISPOSITION,
            "legacy_exit_clean_claimed": False,
            "legacy_restart_allowed": False,
            "global_legacy_absence_claimed": False,
            "offline_retirement_receipt_required": True,
            "v08_start_requires_offline_receipt": True,
        },
        "checkpoint": {
            "descriptor_sha256": checkpoint_record["sha256"],
            "full_file_sha256": intent["checkpoint"]["checkpoint_file"]["sha256"],
            "full_file_size_bytes": intent["checkpoint"]["checkpoint_file"]["size_bytes"],
            "format_version": intent["checkpoint"]["format_version"],
            "chain_id": intent["checkpoint"]["chain_id"],
            "manifest_hash": intent["checkpoint"]["manifest_hash"],
            "payload_hash": intent["checkpoint"]["payload_hash"],
            "community_rewards_v1_activation_height": intent["checkpoint"][
                "community_rewards_v1_activation_height"
            ],
            "certificate_signing_hash": intent["checkpoint"]["checkpoint_certificate"][
                "signing_hash"
            ],
            "certificate_cryptographically_verified": intent["checkpoint"][
                "certificate_cryptographically_verified"
            ],
            "verified_signature_count": intent["checkpoint"]["verified_quorum"][
                "verified_signature_count"
            ],
            "signed_validator_addresses": intent["checkpoint"]["verified_quorum"][
                "signed_validator_addresses"
            ],
            "signed_stake": intent["checkpoint"]["verified_quorum"]["signed_stake"],
            "total_stake": intent["checkpoint"]["verified_quorum"]["total_stake"],
            "source_height": intent["checkpoint"]["source_height"],
            "source_block_hash": intent["checkpoint"]["source_block_hash"],
            "source_state_root": intent["checkpoint"]["source_state_root"],
            "transition_height": intent["checkpoint"]["transition_height"],
            "transition_block_hash": intent["checkpoint"]["transition_block_hash"],
            "canonical_history_source": "signed_recovery_checkpoint",
        },
        "old_process": {
            "retirement_mode": intent["old_process"]["retirement_mode"],
            "pid": intent["old_process"]["pid"],
            "boot_id": intent["old_process"]["boot_id"],
            "start_ticks": intent["old_process"]["start_ticks"],
            "legacy_version": intent["legacy_release"]["version"],
            "executable_sha256": intent["legacy_release"]["executable_sha256"],
            "signals_sent": stop_supervisor["signals_sent"],
            "exit_status_observed": stop_supervisor["exit_status_observed"],
        },
        "offline_stability": offline,
        "old_data_tree": {
            "path": os.fspath(data_dir),
            "root_sha256": tree_before["root_sha256"],
            "entry_count": tree_before["entry_count"],
            "total_file_bytes": tree_before["total_file_bytes"],
            "state_wal_sha256": state_wal_sha,
            "intent_wal_prefix_bytes": intent["old_data"]["wal_prefix"]["observed_prefix_bytes"],
            "intent_wal_prefix_sha256": intent["old_data"]["wal_prefix"]["observed_prefix_sha256"],
        },
        "v08_start": {
            "data_dir": intent["v08_start"]["data_dir"],
            "data_dir_fresh_and_absent": True,
            "canonical_history_source": "signed_recovery_checkpoint",
            "old_wal_migration_allowed": False,
        },
        "local_legacy_replay": (
            {
                "performed": False,
                "classification": "preserved_noncanonical_forensic_not_migrated",
                "canonical_history_source": "signed_recovery_checkpoint",
                "inspection": None,
            }
            if block_binding is None
            else {
                "performed": True,
                "classification": "preserved_local_canonical_boundary_verified_not_migrated",
                "canonical_history_source": "signed_recovery_checkpoint",
                "inspection": {**block_binding, "stdout_sha256": block_stdout_sha},
            }
        ),
        "retirement_result": {
            "retired": True,
            "stake": 0,
            "legacy_process_stably_absent": True,
            "legacy_listeners_stably_absent": True,
            "sigkill_sent": False,
            "legacy_exit_clean_claimed": False,
            "legacy_jobs_disposition": JOBS_DISPOSITION,
            "legacy_data_opened_writable_by_verifier": False,
            "legacy_data_changed_during_verification": False,
            "legacy_data_disposition": (
                "preserved_noncanonical_forensic_not_migrated"
                if block_binding is None
                else "preserved_local_canonical_boundary_verified_not_migrated"
            ),
            "canonical_history_source": "signed_recovery_checkpoint",
            "old_wal_copied_to_v08": False,
            "v08_data_dir_fresh_at_receipt": True,
            "canonical_chain_history_rewritten": False,
        },
    }
    validate_receipt(receipt)
    digest = publish_create_only_atomic(receipt_output, receipt, "retirement receipt")
    return receipt, digest


def absolute_argument(value: str) -> Path:
    path = Path(value)
    try:
        return canonical_absolute(path, "path argument", must_exist=False)
    except RetirementError as error:
        raise argparse.ArgumentTypeError(str(error)) from error


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(
        prog="arc-v07-retirement-verifier",
        description="Offline, create-only ARC v0.7 stake-zero retirement evidence",
    )
    root.add_argument("--version", action="version", version="arc-v07-retirement-verifier 1")
    commands = root.add_subparsers(dest="command", required=True)

    create = commands.add_parser(
        "create-intent", help="bind a running or already-offline v0.7 stake-zero node"
    )
    create.add_argument("--intent-output", required=True, type=absolute_argument)
    create.add_argument("--target-release", required=True, type=absolute_argument)
    create.add_argument("--target-release-sha256", required=True)
    create.add_argument("--maintenance-boundary", required=True, type=absolute_argument)
    create.add_argument("--maintenance-boundary-sha256", required=True)
    create.add_argument("--cutover-policy", required=True, type=absolute_argument)
    create.add_argument("--cutover-policy-sha256", required=True)
    create.add_argument("--checkpoint-descriptor", required=True, type=absolute_argument)
    create.add_argument("--checkpoint-descriptor-sha256", required=True)
    create.add_argument("--inspector-binary", type=absolute_argument)
    create.add_argument("--inspector-asset")
    create.add_argument("--inspector-sha256")
    process_mode = create.add_mutually_exclusive_group(required=True)
    process_mode.add_argument("--legacy-pid", type=int)
    process_mode.add_argument("--already-offline", action="store_true")
    create.add_argument("--legacy-version", required=True)
    create.add_argument("--legacy-executable", required=True, type=absolute_argument)
    create.add_argument("--legacy-executable-sha256", required=True)
    create.add_argument("--supervisor-definition", required=True, type=absolute_argument)
    create.add_argument("--supervisor-definition-sha256", required=True)
    create.add_argument("--data-dir", required=True, type=absolute_argument)
    create.add_argument("--v08-data-dir", required=True, type=absolute_argument)
    replay_mode = create.add_mutually_exclusive_group(required=True)
    replay_mode.add_argument("--forensic-only", action="store_true")
    replay_mode.add_argument("--canonical-replay", action="store_true")
    create.add_argument("--snapshot", type=absolute_argument)
    create.add_argument("--snapshot-sha256")
    create.add_argument("--genesis", type=absolute_argument)
    create.add_argument("--genesis-sha256")
    create.add_argument("--legacy-validator-set", type=absolute_argument)
    create.add_argument("--legacy-validator-set-sha256")
    wal_policy = create.add_mutually_exclusive_group(required=False)
    wal_policy.add_argument("--allow-unbound-legacy-wal", action="store_true")
    wal_policy.add_argument("--require-bound-legacy-wal", action="store_true")

    finish = commands.add_parser("finalize", help="prove stable offline state and seal receipt")
    finish.add_argument("--intent", required=True, type=absolute_argument)
    finish.add_argument("--intent-sha256", required=True)
    finish.add_argument("--stop-evidence", required=True, type=absolute_argument)
    finish.add_argument("--stop-evidence-sha256", required=True)
    finish.add_argument("--receipt-output", required=True, type=absolute_argument)
    finish.add_argument("--stability-seconds", type=float, default=10.0)
    finish.add_argument("--samples", type=int, default=3)
    return root


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        runtime = LinuxProcRuntime()
        if args.command == "create-intent":
            request = PrepareRequest(
                intent_output=args.intent_output,
                target_release=args.target_release,
                target_release_sha256=require_hash(
                    args.target_release_sha256, "target release binding sha256"
                ),
                maintenance_boundary=args.maintenance_boundary,
                maintenance_boundary_sha256=require_hash(
                    args.maintenance_boundary_sha256, "maintenance boundary sha256"
                ),
                cutover_policy=args.cutover_policy,
                cutover_policy_sha256=require_hash(
                    args.cutover_policy_sha256, "cutover policy sha256"
                ),
                checkpoint=args.checkpoint_descriptor,
                checkpoint_sha256=require_hash(
                    args.checkpoint_descriptor_sha256, "checkpoint descriptor sha256"
                ),
                inspector_binary=args.inspector_binary,
                inspector_asset=args.inspector_asset,
                inspector_sha256=(
                    require_hash(args.inspector_sha256, "inspector sha256")
                    if args.inspector_sha256 is not None
                    else None
                ),
                retirement_mode="preexisting_offline" if args.already_offline else "term_only",
                legacy_pid=args.legacy_pid,
                legacy_version=args.legacy_version,
                legacy_executable=args.legacy_executable,
                legacy_executable_sha256=require_hash(
                    args.legacy_executable_sha256, "legacy executable sha256"
                ),
                supervisor_definition=args.supervisor_definition,
                supervisor_definition_sha256=require_hash(
                    args.supervisor_definition_sha256, "supervisor definition sha256"
                ),
                data_dir=args.data_dir,
                v08_data_dir=args.v08_data_dir,
                replay_mode="canonical-replay" if args.canonical_replay else "forensic-only",
                snapshot=args.snapshot,
                snapshot_sha256=(
                    require_hash(args.snapshot_sha256, "snapshot sha256")
                    if args.snapshot_sha256 is not None
                    else None
                ),
                genesis=args.genesis,
                genesis_sha256=(
                    require_hash(args.genesis_sha256, "genesis sha256")
                    if args.genesis_sha256 is not None
                    else None
                ),
                legacy_validator_set=args.legacy_validator_set,
                legacy_validator_set_sha256=(
                    require_hash(
                        args.legacy_validator_set_sha256, "legacy validator set sha256"
                    )
                    if args.legacy_validator_set_sha256 is not None
                    else None
                ),
                allow_unbound_legacy_wal=(
                    args.allow_unbound_legacy_wal
                    if args.allow_unbound_legacy_wal or args.require_bound_legacy_wal
                    else None
                ),
            )
            value, digest = prepare_intent(request, runtime=runtime)
            output = {
                "schema": INTENT_SCHEMA,
                "output": os.fspath(request.intent_output),
                "sha256": digest,
                "protocol_id": value["protocol_id"],
            }
        elif args.command == "finalize":
            if args.stability_seconds < 5 or args.stability_seconds > 300:
                fail("CLI stability duration must be between 5 and 300 seconds")
            if args.samples < 3 or args.samples > 20:
                fail("CLI offline sample count must be between 3 and 20")
            value, digest = finalize(
                intent_path=args.intent,
                expected_intent_sha256=require_hash(args.intent_sha256, "intent sha256"),
                stop_evidence_path=args.stop_evidence,
                expected_stop_evidence_sha256=require_hash(
                    args.stop_evidence_sha256, "stop evidence sha256"
                ),
                receipt_output=args.receipt_output,
                runtime=runtime,
                stability_seconds=args.stability_seconds,
                samples=args.samples,
            )
            output = {
                "schema": RECEIPT_SCHEMA,
                "output": os.fspath(args.receipt_output),
                "sha256": digest,
                "protocol_id": value["protocol_id"],
            }
        else:  # pragma: no cover - argparse enforces the command set.
            fail("unsupported command")
        sys.stdout.buffer.write(canonical_bytes(output))
        return 0
    except (RetirementError, OSError) as error:
        print(f"arc-v07-retirement-verifier: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
