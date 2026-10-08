#!/usr/bin/env python3
"""File-write evidence for the Wave 0 desktop lab (THROWAWAY LAB FILE, never merged).

The pass criteria include "no new files outside expected app/log state". Two complementary records are made for every case:

  * snapshot() before and after the case, and diff() of the two: what was added, removed or changed. This is the
    authoritative record, it needs no special privileges and works the same on Linux, Windows and macOS.
  * Poller: a thread that scans the same roots every ``interval`` seconds while the case runs and appends one JSON line per
    newly seen or changed path to writes.jsonl (first-seen time included), so a file that appears and disappears between
    the two snapshots (a temporary download, a staged installer) still leaves a trace. A scan cannot see a file that lives
    for less than one interval; the report must say so, the poller is supporting evidence, the snapshot diff is the proof.

classify(path, expected_prefixes) decides whether a path is in the state the app is expected to write (its own data, log and
cache directories, named by the OS fork per operating system) or not. Both Windows and POSIX path syntax are understood on
any host, so the logic is unit-tested everywhere.

Entry of a snapshot: {"type": "file"|"dir"|"symlink"|"other", "size": int, "mtime_ns": int, "sha256": hex|None}. sha256 is
recorded for regular files up to 1 MiB only. Unreadable paths are recorded as {"type": "unreadable", ...}, never skipped silently.

Interfaces:
  snapshot(roots, exclude=()) -> dict   path -> entry; ``exclude`` are path prefixes or fnmatch patterns
  diff(before, after) -> {"added": [...], "removed": [...], "changed": [...]}   sorted path lists
  classify(path, expected_prefixes) -> "expected" | "unexpected"
  unexpected(paths, expected_prefixes) -> sorted list of the unexpected ones
  Poller(roots, interval=0.25, out_path=None, exclude=())  start() / stop() / events()
  CLI: fswatch.py snapshot --root DIR [--root DIR2] --out FILE | diff --before FILE --after FILE [--expected PREFIX ...]

UNVERIFIED ON CI: the polling cost on a large home directory of a hosted runner (scan only the app's own directories).
"""
from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import ntpath
import os
import posixpath
import re
import stat as stat_module
import sys
import threading
import time
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Sequence

HASH_LIMIT = 1 << 20
Entry = Dict[str, Any]
Snapshot = Dict[str, Entry]
WINDOWS_PATH = re.compile(r"^[A-Za-z]:[\\/]|^\\\\|\\")


def _entry(path: str) -> Entry:
    try:
        info = os.lstat(path)
    except OSError as error:
        return {"type": "unreadable", "size": 0, "mtime_ns": 0, "sha256": None, "error": type(error).__name__}
    mode = info.st_mode
    if stat_module.S_ISLNK(mode):
        kind = "symlink"
    elif stat_module.S_ISDIR(mode):
        kind = "dir"
    elif stat_module.S_ISREG(mode):
        kind = "file"
    else:
        kind = "other"
    digest = None
    if kind == "file" and info.st_size <= HASH_LIMIT:
        try:
            with open(path, "rb") as handle:
                digest = hashlib.sha256(handle.read()).hexdigest()
        except OSError:
            digest = None
    return {"type": kind, "size": int(info.st_size), "mtime_ns": int(info.st_mtime_ns), "sha256": digest}


def _excluded(path: str, exclude: Sequence[str]) -> bool:
    for pattern in exclude:
        if path == pattern or path.startswith(pattern.rstrip("/\\") + os.sep) or fnmatch.fnmatch(path, pattern):
            return True
    return False


def snapshot(roots: Iterable[str], exclude: Sequence[str] = (), hash_files: bool = True) -> Snapshot:
    """Every path under ``roots`` (the roots themselves included when they exist)."""
    result: Snapshot = {}
    exclude = tuple(str(item) for item in exclude)
    for root in roots:
        root = str(root)
        if not os.path.lexists(root):
            continue
        stack = [root]
        while stack:
            current = stack.pop()
            if _excluded(current, exclude):
                continue
            entry = _entry(current) if hash_files else _light_entry(current)
            result[current] = entry
            if entry["type"] == "dir":
                try:
                    names = sorted(os.listdir(current))
                except OSError:
                    result[current] = dict(entry, listing_error=True)
                    continue
                for name in names:
                    stack.append(os.path.join(current, name))
    return result


def _light_entry(path: str) -> Entry:
    entry = _entry(path)
    entry["sha256"] = None
    return entry


def diff(before: Snapshot, after: Snapshot) -> Dict[str, List[str]]:
    added = sorted(path for path in after if path not in before)
    removed = sorted(path for path in before if path not in after)
    changed = []
    for path in sorted(set(before) & set(after)):
        if _entry_changed(before[path], after[path], with_hash=True):
            changed.append(path)
    return {"added": added, "removed": removed, "changed": changed}


