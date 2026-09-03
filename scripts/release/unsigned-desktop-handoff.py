#!/usr/bin/env python3
"""Create, select, and verify the unsigned desktop build handoff.

The unsigned build runs on a runner that never receives updater signing keys.
It uploads one create-only, commit/run/attempt/hash-bound artifact.  A fresh
release-environment runner downloads that artifact by immutable Actions ID and
uses this helper to verify both the server ZIP digest and the inner build
manifest before the minimal Tauri file signer sees the signing key. Native
bundling never runs in the key-bearing job.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import stat
import tarfile
import tempfile
import zipfile
from pathlib import Path, PurePosixPath
from typing import BinaryIO, NoReturn


SCHEMA = "arc.unsigned-desktop-handoff.v1"
CHECKSUM_HEADER = "# ARC unsigned desktop handoff v1"
PLATFORMS = {
    "linux-x86_64": ("x86_64-unknown-linux-gnu", "arc-desktop"),
    "macos-arm64": ("aarch64-apple-darwin", "arc-desktop"),
    "macos-x86_64": ("x86_64-apple-darwin", "arc-desktop"),
    "windows-x86_64": ("x86_64-pc-windows-msvc", "arc-desktop.exe"),
}
UNSIGNED_FILES = {
    "linux-x86_64": (
        "arc-desktop-linux-x86_64.AppImage",
        "arc-desktop-linux-x86_64.deb",
        "arc-desktop-linux-x86_64.rpm",
    ),
    "macos-arm64": (
        "arc-desktop-macos-arm64.app.tar.gz",
        "arc-desktop-macos-arm64.dmg",
    ),
    "macos-x86_64": (
        "arc-desktop-macos-x86_64.app.tar.gz",
        "arc-desktop-macos-x86_64.dmg",
    ),
    "windows-x86_64": (
        "arc-desktop-windows-x86_64-setup.exe",
        "arc-desktop-windows-x86_64.msi",
    ),
}
UPDATER_FILES = {
    "linux-x86_64": "arc-desktop-linux-x86_64.AppImage",
    "macos-arm64": "arc-desktop-macos-arm64.app.tar.gz",
    "macos-x86_64": "arc-desktop-macos-x86_64.app.tar.gz",
    "windows-x86_64": "arc-desktop-windows-x86_64-setup.exe",
}
MAX_PAYLOAD_FILE_BYTES = 2 * 1024 * 1024 * 1024
MAX_PAYLOAD_BYTES = 4 * 1024 * 1024 * 1024
MAX_METADATA_BYTES = 4 * 1024 * 1024
MAX_ARCHIVE_BYTES = 4 * 1024 * 1024 * 1024
MAX_ACTIONS_ZIP_BYTES = 4 * 1024 * 1024 * 1024
MAX_EXPANSION_RATIO = 30
EXPANSION_SLACK_BYTES = 64 * 1024 * 1024


def fail(message: str) -> NoReturn:
    raise SystemExit(f"unsigned desktop handoff: {message}")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonical_json(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


def validate_scalars(args: argparse.Namespace) -> tuple[str, str]:
    if args.repository != "FerrumVir/arc-chain":
        fail(f"unexpected repository {args.repository!r}")
    if re.fullmatch(r"[0-9a-f]{40}", args.commit) is None:
        fail("commit must be one full lowercase Git SHA")
    if args.run_id <= 0 or args.run_attempt <= 0:
        fail("run ID and attempt must be positive")
    if re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", args.version) is None:
        fail("version must be strict MAJOR.MINOR.PATCH")
    try:
        expected_target, binary_name = PLATFORMS[args.platform]
    except KeyError:
        fail(f"unsupported platform {args.platform!r}")
    if args.rust_target != expected_target:
        fail(
            f"platform {args.platform!r} requires Rust target "
            f"{expected_target!r}, got {args.rust_target!r}"
        )
    return expected_target, binary_name


def safe_relative(path: PurePosixPath) -> bool:
    rendered = str(path)
    return (
        not path.is_absolute()
        and len(path.parts) > 0
        and ".." not in path.parts
        and "." not in path.parts
        and "\\" not in rendered
        and ":" not in rendered
        and not any(ord(char) < 32 or ord(char) == 127 for char in rendered)
        and len(rendered.encode()) <= 240
    )


def require_regular(path: Path, *, label: str, maximum: int) -> os.stat_result:
    try:
        metadata = path.lstat()
    except OSError as error:
        fail(f"cannot inspect {label}: {error}")
    if not stat.S_ISREG(metadata.st_mode) or path.is_symlink():
        fail(f"{label} must be a non-symlink regular file")
    if metadata.st_size <= 0 or metadata.st_size > maximum:
        fail(f"{label} size is outside the bounded contract")
    return metadata


def signer_input_permissions_are_read_only(
    metadata: os.stat_result, *, windows: bool
) -> bool:
    """Return whether a signer input has the host's strict read-only state.

    POSIX preserves the materializer's exact ``0400`` mode. Native Windows
    deliberately maps ``chmod(..., 0400)`` to the read-only file attribute and
    reports the resulting permission bits as ``0444``. Prove both Windows
    representations and reject reparse points instead of weakening the POSIX
    contract to a cross-platform numeric mode comparison.
    """

    mode = stat.S_IMODE(metadata.st_mode)
    if not windows:
        return mode == 0o400

    readonly_flag = getattr(stat, "FILE_ATTRIBUTE_READONLY", None)
    reparse_flag = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", None)
    file_attributes = getattr(metadata, "st_file_attributes", None)
    if (
        not isinstance(readonly_flag, int)
        or not isinstance(reparse_flag, int)
        or not isinstance(file_attributes, int)
    ):
        return False
    return (
        file_attributes & readonly_flag != 0
        and file_attributes & reparse_flag == 0
        and mode & (stat.S_IWUSR | stat.S_IWGRP | stat.S_IWOTH) == 0
    )


def same_regular_file_identity(left: os.stat_result, right: os.stat_result) -> bool:
    """Compare an open descriptor and pathname without trusting mode text."""

    return (
        stat.S_ISREG(left.st_mode)
        and stat.S_ISREG(right.st_mode)
        and left.st_dev == right.st_dev
        and left.st_ino == right.st_ino
        and left.st_nlink == 1
        and right.st_nlink == 1
    )


def windows_permissions_match_mode(metadata: os.stat_result, mode: int) -> bool:
    """Prove the Windows read-only attribute implied by a portable mode."""

    readonly_flag = getattr(stat, "FILE_ATTRIBUTE_READONLY", None)
    reparse_flag = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", None)
    file_attributes = getattr(metadata, "st_file_attributes", None)
    if (
        not isinstance(readonly_flag, int)
        or not isinstance(reparse_flag, int)
        or not isinstance(file_attributes, int)
        or file_attributes & reparse_flag != 0
    ):
        return False
    expects_writable = mode & stat.S_IWUSR != 0
    is_readonly = file_attributes & readonly_flag != 0
    has_write_mode = metadata.st_mode & (
        stat.S_IWUSR | stat.S_IWGRP | stat.S_IWOTH
    ) != 0
    return (
        (expects_writable and not is_readonly and has_write_mode)
        or (not expects_writable and is_readonly and not has_write_mode)
    )


def set_open_file_mode(path: Path, descriptor: int, mode: int) -> None:
    """Set a mode while proving that path still names the open regular file.

    Python gained ``os.fchmod`` on Windows only in 3.13, while the hosted
    Windows runner currently exposes Python 3.12. On that runtime, use the
    pathname API only while retaining the descriptor, and bind the pathname to
    the descriptor both before and after the mutation. A replacement, link, or
    reparse-point race therefore fails closed rather than being accepted as
    the staged handoff object.
    """

    before_descriptor = os.fstat(descriptor)
    before_path = path.lstat()
    if path.is_symlink() or not same_regular_file_identity(
        before_descriptor, before_path
    ):
        fail(f"open file identity differs before chmod for {path.name}")

    descriptor_chmod = getattr(os, "fchmod", None)
    if callable(descriptor_chmod):
        descriptor_chmod(descriptor, mode)
    elif os.name == "nt":
        os.chmod(path, mode)
    else:
        fail(f"descriptor chmod is unavailable for {path.name}")

    after_descriptor = os.fstat(descriptor)
    after_path = path.lstat()
    if (
        path.is_symlink()
        or not same_regular_file_identity(before_descriptor, after_descriptor)
        or not same_regular_file_identity(after_descriptor, after_path)
    ):
        fail(f"open file identity changed during chmod for {path.name}")
    if os.name == "nt":
        if not windows_permissions_match_mode(after_descriptor, mode) or not \
           windows_permissions_match_mode(after_path, mode):
            fail(f"Windows file permissions differ after chmod for {path.name}")
    elif stat.S_IMODE(after_descriptor.st_mode) != mode or \
         stat.S_IMODE(after_path.st_mode) != mode:
        fail(f"POSIX file permissions differ after chmod for {path.name}")


def require_signer_input_read_only(metadata: os.stat_result) -> None:
    if not signer_input_permissions_are_read_only(metadata, windows=os.name == "nt"):
        fail("a signer input regained permissions before verification")


def payload_files(platform: str, root: Path) -> list[tuple[str, Path, int]]:
    if not root.is_dir() or root.is_symlink():
        fail("unsigned payload must be a non-symlink directory")
    expected = UNSIGNED_FILES[platform]
    try:
        actual = sorted(path.name for path in root.iterdir())
    except OSError as error:
        fail(f"cannot enumerate unsigned payload: {error}")
    if actual != sorted(expected):
        fail(
            "unsigned payload membership differs from the exact platform "
            f"allowlist: expected {sorted(expected)}, got {actual}"
        )
    result: list[tuple[str, Path, int]] = []
    total = 0
    for name in expected:
        path = root / name
        metadata = require_regular(
            path, label="unsigned payload member", maximum=MAX_PAYLOAD_FILE_BYTES
        )
        total += metadata.st_size
        if total > MAX_PAYLOAD_BYTES:
            fail("unsigned payload exceeds the aggregate size bound")
        # AppImage must be executable after release assembly. Every other
        # normalized release asset is data. These canonical modes are carried
        # in the signed handoff metadata, while materialization deliberately
        # strips execution permission in the key-bearing workspace.
        mode = 0o755 if name.endswith(".AppImage") else 0o644
        result.append((name, path, mode))
    return result


def add_tar_file(
    archive: tarfile.TarFile,
    source: Path,
    arcname: str,
    mode: int,
) -> None:
    metadata = require_regular(source, label=arcname, maximum=MAX_PAYLOAD_FILE_BYTES)
    info = tarfile.TarInfo(arcname)
    info.size = metadata.st_size
    info.mode = mode
    info.mtime = 0
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    with source.open("rb") as handle:
        archive.addfile(info, handle)


def write_exclusive(path: Path, payload: bytes, mode: int) -> None:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(path, flags, mode)
    try:
        offset = 0
        while offset < len(payload):
            written = os.write(descriptor, payload[offset:])
            if written <= 0:
                fail(f"write made no progress for {path.name}")
            offset += written
        set_open_file_mode(path, descriptor, mode)
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def package(args: argparse.Namespace) -> None:
    validate_scalars(args)
    payload = payload_files(args.platform, args.payload_dir)
    if args.stage_dir.exists():
        fail("stage directory already exists")
    args.stage_dir.mkdir(parents=True, mode=0o700)

    hashes = {name: sha256(path) for name, path, _ in payload}
    modes = {name: mode for name, _, mode in payload}
    metadata = {
        "schema": SCHEMA,
        "repository": args.repository,
        "commit": args.commit,
        "workflow_run_id": args.run_id,
        "workflow_run_attempt": args.run_attempt,
        "platform": args.platform,
        "rust_target": args.rust_target,
        "version": args.version,
        "files": hashes,
        "modes": modes,
        "updater_file": UPDATER_FILES[args.platform],
    }
    metadata_bytes = canonical_json(metadata)
    metadata_sha = hashlib.sha256(metadata_bytes).hexdigest()
    metadata_path = args.stage_dir / "BUILD-METADATA.json"
    write_exclusive(metadata_path, metadata_bytes, 0o600)

    stem = (
        f"arc-unsigned-desktop-{args.platform}-{args.commit}-"
        f"{args.run_id}-{args.run_attempt}"
    )
    archive_name = f"{stem}.tar.gz"
    archive_path = args.stage_dir / archive_name
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(archive_path, flags, 0o600)
    try:
        with os.fdopen(descriptor, "wb", closefd=False) as raw:
            with tarfile.open(
                fileobj=raw, mode="w:gz", format=tarfile.USTAR_FORMAT
            ) as archive:
                for name, path, mode in payload:
                    add_tar_file(archive, path, name, mode)
                info = tarfile.TarInfo("BUILD-METADATA.json")
                info.size = len(metadata_bytes)
                info.mode = 0o600
                info.mtime = 0
                info.uid = 0
                info.gid = 0
                info.uname = ""
                info.gname = ""
                archive.addfile(info, fileobj=_BytesReader(metadata_bytes))
            raw.flush()
            os.fsync(raw.fileno())
    finally:
        os.close(descriptor)
    require_regular(archive_path, label="handoff archive", maximum=MAX_ARCHIVE_BYTES)
    archive_sha = sha256(archive_path)

    checksum_lines = (
        CHECKSUM_HEADER,
        f"# repository={args.repository}",
        f"# commit={args.commit}",
        f"# run_id={args.run_id}",
        f"# run_attempt={args.run_attempt}",
        f"# platform={args.platform}",
        f"# rust_target={args.rust_target}",
        f"# version={args.version}",
        f"# metadata_sha256={metadata_sha}",
        f"{archive_sha}  {archive_name}",
        "",
    )
    checksums = args.stage_dir / "SHA256SUMS"
    write_exclusive(checksums, "\n".join(checksum_lines).encode(), 0o600)
    os.chmod(metadata_path, 0o400)
    os.chmod(archive_path, 0o400)
    os.chmod(checksums, 0o400)

    artifact_name = f"{stem}-{archive_sha}"
    if args.github_output:
        with args.github_output.open("a", encoding="utf-8") as handle:
            handle.write(f"artifact_name={artifact_name}\n")
            handle.write(f"archive_name={archive_name}\n")
            handle.write(f"archive_sha={archive_sha}\n")
            handle.write(f"metadata_sha={metadata_sha}\n")
    print(artifact_name)


class _BytesReader:
    def __init__(self, payload: bytes) -> None:
        self.payload = payload
        self.offset = 0

    def read(self, size: int = -1) -> bytes:
        if size < 0:
            size = len(self.payload) - self.offset
        chunk = self.payload[self.offset : self.offset + size]
        self.offset += len(chunk)
        return chunk


def load_api(path: Path) -> list[dict]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot read artifact API response: {error}")
    artifacts = value.get("artifacts") if isinstance(value, dict) else value
    if not isinstance(artifacts, list):
        fail("artifact API response must contain an artifacts array")
    return artifacts


def select(args: argparse.Namespace) -> None:
    validate_scalars(args)
    stem = (
        f"arc-unsigned-desktop-{args.platform}-{args.commit}-"
        f"{args.run_id}-{args.run_attempt}"
    )
    pattern = re.compile(re.escape(stem) + r"-([0-9a-f]{64})")
    matches: list[tuple[dict, str]] = []
    for artifact in load_api(args.api_json):
        if not isinstance(artifact, dict) or not isinstance(artifact.get("name"), str):
            fail("artifact API entry is not a named object")
        match = pattern.fullmatch(artifact["name"])
        if match:
            matches.append((artifact, match.group(1)))
    if len(matches) != 1:
        fail(
            "expected exactly one unsigned desktop artifact for the exact "
            f"platform/run/attempt; found {len(matches)}"
        )
    artifact, archive_sha = matches[0]
    artifact_id = artifact.get("id")
    digest = artifact.get("digest")
    size = artifact.get("size_in_bytes")
    if not isinstance(artifact_id, int) or artifact_id <= 0:
        fail("unsigned artifact has an invalid immutable ID")
    if artifact.get("expired") is not False:
        fail("unsigned artifact is expired or has unknown expiry state")
    if not isinstance(size, int) or size <= 0 or size > MAX_ACTIONS_ZIP_BYTES:
        fail("unsigned artifact size is outside the bounded contract")
    if not isinstance(digest, str) or re.fullmatch(r"sha256:[0-9a-f]{64}", digest) is None:
        fail("unsigned artifact has no exact server SHA-256 digest")
    workflow_run = artifact.get("workflow_run")
    if isinstance(workflow_run, dict) and workflow_run.get("id") not in (
        None,
        args.run_id,
    ):
        fail("unsigned artifact belongs to another workflow run")
    selected = {
        "id": artifact_id,
        "name": artifact["name"],
        "digest": digest,
        "size_in_bytes": size,
        "archive_sha256": archive_sha,
    }
    compact = json.dumps(selected, sort_keys=True, separators=(",", ":"))
    if args.github_output:
        with args.github_output.open("a", encoding="utf-8") as handle:
            handle.write(f"artifact_id={artifact_id}\n")
            handle.write(f"artifact_digest={digest}\n")
            handle.write(f"archive_sha={archive_sha}\n")
            handle.write(f"artifact_json={compact}\n")
    print(json.dumps(selected, sort_keys=True, indent=2))


def parse_selected(raw: str) -> dict:
    try:
        selected = json.loads(raw)
    except json.JSONDecodeError as error:
        fail(f"artifact selection JSON is invalid: {error}")
    if not isinstance(selected, dict) or set(selected) != {
        "id",
        "name",
        "digest",
        "size_in_bytes",
        "archive_sha256",
    }:
        fail("artifact selection JSON has the wrong fields")
    if not isinstance(selected["id"], int) or selected["id"] <= 0:
        fail("artifact selection has an invalid ID")
    if re.fullmatch(r"sha256:[0-9a-f]{64}", str(selected["digest"])) is None:
        fail("artifact selection has an invalid server digest")
    if (
        not isinstance(selected["size_in_bytes"], int)
        or selected["size_in_bytes"] <= 0
        or selected["size_in_bytes"] > MAX_ACTIONS_ZIP_BYTES
    ):
        fail("artifact selection has an invalid size")
    if re.fullmatch(r"[0-9a-f]{64}", str(selected["archive_sha256"])) is None:
        fail("artifact selection has an invalid archive digest")
    return selected


def raw_actions_zip(root: Path, artifact_name: str) -> Path:
    named = root / artifact_name
    source = named if named.is_dir() and not named.is_symlink() else root
    if not source.is_dir() or source.is_symlink():
        fail("downloaded artifact directory is missing or symlinked")
    entries = list(source.iterdir())
    if len(entries) != 1:
        fail("raw Actions artifact directory must contain exactly one ZIP")
    path = entries[0]
    require_regular(path, label="raw Actions artifact ZIP", maximum=MAX_ACTIONS_ZIP_BYTES)
    return path


def extract_outer(path: Path, selected: dict, archive_name: str, root: Path) -> None:
    if path.stat().st_size != selected["size_in_bytes"]:
        fail("downloaded Actions ZIP size differs from the selected artifact")
    if sha256(path) != selected["digest"].removeprefix("sha256:"):
        fail("downloaded Actions ZIP does not match selected artifact.digest")
    expected = {"SHA256SUMS", archive_name}
    try:
        with zipfile.ZipFile(path, "r") as archive:
            infos = archive.infolist()
            if len(infos) != len(expected):
                fail("Actions ZIP entry count differs from the handoff contract")
            names: set[str] = set()
            expanded = 0
            for info in infos:
                pure = PurePosixPath(info.filename)
                mode_type = (info.external_attr >> 16) & 0o170000
                if (
                    not safe_relative(pure)
                    or len(pure.parts) != 1
                    or info.is_dir()
                    or info.flag_bits & 0x1
                    or mode_type not in (0, 0o100000)
                    or info.file_size <= 0
                ):
                    fail("Actions ZIP contains an unsafe or non-regular entry")
                if info.filename in names:
                    fail("Actions ZIP contains a duplicate entry")
                names.add(info.filename)
                expanded += info.file_size
            if names != expected:
                fail("Actions ZIP membership differs from the handoff contract")
            limit = min(
                MAX_ARCHIVE_BYTES + MAX_METADATA_BYTES,
                path.stat().st_size * MAX_EXPANSION_RATIO + EXPANSION_SLACK_BYTES,
            )
            if expanded > limit:
                fail("Actions ZIP exceeds the expansion bound")
            for info in infos:
                target = root / info.filename
                with archive.open(info, "r") as source, target.open("xb") as output:
                    shutil.copyfileobj(source, output)
    except (OSError, zipfile.BadZipFile) as error:
        fail(f"cannot read selected Actions artifact ZIP: {error}")


def parse_checksums(path: Path) -> tuple[dict[str, str], str, str]:
    require_regular(path, label="checksum manifest", maximum=MAX_METADATA_BYTES)
    lines = path.read_text(encoding="utf-8").splitlines()
    if len(lines) != 10 or lines[0] != CHECKSUM_HEADER:
        fail("checksum manifest has an invalid line contract")
    headers: dict[str, str] = {}
    for line in lines[1:9]:
        match = re.fullmatch(r"# ([a-z0-9_]+)=(.+)", line)
        if match is None or match.group(1) in headers:
            fail("checksum manifest has an invalid header")
        headers[match.group(1)] = match.group(2)
    record = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9_.-]+\.tar\.gz)", lines[9])
    if record is None:
        fail("checksum manifest has an invalid archive record")
    return headers, record.group(1), record.group(2)


def safe_tar_members(
    archive: tarfile.TarFile, platform: str
) -> dict[str, tarfile.TarInfo]:
    expected_files = set(UNSIGNED_FILES[platform])
    members: dict[str, tarfile.TarInfo] = {}
    total = 0
    for member in archive.getmembers():
        pure = PurePosixPath(member.name)
        allowed = member.name in expected_files or member.name == "BUILD-METADATA.json"
        if (
            not allowed
            or not safe_relative(pure)
            or not member.isfile()
            or member.issym()
            or member.islnk()
            or member.pax_headers
            or member.size <= 0
            or member.name in members
        ):
            fail("handoff archive contains an unsafe or duplicate member")
        expected_mode = (
            0o600
            if member.name == "BUILD-METADATA.json"
            else 0o755
            if member.name.endswith(".AppImage")
            else 0o644
        )
        if member.mode & 0o777 != expected_mode:
            fail("handoff archive member has an unexpected mode")
        maximum = (
            MAX_METADATA_BYTES
            if member.name == "BUILD-METADATA.json"
            else MAX_PAYLOAD_FILE_BYTES
        )
        if member.size > maximum:
            fail("handoff archive member exceeds its size bound")
        members[member.name] = member
        total += member.size
    if total > MAX_PAYLOAD_BYTES + MAX_METADATA_BYTES:
        fail("handoff archive exceeds the aggregate payload bound")
    if set(members) != expected_files | {"BUILD-METADATA.json"}:
        fail("handoff archive membership differs from the platform allowlist")
    return members


def copy_stream(source: BinaryIO, target: Path, mode: int) -> None:
    target.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(target, flags, mode)
    try:
        with os.fdopen(descriptor, "wb", closefd=False) as output:
            shutil.copyfileobj(source, output)
            output.flush()
            os.fsync(output.fileno())
        set_open_file_mode(target, descriptor, mode)
    finally:
        os.close(descriptor)


def materialize(args: argparse.Namespace) -> None:
    validate_scalars(args)
    selected = parse_selected(args.artifact_json)
    if selected["id"] != args.expected_artifact_id:
        fail("artifact selection ID differs from the immutable download ID")
    if selected["digest"] != args.expected_artifact_digest:
        fail("artifact selection digest differs from the download digest")
    if selected["archive_sha256"] != args.expected_archive_sha256:
        fail("artifact selection archive hash differs from the handoff output")
    stem = (
        f"arc-unsigned-desktop-{args.platform}-{args.commit}-"
        f"{args.run_id}-{args.run_attempt}"
    )
    expected_name = f"{stem}-{selected['archive_sha256']}"
    if selected["name"] != expected_name:
        fail("artifact selection name is not commit/run/archive-hash bound")
    archive_name = f"{stem}.tar.gz"
    artifact_zip = raw_actions_zip(args.downloads_root, expected_name)

    if args.output_dir.exists():
        fail("materialization output already exists")
    with tempfile.TemporaryDirectory(prefix="arc-unsigned-desktop-verify.") as temporary:
        temporary_root = Path(temporary)
        extract_outer(artifact_zip, selected, archive_name, temporary_root)
        headers, manifest_sha, manifest_archive = parse_checksums(
            temporary_root / "SHA256SUMS"
        )
        archive_path = temporary_root / archive_name
        require_regular(archive_path, label="handoff archive", maximum=MAX_ARCHIVE_BYTES)
        expected_headers = {
            "repository": args.repository,
            "commit": args.commit,
            "run_id": str(args.run_id),
            "run_attempt": str(args.run_attempt),
            "platform": args.platform,
            "rust_target": args.rust_target,
            "version": args.version,
        }
        metadata_sha = headers.pop("metadata_sha256", None)
        if headers != expected_headers or re.fullmatch(r"[0-9a-f]{64}", str(metadata_sha)) is None:
            fail("checksum headers differ from the requested handoff")
        if manifest_archive != archive_name:
            fail("checksum manifest names the wrong handoff archive")
        actual_archive_sha = sha256(archive_path)
        if (
            manifest_sha != selected["archive_sha256"]
            or actual_archive_sha != manifest_sha
        ):
            fail("handoff archive SHA-256 mismatch")

        try:
            with tarfile.open(archive_path, "r:gz") as archive:
                members = safe_tar_members(archive, args.platform)
                metadata_member = members["BUILD-METADATA.json"]
                metadata_handle = archive.extractfile(metadata_member)
                if metadata_handle is None:
                    fail("cannot read handoff build metadata")
                metadata_bytes = metadata_handle.read(MAX_METADATA_BYTES + 1)
                if len(metadata_bytes) > MAX_METADATA_BYTES:
                    fail("handoff build metadata exceeds its size bound")
                if hashlib.sha256(metadata_bytes).hexdigest() != metadata_sha:
                    fail("handoff build metadata digest mismatch")
                try:
                    metadata = json.loads(metadata_bytes)
                except json.JSONDecodeError as error:
                    fail(f"handoff build metadata is invalid JSON: {error}")
                if canonical_json(metadata) != metadata_bytes:
                    fail("handoff build metadata is not canonical JSON")
                expected_metadata = {
                    "schema": SCHEMA,
                    "repository": args.repository,
                    "commit": args.commit,
                    "workflow_run_id": args.run_id,
                    "workflow_run_attempt": args.run_attempt,
                    "platform": args.platform,
                    "rust_target": args.rust_target,
                    "version": args.version,
                    "updater_file": UPDATER_FILES[args.platform],
                }
                for field, expected in expected_metadata.items():
                    if metadata.get(field) != expected:
                        fail(f"handoff build metadata field {field} differs")
                hashes = metadata.get("files")
                modes = metadata.get("modes")
                payload_names = set(members) - {"BUILD-METADATA.json"}
                if not isinstance(hashes, dict) or set(hashes) != payload_names:
                    fail("handoff metadata file set differs from the archive")
                expected_modes = {
                    name: 0o755 if name.endswith(".AppImage") else 0o644
                    for name in payload_names
                }
                if modes != expected_modes:
                    fail("handoff metadata modes differ from normalized release modes")
                for name in sorted(payload_names):
                    expected_hash = hashes.get(name)
                    if re.fullmatch(r"[0-9a-f]{64}", str(expected_hash)) is None:
                        fail("handoff metadata contains an invalid payload hash")
                    handle = archive.extractfile(members[name])
                    if handle is None:
                        fail("cannot read handoff payload member")
                    digest = hashlib.sha256()
                    for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                        digest.update(chunk)
                    if digest.hexdigest() != expected_hash:
                        fail("handoff payload hash differs from build metadata")

                args.output_dir.mkdir(parents=True, mode=0o700)
                for name in sorted(payload_names):
                    handle = archive.extractfile(members[name])
                    if handle is None:
                        fail("cannot materialize handoff payload member")
                    # Nothing produced by source compilation is executable in
                    # the signing workspace. Release modes are restored only
                    # after the signing-key step has exited and the exact input
                    # hashes and signature have been verified.
                    copy_stream(handle, args.output_dir / name, 0o400)
        except (OSError, tarfile.TarError) as error:
            fail(f"cannot read selected handoff archive: {error}")

    receipt = {
        "schema": "arc.unsigned-desktop-materialization.v1",
        "repository": args.repository,
        "commit": args.commit,
        "workflow_run_id": args.run_id,
        "workflow_run_attempt": args.run_attempt,
        "platform": args.platform,
        "rust_target": args.rust_target,
        "version": args.version,
        "artifact": selected,
        "metadata_sha256": metadata_sha,
        "files": hashes,
        "modes": modes,
        "updater_file": UPDATER_FILES[args.platform],
    }
    write_exclusive(
        args.output_dir / "HANDOFF-RECEIPT.json", canonical_json(receipt), 0o444
    )
    print(f"verified unsigned desktop handoff artifact ID {selected['id']}")


def verify_signed(args: argparse.Namespace) -> None:
    """Verify the signer changed only the expected signature and restore modes."""

    validate_scalars(args)
    root = args.workspace
    if not root.is_dir() or root.is_symlink():
        fail("signed workspace must be a non-symlink directory")
    receipt_path = root / "HANDOFF-RECEIPT.json"
    require_regular(receipt_path, label="handoff receipt", maximum=MAX_METADATA_BYTES)
    try:
        receipt_bytes = receipt_path.read_bytes()
        receipt = json.loads(receipt_bytes)
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot read handoff receipt: {error}")
    if not isinstance(receipt, dict) or canonical_json(receipt) != receipt_bytes:
        fail("handoff receipt is not canonical JSON")
    expected_receipt = {
        "schema": "arc.unsigned-desktop-materialization.v1",
        "repository": args.repository,
        "commit": args.commit,
        "workflow_run_id": args.run_id,
        "workflow_run_attempt": args.run_attempt,
        "platform": args.platform,
        "rust_target": args.rust_target,
        "version": args.version,
    }
    for field, expected in expected_receipt.items():
        if receipt.get(field) != expected:
            fail(f"handoff receipt field {field} differs")

    expected_payload = set(UNSIGNED_FILES[args.platform])
    updater_file = UPDATER_FILES[args.platform]
    signature_name = f"{updater_file}.sig"
    expected_members = expected_payload | {"HANDOFF-RECEIPT.json", signature_name}
    try:
        actual_members = {path.name for path in root.iterdir()}
    except OSError as error:
        fail(f"cannot enumerate signed workspace: {error}")
    if actual_members != expected_members:
        fail(
            "signed workspace membership differs: expected "
            f"{sorted(expected_members)}, got {sorted(actual_members)}"
        )

    hashes = receipt.get("files")
    modes = receipt.get("modes")
    expected_modes = {
        name: 0o755 if name.endswith(".AppImage") else 0o644
        for name in expected_payload
    }
    if (
        not isinstance(hashes, dict)
        or set(hashes) != expected_payload
        or modes != expected_modes
        or receipt.get("updater_file") != updater_file
    ):
        fail("handoff receipt payload contract differs")
    for name in sorted(expected_payload):
        path = root / name
        metadata = require_regular(
            path, label="signed-workspace input", maximum=MAX_PAYLOAD_FILE_BYTES
        )
        require_signer_input_read_only(metadata)
        expected_hash = hashes[name]
        if re.fullmatch(r"[0-9a-f]{64}", str(expected_hash)) is None:
            fail("handoff receipt has an invalid payload hash")
        if sha256(path) != expected_hash:
            fail("signer changed an input payload")

    signature_path = root / signature_name
    require_regular(signature_path, label="updater signature", maximum=MAX_METADATA_BYTES)

    # Restore normalized release modes only after the key-bearing command has
    # ended and all source-generated inputs are proven byte-identical.
    for name, mode in expected_modes.items():
        os.chmod(root / name, mode)
    os.chmod(signature_path, 0o644)
    os.chmod(receipt_path, 0o444)
    if args.github_output:
        with args.github_output.open("a", encoding="utf-8") as handle:
            handle.write(f"updater_file={updater_file}\n")
            handle.write(f"signature_file={signature_name}\n")
    print(f"verified signed desktop handoff for {args.platform}")


def common(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--run-id", required=True, type=int)
    parser.add_argument("--run-attempt", required=True, type=int)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--rust-target", required=True)
    parser.add_argument("--version", required=True)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    package_parser = commands.add_parser("package")
    common(package_parser)
    package_parser.add_argument("--payload-dir", required=True, type=Path)
    package_parser.add_argument("--stage-dir", required=True, type=Path)
    package_parser.add_argument("--github-output", type=Path)
    select_parser = commands.add_parser("select")
    common(select_parser)
    select_parser.add_argument("--api-json", required=True, type=Path)
    select_parser.add_argument("--github-output", type=Path)
    materialize_parser = commands.add_parser("materialize")
    common(materialize_parser)
    materialize_parser.add_argument("--artifact-json", required=True)
    materialize_parser.add_argument("--expected-artifact-id", required=True, type=int)
    materialize_parser.add_argument("--expected-artifact-digest", required=True)
    materialize_parser.add_argument("--expected-archive-sha256", required=True)
    materialize_parser.add_argument("--downloads-root", required=True, type=Path)
    materialize_parser.add_argument("--output-dir", required=True, type=Path)
    verify_parser = commands.add_parser("verify-signed")
    common(verify_parser)
    verify_parser.add_argument("--workspace", required=True, type=Path)
    verify_parser.add_argument("--github-output", type=Path)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.command == "package":
        package(args)
    elif args.command == "select":
        select(args)
    elif args.command == "materialize":
        materialize(args)
    else:
        verify_signed(args)


if __name__ == "__main__":
    main()
