#!/usr/bin/env python3
"""CycloneDX 1.5 SBOM from the lock files that pin what is shipped (R2).

Every third-party component comes from a lock file: the workspace
`Cargo.lock`, the desktop's `src-tauri/Cargo.lock`, and the desktop's npm
`package-lock.json`. Nothing is resolved over the network and nothing is
built, so the same lock files always give the same bytes (no timestamp; the
serial number is derived from the content).

    python3 scripts/release/sbom.py --out arc-chain.cdx.json \\
        --cargo-lock Cargo.lock --cargo-lock desktop/src-tauri/Cargo.lock \\
        --npm-lock desktop/package-lock.json
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import re
import sys
import uuid
from pathlib import Path
from urllib.parse import quote

CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"


def parse_cargo_lock(text: str) -> list[dict]:
    """The [[package]] blocks of a Cargo.lock (v3/v4)."""
    packages, current, in_deps = [], None, False
    for raw in text.splitlines():
        line = raw.strip()
        if line == "[[package]]":
            current = {"dependencies": []}
            packages.append(current)
            in_deps = False
            continue
        if current is None or not line or line.startswith("#"):
            continue
        if in_deps:
            if line == "]":
                in_deps = False
            else:
                current["dependencies"].append(line.strip('",'))
            continue
        match = re.fullmatch(r'(\w+) = (.*)', line)
        if not match:
            continue
        key, value = match.groups()
        if key == "dependencies":
            if value.strip() == "[":
                in_deps = True
            else:
                current["dependencies"] = re.findall(r'"([^"]+)"', value)
        else:
            current[key] = value.strip('"')
    return packages


def cargo_purl(package: dict) -> str:
    purl = f"pkg:cargo/{quote(package['name'])}@{quote(package['version'])}"
    source = package.get("source")
    if source and source.startswith("git+"):
        purl += "?vcs_url=" + quote(source[len("git+"):], safe="")
    return purl


def cargo_components(lock: str, origin: str) -> tuple[list[dict], list[dict]]:
    packages = parse_cargo_lock(lock)
    by_name: dict[str, list[dict]] = {}
    for package in packages:
        by_name.setdefault(package["name"], []).append(package)

    def resolve(spec: str) -> str | None:
        # "name", "name version" or "name version (source)"
        parts = spec.split(" ", 2)
        candidates = by_name.get(parts[0], [])
        if len(parts) > 1:
            candidates = [p for p in candidates if p["version"] == parts[1]]
        return cargo_purl(candidates[0]) if len(candidates) == 1 else None

    components, dependencies = [], []
    for package in packages:
        ref = cargo_purl(package)
        source = package.get("source")
        component = {
            "type": "library",
            "bom-ref": ref,
            "name": package["name"],
            "version": package["version"],
            "purl": ref,
            "properties": [
                {"name": "arc:lockfile", "value": origin},
                {"name": "arc:source", "value": "workspace" if source is None
                 else "crates.io" if source == CRATES_IO else source},
            ],
        }
        if package.get("checksum"):
            component["hashes"] = [{"alg": "SHA-256", "content": package["checksum"]}]
        components.append(component)
        resolved = [r for r in (resolve(d) for d in package["dependencies"]) if r]
        dependencies.append({"ref": ref, "dependsOn": sorted(set(resolved))})
    return components, dependencies


def npm_components(lock: dict, origin: str) -> list[dict]:
    components = []
    for path, package in sorted(lock.get("packages", {}).items()):
        if not path:  # the root project itself
            continue
        name = package.get("name") or path.split("node_modules/")[-1]
        version = package.get("version")
        if not version:
            continue
        ref = f"pkg:npm/{quote(name, safe='')}@{quote(version)}"
        component = {
            "type": "library",
            "bom-ref": ref,
            "name": name,
            "version": version,
            "purl": ref,
            "properties": [
                {"name": "arc:lockfile", "value": origin},
                {"name": "arc:scope", "value": "dev" if package.get("dev") else "runtime"},
            ],
        }
        integrity = package.get("integrity", "")
        if integrity.startswith("sha512-"):
            digest = base64.b64decode(integrity[len("sha512-"):]).hex()
            component["hashes"] = [{"alg": "SHA-512", "content": digest}]
        if package.get("license"):
            component["licenses"] = [{"license": {"name": package["license"]}}]
        if package.get("resolved"):
            component["externalReferences"] = [{"type": "distribution", "url": package["resolved"]}]
        components.append(component)
    return components


def build(product: str, version: str, cargo_locks: list[Path], npm_locks: list[Path]) -> dict:
    components: dict[str, dict] = {}
    dependencies: dict[str, set] = {}
    for path in cargo_locks:
        parts, deps = cargo_components(path.read_text(), str(path))
        for component in parts:
            existing = components.setdefault(component["bom-ref"], component)
            if existing is not component:
                existing["properties"].append({"name": "arc:lockfile", "value": str(path)})
        for dep in deps:
            dependencies.setdefault(dep["ref"], set()).update(dep["dependsOn"])
    for path in npm_locks:
        for component in npm_components(json.loads(path.read_text()), str(path)):
            existing = components.setdefault(component["bom-ref"], component)
            if existing is not component:
                existing["properties"].append({"name": "arc:lockfile", "value": str(path)})
    ordered = [components[key] for key in sorted(components)]
    body = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "component": {"type": "application", "name": product, "version": version,
                          "bom-ref": f"{product}@{version}"},
            "tools": {"components": [{"type": "application", "name": "arc-sbom",
                                      "description": "scripts/release/sbom.py (lock files only)"}]},
        },
        "components": ordered,
        "dependencies": [{"ref": ref, "dependsOn": sorted(deps)}
                         for ref, deps in sorted(dependencies.items())],
    }
    digest = hashlib.sha256(json.dumps(body, sort_keys=True).encode()).digest()
    body["serialNumber"] = f"urn:uuid:{uuid.UUID(bytes=digest[:16], version=5)}"
    return body


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--product", default="arc-chain")
    parser.add_argument("--version", default="0.8.6")
    parser.add_argument("--cargo-lock", type=Path, action="append", default=[])
    parser.add_argument("--npm-lock", type=Path, action="append", default=[])
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv[1:])
    if not args.cargo_lock and not args.npm_lock:
        parser.error("name at least one lock file")
    sbom = build(args.product, args.version, args.cargo_lock, args.npm_lock)
    args.out.write_text(json.dumps(sbom, indent=1, sort_keys=True) + "\n")
    print(f"{args.out}: {len(sbom['components'])} components, "
          f"{sum(1 for c in sbom['components'] if 'hashes' in c)} with a pinned hash")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
