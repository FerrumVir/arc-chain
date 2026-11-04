#!/usr/bin/env python3
"""CI guard: no exported node state with personal device names in the tracked tree.

Why: the retired ``scripts/arc-export-volatile-state.sh`` wrote per-seed
endpoint dumps, including worker scoreboards that could carry community
workers' device names. The exporter is retired and ``arc-state-export/`` is
git-ignored, but ``git add -f`` or a new exporter writing elsewhere would bypass
that. This check
fails CI on either of the two shapes that leak looked like:

1. Paths: any tracked file under an export root (``arc-state-export/``) or named
   the way the exporter named its outputs (``<SEED>_<endpoint>.json`` for the six
   seed labels).
2. Content: any tracked JSON whose hostname-like fields (``hostname``,
   ``computer_name`` ...) or worker-name fields inside worker records hold a
   personal device name. "Personal device name" uses the same rules the node
   applies to community nicknames (``looks_like_default_computer_name`` in
   ``crates/arc-node/src/rpc.rs``), plus mDNS/LAN suffixes (``.local``,
   ``.lan``, ``.home``, ``.localdomain``, ``.home.arpa``) and any legacy
   ``name (hostname)`` registration, which the node never publishes either.

Limits, by design: a heuristic for operating-system default names, not a
detector of every hostname. A custom name such as ``kitchen-server`` or a bare
first name passes. Only JSON at the checked-out commit is read: git history and
forks are out of scope, and so are files outside the repository. The three synthetic test fixtures are skipped only
while their bytes match a pinned SHA-256; nothing else is skipped. JSON that does
not parse, or is over 8 MiB, is itself a finding (one existing empty file is
allowed, pinned the same way). Locations print only listed schema keys; any other
key prints as ``<key N>``.

This is not a complete personal-data scan. Custom names (``kitchen-server``,
``alice-laptop``) pass; only the keys listed below and host containers are
checked; ``.jsonl``, ``.txt`` and ``.csv`` files are not read.

Usage:
  python3 scripts/ci/export_privacy.py                 # scan the tracked tree (CI)
  python3 scripts/ci/export_privacy.py PATH [PATH...]  # scan these files or dirs
Exit 0 when clean, 1 on findings, 2 on usage errors. Findings name the file,
the JSON location and the rule; they never print the matched value.
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Iterable, Iterator

EXPORT_ROOTS = ("arc-state-export",)
SEED_LABELS = ("NYC", "LAX", "AMS", "LHR", "NRT", "SGP")
EXPORTER_FILE = re.compile(r"^(?:%s)_[a-z0-9_]+\.json$" % "|".join(SEED_LABELS))
# Exceptions are pinned to the SHA-256 of their reviewed bytes, not just their path: if a file at an excepted path
# changes, the exception no longer applies and the file is scanned (and reported) like any other.
#
# The three synthetic fixtures this check's own tests use, skipped by the tree scan while unchanged.
FIXTURE_FILES = {
    "scripts/ci/tests/fixtures/export-privacy/clean/scoreboard.json":
        "04f21a43db64c5ef2bfaeaecefcc7736191c310947aa8081e554987b1f61cb1a",
    "scripts/ci/tests/fixtures/export-privacy/leaky-content/scoreboard.json":
        "4bb5b357a095299fe65e7a6ced2639b400acd61f64ce21f2909fb4cae58ff5a2",
    "scripts/ci/tests/fixtures/export-privacy/leaky-export/arc-state-export/2026-01-01T00-00-00Z/NYC_workers_scoreboard.json":
        "ec5adcca5abaefddf253468d696e51f45340b6ef3a9f3c772091f7b24275646b",
}
# Tracked JSON that does not parse today: one empty file. Any other unreadable JSON, or new bytes at this path, is a
# finding: this check cannot vouch for what it cannot read.
UNPARSEABLE_ALLOWED = {
    "docs/arc-chain-v2/batch3-canonical-validation.json": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
}
MAX_JSON_BYTES = 8 * 1024 * 1024

# Keys that only ever hold a machine's name: checked wherever they appear.
HOSTNAME_KEYS = {"hostname", "host_name", "computer_name", "computername", "device_name", "machine_name"}
# Keys that hold a display name: checked inside worker-like records only.
NAME_KEYS = {"name", "worker_name", "node_name", "public_name", "nickname", "label", "device"}
WORKER_LIST_KEYS = {"workers", "community_workers", "scoreboard", "registrations", "registry"}
# Containers whose object KEYS can be machine names ({"workers": {"<name>": {...}}}) and whose list items can be
# machine names ({"hostnames": ["...", ...]}).
HOST_CONTAINER_KEYS = WORKER_LIST_KEYS | {"hosts", "hostnames", "devices", "machines", "computers"}
WORKER_MARKERS = {"worker_id", "worker", "address", "pubkey", "public_key"}
# Object keys printed as they are in a finding's location: a fixed list of field names this check and the exporter
# use. Any other key (and every key inside a host container, which is a machine name by definition) prints as
# <key N>, so a finding never repeats a custom machine name it could not recognise.
SCHEMA_KEYS = frozenset(HOSTNAME_KEYS | NAME_KEYS | HOST_CONTAINER_KEYS | WORKER_MARKERS | {
    "schema", "host", "app_version", "node_version", "version", "runs", "status", "notes",
})

# Mirrors DEVICE_TOKENS in crates/arc-node/src/rpc.rs (looks_like_default_computer_name).
DEVICE_TOKENS = {
    "macbook", "macbookpro", "macbookair", "imac", "imacpro", "macmini", "macstudio",
    "macpro", "mbp", "mba", "localhost",
    # Phones and tablets show up in the same way on some platforms.
    "iphone", "ipad",
}
LAN_SUFFIXES = (".local", ".lan", ".home", ".localdomain", ".home.arpa")
SAFE_DEFAULT = re.compile(r"^(?:node|arc)-[0-9a-f]{8}$")
LEGACY_PAREN = re.compile(r"\(([^()]{1,80})\)\s*$")


def looks_like_default_computer_name(value: str) -> bool:
    """Same token rules as the node's nickname check, for one name."""
    tokens = [t for t in re.split(r"[^a-z0-9]+", value.lower()) if t]
    if any(t in DEVICE_TOKENS for t in tokens):
        return True
    if any(a == "mac" and b in {"mini", "studio", "pro"} for a, b in zip(tokens, tokens[1:])):
        return True
    if len(tokens) == 2 and tokens[0] in {"desktop", "laptop"} and 7 <= len(tokens[1]) <= 8:
        return True
    return len(tokens) >= 2 and tokens[-1] == "pc"


