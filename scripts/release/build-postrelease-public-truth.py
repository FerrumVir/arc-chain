#!/usr/bin/env python3
"""Derive the public ARC status files from sealed post-release evidence.

The release source intentionally says that v0.8.0 is unpublished.  That text
must remain immutable in the tag.  After the release, fleet cutover, Pages
deployment, installer canaries, and live reward verification have all passed,
this helper creates three create-only evidence products:

* a README with only its delimited status/quickstart block replaced; and
* a machine-readable public production-status document; and
* the canonical v2 acceptance receipt from which those claims were derived.

It never mutates a checkout.  It validates the exact raw GitHub and CDN files
supplied by the operator, then performs one final read-only live verification
with the exact checked-in recovery verifier.  The output directory is
create-only.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import subprocess
import stat
import sys
import tempfile
import zipfile
from decimal import Decimal
from pathlib import Path
from typing import Any, NoReturn, Sequence


STATUS_SCHEMA = "arc.public-production-status.v1"
ACCEPTANCE_SCHEMA = "arc.post-release-acceptance.v2"
NETWORK_SCHEMA = "arc.frontend.network.v1"
REWARD_SCHEMA = "arc.recovery.reward-evidence.v3"
REPOSITORY = "FerrumVir/arc-chain"
TAG = "v0.8.0"
VERSION = "0.8.0"
CHAIN_ID = "0x415243"
PROTOCOL_VERSION_RE = re.compile(r"^3\.[0-9]+\.[0-9]+$")
RECOVERY_EPOCH = 1
VALIDATOR_SET_ID = 1
REWARD_PER_RECEIPT_BASE = 2_500_000_000
PUBLIC_CONSOLE = "https://ferrumvir.github.io/arc-chain/"
PUBLIC_EXPLORER = PUBLIC_CONSOLE + "explorer/"
BEGIN_MARKER = "<!-- ARC_PUBLIC_TRUTH_BEGIN -->"
END_MARKER = "<!-- ARC_PUBLIC_TRUTH_END -->"
MAX_INPUT_BYTES = 16 * 1024 * 1024
HASH_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
CHAIN_HASH_RE = re.compile(r"^(?:0x)?[0-9a-f]{64}$")
UTC_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")
PAGES_WORKFLOW_PATH = ".github/workflows/deploy-explorer.yml"
PUBLISHED_WORKFLOW_PATH = ".github/workflows/post-release-acceptance.yml"
PAGES_JOB_NAMES = frozenset(
    {"Verify and assemble public console", "Publish GitHub Pages"}
)
PUBLISHED_JOB_NAMES = frozenset(
    {
        "Bind exact release run, tag, and public assets",
        "Linux headless, AppImage, and real v0.7.7 migration",
        "Packaged desktop (macos-arm64)",
        "Packaged desktop (macos-x86_64)",
        "Packaged desktop (windows-x86_64)",
        "Seal canonical published-artifact receipt",
    }
)
PUBLISHED_ACCEPTANCE_RECEIPT = "POST-RELEASE-ARTIFACT-ACCEPTANCE.json"
PUBLISHED_ACCEPTANCE_SUMS = "POST-RELEASE-ARTIFACT-ACCEPTANCE.SHA256SUMS"
PUBLISHED_EVIDENCE_MANIFEST = "EVIDENCE-MANIFEST.json"
PUBLISHED_TOP_LEVEL_JSON = frozenset(
    {
        PUBLISHED_ACCEPTANCE_RECEIPT,
        PUBLISHED_EVIDENCE_MANIFEST,
        "component-artifacts.json",
        "linux-x86_64.json",
        "macos-arm64.json",
        "macos-x86_64.json",
        "release-binding.json",
        "windows-x86_64.json",
    }
)
PUBLISHED_EVIDENCE_FILES = frozenset(
    {
        "linux-x86_64/app.stderr",
        "linux-x86_64/app.stdout",
        "linux-x86_64/extract.stderr",
        "linux-x86_64/extract.stdout",
        "linux-x86_64/headless-install.stderr",
        "linux-x86_64/headless-install.stdout",
        "linux-x86_64/headless-update.stderr",
        "linux-x86_64/headless-update.stdout",
        "linux-x86_64/legacy-data-after.json",
        "linux-x86_64/legacy-data-before.json",
        "linux-x86_64/legacy-home.txt",
        "linux-x86_64/legacy-migration.stderr",
        "linux-x86_64/legacy-migration.stdout",
        "linux-x86_64/legacy-model-before.sha256",
        "linux-x86_64/legacy-source.json",
        "linux-x86_64/legacy-update.stderr",
        "linux-x86_64/legacy-update.stdout",
        "linux-x86_64/legacy-version.txt",
        "linux-x86_64/window-geometry.txt",
        "linux-x86_64/window-pid.txt",
        "linux-x86_64/window-properties.txt",
        "linux-x86_64/window.xwd",
        "macos-arm64/desktop-window.json",
        "macos-arm64/desktop.stderr",
        "macos-arm64/desktop.stdout",
        "macos-x86_64/desktop-window.json",
        "macos-x86_64/desktop.stderr",
        "macos-x86_64/desktop.stdout",
        "release/published-evidence-artifact.json",
        "release/published-evidence.zip",
        "release/release-attempt-jobs.json",
        "release/release-published.json",
        "windows-x86_64/windows-desktop-window.json",
        "windows-x86_64/windows-desktop.stderr",
        "windows-x86_64/windows-desktop.stdout",
        "windows-x86_64/windows-msi-admin.log",
    }
)
EXPECTED_PUBLISHED_ZIP_FILES = frozenset(
    set(PUBLISHED_TOP_LEVEL_JSON)
    | {PUBLISHED_ACCEPTANCE_SUMS}
    | {f"evidence/{name}" for name in PUBLISHED_EVIDENCE_FILES}
)
MAX_PUBLISHED_ZIP_BYTES = 768 * 1024 * 1024
MAX_PUBLISHED_MEMBER_BYTES = 256 * 1024 * 1024
MAX_PUBLISHED_EXPANDED_BYTES = 512 * 1024 * 1024
RECOVERY_VERIFIER_RELATIVE = Path("scripts/recovery/recovery_rollout.py")
PUBLISHED_HELPER_RELATIVE = Path("scripts/release/published-artifact-acceptance.py")

PRODUCTION_FLEET = (
    ("nyc", "149.28.32.76"),
    ("lax", "140.82.16.112"),
    ("ams", "136.244.109.1"),
    ("lhr", "104.238.171.11"),
    ("nrt", "202.182.107.41"),
    ("sgp", "149.28.153.31"),
)

EXPECTED_RELEASE_ASSETS = {
    "arc-node-linux-x86_64",
    "arc-cli-linux-x86_64",
    "arc-node-linux-arm64",
    "arc-cli-linux-arm64",
    "arc-node-macos-arm64",
    "arc-cli-macos-arm64",
    "arc-node-macos-x86_64",
    "arc-cli-macos-x86_64",
    "arc-node-windows-x86_64.exe",
    "arc-cli-windows-x86_64.exe",
    "arc-desktop-macos-arm64.app.tar.gz",
    "arc-desktop-macos-arm64.app.tar.gz.sig",
    "arc-desktop-macos-arm64.dmg",
    "arc-desktop-macos-x86_64.app.tar.gz",
    "arc-desktop-macos-x86_64.app.tar.gz.sig",
    "arc-desktop-macos-x86_64.dmg",
    "arc-desktop-windows-x86_64-setup.exe",
    "arc-desktop-windows-x86_64-setup.exe.sig",
    "arc-desktop-windows-x86_64.msi",
    "arc-desktop-linux-x86_64.AppImage",
    "arc-desktop-linux-x86_64.AppImage.sig",
    "arc-desktop-linux-x86_64.deb",
    "arc-desktop-linux-x86_64.rpm",
    "install.sh",
    "testnet-seeds.txt",
    "genesis.toml",
    "arc-legacy-maintenance-boundary.json",
    "arc-recovery-checkpoint-descriptor.json",
    "arc-cutover-policy.json",
    "latest.json",
    "SHA256SUMS",
    "SHA256SUMS.sig",
}


class TruthError(ValueError):
    """Evidence cannot support a live public status claim."""


def fail(message: str) -> NoReturn:
    raise TruthError(message)


def canonical_json(value: object) -> bytes:
    return (
        json.dumps(
            value,
            allow_nan=False,
            ensure_ascii=True,
            separators=(",", ":"),
            sort_keys=True,
        )
        + "\n"
    ).encode("utf-8")


def sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def load_bytes(path: Path, label: str, maximum: int = MAX_INPUT_BYTES) -> bytes:
    descriptor = -1
    try:
        flags = os.O_RDONLY
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        descriptor = os.open(path, flags)
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode):
            fail(f"{label} must be a non-symlink regular file")
        if before.st_size <= 0 or before.st_size > maximum:
            fail(f"{label} has an unsupported size")
        chunks: list[bytes] = []
        remaining = maximum + 1
        while remaining > 0:
            chunk = os.read(descriptor, min(1024 * 1024, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        raw = b"".join(chunks)
        after = os.fstat(descriptor)
    except OSError as error:
        fail(f"cannot read {label}: {error}")
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    stable_identity = (
        before.st_dev,
        before.st_ino,
        before.st_size,
        before.st_mtime_ns,
        before.st_ctime_ns,
    ) == (
        after.st_dev,
        after.st_ino,
        after.st_size,
        after.st_mtime_ns,
        after.st_ctime_ns,
    )
    if len(raw) != before.st_size or not stable_identity:
        fail(f"{label} changed while it was read")
    return raw


def reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            fail(f"JSON contains duplicate key {key!r}")
        result[key] = value
    return result


def reject_nonfinite_number(value: str) -> NoReturn:
    fail(f"JSON contains unsupported non-finite number {value}")


def load_json_value(path: Path, label: str) -> tuple[Any, bytes]:
    raw = load_bytes(path, label)
    try:
        value = json.loads(
            raw.decode("utf-8", errors="strict"),
            object_pairs_hook=reject_duplicate_keys,
            parse_constant=reject_nonfinite_number,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"{label} is invalid JSON: {error}")
    return value, raw


def load_json(path: Path, label: str, *, canonical: bool) -> tuple[dict[str, Any], bytes]:
    value, raw = load_json_value(path, label)
    if not isinstance(value, dict):
        fail(f"{label} must be a JSON object")
    if canonical and raw != canonical_json(value):
        fail(f"{label} is not canonical JSON")
    return value, raw


def require_keys(value: dict[str, Any], required: set[str], label: str) -> None:
    missing = required - set(value)
    if missing:
        fail(f"{label} omits required fields: {', '.join(sorted(missing))}")


def exact_keys(value: dict[str, Any], required: set[str], label: str) -> None:
    require_keys(value, required, label)
    extra = set(value) - required
    if extra:
        fail(f"{label} contains unsupported fields: {', '.join(sorted(extra))}")


def positive_int(value: object, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        fail(f"{label} must be a positive integer")
    return value


def hash_value(value: object, label: str) -> str:
    if not isinstance(value, str) or HASH_RE.fullmatch(value) is None:
        fail(f"{label} must be a lowercase SHA-256")
    return value


def chain_hash(value: object, label: str) -> str:
    if not isinstance(value, str) or CHAIN_HASH_RE.fullmatch(value) is None:
        fail(f"{label} must be a 32-byte lowercase chain hash")
    return value.removeprefix("0x")


def commit(value: object, label: str) -> str:
    if not isinstance(value, str) or COMMIT_RE.fullmatch(value) is None:
        fail(f"{label} must be a full lowercase Git commit")
    return value


def timestamp(value: object, label: str) -> str:
    if not isinstance(value, str) or UTC_RE.fullmatch(value) is None:
        fail(f"{label} must use canonical UTC seconds")
    try:
        dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError as error:
        fail(f"{label} is invalid: {error}")
    return value


def repository_tool(relative: Path, label: str) -> tuple[Path, str]:
    """Return one exact non-symlink helper from this builder's checkout."""

    script_path = Path(__file__)
    try:
        script_stat = script_path.lstat()
    except OSError as error:
        fail(f"cannot inspect public-truth builder: {error}")
    if not stat.S_ISREG(script_stat.st_mode):
        fail("public-truth builder must be a checked-in non-symlink regular file")
    root = script_path.resolve().parents[2]
    candidate = root / relative
    raw = load_bytes(candidate, label, maximum=4 * 1024 * 1024)
    return candidate, sha256(raw)


