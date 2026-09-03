#!/usr/bin/env python3
"""Expose ARC's path-patched glib to cargo-deny's live advisory database."""

from __future__ import annotations

import json
import pathlib
import sys
import tomllib
from typing import Any


PACKAGE_NAME = "glib"
PACKAGE_VERSION = "0.18.5"
REGISTRY_SOURCE = "registry+https://github.com/rust-lang/crates.io-index"
REGISTRY_ID = f"{REGISTRY_SOURCE}#{PACKAGE_NAME}@{PACKAGE_VERSION}"
ARCHIVE_SHA256 = "233daaf6e83ae6a12a52055f568f9d7cf4671dabb78ff9560ab6da230ce00ee5"
EXPECTED_ADVISORY = "RUSTSEC-2024-0429"
EXPECTED_ALIAS = "GHSA-wrw7-89jp-8q8g"


def fail(message: str) -> None:
    raise SystemExit(message)


def load_object(path: pathlib.Path, label: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text())
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"unable to read {label}: {error}")
    if not isinstance(value, dict):
        fail(f"{label} must be a JSON object")
    return value


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


def rewrite_metadata(source: pathlib.Path, destination: pathlib.Path, manifest: pathlib.Path) -> None:
    metadata = load_object(source, "cargo metadata")
    packages = metadata.get("packages")
    if not isinstance(packages, list):
        fail("cargo metadata packages shape drifted")

    named = [package for package in packages if isinstance(package, dict) and package.get("name") == PACKAGE_NAME]
    if len(named) != 1:
        fail(f"cargo metadata must contain exactly one {PACKAGE_NAME} package, found {len(named)}")
    package = named[0]
    if package.get("version") != PACKAGE_VERSION:
        fail(f"cargo metadata resolved unexpected {PACKAGE_NAME} version: {package.get('version')!r}")
    if package.get("source") is not None or package.get("checksum") is not None:
        fail("vendored glib is no longer an unchecksummed local-path package")
    try:
        actual_manifest = pathlib.Path(package["manifest_path"]).resolve(strict=True)
        expected_manifest = manifest.resolve(strict=True)
    except (KeyError, OSError, TypeError) as error:
        fail(f"unable to validate vendored glib manifest path: {error}")
    if actual_manifest != expected_manifest:
        fail(f"cargo metadata resolved glib outside the reviewed tree: {actual_manifest}")

    old_id = package.get("id")
    if not isinstance(old_id, str) or not old_id.startswith("path+file:"):
        fail(f"vendored glib package ID is not a local path: {old_id!r}")
    if count_exact(metadata, old_id) < 2:
        fail("vendored glib package ID is not connected to the resolved graph")
    if count_exact(metadata, REGISTRY_ID):
        fail("canonical glib registry identity already exists in cargo metadata")

    metadata = replace_exact(metadata, old_id, REGISTRY_ID)
    shadow_packages = [
        item
        for item in metadata["packages"]
        if isinstance(item, dict) and item.get("id") == REGISTRY_ID
    ]
    if len(shadow_packages) != 1:
        fail("failed to create exactly one canonical glib registry shadow")
    shadow_packages[0]["source"] = REGISTRY_SOURCE
    shadow_packages[0]["checksum"] = ARCHIVE_SHA256
    if count_exact(metadata, old_id):
        fail("local glib identity remained in registry-shadow metadata")

    destination.write_text(json.dumps(metadata, sort_keys=True, separators=(",", ":")) + "\n")


def report_entries(report: dict[str, Any]) -> list[tuple[str, str | None, dict[str, Any]]]:
    expected_keys = {"lockfile", "settings", "vulnerabilities", "warnings"}
    if set(report) != expected_keys:
        fail(f"cargo-deny audit report shape drifted: {sorted(report)}")
    vulnerabilities = report["vulnerabilities"]
    warnings = report["warnings"]
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
    return entries


def load_diagnostics(path: pathlib.Path) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    try:
        lines = [line for line in path.read_text().splitlines() if line.strip()]
    except (OSError, UnicodeDecodeError) as error:
        fail(f"unable to read cargo-deny registry-shadow diagnostics: {error}")
    diagnostics = []
    summaries = []
    for line in lines:
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            fail(f"cargo-deny registry-shadow diagnostics are not JSON: {error}")
        if not isinstance(value, dict) or set(value) != {"fields", "type"}:
            fail("cargo-deny registry-shadow diagnostic shape drifted")
        if value["type"] == "diagnostic":
            diagnostics.append(value)
        elif value["type"] == "summary":
            summaries.append(value)
        else:
            fail(f"unexpected cargo-deny registry-shadow message type: {value['type']!r}")
    if len(summaries) != 1:
        fail(f"expected exactly one cargo-deny advisory summary, found {len(summaries)}")
    return diagnostics, summaries[0]


