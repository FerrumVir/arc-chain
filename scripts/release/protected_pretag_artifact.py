#!/usr/bin/env python3
"""Live-verify and privately stage one protected-main pre-tag artifact.

This module is the production trust boundary shared by the macOS runtime canary
and validator-vault restore/install helpers.  A caller-selected receipt is never
an authorization input: every invocation performs fresh unauthenticated public
GitHub REST checks for the protected ``main`` branch, exact successful preflight
run/attempt, and exact unexpired Actions artifact ID before opening its raw ZIP.
"""

from __future__ import annotations

import email.utils
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import time
import zipfile
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Iterator, NoReturn, Sequence


REPOSITORY = "FerrumVir/arc-chain"
API_ORIGIN = "https://api.github.com"
PROTECTED_BRANCH = "main"
WORKFLOW_PATH = ".github/workflows/release-signing-preflight.yml"
WORKFLOW_NAME = "Release signing preflight"
PROVENANCE_SCHEMA = "arc.protected-pretag-artifact.v1"
BUILD_SCHEMA = "arc.pretag.artifact.v1"
MAX_API_BYTES = 1024 * 1024
MAX_ACTIONS_ZIP_BYTES = 4 * 1024 * 1024 * 1024
MAX_EXPANDED_GROUP_BYTES = 4 * 1024 * 1024 * 1024
MAX_INNER_EXPANSION_RATIO = 20
EXPANSION_SLACK_BYTES = 64 * 1024 * 1024
MAX_API_AGE_SECONDS = 300
MAX_CLOCK_SKEW_SECONDS = 60
HEX_40 = re.compile(r"^[0-9a-f]{40}$")
HEX_64 = re.compile(r"^[0-9a-f]{64}$")
SERVER_DIGEST = re.compile(r"^sha256:[0-9a-f]{64}$")
SEMVER = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")

DESKTOP_FILES = {
    "linux-x86_64": (
        "arc-desktop-linux-x86_64.AppImage",
        "arc-desktop-linux-x86_64.AppImage.sig",
        "arc-desktop-linux-x86_64.deb",
        "arc-desktop-linux-x86_64.rpm",
    ),
    "macos-arm64": (
        "arc-desktop-macos-arm64.app.tar.gz",
        "arc-desktop-macos-arm64.app.tar.gz.sig",
        "arc-desktop-macos-arm64.dmg",
    ),
    "macos-x86_64": (
        "arc-desktop-macos-x86_64.app.tar.gz",
        "arc-desktop-macos-x86_64.app.tar.gz.sig",
        "arc-desktop-macos-x86_64.dmg",
    ),
    "windows-x86_64": (
        "arc-desktop-windows-x86_64-setup.exe",
        "arc-desktop-windows-x86_64-setup.exe.sig",
        "arc-desktop-windows-x86_64.msi",
    ),
}

RUST_TARGETS = {
    "linux-x86_64": "x86_64-unknown-linux-gnu",
    "linux-arm64": "aarch64-unknown-linux-gnu",
    "macos-arm64": "aarch64-apple-darwin",
    "macos-x86_64": "x86_64-apple-darwin",
    "windows-x86_64": "x86_64-pc-windows-msvc",
}

# The release-signing-preflight workflow uploads exactly these nine groups, in
# this reviewed order.  Set proofs fail closed unless the run's complete
# artifact listing and the caller's rows both match this tuple exactly.
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


class ProvenanceError(ValueError):
    """The live API or candidate bytes violate the protected artifact contract."""


def fail(message: str) -> NoReturn:
    raise ProvenanceError(message)