def validate_workflow(
    value: dict[str, Any], *, path: str, name: str, label: str
) -> int:
    workflow_id = positive_int(value.get("id"), f"{label}.id")
    if value.get("path") != path or value.get("name") != name or value.get("state") != "active":
        fail(f"{label} does not identify the exact active checked-in workflow")
    return workflow_id


def validate_run(
    value: dict[str, Any],
    *,
    workflow_id: int,
    path: str,
    event: str,
    head_branch: str,
    head_sha: str | None,
    label: str,
) -> tuple[int, int, str]:
    run_id = positive_int(value.get("id"), f"{label}.id")
    run_attempt = positive_int(value.get("run_attempt"), f"{label}.run_attempt")
    if value.get("workflow_id") != workflow_id:
        fail(f"{label} belongs to another workflow")
    repository = value.get("head_repository")
    actual_sha = commit(value.get("head_sha"), f"{label}.head_sha")
    if (
        not isinstance(repository, dict)
        or repository.get("full_name") != REPOSITORY
        or value.get("path") != path
        or value.get("event") != event
        or value.get("head_branch") != head_branch
        or value.get("status") != "completed"
        or value.get("conclusion") != "success"
        or (head_sha is not None and actual_sha != head_sha)
    ):
        fail(f"{label} is not the exact successful repository run")
    return run_id, run_attempt, actual_sha


def validate_jobs(
    value: object,
    *,
    expected_names: frozenset[str],
    run_id: int,
    run_attempt: int,
    head_sha: str,
    label: str,
) -> dict[str, int]:
    if isinstance(value, dict):
        exact_keys(value, {"total_count", "jobs"}, label)
        rows = value["jobs"]
        if value["total_count"] != len(expected_names):
            fail(f"{label} total_count differs from the exact job set")
    else:
        rows = value
    if not isinstance(rows, list) or len(rows) != len(expected_names):
        fail(f"{label} does not contain the exact job count")
    identities: dict[str, int] = {}
    job_ids: set[int] = set()
    for index, row in enumerate(rows):
        if not isinstance(row, dict):
            fail(f"{label} job {index} must be an object")
        name = row.get("name")
        if not isinstance(name, str) or name in identities:
            fail(f"{label} contains an invalid or duplicate job name")
        job_id = positive_int(row.get("id"), f"{label} job {name}.id")
        if job_id in job_ids:
            fail(f"{label} contains duplicate job IDs")
        if (
            row.get("run_id") != run_id
            or row.get("run_attempt") != run_attempt
            or row.get("head_sha") != head_sha
            or row.get("status") != "completed"
            or row.get("conclusion") != "success"
        ):
            fail(f"{label} job {name} is not bound to the exact successful attempt")
        identities[name] = job_id
        job_ids.add(job_id)
    if set(identities) != expected_names:
        fail(f"{label} names differ from the exact checked-in job set")
    return dict(sorted(identities.items()))


def validate_release(
    release: dict[str, Any], source_sha: str
) -> tuple[str, dict[str, dict[str, Any]]]:
    require_keys(
        release,
        {
            "id", "tag_name", "target_commitish", "draft", "prerelease",
            "immutable", "author", "assets", "html_url", "published_at",
        },
        "release API response",
    )
    if (
        release["tag_name"] != TAG
        or release["target_commitish"] != source_sha
        or release["draft"] is not False
        or release["prerelease"] is not False
        or release["immutable"] is not True
        or release.get("html_url")
        != f"https://github.com/{REPOSITORY}/releases/tag/{TAG}"
        or not isinstance(release.get("author"), dict)
        or release["author"].get("login") != "github-actions[bot]"
    ):
        fail("release API response does not prove the exact immutable v0.8.0 release")
    positive_int(release["id"], "release.id")
    published_at = timestamp(release["published_at"], "release.published_at")
    assets = release["assets"]
    if not isinstance(assets, list) or len(assets) != len(EXPECTED_RELEASE_ASSETS):
        fail("release does not contain the exact 32-asset contract")
    names: set[str] = set()
    normalized: dict[str, dict[str, Any]] = {}
    total_size = 0
    for index, asset in enumerate(assets):
        if not isinstance(asset, dict):
            fail(f"release asset {index} is not an object")
        name = asset.get("name")
        digest = asset.get("digest")
        asset_id = asset.get("id")
        if (
            not isinstance(name, str)
            or name in names
            or isinstance(asset_id, bool)
            or not isinstance(asset_id, int)
            or asset_id <= 0
            or asset.get("state") != "uploaded"
            or not isinstance(asset.get("size"), int)
            or isinstance(asset.get("size"), bool)
            or asset["size"] <= 0
            or not isinstance(digest, str)
            or not digest.startswith("sha256:")
            or HASH_RE.fullmatch(digest[7:]) is None
            or not isinstance(asset.get("uploader"), dict)
            or asset["uploader"].get("login") != "github-actions[bot]"
        ):
            fail(f"release asset {index} lacks its immutable uploaded digest contract")
        maximum_size = (
            1024 * 1024
            if name == "arc-recovery-checkpoint-descriptor.json"
            else 4 * 1024 * 1024
            if name.endswith((".sig", ".json"))
            else 2 * 1024 * 1024 * 1024
        )
        if asset["size"] > maximum_size:
            fail(f"release asset {name} exceeds its reviewed size bound")
        if asset.get("browser_download_url") != (
            f"https://github.com/{REPOSITORY}/releases/download/{TAG}/{name}"
        ):
            fail(f"release asset {name} has an unexpected public URL")
        total_size += asset["size"]
        names.add(name)
        normalized[name] = {
            "id": asset_id,
            "sha256": digest[7:],
            "size": asset["size"],
        }
    if names != EXPECTED_RELEASE_ASSETS:
        fail("release asset names differ from the exact v0.8.0 contract")
    if total_size > 12 * 1024 * 1024 * 1024:
        fail("release assets exceed the reviewed aggregate size bound")
    return published_at, dict(sorted(normalized.items()))


