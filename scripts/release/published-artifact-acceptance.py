#!/usr/bin/env python3
"""Bind and verify the public bytes exercised after an ARC release.

This utility deliberately has no GitHub credentials or network client.  The
read-only post-release workflow captures GitHub API documents, downloads
assets by immutable server ID, and gives those local facts to this verifier.
Keeping selection and byte verification here makes the security boundary
unit-testable without teaching repository code how to publish or mutate a
release.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import stat
import sys
import zipfile
from pathlib import Path
from typing import Any, Iterable


SHA40 = re.compile(r"[0-9a-f]{40}")
SHA256 = re.compile(r"[0-9a-f]{64}")
STRICT_TAG = re.compile(r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)")
EXPECTED_REPOSITORY = "FerrumVir/arc-chain"

EXPECTED_RELEASE_ASSETS = frozenset(
    {
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
)

EXPECTED_RELEASE_JOBS = frozenset(
    {
        "Validate release tag and pin commit",
        "Full quality gate on validated release commit",
        "Cargo dependency policy (workspace)",
        "Cargo dependency policy (desktop)",
        "Cargo dependency policy (updater-verifier)",
        "Golden vectors (linux-x86_64)",
        "Golden vectors (linux-arm64)",
        "Golden vectors (macos-arm64)",
        "Golden vectors (macos-x86_64)",
        "Golden vectors (windows-x86_64)",
        "Verify exact pre-tag headless linux-x86_64",
        "Verify exact pre-tag headless linux-arm64",
        "Verify exact pre-tag headless macos-arm64",
        "Verify exact pre-tag headless macos-x86_64",
        "Verify exact pre-tag headless windows-x86_64",
        "Ubuntu server smoke (linux-x86_64)",
        "Ubuntu server smoke (linux-arm64)",
        "Verify exact pre-tag desktop macos-arm64",
        "Verify exact pre-tag desktop macos-x86_64",
        "Verify exact pre-tag desktop windows-x86_64",
        "Verify exact pre-tag desktop linux-x86_64",
        "Assemble the exact unsigned release manifest",
        "Sign only the verified release manifest",
        "Create and upload one isolated release draft",
        "Verify GitHub draft bytes without publication authority",
        "Publish only the independently verified draft",
        "Verify the immutable GitHub release without publication authority",
    }
)
SKIPPED_RELEASE_JOB = "Delete a draft rejected by the unprivileged verifier"
PUBLISHED_EVIDENCE_MEMBER = "release-published.json"
MAX_PUBLISHED_EVIDENCE_BYTES = 4 * 1024 * 1024
MAX_PUBLISHED_EVIDENCE_ZIP_BYTES = 8 * 1024 * 1024

# v0.7.7 is not a GitHub-immutable release.  Its migration input is therefore
# pinned independently by tag commit, release ID, asset ID, size, and digest.
# Replacing or re-uploading the historical binary cannot silently satisfy this
# contract even if the public tag URL continues to resolve.
LEGACY_SOURCE = {
    "tag": "v0.7.7",
    "commit": "37df67b526cd0eebf6b55fa940c7646d1f4c947f",
    "release_id": 331028280,
    "asset": {
        "id": 432306066,
        "name": "arc-node-linux-x86_64",
        "size": 23286656,
        "sha256": "1cfc3039786d023cde24ad0b452f35735b39f9e83aaf293e6ed0bf623a11b20c",
    },
    "installer": {
        "path": "scripts/install-community-node.sh",
        "sha256": "34aabc144750ca656c88412372fff7b94384514776eca7b747de52f57bfa5430",
    },
    "seed_config": {
        "path": "testnet-seeds.txt",
        "sha256": "f2a71401e3b6a9d354a6c00ded4a3d8327f84bd0f4af7aea5076096f41d01671",
    },
    "genesis": {
        "path": "genesis.toml",
        "sha256": "805ea6c83f836a2076d9c140defd3f07ca6d75b0e074c956496db2665e72ee7d",
    },
}

EXPECTED_COMPONENTS = {
    "linux-x86_64": frozenset(
        {
            "install.sh",
            "arc-node-linux-x86_64",
            "arc-cli-linux-x86_64",
            "arc-desktop-linux-x86_64.AppImage",
            "arc-desktop-linux-x86_64.AppImage.sig",
        }
    ),
    "macos-arm64": frozenset(
        {
            "arc-desktop-macos-arm64.app.tar.gz",
            "arc-desktop-macos-arm64.app.tar.gz.sig",
            "arc-desktop-macos-arm64.dmg",
        }
    ),
    "macos-x86_64": frozenset(
        {
            "arc-desktop-macos-x86_64.app.tar.gz",
            "arc-desktop-macos-x86_64.app.tar.gz.sig",
            "arc-desktop-macos-x86_64.dmg",
        }
    ),
    "windows-x86_64": frozenset(
        {
            "arc-desktop-windows-x86_64-setup.exe",
            "arc-desktop-windows-x86_64-setup.exe.sig",
            "arc-desktop-windows-x86_64.msi",
        }
    ),
}

REQUIRED_CHECKS = {
    "linux-x86_64": {
        "appimage_process_stable": True,
        "appimage_updater_signature_valid": True,
        "appimage_visible_window": True,
        "appimage_window_capture_nonempty": True,
        "headless_fresh_install": True,
        "headless_update_only": True,
        "legacy_binary_executed_offline": True,
        "legacy_history_preserved": True,
        "legacy_installer_source_pinned": True,
        "legacy_model_preserved": True,
        "legacy_preexisting_offline": True,
        "legacy_state_preserved": True,
        "service_started": False,
        "v08_fresh_data": True,
    },
    "macos-arm64": {
        "archive_safe": True,
        "bundle_architecture": "arm64",
        "bundle_codesign_valid": True,
        "bundle_identifier": "network.arc.desktop",
        "bundle_version": "0.8.0",
        "desktop_process_stable": True,
        "desktop_visible_window": True,
        "dmg_bundle_matches": True,
        "dmg_verified": True,
        "isolated_profile": True,
        "service_started": False,
        "updater_signature_valid": True,
    },
    "macos-x86_64": {
        "archive_safe": True,
        "bundle_architecture": "x86_64",
        "bundle_codesign_valid": True,
        "bundle_identifier": "network.arc.desktop",
        "bundle_version": "0.8.0",
        "desktop_process_stable": True,
        "desktop_visible_window": True,
        "dmg_bundle_matches": True,
        "dmg_verified": True,
        "isolated_profile": True,
        "service_started": False,
        "updater_signature_valid": True,
    },
    "windows-x86_64": {
        "desktop_process_stable": True,
        "desktop_visible_window": True,
        "embedded_app_pe_machine": "AMD64",
        "isolated_profile": True,
        "msi_administrative_extract": True,
        "msi_product_version": "0.8.0",
        "no_installed_service": True,
        "setup_pe_machine": "AMD64",
        "updater_signature_valid": True,
    },
}

EXPECTED_EVIDENCE_FILES = frozenset(
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
MAX_CANONICAL_EVIDENCE_FILE_BYTES = 256 * 1024 * 1024
MAX_CANONICAL_EVIDENCE_TOTAL_BYTES = 512 * 1024 * 1024


class AcceptanceError(ValueError):
    """A captured release fact or downloaded byte failed closed."""


def load_json(path: Path, description: str) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise AcceptanceError(f"cannot read {description}: {error}") from error


def load_object(path: Path, description: str) -> dict[str, Any]:
    value = load_json(path, description)
    if not isinstance(value, dict):
        raise AcceptanceError(f"{description} must be a JSON object")
    return value


def load_object_bytes(payload: bytes, description: str) -> dict[str, Any]:
    try:
        value = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise AcceptanceError(f"cannot read {description}: {error}") from error
    if not isinstance(value, dict):
        raise AcceptanceError(f"{description} must be a JSON object")
    return value


def positive_int(value: Any, description: str) -> int:
    if type(value) is not int or value <= 0:  # noqa: E721
        raise AcceptanceError(f"{description} must be a positive integer")
    return value


def exact(value: Any, expected: Any, description: str) -> None:
    if type(value) is not type(expected) or value != expected:  # noqa: E721
        raise AcceptanceError(
            f"{description} mismatch: expected {expected!r}, got {value!r}"
        )


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_canonical(path: Path, value: Any) -> None:
    payload = json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n"
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.new.{os.getpid()}")
    try:
        temporary.write_text(payload, encoding="utf-8")
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def write_bytes(path: Path, payload: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.new.{os.getpid()}")
    try:
        temporary.write_bytes(payload)
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def asset_size_limit(name: str) -> int:
    if name == "arc-recovery-checkpoint-descriptor.json":
        return 1024 * 1024
    if name.endswith((".sig", ".json")):
        return 4 * 1024 * 1024
    return 2 * 1024 * 1024 * 1024


def validate_release_assets(
    release: dict[str, Any], repository: str, tag: str
) -> dict[str, dict[str, Any]]:
    raw_assets = release.get("assets")
    if not isinstance(raw_assets, list):
        raise AcceptanceError("release assets must be an array")
    assets: dict[str, dict[str, Any]] = {}
    for index, raw in enumerate(raw_assets):
        if not isinstance(raw, dict):
            raise AcceptanceError(f"release asset {index} must be an object")
        name = raw.get("name")
        if not isinstance(name, str) or not name:
            raise AcceptanceError(f"release asset {index} has an invalid name")
        if name in assets:
            raise AcceptanceError(f"duplicate release asset: {name}")
        asset_id = positive_int(raw.get("id"), f"asset {name} id")
        size = positive_int(raw.get("size"), f"asset {name} size")
        if size > asset_size_limit(name):
            raise AcceptanceError(f"asset {name} exceeds its size limit")
        digest = raw.get("digest")
        if not isinstance(digest, str) or not digest.startswith("sha256:"):
            raise AcceptanceError(f"asset {name} has no SHA-256 digest")
        sha256 = digest.removeprefix("sha256:")
        if not SHA256.fullmatch(sha256):
            raise AcceptanceError(f"asset {name} has a malformed SHA-256 digest")
        exact(raw.get("state"), "uploaded", f"asset {name} state")
        uploader = raw.get("uploader")
        if not isinstance(uploader, dict):
            raise AcceptanceError(f"asset {name} uploader must be an object")
        exact(uploader.get("login"), "github-actions[bot]", f"asset {name} uploader")
        expected_url = f"https://github.com/{repository}/releases/download/{tag}/{name}"
        exact(raw.get("browser_download_url"), expected_url, f"asset {name} URL")
        assets[name] = {
            "id": asset_id,
            "sha256": sha256,
            "size": size,
        }
    if set(assets) != EXPECTED_RELEASE_ASSETS:
        missing = sorted(EXPECTED_RELEASE_ASSETS - set(assets))
        extra = sorted(set(assets) - EXPECTED_RELEASE_ASSETS)
        raise AcceptanceError(
            f"release asset set mismatch; missing={missing!r}, extra={extra!r}"
        )
    if sum(item["size"] for item in assets.values()) > 12 * 1024 * 1024 * 1024:
        raise AcceptanceError("release asset total exceeds 12 GiB")
    return dict(sorted(assets.items()))


def validate_release_attempt_jobs(
    path: Path, repository: str, commit: str, run_id: int, run_attempt: int
) -> dict[str, Any]:
    value = load_object(path, "exact release-attempt jobs document")
    raw_jobs = value.get("jobs")
    if not isinstance(raw_jobs, list):
        raise AcceptanceError("exact release-attempt jobs must be an array")
    expected_count = len(EXPECTED_RELEASE_JOBS) + 1
    exact(value.get("total_count"), expected_count, "release-attempt jobs total_count")
    exact(len(raw_jobs), expected_count, "release-attempt jobs captured count")

    names: set[str] = set()
    ids: set[int] = set()
    jobs: list[dict[str, Any]] = []
    for index, raw in enumerate(raw_jobs):
        if not isinstance(raw, dict):
            raise AcceptanceError(f"release-attempt job {index} must be an object")
        name = raw.get("name")
        if not isinstance(name, str) or not name:
            raise AcceptanceError(f"release-attempt job {index} has an invalid name")
        if name in names:
            raise AcceptanceError(f"duplicate release-attempt job name: {name}")
        names.add(name)
        job_id = positive_int(raw.get("id"), f"release-attempt job {name} id")
        if job_id in ids:
            raise AcceptanceError("release-attempt job IDs must be unique")
        ids.add(job_id)
        exact(raw.get("run_id"), run_id, f"release-attempt job {name} run id")
        exact(
            raw.get("run_attempt"),
            run_attempt,
            f"release-attempt job {name} run attempt",
        )
        exact(raw.get("head_sha"), commit, f"release-attempt job {name} commit")
        exact(raw.get("status"), "completed", f"release-attempt job {name} status")
        expected_conclusion = "skipped" if name == SKIPPED_RELEASE_JOB else "success"
        exact(
            raw.get("conclusion"),
            expected_conclusion,
            f"release-attempt job {name} conclusion",
        )
        jobs.append(
            {
                "conclusion": expected_conclusion,
                "head_sha": commit,
                "id": job_id,
                "name": name,
                "run_attempt": run_attempt,
                "run_id": run_id,
                "status": "completed",
            }
        )
    expected_names = set(EXPECTED_RELEASE_JOBS) | {SKIPPED_RELEASE_JOB}
    if names != expected_names:
        missing = sorted(expected_names - names)
        extra = sorted(names - expected_names)
        raise AcceptanceError(
            f"release-attempt job set mismatch; missing={missing!r}, extra={extra!r}"
        )
    return {
        "commit": commit,
        "jobs": sorted(jobs, key=lambda job: job["name"]),
        "release_run_attempt": run_attempt,
        "release_run_id": run_id,
        "repository": repository,
        "schema": "arc.release-attempt-jobs.v1",
    }


def validate_published_artifact_metadata(
    artifact: dict[str, Any],
    commit: str,
    release_id: int,
    run_id: int,
    run_attempt: int,
) -> tuple[dict[str, Any], str]:
    artifact_id = positive_int(artifact.get("id"), "published-evidence artifact id")
    artifact_size = positive_int(
        artifact.get("size_in_bytes"), "published-evidence artifact size"
    )
    if artifact_size > MAX_PUBLISHED_EVIDENCE_ZIP_BYTES:
        raise AcceptanceError("published-evidence artifact exceeds its size limit")
    artifact_digest = artifact.get("digest")
    if not isinstance(artifact_digest, str) or not re.fullmatch(
        r"sha256:[0-9a-f]{64}", artifact_digest
    ):
        raise AcceptanceError("published-evidence artifact digest is malformed")
    exact(artifact.get("expired"), False, "published-evidence artifact expiration")
    workflow_run = artifact.get("workflow_run")
    if not isinstance(workflow_run, dict):
        raise AcceptanceError("published-evidence artifact workflow_run must be an object")
    exact(workflow_run.get("id"), run_id, "published-evidence artifact run id")
    exact(workflow_run.get("head_sha"), commit, "published-evidence artifact commit")
    artifact_name = artifact.get("name")
    if not isinstance(artifact_name, str):
        raise AcceptanceError("published-evidence artifact name is invalid")
    prefix = (
        f"arc-release-published-evidence-{commit}-{run_id}-{run_attempt}-{release_id}-"
    )
    match = re.fullmatch(re.escape(prefix) + r"([0-9a-f]{64})", artifact_name)
    if match is None:
        raise AcceptanceError("published-evidence artifact name is not exact-attempt bound")
    return (
        {
            "digest": artifact_digest,
            "expired": False,
            "id": artifact_id,
            "name": artifact_name,
            "size_in_bytes": artifact_size,
            "workflow_run": {"head_sha": commit, "id": run_id},
        },
        match.group(1),
    )


def command_select_published_evidence(args: argparse.Namespace) -> None:
    if not SHA40.fullmatch(args.commit):
        raise AcceptanceError("release commit must be 40 lowercase hex characters")
    run_id = positive_int(args.release_run_id, "release run id")
    run_attempt = positive_int(args.release_run_attempt, "release run attempt")
    release = load_object(args.release_json, "release API document")
    release_id = positive_int(release.get("id"), "release id")
    exact(release.get("tag_name"), "v0.8.0", "release tag")
    exact(release.get("target_commitish"), args.commit, "release target")
    exact(release.get("draft"), False, "release draft state")
    exact(release.get("immutable"), True, "release immutable state")

    artifacts_document = load_object(args.artifacts_json, "release artifacts document")
    raw_artifacts = artifacts_document.get("artifacts")
    if not isinstance(raw_artifacts, list):
        raise AcceptanceError("release artifacts must be an array")
    prefix = (
        f"arc-release-published-evidence-{args.commit}-{run_id}-{run_attempt}-"
        f"{release_id}-"
    )
    matches = [
        artifact
        for artifact in raw_artifacts
        if isinstance(artifact, dict)
        and isinstance(artifact.get("name"), str)
        and re.fullmatch(re.escape(prefix) + r"[0-9a-f]{64}", artifact["name"])
    ]
    if len(matches) != 1:
        raise AcceptanceError(
            "expected exactly one exact-attempt publication evidence artifact"
        )
    selected, _ = validate_published_artifact_metadata(
        matches[0], args.commit, release_id, run_id, run_attempt
    )
    write_canonical(args.output, selected)


def locate_actions_zip(root: Path, artifact_name: str) -> Path:
    try:
        root_mode = root.lstat().st_mode
    except OSError as error:
        raise AcceptanceError(f"cannot inspect published-evidence download: {error}") from error
    if not stat.S_ISDIR(root_mode):
        raise AcceptanceError("published-evidence download root must be a directory")
    named = root / artifact_name
    try:
        named_mode = named.lstat().st_mode
    except FileNotFoundError:
        source = root
    except OSError as error:
        raise AcceptanceError(f"cannot inspect published-evidence artifact: {error}") from error
    else:
        if not stat.S_ISDIR(named_mode):
            raise AcceptanceError("named published-evidence artifact must be a directory")
        source = named
    entries = list(source.iterdir())
    if len(entries) != 1:
        raise AcceptanceError("raw published-evidence download must contain exactly one ZIP")
    archive = entries[0]
    regular_file(archive, "raw published-evidence Actions ZIP")
    if archive.stat().st_size > MAX_PUBLISHED_EVIDENCE_ZIP_BYTES:
        raise AcceptanceError("raw published-evidence Actions ZIP exceeds its size limit")
    return archive


def validate_published_evidence(
    artifact_path: Path,
    download_root: Path,
    release: dict[str, Any],
    assets: dict[str, dict[str, Any]],
    repository: str,
    tag: str,
    commit: str,
    release_id: int,
    run_id: int,
    run_attempt: int,
) -> tuple[bytes, bytes, dict[str, Any]]:
    raw_artifact = load_object(artifact_path, "published-evidence artifact document")
    artifact, name_evidence_sha = validate_published_artifact_metadata(
        raw_artifact, commit, release_id, run_id, run_attempt
    )
    artifact_id = artifact["id"]
    artifact_size = artifact["size_in_bytes"]
    artifact_digest = artifact["digest"]
    artifact_name = artifact["name"]
    archive = locate_actions_zip(download_root, artifact_name)
    archive_payload = archive.read_bytes()
    exact(len(archive_payload), artifact_size, "published-evidence ZIP size")
    exact(
        f"sha256:{hashlib.sha256(archive_payload).hexdigest()}",
        artifact_digest,
        "published-evidence ZIP digest",
    )

    try:
        with zipfile.ZipFile(archive, "r") as handle:
            members = handle.infolist()
            if len(members) != 1:
                raise AcceptanceError(
                    "published-evidence ZIP must contain exactly release-published.json"
                )
            member = members[0]
            mode_type = (member.external_attr >> 16) & 0o170000
            if (
                member.filename != PUBLISHED_EVIDENCE_MEMBER
                or member.is_dir()
                or member.flag_bits & 0x1
                or mode_type not in (0, 0o100000)
                or member.file_size <= 0
                or member.file_size > MAX_PUBLISHED_EVIDENCE_BYTES
            ):
                raise AcceptanceError(
                    "published-evidence ZIP contains an unsafe or invalid member"
                )
            payload = handle.read(member)
    except (zipfile.BadZipFile, RuntimeError, OSError) as error:
        raise AcceptanceError(f"cannot read published-evidence ZIP: {error}") from error
    if len(payload) > MAX_PUBLISHED_EVIDENCE_BYTES:
        raise AcceptanceError("published release API evidence exceeds its size limit")
    evidence_sha = hashlib.sha256(payload).hexdigest()
    exact(evidence_sha, name_evidence_sha, "published-evidence content-name digest")

    evidence_release = load_object_bytes(payload, "published release API evidence")
    exact(evidence_release.get("id"), release_id, "published evidence release id")
    exact(evidence_release.get("tag_name"), tag, "published evidence release tag")
    exact(
        evidence_release.get("target_commitish"),
        commit,
        "published evidence release target",
    )
    exact(evidence_release.get("draft"), False, "published evidence draft state")
    exact(
        evidence_release.get("prerelease"),
        False,
        "published evidence prerelease state",
    )
    exact(
        evidence_release.get("immutable"),
        True,
        "published evidence immutable state",
    )
    evidence_author = evidence_release.get("author")
    if not isinstance(evidence_author, dict):
        raise AcceptanceError("published evidence author must be an object")
    exact(
        evidence_author.get("login"),
        "github-actions[bot]",
        "published evidence author",
    )
    evidence_assets = validate_release_assets(evidence_release, repository, tag)
    exact(evidence_assets, assets, "published evidence release assets")

    metadata = {
        "artifact": {
            "digest": artifact_digest,
            "expired": False,
            "id": artifact_id,
            "name": artifact_name,
            "size": artifact_size,
            "workflow_run_id": run_id,
            "head_sha": commit,
        },
        "commit": commit,
        "release_id": release_id,
        "release_published_sha256": evidence_sha,
        "release_run_attempt": run_attempt,
        "release_run_id": run_id,
        "repository": repository,
        "schema": "arc.release-published-evidence-artifact.v1",
        "tag": tag,
    }
    return payload, archive_payload, metadata


def command_bind(args: argparse.Namespace) -> None:
    repository = args.repository
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise AcceptanceError("repository must be an exact owner/name pair")
    exact(repository, EXPECTED_REPOSITORY, "production repository")
    if not STRICT_TAG.fullmatch(args.tag):
        raise AcceptanceError("release tag must be strict vMAJOR.MINOR.PATCH")
    if args.tag != "v0.8.0":
        raise AcceptanceError("this production acceptance is pinned to v0.8.0")
    if not SHA40.fullmatch(args.commit):
        raise AcceptanceError("release commit must be 40 lowercase hex characters")
    run_id = positive_int(args.release_run_id, "release run id")
    run_attempt = positive_int(args.release_run_attempt, "release run attempt")

    workflow = load_object(args.workflow_json, "release workflow document")
    workflow_id = positive_int(workflow.get("id"), "release workflow id")
    exact(workflow.get("path"), ".github/workflows/release.yml", "release workflow path")
    exact(workflow.get("state"), "active", "release workflow state")

    run = load_object(args.run_json, "release run document")
    exact(run.get("id"), run_id, "release run id")
    exact(run.get("run_attempt"), run_attempt, "release run attempt")
    exact(run.get("workflow_id"), workflow_id, "release run workflow id")
    head_repository = run.get("head_repository")
    if not isinstance(head_repository, dict):
        raise AcceptanceError("release run head_repository must be an object")
    exact(head_repository.get("full_name"), repository, "release run repository")
    exact(run.get("path"), ".github/workflows/release.yml", "release run path")
    exact(run.get("event"), "workflow_dispatch", "release run event")
    exact(run.get("head_branch"), "main", "release run branch")
    exact(run.get("head_sha"), args.commit, "release run commit")
    exact(run.get("status"), "completed", "release run status")
    exact(run.get("conclusion"), "success", "release run conclusion")
    jobs = validate_release_attempt_jobs(
        args.jobs_json, repository, args.commit, run_id, run_attempt
    )

    tag_ref = load_object(args.tag_ref_json, "release tag ref")
    exact(tag_ref.get("ref"), f"refs/tags/{args.tag}", "release tag ref name")
    tag_object = tag_ref.get("object")
    if not isinstance(tag_object, dict):
        raise AcceptanceError("release tag ref object must be an object")
    exact(tag_object.get("type"), "commit", "release tag object type")
    exact(tag_object.get("sha"), args.commit, "release tag commit")

    release = load_object(args.release_json, "release API document")
    release_id = positive_int(release.get("id"), "release id")
    exact(release.get("tag_name"), args.tag, "release tag")
    exact(release.get("target_commitish"), args.commit, "release target")
    exact(release.get("draft"), False, "release draft state")
    exact(release.get("prerelease"), False, "release prerelease state")
    exact(release.get("immutable"), True, "release immutable state")
    author = release.get("author")
    if not isinstance(author, dict):
        raise AcceptanceError("release author must be an object")
    exact(author.get("login"), "github-actions[bot]", "release author")
    assets = validate_release_assets(release, repository, args.tag)
    published_payload, published_zip, published_metadata = validate_published_evidence(
        args.published_evidence_artifact_json,
        args.published_evidence_download_root,
        release,
        assets,
        repository,
        args.tag,
        args.commit,
        release_id,
        run_id,
        run_attempt,
    )

    legacy_ref = load_object(args.legacy_tag_ref_json, "v0.7.7 tag ref")
    exact(legacy_ref.get("ref"), "refs/tags/v0.7.7", "v0.7.7 ref name")
    legacy_ref_object = legacy_ref.get("object")
    if not isinstance(legacy_ref_object, dict):
        raise AcceptanceError("v0.7.7 tag object must be an object")
    exact(legacy_ref_object.get("type"), "commit", "v0.7.7 tag object type")
    exact(legacy_ref_object.get("sha"), LEGACY_SOURCE["commit"], "v0.7.7 tag commit")

    legacy_release = load_object(args.legacy_release_json, "v0.7.7 release document")
    exact(legacy_release.get("id"), LEGACY_SOURCE["release_id"], "v0.7.7 release id")
    exact(legacy_release.get("tag_name"), "v0.7.7", "v0.7.7 release tag")
    legacy_matches = [
        asset
        for asset in legacy_release.get("assets", [])
        if isinstance(asset, dict) and asset.get("name") == LEGACY_SOURCE["asset"]["name"]
    ]
    if len(legacy_matches) != 1:
        raise AcceptanceError("v0.7.7 Linux binary selection is ambiguous")
    legacy_asset = legacy_matches[0]
    expected_legacy_asset = LEGACY_SOURCE["asset"]
    exact(legacy_asset.get("id"), expected_legacy_asset["id"], "v0.7.7 asset id")
    exact(legacy_asset.get("size"), expected_legacy_asset["size"], "v0.7.7 asset size")
    exact(
        legacy_asset.get("digest"),
        f"sha256:{expected_legacy_asset['sha256']}",
        "v0.7.7 asset digest",
    )
    exact(legacy_asset.get("state"), "uploaded", "v0.7.7 asset state")
    exact(
        legacy_asset.get("browser_download_url"),
        f"https://github.com/{repository}/releases/download/v0.7.7/arc-node-linux-x86_64",
        "v0.7.7 asset URL",
    )

    write_canonical(args.jobs_output, jobs)
    write_bytes(args.published_evidence_output, published_payload)
    write_bytes(args.published_evidence_zip_output, published_zip)
    write_canonical(args.published_evidence_artifact_output, published_metadata)

    binding = {
        "assets": assets,
        "commit": args.commit,
        "legacy_source": LEGACY_SOURCE,
        "published_evidence": {
            "artifact_digest": published_metadata["artifact"]["digest"],
            "artifact_id": published_metadata["artifact"]["id"],
            "artifact_metadata_sha256": sha256_file(
                args.published_evidence_artifact_output
            ),
            "artifact_name": published_metadata["artifact"]["name"],
            "artifact_size": published_metadata["artifact"]["size"],
            "release_published_sha256": published_metadata[
                "release_published_sha256"
            ],
        },
        "release": {"id": release_id, "immutable": True},
        "release_workflow": {
            "event": "workflow_dispatch",
            "head_branch": "main",
            "head_sha": args.commit,
            "id": workflow_id,
            "jobs_sha256": sha256_file(args.jobs_output),
            "path": ".github/workflows/release.yml",
            "run_attempt": run_attempt,
            "run_id": run_id,
        },
        "repository": repository,
        "schema": "arc.published-release-binding.v1",
        "tag": args.tag,
    }
    write_canonical(args.output, binding)


def validate_binding(binding: dict[str, Any]) -> None:
    exact(binding.get("schema"), "arc.published-release-binding.v1", "binding schema")
    if set(binding) != {
        "assets",
        "commit",
        "legacy_source",
        "published_evidence",
        "release",
        "release_workflow",
        "repository",
        "schema",
        "tag",
    }:
        raise AcceptanceError("binding contains missing or unexpected fields")
    if not SHA40.fullmatch(str(binding.get("commit", ""))):
        raise AcceptanceError("binding commit is malformed")
    exact(binding.get("repository"), EXPECTED_REPOSITORY, "binding repository")
    exact(binding.get("tag"), "v0.8.0", "binding tag")
    exact(binding.get("legacy_source"), LEGACY_SOURCE, "binding v0.7.7 source")
    release = binding.get("release")
    if not isinstance(release, dict):
        raise AcceptanceError("binding release must be an object")
    positive_int(release.get("id"), "binding release id")
    exact(release.get("immutable"), True, "binding release immutability")
    workflow = binding.get("release_workflow")
    if not isinstance(workflow, dict):
        raise AcceptanceError("binding release_workflow must be an object")
    if set(workflow) != {
        "event",
        "head_branch",
        "head_sha",
        "id",
        "jobs_sha256",
        "path",
        "run_attempt",
        "run_id",
    }:
        raise AcceptanceError("binding release_workflow has invalid fields")
    positive_int(workflow.get("id"), "binding workflow id")
    positive_int(workflow.get("run_id"), "binding run id")
    positive_int(workflow.get("run_attempt"), "binding run attempt")
    exact(workflow.get("event"), "workflow_dispatch", "binding workflow event")
    exact(workflow.get("head_branch"), "main", "binding workflow branch")
    exact(
        workflow.get("path"),
        ".github/workflows/release.yml",
        "binding workflow path",
    )
    exact(workflow.get("head_sha"), binding["commit"], "binding workflow commit")
    if not SHA256.fullmatch(str(workflow.get("jobs_sha256", ""))):
        raise AcceptanceError("binding release-attempt jobs digest is malformed")
    published = binding.get("published_evidence")
    if not isinstance(published, dict) or set(published) != {
        "artifact_digest",
        "artifact_id",
        "artifact_metadata_sha256",
        "artifact_name",
        "artifact_size",
        "release_published_sha256",
    }:
        raise AcceptanceError("binding published_evidence has invalid fields")
    positive_int(published.get("artifact_id"), "binding published-evidence artifact id")
    positive_int(
        published.get("artifact_size"), "binding published-evidence artifact size"
    )
    if not re.fullmatch(
        r"sha256:[0-9a-f]{64}", str(published.get("artifact_digest", ""))
    ):
        raise AcceptanceError("binding published-evidence artifact digest is malformed")
    for key in ("artifact_metadata_sha256", "release_published_sha256"):
        if not SHA256.fullmatch(str(published.get(key, ""))):
            raise AcceptanceError(f"binding published-evidence {key} is malformed")
    expected_artifact_name = (
        f"arc-release-published-evidence-{binding['commit']}-{workflow['run_id']}-"
        f"{workflow['run_attempt']}-{release['id']}-"
        f"{published['release_published_sha256']}"
    )
    exact(
        published.get("artifact_name"),
        expected_artifact_name,
        "binding published-evidence artifact name",
    )
    assets = binding.get("assets")
    if not isinstance(assets, dict) or set(assets) != EXPECTED_RELEASE_ASSETS:
        raise AcceptanceError("binding has an invalid release asset set")
    for name, value in assets.items():
        if not isinstance(value, dict):
            raise AcceptanceError(f"binding asset {name} must be an object")
        positive_int(value.get("id"), f"binding asset {name} id")
        positive_int(value.get("size"), f"binding asset {name} size")
        if not SHA256.fullmatch(str(value.get("sha256", ""))):
            raise AcceptanceError(f"binding asset {name} digest is malformed")


def regular_file(path: Path, description: str) -> None:
    try:
        mode = path.lstat().st_mode
    except OSError as error:
        raise AcceptanceError(f"cannot inspect {description}: {error}") from error
    if not stat.S_ISREG(mode):
        raise AcceptanceError(f"{description} must be a regular non-symlink file")


def verify_named_files(
    binding: dict[str, Any], directory: Path, names: Iterable[str]
) -> dict[str, dict[str, Any]]:
    requested = list(names)
    if not requested or len(set(requested)) != len(requested):
        raise AcceptanceError("asset names must be a nonempty unique list")
    assets = binding["assets"]
    verified: dict[str, dict[str, Any]] = {}
    for name in requested:
        if name not in assets:
            raise AcceptanceError(f"asset is not in the release binding: {name}")
        if Path(name).name != name or name in {".", ".."}:
            raise AcceptanceError(f"unsafe asset name: {name}")
        path = directory / name
        regular_file(path, f"downloaded asset {name}")
        actual_size = path.stat().st_size
        actual_sha = sha256_file(path)
        exact(actual_size, assets[name]["size"], f"asset {name} downloaded size")
        exact(actual_sha, assets[name]["sha256"], f"asset {name} downloaded digest")
        verified[name] = dict(assets[name])
    return dict(sorted(verified.items()))


def command_verify_files(args: argparse.Namespace) -> None:
    binding = load_object(args.binding, "release binding")
    validate_binding(binding)
    verified = verify_named_files(binding, args.directory, args.asset)
    receipt = {
        "assets": verified,
        "binding_sha256": sha256_file(args.binding),
        "commit": binding["commit"],
        "release_id": binding["release"]["id"],
        "release_run_attempt": binding["release_workflow"]["run_attempt"],
        "release_run_id": binding["release_workflow"]["run_id"],
        "repository": binding["repository"],
        "schema": "arc.published-release-files.v1",
        "tag": binding["tag"],
    }
    write_canonical(args.output, receipt)


def command_verify_legacy(args: argparse.Namespace) -> None:
    binding = load_object(args.binding, "release binding")
    validate_binding(binding)
    asset = binding["legacy_source"]["asset"]
    regular_file(args.binary, "v0.7.7 binary")
    exact(args.binary.stat().st_size, asset["size"], "v0.7.7 binary size")
    exact(sha256_file(args.binary), asset["sha256"], "v0.7.7 binary digest")
    for path, key in (
        (args.installer, "installer"),
        (args.seed_config, "seed_config"),
        (args.genesis, "genesis"),
    ):
        regular_file(path, f"v0.7.7 {key}")
        exact(
            sha256_file(path),
            binding["legacy_source"][key]["sha256"],
            f"v0.7.7 {key} digest",
        )
    receipt = {
        "asset": asset,
        "binding_sha256": sha256_file(args.binding),
        "commit": binding["legacy_source"]["commit"],
        "genesis_sha256": sha256_file(args.genesis),
        "installer_sha256": sha256_file(args.installer),
        "release_id": binding["legacy_source"]["release_id"],
        "schema": "arc.published-legacy-source.v1",
        "seed_config_sha256": sha256_file(args.seed_config),
        "tag": "v0.7.7",
    }
    write_canonical(args.output, receipt)


def validate_platform_checks(platform: str, checks: dict[str, Any]) -> None:
    for name, expected in REQUIRED_CHECKS[platform].items():
        exact(checks.get(name), expected, f"{platform} check {name}")
    if platform == "windows-x86_64":
        embedded_version = checks.get("embedded_app_product_version")
        if not isinstance(embedded_version, str) or re.fullmatch(
            r"0\.8\.0(?:\.0)?", embedded_version
        ) is None:
            raise AcceptanceError(
                "windows-x86_64 check embedded_app_product_version is not an exact "
                "0.8.0 Windows version"
            )


def command_component(args: argparse.Namespace) -> None:
    binding = load_object(args.binding, "release binding")
    validate_binding(binding)
    if args.platform not in EXPECTED_COMPONENTS:
        raise AcceptanceError(f"unsupported component platform: {args.platform}")
    files = load_object(args.files_receipt, "downloaded-files receipt")
    exact(files.get("schema"), "arc.published-release-files.v1", "files receipt schema")
    exact(files.get("binding_sha256"), sha256_file(args.binding), "files binding digest")
    exact(files.get("repository"), binding["repository"], "files repository")
    exact(files.get("tag"), binding["tag"], "files tag")
    exact(files.get("commit"), binding["commit"], "files commit")
    exact(files.get("release_id"), binding["release"]["id"], "files release id")
    exact(
        files.get("release_run_id"),
        binding["release_workflow"]["run_id"],
        "files release run id",
    )
    exact(
        files.get("release_run_attempt"),
        binding["release_workflow"]["run_attempt"],
        "files release run attempt",
    )
    raw_assets = files.get("assets")
    if not isinstance(raw_assets, dict) or set(raw_assets) != EXPECTED_COMPONENTS[args.platform]:
        raise AcceptanceError(f"{args.platform} files receipt has an invalid asset set")
    for name, value in raw_assets.items():
        exact(value, binding["assets"][name], f"files asset {name}")

    checks = load_object(args.checks_json, "component checks")
    validate_platform_checks(args.platform, checks)
    component = {
        "acceptance_run_attempt": positive_int(
            args.acceptance_run_attempt, "acceptance run attempt"
        ),
        "acceptance_run_id": positive_int(args.acceptance_run_id, "acceptance run id"),
        "assets": raw_assets,
        "binding_sha256": sha256_file(args.binding),
        "checks": checks,
        "commit": binding["commit"],
        "platform": args.platform,
        "release_id": binding["release"]["id"],
        "release_run_attempt": binding["release_workflow"]["run_attempt"],
        "release_run_id": binding["release_workflow"]["run_id"],
        "repository": binding["repository"],
        "schema": "arc.published-artifact-acceptance-component.v1",
        "tag": binding["tag"],
    }
    # Run the same aggregate-side validator here so a malformed component is
    # rejected in its native job as well as by the final fan-in job.
    validate_component(component, binding, sha256_file(args.binding))
    write_canonical(args.output, component)


def validate_component(
    component: dict[str, Any], binding: dict[str, Any], binding_sha: str
) -> str:
    exact(
        component.get("schema"),
        "arc.published-artifact-acceptance-component.v1",
        "component schema",
    )
    platform = component.get("platform")
    if platform not in EXPECTED_COMPONENTS:
        raise AcceptanceError(f"unexpected component platform: {platform!r}")
    exact(component.get("repository"), binding["repository"], f"{platform} repository")
    exact(component.get("tag"), binding["tag"], f"{platform} tag")
    exact(component.get("commit"), binding["commit"], f"{platform} commit")
    exact(component.get("release_id"), binding["release"]["id"], f"{platform} release")
    exact(
        component.get("release_run_id"),
        binding["release_workflow"]["run_id"],
        f"{platform} release run",
    )
    exact(
        component.get("release_run_attempt"),
        binding["release_workflow"]["run_attempt"],
        f"{platform} release attempt",
    )
    exact(component.get("binding_sha256"), binding_sha, f"{platform} binding digest")
    positive_int(component.get("acceptance_run_id"), f"{platform} acceptance run")
    positive_int(component.get("acceptance_run_attempt"), f"{platform} acceptance attempt")
    raw_assets = component.get("assets")
    if not isinstance(raw_assets, dict) or set(raw_assets) != EXPECTED_COMPONENTS[platform]:
        raise AcceptanceError(f"{platform} component has an invalid asset set")
    for name, value in raw_assets.items():
        exact(value, binding["assets"][name], f"{platform} asset {name}")
    checks = component.get("checks")
    if not isinstance(checks, dict):
        raise AcceptanceError(f"{platform} checks must be an object")
    validate_platform_checks(platform, checks)
    return platform


def validate_component_artifacts(
    value: dict[str, Any], binding: dict[str, Any], run_id: int, run_attempt: int
) -> dict[str, dict[str, Any]]:
    exact(
        value.get("schema"),
        "arc.published-artifact-component-binding.v1",
        "component artifact binding schema",
    )
    exact(value.get("repository"), binding["repository"], "component repository")
    exact(value.get("acceptance_run_id"), run_id, "component acceptance run")
    exact(
        value.get("acceptance_run_attempt"),
        run_attempt,
        "component acceptance attempt",
    )
    raw = value.get("artifacts")
    if not isinstance(raw, dict) or set(raw) != set(EXPECTED_COMPONENTS):
        raise AcceptanceError("component artifact platform set mismatch")
    ids: set[int] = set()
    result: dict[str, dict[str, Any]] = {}
    for platform in sorted(EXPECTED_COMPONENTS):
        artifact = raw.get(platform)
        if not isinstance(artifact, dict):
            raise AcceptanceError(f"{platform} artifact identity must be an object")
        expected_name = (
            f"arc-published-acceptance-{platform}-{run_id}-attempt-{run_attempt}"
        )
        exact(artifact.get("name"), expected_name, f"{platform} artifact name")
        artifact_id = positive_int(artifact.get("id"), f"{platform} artifact id")
        if artifact_id in ids:
            raise AcceptanceError("component artifact IDs must be unique")
        ids.add(artifact_id)
        digest = artifact.get("digest")
        if not isinstance(digest, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
            raise AcceptanceError(f"{platform} artifact digest is malformed")
        positive_int(artifact.get("size"), f"{platform} artifact size")
        exact(artifact.get("expired"), False, f"{platform} artifact expiration")
        exact(artifact.get("workflow_run_id"), run_id, f"{platform} artifact run")
        exact(artifact.get("head_sha"), binding["commit"], f"{platform} artifact SHA")
        result[platform] = {
            "digest": digest,
            "id": artifact_id,
            "name": expected_name,
        }
    return result


def collect_evidence_files(root: Path) -> dict[str, dict[str, Any]]:
    try:
        root_mode = root.lstat().st_mode
    except OSError as error:
        raise AcceptanceError(f"cannot inspect canonical evidence root: {error}") from error
    if not stat.S_ISDIR(root_mode):
        raise AcceptanceError("canonical evidence root must be a directory")

    expected_directories = set(EXPECTED_COMPONENTS) | {"release"}
    directories: set[str] = set()
    files: dict[str, dict[str, Any]] = {}
    total = 0
    for path in sorted(root.rglob("*")):
        try:
            mode = path.lstat().st_mode
        except OSError as error:
            raise AcceptanceError(f"cannot inspect canonical evidence path: {error}") from error
        relative = path.relative_to(root).as_posix()
        if stat.S_ISDIR(mode):
            directories.add(relative)
            continue
        if not stat.S_ISREG(mode):
            raise AcceptanceError(
                f"canonical evidence must contain only regular files: {relative}"
            )
        size = path.stat().st_size
        if size > MAX_CANONICAL_EVIDENCE_FILE_BYTES:
            raise AcceptanceError(f"canonical evidence file exceeds its limit: {relative}")
        total += size
        if total > MAX_CANONICAL_EVIDENCE_TOTAL_BYTES:
            raise AcceptanceError("canonical evidence exceeds its aggregate size limit")
        files[relative] = {"sha256": sha256_file(path), "size": size}
    if directories != expected_directories:
        missing = sorted(expected_directories - directories)
        extra = sorted(directories - expected_directories)
        raise AcceptanceError(
            f"canonical evidence directory set mismatch; missing={missing!r}, extra={extra!r}"
        )
    if set(files) != EXPECTED_EVIDENCE_FILES:
        missing = sorted(EXPECTED_EVIDENCE_FILES - set(files))
        extra = sorted(set(files) - EXPECTED_EVIDENCE_FILES)
        raise AcceptanceError(
            f"canonical evidence file set mismatch; missing={missing!r}, extra={extra!r}"
        )
    return dict(sorted(files.items()))


def validate_evidence_manifest(
    value: dict[str, Any],
    manifest_path: Path,
    evidence_root: Path,
    binding: dict[str, Any],
    binding_path: Path,
    component_artifacts_path: Path,
    run_id: int,
    run_attempt: int,
) -> dict[str, dict[str, Any]]:
    if set(value) != {
        "acceptance_run_attempt",
        "acceptance_run_id",
        "binding_sha256",
        "component_artifact_binding_sha256",
        "files",
        "repository",
        "schema",
    }:
        raise AcceptanceError("canonical evidence manifest has invalid fields")
    exact(
        value.get("schema"),
        "arc.published-artifact-evidence-manifest.v1",
        "canonical evidence manifest schema",
    )
    exact(value.get("repository"), binding["repository"], "evidence repository")
    exact(value.get("acceptance_run_id"), run_id, "evidence acceptance run")
    exact(
        value.get("acceptance_run_attempt"),
        run_attempt,
        "evidence acceptance attempt",
    )
    exact(
        value.get("binding_sha256"),
        sha256_file(binding_path),
        "evidence binding digest",
    )
    exact(
        value.get("component_artifact_binding_sha256"),
        sha256_file(component_artifacts_path),
        "evidence component-artifact binding digest",
    )
    raw_files = value.get("files")
    if not isinstance(raw_files, dict):
        raise AcceptanceError("canonical evidence manifest files must be an object")
    actual_files = collect_evidence_files(evidence_root)
    exact(raw_files, actual_files, "canonical evidence file manifest")
    exact(
        raw_files["release/release-attempt-jobs.json"]["sha256"],
        binding["release_workflow"]["jobs_sha256"],
        "canonical release-attempt jobs digest",
    )
    exact(
        raw_files["release/release-published.json"]["sha256"],
        binding["published_evidence"]["release_published_sha256"],
        "canonical published release evidence digest",
    )
    exact(
        raw_files["release/published-evidence-artifact.json"]["sha256"],
        binding["published_evidence"]["artifact_metadata_sha256"],
        "canonical published-evidence artifact metadata digest",
    )
    exact(
        raw_files["release/published-evidence.zip"]["sha256"],
        binding["published_evidence"]["artifact_digest"].removeprefix("sha256:"),
        "canonical published-evidence Actions ZIP digest",
    )
    regular_file(manifest_path, "canonical evidence manifest")
    return actual_files


def command_evidence_manifest(args: argparse.Namespace) -> None:
    binding = load_object(args.binding, "release binding")
    validate_binding(binding)
    run_id = positive_int(args.acceptance_run_id, "acceptance run id")
    run_attempt = positive_int(args.acceptance_run_attempt, "acceptance run attempt")
    component_artifact_value = load_object(
        args.component_artifacts, "component artifact binding"
    )
    validate_component_artifacts(
        component_artifact_value, binding, run_id, run_attempt
    )
    manifest = {
        "acceptance_run_attempt": run_attempt,
        "acceptance_run_id": run_id,
        "binding_sha256": sha256_file(args.binding),
        "component_artifact_binding_sha256": sha256_file(args.component_artifacts),
        "files": collect_evidence_files(args.evidence_root),
        "repository": binding["repository"],
        "schema": "arc.published-artifact-evidence-manifest.v1",
    }
    write_canonical(args.output, manifest)
    validate_evidence_manifest(
        manifest,
        args.output,
        args.evidence_root,
        binding,
        args.binding,
        args.component_artifacts,
        run_id,
        run_attempt,
    )


def command_aggregate(args: argparse.Namespace) -> None:
    binding = load_object(args.binding, "release binding")
    validate_binding(binding)
    binding_sha = sha256_file(args.binding)
    run_id = positive_int(args.acceptance_run_id, "acceptance run id")
    run_attempt = positive_int(args.acceptance_run_attempt, "acceptance run attempt")
    component_artifact_value = load_object(
        args.component_artifacts, "component artifact binding"
    )
    component_artifacts = validate_component_artifacts(
        component_artifact_value, binding, run_id, run_attempt
    )
    evidence_manifest = load_object(args.evidence_manifest, "canonical evidence manifest")
    evidence_files = validate_evidence_manifest(
        evidence_manifest,
        args.evidence_manifest,
        args.evidence_root,
        binding,
        args.binding,
        args.component_artifacts,
        run_id,
        run_attempt,
    )
    components: dict[str, dict[str, Any]] = {}
    component_digests: dict[str, str] = {}
    paths = sorted(args.components.glob("*.json"))
    if not paths:
        raise AcceptanceError("component receipt directory is empty")
    for path in paths:
        regular_file(path, "component receipt")
        component = load_object(path, "component receipt")
        platform = validate_component(component, binding, binding_sha)
        if platform in components:
            raise AcceptanceError(f"duplicate component platform: {platform}")
        exact(component.get("acceptance_run_id"), run_id, f"{platform} acceptance run")
        exact(
            component.get("acceptance_run_attempt"),
            run_attempt,
            f"{platform} acceptance attempt",
        )
        components[platform] = component
        component_digests[platform] = sha256_file(path)
    if set(components) != set(EXPECTED_COMPONENTS):
        missing = sorted(set(EXPECTED_COMPONENTS) - set(components))
        extra = sorted(set(components) - set(EXPECTED_COMPONENTS))
        raise AcceptanceError(
            f"component platform set mismatch; missing={missing!r}, extra={extra!r}"
        )
    receipt = {
        "acceptance_run_attempt": run_attempt,
        "acceptance_run_id": run_id,
        "binding_sha256": binding_sha,
        "commit": binding["commit"],
        "component_artifact_binding_sha256": sha256_file(args.component_artifacts),
        "component_artifacts": component_artifacts,
        "component_receipt_sha256": dict(sorted(component_digests.items())),
        "evidence_file_count": len(evidence_files),
        "evidence_manifest_sha256": sha256_file(args.evidence_manifest),
        "legacy_source": binding["legacy_source"],
        "release_id": binding["release"]["id"],
        "release_immutable": True,
        "release_run_attempt": binding["release_workflow"]["run_attempt"],
        "release_run_id": binding["release_workflow"]["run_id"],
        "repository": binding["repository"],
        "schema": "arc.published-artifact-acceptance.v1",
        "tag": binding["tag"],
        "verified_platforms": sorted(components),
    }
    write_canonical(args.output, receipt)


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)

    select = commands.add_parser(
        "select-published-evidence",
        help="select the sole exact-attempt publication evidence artifact",
    )
    select.add_argument("--commit", required=True)
    select.add_argument("--release-run-id", required=True, type=int)
    select.add_argument("--release-run-attempt", required=True, type=int)
    select.add_argument("--release-json", required=True, type=Path)
    select.add_argument("--artifacts-json", required=True, type=Path)
    select.add_argument("--output", required=True, type=Path)
    select.set_defaults(func=command_select_published_evidence)

    bind = commands.add_parser("bind", help="validate GitHub API facts and seal selection")
    bind.add_argument("--repository", required=True)
    bind.add_argument("--tag", required=True)
    bind.add_argument("--commit", required=True)
    bind.add_argument("--release-run-id", required=True, type=int)
    bind.add_argument("--release-run-attempt", required=True, type=int)
    bind.add_argument("--workflow-json", required=True, type=Path)
    bind.add_argument("--run-json", required=True, type=Path)
    bind.add_argument("--jobs-json", required=True, type=Path)
    bind.add_argument("--tag-ref-json", required=True, type=Path)
    bind.add_argument("--release-json", required=True, type=Path)
    bind.add_argument("--published-evidence-artifact-json", required=True, type=Path)
    bind.add_argument("--published-evidence-download-root", required=True, type=Path)
    bind.add_argument("--legacy-tag-ref-json", required=True, type=Path)
    bind.add_argument("--legacy-release-json", required=True, type=Path)
    bind.add_argument("--jobs-output", required=True, type=Path)
    bind.add_argument("--published-evidence-output", required=True, type=Path)
    bind.add_argument("--published-evidence-zip-output", required=True, type=Path)
    bind.add_argument(
        "--published-evidence-artifact-output", required=True, type=Path
    )
    bind.add_argument("--output", required=True, type=Path)
    bind.set_defaults(func=command_bind)

    files = commands.add_parser("verify-files", help="verify downloaded v0.8 assets")
    files.add_argument("--binding", required=True, type=Path)
    files.add_argument("--directory", required=True, type=Path)
    files.add_argument("--asset", required=True, action="append")
    files.add_argument("--output", required=True, type=Path)
    files.set_defaults(func=command_verify_files)

    legacy = commands.add_parser("verify-legacy", help="verify exact v0.7.7 inputs")
    legacy.add_argument("--binding", required=True, type=Path)
    legacy.add_argument("--binary", required=True, type=Path)
    legacy.add_argument("--installer", required=True, type=Path)
    legacy.add_argument("--seed-config", required=True, type=Path)
    legacy.add_argument("--genesis", required=True, type=Path)
    legacy.add_argument("--output", required=True, type=Path)
    legacy.set_defaults(func=command_verify_legacy)

    component = commands.add_parser("component", help="seal one platform result")
    component.add_argument("--binding", required=True, type=Path)
    component.add_argument("--files-receipt", required=True, type=Path)
    component.add_argument("--checks-json", required=True, type=Path)
    component.add_argument("--platform", required=True)
    component.add_argument("--acceptance-run-id", required=True, type=int)
    component.add_argument("--acceptance-run-attempt", required=True, type=int)
    component.add_argument("--output", required=True, type=Path)
    component.set_defaults(func=command_component)

    evidence = commands.add_parser(
        "evidence-manifest", help="seal every retained platform and release evidence file"
    )
    evidence.add_argument("--binding", required=True, type=Path)
    evidence.add_argument("--component-artifacts", required=True, type=Path)
    evidence.add_argument("--evidence-root", required=True, type=Path)
    evidence.add_argument("--acceptance-run-id", required=True, type=int)
    evidence.add_argument("--acceptance-run-attempt", required=True, type=int)
    evidence.add_argument("--output", required=True, type=Path)
    evidence.set_defaults(func=command_evidence_manifest)

    aggregate = commands.add_parser("aggregate", help="seal all platform receipts")
    aggregate.add_argument("--binding", required=True, type=Path)
    aggregate.add_argument("--component-artifacts", required=True, type=Path)
    aggregate.add_argument("--components", required=True, type=Path)
    aggregate.add_argument("--evidence-manifest", required=True, type=Path)
    aggregate.add_argument("--evidence-root", required=True, type=Path)
    aggregate.add_argument("--acceptance-run-id", required=True, type=int)
    aggregate.add_argument("--acceptance-run-attempt", required=True, type=int)
    aggregate.add_argument("--output", required=True, type=Path)
    aggregate.set_defaults(func=command_aggregate)
    return root


def main() -> int:
    args = parser().parse_args()
    try:
        args.func(args)
    except (AcceptanceError, OSError) as error:
        print(f"published artifact acceptance failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
