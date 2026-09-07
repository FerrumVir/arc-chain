#!/usr/bin/env python3
"""Run a fail-closed live product gate through the published Linux AppImage.

This is deliberately separate from the browser-preview gate.  It launches the
exact public AppImage through WebKitGTK's W3C driver, clicks the real Tauri UI,
and therefore crosses the production JavaScript -> Tauri IPC -> Rust command
boundary.  The host runner creates a disposable, mount-free x86_64 Lima VM;
production HTTPS is reachable only through a run-scoped authenticated CONNECT
relay backed by a host-key-pinned SSH forward to LAX.

The claim is intentionally narrow: Linux x86_64 AppImage/WebKitGTK packaged UI
coverage.  It is not evidence for macOS .app/WKWebView or Windows WebView2.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import datetime as dt
import hashlib
import hmac
import http.client
import importlib.util
import json
from decimal import Decimal, InvalidOperation
import os
import platform
import re
import selectors
import shutil
import socket
import socketserver
import ssl
import stat
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path, PurePosixPath
from typing import Any, Callable, Iterable, Mapping, Sequence
from urllib.parse import quote


SCHEMA = "arc.packaged-appimage-live-product.v1"
HOST_SCHEMA = "arc.packaged-appimage-live-host.v1"
RELEASE_SCHEMA = "arc.published-release-binding.v1"
REPOSITORY = "FerrumVir/arc-chain"
TAG = "v0.8.0"
LAX_HOST = "140.82.16.112"
LAX_PORT = 443
LAX_USER = "root"
LIMA_VERSION = "2.1.1"
LIMA_IMAGE_URL = (
    "https://cloud-images.ubuntu.com/releases/noble/release-20260321/"
    "ubuntu-24.04-server-cloudimg-amd64.img"
)
LIMA_IMAGE_DIGEST = (
    "sha256:5c3ddb00f60bc455dac0862fabe9d8bacec46c33ac1751143c5c3683404b110d"
)
APT_SNAPSHOT = "20260321T235959Z"
GUEST_HOST_ALIAS = "192.168.5.2"
APPIMAGE_NAME = "arc-desktop-linux-x86_64.AppImage"
APPIMAGE_SIGNATURE_NAME = f"{APPIMAGE_NAME}.sig"
NODE_NAME = "arc-node-linux-x86_64"
EXPECTED_APP_BINARY = "arc-desktop"
RETAINED_EARNINGS_SOURCE = "scan of this node's in-memory full_transactions map"
INFERENCE_WAIT_SECONDS = 3960
RECEIPT_UI_WAIT_SECONDS = 210
# The outer limit includes AppImage extraction/onboarding, the protocol-sized
# inference window, receipt inclusion, bounded independent reads, and cleanup.
# It is deliberately larger than the sum of the two live network waits rather
# than accidentally treating the inference deadline as the whole-gate budget.
GUEST_GATE_TIMEOUT_SECONDS = 6000
# `guest-run` enforces the sealed 6,000-second product budget itself. The
# enclosing Lima command needs a small, explicit grace period so it cannot
# kill the guest at the exact instant the guest is persisting its terminal
# evidence and performing secret/profile cleanup.
HOST_GUEST_COMMAND_TIMEOUT_SECONDS = GUEST_GATE_TIMEOUT_SECONDS + 120
MAX_JSON_BYTES = 16 * 1024 * 1024
MAX_HTTP_HEADER_BYTES = 16 * 1024
MAX_RECEIPT_BYTES = 512 * 1024
MAX_PACKAGE_TREE_ENTRIES = 100_000
MAX_PACKAGE_TREE_TOTAL_BYTES = 4 * 1024 * 1024 * 1024
MAX_PACKAGE_FILE_BYTES = 1024 * 1024 * 1024
MAX_PACKAGE_PATH_BYTES = 4096
MAX_PACKAGE_DEPTH = 64
HASH_RE = re.compile(r"[0-9a-f]{64}")
SHA_RE = re.compile(r"[0-9a-f]{40}")
CANONICAL_HASH_RE = re.compile(r"0x[0-9a-f]{64}")
VM_NAME_RE = re.compile(r"arc-packaged-live-v080-[0-9a-f]{8}-[a-z0-9]{6}")

# Independent host-side BLAKE3 verifier for the sealed prompt.  Python's
# standard library has no BLAKE3, and trusting the guest's b3sum output would
# make the evidence self-attesting.  The gate prompt is bounded below one
# 1,024-byte BLAKE3 chunk, so this deliberately omits tree reduction.
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

PLATFORM_CLAIM = (
    "published Linux x86_64 AppImage launched by WebKitWebDriver under "
    "WebKitGTK, exercising real bundled UI and Tauri IPC; this does not prove "
    "the macOS .app/WKWebView or Windows WebView2 packages"
)

APT_PACKAGES = (
    "binutils",
    "b3sum",
    "ca-certificates",
    "dbus-x11",
    "file",
    "iptables",
    "libappindicator3-1",
    "libayatana-appindicator3-1",
    "librsvg2-2",
    "libwebkit2gtk-4.1-0",
    "minisign",
    "python3-minimal",
    "webkit2gtk-driver",
    "xvfb",
)

REQUIRED_EXECUTABLES = (
    "/usr/bin/b3sum",
    "/usr/bin/WebKitWebDriver",
    "/usr/bin/Xvfb",
    "/usr/bin/dbus-run-session",
    "/usr/bin/file",
    "/usr/bin/minisign",
    "/usr/bin/readelf",
    "/usr/bin/python3",
)

SEED_UDP_ENDPOINTS = (
    ("149.28.32.76", 443),
    ("149.28.32.76", 9091),
    ("140.82.16.112", 443),
    ("140.82.16.112", 9091),
    ("136.244.109.1", 443),
    ("136.244.109.1", 9091),
    ("104.238.171.11", 443),
    ("104.238.171.11", 9091),
    ("202.182.107.41", 443),
    ("202.182.107.41", 9091),
    ("149.28.153.31", 443),
    ("149.28.153.31", 9091),
)

EXPECTED_TESTS = (
    "published AppImage entrypoint launches the packaged Tauri application",
    "first launch creates an isolated native identity and starts the exact node",
    "dashboard renders native local-node and selected-chain truth",
    "one fresh community inference completes through the packaged UI",
    "native reward polling reaches an exact successful mined 0x25 receipt",
    "host-scoped transaction lookup and block explorer agree with that receipt",
    "earnings and projection render honest selected-host state for the new identity",
    "managed node stops cleanly and the secret-bearing profile is destroyed",
)

TERMINAL_RECEIPT_FIELDS = frozenset(
    {
        "assignment_epoch",
        "block_hash",
        "block_height",
        "confirmed",
        "evidence_source",
        "included",
        "index",
        "input_hash",
        "job_id",
        "model_id",
        "output_hash",
        "receipt_url",
        "recovery_epoch",
        "reward_arc",
        "reward_base",
        "status",
        "submitted",
        "success",
        "transaction_domain",
        "tx_hash",
        "tx_type",
        "validator_approvals",
        "validator_set_commitment",
        "validator_set_id",
        "worker",
    }
)


class GateError(RuntimeError):
    """A condition that makes the packaged-product claim unavailable."""


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z")


def canonical_json(value: object) -> bytes:
    return (
        json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True)
        + "\n"
    ).encode("ascii")


def sha256_bytes(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


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
    block_length: int,
    flags: int,
) -> tuple[int, ...]:
    state = list(chaining_value) + list(_BLAKE3_IV[:4]) + [0, 0, block_length, flags]
    message = list(block_words)
    for _ in range(7):
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


def independent_blake3_short(raw: bytes) -> str:
    """Return lower-hex BLAKE3 for one bounded chunk, without guest tooling."""

    if len(raw) > 1024:
        raise GateError("independent BLAKE3 input exceeds one reviewed chunk")
    chaining_value: Sequence[int] = _BLAKE3_IV
    block_count = max(1, (len(raw) + 63) // 64)
    for block_index in range(block_count):
        block = raw[block_index * 64 : (block_index + 1) * 64]
        padded = block + bytes(64 - len(block))
        words = tuple(
            int.from_bytes(padded[offset : offset + 4], "little")
            for offset in range(0, 64, 4)
        )
        final = block_index + 1 == block_count
        flags = (1 if block_index == 0 else 0) | (10 if final else 0)
        compressed = _blake3_compress(chaining_value, words, len(block), flags)
        if final:
            return "".join(word.to_bytes(4, "little").hex() for word in compressed[:8])
        chaining_value = compressed[:8]
    raise GateError("independent BLAKE3 reached an impossible state")


def canonical_updater_public_key_bytes(value: str) -> bytes:
    try:
        normalized = value.strip().encode("ascii", "strict")
        decoded = base64.b64decode(normalized, validate=True)
    except (UnicodeEncodeError, ValueError) as error:
        raise GateError("Tauri updater public key is not canonical outer base64") from error
    if not normalized or len(normalized) > 256 * 1024:
        raise GateError("Tauri updater public key is empty/oversized")
    if base64.b64encode(decoded) != normalized:
        raise GateError("Tauri updater public key has a non-canonical base64 encoding")
    return normalized + b"\n"


def regular_file(
    path: Path,
    label: str,
    *,
    max_bytes: int | None = None,
    allow_empty: bool = False,
) -> os.stat_result:
    try:
        info = path.lstat()
    except OSError as error:
        raise GateError(f"cannot inspect {label}: {error}") from error
    if not stat.S_ISREG(info.st_mode):
        raise GateError(f"{label} must be a regular non-symlink file: {path}")
    if info.st_size < 0 or (info.st_size == 0 and not allow_empty):
        raise GateError(f"{label} is empty: {path}")
    if max_bytes is not None and info.st_size > max_bytes:
        raise GateError(f"{label} exceeds {max_bytes} bytes: {path}")
    return info


def private_file(path: Path, label: str) -> os.stat_result:
    info = regular_file(path, label, max_bytes=1024 * 1024)
    if stat.S_IMODE(info.st_mode) & 0o077:
        raise GateError(f"{label} must not be group/world accessible: {path}")
    return info


def read_bytes(
    path: Path,
    label: str,
    *,
    max_bytes: int = MAX_JSON_BYTES,
    allow_empty: bool = False,
) -> bytes:
    info = regular_file(path, label, max_bytes=max_bytes, allow_empty=allow_empty)
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(path, flags)
    try:
        opened = os.fstat(fd)
        if (opened.st_dev, opened.st_ino) != (info.st_dev, info.st_ino):
            raise GateError(f"{label} changed while opening: {path}")
        raw = bytearray()
        while len(raw) <= max_bytes:
            chunk = os.read(fd, min(1024 * 1024, max_bytes + 1 - len(raw)))
            if not chunk:
                break
            raw.extend(chunk)
        if len(raw) > max_bytes:
            raise GateError(f"{label} exceeds {max_bytes} bytes")
        final = os.fstat(fd)
        if (final.st_dev, final.st_ino, final.st_size) != (
            opened.st_dev,
            opened.st_ino,
            opened.st_size,
        ):
            raise GateError(f"{label} changed while reading: {path}")
        return bytes(raw)
    finally:
        os.close(fd)


def load_json(path: Path, label: str) -> tuple[dict[str, Any], bytes]:
    raw = read_bytes(path, label)
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise GateError(f"{label} is not valid JSON: {error}") from error
    if not isinstance(value, dict):
        raise GateError(f"{label} must be a JSON object")
    return value, raw


def create_dir(path: Path, mode: int = 0o700) -> None:
    path.mkdir(mode=mode, parents=False, exist_ok=False)
    os.chmod(path, mode)


def create_durable_host_output_dir(path: Path, mode: int = 0o700) -> None:
    """Create the one-shot host evidence root and persist its parent entry."""

    parent = path.parent
    try:
        parent_info = parent.lstat()
    except OSError as error:
        raise GateError(f"cannot inspect host output parent {parent}: {error}") from error
    if (
        not stat.S_ISDIR(parent_info.st_mode)
        or stat.S_IMODE(parent_info.st_mode) & 0o022
        or parent_info.st_uid != os.geteuid()
    ):
        raise GateError(
            "host output parent must be a real, current-user-owned, non-group/world-writable directory"
        )
    parent_flags = (
        os.O_RDONLY
        | getattr(os, "O_DIRECTORY", 0)
        | getattr(os, "O_NOFOLLOW", 0)
    )
    parent_fd = os.open(parent, parent_flags)
    try:
        opened_parent = os.fstat(parent_fd)
        if (opened_parent.st_dev, opened_parent.st_ino, opened_parent.st_mode) != (
            parent_info.st_dev,
            parent_info.st_ino,
            parent_info.st_mode,
        ):
            raise GateError(f"host output parent changed while opening: {parent}")
        os.mkdir(path.name, mode=mode, dir_fd=parent_fd)
        child_fd = os.open(
            path.name,
            os.O_RDONLY
            | getattr(os, "O_DIRECTORY", 0)
            | getattr(os, "O_NOFOLLOW", 0),
            dir_fd=parent_fd,
        )
        try:
            os.fchmod(child_fd, mode)
            child = os.fstat(child_fd)
            if not stat.S_ISDIR(child.st_mode) or stat.S_IMODE(child.st_mode) != mode:
                raise GateError("new host output directory identity/mode mismatch")
            os.fsync(child_fd)
        finally:
            os.close(child_fd)
        final_parent = os.fstat(parent_fd)
        if (final_parent.st_dev, final_parent.st_ino, final_parent.st_mode) != (
            opened_parent.st_dev,
            opened_parent.st_ino,
            opened_parent.st_mode,
        ):
            raise GateError(f"host output parent changed while creating: {parent}")
        os.fsync(parent_fd)
    finally:
        os.close(parent_fd)


def create_file(path: Path, raw: bytes, mode: int = 0o400) -> None:
    parent = path.parent
    try:
        parent_info = parent.lstat()
    except OSError as error:
        raise GateError(f"cannot inspect create-only parent {parent}: {error}") from error
    if not stat.S_ISDIR(parent_info.st_mode) or stat.S_IMODE(parent_info.st_mode) & 0o022:
        raise GateError(f"create-only parent must be a non-writable real directory: {parent}")
    parent_flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
    parent_fd = os.open(parent, parent_flags)
    opened_parent = os.fstat(parent_fd)
    if (opened_parent.st_dev, opened_parent.st_ino, opened_parent.st_mode) != (
        parent_info.st_dev,
        parent_info.st_ino,
        parent_info.st_mode,
    ):
        os.close(parent_fd)
        raise GateError(f"create-only parent changed while opening: {parent}")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
    try:
        fd = os.open(path.name, flags, mode, dir_fd=parent_fd)
        try:
            offset = 0
            while offset < len(raw):
                offset += os.write(fd, raw[offset:])
            os.fchmod(fd, mode)
            os.fsync(fd)
        finally:
            os.close(fd)
        final_parent = os.fstat(parent_fd)
        if (final_parent.st_dev, final_parent.st_ino, final_parent.st_mode) != (
            opened_parent.st_dev,
            opened_parent.st_ino,
            opened_parent.st_mode,
        ):
            raise GateError(f"create-only parent changed while writing: {parent}")
        # Persist the new directory entry as well as the file content. This is
        # what makes the armed-no-retry marker survive a host crash.
        os.fsync(parent_fd)
    finally:
        os.close(parent_fd)


def require_exact_keys(value: Mapping[str, Any], expected: Iterable[str], label: str) -> None:
    if not isinstance(value, Mapping):
        raise GateError(f"{label} must be an object")
    actual = set(value)
    wanted = set(expected)
    if actual != wanted:
        raise GateError(
            f"{label} fields differ; missing={sorted(wanted - actual)!r}, "
            f"unexpected={sorted(actual - wanted)!r}"
        )


def validate_release_binding(path: Path) -> tuple[dict[str, Any], bytes]:
    binding, raw = load_json(path, "published release binding")
    require_exact_keys(
        binding,
        {
            "assets",
            "commit",
            "legacy_source",
            "published_evidence",
            "release",
            "release_workflow",
            "repository",
            "schema",
            "tag",
        },
        "published release binding",
    )
    if binding.get("schema") != RELEASE_SCHEMA:
        raise GateError("published release binding schema mismatch")
    if binding.get("repository") != REPOSITORY or binding.get("tag") != TAG:
        raise GateError("published release binding repository/tag mismatch")
    if not SHA_RE.fullmatch(str(binding.get("commit", ""))):
        raise GateError("published release binding commit is malformed")
    release = binding.get("release")
    if not isinstance(release, dict) or release.get("immutable") is not True:
        raise GateError("published release binding is not immutable")
    if type(release.get("id")) is not int or release["id"] <= 0:  # noqa: E721
        raise GateError("published release binding release ID is invalid")
    workflow = binding.get("release_workflow")
    if not isinstance(workflow, dict) or workflow.get("head_sha") != binding["commit"]:
        raise GateError("published release workflow is not bound to the release commit")
    assets = binding.get("assets")
    if not isinstance(assets, dict):
        raise GateError("published release binding assets must be an object")
    asset_ids: set[int] = set()
    for name in (APPIMAGE_NAME, APPIMAGE_SIGNATURE_NAME, NODE_NAME):
        asset = assets.get(name)
        if not isinstance(asset, dict):
            raise GateError(f"published release binding omits {name}")
        if set(asset) != {"id", "sha256", "size"}:
            raise GateError(f"published release binding {name} fields differ")
        if type(asset.get("id")) is not int or asset["id"] <= 0:  # noqa: E721
            raise GateError(f"published release binding {name} ID is invalid")
        if asset["id"] in asset_ids:
            raise GateError("published AppImage release asset IDs are not distinct")
        asset_ids.add(asset["id"])
        if type(asset.get("size")) is not int or asset["size"] <= 0:  # noqa: E721
            raise GateError(f"published release binding {name} size is invalid")
        if not HASH_RE.fullmatch(str(asset.get("sha256", ""))):
            raise GateError(f"published release binding {name} digest is invalid")
    return binding, raw


def verify_bound_asset(binding: Mapping[str, Any], directory: Path, name: str) -> dict[str, Any]:
    path = directory / name
    info = regular_file(path, f"published asset {name}", max_bytes=1024 * 1024 * 1024)
    expected = binding["assets"][name]
    actual_sha = sha256_file(path)
    if info.st_size != expected["size"] or actual_sha != expected["sha256"]:
        raise GateError(f"published asset {name} differs from immutable release binding")
    return {
        "id": expected["id"],
        "name": name,
        "sha256": actual_sha,
        "size": info.st_size,
    }


def build_inference_attempt(
    binding: Mapping[str, Any],
    binding_raw: bytes,
    assets: Mapping[str, Mapping[str, Any]],
    source: Mapping[str, Any],
    updater_public_key: str,
) -> tuple[dict[str, Any], str]:
    challenge = os.urandom(32).hex()
    plan = {
        "apt_snapshot": APT_SNAPSHOT,
        "assets": {name: dict(assets[name]) for name in sorted(assets)},
        "binding_sha256": sha256_bytes(binding_raw),
        "challenge": challenge,
        "coordinator": f"https://{LAX_HOST}",
        "gate_sha256": source["gate_sha256"],
        "inference_timeout_seconds": INFERENCE_WAIT_SECONDS,
        "max_tokens": 8,
        "release_commit": binding["commit"],
        "release_id": binding["release"]["id"],
        "release_tag": binding["tag"],
        "retry_post": False,
        "settlement_ui_timeout_seconds": RECEIPT_UI_WAIT_SECONDS,
        "total_guest_timeout_seconds": GUEST_GATE_TIMEOUT_SECONDS,
        "updater_public_key_sha256": sha256_bytes(
            canonical_updater_public_key_bytes(updater_public_key)
        ),
        "vm_image_digest": LIMA_IMAGE_DIGEST,
        "inference_submit_click_budget": 1,
    }
    plan_sha = sha256_bytes(canonical_json(plan))
    prompt = (
        f"ARC packaged AppImage production gate challenge {challenge} "
        f"plan {plan_sha}: reply with the single word OK"
    )
    attempt = {
        "armed_at": utc_now(),
        "plan": plan,
        "plan_sha256": plan_sha,
        "prompt_sha256": sha256_bytes(prompt.encode("utf-8")),
        "schema": "arc.packaged-appimage-inference-attempt.v1",
        "state": "armed-no-retry",
    }
    return attempt, prompt


def validate_inference_attempt(
    path: Path,
    binding: Mapping[str, Any],
    binding_raw: bytes,
    assets: Mapping[str, Mapping[str, Any]],
) -> tuple[dict[str, Any], bytes, str]:
    attempt, raw = load_json(path, "armed packaged inference attempt")
    if raw != canonical_json(attempt):
        raise GateError("armed packaged inference attempt is not canonical JSON")
    require_exact_keys(
        attempt,
        {"armed_at", "plan", "plan_sha256", "prompt_sha256", "schema", "state"},
        "armed packaged inference attempt",
    )
    if attempt.get("schema") != "arc.packaged-appimage-inference-attempt.v1":
        raise GateError("armed packaged inference attempt schema mismatch")
    if attempt.get("state") != "armed-no-retry":
        raise GateError("packaged inference attempt is not terminal/no-retry armed")
    plan = attempt.get("plan")
    if not isinstance(plan, dict):
        raise GateError("armed packaged inference attempt plan is missing")
    require_exact_keys(
        plan,
        {
            "apt_snapshot",
            "assets",
            "binding_sha256",
            "challenge",
            "coordinator",
            "gate_sha256",
            "inference_timeout_seconds",
            "max_tokens",
            "release_commit",
            "release_id",
            "release_tag",
            "retry_post",
            "settlement_ui_timeout_seconds",
            "total_guest_timeout_seconds",
            "updater_public_key_sha256",
            "vm_image_digest",
            "inference_submit_click_budget",
        },
        "armed packaged inference plan",
    )
    if sha256_bytes(canonical_json(plan)) != attempt.get("plan_sha256"):
        raise GateError("armed packaged inference plan digest mismatch")
    expected = {
        "apt_snapshot": APT_SNAPSHOT,
        "assets": {name: dict(assets[name]) for name in sorted(assets)},
        "binding_sha256": sha256_bytes(binding_raw),
        "coordinator": f"https://{LAX_HOST}",
        "inference_timeout_seconds": INFERENCE_WAIT_SECONDS,
        "max_tokens": 8,
        "release_commit": binding["commit"],
        "release_id": binding["release"]["id"],
        "release_tag": binding["tag"],
        "retry_post": False,
        "settlement_ui_timeout_seconds": RECEIPT_UI_WAIT_SECONDS,
        "total_guest_timeout_seconds": GUEST_GATE_TIMEOUT_SECONDS,
        "vm_image_digest": LIMA_IMAGE_DIGEST,
        "inference_submit_click_budget": 1,
    }
    for key, value in expected.items():
        if plan.get(key) != value:
            raise GateError(f"armed packaged inference plan {key} mismatch")
    if not HASH_RE.fullmatch(str(plan.get("challenge", ""))):
        raise GateError("armed packaged inference challenge is not 256 bits")
    if not HASH_RE.fullmatch(str(plan.get("gate_sha256", ""))):
        raise GateError("armed packaged inference gate source digest is malformed")
    if not HASH_RE.fullmatch(str(plan.get("updater_public_key_sha256", ""))):
        raise GateError("armed packaged inference updater public key digest is malformed")
    prompt = (
        f"ARC packaged AppImage production gate challenge {plan['challenge']} "
        f"plan {attempt['plan_sha256']}: reply with the single word OK"
    )
    if sha256_bytes(prompt.encode("utf-8")) != attempt.get("prompt_sha256"):
        raise GateError("armed packaged inference prompt digest mismatch")
    return attempt, raw, prompt


def run_command(
    argv: Sequence[str],
    *,
    cwd: Path | None = None,
    env: Mapping[str, str] | None = None,
    input_data: bytes | None = None,
    timeout: int = 120,
    check: bool = True,
) -> subprocess.CompletedProcess[bytes]:
    try:
        result = subprocess.run(
            list(argv),
            cwd=cwd,
            env=dict(env) if env is not None else None,
            input=input_data,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise GateError(f"command failed to run ({argv[0]}): {error}") from error
    if check and result.returncode != 0:
        message = result.stderr.decode("utf-8", "replace")[-2000:]
        raise GateError(f"command failed ({argv[0]}, rc={result.returncode}): {message}")
    return result


def arc_blake3_hash(raw: bytes) -> str:
    """Hash exact ARC inference bytes with the pinned guest b3sum binary."""
    result = run_command(
        ["/usr/bin/b3sum", "--no-names"], input_data=raw, timeout=15
    )
    rendered = result.stdout.decode("ascii", "strict").strip()
    if not HASH_RE.fullmatch(rendered):
        raise GateError("pinned b3sum returned a malformed BLAKE3 digest")
    return f"0x{rendered}"


def decode_tauri_minisign_document(
    path: Path, label: str, *, max_encoded_bytes: int = 256 * 1024
) -> bytes:
    encoded = read_bytes(path, label, max_bytes=max_encoded_bytes)
    try:
        compact = encoded.decode("ascii", "strict").strip()
        decoded = base64.b64decode(compact, validate=True)
        decoded.decode("utf-8", "strict")
    except (UnicodeDecodeError, ValueError) as error:
        raise GateError(f"{label} is not strict outer-base64 UTF-8") from error
    if not decoded or len(decoded) > max_encoded_bytes or b"\x00" in decoded:
        raise GateError(f"{label} decoded document is empty/oversized/unsafe")
    if base64.b64encode(decoded).decode("ascii") != compact:
        raise GateError(f"{label} outer base64 is not canonical")
    return decoded


def minisign_public_key_line(document: bytes) -> str:
    lines = [line.strip() for line in document.decode("utf-8", "strict").splitlines()]
    candidates: list[str] = []
    for line in lines:
        try:
            decoded = base64.b64decode(line, validate=True)
        except (UnicodeEncodeError, ValueError):
            continue
        if len(decoded) == 42:
            candidates.append(line)
    if len(candidates) != 1:
        raise GateError("Tauri updater public key document is not one exact minisign key")
    return candidates[0]


def verify_updater_signature_in_guest(
    public_key_path: Path,
    appimage: Path,
    signature_path: Path,
    work: Path,
    evidence: Path,
    runtime: Mapping[str, Any],
) -> dict[str, Any]:
    public_document = decode_tauri_minisign_document(
        public_key_path, "Tauri updater public key"
    )
    signature_document = decode_tauri_minisign_document(
        signature_path, "Tauri updater signature"
    )
    public_key = minisign_public_key_line(public_document)
    decoded_signature = work / "updater-signature.minisig"
    create_file(decoded_signature, signature_document, 0o400)
    checked = run_command(
        [
            "/usr/bin/minisign",
            "-Vm",
            os.fspath(appimage),
            "-P",
            public_key,
            "-x",
            os.fspath(decoded_signature),
        ],
        timeout=180,
    )
    stdout_path = evidence / "updater-signature.stdout"
    stderr_path = evidence / "updater-signature.stderr"
    create_file(stdout_path, checked.stdout, 0o400)
    create_file(stderr_path, checked.stderr, 0o400)
    decoded_signature.unlink()
    minisign_identity = runtime.get("executables", {}).get("/usr/bin/minisign")
    minisign_package = runtime.get("packages", {}).get("minisign")
    if not isinstance(minisign_identity, dict) or not isinstance(minisign_package, dict):
        raise GateError("pinned minisign provenance is absent from guest inventory")
    return {
        "appimage_sha256": sha256_file(appimage),
        "minisign_binary": dict(minisign_identity),
        "minisign_package": dict(minisign_package),
        "public_key_sha256": sha256_file(public_key_path),
        "schema": "arc.packaged-appimage-updater-signature.v1",
        "signature_sha256": sha256_file(signature_path),
        "stderr_sha256": sha256_bytes(checked.stderr),
        "stdout_sha256": sha256_bytes(checked.stdout),
        "updater_signature_verified": True,
    }


def free_loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


class RestrictedConnectRelay:
    """Authenticated HTTP CONNECT relay for exactly production LAX TLS."""

    def __init__(self, upstream_port: int, token: str) -> None:
        if not (1 <= upstream_port <= 65535):
            raise GateError("relay upstream port is invalid")
        if not re.fullmatch(r"[0-9a-f]{64}", token):
            raise GateError("relay token must be 32 random bytes encoded as lowercase hex")
        self.upstream_port = upstream_port
        self.token = token
        self.started_at = utc_now()
        self._lock = threading.Lock()
        self._events: list[dict[str, Any]] = []
        relay = self

        class Server(socketserver.ThreadingTCPServer):
            allow_reuse_address = False
            daemon_threads = True

        class Handler(socketserver.BaseRequestHandler):
            def handle(self) -> None:  # noqa: C901 - protocol is clearer linearly
                started = time.monotonic()
                peer = self.client_address[0]
                event: dict[str, Any] = {
                    "accepted": False,
                    "bytes_client_to_lax": 0,
                    "bytes_lax_to_client": 0,
                    "peer": peer,
                    "reason": "invalid_request",
                    "target": None,
                }
                event_recorded = False
                try:
                    self.request.settimeout(5)
                    header = bytearray()
                    while b"\r\n\r\n" not in header:
                        chunk = self.request.recv(1024)
                        if not chunk:
                            raise GateError("CONNECT client closed before headers")
                        header.extend(chunk)
                        if len(header) > MAX_HTTP_HEADER_BYTES:
                            raise GateError("CONNECT headers exceed bound")
                    raw_head, remainder = bytes(header).split(b"\r\n\r\n", 1)
                    try:
                        lines = raw_head.decode("ascii").split("\r\n")
                    except UnicodeDecodeError as error:
                        raise GateError("CONNECT headers are not ASCII") from error
                    first = lines[0].split(" ")
                    if len(first) != 3:
                        raise GateError("CONNECT request line is malformed")
                    method, target, version = first
                    event["target"] = target[:256]
                    headers: dict[str, str] = {}
                    for line in lines[1:]:
                        if ":" not in line:
                            raise GateError("CONNECT header is malformed")
                        key, value = line.split(":", 1)
                        lowered = key.strip().lower()
                        if lowered in headers:
                            raise GateError("duplicate CONNECT header")
                        headers[lowered] = value.strip()
                    expected_auth = "Basic " + base64.b64encode(
                        f"arc:{relay.token}".encode("ascii")
                    ).decode("ascii")
                    provided = headers.get("proxy-authorization", "")
                    if not hmac.compare_digest(provided, expected_auth):
                        event["reason"] = "authentication_rejected"
                        self.request.sendall(
                            b"HTTP/1.1 407 Proxy Authentication Required\r\n"
                            b"Proxy-Authenticate: Basic realm=arc-packaged-gate\r\n"
                            b"Connection: close\r\n\r\n"
                        )
                        return
                    if method != "CONNECT" or version != "HTTP/1.1":
                        event["reason"] = "method_or_version_rejected"
                        self.request.sendall(b"HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\n\r\n")
                        return
                    if target != f"{LAX_HOST}:{LAX_PORT}":
                        event["reason"] = "target_rejected"
                        self.request.sendall(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")
                        return
                    host_header = headers.get("host")
                    if host_header is not None and host_header != target:
                        event["reason"] = "host_header_rejected"
                        self.request.sendall(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                        return
                    upstream = socket.create_connection(
                        ("127.0.0.1", relay.upstream_port), timeout=10
                    )
                    with upstream:
                        upstream.setblocking(False)
                        self.request.setblocking(False)
                        self.request.sendall(
                            b"HTTP/1.1 200 Connection Established\r\n"
                            b"Proxy-Agent: arc-packaged-gate\r\n\r\n"
                        )
                        event["accepted"] = True
                        event["reason"] = "exact_lax_tls"
                        with relay._lock:
                            relay._events.append(event)
                            event_recorded = True
                        if remainder:
                            upstream.sendall(remainder)
                            event["bytes_client_to_lax"] += len(remainder)
                        selector = selectors.DefaultSelector()
                        selector.register(self.request, selectors.EVENT_READ, (upstream, "bytes_client_to_lax"))
                        selector.register(upstream, selectors.EVENT_READ, (self.request, "bytes_lax_to_client"))
                        # The production coordinator permits up to 3900s for
                        # the multi-pass community inference/approval path.
                        deadline = time.monotonic() + 4200
                        while time.monotonic() < deadline:
                            ready = selector.select(timeout=1)
                            if not ready:
                                continue
                            ended = False
                            for key, _ in ready:
                                destination, counter = key.data
                                try:
                                    chunk = key.fileobj.recv(65536)
                                except BlockingIOError:
                                    continue
                                if not chunk:
                                    ended = True
                                    break
                                destination.sendall(chunk)
                                event[counter] += len(chunk)
                            if ended:
                                break
                        selector.close()
                except (GateError, OSError) as error:
                    event["reason"] = f"io_rejected:{type(error).__name__}"
                    with contextlib.suppress(OSError):
                        self.request.sendall(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
                finally:
                    event["duration_ms"] = int((time.monotonic() - started) * 1000)
                    if not event_recorded:
                        with relay._lock:
                            relay._events.append(event)

        self._server = Server(("127.0.0.1", 0), Handler)
        self.port = int(self._server.server_address[1])
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    def start(self) -> None:
        self._thread.start()

    def close(self) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=10)
        if self._thread.is_alive():
            raise GateError("restricted CONNECT relay did not stop")

    def summary(self) -> dict[str, Any]:
        with self._lock:
            events = list(self._events)
        accepted = [event for event in events if event["accepted"]]
        rejected = [event for event in events if not event["accepted"]]
        if not accepted:
            raise GateError("restricted CONNECT relay accepted no LAX TLS connections")
        if any(event["target"] != f"{LAX_HOST}:{LAX_PORT}" for event in accepted):
            raise GateError("restricted CONNECT relay accepted a non-LAX target")
        return {
            "accepted_connections": len(accepted),
            "events": events,
            "listen": "127.0.0.1",
            "listen_port": self.port,
            "rejected_connections": len(rejected),
            "started_at": self.started_at,
            "target": f"{LAX_HOST}:{LAX_PORT}",
            "token_sha256": sha256_bytes(self.token.encode("ascii")),
            "upstream": f"127.0.0.1:{self.upstream_port}",
        }


class W3CClient:
    ELEMENT_KEY = "element-6066-11e4-a52e-4f735466cecf"

    def __init__(self, host: str, port: int) -> None:
        self.host = host
        self.port = port
        self.session_id: str | None = None
        self.transcript: list[dict[str, Any]] = []

    def request(self, method: str, path: str, payload: object | None = None) -> Any:
        raw_payload = None if payload is None else canonical_json(payload).rstrip(b"\n")
        headers = {"Accept": "application/json"}
        if raw_payload is not None:
            headers["Content-Type"] = "application/json; charset=utf-8"
        connection = http.client.HTTPConnection(self.host, self.port, timeout=30)
        started = time.monotonic()
        try:
            connection.request(method, path, body=raw_payload, headers=headers)
            response = connection.getresponse()
            raw = response.read(MAX_JSON_BYTES + 1)
        except OSError as error:
            raise GateError(f"WebDriver {method} {path} failed: {error}") from error
        finally:
            connection.close()
        if len(raw) > MAX_JSON_BYTES:
            raise GateError("WebDriver response exceeded bound")
        self.transcript.append(
            {
                "duration_ms": int((time.monotonic() - started) * 1000),
                "method": method,
                "path": path,
                "request_sha256": sha256_bytes(raw_payload or b""),
                "response_bytes": len(raw),
                "response_sha256": sha256_bytes(raw),
                "status": response.status,
            }
        )
        try:
            value = json.loads(raw) if raw else {"value": None}
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise GateError(f"WebDriver returned invalid JSON for {method} {path}") from error
        if not (200 <= response.status < 300):
            message = value.get("value", {}).get("message") if isinstance(value, dict) else None
            raise GateError(
                f"WebDriver {method} {path} returned HTTP {response.status}: {str(message)[:500]}"
            )
        if not isinstance(value, dict) or "value" not in value:
            raise GateError(f"WebDriver {method} {path} omitted W3C value")
        return value["value"]

    def new_session(self, application: Path) -> None:
        value = self.request(
            "POST",
            "/session",
            {
                "capabilities": {
                    "alwaysMatch": {
                        "acceptInsecureCerts": False,
                        "webkitgtk:browserOptions": {
                            "args": [],
                            "binary": os.fspath(application),
                        },
                    }
                },
                "desiredCapabilities": {
                    "acceptInsecureCerts": False,
                    "webkitgtk:browserOptions": {
                        "args": [],
                        "binary": os.fspath(application),
                    },
                },
            },
        )
        session_id = value.get("sessionId") if isinstance(value, dict) else None
        if not isinstance(session_id, str) or not session_id:
            raise GateError("WebDriver new-session response omitted sessionId")
        self.session_id = session_id
        self.request(
            "POST",
            self._path("timeouts"),
            {"implicit": 0, "pageLoad": 120_000, "script": 30_000},
        )
        self.request(
            "POST",
            self._path("window/rect"),
            {"height": 900, "width": 1280},
        )

    def _path(self, suffix: str) -> str:
        if self.session_id is None:
            raise GateError("WebDriver session is not open")
        return f"/session/{quote(self.session_id, safe='')}/{suffix}"

    @staticmethod
    def _element_id(value: Any) -> str:
        if not isinstance(value, dict):
            raise GateError("WebDriver element response is not an object")
        element_id = value.get(W3CClient.ELEMENT_KEY) or value.get("ELEMENT")
        if not isinstance(element_id, str) or not element_id:
            raise GateError("WebDriver element response omitted element ID")
        return element_id

    def elements(self, css: str) -> list[str]:
        value = self.request(
            "POST", self._path("elements"), {"using": "css selector", "value": css}
        )
        if not isinstance(value, list):
            raise GateError("WebDriver elements response is not an array")
        return [self._element_id(item) for item in value]

    def element(self, css: str) -> str:
        value = self.request(
            "POST", self._path("element"), {"using": "css selector", "value": css}
        )
        return self._element_id(value)

    def wait(self, condition: Callable[[], Any], label: str, timeout: float = 30) -> Any:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            # A false-y value means "not ready yet". GateError is deliberately
            # not swallowed: launch/inference errors and W3C transport failures
            # are terminal facts, not transient element absence.
            value = condition()
            if value:
                return value
            time.sleep(0.25)
        raise GateError(f"timed out waiting for {label}")

    def wait_element(self, css: str, timeout: float = 30) -> str:
        return self.wait(
            lambda: (found[0] if (found := self.elements(css)) else None),
            css,
            timeout,
        )

    def click(self, element_id: str) -> None:
        self.request("POST", self._path(f"element/{quote(element_id, safe='')}/click"), {})

    def clear(self, element_id: str) -> None:
        self.request("POST", self._path(f"element/{quote(element_id, safe='')}/clear"), {})

    def send_keys(self, element_id: str, value: str) -> None:
        self.request(
            "POST",
            self._path(f"element/{quote(element_id, safe='')}/value"),
            {"text": value, "value": list(value)},
        )

    def text(self, element_id: str) -> str:
        value = self.request("GET", self._path(f"element/{quote(element_id, safe='')}/text"))
        if not isinstance(value, str):
            raise GateError("WebDriver element text is not a string")
        return value

    def attribute(self, element_id: str, name: str) -> str | None:
        value = self.request(
            "GET",
            self._path(
                f"element/{quote(element_id, safe='')}/attribute/{quote(name, safe='')}"
            ),
        )
        if value is not None and not isinstance(value, str):
            raise GateError("WebDriver element attribute is not a string/null")
        return value

    def property(self, element_id: str, name: str) -> Any:
        return self.request(
            "GET",
            self._path(f"element/{quote(element_id, safe='')}/property/{quote(name, safe='')}"),
        )

    def screenshot(self, output: Path) -> dict[str, Any]:
        encoded = self.request("GET", self._path("screenshot"))
        if not isinstance(encoded, str):
            raise GateError("WebDriver screenshot was not base64 text")
        try:
            raw = base64.b64decode(encoded, validate=True)
        except ValueError as error:
            raise GateError("WebDriver screenshot base64 is invalid") from error
        if len(raw) < 10_000 or not raw.startswith(b"\x89PNG\r\n\x1a\n"):
            raise GateError("WebDriver screenshot is empty or not PNG")
        create_file(output, raw, 0o400)
        return {"name": output.name, "sha256": sha256_bytes(raw), "size": len(raw)}

    def close(self) -> None:
        if self.session_id is None:
            return
        session_id = self.session_id
        self.session_id = None
        self.request("DELETE", f"/session/{quote(session_id, safe='')}")


def testid(name: str) -> str:
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]*", name):
        raise GateError(f"unsafe test id: {name}")
    return f'[data-testid="{name}"]'


def package_tree_manifest(root: Path) -> tuple[list[dict[str, Any]], str]:
    canonical_root = root.resolve(strict=True)
    root_info = root.lstat()
    if not stat.S_ISDIR(root_info.st_mode):
        raise GateError("extracted AppImage root must be a real directory")
    entries: list[dict[str, Any]] = []
    total_file_bytes = 0
    for path in root.rglob("*"):
        if len(entries) >= MAX_PACKAGE_TREE_ENTRIES:
            raise GateError("extracted AppImage exceeds the package-tree entry bound")
        relative = path.relative_to(root).as_posix()
        if len(relative.encode("utf-8")) > MAX_PACKAGE_PATH_BYTES:
            raise GateError("extracted AppImage contains an oversized path")
        if len(PurePosixPath(relative).parts) > MAX_PACKAGE_DEPTH:
            raise GateError("extracted AppImage exceeds the package-tree depth bound")
        info = path.lstat()
        mode = stat.S_IMODE(info.st_mode)
        if stat.S_ISREG(info.st_mode):
            if info.st_size > MAX_PACKAGE_FILE_BYTES:
                raise GateError("extracted AppImage contains an oversized file")
            total_file_bytes += info.st_size
            if total_file_bytes > MAX_PACKAGE_TREE_TOTAL_BYTES:
                raise GateError("extracted AppImage exceeds the total file-byte bound")
            entries.append(
                {
                    "kind": "file",
                    "mode": f"{mode:04o}",
                    "path": relative,
                    "sha256": sha256_file(path),
                    "size": info.st_size,
                }
            )
        elif stat.S_ISDIR(info.st_mode):
            entries.append({"kind": "directory", "mode": f"{mode:04o}", "path": relative})
        elif stat.S_ISLNK(info.st_mode):
            target = os.readlink(path)
            try:
                resolved_target = path.resolve(strict=True)
                resolved_target.relative_to(canonical_root)
            except (OSError, RuntimeError, ValueError) as error:
                raise GateError(
                    f"extracted AppImage symlink escapes or is unresolved: {relative}"
                ) from error
            entries.append(
                {"kind": "symlink", "mode": f"{mode:04o}", "path": relative, "target": target}
            )
        else:
            raise GateError(f"extracted AppImage contains unsupported file type: {relative}")
    if not entries:
        raise GateError("extracted AppImage tree is empty")
    entries.sort(key=lambda item: item["path"])
    # Resolution is checked separately for the executable.  Merely hashing the
    # paths is not sufficient because an AppRun symlink could escape the tree.
    if not canonical_root.is_dir():
        raise GateError("extracted AppImage root is not a directory")
    raw = canonical_json(entries)
    return entries, sha256_bytes(raw)


def inspect_extracted_appimage(root: Path) -> dict[str, Any]:
    canonical_root = root.resolve(strict=True)
    app_run = root / "AppRun"
    info = app_run.lstat()
    if not (stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode)):
        raise GateError("AppImage AppRun is not a file/symlink")
    executable = app_run.resolve(strict=True)
    try:
        executable.relative_to(canonical_root)
    except ValueError as error:
        raise GateError("AppImage AppRun resolves outside the extracted package") from error
    executable_info = executable.stat()
    if not stat.S_ISREG(executable_info.st_mode) or not (executable_info.st_mode & 0o111):
        raise GateError("AppImage AppRun target is not an executable regular file")
    if executable.name != EXPECTED_APP_BINARY:
        raise GateError(
            f"AppImage AppRun target must be {EXPECTED_APP_BINARY}, got {executable.name}"
        )
    file_result = run_command(["/usr/bin/file", "-b", os.fspath(executable)])
    file_text = file_result.stdout.decode("utf-8", "replace").strip()
    if "ELF 64-bit" not in file_text or "x86-64" not in file_text:
        raise GateError(f"AppImage executable is not Linux ELF64 x86-64: {file_text}")
    readelf = run_command(["/usr/bin/readelf", "-h", "-n", os.fspath(executable)])
    readelf_text = readelf.stdout.decode("utf-8", "replace")
    if "Advanced Micro Devices X86-64" not in readelf_text:
        raise GateError("AppImage executable readelf machine is not x86-64")
    build_ids = re.findall(r"Build ID:\s*([0-9a-f]+)", readelf_text)
    if len(build_ids) != 1:
        raise GateError("AppImage executable does not have exactly one ELF build ID")

    desktop_files = sorted(root.rglob("*.desktop"))
    if len(desktop_files) != 1:
        raise GateError("AppImage must contain exactly one .desktop metadata file")
    desktop_raw = read_bytes(desktop_files[0], "AppImage desktop metadata", max_bytes=128 * 1024)
    desktop_text = desktop_raw.decode("utf-8", "strict")
    names = re.findall(r"^Name=(.+)$", desktop_text, re.MULTILINE)
    execs = re.findall(r"^Exec=(.+)$", desktop_text, re.MULTILINE)
    if names != ["ARC Node"] or len(execs) != 1 or EXPECTED_APP_BINARY not in execs[0]:
        raise GateError("AppImage desktop metadata identity does not describe ARC Node/arc-desktop")

    entries, tree_sha = package_tree_manifest(root)
    return {
        "app_run": {
            "kind": "symlink" if app_run.is_symlink() else "file",
            "sha256": sha256_file(executable),
            "target": executable.relative_to(canonical_root).as_posix(),
        },
        "desktop_metadata": {
            "exec": execs[0],
            "name": names[0],
            "path": desktop_files[0].relative_to(root).as_posix(),
            "sha256": sha256_bytes(desktop_raw),
        },
        "elf": {
            "build_id": build_ids[0],
            "file": file_text,
            "machine": "Advanced Micro Devices X86-64",
            "sha256": sha256_file(executable),
        },
        "entry_count": len(entries),
        "tree_limits": {
            "max_depth": MAX_PACKAGE_DEPTH,
            "max_entries": MAX_PACKAGE_TREE_ENTRIES,
            "max_file_bytes": MAX_PACKAGE_FILE_BYTES,
            "max_path_bytes": MAX_PACKAGE_PATH_BYTES,
            "max_total_file_bytes": MAX_PACKAGE_TREE_TOTAL_BYTES,
        },
        "tree_sha256": tree_sha,
    }


def process_matches(expected_basename: str, expected_sha256: str) -> list[dict[str, Any]]:
    matches: list[dict[str, Any]] = []
    proc = Path("/proc")
    if not proc.is_dir():
        raise GateError("packaged AppImage process inspection requires Linux /proc")
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        exe_link = entry / "exe"
        try:
            target = Path(os.readlink(exe_link))
        except OSError:
            continue
        if target.name != expected_basename:
            continue
        try:
            digest = sha256_file(exe_link)
            started_ticks = (entry / "stat").read_text(encoding="ascii").split()[21]
        except (OSError, IndexError, UnicodeDecodeError):
            continue
        if digest == expected_sha256:
            matches.append(
                {
                    "exe": os.fspath(target),
                    "exe_sha256": digest,
                    "pid": int(entry.name),
                    "start_ticks": int(started_ticks),
                }
            )
    return sorted(matches, key=lambda item: item["pid"])


def wait_exact_process(
    basename: str, digest: str, *, timeout: float = 30, allow_zero: bool = False
) -> list[dict[str, Any]]:
    deadline = time.monotonic() + timeout
    latest: list[dict[str, Any]] = []
    while time.monotonic() < deadline:
        latest = process_matches(basename, digest)
        if (allow_zero and len(latest) == 0) or (not allow_zero and len(latest) == 1):
            return latest
        if len(latest) > 1:
            raise GateError(f"more than one exact {basename} process is running")
        time.sleep(0.25)
    expected = "zero" if allow_zero else "one"
    raise GateError(f"timed out waiting for exactly {expected} {basename} process; saw {len(latest)}")


def runtime_inventory() -> dict[str, Any]:
    executables: dict[str, Any] = {}
    for raw_path in REQUIRED_EXECUTABLES:
        path = Path(raw_path)
        info = regular_file(path, f"runtime executable {raw_path}", max_bytes=1024 * 1024 * 1024)
        if not (info.st_mode & 0o111):
            raise GateError(f"runtime executable is not executable: {raw_path}")
        executables[raw_path] = {"sha256": sha256_file(path), "size": info.st_size}
    versions = run_command(
        ["/usr/bin/dpkg-query", "-W", "-f", "${Package}\t${Version}\t${Architecture}\n", *APT_PACKAGES]
    ).stdout.decode("utf-8", "strict")
    package_rows: dict[str, dict[str, str]] = {}
    for line in versions.splitlines():
        parts = line.split("\t")
        if len(parts) != 3 or parts[0] in package_rows:
            raise GateError("dpkg-query returned malformed/duplicate package inventory")
        package_rows[parts[0]] = {"architecture": parts[2], "version": parts[1]}
    if set(package_rows) != set(APT_PACKAGES):
        raise GateError("installed package inventory differs from exact gate package set")
    driver_version = run_command(
        ["/usr/bin/WebKitWebDriver", "--version"], check=False, timeout=15
    )
    python_version = run_command(["/usr/bin/python3", "--version"])
    source_files: list[Path] = []
    for base in (
        Path("/etc/apt/arc-packaged-live.sources"),
        Path("/etc/apt/sources.list"),
        Path("/etc/apt/sources.list.d"),
    ):
        candidates = [base] if base.is_file() else sorted(base.glob("*")) if base.is_dir() else []
        source_files.extend(
            path for path in candidates if path.is_file() and not path.is_symlink()
        )
    sources = {os.fspath(path): sha256_file(path) for path in source_files}
    return {
        "apt_snapshot": APT_SNAPSHOT,
        "apt_sources": sources,
        "executables": executables,
        "packages": dict(sorted(package_rows.items())),
        "python_version": python_version.stdout.decode("ascii", "replace").strip(),
        "uname": " ".join(platform.uname()),
        "webkit_driver_version": (
            driver_version.stdout + driver_version.stderr
        ).decode("utf-8", "replace").strip()[:500],
    }


def read_connect_response(sock: socket.socket) -> bytes:
    raw = bytearray()
    while b"\r\n\r\n" not in raw:
        chunk = sock.recv(1024)
        if not chunk:
            raise GateError("CONNECT relay closed before its response")
        raw.extend(chunk)
        if len(raw) > MAX_HTTP_HEADER_BYTES:
            raise GateError("CONNECT relay response headers exceed bound")
    return bytes(raw)


def fetch_receipt_through_proxy(
    proxy_host: str, proxy_port: int, token: str, tx_hash: str
) -> tuple[dict[str, Any], bytes, int]:
    authorization = "Basic " + base64.b64encode(f"arc:{token}".encode("ascii")).decode("ascii")
    try:
        raw_socket = socket.create_connection((proxy_host, proxy_port), timeout=15)
    except OSError as error:
        raise GateError(f"cannot connect to restricted CONNECT relay: {error}") from error
    with raw_socket:
        raw_socket.settimeout(30)
        raw_socket.sendall(
            (
                f"CONNECT {LAX_HOST}:{LAX_PORT} HTTP/1.1\r\n"
                f"Host: {LAX_HOST}:{LAX_PORT}\r\n"
                f"Proxy-Authorization: {authorization}\r\n"
                "Connection: keep-alive\r\n\r\n"
            ).encode("ascii")
        )
        connect_response = read_connect_response(raw_socket)
        if not connect_response.startswith(b"HTTP/1.1 200 "):
            raise GateError("restricted CONNECT relay rejected the exact receipt fetch")
        context = ssl.create_default_context()
        with context.wrap_socket(raw_socket, server_hostname=LAX_HOST) as tls_socket:
            tls_socket.sendall(
                (
                    f"GET /community/reward_receipt/{tx_hash} HTTP/1.1\r\n"
                    f"Host: {LAX_HOST}\r\n"
                    "Accept: application/json\r\n"
                    "Connection: close\r\n\r\n"
                ).encode("ascii")
            )
            response = http.client.HTTPResponse(tls_socket)
            response.begin()
            raw = response.read(MAX_RECEIPT_BYTES + 1)
            status_code = response.status
    if status_code != 200:
        raise GateError(f"canonical reward receipt returned HTTP {status_code}")
    if len(raw) > MAX_RECEIPT_BYTES:
        raise GateError("canonical reward receipt exceeds size bound")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise GateError("canonical reward receipt is not JSON") from error
    if not isinstance(value, dict):
        raise GateError("canonical reward receipt must be an object")
    return value, raw, status_code


def validate_terminal_receipt(value: Mapping[str, Any], tx_hash: str) -> dict[str, Any]:
    require_exact_keys(value, TERMINAL_RECEIPT_FIELDS, "canonical 0x25 reward receipt")
    if value.get("status") != "mined_success" or value.get("tx_type") != "0x25":
        raise GateError("canonical reward receipt is not a successful mined 0x25")
    if value.get("tx_hash") != tx_hash:
        raise GateError("canonical reward receipt transaction differs from UI lookup")
    for field in (
        "assignment_epoch",
        "block_hash",
        "input_hash",
        "job_id",
        "model_id",
        "output_hash",
        "transaction_domain",
        "validator_set_commitment",
        "worker",
    ):
        if not CANONICAL_HASH_RE.fullmatch(str(value.get(field, ""))):
            raise GateError(f"canonical reward receipt {field} is malformed")
    if value.get("submitted") is not True or value.get("included") is not True:
        raise GateError("canonical reward receipt does not prove submission/inclusion")
    if value.get("confirmed") is not True or value.get("success") is not True:
        raise GateError("canonical reward receipt does not prove successful confirmation")
    if value.get("reward_base") != 2_500_000_000 or value.get("reward_arc") != 2.5:
        raise GateError("canonical reward receipt amount is not exactly 2.5 ARC")
    if type(value.get("block_height")) is not int or value["block_height"] <= 0:  # noqa: E721
        raise GateError("canonical reward receipt block height is invalid")
    if type(value.get("index")) is not int or value["index"] < 0:  # noqa: E721
        raise GateError("canonical reward receipt index is invalid")
    if type(value.get("validator_approvals")) is not int or value["validator_approvals"] < 5:  # noqa: E721
        raise GateError("canonical reward receipt lacks five validator approvals")
    if value.get("receipt_url") != f"/community/reward_receipt/{tx_hash}":
        raise GateError("canonical reward receipt URL is not transaction-bound")
    if value.get("evidence_source") != "successful mined CommunityInferenceReward receipt":
        raise GateError("canonical reward receipt evidence label is not the success contract")
    for field in ("recovery_epoch", "validator_set_id"):
        if type(value.get(field)) is not int or value[field] < 0:  # noqa: E721
            raise GateError(f"canonical reward receipt {field} is invalid")
    return dict(value)


def validate_receipt_product_binding(
    receipt: Mapping[str, Any], expected: Mapping[str, str]
) -> None:
    """Bind independent public receipt truth to this exact packaged UI run."""
    fields = {
        "input_hash",
        "job_id",
        "model_id",
        "output_hash",
        "receipt_url",
        "tx_hash",
        "tx_type",
        "worker",
    }
    require_exact_keys(expected, fields, "packaged inference UI receipt binding")
    for field in fields:
        if receipt.get(field) != expected[field]:
            raise GateError(
                f"canonical reward receipt {field} differs from the exact packaged UI run"
            )


def fetch_coherent_terminal_receipt(
    proxy_host: str, proxy_port: int, token: str, tx_hash: str
) -> tuple[dict[str, Any], bytes, int]:
    """Retry only safe GETs and require two identical terminal snapshots."""
    previous: bytes | None = None
    last_error: Exception | None = None
    reads = 0
    for attempt in range(6):
        try:
            value, raw, _ = fetch_receipt_through_proxy(
                proxy_host, proxy_port, token, tx_hash
            )
            reads += 1
            validated = validate_terminal_receipt(value, tx_hash)
            canonical = canonical_json(validated)
            if previous == canonical:
                return validated, raw, reads
            previous = canonical
        except GateError as error:
            last_error = error
        if attempt != 5:
            time.sleep(2)
    detail = f": {last_error}" if last_error is not None else ""
    raise GateError(f"canonical reward receipt did not yield two coherent safe reads{detail}")


def copy_regular_exclusive(source: Path, destination: Path, mode: int) -> None:
    regular_file(source, f"copy source {source.name}", max_bytes=1024 * 1024 * 1024)
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(destination, flags, mode)
    try:
        with source.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                offset = 0
                while offset < len(chunk):
                    offset += os.write(fd, chunk[offset:])
        os.fsync(fd)
        os.fchmod(fd, mode)
    finally:
        os.close(fd)


def sanitized_app_environment(
    profile: Path, proxy_host: str, proxy_port: int, token: str
) -> dict[str, str]:
    # Start empty.  In particular, never carry LD_PRELOAD, LD_LIBRARY_PATH,
    # GTK_MODULES, GIO_EXTRA_MODULES, a compiler wrapper, or an ambient ARC
    # override into the process whose packaged identity this gate attests.
    inherited_allowlist = (
        "DBUS_SESSION_BUS_ADDRESS",
        "DISPLAY",
        "XAUTHORITY",
        "XDG_RUNTIME_DIR",
    )
    environment = {
        key: os.environ[key] for key in inherited_allowlist if key in os.environ
    }
    proxy = f"http://arc:{token}@{proxy_host}:{proxy_port}"
    environment.update(
        {
            "APPIMAGE_EXTRACT_AND_RUN": "1",
            "HOME": os.fspath(profile / "home"),
            "HTTPS_PROXY": proxy,
            "LANG": "C.UTF-8",
            "NO_AT_BRIDGE": "1",
            "NO_PROXY": "",
            "TAURI_AUTOMATION": "true",
            "TAURI_WEBVIEW_AUTOMATION": "true",
            "PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            "WEBKIT_DISABLE_COMPOSITING_MODE": "1",
            "XDG_CACHE_HOME": os.fspath(profile / "cache"),
            "XDG_CONFIG_HOME": os.fspath(profile / "config"),
            "XDG_DATA_HOME": os.fspath(profile / "data"),
            "https_proxy": proxy,
            "no_proxy": "",
        }
    )
    return environment


def wait_tcp_listener(process: subprocess.Popen[bytes], port: int, timeout: float = 30) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise GateError(f"WebKitWebDriver exited before listening (rc={process.returncode})")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.25):
                return
        except OSError:
            time.sleep(0.2)
    raise GateError("WebKitWebDriver did not open its loopback W3C listener")


def element_text(client: W3CClient, name: str, timeout: float = 30) -> str:
    return client.text(client.wait_element(testid(name), timeout))


def numeric_text(raw: str, label: str, *, minimum: int = 0) -> int:
    digits = re.sub(r"[^0-9]", "", raw)
    if not digits:
        raise GateError(f"{label} did not contain a number: {raw!r}")
    value = int(digits)
    if value < minimum:
        raise GateError(f"{label} was {value}, expected at least {minimum}")
    return value


def terminate_process(process: subprocess.Popen[bytes], label: str) -> bool:
    if process.poll() is not None:
        return False
    process.terminate()
    try:
        process.wait(timeout=15)
        return False
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=10)
        return True


def run_guest(args: argparse.Namespace) -> dict[str, Any]:  # noqa: C901
    started_at = utc_now()
    started_monotonic = time.monotonic()
    runtime_root = args.runtime_root.resolve(strict=True)
    if not runtime_root.is_dir() or runtime_root.parent != Path("/var/tmp"):
        raise GateError("guest runtime root must be one existing direct child of /var/tmp")
    if not runtime_root.name.startswith("arc-packaged-live-v080-"):
        raise GateError("guest runtime root has an unexpected name")
    binding, binding_raw = validate_release_binding(args.binding)
    if args.binding.resolve(strict=True).parent != args.asset_directory.resolve(strict=True):
        raise GateError("guest binding and published assets must share the staged input directory")
    assets = {
        name: verify_bound_asset(binding, args.asset_directory, name)
        for name in (APPIMAGE_NAME, APPIMAGE_SIGNATURE_NAME, NODE_NAME)
    }
    attempt, attempt_raw, prompt = validate_inference_attempt(
        args.attempt, binding, binding_raw, assets
    )
    if (
        args.updater_public_key.resolve(strict=True).parent
        != args.asset_directory.resolve(strict=True)
        or args.updater_public_key.name != "updater-public-key.b64"
    ):
        raise GateError("guest updater public key is outside the exact staged input set")
    if sha256_file(args.updater_public_key) != attempt["plan"]["updater_public_key_sha256"]:
        raise GateError("guest updater public key differs from the armed release plan")
    if sha256_file(Path(__file__).resolve()) != attempt["plan"]["gate_sha256"]:
        raise GateError("guest gate source differs from the armed inference plan")
    private_file(args.proxy_token_file, "run-scoped proxy token")
    token = read_bytes(args.proxy_token_file, "run-scoped proxy token", max_bytes=128).decode(
        "ascii", "strict"
    ).strip()
    if not re.fullmatch(r"[0-9a-f]{64}", token):
        raise GateError("run-scoped proxy token is malformed")
    if args.proxy_host != GUEST_HOST_ALIAS:
        raise GateError("guest proxy host must be Lima's exact host-loopback alias")
    if not (1024 <= args.proxy_port <= 65535):
        raise GateError("guest proxy port is invalid")

    work = runtime_root / "work"
    profile = runtime_root / "profile"
    evidence = runtime_root / "evidence"
    for path in (work, profile, evidence):
        create_dir(path)
    copy_regular_exclusive(args.attempt, evidence / "inference-attempt.json", 0o400)
    for path in (profile / "home", profile / "cache", profile / "config", profile / "data"):
        create_dir(path)

    # Authenticate the raw updater payload before changing its mode or asking
    # its AppImage header to extract anything. `--appimage-extract` executes
    # payload-controlled code and therefore belongs strictly after this gate.
    appimage = args.asset_directory / APPIMAGE_NAME
    runtime = runtime_inventory()
    updater_signature = verify_updater_signature_in_guest(
        args.updater_public_key,
        appimage,
        args.asset_directory / APPIMAGE_SIGNATURE_NAME,
        work,
        evidence,
        runtime,
    )
    if (
        updater_signature["appimage_sha256"] != assets[APPIMAGE_NAME]["sha256"]
        or updater_signature["signature_sha256"]
        != assets[APPIMAGE_SIGNATURE_NAME]["sha256"]
    ):
        raise GateError("guest updater signature verification is not release-asset-bound")

    managed_dir = profile / "home" / ".arc"
    managed_bin_dir = managed_dir / "bin"
    create_dir(managed_dir)
    create_dir(managed_bin_dir)
    managed_node = managed_bin_dir / "arc-node"
    copy_regular_exclusive(args.asset_directory / NODE_NAME, managed_node, 0o500)
    if sha256_file(managed_node) != assets[NODE_NAME]["sha256"]:
        raise GateError("staged managed arc-node differs from the published asset")
    node_version_result = run_command([os.fspath(managed_node), "--version"], timeout=15)
    node_version = (node_version_result.stdout + node_version_result.stderr).decode(
        "utf-8", "replace"
    ).strip()
    if not re.search(r"(?:^|\s)(?:v)?0\.8\.0(?:\s|$)", node_version):
        raise GateError(f"published managed node does not report v0.8.0: {node_version!r}")

    os.chmod(appimage, 0o500)
    extraction = run_command([os.fspath(appimage), "--appimage-extract"], cwd=work, timeout=180)
    create_file(evidence / "appimage-extract.stdout", extraction.stdout, 0o400)
    create_file(evidence / "appimage-extract.stderr", extraction.stderr, 0o400)
    package = inspect_extracted_appimage(work / "squashfs-root")
    app_binary_sha = package["elf"]["sha256"]
    firewall = run_command(["/usr/bin/sudo", "-n", "/usr/sbin/iptables-save"]).stdout
    firewall_v6 = run_command(
        ["/usr/bin/sudo", "-n", "/usr/sbin/ip6tables-save"], check=False
    ).stdout
    firewall_text = (firewall + firewall_v6).decode("utf-8", "strict")
    required_rules = (
        "-P OUTPUT DROP",
        f"-A OUTPUT -d {GUEST_HOST_ALIAS}/32 -p tcp",
        f"--dport {args.proxy_port}",
    )
    if any(rule not in firewall_text for rule in required_rules):
        raise GateError("guest egress firewall is not bound to the run-scoped host relay")
    create_file(evidence / "egress-firewall.txt", firewall + b"\n" + firewall_v6, 0o400)

    driver_port = free_loopback_port()
    environment = sanitized_app_environment(profile, args.proxy_host, args.proxy_port, token)
    driver_stdout_path = evidence / "webkit-driver.stdout"
    driver_stderr_path = evidence / "webkit-driver.stderr"
    driver_stdout = driver_stdout_path.open("xb")
    driver_stderr = driver_stderr_path.open("xb")
    driver: subprocess.Popen[bytes] | None = None
    client = W3CClient("127.0.0.1", driver_port)
    tests: list[dict[str, Any]] = []
    screenshots: list[dict[str, Any]] = []
    app_process: dict[str, Any] | None = None
    node_process: dict[str, Any] | None = None
    raw_receipt: bytes | None = None
    receipt: dict[str, Any] | None = None
    product_binding: dict[str, str] | None = None
    tx_hash: str | None = None
    store_metadata: dict[str, Any] | None = None
    forced_driver_stop = False
    session_close_error: GateError | None = None
    inference_submit_clicks = 0
    try:
        driver = subprocess.Popen(
            [
                "/usr/bin/WebKitWebDriver",
                f"--host=127.0.0.1",
                f"--port={driver_port}",
            ],
            cwd=work,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=driver_stdout,
            stderr=driver_stderr,
            start_new_session=True,
        )
        wait_tcp_listener(driver, driver_port)
        # The W3C capability points at the exact public AppImage, not an
        # unpackaged cargo binary. APPIMAGE_EXTRACT_AND_RUN avoids depending on
        # FUSE while retaining the AppImage's own production entrypoint.
        client.new_session(appimage)
        app_matches = wait_exact_process(EXPECTED_APP_BINARY, app_binary_sha, timeout=45)
        app_process = app_matches[0]
        if client.elements(testid("production-browser-blocker")):
            raise GateError("packaged application rendered the production browser blocker")
        if client.elements(testid("synthetic-preview-banner")):
            raise GateError("packaged application rendered the synthetic preview banner")
        client.wait_element(testid("step-welcome"), 30)
        screenshots.append(client.screenshot(evidence / "01-welcome.png"))
        tests.append({"name": EXPECTED_TESTS[0], "status": "passed"})

        client.click(client.element(testid("btn-continue-welcome")))
        client.wait_element(testid("step-identity"), 15)
        address_raw = element_text(client, "identity-address", 30).strip().lower()
        address = address_raw if address_raw.startswith("0x") else f"0x{address_raw}"
        if not CANONICAL_HASH_RE.fullmatch(address):
            raise GateError("native onboarding identity address is not canonical")
        # This is the final artifact capture before the explicit reveal.  No
        # screenshot, page-source request, execute-script call, or element-text
        # call is made while the recovery phrase is present in the WebView.
        screenshots.append(client.screenshot(evidence / "02-identity-blurred.png"))
        client.click(client.element(testid("btn-reveal-seed")))

        def continue_ready() -> str | None:
            if client.elements(testid("seed-error")):
                raise GateError("native seed reveal failed")
            found = client.elements(testid("btn-continue-identity"))
            if not found:
                return None
            return found[0] if client.attribute(found[0], "disabled") is None else None

        client.click(client.wait(continue_ready, "enabled identity acknowledgement", 30))
        client.wait_element(testid("step-model"), 15)
        client.click(client.element(testid("tier-skip")))
        client.click(client.element(testid("btn-continue-model")))
        client.wait_element(testid("step-launch"), 15)
        screenshots.append(client.screenshot(evidence / "03-observer-launch.png"))
        client.click(client.element(testid("btn-launch")))

        def dashboard_or_error() -> str | None:
            errors = client.elements(testid("launch-error"))
            if errors:
                raise GateError(f"native onboarding launch failed: {client.text(errors[0])[:500]}")
            dashboards = client.elements(testid("dashboard"))
            return dashboards[0] if dashboards else None

        client.wait(dashboard_or_error, "native dashboard after onboarding", 120)
        node_process = wait_exact_process("arc-node", assets[NODE_NAME]["sha256"], timeout=45)[0]
        store_path = profile / "data" / "network.arc.desktop" / "store.json"
        store_info = regular_file(store_path, "native identity store", max_bytes=1024 * 1024)
        if stat.S_IMODE(store_info.st_mode) & 0o077:
            raise GateError("native identity store is group/world accessible")
        store_metadata = {
            "mode": f"{stat.S_IMODE(store_info.st_mode):04o}",
            "sha256": sha256_file(store_path),
            "size": store_info.st_size,
        }
        tests.append(
            {
                "name": EXPECTED_TESTS[1],
                "observation": {
                    "identity_address": address,
                    "managed_node_sha256": node_process["exe_sha256"],
                },
                "status": "passed",
            }
        )

        dashboard_height_text = element_text(client, "node-block-height", 30)
        peers_tile = client.wait_element(f'{testid("stat-peers")} .stat-value', 30)
        peers = numeric_text(client.text(peers_tile), "local node peer count", minimum=0)
        sidebar_status = element_text(client, "sidebar-status", 30)
        if "offline" in sidebar_status.lower() or "stopped" in sidebar_status.lower():
            raise GateError(f"packaged dashboard reports the managed node offline: {sidebar_status}")
        screenshots.append(client.screenshot(evidence / "04-dashboard-native.png"))
        tests.append(
            {
                "name": EXPECTED_TESTS[2],
                "observation": {
                    "local_block_height_text": dashboard_height_text,
                    "local_peers": peers,
                    "sidebar_status": sidebar_status,
                },
                "status": "passed",
            }
        )

        client.click(client.element(testid("nav-inference")))
        client.wait_element(testid("inference-screen"), 15)
        prompt_element = client.element(testid("inference-prompt"))
        client.clear(prompt_element)
        client.send_keys(prompt_element, prompt)
        token_element = client.element(testid("inference-max-tokens"))
        client.clear(token_element)
        client.send_keys(token_element, "8")
        typed_prompt = client.property(prompt_element, "value")
        if typed_prompt != prompt:
            raise GateError("packaged UI prompt property differs from the sealed challenge")
        input_hash = arc_blake3_hash(prompt.encode("utf-8"))
        if inference_submit_clicks != 0:
            raise GateError("inference submit-click budget was already consumed")
        inference_submit_clicks += 1
        client.click(client.element(testid("btn-run-inference")))

        def inference_or_error() -> str | None:
            errors = client.elements(testid("inference-error"))
            if errors:
                raise GateError(f"fresh packaged inference failed: {client.text(errors[0])[:500]}")
            results = client.elements(testid("inference-result"))
            return results[0] if results else None

        result_element = client.wait(
            inference_or_error,
            "fresh packaged inference result",
            INFERENCE_WAIT_SECONDS,
        )
        output = element_text(client, "inference-output", 10).strip()
        if not output or output == "(empty)":
            raise GateError("fresh packaged inference returned no output")
        output_hash = client.attribute(result_element, "data-output-hash")
        model_id = client.attribute(result_element, "data-model-id")
        routed_via = client.attribute(result_element, "data-routed-via")
        coordinator_origin = client.attribute(result_element, "data-coordinator")
        for label, value in (("output hash", output_hash), ("model ID", model_id)):
            if not CANONICAL_HASH_RE.fullmatch(value or ""):
                raise GateError(f"packaged inference {label} is not canonical")
        if coordinator_origin != f"https://{LAX_HOST}":
            raise GateError(
                f"packaged inference origin is not exact LAX: {coordinator_origin!r}"
            )
        community_worker_text = element_text(client, "inference-community-worker", 10)
        coordinator_text = element_text(client, "inference-coordinator", 10)
        if "community worker" not in community_worker_text.lower():
            raise GateError("fresh inference was not assigned to a community worker")
        if "LAX" not in coordinator_text:
            raise GateError(f"fresh inference coordinator was not LAX: {coordinator_text!r}")
        tests.append(
            {
                "name": EXPECTED_TESTS[3],
                "observation": {
                    "coordinator": coordinator_text,
                    "coordinator_origin": coordinator_origin,
                    "input_hash": input_hash,
                    "model_id": model_id,
                    "output_hash": output_hash,
                    "output_sha256": sha256_bytes(output.encode("utf-8")),
                    "prompt_sha256": sha256_bytes(prompt.encode("utf-8")),
                    "attempt_plan_sha256": attempt["plan_sha256"],
                    "worker_label": community_worker_text,
                    "submit_clicks": inference_submit_clicks,
                },
                "status": "passed",
            }
        )

        def mined_success() -> str | None:
            settlements = client.elements(testid("community-settlement"))
            if not settlements:
                return None
            status = client.attribute(settlements[0], "data-receipt-status")
            if status in {"mined_failed", "receipt_unavailable"}:
                raise GateError(f"native reward receipt reached terminal failure: {status}")
            return settlements[0] if status == "mined_success" else None

        settlement = client.wait(
            mined_success,
            "successful mined native reward receipt",
            RECEIPT_UI_WAIT_SECONDS,
        )
        settlement_binding = {
            "tx_type": client.attribute(settlement, "data-tx-type") or "",
            "tx_hash": client.attribute(settlement, "data-tx-hash") or "",
            "job_id": client.attribute(settlement, "data-job-id") or "",
            "worker": client.attribute(settlement, "data-worker") or "",
            "receipt_url": client.attribute(settlement, "data-receipt-url") or "",
        }
        if client.attribute(settlement, "data-submitted") != "true":
            raise GateError("packaged settlement does not prove a submitted 0x25")
        if settlement_binding["tx_type"] != "0x25":
            raise GateError("packaged settlement is not transaction type 0x25")
        for field in ("tx_hash", "job_id", "worker"):
            if not CANONICAL_HASH_RE.fullmatch(settlement_binding[field]):
                raise GateError(f"packaged settlement {field} is not canonical")
        if settlement_binding["receipt_url"] != (
            f"/community/reward_receipt/{settlement_binding['tx_hash']}"
        ):
            raise GateError("packaged settlement receipt URL is not transaction-bound")
        if routed_via != f"community:{settlement_binding['worker']}":
            raise GateError("packaged inference route differs from settlement worker")
        screenshots.append(client.screenshot(evidence / "05-inference-mined-reward.png"))
        tests.append(
            {
                "name": EXPECTED_TESTS[4],
                "observation": dict(settlement_binding),
                "status": "passed",
            }
        )

        client.click(client.element(testid("btn-lookup-reward")))
        client.wait_element(testid("network-screen"), 15)
        lookup_input = client.wait_element(testid("tx-lookup-input"), 15)
        lookup_value = client.property(lookup_input, "value")
        if not isinstance(lookup_value, str):
            lookup_value = client.attribute(lookup_input, "value")
        if not isinstance(lookup_value, str):
            raise GateError("reward lookup did not expose its transaction value")
        tx_hash = lookup_value.strip().lower()
        if not CANONICAL_HASH_RE.fullmatch(tx_hash):
            raise GateError("reward lookup transaction hash is not canonical")
        if tx_hash != settlement_binding["tx_hash"]:
            raise GateError("reward lookup transaction differs from packaged settlement")
        lookup_result: str | None = None
        lookup_reads = 0
        for lookup_reads in range(1, 4):
            def lookup_outcome() -> tuple[str, str] | None:
                results = client.elements(testid("tx-lookup-result"))
                if results:
                    return ("result", results[0])
                errors = client.elements(testid("tx-lookup-error"))
                if errors:
                    return ("error", errors[0])
                return None

            outcome, element_id = client.wait(
                lookup_outcome, "host-scoped reward lookup read", 30
            )
            if outcome == "result":
                lookup_result = element_id
                break
            if lookup_reads < 3:
                client.click(client.element(testid("tx-lookup-submit")))
                time.sleep(1)
        if lookup_result is None:
            raise GateError("host-scoped reward lookup failed after three safe reads")
        if client.attribute(lookup_result, "data-tx-hash") != tx_hash:
            raise GateError("host-scoped lookup result differs from reward transaction")
        lookup_binding = {
            "status": client.attribute(lookup_result, "data-lookup-status") or "",
            "source_host": client.attribute(lookup_result, "data-source-host") or "",
            "block_height": client.attribute(lookup_result, "data-block-height") or "",
            "block_hash": client.attribute(lookup_result, "data-block-hash") or "",
            "tx_index": client.attribute(lookup_result, "data-tx-index") or "",
            "success": client.attribute(lookup_result, "data-success") or "",
        }
        if lookup_binding["status"] != "mined" or lookup_binding["success"] != "true":
            raise GateError("host-scoped lookup does not prove successful mined execution")
        if lookup_binding["source_host"] != f"https://{LAX_HOST}":
            raise GateError("host-scoped lookup was not served by exact LAX")
        if not lookup_binding["block_height"].isdigit() or int(
            lookup_binding["block_height"]
        ) <= 0:
            raise GateError("host-scoped lookup block height is invalid")
        if not CANONICAL_HASH_RE.fullmatch(lookup_binding["block_hash"]):
            raise GateError("host-scoped lookup block hash is invalid")
        if not lookup_binding["tx_index"].isdigit():
            raise GateError("host-scoped lookup transaction index is invalid")
        client.wait_element(testid("tx-status-mined"), 30)
        validators = element_text(client, "net-stat-validators", 30).strip()
        if validators != "6 / 6":
            raise GateError(f"selected LAX host does not report 6 / 6 validators: {validators!r}")
        chain_height = client.wait(
            lambda: (
                value
                if (value := numeric_text(element_text(client, "net-stat-block-height"), "chain height"))
                > 100
                else None
            ),
            "selected LAX chain height above 100",
            45,
        )
        block_rows = client.wait(
            lambda: client.elements(
                f'{testid("block-list")} [data-testid^="block-row-"]'
            ),
            "selected-host recent block rows",
            45,
        )
        receipt, raw_receipt, receipt_reads = fetch_coherent_terminal_receipt(
            args.proxy_host, args.proxy_port, token, tx_hash
        )
        product_binding = {
            **settlement_binding,
            "input_hash": input_hash,
            "model_id": model_id or "",
            "output_hash": output_hash or "",
        }
        validate_receipt_product_binding(receipt, product_binding)
        if (
            int(lookup_binding["block_height"]) != receipt["block_height"]
            or lookup_binding["block_hash"] != receipt["block_hash"]
            or int(lookup_binding["tx_index"]) != receipt["index"]
            or receipt["success"] is not True
        ):
            raise GateError("host-scoped transaction lookup differs from canonical 0x25 receipt")
        receipt_block_rows = []
        for row in block_rows:
            row_height = client.attribute(row, "data-block-height") or ""
            row_hash = client.attribute(row, "data-block-hash") or ""
            if row_height == str(receipt["block_height"]):
                receipt_block_rows.append((row_height, row_hash))
        if receipt_block_rows != [(str(receipt["block_height"]), receipt["block_hash"])]:
            raise GateError("recent block list does not contain the exact reward receipt block")
        if receipt["block_height"] > chain_height:
            raise GateError("reward receipt height is ahead of selected-host network height")
        create_file(evidence / "canonical-reward-receipt.raw.json", raw_receipt, 0o400)
        create_file(evidence / "canonical-reward-receipt.json", canonical_json(receipt), 0o400)
        screenshots.append(client.screenshot(evidence / "06-host-scoped-reward-lookup.png"))
        tests.append(
            {
                "name": EXPECTED_TESTS[5],
                "observation": {
                    "block_height": receipt["block_height"],
                    "block_hash": receipt["block_hash"],
                    "chain_height": chain_height,
                    "lookup": lookup_binding,
                    "recent_block_rows": len(block_rows),
                    "safe_lookup_reads": lookup_reads,
                    "safe_receipt_gets": receipt_reads,
                    "reward_tx_hash": tx_hash,
                    "validators": validators,
                },
                "status": "passed",
            }
        )

        client.click(client.element(testid("nav-earnings")))
        earnings_screen = client.wait_element(testid("earnings-screen"), 15)

        def exact_decimal(raw: str, label: str) -> Decimal:
            if not re.fullmatch(r"(?:0|[1-9][0-9]*)(?:\.[0-9]+)?", raw):
                raise GateError(f"{label} is not a canonical non-negative decimal: {raw!r}")
            try:
                value = Decimal(raw)
            except InvalidOperation as error:
                raise GateError(f"{label} is not a decimal: {raw!r}") from error
            if not value.is_finite() or value < 0:
                raise GateError(f"{label} is not finite and non-negative: {raw!r}")
            return value

        # The same selected-host endpoint feeds both the earnings list and the
        # projection.  Wait through initial React Query loading, then bind the
        # nonsecret DOM attributes rather than trusting formatted card text.
        def coherent_earnings_root() -> dict[str, str] | None:
            from_chain = client.attribute(earnings_screen, "data-from-chain") or ""
            count = client.attribute(
                earnings_screen, "data-confirmed-receipt-count"
            ) or ""
            if from_chain != "true" or not count.isdigit():
                return None
            return {
                "attestation_count": client.attribute(
                    earnings_screen, "data-attestation-count"
                ) or "",
                "confirmed_receipt_count": count,
                "from_chain": from_chain,
                "receipt_source": client.attribute(
                    earnings_screen, "data-receipt-source"
                ) or "",
                "total_arc": client.attribute(earnings_screen, "data-total-arc") or "",
                "unavailable_reason": client.attribute(
                    earnings_screen, "data-unavailable-reason"
                ) or "",
            }

        earnings_binding = client.wait(
            coherent_earnings_root,
            "selected-host confirmed earnings contract",
            120,
        )
        if earnings_binding["receipt_source"] != RETAINED_EARNINGS_SOURCE:
            raise GateError("earnings source is not the exact retained 0x25 receipt index")
        if earnings_binding["unavailable_reason"]:
            raise GateError("fromChain earnings carried a contradictory unavailable reason")
        if not earnings_binding["attestation_count"].isdigit():
            raise GateError("earnings attestation count is not an exact integer")
        confirmed_count = int(earnings_binding["confirmed_receipt_count"])
        attestation_count = int(earnings_binding["attestation_count"])
        if confirmed_count != attestation_count:
            raise GateError("earnings count differs from confirmed receipt count")
        total_arc = exact_decimal(earnings_binding["total_arc"], "earnings total ARC")

        receipt_rows = client.elements(
            f'{testid("confirmed-reward-receipts")} [data-tx-hash]'
        )
        if len(receipt_rows) != confirmed_count:
            raise GateError("rendered confirmed receipt rows differ from the chain count")
        row_bindings: list[dict[str, str]] = []
        row_total = Decimal(0)
        for row in receipt_rows:
            row_binding = {
                field: client.attribute(row, f"data-{field.replace('_', '-')}") or ""
                for field in (
                    "tx_hash",
                    "worker",
                    "job_id",
                    "block_height",
                    "block_hash",
                    "reward_base",
                    "reward_arc",
                    "receipt_url",
                )
            }
            for field in ("tx_hash", "worker", "job_id", "block_hash"):
                if not CANONICAL_HASH_RE.fullmatch(row_binding[field]):
                    raise GateError(f"earnings row {field} is not canonical")
            if not row_binding["block_height"].isdigit() or int(
                row_binding["block_height"]
            ) <= 0:
                raise GateError("earnings row block height is invalid")
            if not row_binding["reward_base"].isdigit():
                raise GateError("earnings row reward base is invalid")
            row_reward = exact_decimal(row_binding["reward_arc"], "earnings row reward ARC")
            if row_binding["receipt_url"] != (
                f"/community/reward_receipt/{row_binding['tx_hash']}"
            ):
                raise GateError("earnings row receipt URL is not transaction-bound")
            row_total += row_reward
            row_bindings.append(row_binding)
        if row_total != total_arc:
            raise GateError("rendered confirmed receipt rewards do not reconcile to total ARC")

        # The new local observer normally requested work that a remote
        # community worker performed.  In that case its exact confirmed gross
        # earnings are zero.  If assignment selected the local identity, the
        # independently verified fresh receipt must instead appear exactly
        # once and match every exposed receipt field.
        local_worker_receipts = [
            row for row in row_bindings if row["tx_hash"] == receipt["tx_hash"]
        ]
        if receipt["worker"] == address:
            if len(local_worker_receipts) != 1:
                raise GateError("local worker's fresh receipt is absent/duplicated in earnings")
            local_row = local_worker_receipts[0]
            expected_local_row = {
                "block_hash": receipt["block_hash"],
                "block_height": str(receipt["block_height"]),
                "job_id": receipt["job_id"],
                "receipt_url": receipt["receipt_url"],
                "reward_arc": str(receipt["reward_arc"]),
                "reward_base": str(receipt["reward_base"]),
                "tx_hash": receipt["tx_hash"],
                "worker": receipt["worker"],
            }
            if local_row != expected_local_row:
                raise GateError("local earnings row differs from the canonical fresh receipt")
            rendered_earnings = "confirmed_fresh_local_worker_receipt"
        else:
            if confirmed_count != 0 or total_arc != 0 or receipt_rows:
                raise GateError("fresh observer identity rendered another worker's earnings")
            client.wait_element(testid("earnings-empty"), 30)
            rendered_earnings = "confirmed_zero_for_fresh_observer_identity"

        projection_card = client.wait_element(testid("projection-card"), 120)
        projection_binding = {
            field: client.attribute(projection_card, f"data-{field.replace('_', '-')}") or ""
            for field in (
                "projection_state",
                "source_host",
                "economics_source_host",
                "projected_daily_arc",
                "unavailable_reason",
                "reward_per_attestation",
                "reward_rate_source",
                "reward_policy_hash",
                "community_rewards_enabled",
                "issuance_ready_for_worker",
                "reward_program",
            )
        }
        if projection_binding["source_host"] != f"https://{LAX_HOST}" or (
            projection_binding["economics_source_host"] != f"https://{LAX_HOST}"
        ):
            raise GateError("earnings projection/economics did not use exact pinned LAX")
        projection_state = projection_binding["projection_state"]
        if projection_state not in {"numeric", "no_rate"}:
            raise GateError(
                "successful reward path did not render a usable active projection contract"
            )
        has_daily_value = bool(projection_binding["projected_daily_arc"])
        has_unavailable_reason = bool(projection_binding["unavailable_reason"].strip())
        if has_daily_value == has_unavailable_reason:
            raise GateError("projection must expose exactly one of value or unavailable reason")
        if (projection_state == "numeric") != has_daily_value:
            raise GateError("projection state disagrees with its numeric/unavailable XOR")
        if projection_binding["reward_rate_source"] != "chain":
            raise GateError("projection reward rate is not attributed to the selected chain")
        if exact_decimal(
            projection_binding["reward_per_attestation"],
            "projection reward per attestation",
        ) != Decimal(str(receipt["reward_arc"])):
            raise GateError("projection reward amount differs from the canonical fresh receipt")
        if not CANONICAL_HASH_RE.fullmatch(projection_binding["reward_policy_hash"]):
            raise GateError("projection reward policy hash is not canonical")
        if projection_binding["community_rewards_enabled"] != "true" or (
            projection_binding["issuance_ready_for_worker"] != "true"
        ):
            raise GateError("projection does not prove active reward issuance for this identity")
        if projection_binding["reward_program"] != (
            "protocol-capped testnet promotional compute subsidy"
        ):
            raise GateError("projection reward program label differs from the chain contract")
        if has_daily_value:
            exact_decimal(
                projection_binding["projected_daily_arc"],
                "projected daily ARC",
            )
        projection_text = client.text(projection_card)
        if not projection_text.strip():
            raise GateError("earnings projection rendered no policy state")
        screenshots.append(client.screenshot(evidence / "07-earnings-projection.png"))
        tests.append(
            {
                "name": EXPECTED_TESTS[6],
                "observation": {
                    "earnings": earnings_binding,
                    "earnings_state": rendered_earnings,
                    "projection": projection_binding,
                    "projection_sha256": sha256_bytes(projection_text.encode("utf-8")),
                    "receipt_rows": row_bindings,
                },
                "status": "passed",
            }
        )

        client.click(client.element(testid("nav-dashboard")))
        client.wait_element(testid("dashboard"), 15)
        client.click(client.wait_element(testid("btn-stop"), 30))
        client.wait_element(testid("btn-start"), 45)
        wait_exact_process("arc-node", assets[NODE_NAME]["sha256"], timeout=45, allow_zero=True)
    finally:
        try:
            client.close()
        except (GateError, OSError) as error:
            session_close_error = GateError(f"WebDriver session did not close cleanly: {error}")
        if driver is not None:
            forced_driver_stop = terminate_process(driver, "WebKitWebDriver")
        driver_stdout.close()
        driver_stderr.close()
        with contextlib.suppress(OSError):
            os.chmod(driver_stdout_path, 0o400)
        with contextlib.suppress(OSError):
            os.chmod(driver_stderr_path, 0o400)

    if session_close_error is not None:
        raise session_close_error
    if forced_driver_stop:
        raise GateError("WebKitWebDriver required forced termination")
    wait_exact_process(EXPECTED_APP_BINARY, app_binary_sha, timeout=30, allow_zero=True)
    transcript_raw = canonical_json(client.transcript)
    create_file(evidence / "w3c-transcript.json", transcript_raw, 0o400)
    if raw_receipt is None or receipt is None or tx_hash is None or product_binding is None:
        raise GateError("successful run omitted canonical reward receipt evidence")
    if inference_submit_clicks != 1:
        raise GateError("packaged live gate did not perform exactly one inference submit click")
    if [row.get("name") for row in tests] != list(EXPECTED_TESTS[:-1]):
        raise GateError("packaged live gate tests are incomplete or out of order")

    # The only secret-bearing files live below this fresh profile.  They are
    # never copied to evidence.  Remove the whole profile only after the node
    # and GUI are both gone; the disposable VM is then deleted by the host.
    shutil.rmtree(profile)
    if profile.exists():
        raise GateError("secret-bearing packaged-app profile was not destroyed")
    tests.append(
        {
            "name": EXPECTED_TESTS[7],
            "observation": {
                "forced_driver_stop": False,
                "profile_removed": True,
                "store_metadata_only": store_metadata,
            },
            "status": "passed",
        }
    )

    driver_stdout_info = driver_stdout_path.lstat()
    driver_stderr_info = driver_stderr_path.lstat()
    if not stat.S_ISREG(driver_stdout_info.st_mode) or not stat.S_ISREG(driver_stderr_info.st_mode):
        raise GateError("WebKit driver logs are not regular files")
    duration_ms = int((time.monotonic() - started_monotonic) * 1000)
    if duration_ms > GUEST_GATE_TIMEOUT_SECONDS * 1000:
        raise GateError("packaged live gate exceeded its sealed total guest budget")
    result = {
        "artifacts": {
            "appimage_extract_stderr": {
                "path": "appimage-extract.stderr",
                "sha256": sha256_file(evidence / "appimage-extract.stderr"),
                "size": (evidence / "appimage-extract.stderr").stat().st_size,
            },
            "appimage_extract_stdout": {
                "path": "appimage-extract.stdout",
                "sha256": sha256_file(evidence / "appimage-extract.stdout"),
                "size": (evidence / "appimage-extract.stdout").stat().st_size,
            },
            "canonical_reward_receipt": {
                "path": "canonical-reward-receipt.json",
                "sha256": sha256_file(evidence / "canonical-reward-receipt.json"),
                "size": (evidence / "canonical-reward-receipt.json").stat().st_size,
            },
            "canonical_reward_receipt_raw": {
                "path": "canonical-reward-receipt.raw.json",
                "sha256": sha256_file(evidence / "canonical-reward-receipt.raw.json"),
                "size": (evidence / "canonical-reward-receipt.raw.json").stat().st_size,
            },
            "inference_attempt": {
                "path": "inference-attempt.json",
                "sha256": sha256_bytes(attempt_raw),
                "size": len(attempt_raw),
            },
            "egress_firewall": {
                "path": "egress-firewall.txt",
                "sha256": sha256_file(evidence / "egress-firewall.txt"),
                "size": (evidence / "egress-firewall.txt").stat().st_size,
            },
            "screenshots": screenshots,
            "updater_signature_stderr": {
                "path": "updater-signature.stderr",
                "sha256": sha256_file(evidence / "updater-signature.stderr"),
                "size": (evidence / "updater-signature.stderr").stat().st_size,
            },
            "updater_signature_stdout": {
                "path": "updater-signature.stdout",
                "sha256": sha256_file(evidence / "updater-signature.stdout"),
                "size": (evidence / "updater-signature.stdout").stat().st_size,
            },
            "w3c_transcript": {
                "entry_count": len(client.transcript),
                "path": "w3c-transcript.json",
                "sha256": sha256_bytes(transcript_raw),
                "size": len(transcript_raw),
            },
            "webkit_driver_stderr": {
                "path": "webkit-driver.stderr",
                "sha256": sha256_file(driver_stderr_path),
                "size": driver_stderr_info.st_size,
            },
            "webkit_driver_stdout": {
                "path": "webkit-driver.stdout",
                "sha256": sha256_file(driver_stdout_path),
                "size": driver_stdout_info.st_size,
            },
        },
        "completed_at": utc_now(),
        "duration_ms": duration_ms,
        "execution": {
            "app_process": app_process,
            "external_webkit_automation_enabled": True,
            "identity_profile_destroyed": True,
            "inference_submit_click_budget": 1,
            "inference_submit_clicks": inference_submit_clicks,
            "inference_wait_seconds": INFERENCE_WAIT_SECONDS,
            "node_process": node_process,
            "page_source_captured": False,
            "production_binary_modified": False,
            "recovery_phrase_element_read": False,
            "recovery_phrase_screenshot_captured": False,
            "shipped_test_plugin_or_server": False,
            "settlement_ui_wait_seconds": RECEIPT_UI_WAIT_SECONDS,
            "total_guest_timeout_seconds": GUEST_GATE_TIMEOUT_SECONDS,
            "webdriver_execute_script_used": False,
            "webdriver": "direct W3C WebKitWebDriver",
        },
        "network": {
            "compiled_origin": f"https://{LAX_HOST}",
            "proxy_host": args.proxy_host,
            "proxy_port": args.proxy_port,
            "proxy_token_sha256": sha256_bytes(token.encode("ascii")),
            "product_binding": product_binding,
            "product_input_hash_algorithm": "BLAKE3 via pinned /usr/bin/b3sum",
            "receipt_body_sha256": sha256_bytes(raw_receipt),
            "receipt_tx_hash": tx_hash,
            "tls_validation": "system trust; acceptInsecureCerts=false",
        },
        "package": package,
        "platform_claim": PLATFORM_CLAIM,
        "release": {
            "assets": assets,
            "binding_sha256": sha256_bytes(binding_raw),
            "commit": binding["commit"],
            "release_id": binding["release"]["id"],
            "repository": binding["repository"],
            "tag": binding["tag"],
        },
        "result": "passed",
        "runtime": runtime,
        "schema": SCHEMA,
        "started_at": started_at,
        "tests": tests,
        "updater_signature": updater_signature,
    }
    if [row["name"] for row in tests] != list(EXPECTED_TESTS):
        raise GateError("packaged live gate final test list differs from the mandatory contract")
    if any(row.get("status") != "passed" for row in tests):
        raise GateError("packaged live gate contains a non-passing test")
    create_file(evidence / "receipt.json", canonical_json(result), 0o400)
    return result


APT_SOURCE = f"""Types: deb
URIs: http://archive.ubuntu.com/ubuntu
Suites: noble noble-updates noble-backports
Components: main universe restricted multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
Snapshot: {APT_SNAPSHOT}