def validate_network(config: dict[str, Any], source_sha: str) -> dict[str, Any]:
    exact_keys(
        config,
        {"schema", "state", "network", "checkpoint", "sources", "services", "notices"},
        "frontend config",
    )
    if config["schema"] != NETWORK_SCHEMA or config["state"] != "recovered":
        fail("frontend config is not the recovered production configuration")
    if config["network"] != {"name": "ARC Testnet", "chainId": CHAIN_ID}:
        fail("frontend config does not identify the reviewed ARC production testnet")
    if not isinstance(config["notices"], list) or not config["notices"] or not all(
        isinstance(item, str) and item.strip() for item in config["notices"]
    ):
        fail("frontend config notices must be nonempty strings")
    checkpoint = config["checkpoint"]
    if not isinstance(checkpoint, dict):
        fail("frontend checkpoint is missing")
    exact_keys(
        checkpoint,
        {
            "height", "recoveryHeight", "legacyPublicMaxHeight", "blockHash",
            "stateRoot", "manifestHash", "boundaryBlockHash", "boundaryStateRoot",
            "recoveryEpoch", "validatorSetId", "protocolVersion", "recoveryDomain",
            "legacySourceId", "v3SourceId",
        },
        "frontend checkpoint",
    )
    height = positive_int(checkpoint["height"], "checkpoint.height")
    recovery_height = positive_int(checkpoint["recoveryHeight"], "checkpoint.recoveryHeight")
    legacy_max = positive_int(checkpoint["legacyPublicMaxHeight"], "checkpoint.legacyPublicMaxHeight")
    if height != 137_145 or recovery_height != height + 1 or legacy_max < recovery_height:
        fail("frontend checkpoint does not preserve the reviewed H/H+1 recovery boundary")
    if (
        not isinstance(checkpoint["protocolVersion"], str)
        or PROTOCOL_VERSION_RE.fullmatch(checkpoint["protocolVersion"]) is None
    ):
        fail("frontend checkpoint is not protocol v3")
    if (
        checkpoint["recoveryEpoch"] != RECOVERY_EPOCH
        or checkpoint["validatorSetId"] != VALIDATOR_SET_ID
        or checkpoint["legacySourceId"] != "v3-nyc"
        or checkpoint["v3SourceId"] != "v3-nyc"
    ):
        fail("frontend checkpoint differs from the reviewed recovery identity")
    for field in (
        "blockHash", "stateRoot", "manifestHash", "boundaryBlockHash",
        "boundaryStateRoot", "recoveryDomain",
    ):
        chain_hash(checkpoint[field], f"checkpoint.{field}")
    sources = config["sources"]
    if not isinstance(sources, list) or len(sources) != 12:
        fail("frontend sources must contain only the six validators and six legacy forks")
    if any(not isinstance(row, dict) for row in sources):
        fail("frontend source rows must be objects")
    identifiers = [row.get("id") for row in sources]
    endpoints = [row.get("baseUrl") for row in sources]
    if not all(isinstance(value, str) for value in identifiers + endpoints):
        fail("frontend source identities and endpoints must be strings")
    if len(set(identifiers)) != len(identifiers) or len(set(endpoints)) != len(endpoints):
        fail("frontend source identities and endpoints must be unique")
    v3 = [row for row in sources if isinstance(row, dict) and row.get("kind") == "v3"]
    forks = [row for row in sources if isinstance(row, dict) and row.get("kind") == "legacy-fork"]
    expected_v3 = [
        (f"v3-{name}", f"https://{host}") for name, host in PRODUCTION_FLEET
    ]
    actual_v3 = [(row.get("id"), row.get("baseUrl")) for row in v3]
    if actual_v3 != expected_v3:
        fail("frontend config does not expose the exact six protocol-v3 validators")
    replica_groups: set[str] = set()
    for (name, _host), row in zip(PRODUCTION_FLEET, v3, strict=True):
        exact_keys(
            row,
            {"id", "name", "region", "kind", "baseUrl", "enabled", "replicaGroup"},
            f"frontend v3 source {name}",
        )
        replica_group = row["replicaGroup"]
        if (
            row["name"] != f"ARC v3 {name.upper()}"
            or row["region"] != name.upper()
            or row["enabled"] is not True
            or not isinstance(replica_group, str)
            or not replica_group
        ):
            fail(f"frontend v3 source {name} differs from its rollout identity")
        replica_groups.add(replica_group)
    if len(replica_groups) != 1:
        fail("frontend validators do not share one rollout identity")
    if len(forks) != 6:
        fail("frontend config does not preserve exactly six legacy fork views")
    expected_forks = [
        (f"legacy-fork-{name}", f"https://{host}/legacy/{name}")
        for name, host in PRODUCTION_FLEET
    ]
    if [(row.get("id"), row.get("baseUrl")) for row in forks] != expected_forks:
        fail("frontend legacy forks differ from the exact six archived validators")
    capture_ids: set[str] = set()
    for (name, _host), row in zip(PRODUCTION_FLEET, forks, strict=True):
        exact_keys(
            row,
            {
                "id", "name", "region", "kind", "baseUrl", "enabled",
                "replicaGroup", "description", "archive",
            },
            f"frontend legacy source {name}",
        )
        archive = row.get("archive")
        if not isinstance(archive, dict):
            fail("a legacy history lacks its archive commitment")
        exact_keys(
            archive,
            {
                "schema", "readOnly", "classification", "captureId", "node",
                "rolloutManifestSha256", "archiveManifestSha256", "completeSha256",
                "bundleSha256", "inventorySha256", "bindingIndexSha256",
                "bindingSha256", "checkpointSha256", "checkpointManifestHash",
                "checkpointPayloadHash", "canonicalCheckpointHeight", "sourceHeight",
                "sourceBlockHash", "sourceStateRoot", "provenancePath",
            },
            f"frontend legacy source {name} archive",
        )
        if (
            row.get("enabled") is not True
            or row.get("name") != f"Preserved legacy fork · {name.upper()}"
            or row.get("region") != name.upper()
            or row.get("description")
            != "Explicit immutable historical fork; diagnostic only and never canonical."
            or archive.get("readOnly") is not True
            or archive.get("schema") != "arc.legacy-archive.source.v1"
            or archive.get("classification") != "valid_noncanonical_fork"
            or archive.get("node") != name
            or archive.get("provenancePath") != "/provenance"
            or archive.get("canonicalCheckpointHeight") != height
            or isinstance(archive.get("sourceHeight"), bool)
            or not isinstance(archive.get("sourceHeight"), int)
            or archive["sourceHeight"] < 0
        ):
            fail("a legacy history is not explicit, read-only, and noncanonical")
        capture_id = hash_value(archive["captureId"], f"legacy source {name} captureId")
        if row.get("replicaGroup") != f"legacy-capture-{capture_id}":
            fail(f"legacy source {name} is not bound to its capture identity")
        capture_ids.add(capture_id)
        for field in (
            "rolloutManifestSha256", "archiveManifestSha256", "completeSha256",
            "bundleSha256", "inventorySha256", "bindingIndexSha256", "bindingSha256",
            "checkpointSha256", "checkpointManifestHash", "checkpointPayloadHash",
            "sourceBlockHash", "sourceStateRoot",
        ):
            chain_hash(archive[field], f"legacy source {name} archive.{field}")
        if chain_hash(archive["checkpointManifestHash"], f"legacy source {name} checkpoint") != chain_hash(
            checkpoint["manifestHash"], "checkpoint.manifestHash"
        ):
            fail(f"legacy source {name} refers to another checkpoint manifest")
    if len(capture_ids) != 1:
        fail("frontend legacy forks do not share one sealed capture")
    services = config["services"]
    if not isinstance(services, dict):
        fail("frontend services must be an object")
    exact_keys(services, {"maintenanceInterlock"}, "frontend services")
    interlock = services.get("maintenanceInterlock")
    if not isinstance(interlock, dict) or interlock.get("sourceMainCommit") != source_sha:
        fail("frontend maintenance interlock is not bound to the release source")
    exact_keys(
        interlock,
        {
            "schema", "path", "sourceMainCommit", "observedCutoffHeight",
            "sourceSetSha256", "boundarySha256", "toolSha256",
            "requiredHealthyReplicas", "maxStalenessSeconds",
        },
        "frontend maintenance interlock",
    )
    if (
        interlock.get("schema") != "arc.frontend.maintenance-interlock.v1"
        or interlock.get("path") != "/maintenance/status"
        or interlock.get("requiredHealthyReplicas") != 6
        or interlock.get("maxStalenessSeconds") != 90
        or isinstance(interlock.get("observedCutoffHeight"), bool)
        or not isinstance(interlock.get("observedCutoffHeight"), int)
        or interlock["observedCutoffHeight"] < 1
    ):
        fail("frontend live gate does not require all six validators")
    for field in ("sourceSetSha256", "boundarySha256", "toolSha256"):
        hash_value(interlock[field], f"frontend maintenance interlock.{field}")
    return checkpoint