def personal_hostname_rule(value: str) -> str | None:
    """The rule a value breaks, or None. Hashed default labels and plain text pass."""
    text = value.strip()
    if not text or SAFE_DEFAULT.match(text):
        return None
    # v0.8.10 and earlier registered workers as `<label> (<hostname>)`; the node refuses to publish any such
    # name (community_public_name), whatever the host looks like, and so does this check.
    if LEGACY_PAREN.search(text):
        return "legacy 'name (hostname)' registration"
    lower = text.lower().rstrip(".")
    if " " not in lower and any(lower.endswith(s) for s in LAN_SUFFIXES):
        return "mDNS/LAN host suffix"
    # Prose (several words with sentence punctuation) is not a name.
    if len(text) > 64 or re.search(r"[.!?,;:]\s", text):
        return None
    if looks_like_default_computer_name(text):
        return "operating-system default computer name"
    return None


@dataclass(frozen=True)
class Finding:
    path: str
    where: str
    rule: str

    def __str__(self) -> str:
        return f"{self.path}: {self.where}: {self.rule}"


def path_findings(rel: str) -> list[Finding]:
    parts = PurePosixPath(rel).parts
    found = []
    if any(p in EXPORT_ROOTS for p in parts[:-1]):
        found.append(Finding(rel, "path", "file under an exported-state root (arc-state-export/)"))
    if EXPORTER_FILE.match(parts[-1]):
        found.append(Finding(rel, "path", "file named like an exported seed endpoint (<SEED>_<endpoint>.json)"))
    return found


def _segment(key: str, position: int, in_host_container: bool) -> str:
    """A location segment that is safe to print: listed schema keys as they are, anything else by position."""
    if not in_host_container and str(key) in SCHEMA_KEYS:
        return str(key)
    return f"<key {position}>"