def verify_status(
    diagnostics_path: pathlib.Path, deny_path: pathlib.Path, scan_status: int
) -> None:
    try:
        deny = tomllib.loads(deny_path.read_text())
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        fail(f"unable to read cargo-deny policy: {error}")
    advisories = deny.get("advisories")
    if not isinstance(advisories, dict) or advisories.get("version") != 2:
        fail("cargo-deny advisory policy shape drifted")
    ignored = advisories.get("ignore")
    if not isinstance(ignored, list) or not all(isinstance(item, str) for item in ignored):
        fail("cargo-deny advisory ignore policy shape drifted")
    if EXPECTED_ADVISORY in ignored or EXPECTED_ALIAS in ignored:
        fail("the glib backport advisory must not be suppressed")

    diagnostics, summary = load_diagnostics(diagnostics_path)
    fields = summary.get("fields")
    stats = fields.get("advisories") if isinstance(fields, dict) else None
    if not isinstance(stats, dict) or not isinstance(stats.get("errors"), int):
        fail("cargo-deny advisory summary shape drifted")
    error_diagnostics = []
    for diagnostic in diagnostics:
        fields = diagnostic.get("fields")
        if not isinstance(fields, dict):
            fail("cargo-deny advisory diagnostic fields drifted")
        if fields.get("severity") == "error":
            error_diagnostics.append(fields)

    unsound_scope = advisories.get("unsound")
    if unsound_scope is None:
        if scan_status != 0 or stats["errors"] != 0 or error_diagnostics:
            fail(
                "cargo-deny registry-shadow scan failed outside an explicit "
                "transitive-unsound policy"
            )
    elif unsound_scope == "all":
        if scan_status != 1 or stats["errors"] != 1 or len(error_diagnostics) != 1:
            fail("cargo-deny transitive-unsound registry-shadow status drifted")
        if error_diagnostics[0].get("code") != "unsound":
            fail("cargo-deny registry-shadow failed for an unexpected reason")
    else:
        fail(f"unsupported cargo-deny unsound scope: {unsound_scope!r}")


def verify_report(
    path: pathlib.Path,
    diagnostics_path: pathlib.Path,
    deny_path: pathlib.Path,
    scan_status: int,
) -> None:
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

    settings = report.get("settings")
    if not isinstance(settings, dict) or not isinstance(settings.get("ignore"), list):
        fail("cargo-deny advisory settings shape drifted")
    if not all(isinstance(item, str) for item in settings["ignore"]):
        fail("cargo-deny advisory ignore settings drifted")
    if EXPECTED_ADVISORY in settings["ignore"] or EXPECTED_ALIAS in settings["ignore"]:
        fail("the glib backport advisory must not be suppressed")

    glib_entries = []
    for section, kind, item in report_entries(report):
        package = item.get("package")
        advisory = item.get("advisory")
        if not isinstance(package, dict) or not isinstance(advisory, dict):
            fail("cargo-deny advisory item shape drifted")
        if package.get("name") == PACKAGE_NAME:
            glib_entries.append((section, kind, item, package, advisory))

    if len(glib_entries) != 1:
        found = [entry[4].get("id") for entry in glib_entries]
        fail(f"expected exactly {EXPECTED_ADVISORY} for glib, found {found}")
    section, kind, item, package, advisory = glib_entries[0]
    if (section, kind, item.get("kind")) != ("warnings", "unsound", "unsound"):
        fail("the reviewed glib advisory classification drifted")
    if package.get("version") != PACKAGE_VERSION or package.get("source") != REGISTRY_SOURCE:
        fail("cargo-deny did not scan the canonical glib 0.18.5 registry identity")
    if advisory.get("id") != EXPECTED_ADVISORY or advisory.get("package") != PACKAGE_NAME:
        fail(f"unexpected glib advisory: {advisory.get('id')!r}")
    aliases = advisory.get("aliases")
    if not isinstance(aliases, list) or aliases != [EXPECTED_ALIAS]:
        fail(f"the reviewed glib advisory aliases drifted: {aliases!r}")
    if advisory.get("informational") != "unsound":
        fail("the reviewed glib advisory metadata drifted")
    versions = item.get("versions")
    if versions != {"patched": [">=0.20.0"], "unaffected": ["<0.15.0"]}:
        fail(f"the reviewed glib affected-version contract drifted: {versions!r}")
    verify_status(diagnostics_path, deny_path, scan_status)


def main(argv: list[str]) -> None:
    if len(argv) == 5 and argv[1] == "rewrite-metadata":
        rewrite_metadata(pathlib.Path(argv[2]), pathlib.Path(argv[3]), pathlib.Path(argv[4]))
        return
    if len(argv) == 6 and argv[1] == "verify-report":
        try:
            scan_status = int(argv[5])
        except ValueError:
            fail("cargo-deny scan status must be an integer")
        verify_report(
            pathlib.Path(argv[2]),
            pathlib.Path(argv[3]),
            pathlib.Path(argv[4]),
            scan_status,
        )
        return
    fail(
        "usage: vendored-glib-advisory-shadow.py "
        "rewrite-metadata <input> <output> <vendored-manifest> | "
        "verify-report <report> <diagnostics> <deny-config> <scan-status>"
    )


if __name__ == "__main__":
    main(sys.argv)