def validate_reward(
    reward: dict[str, Any], manifest: dict[str, Any], source_sha: str
) -> tuple[str, int, int]:
    exact_keys(
        reward,
        {"schema", "rollout_sha256", "earnings_baseline", "receipts", "canonical_cutoff"},
        "reward evidence",
    )
    if reward["schema"] != REWARD_SCHEMA:
        fail("reward evidence schema is unsupported")
    rollout_sha = hash_value(reward["rollout_sha256"], "reward rollout SHA-256")
    if sha256(canonical_json(manifest)) != rollout_sha:
        fail("reward evidence is not bound to the supplied rollout manifest")
    if manifest.get("mode") != "production":
        fail("rollout manifest is not production mode")
    provenance = manifest.get("provenance")
    if not isinstance(provenance, dict) or provenance.get("source_main_commit") != source_sha:
        fail("rollout manifest is not bound to the release source")
    checks = manifest.get("checks")
    policy = checks.get("reward") if isinstance(checks, dict) else None
    if not isinstance(policy, dict) or policy.get("mode") != "receipt":
        fail("rollout manifest does not require receipt-mode rewards")
    reward_base = positive_int(policy.get("expected_reward_base"), "expected reward base")
    if reward_base != REWARD_PER_RECEIPT_BASE:
        fail("rollout manifest reward differs from the reviewed 2.5 ARC contract")
    expected_worker = policy.get("expected_worker")
    if not isinstance(expected_worker, str) or CHAIN_HASH_RE.fullmatch(expected_worker) is None:
        fail("rollout manifest has no exact expected canary worker")
    expected_worker = "0x" + expected_worker.removeprefix("0x")
    receipts = reward["receipts"]
    if not isinstance(receipts, list) or len(receipts) != 2:
        fail("reward evidence must contain exactly two canary receipts")
    tx_hashes: set[str] = set()
    job_ids: set[str] = set()
    for index, row in enumerate(receipts):
        if not isinstance(row, dict) or set(row) != {"tx_hash", "job_id", "worker"}:
            fail(f"reward receipt {index} has an unsupported shape")
        worker = "0x" + chain_hash(row["worker"], f"reward receipt {index} worker")
        tx_hashes.add(chain_hash(row["tx_hash"], f"reward receipt {index} tx_hash"))
        job_ids.add(chain_hash(row["job_id"], f"reward receipt {index} job_id"))
        if worker != expected_worker:
            fail("reward receipt belongs to a worker other than the accepted macOS canary")
    if len(tx_hashes) != 2 or len(job_ids) != 2:
        fail("reward canary receipt transaction and job identities must be distinct")
    baseline = reward["earnings_baseline"]
    if not isinstance(baseline, dict):
        fail("reward evidence earnings baseline must be an object")
    exact_keys(
        baseline,
        {"worker", "confirmed_receipt_count", "confirmed_gross_earnings_base", "confirmed_receipts"},
        "reward evidence earnings baseline",
    )
    if "0x" + chain_hash(baseline["worker"], "reward baseline worker") != expected_worker:
        fail("reward evidence baseline belongs to another worker")
    baseline_count = baseline["confirmed_receipt_count"]
    baseline_gross = baseline["confirmed_gross_earnings_base"]
    if (
        isinstance(baseline_count, bool)
        or not isinstance(baseline_count, int)
        or baseline_count < 0
        or isinstance(baseline_gross, bool)
        or not isinstance(baseline_gross, int)
        or baseline_gross < 0
        or not isinstance(baseline["confirmed_receipts"], list)
        or len(baseline["confirmed_receipts"]) != baseline_count
    ):
        fail("reward evidence baseline counters are invalid")
    cutoff = reward["canonical_cutoff"]
    if not isinstance(cutoff, dict):
        fail("reward evidence canonical cutoff must be an object")
    exact_keys(cutoff, {"block_height", "block_hash", "index"}, "reward evidence canonical cutoff")
    positive_int(cutoff["block_height"], "reward evidence canonical cutoff.block_height")
    if isinstance(cutoff["index"], bool) or not isinstance(cutoff["index"], int) or cutoff["index"] < 0:
        fail("reward evidence canonical cutoff.index must be a non-negative integer")
    chain_hash(cutoff["block_hash"], "reward evidence canonical cutoff.block_hash")
    return expected_worker, reward_base, len(receipts)


def hash_regular_file(
    path: Path, label: str, maximum: int, *, allow_empty: bool = False
) -> tuple[int, str]:
    descriptor = -1
    try:
        flags = os.O_RDONLY
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        descriptor = os.open(path, flags)
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode):
            fail(f"{label} must be a non-symlink regular file")
        if before.st_size > maximum or (before.st_size == 0 and not allow_empty):
            fail(f"{label} has an unsupported size")
        digest = hashlib.sha256()
        total = 0
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            total += len(chunk)
            if total > maximum:
                fail(f"{label} exceeds its size limit")
            digest.update(chunk)
        after = os.fstat(descriptor)
    except OSError as error:
        fail(f"cannot read {label}: {error}")
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    if total != before.st_size or (
        before.st_dev,
        before.st_ino,
        before.st_size,
        before.st_mtime_ns,
        before.st_ctime_ns,
    ) != (
        after.st_dev,
        after.st_ino,
        after.st_size,
        after.st_mtime_ns,
        after.st_ctime_ns,
    ):
        fail(f"{label} changed while it was read")
    return total, digest.hexdigest()


def parse_site_sha256sums(raw: bytes) -> dict[str, str]:
    try:
        text = raw.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        fail(f"deployed SHA256SUMS is not UTF-8: {error}")
    if not text.endswith("\n"):
        fail("deployed SHA256SUMS must end with one complete record")
    result: dict[str, str] = {}
    for index, line in enumerate(text.splitlines(), 1):
        match = re.fullmatch(r"([0-9a-f]{64})  (\./[^\r\n]+)", line)
        if match is None:
            fail(f"deployed SHA256SUMS line {index} is malformed")
        digest, name = match.groups()
        pure = Path(name.removeprefix("./"))
        if (
            pure.is_absolute()
            or not pure.parts
            or any(part in {"", ".", ".."} for part in pure.parts)
            or "\\" in name
            or name in result
        ):
            fail("deployed SHA256SUMS contains an unsafe or duplicate path")
        result[name] = digest
    return result


