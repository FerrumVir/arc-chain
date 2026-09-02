#!/usr/bin/env python3
"""Create one hash-bound, create-only ARC pre-tag candidate archive."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import tarfile
from pathlib import Path


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


def fail(message: str) -> "NoReturn":
    raise SystemExit(f"pre-tag artifact packaging: {message}")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--kind", required=True, choices=("headless", "desktop"))
    parser.add_argument("--platform", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--run-id", required=True, type=int)
    parser.add_argument("--run-attempt", required=True, type=int)
    parser.add_argument("--version", required=True)
    parser.add_argument("--rust-target", required=True)
    parser.add_argument("--payload-dir", required=True, type=Path)
    parser.add_argument("--stage-dir", required=True, type=Path)
    parser.add_argument("--github-output", type=Path)
    return parser.parse_args()


def expected_files(kind: str, platform: str) -> tuple[str, ...]:
    if kind == "desktop":
        try:
            return DESKTOP_FILES[platform]
        except KeyError:
            fail(f"unsupported desktop platform {platform!r}")
    suffix = ".exe" if platform == "windows-x86_64" else ""
    return (
        f"arc-node-{platform}{suffix}",
        f"arc-cli-{platform}{suffix}",
        "genesis.toml",
    )


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
    expected_target = RUST_TARGETS.get(args.platform)
    if expected_target is None or args.rust_target != expected_target:
        fail(
            f"platform {args.platform!r} requires Rust target "
            f"{expected_target!r}, got {args.rust_target!r}"
        )
    if args.kind == "desktop" and args.platform not in DESKTOP_FILES:
        fail(f"unsupported desktop platform {args.platform!r}")
    if not args.payload_dir.is_dir() or args.payload_dir.is_symlink():
        fail("payload directory must be a real directory")
    if args.stage_dir.exists() and args.stage_dir.is_symlink():
        fail("stage directory must not be a symlink")
    args.stage_dir.mkdir(parents=True, exist_ok=True)

    required = expected_files(args.kind, args.platform)
    actual = sorted(path.name for path in args.payload_dir.iterdir())
    if actual != sorted(required):
        fail(f"payload membership differs: expected {sorted(required)}, got {actual}")
    for name in required:
        path = args.payload_dir / name
        if not path.is_file() or path.is_symlink() or path.stat().st_size <= 0:
            fail(f"payload is empty, non-regular, or symlinked: {name}")

    files = {name: sha256(args.payload_dir / name) for name in sorted(required)}
    metadata = {
        "schema": "arc.pretag.artifact.v1",
        "kind": args.kind,
        "repository": args.repository,
        "commit": args.commit,
        "platform": args.platform,
        "rust_target": args.rust_target,
        "version": args.version,
        "workflow_run_id": args.run_id,
        "workflow_run_attempt": args.run_attempt,
        "files": files,
    }
    metadata_path = args.payload_dir / "BUILD-METADATA.json"
    metadata_path.write_text(
        json.dumps(metadata, sort_keys=True, indent=2) + "\n", encoding="utf-8"
    )

    stem = (
        f"arc-pretag-{args.kind}-{args.platform}-{args.commit}-"
        f"{args.run_id}-{args.run_attempt}"
    )
    archive_name = f"{stem}.tar.gz"
    archive_path = args.stage_dir / archive_name
    if archive_path.exists():
        fail(f"refusing to replace archive {archive_path}")
    with tarfile.open(archive_path, "w:gz", format=tarfile.PAX_FORMAT) as archive:
        for name in (*sorted(required), "BUILD-METADATA.json"):
            archive.add(args.payload_dir / name, arcname=name, recursive=False)

    archive_sha256 = sha256(archive_path)
    checksums_path = args.stage_dir / "SHA256SUMS"
    if checksums_path.exists():
        fail(f"refusing to replace checksum manifest {checksums_path}")
    checksums_path.write_text(
        "\n".join(
            (
                "# ARC pre-tag artifact v1",
                f"# kind={args.kind}",
                f"# repository={args.repository}",
                f"# commit={args.commit}",
                f"# run_id={args.run_id}",
                f"# run_attempt={args.run_attempt}",
                f"# platform={args.platform}",
                f"{archive_sha256}  {archive_name}",
                "",
            )
        ),
        encoding="utf-8",
    )
    artifact_name = f"{stem}-{archive_sha256}"
    if args.github_output:
        with args.github_output.open("a", encoding="utf-8") as handle:
            handle.write(f"artifact_name={artifact_name}\n")
            handle.write(f"archive_sha={archive_sha256}\n")
            handle.write(f"archive_name={archive_name}\n")
    print(artifact_name)


if __name__ == "__main__":
    main()
