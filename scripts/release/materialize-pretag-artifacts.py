#!/usr/bin/env python3
"""Verify selected pre-tag archives and materialize the exact release bytes."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import tarfile
import tempfile
import zipfile
from contextlib import contextmanager
from collections.abc import Iterator
from pathlib import Path, PurePosixPath

GROUPS = (
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

MAX_ACTIONS_ZIP_BYTES = 4 * 1024 * 1024 * 1024
MAX_EXPANDED_GROUP_BYTES = 4 * 1024 * 1024 * 1024
MAX_INNER_EXPANSION_RATIO = 20
EXPANSION_SLACK_BYTES = 64 * 1024 * 1024
MATERIALIZATION_SCHEMA = "arc.pretag.materialization.v1"


def fail(message: str) -> "NoReturn":
    raise SystemExit(f"pre-tag artifact verification: {message}")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def headless_files(platform: str) -> tuple[str, ...]:
    suffix = ".exe" if platform == "windows-x86_64" else ""
    return (
        f"arc-node-{platform}{suffix}",
        f"arc-cli-{platform}{suffix}",
        "genesis.toml",
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--downloads-root", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--run-id", required=True, type=int)
    parser.add_argument("--run-attempt", required=True, type=int)
    parser.add_argument("--version", required=True)
    parser.add_argument("--selection-json", required=True)
    parser.add_argument("--only", action="append", default=[])
    parser.add_argument(
        "--retain-build-metadata",
        action="store_true",
        help=(
            "retain the already-verified BUILD-METADATA.json beside materialized "
            "payloads for a pre-tag runtime canary; publication leaves it out"
        ),
    )
    return parser.parse_args()


def selected_groups(values: list[str]) -> tuple[tuple[str, str], ...]:
    if not values:
        return GROUPS
    result: list[tuple[str, str]] = []
    allowed = {f"{kind}:{platform}": (kind, platform) for kind, platform in GROUPS}
    for value in values:
        if value not in allowed:
            fail(f"unsupported --only group {value!r}")
        if allowed[value] in result:
            fail(f"duplicate --only group {value!r}")
        result.append(allowed[value])
    return tuple(result)


def load_selection(raw: str) -> dict:
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as error:
        fail(f"selection JSON is invalid: {error}")
    if not isinstance(value, dict):
        fail("selection JSON must be an object")
    return value


def validate_selection_shape(selection: dict) -> None:
    expected: dict[str, set[str]] = {}
    for kind, platform in GROUPS:
        expected.setdefault(platform, set()).add(kind)
    if set(selection) != set(expected):
        fail(
            f"selection platforms differ: expected {sorted(expected)}, "
            f"got {sorted(selection)}"
        )

    selected_ids: set[int] = set()
    for platform, kinds in expected.items():
        platform_value = selection.get(platform)
        if not isinstance(platform_value, dict) or set(platform_value) != kinds:
            actual = sorted(platform_value) if isinstance(platform_value, dict) else []
            fail(
                f"selection groups differ for {platform}: "
                f"expected {sorted(kinds)}, got {actual}"
            )
        for kind in kinds:
            value = selection_entry(selection, kind, platform)
            artifact_id = value["id"]
            if artifact_id in selected_ids:
                fail(f"selection reuses artifact ID {artifact_id}")
            selected_ids.add(artifact_id)


def selection_entry(selection: dict, kind: str, platform: str) -> dict:
    platform_value = selection.get(platform)
    if not isinstance(platform_value, dict):
        fail(f"selection omits platform {platform}")
    value = platform_value.get(kind)
    if not isinstance(value, dict):
        fail(f"selection omits {kind}/{platform}")
    if not isinstance(value.get("id"), int) or value["id"] <= 0:
        fail(f"selection has invalid artifact ID for {kind}/{platform}")
    if re.fullmatch(r"sha256:[0-9a-f]{64}", str(value.get("digest", ""))) is None:
        fail(f"selection has invalid server digest for {kind}/{platform}")
    if re.fullmatch(r"[0-9a-f]{64}", str(value.get("archive_sha256", ""))) is None:
        fail(f"selection has invalid archive digest for {kind}/{platform}")
    if not isinstance(value.get("size_in_bytes"), int) or value["size_in_bytes"] <= 0:
        fail(f"selection has invalid artifact size for {kind}/{platform}")
    return value


def artifact_directory(root: Path, artifact_name: str, one_group: bool) -> Path:
    named = root / artifact_name
    if named.is_dir() and not named.is_symlink():
        return named
    if one_group and root.is_dir() and not root.is_symlink():
        return root
    fail(f"downloaded artifact directory is missing: {artifact_name}")


@contextmanager
def verified_outer_artifact(
    source_dir: Path,
    value: dict,
    archive_name: str,
) -> Iterator[Path]:
    entries = list(source_dir.iterdir())
    if len(entries) != 1:
        fail(
            "raw Actions artifact directory must contain exactly one ZIP; "
            f"got {sorted(path.name for path in entries)}"
        )
    artifact_zip = entries[0]
    if (
        not artifact_zip.is_file()
        or artifact_zip.is_symlink()
        or artifact_zip.stat().st_size <= 0
        or artifact_zip.stat().st_size > MAX_ACTIONS_ZIP_BYTES
    ):
        fail(
            "raw Actions artifact is empty, oversized, non-regular, or "
            f"symlinked: {artifact_zip}"
        )

    expected_server_digest = value["digest"].removeprefix("sha256:")
    if sha256(artifact_zip) != expected_server_digest:
        fail(
            "downloaded Actions ZIP does not match the selected artifact.digest "
            f"for artifact ID {value['id']}"
        )
    if artifact_zip.stat().st_size != value["size_in_bytes"]:
        fail(
            f"downloaded Actions ZIP size differs for artifact ID {value['id']}: "
            f"expected {value['size_in_bytes']}, got {artifact_zip.stat().st_size}"
        )

    expected_names = {"SHA256SUMS", archive_name}
    with tempfile.TemporaryDirectory(prefix="arc-pretag-outer-") as temporary:
        temporary_root = Path(temporary)
        try:
            with zipfile.ZipFile(artifact_zip, "r") as outer:
                infos = outer.infolist()
                if len(infos) != len(expected_names):
                    fail(
                        "Actions ZIP entry count differs: "
                        f"expected {len(expected_names)}, got {len(infos)}"
                    )
                actual_names: set[str] = set()
                expanded_bytes = 0
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
                    ):
                        fail(
                            "Actions ZIP contains an unsafe, encrypted, or "
                            f"non-regular entry: {info.filename!r}"
                        )
                    if info.filename in actual_names:
                        fail(f"Actions ZIP contains duplicate entry {info.filename!r}")
                    if info.file_size <= 0:
                        fail(f"Actions ZIP contains empty entry {info.filename!r}")
                    expanded_bytes += info.file_size
                    actual_names.add(info.filename)
                outer_expansion_limit = min(
                    MAX_EXPANDED_GROUP_BYTES,
                    artifact_zip.stat().st_size + EXPANSION_SLACK_BYTES,
                )
                if expanded_bytes > outer_expansion_limit:
                    fail(
                        "Actions ZIP exceeds the allowed expansion bound: "
                        f"{expanded_bytes} > {outer_expansion_limit} bytes"
                    )
                if actual_names != expected_names:
                    fail(
                        "Actions ZIP membership differs: "
                        f"expected {sorted(expected_names)}, got {sorted(actual_names)}"
                    )
                for info in infos:
                    target = temporary_root / info.filename
                    with outer.open(info, "r") as source, target.open("xb") as handle:
                        shutil.copyfileobj(source, handle)
        except (OSError, zipfile.BadZipFile) as error:
            fail(f"cannot read selected Actions artifact ZIP: {error}")
        yield temporary_root


def parse_checksums(path: Path) -> tuple[dict[str, str], str, str]:
    if not path.is_file() or path.is_symlink():
        fail(f"checksum manifest is missing or symlinked: {path}")
    lines = path.read_text(encoding="utf-8").splitlines()
    if len(lines) != 8 or lines[0] != "# ARC pre-tag artifact v1":
        fail(f"checksum manifest has an invalid line contract: {path}")
    headers: dict[str, str] = {}
    for line in lines[1:7]:
        match = re.fullmatch(r"# ([a-z_]+)=(.+)", line)
        if match is None or match.group(1) in headers:
            fail(f"checksum manifest has an invalid header: {line!r}")
        headers[match.group(1)] = match.group(2)
    match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9_.-]+\.tar\.gz)", lines[7])
    if match is None:
        fail("checksum manifest has an invalid archive record")
    return headers, match.group(1), match.group(2)


def safe_members(
    archive: tarfile.TarFile,
    expected: set[str],
    compressed_size: int,
) -> dict[str, tarfile.TarInfo]:
    result: dict[str, tarfile.TarInfo] = {}
    expanded_bytes = 0
    for member in archive.getmembers():
        pure = PurePosixPath(member.name)
        if (
            pure.is_absolute()
            or ".." in pure.parts
            or len(pure.parts) != 1
            or "\\" in member.name
            or ":" in member.name
        ):
            fail(f"archive contains an unsafe path: {member.name!r}")
        if not member.isfile() or member.issym() or member.islnk():
            fail(f"archive contains a non-regular member: {member.name!r}")
        if member.size <= 0:
            fail(f"archive contains an empty member: {member.name!r}")
        if member.name in result:
            fail(f"archive contains duplicate member {member.name!r}")
        result[member.name] = member
        expanded_bytes += member.size
    if set(result) != expected:
        fail(
            f"archive membership differs: expected {sorted(expected)}, "
            f"got {sorted(result)}"
        )
    expansion_limit = min(
        MAX_EXPANDED_GROUP_BYTES,
        compressed_size * MAX_INNER_EXPANSION_RATIO + EXPANSION_SLACK_BYTES,
    )
    if expanded_bytes > expansion_limit:
        fail(
            "inner candidate archive exceeds the allowed expansion bound: "
            f"{expanded_bytes} > {expansion_limit} bytes"
        )
    return result


def verify_group(
    args: argparse.Namespace,
    selection: dict,
    kind: str,
    platform: str,
    one_group: bool,
) -> None:
    value = selection_entry(selection, kind, platform)
    stem = (
        f"arc-pretag-{kind}-{platform}-{args.commit}-"
        f"{args.run_id}-{args.run_attempt}"
    )
    expected_artifact_name = f"{stem}-{value['archive_sha256']}"
    if value.get("name") != expected_artifact_name:
        fail(f"selection name is not commit/run/hash bound for {kind}/{platform}")
    source_dir = artifact_directory(args.downloads_root, expected_artifact_name, one_group)
    archive_name = f"{stem}.tar.gz"
    with verified_outer_artifact(source_dir, value, archive_name) as artifact_root:
        headers, manifest_sha, manifest_archive = parse_checksums(
            artifact_root / "SHA256SUMS"
        )
        expected_headers = {
            "kind": kind,
            "repository": args.repository,
            "commit": args.commit,
            "run_id": str(args.run_id),
            "run_attempt": str(args.run_attempt),
            "platform": platform,
        }
        if headers != expected_headers:
            fail(
                f"checksum headers differ for {kind}/{platform}: "
                f"expected {expected_headers}, got {headers}"
            )
        if manifest_archive != archive_name:
            fail(f"checksum manifest names the wrong archive for {kind}/{platform}")
        archive_path = artifact_root / archive_name
        actual_archive_sha = sha256(archive_path)
        if manifest_sha != value["archive_sha256"] or actual_archive_sha != manifest_sha:
            fail(f"archive SHA-256 mismatch for {kind}/{platform}")

        required = (
            DESKTOP_FILES[platform]
            if kind == "desktop"
            else headless_files(platform)
        )
        expected_members = set(required) | {"BUILD-METADATA.json"}
        destination_name = (
            f"arc-desktop-{platform}"
            if kind == "desktop"
            else f"headless-{platform}"
        )
        destination = args.output_dir / destination_name
        if destination.exists():
            fail(f"refusing to replace materialized group {destination}")
        destination.mkdir(parents=True)

        with tempfile.TemporaryDirectory(prefix="arc-pretag-verify-") as temporary:
            temporary_root = Path(temporary)
            with tarfile.open(archive_path, "r:gz") as archive:
                members = safe_members(
                    archive,
                    expected_members,
                    archive_path.stat().st_size,
                )
                for name, member in members.items():
                    extracted = archive.extractfile(member)
                    if extracted is None:
                        fail(f"cannot read archive member {name}")
                    target = temporary_root / name
                    with target.open("xb") as handle:
                        shutil.copyfileobj(extracted, handle)
                    os.chmod(target, member.mode & 0o777)

            metadata_path = temporary_root / "BUILD-METADATA.json"
            try:
                metadata_raw = metadata_path.read_bytes()
                metadata = json.loads(metadata_raw)
            except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
                fail(f"invalid build metadata for {kind}/{platform}: {error}")
            expected_metadata = {
                "schema": "arc.pretag.artifact.v1",
                "kind": kind,
                "repository": args.repository,
                "commit": args.commit,
                "platform": platform,
                "version": args.version,
                "workflow_run_id": args.run_id,
                "workflow_run_attempt": args.run_attempt,
            }
            expected_metadata_fields = set(expected_metadata) | {
                "rust_target",
                "files",
            }
            if not isinstance(metadata, dict) or set(metadata) != expected_metadata_fields:
                fail(f"metadata field set differs for {kind}/{platform}")
            try:
                canonical_metadata = (
                    json.dumps(
                        metadata,
                        sort_keys=True,
                        separators=(",", ":"),
                        allow_nan=False,
                    )
                    + "\n"
                ).encode("utf-8")
            except (TypeError, ValueError) as error:
                fail(
                    f"build metadata is not canonical JSON for "
                    f"{kind}/{platform}: {error}"
                )
            if metadata_raw != canonical_metadata:
                fail(f"build metadata is not canonical JSON for {kind}/{platform}")
            for field, expected in expected_metadata.items():
                if metadata.get(field) != expected:
                    fail(
                        f"metadata {field} mismatch for {kind}/{platform}: "
                        f"expected {expected!r}, got {metadata.get(field)!r}"
                    )
            if metadata.get("rust_target") != RUST_TARGETS[platform]:
                fail(
                    f"metadata rust_target mismatch for {kind}/{platform}: "
                    f"expected {RUST_TARGETS[platform]!r}, "
                    f"got {metadata.get('rust_target')!r}"
                )
            hashes = metadata.get("files")
            if not isinstance(hashes, dict) or set(hashes) != set(required):
                fail(f"metadata file set differs for {kind}/{platform}")
            for name in required:
                expected_hash = hashes.get(name)
                if re.fullmatch(r"[0-9a-f]{64}", str(expected_hash)) is None:
                    fail(f"metadata hash is invalid for {kind}/{platform}/{name}")
                if sha256(temporary_root / name) != expected_hash:
                    fail(f"payload hash mismatch for {kind}/{platform}/{name}")
                shutil.copy2(temporary_root / name, destination / name)
            if args.retain_build_metadata:
                shutil.copy2(
                    temporary_root / "BUILD-METADATA.json",
                    destination / "BUILD-METADATA.json",
                )
                receipt = {
                    "schema": MATERIALIZATION_SCHEMA,
                    "repository": args.repository,
                    "commit": args.commit,
                    "run_id": args.run_id,
                    "run_attempt": args.run_attempt,
                    "version": args.version,
                    "kind": kind,
                    "platform": platform,
                    "artifact": {
                        "id": value["id"],
                        "name": value["name"],
                        "digest": value["digest"],
                        "size_in_bytes": value["size_in_bytes"],
                        "archive_sha256": value["archive_sha256"],
                    },
                    "build_metadata_sha256": sha256(
                        temporary_root / "BUILD-METADATA.json"
                    ),
                    "files": hashes,
                }
                receipt_path = destination / "MATERIALIZATION-RECEIPT.json"
                payload = (
                    json.dumps(receipt, sort_keys=True, separators=(",", ":")) + "\n"
                ).encode()
                flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
                if hasattr(os, "O_NOFOLLOW"):
                    flags |= os.O_NOFOLLOW
                descriptor = os.open(receipt_path, flags, 0o444)
                try:
                    offset = 0
                    while offset < len(payload):
                        written = os.write(descriptor, payload[offset:])
                        if written <= 0:
                            fail("materialization receipt write made no progress")
                        offset += written
                    os.fchmod(descriptor, 0o444)
                    os.fsync(descriptor)
                finally:
                    os.close(descriptor)


def main() -> None:
    args = parse_args()
    if args.repository != "FerrumVir/arc-chain":
        fail(f"unexpected repository {args.repository!r}")
    if re.fullmatch(r"[0-9a-f]{40}", args.commit) is None:
        fail("commit must be one full lowercase Git SHA")
    if args.run_id <= 0 or args.run_attempt <= 0:
        fail("run ID and attempt must be positive")
    if re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", args.version) is None:
        fail("version must be strict MAJOR.MINOR.PATCH")
    if not args.downloads_root.is_dir() or args.downloads_root.is_symlink():
        fail("downloads root must be a real directory")
    if args.output_dir.exists() and args.output_dir.is_symlink():
        fail("output directory must not be a symlink")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    selection = load_selection(args.selection_json)
    validate_selection_shape(selection)
    groups = selected_groups(args.only)
    for kind, platform in groups:
        verify_group(args, selection, kind, platform, len(groups) == 1)
    print(f"Verified and materialized {len(groups)} exact pre-tag artifact group(s)")


if __name__ == "__main__":
    main()
