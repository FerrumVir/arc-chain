#!/usr/bin/env python3
"""Audit provenance-pinned path patches as their canonical registry crates."""

from __future__ import annotations

import dataclasses
import hashlib
import json
import pathlib
import sys
import tomllib
from typing import Any


REGISTRY_SOURCE = "registry+https://github.com/rust-lang/crates.io-index"
REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]


@dataclasses.dataclass(frozen=True)
class PackageSpec:
    name: str
    version: str
    manifest: str
    manifest_sha256: str
    patch_path: str
    provenance: str
    provenance_sha256: str
    archive_sha256: str

    @property
    def registry_id(self) -> str:
        return f"{REGISTRY_SOURCE}#{self.name}@{self.version}"


@dataclasses.dataclass(frozen=True)
class ProfileSpec:
    manifest: str
    packages: tuple[PackageSpec, ...]
    expected_advisory: str | None = None
    expected_alias: str | None = None


ROOT_PACKAGES = (
    PackageSpec(
        name="wasmer-derive",
        version="6.1.0",
        manifest="vendor/wasmer-derive/Cargo.toml",
        manifest_sha256="efdea62889adca513fa4872acd4a0c848f61baf83cfa25e83e167fcc00a1ce38",
        patch_path="vendor/wasmer-derive",
        provenance="vendor/wasmer-derive/PROVENANCE.md",
        provenance_sha256="50bdd0be11316bdb03dcdc0c1154370fbc90e65848802bb2830f483b6b851095",
        archive_sha256="c546f3380840cd63fdcc390f04cd19002f2dfa19b4691b77ecbd27642bd93452",
    ),
    PackageSpec(
        name="shared-buffer",
        version="0.1.4",
        manifest="vendor/shared-buffer/Cargo.toml",
        manifest_sha256="f0bd49214e65470705d249096c9a7b794f7fe8b50440d16c9f7a1f003415eaeb",
        patch_path="vendor/shared-buffer",
        provenance="vendor/shared-buffer/ARC-PROVENANCE.md",
        provenance_sha256="655b1ca19541f9b70b162c51d2aad9084d24d0923a92314af7d3def6e1945118",
        archive_sha256="f6c99835bad52957e7aa241d3975ed17c1e5f8c92026377d117a606f36b84b16",
    ),
    PackageSpec(
        name="wasmer-compiler",
        version="6.1.0",
        manifest="vendor/wasmer-compiler/Cargo.toml",
        manifest_sha256="4f8183fb8c5f90a3f226f2962b5c8e61542ba1902451a6d6ac3b28cc08517b9a",
        patch_path="vendor/wasmer-compiler",
        provenance="vendor/wasmer-compiler/ARC-PROVENANCE.md",
        provenance_sha256="5209c1e19b3ffaa6ff6b68ab04f4569d9d2685ec1a004d739fc9b0658dd32cc6",
        archive_sha256="4946475adc0af265af8f10aadf4d4a3c64845bcd3801c655bdd81ce5e3ee869b",
    ),
)

GLIB_PACKAGE = PackageSpec(
    name="glib",
    version="0.18.5",
    manifest="vendor/third_party/glib-0.18.5/Cargo.toml",
    manifest_sha256="bcd52d812b4c111864ae8e88ba6c0a8311eb0cf781b5e81634950b4619592132",
    patch_path="../../vendor/third_party/glib-0.18.5",
    provenance="vendor/third_party/glib-0.18.5/ARC-PROVENANCE.md",
    provenance_sha256="dd88a25d1e8bb545a37e7b08418e52cf2aa1319d604a60ebd49d2981d825abd1",
    archive_sha256="233daaf6e83ae6a12a52055f568f9d7cf4671dabb78ff9560ab6da230ce00ee5",
)

PROFILES = {
    "root": ProfileSpec(manifest="Cargo.toml", packages=ROOT_PACKAGES),
    "desktop": ProfileSpec(
        manifest="desktop/src-tauri/Cargo.toml",
        packages=(GLIB_PACKAGE,),
        expected_advisory="RUSTSEC-2024-0429",
        expected_alias="GHSA-wrw7-89jp-8q8g",
    ),
}

