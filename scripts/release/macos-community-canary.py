#!/usr/bin/env python3
"""Operate one hash-bound macOS arm64 pre-tag community-worker canary.

The helper deliberately does not download artifacts, contact GitHub, or infer
which preflight run should be trusted.  The operator first materializes the
selected protected-preflight artifact with the repository verifier, then pins
that run's exact commit/run/attempt on this command line.
"""

from __future__ import annotations

import argparse
import contextlib
import fcntl
import functools
import hashlib
import importlib.util
import json
import os
import plistlib
import re
import shlex
import stat
import subprocess
import sys
import threading
import time
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any, NoReturn, Sequence


SCHEMA = "arc.macos.pretag-community-canary.v1"
REPOSITORY = "FerrumVir/arc-chain"
PLATFORM = "macos-arm64"
RUST_TARGET = "aarch64-apple-darwin"
LABEL = "network.arc.pretag-community-canary"
RPC = "127.0.0.1:19944"
STOP_BUDGET_SECONDS = 4_420
START_PROOF_SECONDS = 60
CANONICAL_GENESIS_SHA256 = (
    "8394894aaf32aff64df5c6988186e4802cb77a62daf259d8f5cab11d818ed269"
)
CANONICAL_MODEL_SHA256 = (
    "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa"
)
CANONICAL_MODEL_SIZE_BYTES = 4_081_004_224
COMMUNITY_RPC_URLS = (
    "https://149.28.32.76",
    "https://140.82.16.112",
    "https://136.244.109.1",
    "https://104.238.171.11",
    "https://202.182.107.41",
    "https://149.28.153.31",
)
HEX_40 = re.compile(r"^[0-9a-f]{40}$")
HEX_64 = re.compile(r"^[0-9a-f]{64}$")
SEMVER = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
SAFE_RUNTIME_ARGUMENT = re.compile(r"^/[A-Za-z0-9._/+@:-]+$")
ADDRESS = re.compile(r"^[0-9a-f]{64}$")
FIXED_RUNTIME_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"
RUNNER_ENV_UNSET = (
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_FALLBACK_FRAMEWORK_PATH",
    "DYLD_VERSIONED_LIBRARY_PATH",
    "DYLD_VERSIONED_FRAMEWORK_PATH",
    "DYLD_IMAGE_SUFFIX",
    "DYLD_ROOT_PATH",
    "DYLD_SHARED_REGION",
    "DYLD_FORCE_FLAT_NAMESPACE",
    "DYLD_PRINT_OPTS",
    "DYLD_PRINT_ENV",
    "DYLD_PRINT_LIBRARIES",
    "DYLD_PRINT_LIBRARIES_POST_LAUNCH",
    "DYLD_PRINT_APIS",
    "DYLD_PRINT_BINDINGS",
    "DYLD_PRINT_INITIALIZERS",
    "DYLD_PRINT_REBASINGS",
    "DYLD_PRINT_SEGMENTS",
    "DYLD_PRINT_STATISTICS",
    "DYLD_PRINT_DOFS",
    "DYLD_PRINT_RPATHS",
    "DYLD_PRINT_SEARCHING",
    "DYLD_PRINT_UUIDS",
    "DYLD_PRINT_WARNINGS",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "BASH_ENV",
    "ENV",
    "CDPATH",
    "GLOBIGNORE",
    "IFS",
    "SHELLOPTS",
    "BASHOPTS",
    "ZDOTDIR",
    "PERL5OPT",
    "PERL5LIB",
    "PYTHONHOME",
    "PYTHONPATH",
    "RUBYOPT",
    "RUBYLIB",
    "NODE_OPTIONS",
    "OPENSSL_CONF",
    "OPENSSL_MODULES",
    "OPENSSL_ENGINES",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "SSH_AUTH_SOCK",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
)


class CanaryError(RuntimeError):
    pass


def fail(message: str) -> NoReturn:
    raise CanaryError(message)


_PROVENANCE_PATH = Path(__file__).with_name("protected_pretag_artifact.py")
_PROVENANCE_SPEC = importlib.util.spec_from_file_location(
    "arc_protected_pretag_for_canary", _PROVENANCE_PATH
)
if _PROVENANCE_SPEC is None or _PROVENANCE_SPEC.loader is None:
    raise RuntimeError("cannot load protected pre-tag artifact verifier")
artifact_provenance = importlib.util.module_from_spec(_PROVENANCE_SPEC)
sys.modules[_PROVENANCE_SPEC.name] = artifact_provenance
_PROVENANCE_SPEC.loader.exec_module(artifact_provenance)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_descriptor(descriptor: int) -> str:
    digest = hashlib.sha256()
    os.lseek(descriptor, 0, os.SEEK_SET)
    while chunk := os.read(descriptor, 1024 * 1024):
        digest.update(chunk)
    os.lseek(descriptor, 0, os.SEEK_SET)
    return digest.hexdigest()


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def regular_file(path: Path, description: str) -> os.stat_result:
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        fail(f"{description} is missing: {path}")
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        fail(f"{description} must be a non-symlink regular file: {path}")
    return metadata


def secure_operator_source(path: Path, uid: int, description: str) -> os.stat_result:
    """Reject writable/symlinked ancestry before an operator input is pinned."""

    path = Path(os.path.abspath(path.expanduser()))
    if not path.is_absolute():
        fail(f"{description} path must be absolute: {path}")
    current = Path(path.anchor)
    for part in path.parts[1:-1]:
        current /= part
        try:
            metadata = current.lstat()
        except OSError as error:
            fail(f"{description} ancestor is unavailable: {current}: {error}")
        if (
            stat.S_ISLNK(metadata.st_mode)
            or not stat.S_ISDIR(metadata.st_mode)
            or metadata.st_uid not in (0, uid)
            or stat.S_IMODE(metadata.st_mode) & 0o022
        ):
            fail(
                f"{description} ancestor must be a root/operator-owned, "
                f"non-symlink, non-group/world-writable directory: {current}"
            )
    metadata = regular_file(path, description)
    if (
        metadata.st_uid != uid
        or stat.S_IMODE(metadata.st_mode) & 0o022
        or metadata.st_nlink != 1
    ):
        fail(
            f"{description} must be operator-owned, non-group/world-writable, "
            f"and have exactly one hard link: {path}"
        )
    return metadata


def secure_operator_directory(path: Path, uid: int, description: str) -> Path:
    raw = Path(os.path.abspath(path.expanduser()))
    current = Path(raw.anchor)
    for part in raw.parts[1:]:
        current /= part
        try:
            metadata = current.lstat()
        except OSError as error:
            fail(f"{description} path is unavailable: {current}: {error}")
        if (
            stat.S_ISLNK(metadata.st_mode)
            or not stat.S_ISDIR(metadata.st_mode)
            or metadata.st_uid not in (0, uid)
            or stat.S_IMODE(metadata.st_mode) & 0o022
        ):
            fail(
                f"{description} path must have only root/operator-owned, "
                f"non-symlink, non-group/world-writable directories: {current}"
            )
    return raw


def private_file(path: Path, uid: int, description: str) -> os.stat_result:
    metadata = regular_file(path, description)
    if metadata.st_uid != uid or stat.S_IMODE(metadata.st_mode) != 0o600:
        fail(f"{description} must be owned by uid {uid} with mode 0600: {path}")
    return metadata


def executable_file(path: Path, uid: int, description: str) -> os.stat_result:
    metadata = regular_file(path, description)
    if metadata.st_uid != uid or stat.S_IMODE(metadata.st_mode) != 0o700:
        fail(f"{description} must be owned by uid {uid} with mode 0700: {path}")
    return metadata


def private_directory(path: Path, uid: int, description: str) -> os.stat_result:
    try:
        metadata = path.lstat()
    except OSError as error:
        fail(f"{description} is unavailable: {path}: {error}")
    if (
        stat.S_ISLNK(metadata.st_mode)
        or not stat.S_ISDIR(metadata.st_mode)
        or metadata.st_uid != uid
        or stat.S_IMODE(metadata.st_mode) != 0o700
    ):
        fail(f"{description} must be owned by uid {uid} with mode 0700: {path}")
    return metadata


def require_safe_runtime_path(path: Path, description: str) -> None:
    value = str(path)
    if SAFE_RUNTIME_ARGUMENT.fullmatch(value) is None:
        fail(
            f"{description} must use an absolute path containing only "
            f"A-Z, a-z, 0-9, '.', '_', '/', '+', '@', ':', or '-': {path}"
        )


def fsync_directory(path: Path) -> None:
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def ensure_private_directory(path: Path, uid: int) -> None:
    if path.exists() or path.is_symlink():
        metadata = path.lstat()
        if (
            stat.S_ISLNK(metadata.st_mode)
            or not stat.S_ISDIR(metadata.st_mode)
            or metadata.st_uid != uid
            or stat.S_IMODE(metadata.st_mode) != 0o700
        ):
            fail(f"managed directory must be owned by uid {uid} with mode 0700: {path}")
        return
    path.mkdir(mode=0o700)
    fsync_directory(path.parent)


def ensure_launch_agent_directory(path: Path, uid: int) -> None:
    if path.exists() or path.is_symlink():
        metadata = path.lstat()
        if (
            stat.S_ISLNK(metadata.st_mode)
            or not stat.S_ISDIR(metadata.st_mode)
            or metadata.st_uid != uid
            or stat.S_IMODE(metadata.st_mode) & 0o022
        ):
            fail(
                "LaunchAgents directory must be a non-symlink directory owned by "
                f"uid {uid} and not group/world writable: {path}"
            )
        return
    path.mkdir(mode=0o700, parents=True)
    fsync_directory(path.parent)


def publish_bytes_create_only(path: Path, content: bytes, mode: int, uid: int) -> None:
    expected_hash = sha256_bytes(content)
    if path.exists() or path.is_symlink():
        metadata = regular_file(path, "managed create-only file")
        if (
            metadata.st_uid != uid
            or stat.S_IMODE(metadata.st_mode) != mode
            or metadata.st_size != len(content)
            or sha256(path) != expected_hash
        ):
            fail(f"refusing to replace mismatched create-only file: {path}")
        return

    staging = path.parent / f".{path.name}.new.{os.getpid()}.{time.time_ns()}"
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(staging, flags, mode)
        try:
            offset = 0
            while offset < len(content):
                written = os.write(descriptor, content[offset:])
                if written <= 0:
                    fail(f"create-only publication made no write progress: {path}")
                offset += written
            os.fchmod(descriptor, mode)
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
        os.link(staging, path, follow_symlinks=False)
        fsync_directory(path.parent)
    except FileExistsError:
        fail(f"create-only destination appeared concurrently: {path}")
    finally:
        staging.unlink(missing_ok=True)


