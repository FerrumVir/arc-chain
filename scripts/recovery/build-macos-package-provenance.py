#!/usr/bin/env python3
"""Fail-closed provenance and execution controller for the shipped macOS app.

The controller has three deliberately separate phases:

* ``lima-verify`` verifies the Tauri updater signature in a new, digest-pinned
  Lima VM with ``minisign`` installed from one Ubuntu snapshot.
* ``inspect`` safely extracts the updater archive, mounts the exact DMG read
  only, proves both packages contain the same app, records the truthful ad-hoc
  code-signing state, detaches the image, and seals the bundle identity used to
  construct the native acceptance input.
* ``run`` independently rechecks every input and the extracted updater app,
  remounts the exact DMG read only, and invokes the mounted shipped executable
  in its bounded ``--arc-production-acceptance`` mode.  It hashes the mounted
  tree before and after execution and detaches before sealing final evidence.

No phase downloads release assets, changes package bytes, clears quarantine,
re-signs an app, or exposes a WebDriver/devtools/test-only service.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import datetime as dt
import hashlib
import json
import os
import platform
import plistlib
import re
import resource
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import unicodedata
from pathlib import Path, PurePosixPath
from typing import Any, Iterable, Mapping, NoReturn, Sequence


REPOSITORY = "FerrumVir/arc-chain"
TAG = "v0.8.0"
RELEASE_SCHEMA = "arc.published-release-binding.v1"
SIGNATURE_SCHEMA = "arc.macos-updater-signature-verification.v1"
GUEST_SIGNATURE_SCHEMA = "arc.macos-updater-signature-guest.v1"
INSPECTION_SCHEMA = "arc.macos-package-inspection.v1"
PROVENANCE_SCHEMA = "arc.macos-packaged-native-provenance.v1"
VERIFICATION_SCHEMA = "arc.macos-package-provenance-verification.v1"
NATIVE_INPUT_SCHEMA = "arc.packaged-desktop-native-input.v1"
NATIVE_OUTPUT_SCHEMA = "arc.packaged-desktop-native-acceptance.v1"
NATIVE_ATTEMPT_SCHEMA = "arc.packaged-desktop-native-dispatch-attempt.v1"

APP_ARCHIVE_NAME = "arc-desktop-macos-arm64.app.tar.gz"
APP_SIGNATURE_NAME = f"{APP_ARCHIVE_NAME}.sig"
DMG_NAME = "arc-desktop-macos-arm64.dmg"
APP_BUNDLE_NAME = "ARC Node.app"
EXECUTABLE_RELATIVE_PATH = "Contents/MacOS/arc-desktop"
UPDATER_KEY_RELATIVE_PATH = "desktop/src-tauri/tauri.conf.json"

LIMA_VERSION = "2.1.1"
LIMA_IMAGE_URL = (
    "https://cloud-images.ubuntu.com/releases/noble/release-20260321/"
    "ubuntu-24.04-server-cloudimg-amd64.img"
)
LIMA_IMAGE_DIGEST = (
    "sha256:5c3ddb00f60bc455dac0862fabe9d8bacec46c33ac1751143c5c3683404b110d"
)
APT_SNAPSHOT = "20260321T235959Z"
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

MAX_JSON_BYTES = 16 * 1024 * 1024
MAX_ASSET_BYTES = 2 * 1024 * 1024 * 1024
MAX_SIGNATURE_BYTES = 1024 * 1024
MAX_ARCHIVE_ENTRIES = 20_000
MAX_ARCHIVE_TOTAL_BYTES = 2 * 1024 * 1024 * 1024
MAX_ARCHIVE_MEMBER_BYTES = 512 * 1024 * 1024
MAX_ARCHIVE_PATH_BYTES = 4_096
MAX_ARCHIVE_DEPTH = 33  # one app root plus the native verifier's depth 32
MAX_COMMAND_OUTPUT_BYTES = 16 * 1024 * 1024
NATIVE_INNER_MAX_SECONDS = 4_300
NATIVE_OUTER_TIMEOUT_SECONDS = 4_360

HASH_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
DEVICE_RE = re.compile(r"^/dev/disk[0-9]+(?:s[0-9]+)*$")
VM_NAME_RE = re.compile(r"^arc-macos-signature-v080-[0-9a-f]{8}-[0-9a-f]{6}$")
FIXED_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"


class ProvenanceError(RuntimeError):
    """Evidence is incomplete, ambiguous, mutable, or outside the gate."""


def fail(message: str) -> NoReturn:
    raise ProvenanceError(message)


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z")


def canonical_json(value: object) -> bytes:
    return (
        json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True)
        + "\n"
    ).encode("ascii")


def sha256_bytes(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def require_exact_keys(value: Mapping[str, Any], expected: Iterable[str], label: str) -> None:
    actual = set(value)
    wanted = set(expected)
    if actual != wanted:
        fail(
            f"{label} fields differ; missing={sorted(wanted - actual)!r}, "
            f"unexpected={sorted(actual - wanted)!r}"
        )


def positive_int(value: object, label: str) -> int:
    if type(value) is not int or value <= 0:  # noqa: E721 - bool is not accepted
        fail(f"{label} must be a positive integer")
    return value


def require_hash(value: object, label: str) -> str:
    if not isinstance(value, str) or HASH_RE.fullmatch(value) is None:
        fail(f"{label} is not one lowercase SHA-256 digest")
    return value


def require_commit(value: object, label: str) -> str:
    if not isinstance(value, str) or COMMIT_RE.fullmatch(value) is None:
        fail(f"{label} is not one full lowercase Git commit")
    return value


def regular_file(path: Path, label: str, *, max_bytes: int) -> os.stat_result:
    try:
        info = path.lstat()
    except OSError as error:
        fail(f"cannot inspect {label}: {error}")
    if not stat.S_ISREG(info.st_mode) or stat.S_ISLNK(info.st_mode):
        fail(f"{label} must be a non-symlink regular file: {path}")
    if info.st_size <= 0 or info.st_size > max_bytes:
        fail(f"{label} has an unsupported size: {path}")
    return info


def private_regular_file(path: Path, label: str, *, max_bytes: int) -> os.stat_result:
    info = regular_file(path, label, max_bytes=max_bytes)
    if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o400 or info.st_nlink != 1:
        fail(f"{label} must be operator-owned mode 0400 with link count one: {path}")
    return info


def private_directory(path: Path, label: str) -> os.stat_result:
    try:
        info = path.lstat()
    except OSError as error:
        fail(f"cannot inspect {label}: {error}")
    if (
        not stat.S_ISDIR(info.st_mode)
        or stat.S_ISLNK(info.st_mode)
        or info.st_uid != os.getuid()
        or stat.S_IMODE(info.st_mode) != 0o700
    ):
        fail(f"{label} must be an operator-owned real directory with mode 0700: {path}")
    return info


def require_absolute(path: Path, label: str) -> Path:
    if not path.is_absolute():
        fail(f"{label} must be absolute: {path}")
    return path


def secure_ancestry(path: Path, label: str, *, include_leaf: bool = False) -> None:
    """Reject symlinked or group/world-writable operator input ancestry."""

    require_absolute(path, label)
    stop = len(path.parts) if include_leaf else len(path.parts) - 1
    current = Path(path.anchor)
    for component in path.parts[1:stop]:
        current /= component
        try:
            info = current.lstat()
        except OSError as error:
            fail(f"cannot inspect {label} ancestor {current}: {error}")
        if (
            not stat.S_ISDIR(info.st_mode)
            or stat.S_ISLNK(info.st_mode)
            or info.st_uid not in (0, os.getuid())
            or stat.S_IMODE(info.st_mode) & 0o022
        ):
            fail(f"{label} has unsafe writable/symlinked ancestry at {current}")


def read_bytes(path: Path, label: str, *, max_bytes: int, private: bool = False) -> bytes:
    before = (
        private_regular_file(path, label, max_bytes=max_bytes)
        if private
        else regular_file(path, label, max_bytes=max_bytes)
    )
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    fd = os.open(path, flags)
    try:
        opened = os.fstat(fd)
        if (opened.st_dev, opened.st_ino, opened.st_size, opened.st_mode) != (
            before.st_dev,
            before.st_ino,
            before.st_size,
            before.st_mode,
        ):
            fail(f"{label} changed while opening: {path}")
        raw = bytearray()
        while len(raw) <= max_bytes:
            chunk = os.read(fd, min(1024 * 1024, max_bytes + 1 - len(raw)))
            if not chunk:
                break
            raw.extend(chunk)
        after = os.fstat(fd)
        if len(raw) > max_bytes or len(raw) != opened.st_size:
            fail(f"{label} changed size or exceeded its bound while read")
        if (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns) != (
            opened.st_dev,
            opened.st_ino,
            opened.st_size,
            opened.st_mtime_ns,
        ):
            fail(f"{label} changed while read: {path}")
        return bytes(raw)
    finally:
        os.close(fd)


def sha256_file(path: Path, label: str, *, max_bytes: int, private: bool = False) -> str:
    before = (
        private_regular_file(path, label, max_bytes=max_bytes)
        if private
        else regular_file(path, label, max_bytes=max_bytes)
    )
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    fd = os.open(path, flags)
    try:
        opened = os.fstat(fd)
        if (opened.st_dev, opened.st_ino, opened.st_size, opened.st_mode) != (
            before.st_dev,
            before.st_ino,
            before.st_size,
            before.st_mode,
        ):
            fail(f"{label} changed while opening: {path}")
        digest = hashlib.sha256()
        consumed = 0
        while chunk := os.read(fd, 1024 * 1024):
            consumed += len(chunk)
            if consumed > max_bytes:
                fail(f"{label} exceeded its size bound while hashing")
            digest.update(chunk)
        after = os.fstat(fd)
        if consumed != opened.st_size or (
            after.st_dev,
            after.st_ino,
            after.st_size,
            after.st_mtime_ns,
        ) != (
            opened.st_dev,
            opened.st_ino,
            opened.st_size,
            opened.st_mtime_ns,
        ):
            fail(f"{label} changed while hashing: {path}")
        return digest.hexdigest()
    finally:
        os.close(fd)


def load_json(
    path: Path, label: str, *, private: bool = True
) -> tuple[dict[str, Any], bytes]:
    raw = read_bytes(path, label, max_bytes=MAX_JSON_BYTES, private=private)
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"{label} is not valid JSON: {error}")
    if not isinstance(value, dict):
        fail(f"{label} must be a JSON object")
    if raw != canonical_json(value):
        fail(f"{label} is not canonical sorted newline-terminated JSON")
    return value, raw


def create_directory(path: Path, label: str) -> None:
    require_absolute(path, label)
    secure_ancestry(path, label)
    try:
        path.mkdir(mode=0o700, parents=False, exist_ok=False)
        os.chmod(path, 0o700)
    except OSError as error:
        fail(f"cannot create {label}: {error}")
    private_directory(path, label)
    parent_fd = os.open(
        path.parent,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    try:
        os.fsync(parent_fd)
    finally:
        os.close(parent_fd)


def create_file(path: Path, raw: bytes, label: str, mode: int = 0o400) -> None:
    require_absolute(path, label)
    parent_info = private_directory(path.parent, f"{label} parent")
    parent_fd = os.open(
        path.parent,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    try:
        opened_parent = os.fstat(parent_fd)
        if (opened_parent.st_dev, opened_parent.st_ino) != (
            parent_info.st_dev,
            parent_info.st_ino,
        ):
            fail(f"{label} parent changed while opening")
        flags = (
            os.O_WRONLY
            | os.O_CREAT
            | os.O_EXCL
            | getattr(os, "O_NOFOLLOW", 0)
            | getattr(os, "O_CLOEXEC", 0)
        )
        fd = os.open(path.name, flags, mode, dir_fd=parent_fd)
        try:
            offset = 0
            while offset < len(raw):
                written = os.write(fd, raw[offset:])
                if written <= 0:
                    fail(f"short write while sealing {label}")
                offset += written
            os.fchmod(fd, mode)
            os.fsync(fd)
            info = os.fstat(fd)
            if (
                not stat.S_ISREG(info.st_mode)
                or stat.S_IMODE(info.st_mode) != mode
                or info.st_size != len(raw)
                or info.st_nlink != 1
            ):
                fail(f"sealed {label} metadata differs")
        finally:
            os.close(fd)
        os.fsync(parent_fd)
        after_parent = os.fstat(parent_fd)
        if (after_parent.st_dev, after_parent.st_ino) != (
            opened_parent.st_dev,
            opened_parent.st_ino,
        ):
            fail(f"{label} parent changed while sealing")
    finally:
        os.close(parent_fd)


def create_root_system_file(path: Path, raw: bytes, label: str, mode: int) -> None:
    """Create one root-owned system file without trusting a path recheck."""

    if os.geteuid() != 0 or not path.is_absolute():
        fail(f"{label} requires root and an absolute path")
    parent_fd = os.open(
        path.parent,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    try:
        flags = (
            os.O_WRONLY
            | os.O_CREAT
            | os.O_EXCL
            | getattr(os, "O_NOFOLLOW", 0)
            | getattr(os, "O_CLOEXEC", 0)
        )
        fd = os.open(path.name, flags, mode, dir_fd=parent_fd)
        try:
            offset = 0
            while offset < len(raw):
                offset += os.write(fd, raw[offset:])
            os.fchmod(fd, mode)
            os.fsync(fd)
            info = os.fstat(fd)
            if info.st_uid != 0 or stat.S_IMODE(info.st_mode) != mode or info.st_nlink != 1:
                fail(f"{label} metadata differs after create")
        finally:
            os.close(fd)
        os.fsync(parent_fd)
    finally:
        os.close(parent_fd)


def load_release_binding(path: Path) -> tuple[dict[str, Any], bytes]:
    value, raw = load_json(path, "published release binding")
    require_exact_keys(
        value,
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
    if value["schema"] != RELEASE_SCHEMA or value["repository"] != REPOSITORY or value["tag"] != TAG:
        fail("published release binding repository/schema/tag differs")
    commit = require_commit(value["commit"], "published release binding commit")
    release = value["release"]
    if not isinstance(release, dict) or set(release) != {"id", "immutable"}:
        fail("published release identity fields differ")
    positive_int(release["id"], "published release ID")
    if release["immutable"] is not True:
        fail("published release is not immutable")
    workflow = value["release_workflow"]
    require_exact_keys(
        workflow,
        {
            "event",
            "head_branch",
            "head_sha",
            "id",
            "jobs_sha256",
            "path",
            "run_attempt",
            "run_id",
        },
        "published release workflow",
    )
    if (
        workflow["event"] != "workflow_dispatch"
        or workflow["head_branch"] != "main"
        or workflow["head_sha"] != commit
        or workflow["path"] != ".github/workflows/release.yml"
    ):
        fail("published release workflow source/event differs")
    for field in ("id", "run_id", "run_attempt"):
        positive_int(workflow[field], f"published release workflow {field}")
    require_hash(workflow["jobs_sha256"], "published release workflow jobs digest")
    assets = value["assets"]
    if not isinstance(assets, dict):
        fail("published release assets are missing")
    asset_ids: set[int] = set()
    for name in (APP_ARCHIVE_NAME, APP_SIGNATURE_NAME, DMG_NAME):
        asset = assets.get(name)
        if not isinstance(asset, dict) or set(asset) != {"id", "sha256", "size"}:
            fail(f"published release asset {name} fields differ")
        asset_id = positive_int(asset["id"], f"published release asset {name} ID")
        if asset_id in asset_ids:
            fail("published macOS release asset IDs are not distinct")
        asset_ids.add(asset_id)
        positive_int(asset["size"], f"published release asset {name} size")
        require_hash(asset["sha256"], f"published release asset {name} digest")
    return value, raw


def bound_assets(
    binding: Mapping[str, Any], asset_directory: Path
) -> dict[str, dict[str, Any]]:
    private_directory(asset_directory, "published asset directory")
    result: dict[str, dict[str, Any]] = {}
    for name, maximum in (
        (APP_ARCHIVE_NAME, MAX_ASSET_BYTES),
        (APP_SIGNATURE_NAME, MAX_SIGNATURE_BYTES),
        (DMG_NAME, MAX_ASSET_BYTES),
    ):
        path = asset_directory / name
        info = private_regular_file(path, f"published asset {name}", max_bytes=maximum)
        expected = binding["assets"][name]
        digest = sha256_file(path, f"published asset {name}", max_bytes=maximum, private=True)
        if info.st_size != expected["size"] or digest != expected["sha256"]:
            fail(f"published asset {name} differs from immutable release binding")
        result[name] = {
            "id": expected["id"],
            "name": name,
            "sha256": digest,
            "size": info.st_size,
        }
    return result


def canonical_updater_public_key(repository_root: Path, commit: str) -> bytes:
    result = run_command(
        ["/usr/bin/git", "show", f"{commit}:{UPDATER_KEY_RELATIVE_PATH}"],
        cwd=repository_root,
        timeout=30,
    )
    try:
        value = json.loads(result.stdout)
        key = value["plugins"]["updater"]["pubkey"]
        compact = key.strip().encode("ascii", "strict")
        base64.b64decode(compact, validate=True)
    except (UnicodeError, ValueError, KeyError, TypeError, json.JSONDecodeError) as error:
        raise ProvenanceError("release-commit updater key is unavailable or malformed") from error
    if not compact or len(compact) > 256 * 1024:
        fail("release-commit updater key is empty or oversized")
    return compact + b"\n"


def verify_source_checkout(repository_root: Path, commit: str) -> dict[str, Any]:
    require_absolute(repository_root, "repository root")
    if repository_root.is_symlink() or not repository_root.is_dir():
        fail("repository root must be a real directory")
    head = run_command(["/usr/bin/git", "rev-parse", "HEAD"], cwd=repository_root).stdout.decode(
        "ascii", "strict"
    ).strip()
    if head != commit:
        fail("repository HEAD differs from published release commit")
    status_output = run_command(
        ["/usr/bin/git", "status", "--porcelain=v1", "--untracked-files=all"],
        cwd=repository_root,
    ).stdout
    if status_output:
        fail("macOS provenance controller requires a clean exact release checkout")
    helper = Path(__file__).resolve(strict=True)
    relative = helper.relative_to(repository_root.resolve(strict=True)).as_posix()
    committed = run_command(
        ["/usr/bin/git", "show", f"{commit}:{relative}"], cwd=repository_root
    ).stdout
    current = read_bytes(helper, "macOS provenance controller", max_bytes=4 * 1024 * 1024)
    if committed != current:
        fail("running macOS provenance controller differs from the release commit")
    return {"commit": commit, "path": relative, "sha256": sha256_bytes(current), "treeClean": True}


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
            stdin=None if input_data is not None else subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ProvenanceError(f"command failed to run ({argv[0]}): {error}") from error
    if len(result.stdout) > MAX_COMMAND_OUTPUT_BYTES or len(result.stderr) > MAX_COMMAND_OUTPUT_BYTES:
        fail(f"command output exceeded its bound: {argv[0]}")
    if check and result.returncode != 0:
        stderr = result.stderr.decode("utf-8", "replace")[-2_000:]
        fail(f"command failed ({argv[0]}, rc={result.returncode}): {stderr}")
    return result


def executable_identity(path: Path, label: str) -> dict[str, Any]:
    resolved = path.resolve(strict=True)
    info = regular_file(resolved, label, max_bytes=MAX_ASSET_BYTES)
    if not info.st_mode & 0o111:
        fail(f"{label} is not executable: {resolved}")
    return {
        "path": os.fspath(path),
        "resolvedPath": os.fspath(resolved),
        "sha256": sha256_file(resolved, label, max_bytes=MAX_ASSET_BYTES),
        "size": info.st_size,
    }


def decode_tauri_document(path: Path, label: str) -> bytes:
    encoded = read_bytes(path, label, max_bytes=MAX_SIGNATURE_BYTES, private=True)
    try:
        compact = encoded.decode("ascii", "strict").strip()
        decoded = base64.b64decode(compact, validate=True)
        decoded.decode("utf-8", "strict")
    except (UnicodeError, ValueError) as error:
        raise ProvenanceError(f"{label} is not strict outer-base64 UTF-8") from error
    if not decoded or len(decoded) > MAX_SIGNATURE_BYTES or b"\x00" in decoded:
        fail(f"{label} decoded document is empty, oversized, or contains NUL")
    return decoded


def minisign_public_key_line(document: bytes) -> str:
    candidates: list[str] = []
    for line in document.decode("utf-8", "strict").splitlines():
        candidate = line.strip()
        try:
            decoded = base64.b64decode(candidate, validate=True)
        except (UnicodeError, ValueError):
            continue
        if len(decoded) == 42:
            candidates.append(candidate)
    if len(candidates) != 1:
        fail("Tauri updater key document does not contain one exact minisign key")
    return candidates[0]


def apt_package_identity(name: str) -> dict[str, str]:
    result = run_command(
        [
            "/usr/bin/dpkg-query",
            "--show",
            "--showformat=${Architecture}\t${Version}\n",
            name,
        ],
        timeout=30,
    )
    try:
        architecture, version = result.stdout.decode("utf-8", "strict").strip().split("\t")
    except (UnicodeError, ValueError) as error:
        raise ProvenanceError(f"dpkg returned malformed identity for {name}") from error
    if not architecture or not version or any(character.isspace() for character in architecture):
        fail(f"dpkg returned unsafe identity for {name}")
    return {"architecture": architecture, "version": version}


def guest_verify_signature(args: argparse.Namespace) -> dict[str, Any]:
    if os.geteuid() != 0 or platform.system() != "Linux" or platform.machine() != "x86_64":
        fail("guest signature verification requires root in the pinned Linux x86_64 VM")
    runtime = require_absolute(args.runtime_root, "guest runtime root")
    if runtime.parent != Path("/var/tmp") or not runtime.name.startswith("arc-macos-signature-v080-"):
        fail("guest runtime root is outside the exact disposable signature namespace")
    private_directory(runtime, "guest runtime root")
    archive = runtime / "input" / APP_ARCHIVE_NAME
    signature = runtime / "input" / APP_SIGNATURE_NAME
    public_key = runtime / "input" / "updater-public-key.b64"
    for path, label, maximum in (
        (archive, "guest updater archive", MAX_ASSET_BYTES),
        (signature, "guest updater signature", MAX_SIGNATURE_BYTES),
        (public_key, "guest updater public key", MAX_SIGNATURE_BYTES),
    ):
        private_regular_file(path, label, max_bytes=maximum)

    source_path = Path("/etc/apt/sources.list.d/arc-macos-signature.sources")
    if source_path.exists() or source_path.is_symlink():
        fail("fresh signature VM already contains controller APT state")
    create_root_system_file(
        source_path,
        APT_SOURCE.encode("ascii"),
        "pinned updater signature APT source",
        0o444,
    )
    apt_options = [
        "-o",
        f"Dir::Etc::sourcelist={source_path}",
        "-o",
        "Dir::Etc::sourceparts=-",
        "-o",
        "APT::Get::List-Cleanup=0",
    ]
    apt_environment = {
        "DEBIAN_FRONTEND": "noninteractive",
        "HOME": "/root",
        "LANG": "C",
        "LC_ALL": "C",
        "PATH": FIXED_PATH,
    }
    update = run_command(
        ["/usr/bin/apt-get", *apt_options, "update"],
        env=apt_environment,
        timeout=600,
    )
    combined_update = (update.stdout + update.stderr).decode("utf-8", "replace")
    if f"snapshot.ubuntu.com/ubuntu/{APT_SNAPSHOT}" not in combined_update:
        fail("APT did not report the exact configured Ubuntu snapshot")
    install = run_command(
        [
            "/usr/bin/apt-get",
            *apt_options,
            "install",
            "-y",
            "--no-install-recommends",
            "--allow-downgrades",
            "minisign",
            "python3-minimal",
        ],
        env=apt_environment,
        timeout=900,
    )

    public_document = decode_tauri_document(public_key, "guest updater public key")
    signature_document = decode_tauri_document(signature, "guest updater signature")
    key_line = minisign_public_key_line(public_document)
    decoded_signature = runtime / "updater-signature.minisig"
    create_file(decoded_signature, signature_document, "decoded updater signature", 0o400)
    try:
        verified = run_command(
            [
                "/usr/bin/minisign",
                "-Vm",
                os.fspath(archive),
                "-P",
                key_line,
                "-x",
                os.fspath(decoded_signature),
            ],
            env={"HOME": "/root", "LANG": "C", "LC_ALL": "C", "PATH": FIXED_PATH},
            timeout=180,
        )
    finally:
        with contextlib.suppress(FileNotFoundError):
            decoded_signature.unlink()

    helper = Path(__file__).resolve(strict=True)
    proof = {
        "apt": {
            "installStderrSha256": sha256_bytes(install.stderr),
            "installStdoutSha256": sha256_bytes(install.stdout),
            "snapshot": APT_SNAPSHOT,
            "sourceSha256": sha256_bytes(APT_SOURCE.encode("ascii")),
            "updateStderrSha256": sha256_bytes(update.stderr),
            "updateStdoutSha256": sha256_bytes(update.stdout),
        },
        "archiveSha256": sha256_file(
            archive, "guest updater archive", max_bytes=MAX_ASSET_BYTES, private=True
        ),
        "completedAt": utc_now(),
        "helperSha256": sha256_file(
            helper, "guest provenance helper", max_bytes=4 * 1024 * 1024
        ),
        "minisign": {
            "binary": executable_identity(Path("/usr/bin/minisign"), "guest minisign"),
            "package": apt_package_identity("minisign"),
        },
        "publicKeySha256": sha256_file(
            public_key, "guest updater public key", max_bytes=MAX_SIGNATURE_BYTES, private=True
        ),
        "schema": GUEST_SIGNATURE_SCHEMA,
        "signatureSha256": sha256_file(
            signature, "guest updater signature", max_bytes=MAX_SIGNATURE_BYTES, private=True
        ),
        "verification": {
            "stderrSha256": sha256_bytes(verified.stderr),
            "stdoutSha256": sha256_bytes(verified.stdout),
            "verified": True,
        },
    }
    output = runtime / "MACOS-UPDATER-SIGNATURE-GUEST.json"
    create_file(output, canonical_json(proof), "guest updater signature receipt", 0o400)
    return proof


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
cpus: 2
memory: "2GiB"
disk: "12GiB"
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


def lima_environment(args: argparse.Namespace) -> dict[str, str]:
    home = require_absolute(args.host_home, "explicit Lima host HOME")
    temporary = require_absolute(args.host_tmpdir, "explicit Lima host TMPDIR")
    lima_home = require_absolute(args.lima_home, "explicit LIMA_HOME")
    private_directory(home, "explicit Lima host HOME")
    private_directory(temporary, "explicit Lima host TMPDIR")
    private_directory(lima_home, "explicit LIMA_HOME")
    return {
        "HOME": os.fspath(home),
        "LANG": "C",
        "LC_ALL": "C",
        "LIMA_HOME": os.fspath(lima_home),
        "PATH": FIXED_PATH,
        "TMPDIR": os.fspath(temporary),
    }


def lima_run(
    limactl: Path,
    environment: Mapping[str, str],
    *arguments: str,
    timeout: int = 120,
) -> subprocess.CompletedProcess[bytes]:
    return run_command(
        [os.fspath(limactl), *arguments], env=environment, timeout=timeout
    )


def lima_list(limactl: Path, environment: Mapping[str, str]) -> dict[str, dict[str, Any]]:
    result = lima_run(limactl, environment, "list", "--json", timeout=30)
    instances: dict[str, dict[str, Any]] = {}
    for line in result.stdout.splitlines():
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise ProvenanceError("limactl list returned malformed JSON") from error
        if not isinstance(value, dict) or not isinstance(value.get("name"), str):
            fail("limactl list returned an invalid instance")
        if value["name"] in instances:
            fail("limactl list returned a duplicate instance")
        instances[value["name"]] = value
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


def validate_guest_signature(
    value: Mapping[str, Any],
    *,
    archive_sha256: str,
    signature_sha256: str,
    public_key_sha256: str,
    helper_sha256: str,
) -> None:
    require_exact_keys(
        value,
        {
            "apt",
            "archiveSha256",
            "completedAt",
            "helperSha256",
            "minisign",
            "publicKeySha256",
            "schema",
            "signatureSha256",
            "verification",
        },
        "guest updater signature receipt",
    )
    if value["schema"] != GUEST_SIGNATURE_SCHEMA:
        fail("guest updater signature schema differs")
    if (
        value["archiveSha256"] != archive_sha256
        or value["signatureSha256"] != signature_sha256
        or value["publicKeySha256"] != public_key_sha256
        or value["helperSha256"] != helper_sha256
    ):
        fail("guest updater signature receipt is not bound to the host inputs/helper")
    apt = value["apt"]
    require_exact_keys(
        apt,
        {
            "installStderrSha256",
            "installStdoutSha256",
            "snapshot",
            "sourceSha256",
            "updateStderrSha256",
            "updateStdoutSha256",
        },
        "guest updater signature APT evidence",
    )
    if apt["snapshot"] != APT_SNAPSHOT or apt["sourceSha256"] != sha256_bytes(APT_SOURCE.encode("ascii")):
        fail("guest updater signature APT snapshot differs")
    for field in (
        "installStderrSha256",
        "installStdoutSha256",
        "updateStderrSha256",
        "updateStdoutSha256",
    ):
        require_hash(apt[field], f"guest updater signature apt {field}")
    verification = value["verification"]
    require_exact_keys(
        verification, {"stderrSha256", "stdoutSha256", "verified"}, "minisign result"
    )
    if verification["verified"] is not True:
        fail("minisign did not verify the updater archive")
    require_hash(verification["stdoutSha256"], "minisign stdout digest")
    require_hash(verification["stderrSha256"], "minisign stderr digest")
    minisign = value["minisign"]
    require_exact_keys(minisign, {"binary", "package"}, "guest minisign identity")
    binary = minisign["binary"]
    require_exact_keys(binary, {"path", "resolvedPath", "sha256", "size"}, "guest minisign binary")
    if binary["path"] != "/usr/bin/minisign" or binary["resolvedPath"] != "/usr/bin/minisign":
        fail("guest minisign executable path differs")
    require_hash(binary["sha256"], "guest minisign executable digest")
    positive_int(binary["size"], "guest minisign executable size")
    package = minisign["package"]
    require_exact_keys(package, {"architecture", "version"}, "guest minisign package")
    if not all(isinstance(package[field], str) and package[field] for field in package):
        fail("guest minisign package identity is empty")


def lima_verify(args: argparse.Namespace) -> dict[str, Any]:
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        fail("Lima signature orchestration requires the audited macOS arm64 host")
    binding, binding_raw = load_release_binding(args.release_binding)
    assets = bound_assets(binding, args.asset_directory)
    source = verify_source_checkout(args.repository_root, binding["commit"])
    output = require_absolute(args.output, "signature receipt output")
    if output.name != "MACOS-UPDATER-SIGNATURE.json" or output.exists() or output.is_symlink():
        fail("signature receipt output must be one absent MACOS-UPDATER-SIGNATURE.json")
    private_directory(output.parent, "signature receipt output parent")
    limactl = require_absolute(args.limactl, "limactl")
    limactl_id = executable_identity(limactl, "limactl")
    if limactl_id["sha256"] != require_hash(args.limactl_sha256, "expected limactl digest"):
        fail("limactl executable differs from its reviewed digest")
    environment = lima_environment(args)
    version = lima_run(limactl, environment, "--version", timeout=30).stdout.decode(
        "utf-8", "strict"
    ).strip()
    if version != f"limactl version {LIMA_VERSION}":
        fail(f"limactl version differs: {version!r}")
    initial = lima_list(limactl, environment)
    initial_stable = stable_lima_state(initial)
    vm_name = args.vm_name or f"arc-macos-signature-v080-{binding['commit'][:8]}-{os.urandom(3).hex()}"
    if VM_NAME_RE.fullmatch(vm_name) is None or vm_name in initial:
        fail("disposable signature VM name is unsafe or already exists")

    working = Path(tempfile.mkdtemp(prefix=f".{vm_name}.", dir=output.parent))
    os.chmod(working, 0o700)
    config = working / "lima.yaml"
    key_path = working / "updater-public-key.b64"
    create_file(config, lima_config(), "Lima signature config", 0o400)
    updater_key = canonical_updater_public_key(args.repository_root, binding["commit"])
    create_file(key_path, updater_key, "release updater public key", 0o400)
    helper_sha256 = source["sha256"]
    guest_root = f"/var/tmp/{vm_name}"
    vm_created = False
    primary_error: BaseException | None = None
    cleanup_errors: list[str] = []
    guest_value: dict[str, Any] | None = None
    guest_raw: bytes | None = None
    try:
        lima_run(
            limactl,
            environment,
            "create",
            f"--name={vm_name}",
            "--tty=false",
            os.fspath(config),
            timeout=300,
        )
        vm_created = True
        lima_run(limactl, environment, "start", "--tty=false", vm_name, timeout=600)
        identity = lima_list(limactl, environment).get(vm_name)
        if (
            not identity
            or identity.get("protected") is not False
            or identity.get("arch") != "x86_64"
            or identity.get("vmType") != "qemu"
        ):
            fail("new disposable signature VM identity/protection/type differs")
        lima_run(
            limactl,
            environment,
            "shell",
            vm_name,
            "--",
            "/usr/bin/install",
            "-d",
            "-m",
            "0700",
            guest_root,
            f"{guest_root}/input",
            timeout=60,
        )
        staged = (
            (Path(__file__).resolve(), f"{guest_root}/build-macos-package-provenance.py"),
            (args.asset_directory / APP_ARCHIVE_NAME, f"{guest_root}/input/{APP_ARCHIVE_NAME}"),
            (args.asset_directory / APP_SIGNATURE_NAME, f"{guest_root}/input/{APP_SIGNATURE_NAME}"),
            (key_path, f"{guest_root}/input/updater-public-key.b64"),
        )
        for local, remote in staged:
            lima_run(
                limactl,
                environment,
                "copy",
                "--backend=scp",
                os.fspath(local),
                f"{vm_name}:{remote}",
                timeout=900,
            )
        lima_run(
            limactl,
            environment,
            "shell",
            vm_name,
            "--",
            "/usr/bin/sudo",
            "-n",
            "/bin/chown",
            "-R",
            "root:root",
            guest_root,
            timeout=60,
        )
        lima_run(
            limactl,
            environment,
            "shell",
            vm_name,
            "--",
            "/usr/bin/sudo",
            "-n",
            "/bin/chmod",
            "0700",
            guest_root,
            f"{guest_root}/input",
            timeout=60,
        )
        lima_run(
            limactl,
            environment,
            "shell",
            vm_name,
            "--",
            "/usr/bin/sudo",
            "-n",
            "/bin/chmod",
            "0500",
            f"{guest_root}/build-macos-package-provenance.py",
            timeout=60,
        )
        lima_run(
            limactl,
            environment,
            "shell",
            vm_name,
            "--",
            "/usr/bin/sudo",
            "-n",
            "/bin/chmod",
            "0400",
            f"{guest_root}/input/{APP_ARCHIVE_NAME}",
            f"{guest_root}/input/{APP_SIGNATURE_NAME}",
            f"{guest_root}/input/updater-public-key.b64",
            timeout=60,
        )
        lima_run(
            limactl,
            environment,
            "shell",
            vm_name,
            "--",
            "/usr/bin/sudo",
            "-n",
            "/usr/bin/python3",
            f"{guest_root}/build-macos-package-provenance.py",
            "guest-verify",
            "--runtime-root",
            guest_root,
            timeout=1_800,
        )
        guest_result = lima_run(
            limactl,
            environment,
            "shell",
            vm_name,
            "--",
            "/usr/bin/sudo",
            "-n",
            "/bin/cat",
            f"{guest_root}/MACOS-UPDATER-SIGNATURE-GUEST.json",
            timeout=60,
        )
        guest_raw = guest_result.stdout
        try:
            parsed = json.loads(guest_raw)
        except (UnicodeError, json.JSONDecodeError) as error:
            raise ProvenanceError("copied guest signature receipt is malformed") from error
        if not isinstance(parsed, dict) or canonical_json(parsed) != guest_raw:
            fail("copied guest signature receipt is not canonical JSON")
        guest_value = parsed
        validate_guest_signature(
            guest_value,
            archive_sha256=assets[APP_ARCHIVE_NAME]["sha256"],
            signature_sha256=assets[APP_SIGNATURE_NAME]["sha256"],
            public_key_sha256=sha256_bytes(updater_key),
            helper_sha256=helper_sha256,
        )
    except BaseException as error:  # cleanup also runs for interruption
        primary_error = error
    finally:
        if vm_created:
            try:
                current = lima_list(limactl, environment).get(vm_name)
                if current is None:
                    cleanup_errors.append("disposable signature VM disappeared")
                elif current.get("protected") is not False:
                    cleanup_errors.append("refusing to delete signature VM that became protected")
                else:
                    if current.get("status") != "Stopped":
                        lima_run(limactl, environment, "stop", vm_name, timeout=300)
                    lima_run(limactl, environment, "delete", vm_name, timeout=300)
            except Exception as error:  # noqa: BLE001
                cleanup_errors.append(f"signature VM cleanup: {error}")
        try:
            if stable_lima_state(lima_list(limactl, environment)) != initial_stable:
                cleanup_errors.append("pre-existing Lima state changed")
        except Exception as error:  # noqa: BLE001
            cleanup_errors.append(f"Lima state recheck: {error}")
        with contextlib.suppress(OSError):
            shutil.rmtree(working)

    if primary_error is not None:
        suffix = f"; cleanup also failed: {'; '.join(cleanup_errors)}" if cleanup_errors else ""
        raise ProvenanceError(f"Lima updater signature verification failed: {primary_error}{suffix}") from primary_error
    if cleanup_errors:
        fail("Lima updater signature cleanup failed: " + "; ".join(cleanup_errors))
    if guest_value is None or guest_raw is None:
        fail("Lima updater signature verification omitted guest evidence")
    receipt = {
        "assets": {
            "appArchive": assets[APP_ARCHIVE_NAME],
            "appArchiveSignature": assets[APP_SIGNATURE_NAME],
        },
        "bindingSha256": sha256_bytes(binding_raw),
        "completedAt": utc_now(),
        "disposableVm": {
            "configSha256": sha256_bytes(lima_config()),
            "deletedAfterEvidenceRead": True,
            "imageDigest": LIMA_IMAGE_DIGEST,
            "imageUrl": LIMA_IMAGE_URL,
            "mounts": [],
            "name": vm_name,
            "preexistingInstancesUnchanged": True,
            "recoveryEnclaveAccessed": False,
        },
        "guest": guest_value,
        "guestReceiptSha256": sha256_bytes(guest_raw),
        "limactl": limactl_id,
        "release": {
            "id": binding["release"]["id"],
            "runAttempt": binding["release_workflow"]["run_attempt"],
            "runId": binding["release_workflow"]["run_id"],
            "tag": binding["tag"],
        },
        "repository": REPOSITORY,
        "schema": SIGNATURE_SCHEMA,
        "source": source,
        "sourceCommit": binding["commit"],
        "updaterPublicKeySha256": sha256_bytes(updater_key),
        "verified": True,
    }
    create_file(output, canonical_json(receipt), "Lima updater signature receipt", 0o400)
    return receipt


def normalized_archive_path(raw_name: str, label: str) -> PurePosixPath:
    if not raw_name or "\x00" in raw_name or any(character in raw_name for character in "\r\n\t"):
        fail(f"{label} has an empty/control-bearing path")
    try:
        encoded = raw_name.encode("ascii", "strict")
    except UnicodeEncodeError as error:
        raise ProvenanceError(f"{label} path must be ASCII for cross-platform ordering") from error
    if len(encoded) > MAX_ARCHIVE_PATH_BYTES or raw_name.startswith("/"):
        fail(f"{label} path is absolute or exceeds its byte bound")
    path = PurePosixPath(raw_name)
    parts = path.parts
    if not parts or any(part in ("", ".", "..") for part in parts):
        fail(f"{label} path contains traversal or an empty component: {raw_name!r}")
    if len(parts) > MAX_ARCHIVE_DEPTH or parts[0] != APP_BUNDLE_NAME:
        fail(f"{label} is outside the one exact {APP_BUNDLE_NAME} root")
    if unicodedata.normalize("NFC", raw_name) != raw_name:
        fail(f"{label} path is not canonical Unicode NFC")
    return path


def normalized_symlink_target(member: PurePosixPath, raw_target: str) -> str:
    if (
        not raw_target
        or raw_target.startswith("/")
        or "\x00" in raw_target
        or any(character in raw_target for character in "\r\n\t")
    ):
        fail(f"archive symlink {member} has an empty, absolute, or control-bearing target")
    try:
        encoded = raw_target.encode("ascii", "strict")
    except UnicodeEncodeError as error:
        raise ProvenanceError(f"archive symlink {member} target must be ASCII") from error
    if len(encoded) > MAX_ARCHIVE_PATH_BYTES:
        fail(f"archive symlink {member} target exceeds its byte bound")
    stack = list(member.parent.parts)
    for part in PurePosixPath(raw_target).parts:
        if part in ("", "."):
            continue
        if part == "..":
            if len(stack) <= 1:
                fail(f"archive symlink {member} escapes the app bundle")
            stack.pop()
        else:
            stack.append(part)
    if not stack or stack[0] != APP_BUNDLE_NAME:
        fail(f"archive symlink {member} escapes the app bundle")
    return raw_target


def archive_manifest(archive: Path) -> tuple[list[tarfile.TarInfo], dict[str, Any]]:
    before = private_regular_file(archive, "macOS updater archive", max_bytes=MAX_ASSET_BYTES)
    archive_fd = os.open(
        archive,
        os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0),
    )
    try:
        opened_before = os.fstat(archive_fd)
        if (opened_before.st_dev, opened_before.st_ino, opened_before.st_size) != (
            before.st_dev,
            before.st_ino,
            before.st_size,
        ):
            fail("macOS updater archive changed while opening its manifest")
        with os.fdopen(archive_fd, "rb", closefd=False) as archive_file, tarfile.open(
            fileobj=archive_file, mode="r:gz"
        ) as opened:
            members = opened.getmembers()
    except (OSError, tarfile.TarError) as error:
        raise ProvenanceError(f"cannot read bounded macOS updater archive: {error}") from error
    finally:
        opened_after = os.fstat(archive_fd)
        os.close(archive_fd)
    after = private_regular_file(archive, "macOS updater archive", max_bytes=MAX_ASSET_BYTES)
    if (
        before.st_dev,
        before.st_ino,
        before.st_size,
        before.st_mtime_ns,
    ) != (
        opened_after.st_dev,
        opened_after.st_ino,
        opened_after.st_size,
        opened_after.st_mtime_ns,
    ) or (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns) != (
        opened_after.st_dev,
        opened_after.st_ino,
        opened_after.st_size,
        opened_after.st_mtime_ns,
    ):
        fail("macOS updater archive changed while its manifest was read")
    if not members or len(members) > MAX_ARCHIVE_ENTRIES:
        fail("macOS updater archive has an empty or oversized member list")

    names: dict[str, tarfile.TarInfo] = {}
    folded_names: dict[str, str] = {}
    directory_names: set[str] = set()
    total_bytes = 0
    regular_count = 0
    symlink_count = 0
    for member in members:
        path = normalized_archive_path(member.name, "macOS updater archive member")
        name = path.as_posix()
        folded = unicodedata.normalize("NFD", name).casefold()
        if name in names or (folded in folded_names and folded_names[folded] != name):
            fail("macOS updater archive contains duplicate or case/normalization-colliding paths")
        names[name] = member
        folded_names[folded] = name
        if member.isdir():
            if member.mode & 0o500 != 0o500:
                fail(f"macOS updater archive directory lacks owner read/execute: {name}")
            directory_names.add(name)
        elif member.isfile():
            if getattr(member, "sparse", None):
                fail(f"macOS updater archive contains a sparse file: {name}")
            if member.size < 0 or member.size > MAX_ARCHIVE_MEMBER_BYTES:
                fail(f"macOS updater archive member exceeds its size bound: {name}")
            total_bytes += member.size
            if total_bytes > MAX_ARCHIVE_TOTAL_BYTES:
                fail("macOS updater archive exceeds its total uncompressed byte bound")
            regular_count += 1
        elif member.issym():
            normalized_symlink_target(path, member.linkname)
            symlink_count += 1
        else:
            fail(f"macOS updater archive contains a hardlink or special member: {name}")
        if member.mode & 0o7000:
            fail(f"macOS updater archive contains set-id/sticky permissions: {name}")
    if APP_BUNDLE_NAME not in directory_names:
        fail(f"macOS updater archive omits the explicit {APP_BUNDLE_NAME} directory")
    for name in names:
        path = PurePosixPath(name)
        for depth in range(1, len(path.parts)):
            parent = PurePosixPath(*path.parts[:depth]).as_posix()
            if parent not in directory_names:
                fail(f"macOS updater archive omits explicit parent directory {parent}")
    return members, {
        "directoryCount": len(directory_names),
        "memberCount": len(members),
        "regularFileCount": regular_count,
        "symlinkCount": symlink_count,
        "totalRegularBytes": total_bytes,
    }


def open_directory_at(root_fd: int, parts: Sequence[str]) -> int:
    current = os.dup(root_fd)
    try:
        for part in parts:
            next_fd = os.open(
                part,
                os.O_RDONLY
                | getattr(os, "O_DIRECTORY", 0)
                | getattr(os, "O_NOFOLLOW", 0)
                | getattr(os, "O_CLOEXEC", 0),
                dir_fd=current,
            )
            os.close(current)
            current = next_fd
        return current
    except BaseException:
        os.close(current)
        raise


def safe_extract_archive(archive: Path, extraction_root: Path) -> dict[str, Any]:
    require_absolute(extraction_root, "archive extraction root")
    if extraction_root.exists() or extraction_root.is_symlink():
        fail("archive extraction root must not already exist")
    archive_identity = private_regular_file(
        archive, "macOS updater archive", max_bytes=MAX_ASSET_BYTES
    )
    members, summary = archive_manifest(archive)
    create_directory(extraction_root, "archive extraction root")
    root_fd = os.open(
        extraction_root,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    directory_modes: list[tuple[PurePosixPath, int]] = []
    try:
        for member in sorted(
            (item for item in members if item.isdir()),
            key=lambda item: (len(normalized_archive_path(item.name, "archive directory").parts), item.name),
        ):
            path = normalized_archive_path(member.name, "archive directory")
            parent_fd = open_directory_at(root_fd, path.parts[:-1])
            try:
                os.mkdir(path.name, mode=0o700, dir_fd=parent_fd)
                created = os.stat(path.name, dir_fd=parent_fd, follow_symlinks=False)
                if not stat.S_ISDIR(created.st_mode):
                    fail(f"archive directory was not created safely: {path}")
            except FileExistsError as error:
                raise ProvenanceError(f"archive directory already existed during extraction: {path}") from error
            finally:
                os.close(parent_fd)
            directory_modes.append((path, member.mode & 0o7777))

        archive_fd = os.open(
            archive,
            os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0),
        )
        opened_identity = os.fstat(archive_fd)
        if (opened_identity.st_dev, opened_identity.st_ino, opened_identity.st_size) != (
            archive_identity.st_dev,
            archive_identity.st_ino,
            archive_identity.st_size,
        ):
            os.close(archive_fd)
            fail("macOS updater archive changed between validation and extraction")
        with os.fdopen(archive_fd, "rb", closefd=True) as archive_file, tarfile.open(
            fileobj=archive_file, mode="r:gz"
        ) as opened:
            by_name = {
                normalized_archive_path(item.name, "archive member").as_posix(): item
                for item in opened
            }
            if set(by_name) != {
                normalized_archive_path(item.name, "archive member").as_posix() for item in members
            }:
                fail("macOS updater archive manifest changed between validation and extraction")
            for name in sorted(by_name):
                member = by_name[name]
                if not member.isfile():
                    continue
                path = PurePosixPath(name)
                source = opened.extractfile(member)
                if source is None:
                    fail(f"archive regular member has no data stream: {name}")
                parent_fd = open_directory_at(root_fd, path.parts[:-1])
                flags = (
                    os.O_WRONLY
                    | os.O_CREAT
                    | os.O_EXCL
                    | getattr(os, "O_NOFOLLOW", 0)
                    | getattr(os, "O_CLOEXEC", 0)
                )
                try:
                    fd = os.open(path.name, flags, member.mode & 0o7777, dir_fd=parent_fd)
                    try:
                        consumed = 0
                        while True:
                            chunk = source.read(min(1024 * 1024, member.size - consumed + 1))
                            if not chunk:
                                break
                            consumed += len(chunk)
                            if consumed > member.size or consumed > MAX_ARCHIVE_MEMBER_BYTES:
                                fail(f"archive member expanded past its declared size: {name}")
                            offset = 0
                            while offset < len(chunk):
                                offset += os.write(fd, chunk[offset:])
                        if consumed != member.size:
                            fail(f"archive member ended before its declared size: {name}")
                        os.fchmod(fd, member.mode & 0o7777)
                        os.fsync(fd)
                    finally:
                        os.close(fd)
                finally:
                    source.close()
                    os.close(parent_fd)

            for name in sorted(by_name):
                member = by_name[name]
                if not member.issym():
                    continue
                path = PurePosixPath(name)
                target = normalized_symlink_target(path, member.linkname)
                if member.mode & 0o7777 != 0o777:
                    fail(f"archive symbolic link mode is not the portable 0777 value: {name}")
                parent_fd = open_directory_at(root_fd, path.parts[:-1])
                try:
                    os.symlink(target, path.name, dir_fd=parent_fd)
                finally:
                    os.close(parent_fd)

        for path, mode in sorted(directory_modes, key=lambda row: len(row[0].parts), reverse=True):
            directory_fd = open_directory_at(root_fd, path.parts)
            try:
                os.fchmod(directory_fd, mode)
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
        os.fsync(root_fd)
    except BaseException:
        # Preserve a partial extraction for diagnosis; every later phase
        # refuses an existing destination, so it can never be resumed.
        raise
    finally:
        os.close(root_fd)

    app = extraction_root / APP_BUNDLE_NAME
    tree = app_bundle_tree(app, "safely extracted updater app")
    if tree["entryCount"] != summary["memberCount"] - 1:
        fail("extracted app entry count differs from the archive manifest")
    archive_after = sha256_file(
        archive, "macOS updater archive after extraction", max_bytes=MAX_ASSET_BYTES, private=True
    )
    return {
        "appBundleRelativePath": APP_BUNDLE_NAME,
        "appBundleTreeSha256": tree["sha256"],
        "archiveSha256AfterExtraction": archive_after,
        "implementation": "arc-openat-create-only-tar-extractor-v1",
        "limits": {
            "maxDepth": MAX_ARCHIVE_DEPTH,
            "maxEntries": MAX_ARCHIVE_ENTRIES,
            "maxMemberBytes": MAX_ARCHIVE_MEMBER_BYTES,
            "maxPathBytes": MAX_ARCHIVE_PATH_BYTES,
            "maxTotalRegularBytes": MAX_ARCHIVE_TOTAL_BYTES,
        },
        "manifest": summary,
        "safety": {
            "absolutePathsRejected": True,
            "caseAndNormalizationCollisionsRejected": True,
            "descriptorRelativeCreateOnly": True,
            "hardlinksAndSpecialEntriesRejected": True,
            "parentTraversalRejected": True,
            "setIdAndStickyModesRejected": True,
            "symlinksResolvedInsideBundle": True,
        },
    }


def app_bundle_tree(root: Path, label: str) -> dict[str, Any]:
    try:
        root_info = root.lstat()
    except OSError as error:
        fail(f"cannot inspect {label}: {error}")
    if not stat.S_ISDIR(root_info.st_mode) or stat.S_ISLNK(root_info.st_mode):
        fail(f"{label} root must be a real directory")
    canonical_root = root.resolve(strict=True)
    entries: list[dict[str, Any]] = []
    total_bytes = 0

    def walk(directory: Path, depth: int) -> None:
        nonlocal total_bytes
        if depth > 32:
            fail(f"{label} exceeds the reviewed traversal depth")
        try:
            children = sorted(os.scandir(directory), key=lambda item: item.name)
        except OSError as error:
            fail(f"cannot enumerate {label}: {error}")
        for child in children:
            if len(entries) >= MAX_ARCHIVE_ENTRIES:
                fail(f"{label} exceeds the reviewed member count")
            path = Path(child.path)
            try:
                relative = path.relative_to(canonical_root).as_posix()
                relative.encode("ascii", "strict")
            except (ValueError, UnicodeEncodeError) as error:
                raise ProvenanceError(f"{label} has an unsafe/non-ASCII member path") from error
            if not relative or any(character in relative for character in "\r\n\t"):
                fail(f"{label} has an empty/control-bearing member path")
            info = path.lstat()
            mode = stat.S_IMODE(info.st_mode)
            if stat.S_ISLNK(info.st_mode):
                target = os.readlink(path)
                normalized_symlink_target(PurePosixPath(APP_BUNDLE_NAME) / relative, target)
                try:
                    path.resolve(strict=True).relative_to(canonical_root)
                except (OSError, ValueError) as error:
                    raise ProvenanceError(f"{label} symlink escapes or is dangling: {relative}") from error
                entries.append(
                    {
                        "kind": "symlink",
                        "mode": mode,
                        "path": relative,
                        "sha256": None,
                        "size": None,
                        "target": target,
                    }
                )
            elif stat.S_ISDIR(info.st_mode):
                entries.append(
                    {
                        "kind": "directory",
                        "mode": mode,
                        "path": relative,
                        "sha256": None,
                        "size": None,
                        "target": None,
                    }
                )
                walk(path, depth + 1)
            elif stat.S_ISREG(info.st_mode):
                if info.st_nlink != 1 or info.st_size > MAX_ARCHIVE_MEMBER_BYTES:
                    fail(f"{label} file is hardlinked or oversized: {relative}")
                total_bytes += info.st_size
                if total_bytes > MAX_ARCHIVE_TOTAL_BYTES:
                    fail(f"{label} exceeds the reviewed total byte bound")
                entries.append(
                    {
                        "kind": "file",
                        "mode": mode,
                        "path": relative,
                        "sha256": sha256_file(
                            path,
                            f"{label} member {relative}",
                            max_bytes=MAX_ARCHIVE_MEMBER_BYTES,
                        ),
                        "size": info.st_size,
                        "target": None,
                    }
                )
            else:
                fail(f"{label} contains a special filesystem entry: {relative}")

    walk(canonical_root, 0)
    if not entries:
        fail(f"{label} is empty")
    entries.sort(key=lambda row: row["path"])
    return {
        "entryCount": len(entries),
        "sha256": sha256_bytes(canonical_json(entries)),
        "totalRegularBytes": total_bytes,
    }


def plist_object(raw: bytes, label: str) -> dict[str, Any]:
    if not raw or len(raw) > MAX_COMMAND_OUTPUT_BYTES:
        fail(f"{label} is empty or oversized")
    try:
        value = plistlib.loads(raw)
    except Exception as error:  # plistlib raises several parser-specific types
        raise ProvenanceError(f"{label} is not a valid property list") from error
    if not isinstance(value, dict):
        fail(f"{label} must be a property-list dictionary")
    return value


def mounted_entity(attach: Mapping[str, Any], mount_point: Path) -> dict[str, Any]:
    entities = attach.get("system-entities")
    if not isinstance(entities, list):
        fail("hdiutil attach omitted system entities")
    selected = [
        entity
        for entity in entities
        if isinstance(entity, dict) and entity.get("mount-point") == os.fspath(mount_point)
    ]
    if len(selected) != 1:
        fail("hdiutil attach did not report exactly one requested mounted filesystem")
    device = selected[0].get("dev-entry")
    if not isinstance(device, str) or DEVICE_RE.fullmatch(device) is None:
        fail("hdiutil attach returned an unsafe mounted device")
    return selected[0]


def validate_hdiutil_source(
    info: Mapping[str, Any], dmg: Path, mount_point: Path, device: str
) -> dict[str, Any]:
    images = info.get("images")
    if not isinstance(images, list):
        fail("hdiutil info omitted attached image inventory")
    expected = dmg.resolve(strict=True)
    matches: list[Mapping[str, Any]] = []
    for image in images:
        if not isinstance(image, dict) or not isinstance(image.get("image-path"), str):
            continue
        try:
            image_path = Path(image["image-path"]).resolve(strict=True)
        except OSError:
            continue
        if image_path == expected:
            matches.append(image)
    if len(matches) != 1:
        fail("hdiutil info did not bind exactly one image to the exact DMG source")
    image = matches[0]
    if image.get("writeable") is not False:
        fail("hdiutil reports the exact DMG image as writable")
    entities = image.get("system-entities")
    if not isinstance(entities, list):
        fail("hdiutil info exact image omitted system entities")
    selected = [
        entity
        for entity in entities
        if isinstance(entity, dict)
        and entity.get("dev-entry") == device
        and entity.get("mount-point") == os.fspath(mount_point)
    ]
    if len(selected) != 1:
        fail("hdiutil info did not cross-bind the exact device and mount point")
    return {
        "imagePathMatched": True,
        "imagePathSha256": sha256_bytes(os.fspath(expected).encode("utf-8")),
        "writeable": False,
    }


def validate_diskutil_info(
    info: Mapping[str, Any], mount_point: Path, device: str
) -> dict[str, Any]:
    if info.get("DeviceNode") != device or info.get("MountPoint") != os.fspath(mount_point):
        fail("diskutil did not cross-bind the exact device and mount point")
    if info.get("ReadOnlyMedia") is not True or info.get("ReadOnlyVolume") is not True:
        fail("diskutil does not report both read-only media and a read-only volume")
    owners = info.get("Owners")
    if owners not in (False, "Disabled"):
        fail("diskutil does not report ownership disabled for the mounted DMG")
    filesystem = info.get("FilesystemType") or info.get("TypeBundle") or info.get("Content")
    if not isinstance(filesystem, str) or not filesystem:
        fail("diskutil omitted the mounted filesystem identity")
    return {
        "filesystem": filesystem,
        "ownersDisabled": True,
        "readOnlyMedia": True,
        "readOnlyVolume": True,
    }


def validate_mount_output(raw: bytes, mount_point: Path, device: str) -> dict[str, Any]:
    try:
        lines = raw.decode("utf-8", "strict").splitlines()
    except UnicodeError as error:
        raise ProvenanceError("mount inventory is not UTF-8") from error
    prefix = f"{device} on {mount_point} ("
    matches = [line for line in lines if line.startswith(prefix) and line.endswith(")")]
    if len(matches) != 1:
        fail("mount inventory did not contain exactly one selected DMG device")
    options = {
        option.strip()
        for option in matches[0][len(prefix) : -1].split(",")
        if option.strip()
    }
    for required in ("read-only", "noowners", "nobrowse"):
        if required not in options:
            fail(f"mounted DMG lacks required {required} flag")
    if "read-write" in options:
        fail("mounted DMG unexpectedly reports read-write")
    return {"lineSha256": sha256_bytes((matches[0] + "\n").encode("utf-8")), "options": sorted(options)}


def dmg_root_inventory(mount_point: Path) -> list[dict[str, Any]]:
    try:
        children = sorted(os.scandir(mount_point), key=lambda item: item.name)
    except OSError as error:
        fail(f"cannot enumerate mounted DMG root: {error}")
    if not children or len(children) > 64:
        fail("mounted DMG has an empty or oversized top-level inventory")
    result: list[dict[str, Any]] = []
    app_count = 0
    for child in children:
        try:
            child.name.encode("ascii", "strict")
        except UnicodeEncodeError as error:
            raise ProvenanceError("mounted DMG contains a non-ASCII top-level entry") from error
        if any(character in child.name for character in "\r\n\t/"):
            fail("mounted DMG contains an unsafe top-level name")
        info = Path(child.path).lstat()
        if stat.S_ISDIR(info.st_mode):
            kind = "directory"
        elif stat.S_ISREG(info.st_mode):
            kind = "file"
        elif stat.S_ISLNK(info.st_mode):
            kind = "symlink"
        else:
            fail("mounted DMG contains a top-level special filesystem entry")
        if child.name.endswith(".app"):
            app_count += 1
            if child.name != APP_BUNDLE_NAME or kind != "directory":
                fail("mounted DMG contains an unexpected app bundle")
        target = os.readlink(child.path) if kind == "symlink" else None
        if child.name == "Applications" and target not in (None, "/Applications"):
            fail("mounted DMG Applications link targets an unexpected path")
        result.append({"kind": kind, "mode": stat.S_IMODE(info.st_mode), "name": child.name, "target": target})
    if app_count != 1:
        fail(f"mounted DMG must contain exactly one {APP_BUNDLE_NAME}")
    return result


def mac_tool_environment(home: Path, temporary: Path) -> dict[str, str]:
    private_directory(home, "macOS controller HOME")
    private_directory(temporary, "macOS controller TMPDIR")
    try:
        temporary.resolve(strict=True).relative_to(home.resolve(strict=True))
    except (OSError, ValueError) as error:
        raise ProvenanceError("macOS controller TMPDIR must be inside its private HOME") from error
    return {
        "HOME": os.fspath(home),
        "LANG": "C",
        "LC_ALL": "C",
        "PATH": FIXED_PATH,
        "TMPDIR": os.fspath(temporary),
    }


def code_signature(app: Path, environment: Mapping[str, str]) -> dict[str, Any]:
    codesign = Path("/usr/bin/codesign")
    verifier = executable_identity(codesign, "codesign")
    verified = run_command(
        [os.fspath(codesign), "--verify", "--deep", "--strict", "--verbose=4", os.fspath(app)],
        env=environment,
        timeout=120,
    )
    displayed = run_command(
        [os.fspath(codesign), "--display", "--verbose=4", os.fspath(app)],
        env=environment,
        timeout=120,
    )
    requirements = run_command(
        [os.fspath(codesign), "--display", "--requirements", "-", os.fspath(app)],
        env=environment,
        timeout=120,
    )
    display_text = (displayed.stdout + displayed.stderr).decode("utf-8", "strict")
    requirement_text = (requirements.stdout + requirements.stderr).decode("utf-8", "strict")
    lines = [line.strip() for line in display_text.splitlines() if line.strip()]
    identifier_rows = [line.split("=", 1)[1] for line in lines if line.startswith("Identifier=")]
    signature_rows = [line.split("=", 1)[1] for line in lines if line.startswith("Signature=")]
    team_rows = [line.split("=", 1)[1] for line in lines if line.startswith("TeamIdentifier=")]
    authorities = [line.split("=", 1)[1] for line in lines if line.startswith("Authority=")]
    code_directories = [line for line in lines if line.startswith("CodeDirectory ")]
    if identifier_rows != ["network.arc.desktop"]:
        fail("codesign identifier differs from network.arc.desktop")
    if signature_rows != ["adhoc"] or authorities:
        fail("macOS app is not truthfully one authority-free ad-hoc signature")
    if team_rows != ["not set"]:
        fail("ad-hoc macOS app unexpectedly carries or omits a TeamIdentifier assertion")
    if len(code_directories) != 1 or "adhoc" not in code_directories[0]:
        fail("codesign CodeDirectory does not report the ad-hoc flag")
    hardened_runtime = "runtime" in code_directories[0].lower()
    designated = [
        line.strip().removeprefix("# ")
        for line in requirement_text.splitlines()
        if line.strip().removeprefix("# ").startswith("designated =>")
    ]
    if len(designated) != 1:
        fail("codesign designated requirement is missing or ambiguous")
    cdhash_requirement = re.fullmatch(
        r'designated => cdhash H"[0-9a-f]{40}"(?: or cdhash H"[0-9a-f]{40}")*',
        designated[0],
    )
    if 'identifier "network.arc.desktop"' in designated[0]:
        designated_kind = "identifier"
    elif cdhash_requirement is not None:
        designated_kind = "cdhash"
    else:
        fail("codesign designated requirement has an unsupported ad-hoc form")

    info_plist = app / "Contents" / "Info.plist"
    raw_info = read_bytes(info_plist, "mounted app Info.plist", max_bytes=4 * 1024 * 1024)
    info = plist_object(raw_info, "mounted app Info.plist")
    if (
        info.get("CFBundleIdentifier") != "network.arc.desktop"
        or info.get("CFBundleExecutable") != "arc-desktop"
    ):
        fail("mounted app Info.plist identifier/executable differs")
    return {
        "appleDeveloperIdSigned": False,
        "authorities": [],
        "designatedRequirement": designated[0],
        "designatedRequirementKind": designated_kind,
        "displayStderrSha256": sha256_bytes(displayed.stderr),
        "displayStdoutSha256": sha256_bytes(displayed.stdout),
        "gatekeeperAssessed": False,
        "hardenedRuntime": hardened_runtime,
        "identifier": "network.arc.desktop",
        "infoPlistSha256": sha256_bytes(raw_info),
        "kind": "adhoc",
        "notarizationAssessed": False,
        "requirementsStderrSha256": sha256_bytes(requirements.stderr),
        "requirementsStdoutSha256": sha256_bytes(requirements.stdout),
        "teamIdentifier": None,
        "verifier": verifier,
        "verifyDeepStrict": True,
        "verifyStderrSha256": sha256_bytes(verified.stderr),
        "verifyStdoutSha256": sha256_bytes(verified.stdout),
    }


def semantic_code_signature(value: Mapping[str, Any]) -> dict[str, Any]:
    return {
        field: value[field]
        for field in (
            "appleDeveloperIdSigned",
            "authorities",
            "designatedRequirement",
            "designatedRequirementKind",
            "gatekeeperAssessed",
            "hardenedRuntime",
            "identifier",
            "infoPlistSha256",
            "kind",
            "notarizationAssessed",
            "teamIdentifier",
            "verifier",
            "verifyDeepStrict",
        )
    }


def validate_code_signature(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    require_exact_keys(
        value,
        {
            "appleDeveloperIdSigned",
            "authorities",
            "designatedRequirement",
            "designatedRequirementKind",
            "displayStderrSha256",
            "displayStdoutSha256",
            "gatekeeperAssessed",
            "hardenedRuntime",
            "identifier",
            "infoPlistSha256",
            "kind",
            "notarizationAssessed",
            "requirementsStderrSha256",
            "requirementsStdoutSha256",
            "teamIdentifier",
            "verifier",
            "verifyDeepStrict",
            "verifyStderrSha256",
            "verifyStdoutSha256",
        },
        label,
    )
    if (
        value["appleDeveloperIdSigned"] is not False
        or value["authorities"] != []
        or value["gatekeeperAssessed"] is not False
        or type(value["hardenedRuntime"]) is not bool
        or value["identifier"] != "network.arc.desktop"
        or value["kind"] != "adhoc"
        or value["notarizationAssessed"] is not False
        or value["teamIdentifier"] is not None
        or value["verifyDeepStrict"] is not True
    ):
        fail(f"{label} is not a truthful verified ad-hoc identity")
    requirement = value["designatedRequirement"]
    requirement_kind = value["designatedRequirementKind"]
    valid_identifier = (
        requirement_kind == "identifier"
        and isinstance(requirement, str)
        and 'identifier "network.arc.desktop"' in requirement
    )
    valid_cdhash = (
        requirement_kind == "cdhash"
        and isinstance(requirement, str)
        and re.fullmatch(
            r'designated => cdhash H"[0-9a-f]{40}"(?: or cdhash H"[0-9a-f]{40}")*',
            requirement,
        )
        is not None
    )
    if not (valid_identifier or valid_cdhash):
        fail(f"{label} designated requirement differs")
    for field in (
        "displayStderrSha256",
        "displayStdoutSha256",
        "infoPlistSha256",
        "requirementsStderrSha256",
        "requirementsStdoutSha256",
        "verifyStderrSha256",
        "verifyStdoutSha256",
    ):
        require_hash(value[field], f"{label} {field}")
    verifier = value["verifier"]
    require_exact_keys(verifier, {"path", "resolvedPath", "sha256", "size"}, f"{label} verifier")
    if verifier["path"] != "/usr/bin/codesign" or verifier["resolvedPath"] != "/usr/bin/codesign":
        fail(f"{label} codesign path differs")
    require_hash(verifier["sha256"], f"{label} verifier digest")
    positive_int(verifier["size"], f"{label} verifier size")
    return value


def attach_dmg(
    dmg: Path,
    mount_point: Path,
    environment: Mapping[str, str],
) -> tuple[dict[str, Any], str]:
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        fail("DMG inspection requires the audited macOS arm64 host")
    dmg_before = sha256_file(dmg, "macOS DMG before mount", max_bytes=MAX_ASSET_BYTES, private=True)
    if mount_point.exists() or mount_point.is_symlink():
        fail("DMG mount point must not already exist")
    create_directory(mount_point, "DMG mount point")
    tools = {
        "codesign": executable_identity(Path("/usr/bin/codesign"), "codesign"),
        "diskutil": executable_identity(Path("/usr/sbin/diskutil"), "diskutil"),
        "hdiutil": executable_identity(Path("/usr/bin/hdiutil"), "hdiutil"),
        "mount": executable_identity(Path("/sbin/mount"), "mount"),
    }
    verified = run_command(
        ["/usr/bin/hdiutil", "verify", os.fspath(dmg)], env=environment, timeout=300
    )
    attached = run_command(
        [
            "/usr/bin/hdiutil",
            "attach",
            "-readonly",
            "-noowners",
            "-nobrowse",
            "-plist",
            "-mountpoint",
            os.fspath(mount_point),
            os.fspath(dmg),
        ],
        env=environment,
        timeout=300,
    )
    device: str | None = None
    try:
        attach_plist = plist_object(attached.stdout, "hdiutil attach output")
        entity = mounted_entity(attach_plist, mount_point)
        device = entity["dev-entry"]
        info_result = run_command(["/usr/bin/hdiutil", "info", "-plist"], env=environment, timeout=60)
        hdiutil_info = plist_object(info_result.stdout, "hdiutil info output")
        source = validate_hdiutil_source(hdiutil_info, dmg, mount_point, device)
        disk_result = run_command(
            ["/usr/sbin/diskutil", "info", "-plist", device], env=environment, timeout=60
        )
        disk = validate_diskutil_info(
            plist_object(disk_result.stdout, "diskutil info output"), mount_point, device
        )
        mount_result = run_command(["/sbin/mount"], env=environment, timeout=30)
        mount_flags = validate_mount_output(mount_result.stdout, mount_point, device)
        statvfs = os.statvfs(mount_point)
        readonly_flag = getattr(os, "ST_RDONLY", 1)
        if not statvfs.f_flag & readonly_flag:
            fail("statvfs does not report the mounted DMG read only")
        if sha256_file(dmg, "macOS DMG after attach", max_bytes=MAX_ASSET_BYTES, private=True) != dmg_before:
            fail("macOS DMG bytes changed during attach")
    except BaseException as error:
        cleanup_target = device or os.fspath(mount_point)
        try:
            run_command(
                ["/usr/bin/hdiutil", "detach", cleanup_target],
                env=environment,
                timeout=180,
            )
        except BaseException as cleanup_error:
            raise ProvenanceError(
                f"DMG attach validation failed ({error}); emergency detach also failed: {cleanup_error}"
            ) from error
        raise
    evidence = {
        "attachPlistSha256": sha256_bytes(attached.stdout),
        "device": device,
        "diskutilInfoPlistSha256": sha256_bytes(disk_result.stdout),
        "filesystem": disk["filesystem"],
        "hdiutilInfoPlistSha256": sha256_bytes(info_result.stdout),
        "imagePathMatched": source["imagePathMatched"],
        "imagePathSha256": source["imagePathSha256"],
        "mountFlags": mount_flags,
        "mountPointBasename": mount_point.name,
        "nobrowse": True,
        "noowners": True,
        "readOnlyMedia": disk["readOnlyMedia"],
        "readOnlyVolume": disk["readOnlyVolume"],
        "statvfsReadOnly": True,
        "tools": tools,
        "verifyStderrSha256": sha256_bytes(verified.stderr),
        "verifyStdoutSha256": sha256_bytes(verified.stdout),
        "writeable": source["writeable"],
    }
    return evidence, device


def detach_dmg(
    dmg: Path,
    mount_point: Path,
    device: str,
    environment: Mapping[str, str],
) -> dict[str, Any]:
    detached = run_command(
        ["/usr/bin/hdiutil", "detach", device], env=environment, timeout=180
    )
    info_result = run_command(["/usr/bin/hdiutil", "info", "-plist"], env=environment, timeout=60)
    info = plist_object(info_result.stdout, "post-detach hdiutil info")
    images = info.get("images")
    if not isinstance(images, list):
        fail("post-detach hdiutil info omitted image inventory")
    expected = dmg.resolve(strict=True)
    for image in images:
        if isinstance(image, dict) and isinstance(image.get("image-path"), str):
            with contextlib.suppress(OSError):
                if Path(image["image-path"]).resolve(strict=True) == expected:
                    fail("exact DMG remains attached after hdiutil detach")
    private_directory(mount_point, "empty detached DMG mount point")
    if any(mount_point.iterdir()):
        fail("DMG mount point is not empty after detach")
    return {
        "detachStderrSha256": sha256_bytes(detached.stderr),
        "detachStdoutSha256": sha256_bytes(detached.stdout),
        "detached": True,
        "mountPointEmpty": True,
        "postDetachInfoPlistSha256": sha256_bytes(info_result.stdout),
    }


def validate_signature_receipt(
    path: Path,
    binding: Mapping[str, Any],
    binding_raw: bytes,
    assets: Mapping[str, Mapping[str, Any]],
    source: Mapping[str, Any],
    updater_key: bytes,
) -> tuple[dict[str, Any], bytes]:
    value, raw = load_json(path, "Lima updater signature receipt")
    require_exact_keys(
        value,
        {
            "assets",
            "bindingSha256",
            "completedAt",
            "disposableVm",
            "guest",
            "guestReceiptSha256",
            "limactl",
            "release",
            "repository",
            "schema",
            "source",
            "sourceCommit",
            "updaterPublicKeySha256",
            "verified",
        },
        "Lima updater signature receipt",
    )
    if (
        value["schema"] != SIGNATURE_SCHEMA
        or value["repository"] != REPOSITORY
        or value["sourceCommit"] != binding["commit"]
        or value["bindingSha256"] != sha256_bytes(binding_raw)
        or value["updaterPublicKeySha256"] != sha256_bytes(updater_key)
        or value["verified"] is not True
    ):
        fail("Lima updater signature receipt release/source/key binding differs")
    if value["source"] != source:
        fail("Lima updater signature receipt used another controller source")
    expected_release = {
        "id": binding["release"]["id"],
        "runAttempt": binding["release_workflow"]["run_attempt"],
        "runId": binding["release_workflow"]["run_id"],
        "tag": binding["tag"],
    }
    if value["release"] != expected_release:
        fail("Lima updater signature receipt release identity differs")
    expected_assets = {
        "appArchive": assets[APP_ARCHIVE_NAME],
        "appArchiveSignature": assets[APP_SIGNATURE_NAME],
    }
    if value["assets"] != expected_assets:
        fail("Lima updater signature receipt asset identity differs")
    vm = value["disposableVm"]
    require_exact_keys(
        vm,
        {
            "configSha256",
            "deletedAfterEvidenceRead",
            "imageDigest",
            "imageUrl",
            "mounts",
            "name",
            "preexistingInstancesUnchanged",
            "recoveryEnclaveAccessed",
        },
        "Lima updater signature VM",
    )
    if (
        vm["configSha256"] != sha256_bytes(lima_config())
        or vm["deletedAfterEvidenceRead"] is not True
        or vm["imageDigest"] != LIMA_IMAGE_DIGEST
        or vm["imageUrl"] != LIMA_IMAGE_URL
        or vm["mounts"] != []
        or vm["preexistingInstancesUnchanged"] is not True
        or vm["recoveryEnclaveAccessed"] is not False
        or not isinstance(vm["name"], str)
        or VM_NAME_RE.fullmatch(vm["name"]) is None
    ):
        fail("Lima updater signature VM proof differs from the pinned disposable VM")
    limactl = value["limactl"]
    require_exact_keys(limactl, {"path", "resolvedPath", "sha256", "size"}, "signature limactl")
    require_hash(limactl["sha256"], "signature limactl digest")
    positive_int(limactl["size"], "signature limactl size")
    guest = value["guest"]
    if not isinstance(guest, dict):
        fail("Lima updater signature receipt omits guest proof")
    validate_guest_signature(
        guest,
        archive_sha256=assets[APP_ARCHIVE_NAME]["sha256"],
        signature_sha256=assets[APP_SIGNATURE_NAME]["sha256"],
        public_key_sha256=sha256_bytes(updater_key),
        helper_sha256=source["sha256"],
    )
    if sha256_bytes(canonical_json(guest)) != value["guestReceiptSha256"]:
        fail("Lima updater signature guest receipt digest differs")
    return value, raw


def common_inputs(args: argparse.Namespace) -> tuple[
    dict[str, Any], bytes, dict[str, dict[str, Any]], dict[str, Any], bytes, dict[str, Any], bytes
]:
    binding, binding_raw = load_release_binding(args.release_binding)
    assets = bound_assets(binding, args.asset_directory)
    source = verify_source_checkout(args.repository_root, binding["commit"])
    updater_key = canonical_updater_public_key(args.repository_root, binding["commit"])
    signature, signature_raw = validate_signature_receipt(
        args.signature_receipt,
        binding,
        binding_raw,
        assets,
        source,
        updater_key,
    )
    return binding, binding_raw, assets, source, updater_key, signature, signature_raw


def bundle_identity(app: Path) -> dict[str, Any]:
    tree = app_bundle_tree(app, "macOS app bundle")
    executable = app / EXECUTABLE_RELATIVE_PATH
    info = regular_file(executable, "macOS app executable", max_bytes=MAX_ARCHIVE_MEMBER_BYTES)
    if not info.st_mode & 0o111 or info.st_nlink != 1:
        fail("macOS app executable is not an executable single-link regular file")
    return {
        "appBundleTreeSha256": tree["sha256"],
        "executableRelativePath": EXECUTABLE_RELATIVE_PATH,
        "executableSha256": sha256_file(
            executable, "macOS app executable", max_bytes=MAX_ARCHIVE_MEMBER_BYTES
        ),
        "executableSize": info.st_size,
    }


def inspect_package(args: argparse.Namespace) -> dict[str, Any]:
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        fail("macOS package inspection requires a macOS arm64 host")
    (
        binding,
        binding_raw,
        assets,
        source,
        _updater_key,
        signature,
        signature_raw,
    ) = common_inputs(args)
    output = require_absolute(args.output, "package inspection output")
    if output.name != "MACOS-PACKAGE-INSPECTION.json" or output.exists() or output.is_symlink():
        fail("package inspection output must be one absent MACOS-PACKAGE-INSPECTION.json")
    private_directory(output.parent, "package inspection output parent")
    environment = mac_tool_environment(args.controller_home, args.controller_tmpdir)
    if args.extraction_root.name != "macos-app-extraction" or args.mount_point.name != "inspect-dmg-mount":
        fail("inspection extraction/mount basenames differ from the canonical controller layout")
    archive = args.asset_directory / APP_ARCHIVE_NAME
    dmg = args.asset_directory / DMG_NAME
    extraction = safe_extract_archive(archive, args.extraction_root)
    if extraction["archiveSha256AfterExtraction"] != assets[APP_ARCHIVE_NAME]["sha256"]:
        fail("updater archive digest changed across safe extraction")
    archive_app = args.extraction_root / APP_BUNDLE_NAME
    archive_bundle = bundle_identity(archive_app)
    if archive_bundle["appBundleTreeSha256"] != extraction["appBundleTreeSha256"]:
        fail("safe extraction summary and extracted bundle identity differ")

    mount_evidence: dict[str, Any] | None = None
    detach_evidence: dict[str, Any] | None = None
    device: str | None = None
    primary_error: BaseException | None = None
    mounted_tree: dict[str, Any] | None = None
    mounted_bundle: dict[str, Any] | None = None
    signature_identity: dict[str, Any] | None = None
    root_inventory: list[dict[str, Any]] | None = None
    try:
        mount_evidence, device = attach_dmg(dmg, args.mount_point, environment)
        root_inventory = dmg_root_inventory(args.mount_point)
        mounted_app = args.mount_point / APP_BUNDLE_NAME
        mounted_tree = app_bundle_tree(mounted_app, "mounted DMG app bundle")
        mounted_bundle = bundle_identity(mounted_app)
        if mounted_bundle != archive_bundle:
            fail("updater archive and exact DMG contain different complete app identities")
        signature_identity = code_signature(mounted_app, environment)
    except BaseException as error:
        primary_error = error
    finally:
        if device is not None:
            try:
                detach_evidence = detach_dmg(dmg, args.mount_point, device, environment)
            except BaseException as error:
                if primary_error is None:
                    primary_error = error
                else:
                    primary_error = ProvenanceError(f"{primary_error}; DMG detach also failed: {error}")
    if primary_error is not None:
        raise primary_error
    if any(
        value is None
        for value in (
            mount_evidence,
            detach_evidence,
            mounted_tree,
            mounted_bundle,
            signature_identity,
            root_inventory,
        )
    ):
        fail("macOS package inspection omitted mandatory mounted evidence")
    if sha256_file(dmg, "macOS DMG after detach", max_bytes=MAX_ASSET_BYTES, private=True) != assets[DMG_NAME]["sha256"]:
        fail("macOS DMG bytes changed across read-only inspection")
    if bundle_identity(archive_app) != archive_bundle:
        fail("extracted updater app changed during DMG inspection")

    receipt = {
        "assets": {
            "appArchive": assets[APP_ARCHIVE_NAME],
            "appArchiveSignature": assets[APP_SIGNATURE_NAME],
            "dmg": assets[DMG_NAME],
        },
        "bindingSha256": sha256_bytes(binding_raw),
        "bundle": archive_bundle,
        "codeSignature": signature_identity,
        "completedAt": utc_now(),
        "dmg": {
            "appBundleTree": mounted_tree,
            "attach": mount_evidence,
            "detach": detach_evidence,
            "rootInventory": root_inventory,
            "sha256AfterDetach": assets[DMG_NAME]["sha256"],
        },
        "extraction": extraction,
        "release": {
            "id": binding["release"]["id"],
            "runAttempt": binding["release_workflow"]["run_attempt"],
            "runId": binding["release_workflow"]["run_id"],
            "tag": binding["tag"],
        },
        "repository": REPOSITORY,
        "schema": INSPECTION_SCHEMA,
        "source": source,
        "sourceCommit": binding["commit"],
        "updaterSignature": {
            "receiptSha256": sha256_bytes(signature_raw),
            "schema": signature["schema"],
            "updaterPublicKeySha256": signature["updaterPublicKeySha256"],
            "verified": True,
        },
    }
    create_file(output, canonical_json(receipt), "macOS package inspection", 0o400)
    return receipt


def validate_bundle(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    require_exact_keys(
        value,
        {"appBundleTreeSha256", "executableRelativePath", "executableSha256", "executableSize"},
        label,
    )
    require_hash(value["appBundleTreeSha256"], f"{label} tree digest")
    require_hash(value["executableSha256"], f"{label} executable digest")
    positive_int(value["executableSize"], f"{label} executable size")
    if value["executableRelativePath"] != EXECUTABLE_RELATIVE_PATH:
        fail(f"{label} executable path differs")
    return value


def validate_inspection(
    path: Path,
    binding: Mapping[str, Any],
    binding_raw: bytes,
    assets: Mapping[str, Mapping[str, Any]],
    source: Mapping[str, Any],
    signature_raw: bytes,
    updater_key: bytes,
) -> tuple[dict[str, Any], bytes]:
    value, raw = load_json(path, "macOS package inspection")
    require_exact_keys(
        value,
        {
            "assets",
            "bindingSha256",
            "bundle",
            "codeSignature",
            "completedAt",
            "dmg",
            "extraction",
            "release",
            "repository",
            "schema",
            "source",
            "sourceCommit",
            "updaterSignature",
        },
        "macOS package inspection",
    )
    if (
        value["schema"] != INSPECTION_SCHEMA
        or value["repository"] != REPOSITORY
        or value["sourceCommit"] != binding["commit"]
        or value["bindingSha256"] != sha256_bytes(binding_raw)
        or value["source"] != source
    ):
        fail("macOS package inspection release/source binding differs")
    expected_assets = {
        "appArchive": assets[APP_ARCHIVE_NAME],
        "appArchiveSignature": assets[APP_SIGNATURE_NAME],
        "dmg": assets[DMG_NAME],
    }
    if value["assets"] != expected_assets:
        fail("macOS package inspection assets differ")
    expected_release = {
        "id": binding["release"]["id"],
        "runAttempt": binding["release_workflow"]["run_attempt"],
        "runId": binding["release_workflow"]["run_id"],
        "tag": binding["tag"],
    }
    if value["release"] != expected_release:
        fail("macOS package inspection release identity differs")
    validate_bundle(value["bundle"], "macOS package inspection bundle")
    updater = value["updaterSignature"]
    require_exact_keys(
        updater,
        {"receiptSha256", "schema", "updaterPublicKeySha256", "verified"},
        "macOS package inspection updater signature",
    )
    if (
        updater["receiptSha256"] != sha256_bytes(signature_raw)
        or updater["schema"] != SIGNATURE_SCHEMA
        or updater["updaterPublicKeySha256"] != sha256_bytes(updater_key)
        or updater["verified"] is not True
    ):
        fail("macOS package inspection updater signature binding differs")
    code = validate_code_signature(value["codeSignature"], "macOS package inspection code signature")
    if semantic_code_signature(code) != {
        "appleDeveloperIdSigned": False,
        "authorities": [],
        "designatedRequirement": code.get("designatedRequirement"),
        "designatedRequirementKind": code.get("designatedRequirementKind"),
        "gatekeeperAssessed": False,
        "hardenedRuntime": code["hardenedRuntime"],
        "identifier": "network.arc.desktop",
        "infoPlistSha256": code.get("infoPlistSha256"),
        "kind": "adhoc",
        "notarizationAssessed": False,
        "teamIdentifier": None,
        "verifier": code["verifier"],
        "verifyDeepStrict": True,
    }:
        fail("macOS package inspection code-signature truth differs")
    dmg = value["dmg"]
    if not isinstance(dmg, dict) or dmg.get("detach", {}).get("detached") is not True:
        fail("macOS package inspection did not detach its exact DMG")
    extraction = value["extraction"]
    if not isinstance(extraction, dict) or extraction.get("appBundleTreeSha256") != value["bundle"]["appBundleTreeSha256"]:
        fail("macOS package inspection extraction tree differs")
    mounted_tree = dmg.get("appBundleTree")
    if not isinstance(mounted_tree, dict) or mounted_tree.get("sha256") != value["bundle"]["appBundleTreeSha256"]:
        fail("macOS package inspection mounted tree differs")
    return value, raw


NATIVE_INPUT_KEYS = {
    "assets",
    "budgets",
    "challenge",
    "expectedActiveValidators",
    "expectedBundle",
    "expectedCoordinator",
    "expectedModelId",
    "expectedRegisteredValidators",
    "expiresAtUnix",
    "frontendCommit",
    "frontendConfigSha256",
    "issuedAtUnix",
    "maximumBlockAgeSeconds",
    "minimumPeers",
    "pagesOrigin",
    "recoveryEpoch",
    "releaseId",
    "releaseRunAttempt",
    "releaseRunId",
    "releaseVersion",
    "repository",
    "rolloutManifestSha256",
    "schema",
    "sourceCommit",
    "transactionDomain",
    "validatorApprovalsRequired",
    "validatorSetCommitment",
    "validatorSetId",
}

NATIVE_BUDGETS = {
    "dispatchMaxWaitMs": 3_960_000,
    "earningsMaxPolls": 11,
    "earningsMaxWaitMs": 30_000,
    "finalReadMaxPolls": 11,
    "finalReadMaxWaitMs": 30_000,
    "preflightMaxWaitMs": 30_000,
    "receiptMaxPolls": 61,
    "receiptMaxWaitMs": 180_000,
    "totalMaxWaitMs": 4_300_000,
}


def validate_native_input(
    path: Path,
    binding: Mapping[str, Any],
    assets: Mapping[str, Mapping[str, Any]],
    expected_bundle: Mapping[str, Any],
) -> tuple[dict[str, Any], bytes]:
    if path.name != "DESKTOP-LIVE-INPUT.json":
        fail("native input basename must be DESKTOP-LIVE-INPUT.json")
    value, raw = load_json(path, "packaged native input")
    require_exact_keys(value, NATIVE_INPUT_KEYS, "packaged native input")
    if (
        value["schema"] != NATIVE_INPUT_SCHEMA
        or value["repository"] != REPOSITORY
        or value["sourceCommit"] != binding["commit"]
        or value["releaseVersion"] != "0.8.0"
        or value["releaseId"] != binding["release"]["id"]
        or value["releaseRunId"] != binding["release_workflow"]["run_id"]
        or value["releaseRunAttempt"] != binding["release_workflow"]["run_attempt"]
        or value["pagesOrigin"] != "https://ferrumvir.github.io/arc-chain"
        or value["expectedCoordinator"] != "https://140.82.16.112"
    ):
        fail("packaged native input release/source/origin identity differs")
    for field in ("frontendCommit",):
        require_commit(value[field], f"packaged native input {field}")
    for field in ("challenge", "frontendConfigSha256", "rolloutManifestSha256"):
        require_hash(value[field], f"packaged native input {field}")
    for field in ("expectedModelId", "transactionDomain", "validatorSetCommitment"):
        raw_hash = value[field]
        if not isinstance(raw_hash, str) or not raw_hash.startswith("0x"):
            fail(f"packaged native input {field} lacks its canonical 0x prefix")
        require_hash(raw_hash[2:], f"packaged native input {field}")
    if value["budgets"] != NATIVE_BUDGETS:
        fail("packaged native input budgets differ from the shipped executable")
    if value["expectedBundle"] != expected_bundle:
        fail("packaged native input expected bundle differs from inspected packages")
    expected_assets = {
        "appArchive": assets[APP_ARCHIVE_NAME],
        "appArchiveSignature": assets[APP_SIGNATURE_NAME],
        "dmg": assets[DMG_NAME],
    }
    if value["assets"] != expected_assets:
        fail("packaged native input assets differ from immutable release assets")
    now = int(dt.datetime.now(dt.timezone.utc).timestamp())
    if (
        type(value["issuedAtUnix"]) is not int
        or type(value["expiresAtUnix"]) is not int
        or value["expiresAtUnix"] <= value["issuedAtUnix"]
        or value["expiresAtUnix"] - value["issuedAtUnix"] > 21_600
        or value["issuedAtUnix"] > now + 60
        or value["expiresAtUnix"] - now < NATIVE_OUTER_TIMEOUT_SECONDS
    ):
        fail("packaged native input is stale/future-dated or lacks its full outer runtime window")
    if (
        value["expectedActiveValidators"] != 6
        or value["expectedRegisteredValidators"] != 6
        or type(value["minimumPeers"]) is not int
        or value["minimumPeers"] < 5
        or type(value["maximumBlockAgeSeconds"]) is not int
        or not 1 <= value["maximumBlockAgeSeconds"] <= 300
        or type(value["recoveryEpoch"]) is not int
        or value["recoveryEpoch"] <= 0
        or type(value["validatorSetId"]) is not int
        or value["validatorSetId"] <= 0
        or type(value["validatorApprovalsRequired"]) is not int
        or value["validatorApprovalsRequired"] != 5
    ):
        fail("packaged native input network/reward policy differs")
    return value, raw


def require_hash32(value: object, label: str) -> str:
    if not isinstance(value, str) or not value.startswith("0x"):
        fail(f"{label} must be canonical 0x-prefixed lowercase hex")
    require_hash(value[2:], label)
    return value


def build_native_input(args: argparse.Namespace) -> dict[str, Any]:
    (
        binding,
        binding_raw,
        assets,
        source,
        updater_key,
        _signature,
        signature_raw,
    ) = common_inputs(args)
    inspection, _inspection_raw = validate_inspection(
        args.inspection,
        binding,
        binding_raw,
        assets,
        source,
        signature_raw,
        updater_key,
    )
    output = require_absolute(args.output, "packaged native input output")
    if output.name != "DESKTOP-LIVE-INPUT.json" or output.exists() or output.is_symlink():
        fail("native input output must be one absent DESKTOP-LIVE-INPUT.json")
    private_directory(output.parent, "packaged native input output parent")
    frontend_commit = require_commit(args.frontend_commit, "native input frontend commit")
    frontend_config = require_hash(
        args.frontend_config_sha256, "native input frontend config digest"
    )
    rollout = require_hash(
        args.rollout_manifest_sha256, "native input rollout manifest digest"
    )
    model = require_hash32(args.expected_model_id, "native input expected model")
    validator_commitment = require_hash32(
        args.validator_set_commitment, "native input validator-set commitment"
    )
    transaction_domain = require_hash32(
        args.transaction_domain, "native input transaction domain"
    )
    recovery_epoch = positive_int(args.recovery_epoch, "native input recovery epoch")
    validator_set_id = positive_int(args.validator_set_id, "native input validator-set ID")
    if not 4_500 <= args.validity_seconds <= 21_600:
        fail("native input validity must be between 4,500 and 21,600 seconds")
    if not 1 <= args.maximum_block_age_seconds <= 300:
        fail("native input maximum block age must be between 1 and 300 seconds")
    if args.minimum_peers < 5:
        fail("native input minimum peer requirement must be at least five")
    issued = int(dt.datetime.now(dt.timezone.utc).timestamp())
    value = {
        "assets": inspection["assets"],
        "budgets": dict(NATIVE_BUDGETS),
        "challenge": os.urandom(32).hex(),
        "expectedActiveValidators": 6,
        "expectedBundle": inspection["bundle"],
        "expectedCoordinator": "https://140.82.16.112",
        "expectedModelId": model,
        "expectedRegisteredValidators": 6,
        "expiresAtUnix": issued + args.validity_seconds,
        "frontendCommit": frontend_commit,
        "frontendConfigSha256": frontend_config,
        "issuedAtUnix": issued,
        "maximumBlockAgeSeconds": args.maximum_block_age_seconds,
        "minimumPeers": args.minimum_peers,
        "pagesOrigin": "https://ferrumvir.github.io/arc-chain",
        "recoveryEpoch": recovery_epoch,
        "releaseId": binding["release"]["id"],
        "releaseRunAttempt": binding["release_workflow"]["run_attempt"],
        "releaseRunId": binding["release_workflow"]["run_id"],
        "releaseVersion": "0.8.0",
        "repository": REPOSITORY,
        "rolloutManifestSha256": rollout,
        "schema": NATIVE_INPUT_SCHEMA,
        "sourceCommit": binding["commit"],
        "transactionDomain": transaction_domain,
        "validatorApprovalsRequired": 5,
        "validatorSetCommitment": validator_commitment,
        "validatorSetId": validator_set_id,
    }
    # Exercise the same validator used immediately before execution.  The
    # output does not exist yet, so materialize bytes only after all scalar,
    # release, asset, and package checks pass.
    raw = canonical_json(value)
    create_file(output, raw, "packaged native input", 0o400)
    validate_native_input(output, binding, assets, inspection["bundle"])
    return value


def native_environment(home: Path) -> dict[str, str]:
    temporary = home / "tmp"
    return {
        "HOME": os.fspath(home),
        "LANG": "C",
        "LC_ALL": "C",
        "PATH": FIXED_PATH,
        "TMPDIR": os.fspath(temporary),
    }


def environment_sha256(environment: Mapping[str, str]) -> str:
    raw = "".join(f"{name}={environment[name]}\0" for name in sorted(environment)).encode("utf-8")
    return sha256_bytes(raw)


def limit_native_child() -> None:
    os.umask(0o077)
    resource.setrlimit(
        resource.RLIMIT_FSIZE,
        (MAX_COMMAND_OUTPUT_BYTES, MAX_COMMAND_OUTPUT_BYTES),
    )


def run_native_process(
    executable: Path,
    native_input: Path,
    native_output: Path,
    environment: Mapping[str, str],
) -> dict[str, Any]:
    started = dt.datetime.now(dt.timezone.utc)
    started_monotonic = time.monotonic()
    with tempfile.TemporaryFile() as stdout_file, tempfile.TemporaryFile() as stderr_file:
        try:
            process = subprocess.Popen(
                [
                    os.fspath(executable),
                    "--arc-production-acceptance",
                    os.fspath(native_input),
                    os.fspath(native_output),
                ],
                cwd=native_input.parent,
                env=dict(environment),
                stdin=subprocess.DEVNULL,
                stdout=stdout_file,
                stderr=stderr_file,
                start_new_session=True,
                close_fds=True,
                preexec_fn=limit_native_child,
            )
        except OSError as error:
            raise ProvenanceError(f"mounted native executable could not start: {error}") from error
        timed_out = False
        try:
            returncode = process.wait(timeout=NATIVE_OUTER_TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired:
            timed_out = True
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
            returncode = process.wait(timeout=30)
        lingering_group = False
        if not timed_out:
            try:
                os.killpg(process.pid, 0)
                lingering_group = True
            except ProcessLookupError:
                pass
            if lingering_group:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(process.pid, signal.SIGKILL)
        completed = dt.datetime.now(dt.timezone.utc)
        elapsed_ms = max(0, int((time.monotonic() - started_monotonic) * 1_000))
        stdout_file.seek(0)
        stderr_file.seek(0)
        stdout = stdout_file.read(MAX_COMMAND_OUTPUT_BYTES + 1)
        stderr = stderr_file.read(MAX_COMMAND_OUTPUT_BYTES + 1)
    if len(stdout) > MAX_COMMAND_OUTPUT_BYTES or len(stderr) > MAX_COMMAND_OUTPUT_BYTES:
        fail("mounted native executable exceeded its bounded stdout/stderr budget")
    evidence = {
        "completedAt": completed.isoformat().replace("+00:00", "Z"),
        "elapsedMs": elapsed_ms,
        "innerMaxSeconds": NATIVE_INNER_MAX_SECONDS,
        "outerTimeoutSeconds": NATIVE_OUTER_TIMEOUT_SECONDS,
        "returnCode": returncode,
        "startedAt": started.isoformat().replace("+00:00", "Z"),
        "stderrSha256": sha256_bytes(stderr),
        "stderrSize": len(stderr),
        "stdoutSha256": sha256_bytes(stdout),
        "stdoutSize": len(stdout),
        "timedOut": timed_out,
    }
    if timed_out:
        fail("mounted native acceptance exceeded its outer timeout; outcome is terminal/ambiguous")
    if lingering_group:
        fail("mounted native acceptance left a process in its isolated process group")
    if returncode != 0:
        tail = stderr.decode("utf-8", "replace")[-2_000:]
        fail(f"mounted native acceptance failed rc={returncode}: {tail}")
    return evidence


def validate_native_outputs(
    native_input: Mapping[str, Any],
    input_raw: bytes,
    native_output_path: Path,
    attempt_path: Path,
    expected_bundle: Mapping[str, Any],
    expected_environment_sha256: str,
) -> tuple[dict[str, Any], bytes, dict[str, Any], bytes]:
    receipt, receipt_raw = load_json(native_output_path, "packaged native acceptance receipt")
    attempt, attempt_raw = load_json(attempt_path, "packaged native dispatch attempt")
    require_exact_keys(
        attempt,
        {
            "armedAt",
            "challenge",
            "dispatchLimit",
            "executableSha256",
            "inputSha256",
            "schema",
            "sourceCommit",
            "sourceHost",
        },
        "packaged native dispatch attempt",
    )
    input_sha256 = sha256_bytes(input_raw)
    if (
        attempt["schema"] != NATIVE_ATTEMPT_SCHEMA
        or attempt["challenge"] != native_input["challenge"]
        or attempt["dispatchLimit"] != 1
        or attempt["executableSha256"] != expected_bundle["executableSha256"]
        or attempt["inputSha256"] != input_sha256
        or attempt["sourceCommit"] != native_input["sourceCommit"]
        or attempt["sourceHost"] != native_input["expectedCoordinator"]
    ):
        fail("packaged native dispatch attempt binding differs")
    mandatory = {
        "assets",
        "bundle",
        "challenge",
        "dispatch",
        "dispatchAttemptSha256",
        "inputSha256",
        "repository",
        "runtime",
        "schema",
        "sourceCommit",
    }
    if not mandatory.issubset(receipt):
        fail("packaged native acceptance receipt omits mandatory controller bindings")
    if (
        receipt["schema"] != NATIVE_OUTPUT_SCHEMA
        or receipt["repository"] != REPOSITORY
        or receipt["sourceCommit"] != native_input["sourceCommit"]
        or receipt["challenge"] != native_input["challenge"]
        or receipt["assets"] != native_input["assets"]
        or receipt["bundle"] != expected_bundle
        or receipt["inputSha256"] != input_sha256
        or receipt["dispatchAttemptSha256"] != sha256_bytes(attempt_raw)
    ):
        fail("packaged native acceptance receipt release/input/bundle binding differs")
    dispatch = receipt["dispatch"]
    if not isinstance(dispatch, dict) or dispatch.get("count") != 1:
        fail("packaged native acceptance receipt does not prove exactly one dispatch")
    runtime = receipt["runtime"]
    if (
        not isinstance(runtime, dict)
        or runtime.get("environmentNames") != ["HOME", "LANG", "LC_ALL", "PATH", "TMPDIR"]
        or runtime.get("environmentSha256") != expected_environment_sha256
        or runtime.get("tauriBuilderStarted") is not False
        or runtime.get("pluginsLoaded") is not False
        or runtime.get("ipcHandlersRegistered") is not False
        or runtime.get("webviewsCreated") != 0
    ):
        fail("packaged native acceptance runtime/environment truth differs")
    return receipt, receipt_raw, attempt, attempt_raw


def run_package(args: argparse.Namespace) -> dict[str, Any]:
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        fail("packaged native execution requires a macOS arm64 host")
    (
        binding,
        binding_raw,
        assets,
        source,
        updater_key,
        _signature,
        signature_raw,
    ) = common_inputs(args)
    inspection, inspection_raw = validate_inspection(
        args.inspection,
        binding,
        binding_raw,
        assets,
        source,
        signature_raw,
        updater_key,
    )
    if args.extraction_root.name != "macos-app-extraction" or args.mount_point.name != "native-dmg-mount":
        fail("native extraction/mount basenames differ from the canonical controller layout")
    extracted_app = args.extraction_root / APP_BUNDLE_NAME
    extracted_bundle = bundle_identity(extracted_app)
    if extracted_bundle != inspection["bundle"]:
        fail("safely extracted updater app changed after package inspection")
    native_input, input_raw = validate_native_input(
        args.native_input, binding, assets, inspection["bundle"]
    )
    native_output = require_absolute(args.native_output, "native acceptance output")
    provenance_output = require_absolute(args.output, "macOS package provenance output")
    if native_output.name != "PACKAGED-NATIVE-ACCEPTANCE.json":
        fail("native output basename must be PACKAGED-NATIVE-ACCEPTANCE.json")
    if provenance_output.name != "MACOS-PACKAGE-PROVENANCE.json":
        fail("provenance output basename must be MACOS-PACKAGE-PROVENANCE.json")
    if args.native_input.parent != native_output.parent or native_output.parent != provenance_output.parent:
        fail("native input, native output, and provenance must share one private directory")
    private_directory(native_output.parent, "native acceptance output directory")
    attempt_path = native_output.with_name("PACKAGED-NATIVE-DISPATCH-ATTEMPT.json")
    controller_attempt_path = native_output.with_name("MACOS-NATIVE-CONTROLLER-ATTEMPT.json")
    for path, label in (
        (native_output, "native acceptance output"),
        (provenance_output, "macOS package provenance"),
        (attempt_path, "native dispatch attempt"),
        (controller_attempt_path, "native controller attempt"),
    ):
        if path.exists() or path.is_symlink():
            fail(f"{label} already exists; preserve it and never retry this input")
    native_home = require_absolute(args.native_home, "native isolated HOME")
    if native_home.name != "isolated-home" or native_home.exists() or native_home.is_symlink():
        fail("native isolated HOME must be one absent isolated-home directory")
    environment = native_environment(native_home)
    expected_environment_sha256 = environment_sha256(environment)
    tool_environment = mac_tool_environment(args.controller_home, args.controller_tmpdir)
    dmg = args.asset_directory / DMG_NAME

    mount_evidence: dict[str, Any] | None = None
    detach_evidence: dict[str, Any] | None = None
    device: str | None = None
    primary_error: BaseException | None = None
    mounted_bundle_before: dict[str, Any] | None = None
    mounted_bundle_after: dict[str, Any] | None = None
    tree_before: dict[str, Any] | None = None
    tree_after: dict[str, Any] | None = None
    signature_before: dict[str, Any] | None = None
    signature_after: dict[str, Any] | None = None
    process_evidence: dict[str, Any] | None = None
    controller_attempt_raw: bytes | None = None
    native_receipt: dict[str, Any] | None = None
    native_receipt_raw: bytes | None = None
    native_attempt: dict[str, Any] | None = None
    native_attempt_raw: bytes | None = None
    try:
        mount_evidence, device = attach_dmg(dmg, args.mount_point, tool_environment)
        mounted_app = args.mount_point / APP_BUNDLE_NAME
        dmg_root_inventory(args.mount_point)
        tree_before = app_bundle_tree(mounted_app, "mounted app before native execution")
        mounted_bundle_before = bundle_identity(mounted_app)
        if mounted_bundle_before != inspection["bundle"] or mounted_bundle_before != extracted_bundle:
            fail("freshly mounted app differs from inspection/updater archive")
        signature_before = code_signature(mounted_app, tool_environment)
        if semantic_code_signature(signature_before) != semantic_code_signature(inspection["codeSignature"]):
            fail("freshly mounted app code-signature truth differs from inspection")
        executable = mounted_app / EXECUTABLE_RELATIVE_PATH
        # All package, mount, and code-seal checks remain safely pre-dispatch.
        # Create the isolated state and durable no-retry marker only once the
        # exact mounted executable is ready to be spawned.
        create_directory(native_home, "native isolated HOME")
        create_directory(native_home / "tmp", "native isolated TMPDIR")
        controller_attempt = {
            "armedAt": utc_now(),
            "challenge": native_input["challenge"],
            "executableSha256": mounted_bundle_before["executableSha256"],
            "inputSha256": sha256_bytes(input_raw),
            "inspectionSha256": sha256_bytes(inspection_raw),
            "outerTimeoutSeconds": NATIVE_OUTER_TIMEOUT_SECONDS,
            "retryPermitted": False,
            "schema": "arc.macos-native-controller-attempt.v1",
            "sourceCommit": binding["commit"],
            "state": "armed-no-retry",
        }
        controller_attempt_raw = canonical_json(controller_attempt)
        create_file(
            controller_attempt_path,
            controller_attempt_raw,
            "macOS native controller no-retry attempt",
            0o400,
        )
        process_evidence = run_native_process(
            executable, args.native_input, native_output, environment
        )
        tree_after = app_bundle_tree(mounted_app, "mounted app after native execution")
        mounted_bundle_after = bundle_identity(mounted_app)
        signature_after = code_signature(mounted_app, tool_environment)
        if tree_after != tree_before or mounted_bundle_after != mounted_bundle_before:
            fail("mounted app tree/executable changed during native execution")
        if semantic_code_signature(signature_after) != semantic_code_signature(signature_before):
            fail("mounted app code-signature truth changed during native execution")
        native_receipt, native_receipt_raw, native_attempt, native_attempt_raw = validate_native_outputs(
            native_input,
            input_raw,
            native_output,
            attempt_path,
            inspection["bundle"],
            expected_environment_sha256,
        )
    except BaseException as error:
        primary_error = error
    finally:
        if device is not None:
            try:
                detach_evidence = detach_dmg(dmg, args.mount_point, device, tool_environment)
            except BaseException as error:
                if primary_error is None:
                    primary_error = error
                else:
                    primary_error = ProvenanceError(f"{primary_error}; DMG detach also failed: {error}")
    if primary_error is not None:
        raise primary_error
    if any(
        value is None
        for value in (
            mount_evidence,
            detach_evidence,
            mounted_bundle_before,
            mounted_bundle_after,
            tree_before,
            tree_after,
            signature_before,
            signature_after,
            process_evidence,
            controller_attempt_raw,
            native_receipt,
            native_receipt_raw,
            native_attempt,
            native_attempt_raw,
        )
    ):
        fail("successful native execution omitted mandatory package evidence")
    if sha256_file(dmg, "macOS DMG after native detach", max_bytes=MAX_ASSET_BYTES, private=True) != assets[DMG_NAME]["sha256"]:
        fail("macOS DMG changed across native execution")
    if bundle_identity(extracted_app) != extracted_bundle:
        fail("safely extracted updater app changed across native execution")

    provenance = {
        "assets": {
            "appArchive": assets[APP_ARCHIVE_NAME],
            "appArchiveSignature": assets[APP_SIGNATURE_NAME],
            "dmg": assets[DMG_NAME],
        },
        "bindingSha256": sha256_bytes(binding_raw),
        "bundle": inspection["bundle"],
        "codeSignature": {
            "after": signature_after,
            "before": signature_before,
            "semanticIdentityUnchanged": True,
        },
        "completedAt": utc_now(),
        "controllerAttemptSha256": sha256_bytes(controller_attempt_raw),
        "dmgExecution": {
            "attach": mount_evidence,
            "bundleAfter": mounted_bundle_after,
            "bundleBefore": mounted_bundle_before,
            "detach": detach_evidence,
            "treeAfter": tree_after,
            "treeBefore": tree_before,
        },
        "extractedArchiveBundleAfter": extracted_bundle,
        "inspectionSha256": sha256_bytes(inspection_raw),
        "nativeExecution": process_evidence,
        "nativeInputSha256": sha256_bytes(input_raw),
        "nativeReceiptSha256": sha256_bytes(native_receipt_raw),
        "nativeDispatchAttemptSha256": sha256_bytes(native_attempt_raw),
        "release": inspection["release"],
        "repository": REPOSITORY,
        "schema": PROVENANCE_SCHEMA,
        "source": source,
        "sourceCommit": binding["commit"],
        "truthScope": {
            "appleDeveloperIdSigned": False,
            "exactMountedDmgExecutableRan": True,
            "gatekeeperAssessed": False,
            "nativeCoreOnly": True,
            "notarizationAssessed": False,
            "shippedDebugOrWebdriverSurfaceAdded": False,
            "uiToIpcCoveredByThisReceipt": False,
            "updaterArchiveMinisignVerified": True,
        },
        "updaterSignatureReceiptSha256": sha256_bytes(signature_raw),
    }
    create_file(provenance_output, canonical_json(provenance), "macOS package provenance", 0o400)
    return provenance


def validate_tree_summary(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    require_exact_keys(value, {"entryCount", "sha256", "totalRegularBytes"}, label)
    positive_int(value["entryCount"], f"{label} entry count")
    if type(value["totalRegularBytes"]) is not int or value["totalRegularBytes"] <= 0:
        fail(f"{label} total regular bytes is invalid")
    require_hash(value["sha256"], f"{label} digest")
    return value


def validate_mount_evidence(value: object, *, expected_basename: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail("DMG attach evidence must be an object")
    require_exact_keys(
        value,
        {
            "attachPlistSha256",
            "device",
            "diskutilInfoPlistSha256",
            "filesystem",
            "hdiutilInfoPlistSha256",
            "imagePathMatched",
            "imagePathSha256",
            "mountFlags",
            "mountPointBasename",
            "nobrowse",
            "noowners",
            "readOnlyMedia",
            "readOnlyVolume",
            "statvfsReadOnly",
            "tools",
            "verifyStderrSha256",
            "verifyStdoutSha256",
            "writeable",
        },
        "DMG attach evidence",
    )
    for field in (
        "attachPlistSha256",
        "diskutilInfoPlistSha256",
        "hdiutilInfoPlistSha256",
        "imagePathSha256",
        "verifyStderrSha256",
        "verifyStdoutSha256",
    ):
        require_hash(value[field], f"DMG attach {field}")
    if (
        not isinstance(value["device"], str)
        or DEVICE_RE.fullmatch(value["device"]) is None
        or not isinstance(value["filesystem"], str)
        or not value["filesystem"]
        or value["mountPointBasename"] != expected_basename
        or value["imagePathMatched"] is not True
        or value["nobrowse"] is not True
        or value["noowners"] is not True
        or value["readOnlyMedia"] is not True
        or value["readOnlyVolume"] is not True
        or value["statvfsReadOnly"] is not True
        or value["writeable"] is not False
    ):
        fail("DMG attach source/device/read-only truth differs")
    flags = value["mountFlags"]
    require_exact_keys(flags, {"lineSha256", "options"}, "DMG mount flags")
    require_hash(flags["lineSha256"], "DMG mount-line digest")
    if (
        not isinstance(flags["options"], list)
        or not {"read-only", "noowners", "nobrowse"}.issubset(flags["options"])
        or "read-write" in flags["options"]
    ):
        fail("DMG mount flags are not the exact fail-closed subset")
    tools = value["tools"]
    if not isinstance(tools, dict) or set(tools) != {"codesign", "diskutil", "hdiutil", "mount"}:
        fail("DMG attach tool inventory differs")
    expected_paths = {
        "codesign": "/usr/bin/codesign",
        "diskutil": "/usr/sbin/diskutil",
        "hdiutil": "/usr/bin/hdiutil",
        "mount": "/sbin/mount",
    }
    for name, identity in tools.items():
        require_exact_keys(identity, {"path", "resolvedPath", "sha256", "size"}, f"DMG tool {name}")
        if identity["path"] != expected_paths[name] or identity["resolvedPath"] != expected_paths[name]:
            fail(f"DMG tool {name} path differs")
        require_hash(identity["sha256"], f"DMG tool {name} digest")
        positive_int(identity["size"], f"DMG tool {name} size")
    return value


def validate_detach_evidence(value: object) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail("DMG detach evidence must be an object")
    require_exact_keys(
        value,
        {
            "detachStderrSha256",
            "detachStdoutSha256",
            "detached",
            "mountPointEmpty",
            "postDetachInfoPlistSha256",
        },
        "DMG detach evidence",
    )
    if value["detached"] is not True or value["mountPointEmpty"] is not True:
        fail("DMG detach did not complete cleanly")
    for field in ("detachStderrSha256", "detachStdoutSha256", "postDetachInfoPlistSha256"):
        require_hash(value[field], f"DMG detach {field}")
    return value


def validate_controller_attempt(
    path: Path,
    native_input: Mapping[str, Any],
    input_raw: bytes,
    inspection_raw: bytes,
    bundle: Mapping[str, Any],
) -> tuple[dict[str, Any], bytes]:
    value, raw = load_json(path, "macOS native controller attempt")
    require_exact_keys(
        value,
        {
            "armedAt",
            "challenge",
            "executableSha256",
            "inputSha256",
            "inspectionSha256",
            "outerTimeoutSeconds",
            "retryPermitted",
            "schema",
            "sourceCommit",
            "state",
        },
        "macOS native controller attempt",
    )
    if (
        value["schema"] != "arc.macos-native-controller-attempt.v1"
        or value["state"] != "armed-no-retry"
        or value["retryPermitted"] is not False
        or value["outerTimeoutSeconds"] != NATIVE_OUTER_TIMEOUT_SECONDS
        or value["challenge"] != native_input["challenge"]
        or value["sourceCommit"] != native_input["sourceCommit"]
        or value["executableSha256"] != bundle["executableSha256"]
        or value["inputSha256"] != sha256_bytes(input_raw)
        or value["inspectionSha256"] != sha256_bytes(inspection_raw)
        or not isinstance(value["armedAt"], str)
    ):
        fail("macOS native controller no-retry attempt binding differs")
    return value, raw


def verify_provenance(args: argparse.Namespace) -> dict[str, Any]:
    if args.extraction_root.name != "macos-app-extraction":
        fail("verified extraction root basename differs from macos-app-extraction")
    (
        binding,
        binding_raw,
        assets,
        source,
        updater_key,
        _signature,
        signature_raw,
    ) = common_inputs(args)
    inspection, inspection_raw = validate_inspection(
        args.inspection,
        binding,
        binding_raw,
        assets,
        source,
        signature_raw,
        updater_key,
    )
    extracted_bundle = bundle_identity(args.extraction_root / APP_BUNDLE_NAME)
    if extracted_bundle != inspection["bundle"]:
        fail("extracted updater app differs during final provenance verification")
    native_input, input_raw = validate_native_input(
        args.native_input, binding, assets, inspection["bundle"]
    )
    native_home = require_absolute(args.native_home, "native isolated HOME")
    private_directory(native_home, "native isolated HOME")
    private_directory(native_home / "tmp", "native isolated TMPDIR")
    expected_environment_sha256 = environment_sha256(native_environment(native_home))
    native_output = require_absolute(args.native_output, "packaged native receipt")
    if native_output.name != "PACKAGED-NATIVE-ACCEPTANCE.json":
        fail("packaged native receipt basename differs")
    attempt_path = native_output.with_name("PACKAGED-NATIVE-DISPATCH-ATTEMPT.json")
    controller_attempt_path = native_output.with_name("MACOS-NATIVE-CONTROLLER-ATTEMPT.json")
    receipt, receipt_raw, _attempt, attempt_raw = validate_native_outputs(
        native_input,
        input_raw,
        native_output,
        attempt_path,
        inspection["bundle"],
        expected_environment_sha256,
    )
    _controller_attempt, controller_attempt_raw = validate_controller_attempt(
        controller_attempt_path, native_input, input_raw, inspection_raw, inspection["bundle"]
    )
    provenance, provenance_raw = load_json(args.receipt, "macOS package provenance receipt")
    require_exact_keys(
        provenance,
        {
            "assets",
            "bindingSha256",
            "bundle",
            "codeSignature",
            "completedAt",
            "controllerAttemptSha256",
            "dmgExecution",
            "extractedArchiveBundleAfter",
            "inspectionSha256",
            "nativeDispatchAttemptSha256",
            "nativeExecution",
            "nativeInputSha256",
            "nativeReceiptSha256",
            "release",
            "repository",
            "schema",
            "source",
            "sourceCommit",
            "truthScope",
            "updaterSignatureReceiptSha256",
        },
        "macOS package provenance receipt",
    )
    if (
        provenance["schema"] != PROVENANCE_SCHEMA
        or provenance["repository"] != REPOSITORY
        or provenance["sourceCommit"] != binding["commit"]
        or provenance["source"] != source
        or provenance["bindingSha256"] != sha256_bytes(binding_raw)
        or provenance["assets"] != inspection["assets"]
        or provenance["release"] != inspection["release"]
        or provenance["bundle"] != inspection["bundle"]
        or provenance["extractedArchiveBundleAfter"] != extracted_bundle
        or provenance["inspectionSha256"] != sha256_bytes(inspection_raw)
        or provenance["updaterSignatureReceiptSha256"] != sha256_bytes(signature_raw)
        or provenance["nativeInputSha256"] != sha256_bytes(input_raw)
        or provenance["nativeReceiptSha256"] != sha256_bytes(receipt_raw)
        or provenance["nativeDispatchAttemptSha256"] != sha256_bytes(attempt_raw)
        or provenance["controllerAttemptSha256"] != sha256_bytes(controller_attempt_raw)
    ):
        fail("macOS package provenance receipt cross-binding differs")
    code = provenance["codeSignature"]
    require_exact_keys(code, {"after", "before", "semanticIdentityUnchanged"}, "provenance code signature")
    before_code = validate_code_signature(code["before"], "provenance pre-run code signature")
    after_code = validate_code_signature(code["after"], "provenance post-run code signature")
    if (
        code["semanticIdentityUnchanged"] is not True
        or semantic_code_signature(before_code) != semantic_code_signature(after_code)
        or semantic_code_signature(before_code) != semantic_code_signature(inspection["codeSignature"])
    ):
        fail("macOS package provenance code-signature identity changed")
    execution = provenance["dmgExecution"]
    require_exact_keys(
        execution,
        {"attach", "bundleAfter", "bundleBefore", "detach", "treeAfter", "treeBefore"},
        "provenance DMG execution",
    )
    validate_mount_evidence(execution["attach"], expected_basename="native-dmg-mount")
    validate_detach_evidence(execution["detach"])
    if execution["bundleBefore"] != inspection["bundle"] or execution["bundleAfter"] != inspection["bundle"]:
        fail("provenance mounted bundle identity changed")
    tree_before = validate_tree_summary(execution["treeBefore"], "provenance pre-run tree")
    tree_after = validate_tree_summary(execution["treeAfter"], "provenance post-run tree")
    if tree_before != tree_after or tree_before["sha256"] != inspection["bundle"]["appBundleTreeSha256"]:
        fail("provenance mounted app tree changed or differs from inspection")
    native_execution = provenance["nativeExecution"]
    require_exact_keys(
        native_execution,
        {
            "completedAt",
            "elapsedMs",
            "innerMaxSeconds",
            "outerTimeoutSeconds",
            "returnCode",
            "startedAt",
            "stderrSha256",
            "stderrSize",
            "stdoutSha256",
            "stdoutSize",
            "timedOut",
        },
        "provenance native execution",
    )
    if (
        native_execution["innerMaxSeconds"] != NATIVE_INNER_MAX_SECONDS
        or native_execution["outerTimeoutSeconds"] != NATIVE_OUTER_TIMEOUT_SECONDS
        or native_execution["returnCode"] != 0
        or native_execution["timedOut"] is not False
        or type(native_execution["elapsedMs"]) is not int
        or not 0 <= native_execution["elapsedMs"] <= NATIVE_OUTER_TIMEOUT_SECONDS * 1_000
    ):
        fail("provenance native execution result/budget differs")
    for field in ("stdoutSha256", "stderrSha256"):
        require_hash(native_execution[field], f"provenance native {field}")
    for field in ("stdoutSize", "stderrSize"):
        if type(native_execution[field]) is not int or not 0 <= native_execution[field] <= MAX_COMMAND_OUTPUT_BYTES:
            fail(f"provenance native {field} is outside its bound")
    truth = provenance["truthScope"]
    if truth != {
        "appleDeveloperIdSigned": False,
        "exactMountedDmgExecutableRan": True,
        "gatekeeperAssessed": False,
        "nativeCoreOnly": True,
        "notarizationAssessed": False,
        "shippedDebugOrWebdriverSurfaceAdded": False,
        "uiToIpcCoveredByThisReceipt": False,
        "updaterArchiveMinisignVerified": True,
    }:
        fail("macOS package provenance truth scope is widened or incomplete")
    if receipt.get("inputSha256") != provenance["nativeInputSha256"]:
        fail("native receipt and provenance input binding differ")
    verification_output = require_absolute(args.output, "macOS provenance verification output")
    if (
        verification_output.name != "MACOS-PACKAGE-PROVENANCE-VERIFICATION.json"
        or verification_output.exists()
        or verification_output.is_symlink()
    ):
        fail(
            "verification output must be one absent "
            "MACOS-PACKAGE-PROVENANCE-VERIFICATION.json"
        )
    private_directory(verification_output.parent, "macOS provenance verification output parent")
    verification = {
        "assets": inspection["assets"],
        "bindingSha256": sha256_bytes(binding_raw),
        "bundle": extracted_bundle,
        "completedAt": utc_now(),
        "controllerAttemptSha256": sha256_bytes(controller_attempt_raw),
        "inspectionSha256": sha256_bytes(inspection_raw),
        "nativeDispatchAttemptSha256": sha256_bytes(attempt_raw),
        "nativeInputSha256": sha256_bytes(input_raw),
        "nativeReceiptSha256": sha256_bytes(receipt_raw),
        "provenanceSha256": sha256_bytes(provenance_raw),
        "release": inspection["release"],
        "repository": REPOSITORY,
        "schema": VERIFICATION_SCHEMA,
        "source": source,
        "sourceCommit": binding["commit"],
        "updaterSignatureReceiptSha256": sha256_bytes(signature_raw),
        "verified": True,
    }
    verification_raw = canonical_json(verification)
    create_file(
        verification_output,
        verification_raw,
        "macOS package provenance verification",
        0o400,
    )
    return {
        "provenance": provenance,
        "provenanceSha256": sha256_bytes(provenance_raw),
        "verification": verification,
        "verificationSha256": sha256_bytes(verification_raw),
    }


def add_release_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--repository-root", required=True, type=Path)
    parser.add_argument("--release-binding", required=True, type=Path)
    parser.add_argument("--asset-directory", required=True, type=Path)


def add_verified_package_arguments(parser: argparse.ArgumentParser) -> None:
    add_release_arguments(parser)
    parser.add_argument("--signature-receipt", required=True, type=Path)


def add_mac_controller_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--controller-home", required=True, type=Path)
    parser.add_argument("--controller-tmpdir", required=True, type=Path)
    parser.add_argument("--mount-point", required=True, type=Path)


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(
        description="Prove and run the exact published macOS ARC app without a shipped debug seam"
    )
    commands = root.add_subparsers(dest="command", required=True)

    lima = commands.add_parser(
        "lima-verify", help="verify the updater archive in one fresh digest-pinned Lima VM"
    )
    add_release_arguments(lima)
    lima.add_argument("--limactl", required=True, type=Path)
    lima.add_argument("--limactl-sha256", required=True)
    lima.add_argument("--lima-home", required=True, type=Path)
    lima.add_argument("--host-home", required=True, type=Path)
    lima.add_argument("--host-tmpdir", required=True, type=Path)
    lima.add_argument("--output", required=True, type=Path)
    lima.add_argument("--vm-name")

    guest = commands.add_parser(
        "guest-verify", help="internal verifier entered only inside the fresh pinned Lima guest"
    )
    guest.add_argument("--runtime-root", required=True, type=Path)

    inspect = commands.add_parser(
        "inspect", help="safe-extract and inspect identical updater/DMG app packages"
    )
    add_verified_package_arguments(inspect)
    add_mac_controller_arguments(inspect)
    inspect.add_argument("--extraction-root", required=True, type=Path)
    inspect.add_argument("--output", required=True, type=Path)

    build_input = commands.add_parser(
        "build-input", help="seal the native input from inspected package and chain/release truth"
    )
    add_verified_package_arguments(build_input)
    build_input.add_argument("--inspection", required=True, type=Path)
    build_input.add_argument("--frontend-commit", required=True)
    build_input.add_argument("--frontend-config-sha256", required=True)
    build_input.add_argument("--rollout-manifest-sha256", required=True)
    build_input.add_argument("--expected-model-id", required=True)
    build_input.add_argument("--recovery-epoch", required=True, type=int)
    build_input.add_argument("--validator-set-id", required=True, type=int)
    build_input.add_argument("--validator-set-commitment", required=True)
    build_input.add_argument("--transaction-domain", required=True)
    build_input.add_argument("--minimum-peers", type=int, default=5)
    build_input.add_argument("--maximum-block-age-seconds", type=int, default=300)
    build_input.add_argument("--validity-seconds", type=int, default=7_200)
    build_input.add_argument("--output", required=True, type=Path)

    run = commands.add_parser(
        "run", help="remount and run the exact DMG executable in bounded native acceptance mode"
    )
    add_verified_package_arguments(run)
    add_mac_controller_arguments(run)
    run.add_argument("--inspection", required=True, type=Path)
    run.add_argument("--extraction-root", required=True, type=Path)
    run.add_argument("--native-home", required=True, type=Path)
    run.add_argument("--native-input", required=True, type=Path)
    run.add_argument("--native-output", required=True, type=Path)
    run.add_argument("--output", required=True, type=Path)

    verify = commands.add_parser(
        "verify", help="reverify sealed evidence read-only, then seal one verification receipt"
    )
    add_verified_package_arguments(verify)
    verify.add_argument("--inspection", required=True, type=Path)
    verify.add_argument("--extraction-root", required=True, type=Path)
    verify.add_argument("--native-home", required=True, type=Path)
    verify.add_argument("--native-input", required=True, type=Path)
    verify.add_argument("--native-output", required=True, type=Path)
    verify.add_argument("--receipt", required=True, type=Path)
    verify.add_argument("--output", required=True, type=Path)
    return root


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "lima-verify":
            result = lima_verify(args)
            print(
                f"VERIFIED macOS updater minisign in pinned Lima receipt={args.output} "
                f"sha256={sha256_bytes(canonical_json(result))}"
            )
        elif args.command == "guest-verify":
            guest_verify_signature(args)
        elif args.command == "inspect":
            result = inspect_package(args)
            print(
                f"VERIFIED identical updater/DMG app inspection={args.output} "
                f"tree={result['bundle']['appBundleTreeSha256']}"
            )
        elif args.command == "build-input":
            result = build_native_input(args)
            print(
                f"SEALED packaged native input={args.output} "
                f"challenge={result['challenge']} expires={result['expiresAtUnix']}"
            )
        elif args.command == "run":
            result = run_package(args)
            print(
                f"VERIFIED mounted packaged native acceptance provenance={args.output} "
                f"receipt={result['nativeReceiptSha256']}"
            )
        elif args.command == "verify":
            result = verify_provenance(args)
            print(
                f"VERIFIED sealed macOS package provenance receipt={args.receipt} "
                f"sha256={result['provenanceSha256']} verification={args.output} "
                f"verification_sha256={result['verificationSha256']}"
            )
        else:  # pragma: no cover - argparse enforces a command
            fail("unsupported command")
    except ProvenanceError as error:
        print(f"macOS package provenance failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
