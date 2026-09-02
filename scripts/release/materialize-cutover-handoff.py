#!/usr/bin/env python3
"""Safely materialize one immutable protected recovery handoff artifact."""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import shutil
import stat
import tempfile
import zipfile
from pathlib import Path, PurePosixPath
from typing import NoReturn, Sequence


EXPECTED_FILES = {
    "arc-cutover-policy.json",
    "arc-legacy-maintenance-boundary.json",
    "arc-recovery-checkpoint-descriptor.json",
}
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
DIGEST_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
MAX_ACTIONS_ZIP_BYTES = 32 * 1024 * 1024
MAX_EXPANDED_BYTES = 18 * 1024 * 1024
EXPANSION_SLACK_BYTES = 2 * 1024 * 1024
MAX_EXPANSION_RATIO = 20


def fail(message: str) -> NoReturn:
    raise SystemExit(f"cutover handoff materialization: {message}")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--downloads-root", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--artifact-id", required=True, type=int)
    parser.add_argument("--artifact-name", required=True)
    parser.add_argument("--artifact-digest", required=True)
    parser.add_argument("--artifact-size", required=True, type=int)
    parser.add_argument("--commit", required=True)
    return parser.parse_args(argv)


def locate_raw_zip(root: Path, artifact_name: str) -> Path:
    candidates: list[Path] = []
    named = root / artifact_name
    search = named if named.is_dir() and not named.is_symlink() else root
    if not search.is_dir() or search.is_symlink():
        fail("downloaded artifact root is missing or symlinked")
    for path in search.iterdir():
        if path.is_file() and not path.is_symlink():
            candidates.append(path)
    if len(candidates) != 1 or len(list(search.iterdir())) != 1:
        fail("raw Actions artifact directory must contain exactly one regular ZIP")
    return candidates[0]


def validate_zip_entry(info: zipfile.ZipInfo) -> None:
    pure = PurePosixPath(info.filename)
    mode_type = (info.external_attr >> 16) & 0o170000
    maximum = (
        16 * 1024 * 1024
        if info.filename == "arc-legacy-maintenance-boundary.json"
        else 1024 * 1024
    )
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
        or info.file_size > maximum
    ):
        fail(f"Actions artifact has an unsafe or oversized entry: {info.filename!r}")


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    if args.artifact_id <= 0:
        fail("artifact ID must be positive")
    if COMMIT_RE.fullmatch(args.commit) is None:
        fail("commit must be one full lowercase Git SHA")
    expected_name = f"arc-recovery-release-handoff-{args.commit}"
    if args.artifact_name != expected_name:
        fail("artifact name is not exact-commit bound")
    if DIGEST_RE.fullmatch(args.artifact_digest) is None:
        fail("artifact digest must be one server SHA-256")
    if not 0 < args.artifact_size <= MAX_ACTIONS_ZIP_BYTES:
        fail("artifact size is outside the reviewed bound")
    if args.output_dir.exists() or args.output_dir.is_symlink():
        fail("output directory must be absent for create-only materialization")

    artifact_zip = locate_raw_zip(args.downloads_root, args.artifact_name)
    details = artifact_zip.lstat()
    if (
        not stat.S_ISREG(details.st_mode)
        or details.st_nlink != 1
        or details.st_size != args.artifact_size
        or sha256_file(artifact_zip) != args.artifact_digest.removeprefix("sha256:")
    ):
        fail("downloaded Actions ZIP differs from the selected ID/digest/size tuple")

    parent = args.output_dir.parent.resolve()
    if not parent.is_dir() or parent.is_symlink():
        fail("output parent is unavailable or symlinked")
    stage = Path(tempfile.mkdtemp(prefix=f".{args.output_dir.name}.stage-", dir=parent))
    published = False
    try:
        try:
            with zipfile.ZipFile(artifact_zip, "r") as archive:
                infos = archive.infolist()
                if len(infos) != len(EXPECTED_FILES):
                    fail("Actions artifact entry count differs from the three-file contract")
                names: set[str] = set()
                expanded = 0
                for info in infos:
                    validate_zip_entry(info)
                    if info.filename in names:
                        fail(f"Actions artifact repeats entry {info.filename!r}")
                    names.add(info.filename)
                    expanded += info.file_size
                if names != EXPECTED_FILES:
                    fail("Actions artifact membership differs from the protected handoff contract")
                expansion_bound = min(
                    MAX_EXPANDED_BYTES,
                    details.st_size * MAX_EXPANSION_RATIO + EXPANSION_SLACK_BYTES,
                )
                if expanded > expansion_bound:
                    fail("Actions artifact exceeds its reviewed expansion bound")
                for info in infos:
                    target = stage / info.filename
                    with archive.open(info, "r") as source, target.open("xb") as output:
                        shutil.copyfileobj(source, output, 1024 * 1024)
                        output.flush()
                        os.fsync(output.fileno())
                    os.chmod(target, 0o444)
        except (OSError, zipfile.BadZipFile) as error:
            fail(f"cannot read selected Actions artifact ZIP: {error}")
        directory_fd = os.open(stage, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
        os.rename(stage, args.output_dir)
        published = True
    finally:
        if not published and stage.exists():
            shutil.rmtree(stage)
    print(
        f"Materialized protected recovery handoff artifact {args.artifact_id} "
        f"for {args.commit}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