REPORT_KEYS = {"lockfile", "settings", "vulnerabilities", "warnings"}
SETTINGS_KEYS = {
    "ignore",
    "informational_warnings",
    "severity",
    "target_arch",
    "target_os",
}
ENTRY_KEYS = {"advisory", "affected", "kind", "package", "versions"}
PACKAGE_KEYS = {"checksum", "dependencies", "name", "replace", "source", "version"}
ADVISORY_KEYS = {
    "aliases",
    "categories",
    "collection",
    "cvss",
    "date",
    "description",
    "expect-deleted",
    "id",
    "informational",
    "keywords",
    "license",
    "package",
    "references",
    "related",
    "source",
    "title",
    "url",
    "withdrawn",
}
VERSIONS_KEYS = {"patched", "unaffected"}
BASIC_DIAGNOSTIC_KEYS = {"code", "graphs", "labels", "message", "severity"}
ADVISORY_DIAGNOSTIC_KEYS = BASIC_DIAGNOSTIC_KEYS | {"advisory", "notes"}


def fail(message: str) -> None:
    raise SystemExit(message)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def load_object(path: pathlib.Path, label: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text())
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"unable to read {label}: {error}")
    if not isinstance(value, dict):
        fail(f"{label} must be a JSON object")
    return value


def load_profile(name: str) -> ProfileSpec:
    try:
        return PROFILES[name]
    except KeyError:
        fail(f"unsupported vendored advisory profile: {name!r}")


def read_regular(path: pathlib.Path, label: str) -> bytes:
    if path.is_symlink() or not path.is_file():
        fail(f"{label} is missing, linked, or not a regular file: {path}")
    try:
        return path.read_bytes()
    except OSError as error:
        fail(f"unable to read {label}: {error}")


def validate_profile_sources(profile: ProfileSpec) -> None:
    profile_manifest_path = REPO_ROOT / profile.manifest
    manifest_bytes = read_regular(profile_manifest_path, "profile Cargo manifest")
    try:
        profile_manifest = tomllib.loads(manifest_bytes.decode())
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        fail(f"unable to parse profile Cargo manifest: {error}")
    patches = profile_manifest.get("patch", {}).get("crates-io")
    expected_names = {spec.name for spec in profile.packages}
    if not isinstance(patches, dict) or set(patches) != expected_names:
        found = sorted(patches) if isinstance(patches, dict) else patches
        fail(f"{profile.manifest} path-patch set drifted: {found!r}")

    for spec in profile.packages:
        patch = patches.get(spec.name)
        if patch != {"path": spec.patch_path}:
            fail(f"{spec.name} path patch drifted: {patch!r}")
        source_manifest_path = REPO_ROOT / spec.manifest
        source_manifest = read_regular(source_manifest_path, f"{spec.name} manifest")
        if sha256(source_manifest) != spec.manifest_sha256:
            fail(f"{spec.name} reviewed manifest hash drifted")
        try:
            parsed = tomllib.loads(source_manifest.decode())
        except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
            fail(f"unable to parse {spec.name} manifest: {error}")
        package = parsed.get("package")
        if not isinstance(package, dict) or (
            package.get("name"),
            package.get("version"),
        ) != (spec.name, spec.version):
            fail(f"{spec.name} reviewed package identity drifted")
        try:
            patched_directory = (profile_manifest_path.parent / spec.patch_path).resolve(
                strict=True
            )
            expected_directory = source_manifest_path.parent.resolve(strict=True)
        except OSError as error:
            fail(f"unable to resolve {spec.name} path patch: {error}")
        if patched_directory != expected_directory:
            fail(f"{spec.name} path patch resolves outside its reviewed tree")

        provenance_path = REPO_ROOT / spec.provenance
        provenance_bytes = read_regular(provenance_path, f"{spec.name} provenance")
        if sha256(provenance_bytes) != spec.provenance_sha256:
            fail(f"{spec.name} provenance document hash drifted")
        try:
            provenance = provenance_bytes.decode()
        except UnicodeDecodeError as error:
            fail(f"unable to decode {spec.name} provenance: {error}")
        if provenance.count(spec.archive_sha256) != 1:
            fail(f"{spec.name} provenance does not bind exactly one registry archive checksum")


