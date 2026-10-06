#!/usr/bin/env python3
"""Byte-level snapshots of a v0.7 data directory for the bridge acceptance.

`snapshot` records the SHA-256, size, and type of every entry under a
directory without following links. `compare` exits non-zero unless two
snapshots are identical. `hook` is the systemd ExecStartPre form: it appends a
numbered snapshot together with the SHA-256 of the binary the unit is about to
start, so the test can pick the exact pre-bridge state even though the v0.7
updater, not the test, restarts the service.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
import sys
import time
from pathlib import Path

DESKTOP_EXCLUDED = {"bin", "models", "legacy-bridge"}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def snapshot(root: Path, desktop_root: bool) -> dict[str, dict[str, object]]:
    entries: dict[str, dict[str, object]] = {}
    if not root.exists():
        return entries
    for current, dirnames, filenames in os.walk(root, followlinks=False):
        relative_dir = Path(current).relative_to(root)
        if relative_dir == Path("."):
            if desktop_root:
                dirnames[:] = [
                    name
                    for name in dirnames
                    if name not in DESKTOP_EXCLUDED and not name.startswith("data-v3")
                ]
                filenames = [name for name in filenames if name not in DESKTOP_EXCLUDED]
        for name in sorted(dirnames) + sorted(filenames):
            path = Path(current) / name
            info = path.lstat()
            key = (relative_dir / name).as_posix()
            if stat.S_ISLNK(info.st_mode):
                entries[key] = {"type": "symlink", "target": os.readlink(path)}
            elif stat.S_ISDIR(info.st_mode):
                entries[key] = {"type": "dir"}
            elif stat.S_ISREG(info.st_mode):
                entries[key] = {"type": "file", "size": info.st_size, "sha256": sha256_file(path)}
            else:
                entries[key] = {"type": "other"}
    return dict(sorted(entries.items()))


def write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    take = sub.add_parser("snapshot")
    take.add_argument("--root", type=Path, required=True)
    take.add_argument("--desktop-root", action="store_true")
    take.add_argument("--out", type=Path, required=True)

    hook = sub.add_parser("hook")
    hook.add_argument("--root", type=Path, required=True)
    hook.add_argument("--binary", type=Path, required=True)
    hook.add_argument("--out-dir", type=Path, required=True)

    compare = sub.add_parser("compare")
    compare.add_argument("before", type=Path)
    compare.add_argument("after", type=Path)

    args = parser.parse_args()
    if args.command == "snapshot":
        write_json(args.out, {"root": str(args.root), "entries": snapshot(args.root, args.desktop_root)})
        return 0
    if args.command == "hook":
        # Never fail the unit start: a broken hook would change the behavior
        # under test. Errors are recorded in the snapshot itself instead.
        try:
            args.out_dir.mkdir(parents=True, exist_ok=True)
            number = len(list(args.out_dir.glob("start-*.json"))) + 1
            binary_sha = sha256_file(args.binary) if args.binary.is_file() else None
            record = {
                "taken_unix": time.time(),
                "binary": str(args.binary),
                "binary_sha256": binary_sha,
                "root": str(args.root),
                "entries": snapshot(args.root, False),
            }
            write_json(args.out_dir / f"start-{number:03d}.json", record)
        except Exception as error:  # noqa: BLE001 - must never block the unit
            print(f"snapshot hook error: {error}", file=sys.stderr)
        return 0
    before = json.loads(args.before.read_text(encoding="utf-8"))["entries"]
    after = json.loads(args.after.read_text(encoding="utf-8"))["entries"]
    if before == after:
        print(f"identical: {len(before)} entries")
        return 0
    for key in sorted(set(before) | set(after)):
        if before.get(key) != after.get(key):
            print(f"DIFFERS {key}: {before.get(key)} -> {after.get(key)}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
