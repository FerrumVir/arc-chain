#!/usr/bin/env python3
"""Validate validator-vault tar metadata without extracting or printing members."""

from __future__ import annotations

import argparse
import stat
import sys
import tarfile
from pathlib import Path, PurePosixPath


MAX_ARCHIVE_BYTES = 1024 * 1024
MAX_MEMBERS = 64
MAX_MEMBER_BYTES = 128 * 1024
MAX_TOTAL_FILE_BYTES = 512 * 1024
MIN_REGULAR_FILES = 6
MAX_PATH_BYTES = 192
MAX_PATH_DEPTH = 4


class VaultValidationError(ValueError):
    """The decrypted vault is not a bounded, safe tar archive."""


def reject(message: str) -> "NoReturn":
    raise VaultValidationError(message)


def validate_member_path(name: str, index: int) -> str:
    if not name or len(name.encode("utf-8", errors="strict")) > MAX_PATH_BYTES:
        reject(f"member {index} has an empty or oversized path")
    if any(ord(character) < 32 or ord(character) == 127 for character in name):
        reject(f"member {index} path contains a control character")
    pure = PurePosixPath(name)
    if (
        pure.is_absolute()
        or "\\" in name
        or ":" in name
        or any(part in ("", ".", "..") for part in pure.parts)
        or len(pure.parts) > MAX_PATH_DEPTH
    ):
        reject(f"member {index} has an unsafe path")
    return pure.as_posix()


def validate_archive(path: Path) -> None:
    try:
        mode = path.lstat().st_mode
        size = path.stat().st_size
    except OSError as error:
        reject(f"cannot stat decrypted archive: {error.strerror or 'I/O error'}")
    if not stat.S_ISREG(mode) or path.is_symlink():
        reject("decrypted archive is not a regular, non-symlink file")
    if size <= 0 or size > MAX_ARCHIVE_BYTES or size % 512 != 0:
        reject("decrypted archive size is outside the bounded tar contract")

    names: set[str] = set()
    folded_names: set[str] = set()
    file_count = 0
    total_file_bytes = 0
    try:
        # `r:` deliberately rejects compressed input. The source is an
        # encrypted plain tar, so no decompression-bomb surface is needed.
        with tarfile.open(path, mode="r:") as archive:
            members = archive.getmembers()
            if not members or len(members) > MAX_MEMBERS:
                reject("archive member count is outside the bounded contract")
            for index, member in enumerate(members, start=1):
                normalized = validate_member_path(member.name, index)
                folded = normalized.casefold()
                if normalized in names or folded in folded_names:
                    reject(f"member {index} duplicates another archive path")
                names.add(normalized)
                folded_names.add(folded)

                if member.pax_headers:
                    forbidden = {
                        "path",
                        "linkpath",
                        "size",
                        "GNU.sparse.map",
                        "GNU.sparse.name",
                        "GNU.sparse.realsize",
                    }
                    if forbidden.intersection(member.pax_headers):
                        reject(f"member {index} has a path/size-changing PAX header")

                if member.isdir():
                    if member.mode & 0o7777 not in (0o500, 0o700):
                        reject(f"member {index} is not a private directory")
                    continue
                if (
                    not member.isfile()
                    or member.issym()
                    or member.islnk()
                    or member.issparse()
                ):
                    reject(f"member {index} is not a regular file or directory")
                if member.size <= 0 or member.size > MAX_MEMBER_BYTES:
                    reject(f"member {index} size is outside the bounded contract")
                if member.mode & 0o7777 not in (0o400, 0o600):
                    reject(f"member {index} does not have private file permissions")
                file_count += 1
                total_file_bytes += member.size
                if total_file_bytes > MAX_TOTAL_FILE_BYTES:
                    reject("archive regular-file bytes exceed the bounded contract")
    except (OSError, tarfile.TarError, UnicodeError) as error:
        reject(f"decrypted input is not a valid plain tar: {type(error).__name__}")

    if file_count < MIN_REGULAR_FILES:
        reject("archive contains fewer than six private vault files")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        validate_archive(args.archive)
    except VaultValidationError as error:
        print(f"validator vault validation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
