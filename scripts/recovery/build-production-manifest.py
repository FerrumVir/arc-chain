#!/usr/bin/env python3
"""Build and finalize the only supported ARC production rollout manifest.

The builder deliberately accepts source evidence, never caller-authored derived
chain/topology fields.  ``prearchive`` verifies the exact protected-main
pre-tag artifacts, reproduces the checkpoint from the preserved snapshot/WAL,
and seals a private create-only rollout.  ``finalize`` authenticates downloaded
archive completion evidence and changes only the four archive finalization
roots.
"""

from __future__ import annotations

import argparse
import base64
import binascii
import copy
import contextlib
import datetime as dt
import hashlib
import importlib.util
import json
import os
import re
import secrets
import stat
import struct
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path
from typing import Any, Mapping, NoReturn, Sequence


SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent
RELEASE_SCRIPT_DIR = REPO_ROOT / "scripts" / "release"
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import recovery_rollout as rollout
from recovery_freeze import FreezeValidationError, validate_pinned_freeze_plan


def _load_height_module() -> Any:
    path = SCRIPT_DIR / "legacy-public-height.py"
    spec = importlib.util.spec_from_file_location("arc_legacy_public_height", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load legacy public-height validator")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


legacy_height = _load_height_module()


def _load_late_fork_module() -> Any:
    path = SCRIPT_DIR / "legacy-late-fork-interlock.py"
    spec = importlib.util.spec_from_file_location("arc_legacy_late_fork_interlock", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load legacy late-fork interlock validator")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


late_fork = _load_late_fork_module()


def _load_protected_pretag_module() -> Any:
    path = RELEASE_SCRIPT_DIR / "protected_pretag_artifact.py"
    spec = importlib.util.spec_from_file_location("arc_protected_pretag", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load protected pre-tag artifact verifier")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


protected_pretag = _load_protected_pretag_module()


REPOSITORY = "FerrumVir/arc-chain"
VERSION = "0.8.0"
PRETAG_PLATFORM = "linux-x86_64"
PRETAG_TARGET = "x86_64-unknown-linux-gnu"
PRETAG_GROUPS = (
    ("headless", "linux-x86_64"),
    ("headless", "linux-arm64"),
    ("headless", "macos-arm64"),
    ("headless", "macos-x86_64"),
    ("headless", "windows-x86_64"),
    ("desktop", "linux-x86_64"),
    ("desktop", "macos-arm64"),
    ("desktop", "macos-x86_64"),
    ("desktop", "windows-x86_64"),
)
PRETAG_INPUT_SET_SCHEMA = "arc.recovery.pretag-artifact-input-set.v1"
PRETAG_PROVENANCE_SET_SCHEMA = "arc.protected-pretag-artifact-set.v1"
PRETAG_WINDOW_SET_SCHEMA = "arc.protected-pretag-artifact-window-set.v1"
APPROVED_ACME_EMAIL = "tj@arc.ai"
MAX_OFFLINE_STOP_VERIFICATION_AGE_SECONDS = 300
MAX_OFFLINE_STOP_VERIFICATION_DURATION_MS = 120_000
ZERO_HASH = "0" * 64
MAX_JSON_BYTES = 16 * 1024 * 1024
MAX_METADATA_BYTES = 64 * 1024
HASH_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
SAFE_OBJECT_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")

FLEET = (
    ("nyc", "149.28.32.76"),
    ("lax", "140.82.16.112"),
    ("ams", "136.244.109.1"),
    ("lhr", "104.238.171.11"),
    ("nrt", "202.182.107.41"),
    ("sgp", "149.28.153.31"),
)
SYSTEM_SSH = Path("/usr/bin/ssh")
SYSTEM_SCP = Path("/usr/bin/scp")
SYSTEM_GIT = Path("/usr/bin/git")
SYSTEM_PYTHON_ENTRYPOINT = Path("/usr/bin/python3")
SYSTEM_BASH_CANDIDATES = (Path("/usr/bin/bash"), Path("/bin/bash"))
SHARDS = {
    "nyc": [[0, 6], [22, 27], [27, 32]],
    "lax": [[0, 6], [6, 12], [27, 32]],
    "ams": [[0, 6], [6, 12], [12, 17]],
    "lhr": [[6, 12], [12, 17], [17, 22]],
    "nrt": [[12, 17], [17, 22], [22, 27]],
    "sgp": [[17, 22], [22, 27], [27, 32]],
}

PROTECTED_MAIN_EXECUTION_FILES = (
    SCRIPT_DIR / "build-production-manifest.py",
    SCRIPT_DIR / "recovery_freeze.py",
    SCRIPT_DIR / "legacy-public-height.py",
    SCRIPT_DIR / "recovery_rollout.py",
    SCRIPT_DIR / "recovery-manifest.schema.json",
    SCRIPT_DIR / "archive-fleet-to-drive.sh",
    SCRIPT_DIR / "archive-node.sh",
    SCRIPT_DIR / "community-reward-probe.py",
    SCRIPT_DIR / "legacy-late-fork-interlock.py",
    RELEASE_SCRIPT_DIR / "protected_pretag_artifact.py",
)


class BuilderError(RuntimeError):
    """An input or archive root failed closed before publication."""


def fail(message: str) -> NoReturn:
    raise BuilderError(message)


def canonical_bytes(value: Any) -> bytes:
    try:
        return (
            json.dumps(
                value,
                sort_keys=True,
                separators=(",", ":"),
                ensure_ascii=True,
                allow_nan=False,
            )
            + "\n"
        ).encode("utf-8")
    except (TypeError, ValueError) as error:
        fail(f"value cannot be represented as canonical JSON: {error}")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def require_hash(value: Any, field: str) -> str:
    if not isinstance(value, str) or HASH_RE.fullmatch(value) is None:
        fail(f"{field} must be exactly 64 lowercase hexadecimal characters")
    return value


def require_commit(value: Any, field: str) -> str:
    if not isinstance(value, str) or COMMIT_RE.fullmatch(value) is None:
        fail(f"{field} must be one full lowercase Git SHA")
    return value


def require_uint(value: Any, field: str, *, positive: bool = False) -> int:
    minimum = 1 if positive else 0
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        fail(f"{field} must be an integer >= {minimum}")
    return value


def validate_protected_main_commit(source_main_sha: str) -> None:
    """Prove the executing recovery implementation is the selected Git commit."""

    environment = {
        "HOME": "/var/empty",
        "PATH": "/usr/bin:/bin",
        "LANG": "C",
        "LC_ALL": "C",
        "TZ": "UTC",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_OPTIONAL_LOCKS": "0",
        "GIT_PAGER": "cat",
    }
    validate_root_system_tool(
        SYSTEM_GIT,
        "production Git client",
        allow_multiple_hardlinks=sys.platform == "darwin",
    )

    def git(*argv: str) -> bytes:
        try:
            result = subprocess.run(
                [os.fspath(SYSTEM_GIT), "-C", os.fspath(REPO_ROOT), *argv],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd="/",
                env=environment,
                timeout=60,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            fail(f"cannot verify protected-main Git source: {error}")
        if result.returncode != 0:
            diagnostic = result.stderr.decode("utf-8", errors="replace").strip()[:1000]
            fail(f"protected-main Git verification failed: {diagnostic}")
        return result.stdout

    head = git("rev-parse", "--verify", "HEAD^{commit}").decode("ascii").strip()
    if head != source_main_sha:
        fail("executing recovery worktree HEAD differs from the selected protected-main commit")
    for path in PROTECTED_MAIN_EXECUTION_FILES:
        try:
            relative = path.relative_to(REPO_ROOT).as_posix()
        except ValueError:
            fail(f"protected-main execution file is outside the repository: {path}")
        committed = git("show", f"{source_main_sha}:{relative}")
        current_sha, current_size = hash_secure(
            path,
            f"protected-main execution file {relative}",
            executable=path.suffix in {".py", ".sh"},
        )
        if len(committed) != current_size or sha256_bytes(committed) != current_sha:
            fail(f"executing recovery file differs from protected main: {relative}")


def require_exact_object(value: Any, fields: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    if set(value) != fields:
        fail(
            f"{label} fields differ (missing={sorted(fields - set(value))}, "
            f"unknown={sorted(set(value) - fields)})"
        )
    return value


def _reject_duplicate_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            fail(f"duplicate JSON key is forbidden: {key!r}")
        result[key] = value
    return result


def _reject_nonfinite(token: str) -> NoReturn:
    fail(f"non-finite JSON number is forbidden: {token}")


def decode_json(payload: bytes, label: str) -> Any:
    try:
        return json.loads(
            payload.decode("utf-8"),
            object_pairs_hook=_reject_duplicate_pairs,
            parse_constant=_reject_nonfinite,
        )
    except BuilderError:
        raise
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"{label} is not valid UTF-8 JSON: {error}")


def _lexical_absolute(path: Path, label: str) -> None:
    raw = os.fspath(path)
    if not path.is_absolute() or path.name in {"", ".", ".."}:
        fail(f"{label} must be an absolute file path")
    if raw != os.path.normpath(raw) or "\x00" in raw or "\n" in raw or "\r" in raw:
        fail(f"{label} must be lexically normalized without unsafe characters")


def open_parent_directory(path: Path, label: str) -> tuple[int, str]:
    """Open every parent component without following symlinks."""

    _lexical_absolute(path, label)
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open("/", flags)
    try:
        for component in path.parent.parts[1:]:
            next_descriptor = os.open(component, flags, dir_fd=descriptor)
            details = os.fstat(next_descriptor)
            if not stat.S_ISDIR(details.st_mode):
                fail(f"{label} has a non-directory parent component")
            os.close(descriptor)
            descriptor = next_descriptor
        return descriptor, path.name
    except Exception:
        os.close(descriptor)
        raise


def read_secure(
    path: Path,
    *,
    label: str,
    maximum_bytes: int,
    exact_mode: int | None = None,
    require_read_only: bool = False,
) -> tuple[bytes, os.stat_result]:
    parent_fd, name = open_parent_directory(path, label)
    descriptor = -1
    try:
        descriptor = os.open(
            name,
            os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0),
            dir_fd=parent_fd,
        )
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode):
            fail(f"{label} must be a regular non-symlink file")
        mode = stat.S_IMODE(before.st_mode)
        if exact_mode is not None and mode != exact_mode:
            fail(f"{label} mode must be exactly {exact_mode:04o}, found {mode:04o}")
        if require_read_only and before.st_mode & 0o222:
            fail(f"{label} must have no write bits")
        if before.st_size <= 0 or before.st_size > maximum_bytes:
            fail(f"{label} size must be in 1..={maximum_bytes} bytes")
        chunks: list[bytes] = []
        remaining = maximum_bytes + 1
        while remaining:
            chunk = os.read(descriptor, min(1024 * 1024, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        payload = b"".join(chunks)
        after = os.fstat(descriptor)
        identity = lambda details: (
            details.st_dev,
            details.st_ino,
            details.st_size,
            details.st_mtime_ns,
            details.st_ctime_ns,
        )
        if len(payload) != before.st_size or identity(before) != identity(after):
            fail(f"{label} changed while it was read")
        return payload, before
    except OSError as error:
        fail(f"cannot securely read {label}: {error}")
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        os.close(parent_fd)


def hash_secure(path: Path, label: str, *, executable: bool = False) -> tuple[str, int]:
    parent_fd, name = open_parent_directory(path, label)
    descriptor = -1
    try:
        descriptor = os.open(
            name,
            os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0),
            dir_fd=parent_fd,
        )
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_size <= 0:
            fail(f"{label} must be a non-empty regular non-symlink file")
        if executable and before.st_mode & 0o111 == 0:
            fail(f"{label} must have an executable mode bit")
        digest = hashlib.sha256()
        size = 0
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
            size += len(chunk)
        after = os.fstat(descriptor)
        identity = lambda details: (
            details.st_dev,
            details.st_ino,
            details.st_size,
            details.st_mtime_ns,
            details.st_ctime_ns,
        )
        if size != before.st_size or identity(before) != identity(after):
            fail(f"{label} changed while it was hashed")
        return digest.hexdigest(), size
    except OSError as error:
        fail(f"cannot securely hash {label}: {error}")
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        os.close(parent_fd)


def _stage_copy(
    source: Path,
    destination_parent_fd: int,
    destination_name: str,
    *,
    label: str,
    maximum_bytes: int,
    mode: int,
    executable: bool = False,
) -> dict[str, Any]:
    """Create one immutable private copy from a stable no-follow source FD."""

    source_parent_fd, source_name = open_parent_directory(source, label)
    source_fd = -1
    destination_fd = -1
    created = False
    try:
        source_fd = os.open(
            source_name,
            os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0),
            dir_fd=source_parent_fd,
        )
        before = os.fstat(source_fd)
        if not stat.S_ISREG(before.st_mode):
            fail(f"{label} must be a regular non-symlink file")
        if before.st_size <= 0 or before.st_size > maximum_bytes:
            fail(f"{label} size must be in 1..={maximum_bytes} bytes")
        if executable and before.st_mode & 0o111 == 0:
            fail(f"{label} must have an executable mode bit")
        destination_fd = os.open(
            destination_name,
            os.O_WRONLY
            | os.O_CREAT
            | os.O_EXCL
            | getattr(os, "O_NOFOLLOW", 0)
            | getattr(os, "O_CLOEXEC", 0),
            mode,
            dir_fd=destination_parent_fd,
        )
        created = True
        digest = hashlib.sha256()
        copied = 0
        while True:
            chunk = os.read(source_fd, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
            offset = 0
            while offset < len(chunk):
                written = os.write(destination_fd, chunk[offset:])
                if written <= 0:
                    fail(f"{label} staging write made no progress")
                offset += written
            copied += len(chunk)
        after = os.fstat(source_fd)
        identity = lambda details: (
            details.st_dev,
            details.st_ino,
            details.st_size,
            details.st_mtime_ns,
            details.st_ctime_ns,
        )
        if copied != before.st_size or identity(before) != identity(after):
            fail(f"{label} changed while its private stage copy was created")
        os.fchmod(destination_fd, mode)
        os.fsync(destination_fd)
        staged = os.fstat(destination_fd)
        if (
            not stat.S_ISREG(staged.st_mode)
            or staged.st_uid != os.geteuid()
            or staged.st_nlink != 1
            or stat.S_IMODE(staged.st_mode) != mode
            or staged.st_size != copied
        ):
            fail(f"{label} private stage copy failed its file identity contract")
        return {
            "sha256": digest.hexdigest(),
            "size_bytes": copied,
            "mode": f"{mode:04o}",
        }
    except FileExistsError:
        fail(f"{label} private stage destination already exists")
    except OSError as error:
        fail(f"cannot create private stage copy for {label}: {error}")
    finally:
        if destination_fd >= 0:
            os.close(destination_fd)
        if source_fd >= 0:
            os.close(source_fd)
        os.close(source_parent_fd)
        if created:
            os.fsync(destination_parent_fd)


def _stage_bytes(
    payload: bytes,
    destination_parent_fd: int,
    destination_name: str,
    *,
    label: str,
    mode: int = 0o400,
) -> dict[str, Any]:
    if not payload or SAFE_OBJECT_NAME_RE.fullmatch(destination_name) is None:
        fail(f"{label} has empty bytes or an unsafe private stage name")
    descriptor = -1
    try:
        descriptor = os.open(
            destination_name,
            os.O_WRONLY
            | os.O_CREAT
            | os.O_EXCL
            | getattr(os, "O_NOFOLLOW", 0)
            | getattr(os, "O_CLOEXEC", 0),
            mode,
            dir_fd=destination_parent_fd,
        )
        offset = 0
        while offset < len(payload):
            written = os.write(descriptor, payload[offset:])
            if written <= 0:
                fail(f"{label} private stage write made no progress")
            offset += written
        os.fchmod(descriptor, mode)
        os.fsync(descriptor)
        details = os.fstat(descriptor)
        if (
            not stat.S_ISREG(details.st_mode)
            or details.st_uid != os.geteuid()
            or details.st_nlink != 1
            or stat.S_IMODE(details.st_mode) != mode
            or details.st_size != len(payload)
        ):
            fail(f"{label} private stage bytes failed their file identity contract")
        os.fsync(destination_parent_fd)
        return {
            "sha256": sha256_bytes(payload),
            "size_bytes": len(payload),
            "mode": f"{mode:04o}",
        }
    except FileExistsError:
        fail(f"{label} private stage destination already exists")
    except OSError as error:
        fail(f"cannot create private stage bytes for {label}: {error}")
    finally:
        if descriptor >= 0:
            os.close(descriptor)


def stage_prearchive_inputs(args: argparse.Namespace) -> tuple[argparse.Namespace, str]:
    """Freeze every semantic prearchive input into one create-only private tree.

    Subsequent validators, checkpoint reproduction, binary execution, archive
    capture, and rollout consume only these staged paths.  This removes the
    caller-path hash/open gap; the documented same-operator account remains the
    outer trust boundary.
    """

    stage_root = args.stage_root
    _lexical_absolute(stage_root, "production input stage root")
    if args.reward_probe != SCRIPT_DIR / "community-reward-probe.py":
        fail("community reward probe must be the exact protected-main recovery probe")
    for path, label in (
        (args.freeze_plan, "freeze plan"),
        (args.legacy_maintenance_evidence_bundle, "legacy maintenance evidence bundle"),
        (args.legacy_maintenance_boundary, "legacy maintenance boundary"),
        (args.legacy_late_fork_source_set, "legacy late-fork source set"),
        (args.offline_stop_evidence, "offline-stop evidence"),
    ):
        if SAFE_OBJECT_NAME_RE.fullmatch(path.name) is None:
            fail(f"{label} basename is unsafe for the private stage")
    parent_fd, root_name = open_parent_directory(stage_root, "production input stage root")
    root_fd = -1
    child_fds: dict[str, int] = {}
    try:
        parent_details = os.fstat(parent_fd)
        if (
            not stat.S_ISDIR(parent_details.st_mode)
            or parent_details.st_uid not in (0, os.geteuid())
            or parent_details.st_mode & 0o022
        ):
            fail(
                "production input stage parent must be root/operator-owned and "
                "not group/world writable"
            )
        try:
            os.mkdir(root_name, 0o700, dir_fd=parent_fd)
        except FileExistsError:
            fail("production input stage root already exists; refusing mutable reuse")
        os.fsync(parent_fd)
        root_fd = os.open(
            root_name,
            os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
            dir_fd=parent_fd,
        )
        root_details = os.fstat(root_fd)
        if (
            not stat.S_ISDIR(root_details.st_mode)
            or root_details.st_uid != os.geteuid()
            or stat.S_IMODE(root_details.st_mode) != 0o700
        ):
            fail("production input stage root failed its private directory contract")
        for directory in ("source", "private"):
            os.mkdir(directory, 0o700, dir_fd=root_fd)
            child_fds[directory] = os.open(
                directory,
                os.O_RDONLY
                | getattr(os, "O_DIRECTORY", 0)
                | getattr(os, "O_NOFOLLOW", 0),
                dir_fd=root_fd,
            )

        staged = copy.copy(args)
        rows: list[dict[str, Any]] = []
        specs = (
            (
                "freeze_plan",
                args.freeze_plan,
                args.freeze_plan.name,
                32 * 1024 * 1024,
                0o400,
                False,
                "root",
            ),
            (
                "freeze_plan_sidecar",
                args.freeze_plan.with_name(args.freeze_plan.name + ".sha256"),
                args.freeze_plan.name + ".sha256",
                512,
                0o400,
                False,
                "root",
            ),
            (
                "legacy_public_height_receipt",
                args.legacy_public_height_receipt,
                "legacy-public-height.json",
                16 * 1024 * 1024,
                0o400,
                False,
                "root",
            ),
            (
                "legacy_maintenance_evidence_bundle",
                args.legacy_maintenance_evidence_bundle,
                "legacy-maintenance-evidence-bundle.json",
                32 * 1024 * 1024,
                0o400,
                False,
                "root",
            ),
            (
                "legacy_maintenance_evidence_bundle_sidecar",
                args.legacy_maintenance_evidence_bundle.with_name(
                    args.legacy_maintenance_evidence_bundle.name + ".sha256"
                ),
                "legacy-maintenance-evidence-bundle.json.sha256",
                512,
                0o400,
                False,
                "root",
            ),
            (
                "legacy_maintenance_boundary",
                args.legacy_maintenance_boundary,
                "legacy-maintenance-boundary.json",
                16 * 1024 * 1024,
                0o400,
                False,
                "root",
            ),
            (
                "legacy_maintenance_boundary_sidecar",
                args.legacy_maintenance_boundary.with_name(
                    args.legacy_maintenance_boundary.name + ".sha256"
                ),
                "legacy-maintenance-boundary.json.sha256",
                512,
                0o400,
                False,
                "root",
            ),
            (
                "legacy_late_fork_source_set",
                args.legacy_late_fork_source_set,
                "legacy-late-fork-source-set.json",
                4 * 1024 * 1024,
                0o400,
                False,
                "root",
            ),
            (
                "legacy_late_fork_source_set_sidecar",
                args.legacy_late_fork_source_set.with_name(
                    args.legacy_late_fork_source_set.name + ".sha256"
                ),
                "legacy-late-fork-source-set.json.sha256",
                512,
                0o400,
                False,
                "root",
            ),
            (
                "legacy_late_fork_interlock_tool",
                SCRIPT_DIR / "legacy-late-fork-interlock.py",
                "legacy-late-fork-interlock.py",
                4 * 1024 * 1024,
                0o500,
                True,
                "root",
            ),
            (
                "offline_stop_evidence",
                args.offline_stop_evidence,
                args.offline_stop_evidence.name,
                16 * 1024 * 1024,
                0o400,
                False,
                "root",
            ),
            (
                "offline_stop_sidecar",
                args.offline_stop_evidence.with_name(args.offline_stop_evidence.name + ".sha256"),
                args.offline_stop_evidence.name + ".sha256",
                512,
                0o400,
                False,
                "root",
            ),
            ("ssh_known_hosts", args.ssh_known_hosts, "known_hosts", 16 * 1024, 0o400, False, "private"),
            ("ssh_identity", args.ssh_identity, "id_ed25519", 128 * 1024, 0o400, False, "private"),
            (
                "validator_vault_restore_receipt",
                args.validator_vault_restore_receipt,
                "VALIDATOR-VAULT-RESTORE-RECEIPT.json",
                1024 * 1024,
                0o400,
                False,
                "private",
            ),
            (
                "validator_key_install_receipt",
                args.validator_key_install_receipt,
                "VALIDATOR-KEY-INSTALL-RECEIPT.json",
                1024 * 1024,
                0o400,
                False,
                "private",
            ),
            ("binary", args.binary, "arc-node-linux-x86_64", 1024 * 1024 * 1024, 0o500, True, "root"),
            ("cli", args.cli, "arc-cli-linux-x86_64", 512 * 1024 * 1024, 0o500, True, "root"),
            ("build_metadata", args.build_metadata, "BUILD-METADATA.json", MAX_METADATA_BYTES, 0o400, False, "root"),
            ("genesis", args.genesis, "genesis.toml", 1024 * 1024, 0o400, False, "root"),
            (
                "validator_public_keys",
                args.validator_public_keys,
                "validator-public-keys.json",
                1024 * 1024,
                0o400,
                False,
                "root",
            ),
            (
                "legacy_validator_set",
                args.legacy_validator_set,
                "legacy-validator-set-40m.json",
                1024 * 1024,
                0o400,
                False,
                "root",
            ),
            ("checkpoint", args.checkpoint, "recovery.arcchkpt", 1024 * 1024 * 1024, 0o400, False, "root"),
            (
                "source_snapshot",
                args.source_snapshot,
                "state.snapshot.lz4",
                16 * 1024 * 1024 * 1024,
                0o400,
                False,
                "source",
            ),
            (
                "source_wal",
                args.source_wal,
                "state.wal",
                16 * 1024 * 1024 * 1024,
                0o400,
                False,
                "source",
            ),
            ("caddy", args.caddy, "caddy-linux-amd64", 512 * 1024 * 1024, 0o500, True, "root"),
            (
                "reward_probe",
                args.reward_probe,
                "community-reward-probe.py",
                4 * 1024 * 1024,
                0o500,
                True,
                "root",
            ),
        )
        for attribute, source, name, maximum, mode, executable, directory in specs:
            destination_parent_fd = root_fd if directory == "root" else child_fds[directory]
            details = _stage_copy(
                source,
                destination_parent_fd,
                name,
                label=f"production input {attribute}",
                maximum_bytes=maximum,
                mode=mode,
                executable=executable,
            )
            relative = name if directory == "root" else f"{directory}/{name}"
            if attribute in {
                "freeze_plan_sidecar",
                "legacy_maintenance_evidence_bundle_sidecar",
                "legacy_maintenance_boundary_sidecar",
                "legacy_late_fork_source_set_sidecar",
                "offline_stop_sidecar",
            }:
                pass
            else:
                setattr(staged, attribute, stage_root / relative)
            rows.append({"name": attribute, "path": relative, **details})

        staged_pretag_rows: list[dict[str, Any]] = []
        for row in args.verified_pretag_artifacts:
            kind = row["kind"]
            platform = row["platform"]
            destination_name = f"pretag-{kind}-{platform}.actions.zip"
            details = _stage_copy(
                row["raw_actions_zip"],
                child_fds["source"],
                destination_name,
                label=f"protected pre-tag {kind}/{platform} raw Actions ZIP",
                maximum_bytes=protected_pretag.MAX_ACTIONS_ZIP_BYTES,
                mode=0o400,
                executable=False,
            )
            if details["sha256"] != row["provenance"]["artifact"]["raw_actions_zip_sha256"]:
                fail(f"staged protected pre-tag {kind}/{platform} raw ZIP changed after live proof")
            if details["size_bytes"] != row["provenance"]["artifact"]["raw_actions_zip_size"]:
                fail(f"staged protected pre-tag {kind}/{platform} raw ZIP size changed after live proof")
            relative = f"source/{destination_name}"
            rows.append(
                {
                    "name": pretag_artifact_key(kind, platform),
                    "path": relative,
                    **details,
                }
            )
            staged_pretag_rows.append(
                {
                    "kind": kind,
                    "platform": platform,
                    "artifact_id": row["artifact_id"],
                    "raw_actions_zip": stage_root / relative,
                    "provenance": copy.deepcopy(row["provenance"]),
                }
            )

        staged_input_set = {
            "schema": PRETAG_INPUT_SET_SCHEMA,
            "repository": REPOSITORY,
            "commit": args.source_main_sha,
            "run_id": args.pretag_run_id,
            "run_attempt": args.pretag_run_attempt,
            "artifacts": [
                {
                    "kind": row["kind"],
                    "platform": row["platform"],
                    "artifact_id": row["artifact_id"],
                    "raw_actions_zip": os.fspath(row["raw_actions_zip"]),
                }
                for row in staged_pretag_rows
            ],
        }
        staged_input_payload = canonical_bytes(staged_input_set)
        staged_input_details = _stage_bytes(
            staged_input_payload,
            root_fd,
            "PRETAG-ARTIFACT-INPUT-SET.json",
            label="staged protected pre-tag artifact input set",
        )
        rows.append(
            {
                "name": "pretag_artifact_input_set",
                "path": "PRETAG-ARTIFACT-INPUT-SET.json",
                **staged_input_details,
            }
        )
        staged.pretag_artifact_input_set = stage_root / "PRETAG-ARTIFACT-INPUT-SET.json"

        provenance_set = {
            "schema": PRETAG_PROVENANCE_SET_SCHEMA,
            "repository": REPOSITORY,
            "commit": args.source_main_sha,
            "run_id": args.pretag_run_id,
            "run_attempt": args.pretag_run_attempt,
            "artifacts": [copy.deepcopy(row["provenance"]) for row in staged_pretag_rows],
        }
        provenance_set_payload = canonical_bytes(provenance_set)
        provenance_set_details = _stage_bytes(
            provenance_set_payload,
            root_fd,
            "PRETAG-INITIAL-LIVE-PROVENANCE-SET.json",
            label="protected pre-tag initial live provenance set",
        )
        rows.append(
            {
                "name": "pretag_initial_live_provenance_set",
                "path": "PRETAG-INITIAL-LIVE-PROVENANCE-SET.json",
                **provenance_set_details,
            }
        )
        staged.pretag_verified_artifacts = staged_pretag_rows
        staged.pretag_initial_provenance_set = (
            stage_root / "PRETAG-INITIAL-LIVE-PROVENANCE-SET.json"
        )

        manifest = {
            "schema": "arc.recovery.production-input-stage.v1",
            "source_main_commit": args.source_main_sha,
            "files": rows,
        }
        payload = canonical_bytes(manifest)
        manifest_fd = os.open(
            "STAGE-MANIFEST.json",
            os.O_WRONLY
            | os.O_CREAT
            | os.O_EXCL
            | getattr(os, "O_NOFOLLOW", 0)
            | getattr(os, "O_CLOEXEC", 0),
            0o400,
            dir_fd=root_fd,
        )
        try:
            offset = 0
            while offset < len(payload):
                written = os.write(manifest_fd, payload[offset:])
                if written <= 0:
                    fail("production input stage manifest write made no progress")
                offset += written
            os.fchmod(manifest_fd, 0o400)
            os.fsync(manifest_fd)
            manifest_details = os.fstat(manifest_fd)
            if (
                not stat.S_ISREG(manifest_details.st_mode)
                or manifest_details.st_uid != os.geteuid()
                or manifest_details.st_nlink != 1
                or stat.S_IMODE(manifest_details.st_mode) != 0o400
                or manifest_details.st_size != len(payload)
            ):
                fail("production input stage manifest failed its file identity contract")
        finally:
            os.close(manifest_fd)
        for descriptor in child_fds.values():
            os.fsync(descriptor)
            os.fchmod(descriptor, 0o500)
        os.fsync(root_fd)
        os.fchmod(root_fd, 0o500)
        os.fsync(parent_fd)
        staged.stage_manifest = stage_root / "STAGE-MANIFEST.json"
        return staged, sha256_bytes(payload)
    except OSError as error:
        fail(f"cannot create production input stage: {error}")
    finally:
        for descriptor in child_fds.values():
            os.close(descriptor)
        if root_fd >= 0:
            os.close(root_fd)
        os.close(parent_fd)


def artifact(path: Path, label: str, *, executable: bool = False) -> dict[str, str]:
    digest, _size = hash_secure(path, label, executable=executable)
    return {"path": os.fspath(path), "sha256": digest}


@contextlib.contextmanager
def stable_artifact_identity_window(
    manifest: Mapping[str, Any],
) -> Any:
    """Hold every staged artifact open across the final public API reproof.

    Rehashing all nine release ZIPs after the branch-last query could make that
    proof stale.  This window hashes first, retains no-follow descriptors, and
    then proves full descriptor/path identity immediately before publication.
    """

    opened: list[tuple[str, Path, int, tuple[int, ...]]] = []
    opened_directories: list[tuple[str, Path, int, tuple[int, ...]]] = []
    identity_fields = (
        "device",
        "inode",
        "mode",
        "owner",
        "group",
        "link-count",
        "size",
        "mtime",
    )

    def identity(details: os.stat_result) -> tuple[int, ...]:
        return (
            details.st_dev,
            details.st_ino,
            details.st_mode,
            details.st_uid,
            details.st_gid,
            details.st_nlink,
            details.st_size,
            details.st_mtime_ns,
        )

    def immediate_identity(details: os.stat_result) -> tuple[int, ...]:
        # ctime can advance asynchronously after create/chmod on APFS.  It is
        # useful for an instantaneous descriptor/path equality check, but is
        # not a deterministic cross-network-window identity field.
        return (*identity(details), details.st_ctime_ns)

    def changed_fields(actual: tuple[int, ...], expected: tuple[int, ...]) -> str:
        return ",".join(
            name
            for name, actual_value, expected_value in zip(
                identity_fields, actual, expected
            )
            if actual_value != expected_value
        )

    try:
        stage_manifest_artifact = manifest["artifacts"]["production_input_stage_manifest"]
        stage_manifest_path = Path(stage_manifest_artifact["path"])
        stage_root = stage_manifest_path.parent
        stage_value, _stage_bytes, _stage_sha = load_canonical_json(
            stage_manifest_path,
            label="final identity-window production stage manifest",
            maximum_bytes=4 * 1024 * 1024,
            exact_mode=0o400,
            require_read_only=True,
        )
        stage_rows = stage_value.get("files")
        if not isinstance(stage_rows, list):
            fail("final identity-window stage manifest has no file inventory")
        file_specs: list[tuple[str, Path, str]] = [
            (
                "production_input_stage_manifest",
                stage_manifest_path,
                stage_manifest_artifact["sha256"],
            )
        ]
        seen_paths = {stage_manifest_path}
        for index, raw_row in enumerate(stage_rows):
            row = require_exact_object(
                raw_row,
                {"name", "path", "sha256", "size_bytes", "mode"},
                f"final identity-window stage row {index}",
            )
            name = row["name"]
            relative = row["path"]
            if not isinstance(name, str) or not isinstance(relative, str):
                fail("final identity-window stage row has invalid name or path")
            path = stage_root / relative
            if path in seen_paths:
                fail("final identity-window stage inventory repeats a path")
            seen_paths.add(path)
            file_specs.append((name, path, require_hash(row["sha256"], f"stage row {name} sha256")))
        for label, path in (
            ("root", stage_root),
            ("source", stage_root / "source"),
            ("private", stage_root / "private"),
        ):
            descriptor = os.open(
                path,
                os.O_RDONLY
                | getattr(os, "O_DIRECTORY", 0)
                | getattr(os, "O_NOFOLLOW", 0)
                | getattr(os, "O_CLOEXEC", 0),
            )
            details = os.fstat(descriptor)
            visible = os.lstat(path)
            if (
                not stat.S_ISDIR(details.st_mode)
                or identity(details) != identity(visible)
                or details.st_uid != os.geteuid()
                or stat.S_IMODE(details.st_mode) != 0o500
            ):
                os.close(descriptor)
                fail(f"production stage {label} directory failed the final identity boundary")
            opened_directories.append((label, path, descriptor, identity(details)))

        for name, path, expected_sha in file_specs:
            descriptor = os.open(
                path,
                os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0),
            )
            details = os.fstat(descriptor)
            visible = os.lstat(path)
            if (
                not stat.S_ISREG(details.st_mode)
                or identity(details) != identity(visible)
                or details.st_uid != os.geteuid()
                or details.st_nlink != 1
                or details.st_mode & 0o022
            ):
                os.close(descriptor)
                fail(f"artifact {name} failed the final stable identity boundary")
            digest = hashlib.sha256()
            size = 0
            while chunk := os.read(descriptor, 1024 * 1024):
                digest.update(chunk)
                size += len(chunk)
            after = os.fstat(descriptor)
            visible_after = os.lstat(path)
            if (
                identity(details) != identity(after)
                or immediate_identity(after) != immediate_identity(visible_after)
                or size != after.st_size
                or digest.hexdigest() != expected_sha
            ):
                os.close(descriptor)
                fail(f"artifact {name} changed before the final live reproof")
            opened.append((name, path, descriptor, identity(after)))

        checked = False

        def recheck() -> None:
            nonlocal checked
            for name, path, descriptor, expected in opened:
                descriptor_identity = identity(os.fstat(descriptor))
                path_identity = identity(os.lstat(path))
                if descriptor_identity != expected or path_identity != expected:
                    descriptor_changes = changed_fields(descriptor_identity, expected)
                    path_changes = changed_fields(path_identity, expected)
                    fail(
                        f"artifact {name} changed during the final live reproof "
                        f"(descriptor={descriptor_changes or 'none'}; "
                        f"path={path_changes or 'none'})"
                    )
            for label, path, descriptor, expected in opened_directories:
                descriptor_identity = identity(os.fstat(descriptor))
                path_identity = identity(os.lstat(path))
                if descriptor_identity != expected or path_identity != expected:
                    descriptor_changes = changed_fields(descriptor_identity, expected)
                    path_changes = changed_fields(path_identity, expected)
                    fail(
                        f"production stage {label} directory changed during final "
                        f"live reproof (descriptor={descriptor_changes or 'none'}; "
                        f"path={path_changes or 'none'})"
                    )
            checked = True

        yield recheck
        if not checked:
            fail("final artifact identity window was not rechecked before publication")
    except OSError as error:
        fail(f"final artifact identity window is unavailable: {error}")
    finally:
        for _name, _path, descriptor, _expected in opened:
            os.close(descriptor)
        for _label, _path, descriptor, _expected in opened_directories:
            os.close(descriptor)


def load_canonical_json(
    path: Path,
    *,
    label: str,
    maximum_bytes: int = MAX_JSON_BYTES,
    exact_mode: int | None = None,
    require_read_only: bool = False,
) -> tuple[dict[str, Any], bytes, str]:
    payload, _details = read_secure(
        path,
        label=label,
        maximum_bytes=maximum_bytes,
        exact_mode=exact_mode,
        require_read_only=require_read_only,
    )
    value = decode_json(payload, label)
    if not isinstance(value, dict) or canonical_bytes(value) != payload:
        fail(f"{label} must be one canonical JSON object")
    return value, payload, sha256_bytes(payload)


def pretag_artifact_key(kind: str, platform: str) -> str:
    return f"pretag_raw_{kind}_{platform.replace('-', '_')}"


def load_pretag_input_set(
    path: Path,
    *,
    source_main_sha: str,
    run_id: int,
    run_attempt: int,
) -> tuple[list[dict[str, Any]], bytes, str]:
    """Load only transport coordinates; every tuple is authorized live later."""

    value, payload, digest = load_canonical_json(
        path,
        label="protected pre-tag artifact input set",
        maximum_bytes=1024 * 1024,
        exact_mode=0o400,
        require_read_only=True,
    )
    value = require_exact_object(
        value,
        {
            "schema",
            "repository",
            "commit",
            "run_id",
            "run_attempt",
            "artifacts",
        },
        "protected pre-tag artifact input set",
    )
    expected_header = {
        "schema": PRETAG_INPUT_SET_SCHEMA,
        "repository": REPOSITORY,
        "commit": source_main_sha,
        "run_id": run_id,
        "run_attempt": run_attempt,
    }
    for field, expected in expected_header.items():
        if value.get(field) != expected:
            fail(f"protected pre-tag artifact input set {field} differs")
    rows = value["artifacts"]
    if not isinstance(rows, list) or len(rows) != len(PRETAG_GROUPS):
        fail("protected pre-tag artifact input set must contain exactly nine groups")
    result: list[dict[str, Any]] = []
    seen_ids: set[int] = set()
    seen_paths: set[Path] = set()
    for index, (expected_group, raw) in enumerate(zip(PRETAG_GROUPS, rows)):
        row = require_exact_object(
            raw,
            {"kind", "platform", "artifact_id", "raw_actions_zip"},
            f"protected pre-tag artifact input {index}",
        )
        kind, platform = expected_group
        if row.get("kind") != kind or row.get("platform") != platform:
            fail("protected pre-tag artifact input groups are missing, duplicated, or out of order")
        artifact_id = require_uint(
            row.get("artifact_id"), f"protected pre-tag {kind}/{platform} artifact id", positive=True
        )
        if artifact_id in seen_ids:
            fail("protected pre-tag artifact IDs must be unique across all nine groups")
        raw_path_value = row.get("raw_actions_zip")
        if not isinstance(raw_path_value, str):
            fail(f"protected pre-tag {kind}/{platform} raw ZIP path must be a string")
        raw_path = Path(raw_path_value)
        _lexical_absolute(raw_path, f"protected pre-tag {kind}/{platform} raw ZIP")
        if raw_path in seen_paths:
            fail("protected pre-tag raw ZIP paths must be unique across all nine groups")
        seen_ids.add(artifact_id)
        seen_paths.add(raw_path)
        result.append(
            {
                "kind": kind,
                "platform": platform,
                "artifact_id": artifact_id,
                "raw_actions_zip": raw_path,
            }
        )
    return result, payload, digest


def create_private_seal(path: Path, value: Mapping[str, Any]) -> str:
    if path.suffix != ".json":
        fail("rollout output must end in .json")
    payload = canonical_bytes(value)
    digest = sha256_bytes(payload)
    sidecar = path.with_name(path.name + ".sha256")
    output_parent, output_name = open_parent_directory(path, "rollout output")
    sidecar_parent, sidecar_name = open_parent_directory(sidecar, "rollout checksum output")
    if os.fstat(output_parent).st_ino != os.fstat(sidecar_parent).st_ino or os.fstat(
        output_parent
    ).st_dev != os.fstat(sidecar_parent).st_dev:
        os.close(output_parent)
        os.close(sidecar_parent)
        fail("rollout and checksum outputs must share one exact parent directory")
    created: list[tuple[int, str]] = []
    try:
        for parent_fd, name, body in (
            (output_parent, output_name, payload),
            (sidecar_parent, sidecar_name, f"{digest}  {path.name}\n".encode("ascii")),
        ):
            try:
                descriptor = os.open(
                    name,
                    os.O_WRONLY
                    | os.O_CREAT
                    | os.O_EXCL
                    | getattr(os, "O_NOFOLLOW", 0)
                    | getattr(os, "O_CLOEXEC", 0),
                    0o400,
                    dir_fd=parent_fd,
                )
            except FileExistsError:
                fail(f"sealed output already exists; refusing replacement: {path.parent / name}")
            created.append((parent_fd, name))
            try:
                offset = 0
                while offset < len(body):
                    offset += os.write(descriptor, body[offset:])
                os.fsync(descriptor)
                os.fchmod(descriptor, 0o400)
            finally:
                os.close(descriptor)
        os.fsync(output_parent)
        return digest
    except Exception:
        for parent_fd, name in reversed(created):
            try:
                os.unlink(name, dir_fd=parent_fd)
            except OSError:
                pass
        try:
            os.fsync(output_parent)
        except OSError:
            pass
        raise
    finally:
        os.close(output_parent)
        os.close(sidecar_parent)


def load_private_rollout(path: Path) -> tuple[dict[str, Any], bytes, str]:
    value, payload, digest = load_canonical_json(
        path,
        label="sealed prearchive rollout",
        exact_mode=0o400,
    )
    sidecar_payload, _ = read_secure(
        path.with_name(path.name + ".sha256"),
        label="sealed prearchive rollout checksum",
        maximum_bytes=512,
        exact_mode=0o400,
    )
    if sidecar_payload != f"{digest}  {path.name}\n".encode("ascii"):
        fail("sealed prearchive rollout checksum differs from the exact canonical bytes")
    try:
        rollout.validate_manifest(value)
        rollout.require_prearchive_manifest(value)
    except rollout.RolloutError as error:
        fail(f"prearchive rollout failed validation: {error}")
    return value, payload, digest


def validate_build_metadata(args: argparse.Namespace) -> tuple[dict[str, Any], str]:
    payload, _ = read_secure(
        args.build_metadata,
        label="retained pre-tag BUILD-METADATA.json",
        maximum_bytes=MAX_METADATA_BYTES,
    )
    metadata = decode_json(payload, "retained pre-tag BUILD-METADATA.json")
    fields = {
        "schema",
        "kind",
        "repository",
        "commit",
        "platform",
        "rust_target",
        "version",
        "workflow_run_id",
        "workflow_run_attempt",
        "files",
    }
    metadata = require_exact_object(metadata, fields, "pre-tag build metadata")
    if payload != (
        json.dumps(metadata, sort_keys=True, indent=2, ensure_ascii=True) + "\n"
    ).encode("utf-8"):
        fail("retained pre-tag BUILD-METADATA.json bytes are not the packaged canonical form")
    expected = {
        "schema": "arc.pretag.artifact.v1",
        "kind": "headless",
        "repository": REPOSITORY,
        "commit": args.source_main_sha,
        "platform": PRETAG_PLATFORM,
        "rust_target": PRETAG_TARGET,
        "version": VERSION,
        "workflow_run_id": args.pretag_run_id,
        "workflow_run_attempt": args.pretag_run_attempt,
    }
    for field, wanted in expected.items():
        if metadata.get(field) != wanted:
            fail(
                f"pre-tag build metadata {field} differs: expected {wanted!r}, "
                f"got {metadata.get(field)!r}"
            )
    files = metadata["files"]
    expected_names = {
        "arc-node-linux-x86_64": args.binary,
        "arc-cli-linux-x86_64": args.cli,
        "genesis.toml": args.genesis,
    }
    if not isinstance(files, dict) or set(files) != set(expected_names):
        fail("pre-tag build metadata payload membership differs from the Linux x86_64 group")
    for name, path in expected_names.items():
        expected_hash = require_hash(files[name], f"build metadata hash for {name}")
        actual, _size = hash_secure(
            path,
            f"pre-tag payload {name}",
            executable=name != "genesis.toml",
        )
        if actual != expected_hash:
            fail(f"pre-tag payload {name} differs from BUILD-METADATA.json")
    return metadata, sha256_bytes(payload)


def validate_genesis(path: Path) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    payload, _ = read_secure(path, label="production genesis", maximum_bytes=1024 * 1024)
    try:
        value = tomllib.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        fail(f"production genesis is invalid UTF-8 TOML: {error}")
    if not isinstance(value, dict):
        fail("production genesis must be a TOML object")
    chain = value.get("chain")
    if not isinstance(chain, dict):
        fail("production genesis has no [chain] table")
    exact_chain = {
        "name": "arc-testnet",
        "chain_id": "0x415243",
        "validator_set_complete": True,
    }
    for field, wanted in exact_chain.items():
        if chain.get(field) != wanted:
            fail(f"production genesis chain.{field} differs from {wanted!r}")
    activation = require_uint(
        chain.get("community_rewards_v1_activation_height"),
        "genesis community reward activation height",
        positive=True,
    )
    validators = value.get("validators")
    if not isinstance(validators, list) or len(validators) != len(FLEET):
        fail("production genesis must contain exactly six validators")
    result: list[dict[str, Any]] = []
    seen: set[str] = set()
    for index, raw in enumerate(validators):
        row = require_exact_object(raw, {"address", "stake"}, f"genesis validator {index}")
        address = require_hash(row["address"], f"genesis validator {index} address")
        stake = require_uint(row["stake"], f"genesis validator {index} stake", positive=True)
        if address in seen:
            fail("production genesis contains a duplicate validator address")
        seen.add(address)
        result.append({"address": address, "stake": stake})
    accounts = value.get("accounts")
    if not isinstance(accounts, list):
        fail("production genesis has no account allocation list")
    account_addresses = {
        row.get("address") for row in accounts if isinstance(row, dict)
    }
    if not seen.issubset(account_addresses):
        fail("production genesis omits one or more validator account allocations")
    total = sum(row["stake"] for row in result)
    if any((total - row["stake"]) * 3 <= total * 2 for row in result):
        fail("production genesis does not preserve strict quorum during a one-node restart")
    return {"activation_height": activation}, result


def validate_validator_public_keys(
    path: Path, genesis_validators: list[dict[str, Any]]
) -> tuple[list[dict[str, Any]], str, int]:
    payload, details = read_secure(
        path,
        label="validator public-key manifest",
        maximum_bytes=1024 * 1024,
        require_read_only=True,
    )
    value = decode_json(payload, "validator public-key manifest")
    expected_payload = (
        json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True) + "\n"
    ).encode("utf-8")
    if payload != expected_payload:
        fail("validator public-key manifest must be canonical JSON")
    if not isinstance(value, list) or len(value) != len(FLEET):
        fail("validator public-key manifest must contain exactly six rows")
    normalized: list[dict[str, Any]] = []
    public_keys: set[str] = set()
    for index, (raw, genesis) in enumerate(zip(value, genesis_validators)):
        row = require_exact_object(
            raw,
            {"address", "public_key", "stake"},
            f"validator public-key row {index}",
        )
        address = require_hash(row["address"], f"validator public-key row {index} address")
        public_key = require_hash(
            row["public_key"], f"validator public-key row {index} public_key"
        )
        stake = require_uint(row["stake"], f"validator public-key row {index} stake", positive=True)
        if {"address": address, "stake": stake} != genesis:
            fail("validator public-key address/stake order differs from complete genesis")
        if public_key in public_keys:
            fail("validator public-key manifest contains a duplicate public key")
        public_keys.add(public_key)
        normalized.append({"address": address, "public_key": public_key, "stake": stake})
    return normalized, sha256_bytes(payload), details.st_size


def validate_legacy_validator_set(path: Path, expected_sha256: str) -> tuple[str, int]:
    payload, details = read_secure(
        path,
        label="legacy validator set",
        maximum_bytes=1024 * 1024,
        require_read_only=True,
    )
    actual = sha256_bytes(payload)
    if actual != expected_sha256:
        fail("legacy validator set bytes differ from the sealed freeze plan")
    value = decode_json(payload, "legacy validator set")
    if not isinstance(value, list) or len(value) != 8:
        fail("legacy validator set must contain exactly eight validators")
    addresses: set[str] = set()
    total = 0
    for index, raw in enumerate(value):
        row = require_exact_object(raw, {"address", "stake"}, f"legacy validator {index}")
        address = require_hash(row["address"], f"legacy validator {index} address")
        stake = require_uint(row["stake"], f"legacy validator {index} stake", positive=True)
        if address in addresses or stake != 5_000_000:
            fail("legacy validator set address/stake contract differs")
        addresses.add(address)
        total += stake
    if total != 40_000_000:
        fail("legacy validator set total stake must be exactly 40,000,000")
    return actual, details.st_size


def validate_freeze_inputs(args: argparse.Namespace) -> tuple[dict[str, Any], bytes, str, str, bytes]:
    freeze_payload, _details = read_secure(
        args.freeze_plan,
        label="sealed freeze plan",
        maximum_bytes=32 * 1024 * 1024,
        require_read_only=True,
    )
    freeze_sidecar_path = args.freeze_plan.with_name(args.freeze_plan.name + ".sha256")
    freeze_sidecar, _ = read_secure(
        freeze_sidecar_path,
        label="sealed freeze plan checksum",
        maximum_bytes=512,
        require_read_only=True,
    )
    freeze_sha = sha256_bytes(freeze_payload)
    if freeze_sha != args.freeze_plan_sha256:
        fail("freeze plan bytes differ from the explicitly selected hash")
    if freeze_sidecar != f"{freeze_sha}  {args.freeze_plan.name}\n".encode("ascii"):
        fail("freeze plan checksum does not bind the exact canonical plan bytes")
    try:
        pinned = validate_pinned_freeze_plan(freeze_payload, freeze_sha)
    except FreezeValidationError as error:
        fail(f"freeze plan failed complete v5 validation: {error}")
    value = pinned.value()
    if value.get("source_commit") != args.source_main_sha:
        fail("freeze plan is not bound to the exact protected-main commit")
    expected_topology = list(FLEET)
    observed_topology = [
        (row.get("name"), row.get("host"))
        for row in value.get("nodes", [])
        if isinstance(row, dict)
    ]
    if observed_topology != expected_topology:
        fail("freeze plan validator order/IP topology differs from the reviewed fleet")
    for row in value["nodes"]:
        name = row["name"]
        if (
            row.get("model_sha256") != rollout.CANONICAL_MODEL_SHA256
            or row.get("model_size_bytes") != rollout.CANONICAL_MODEL_SIZE_BYTES
            or row.get("shard_ranges") != SHARDS[name]
        ):
            fail(f"freeze plan {name} model or shard assignment differs from production")
    drive = value.get("drive_prefreeze")
    if not isinstance(drive, dict) or drive.get("remote_root") != (
        "arc-drive-arc:ARC Chain Recovery v0.8"
    ):
        fail("freeze plan does not use the dedicated ARC production Drive root")
    if sha256_bytes(drive["remote_root"].encode("utf-8")) != drive.get(
        "remote_root_sha256"
    ):
        fail("freeze plan Drive root hash is not derived from its exact text")
    execution_files = (
        (SCRIPT_DIR / "archive-fleet-to-drive.sh", "orchestrator_sha256"),
        (SCRIPT_DIR / "archive-node.sh", "remote_helper_sha256"),
        (SCRIPT_DIR / "recovery_rollout.py", "rollout_tool_sha256"),
        (SCRIPT_DIR / "recovery-manifest.schema.json", "rollout_schema_sha256"),
    )
    for path, field in execution_files:
        actual, _size = hash_secure(path, f"recovery provenance {field}")
        if actual != value.get(field):
            fail(f"freeze plan {field} differs from the executing protected-main bytes")
    return value, freeze_payload, freeze_sha, sha256_bytes(freeze_sidecar), freeze_sidecar


def validate_legacy_maintenance_evidence_bundle(
    args: argparse.Namespace,
    freeze: Mapping[str, Any],
    freeze_sha: str,
    height_receipt: Mapping[str, Any],
    height_receipt_sha: str,
) -> tuple[str, int, dict[str, Any], bytes, bytes]:
    """Authenticate the complete create-only pre-boundary maintenance evidence set."""

    payload, details = read_secure(
        args.legacy_maintenance_evidence_bundle,
        label="legacy maintenance evidence bundle",
        maximum_bytes=32 * 1024 * 1024,
        exact_mode=0o400,
    )
    sidecar_path = args.legacy_maintenance_evidence_bundle.with_name(
        args.legacy_maintenance_evidence_bundle.name + ".sha256"
    )
    sidecar, _ = read_secure(
        sidecar_path,
        label="legacy maintenance evidence bundle checksum",
        maximum_bytes=512,
        exact_mode=0o400,
    )
    digest = sha256_bytes(payload)
    if sidecar != (
        f"{digest}  {args.legacy_maintenance_evidence_bundle.name}\n".encode("ascii")
    ):
        fail("legacy maintenance evidence bundle checksum does not bind the exact bytes")
    value = decode_json(payload, "legacy maintenance evidence bundle")
    if not isinstance(value, dict) or canonical_bytes(value) != payload:
        fail("legacy maintenance evidence bundle must be canonical JSON")
    bundle = require_exact_object(
        value,
        {
            "schema",
            "source_main_commit",
            "freeze_plan_sha256",
            "capture_id",
            "first_quarantine_started_at",
            "all_controlled_stopped_at",
            "challenge",
            "authenticated_prefence_height_cross_proof",
            "network_quarantine_challenge",
            "quarantine_stability_proof",
            "nodes",
            "object_inventory",
            "aggregate_root_sha256",
        },
        "legacy maintenance evidence bundle",
    )
    capture_id = rollout.capture_id_for_freeze_plan_hash(freeze_sha)
    expected_identity = {
        "schema": "arc.recovery.legacy-maintenance-evidence-bundle.v1",
        "source_main_commit": args.source_main_sha,
        "freeze_plan_sha256": freeze_sha,
        "capture_id": capture_id,
    }
    for field, expected in expected_identity.items():
        if bundle.get(field) != expected:
            fail(f"legacy maintenance evidence bundle {field} differs")
    first = _parse_utc_seconds(
        bundle.get("first_quarantine_started_at"),
        "maintenance evidence first quarantine",
    )
    stopped = _parse_utc_seconds(
        bundle.get("all_controlled_stopped_at"),
        "maintenance evidence all-controlled-stopped",
    )
    if first > stopped:
        fail("legacy maintenance evidence timestamps are reversed")
    challenge = require_hash(bundle.get("challenge"), "maintenance evidence challenge")

    inventory_expected: list[dict[str, Any]] = []

    def sealed(raw: Any, node: str, role: str, label: str) -> tuple[dict[str, Any], str]:
        wrapper = require_exact_object(raw, {"value", "sha256"}, label)
        object_value = wrapper.get("value")
        if not isinstance(object_value, dict):
            fail(f"{label} value must be an object")
        object_payload = canonical_bytes(object_value)
        object_sha = require_hash(wrapper.get("sha256"), f"{label} sha256")
        if sha256_bytes(object_payload) != object_sha:
            fail(f"{label} hash is not reproducible from its canonical value")
        inventory_expected.append(
            {"node": node, "role": role, "sha256": object_sha, "size": len(object_payload)}
        )
        return copy.deepcopy(object_value), object_sha

    authenticated, authenticated_sha = sealed(
        bundle.get("authenticated_prefence_height_cross_proof"),
        "fleet",
        "authenticated-prefence-height-cross-proof",
        "maintenance authenticated pre-fence proof",
    )
    authenticated = require_exact_object(
        authenticated,
        {
            "schema",
            "source_main_commit",
            "freeze_plan_sha256",
            "capture_id",
            "legacy_public_height_receipt_sha256",
            "challenge",
            "started_at",
            "completed_at",
            "conservative_height_floor",
            "nodes",
        },
        "maintenance authenticated pre-fence fleet proof",
    )
    if (
        authenticated.get("schema")
        != "arc.recovery.authenticated-legacy-height-fleet.v1"
        or authenticated.get("source_main_commit") != args.source_main_sha
        or authenticated.get("freeze_plan_sha256") != freeze_sha
        or authenticated.get("capture_id") != capture_id
        or authenticated.get("legacy_public_height_receipt_sha256") != height_receipt_sha
        or authenticated.get("challenge") != challenge
    ):
        fail("maintenance authenticated pre-fence fleet proof identity differs")
    authenticated_nodes = authenticated.get("nodes")
    if (
        not isinstance(authenticated_nodes, list)
        or [(row.get("node"), row.get("host")) for row in authenticated_nodes]
        != list(FLEET)
    ):
        fail("maintenance authenticated pre-fence fleet topology differs")

    challenge_receipt, _challenge_sha = sealed(
        bundle.get("network_quarantine_challenge"),
        "fleet",
        "network-quarantine-challenge",
        "maintenance quarantine challenge receipt",
    )
    if challenge_receipt != {
        "schema": "arc.recovery.legacy-network-quarantine-challenge.v1",
        "freeze_plan_sha256": freeze_sha,
        "capture_id": capture_id,
        "challenge": challenge,
    }:
        fail("maintenance quarantine challenge receipt differs")

    stability, stability_sha = sealed(
        bundle.get("quarantine_stability_proof"),
        "fleet",
        "network-quarantine-stability-proof",
        "maintenance quarantine stability proof",
    )
    stability = require_exact_object(
        stability,
        {
            "schema",
            "source_main_commit",
            "freeze_plan_sha256",
            "capture_id",
            "challenge",
            "interval_seconds",
            "sample_count",
            "started_at",
            "completed_at",
            "monotonic_elapsed_ns",
            "fleet_heads",
            "nodes",
            "global_absence_claimed",
        },
        "maintenance quarantine stability proof value",
    )
    if (
        stability.get("schema")
        != "arc.recovery.legacy-network-quarantine-stability.v1"
        or stability.get("source_main_commit") != args.source_main_sha
        or stability.get("freeze_plan_sha256") != freeze_sha
        or stability.get("capture_id") != capture_id
        or stability.get("challenge") != challenge
        or stability.get("interval_seconds") != 120
        or stability.get("sample_count") != 2
        or stability.get("global_absence_claimed") is not False
    ):
        fail("maintenance quarantine stability proof identity differs")
    elapsed_ns = require_uint(
        stability.get("monotonic_elapsed_ns"),
        "maintenance quarantine stability monotonic elapsed nanoseconds",
        positive=True,
    )
    if elapsed_ns < 120_000_000_000:
        fail("maintenance quarantine stability interval is below 120 seconds")
    stability_started = _parse_utc_seconds(
        stability.get("started_at"), "maintenance quarantine stability started_at"
    )
    stability_completed = _parse_utc_seconds(
        stability.get("completed_at"), "maintenance quarantine stability completed_at"
    )
    if not first <= stability_started <= stability_completed <= stopped:
        fail("maintenance quarantine stability timestamps are outside the quarantine/stop window")

    stability_rows = stability.get("nodes")
    stability_heads = stability.get("fleet_heads")
    if (
        not isinstance(stability_rows, list)
        or not isinstance(stability_heads, list)
        or [(row.get("node"), row.get("host")) for row in stability_rows]
        != list(FLEET)
        or [(row.get("node"), row.get("host")) for row in stability_heads]
        != list(FLEET)
    ):
        fail("maintenance quarantine stability topology differs")
    stability_status_fields = {
        "schema",
        "capture_id",
        "node",
        "freeze_plan_sha256",
        "receipt_sha256",
        "table",
        "rule_counters",
        "counter_snapshot_sha256",
        "owned_ruleset_stateless_sha256",
        "listener_inventory",
        "loopback_head",
        "quarantine_policy",
        "active",
        "enabled",
    }
    stability_sample_fields = {
        "schema",
        "capture_id",
        "node",
        "freeze_plan_sha256",
        "challenge",
        "sample_index",
        "started_at",
        "completed_at",
        "quarantine_status_before",
        "quarantine_status_before_sha256",
        "quarantine_status_after",
        "quarantine_status_after_sha256",
        "writer",
        "listener_ownership",
        "head",
        "output_deny_packets",
        "ss_sha256",
        "global_absence_claimed",
    }
    normalized_stability_rows: list[dict[str, Any]] = []
    normalized_stability_heads: list[dict[str, Any]] = []
    for row_index, ((name, host), raw_row, raw_fleet_head) in enumerate(
        zip(FLEET, stability_rows, stability_heads)
    ):
        row = require_exact_object(
            raw_row,
            {"node", "host", "samples", "output_deny_packets"},
            f"maintenance quarantine stability node {row_index}",
        )
        if (row.get("node"), row.get("host")) != (name, host):
            fail(f"maintenance quarantine stability topology differs at {name}")
        samples = row.get("samples")
        if not isinstance(samples, list) or len(samples) != 2:
            fail(f"maintenance quarantine stability samples differ at {name}")
        normalized_samples: list[dict[str, Any]] = []
        projected_heads: list[dict[str, Any]] = []
        counters: list[int] = []
        writers: list[dict[str, Any]] = []
        sample_times: list[tuple[dt.datetime, dt.datetime]] = []
        for sample_index, raw_sample_wrapper in enumerate(samples):
            sample_wrapper = require_exact_object(
                raw_sample_wrapper,
                {"value", "sha256"},
                f"maintenance quarantine stability sealed sample {name}/{sample_index}",
            )
            sample = require_exact_object(
                sample_wrapper.get("value"),
                stability_sample_fields,
                f"maintenance quarantine stability sample {name}/{sample_index}",
            )
            sample_payload = canonical_bytes(sample)
            sample_sha = require_hash(
                sample_wrapper.get("sha256"),
                f"maintenance quarantine stability sample {name}/{sample_index} root",
            )
            if sha256_bytes(sample_payload) != sample_sha:
                fail(f"maintenance quarantine stability sample root differs at {name}/{sample_index}")
            if (
                sample.get("schema")
                != "arc.recovery.legacy-network-quarantine-stability-sample.v1"
                or (
                    sample.get("capture_id"),
                    sample.get("node"),
                    sample.get("freeze_plan_sha256"),
                    sample.get("challenge"),
                    sample.get("sample_index"),
                )
                != (capture_id, name, freeze_sha, challenge, sample_index)
                or sample.get("global_absence_claimed") is not False
            ):
                fail(f"maintenance quarantine stability sample identity differs at {name}/{sample_index}")
            sample_started = _parse_utc_seconds(
                sample.get("started_at"),
                f"maintenance quarantine stability sample {name}/{sample_index} started_at",
            )
            sample_completed = _parse_utc_seconds(
                sample.get("completed_at"),
                f"maintenance quarantine stability sample {name}/{sample_index} completed_at",
            )
            if not stability_started <= sample_started <= sample_completed <= stability_completed:
                fail(f"maintenance quarantine stability sample timestamps differ at {name}/{sample_index}")
            sample_times.append((sample_started, sample_completed))
            for status_side in ("before", "after"):
                status = require_exact_object(
                    sample.get(f"quarantine_status_{status_side}"),
                    stability_status_fields,
                    f"maintenance quarantine stability {name}/{sample_index} {status_side} status",
                )
                status_sha = require_hash(
                    sample.get(f"quarantine_status_{status_side}_sha256"),
                    f"maintenance quarantine stability {name}/{sample_index} {status_side} status root",
                )
                if (
                    sha256_bytes(canonical_bytes(status)) != status_sha
                    or status.get("schema")
                    != "arc.recovery.legacy-network-quarantine-status.v1"
                    or (
                        status.get("capture_id"),
                        status.get("node"),
                        status.get("freeze_plan_sha256"),
                    )
                    != (capture_id, name, freeze_sha)
                    or status.get("active") is not True
                    or status.get("enabled") is not True
                ):
                    fail(f"maintenance quarantine stability status differs at {name}/{sample_index}/{status_side}")
            before_status = sample["quarantine_status_before"]
            after_status = sample["quarantine_status_after"]
            if (
                before_status["receipt_sha256"] != after_status["receipt_sha256"]
                or before_status["owned_ruleset_stateless_sha256"]
                != after_status["owned_ruleset_stateless_sha256"]
            ):
                fail(f"maintenance quarantine stability fence changed at {name}/{sample_index}")
            writer = require_exact_object(
                sample.get("writer"),
                {"pid", "start_ticks", "executable_sha256", "argv_sha256", "cgroup_sha256"},
                f"maintenance quarantine stability writer {name}/{sample_index}",
            )
            require_uint(writer.get("pid"), f"stability writer PID {name}/{sample_index}", positive=True)
            require_uint(
                writer.get("start_ticks"),
                f"stability writer start ticks {name}/{sample_index}",
                positive=True,
            )
            for field in ("executable_sha256", "argv_sha256", "cgroup_sha256"):
                require_hash(writer.get(field), f"stability writer {field} {name}/{sample_index}")
            listeners = require_exact_object(
                sample.get("listener_ownership"),
                {"rpc_tcp_9090_ss_sha256", "p2p_udp_9091_ss_sha256", "writer_pid"},
                f"maintenance quarantine stability listeners {name}/{sample_index}",
            )
            if listeners.get("writer_pid") != writer["pid"]:
                fail(f"maintenance quarantine stability listener writer differs at {name}/{sample_index}")
            for field in ("rpc_tcp_9090_ss_sha256", "p2p_udp_9091_ss_sha256"):
                require_hash(listeners.get(field), f"stability listener {field} {name}/{sample_index}")
            require_hash(sample.get("ss_sha256"), f"stability ss root {name}/{sample_index}")
            sample_head = require_exact_object(
                sample.get("head"),
                {"height", "block_hash", "state_root", "response_sha256", "stable_attempt"},
                f"maintenance quarantine stability head {name}/{sample_index}",
            )
            projected = {
                "height": require_uint(
                    sample_head.get("height"),
                    f"maintenance quarantine stability height {name}/{sample_index}",
                    positive=True,
                ),
                "block_hash": require_hash(
                    sample_head.get("block_hash"),
                    f"maintenance quarantine stability block hash {name}/{sample_index}",
                ),
                "state_root": require_hash(
                    sample_head.get("state_root"),
                    f"maintenance quarantine stability state root {name}/{sample_index}",
                ),
            }
            response_roots = require_exact_object(
                sample_head.get("response_sha256"),
                {"info_before", "latest", "exact", "info_after"},
                f"maintenance quarantine stability response roots {name}/{sample_index}",
            )
            for field in ("info_before", "latest", "exact", "info_after"):
                require_hash(response_roots.get(field), f"stability response {field} {name}/{sample_index}")
            stable_attempt = require_uint(
                sample_head.get("stable_attempt"),
                f"maintenance quarantine stability attempt {name}/{sample_index}",
                positive=True,
            )
            if stable_attempt > 10:
                fail(f"maintenance quarantine stability attempt exceeds ten at {name}/{sample_index}")
            counter = require_uint(
                sample.get("output_deny_packets"),
                f"maintenance quarantine stability output deny counter {name}/{sample_index}",
            )
            projected_heads.append(projected)
            counters.append(counter)
            writers.append(copy.deepcopy(writer))
            normalized_samples.append({"value": copy.deepcopy(sample), "sha256": sample_sha})
        if sample_times[0][1] > sample_times[1][0]:
            fail(f"maintenance quarantine stability samples overlap or are reversed at {name}")
        if projected_heads[0] != projected_heads[1]:
            fail(f"maintenance quarantine stability per-host head changed at {name}")
        if writers[0] != writers[1]:
            fail(f"maintenance quarantine stability writer changed at {name}")
        if counters[1] < counters[0]:
            fail(f"maintenance quarantine stability output deny counter regressed at {name}")
        expected_counters = {"sample_0": counters[0], "sample_1": counters[1]}
        if row.get("output_deny_packets") != expected_counters:
            fail(f"maintenance quarantine stability counter summary differs at {name}")
        fleet_head = require_exact_object(
            raw_fleet_head,
            {"node", "host", "head"},
            f"maintenance quarantine stability fleet head {name}",
        )
        if fleet_head != {"node": name, "host": host, "head": projected_heads[0]}:
            fail(f"maintenance quarantine stability fleet head differs at {name}")
        normalized_stability_rows.append(
            {
                "node": name,
                "host": host,
                "samples": normalized_samples,
                "output_deny_packets": expected_counters,
            }
        )
        normalized_stability_heads.append(copy.deepcopy(fleet_head))
    stability["nodes"] = normalized_stability_rows
    stability["fleet_heads"] = normalized_stability_heads

    raw_nodes = bundle.get("nodes")
    if not isinstance(raw_nodes, list) or len(raw_nodes) != len(FLEET):
        fail("legacy maintenance evidence bundle must contain exactly six nodes")
    status_fields = {
        "schema",
        "capture_id",
        "node",
        "freeze_plan_sha256",
        "receipt_sha256",
        "table",
        "rule_counters",
        "counter_snapshot_sha256",
        "owned_ruleset_stateless_sha256",
        "listener_inventory",
        "loopback_head",
        "quarantine_policy",
        "active",
        "enabled",
    }
    monitor_fields = {
        "schema", "capture_id", "node", "freeze_plan_sha256",
        "network_quarantine_receipt_sha256", "monitor_contract_sha256",
        "semantic_interpreter", "firewall_loader_inventory", "file_sha256", "unit",
        "legacy_exec_start_pre", "incident_latched", "continuous_fail_closed",
        "automatic_unfence", "global_absence_claimed",
    }
    external_fields = {
        "schema",
        "capture_id",
        "node",
        "host",
        "freeze_plan_sha256",
        "challenge",
        "started_at",
        "completed_at",
        "operator_source_address",
        "listener_inventory",
        "targets",
        "results",
        "network_quarantine_receipt_sha256",
        "before_status_sha256",
        "after_status_sha256",
        "after_status",
        "deny_counter",
        "ssh_status_reproved",
        "global_absence_claimed",
    }
    cross_fields = {
        "schema",
        "capture_id",
        "node",
        "freeze_plan_sha256",
        "challenge",
        "network_quarantine_receipt_sha256",
        "quarantine_status_sha256",
        "quarantine_status",
        "rule_counters",
        "public_info_after_block",
        "public_latest_block",
        "fenced_head",
        "fenced_head_covers_public_info_after",
        "public_latest_hash_matches",
        "global_absence_claimed",
    }
    persisted_fields = {
        "schema",
        "source_main_commit",
        "capture_id",
        "node",
        "freeze_plan_sha256",
        "boot_id",
        "inspector_binary_sha256",
        "genesis_sha256",
        "validator_public_keys_sha256",
        "legacy_validator_set_sha256",
        "network_quarantine_receipt_sha256",
        "stop_complete_sha256",
        "stop_files_sha256",
        "capture_complete_sha256",
        "capture_files_sha256",
        "capture_source_sha256",
        "source_data_index_sha256",
        "state_wal_sha256",
        "state_wal_size",
        "snapshot_sha256",
        "snapshot_size",
        "source_file_identity",
        "staged_file_contract",
        "export_summary_sha256",
        "inspect_summary_sha256",
        "wal_boundary_sha256",
        "export_status",
        "head",
        "candidate_checkpoint_sha256",
        "candidate_checkpoint_size",
        "snapshot_path",
        "state_wal_path",
        "export_contract",
        "completed_at",
        "rerun_reexecutes_export",
        "writer_stopped",
        "restart_barrier_active",
        "network_quarantine_active",
        "global_absence_claimed",
    }
    stopped_fields = {
        "schema",
        "capture_id",
        "node",
        "freeze_plan_sha256",
        "validator_address",
        "stake",
        "stopped",
        "restart_fenced",
        "stop_schema",
        "stop_complete_sha256",
        "stop_files_sha256",
    }
    normalized_nodes: list[dict[str, Any]] = []
    for index, ((name, host), frozen, public, raw_node) in enumerate(
        zip(FLEET, freeze["nodes"], height_receipt["origins"], raw_nodes)
    ):
        node = require_exact_object(
            raw_node,
            {
                "node",
                "host",
                "stopped_status",
                "quarantine_status",
                "quarantine_monitor",
                "post_proof_quarantine_status",
                "external_quarantine_proof",
                "public_cross_proof",
                "persisted_head",
            },
            f"maintenance evidence node {index}",
        )
        if (node.get("node"), node.get("host")) != (name, host):
            fail(f"maintenance evidence topology differs at {name}")
        stopped_value, stopped_sha = sealed(
            node.get("stopped_status"), name, "stopped-status", f"{name} stopped status"
        )
        status_value, status_sha = sealed(
            node.get("quarantine_status"),
            name,
            "quarantine-status",
            f"{name} quarantine status",
        )
        monitor_value, monitor_sha = sealed(
            node.get("quarantine_monitor"),
            name,
            "network-quarantine-monitor",
            f"{name} network quarantine monitor",
        )
        post_value, post_sha = sealed(
            node.get("post_proof_quarantine_status"),
            name,
            "post-proof-quarantine-status",
            f"{name} post-proof quarantine status",
        )
        external_value, external_sha = sealed(
            node.get("external_quarantine_proof"),
            name,
            "external-quarantine-proof",
            f"{name} external quarantine proof",
        )
        cross_value, cross_sha = sealed(
            node.get("public_cross_proof"),
            name,
            "public-cross-proof",
            f"{name} public cross proof",
        )
        persisted_value, persisted_sha = sealed(
            node.get("persisted_head"), name, "persisted-head", f"{name} persisted head"
        )
        identity = (capture_id, name, freeze_sha)
        stopped_value = require_exact_object(
            stopped_value, stopped_fields, f"{name} stopped status value"
        )
        if (
            stopped_value.get("schema") != "arc.recovery.offline-stop-status.v1"
            or (
                stopped_value.get("capture_id"),
                stopped_value.get("node"),
                stopped_value.get("freeze_plan_sha256"),
            )
            != identity
            or stopped_value.get("validator_address") != frozen["validator_address"]
            or stopped_value.get("stake") != frozen["stake"]
            or stopped_value.get("stop_schema") != "arc.recovery.offline-stop.v4"
            or stopped_value.get("stopped") is not True
            or stopped_value.get("restart_fenced") is not True
        ):
            fail(f"maintenance evidence stopped status differs at {name}")
        for field in ("stop_complete_sha256", "stop_files_sha256"):
            require_hash(stopped_value.get(field), f"{name} stopped status {field}")

        checked_statuses: list[dict[str, Any]] = []
        for raw_status, label in ((status_value, "quarantine"), (post_value, "post-proof")):
            checked = require_exact_object(raw_status, status_fields, f"{name} {label} status value")
            if (
                checked.get("schema")
                != "arc.recovery.legacy-network-quarantine-status.v1"
                or (
                    checked.get("capture_id"),
                    checked.get("node"),
                    checked.get("freeze_plan_sha256"),
                )
                != identity
                or checked.get("active") is not True
                or checked.get("enabled") is not True
            ):
                fail(f"maintenance evidence {label} status differs at {name}")
            for field in (
                "receipt_sha256",
                "counter_snapshot_sha256",
                "owned_ruleset_stateless_sha256",
            ):
                require_hash(checked.get(field), f"{name} {label} status {field}")
            checked_statuses.append(checked)
        status_value, post_value = checked_statuses
        if status_value["receipt_sha256"] != post_value["receipt_sha256"]:
            fail(f"maintenance evidence quarantine receipt changed at {name}")

        monitor_value = require_exact_object(
            monitor_value, monitor_fields, f"{name} quarantine monitor value"
        )
        interpreter = require_exact_object(
            monitor_value.get("semantic_interpreter"),
            {
                "normalized_path", "sha256", "device", "inode", "uid", "gid",
                "mode", "nlink", "isolated", "environment",
            },
            f"{name} quarantine semantic interpreter",
        )
        interpreter_environment = require_exact_object(
            interpreter.get("environment"),
            {"PATH", "LC_ALL", "TZ", "PYTHONHASHSEED"},
            f"{name} quarantine semantic interpreter environment",
        )
        if (
            monitor_value.get("schema")
            != "arc.recovery.legacy-network-quarantine-monitor.v1"
            or (
                monitor_value.get("capture_id"), monitor_value.get("node"),
                monitor_value.get("freeze_plan_sha256"),
            )
            != identity
            or monitor_value.get("network_quarantine_receipt_sha256")
            != status_value["receipt_sha256"]
            or monitor_value.get("incident_latched") is not False
            or monitor_value.get("continuous_fail_closed") is not True
            or monitor_value.get("automatic_unfence") is not False
            or monitor_value.get("global_absence_claimed") is not False
            or not isinstance(interpreter.get("normalized_path"), str)
            or re.fullmatch(r"/usr/bin/python3\.[0-9]+", interpreter["normalized_path"])
            is None
            or require_hash(interpreter.get("sha256"), f"{name} interpreter sha256")
            != interpreter["sha256"]
            or any(
                isinstance(interpreter.get(field), bool)
                or not isinstance(interpreter.get(field), int)
                or interpreter[field] <= 0
                for field in ("device", "inode")
            )
            or (interpreter.get("uid"), interpreter.get("gid"), interpreter.get("mode"), interpreter.get("nlink"))
            != (0, 0, 0o755, 1)
            or interpreter.get("isolated") is not True
            or interpreter_environment
            != {
                "PATH": "/usr/bin:/bin", "LC_ALL": "C", "TZ": "UTC",
                "PYTHONHASHSEED": "0",
            }
        ):
            fail(f"maintenance evidence quarantine monitor differs at {name}")
        require_hash(
            monitor_value.get("monitor_contract_sha256"),
            f"{name} quarantine monitor contract",
        )
        for collection in ("file_sha256", "legacy_exec_start_pre"):
            rows = monitor_value.get(collection)
            if not isinstance(rows, dict) or not rows:
                fail(f"maintenance evidence {name} monitor {collection} is empty")
        if any(
            require_hash(value, f"{name} quarantine monitor file hash") != value
            for value in monitor_value["file_sha256"].values()
        ):
            fail(f"maintenance evidence {name} monitor file hash differs")
        unit = require_exact_object(
            monitor_value.get("unit"),
            {
                "name", "active", "enabled", "continuous_poll_interval_milliseconds",
                "full_loader_revalidation_interval_seconds",
            },
            f"{name} quarantine monitor unit",
        )
        if unit != {
            "name": "arc-legacy-maintenance-fence.service", "active": True,
            "enabled": True, "continuous_poll_interval_milliseconds": 100,
            "full_loader_revalidation_interval_seconds": 10,
        }:
            fail(f"maintenance evidence {name} monitor unit differs")

        external_value = require_exact_object(
            external_value, external_fields, f"{name} external proof value"
        )
        if (
            external_value.get("schema")
            != "arc.recovery.legacy-network-quarantine-external-proof.v1"
            or (
                external_value.get("capture_id"),
                external_value.get("node"),
                external_value.get("freeze_plan_sha256"),
            )
            != identity
            or external_value.get("host") != host
            or external_value.get("challenge") != challenge
            or external_value.get("before_status_sha256") != status_sha
            or external_value.get("network_quarantine_receipt_sha256")
            != status_value["receipt_sha256"]
            or external_value.get("ssh_status_reproved") is not True
            or external_value.get("global_absence_claimed") is not False
        ):
            fail(f"maintenance evidence external proof differs at {name}")
        external_started = _parse_utc_seconds(
            external_value.get("started_at"), f"{name} external proof started_at"
        )
        external_completed = _parse_utc_seconds(
            external_value.get("completed_at"), f"{name} external proof completed_at"
        )
        if external_started > external_completed:
            fail(f"maintenance evidence external proof timestamps are reversed at {name}")
        external_after = require_exact_object(
            external_value.get("after_status"), status_fields, f"{name} external after-status"
        )
        if (
            sha256_bytes(canonical_bytes(external_after))
            != external_value.get("after_status_sha256")
            or external_after.get("receipt_sha256") != status_value["receipt_sha256"]
            or (
                external_after.get("capture_id"),
                external_after.get("node"),
                external_after.get("freeze_plan_sha256"),
            )
            != identity
            or external_after.get("schema")
            != "arc.recovery.legacy-network-quarantine-status.v1"
            or external_after.get("active") is not True
            or external_after.get("enabled") is not True
        ):
            fail(f"maintenance evidence external after-status differs at {name}")
        targets = require_exact_object(
            external_value.get("targets"), {"tcp", "udp"}, f"{name} external targets"
        )
        if (
            not isinstance(targets.get("tcp"), list)
            or not isinstance(targets.get("udp"), list)
            or any(
                isinstance(port, bool) or not isinstance(port, int) or not 0 < port < 65536
                for port in targets["tcp"] + targets["udp"]
            )
            or targets["tcp"] != sorted(set(targets["tcp"]))
            or targets["udp"] != sorted(set(targets["udp"]))
        ):
            fail(f"maintenance evidence external target inventory differs at {name}")
        results = external_value.get("results")
        expected_result_coordinates = [
            ("tcp", port) for port in targets["tcp"]
        ] + [("udp", port) for port in targets["udp"]]
        if (
            not isinstance(results, list)
            or [(row.get("protocol"), row.get("port")) for row in results]
            != expected_result_coordinates
        ):
            fail(f"maintenance evidence external results differ at {name}")
        challenge_payload_sha = sha256_bytes(bytes.fromhex(challenge))
        for result in results:
            if result.get("protocol") == "tcp":
                result = require_exact_object(
                    result,
                    {"protocol", "port", "connect_succeeded", "connect_errno"},
                    f"{name} external TCP result",
                )
                errno = result.get("connect_errno")
                if (
                    result.get("connect_succeeded") is not False
                    or isinstance(errno, bool)
                    or not isinstance(errno, int)
                    or errno in {0, 61, 111}
                ):
                    fail(f"maintenance evidence TCP drop proof differs at {name}")
            else:
                result = require_exact_object(
                    result,
                    {"protocol", "port", "payload_sha256", "bytes_sent"},
                    f"{name} external UDP result",
                )
                if (
                    result.get("payload_sha256") != challenge_payload_sha
                    or result.get("bytes_sent") != 32
                ):
                    fail(f"maintenance evidence UDP challenge proof differs at {name}")
        deny_counter = require_exact_object(
            external_value.get("deny_counter"),
            {"comment", "before_packets", "after_packets", "minimum_delta"},
            f"{name} external deny counter",
        )
        before_packets = require_uint(
            deny_counter.get("before_packets"), f"{name} external before packets"
        )
        after_packets = require_uint(
            deny_counter.get("after_packets"), f"{name} external after packets"
        )
        minimum_delta = require_uint(
            deny_counter.get("minimum_delta"), f"{name} external minimum delta", positive=True
        )
        if (
            deny_counter.get("comment") != "arc-recovery:prerouting:iifname:deny"
            or minimum_delta != len(results)
            or after_packets - before_packets < minimum_delta
        ):
            fail(f"maintenance evidence external deny counter differs at {name}")

        cross_value = require_exact_object(
            cross_value, cross_fields, f"{name} public cross-proof value"
        )
        if (
            cross_value.get("schema")
            != "arc.recovery.legacy-network-quarantine-public-cross-proof.v1"
            or (
                cross_value.get("capture_id"),
                cross_value.get("node"),
                cross_value.get("freeze_plan_sha256"),
            )
            != identity
            or cross_value.get("challenge") != challenge
            or cross_value.get("network_quarantine_receipt_sha256")
            != status_value["receipt_sha256"]
            or cross_value.get("fenced_head_covers_public_info_after") is not True
            or cross_value.get("public_latest_hash_matches") is not True
            or cross_value.get("global_absence_claimed") is not False
        ):
            fail(f"maintenance evidence public cross-proof differs at {name}")
        embedded_status = require_exact_object(
            cross_value.get("quarantine_status"), status_fields, f"{name} embedded cross status"
        )
        if (
            sha256_bytes(canonical_bytes(embedded_status))
            != cross_value.get("quarantine_status_sha256")
            or embedded_status.get("receipt_sha256") != status_value["receipt_sha256"]
            or (
                embedded_status.get("capture_id"),
                embedded_status.get("node"),
                embedded_status.get("freeze_plan_sha256"),
            )
            != identity
            or embedded_status.get("schema")
            != "arc.recovery.legacy-network-quarantine-status.v1"
            or embedded_status.get("active") is not True
            or embedded_status.get("enabled") is not True
        ):
            fail(f"maintenance evidence embedded cross status differs at {name}")

        def head(raw: Any, label: str, *, response_hash: bool = False) -> dict[str, Any]:
            fields = {"height", "block_hash", "state_root"}
            if response_hash:
                fields.add("response_sha256")
            value_head = require_exact_object(raw, fields, label)
            require_uint(value_head.get("height"), f"{label} height")
            require_hash(value_head.get("block_hash"), f"{label} block hash")
            require_hash(value_head.get("state_root"), f"{label} state root")
            if response_hash:
                require_hash(value_head.get("response_sha256"), f"{label} response root")
            return value_head

        public_after = head(
            cross_value.get("public_info_after_block"),
            f"{name} public-after block",
            response_hash=True,
        )
        public_latest = head(
            cross_value.get("public_latest_block"),
            f"{name} public-latest block",
            response_hash=True,
        )
        fenced_head = head(cross_value.get("fenced_head"), f"{name} fenced head")
        if (
            public_after["height"] != public["info_after_height"]
            or public_latest["height"] != public["latest_block_height"]
            or public_latest["block_hash"] != public["latest_block_hash"]
            or fenced_head["height"] < public_after["height"]
        ):
            fail(f"maintenance evidence public tuple binding differs at {name}")

        persisted_value = require_exact_object(
            persisted_value, persisted_fields, f"{name} persisted-head value"
        )
        if (
            persisted_value.get("schema") != "arc.recovery.persisted-legacy-head.v1"
            or persisted_value.get("source_main_commit") != args.source_main_sha
            or (
                persisted_value.get("capture_id"),
                persisted_value.get("node"),
                persisted_value.get("freeze_plan_sha256"),
            )
            != identity
            or persisted_value.get("boot_id") != frozen["boot_id"]
            or persisted_value.get("network_quarantine_receipt_sha256")
            != status_value["receipt_sha256"]
            or persisted_value.get("export_status") != "EXPORTED_UNSIGNED"
            or persisted_value.get("rerun_reexecutes_export") is not True
            or persisted_value.get("writer_stopped") is not True
            or persisted_value.get("restart_barrier_active") is not True
            or persisted_value.get("network_quarantine_active") is not True
            or persisted_value.get("global_absence_claimed") is not False
        ):
            fail(f"maintenance evidence persisted-head identity differs at {name}")
        expected_tool_roots = {
            "inspector_binary_sha256": hash_secure(
                args.binary, "maintenance evidence inspector binary", executable=True
            )[0],
            "genesis_sha256": hash_secure(args.genesis, "maintenance evidence genesis")[0],
            "validator_public_keys_sha256": hash_secure(
                args.validator_public_keys, "maintenance evidence validator keys"
            )[0],
            "legacy_validator_set_sha256": hash_secure(
                args.legacy_validator_set, "maintenance evidence legacy validator set"
            )[0],
        }
        for field, expected in expected_tool_roots.items():
            if persisted_value.get(field) != expected:
                fail(f"maintenance evidence persisted-head {field} differs at {name}")
        for field in (
            "stop_complete_sha256",
            "stop_files_sha256",
            "capture_complete_sha256",
            "capture_files_sha256",
            "capture_source_sha256",
            "source_data_index_sha256",
            "state_wal_sha256",
            "snapshot_sha256",
            "export_summary_sha256",
            "inspect_summary_sha256",
            "wal_boundary_sha256",
            "candidate_checkpoint_sha256",
        ):
            require_hash(persisted_value.get(field), f"{name} persisted-head {field}")
        if (
            persisted_value["stop_complete_sha256"]
            != stopped_value["stop_complete_sha256"]
            or persisted_value["stop_files_sha256"] != stopped_value["stop_files_sha256"]
        ):
            fail(f"maintenance evidence persisted/stopped roots differ at {name}")
        for field in ("state_wal_size", "snapshot_size", "candidate_checkpoint_size"):
            require_uint(persisted_value.get(field), f"{name} persisted-head {field}", positive=True)
        staged_contract = require_exact_object(
            persisted_value.get("staged_file_contract"),
            {"state_wal", "snapshot", "ephemeral_inode_receipted"},
            f"{name} persisted staged-file contract",
        )
        if staged_contract.get("ephemeral_inode_receipted") is not False:
            fail(f"maintenance evidence persisted file contract is ephemeral at {name}")
        for key, hash_field, size_field in (
            ("state_wal", "state_wal_sha256", "state_wal_size"),
            ("snapshot", "snapshot_sha256", "snapshot_size"),
        ):
            row = require_exact_object(
                staged_contract.get(key),
                {"sha256", "size", "mode", "uid", "gid", "nlink"},
                f"{name} persisted staged {key}",
            )
            if row != {
                "sha256": persisted_value[hash_field],
                "size": persisted_value[size_field],
                "mode": 0o100400,
                "uid": 0,
                "gid": 0,
                "nlink": 1,
            }:
                fail(f"maintenance evidence persisted staged {key} identity differs at {name}")
        export_contract = require_exact_object(
            persisted_value.get("export_contract"),
            {
                "binary_path",
                "exit_code",
                "source_consensus_round",
                "created_at_unix_ms",
                "recovery_epoch",
                "validator_set_id",
                "allow_unbound_legacy_wal",
                "read_only",
            },
            f"{name} persisted export contract",
        )
        if export_contract != {
            "binary_path": "/proc/self/fd/8",
            "exit_code": 0,
            "source_consensus_round": 0,
            "created_at_unix_ms": 0,
            "recovery_epoch": 1,
            "validator_set_id": 1,
            "allow_unbound_legacy_wal": True,
            "read_only": True,
        }:
            fail(f"maintenance evidence persisted export contract differs at {name}")
        _parse_utc_seconds(
            persisted_value.get("completed_at"), f"{name} persisted-head completed_at"
        )
        persisted_head = head(persisted_value.get("head"), f"{name} persisted head tuple")
        if persisted_head["height"] < fenced_head["height"]:
            fail(f"maintenance evidence persisted head precedes fenced head at {name}")
        normalized_nodes.append(
            {
                "node": name,
                "host": host,
                "stopped_status": {"value": stopped_value, "sha256": stopped_sha},
                "quarantine_status": {"value": status_value, "sha256": status_sha},
                "quarantine_monitor": {
                    "value": monitor_value,
                    "sha256": monitor_sha,
                },
                "post_proof_quarantine_status": {"value": post_value, "sha256": post_sha},
                "external_quarantine_proof": {
                    "value": external_value,
                    "sha256": external_sha,
                },
                "public_cross_proof": {"value": cross_value, "sha256": cross_sha},
                "persisted_head": {"value": persisted_value, "sha256": persisted_sha},
            }
        )

    inventory = bundle.get("object_inventory")
    if inventory != inventory_expected:
        fail("legacy maintenance evidence inventory differs from its exact ordered objects")
    inventory_root = sha256_bytes(
        canonical_bytes(
            {
                "schema": "arc.recovery.legacy-maintenance-evidence-inventory.v1",
                "objects": inventory_expected,
            }
        )
    )
    if bundle.get("aggregate_root_sha256") != inventory_root:
        fail("legacy maintenance evidence aggregate root is not reproducible")
    normalized = copy.deepcopy(bundle)
    normalized["authenticated_prefence_height_cross_proof"] = {
        "value": authenticated,
        "sha256": authenticated_sha,
    }
    normalized["quarantine_stability_proof"] = {
        "value": stability,
        "sha256": stability_sha,
    }
    normalized["nodes"] = normalized_nodes
    return digest, details.st_size, normalized, payload, sidecar


def validate_legacy_maintenance_boundary(
    args: argparse.Namespace,
    freeze: Mapping[str, Any],
    freeze_sha: str,
    height_receipt: Mapping[str, Any],
    height_receipt_sha: str,
    evidence_bundle: Mapping[str, Any],
    evidence_bundle_sha: str,
) -> tuple[str, int, dict[str, Any], bytes, bytes]:
    """Validate the create-only ceiling that separates preserved legacy forks from v3."""

    payload, details = read_secure(
        args.legacy_maintenance_boundary,
        label="legacy maintenance boundary",
        maximum_bytes=16 * 1024 * 1024,
        exact_mode=0o400,
    )
    sidecar_path = args.legacy_maintenance_boundary.with_name(
        args.legacy_maintenance_boundary.name + ".sha256"
    )
    sidecar, _ = read_secure(
        sidecar_path,
        label="legacy maintenance boundary checksum",
        maximum_bytes=512,
        exact_mode=0o400,
    )
    digest = sha256_bytes(payload)
    if sidecar != f"{digest}  {args.legacy_maintenance_boundary.name}\n".encode("ascii"):
        fail("legacy maintenance boundary checksum does not bind the exact receipt bytes")
    value = decode_json(payload, "legacy maintenance boundary")
    if not isinstance(value, dict) or canonical_bytes(value) != payload:
        fail("legacy maintenance boundary must be canonical JSON")
    boundary = require_exact_object(
        value,
        {
            "schema",
            "source_main_commit",
            "freeze_plan_sha256",
            "capture_id",
            "first_quarantine_started_at",
            "all_controlled_stopped_at",
            "created_at",
            "official_origin_scope",
            "legacy_public_height_receipt",
            "authenticated_prefence_height_cross_proof_sha256",
            "legacy_maintenance_evidence_bundle_sha256",
            "network_quarantine_stability_proof_sha256",
            "network_quarantine_challenge",
            "tools",
            "nodes",
            "evidence_heights",
            "observed_cutoff_height",
            "continuity_safety_margin",
            "continuity_safety_margin_policy",
            "legacy_public_max_height",
            "global_absence_claimed",
            "reopening_policy",
            "late_fork_circuit",
            "threat_model",
        },
        "legacy maintenance boundary",
    )
    capture_id = rollout.capture_id_for_freeze_plan_hash(freeze_sha)
    expected_identity = {
        "schema": "arc.recovery.legacy-maintenance-boundary.v1",
        "source_main_commit": args.source_main_sha,
        "freeze_plan_sha256": freeze_sha,
        "capture_id": capture_id,
        "global_absence_claimed": False,
    }
    for field, expected in expected_identity.items():
        if boundary.get(field) != expected:
            fail(f"legacy maintenance boundary {field} differs from the sealed recovery")

    first = _parse_utc_seconds(
        boundary.get("first_quarantine_started_at"),
        "legacy maintenance first quarantine",
    )
    stopped = _parse_utc_seconds(
        boundary.get("all_controlled_stopped_at"),
        "legacy maintenance all-controlled-stopped",
    )
    created = _parse_utc_seconds(
        boundary.get("created_at"), "legacy maintenance boundary created_at"
    )
    if not first <= stopped <= created:
        fail("legacy maintenance boundary timestamps are not ordered")

    expected_origins = [
        {"node": name, "host": host, "origin": origin}
        for name, host, origin in legacy_height.FLEET
    ]
    origin_scope = require_exact_object(
        boundary.get("official_origin_scope"),
        {"global_absence_claimed", "origins"},
        "legacy maintenance official origin scope",
    )
    if origin_scope != {
        "global_absence_claimed": False,
        "origins": expected_origins,
    }:
        fail("legacy maintenance boundary official origins differ from the exact six")

    public_root = require_exact_object(
        boundary.get("legacy_public_height_receipt"),
        {"schema", "sha256", "completed_at", "observed_max_height"},
        "legacy maintenance public-height root",
    )
    expected_public_root = {
        "schema": legacy_height.SCHEMA,
        "sha256": height_receipt_sha,
        "completed_at": height_receipt["completed_at"],
        "observed_max_height": height_receipt["legacy_public_max_height"],
    }
    if public_root != expected_public_root:
        fail("legacy maintenance boundary does not bind the exact public-height receipt")
    require_hash(
        boundary.get("authenticated_prefence_height_cross_proof_sha256"),
        "legacy maintenance authenticated pre-fence proof root",
    )
    if boundary.get("legacy_maintenance_evidence_bundle_sha256") != evidence_bundle_sha:
        fail("legacy maintenance boundary does not bind the exact evidence bundle")
    if (
        boundary.get("authenticated_prefence_height_cross_proof_sha256")
        != evidence_bundle["authenticated_prefence_height_cross_proof"]["sha256"]
        or boundary.get("network_quarantine_stability_proof_sha256")
        != evidence_bundle["quarantine_stability_proof"]["sha256"]
        or boundary.get("network_quarantine_challenge") != evidence_bundle["challenge"]
    ):
        fail("legacy maintenance boundary evidence roots differ from the evidence bundle")
    require_hash(
        boundary.get("network_quarantine_challenge"),
        "legacy maintenance quarantine challenge",
    )

    tools = require_exact_object(
        boundary.get("tools"),
        {
            "remote_helper_sha256",
            "inspector_binary_sha256",
            "genesis_sha256",
            "validator_public_keys_sha256",
            "legacy_validator_set_sha256",
            "orchestrator_sha256",
            "rollout_tool_sha256",
            "rollout_schema_sha256",
        },
        "legacy maintenance tool roots",
    )
    expected_tools = {
        "remote_helper_sha256": freeze["remote_helper_sha256"],
        "inspector_binary_sha256": hash_secure(args.binary, "boundary inspector binary", executable=True)[0],
        "genesis_sha256": hash_secure(args.genesis, "boundary genesis")[0],
        "validator_public_keys_sha256": hash_secure(
            args.validator_public_keys, "boundary validator public keys"
        )[0],
        "legacy_validator_set_sha256": hash_secure(
            args.legacy_validator_set, "boundary legacy validator set"
        )[0],
        "orchestrator_sha256": freeze["orchestrator_sha256"],
        "rollout_tool_sha256": freeze["rollout_tool_sha256"],
        "rollout_schema_sha256": freeze["rollout_schema_sha256"],
    }
    if tools != expected_tools:
        fail("legacy maintenance boundary tool/config roots differ from staged protected inputs")

    if boundary.get("continuity_safety_margin") != 128:
        fail("legacy maintenance continuity margin must be exactly 128 blocks")
    margin_policy = {
        "prune_depth": 100,
        "commit_rule_rounds": 2,
        "operational_headroom": 26,
        "cryptographic_global_absence_proof": False,
    }
    if boundary.get("continuity_safety_margin_policy") != margin_policy:
        fail("legacy maintenance continuity-margin policy differs")
    if sum(margin_policy[key] for key in ("prune_depth", "commit_rule_rounds", "operational_headroom")) != 128:
        fail("legacy maintenance continuity-margin arithmetic is internally invalid")
    if boundary.get("reopening_policy") != {
        "required_validator_count": 6,
        "height_relation": "strictly-greater-than-legacy_public_max_height",
        "required_equal_fields": ["block_hash", "state_root"],
    }:
        fail("legacy maintenance reopening policy differs")
    if boundary.get("late_fork_circuit") != {
        "monitor_scope": "retired-and-community-legacy-sources",
        "trigger": "self-consistent-legacy-fork-candidate-above-observed-cutoff-height",
        "action": "enter-maintenance-preserve-and-offline-validate",
        "rewrite_v3_history_allowed": False,
    }:
        fail("legacy maintenance late-fork circuit differs")
    if boundary.get("threat_model") != {
        "trusted_host_root_required": True,
        "sealed_reviewed_legacy_binary_non_adversarial": True,
        "quarantine_purpose": "operational-network-isolation",
        "hostile_root_containment_claimed": False,
    }:
        fail("legacy maintenance quarantine threat model differs")

    raw_nodes = boundary.get("nodes")
    if not isinstance(raw_nodes, list) or len(raw_nodes) != len(FLEET):
        fail("legacy maintenance boundary must contain exactly six ordered nodes")
    public_rows = height_receipt["origins"]
    normalized_nodes: list[dict[str, Any]] = []
    tuple_fields = {"height", "block_hash", "state_root"}
    wrapper_fields = {"tuple", "evidence_sha256"}

    def exact_head(raw: Any, label: str) -> dict[str, Any]:
        head = require_exact_object(raw, tuple_fields, label)
        return {
            "height": require_uint(head.get("height"), f"{label} height"),
            "block_hash": require_hash(head.get("block_hash"), f"{label} block hash"),
            "state_root": require_hash(head.get("state_root"), f"{label} state root"),
        }

    def exact_observation(raw: Any, label: str) -> dict[str, Any]:
        observation = require_exact_object(raw, wrapper_fields, label)
        require_hash(observation.get("evidence_sha256"), f"{label} evidence root")
        head = exact_head(observation.get("tuple"), f"{label} tuple")
        return {"tuple": head, "evidence_sha256": observation["evidence_sha256"]}

    node_fields = {
        "node",
        "host",
        "origin",
        "public_observation",
        "authenticated_prefence_proof_sha256",
        "network_quarantine_receipt_sha256",
        "quarantine_status_sha256",
        "post_proof_quarantine_status_sha256",
        "external_quarantine_proof_sha256",
        "public_cross_proof_sha256",
        "initial_post_quarantine_head",
        "post_quarantine_head",
        "final_persisted_head",
    }
    for index, ((name, host), public, raw_node, bundle_node, authenticated_node) in enumerate(
        zip(
            FLEET,
            public_rows,
            raw_nodes,
            evidence_bundle["nodes"],
            evidence_bundle["authenticated_prefence_height_cross_proof"]["value"]["nodes"],
        )
    ):
        node = require_exact_object(raw_node, node_fields, f"legacy maintenance node {index}")
        if (node.get("node"), node.get("host"), node.get("origin")) != (
            name,
            host,
            public["origin"],
        ):
            fail(f"legacy maintenance node topology differs at {name}")
        for field in (
            "authenticated_prefence_proof_sha256",
            "network_quarantine_receipt_sha256",
            "quarantine_status_sha256",
            "post_proof_quarantine_status_sha256",
            "external_quarantine_proof_sha256",
            "public_cross_proof_sha256",
        ):
            require_hash(node.get(field), f"legacy maintenance {name} {field}")
        public_observation = exact_observation(
            node.get("public_observation"), f"legacy maintenance {name} public observation"
        )
        initial = exact_observation(
            node.get("initial_post_quarantine_head"),
            f"legacy maintenance {name} initial post-quarantine head",
        )
        later = exact_observation(
            node.get("post_quarantine_head"),
            f"legacy maintenance {name} post-quarantine head",
        )
        persisted = exact_observation(
            node.get("final_persisted_head"),
            f"legacy maintenance {name} final persisted head",
        )
        bundle_status = bundle_node["quarantine_status"]
        bundle_post = bundle_node["post_proof_quarantine_status"]
        bundle_external = bundle_node["external_quarantine_proof"]
        bundle_cross = bundle_node["public_cross_proof"]
        bundle_persisted = bundle_node["persisted_head"]
        expected_roots = {
            "authenticated_prefence_proof_sha256": authenticated_node["proof_sha256"],
            "network_quarantine_receipt_sha256": bundle_status["value"]["receipt_sha256"],
            "quarantine_status_sha256": bundle_status["sha256"],
            "post_proof_quarantine_status_sha256": bundle_post["sha256"],
            "external_quarantine_proof_sha256": bundle_external["sha256"],
            "public_cross_proof_sha256": bundle_cross["sha256"],
        }
        if any(node.get(field) != expected for field, expected in expected_roots.items()):
            fail(f"legacy maintenance {name} roots differ from the evidence bundle")
        cross_value = bundle_cross["value"]
        expected_public_tuple = {
            field: cross_value["public_info_after_block"][field]
            for field in ("height", "block_hash", "state_root")
        }
        status_head = bundle_status["value"]["loopback_head"]
        expected_initial_tuple = {
            "height": status_head.get("latest_height"),
            "block_hash": status_head.get("block_hash"),
            "state_root": status_head.get("state_root"),
        }
        expected_later_tuple = copy.deepcopy(cross_value["fenced_head"])
        expected_persisted_tuple = copy.deepcopy(bundle_persisted["value"]["head"])
        if (
            public_observation["tuple"] != expected_public_tuple
            or initial["tuple"] != expected_initial_tuple
            or later["tuple"] != expected_later_tuple
            or persisted["tuple"] != expected_persisted_tuple
            or persisted["evidence_sha256"] != bundle_persisted["sha256"]
        ):
            fail(f"legacy maintenance {name} tuples differ from the evidence bundle")
        if public_observation["tuple"]["height"] != public["info_after_height"]:
            fail(f"legacy maintenance {name} public tuple height differs")
        if public_observation["evidence_sha256"] != node["public_cross_proof_sha256"]:
            fail(f"legacy maintenance {name} public tuple evidence differs")
        if initial["evidence_sha256"] != node["quarantine_status_sha256"]:
            fail(f"legacy maintenance {name} initial quarantine evidence differs")
        if later["evidence_sha256"] != node["public_cross_proof_sha256"]:
            fail(f"legacy maintenance {name} later quarantine evidence differs")
        floor = max(
            public_observation["tuple"]["height"],
            initial["tuple"]["height"],
            later["tuple"]["height"],
        )
        if persisted["tuple"]["height"] < floor:
            fail(f"legacy maintenance {name} persisted head precedes observed legacy evidence")
        for prior in (initial["tuple"], later["tuple"]):
            if persisted["tuple"]["height"] == prior["height"] and persisted["tuple"] != prior:
                fail(f"legacy maintenance {name} same-height persisted tuple disagrees")
        normalized_nodes.append(copy.deepcopy(node))

    raw_heights = boundary.get("evidence_heights")
    labels = (
        "public_info_before",
        "public_latest",
        "public_info_after",
        "authenticated_info_before",
        "authenticated_latest",
        "authenticated_info_after",
        "authenticated_conservative_floor",
        "initial_post_quarantine_head",
        "public_cross_info_after",
        "post_quarantine_head",
        "quarantine_stability_sample_0",
        "quarantine_stability_sample_1",
        "final_persisted_head",
    )
    if not isinstance(raw_heights, list) or len(raw_heights) != len(FLEET) * len(labels):
        fail("legacy maintenance evidence-height ledger is not exact six-by-thirteen")
    normalized_heights: list[dict[str, Any]] = []
    height_fields = {"node", "label", "height", "evidence_sha256"}
    for index, raw_height in enumerate(raw_heights):
        row = require_exact_object(
            raw_height, height_fields, f"legacy maintenance evidence height {index}"
        )
        node_index, label_index = divmod(index, len(labels))
        name = FLEET[node_index][0]
        if (row.get("node"), row.get("label")) != (name, labels[label_index]):
            fail("legacy maintenance evidence-height order differs")
        normalized_heights.append(
            {
                "node": name,
                "label": labels[label_index],
                "height": require_uint(
                    row.get("height"), f"legacy maintenance {name}/{labels[label_index]} height"
                ),
                "evidence_sha256": require_hash(
                    row.get("evidence_sha256"),
                    f"legacy maintenance {name}/{labels[label_index]} evidence root",
                ),
            }
        )

    for node_index, (node, public) in enumerate(zip(normalized_nodes, public_rows)):
        by_label = {
            row["label"]: row for row in normalized_heights[
                node_index * len(labels) : (node_index + 1) * len(labels)
            ]
        }
        expected_public = {
            "public_info_before": public["info_before_height"],
            "public_latest": public["latest_block_height"],
            "public_info_after": public["info_after_height"],
            "public_cross_info_after": node["public_observation"]["tuple"]["height"],
            "initial_post_quarantine_head": node["initial_post_quarantine_head"]["tuple"]["height"],
            "post_quarantine_head": node["post_quarantine_head"]["tuple"]["height"],
            "quarantine_stability_sample_0": evidence_bundle["quarantine_stability_proof"][
                "value"
            ]["nodes"][node_index]["samples"][0]["value"]["head"]["height"],
            "quarantine_stability_sample_1": evidence_bundle["quarantine_stability_proof"][
                "value"
            ]["nodes"][node_index]["samples"][1]["value"]["head"]["height"],
            "final_persisted_head": node["final_persisted_head"]["tuple"]["height"],
        }
        for label, expected in expected_public.items():
            if by_label[label]["height"] != expected:
                fail(f"legacy maintenance {node['node']} evidence-height tuple differs: {label}")
        if any(
            by_label[label]["evidence_sha256"] != height_receipt_sha
            for label in ("public_info_before", "public_latest", "public_info_after")
        ):
            fail(f"legacy maintenance {node['node']} public evidence roots differ")
        if by_label["initial_post_quarantine_head"]["evidence_sha256"] != node[
            "quarantine_status_sha256"
        ]:
            fail(f"legacy maintenance {node['node']} initial evidence-height root differs")
        for label in ("public_cross_info_after", "post_quarantine_head"):
            if by_label[label]["evidence_sha256"] != node["public_cross_proof_sha256"]:
                fail(f"legacy maintenance {node['node']} cross-proof evidence root differs")
        stability_samples = evidence_bundle["quarantine_stability_proof"]["value"]["nodes"][
            node_index
        ]["samples"]
        for sample_index in (0, 1):
            label = f"quarantine_stability_sample_{sample_index}"
            if by_label[label]["evidence_sha256"] != stability_samples[sample_index]["sha256"]:
                fail(
                    f"legacy maintenance {node['node']} stability evidence root differs: {label}"
                )
        if by_label["final_persisted_head"]["evidence_sha256"] != node[
            "final_persisted_head"
        ]["evidence_sha256"]:
            fail(f"legacy maintenance {node['node']} persisted evidence root differs")

    cutoff = require_uint(
        boundary.get("observed_cutoff_height"), "legacy maintenance observed cutoff"
    )
    if cutoff != max(row["height"] for row in normalized_heights):
        fail("legacy maintenance cutoff is not the maximum enumerated evidence height")
    if cutoff < height_receipt["legacy_public_max_height"]:
        fail("legacy maintenance cutoff is below the raw public observation ceiling")
    legacy_max = require_uint(
        boundary.get("legacy_public_max_height"), "legacy maintenance public maximum"
    )
    if legacy_max != cutoff + 128:
        fail("legacy maintenance public maximum is not cutoff plus 128")
    return digest, details.st_size, copy.deepcopy(boundary), payload, sidecar


def validate_legacy_late_fork_source_set(
    args: argparse.Namespace,
    *,
    boundary: Mapping[str, Any],
    boundary_sha: str,
) -> tuple[dict[str, Any], str, bytes, bytes, str]:
    """Bind the online fail-closed monitor to the exact legacy cutoff and tool."""

    tool_sha, _tool_size = hash_secure(
        args.legacy_late_fork_interlock_tool,
        "legacy late-fork interlock tool",
        executable=True,
    )
    payload, _details = read_secure(
        args.legacy_late_fork_source_set,
        label="legacy late-fork source set",
        maximum_bytes=4 * 1024 * 1024,
        exact_mode=0o400,
    )
    source_sha = sha256_bytes(payload)
    sidecar_path = args.legacy_late_fork_source_set.with_name(
        args.legacy_late_fork_source_set.name + ".sha256"
    )
    sidecar, _ = read_secure(
        sidecar_path,
        label="legacy late-fork source-set checksum",
        maximum_bytes=512,
        exact_mode=0o400,
    )
    if sidecar != f"{source_sha}  {args.legacy_late_fork_source_set.name}\n".encode(
        "ascii"
    ):
        fail("legacy late-fork source-set checksum does not bind the exact bytes")
    try:
        source_set, loaded_sha = late_fork.load_source_set(
            args.legacy_late_fork_source_set,
            expected_sha256=source_sha,
            expected_boundary_sha256=boundary_sha,
            expected_tool_sha256=tool_sha,
        )
    except late_fork.InterlockError as error:
        fail(f"legacy late-fork source set failed validation: {error}")
    if loaded_sha != source_sha:
        fail("legacy late-fork source-set validator returned a different root")
    if (
        source_set.get("source_main_commit") != args.source_main_sha
        or source_set.get("observed_cutoff_height") != boundary["observed_cutoff_height"]
        or source_set.get("boundary_sha256") != boundary_sha
        or source_set.get("validation_tool_sha256") != tool_sha
    ):
        fail("legacy late-fork source set differs from the selected boundary/tool")
    return copy.deepcopy(source_set), source_sha, payload, sidecar, tool_sha


def validate_offline_stop_evidence(
    args: argparse.Namespace,
    freeze: Mapping[str, Any],
    freeze_sha: str,
    freeze_sidecar_sha: str,
    boundary: Mapping[str, Any],
    boundary_sha: str,
    evidence_bundle: Mapping[str, Any],
    evidence_bundle_sha: str,
    height_receipt: Mapping[str, Any],
    height_receipt_sha: str,
) -> tuple[str, int, dict[str, Any], bytes, bytes]:
    payload, details = read_secure(
        args.offline_stop_evidence,
        label="offline-stop fleet evidence",
        maximum_bytes=16 * 1024 * 1024,
        exact_mode=0o400,
    )
    sidecar_path = args.offline_stop_evidence.with_name(
        args.offline_stop_evidence.name + ".sha256"
    )
    sidecar, _ = read_secure(
        sidecar_path,
        label="offline-stop fleet evidence checksum",
        maximum_bytes=512,
        exact_mode=0o400,
    )
    digest = sha256_bytes(payload)
    if sidecar != f"{digest}  {args.offline_stop_evidence.name}\n".encode("ascii"):
        fail("offline-stop evidence checksum does not bind the exact receipt bytes")
    value = decode_json(payload, "offline-stop fleet evidence")
    if not isinstance(value, dict) or canonical_bytes(value) != payload:
        fail("offline-stop fleet evidence must be canonical JSON")
    receipt = require_exact_object(
        value,
        {
            "schema",
            "source_main_commit",
            "freeze_plan_sha256",
            "freeze_plan_sidecar_sha256",
            "capture_id",
            "remote_helper_sha256",
            "remote_helper_path",
            "first_quarantine_started_at",
            "all_controlled_stopped_at",
            "legacy_height_cross_proof",
            "legacy_maintenance_boundary",
            "legacy_maintenance_boundary_sha256",
            "legacy_maintenance_evidence_bundle_sha256",
            "nodes",
        },
        "offline-stop fleet evidence",
    )
    capture_id = rollout.capture_id_for_freeze_plan_hash(freeze_sha)
    expected_helper_sha = freeze["remote_helper_sha256"]
    expected_helper_path = (
        f"/root/.arc-recovery-helpers/{expected_helper_sha}/archive-node.sh"
    )
    expected_top = {
        "schema": "arc.validator-vault.offline-stop-evidence.v2",
        "source_main_commit": args.source_main_sha,
        "freeze_plan_sha256": freeze_sha,
        "freeze_plan_sidecar_sha256": freeze_sidecar_sha,
        "capture_id": capture_id,
        "remote_helper_sha256": expected_helper_sha,
        "remote_helper_path": expected_helper_path,
    }
    for field, expected in expected_top.items():
        if receipt.get(field) != expected:
            fail(f"offline-stop evidence {field} differs from the sealed source/freeze")
    if (
        receipt.get("first_quarantine_started_at")
        != boundary["first_quarantine_started_at"]
        or receipt.get("all_controlled_stopped_at")
        != boundary["all_controlled_stopped_at"]
    ):
        fail("offline-stop evidence maintenance timestamps differ from the boundary")
    if receipt.get("legacy_maintenance_boundary_sha256") != boundary_sha:
        fail("offline-stop evidence does not bind the standalone maintenance boundary hash")
    if receipt.get("legacy_maintenance_boundary") != boundary:
        fail("offline-stop evidence embedded maintenance boundary differs byte-for-byte")
    if (
        receipt.get("legacy_maintenance_evidence_bundle_sha256") != evidence_bundle_sha
        or receipt.get("legacy_maintenance_evidence_bundle_sha256")
        != boundary["legacy_maintenance_evidence_bundle_sha256"]
    ):
        fail("offline-stop evidence does not bind the exact maintenance evidence bundle")

    cross = require_exact_object(
        receipt.get("legacy_height_cross_proof"),
        {
            "schema",
            "source_main_commit",
            "freeze_plan_sha256",
            "capture_id",
            "legacy_public_height_receipt_sha256",
            "challenge",
            "started_at",
            "completed_at",
            "conservative_height_floor",
            "nodes",
        },
        "offline-stop authenticated legacy-height fleet proof",
    )
    if (
        cross.get("schema") != "arc.recovery.authenticated-legacy-height-fleet.v1"
        or cross.get("source_main_commit") != args.source_main_sha
        or cross.get("freeze_plan_sha256") != freeze_sha
        or cross.get("capture_id") != capture_id
        or cross.get("legacy_public_height_receipt_sha256") != height_receipt_sha
        or cross.get("challenge") != boundary["network_quarantine_challenge"]
        or sha256_bytes(canonical_bytes(cross))
        != boundary["authenticated_prefence_height_cross_proof_sha256"]
        or cross
        != evidence_bundle["authenticated_prefence_height_cross_proof"]["value"]
    ):
        fail("offline-stop authenticated legacy-height fleet root differs")
    cross_started = _parse_utc_seconds(
        cross.get("started_at"), "offline-stop authenticated-height started_at"
    )
    cross_completed = _parse_utc_seconds(
        cross.get("completed_at"), "offline-stop authenticated-height completed_at"
    )
    if cross_started > cross_completed:
        fail("offline-stop authenticated legacy-height timestamps are reversed")
    cross_nodes = cross.get("nodes")
    if not isinstance(cross_nodes, list) or len(cross_nodes) != len(FLEET):
        fail("offline-stop authenticated legacy-height proof must contain six nodes")
    proof_fields = {
        "schema",
        "capture_id",
        "node",
        "freeze_plan_sha256",
        "challenge",
        "rpc_origin",
        "writer_pid",
        "writer_start_ticks",
        "boot_id",
        "executable_sha256",
        "argv_sha256",
        "started_at",
        "completed_at",
        "public_info_before_height",
        "public_latest_block_height",
        "public_info_after_height",
        "public_latest_block_hash",
        "authenticated_info_before_height",
        "authenticated_latest_block_height",
        "authenticated_info_after_height",
        "authenticated_latest_block_hash",
        "authenticated_info_before_body_sha256",
        "authenticated_latest_block_body_sha256",
        "authenticated_info_after_body_sha256",
        "conservative_height_floor",
    }
    proof_roots: dict[str, str] = {}
    proof_values: dict[str, dict[str, Any]] = {}
    proof_started: list[str] = []
    proof_completed: list[str] = []
    for index, ((name, host), frozen, public, raw_cross) in enumerate(
        zip(FLEET, freeze["nodes"], height_receipt["origins"], cross_nodes)
    ):
        cross_row = require_exact_object(
            raw_cross,
            {"node", "host", "proof", "proof_sha256"},
            f"offline-stop authenticated-height node {index}",
        )
        if (cross_row.get("node"), cross_row.get("host")) != (name, host):
            fail(f"offline-stop authenticated-height topology differs at {name}")
        proof = require_exact_object(
            cross_row.get("proof"), proof_fields, f"offline-stop {name} authenticated proof"
        )
        proof_sha = require_hash(
            cross_row.get("proof_sha256"), f"offline-stop {name} authenticated proof root"
        )
        if sha256_bytes(canonical_bytes(proof)) != proof_sha:
            fail(f"offline-stop {name} authenticated proof hash is not reproducible")
        expected_proof = {
            "schema": "arc.recovery.authenticated-legacy-height-bracket.v1",
            "capture_id": capture_id,
            "node": name,
            "freeze_plan_sha256": freeze_sha,
            "challenge": cross["challenge"],
            "rpc_origin": frozen["rpc_origin"],
            "writer_pid": frozen["writer_pid"],
            "writer_start_ticks": frozen["writer_start_ticks"],
            "boot_id": frozen["boot_id"],
            "executable_sha256": frozen["executable_sha256"],
            "argv_sha256": frozen["argv_sha256"],
            "public_info_before_height": public["info_before_height"],
            "public_latest_block_height": public["latest_block_height"],
            "public_info_after_height": public["info_after_height"],
            "public_latest_block_hash": public["latest_block_hash"],
        }
        if any(proof.get(field) != expected for field, expected in expected_proof.items()):
            fail(f"offline-stop {name} authenticated proof binding differs")
        before = require_uint(
            proof.get("authenticated_info_before_height"),
            f"offline-stop {name} authenticated before height",
        )
        latest = require_uint(
            proof.get("authenticated_latest_block_height"),
            f"offline-stop {name} authenticated latest height",
        )
        after = require_uint(
            proof.get("authenticated_info_after_height"),
            f"offline-stop {name} authenticated after height",
        )
        floor = require_uint(
            proof.get("conservative_height_floor"),
            f"offline-stop {name} conservative height floor",
        )
        if not before <= latest <= after or floor != max(public["info_after_height"], after):
            fail(f"offline-stop {name} authenticated height bracket differs")
        for field in (
            "authenticated_latest_block_hash",
            "authenticated_info_before_body_sha256",
            "authenticated_latest_block_body_sha256",
            "authenticated_info_after_body_sha256",
        ):
            require_hash(proof.get(field), f"offline-stop {name} {field}")
        if latest == public["latest_block_height"] and proof[
            "authenticated_latest_block_hash"
        ] != public["latest_block_hash"]:
            fail(f"offline-stop {name} authenticated/public same-height block hash differs")
        proof_started.append(proof.get("started_at"))
        proof_completed.append(proof.get("completed_at"))
        _parse_utc_seconds(proof["started_at"], f"offline-stop {name} proof started_at")
        _parse_utc_seconds(proof["completed_at"], f"offline-stop {name} proof completed_at")
        if proof["started_at"] > proof["completed_at"]:
            fail(f"offline-stop {name} authenticated proof timestamps are reversed")
        proof_roots[name] = proof_sha
        proof_values[name] = copy.deepcopy(proof)
    if cross["started_at"] != min(proof_started) or cross["completed_at"] != max(proof_completed):
        fail("offline-stop authenticated fleet timestamps do not bracket its six proofs")
    expected_floor = max(
        proof["conservative_height_floor"] for proof in proof_values.values()
    )
    if cross.get("conservative_height_floor") != expected_floor:
        fail("offline-stop authenticated fleet conservative height floor differs")

    boundary_height_rows = boundary["evidence_heights"]
    for name in proof_values:
        indexed = {
            row["label"]: row for row in boundary_height_rows if row["node"] == name
        }
        proof = proof_values[name]
        expected_authenticated = {
            "authenticated_info_before": proof["authenticated_info_before_height"],
            "authenticated_latest": proof["authenticated_latest_block_height"],
            "authenticated_info_after": proof["authenticated_info_after_height"],
            "authenticated_conservative_floor": proof["conservative_height_floor"],
        }
        for label, expected_height in expected_authenticated.items():
            row = indexed.get(label)
            if row is None or row.get("height") != expected_height or row.get(
                "evidence_sha256"
            ) != proof_roots[name]:
                fail(f"offline-stop {name} boundary authenticated height differs: {label}")
    rows = receipt["nodes"]
    if not isinstance(rows, list) or len(rows) != len(FLEET):
        fail("offline-stop evidence must contain exactly six ordered node roots")
    frozen_rows = freeze["nodes"]
    node_fields = {
        "node",
        "host",
        "validator_address",
        "stake",
        "stop_complete_sha256",
        "stop_files_sha256",
        "stopped_status_sha256",
        "stopped_status_argv_sha256",
    }
    status_fields = (
        "validator_address",
        "stake",
        "writer_pid",
        "writer_start_ticks",
        "boot_id",
        "writer_cgroup_sha256",
        "writer_supervision_mode",
        "supervisor_unit",
        "supervisor_main_pid",
        "supervisor_start_ticks",
        "supervisor_executable_path",
        "supervisor_executable_sha256",
        "supervisor_argv_sha256",
        "supervisor_context_sha256",
        "executable_path",
        "executable_sha256",
        "argv_sha256",
        "data_dir",
    )
    complete_roots: set[str] = set()
    for index, ((name, host), frozen, raw_row, bundle_node) in enumerate(
        zip(FLEET, frozen_rows, rows, evidence_bundle["nodes"])
    ):
        row = require_exact_object(raw_row, node_fields, f"offline-stop node {index}")
        expected_identity = {
            "node": name,
            "host": host,
            "validator_address": frozen["validator_address"],
            "stake": frozen["stake"],
        }
        for field, expected in expected_identity.items():
            if row.get(field) != expected:
                fail(f"offline-stop evidence {name} {field} differs from the fixed freeze")
        complete_sha = require_hash(
            row.get("stop_complete_sha256"), f"offline-stop {name} completion root"
        )
        files_sha = require_hash(
            row.get("stop_files_sha256"), f"offline-stop {name} tree-index root"
        )
        argv_sha = require_hash(
            row.get("stopped_status_argv_sha256"),
            f"offline-stop {name} stopped-status argv root",
        )
        status_sha = require_hash(
            row.get("stopped_status_sha256"),
            f"offline-stop {name} stopped-status receipt root",
        )
        argv = ["stopped-status", capture_id, name, freeze_sha]
        argv.extend(str(frozen[field]) for field in status_fields)
        if sha256_bytes(canonical_bytes(argv)) != argv_sha:
            fail(f"offline-stop evidence {name} does not bind the exact stopped-status argv")
        status = {
            "capture_id": capture_id,
            "freeze_plan_sha256": freeze_sha,
            "node": name,
            "restart_fenced": True,
            "schema": "arc.recovery.offline-stop-status.v1",
            "stake": frozen["stake"],
            "stop_complete_sha256": complete_sha,
            "stop_files_sha256": files_sha,
            "stop_schema": "arc.recovery.offline-stop.v4",
            "stopped": True,
            "validator_address": frozen["validator_address"],
        }
        if sha256_bytes(canonical_bytes(status)) != status_sha:
            fail(f"offline-stop evidence {name} stopped-status hash is not reproducible")
        if (
            bundle_node["stopped_status"]["sha256"] != status_sha
            or bundle_node["stopped_status"]["value"] != status
        ):
            fail(f"offline-stop evidence {name} status differs from the evidence bundle")
        if complete_sha in complete_roots:
            fail("offline-stop evidence repeats a validator stop.complete root")
        complete_roots.add(complete_sha)
    return digest, details.st_size, copy.deepcopy(receipt), payload, sidecar


def validate_known_hosts(path: Path) -> tuple[str, int, bytes]:
    """Validate the complete, public trust anchor for the fixed production fleet."""

    payload, details = read_secure(
        path,
        label="production SSH known-hosts trust anchor",
        maximum_bytes=16 * 1024,
        exact_mode=0o400,
    )
    try:
        text = payload.decode("ascii")
    except UnicodeDecodeError:
        fail("production SSH known-hosts trust anchor must be ASCII")
    lines = text.splitlines(keepends=True)
    if len(lines) != len(FLEET) or any(not line.endswith("\n") for line in lines):
        fail("production SSH known-hosts trust anchor must contain exactly six LF-terminated lines")
    key_blobs: set[bytes] = set()
    for index, ((node, host), line) in enumerate(zip(FLEET, lines)):
        fields = line[:-1].split(" ")
        if len(fields) != 3 or any(not field for field in fields):
            fail(f"production SSH known-hosts line {index} is not exact")
        actual_host, algorithm, encoded = fields
        if actual_host != host:
            fail(f"production SSH known-hosts {node} address differs from the fixed fleet")
        if algorithm != "ssh-ed25519":
            fail(f"production SSH known-hosts {node} must pin one ssh-ed25519 key")
        try:
            blob = base64.b64decode(encoded, validate=True)
        except (binascii.Error, ValueError):
            fail(f"production SSH known-hosts {node} key is not canonical base64")
        expected_prefix = struct.pack(">I", len(b"ssh-ed25519")) + b"ssh-ed25519"
        if (
            not blob.startswith(expected_prefix)
            or len(blob) != len(expected_prefix) + 4 + 32
            or blob[len(expected_prefix) : len(expected_prefix) + 4] != struct.pack(">I", 32)
        ):
            fail(f"production SSH known-hosts {node} is not an Ed25519 public-key blob")
        if base64.b64encode(blob).decode("ascii") != encoded:
            fail(f"production SSH known-hosts {node} key is not canonical base64")
        if blob in key_blobs:
            fail("production SSH known-hosts trust anchor repeats an Ed25519 host key")
        key_blobs.add(blob)
    return sha256_bytes(payload), details.st_size, payload


def validate_private_ssh_identity(path: Path) -> bytes:
    payload, details = read_secure(
        path,
        label="production SSH identity",
        maximum_bytes=128 * 1024,
        exact_mode=0o400,
    )
    if details.st_nlink != 1 or details.st_uid != os.geteuid():
        fail("production SSH identity must be single-linked and owned by the invoking operator")
    return payload


def _validate_protected_system_ancestry(path: Path, label: str) -> None:
    """Require a root-owned, non-writable, symlink-free system parent chain."""

    _lexical_absolute(path, label)
    current = Path("/")
    ancestors = [current]
    for component in path.parent.parts[1:]:
        current /= component
        ancestors.append(current)
    for current in ancestors:
        details = current.lstat()
        if current.is_symlink() or not stat.S_ISDIR(details.st_mode):
            fail(f"{label} has a symlink or non-directory system ancestor: {current}")
        if details.st_uid != 0 or details.st_mode & 0o022:
            fail(f"{label} has an unprotected system ancestor: {current}")


def validate_root_system_tool(
    path: Path,
    label: str,
    *,
    allow_multiple_hardlinks: bool = False,
) -> str:
    _validate_protected_system_ancestry(path, label)
    digest, _size = hash_secure(path, label, executable=True)
    details = path.lstat()
    if path.is_symlink() or not stat.S_ISREG(details.st_mode):
        fail(f"{label} must be an absolute regular non-symlink file")
    if details.st_uid != 0 or details.st_mode & 0o022:
        fail(f"{label} must be root-owned and not group/world writable")
    if details.st_nlink < 1 or (details.st_nlink != 1 and not allow_multiple_hardlinks):
        fail(f"{label} has an unreviewed hard-link count")
    return digest


def _resolve_system_python_entrypoint(
    *,
    lstat_fn: Any = os.lstat,
    readlink_fn: Any = os.readlink,
) -> Path:
    """Resolve the reviewed direct/macOS or same-directory/Linux shape."""

    candidate = SYSTEM_PYTHON_ENTRYPOINT
    seen: set[Path] = set()
    for _ in range(8):
        if candidate in seen:
            fail("production Python entry point contains a symlink cycle")
        seen.add(candidate)
        try:
            details = lstat_fn(os.fspath(candidate))
        except OSError as error:
            fail(f"production Python entry point is unavailable: {error}")
        if not stat.S_ISLNK(details.st_mode):
            if candidate.parent != Path("/usr/bin") or re.fullmatch(
                r"python3(?:\.[0-9]+)?", candidate.name
            ) is None:
                fail("production Python resolved outside the reviewed /usr/bin family")
            return candidate
        if details.st_uid != 0:
            fail("production Python entry-point symlink is not root-owned")
        target = Path(readlink_fn(os.fspath(candidate)))
        candidate = target if target.is_absolute() else candidate.parent / target
        candidate = Path(os.path.normpath(os.fspath(candidate)))
        if candidate.parent != Path("/usr/bin"):
            fail("production Python symlink resolves outside protected /usr/bin")
    fail("production Python entry point exceeds the reviewed symlink depth")


def _system_python() -> tuple[Path, str]:
    """Resolve only the protected /usr/bin Python entry point.

    macOS legitimately ships ``/usr/bin/python3`` as one of many hard links,
    while Debian-family Linux commonly ships it as a root-owned symlink to a
    versioned binary in the same protected directory.  Both shapes are safe
    here; arbitrary alternative/PATH resolution is not.
    """

    _validate_protected_system_ancestry(
        SYSTEM_PYTHON_ENTRYPOINT, "production Python entry point"
    )
    candidate = _resolve_system_python_entrypoint()
    return candidate, validate_root_system_tool(
        candidate,
        "production Python",
        allow_multiple_hardlinks=True,
    )


def _system_bash() -> tuple[Path, str]:
    errors: list[str] = []
    for candidate in SYSTEM_BASH_CANDIDATES:
        try:
            return candidate, validate_root_system_tool(candidate, "production Bash")
        except (BuilderError, OSError) as error:
            errors.append(f"{candidate}: {error}")
    fail("no reviewed absolute production Bash is available: " + "; ".join(errors))


def execute_remote_stop_verifier(
    args: argparse.Namespace,
    freeze_sha: str,
    evidence_sha: str,
    known_hosts_sha: str,
    challenge: str,
    *,
    python_path: Path,
    python_sha: str,
    ssh_sha: str,
    freeze_payload: bytes,
    freeze_sidecar: bytes,
    evidence_payload: bytes,
    evidence_sidecar: bytes,
    known_hosts_payload: bytes,
    identity_payload: bytes,
) -> dict[str, Any]:
    """Run the protected orchestrator with a non-inheriting execution boundary."""

    bash, _bash_sha = _system_bash()
    environment = {
        "PATH": "/usr/bin:/bin",
        "LANG": "C",
        "LC_ALL": "C",
        "TZ": "UTC",
        "HOME": "/var/empty",
    }
    try:
        with tempfile.TemporaryDirectory(prefix="arc-offline-stop-verify-") as temporary_raw:
            temporary = Path(temporary_raw)
            os.chmod(temporary, 0o700)
            staged = {
                "freeze": (temporary / args.freeze_plan.name, freeze_payload),
                "freeze_sidecar": (
                    temporary / f"{args.freeze_plan.name}.sha256",
                    freeze_sidecar,
                ),
                "evidence": (temporary / args.offline_stop_evidence.name, evidence_payload),
                "evidence_sidecar": (
                    temporary / f"{args.offline_stop_evidence.name}.sha256",
                    evidence_sidecar,
                ),
                "known_hosts": (temporary / "known_hosts", known_hosts_payload),
                "identity": (temporary / "id_ed25519", identity_payload),
            }
            for path, payload in staged.values():
                descriptor = os.open(
                    path,
                    os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
                    0o400,
                )
                with os.fdopen(descriptor, "wb") as handle:
                    handle.write(payload)
                    handle.flush()
                    os.fsync(handle.fileno())
            directory = os.open(temporary, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
            command = [
                os.fspath(bash),
                os.fspath(SCRIPT_DIR / "archive-fleet-to-drive.sh"),
                "verify-offline-stop",
                "--freeze-plan",
                os.fspath(staged["freeze"][0]),
                "--offline-stop-evidence",
                os.fspath(staged["evidence"][0]),
                "--offline-stop-evidence-sha256",
                evidence_sha,
                "--ssh-known-hosts",
                os.fspath(staged["known_hosts"][0]),
                "--ssh-known-hosts-sha256",
                known_hosts_sha,
                "--ssh-identity",
                os.fspath(staged["identity"][0]),
                "--python-path",
                os.fspath(python_path),
                "--python-sha256",
                python_sha,
                "--ssh-sha256",
                ssh_sha,
                "--challenge",
                challenge,
            ]
            result = subprocess.run(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                errors="strict",
                cwd="/",
                env=environment,
                close_fds=True,
                start_new_session=True,
                timeout=180,
                check=False,
            )
    except (OSError, subprocess.TimeoutExpired, UnicodeError) as error:
        fail(f"fresh six-host offline-stop verification is unavailable: {error}")
    value = parse_command_json(result, "fresh six-host offline-stop verification")
    if result.stdout.encode("utf-8") != canonical_bytes(value):
        fail("fresh six-host offline-stop verification returned noncanonical or multiple output")
    return value


def _parse_utc_seconds(value: Any, field: str) -> dt.datetime:
    if not isinstance(value, str) or re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", value) is None:
        fail(f"{field} must be canonical whole-second UTC")
    try:
        parsed = dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=dt.timezone.utc)
    except ValueError:
        fail(f"{field} is not a real UTC timestamp")
    return parsed


def validate_remote_stop_verification(
    value: Any,
    *,
    args: argparse.Namespace,
    freeze: Mapping[str, Any],
    freeze_sha: str,
    evidence: Mapping[str, Any],
    evidence_sha: str,
    known_hosts_sha: str,
    ssh_sha: str,
    challenge: str,
) -> dict[str, Any]:
    top_fields = {
        "schema", "source_main_commit", "freeze_plan_sha256", "capture_id",
        "remote_helper_sha256", "remote_helper_path", "offline_stop_evidence_sha256",
        "ssh_known_hosts_sha256", "ssh_path", "ssh_sha256", "challenge",
        "started_at", "completed_at", "duration_ms", "nodes",
    }
    receipt = require_exact_object(value, top_fields, "fresh offline-stop remote verification")
    capture = rollout.capture_id_for_freeze_plan_hash(freeze_sha)
    helper_sha = freeze["remote_helper_sha256"]
    expected = {
        "schema": "arc.recovery.offline-stop-remote-verification.v1",
        "source_main_commit": args.source_main_sha,
        "freeze_plan_sha256": freeze_sha,
        "capture_id": capture,
        "remote_helper_sha256": helper_sha,
        "remote_helper_path": f"/root/.arc-recovery-helpers/{helper_sha}/archive-node.sh",
        "offline_stop_evidence_sha256": evidence_sha,
        "ssh_known_hosts_sha256": known_hosts_sha,
        "ssh_path": os.fspath(SYSTEM_SSH),
        "ssh_sha256": ssh_sha,
        "challenge": challenge,
    }
    for field, wanted in expected.items():
        if receipt.get(field) != wanted:
            fail(f"fresh offline-stop remote verification {field} differs")
    require_hash(challenge, "fresh offline-stop challenge")
    duration = require_uint(receipt.get("duration_ms"), "fresh offline-stop verification duration")
    if duration > MAX_OFFLINE_STOP_VERIFICATION_DURATION_MS:
        fail("fresh offline-stop remote verification exceeded its bounded execution window")
    started = _parse_utc_seconds(receipt.get("started_at"), "fresh offline-stop started_at")
    completed = _parse_utc_seconds(receipt.get("completed_at"), "fresh offline-stop completed_at")
    if completed < started or (completed - started).total_seconds() * 1000 > duration + 1999:
        fail("fresh offline-stop remote verification timestamps disagree with duration")
    now = dt.datetime.now(dt.timezone.utc)
    age = (now - completed).total_seconds()
    if age < -5 or age > MAX_OFFLINE_STOP_VERIFICATION_AGE_SECONDS:
        fail("fresh offline-stop remote verification is stale or from the future")
    rows = receipt.get("nodes")
    if not isinstance(rows, list) or len(rows) != len(FLEET):
        fail("fresh offline-stop remote verification must contain all six ordered hosts")
    local_rows = evidence["nodes"]
    status_fields = {
        "schema", "capture_id", "node", "host", "freeze_plan_sha256",
        "validator_address", "stake", "stopped", "restart_fenced", "stop_schema",
        "stop_complete_sha256", "stop_files_sha256", "challenge",
    }
    seen_statuses: set[str] = set()
    for index, ((node, host), frozen, local, raw) in enumerate(
        zip(FLEET, freeze["nodes"], local_rows, rows)
    ):
        row = require_exact_object(raw, {"node", "host", "status", "status_sha256"}, f"fresh stop node {index}")
        if row.get("node") != node or row.get("host") != host:
            fail(f"fresh offline-stop remote verification topology differs at {node}")
        status = require_exact_object(row.get("status"), status_fields, f"fresh stop status {node}")
        status_sha = require_hash(row.get("status_sha256"), f"fresh stop status hash {node}")
        if sha256_bytes(canonical_bytes(status)) != status_sha or status_sha in seen_statuses:
            fail(f"fresh offline-stop {node} status hash is not unique and reproducible")
        seen_statuses.add(status_sha)
        expected_status = {
            "schema": "arc.recovery.offline-stop-challenged-status.v1",
            "capture_id": capture,
            "node": node,
            "host": host,
            "freeze_plan_sha256": freeze_sha,
            "validator_address": frozen["validator_address"],
            "stake": frozen["stake"],
            "stopped": True,
            "restart_fenced": True,
            "stop_schema": "arc.recovery.offline-stop.v4",
            "stop_complete_sha256": local["stop_complete_sha256"],
            "stop_files_sha256": local["stop_files_sha256"],
            "challenge": challenge,
        }
        if status != expected_status:
            fail(f"fresh offline-stop {node} status differs from the sealed stop tree/freeze")
    return copy.deepcopy(receipt)


def parse_command_json(result: subprocess.CompletedProcess[str], label: str) -> dict[str, Any]:
    if result.returncode != 0:
        diagnostic = result.stderr.strip()[:1000]
        fail(f"{label} failed closed (exit {result.returncode}): {diagnostic}")
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        fail(f"{label} did not return one JSON object: {error}")
    if not isinstance(value, dict):
        fail(f"{label} did not return one JSON object")
    return value


def execute_installed_key_verifier(
    args: argparse.Namespace, manifest: Mapping[str, Any]
) -> dict[str, Any]:
    """Obtain one fresh, challenged proof of the six installed validator keys.

    The verifier consumes a temporary, create-only provisional manifest. Its
    result binds immutable stage and receipt roots rather than that provisional
    manifest digest, avoiding a hash cycle when the proof is added to final
    provenance.
    """

    try:
        rollout.validate_manifest(
            manifest, allow_provisional_installed_key_proof=True
        )
        rollout.require_prearchive_manifest(manifest)
    except rollout.RolloutError as error:
        fail(f"provisional installed-key rollout is invalid: {error}")

    chain = manifest["provenance"]["validator_key_receipt_chain"]
    python_path, python_sha = _system_python()
    ssh_sha = validate_root_system_tool(SYSTEM_SSH, "installed-key SSH client")
    scp_sha = validate_root_system_tool(SYSTEM_SCP, "installed-key SCP client")
    identity_sha, _identity_size = hash_secure(
        args.ssh_identity, "staged installed-key SSH identity"
    )
    transport_expected = {
        "ssh_sha256": ssh_sha,
        "scp_sha256": scp_sha,
        "ssh_identity_sha256": identity_sha,
        "known_hosts_sha256": manifest["artifacts"]["ssh_known_hosts"]["sha256"],
    }
    for field, observed in transport_expected.items():
        if chain[field] != observed:
            fail(f"installed-key verifier {field} differs from the sealed receipt chain")

    artifacts = manifest["artifacts"]
    stage_root = Path(artifacts["production_input_stage_manifest"]["path"]).parent
    challenge = secrets.token_hex(32)
    bash, _bash_sha = _system_bash()
    environment = {
        "PATH": "/usr/bin:/bin",
        "LANG": "C",
        "LC_ALL": "C",
        "TZ": "UTC",
        "HOME": "/var/empty",
        "ARC_RECOVERY_SSH_USER": "root",
        "ARC_RECOVERY_PYTHON_PATH": os.fspath(python_path),
        "ARC_RECOVERY_PYTHON_SHA256": python_sha,
        "ARC_RECOVERY_SSH_KNOWN_HOSTS": os.fspath(args.ssh_known_hosts),
        "ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256": artifacts["ssh_known_hosts"]["sha256"],
        "ARC_RECOVERY_SSH_IDENTITY": os.fspath(args.ssh_identity),
        "ARC_RECOVERY_SSH_IDENTITY_SHA256": identity_sha,
        "ARC_RECOVERY_SSH_SHA256": ssh_sha,
        "ARC_RECOVERY_SCP_SHA256": scp_sha,
    }
    try:
        with tempfile.TemporaryDirectory(
            prefix=".arc-installed-proof.", dir=stage_root.parent
        ) as temporary_raw:
            temporary = Path(temporary_raw)
            os.chmod(temporary, 0o700)
            provisional = temporary / "provisional-rollout.json"
            create_private_seal(provisional, manifest)
            command = [
                os.fspath(bash),
                os.fspath(SCRIPT_DIR / "archive-fleet-to-drive.sh"),
                "verify-installed-keys",
                "--freeze-plan",
                os.fspath(args.freeze_plan),
                "--manifest",
                os.fspath(provisional),
                "--cli",
                os.fspath(args.cli),
                "--cli-sha256",
                artifacts["cli"]["sha256"],
                "--validator-public-keys",
                os.fspath(args.validator_public_keys),
                "--validator-public-keys-sha256",
                artifacts["validator_public_keys"]["sha256"],
                "--validator-install-receipt",
                os.fspath(args.validator_key_install_receipt),
                "--validator-install-receipt-sha256",
                artifacts["validator_key_install_receipt"]["sha256"],
                "--vault-restore-receipt",
                os.fspath(args.validator_vault_restore_receipt),
                "--vault-restore-receipt-sha256",
                artifacts["validator_vault_restore_receipt"]["sha256"],
                "--challenge",
                challenge,
            ]
            result = subprocess.run(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                errors="strict",
                cwd="/",
                env=environment,
                close_fds=True,
                start_new_session=True,
                timeout=300,
                check=False,
            )
    except (OSError, subprocess.TimeoutExpired, UnicodeError) as error:
        fail(f"fresh six-host installed-key verification is unavailable: {error}")

    proof = parse_command_json(result, "fresh six-host installed-key verification")
    if result.stdout.encode("utf-8") != canonical_bytes(proof):
        fail("fresh six-host installed-key verification returned noncanonical or multiple output")
    if proof.get("challenge") != challenge:
        fail("fresh six-host installed-key verification challenge differs")
    try:
        rollout.validate_validator_installed_key_proof(proof, manifest)
    except rollout.RolloutError as error:
        fail(f"fresh six-host installed-key verification failed validation: {error}")
    now_ms = int(dt.datetime.now(dt.timezone.utc).timestamp() * 1000)
    completed_ms = proof["completed_at_unix_ms"]
    if completed_ms > now_ms + 5_000 or now_ms - completed_ms > 60_000:
        fail("fresh six-host installed-key verification is stale or future-dated")
    return copy.deepcopy(proof)


def run_exact_binary(binary: Path, argv: list[str], label: str, *, timeout: int = 1800) -> dict[str, Any]:
    command = [os.fspath(binary), *argv]
    environment = {
        "PATH": "/usr/local/bin:/usr/bin:/bin",
        "LANG": "C",
        "LC_ALL": "C",
        "TZ": "UTC",
    }
    try:
        result = subprocess.run(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="strict",
            cwd="/",
            env=environment,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired, UnicodeError) as error:
        fail(f"{label} could not execute safely: {error}")
    return parse_command_json(result, label)


CHECKPOINT_COMPARE_FIELDS = (
    "manifest_hash",
    "payload_hash",
    "full_state_root",
    "chain_id",
    "genesis_hash",
    "source_height",
    "source_block_hash",
    "source_state_root",
    "source_consensus_round",
    "created_at_unix_ms",
    "transition_height",
    "transition_block_hash",
    "recovery_domain",
    "recovery_epoch",
    "validator_set_id",
    "protocol_version",
    "validator_count",
    "source_validator_count",
    "source_validator_stake",
    "source_validator_set_hash",
    "community_reward_issuance_policy_hash",
)


def validate_checkpoint_summary(value: dict[str, Any], label: str) -> None:
    for field in (
        "manifest_hash",
        "payload_hash",
        "full_state_root",
        "genesis_hash",
        "source_block_hash",
        "source_state_root",
        "transition_block_hash",
        "recovery_domain",
        "source_validator_set_hash",
        "community_reward_issuance_policy_hash",
    ):
        raw = value.get(field)
        if not isinstance(raw, str):
            fail(f"{label}.{field} is missing")
        require_hash(raw.removeprefix("0x"), f"{label}.{field}")
    for field in (
        "source_height",
        "source_consensus_round",
        "created_at_unix_ms",
        "transition_height",
        "recovery_epoch",
        "validator_set_id",
        "validator_count",
        "signature_count",
        "source_validator_count",
        "source_validator_stake",
    ):
        require_uint(value.get(field), f"{label}.{field}")
    if not isinstance(value.get("chain_id"), str) or not value["chain_id"]:
        fail(f"{label}.chain_id is missing")
    if not isinstance(value.get("protocol_version"), str) or re.fullmatch(
        r"3\.[0-9]+\.[0-9]+", value["protocol_version"]
    ) is None:
        fail(f"{label}.protocol_version is not protocol v3")
    if value["validator_count"] != 6:
        fail(f"{label} does not contain the six-validator recovery set")
    if value["source_validator_count"] != 8 or value["source_validator_stake"] != 40_000_000:
        fail(f"{label} does not bind the canonical eight-validator/40M source set")
    if value["transition_height"] != value["source_height"] + 1:
        fail(f"{label} transition height is not exactly source H+1")


def inspect_signed_checkpoint(args: argparse.Namespace) -> dict[str, Any]:
    inspected = run_exact_binary(
        args.binary,
        ["recovery", "inspect", "--checkpoint", os.fspath(args.checkpoint)],
        "signed checkpoint inspect",
    )
    validate_checkpoint_summary(inspected, "signed checkpoint inspect")
    if inspected.get("status") != "UNTRUSTED_INSPECTION":
        fail("signed checkpoint inspect returned an unexpected status")
    return inspected


def reproduce_checkpoint(args: argparse.Namespace, inspected: dict[str, Any]) -> None:
    source_wal_parent = args.source_wal.parent
    if args.source_wal.name != "state.wal":
        fail("source WAL must be the exact state.wal inside the preserved reference data directory")
    with tempfile.TemporaryDirectory(prefix=".arc-production-manifest-export-") as temporary:
        temporary_root = Path(temporary)
        os.chmod(temporary_root, 0o700)
        candidate = temporary_root / "reproduced.arcchkpt"
        exported = run_exact_binary(
            args.binary,
            [
                "recovery",
                "export",
                "--data-dir",
                os.fspath(source_wal_parent),
                "--snapshot",
                os.fspath(args.source_snapshot),
                "--genesis",
                os.fspath(args.genesis),
                "--validator-public-keys",
                os.fspath(args.validator_public_keys),
                "--legacy-validator-set",
                os.fspath(args.legacy_validator_set),
                "--output",
                os.fspath(candidate),
                "--source-consensus-round",
                str(inspected["source_consensus_round"]),
                "--created-at-unix-ms",
                str(inspected["created_at_unix_ms"]),
                "--recovery-epoch",
                str(inspected["recovery_epoch"]),
                "--validator-set-id",
                str(inspected["validator_set_id"]),
                "--allow-unbound-legacy-wal",
            ],
            "canonical snapshot/WAL checkpoint reproduction",
            timeout=3600,
        )
        validate_checkpoint_summary(exported, "reproduced checkpoint")
        if exported.get("status") != "EXPORTED_UNSIGNED" or exported.get("signature_count") != 0:
            fail("checkpoint reproduction did not produce one unsigned canonical candidate")
        if not candidate.is_file() or candidate.is_symlink():
            fail("checkpoint reproduction did not create a regular candidate file")
        for field in CHECKPOINT_COMPARE_FIELDS:
            if exported.get(field) != inspected.get(field):
                fail(f"checkpoint reproduction {field} differs from the selected signed checkpoint")


def chain_from_checkpoint(
    inspected: dict[str, Any],
    boundary: Mapping[str, Any],
    boundary_sha: str,
    evidence_bundle_sha: str,
    late_fork_source_set_sha: str,
) -> dict[str, Any]:
    return {
        "chain_id": inspected["chain_id"],
        "genesis_hash": inspected["genesis_hash"],
        "protocol_version": inspected["protocol_version"],
        "recovery_epoch": inspected["recovery_epoch"],
        "validator_set_id": inspected["validator_set_id"],
        "source_height": inspected["source_height"],
        "legacy_maintenance_evidence_bundle_sha256": evidence_bundle_sha,
        "legacy_maintenance_boundary_sha256": boundary_sha,
        "legacy_late_fork_source_set_sha256": late_fork_source_set_sha,
        "legacy_observed_cutoff_height": boundary["observed_cutoff_height"],
        "legacy_continuity_safety_margin": boundary["continuity_safety_margin"],
        "legacy_public_max_height": boundary["legacy_public_max_height"],
        "legacy_global_absence_claimed": boundary["global_absence_claimed"],
        "legacy_official_origins": copy.deepcopy(
            boundary["official_origin_scope"]["origins"]
        ),
        "legacy_reopening_policy": copy.deepcopy(boundary["reopening_policy"]),
        "legacy_late_fork_circuit": copy.deepcopy(boundary["late_fork_circuit"]),
        "legacy_quarantine_threat_model": copy.deepcopy(boundary["threat_model"]),
        "source_consensus_round": inspected["source_consensus_round"],
        "created_at_unix_ms": inspected["created_at_unix_ms"],
        "source_block_hash": inspected["source_block_hash"],
        "source_state_root": inspected["source_state_root"],
        "transition_height": inspected["transition_height"],
        "transition_block_hash": inspected["transition_block_hash"],
        "full_state_root": inspected["full_state_root"],
        "recovery_domain": inspected["recovery_domain"],
        "approved_checkpoint_manifest_hash": inspected["manifest_hash"],
    }


def production_validators(
    freeze: dict[str, Any], genesis_validators: list[dict[str, Any]]
) -> list[dict[str, Any]]:
    frozen = {row["name"]: row for row in freeze["nodes"]}
    result: list[dict[str, Any]] = []
    for (name, host), genesis in zip(FLEET, genesis_validators):
        frozen_node = frozen[name]
        result.append(
            {
                "name": name,
                "address": genesis["address"],
                "stake": genesis["stake"],
                "key_file": "/etc/arc-v3/validator-key.json",
                "rpc_listen": "127.0.0.1:9090",
                "rpc_url": f"https://{host}",
                "p2p_port": 9091,
                "p2p_advertise": f"{host}:9091",
                "data_dir": "/var/lib/arc-v3",
                "extra_args": ["--enable-community-rewards-v1", "--node-name", name.upper()],
                "host": host,
                "ssh_user": "root",
                "remote_root": "/opt/arc/recovery-v3",
                "service_user": "root",
                "service_name": f"arc-node-v3-{name}.service",
                "model_path": frozen_node["model_path"],
                "model_sha256": frozen_node["model_sha256"],
                "model_size_bytes": frozen_node["model_size_bytes"],
                "shard_ranges": copy.deepcopy(frozen_node["shard_ranges"]),
            }
        )
    return result


def prearchive(args: argparse.Namespace) -> str:
    require_commit(args.source_main_sha, "protected-main commit")
    require_hash(args.freeze_plan_sha256, "freeze-plan sha256")
    require_uint(args.pretag_run_id, "pre-tag workflow run id", positive=True)
    require_uint(args.pretag_run_attempt, "pre-tag workflow run attempt", positive=True)
    require_hash(args.curl_sha256, "production curl sha256")
    require_hash(args.ca_bundle_sha256, "production CA bundle sha256")
    if args.reward_probe != SCRIPT_DIR / "community-reward-probe.py":
        fail("community reward probe must be the exact protected-main recovery probe")
    validate_protected_main_commit(args.source_main_sha)
    if args.acme_email != APPROVED_ACME_EMAIL:
        fail(f"ACME email must be the reviewed production account {APPROVED_ACME_EMAIL}")
    pretag_inputs, _pretag_input_payload, _pretag_input_sha = load_pretag_input_set(
        args.pretag_artifact_input_set,
        source_main_sha=args.source_main_sha,
        run_id=args.pretag_run_id,
        run_attempt=args.pretag_run_attempt,
    )
    try:
        set_rows = [
            {
                "raw_actions_zip": row["raw_actions_zip"],
                "expected_artifact_id": row["artifact_id"],
                "kind": row["kind"],
                "platform": row["platform"],
            }
            for row in pretag_inputs
        ]
        with protected_pretag.pretag_actions_set_proof(
            rows=set_rows,
            expected_commit=args.source_main_sha,
            expected_run_id=args.pretag_run_id,
            expected_run_attempt=args.pretag_run_attempt,
            expected_version=VERSION,
            curl=args.curl,
            curl_sha256=args.curl_sha256,
            ca_bundle=args.ca_bundle,
            ca_bundle_sha256=args.ca_bundle_sha256,
        ) as verified_set:
            if verified_set.api_request_count != 4:
                fail("protected pre-tag set proof exceeded its four-request API contract")
            if len(verified_set.artifacts) != len(PRETAG_GROUPS):
                fail("protected pre-tag set proof did not return exactly nine groups")
            verified_rows: list[dict[str, Any]] = []
            for row, proof in zip(pretag_inputs, verified_set.artifacts):
                provenance = copy.deepcopy(proof.provenance)
                if canonical_bytes(provenance) != bytes(proof.provenance_bytes):
                    fail("protected pre-tag verifier returned noncanonical provenance")
                verified_rows.append({**row, "provenance": provenance})
            linux_proof = verified_set.artifacts[0]
            if (verified_rows[0]["kind"], verified_rows[0]["platform"]) != (
                "headless",
                PRETAG_PLATFORM,
            ):
                fail("protected pre-tag set omitted ordered Linux x86_64 headless payloads")
            verified_args = copy.copy(args)
            verified_args.verified_pretag_artifacts = verified_rows
            verified_args.binary = linux_proof.payloads["arc-node-linux-x86_64"]
            verified_args.cli = linux_proof.payloads["arc-cli-linux-x86_64"]
            verified_args.build_metadata = linux_proof.build_metadata_path
            verified_args.genesis = linux_proof.payloads["genesis.toml"]
            args, stage_manifest_sha256 = stage_prearchive_inputs(verified_args)
    except protected_pretag.ProvenanceError as error:
        fail(f"protected pre-tag artifact proof failed: {error}")
    staged_input_rows, _staged_input_payload, staged_input_sha = load_pretag_input_set(
        args.pretag_artifact_input_set,
        source_main_sha=args.source_main_sha,
        run_id=args.pretag_run_id,
        run_attempt=args.pretag_run_attempt,
    )
    expected_staged_coordinates = [
        {
            "kind": row["kind"],
            "platform": row["platform"],
            "artifact_id": row["artifact_id"],
            "raw_actions_zip": row["raw_actions_zip"],
        }
        for row in args.pretag_verified_artifacts
    ]
    if staged_input_rows != expected_staged_coordinates:
        fail("staged protected pre-tag input set does not reference the exact staged raw ZIPs")
    staged_provenance_set, staged_provenance_set_bytes, staged_provenance_set_sha = load_canonical_json(
        args.pretag_initial_provenance_set,
        label="staged protected pre-tag provenance set",
        maximum_bytes=1024 * 1024,
        exact_mode=0o400,
        require_read_only=True,
    )
    expected_staged_set = {
        "schema": PRETAG_PROVENANCE_SET_SCHEMA,
        "repository": REPOSITORY,
        "commit": args.source_main_sha,
        "run_id": args.pretag_run_id,
        "run_attempt": args.pretag_run_attempt,
        "artifacts": [copy.deepcopy(row["provenance"]) for row in args.pretag_verified_artifacts],
    }
    if staged_provenance_set != expected_staged_set:
        fail("staged protected pre-tag provenance set differs from the nine live proof transactions")
    metadata, metadata_sha = validate_build_metadata(args)
    freeze, freeze_payload, freeze_sha, freeze_sidecar_sha, freeze_sidecar = validate_freeze_inputs(args)
    try:
        height_receipt, prequarantine_public_max_height = legacy_height.load_and_validate_receipt(
            args.legacy_public_height_receipt,
            source_main=args.source_main_sha,
            freeze_sha=freeze_sha,
            max_age_seconds=300,
        )
    except legacy_height.HeightReceiptError as error:
        fail(f"legacy public-height receipt failed validation: {error}")
    height_receipt_sha, _ = hash_secure(
        args.legacy_public_height_receipt, "legacy public-height receipt"
    )
    genesis_info, genesis_validators = validate_genesis(args.genesis)
    _public_keys, public_keys_sha, _public_keys_size = validate_validator_public_keys(
        args.validator_public_keys, genesis_validators
    )
    validate_legacy_validator_set(
        args.legacy_validator_set, freeze["legacy_validator_set_sha256"]
    )
    (
        evidence_bundle_sha,
        _evidence_bundle_size,
        maintenance_evidence_bundle,
        maintenance_evidence_bundle_payload,
        maintenance_evidence_bundle_sidecar,
    ) = validate_legacy_maintenance_evidence_bundle(
        args,
        freeze,
        freeze_sha,
        height_receipt,
        height_receipt_sha,
    )
    (
        boundary_sha,
        _boundary_size,
        maintenance_boundary,
        maintenance_boundary_payload,
        maintenance_boundary_sidecar,
    ) = validate_legacy_maintenance_boundary(
        args,
        freeze,
        freeze_sha,
        height_receipt,
        height_receipt_sha,
        maintenance_evidence_bundle,
        evidence_bundle_sha,
    )
    (
        late_fork_source_set,
        late_fork_source_set_sha,
        late_fork_source_set_payload,
        late_fork_source_set_sidecar,
        late_fork_interlock_tool_sha,
    ) = validate_legacy_late_fork_source_set(
        args,
        boundary=maintenance_boundary,
        boundary_sha=boundary_sha,
    )
    legacy_public_max_height = maintenance_boundary["legacy_public_max_height"]
    (
        offline_stop_sha,
        _offline_stop_size,
        offline_stop,
        offline_stop_payload,
        offline_stop_sidecar,
    ) = validate_offline_stop_evidence(
        args,
        freeze,
        freeze_sha,
        freeze_sidecar_sha,
        maintenance_boundary,
        boundary_sha,
        maintenance_evidence_bundle,
        evidence_bundle_sha,
        height_receipt,
        height_receipt_sha,
    )
    known_hosts_sha, _known_hosts_size, known_hosts_payload = validate_known_hosts(
        args.ssh_known_hosts
    )
    identity_payload = validate_private_ssh_identity(args.ssh_identity)
    ssh_sha = validate_root_system_tool(SYSTEM_SSH, "production OpenSSH client")
    python_path, python_sha = _system_python()
    challenge = secrets.token_hex(32)
    remote_verification = validate_remote_stop_verification(
        execute_remote_stop_verifier(
            args,
            freeze_sha,
            offline_stop_sha,
            known_hosts_sha,
            challenge,
            python_path=python_path,
            python_sha=python_sha,
            ssh_sha=ssh_sha,
            freeze_payload=freeze_payload,
            freeze_sidecar=freeze_sidecar,
            evidence_payload=offline_stop_payload,
            evidence_sidecar=offline_stop_sidecar,
            known_hosts_payload=known_hosts_payload,
            identity_payload=identity_payload,
        ),
        args=args,
        freeze=freeze,
        freeze_sha=freeze_sha,
        evidence=offline_stop,
        evidence_sha=offline_stop_sha,
        known_hosts_sha=known_hosts_sha,
        ssh_sha=ssh_sha,
        challenge=challenge,
    )
    if metadata_sha != hash_secure(args.build_metadata, "pre-tag build metadata")[0]:
        fail("pre-tag build metadata changed after validation")
    inspected = inspect_signed_checkpoint(args)
    if prequarantine_public_max_height < inspected["source_height"]:
        fail("legacy public-height receipt is below the selected checkpoint height")
    if maintenance_boundary["observed_cutoff_height"] < prequarantine_public_max_height:
        fail("legacy maintenance cutoff is below a pre-quarantine public observation")
    if genesis_info["activation_height"] != inspected["transition_height"]:
        fail("genesis community reward activation is not the checkpoint H+1 transition")
    reproduce_checkpoint(args, inspected)

    artifacts = {
        "binary": artifact(args.binary, "pre-tag arc-node", executable=True),
        "cli": artifact(args.cli, "pre-tag arc-cli", executable=True),
        "build_metadata": {
            "path": os.fspath(args.build_metadata),
            "sha256": metadata_sha,
        },
        "production_input_stage_manifest": artifact(
            args.stage_manifest, "production input stage manifest"
        ),
        "pretag_artifact_input_set": artifact(
            args.pretag_artifact_input_set, "protected pre-tag artifact input set"
        ),
        "pretag_initial_live_provenance_set": {
            "path": os.fspath(args.pretag_initial_provenance_set),
            "sha256": staged_provenance_set_sha,
        },
        "genesis": artifact(args.genesis, "production genesis"),
        "validator_public_keys": {
            "path": os.fspath(args.validator_public_keys),
            "sha256": public_keys_sha,
        },
        "legacy_public_height_receipt": {
            "path": os.fspath(args.legacy_public_height_receipt),
            "sha256": height_receipt_sha,
        },
        "legacy_maintenance_evidence_bundle": {
            "path": os.fspath(args.legacy_maintenance_evidence_bundle),
            "sha256": evidence_bundle_sha,
        },
        "legacy_maintenance_evidence_bundle_sidecar": artifact(
            args.legacy_maintenance_evidence_bundle.with_name(
                args.legacy_maintenance_evidence_bundle.name + ".sha256"
            ),
            "legacy maintenance evidence bundle sidecar",
        ),
        "legacy_maintenance_boundary": {
            "path": os.fspath(args.legacy_maintenance_boundary),
            "sha256": boundary_sha,
        },
        "legacy_maintenance_boundary_sidecar": artifact(
            args.legacy_maintenance_boundary.with_name(
                args.legacy_maintenance_boundary.name + ".sha256"
            ),
            "legacy maintenance boundary sidecar",
        ),
        "legacy_late_fork_source_set": {
            "path": os.fspath(args.legacy_late_fork_source_set),
            "sha256": late_fork_source_set_sha,
        },
        "legacy_late_fork_source_set_sidecar": artifact(
            args.legacy_late_fork_source_set.with_name(
                args.legacy_late_fork_source_set.name + ".sha256"
            ),
            "legacy late-fork source-set sidecar",
        ),
        "legacy_late_fork_interlock_tool": {
            "path": os.fspath(args.legacy_late_fork_interlock_tool),
            "sha256": late_fork_interlock_tool_sha,
        },
        "offline_stop_evidence": {
            "path": os.fspath(args.offline_stop_evidence),
            "sha256": offline_stop_sha,
        },
        "offline_stop_evidence_sidecar": artifact(
            args.offline_stop_evidence.with_name(args.offline_stop_evidence.name + ".sha256"),
            "offline-stop evidence sidecar",
        ),
        "ssh_known_hosts": {
            "path": os.fspath(args.ssh_known_hosts),
            "sha256": known_hosts_sha,
        },
        "validator_vault_restore_receipt": artifact(
            args.validator_vault_restore_receipt,
            "staged validator vault restore receipt",
        ),
        "validator_key_install_receipt": artifact(
            args.validator_key_install_receipt,
            "staged validator key install receipt",
        ),
        "reward_probe": artifact(
            args.reward_probe, "community reward probe", executable=True
        ),
        "checkpoint": artifact(args.checkpoint, "signed recovery checkpoint"),
        "legacy_validator_set": artifact(args.legacy_validator_set, "legacy validator set"),
        "source_snapshot": artifact(args.source_snapshot, "canonical source snapshot"),
        "source_wal": artifact(args.source_wal, "canonical source WAL"),
        "caddy": artifact(args.caddy, "Caddy linux-amd64", executable=True),
    }
    if artifacts["pretag_artifact_input_set"]["sha256"] != staged_input_sha:
        fail("staged protected pre-tag artifact input set changed")
    if artifacts["production_input_stage_manifest"]["sha256"] != stage_manifest_sha256:
        fail("production input stage manifest changed after its create-only seal")
    for row in args.pretag_verified_artifacts:
        key = pretag_artifact_key(row["kind"], row["platform"])
        artifacts[key] = artifact(
            row["raw_actions_zip"],
            f"protected pre-tag {row['kind']}/{row['platform']} raw Actions ZIP",
        )
        expected_artifact = row["provenance"]["artifact"]
        if (
            artifacts[key]["sha256"] != expected_artifact["raw_actions_zip_sha256"]
            or Path(artifacts[key]["path"]).stat().st_size
            != expected_artifact["raw_actions_zip_size"]
        ):
            fail(f"protected pre-tag {row['kind']}/{row['platform']} staged raw ZIP differs")
    if artifacts["caddy"]["sha256"] != rollout.CADDY_LINUX_AMD64_SHA256:
        fail(
            f"Caddy must be exact {rollout.CADDY_VERSION} linux-amd64 bytes "
            f"({rollout.CADDY_LINUX_AMD64_SHA256})"
        )
    archive_hashes = {
        "archive_orchestrator_sha256": freeze["orchestrator_sha256"],
        "remote_helper_sha256": freeze["remote_helper_sha256"],
        "rollout_tool_sha256": freeze["rollout_tool_sha256"],
        "rollout_schema_sha256": freeze["rollout_schema_sha256"],
    }
    capture = rollout.capture_id_for_freeze_plan_hash(freeze_sha)
    destination = f"{freeze['drive_prefreeze']['remote_root']}/captures/{capture}"
    manifest: dict[str, Any] = {
        "schema": rollout.SCHEMA,
        "rollout_id": f"arc-v080-recovery-{freeze_sha[:12]}",
        "mode": "production",
        "provenance": {
            "source_main_commit": args.source_main_sha,
            "pretag_repository": REPOSITORY,
            "pretag_version": VERSION,
            "pretag_workflow_run_id": args.pretag_run_id,
            "pretag_workflow_run_attempt": args.pretag_run_attempt,
            "production_input_stage_manifest_sha256": stage_manifest_sha256,
            "protected_pretag_artifact": {
                "schema": PRETAG_WINDOW_SET_SCHEMA,
                "groups": [
                    {
                        "kind": row["kind"],
                        "platform": row["platform"],
                        "initial": copy.deepcopy(row["provenance"]),
                        "final": copy.deepcopy(row["provenance"]),
                    }
                    for row in args.pretag_verified_artifacts
                ],
            },
            "freeze_plan_sidecar_sha256": freeze_sidecar_sha,
            "offline_stop_verification": remote_verification,
        },
        "archive": {
            "freeze_plan_sha256": freeze_sha,
            "capture_id": capture,
            "destination": destination,
            "allow_unbound_legacy_wal": True,
            **archive_hashes,
            "complete_sha256": ZERO_HASH,
            "archive_manifest_sha256": ZERO_HASH,
            "sha256sums_sha256": ZERO_HASH,
            "prearchive_rollout_sha256": ZERO_HASH,
        },
        "chain": chain_from_checkpoint(
            inspected,
            maintenance_boundary,
            boundary_sha,
            evidence_bundle_sha,
            late_fork_source_set_sha,
        ),
        "artifacts": artifacts,
        "checks": {
            "startup_timeout_seconds": 600,
            "convergence_timeout_seconds": 7200,
            "observation_seconds": 300,
            "restart_timeout_seconds": 3600,
            "poll_interval_seconds": 5,
            "min_height_advance": 2,
            "reward": {
                "mode": "receipt",
                "expect_protocol_active": True,
                "expect_issuance_ready": True,
                "probe_argv": [os.fspath(args.reward_probe), "--max-tokens", "1"],
                "probe_sha256": artifacts["reward_probe"]["sha256"],
                "expected_reward_base": rollout.COMMUNITY_REWARD_BASE,
            },
        },
        "gateway": {
            "mode": "caddy-nginx",
            "acme_email": args.acme_email,
            "public_get_paths": list(rollout.DEFAULT_PUBLIC_GET_PATHS),
            "public_post_paths": list(rollout.DEFAULT_PUBLIC_POST_PATHS),
        },
        "validators": production_validators(freeze, genesis_validators),
    }
    try:
        _stage_rows, stage_payloads = rollout.verify_production_input_stage(manifest)
        manifest["provenance"]["validator_key_receipt_chain"] = (
            rollout.validate_validator_receipt_chain(manifest, stage_payloads)
        )
    except rollout.RolloutError as error:
        fail(f"validator restore/install receipt chain failed closed: {error}")
    # The manifest is not trusted merely because this builder assembled it.
    # Exercise the same validator and exact checkpoint verifier used at rollout
    # before asking the stopped fleet to prove its installed key bytes.
    try:
        rollout.validate_manifest(
            manifest, allow_provisional_installed_key_proof=True
        )
        rollout.require_prearchive_manifest(manifest)
        rollout.verify_artifacts(manifest)
        digest = rollout.sha256_bytes(rollout.canonical_bytes(manifest))
        rollout.RecoveryRollout(manifest, digest).verify_checkpoint()
    except rollout.RolloutError as error:
        fail(f"derived prearchive rollout failed the execution validator: {error}")
    manifest["provenance"]["validator_installed_key_proof"] = (
        execute_installed_key_verifier(args, manifest)
    )
    try:
        rollout.validate_manifest(manifest)
        rollout.require_prearchive_manifest(manifest)
    except rollout.RolloutError as error:
        fail(f"installed-key-bound prearchive rollout failed validation: {error}")
    # Recheck source seals and freshness immediately before create-only write.
    current_freeze, _ = read_secure(
        args.freeze_plan,
        label="sealed freeze plan final recheck",
        maximum_bytes=32 * 1024 * 1024,
        require_read_only=True,
    )
    current_freeze_sidecar, _ = read_secure(
        args.freeze_plan.with_name(args.freeze_plan.name + ".sha256"),
        label="sealed freeze plan checksum final recheck",
        maximum_bytes=512,
        require_read_only=True,
    )
    if current_freeze != freeze_payload or sha256_bytes(current_freeze) != freeze_sha:
        fail("freeze plan changed before prearchive publication")
    if sha256_bytes(current_freeze_sidecar) != freeze_sidecar_sha:
        fail("freeze plan checksum changed before prearchive publication")
    if hash_secure(args.validator_public_keys, "validator public-key manifest")[0] != public_keys_sha:
        fail("validator public-key manifest changed before prearchive publication")
    try:
        _receipt, final_legacy_max = legacy_height.load_and_validate_receipt(
            args.legacy_public_height_receipt,
            source_main=args.source_main_sha,
            freeze_sha=freeze_sha,
            max_age_seconds=300,
        )
    except legacy_height.HeightReceiptError as error:
        fail(f"legacy public-height receipt failed final freshness check: {error}")
    if final_legacy_max != prequarantine_public_max_height:
        fail("legacy public-height receipt changed before prearchive publication")
    final_bundle_payload, _ = read_secure(
        args.legacy_maintenance_evidence_bundle,
        label="legacy maintenance evidence bundle final recheck",
        maximum_bytes=32 * 1024 * 1024,
        exact_mode=0o400,
    )
    if (
        final_bundle_payload != maintenance_evidence_bundle_payload
        or sha256_bytes(final_bundle_payload) != evidence_bundle_sha
    ):
        fail("legacy maintenance evidence bundle changed before prearchive publication")
    final_bundle_sidecar, _ = read_secure(
        args.legacy_maintenance_evidence_bundle.with_name(
            args.legacy_maintenance_evidence_bundle.name + ".sha256"
        ),
        label="legacy maintenance evidence bundle sidecar final recheck",
        maximum_bytes=512,
        exact_mode=0o400,
    )
    if final_bundle_sidecar != maintenance_evidence_bundle_sidecar:
        fail("legacy maintenance evidence bundle sidecar changed before publication")
    final_boundary_payload, _ = read_secure(
        args.legacy_maintenance_boundary,
        label="legacy maintenance boundary final recheck",
        maximum_bytes=16 * 1024 * 1024,
        exact_mode=0o400,
    )
    if (
        final_boundary_payload != maintenance_boundary_payload
        or sha256_bytes(final_boundary_payload) != boundary_sha
    ):
        fail("legacy maintenance boundary changed before prearchive publication")
    final_boundary_sidecar, _ = read_secure(
        args.legacy_maintenance_boundary.with_name(
            args.legacy_maintenance_boundary.name + ".sha256"
        ),
        label="legacy maintenance boundary sidecar final recheck",
        maximum_bytes=512,
        exact_mode=0o400,
    )
    if final_boundary_sidecar != maintenance_boundary_sidecar:
        fail("legacy maintenance boundary sidecar changed before prearchive publication")
    final_late_fork_payload, _ = read_secure(
        args.legacy_late_fork_source_set,
        label="legacy late-fork source set final recheck",
        maximum_bytes=4 * 1024 * 1024,
        exact_mode=0o400,
    )
    if (
        final_late_fork_payload != late_fork_source_set_payload
        or sha256_bytes(final_late_fork_payload) != late_fork_source_set_sha
    ):
        fail("legacy late-fork source set changed before prearchive publication")
    final_late_fork_sidecar, _ = read_secure(
        args.legacy_late_fork_source_set.with_name(
            args.legacy_late_fork_source_set.name + ".sha256"
        ),
        label="legacy late-fork source-set sidecar final recheck",
        maximum_bytes=512,
        exact_mode=0o400,
    )
    if final_late_fork_sidecar != late_fork_source_set_sidecar:
        fail("legacy late-fork source-set sidecar changed before prearchive publication")
    if hash_secure(
        args.legacy_late_fork_interlock_tool,
        "legacy late-fork interlock tool final recheck",
        executable=True,
    )[0] != late_fork_interlock_tool_sha:
        fail("legacy late-fork interlock tool changed before prearchive publication")
    if hash_secure(args.offline_stop_evidence, "offline-stop evidence final recheck")[0] != offline_stop_sha:
        fail("offline-stop evidence changed before prearchive publication")
    final_stop_sidecar, _ = read_secure(
        args.offline_stop_evidence.with_name(args.offline_stop_evidence.name + ".sha256"),
        label="offline-stop evidence sidecar final recheck",
        maximum_bytes=512,
        exact_mode=0o400,
    )
    if final_stop_sidecar != offline_stop_sidecar:
        fail("offline-stop evidence sidecar changed before prearchive publication")
    final_known_sha, _final_known_size, final_known_payload = validate_known_hosts(
        args.ssh_known_hosts
    )
    if final_known_sha != known_hosts_sha or final_known_payload != known_hosts_payload:
        fail("SSH known-hosts trust anchor changed before prearchive publication")
    if validate_private_ssh_identity(args.ssh_identity) != identity_payload:
        fail("SSH identity changed before prearchive publication")
    for key, label in (
        ("validator_vault_restore_receipt", "validator vault restore receipt"),
        ("validator_key_install_receipt", "validator key install receipt"),
    ):
        if hash_secure(Path(artifacts[key]["path"]), f"{label} final recheck")[0] != artifacts[key]["sha256"]:
            fail(f"{label} changed before prearchive publication")
    final_provenance_set, final_provenance_set_bytes, final_provenance_set_sha = load_canonical_json(
        args.pretag_initial_provenance_set,
        label="protected pre-tag provenance set final recheck",
        maximum_bytes=1024 * 1024,
        exact_mode=0o400,
        require_read_only=True,
    )
    if (
        final_provenance_set != staged_provenance_set
        or final_provenance_set_bytes != staged_provenance_set_bytes
        or final_provenance_set_sha
        != artifacts["pretag_initial_live_provenance_set"]["sha256"]
    ):
        fail("protected pre-tag provenance set changed before prearchive publication")
    if hash_secure(args.stage_manifest, "production input stage manifest final recheck")[0] != stage_manifest_sha256:
        fail("production input stage manifest changed before prearchive publication")
    for row in args.pretag_verified_artifacts:
        key = pretag_artifact_key(row["kind"], row["platform"])
        raw_zip_sha, raw_zip_size = hash_secure(
            row["raw_actions_zip"],
            f"protected pre-tag {row['kind']}/{row['platform']} raw Actions ZIP final recheck",
        )
        expected_artifact = row["provenance"]["artifact"]
        if (
            raw_zip_sha != artifacts[key]["sha256"]
            or raw_zip_sha != expected_artifact["raw_actions_zip_sha256"]
            or raw_zip_size != expected_artifact["raw_actions_zip_size"]
        ):
            fail(f"protected pre-tag {row['kind']}/{row['platform']} raw ZIP changed")
    validate_remote_stop_verification(
        remote_verification,
        args=args,
        freeze=freeze,
        freeze_sha=freeze_sha,
        evidence=offline_stop,
        evidence_sha=offline_stop_sha,
        known_hosts_sha=known_hosts_sha,
        ssh_sha=ssh_sha,
        challenge=challenge,
    )
    validate_protected_main_commit(args.source_main_sha)
    with stable_artifact_identity_window(manifest) as recheck_artifact_identities:
        final_groups: list[dict[str, Any]] = []
        try:
            reproof_set = protected_pretag.final_live_set_reproof(
                initial_provenance_bytes_list=[
                    canonical_bytes(row["provenance"])
                    for row in args.pretag_verified_artifacts
                ],
                expected_commit=args.source_main_sha,
                expected_run_id=args.pretag_run_id,
                expected_run_attempt=args.pretag_run_attempt,
                expected_artifact_ids=[
                    row["artifact_id"] for row in args.pretag_verified_artifacts
                ],
                expected_version=VERSION,
                curl=args.curl,
                curl_sha256=args.curl_sha256,
                ca_bundle=args.ca_bundle,
                ca_bundle_sha256=args.ca_bundle_sha256,
            )
            if reproof_set.api_request_count != 4 or len(reproof_set.proofs) != len(PRETAG_GROUPS):
                fail("final protected pre-tag set reproof exceeded four requests or omitted groups")
            for row, reproof in zip(args.pretag_verified_artifacts, reproof_set.proofs):
                final_provenance = copy.deepcopy(reproof.value)
                if canonical_bytes(final_provenance) != bytes(reproof.canonical_bytes):
                    fail("final protected pre-tag live reproof returned noncanonical provenance")
                final_groups.append(
                    {
                        "kind": row["kind"],
                        "platform": row["platform"],
                        "initial": copy.deepcopy(row["provenance"]),
                        "final": final_provenance,
                    }
                )
        except protected_pretag.ProvenanceError as error:
            fail(f"final protected pre-tag artifact reproof failed: {error}")
        manifest["provenance"]["protected_pretag_artifact"]["groups"] = final_groups
        try:
            rollout.validate_manifest(manifest)
            rollout.require_prearchive_manifest(manifest)
        except rollout.RolloutError as error:
            fail(f"final fresh protected-artifact manifest validation failed: {error}")
        recheck_artifact_identities()
        return create_private_seal(args.output, manifest)


def _archive_item(value: Any, label: str) -> tuple[str, int, str]:
    row = require_exact_object(value, {"name", "size", "sha256"}, label)
    name = row["name"]
    if not isinstance(name, str) or SAFE_OBJECT_NAME_RE.fullmatch(name) is None:
        fail(f"{label}.name is unsafe")
    size = require_uint(row["size"], f"{label}.size", positive=True)
    digest = require_hash(row["sha256"], f"{label}.sha256")
    return name, size, digest


def _expected_artifact_object(
    prearchive: Mapping[str, Any], artifact_name: str, archive_name: str
) -> tuple[str, int, str]:
    artifact_value = prearchive["artifacts"][artifact_name]
    _digest, size = hash_secure(Path(artifact_value["path"]), f"prearchive {artifact_name}")
    if _digest != artifact_value["sha256"]:
        fail(f"prearchive artifact changed before archive finalization: {artifact_name}")
    return archive_name, size, _digest


def validate_drive_archive_seal_receipt(
    value: Any,
    *,
    prearchive: Mapping[str, Any],
    freeze: Mapping[str, Any],
) -> None:
    fields = {
        "schema",
        "mode",
        "freeze_plan_sha256",
        "capture_id",
        "remote_root_sha256",
        "client_id_sha256",
        "account_sha256",
        "permission_id_sha256",
        "rclone_version",
        "source_bytes",
        "archive_reservation_bytes",
        "largest_object_reservation_bytes",
        "daily_upload_budget_bytes",
        "daily_upload_budget_basis",
        "available_bytes_before",
        "available_bytes_after",
        "canary_bytes",
        "canary_verified",
        "canary_deleted",
    }
    receipt = require_exact_object(
        value, fields, "Drive archive-seal prefreeze receipt"
    )
    drive = require_exact_object(
        freeze.get("drive_prefreeze"),
        {
            "gate_sha256",
            "remote_root",
            "remote_root_sha256",
            "oauth_client_id_sha256",
            "account_sha256",
            "daily_upload_budget_bytes",
            "dedicated_no_other_upload_writers_attested",
        },
        "freeze-plan Drive prefreeze",
    )
    expected = {
        "schema": "arc.recovery.drive-prefreeze.v1",
        "mode": "execute",
        "freeze_plan_sha256": prearchive["archive"]["freeze_plan_sha256"],
        "capture_id": prearchive["archive"]["capture_id"],
        "remote_root_sha256": drive["remote_root_sha256"],
        "client_id_sha256": drive["oauth_client_id_sha256"],
        "account_sha256": drive["account_sha256"],
        "rclone_version": "v1.75.0",
        "daily_upload_budget_bytes": drive["daily_upload_budget_bytes"],
        "daily_upload_budget_basis": (
            "operator-reviewed-remaining-dedicated-account"
        ),
        "canary_bytes": 8 * 1024 * 1024,
        "canary_verified": True,
        "canary_deleted": True,
    }
    for field, wanted in expected.items():
        if receipt.get(field) != wanted:
            fail(f"Drive archive-seal prefreeze receipt {field} differs")
    if drive["dedicated_no_other_upload_writers_attested"] is not True:
        fail("Drive archive-seal prefreeze uploader exclusivity is not attested")
    permission = require_hash(
        receipt["permission_id_sha256"],
        "Drive archive-seal prefreeze permission identity",
    )
    if permission == ZERO_HASH:
        fail("Drive archive-seal prefreeze permission identity must be nonzero")
    nodes = freeze.get("nodes")
    if not isinstance(nodes, list) or len(nodes) != len(FLEET):
        fail("Drive archive-seal prefreeze source inventory must contain six nodes")
    sizes = []
    for index, row in enumerate(nodes):
        if not isinstance(row, dict):
            fail(f"freeze node {index} must be an object")
        sizes.append(
            require_uint(
                row.get("data_bytes"),
                f"freeze node {index} data bytes",
                positive=True,
            )
        )
    source_bytes = sum(sizes)
    archive_reservation = 3 * source_bytes + 32 * 1024**3
    largest_reservation = 3 * max(sizes) + 4 * 1024**3
    for field, wanted in (
        ("source_bytes", source_bytes),
        ("archive_reservation_bytes", archive_reservation),
        ("largest_object_reservation_bytes", largest_reservation),
    ):
        observed = require_uint(
            receipt[field],
            f"Drive archive-seal prefreeze {field}",
            positive=True,
        )
        if observed != wanted:
            fail(f"Drive archive-seal prefreeze receipt {field} differs")
    budget = require_uint(
        receipt["daily_upload_budget_bytes"],
        "Drive archive-seal prefreeze daily upload budget",
        positive=True,
    )
    if archive_reservation > budget:
        fail("Drive archive-seal prefreeze archive reservation exceeds its budget")
    before = require_uint(
        receipt["available_bytes_before"],
        "Drive archive-seal prefreeze available bytes before canary",
    )
    after = require_uint(
        receipt["available_bytes_after"],
        "Drive archive-seal prefreeze available bytes after canary",
    )
    if (
        before < archive_reservation + 8 * 1024 * 1024
        or after < archive_reservation
    ):
        fail("Drive archive-seal prefreeze capacity is below the sealed reservation")


def validate_drive_archive_seal_attempt(
    value: Any,
    *,
    prearchive: Mapping[str, Any],
    drive_receipt: Mapping[str, Any],
    drive_receipt_sha: str,
) -> None:
    receipt = require_exact_object(
        value,
        {
            "schema",
            "phase",
            "freeze_plan_sha256",
            "capture_id",
            "attempt_nonce",
            "started_at_unix_ns",
            "completed_at_unix_ns",
            "completed_at",
            "drive_prefreeze_receipt",
            "drive_prefreeze_receipt_sha256",
            "rclone_path",
            "rclone_sha256",
            "rclone_config_sha256",
            "selected_immediately_before_first_archive_upload",
        },
        "Drive archive-seal attempt receipt",
    )
    expected = {
        "schema": "arc.recovery.drive-archive-seal-attempt.v1",
        "phase": "archive-seal",
        "freeze_plan_sha256": prearchive["archive"]["freeze_plan_sha256"],
        "capture_id": prearchive["archive"]["capture_id"],
        "drive_prefreeze_receipt": drive_receipt,
        "drive_prefreeze_receipt_sha256": drive_receipt_sha,
        "selected_immediately_before_first_archive_upload": True,
    }
    if any(receipt.get(field) != wanted for field, wanted in expected.items()):
        fail("Drive archive-seal attempt does not bind the exact fresh execute receipt")
    nonce = require_hash(receipt.get("attempt_nonce"), "Drive archive-seal attempt nonce")
    if nonce == ZERO_HASH:
        fail("Drive archive-seal attempt nonce must be nonzero")
    started = require_uint(
        receipt.get("started_at_unix_ns"),
        "Drive archive-seal attempt started_at_unix_ns",
        positive=True,
    )
    completed = require_uint(
        receipt.get("completed_at_unix_ns"),
        "Drive archive-seal attempt completed_at_unix_ns",
        positive=True,
    )
    if completed < started:
        fail("Drive archive-seal attempt monotonic interval is reversed")
    completed_at = _parse_utc_seconds(
        receipt.get("completed_at"), "Drive archive-seal attempt completed_at"
    )
    if completed_at > dt.datetime.now(dt.timezone.utc) + dt.timedelta(seconds=300):
        fail("Drive archive-seal attempt completion is in the future")
    rclone_path = receipt.get("rclone_path")
    if (
        not isinstance(rclone_path, str)
        or not rclone_path.startswith("/")
        or os.path.normpath(rclone_path) != rclone_path
    ):
        fail("Drive archive-seal attempt rclone path is not canonical absolute")
    for field in ("rclone_sha256", "rclone_config_sha256"):
        if require_hash(receipt.get(field), f"Drive archive-seal attempt {field}") == ZERO_HASH:
            fail(f"Drive archive-seal attempt {field} must be nonzero")


def validate_github_gist_write_canary(
    value: Any,
    *,
    prearchive: Mapping[str, Any],
) -> None:
    fields = {
        "schema",
        "provider",
        "owner_login",
        "freeze_plan_sha256",
        "capture_id",
        "challenge",
        "gist_id",
        "gist_revision",
        "gist_filename",
        "gist_content_sha256",
        "github_cli_path",
        "github_cli_sha256",
        "create_verified",
        "revision_read_verified",
        "delete_verified",
        "completed_at",
    }
    receipt = require_exact_object(value, fields, "GitHub Gist write canary")
    expected_scalars = {
        "schema": "arc.recovery.github-gist-write-canary.v1",
        "provider": "github.com",
        "owner_login": REPOSITORY.split("/", 1)[0],
        "freeze_plan_sha256": prearchive["archive"]["freeze_plan_sha256"],
        "capture_id": prearchive["archive"]["capture_id"],
        "create_verified": True,
        "revision_read_verified": True,
        "delete_verified": True,
    }
    for field, wanted in expected_scalars.items():
        if receipt.get(field) != wanted:
            fail(f"GitHub Gist write canary {field} differs")
    challenge = require_hash(receipt["challenge"], "GitHub Gist write canary challenge")
    if challenge == ZERO_HASH:
        fail("GitHub Gist write canary challenge must be nonzero")
    gist_id = receipt["gist_id"]
    if not isinstance(gist_id, str) or re.fullmatch(r"[0-9a-f]{20,64}", gist_id) is None:
        fail("GitHub Gist write canary id is malformed")
    revision = receipt["gist_revision"]
    if not isinstance(revision, str) or re.fullmatch(r"[0-9a-f]{40}", revision) is None:
        fail("GitHub Gist write canary revision is malformed")
    expected_filename = f"arc-recovery-gist-canary-{challenge}.txt"
    if receipt["gist_filename"] != expected_filename:
        fail("GitHub Gist write canary filename differs")
    expected_content = (
        f"freeze_plan_sha256={prearchive['archive']['freeze_plan_sha256']}\n"
        f"capture_id={prearchive['archive']['capture_id']}\n"
        f"challenge={challenge}\n"
    ).encode("ascii")
    if require_hash(
        receipt["gist_content_sha256"],
        "GitHub Gist write canary content sha256",
    ) != sha256_bytes(expected_content):
        fail("GitHub Gist write canary content hash differs")
    cli_raw = receipt["github_cli_path"]
    if not isinstance(cli_raw, str):
        fail("GitHub Gist write canary CLI path is malformed")
    cli = Path(cli_raw)
    if (
        not cli.is_absolute()
        or os.path.normpath(cli_raw) != cli_raw
        or os.path.realpath(cli_raw) != cli_raw
    ):
        fail("GitHub Gist write canary CLI path is not a normalized real path")
    cli_sha, _cli_size = hash_secure(
        cli,
        "GitHub Gist write canary CLI",
        executable=True,
    )
    if cli_sha != require_hash(
        receipt["github_cli_sha256"],
        "GitHub Gist write canary CLI sha256",
    ):
        fail("GitHub Gist write canary CLI hash differs")
    completed = _parse_utc_seconds(
        receipt["completed_at"], "GitHub Gist write canary completed_at"
    )
    if completed > dt.datetime.now(dt.timezone.utc) + dt.timedelta(seconds=300):
        fail("GitHub Gist write canary completion is in the future")


def validate_archive_evidence(
    prearchive: dict[str, Any],
    prearchive_path: Path,
    prearchive_payload: bytes,
    prearchive_sha: str,
    args: argparse.Namespace,
) -> tuple[str, str, str]:
    try:
        _stage_rows, stage_payloads = rollout.verify_production_input_stage(
            prearchive
        )
    except rollout.RolloutError as error:
        fail(
            "prearchive production input stage failed finalization validation: "
            f"{error}"
        )
    freeze_payload = stage_payloads["freeze_plan"]
    try:
        freeze = validate_pinned_freeze_plan(
            freeze_payload,
            prearchive["archive"]["freeze_plan_sha256"],
        ).value()
    except FreezeValidationError as error:
        fail(f"staged freeze plan failed archive-finalization validation: {error}")
    drive_receipt, drive_receipt_payload, drive_receipt_sha = load_canonical_json(
        args.drive_archive_seal_prefreeze,
        label="downloaded Drive archive-seal prefreeze receipt",
        maximum_bytes=1024 * 1024,
        require_read_only=True,
    )
    validate_drive_archive_seal_receipt(
        drive_receipt,
        prearchive=prearchive,
        freeze=freeze,
    )
    drive_attempt, drive_attempt_payload, drive_attempt_sha = load_canonical_json(
        args.drive_archive_seal_attempt,
        label="downloaded Drive archive-seal attempt receipt",
        maximum_bytes=1024 * 1024,
        require_read_only=True,
    )
    validate_drive_archive_seal_attempt(
        drive_attempt,
        prearchive=prearchive,
        drive_receipt=drive_receipt,
        drive_receipt_sha=drive_receipt_sha,
    )
    gist_canary, gist_canary_payload, gist_canary_sha = load_canonical_json(
        args.github_gist_write_canary,
        label="downloaded GitHub Gist write canary receipt",
        maximum_bytes=1024 * 1024,
        require_read_only=True,
    )
    validate_github_gist_write_canary(gist_canary, prearchive=prearchive)
    complete, complete_payload, complete_sha = load_canonical_json(
        args.complete,
        label="downloaded COMPLETE.json",
        require_read_only=True,
    )
    archive_manifest, archive_payload, archive_sha = load_canonical_json(
        args.archive_manifest,
        label="downloaded ARCHIVE-MANIFEST.json",
        require_read_only=True,
    )
    sums_payload, sums_details = read_secure(
        args.sha256sums,
        label="downloaded SHA256SUMS",
        maximum_bytes=4 * 1024 * 1024,
        require_read_only=True,
    )
    sums_sha = sha256_bytes(sums_payload)
    for label, wanted, actual in (
        ("COMPLETE.json", args.complete_sha256, complete_sha),
        ("ARCHIVE-MANIFEST.json", args.archive_manifest_sha256, archive_sha),
        ("SHA256SUMS", args.sha256sums_sha256, sums_sha),
    ):
        if require_hash(wanted, f"trusted {label} sha256") != actual:
            fail(f"downloaded {label} differs from its independently selected trust root")
    sidecar_payload, sidecar_details = read_secure(
        args.archive_manifest_sidecar,
        label="downloaded ARCHIVE-MANIFEST.json.sha256",
        maximum_bytes=512,
        require_read_only=True,
    )
    if sidecar_payload != f"{archive_sha}  ARCHIVE-MANIFEST.json\n".encode("ascii"):
        fail("archive-manifest sidecar does not bind the exact canonical archive manifest")

    complete_fields = {
        "schema",
        "freeze_plan_sha256",
        "capture_id",
        "rollout_manifest_sha256",
        "source_commit",
        "archive_manifest_sha256",
        "object_count_before_complete",
        "validator_bundle_count",
        "finalization_anchor",
    }
    require_exact_object(complete, complete_fields, "COMPLETE.json")
    if complete["schema"] != "arc.recovery.archive-complete.v2":
        fail("COMPLETE.json schema is unsupported")
    finalization_anchor = require_exact_object(
        complete["finalization_anchor"],
        {
            "intent_sha256",
            "gist_id",
            "gist_revision",
            "gist_file_sha256",
        },
        "COMPLETE.json finalization_anchor",
    )
    intent_sha256 = require_hash(
        finalization_anchor["intent_sha256"],
        "COMPLETE.json finalization intent sha256",
    )
    if intent_sha256 == ZERO_HASH:
        fail("COMPLETE.json finalization intent sha256 must be nonzero")
    gist_file_sha256 = require_hash(
        finalization_anchor["gist_file_sha256"],
        "COMPLETE.json Gist file sha256",
    )
    if gist_file_sha256 != intent_sha256:
        fail("COMPLETE.json Gist file hash differs from the finalization intent")
    gist_id = finalization_anchor["gist_id"]
    if not isinstance(gist_id, str) or re.fullmatch(r"[0-9a-f]{20,64}", gist_id) is None:
        fail("COMPLETE.json Gist id is malformed")
    gist_revision = finalization_anchor["gist_revision"]
    if (
        not isinstance(gist_revision, str)
        or re.fullmatch(r"[0-9a-f]{40}", gist_revision) is None
    ):
        fail("COMPLETE.json Gist revision is malformed")
    manifest_fields = {
        "schema",
        "freeze_plan_sha256",
        "capture_id",
        "rollout_manifest_sha256",
        "source_commit",
        "orchestrator_sha256",
        "remote_helper_sha256",
        "rollout_tool_sha256",
        "rollout_schema_sha256",
        "canonical_reference",
        "capture_classification_counts",
        "shared_inputs",
        "validator_bundles",
        "sha256sums",
    }
    require_exact_object(archive_manifest, manifest_fields, "ARCHIVE-MANIFEST.json")
    if archive_manifest["schema"] != "arc.recovery.archive-manifest.v2":
        fail("ARCHIVE-MANIFEST.json schema is unsupported")
    expected_bindings = {
        "freeze_plan_sha256": prearchive["archive"]["freeze_plan_sha256"],
        "capture_id": prearchive["archive"]["capture_id"],
        "rollout_manifest_sha256": prearchive_sha,
        "source_commit": prearchive["provenance"]["source_main_commit"],
    }
    for field, wanted in expected_bindings.items():
        if archive_manifest.get(field) != wanted or complete.get(field) != wanted:
            fail(f"archive completion {field} differs from the sealed prearchive source")
    if complete["archive_manifest_sha256"] != archive_sha:
        fail("COMPLETE.json does not bind the exact ARCHIVE-MANIFEST.json bytes")
    for archive_field, rollout_field in (
        ("orchestrator_sha256", "archive_orchestrator_sha256"),
        ("remote_helper_sha256", "remote_helper_sha256"),
        ("rollout_tool_sha256", "rollout_tool_sha256"),
        ("rollout_schema_sha256", "rollout_schema_sha256"),
    ):
        if archive_manifest[archive_field] != prearchive["archive"][rollout_field]:
            fail(f"archive {archive_field} differs from prearchive execution provenance")

    shared = archive_manifest["shared_inputs"]
    if not isinstance(shared, list):
        fail("archive shared_inputs must be an array")
    objects: dict[str, tuple[str, int]] = {}
    for index, item in enumerate(shared):
        name, size, digest = _archive_item(item, f"archive shared_inputs[{index}]")
        if name in objects:
            fail("archive shared_inputs contains a duplicate object name")
        objects[name] = (digest, size)

    expected_shared = {
        "arc-node": _expected_artifact_object(prearchive, "binary", "arc-node"),
        "arc-cli": _expected_artifact_object(prearchive, "cli", "arc-cli"),
        "build-metadata.json": _expected_artifact_object(
            prearchive, "build_metadata", "build-metadata.json"
        ),
        "genesis.toml": _expected_artifact_object(prearchive, "genesis", "genesis.toml"),
        "validator-public-keys.json": _expected_artifact_object(
            prearchive, "validator_public_keys", "validator-public-keys.json"
        ),
        "legacy-public-height.json": _expected_artifact_object(
            prearchive,
            "legacy_public_height_receipt",
            "legacy-public-height.json",
        ),
        "legacy-maintenance-evidence-bundle.json": _expected_artifact_object(
            prearchive,
            "legacy_maintenance_evidence_bundle",
            "legacy-maintenance-evidence-bundle.json",
        ),
        "legacy-maintenance-evidence-bundle.json.sha256": _expected_artifact_object(
            prearchive,
            "legacy_maintenance_evidence_bundle_sidecar",
            "legacy-maintenance-evidence-bundle.json.sha256",
        ),
        "legacy-maintenance-boundary.json": _expected_artifact_object(
            prearchive,
            "legacy_maintenance_boundary",
            "legacy-maintenance-boundary.json",
        ),
        "legacy-maintenance-boundary.json.sha256": _expected_artifact_object(
            prearchive,
            "legacy_maintenance_boundary_sidecar",
            "legacy-maintenance-boundary.json.sha256",
        ),
        "legacy-late-fork-source-set.json": _expected_artifact_object(
            prearchive,
            "legacy_late_fork_source_set",
            "legacy-late-fork-source-set.json",
        ),
        "legacy-late-fork-source-set.json.sha256": _expected_artifact_object(
            prearchive,
            "legacy_late_fork_source_set_sidecar",
            "legacy-late-fork-source-set.json.sha256",
        ),
        "legacy-late-fork-interlock.py": _expected_artifact_object(
            prearchive,
            "legacy_late_fork_interlock_tool",
            "legacy-late-fork-interlock.py",
        ),
        "offline-stop-evidence.json": _expected_artifact_object(
            prearchive,
            "offline_stop_evidence",
            "offline-stop-evidence.json",
        ),
        "offline-stop-evidence.json.sha256": _expected_artifact_object(
            prearchive,
            "offline_stop_evidence_sidecar",
            "offline-stop-evidence.json.sha256",
        ),
        "ssh-known-hosts": _expected_artifact_object(
            prearchive,
            "ssh_known_hosts",
            "ssh-known-hosts",
        ),
        "legacy-validator-set-40m.json": _expected_artifact_object(
            prearchive, "legacy_validator_set", "legacy-validator-set-40m.json"
        ),
        "source.snapshot.lz4": _expected_artifact_object(
            prearchive, "source_snapshot", "source.snapshot.lz4"
        ),
        "source.state.wal": _expected_artifact_object(
            prearchive, "source_wal", "source.state.wal"
        ),
        "recovery.arcchkpt": _expected_artifact_object(
            prearchive, "checkpoint", "recovery.arcchkpt"
        ),
        "caddy": _expected_artifact_object(prearchive, "caddy", "caddy"),
        "PRETAG-ARTIFACT-INPUT-SET.json": _expected_artifact_object(
            prearchive,
            "pretag_artifact_input_set",
            "PRETAG-ARTIFACT-INPUT-SET.json",
        ),
        "PRETAG-INITIAL-LIVE-PROVENANCE-SET.json": _expected_artifact_object(
            prearchive,
            "pretag_initial_live_provenance_set",
            "PRETAG-INITIAL-LIVE-PROVENANCE-SET.json",
        ),
        "PRODUCTION-INPUT-STAGE-MANIFEST.json": _expected_artifact_object(
            prearchive,
            "production_input_stage_manifest",
            "PRODUCTION-INPUT-STAGE-MANIFEST.json",
        ),
        "VALIDATOR-VAULT-RESTORE-RECEIPT.json": _expected_artifact_object(
            prearchive,
            "validator_vault_restore_receipt",
            "VALIDATOR-VAULT-RESTORE-RECEIPT.json",
        ),
        "VALIDATOR-KEY-INSTALL-RECEIPT.json": _expected_artifact_object(
            prearchive,
            "validator_key_install_receipt",
            "VALIDATOR-KEY-INSTALL-RECEIPT.json",
        ),
        "drive-archive-seal-prefreeze.json": (
            "drive-archive-seal-prefreeze.json",
            len(drive_receipt_payload),
            drive_receipt_sha,
        ),
        "drive-archive-seal-attempt.json": (
            "drive-archive-seal-attempt.json",
            len(drive_attempt_payload),
            drive_attempt_sha,
        ),
        "github-gist-write-canary.json": (
            "github-gist-write-canary.json",
            len(gist_canary_payload),
            gist_canary_sha,
        ),
    }
    for kind, platform in PRETAG_GROUPS:
        archive_name = f"pretag-{kind}-{platform}.actions.zip"
        expected_shared[archive_name] = _expected_artifact_object(
            prearchive,
            pretag_artifact_key(kind, platform),
            archive_name,
        )
    validator_chain_payload = canonical_bytes(
        prearchive["provenance"]["validator_key_receipt_chain"]
    )
    expected_shared["VALIDATOR-KEY-RECEIPT-CHAIN.json"] = (
        "VALIDATOR-KEY-RECEIPT-CHAIN.json",
        len(validator_chain_payload),
        sha256_bytes(validator_chain_payload),
    )
    reward_probe_path = Path(prearchive["checks"]["reward"]["probe_argv"][0])
    reward_probe_sha, reward_probe_size = hash_secure(
        reward_probe_path, "prearchive reward probe", executable=True
    )
    if reward_probe_sha != prearchive["checks"]["reward"]["probe_sha256"]:
        fail("reward probe changed before archive finalization")
    expected_shared["community-reward-probe.py"] = (
        "community-reward-probe.py",
        reward_probe_size,
        reward_probe_sha,
    )
    for archive_field, archive_name in (
        ("archive_orchestrator_sha256", "archive-fleet-to-drive.sh"),
        ("remote_helper_sha256", "archive-node.sh"),
        ("rollout_tool_sha256", "recovery_rollout.py"),
        ("rollout_schema_sha256", "recovery-manifest.schema.json"),
    ):
        source = {
            "archive_orchestrator_sha256": SCRIPT_DIR / "archive-fleet-to-drive.sh",
            "remote_helper_sha256": SCRIPT_DIR / "archive-node.sh",
            "rollout_tool_sha256": SCRIPT_DIR / "recovery_rollout.py",
            "rollout_schema_sha256": SCRIPT_DIR / "recovery-manifest.schema.json",
        }[archive_field]
        digest, size = hash_secure(source, f"archive source {archive_name}")
        if digest != prearchive["archive"][archive_field]:
            fail(f"executing {archive_name} changed since the prearchive seal")
        expected_shared[archive_name] = (archive_name, size, digest)
    expected_shared["freeze-plan.json"] = (
        "freeze-plan.json",
        objects.get("freeze-plan.json", ("", 0))[1],
        prearchive["archive"]["freeze_plan_sha256"],
    )
    expected_shared["freeze-plan.json.sha256"] = (
        "freeze-plan.json.sha256",
        objects.get("freeze-plan.json.sha256", ("", 0))[1],
        prearchive["provenance"]["freeze_plan_sidecar_sha256"],
    )
    expected_shared["rollout-manifest.json"] = (
        "rollout-manifest.json",
        len(prearchive_payload),
        prearchive_sha,
    )
    prearchive_sidecar = f"{prearchive_sha}  {prearchive_path.name}\n".encode("ascii")
    expected_shared["rollout-manifest.json.sha256"] = (
        "rollout-manifest.json.sha256",
        len(prearchive_sidecar),
        sha256_bytes(prearchive_sidecar),
    )
    derived_text = {
        "source-commit.txt": (prearchive["provenance"]["source_main_commit"] + "\n").encode(),
        "capture-id.txt": (prearchive["archive"]["capture_id"] + "\n").encode(),
    }
    for name, payload in derived_text.items():
        expected_shared[name] = (name, len(payload), sha256_bytes(payload))
    for name, (_expected_name, expected_size, expected_digest) in expected_shared.items():
        observed = objects.get(name)
        if observed != (expected_digest, expected_size):
            fail(f"archive shared object {name} differs from the prearchive source")

    reference_fields = {
        "schema",
        "independently_verified",
        "allow_unbound_legacy_wal",
        "verifier_binary",
        "genesis",
        "validator_public_keys",
        "legacy_validator_set",
        "source_snapshot",
        "source_wal",
        "selected_checkpoint",
        "source_height",
        "source_block_hash",
        "source_state_root",
        "transition_state_root",
        "checkpoint_manifest_hash",
        "source_consensus_round",
        "created_at_unix_ms",
        "recovery_epoch",
        "validator_set_id",
    }
    reference = require_exact_object(
        archive_manifest["canonical_reference"],
        reference_fields,
        "archive canonical reference",
    )
    if (
        reference["schema"] != "arc.recovery.canonical-reference.v1"
        or reference["independently_verified"] is not True
        or reference["allow_unbound_legacy_wal"]
        != prearchive["archive"]["allow_unbound_legacy_wal"]
    ):
        fail("archive canonical reference status/policy differs")
    reference_objects = {
        "verifier_binary": "arc-node",
        "genesis": "genesis.toml",
        "validator_public_keys": "validator-public-keys.json",
        "legacy_validator_set": "legacy-validator-set-40m.json",
        "source_snapshot": "source.snapshot.lz4",
        "source_wal": "source.state.wal",
        "selected_checkpoint": "recovery.arcchkpt",
    }
    for field, name in reference_objects.items():
        item_name, item_size, item_digest = _archive_item(reference[field], f"canonical reference {field}")
        if item_name != name or objects.get(name) != (item_digest, item_size):
            fail(f"archive canonical reference {field} differs from shared object roots")
    chain = prearchive["chain"]
    reference_chain = {
        "source_height": chain["source_height"],
        "source_block_hash": chain["source_block_hash"].removeprefix("0x"),
        "source_state_root": chain["source_state_root"].removeprefix("0x"),
        "transition_state_root": chain["full_state_root"].removeprefix("0x"),
        "checkpoint_manifest_hash": chain["approved_checkpoint_manifest_hash"].removeprefix("0x"),
        "source_consensus_round": chain["source_consensus_round"],
        "created_at_unix_ms": chain["created_at_unix_ms"],
        "recovery_epoch": chain["recovery_epoch"],
        "validator_set_id": chain["validator_set_id"],
    }
    for field, wanted in reference_chain.items():
        if reference[field] != wanted:
            fail(f"archive canonical reference {field} differs from the prearchive chain")
    reference_payload = canonical_bytes(reference)
    if objects.get("canonical-reference.json") != (
        sha256_bytes(reference_payload),
        len(reference_payload),
    ):
        fail("canonical-reference.json object differs from its manifest projection")
    options_payload = canonical_bytes(
        {"allow_unbound_legacy_wal": prearchive["archive"]["allow_unbound_legacy_wal"]}
    )
    if objects.get("archive-seal-options.json") != (
        sha256_bytes(options_payload),
        len(options_payload),
    ):
        fail("archive seal options differ from the prearchive WAL policy")

    bundles = archive_manifest["validator_bundles"]
    if not isinstance(bundles, list) or [
        row.get("node") for row in bundles if isinstance(row, dict)
    ] != [name for name, _host in FLEET]:
        fail("archive validator bundles omit, duplicate, or reorder the six-node fleet")
    allowed = {"valid_canonical", "valid_noncanonical_fork", "preserved_unclassified"}
    counts = {classification: 0 for classification in allowed}
    for (node, _host), raw in zip(FLEET, bundles):
        row = require_exact_object(
            raw,
            {"node", "classification", "bundle", "inventory"},
            f"archive bundle {node}",
        )
        if row["node"] != node or row["classification"] not in allowed:
            fail(f"archive bundle {node} identity/classification differs")
        counts[row["classification"]] += 1
        for label, suffix in (("bundle", ".tar.zst"), ("inventory", ".inventory")):
            item = require_exact_object(
                row[label],
                {"name", "size", "sha256", "sidecar_name", "sidecar_sha256"},
                f"archive {node} {label}",
            )
            expected_name = f"legacy-{node}{suffix}"
            if item["name"] != expected_name or item["sidecar_name"] != expected_name + ".sha256":
                fail(f"archive {node} {label} object names are noncanonical")
            size = require_uint(item["size"], f"archive {node} {label} size", positive=True)
            digest = require_hash(item["sha256"], f"archive {node} {label} sha256")
            sidecar_digest = require_hash(
                item["sidecar_sha256"], f"archive {node} {label} sidecar sha256"
            )
            sidecar_size = len(f"{digest}  {expected_name}\n".encode("ascii"))
            for name, value in (
                (expected_name, (digest, size)),
                (expected_name + ".sha256", (sidecar_digest, sidecar_size)),
            ):
                if name in objects:
                    fail("archive object name is duplicated across shared and bundle evidence")
                objects[name] = value
    if archive_manifest["capture_classification_counts"] != counts:
        fail("archive classification counts differ from the six bundle rows")

    try:
        sums_text = sums_payload.decode("ascii")
    except UnicodeDecodeError:
        fail("SHA256SUMS must be ASCII")
    if not sums_text.endswith("\n") or "\r" in sums_text or "\x00" in sums_text:
        fail("SHA256SUMS is not canonical line text")
    sums: dict[str, str] = {}
    for line in sums_text.splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9][A-Za-z0-9._-]{0,127})", line)
        if match is None or match.group(2) in sums:
            fail("SHA256SUMS contains a malformed or duplicate row")
        sums[match.group(2)] = match.group(1)
    if sums != {name: digest for name, (digest, _size) in objects.items()}:
        fail("SHA256SUMS does not exactly cover every shared and bundle object")
    expected_sums_entry = {
        "name": "SHA256SUMS",
        "size": sums_details.st_size,
        "sha256": sums_sha,
    }
    if archive_manifest["sha256sums"] != expected_sums_entry:
        fail("ARCHIVE-MANIFEST.json does not exactly bind SHA256SUMS")
    expected_object_count = len(shared) + 24 + 3
    if complete["validator_bundle_count"] != 6 or complete[
        "object_count_before_complete"
    ] != expected_object_count:
        fail("COMPLETE.json object cardinality differs from the archive manifest")
    all_names = set(objects) | {
        "SHA256SUMS",
        "ARCHIVE-MANIFEST.json",
        "ARCHIVE-MANIFEST.json.sha256",
        "COMPLETE.json",
    }
    if len(all_names) != complete["object_count_before_complete"] + 1:
        fail("archive object names contain a collision or cardinality mismatch")
    if sidecar_details.st_size != len(sidecar_payload) or len(archive_payload) == 0 or len(
        complete_payload
    ) == 0:
        fail("archive metadata size proof is inconsistent")
    return complete_sha, archive_sha, sums_sha


