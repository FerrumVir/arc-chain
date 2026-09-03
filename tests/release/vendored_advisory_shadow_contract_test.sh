#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

SHADOW_HELPER="$REPO_ROOT/scripts/ci/vendored-advisory-shadow.py"
CARGO_DENY_RUNNER="$REPO_ROOT/scripts/ci/run-cargo-deny.sh"
RELEASE_TEST_RUNNER="$TEST_DIR/run.sh"
DENY_CONFIG="$REPO_ROOT/deny.toml"

live_root_metadata_shadows_exact_reachable_registry_identities() {
    python3 - "$REPO_ROOT" "$SHADOW_HELPER" <<'PY'
import json
import os
import pathlib
import subprocess
import sys
import tempfile

root, helper = map(pathlib.Path, sys.argv[1:])
registry_source = "registry+https://github.com/rust-lang/crates.io-index"
specs = {
    "wasmer-derive": (
        "6.1.0",
        "c546f3380840cd63fdcc390f04cd19002f2dfa19b4691b77ecbd27642bd93452",
    ),
    "shared-buffer": (
        "0.1.4",
        "f6c99835bad52957e7aa241d3975ed17c1e5f8c92026377d117a606f36b84b16",
    ),
    "wasmer-compiler": (
        "6.1.0",
        "4946475adc0af265af8f10aadf4d4a3c64845bcd3801c655bdd81ce5e3ee869b",
    ),
}

metadata = subprocess.run(
    [
        "cargo",
        "metadata",
        "--manifest-path",
        str(root / "Cargo.toml"),
        "--format-version",
        "1",
        "--locked",
        "--offline",
    ],
    check=True,
    stdout=subprocess.PIPE,
    text=True,
    env={**os.environ, "CARGO_TERM_COLOR": "never"},
).stdout

with tempfile.TemporaryDirectory(prefix="arc-root-advisory-shadow.") as temporary:
    temporary = pathlib.Path(temporary)
    actual_path = temporary / "actual.json"
    shadow_path = temporary / "shadow.json"
    actual_path.write_text(metadata)
    result = subprocess.run(
        [sys.executable, str(helper), "rewrite-metadata", "root", str(actual_path), str(shadow_path)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )
    if result.returncode:
        raise SystemExit(f"live root registry-shadow rewrite failed: {result.stdout}")
    actual = json.loads(metadata)
    shadow = json.loads(shadow_path.read_text())
    shadow_text = shadow_path.read_text()
    for name, (version, checksum) in specs.items():
        originals = [package for package in actual["packages"] if package["name"] == name]
        packages = [package for package in shadow["packages"] if package["name"] == name]
        if len(originals) != 1 or len(packages) != 1:
            raise SystemExit(f"{name} package cardinality drifted")
        if originals[0]["id"] in shadow_text:
            raise SystemExit(f"{name} local identity survived the shadow rewrite")
        expected_id = f"{registry_source}#{name}@{version}"
        package = packages[0]
        if (
            package["id"],
            package["source"],
            package["checksum"],
        ) != (expected_id, registry_source, checksum):
            raise SystemExit(f"{name} canonical shadow identity drifted: {package}")
        if not any(node["id"] == expected_id for node in shadow["resolve"]["nodes"]):
            raise SystemExit(f"{name} canonical identity is absent from the resolved graph")
PY
}

root_metadata_rewrite_rejects_missing_extra_and_unreachable_patches() {
    python3 - "$REPO_ROOT" "$SHADOW_HELPER" <<'PY'
import copy
import json
import os
import pathlib
import subprocess
import sys
import tempfile

root, helper = map(pathlib.Path, sys.argv[1:])
metadata = json.loads(
    subprocess.run(
        ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"],
        cwd=root,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
        env={**os.environ, "CARGO_TERM_COLOR": "never"},
    ).stdout
)
by_name = {
    package["name"]: package
    for package in metadata["packages"]
    if package["name"] in {"wasmer-derive", "shared-buffer", "wasmer-compiler"}
}


def run(candidate, directory):
    actual = directory / "actual.json"
    shadow = directory / "shadow.json"
    actual.write_text(json.dumps(candidate))
    return subprocess.run(
        [sys.executable, str(helper), "rewrite-metadata", "root", str(actual), str(shadow)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )


def require_failure(candidate, label, directory):
    result = run(candidate, directory)
    if result.returncode == 0:
        raise SystemExit(f"{label} did not fail closed")


with tempfile.TemporaryDirectory(prefix="arc-root-advisory-shadow-negative.") as temporary:
    temporary = pathlib.Path(temporary)

    missing = copy.deepcopy(metadata)
    missing["packages"] = [
        package for package in missing["packages"] if package["name"] != "shared-buffer"
    ]
    require_failure(missing, "missing reviewed path package", temporary)

    extra = copy.deepcopy(metadata)
    extra["packages"].append(copy.deepcopy(by_name["wasmer-compiler"]))
    require_failure(extra, "additional reviewed-name package", temporary)

    unreachable = copy.deepcopy(metadata)
    target_id = by_name["wasmer-derive"]["id"]
    for node in unreachable["resolve"]["nodes"]:
        node["deps"] = [dependency for dependency in node["deps"] if dependency["pkg"] != target_id]
        node["dependencies"] = [
            dependency for dependency in node["dependencies"] if dependency != target_id
        ]
    require_failure(unreachable, "unreachable reviewed path package", temporary)

    checksummed = copy.deepcopy(metadata)
    target = next(package for package in checksummed["packages"] if package["name"] == "shared-buffer")
    target["checksum"] = "0" * 64
    require_failure(checksummed, "pre-shadow checksummed path package", temporary)

    collision = copy.deepcopy(metadata)
    collision["metadata"] = {
        "collision": (
            "registry+https://github.com/rust-lang/crates.io-index"
            "#shared-buffer@0.1.4"
        )
    }
    require_failure(collision, "pre-existing canonical registry identity", temporary)
PY
}

root_report_rejects_findings_suppressions_shape_drift_and_unexpected_exit() {
    python3 - "$SHADOW_HELPER" "$DENY_CONFIG" <<'PY'
import copy
import json
import pathlib
import subprocess
import sys
import tempfile
import tomllib

helper, real_deny_path = map(pathlib.Path, sys.argv[1:])
repository_ignored = tomllib.loads(real_deny_path.read_text())["advisories"]["ignore"]
registry_source = "registry+https://github.com/rust-lang/crates.io-index"


def advisory_finding(advisory_id, package_name="wasmer-derive", aliases=None):
    return {
        "advisory": {
            "aliases": aliases or [],
            "categories": [],
            "collection": "crates",
            "cvss": None,
            "date": "2099-01-01",
            "description": "fixture",
            "expect-deleted": False,
            "id": advisory_id,
            "informational": "unsound",
            "keywords": [],
            "license": "CC0-1.0",
            "package": package_name,
            "references": [],
            "related": [],
            "source": None,
            "title": "fixture",
            "url": "https://example.invalid/advisory",
            "withdrawn": None,
        },
        "affected": None,
        "kind": "unsound",
        "package": {
            "checksum": None,
            "dependencies": [],
            "name": package_name,
            "replace": None,
            "source": registry_source,
            "version": "6.1.0",
        },
        "versions": {"patched": [], "unaffected": []},
    }


base_report = {
    "lockfile": {"dependency-count": 3},
    "settings": {
        "ignore": [],
        "informational_warnings": ["notice", "unmaintained", "unsound"],
        "severity": None,
        "target_arch": [],
        "target_os": [],
    },
    "vulnerabilities": [],
    "warnings": {},
}
base_summary = {
    "fields": {"advisories": {"errors": 0, "helps": 0, "notes": 0, "warnings": 0}},
    "type": "summary",
}


def run(report, diagnostics, scan_exit, directory):
    report_path = directory / "report.jsonl"
    diagnostics_path = directory / "diagnostics.jsonl"
    report_path.write_text(json.dumps(report) + "\n")
    diagnostics_path.write_text("\n".join(json.dumps(item) for item in diagnostics) + "\n")
    return subprocess.run(
        [
            sys.executable,
            str(helper),
            "verify-report",
            "root",
            str(report_path),
            str(diagnostics_path),
            str(shadow_deny_path),
            str(scan_exit),
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )


def require_failure(report, diagnostics, scan_exit, label, directory):
    result = run(report, diagnostics, scan_exit, directory)
    if result.returncode == 0:
        raise SystemExit(f"{label} did not fail closed")


with tempfile.TemporaryDirectory(prefix="arc-root-advisory-report.") as temporary:
    temporary = pathlib.Path(temporary)
    shadow_deny_path = temporary / "shadow-deny.toml"
    policy = subprocess.run(
        [sys.executable, str(helper), "write-policy", str(real_deny_path), str(shadow_deny_path)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )
    if policy.returncode:
        raise SystemExit(f"suppression-free policy generation failed: {policy.stdout}")
    shadow_policy = tomllib.loads(shadow_deny_path.read_text())
    if shadow_policy.get("advisories", {}).get("ignore") != []:
        raise SystemExit("registry-shadow policy retained repository suppressions")
    if "RUSTSEC-2024-0436" not in repository_ignored:
        raise SystemExit("suppression visibility fixture is no longer present in repository policy")
    clean = run(base_report, [base_summary], 0, temporary)
    if clean.returncode:
        raise SystemExit(f"clean root report failed: {clean.stdout}")

    finding = copy.deepcopy(base_report)
    finding["warnings"] = {"unsound": [advisory_finding("RUSTSEC-2099-0001")]}
    require_failure(finding, [base_summary], 0, "root package advisory", temporary)

    suppressed = copy.deepcopy(base_report)
    suppressed["warnings"] = {
        "unmaintained": [advisory_finding("RUSTSEC-2024-0436")]
    }
    require_failure(
        suppressed,
        [base_summary],
        0,
        "deny.toml-suppressed root package advisory",
        temporary,
    )

    report_shape = copy.deepcopy(base_report)
    report_shape["unexpected"] = True
    require_failure(report_shape, [base_summary], 0, "report shape drift", temporary)

    diagnostic_shape = copy.deepcopy(base_summary)
    diagnostic_shape["fields"]["unexpected"] = True
    require_failure(
        base_report,
        [diagnostic_shape],
        0,
        "diagnostic shape drift",
        temporary,
    )

    require_failure(base_report, [base_summary], 2, "unexpected scanner exit", temporary)
PY
}

runner_wires_root_and_desktop_shadow_profiles_once() {
    python3 - "$CARGO_DENY_RUNNER" "$RELEASE_TEST_RUNNER" <<'PY'
import pathlib
import sys

deny_runner_path, release_runner_path = map(pathlib.Path, sys.argv[1:])
deny_runner = deny_runner_path.read_text()
release_runner = release_runner_path.read_text()

required_once = (
    'SHADOW_HELPER="$SCRIPT_DIR/vendored-advisory-shadow.py"',
    'SHADOW_PROFILE=root',
    'SHADOW_PROFILE=desktop',
    'python3 "$SHADOW_HELPER" rewrite-metadata',
    'python3 "$SHADOW_HELPER" write-policy',
    'python3 "$SHADOW_HELPER" verify-report',
    '--metadata-path "$SHADOW_METADATA"',
    'check --audit-compatible-output advisories',
)
for value in required_once:
    if deny_runner.count(value) != 1:
        raise SystemExit(f"cargo-deny registry-shadow wiring drifted: {value}")
if "vendored-glib-advisory-shadow.py" in deny_runner:
    raise SystemExit("cargo-deny runner retained the partial glib-only helper")
if deny_runner.count('--offline') != 2:
    raise SystemExit("metadata and registry-shadow scans must reuse the refreshed live database")
if "--allow" in deny_runner or " -A " in deny_runner:
    raise SystemExit("cargo-deny registry-shadow runner broadly allows findings")

entry = '"$TEST_DIR/vendored_advisory_shadow_contract_test.sh"'
if release_runner.count(entry) != 1:
    raise SystemExit("release runner must invoke the vendored advisory contract exactly once")
PY
}

run_test 'live root metadata shadows exactly three provenance-pinned reachable packages' \
    live_root_metadata_shadows_exact_reachable_registry_identities
run_test 'root metadata rewrite rejects missing, extra, unreachable, and collided packages' \
    root_metadata_rewrite_rejects_missing_extra_and_unreachable_patches
run_test 'root advisory report rejects findings, suppressions, shape drift, and bad exits' \
    root_report_rejects_findings_suppressions_shape_drift_and_unexpected_exit
run_test 'cargo-deny runner wires root and desktop advisory shadows exactly once' \
    runner_wires_root_and_desktop_shadow_profiles_once

finish_tests
