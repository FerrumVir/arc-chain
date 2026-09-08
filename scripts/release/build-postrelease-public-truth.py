#!/usr/bin/env python3
"""Derive the public ARC status files from sealed post-release evidence.

The release source intentionally says that v0.8.0 is unpublished.  That text
must remain immutable in the tag.  After the release, fleet cutover, Pages
deployment, installer canaries, and live reward verification have all passed,
this helper creates three create-only evidence products:

* a README with only its delimited status/quickstart block replaced; and
* a machine-readable public production-status document; and
* the canonical v2 acceptance receipt from which those claims were derived.

It never mutates a checkout.  It validates the exact raw GitHub and CDN files
supplied by the operator, then performs one final read-only live verification
with the exact checked-in recovery verifier and proves that the same canary
receipts are visible through the checked-in dashboard, explorer, and desktop
readers. The desktop proof is a create-only receipt emitted only after the
exact fail-closed Playwright live suite succeeds.
The output directory is create-only.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import subprocess
import stat
import sys
import tempfile
import zipfile
from decimal import Decimal
from pathlib import Path
from typing import Any, Mapping, NoReturn, Sequence


STATUS_SCHEMA = "arc.public-production-status.v1"
ACCEPTANCE_SCHEMA = "arc.post-release-acceptance.v2"
NETWORK_SCHEMA = "arc.frontend.network.v1"
REWARD_SCHEMA = "arc.recovery.reward-evidence.v3"
REPOSITORY = "FerrumVir/arc-chain"
TAG = "v0.8.0"
VERSION = "0.8.0"
CHAIN_ID = "0x415243"
PROTOCOL_VERSION_RE = re.compile(r"^3\.[0-9]+\.[0-9]+$")
RECOVERY_EPOCH = 1
VALIDATOR_SET_ID = 1
LEGACY_CONTINUITY_SAFETY_MARGIN = 128
# The exact projection deliberately nests the complete archive verifier (up to
# 24h), bounded per-object/per-fork provenance reads, and fresh all-six live,
# convergence, and reward checks. Keep the outer watchdog beyond the sum of
# every valid inner watchdog so it never kills a slow operation that remains
# inside the reviewed contract.
RECOVERY_FRONTEND_PROJECTION_TIMEOUT_SECONDS = 72 * 60 * 60
REWARD_PER_RECEIPT_BASE = 2_500_000_000
PUBLIC_CONSOLE = "https://ferrumvir.github.io/arc-chain/"
PUBLIC_EXPLORER = PUBLIC_CONSOLE + "explorer/"
BEGIN_MARKER = "<!-- ARC_PUBLIC_TRUTH_BEGIN -->"
END_MARKER = "<!-- ARC_PUBLIC_TRUTH_END -->"
MAX_INPUT_BYTES = 16 * 1024 * 1024
HASH_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
CHAIN_HASH_RE = re.compile(r"^(?:0x)?[0-9a-f]{64}$")
UTC_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")
PAGES_WORKFLOW_PATH = ".github/workflows/deploy-explorer.yml"
PUBLISHED_WORKFLOW_PATH = ".github/workflows/post-release-acceptance.yml"
PAGES_JOB_NAMES = frozenset(
    {"Verify and assemble public console", "Publish GitHub Pages"}
)
PUBLISHED_JOB_NAMES = frozenset(
    {
        "Bind exact release run, tag, and public assets",
        "Linux headless, AppImage, and real v0.7.7 migration",
        "Packaged desktop (macos-arm64)",
        "Packaged desktop (macos-x86_64)",
        "Packaged desktop (windows-x86_64)",
        "Seal canonical published-artifact receipt",
    }
)
PUBLISHED_ACCEPTANCE_RECEIPT = "POST-RELEASE-ARTIFACT-ACCEPTANCE.json"
PUBLISHED_ACCEPTANCE_SUMS = "POST-RELEASE-ARTIFACT-ACCEPTANCE.SHA256SUMS"
PUBLISHED_EVIDENCE_MANIFEST = "EVIDENCE-MANIFEST.json"
PUBLISHED_TOP_LEVEL_JSON = frozenset(
    {
        PUBLISHED_ACCEPTANCE_RECEIPT,
        PUBLISHED_EVIDENCE_MANIFEST,
        "component-artifacts.json",
        "linux-x86_64.json",
        "macos-arm64.json",
        "macos-x86_64.json",
        "release-binding.json",
        "windows-x86_64.json",
    }
)
PUBLISHED_EVIDENCE_FILES = frozenset(
    {
        "linux-x86_64/app.stderr",
        "linux-x86_64/app.stdout",
        "linux-x86_64/extract.stderr",
        "linux-x86_64/extract.stdout",
        "linux-x86_64/headless-install.stderr",
        "linux-x86_64/headless-install.stdout",
        "linux-x86_64/headless-update.stderr",
        "linux-x86_64/headless-update.stdout",
        "linux-x86_64/legacy-data-after.json",
        "linux-x86_64/legacy-data-before.json",
        "linux-x86_64/legacy-home.txt",
        "linux-x86_64/legacy-migration.stderr",
        "linux-x86_64/legacy-migration.stdout",
        "linux-x86_64/legacy-model-before.sha256",
        "linux-x86_64/legacy-source.json",
        "linux-x86_64/legacy-update.stderr",
        "linux-x86_64/legacy-update.stdout",
        "linux-x86_64/legacy-version.txt",
        "linux-x86_64/window-geometry.txt",
        "linux-x86_64/window-pid.txt",
        "linux-x86_64/window-properties.txt",
        "linux-x86_64/window.xwd",
        "macos-arm64/desktop-window.json",
        "macos-arm64/desktop.stderr",
        "macos-arm64/desktop.stdout",
        "macos-x86_64/desktop-window.json",
        "macos-x86_64/desktop.stderr",
        "macos-x86_64/desktop.stdout",
        "release/published-evidence-artifact.json",
        "release/published-evidence.zip",
        "release/release-attempt-jobs.json",
        "release/release-published.json",
        "windows-x86_64/windows-desktop-window.json",
        "windows-x86_64/windows-desktop.stderr",
        "windows-x86_64/windows-desktop.stdout",
        "windows-x86_64/windows-msi-admin.log",
    }
)
EXPECTED_PUBLISHED_ZIP_FILES = frozenset(
    set(PUBLISHED_TOP_LEVEL_JSON)
    | {PUBLISHED_ACCEPTANCE_SUMS}
    | {f"evidence/{name}" for name in PUBLISHED_EVIDENCE_FILES}
)
MAX_PUBLISHED_ZIP_BYTES = 768 * 1024 * 1024
MAX_PUBLISHED_MEMBER_BYTES = 256 * 1024 * 1024
MAX_PUBLISHED_EXPANDED_BYTES = 512 * 1024 * 1024
RECOVERY_VERIFIER_RELATIVE = Path("scripts/recovery/recovery_rollout.py")
PRODUCT_SURFACE_VERIFIER_RELATIVE = Path(
    "scripts/release/verify-postcutover-product-surfaces.mjs"
)
PRODUCT_READER_RELATIVES = (
    Path("dashboard/app.js"),
    Path("explorer/app.js"),
    Path("shared/frontend/arc-network.js"),
)
DESKTOP_LIVE_GENERATOR_RELATIVE = Path(
    "scripts/release/build-desktop-live-product-receipt.mjs"
)
DESKTOP_LIVE_SCHEMA = "arc.desktop-live-product-gate.v2"
DESKTOP_LIVE_SUITE_RELATIVES = (
    Path("desktop/index.html"),
    Path("desktop/package.json"),
    Path("desktop/playwright.config.ts"),
    Path("desktop/playwright.live.config.ts"),
    Path("desktop/tests/helpers.ts"),
    Path("desktop/tests/live.spec.ts"),
    Path("desktop/vite.config.ts"),
)
DESKTOP_PACKAGE_LOCK_RELATIVE = Path("desktop/package-lock.json")
PACKAGED_APPIMAGE_VERIFIER_RELATIVE = Path(
    "scripts/release/packaged-appimage-live-gate.py"
)
PACKAGED_APPIMAGE_HOST_SCHEMA = "arc.packaged-appimage-live-host.v1"
PACKAGED_APPIMAGE_SCOPE = (
    "linux-x86_64-packaged-ui-to-tauri-ipc-under-external-webkit-automation"
)
PACKAGED_NATIVE_SCOPE = "macos-arm64-packaged-native-core"
MACOS_PACKAGE_CONTROLLER_RELATIVE = Path(
    "scripts/recovery/build-macos-package-provenance.py"
)
MACOS_PACKAGE_INSPECTION_SCHEMA = "arc.macos-package-inspection.v1"
MACOS_PACKAGE_PROVENANCE_SCHEMA = "arc.macos-packaged-native-provenance.v1"
MACOS_PACKAGE_PROVENANCE_VERIFICATION_SCHEMA = (
    "arc.macos-package-provenance-verification.v1"
)
MACOS_UPDATER_SIGNATURE_SCHEMA = "arc.macos-updater-signature-verification.v1"
MACOS_PACKAGE_TRUTH_SCOPE = {
    "appleDeveloperIdSigned": False,
    "exactMountedDmgExecutableRan": True,
    "gatekeeperAssessed": False,
    "nativeCoreOnly": True,
    "notarizationAssessed": False,
    "shippedDebugOrWebdriverSurfaceAdded": False,
    "uiToIpcCoveredByThisReceipt": False,
    "updaterArchiveMinisignVerified": True,
}
MACOS_EVIDENCE_FILES = {
    "controllerAttempt": "MACOS-NATIVE-CONTROLLER-ATTEMPT.json",
    "inspection": "MACOS-PACKAGE-INSPECTION.json",
    "nativeAttempt": "PACKAGED-NATIVE-DISPATCH-ATTEMPT.json",
    "nativeInput": "DESKTOP-LIVE-INPUT.json",
    "nativeReceipt": "PACKAGED-NATIVE-ACCEPTANCE.json",
    "provenance": "MACOS-PACKAGE-PROVENANCE.json",
    "signature": "MACOS-UPDATER-SIGNATURE.json",
    "verification": "MACOS-PACKAGE-PROVENANCE-VERIFICATION.json",
}
_BLAKE3_U32_MASK = (1 << 32) - 1
_BLAKE3_IV = (
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A,
    0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
)
_BLAKE3_PERMUTATION = (2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8)
DESKTOP_NODE_ARCHIVE_SHA256 = "b7bf7707070b950ba1ec5f1af3bb6de0f2b1962c5033973d94068ab021ef3014"
DESKTOP_NODE_EXECUTABLE_SHA256 = "9d050fd455b56426e25d4d603c7c501cbb2630348e836cf221dcce748e90588a"
DESKTOP_NPM_CLI_SHA256 = "8e5f6f3429f8cdbe693cdc29904e9d5a7b127a494bd15c804bd54c7403bfcbe7"
DESKTOP_NPM_PACKAGE_SHA256 = "09dfcf187178ce1ab3ea6194c80d3ae082ad2a86dc1269ac963f94429e718122"
DESKTOP_SSH_EXECUTABLE_SHA256 = "75ae4b414b57e0c52ad1cb24a9d7dae2496071fdf153c7fc8e94db3c9c4b0faa"
DESKTOP_SSH_KNOWN_HOSTS_SHA256 = "97c826f7e1a3940f6d18095ccdb0eaeebb5d66ec16fe60b9c5c47690e707485d"
DESKTOP_SSH_IDENTITY_SHA256 = "9a7b57700dc7acf0faeca152fc341f237704e81965b5a9656fe8ccee4931444a"
DESKTOP_FORWARD_KIND = "host-key-pinned-ssh-local-tcp-to-validator-unix-v1"
DESKTOP_VALIDATOR_NAME = "lax"
DESKTOP_VALIDATOR_HOST = "140.82.16.112"
DESKTOP_DIRECT_RECEIPT_KEYS = frozenset(
    {
        "assignment_epoch", "block_hash", "block_height", "confirmed",
        "evidence_source", "included", "index", "input_hash", "job_id",
        "model_id", "output_hash", "receipt_url", "recovery_epoch", "reward_arc",
        "reward_base", "status", "submitted", "success", "transaction_domain",
        "tx_hash", "tx_type", "validator_approvals", "validator_set_commitment",
        "validator_set_id", "worker",
    }
)
DESKTOP_EARNINGS_RECEIPT_KEYS = frozenset(
    {
        "assignment_epoch", "block_hash", "block_height", "confirmed", "included",
        "index", "input_hash", "job_id", "model_id", "output_hash", "receipt_url",
        "recovery_epoch", "reward_arc", "reward_base", "submitted", "success",
        "transaction_domain", "tx_hash", "tx_type", "validator_set_id", "worker",
    }
)
DESKTOP_HISTORY_DOMAIN = (
    "all canonical 0x25 reward domains since the v3 recovery boundary; historical "
    "rows retain their own recovery_epoch, validator_set_id, and transaction_domain"
)
DESKTOP_RETAINED_SOURCE = "scan of this node's in-memory full_transactions map"
DESKTOP_RETAINED_SCOPE = "this node's bounded retained reward-receipt window"
DESKTOP_ARCHIVE_SCOPE = "complete canonical reward history since the v3 recovery boundary"
PUBLISHED_HELPER_RELATIVE = Path("scripts/release/published-artifact-acceptance.py")

PRODUCTION_FLEET = (
    ("nyc", "149.28.32.76"),
    ("lax", "140.82.16.112"),
    ("ams", "136.244.109.1"),
    ("lhr", "104.238.171.11"),
    ("nrt", "202.182.107.41"),
    ("sgp", "149.28.153.31"),
)

EXPECTED_RELEASE_ASSETS = {
    "arc-node-linux-x86_64",
    "arc-cli-linux-x86_64",
    "arc-node-linux-arm64",
    "arc-cli-linux-arm64",
    "arc-node-macos-arm64",
    "arc-cli-macos-arm64",
    "arc-node-macos-x86_64",
    "arc-cli-macos-x86_64",
    "arc-node-windows-x86_64.exe",
    "arc-cli-windows-x86_64.exe",
    "arc-desktop-macos-arm64.app.tar.gz",
    "arc-desktop-macos-arm64.app.tar.gz.sig",
    "arc-desktop-macos-arm64.dmg",
    "arc-desktop-macos-x86_64.app.tar.gz",
    "arc-desktop-macos-x86_64.app.tar.gz.sig",
    "arc-desktop-macos-x86_64.dmg",
    "arc-desktop-windows-x86_64-setup.exe",
    "arc-desktop-windows-x86_64-setup.exe.sig",
    "arc-desktop-windows-x86_64.msi",
    "arc-desktop-linux-x86_64.AppImage",
    "arc-desktop-linux-x86_64.AppImage.sig",
    "arc-desktop-linux-x86_64.deb",
    "arc-desktop-linux-x86_64.rpm",
    "install.sh",
    "testnet-seeds.txt",
    "genesis.toml",
    "arc-legacy-maintenance-boundary.json",
    "arc-recovery-checkpoint-descriptor.json",
    "arc-cutover-policy.json",
    "latest.json",
    "SHA256SUMS",
    "SHA256SUMS.sig",
}


class TruthError(ValueError):
    """Evidence cannot support a live public status claim."""


def fail(message: str) -> NoReturn:
    raise TruthError(message)


def canonical_json(value: object) -> bytes:
    return (
        json.dumps(
            value,
            allow_nan=False,
            ensure_ascii=True,
            separators=(",", ":"),
            sort_keys=True,
        )
        + "\n"
    ).encode("utf-8")


def sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def _blake3_rotate(value: int, count: int) -> int:
    return ((value >> count) | (value << (32 - count))) & _BLAKE3_U32_MASK


def _blake3_mix(
    state: list[int], a: int, b: int, c: int, d: int, first: int, second: int
) -> None:
    state[a] = (state[a] + state[b] + first) & _BLAKE3_U32_MASK
    state[d] = _blake3_rotate(state[d] ^ state[a], 16)
    state[c] = (state[c] + state[d]) & _BLAKE3_U32_MASK
    state[b] = _blake3_rotate(state[b] ^ state[c], 12)
    state[a] = (state[a] + state[b] + second) & _BLAKE3_U32_MASK
    state[d] = _blake3_rotate(state[d] ^ state[a], 8)
    state[c] = (state[c] + state[d]) & _BLAKE3_U32_MASK
    state[b] = _blake3_rotate(state[b] ^ state[c], 7)


def _blake3_compress_short(
    chaining: Sequence[int], words: Sequence[int], block_length: int, flags: int
) -> tuple[int, ...]:
    state = list(chaining) + list(_BLAKE3_IV[:4]) + [0, 0, block_length, flags]
    message = list(words)
    for _ in range(7):
        _blake3_mix(state, 0, 4, 8, 12, message[0], message[1])
        _blake3_mix(state, 1, 5, 9, 13, message[2], message[3])
        _blake3_mix(state, 2, 6, 10, 14, message[4], message[5])
        _blake3_mix(state, 3, 7, 11, 15, message[6], message[7])
        _blake3_mix(state, 0, 5, 10, 15, message[8], message[9])
        _blake3_mix(state, 1, 6, 11, 12, message[10], message[11])
        _blake3_mix(state, 2, 7, 8, 13, message[12], message[13])
        _blake3_mix(state, 3, 4, 9, 14, message[14], message[15])
        message = [message[index] for index in _BLAKE3_PERMUTATION]
    return tuple(
        [state[index] ^ state[index + 8] for index in range(8)]
        + [state[index + 8] ^ chaining[index] for index in range(8)]
    )


def blake3_short(raw: bytes) -> str:
    """Independently hash the bounded packaged-native challenge prompt."""

    if len(raw) > 1024:
        fail("packaged native challenge prompt exceeds one reviewed BLAKE3 chunk")
    chaining: Sequence[int] = _BLAKE3_IV
    block_count = max(1, (len(raw) + 63) // 64)
    for block_index in range(block_count):
        block = raw[block_index * 64 : (block_index + 1) * 64]
        padded = block + bytes(64 - len(block))
        words = tuple(
            int.from_bytes(padded[offset : offset + 4], "little")
            for offset in range(0, 64, 4)
        )
        final = block_index + 1 == block_count
        compressed = _blake3_compress_short(
            chaining,
            words,
            len(block),
            (1 if block_index == 0 else 0) | (10 if final else 0),
        )
        if final:
            return "".join(
                word.to_bytes(4, "little").hex() for word in compressed[:8]
            )
        chaining = compressed[:8]
    raise AssertionError("unreachable BLAKE3 state")


if blake3_short(b"") != "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262":
    raise RuntimeError("public-truth BLAKE3 known-answer self-test failed")


def load_bytes(
    path: Path,
    label: str,
    maximum: int = MAX_INPUT_BYTES,
    *,
    allow_empty: bool = False,
) -> bytes:
    descriptor = -1
    try:
        flags = os.O_RDONLY
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        descriptor = os.open(path, flags)
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode):
            fail(f"{label} must be a non-symlink regular file")
        if before.st_size > maximum or (before.st_size == 0 and not allow_empty):
            fail(f"{label} has an unsupported size")
        chunks: list[bytes] = []
        remaining = maximum + 1
        while remaining > 0:
            chunk = os.read(descriptor, min(1024 * 1024, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        raw = b"".join(chunks)
        after = os.fstat(descriptor)
    except OSError as error:
        fail(f"cannot read {label}: {error}")
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    stable_identity = (
        before.st_dev,
        before.st_ino,
        before.st_size,
        before.st_mtime_ns,
        before.st_ctime_ns,
    ) == (
        after.st_dev,
        after.st_ino,
        after.st_size,
        after.st_mtime_ns,
        after.st_ctime_ns,
    )
    if len(raw) != before.st_size or not stable_identity:
        fail(f"{label} changed while it was read")
    return raw


def reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            fail(f"JSON contains duplicate key {key!r}")
        result[key] = value
    return result


def reject_nonfinite_number(value: str) -> NoReturn:
    fail(f"JSON contains unsupported non-finite number {value}")


def load_json_value(path: Path, label: str) -> tuple[Any, bytes]:
    raw = load_bytes(path, label)
    try:
        value = json.loads(
            raw.decode("utf-8", errors="strict"),
            object_pairs_hook=reject_duplicate_keys,
            parse_constant=reject_nonfinite_number,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"{label} is invalid JSON: {error}")
    return value, raw


def load_json(path: Path, label: str, *, canonical: bool) -> tuple[dict[str, Any], bytes]:
    value, raw = load_json_value(path, label)
    if not isinstance(value, dict):
        fail(f"{label} must be a JSON object")
    if canonical and raw != canonical_json(value):
        fail(f"{label} is not canonical JSON")
    return value, raw


def require_keys(value: dict[str, Any], required: set[str], label: str) -> None:
    missing = required - set(value)
    if missing:
        fail(f"{label} omits required fields: {', '.join(sorted(missing))}")


def exact_keys(value: dict[str, Any], required: set[str], label: str) -> None:
    require_keys(value, required, label)
    extra = set(value) - required
    if extra:
        fail(f"{label} contains unsupported fields: {', '.join(sorted(extra))}")


def positive_int(value: object, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        fail(f"{label} must be a positive integer")
    return value


def hash_value(value: object, label: str) -> str:
    if not isinstance(value, str) or HASH_RE.fullmatch(value) is None:
        fail(f"{label} must be a lowercase SHA-256")
    return value


def chain_hash(value: object, label: str) -> str:
    if not isinstance(value, str) or CHAIN_HASH_RE.fullmatch(value) is None:
        fail(f"{label} must be a 32-byte lowercase chain hash")
    return value.removeprefix("0x")


def commit(value: object, label: str) -> str:
    if not isinstance(value, str) or COMMIT_RE.fullmatch(value) is None:
        fail(f"{label} must be a full lowercase Git commit")
    return value


def timestamp(value: object, label: str) -> str:
    if not isinstance(value, str) or UTC_RE.fullmatch(value) is None:
        fail(f"{label} must use canonical UTC seconds")
    try:
        dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ")
    except ValueError as error:
        fail(f"{label} is invalid: {error}")
    return value


def repository_tool(relative: Path, label: str) -> tuple[Path, str]:
    """Return one exact non-symlink helper from this builder's checkout."""

    script_path = Path(__file__)
    try:
        script_stat = script_path.lstat()
    except OSError as error:
        fail(f"cannot inspect public-truth builder: {error}")
    if not stat.S_ISREG(script_stat.st_mode):
        fail("public-truth builder must be a checked-in non-symlink regular file")
    root = script_path.resolve().parents[2]
    candidate = root / relative
    raw = load_bytes(candidate, label, maximum=4 * 1024 * 1024)
    return candidate, sha256(raw)