def _walk(node, pointer: str, container: str | None, keys_are_hosts: bool) -> Iterator[tuple[str, str, str]]:
    """Yield (pointer, what, value) for every string that must not hold a device name.

    `container` is the lower-cased worker/host container key this node sits under, if any. `keys_are_hosts` is true
    only for a mapping directly under such a container ({"workers": {"<machine name>": {...}}}): its keys are
    machine names, checked and never printed. Records in a list under a container are worker records, not maps.
    """
    if isinstance(node, dict):
        workerish = container in WORKER_LIST_KEYS or bool(WORKER_MARKERS & {str(k).lower() for k in node})
        for position, (key, value) in enumerate(node.items()):
            child = f"{pointer}/{_segment(key, position, keys_are_hosts)}"
            lowered = str(key).lower()
            if keys_are_hosts:
                yield child, "key", str(key)
            if isinstance(value, str):
                if lowered in HOSTNAME_KEYS or (workerish and lowered in NAME_KEYS):
                    yield child, lowered, value
            elif keys_are_hosts:
                yield from _walk(value, child, container, False)  # the record under a machine-name key
            elif lowered in HOST_CONTAINER_KEYS | HOSTNAME_KEYS:
                yield from _walk(value, child, lowered, isinstance(value, dict) and lowered in HOST_CONTAINER_KEYS)
            else:
                yield from _walk(value, child, None, False)
    elif isinstance(node, list):
        for i, item in enumerate(node):
            if isinstance(item, str) and container in HOST_CONTAINER_KEYS | HOSTNAME_KEYS:
                yield f"{pointer}/{i}", "item", item  # {"hostnames": ["<machine name>", ...]}
            else:
                yield from _walk(item, f"{pointer}/{i}", container, False)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def content_findings(rel: str, data: bytes) -> list[Finding]:
    try:
        doc = json.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        if UNPARSEABLE_ALLOWED.get(rel) == sha256(data):
            return []
        return [Finding(rel, "file", "unreadable JSON: it does not parse, so this check cannot vouch for it")]
    found = []
    for pointer, what, value in _walk(doc, "", None, False):
        rule = personal_hostname_rule(value)
        if rule:
            found.append(Finding(rel, f"{pointer or '/'} ({what})", rule))
    return found


def scan_file(path: Path, rel: str) -> list[Finding]:
    found = path_findings(rel)  # always, wherever the file is
    if rel.endswith(".json"):
        try:
            size = path.stat().st_size
            if size > MAX_JSON_BYTES:
                found.append(Finding(rel, "file", f"JSON larger than {MAX_JSON_BYTES // (1024 * 1024)} MiB was not scanned"))
            else:
                found += content_findings(rel, path.read_bytes())
        except OSError:
            found.append(Finding(rel, "file", "could not be read, so this check cannot vouch for it"))
    return found


def tracked_files(root: Path) -> list[str]:
    out = subprocess.run(["git", "-C", str(root), "ls-files", "-z"], check=True, capture_output=True).stdout
    return [p for p in out.decode("utf-8").split("\0") if p]


def scan_tree(root: Path) -> list[Finding]:
    found = []
    for rel in tracked_files(root):
        pinned = FIXTURE_FILES.get(rel)
        if pinned is not None:
            try:
                if sha256((root / rel).read_bytes()) == pinned:
                    continue
            except OSError:
                pass
            found.append(Finding(rel, "file", "excepted fixture changed since review, so it is scanned like any other file"))
        found += scan_file(root / rel, rel)
    return found


def scan_paths(paths: Iterable[str]) -> list[Finding]:
    found = []
    for raw in paths:
        p = Path(raw)
        files = sorted(x for x in p.rglob("*") if x.is_file()) if p.is_dir() else [p]
        for f in files:
            found += scan_file(f, f.as_posix())
    return found


def main(argv: list[str]) -> int:
    if any(a in {"-h", "--help"} for a in argv):
        print(__doc__)
        return 0
    if argv:
        missing = [a for a in argv if not os.path.exists(a)]
        if missing:
            print(f"export-privacy: no such path: {', '.join(missing)}", file=sys.stderr)
            return 2
        findings = scan_paths(argv)
    else:
        root = Path(__file__).resolve().parents[2]
        findings = scan_tree(root)
    for f in findings:
        print(f"export-privacy: {f}", file=sys.stderr)
    if findings:
        print(f"export-privacy: {len(findings)} finding(s). Do not commit exported node state; see scripts/ci/export_privacy.py.", file=sys.stderr)
        return 1
    print("export-privacy: no exported node state or personal device names in the checked files.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