def replace_exact(value: Any, old: str, new: str) -> Any:
    if isinstance(value, dict):
        return {key: replace_exact(item, old, new) for key, item in value.items()}
    if isinstance(value, list):
        return [replace_exact(item, old, new) for item in value]
    return new if value == old else value


def count_exact(value: Any, expected: str) -> int:
    if isinstance(value, dict):
        return sum(count_exact(item, expected) for item in value.values())
    if isinstance(value, list):
        return sum(count_exact(item, expected) for item in value)
    return int(value == expected)


def non_dev_dependencies(node: dict[str, Any]) -> list[str]:
    deps = node.get("deps")
    if not isinstance(deps, list):
        fail("cargo metadata resolve-node dependency shape drifted")
    package_ids: list[str] = []
    for dependency in deps:
        if not isinstance(dependency, dict):
            fail("cargo metadata dependency entry shape drifted")
        package_id = dependency.get("pkg")
        kinds = dependency.get("dep_kinds")
        if not isinstance(package_id, str) or not isinstance(kinds, list) or not kinds:
            fail("cargo metadata dependency identity shape drifted")
        include = False
        for kind in kinds:
            if not isinstance(kind, dict) or set(kind) != {"kind", "target"}:
                fail("cargo metadata dependency-kind shape drifted")
            dependency_kind = kind.get("kind")
            if dependency_kind not in (None, "build", "dev"):
                fail(f"cargo metadata dependency kind drifted: {dependency_kind!r}")
            include |= dependency_kind != "dev"
        if include:
            package_ids.append(package_id)
    return package_ids


