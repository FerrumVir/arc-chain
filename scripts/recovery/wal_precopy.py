#!/usr/bin/env python3
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import stat
import sys
import time
from pathlib import Path
from typing import Any, BinaryIO, Callable

HEADER_SCHEMA = "arc.wal-precopy.header.v1"
FOOTER_SCHEMA = "arc.wal-precopy.footer.v1"
MANIFEST_SCHEMA = "arc.wal-precopy.manifest.v1"
PROTOCOL_VERSION = 1
STATUS_PRECOPY_ONLY = "PRECOPY_ONLY"
CANDIDATE_PREFIX = "peer-recovery-candidate-"
SOURCE_BASENAME = "state.wal"
TARGET_WAL_NAME = "state.wal.precopy"
TARGET_MANIFEST_NAME = "state.wal.precopy.manifest.json"
MAX_CONTROL_LINE_BYTES = 4096
MAX_PREFIX_BYTES = 16 * 1024 * 1024 * 1024
DEFAULT_CHUNK_BYTES = 1024 * 1024
DEFAULT_RATE_BYTES_PER_SEC = 10 * 1024 * 1024
MAX_CLI_RATE_MIB_S = 200
DEFAULT_DEADLINE_S = 3600.0
DEFAULT_MIN_RESERVE_BYTES = 3 * 1024 * 1024 * 1024
DEFAULT_RESERVE_CHECK_BYTES = 64 * 1024 * 1024

HEADER_KEYS = {
    "schema",
    "protocol_version",
    "prefix_bytes",
    "source_dev",
    "source_ino",
    "source_nlink",
    "source_mtime_ns",
    "source_ctime_ns",
    "chunk_bytes",
    "status",
}
FOOTER_KEYS = {
    "schema",
    "protocol_version",
    "bytes_sent",
    "sha256",
    "source_end_size",
    "source_dev",
    "source_ino",
    "source_nlink",
    "source_end_mtime_ns",
    "source_end_ctime_ns",
    "status",
}


class PrecopyError(RuntimeError):
    pass


def default_free_space_bytes(path: Path) -> int:
    vfs = os.statvfs(path)
    return int(vfs.f_bavail * vfs.f_frsize)


def check_deadline(deadline: float, clock: Callable[[], float], phase: str) -> float:
    now = clock()
    if now > deadline:
        raise PrecopyError(f"deadline exceeded during {phase}")
    return now


def require_int(obj: dict[str, Any], key: str, lo: int, hi: int | None = None) -> int:
    val = obj[key]
    if type(val) is not int or val < lo or (hi is not None and val > hi):
        raise PrecopyError(f"invalid integer field {key}")
    return val


def is_lower_sha256(val: Any) -> bool:
    return isinstance(val, str) and len(val) == 64 and all(c in "0123456789abcdef" for c in val)


