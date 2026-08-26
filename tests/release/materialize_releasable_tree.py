#!/usr/bin/env python3
"""Materialize bytes that can become the next ARC release.

Index mode reads blobs directly from Git's index, so a staged secret cannot be
hidden by replacing the working-copy file before running the local gate.
Worktree mode includes tracked files plus untracked, non-ignored files and
therefore catches edits that have not been staged yet. Symlinks are always
written as their target text; this scanner never follows them outside the repo.
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import shutil
import subprocess


def git(repository: Path, *args: str) -> bytes:
    return subprocess.check_output(["git", "-C", str(repository), *args])


def safe_relative_path(encoded: bytes) -> Path:
    relative = Path(os.fsdecode(encoded))
    if relative.is_absolute() or not relative.parts or ".." in relative.parts:
        raise SystemExit(f"unsafe Git path: {relative}")
    return relative


def prepare_destination(destination: Path) -> None:
    if destination.exists() and any(destination.iterdir()):
        raise SystemExit(f"destination must be empty: {destination}")
    destination.mkdir(parents=True, exist_ok=True)


def materialize_index(repository: Path, destination: Path) -> None:
    for record in git(repository, "ls-files", "--stage", "-z").split(b"\0"):
        if not record:
            continue
        metadata, separator, encoded_path = record.partition(b"\t")
        if not separator:
            raise SystemExit("malformed git ls-files --stage record")
        mode, object_id, stage = metadata.decode("ascii").split()
        if stage != "0":
            raise SystemExit(
                f"cannot scan an unmerged index entry: {os.fsdecode(encoded_path)}"
            )

        relative = safe_relative_path(encoded_path)
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        if mode == "160000":
            # A gitlink contains no file bytes in this repository. Preserve the
            # exact object identity as inert text without entering the submodule.
            target.write_text(object_id + "\n", encoding="ascii")
        else:
            blob = git(repository, "cat-file", "blob", object_id)
            target.write_bytes(blob)


def materialize_worktree(repository: Path, destination: Path) -> None:
    listed = git(
        repository,
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
    )
    for encoded_path in listed.split(b"\0"):
        if not encoded_path:
            continue
        relative = safe_relative_path(encoded_path)
        source = repository / relative
        if not os.path.lexists(source):
            continue
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        if source.is_symlink():
            target.write_text(os.readlink(source), encoding="utf-8")
        elif source.is_file():
            shutil.copy2(source, target)


def main() -> None:
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--index", action="store_true")
    mode.add_argument("--worktree", action="store_true")
    parser.add_argument("repository", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()

    repository = args.repository.resolve()
    destination = args.destination.resolve()
    if not (repository / ".git").exists():
        # Linked worktrees have a .git control file, so `exists()` is enough.
        raise SystemExit(f"not a Git worktree: {repository}")
    prepare_destination(destination)
    if args.index:
        materialize_index(repository, destination)
    else:
        materialize_worktree(repository, destination)


if __name__ == "__main__":
    main()
