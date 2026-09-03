#!/usr/bin/env bash
set -uo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
# shellcheck source=/dev/null
. "$TEST_DIR/helpers/testlib.sh"

VENDORED_GLIB="$REPO_ROOT/vendor/third_party/glib-0.18.5"
DESKTOP_MANIFEST="$REPO_ROOT/desktop/src-tauri/Cargo.toml"
DESKTOP_LOCK="$REPO_ROOT/desktop/src-tauri/Cargo.lock"
DENY_CONFIG="$REPO_ROOT/deny.toml"
RELEASE_TEST_RUNNER="$TEST_DIR/run.sh"
CARGO_DENY_RUNNER="$REPO_ROOT/scripts/ci/run-cargo-deny.sh"
ADVISORY_SHADOW_HELPER="$REPO_ROOT/scripts/ci/vendored-glib-advisory-shadow.py"
GIT_ATTRIBUTES="$REPO_ROOT/.gitattributes"

vendored_source_and_backport_are_exact() {
    python3 - "$VENDORED_GLIB" "$GIT_ATTRIBUTES" <<'PY'
import hashlib
import json
import pathlib
import sys
import tomllib

root, attributes_path = map(pathlib.Path, sys.argv[1:])
if root.is_symlink() or not root.is_dir():
    raise SystemExit("vendored glib root is missing, linked, or not a directory")

all_paths = list(root.rglob("*"))
links = [path.relative_to(root).as_posix() for path in all_paths if path.is_symlink()]
if links:
    raise SystemExit(f"vendored glib contains links: {links}")

arc_files = sorted(
    path.relative_to(root).as_posix()
    for path in all_paths
    if path.is_file() and path.name.startswith("ARC-")
)
if arc_files != ["ARC-PROVENANCE.md"]:
    raise SystemExit(f"unexpected ARC metadata in vendored glib: {arc_files}")

attributes = attributes_path.read_text().splitlines()
lf_rule = "vendor/third_party/glib-0.18.5/** text eol=lf"
if attributes.count(lf_rule) != 1:
    raise SystemExit("vendored glib bytes are not pinned to LF in .gitattributes")
license_rule = "vendor/third_party/glib-0.18.5/LICENSE whitespace=-blank-at-eof"
if attributes.count(license_rule) != 1:
    raise SystemExit("canonical glib license whitespace policy drifted")

upstream_files = sorted(
    (path.relative_to(root).as_posix(), path)
    for path in all_paths
    if path.is_file() and not path.name.startswith("ARC-")
)
if len(upstream_files) != 121:
    raise SystemExit(f"vendored glib upstream-file count drifted: {len(upstream_files)}")

variant_path = root / "src/variant_iter.rs"
patched_variant = variant_path.read_bytes()
old_declaration = b"            let p: *mut libc::c_char = std::ptr::null_mut();\n"
new_declaration = b"            let mut p: *mut libc::c_char = std::ptr::null_mut();\n"
old_argument = b"                &p,\n"
new_argument = b"                &mut p,\n"

for old, new, label in (
    (old_declaration, new_declaration, "out-pointer declaration"),
    (old_argument, new_argument, "out-pointer argument"),
):
    if old in patched_variant:
        raise SystemExit(f"vulnerable {label} remains in VariantStrIter")
    if patched_variant.count(new) != 1:
        raise SystemExit(f"fixed {label} must occur exactly once")

canonical_variant = patched_variant.replace(new_declaration, old_declaration, 1)
canonical_variant = canonical_variant.replace(new_argument, old_argument, 1)


def file_sha(data):
    return hashlib.sha256(data).hexdigest()


def tree_sha(variant_bytes):
    rows = []
    for relative, path in upstream_files:
        data = variant_bytes if path == variant_path else path.read_bytes()
        rows.append(f"{file_sha(data)}  {relative}\n".encode())
    return file_sha(b"".join(rows))


expected = {
    "canonical_variant": "1fd02859333761c45321b32f28b24233446b97d0022a90d3a937ed162585b90e",
    "patched_variant": "a0f5ee8acb8faa089bcdfbc9a57372609fce7654026ccef7d9a224d05a654ccc",
    "canonical_tree": "c977877cf8a028d8e42fc2ce60cd85ae193c8959147d5560ed1958b9bfba6875",
    "patched_tree": "0a72c413b5a125e0312a2bd9740b852388f4e2ac784031dc78c683a78202b8b4",
}
actual = {
    "canonical_variant": file_sha(canonical_variant),
    "patched_variant": file_sha(patched_variant),
    "canonical_tree": tree_sha(canonical_variant),
    "patched_tree": tree_sha(patched_variant),
}
if actual != expected:
    raise SystemExit(f"vendored glib source identity drifted: {actual}")

manifest = tomllib.loads((root / "Cargo.toml").read_text())
package = manifest.get("package", {})
if (package.get("name"), package.get("version"), package.get("license")) != (
    "glib",
    "0.18.5",
    "MIT",
):
    raise SystemExit("vendored glib package identity drifted")

vcs = json.loads((root / ".cargo_vcs_info.json").read_text())
if vcs != {
    "git": {"sha1": "42b9caf98e03ded086362d9653ca58fe94dc8658"},
    "path_in_vcs": "glib",
}:
    raise SystemExit(f"vendored glib VCS provenance drifted: {vcs}")

provenance_bytes = (root / "ARC-PROVENANCE.md").read_bytes()
expected_provenance_sha = "dd88a25d1e8bb545a37e7b08418e52cf2aa1319d604a60ebd49d2981d825abd1"
if file_sha(provenance_bytes) != expected_provenance_sha:
    raise SystemExit("ARC-PROVENANCE.md content drifted")
provenance = provenance_bytes.decode()
required_provenance = (
    "233daaf6e83ae6a12a52055f568f9d7cf4671dabb78ff9560ab6da230ce00ee5",
    "c977877cf8a028d8e42fc2ce60cd85ae193c8959147d5560ed1958b9bfba6875",
    "0a72c413b5a125e0312a2bd9740b852388f4e2ac784031dc78c683a78202b8b4",
    "https://rustsec.org/advisories/RUSTSEC-2024-0429.html",
    "https://github.com/advisories/GHSA-wrw7-89jp-8q8g",
    "https://github.com/gtk-rs/gtk-rs-core/pull/1343",
    "https://github.com/gtk-rs/gtk-rs-core/commit/05dff0ee696f9bcd8617cd48c4b812d046d440cb",
)
for value in required_provenance:
    if provenance.count(value) != 1:
        raise SystemExit(f"provenance must contain exact value once: {value}")
PY
}