Types: deb
URIs: http://security.ubuntu.com/ubuntu
Suites: noble-security
Components: main universe restricted multiverse
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
Snapshot: {APT_SNAPSHOT}
"""


def require_guest_runtime_root(path: Path) -> Path:
    resolved_parent = path.parent.resolve(strict=True)
    if resolved_parent != Path("/var/tmp"):
        raise GateError("guest runtime must be a direct child of /var/tmp")
    if not path.name.startswith("arc-packaged-live-v080-"):
        raise GateError("guest runtime name is outside the packaged-live namespace")
    return path


def provision_guest(args: argparse.Namespace) -> dict[str, Any]:
    if os.geteuid() != 0:
        raise GateError("guest provisioning must run as root in the disposable VM")
    runtime_root = require_guest_runtime_root(args.runtime_root)
    if not runtime_root.is_dir() or runtime_root.is_symlink():
        raise GateError("guest runtime root is missing or unsafe")
    source_path = Path("/etc/apt/arc-packaged-live.sources")
    if source_path.exists():
        raise GateError("disposable VM already contains packaged-live APT source state")
    create_file(source_path, APT_SOURCE.encode("ascii"), 0o444)
    apt_options = [
        "-o",
        f"Dir::Etc::sourcelist={source_path}",
        "-o",
        "Dir::Etc::sourceparts=-",
        "-o",
        "APT::Get::List-Cleanup=0",
    ]
    environment = dict(os.environ)
    environment["DEBIAN_FRONTEND"] = "noninteractive"
    update = run_command(
        ["/usr/bin/apt-get", *apt_options, "update"], env=environment, timeout=600
    )
    combined_update = (update.stdout + update.stderr).decode("utf-8", "replace")
    if f"snapshot.ubuntu.com/ubuntu/{APT_SNAPSHOT}" not in combined_update:
        raise GateError("APT update did not prove use of the exact Ubuntu snapshot service")
    install = run_command(
        [
            "/usr/bin/apt-get",
            *apt_options,
            "install",
            "-y",
            "--no-install-recommends",
            "--allow-downgrades",
            *APT_PACKAGES,
        ],
        env=environment,
        timeout=900,
    )
    inventory = runtime_inventory()
    result = {
        "apt_snapshot": APT_SNAPSHOT,
        "apt_source_sha256": sha256_bytes(APT_SOURCE.encode("ascii")),
        "install_stderr_sha256": sha256_bytes(install.stderr),
        "install_stdout_sha256": sha256_bytes(install.stdout),
        "runtime": inventory,
        "schema": "arc.packaged-appimage-guest-provision.v1",
        "update_stderr_sha256": sha256_bytes(update.stderr),
        "update_stdout_sha256": sha256_bytes(update.stdout),
    }
    create_file(runtime_root / "guest-provision.json", canonical_json(result), 0o444)
    return result


def lock_guest_egress(args: argparse.Namespace) -> dict[str, Any]:
    if os.geteuid() != 0:
        raise GateError("guest egress lock must run as root in the disposable VM")
    runtime_root = require_guest_runtime_root(args.runtime_root)
    if not runtime_root.is_dir() or runtime_root.is_symlink():
        raise GateError("guest runtime root is missing or unsafe")
    if not (1024 <= args.proxy_port <= 65535):
        raise GateError("guest egress proxy port is invalid")
    iptables = "/usr/sbin/iptables"
    ip6tables = "/usr/sbin/ip6tables"
    commands: list[list[str]] = [
        [iptables, "-F", "OUTPUT"],
        [iptables, "-P", "OUTPUT", "DROP"],
        [iptables, "-A", "OUTPUT", "-o", "lo", "-j", "ACCEPT"],
        [
            iptables,
            "-A",
            "OUTPUT",
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "ACCEPT",
        ],
        [
            iptables,
            "-A",
            "OUTPUT",
            "-d",
            GUEST_HOST_ALIAS,
            "-p",
            "tcp",
            "--dport",
            str(args.proxy_port),
            "-j",
            "ACCEPT",
        ],
    ]
    for host, port in SEED_UDP_ENDPOINTS:
        commands.append(
            [
                iptables,
                "-A",
                "OUTPUT",
                "-d",
                host,
                "-p",
                "udp",
                "--dport",
                str(port),
                "-j",
                "ACCEPT",
            ]
        )
    commands.extend(
        [
            [ip6tables, "-F", "OUTPUT"],
            [ip6tables, "-P", "OUTPUT", "DROP"],
            [ip6tables, "-A", "OUTPUT", "-o", "lo", "-j", "ACCEPT"],
            [
                ip6tables,
                "-A",
                "OUTPUT",
                "-m",
                "conntrack",
                "--ctstate",
                "ESTABLISHED,RELATED",
                "-j",
                "ACCEPT",
            ],
        ]
    )
    for command in commands:
        run_command(command, timeout=30)
    saved = run_command(["/usr/sbin/iptables-save"]).stdout
    saved_v6 = run_command(["/usr/sbin/ip6tables-save"]).stdout
    result = {
        "ipv4_sha256": sha256_bytes(saved),
        "ipv6_sha256": sha256_bytes(saved_v6),
        "proxy_destination": f"{GUEST_HOST_ALIAS}:{args.proxy_port}",
        "schema": "arc.packaged-appimage-egress-lock.v1",
        "seed_udp_endpoints": [f"{host}:{port}" for host, port in SEED_UDP_ENDPOINTS],
        "tcp_default_policy": "DROP",
    }
    create_file(runtime_root / "egress-lock.json", canonical_json(result), 0o444)
    return result


def lima_config() -> bytes:
    return (
        f'''minimumLimaVersion: "{LIMA_VERSION}"
vmType: qemu
os: Linux
arch: x86_64
images:
  - location: "{LIMA_IMAGE_URL}"
    arch: x86_64
    digest: "{LIMA_IMAGE_DIGEST}"
cpus: 4
memory: "8GiB"
disk: "24GiB"
mounts: []
mountType: 9p
mountInotify: false
ssh:
  localPort: 0
  loadDotSSHPubKeys: false
  forwardAgent: false
  forwardX11: false
  forwardX11Trusted: false
upgradePackages: false
containerd:
  system: false
  user: false
plain: false
hostResolver:
  enabled: true
  ipv6: false
propagateProxyEnv: false
caCerts:
  removeDefaults: false
video:
  display: none
'''
    ).encode("ascii")


def executable_identity(path: Path, label: str) -> dict[str, Any]:
    try:
        resolved = path.resolve(strict=True)
    except OSError as error:
        raise GateError(f"cannot resolve {label}: {error}") from error
    info = regular_file(resolved, label, max_bytes=1024 * 1024 * 1024)
    if not (info.st_mode & 0o111):
        raise GateError(f"{label} is not executable")
    return {
        "path": os.fspath(path),
        "resolved_path": os.fspath(resolved),
        "sha256": sha256_file(resolved),
        "size": info.st_size,
    }


def lima_list(limactl: Path) -> dict[str, dict[str, Any]]:
    output = run_command([os.fspath(limactl), "list", "--json"], timeout=30).stdout
    instances: dict[str, dict[str, Any]] = {}
    for line in output.splitlines():
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise GateError("limactl list returned malformed JSON") from error
        if not isinstance(value, dict) or not isinstance(value.get("name"), str):
            raise GateError("limactl list returned an invalid instance")
        name = value["name"]
        if name in instances:
            raise GateError("limactl list returned a duplicate instance name")
        instances[name] = value
    return instances


def stable_lima_state(instances: Mapping[str, Mapping[str, Any]]) -> dict[str, Any]:
    return {
        name: {
            "arch": value.get("arch"),
            "dir": value.get("dir"),
            "protected": value.get("protected"),
            "status": value.get("status"),
            "vmType": value.get("vmType"),
        }
        for name, value in sorted(instances.items())
    }


def verify_source_checkout(repo: Path, binding: Mapping[str, Any]) -> dict[str, Any]:
    head = run_command(["/usr/bin/git", "rev-parse", "HEAD"], cwd=repo).stdout.decode(
        "ascii", "strict"
    ).strip()
    if head != binding["commit"]:
        raise GateError("gate checkout HEAD differs from published release commit")
    status_output = run_command(
        ["/usr/bin/git", "status", "--porcelain=v1", "--untracked-files=all"], cwd=repo
    ).stdout
    if status_output:
        raise GateError("packaged live gate requires a clean exact release checkout")
    relative_script = Path(__file__).resolve().relative_to(repo.resolve(strict=True)).as_posix()
    run_command(["/usr/bin/git", "ls-files", "--error-unmatch", relative_script], cwd=repo)
    committed = run_command(
        ["/usr/bin/git", "show", f"{head}:{relative_script}"], cwd=repo
    ).stdout
    current = read_bytes(Path(__file__).resolve(), "packaged live gate source")
    if committed != current:
        raise GateError("running packaged live gate source differs from release commit")
    return {
        "commit": head,
        "gate_path": relative_script,
        "gate_sha256": sha256_bytes(current),
        "tree_clean": True,
    }


def updater_public_key(repo: Path, commit: str) -> str:
    raw = run_command(
        ["/usr/bin/git", "show", f"{commit}:desktop/src-tauri/tauri.conf.json"], cwd=repo
    ).stdout
    try:
        config = json.loads(raw)
        key = config["plugins"]["updater"]["pubkey"]
    except (json.JSONDecodeError, KeyError, TypeError) as error:
        raise GateError("release commit updater public key is unavailable") from error
    if not isinstance(key, str) or not key.strip():
        raise GateError("release commit updater public key is invalid")
    return key


def start_ssh_tunnel(
    ssh: Path, known_hosts: Path, identity: Path, local_port: int
) -> tuple[subprocess.Popen[bytes], list[str]]:
    options = [
        "-F",
        "/dev/null",
        "-o",
        "BatchMode=yes",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ConnectTimeout=15",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "IdentityAgent=none",
        "-o",
        "KbdInteractiveAuthentication=no",
        "-o",
        "PasswordAuthentication=no",
        "-o",
        "PermitLocalCommand=no",
        "-o",
        "PreferredAuthentications=publickey",
        "-o",
        "ProxyCommand=none",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        f"UserKnownHostsFile={known_hosts}",
        "-i",
        os.fspath(identity),
        "-L",
        f"127.0.0.1:{local_port}:127.0.0.1:{LAX_PORT}",
        "-NT",
        f"{LAX_USER}@{LAX_HOST}",
    ]
    process = subprocess.Popen(
        [os.fspath(ssh), *options],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if process.poll() is not None:
            _, stderr = process.communicate()
            raise GateError(f"host-key-pinned LAX SSH tunnel failed: {stderr.decode('utf-8', 'replace')[-1000:]}")
        try:
            with socket.create_connection(("127.0.0.1", local_port), timeout=0.25):
                return process, options
        except OSError:
            time.sleep(0.2)
    terminate_process(process, "LAX SSH tunnel")
    raise GateError("host-key-pinned LAX SSH tunnel did not open")


def limactl_command(limactl: Path, *arguments: str, timeout: int = 120) -> subprocess.CompletedProcess[bytes]:
    return run_command([os.fspath(limactl), *arguments], timeout=timeout)


def validate_copied_inference_attempt(
    path: Path,
    binding: Mapping[str, Any],
    binding_raw: bytes,
    assets: Mapping[str, Mapping[str, Any]],
    expected_attempt: Mapping[str, Any],
    expected_attempt_raw: bytes,
) -> tuple[dict[str, Any], bytes, str, str]:
    copied_attempt, copied_raw, prompt = validate_inference_attempt(
        path, binding, binding_raw, assets
    )
    if copied_raw != expected_attempt_raw or copied_attempt != expected_attempt:
        raise GateError("guest inference attempt differs from durable host arming")
    input_hash = f"0x{independent_blake3_short(prompt.encode('utf-8'))}"
    return copied_attempt, copied_raw, prompt, input_hash


def validate_guest_evidence(
    evidence: Path,
    binding: Mapping[str, Any],
    binding_raw: bytes,
    expected_attempt: Mapping[str, Any],
    expected_attempt_raw: bytes,
    assets: Mapping[str, Mapping[str, Any]],
) -> tuple[dict[str, Any], bytes]:
    if not evidence.is_dir() or evidence.is_symlink():
        raise GateError("copied guest evidence directory is missing or unsafe")
    expected_files = {
        "01-welcome.png",
        "02-identity-blurred.png",
        "03-observer-launch.png",
        "04-dashboard-native.png",
        "05-inference-mined-reward.png",
        "06-host-scoped-reward-lookup.png",
        "07-earnings-projection.png",
        "appimage-extract.stderr",
        "appimage-extract.stdout",
        "canonical-reward-receipt.json",
        "canonical-reward-receipt.raw.json",
        "egress-firewall.txt",
        "inference-attempt.json",
        "receipt.json",
        "w3c-transcript.json",
        "webkit-driver.stderr",
        "webkit-driver.stdout",
        "updater-signature.stderr",
        "updater-signature.stdout",
    }
    actual_files: set[str] = set()
    for path in evidence.rglob("*"):
        if path.is_symlink() or not path.is_file():
            raise GateError(f"guest evidence contains a non-regular entry: {path}")
        relative = path.relative_to(evidence).as_posix()
        if "/" in relative:
            raise GateError("guest evidence contains an unexpected nested path")
        actual_files.add(relative)
    if actual_files != expected_files:
        raise GateError(
            f"guest evidence files differ; missing={sorted(expected_files - actual_files)!r}, "
            f"unexpected={sorted(actual_files - expected_files)!r}"
        )
    copied_attempt, copied_attempt_raw, prompt, expected_input_hash = (
        validate_copied_inference_attempt(
            evidence / "inference-attempt.json",
            binding,
            binding_raw,
            assets,
            expected_attempt,
            expected_attempt_raw,
        )
    )
    receipt, raw = load_json(evidence / "receipt.json", "packaged AppImage guest receipt")
    if raw != canonical_json(receipt):
        raise GateError("packaged AppImage guest receipt is not canonical JSON")
    require_exact_keys(
        receipt,
        {
            "artifacts",
            "completed_at",
            "duration_ms",
            "execution",
            "network",
            "package",
            "platform_claim",
            "release",
            "result",
            "runtime",
            "schema",
            "started_at",
            "tests",
            "updater_signature",
        },
        "packaged AppImage guest receipt",
    )
    if receipt.get("schema") != SCHEMA or receipt.get("result") != "passed":
        raise GateError("packaged AppImage guest receipt is not a passing supported schema")
    duration_ms = receipt.get("duration_ms")
    if (
        type(duration_ms) is not int  # noqa: E721
        or duration_ms <= 0
        or duration_ms > GUEST_GATE_TIMEOUT_SECONDS * 1000
    ):
        raise GateError("packaged AppImage guest receipt exceeded its total budget")
    if receipt.get("platform_claim") != PLATFORM_CLAIM:
        raise GateError("packaged AppImage guest receipt widened its platform claim")
    tests = receipt.get("tests")
    if not isinstance(tests, list) or [row.get("name") for row in tests if isinstance(row, dict)] != list(
        EXPECTED_TESTS
    ):
        raise GateError("packaged AppImage guest receipt omitted/reordered mandatory tests")
    if any(not isinstance(row, dict) or row.get("status") != "passed" for row in tests):
        raise GateError("packaged AppImage guest receipt contains a skipped/non-passing test")
    release = receipt.get("release")
    if not isinstance(release, dict):
        raise GateError("packaged AppImage guest receipt release binding is missing")
    for field, expected in (
        ("binding_sha256", sha256_bytes(binding_raw)),
        ("commit", binding["commit"]),
        ("release_id", binding["release"]["id"]),
        ("repository", binding["repository"]),
        ("tag", binding["tag"]),
    ):
        if release.get(field) != expected:
            raise GateError(f"packaged AppImage guest receipt release {field} mismatch")
    expected_release = {
        "assets": {name: dict(assets[name]) for name in sorted(assets)},
        "binding_sha256": sha256_bytes(binding_raw),
        "commit": binding["commit"],
        "release_id": binding["release"]["id"],
        "repository": binding["repository"],
        "tag": binding["tag"],
    }
    if release != expected_release:
        raise GateError("packaged AppImage guest release/assets are not exactly bound")
    execution = receipt.get("execution")
    if not isinstance(execution, dict) or any(
        execution.get(field) is not False
        for field in (
            "page_source_captured",
            "production_binary_modified",
            "recovery_phrase_element_read",
            "recovery_phrase_screenshot_captured",
            "shipped_test_plugin_or_server",
            "webdriver_execute_script_used",
        )
    ):
        raise GateError("packaged AppImage guest receipt used/captured a forbidden test seam")
    if execution.get("external_webkit_automation_enabled") is not True:
        raise GateError("packaged AppImage guest receipt hides its external WebKit automation")
    if execution.get("identity_profile_destroyed") is not True:
        raise GateError("packaged AppImage guest receipt retained its secret identity profile")
    if (
        execution.get("inference_submit_click_budget") != 1
        or execution.get("inference_submit_clicks") != 1
    ):
        raise GateError("packaged AppImage guest receipt violated its submit-click budget")
    if (
        execution.get("inference_wait_seconds") != INFERENCE_WAIT_SECONDS
        or execution.get("settlement_ui_wait_seconds") != RECEIPT_UI_WAIT_SECONDS
        or execution.get("total_guest_timeout_seconds") != GUEST_GATE_TIMEOUT_SECONDS
    ):
        raise GateError("packaged AppImage guest receipt weakened its sealed phase budgets")
    runtime = receipt.get("runtime")
    if not isinstance(runtime, dict):
        raise GateError("packaged AppImage runtime inventory is missing")
    require_exact_keys(
        runtime,
        {
            "apt_snapshot",
            "apt_sources",
            "executables",
            "packages",
            "python_version",
            "uname",
            "webkit_driver_version",
        },
        "packaged AppImage runtime inventory",
    )
    if runtime.get("apt_snapshot") != APT_SNAPSHOT:
        raise GateError("packaged AppImage runtime snapshot differs from sealed plan")
    apt_sources = runtime.get("apt_sources")
    expected_apt_source_sha = sha256_bytes(APT_SOURCE.encode("ascii"))
    if (
        not isinstance(apt_sources, dict)
        or apt_sources.get("/etc/apt/arc-packaged-live.sources")
        != expected_apt_source_sha
        or any(
            not isinstance(path, str)
            or not path.startswith("/etc/apt/")
            or not HASH_RE.fullmatch(str(source_sha))
            for path, source_sha in apt_sources.items()
        )
    ):
        raise GateError("packaged AppImage runtime lacks the exact sealed APT source")
    runtime_packages = runtime.get("packages") if isinstance(runtime, dict) else None
    if not isinstance(runtime_packages, dict) or set(runtime_packages) != set(APT_PACKAGES):
        raise GateError("packaged AppImage runtime package provenance is incomplete")
    runtime_executables = runtime.get("executables")
    if not isinstance(runtime_executables, dict) or set(runtime_executables) != set(
        REQUIRED_EXECUTABLES
    ):
        raise GateError("packaged AppImage runtime executable provenance is incomplete")
    for path, identity in runtime_executables.items():
        if (
            not isinstance(identity, dict)
            or set(identity) != {"sha256", "size"}
            or not HASH_RE.fullmatch(str(identity.get("sha256", "")))
            or type(identity.get("size")) is not int  # noqa: E721
            or identity["size"] <= 0
        ):
            raise GateError(f"packaged AppImage runtime executable identity is invalid: {path}")
    for package_name, identity in runtime_packages.items():
        if (
            not isinstance(identity, dict)
            or set(identity) != {"architecture", "version"}
            or not str(identity.get("architecture", "")).strip()
            or not str(identity.get("version", "")).strip()
        ):
            raise GateError(f"packaged AppImage package identity is invalid: {package_name}")
    b3sum_identity = runtime_executables.get("/usr/bin/b3sum")
    if not isinstance(b3sum_identity, dict) or not HASH_RE.fullmatch(
        str(b3sum_identity.get("sha256", ""))
    ):
        raise GateError("packaged AppImage BLAKE3 implementation is not provenance-bound")
    updater_signature = receipt.get("updater_signature")
    if not isinstance(updater_signature, dict):
        raise GateError("packaged AppImage updater signature evidence is missing")
    require_exact_keys(
        updater_signature,
        {
            "appimage_sha256",
            "minisign_binary",
            "minisign_package",
            "public_key_sha256",
            "schema",
            "signature_sha256",
            "stderr_sha256",
            "stdout_sha256",
            "updater_signature_verified",
        },
        "packaged AppImage updater signature evidence",
    )
    if (
        updater_signature.get("schema") != "arc.packaged-appimage-updater-signature.v1"
        or updater_signature.get("updater_signature_verified") is not True
        or updater_signature.get("appimage_sha256")
        != binding["assets"][APPIMAGE_NAME]["sha256"]
        or updater_signature.get("signature_sha256")
        != binding["assets"][APPIMAGE_SIGNATURE_NAME]["sha256"]
        or updater_signature.get("minisign_binary")
        != runtime_executables.get("/usr/bin/minisign")
        or updater_signature.get("minisign_package") != runtime_packages.get("minisign")
    ):
        raise GateError("packaged AppImage updater signature proof is not runtime/release-bound")
    if updater_signature.get("public_key_sha256") != expected_attempt["plan"].get(
        "updater_public_key_sha256"
    ):
        raise GateError("packaged AppImage updater key differs from durable host plan")
    package = receipt.get("package")
    if not isinstance(package, dict):
        raise GateError("packaged AppImage extracted package identity is missing")
    require_exact_keys(
        package,
        {
            "app_run",
            "desktop_metadata",
            "elf",
            "entry_count",
            "tree_limits",
            "tree_sha256",
        },
        "packaged AppImage extracted package identity",
    )
    expected_tree_limits = {
        "max_depth": MAX_PACKAGE_DEPTH,
        "max_entries": MAX_PACKAGE_TREE_ENTRIES,
        "max_file_bytes": MAX_PACKAGE_FILE_BYTES,
        "max_path_bytes": MAX_PACKAGE_PATH_BYTES,
        "max_total_file_bytes": MAX_PACKAGE_TREE_TOTAL_BYTES,
    }
    app_run = package.get("app_run")
    desktop_metadata = package.get("desktop_metadata")
    elf = package.get("elf")
    require_exact_keys(app_run, {"kind", "sha256", "target"}, "AppImage AppRun identity")
    require_exact_keys(
        desktop_metadata,
        {"exec", "name", "path", "sha256"},
        "AppImage desktop metadata identity",
    )
    require_exact_keys(
        elf,
        {"build_id", "file", "machine", "sha256"},
        "AppImage ELF identity",
    )
    app_target = PurePosixPath(str(app_run.get("target", "")))
    desktop_path = PurePosixPath(str(desktop_metadata.get("path", "")))
    if (
        app_run.get("kind") not in {"file", "symlink"}
        or app_target.is_absolute()
        or ".." in app_target.parts
        or app_target.name != EXPECTED_APP_BINARY
        or app_run.get("sha256") != elf.get("sha256")
        or desktop_metadata.get("name") != "ARC Node"
        or EXPECTED_APP_BINARY not in str(desktop_metadata.get("exec", ""))
        or desktop_path.is_absolute()
        or ".." in desktop_path.parts
        or desktop_path.suffix != ".desktop"
        or not HASH_RE.fullmatch(str(desktop_metadata.get("sha256", "")))
        or elf.get("machine") != "Advanced Micro Devices X86-64"
        or "ELF 64-bit" not in str(elf.get("file", ""))
        or "x86-64" not in str(elf.get("file", ""))
        or not re.fullmatch(r"[0-9a-f]+", str(elf.get("build_id", "")))
        or not HASH_RE.fullmatch(str(elf.get("sha256", "")))
        or type(package.get("entry_count")) is not int  # noqa: E721
        or not 1 <= package["entry_count"] <= MAX_PACKAGE_TREE_ENTRIES
        or package.get("tree_limits") != expected_tree_limits
        or not HASH_RE.fullmatch(str(package.get("tree_sha256", "")))
    ):
        raise GateError("packaged AppImage extracted identity is malformed or unbounded")
    network = receipt.get("network")
    if not isinstance(network, dict) or network.get("compiled_origin") != f"https://{LAX_HOST}":
        raise GateError("packaged AppImage guest receipt was not bound to compiled LAX HTTPS")
    if network.get("tls_validation") != "system trust; acceptInsecureCerts=false":
        raise GateError("packaged AppImage guest receipt weakened TLS validation")
    raw_receipt = read_bytes(
        evidence / "canonical-reward-receipt.raw.json",
        "raw canonical reward receipt",
        max_bytes=MAX_RECEIPT_BYTES,
    )
    if sha256_bytes(raw_receipt) != network.get("receipt_body_sha256"):
        raise GateError("raw reward receipt bytes differ from the guest network binding")
    try:
        parsed_raw = json.loads(raw_receipt)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise GateError("raw canonical reward receipt evidence is invalid JSON") from error
    validated = validate_terminal_receipt(parsed_raw, network.get("receipt_tx_hash"))
    product_binding = network.get("product_binding")
    if not isinstance(product_binding, dict):
        raise GateError("packaged AppImage receipt lacks the UI product binding")
    if network.get("product_input_hash_algorithm") != "BLAKE3 via pinned /usr/bin/b3sum":
        raise GateError("packaged AppImage input hash algorithm is unbound")
    if product_binding.get("input_hash") != expected_input_hash:
        raise GateError(
            "packaged AppImage input hash differs from the independently hashed sealed prompt"
        )
    validate_receipt_product_binding(validated, product_binding)
    canonical_receipt = read_bytes(
        evidence / "canonical-reward-receipt.json", "canonical reward receipt"
    )
    if canonical_receipt != canonical_json(validated):
        raise GateError("canonical reward receipt evidence differs from raw public bytes")
    artifacts = receipt.get("artifacts")
    if not isinstance(artifacts, dict):
        raise GateError("packaged AppImage guest artifact binding is missing")
    referenced: dict[str, Mapping[str, Any]] = {}
    for key in (
        "appimage_extract_stderr",
        "appimage_extract_stdout",
        "canonical_reward_receipt",
        "canonical_reward_receipt_raw",
        "egress_firewall",
        "inference_attempt",
        "updater_signature_stderr",
        "updater_signature_stdout",
        "w3c_transcript",
        "webkit_driver_stderr",
        "webkit_driver_stdout",
    ):
        item = artifacts.get(key)
        if not isinstance(item, dict) or not isinstance(item.get("path"), str):
            raise GateError(f"packaged AppImage guest artifact {key} is malformed")
        referenced[item["path"]] = item
    screenshots = artifacts.get("screenshots")
    if not isinstance(screenshots, list) or [item.get("name") for item in screenshots] != [
        f"0{index}-{name}.png"
        for index, name in enumerate(
            (
                "welcome",
                "identity-blurred",
                "observer-launch",
                "dashboard-native",
                "inference-mined-reward",
                "host-scoped-reward-lookup",
                "earnings-projection",
            ),
            start=1,
        )
    ]:
        raise GateError("packaged AppImage guest screenshot set differs")
    for item in screenshots:
        referenced[item["name"]] = item
    for name, item in referenced.items():
        path = evidence / name
        info = path.stat()
        if item.get("sha256") != sha256_file(path) or item.get("size") != info.st_size:
            raise GateError(f"packaged AppImage guest artifact digest/size mismatch: {name}")
    attempt_artifact = artifacts.get("inference_attempt")
    if (
        not isinstance(attempt_artifact, dict)
        or attempt_artifact.get("path") != "inference-attempt.json"
        or attempt_artifact.get("sha256") != sha256_bytes(expected_attempt_raw)
        or attempt_artifact.get("size") != len(expected_attempt_raw)
    ):
        raise GateError("guest attempt artifact is not bound to durable host arming")
    for stream in ("stdout", "stderr"):
        if updater_signature.get(f"{stream}_sha256") != artifacts[
            f"updater_signature_{stream}"
        ].get("sha256"):
            raise GateError("updater signature result differs from its bound command log")
    validate_test_observations(
        tests,
        validated,
        product_binding,
        expected_attempt,
        package,
        execution,
        assets,
    )
    return receipt, raw


def validate_w3c_transcript(path: Path) -> list[dict[str, Any]]:
    raw = read_bytes(path, "sanitized W3C transcript")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise GateError("sanitized W3C transcript is invalid JSON") from error
    if raw != canonical_json(value) or not isinstance(value, list) or not value:
        raise GateError("sanitized W3C transcript is empty/non-canonical")
    for row in value:
        if not isinstance(row, dict):
            raise GateError("sanitized W3C transcript row is not an object")
        require_exact_keys(
            row,
            {
                "duration_ms",
                "method",
                "path",
                "request_sha256",
                "response_bytes",
                "response_sha256",
                "status",
            },
            "sanitized W3C transcript row",
        )
        path_value = str(row.get("path", ""))
        if "/source" in path_value or "/execute" in path_value:
            raise GateError("sanitized W3C transcript contains forbidden page-source/script access")
        if not HASH_RE.fullmatch(str(row.get("request_sha256", ""))) or not HASH_RE.fullmatch(
            str(row.get("response_sha256", ""))
        ):
            raise GateError("sanitized W3C transcript has malformed body digests")
        if type(row.get("status")) is not int or not 200 <= row["status"] < 300:  # noqa: E721
            raise GateError("sanitized W3C transcript contains a failed W3C request")
    if value[0].get("method") != "POST" or value[0].get("path") != "/session":
        raise GateError("sanitized W3C transcript does not begin with packaged session creation")
    if value[-1].get("method") != "DELETE" or not str(value[-1].get("path", "")).startswith(
        "/session/"
    ):
        raise GateError("sanitized W3C transcript does not end with session deletion")
    return value


def copy_guest_evidence(limactl: Path, vm_name: str, guest_runtime: str, output: Path) -> Path:
    destination = output / "guest-evidence"
    if destination.exists():
        raise GateError("guest evidence destination already exists")
    limactl_command(
        limactl,
        "copy",
        "--backend=scp",
        "--recursive",
        f"{vm_name}:{guest_runtime}/evidence",
        os.fspath(destination),
        timeout=180,
    )
    if (destination / "receipt.json").is_file():
        return destination
    # scp implementations sometimes preserve the source basename beneath the
    # requested directory. Accept only that one unambiguous shape.
    nested = destination / "evidence"
    if nested.is_dir() and (nested / "receipt.json").is_file():
        return nested
    raise GateError("limactl did not copy the guest evidence directory")


def validate_test_observations(
    tests: Sequence[Mapping[str, Any]],
    receipt: Mapping[str, Any],
    product_binding: Mapping[str, Any],
    expected_attempt: Mapping[str, Any],
    package: Mapping[str, Any],
    execution: Mapping[str, Any],
    expected_assets: Mapping[str, Mapping[str, Any]],
) -> None:
    """Reject passed labels that are not backed by exact product observations."""

    for index, row in enumerate(tests):
        expected_keys = {"name", "status"} if index == 0 else {
            "name",
            "observation",
            "status",
        }
        require_exact_keys(row, expected_keys, f"packaged AppImage test {index}")
    observations = [row.get("observation") for row in tests]
    if observations[0] is not None:
        raise GateError("AppImage entrypoint test must not carry unvalidated observations")

    identity = observations[1]
    require_exact_keys(
        identity,
        {"identity_address", "managed_node_sha256"},
        "packaged identity observation",
    )
    if not CANONICAL_HASH_RE.fullmatch(str(identity.get("identity_address", ""))) or (
        identity.get("managed_node_sha256") != expected_assets[NODE_NAME]["sha256"]
    ):
        raise GateError("packaged identity/node observation is malformed or unbound")

    dashboard = observations[2]
    require_exact_keys(
        dashboard,
        {"local_block_height_text", "local_peers", "sidebar_status"},
        "packaged dashboard observation",
    )
    if not str(dashboard.get("local_block_height_text", "")).strip() or (
        type(dashboard.get("local_peers")) is not int  # noqa: E721
        or dashboard["local_peers"] < 0
    ) or any(
        word in str(dashboard.get("sidebar_status", "")).lower()
        for word in ("offline", "stopped")
    ):
        raise GateError("packaged dashboard observation is not a live native state")

    inference = observations[3]
    require_exact_keys(
        inference,
        {
            "attempt_plan_sha256",
            "coordinator",
            "coordinator_origin",
            "input_hash",
            "model_id",
            "output_hash",
            "output_sha256",
            "prompt_sha256",
            "submit_clicks",
            "worker_label",
        },
        "packaged inference observation",
    )
    if (
        inference.get("attempt_plan_sha256") != expected_attempt["plan_sha256"]
        or inference.get("prompt_sha256") != expected_attempt["prompt_sha256"]
        or inference.get("coordinator_origin") != f"https://{LAX_HOST}"
        or "LAX" not in str(inference.get("coordinator", ""))
        or "community worker" not in str(inference.get("worker_label", "")).lower()
        or inference.get("submit_clicks") != 1
        or not HASH_RE.fullmatch(str(inference.get("output_sha256", "")))
    ):
        raise GateError("packaged inference observation is incomplete or unbound")
    for field in ("input_hash", "model_id", "output_hash"):
        if inference.get(field) != product_binding.get(field):
            raise GateError(f"packaged inference observation {field} differs from product binding")

    settlement = observations[4]
    require_exact_keys(
        settlement,
        {"job_id", "receipt_url", "tx_hash", "tx_type", "worker"},
        "packaged settlement observation",
    )
    for field in ("job_id", "receipt_url", "tx_hash", "tx_type", "worker"):
        if settlement.get(field) != product_binding.get(field):
            raise GateError(f"packaged settlement observation {field} differs from product binding")

    network = observations[5]
    require_exact_keys(
        network,
        {
            "block_hash",
            "block_height",
            "chain_height",
            "lookup",
            "recent_block_rows",
            "reward_tx_hash",
            "safe_lookup_reads",
            "safe_receipt_gets",
            "validators",
        },
        "packaged network observation",
    )
    lookup = network.get("lookup")
    require_exact_keys(
        lookup,
        {"block_hash", "block_height", "source_host", "status", "success", "tx_index"},
        "packaged transaction lookup observation",
    )
    if (
        network.get("block_hash") != receipt["block_hash"]
        or network.get("block_height") != receipt["block_height"]
        or network.get("reward_tx_hash") != receipt["tx_hash"]
        or lookup.get("block_hash") != receipt["block_hash"]
        or lookup.get("block_height") != str(receipt["block_height"])
        or lookup.get("tx_index") != str(receipt["index"])
        or lookup.get("source_host") != f"https://{LAX_HOST}"
        or lookup.get("status") != "mined"
        or lookup.get("success") != "true"
        or network.get("validators") != "6 / 6"
        or type(network.get("chain_height")) is not int  # noqa: E721
        or network["chain_height"] < receipt["block_height"]
        or type(network.get("recent_block_rows")) is not int  # noqa: E721
        or network["recent_block_rows"] <= 0
        or type(network.get("safe_lookup_reads")) is not int  # noqa: E721
        or not 1 <= network["safe_lookup_reads"] <= 3
        or type(network.get("safe_receipt_gets")) is not int  # noqa: E721
        or not 2 <= network["safe_receipt_gets"] <= 6
    ):
        raise GateError("packaged network observation differs from canonical receipt truth")

    earnings_test = observations[6]
    require_exact_keys(
        earnings_test,
        {"earnings", "earnings_state", "projection", "projection_sha256", "receipt_rows"},
        "packaged earnings observation",
    )
    earnings = earnings_test.get("earnings")
    require_exact_keys(
        earnings,
        {
            "attestation_count",
            "confirmed_receipt_count",
            "from_chain",
            "receipt_source",
            "total_arc",
            "unavailable_reason",
        },
        "packaged earnings binding",
    )
    if (
        earnings.get("from_chain") != "true"
        or earnings.get("receipt_source") != RETAINED_EARNINGS_SOURCE
        or earnings.get("unavailable_reason") != ""
        or not str(earnings.get("confirmed_receipt_count", "")).isdigit()
        or earnings.get("attestation_count") != earnings.get("confirmed_receipt_count")
    ):
        raise GateError("packaged earnings binding is unavailable, inconsistent, or unbound")
    try:
        earnings_total = Decimal(str(earnings.get("total_arc")))
    except InvalidOperation as error:
        raise GateError("packaged earnings total is not numeric") from error
    rows = earnings_test.get("receipt_rows")
    if not isinstance(rows, list) or len(rows) != int(earnings["confirmed_receipt_count"]):
        raise GateError("packaged earnings rows differ from confirmed count")
    row_total = Decimal(0)
    for row in rows:
        require_exact_keys(
            row,
            {
                "block_hash",
                "block_height",
                "job_id",
                "receipt_url",
                "reward_arc",
                "reward_base",
                "tx_hash",
                "worker",
            },
            "packaged confirmed earnings row",
        )
        if any(
            not CANONICAL_HASH_RE.fullmatch(str(row.get(field, "")))
            for field in ("block_hash", "job_id", "tx_hash", "worker")
        ):
            raise GateError("packaged confirmed earnings row contains a malformed identity")
        try:
            row_total += Decimal(str(row["reward_arc"]))
        except InvalidOperation as error:
            raise GateError("packaged earnings row reward is not numeric") from error
    if not earnings_total.is_finite() or earnings_total < 0 or row_total != earnings_total:
        raise GateError("packaged earnings rows do not reconcile to the displayed total")
    identity_address = identity["identity_address"]
    if receipt["worker"] == identity_address:
        matching = [row for row in rows if row.get("tx_hash") == receipt["tx_hash"]]
        if earnings_test.get("earnings_state") != "confirmed_fresh_local_worker_receipt" or (
            len(matching) != 1
        ):
            raise GateError("packaged local-worker earnings do not contain the fresh receipt")
        expected_row = {
            "block_hash": receipt["block_hash"],
            "block_height": str(receipt["block_height"]),
            "job_id": receipt["job_id"],
            "receipt_url": receipt["receipt_url"],
            "reward_arc": str(receipt["reward_arc"]),
            "reward_base": str(receipt["reward_base"]),
            "tx_hash": receipt["tx_hash"],
            "worker": receipt["worker"],
        }
        if matching[0] != expected_row:
            raise GateError("packaged local-worker earnings row differs from fresh receipt")
    elif (
        earnings_test.get("earnings_state")
        != "confirmed_zero_for_fresh_observer_identity"
        or rows
        or earnings_total != 0
    ):
        raise GateError("packaged observer identity claimed another worker's earnings")

    projection = earnings_test.get("projection")
    require_exact_keys(
        projection,
        {
            "community_rewards_enabled",
            "economics_source_host",
            "issuance_ready_for_worker",
            "projected_daily_arc",
            "projection_state",
            "reward_per_attestation",
            "reward_policy_hash",
            "reward_program",
            "reward_rate_source",
            "source_host",
            "unavailable_reason",
        },
        "packaged projection binding",
    )
    has_value = bool(projection.get("projected_daily_arc"))
    has_reason = bool(str(projection.get("unavailable_reason", "")).strip())
    if (
        projection.get("source_host") != f"https://{LAX_HOST}"
        or projection.get("economics_source_host") != f"https://{LAX_HOST}"
        or projection.get("projection_state") not in {"numeric", "no_rate"}
        or has_value == has_reason
        or (projection.get("projection_state") == "numeric") != has_value
        or projection.get("reward_rate_source") != "chain"
        or Decimal(str(projection.get("reward_per_attestation")))
        != Decimal(str(receipt["reward_arc"]))
        or not CANONICAL_HASH_RE.fullmatch(str(projection.get("reward_policy_hash", "")))
        or projection.get("community_rewards_enabled") != "true"
        or projection.get("issuance_ready_for_worker") != "true"
        or projection.get("reward_program")
        != "protocol-capped testnet promotional compute subsidy"
        or not HASH_RE.fullmatch(str(earnings_test.get("projection_sha256", "")))
    ):
        raise GateError("packaged projection binding is inconsistent or unbound")

    cleanup = observations[7]
    require_exact_keys(
        cleanup,
        {"forced_driver_stop", "profile_removed", "store_metadata_only"},
        "packaged cleanup observation",
    )
    store = cleanup.get("store_metadata_only")
    require_exact_keys(store, {"mode", "sha256", "size"}, "packaged store metadata")
    if (
        cleanup.get("forced_driver_stop") is not False
        or cleanup.get("profile_removed") is not True
        or not re.fullmatch(r"0[0-7]{3}", str(store.get("mode", "")))
        or int(str(store["mode"]), 8) & 0o077
        or not HASH_RE.fullmatch(str(store.get("sha256", "")))
        or type(store.get("size")) is not int  # noqa: E721
        or store["size"] <= 0
    ):
        raise GateError("packaged profile cleanup/store observation is not durable/private")

    require_exact_keys(
        execution,
        {
            "app_process",
            "external_webkit_automation_enabled",
            "identity_profile_destroyed",
            "inference_submit_click_budget",
            "inference_submit_clicks",
            "inference_wait_seconds",
            "node_process",
            "page_source_captured",
            "production_binary_modified",
            "recovery_phrase_element_read",
            "recovery_phrase_screenshot_captured",
            "shipped_test_plugin_or_server",
            "settlement_ui_wait_seconds",
            "total_guest_timeout_seconds",
            "webdriver",
            "webdriver_execute_script_used",
        },
        "packaged execution identity",
    )
    for process_name, expected_sha in (
        ("app_process", package.get("elf", {}).get("sha256")),
        ("node_process", expected_assets[NODE_NAME]["sha256"]),
    ):
        process = execution.get(process_name)
        require_exact_keys(
            process,
            {"exe", "exe_sha256", "pid", "start_ticks"},
            f"packaged {process_name}",
        )
        if (
            process.get("exe_sha256") != expected_sha
            or not Path(str(process.get("exe", ""))).is_absolute()
            or type(process.get("pid")) is not int  # noqa: E721
            or process["pid"] <= 0
            or type(process.get("start_ticks")) is not int  # noqa: E721
            or process["start_ticks"] <= 0
        ):
            raise GateError(f"packaged {process_name} is not executable-byte bound")


def run_host(args: argparse.Namespace) -> dict[str, Any]:  # noqa: C901
    if platform.system() != "Darwin":
        raise GateError("host orchestration is intentionally limited to the audited macOS Lima host")
    if not args.limactl.is_absolute() or not args.ssh.is_absolute():
        raise GateError("host limactl and OpenSSH executable paths must be absolute")
    repo = Path(__file__).resolve().parents[2]
    binding, binding_raw = validate_release_binding(args.binding)
    source = verify_source_checkout(repo, binding)
    if not args.output.is_absolute() or args.output.exists():
        raise GateError("host output must be an absolute path that does not exist")
    if not args.output.parent.is_dir():
        raise GateError("host output parent does not exist")
    assets = {
        name: verify_bound_asset(binding, args.asset_directory, name)
        for name in (APPIMAGE_NAME, APPIMAGE_SIGNATURE_NAME, NODE_NAME)
    }
    private_file(args.known_hosts, "production SSH known_hosts")
    private_file(args.identity, "production SSH identity")
    limactl_id = executable_identity(args.limactl, "limactl")
    ssh_id = executable_identity(args.ssh, "OpenSSH client")
    lima_version_output = run_command([os.fspath(args.limactl), "--version"]).stdout.decode(
        "utf-8", "strict"
    ).strip()
    if lima_version_output != f"limactl version {LIMA_VERSION}":
        raise GateError(f"limactl version mismatch: {lima_version_output!r}")
    initial_instances = lima_list(args.limactl)
    initial_stable = stable_lima_state(initial_instances)
    vm_name = args.vm_name or (
        f"arc-packaged-live-v080-{binding['commit'][:8]}-{os.urandom(3).hex()}"
    )
    if not VM_NAME_RE.fullmatch(vm_name):
        raise GateError("disposable VM name is outside the exact packaged-live namespace")
    if vm_name in initial_instances:
        raise GateError("disposable packaged-live VM name already exists")

    # Durably persist the one-shot output root before arming the inference.
    # A host crash must never erase the directory entry and make reusing the
    # same output/challenge path appear safe.
    create_durable_host_output_dir(args.output)
    config_raw = lima_config()
    config_path = args.output / "lima.yaml"
    create_file(config_path, config_raw, 0o400)
    run_root = Path(tempfile.mkdtemp(prefix=f".{vm_name}.", dir=args.output.parent))
    os.chmod(run_root, 0o700)
    token = os.urandom(32).hex()
    token_path = run_root / "proxy-token"
    create_file(token_path, (token + "\n").encode("ascii"), 0o400)

    pubkey = updater_public_key(repo, binding["commit"])
    canonical_pubkey = canonical_updater_public_key_bytes(pubkey)
    pubkey_path = run_root / "updater-public-key.b64"
    create_file(pubkey_path, canonical_pubkey, 0o400)
    attempt, attempt_prompt = build_inference_attempt(
        binding, binding_raw, assets, source, pubkey
    )
    attempt_raw = canonical_json(attempt)
    attempt_path = args.output / "inference-attempt.json"
    # This is intentionally armed on durable host storage before the guest can
    # click Run inference. Any later error is terminal for this challenge and
    # output directory; host-run refuses to reuse either by requiring a new,
    # absent output path.
    create_file(attempt_path, attempt_raw, 0o400)
    if sha256_bytes(attempt_prompt.encode("utf-8")) != attempt["prompt_sha256"]:
        raise GateError("internal armed inference prompt binding failed")

    vm_created = False
    ssh_process: subprocess.Popen[bytes] | None = None
    ssh_options: list[str] | None = None
    ssh_stdout = b""
    ssh_stderr = b""
    relay: RestrictedConnectRelay | None = None
    relay_summary: dict[str, Any] | None = None
    guest_receipt: dict[str, Any] | None = None
    guest_receipt_raw: bytes | None = None
    primary_error: BaseException | None = None
    cleanup_errors: list[str] = []
    guest_runtime = f"/var/tmp/{vm_name}"
    try:
        limactl_command(
            args.limactl, "create", f"--name={vm_name}", "--tty=false", os.fspath(config_path), timeout=300
        )
        vm_created = True
        limactl_command(args.limactl, "start", "--tty=false", vm_name, timeout=600)
        target = lima_list(args.limactl).get(vm_name)
        if not target or target.get("protected") is not False or target.get("arch") != "x86_64":
            raise GateError("new disposable VM identity/protection/architecture mismatch")
        limactl_command(
            args.limactl,
            "shell",
            vm_name,
            "--",
            "/usr/bin/install",
            "-d",
            "-m",
            "0700",
            guest_runtime,
            f"{guest_runtime}/input",
            timeout=60,
        )
        staged = (
            (Path(__file__).resolve(), f"{guest_runtime}/packaged-appimage-live-gate.py"),
            (args.binding, f"{guest_runtime}/input/release-binding.json"),
            (args.asset_directory / APPIMAGE_NAME, f"{guest_runtime}/input/{APPIMAGE_NAME}"),
            (
                args.asset_directory / APPIMAGE_SIGNATURE_NAME,
                f"{guest_runtime}/input/{APPIMAGE_SIGNATURE_NAME}",
            ),
            (args.asset_directory / NODE_NAME, f"{guest_runtime}/input/{NODE_NAME}"),
            (pubkey_path, f"{guest_runtime}/input/updater-public-key.b64"),
            (token_path, f"{guest_runtime}/input/proxy-token"),
            (attempt_path, f"{guest_runtime}/input/inference-attempt.json"),
        )
        for local, remote in staged:
            limactl_command(
                args.limactl,
                "copy",
                "--backend=scp",
                os.fspath(local),
                f"{vm_name}:{remote}",
                timeout=180,
            )
        limactl_command(
            args.limactl,
            "shell",
            vm_name,
            "--",
            "/bin/chmod",
            "0500",
            f"{guest_runtime}/packaged-appimage-live-gate.py",
            f"{guest_runtime}/input/{NODE_NAME}",
        )
        limactl_command(
            args.limactl,
            "shell",
            vm_name,
            "--",
            "/bin/chmod",
            "0400",
            f"{guest_runtime}/input/release-binding.json",
            f"{guest_runtime}/input/{APPIMAGE_NAME}",
            f"{guest_runtime}/input/{APPIMAGE_SIGNATURE_NAME}",
            f"{guest_runtime}/input/updater-public-key.b64",
            f"{guest_runtime}/input/proxy-token",
            f"{guest_runtime}/input/inference-attempt.json",
        )
        limactl_command(
            args.limactl,
            "shell",
            vm_name,
            "--",
            "/usr/bin/sudo",
            "-n",
            "/usr/bin/python3",
            f"{guest_runtime}/packaged-appimage-live-gate.py",
            "provision-guest",
            "--runtime-root",
            guest_runtime,
            timeout=1200,
        )

        tunnel_port = free_loopback_port()
        ssh_process, ssh_options = start_ssh_tunnel(
            args.ssh, args.known_hosts, args.identity, tunnel_port
        )
        relay = RestrictedConnectRelay(tunnel_port, token)
        relay.start()
        limactl_command(
            args.limactl,
            "shell",
            vm_name,
            "--",
            "/usr/bin/sudo",
            "-n",
            "/usr/bin/python3",
            f"{guest_runtime}/packaged-appimage-live-gate.py",
            "lock-egress",
            "--runtime-root",
            guest_runtime,
            "--proxy-port",
            str(relay.port),
            timeout=120,
        )
        limactl_command(
            args.limactl,
            "shell",
            vm_name,
            "--",
            "/usr/bin/dbus-run-session",
            "--",
            "/usr/bin/xvfb-run",
            "-a",
            "-s",
            "-screen 0 1280x900x24",
            "/usr/bin/python3",
            f"{guest_runtime}/packaged-appimage-live-gate.py",
            "guest-run",
            "--runtime-root",
            guest_runtime,
            "--binding",
            f"{guest_runtime}/input/release-binding.json",
            "--asset-directory",
            f"{guest_runtime}/input",
            "--updater-public-key",
            f"{guest_runtime}/input/updater-public-key.b64",
            "--proxy-host",
            GUEST_HOST_ALIAS,
            "--proxy-port",
            str(relay.port),
            "--proxy-token-file",
            f"{guest_runtime}/input/proxy-token",
            "--attempt",
            f"{guest_runtime}/input/inference-attempt.json",
            timeout=HOST_GUEST_COMMAND_TIMEOUT_SECONDS,
        )
        relay.close()
        relay_summary = relay.summary()
        relay = None
        if ssh_process is not None:
            forced = terminate_process(ssh_process, "LAX SSH tunnel")
            ssh_stdout, ssh_stderr = ssh_process.communicate(timeout=5)
            if forced:
                raise GateError("LAX SSH tunnel required forced termination")
            ssh_process = None
        evidence_path = copy_guest_evidence(
            args.limactl, vm_name, guest_runtime, args.output
        )
        for path in evidence_path.rglob("*"):
            if path.is_symlink():
                raise GateError("copied guest evidence contains a symlink")
            os.chmod(path, 0o700 if path.is_dir() else 0o400)
        os.chmod(evidence_path, 0o700)
        validate_w3c_transcript(evidence_path / "w3c-transcript.json")
        guest_receipt, guest_receipt_raw = validate_guest_evidence(
            evidence_path,
            binding,
            binding_raw,
            attempt,
            attempt_raw,
            assets,
        )
    except BaseException as error:  # cleanup must run for interrupts too
        primary_error = error
    finally:
        if relay is not None:
            try:
                relay.close()
                relay_summary = relay.summary()
            except Exception as error:  # noqa: BLE001
                cleanup_errors.append(f"relay cleanup: {error}")
        if ssh_process is not None:
            try:
                forced = terminate_process(ssh_process, "LAX SSH tunnel")
                ssh_stdout, ssh_stderr = ssh_process.communicate(timeout=5)
                if forced:
                    cleanup_errors.append("LAX SSH tunnel required forced termination")
            except Exception as error:  # noqa: BLE001
                cleanup_errors.append(f"SSH cleanup: {error}")
        if vm_created:
            try:
                current = lima_list(args.limactl).get(vm_name)
                if current is None:
                    cleanup_errors.append("disposable VM disappeared before controlled deletion")
                else:
                    if current.get("protected") is not False:
                        cleanup_errors.append("refusing to delete a VM that became protected")
                    else:
                        if current.get("status") != "Stopped":
                            limactl_command(args.limactl, "stop", vm_name, timeout=300)
                        limactl_command(args.limactl, "delete", vm_name, timeout=300)
            except Exception as error:  # noqa: BLE001
                cleanup_errors.append(f"disposable VM cleanup: {error}")
        try:
            final_instances = lima_list(args.limactl)
            if stable_lima_state(final_instances) != initial_stable:
                cleanup_errors.append("pre-existing Lima VM state changed")
        except Exception as error:  # noqa: BLE001
            cleanup_errors.append(f"Lima state recheck: {error}")
        with contextlib.suppress(OSError):
            shutil.rmtree(run_root)

    if primary_error is not None:
        if cleanup_errors:
            raise GateError(
                f"packaged AppImage gate failed ({primary_error}); cleanup also failed: "
                + "; ".join(cleanup_errors)
            ) from primary_error
        raise primary_error
    if cleanup_errors:
        raise GateError("packaged AppImage cleanup failed: " + "; ".join(cleanup_errors))
    if relay_summary is None or guest_receipt is None or guest_receipt_raw is None:
        raise GateError("packaged AppImage host run omitted mandatory evidence")

    relay_raw = canonical_json(relay_summary)
    create_file(args.output / "connect-relay.json", relay_raw, 0o400)
    create_file(args.output / "ssh.stdout", ssh_stdout, 0o400)
    create_file(args.output / "ssh.stderr", ssh_stderr, 0o400)
    signature_receipt = guest_receipt.get("updater_signature")
    if not isinstance(signature_receipt, dict):
        raise GateError("validated guest receipt omitted updater signature evidence")
    if signature_receipt.get("public_key_sha256") != sha256_bytes(
        canonical_pubkey
    ):
        raise GateError("guest updater signature used a different release-commit public key")
    create_file(
        args.output / "updater-signature-verification.json",
        canonical_json(signature_receipt),
        0o400,
    )
    final_receipt = {
        "completed_at": utc_now(),
        "disposable_vm": {
            "config_sha256": sha256_bytes(config_raw),
            "deleted_after_evidence_copy": True,
            "image_digest": LIMA_IMAGE_DIGEST,
            "image_url": LIMA_IMAGE_URL,
            "mounts": [],
            "name": vm_name,
            "preexisting_instances_unchanged": True,
            "recovery_enclave_accessed": False,
        },
        "guest_receipt": {
            "path": (evidence_path / "receipt.json").relative_to(args.output).as_posix(),
            "sha256": sha256_bytes(guest_receipt_raw),
            "schema": guest_receipt["schema"],
        },
        "inference_attempt": {
            "path": "inference-attempt.json",
            "plan_sha256": attempt["plan_sha256"],
            "sha256": sha256_bytes(attempt_raw),
            "state": attempt["state"],
        },
        "host_runtime": {
            "limactl": limactl_id,
            "limactl_version": lima_version_output,
            "ssh": ssh_id,
        },
        "platform_claim": PLATFORM_CLAIM,
        "release": {
            "assets": assets,
            "binding_sha256": sha256_bytes(binding_raw),
            "commit": binding["commit"],
            "release_id": binding["release"]["id"],
            "repository": binding["repository"],
            "tag": binding["tag"],
        },
        "result": "passed",
        "schema": HOST_SCHEMA,
        "source": source,
        "transport": {
            "accepted_connect_count": relay_summary["accepted_connections"],
            "identity_sha256": sha256_file(args.identity),
            "known_hosts_sha256": sha256_file(args.known_hosts),
            "relay_log_sha256": sha256_bytes(relay_raw),
            "relay_target": relay_summary["target"],
            "relay_token_sha256": relay_summary["token_sha256"],
            "remote": f"{LAX_USER}@{LAX_HOST}:127.0.0.1:{LAX_PORT}",
            "ssh_options_sha256": sha256_bytes(canonical_json(ssh_options)),
            "ssh_stderr_sha256": sha256_bytes(ssh_stderr),
            "ssh_stdout_sha256": sha256_bytes(ssh_stdout),
        },
        "updater_signature": signature_receipt,
    }
    create_file(args.output / "receipt.json", canonical_json(final_receipt), 0o400)
    return final_receipt


def implementation_contract() -> dict[str, Any]:
    return {
        "apt": {
            "packages": list(APT_PACKAGES),
            "snapshot": APT_SNAPSHOT,
            "source_sha256": sha256_bytes(APT_SOURCE.encode("ascii")),
        },
        "evidence": {
            "guest_schema": SCHEMA,
            "host_schema": HOST_SCHEMA,
            "mode": (
                "canonical JSON, create-only 0400 files under non-writable directories, "
                "with both file and parent-directory fsync"
            ),
            "no_skip": list(EXPECTED_TESTS),
            "product_binding": (
                "exact prompt BLAKE3 plus UI output/model and settlement tx/job/worker/URL "
                "must equal one independently fetched canonical 0x25 receipt"
            ),
        },
        "identity": {
            "creation": "real generate_identity Tauri command via first-run UI",
            "profile": "fresh HOME and XDG roots inside disposable VM",
            "recovery_phrase": (
                "reveal clicked only to acknowledge; no text/page-source/script/screenshot capture "
                "until the phrase-bearing screen is gone"
            ),
            "teardown": "stop managed node, close GUI, remove profile, delete exact new VM",
        },
        "package": {
            "application_capability": APPIMAGE_NAME,
            "architecture": "Linux ELF64 x86-64",
            "entrypoint": "exact AppImage with APPIMAGE_EXTRACT_AND_RUN=1",
            "expected_binary": EXPECTED_APP_BINARY,
            "extracted_tree": (
                "bounded entries/depth/path/file/total bytes; every symlink must resolve inside tree"
            ),
            "platform_claim": PLATFORM_CLAIM,
            "release_binding": RELEASE_SCHEMA,
            "updater_signature": (
                "verified inside the disposable VM with snapshot-pinned minisign, "
                "the release-commit Tauri public key, and exact bound AppImage/.sig bytes"
            ),
        },
        "source_and_transport": {
            "compiled_origin": f"https://{LAX_HOST}",
            "egress": (
                "TCP default DROP; only authenticated host-loopback CONNECT proxy is reachable; "
                "local observer QUIC is restricted to the twelve committed seed endpoints"
            ),
            "host_gateway": GUEST_HOST_ALIAS,
            "proxy_policy": f"Basic-auth CONNECT only to {LAX_HOST}:{LAX_PORT}",
            "ssh": f"host-key-pinned public-key local forward to {LAX_USER}@{LAX_HOST}:127.0.0.1:{LAX_PORT}",
        },
        "ui": {
            "dashboard": ["dashboard", "sidebar-status", "node-block-height", "stat-peers"],
            "earnings": [
                "nav-earnings",
                "earnings-screen",
                "projection-card",
                "earnings-empty|confirmed-reward-receipts",
            ],
            "inference": [
                "nav-inference",
                "inference-prompt",
                "inference-max-tokens",
                "btn-run-inference",
                "inference-result[data-output-hash][data-model-id][data-routed-via][data-coordinator]",
                "inference-community-worker",
                "inference-coordinator",
                (
                    "community-settlement[data-receipt-status=mined_success]"
                    "[data-tx-type][data-tx-hash][data-job-id][data-worker]"
                    "[data-receipt-url][data-submitted=true]"
                ),
                "btn-lookup-reward",
            ],
            "network": [
                "network-screen",
                "tx-lookup-input",
                "tx-lookup-result",
                "tx-status-mined",
                "net-stat-validators",
                "net-stat-block-height",
                "block-list",
            ],
            "onboarding": [
                "step-welcome",
                "btn-continue-welcome",
                "step-identity",
                "identity-address",
                "btn-reveal-seed",
                "btn-continue-identity",
                "tier-skip",
                "btn-continue-model",
                "btn-launch",
            ],
        },
        "vm": {
            "arch": "x86_64",
            "cpus": 4,
            "disk": "24GiB",
            "image_digest": LIMA_IMAGE_DIGEST,
            "image_url": LIMA_IMAGE_URL,
            "lima_version": LIMA_VERSION,
            "memory": "8GiB",
            "mounts": [],
            "name_pattern": VM_NAME_RE.pattern,
            "recovery_enclave": "never targeted; all pre-existing VM state must be unchanged",
        },
        "w3c": {
            "driver": "/usr/bin/WebKitWebDriver",
            "endpoints": [
                "POST /session",
                "POST /session/{id}/timeouts",
                "POST /session/{id}/window/rect",
                "POST /session/{id}/element",
                "POST /session/{id}/elements",
                "POST /session/{id}/element/{element}/click",
                "POST /session/{id}/element/{element}/clear",
                "POST /session/{id}/element/{element}/value",
                "GET /session/{id}/element/{element}/text",
                "GET /session/{id}/element/{element}/attribute/{name}",
                "GET /session/{id}/element/{element}/property/{name}",
                "GET /session/{id}/screenshot",
                "DELETE /session/{id}",
            ],
            "forbidden": ["page source", "execute sync/async", "direct __TAURI_INTERNALS__ invoke"],
            "native_capability": "webkitgtk:browserOptions.binary",
        },
        "dispatch_budget": {
            "automatic_retries": 0,
            "host_guest_command_timeout_seconds": HOST_GUEST_COMMAND_TIMEOUT_SECONDS,
            "inference_wait_seconds": INFERENCE_WAIT_SECONDS,
            "inference_submit_clicks": 1,
            "receipt_reads": "bounded polling plus two coherent independent public GETs",
            "settlement_ui_wait_seconds": RECEIPT_UI_WAIT_SECONDS,
            "total_guest_timeout_seconds": GUEST_GATE_TIMEOUT_SECONDS,
        },
    }


def validate_host_receipt_envelope(
    receipt: Mapping[str, Any],
    binding: Mapping[str, Any],
    binding_raw: bytes,
    evidence_root: Path,
) -> tuple[dict[str, Mapping[str, Any]], Mapping[str, Any]]:
    """Validate host-side claims that are outside the guest evidence tree."""

    expected_assets: dict[str, Mapping[str, Any]] = {
        name: {"name": name, **binding["assets"][name]}
        for name in (APPIMAGE_NAME, APPIMAGE_SIGNATURE_NAME, NODE_NAME)
    }
    expected_release = {
        "assets": expected_assets,
        "binding_sha256": sha256_bytes(binding_raw),
        "commit": binding["commit"],
        "release_id": binding["release"]["id"],
        "repository": binding["repository"],
        "tag": binding["tag"],
    }
    if receipt.get("release") != expected_release:
        raise GateError("packaged AppImage host release/assets differ from the immutable binding")

    repo = Path(__file__).resolve().parents[2]
    gate_path = Path(__file__).resolve()
    expected_source = {
        "commit": binding["commit"],
        "gate_path": gate_path.relative_to(repo).as_posix(),
        "gate_sha256": sha256_file(gate_path),
        "tree_clean": True,
    }
    if receipt.get("source") != expected_source:
        raise GateError("packaged AppImage host source differs from the exact verifier/release")

    vm = receipt.get("disposable_vm")
    require_exact_keys(
        vm,
        {
            "config_sha256",
            "deleted_after_evidence_copy",
            "image_digest",
            "image_url",
            "mounts",
            "name",
            "preexisting_instances_unchanged",
            "recovery_enclave_accessed",
        },
        "packaged AppImage disposable VM",
    )
    config_raw = read_bytes(evidence_root / "lima.yaml", "packaged AppImage Lima config")
    if (
        config_raw != lima_config()
        or vm.get("config_sha256") != sha256_bytes(config_raw)
        or vm.get("deleted_after_evidence_copy") is not True
        or vm.get("image_digest") != LIMA_IMAGE_DIGEST
        or vm.get("image_url") != LIMA_IMAGE_URL
        or vm.get("mounts") != []
        or not VM_NAME_RE.fullmatch(str(vm.get("name", "")))
        or vm.get("preexisting_instances_unchanged") is not True
        or vm.get("recovery_enclave_accessed") is not False
    ):
        raise GateError("packaged AppImage disposable VM identity/isolation differs")

    runtime = receipt.get("host_runtime")
    require_exact_keys(
        runtime,
        {"limactl", "limactl_version", "ssh"},
        "packaged AppImage host runtime",
    )
    if runtime.get("limactl_version") != f"limactl version {LIMA_VERSION}":
        raise GateError("packaged AppImage host Lima version differs")
    for key, label in (("limactl", "limactl"), ("ssh", "OpenSSH client")):
        identity = runtime.get(key)
        require_exact_keys(
            identity,
            {"path", "resolved_path", "sha256", "size"},
            f"packaged AppImage host {key} identity",
        )
        if (
            not isinstance(identity.get("path"), str)
            or not Path(identity["path"]).is_absolute()
            or not isinstance(identity.get("resolved_path"), str)
            or not Path(identity["resolved_path"]).is_absolute()
            or not HASH_RE.fullmatch(str(identity.get("sha256", "")))
            or type(identity.get("size")) is not int  # noqa: E721
            or identity["size"] <= 0
            or executable_identity(Path(identity["path"]), label) != identity
        ):
            raise GateError(f"packaged AppImage host {key} executable identity differs")

    transport = receipt.get("transport")
    require_exact_keys(
        transport,
        {
            "accepted_connect_count",
            "identity_sha256",
            "known_hosts_sha256",
            "relay_log_sha256",
            "relay_target",
            "relay_token_sha256",
            "remote",
            "ssh_options_sha256",
            "ssh_stderr_sha256",
            "ssh_stdout_sha256",
        },
        "packaged AppImage host transport",
    )
    if (
        type(transport.get("accepted_connect_count")) is not int  # noqa: E721
        or transport["accepted_connect_count"] <= 0
        or transport.get("relay_target") != f"{LAX_HOST}:{LAX_PORT}"
        or transport.get("remote")
        != f"{LAX_USER}@{LAX_HOST}:127.0.0.1:{LAX_PORT}"
        or any(
            not HASH_RE.fullmatch(str(transport.get(field, "")))
            for field in (
                "identity_sha256",
                "known_hosts_sha256",
                "relay_log_sha256",
                "relay_token_sha256",
                "ssh_options_sha256",
                "ssh_stderr_sha256",
                "ssh_stdout_sha256",
            )
        )
    ):
        raise GateError("packaged AppImage host transport identity is malformed/unbound")
    for name in ("stdout", "stderr"):
        raw = read_bytes(
            evidence_root / f"ssh.{name}",
            f"packaged AppImage SSH {name}",
            max_bytes=16 * 1024 * 1024,
            allow_empty=True,
        )
        if sha256_bytes(raw) != transport.get(f"ssh_{name}_sha256"):
            raise GateError(f"packaged AppImage SSH {name} differs from transport binding")
    return expected_assets, transport


def validate_connect_relay_evidence(
    relay: Mapping[str, Any], transport: Mapping[str, Any]
) -> None:
    require_exact_keys(
        relay,
        {
            "accepted_connections",
            "events",
            "listen",
            "listen_port",
            "rejected_connections",
            "started_at",
            "target",
            "token_sha256",
            "upstream",
        },
        "packaged AppImage CONNECT relay evidence",
    )
    events = relay.get("events")
    if not isinstance(events, list) or not events:
        raise GateError("packaged AppImage CONNECT relay has no exact event evidence")
    accepted: list[Mapping[str, Any]] = []
    rejected = 0
    for index, row in enumerate(events):
        require_exact_keys(
            row,
            {
                "accepted",
                "bytes_client_to_lax",
                "bytes_lax_to_client",
                "peer",
                "reason",
                "target",
            },
            f"packaged AppImage CONNECT relay event {index}",
        )
        if (
            type(row.get("accepted")) is not bool  # noqa: E721
            or type(row.get("bytes_client_to_lax")) is not int  # noqa: E721
            or type(row.get("bytes_lax_to_client")) is not int  # noqa: E721
            or row["bytes_client_to_lax"] < 0
            or row["bytes_lax_to_client"] < 0
            or not isinstance(row.get("peer"), str)
            or not isinstance(row.get("reason"), str)
            or (row.get("target") is not None and not isinstance(row.get("target"), str))
        ):
            raise GateError("packaged AppImage CONNECT relay event is malformed")
        if row["accepted"]:
            accepted.append(row)
        else:
            rejected += 1
    upstream_match = re.fullmatch(
        r"127\.0\.0\.1:([1-9][0-9]{0,4})", str(relay.get("upstream", ""))
    )
    if (
        relay.get("listen") != "127.0.0.1"
        or type(relay.get("listen_port")) is not int  # noqa: E721
        or not 1 <= relay["listen_port"] <= 65_535
        or relay.get("target") != f"{LAX_HOST}:{LAX_PORT}"
        or relay.get("token_sha256") != transport.get("relay_token_sha256")
        or upstream_match is None
        or int(upstream_match.group(1)) > 65_535
        or relay.get("accepted_connections") != len(accepted)
        or relay.get("accepted_connections") != transport.get("accepted_connect_count")
        or relay.get("rejected_connections") != rejected
        or any(
            row.get("target") != f"{LAX_HOST}:{LAX_PORT}"
            or row.get("reason") != "exact_lax_tls"
            or row.get("bytes_client_to_lax", 0) <= 0
            or row.get("bytes_lax_to_client", 0) <= 0
            for row in accepted
        )
    ):
        raise GateError("packaged AppImage CONNECT relay evidence is inconsistent/unbound")


def verify_host_evidence(args: argparse.Namespace) -> dict[str, Any]:
    binding, binding_raw = validate_release_binding(args.binding)
    if not args.receipt.is_absolute() or args.receipt.name != "receipt.json":
        raise GateError("packaged AppImage host receipt must be an absolute exact receipt.json")
    receipt_info = private_file(args.receipt, "packaged AppImage host receipt")
    if stat.S_IMODE(receipt_info.st_mode) != 0o400:
        raise GateError("packaged AppImage host receipt must have exact mode 0400")
    receipt, raw = load_json(args.receipt, "packaged AppImage host receipt")
    if raw != canonical_json(receipt):
        raise GateError("packaged AppImage host receipt is not canonical JSON")
    require_exact_keys(
        receipt,
        {
            "completed_at",
            "disposable_vm",
            "guest_receipt",
            "host_runtime",
            "inference_attempt",
            "platform_claim",
            "release",
            "result",
            "schema",
            "source",
            "transport",
            "updater_signature",
        },
        "packaged AppImage host receipt",
    )
    if receipt.get("schema") != HOST_SCHEMA or receipt.get("result") != "passed":
        raise GateError("packaged AppImage host receipt is not a passing supported schema")
    if receipt.get("platform_claim") != PLATFORM_CLAIM:
        raise GateError("packaged AppImage host receipt widened its platform claim")
    evidence_root = args.receipt.parent.resolve(strict=True)
    expected_assets, transport = validate_host_receipt_envelope(
        receipt, binding, binding_raw, evidence_root
    )
    signature = receipt.get("updater_signature")
    if not isinstance(signature, dict) or signature.get("updater_signature_verified") is not True:
        raise GateError("packaged AppImage host receipt lacks updater-signature proof")
    attempt_binding = receipt.get("inference_attempt")
    if not isinstance(attempt_binding, dict) or attempt_binding.get("state") != "armed-no-retry":
        raise GateError("packaged AppImage host receipt lacks terminal attempt arming")
    attempt_relative = str(attempt_binding.get("path", ""))
    attempt_parts = PurePosixPath(attempt_relative)
    if attempt_relative != "inference-attempt.json" or attempt_parts.is_absolute() or (
        ".." in attempt_parts.parts
    ):
        raise GateError("packaged AppImage host attempt path is not exact/safe")
    attempt_path = evidence_root / attempt_relative
    attempt, attempt_raw, _ = validate_inference_attempt(
        attempt_path, binding, binding_raw, expected_assets
    )
    if sha256_bytes(attempt_raw) != attempt_binding.get("sha256") or (
        attempt["plan_sha256"] != attempt_binding.get("plan_sha256")
    ):
        raise GateError("packaged AppImage host receipt attempt marker mismatch")
    guest_binding = receipt.get("guest_receipt")
    require_exact_keys(
        guest_binding,
        {"path", "schema", "sha256"},
        "packaged AppImage host guest-receipt binding",
    )
    guest_path = guest_binding.get("path")
    if (
        guest_binding.get("schema") != SCHEMA
        or not HASH_RE.fullmatch(str(guest_binding.get("sha256", "")))
        or guest_path
        not in {"guest-evidence/receipt.json", "guest-evidence/evidence/receipt.json"}
        or PurePosixPath(str(guest_path)).is_absolute()
        or ".." in PurePosixPath(str(guest_path)).parts
    ):
        raise GateError("packaged AppImage host receipt guest path is unsafe")
    guest_receipt_path = evidence_root / guest_path
    guest_receipt, guest_raw = validate_guest_evidence(
        guest_receipt_path.parent,
        binding,
        binding_raw,
        attempt,
        attempt_raw,
        expected_assets,
    )
    signature_path = evidence_root / "updater-signature-verification.json"
    persisted_signature, persisted_signature_raw = load_json(
        signature_path, "packaged AppImage updater signature verification"
    )
    if (
        persisted_signature_raw != canonical_json(persisted_signature)
        or persisted_signature != signature
        or guest_receipt.get("updater_signature") != signature
    ):
        raise GateError("host/guest updater signature evidence differs")
    expected_public_key_sha = sha256_bytes(
        canonical_updater_public_key_bytes(
            updater_public_key(Path(__file__).resolve().parents[2], binding["commit"])
        )
    )
    if signature.get("public_key_sha256") != expected_public_key_sha:
        raise GateError("updater signature public key differs from the release commit")
    validate_w3c_transcript(guest_receipt_path.parent / "w3c-transcript.json")
    if sha256_bytes(guest_raw) != receipt["guest_receipt"].get("sha256"):
        raise GateError("packaged AppImage host receipt guest digest mismatch")
    if attempt["plan"].get("updater_public_key_sha256") != expected_public_key_sha:
        raise GateError("armed inference plan used a different updater public key")
    relay, relay_raw = load_json(evidence_root / "connect-relay.json", "CONNECT relay evidence")
    if canonical_json(relay) != relay_raw or sha256_bytes(relay_raw) != transport.get("relay_log_sha256"):
        raise GateError("CONNECT relay evidence is non-canonical or unbound")
    validate_connect_relay_evidence(relay, transport)
    guest_network = guest_receipt.get("network")
    if not isinstance(guest_network, dict) or guest_network.get("proxy_token_sha256") != relay.get(
        "token_sha256"
    ):
        raise GateError("guest HTTPS proxy token is not bound to the host CONNECT relay")
    return receipt


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    subparsers.add_parser("describe", help="print the exact non-mutating harness contract")

    provision = subparsers.add_parser("provision-guest", help=argparse.SUPPRESS)
    provision.add_argument("--runtime-root", required=True, type=Path)

    lock = subparsers.add_parser("lock-egress", help=argparse.SUPPRESS)
    lock.add_argument("--runtime-root", required=True, type=Path)
    lock.add_argument("--proxy-port", required=True, type=int)

    guest = subparsers.add_parser("guest-run", help=argparse.SUPPRESS)
    guest.add_argument("--runtime-root", required=True, type=Path)
    guest.add_argument("--binding", required=True, type=Path)
    guest.add_argument("--asset-directory", required=True, type=Path)
    guest.add_argument("--updater-public-key", required=True, type=Path)
    guest.add_argument("--proxy-host", required=True)
    guest.add_argument("--proxy-port", required=True, type=int)
    guest.add_argument("--proxy-token-file", required=True, type=Path)
    guest.add_argument("--attempt", required=True, type=Path)

    host = subparsers.add_parser(
        "host-run", help="run the packaged gate in one exact new disposable Lima VM"
    )
    host.add_argument("--binding", required=True, type=Path)
    host.add_argument("--asset-directory", required=True, type=Path)
    host.add_argument("--known-hosts", required=True, type=Path)
    host.add_argument("--identity", required=True, type=Path)
    host.add_argument("--output", required=True, type=Path)
    host.add_argument("--vm-name")
    host.add_argument("--limactl", type=Path, default=Path("/opt/homebrew/bin/limactl"))
    host.add_argument("--ssh", type=Path, default=Path("/usr/bin/ssh"))

    verify = subparsers.add_parser("verify", help="verify a completed create-only host receipt")
    verify.add_argument("--binding", required=True, type=Path)
    verify.add_argument("--receipt", required=True, type=Path)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        if args.command == "describe":
            sys.stdout.buffer.write(canonical_json(implementation_contract()))
        elif args.command == "provision-guest":
            sys.stdout.buffer.write(canonical_json(provision_guest(args)))
        elif args.command == "lock-egress":
            sys.stdout.buffer.write(canonical_json(lock_guest_egress(args)))
        elif args.command == "guest-run":
            result = run_guest(args)
            print(f"packaged AppImage live gate passed: {result['release']['commit']}")
        elif args.command == "host-run":
            result = run_host(args)
            print(f"packaged AppImage host gate passed: {result['release']['commit']}")
        elif args.command == "verify":
            result = verify_host_evidence(args)
            print(f"packaged AppImage evidence verified: {result['release']['commit']}")
        else:  # pragma: no cover - argparse owns choices
            raise GateError(f"unsupported command: {args.command}")
    except (GateError, OSError, ValueError) as error:
        print(f"packaged AppImage live gate failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