def canonical_chain_hash(value: object, label: str) -> str:
    if (
        not isinstance(value, str)
        or re.fullmatch(r"0x[0-9a-f]{64}", value) is None
    ):
        fail(f"{label} must be canonical lowercase 0x-prefixed 32-byte hex")
    return value


def desktop_app_source_tree_sha256() -> str:
    """Hash the reviewed desktop/src tree exactly as the Node receipt helper does."""

    script_path = Path(__file__)
    root = script_path.resolve().parents[2]
    source_root = root / "desktop" / "src"
    try:
        source_stat = source_root.lstat()
    except OSError as error:
        fail(f"cannot inspect desktop app source root: {error}")
    if not stat.S_ISDIR(source_stat.st_mode) or source_root.is_symlink():
        fail("desktop app source root must be a non-symlink directory")
    entries: list[dict[str, str]] = []
    def walk_error(error: OSError) -> NoReturn:
        fail(f"cannot enumerate desktop app source tree: {error}")

    for directory, dirnames, filenames in os.walk(
        source_root, followlinks=False, onerror=walk_error
    ):
        directory_path = Path(directory)
        for name in dirnames:
            if (directory_path / name).is_symlink():
                fail(f"desktop app source contains symlink {directory_path / name}")
        for name in filenames:
            path = directory_path / name
            if path.is_symlink():
                fail(f"desktop app source contains symlink {path}")
            raw = load_bytes(
                path,
                f"desktop app source {path.relative_to(root).as_posix()}",
                maximum=4 * 1024 * 1024,
            )
            entries.append(
                {
                    "path": path.relative_to(root).as_posix(),
                    "sha256": sha256(raw),
                }
            )
    entries.sort(key=lambda row: row["path"])
    if not entries:
        fail("desktop app source tree is empty")
    return sha256(canonical_json(entries))


def run_packaged_appimage_verify(
    receipt_path: Path,
    release_binding_path: Path,
) -> dict[str, Any]:
    """Re-run the exact checked-in deep verifier over host and guest evidence."""

    verifier, verifier_sha = repository_tool(
        PACKAGED_APPIMAGE_VERIFIER_RELATIVE, "packaged AppImage live verifier"
    )
    receipt_raw = load_bytes(
        receipt_path, "packaged AppImage host receipt", maximum=MAX_INPUT_BYTES
    )
    result = subprocess.run(
        [
            sys.executable,
            "-B",
            "-I",
            str(verifier),
            "verify",
            "--binding",
            str(release_binding_path),
            "--receipt",
            str(receipt_path),
        ],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=5 * 60,
    )
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", errors="replace").strip()[-1000:]
        fail(
            "exact packaged AppImage verifier failed closed: "
            + (detail or f"exit {result.returncode}")
        )
    if b"packaged AppImage evidence verified:" not in result.stdout:
        fail("exact packaged AppImage verifier omitted its terminal verification record")
    _verifier_after, verifier_after_sha = repository_tool(
        PACKAGED_APPIMAGE_VERIFIER_RELATIVE, "packaged AppImage live verifier"
    )
    if verifier_after_sha != verifier_sha:
        fail("packaged AppImage verifier changed while it was invoked")
    if load_bytes(
        receipt_path, "packaged AppImage host receipt", maximum=MAX_INPUT_BYTES
    ) != receipt_raw:
        fail("packaged AppImage host receipt changed during verification")
    receipt, canonical_raw = load_json(
        receipt_path, "packaged AppImage host receipt", canonical=True
    )
    if (
        receipt.get("schema") != PACKAGED_APPIMAGE_HOST_SCHEMA
        or receipt.get("result") != "passed"
    ):
        fail("packaged AppImage verifier accepted a non-passing receipt")
    return {
        "receipt": receipt,
        "receiptSha256": sha256(canonical_raw),
        "stdoutSha256": sha256(result.stdout),
        "verifierPath": PACKAGED_APPIMAGE_VERIFIER_RELATIVE.as_posix(),
        "verifierSha256": verifier_sha,
    }


def load_macos_package_evidence(directory: Path) -> dict[str, dict[str, Any]]:
    """Load the exact private, canonical receipt-only macOS evidence bundle."""

    if not directory.is_absolute():
        fail("macOS package evidence directory must be absolute")
    try:
        info = directory.lstat()
        names = {entry.name for entry in directory.iterdir()}
    except OSError as error:
        fail(f"cannot inspect macOS package evidence directory: {error}")
    if (
        not stat.S_ISDIR(info.st_mode)
        or directory.is_symlink()
        or info.st_uid != os.getuid()
        or stat.S_IMODE(info.st_mode) != 0o700
        or names != set(MACOS_EVIDENCE_FILES.values())
    ):
        fail("macOS package evidence must be an operator-owned mode-0700 exact receipt directory")
    loaded: dict[str, dict[str, Any]] = {}
    for key, name in MACOS_EVIDENCE_FILES.items():
        path = directory / name
        file_info = path.lstat()
        if (
            not stat.S_ISREG(file_info.st_mode)
            or path.is_symlink()
            or file_info.st_uid != os.getuid()
            or stat.S_IMODE(file_info.st_mode) != 0o400
            or file_info.st_nlink != 1
        ):
            fail(f"macOS package evidence {name} must be owner mode 0400 with link count one")
        value, raw = load_json(path, f"macOS package evidence {name}", canonical=True)
        loaded[key] = {"path": path, "raw": raw, "sha256": sha256(raw), "value": value}
    return loaded