def validate_pages(
    args: argparse.Namespace, config_raw: bytes
) -> tuple[dict[str, Any], dict[str, str]]:
    workflow, workflow_raw = load_json(args.pages_workflow, "Pages workflow", canonical=False)
    workflow_id = validate_workflow(
        workflow,
        path=PAGES_WORKFLOW_PATH,
        name="Deploy ARC public console",
        label="Pages workflow",
    )
    run, run_raw = load_json(args.pages_run, "Pages run", canonical=False)
    run_id, run_attempt, frontend_sha = validate_run(
        run,
        workflow_id=workflow_id,
        path=PAGES_WORKFLOW_PATH,
        event="push",
        head_branch="main",
        head_sha=None,
        label="Pages run",
    )
    jobs_value, jobs_raw = load_json_value(args.pages_jobs, "Pages attempt jobs")
    jobs = validate_jobs(
        jobs_value,
        expected_names=PAGES_JOB_NAMES,
        run_id=run_id,
        run_attempt=run_attempt,
        head_sha=frontend_sha,
        label="Pages attempt jobs",
    )
    pages_api, pages_api_raw = load_json(args.pages_api, "Pages API document", canonical=False)
    if (
        pages_api.get("build_type") != "workflow"
        or pages_api.get("html_url") != PUBLIC_CONSOLE
    ):
        fail("Pages API document does not identify the exact workflow-backed public console")

    deployments, deployments_raw = load_json_value(args.pages_deployments, "Pages deployments")
    if not isinstance(deployments, list) or len(deployments) != 1:
        fail("Pages deployment evidence must contain exactly one matching deployment")
    deployment = deployments[0]
    if not isinstance(deployment, dict):
        fail("Pages deployment must be an object")
    deployment_id = positive_int(deployment.get("id"), "Pages deployment.id")
    if (
        deployment.get("sha") != frontend_sha
        or deployment.get("ref") != "main"
        or deployment.get("environment") != "github-pages"
        or deployment.get("task") != "deploy"
    ):
        fail("Pages deployment is not bound to the accepted config commit")

    statuses, statuses_raw = load_json_value(args.pages_statuses, "Pages deployment statuses")
    if not isinstance(statuses, list) or not statuses:
        fail("Pages deployment has no status evidence")
    status_ids: set[int] = set()
    successful: list[dict[str, Any]] = []
    for index, row in enumerate(statuses):
        if not isinstance(row, dict):
            fail(f"Pages deployment status {index} must be an object")
        status_id = positive_int(row.get("id"), f"Pages deployment status {index}.id")
        if status_id in status_ids:
            fail("Pages deployment statuses contain duplicate IDs")
        status_ids.add(status_id)
        if row.get("state") == "success":
            successful.append(row)
    if (
        len(successful) != 1
        or statuses[0] is not successful[0]
        or successful[0].get("environment") != "github-pages"
        or str(successful[0].get("environment_url", "")).rstrip("/")
        != PUBLIC_CONSOLE.rstrip("/")
    ):
        fail("latest exact Pages deployment status is not the unique public success")

    deployed_commit_raw = load_bytes(args.deployed_commit, "deployed commit", maximum=1024)
    if deployed_commit_raw != f"{frontend_sha}\n".encode("ascii"):
        fail("deployed-commit.txt does not contain the exact Pages commit")
    deployed_sums_raw = load_bytes(
        args.deployed_sha256sums, "deployed SHA256SUMS", maximum=1024 * 1024
    )
    sums = parse_site_sha256sums(deployed_sums_raw)
    expected_cdn = {
        "./shared/frontend/arc-network.json": sha256(config_raw),
        "./deployed-commit.txt": sha256(deployed_commit_raw),
    }
    for name, digest in expected_cdn.items():
        if sums.get(name) != digest:
            fail(f"deployed SHA256SUMS does not bind the CDN bytes for {name}")
    return (
        {
            "acceptedConfigCommit": frontend_sha,
            "deploymentId": deployment_id,
            "jobIds": jobs,
            "runAttempt": run_attempt,
            "runId": run_id,
            "statusId": successful[0]["id"],
            "url": PUBLIC_CONSOLE,
            "workflowId": workflow_id,
        },
        {
            "configSha256": sha256(config_raw),
            "deployedCommitSha256": sha256(deployed_commit_raw),
            "deploymentsSha256": sha256(deployments_raw),
            "jobsSha256": sha256(jobs_raw),
            "pagesApiSha256": sha256(pages_api_raw),
            "runSha256": sha256(run_raw),
            "sha256sumsSha256": sha256(deployed_sums_raw),
            "statusesSha256": sha256(statuses_raw),
            "workflowSha256": sha256(workflow_raw),
        },
    )


def extract_published_acceptance(archive: Path, target: Path) -> tuple[int, str]:
    archive_size, archive_sha = hash_regular_file(
        archive, "published-acceptance Actions ZIP", MAX_PUBLISHED_ZIP_BYTES
    )
    try:
        with zipfile.ZipFile(archive, "r") as handle:
            entries = handle.infolist()
            names = [entry.filename for entry in entries]
            if len(names) != len(set(names)) or set(names) != EXPECTED_PUBLISHED_ZIP_FILES:
                fail("published-acceptance ZIP does not contain the exact canonical file set")
            expanded = 0
            for entry in entries:
                pure = Path(entry.filename)
                mode_type = (entry.external_attr >> 16) & 0o170000
                if (
                    pure.is_absolute()
                    or any(part in {"", ".", ".."} for part in pure.parts)
                    or "\\" in entry.filename
                    or entry.is_dir()
                    or entry.flag_bits & 0x1
                    or mode_type not in (0, 0o100000)
                    or (entry.file_size == 0 and entry.filename in PUBLISHED_TOP_LEVEL_JSON)
                    or entry.file_size > MAX_PUBLISHED_MEMBER_BYTES
                ):
                    fail("published-acceptance ZIP contains an unsafe member")
                expanded += entry.file_size
                if expanded > MAX_PUBLISHED_EXPANDED_BYTES:
                    fail("published-acceptance ZIP expands beyond its reviewed bound")
                destination = target.joinpath(*pure.parts)
                destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                with handle.open(entry, "r") as source, destination.open("xb") as output:
                    copied = 0
                    while True:
                        chunk = source.read(1024 * 1024)
                        if not chunk:
                            break
                        copied += len(chunk)
                        if copied > entry.file_size:
                            fail("published-acceptance ZIP member exceeds its declared size")
                        output.write(chunk)
                    output.flush()
                    os.fsync(output.fileno())
                if copied != entry.file_size:
                    fail("published-acceptance ZIP member is truncated")
                os.chmod(destination, 0o400)
    except (zipfile.BadZipFile, RuntimeError, OSError) as error:
        fail(f"cannot extract published-acceptance ZIP: {error}")
    return archive_size, archive_sha


def validate_artifact_sha256sums(root: Path) -> dict[str, str]:
    raw = load_bytes(root / PUBLISHED_ACCEPTANCE_SUMS, "published-acceptance SHA256SUMS")
    try:
        text = raw.decode("ascii", errors="strict")
    except UnicodeDecodeError as error:
        fail(f"published-acceptance SHA256SUMS is not ASCII: {error}")
    if not text.endswith("\n"):
        fail("published-acceptance SHA256SUMS is truncated")
    hashes: dict[str, str] = {}
    for line in text.splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  (\./[^\r\n]+)", line)
        if match is None:
            fail("published-acceptance SHA256SUMS contains a malformed record")
        digest, relative = match.groups()
        name = relative[2:]
        if name in hashes or name not in EXPECTED_PUBLISHED_ZIP_FILES:
            fail("published-acceptance SHA256SUMS contains an unknown or duplicate path")
        hashes[name] = digest
    expected = set(EXPECTED_PUBLISHED_ZIP_FILES) - {PUBLISHED_ACCEPTANCE_SUMS}
    if set(hashes) != expected:
        fail("published-acceptance SHA256SUMS does not cover every canonical member")
    for name, expected_sha in hashes.items():
        _size, actual_sha = hash_regular_file(
            root / name,
            f"published member {name}",
            MAX_PUBLISHED_MEMBER_BYTES,
            allow_empty=True,
        )
        if actual_sha != expected_sha:
            fail(f"published-acceptance member {name} differs from SHA256SUMS")
    return dict(sorted(hashes.items()))


def run_checked_helper(command: Sequence[str], label: str) -> tuple[str, bytes]:
    helper, helper_sha = repository_tool(PUBLISHED_HELPER_RELATIVE, "published acceptance helper")
    result = subprocess.run(
        [sys.executable, "-I", str(helper), *command],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=180,
    )
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", errors="replace").strip()[-1000:]
        fail(f"{label} failed closed: {detail or f'exit {result.returncode}'}")
    _helper_after, helper_after_sha = repository_tool(
        PUBLISHED_HELPER_RELATIVE, "published acceptance helper"
    )
    if helper_after_sha != helper_sha:
        fail("published acceptance helper changed while it was invoked")
    return helper_sha, result.stdout