def publish_file_create_only(
    source: Path,
    destination: Path,
    mode: int,
    uid: int,
    expected_hash: str,
    expected_size: int,
) -> None:
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        source_descriptor = os.open(source, flags)
    except OSError as error:
        fail(f"candidate source cannot be opened no-follow: {source}: {error}")
    metadata = os.fstat(source_descriptor)
    source_identity = (
        metadata.st_dev,
        metadata.st_ino,
        metadata.st_size,
        metadata.st_mtime_ns,
        metadata.st_ctime_ns,
        stat.S_IMODE(metadata.st_mode),
        metadata.st_nlink,
    )
    if (
        not stat.S_ISREG(metadata.st_mode)
        or metadata.st_size != expected_size
        or sha256_descriptor(source_descriptor) != expected_hash
    ):
        os.close(source_descriptor)
        fail(f"candidate source changed after verification: {source}")
    if destination.exists() or destination.is_symlink():
        os.close(source_descriptor)
        current = regular_file(destination, "managed create-only file")
        if (
            current.st_uid != uid
            or stat.S_IMODE(current.st_mode) != mode
            or current.st_size != expected_size
            or sha256(destination) != expected_hash
        ):
            fail(f"refusing to replace mismatched create-only file: {destination}")
        return

    staging = destination.parent / (
        f".{destination.name}.new.{os.getpid()}.{time.time_ns()}"
    )
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
    try:
        output = os.open(staging, flags, mode)
        try:
            os.lseek(source_descriptor, 0, os.SEEK_SET)
            while chunk := os.read(source_descriptor, 1024 * 1024):
                view = memoryview(chunk)
                while view:
                    written = os.write(output, view)
                    if written <= 0:
                        fail(f"managed file copy made no write progress: {destination}")
                    view = view[written:]
            after = os.fstat(source_descriptor)
            after_identity = (
                after.st_dev,
                after.st_ino,
                after.st_size,
                after.st_mtime_ns,
                after.st_ctime_ns,
                stat.S_IMODE(after.st_mode),
                after.st_nlink,
            )
            if after_identity != source_identity:
                fail(f"candidate source changed during create-only copy: {source}")
            os.fchmod(output, mode)
            os.fsync(output)
        finally:
            os.close(output)
        staged = regular_file(staging, "staged managed file")
        if staged.st_size != expected_size or sha256(staging) != expected_hash:
            fail(f"staged managed file failed its exact hash/size proof: {staging}")
        os.link(staging, destination, follow_symlinks=False)
        fsync_directory(destination.parent)
    except FileExistsError:
        fail(f"create-only destination appeared concurrently: {destination}")
    finally:
        os.close(source_descriptor)
        staging.unlink(missing_ok=True)


class PlatformCommands:
    SYSTEM_TOOLS = {
        "id": Path("/usr/bin/id"),
        "uname": Path("/usr/bin/uname"),
        "launchctl": Path("/bin/launchctl"),
        "ps": Path("/bin/ps"),
        "lsof": Path("/usr/sbin/lsof"),
    }
    RUNNER_SYSTEM_TOOLS = (
        Path("/usr/bin/env"),
        Path("/bin/sh"),
        Path("/usr/bin/stat"),
        Path("/usr/bin/shasum"),
        Path("/usr/bin/cut"),
    )

    def prove_runner_tools(self) -> None:
        for executable in self.RUNNER_SYSTEM_TOOLS:
            try:
                metadata = executable.lstat()
            except OSError as error:
                fail(f"protected canary runner tool is unavailable: {executable}: {error}")
            if (
                stat.S_ISLNK(metadata.st_mode)
                or not stat.S_ISREG(metadata.st_mode)
                or metadata.st_uid != 0
                or stat.S_IMODE(metadata.st_mode) & 0o022
                or not os.access(executable, os.X_OK)
            ):
                fail(f"protected canary runner tool is unsafe: {executable}")

    def run(self, argv: Sequence[str], *, check: bool = True) -> subprocess.CompletedProcess[str]:
        if not argv:
            fail("empty platform command is forbidden")
        selected = str(argv[0])
        if selected in self.SYSTEM_TOOLS:
            executable = self.SYSTEM_TOOLS[selected]
            try:
                metadata = executable.lstat()
            except OSError as error:
                fail(f"protected macOS system tool is unavailable: {executable}: {error}")
            if (
                stat.S_ISLNK(metadata.st_mode)
                or not stat.S_ISREG(metadata.st_mode)
                or metadata.st_uid != 0
                or stat.S_IMODE(metadata.st_mode) & 0o022
                or not os.access(executable, os.X_OK)
            ):
                fail(f"protected macOS system tool is unsafe: {executable}")
        else:
            executable = Path(selected)
            if not executable.is_absolute():
                fail(f"unmapped non-absolute platform command is forbidden: {selected}")
        command = [str(executable), *(str(value) for value in argv[1:])]
        result = subprocess.run(
            command,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env={
                "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
                "LANG": "C",
                "LC_ALL": "C",
            },
        )
        if check and result.returncode != 0:
            rendered = " ".join(shlex.quote(part) for part in command)
            detail = result.stderr.strip() or result.stdout.strip() or "no diagnostic"
            fail(f"platform command failed ({rendered}): {detail}")
        return result

    def sleep(self, seconds: float) -> None:
        time.sleep(seconds)


@dataclass(frozen=True)
class Paths:
    root: Path
    bin_dir: Path
    config_dir: Path
    identity_dir: Path
    model_dir: Path
    data_dir: Path
    evidence_dir: Path
    logs_dir: Path
    tmp_dir: Path
    node: Path
    cli: Path
    genesis: Path
    key: Path
    model: Path
    runner: Path
    metadata: Path
    provenance_receipt: Path
    provenance_recheck: Path
    config: Path
    config_checksum: Path
    acceptance_receipt: Path
    log: Path
    launch_agent: Path
    lifecycle_lock: Path


@dataclass(frozen=True)
class Candidate:
    directory: Path
    metadata_path: Path
    metadata: dict[str, Any]
    metadata_bytes: bytes
    provenance_receipt_bytes: bytes
    artifact_id: int
    artifact_digest: str
    archive_sha256: str
    node: Path
    cli: Path
    genesis: Path
    node_sha256: str
    node_size: int
    cli_sha256: str
    cli_size: int
    genesis_sha256: str
    genesis_size: int


def managed_paths(root: Path, home: Path) -> Paths:
    home = home.expanduser().resolve(strict=True)
    raw_root = Path(os.path.abspath(root.expanduser()))
    if raw_root.is_symlink():
        fail("the dedicated canary root must not be a symlink")
    try:
        root_parent = raw_root.parent.resolve(strict=True)
    except FileNotFoundError:
        fail("the dedicated canary root parent must already exist")
    if root_parent != home:
        fail("the dedicated canary root must be one direct child directory of HOME")
    root = root_parent / raw_root.name
    return Paths(
        root=root,
        bin_dir=root / "bin",
        config_dir=root / "config",
        identity_dir=root / "identity",
        model_dir=root / "model",
        data_dir=root / "data",
        evidence_dir=root / "evidence",
        logs_dir=root / "logs",
        tmp_dir=root / "tmp",
        node=root / "bin/arc-node-macos-arm64",
        cli=root / "bin/arc-cli-macos-arm64",
        genesis=root / "config/genesis.toml",
        key=root / "identity/community-worker-ed25519.json",
        model=root / "model/llama-2-7b-chat.Q4_K_M.gguf",
        runner=root / "bin/run-community-canary",
        metadata=root / "config/BUILD-METADATA.json",
        provenance_receipt=root / "config/LIVE-PROVENANCE.json",
        provenance_recheck=root / "config/LIVE-RECHECK.json",
        config=root / "config/canary.json",
        config_checksum=root / "config/canary.json.sha256",
        acceptance_receipt=root / "evidence/ACCEPTED.json",
        log=root / "logs/node.log",
        launch_agent=home / f"Library/LaunchAgents/{LABEL}.plist",
        # This lock is deliberately outside the managed root and is never
        # removed by cleanup.  Every mutating/lifecycle command therefore
        # contends on the same inode even while the LaunchAgent plist is being
        # removed.
        lifecycle_lock=home / f".{LABEL}.lifecycle.lock",
    )


def validate_genesis(path: Path) -> None:
    try:
        document = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        fail(f"candidate genesis is not valid UTF-8 TOML: {error}")
    if not isinstance(document, dict) or set(document) != {"chain", "accounts", "validators"}:
        fail("candidate genesis must contain exactly chain, accounts, and validators")
    chain = document.get("chain")
    if not isinstance(chain, dict) or set(chain) != {
        "name",
        "chain_id",
        "validator_set_complete",
        "community_rewards_v1_activation_height",
    }:
        fail("candidate genesis chain table is incomplete or contains unknown fields")
    if chain.get("validator_set_complete") is not True:
        fail("candidate genesis validator_set_complete must be true")
    if chain.get("community_rewards_v1_activation_height") != 137_146:
        fail("candidate genesis must carry recovered reward activation height 137146")
    accounts = document.get("accounts")
    validators = document.get("validators")
    if not isinstance(accounts, list) or not isinstance(validators, list):
        fail("candidate genesis accounts and validators must be arrays")
    if len(validators) != 6:
        fail("candidate genesis must contain the complete six-validator set")
    account_addresses: set[str] = set()
    for account in accounts:
        if not isinstance(account, dict) or set(account) != {"address", "balance"}:
            fail("candidate genesis contains a malformed account")
        address = account.get("address")
        balance = account.get("balance")
        if not isinstance(address, str) or ADDRESS.fullmatch(address) is None:
            fail("candidate genesis contains an invalid public account address")
        if isinstance(balance, bool) or not isinstance(balance, int) or balance < 0:
            fail("candidate genesis contains an invalid account balance")
        if address in account_addresses:
            fail("candidate genesis contains a duplicate account address")
        account_addresses.add(address)
    validator_addresses: set[str] = set()
    for validator in validators:
        if not isinstance(validator, dict) or set(validator) != {"address", "stake"}:
            fail("candidate genesis contains a malformed validator")
        address = validator.get("address")
        stake = validator.get("stake")
        if (
            not isinstance(address, str)
            or ADDRESS.fullmatch(address) is None
            or address not in account_addresses
        ):
            fail("candidate genesis validator lacks a matching public account")
        if isinstance(stake, bool) or not isinstance(stake, int) or stake <= 0:
            fail("candidate genesis validator stake must be positive")
        if address in validator_addresses:
            fail("candidate genesis contains a duplicate validator address")
        validator_addresses.add(address)