def finalize(args: argparse.Namespace) -> str:
    prearchive, prearchive_payload, prearchive_sha = load_private_rollout(args.prearchive)
    complete_sha, archive_sha, sums_sha = validate_archive_evidence(
        prearchive,
        args.prearchive,
        prearchive_payload,
        prearchive_sha,
        args,
    )
    final_manifest = copy.deepcopy(prearchive)
    final_manifest["archive"].update(
        {
            "complete_sha256": complete_sha,
            "archive_manifest_sha256": archive_sha,
            "sha256sums_sha256": sums_sha,
            "prearchive_rollout_sha256": prearchive_sha,
        }
    )
    try:
        rollout.validate_manifest(final_manifest)
        if rollout.prearchive_projection_digest(final_manifest) != prearchive_sha:
            fail("final rollout does not project to the exact sealed prearchive digest")
        # Reuse the rollout's local archive identity/classification validator in
        # addition to the stricter roots/SHA256SUMS checks above.
        digest = rollout.sha256_bytes(rollout.canonical_bytes(final_manifest))
        checked = rollout.RecoveryRollout(final_manifest, digest)
        rollout.load_legacy_archive_fork_nodes(
            checked,
            args.archive_manifest,
            args.complete,
        )
        rollout.verify_artifacts(final_manifest)
    except rollout.RolloutError as error:
        fail(f"final rollout failed execution/archive validation: {error}")
    changed = []
    for field in rollout.ARCHIVE_FINALIZATION_FIELDS:
        before = prearchive["archive"][field]
        after = final_manifest["archive"][field]
        if before != after:
            changed.append(field)
    if changed != list(rollout.ARCHIVE_FINALIZATION_FIELDS):
        fail("finalization did not change exactly the four archive root fields")
    return create_private_seal(args.output, final_manifest)