def rewrite_metadata(
    profile: ProfileSpec, source: pathlib.Path, destination: pathlib.Path
) -> None:
    validate_profile_sources(profile)
    metadata = load_object(source, "cargo metadata")
    packages = metadata.get("packages")
    resolve = metadata.get("resolve")
    workspace_members = metadata.get("workspace_members")
    if not isinstance(packages, list):
        fail("cargo metadata packages shape drifted")
    if not isinstance(resolve, dict) or set(resolve) != {"nodes", "root"}:
        fail("cargo metadata resolve shape drifted")
    nodes = resolve.get("nodes")
    if not isinstance(nodes, list) or not isinstance(workspace_members, list):
        fail("cargo metadata resolved-graph shape drifted")
    if not all(isinstance(item, str) for item in workspace_members):
        fail("cargo metadata workspace-member shape drifted")
    if len(set(workspace_members)) != len(workspace_members):
        fail("cargo metadata contains duplicate workspace members")

    node_by_id: dict[str, dict[str, Any]] = {}
    for node in nodes:
        if not isinstance(node, dict):
            fail("cargo metadata resolve-node shape drifted")
        node_id = node.get("id")
        if not isinstance(node_id, str) or node_id in node_by_id:
            fail("cargo metadata resolve-node identity drifted")
        non_dev_dependencies(node)
        node_by_id[node_id] = node

    selected: list[tuple[PackageSpec, str]] = []
    for spec in profile.packages:
        named = [
            package
            for package in packages
            if isinstance(package, dict) and package.get("name") == spec.name
        ]
        if len(named) != 1:
            fail(f"cargo metadata must contain exactly one {spec.name} package, found {len(named)}")
        package = named[0]
        if package.get("version") != spec.version:
            fail(f"cargo metadata resolved unexpected {spec.name} version: {package.get('version')!r}")
        if package.get("source") is not None or package.get("checksum") is not None:
            fail(f"vendored {spec.name} is no longer an unchecksummed local-path package")
        try:
            actual_manifest = pathlib.Path(package["manifest_path"]).resolve(strict=True)
            expected_manifest = (REPO_ROOT / spec.manifest).resolve(strict=True)
        except (KeyError, OSError, TypeError) as error:
            fail(f"unable to validate vendored {spec.name} manifest path: {error}")
        if actual_manifest != expected_manifest:
            fail(f"cargo metadata resolved {spec.name} outside the reviewed tree")
        old_id = package.get("id")
        if not isinstance(old_id, str) or not old_id.startswith("path+file:"):
            fail(f"vendored {spec.name} package ID is not a local path: {old_id!r}")
        if old_id not in node_by_id:
            fail(f"vendored {spec.name} is absent from the resolved graph")
        if count_exact(metadata, spec.registry_id):
            fail(f"canonical {spec.name} registry identity already exists in cargo metadata")
        selected.append((spec, old_id))

    selected_ids = {old_id for _, old_id in selected}
    pending = [member for member in workspace_members if member not in selected_ids]
    if not pending:
        fail("cargo metadata has no independent workspace root for reachability proof")
    reachable: set[str] = set()
    while pending:
        package_id = pending.pop()
        if package_id in reachable:
            continue
        node = node_by_id.get(package_id)
        if node is None:
            fail(f"cargo metadata workspace/dependency ID is unresolved: {package_id!r}")
        reachable.add(package_id)
        pending.extend(non_dev_dependencies(node))
    for spec, old_id in selected:
        if old_id not in reachable:
            fail(f"vendored {spec.name} is not reachable from a non-dev workspace graph")

    for spec, old_id in selected:
        metadata = replace_exact(metadata, old_id, spec.registry_id)
        shadow_packages = [
            item
            for item in metadata["packages"]
            if isinstance(item, dict) and item.get("id") == spec.registry_id
        ]
        if len(shadow_packages) != 1:
            fail(f"failed to create exactly one canonical {spec.name} registry shadow")
        shadow_packages[0]["source"] = REGISTRY_SOURCE
        shadow_packages[0]["checksum"] = spec.archive_sha256
        if count_exact(metadata, old_id):
            fail(f"local {spec.name} identity remained in registry-shadow metadata")

    projected_packages: list[dict[str, Any]] = []
    for spec in profile.packages:
        matches = [
            item
            for item in metadata["packages"]
            if isinstance(item, dict) and item.get("id") == spec.registry_id
        ]
        if len(matches) != 1:
            fail(f"registry-shadow projection lost {spec.name}")
        projected = dict(matches[0])
        # This second metadata document is advisory-only. Reachability was
        # proven against the unmodified resolved graph above; removing all
        # other packages lets the live scan use an empty ignore list without
        # turning accepted advisories in unrelated dependencies into errors.
        projected["dependencies"] = []
        projected_packages.append(projected)

    projected_ids = [spec.registry_id for spec in profile.packages]
    metadata["packages"] = projected_packages
    metadata["workspace_members"] = projected_ids
    metadata["workspace_default_members"] = projected_ids
    metadata["resolve"] = {
        "root": None,
        "nodes": [
            {
                "id": spec.registry_id,
                "dependencies": [],
                "deps": [],
                "features": [],
            }
            for spec in profile.packages
        ],
    }

    canonical = {
        item.get("name")
        for item in metadata["packages"]
        if isinstance(item, dict)
        and item.get("id") in {spec.registry_id for spec in profile.packages}
        and item.get("source") == REGISTRY_SOURCE
    }
    expected = {spec.name for spec in profile.packages}
    if canonical != expected:
        fail(f"registry-shadow package set drifted: {sorted(canonical)}")
    try:
        destination.write_text(json.dumps(metadata, sort_keys=True, separators=(",", ":")) + "\n")
    except OSError as error:
        fail(f"unable to write registry-shadow metadata: {error}")