def validate_full_live_provenance(
    value: object,
    raw: bytes,
    *,
    expected_commit: str,
    expected_run_id: int,
    expected_run_attempt: int,
    expected_artifact_id: int,
    expected_version: str,
    expected_metadata_sha256: str,
    expected_files: dict[str, str],
    phase: str,
) -> dict[str, Any]:
    """Validate the complete canonical single-artifact public-API proof."""

    if (
        not isinstance(value, dict)
        or raw != canonical_json(value)
        or set(value) != {"schema", "live", "api", "artifact"}
        or value.get("schema") != artifact_provenance.PROVENANCE_SCHEMA
    ):
        fail(f"{phase} live provenance is not one canonical full proof")
    live = value.get("live")
    api = value.get("api")
    artifact = value.get("artifact")
    live_fields = {
        "repository",
        "protected_branch",
        "commit",
        "workflow_id",
        "workflow_path",
        "run_id",
        "run_attempt",
        "artifact_id",
        "artifact_name",
        "artifact_digest",
        "artifact_size_in_bytes",
        "api_verified_at_unix",
    }
    api_fields = {
        "origin",
        "anonymous",
        "redirects_followed",
        "max_age_seconds",
        "curl_sha256",
        "ca_bundle_sha256",
        "responses",
    }
    artifact_fields = {
        "kind",
        "platform",
        "version",
        "raw_actions_zip_sha256",
        "raw_actions_zip_size",
        "archive_sha256",
        "build_metadata_sha256",
        "files",
    }
    if (
        not isinstance(live, dict)
        or set(live) != live_fields
        or not isinstance(api, dict)
        or set(api) != api_fields
        or not isinstance(artifact, dict)
        or set(artifact) != artifact_fields
    ):
        fail(f"{phase} live provenance has missing or unknown fields")
    for key in ("workflow_id", "run_id", "run_attempt", "artifact_id"):
        if isinstance(live[key], bool) or not isinstance(live[key], int) or live[key] <= 0:
            fail(f"{phase} live provenance {key} is not a positive integer")
    for key in ("artifact_size_in_bytes", "api_verified_at_unix"):
        if isinstance(live[key], bool) or not isinstance(live[key], int) or live[key] <= 0:
            fail(f"{phase} live provenance {key} is invalid")
    for key in ("curl_sha256", "ca_bundle_sha256"):
        if not isinstance(api[key], str) or HEX_64.fullmatch(api[key]) is None:
            fail(f"{phase} live provenance {key} is malformed")
    for key in (
        "raw_actions_zip_sha256",
        "archive_sha256",
        "build_metadata_sha256",
    ):
        if not isinstance(artifact[key], str) or HEX_64.fullmatch(artifact[key]) is None:
            fail(f"{phase} live provenance {key} is malformed")
    archive_sha256 = artifact["archive_sha256"]
    prefix = (
        f"arc-pretag-headless-{PLATFORM}-{expected_commit}-"
        f"{expected_run_id}-{expected_run_attempt}-"
    )
    if (
        live["repository"] != REPOSITORY
        or live["protected_branch"] != "main"
        or live["commit"] != expected_commit
        or live["workflow_path"] != artifact_provenance.WORKFLOW_PATH
        or live["run_id"] != expected_run_id
        or live["run_attempt"] != expected_run_attempt
        or live["artifact_id"] != expected_artifact_id
        or live["artifact_name"] != prefix + archive_sha256
        or live["artifact_digest"] != "sha256:" + artifact["raw_actions_zip_sha256"]
        or live["artifact_size_in_bytes"] != artifact["raw_actions_zip_size"]
        or api["origin"] != artifact_provenance.API_ORIGIN
        or api["anonymous"] is not True
        or api["redirects_followed"] is not False
        or api["max_age_seconds"] != artifact_provenance.MAX_API_AGE_SECONDS
        or artifact["kind"] != "headless"
        or artifact["platform"] != PLATFORM
        or artifact["version"] != expected_version
        or artifact["build_metadata_sha256"] != expected_metadata_sha256
        or artifact["files"] != expected_files
        or isinstance(artifact["raw_actions_zip_size"], bool)
        or not isinstance(artifact["raw_actions_zip_size"], int)
        or artifact["raw_actions_zip_size"] <= 0
    ):
        fail(f"{phase} live provenance differs from the exact canary artifact")
    responses = api["responses"]
    if (
        not isinstance(responses, list)
        or len(responses) != 4
        or [row.get("label") for row in responses if isinstance(row, dict)]
        != ["workflow", "run", "artifact", "protected_main"]
    ):
        fail(f"{phase} live provenance API response set differs")
    response_times: list[int] = []
    for row in responses:
        if not isinstance(row, dict) or set(row) != {
            "label",
            "body_sha256",
            "response_unix",
            "request_id",
            "cache_control",
            "age",
        }:
            fail(f"{phase} live provenance API response shape differs")
        if (
            not isinstance(row["body_sha256"], str)
            or HEX_64.fullmatch(row["body_sha256"]) is None
            or isinstance(row["response_unix"], bool)
            or not isinstance(row["response_unix"], int)
            or row["response_unix"] <= 0
            or not isinstance(row["request_id"], str)
            or re.fullmatch(r"[A-F0-9:-]{8,128}", row["request_id"]) is None
            or not isinstance(row["cache_control"], str)
            or isinstance(row["age"], bool)
            or row["age"] != 0
        ):
            fail(f"{phase} live provenance API response is malformed or cached")
        response_times.append(row["response_unix"])
    if live["api_verified_at_unix"] != min(response_times):
        fail(f"{phase} live provenance timestamp does not cover every API response")
    return value


def live_provenance_invariant(value: dict[str, Any]) -> tuple[object, object, object]:
    """Return the immutable tuple shared by initial, final, and retry proofs."""

    return (
        {
            key: item
            for key, item in value["live"].items()
            if key != "api_verified_at_unix"
        },
        value["artifact"],
        {
            key: item
            for key, item in value["api"].items()
            if key != "responses"
        },
    )


def require_same_live_provenance_invariant(
    first: dict[str, Any], second: dict[str, Any]
) -> None:
    if live_provenance_invariant(first) != live_provenance_invariant(second):
        fail("initial/final live protected preflight provenance invariants differ")


def require_ordered_fresh_live_provenance_pair(
    initial: dict[str, Any], final: dict[str, Any]
) -> None:
    """Prove the final API observation is a distinct, non-earlier recheck.

    GitHub's authoritative Date header has one-second resolution, so equality
    at the boundary is valid. Reusing any request identity is not: every GET in
    the final proof must be a fresh request made after the initial sequence.
    """

    require_same_live_provenance_invariant(initial, final)
    initial_responses = initial["api"]["responses"]
    final_responses = final["api"]["responses"]
    initial_times = [row["response_unix"] for row in initial_responses]
    final_times = [row["response_unix"] for row in final_responses]
    if min(final_times) < max(initial_times):
        fail("final live protected preflight proof predates the initial proof")
    initial_requests = [row["request_id"] for row in initial_responses]
    final_requests = [row["request_id"] for row in final_responses]
    if (
        len(set(initial_requests)) != len(initial_requests)
        or len(set(final_requests)) != len(final_requests)
        or set(initial_requests) & set(final_requests)
        or initial_responses == final_responses
    ):
        fail("final live protected preflight proof did not use fresh API requests")