def parse_strict_json_line(raw: bytes, label: str, required_keys: set[str]) -> dict[str, Any]:
    if not raw or not raw.endswith(b"\n"):
        raise PrecopyError(f"{label} missing terminating newline")
    if len(raw) > MAX_CONTROL_LINE_BYTES:
        raise PrecopyError(f"{label} exceeds {MAX_CONTROL_LINE_BYTES} bytes")

    def no_dups(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        out: dict[str, Any] = {}
        for k, v in pairs:
            if k in out:
                raise PrecopyError(f"{label} has duplicate key {k!r}")
            out[k] = v
        return out

    try:
        obj = json.loads(raw.decode("utf-8"), object_pairs_hook=no_dups)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise PrecopyError(f"invalid {label} JSON: {exc}") from exc
    if not isinstance(obj, dict) or set(obj.keys()) != required_keys:
        raise PrecopyError(f"{label} keys do not match exact schema")
    return obj


def read_bounded_line(
    stream: BinaryIO, deadline: float, clock: Callable[[], float], label: str
) -> bytes:
    check_deadline(deadline, clock, label)
    raw = stream.readline(MAX_CONTROL_LINE_BYTES + 1)
    check_deadline(deadline, clock, label)
    if not raw:
        raise PrecopyError(f"missing {label}")
    if len(raw) > MAX_CONTROL_LINE_BYTES or not raw.endswith(b"\n"):
        raise PrecopyError(f"{label} exceeded {MAX_CONTROL_LINE_BYTES} bytes or lacked newline")
    return raw


def encode_control_line(payload: dict[str, Any], label: str) -> bytes:
    line = (json.dumps(payload, separators=(",", ":"), sort_keys=True) + "\n").encode("utf-8")
    if len(line) > MAX_CONTROL_LINE_BYTES:
        raise PrecopyError(f"{label} exceeds {MAX_CONTROL_LINE_BYTES} bytes")
    return line


def validate_source_wal_path(source_wal: Path) -> os.stat_result:
    if not source_wal.is_absolute() or ".." in source_wal.parts:
        raise PrecopyError("source path must be absolute without parent traversal")
    if source_wal.name != SOURCE_BASENAME:
        raise PrecopyError(f"source basename must be exactly {SOURCE_BASENAME!r}")
    require_no_symlink_ancestors(source_wal)
    try:
        pst = os.lstat(source_wal.parent)
        st = os.lstat(source_wal)
    except OSError as exc:
        raise PrecopyError(f"cannot stat source WAL: {exc}") from exc
    if not stat.S_ISDIR(pst.st_mode) or stat.S_ISLNK(pst.st_mode):
        raise PrecopyError("source parent must be a real non-symlink directory")
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISREG(st.st_mode):
        raise PrecopyError("source WAL must be a regular non-symlink file")
    if st.st_nlink != 1:
        raise PrecopyError("source WAL must have exactly one hard link")
    if st.st_size <= 0 or st.st_size > MAX_PREFIX_BYTES:
        raise PrecopyError(f"source WAL size {st.st_size} outside 1..={MAX_PREFIX_BYTES}")
    return st


def require_no_symlink_ancestors(path: Path) -> None:
    for parent in reversed(path.parents):
        st = parent.lstat()
        if not stat.S_ISDIR(st.st_mode) or stat.S_ISLNK(st.st_mode):
            raise PrecopyError(f"path ancestor must be a real non-symlink directory: {parent}")


def validate_deadline_seconds(deadline_s: float) -> None:
    if not math.isfinite(deadline_s) or not 0 < deadline_s <= DEFAULT_DEADLINE_S:
        raise PrecopyError("deadline must be finite and within (0, 3600] seconds")


def validate_candidate_dir_new(candidate_dir: Path) -> Path:
    if not candidate_dir.is_absolute() or ".." in candidate_dir.parts:
        raise PrecopyError("candidate dir must be absolute without parent traversal")
    name = candidate_dir.name
    if not name.startswith(CANDIDATE_PREFIX) or len(name) <= len(CANDIDATE_PREFIX):
        raise PrecopyError(f"candidate dir basename must start with {CANDIDATE_PREFIX!r}")
    parent = candidate_dir.parent
    require_no_symlink_ancestors(candidate_dir)
    try:
        pst = os.lstat(parent)
    except OSError as exc:
        raise PrecopyError(f"candidate parent missing: {exc}") from exc
    if stat.S_ISLNK(pst.st_mode) or not stat.S_ISDIR(pst.st_mode):
        raise PrecopyError("candidate parent must be a real non-symlink directory")
    if os.path.lexists(candidate_dir):
        raise PrecopyError("candidate dir already exists")
    return parent


def write_all(fd: int, data: bytes) -> None:
    view = memoryview(data)
    written = 0
    while written < len(view):
        n = os.write(fd, view[written:])
        if n <= 0:
            raise PrecopyError("short write on target file")
        written += n


def fsync_dir(path: Path) -> None:
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    fd = os.open(path, flags)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def stream_source(
    source_wal: Path,
    out_stream: BinaryIO,
    *,
    chunk_bytes: int = DEFAULT_CHUNK_BYTES,
    rate_bytes_per_sec: int = DEFAULT_RATE_BYTES_PER_SEC,
    deadline_s: float = DEFAULT_DEADLINE_S,
    clock: Callable[[], float] = time.monotonic,
    sleeper: Callable[[float], None] = time.sleep,
    on_after_first_chunk: Callable[[], None] | None = None,
) -> dict[str, Any]:
    if chunk_bytes <= 0 or chunk_bytes > DEFAULT_CHUNK_BYTES:
        raise PrecopyError("invalid chunk_bytes")
    if rate_bytes_per_sec < 0:
        raise PrecopyError("invalid throttle or deadline")
    validate_deadline_seconds(deadline_s)
    st_path_start = validate_source_wal_path(source_wal)
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    try:
        fd = os.open(source_wal, flags)
    except OSError as exc:
        raise PrecopyError(f"failed to open source WAL: {exc}") from exc
    try:
        st_fd_start = os.fstat(fd)
        if (
            not stat.S_ISREG(st_fd_start.st_mode)
            or st_fd_start.st_nlink != 1
            or (st_fd_start.st_dev, st_fd_start.st_ino) != (st_path_start.st_dev, st_path_start.st_ino)
        ):
            raise PrecopyError("source WAL identity changed during open")
        prefix_bytes = st_fd_start.st_size
        if prefix_bytes <= 0 or prefix_bytes > MAX_PREFIX_BYTES:
            raise PrecopyError("source WAL size out of bounds")
        t0 = clock()
        deadline = t0 + deadline_s
        header = {
            "schema": HEADER_SCHEMA,
            "protocol_version": PROTOCOL_VERSION,
            "prefix_bytes": prefix_bytes,
            "source_dev": st_fd_start.st_dev,
            "source_ino": st_fd_start.st_ino,
            "source_nlink": st_fd_start.st_nlink,
            "source_mtime_ns": st_fd_start.st_mtime_ns,
            "source_ctime_ns": st_fd_start.st_ctime_ns,
            "chunk_bytes": chunk_bytes,
            "status": STATUS_PRECOPY_ONLY,
        }
        out_stream.write(encode_control_line(header, "header"))
        out_stream.flush()
        hasher = hashlib.sha256()
        bytes_sent = 0
        hook_ran = False
        while bytes_sent < prefix_bytes:
            check_deadline(deadline, clock, "source stream")
            to_read = min(chunk_bytes, prefix_bytes - bytes_sent)
            chunk = os.read(fd, to_read)
            if not chunk:
                raise PrecopyError("source WAL truncated mid-stream")
            hasher.update(chunk)
            out_stream.write(chunk)
            bytes_sent += len(chunk)
            if on_after_first_chunk is not None and not hook_ran:
                hook_ran = True
                on_after_first_chunk()
            if rate_bytes_per_sec > 0 and bytes_sent < prefix_bytes:
                now = check_deadline(deadline, clock, "source throttle")
                delay = (bytes_sent / rate_bytes_per_sec) - (now - t0)
                if delay > 0:
                    if now + delay > deadline:
                        raise PrecopyError("deadline exceeded by throttle wait")
                    sleeper(delay)
        st_fd_end = os.fstat(fd)
        try:
            st_path_end = os.lstat(source_wal)
        except OSError as exc:
            raise PrecopyError(f"source WAL disappeared before completion: {exc}") from exc
        if (
            stat.S_ISLNK(st_path_end.st_mode)
            or not stat.S_ISREG(st_path_end.st_mode)
            or not stat.S_ISREG(st_fd_end.st_mode)
            or st_fd_end.st_nlink != 1
            or st_path_end.st_nlink != 1
            or (st_fd_end.st_dev, st_fd_end.st_ino) != (st_fd_start.st_dev, st_fd_start.st_ino)
            or (st_path_end.st_dev, st_path_end.st_ino) != (st_fd_start.st_dev, st_fd_start.st_ino)
        ):
            raise PrecopyError("source WAL replaced or relinked during precopy")
        if st_fd_end.st_size < prefix_bytes or st_path_end.st_size < prefix_bytes:
            raise PrecopyError("source WAL shrank below prefix_bytes during precopy")
        footer = {
            "schema": FOOTER_SCHEMA,
            "protocol_version": PROTOCOL_VERSION,
            "bytes_sent": bytes_sent,
            "sha256": hasher.hexdigest(),
            "source_end_size": st_fd_end.st_size,
            "source_dev": st_fd_end.st_dev,
            "source_ino": st_fd_end.st_ino,
            "source_nlink": st_fd_end.st_nlink,
            "source_end_mtime_ns": st_fd_end.st_mtime_ns,
            "source_end_ctime_ns": st_fd_end.st_ctime_ns,
            "status": STATUS_PRECOPY_ONLY,
        }
        out_stream.write(encode_control_line(footer, "footer"))
        out_stream.flush()
        return footer
    finally:
        os.close(fd)


def receive_stream(
    candidate_dir: Path,
    in_stream: BinaryIO,
    *,
    min_reserve_bytes: int = DEFAULT_MIN_RESERVE_BYTES,
    reserve_check_bytes: int = DEFAULT_RESERVE_CHECK_BYTES,
    deadline_s: float = DEFAULT_DEADLINE_S,
    clock: Callable[[], float] = time.monotonic,
    free_space_fn: Callable[[Path], int] = default_free_space_bytes,
) -> dict[str, Any]:
    if min_reserve_bytes < 0 or reserve_check_bytes <= 0:
        raise PrecopyError("invalid receiver bounds")
    validate_deadline_seconds(deadline_s)
    parent = validate_candidate_dir_new(candidate_dir)
    deadline = clock() + deadline_s
    header = parse_strict_json_line(
        read_bounded_line(in_stream, deadline, clock, "header"), "header", HEADER_KEYS
    )
    if (
        header["schema"] != HEADER_SCHEMA
        or require_int(header, "protocol_version", 1, PROTOCOL_VERSION) != PROTOCOL_VERSION
        or header["status"] != STATUS_PRECOPY_ONLY
    ):
        raise PrecopyError("header schema, version, or status invalid")
    prefix_bytes = require_int(header, "prefix_bytes", 1, MAX_PREFIX_BYTES)
    chunk_bytes = require_int(header, "chunk_bytes", 1, DEFAULT_CHUNK_BYTES)
    source_dev = require_int(header, "source_dev", 0)
    source_ino = require_int(header, "source_ino", 0)
    require_int(header, "source_nlink", 1, 1)
    source_mtime_ns = require_int(header, "source_mtime_ns", 0)
    source_ctime_ns = require_int(header, "source_ctime_ns", 0)
    if free_space_fn(parent) < prefix_bytes + min_reserve_bytes:
        raise PrecopyError("insufficient target free space for prefix plus reserve")
    try:
        os.mkdir(candidate_dir, 0o700)
        os.chmod(candidate_dir, 0o700)
    except OSError as exc:
        raise PrecopyError(f"failed to create candidate dir exclusively: {exc}") from exc
    cst = os.lstat(candidate_dir)
    if stat.S_ISLNK(cst.st_mode) or not stat.S_ISDIR(cst.st_mode):
        raise PrecopyError("candidate dir is not a real directory")
    fsync_dir(parent)
    if free_space_fn(candidate_dir) < prefix_bytes + min_reserve_bytes:
        raise PrecopyError("insufficient candidate filesystem free space")
    wal_path = candidate_dir / TARGET_WAL_NAME
    manifest_path = candidate_dir / TARGET_MANIFEST_NAME
    create_flags = (
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    )
    hasher = hashlib.sha256()
    received = 0
    next_check = reserve_check_bytes
    wal_fd = os.open(wal_path, create_flags, 0o600)
    try:
        os.fchmod(wal_fd, 0o600)
        while received < prefix_bytes:
            check_deadline(deadline, clock, "receive payload")
            to_read = min(chunk_bytes, prefix_bytes - received)
            chunk = in_stream.read(to_read)
            if not chunk:
                raise PrecopyError("truncated payload before prefix_bytes")
            write_all(wal_fd, chunk)
            hasher.update(chunk)
            received += len(chunk)
            if received >= next_check and received < prefix_bytes:
                remaining = prefix_bytes - received
                if free_space_fn(candidate_dir) < remaining + min_reserve_bytes:
                    raise PrecopyError("target free space dropped below reserve mid-stream")
                next_check = received + reserve_check_bytes
        os.fsync(wal_fd)
        target_st = os.fstat(wal_fd)
    finally:
        os.close(wal_fd)
    if (
        not stat.S_ISREG(target_st.st_mode)
        or target_st.st_nlink != 1
        or target_st.st_size != prefix_bytes
        or (target_st.st_mode & 0o777) != 0o600
    ):
        raise PrecopyError("target precopy file failed post-write identity check")
    footer = parse_strict_json_line(
        read_bounded_line(in_stream, deadline, clock, "footer"), "footer", FOOTER_KEYS
    )
    if (
        footer["schema"] != FOOTER_SCHEMA
        or require_int(footer, "protocol_version", 1, PROTOCOL_VERSION) != PROTOCOL_VERSION
        or footer["status"] != STATUS_PRECOPY_ONLY
    ):
        raise PrecopyError("footer schema, version, or status invalid")
    if require_int(footer, "bytes_sent", 1, MAX_PREFIX_BYTES) != prefix_bytes:
        raise PrecopyError("footer bytes_sent mismatch")
    digest = hasher.hexdigest()
    if not is_lower_sha256(footer["sha256"]) or footer["sha256"] != digest:
        raise PrecopyError("footer sha256 mismatch")
    if (
        require_int(footer, "source_dev", 0) != source_dev
        or require_int(footer, "source_ino", 0) != source_ino
    ):
        raise PrecopyError("footer source dev/ino mismatch")
    require_int(footer, "source_nlink", 1, 1)
    source_end_size = require_int(footer, "source_end_size", prefix_bytes)
    source_end_mtime_ns = require_int(footer, "source_end_mtime_ns", 0)
    source_end_ctime_ns = require_int(footer, "source_end_ctime_ns", 0)
    free_after = free_space_fn(candidate_dir)
    if free_after < min_reserve_bytes:
        raise PrecopyError("target free space below reserve after payload")
    manifest = {
        "schema": MANIFEST_SCHEMA,
        "status": "PRECOPY_ONLY_UNQUALIFIED",
        "qualified_for_recovery": False,
        "source_atomicity_claimed": False,
        "wal_filename": TARGET_WAL_NAME,
        "prefix_bytes": prefix_bytes,
        "sha256": digest,
        "source_dev": source_dev,
        "source_ino": source_ino,
        "source_nlink": 1,
        "source_start_mtime_ns": source_mtime_ns,
        "source_start_ctime_ns": source_ctime_ns,
        "source_end_size": source_end_size,
        "source_end_mtime_ns": source_end_mtime_ns,
        "source_end_ctime_ns": source_end_ctime_ns,
        "target_dev": target_st.st_dev,
        "target_ino": target_st.st_ino,
        "target_free_bytes_after": free_after,
    }
    manifest_bytes = (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode("utf-8")
    partial_manifest_path = candidate_dir / (TARGET_MANIFEST_NAME + ".partial")
    m_fd = os.open(partial_manifest_path, create_flags, 0o600)
    try:
        os.fchmod(m_fd, 0o600)
        write_all(m_fd, manifest_bytes)
        os.fsync(m_fd)
    finally:
        os.close(m_fd)
    os.link(partial_manifest_path, manifest_path, follow_symlinks=False)
    partial_manifest_path.unlink()
    fsync_dir(candidate_dir)
    return manifest


def run_dry_run(
    *,
    source_wal: Path | None = None,
    candidate_dir: Path | None = None,
    expected_bytes: int | None = None,
    min_reserve_bytes: int = DEFAULT_MIN_RESERVE_BYTES,
    free_space_fn: Callable[[Path], int] = default_free_space_bytes,
) -> dict[str, Any]:
    if source_wal is None and candidate_dir is None:
        raise PrecopyError("dry-run requires --source-wal and/or --candidate-dir")
    report: dict[str, Any] = {"status": "DRY_RUN_METADATA_ONLY", "qualified_for_recovery": False}
    if source_wal is not None:
        st = validate_source_wal_path(source_wal)
        report["source"] = {
            "dev": st.st_dev,
            "ino": st.st_ino,
            "nlink": st.st_nlink,
            "size": st.st_size,
            "free_bytes": free_space_fn(source_wal.parent),
        }
        if expected_bytes is None:
            expected_bytes = st.st_size
    if candidate_dir is not None:
        parent = validate_candidate_dir_new(candidate_dir)
        free_bytes = free_space_fn(parent)
        if expected_bytes is not None:
            if expected_bytes <= 0 or expected_bytes > MAX_PREFIX_BYTES:
                raise PrecopyError("expected_bytes out of bounds")
            if free_bytes < expected_bytes + min_reserve_bytes:
                raise PrecopyError("insufficient target free space for expected_bytes plus reserve")
        report["target"] = {
            "parent": str(parent),
            "candidate_name": candidate_dir.name,
            "free_bytes": free_bytes,
            "min_reserve_bytes": min_reserve_bytes,
            "expected_bytes": expected_bytes,
        }
    return report


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="wal_precopy.py")
    sub = parser.add_subparsers(dest="cmd", required=True)
    p_src = sub.add_parser("source")
    p_src.add_argument("--source-wal", required=True, type=Path)
    p_src.add_argument("--rate-mib-s", type=int, default=10)
    p_src.add_argument("--deadline-s", type=float, default=DEFAULT_DEADLINE_S)
    p_rcv = sub.add_parser("receive")
    p_rcv.add_argument("--candidate-dir", required=True, type=Path)
    p_rcv.add_argument("--min-reserve-gib", type=int, default=3)
    p_rcv.add_argument("--deadline-s", type=float, default=DEFAULT_DEADLINE_S)
    p_dry = sub.add_parser("dry-run")
    p_dry.add_argument("--source-wal", type=Path)
    p_dry.add_argument("--candidate-dir", type=Path)
    p_dry.add_argument("--expected-bytes", type=int)
    p_dry.add_argument("--min-reserve-gib", type=int, default=3)
    args = parser.parse_args(argv)
    try:
        if args.cmd == "source":
            if not (1 <= args.rate_mib_s <= MAX_CLI_RATE_MIB_S):
                raise PrecopyError(f"rate-mib-s must be within 1..={MAX_CLI_RATE_MIB_S}")
            summary = stream_source(
                args.source_wal,
                sys.stdout.buffer,
                rate_bytes_per_sec=args.rate_mib_s * 1024 * 1024,
                deadline_s=args.deadline_s,
            )
            sys.stderr.write(json.dumps(summary, sort_keys=True) + "\n")
            return 0
        if args.cmd == "receive":
            if args.min_reserve_gib < 1:
                raise PrecopyError("min-reserve-gib must be >= 1")
            manifest = receive_stream(
                args.candidate_dir,
                sys.stdin.buffer,
                min_reserve_bytes=args.min_reserve_gib * 1024 * 1024 * 1024,
                deadline_s=args.deadline_s,
            )
            sys.stdout.write(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
            return 0
        if args.min_reserve_gib < 1:
            raise PrecopyError("min-reserve-gib must be >= 1")
        report = run_dry_run(
            source_wal=args.source_wal,
            candidate_dir=args.candidate_dir,
            expected_bytes=args.expected_bytes,
            min_reserve_bytes=args.min_reserve_gib * 1024 * 1024 * 1024,
        )
        sys.stdout.write(json.dumps(report, indent=2, sort_keys=True) + "\n")
        return 0
    except (PrecopyError, OSError) as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
