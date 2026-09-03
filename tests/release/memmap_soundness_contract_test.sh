#!/usr/bin/env bash
set -euo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"

python3 - "$REPO_ROOT" <<'PY'
import hashlib
import json
import os
import pathlib
import subprocess
import sys
import tomllib

root = pathlib.Path(sys.argv[1]).resolve()


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(message)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def archive_member_tree(
    directory: pathlib.Path,
    *,
    excluded: set[str],
    manifest_override: bytes | None = None,
) -> tuple[int, str]:
    files: list[pathlib.Path] = []
    for path in directory.rglob("*"):
        relative = path.relative_to(directory).as_posix()
        require("\n" not in relative and "\r" not in relative, f"unsafe vendor path: {relative!r}")
        require(not path.is_symlink(), f"vendor tree contains a symlink: {relative}")
        if path.is_file() and relative not in excluded:
            files.append(path)
        elif not path.is_file() and not path.is_dir():
            raise SystemExit(f"vendor tree contains a non-regular entry: {relative}")

    rows: list[bytes] = []
    for path in sorted(files, key=lambda item: item.relative_to(directory).as_posix()):
        relative = path.relative_to(directory).as_posix()
        data = manifest_override if relative == "Cargo.toml" and manifest_override is not None else path.read_bytes()
        rows.append(f"{sha256(data)}  {relative}\n".encode())
    return len(files), sha256(b"".join(rows))


specs = {
    "shared-buffer": {
        "version": "0.1.4",
        "license": "MIT OR Apache-2.0",
        "vcs": "65e72d29726a748b71f754846fef3dad7df64f61",
        "old": b'version = "0.6.1"',
        "new": b'version = "0.9.11"',
        "excluded": {"ARC-PROVENANCE.md"},
        "files": 13,
        "upstream_tree": "9093016e27b7669d0c17645033923688760dd6c4da1e8b23c67048dd34efa553",
        "patched_tree": "3c190ce902b46fb35742215734a3d00f90bffb2d6bff9da464239f77cb8b5086",
        "upstream_manifest": "aa7c01b846cfc75309c0d59a7cf5aa9f936684b5da9c8c2e23890474d2b0fe67",
        "patched_manifest": "f0bd49214e65470705d249096c9a7b794f7fe8b50440d16c9f7a1f003415eaeb",
        "orig_manifest": "0b88e0feff65c1b84d15b58edc72552c709473a3ee536f9f95df9fa39f6d7566",
        "archive": "f6c99835bad52957e7aa241d3975ed17c1e5f8c92026377d117a606f36b84b16",
        "provenance": "655b1ca19541f9b70b162c51d2aad9084d24d0923a92314af7d3def6e1945118",
        "licenses": {
            "LICENSE_MIT.md": "e25487d4fa108f45f082cb416574dd1d8888a036d733e0d6c891c78574acacb8",
            "LICENSE_APACHE.md": "62c7a1e35f56406896d7aa7ca52d0cc0d272ac022b5d2796e7d6905db8a3636a",
        },
    },
    "wasmer-compiler": {
        "version": "6.1.0",
        "license": "MIT",
        "vcs": "3189527eec99cfea7e6991328509e72ed0bec2e0",
        "old": b'version = "0.6.2"',
        "new": b'version = "0.9.11"',
        "excluded": {"ARC-PROVENANCE.md", "LICENSE"},
        "files": 48,
        "upstream_tree": "e7840ac914010cebba035656508e2be063324e8a86203cfea2782affd97f2dda",
        "patched_tree": "338c2b414786a8c34fce99045b001d360ffb4e8364bf9b1de248bc2ca54326b3",
        "upstream_manifest": "eba4475a226e9e1c9a72f9726041e7dc231a1d19df482d9c84dd425637966878",
        "patched_manifest": "4f8183fb8c5f90a3f226f2962b5c8e61542ba1902451a6d6ac3b28cc08517b9a",
        "orig_manifest": "01a0233523acde44a2354252705042b9cf52d3e6a3151b362381043b5b805b3b",
        "archive": "4946475adc0af265af8f10aadf4d4a3c64845bcd3801c655bdd81ce5e3ee869b",
        "provenance": "5209c1e19b3ffaa6ff6b68ab04f4569d9d2685ec1a004d739fc9b0658dd32cc6",
        "licenses": {
            "LICENSE": "76dc7d305458d07478bc62669fe53dbfd3b94b95c5e00fbb45af1f492cbd7284",
        },
    },
}