def load_candidate(
    proof: Any,
    expected_commit: str,
    expected_run_id: int,
    expected_run_attempt: int,
    expected_artifact_id: int,
    uid: int,
) -> Candidate:
    directory = proof.payload_root
    metadata_path = proof.build_metadata_path
    metadata_bytes = metadata_path.read_bytes()
    if len(metadata_bytes) > 64 * 1024:
        fail("preflight build metadata exceeds 64 KiB")
    try:
        metadata = json.loads(metadata_bytes)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"preflight build metadata is invalid: {error}")
    expected_fields = {
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
    if not isinstance(metadata, dict) or set(metadata) != expected_fields:
        fail("preflight build metadata has an unexpected field set")
    exact = {
        "schema": "arc.pretag.artifact.v1",
        "kind": "headless",
        "repository": REPOSITORY,
        "commit": expected_commit,
        "platform": PLATFORM,
        "rust_target": RUST_TARGET,
        "workflow_run_id": expected_run_id,
        "workflow_run_attempt": expected_run_attempt,
    }
    for key, value in exact.items():
        if metadata.get(key) != value:
            fail(f"preflight build metadata {key} differs from the operator pin")
    if HEX_40.fullmatch(expected_commit) is None:
        fail("expected commit must be one full lowercase Git SHA")
    if expected_run_id <= 0 or expected_run_attempt <= 0:
        fail("expected preflight run ID and attempt must be positive")
    if not isinstance(metadata.get("version"), str) or SEMVER.fullmatch(metadata["version"]) is None:
        fail("preflight build metadata version is not strict semantic versioning")
    files = metadata.get("files")
    expected_payload_names = {
        "arc-node-macos-arm64",
        "arc-cli-macos-arm64",
        "genesis.toml",
    }
    if not isinstance(files, dict) or set(files) != expected_payload_names:
        fail("preflight build metadata payload membership differs")
    for name, digest in files.items():
        if not isinstance(digest, str) or HEX_64.fullmatch(digest) is None:
            fail(f"preflight build metadata has an invalid SHA-256 for {name}")

    receipt_bytes = proof.provenance_bytes
    receipt = proof.provenance
    if (
        isinstance(expected_artifact_id, bool)
        or not isinstance(expected_artifact_id, int)
        or expected_artifact_id <= 0
    ):
        fail("explicit preflight artifact ID pin is malformed")
    live = receipt.get("live") if isinstance(receipt, dict) else None
    artifact = receipt.get("artifact") if isinstance(receipt, dict) else None
    if (
        receipt.get("schema") != artifact_provenance.PROVENANCE_SCHEMA
        or not isinstance(live, dict)
        or live.get("repository") != REPOSITORY
        or live.get("protected_branch") != "main"
        or live.get("commit") != expected_commit
        or live.get("run_id") != expected_run_id
        or live.get("run_attempt") != expected_run_attempt
        or live.get("artifact_id") != expected_artifact_id
        or not isinstance(live.get("artifact_digest"), str)
        or re.fullmatch(r"sha256:[0-9a-f]{64}", live["artifact_digest"]) is None
        or not isinstance(live.get("artifact_size_in_bytes"), int)
        or live["artifact_size_in_bytes"] <= 0
        or not isinstance(artifact, dict)
        or artifact.get("kind") != "headless"
        or artifact.get("platform") != PLATFORM
        or artifact.get("version") != metadata["version"]
        or artifact.get("build_metadata_sha256") != sha256_bytes(metadata_bytes)
        or artifact.get("files") != files
        or not isinstance(artifact.get("archive_sha256"), str)
        or HEX_64.fullmatch(artifact["archive_sha256"]) is None
    ):
        fail("live protected preflight proof differs from the exact canary artifact")
    validate_full_live_provenance(
        receipt,
        receipt_bytes,
        expected_commit=expected_commit,
        expected_run_id=expected_run_id,
        expected_run_attempt=expected_run_attempt,
        expected_artifact_id=expected_artifact_id,
        expected_version=metadata["version"],
        expected_metadata_sha256=sha256_bytes(metadata_bytes),
        expected_files=files,
        phase="initial",
    )

    node = proof.payloads["arc-node-macos-arm64"]
    cli = proof.payloads["arc-cli-macos-arm64"]
    genesis = proof.payloads["genesis.toml"]
    values: dict[str, tuple[str, int]] = {}
    for name, path in ((node.name, node), (cli.name, cli), (genesis.name, genesis)):
        file_metadata = regular_file(path, f"private staged preflight payload {name}")
        if (
            file_metadata.st_uid != uid
            or file_metadata.st_nlink != 1
            or stat.S_IMODE(file_metadata.st_mode) not in (0o400, 0o500)
        ):
            fail(f"private staged preflight payload has an unsafe owner/mode/link: {name}")
        digest = sha256(path)
        if file_metadata.st_size <= 0 or digest != files[name]:
            fail(f"preflight payload hash/size proof failed: {name}")
        values[name] = (digest, file_metadata.st_size)
    if values[genesis.name][0] != CANONICAL_GENESIS_SHA256:
        fail("preflight genesis is not the reviewed recovered ARC genesis")
    validate_genesis(genesis)
    return Candidate(
        directory=directory,
        metadata_path=metadata_path,
        metadata=metadata,
        metadata_bytes=metadata_bytes,
        provenance_receipt_bytes=receipt_bytes,
        artifact_id=expected_artifact_id,
        artifact_digest=live["artifact_digest"],
        archive_sha256=artifact["archive_sha256"],
        node=node,
        cli=cli,
        genesis=genesis,
        node_sha256=values[node.name][0],
        node_size=values[node.name][1],
        cli_sha256=values[cli.name][0],
        cli_size=values[cli.name][1],
        genesis_sha256=values[genesis.name][0],
        genesis_size=values[genesis.name][1],
    )


def validate_model(path: Path, uid: int) -> tuple[Path, str, int]:
    path = Path(os.path.abspath(path.expanduser()))
    metadata = secure_operator_source(path, uid, "canonical local GGUF")
    if metadata.st_size != CANONICAL_MODEL_SIZE_BYTES:
        fail(
            "canonical local GGUF size mismatch: expected "
            f"{CANONICAL_MODEL_SIZE_BYTES}, got {metadata.st_size}"
        )
    digest = sha256(path)
    if digest != CANONICAL_MODEL_SHA256:
        fail(
            "canonical local GGUF SHA-256 mismatch: expected "
            f"{CANONICAL_MODEL_SHA256}, got {digest}"
        )
    require_safe_runtime_path(path, "canonical local GGUF")
    return path, digest, metadata.st_size


def expected_argv(paths: Paths) -> list[str]:
    argv = [
        str(paths.node),
        "--rpc",
        RPC,
        "--p2p-port",
        "0",
        "--eth-rpc-port",
        "0",
        "--stake",
        "0",
        "--community-mode",
        "--full-integer-worker",
        "--node-name",
        "macos-arm64-pretag-canary",
        "--data-dir",
        str(paths.data_dir),
        "--genesis",
        str(paths.genesis),
        "--model",
        str(paths.model),
        "--validator-key-file",
        str(paths.key),
    ]
    for url in COMMUNITY_RPC_URLS:
        argv.extend(("--community-rpc-url", url))
    return argv


def runner_bytes(
    paths: Paths,
    argv: Sequence[str],
    *,
    uid: int,
    node_sha256: str,
    node_size: int,
    genesis_sha256: str,
    genesis_size: int,
) -> bytes:
    checks = (
        (paths.node, 0o700, node_size, node_sha256),
        (paths.genesis, 0o600, genesis_size, genesis_sha256),
        (
            paths.model,
            0o400,
            CANONICAL_MODEL_SIZE_BYTES,
            CANONICAL_MODEL_SHA256,
        ),
    )
    fixed_environment = {
        "HOME": str(paths.root.parent),
        "TMPDIR": str(paths.tmp_dir),
        "PATH": FIXED_RUNTIME_PATH,
        "LANG": "C",
        "LC_ALL": "C",
        "RUST_LOG": "arc=info",
    }
    lines = [
        "#!/bin/sh",
        "set -eu",
        "umask 077",
        "unset " + " ".join(RUNNER_ENV_UNSET) + " 2>/dev/null || :",
        *(f"export {name}={shlex.quote(value)}" for name, value in fixed_environment.items()),
        f"test -d {shlex.quote(str(paths.tmp_dir))} && "
        f"test ! -L {shlex.quote(str(paths.tmp_dir))}",
        f"test \"$(/usr/bin/stat -f '%u:%Lp' {shlex.quote(str(paths.tmp_dir))})\" = "
        f"{shlex.quote(f'{uid}:700')}",
    ]
    for index, (path, mode, size, digest) in enumerate(checks, start=1):
        quoted = shlex.quote(str(path))
        lines.extend(
            (
                f"test -f {quoted} && test ! -L {quoted}",
                f"test \"$(/usr/bin/stat -f '%u:%Lp:%l:%z' {quoted})\" = "
                f"{shlex.quote(f'{uid}:{mode:o}:1:{size}')}",
                f"arc_canary_hash_{index}=$(/usr/bin/shasum -a 256 {quoted} "
                "| /usr/bin/cut -d ' ' -f 1)",
                f"test \"$arc_canary_hash_{index}\" = {digest}",
            )
        )
    lines.append(f"exec {shlex.join(argv)}")
    return (
        "\n".join(lines) + "\n"
    ).encode("utf-8")


def plist_bytes(paths: Paths) -> bytes:
    clean_environment = (
        f"HOME={paths.root.parent}",
        f"TMPDIR={paths.tmp_dir}",
        f"PATH={FIXED_RUNTIME_PATH}",
        "LANG=C",
        "LC_ALL=C",
        "RUST_LOG=arc=info",
    )
    value = {
        "Label": LABEL,
        # /usr/bin/env is SIP-protected on the supported macOS host. Clearing
        # the inherited launchd environment before /bin/sh interprets the
        # runner prevents BASH_ENV/ENV and DYLD interposition from running
        # before the script's own defensive unset contract.
        "ProgramArguments": [
            "/usr/bin/env",
            "-i",
            *clean_environment,
            str(paths.runner),
        ],
        "RunAtLoad": False,
        "KeepAlive": False,
        "ProcessType": "Background",
        "ThrottleInterval": 5,
        "ExitTimeOut": STOP_BUDGET_SECONDS,
        "WorkingDirectory": str(paths.root),
        "StandardOutPath": str(paths.log),
        "StandardErrorPath": str(paths.log),
        "Umask": 0o077,
    }
    return plistlib.dumps(value, fmt=plistlib.FMT_XML, sort_keys=True)


def canonical_json(value: dict[str, Any]) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


def lifecycle_transaction(method):
    @functools.wraps(method)
    def wrapped(self: "CanaryController", *args, **kwargs):
        with self._lifecycle_transaction():
            return method(self, *args, **kwargs)

    return wrapped


class CanaryController:
    def __init__(
        self,
        root: Path,
        platform_commands: PlatformCommands | None = None,
        *,
        home: Path | None = None,
        stop_budget_seconds: int = STOP_BUDGET_SECONDS,
        start_proof_seconds: int = START_PROOF_SECONDS,
    ) -> None:
        self.platform = platform_commands or PlatformCommands()
        self.home = (home or Path.home()).expanduser().absolute()
        self.paths = managed_paths(root, self.home)
        self.stop_budget_seconds = stop_budget_seconds
        self.start_proof_seconds = start_proof_seconds
        self.uid = self._platform_uid()
        self.domain = f"gui/{self.uid}"
        self._lifecycle_lock_descriptor: int | None = None
        self._lifecycle_lock_owner: int | None = None

    def _platform_uid(self) -> int:
        result = self.platform.run(("id", "-u"))
        value = result.stdout.strip()
        if not value.isdigit() or int(value) <= 0:
            fail("the canary requires a non-root interactive user")
        uid = int(value)
        if uid != os.getuid():
            fail("platform uid differs from the process owner")
        return uid

    @contextlib.contextmanager
    def _lifecycle_transaction(self):
        owner = threading.get_ident()
        if (
            self._lifecycle_lock_descriptor is not None
            and self._lifecycle_lock_owner == owner
        ):
            yield
            return
        path = self.paths.lifecycle_lock
        parent = path.parent
        parent_metadata = parent.lstat()
        if (
            stat.S_ISLNK(parent_metadata.st_mode)
            or not stat.S_ISDIR(parent_metadata.st_mode)
            or parent_metadata.st_uid != self.uid
            or stat.S_IMODE(parent_metadata.st_mode) & 0o022
        ):
            fail("canary lifecycle-lock parent is not a secure operator directory")
        flags = (
            os.O_RDWR
            | getattr(os, "O_NOFOLLOW", 0)
            | getattr(os, "O_CLOEXEC", 0)
        )
        created = False
        try:
            descriptor = os.open(path, flags | os.O_CREAT | os.O_EXCL, 0o600)
            created = True
        except FileExistsError:
            try:
                descriptor = os.open(path, flags)
            except OSError as error:
                fail(f"canary lifecycle lock cannot be opened no-follow: {error}")
        try:
            if created:
                os.fchmod(descriptor, 0o600)
            before = os.fstat(descriptor)
            if (
                not stat.S_ISREG(before.st_mode)
                or before.st_uid != self.uid
                or stat.S_IMODE(before.st_mode) != 0o600
                or before.st_nlink != 1
            ):
                fail("canary lifecycle lock has an unsafe owner/mode/type/link count")
            fcntl.flock(descriptor, fcntl.LOCK_EX)
            after = os.fstat(descriptor)
            lexical = path.lstat()
            if (
                (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino)
                or (after.st_dev, after.st_ino) != (lexical.st_dev, lexical.st_ino)
                or lexical.st_uid != self.uid
                or stat.S_IMODE(lexical.st_mode) != 0o600
                or lexical.st_nlink != 1
            ):
                fail("canary lifecycle lock identity changed during acquisition")
            if created:
                fsync_directory(parent)
            self._lifecycle_lock_descriptor = descriptor
            self._lifecycle_lock_owner = owner
            try:
                yield
            finally:
                self._lifecycle_lock_descriptor = None
                self._lifecycle_lock_owner = None
                fcntl.flock(descriptor, fcntl.LOCK_UN)
        finally:
            os.close(descriptor)

    def check_platform(self) -> None:
        system = self.platform.run(("uname", "-s")).stdout.strip()
        machine = self.platform.run(("uname", "-m")).stdout.strip()
        if system != "Darwin" or machine != "arm64":
            fail(
                "this canary accepts only native macOS arm64; got "
                f"system={system!r}, machine={machine!r}"
            )
        self.platform.prove_runner_tools()
        for path, description in (
            (self.paths.root, "canary root"),
            (self.paths.node, "managed node"),
            (self.paths.cli, "managed CLI"),
            (self.paths.genesis, "managed genesis"),
            (self.paths.key, "dedicated keyfile"),
            (self.paths.model, "managed canonical GGUF"),
            (self.paths.data_dir, "canary data directory"),
            (self.paths.runner, "managed runner"),
            (self.paths.log, "node log"),
            (self.paths.tmp_dir, "private runtime temporary directory"),
        ):
            require_safe_runtime_path(path, description)

    def _prepare_directories(self) -> None:
        root_parent = self.paths.root.parent
        if not root_parent.is_dir() or root_parent.is_symlink():
            fail(f"canary root parent must be an existing non-symlink directory: {root_parent}")
        ensure_private_directory(self.paths.root, self.uid)
        for path in (
            self.paths.bin_dir,
            self.paths.config_dir,
            self.paths.identity_dir,
            self.paths.model_dir,
            self.paths.data_dir,
            self.paths.evidence_dir,
            self.paths.logs_dir,
            self.paths.tmp_dir,
        ):
            ensure_private_directory(path, self.uid)
        ensure_launch_agent_directory(self.home / "Library", self.uid)
        ensure_launch_agent_directory(self.paths.launch_agent.parent, self.uid)

    def _validate_key(self) -> str:
        metadata = private_file(
            self.paths.key, self.uid, "dedicated community-worker keyfile"
        )
        if metadata.st_nlink != 1:
            fail("dedicated community-worker keyfile must have exactly one hard link")
        result = self.platform.run(
            (str(self.paths.cli), "keygen", "--verify-keyfile", str(self.paths.key))
        )
        address = result.stdout.strip()
        if ADDRESS.fullmatch(address) is None:
            fail("exact preflight CLI returned an invalid public keyfile address")
        return address

    def _ensure_key(self) -> str:
        if not self.paths.key.exists() and not self.paths.key.is_symlink():
            self.platform.run(
                (
                    str(self.paths.cli),
                    "keygen",
                    "--scheme",
                    "ed25519",
                    "--output",
                    str(self.paths.key),
                )
            )
        address = self._validate_key()
        fsync_directory(self.paths.identity_dir)
        return address

    def _retained_provenance(
        self,
        path: Path,
        candidate: Candidate,
        *,
        phase: str,
    ) -> tuple[dict[str, Any], bytes] | None:
        if not path.exists() and not path.is_symlink():
            return None
        private_file(path, self.uid, f"retained {phase} live provenance")
        raw = path.read_bytes()
        if len(raw) > 256 * 1024:
            fail(f"retained {phase} live provenance is oversized")
        try:
            value = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            fail(f"retained {phase} live provenance is invalid: {error}")
        value = validate_full_live_provenance(
            value,
            raw,
            expected_commit=candidate.metadata["commit"],
            expected_run_id=candidate.metadata["workflow_run_id"],
            expected_run_attempt=candidate.metadata["workflow_run_attempt"],
            expected_artifact_id=candidate.artifact_id,
            expected_version=candidate.metadata["version"],
            expected_metadata_sha256=sha256_bytes(candidate.metadata_bytes),
            expected_files=candidate.metadata["files"],
            phase=f"retained {phase}",
        )
        return value, raw

    @contextlib.contextmanager
    def _verified_plan(
        self,
        raw_actions_zip: Path,
        model_path: Path,
        expected_commit: str,
        expected_run_id: int,
        expected_run_attempt: int,
        expected_artifact_id: int,
        curl: Path,
        curl_sha256: str,
        ca_bundle: Path,
        ca_bundle_sha256: str,
    ) -> Any:
        self.check_platform()
        try:
            with artifact_provenance.pretag_actions_proof(
                raw_actions_zip=raw_actions_zip,
                expected_commit=expected_commit,
                expected_run_id=expected_run_id,
                expected_run_attempt=expected_run_attempt,
                expected_artifact_id=expected_artifact_id,
                kind="headless",
                platform=PLATFORM,
                expected_version="0.8.0",
                curl=curl,
                curl_sha256=curl_sha256,
                ca_bundle=ca_bundle,
                ca_bundle_sha256=ca_bundle_sha256,
            ) as proof:
                candidate = load_candidate(
                    proof,
                    expected_commit,
                    expected_run_id,
                    expected_run_attempt,
                    expected_artifact_id,
                    self.uid,
                )
                model, model_hash, model_size = validate_model(model_path, self.uid)
                yield {
                    "candidate": candidate,
                    "proof": proof,
                    "model": model,
                    "model_hash": model_hash,
                    "model_size": model_size,
                    "argv": expected_argv(self.paths),
                }
        except artifact_provenance.ProvenanceError as error:
            fail(f"live protected preflight verification failed: {error}")

    def plan(
        self,
        raw_actions_zip: Path,
        model_path: Path,
        expected_commit: str,
        expected_run_id: int,
        expected_run_attempt: int,
        expected_artifact_id: int,
        curl: Path,
        curl_sha256: str,
        ca_bundle: Path,
        ca_bundle_sha256: str,
    ) -> None:
        with self._verified_plan(
            raw_actions_zip,
            model_path,
            expected_commit,
            expected_run_id,
            expected_run_attempt,
            expected_artifact_id,
            curl,
            curl_sha256,
            ca_bundle,
            ca_bundle_sha256,
        ) as values:
            candidate: Candidate = values["candidate"]
            print(f"Preflight: {candidate.metadata['commit']} run {expected_run_id}/{expected_run_attempt}")
            print(f"Node:      sha256={candidate.node_sha256} bytes={candidate.node_size}")
            print(f"Genesis:   sha256={candidate.genesis_sha256} bytes={candidate.genesis_size}")
            print(f"Model:     sha256={values['model_hash']} bytes={values['model_size']}")
            print(f"Root:      {self.paths.root}")
            print(f"RPC:       http://{RPC} (loopback only)")
            print("Role:      stake 0, community mode, full integer worker, no P2P peers")
            print("Install does not bootstrap or start the LaunchAgent.")

    @lifecycle_transaction
    def install(
        self,
        raw_actions_zip: Path,
        model_path: Path,
        expected_commit: str,
        expected_run_id: int,
        expected_run_attempt: int,
        expected_artifact_id: int,
        curl: Path,
        curl_sha256: str,
        ca_bundle: Path,
        ca_bundle_sha256: str,
    ) -> None:
        with self._verified_plan(
            raw_actions_zip,
            model_path,
            expected_commit,
            expected_run_id,
            expected_run_attempt,
            expected_artifact_id,
            curl,
            curl_sha256,
            ca_bundle,
            ca_bundle_sha256,
        ) as values:
            self._install_verified(values)

    def _install_verified(self, values: dict[str, Any]) -> None:
        candidate: Candidate = values["candidate"]
        model: Path = values["model"]
        argv: list[str] = values["argv"]
        if self.is_loaded():
            fail("refusing install while the canary LaunchAgent is loaded")
        self._prepare_directories()
        publish_file_create_only(
            candidate.node,
            self.paths.node,
            0o700,
            self.uid,
            candidate.node_sha256,
            candidate.node_size,
        )
        publish_file_create_only(
            candidate.cli,
            self.paths.cli,
            0o700,
            self.uid,
            candidate.cli_sha256,
            candidate.cli_size,
        )
        publish_file_create_only(
            candidate.genesis,
            self.paths.genesis,
            0o600,
            self.uid,
            candidate.genesis_sha256,
            candidate.genesis_size,
        )
        publish_file_create_only(
            model,
            self.paths.model,
            0o400,
            self.uid,
            CANONICAL_MODEL_SHA256,
            CANONICAL_MODEL_SIZE_BYTES,
        )
        publish_bytes_create_only(
            self.paths.metadata, candidate.metadata_bytes, 0o600, self.uid
        )
        current_initial = values["proof"].provenance
        selected_initial = self._retained_provenance(
            self.paths.provenance_receipt, candidate, phase="initial"
        )
        if selected_initial is None:
            selected_initial_value = current_initial
            selected_initial_bytes = candidate.provenance_receipt_bytes
        else:
            selected_initial_value, selected_initial_bytes = selected_initial
            require_same_live_provenance_invariant(
                selected_initial_value, current_initial
            )
        publish_bytes_create_only(
            self.paths.provenance_receipt,
            selected_initial_bytes,
            0o600,
            self.uid,
        )
        public_address = self._ensure_key()
        final_recheck = values["proof"].recheck()
        final_value = validate_full_live_provenance(
            final_recheck.value,
            final_recheck.canonical_bytes,
            expected_commit=candidate.metadata["commit"],
            expected_run_id=candidate.metadata["workflow_run_id"],
            expected_run_attempt=candidate.metadata["workflow_run_attempt"],
            expected_artifact_id=candidate.artifact_id,
            expected_version=candidate.metadata["version"],
            expected_metadata_sha256=sha256_bytes(candidate.metadata_bytes),
            expected_files=candidate.metadata["files"],
            phase="final",
        )
        require_ordered_fresh_live_provenance_pair(current_initial, final_value)
        selected_final = self._retained_provenance(
            self.paths.provenance_recheck, candidate, phase="final"
        )
        if selected_final is None:
            selected_final_value = final_value
            selected_final_bytes = final_recheck.canonical_bytes
        else:
            selected_final_value, selected_final_bytes = selected_final
            require_same_live_provenance_invariant(selected_final_value, final_value)
        require_ordered_fresh_live_provenance_pair(
            selected_initial_value, selected_final_value
        )
        publish_bytes_create_only(
            self.paths.provenance_recheck,
            selected_final_bytes,
            0o600,
            self.uid,
        )
        runner = runner_bytes(
            self.paths,
            argv,
            uid=self.uid,
            node_sha256=candidate.node_sha256,
            node_size=candidate.node_size,
            genesis_sha256=candidate.genesis_sha256,
            genesis_size=candidate.genesis_size,
        )
        plist = plist_bytes(self.paths)
        publish_bytes_create_only(self.paths.runner, runner, 0o700, self.uid)
        if self.paths.log.exists() or self.paths.log.is_symlink():
            private_file(self.paths.log, self.uid, "canary log")
        else:
            publish_bytes_create_only(self.paths.log, b"", 0o600, self.uid)
        publish_bytes_create_only(self.paths.launch_agent, plist, 0o600, self.uid)
        config = {
            "schema": SCHEMA,
            "label": LABEL,
            "launchd_domain": self.domain,
            "pretag": {
                "schema": candidate.metadata["schema"],
                "repository": REPOSITORY,
                "commit": candidate.metadata["commit"],
                "run_id": candidate.metadata["workflow_run_id"],
                "run_attempt": candidate.metadata["workflow_run_attempt"],
                "version": candidate.metadata["version"],
                "platform": PLATFORM,
                "rust_target": RUST_TARGET,
                "metadata_sha256": sha256_bytes(candidate.metadata_bytes),
                "live_provenance_sha256": sha256_bytes(selected_initial_bytes),
                "live_recheck_sha256": sha256_bytes(selected_final_bytes),
                "artifact_id": candidate.artifact_id,
                "artifact_digest": candidate.artifact_digest,
                "archive_sha256": candidate.archive_sha256,
            },
            "node": {
                "path": str(self.paths.node),
                "sha256": candidate.node_sha256,
                "size_bytes": candidate.node_size,
            },
            "cli": {
                "path": str(self.paths.cli),
                "sha256": candidate.cli_sha256,
                "size_bytes": candidate.cli_size,
            },
            "genesis": {
                "path": str(self.paths.genesis),
                "sha256": candidate.genesis_sha256,
                "size_bytes": candidate.genesis_size,
                "validator_set_complete": True,
                "validator_count": 6,
                "community_rewards_v1_activation_height": 137_146,
            },
            "model": {
                "path": str(self.paths.model),
                "sha256": CANONICAL_MODEL_SHA256,
                "size_bytes": CANONICAL_MODEL_SIZE_BYTES,
            },
            "identity": {
                "path": str(self.paths.key),
                "public_address": public_address,
                "mode": "0600",
            },
            "runtime": {
                "rpc": RPC,
                "rpc_loopback_only": True,
                "stake": 0,
                "community_mode": True,
                "full_integer_worker": True,
                "p2p_peers": [],
                "p2p_port": 0,
                "community_rpc_urls": list(COMMUNITY_RPC_URLS),
                "argv": argv,
                "argv_sha256": sha256_bytes("\0".join(argv).encode()),
            },
            "managed": {
                "root": str(self.paths.root),
                "data": str(self.paths.data_dir),
                "evidence": str(self.paths.evidence_dir),
                "acceptance_receipt": str(self.paths.acceptance_receipt),
                "tmp": str(self.paths.tmp_dir),
                "log": str(self.paths.log),
                "runner": str(self.paths.runner),
                "runner_sha256": sha256_bytes(runner),
                "launch_agent": str(self.paths.launch_agent),
                "launch_agent_sha256": sha256_bytes(plist),
                "graceful_stop_budget_seconds": STOP_BUDGET_SECONDS,
                "cleanup_preserves": ["model", "key", "data", "evidence"],
            },
        }
        config_content = canonical_json(config)
        publish_bytes_create_only(self.paths.config, config_content, 0o600, self.uid)
        checksum = f"{sha256_bytes(config_content)}  canary.json\n".encode()
        publish_bytes_create_only(
            self.paths.config_checksum, checksum, 0o600, self.uid
        )
        self._validate_installation(require_launch_agent=True)
        self._write_evidence("install", {"public_address": public_address})
        print("Installed exact pre-tag canary files; LaunchAgent remains stopped.")

    def _load_config(self) -> dict[str, Any]:
        private_file(self.paths.config, self.uid, "canary config")
        private_file(self.paths.config_checksum, self.uid, "canary config checksum")
        checksum = self.paths.config_checksum.read_text(encoding="ascii")
        match = re.fullmatch(r"([0-9a-f]{64})  canary\.json\n", checksum)
        if match is None or sha256(self.paths.config) != match.group(1):
            fail("canary config checksum is invalid")
        try:
            config = json.loads(self.paths.config.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            fail(f"canary config is invalid: {error}")
        if (
            not isinstance(config, dict)
            or set(config)
            != {
                "schema",
                "label",
                "launchd_domain",
                "pretag",
                "node",
                "cli",
                "genesis",
                "model",
                "identity",
                "runtime",
                "managed",
            }
            or config.get("schema") != SCHEMA
        ):
            fail("canary config schema is invalid")
        return config

    @staticmethod
    def _pinned_file(config: dict[str, Any], key: str) -> tuple[Path, str, int]:
        value = config.get(key)
        if not isinstance(value, dict):
            fail(f"canary config {key} pin is missing")
        path = value.get("path")
        digest = value.get("sha256")
        size = value.get("size_bytes")
        if (
            not isinstance(path, str)
            or not isinstance(digest, str)
            or HEX_64.fullmatch(digest) is None
            or isinstance(size, bool)
            or not isinstance(size, int)
            or size <= 0
        ):
            fail(f"canary config {key} pin is malformed")
        return Path(path), digest, size

    def _validate_installation(self, *, require_launch_agent: bool) -> dict[str, Any]:
        self.check_platform()
        config = self._load_config()
        if config.get("label") != LABEL or config.get("launchd_domain") != self.domain:
            fail("canary config label/domain differs from this operator")
        pretag = config.get("pretag")
        if not isinstance(pretag, dict) or set(pretag) != {
            "schema",
            "repository",
            "commit",
            "run_id",
            "run_attempt",
            "version",
            "platform",
            "rust_target",
            "metadata_sha256",
            "live_provenance_sha256",
            "live_recheck_sha256",
            "artifact_id",
            "artifact_digest",
            "archive_sha256",
        }:
            fail("canary preflight binding is malformed")
        if (
            pretag.get("schema") != "arc.pretag.artifact.v1"
            or pretag.get("repository") != REPOSITORY
            or pretag.get("platform") != PLATFORM
            or pretag.get("rust_target") != RUST_TARGET
            or not isinstance(pretag.get("commit"), str)
            or HEX_40.fullmatch(pretag["commit"]) is None
            or not isinstance(pretag.get("run_id"), int)
            or pretag["run_id"] <= 0
            or not isinstance(pretag.get("run_attempt"), int)
            or pretag["run_attempt"] <= 0
            or not isinstance(pretag.get("version"), str)
            or SEMVER.fullmatch(pretag["version"]) is None
            or not isinstance(pretag.get("metadata_sha256"), str)
            or HEX_64.fullmatch(pretag["metadata_sha256"]) is None
            or not isinstance(pretag.get("live_provenance_sha256"), str)
            or HEX_64.fullmatch(pretag["live_provenance_sha256"]) is None
            or not isinstance(pretag.get("live_recheck_sha256"), str)
            or HEX_64.fullmatch(pretag["live_recheck_sha256"]) is None
            or isinstance(pretag.get("artifact_id"), bool)
            or not isinstance(pretag.get("artifact_id"), int)
            or pretag["artifact_id"] <= 0
            or not isinstance(pretag.get("artifact_digest"), str)
            or re.fullmatch(r"sha256:[0-9a-f]{64}", pretag["artifact_digest"]) is None
            or not isinstance(pretag.get("archive_sha256"), str)
            or HEX_64.fullmatch(pretag["archive_sha256"]) is None
        ):
            fail("canary preflight binding violates the exact metadata contract")

        node_path, node_hash, node_size = self._pinned_file(config, "node")
        cli_path, cli_hash, cli_size = self._pinned_file(config, "cli")
        genesis_path, genesis_hash, genesis_size = self._pinned_file(config, "genesis")
        model_path, model_hash, model_size = self._pinned_file(config, "model")
        if node_path != self.paths.node or cli_path != self.paths.cli:
            fail("canary binary paths differ from the dedicated managed root")
        if genesis_path != self.paths.genesis or genesis_hash != CANONICAL_GENESIS_SHA256:
            fail("canary genesis differs from the dedicated reviewed genesis")
        if (
            config["genesis"].get("validator_set_complete") is not True
            or config["genesis"].get("validator_count") != 6
            or config["genesis"].get("community_rewards_v1_activation_height")
            != 137_146
        ):
            fail("canary genesis semantic binding is invalid")
        if (
            model_path != self.paths.model
            or model_hash != CANONICAL_MODEL_SHA256
            or model_size != CANONICAL_MODEL_SIZE_BYTES
        ):
            fail("canary model differs from the canonical exact SHA/size pin")
        for path, digest, size, mode, description in (
            (node_path, node_hash, node_size, 0o700, "managed preflight node"),
            (cli_path, cli_hash, cli_size, 0o700, "managed preflight CLI"),
            (genesis_path, genesis_hash, genesis_size, 0o600, "managed genesis"),
        ):
            metadata = regular_file(path, description)
            if (
                metadata.st_uid != self.uid
                or stat.S_IMODE(metadata.st_mode) != mode
                or metadata.st_size != size
                or sha256(path) != digest
            ):
                fail(f"{description} failed its exact owner/mode/hash/size proof")
        validate_genesis(genesis_path)
        model_metadata = regular_file(model_path, "managed canonical GGUF")
        if (
            model_metadata.st_uid != self.uid
            or stat.S_IMODE(model_metadata.st_mode) != 0o400
            or model_metadata.st_nlink != 1
            or model_metadata.st_size != model_size
            or sha256(model_path) != model_hash
        ):
            fail("managed canonical GGUF failed its exact owner/mode/link/hash/size proof")
        require_safe_runtime_path(model_path, "canonical local GGUF")

        private_file(self.paths.metadata, self.uid, "retained preflight metadata")
        if sha256(self.paths.metadata) != pretag["metadata_sha256"]:
            fail("retained preflight metadata hash differs from canary config")
        try:
            metadata_value = json.loads(self.paths.metadata.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            fail(f"retained preflight metadata is invalid: {error}")
        if (
            metadata_value.get("commit") != pretag["commit"]
            or metadata_value.get("workflow_run_id") != pretag["run_id"]
            or metadata_value.get("workflow_run_attempt") != pretag["run_attempt"]
            or metadata_value.get("version") != pretag["version"]
            or metadata_value.get("files")
            != {
                "arc-node-macos-arm64": node_hash,
                "arc-cli-macos-arm64": cli_hash,
                "genesis.toml": genesis_hash,
            }
        ):
            fail("retained preflight metadata no longer binds the installed files")

        private_file(
            self.paths.provenance_receipt,
            self.uid,
            "retained live protected preflight provenance",
        )
        receipt_bytes = self.paths.provenance_receipt.read_bytes()
        if sha256_bytes(receipt_bytes) != pretag["live_provenance_sha256"]:
            fail("retained live provenance hash differs from canary config")
        try:
            provenance_value = json.loads(receipt_bytes)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            fail(f"retained live provenance is invalid: {error}")
        if not isinstance(provenance_value, dict) or receipt_bytes != canonical_json(provenance_value):
            fail("retained live provenance is not canonical JSON")
        provenance_value = validate_full_live_provenance(
            provenance_value,
            receipt_bytes,
            expected_commit=pretag["commit"],
            expected_run_id=pretag["run_id"],
            expected_run_attempt=pretag["run_attempt"],
            expected_artifact_id=pretag["artifact_id"],
            expected_version=pretag["version"],
            expected_metadata_sha256=pretag["metadata_sha256"],
            expected_files=metadata_value["files"],
            phase="retained initial",
        )
        live = provenance_value.get("live")
        artifact = provenance_value.get("artifact")
        if (
            provenance_value.get("schema") != artifact_provenance.PROVENANCE_SCHEMA
            or not isinstance(live, dict)
            or live.get("repository") != REPOSITORY
            or live.get("protected_branch") != "main"
            or live.get("commit") != pretag["commit"]
            or live.get("run_id") != pretag["run_id"]
            or live.get("run_attempt") != pretag["run_attempt"]
            or live.get("artifact_id") != pretag["artifact_id"]
            or live.get("artifact_digest") != pretag["artifact_digest"]
            or not isinstance(artifact, dict)
            or artifact.get("kind") != "headless"
            or artifact.get("platform") != PLATFORM
            or artifact.get("version") != pretag["version"]
            or artifact.get("archive_sha256") != pretag["archive_sha256"]
            or artifact.get("build_metadata_sha256") != pretag["metadata_sha256"]
            or artifact.get("files") != metadata_value["files"]
        ):
            fail("retained live provenance no longer binds the reviewed artifact")

        private_file(
            self.paths.provenance_recheck,
            self.uid,
            "retained final live protected preflight recheck",
        )
        recheck_bytes = self.paths.provenance_recheck.read_bytes()
        if sha256_bytes(recheck_bytes) != pretag["live_recheck_sha256"]:
            fail("retained final live recheck hash differs from canary config")
        try:
            recheck = json.loads(recheck_bytes)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            fail(f"retained final live recheck is invalid: {error}")
        recheck = validate_full_live_provenance(
            recheck,
            recheck_bytes,
            expected_commit=pretag["commit"],
            expected_run_id=pretag["run_id"],
            expected_run_attempt=pretag["run_attempt"],
            expected_artifact_id=pretag["artifact_id"],
            expected_version=pretag["version"],
            expected_metadata_sha256=pretag["metadata_sha256"],
            expected_files=metadata_value["files"],
            phase="retained final",
        )
        if (
            not isinstance(recheck, dict)
            or recheck_bytes != canonical_json(recheck)
            or recheck.get("schema") != artifact_provenance.PROVENANCE_SCHEMA
            or not isinstance(recheck.get("live"), dict)
            or recheck.get("artifact") != provenance_value.get("artifact")
            or {
                key: recheck["live"].get(key)
                for key in (
                    "repository",
                    "commit",
                    "run_id",
                    "run_attempt",
                    "artifact_id",
                    "artifact_digest",
                )
            }
            != {
                "repository": REPOSITORY,
                "commit": pretag["commit"],
                "run_id": pretag["run_id"],
                "run_attempt": pretag["run_attempt"],
                "artifact_id": pretag["artifact_id"],
                "artifact_digest": pretag["artifact_digest"],
            }
        ):
            fail("retained final live recheck no longer binds the installed artifact")
        require_ordered_fresh_live_provenance_pair(provenance_value, recheck)

        identity = config.get("identity")
        if (
            not isinstance(identity, dict)
            or identity.get("path") != str(self.paths.key)
            or identity.get("mode") != "0600"
            or not isinstance(identity.get("public_address"), str)
            or ADDRESS.fullmatch(identity["public_address"]) is None
        ):
            fail("canary identity binding is malformed")
        if self._validate_key() != identity["public_address"]:
            fail("dedicated keyfile public identity changed")

        runtime = config.get("runtime")
        expected = expected_argv(self.paths)
        if (
            not isinstance(runtime, dict)
            or runtime.get("rpc") != RPC
            or runtime.get("rpc_loopback_only") is not True
            or runtime.get("stake") != 0
            or runtime.get("community_mode") is not True
            or runtime.get("full_integer_worker") is not True
            or runtime.get("p2p_peers") != []
            or runtime.get("p2p_port") != 0
            or runtime.get("community_rpc_urls") != list(COMMUNITY_RPC_URLS)
            or runtime.get("argv") != expected
            or runtime.get("argv_sha256")
            != sha256_bytes("\0".join(expected).encode())
        ):
            fail("canary runtime role/argv binding is invalid")
        managed = config.get("managed")
        if not isinstance(managed, dict):
            fail("canary managed-file binding is missing")
        expected_runner = runner_bytes(
            self.paths,
            expected,
            uid=self.uid,
            node_sha256=node_hash,
            node_size=node_size,
            genesis_sha256=genesis_hash,
            genesis_size=genesis_size,
        )
        expected_plist = plist_bytes(self.paths)
        if (
            managed.get("root") != str(self.paths.root)
            or managed.get("data") != str(self.paths.data_dir)
            or managed.get("evidence") != str(self.paths.evidence_dir)
            or managed.get("acceptance_receipt")
            != str(self.paths.acceptance_receipt)
            or managed.get("tmp") != str(self.paths.tmp_dir)
            or managed.get("log") != str(self.paths.log)
            or managed.get("runner") != str(self.paths.runner)
            or managed.get("runner_sha256") != sha256_bytes(expected_runner)
            or managed.get("launch_agent") != str(self.paths.launch_agent)
            or managed.get("launch_agent_sha256") != sha256_bytes(expected_plist)
            or managed.get("graceful_stop_budget_seconds") != STOP_BUDGET_SECONDS
            or managed.get("cleanup_preserves") != ["model", "key", "data", "evidence"]
        ):
            fail("canary managed-file binding is invalid")
        private_directory(
            self.paths.tmp_dir, self.uid, "private runtime temporary directory"
        )
        executable_file(self.paths.runner, self.uid, "managed canary runner")
        if self.paths.runner.read_bytes() != expected_runner:
            fail("managed canary runner differs from the exact expected argv")
        private_file(self.paths.log, self.uid, "canary log")
        if require_launch_agent:
            private_file(self.paths.launch_agent, self.uid, "canary LaunchAgent")
            if self.paths.launch_agent.read_bytes() != expected_plist:
                fail("canary LaunchAgent differs from the exact expected plist")
        if self.paths.acceptance_receipt.exists() or self.paths.acceptance_receipt.is_symlink():
            self._validate_acceptance_receipt(config)
        return config

    def _acceptance_payload(
        self, config: dict[str, Any], *, pid: int, accepted_at_unix_ns: int
    ) -> dict[str, Any]:
        """Return the immutable handoff that binds production probes to this worker."""

        return {
            "schema": "arc.macos.pretag-community-canary.acceptance.v1",
            "accepted": True,
            "accepted_at_unix_ns": accepted_at_unix_ns,
            "canary_schema": SCHEMA,
            "label": LABEL,
            "platform": PLATFORM,
            "rust_target": RUST_TARGET,
            "config_sha256": sha256(self.paths.config),
            "worker": "0x" + config["identity"]["public_address"],
            "pretag": {
                "repository": config["pretag"]["repository"],
                "commit": config["pretag"]["commit"],
                "run_id": config["pretag"]["run_id"],
                "run_attempt": config["pretag"]["run_attempt"],
                "artifact_id": config["pretag"]["artifact_id"],
                "artifact_digest": config["pretag"]["artifact_digest"],
                "archive_sha256": config["pretag"]["archive_sha256"],
                "metadata_sha256": config["pretag"]["metadata_sha256"],
            },
            "runtime": {
                "stake": config["runtime"]["stake"],
                "community_mode": config["runtime"]["community_mode"],
                "full_integer_worker": config["runtime"]["full_integer_worker"],
                "rpc_loopback_only": config["runtime"]["rpc_loopback_only"],
                "p2p_peers": config["runtime"]["p2p_peers"],
                "p2p_port": config["runtime"]["p2p_port"],
                "community_rpc_urls": config["runtime"]["community_rpc_urls"],
                "argv_sha256": config["runtime"]["argv_sha256"],
            },
            "artifacts": {
                "node_sha256": config["node"]["sha256"],
                "model_sha256": config["model"]["sha256"],
                "genesis_sha256": config["genesis"]["sha256"],
            },
            "process": {
                "pid": pid,
                "exact_executable_argv_and_listeners_proved": True,
            },
        }

    def _validate_acceptance_receipt(self, config: dict[str, Any]) -> dict[str, Any]:
        private_file(
            self.paths.acceptance_receipt,
            self.uid,
            "macOS canary acceptance receipt",
        )
        payload = self.paths.acceptance_receipt.read_bytes()
        if len(payload) > 64 * 1024:
            fail("macOS canary acceptance receipt is oversized")
        try:
            receipt = json.loads(payload)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            fail(f"macOS canary acceptance receipt is invalid: {error}")
        if not isinstance(receipt, dict) or payload != canonical_json(receipt):
            fail("macOS canary acceptance receipt is not canonical JSON")
        if set(receipt) != {
            "schema", "accepted", "accepted_at_unix_ns", "canary_schema",
            "label", "platform", "rust_target", "config_sha256", "worker",
            "pretag", "runtime", "artifacts", "process",
        }:
            fail("macOS canary acceptance receipt field set differs")
        timestamp = receipt.get("accepted_at_unix_ns")
        process = receipt.get("process")
        if (
            receipt.get("schema")
            != "arc.macos.pretag-community-canary.acceptance.v1"
            or receipt.get("accepted") is not True
            or isinstance(timestamp, bool)
            or not isinstance(timestamp, int)
            or timestamp <= 0
            or receipt.get("canary_schema") != SCHEMA
            or receipt.get("label") != LABEL
            or receipt.get("platform") != PLATFORM
            or receipt.get("rust_target") != RUST_TARGET
            or not isinstance(process, dict)
            or set(process) != {"pid", "exact_executable_argv_and_listeners_proved"}
            or isinstance(process.get("pid"), bool)
            or not isinstance(process.get("pid"), int)
            or process["pid"] <= 0
            or process.get("exact_executable_argv_and_listeners_proved") is not True
        ):
            fail("macOS canary acceptance receipt header/process proof is malformed")
        expected = self._acceptance_payload(
            config,
            pid=process["pid"],
            accepted_at_unix_ns=timestamp,
        )
        if receipt != expected:
            fail("macOS canary acceptance receipt differs from the validated canary config")
        return receipt

    def _service_target(self) -> str:
        return f"{self.domain}/{LABEL}"

    def _launchctl_print(self) -> subprocess.CompletedProcess[str]:
        return self.platform.run(
            ("launchctl", "print", self._service_target()), check=False
        )

    def is_loaded(self) -> bool:
        return self._launchctl_print().returncode == 0

    def _service_pid(self) -> int | None:
        result = self._launchctl_print()
        if result.returncode != 0:
            return None
        matches = re.findall(r"(?m)^\s*pid = ([1-9][0-9]*)\s*$", result.stdout)
        if len(matches) > 1:
            fail("launchctl reported more than one canary PID")
        return int(matches[0]) if matches else None

    def _process_exists(self, pid: int) -> bool:
        result = self.platform.run(
            ("ps", "-p", str(pid), "-o", "pid="), check=False
        )
        if result.returncode != 0:
            return False
        return result.stdout.strip() == str(pid)

    def _prove_pid_base_identity(self, config: dict[str, Any], pid: int) -> None:
        expected_command = " ".join(config["runtime"]["argv"])
        command = self.platform.run(
            ("ps", "-ww", "-p", str(pid), "-o", "command=")
        ).stdout.rstrip("\n")
        if command != expected_command:
            fail("canary PID argv differs from the exact hash-bound runtime argv")
        lsof = self.platform.run(
            ("lsof", "-a", "-p", str(pid), "-d", "txt", "-Fn")
        ).stdout.splitlines()
        executable_paths = [line[1:] for line in lsof if line.startswith("n")]
        if len(executable_paths) != 1:
            fail("lsof did not return exactly one executable for the canary PID")
        executable = Path(executable_paths[0]).resolve(strict=True)
        if executable != self.paths.node.resolve(strict=True):
            fail("canary PID executable differs from the exact preflight node")
        node = config["node"]
        metadata = regular_file(executable, "running preflight node")
        if metadata.st_size != node["size_bytes"] or sha256(executable) != node["sha256"]:
            fail("running canary executable failed its exact hash/size proof")

    def _prove_pid_listeners(self, pid: int) -> None:
        listeners = self.platform.run(
            (
                "lsof",
                "-nP",
                "-a",
                "-p",
                str(pid),
                "-iTCP",
                "-sTCP:LISTEN",
                "-Fn",
            )
        ).stdout.splitlines()
        listener_names = [line[1:] for line in listeners if line.startswith("n")]
        if listener_names != [RPC]:
            fail(
                "canary TCP listeners differ from the sole loopback RPC contract: "
                f"{listener_names!r}"
            )
        udp = self.platform.run(
            ("lsof", "-nP", "-a", "-p", str(pid), "-iUDP", "-Fn")
        ).stdout.splitlines()
        udp_names = [line[1:] for line in udp if line.startswith("n")]
        match = (
            re.fullmatch(r"127[.]0[.]0[.]1:([1-9][0-9]{0,4})", udp_names[0])
            if len(udp_names) == 1
            else None
        )
        if match is None or int(match.group(1)) > 65_535:
            fail(
                "canary UDP sockets differ from the sole ephemeral loopback "
                f"QUIC contract: {udp_names!r}"
            )

    def _prove_pid_identity(self, config: dict[str, Any], pid: int) -> None:
        self._prove_pid_base_identity(config, pid)
        self._prove_pid_listeners(pid)

    def _prove_process(self, config: dict[str, Any], expected_pid: int | None = None) -> int:
        pid = self._service_pid()
        if pid is None:
            fail("canary LaunchAgent has no running PID")
        if expected_pid is not None and pid != expected_pid:
            fail(f"canary LaunchAgent PID changed from {expected_pid} to {pid}")
        self._prove_pid_identity(config, pid)
        return pid

    def _domain_available(self) -> None:
        if self.platform.run(("launchctl", "print", self.domain), check=False).returncode != 0:
            fail(f"interactive launchd domain is unavailable: {self.domain}")

    @lifecycle_transaction
    def start(self) -> None:
        config = self._validate_installation(require_launch_agent=True)
        self._domain_available()
        if self.is_loaded():
            pid = self._prove_process(config)
            print(f"Canary is already running with exact PID/exe/argv proof: pid={pid}")
            return
        self.platform.run(("launchctl", "enable", self._service_target()))
        self.platform.run(
            ("launchctl", "bootstrap", self.domain, str(self.paths.launch_agent))
        )
        self.platform.run(("launchctl", "kickstart", self._service_target()))
        for _ in range(self.start_proof_seconds):
            pid = self._service_pid()
            if pid is not None:
                try:
                    self._prove_pid_base_identity(config, pid)
                except CanaryError:
                    # Prevent a restart, but never signal a PID whose identity
                    # has not been established.
                    self.platform.run(
                        ("launchctl", "disable", self._service_target()),
                        check=False,
                    )
                    raise
                try:
                    self._prove_pid_listeners(pid)
                except CanaryError:
                    # The executable and complete argv are exact. Quarantine
                    # an unexpected listener using SIGTERM only, then preserve
                    # the original proof failure for the operator.
                    self.platform.run(("launchctl", "disable", self._service_target()))
                    self._prove_pid_base_identity(config, pid)
                    self.platform.run(
                        ("launchctl", "kill", "SIGTERM", self._service_target())
                    )
                    for _ in range(self.stop_budget_seconds):
                        current = self._service_pid()
                        alive = self._process_exists(pid)
                        if current is None and not alive:
                            break
                        if current is not None and current != pid:
                            fail("canary PID changed while quarantining a failed start")
                        if alive:
                            self._prove_pid_base_identity(config, pid)
                        self.platform.sleep(1)
                    else:
                        fail(
                            "failed-start canary did not exit within the graceful "
                            "budget; the label remains disabled and no force signal was sent"
                        )
                    self.platform.run(("launchctl", "bootout", self._service_target()))
                    if self.is_loaded():
                        fail("failed-start canary remained loaded after graceful quarantine")
                    raise
                self._write_evidence("start", {"pid": pid})
                print(f"Started exact pre-tag community canary: pid={pid}, rpc=http://{RPC}")
                return
            self.platform.sleep(1)
        self.platform.run(
            ("launchctl", "disable", self._service_target()), check=False
        )
        if self.is_loaded():
            fail(
                "LaunchAgent did not produce a provable PID within 60 seconds; "
                "the label is disabled and the job remains loaded for review; "
                "bootout was not attempted across a racy no-PID observation"
            )
        fail("LaunchAgent did not produce a provable canary PID within 60 seconds")

    @lifecycle_transaction
    def status(self) -> int:
        require_plist = self.paths.launch_agent.exists() or self.paths.launch_agent.is_symlink()
        config = self._validate_installation(require_launch_agent=require_plist)
        if not require_plist:
            if self.is_loaded():
                fail("canary LaunchAgent is loaded although its exact plist is absent")
            print("Canary is cleaned and stopped; preserved state/evidence remain on disk.")
            return 3
        if not self.is_loaded():
            print("Canary is installed but stopped.")
            return 3
        pid = self._prove_process(config)
        print(f"Canary is running with exact PID/exe/argv proof: pid={pid}")
        return 0

    @lifecycle_transaction
    def accept(self) -> None:
        """Seal the exact running worker identity for production reward probes."""

        config = self._validate_installation(require_launch_agent=True)
        pid = self._prove_process(config)
        if self.paths.acceptance_receipt.exists() or self.paths.acceptance_receipt.is_symlink():
            receipt = self._validate_acceptance_receipt(config)
            print(
                "Canary acceptance already sealed for exact worker "
                f"{receipt['worker']}; sha256={sha256(self.paths.acceptance_receipt)}"
            )
            return
        payload = self._acceptance_payload(
            config,
            pid=pid,
            accepted_at_unix_ns=time.time_ns(),
        )
        publish_bytes_create_only(
            self.paths.acceptance_receipt,
            canonical_json(payload),
            0o600,
            self.uid,
        )
        receipt = self._validate_acceptance_receipt(config)
        self._write_evidence(
            "accept",
            {
                "worker": receipt["worker"],
                "acceptance_sha256": sha256(self.paths.acceptance_receipt),
            },
        )
        print(
            "Accepted exact pre-tag community canary worker "
            f"{receipt['worker']}; receipt={self.paths.acceptance_receipt}; "
            f"sha256={sha256(self.paths.acceptance_receipt)}"
        )

    @lifecycle_transaction
    def stop(self) -> None:
        require_plist = self.paths.launch_agent.exists() or self.paths.launch_agent.is_symlink()
        config = self._validate_installation(require_launch_agent=require_plist)
        if not self.is_loaded():
            print("Canary is already stopped.")
            return
        if not require_plist:
            fail("refusing to control a loaded canary without its exact LaunchAgent plist")
        pid = self._service_pid()
        if pid is None:
            self.platform.run(("launchctl", "disable", self._service_target()))
            if self._service_pid() is not None:
                fail("canary PID appeared after the label was disabled")
            fail(
                "loaded canary has no provable PID; the label is disabled and "
                "the job remains loaded for review; bootout was not attempted "
                "across a racy no-PID observation"
            )

        pid = self._prove_process(config, pid)
        self.platform.run(("launchctl", "disable", self._service_target()))
        current = self._service_pid()
        if current is None:
            if self._process_exists(pid):
                fail("launchd lost ownership while the exact canary PID remained alive")
        else:
            self._prove_process(config, pid)
            self.platform.run(
                ("launchctl", "kill", "SIGTERM", self._service_target())
            )
            for _ in range(self.stop_budget_seconds):
                current = self._service_pid()
                alive = self._process_exists(pid)
                if current is None and not alive:
                    break
                if current is not None and current != pid:
                    fail("canary PID changed during graceful SIGTERM drain")
                if alive:
                    self._prove_pid_identity(config, pid)
                self.platform.sleep(1)
            else:
                fail(
                    "canary did not exit within the 4420-second graceful budget; "
                    "the label remains disabled and no force signal was sent"
                )
        if self._service_pid() is not None or self._process_exists(pid):
            fail("canary death is not proven after graceful SIGTERM")
        self.platform.run(("launchctl", "bootout", self._service_target()))
        if self.is_loaded():
            fail("canary LaunchAgent remained loaded after proven process death")
        self._write_evidence("stop", {"pid": pid, "signal": "SIGTERM"})
        print(f"Stopped exact canary PID {pid} gracefully; no force signal was used.")

    @lifecycle_transaction
    def cleanup(self) -> None:
        self.stop()
        if self.paths.launch_agent.exists() or self.paths.launch_agent.is_symlink():
            expected = plist_bytes(self.paths)
            private_file(self.paths.launch_agent, self.uid, "canary LaunchAgent")
            if self.paths.launch_agent.read_bytes() != expected:
                fail("refusing to remove a LaunchAgent that differs from the canary contract")
            self.paths.launch_agent.unlink()
            fsync_directory(self.paths.launch_agent.parent)
        self.platform.run(
            ("launchctl", "disable", self._service_target()), check=False
        )
        self._write_evidence("cleanup", {"preserved": ["model", "key", "data", "evidence"]})
        print("Cleaned LaunchAgent registration; model, key, data, logs, and evidence were preserved.")

    def _write_evidence(self, action: str, values: dict[str, Any]) -> None:
        ensure_private_directory(self.paths.evidence_dir, self.uid)
        config = self._load_config()
        payload = {
            "schema": "arc.macos.pretag-community-canary.evidence.v1",
            "action": action,
            "label": LABEL,
            "unix_time_ns": time.time_ns(),
            "binding": {
                "config_sha256": sha256(self.paths.config),
                "pretag_commit": config["pretag"]["commit"],
                "pretag_run_id": config["pretag"]["run_id"],
                "pretag_run_attempt": config["pretag"]["run_attempt"],
                "pretag_artifact_id": config["pretag"]["artifact_id"],
                "pretag_artifact_digest": config["pretag"]["artifact_digest"],
                "pretag_archive_sha256": config["pretag"]["archive_sha256"],
                "node_sha256": config["node"]["sha256"],
                "model_sha256": config["model"]["sha256"],
                "argv_sha256": config["runtime"]["argv_sha256"],
                "public_address": config["identity"]["public_address"],
            },
            **values,
        }
        while True:
            path = self.paths.evidence_dir / (
                f"{payload['unix_time_ns']}-{action}-{os.getpid()}.json"
            )
            try:
                publish_bytes_create_only(path, canonical_json(payload), 0o600, self.uid)
                return
            except CanaryError as error:
                if "replace mismatched create-only" not in str(error):
                    raise
                payload["unix_time_ns"] += 1


def add_root_argument(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--root",
        type=Path,
        default=Path.home() / ".arc-pretag-community-canary",
        help="dedicated canary root (default: ~/.arc-pretag-community-canary)",
    )


def add_candidate_arguments(parser: argparse.ArgumentParser) -> None:
    add_root_argument(parser)
    parser.add_argument("--raw-actions-zip", required=True, type=Path)
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--expected-commit", required=True)
    parser.add_argument("--expected-run-id", required=True, type=int)
    parser.add_argument("--expected-run-attempt", required=True, type=int)
    parser.add_argument("--expected-artifact-id", required=True, type=int)
    parser.add_argument("--curl", required=True, type=Path)
    parser.add_argument("--curl-sha256", required=True)
    parser.add_argument("--ca-bundle", required=True, type=Path)
    parser.add_argument("--ca-bundle-sha256", required=True)


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    for name in ("plan", "install"):
        add_candidate_arguments(subparsers.add_parser(name))
    for name in ("start", "status", "accept", "stop", "cleanup"):
        add_root_argument(subparsers.add_parser(name))
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    controller = CanaryController(args.root)
    if args.command in {"plan", "install"}:
        arguments = (
            args.raw_actions_zip,
            args.model,
            args.expected_commit,
            args.expected_run_id,
            args.expected_run_attempt,
            args.expected_artifact_id,
            args.curl,
            args.curl_sha256,
            args.ca_bundle,
            args.ca_bundle_sha256,
        )
        if args.command == "plan":
            controller.plan(*arguments)
        else:
            controller.install(*arguments)
        return 0
    if args.command == "start":
        controller.start()
        return 0
    if args.command == "status":
        return controller.status()
    if args.command == "accept":
        controller.accept()
        return 0
    if args.command == "stop":
        controller.stop()
        return 0
    if args.command == "cleanup":
        controller.cleanup()
        return 0
    raise AssertionError(args.command)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except CanaryError as error:
        print(f"macOS pre-tag canary: {error}", file=sys.stderr)
        raise SystemExit(1)