def validate_published_acceptance(
    args: argparse.Namespace, temporary_root: Path
) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
    workflow, workflow_raw = load_json(
        args.published_workflow, "published-acceptance workflow", canonical=False
    )
    workflow_id = validate_workflow(
        workflow,
        path=PUBLISHED_WORKFLOW_PATH,
        name="Published artifact acceptance",
        label="published-acceptance workflow",
    )
    run, run_raw = load_json(args.published_run, "published-acceptance run", canonical=False)
    run_id, run_attempt, source_sha = validate_run(
        run,
        workflow_id=workflow_id,
        path=PUBLISHED_WORKFLOW_PATH,
        event="workflow_dispatch",
        head_branch=TAG,
        head_sha=None,
        label="published-acceptance run",
    )
    jobs_value, jobs_raw = load_json_value(
        args.published_jobs, "published-acceptance attempt jobs"
    )
    job_ids = validate_jobs(
        jobs_value,
        expected_names=PUBLISHED_JOB_NAMES,
        run_id=run_id,
        run_attempt=run_attempt,
        head_sha=source_sha,
        label="published-acceptance attempt jobs",
    )
    metadata, metadata_raw = load_json(
        args.published_artifact_metadata,
        "published-acceptance artifact metadata",
        canonical=False,
    )
    artifact_id = positive_int(metadata.get("id"), "published-acceptance artifact.id")
    artifact_size = positive_int(
        metadata.get("size_in_bytes"), "published-acceptance artifact.size_in_bytes"
    )
    artifact_digest = metadata.get("digest")
    artifact_name = (
        f"arc-published-artifact-acceptance-{TAG}-{source_sha}-{run_id}-"
        f"attempt-{run_attempt}"
    )
    workflow_run = metadata.get("workflow_run")
    if (
        artifact_size > MAX_PUBLISHED_ZIP_BYTES
        or metadata.get("name") != artifact_name
        or metadata.get("expired") is not False
        or not isinstance(artifact_digest, str)
        or re.fullmatch(r"sha256:[0-9a-f]{64}", artifact_digest) is None
        or not isinstance(workflow_run, dict)
        or workflow_run.get("id") != run_id
        or workflow_run.get("head_sha") != source_sha
    ):
        fail("published-acceptance artifact metadata is not bound to the exact run")

    extracted = temporary_root / "published-acceptance"
    extracted.mkdir(mode=0o700)
    zip_size, zip_sha = extract_published_acceptance(args.published_artifact_zip, extracted)
    if artifact_size != zip_size or artifact_digest != f"sha256:{zip_sha}":
        fail("published-acceptance ZIP bytes differ from artifact metadata")
    member_hashes = validate_artifact_sha256sums(extracted)
    for name in PUBLISHED_TOP_LEVEL_JSON:
        load_json(extracted / name, f"canonical published member {name}", canonical=True)

    receipt, receipt_raw = load_json(
        extracted / PUBLISHED_ACCEPTANCE_RECEIPT,
        "canonical published-acceptance receipt",
        canonical=True,
    )
    if (
        receipt.get("schema") != "arc.published-artifact-acceptance.v1"
        or receipt.get("repository") != REPOSITORY
        or receipt.get("tag") != TAG
        or receipt.get("commit") != source_sha
        or receipt.get("acceptance_run_id") != run_id
        or receipt.get("acceptance_run_attempt") != run_attempt
        or receipt.get("verified_platforms")
        != ["linux-x86_64", "macos-arm64", "macos-x86_64", "windows-x86_64"]
        or receipt.get("evidence_file_count") != len(PUBLISHED_EVIDENCE_FILES)
    ):
        fail("canonical published-acceptance receipt has the wrong exact identity")
    component_hashes = receipt.get("component_receipt_sha256")
    if not isinstance(component_hashes, dict) or set(component_hashes) != {
        "linux-x86_64", "macos-arm64", "macos-x86_64", "windows-x86_64"
    }:
        fail("canonical published-acceptance receipt has an invalid component hash set")
    for platform, digest in component_hashes.items():
        if hash_value(digest, f"published component {platform} hash") != member_hashes[f"{platform}.json"]:
            fail(f"published component {platform} hash is not bound to the ZIP member")
    if receipt.get("evidence_manifest_sha256") != member_hashes[PUBLISHED_EVIDENCE_MANIFEST]:
        fail("published evidence manifest hash is not bound to the ZIP member")

    components = temporary_root / "components"
    components.mkdir(mode=0o700)
    for platform in component_hashes:
        os.link(extracted / f"{platform}.json", components / f"{platform}.json")
    rebuilt = temporary_root / "rebuilt-published-acceptance.json"
    helper_sha, _helper_stdout = run_checked_helper(
        (
            "aggregate",
            "--binding", str(extracted / "release-binding.json"),
            "--component-artifacts", str(extracted / "component-artifacts.json"),
            "--components", str(components),
            "--evidence-manifest", str(extracted / PUBLISHED_EVIDENCE_MANIFEST),
            "--evidence-root", str(extracted / "evidence"),
            "--acceptance-run-id", str(run_id),
            "--acceptance-run-attempt", str(run_attempt),
            "--output", str(rebuilt),
        ),
        "exact published-acceptance helper rebuild",
    )
    rebuilt_raw = load_bytes(rebuilt, "rebuilt published-acceptance receipt")
    if rebuilt_raw != receipt_raw:
        fail("published-acceptance canonical receipt differs from the exact helper rebuild")

    binding, _ = load_json(
        extracted / "release-binding.json", "published release binding", canonical=True
    )
    linux, _ = load_json(
        extracted / "linux-x86_64.json", "published Linux component", canonical=True
    )
    linux_assets = linux.get("assets")
    if not isinstance(linux_assets, dict) or "install.sh" not in linux_assets:
        fail("published Linux component does not contain the validated installer")
    installer = linux_assets["install.sh"]
    if not isinstance(installer, dict):
        fail("published Linux component installer identity must be an object")
    installer_sha = hash_value(installer.get("sha256"), "published Linux installer SHA-256")
    positive_int(installer.get("id"), "published Linux installer asset ID")
    positive_int(installer.get("size"), "published Linux installer size")
    return (
        {
            "artifactDigest": artifact_digest,
            "artifactId": artifact_id,
            "artifactName": artifact_name,
            "canonicalReceiptSha256": sha256(receipt_raw),
            "componentReceiptSha256": dict(sorted(component_hashes.items())),
            "helperPath": PUBLISHED_HELPER_RELATIVE.as_posix(),
            "helperSha256": helper_sha,
            "jobIds": job_ids,
            "releaseId": positive_int(
                receipt.get("release_id"), "canonical published receipt release ID"
            ),
            "releaseRunAttempt": positive_int(
                receipt.get("release_run_attempt"),
                "canonical published receipt release run attempt",
            ),
            "releaseRunId": positive_int(
                receipt.get("release_run_id"),
                "canonical published receipt release run ID",
            ),
            "runAttempt": run_attempt,
            "runId": run_id,
            "workflowId": workflow_id,
        },
        {
            "artifactMetadataSha256": sha256(metadata_raw),
            "artifactZipSha256": zip_sha,
            "jobsSha256": sha256(jobs_raw),
            "runSha256": sha256(run_raw),
            "workflowSha256": sha256(workflow_raw),
        },
        {
            "binding": binding,
            "installerSha256": installer_sha,
            "linuxComponent": linux,
            "sourceSha": source_sha,
        },
    )