runner_entry = '"$TEST_DIR/memmap_soundness_contract_test.sh"'
runner = (root / "tests" / "release" / "run.sh").read_text()
require(
    runner.count(runner_entry) == 1,
    "release runner must invoke the memmap soundness contract exactly once",
)

attributes = (root / ".gitattributes").read_text().splitlines()
for vendor_glob in ("vendor/shared-buffer/**", "vendor/wasmer-compiler/**"):
    expected_rule = f"{vendor_glob} text eol=lf"
    matching_rules = [
        line.strip()
        for line in attributes
        if line.strip() and not line.lstrip().startswith("#") and line.split()[0] == vendor_glob
    ]
    require(
        matching_rules == [expected_rule],
        f".gitattributes must contain exactly one LF rule for {vendor_glob}",
    )

for crate, spec in specs.items():
    directory = root / "vendor" / crate
    manifest_path = directory / "Cargo.toml"
    manifest_bytes = manifest_path.read_bytes()
    require(manifest_bytes.count(spec["new"]) == 1, f"{crate} must contain one patched memmap2 requirement")
    require(spec["old"] not in manifest_bytes, f"{crate} active manifest retains the old memmap2 requirement")

    manifest = tomllib.loads(manifest_bytes.decode())
    require(manifest["package"]["name"] == crate, f"{crate} package name changed")
    require(manifest["package"]["version"] == spec["version"], f"{crate} package version changed")
    require(manifest["package"]["license"] == spec["license"], f"{crate} license expression changed")
    require(
        manifest["dependencies"]["memmap2"]["version"] == "0.9.11",
        f"{crate} does not require the patched memmap2 minimum",
    )

    vcs = json.loads((directory / ".cargo_vcs_info.json").read_text())
    require(vcs["git"]["sha1"] == spec["vcs"], f"{crate} VCS provenance changed")
    require(sha256(manifest_bytes) == spec["patched_manifest"], f"{crate} patched manifest hash changed")
    require(
        sha256((directory / "Cargo.toml.orig").read_bytes()) == spec["orig_manifest"],
        f"{crate} original manifest changed",
    )

    upstream_manifest = manifest_bytes.replace(spec["new"], spec["old"])
    require(sha256(upstream_manifest) == spec["upstream_manifest"], f"{crate} delta is not the one reviewed line")
    patched_count, patched_tree = archive_member_tree(directory, excluded=spec["excluded"])
    upstream_count, upstream_tree = archive_member_tree(
        directory,
        excluded=spec["excluded"],
        manifest_override=upstream_manifest,
    )
    require(patched_count == upstream_count == spec["files"], f"{crate} archive inventory changed")
    require(patched_tree == spec["patched_tree"], f"{crate} patched tree hash changed")
    require(upstream_tree == spec["upstream_tree"], f"{crate} upstream reconstruction hash changed")

    provenance_bytes = (directory / "ARC-PROVENANCE.md").read_bytes()
    require(sha256(provenance_bytes) == spec["provenance"], f"{crate} provenance document changed")
    provenance = provenance_bytes.decode()
    for expected in (
        spec["archive"],
        spec["vcs"],
        spec["upstream_tree"],
        spec["patched_tree"],
        "RUSTSEC-2026-0186",
    ):
        require(expected in provenance, f"{crate} provenance omits {expected}")
    for license_name, expected_hash in spec["licenses"].items():
        require(
            sha256((directory / license_name).read_bytes()) == expected_hash,
            f"{crate} {license_name} hash changed",
        )
        require(expected_hash in provenance, f"{crate} provenance omits {license_name} hash")

affected_calls = (
    "advise_range(",
    "unchecked_advise_range(",
    "flush_range(",
    "flush_async_range(",
)
for crate in specs:
    rust_source = b"\n".join(
        path.read_bytes() for path in sorted((root / "vendor" / crate / "src").rglob("*.rs"))
    ).decode(errors="strict")
    for affected_call in affected_calls:
        require(affected_call not in rust_source, f"{crate} calls affected API {affected_call}")
