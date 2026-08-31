#!/usr/bin/env python3
"""Restore and create-only install the six production validator keys.

The restore boundary accepts only the exact one-shot CMS artifact and pre-tag
CLI that are bound to one protected-main commit.  Private bytes are decrypted
and inspected only below mode-0700 local directories.  The install boundary is
separate and additionally requires durable evidence that every controlled
legacy writer is offline behind its persistent restart fence.
"""

from __future__ import annotations

import argparse
import base64
import binascii
import datetime
import hashlib
import importlib.util
import io
import json
import os
import re
import stat
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, BinaryIO, NoReturn, Sequence


REPOSITORY = "FerrumVir/arc-chain"
VERSION = "0.8.0"
RESTORE_SCHEMA = "arc.validator-vault.restore.v1"
INSTALL_SCHEMA = "arc.validator-vault.install.v1"
OFFLINE_STOP_EVIDENCE_SCHEMA = "arc.validator-vault.offline-stop-evidence.v2"
MAINTENANCE_EVIDENCE_BUNDLE_SCHEMA = "arc.recovery.legacy-maintenance-evidence-bundle.v1"
MAINTENANCE_BOUNDARY_SCHEMA = "arc.recovery.legacy-maintenance-boundary.v1"
AUTHENTICATED_HEIGHT_FLEET_SCHEMA = "arc.recovery.authenticated-legacy-height-fleet.v1"
REWRAP_SCHEMA = "arc.validator-vault-rewrap.v1"
FREEZE_PLAN_SCHEMA = "arc.recovery.freeze-plan.v5"
REMOTE_KEY_DIR = "/etc/arc-v3"
REMOTE_KEY_PATH = f"{REMOTE_KEY_DIR}/validator-key.json"
MAX_JSON_BYTES = 64 * 1024
MAX_CMS_BYTES = 2 * 1024 * 1024
MAX_TAR_BYTES = 1024 * 1024
MAX_KEY_BYTES = 16 * 1024
MAX_TOTAL_KEY_BYTES = 96 * 1024
MAX_MEMBERS = 16
MAX_PATH_BYTES = 192
MAX_PATH_DEPTH = 4
LOWER_HASH_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
SAFE_HOST_RE = re.compile(r"^[A-Za-z0-9](?:[A-Za-z0-9.-]{0,251}[A-Za-z0-9])?$")
UTC_SECONDS_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")