def _macos_bundle(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    exact_keys(
        value,
        {"appBundleTreeSha256", "executableRelativePath", "executableSha256", "executableSize"},
        label,
    )
    hash_value(value["appBundleTreeSha256"], f"{label}.appBundleTreeSha256")
    hash_value(value["executableSha256"], f"{label}.executableSha256")
    positive_int(value["executableSize"], f"{label}.executableSize")
    if value["executableRelativePath"] != "Contents/MacOS/arc-desktop":
        fail(f"{label} executable path differs")
    return value


def _macos_assets(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    exact_keys(value, {"appArchive", "appArchiveSignature", "dmg"}, label)
    expected_names = {
        "appArchive": "arc-desktop-macos-arm64.app.tar.gz",
        "appArchiveSignature": "arc-desktop-macos-arm64.app.tar.gz.sig",
        "dmg": "arc-desktop-macos-arm64.dmg",
    }
    ids: set[int] = set()
    for key, expected_name in expected_names.items():
        row = value[key]
        if not isinstance(row, dict):
            fail(f"{label}.{key} must be an object")
        exact_keys(row, {"id", "name", "sha256", "size"}, f"{label}.{key}")
        asset_id = positive_int(row["id"], f"{label}.{key}.id")
        positive_int(row["size"], f"{label}.{key}.size")
        hash_value(row["sha256"], f"{label}.{key}.sha256")
        if row["name"] != expected_name or asset_id in ids:
            fail(f"{label}.{key} identity differs")
        ids.add(asset_id)
    return value


def _macos_source(value: object, source_sha: str, controller_sha: str, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    exact_keys(value, {"commit", "path", "sha256", "treeClean"}, label)
    expected = {
        "commit": source_sha,
        "path": MACOS_PACKAGE_CONTROLLER_RELATIVE.as_posix(),
        "sha256": controller_sha,
        "treeClean": True,
    }
    if value != expected:
        fail(f"{label} differs from the exact checked-in controller")
    return value


def _macos_release(value: object, native_input: dict[str, Any], label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    exact_keys(value, {"id", "runAttempt", "runId", "tag"}, label)
    expected = {
        "id": native_input.get("releaseId"),
        "runAttempt": native_input.get("releaseRunAttempt"),
        "runId": native_input.get("releaseRunId"),
        "tag": TAG,
    }
    if value != expected or not all(
        type(value[field]) is int and value[field] > 0
        for field in ("id", "runAttempt", "runId")
    ):
        fail(f"{label} differs from the sealed native release")
    return value


def _macos_code_signature(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    exact_keys(
        value,
        {
            "appleDeveloperIdSigned", "authorities", "designatedRequirement",
            "designatedRequirementKind", "displayStderrSha256", "displayStdoutSha256",
            "gatekeeperAssessed", "hardenedRuntime", "identifier", "infoPlistSha256",
            "kind", "notarizationAssessed", "requirementsStderrSha256",
            "requirementsStdoutSha256", "teamIdentifier", "verifier", "verifyDeepStrict",
            "verifyStderrSha256", "verifyStdoutSha256",
        },
        label,
    )
    if (
        value["appleDeveloperIdSigned"] is not False
        or value["authorities"] != []
        or value["gatekeeperAssessed"] is not False
        or type(value["hardenedRuntime"]) is not bool
        or value["identifier"] != "network.arc.desktop"
        or value["kind"] != "adhoc"
        or value["notarizationAssessed"] is not False
        or value["teamIdentifier"] is not None
        or value["verifyDeepStrict"] is not True
    ):
        fail(f"{label} is not the truthful deep/strict ad-hoc code seal")
    requirement = value["designatedRequirement"]
    requirement_kind = value["designatedRequirementKind"]
    valid_identifier = (
        requirement_kind == "identifier"
        and isinstance(requirement, str)
        and 'identifier "network.arc.desktop"' in requirement
    )
    valid_cdhash = (
        requirement_kind == "cdhash"
        and isinstance(requirement, str)
        and re.fullmatch(
            r'designated => cdhash H"[0-9a-f]{40}"(?: or cdhash H"[0-9a-f]{40}")*',
            requirement,
        ) is not None
    )
    if not (valid_identifier or valid_cdhash):
        fail(f"{label} designated requirement differs")
    for field in (
        "displayStderrSha256", "displayStdoutSha256", "infoPlistSha256",
        "requirementsStderrSha256", "requirementsStdoutSha256",
        "verifyStderrSha256", "verifyStdoutSha256",
    ):
        hash_value(value[field], f"{label}.{field}")
    verifier = value["verifier"]
    if not isinstance(verifier, dict):
        fail(f"{label}.verifier must be an object")
    exact_keys(verifier, {"path", "resolvedPath", "sha256", "size"}, f"{label}.verifier")
    if verifier["path"] != "/usr/bin/codesign" or verifier["resolvedPath"] != "/usr/bin/codesign":
        fail(f"{label} used another codesign path")
    hash_value(verifier["sha256"], f"{label}.verifier.sha256")
    positive_int(verifier["size"], f"{label}.verifier.size")
    return value


def _macos_mount(value: object, label: str, *, expected_basename: str) -> None:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    exact_keys(
        value,
        {
            "attachPlistSha256", "device", "diskutilInfoPlistSha256", "filesystem",
            "hdiutilInfoPlistSha256", "imagePathMatched", "imagePathSha256",
            "mountFlags", "mountPointBasename", "nobrowse", "noowners",
            "readOnlyMedia", "readOnlyVolume", "statvfsReadOnly", "tools",
            "verifyStderrSha256", "verifyStdoutSha256", "writeable",
        },
        label,
    )
    if (
        value["mountPointBasename"] != expected_basename
        or value["imagePathMatched"] is not True
        or value["nobrowse"] is not True
        or value["noowners"] is not True
        or value["readOnlyMedia"] is not True
        or value["readOnlyVolume"] is not True
        or value["statvfsReadOnly"] is not True
        or value["writeable"] is not False
        or not isinstance(value["device"], str)
        or re.fullmatch(r"/dev/disk[0-9]+(?:s[0-9]+)*", value["device"]) is None
        or not isinstance(value["filesystem"], str)
        or not value["filesystem"]
        or len(value["filesystem"]) > 64
    ):
        fail(f"{label} is not the exact read-only/noowners/nobrowse mount")
    flags = value["mountFlags"]
    if not isinstance(flags, dict):
        fail(f"{label}.mountFlags must be an object")
    exact_keys(flags, {"lineSha256", "options"}, f"{label}.mountFlags")
    if (
        not isinstance(flags["options"], list)
        or not {"read-only", "noowners", "nobrowse"}.issubset(flags["options"])
        or "read-write" in flags["options"]
    ):
        fail(f"{label} mount flags differ")
    hash_value(flags["lineSha256"], f"{label}.mountFlags.lineSha256")
    for field in (
        "attachPlistSha256", "diskutilInfoPlistSha256", "hdiutilInfoPlistSha256",
        "imagePathSha256", "verifyStderrSha256", "verifyStdoutSha256",
    ):
        hash_value(value[field], f"{label}.{field}")
    tools = value["tools"]
    expected_tools = {
        "codesign": "/usr/bin/codesign",
        "diskutil": "/usr/sbin/diskutil",
        "hdiutil": "/usr/bin/hdiutil",
        "mount": "/sbin/mount",
    }
    if not isinstance(tools, dict) or set(tools) != set(expected_tools):
        fail(f"{label} tool inventory differs")
    for name, expected_path in expected_tools.items():
        identity = tools[name]
        if not isinstance(identity, dict):
            fail(f"{label}.{name} identity must be an object")
        exact_keys(
            identity,
            {"path", "resolvedPath", "sha256", "size"},
            f"{label}.{name}",
        )
        if identity["path"] != expected_path or identity["resolvedPath"] != expected_path:
            fail(f"{label}.{name} used another executable")
        hash_value(identity["sha256"], f"{label}.{name}.sha256")
        positive_int(identity["size"], f"{label}.{name}.size")


def _macos_detach(value: object, label: str) -> None:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    exact_keys(
        value,
        {
            "detachStderrSha256", "detachStdoutSha256", "detached",
            "mountPointEmpty", "postDetachInfoPlistSha256",
        },
        label,
    )
    if value["detached"] is not True or value["mountPointEmpty"] is not True:
        fail(f"{label} did not complete cleanly")
    for field in (
        "detachStderrSha256", "detachStdoutSha256", "postDetachInfoPlistSha256"
    ):
        hash_value(value[field], f"{label}.{field}")


def _macos_tree_summary(value: object, label: str, expected_sha256: str) -> None:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    exact_keys(value, {"entryCount", "sha256", "totalRegularBytes"}, label)
    positive_int(value["entryCount"], f"{label}.entryCount")
    positive_int(value["totalRegularBytes"], f"{label}.totalRegularBytes")
    if value["sha256"] != expected_sha256:
        fail(f"{label} digest differs from the complete bundle")


def _macos_root_inventory(value: object, label: str) -> None:
    if not isinstance(value, list) or not value or len(value) > 64:
        fail(f"{label} must contain 1..64 entries")
    names: list[str] = []
    app_count = 0
    for index, row in enumerate(value):
        if not isinstance(row, dict):
            fail(f"{label}[{index}] must be an object")
        exact_keys(row, {"kind", "mode", "name", "target"}, f"{label}[{index}]")
        name = row["name"]
        kind = row["kind"]
        target = row["target"]
        try:
            name_size = len(name.encode("ascii", "strict")) if isinstance(name, str) else -1
        except UnicodeEncodeError:
            name_size = -1
        if (
            not isinstance(name, str)
            or not name
            or not 0 < name_size <= 255
            or any(character in name for character in "\r\n\t/")
            or name in names
            or kind not in {"directory", "file", "symlink"}
            or type(row["mode"]) is not int
            or not 0 <= row["mode"] <= 0o7777
            or (target is not None and not isinstance(target, str))
        ):
            fail(f"{label}[{index}] is unsafe or malformed")
        if name.endswith(".app"):
            app_count += 1
            if name != "ARC Node.app" or kind != "directory":
                fail(f"{label} contains an unexpected app bundle")
        if name == "Applications" and target not in {None, "/Applications"}:
            fail(f"{label} Applications link differs")
        names.append(name)
    if names != sorted(names) or app_count != 1:
        fail(f"{label} is not the sorted single-app DMG inventory")


def validate_macos_package_evidence(
    wrapper: dict[str, Any],
    evidence: dict[str, dict[str, Any]],
    *,
    source_sha: str,
    config_sha256: str,
    rollout_sha256: str,
) -> None:
    """Independently cross-bind the original macOS controller receipts."""

    package = wrapper.get("packageEvidence")
    if not isinstance(package, dict):
        fail("packaged native wrapper omits macOS package provenance")
    exact_keys(
        package,
        {
            "controllerPath", "controllerSha256", "inspectionReceipt",
            "inspectionReceiptSha256", "provenanceReceipt", "provenanceReceiptSha256",
            "signatureReceipt", "signatureReceiptSha256", "verificationReceipt",
            "verificationReceiptSha256",
        },
        "macOS package evidence wrapper",
    )
    _controller, controller_sha = repository_tool(
        MACOS_PACKAGE_CONTROLLER_RELATIVE, "macOS package provenance controller"
    )
    if package["controllerPath"] != MACOS_PACKAGE_CONTROLLER_RELATIVE.as_posix() or package["controllerSha256"] != controller_sha:
        fail("macOS package wrapper used another controller")
    for embedded, key in (
        ("inspectionReceipt", "inspection"),
        ("provenanceReceipt", "provenance"),
        ("signatureReceipt", "signature"),
        ("verificationReceipt", "verification"),
    ):
        if (
            package[embedded] != evidence[key]["value"]
            or package[f"{embedded}Sha256"] != evidence[key]["sha256"]
        ):
            fail(f"macOS package wrapper {embedded} differs from the imported original")
    if wrapper["receipt"] != evidence["nativeReceipt"]["value"]:
        fail("packaged native embedded receipt differs from the imported original")
    if (
        wrapper["receiptSha256"] != evidence["nativeReceipt"]["sha256"]
        or wrapper["inputSha256"] != evidence["nativeInput"]["sha256"]
        or wrapper["dispatchAttemptSha256"] != evidence["nativeAttempt"]["sha256"]
    ):
        fail("packaged native original input/attempt/receipt hashes differ")

    native_input = evidence["nativeInput"]["value"]
    native_attempt = evidence["nativeAttempt"]["value"]
    native_receipt = evidence["nativeReceipt"]["value"]
    require_keys(
        native_input,
        {
            "assets", "challenge", "expectedBundle", "expectedCoordinator",
            "frontendConfigSha256", "releaseId", "releaseRunAttempt", "releaseRunId",
            "repository", "rolloutManifestSha256", "schema", "sourceCommit",
        },
        "original packaged native input",
    )
    if (
        native_input["schema"] != "arc.packaged-desktop-native-input.v1"
        or native_input["repository"] != REPOSITORY
        or native_input["sourceCommit"] != source_sha
        or native_input["frontendConfigSha256"] != config_sha256
        or native_input["rolloutManifestSha256"] != rollout_sha256
        or native_input["expectedCoordinator"] != "https://140.82.16.112"
        or native_input["challenge"] != wrapper["challenge"]
    ):
        fail("original packaged native input differs from the accepted release/session")
    native_assets = _macos_assets(native_input["assets"], "original native assets")
    native_bundle = _macos_bundle(native_input["expectedBundle"], "original native bundle")
    exact_keys(
        native_attempt,
        {"armedAt", "challenge", "dispatchLimit", "executableSha256", "inputSha256", "schema", "sourceCommit", "sourceHost"},
        "original native dispatch attempt",
    )
    if (
        native_attempt["schema"] != "arc.packaged-desktop-native-dispatch-attempt.v1"
        or native_attempt["challenge"] != native_input["challenge"]
        or native_attempt["dispatchLimit"] != 1
        or native_attempt["executableSha256"] != native_bundle["executableSha256"]
        or native_attempt["inputSha256"] != evidence["nativeInput"]["sha256"]
        or native_attempt["sourceCommit"] != source_sha
        or native_attempt["sourceHost"] != native_input["expectedCoordinator"]
    ):
        fail("original native dispatch attempt binding differs")
    if native_receipt.get("inputSha256") != evidence["nativeInput"]["sha256"] or native_receipt.get("dispatchAttemptSha256") != evidence["nativeAttempt"]["sha256"]:
        fail("original native receipt input/attempt binding differs")

    signature = evidence["signature"]["value"]
    exact_keys(
        signature,
        {"assets", "bindingSha256", "completedAt", "disposableVm", "guest", "guestReceiptSha256", "limactl", "release", "repository", "schema", "source", "sourceCommit", "updaterPublicKeySha256", "verified"},
        "macOS updater-signature receipt",
    )
    if signature["schema"] != MACOS_UPDATER_SIGNATURE_SCHEMA or signature["repository"] != REPOSITORY or signature["sourceCommit"] != source_sha or signature["verified"] is not True:
        fail("macOS updater-signature result differs")
    _macos_source(signature["source"], source_sha, controller_sha, "macOS updater source")
    _macos_release(signature["release"], native_input, "macOS updater release")
    if signature["assets"] != {"appArchive": native_assets["appArchive"], "appArchiveSignature": native_assets["appArchiveSignature"]}:
        fail("macOS updater-signature assets differ")
    hash_value(signature["bindingSha256"], "macOS updater binding")
    hash_value(signature["updaterPublicKeySha256"], "macOS updater key")
    vm = signature["disposableVm"]
    if not isinstance(vm, dict):
        fail("macOS updater disposable VM is absent")
    exact_keys(vm, {"configSha256", "deletedAfterEvidenceRead", "imageDigest", "imageUrl", "mounts", "name", "preexistingInstancesUnchanged", "recoveryEnclaveAccessed"}, "macOS updater disposable VM")
    if (
        vm["deletedAfterEvidenceRead"] is not True
        or vm["imageDigest"] != "sha256:5c3ddb00f60bc455dac0862fabe9d8bacec46c33ac1751143c5c3683404b110d"
        or vm["imageUrl"] != "https://cloud-images.ubuntu.com/releases/noble/release-20260321/ubuntu-24.04-server-cloudimg-amd64.img"
        or vm["mounts"] != []
        or vm["preexistingInstancesUnchanged"] is not True
        or vm["recoveryEnclaveAccessed"] is not False
    ):
        fail("macOS updater disposable VM isolation differs")
    guest = signature["guest"]
    if not isinstance(guest, dict):
        fail("macOS updater guest receipt is absent")
    exact_keys(guest, {"apt", "archiveSha256", "completedAt", "helperSha256", "minisign", "publicKeySha256", "schema", "signatureSha256", "verification"}, "macOS updater guest receipt")
    if (
        guest["schema"] != "arc.macos-updater-signature-guest.v1"
        or guest["archiveSha256"] != native_assets["appArchive"]["sha256"]
        or guest["signatureSha256"] != native_assets["appArchiveSignature"]["sha256"]
        or guest["publicKeySha256"] != signature["updaterPublicKeySha256"]
        or guest["helperSha256"] != controller_sha
        or not isinstance(guest["verification"], dict)
        or guest["verification"].get("verified") is not True
        or signature["guestReceiptSha256"] != sha256(canonical_json(guest))
    ):
        fail("macOS updater guest signature proof differs")
    apt = guest["apt"]
    if not isinstance(apt, dict):
        fail("macOS updater APT evidence is absent")
    exact_keys(apt, {"installStderrSha256", "installStdoutSha256", "snapshot", "sourceSha256", "updateStderrSha256", "updateStdoutSha256"}, "macOS updater APT evidence")
    if apt["snapshot"] != "20260321T235959Z":
        fail("macOS updater APT snapshot differs")
    minisign = guest["minisign"]
    if not isinstance(minisign, dict) or set(minisign) != {"binary", "package"}:
        fail("macOS updater minisign identity differs")
    binary = minisign["binary"]
    package_id = minisign["package"]
    if (
        not isinstance(binary, dict)
        or binary.get("path") != "/usr/bin/minisign"
        or binary.get("resolvedPath") != "/usr/bin/minisign"
        or not isinstance(package_id, dict)
        or set(package_id) != {"architecture", "version"}
        or package_id.get("architecture") != "amd64"
    ):
        fail("macOS updater minisign toolchain differs")

    inspection = evidence["inspection"]["value"]
    exact_keys(inspection, {"assets", "bindingSha256", "bundle", "codeSignature", "completedAt", "dmg", "extraction", "release", "repository", "schema", "source", "sourceCommit", "updaterSignature"}, "macOS package inspection")
    if (
        inspection["schema"] != MACOS_PACKAGE_INSPECTION_SCHEMA
        or inspection["repository"] != REPOSITORY
        or inspection["sourceCommit"] != source_sha
        or inspection["bindingSha256"] != signature["bindingSha256"]
        or inspection["assets"] != native_assets
        or inspection["bundle"] != native_bundle
        or inspection["source"] != signature["source"]
        or inspection["release"] != signature["release"]
    ):
        fail("macOS package inspection cross-binding differs")
    inspected_code = _macos_code_signature(inspection["codeSignature"], "macOS inspected code signature")
    updater = inspection["updaterSignature"]
    if not isinstance(updater, dict):
        fail("macOS inspection updater proof is absent")
    exact_keys(updater, {"receiptSha256", "schema", "updaterPublicKeySha256", "verified"}, "macOS inspection updater proof")
    if updater != {"receiptSha256": evidence["signature"]["sha256"], "schema": MACOS_UPDATER_SIGNATURE_SCHEMA, "updaterPublicKeySha256": signature["updaterPublicKeySha256"], "verified": True}:
        fail("macOS inspection updater proof differs")
    extraction = inspection["extraction"]
    if not isinstance(extraction, dict) or extraction.get("appBundleTreeSha256") != native_bundle["appBundleTreeSha256"] or extraction.get("archiveSha256AfterExtraction") != native_assets["appArchive"]["sha256"] or extraction.get("implementation") != "arc-openat-create-only-tar-extractor-v1":
        fail("macOS package safe extraction evidence differs")
    safety = extraction.get("safety")
    if not isinstance(safety, dict) or not safety or any(value is not True for value in safety.values()):
        fail("macOS package extraction safety proof is incomplete")
    dmg_inspection = inspection["dmg"]
    if not isinstance(dmg_inspection, dict):
        fail("macOS inspection DMG evidence is absent")
    exact_keys(
        dmg_inspection,
        {"appBundleTree", "attach", "detach", "rootInventory", "sha256AfterDetach"},
        "macOS inspection DMG evidence",
    )
    if dmg_inspection["sha256AfterDetach"] != native_assets["dmg"]["sha256"]:
        fail("macOS inspection DMG digest differs")
    _macos_mount(
        dmg_inspection.get("attach"),
        "macOS inspection DMG mount",
        expected_basename="inspect-dmg-mount",
    )
    _macos_detach(dmg_inspection["detach"], "macOS inspection DMG detach")
    _macos_tree_summary(
        dmg_inspection["appBundleTree"],
        "macOS inspection mounted tree",
        native_bundle["appBundleTreeSha256"],
    )
    _macos_root_inventory(
        dmg_inspection["rootInventory"], "macOS inspection DMG root inventory"
    )

    controller_attempt = evidence["controllerAttempt"]["value"]
    exact_keys(controller_attempt, {"armedAt", "challenge", "executableSha256", "inputSha256", "inspectionSha256", "outerTimeoutSeconds", "retryPermitted", "schema", "sourceCommit", "state"}, "macOS native controller attempt")
    if (
        controller_attempt["schema"] != "arc.macos-native-controller-attempt.v1"
        or controller_attempt["challenge"] != native_input["challenge"]
        or controller_attempt["executableSha256"] != native_bundle["executableSha256"]
        or controller_attempt["inputSha256"] != evidence["nativeInput"]["sha256"]
        or controller_attempt["inspectionSha256"] != evidence["inspection"]["sha256"]
        or controller_attempt["outerTimeoutSeconds"] != 4360
        or controller_attempt["retryPermitted"] is not False
        or controller_attempt["sourceCommit"] != source_sha
        or controller_attempt["state"] != "armed-no-retry"
    ):
        fail("macOS native controller no-retry binding differs")

    provenance = evidence["provenance"]["value"]
    exact_keys(provenance, {"assets", "bindingSha256", "bundle", "codeSignature", "completedAt", "controllerAttemptSha256", "dmgExecution", "extractedArchiveBundleAfter", "inspectionSha256", "nativeDispatchAttemptSha256", "nativeExecution", "nativeInputSha256", "nativeReceiptSha256", "release", "repository", "schema", "source", "sourceCommit", "truthScope", "updaterSignatureReceiptSha256"}, "macOS package provenance")
    if (
        provenance["schema"] != MACOS_PACKAGE_PROVENANCE_SCHEMA
        or provenance["repository"] != REPOSITORY
        or provenance["sourceCommit"] != source_sha
        or provenance["source"] != signature["source"]
        or provenance["bindingSha256"] != signature["bindingSha256"]
        or provenance["assets"] != native_assets
        or provenance["release"] != signature["release"]
        or provenance["bundle"] != native_bundle
        or provenance["extractedArchiveBundleAfter"] != native_bundle
        or provenance["inspectionSha256"] != evidence["inspection"]["sha256"]
        or provenance["updaterSignatureReceiptSha256"] != evidence["signature"]["sha256"]
        or provenance["nativeInputSha256"] != evidence["nativeInput"]["sha256"]
        or provenance["nativeReceiptSha256"] != evidence["nativeReceipt"]["sha256"]
        or provenance["nativeDispatchAttemptSha256"] != evidence["nativeAttempt"]["sha256"]
        or provenance["controllerAttemptSha256"] != evidence["controllerAttempt"]["sha256"]
        or provenance["truthScope"] != MACOS_PACKAGE_TRUTH_SCOPE
    ):
        fail("macOS package provenance cross-binding/truth scope differs")
    code = provenance["codeSignature"]
    if not isinstance(code, dict):
        fail("macOS provenance code-signature proof is absent")
    exact_keys(code, {"after", "before", "semanticIdentityUnchanged"}, "macOS provenance code signature")
    before_code = _macos_code_signature(code["before"], "macOS provenance pre-run code signature")
    after_code = _macos_code_signature(code["after"], "macOS provenance post-run code signature")
    if code["semanticIdentityUnchanged"] is not True or before_code != after_code or before_code != inspected_code:
        fail("macOS package code-signature identity changed")
    execution = provenance["dmgExecution"]
    if not isinstance(execution, dict):
        fail("macOS provenance DMG execution is absent")
    exact_keys(execution, {"attach", "bundleAfter", "bundleBefore", "detach", "treeAfter", "treeBefore"}, "macOS provenance DMG execution")
    _macos_mount(
        execution["attach"],
        "macOS native DMG mount",
        expected_basename="native-dmg-mount",
    )
    if execution["bundleBefore"] != native_bundle or execution["bundleAfter"] != native_bundle or execution["treeBefore"] != execution["treeAfter"]:
        fail("macOS native mounted package identity changed")
    _macos_tree_summary(
        execution["treeBefore"],
        "macOS native mounted tree",
        native_bundle["appBundleTreeSha256"],
    )
    _macos_detach(execution["detach"], "macOS native DMG detach")
    process = provenance["nativeExecution"]
    if not isinstance(process, dict):
        fail("macOS native execution proof is absent")
    if process.get("innerMaxSeconds") != 4300 or process.get("outerTimeoutSeconds") != 4360 or process.get("returnCode") != 0 or process.get("timedOut") is not False or type(process.get("elapsedMs")) is not int or not 0 <= process["elapsedMs"] <= 4_360_000:
        fail("macOS native execution result/budget differs")

    verification = evidence["verification"]["value"]
    exact_keys(verification, {"assets", "bindingSha256", "bundle", "completedAt", "controllerAttemptSha256", "inspectionSha256", "nativeDispatchAttemptSha256", "nativeInputSha256", "nativeReceiptSha256", "provenanceSha256", "release", "repository", "schema", "source", "sourceCommit", "updaterSignatureReceiptSha256", "verified"}, "macOS package provenance verification")
    if (
        verification["schema"] != MACOS_PACKAGE_PROVENANCE_VERIFICATION_SCHEMA
        or verification["verified"] is not True
        or verification["repository"] != REPOSITORY
        or verification["sourceCommit"] != source_sha
        or verification["source"] != signature["source"]
        or verification["assets"] != native_assets
        or verification["bundle"] != native_bundle
        or verification["release"] != signature["release"]
        or verification["bindingSha256"] != signature["bindingSha256"]
        or verification["controllerAttemptSha256"] != evidence["controllerAttempt"]["sha256"]
        or verification["inspectionSha256"] != evidence["inspection"]["sha256"]
        or verification["nativeDispatchAttemptSha256"] != evidence["nativeAttempt"]["sha256"]
        or verification["nativeInputSha256"] != evidence["nativeInput"]["sha256"]
        or verification["nativeReceiptSha256"] != evidence["nativeReceipt"]["sha256"]
        or verification["provenanceSha256"] != evidence["provenance"]["sha256"]
        or verification["updaterSignatureReceiptSha256"] != evidence["signature"]["sha256"]
    ):
        fail("macOS package provenance verification differs")


def validate_packaged_native_wrapper(
    wrapper: object,
    *,
    source_sha: str,
    config_sha256: str,
    rollout_sha256: str,
    macos_evidence: dict[str, dict[str, Any]],
) -> None:
    if not isinstance(wrapper, dict):
        fail("desktop live receipt omits packaged native evidence")
    exact_keys(
        wrapper,
        {
            "appArchiveSha256", "appArchiveSignatureSha256",
            "appBundleTreeSha256", "challenge", "dispatchAttemptSha256",
            "dmgSha256", "executableSha256", "inputHashVerification",
            "inputSha256", "packageEvidence", "receipt", "receiptSha256", "scope",
        },
        "packaged native wrapper",
    )
    if wrapper["scope"] != PACKAGED_NATIVE_SCOPE:
        fail("packaged native evidence widened its macOS arm64 native-core scope")
    for field in (
        "appArchiveSha256", "appArchiveSignatureSha256", "appBundleTreeSha256",
        "dispatchAttemptSha256", "dmgSha256", "executableSha256",
        "inputSha256", "receiptSha256",
    ):
        hash_value(wrapper[field], f"packaged native {field}")
    receipt = wrapper["receipt"]
    if not isinstance(receipt, dict):
        fail("packaged native wrapper receipt must be an object")
    require_keys(
        receipt,
        {
            "assets", "bundle", "challenge", "dispatch", "dispatchAttemptSha256",
            "expectedCoordinator", "expectedModelId", "frontendConfigSha256",
            "immutableSessionOrigin", "inputSha256", "negativeRouteChecks",
            "network", "receiptPoll", "repository", "rolloutManifestSha256",
            "runtime", "schema", "sourceCommit", "transaction", "worker",
        },
        "packaged native receipt",
    )
    if (
        receipt["schema"] != "arc.packaged-desktop-native-acceptance.v1"
        or receipt["repository"] != REPOSITORY
        or receipt["sourceCommit"] != source_sha
        or receipt["frontendConfigSha256"] != config_sha256
        or receipt["rolloutManifestSha256"] != rollout_sha256
        or receipt["expectedCoordinator"] != "https://140.82.16.112"
        or receipt["immutableSessionOrigin"] is not True
        or receipt["challenge"] != wrapper["challenge"]
        or receipt["inputSha256"] != wrapper["inputSha256"]
        or receipt["dispatchAttemptSha256"] != wrapper["dispatchAttemptSha256"]
    ):
        fail("packaged native receipt is not bound to the exact release/session")
    if sha256(canonical_json(receipt)) != wrapper["receiptSha256"]:
        fail("packaged native embedded receipt differs from its canonical hash")
    worker = canonical_chain_hash(receipt["worker"], "packaged native worker")
    model = canonical_chain_hash(receipt["expectedModelId"], "packaged native model")
    bundle = receipt["bundle"]
    if not isinstance(bundle, dict) or (
        bundle.get("appBundleTreeSha256") != wrapper["appBundleTreeSha256"]
        or bundle.get("executableSha256") != wrapper["executableSha256"]
    ):
        fail("packaged native running bundle differs from wrapper identity")
    assets = receipt["assets"]
    if not isinstance(assets, dict) or (
        not isinstance(assets.get("appArchive"), dict)
        or not isinstance(assets.get("appArchiveSignature"), dict)
        or not isinstance(assets.get("dmg"), dict)
        or assets["appArchive"].get("sha256") != wrapper["appArchiveSha256"]
        or assets["appArchiveSignature"].get("sha256")
        != wrapper["appArchiveSignatureSha256"]
        or assets["dmg"].get("sha256") != wrapper["dmgSha256"]
    ):
        fail("packaged native assets differ from wrapper identity")
    runtime = receipt["runtime"]
    exact_keys(
        runtime,
        {
            "appDataRelativePath", "appVersion", "architecture",
            "buildSourceCommit", "environmentNames", "environmentSha256",
            "ipcHandlersRegistered", "isolatedHomeBasename", "operatingSystem",
            "pluginsLoaded", "tauriBuilderStarted", "webviewsCreated",
        },
        "packaged native runtime",
    )
    if (
        runtime["architecture"] != "aarch64"
        or runtime["operatingSystem"] != "macos"
        or runtime["buildSourceCommit"] != source_sha
        or runtime["environmentNames"] != ["HOME", "LANG", "LC_ALL", "PATH", "TMPDIR"]
        or runtime["ipcHandlersRegistered"] is not False
        or runtime["pluginsLoaded"] is not False
        or runtime["tauriBuilderStarted"] is not False
        or runtime["webviewsCreated"] != 0
    ):
        fail("packaged native executable was not isolated before Tauri startup")
    hash_value(runtime["environmentSha256"], "packaged native environment")
    dispatch = receipt["dispatch"]
    if not isinstance(dispatch, dict) or dispatch.get("count") != 1:
        fail("packaged native dispatch is not exactly one accepted POST")
    result = dispatch.get("result")
    poll = receipt["receiptPoll"]
    terminal = poll.get("receipt") if isinstance(poll, dict) else None
    if not isinstance(result, dict) or not isinstance(terminal, dict):
        fail("packaged native receipt omits inference/terminal evidence")
    provisional = result.get("settlement")
    if not isinstance(provisional, dict):
        fail("packaged native inference omits provisional settlement")
    for field in ("txHash", "jobId", "worker", "receiptUrl", "txType", "submitted"):
        if provisional.get(field) != terminal.get(field):
            fail(f"packaged native provisional {field} differs from terminal receipt")
    for field in ("txHash", "jobId", "worker", "modelId", "inputHash", "outputHash"):
        canonical_chain_hash(terminal.get(field), f"packaged native terminal {field}")
    prompt = (
        f"ARC packaged v0.8.0 production acceptance challenge {wrapper['challenge']} "
        f"input {wrapper['inputSha256']}"
    )
    if (
        result.get("input") != prompt
        or dispatch.get("promptSha256") != sha256(prompt.encode("utf-8"))
        or terminal.get("inputHash") != f"0x{blake3_short(prompt.encode('utf-8'))}"
        or not isinstance(result.get("output"), str)
        or not result["output"].strip()
        or terminal.get("outputHash")
        != f"0x{blake3_short(result['output'].encode('utf-8'))}"
    ):
        fail("packaged native transaction is not bound to exact inference bytes")
    if (
        terminal.get("status") != "mined_success"
        or terminal.get("txType") != "0x25"
        or terminal.get("submitted") is not True
        or terminal.get("included") is not True
        or terminal.get("confirmed") is not True
        or terminal.get("success") is not True
        or terminal.get("rewardBase") != REWARD_PER_RECEIPT_BASE
        or terminal.get("rewardArc") != 2.5
        or not isinstance(terminal.get("validatorApprovals"), int)
        or isinstance(terminal.get("validatorApprovals"), bool)
        or terminal.get("validatorApprovals", 0) < 5
        or terminal.get("worker") != worker
        or terminal.get("modelId") != model
        or terminal.get("modelId") != result.get("modelHash")
        or terminal.get("outputHash") != result.get("outputHash")
        or terminal.get("receiptUrl")
        != f"/community/reward_receipt/{terminal.get('txHash')}"
    ):
        fail("packaged native terminal receipt differs from its inference result")
    input_verification = wrapper["inputHashVerification"]
    exact_keys(
        input_verification,
        {
            "algorithm", "implementation", "implementationSourceSha256",
            "maximumInputBytes", "promptHash",
        },
        "packaged native input-hash verification",
    )
    if (
        input_verification["algorithm"] != "BLAKE3-256"
        or input_verification["implementation"]
        != "arc-reviewed-js-blake3-one-chunk-v1"
        or input_verification["maximumInputBytes"] != 1024
        or terminal.get("inputHash") != input_verification["promptHash"]
    ):
        fail("packaged native terminal input is not independently prompt-bound")
    hash_value(
        input_verification["implementationSourceSha256"],
        "packaged native BLAKE3 verifier source",
    )
    validate_macos_package_evidence(
        wrapper,
        macos_evidence,
        source_sha=source_sha,
        config_sha256=config_sha256,
        rollout_sha256=rollout_sha256,
    )


def validate_packaged_appimage_wrapper(
    wrapper: object,
    verified: dict[str, Any],
    *,
    source_sha: str,
) -> None:
    if not isinstance(wrapper, dict):
        fail("desktop live receipt omits packaged AppImage evidence")
    exact_keys(
        wrapper,
        {"gatePath", "gateSha256", "receipt", "receiptSha256", "scope"},
        "packaged AppImage wrapper",
    )
    if (
        wrapper["scope"] != PACKAGED_APPIMAGE_SCOPE
        or wrapper["gatePath"] != PACKAGED_APPIMAGE_VERIFIER_RELATIVE.as_posix()
        or wrapper["gateSha256"] != verified["verifierSha256"]
        or wrapper["receiptSha256"] != verified["receiptSha256"]
        or wrapper["receipt"] != verified["receipt"]
        or wrapper["receipt"].get("schema") != PACKAGED_APPIMAGE_HOST_SCHEMA
        or wrapper["receipt"].get("result") != "passed"
        or wrapper["receipt"].get("release", {}).get("commit") != source_sha
    ):
        fail("packaged AppImage wrapper differs from independently verified evidence")
    release_assets = wrapper["receipt"].get("release", {}).get("assets")
    expected_assets = {
        "arc-desktop-linux-x86_64.AppImage",
        "arc-desktop-linux-x86_64.AppImage.sig",
        "arc-node-linux-x86_64",
    }
    if not isinstance(release_assets, dict) or set(release_assets) != expected_assets:
        fail("packaged AppImage receipt assets differ")
    appimage_asset_ids: set[int] = set()
    for name, asset in release_assets.items():
        if not isinstance(asset, dict):
            fail(f"packaged AppImage asset {name} must be an object")
        asset_id = positive_int(asset.get("id"), f"packaged AppImage asset {name}.id")
        if asset_id in appimage_asset_ids:
            fail("packaged AppImage asset IDs are not distinct")
        appimage_asset_ids.add(asset_id)


def validate_desktop_live_receipt(
    path: Path,
    *,
    source_sha: str,
    config_raw: bytes,
    reward: dict[str, Any],
    rollout_sha256: str,
    appimage_verification: dict[str, Any],
    macos_evidence: dict[str, dict[str, Any]],
) -> dict[str, Any]:
    """Validate and embed the exact successful desktop live-product receipt."""

    receipt, receipt_raw = load_json(
        path, "desktop live-product receipt", canonical=True
    )
    exact_keys(
        receipt,
        {
            "appSourceTreeSha256", "blockReceipt", "earnings", "forward",
            "frontendConfigSha256", "packageLockSha256",
            "packagedAppImage", "packagedNative", "playwrightReportSha256",
            "pollContract", "repository",
            "rewardReceipt", "rewardTx", "rpcOrigin", "rpcPort", "schema",
            "runtime", "sourceCommit", "suite", "worker",
        },
        "desktop live-product receipt",
    )
    if (
        receipt["schema"] != DESKTOP_LIVE_SCHEMA
        or receipt["repository"] != REPOSITORY
        or receipt["sourceCommit"] != source_sha
        or receipt["frontendConfigSha256"] != sha256(config_raw)
    ):
        fail("desktop live-product receipt differs from the accepted source/config")
    rollout_sha256 = hash_value(rollout_sha256, "rollout manifest SHA-256")
    validate_packaged_native_wrapper(
        receipt["packagedNative"],
        source_sha=source_sha,
        config_sha256=sha256(config_raw),
        rollout_sha256=rollout_sha256,
        macos_evidence=macos_evidence,
    )
    validate_packaged_appimage_wrapper(
        receipt["packagedAppImage"],
        appimage_verification,
        source_sha=source_sha,
    )
    port = positive_int(receipt["rpcPort"], "desktop live RPC port")
    if port > 65_535 or receipt["rpcOrigin"] != f"http://127.0.0.1:{port}":
        fail("desktop live-product receipt has an invalid loopback RPC origin")
    worker = canonical_chain_hash(receipt["worker"], "desktop live worker")
    reward_tx = canonical_chain_hash(
        receipt["rewardTx"], "desktop live reward transaction"
    )
    hash_value(receipt["playwrightReportSha256"], "Playwright report SHA-256")

    package_lock_path, package_lock_sha = repository_tool(
        DESKTOP_PACKAGE_LOCK_RELATIVE, "desktop package lock"
    )
    if receipt["packageLockSha256"] != package_lock_sha:
        fail("desktop live receipt package lock differs from protected source")
    if receipt["appSourceTreeSha256"] != desktop_app_source_tree_sha256():
        fail("desktop live receipt app source tree differs from protected source")
    suite = receipt["suite"]
    exact_keys(
        suite,
        {"durationMs", "fileSha256", "playwrightVersion", "startedAt", "testCount"},
        "desktop live suite",
    )
    duration = suite["durationMs"]
    if isinstance(duration, bool) or not isinstance(duration, (int, float)) or duration < 0:
        fail("desktop live suite duration must be a non-negative number")
    started_at = suite["startedAt"]
    if not isinstance(started_at, str) or re.fullmatch(
        r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z",
        started_at,
    ) is None:
        fail("desktop live suite start time must use canonical UTC milliseconds")
    try:
        dt.datetime.strptime(started_at, "%Y-%m-%dT%H:%M:%S.%fZ")
    except ValueError as error:
        fail(f"desktop live suite start time is invalid: {error}")
    if suite["testCount"] != 4:
        fail("desktop live suite did not execute the exact four reviewed tests")
    file_hashes = suite["fileSha256"]
    if not isinstance(file_hashes, dict) or set(file_hashes) != {
        item.as_posix() for item in DESKTOP_LIVE_SUITE_RELATIVES
    }:
        fail("desktop live suite file set differs from protected source")
    for relative in DESKTOP_LIVE_SUITE_RELATIVES:
        _file, expected = repository_tool(
            relative, f"desktop live suite file {relative.as_posix()}"
        )
        if file_hashes[relative.as_posix()] != expected:
            fail(f"desktop live suite file {relative.as_posix()} differs from protected source")
    package_lock, _ = load_json(
        package_lock_path, "desktop package lock", canonical=False
    )
    locked_playwright = (
        package_lock.get("packages", {})
        .get("node_modules/@playwright/test", {})
        .get("version")
    )
    if suite["playwrightVersion"] != locked_playwright:
        fail("desktop live Playwright runtime differs from the protected package lock")
    if receipt["pollContract"] != {"maxPolls": 61, "maxWaitMs": 180_000}:
        fail("desktop live receipt poll budget differs from 61 polls / 180 seconds")

    runtime = receipt["runtime"]
    exact_keys(
        runtime,
        {
            "nodeArch", "nodeDistributionArchiveSha256",
            "nodeExecutableSha256", "nodePlatform", "nodeVersion",
            "npmCliSha256", "npmPackageSha256", "npmVersion",
        },
        "desktop live runtime",
    )
    if runtime != {
        "nodeArch": "arm64",
        "nodeDistributionArchiveSha256": DESKTOP_NODE_ARCHIVE_SHA256,
        "nodeExecutableSha256": DESKTOP_NODE_EXECUTABLE_SHA256,
        "nodePlatform": "darwin",
        "nodeVersion": "v24.20.0",
        "npmCliSha256": DESKTOP_NPM_CLI_SHA256,
        "npmPackageSha256": DESKTOP_NPM_PACKAGE_SHA256,
        "npmVersion": "11.19.0",
    }:
        fail("desktop live runtime differs from the reviewed native Node/npm distribution")

    forward = receipt["forward"]
    exact_keys(
        forward,
        {
            "kind", "localPort", "rolloutManifestSha256",
            "sshExecutableSha256", "sshIdentitySha256",
            "sshKnownHostsSha256", "validatorHost", "validatorName",
            "validatorRpcSocket",
        },
        "desktop live validator forward",
    )
    expected_socket = (
        f"/run/arc-v3-rpc-{DESKTOP_VALIDATOR_NAME}-{rollout_sha256[:16]}/rpc.sock"
    )
    if forward != {
        "kind": DESKTOP_FORWARD_KIND,
        "localPort": port,
        "rolloutManifestSha256": rollout_sha256,
        "sshExecutableSha256": DESKTOP_SSH_EXECUTABLE_SHA256,
        "sshIdentitySha256": DESKTOP_SSH_IDENTITY_SHA256,
        "sshKnownHostsSha256": DESKTOP_SSH_KNOWN_HOSTS_SHA256,
        "validatorHost": DESKTOP_VALIDATOR_HOST,
        "validatorName": DESKTOP_VALIDATOR_NAME,
        "validatorRpcSocket": expected_socket,
    }:
        fail("desktop live RPC was not the exact authenticated validator Unix-socket forward")
    inference_path, _ = repository_tool(
        Path("desktop/src/screens/Inference.tsx"), "desktop inference screen"
    )
    inference_text = load_bytes(
        inference_path, "desktop inference screen", maximum=4 * 1024 * 1024
    ).decode("utf-8", errors="strict")
    if (
        "const MAX_RECEIPT_POLLS = 61;" not in inference_text
        or "const MAX_RECEIPT_POLL_MS = 180_000;" not in inference_text
    ):
        fail("protected desktop source does not implement the accepted receipt poll budget")

    direct = receipt["rewardReceipt"]
    exact_keys(direct, set(DESKTOP_DIRECT_RECEIPT_KEYS), "desktop direct reward receipt")
    for field in (
        "tx_hash", "job_id", "worker", "model_id", "input_hash", "output_hash",
        "assignment_epoch", "validator_set_commitment", "transaction_domain",
        "block_hash",
    ):
        canonical_chain_hash(direct[field], f"desktop direct reward receipt.{field}")
    for field in (
        "recovery_epoch", "validator_set_id", "validator_approvals",
        "block_height", "index",
    ):
        exact_nonnegative = direct[field]
        if (
            isinstance(exact_nonnegative, bool)
            or not isinstance(exact_nonnegative, int)
            or exact_nonnegative < 0
        ):
            fail(f"desktop direct reward receipt.{field} must be a non-negative integer")
    if not 5 <= direct["validator_approvals"] <= 6:
        fail("desktop direct reward receipt has an invalid approval count")
    if (
        direct["status"] != "mined_success"
        or direct["tx_type"] != "0x25"
        or direct["tx_hash"] != reward_tx
        or direct["worker"] != worker
        or direct["submitted"] is not True
        or direct["included"] is not True
        or direct["confirmed"] is not True
        or direct["success"] is not True
        or direct["reward_base"] != REWARD_PER_RECEIPT_BASE
        or direct["reward_arc"] != 2.5
        or direct["receipt_url"] != f"/community/reward_receipt/{reward_tx}"
        or direct["evidence_source"]
        != "successful mined CommunityInferenceReward receipt"
    ):
        fail("desktop direct reward receipt is not the exact successful canary")

    expected_canaries: dict[str, tuple[str, str]] = {}
    rows = reward.get("receipts")
    if not isinstance(rows, list):
        fail("sealed reward evidence omits canary receipts")
    for index, row in enumerate(rows):
        if not isinstance(row, dict):
            fail(f"sealed reward receipt {index} must be an object")
        tx = f"0x{chain_hash(row.get('tx_hash'), f'sealed reward receipt {index}.tx_hash')}"
        expected_canaries[tx] = (
            f"0x{chain_hash(row.get('job_id'), f'sealed reward receipt {index}.job_id')}",
            f"0x{chain_hash(row.get('worker'), f'sealed reward receipt {index}.worker')}",
        )
    expected = expected_canaries.get(reward_tx)
    if expected is None or expected != (direct["job_id"], worker):
        fail("desktop live canary is not one exact sealed rollout reward")

    earnings = receipt["earnings"]
    exact_keys(
        earnings,
        {
            "address", "archiveMode", "canaryReceipt",
            "communityRewardsV1ApprovalCollectionReady",
            "communityRewardsV1Enabled", "communityRewardsV1ProtocolActive",
            "confirmedGrossEarningsArc", "confirmedGrossEarningsBase",
            "confirmedReceiptCount", "historyCompleteSinceRecovery",
            "historyDomain", "historyScope", "issuanceReadyForWorker",
            "projectedDailyArc", "projectedDailyUnavailableReason",
            "recoveryEpoch", "rewardPerAttestationArc",
            "rewardPerAttestationBase", "source", "stakeZeroEligible",
            "validatorSetCommitment", "validatorSetId", "workerMinStakeBase",
        },
        "desktop live earnings",
    )
    if not isinstance(earnings["archiveMode"], bool) or not isinstance(
        earnings["historyCompleteSinceRecovery"], bool
    ):
        fail("desktop live earnings archive/history flags must be boolean")
    if (
        earnings["address"] != worker
        or earnings["source"] != DESKTOP_RETAINED_SOURCE
        or earnings["historyDomain"] != DESKTOP_HISTORY_DOMAIN
        or earnings["historyScope"]
        != (DESKTOP_ARCHIVE_SCOPE if earnings["archiveMode"] is True else DESKTOP_RETAINED_SCOPE)
        or earnings["historyCompleteSinceRecovery"] is not earnings["archiveMode"]
        or earnings["communityRewardsV1Enabled"] is not True
        or earnings["communityRewardsV1ProtocolActive"] is not True
        or earnings["communityRewardsV1ApprovalCollectionReady"] is not True
        or earnings["issuanceReadyForWorker"] is not True
        or earnings["stakeZeroEligible"] is not True
        or earnings["workerMinStakeBase"] != 0
        or earnings["rewardPerAttestationBase"] != REWARD_PER_RECEIPT_BASE
        or earnings["rewardPerAttestationArc"] != 2.5
        or earnings["recoveryEpoch"] != direct["recovery_epoch"]
        or earnings["validatorSetId"] != direct["validator_set_id"]
        or earnings["validatorSetCommitment"] != direct["validator_set_commitment"]
    ):
        fail("desktop live earnings readiness/history identity differs from the canary")
    count = earnings["confirmedReceiptCount"]
    gross_base = earnings["confirmedGrossEarningsBase"]
    gross_arc = earnings["confirmedGrossEarningsArc"]
    if (
        isinstance(count, bool)
        or not isinstance(count, int)
        or count <= 0
        or gross_base != count * REWARD_PER_RECEIPT_BASE
        or gross_arc != count * 2.5
    ):
        fail("desktop live earnings totals do not reconcile to exact rewards")
    projected = earnings["projectedDailyArc"]
    reason = earnings["projectedDailyUnavailableReason"]
    projection_available = (
        not isinstance(projected, bool)
        and isinstance(projected, (int, float))
        and projected >= 0
        and reason is None
    )
    projection_unavailable = (
        projected is None and isinstance(reason, str) and bool(reason.strip())
    )
    if not (projection_available or projection_unavailable):
        fail("desktop live earnings projection lacks an exact value/reason XOR")
    earnings_receipt = earnings["canaryReceipt"]
    exact_keys(
        earnings_receipt,
        set(DESKTOP_EARNINGS_RECEIPT_KEYS),
        "desktop live earnings canary receipt",
    )
    for field in DESKTOP_EARNINGS_RECEIPT_KEYS:
        if field in direct and earnings_receipt[field] != direct[field]:
            fail(f"desktop live earnings canary {field} differs from direct receipt")

    block = receipt["blockReceipt"]
    exact_keys(
        block,
        {"blockHash", "blockHeight", "gasUsed", "index", "success", "txHash"},
        "desktop live block receipt",
    )
    if (
        block["txHash"] != reward_tx
        or block["blockHash"] != direct["block_hash"]
        or block["blockHeight"] != direct["block_height"]
        or block["index"] != direct["index"]
        or block["success"] is not True
        or isinstance(block["gasUsed"], bool)
        or not isinstance(block["gasUsed"], int)
        or block["gasUsed"] < 0
    ):
        fail("desktop live block receipt differs from the exact reward inclusion")

    _generator, generator_sha = repository_tool(
        DESKTOP_LIVE_GENERATOR_RELATIVE, "desktop live receipt generator"
    )
    return {
        "appImageVerification": {
            "receiptSha256": appimage_verification["receiptSha256"],
            "stdoutSha256": appimage_verification["stdoutSha256"],
            "verifierPath": appimage_verification["verifierPath"],
            "verifierSha256": appimage_verification["verifierSha256"],
        },
        "generatorPath": DESKTOP_LIVE_GENERATOR_RELATIVE.as_posix(),
        "generatorSha256": generator_sha,
        "macosPackageVerification": {
            "controllerAttemptSha256": macos_evidence["controllerAttempt"]["sha256"],
            "controllerPath": MACOS_PACKAGE_CONTROLLER_RELATIVE.as_posix(),
            "controllerSha256": receipt["packagedNative"]["packageEvidence"]["controllerSha256"],
            "inspectionSha256": macos_evidence["inspection"]["sha256"],
            "nativeAttemptSha256": macos_evidence["nativeAttempt"]["sha256"],
            "nativeInputSha256": macos_evidence["nativeInput"]["sha256"],
            "nativeReceiptSha256": macos_evidence["nativeReceipt"]["sha256"],
            "provenanceSha256": macos_evidence["provenance"]["sha256"],
            "signatureReceiptSha256": macos_evidence["signature"]["sha256"],
            "truthScope": dict(MACOS_PACKAGE_TRUTH_SCOPE),
            "verificationReceiptSha256": macos_evidence["verification"]["sha256"],
        },
        "receipt": receipt,
        "receiptSha256": sha256(receipt_raw),
    }


def validate_workflow(
    value: dict[str, Any], *, path: str, name: str, label: str
) -> int:
    workflow_id = positive_int(value.get("id"), f"{label}.id")
    if value.get("path") != path or value.get("name") != name or value.get("state") != "active":
        fail(f"{label} does not identify the exact active checked-in workflow")
    return workflow_id


def validate_run(
    value: dict[str, Any],
    *,
    workflow_id: int,
    path: str,
    event: str,
    head_branch: str,
    head_sha: str | None,
    label: str,
) -> tuple[int, int, str]:
    run_id = positive_int(value.get("id"), f"{label}.id")
    run_attempt = positive_int(value.get("run_attempt"), f"{label}.run_attempt")
    if value.get("workflow_id") != workflow_id:
        fail(f"{label} belongs to another workflow")
    repository = value.get("head_repository")
    actual_sha = commit(value.get("head_sha"), f"{label}.head_sha")
    if (
        not isinstance(repository, dict)
        or repository.get("full_name") != REPOSITORY
        or value.get("path") != path
        or value.get("event") != event
        or value.get("head_branch") != head_branch
        or value.get("status") != "completed"
        or value.get("conclusion") != "success"
        or (head_sha is not None and actual_sha != head_sha)
    ):
        fail(f"{label} is not the exact successful repository run")
    return run_id, run_attempt, actual_sha


def validate_jobs(
    value: object,
    *,
    expected_names: frozenset[str],
    run_id: int,
    run_attempt: int,
    head_sha: str,
    label: str,
) -> dict[str, int]:
    if isinstance(value, dict):
        exact_keys(value, {"total_count", "jobs"}, label)
        rows = value["jobs"]
        if value["total_count"] != len(expected_names):
            fail(f"{label} total_count differs from the exact job set")
    else:
        rows = value
    if not isinstance(rows, list) or len(rows) != len(expected_names):
        fail(f"{label} does not contain the exact job count")
    identities: dict[str, int] = {}
    job_ids: set[int] = set()
    for index, row in enumerate(rows):
        if not isinstance(row, dict):
            fail(f"{label} job {index} must be an object")
        name = row.get("name")
        if not isinstance(name, str) or name in identities:
            fail(f"{label} contains an invalid or duplicate job name")
        job_id = positive_int(row.get("id"), f"{label} job {name}.id")
        if job_id in job_ids:
            fail(f"{label} contains duplicate job IDs")
        if (
            row.get("run_id") != run_id
            or row.get("run_attempt") != run_attempt
            or row.get("head_sha") != head_sha
            or row.get("status") != "completed"
            or row.get("conclusion") != "success"
        ):
            fail(f"{label} job {name} is not bound to the exact successful attempt")
        identities[name] = job_id
        job_ids.add(job_id)
    if set(identities) != expected_names:
        fail(f"{label} names differ from the exact checked-in job set")
    return dict(sorted(identities.items()))


def validate_release(
    release: dict[str, Any], source_sha: str
) -> tuple[str, dict[str, dict[str, Any]]]:
    require_keys(
        release,
        {
            "id", "tag_name", "target_commitish", "draft", "prerelease",
            "immutable", "author", "assets", "html_url", "published_at",
        },
        "release API response",
    )
    if (
        release["tag_name"] != TAG
        or release["target_commitish"] != source_sha
        or release["draft"] is not False
        or release["prerelease"] is not False
        or release["immutable"] is not True
        or release.get("html_url")
        != f"https://github.com/{REPOSITORY}/releases/tag/{TAG}"
        or not isinstance(release.get("author"), dict)
        or release["author"].get("login") != "github-actions[bot]"
    ):
        fail("release API response does not prove the exact immutable v0.8.0 release")
    positive_int(release["id"], "release.id")
    published_at = timestamp(release["published_at"], "release.published_at")
    assets = release["assets"]
    if not isinstance(assets, list) or len(assets) != len(EXPECTED_RELEASE_ASSETS):
        fail("release does not contain the exact 32-asset contract")
    names: set[str] = set()
    asset_ids: set[int] = set()
    normalized: dict[str, dict[str, Any]] = {}
    total_size = 0
    for index, asset in enumerate(assets):
        if not isinstance(asset, dict):
            fail(f"release asset {index} is not an object")
        name = asset.get("name")
        digest = asset.get("digest")
        asset_id = asset.get("id")
        if (
            not isinstance(name, str)
            or name in names
            or asset_id in asset_ids
            or isinstance(asset_id, bool)
            or not isinstance(asset_id, int)
            or asset_id <= 0
            or asset.get("state") != "uploaded"
            or not isinstance(asset.get("size"), int)
            or isinstance(asset.get("size"), bool)
            or asset["size"] <= 0
            or not isinstance(digest, str)
            or not digest.startswith("sha256:")
            or HASH_RE.fullmatch(digest[7:]) is None
            or not isinstance(asset.get("uploader"), dict)
            or asset["uploader"].get("login") != "github-actions[bot]"
        ):
            fail(f"release asset {index} lacks its immutable uploaded digest contract")
        maximum_size = (
            1024 * 1024
            if name == "arc-recovery-checkpoint-descriptor.json"
            else 4 * 1024 * 1024
            if name.endswith((".sig", ".json"))
            else 2 * 1024 * 1024 * 1024
        )
        if asset["size"] > maximum_size:
            fail(f"release asset {name} exceeds its reviewed size bound")
        if asset.get("browser_download_url") != (
            f"https://github.com/{REPOSITORY}/releases/download/{TAG}/{name}"
        ):
            fail(f"release asset {name} has an unexpected public URL")
        total_size += asset["size"]
        names.add(name)
        asset_ids.add(asset_id)
        normalized[name] = {
            "id": asset_id,
            "sha256": digest[7:],
            "size": asset["size"],
        }
    if names != EXPECTED_RELEASE_ASSETS:
        fail("release asset names differ from the exact v0.8.0 contract")
    if total_size > 12 * 1024 * 1024 * 1024:
        fail("release assets exceed the reviewed aggregate size bound")
    return published_at, dict(sorted(normalized.items()))


def validate_network(
    config: dict[str, Any], source_sha: str, manifest: Mapping[str, Any]
) -> dict[str, Any]:
    exact_keys(
        config,
        {"schema", "state", "network", "checkpoint", "sources", "services", "notices"},
        "frontend config",
    )
    if config["schema"] != NETWORK_SCHEMA or config["state"] != "recovered":
        fail("frontend config is not the recovered production configuration")
    if config["network"] != {"name": "ARC Testnet", "chainId": CHAIN_ID}:
        fail("frontend config does not identify the reviewed ARC production testnet")
    if not isinstance(config["notices"], list) or not config["notices"] or not all(
        isinstance(item, str) and item.strip() for item in config["notices"]
    ):
        fail("frontend config notices must be nonempty strings")
    checkpoint = config["checkpoint"]
    if not isinstance(checkpoint, dict):
        fail("frontend checkpoint is missing")
    exact_keys(
        checkpoint,
        {
            "height", "recoveryHeight", "legacyPublicMaxHeight", "blockHash",
            "stateRoot", "manifestHash", "boundaryBlockHash", "boundaryStateRoot",
            "recoveryEpoch", "validatorSetId", "protocolVersion", "recoveryDomain",
            "checkpointFileSha256", "checkpointPayloadHash", "legacySourceId",
            "v3SourceId",
        },
        "frontend checkpoint",
    )
    height = positive_int(checkpoint["height"], "checkpoint.height")
    recovery_height = positive_int(checkpoint["recoveryHeight"], "checkpoint.recoveryHeight")
    legacy_max = positive_int(checkpoint["legacyPublicMaxHeight"], "checkpoint.legacyPublicMaxHeight")
    if height < 137_145 or recovery_height != height + 1:
        fail(
            "frontend checkpoint does not preserve capture-derived H/H+1 "
            "above trusted anchor 137145"
        )
    chain = manifest.get("chain")
    manifest_archive = manifest.get("archive")
    artifacts = manifest.get("artifacts")
    rollout_id = manifest.get("rollout_id")
    if (
        not isinstance(chain, dict)
        or not isinstance(manifest_archive, dict)
        or not isinstance(artifacts, dict)
        or not isinstance(rollout_id, str)
        or not rollout_id
    ):
        fail("rollout manifest omits its production chain/archive identity")
    canonical_source = chain.get("canonical_source")
    canonical_node = (
        canonical_source.get("node") if isinstance(canonical_source, dict) else None
    )
    if canonical_node not in {name for name, _host in PRODUCTION_FLEET}:
        fail("rollout manifest omits its captured canonical source node")
    manifest_epoch = positive_int(
        chain.get("recovery_epoch"), "manifest recovery epoch"
    )
    manifest_validator_set = positive_int(
        chain.get("validator_set_id"), "manifest validator set ID"
    )
    observed_cutoff = positive_int(
        chain.get("legacy_observed_cutoff_height"),
        "manifest legacy observed cutoff height",
    )
    continuity_margin = positive_int(
        chain.get("legacy_continuity_safety_margin"),
        "manifest legacy continuity safety margin",
    )
    protocol_version = chain.get("protocol_version")
    if (
        not isinstance(protocol_version, str)
        or PROTOCOL_VERSION_RE.fullmatch(protocol_version) is None
        or not isinstance(checkpoint["protocolVersion"], str)
        or PROTOCOL_VERSION_RE.fullmatch(checkpoint["protocolVersion"]) is None
    ):
        fail("frontend checkpoint is not protocol v3")
    if continuity_margin != LEGACY_CONTINUITY_SAFETY_MARGIN:
        fail("rollout manifest continuity safety margin is not exactly 128")
    if observed_cutoff < height or legacy_max != observed_cutoff + continuity_margin:
        fail("rollout manifest does not prove the exact F=C+128 reopening floor")

    checkpoint_artifact = artifacts.get("checkpoint")
    interlock_artifact = artifacts.get("legacy_late_fork_interlock_tool")
    if not isinstance(checkpoint_artifact, dict) or not isinstance(
        interlock_artifact, dict
    ):
        fail("rollout manifest omits checkpoint or interlock artifacts")
    manifest_checkpoint_sha = hash_value(
        checkpoint_artifact.get("sha256"), "manifest checkpoint artifact SHA-256"
    )
    checkpoint_payload_hash = hash_value(
        checkpoint.get("checkpointPayloadHash"),
        "frontend canonical checkpoint payload hash",
    )
    manifest_interlock_tool_sha = hash_value(
        interlock_artifact.get("sha256"), "manifest interlock tool SHA-256"
    )
    expected_capture_id = hash_value(
        manifest_archive.get("capture_id"), "manifest archive capture ID"
    )
    expected_archive_roots = {
        "rolloutManifestSha256": hash_value(
            manifest_archive.get("prearchive_rollout_sha256"),
            "manifest prearchive rollout SHA-256",
        ),
        "archiveManifestSha256": hash_value(
            manifest_archive.get("archive_manifest_sha256"),
            "manifest archive-manifest SHA-256",
        ),
        "completeSha256": hash_value(
            manifest_archive.get("complete_sha256"),
            "manifest archive COMPLETE SHA-256",
        ),
    }
    if any(value == "0" * 64 for value in expected_archive_roots.values()):
        fail("public truth requires a fully finalized archive manifest")

    source_id = f"v3-{canonical_node}"
    if (
        chain.get("chain_id") != CHAIN_ID
        or checkpoint["recoveryEpoch"] != manifest_epoch
        or checkpoint["validatorSetId"] != manifest_validator_set
        or manifest_epoch != RECOVERY_EPOCH
        or manifest_validator_set != VALIDATOR_SET_ID
        or checkpoint["protocolVersion"] != protocol_version
        or checkpoint.get("checkpointFileSha256") != manifest_checkpoint_sha
        or checkpoint["legacySourceId"] != source_id
        or checkpoint["v3SourceId"] != source_id
        or height != chain.get("source_height")
        or recovery_height != chain.get("transition_height")
        or legacy_max != chain.get("legacy_public_max_height")
        or chain_hash(checkpoint["blockHash"], "checkpoint.blockHash")
        != chain_hash(chain.get("source_block_hash"), "manifest source block hash")
        or chain_hash(checkpoint["stateRoot"], "checkpoint.stateRoot")
        != chain_hash(chain.get("source_state_root"), "manifest source state root")
        or chain_hash(checkpoint["manifestHash"], "checkpoint.manifestHash")
        != chain_hash(
            chain.get("approved_checkpoint_manifest_hash"),
            "manifest approved checkpoint manifest hash",
        )
        or chain_hash(
            checkpoint["boundaryBlockHash"], "checkpoint.boundaryBlockHash"
        )
        != chain_hash(
            chain.get("transition_block_hash"), "manifest transition block hash"
        )
        or chain_hash(
            checkpoint["boundaryStateRoot"], "checkpoint.boundaryStateRoot"
        )
        != chain_hash(chain.get("full_state_root"), "manifest full state root")
        or chain_hash(checkpoint["recoveryDomain"], "checkpoint.recoveryDomain")
        != chain_hash(chain.get("recovery_domain"), "manifest recovery domain")
    ):
        fail("frontend checkpoint differs from the reviewed recovery identity")
    sources = config["sources"]
    # The former `len(sources) != 12` invariant incorrectly presented every
    # stopped capture as a noncanonical fork. The producer emits six v3 rows
    # plus only the captures actually classified valid_noncanonical_fork.
    if not isinstance(sources, list) or not 6 <= len(sources) <= 12:
        fail("frontend sources must contain six validators and only captured legacy forks")
    if any(not isinstance(row, dict) for row in sources):
        fail("frontend source rows must be objects")
    identifiers = [row.get("id") for row in sources]
    endpoints = [row.get("baseUrl") for row in sources]
    if not all(isinstance(value, str) for value in identifiers + endpoints):
        fail("frontend source identities and endpoints must be strings")
    if len(set(identifiers)) != len(identifiers) or len(set(endpoints)) != len(endpoints):
        fail("frontend source identities and endpoints must be unique")
    v3 = [row for row in sources if isinstance(row, dict) and row.get("kind") == "v3"]
    forks = [row for row in sources if isinstance(row, dict) and row.get("kind") == "legacy-fork"]
    expected_v3 = [
        (f"v3-{name}", f"https://{host}") for name, host in PRODUCTION_FLEET
    ]
    actual_v3 = [(row.get("id"), row.get("baseUrl")) for row in v3]
    if (
        actual_v3 != expected_v3
        or sources[: len(PRODUCTION_FLEET)] != v3
        or len(v3) + len(forks) != len(sources)
    ):
        fail("frontend config does not expose the exact six protocol-v3 validators")
    for (name, _host), row in zip(PRODUCTION_FLEET, v3, strict=True):
        exact_keys(
            row,
            {"id", "name", "region", "kind", "baseUrl", "enabled", "replicaGroup"},
            f"frontend v3 source {name}",
        )
        replica_group = row["replicaGroup"]
        if (
            row["name"] != f"ARC v3 {name.upper()}"
            or row["region"] != name.upper()
            or row["enabled"] is not True
            or replica_group != rollout_id
        ):
            fail(f"frontend v3 source {name} differs from its rollout identity")
    fleet_hosts = dict(PRODUCTION_FLEET)
    fleet_order = {name: index for index, (name, _host) in enumerate(PRODUCTION_FLEET)}
    capture_ids: set[str] = set()
    fork_nodes: list[str] = []
    previous_fleet_index = -1
    # Selection is only through H. The selected source node may itself retain
    # a divergent post-H legacy tail, so archive classification—not node name—
    # decides whether that capture receives a noncanonical history view.
    for index, row in enumerate(forks):
        exact_keys(
            row,
            {
                "id", "name", "region", "kind", "baseUrl", "enabled",
                "replicaGroup", "description", "archive",
            },
            f"frontend legacy source {index}",
        )
        archive_view = row.get("archive")
        if not isinstance(archive_view, dict):
            fail("a legacy history lacks its archive commitment")
        name = archive_view.get("node")
        if not isinstance(name, str) or name not in fleet_hosts:
            fail("a legacy history does not name a production capture")
        exact_keys(
            archive_view,
            {
                "schema", "readOnly", "classification", "captureId", "node",
                "rolloutManifestSha256", "archiveManifestSha256", "completeSha256",
                "bundleSha256", "inventorySha256", "bindingIndexSha256",
                "bindingSha256", "checkpointSha256", "checkpointManifestHash",
                "checkpointPayloadHash", "canonicalCheckpointHeight", "sourceHeight",
                "sourceBlockHash", "sourceStateRoot", "provenancePath",
            },
            f"frontend legacy source {name} archive",
        )
        fleet_index = fleet_order[name]
        if (
            fleet_index <= previous_fleet_index
            or row.get("id") != f"legacy-fork-{name}"
            or row.get("baseUrl") != f"https://{fleet_hosts[name]}/legacy/{name}"
            or row.get("enabled") is not True
            or row.get("name") != f"Preserved legacy fork · {name.upper()}"
            or row.get("region") != name.upper()
            or row.get("description")
            != "Explicit immutable historical fork; diagnostic only and never canonical."
            or archive_view.get("readOnly") is not True
            or archive_view.get("schema") != "arc.legacy-archive.source.v1"
            or archive_view.get("classification") != "valid_noncanonical_fork"
            or archive_view.get("node") != name
            or archive_view.get("provenancePath") != "/provenance"
            or archive_view.get("canonicalCheckpointHeight") != height
            or isinstance(archive_view.get("sourceHeight"), bool)
            or not isinstance(archive_view.get("sourceHeight"), int)
            or archive_view["sourceHeight"] < 0
        ):
            fail("a legacy history is not explicit, read-only, and noncanonical")
        previous_fleet_index = fleet_index
        fork_nodes.append(name)
        capture_id = hash_value(
            archive_view["captureId"], f"legacy source {name} captureId"
        )
        if (
            capture_id != expected_capture_id
            or row.get("replicaGroup") != f"legacy-capture-{expected_capture_id}"
        ):
            fail(f"legacy source {name} is not bound to its capture identity")
        capture_ids.add(capture_id)
        for field in (
            "rolloutManifestSha256", "archiveManifestSha256", "completeSha256",
            "bundleSha256", "inventorySha256", "bindingIndexSha256", "bindingSha256",
            "checkpointSha256", "checkpointManifestHash", "checkpointPayloadHash",
            "sourceBlockHash", "sourceStateRoot",
        ):
            chain_hash(archive_view[field], f"legacy source {name} archive.{field}")
        for field, expected in expected_archive_roots.items():
            if chain_hash(
                archive_view[field], f"legacy source {name} archive.{field}"
            ) != expected:
                fail(f"legacy source {name} {field} differs from the sealed manifest")
    if capture_ids and capture_ids != {expected_capture_id}:
        fail("frontend legacy forks do not share one sealed capture")
    services = config["services"]
    if not isinstance(services, dict):
        fail("frontend services must be an object")
    exact_keys(services, {"maintenanceInterlock"}, "frontend services")
    interlock = services.get("maintenanceInterlock")
    if not isinstance(interlock, dict) or interlock.get("sourceMainCommit") != source_sha:
        fail("frontend maintenance interlock is not bound to the release source")
    exact_keys(
        interlock,
        {
            "schema", "path", "sourceMainCommit", "observedCutoffHeight",
            "sourceSetSha256", "boundarySha256", "toolSha256",
            "requiredHealthyReplicas", "maxStalenessSeconds",
        },
        "frontend maintenance interlock",
    )
    expected_source_set_sha = hash_value(
        chain.get("legacy_late_fork_source_set_sha256"),
        "manifest late-fork source-set SHA-256",
    )
    expected_boundary_sha = hash_value(
        chain.get("legacy_maintenance_boundary_sha256"),
        "manifest maintenance-boundary SHA-256",
    )
    if (
        interlock.get("schema") != "arc.frontend.maintenance-interlock.v1"
        or interlock.get("path") != "/maintenance/status"
        or interlock.get("requiredHealthyReplicas") != 6
        or interlock.get("maxStalenessSeconds") != 90
        or isinstance(interlock.get("observedCutoffHeight"), bool)
        or not isinstance(interlock.get("observedCutoffHeight"), int)
        or interlock["observedCutoffHeight"] != observed_cutoff
        or legacy_max != interlock["observedCutoffHeight"] + continuity_margin
        or interlock.get("sourceSetSha256") != expected_source_set_sha
        or interlock.get("boundarySha256") != expected_boundary_sha
        or interlock.get("toolSha256") != manifest_interlock_tool_sha
    ):
        fail("frontend live gate differs from the sealed all-six interlock")

    # Per-fork checkpoint, binding, inventory, and source roots are
    # intentionally candidate-specific: a valid noncanonical capture is
    # expected to differ from the selected checkpoint. They are authenticated
    # by the exact recovery verifier's deterministic frontend-config
    # projection below, which derives them from the capture-bound immutable
    # archive rather than trusting the supplied frontend JSON.
    validated_checkpoint = dict(checkpoint)
    validated_checkpoint.update(
        {
            "checkpointFileSha256": manifest_checkpoint_sha,
            "checkpointPayloadHash": checkpoint_payload_hash,
            "legacyContinuitySafetyMargin": continuity_margin,
            "legacyObservedCutoffHeight": observed_cutoff,
            "legacyForkCount": len(fork_nodes),
            "legacyForkNodes": fork_nodes,
            "rolloutId": rollout_id,
        }
    )
    return validated_checkpoint


def validate_reward(
    reward: dict[str, Any], manifest: dict[str, Any], source_sha: str
) -> tuple[str, int, int]:
    exact_keys(
        reward,
        {"schema", "rollout_sha256", "earnings_baseline", "receipts", "canonical_cutoff"},
        "reward evidence",
    )
    if reward["schema"] != REWARD_SCHEMA:
        fail("reward evidence schema is unsupported")
    rollout_sha = hash_value(reward["rollout_sha256"], "reward rollout SHA-256")
    if sha256(canonical_json(manifest)) != rollout_sha:
        fail("reward evidence is not bound to the supplied rollout manifest")
    if manifest.get("mode") != "production":
        fail("rollout manifest is not production mode")
    provenance = manifest.get("provenance")
    if not isinstance(provenance, dict) or provenance.get("source_main_commit") != source_sha:
        fail("rollout manifest is not bound to the release source")
    checks = manifest.get("checks")
    policy = checks.get("reward") if isinstance(checks, dict) else None
    if not isinstance(policy, dict) or policy.get("mode") != "receipt":
        fail("rollout manifest does not require receipt-mode rewards")
    reward_base = positive_int(policy.get("expected_reward_base"), "expected reward base")
    if reward_base != REWARD_PER_RECEIPT_BASE:
        fail("rollout manifest reward differs from the reviewed 2.5 ARC contract")
    expected_worker = policy.get("expected_worker")
    if not isinstance(expected_worker, str) or CHAIN_HASH_RE.fullmatch(expected_worker) is None:
        fail("rollout manifest has no exact expected canary worker")
    expected_worker = "0x" + expected_worker.removeprefix("0x")
    receipts = reward["receipts"]
    if not isinstance(receipts, list) or len(receipts) != 2:
        fail("reward evidence must contain exactly two canary receipts")
    tx_hashes: set[str] = set()
    job_ids: set[str] = set()
    for index, row in enumerate(receipts):
        if not isinstance(row, dict) or set(row) != {"tx_hash", "job_id", "worker"}:
            fail(f"reward receipt {index} has an unsupported shape")
        worker = "0x" + chain_hash(row["worker"], f"reward receipt {index} worker")
        tx_hashes.add(chain_hash(row["tx_hash"], f"reward receipt {index} tx_hash"))
        job_ids.add(chain_hash(row["job_id"], f"reward receipt {index} job_id"))
        if worker != expected_worker:
            fail("reward receipt belongs to a worker other than the accepted macOS canary")
    if len(tx_hashes) != 2 or len(job_ids) != 2:
        fail("reward canary receipt transaction and job identities must be distinct")
    baseline = reward["earnings_baseline"]
    if not isinstance(baseline, dict):
        fail("reward evidence earnings baseline must be an object")
    exact_keys(
        baseline,
        {"worker", "confirmed_receipt_count", "confirmed_gross_earnings_base", "confirmed_receipts"},
        "reward evidence earnings baseline",
    )
    if "0x" + chain_hash(baseline["worker"], "reward baseline worker") != expected_worker:
        fail("reward evidence baseline belongs to another worker")
    baseline_count = baseline["confirmed_receipt_count"]
    baseline_gross = baseline["confirmed_gross_earnings_base"]
    if (
        isinstance(baseline_count, bool)
        or not isinstance(baseline_count, int)
        or baseline_count < 0
        or isinstance(baseline_gross, bool)
        or not isinstance(baseline_gross, int)
        or baseline_gross < 0
        or not isinstance(baseline["confirmed_receipts"], list)
        or len(baseline["confirmed_receipts"]) != baseline_count
    ):
        fail("reward evidence baseline counters are invalid")
    cutoff = reward["canonical_cutoff"]
    if not isinstance(cutoff, dict):
        fail("reward evidence canonical cutoff must be an object")
    exact_keys(cutoff, {"block_height", "block_hash", "index"}, "reward evidence canonical cutoff")
    positive_int(cutoff["block_height"], "reward evidence canonical cutoff.block_height")
    if isinstance(cutoff["index"], bool) or not isinstance(cutoff["index"], int) or cutoff["index"] < 0:
        fail("reward evidence canonical cutoff.index must be a non-negative integer")
    chain_hash(cutoff["block_hash"], "reward evidence canonical cutoff.block_hash")
    return expected_worker, reward_base, len(receipts)


def hash_regular_file(
    path: Path, label: str, maximum: int, *, allow_empty: bool = False
) -> tuple[int, str]:
    descriptor = -1
    try:
        flags = os.O_RDONLY
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        descriptor = os.open(path, flags)
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode):
            fail(f"{label} must be a non-symlink regular file")
        if before.st_size > maximum or (before.st_size == 0 and not allow_empty):
            fail(f"{label} has an unsupported size")
        digest = hashlib.sha256()
        total = 0
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            total += len(chunk)
            if total > maximum:
                fail(f"{label} exceeds its size limit")
            digest.update(chunk)
        after = os.fstat(descriptor)
    except OSError as error:
        fail(f"cannot read {label}: {error}")
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    if total != before.st_size or (
        before.st_dev,
        before.st_ino,
        before.st_size,
        before.st_mtime_ns,
        before.st_ctime_ns,
    ) != (
        after.st_dev,
        after.st_ino,
        after.st_size,
        after.st_mtime_ns,
        after.st_ctime_ns,
    ):
        fail(f"{label} changed while it was read")
    return total, digest.hexdigest()


def parse_site_sha256sums(raw: bytes) -> dict[str, str]:
    try:
        text = raw.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        fail(f"deployed SHA256SUMS is not UTF-8: {error}")
    if not text.endswith("\n"):
        fail("deployed SHA256SUMS must end with one complete record")
    result: dict[str, str] = {}
    for index, line in enumerate(text.splitlines(), 1):
        match = re.fullmatch(r"([0-9a-f]{64})  (\./[^\r\n]+)", line)
        if match is None:
            fail(f"deployed SHA256SUMS line {index} is malformed")
        digest, name = match.groups()
        pure = Path(name.removeprefix("./"))
        if (
            pure.is_absolute()
            or not pure.parts
            or any(part in {"", ".", ".."} for part in pure.parts)
            or "\\" in name
            or name in result
        ):
            fail("deployed SHA256SUMS contains an unsafe or duplicate path")
        result[name] = digest
    return result


def validate_pages(
    args: argparse.Namespace, config_raw: bytes
) -> tuple[dict[str, Any], dict[str, str]]:
    workflow, workflow_raw = load_json(args.pages_workflow, "Pages workflow", canonical=False)
    workflow_id = validate_workflow(
        workflow,
        path=PAGES_WORKFLOW_PATH,
        name="Deploy ARC public console",
        label="Pages workflow",
    )
    run, run_raw = load_json(args.pages_run, "Pages run", canonical=False)
    run_id, run_attempt, frontend_sha = validate_run(
        run,
        workflow_id=workflow_id,
        path=PAGES_WORKFLOW_PATH,
        event="push",
        head_branch="main",
        head_sha=None,
        label="Pages run",
    )
    jobs_value, jobs_raw = load_json_value(args.pages_jobs, "Pages attempt jobs")
    jobs = validate_jobs(
        jobs_value,
        expected_names=PAGES_JOB_NAMES,
        run_id=run_id,
        run_attempt=run_attempt,
        head_sha=frontend_sha,
        label="Pages attempt jobs",
    )
    pages_api, pages_api_raw = load_json(args.pages_api, "Pages API document", canonical=False)
    if (
        pages_api.get("build_type") != "workflow"
        or pages_api.get("html_url") != PUBLIC_CONSOLE
    ):
        fail("Pages API document does not identify the exact workflow-backed public console")

    deployments, deployments_raw = load_json_value(args.pages_deployments, "Pages deployments")
    if not isinstance(deployments, list) or len(deployments) != 1:
        fail("Pages deployment evidence must contain exactly one matching deployment")
    deployment = deployments[0]
    if not isinstance(deployment, dict):
        fail("Pages deployment must be an object")
    deployment_id = positive_int(deployment.get("id"), "Pages deployment.id")
    if (
        deployment.get("sha") != frontend_sha
        or deployment.get("ref") != "main"
        or deployment.get("environment") != "github-pages"
        or deployment.get("task") != "deploy"
    ):
        fail("Pages deployment is not bound to the accepted config commit")

    statuses, statuses_raw = load_json_value(args.pages_statuses, "Pages deployment statuses")
    if not isinstance(statuses, list) or not statuses:
        fail("Pages deployment has no status evidence")
    status_ids: set[int] = set()
    successful: list[dict[str, Any]] = []
    for index, row in enumerate(statuses):
        if not isinstance(row, dict):
            fail(f"Pages deployment status {index} must be an object")
        status_id = positive_int(row.get("id"), f"Pages deployment status {index}.id")
        if status_id in status_ids:
            fail("Pages deployment statuses contain duplicate IDs")
        status_ids.add(status_id)
        if row.get("state") == "success":
            successful.append(row)
    if (
        len(successful) != 1
        or statuses[0] is not successful[0]
        or successful[0].get("environment") != "github-pages"
        or str(successful[0].get("environment_url", "")).rstrip("/")
        != PUBLIC_CONSOLE.rstrip("/")
    ):
        fail("latest exact Pages deployment status is not the unique public success")

    deployed_commit_raw = load_bytes(args.deployed_commit, "deployed commit", maximum=1024)
    if deployed_commit_raw != f"{frontend_sha}\n".encode("ascii"):
        fail("deployed-commit.txt does not contain the exact Pages commit")
    deployed_sums_raw = load_bytes(
        args.deployed_sha256sums, "deployed SHA256SUMS", maximum=1024 * 1024
    )
    sums = parse_site_sha256sums(deployed_sums_raw)
    expected_cdn = {
        "./shared/frontend/arc-network.json": sha256(config_raw),
        "./deployed-commit.txt": sha256(deployed_commit_raw),
    }
    for name, digest in expected_cdn.items():
        if sums.get(name) != digest:
            fail(f"deployed SHA256SUMS does not bind the CDN bytes for {name}")
    return (
        {
            "acceptedConfigCommit": frontend_sha,
            "deploymentId": deployment_id,
            "jobIds": jobs,
            "runAttempt": run_attempt,
            "runId": run_id,
            "statusId": successful[0]["id"],
            "url": PUBLIC_CONSOLE,
            "workflowId": workflow_id,
        },
        {
            "configSha256": sha256(config_raw),
            "deployedCommitSha256": sha256(deployed_commit_raw),
            "deploymentsSha256": sha256(deployments_raw),
            "jobsSha256": sha256(jobs_raw),
            "pagesApiSha256": sha256(pages_api_raw),
            "runSha256": sha256(run_raw),
            "sha256sumsSha256": sha256(deployed_sums_raw),
            "statusesSha256": sha256(statuses_raw),
            "workflowSha256": sha256(workflow_raw),
        },
    )


def extract_published_acceptance(archive: Path, target: Path) -> tuple[int, str]:
    archive_size, archive_sha = hash_regular_file(
        archive, "published-acceptance Actions ZIP", MAX_PUBLISHED_ZIP_BYTES
    )
    try:
        with zipfile.ZipFile(archive, "r") as handle:
            entries = handle.infolist()
            names = [entry.filename for entry in entries]
            if len(names) != len(set(names)) or set(names) != EXPECTED_PUBLISHED_ZIP_FILES:
                fail("published-acceptance ZIP does not contain the exact canonical file set")
            expanded = 0
            for entry in entries:
                pure = Path(entry.filename)
                mode_type = (entry.external_attr >> 16) & 0o170000
                if (
                    pure.is_absolute()
                    or any(part in {"", ".", ".."} for part in pure.parts)
                    or "\\" in entry.filename
                    or entry.is_dir()
                    or entry.flag_bits & 0x1
                    or mode_type not in (0, 0o100000)
                    or (entry.file_size == 0 and entry.filename in PUBLISHED_TOP_LEVEL_JSON)
                    or entry.file_size > MAX_PUBLISHED_MEMBER_BYTES
                ):
                    fail("published-acceptance ZIP contains an unsafe member")
                expanded += entry.file_size
                if expanded > MAX_PUBLISHED_EXPANDED_BYTES:
                    fail("published-acceptance ZIP expands beyond its reviewed bound")
                destination = target.joinpath(*pure.parts)
                destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                with handle.open(entry, "r") as source, destination.open("xb") as output:
                    copied = 0
                    while True:
                        chunk = source.read(1024 * 1024)
                        if not chunk:
                            break
                        copied += len(chunk)
                        if copied > entry.file_size:
                            fail("published-acceptance ZIP member exceeds its declared size")
                        output.write(chunk)
                    output.flush()
                    os.fsync(output.fileno())
                if copied != entry.file_size:
                    fail("published-acceptance ZIP member is truncated")
                os.chmod(destination, 0o400)
    except (zipfile.BadZipFile, RuntimeError, OSError) as error:
        fail(f"cannot extract published-acceptance ZIP: {error}")
    return archive_size, archive_sha


def validate_artifact_sha256sums(root: Path) -> dict[str, str]:
    raw = load_bytes(root / PUBLISHED_ACCEPTANCE_SUMS, "published-acceptance SHA256SUMS")
    try:
        text = raw.decode("ascii", errors="strict")
    except UnicodeDecodeError as error:
        fail(f"published-acceptance SHA256SUMS is not ASCII: {error}")
    if not text.endswith("\n"):
        fail("published-acceptance SHA256SUMS is truncated")
    hashes: dict[str, str] = {}
    for line in text.splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  (\./[^\r\n]+)", line)
        if match is None:
            fail("published-acceptance SHA256SUMS contains a malformed record")
        digest, relative = match.groups()
        name = relative[2:]
        if name in hashes or name not in EXPECTED_PUBLISHED_ZIP_FILES:
            fail("published-acceptance SHA256SUMS contains an unknown or duplicate path")
        hashes[name] = digest
    expected = set(EXPECTED_PUBLISHED_ZIP_FILES) - {PUBLISHED_ACCEPTANCE_SUMS}
    if set(hashes) != expected:
        fail("published-acceptance SHA256SUMS does not cover every canonical member")
    for name, expected_sha in hashes.items():
        _size, actual_sha = hash_regular_file(
            root / name,
            f"published member {name}",
            MAX_PUBLISHED_MEMBER_BYTES,
            allow_empty=True,
        )
        if actual_sha != expected_sha:
            fail(f"published-acceptance member {name} differs from SHA256SUMS")
    return dict(sorted(hashes.items()))


def run_checked_helper(command: Sequence[str], label: str) -> tuple[str, bytes]:
    helper, helper_sha = repository_tool(PUBLISHED_HELPER_RELATIVE, "published acceptance helper")
    result = subprocess.run(
        [sys.executable, "-B", "-I", str(helper), *command],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
        timeout=180,
    )
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", errors="replace").strip()[-1000:]
        fail(f"{label} failed closed: {detail or f'exit {result.returncode}'}")
    _helper_after, helper_after_sha = repository_tool(
        PUBLISHED_HELPER_RELATIVE, "published acceptance helper"
    )
    if helper_after_sha != helper_sha:
        fail("published acceptance helper changed while it was invoked")
    return helper_sha, result.stdout


def validate_published_acceptance(
    args: argparse.Namespace, temporary_root: Path
) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
    workflow, workflow_raw = load_json(
        args.published_workflow, "published-acceptance workflow", canonical=False
    )
    workflow_id = validate_workflow(
        workflow,
        path=PUBLISHED_WORKFLOW_PATH,
        name="Published artifact acceptance",
        label="published-acceptance workflow",
    )
    run, run_raw = load_json(args.published_run, "published-acceptance run", canonical=False)
    run_id, run_attempt, source_sha = validate_run(
        run,
        workflow_id=workflow_id,
        path=PUBLISHED_WORKFLOW_PATH,
        event="workflow_dispatch",
        head_branch=TAG,
        head_sha=None,
        label="published-acceptance run",
    )
    jobs_value, jobs_raw = load_json_value(
        args.published_jobs, "published-acceptance attempt jobs"
    )
    job_ids = validate_jobs(
        jobs_value,
        expected_names=PUBLISHED_JOB_NAMES,
        run_id=run_id,
        run_attempt=run_attempt,
        head_sha=source_sha,
        label="published-acceptance attempt jobs",
    )
    metadata, metadata_raw = load_json(
        args.published_artifact_metadata,
        "published-acceptance artifact metadata",
        canonical=False,
    )
    artifact_id = positive_int(metadata.get("id"), "published-acceptance artifact.id")
    artifact_size = positive_int(
        metadata.get("size_in_bytes"), "published-acceptance artifact.size_in_bytes"
    )
    artifact_digest = metadata.get("digest")
    artifact_name = (
        f"arc-published-artifact-acceptance-{TAG}-{source_sha}-{run_id}-"
        f"attempt-{run_attempt}"
    )
    workflow_run = metadata.get("workflow_run")
    if (
        artifact_size > MAX_PUBLISHED_ZIP_BYTES
        or metadata.get("name") != artifact_name
        or metadata.get("expired") is not False
        or not isinstance(artifact_digest, str)
        or re.fullmatch(r"sha256:[0-9a-f]{64}", artifact_digest) is None
        or not isinstance(workflow_run, dict)
        or workflow_run.get("id") != run_id
        or workflow_run.get("head_sha") != source_sha
    ):
        fail("published-acceptance artifact metadata is not bound to the exact run")

    extracted = temporary_root / "published-acceptance"
    extracted.mkdir(mode=0o700)
    zip_size, zip_sha = extract_published_acceptance(args.published_artifact_zip, extracted)
    if artifact_size != zip_size or artifact_digest != f"sha256:{zip_sha}":
        fail("published-acceptance ZIP bytes differ from artifact metadata")
    member_hashes = validate_artifact_sha256sums(extracted)
    for name in PUBLISHED_TOP_LEVEL_JSON:
        load_json(extracted / name, f"canonical published member {name}", canonical=True)

    receipt, receipt_raw = load_json(
        extracted / PUBLISHED_ACCEPTANCE_RECEIPT,
        "canonical published-acceptance receipt",
        canonical=True,
    )
    if (
        receipt.get("schema") != "arc.published-artifact-acceptance.v1"
        or receipt.get("repository") != REPOSITORY
        or receipt.get("tag") != TAG
        or receipt.get("commit") != source_sha
        or receipt.get("acceptance_run_id") != run_id
        or receipt.get("acceptance_run_attempt") != run_attempt
        or receipt.get("verified_platforms")
        != ["linux-x86_64", "macos-arm64", "macos-x86_64", "windows-x86_64"]
        or receipt.get("evidence_file_count") != len(PUBLISHED_EVIDENCE_FILES)
    ):
        fail("canonical published-acceptance receipt has the wrong exact identity")
    component_hashes = receipt.get("component_receipt_sha256")
    if not isinstance(component_hashes, dict) or set(component_hashes) != {
        "linux-x86_64", "macos-arm64", "macos-x86_64", "windows-x86_64"
    }:
        fail("canonical published-acceptance receipt has an invalid component hash set")
    for platform, digest in component_hashes.items():
        if hash_value(digest, f"published component {platform} hash") != member_hashes[f"{platform}.json"]:
            fail(f"published component {platform} hash is not bound to the ZIP member")
    if receipt.get("evidence_manifest_sha256") != member_hashes[PUBLISHED_EVIDENCE_MANIFEST]:
        fail("published evidence manifest hash is not bound to the ZIP member")

    components = temporary_root / "components"
    components.mkdir(mode=0o700)
    for platform in component_hashes:
        os.link(extracted / f"{platform}.json", components / f"{platform}.json")
    rebuilt = temporary_root / "rebuilt-published-acceptance.json"
    helper_sha, _helper_stdout = run_checked_helper(
        (
            "aggregate",
            "--binding", str(extracted / "release-binding.json"),
            "--component-artifacts", str(extracted / "component-artifacts.json"),
            "--components", str(components),
            "--evidence-manifest", str(extracted / PUBLISHED_EVIDENCE_MANIFEST),
            "--evidence-root", str(extracted / "evidence"),
            "--acceptance-run-id", str(run_id),
            "--acceptance-run-attempt", str(run_attempt),
            "--output", str(rebuilt),
        ),
        "exact published-acceptance helper rebuild",
    )
    rebuilt_raw = load_bytes(rebuilt, "rebuilt published-acceptance receipt")
    if rebuilt_raw != receipt_raw:
        fail("published-acceptance canonical receipt differs from the exact helper rebuild")

    binding, _ = load_json(
        extracted / "release-binding.json", "published release binding", canonical=True
    )
    linux, _ = load_json(
        extracted / "linux-x86_64.json", "published Linux component", canonical=True
    )
    linux_assets = linux.get("assets")
    if not isinstance(linux_assets, dict) or "install.sh" not in linux_assets:
        fail("published Linux component does not contain the validated installer")
    installer = linux_assets["install.sh"]
    if not isinstance(installer, dict):
        fail("published Linux component installer identity must be an object")
    installer_sha = hash_value(installer.get("sha256"), "published Linux installer SHA-256")
    positive_int(installer.get("id"), "published Linux installer asset ID")
    positive_int(installer.get("size"), "published Linux installer size")
    return (
        {
            "artifactDigest": artifact_digest,
            "artifactId": artifact_id,
            "artifactName": artifact_name,
            "canonicalReceiptSha256": sha256(receipt_raw),
            "componentReceiptSha256": dict(sorted(component_hashes.items())),
            "helperPath": PUBLISHED_HELPER_RELATIVE.as_posix(),
            "helperSha256": helper_sha,
            "jobIds": job_ids,
            "releaseId": positive_int(
                receipt.get("release_id"), "canonical published receipt release ID"
            ),
            "releaseRunAttempt": positive_int(
                receipt.get("release_run_attempt"),
                "canonical published receipt release run attempt",
            ),
            "releaseRunId": positive_int(
                receipt.get("release_run_id"),
                "canonical published receipt release run ID",
            ),
            "runAttempt": run_attempt,
            "runId": run_id,
            "workflowId": workflow_id,
        },
        {
            "artifactMetadataSha256": sha256(metadata_raw),
            "artifactZipSha256": zip_sha,
            "jobsSha256": sha256(jobs_raw),
            "runSha256": sha256(run_raw),
            "workflowSha256": sha256(workflow_raw),
        },
        {
            "binding": binding,
            "installerSha256": installer_sha,
            "linuxComponent": linux,
            "sourceSha": source_sha,
        },
    )


def run_recovery_verify(
    manifest_path: Path,
    reward_path: Path,
    config_path: Path,
    manifest_raw: bytes,
    reward_raw: bytes,
    config_raw: bytes,
    temporary_root: Path,
) -> dict[str, Any]:
    """Rebuild the public config with the exact verifier and require byte identity.

    ``frontend-config`` includes the same final all-six live/reward gate as
    ``verify`` and additionally authenticates the finalized archive before
    deriving every frontend field.  Using that command here avoids a duplicate
    live pass while making the archive checkpoint payload hash come from the
    exact capture-bound provenance path instead of an unauthenticated value in
    the supplied frontend JSON.
    """

    verifier, verifier_sha = repository_tool(
        RECOVERY_VERIFIER_RELATIVE, "recovery rollout verifier"
    )
    stdout_path = temporary_root / "recovery-verify.stdout"
    stderr_path = temporary_root / "recovery-verify.stderr"
    projected_config_path = temporary_root / "recovery-projected-frontend.json"
    projected_sidecar_path = projected_config_path.with_name(
        projected_config_path.name + ".sha256"
    )
    with stdout_path.open("xb") as stdout, stderr_path.open("xb") as stderr:
        result = subprocess.run(
            [
                sys.executable,
                "-B",
                "-I",
                str(verifier),
                "frontend-config",
                "--manifest",
                str(manifest_path),
                "--reward-evidence",
                str(reward_path),
                "--output",
                str(projected_config_path),
            ],
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
            check=False,
            # The nested immutable-archive verifier alone has a reviewed
            # 24-hour bound. Leave room for the subsequent all-six live and
            # reward checks while retaining a finite outer watchdog.
            timeout=RECOVERY_FRONTEND_PROJECTION_TIMEOUT_SECONDS,
        )
    stderr_raw = load_bytes(
        stderr_path,
        "recovery verifier stderr",
        maximum=16 * 1024 * 1024,
        allow_empty=True,
    )
    if result.returncode != 0:
        detail = stderr_raw.decode("utf-8", errors="replace").strip()[-1000:]
        fail(f"exact checked-in recovery verifier failed closed: {detail or f'exit {result.returncode}'}")
    stdout_raw = load_bytes(stdout_path, "recovery verifier stdout", maximum=16 * 1024 * 1024)
    projected_raw = load_bytes(
        projected_config_path,
        "recovery-projected frontend config",
        maximum=MAX_INPUT_BYTES,
    )
    projected_sidecar_raw = load_bytes(
        projected_sidecar_path,
        "recovery-projected frontend config sidecar",
        maximum=1024,
    )
    projected_sha = sha256(projected_raw)
    if projected_sidecar_raw != (
        f"{projected_sha}  {projected_config_path.name}\n".encode("ascii")
    ):
        fail("recovery-projected frontend config sidecar differs")
    expected_terminal = (
        f"FRONTEND CONFIG {projected_config_path} sha256={projected_sha} "
        f"rollout_sha256={sha256(manifest_raw)}\n"
    ).encode("utf-8")
    if (
        stdout_raw.count(b"FRONTEND CONFIG ") != 1
        or not stdout_raw.endswith(expected_terminal)
    ):
        fail("exact checked-in recovery verifier produced no terminal verification record")
    require_exact_frontend_projection(config_raw, projected_raw)
    _verifier_after, verifier_after_sha = repository_tool(
        RECOVERY_VERIFIER_RELATIVE, "recovery rollout verifier"
    )
    if verifier_after_sha != verifier_sha:
        fail("recovery rollout verifier changed while it was invoked")
    if (
        load_bytes(manifest_path, "rollout manifest", maximum=MAX_INPUT_BYTES) != manifest_raw
        or load_bytes(reward_path, "reward evidence", maximum=MAX_INPUT_BYTES) != reward_raw
        or load_bytes(config_path, "frontend config", maximum=MAX_INPUT_BYTES)
        != config_raw
    ):
        fail("sealed rollout, reward evidence, or frontend config changed during verification")
    return {
        "frontendConfigSha256": projected_sha,
        "frontendProjectionSidecarSha256": sha256(projected_sidecar_raw),
        "manifestSha256": sha256(manifest_raw),
        "rewardEvidenceSha256": sha256(reward_raw),
        "stdoutSha256": sha256(stdout_raw),
        "verifierPath": RECOVERY_VERIFIER_RELATIVE.as_posix(),
        "verifierSha256": verifier_sha,
    }


def require_exact_frontend_projection(
    supplied_raw: bytes, projected_raw: bytes
) -> None:
    """Fail unless public bytes are the verifier's exact deterministic projection."""

    if supplied_raw != projected_raw:
        fail(
            "frontend config differs from the exact capture-bound sealed-manifest "
            "projection"
        )


def run_product_surface_verify(
    config_path: Path,
    reward_path: Path,
    config_raw: bytes,
    reward_raw: bytes,
    temporary_root: Path,
    node_path: Path,
    expected_node_sha256: str,
) -> dict[str, str]:
    """Prove exact rollout receipts through the shipped dashboard/explorer readers."""

    verifier, verifier_sha = repository_tool(
        PRODUCT_SURFACE_VERIFIER_RELATIVE, "post-cutover product-surface verifier"
    )
    reader_shas = {
        relative.as_posix(): repository_tool(
            relative, f"post-cutover product reader {relative.as_posix()}"
        )[1]
        for relative in PRODUCT_READER_RELATIVES
    }
    if not node_path.is_absolute():
        fail("post-cutover Node.js runtime path must be absolute")
    expected_node_sha256 = hash_value(
        expected_node_sha256, "post-cutover Node.js runtime SHA-256"
    )
    _node_size, node_sha256 = hash_regular_file(
        node_path, "post-cutover Node.js runtime", 256 * 1024 * 1024
    )
    if node_sha256 != expected_node_sha256:
        fail("post-cutover Node.js runtime differs from its reviewed SHA-256")
    if not os.access(node_path, os.X_OK):
        fail("post-cutover Node.js runtime is not executable")
    version = subprocess.run(
        [str(node_path), "--version"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        env={"PATH": "/usr/bin:/bin", "LANG": "C", "LC_ALL": "C", "TZ": "UTC"},
        check=False,
        timeout=10,
    )
    node_version = version.stdout.decode("ascii", errors="replace").strip()
    if version.returncode != 0 or node_version != "v24.20.0":
        fail(
            "post-cutover product verification requires exact Node.js v24.20.0, "
            f"found {node_version or 'unknown'}"
        )
    stdout_path = temporary_root / "product-surfaces.stdout"
    stderr_path = temporary_root / "product-surfaces.stderr"
    with stdout_path.open("xb") as stdout, stderr_path.open("xb") as stderr:
        result = subprocess.run(
            [
                str(node_path),
                str(verifier),
                "--config",
                str(config_path),
                "--reward-evidence",
                str(reward_path),
            ],
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
            env={"PATH": "/usr/bin:/bin", "LANG": "C", "LC_ALL": "C", "TZ": "UTC"},
            check=False,
            timeout=15 * 60,
        )
    stderr_raw = load_bytes(
        stderr_path, "product-surface verifier stderr", maximum=16 * 1024 * 1024
    )
    if result.returncode != 0:
        detail = stderr_raw.decode("utf-8", errors="replace").strip()[-1000:]
        fail(
            "exact checked-in product-surface verifier failed closed: "
            + (detail or f"exit {result.returncode}")
        )
    stdout_raw = load_bytes(
        stdout_path, "product-surface verifier stdout", maximum=16 * 1024 * 1024
    )
    if not stdout_raw or b"VERIFIED ARC post-cutover product surfaces " not in stdout_raw:
        fail("exact checked-in product-surface verifier produced no terminal verification record")
    _verifier_after, verifier_after_sha = repository_tool(
        PRODUCT_SURFACE_VERIFIER_RELATIVE, "post-cutover product-surface verifier"
    )
    if verifier_after_sha != verifier_sha:
        fail("post-cutover product-surface verifier changed while it was invoked")
    for relative, expected_sha in reader_shas.items():
        _reader_after, reader_after_sha = repository_tool(
            Path(relative), f"post-cutover product reader {relative}"
        )
        if reader_after_sha != expected_sha:
            fail(f"post-cutover product reader {relative} changed while it was invoked")
    _node_size_after, node_sha256_after = hash_regular_file(
        node_path, "post-cutover Node.js runtime", 256 * 1024 * 1024
    )
    if node_sha256_after != node_sha256:
        fail("post-cutover Node.js runtime changed while it was invoked")
    if (
        load_bytes(config_path, "frontend config", maximum=MAX_INPUT_BYTES) != config_raw
        or load_bytes(reward_path, "reward evidence", maximum=MAX_INPUT_BYTES)
        != reward_raw
    ):
        fail("frontend config or reward evidence changed during product verification")
    return {
        "configSha256": sha256(config_raw),
        "nodeSha256": node_sha256,
        "nodeVersion": node_version,
        "readerSha256": reader_shas,
        "rewardEvidenceSha256": sha256(reward_raw),
        "stdoutSha256": sha256(stdout_raw),
        "verifierPath": PRODUCT_SURFACE_VERIFIER_RELATIVE.as_posix(),
        "verifierSha256": verifier_sha,
    }


def arc_text(base: int) -> str:
    value = Decimal(base) / Decimal(1_000_000_000)
    return format(value.normalize(), "f")


def render_readme_block(
    *,
    source_sha: str,
    frontend_sha: str,
    published_at: str,
    checkpoint: dict[str, Any],
    installer_sha: str,
    reward_base: int,
    receipt_count: int,
) -> str:
    total_arc = arc_text(reward_base * receipt_count)
    each_arc = arc_text(reward_base)
    short_source = source_sha[:12]
    short_frontend = frontend_sha[:12]
    fork_count = checkpoint["legacyForkCount"]
    fork_summary = (
        "No divergent capture requires an alternate public fork view."
        if fork_count == 0
        else (
            f"**{fork_count}** divergent captured history "
            f"{'view is' if fork_count == 1 else 'views are'} available as explicit, "
            "immutable, read-only noncanonical forks."
        )
    )
    return f"""{BEGIN_MARKER}
> **Live public testnet (evidence sealed after {published_at}):** The immutable
> [v0.8.0 release](https://github.com/FerrumVir/arc-chain/releases/tag/v0.8.0)
> is built from [`{short_source}`](https://github.com/FerrumVir/arc-chain/commit/{source_sha}).
> All six protocol-v3 validators serve the retained canonical chain through
> block **{checkpoint['height']:,}**, continue at **{checkpoint['recoveryHeight']:,}**, and are
> required to agree before the public console reports the network as healthy.
> Checkpoint height H is distinct from the last observed legacy height
> C=**{checkpoint['legacyObservedCutoffHeight']:,}**. The sealed continuity margin is
> **{checkpoint['legacyContinuitySafetyMargin']} blocks**, so
> F=C+128=**{checkpoint['legacyPublicMaxHeight']:,}**; every v3 validator must advance
> strictly above F before canonical public surfaces reopen. The H+1 boundary
> block/state are `{chain_hash(checkpoint['boundaryBlockHash'], 'checkpoint.boundaryBlockHash')}` /
> `{chain_hash(checkpoint['boundaryStateRoot'], 'checkpoint.boundaryStateRoot')}`;
> recovery domain `{chain_hash(checkpoint['recoveryDomain'], 'checkpoint.recoveryDomain')}`,
> epoch **{checkpoint['recoveryEpoch']}**, validator set **{checkpoint['validatorSetId']}**.
> All six stopped captures remain preservation-bound. Selected or equivalent
> canonical history is retained by v3. {fork_summary} No legacy block was erased
> or renumbered.
> The recovered network bytes were first accepted on Pages from verified config
> commit [`{short_frontend}`](https://github.com/FerrumVir/arc-chain/commit/{frontend_sha}).
> This block and its machine-readable status are published only by a subsequent
> reviewed commit; the deployed site's `deployed-commit.txt` identifies that commit.

The first migration from an unsigned v0.7 build remains manual and pinned to
the exact tag. The published Linux x86_64 component proved this exact
bootstrap in both fresh-install and update-only modes:

```bash
curl -fsSLO --proto '=https' --proto-redir '=https' --tlsv1.2 https://raw.githubusercontent.com/FerrumVir/arc-chain/v0.8.0/install.sh
ARC_INSTALL_SHA256={installer_sha}
if command -v sha256sum >/dev/null 2>&1; then
  printf '%s  %s\\n' "$ARC_INSTALL_SHA256" install.sh | sha256sum -c -
else
  printf '%s  %s\\n' "$ARC_INSTALL_SHA256" install.sh | shasum -a 256 -c -
fi
bash install.sh --version 0.8.0
```

The immutable release includes headless Linux amd64 and arm64 binaries, Intel
and Apple Silicon macOS binaries, Windows CLI binaries, and desktop packages.
The command above is claimed only for the independently exercised Linux
x86_64 published component; the displayed installer digest comes from that
component's canonical acceptance receipt.
The v0.8 desktop updater verifies its updater-signed archive, checks shortly
after startup and every 24 hours when enabled, then asks for confirmation.
The current macOS app has an ad-hoc code seal, not an Apple Developer ID or
notarization claim. The transactional headless updater
runs daily when installed as a service. Linux `.deb` and `.rpm` packages remain
package-manager owned.
The final product gate separately drives the published Linux x86_64 AppImage's
real UI through Tauri IPC under external WebKit automation and runs the exact
published macOS arm64 executable's native inference, settlement, earnings,
projection, network, and explorer core with no Tauri runtime, WebView, plugin,
or IPC actor. The Chromium suite remains the cross-platform rendering proof;
these scopes are recorded separately and are not presented as interchangeable.

### Community support answer sheet

| Community question | Current evidence-backed answer |
|---|---|
| Can an SSH-only EC2/VPS install ARC? | **Yes, on Linux x86_64 as exercised.** Use the pinned command above. `arc-node-linux-x86_64` is the GUI-free amd64 binary; the immutable release also includes Linux arm64 assets without extending this canary claim. |
| Are Intel and Apple Silicon Macs supported? | **Yes, with exact scope stated.** v0.8.0 publishes separate CLI and desktop bytes for x86_64 and arm64 macOS; published-package launch checks cover both, and the production inference/receipt/earnings native-core gate covers the arm64 package. The current app is updater-signature authenticated and ad-hoc code sealed, not claimed as Developer-ID notarized. |
| Does automatic update work? | **Yes, within the documented safety boundary.** v0.8 desktop checks automatically but requires confirmation; managed headless installs use the transactional daily updater. The one-time unsigned v0.7 migration is deliberately manual. |
| Are the seed nodes upgraded? | **Yes.** Six protocol-v3 validators are bound to checkpoint H={checkpoint['height']:,}, transition H+1={checkpoint['recoveryHeight']:,}, and an all-six public health gate. |
| Can a stake-zero community worker earn the configured {each_arc} ARC reward? | **Yes.** The production canary proved {receipt_count} distinct mined rewards totaling {total_arc} ARC for one exact-model stake-zero worker. Registration or a raw `0x16` inference attestation alone does not pay. |
| What hardware should a worker run? | The production target is Llama-2-7B Q4_K_M (about 4 GB on disk). Leave RAM and CPU headroom for a complete model load; GPU is optional and hardware never guarantees work. |
| Where are inference, earnings, and blocks? | The [dashboard]({PUBLIC_CONSOLE}) shows receipt-backed inference and confirmed/projected earnings. The [explorer]({PUBLIC_EXPLORER}) resolves the matching canonical block and transaction. Projections remain unavailable until enough real observations exist. |

See the [headless/server guide](docs/HEADLESS_INSTALL.md), the
[desktop guide](docs/GETTING_STARTED.md), and the
[2–3 minute walkthrough](docs/COMMUNITY-NODE-WALKTHROUGH.md). The exact public
proof identifiers are published in
[`shared/frontend/production-status.json`](shared/frontend/production-status.json).
{END_MARKER}"""


def build(args: argparse.Namespace) -> tuple[Path, Path]:
    readme_raw = load_bytes(args.readme, "README", maximum=4 * 1024 * 1024)
    try:
        readme = readme_raw.decode("utf-8", errors="strict")
    except UnicodeDecodeError as error:
        fail(f"README is not UTF-8: {error}")
    if readme.count(BEGIN_MARKER) != 1 or readme.count(END_MARKER) != 1:
        fail("README must contain exactly one ordered ARC public-truth marker pair")
    begin = readme.index(BEGIN_MARKER)
    end_begin = readme.index(END_MARKER)
    if end_begin <= begin:
        fail("README must contain exactly one ordered ARC public-truth marker pair")
    end = end_begin + len(END_MARKER)

    release, _release_raw = load_json(args.release_api, "release API response", canonical=False)
    config, config_raw = load_json(args.frontend_config, "frontend config", canonical=True)
    reward, reward_raw = load_json(args.reward_evidence, "reward evidence", canonical=True)
    manifest, manifest_raw = load_json(args.rollout_manifest, "rollout manifest", canonical=True)

    with tempfile.TemporaryDirectory(prefix="arc-public-truth-") as temporary:
        temporary_root = Path(temporary)
        published, published_hashes, published_values = validate_published_acceptance(
            args, temporary_root
        )
        source_sha = published_values["sourceSha"]
        published_at, release_assets = validate_release(release, source_sha)
        binding = published_values["binding"]
        binding_release = binding.get("release")
        binding_workflow = binding.get("release_workflow")
        if (
            binding.get("repository") != REPOSITORY
            or binding.get("tag") != TAG
            or binding.get("commit") != source_sha
            or not isinstance(binding_release, dict)
            or binding_release.get("id") != release["id"]
            or binding_release.get("immutable") is not True
            or not isinstance(binding_workflow, dict)
            or binding_workflow.get("run_id") != published["releaseRunId"]
            or binding_workflow.get("run_attempt") != published["releaseRunAttempt"]
            or binding_release.get("id") != published["releaseId"]
            or binding.get("assets") != release_assets
        ):
            fail("published release binding differs from the current immutable release API")
        release_run_id = positive_int(
            binding_workflow.get("run_id"), "published release binding run ID"
        )
        release_run_attempt = positive_int(
            binding_workflow.get("run_attempt"), "published release binding run attempt"
        )
        if (
            published_values["linuxComponent"].get("release_run_id") != release_run_id
            or published_values["linuxComponent"].get("release_run_attempt")
            != release_run_attempt
        ):
            fail("published Linux installer component belongs to another release attempt")
        installer_sha = published_values["installerSha256"]
        if release_assets["install.sh"]["sha256"] != installer_sha:
            fail("published Linux installer differs from the immutable release API")

        pages, pages_hashes = validate_pages(args, config_raw)
        frontend_sha = pages["acceptedConfigCommit"]
        checkpoint = validate_network(config, source_sha, manifest)
        worker, reward_base, receipt_count = validate_reward(
            reward, manifest, source_sha
        )

        # This is deliberately the final evidence gate.  The exact checked-in
        # rollout verifier re-reads the sealed files and performs live all-six
        # convergence and mined-reward verification immediately before any
        # public claim or acceptance receipt is rendered.
        recovery = run_recovery_verify(
            args.rollout_manifest,
            args.reward_evidence,
            args.frontend_config,
            manifest_raw,
            reward_raw,
            config_raw,
            temporary_root,
        )
        product_surfaces = run_product_surface_verify(
            args.frontend_config,
            args.reward_evidence,
            config_raw,
            reward_raw,
            temporary_root,
            args.node,
            args.node_sha256,
        )
        appimage_verification = run_packaged_appimage_verify(
            args.packaged_appimage_receipt,
            temporary_root / "published-acceptance" / "release-binding.json",
        )
        macos_evidence = load_macos_package_evidence(
            args.macos_package_evidence_dir
        )
        desktop_live = validate_desktop_live_receipt(
            args.desktop_live_receipt,
            source_sha=source_sha,
            config_raw=config_raw,
            reward=reward,
            rollout_sha256=sha256(manifest_raw),
            appimage_verification=appimage_verification,
            macos_evidence=macos_evidence,
        )
        product_surfaces = {**product_surfaces, "desktopLive": desktop_live}

        acceptance = {
            "pages": {**pages, "evidenceSha256": pages_hashes},
            "publishedAcceptance": {
                **published,
                "evidenceSha256": published_hashes,
            },
            "productSurfaces": product_surfaces,
            "recovery": recovery,
            "release": {
                "assetSetSha256": sha256(canonical_json(release_assets)),
                "id": release["id"],
                "publishedAt": published_at,
                "releaseApiSha256": sha256(_release_raw),
                "runAttempt": release_run_attempt,
                "runId": release_run_id,
                "sourceCommit": source_sha,
                "tag": TAG,
            },
            "repository": REPOSITORY,
            "schema": ACCEPTANCE_SCHEMA,
        }
        acceptance_raw = canonical_json(acceptance)

        block = render_readme_block(
            source_sha=source_sha,
            frontend_sha=frontend_sha,
            published_at=published_at,
            checkpoint=checkpoint,
            installer_sha=installer_sha,
            reward_base=reward_base,
            receipt_count=receipt_count,
        )
    output_readme = (readme[:begin] + block + readme[end:]).encode("utf-8")
    status = {
        "acceptance": {
            # Embed the complete canonical v2 receipt so a public reader can
            # independently canonicalize/re-hash it and follow its exact
            # workflow, run, job, artifact, Pages, and recovery evidence
            # identities.  The receipt deliberately does not depend on this
            # status document, so this introduces no circular hash.
            "receipt": acceptance,
            "publishedArtifactReceiptSha256": published[
                "canonicalReceiptSha256"
            ],
            "receiptSha256": sha256(acceptance_raw),
            "releaseRunAttempt": release_run_attempt,
            "releaseRunId": release_run_id,
        },
        "checkpoint": {
            "blockHash": chain_hash(checkpoint["blockHash"], "checkpoint.blockHash"),
            "boundaryBlockHash": chain_hash(
                checkpoint["boundaryBlockHash"], "checkpoint.boundaryBlockHash"
            ),
            "boundaryStateRoot": chain_hash(
                checkpoint["boundaryStateRoot"], "checkpoint.boundaryStateRoot"
            ),
            "checkpointFileSha256": checkpoint["checkpointFileSha256"],
            "checkpointPayloadHash": checkpoint["checkpointPayloadHash"],
            "height": checkpoint["height"],
            "legacyContinuitySafetyMargin": checkpoint[
                "legacyContinuitySafetyMargin"
            ],
            "legacyObservedCutoffHeight": checkpoint[
                "legacyObservedCutoffHeight"
            ],
            "legacyPublicMaxHeight": checkpoint["legacyPublicMaxHeight"],
            "legacyReopeningFormula": "F=C+128",
            "manifestHash": chain_hash(checkpoint["manifestHash"], "checkpoint.manifestHash"),
            "protocolVersion": checkpoint["protocolVersion"],
            "recoveryDomain": chain_hash(
                checkpoint["recoveryDomain"], "checkpoint.recoveryDomain"
            ),
            "recoveryEpoch": checkpoint["recoveryEpoch"],
            "recoveryHeight": checkpoint["recoveryHeight"],
            "stateRoot": chain_hash(checkpoint["stateRoot"], "checkpoint.stateRoot"),
            "validatorSetId": checkpoint["validatorSetId"],
        },
        "fleet": {
            "legacyForkCount": checkpoint["legacyForkCount"],
            "legacyForkNodes": checkpoint["legacyForkNodes"],
            "legacyForkPolicy": "immutable-read-only-noncanonical",
            "requiredHealthyValidators": 6,
            "rolloutId": checkpoint["rolloutId"],
            "validatorCount": 6,
        },
        "network": config["network"],
        "pages": {
            "acceptedConfigCommit": frontend_sha,
            "deploymentId": pages["deploymentId"],
            "runAttempt": pages["runAttempt"],
            "runId": pages["runId"],
        },
        "release": {
            "id": release["id"],
            "immutable": True,
            "publishedAt": published_at,
            "sourceCommit": source_sha,
            "tag": TAG,
            "url": release["html_url"],
            "version": VERSION,
        },
        "rewards": {
            "canaryReceiptCount": receipt_count,
            "canaryWorker": worker,
            "demonstratedGrossArc": float(Decimal(reward_base * receipt_count) / Decimal(1_000_000_000)),
            "demonstratedGrossBase": reward_base * receipt_count,
            "rewardPerReceiptArc": float(Decimal(reward_base) / Decimal(1_000_000_000)),
            "rewardPerReceiptBase": reward_base,
            "stakeZeroEligible": True,
        },
        "schema": STATUS_SCHEMA,
        "services": {"dashboard": PUBLIC_CONSOLE, "explorer": PUBLIC_EXPLORER},
        "state": "recovered",
    }
    output_status = canonical_json(status)

    output_dir: Path = args.output_dir
    if not output_dir.is_absolute() or output_dir.name in {"", ".", ".."}:
        fail("output directory must be an absolute dedicated path")
    parent_fd = -1
    directory_fd = -1
    try:
        parent = output_dir.parent
        parent_before = parent.lstat()
        if (
            not stat.S_ISDIR(parent_before.st_mode)
            or parent_before.st_uid != os.geteuid()
            or stat.S_IMODE(parent_before.st_mode) & 0o022
        ):
            fail(
                "public-truth output parent must be a real, current-user-owned, "
                "non-group/world-writable directory"
            )
        directory_flags = (
            os.O_RDONLY
            | getattr(os, "O_DIRECTORY", 0)
            | getattr(os, "O_NOFOLLOW", 0)
        )
        parent_fd = os.open(parent, directory_flags)
        opened_parent = os.fstat(parent_fd)
        if (
            opened_parent.st_dev,
            opened_parent.st_ino,
            opened_parent.st_mode,
        ) != (
            parent_before.st_dev,
            parent_before.st_ino,
            parent_before.st_mode,
        ):
            fail("public-truth output parent changed while it was opened")
        os.mkdir(output_dir.name, mode=0o700, dir_fd=parent_fd)
        directory_fd = os.open(output_dir.name, directory_flags, dir_fd=parent_fd)
        os.fchmod(directory_fd, 0o700)
        created = os.stat(
            output_dir.name, dir_fd=parent_fd, follow_symlinks=False
        )
        opened = os.fstat(directory_fd)
        if (
            not stat.S_ISDIR(opened.st_mode)
            or stat.S_IMODE(opened.st_mode) != 0o700
            or (created.st_dev, created.st_ino) != (opened.st_dev, opened.st_ino)
        ):
            fail("public-truth output directory changed while it was opened")
        readme_path = output_dir / "README.md"
        status_path = output_dir / "production-status.json"
        acceptance_path = output_dir / "POST-RELEASE-ACCEPTANCE.json"
        for name, raw in (
            ("README.md", output_readme),
            ("production-status.json", output_status),
            ("POST-RELEASE-ACCEPTANCE.json", acceptance_raw),
        ):
            file_flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
            if hasattr(os, "O_NOFOLLOW"):
                file_flags |= os.O_NOFOLLOW
            descriptor = os.open(name, file_flags, 0o400, dir_fd=directory_fd)
            try:
                view = memoryview(raw)
                while view:
                    written = os.write(descriptor, view)
                    if written <= 0:
                        fail(f"cannot completely write public-truth output {name}")
                    view = view[written:]
                os.fsync(descriptor)
                os.fchmod(descriptor, 0o400)
            finally:
                os.close(descriptor)
        os.fsync(directory_fd)
        final = os.stat(output_dir.name, dir_fd=parent_fd, follow_symlinks=False)
        if (final.st_dev, final.st_ino) != (opened.st_dev, opened.st_ino):
            fail("public-truth output directory changed while it was written")
        final_parent = os.fstat(parent_fd)
        if (
            final_parent.st_dev,
            final_parent.st_ino,
            final_parent.st_mode,
        ) != (
            opened_parent.st_dev,
            opened_parent.st_ino,
            opened_parent.st_mode,
        ):
            fail("public-truth output parent changed while it was written")
        # Persist the one-shot directory entry as well as its contents. Without
        # this parent fsync, a crash can make the create-only path disappear and
        # incorrectly permit the same evidence attempt to be reused.
        os.fsync(parent_fd)
    except OSError as error:
        fail(f"cannot create public-truth output: {error}")
    finally:
        if directory_fd >= 0:
            os.close(directory_fd)
        if parent_fd >= 0:
            os.close(parent_fd)
    return readme_path, status_path


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--readme", required=True, type=Path)
    result.add_argument("--release-api", required=True, type=Path)
    result.add_argument("--pages-workflow", required=True, type=Path)
    result.add_argument("--pages-run", required=True, type=Path)
    result.add_argument("--pages-jobs", required=True, type=Path)
    result.add_argument("--pages-api", required=True, type=Path)
    result.add_argument("--pages-deployments", required=True, type=Path)
    result.add_argument("--pages-statuses", required=True, type=Path)
    result.add_argument("--frontend-config", required=True, type=Path)
    result.add_argument("--deployed-commit", required=True, type=Path)
    result.add_argument("--deployed-sha256sums", required=True, type=Path)
    result.add_argument("--published-workflow", required=True, type=Path)
    result.add_argument("--published-run", required=True, type=Path)
    result.add_argument("--published-jobs", required=True, type=Path)
    result.add_argument("--published-artifact-metadata", required=True, type=Path)
    result.add_argument("--published-artifact-zip", required=True, type=Path)
    result.add_argument("--reward-evidence", required=True, type=Path)
    result.add_argument("--rollout-manifest", required=True, type=Path)
    result.add_argument("--desktop-live-receipt", required=True, type=Path)
    result.add_argument("--packaged-appimage-receipt", required=True, type=Path)
    result.add_argument("--macos-package-evidence-dir", required=True, type=Path)
    result.add_argument("--node", required=True, type=Path)
    result.add_argument("--node-sha256", required=True)
    result.add_argument("--output-dir", required=True, type=Path)
    return result


def main() -> int:
    try:
        readme, status = build(parser().parse_args())
    except TruthError as error:
        print(f"public truth: {error}", file=sys.stderr)
        return 1
    print(f"public truth README: {readme}")
    print(f"public truth status: {status}")
    print(f"public truth acceptance: {status.parent / 'POST-RELEASE-ACCEPTANCE.json'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
