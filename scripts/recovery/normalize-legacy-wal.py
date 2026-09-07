#!/usr/bin/env python3
"""Create one content-addressed, sequence-normalized legacy state-WAL derivative.

The source WAL and matching snapshot are opened read-only with ``O_NOFOLLOW``
and are re-proved after the derivative is sealed.  Every source byte must be
covered exactly once by the plan's ordered partition.  Selected frame runs are
copied in plan order; only the sequence field and its CRC32 may change.
Excluded runs remain represented by their hashes and byte ranges in both the
plan and the emitted receipt.  This tool never edits or replaces a source.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import stat
import struct
import sys
import zlib
from typing import Any, BinaryIO, Mapping


SCHEMA = "arc.recovery.legacy-wal-normalization-plan.v1"
RECEIPT_SCHEMA = "arc.recovery.legacy-wal-normalization.v1"
HASH_RE = re.compile(r"[0-9a-f]{64}")
MAX_WAL_ENTRY_BYTES = 256 * 1024 * 1024
MAX_PLAN_BYTES = 4 * 1024 * 1024
MAX_SNAPSHOT_BYTES = 256 * 1024 * 1024
FILE_IDENTITY_FIELDS = {
    "device",
    "inode",
    "mode",
    "uid",
    "gid",
    "nlink",
    "size",
    "mtime_ns",
    "ctime_ns",
    "sha256",
}


class NormalizationError(ValueError):
    """The normalization plan or an input failed a closed-world check."""


def fail(message: str) -> None:
    raise NormalizationError(message)


def canonical_bytes(value: Any) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


def require_hash(value: Any, label: str) -> str:
    if not isinstance(value, str) or HASH_RE.fullmatch(value) is None:
        fail(f"{label} must be exactly 64 lowercase hexadecimal characters")
    return value


def require_uint(value: Any, label: str, *, positive: bool = False) -> int:
    minimum = 1 if positive else 0
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        fail(f"{label} must be a {'positive' if positive else 'non-negative'} integer")
    return value


def fsync_directory(path: pathlib.Path) -> None:
    descriptor = os.open(
        path,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def identity(details: os.stat_result, digest: str) -> dict[str, Any]:
    return {
        "device": details.st_dev,
        "inode": details.st_ino,
        "mode": details.st_mode,
        "uid": details.st_uid,
        "gid": details.st_gid,
        "nlink": details.st_nlink,
        "size": details.st_size,
        "mtime_ns": details.st_mtime_ns,
        "ctime_ns": details.st_ctime_ns,
        "sha256": digest,
    }


def metadata_projection(details: os.stat_result) -> tuple[int, ...]:
    return (
        details.st_dev,
        details.st_ino,
        details.st_mode,
        details.st_uid,
        details.st_gid,
        details.st_nlink,
        details.st_size,
        details.st_mtime_ns,
        details.st_ctime_ns,
    )


def open_input(
    path: pathlib.Path, label: str, *, maximum: int | None = None
) -> tuple[int, os.stat_result]:
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    details = os.fstat(descriptor)
    path_details = path.lstat()
    if (
        not stat.S_ISREG(details.st_mode)
        or path.is_symlink()
        or (details.st_dev, details.st_ino) != (path_details.st_dev, path_details.st_ino)
        or details.st_uid != os.geteuid()
        or details.st_nlink != 1
        or details.st_mode & 0o022
        or details.st_size <= 0
        or (maximum is not None and details.st_size > maximum)
    ):
        os.close(descriptor)
        fail(f"{label} identity is unsafe")
    return descriptor, details


def hash_fd(descriptor: int, *, start: int = 0, end: int | None = None) -> str:
    details = os.fstat(descriptor)
    stop = details.st_size if end is None else end
    if start < 0 or stop < start or stop > details.st_size:
        fail("hash range is outside the held input")
    result = hashlib.sha256()
    offset = start
    while offset < stop:
        chunk = os.pread(descriptor, min(1024 * 1024, stop - offset), offset)
        if not chunk:
            fail("held input ended during hashing")
        result.update(chunk)
        offset += len(chunk)
    return result.hexdigest()


def read_exact_at(descriptor: int, offset: int, length: int, label: str) -> bytes:
    chunks: list[bytes] = []
    remaining = length
    cursor = offset
    while remaining:
        chunk = os.pread(descriptor, min(1024 * 1024, remaining), cursor)
        if not chunk:
            fail(f"{label} ended before its declared boundary")
        chunks.append(chunk)
        cursor += len(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def parse_plan(raw: bytes, expected_sha256: str) -> dict[str, Any]:
    if hashlib.sha256(raw).hexdigest() != expected_sha256:
        fail("normalization plan hash differs")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise NormalizationError("normalization plan is invalid JSON") from error
    if not isinstance(value, dict) or canonical_bytes(value) != raw:
        fail("normalization plan is not canonical JSON")
    if set(value) != {
        "schema",
        "node",
        "source_wal",
        "source_snapshot",
        "derivative_wal",
        "head",
        "partition",
        "sequence_rewrites",
    }:
        fail("normalization plan fields differ")
    if value.get("schema") != SCHEMA or value.get("node") not in {"lax", "ams"}:
        fail("normalization plan schema/node differs")
    for name in ("source_wal", "source_snapshot", "derivative_wal"):
        item = value.get(name)
        if not isinstance(item, dict) or set(item) != {"sha256", "size"}:
            fail(f"normalization plan {name} fields differ")
        require_hash(item.get("sha256"), f"normalization plan {name} sha256")
        require_uint(item.get("size"), f"normalization plan {name} size", positive=True)
    head = value.get("head")
    if not isinstance(head, dict) or set(head) != {"height", "block_hash", "state_root"}:
        fail("normalization plan head fields differ")
    require_uint(head.get("height"), "normalization plan head height", positive=True)
    require_hash(head.get("block_hash"), "normalization plan head block hash")
    require_hash(head.get("state_root"), "normalization plan head state root")
    partition = value.get("partition")
    if not isinstance(partition, list) or not partition:
        fail("normalization plan partition is empty")
    expected_start = 0
    selected_bytes = 0
    selected_ranges: list[tuple[int, int]] = []
    for index, row in enumerate(partition):
        if not isinstance(row, dict):
            fail(f"normalization partition row {index} is not an object")
        kind = row.get("kind")
        common = {"kind", "source_start", "source_end", "sha256"}
        if kind == "selected-frame-run":
            fields = common | {
                "frame_count",
                "source_first_sequence",
                "source_last_sequence",
                "sequence_delta",
                "derivative_first_sequence",
                "derivative_last_sequence",
            }
        elif kind == "excluded-zero-fill":
            fields = common | {"classification"}
            if row.get("classification") != "interior-zero-fill-gap":
                fail("zero-fill partition classification differs")
        elif kind == "excluded-valid-frame-run":
            fields = common | {
                "classification",
                "frame_count",
                "first_sequence",
                "last_sequence",
            }
            if row.get("classification") != "stale-valid-fork":
                fail("excluded frame-run classification differs")
        else:
            fail(f"normalization partition row {index} kind differs")
        if set(row) != fields:
            fail(f"normalization partition row {index} fields differ")
        start = require_uint(row.get("source_start"), f"partition row {index} start")
        end = require_uint(row.get("source_end"), f"partition row {index} end", positive=True)
        require_hash(row.get("sha256"), f"partition row {index} sha256")
        if start != expected_start or end <= start:
            fail("normalization partition is not an exact ordered byte partition")
        expected_start = end
        if kind == "selected-frame-run":
            count = require_uint(row.get("frame_count"), "selected frame count", positive=True)
            first = require_uint(row.get("source_first_sequence"), "selected first sequence")
            last = require_uint(row.get("source_last_sequence"), "selected last sequence")
            derivative_first = require_uint(
                row.get("derivative_first_sequence"), "derivative first sequence"
            )
            derivative_last = require_uint(
                row.get("derivative_last_sequence"), "derivative last sequence"
            )
            delta = row.get("sequence_delta")
            if isinstance(delta, bool) or not isinstance(delta, int):
                fail("selected sequence delta is not an integer")
            if last - first + 1 != count or derivative_last - derivative_first + 1 != count:
                fail("selected frame-run sequence/count accounting differs")
            selected_bytes += end - start
            selected_ranges.append((start, end))
        elif kind == "excluded-valid-frame-run":
            count = require_uint(row.get("frame_count"), "excluded frame count", positive=True)
            first = require_uint(row.get("first_sequence"), "excluded first sequence")
            last = require_uint(row.get("last_sequence"), "excluded last sequence")
            if last - first + 1 != count:
                fail("excluded frame-run sequence/count accounting differs")
    if expected_start != value["source_wal"]["size"]:
        fail("normalization partition does not cover every source WAL byte")
    if selected_bytes != value["derivative_wal"]["size"]:
        fail("selected partition bytes do not equal derivative size")
    rewrites = value.get("sequence_rewrites")
    if not isinstance(rewrites, list):
        fail("normalization sequence rewrites are not a list")
    previous_offset = -1
    for index, rewrite in enumerate(rewrites):
        if not isinstance(rewrite, dict) or set(rewrite) != {
            "source_offset",
            "source_sequence",
            "derivative_sequence",
        }:
            fail(f"sequence rewrite {index} fields differ")
        offset = require_uint(rewrite.get("source_offset"), "sequence rewrite offset")
        require_uint(rewrite.get("source_sequence"), "sequence rewrite source")
        require_uint(rewrite.get("derivative_sequence"), "sequence rewrite derivative")
        if offset <= previous_offset:
            fail("sequence rewrites are not strictly ordered")
        if not any(start <= offset < end for start, end in selected_ranges):
            fail("sequence rewrite offset is outside every selected frame run")
        previous_offset = offset
    return value


def read_frame(
    descriptor: int, offset: int, run_end: int
) -> tuple[bytes, int, int, bytes]:
    if run_end - offset < 4:
        fail(f"truncated WAL frame length at byte {offset}")
    length_raw = read_exact_at(descriptor, offset, 4, "WAL frame length")
    length = struct.unpack("<I", length_raw)[0]
    if length == 0 or length > MAX_WAL_ENTRY_BYTES:
        fail(f"invalid WAL frame length {length} at byte {offset}")
    frame_end = offset + 4 + length
    if frame_end > run_end:
        fail(f"WAL frame at byte {offset} crosses its manifest run boundary")
    payload = read_exact_at(descriptor, offset + 4, length, "WAL frame payload")
    if len(payload) < 20:
        fail(f"WAL frame at byte {offset} is too short for the stable envelope")
    expected_crc = struct.unpack_from("<I", payload, len(payload) - 4)[0]
    actual_crc = zlib.crc32(payload[:-4]) & 0xFFFFFFFF
    if actual_crc != expected_crc:
        fail(f"WAL frame checksum differs at byte {offset}")
    sequence = struct.unpack_from("<Q", payload, 8)[0]
    return length_raw, frame_end, sequence, payload


def validate_excluded_frame_run(
    descriptor: int, row: Mapping[str, Any]
) -> None:
    offset = row["source_start"]
    count = 0
    first: int | None = None
    previous: int | None = None
    while offset < row["source_end"]:
        _length, frame_end, sequence, _payload = read_frame(
            descriptor, offset, row["source_end"]
        )
        if first is None:
            first = sequence
        if previous is not None and sequence != previous + 1:
            fail("excluded valid frame run is not sequence-contiguous")
        previous = sequence
        count += 1
        offset = frame_end
    if (
        count != row["frame_count"]
        or first != row["first_sequence"]
        or previous != row["last_sequence"]
    ):
        fail("excluded valid frame-run inventory differs")


def write_all(handle: BinaryIO, raw: bytes) -> None:
    written = handle.write(raw)
    if written != len(raw):
        fail("short derivative WAL write")


def normalize(
    plan: Mapping[str, Any],
    plan_sha256: str,
    source_path: pathlib.Path,
    snapshot_path: pathlib.Path,
    output_dir: pathlib.Path,
) -> dict[str, Any]:
    source_fd, source_before = open_input(source_path, "source WAL")
    snapshot_fd, snapshot_before = open_input(
        snapshot_path, "source snapshot", maximum=MAX_SNAPSHOT_BYTES
    )
    try:
        source_sha = hash_fd(source_fd)
        snapshot_sha = hash_fd(snapshot_fd)
        if (
            source_before.st_size != plan["source_wal"]["size"]
            or source_sha != plan["source_wal"]["sha256"]
        ):
            fail("source WAL differs from the content-addressed plan")
        if (
            snapshot_before.st_size != plan["source_snapshot"]["size"]
            or snapshot_sha != plan["source_snapshot"]["sha256"]
        ):
            fail("source snapshot differs from the content-addressed plan")
        if output_dir.exists() or output_dir.is_symlink():
            fail("normalization output directory already exists")
        output_parent = output_dir.parent
        parent_details = output_parent.lstat()
        if (
            output_parent.is_symlink()
            or not stat.S_ISDIR(parent_details.st_mode)
            or parent_details.st_uid != os.geteuid()
            or parent_details.st_mode & 0o022
        ):
            fail("normalization output parent is unsafe")
        os.mkdir(output_dir, 0o700)
        fsync_directory(output_parent)
        partial_path = output_dir / "state.wal.partial"
        final_path = output_dir / "state.wal"
        output_fd = os.open(
            partial_path,
            os.O_WRONLY
            | os.O_CREAT
            | os.O_EXCL
            | getattr(os, "O_NOFOLLOW", 0),
            0o600,
        )
        output_hasher = hashlib.sha256()
        semantic_hasher = hashlib.sha256()
        expected_sequence = 0
        selected_frame_count = 0
        excluded_bytes = 0
        rewrite_by_offset = {
            row["source_offset"]: row for row in plan["sequence_rewrites"]
        }
        consumed_rewrites: set[int] = set()
        try:
            with os.fdopen(output_fd, "wb", closefd=False) as output:
                for row in plan["partition"]:
                    start, end = row["source_start"], row["source_end"]
                    if hash_fd(source_fd, start=start, end=end) != row["sha256"]:
                        fail("source WAL partition hash differs")
                    if row["kind"] == "excluded-zero-fill":
                        cursor = start
                        while cursor < end:
                            chunk = read_exact_at(
                                source_fd,
                                cursor,
                                min(1024 * 1024, end - cursor),
                                "zero-fill partition",
                            )
                            if any(chunk):
                                fail("excluded zero-fill partition contains a nonzero byte")
                            cursor += len(chunk)
                        excluded_bytes += end - start
                        continue
                    if row["kind"] == "excluded-valid-frame-run":
                        validate_excluded_frame_run(source_fd, row)
                        excluded_bytes += end - start
                        continue
                    offset = start
                    run_count = 0
                    first_source: int | None = None
                    last_source: int | None = None
                    first_derivative: int | None = None
                    last_derivative: int | None = None
                    while offset < end:
                        length_raw, frame_end, source_sequence, payload = read_frame(
                            source_fd, offset, end
                        )
                        if first_source is None:
                            first_source = source_sequence
                        last_source = source_sequence
                        rewrite = rewrite_by_offset.get(offset)
                        if rewrite is not None:
                            if source_sequence != rewrite["source_sequence"]:
                                fail("explicit sequence rewrite source differs")
                            derivative_sequence = rewrite["derivative_sequence"]
                            consumed_rewrites.add(offset)
                        else:
                            derivative_sequence = source_sequence + row["sequence_delta"]
                        if derivative_sequence < 0 or derivative_sequence > 0xFFFFFFFFFFFFFFFF:
                            fail("normalized WAL sequence is outside u64")
                        if derivative_sequence != expected_sequence:
                            fail(
                                "normalized WAL is not globally sequence-contiguous: "
                                f"expected {expected_sequence}, got {derivative_sequence}"
                            )
                        if first_derivative is None:
                            first_derivative = derivative_sequence
                        last_derivative = derivative_sequence
                        normalized = bytearray(payload)
                        if derivative_sequence != source_sequence:
                            struct.pack_into("<Q", normalized, 8, derivative_sequence)
                            struct.pack_into(
                                "<I",
                                normalized,
                                len(normalized) - 4,
                                zlib.crc32(normalized[:-4]) & 0xFFFFFFFF,
                            )
                        semantic = payload[:8] + payload[16:-4]
                        semantic_hasher.update(struct.pack("<Q", len(semantic)))
                        semantic_hasher.update(semantic)
                        frame = length_raw + normalized
                        write_all(output, frame)
                        output_hasher.update(frame)
                        selected_frame_count += 1
                        run_count += 1
                        expected_sequence += 1
                        offset = frame_end
                    observed = (
                        run_count,
                        first_source,
                        last_source,
                        first_derivative,
                        last_derivative,
                    )
                    wanted = (
                        row["frame_count"],
                        row["source_first_sequence"],
                        row["source_last_sequence"],
                        row["derivative_first_sequence"],
                        row["derivative_last_sequence"],
                    )
                    if observed != wanted:
                        fail("selected frame-run inventory differs")
                output.flush()
                os.fsync(output.fileno())
                os.fchmod(output.fileno(), 0o400)
                os.fsync(output.fileno())
        finally:
            os.close(output_fd)
        if consumed_rewrites != set(rewrite_by_offset):
            fail("one or more explicit sequence rewrites were not selected")
        derivative_details = partial_path.lstat()
        derivative_sha = output_hasher.hexdigest()
        verification_fd = os.open(
            partial_path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
        )
        try:
            verified_derivative_sha = hash_fd(verification_fd)
        finally:
            os.close(verification_fd)
        if (
            derivative_details.st_size != plan["derivative_wal"]["size"]
            or derivative_sha != plan["derivative_wal"]["sha256"]
            or verified_derivative_sha != derivative_sha
        ):
            fail("normalized derivative differs from the content-addressed plan")
        os.rename(partial_path, final_path)
        fsync_directory(output_dir)
        os.chmod(output_dir, 0o500)
        fsync_directory(output_dir)
        fsync_directory(output_parent)
        derivative_fd, derivative_after = open_input(final_path, "normalized derivative")
        try:
            derivative_identity = identity(derivative_after, hash_fd(derivative_fd))
        finally:
            os.close(derivative_fd)
        if metadata_projection(os.fstat(source_fd)) != metadata_projection(source_before):
            fail("source WAL identity changed during normalization")
        if metadata_projection(os.fstat(snapshot_fd)) != metadata_projection(snapshot_before):
            fail("source snapshot identity changed during normalization")
        if hash_fd(source_fd) != source_sha or hash_fd(snapshot_fd) != snapshot_sha:
            fail("source WAL or snapshot bytes changed during normalization")
        return {
            "schema": RECEIPT_SCHEMA,
            "plan_sha256": plan_sha256,
            "node": plan["node"],
            "source_wal": identity(source_before, source_sha),
            "source_snapshot": identity(snapshot_before, snapshot_sha),
            "partition": plan["partition"],
            "sequence_rewrites": plan["sequence_rewrites"],
            "derivative_wal": derivative_identity,
            "head": plan["head"],
            "selected_frame_count": selected_frame_count,
            "excluded_bytes": excluded_bytes,
            "semantic_stream_sha256": semantic_hasher.hexdigest(),
            "transform": "copy-selected-runs-rewrite-sequence-and-crc32",
            "source_unchanged": True,
        }
    finally:
        os.close(source_fd)
        os.close(snapshot_fd)


def load_plan(path: pathlib.Path, expected_sha256: str) -> dict[str, Any]:
    descriptor, details = open_input(path, "normalization plan", maximum=MAX_PLAN_BYTES)
    try:
        raw = read_exact_at(descriptor, 0, details.st_size, "normalization plan")
        if metadata_projection(os.fstat(descriptor)) != metadata_projection(details):
            fail("normalization plan changed while read")
    finally:
        os.close(descriptor)
    return parse_plan(raw, expected_sha256)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--plan", type=pathlib.Path, required=True)
    result.add_argument("--plan-sha256", required=True)
    result.add_argument("--source-wal", type=pathlib.Path, required=True)
    result.add_argument("--snapshot", type=pathlib.Path, required=True)
    result.add_argument("--output-data-dir", type=pathlib.Path, required=True)
    return result


def main() -> int:
    args = parser().parse_args()
    plan_sha256 = require_hash(args.plan_sha256, "normalization plan hash")
    for path, label in (
        (args.plan, "normalization plan"),
        (args.source_wal, "source WAL"),
        (args.snapshot, "source snapshot"),
        (args.output_data_dir, "output data directory"),
    ):
        if not path.is_absolute() or ".." in path.parts:
            fail(f"{label} path must be normalized and absolute")
    plan = load_plan(args.plan, plan_sha256)
    receipt = normalize(
        plan,
        plan_sha256,
        args.source_wal,
        args.snapshot,
        args.output_data_dir,
    )
    sys.stdout.buffer.write(canonical_bytes(receipt))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (NormalizationError, OSError) as error:
        print(f"legacy WAL normalization: {error}", file=sys.stderr)
        raise SystemExit(1) from error