desktop_resolves_only_the_verified_local_backport() {
    python3 - \
        "$REPO_ROOT" \
        "$VENDORED_GLIB" \
        "$DESKTOP_MANIFEST" \
        "$DESKTOP_LOCK" <<'PY'
import pathlib
import sys
import tomllib

repo_root, vendored_glib, manifest_path, lock_path = map(pathlib.Path, sys.argv[1:])
manifest = tomllib.loads(manifest_path.read_text())
patch = manifest.get("patch", {}).get("crates-io", {}).get("glib")
expected_patch = {"path": "../../vendor/third_party/glib-0.18.5"}
if patch != expected_patch:
    raise SystemExit(f"desktop glib patch is not exact: {patch!r}")

resolved_patch = (manifest_path.parent / patch["path"]).resolve()
if resolved_patch != vendored_glib.resolve():
    raise SystemExit(f"desktop glib patch resolves outside the reviewed tree: {resolved_patch}")
try:
    resolved_patch.relative_to(repo_root.resolve())
except ValueError as error:
    raise SystemExit("desktop glib patch escapes the repository") from error

lock = tomllib.loads(lock_path.read_text())
glib_packages = [package for package in lock.get("package", []) if package.get("name") == "glib"]
if len(glib_packages) != 1:
    raise SystemExit(f"desktop lock must contain exactly one glib package: {glib_packages}")
glib = glib_packages[0]
if glib.get("version") != "0.18.5":
    raise SystemExit(f"desktop lock resolved unexpected glib version: {glib.get('version')}")
for forbidden in ("source", "checksum"):
    if forbidden in glib:
        raise SystemExit(f"desktop lock still resolves registry glib ({forbidden} is present)")
PY
}

