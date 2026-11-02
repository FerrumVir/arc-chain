#!/usr/bin/env python3
"""Prove the legacy bridge's v0.7 fixtures match every released v0.6/v0.7 tag.

The consent tests run each fixture through the bridge and the updated desktop:

* fixtures/v07-desktop-stores.json -> desktop/src-tauri/src/legacy_upgrade.rs
* fixtures/v07-argv.json           -> crates/arc-legacy-bridge/src/argv.rs

Those tests only mean something if the fixtures are what the released code
wrote and ran. This script reads each tag with `git show` (no checkout, no
build) and fails if a tag's store shape, dataDir writes, or arc-node command
lines differ from the fixtures.

Usage: check_v07_fixtures.py --repo <arc-chain checkout with the v0.6/v0.7 tags>
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
STORES = HERE / "fixtures" / "v07-desktop-stores.json"
ARGV = HERE / "fixtures" / "v07-argv.json"


def show(repo: Path, tag: str, path: str) -> str | None:
    result = subprocess.run(
        ["git", "-C", str(repo), "show", f"{tag}:{path}"],
        capture_output=True,
        text=True,
        check=False,
    )
    return result.stdout if result.returncode == 0 else None


def camel(name: str) -> str:
    head, *rest = name.split("_")
    return head + "".join(part.title() for part in rest)


def struct_fields(source: str, name: str) -> tuple[list[str], bool]:
    """Field names of `pub struct <name>` and whether it is camelCase on the wire."""
    match = re.search(
        r"((?:#\[[^\]]*\]\s*)*)pub struct " + re.escape(name) + r"\s*\{(.*?)\n\}",
        source,
        re.S,
    )
    if not match:
        raise AssertionError(f"pub struct {name} not found")
    attributes, body = match.group(1), match.group(2)
    fields = re.findall(r"^\s*pub (\w+):", body, re.M)
    return fields, 'rename_all = "camelCase"' in attributes


def check_stores(repo: Path) -> list[str]:
    fixture = json.loads(STORES.read_text(encoding="utf-8"))
    errors: list[str] = []
    for tag in fixture["tags"]:
        types = show(repo, tag, "desktop/src-tauri/src/types.rs")
        store = show(repo, tag, "desktop/src-tauri/src/store.rs")
        if types is None or store is None:
            errors.append(f"{tag}: desktop types.rs or store.rs is missing")
            continue
        fields, is_camel = struct_fields(types, "NodeConfig")
        if not is_camel or [camel(f) for f in fields] != fixture["node_config_fields"]:
            errors.append(f"{tag}: NodeConfig is {fields} (camelCase={is_camel})")
        fields, is_camel = struct_fields(types, "Identity")
        if not is_camel or [camel(f) for f in fields] != fixture["identity_fields"]:
            errors.append(f"{tag}: Identity is {fields} (camelCase={is_camel})")
        fields, _ = struct_fields(store, "Store")
        if fields != fixture["store_fields"]:
            errors.append(f"{tag}: Store is {fields}")
        defaults = re.findall(r'data_dir:\s*"([^"]*)"', types)
        if defaults != ["~/.arc"]:
            errors.append(f"{tag}: NodeConfig default data_dir is {defaults}")
        # Every place the UI builds a config must write the literal "~/.arc";
        # nothing may let the user pick another data directory.
        tree = subprocess.run(
            ["git", "-C", str(repo), "ls-tree", "-r", "--name-only", tag, "desktop/src"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.split()
        for path in tree:
            if not path.endswith((".ts", ".tsx")):
                continue
            text = show(repo, tag, path) or ""
            for line in text.splitlines():
                code = line.split("//", 1)[0]
                for value in re.findall(r"\bdataDir\s*:\s*([^,;}\n]+)", code):
                    value = value.strip()
                    if value not in ('"~/.arc"', "string"):
                        errors.append(f"{tag}: {path} sets dataDir to {value}")
        for name, entry in fixture["stores"].items():
            config = entry["store"]["config"]
            if list(config) != fixture["node_config_fields"]:
                errors.append(f"fixture {name}: config keys {list(config)}")
            if config["dataDir"] != "~/.arc" or config["role"] not in ("worker", "observer"):
                errors.append(f"fixture {name}: not a v0.7 config")
    return errors


def check_argv(repo: Path) -> list[str]:
    if not ARGV.exists():
        return [f"{ARGV} is missing"]
    fixture = json.loads(ARGV.read_text(encoding="utf-8"))
    errors: list[str] = []
    for shape in fixture["shapes"]:
        for tag in shape["tags"]:
            for check in shape["source_checks"]:
                text = show(repo, tag, check["path"])
                if text is None:
                    errors.append(f"{shape['name']} {tag}: {check['path']} is missing")
                    continue
                flat = re.sub(r"\s+", " ", text)
                for needle in check["contains"]:
                    if re.sub(r"\s+", " ", needle) not in flat:
                        errors.append(
                            f"{shape['name']} {tag}: {check['path']} no longer contains {needle!r}"
                        )
                for needle in check.get("absent", []):
                    if needle in text:
                        errors.append(f"{shape['name']} {tag}: {check['path']} contains {needle!r}")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", type=Path, default=Path("."))
    args = parser.parse_args()
    errors = check_stores(args.repo) + check_argv(args.repo)
    for error in errors:
        print(f"FAIL {error}", file=sys.stderr)
    if errors:
        return 1
    print("v0.7 desktop store and command-line fixtures match every released tag")
    return 0


if __name__ == "__main__":
    sys.exit(main())