def report_entries(report: dict[str, Any]) -> list[tuple[str, str | None, dict[str, Any]]]:
    if set(report) != REPORT_KEYS:
        fail(f"cargo-deny audit report shape drifted: {sorted(report)}")
    lockfile = report["lockfile"]
    settings = report["settings"]
    vulnerabilities = report["vulnerabilities"]
    warnings = report["warnings"]
    if (
        not isinstance(lockfile, dict)
        or set(lockfile) != {"dependency-count"}
        or not isinstance(lockfile.get("dependency-count"), int)
        or lockfile["dependency-count"] < 0
    ):
        fail("cargo-deny advisory lockfile shape drifted")
    if not isinstance(settings, dict) or set(settings) != SETTINGS_KEYS:
        fail("cargo-deny advisory settings shape drifted")
    if not isinstance(vulnerabilities, list) or not isinstance(warnings, dict):
        fail("cargo-deny advisory collection shape drifted")

    entries: list[tuple[str, str | None, dict[str, Any]]] = []
    for item in vulnerabilities:
        if not isinstance(item, dict):
            fail("cargo-deny vulnerability entry shape drifted")
        entries.append(("vulnerabilities", None, item))
    for kind, items in warnings.items():
        if not isinstance(kind, str) or not isinstance(items, list):
            fail("cargo-deny warning entry shape drifted")
        for item in items:
            if not isinstance(item, dict):
                fail("cargo-deny warning item shape drifted")
            entries.append(("warnings", kind, item))

    for _, _, item in entries:
        if set(item) != ENTRY_KEYS:
            fail("cargo-deny advisory item shape drifted")
        package = item.get("package")
        advisory = item.get("advisory")
        versions = item.get("versions")
        if not isinstance(package, dict) or set(package) != PACKAGE_KEYS:
            fail("cargo-deny advisory package shape drifted")
        if not isinstance(advisory, dict) or set(advisory) != ADVISORY_KEYS:
            fail("cargo-deny advisory record shape drifted")
        if not isinstance(versions, dict) or set(versions) != VERSIONS_KEYS:
            fail("cargo-deny advisory versions shape drifted")
        if not isinstance(item.get("kind"), str):
            fail("cargo-deny advisory kind shape drifted")
        if not isinstance(package.get("name"), str) or not isinstance(package.get("version"), str):
            fail("cargo-deny advisory package identity drifted")
        if not isinstance(advisory.get("id"), str) or not isinstance(advisory.get("aliases"), list):
            fail("cargo-deny advisory identity shape drifted")
        if not all(isinstance(alias, str) for alias in advisory["aliases"]):
            fail("cargo-deny advisory aliases shape drifted")
        if not all(
            isinstance(ranges, list)
            and all(isinstance(version_range, str) for version_range in ranges)
            for ranges in versions.values()
        ):
            fail("cargo-deny advisory version ranges shape drifted")
    return entries


def load_policy(path: pathlib.Path) -> tuple[dict[str, Any], list[str]]:
    try:
        deny = tomllib.loads(path.read_text())
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        fail(f"unable to read cargo-deny policy: {error}")
    advisories = deny.get("advisories")
    if (
        not isinstance(advisories, dict)
        or advisories.get("version") != 2
        or advisories.get("unsound") != "all"
        or advisories.get("yanked") != "deny"
    ):
        fail("cargo-deny advisory policy shape drifted")
    ignored = advisories.get("ignore")
    if not isinstance(ignored, list) or not all(isinstance(item, str) for item in ignored):
        fail("cargo-deny advisory ignore policy shape drifted")
    if len(set(ignored)) != len(ignored):
        fail("cargo-deny advisory ignore policy contains duplicates")
    return advisories, ignored


def write_shadow_policy(source: pathlib.Path, destination: pathlib.Path) -> None:
    """Derive an advisory-only policy that cannot suppress a vendored finding."""
    load_policy(source)
    policy = (
        "# Generated by vendored-advisory-shadow.py; do not add suppressions.\n"
        "[graph]\n"
        "all-features = false\n"
        "\n"
        "[advisories]\n"
        "version = 2\n"
        'unsound = "all"\n'
        'yanked = "deny"\n'
        "ignore = []\n"
    )
    try:
        destination.write_text(policy)
    except OSError as error:
        fail(f"unable to write suppression-free registry-shadow policy: {error}")