def run_recovery_verify(
    manifest_path: Path,
    reward_path: Path,
    manifest_raw: bytes,
    reward_raw: bytes,
    temporary_root: Path,
) -> dict[str, str]:
    verifier, verifier_sha = repository_tool(
        RECOVERY_VERIFIER_RELATIVE, "recovery rollout verifier"
    )
    stdout_path = temporary_root / "recovery-verify.stdout"
    stderr_path = temporary_root / "recovery-verify.stderr"
    with stdout_path.open("xb") as stdout, stderr_path.open("xb") as stderr:
        result = subprocess.run(
            [
                sys.executable,
                "-I",
                str(verifier),
                "verify",
                "--manifest",
                str(manifest_path),
                "--reward-evidence",
                str(reward_path),
            ],
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
            check=False,
            timeout=30 * 60,
        )
    stderr_raw = load_bytes(stderr_path, "recovery verifier stderr", maximum=16 * 1024 * 1024)
    if result.returncode != 0:
        detail = stderr_raw.decode("utf-8", errors="replace").strip()[-1000:]
        fail(f"exact checked-in recovery verifier failed closed: {detail or f'exit {result.returncode}'}")
    stdout_raw = load_bytes(stdout_path, "recovery verifier stdout", maximum=16 * 1024 * 1024)
    if not stdout_raw or b"VERIFIED locked rollout sha256=" not in stdout_raw:
        fail("exact checked-in recovery verifier produced no terminal verification record")
    _verifier_after, verifier_after_sha = repository_tool(
        RECOVERY_VERIFIER_RELATIVE, "recovery rollout verifier"
    )
    if verifier_after_sha != verifier_sha:
        fail("recovery rollout verifier changed while it was invoked")
    if (
        load_bytes(manifest_path, "rollout manifest", maximum=MAX_INPUT_BYTES) != manifest_raw
        or load_bytes(reward_path, "reward evidence", maximum=MAX_INPUT_BYTES) != reward_raw
    ):
        fail("sealed rollout or reward evidence changed during live verification")
    return {
        "manifestSha256": sha256(manifest_raw),
        "rewardEvidenceSha256": sha256(reward_raw),
        "stdoutSha256": sha256(stdout_raw),
        "verifierPath": RECOVERY_VERIFIER_RELATIVE.as_posix(),
        "verifierSha256": verifier_sha,
    }


def arc_text(base: int) -> str:
    value = Decimal(base) / Decimal(1_000_000_000)
    return format(value.normalize(), "f")


def render_readme_block(
    *,
    source_sha: str,
    frontend_sha: str,
    published_at: str,
    checkpoint: dict[str, Any],
    installer_sha: str,
    reward_base: int,
    receipt_count: int,
) -> str:
    total_arc = arc_text(reward_base * receipt_count)
    each_arc = arc_text(reward_base)
    short_source = source_sha[:12]
    short_frontend = frontend_sha[:12]
    return f"""{BEGIN_MARKER}
> **Live public testnet (evidence sealed after {published_at}):** The immutable
> [v0.8.0 release](https://github.com/FerrumVir/arc-chain/releases/tag/v0.8.0)
> is built from [`{short_source}`](https://github.com/FerrumVir/arc-chain/commit/{source_sha}).
> All six protocol-v3 validators serve the retained canonical chain through
> block **{checkpoint['height']:,}**, continue at **{checkpoint['recoveryHeight']:,}**, and are
> required to agree before the public console reports the network as healthy.
> The six prior public histories remain available as explicit, immutable,
> read-only noncanonical fork views; no legacy block was erased or renumbered.
> The recovered network bytes were first accepted on Pages from verified config
> commit [`{short_frontend}`](https://github.com/FerrumVir/arc-chain/commit/{frontend_sha}).
> This block and its machine-readable status are published only by a subsequent
> reviewed commit; the deployed site's `deployed-commit.txt` identifies that commit.

The first migration from an unsigned v0.7 build remains manual and pinned to
the exact tag. The published Linux x86_64 component proved this exact
bootstrap in both fresh-install and update-only modes:

```bash
curl -fsSLO --proto '=https' --proto-redir '=https' --tlsv1.2 https://raw.githubusercontent.com/FerrumVir/arc-chain/v0.8.0/install.sh
ARC_INSTALL_SHA256={installer_sha}
if command -v sha256sum >/dev/null 2>&1; then
  printf '%s  %s\\n' "$ARC_INSTALL_SHA256" install.sh | sha256sum -c -
else
  printf '%s  %s\\n' "$ARC_INSTALL_SHA256" install.sh | shasum -a 256 -c -
fi
bash install.sh --version 0.8.0
```

The immutable release includes headless Linux amd64 and arm64 binaries, Intel
and Apple Silicon macOS binaries, Windows CLI binaries, and desktop packages.
The command above is claimed only for the independently exercised Linux
x86_64 published component; the displayed installer digest comes from that
component's canonical acceptance receipt.
The signed v0.8 desktop updater checks shortly after startup and every 24 hours
when enabled, then asks for confirmation. The transactional headless updater
runs daily when installed as a service. Linux `.deb` and `.rpm` packages remain
package-manager owned.

### Community support answer sheet

| Community question | Current evidence-backed answer |
|---|---|
| Can an SSH-only EC2/VPS install ARC? | **Yes, on Linux x86_64 as exercised.** Use the pinned command above. `arc-node-linux-x86_64` is the GUI-free amd64 binary; the immutable release also includes Linux arm64 assets without extending this canary claim. |
| Are Intel and Apple Silicon Macs supported? | **Yes.** v0.8.0 contains separate signed CLI and desktop packages for x86_64 and arm64 macOS. |
| Does automatic update work? | **Yes, within the documented safety boundary.** v0.8 desktop checks automatically but requires confirmation; managed headless installs use the transactional daily updater. The one-time unsigned v0.7 migration is deliberately manual. |
| Are the seed nodes upgraded? | **Yes.** Six protocol-v3 validators are bound to checkpoint H={checkpoint['height']:,}, transition H+1={checkpoint['recoveryHeight']:,}, and an all-six public health gate. |
| Can a stake-zero community worker earn the configured {each_arc} ARC reward? | **Yes.** The production canary proved {receipt_count} distinct mined rewards totaling {total_arc} ARC for one exact-model stake-zero worker. Registration or a raw `0x16` inference attestation alone does not pay. |
| What hardware should a worker run? | The production target is Llama-2-7B Q4_K_M (about 4 GB on disk). Leave RAM and CPU headroom for a complete model load; GPU is optional and hardware never guarantees work. |
| Where are inference, earnings, and blocks? | The [dashboard]({PUBLIC_CONSOLE}) shows receipt-backed inference and confirmed/projected earnings. The [explorer]({PUBLIC_EXPLORER}) resolves the matching canonical block and transaction. Projections remain unavailable until enough real observations exist. |

See the [headless/server guide](docs/HEADLESS_INSTALL.md), the
[desktop guide](docs/GETTING_STARTED.md), and the
[2–3 minute walkthrough](docs/COMMUNITY-NODE-WALKTHROUGH.md). The exact public
proof identifiers are published in
[`shared/frontend/production-status.json`](shared/frontend/production-status.json).
{END_MARKER}"""