def _entry_changed(old: Entry, new: Entry, with_hash: bool) -> bool:
    """A directory's size and mtime change whenever an entry is added or removed below it; those are reported through the
    added/removed paths of the entries themselves, so a directory is "changed" only when its type changed."""
    if old.get("type") != new.get("type"):
        return True
    if new.get("type") == "dir":
        return False
    if old.get("size") != new.get("size") or old.get("mtime_ns") != new.get("mtime_ns"):
        return True
    return with_hash and old.get("sha256") != new.get("sha256")


def _flavour(text: str):
    return ntpath if WINDOWS_PATH.search(text) else posixpath


def _normal(path: str, flavour) -> str:
    text = flavour.normpath(path)
    if flavour is ntpath:
        text = text.replace("/", "\\").lower()
    return text


def classify(path: str, expected_prefixes: Iterable[str]) -> str:
    """"expected" when ``path`` is one of the expected prefixes or lies below one (on a path-component boundary)."""
    for prefix in expected_prefixes:
        flavour = ntpath if (_flavour(prefix) is ntpath or _flavour(path) is ntpath) else posixpath
        left, right = _normal(path, flavour), _normal(prefix, flavour)
        separator = "\\" if flavour is ntpath else "/"
        if left == right or left.startswith(right.rstrip("\\/") + separator):
            return "expected"
    return "unexpected"


def unexpected(paths: Iterable[str], expected_prefixes: Iterable[str]) -> List[str]:
    prefixes = list(expected_prefixes)
    return sorted(path for path in paths if classify(path, prefixes) == "unexpected")


class Poller:
    """Scan ``roots`` every ``interval`` seconds and record each new or changed path once, with its first-seen time."""

    def __init__(self, roots: Iterable[str], interval: float = 0.25, out_path: Optional[str] = None, exclude: Sequence[str] = ()):
        self.roots = [str(root) for root in roots]
        self.interval = interval
        self.out_path = out_path
        self.exclude = tuple(exclude)
        self._events: List[Dict[str, Any]] = []
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self._known: Snapshot = {}
        self._handle = None
        self.scans = 0

    def _scan(self, record: bool) -> None:
        current = snapshot(self.roots, self.exclude, hash_files=False)
        self.scans += 1
        stamp = round(time.time(), 6)
        if record:
            for path, entry in sorted(current.items()):
                old = self._known.get(path)
                if old is None:
                    self._emit("added", path, entry, stamp)
                elif _entry_changed(old, entry, with_hash=False):
                    self._emit("changed", path, entry, stamp)
            for path in sorted(self._known):
                if path not in current:
                    self._emit("removed", path, self._known[path], stamp)
        self._known = current

    def _emit(self, event: str, path: str, entry: Entry, stamp: float) -> None:
        record = {"t": stamp, "event": event, "path": path, "type": entry.get("type"), "size": entry.get("size"), "mtime_ns": entry.get("mtime_ns")}
        with self._lock:
            self._events.append(record)
            if self._handle is not None:
                self._handle.write(json.dumps(record, sort_keys=True) + "\n")
                self._handle.flush()
                try:
                    os.fsync(self._handle.fileno())
                except OSError:
                    pass

    def _run(self) -> None:
        while not self._stop.wait(self.interval):
            self._scan(record=True)

    def start(self) -> None:
        if self.out_path:
            Path(self.out_path).parent.mkdir(parents=True, exist_ok=True)
            self._handle = open(self.out_path, "a", encoding="utf-8")
        self._scan(record=False)  # the baseline: what is already there is not a write of this case
        self._thread = threading.Thread(target=self._run, daemon=True, name="fswatch-poller")
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=10)
        self._scan(record=True)  # a last scan so nothing written just before stop() is lost
        with self._lock:
            if self._handle is not None:
                self._handle.close()
                self._handle = None

    def events(self) -> List[Dict[str, Any]]:
        with self._lock:
            return [dict(item) for item in self._events]


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    snap = sub.add_parser("snapshot")
    snap.add_argument("--root", action="append", required=True)
    snap.add_argument("--exclude", action="append", default=[])
    snap.add_argument("--out", required=True)
    delta = sub.add_parser("diff")
    delta.add_argument("--before", required=True)
    delta.add_argument("--after", required=True)
    delta.add_argument("--expected", action="append", default=[])
    args = parser.parse_args(argv)
    if args.command == "snapshot":
        Path(args.out).write_text(json.dumps(snapshot(args.root, args.exclude), indent=1, sort_keys=True) + "\n", encoding="utf-8")
        return 0
    before = json.loads(Path(args.before).read_text(encoding="utf-8"))
    after = json.loads(Path(args.after).read_text(encoding="utf-8"))
    result = diff(before, after)
    result["unexpected_added"] = unexpected(result["added"], args.expected)
    result["unexpected_changed"] = unexpected(result["changed"], args.expected)
    print(json.dumps(result, indent=1, sort_keys=True))
    return 1 if (result["unexpected_added"] or result["unexpected_changed"]) else 0


if __name__ == "__main__":
    sys.exit(main())