require(
    "memmap2" not in b"\n".join(
        path.read_bytes()
        for path in sorted((root / "vendor" / "wasmer-compiler" / "src").rglob("*.rs"))
    ).decode(errors="strict"),
    "wasmer-compiler gained a memmap2 Rust call site",
)

workspace = tomllib.loads((root / "Cargo.toml").read_text())
patches = workspace["patch"]["crates-io"]
require(patches["shared-buffer"] == {"path": "vendor/shared-buffer"}, "shared-buffer path patch changed")
require(
    patches["wasmer-compiler"] == {"path": "vendor/wasmer-compiler"},
    "wasmer-compiler path patch changed",
)
policy = tomllib.loads((root / "deny.toml").read_text())
require(policy["advisories"].get("unsound") == "all", "transitive unsound advisories are not denied")
ignored_advisories: set[str] = set()
for entry in policy["advisories"].get("ignore", []):
    if isinstance(entry, str):
        ignored_advisories.add(entry)
    elif isinstance(entry, dict):
        advisory_id = entry.get("id") or entry.get("advisory")
        require(isinstance(advisory_id, str), f"unrecognized advisory ignore table: {entry!r}")
        ignored_advisories.add(advisory_id)
    else:
        raise SystemExit(f"unrecognized advisory ignore entry: {entry!r}")
require("RUSTSEC-2026-0186" not in ignored_advisories, "memmap2 advisory is suppressed")

lock = tomllib.loads((root / "Cargo.lock").read_text())
memmaps = [package for package in lock["package"] if package["name"] == "memmap2"]
require(len(memmaps) == 1, f"locked graph must contain one memmap2 package, found {len(memmaps)}")
memmap = memmaps[0]
require(memmap["version"] == "0.9.11", f"locked memmap2 is {memmap['version']}, not 0.9.11")
require(
    memmap.get("checksum") == "d1219ed1b7f229ee7104d281dd01d6802fe28bb6e95d292942c4daacdeb798c0",
    "locked memmap2 0.9.11 registry checksum changed",
)
for crate in specs:
    matches = [
        package
        for package in lock["package"]
        if package["name"] == crate and package["version"] == specs[crate]["version"]
    ]
    require(len(matches) == 1, f"locked graph does not contain exactly one {crate}")
    package = matches[0]
    require("source" not in package and "checksum" not in package, f"{crate} is not path-resolved")
    require("memmap2" in package.get("dependencies", []), f"{crate} does not resolve memmap2")

metadata = json.loads(
    subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1"],
        cwd=root,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
        env={**os.environ, "CARGO_TERM_COLOR": "never"},
    ).stdout
)
packages = metadata["packages"]
memmap_packages = [package for package in packages if package["name"] == "memmap2"]
require(
    len(memmap_packages) == 1 and memmap_packages[0]["version"] == "0.9.11",
    "Cargo metadata resolved an unexpected memmap2 package",
)
memmap_id = memmap_packages[0]["id"]
by_name = {package["name"]: package for package in packages if package["name"] in {*specs, "arc-node"}}
nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}

for crate in specs:
    package = by_name[crate]
    require(package["source"] is None, f"{crate} metadata is not path-resolved")
    memmap_reqs = [dependency["req"] for dependency in package["dependencies"] if dependency["name"] == "memmap2"]
    require(memmap_reqs == ["^0.9.11"], f"{crate} metadata requirement changed: {memmap_reqs}")
    direct = {dependency["pkg"] for dependency in nodes[package["id"]]["deps"]}
    require(memmap_id in direct, f"{crate} does not resolve directly to memmap2 0.9.11")

node_id = by_name["arc-node"]["id"]
pending = [node_id]
reachable: set[str] = set()
while pending:
    package_id = pending.pop()
    if package_id in reachable:
        continue
    reachable.add(package_id)
    pending.extend(dependency["pkg"] for dependency in nodes[package_id]["deps"])
require(memmap_id in reachable, "arc-node does not exercise the reviewed memmap2 graph")

print("memmap soundness contract: PASS")
PY
