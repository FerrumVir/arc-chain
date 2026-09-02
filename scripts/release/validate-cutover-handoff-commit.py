#!/usr/bin/env python3
"""Validate and materialize one exact-parent, exact-tree cutover handoff commit."""

from __future__ import annotations

import argparse
import os
import re
import stat
import subprocess
import sys
from pathlib import Path
from typing import NoReturn, Sequence


PUBLIC_FILES = (
    "arc-cutover-policy.json",
    "arc-legacy-maintenance-boundary.json",
    "arc-recovery-checkpoint-descriptor.json",
)
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
OBJECT_RE = re.compile(r"^[0-9a-f]{40,64}$")


class CommitValidationError(RuntimeError):
    pass


def fail(message: str) -> NoReturn:
    raise CommitValidationError(message)


def git(repository: Path, arguments: Sequence[str], *, binary: bool = False):
    environment = {
        "HOME": "/var/empty",
        "PATH": "/usr/bin:/bin:/usr/local/bin",
        "LANG": "C",
        "LC_ALL": "C",
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_TERMINAL_PROMPT": "0",
    }
    try:
        completed = subprocess.run(
            ["git", "-C", os.fspath(repository), *arguments],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=not binary,
            env=environment,
            timeout=120,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        fail(f"git object inspection failed: {error}")
    if completed.returncode != 0:
        diagnostic = completed.stderr
        if isinstance(diagnostic, bytes):
            diagnostic = diagnostic.decode("utf-8", errors="replace")
        fail(f"git rejected the derived handoff object: {diagnostic.strip()[:2000]}")
    return completed.stdout


def write_create_only(path: Path, payload: bytes) -> None:
    descriptor = os.open(
        path,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
        0o444,
    )
    try:
        offset = 0
        while offset < len(payload):
            written = os.write(descriptor, payload[offset:])
            if written <= 0:
                fail("derived handoff materialization made no progress")
            offset += written
        os.fchmod(descriptor, 0o444)
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def validate_commit(repository: Path, handoff_commit: str, main_commit: str) -> None:
    resolved = git(repository, ["rev-parse", f"{handoff_commit}^{{commit}}"]).strip()
    if resolved != handoff_commit:
        fail("handoff commit does not resolve exactly")
    parent_line = git(
        repository, ["rev-list", "--parents", "-n", "1", handoff_commit]
    ).strip()
    if parent_line != f"{handoff_commit} {main_commit}":
        fail("handoff commit must have current main as its sole parent")
    raw_listing = git(
        repository, ["ls-tree", "-rz", "--full-tree", handoff_commit], binary=True
    )
    entries = []
    for raw in raw_listing.split(b"\0"):
        if not raw:
            continue
        try:
            metadata, path = raw.split(b"\t", 1)
            mode, kind, object_id = metadata.decode("ascii").split(" ")
            name = path.decode("utf-8")
        except (ValueError, UnicodeError):
            fail("handoff commit tree record is malformed")
        if mode != "100644" or kind != "blob" or OBJECT_RE.fullmatch(object_id) is None:
            fail(f"handoff commit contains a non-regular or non-0644 entry: {name!r}")
        entries.append(name)
    if entries != list(PUBLIC_FILES):
        fail("handoff commit tree differs from the exact three-file contract")


def materialize(
    repository: Path, handoff_commit: str, output_dir: Path
) -> None:
    if output_dir.exists() or output_dir.is_symlink():
        fail("handoff output directory must be absent")
    parent = output_dir.parent.resolve()
    if not parent.is_dir() or parent.is_symlink():
        fail("handoff output parent is unavailable")
    output_dir.mkdir(mode=0o700)
    published = False
    try:
        for filename in PUBLIC_FILES:
            payload = git(
                repository, ["cat-file", "blob", f"{handoff_commit}:{filename}"], binary=True
            )
            if not payload:
                fail(f"handoff commit contains an empty asset: {filename}")
            write_create_only(output_dir / filename, payload)
        directory_fd = os.open(
            output_dir, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
        )
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
        published = True
    finally:
        if not published:
            for path in output_dir.iterdir() if output_dir.is_dir() else ():
                if path.is_file() and not path.is_symlink():
                    os.chmod(path, stat.S_IWUSR | stat.S_IRUSR)
                    path.unlink()
            if output_dir.is_dir():
                output_dir.rmdir()


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository-root", required=True, type=Path)
    parser.add_argument("--handoff-commit", required=True)
    parser.add_argument("--main-commit", required=True)
    parser.add_argument("--output-dir", required=True, type=Path)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        if (
            COMMIT_RE.fullmatch(args.handoff_commit) is None
            or COMMIT_RE.fullmatch(args.main_commit) is None
        ):
            fail("handoff and main commits must be full lowercase Git SHAs")
        repository = args.repository_root.resolve()
        if not repository.is_dir():
            fail("repository root is unavailable")
        validate_commit(repository, args.handoff_commit, args.main_commit)
        materialize(repository, args.handoff_commit, args.output_dir)
        print(f"Validated compact handoff commit {args.handoff_commit}")
        return 0
    except (CommitValidationError, OSError) as error:
        print(f"cutover handoff commit validation: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