advisory_policy_does_not_suppress_the_backport() {
    python3 - \
        "$DENY_CONFIG" \
        "$RELEASE_TEST_RUNNER" \
        "$CARGO_DENY_RUNNER" <<'PY'
import pathlib
import sys
import tomllib

deny_path, release_runner_path, deny_runner_path = map(pathlib.Path, sys.argv[1:])
deny = tomllib.loads(deny_path.read_text())
advisories = deny.get("advisories", {})
ignored_ids = []
for entry in advisories.get("ignore", []):
    ignored_ids.append(entry if isinstance(entry, str) else entry.get("id"))
if "RUSTSEC-2024-0429" in ignored_ids or "GHSA-wrw7-89jp-8q8g" in ignored_ids:
    raise SystemExit("the verified glib backport must not rely on an advisory suppression")

runner = release_runner_path.read_text()
needle = '"$TEST_DIR/glib_backport_contract_test.sh"'
if runner.count(needle) != 1:
    raise SystemExit("the glib backport contract is not run exactly once by release tests")

deny_runner = deny_runner_path.read_text()
required_shadow_steps = (
    'SHADOW_HELPER="$SCRIPT_DIR/vendored-glib-advisory-shadow.py"',
    'python3 "$SHADOW_HELPER" rewrite-metadata',
    '--metadata-path "$SHADOW_METADATA"',
    'check --audit-compatible-output advisories',
    'python3 "$SHADOW_HELPER" verify-report',
)
for step in required_shadow_steps:
    if deny_runner.count(step) != 1:
        raise SystemExit(f"cargo-deny glib registry-shadow step drifted: {step}")
if "--allow" in deny_runner or " -A " in deny_runner:
    raise SystemExit("cargo-deny runner must not broadly allow advisory findings")
PY
}

