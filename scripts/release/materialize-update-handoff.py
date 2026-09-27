#!/usr/bin/env python3
"""Materialize the exact one-file existing-chain update Actions artifact."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import zipfile

ASSET = "arc-existing-chain-update-attestation.json"
MAX_BYTES = 2 * 1024 * 1024


def need(condition, message):
    if not condition:
        raise ValueError(message)


def materialize(args):
    need(type(args.artifact_id) is int and args.artifact_id > 0, "invalid artifact ID")
    need(re.fullmatch(r"[0-9a-f]{40}", args.commit), "invalid source commit")
    need(re.fullmatch(rf"arc-existing-chain-update-handoff-{args.commit}-[1-9][0-9]*-[1-9][0-9]*",
                      args.artifact_name), "update artifact name is not commit/run/attempt bound")
    need(re.fullmatch(r"sha256:[0-9a-f]{64}", args.artifact_digest), "invalid server digest")
    need(0 < args.artifact_size <= MAX_BYTES, "artifact size exceeds bounded contract")
    root = args.downloads_root
    need(root.is_dir() and not root.is_symlink(), "unsafe download root")
    entries = list(root.iterdir())
    if len(entries) == 1 and entries[0].name == args.artifact_name and entries[0].is_dir():
        root = entries[0]
        need(not root.is_symlink(), "symlinked artifact directory")
        entries = list(root.iterdir())
    need(len(entries) == 1, "download must contain exactly one raw artifact ZIP")
    archive = entries[0]
    metadata = archive.lstat()
    need(stat.S_ISREG(metadata.st_mode) and metadata.st_nlink == 1 and
         metadata.st_size == args.artifact_size, "unsafe or wrong-sized raw ZIP")
    raw = archive.read_bytes()
    need("sha256:" + hashlib.sha256(raw).hexdigest() == args.artifact_digest,
         "raw ZIP differs from selected immutable digest")
    with zipfile.ZipFile(archive) as handle:
        members = handle.infolist()
        need(len(members) == 1, "update handoff must have exactly one file")
        member = members[0]
        need(member.filename == ASSET and not member.is_dir() and not member.flag_bits & 1 and
             stat.S_IFMT(member.external_attr >> 16) in (0, stat.S_IFREG) and
             0 < member.file_size <= MAX_BYTES, "unexpected or unsafe ZIP entry")
        payload = handle.read(member)
    value = json.loads(payload)
    need(isinstance(value, dict) and value.get("schema") == "arc.existing-recovered-chain-update/v1" and
         value.get("release", {}).get("repository") == "FerrumVir/arc-chain" and
         value.get("release", {}).get("commit") == args.commit,
         "attestation schema/repository/commit differs")
    need(not args.output_dir.exists() and not args.output_dir.is_symlink(), "output must be absent")
    args.output_dir.mkdir(mode=0o700)
    fd = os.open(args.output_dir / ASSET, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o444)
    with os.fdopen(fd, "wb") as handle:
        handle.write(payload)
        handle.flush()
        os.fsync(handle.fileno())
    fd = os.open(args.output_dir, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("downloads-root", "output-dir"):
        parser.add_argument("--" + name, type=Path, required=True)
    for name in ("artifact-id", "artifact-size"):
        parser.add_argument("--" + name, type=int, required=True)
    for name in ("artifact-name", "artifact-digest", "commit"):
        parser.add_argument("--" + name, required=True)
    try:
        materialize(parser.parse_args())
        print("Verified and materialized exact existing-chain update handoff")
        return 0
    except (ValueError, OSError, zipfile.BadZipFile) as error:
        print("existing-chain update materialization: " + str(error))
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