def build(args: argparse.Namespace) -> tuple[Path, Path]:
    readme_raw = load_bytes(args.readme, "README", maximum=4 * 1024 * 1024)
    try:
        readme = readme_raw.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        fail(f"README is not UTF-8: {error}")
    if readme.count(BEGIN_MARKER) != 1 or readme.count(END_MARKER) != 1:
        fail("README must contain exactly one ordered ARC public-truth marker pair")
    begin = readme.index(BEGIN_MARKER)
    end_begin = readme.index(END_MARKER)
    if end_begin <= begin:
        fail("README must contain exactly one ordered ARC public-truth marker pair")
    end = end_begin + len(END_MARKER)

    release, _release_raw = load_json(args.release_api, "release API response", canonical=False)
    config, config_raw = load_json(args.frontend_config, "frontend config", canonical=True)
    reward, reward_raw = load_json(args.reward_evidence, "reward evidence", canonical=True)
    manifest, manifest_raw = load_json(args.rollout_manifest, "rollout manifest", canonical=True)

    with tempfile.TemporaryDirectory(prefix="arc-public-truth-") as temporary:
        temporary_root = Path(temporary)
        published, published_hashes, published_values = validate_published_acceptance(
            args, temporary_root
        )
        source_sha = published_values["sourceSha"]
        published_at, release_assets = validate_release(release, source_sha)
        binding = published_values["binding"]
        binding_release = binding.get("release")
        binding_workflow = binding.get("release_workflow")
        if (
            binding.get("repository") != REPOSITORY
            or binding.get("tag") != TAG
            or binding.get("commit") != source_sha
            or not isinstance(binding_release, dict)
            or binding_release.get("id") != release["id"]
            or binding_release.get("immutable") is not True
            or not isinstance(binding_workflow, dict)
            or binding_workflow.get("run_id") != published["releaseRunId"]
            or binding_workflow.get("run_attempt") != published["releaseRunAttempt"]
            or binding_release.get("id") != published["releaseId"]
            or binding.get("assets") != release_assets
        ):
            fail("published release binding differs from the current immutable release API")
        release_run_id = positive_int(
            binding_workflow.get("run_id"), "published release binding run ID"
        )
        release_run_attempt = positive_int(
            binding_workflow.get("run_attempt"), "published release binding run attempt"
        )
        if (
            published_values["linuxComponent"].get("release_run_id") != release_run_id
            or published_values["linuxComponent"].get("release_run_attempt")
            != release_run_attempt
        ):
            fail("published Linux installer component belongs to another release attempt")
        installer_sha = published_values["installerSha256"]
        if release_assets["install.sh"]["sha256"] != installer_sha:
            fail("published Linux installer differs from the immutable release API")

        pages, pages_hashes = validate_pages(args, config_raw)
        frontend_sha = pages["acceptedConfigCommit"]
        checkpoint = validate_network(config, source_sha)
        worker, reward_base, receipt_count = validate_reward(
            reward, manifest, source_sha
        )

        # This is deliberately the final evidence gate.  The exact checked-in
        # rollout verifier re-reads the sealed files and performs live all-six
        # convergence and mined-reward verification immediately before any
        # public claim or acceptance receipt is rendered.
        recovery = run_recovery_verify(
            args.rollout_manifest,
            args.reward_evidence,
            manifest_raw,
            reward_raw,
            temporary_root,
        )

        acceptance = {
            "pages": {**pages, "evidenceSha256": pages_hashes},
            "publishedAcceptance": {
                **published,
                "evidenceSha256": published_hashes,
            },
            "recovery": recovery,
            "release": {
                "assetSetSha256": sha256(canonical_json(release_assets)),
                "id": release["id"],
                "publishedAt": published_at,
                "releaseApiSha256": sha256(_release_raw),
                "runAttempt": release_run_attempt,
                "runId": release_run_id,
                "sourceCommit": source_sha,
                "tag": TAG,
            },
            "repository": REPOSITORY,
            "schema": ACCEPTANCE_SCHEMA,
        }
        acceptance_raw = canonical_json(acceptance)

        block = render_readme_block(
            source_sha=source_sha,
            frontend_sha=frontend_sha,
            published_at=published_at,
            checkpoint=checkpoint,
            installer_sha=installer_sha,
            reward_base=reward_base,
            receipt_count=receipt_count,
        )
    output_readme = (readme[:begin] + block + readme[end:]).encode("utf-8")
    status = {
        "acceptance": {
            # Embed the complete canonical v2 receipt so a public reader can
            # independently canonicalize/re-hash it and follow its exact
            # workflow, run, job, artifact, Pages, and recovery evidence
            # identities.  The receipt deliberately does not depend on this
            # status document, so this introduces no circular hash.
            "receipt": acceptance,
            "publishedArtifactReceiptSha256": published[
                "canonicalReceiptSha256"
            ],
            "receiptSha256": sha256(acceptance_raw),
            "releaseRunAttempt": release_run_attempt,
            "releaseRunId": release_run_id,
        },
        "checkpoint": {
            "blockHash": chain_hash(checkpoint["blockHash"], "checkpoint.blockHash"),
            "height": checkpoint["height"],
            "legacyPublicMaxHeight": checkpoint["legacyPublicMaxHeight"],
            "manifestHash": chain_hash(checkpoint["manifestHash"], "checkpoint.manifestHash"),
            "protocolVersion": checkpoint["protocolVersion"],
            "recoveryHeight": checkpoint["recoveryHeight"],
            "stateRoot": chain_hash(checkpoint["stateRoot"], "checkpoint.stateRoot"),
        },
        "fleet": {
            "legacyForkCount": 6,
            "legacyForkPolicy": "immutable-read-only-noncanonical",
            "requiredHealthyValidators": 6,
            "validatorCount": 6,
        },
        "network": config["network"],
        "pages": {
            "acceptedConfigCommit": frontend_sha,
            "deploymentId": pages["deploymentId"],
            "runAttempt": pages["runAttempt"],
            "runId": pages["runId"],
        },
        "release": {
            "id": release["id"],
            "immutable": True,
            "publishedAt": published_at,
            "sourceCommit": source_sha,
            "tag": TAG,
            "url": release["html_url"],
            "version": VERSION,
        },
        "rewards": {
            "canaryReceiptCount": receipt_count,
            "canaryWorker": worker,
            "demonstratedGrossArc": float(Decimal(reward_base * receipt_count) / Decimal(1_000_000_000)),
            "demonstratedGrossBase": reward_base * receipt_count,
            "rewardPerReceiptArc": float(Decimal(reward_base) / Decimal(1_000_000_000)),
            "rewardPerReceiptBase": reward_base,
            "stakeZeroEligible": True,
        },
        "schema": STATUS_SCHEMA,
        "services": {"dashboard": PUBLIC_CONSOLE, "explorer": PUBLIC_EXPLORER},
        "state": "recovered",
    }
    output_status = canonical_json(status)

    output_dir: Path = args.output_dir
    if not output_dir.is_absolute() or output_dir.name in {"", ".", ".."}:
        fail("output directory must be an absolute dedicated path")
    directory_fd = -1
    try:
        output_dir.mkdir(mode=0o700, parents=False, exist_ok=False)
        os.chmod(output_dir, 0o700)
        created = output_dir.lstat()
        directory_flags = os.O_RDONLY
        if hasattr(os, "O_DIRECTORY"):
            directory_flags |= os.O_DIRECTORY
        if hasattr(os, "O_NOFOLLOW"):
            directory_flags |= os.O_NOFOLLOW
        directory_fd = os.open(output_dir, directory_flags)
        opened = os.fstat(directory_fd)
        if (
            not stat.S_ISDIR(opened.st_mode)
            or (created.st_dev, created.st_ino) != (opened.st_dev, opened.st_ino)
        ):
            fail("public-truth output directory changed while it was opened")
        readme_path = output_dir / "README.md"
        status_path = output_dir / "production-status.json"
        acceptance_path = output_dir / "POST-RELEASE-ACCEPTANCE.json"
        for name, raw in (
            ("README.md", output_readme),
            ("production-status.json", output_status),
            ("POST-RELEASE-ACCEPTANCE.json", acceptance_raw),
        ):
            file_flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
            if hasattr(os, "O_NOFOLLOW"):
                file_flags |= os.O_NOFOLLOW
            descriptor = os.open(name, file_flags, 0o400, dir_fd=directory_fd)
            try:
                view = memoryview(raw)
                while view:
                    written = os.write(descriptor, view)
                    if written <= 0:
                        fail(f"cannot completely write public-truth output {name}")
                    view = view[written:]
                os.fsync(descriptor)
                os.fchmod(descriptor, 0o400)
            finally:
                os.close(descriptor)
        os.fsync(directory_fd)
        final = output_dir.lstat()
        if (final.st_dev, final.st_ino) != (opened.st_dev, opened.st_ino):
            fail("public-truth output directory changed while it was written")
    except OSError as error:
        fail(f"cannot create public-truth output: {error}")
    finally:
        if directory_fd >= 0:
            os.close(directory_fd)
    return readme_path, status_path


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--readme", required=True, type=Path)
    result.add_argument("--release-api", required=True, type=Path)
    result.add_argument("--pages-workflow", required=True, type=Path)
    result.add_argument("--pages-run", required=True, type=Path)
    result.add_argument("--pages-jobs", required=True, type=Path)
    result.add_argument("--pages-api", required=True, type=Path)
    result.add_argument("--pages-deployments", required=True, type=Path)
    result.add_argument("--pages-statuses", required=True, type=Path)
    result.add_argument("--frontend-config", required=True, type=Path)
    result.add_argument("--deployed-commit", required=True, type=Path)
    result.add_argument("--deployed-sha256sums", required=True, type=Path)
    result.add_argument("--published-workflow", required=True, type=Path)
    result.add_argument("--published-run", required=True, type=Path)
    result.add_argument("--published-jobs", required=True, type=Path)
    result.add_argument("--published-artifact-metadata", required=True, type=Path)
    result.add_argument("--published-artifact-zip", required=True, type=Path)
    result.add_argument("--reward-evidence", required=True, type=Path)
    result.add_argument("--rollout-manifest", required=True, type=Path)
    result.add_argument("--output-dir", required=True, type=Path)
    return result


def main() -> int:
    try:
        readme, status = build(parser().parse_args())
    except TruthError as error:
        print(f"public truth: {error}", file=sys.stderr)
        return 1
    print(f"public truth README: {readme}")
    print(f"public truth status: {status}")
    print(f"public truth acceptance: {status.parent / 'POST-RELEASE-ACCEPTANCE.json'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