advisory_shadow_is_explicit_and_fail_closed() {
    python3 - "$ADVISORY_SHADOW_HELPER" "$VENDORED_GLIB/Cargo.toml" <<'PY'
import copy
import json
import pathlib
import subprocess
import sys
import tempfile

helper, manifest = map(pathlib.Path, sys.argv[1:])
python = sys.executable
old_id = "path+file:///reviewed/glib-0.18.5#glib@0.18.5"
registry_source = "registry+https://github.com/rust-lang/crates.io-index"
registry_id = f"{registry_source}#glib@0.18.5"
archive_sha = "233daaf6e83ae6a12a52055f568f9d7cf4671dabb78ff9560ab6da230ce00ee5"


def run(*args):
    return subprocess.run(
        [python, str(helper), *map(str, args)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )


def require_success(result, label):
    if result.returncode != 0:
        raise SystemExit(f"{label} failed: {result.stdout}")


def require_failure(result, label):
    if result.returncode == 0:
        raise SystemExit(f"{label} did not fail closed")


with tempfile.TemporaryDirectory(prefix="arc-glib-advisory-shadow.") as temporary:
    temporary = pathlib.Path(temporary)
    actual_path = temporary / "actual.json"
    shadow_path = temporary / "shadow.json"
    report_path = temporary / "report.jsonl"
    diagnostics_path = temporary / "diagnostics.jsonl"
    deny_path = temporary / "deny.toml"
    transitive_deny_path = temporary / "transitive-deny.toml"
    metadata = {
        "packages": [
            {
                "name": "glib",
                "version": "0.18.5",
                "id": old_id,
                "source": None,
                "checksum": None,
                "manifest_path": str(manifest),
            }
        ],
        "resolve": {"root": old_id, "nodes": [{"id": old_id, "dependencies": []}]},
    }
    actual_path.write_text(json.dumps(metadata))
    require_success(
        run("rewrite-metadata", actual_path, shadow_path, manifest),
        "canonical registry-shadow rewrite",
    )
    shadow = json.loads(shadow_path.read_text())
    package = shadow["packages"][0]
    if package != {
        "name": "glib",
        "version": "0.18.5",
        "id": registry_id,
        "source": registry_source,
        "checksum": archive_sha,
        "manifest_path": str(manifest),
    }:
        raise SystemExit(f"registry-shadow package identity drifted: {package}")
    if old_id in shadow_path.read_text():
        raise SystemExit("registry-shadow metadata retained the local package ID")

    already_registry = copy.deepcopy(metadata)
    already_registry["packages"][0]["source"] = registry_source
    actual_path.write_text(json.dumps(already_registry))
    require_failure(
        run("rewrite-metadata", actual_path, shadow_path, manifest),
        "non-path glib metadata",
    )

    finding = {
        "advisory": {
            "id": "RUSTSEC-2024-0429",
            "package": "glib",
            "aliases": ["GHSA-wrw7-89jp-8q8g"],
            "informational": "unsound",
        },
        "kind": "unsound",
        "package": {
            "name": "glib",
            "version": "0.18.5",
            "source": registry_source,
        },
        "versions": {"patched": [">=0.20.0"], "unaffected": ["<0.15.0"]},
    }
    report = {
        "lockfile": {"dependency-count": 1},
        "settings": {"ignore": []},
        "vulnerabilities": [],
        "warnings": {"unsound": [finding]},
    }
    summary = {
        "fields": {
            "advisories": {"errors": 0, "helps": 0, "notes": 0, "warnings": 1}
        },
        "type": "summary",
    }
    deny_path.write_text('[advisories]\nversion = 2\nignore = []\n')
    diagnostics_path.write_text(json.dumps(summary) + "\n")
    report_path.write_text(json.dumps(report) + "\n")
    require_success(
        run("verify-report", report_path, diagnostics_path, deny_path, 0),
        "exact advisory set",
    )

    missing = copy.deepcopy(report)
    missing["warnings"]["unsound"] = []
    report_path.write_text(json.dumps(missing) + "\n")
    require_failure(
        run("verify-report", report_path, diagnostics_path, deny_path, 0),
        "missing reviewed advisory",
    )

    additional = copy.deepcopy(report)
    extra = copy.deepcopy(finding)
    extra["advisory"]["id"] = "RUSTSEC-2099-0001"
    additional["warnings"]["unsound"].append(extra)
    report_path.write_text(json.dumps(additional) + "\n")
    require_failure(
        run("verify-report", report_path, diagnostics_path, deny_path, 0),
        "additional glib advisory",
    )

    suppressed = copy.deepcopy(report)
    suppressed["settings"]["ignore"] = ["RUSTSEC-2024-0429"]
    report_path.write_text(json.dumps(suppressed) + "\n")
    require_failure(
        run("verify-report", report_path, diagnostics_path, deny_path, 0),
        "suppressed reviewed advisory",
    )

    shape_drift = copy.deepcopy(report)
    shape_drift["unexpected"] = True
    report_path.write_text(json.dumps(shape_drift) + "\n")
    require_failure(
        run("verify-report", report_path, diagnostics_path, deny_path, 0),
        "cargo-deny output shape drift",
    )

    report_path.write_text(json.dumps(report) + "\n")
    require_failure(
        run("verify-report", report_path, diagnostics_path, deny_path, 1),
        "unexplained nonzero cargo-deny status",
    )

    transitive_deny_path.write_text(
        '[advisories]\nversion = 2\nunsound = "all"\nignore = []\n'
    )
    unsound_error = {
        "fields": {"code": "unsound", "severity": "error"},
        "type": "diagnostic",
    }
    transitive_summary = copy.deepcopy(summary)
    transitive_summary["fields"]["advisories"]["errors"] = 1
    diagnostics_path.write_text(
        json.dumps(unsound_error) + "\n" + json.dumps(transitive_summary) + "\n"
    )
    require_success(
        run(
            "verify-report",
            report_path,
            diagnostics_path,
            transitive_deny_path,
            1,
        ),
        "transitive-unsound policy advisory exit",
    )
    require_failure(
        run(
            "verify-report",
            report_path,
            diagnostics_path,
            transitive_deny_path,
            0,
        ),
        "missing transitive-unsound policy failure",
    )
PY
}

run_test 'vendored glib is the exact canonical crate plus the upstream two-line fix' \
    vendored_source_and_backport_are_exact
run_test 'desktop lock resolves only the verified local glib backport' \
    desktop_resolves_only_the_verified_local_backport
run_test 'the verified glib backport does not suppress its advisory' \
    advisory_policy_does_not_suppress_the_backport
run_test 'vendored glib advisory shadow is explicit and fail closed' \
    advisory_shadow_is_explicit_and_fail_closed

finish_tests