def canonical_json(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_descriptor(descriptor: int) -> str:
    digest = hashlib.sha256()
    os.lseek(descriptor, 0, os.SEEK_SET)
    while chunk := os.read(descriptor, 1024 * 1024):
        digest.update(chunk)
    os.lseek(descriptor, 0, os.SEEK_SET)
    return digest.hexdigest()


def sha256_file(path: Path) -> str:
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        details = os.fstat(descriptor)
        if not stat.S_ISREG(details.st_mode):
            fail(f"hash input is not a regular file: {path}")
        return sha256_descriptor(descriptor)
    finally:
        os.close(descriptor)


def fsync_directory(path: Path) -> None:
    descriptor = os.open(
        path,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def write_all(descriptor: int, payload: bytes) -> None:
    offset = 0
    while offset < len(payload):
        try:
            written = os.write(descriptor, payload[offset:])
        except InterruptedError:
            continue
        if written <= 0:
            fail("create-only artifact staging made no write progress")
        offset += written


def create_file(path: Path, payload: bytes, mode: int) -> None:
    descriptor = os.open(
        path,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
        mode,
    )
    try:
        write_all(descriptor, payload)
        os.fchmod(descriptor, mode)
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def require_private_root(path: Path) -> Path:
    path = Path(os.path.abspath(path))
    details = path.lstat()
    if (
        stat.S_ISLNK(details.st_mode)
        or not stat.S_ISDIR(details.st_mode)
        or details.st_uid != os.getuid()
        or stat.S_IMODE(details.st_mode) != 0o700
    ):
        fail("artifact transaction root must be operator-owned mode 0700 and non-symlink")
    return path


def protected_root_file(
    path: Path,
    expected_sha256: str,
    *,
    label: str,
    executable: bool,
    maximum: int,
) -> os.stat_result:
    if not path.is_absolute() or Path(os.path.abspath(path)) != path:
        fail(f"{label} path must be normalized and absolute")
    if HEX_64.fullmatch(expected_sha256) is None:
        fail(f"{label} expected SHA-256 is malformed")
    current = Path(path.anchor)
    for part in path.parts[1:-1]:
        current /= part
        details = current.lstat()
        if (
            stat.S_ISLNK(details.st_mode)
            or not stat.S_ISDIR(details.st_mode)
            or details.st_uid != 0
            or stat.S_IMODE(details.st_mode) & 0o022
        ):
            fail(f"{label} ancestry is not root-owned and protected")
    details = path.lstat()
    if (
        stat.S_ISLNK(details.st_mode)
        or not stat.S_ISREG(details.st_mode)
        or details.st_uid != 0
        or stat.S_IMODE(details.st_mode) & 0o022
        or details.st_nlink != 1
        or details.st_size <= 0
        or details.st_size > maximum
        or (executable and not os.access(path, os.X_OK))
    ):
        fail(f"{label} is not one protected root-owned regular file")
    if sha256_file(path) != expected_sha256:
        fail(f"{label} differs from its reviewed SHA-256")
    return details


def identity(details: os.stat_result) -> tuple[int, ...]:
    return (
        details.st_dev,
        details.st_ino,
        details.st_size,
        details.st_mtime_ns,
        details.st_ctime_ns,
        stat.S_IMODE(details.st_mode),
        details.st_nlink,
        details.st_uid,
    )


@dataclass(frozen=True)
class ApiDocument:
    value: dict[str, Any]
    body_sha256: str
    response_unix: int
    request_id: str
    cache_control: str = ""
    age: int = 0


class CurlApiClient:
    """Small config-free public GitHub JSON client with a fixed trust store."""

    def __init__(
        self,
        curl: Path,
        curl_sha256: str,
        ca_bundle: Path,
        ca_bundle_sha256: str,
        transaction_root: Path,
        *,
        now: int,
    ) -> None:
        self.curl = Path(curl)
        self.curl_sha256 = curl_sha256
        self.ca_bundle = Path(ca_bundle)
        self.ca_bundle_sha256 = ca_bundle_sha256
        self.transaction_root = require_private_root(transaction_root)
        self.now = now
        self.curl_identity = identity(
            protected_root_file(
                self.curl,
                curl_sha256,
                label="GitHub REST curl",
                executable=True,
                maximum=32 * 1024 * 1024,
            )
        )
        self.ca_identity = identity(
            protected_root_file(
                self.ca_bundle,
                ca_bundle_sha256,
                label="GitHub REST CA bundle",
                executable=False,
                maximum=4 * 1024 * 1024,
            )
        )
        self.counter = 0

    def _reprove_tools(self) -> None:
        curl = protected_root_file(
            self.curl,
            self.curl_sha256,
            label="GitHub REST curl",
            executable=True,
            maximum=32 * 1024 * 1024,
        )
        ca = protected_root_file(
            self.ca_bundle,
            self.ca_bundle_sha256,
            label="GitHub REST CA bundle",
            executable=False,
            maximum=4 * 1024 * 1024,
        )
        if identity(curl) != self.curl_identity or identity(ca) != self.ca_identity:
            fail("GitHub REST curl or CA trust anchor changed during live verification")

    @staticmethod
    def _parse_headers(raw: bytes, *, now: int) -> tuple[int, str, str, int]:
        try:
            text = raw.decode("iso-8859-1")
        except UnicodeDecodeError:
            fail("GitHub REST response headers are not decodable")
        blocks = [block for block in re.split(r"\r?\n\r?\n", text) if block.strip()]
        if len(blocks) != 1:
            fail("GitHub REST returned redirects, proxy preambles, or multiple responses")
        lines = blocks[0].splitlines()
        if not lines or re.fullmatch(r"HTTP/(?:1\.[01]|2) 200(?: .*)?", lines[0]) is None:
            fail("GitHub REST did not return one direct HTTP 200 response")
        headers: dict[str, str] = {}
        for line in lines[1:]:
            if ":" not in line:
                fail("GitHub REST returned a malformed response header")
            name, value = line.split(":", 1)
            key = name.strip().lower()
            if key in headers and key in {
                "date",
                "x-github-request-id",
                "cache-control",
                "age",
            }:
                fail(f"GitHub REST duplicated security-relevant header {key}")
            headers[key] = value.strip()
        date = headers.get("date")
        request_id = headers.get("x-github-request-id")
        cache_control = headers.get("cache-control", "")
        age_raw = headers.get("age", "0")
        if not date or not request_id or re.fullmatch(r"[A-F0-9:-]{8,128}", request_id) is None:
            fail("GitHub REST response lacks canonical Date or request ID")
        parsed = email.utils.parsedate_to_datetime(date)
        if parsed is None or parsed.tzinfo is None:
            fail("GitHub REST Date header is invalid")
        response_unix = int(parsed.timestamp())
        age = now - response_unix
        if age > MAX_API_AGE_SECONDS or age < -MAX_CLOCK_SKEW_SECONDS:
            fail("GitHub REST response is stale or beyond the allowed clock skew")
        if re.fullmatch(r"[0-9]+", age_raw) is None or int(age_raw) != 0:
            fail("GitHub REST response was served from a positively aged cache")
        if len(cache_control) > 1024 or any(
            character in cache_control for character in ("\r", "\n", "\0")
        ):
            fail("GitHub REST cache-control metadata is malformed")
        return response_unix, request_id, cache_control, int(age_raw)

    def get_json(self, endpoint: str, *, label: str) -> ApiDocument:
        if not endpoint.startswith("/repos/FerrumVir/arc-chain/") or any(
            character in endpoint for character in ("\r", "\n", "\0", "#")
        ):
            fail("GitHub REST endpoint escaped the fixed repository boundary")
        self.counter += 1
        header = self.transaction_root / f"api-{self.counter}.headers"
        body = self.transaction_root / f"api-{self.counter}.json"
        self._reprove_tools()
        command = [
            str(self.curl),
            "-q",
            "--silent",
            "--show-error",
            "--fail",
            "--request",
            "GET",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--cacert",
            str(self.ca_bundle),
            "--config",
            "/dev/null",
            "--proxy",
            "",
            "--noproxy",
            "*",
            "--max-redirs",
            "0",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "--max-filesize",
            str(MAX_API_BYTES),
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
            "--header",
            "Authorization:",
            "--header",
            "Cache-Control: no-cache",
            "--header",
            "Pragma: no-cache",
            "--user-agent",
            "arc-chain-protected-artifact-v1",
            "--dump-header",
            str(header),
            "--output",
            str(body),
            f"{API_ORIGIN}{endpoint}",
        ]
        environment = {
            "HOME": str(self.transaction_root),
            "PATH": "/usr/bin:/bin",
            "LANG": "C",
            "LC_ALL": "C",
        }
        try:
            result = subprocess.run(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                env=environment,
                timeout=40,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired):
            fail(f"{label} GitHub REST request could not complete")
        self._reprove_tools()
        if result.returncode != 0:
            fail(f"{label} GitHub REST request failed closed")
        header_raw = read_staged_file(header, "GitHub REST headers", 64 * 1024)
        body_raw = read_staged_file(body, "GitHub REST body", MAX_API_BYTES)
        response_unix, request_id, cache_control, age = self._parse_headers(
            header_raw, now=self.now
        )
        try:
            value = json.loads(body_raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            fail(f"{label} GitHub REST body is not valid JSON")
        if not isinstance(value, dict):
            fail(f"{label} GitHub REST body is not one JSON object")
        return ApiDocument(
            value,
            sha256_bytes(body_raw),
            response_unix,
            request_id,
            cache_control,
            age,
        )


def read_staged_file(path: Path, label: str, maximum: int) -> bytes:
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        details = os.fstat(descriptor)
        if (
            not stat.S_ISREG(details.st_mode)
            or details.st_uid != os.getuid()
            or details.st_nlink != 1
            or details.st_size <= 0
            or details.st_size > maximum
        ):
            fail(f"{label} is empty, oversized, linked, or not operator-owned regular data")
        chunks: list[bytes] = []
        while chunk := os.read(descriptor, 1024 * 1024):
            chunks.append(chunk)
        return b"".join(chunks)
    finally:
        os.close(descriptor)


def stable_copy(source: Path, destination: Path, *, maximum: int, mode: int) -> tuple[str, int]:
    try:
        descriptor = os.open(source, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    except OSError:
        fail("raw Actions ZIP cannot be opened through the no-follow boundary")
    try:
        before = os.fstat(descriptor)
        before_identity = identity(before)
        if (
            not stat.S_ISREG(before.st_mode)
            or before.st_uid != os.getuid()
            or stat.S_IMODE(before.st_mode) & 0o022
            or before.st_nlink != 1
            or before.st_size <= 0
            or before.st_size > maximum
        ):
            fail("raw Actions ZIP must be one protected operator-owned regular file")
        output = os.open(
            destination,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
            mode,
        )
        digest = hashlib.sha256()
        total = 0
        try:
            while chunk := os.read(descriptor, 1024 * 1024):
                digest.update(chunk)
                total += len(chunk)
                write_all(output, chunk)
            os.fchmod(output, mode)
            os.fsync(output)
        finally:
            os.close(output)
        after = os.fstat(descriptor)
        if identity(after) != before_identity or total != before.st_size:
            fail("raw Actions ZIP changed during stable no-follow staging")
        return digest.hexdigest(), total
    finally:
        os.close(descriptor)


def expected_files(kind: str, platform: str) -> tuple[str, ...]:
    if kind == "desktop":
        if platform not in DESKTOP_FILES:
            fail(f"unsupported protected desktop artifact platform {platform!r}")
        return DESKTOP_FILES[platform]
    if kind != "headless" or platform not in RUST_TARGETS:
        fail(f"unsupported protected artifact group {kind}/{platform}")
    suffix = ".exe" if platform == "windows-x86_64" else ""
    return (f"arc-node-{platform}{suffix}", f"arc-cli-{platform}{suffix}", "genesis.toml")


def exact_object(value: object, fields: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != fields:
        fail(f"{label} has missing, unknown, or unsupported fields")
    return value


def validate_canonical_provenance(
    value: object,
    *,
    canonical_bytes: bytes | None = None,
    response_labels: Sequence[str] = (
        "workflow",
        "run",
        "artifact",
        "protected_main",
    ),
) -> dict[str, Any]:
    """Validate one complete v1 proof and all internal cross-bindings."""

    proof = exact_object(
        value,
        {"schema", "live", "api", "artifact"},
        "protected artifact provenance",
    )
    if proof["schema"] != PROVENANCE_SCHEMA or (
        canonical_bytes is not None and canonical_bytes != canonical_json(proof)
    ):
        fail("protected artifact provenance is not canonical v1 JSON")
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
        "protected artifact live tuple",
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
        "protected artifact API proof",
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
        "protected artifact byte proof",
    )
    for field in (
        "workflow_id",
        "run_id",
        "run_attempt",
        "artifact_id",
        "artifact_size_in_bytes",
        "api_verified_at_unix",
    ):
        if (
            isinstance(live[field], bool)
            or not isinstance(live[field], int)
            or live[field] <= 0
        ):
            fail(f"protected artifact live field {field} is not a positive integer")
    for field in ("curl_sha256", "ca_bundle_sha256"):
        if not isinstance(api[field], str) or HEX_64.fullmatch(api[field]) is None:
            fail(f"protected artifact API {field} is malformed")
    for field in (
        "raw_actions_zip_sha256",
        "archive_sha256",
        "build_metadata_sha256",
    ):
        if not isinstance(artifact[field], str) or HEX_64.fullmatch(artifact[field]) is None:
            fail(f"protected artifact {field} is malformed")
    kind = artifact["kind"]
    platform = artifact["platform"]
    required_files = expected_files(kind, platform)
    files = artifact["files"]
    if not isinstance(files, dict) or set(files) != set(required_files):
        fail("protected artifact payload membership differs")
    for name in required_files:
        if not isinstance(files[name], str) or HEX_64.fullmatch(files[name]) is None:
            fail(f"protected artifact payload hash is malformed: {name}")
    archive_sha256 = artifact["archive_sha256"]
    prefix = (
        f"arc-pretag-{kind}-{platform}-{live['commit']}-"
        f"{live['run_id']}-{live['run_attempt']}-"
    )
    if (
        live["repository"] != REPOSITORY
        or live["protected_branch"] != PROTECTED_BRANCH
        or not isinstance(live["commit"], str)
        or HEX_40.fullmatch(live["commit"]) is None
        or live["workflow_path"] != WORKFLOW_PATH
        or live["artifact_name"] != prefix + archive_sha256
        or live["artifact_digest"]
        != "sha256:" + artifact["raw_actions_zip_sha256"]
        or isinstance(artifact["raw_actions_zip_size"], bool)
        or not isinstance(artifact["raw_actions_zip_size"], int)
        or artifact["raw_actions_zip_size"] <= 0
        or artifact["raw_actions_zip_size"] != live["artifact_size_in_bytes"]
        or not isinstance(artifact["version"], str)
        or SEMVER.fullmatch(artifact["version"]) is None
        or api["origin"] != API_ORIGIN
        or api["anonymous"] is not True
        or api["redirects_followed"] is not False
        or api["max_age_seconds"] != MAX_API_AGE_SECONDS
    ):
        fail("protected artifact provenance tuple or byte cross-binding differs")
    responses = api["responses"]
    if (
        not isinstance(responses, list)
        or len(responses) != len(response_labels)
        or [row.get("label") for row in responses if isinstance(row, dict)]
        != list(response_labels)
    ):
        fail("protected artifact API response set differs")
    response_times: list[int] = []
    for row in responses:
        row = exact_object(
            row,
            {
                "label",
                "body_sha256",
                "response_unix",
                "request_id",
                "cache_control",
                "age",
            },
            "protected artifact API response",
        )
        if (
            not isinstance(row["body_sha256"], str)
            or HEX_64.fullmatch(row["body_sha256"]) is None
            or isinstance(row["response_unix"], bool)
            or not isinstance(row["response_unix"], int)
            or row["response_unix"] <= 0
            or not isinstance(row["request_id"], str)
            or re.fullmatch(r"[A-F0-9:-]{8,128}", row["request_id"]) is None
            or not isinstance(row["cache_control"], str)
            or len(row["cache_control"]) > 1024
            or any(character in row["cache_control"] for character in ("\r", "\n", "\0"))
            or isinstance(row["age"], bool)
            or row["age"] != 0
        ):
            fail("protected artifact API response is malformed or cached")
        response_times.append(row["response_unix"])
    if live["api_verified_at_unix"] != min(response_times):
        fail("protected artifact live timestamp does not cover every API response")
    return proof


def _api_row(label: str, document: ApiDocument) -> dict[str, Any]:
    return {
        "label": label,
        "body_sha256": document.body_sha256,
        "response_unix": document.response_unix,
        "request_id": document.request_id,
        "cache_control": document.cache_control,
        "age": document.age,
    }


def _validate_workflow(document: ApiDocument) -> int:
    value = document.value
    workflow_id = value.get("id")
    if (
        isinstance(workflow_id, bool)
        or not isinstance(workflow_id, int)
        or workflow_id <= 0
        or value.get("name") != WORKFLOW_NAME
        or value.get("path") != WORKFLOW_PATH
        or value.get("state") != "active"
    ):
        fail("live workflow is not the active reviewed release-signing preflight")
    return workflow_id


def _validate_run(
    document: ApiDocument,
    *,
    workflow_id: int,
    commit: str,
    run_id: int,
    run_attempt: int,
) -> None:
    value = document.value
    head_repository = value.get("head_repository")
    if (
        value.get("id") != run_id
        or value.get("workflow_id") != workflow_id
        or value.get("run_attempt") != run_attempt
        or value.get("head_branch") != PROTECTED_BRANCH
        or value.get("head_sha") != commit
        or value.get("event") != "workflow_dispatch"
        or value.get("status") != "completed"
        or value.get("conclusion") != "success"
        or value.get("path") not in (None, WORKFLOW_PATH)
        or not isinstance(head_repository, dict)
        or head_repository.get("full_name") != REPOSITORY
    ):
        fail("selected run/attempt is not the completed successful exact-main preflight")


def _validate_branch(document: ApiDocument, *, commit: str) -> None:
    value = document.value
    branch_commit = value.get("commit")
    if (
        value.get("name") != PROTECTED_BRANCH
        or value.get("protected") is not True
        or not isinstance(branch_commit, dict)
        or branch_commit.get("sha") != commit
    ):
        fail("selected source is not the current protected main commit")


def _validate_artifact(
    value: object,
    *,
    artifact_id: int,
    kind: str,
    platform: str,
    commit: str,
    run_id: int,
    run_attempt: int,
) -> tuple[str, str, int]:
    if not isinstance(value, dict):
        fail("live artifact is not one JSON object")
    workflow_run = value.get("workflow_run")
    name = value.get("name")
    prefix = f"arc-pretag-{kind}-{platform}-{commit}-{run_id}-{run_attempt}-"
    if (
        isinstance(value.get("id"), bool)
        or value.get("id") != artifact_id
        or not isinstance(name, str)
        or not name.startswith(prefix)
        or HEX_64.fullmatch(name.removeprefix(prefix)) is None
        or value.get("expired") is not False
        or isinstance(value.get("size_in_bytes"), bool)
        or not isinstance(value.get("size_in_bytes"), int)
        or not 0 < value["size_in_bytes"] <= MAX_ACTIONS_ZIP_BYTES
        or not isinstance(value.get("digest"), str)
        or SERVER_DIGEST.fullmatch(value["digest"]) is None
        or not isinstance(workflow_run, dict)
        or workflow_run.get("id") != run_id
        or workflow_run.get("head_branch") not in (None, PROTECTED_BRANCH)
        or workflow_run.get("head_sha") not in (None, commit)
    ):
        fail("live artifact ID/name/digest/size/expiry/run binding is invalid")
    return name, value["digest"], value["size_in_bytes"]


def _live_tuple(
    *,
    workflow_id: int,
    commit: str,
    run_id: int,
    run_attempt: int,
    artifact_id: int,
    name: str,
    digest: str,
    size: int,
    documents: Sequence[ApiDocument],
) -> dict[str, Any]:
    return {
        "repository": REPOSITORY,
        "protected_branch": PROTECTED_BRANCH,
        "commit": commit,
        "workflow_id": workflow_id,
        "workflow_path": WORKFLOW_PATH,
        "run_id": run_id,
        "run_attempt": run_attempt,
        "artifact_id": artifact_id,
        "artifact_name": name,
        "artifact_digest": digest,
        "artifact_size_in_bytes": size,
        "api_verified_at_unix": min(document.response_unix for document in documents),
    }


def prove_live_api(
    client: CurlApiClient,
    *,
    commit: str,
    run_id: int,
    run_attempt: int,
    artifact_id: int,
    kind: str,
    platform: str,
) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    """Prove one artifact, checking protected main as the final API read."""

    workflow = client.get_json(
        f"/repos/{REPOSITORY}/actions/workflows/release-signing-preflight.yml",
        label="preflight workflow",
    )
    workflow_id = _validate_workflow(workflow)
    run = client.get_json(
        f"/repos/{REPOSITORY}/actions/runs/{run_id}", label="preflight run"
    )
    _validate_run(
        run,
        workflow_id=workflow_id,
        commit=commit,
        run_id=run_id,
        run_attempt=run_attempt,
    )
    artifact = client.get_json(
        f"/repos/{REPOSITORY}/actions/artifacts/{artifact_id}", label="Actions artifact"
    )
    name, digest, size = _validate_artifact(
        artifact.value,
        artifact_id=artifact_id,
        kind=kind,
        platform=platform,
        commit=commit,
        run_id=run_id,
        run_attempt=run_attempt,
    )
    # This is intentionally last.  Consumers publish immediately after their
    # final reproof, so main cannot move during earlier API reads unnoticed.
    branch = client.get_json(
        f"/repos/{REPOSITORY}/branches/{PROTECTED_BRANCH}", label="protected main"
    )
    _validate_branch(branch, commit=commit)
    documents = (workflow, run, artifact, branch)
    live = _live_tuple(
        workflow_id=workflow_id,
        commit=commit,
        run_id=run_id,
        run_attempt=run_attempt,
        artifact_id=artifact_id,
        name=name,
        digest=digest,
        size=size,
        documents=documents,
    )
    return live, [
        _api_row(label, document)
        for label, document in zip(
            ("workflow", "run", "artifact", "protected_main"), documents
        )
    ]


def _normalize_live_set_rows(rows: Sequence[dict[str, Any]]) -> tuple[dict[str, Any], ...]:
    if isinstance(rows, (str, bytes, bytearray)) or len(rows) != len(PRETAG_GROUPS):
        fail("protected artifact set must contain exactly nine canonical rows")
    normalized: list[dict[str, Any]] = []
    artifact_ids: set[int] = set()
    for index, (row, expected_group) in enumerate(zip(rows, PRETAG_GROUPS)):
        row = exact_object(
            row,
            {"artifact_id", "kind", "platform"},
            f"protected artifact set row {index}",
        )
        artifact_id = row["artifact_id"]
        if (
            isinstance(artifact_id, bool)
            or not isinstance(artifact_id, int)
            or artifact_id <= 0
            or (row["kind"], row["platform"]) != expected_group
            or artifact_id in artifact_ids
        ):
            fail("protected artifact set row order, group, or ID is invalid")
        expected_files(row["kind"], row["platform"])
        artifact_ids.add(artifact_id)
        normalized.append(dict(row))
    return tuple(normalized)


def prove_live_api_set(
    client: CurlApiClient,
    *,
    commit: str,
    run_id: int,
    run_attempt: int,
    rows: Sequence[dict[str, Any]],
) -> tuple[tuple[dict[str, Any], ...], tuple[list[dict[str, Any]], ...]]:
    """Prove the exact nine-artifact preflight set in four public API reads."""

    normalized = _normalize_live_set_rows(rows)
    workflow = client.get_json(
        f"/repos/{REPOSITORY}/actions/workflows/release-signing-preflight.yml",
        label="preflight workflow",
    )
    workflow_id = _validate_workflow(workflow)
    run = client.get_json(
        f"/repos/{REPOSITORY}/actions/runs/{run_id}", label="preflight run"
    )
    _validate_run(
        run,
        workflow_id=workflow_id,
        commit=commit,
        run_id=run_id,
        run_attempt=run_attempt,
    )
    artifact_set = client.get_json(
        f"/repos/{REPOSITORY}/actions/runs/{run_id}/artifacts?per_page=100",
        label="Actions artifact set",
    )
    listing = artifact_set.value
    artifacts = listing.get("artifacts")
    total_count = listing.get("total_count")
    if (
        isinstance(total_count, bool)
        or total_count != len(PRETAG_GROUPS)
        or not isinstance(artifacts, list)
        or len(artifacts) != len(PRETAG_GROUPS)
    ):
        fail("live preflight run does not contain exactly nine unpaginated artifacts")
    by_id: dict[int, dict[str, Any]] = {}
    names: set[str] = set()
    for artifact in artifacts:
        if not isinstance(artifact, dict):
            fail("live preflight artifact listing contains a non-object row")
        artifact_id = artifact.get("id")
        name = artifact.get("name")
        if (
            isinstance(artifact_id, bool)
            or not isinstance(artifact_id, int)
            or artifact_id <= 0
            or artifact_id in by_id
            or not isinstance(name, str)
            or name in names
        ):
            fail("live preflight artifact listing has duplicate or malformed identity")
        by_id[artifact_id] = artifact
        names.add(name)
    if set(by_id) != {row["artifact_id"] for row in normalized}:
        fail("live preflight artifact IDs differ from the exact selected set")

    validated: list[tuple[dict[str, Any], str, str, int]] = []
    for row in normalized:
        name, digest, size = _validate_artifact(
            by_id[row["artifact_id"]],
            artifact_id=row["artifact_id"],
            kind=row["kind"],
            platform=row["platform"],
            commit=commit,
            run_id=run_id,
            run_attempt=run_attempt,
        )
        validated.append((row, name, digest, size))

    # The protected branch must be the final security-relevant read after the
    # complete artifact set has already been fetched and validated.
    branch = client.get_json(
        f"/repos/{REPOSITORY}/branches/{PROTECTED_BRANCH}", label="protected main"
    )
    _validate_branch(branch, commit=commit)
    documents = (workflow, run, artifact_set, branch)
    response_rows = [
        _api_row(label, document)
        for label, document in zip(
            ("workflow", "run", "artifact_set", "protected_main"), documents
        )
    ]
    lives = tuple(
        _live_tuple(
            workflow_id=workflow_id,
            commit=commit,
            run_id=run_id,
            run_attempt=run_attempt,
            artifact_id=row["artifact_id"],
            name=name,
            digest=digest,
            size=size,
            documents=documents,
        )
        for row, name, digest, size in validated
    )
    return lives, tuple([dict(response) for response in response_rows] for _ in lives)


def extract_outer(
    actions_zip: Path,
    output: Path,
    *,
    archive_name: str,
) -> tuple[Path, Path]:
    expected = {"SHA256SUMS", archive_name}
    try:
        with zipfile.ZipFile(actions_zip, "r") as outer:
            infos = outer.infolist()
            if len(infos) != 2:
                fail("Actions ZIP must contain exactly SHA256SUMS and one inner archive")
            names: set[str] = set()
            expanded = 0
            for info in infos:
                pure = PurePosixPath(info.filename)
                mode_type = (info.external_attr >> 16) & 0o170000
                if (
                    pure.is_absolute()
                    or ".." in pure.parts
                    or len(pure.parts) != 1
                    or "\\" in info.filename
                    or ":" in info.filename
                    or info.is_dir()
                    or info.flag_bits & 0x1
                    or mode_type not in (0, 0o100000)
                    or info.file_size <= 0
                ):
                    fail("Actions ZIP contains unsafe, encrypted, empty, or non-regular data")
                if info.filename in names:
                    fail("Actions ZIP contains duplicate membership")
                names.add(info.filename)
                expanded += info.file_size
            if names != expected:
                fail("Actions ZIP membership differs from the exact protected artifact")
            if expanded > min(
                MAX_EXPANDED_GROUP_BYTES,
                actions_zip.stat().st_size + EXPANSION_SLACK_BYTES,
            ):
                fail("Actions ZIP exceeds its bounded expansion contract")
            for info in infos:
                destination = output / info.filename
                descriptor = os.open(
                    destination,
                    os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
                    0o400,
                )
                try:
                    with outer.open(info, "r") as source:
                        while chunk := source.read(1024 * 1024):
                            write_all(descriptor, chunk)
                    os.fchmod(descriptor, 0o400)
                    os.fsync(descriptor)
                finally:
                    os.close(descriptor)
    except (OSError, zipfile.BadZipFile):
        fail("raw Actions artifact is not one readable bounded ZIP")
    return output / "SHA256SUMS", output / archive_name


def parse_checksums(
    path: Path,
    *,
    kind: str,
    platform: str,
    commit: str,
    run_id: int,
    run_attempt: int,
    archive_name: str,
) -> str:
    raw = read_staged_file(path, "pre-tag SHA256SUMS", 64 * 1024)
    try:
        lines = raw.decode("ascii").splitlines()
    except UnicodeDecodeError:
        fail("pre-tag SHA256SUMS is not ASCII")
    expected_headers = (
        "# ARC pre-tag artifact v1",
        f"# kind={kind}",
        f"# repository={REPOSITORY}",
        f"# commit={commit}",
        f"# run_id={run_id}",
        f"# run_attempt={run_attempt}",
        f"# platform={platform}",
    )
    if tuple(lines[:7]) != expected_headers or len(lines) != 8:
        fail("pre-tag SHA256SUMS headers differ from the live artifact tuple")
    match = re.fullmatch(rf"([0-9a-f]{{64}})  {re.escape(archive_name)}", lines[7])
    if match is None:
        fail("pre-tag SHA256SUMS archive record is invalid")
    return match.group(1)


def extract_inner(
    archive_path: Path,
    payload_root: Path,
    *,
    kind: str,
    platform: str,
    commit: str,
    run_id: int,
    run_attempt: int,
    version: str,
) -> tuple[dict[str, Any], bytes, dict[str, Path]]:
    required = expected_files(kind, platform)
    expected_members = set(required) | {"BUILD-METADATA.json"}
    payload_root.mkdir(mode=0o700)
    try:
        with tarfile.open(archive_path, "r:gz") as archive:
            if archive.pax_headers:
                fail("pre-tag archive has unsupported global PAX metadata")
            members: dict[str, tarfile.TarInfo] = {}
            expanded = 0
            for member in archive.getmembers():
                pure = PurePosixPath(member.name)
                if (
                    pure.is_absolute()
                    or ".." in pure.parts
                    or len(pure.parts) != 1
                    or "\\" in member.name
                    or ":" in member.name
                    or not member.isfile()
                    or member.issym()
                    or member.islnk()
                    or member.issparse()
                    or member.size <= 0
                ):
                    fail("pre-tag archive contains unsafe or unsupported membership")
                if set(member.pax_headers) - {"mtime"} or (
                    "mtime" in member.pax_headers
                    and re.fullmatch(r"[0-9]+(?:\.[0-9]+)?", member.pax_headers["mtime"])
                    is None
                ):
                    fail("pre-tag archive contains unsupported PAX metadata")
                if member.name in members:
                    fail("pre-tag archive contains duplicate membership")
                members[member.name] = member
                expanded += member.size
            if set(members) != expected_members:
                fail("pre-tag archive membership differs from the exact group")
            if expanded > min(
                MAX_EXPANDED_GROUP_BYTES,
                archive_path.stat().st_size * MAX_INNER_EXPANSION_RATIO
                + EXPANSION_SLACK_BYTES,
            ):
                fail("pre-tag inner archive exceeds its bounded expansion contract")
            paths: dict[str, Path] = {}
            for name, member in members.items():
                source = archive.extractfile(member)
                if source is None:
                    fail("pre-tag archive member has no readable payload")
                mode = 0o500 if name.startswith(("arc-node-", "arc-cli-")) else 0o400
                destination = payload_root / name
                descriptor = os.open(
                    destination,
                    os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
                    mode,
                )
                try:
                    with source:
                        remaining = member.size
                        while remaining:
                            chunk = source.read(min(1024 * 1024, remaining))
                            if not chunk:
                                fail("pre-tag archive member ended before its declared size")
                            write_all(descriptor, chunk)
                            remaining -= len(chunk)
                        if source.read(1):
                            fail("pre-tag archive member exceeded its declared size")
                    os.fchmod(descriptor, mode)
                    os.fsync(descriptor)
                finally:
                    os.close(descriptor)
                paths[name] = destination
    except (OSError, tarfile.TarError):
        fail("pre-tag inner archive is not one readable bounded tar.gz")
    fsync_directory(payload_root)

    metadata_raw = read_staged_file(
        paths["BUILD-METADATA.json"], "pre-tag BUILD-METADATA", 64 * 1024
    )
    try:
        metadata = json.loads(metadata_raw)
    except (UnicodeDecodeError, json.JSONDecodeError):
        fail("pre-tag BUILD-METADATA is not valid JSON")
    if not isinstance(metadata, dict) or metadata_raw != canonical_json(metadata):
        fail("pre-tag BUILD-METADATA is not canonical JSON")
    metadata = exact_object(
        metadata,
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
        "pre-tag BUILD-METADATA",
    )
    expected = {
        "schema": BUILD_SCHEMA,
        "kind": kind,
        "repository": REPOSITORY,
        "commit": commit,
        "platform": platform,
        "rust_target": RUST_TARGETS[platform],
        "version": version,
        "workflow_run_id": run_id,
        "workflow_run_attempt": run_attempt,
    }
    if any(metadata.get(field) != value for field, value in expected.items()):
        fail("pre-tag BUILD-METADATA differs from the exact live tuple")
    hashes = metadata.get("files")
    if not isinstance(hashes, dict) or set(hashes) != set(required):
        fail("pre-tag BUILD-METADATA payload membership differs")
    for name in required:
        expected_hash = hashes.get(name)
        if not isinstance(expected_hash, str) or HEX_64.fullmatch(expected_hash) is None:
            fail("pre-tag BUILD-METADATA contains a malformed payload digest")
        if sha256_file(paths[name]) != expected_hash:
            fail(f"pre-tag payload hash mismatch: {name}")
    return metadata, metadata_raw, {name: paths[name] for name in required}


@dataclass(frozen=True)
class FinalLiveReproof:
    value: dict[str, Any]
    canonical_bytes: bytes
    path: Path | None = None


@dataclass(frozen=True)
class VerifiedArtifact:
    transaction_root: Path
    raw_actions_zip: Path
    payload_root: Path
    payloads: dict[str, Path]
    build_metadata: dict[str, Any]
    build_metadata_path: Path
    provenance: dict[str, Any]
    provenance_path: Path
    provenance_bytes: bytes
    _client: CurlApiClient
    _pins: dict[str, Any]
    _fixed_now: int | None

    def recheck(self) -> FinalLiveReproof:
        """Freshly re-prove the same live tuple immediately before publication."""

        self._client.now = (
            int(time.time()) if self._fixed_now is None else int(self._fixed_now)
        )
        live, api_rows = prove_live_api(self._client, **self._pins)
        invariant_fields = {
            key
            for key in self.provenance["live"]
            if key != "api_verified_at_unix"
        }
        if (
            {key: live.get(key) for key in invariant_fields}
            != {key: self.provenance["live"].get(key) for key in invariant_fields}
            or set(live) != set(self.provenance["live"])
        ):
            fail("final live recheck tuple differs from the initially verified artifact")
        value = {
            "schema": PROVENANCE_SCHEMA,
            "live": live,
            "api": {
                **{
                    key: value
                    for key, value in self.provenance["api"].items()
                    if key != "responses"
                },
                "responses": api_rows,
            },
            "artifact": self.provenance["artifact"],
        }
        validate_canonical_provenance(value)
        payload = canonical_json(value)
        path = self.transaction_root / f"LIVE-RECHECK-{self._client.counter}.json"
        create_file(path, payload, 0o400)
        fsync_directory(self.transaction_root)
        return FinalLiveReproof(value=value, canonical_bytes=payload, path=path)


@dataclass(frozen=True)
class FinalLiveReproofSet:
    """Ordered full provenances returned by one shared four-request reproof."""

    proofs: tuple[FinalLiveReproof, ...]
    api_request_count: int


@dataclass(frozen=True)
class VerifiedArtifactSet:
    """The exact ordered nine-group artifact set in one private transaction."""

    transaction_root: Path
    artifacts: tuple[VerifiedArtifact, ...]
    _client: CurlApiClient
    _rows: tuple[dict[str, Any], ...]
    _common_pins: dict[str, Any]
    _fixed_now: int | None

    @property
    def api_request_count(self) -> int:
        return self._client.counter

    def recheck(self) -> FinalLiveReproofSet:
        """Re-prove all nine live tuples with four shared API reads."""

        self._client.now = (
            int(time.time()) if self._fixed_now is None else int(self._fixed_now)
        )
        lives, api_rows_set = prove_live_api_set(
            self._client,
            rows=self._rows,
            **self._common_pins,
        )
        proofs: list[FinalLiveReproof] = []
        for index, (verified, live, api_rows) in enumerate(
            zip(self.artifacts, lives, api_rows_set)
        ):
            invariant = {
                key: value
                for key, value in verified.provenance["live"].items()
                if key != "api_verified_at_unix"
            }
            refreshed = {
                key: value for key, value in live.items() if key != "api_verified_at_unix"
            }
            if invariant != refreshed or set(live) != set(verified.provenance["live"]):
                fail("final live set recheck differs from the initially verified set")
            value = {
                "schema": PROVENANCE_SCHEMA,
                "live": live,
                "api": {
                    **{
                        key: value
                        for key, value in verified.provenance["api"].items()
                        if key != "responses"
                    },
                    "responses": api_rows,
                },
                "artifact": verified.provenance["artifact"],
            }
            validate_canonical_provenance(
                value,
                response_labels=(
                    "workflow",
                    "run",
                    "artifact_set",
                    "protected_main",
                ),
            )
            payload = canonical_json(value)
            path = self.transaction_root / f"LIVE-SET-RECHECK-{index:02d}.json"
            create_file(path, payload, 0o400)
            proofs.append(
                FinalLiveReproof(value=value, canonical_bytes=payload, path=path)
            )
        fsync_directory(self.transaction_root)
        return FinalLiveReproofSet(
            proofs=tuple(proofs), api_request_count=self._client.counter
        )


def _validate_common_inputs(
    *,
    expected_commit: str,
    expected_run_id: int,
    expected_run_attempt: int,
    expected_version: str,
) -> None:
    if HEX_40.fullmatch(expected_commit) is None:
        fail("expected commit must be one full lowercase Git SHA")
    if (
        isinstance(expected_run_id, bool)
        or not isinstance(expected_run_id, int)
        or expected_run_id <= 0
        or isinstance(expected_run_attempt, bool)
        or not isinstance(expected_run_attempt, int)
        or expected_run_attempt <= 0
    ):
        fail("expected run and attempt must be positive integers")
    if SEMVER.fullmatch(expected_version) is None:
        fail("expected version must be strict MAJOR.MINOR.PATCH")


def _api_provenance(
    *,
    curl_sha256: str,
    ca_bundle_sha256: str,
    responses: list[dict[str, Any]],
) -> dict[str, Any]:
    return {
        "origin": API_ORIGIN,
        "anonymous": True,
        "redirects_followed": False,
        "max_age_seconds": MAX_API_AGE_SECONDS,
        "curl_sha256": curl_sha256,
        "ca_bundle_sha256": ca_bundle_sha256,
        "responses": responses,
    }


def _stage_verified_artifact(
    *,
    root: Path,
    raw_actions_zip: Path,
    expected_commit: str,
    expected_run_id: int,
    expected_run_attempt: int,
    expected_artifact_id: int,
    kind: str,
    platform: str,
    expected_version: str,
    curl_sha256: str,
    ca_bundle_sha256: str,
    live: dict[str, Any],
    api_rows: list[dict[str, Any]],
    client: CurlApiClient,
    fixed_now: int | None,
) -> VerifiedArtifact:
    root = require_private_root(root)
    staged_zip = root / "actions-artifact.zip"
    zip_sha256, zip_size = stable_copy(
        Path(raw_actions_zip),
        staged_zip,
        maximum=MAX_ACTIONS_ZIP_BYTES,
        mode=0o400,
    )
    if (
        f"sha256:{zip_sha256}" != live["artifact_digest"]
        or zip_size != live["artifact_size_in_bytes"]
    ):
        fail("raw Actions ZIP differs from the fresh exact-ID server digest or size")
    archive_sha256 = live["artifact_name"].rsplit("-", 1)[1]
    stem = (
        f"arc-pretag-{kind}-{platform}-{expected_commit}-"
        f"{expected_run_id}-{expected_run_attempt}"
    )
    archive_name = f"{stem}.tar.gz"
    outer = root / "outer"
    outer.mkdir(mode=0o700)
    checksums, archive = extract_outer(staged_zip, outer, archive_name=archive_name)
    manifest_archive_sha = parse_checksums(
        checksums,
        kind=kind,
        platform=platform,
        commit=expected_commit,
        run_id=expected_run_id,
        run_attempt=expected_run_attempt,
        archive_name=archive_name,
    )
    if manifest_archive_sha != archive_sha256 or sha256_file(archive) != archive_sha256:
        fail("inner archive digest differs from its live artifact name and SHA256SUMS")
    payload_root = root / "payload"
    metadata, metadata_raw, payloads = extract_inner(
        archive,
        payload_root,
        kind=kind,
        platform=platform,
        commit=expected_commit,
        run_id=expected_run_id,
        run_attempt=expected_run_attempt,
        version=expected_version,
    )
    provenance = {
        "schema": PROVENANCE_SCHEMA,
        "live": live,
        "api": _api_provenance(
            curl_sha256=curl_sha256,
            ca_bundle_sha256=ca_bundle_sha256,
            responses=api_rows,
        ),
        "artifact": {
            "kind": kind,
            "platform": platform,
            "version": expected_version,
            "raw_actions_zip_sha256": zip_sha256,
            "raw_actions_zip_size": zip_size,
            "archive_sha256": archive_sha256,
            "build_metadata_sha256": sha256_bytes(metadata_raw),
            "files": metadata["files"],
        },
    }
    provenance_bytes = canonical_json(provenance)
    validate_canonical_provenance(
        provenance,
        canonical_bytes=provenance_bytes,
        response_labels=tuple(row["label"] for row in api_rows),
    )
    provenance_path = root / "LIVE-PROVENANCE.json"
    create_file(provenance_path, provenance_bytes, 0o400)
    fsync_directory(root)
    return VerifiedArtifact(
        transaction_root=root,
        raw_actions_zip=staged_zip,
        payload_root=payload_root,
        payloads=payloads,
        build_metadata=metadata,
        build_metadata_path=payload_root / "BUILD-METADATA.json",
        provenance=provenance,
        provenance_path=provenance_path,
        provenance_bytes=provenance_bytes,
        _client=client,
        _pins={
            "commit": expected_commit,
            "run_id": expected_run_id,
            "run_attempt": expected_run_attempt,
            "artifact_id": expected_artifact_id,
            "kind": kind,
            "platform": platform,
        },
        _fixed_now=fixed_now,
    )


def final_live_reproof(
    *,
    initial_provenance_bytes: bytes,
    expected_commit: str,
    expected_run_id: int,
    expected_run_attempt: int,
    expected_artifact_id: int,
    kind: str,
    platform: str,
    expected_version: str,
    curl: Path,
    curl_sha256: str,
    ca_bundle: Path,
    ca_bundle_sha256: str,
    now: int | None = None,
) -> FinalLiveReproof:
    """Fresh API-only reproof of one previously verified complete provenance.

    The raw ZIP is deliberately not reopened.  The returned object is a second
    full canonical ``arc.protected-pretag-artifact.v1`` provenance with the
    identical artifact section and live tuple, but fresh API bodies, request
    IDs, response timestamps, and minimum live timestamp.
    """

    if not isinstance(initial_provenance_bytes, bytes) or not (
        0 < len(initial_provenance_bytes) <= MAX_API_BYTES
    ):
        fail("initial protected artifact provenance is empty or oversized")
    try:
        initial = json.loads(initial_provenance_bytes)
    except (UnicodeDecodeError, json.JSONDecodeError):
        fail("initial protected artifact provenance is not valid JSON")
    if (
        not isinstance(initial, dict)
        or initial_provenance_bytes != canonical_json(initial)
        or set(initial) != {"schema", "live", "api", "artifact"}
        or initial.get("schema") != PROVENANCE_SCHEMA
        or not isinstance(initial.get("live"), dict)
        or not isinstance(initial.get("api"), dict)
        or not isinstance(initial.get("artifact"), dict)
    ):
        fail("initial protected artifact provenance is not one canonical full proof")
    initial = validate_canonical_provenance(
        initial, canonical_bytes=initial_provenance_bytes
    )
    live = initial["live"]
    artifact = initial["artifact"]
    if (
        live.get("repository") != REPOSITORY
        or live.get("protected_branch") != PROTECTED_BRANCH
        or live.get("commit") != expected_commit
        or live.get("run_id") != expected_run_id
        or live.get("run_attempt") != expected_run_attempt
        or live.get("artifact_id") != expected_artifact_id
        or artifact.get("kind") != kind
        or artifact.get("platform") != platform
        or artifact.get("version") != expected_version
        or initial["api"].get("curl_sha256") != curl_sha256
        or initial["api"].get("ca_bundle_sha256") != ca_bundle_sha256
    ):
        fail("initial provenance differs from the exact requested reproof tuple")
    verified_now = int(time.time()) if now is None else int(now)
    root = Path(tempfile.mkdtemp(prefix="arc-protected-artifact-reproof."))
    root.chmod(0o700)
    try:
        client = CurlApiClient(
            Path(curl),
            curl_sha256,
            Path(ca_bundle),
            ca_bundle_sha256,
            root,
            now=verified_now,
        )
        pins = {
            "commit": expected_commit,
            "run_id": expected_run_id,
            "run_attempt": expected_run_attempt,
            "artifact_id": expected_artifact_id,
            "kind": kind,
            "platform": platform,
        }
        refreshed_live, api_rows = prove_live_api(client, **pins)
        invariant = {key: value for key, value in live.items() if key != "api_verified_at_unix"}
        refreshed_invariant = {
            key: value
            for key, value in refreshed_live.items()
            if key != "api_verified_at_unix"
        }
        if invariant != refreshed_invariant or set(live) != set(refreshed_live):
            fail("final live reproof tuple differs from the initial provenance")
        value = {
            "schema": PROVENANCE_SCHEMA,
            "live": refreshed_live,
            "api": {
                **{key: value for key, value in initial["api"].items() if key != "responses"},
                "responses": api_rows,
            },
            "artifact": artifact,
        }
        validate_canonical_provenance(value)
        return FinalLiveReproof(value=value, canonical_bytes=canonical_json(value))
    finally:
        shutil.rmtree(root, ignore_errors=True)


@contextmanager
def pretag_actions_proof(
    *,
    raw_actions_zip: Path,
    expected_commit: str,
    expected_run_id: int,
    expected_run_attempt: int,
    expected_artifact_id: int,
    kind: str,
    platform: str,
    expected_version: str,
    curl: Path,
    curl_sha256: str,
    ca_bundle: Path,
    ca_bundle_sha256: str,
    now: int | None = None,
) -> Iterator[VerifiedArtifact]:
    """Yield stable private payloads only while their live proof is in scope."""

    _validate_common_inputs(
        expected_commit=expected_commit,
        expected_run_id=expected_run_id,
        expected_run_attempt=expected_run_attempt,
        expected_version=expected_version,
    )
    if (
        isinstance(expected_artifact_id, bool)
        or not isinstance(expected_artifact_id, int)
        or expected_artifact_id <= 0
    ):
        fail("expected artifact ID must be a positive integer")
    expected_files(kind, platform)
    verified_now = int(time.time()) if now is None else int(now)
    root = Path(tempfile.mkdtemp(prefix="arc-protected-artifact."))
    root.chmod(0o700)
    try:
        root = require_private_root(root)
        client = CurlApiClient(
            Path(curl),
            curl_sha256,
            Path(ca_bundle),
            ca_bundle_sha256,
            root,
            now=verified_now,
        )
        live_pins = {
            "commit": expected_commit,
            "run_id": expected_run_id,
            "run_attempt": expected_run_attempt,
            "artifact_id": expected_artifact_id,
            "kind": kind,
            "platform": platform,
        }
        live, api_rows = prove_live_api(
            client,
            **live_pins,
        )
        yield _stage_verified_artifact(
            root=root,
            raw_actions_zip=Path(raw_actions_zip),
            expected_commit=expected_commit,
            expected_run_id=expected_run_id,
            expected_run_attempt=expected_run_attempt,
            expected_artifact_id=expected_artifact_id,
            kind=kind,
            platform=platform,
            expected_version=expected_version,
            curl_sha256=curl_sha256,
            ca_bundle_sha256=ca_bundle_sha256,
            live=live,
            api_rows=api_rows,
            client=client,
            fixed_now=now,
        )
    finally:
        # The directory is created by this invocation and contains only files
        # created through O_EXCL under its mode-0700 boundary.
        shutil.rmtree(root, ignore_errors=True)


def _normalize_materialization_rows(
    rows: Sequence[dict[str, Any]],
) -> tuple[dict[str, Any], ...]:
    if isinstance(rows, (str, bytes, bytearray)) or len(rows) != len(PRETAG_GROUPS):
        fail("protected artifact materialization requires exactly nine canonical rows")
    normalized: list[dict[str, Any]] = []
    live_rows: list[dict[str, Any]] = []
    for index, (row, expected_group) in enumerate(zip(rows, PRETAG_GROUPS)):
        row = exact_object(
            row,
            {"raw_actions_zip", "expected_artifact_id", "kind", "platform"},
            f"protected artifact materialization row {index}",
        )
        if (row["kind"], row["platform"]) != expected_group:
            fail("protected artifact materialization rows are not in canonical group order")
        try:
            raw_actions_zip = Path(row["raw_actions_zip"])
        except TypeError:
            fail("protected artifact materialization raw ZIP path is invalid")
        normalized_row = {
            "raw_actions_zip": raw_actions_zip,
            "expected_artifact_id": row["expected_artifact_id"],
            "kind": row["kind"],
            "platform": row["platform"],
        }
        normalized.append(normalized_row)
        live_rows.append(
            {
                "artifact_id": row["expected_artifact_id"],
                "kind": row["kind"],
                "platform": row["platform"],
            }
        )
    _normalize_live_set_rows(live_rows)
    return tuple(normalized)


@contextmanager
def pretag_actions_set_proof(
    *,
    rows: Sequence[dict[str, Any]],
    expected_commit: str,
    expected_run_id: int,
    expected_run_attempt: int,
    expected_version: str,
    curl: Path,
    curl_sha256: str,
    ca_bundle: Path,
    ca_bundle_sha256: str,
    now: int | None = None,
) -> Iterator[VerifiedArtifactSet]:
    """Privately materialize the exact nine groups with four shared API reads.

    Each row has exactly ``raw_actions_zip``, ``expected_artifact_id``, ``kind``,
    and ``platform`` and must follow :data:`PRETAG_GROUPS` order.
    """

    _validate_common_inputs(
        expected_commit=expected_commit,
        expected_run_id=expected_run_id,
        expected_run_attempt=expected_run_attempt,
        expected_version=expected_version,
    )
    normalized = _normalize_materialization_rows(rows)
    live_rows = tuple(
        {
            "artifact_id": row["expected_artifact_id"],
            "kind": row["kind"],
            "platform": row["platform"],
        }
        for row in normalized
    )
    verified_now = int(time.time()) if now is None else int(now)
    root = Path(tempfile.mkdtemp(prefix="arc-protected-artifact-set."))
    root.chmod(0o700)
    try:
        root = require_private_root(root)
        client = CurlApiClient(
            Path(curl),
            curl_sha256,
            Path(ca_bundle),
            ca_bundle_sha256,
            root,
            now=verified_now,
        )
        common_pins = {
            "commit": expected_commit,
            "run_id": expected_run_id,
            "run_attempt": expected_run_attempt,
        }
        lives, api_rows_set = prove_live_api_set(
            client, rows=live_rows, **common_pins
        )
        artifacts: list[VerifiedArtifact] = []
        for index, (row, live, api_rows) in enumerate(
            zip(normalized, lives, api_rows_set)
        ):
            group_root = root / f"{index:02d}-{row['kind']}-{row['platform']}"
            group_root.mkdir(mode=0o700)
            artifacts.append(
                _stage_verified_artifact(
                    root=group_root,
                    raw_actions_zip=row["raw_actions_zip"],
                    expected_commit=expected_commit,
                    expected_run_id=expected_run_id,
                    expected_run_attempt=expected_run_attempt,
                    expected_artifact_id=row["expected_artifact_id"],
                    kind=row["kind"],
                    platform=row["platform"],
                    expected_version=expected_version,
                    curl_sha256=curl_sha256,
                    ca_bundle_sha256=ca_bundle_sha256,
                    live=live,
                    api_rows=api_rows,
                    client=client,
                    fixed_now=now,
                )
            )
        fsync_directory(root)
        yield VerifiedArtifactSet(
            transaction_root=root,
            artifacts=tuple(artifacts),
            _client=client,
            _rows=live_rows,
            _common_pins=common_pins,
            _fixed_now=now,
        )
    finally:
        shutil.rmtree(root, ignore_errors=True)


def final_live_set_reproof(
    *,
    initial_provenance_bytes_list: Sequence[bytes],
    expected_commit: str,
    expected_run_id: int,
    expected_run_attempt: int,
    expected_artifact_ids: Sequence[int],
    expected_version: str,
    curl: Path,
    curl_sha256: str,
    ca_bundle: Path,
    ca_bundle_sha256: str,
    now: int | None = None,
) -> FinalLiveReproofSet:
    """Fresh four-request API-only reproof of an ordered nine-proof set."""

    _validate_common_inputs(
        expected_commit=expected_commit,
        expected_run_id=expected_run_id,
        expected_run_attempt=expected_run_attempt,
        expected_version=expected_version,
    )
    if (
        isinstance(initial_provenance_bytes_list, (str, bytes, bytearray))
        or len(initial_provenance_bytes_list) != len(PRETAG_GROUPS)
        or isinstance(expected_artifact_ids, (str, bytes, bytearray))
        or len(expected_artifact_ids) != len(PRETAG_GROUPS)
    ):
        fail("final live set reproof requires exactly nine ordered proofs and IDs")
    live_rows = _normalize_live_set_rows(
        tuple(
            {
                "artifact_id": artifact_id,
                "kind": kind,
                "platform": platform,
            }
            for artifact_id, (kind, platform) in zip(
                expected_artifact_ids, PRETAG_GROUPS
            )
        )
    )
    initials: list[dict[str, Any]] = []
    for index, (raw, row) in enumerate(zip(initial_provenance_bytes_list, live_rows)):
        if not isinstance(raw, bytes) or not 0 < len(raw) <= MAX_API_BYTES:
            fail("initial protected artifact set provenance is empty or oversized")
        try:
            initial = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            fail("initial protected artifact set provenance is not valid JSON")
        if (
            not isinstance(initial, dict)
            or raw != canonical_json(initial)
            or set(initial) != {"schema", "live", "api", "artifact"}
            or initial.get("schema") != PROVENANCE_SCHEMA
            or not isinstance(initial.get("live"), dict)
            or not isinstance(initial.get("api"), dict)
            or not isinstance(initial.get("artifact"), dict)
        ):
            fail("initial protected artifact set provenance is not one canonical full proof")
        initial = validate_canonical_provenance(
            initial,
            canonical_bytes=raw,
            response_labels=(
                "workflow",
                "run",
                "artifact_set",
                "protected_main",
            ),
        )
        live = initial["live"]
        artifact = initial["artifact"]
        responses = initial["api"].get("responses")
        if (
            live.get("repository") != REPOSITORY
            or live.get("protected_branch") != PROTECTED_BRANCH
            or live.get("commit") != expected_commit
            or live.get("run_id") != expected_run_id
            or live.get("run_attempt") != expected_run_attempt
            or live.get("artifact_id") != row["artifact_id"]
            or artifact.get("kind") != row["kind"]
            or artifact.get("platform") != row["platform"]
            or artifact.get("version") != expected_version
            or initial["api"].get("curl_sha256") != curl_sha256
            or initial["api"].get("ca_bundle_sha256") != ca_bundle_sha256
            or not isinstance(responses, list)
            or [item.get("label") for item in responses if isinstance(item, dict)]
            != ["workflow", "run", "artifact_set", "protected_main"]
        ):
            fail(f"initial protected artifact set row {index} differs from its exact tuple")
        initials.append(initial)
    shared_api = initials[0]["api"]
    if any(initial["api"] != shared_api for initial in initials[1:]):
        fail("initial protected artifact set does not share one exact API proof")

    verified_now = int(time.time()) if now is None else int(now)
    root = Path(tempfile.mkdtemp(prefix="arc-protected-artifact-set-reproof."))
    root.chmod(0o700)
    try:
        root = require_private_root(root)
        client = CurlApiClient(
            Path(curl),
            curl_sha256,
            Path(ca_bundle),
            ca_bundle_sha256,
            root,
            now=verified_now,
        )
        lives, api_rows_set = prove_live_api_set(
            client,
            commit=expected_commit,
            run_id=expected_run_id,
            run_attempt=expected_run_attempt,
            rows=live_rows,
        )
        proofs: list[FinalLiveReproof] = []
        for initial, live, api_rows in zip(initials, lives, api_rows_set):
            invariant = {
                key: value
                for key, value in initial["live"].items()
                if key != "api_verified_at_unix"
            }
            refreshed = {
                key: value for key, value in live.items() if key != "api_verified_at_unix"
            }
            if invariant != refreshed or set(live) != set(initial["live"]):
                fail("final live set reproof tuple differs from initial provenance")
            value = {
                "schema": PROVENANCE_SCHEMA,
                "live": live,
                "api": {
                    **{
                        key: value
                        for key, value in initial["api"].items()
                        if key != "responses"
                    },
                    "responses": api_rows,
                },
                "artifact": initial["artifact"],
            }
            payload = canonical_json(value)
            validate_canonical_provenance(
                value,
                canonical_bytes=payload,
                response_labels=(
                    "workflow",
                    "run",
                    "artifact_set",
                    "protected_main",
                ),
            )
            proofs.append(FinalLiveReproof(value=value, canonical_bytes=payload))
        return FinalLiveReproofSet(
            proofs=tuple(proofs), api_request_count=client.counter
        )
    finally:
        shutil.rmtree(root, ignore_errors=True)


# Descriptive compatibility alias for callers that prefer a verb phrase.  Both
# names enter the same mandatory live public-API transaction; neither accepts a
# local receipt or injectable trust decision.
verified_protected_pretag_artifact = pretag_actions_proof
verified_protected_pretag_artifact_set = pretag_actions_set_proof