def absolute_path(value: str) -> Path:
    path = Path(value)
    if not path.is_absolute():
        raise argparse.ArgumentTypeError("path must be absolute")
    return path


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)
    prepare = commands.add_parser("prearchive", help="derive and seal the prearchive rollout")
    prepare.add_argument("--source-main-sha", required=True)
    prepare.add_argument("--pretag-run-id", required=True, type=int)
    prepare.add_argument("--pretag-run-attempt", required=True, type=int)
    prepare.add_argument("--pretag-artifact-input-set", required=True, type=absolute_path)
    prepare.add_argument("--curl", required=True, type=absolute_path)
    prepare.add_argument("--curl-sha256", required=True)
    prepare.add_argument("--ca-bundle", required=True, type=absolute_path)
    prepare.add_argument("--ca-bundle-sha256", required=True)
    prepare.add_argument("--freeze-plan", required=True, type=absolute_path)
    prepare.add_argument("--freeze-plan-sha256", required=True)
    prepare.add_argument("--legacy-public-height-receipt", required=True, type=absolute_path)
    prepare.add_argument("--legacy-maintenance-evidence-bundle", required=True, type=absolute_path)
    prepare.add_argument("--legacy-maintenance-boundary", required=True, type=absolute_path)
    prepare.add_argument("--legacy-late-fork-source-set", required=True, type=absolute_path)
    prepare.add_argument("--offline-stop-evidence", required=True, type=absolute_path)
    prepare.add_argument("--ssh-known-hosts", required=True, type=absolute_path)
    prepare.add_argument("--ssh-identity", required=True, type=absolute_path)
    prepare.add_argument("--validator-vault-restore-receipt", required=True, type=absolute_path)
    prepare.add_argument("--validator-key-install-receipt", required=True, type=absolute_path)
    prepare.add_argument("--validator-public-keys", required=True, type=absolute_path)
    prepare.add_argument("--legacy-validator-set", required=True, type=absolute_path)
    prepare.add_argument("--checkpoint", required=True, type=absolute_path)
    prepare.add_argument("--source-snapshot", required=True, type=absolute_path)
    prepare.add_argument("--source-wal", required=True, type=absolute_path)
    prepare.add_argument("--caddy", required=True, type=absolute_path)
    prepare.add_argument("--reward-probe", required=True, type=absolute_path)
    prepare.add_argument(
        "--stage-root",
        required=True,
        type=absolute_path,
        help=(
            "new private directory that receives the only semantic input bytes "
            "used by checkpoint reproduction, archive capture, and rollout"
        ),
    )
    prepare.add_argument("--acme-email", default=APPROVED_ACME_EMAIL)
    prepare.add_argument("--output", required=True, type=absolute_path)

    finish = commands.add_parser("finalize", help="authenticate archive roots and seal final rollout")
    finish.add_argument("--prearchive", required=True, type=absolute_path)
    finish.add_argument("--complete", required=True, type=absolute_path)
    finish.add_argument("--complete-sha256", required=True)
    finish.add_argument("--archive-manifest", required=True, type=absolute_path)
    finish.add_argument("--archive-manifest-sidecar", required=True, type=absolute_path)
    finish.add_argument("--archive-manifest-sha256", required=True)
    finish.add_argument("--sha256sums", required=True, type=absolute_path)
    finish.add_argument("--sha256sums-sha256", required=True)
    finish.add_argument(
        "--drive-archive-seal-prefreeze",
        required=True,
        type=absolute_path,
    )
    finish.add_argument(
        "--drive-archive-seal-attempt",
        required=True,
        type=absolute_path,
    )
    finish.add_argument(
        "--github-gist-write-canary",
        required=True,
        type=absolute_path,
    )
    finish.add_argument("--output", required=True, type=absolute_path)
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "prearchive":
            digest = prearchive(args)
            print(
                json.dumps(
                    {
                        "schema": "arc.recovery.production-manifest-build.v1",
                        "phase": "prearchive",
                        "rollout_sha256": digest,
                        "output": os.fspath(args.output),
                    },
                    sort_keys=True,
                    separators=(",", ":"),
                )
            )
        else:
            digest = finalize(args)
            print(
                json.dumps(
                    {
                        "schema": "arc.recovery.production-manifest-build.v1",
                        "phase": "final",
                        "rollout_sha256": digest,
                        "output": os.fspath(args.output),
                    },
                    sort_keys=True,
                    separators=(",", ":"),
                )
            )
        return 0
    except (BuilderError, OSError, ValueError) as error:
        print(f"production manifest builder: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
