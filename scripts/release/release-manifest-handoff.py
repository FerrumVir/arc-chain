#!/usr/bin/env python3
"""Stage and verify exact ARC release assets across isolated signing jobs."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import stat
from pathlib import Path
from typing import NoReturn


SCHEMA = "arc.release-manifest-handoff.v1"
HANDOFF_NAME = "ARC-RELEASE-HANDOFF.json"
HEADLESS = (
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
)
DESKTOP = (
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
)
BASE_FILES = HEADLESS + DESKTOP + (
    "install.sh",
    "testnet-seeds.txt",
    "genesis.toml",
    "latest.json",
    "SHA256SUMS",
)
MAX_FILE_BYTES = 4 * 1024 * 1024 * 1024
MAX_TOTAL_BYTES = 12 * 1024 * 1024 * 1024
MAX_METADATA_BYTES = 4 * 1024 * 1024


def fail(message: str) -> NoReturn:
    raise SystemExit(f"release manifest handoff: {message}")


def canonical_json(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def regular(path: Path, label: str, maximum: int = MAX_FILE_BYTES) -> os.stat_result:
    try:
        metadata = path.lstat()
    except OSError as error:
        fail(f"cannot inspect {label}: {error}")
    if not stat.S_ISREG(metadata.st_mode) or path.is_symlink():
        fail(f"{label} must be a non-symlink regular file")
    if metadata.st_size <= 0 or metadata.st_size > maximum:
        fail(f"{label} size is outside the bounded contract")
    return metadata


def validate_identity(args: argparse.Namespace) -> None:
    if args.repository != "FerrumVir/arc-chain":
        fail("unexpected repository")
    if re.fullmatch(r"[0-9a-f]{40}", args.commit) is None:
        fail("commit must be one full lowercase Git SHA")
    if re.fullmatch(r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", args.tag) is None:
        fail("tag must be strict vMAJOR.MINOR.PATCH")
    if args.run_id <= 0 or args.run_attempt <= 0:
        fail("run ID and attempt must be positive")


def normalized_mode(name: str) -> int:
    if name == "install.sh" or name.endswith(".AppImage"):
        return 0o755
    if name in HEADLESS and not name.endswith(".exe"):
        return 0o755
    return 0o644


def expected_files(sealed: bool) -> tuple[str, ...]:
    return BASE_FILES + (("SHA256SUMS.sig",) if sealed else ())


def validate_manifest(root: Path, args: argparse.Namespace) -> dict[str, str]:
    manifest = root / "SHA256SUMS"
    regular(manifest, "SHA256SUMS", MAX_METADATA_BYTES)
    try:
        lines = manifest.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        fail(f"cannot read SHA256SUMS: {error}")
    expected_header = (
        "# ARC release manifest v1",
        f"# repository={args.repository}",
        f"# tag={args.tag}",
        f"# commit={args.commit}",
    )
    if tuple(lines[:4]) != expected_header:
        fail("SHA256SUMS identity header differs")
    records: dict[str, str] = {}
    for line in lines[4:]:
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9_.-]+)", line)
        if match is None or match.group(2) in records:
            fail("SHA256SUMS contains an invalid or duplicate record")
        records[match.group(2)] = match.group(1)
    expected_records = set(BASE_FILES) - {"SHA256SUMS"}
    if set(records) != expected_records:
        fail("SHA256SUMS record set differs from the release allowlist")
    for name, expected_hash in records.items():
        if sha256(root / name) != expected_hash:
            fail(f"SHA256SUMS digest differs for {name}")
    return records


def validate_release(root: Path, args: argparse.Namespace, sealed: bool) -> tuple[dict, dict]:
    if not root.is_dir() or root.is_symlink():
        fail("release-files must be a non-symlink directory")
    expected = set(expected_files(sealed))
    try:
        actual = {path.name for path in root.iterdir()}
    except OSError as error:
        fail(f"cannot enumerate release-files: {error}")
    if actual != expected:
        fail(f"release membership differs: expected {sorted(expected)}, got {sorted(actual)}")
    total = 0
    hashes: dict[str, str] = {}
    modes: dict[str, int] = {}
    for name in sorted(expected):
        metadata = regular(
            root / name,
            name,
            MAX_METADATA_BYTES if name.endswith(".sig") or name.endswith(".json") else MAX_FILE_BYTES,
        )
        total += metadata.st_size
        if total > MAX_TOTAL_BYTES:
            fail("release payload exceeds the aggregate size bound")
        hashes[name] = sha256(root / name)
        modes[name] = normalized_mode(name)
    records = validate_manifest(root, args)
    if "SHA256SUMS.sig" in records:
        fail("detached manifest signature must not be self-hashed")
    return hashes, modes


def copy_exclusive(source: Path, target: Path, mode: int) -> None:
    target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(target, flags, mode)
    try:
        with source.open("rb") as input_file, os.fdopen(descriptor, "wb", closefd=False) as output:
            shutil.copyfileobj(input_file, output)
            output.flush()
            os.fsync(output.fileno())
        os.fchmod(descriptor, mode)
    finally:
        os.close(descriptor)


def stage(args: argparse.Namespace) -> None:
    validate_identity(args)
    hashes, modes = validate_release(args.source_dir, args, args.sealed)
    if args.stage_dir.exists():
        fail("stage directory already exists")
    release_dir = args.stage_dir / "release-files"
    release_dir.mkdir(parents=True, mode=0o700)
    for name in sorted(expected_files(args.sealed)):
        copy_exclusive(args.source_dir / name, release_dir / name, modes[name])
    metadata = {
        "schema": SCHEMA,
        "sealed": args.sealed,
        "repository": args.repository,
        "commit": args.commit,
        "tag": args.tag,
        "workflow_run_id": args.run_id,
        "workflow_run_attempt": args.run_attempt,
        "files": hashes,
        "modes": modes,
        "manifest_sha256": hashes["SHA256SUMS"],
    }
    metadata_bytes = canonical_json(metadata)
    metadata_sha = hashlib.sha256(metadata_bytes).hexdigest()
    copy_target = args.stage_dir / HANDOFF_NAME
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    descriptor = os.open(copy_target, flags, 0o444)
    try:
        offset = 0
        while offset < len(metadata_bytes):
            written = os.write(descriptor, metadata_bytes[offset:])
            if written <= 0:
                fail("metadata write made no progress")
            offset += written
        os.fsync(descriptor)
    finally:
        os.close(descriptor)
    kind = "sealed" if args.sealed else "unsigned"
    artifact_name = (
        f"arc-release-{kind}-handoff-{args.commit}-{args.run_id}-"
        f"{args.run_attempt}-{metadata_sha}"
    )
    if args.github_output:
        with args.github_output.open("a", encoding="utf-8") as output:
            output.write(f"artifact_name={artifact_name}\n")
            output.write(f"metadata_sha={metadata_sha}\n")
            output.write(f"manifest_sha={hashes['SHA256SUMS']}\n")
    print(artifact_name)


def verify(args: argparse.Namespace) -> None:
    validate_identity(args)
    root = args.handoff_dir
    if not root.is_dir() or root.is_symlink():
        fail("handoff root must be a non-symlink directory")
    if {path.name for path in root.iterdir()} != {HANDOFF_NAME, "release-files"}:
        fail("handoff root membership differs")
    metadata_path = root / HANDOFF_NAME
    regular(metadata_path, HANDOFF_NAME, MAX_METADATA_BYTES)
    metadata_bytes = metadata_path.read_bytes()
    if hashlib.sha256(metadata_bytes).hexdigest() != args.expected_metadata_sha:
        fail("handoff metadata hash differs")
    try:
        metadata = json.loads(metadata_bytes)
    except json.JSONDecodeError as error:
        fail(f"handoff metadata is invalid JSON: {error}")
    if canonical_json(metadata) != metadata_bytes:
        fail("handoff metadata is not canonical")
    expected_identity = {
        "schema": SCHEMA,
        "sealed": args.sealed,
        "repository": args.repository,
        "commit": args.commit,
        "tag": args.tag,
        "workflow_run_id": args.run_id,
        "workflow_run_attempt": args.run_attempt,
    }
    for field, expected in expected_identity.items():
        if metadata.get(field) != expected:
            fail(f"handoff metadata field {field} differs")
    hashes, modes = validate_release(root / "release-files", args, args.sealed)
    if metadata.get("files") != hashes or metadata.get("modes") != modes:
        fail("handoff metadata file contract differs")
    if metadata.get("manifest_sha256") != hashes["SHA256SUMS"]:
        fail("handoff metadata manifest digest differs")
    for name, mode in modes.items():
        os.chmod(root / "release-files" / name, mode)
    os.chmod(metadata_path, 0o444)
    print(f"verified {'sealed' if args.sealed else 'unsigned'} release handoff")


def common(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--run-id", required=True, type=int)
    parser.add_argument("--run-attempt", required=True, type=int)
    parser.add_argument("--sealed", action="store_true")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    stage_parser = commands.add_parser("stage")
    common(stage_parser)
    stage_parser.add_argument("--source-dir", required=True, type=Path)
    stage_parser.add_argument("--stage-dir", required=True, type=Path)
    stage_parser.add_argument("--github-output", type=Path)
    verify_parser = commands.add_parser("verify")
    common(verify_parser)
    verify_parser.add_argument("--handoff-dir", required=True, type=Path)
    verify_parser.add_argument("--expected-metadata-sha", required=True)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if re.fullmatch(r"[0-9a-f]{64}", getattr(args, "expected_metadata_sha", "0" * 64)) is None:
        fail("expected metadata SHA-256 is invalid")
    if args.command == "stage":
        stage(args)
    else:
        verify(args)


if __name__ == "__main__":
    main()
