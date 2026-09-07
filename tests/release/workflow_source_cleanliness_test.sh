#!/usr/bin/env bash
set -Eeuo pipefail

TEST_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH='' cd -- "$TEST_DIR/../.." && pwd)"
PREFLIGHT="$REPO_ROOT/.github/workflows/release-signing-preflight.yml"
RELEASE="$REPO_ROOT/.github/workflows/release.yml"
ATTRIBUTES="$REPO_ROOT/.gitattributes"

python3 - "$PREFLIGHT" "$RELEASE" "$ATTRIBUTES" <<'PY'
from pathlib import Path
import re
import sys

preflight = Path(sys.argv[1]).read_text(encoding="utf-8")
release = Path(sys.argv[2]).read_text(encoding="utf-8")
attributes = Path(sys.argv[3]).read_text(encoding="utf-8").splitlines()

windows_tauri_manifest_rule = "desktop/src-tauri/Cargo.toml text eol=lf"
if attributes.count(windows_tauri_manifest_rule) != 1:
    raise SystemExit(
        "the Windows Tauri manifest must have exactly one canonical LF checkout rule"
    )


def job(document: str, name: str) -> str:
    match = re.search(
        rf"(?ms)^  {re.escape(name)}:\n(.*?)(?=^  [A-Za-z0-9_-]+:\n|\Z)",
        document,
    )
    if match is None:
        raise SystemExit(f"workflow job is missing: {name}")
    return match.group(0)


def require(block: str, needle: str, label: str) -> None:
    if needle not in block:
        raise SystemExit(f"{label} is missing {needle!r}")


def require_order(block: str, needles: list[str], label: str) -> None:
    positions = []
    for needle in needles:
        require(block, needle, label)
        positions.append(block.index(needle))
    if positions != sorted(positions) or len(set(positions)) != len(positions):
        raise SystemExit(f"{label} source-cleanliness checks are out of order")


def require_clean_checks(block: str, minimum: int, label: str) -> None:
    for marker, description in (
        ("git status --porcelain=v1 --untracked-files=all", "tracked+untracked"),
        ("git ls-files --others --ignored --exclude-standard", "ignored-input"),
        ("git update-index --really-refresh", "forced-index-refresh"),
        ("index_flags=\"$(git ls-files -v)\"", "index-flag"),
        ("git rev-parse HEAD", "exact-HEAD"),
    ):
        count = block.count(marker)
        if count < minimum:
            raise SystemExit(
                f"{label} requires at least {minimum} {description} checks; found {count}"
            )


headless = job(preflight, "headless-runtime")
if len(re.findall(r"^          - platform:", headless, flags=re.MULTILINE)) != 5:
    raise SystemExit("headless source checks must cover all five build targets")
require_clean_checks(headless, 2, "pre-tag headless build")
require_order(
    headless,
    [
        "Refuse pre-build headless source drift",
        "Build the real release node and CLI",
        "Package a hash-bound validator-staging artifact",
    ],
    "pre-tag headless build",
)

desktop_unsigned = job(preflight, "desktop-unsigned")
if len(re.findall(r"^          - platform:", desktop_unsigned, flags=re.MULTILINE)) != 4:
    raise SystemExit("desktop source checks must cover all four build targets")
require_clean_checks(desktop_unsigned, 2, "pre-tag desktop build")
require_order(
    desktop_unsigned,
    [
        "Install exact locked desktop dependencies for the unsigned build",
        "Refuse lifecycle changes before the desktop build",
        "Build the desktop binary without signing keys",
        "Normalize and package the exact no-key bundle handoff",
    ],
    "pre-tag desktop build",
)
if "git diff --exit-code --" in desktop_unsigned:
    raise SystemExit("desktop build retained the incomplete fixed-file diff check")

desktop_signer = job(preflight, "desktop-bundle")
if len(re.findall(r"^          - platform:", desktop_signer, flags=re.MULTILINE)) != 4:
    raise SystemExit("desktop signer source checks must cover all four targets")
require_clean_checks(desktop_signer, 2, "pre-tag desktop signer")
for exclusion in (
    "':(exclude)signer-handoff/**'",
    "':(exclude)unsigned-materialized/**'",
):
    require(desktop_signer, exclusion, "pre-tag desktop signer")
require_order(
    desktop_signer,
    [
        "Rehydrate the exact locked signer after materialization",
        "Freeze the exact locked file-signing surface without executing it",
        "Sign only the verified updater payload",
        "Prove signer output, restore release modes, and verify the signature",
        "Package the exact signed desktop candidate",
    ],
    "pre-tag desktop signer",
)

quality = job(release, "release-quality")
require_clean_checks(quality, 2, "release-quality")
require_order(
    quality,
    [
        "npm ci",
        "Refuse lifecycle changes before release authorization",
        "./scripts/ci_check.sh --full",
        "The release gate changed tracked or untracked source",
    ],
    "release-quality",
)

assembler = job(release, "assemble-release")
require_clean_checks(assembler, 2, "release assembler")
require_order(
    assembler,
    [
        "Revalidate the selected run, attempt, IDs, and digests",
        "Refuse source drift before release materialization",
        "actions/download-artifact@",
        "Assemble assets, updater manifest, and checksums",
        "Stage the exact unsigned manifest handoff",
    ],
    "release assembler",
)
for exclusion in (
    "':(exclude)pretag-downloads/**'",
    "':(exclude)artifacts/**'",
    "':(exclude)cutover-handoff-download/**'",
    "':(exclude)cutover-handoff/**'",
    "':(exclude)release-files/**'",
):
    require(assembler, exclusion, "release assembler")

manifest_signer = job(release, "manifest-sign")
require_clean_checks(manifest_signer, 1, "manifest signer")
require_order(
    manifest_signer,
    [
        "Sign only the frozen SHA256SUMS manifest",
        "Verify manifest signature after private-key removal",
        "git status --porcelain=v1 --untracked-files=all",
        "python3 scripts/release/release-manifest-handoff.py stage",
    ],
    "manifest signer",
)
for exclusion in (
    "':(exclude)unsigned-release-handoff/**'",
    "':(exclude)sealed-release-handoff/**'",
):
    require(manifest_signer, exclusion, "manifest signer")

print("workflow source cleanliness contract: ok")
PY