def load_diagnostics(path: pathlib.Path) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    try:
        lines = [line for line in path.read_text().splitlines() if line.strip()]
    except (OSError, UnicodeDecodeError) as error:
        fail(f"unable to read cargo-deny registry-shadow diagnostics: {error}")
    diagnostics: list[dict[str, Any]] = []
    summaries: list[dict[str, Any]] = []
    for line in lines:
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            fail(f"cargo-deny registry-shadow diagnostics are not JSON: {error}")
        if not isinstance(value, dict) or set(value) != {"fields", "type"}:
            fail("cargo-deny registry-shadow diagnostic shape drifted")
        fields = value.get("fields")
        if not isinstance(fields, dict):
            fail("cargo-deny registry-shadow diagnostic fields drifted")
        if value["type"] == "diagnostic":
            if set(fields) not in (BASIC_DIAGNOSTIC_KEYS, ADVISORY_DIAGNOSTIC_KEYS):
                fail("cargo-deny registry-shadow diagnostic payload shape drifted")
            if (
                not isinstance(fields.get("code"), str)
                or not isinstance(fields.get("message"), str)
                or fields.get("severity") not in ("error", "warning", "note", "help")
                or not isinstance(fields.get("graphs"), list)
                or not isinstance(fields.get("labels"), list)
            ):
                fail("cargo-deny registry-shadow diagnostic payload type drifted")
            if set(fields) == ADVISORY_DIAGNOSTIC_KEYS and (
                not isinstance(fields.get("advisory"), dict)
                or not isinstance(fields.get("notes"), list)
            ):
                fail("cargo-deny registry-shadow advisory diagnostic shape drifted")
            diagnostics.append(value)
        elif value["type"] == "summary":
            if set(fields) != {"advisories"}:
                fail("cargo-deny registry-shadow summary shape drifted")
            stats = fields.get("advisories")
            if (
                not isinstance(stats, dict)
                or set(stats) != {"errors", "helps", "notes", "warnings"}
                or not all(isinstance(item, int) and item >= 0 for item in stats.values())
            ):
                fail("cargo-deny registry-shadow advisory summary shape drifted")
            summaries.append(value)
        else:
            fail(f"unexpected cargo-deny registry-shadow message type: {value['type']!r}")
    if len(summaries) != 1:
        fail(f"expected exactly one cargo-deny advisory summary, found {len(summaries)}")
    return diagnostics, summaries[0]


def load_report(path: pathlib.Path) -> dict[str, Any]:
    try:
        lines = [line for line in path.read_text().splitlines() if line.strip()]
    except (OSError, UnicodeDecodeError) as error:
        fail(f"unable to read cargo-deny registry-shadow report: {error}")
    if len(lines) != 1:
        fail(f"expected exactly one cargo-deny advisory database report, found {len(lines)}")
    try:
        report = json.loads(lines[0])
    except json.JSONDecodeError as error:
        fail(f"cargo-deny registry-shadow report is not JSON: {error}")
    if not isinstance(report, dict):
        fail("cargo-deny registry-shadow report must be an object")
    return report