# This is the reviewed v0.8 trust-root mapping, not a mapping inferred from
# private filenames or tar order.  A changed address requires a reviewed source
# change, a new complete genesis, and a new protected-main pre-tag build.
PRODUCTION_FLEET: tuple[tuple[str, str, str], ...] = (
    ("NYC", "adf4ff16f997c871c16f3897e67881311d08f975f28ebdcf79e86ea9e3b99d0f", "149.28.32.76"),
    ("LAX", "44d20543df6e76696da2ebbbd79e4243cd41729fa5b890e2618991e489314780", "140.82.16.112"),
    ("AMS", "5772741c93d8a4b04ec39007cb568a31e13ffba0d3e786596d1900d30e529f21", "136.244.109.1"),
    ("LHR", "228787281308d6c1a560848c2c168814bde1b6153e9e65a286d7211f04628fdd", "104.238.171.11"),
    ("NRT", "f03cbab49cf553a05541ddebc09b32a4c5507efb157d354b6d7f8c6682c32f5f", "202.182.107.41"),
    ("SGP", "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b", "149.28.153.31"),
)
PRODUCTION_VALIDATORS = tuple((name, address) for name, address, _ in PRODUCTION_FLEET)
LOWER_NODE_ORDER = tuple(name.lower() for name, _, _ in PRODUCTION_FLEET)
ADDRESS_TO_NODE = {address: name for name, address, _ in PRODUCTION_FLEET}
NODE_TO_HOST = {name: host for name, _, host in PRODUCTION_FLEET}
STOPPED_STATUS_ARGV_FIELDS = (
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


class VaultError(ValueError):
    """A restore/install input violates the sealed production contract."""


def fail(message: str) -> NoReturn:
    raise VaultError(message)


_PROVENANCE_PATH = Path(__file__).with_name("protected_pretag_artifact.py")
_PROVENANCE_SPEC = importlib.util.spec_from_file_location(
    "arc_protected_pretag_for_vault", _PROVENANCE_PATH
)
if _PROVENANCE_SPEC is None or _PROVENANCE_SPEC.loader is None:
    raise RuntimeError("cannot load protected pre-tag artifact verifier")
artifact_provenance = importlib.util.module_from_spec(_PROVENANCE_SPEC)
sys.modules[_PROVENANCE_SPEC.name] = artifact_provenance
_PROVENANCE_SPEC.loader.exec_module(artifact_provenance)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonical_json_bytes(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


def exact_object(value: object, fields: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != fields:
        fail(f"{label} has missing, unknown, or unsupported fields")
    return value


def require_hash(value: object, label: str) -> str:
    if not isinstance(value, str) or LOWER_HASH_RE.fullmatch(value) is None:
        fail(f"{label} is not one lowercase SHA-256")
    return value


def require_commit(value: object, label: str) -> str:
    if not isinstance(value, str) or COMMIT_RE.fullmatch(value) is None:
        fail(f"{label} is not one full lowercase Git commit")
    return value


def validate_provenance_pair(
    initial: object,
    final: object,
    *,
    source_commit: str,
    platform: str = "linux-x86_64",
) -> tuple[dict[str, Any], dict[str, Any]]:
    values: list[dict[str, Any]] = []
    response_observations: list[tuple[list[int], list[str], list[dict[str, Any]]]] = []
    for phase, candidate in (("initial", initial), ("final", final)):
        proof = exact_object(
            candidate,
            {"schema", "live", "api", "artifact"},
            f"{phase} protected pre-tag provenance",
        )
        if proof["schema"] != artifact_provenance.PROVENANCE_SCHEMA:
            fail(f"{phase} protected pre-tag provenance schema is unsupported")
        live = exact_object(
            proof["live"],
            {
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
            },
            f"{phase} protected pre-tag live tuple",
        )
        api = exact_object(
            proof["api"],
            {
                "origin",
                "anonymous",
                "redirects_followed",
                "max_age_seconds",
                "curl_sha256",
                "ca_bundle_sha256",
                "responses",
            },
            f"{phase} protected pre-tag API proof",
        )
        artifact = exact_object(
            proof["artifact"],
            {
                "kind",
                "platform",
                "version",
                "raw_actions_zip_sha256",
                "raw_actions_zip_size",
                "archive_sha256",
                "build_metadata_sha256",
                "files",
            },
            f"{phase} protected pre-tag artifact proof",
        )
        if (
            live["repository"] != REPOSITORY
            or live["protected_branch"] != "main"
            or live["commit"] != source_commit
            or live["workflow_path"] != artifact_provenance.WORKFLOW_PATH
            or isinstance(live["workflow_id"], bool)
            or not isinstance(live["workflow_id"], int)
            or live["workflow_id"] <= 0
            or isinstance(live["run_id"], bool)
            or not isinstance(live["run_id"], int)
            or live["run_id"] <= 0
            or isinstance(live["run_attempt"], bool)
            or not isinstance(live["run_attempt"], int)
            or live["run_attempt"] <= 0
            or isinstance(live["artifact_id"], bool)
            or not isinstance(live["artifact_id"], int)
            or live["artifact_id"] <= 0
            or re.fullmatch(r"sha256:[0-9a-f]{64}", str(live["artifact_digest"])) is None
            or isinstance(live["artifact_size_in_bytes"], bool)
            or not isinstance(live["artifact_size_in_bytes"], int)
            or live["artifact_size_in_bytes"] <= 0
            or isinstance(live["api_verified_at_unix"], bool)
            or not isinstance(live["api_verified_at_unix"], int)
            or live["api_verified_at_unix"] <= 0
            or api["origin"] != artifact_provenance.API_ORIGIN
            or api["anonymous"] is not True
            or api["redirects_followed"] is not False
            or api["max_age_seconds"] != artifact_provenance.MAX_API_AGE_SECONDS
            or artifact["kind"] != "headless"
            or artifact["platform"] != platform
            or artifact["version"] != VERSION
        ):
            fail(f"{phase} protected pre-tag provenance violates the production tuple")
        for field in ("curl_sha256", "ca_bundle_sha256"):
            require_hash(api[field], f"{phase} protected pre-tag {field}")
        for field in (
            "raw_actions_zip_sha256",
            "archive_sha256",
            "build_metadata_sha256",
        ):
            require_hash(artifact[field], f"{phase} protected pre-tag {field}")
        expected_name = (
            f"arc-pretag-headless-{platform}-{source_commit}-"
            f"{live['run_id']}-{live['run_attempt']}-{artifact['archive_sha256']}"
        )
        if (
            live["artifact_name"] != expected_name
            or live["artifact_digest"]
            != "sha256:" + artifact["raw_actions_zip_sha256"]
            or isinstance(artifact["raw_actions_zip_size"], bool)
            or not isinstance(artifact["raw_actions_zip_size"], int)
            or artifact["raw_actions_zip_size"] <= 0
            or artifact["raw_actions_zip_size"] != live["artifact_size_in_bytes"]
        ):
            fail(f"{phase} protected pre-tag raw ZIP/name tuple differs")
        files = artifact["files"]
        expected_files = {
            f"arc-node-{platform}",
            f"arc-cli-{platform}",
            "genesis.toml",
        }
        if not isinstance(files, dict) or set(files) != expected_files:
            fail(f"{phase} protected pre-tag payload membership differs")
        for name, digest in files.items():
            require_hash(digest, f"{phase} protected pre-tag payload {name}")
        responses = api["responses"]
        if not isinstance(responses, list) or [row.get("label") for row in responses if isinstance(row, dict)] != [
            "workflow",
            "run",
            "artifact",
            "protected_main",
        ]:
            fail(f"{phase} protected pre-tag API response set differs")
        response_times: list[int] = []
        request_ids: list[str] = []
        for row in responses:
            item = exact_object(
                row,
                {
                    "label",
                    "body_sha256",
                    "response_unix",
                    "request_id",
                    "cache_control",
                    "age",
                },
                f"{phase} protected pre-tag API response",
            )
            require_hash(item["body_sha256"], f"{phase} API body digest")
            if (
                isinstance(item["age"], bool)
                or item["age"] != 0
                or isinstance(item["response_unix"], bool)
                or not isinstance(item["response_unix"], int)
                or item["response_unix"] <= 0
                or not isinstance(item["request_id"], str)
                or re.fullmatch(r"[A-F0-9:-]{8,128}", item["request_id"]) is None
                or not isinstance(item["cache_control"], str)
            ):
                fail(f"{phase} protected pre-tag API response is cached or untimestamped")
            response_times.append(item["response_unix"])
            request_ids.append(item["request_id"])
        if live["api_verified_at_unix"] != min(response_times):
            fail(f"{phase} protected pre-tag live timestamp does not cover every response")
        if len(set(request_ids)) != len(request_ids):
            fail(f"{phase} protected pre-tag API request identities are not unique")
        values.append(proof)
        response_observations.append((response_times, request_ids, responses))
    initial_value, final_value = values
    initial_invariant = {
        key: value
        for key, value in initial_value["live"].items()
        if key != "api_verified_at_unix"
    }
    final_invariant = {
        key: value
        for key, value in final_value["live"].items()
        if key != "api_verified_at_unix"
    }
    if (
        initial_invariant != final_invariant
        or initial_value["artifact"] != final_value["artifact"]
        or {
            key: value
            for key, value in initial_value["api"].items()
            if key != "responses"
        }
        != {
            key: value
            for key, value in final_value["api"].items()
            if key != "responses"
        }
    ):
        fail("initial/final protected pre-tag provenance invariants differ")
    initial_times, initial_requests, initial_responses = response_observations[0]
    final_times, final_requests, final_responses = response_observations[1]
    if min(final_times) < max(initial_times):
        fail("final protected pre-tag provenance predates the initial proof")
    if set(initial_requests) & set(final_requests) or initial_responses == final_responses:
        fail("final protected pre-tag provenance did not use fresh API requests")
    return initial_value, final_value


def read_regular_nofollow(
    path: Path,
    *,
    label: str,
    maximum: int,
    exact_mode: int | None = None,
    reject_group_world_writable: bool = True,
    require_single_link: bool = False,
) -> bytes:
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        fail(f"{label} cannot be opened through the no-follow boundary: {error.strerror}")
    try:
        details = os.fstat(descriptor)
        if not stat.S_ISREG(details.st_mode) or details.st_size <= 0 or details.st_size > maximum:
            fail(f"{label} is empty, oversized, or not a regular file")
        if reject_group_world_writable and stat.S_IMODE(details.st_mode) & 0o022:
            fail(f"{label} must not be group/world writable")
        if exact_mode is not None and stat.S_IMODE(details.st_mode) != exact_mode:
            fail(f"{label} must be mode {exact_mode:04o}")
        if exact_mode is not None and details.st_uid != os.getuid():
            fail(f"{label} must be owned by the restore operator")
        if require_single_link and details.st_nlink != 1:
            fail(f"{label} must have exactly one hard link")
        chunks: list[bytes] = []
        remaining = maximum + 1
        while remaining:
            chunk = os.read(descriptor, min(remaining, 1024 * 1024))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        payload = b"".join(chunks)
        after = os.fstat(descriptor)
        identity_before = (
            details.st_dev,
            details.st_ino,
            details.st_size,
            details.st_mtime_ns,
            details.st_ctime_ns,
            stat.S_IMODE(details.st_mode),
            details.st_nlink,
        )
        identity_after = (
            after.st_dev,
            after.st_ino,
            after.st_size,
            after.st_mtime_ns,
            after.st_ctime_ns,
            stat.S_IMODE(after.st_mode),
            after.st_nlink,
        )
        if len(payload) != details.st_size or len(payload) > maximum or identity_before != identity_after:
            fail(f"{label} changed or exceeded its read bound")
        return payload
    finally:
        os.close(descriptor)


def load_canonical_json(path: Path, *, label: str, maximum: int = MAX_JSON_BYTES) -> dict[str, Any]:
    raw = read_regular_nofollow(path, label=label, maximum=maximum)
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError):
        fail(f"{label} is not valid UTF-8 JSON")
    if not isinstance(value, dict) or raw != canonical_json_bytes(value):
        fail(f"{label} is not a canonical JSON object")
    return value


def sanitized_env(
    *,
    remote: bool = False,
    tool_directory: Path | None = None,
) -> dict[str, str]:
    environment: dict[str, str] = {}
    if remote:
        if tool_directory is None:
            fail("remote command environment requires one private tool directory")
        # Do not expose the caller's ~/.ssh tree to OpenSSH.  The command also
        # uses -F /dev/null, one explicit IdentityFile, IdentityAgent=none and
        # an exact UserKnownHostsFile, but HOME remains private as defense in
        # depth against any future client-side default lookup.
        environment["HOME"] = str(tool_directory)
    environment["PATH"] = (
        f"{tool_directory}:/usr/bin:/bin:/usr/sbin:/sbin"
        if tool_directory is not None
        else "/usr/bin:/bin:/usr/sbin:/sbin"
    )
    environment.update({"LANG": "C", "LC_ALL": "C"})
    if not remote:
        environment["OPENSSL_CONF"] = "/dev/null"
    return environment


def secure_tool_ancestry(path: Path, label: str) -> os.stat_result:
    if not path.is_absolute() or Path(os.path.abspath(path)) != path:
        fail(f"{label} path must be normalized and absolute")
    current = Path(path.anchor)
    for part in path.parts[1:-1]:
        current /= part
        try:
            details = current.lstat()
        except OSError:
            fail(f"{label} has an unavailable ancestor")
        permissions = stat.S_IMODE(details.st_mode)
        if (
            stat.S_ISLNK(details.st_mode)
            or not stat.S_ISDIR(details.st_mode)
            or details.st_uid not in (0, os.getuid())
            or permissions & 0o002
            # Homebrew's Cellar is conventionally 0775 and operator-owned.
            # Its reviewed file is still copied through one no-follow FD and
            # accepted only when its immutable expected digest matches.  A
            # group-writable directory not owned by this operator remains an
            # unsafe substitution boundary.
            or (permissions & 0o020 and details.st_uid != os.getuid())
        ):
            fail(f"{label} has an unsafe owner/mode/type/symlink in its ancestry")
    try:
        details = path.lstat()
    except OSError:
        fail(f"{label} is unavailable")
    if (
        stat.S_ISLNK(details.st_mode)
        or not stat.S_ISREG(details.st_mode)
        or details.st_uid not in (0, os.getuid())
        or stat.S_IMODE(details.st_mode) & 0o022
        or details.st_nlink != 1
    ):
        fail(f"{label} must be a root/operator-owned, single-link, non-writable regular file")
    return details


def pin_reviewed_file(
    source: Path,
    destination: Path,
    expected_sha256: str,
    *,
    label: str,
    maximum: int,
    executable: bool,
) -> str:
    expected = require_hash(expected_sha256, f"expected {label} digest")
    before = secure_tool_ancestry(source, label)
    if executable and not os.access(source, os.X_OK):
        fail(f"{label} is not executable")
    raw = read_regular_nofollow(source, label=label, maximum=maximum)
    after = secure_tool_ancestry(source, label)
    identity = lambda item: (
        item.st_dev,
        item.st_ino,
        item.st_size,
        item.st_mtime_ns,
        item.st_ctime_ns,
        stat.S_IMODE(item.st_mode),
        item.st_nlink,
    )
    if identity(before) != identity(after) or hashlib.sha256(raw).hexdigest() != expected:
        fail(f"{label} changed or differs from its reviewed SHA-256")
    mode = 0o500 if executable else 0o400
    create_file(destination, raw, mode)
    pinned = read_regular_nofollow(
        destination, label=f"pinned {label}", maximum=maximum, exact_mode=mode
    )
    if hashlib.sha256(pinned).hexdigest() != expected:
        fail(f"pinned {label} differs from its reviewed SHA-256")
    return expected


def prove_pinned_file(
    path: Path,
    expected_sha256: str,
    *,
    label: str,
    maximum: int,
    mode: int,
) -> None:
    raw = read_regular_nofollow(path, label=label, maximum=maximum, exact_mode=mode)
    if hashlib.sha256(raw).hexdigest() != expected_sha256:
        fail(f"{label} changed after its reviewed execution")


def run_local(
    command: Sequence[str],
    *,
    label: str,
    stdout: int | BinaryIO = subprocess.PIPE,
    input_bytes: bytes | None = None,
    timeout: int = 60,
    extra_env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[bytes]:
    environment = sanitized_env()
    if extra_env:
        environment.update(extra_env)
    try:
        result = subprocess.run(
            list(command),
            input=input_bytes,
            stdout=stdout,
            stderr=subprocess.PIPE,
            env=environment,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        fail(f"{label} could not complete")
    if result.returncode != 0:
        fail(f"{label} failed closed")
    return result


@dataclass(frozen=True)
class OpenSSLRuntime:
    executable: Path
    root: Path
    library_root: Path
    libssl_name: str
    libcrypto_name: str
    digests: dict[str, str]


def openssl_loader_environment(runtime: OpenSSLRuntime) -> dict[str, str]:
    environment = {
        "DYLD_LIBRARY_PATH": str(runtime.library_root),
        "LD_LIBRARY_PATH": str(runtime.library_root),
        "OPENSSL_CONF": "/dev/null",
        "OPENSSL_MODULES": "/dev/null",
        "OPENSSL_ENGINES": "/dev/null",
    }
    if sys.platform == "darwin":
        environment["DYLD_PRINT_LIBRARIES"] = "1"
    elif sys.platform.startswith("linux"):
        environment["LD_DEBUG"] = "libs"
    else:
        fail("reviewed OpenSSL loader proof supports only macOS or Linux")
    return environment


def run_openssl(
    runtime: OpenSSLRuntime,
    arguments: Sequence[str],
    *,
    label: str,
    stdout: int | BinaryIO = subprocess.PIPE,
    input_bytes: bytes | None = None,
    timeout: int = 60,
) -> subprocess.CompletedProcess[bytes]:
    prove_openssl_runtime(
        runtime.root,
        runtime.digests,
        runtime.libssl_name,
        runtime.libcrypto_name,
    )
    result = run_local(
        [str(runtime.executable), *arguments],
        label=label,
        stdout=stdout,
        input_bytes=input_bytes,
        timeout=timeout,
        extra_env=openssl_loader_environment(runtime),
    )
    for dependency in (
        runtime.library_root / runtime.libssl_name,
        runtime.library_root / runtime.libcrypto_name,
    ):
        if str(dependency).encode() not in result.stderr:
            fail(f"{label} did not load the reviewed private {dependency.name}")
    for line in result.stderr.splitlines():
        for name in (runtime.libssl_name, runtime.libcrypto_name):
            if name.encode() in line and str(runtime.library_root / name).encode() not in line:
                fail(f"{label} resolved an unreviewed OpenSSL dependency path")
    prove_openssl_runtime(
        runtime.root,
        runtime.digests,
        runtime.libssl_name,
        runtime.libcrypto_name,
    )
    return result


def validate_rewrap_receipt(
    path: Path,
    *,
    source_commit: str,
    expected_cms_sha256: str,
) -> dict[str, Any]:
    receipt = exact_object(
        load_canonical_json(path, label="rewrap receipt"),
        {
            "schema",
            "source_commit",
            "source_ciphertext_sha256",
            "restore_cert_sha256",
            "cms_sha256",
            "key_transport",
            "content_encryption",
        },
        "rewrap receipt",
    )
    if receipt["schema"] != REWRAP_SCHEMA:
        fail("rewrap receipt schema is unsupported")
    if require_commit(receipt["source_commit"], "rewrap source commit") != source_commit:
        fail("rewrap receipt is not bound to the selected protected-main commit")
    require_hash(receipt["source_ciphertext_sha256"], "source ciphertext digest")
    require_hash(receipt["restore_cert_sha256"], "restore certificate digest")
    if require_hash(receipt["cms_sha256"], "CMS digest") != expected_cms_sha256:
        fail("rewrap receipt CMS digest differs from the authorized artifact")
    if receipt["key_transport"] != "RSA-OAEP-SHA256":
        fail("rewrap receipt key-transport profile differs")
    if receipt["content_encryption"] != "AES-256-GCM":
        fail("rewrap receipt content-encryption profile differs")
    return receipt


def validate_restore_identity(
    openssl: OpenSSLRuntime,
    certificate: Path,
    private_key: Path,
    expected_cert_sha256: str,
) -> None:
    cert_bytes = read_regular_nofollow(
        certificate, label="restore certificate", maximum=64 * 1024, exact_mode=0o600
    )
    key_bytes = read_regular_nofollow(
        private_key, label="restore private key", maximum=64 * 1024, exact_mode=0o600
    )
    if hashlib.sha256(cert_bytes).hexdigest() != expected_cert_sha256:
        fail("restore certificate digest differs from the rewrap receipt")
    if cert_bytes.count(b"-----BEGIN CERTIFICATE-----") != 1 or cert_bytes.count(
        b"-----END CERTIFICATE-----"
    ) != 1 or b"PRIVATE KEY-----" in cert_bytes:
        fail("restore certificate PEM membership is not exactly one public certificate")

    cert_text = run_openssl(
        openssl,
        ["x509", "-in", str(certificate), "-noout", "-text"],
        label="restore certificate inspection",
    ).stdout
    match = re.search(rb"Public-Key: \(([0-9]+) bit\)", cert_text)
    if b"Public Key Algorithm: rsaEncryption" not in cert_text or match is None or int(match.group(1)) < 3072:
        fail("restore certificate does not contain the approved RSA-3072-or-larger key")
    run_openssl(
        openssl,
        ["x509", "-in", str(certificate), "-checkend", "86400", "-noout"],
        label="restore certificate validity check",
    )
    cert_public = run_openssl(
        openssl,
        ["x509", "-in", str(certificate), "-pubkey", "-noout"],
        label="restore certificate public-key extraction",
    ).stdout
    private_public = run_openssl(
        openssl,
        ["pkey", "-in", str(private_key), "-pubout"],
        label="restore private-key public derivation",
    ).stdout
    if not cert_public or cert_public != private_public:
        fail("restore private key does not match the pinned certificate")
    if read_regular_nofollow(
        certificate, label="pinned restore certificate", maximum=64 * 1024, exact_mode=0o600
    ) != cert_bytes or read_regular_nofollow(
        private_key, label="pinned restore private key", maximum=64 * 1024, exact_mode=0o600
    ) != key_bytes:
        fail("pinned restore identity changed during OpenSSL verification")


def validate_cms_profile(openssl: OpenSSLRuntime, cms_path: Path) -> None:
    before = read_regular_nofollow(
        cms_path, label="pinned CMS ciphertext", maximum=MAX_CMS_BYTES, exact_mode=0o600
    )
    cms_text = run_openssl(
        openssl,
        ["cms", "-cmsout", "-inform", "DER", "-in", str(cms_path), "-print", "-noout"],
        label="CMS profile inspection",
    ).stdout
    if cms_text.count(b"d.ktri") != 1 and cms_text.count(b"ktri") != 1:
        # OpenSSL output varies by supported version; the exact one-recipient
        # check below remains authoritative when the debug label is absent.
        if cms_text.count(b"issuerAndSerialNumber") != 1:
            fail("CMS artifact does not contain exactly one key-transport recipient")
    if cms_text.count(b"issuerAndSerialNumber") != 1:
        fail("CMS artifact does not contain exactly one certificate recipient")
    if b"algorithm: rsaesOaep" not in cms_text:
        fail("CMS artifact does not use RSA-OAEP key transport")
    if b"algorithm: aes-256-gcm" not in cms_text:
        fail("CMS artifact does not use authenticated AES-256-GCM content encryption")
    if cms_text.count(b"OBJECT            :sha256") < 2:
        fail("CMS RSA-OAEP parameters do not use SHA-256 for digest and MGF1")
    if read_regular_nofollow(
        cms_path, label="pinned CMS ciphertext", maximum=MAX_CMS_BYTES, exact_mode=0o600
    ) != before:
        fail("pinned CMS ciphertext changed during profile inspection")


def validate_member_path(name: str, index: int) -> tuple[str, ...]:
    try:
        encoded = name.encode("utf-8", errors="strict")
    except UnicodeError:
        fail(f"archive member {index} path is not valid UTF-8")
    if not encoded or len(encoded) > MAX_PATH_BYTES:
        fail(f"archive member {index} path is empty or oversized")
    if any(byte < 32 or byte == 127 for byte in encoded):
        fail(f"archive member {index} path contains a control byte")
    pure = PurePosixPath(name)
    if (
        pure.is_absolute()
        or "\\" in name
        or ":" in name
        or any(part in ("", ".", "..") for part in pure.parts)
        or len(pure.parts) > MAX_PATH_DEPTH
    ):
        fail(f"archive member {index} path is unsafe")
    return tuple(pure.parts)


def open_child_directory(parent_fd: int, name: str) -> int:
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
    return os.open(name, flags, dir_fd=parent_fd)


def write_all(descriptor: int, payload: bytes) -> None:
    offset = 0
    while offset < len(payload):
        try:
            written = os.write(descriptor, payload[offset:])
        except InterruptedError:
            continue
        if written <= 0:
            fail("create-only publication made no forward write progress")
        offset += written


def create_member_file(root_fd: int, parts: tuple[str, ...], source: BinaryIO, size: int) -> None:
    directory_fd = os.dup(root_fd)
    try:
        for part in parts[:-1]:
            try:
                os.mkdir(part, 0o700, dir_fd=directory_fd)
            except FileExistsError:
                pass
            child_fd = open_child_directory(directory_fd, part)
            os.close(directory_fd)
            directory_fd = child_fd
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
        descriptor = os.open(parts[-1], flags, 0o600, dir_fd=directory_fd)
        try:
            remaining = size
            while remaining:
                chunk = source.read(min(remaining, 64 * 1024))
                if not chunk:
                    fail("archive member ended before its declared size")
                write_all(descriptor, chunk)
                remaining -= len(chunk)
            if source.read(1):
                fail("archive member exceeded its declared size")
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)


def extract_six_keys(plain_tar: Path, extraction_root: Path) -> list[Path]:
    raw = read_regular_nofollow(plain_tar, label="decrypted vault tar", maximum=MAX_TAR_BYTES, exact_mode=0o600)
    if len(raw) % 512 != 0:
        fail("decrypted vault is not a bounded plain tar")
    names: set[str] = set()
    folded: set[str] = set()
    files: list[tuple[tarfile.TarInfo, tuple[str, ...]]] = []
    total = 0
    try:
        # Parse and extract only from the already bounded no-follow read.  The
        # path is never reopened after validation, eliminating a pathname
        # substitution gap between the tar checks and member reads.
        with tarfile.open(fileobj=io.BytesIO(raw), mode="r:") as archive:
            if archive.pax_headers:
                fail("decrypted vault has unsupported global PAX metadata")
            members = archive.getmembers()
            if not members or len(members) > MAX_MEMBERS:
                fail("decrypted vault member count is outside the bounded contract")
            for index, member in enumerate(members, start=1):
                parts = validate_member_path(member.name, index)
                normalized = "/".join(parts)
                if normalized in names or normalized.casefold() in folded:
                    fail(f"archive member {index} duplicates another path")
                names.add(normalized)
                folded.add(normalized.casefold())
                if member.pax_headers:
                    fail(f"archive member {index} has unsupported PAX metadata")
                if member.isdir():
                    if member.type != tarfile.DIRTYPE or member.mode & 0o7777 not in (0o500, 0o700):
                        fail(f"archive member {index} is not a private canonical directory")
                    continue
                if (
                    member.type not in (tarfile.REGTYPE, tarfile.AREGTYPE)
                    or not member.isfile()
                    or member.issym()
                    or member.islnk()
                    or member.issparse()
                ):
                    fail(f"archive member {index} is not a nonsparse regular file")
                if member.mode & 0o7777 not in (0o400, 0o600):
                    fail(f"archive member {index} does not have private permissions")
                if member.size <= 0 or member.size > MAX_KEY_BYTES:
                    fail(f"archive member {index} size is outside the keyfile contract")
                total += member.size
                if total > MAX_TOTAL_KEY_BYTES:
                    fail("decrypted vault key bytes exceed the bounded contract")
                files.append((member, parts))
            if len(files) != 6:
                fail("decrypted vault must contain exactly six private keyfiles")
            root_fd = os.open(
                extraction_root,
                os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
            )
            try:
                for member, parts in files:
                    source = archive.extractfile(member)
                    if source is None:
                        fail("a declared keyfile has no readable tar payload")
                    with source:
                        create_member_file(root_fd, parts, source, member.size)
                os.fsync(root_fd)
            finally:
                os.close(root_fd)
    except (OSError, tarfile.TarError, UnicodeError):
        fail("decrypted vault is not a valid bounded plain tar")
    return [extraction_root.joinpath(*parts) for _, parts in files]


def validate_pretag_cli(
    cli: Path,
    metadata_path: Path,
    genesis: Path,
    *,
    source_commit: str,
    expected_cli_sha256: str,
    expected_genesis_sha256: str,
) -> dict[str, Any]:
    cli_raw = read_regular_nofollow(cli, label="pre-tag ARC CLI", maximum=256 * 1024 * 1024)
    if hashlib.sha256(cli_raw).hexdigest() != expected_cli_sha256:
        fail("pre-tag ARC CLI bytes differ from the selected digest")
    if not os.access(cli, os.X_OK):
        fail("pre-tag ARC CLI is not executable")
    metadata = exact_object(
        load_canonical_json(metadata_path, label="pre-tag build metadata"),
        {
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
        },
        "pre-tag build metadata",
    )
    expected = {
        "schema": "arc.pretag.artifact.v1",
        "kind": "headless",
        "repository": REPOSITORY,
        "commit": source_commit,
        "platform": "linux-x86_64",
        "rust_target": "x86_64-unknown-linux-gnu",
        "version": VERSION,
    }
    if any(metadata.get(field) != value for field, value in expected.items()):
        fail("pre-tag build metadata is not the exact Linux x86_64 protected-main group")
    if (
        isinstance(metadata["workflow_run_id"], bool)
        or not isinstance(metadata["workflow_run_id"], int)
        or metadata["workflow_run_id"] <= 0
        or isinstance(metadata["workflow_run_attempt"], bool)
        or not isinstance(metadata["workflow_run_attempt"], int)
        or metadata["workflow_run_attempt"] <= 0
    ):
        fail("pre-tag workflow run identity is invalid")
    files = metadata["files"]
    expected_names = {"arc-node-linux-x86_64", "arc-cli-linux-x86_64", "genesis.toml"}
    if not isinstance(files, dict) or set(files) != expected_names:
        fail("pre-tag Linux headless file set differs")
    for name, digest in files.items():
        require_hash(digest, f"pre-tag digest for {name}")
    if files["arc-cli-linux-x86_64"] != expected_cli_sha256:
        fail("pre-tag metadata does not bind the selected ARC CLI")
    genesis_raw = read_regular_nofollow(genesis, label="complete genesis", maximum=1024 * 1024)
    if hashlib.sha256(genesis_raw).hexdigest() != expected_genesis_sha256:
        fail("complete genesis bytes differ from the selected digest")
    if files["genesis.toml"] != expected_genesis_sha256:
        fail("pre-tag metadata does not bind the selected complete genesis")
    return metadata


def validate_complete_genesis(path: Path) -> list[dict[str, Any]]:
    raw = read_regular_nofollow(path, label="complete genesis", maximum=1024 * 1024)
    try:
        genesis = tomllib.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, tomllib.TOMLDecodeError):
        fail("complete genesis is not valid UTF-8 TOML")
    if not isinstance(genesis, dict):
        fail("complete genesis root is invalid")
    chain = genesis.get("chain")
    validators = genesis.get("validators")
    accounts = genesis.get("accounts")
    if not isinstance(chain, dict) or chain.get("validator_set_complete") is not True:
        fail("genesis validator set is not explicitly complete")
    if not isinstance(validators, list) or len(validators) != 6:
        fail("complete genesis must contain exactly six validators")
    if not isinstance(accounts, list):
        fail("complete genesis accounts are missing")
    account_addresses = [row.get("address", "").lower() for row in accounts if isinstance(row, dict)]
    if len(account_addresses) != len(set(account_addresses)):
        fail("complete genesis contains duplicate account addresses")
    result: list[dict[str, Any]] = []
    for index, ((node, expected_address), row) in enumerate(zip(PRODUCTION_VALIDATORS, validators), start=1):
        if not isinstance(row, dict) or set(row) != {"address", "stake"}:
            fail(f"genesis validator {index} fields are not exact")
        address = row.get("address")
        stake = row.get("stake")
        if not isinstance(address, str) or address.lower() != expected_address:
            fail(f"genesis validator {index} differs from the reviewed {node} address")
        if isinstance(stake, bool) or not isinstance(stake, int) or stake <= 0:
            fail(f"genesis validator {index} stake is invalid")
        if account_addresses.count(expected_address) != 1:
            fail(f"genesis validator {index} does not have one matching account")
        result.append({"node": node, "address": expected_address, "stake": stake})
    total = sum(row["stake"] for row in result)
    if any((total - row["stake"]) * 3 <= total * 2 for row in result):
        fail("genesis stakes do not retain strict quorum during one-node maintenance")
    return result


@dataclass(frozen=True)
class VerifiedKey:
    node: str
    address: str
    public_key: str
    source_path: Path
    sha256: str


def verify_keyfiles(
    cli: Path,
    paths: Sequence[Path],
    *,
    expected_cli_sha256: str,
) -> dict[str, VerifiedKey]:
    verified: dict[str, VerifiedKey] = {}
    for index, path in enumerate(paths, start=1):
        raw = read_regular_nofollow(
            path, label=f"extracted keyfile {index}", maximum=MAX_KEY_BYTES, exact_mode=0o600
        )
        try:
            value = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            fail(f"extracted keyfile {index} is not valid UTF-8 JSON")
        row = exact_object(
            value,
            {"scheme", "secret_key", "public_key", "address"},
            f"extracted keyfile {index}",
        )
        if row["scheme"] != "ed25519":
            fail(f"extracted keyfile {index} is not Ed25519")
        for field in ("secret_key", "public_key", "address"):
            if not isinstance(row[field], str) or LOWER_HASH_RE.fullmatch(row[field]) is None:
                fail(f"extracted keyfile {index} has an invalid {field} encoding")
        address = row["address"]
        node = ADDRESS_TO_NODE.get(address)
        if node is None:
            fail(f"extracted keyfile {index} is not a reviewed production validator")
        prove_pinned_file(
            cli,
            expected_cli_sha256,
            label="pinned pre-tag ARC CLI",
            maximum=256 * 1024 * 1024,
            mode=0o500,
        )
        result = run_local(
            [str(cli), "keygen", "--verify-keyfile", str(path)],
            label=f"exact pre-tag CLI verification for keyfile {index}",
        )
        try:
            cli_address = result.stdout.decode("ascii").strip()
        except UnicodeDecodeError:
            fail(f"exact pre-tag CLI returned a non-ASCII result for keyfile {index}")
        if cli_address != address or LOWER_HASH_RE.fullmatch(cli_address) is None:
            fail(f"exact pre-tag CLI address differs for keyfile {index}")
        if read_regular_nofollow(
            path, label=f"pinned keyfile {index}", maximum=MAX_KEY_BYTES, exact_mode=0o600
        ) != raw:
            fail(f"pinned keyfile {index} changed during exact CLI verification")
        cli_after = read_regular_nofollow(
            cli, label="pinned pre-tag ARC CLI", maximum=256 * 1024 * 1024, exact_mode=0o500
        )
        if hashlib.sha256(cli_after).hexdigest() != expected_cli_sha256:
            fail("pinned pre-tag ARC CLI changed during key verification")
        if node in verified:
            fail("decrypted vault contains duplicate validator identities")
        verified[node] = VerifiedKey(
            node=node,
            address=address,
            public_key=row["public_key"],
            source_path=path,
            sha256=hashlib.sha256(raw).hexdigest(),
        )
    if set(verified) != {node for node, _ in PRODUCTION_VALIDATORS}:
        fail("decrypted vault does not contain the exact reviewed six-validator set")
    return verified


def sync_directory(path: Path) -> None:
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def create_file(path: Path, payload: bytes, mode: int) -> None:
    descriptor = os.open(
        path,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
        mode,
    )
    completed = False
    try:
        write_all(descriptor, payload)
        os.fsync(descriptor)
        os.fchmod(descriptor, mode)
        completed = True
    finally:
        os.close(descriptor)
        if not completed:
            try:
                path.unlink()
            except OSError:
                pass


def pin_openssl_runtime(
    root: Path,
    *,
    executable: Path,
    executable_sha256: str,
    libssl: Path,
    libssl_sha256: str,
    libcrypto: Path,
    libcrypto_sha256: str,
) -> tuple[OpenSSLRuntime, dict[str, str]]:
    binary = root / "openssl.bin"
    library_root = root / "openssl-libraries"
    library_root.mkdir(mode=0o700)
    executable_digest = pin_reviewed_file(
        executable,
        binary,
        executable_sha256,
        label="reviewed OpenSSL executable",
        maximum=32 * 1024 * 1024,
        executable=True,
    )
    library_rows: dict[str, str] = {}
    for name, source, expected in (
        ("libssl", libssl, libssl_sha256),
        ("libcrypto", libcrypto, libcrypto_sha256),
    ):
        destination = library_root / source.name
        if destination.name in library_rows:
            fail("reviewed OpenSSL dependency basenames collide")
        library_rows[destination.name] = pin_reviewed_file(
            source,
            destination,
            expected,
            label=f"reviewed OpenSSL {name} dependency",
            maximum=64 * 1024 * 1024,
            executable=False,
        )
    sync_directory(library_root)
    sync_directory(root)
    digests = {
        "openssl_sha256": executable_digest,
        "openssl_libssl_sha256": library_rows[libssl.name],
        "openssl_libcrypto_sha256": library_rows[libcrypto.name],
    }
    runtime = OpenSSLRuntime(
        executable=binary,
        root=root,
        library_root=library_root,
        libssl_name=libssl.name,
        libcrypto_name=libcrypto.name,
        digests=digests,
    )
    # Prove both the copied image and the loader's resolved private dependency
    # paths before any certificate, private key, or CMS byte is touched.
    run_openssl(runtime, ["version"], label="pinned OpenSSL loader proof")
    return runtime, digests


def prove_openssl_runtime(root: Path, digests: dict[str, str], libssl_name: str, libcrypto_name: str) -> None:
    prove_pinned_file(
        root / "openssl.bin",
        digests["openssl_sha256"],
        label="pinned OpenSSL executable",
        maximum=32 * 1024 * 1024,
        mode=0o500,
    )
    for name, field in (
        (libssl_name, "openssl_libssl_sha256"),
        (libcrypto_name, "openssl_libcrypto_sha256"),
    ):
        prove_pinned_file(
            root / "openssl-libraries" / name,
            digests[field],
            label=f"pinned OpenSSL dependency {name}",
            maximum=64 * 1024 * 1024,
            mode=0o400,
        )


def create_private_output(path: Path) -> Path:
    if not path.is_absolute() or path.name in ("", ".", ".."):
        fail("restore output must be an absolute new directory")
    try:
        parent = path.parent.resolve(strict=True)
    except OSError:
        fail("restore output parent does not exist")
    if not parent.is_dir():
        fail("restore output parent is not a directory")
    parent_fd = os.open(
        parent,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    try:
        os.mkdir(path.name, 0o700, dir_fd=parent_fd)
        os.fsync(parent_fd)
    except FileExistsError:
        fail("restore output already exists; replacement and merge are forbidden")
    finally:
        os.close(parent_fd)
    result = parent / path.name
    if stat.S_IMODE(result.stat().st_mode) != 0o700:
        fail("restore output could not be created mode 0700")
    return result


def publish_restore_output(
    output: Path,
    verified: dict[str, VerifiedKey],
    genesis_rows: list[dict[str, Any]],
    *,
    source_commit: str,
    cms_sha256: str,
    source_ciphertext_sha256: str,
    restore_cert_sha256: str,
    arc_cli_sha256: str,
    genesis_sha256: str,
    openssl_digests: dict[str, str],
    pretag_initial_provenance: dict[str, Any],
    pretag_final_provenance: dict[str, Any],
) -> None:
    keys_dir = output / "keys"
    keys_dir.mkdir(mode=0o700)
    private_rows: list[dict[str, Any]] = []
    public_rows: list[dict[str, Any]] = []
    for genesis_row in genesis_rows:
        node = genesis_row["node"]
        key = verified[node]
        relative = f"keys/{node}.validator-key.json"
        destination = output / relative
        raw = read_regular_nofollow(
            key.source_path, label=f"verified {node} keyfile", maximum=MAX_KEY_BYTES, exact_mode=0o600
        )
        create_file(destination, raw, 0o600)
        if sha256_file(destination) != key.sha256:
            fail("published private keyfile changed during create-only copy")
        private_rows.append(
            {
                "node": node,
                "key_file": relative,
                "address": key.address,
                "keyfile_sha256": key.sha256,
            }
        )
        public_rows.append(
            {
                "address": key.address,
                "public_key": key.public_key,
                "stake": genesis_row["stake"],
            }
        )
    sync_directory(keys_dir)
    public_payload = canonical_json_bytes(public_rows)
    create_file(output / "validator-public-keys.json", public_payload, 0o444)
    private_receipt = {
        "schema": RESTORE_SCHEMA,
        "source_commit": source_commit,
        "cms_sha256": cms_sha256,
        "source_ciphertext_sha256": source_ciphertext_sha256,
        "restore_cert_sha256": restore_cert_sha256,
        "arc_cli_sha256": arc_cli_sha256,
        "genesis_sha256": genesis_sha256,
        **openssl_digests,
        "pretag_initial_provenance": pretag_initial_provenance,
        "pretag_final_provenance": pretag_final_provenance,
        "validators": private_rows,
    }
    create_file(output / "RESTORE-RECEIPT.json", canonical_json_bytes(private_receipt), 0o600)
    sync_directory(output)


def secure_cleanup(path: Path) -> None:
    """Best-effort unlink through stable directory FDs without following links.

    Overwriting by pathname is both ineffective on modern storage and unsafe:
    a concurrent substitution could redirect it outside the private root.  All
    secret removal here is therefore unlink-only and relative to no-follow
    directory descriptors.
    """

    def clear_directory(descriptor: int) -> None:
        try:
            names = [entry.name for entry in os.scandir(descriptor)]
        except OSError:
            return
        for name in names:
            try:
                details = os.stat(name, dir_fd=descriptor, follow_symlinks=False)
                if stat.S_ISDIR(details.st_mode):
                    child = os.open(
                        name,
                        os.O_RDONLY
                        | getattr(os, "O_DIRECTORY", 0)
                        | getattr(os, "O_NOFOLLOW", 0),
                        dir_fd=descriptor,
                    )
                    try:
                        opened = os.fstat(child)
                        if (opened.st_dev, opened.st_ino) != (
                            details.st_dev,
                            details.st_ino,
                        ):
                            continue
                        clear_directory(child)
                    finally:
                        os.close(child)
                    os.rmdir(name, dir_fd=descriptor)
                else:
                    if stat.S_ISREG(details.st_mode) and details.st_nlink == 1:
                        file_descriptor = os.open(
                            name,
                            os.O_WRONLY | getattr(os, "O_NOFOLLOW", 0),
                            dir_fd=descriptor,
                        )
                        try:
                            opened = os.fstat(file_descriptor)
                            if (
                                (opened.st_dev, opened.st_ino)
                                == (details.st_dev, details.st_ino)
                                and opened.st_nlink == 1
                            ):
                                remaining = min(opened.st_size, MAX_TAR_BYTES)
                                zeroes = b"\0" * min(64 * 1024, remaining)
                                while remaining:
                                    written = os.write(
                                        file_descriptor, zeroes[:remaining]
                                    )
                                    if written <= 0:
                                        break
                                    remaining -= written
                                os.fsync(file_descriptor)
                        finally:
                            os.close(file_descriptor)
                    os.unlink(name, dir_fd=descriptor)
            except OSError:
                # Cleanup must never turn a primary validation failure into a
                # broader or path-following mutation attempt.
                continue
        try:
            os.fsync(descriptor)
        except OSError:
            pass

    if not path.is_absolute() or path.name in ("", ".", ".."):
        return
    parent_descriptor: int | None = None
    root_descriptor: int | None = None
    try:
        parent_descriptor = os.open(
            path.parent,
            os.O_RDONLY
            | getattr(os, "O_DIRECTORY", 0)
            | getattr(os, "O_NOFOLLOW", 0),
        )
        root_descriptor = os.open(
            path.name,
            os.O_RDONLY
            | getattr(os, "O_DIRECTORY", 0)
            | getattr(os, "O_NOFOLLOW", 0),
            dir_fd=parent_descriptor,
        )
        root_details = os.fstat(root_descriptor)
        if (
            not stat.S_ISDIR(root_details.st_mode)
            or root_details.st_uid != os.getuid()
            or stat.S_IMODE(root_details.st_mode) != 0o700
        ):
            return
        root_identity = (root_details.st_dev, root_details.st_ino)
        clear_directory(root_descriptor)
        lexical = os.stat(
            path.name, dir_fd=parent_descriptor, follow_symlinks=False
        )
        if (lexical.st_dev, lexical.st_ino) == root_identity:
            os.rmdir(path.name, dir_fd=parent_descriptor)
            os.fsync(parent_descriptor)
    except OSError:
        pass
    finally:
        if root_descriptor is not None:
            os.close(root_descriptor)
        if parent_descriptor is not None:
            os.close(parent_descriptor)


def restore(args: argparse.Namespace) -> None:
    try:
        with artifact_provenance.pretag_actions_proof(
            raw_actions_zip=args.raw_actions_zip,
            expected_commit=args.source_main_sha,
            expected_run_id=args.pretag_run_id,
            expected_run_attempt=args.pretag_run_attempt,
            expected_artifact_id=args.pretag_artifact_id,
            kind="headless",
            platform="linux-x86_64",
            expected_version=VERSION,
            curl=args.curl,
            curl_sha256=args.curl_sha256,
            ca_bundle=args.ca_bundle,
            ca_bundle_sha256=args.ca_bundle_sha256,
        ) as proof:
            _restore_verified(args, proof)
    except artifact_provenance.ProvenanceError as error:
        fail(f"live protected preflight verification failed: {error}")


def _restore_verified(args: argparse.Namespace, proof: Any) -> None:
    source_commit = require_commit(args.source_main_sha, "source-main commit")
    expected_cms_sha256 = require_hash(args.expected_cms_sha256, "expected CMS digest")
    expected_cli_sha256 = proof.build_metadata["files"]["arc-cli-linux-x86_64"]
    expected_genesis_sha256 = proof.build_metadata["files"]["genesis.toml"]
    temporary_root = Path(tempfile.mkdtemp(prefix="arc-validator-vault-restore."))
    temporary_root.chmod(0o700)
    pinned = temporary_root / "pinned"
    pinned.mkdir(mode=0o700)
    plain_tar = temporary_root / "vault.tar"
    extracted = temporary_root / "extracted"
    extracted.mkdir(mode=0o700)
    output: Path | None = None
    try:
        openssl_root = pinned / "openssl-runtime"
        openssl_root.mkdir(mode=0o700)
        openssl, openssl_digests = pin_openssl_runtime(
            openssl_root,
            executable=args.openssl,
            executable_sha256=args.openssl_sha256,
            libssl=args.openssl_libssl,
            libssl_sha256=args.openssl_libssl_sha256,
            libcrypto=args.openssl_libcrypto,
            libcrypto_sha256=args.openssl_libcrypto_sha256,
        )
        cms_raw = read_regular_nofollow(
            args.cms, label="CMS ciphertext", maximum=MAX_CMS_BYTES, exact_mode=0o600
        )
        if hashlib.sha256(cms_raw).hexdigest() != expected_cms_sha256:
            fail("CMS ciphertext bytes differ from the authorized digest")
        create_file(pinned / "vault.tar.cms", cms_raw, 0o600)
        receipt_raw = read_regular_nofollow(
            args.rewrap_receipt, label="rewrap receipt", maximum=MAX_JSON_BYTES
        )
        create_file(pinned / "REWRAP-RECEIPT.json", receipt_raw, 0o600)
        certificate_raw = read_regular_nofollow(
            args.restore_certificate,
            label="restore certificate",
            maximum=64 * 1024,
            exact_mode=0o600,
        )
        private_key_raw = read_regular_nofollow(
            args.restore_private_key,
            label="restore private key",
            maximum=64 * 1024,
            exact_mode=0o600,
        )
        create_file(pinned / "restore.cert.pem", certificate_raw, 0o600)
        create_file(pinned / "restore.key.pem", private_key_raw, 0o600)
        cli_source = proof.payloads["arc-cli-linux-x86_64"]
        metadata_source = proof.build_metadata_path
        genesis_source = proof.payloads["genesis.toml"]
        cli_raw = read_regular_nofollow(
            cli_source, label="private staged pre-tag ARC CLI", maximum=256 * 1024 * 1024
        )
        if not os.access(cli_source, os.X_OK):
            fail("private staged pre-tag ARC CLI is not executable")
        create_file(pinned / "arc-cli-linux-x86_64", cli_raw, 0o500)
        metadata_raw = read_regular_nofollow(
            metadata_source, label="private staged pre-tag build metadata", maximum=MAX_JSON_BYTES
        )
        genesis_raw = read_regular_nofollow(
            genesis_source, label="private staged complete genesis", maximum=1024 * 1024
        )
        create_file(pinned / "BUILD-METADATA.json", metadata_raw, 0o600)
        create_file(pinned / "genesis.toml", genesis_raw, 0o600)
        sync_directory(pinned)

        receipt = validate_rewrap_receipt(
            pinned / "REWRAP-RECEIPT.json",
            source_commit=source_commit,
            expected_cms_sha256=expected_cms_sha256,
        )
        validate_restore_identity(
            openssl,
            pinned / "restore.cert.pem",
            pinned / "restore.key.pem",
            receipt["restore_cert_sha256"],
        )
        validate_cms_profile(openssl, pinned / "vault.tar.cms")
        validate_pretag_cli(
            pinned / "arc-cli-linux-x86_64",
            pinned / "BUILD-METADATA.json",
            pinned / "genesis.toml",
            source_commit=source_commit,
            expected_cli_sha256=expected_cli_sha256,
            expected_genesis_sha256=expected_genesis_sha256,
        )
        genesis_rows = validate_complete_genesis(pinned / "genesis.toml")

        descriptor = os.open(
            plain_tar,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
            0o600,
        )
        with os.fdopen(descriptor, "wb") as handle:
            run_openssl(
                openssl,
                [
                    "cms",
                    "-decrypt",
                    "-binary",
                    "-inform",
                    "DER",
                    "-recip",
                    str(pinned / "restore.cert.pem"),
                    "-inkey",
                    str(pinned / "restore.key.pem"),
                    "-in",
                    str(pinned / "vault.tar.cms"),
                ],
                label="CMS authenticated decryption",
                stdout=handle,
            )
            handle.flush()
            os.fsync(handle.fileno())
        if plain_tar.stat().st_size > MAX_TAR_BYTES:
            fail("decrypted vault exceeds the bounded plain-tar contract")
        if read_regular_nofollow(
            pinned / "vault.tar.cms",
            label="pinned CMS ciphertext",
            maximum=MAX_CMS_BYTES,
            exact_mode=0o600,
        ) != cms_raw or read_regular_nofollow(
            pinned / "restore.cert.pem",
            label="pinned restore certificate",
            maximum=64 * 1024,
            exact_mode=0o600,
        ) != certificate_raw or read_regular_nofollow(
            pinned / "restore.key.pem",
            label="pinned restore private key",
            maximum=64 * 1024,
            exact_mode=0o600,
        ) != private_key_raw:
            fail("pinned CMS restore inputs changed during authenticated decryption")
        prove_openssl_runtime(
            openssl_root,
            openssl_digests,
            args.openssl_libssl.name,
            args.openssl_libcrypto.name,
        )
        key_paths = extract_six_keys(plain_tar, extracted)
        verified = verify_keyfiles(
            pinned / "arc-cli-linux-x86_64",
            key_paths,
            expected_cli_sha256=expected_cli_sha256,
        )
        output = create_private_output(args.output_dir)
        final_reproof = proof.recheck()
        validate_provenance_pair(
            proof.provenance,
            final_reproof.value,
            source_commit=source_commit,
        )
        publish_restore_output(
            output,
            verified,
            genesis_rows,
            source_commit=source_commit,
            cms_sha256=expected_cms_sha256,
            source_ciphertext_sha256=receipt["source_ciphertext_sha256"],
            restore_cert_sha256=receipt["restore_cert_sha256"],
            arc_cli_sha256=expected_cli_sha256,
            genesis_sha256=expected_genesis_sha256,
            openssl_digests=openssl_digests,
            pretag_initial_provenance=proof.provenance,
            pretag_final_provenance=final_reproof.value,
        )
    except Exception:
        if output is not None:
            secure_cleanup(output)
        raise
    finally:
        secure_cleanup(temporary_root)
    print("validator vault restore complete: six reviewed identities verified")


def load_restore_receipt(
    path: Path,
    root: Path,
    pinned_keys_root: Path,
) -> tuple[dict[str, Any], dict[str, VerifiedKey]]:
    raw = read_regular_nofollow(path, label="private restore receipt", maximum=MAX_JSON_BYTES, exact_mode=0o600)
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError):
        fail("private restore receipt is not valid JSON")
    if raw != canonical_json_bytes(value):
        fail("private restore receipt is not canonical JSON")
    receipt = exact_object(
        value,
        {
            "schema",
            "source_commit",
            "cms_sha256",
            "source_ciphertext_sha256",
            "restore_cert_sha256",
            "arc_cli_sha256",
            "genesis_sha256",
            "openssl_sha256",
            "openssl_libssl_sha256",
            "openssl_libcrypto_sha256",
            "pretag_initial_provenance",
            "pretag_final_provenance",
            "validators",
        },
        "private restore receipt",
    )
    if receipt["schema"] != RESTORE_SCHEMA:
        fail("private restore receipt schema is unsupported")
    for field in (
        "cms_sha256",
        "source_ciphertext_sha256",
        "restore_cert_sha256",
        "arc_cli_sha256",
        "genesis_sha256",
        "openssl_sha256",
        "openssl_libssl_sha256",
        "openssl_libcrypto_sha256",
    ):
        require_hash(receipt[field], f"private restore receipt {field}")
    require_commit(receipt["source_commit"], "private restore source commit")
    validate_provenance_pair(
        receipt["pretag_initial_provenance"],
        receipt["pretag_final_provenance"],
        source_commit=receipt["source_commit"],
    )
    rows = receipt["validators"]
    if not isinstance(rows, list) or len(rows) != 6:
        fail("private restore receipt must contain exactly six validators")
    verified: dict[str, VerifiedKey] = {}
    for expected, row in zip(PRODUCTION_VALIDATORS, rows):
        item = exact_object(row, {"node", "key_file", "address", "keyfile_sha256"}, "private validator receipt")
        node, address = expected
        relative = f"keys/{node}.validator-key.json"
        if item["node"] != node or item["address"] != address or item["key_file"] != relative:
            fail("private restore receipt validator mapping differs from the reviewed order")
        digest = require_hash(item["keyfile_sha256"], f"{node} keyfile digest")
        key_path = root / relative
        raw_key = read_regular_nofollow(key_path, label=f"{node} restored keyfile", maximum=MAX_KEY_BYTES, exact_mode=0o600)
        if hashlib.sha256(raw_key).hexdigest() != digest:
            fail(f"{node} restored keyfile differs from its receipt")
        try:
            key_json = json.loads(raw_key)
        except (UnicodeDecodeError, json.JSONDecodeError):
            fail(f"{node} restored keyfile is invalid JSON")
        public_key = key_json.get("public_key") if isinstance(key_json, dict) else None
        if not isinstance(public_key, str) or LOWER_HASH_RE.fullmatch(public_key) is None:
            fail(f"{node} restored keyfile public key is invalid")
        pinned_path = pinned_keys_root / f"{node}.validator-key.json"
        create_file(pinned_path, raw_key, 0o600)
        verified[node] = VerifiedKey(node, address, public_key, pinned_path, digest)
    sync_directory(pinned_keys_root)
    return receipt, verified


def load_freeze_plan(path: Path, sidecar: Path, expected_sha256: str) -> tuple[Any, bytes, bytes]:
    expected = require_hash(expected_sha256, "expected freeze-plan digest")
    raw = read_regular_nofollow(
        path,
        label="sealed freeze plan",
        maximum=4 * 1024 * 1024,
        exact_mode=0o444,
    )
    if hashlib.sha256(raw).hexdigest() != expected:
        fail("sealed freeze-plan bytes differ from the expected digest")
    sidecar_raw = read_regular_nofollow(
        sidecar,
        label="freeze-plan checksum sidecar",
        maximum=1024,
        exact_mode=0o444,
    )
    if sidecar_raw != f"{expected}  {path.name}\n".encode("ascii"):
        fail("freeze-plan checksum sidecar differs")
    module_path = Path(__file__).resolve().parents[1] / "recovery" / "recovery_freeze.py"
    spec = importlib.util.spec_from_file_location("arc_recovery_freeze_for_vault", module_path)
    if spec is None or spec.loader is None:
        fail("audited freeze-plan validator cannot be loaded")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    try:
        spec.loader.exec_module(module)
        plan = module.validate_pinned_freeze_plan(
            raw,
            expected,
            expected_node_names=LOWER_NODE_ORDER,
            expected_sentinels=("nyc", "lax"),
        )
    except Exception:
        fail("sealed freeze plan failed the complete audited v5 validator")
    return plan, raw, sidecar_raw


def capture_id_for_freeze_hash(digest: str) -> str:
    return hashlib.sha256(b"ARC recovery capture v2\0" + bytes.fromhex(digest)).hexdigest()


def require_uint(value: object, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        fail(f"{label} is not an unsigned integer")
    return value


def require_utc_seconds(value: object, label: str) -> datetime.datetime:
    if not isinstance(value, str) or UTC_SECONDS_RE.fullmatch(value) is None:
        fail(f"{label} is not canonical UTC seconds")
    try:
        return datetime.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=datetime.timezone.utc
        )
    except ValueError:
        fail(f"{label} is not a real UTC timestamp")


def load_private_sealed_json(
    path: Path,
    sidecar: Path,
    expected_sha256: str,
    *,
    label: str,
    maximum: int,
) -> tuple[dict[str, Any], str]:
    expected = require_hash(expected_sha256, f"expected {label} digest")
    raw = read_regular_nofollow(
        path,
        label=label,
        maximum=maximum,
        exact_mode=0o400,
        require_single_link=True,
    )
    if hashlib.sha256(raw).hexdigest() != expected:
        fail(f"{label} bytes differ from the expected digest")
    sidecar_raw = read_regular_nofollow(
        sidecar,
        label=f"{label} checksum sidecar",
        maximum=1024,
        exact_mode=0o400,
        require_single_link=True,
    )
    if sidecar_raw != f"{expected}  {path.name}\n".encode("ascii"):
        fail(f"{label} checksum sidecar differs")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError):
        fail(f"{label} is not valid JSON")
    if not isinstance(value, dict) or raw != canonical_json_bytes(value):
        fail(f"{label} is not canonical JSON")
    return value, expected


def load_legacy_maintenance_evidence_bundle(
    path: Path,
    sidecar: Path,
    expected_sha256: str,
    *,
    source_commit: str,
    freeze_sha256: str,
    freeze_plan: Any,
) -> tuple[dict[str, Any], dict[str, dict[str, Any]], str]:
    value, expected = load_private_sealed_json(
        path,
        sidecar,
        expected_sha256,
        label="legacy maintenance evidence bundle",
        maximum=32 * 1024 * 1024,
    )
    bundle = exact_object(
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
    capture_id = capture_id_for_freeze_hash(freeze_sha256)
    expected_identity = {
        "schema": MAINTENANCE_EVIDENCE_BUNDLE_SCHEMA,
        "source_main_commit": source_commit,
        "freeze_plan_sha256": freeze_sha256,
        "capture_id": capture_id,
    }
    if any(bundle.get(field) != item for field, item in expected_identity.items()):
        fail("legacy maintenance evidence bundle source/freeze identity differs")
    first = require_utc_seconds(
        bundle.get("first_quarantine_started_at"), "maintenance first quarantine"
    )
    stopped = require_utc_seconds(
        bundle.get("all_controlled_stopped_at"), "maintenance all-controlled-stopped"
    )
    if first > stopped:
        fail("legacy maintenance evidence timestamps are reversed")
    challenge = require_hash(bundle.get("challenge"), "maintenance quarantine challenge")
    inventory_expected: list[dict[str, Any]] = []

    def sealed(raw: object, node: str, role: str, label: str) -> tuple[dict[str, Any], str]:
        wrapper = exact_object(raw, {"value", "sha256"}, label)
        object_value = wrapper.get("value")
        if not isinstance(object_value, dict):
            fail(f"{label} value is not an object")
        object_sha = require_hash(wrapper.get("sha256"), f"{label} digest")
        payload = canonical_json_bytes(object_value)
        if hashlib.sha256(payload).hexdigest() != object_sha:
            fail(f"{label} digest is not reproducible")
        inventory_expected.append(
            {"node": node, "role": role, "sha256": object_sha, "size": len(payload)}
        )
        return object_value, object_sha

    authenticated, _authenticated_sha = sealed(
        bundle.get("authenticated_prefence_height_cross_proof"),
        "fleet",
        "authenticated-prefence-height-cross-proof",
        "maintenance authenticated pre-fence proof",
    )
    authenticated = exact_object(
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
        authenticated.get("schema") != AUTHENTICATED_HEIGHT_FLEET_SCHEMA
        or authenticated.get("source_main_commit") != source_commit
        or authenticated.get("freeze_plan_sha256") != freeze_sha256
        or authenticated.get("capture_id") != capture_id
        or authenticated.get("challenge") != challenge
    ):
        fail("maintenance authenticated pre-fence fleet proof identity differs")
    require_hash(
        authenticated.get("legacy_public_height_receipt_sha256"),
        "maintenance public-height receipt root",
    )
    require_uint(
        authenticated.get("conservative_height_floor"),
        "maintenance conservative height floor",
    )
    if require_utc_seconds(authenticated.get("started_at"), "authenticated proof start") > require_utc_seconds(
        authenticated.get("completed_at"), "authenticated proof completion"
    ):
        fail("maintenance authenticated proof timestamps are reversed")
    authenticated_rows = authenticated.get("nodes")
    expected_topology = [(name.lower(), host) for name, _address, host in PRODUCTION_FLEET]
    if (
        not isinstance(authenticated_rows, list)
        or [(row.get("node"), row.get("host")) for row in authenticated_rows if isinstance(row, dict)]
        != expected_topology
    ):
        fail("maintenance authenticated proof topology differs")
    for index, row in enumerate(authenticated_rows):
        item = exact_object(
            row,
            {"node", "host", "proof", "proof_sha256"},
            f"maintenance authenticated node {index}",
        )
        if not isinstance(item["proof"], dict):
            fail("maintenance authenticated node proof is not an object")
        proof_sha = require_hash(item["proof_sha256"], "maintenance authenticated node proof root")
        if hashlib.sha256(canonical_json_bytes(item["proof"])).hexdigest() != proof_sha:
            fail("maintenance authenticated node proof root is not reproducible")

    challenge_receipt, _challenge_sha = sealed(
        bundle.get("network_quarantine_challenge"),
        "fleet",
        "network-quarantine-challenge",
        "maintenance quarantine challenge receipt",
    )
    if challenge_receipt != {
        "schema": "arc.recovery.legacy-network-quarantine-challenge.v1",
        "freeze_plan_sha256": freeze_sha256,
        "capture_id": capture_id,
        "challenge": challenge,
    }:
        fail("maintenance quarantine challenge receipt differs")

    stability, _stability_sha = sealed(
        bundle.get("quarantine_stability_proof"),
        "fleet",
        "network-quarantine-stability-proof",
        "maintenance quarantine stability proof",
    )
    stability = exact_object(
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
        "maintenance quarantine stability proof",
    )
    if (
        stability.get("schema") != "arc.recovery.legacy-network-quarantine-stability.v1"
        or stability.get("source_main_commit") != source_commit
        or stability.get("freeze_plan_sha256") != freeze_sha256
        or stability.get("capture_id") != capture_id
        or stability.get("challenge") != challenge
        or stability.get("interval_seconds") != 120
        or stability.get("sample_count") != 2
        or stability.get("global_absence_claimed") is not False
        or require_uint(stability.get("monotonic_elapsed_ns"), "maintenance stability elapsed")
        < 120_000_000_000
    ):
        fail("maintenance quarantine stability proof differs")
    stability_started = require_utc_seconds(stability.get("started_at"), "stability start")
    stability_completed = require_utc_seconds(
        stability.get("completed_at"), "stability completion"
    )
    if not first <= stability_started <= stability_completed <= stopped:
        fail("maintenance quarantine stability timestamps are outside the quarantine/stop window")
    stability_rows = stability.get("nodes")
    stability_heads = stability.get("fleet_heads")
    if (
        not isinstance(stability_rows, list)
        or not isinstance(stability_heads, list)
        or [(row.get("node"), row.get("host")) for row in stability_rows if isinstance(row, dict)]
        != expected_topology
        or [(row.get("node"), row.get("host")) for row in stability_heads if isinstance(row, dict)]
        != expected_topology
    ):
        fail("maintenance quarantine stability topology differs")

    rows = bundle.get("nodes")
    if not isinstance(rows, list) or len(rows) != len(PRODUCTION_FLEET):
        fail("legacy maintenance evidence bundle must contain six ordered nodes")
    node_fields = {
        "node",
        "host",
        "stopped_status",
        "quarantine_status",
        "post_proof_quarantine_status",
        "external_quarantine_proof",
        "public_cross_proof",
        "persisted_head",
    }
    role_fields = (
        ("stopped_status", "stopped-status", "arc.recovery.offline-stop-status.v1"),
        ("quarantine_status", "quarantine-status", "arc.recovery.legacy-network-quarantine-status.v1"),
        ("post_proof_quarantine_status", "post-proof-quarantine-status", "arc.recovery.legacy-network-quarantine-status.v1"),
        ("external_quarantine_proof", "external-quarantine-proof", "arc.recovery.legacy-network-quarantine-external-proof.v1"),
        ("public_cross_proof", "public-cross-proof", "arc.recovery.legacy-network-quarantine-public-cross-proof.v1"),
        ("persisted_head", "persisted-head", "arc.recovery.persisted-legacy-head.v1"),
    )
    by_node: dict[str, dict[str, Any]] = {}
    for (upper_node, _address, host), raw_row in zip(PRODUCTION_FLEET, rows):
        node = upper_node.lower()
        item = exact_object(raw_row, node_fields, "maintenance evidence node")
        if (item.get("node"), item.get("host")) != (node, host):
            fail("legacy maintenance evidence bundle topology differs")
        normalized: dict[str, Any] = {"node": node, "host": host}
        for field, role, schema in role_fields:
            object_value, object_sha = sealed(
                item.get(field), node, role, f"maintenance {node} {role}"
            )
            if (
                object_value.get("schema") != schema
                or object_value.get("capture_id") != capture_id
                or object_value.get("node") != node
                or object_value.get("freeze_plan_sha256") != freeze_sha256
            ):
                fail(f"maintenance {node} {role} identity differs")
            normalized[field] = {"value": object_value, "sha256": object_sha}
        if (
            normalized["stopped_status"]["value"].get("stopped") is not True
            or normalized["stopped_status"]["value"].get("restart_fenced") is not True
            or normalized["quarantine_status"]["value"].get("active") is not True
            or normalized["quarantine_status"]["value"].get("enabled") is not True
            or normalized["post_proof_quarantine_status"]["value"].get("active") is not True
            or normalized["post_proof_quarantine_status"]["value"].get("enabled") is not True
            or normalized["external_quarantine_proof"]["value"].get("host") != host
            or normalized["external_quarantine_proof"]["value"].get("challenge") != challenge
            or normalized["external_quarantine_proof"]["value"].get("global_absence_claimed") is not False
            or normalized["public_cross_proof"]["value"].get("challenge") != challenge
            or normalized["public_cross_proof"]["value"].get("global_absence_claimed") is not False
            or normalized["persisted_head"]["value"].get("source_main_commit") != source_commit
            or normalized["persisted_head"]["value"].get("writer_stopped") is not True
            or normalized["persisted_head"]["value"].get("restart_barrier_active") is not True
            or normalized["persisted_head"]["value"].get("network_quarantine_active") is not True
            or normalized["persisted_head"]["value"].get("global_absence_claimed") is not False
        ):
            fail(f"maintenance {node} stopped/persisted state differs")
        by_node[upper_node] = normalized

    if bundle.get("object_inventory") != inventory_expected:
        fail("legacy maintenance evidence bundle inventory differs from its sealed objects")
    inventory_root = hashlib.sha256(
        canonical_json_bytes(
            {
                "schema": "arc.recovery.legacy-maintenance-evidence-inventory.v1",
                "objects": inventory_expected,
            }
        )
    ).hexdigest()
    if bundle.get("aggregate_root_sha256") != inventory_root:
        fail("legacy maintenance evidence aggregate root is not reproducible")
    return bundle, by_node, expected


def load_legacy_maintenance_boundary(
    path: Path,
    sidecar: Path,
    expected_sha256: str,
    *,
    source_commit: str,
    freeze_sha256: str,
    freeze_plan: Any,
    evidence_bundle: dict[str, Any],
    evidence_bundle_sha256: str,
) -> tuple[dict[str, Any], str]:
    value, expected = load_private_sealed_json(
        path,
        sidecar,
        expected_sha256,
        label="legacy maintenance boundary",
        maximum=16 * 1024 * 1024,
    )
    boundary = exact_object(
        value,
        {
            "schema", "source_main_commit", "freeze_plan_sha256", "capture_id",
            "first_quarantine_started_at", "all_controlled_stopped_at", "created_at",
            "official_origin_scope", "legacy_public_height_receipt",
            "authenticated_prefence_height_cross_proof_sha256",
            "legacy_maintenance_evidence_bundle_sha256",
            "network_quarantine_stability_proof_sha256", "network_quarantine_challenge",
            "tools", "nodes", "evidence_heights", "observed_cutoff_height",
            "continuity_safety_margin", "continuity_safety_margin_policy",
            "legacy_public_max_height", "global_absence_claimed", "reopening_policy",
            "late_fork_circuit", "threat_model",
        },
        "legacy maintenance boundary",
    )
    capture_id = capture_id_for_freeze_hash(freeze_sha256)
    if (
        boundary.get("schema") != MAINTENANCE_BOUNDARY_SCHEMA
        or boundary.get("source_main_commit") != source_commit
        or boundary.get("freeze_plan_sha256") != freeze_sha256
        or boundary.get("capture_id") != capture_id
        or boundary.get("global_absence_claimed") is not False
    ):
        fail("legacy maintenance boundary source/freeze identity differs")
    first = require_utc_seconds(boundary.get("first_quarantine_started_at"), "boundary first quarantine")
    stopped = require_utc_seconds(boundary.get("all_controlled_stopped_at"), "boundary all-controlled-stopped")
    created = require_utc_seconds(boundary.get("created_at"), "boundary creation")
    if not first <= stopped <= created:
        fail("legacy maintenance boundary timestamps are not ordered")
    if (
        boundary.get("first_quarantine_started_at") != evidence_bundle.get("first_quarantine_started_at")
        or boundary.get("all_controlled_stopped_at") != evidence_bundle.get("all_controlled_stopped_at")
    ):
        fail("legacy maintenance boundary timestamps differ from the evidence bundle")
    origins = [
        {"node": node.lower(), "host": host, "origin": f"http://{host}:9090"}
        for node, _address, host in PRODUCTION_FLEET
    ]
    if boundary.get("official_origin_scope") != {
        "global_absence_claimed": False,
        "origins": origins,
    }:
        fail("legacy maintenance boundary official origins differ")
    public_root = exact_object(
        boundary.get("legacy_public_height_receipt"),
        {"schema", "sha256", "completed_at", "observed_max_height"},
        "legacy maintenance public-height root",
    )
    if not isinstance(public_root.get("schema"), str) or not public_root["schema"]:
        fail("legacy maintenance public-height schema is invalid")
    require_hash(public_root.get("sha256"), "legacy maintenance public-height root")
    require_utc_seconds(public_root.get("completed_at"), "legacy public-height completion")
    require_uint(public_root.get("observed_max_height"), "legacy public observed maximum")
    authenticated_root = evidence_bundle["authenticated_prefence_height_cross_proof"]["sha256"]
    stability_root = evidence_bundle["quarantine_stability_proof"]["sha256"]
    if (
        boundary.get("legacy_maintenance_evidence_bundle_sha256") != evidence_bundle_sha256
        or boundary.get("authenticated_prefence_height_cross_proof_sha256") != authenticated_root
        or boundary.get("network_quarantine_stability_proof_sha256") != stability_root
        or boundary.get("network_quarantine_challenge") != evidence_bundle.get("challenge")
    ):
        fail("legacy maintenance boundary evidence roots differ from the evidence bundle")
    tools = exact_object(
        boundary.get("tools"),
        {
            "remote_helper_sha256", "inspector_binary_sha256", "genesis_sha256",
            "validator_public_keys_sha256", "legacy_validator_set_sha256",
            "orchestrator_sha256", "rollout_tool_sha256", "rollout_schema_sha256",
        },
        "legacy maintenance tool roots",
    )
    plan_value = freeze_plan.value()
    for field in tools:
        require_hash(tools[field], f"legacy maintenance {field}")
    for field in ("remote_helper_sha256", "orchestrator_sha256", "rollout_tool_sha256", "rollout_schema_sha256"):
        if tools[field] != plan_value.get(field):
            fail(f"legacy maintenance {field} differs from the freeze plan")
    if (
        boundary.get("continuity_safety_margin") != 128
        or boundary.get("continuity_safety_margin_policy") != {
            "prune_depth": 100,
            "commit_rule_rounds": 2,
            "operational_headroom": 26,
            "cryptographic_global_absence_proof": False,
        }
        or boundary.get("reopening_policy") != {
            "required_validator_count": 6,
            "height_relation": "strictly-greater-than-legacy_public_max_height",
            "required_equal_fields": ["block_hash", "state_root"],
        }
        or boundary.get("late_fork_circuit") != {
            "monitor_scope": "retired-and-community-legacy-sources",
            "trigger": "self-consistent-legacy-fork-candidate-above-observed-cutoff-height",
            "action": "enter-maintenance-preserve-and-offline-validate",
            "rewrite_v3_history_allowed": False,
        }
        or boundary.get("threat_model") != {
            "trusted_host_root_required": True,
            "sealed_reviewed_legacy_binary_non_adversarial": True,
            "quarantine_purpose": "operational-network-isolation",
            "hostile_root_containment_claimed": False,
        }
    ):
        fail("legacy maintenance boundary safety/reopening policy differs")
    rows = boundary.get("nodes")
    bundle_rows = evidence_bundle.get("nodes")
    auth_rows = evidence_bundle["authenticated_prefence_height_cross_proof"]["value"]["nodes"]
    if not isinstance(rows, list) or len(rows) != len(PRODUCTION_FLEET):
        fail("legacy maintenance boundary must contain six ordered nodes")
    node_fields = {
        "node", "host", "origin", "public_observation",
        "authenticated_prefence_proof_sha256", "network_quarantine_receipt_sha256",
        "quarantine_status_sha256", "post_proof_quarantine_status_sha256",
        "external_quarantine_proof_sha256", "public_cross_proof_sha256",
        "initial_post_quarantine_head", "post_quarantine_head", "final_persisted_head",
    }

    def observation(raw: object, label: str) -> dict[str, Any]:
        wrapper = exact_object(raw, {"tuple", "evidence_sha256"}, label)
        head = exact_object(wrapper.get("tuple"), {"height", "block_hash", "state_root"}, f"{label} tuple")
        require_uint(head.get("height"), f"{label} height")
        require_hash(head.get("block_hash"), f"{label} block hash")
        require_hash(head.get("state_root"), f"{label} state root")
        require_hash(wrapper.get("evidence_sha256"), f"{label} evidence root")
        return wrapper

    for index, (origin, row, bundle_row, auth_row) in enumerate(zip(origins, rows, bundle_rows, auth_rows)):
        item = exact_object(row, node_fields, f"legacy maintenance boundary node {index}")
        if (item.get("node"), item.get("host"), item.get("origin")) != (
            origin["node"], origin["host"], origin["origin"]
        ):
            fail("legacy maintenance boundary topology differs")
        for field in (
            "authenticated_prefence_proof_sha256", "network_quarantine_receipt_sha256",
            "quarantine_status_sha256", "post_proof_quarantine_status_sha256",
            "external_quarantine_proof_sha256", "public_cross_proof_sha256",
        ):
            require_hash(item.get(field), f"legacy maintenance {origin['node']} {field}")
        for field in (
            "public_observation", "initial_post_quarantine_head", "post_quarantine_head", "final_persisted_head"
        ):
            observation(item.get(field), f"legacy maintenance {origin['node']} {field}")
        expected_roots = {
            "authenticated_prefence_proof_sha256": auth_row["proof_sha256"],
            "quarantine_status_sha256": bundle_row["quarantine_status"]["sha256"],
            "post_proof_quarantine_status_sha256": bundle_row["post_proof_quarantine_status"]["sha256"],
            "external_quarantine_proof_sha256": bundle_row["external_quarantine_proof"]["sha256"],
            "public_cross_proof_sha256": bundle_row["public_cross_proof"]["sha256"],
        }
        if any(item.get(field) != root for field, root in expected_roots.items()):
            fail(f"legacy maintenance {origin['node']} boundary roots differ from the evidence bundle")
        persisted_root = bundle_row["persisted_head"]["sha256"]
        if item["final_persisted_head"]["evidence_sha256"] != persisted_root:
            fail(f"legacy maintenance {origin['node']} persisted root differs from the evidence bundle")
    heights = boundary.get("evidence_heights")
    if not isinstance(heights, list) or not heights:
        fail("legacy maintenance boundary has no enumerated evidence heights")
    seen: set[tuple[str, str]] = set()
    normalized_heights: list[int] = []
    expected_nodes = {node.lower() for node, _address, _host in PRODUCTION_FLEET}
    for raw_height in heights:
        item = exact_object(raw_height, {"node", "label", "height", "evidence_sha256"}, "legacy maintenance evidence height")
        if item.get("node") not in expected_nodes or not isinstance(item.get("label"), str) or not item["label"]:
            fail("legacy maintenance evidence height identity differs")
        identity = (item["node"], item["label"])
        if identity in seen:
            fail("legacy maintenance evidence height is duplicated")
        seen.add(identity)
        normalized_heights.append(require_uint(item.get("height"), "legacy maintenance evidence height"))
        require_hash(item.get("evidence_sha256"), "legacy maintenance evidence height root")
    cutoff = require_uint(boundary.get("observed_cutoff_height"), "legacy maintenance observed cutoff")
    legacy_max = require_uint(boundary.get("legacy_public_max_height"), "legacy maintenance public maximum")
    if cutoff != max(normalized_heights) or cutoff < public_root["observed_max_height"] or legacy_max != cutoff + 128:
        fail("legacy maintenance boundary cutoff/continuity ceiling differs")
    return boundary, expected


def load_offline_stop_evidence(
    path: Path,
    sidecar: Path,
    expected_sha256: str,
    *,
    source_commit: str,
    freeze_sha256: str,
    freeze_sidecar_sha256: str,
    freeze_plan: Any,
    evidence_bundle: dict[str, Any],
    evidence_bundle_nodes: dict[str, dict[str, Any]],
    evidence_bundle_sha256: str,
    maintenance_boundary: dict[str, Any],
    maintenance_boundary_sha256: str,
) -> tuple[dict[str, Any], dict[str, dict[str, Any]], str]:
    value, expected = load_private_sealed_json(
        path,
        sidecar,
        expected_sha256,
        label="offline-stop evidence",
        maximum=16 * 1024 * 1024,
    )
    evidence = exact_object(
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
        "offline-stop evidence",
    )
    plan_value = freeze_plan.value()
    helper_sha = require_hash(plan_value.get("remote_helper_sha256"), "freeze-plan remote helper")
    helper_path = f"/root/.arc-recovery-helpers/{helper_sha}/archive-node.sh"
    expected_top = {
        "schema": OFFLINE_STOP_EVIDENCE_SCHEMA,
        "source_main_commit": source_commit,
        "freeze_plan_sha256": freeze_sha256,
        "freeze_plan_sidecar_sha256": freeze_sidecar_sha256,
        "capture_id": capture_id_for_freeze_hash(freeze_sha256),
        "remote_helper_sha256": helper_sha,
        "remote_helper_path": helper_path,
    }
    if any(evidence.get(field) != expected_value for field, expected_value in expected_top.items()):
        fail("offline-stop evidence source/freeze/helper binding differs")
    if (
        evidence.get("first_quarantine_started_at") != maintenance_boundary["first_quarantine_started_at"]
        or evidence.get("all_controlled_stopped_at") != maintenance_boundary["all_controlled_stopped_at"]
        or evidence.get("legacy_maintenance_boundary_sha256") != maintenance_boundary_sha256
        or evidence.get("legacy_maintenance_boundary") != maintenance_boundary
        or evidence.get("legacy_maintenance_evidence_bundle_sha256") != evidence_bundle_sha256
        or evidence.get("legacy_maintenance_evidence_bundle_sha256")
        != maintenance_boundary["legacy_maintenance_evidence_bundle_sha256"]
    ):
        fail("offline-stop evidence maintenance bundle/boundary binding differs")
    cross = exact_object(
        evidence.get("legacy_height_cross_proof"),
        {
            "schema", "source_main_commit", "freeze_plan_sha256", "capture_id",
            "legacy_public_height_receipt_sha256", "challenge", "started_at",
            "completed_at", "conservative_height_floor", "nodes",
        },
        "offline-stop authenticated legacy-height fleet proof",
    )
    if (
        cross != evidence_bundle["authenticated_prefence_height_cross_proof"]["value"]
        or hashlib.sha256(canonical_json_bytes(cross)).hexdigest()
        != maintenance_boundary["authenticated_prefence_height_cross_proof_sha256"]
        or cross.get("legacy_public_height_receipt_sha256")
        != maintenance_boundary["legacy_public_height_receipt"]["sha256"]
        or cross.get("challenge") != maintenance_boundary["network_quarantine_challenge"]
    ):
        fail("offline-stop authenticated legacy-height root differs")
    rows = evidence["nodes"]
    if not isinstance(rows, list) or len(rows) != len(PRODUCTION_FLEET):
        fail("offline-stop evidence must contain the ordered six validators")
    plan_rows = plan_value["nodes"]
    by_node: dict[str, dict[str, Any]] = {}
    completion_roots: set[str] = set()
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
    for (upper_node, _new_address, host), plan_node, row in zip(PRODUCTION_FLEET, plan_rows, rows):
        expected_node = upper_node.lower()
        legacy_address = require_hash(
            plan_node.get("validator_address"), f"{expected_node} legacy validator address"
        )
        item = exact_object(
            row,
            node_fields,
            "offline-stop node evidence",
        )
        if (
            plan_node.get("name") != expected_node
            or plan_node.get("host") != host
            or item.get("node") != expected_node
            or item.get("host") != host
            or item.get("validator_address") != legacy_address
            or item.get("stake") != plan_node.get("stake")
        ):
            fail("offline-stop evidence differs from the fixed production host/key topology")
        complete_sha = require_hash(item["stop_complete_sha256"], f"{expected_node} stop.complete")
        files_sha = require_hash(item["stop_files_sha256"], f"{expected_node} stop.files")
        status_sha = require_hash(item["stopped_status_sha256"], f"{expected_node} stopped-status")
        argv_sha = require_hash(
            item["stopped_status_argv_sha256"], f"{expected_node} stopped-status argv"
        )
        argv = [
            "stopped-status",
            expected_top["capture_id"],
            expected_node,
            freeze_sha256,
            *(str(plan_node[field]) for field in STOPPED_STATUS_ARGV_FIELDS),
        ]
        status = {
            "capture_id": expected_top["capture_id"],
            "freeze_plan_sha256": freeze_sha256,
            "node": expected_node,
            "restart_fenced": True,
            "schema": "arc.recovery.offline-stop-status.v1",
            "stake": plan_node["stake"],
            "stop_complete_sha256": complete_sha,
            "stop_files_sha256": files_sha,
            "stop_schema": "arc.recovery.offline-stop.v4",
            "stopped": True,
            "validator_address": legacy_address,
        }
        if hashlib.sha256(canonical_json_bytes(argv)).hexdigest() != argv_sha:
            fail(f"offline-stop evidence {expected_node} argv hash is not reproducible")
        if hashlib.sha256(canonical_json_bytes(status)).hexdigest() != status_sha:
            fail(f"offline-stop evidence {expected_node} status hash is not reproducible")
        bundle_status = evidence_bundle_nodes[upper_node]["stopped_status"]
        if bundle_status["sha256"] != status_sha or bundle_status["value"] != status:
            fail(f"offline-stop evidence {expected_node} status differs from the evidence bundle")
        if complete_sha in completion_roots:
            fail("offline-stop evidence repeats a validator completion root")
        completion_roots.add(complete_sha)
        by_node[upper_node] = {"argv": argv, "status": status, "row": item}
    return evidence, by_node, expected


def validate_fresh_stopped_status(
    output: str,
    expected: dict[str, Any],
    expected_sha256: str,
    node: str,
) -> None:
    try:
        raw = output.encode("ascii")
    except UnicodeEncodeError:
        fail(f"fresh {node} stopped-status is not ASCII")
    try:
        value = json.loads(raw)
    except json.JSONDecodeError:
        fail(f"fresh {node} stopped-status is not valid JSON")
    if raw != canonical_json_bytes(value) or value != expected:
        fail(f"fresh {node} stopped-status differs from the authenticated offline root")
    if hashlib.sha256(raw).hexdigest() != expected_sha256:
        fail(f"fresh {node} stopped-status hash differs from the authenticated evidence")


def exact_control_line(output: str, label: str) -> str:
    if "\r" in output or not output.endswith("\n") or "\n" in output[:-1]:
        fail(f"{label} did not return exactly one newline-terminated control line")
    return output[:-1]


REMOTE_SCRIPT = r'''set -eu
PATH=/usr/bin:/bin
export PATH
unset BASH_ENV ENV CDPATH GLOBIGNORE
op=$1
case "$op" in
  stopped-status)
    helper=$2 expected_helper=$3
    shift 3
    case "$helper" in /root/.arc-recovery-helpers/"$expected_helper"/archive-node.sh) ;; *) exit 1 ;; esac
    test -f "$helper" && test ! -L "$helper"
    exec 9<"$helper"
    test -f /proc/self/fd/9
    actual_helper=$(sha256sum /proc/self/fd/9 | cut -d' ' -f1)
    test "$actual_helper" = "$expected_helper"
    test "$1" = stopped-status
    /proc/self/fd/9 "$@"
    ;;
  probe)
    destination=$2 expected=$3
    if test -e "$destination" || test -L "$destination"; then
      test -d /etc && test ! -L /etc
      test "$(stat -c %U:%G /etc)" = root:root
      test -d /etc/arc-v3 && test ! -L /etc/arc-v3
      test "$(stat -c %U:%G:%a /etc/arc-v3)" = root:root:700
      test -f "$destination" && test ! -L "$destination"
      test "$(stat -c %U:%G:%a "$destination")" = root:root:600
      test "$(stat -c %h "$destination")" = 1
      printf '%s  %s\n' "$expected" "$destination" | sha256sum --check --strict >/dev/null
      printf 'VERIFIED\n'
    else
      printf 'MISSING\n'
    fi
    ;;
  prepare)
    directory=$2 digest=$3
    test "$directory" = /etc/arc-v3
    test -d /etc && test ! -L /etc
    if test ! -e "$directory" && test ! -L "$directory"; then
      mkdir -m 0700 -- "$directory"
      chown root:root -- "$directory"
      python3 - <<'PY'
import os
fd=os.open('/etc',os.O_RDONLY|getattr(os,'O_DIRECTORY',0)|getattr(os,'O_NOFOLLOW',0))
try: os.fsync(fd)
finally: os.close(fd)
PY
    fi
    test -d "$directory" && test ! -L "$directory"
    test "$(stat -c %U:%G:%a "$directory")" = root:root:700
    mktemp "$directory/.validator-key.upload.${digest}.XXXXXXXX"
    ;;
  commit)
    temporary=$2 destination=$3 expected=$4
    case "$temporary" in /etc/arc-v3/.validator-key.upload."$expected".*) ;; *) exit 1 ;; esac
    test "$destination" = /etc/arc-v3/validator-key.json
    test -f "$temporary" && test ! -L "$temporary"
    test "$(stat -c %h "$temporary")" = 1
    chown root:root -- "$temporary"
    chmod 0600 -- "$temporary"
    printf '%s  %s\n' "$expected" "$temporary" | sha256sum --check --strict >/dev/null
    python3 - "$temporary" <<'PY'
import os,sys
fd=os.open(sys.argv[1],os.O_RDONLY|getattr(os,'O_NOFOLLOW',0))
try: os.fsync(fd)
finally: os.close(fd)
PY
    if ! ln -- "$temporary" "$destination"; then
      test -f "$destination" && test ! -L "$destination"
      test "$(stat -c %U:%G:%a "$destination")" = root:root:600
      printf '%s  %s\n' "$expected" "$destination" | sha256sum --check --strict >/dev/null
    fi
    rm -f -- "$temporary"
    python3 - "$destination" <<'PY'
import os,sys
path=sys.argv[1]
fd=os.open(path,os.O_RDONLY|getattr(os,'O_NOFOLLOW',0))
try: os.fsync(fd)
finally: os.close(fd)
fd=os.open(os.path.dirname(path),os.O_RDONLY|getattr(os,'O_DIRECTORY',0)|getattr(os,'O_NOFOLLOW',0))
try: os.fsync(fd)
finally: os.close(fd)
PY
    test "$(stat -c %U:%G:%a "$destination")" = root:root:600
    test "$(stat -c %h "$destination")" = 1
    printf '%s  %s\n' "$expected" "$destination" | sha256sum --check --strict >/dev/null
    printf 'VERIFIED\n'
    ;;
  cleanup)
    temporary=$2 expected=$3
    case "$temporary" in /etc/arc-v3/.validator-key.upload."$expected".*) ;; *) exit 1 ;; esac
    test ! -L "$temporary"
    rm -f -- "$temporary"
    ;;
  *) exit 1 ;;
esac
'''


def validate_exact_known_hosts(raw: bytes) -> None:
    """Require the reviewed six-IP OpenSSH Ed25519 trust-anchor grammar."""

    try:
        text = raw.decode("ascii")
    except UnicodeDecodeError:
        fail("known-hosts anchor is not ASCII")
    if not text.endswith("\n") or "\r" in text or "\x00" in text:
        fail("known-hosts anchor must be LF-terminated canonical text")
    lines = text[:-1].split("\n")
    expected_hosts = [host for _node, _address, host in PRODUCTION_FLEET]
    if len(lines) != len(expected_hosts):
        fail("known-hosts anchor must contain exactly six records")
    seen_keys: set[bytes] = set()
    canonical_lines: list[str] = []
    for index, (line, expected_host) in enumerate(zip(lines, expected_hosts), start=1):
        fields = line.split(" ")
        if len(fields) != 3 or any(not field for field in fields):
            fail(f"known-hosts record {index} is not one canonical three-field record")
        host, algorithm, encoded = fields
        if host != expected_host:
            fail(f"known-hosts record {index} is not the fixed production IP")
        if algorithm != "ssh-ed25519":
            fail(f"known-hosts record {index} is not ssh-ed25519")
        try:
            blob = base64.b64decode(encoded, validate=True)
        except (binascii.Error, ValueError):
            fail(f"known-hosts record {index} has invalid canonical base64")
        expected_prefix = b"\x00\x00\x00\x0bssh-ed25519\x00\x00\x00\x20"
        if (
            len(blob) != len(expected_prefix) + 32
            or not blob.startswith(expected_prefix)
            or base64.b64encode(blob).decode("ascii") != encoded
        ):
            fail(f"known-hosts record {index} is not one canonical Ed25519 key blob")
        if blob in seen_keys:
            fail("known-hosts anchor reuses an Ed25519 host key")
        seen_keys.add(blob)
        canonical_lines.append(f"{host} {algorithm} {encoded}")
    if raw != ("\n".join(canonical_lines) + "\n").encode("ascii"):
        fail("known-hosts anchor is not byte-canonical")


def strict_transport_options(known_hosts: Path, identity: Path) -> list[str]:
    return [
        "-F",
        "/dev/null",
        "-i",
        str(identity),
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        f"UserKnownHostsFile={known_hosts}",
        "-o",
        "GlobalKnownHostsFile=/dev/null",
        "-o",
        "HostKeyAlgorithms=ssh-ed25519",
        "-o",
        "PubkeyAcceptedAlgorithms=ssh-ed25519",
        "-o",
        "UpdateHostKeys=no",
        "-o",
        "PasswordAuthentication=no",
        "-o",
        "KbdInteractiveAuthentication=no",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "IdentityAgent=none",
        "-o",
        "PreferredAuthentications=publickey",
        "-o",
        "PubkeyAuthentication=yes",
        "-o",
        "LogLevel=ERROR",
    ]


def run_ssh(
    ssh: Path,
    ssh_sha256: str,
    tool_directory: Path,
    known_hosts: Path,
    identity: Path,
    identity_sha256: str,
    host: str,
    remote_args: Sequence[str],
    *,
    timeout: int = 120,
) -> str:
    if SAFE_HOST_RE.fullmatch(host) is None or any(re.fullmatch(r"[A-Za-z0-9_./:-]+", arg) is None for arg in remote_args):
        fail("unsafe SSH host or remote argument")
    prove_pinned_file(
        ssh,
        ssh_sha256,
        label="pinned SSH executable",
        maximum=32 * 1024 * 1024,
        mode=0o500,
    )
    prove_pinned_file(
        identity,
        identity_sha256,
        label="pinned SSH identity",
        maximum=64 * 1024,
        mode=0o400,
    )
    command = [
        str(ssh),
        *strict_transport_options(known_hosts, identity),
        f"root@{host}",
        "/bin/sh",
        "-s",
        "--",
        *remote_args,
    ]
    try:
        result = subprocess.run(
            command,
            input=REMOTE_SCRIPT.encode(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=sanitized_env(
                remote=True,
                tool_directory=tool_directory,
            ),
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        fail("strict SSH operation could not complete")
    prove_pinned_file(
        ssh,
        ssh_sha256,
        label="pinned SSH executable",
        maximum=32 * 1024 * 1024,
        mode=0o500,
    )
    prove_pinned_file(
        identity,
        identity_sha256,
        label="pinned SSH identity",
        maximum=64 * 1024,
        mode=0o400,
    )
    if result.returncode != 0:
        fail("strict SSH operation failed closed")
    try:
        return result.stdout.decode("ascii")
    except UnicodeDecodeError:
        fail("strict SSH operation returned non-ASCII control output")


def run_scp(
    scp: Path,
    scp_sha256: str,
    ssh: Path,
    ssh_sha256: str,
    tool_directory: Path,
    known_hosts: Path,
    identity: Path,
    identity_sha256: str,
    host: str,
    source: Path,
    destination: str,
) -> None:
    if SAFE_HOST_RE.fullmatch(host) is None or re.fullmatch(r"/etc/arc-v3/[A-Za-z0-9._-]+", destination) is None:
        fail("unsafe SCP host or destination")
    prove_pinned_file(
        scp,
        scp_sha256,
        label="pinned SCP executable",
        maximum=32 * 1024 * 1024,
        mode=0o500,
    )
    prove_pinned_file(
        ssh,
        ssh_sha256,
        label="pinned SSH executable selected by SCP",
        maximum=32 * 1024 * 1024,
        mode=0o500,
    )
    prove_pinned_file(
        identity,
        identity_sha256,
        label="pinned SSH identity",
        maximum=64 * 1024,
        mode=0o400,
    )
    command = [
        str(scp),
        "-q",
        "-S",
        str(ssh),
        *strict_transport_options(known_hosts, identity),
        "--",
        str(source),
        f"root@{host}:{destination}",
    ]
    try:
        result = subprocess.run(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=sanitized_env(
                remote=True,
                tool_directory=tool_directory,
            ),
            timeout=600,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        fail("strict SCP upload could not complete")
    prove_pinned_file(
        scp,
        scp_sha256,
        label="pinned SCP executable",
        maximum=32 * 1024 * 1024,
        mode=0o500,
    )
    prove_pinned_file(
        identity,
        identity_sha256,
        label="pinned SSH identity",
        maximum=64 * 1024,
        mode=0o400,
    )
    prove_pinned_file(
        ssh,
        ssh_sha256,
        label="pinned SSH executable selected by SCP",
        maximum=32 * 1024 * 1024,
        mode=0o500,
    )
    if result.returncode != 0:
        fail("strict SCP upload failed closed")


def pin_transport_runtime(
    root: Path,
    *,
    ssh: Path,
    ssh_sha256: str,
    scp: Path,
    scp_sha256: str,
) -> tuple[Path, Path, str, str]:
    pinned_ssh = root / "ssh"
    pinned_scp = root / "scp"
    ssh_digest = pin_reviewed_file(
        ssh,
        pinned_ssh,
        ssh_sha256,
        label="reviewed SSH executable",
        maximum=32 * 1024 * 1024,
        executable=True,
    )
    scp_digest = pin_reviewed_file(
        scp,
        pinned_scp,
        scp_sha256,
        label="reviewed SCP executable",
        maximum=32 * 1024 * 1024,
        executable=True,
    )
    sync_directory(root)
    return pinned_ssh, pinned_scp, ssh_digest, scp_digest


def publish_install_receipt(path: Path, value: dict[str, Any]) -> None:
    if not path.is_absolute():
        fail("install receipt output must be absolute")
    payload = canonical_json_bytes(value)
    if path.exists() or path.is_symlink():
        raw = read_regular_nofollow(
            path,
            label="existing install receipt",
            maximum=MAX_JSON_BYTES,
            exact_mode=0o444,
        )
        try:
            existing = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            fail("existing install receipt is not canonical JSON")
        if raw != canonical_json_bytes(existing):
            fail("existing install receipt is not canonical JSON")
        if not isinstance(existing, dict) or set(existing) != set(value):
            fail("existing install receipt schema differs; replacement is forbidden")
        source_commit = require_commit(value.get("source_commit"), "install source commit")
        validate_provenance_pair(
            existing.get("pretag_initial_provenance"),
            existing.get("pretag_final_provenance"),
            source_commit=source_commit,
        )
        validate_provenance_pair(
            value.get("pretag_initial_provenance"),
            value.get("pretag_final_provenance"),
            source_commit=source_commit,
        )
        validate_provenance_pair(
            existing.get("pretag_final_provenance"),
            value.get("pretag_initial_provenance"),
            source_commit=source_commit,
        )
        stable_existing = {
            key: item
            for key, item in existing.items()
            if key not in {"pretag_initial_provenance", "pretag_final_provenance"}
        }
        stable_current = {
            key: item
            for key, item in value.items()
            if key not in {"pretag_initial_provenance", "pretag_final_provenance"}
        }
        if stable_existing != stable_current:
            fail("existing install receipt differs; replacement is forbidden")
        return
    create_file(path, payload, 0o444)
    sync_directory(path.parent.resolve(strict=True))


def install(args: argparse.Namespace) -> None:
    try:
        with artifact_provenance.pretag_actions_proof(
            raw_actions_zip=args.raw_actions_zip,
            expected_commit=args.source_main_sha,
            expected_run_id=args.pretag_run_id,
            expected_run_attempt=args.pretag_run_attempt,
            expected_artifact_id=args.pretag_artifact_id,
            kind="headless",
            platform="linux-x86_64",
            expected_version=VERSION,
            curl=args.curl,
            curl_sha256=args.curl_sha256,
            ca_bundle=args.ca_bundle,
            ca_bundle_sha256=args.ca_bundle_sha256,
        ) as proof:
            _install_verified(args, proof)
    except artifact_provenance.ProvenanceError as error:
        fail(f"live protected preflight verification failed: {error}")


def _install_verified(args: argparse.Namespace, proof: Any) -> None:
    temporary_root = Path(tempfile.mkdtemp(prefix="arc-validator-vault-install."))
    temporary_root.chmod(0o700)
    pinned = temporary_root / "pinned"
    pinned.mkdir(mode=0o700)
    pinned_keys = pinned / "keys"
    pinned_keys.mkdir(mode=0o700)
    try:
        receipt_root = args.restore_receipt.parent.resolve(strict=True)
        receipt, keys = load_restore_receipt(args.restore_receipt, receipt_root, pinned_keys)
        source_commit = receipt["source_commit"]
        if require_commit(args.source_main_sha, "install source-main commit") != source_commit:
            fail("install source-main commit differs from the private restore receipt")
        validate_provenance_pair(
            receipt["pretag_final_provenance"],
            proof.provenance,
            source_commit=source_commit,
        )

        cli_source = proof.payloads["arc-cli-linux-x86_64"]
        metadata_source = proof.build_metadata_path
        genesis_source = proof.payloads["genesis.toml"]
        cli_raw = read_regular_nofollow(
            cli_source, label="private staged pre-tag ARC CLI", maximum=256 * 1024 * 1024
        )
        if not os.access(cli_source, os.X_OK):
            fail("private staged pre-tag ARC CLI source is not executable")
        metadata_raw = read_regular_nofollow(
            metadata_source, label="private staged pre-tag build metadata", maximum=MAX_JSON_BYTES
        )
        genesis_raw = read_regular_nofollow(
            genesis_source, label="private staged complete genesis", maximum=1024 * 1024
        )
        pinned_cli = pinned / "arc-cli-linux-x86_64"
        pinned_metadata = pinned / "BUILD-METADATA.json"
        pinned_genesis = pinned / "genesis.toml"
        create_file(pinned_cli, cli_raw, 0o500)
        create_file(pinned_metadata, metadata_raw, 0o600)
        create_file(pinned_genesis, genesis_raw, 0o600)
        sync_directory(pinned)
        validate_pretag_cli(
            pinned_cli,
            pinned_metadata,
            pinned_genesis,
            source_commit=source_commit,
            expected_cli_sha256=receipt["arc_cli_sha256"],
            expected_genesis_sha256=receipt["genesis_sha256"],
        )
        genesis_rows = validate_complete_genesis(pinned_genesis)
        locally_verified = verify_keyfiles(
            pinned_cli,
            [keys[node].source_path for node, _ in PRODUCTION_VALIDATORS],
            expected_cli_sha256=receipt["arc_cli_sha256"],
        )
        if any(locally_verified[node].sha256 != keys[node].sha256 for node, _ in PRODUCTION_VALIDATORS):
            fail("trusted local public-derivation contract differs from the restore receipt")
        plan, plan_raw, plan_sidecar_raw = load_freeze_plan(
            args.freeze_plan, args.freeze_plan_sidecar, args.freeze_plan_sha256
        )
        try:
            plan_value = json.loads(plan_raw)
        except json.JSONDecodeError:
            fail("sealed freeze plan is not valid JSON")
        if plan.sha256 != args.freeze_plan_sha256 or plan_value.get("source_commit") != source_commit:
            fail("sealed freeze plan is not bound to the restore source commit")
        maintenance_bundle, maintenance_bundle_nodes, maintenance_bundle_sha256 = (
            load_legacy_maintenance_evidence_bundle(
                args.legacy_maintenance_evidence_bundle,
                args.legacy_maintenance_evidence_bundle_sidecar,
                args.legacy_maintenance_evidence_bundle_sha256,
                source_commit=source_commit,
                freeze_sha256=plan.sha256,
                freeze_plan=plan,
            )
        )
        maintenance_boundary, maintenance_boundary_sha256 = load_legacy_maintenance_boundary(
            args.legacy_maintenance_boundary,
            args.legacy_maintenance_boundary_sidecar,
            args.legacy_maintenance_boundary_sha256,
            source_commit=source_commit,
            freeze_sha256=plan.sha256,
            freeze_plan=plan,
            evidence_bundle=maintenance_bundle,
            evidence_bundle_sha256=maintenance_bundle_sha256,
        )
        evidence, stop_evidence, evidence_sha256 = load_offline_stop_evidence(
            args.offline_stop_evidence,
            args.offline_stop_evidence_sidecar,
            args.offline_stop_evidence_sha256,
            source_commit=source_commit,
            freeze_sha256=plan.sha256,
            freeze_sidecar_sha256=hashlib.sha256(plan_sidecar_raw).hexdigest(),
            freeze_plan=plan,
            evidence_bundle=maintenance_bundle,
            evidence_bundle_nodes=maintenance_bundle_nodes,
            evidence_bundle_sha256=maintenance_bundle_sha256,
            maintenance_boundary=maintenance_boundary,
            maintenance_boundary_sha256=maintenance_boundary_sha256,
        )

        known_hosts_raw = read_regular_nofollow(
            args.known_hosts,
            label="pinned known-hosts file",
            maximum=64 * 1024,
            exact_mode=0o400,
            require_single_link=True,
        )
        known_hosts_sha = require_hash(args.known_hosts_sha256, "known-hosts digest")
        if hashlib.sha256(known_hosts_raw).hexdigest() != known_hosts_sha:
            fail("known-hosts bytes differ from the selected digest")
        validate_exact_known_hosts(known_hosts_raw)
        pinned_known_hosts = pinned / "known_hosts"
        create_file(pinned_known_hosts, known_hosts_raw, 0o400)
        identity_raw = read_regular_nofollow(
            args.ssh_identity,
            label="explicit SSH identity",
            maximum=64 * 1024,
            exact_mode=0o400,
            require_single_link=True,
        )
        identity_sha256 = require_hash(
            args.ssh_identity_sha256, "explicit SSH identity digest"
        )
        if hashlib.sha256(identity_raw).hexdigest() != identity_sha256:
            fail("explicit SSH identity bytes differ from the selected digest")
        pinned_identity = pinned / "id_ed25519"
        create_file(pinned_identity, identity_raw, 0o400)
        transport_root = pinned / "transport"
        transport_root.mkdir(mode=0o700)
        pinned_ssh, pinned_scp, ssh_sha256, scp_sha256 = pin_transport_runtime(
            transport_root,
            ssh=args.ssh,
            ssh_sha256=args.ssh_sha256,
            scp=args.scp,
            scp_sha256=args.scp_sha256,
        )
        sync_directory(pinned)

        # This is a mandatory all-or-nothing preflight immediately before any
        # key probe, upload, or install.  The remote helper is selected by the
        # archive evidence's reviewed SHA-256 and reopens the actual v4 stop
        # roots and persistent restart fence on each hard-coded production
        # host.  A stale or merely self-hashed local JSON document cannot pass.
        def prove_node_stopped(node: str, host: str) -> None:
            node_evidence = stop_evidence[node]
            fresh_status = run_ssh(
                pinned_ssh,
                ssh_sha256,
                transport_root,
                pinned_known_hosts,
                pinned_identity,
                identity_sha256,
                host,
                (
                    "stopped-status",
                    evidence["remote_helper_path"],
                    evidence["remote_helper_sha256"],
                    *node_evidence["argv"],
                ),
            )
            validate_fresh_stopped_status(
                fresh_status,
                node_evidence["status"],
                node_evidence["row"]["stopped_status_sha256"],
                node.lower(),
            )

        for node, _address, host in PRODUCTION_FLEET:
            prove_node_stopped(node, host)

        result_rows: list[dict[str, Any]] = []
        final_reproof = None
        for genesis_row in genesis_rows:
            node = genesis_row["node"]
            key = keys[node]
            host = NODE_TO_HOST[node]
            # Repeat the authenticated fence/root proof at the per-node
            # installation boundary, not only during the fleet preflight.
            prove_node_stopped(node, host)
            if final_reproof is None:
                # Fetch protected main last and seal the complete second live
                # proof at the immediate boundary before the first remote key
                # probe or mutation.  All six fleet fences were already proved.
                final_reproof = proof.recheck()
            probe = exact_control_line(
                run_ssh(
                    pinned_ssh,
                    ssh_sha256,
                    transport_root,
                    pinned_known_hosts,
                    pinned_identity,
                    identity_sha256,
                    host,
                    ("probe", REMOTE_KEY_PATH, key.sha256),
                ),
                f"{node} remote key probe",
            )
            if probe == "MISSING":
                remote_temporary = exact_control_line(
                    run_ssh(
                        pinned_ssh,
                        ssh_sha256,
                        transport_root,
                        pinned_known_hosts,
                        pinned_identity,
                        identity_sha256,
                        host,
                        ("prepare", REMOTE_KEY_DIR, key.sha256),
                    ),
                    f"{node} remote upload prepare",
                )
                if re.fullmatch(
                    rf"/etc/arc-v3/\.validator-key\.upload\.{key.sha256}\.[A-Za-z0-9]+",
                    remote_temporary,
                ) is None:
                    fail("remote host returned an unsafe fresh upload path")
                try:
                    if hashlib.sha256(
                        read_regular_nofollow(
                            key.source_path,
                            label=f"pinned {node} upload key",
                            maximum=MAX_KEY_BYTES,
                            exact_mode=0o600,
                        )
                    ).hexdigest() != key.sha256:
                        fail(f"pinned {node} key changed before SCP upload")
                    run_scp(
                        pinned_scp,
                        scp_sha256,
                        pinned_ssh,
                        ssh_sha256,
                        transport_root,
                        pinned_known_hosts,
                        pinned_identity,
                        identity_sha256,
                        host,
                        key.source_path,
                        remote_temporary,
                    )
                    if hashlib.sha256(
                        read_regular_nofollow(
                            key.source_path,
                            label=f"pinned {node} upload key",
                            maximum=MAX_KEY_BYTES,
                            exact_mode=0o600,
                        )
                    ).hexdigest() != key.sha256:
                        fail(f"pinned {node} key changed during SCP upload")
                    committed = exact_control_line(
                        run_ssh(
                            pinned_ssh,
                            ssh_sha256,
                            transport_root,
                            pinned_known_hosts,
                            pinned_identity,
                            identity_sha256,
                            host,
                            ("commit", remote_temporary, REMOTE_KEY_PATH, key.sha256),
                        ),
                        f"{node} remote key commit",
                    )
                    if committed != "VERIFIED":
                        fail("remote create-only install did not return an exact verification")
                except Exception:
                    try:
                        run_ssh(
                            pinned_ssh,
                            ssh_sha256,
                            transport_root,
                            pinned_known_hosts,
                            pinned_identity,
                            identity_sha256,
                            host,
                            ("cleanup", remote_temporary, key.sha256),
                        )
                    except Exception:
                        pass
                    raise
            elif probe != "VERIFIED":
                fail("remote key probe returned an unsupported state")
            final_probe = exact_control_line(
                run_ssh(
                    pinned_ssh,
                    ssh_sha256,
                    transport_root,
                    pinned_known_hosts,
                    pinned_identity,
                    identity_sha256,
                    host,
                    ("probe", REMOTE_KEY_PATH, key.sha256),
                ),
                f"{node} final remote key probe",
            )
            if final_probe != "VERIFIED":
                fail("remote key failed post-install exact-match verification")
            result_rows.append(
                {
                    "node": node,
                    "address": key.address,
                    "keyfile_sha256": key.sha256,
                    "destination": REMOTE_KEY_PATH,
                    "state": "verified",
                }
            )

        if final_reproof is None:
            fail("reviewed validator topology unexpectedly contained no install target")

        validate_provenance_pair(
            proof.provenance,
            final_reproof.value,
            source_commit=source_commit,
        )

        install_receipt = {
            "schema": INSTALL_SCHEMA,
            "source_commit": source_commit,
            "cms_sha256": receipt["cms_sha256"],
            "arc_cli_sha256": receipt["arc_cli_sha256"],
            "genesis_sha256": receipt["genesis_sha256"],
            "known_hosts_sha256": known_hosts_sha,
            "ssh_identity_sha256": identity_sha256,
            "ssh_sha256": ssh_sha256,
            "scp_sha256": scp_sha256,
            "freeze_plan_sha256": plan.sha256,
            "legacy_maintenance_evidence_bundle_sha256": maintenance_bundle_sha256,
            "legacy_maintenance_boundary_sha256": maintenance_boundary_sha256,
            "offline_stop_evidence_sha256": evidence_sha256,
            "pretag_initial_provenance": proof.provenance,
            "pretag_final_provenance": final_reproof.value,
            "validators": result_rows,
        }
        publish_install_receipt(args.receipt_output, install_receipt)
    finally:
        secure_cleanup(temporary_root)
    print("validator key install complete: six create-only remote identities verified")


def absolute_path(value: str) -> Path:
    path = Path(value)
    if not path.is_absolute():
        raise argparse.ArgumentTypeError("path must be absolute")
    return path


def add_pretag_proof_arguments(command: argparse.ArgumentParser) -> None:
    command.add_argument("--raw-actions-zip", required=True, type=absolute_path)
    command.add_argument("--pretag-run-id", required=True, type=int)
    command.add_argument("--pretag-run-attempt", required=True, type=int)
    command.add_argument("--pretag-artifact-id", required=True, type=int)
    command.add_argument("--curl", required=True, type=absolute_path)
    command.add_argument("--curl-sha256", required=True)
    command.add_argument("--ca-bundle", required=True, type=absolute_path)
    command.add_argument("--ca-bundle-sha256", required=True)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)
    restore_parser = commands.add_parser("restore", help="decrypt and verify into one new private directory")
    restore_parser.add_argument("--cms", required=True, type=absolute_path)
    restore_parser.add_argument("--expected-cms-sha256", required=True)
    restore_parser.add_argument("--rewrap-receipt", required=True, type=absolute_path)
    restore_parser.add_argument("--source-main-sha", required=True)
    add_pretag_proof_arguments(restore_parser)
    restore_parser.add_argument("--restore-certificate", required=True, type=absolute_path)
    restore_parser.add_argument("--restore-private-key", required=True, type=absolute_path)
    restore_parser.add_argument("--openssl", required=True, type=absolute_path)
    restore_parser.add_argument("--openssl-sha256", required=True)
    restore_parser.add_argument("--openssl-libssl", required=True, type=absolute_path)
    restore_parser.add_argument("--openssl-libssl-sha256", required=True)
    restore_parser.add_argument("--openssl-libcrypto", required=True, type=absolute_path)
    restore_parser.add_argument("--openssl-libcrypto-sha256", required=True)
    restore_parser.add_argument("--output-dir", required=True, type=absolute_path)

    install_parser = commands.add_parser("install", help="install verified keys after the sealed legacy freeze")
    install_parser.add_argument("--restore-receipt", required=True, type=absolute_path)
    install_parser.add_argument("--source-main-sha", required=True)
    add_pretag_proof_arguments(install_parser)
    install_parser.add_argument("--freeze-plan", required=True, type=absolute_path)
    install_parser.add_argument("--freeze-plan-sidecar", required=True, type=absolute_path)
    install_parser.add_argument("--freeze-plan-sha256", required=True)
    install_parser.add_argument("--legacy-maintenance-evidence-bundle", required=True, type=absolute_path)
    install_parser.add_argument("--legacy-maintenance-evidence-bundle-sidecar", required=True, type=absolute_path)
    install_parser.add_argument("--legacy-maintenance-evidence-bundle-sha256", required=True)
    install_parser.add_argument("--legacy-maintenance-boundary", required=True, type=absolute_path)
    install_parser.add_argument("--legacy-maintenance-boundary-sidecar", required=True, type=absolute_path)
    install_parser.add_argument("--legacy-maintenance-boundary-sha256", required=True)
    install_parser.add_argument("--offline-stop-evidence", required=True, type=absolute_path)
    install_parser.add_argument("--offline-stop-evidence-sidecar", required=True, type=absolute_path)
    install_parser.add_argument("--offline-stop-evidence-sha256", required=True)
    install_parser.add_argument("--known-hosts", required=True, type=absolute_path)
    install_parser.add_argument("--known-hosts-sha256", required=True)
    install_parser.add_argument("--ssh-identity", required=True, type=absolute_path)
    install_parser.add_argument("--ssh-identity-sha256", required=True)
    install_parser.add_argument("--ssh", required=True, type=absolute_path)
    install_parser.add_argument("--ssh-sha256", required=True)
    install_parser.add_argument("--scp", required=True, type=absolute_path)
    install_parser.add_argument("--scp-sha256", required=True)
    install_parser.add_argument("--receipt-output", required=True, type=absolute_path)
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "restore":
            restore(args)
        else:
            install(args)
    except VaultError as error:
        print(f"validator vault {args.command} failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
