#!/usr/bin/env python3
"""Seal and verify the final public height of the six legacy ARC validators.

The sampler is intentionally independent of the rollout RPC client.  It talks
only to the fixed, reviewed legacy HTTP origins and writes a private,
create-only canonical receipt.  The live capture orchestrator must verify the
receipt within five minutes before it seals the authenticated cross-proof;
post-stop builders validate that immutable historical decision intrinsically.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime as dt
import hashlib
import json
import os
import re
import stat
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Callable, Mapping, NoReturn, Sequence

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))
from recovery_freeze import FreezeValidationError, validate_pinned_freeze_plan
from quarantine_rounds import (
    FLEET_MAP as ROUND_FLEET_MAP,
    TARGET_HEIGHT_SCHEMA,
    QuarantineRoundError,
    validate_target_height_receipt,
)


SCHEMA = "arc.recovery.legacy-public-height.v1"
FREEZE_PLAN_SCHEMA = "arc.recovery.freeze-plan.v5"
MAX_BODY_BYTES = 1024 * 1024
MAX_RECEIPT_AGE_SECONDS = 300
FLEET = (
    ("nyc", "149.28.32.76", "http://149.28.32.76:9090"),
    ("lax", "140.82.16.112", "http://140.82.16.112:9090"),
    ("ams", "136.244.109.1", "http://136.244.109.1:9090"),
    ("lhr", "104.238.171.11", "http://104.238.171.11:9090"),
    ("nrt", "202.182.107.41", "http://202.182.107.41:9090"),
    ("sgp", "149.28.153.31", "http://149.28.153.31:9090"),
)
HASH_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$")
LEGACY_VERSION_RE = re.compile(r"^0\.7\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$")
UTC_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")


class HeightReceiptError(RuntimeError):
    """Fail-closed input, network, or receipt error."""


def fail(message: str) -> NoReturn:
    raise HeightReceiptError(message)


def canonical_bytes(value: Mapping[str, Any]) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def utc_seconds_now() -> str:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def parse_utc(value: Any, field: str) -> dt.datetime:
    if not isinstance(value, str) or not UTC_RE.fullmatch(value):
        fail(f"{field} must be canonical UTC seconds")
    try:
        parsed = dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=dt.timezone.utc)
    except ValueError as error:
        fail(f"{field} is invalid: {error}")
    return parsed


def require_hash(value: Any, field: str) -> str:
    if not isinstance(value, str) or not HASH_RE.fullmatch(value):
        fail(f"{field} must be 64 lowercase hexadecimal characters")
    return value


def require_commit(value: Any, field: str) -> str:
    if not isinstance(value, str) or not COMMIT_RE.fullmatch(value):
        fail(f"{field} must be a canonical 40- or 64-character lowercase Git object id")
    return value


def require_uint(value: Any, field: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        fail(f"{field} must be a non-negative integer")
    return value


def open_parent_directory(path: Path) -> tuple[int, str]:
    if not path.is_absolute() or path.name in {"", ".", ".."}:
        fail(f"path must be absolute with a file name: {path}")
    if any(part in {".", ".."} for part in path.parts[1:]):
        fail(f"path traversal is forbidden: {path}")
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open("/", flags)
    try:
        for component in path.parent.parts[1:]:
            next_descriptor = os.open(component, flags, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = next_descriptor
        return descriptor, path.name
    except Exception:
        os.close(descriptor)
        raise


def read_locked(path: Path, *, expected_mode: int | None = None, limit: int = 16 * 1024 * 1024) -> bytes:
    parent_fd, name = open_parent_directory(path)
    descriptor = -1
    try:
        descriptor = os.open(name, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=parent_fd)
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_mode & 0o222:
            fail(f"sealed input is mutable or non-regular: {path}")
        if expected_mode is not None and stat.S_IMODE(before.st_mode) != expected_mode:
            fail(f"sealed input mode must be {expected_mode:04o}: {path}")
        chunks: list[bytes] = []
        size = 0
        while True:
            chunk = os.read(descriptor, min(1024 * 1024, limit + 1 - size))
            if not chunk:
                break
            chunks.append(chunk)
            size += len(chunk)
            if size > limit:
                fail(f"sealed input exceeds {limit} bytes: {path}")
        after = os.fstat(descriptor)
        identity = lambda value: (value.st_dev, value.st_ino, value.st_size, value.st_mtime_ns)
        if identity(before) != identity(after):
            fail(f"sealed input changed while read: {path}")
        return b"".join(chunks)
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        os.close(parent_fd)


def load_freeze_plan(path: Path, expected_sha256: str, source_main: str) -> Mapping[str, Any]:
    require_hash(expected_sha256, "freeze plan sha256")
    require_commit(source_main, "source main commit")
    payload = read_locked(path)
    sidecar = read_locked(path.with_name(path.name + ".sha256"))
    actual = sha256_bytes(payload)
    if actual != expected_sha256:
        fail("freeze plan bytes differ from the explicitly approved sha256")
    if sidecar != f"{actual}  {path.name}\n".encode("ascii"):
        fail("freeze plan sidecar does not bind the exact plan bytes")
    try:
        value = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"freeze plan is invalid JSON: {error}")
    if not isinstance(value, dict) or canonical_bytes(value) != payload:
        fail("freeze plan must be canonical JSON")
    try:
        validate_pinned_freeze_plan(payload, expected_sha256)
    except FreezeValidationError as error:
        fail(f"freeze plan failed complete v5 validation: {error}")
    if value.get("schema") != FREEZE_PLAN_SCHEMA:
        fail("freeze plan schema is unsupported")
    if value.get("source_commit") != source_main:
        fail("freeze plan is not bound to the exact protected-main commit")
    nodes = value.get("nodes")
    if not isinstance(nodes, list) or len(nodes) != len(FLEET):
        fail("freeze plan must contain exactly the reviewed six validators")
    observed = [(row.get("name"), row.get("host")) for row in nodes if isinstance(row, dict)]
    expected = [(name, host) for name, host, _origin in FLEET]
    if observed != expected:
        fail("freeze plan validator order/topology differs from the reviewed fleet")
    if len(set(observed)) != len(FLEET):
        fail("freeze plan contains duplicate validators")
    return value


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # type: ignore[no-untyped-def]
        return None


OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect)


def request_json(origin: str, path: str, timeout: float) -> tuple[Any, str]:
    request = urllib.request.Request(
        f"{origin}{path}",
        headers={"Accept": "application/json", "User-Agent": "arc-legacy-height/1"},
        method="GET",
    )
    try:
        with OPENER.open(request, timeout=timeout) as response:
            if response.status != 200:
                fail(f"{origin}{path} returned HTTP {response.status}")
            content_type = response.headers.get_content_type()
            if content_type != "application/json":
                fail(f"{origin}{path} returned content type {content_type!r}")
            raw = response.read(MAX_BODY_BYTES + 1)
    except HeightReceiptError:
        raise
    except urllib.error.HTTPError as error:
        code = error.code
        error.close()
        fail(f"{origin}{path} returned HTTP {code}; redirects are forbidden")
    except (TimeoutError, OSError, urllib.error.URLError) as error:
        fail(f"{origin}{path} failed: {error}")
    if len(raw) > MAX_BODY_BYTES:
        fail(f"{origin}{path} response exceeded {MAX_BODY_BYTES} bytes")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"{origin}{path} returned invalid JSON: {error}")
    return value, sha256_bytes(raw)


Fetch = Callable[[str, str, float], tuple[Any, str]]


def sample_origin(name: str, origin: str, timeout: float, fetch: Fetch = request_json) -> dict[str, Any]:
    before, before_sha = fetch(origin, "/info", timeout)
    latest, latest_sha = fetch(origin, "/block/latest", timeout)
    after, after_sha = fetch(origin, "/info", timeout)
    if not isinstance(before, dict) or not isinstance(after, dict):
        fail(f"{name} /info response must be an object")
    for label, value in (("before", before), ("after", after)):
        if value.get("chain") != "ARC Chain":
            fail(f"{name} {label} /info is not ARC Chain")
        version = value.get("version")
        if not isinstance(version, str) or not LEGACY_VERSION_RE.fullmatch(version):
            fail(f"{name} {label} /info is not a v0.7 legacy node")
    before_height = require_uint(before.get("block_height"), f"{name} before height")
    after_height = require_uint(after.get("block_height"), f"{name} after height")
    if not isinstance(latest, dict) or not isinstance(latest.get("header"), dict):
        fail(f"{name} latest block has no header")
    header = latest["header"]
    latest_height = require_uint(header.get("height", latest.get("height")), f"{name} latest height")
    block_hash = latest.get("hash", header.get("hash"))
    require_hash(block_hash, f"{name} latest block hash")
    if after_height < before_height:
        fail(f"{name} height moved backwards during sampling")
    if not before_height <= latest_height <= after_height:
        fail(f"{name} latest block height is outside the two /info observations")
    return {
        "name": name,
        "origin": origin,
        "info_before_height": before_height,
        "latest_block_height": latest_height,
        "info_after_height": after_height,
        "latest_block_hash": block_hash,
        "info_before_body_sha256": require_hash(before_sha, f"{name} before body sha256"),
        "latest_block_body_sha256": require_hash(latest_sha, f"{name} block body sha256"),
        "info_after_body_sha256": require_hash(after_sha, f"{name} after body sha256"),
    }


def capture_id(freeze_plan_sha256: str) -> str:
    return hashlib.sha256(b"ARC recovery capture v2\0" + bytes.fromhex(freeze_plan_sha256)).hexdigest()


def build_receipt(source_main: str, freeze_sha: str, timeout: float) -> dict[str, Any]:
    if not (0 < timeout <= 30):
        fail("timeout must be greater than zero and at most 30 seconds")
    started_at = utc_seconds_now()
    started = time.monotonic_ns()
    rows_by_name: dict[str, dict[str, Any]] = {}
    errors: list[str] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(FLEET)) as executor:
        pending = {
            executor.submit(sample_origin, name, origin, timeout): name
            for name, _host, origin in FLEET
        }
        for future in concurrent.futures.as_completed(pending):
            name = pending[future]
            try:
                rows_by_name[name] = future.result()
            except Exception as error:  # aggregate all six diagnostics without accepting a partial sample
                errors.append(f"{name}: {error}")
    if errors:
        fail("legacy public-height sample failed: " + "; ".join(sorted(errors)))
    rows = [rows_by_name[name] for name, _host, _origin in FLEET]
    completed_at = utc_seconds_now()
    duration_ms = (time.monotonic_ns() - started) // 1_000_000
    return {
        "schema": SCHEMA,
        "source_main_commit": source_main,
        "freeze_plan_sha256": freeze_sha,
        "capture_id": capture_id(freeze_sha),
        "started_at": started_at,
        "completed_at": completed_at,
        "duration_ms": duration_ms,
        "request_policy": {
            "redirects": "forbidden",
            "maximum_body_bytes": MAX_BODY_BYTES,
            "timeout_seconds": timeout,
            "proxy_environment": "ignored",
            "sequence": ["/info", "/block/latest", "/info"],
        },
        "origins": rows,
        "legacy_public_max_height": max(row["info_after_height"] for row in rows),
    }


def parse_targets(raw: str) -> tuple[str, ...]:
    requested = raw.split(",") if raw else []
    expected_order = [name for name, _host, _origin in FLEET if name in set(requested)]
    if (not requested or requested != expected_order or len(requested) != len(set(requested))
            or any(name not in ROUND_FLEET_MAP for name in requested)):
        fail("targets must be a non-empty, comma-separated subset in fixed fleet order")
    return tuple(requested)


def build_target_receipt(
    source_main: str, freeze_sha: str, timeout: float, targets: Sequence[str]
) -> dict[str, Any]:
    if not (0 < timeout <= 30):
        fail("timeout must be greater than zero and at most 30 seconds")
    selected = [(name, host, origin) for name, host, origin in FLEET if name in set(targets)]
    if [name for name, _host, _origin in selected] != list(targets):
        fail("target public-height fleet subset differs")
    started_at = utc_seconds_now();started = time.monotonic_ns()
    rows_by_name: dict[str, dict[str, Any]] = {};errors: list[str] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(selected)) as executor:
        pending = {
            executor.submit(sample_origin, name, origin, timeout): name
            for name, _host, origin in selected
        }
        for future in concurrent.futures.as_completed(pending):
            name = pending[future]
            try: rows_by_name[name] = future.result()
            except Exception as error: errors.append(f"{name}: {error}")
    if errors:
        fail("target legacy public-height sample failed: " + "; ".join(sorted(errors)))
    rows = [rows_by_name[name] for name in targets]
    return {
        "schema": TARGET_HEIGHT_SCHEMA, "source_main_commit": source_main,
        "freeze_plan_sha256": freeze_sha, "capture_id": capture_id(freeze_sha),
        "started_at": started_at, "completed_at": utc_seconds_now(),
        "duration_ms": (time.monotonic_ns() - started) // 1_000_000,
        "request_policy": {
            "redirects": "forbidden", "maximum_body_bytes": MAX_BODY_BYTES,
            "timeout_seconds": timeout, "proxy_environment": "ignored",
            "sequence": ["/info", "/block/latest", "/info"],
        },
        "targets": [
            {"node": name, "host": host, "rpc_origin": origin}
            for name, host, origin in selected
        ],
        "origins": rows,
        "legacy_public_max_height": max(row["info_after_height"] for row in rows),
    }


def validate_target_receipt_live(
    value: Any, *, source_main: str, freeze_sha: str, targets: Sequence[str],
    now: dt.datetime | None = None, max_age_seconds: int = MAX_RECEIPT_AGE_SECONDS,
) -> int:
    if not isinstance(value, dict) or value.get("source_main_commit") != require_commit(
        source_main, "source main commit"
    ):
        fail("target height receipt source binding differs")
    try:
        _started, completed, maximum = validate_target_height_receipt(
            value, capture_id=capture_id(freeze_sha), freeze_sha256=freeze_sha,
            targets=targets,
        )
    except QuarantineRoundError as error:
        fail(str(error))
    if (isinstance(max_age_seconds, bool) or not isinstance(max_age_seconds, int)
            or not 1 <= max_age_seconds <= MAX_RECEIPT_AGE_SECONDS):
        fail("maximum receipt age must be between 1 and 300 seconds")
    age = ((now or dt.datetime.now(dt.timezone.utc)) - completed).total_seconds()
    if age < -5: fail("target height receipt completion time is in the future")
    if age > max_age_seconds:
        fail(f"target height receipt is stale ({int(age)}s old; maximum {max_age_seconds}s)")
    return maximum


def create_private_receipt(path: Path, value: Mapping[str, Any]) -> None:
    if path.suffix != ".json":
        fail("receipt output must have a .json suffix")
    payload = canonical_bytes(value)
    parent_fd, name = open_parent_directory(path)
    descriptor = -1
    created = False
    complete = False
    try:
        descriptor = os.open(
            name,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
            0o400,
            dir_fd=parent_fd,
        )
        created = True
        offset = 0
        while offset < len(payload):
            offset += os.write(descriptor, payload[offset:])
        os.fsync(descriptor)
        os.fchmod(descriptor, 0o400)
        os.fsync(parent_fd)
        complete = True
    except FileExistsError:
        fail(f"receipt already exists; refusing replacement: {path}")
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        if created and not complete:
            try:
                os.unlink(name, dir_fd=parent_fd)
                os.fsync(parent_fd)
            except FileNotFoundError:
                pass
        os.close(parent_fd)


def validate_receipt(
    value: Any,
    *,
    source_main: str,
    freeze_sha: str,
    now: dt.datetime | None = None,
    max_age_seconds: int = MAX_RECEIPT_AGE_SECONDS,
) -> int:
    if not isinstance(value, dict) or set(value) != {
        "schema", "source_main_commit", "freeze_plan_sha256", "capture_id",
        "started_at", "completed_at", "duration_ms", "request_policy", "origins",
        "legacy_public_max_height",
    }:
        fail("height receipt has missing or unknown fields")
    if value["schema"] != SCHEMA or value["source_main_commit"] != require_commit(source_main, "source main commit"):
        fail("height receipt source binding differs")
    if value["freeze_plan_sha256"] != require_hash(freeze_sha, "freeze plan sha256"):
        fail("height receipt freeze-plan binding differs")
    if value["capture_id"] != capture_id(freeze_sha):
        fail("height receipt capture identity differs")
    started = parse_utc(value["started_at"], "started_at")
    completed = parse_utc(value["completed_at"], "completed_at")
    if completed < started:
        fail("height receipt completed before it started")
    require_uint(value["duration_ms"], "duration_ms")
    policy = value["request_policy"]
    if not isinstance(policy, dict) or policy.get("redirects") != "forbidden" or policy.get("maximum_body_bytes") != MAX_BODY_BYTES or policy.get("proxy_environment") != "ignored" or policy.get("sequence") != ["/info", "/block/latest", "/info"]:
        fail("height receipt request policy differs")
    timeout = policy.get("timeout_seconds")
    if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or not 0 < timeout <= 30:
        fail("height receipt timeout policy is invalid")
    rows = value["origins"]
    if not isinstance(rows, list) or len(rows) != len(FLEET):
        fail("height receipt must contain exactly six origins")
    expected_pairs = [(name, origin) for name, _host, origin in FLEET]
    if [(row.get("name"), row.get("origin")) for row in rows if isinstance(row, dict)] != expected_pairs:
        fail("height receipt origin order/topology differs")
    row_fields = {
        "name", "origin", "info_before_height", "latest_block_height", "info_after_height",
        "latest_block_hash", "info_before_body_sha256", "latest_block_body_sha256",
        "info_after_body_sha256",
    }
    after_heights = []
    for row in rows:
        if not isinstance(row, dict) or set(row) != row_fields:
            fail("height receipt origin has missing or unknown fields")
        before = require_uint(row["info_before_height"], f"{row['name']} before height")
        latest = require_uint(row["latest_block_height"], f"{row['name']} latest height")
        after = require_uint(row["info_after_height"], f"{row['name']} after height")
        if not before <= latest <= after:
            fail(f"{row['name']} height observations are inconsistent")
        for field in ("latest_block_hash", "info_before_body_sha256", "latest_block_body_sha256", "info_after_body_sha256"):
            require_hash(row[field], f"{row['name']} {field}")
        after_heights.append(after)
    maximum = require_uint(value["legacy_public_max_height"], "legacy_public_max_height")
    if maximum != max(after_heights):
        fail("legacy_public_max_height is not the maximum final observation")
    if isinstance(max_age_seconds, bool) or not isinstance(max_age_seconds, int) or not 1 <= max_age_seconds <= MAX_RECEIPT_AGE_SECONDS:
        fail("maximum receipt age must be between 1 and 300 seconds")
    current = now or dt.datetime.now(dt.timezone.utc)
    age = (current - completed).total_seconds()
    if age < -5:
        fail("height receipt completion time is in the future")
    if age > max_age_seconds:
        fail(f"height receipt is stale ({int(age)}s old; maximum {max_age_seconds}s)")
    return maximum


def load_and_validate_receipt(
    path: Path,
    *,
    source_main: str,
    freeze_sha: str,
    now: dt.datetime | None = None,
    max_age_seconds: int = MAX_RECEIPT_AGE_SECONDS,
) -> tuple[Mapping[str, Any], int]:
    payload = read_locked(path, expected_mode=0o400)
    try:
        value = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"height receipt is invalid JSON: {error}")
    if not isinstance(value, dict) or canonical_bytes(value) != payload:
        fail("height receipt must be canonical JSON")
    maximum = validate_receipt(
        value,
        source_main=source_main,
        freeze_sha=freeze_sha,
        now=now,
        max_age_seconds=max_age_seconds,
    )
    return value, maximum


def load_and_validate_target_receipt(
    path: Path, *, source_main: str, freeze_sha: str, targets: Sequence[str],
    now: dt.datetime | None = None, max_age_seconds: int = MAX_RECEIPT_AGE_SECONDS,
) -> tuple[Mapping[str, Any], int]:
    payload = read_locked(path, expected_mode=0o400)
    try:value = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"target height receipt is invalid JSON: {error}")
    if not isinstance(value, dict) or canonical_bytes(value) != payload:
        fail("target height receipt must be canonical JSON")
    maximum = validate_target_receipt_live(
        value, source_main=source_main, freeze_sha=freeze_sha, targets=targets,
        now=now, max_age_seconds=max_age_seconds,
    )
    return value, maximum


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)
    for name in ("sample", "verify", "sample-targets", "verify-targets"):
        command = commands.add_parser(name)
        command.add_argument("--source-main", required=True)
        command.add_argument("--freeze-plan", required=True, type=Path)
        command.add_argument("--freeze-plan-sha256", required=True)
        if name in {"sample", "sample-targets"}:
            command.add_argument("--output", required=True, type=Path)
            command.add_argument("--timeout-seconds", type=float, default=10.0)
        else:
            command.add_argument("--receipt", required=True, type=Path)
            command.add_argument("--max-age-seconds", type=int, default=MAX_RECEIPT_AGE_SECONDS)
        if name in {"sample-targets", "verify-targets"}:
            command.add_argument("--targets", required=True)
    return root


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        load_freeze_plan(args.freeze_plan, args.freeze_plan_sha256, args.source_main)
        if args.command == "sample":
            receipt = build_receipt(args.source_main, args.freeze_plan_sha256, args.timeout_seconds)
            validate_receipt(receipt, source_main=args.source_main, freeze_sha=args.freeze_plan_sha256)
            create_private_receipt(args.output, receipt)
            print(json.dumps({"receipt_sha256": sha256_bytes(canonical_bytes(receipt)), "legacy_public_max_height": receipt["legacy_public_max_height"]}, sort_keys=True, separators=(",", ":")))
        elif args.command == "verify":
            value, maximum = load_and_validate_receipt(args.receipt, source_main=args.source_main, freeze_sha=args.freeze_plan_sha256, max_age_seconds=args.max_age_seconds)
            print(json.dumps({"receipt_sha256": sha256_bytes(canonical_bytes(value)), "legacy_public_max_height": maximum}, sort_keys=True, separators=(",", ":")))
        elif args.command == "sample-targets":
            targets = parse_targets(args.targets)
            receipt = build_target_receipt(
                args.source_main, args.freeze_plan_sha256, args.timeout_seconds, targets
            )
            validate_target_receipt_live(
                receipt, source_main=args.source_main, freeze_sha=args.freeze_plan_sha256,
                targets=targets,
            )
            create_private_receipt(args.output, receipt)
            print(json.dumps({"receipt_sha256": sha256_bytes(canonical_bytes(receipt)),
                              "legacy_public_max_height": receipt["legacy_public_max_height"],
                              "targets": list(targets)}, sort_keys=True, separators=(",", ":")))
        else:
            targets = parse_targets(args.targets)
            value, maximum = load_and_validate_target_receipt(
                args.receipt, source_main=args.source_main, freeze_sha=args.freeze_plan_sha256,
                targets=targets, max_age_seconds=args.max_age_seconds,
            )
            print(json.dumps({"receipt_sha256": sha256_bytes(canonical_bytes(value)),
                              "legacy_public_max_height": maximum, "targets": list(targets)},
                             sort_keys=True, separators=(",", ":")))
        return 0
    except (HeightReceiptError, OSError) as error:
        print(f"legacy public height: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