def verify_report(
    profile: ProfileSpec,
    path: pathlib.Path,
    diagnostics_path: pathlib.Path,
    deny_path: pathlib.Path,
    scan_status: int,
) -> None:
    validate_profile_sources(profile)
    _, policy_ignored = load_policy(deny_path)
    if policy_ignored:
        fail("registry-shadow advisory policy must be suppression-free")
    report = load_report(path)
    entries = report_entries(report)
    if report["lockfile"]["dependency-count"] != len(profile.packages):
        fail("cargo-deny registry-shadow package count drifted")
    settings = report["settings"]
    report_ignored = settings.get("ignore")
    if report_ignored != sorted(policy_ignored):
        fail("cargo-deny advisory ignore settings do not exactly match policy")
    if (
        settings.get("informational_warnings") != ["notice", "unmaintained", "unsound"]
        or settings.get("severity") is not None
        or settings.get("target_arch") != []
        or settings.get("target_os") != []
    ):
        fail("cargo-deny advisory runtime settings drifted")

    diagnostics, summary = load_diagnostics(diagnostics_path)
    stats = summary["fields"]["advisories"]
    error_diagnostics = [
        item["fields"] for item in diagnostics if item["fields"].get("severity") == "error"
    ]
    if stats["errors"] != len(error_diagnostics):
        fail("cargo-deny registry-shadow error accounting drifted")
    if scan_status not in (0, 1):
        fail(f"cargo-deny registry-shadow exited unexpectedly: {scan_status}")

    target_names = {spec.name for spec in profile.packages}
    target_entries: list[tuple[str, str | None, dict[str, Any]]] = []
    ignored = set(policy_ignored)
    for entry in entries:
        advisory = entry[2]["advisory"]
        package = entry[2]["package"]
        if package["name"] not in target_names:
            fail(f"registry-shadow report contains an unselected package: {package['name']}")
        target_entries.append(entry)
        advisory_ids = {advisory["id"], *advisory["aliases"]}
        suppressed = sorted(advisory_ids & ignored)
        if suppressed:
            fail(
                f"vendored {package['name']} advisory is hidden by deny.toml: {suppressed}"
            )

    if profile.expected_advisory is None:
        if target_entries:
            found = [
                (entry[2]["package"]["name"], entry[2]["advisory"]["id"])
                for entry in target_entries
            ]
            fail(f"root vendored packages have live advisory findings: {found}")
        if scan_status != 0 or stats["errors"] != 0 or error_diagnostics:
            fail("clean root registry-shadow scan status drifted")
        return

    if profile.expected_advisory in ignored or profile.expected_alias in ignored:
        fail("the glib backport advisory must not be suppressed")
    if len(target_entries) != 1:
        found = [entry[2]["advisory"].get("id") for entry in target_entries]
        fail(f"expected exactly {profile.expected_advisory} for glib, found {found}")
    section, kind, item = target_entries[0]
    package = item["package"]
    advisory = item["advisory"]
    if (section, kind, item.get("kind")) != ("warnings", "unsound", "unsound"):
        fail("the reviewed glib advisory classification drifted")
    if package.get("version") != GLIB_PACKAGE.version or package.get("source") != REGISTRY_SOURCE:
        fail("cargo-deny did not scan the canonical glib 0.18.5 registry identity")
    if advisory.get("id") != profile.expected_advisory or advisory.get("package") != "glib":
        fail(f"unexpected glib advisory: {advisory.get('id')!r}")
    if advisory.get("aliases") != [profile.expected_alias]:
        fail(f"the reviewed glib advisory aliases drifted: {advisory.get('aliases')!r}")
    if advisory.get("informational") != "unsound":
        fail("the reviewed glib advisory metadata drifted")
    if item.get("versions") != {"patched": [">=0.20.0"], "unaffected": ["<0.15.0"]}:
        fail(f"the reviewed glib affected-version contract drifted: {item.get('versions')!r}")
    if scan_status != 1 or stats["errors"] != 1 or len(error_diagnostics) != 1:
        fail("cargo-deny transitive-unsound registry-shadow status drifted")
    if error_diagnostics[0].get("code") != "unsound":
        fail("cargo-deny registry-shadow failed for an unexpected reason")


def main(argv: list[str]) -> None:
    if len(argv) == 4 and argv[1] == "write-policy":
        write_shadow_policy(pathlib.Path(argv[2]), pathlib.Path(argv[3]))
        return
    if len(argv) == 5 and argv[1] == "rewrite-metadata":
        rewrite_metadata(
            load_profile(argv[2]),
            pathlib.Path(argv[3]),
            pathlib.Path(argv[4]),
        )
        return
    if len(argv) == 7 and argv[1] == "verify-report":
        try:
            scan_status = int(argv[6])
        except ValueError:
            fail("cargo-deny scan status must be an integer")
        verify_report(
            load_profile(argv[2]),
            pathlib.Path(argv[3]),
            pathlib.Path(argv[4]),
            pathlib.Path(argv[5]),
            scan_status,
        )
        return
    fail(
        "usage: vendored-advisory-shadow.py "
        "write-policy <source-deny-config> <output> | "
        "rewrite-metadata <root|desktop> <input> <output> | "
        "verify-report <root|desktop> <report> <diagnostics> <deny-config> <scan-status>"
    )


if __name__ == "__main__":
    main(sys.argv)
