#!/usr/bin/env python3
"""Fail closed unless a GitHub release exactly matches local release bytes."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import stat
import sys
from pathlib import Path
from typing import Any


SHA_PATTERN = re.compile(r"[0-9a-f]{40}")
TAG_PATTERN = re.compile(r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)")


class VerificationError(ValueError):
    """The server release does not match the expected immutable contract."""


def parse_boolean(value: str) -> bool:
    if value == "true":
        return True
    if value == "false":
        return False
    raise argparse.ArgumentTypeError("expected true or false")


def require_object(value: Any, description: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise VerificationError(f"{description} must be a JSON object")
    return value


def require_exact(value: Any, expected: Any, description: str) -> None:
    if type(value) is not type(expected) or value != expected:  # noqa: E721
        raise VerificationError(
            f"{description} mismatch: expected {expected!r}, got {value!r}"
        )


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return f"sha256:{digest.hexdigest()}"


def local_assets(asset_directory: Path) -> dict[str, dict[str, Any]]:
    if not asset_directory.is_dir():
        raise VerificationError(
            f"local asset directory is missing: {asset_directory}"
        )

    assets: dict[str, dict[str, Any]] = {}
    for path in sorted(asset_directory.iterdir(), key=lambda item: item.name):
        mode = path.lstat().st_mode
        if not stat.S_ISREG(mode):
            raise VerificationError(f"local release asset is not a regular file: {path}")
        if path.name in assets:
            raise VerificationError(f"duplicate local release asset: {path.name}")
        size = path.stat().st_size
        if size <= 0:
            raise VerificationError(f"local release asset is empty: {path.name}")
        assets[path.name] = {"size": size, "digest": sha256_file(path)}

    if not assets:
        raise VerificationError("local release asset directory is empty")
    return assets


def verify_release(
    release: dict[str, Any],
    expected_assets: dict[str, dict[str, Any]],
    *,
    tag: str,
    commit: str,
    draft: bool,
    immutable: bool,
    expected_id: int | None,
) -> int:
    if not TAG_PATTERN.fullmatch(tag):
        raise VerificationError(f"invalid strict release tag: {tag!r}")
    if not SHA_PATTERN.fullmatch(commit):
        raise VerificationError(f"invalid lowercase commit SHA: {commit!r}")

    release_id = release.get("id")
    if type(release_id) is not int or release_id <= 0:  # noqa: E721
        raise VerificationError(f"release id must be a positive integer: {release_id!r}")
    if expected_id is not None:
        require_exact(release_id, expected_id, "release id")
    require_exact(release.get("tag_name"), tag, "release tag")
    require_exact(release.get("target_commitish"), commit, "release target")
    require_exact(release.get("draft"), draft, "release draft state")
    require_exact(release.get("prerelease"), False, "release prerelease state")
    require_exact(release.get("immutable"), immutable, "release immutable state")

    author = require_object(release.get("author"), "release author")
    require_exact(author.get("login"), "github-actions[bot]", "release author login")

    remote_assets_value = release.get("assets")
    if not isinstance(remote_assets_value, list):
        raise VerificationError("release assets must be a JSON array")
    remote_assets: dict[str, dict[str, Any]] = {}
    for index, raw_asset in enumerate(remote_assets_value):
        asset = require_object(raw_asset, f"release asset {index}")
        name = asset.get("name")
        if not isinstance(name, str) or not name:
            raise VerificationError(f"release asset {index} has an invalid name")
        if name in remote_assets:
            raise VerificationError(f"duplicate remote release asset: {name}")
        remote_assets[name] = asset

    local_names = set(expected_assets)
    remote_names = set(remote_assets)
    if remote_names != local_names:
        missing = sorted(local_names - remote_names)
        extra = sorted(remote_names - local_names)
        raise VerificationError(
            f"release asset name set mismatch; missing={missing!r}, extra={extra!r}"
        )

    for name, expected in expected_assets.items():
        asset = remote_assets[name]
        require_exact(asset.get("state"), "uploaded", f"asset {name} state")
        require_exact(asset.get("size"), expected["size"], f"asset {name} size")
        require_exact(asset.get("digest"), expected["digest"], f"asset {name} digest")
        uploader = require_object(asset.get("uploader"), f"asset {name} uploader")
        require_exact(
            uploader.get("login"),
            "github-actions[bot]",
            f"asset {name} uploader login",
        )

    return release_id


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release-json", required=True, type=Path)
    parser.add_argument("--asset-directory", required=True, type=Path)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--draft", required=True, type=parse_boolean)
    parser.add_argument("--immutable", required=True, type=parse_boolean)
    parser.add_argument("--expected-id", type=int)
    parser.add_argument(
        "--github-output",
        type=Path,
        help="append release_id=<id> for a subsequent workflow step",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        release = require_object(
            json.loads(args.release_json.read_text(encoding="utf-8")),
            "release document",
        )
        release_id = verify_release(
            release,
            local_assets(args.asset_directory),
            tag=args.tag,
            commit=args.commit,
            draft=args.draft,
            immutable=args.immutable,
            expected_id=args.expected_id,
        )
        if args.github_output is not None:
            with args.github_output.open("a", encoding="utf-8") as output:
                output.write(f"release_id={release_id}\n")
    except (OSError, json.JSONDecodeError, VerificationError) as error:
        print(f"release verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
