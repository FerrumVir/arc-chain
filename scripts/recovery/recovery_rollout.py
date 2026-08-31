#!/usr/bin/env python3
"""Content-addressed six-validator ARC recovery rehearsal and rollout.

The command is deliberately read-only unless ``run --execute`` is supplied
with two exact copies of the sealed rollout-manifest hash.  It uses only the
Python standard library so the same artifact can be audited and run on a clean
operator workstation.
"""

from __future__ import annotations

import argparse
import base64
import binascii
import concurrent.futures
import datetime as dt
import hashlib
import ipaddress
import json
import os
import re
import shlex
import signal
import ssl
import stat
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable, Mapping, NoReturn, Sequence


SCHEMA = "arc.recovery.rollout.v1"
COMMUNITY_REWARD_BASE = 2_500_000_000
PROJECTION_COLLECTING_REASON = (
    "collecting data: a projection needs at least 3 successful mined reward "
    "receipts spanning at least 24 hours, not the initial one or two rollout canaries"
)
HEX_32_RE = re.compile(r"^(?:0x)?[0-9a-f]{64}$")
LOWER_HEX_32_RE = re.compile(r"^[0-9a-f]{64}$")
SAFE_ID_RE = re.compile(r"^[a-z][a-z0-9-]{0,62}$")
SAFE_REMOTE_RE = re.compile(r"^[A-Za-z0-9_./:@+=,-]+$")
SAFE_HOST_RE = re.compile(r"^[A-Za-z0-9.-]+$")
PROTECTED_FLAGS = {
    "--approved-recovery-manifest-hash",
    "--allow-insecure-community-rpc",
    "--auto-shard-join",
    "--community",
    "--community-mode",
    "--community-rpc-url",
    "--data-dir",
    "--enable-i16",
    "--genesis",
    "--insecure-dev-validator-seed",
    "--full-integer-worker",
    "--model",
    "--p2p-port",
    "--peers",
    "--recovery-checkpoint",
    "--recovery-epoch",
    "--rpc",
    "--rpc-unix",
    "--shard-hosts",
    "--shard-end",
    "--shard-range",
    "--shard-start",
    "--stake",
    "--tokenizer-only",
    "--no-community",
    "--validator-key-file",
    "--validator-seed",
    "--validator-set-id",
}
REQUIRED_VALIDATORS = 6
REQUIRED_APPROVALS = 5
PRODUCTION_FLEET = (
    ("nyc", "149.28.32.76"),
    ("lax", "140.82.16.112"),
    ("ams", "136.244.109.1"),
    ("lhr", "104.238.171.11"),
    ("nrt", "202.182.107.41"),
    ("sgp", "149.28.153.31"),
)
LEGACY_OFFICIAL_ORIGINS = tuple(
    {"node": node, "host": host, "origin": f"http://{host}:9090"}
    for node, host in PRODUCTION_FLEET
)
LEGACY_CONTINUITY_SAFETY_MARGIN = 128
LEGACY_CONTINUITY_SAFETY_MARGIN_POLICY = {
    "prune_depth": 100,
    "commit_rule_rounds": 2,
    "operational_headroom": 26,
    "cryptographic_global_absence_proof": False,
}
LEGACY_REOPENING_POLICY = {
    "required_validator_count": 6,
    "height_relation": "strictly-greater-than-legacy_public_max_height",
    "required_equal_fields": ["block_hash", "state_root"],
}
LEGACY_LATE_FORK_CIRCUIT = {
    "monitor_scope": "retired-and-community-legacy-sources",
    "trigger": "self-consistent-legacy-fork-candidate-above-observed-cutoff-height",
    "action": "enter-maintenance-preserve-and-offline-validate",
    "rewrite_v3_history_allowed": False,
}
LEGACY_QUARANTINE_THREAT_MODEL = {
    "trusted_host_root_required": True,
    "sealed_reviewed_legacy_binary_non_adversarial": True,
    "quarantine_purpose": "operational-network-isolation",
    "hostile_root_containment_claimed": False,
}
CANONICAL_MODEL_SHA256 = "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa"
CANONICAL_MODEL_SIZE_BYTES = 4_081_004_224
CANONICAL_MODEL_LAYERS = 32
CANONICAL_EXECUTION_PROFILE = "INT8 integer (per-row, cross-platform deterministic)"
REQUIRED_SHARD_REPLICATION = 3
MIN_LAYERS_PER_VALIDATOR = 15
MAX_LAYERS_PER_VALIDATOR = 17
DEFAULT_PUBLIC_GET_PATHS = (
    "/health",
    "/info",
    "/network/info",
    "/stats",
    "/validators",
    "/block/latest",
    "/blocks",
    "/inference/attestations",
    "/economics/rewards",
    "/faucet/status",
    "/community/list",
    "/community/reward_policy",
    "/workers/scoreboard",
    "/shards",
    "/models",
    "/models/shards",
)
PUBLIC_BROWSER_ORIGIN = "https://ferrumvir.github.io"
LETS_ENCRYPT_PRODUCTION_DIRECTORY = "https://acme-v02.api.letsencrypt.org/directory"
CADDY_VERSION = "v2.11.4"
CADDY_LINUX_AMD64_SHA256 = "b7105518e3ed1c0761f232e44fc09345535533c9cb0abf0e12809416c7ac64d9"
TLS_MAX_LEAF_LIFETIME_SECONDS = 160 * 60 * 60
TLS_MIN_REMAINING_VALIDITY_SECONDS = 48 * 60 * 60
# Uniform Ubuntu 24.04 (Noble) production fleet security boundary.  The
# interlock lives in nginx auth_request specifically to avoid Caddy
# GHSA-6365-7ppr-5r92; never accept an unversioned apt candidate or a binary
# that merely reports the expected version string.
NGINX_PACKAGE_VERSION = "1.24.0-2ubuntu7.17"
NGINX_LINUX_AMD64_SHA256 = "1f16b72bea2f44e5d04fe6cf9e3e4b0dec53a82c50c7c1533c302a8ecaeccacf"
RECOVERY_PROBE_PREFIX = b"ARC-RCV-PROBE1\0\0"
RECOVERY_PROBE_ID_DOMAIN = b"ARC-recovery-reward-probe-id-v1\0"
DEFAULT_PUBLIC_POST_PATHS = (
    "/inference/run",
    "/inference/run_consensus",
    "/community/register",
    "/community/heartbeat",
    "/community/claim_work",
    "/community/submit_work",
    "/tx/submit",
    "/tx/submit_signed",
    "/tx/submit_batch",
    "/faucet/claim",
)
PUBLIC_PARAMETERIZED_GET_PATHS = (
    "/block/{height}",
    "/block/{height}/txs",
    "/tx/{hash}",
    "/tx/{hash}/full",
    "/account/{address}",
    "/account/{address}/txs",
    "/worker/earnings/{address}",
    "/community/reward_receipt/{tx_hash}",
    "/community/reward_job/{job_id}",
)
INTERNAL_VALIDATOR_POST_PATHS = (
    "/internal/community/reward/approve",
    "/shards/announce",
    "/inference/forward_shard",
    "/inference/cleanup_shard",
)
SOURCE_ONLY_NOT_PUBLIC_PATHS = (
    "/inference/run_sharded",
    "/inference/results",
    "/community/reward_approval/{job_id}",
    "/eth",
)
# Must match `MAX_TX_SUBMIT_BATCH_SIZE` in `crates/arc-node/src/rpc.rs`.
# The rollout probes one item beyond this bound through every public TLS
# gateway so a stale/unbounded node binary cannot pass production promotion.
PUBLIC_TX_SUBMIT_BATCH_MAX_ITEMS = 64
PUBLIC_INFERENCE_TIMEOUT_SECONDS = 4000
# Background admission closes at SIGTERM. Already-owned community work keeps
# its 300-second crash/late-submit grace, then task joins and the WAL durability
# barrier retain another two minutes before systemd may SIGKILL the validator.
COMMUNITY_LATE_SUBMIT_GRACE_SECONDS = 300
NODE_GRACEFUL_STOP_TIMEOUT_SECONDS = (
    PUBLIC_INFERENCE_TIMEOUT_SECONDS + COMMUNITY_LATE_SUBMIT_GRACE_SECONDS + 120
)
# `systemctl stop/restart` waits for the unit's TimeoutStopSec before it can
# return. The local SSH watchdog must remain outside that complete durability
# window, including RestartSec=2 and remote scheduling/transport slack.
NODE_SERVICE_START_TIMEOUT_SECONDS = 90
NODE_SERVICE_STOP_TIMEOUT_SECONDS = NODE_GRACEFUL_STOP_TIMEOUT_SECONDS + 60
NODE_SERVICE_RESTART_TIMEOUT_SECONDS = NODE_GRACEFUL_STOP_TIMEOUT_SECONDS + 60
# Rollback may restore the validator and the four lightweight gateway/archive
# units serially. Preserve one full validator drain plus a bounded five-minute
# allowance rather than abandoning an in-progress remote systemd transaction.
PRODUCTION_ROLLBACK_TIMEOUT_SECONDS = NODE_GRACEFUL_STOP_TIMEOUT_SECONDS + 300
VALIDATOR_APPROVAL_TIMEOUT_SECONDS = 1500
WORKER_SUBMIT_TIMEOUT_SECONDS = 2700
LEGACY_ARCHIVE_USER = "arc-archive"
CADDY_USER = "arc-caddy"
NGINX_FILTER_USER = "arc-rpc-filter"
NGINX_ATTACKER_USER = "arc-rpc-attacker"
LATE_FORK_INTERLOCK_USER = "arc-interlock"
LATE_FORK_INTERLOCK_GROUP = "arc-interlock-gate"
RPC_ORIGIN_GROUP = "arc-rpc-origin"
LEGACY_ARCHIVE_FILTER_PORT = 18090
LEGACY_ARCHIVE_RPC_PORT = 19090
CAPTURE_DOMAIN = b"ARC recovery capture v2\0"
ARCHIVE_FINALIZATION_FIELDS = (
    "complete_sha256",
    "archive_manifest_sha256",
    "sha256sums_sha256",
    "prearchive_rollout_sha256",
)
PRETAG_GROUPS = (
    ("headless", "linux-x86_64"),
    ("headless", "linux-arm64"),
    ("headless", "macos-arm64"),
    ("headless", "macos-x86_64"),
    ("headless", "windows-x86_64"),
    ("desktop", "linux-x86_64"),
    ("desktop", "macos-arm64"),
    ("desktop", "macos-x86_64"),
    ("desktop", "windows-x86_64"),
)
PRETAG_DESKTOP_FILES = {
    "linux-x86_64": (
        "arc-desktop-linux-x86_64.AppImage",
        "arc-desktop-linux-x86_64.AppImage.sig",
        "arc-desktop-linux-x86_64.deb",
        "arc-desktop-linux-x86_64.rpm",
    ),
    "macos-arm64": (
        "arc-desktop-macos-arm64.app.tar.gz",
        "arc-desktop-macos-arm64.app.tar.gz.sig",
        "arc-desktop-macos-arm64.dmg",
    ),
    "macos-x86_64": (
        "arc-desktop-macos-x86_64.app.tar.gz",
        "arc-desktop-macos-x86_64.app.tar.gz.sig",
        "arc-desktop-macos-x86_64.dmg",
    ),
    "windows-x86_64": (
        "arc-desktop-windows-x86_64-setup.exe",
        "arc-desktop-windows-x86_64-setup.exe.sig",
        "arc-desktop-windows-x86_64.msi",
    ),
}


class RolloutError(RuntimeError):
    """A fail-closed manifest, preflight, or rollout error."""


def fail(message: str) -> NoReturn:
    raise RolloutError(message)


def canonical_bytes(value: Mapping[str, Any]) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n").encode()


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def capture_id_for_freeze_plan_hash(freeze_plan_sha256: str) -> str:
    if not LOWER_HEX_32_RE.fullmatch(freeze_plan_sha256):
        fail("freeze plan sha256 must be exactly 64 lowercase hexadecimal characters")
    return hashlib.sha256(CAPTURE_DOMAIN + bytes.fromhex(freeze_plan_sha256)).hexdigest()


def prearchive_projection_digest(manifest: Mapping[str, Any]) -> str:
    projection = json.loads(json.dumps(manifest))
    archive = projection.get("archive")
    if not isinstance(archive, dict):
        fail("prearchive projection requires manifest.archive")
    for field in ARCHIVE_FINALIZATION_FIELDS:
        archive[field] = "0" * 64
    return sha256_bytes(canonical_bytes(projection))


def require_prearchive_manifest(manifest: Mapping[str, Any]) -> None:
    archive = manifest.get("archive")
    if not isinstance(archive, Mapping) or any(
        archive.get(field) != "0" * 64 for field in ARCHIVE_FINALIZATION_FIELDS
    ):
        fail(
            "archive sealing requires the exact prearchive manifest with all four "
            "archive finalization roots zero"
        )


def validate_drive_remote(value: Any, field: str) -> str:
    remote = required_string(value, field)
    if (
        "\x00" in remote
        or "\n" in remote
        or "\r" in remote
        or remote.startswith("-")
        or ":" not in remote
        or remote.endswith("/")
        or "/../" in f"/{remote.split(':', 1)[1]}/"
    ):
        fail(f"{field} must be an exact, non-option rclone remote path without traversal")
    remote_name, remote_path = remote.split(":", 1)
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}", remote_name):
        fail(f"{field} has an unsafe rclone remote name")
    if not remote_path or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9 ._/@%+=,-]{0,511}", remote_path):
        fail(f"{field} has an unsafe rclone remote path")
    return remote


def bare_hash(value: Any, field: str) -> str:
    if not isinstance(value, str) or not HEX_32_RE.fullmatch(value):
        fail(f"{field} must be exactly 32 lowercase hexadecimal bytes")
    return value.removeprefix("0x")


def required_string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value:
        fail(f"{field} must be a non-empty string")
    return value


def required_int(value: Any, field: str, *, minimum: int = 0, maximum: int | None = None) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        fail(f"{field} must be an integer >= {minimum}")
    if maximum is not None and value > maximum:
        fail(f"{field} must be <= {maximum}")
    return value


def require_keys(value: Any, field: str, required: Iterable[str], optional: Iterable[str] = ()) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{field} must be an object")
    required_set = set(required)
    allowed = required_set | set(optional)
    missing = sorted(required_set - set(value))
    unknown = sorted(set(value) - allowed)
    if missing:
        fail(f"{field} is missing: {', '.join(missing)}")
    if unknown:
        fail(f"{field} contains unknown fields: {', '.join(unknown)}")
    return value


def validate_public_tls_evidence(
    value: Any,
    *,
    rollout_sha256: str,
    node: str,
    host: str,
    phase: str,
    now_unix: int | None = None,
) -> dict[str, Any]:
    """Validate one exact, freshly verified public IPv4 leaf-certificate proof."""

    evidence = require_keys(
        value,
        f"{node} public TLS evidence",
        (
            "schema", "rollout_manifest_sha256", "phase", "node", "host",
            "caddy_version", "caddy_binary_sha256", "acme_directory",
            "acme_profile", "verification_host", "san_ip_addresses",
            "san_dns_names", "issuer_organization", "leaf_sha256",
            "not_before_unix", "not_after_unix", "lifetime_seconds",
            "remaining_validity_seconds", "verified_at_unix",
            "hostname_verified", "public_trust_verified", "leaf_self_signed",
            "https_probe_status", "renewal_observed", "evidence_scope",
        ),
    )
    try:
        address = ipaddress.ip_address(host)
    except ValueError:
        fail(f"{node} TLS host is not an IP address")
    if address.version != 4 or str(address) != host:
        fail(f"{node} TLS host must be one canonical IPv4 address")
    if phase not in {"preflight", "post-rollout"}:
        fail(f"{node} TLS evidence phase is unsupported")
    verified_at = required_int(
        evidence["verified_at_unix"], f"{node} TLS verified_at_unix", minimum=1
    )
    not_before = required_int(
        evidence["not_before_unix"], f"{node} TLS not_before_unix", minimum=1
    )
    not_after = required_int(
        evidence["not_after_unix"], f"{node} TLS not_after_unix", minimum=1
    )
    lifetime = required_int(
        evidence["lifetime_seconds"], f"{node} TLS lifetime_seconds", minimum=1
    )
    remaining = required_int(
        evidence["remaining_validity_seconds"],
        f"{node} TLS remaining_validity_seconds",
        minimum=1,
    )
    current = int(time.time()) if now_unix is None else now_unix
    if isinstance(current, bool) or not isinstance(current, int) or current <= 0:
        fail("TLS validation time must be one positive Unix second")
    if (
        evidence["schema"] != "arc.recovery.public-tls-evidence.v1"
        or evidence["rollout_manifest_sha256"] != rollout_sha256
        or evidence["phase"] != phase
        or evidence["node"] != node
        or evidence["host"] != host
        or evidence["caddy_version"] != CADDY_VERSION
        or evidence["caddy_binary_sha256"] != CADDY_LINUX_AMD64_SHA256
        or evidence["acme_directory"] != LETS_ENCRYPT_PRODUCTION_DIRECTORY
        or evidence["acme_profile"] != "shortlived"
        or evidence["verification_host"] != host
        or evidence["san_ip_addresses"] != [host]
        or evidence["san_dns_names"] != []
        or evidence["issuer_organization"] != "Let's Encrypt"
        or evidence["hostname_verified"] is not True
        or evidence["public_trust_verified"] is not True
        or evidence["leaf_self_signed"] is not False
        or evidence["https_probe_status"] != 404
        or evidence["renewal_observed"] is not False
        or evidence["evidence_scope"]
        != "fresh-verified-handshake-and-https-probe-not-renewal"
    ):
        fail(f"{node} public TLS identity/trust evidence differs")
    if (
        not isinstance(evidence["leaf_sha256"], str)
        or not LOWER_HEX_32_RE.fullmatch(evidence["leaf_sha256"])
        or evidence["leaf_sha256"] == "0" * 64
    ):
        fail(f"{node} TLS leaf sha256 must be one nonzero lowercase SHA-256")
    if lifetime != not_after - not_before:
        fail(f"{node} TLS leaf lifetime arithmetic differs")
    if lifetime > TLS_MAX_LEAF_LIFETIME_SECONDS:
        fail(f"{node} TLS leaf exceeds the 160-hour short-lived maximum")
    if remaining != not_after - verified_at:
        fail(f"{node} TLS remaining-validity arithmetic differs")
    if remaining < TLS_MIN_REMAINING_VALIDITY_SECONDS:
        fail(f"{node} TLS leaf is too near expiry for production rollout")
    if not_before > verified_at or verified_at >= not_after:
        fail(f"{node} TLS evidence time is outside the leaf validity window")
    if abs(current - verified_at) > 300:
        fail(f"{node} TLS evidence is stale or future-dated")
    return evidence


def absolute_path(value: Any, field: str) -> str:
    raw = required_string(value, field)
    path = Path(raw)
    if not path.is_absolute():
        fail(f"{field} must be an absolute path")
    if raw in {"/", "/root", "/home", "/Users", "/opt", "/var", "/tmp"}:
        fail(f"{field} is too broad to be a rollout path")
    if "\x00" in raw or "\n" in raw or "\r" in raw:
        fail(f"{field} contains an unsafe character")
    if os.path.normpath(raw) != raw:
        fail(f"{field} must be lexically normalized without dot segments or a trailing slash")
    return raw


def systemd_literal(value: str, field: str) -> str:
    """Require one literal systemd argv/environment token.

    Percent is never literal in an ``ExecStart=`` or ``Environment=`` line:
    systemd expands it as a unit specifier before the process starts.  Reject it
    while the rollout manifest is still read-only so the sealed argv cannot name
    one path during import and a different path at service startup.
    """

    if "%" in value:
        fail(f"{field} must not contain systemd percent specifiers")
    if not SAFE_REMOTE_RE.fullmatch(value):
        fail(f"{field} contains a character unsupported by systemd argv")
    return value


def paths_overlap(left: str, right: str) -> bool:
    common = os.path.commonpath((left, right))
    return common in {left, right}


def validate_artifact(value: Any, field: str) -> dict[str, Any]:
    artifact = require_keys(value, field, ("path", "sha256"))
    absolute_path(artifact["path"], f"{field}.path")
    if not isinstance(artifact["sha256"], str) or not LOWER_HEX_32_RE.fullmatch(artifact["sha256"]):
        fail(f"{field}.sha256 must be exactly 64 lowercase hexadecimal characters")
    return artifact


def pretag_artifact_key(kind: str, platform: str) -> str:
    return f"pretag_raw_{kind}_{platform.replace('-', '_')}"


def _validate_protected_pretag_proof(
    value: Any,
    *,
    label: str,
    kind: str,
    platform: str,
    source_commit: str,
    run_id: int,
    run_attempt: int,
    response_labels: tuple[str, str, str, str] = (
        "workflow", "run", "artifact_set", "protected_main"
    ),
) -> dict[str, Any]:
    proof = require_keys(value, label, ("schema", "live", "api", "artifact"))
    if proof["schema"] != "arc.protected-pretag-artifact.v1":
        fail(f"{label}.schema is unsupported")
    live = require_keys(
        proof["live"],
        f"{label}.live",
        (
            "repository", "protected_branch", "commit", "workflow_id", "workflow_path",
            "run_id", "run_attempt", "artifact_id", "artifact_name", "artifact_digest",
            "artifact_size_in_bytes", "api_verified_at_unix",
        ),
    )
    api = require_keys(
        proof["api"],
        f"{label}.api",
        (
            "origin", "anonymous", "redirects_followed", "max_age_seconds",
            "curl_sha256", "ca_bundle_sha256", "responses",
        ),
    )
    artifact = require_keys(
        proof["artifact"],
        f"{label}.artifact",
        (
            "kind", "platform", "version", "raw_actions_zip_sha256",
            "raw_actions_zip_size", "archive_sha256", "build_metadata_sha256", "files",
        ),
    )
    expected_live = {
        "repository": "FerrumVir/arc-chain",
        "protected_branch": "main",
        "commit": source_commit,
        "workflow_path": ".github/workflows/release-signing-preflight.yml",
        "run_id": run_id,
        "run_attempt": run_attempt,
    }
    if any(live.get(field) != expected for field, expected in expected_live.items()):
        fail(f"{label}.live differs from protected main/run provenance")
    for field in ("workflow_id", "artifact_id", "artifact_size_in_bytes", "api_verified_at_unix"):
        required_int(live[field], f"{label}.live.{field}", minimum=1)
    raw_sha = bare_hash(artifact["raw_actions_zip_sha256"], f"{label}.artifact.raw_actions_zip_sha256")
    archive_sha = bare_hash(artifact["archive_sha256"], f"{label}.artifact.archive_sha256")
    bare_hash(artifact["build_metadata_sha256"], f"{label}.artifact.build_metadata_sha256")
    if (
        artifact["kind"] != kind
        or artifact["platform"] != platform
        or artifact["version"] != "0.8.0"
        or required_int(artifact["raw_actions_zip_size"], f"{label}.artifact.raw_actions_zip_size", minimum=1)
        != live["artifact_size_in_bytes"]
        or live["artifact_digest"] != f"sha256:{raw_sha}"
    ):
        fail(f"{label}.artifact differs from its live server tuple")
    expected_name = (
        f"arc-pretag-{kind}-{platform}-{source_commit}-{run_id}-{run_attempt}-{archive_sha}"
    )
    if live["artifact_name"] != expected_name:
        fail(f"{label}.live.artifact_name does not bind the inner archive digest")
    if (
        api["origin"] != "https://api.github.com"
        or api["anonymous"] is not True
        or api["redirects_followed"] is not False
        or api["max_age_seconds"] != 300
    ):
        fail(f"{label}.api policy differs from the protected public GitHub proof")
    bare_hash(api["curl_sha256"], f"{label}.api.curl_sha256")
    bare_hash(api["ca_bundle_sha256"], f"{label}.api.ca_bundle_sha256")
    responses = api["responses"]
    expected_labels = response_labels
    if not isinstance(responses, list) or len(responses) != len(expected_labels):
        fail(f"{label}.api.responses must contain the ordered four public API proofs")
    response_times: list[int] = []
    for index, (expected_label, raw_response) in enumerate(zip(expected_labels, responses)):
        response = require_keys(
            raw_response,
            f"{label}.api.responses[{index}]",
            ("label", "body_sha256", "response_unix", "request_id", "cache_control", "age"),
        )
        if response["label"] != expected_label:
            fail(f"{label}.api.responses labels are reordered")
        bare_hash(response["body_sha256"], f"{label}.api.responses[{index}].body_sha256")
        response_times.append(
            required_int(response["response_unix"], f"{label}.api.responses[{index}].response_unix", minimum=1)
        )
        request_id = required_string(response["request_id"], f"{label}.api.responses[{index}].request_id")
        cache_control = response["cache_control"]
        if (
            re.fullmatch(r"[A-F0-9:-]{8,128}", request_id) is None
            or not isinstance(cache_control, str)
            or len(cache_control) > 1024
            or any(char in cache_control for char in "\r\n\0")
            or response["age"] != 0
        ):
            fail(f"{label}.api.responses[{index}] cache/request metadata is invalid")
    if live["api_verified_at_unix"] != min(response_times) or max(response_times) - min(response_times) > 360:
        fail(f"{label}.live freshness does not match its bounded API response window")
    files = artifact["files"]
    if kind == "headless":
        suffix = ".exe" if platform == "windows-x86_64" else ""
        expected_files = {
            f"arc-node-{platform}{suffix}", f"arc-cli-{platform}{suffix}", "genesis.toml"
        }
    else:
        expected_files = set(PRETAG_DESKTOP_FILES[platform])
    if not isinstance(files, dict) or set(files) != expected_files:
        fail(f"{label}.artifact.files differs from the fixed {kind}/{platform} payload set")
    for filename, digest in files.items():
        bare_hash(digest, f"{label}.artifact.files[{filename}]")
    return proof


def validate_protected_pretag_window_set(
    value: Any, provenance: Mapping[str, Any], artifacts: Mapping[str, Any]
) -> list[dict[str, Any]]:
    window = require_keys(value, "manifest.provenance.protected_pretag_artifact", ("schema", "groups"))
    if window["schema"] != "arc.protected-pretag-artifact-window-set.v1":
        fail("manifest protected pre-tag window-set schema is unsupported")
    groups = window["groups"]
    if not isinstance(groups, list) or len(groups) != len(PRETAG_GROUPS):
        fail("manifest protected pre-tag window set must contain exactly nine groups")
    run_id = provenance["pretag_workflow_run_id"]
    run_attempt = provenance["pretag_workflow_run_attempt"]
    source_commit = provenance["source_main_commit"]
    seen_artifact_ids: set[int] = set()
    seen_names: set[str] = set()
    shared_initial_api: dict[str, Any] | None = None
    shared_final_api: dict[str, Any] | None = None
    shared_initial_verified_at: int | None = None
    shared_final_verified_at: int | None = None
    for index, ((kind, platform), raw_group) in enumerate(zip(PRETAG_GROUPS, groups)):
        group = require_keys(
            raw_group,
            f"manifest.provenance.protected_pretag_artifact.groups[{index}]",
            ("kind", "platform", "initial", "final"),
        )
        if (group["kind"], group["platform"]) != (kind, platform):
            fail("manifest protected pre-tag groups are missing, duplicated, or reordered")
        initial = _validate_protected_pretag_proof(
            group["initial"], label=f"protected pre-tag {kind}/{platform} initial",
            kind=kind, platform=platform, source_commit=source_commit,
            run_id=run_id, run_attempt=run_attempt,
        )
        final = _validate_protected_pretag_proof(
            group["final"], label=f"protected pre-tag {kind}/{platform} final",
            kind=kind, platform=platform, source_commit=source_commit,
            run_id=run_id, run_attempt=run_attempt,
        )
        initial_live = {key: item for key, item in initial["live"].items() if key != "api_verified_at_unix"}
        final_live = {key: item for key, item in final["live"].items() if key != "api_verified_at_unix"}
        initial_api = {key: item for key, item in initial["api"].items() if key != "responses"}
        final_api = {key: item for key, item in final["api"].items() if key != "responses"}
        if (
            initial_live != final_live
            or initial_api != final_api
            or initial["artifact"] != final["artifact"]
            or final["live"]["api_verified_at_unix"] < initial["live"]["api_verified_at_unix"]
        ):
            fail(f"protected pre-tag {kind}/{platform} final proof changed or predates its initial tuple")
        if index == 0:
            shared_initial_api = initial["api"]
            shared_final_api = final["api"]
            shared_initial_verified_at = initial["live"]["api_verified_at_unix"]
            shared_final_verified_at = final["live"]["api_verified_at_unix"]
        elif (
            initial["api"] != shared_initial_api
            or final["api"] != shared_final_api
            or initial["live"]["api_verified_at_unix"] != shared_initial_verified_at
            or final["live"]["api_verified_at_unix"] != shared_final_verified_at
        ):
            fail("protected pre-tag window groups do not share the exact set-level API roots")
        artifact_id = initial["live"]["artifact_id"]
        artifact_name = initial["live"]["artifact_name"]
        if artifact_id in seen_artifact_ids or artifact_name in seen_names:
            fail("protected pre-tag window set repeats an artifact ID or name")
        seen_artifact_ids.add(artifact_id); seen_names.add(artifact_name)
        artifact_key = pretag_artifact_key(kind, platform)
        if artifacts[artifact_key]["sha256"] != initial["artifact"]["raw_actions_zip_sha256"]:
            fail(f"manifest artifact {artifact_key} differs from protected live provenance")
    linux = groups[0]["initial"]["artifact"]
    for artifact_key, filename in (
        ("binary", "arc-node-linux-x86_64"),
        ("cli", "arc-cli-linux-x86_64"),
        ("genesis", "genesis.toml"),
    ):
        if artifacts[artifact_key]["sha256"] != linux["files"][filename]:
            fail(f"manifest {artifact_key} differs from protected Linux headless payload")
    if artifacts["build_metadata"]["sha256"] != linux["build_metadata_sha256"]:
        fail("manifest build metadata differs from protected Linux headless provenance")
    return groups


def parse_listen(value: Any, field: str) -> tuple[str, int]:
    raw = required_string(value, field)
    if raw.count(":") != 1:
        fail(f"{field} must be HOST:PORT with an IPv4 or DNS host")
    host, port_raw = raw.rsplit(":", 1)
    try:
        port = int(port_raw)
    except ValueError:
        fail(f"{field} has an invalid port")
    if not host or not 1 <= port <= 65535 or not SAFE_HOST_RE.fullmatch(host):
        fail(f"{field} must contain a safe host and port")
    return host, port


def is_loopback_host(host: str) -> bool:
    if host == "localhost":
        return True
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return False


def validate_url(value: Any, field: str, mode: str) -> str:
    raw = required_string(value, field)
    parsed = urllib.parse.urlsplit(raw)
    if parsed.username or parsed.password or parsed.query or parsed.fragment or parsed.path not in {"", "/"}:
        fail(f"{field} must be an origin URL with no credentials, query, fragment, or path")
    if mode == "production":
        if parsed.scheme != "https" or parsed.port not in {None, 443}:
            fail(f"{field} must use HTTPS on the standard port in production")
    else:
        if parsed.scheme != "http" or not parsed.hostname or not is_loopback_host(parsed.hostname):
            fail(f"{field} must be a loopback HTTP origin for a local rehearsal")
    if not parsed.hostname:
        fail(f"{field} has no hostname")
    return raw.rstrip("/")


def validate_manifest(
    value: Any, *, allow_provisional_installed_key_proof: bool = False
) -> dict[str, Any]:
    manifest = require_keys(
        value,
        "manifest",
        ("schema", "rollout_id", "mode", "chain", "artifacts", "checks", "gateway", "validators"),
        ("archive", "provenance"),
    )
    if manifest["schema"] != SCHEMA:
        fail(f"manifest.schema must be {SCHEMA}")
    rollout_id = required_string(manifest["rollout_id"], "manifest.rollout_id")
    if not SAFE_ID_RE.fullmatch(rollout_id):
        fail("manifest.rollout_id must be a lowercase, DNS-safe identifier")
    mode = manifest["mode"]
    if mode not in {"local", "production"}:
        fail("manifest.mode must be local or production")

    if mode == "production":
        provenance_required = (
            "source_main_commit",
            "pretag_repository",
            "pretag_version",
            "pretag_workflow_run_id",
            "pretag_workflow_run_attempt",
            "protected_pretag_artifact",
            "production_input_stage_manifest_sha256",
            "validator_key_receipt_chain",
            "freeze_plan_sidecar_sha256",
            "offline_stop_verification",
        )
        provenance = require_keys(
            manifest.get("provenance"),
            "manifest.provenance",
            provenance_required
            if allow_provisional_installed_key_proof
            else (*provenance_required, "validator_installed_key_proof"),
            ("validator_installed_key_proof",)
            if allow_provisional_installed_key_proof
            else (),
        )
        if not isinstance(provenance["source_main_commit"], str) or not re.fullmatch(
            r"[0-9a-f]{40}", provenance["source_main_commit"]
        ):
            fail("manifest.provenance.source_main_commit must be one full lowercase Git SHA")
        if provenance["pretag_repository"] != "FerrumVir/arc-chain":
            fail("manifest.provenance.pretag_repository must be FerrumVir/arc-chain")
        if provenance["pretag_version"] != "0.8.0":
            fail("manifest.provenance.pretag_version must be the v0.8.0 recovery release")
        required_int(
            provenance["pretag_workflow_run_id"],
            "manifest.provenance.pretag_workflow_run_id",
            minimum=1,
        )
        bare_hash(
            provenance["production_input_stage_manifest_sha256"],
            "manifest.provenance.production_input_stage_manifest_sha256",
        )
        required_int(
            provenance["pretag_workflow_run_attempt"],
            "manifest.provenance.pretag_workflow_run_attempt",
            minimum=1,
        )
        if not isinstance(provenance["freeze_plan_sidecar_sha256"], str) or not LOWER_HEX_32_RE.fullmatch(
            provenance["freeze_plan_sidecar_sha256"]
        ):
            fail(
                "manifest.provenance.freeze_plan_sidecar_sha256 must be exactly 64 lowercase hexadecimal characters"
            )
        stop_verification = require_keys(
            provenance["offline_stop_verification"],
            "manifest.provenance.offline_stop_verification",
            (
                "schema", "source_main_commit", "freeze_plan_sha256", "capture_id",
                "remote_helper_sha256", "remote_helper_path",
                "offline_stop_evidence_sha256", "ssh_known_hosts_sha256", "ssh_path",
                "ssh_sha256", "challenge", "started_at", "completed_at", "duration_ms", "nodes",
            ),
        )
        if stop_verification["schema"] != "arc.recovery.offline-stop-remote-verification.v1":
            fail("manifest.provenance.offline_stop_verification.schema is unsupported")
        for key in (
            "freeze_plan_sha256", "capture_id", "remote_helper_sha256",
            "offline_stop_evidence_sha256", "ssh_known_hosts_sha256", "ssh_sha256", "challenge",
        ):
            if not isinstance(stop_verification[key], str) or not LOWER_HEX_32_RE.fullmatch(stop_verification[key]):
                fail(f"manifest.provenance.offline_stop_verification.{key} must be one lowercase hash")
        if stop_verification["source_main_commit"] != provenance["source_main_commit"]:
            fail("manifest offline-stop verification source commit differs from provenance")
        if stop_verification["ssh_path"] != "/usr/bin/ssh":
            fail("manifest offline-stop verification must use /usr/bin/ssh")
        helper_path = f"/root/.arc-recovery-helpers/{stop_verification['remote_helper_sha256']}/archive-node.sh"
        if stop_verification["remote_helper_path"] != helper_path:
            fail("manifest offline-stop verification helper path is not hash-pinned")
        for key in ("started_at", "completed_at"):
            if not isinstance(stop_verification[key], str) or not re.fullmatch(
                r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", stop_verification[key]
            ):
                fail(f"manifest offline-stop verification {key} is not canonical UTC")
        required_int(
            stop_verification["duration_ms"],
            "manifest.provenance.offline_stop_verification.duration_ms",
            maximum=120_000,
        )
        stop_nodes = stop_verification["nodes"]
        if not isinstance(stop_nodes, list) or len(stop_nodes) != REQUIRED_VALIDATORS:
            fail("manifest offline-stop verification must contain exactly six nodes")
        for index, ((expected_node, expected_host), raw_stop_node) in enumerate(
            zip(PRODUCTION_FLEET, stop_nodes)
        ):
            stop_node = require_keys(
                raw_stop_node,
                f"manifest.provenance.offline_stop_verification.nodes[{index}]",
                ("node", "host", "status", "status_sha256"),
            )
            if (stop_node["node"], stop_node["host"]) != (expected_node, expected_host):
                fail("manifest offline-stop verification topology differs from the fixed fleet")
            if not isinstance(stop_node["status_sha256"], str) or not LOWER_HEX_32_RE.fullmatch(stop_node["status_sha256"]):
                fail("manifest offline-stop verification status hash is malformed")
            status = require_keys(
                stop_node["status"],
                f"manifest.provenance.offline_stop_verification.nodes[{index}].status",
                (
                    "schema", "capture_id", "node", "host", "freeze_plan_sha256",
                    "validator_address", "stake", "stopped", "restart_fenced", "stop_schema",
                    "stop_complete_sha256", "stop_files_sha256", "challenge",
                ),
            )
            if status["schema"] != "arc.recovery.offline-stop-challenged-status.v1":
                fail("manifest offline-stop challenged status schema is unsupported")
            if status["stop_schema"] != "arc.recovery.offline-stop.v4" or status["stopped"] is not True or status["restart_fenced"] is not True:
                fail("manifest offline-stop challenged status is not stopped and fenced")
            if (status["node"], status["host"]) != (expected_node, expected_host):
                fail("manifest offline-stop challenged status topology differs")
            for key in ("capture_id", "freeze_plan_sha256", "validator_address", "stop_complete_sha256", "stop_files_sha256", "challenge"):
                if not isinstance(status[key], str) or not LOWER_HEX_32_RE.fullmatch(status[key]):
                    fail(f"manifest offline-stop challenged status {key} is malformed")
            required_int(status["stake"], "manifest offline-stop challenged status stake", minimum=1)
            for key in ("capture_id", "freeze_plan_sha256", "challenge"):
                if status[key] != stop_verification[key]:
                    fail(f"manifest offline-stop challenged status {key} differs from its envelope")
            if sha256_bytes(canonical_bytes(status)) != stop_node["status_sha256"]:
                fail("manifest offline-stop challenged status hash is not reproducible")
        archive = require_keys(
            manifest.get("archive"),
            "manifest.archive",
            (
                "freeze_plan_sha256",
                "capture_id",
                "destination",
                "allow_unbound_legacy_wal",
                "archive_orchestrator_sha256",
                "remote_helper_sha256",
                "rollout_tool_sha256",
                "rollout_schema_sha256",
                "complete_sha256",
                "archive_manifest_sha256",
                "sha256sums_sha256",
                "prearchive_rollout_sha256",
            ),
        )
        for key in (
            "freeze_plan_sha256",
            "capture_id",
            "archive_orchestrator_sha256",
            "remote_helper_sha256",
            "rollout_tool_sha256",
            "rollout_schema_sha256",
            "complete_sha256",
            "archive_manifest_sha256",
            "sha256sums_sha256",
            "prearchive_rollout_sha256",
        ):
            if not isinstance(archive[key], str) or not LOWER_HEX_32_RE.fullmatch(archive[key]):
                fail(f"manifest.archive.{key} must be exactly 64 lowercase hexadecimal characters")
        expected_capture = capture_id_for_freeze_plan_hash(archive["freeze_plan_sha256"])
        if archive["capture_id"] != expected_capture:
            fail("manifest.archive.capture_id is not derived from the exact freeze-plan hash")
        validate_drive_remote(archive["destination"], "manifest.archive.destination")
        if not archive["destination"].endswith(f"/captures/{archive['capture_id']}"):
            fail("manifest.archive.destination must be the exact capture-scoped path")
        if not isinstance(archive["allow_unbound_legacy_wal"], bool):
            fail("manifest.archive.allow_unbound_legacy_wal must be a boolean")
        finalization_values = [archive[field] for field in ARCHIVE_FINALIZATION_FIELDS]
        zeros = "0" * 64
        if all(value == zeros for value in finalization_values):
            pass  # exact prearchive form, sealed before any remote archive exists
        elif any(value == zeros for value in finalization_values):
            fail("manifest.archive finalization roots must be either all-zero prearchive or all nonzero")
        elif prearchive_projection_digest(manifest) != archive["prearchive_rollout_sha256"]:
            fail(
                "final manifest differs from its prearchive manifest outside the four archive finalization roots"
            )
    elif "archive" in manifest or "provenance" in manifest:
        fail("local rehearsals must not contain manifest.archive or manifest.provenance")

    chain_fields = (
        "chain_id",
        "genesis_hash",
        "protocol_version",
        "recovery_epoch",
        "validator_set_id",
        "source_height",
        "legacy_public_max_height",
        "source_consensus_round",
        "created_at_unix_ms",
        "source_block_hash",
        "source_state_root",
        "transition_height",
        "transition_block_hash",
        "full_state_root",
        "recovery_domain",
        "approved_checkpoint_manifest_hash",
    )
    production_legacy_fields = (
        "legacy_maintenance_evidence_bundle_sha256",
        "legacy_maintenance_boundary_sha256",
        "legacy_late_fork_source_set_sha256",
        "legacy_observed_cutoff_height",
        "legacy_continuity_safety_margin",
        "legacy_global_absence_claimed",
        "legacy_official_origins",
        "legacy_reopening_policy",
        "legacy_late_fork_circuit",
        "legacy_quarantine_threat_model",
    )
    chain = require_keys(
        manifest["chain"],
        "manifest.chain",
        chain_fields + (production_legacy_fields if mode == "production" else ()),
    )
    required_string(chain["chain_id"], "manifest.chain.chain_id")
    version = required_string(chain["protocol_version"], "manifest.chain.protocol_version")
    if not re.fullmatch(r"3\.\d+\.\d+", version):
        fail("manifest.chain.protocol_version must be protocol v3")
    required_int(chain["recovery_epoch"], "manifest.chain.recovery_epoch", minimum=1)
    required_int(chain["validator_set_id"], "manifest.chain.validator_set_id", minimum=1)
    source_height = required_int(chain["source_height"], "manifest.chain.source_height")
    legacy_public_max_height = required_int(
        chain["legacy_public_max_height"],
        "manifest.chain.legacy_public_max_height",
    )
    if legacy_public_max_height < source_height:
        fail("manifest.chain.legacy_public_max_height must be at least source_height")
    if mode == "production":
        bare_hash(
            chain["legacy_maintenance_evidence_bundle_sha256"],
            "manifest.chain.legacy_maintenance_evidence_bundle_sha256",
        )
        bare_hash(
            chain["legacy_maintenance_boundary_sha256"],
            "manifest.chain.legacy_maintenance_boundary_sha256",
        )
        bare_hash(
            chain["legacy_late_fork_source_set_sha256"],
            "manifest.chain.legacy_late_fork_source_set_sha256",
        )
        observed_cutoff = required_int(
            chain["legacy_observed_cutoff_height"],
            "manifest.chain.legacy_observed_cutoff_height",
        )
        if chain["legacy_continuity_safety_margin"] != LEGACY_CONTINUITY_SAFETY_MARGIN:
            fail(
                "manifest.chain.legacy_continuity_safety_margin must be exactly "
                f"{LEGACY_CONTINUITY_SAFETY_MARGIN}"
            )
        if legacy_public_max_height != observed_cutoff + LEGACY_CONTINUITY_SAFETY_MARGIN:
            fail(
                "manifest.chain.legacy_public_max_height must be exactly "
                "legacy_observed_cutoff_height + 128"
            )
        if chain["legacy_global_absence_claimed"] is not False:
            fail("manifest.chain.legacy_global_absence_claimed must be false")
        if chain["legacy_official_origins"] != list(LEGACY_OFFICIAL_ORIGINS):
            fail("manifest.chain.legacy_official_origins must be the exact ordered six")
        if chain["legacy_reopening_policy"] != LEGACY_REOPENING_POLICY:
            fail("manifest.chain.legacy_reopening_policy differs from the sealed policy")
        if chain["legacy_late_fork_circuit"] != LEGACY_LATE_FORK_CIRCUIT:
            fail("manifest.chain.legacy_late_fork_circuit differs from the sealed circuit")
        if chain["legacy_quarantine_threat_model"] != LEGACY_QUARANTINE_THREAT_MODEL:
            fail(
                "manifest.chain.legacy_quarantine_threat_model differs from the sealed threat model"
            )
    required_int(chain["source_consensus_round"], "manifest.chain.source_consensus_round")
    required_int(chain["created_at_unix_ms"], "manifest.chain.created_at_unix_ms", minimum=1)
    transition_height = required_int(chain["transition_height"], "manifest.chain.transition_height", minimum=1)
    if transition_height != source_height + 1:
        fail("manifest.chain.transition_height must be exactly source_height + 1")
    for key in (
        "source_block_hash",
        "source_state_root",
        "transition_block_hash",
        "full_state_root",
        "genesis_hash",
        "recovery_domain",
        "approved_checkpoint_manifest_hash",
    ):
        bare_hash(chain[key], f"manifest.chain.{key}")

    artifact_names = (
        "binary",
        "genesis",
        "checkpoint",
        "legacy_validator_set",
        *(
            (
                "cli",
                "build_metadata",
                "pretag_artifact_input_set",
                "pretag_initial_live_provenance_set",
                "production_input_stage_manifest",
                "validator_vault_restore_receipt",
                "validator_key_install_receipt",
                "validator_public_keys",
                "legacy_public_height_receipt",
                "legacy_maintenance_evidence_bundle",
                "legacy_maintenance_evidence_bundle_sidecar",
                "legacy_maintenance_boundary",
                "legacy_maintenance_boundary_sidecar",
                "legacy_late_fork_source_set",
                "legacy_late_fork_source_set_sidecar",
                "legacy_late_fork_interlock_tool",
                "offline_stop_evidence",
                "offline_stop_evidence_sidecar",
                "ssh_known_hosts",
                "reward_probe",
                "source_snapshot",
                "source_wal",
                "caddy",
                *(pretag_artifact_key(kind, platform) for kind, platform in PRETAG_GROUPS),
            )
            if mode == "production"
            else ()
        ),
    )
    artifacts = require_keys(manifest["artifacts"], "manifest.artifacts", artifact_names)
    for key in artifact_names:
        validate_artifact(artifacts[key], f"manifest.artifacts.{key}")
    if mode == "production":
        validate_protected_pretag_window_set(
            provenance["protected_pretag_artifact"], provenance, artifacts
        )
        if (
            artifacts["production_input_stage_manifest"]["sha256"]
            != provenance["production_input_stage_manifest_sha256"]
        ):
            fail("production input stage manifest artifact differs from provenance")
        stop_verification = provenance["offline_stop_verification"]
        for verification_key, archive_key in (
            ("freeze_plan_sha256", "freeze_plan_sha256"),
            ("capture_id", "capture_id"),
            ("remote_helper_sha256", "remote_helper_sha256"),
        ):
            if stop_verification[verification_key] != archive[archive_key]:
                fail(
                    f"manifest offline-stop verification {verification_key} differs from archive provenance"
                )
        if stop_verification["offline_stop_evidence_sha256"] != artifacts["offline_stop_evidence"]["sha256"]:
            fail("manifest offline-stop verification differs from the sealed offline-stop artifact")
        if stop_verification["ssh_known_hosts_sha256"] != artifacts["ssh_known_hosts"]["sha256"]:
            fail("manifest offline-stop verification differs from the sealed SSH trust anchor")
        if (
            chain["legacy_maintenance_boundary_sha256"]
            != artifacts["legacy_maintenance_boundary"]["sha256"]
        ):
            fail("manifest legacy maintenance boundary chain root differs from its artifact")
        if (
            chain["legacy_maintenance_evidence_bundle_sha256"]
            != artifacts["legacy_maintenance_evidence_bundle"]["sha256"]
        ):
            fail("manifest legacy maintenance evidence-bundle chain root differs from its artifact")
        if (
            chain["legacy_late_fork_source_set_sha256"]
            != artifacts["legacy_late_fork_source_set"]["sha256"]
        ):
            fail("manifest legacy late-fork source-set chain root differs from its artifact")
    if mode == "production" and artifacts["caddy"]["sha256"] != CADDY_LINUX_AMD64_SHA256:
        fail(
            "manifest.artifacts.caddy.sha256 must pin the reviewed Caddy "
            f"{CADDY_VERSION} linux-amd64 binary"
        )

    checks = require_keys(
        manifest["checks"],
        "manifest.checks",
        (
            "startup_timeout_seconds",
            "convergence_timeout_seconds",
            "observation_seconds",
            "restart_timeout_seconds",
            "poll_interval_seconds",
            "min_height_advance",
            "reward",
        ),
    )
    required_int(checks["startup_timeout_seconds"], "manifest.checks.startup_timeout_seconds", minimum=10, maximum=3600)
    required_int(checks["convergence_timeout_seconds"], "manifest.checks.convergence_timeout_seconds", minimum=10, maximum=7200)
    required_int(checks["observation_seconds"], "manifest.checks.observation_seconds", minimum=1, maximum=3600)
    required_int(checks["restart_timeout_seconds"], "manifest.checks.restart_timeout_seconds", minimum=10, maximum=3600)
    required_int(checks["poll_interval_seconds"], "manifest.checks.poll_interval_seconds", minimum=1, maximum=30)
    required_int(checks["min_height_advance"], "manifest.checks.min_height_advance", minimum=1, maximum=10000)
    reward = require_keys(
        checks["reward"],
        "manifest.checks.reward",
        ("mode", "expect_protocol_active", "expect_issuance_ready"),
        ("probe_argv", "probe_sha256", "receipts", "expected_reward_base"),
    )
    if reward["mode"] not in {"policy", "receipt"}:
        fail("manifest.checks.reward.mode must be policy or receipt")
    if not isinstance(reward["expect_protocol_active"], bool):
        fail("manifest.checks.reward.expect_protocol_active must be boolean")
    if not isinstance(reward["expect_issuance_ready"], bool):
        fail("manifest.checks.reward.expect_issuance_ready must be boolean")
    if reward["expect_issuance_ready"] and not reward["expect_protocol_active"]:
        fail("issuance cannot be ready while the reward protocol is inactive")
    if reward["mode"] == "receipt":
        if reward["expect_issuance_ready"] is not True:
            fail("receipt mode requires expect_issuance_ready=true")
        fixed = "receipts" in reward
        probe = "probe_argv" in reward
        if fixed == probe:
            fail("receipt mode requires exactly one of two fixed receipts or probe_argv")
        if fixed:
            receipts = reward["receipts"]
            if not isinstance(receipts, list) or len(receipts) != 2:
                fail("manifest.checks.reward.receipts must contain exactly two evidence objects")
            evidence = [ReceiptEvidence.from_value(item) for item in receipts]
            validate_distinct_receipt_evidence(evidence)
        else:
            argv = reward["probe_argv"]
            if not isinstance(argv, list) or not argv or not all(isinstance(arg, str) and arg for arg in argv):
                fail("manifest.checks.reward.probe_argv must be a non-empty string array")
            absolute_path(argv[0], "manifest.checks.reward.probe_argv[0]")
            if not isinstance(reward.get("probe_sha256"), str) or not LOWER_HEX_32_RE.fullmatch(reward["probe_sha256"]):
                fail("manifest.checks.reward.probe_sha256 must pin the executable probe")
        expected_reward_base = required_int(
            reward.get("expected_reward_base"),
            "manifest.checks.reward.expected_reward_base",
            minimum=1,
        )
        if expected_reward_base != COMMUNITY_REWARD_BASE:
            fail(
                "manifest.checks.reward.expected_reward_base must equal the protocol reward "
                f"{COMMUNITY_REWARD_BASE} base units (2.5 ARC)"
            )
    else:
        forbidden = set(reward) & {"probe_argv", "probe_sha256", "receipts", "expected_reward_base"}
        if forbidden:
            fail(f"policy reward mode cannot contain: {', '.join(sorted(forbidden))}")

    gateway = require_keys(
        manifest["gateway"],
        "manifest.gateway",
        ("mode",),
        ("acme_email", "public_get_paths", "public_post_paths"),
    )
    if mode == "local":
        if gateway != {"mode": "none"}:
            fail("local rehearsals require gateway.mode=none and no gateway side effects")
    else:
        if gateway["mode"] != "caddy-nginx":
            fail("production requires gateway.mode=caddy-nginx")
        email = required_string(gateway.get("acme_email"), "manifest.gateway.acme_email")
        if not re.fullmatch(r"[^@\s]+@[^@\s]+\.[^@\s]+", email):
            fail("manifest.gateway.acme_email is invalid")
        for key, expected in (("public_get_paths", DEFAULT_PUBLIC_GET_PATHS), ("public_post_paths", DEFAULT_PUBLIC_POST_PATHS)):
            paths = gateway.get(key)
            if paths != list(expected):
                fail(f"manifest.gateway.{key} must exactly match the sealed protocol-v3 allowlist")

    validators = manifest["validators"]
    if not isinstance(validators, list) or len(validators) != REQUIRED_VALIDATORS:
        fail(f"manifest.validators must contain exactly {REQUIRED_VALIDATORS} validators")
    names: set[str] = set()
    addresses: set[str] = set()
    data_dirs: set[str] = set()
    rpc_urls: set[str] = set()
    rpc_listens: set[str] = set()
    p2p_advertise: set[str] = set()
    remote_roots: set[str] = set()
    service_names: set[str] = set()
    hosts: set[str] = set()
    stakes: list[int] = []
    shard_coverage = [0] * CANONICAL_MODEL_LAYERS
    for index, raw_node in enumerate(validators):
        field = f"manifest.validators[{index}]"
        common = (
            "name",
            "address",
            "stake",
            "key_file",
            "rpc_listen",
            "rpc_url",
            "p2p_port",
            "p2p_advertise",
            "data_dir",
            "extra_args",
        )
        production = (
            "host",
            "ssh_user",
            "remote_root",
            "service_user",
            "service_name",
            "model_path",
            "model_sha256",
            "model_size_bytes",
            "shard_ranges",
        )
        node = require_keys(raw_node, field, common + (production if mode == "production" else ()))
        name = required_string(node["name"], f"{field}.name")
        if not SAFE_ID_RE.fullmatch(name) or name in names:
            fail(f"{field}.name must be unique and DNS-safe")
        names.add(name)
        address = bare_hash(node["address"], f"{field}.address")
        if address in addresses:
            fail(f"{field}.address is duplicated")
        addresses.add(address)
        stake = required_int(node["stake"], f"{field}.stake", minimum=1)
        stakes.append(stake)
        absolute_path(node["key_file"], f"{field}.key_file")
        listen_host, listen_port = parse_listen(node["rpc_listen"], f"{field}.rpc_listen")
        if not is_loopback_host(listen_host):
            fail(f"{field}.rpc_listen must bind loopback, never a public interface")
        if mode == "local" and node["rpc_listen"] in rpc_listens:
            fail(f"{field}.rpc_listen is duplicated")
        rpc_listens.add(node["rpc_listen"])
        rpc_url = validate_url(node["rpc_url"], f"{field}.rpc_url", mode)
        if rpc_url in rpc_urls:
            fail(f"{field}.rpc_url is duplicated")
        rpc_urls.add(rpc_url)
        if mode == "local" and listen_port != 9090:
            fail(f"{field}.rpc_listen must use port 9090 so the current peer RPC derivation can be rehearsed")
        p2p_port = required_int(node["p2p_port"], f"{field}.p2p_port", minimum=1024, maximum=65535)
        advertise_host, advertise_port = parse_listen(node["p2p_advertise"], f"{field}.p2p_advertise")
        if advertise_port != p2p_port:
            fail(f"{field}.p2p_advertise port must equal p2p_port")
        if node["p2p_advertise"] in p2p_advertise:
            fail(f"{field}.p2p_advertise is duplicated")
        p2p_advertise.add(node["p2p_advertise"])
        data_dir = absolute_path(node["data_dir"], f"{field}.data_dir")
        if mode == "local" and data_dir in data_dirs:
            fail(f"{field}.data_dir is duplicated")
        data_dirs.add(data_dir)
        extra_args = node["extra_args"]
        if not isinstance(extra_args, list) or not all(isinstance(arg, str) and arg and "\x00" not in arg for arg in extra_args):
            fail(f"{field}.extra_args must be a string array")
        for arg in extra_args:
            protected = arg.split("=", 1)[0]
            if protected in PROTECTED_FLAGS:
                fail(f"{field}.extra_args cannot override protected flag {protected}")
        if mode == "production":
            host = required_string(node["host"], f"{field}.host")
            if not SAFE_HOST_RE.fullmatch(host) or host in hosts:
                fail(f"{field}.host is unsafe or duplicated")
            hosts.add(host)
            if (name, host) != PRODUCTION_FLEET[index]:
                fail(f"{field} node/host differs from the fixed production fleet")
            if advertise_host != host:
                fail(f"{field}.p2p_advertise host must equal the production host")
            for key in ("ssh_user", "service_user"):
                token = required_string(node[key], f"{field}.{key}")
                if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_-]{0,31}", token):
                    fail(f"{field}.{key} is unsafe")
            if node["ssh_user"] != "root" or node["service_user"] != "root":
                fail(f"{field} currently requires audited root SSH/service ownership for low ports and mode-0600 keys")
            remote_root = absolute_path(node["remote_root"], f"{field}.remote_root")
            systemd_literal(remote_root, f"{field}.remote_root")
            remote_roots.add(remote_root)
            if paths_overlap(remote_root, data_dir):
                fail(f"{field}.remote_root and data_dir must be disjoint, non-nested paths")
            if remote_root != "/opt/arc/recovery-v3":
                fail(
                    f"{field}.remote_root must be the audited /opt/arc/recovery-v3 "
                    "runtime outside ProtectHome"
                )
            service_name = required_string(node["service_name"], f"{field}.service_name")
            if not re.fullmatch(r"arc-node-v3-[a-z0-9-]+\.service", service_name):
                fail(f"{field}.service_name must be an arc-node-v3-*.service")
            service_names.add(service_name)
            try:
                approved_ip = str(ipaddress.ip_address(host))
            except ValueError:
                fail(f"{field}.host must be the validator's literal public IP")
            if rpc_url != f"https://{approved_ip}":
                fail(f"{field}.rpc_url must be exactly https://{approved_ip}")
            model_path = absolute_path(node["model_path"], f"{field}.model_path")
            for systemd_value, systemd_field in (
                (data_dir, f"{field}.data_dir"),
                (node["key_file"], f"{field}.key_file"),
                (model_path, f"{field}.model_path"),
                *((arg, f"{field}.extra_args[{index}]") for index, arg in enumerate(extra_args)),
            ):
                systemd_literal(systemd_value, systemd_field)
            if node["model_sha256"] != CANONICAL_MODEL_SHA256:
                fail(
                    f"{field}.model_sha256 must pin the canonical v0.8 Llama-2-7B artifact"
                )
            if node["model_size_bytes"] != CANONICAL_MODEL_SIZE_BYTES:
                fail(
                    f"{field}.model_size_bytes must pin the exact reviewed 4,081,004,224-byte model"
                )
            ranges = node["shard_ranges"]
            if not isinstance(ranges, list) or not ranges:
                fail(f"{field}.shard_ranges must be a non-empty array")
            normalized_ranges: list[tuple[int, int]] = []
            for range_index, layer_range in enumerate(ranges):
                range_field = f"{field}.shard_ranges[{range_index}]"
                if (
                    not isinstance(layer_range, list)
                    or len(layer_range) != 2
                    or not all(isinstance(bound, int) and not isinstance(bound, bool) for bound in layer_range)
                ):
                    fail(f"{range_field} must be [start, end] integer bounds")
                start, end = layer_range
                if start < 0 or start >= end or end > CANONICAL_MODEL_LAYERS:
                    fail(
                        f"{range_field} must be non-empty and inside [0, {CANONICAL_MODEL_LAYERS})"
                    )
                normalized_ranges.append((start, end))
            normalized_ranges.sort()
            for previous, current in zip(normalized_ranges, normalized_ranges[1:]):
                if current[0] < previous[1]:
                    fail(f"{field}.shard_ranges overlap within one validator")
            held_layers = sum(end - start for start, end in normalized_ranges)
            if not MIN_LAYERS_PER_VALIDATOR <= held_layers <= MAX_LAYERS_PER_VALIDATOR:
                fail(
                    f"{field}.shard_ranges must hold {MIN_LAYERS_PER_VALIDATOR}..{MAX_LAYERS_PER_VALIDATOR} layers, found {held_layers}"
                )
            for start, end in normalized_ranges:
                for layer in range(start, end):
                    shard_coverage[layer] += 1

    total_stake = sum(stakes)
    if any((total_stake - stake) * 3 <= total_stake * 2 for stake in stakes):
        fail("validator stakes do not preserve strict >2/3 quorum during every one-node restart")
    if mode == "production":
        validate_validator_key_receipt_chain_value(
            provenance["validator_key_receipt_chain"], manifest
        )
        installed_key_proof = provenance.get("validator_installed_key_proof")
        if installed_key_proof is not None:
            validate_validator_installed_key_proof(installed_key_proof, manifest)
    if mode == "production" and any(
        replicas != REQUIRED_SHARD_REPLICATION for replicas in shard_coverage
    ):
        bad = [
            f"{layer}:{replicas}"
            for layer, replicas in enumerate(shard_coverage)
            if replicas != REQUIRED_SHARD_REPLICATION
        ]
        fail(
            "production shard_ranges must provide exact 3x coverage for every model layer; "
            + ", ".join(bad)
        )
    return manifest


def verify_artifacts(manifest: Mapping[str, Any]) -> None:
    for name, artifact in manifest["artifacts"].items():
        path = Path(artifact["path"])
        if not path.is_file() or path.is_symlink():
            fail(f"artifact {name} is missing, not regular, or a symlink: {path}")
        actual = sha256_file(path)
        if actual != artifact["sha256"]:
            fail(f"artifact {name} sha256 mismatch: expected {artifact['sha256']}, got {actual}")
    reward = manifest["checks"]["reward"]
    if "probe_argv" in reward:
        probe = Path(reward["probe_argv"][0])
        if not probe.is_file() or probe.is_symlink():
            fail(f"reward probe is missing, not regular, or a symlink: {probe}")
        actual = sha256_file(probe)
        if actual != reward["probe_sha256"]:
            fail(f"reward probe sha256 mismatch: expected {reward['probe_sha256']}, got {actual}")
    if manifest["mode"] == "production":
        stage_rows, stage_payloads = verify_production_input_stage(manifest)
        verify_legacy_maintenance_stage_payloads(manifest, stage_rows, stage_payloads)
        verify_protected_pretag_stage_payloads(manifest, stage_payloads)


def verify_production_input_stage(
    manifest: Mapping[str, Any],
) -> tuple[dict[str, dict[str, Any]], dict[str, bytes]]:
    """Verify the builder's one create-only semantic input tree in full."""

    artifacts = manifest["artifacts"]
    artifact_paths = [Path(item["path"]) for item in artifacts.values()]
    if not artifact_paths:
        fail("production input stage has no artifacts")
    try:
        common = Path(os.path.commonpath(tuple(os.fspath(path) for path in artifact_paths)))
    except ValueError:
        fail("production artifacts do not share one private stage root")
    if not common.is_absolute() or common == Path(common.anchor):
        fail("production input stage root is missing or too broad")
    stage_root = common

    current = Path(stage_root.anchor)
    for component in stage_root.parts[1:]:
        current /= component
        try:
            details = current.lstat()
        except OSError as error:
            fail(f"production input stage ancestry is unavailable: {error}")
        if stat.S_ISLNK(details.st_mode) or not stat.S_ISDIR(details.st_mode):
            fail("production input stage ancestry contains a symlink or non-directory")
        if details.st_uid not in {0, os.geteuid()} or stat.S_IMODE(details.st_mode) & 0o022:
            fail("production input stage ancestry has an unreviewed owner or write boundary")
    root_details = stage_root.lstat()
    if root_details.st_uid != os.geteuid() or stat.S_IMODE(root_details.st_mode) != 0o500:
        fail("production input stage root must be operator-owned mode 0500")
    for child in ("source", "private"):
        details = (stage_root / child).lstat()
        if (
            stat.S_ISLNK(details.st_mode)
            or not stat.S_ISDIR(details.st_mode)
            or details.st_uid != os.geteuid()
            or stat.S_IMODE(details.st_mode) != 0o500
        ):
            fail(f"production input stage {child} directory identity differs")

    def locked_file(
        path: Path, label: str, maximum: int, *, retain_payload: bool
    ) -> tuple[bytes | None, os.stat_result, str]:
        try:
            descriptor = os.open(
                path,
                os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0),
            )
        except OSError as error:
            fail(f"cannot open {label} through the no-follow boundary: {error}")
        try:
            before = os.fstat(descriptor)
            visible = os.lstat(path)
            identity = lambda value: (
                value.st_dev,
                value.st_ino,
                value.st_mode,
                value.st_uid,
                value.st_gid,
                value.st_nlink,
                value.st_size,
                value.st_mtime_ns,
            )
            if (
                not stat.S_ISREG(before.st_mode)
                or stat.S_ISLNK(visible.st_mode)
                or identity(before) != identity(visible)
                or before.st_uid != os.geteuid()
                or before.st_nlink != 1
                or before.st_size <= 0
                or before.st_size > maximum
            ):
                fail(f"{label} owner/type/link/size identity differs")
            digest = hashlib.sha256()
            payload = bytearray() if retain_payload else None
            total = 0
            remaining = maximum + 1
            while remaining:
                chunk = os.read(descriptor, min(1024 * 1024, remaining))
                if not chunk:
                    break
                total += len(chunk)
                digest.update(chunk)
                if payload is not None:
                    payload.extend(chunk)
                remaining -= len(chunk)
            after = os.fstat(descriptor)
            if total != before.st_size or identity(before) != identity(after):
                fail(f"{label} changed while it was read")
            return bytes(payload) if payload is not None else None, before, digest.hexdigest()
        finally:
            os.close(descriptor)

    stage_payload, stage_details, stage_digest = locked_file(
        stage_root / "STAGE-MANIFEST.json",
        "production stage manifest",
        4 * 1024 * 1024,
        retain_payload=True,
    )
    assert stage_payload is not None
    if stat.S_IMODE(stage_details.st_mode) != 0o400:
        fail("production stage manifest must be mode 0400")
    try:
        stage = json.loads(stage_payload)
    except (UnicodeError, json.JSONDecodeError):
        fail("production stage manifest is invalid JSON")
    if stage_payload != canonical_bytes(stage) or not isinstance(stage, dict):
        fail("production stage manifest is not one canonical JSON object")
    if set(stage) != {"schema", "source_main_commit", "files"}:
        fail("production stage manifest has missing or unknown fields")
    if stage["schema"] != "arc.recovery.production-input-stage.v1":
        fail("production stage manifest schema is unsupported")
    if stage["source_main_commit"] != manifest["provenance"]["source_main_commit"]:
        fail("production stage source commit differs from rollout provenance")
    rows = stage["files"]
    if not isinstance(rows, list):
        fail("production stage files must be an array")
    artifact_stage_names = {
        name: "offline_stop_sidecar" if name == "offline_stop_evidence_sidecar" else name
        for name in artifacts
        if name != "production_input_stage_manifest"
    }
    expected_names = set(artifact_stage_names.values()) | {
        "freeze_plan",
        "freeze_plan_sidecar",
        "offline_stop_sidecar",
        "ssh_identity",
    }
    if len(rows) != len(expected_names):
        fail("production stage file inventory cardinality differs")
    by_name: dict[str, dict[str, Any]] = {}
    payload_by_name: dict[str, bytes] = {}
    seen_paths: set[str] = set()
    retained_names = {
        "freeze_plan", "freeze_plan_sidecar", "offline_stop_sidecar",
        "legacy_public_height_receipt",
        "legacy_maintenance_evidence_bundle",
        "legacy_maintenance_evidence_bundle_sidecar",
        "legacy_maintenance_boundary",
        "legacy_maintenance_boundary_sidecar",
        "legacy_late_fork_source_set",
        "legacy_late_fork_source_set_sidecar",
        "offline_stop_evidence",
        "pretag_artifact_input_set", "pretag_initial_live_provenance_set",
        "build_metadata", "validator_vault_restore_receipt",
        "validator_key_install_receipt", "ssh_identity", "validator_public_keys",
    }
    for raw in rows:
        row = require_keys(
            raw,
            "production stage file",
            ("name", "path", "sha256", "size_bytes", "mode"),
        )
        name = required_string(row["name"], "production stage file name")
        relative = required_string(row["path"], f"production stage {name} path")
        if name in by_name or name not in expected_names:
            fail("production stage file name is duplicate or unreviewed")
        if relative in seen_paths or Path(relative).is_absolute() or os.path.normpath(relative) != relative:
            fail("production stage file path is duplicate, absolute, or non-normalized")
        parts = Path(relative).parts
        if not parts or any(part in {"", ".", ".."} for part in parts) or len(parts) > 2:
            fail("production stage file path escapes the reviewed tree shape")
        if len(parts) == 2 and parts[0] not in {"source", "private"}:
            fail("production stage file uses an unreviewed child directory")
        if row["mode"] not in {"0400", "0500"}:
            fail("production stage file mode is unsupported")
        expected_mode = int(row["mode"], 8)
        size = required_int(row["size_bytes"], f"production stage {name} size", minimum=1)
        digest = bare_hash(row["sha256"], f"production stage {name} sha256")
        retain = name in retained_names
        payload, details, observed_digest = locked_file(
            stage_root / relative,
            f"production stage {name}",
            32 * 1024**2 if retain else 16 * 1024**3,
            retain_payload=retain,
        )
        if stat.S_IMODE(details.st_mode) != expected_mode:
            fail(f"production stage {name} mode differs from its inventory")
        if details.st_size != size or observed_digest != digest:
            fail(f"production stage {name} size/hash differs from its inventory")
        by_name[name] = row
        if payload is not None:
            payload_by_name[name] = payload
        seen_paths.add(relative)
    if set(by_name) != expected_names:
        fail("production stage file inventory omits reviewed inputs")

    stage_artifact = artifacts["production_input_stage_manifest"]
    if (
        Path(stage_artifact["path"]) != stage_root / "STAGE-MANIFEST.json"
        or stage_artifact["sha256"] != stage_digest
        or stage_artifact["sha256"]
        != manifest["provenance"]["production_input_stage_manifest_sha256"]
    ):
        fail("production stage-manifest path/hash differs from its sealed artifact")
    for name, artifact_value in artifacts.items():
        if name == "production_input_stage_manifest":
            continue
        row = by_name[artifact_stage_names[name]]
        if Path(artifact_value["path"]) != stage_root / row["path"]:
            fail(f"artifact {name} path differs from the private stage inventory")
        if artifact_value["sha256"] != row["sha256"]:
            fail(f"artifact {name} hash differs from the private stage inventory")
    freeze_row = by_name["freeze_plan"]
    if freeze_row["sha256"] != manifest["archive"]["freeze_plan_sha256"]:
        fail("private stage freeze plan differs from archive provenance")
    freeze_sidecar = payload_by_name["freeze_plan_sidecar"]
    if freeze_sidecar != f"{freeze_row['sha256']}  {Path(freeze_row['path']).name}\n".encode("ascii"):
        fail("private stage freeze-plan sidecar differs")
    if sha256_bytes(freeze_sidecar) != manifest["provenance"]["freeze_plan_sidecar_sha256"]:
        fail("private stage freeze-plan sidecar hash differs from provenance")
    offline_row = by_name["offline_stop_evidence"]
    offline_sidecar = payload_by_name["offline_stop_sidecar"]
    if offline_sidecar != f"{offline_row['sha256']}  {Path(offline_row['path']).name}\n".encode("ascii"):
        fail("private stage offline-stop sidecar differs")
    if (
        sha256_bytes(offline_sidecar)
        != artifacts["offline_stop_evidence_sidecar"]["sha256"]
    ):
        fail("private stage offline-stop sidecar hash differs from its artifact")
    return by_name, payload_by_name


def verify_legacy_maintenance_stage_payloads(
    manifest: Mapping[str, Any],
    stage_rows: Mapping[str, Mapping[str, Any]],
    payloads: Mapping[str, bytes],
) -> None:
    """Re-derive the legacy/v3 continuity roots from the immutable input stage.

    The manifest's hashes are necessary but not sufficient: the standalone
    evidence bundle, boundary, and offline-stop receipt must name the same
    canonical objects.  This validator keeps those small semantic payloads in
    the same no-follow read transaction as the stage inventory and rejects a
    hash-valid collection assembled from different captures.
    """

    artifacts = manifest["artifacts"]
    provenance = manifest["provenance"]
    archive = manifest["archive"]
    chain = manifest["chain"]

    def canonical_object(name: str, label: str) -> dict[str, Any]:
        raw = payloads.get(name)
        if raw is None:
            fail(f"production stage did not retain {label}")
        try:
            value = json.loads(raw)
        except (UnicodeError, json.JSONDecodeError):
            fail(f"{label} is invalid JSON")
        if not isinstance(value, dict) or raw != canonical_bytes(value):
            fail(f"{label} is not one canonical JSON object")
        return value

    def hash_value(value: Mapping[str, Any]) -> str:
        return sha256_bytes(canonical_bytes(value))

    def exact_hash(value: Any, label: str) -> str:
        if not isinstance(value, str) or not LOWER_HEX_32_RE.fullmatch(value):
            fail(f"{label} must be one lowercase SHA-256")
        return value

    def exact_utc(value: Any, label: str) -> str:
        if not isinstance(value, str) or re.fullmatch(
            r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", value
        ) is None:
            fail(f"{label} must be canonical UTC seconds")
        return value

    freeze = canonical_object("freeze_plan", "production staged freeze plan")
    if (
        freeze.get("schema") != "arc.recovery.freeze-plan.v5"
        or freeze.get("source_commit") != provenance["source_main_commit"]
        or [(row.get("name"), row.get("host")) for row in freeze.get("nodes", [])]
        != list(PRODUCTION_FLEET)
    ):
        fail("production staged freeze plan identity/topology differs")

    public = canonical_object(
        "legacy_public_height_receipt", "legacy public-height receipt"
    )
    bundle = canonical_object(
        "legacy_maintenance_evidence_bundle", "legacy maintenance evidence bundle"
    )
    boundary = canonical_object(
        "legacy_maintenance_boundary", "legacy maintenance boundary"
    )
    offline = canonical_object("offline_stop_evidence", "offline-stop evidence")

    def exact_sidecar(
        object_name: str, sidecar_name: str, artifact_name: str, label: str
    ) -> None:
        row = stage_rows[object_name]
        expected = (
            f"{row['sha256']}  {Path(row['path']).name}\n".encode("ascii")
        )
        if payloads.get(sidecar_name) != expected:
            fail(f"{label} sidecar does not bind the exact staged filename and bytes")
        if sha256_bytes(expected) != artifacts[artifact_name]["sha256"]:
            fail(f"{label} sidecar differs from its manifest artifact")

    exact_sidecar(
        "legacy_maintenance_evidence_bundle",
        "legacy_maintenance_evidence_bundle_sidecar",
        "legacy_maintenance_evidence_bundle_sidecar",
        "legacy maintenance evidence bundle",
    )
    exact_sidecar(
        "legacy_maintenance_boundary",
        "legacy_maintenance_boundary_sidecar",
        "legacy_maintenance_boundary_sidecar",
        "legacy maintenance boundary",
    )
    exact_sidecar(
        "legacy_late_fork_source_set",
        "legacy_late_fork_source_set_sidecar",
        "legacy_late_fork_source_set_sidecar",
        "legacy late-fork source set",
    )
    exact_sidecar(
        "offline_stop_evidence",
        "offline_stop_sidecar",
        "offline_stop_evidence_sidecar",
        "offline-stop evidence",
    )

    source_commit = provenance["source_main_commit"]
    freeze_sha = archive["freeze_plan_sha256"]
    capture_id = archive["capture_id"]
    common_identity = {
        "source_main_commit": source_commit,
        "freeze_plan_sha256": freeze_sha,
        "capture_id": capture_id,
    }

    bundle = require_keys(
        bundle,
        "legacy maintenance evidence bundle",
        (
            "schema", "source_main_commit", "freeze_plan_sha256", "capture_id",
            "first_quarantine_started_at", "all_controlled_stopped_at", "challenge",
            "authenticated_prefence_height_cross_proof", "network_quarantine_challenge",
            "quarantine_stability_proof", "nodes", "object_inventory",
            "aggregate_root_sha256",
        ),
    )
    if bundle["schema"] != "arc.recovery.legacy-maintenance-evidence-bundle.v1":
        fail("legacy maintenance evidence bundle schema is unsupported")
    if any(bundle.get(field) != expected for field, expected in common_identity.items()):
        fail("legacy maintenance evidence bundle capture identity differs")
    first_quarantine = exact_utc(
        bundle["first_quarantine_started_at"],
        "legacy maintenance first-quarantine timestamp",
    )
    all_stopped = exact_utc(
        bundle["all_controlled_stopped_at"],
        "legacy maintenance all-stopped timestamp",
    )
    if first_quarantine > all_stopped:
        fail("legacy maintenance evidence bundle timestamps are reversed")
    challenge = exact_hash(bundle["challenge"], "legacy maintenance challenge")

    derived_inventory: list[dict[str, Any]] = []

    def sealed(
        value: Any, *, node: str, role: str, label: str
    ) -> tuple[dict[str, Any], str]:
        wrapper = require_keys(value, label, ("value", "sha256"))
        inner = wrapper["value"]
        if not isinstance(inner, dict):
            fail(f"{label}.value must be an object")
        digest = exact_hash(wrapper["sha256"], f"{label}.sha256")
        raw = canonical_bytes(inner)
        if sha256_bytes(raw) != digest:
            fail(f"{label} hash is not reproducible from its canonical value")
        derived_inventory.append(
            {"node": node, "role": role, "sha256": digest, "size": len(raw)}
        )
        return inner, digest

    authenticated, authenticated_sha = sealed(
        bundle["authenticated_prefence_height_cross_proof"],
        node="fleet",
        role="authenticated-prefence-height-cross-proof",
        label="legacy maintenance authenticated pre-fence proof",
    )
    challenge_value, challenge_sha = sealed(
        bundle["network_quarantine_challenge"],
        node="fleet",
        role="network-quarantine-challenge",
        label="legacy maintenance quarantine challenge receipt",
    )
    if (
        authenticated.get("schema")
        != "arc.recovery.authenticated-legacy-height-fleet.v1"
        or any(authenticated.get(field) != expected for field, expected in common_identity.items())
        or challenge_value.get("schema")
        != "arc.recovery.legacy-network-quarantine-challenge.v1"
        or challenge_value.get("freeze_plan_sha256") != freeze_sha
        or challenge_value.get("capture_id") != capture_id
        or challenge_value.get("challenge") != challenge
    ):
        fail("legacy maintenance bundle proof/challenge identity differs")

    stability, stability_sha = sealed(
        bundle["quarantine_stability_proof"],
        node="fleet",
        role="network-quarantine-stability-proof",
        label="legacy maintenance quarantine stability proof",
    )
    stability = require_keys(
        stability,
        "legacy maintenance quarantine stability proof",
        (
            "schema", "source_main_commit", "freeze_plan_sha256", "capture_id",
            "challenge", "interval_seconds", "sample_count", "started_at",
            "completed_at", "monotonic_elapsed_ns", "fleet_heads", "nodes",
            "global_absence_claimed",
        ),
    )
    stability_started = exact_utc(
        stability["started_at"], "legacy maintenance stability started_at"
    )
    stability_completed = exact_utc(
        stability["completed_at"], "legacy maintenance stability completed_at"
    )
    if (
        stability["schema"]
        != "arc.recovery.legacy-network-quarantine-stability.v1"
        or any(stability.get(field) != expected for field, expected in common_identity.items())
        or stability["challenge"] != challenge
        or stability["interval_seconds"] != 120
        or stability["sample_count"] != 2
        or stability["global_absence_claimed"] is not False
        or required_int(
            stability["monotonic_elapsed_ns"],
            "legacy maintenance stability monotonic elapsed nanoseconds",
            minimum=120_000_000_000,
        )
        < 120_000_000_000
        or not first_quarantine <= stability_started <= stability_completed <= all_stopped
    ):
        fail("legacy maintenance quarantine stability proof identity/window differs")

    stability_rows = stability["nodes"]
    stability_heads = stability["fleet_heads"]
    if (
        not isinstance(stability_rows, list)
        or not isinstance(stability_heads, list)
        or len(stability_rows) != REQUIRED_VALIDATORS
        or len(stability_heads) != REQUIRED_VALIDATORS
    ):
        fail("legacy maintenance quarantine stability proof must contain exact six rows")
    status_fields = (
        "schema", "capture_id", "node", "freeze_plan_sha256", "receipt_sha256",
        "table", "rule_counters", "counter_snapshot_sha256",
        "owned_ruleset_stateless_sha256", "listener_inventory", "loopback_head",
        "quarantine_policy", "active", "enabled",
    )
    sample_fields = (
        "schema", "capture_id", "node", "freeze_plan_sha256", "challenge",
        "sample_index", "started_at", "completed_at", "quarantine_status_before",
        "quarantine_status_before_sha256", "quarantine_status_after",
        "quarantine_status_after_sha256", "writer", "listener_ownership", "head",
        "output_deny_packets", "ss_sha256", "global_absence_claimed",
    )
    stability_samples_by_node: dict[str, list[tuple[dict[str, Any], str]]] = {}
    stability_receipt_by_node: dict[str, str] = {}
    for index, ((node, host), raw_row, raw_fleet_head) in enumerate(
        zip(PRODUCTION_FLEET, stability_rows, stability_heads)
    ):
        row = require_keys(
            raw_row,
            f"legacy maintenance stability node {index}",
            ("node", "host", "samples", "output_deny_packets"),
        )
        if (row["node"], row["host"]) != (node, host):
            fail("legacy maintenance quarantine stability topology differs")
        raw_samples = row["samples"]
        if not isinstance(raw_samples, list) or len(raw_samples) != 2:
            fail(f"legacy maintenance {node} stability samples are not exact two")
        samples: list[tuple[dict[str, Any], str]] = []
        heads: list[dict[str, Any]] = []
        writers: list[dict[str, Any]] = []
        counters: list[int] = []
        windows: list[tuple[str, str]] = []
        receipt_root: str | None = None
        stateless_root: str | None = None
        for sample_index, raw_wrapper in enumerate(raw_samples):
            wrapper = require_keys(
                raw_wrapper,
                f"legacy maintenance {node} stability sample wrapper {sample_index}",
                ("value", "sha256"),
            )
            sample = require_keys(
                wrapper["value"],
                f"legacy maintenance {node} stability sample {sample_index}",
                sample_fields,
            )
            sample_sha = exact_hash(
                wrapper["sha256"],
                f"legacy maintenance {node} stability sample {sample_index} root",
            )
            if hash_value(sample) != sample_sha:
                fail(f"legacy maintenance {node} stability sample root differs")
            started = exact_utc(
                sample["started_at"],
                f"legacy maintenance {node} stability sample {sample_index} started_at",
            )
            completed = exact_utc(
                sample["completed_at"],
                f"legacy maintenance {node} stability sample {sample_index} completed_at",
            )
            if (
                sample["schema"]
                != "arc.recovery.legacy-network-quarantine-stability-sample.v1"
                or (
                    sample["capture_id"], sample["node"],
                    sample["freeze_plan_sha256"], sample["challenge"],
                    sample["sample_index"],
                )
                != (capture_id, node, freeze_sha, challenge, sample_index)
                or sample["global_absence_claimed"] is not False
                or not stability_started <= started <= completed <= stability_completed
            ):
                fail(f"legacy maintenance {node} stability sample identity/window differs")
            windows.append((started, completed))
            for side in ("before", "after"):
                status = require_keys(
                    sample[f"quarantine_status_{side}"],
                    f"legacy maintenance {node} stability {sample_index}/{side} status",
                    status_fields,
                )
                status_sha = exact_hash(
                    sample[f"quarantine_status_{side}_sha256"],
                    f"legacy maintenance {node} stability {sample_index}/{side} status root",
                )
                if (
                    hash_value(status) != status_sha
                    or status["schema"]
                    != "arc.recovery.legacy-network-quarantine-status.v1"
                    or (
                        status["capture_id"], status["node"],
                        status["freeze_plan_sha256"], status["active"], status["enabled"],
                    )
                    != (capture_id, node, freeze_sha, True, True)
                ):
                    fail(f"legacy maintenance {node} stability status differs")
            before = sample["quarantine_status_before"]
            after = sample["quarantine_status_after"]
            if (
                before["receipt_sha256"] != after["receipt_sha256"]
                or before["owned_ruleset_stateless_sha256"]
                != after["owned_ruleset_stateless_sha256"]
            ):
                fail(f"legacy maintenance {node} stability fence changed during a sample")
            this_receipt = exact_hash(
                before["receipt_sha256"],
                f"legacy maintenance {node} stability quarantine receipt",
            )
            this_stateless = exact_hash(
                before["owned_ruleset_stateless_sha256"],
                f"legacy maintenance {node} stability stateless ruleset",
            )
            if receipt_root is None:
                receipt_root = this_receipt
                stateless_root = this_stateless
            elif receipt_root != this_receipt or stateless_root != this_stateless:
                fail(f"legacy maintenance {node} stability fence changed between samples")
            writer = require_keys(
                sample["writer"],
                f"legacy maintenance {node} stability writer {sample_index}",
                ("pid", "start_ticks", "executable_sha256", "argv_sha256", "cgroup_sha256"),
            )
            required_int(writer["pid"], f"legacy maintenance {node} writer PID", minimum=1)
            required_int(
                writer["start_ticks"],
                f"legacy maintenance {node} writer start ticks",
                minimum=1,
            )
            for field in ("executable_sha256", "argv_sha256", "cgroup_sha256"):
                exact_hash(writer[field], f"legacy maintenance {node} writer {field}")
            listeners = require_keys(
                sample["listener_ownership"],
                f"legacy maintenance {node} stability listener ownership",
                ("rpc_tcp_9090_ss_sha256", "p2p_udp_9091_ss_sha256", "writer_pid"),
            )
            if listeners["writer_pid"] != writer["pid"]:
                fail(f"legacy maintenance {node} stability listener owner differs")
            for field in ("rpc_tcp_9090_ss_sha256", "p2p_udp_9091_ss_sha256"):
                exact_hash(listeners[field], f"legacy maintenance {node} listener {field}")
            exact_hash(sample["ss_sha256"], f"legacy maintenance {node} stability ss root")
            raw_head = require_keys(
                sample["head"],
                f"legacy maintenance {node} stability head {sample_index}",
                ("height", "block_hash", "state_root", "response_sha256", "stable_attempt"),
            )
            head = {
                "height": required_int(
                    raw_head["height"],
                    f"legacy maintenance {node} stability height",
                    minimum=1,
                ),
                "block_hash": exact_hash(
                    raw_head["block_hash"],
                    f"legacy maintenance {node} stability block hash",
                ),
                "state_root": exact_hash(
                    raw_head["state_root"],
                    f"legacy maintenance {node} stability state root",
                ),
            }
            responses = require_keys(
                raw_head["response_sha256"],
                f"legacy maintenance {node} stability response roots",
                ("info_before", "latest", "exact", "info_after"),
            )
            for field, digest in responses.items():
                exact_hash(digest, f"legacy maintenance {node} stability response {field}")
            required_int(
                raw_head["stable_attempt"],
                f"legacy maintenance {node} stability attempt",
                minimum=1,
                maximum=10,
            )
            counter = required_int(
                sample["output_deny_packets"],
                f"legacy maintenance {node} stability output deny counter",
            )
            heads.append(head)
            writers.append(dict(writer))
            counters.append(counter)
            samples.append((sample, sample_sha))
        if windows[0][1] > windows[1][0]:
            fail(f"legacy maintenance {node} stability sample windows overlap")
        if heads[0] != heads[1]:
            fail(f"legacy maintenance {node} head changed during the stability window")
        if writers[0] != writers[1]:
            fail(f"legacy maintenance {node} writer changed during the stability window")
        if counters[1] < counters[0]:
            fail(f"legacy maintenance {node} output deny counter regressed")
        if row["output_deny_packets"] != {
            "sample_0": counters[0], "sample_1": counters[1]
        }:
            fail(f"legacy maintenance {node} stability counter summary differs")
        fleet_head = require_keys(
            raw_fleet_head,
            f"legacy maintenance {node} stable fleet head",
            ("node", "host", "head"),
        )
        if fleet_head != {"node": node, "host": host, "head": heads[0]}:
            fail(f"legacy maintenance {node} stable fleet head differs")
        assert receipt_root is not None
        stability_receipt_by_node[node] = receipt_root
        stability_samples_by_node[node] = samples

    bundle_nodes = bundle["nodes"]
    if not isinstance(bundle_nodes, list) or len(bundle_nodes) != REQUIRED_VALIDATORS:
        fail("legacy maintenance evidence bundle must contain exactly six nodes")
    sealed_nodes: dict[str, dict[str, tuple[dict[str, Any], str]]] = {}
    wrapper_specs = (
        ("stopped_status", "stopped-status"),
        ("quarantine_status", "quarantine-status"),
        ("quarantine_monitor", "network-quarantine-monitor"),
        ("post_proof_quarantine_status", "post-proof-quarantine-status"),
        ("external_quarantine_proof", "external-quarantine-proof"),
        ("public_cross_proof", "public-cross-proof"),
        ("persisted_head", "persisted-head"),
    )
    for index, ((node, host), raw_node) in enumerate(zip(PRODUCTION_FLEET, bundle_nodes)):
        row = require_keys(
            raw_node,
            f"legacy maintenance bundle node {index}",
            ("node", "host", *(field for field, _role in wrapper_specs)),
        )
        if (row["node"], row["host"]) != (node, host):
            fail("legacy maintenance evidence bundle topology differs")
        sealed_nodes[node] = {}
        for field, role in wrapper_specs:
            inner, digest = sealed(
                row[field], node=node, role=role,
                label=f"legacy maintenance {node} {field}",
            )
            if (
                inner.get("capture_id") != capture_id
                or inner.get("node") != node
                or inner.get("freeze_plan_sha256") != freeze_sha
            ):
                fail(f"legacy maintenance {node} {field} identity differs")
            sealed_nodes[node][field] = (inner, digest)

    inventory = bundle["object_inventory"]
    if inventory != derived_inventory:
        fail("legacy maintenance evidence inventory differs from its retained objects")
    expected_inventory_root = sha256_bytes(
        canonical_bytes(
            {
                "schema": "arc.recovery.legacy-maintenance-evidence-inventory.v1",
                "objects": derived_inventory,
            }
        )
    )
    if bundle["aggregate_root_sha256"] != expected_inventory_root:
        fail("legacy maintenance evidence aggregate root is not reproducible")

    boundary = require_keys(
        boundary,
        "legacy maintenance boundary",
        (
            "schema", "source_main_commit", "freeze_plan_sha256", "capture_id",
            "first_quarantine_started_at", "all_controlled_stopped_at", "created_at",
            "official_origin_scope", "legacy_public_height_receipt",
            "authenticated_prefence_height_cross_proof_sha256",
            "legacy_maintenance_evidence_bundle_sha256",
            "network_quarantine_stability_proof_sha256",
            "network_quarantine_challenge",
            "tools", "nodes", "evidence_heights", "observed_cutoff_height",
            "continuity_safety_margin", "continuity_safety_margin_policy",
            "legacy_public_max_height", "global_absence_claimed", "reopening_policy",
            "late_fork_circuit", "threat_model",
        ),
    )
    if boundary["schema"] != "arc.recovery.legacy-maintenance-boundary.v1":
        fail("legacy maintenance boundary schema is unsupported")
    if any(boundary.get(field) != expected for field, expected in common_identity.items()):
        fail("legacy maintenance boundary capture identity differs")
    created_at = exact_utc(boundary["created_at"], "legacy maintenance created_at")
    if (
        boundary["first_quarantine_started_at"] != first_quarantine
        or boundary["all_controlled_stopped_at"] != all_stopped
        or all_stopped > created_at
    ):
        fail("legacy maintenance boundary timestamps differ from the evidence bundle")
    bundle_sha = artifacts["legacy_maintenance_evidence_bundle"]["sha256"]
    boundary_sha = artifacts["legacy_maintenance_boundary"]["sha256"]
    if boundary["legacy_maintenance_evidence_bundle_sha256"] != bundle_sha:
        fail("legacy maintenance boundary does not bind the exact evidence bundle")
    if boundary["authenticated_prefence_height_cross_proof_sha256"] != authenticated_sha:
        fail("legacy maintenance boundary authenticated proof root differs from the bundle")
    if boundary["network_quarantine_stability_proof_sha256"] != stability_sha:
        fail("legacy maintenance boundary stability proof root differs from the bundle")
    if boundary["network_quarantine_challenge"] != challenge:
        fail("legacy maintenance boundary challenge differs from the bundle")
    if chain["legacy_maintenance_boundary_sha256"] != boundary_sha:
        fail("legacy maintenance boundary artifact differs from the chain root")

    origin_scope = require_keys(
        boundary["official_origin_scope"],
        "legacy maintenance official origin scope",
        ("global_absence_claimed", "origins"),
    )
    if origin_scope != {
        "global_absence_claimed": False,
        "origins": list(LEGACY_OFFICIAL_ORIGINS),
    }:
        fail("legacy maintenance boundary official origins differ from the exact six")
    public_root = require_keys(
        boundary["legacy_public_height_receipt"],
        "legacy maintenance public-height root",
        ("schema", "sha256", "completed_at", "observed_max_height"),
    )
    if (
        public_root.get("sha256") != artifacts["legacy_public_height_receipt"]["sha256"]
        or public_root.get("schema") != public.get("schema")
        or public_root.get("completed_at") != public.get("completed_at")
        or public_root.get("observed_max_height") != public.get("legacy_public_max_height")
        or public.get("source_main_commit") != source_commit
        or public.get("freeze_plan_sha256") != freeze_sha
        or public.get("capture_id") != capture_id
    ):
        fail("legacy maintenance boundary public-height receipt binding differs")
    public_origins = public.get("origins")
    if (
        not isinstance(public_origins, list)
        or len(public_origins) != REQUIRED_VALIDATORS
        or [
            (row.get("name"), row.get("origin"))
            for row in public_origins
            if isinstance(row, dict)
        ]
        != [
            (row["node"], row["origin"])
            for row in LEGACY_OFFICIAL_ORIGINS
        ]
    ):
        fail("legacy public-height receipt origin topology differs from the exact six")
    public_after_heights = [
        required_int(
            row.get("info_after_height"),
            f"legacy public-height origin {index} info_after_height",
        )
        for index, row in enumerate(public_origins)
    ]
    if public.get("legacy_public_max_height") != max(public_after_heights):
        fail("legacy public-height receipt maximum is not its six-origin maximum")

    if (
        boundary["continuity_safety_margin"] != LEGACY_CONTINUITY_SAFETY_MARGIN
        or boundary["continuity_safety_margin_policy"]
        != LEGACY_CONTINUITY_SAFETY_MARGIN_POLICY
        or boundary["global_absence_claimed"] is not False
        or boundary["reopening_policy"] != LEGACY_REOPENING_POLICY
        or boundary["late_fork_circuit"] != LEGACY_LATE_FORK_CIRCUIT
        or boundary["threat_model"] != LEGACY_QUARANTINE_THREAT_MODEL
    ):
        fail("legacy maintenance boundary continuity policy differs")
    if sum(
        LEGACY_CONTINUITY_SAFETY_MARGIN_POLICY[field]
        for field in ("prune_depth", "commit_rule_rounds", "operational_headroom")
    ) != LEGACY_CONTINUITY_SAFETY_MARGIN:
        fail("legacy continuity safety-margin policy is internally inconsistent")
    tools = require_keys(
        boundary["tools"],
        "legacy maintenance boundary tool roots",
        (
            "remote_helper_sha256", "inspector_binary_sha256", "genesis_sha256",
            "validator_public_keys_sha256", "legacy_validator_set_sha256",
            "orchestrator_sha256", "rollout_tool_sha256", "rollout_schema_sha256",
        ),
    )
    expected_tools = {
        "remote_helper_sha256": archive["remote_helper_sha256"],
        "inspector_binary_sha256": artifacts["binary"]["sha256"],
        "genesis_sha256": artifacts["genesis"]["sha256"],
        "validator_public_keys_sha256": artifacts["validator_public_keys"]["sha256"],
        "legacy_validator_set_sha256": artifacts["legacy_validator_set"]["sha256"],
        "orchestrator_sha256": archive["archive_orchestrator_sha256"],
        "rollout_tool_sha256": archive["rollout_tool_sha256"],
        "rollout_schema_sha256": archive["rollout_schema_sha256"],
    }
    if tools != expected_tools:
        fail("legacy maintenance boundary tool roots differ from the staged recovery")

    boundary_nodes = boundary["nodes"]
    if not isinstance(boundary_nodes, list) or len(boundary_nodes) != REQUIRED_VALIDATORS:
        fail("legacy maintenance boundary must contain exactly six nodes")
    boundary_node_fields = (
        "node", "host", "origin", "public_observation",
        "authenticated_prefence_proof_sha256", "network_quarantine_receipt_sha256",
        "quarantine_status_sha256", "post_proof_quarantine_status_sha256",
        "external_quarantine_proof_sha256", "public_cross_proof_sha256",
        "initial_post_quarantine_head", "post_quarantine_head", "final_persisted_head",
    )

    def observation(value: Any, label: str) -> tuple[int, str]:
        wrapper = require_keys(value, label, ("tuple", "evidence_sha256"))
        head = require_keys(wrapper["tuple"], f"{label}.tuple", ("height", "block_hash", "state_root"))
        height = required_int(head["height"], f"{label}.tuple.height")
        exact_hash(head["block_hash"], f"{label}.tuple.block_hash")
        exact_hash(head["state_root"], f"{label}.tuple.state_root")
        return height, exact_hash(wrapper["evidence_sha256"], f"{label}.evidence_sha256")

    authenticated_rows = authenticated.get("nodes")
    if not isinstance(authenticated_rows, list) or len(authenticated_rows) != REQUIRED_VALIDATORS:
        fail("legacy maintenance authenticated proof must contain exactly six nodes")
    expected_height_rows: list[dict[str, Any]] = []
    for index, ((node, host), raw_boundary_node, authenticated_row, public_origin) in enumerate(
        zip(PRODUCTION_FLEET, boundary_nodes, authenticated_rows, public_origins)
    ):
        row = require_keys(
            raw_boundary_node,
            f"legacy maintenance boundary node {index}",
            boundary_node_fields,
        )
        if (row["node"], row["host"], row["origin"]) != (
            node,
            host,
            LEGACY_OFFICIAL_ORIGINS[index]["origin"],
        ):
            fail("legacy maintenance boundary topology differs")
        auth_row = require_keys(
            authenticated_row,
            f"legacy maintenance authenticated row {index}",
            ("node", "host", "proof", "proof_sha256"),
        )
        if (auth_row["node"], auth_row["host"]) != (node, host):
            fail("legacy maintenance authenticated topology differs")
        auth_proof = auth_row["proof"]
        if (
            not isinstance(auth_proof, dict)
            or hash_value(auth_proof) != auth_row["proof_sha256"]
            or row["authenticated_prefence_proof_sha256"] != auth_row["proof_sha256"]
        ):
            fail(f"legacy maintenance {node} authenticated proof root differs")
        authenticated_heights = {
            "authenticated_info_before": required_int(
                auth_proof.get("authenticated_info_before_height"),
                f"legacy maintenance {node} authenticated before height",
            ),
            "authenticated_latest": required_int(
                auth_proof.get("authenticated_latest_block_height"),
                f"legacy maintenance {node} authenticated latest height",
            ),
            "authenticated_info_after": required_int(
                auth_proof.get("authenticated_info_after_height"),
                f"legacy maintenance {node} authenticated after height",
            ),
            "authenticated_conservative_floor": required_int(
                auth_proof.get("conservative_height_floor"),
                f"legacy maintenance {node} authenticated conservative floor",
            ),
        }
        wrappers = sealed_nodes[node]
        stopped_value = wrappers["stopped_status"][0]
        status_value = wrappers["quarantine_status"][0]
        monitor_value = wrappers["quarantine_monitor"][0]
        post_status_value = wrappers["post_proof_quarantine_status"][0]
        external_value = wrappers["external_quarantine_proof"][0]
        cross_value = wrappers["public_cross_proof"][0]
        persisted_value = wrappers["persisted_head"][0]
        if (
            stopped_value.get("schema") != "arc.recovery.offline-stop-status.v1"
            or stopped_value.get("stopped") is not True
            or stopped_value.get("restart_fenced") is not True
            or status_value.get("schema")
            != "arc.recovery.legacy-network-quarantine-status.v1"
            or status_value.get("active") is not True
            or status_value.get("enabled") is not True
            or monitor_value.get("schema")
            != "arc.recovery.legacy-network-quarantine-monitor.v1"
            or monitor_value.get("incident_latched") is not False
            or monitor_value.get("continuous_fail_closed") is not True
            or monitor_value.get("automatic_unfence") is not False
            or monitor_value.get("global_absence_claimed") is not False
            or post_status_value.get("schema")
            != "arc.recovery.legacy-network-quarantine-status.v1"
            or post_status_value.get("active") is not True
            or post_status_value.get("enabled") is not True
            or external_value.get("schema")
            != "arc.recovery.legacy-network-quarantine-external-proof.v1"
            or cross_value.get("schema")
            != "arc.recovery.legacy-network-quarantine-public-cross-proof.v1"
            or persisted_value.get("schema") != "arc.recovery.persisted-legacy-head.v1"
            or persisted_value.get("source_main_commit") != source_commit
            or persisted_value.get("writer_stopped") is not True
            or persisted_value.get("restart_barrier_active") is not True
            or persisted_value.get("network_quarantine_active") is not True
            or persisted_value.get("global_absence_claimed") is not False
        ):
            fail(f"legacy maintenance {node} retained object policy differs")
        receipt_sha = exact_hash(
            status_value.get("receipt_sha256"),
            f"legacy maintenance {node} quarantine receipt root",
        )
        if (
            row["network_quarantine_receipt_sha256"] != receipt_sha
            or monitor_value.get("network_quarantine_receipt_sha256") != receipt_sha
            or post_status_value.get("receipt_sha256") != receipt_sha
            or external_value.get("network_quarantine_receipt_sha256") != receipt_sha
            or cross_value.get("network_quarantine_receipt_sha256") != receipt_sha
            or persisted_value.get("network_quarantine_receipt_sha256") != receipt_sha
            or stability_receipt_by_node[node] != receipt_sha
        ):
            fail(f"legacy maintenance {node} quarantine receipt chain differs")
        interpreter = require_keys(
            monitor_value.get("semantic_interpreter"),
            f"legacy maintenance {node} semantic interpreter",
            (
                "normalized_path", "sha256", "device", "inode", "uid", "gid",
                "mode", "nlink", "isolated", "environment",
            ),
        )
        environment = require_keys(
            interpreter["environment"],
            f"legacy maintenance {node} semantic interpreter environment",
            ("PATH", "LC_ALL", "TZ", "PYTHONHASHSEED"),
        )
        if (
            not isinstance(interpreter["normalized_path"], str)
            or re.fullmatch(r"/usr/bin/python3(?:\.[0-9]+)?", interpreter["normalized_path"])
            is None
            or interpreter["normalized_path"] == "/usr/bin/python3"
            or exact_hash(
                interpreter["sha256"],
                f"legacy maintenance {node} semantic interpreter hash",
            )
            != interpreter["sha256"]
            or any(
                isinstance(interpreter[field], bool)
                or not isinstance(interpreter[field], int)
                or interpreter[field] <= 0
                for field in ("device", "inode")
            )
            or (interpreter["uid"], interpreter["gid"], interpreter["mode"], interpreter["nlink"])
            != (0, 0, 0o755, 1)
            or interpreter["isolated"] is not True
            or environment
            != {
                "PATH": "/usr/bin:/bin",
                "LC_ALL": "C",
                "TZ": "UTC",
                "PYTHONHASHSEED": "0",
            }
        ):
            fail(f"legacy maintenance {node} semantic interpreter contract differs")
        expected_roots = {
            "quarantine_status_sha256": wrappers["quarantine_status"][1],
            "post_proof_quarantine_status_sha256": wrappers[
                "post_proof_quarantine_status"
            ][1],
            "external_quarantine_proof_sha256": wrappers[
                "external_quarantine_proof"
            ][1],
            "public_cross_proof_sha256": wrappers["public_cross_proof"][1],
        }
        if any(row.get(field) != wanted for field, wanted in expected_roots.items()):
            fail(f"legacy maintenance {node} boundary roots differ from the bundle")
        public_height, public_evidence = observation(
            row["public_observation"], f"legacy maintenance {node} public observation"
        )
        initial_height, initial_evidence = observation(
            row["initial_post_quarantine_head"],
            f"legacy maintenance {node} initial post-quarantine head",
        )
        later_height, later_evidence = observation(
            row["post_quarantine_head"],
            f"legacy maintenance {node} post-quarantine head",
        )
        persisted_height, persisted_evidence = observation(
            row["final_persisted_head"], f"legacy maintenance {node} persisted head"
        )
        stability_samples = stability_samples_by_node[node]
        stability_heights = [
            required_int(
                sample[0]["head"]["height"],
                f"legacy maintenance {node} stability sample {sample_index} height",
                minimum=1,
            )
            for sample_index, sample in enumerate(stability_samples)
        ]
        if (
            public_evidence != wrappers["public_cross_proof"][1]
            or initial_evidence != wrappers["quarantine_status"][1]
            or later_evidence != wrappers["public_cross_proof"][1]
            or persisted_evidence != wrappers["persisted_head"][1]
        ):
            fail(f"legacy maintenance {node} observed heads differ from the bundle roots")
        public_heights = {
            "public_info_before": required_int(
                public_origin.get("info_before_height"),
                f"legacy maintenance {node} public before height",
            ),
            "public_latest": required_int(
                public_origin.get("latest_block_height"),
                f"legacy maintenance {node} public latest height",
            ),
            "public_info_after": required_int(
                public_origin.get("info_after_height"),
                f"legacy maintenance {node} public after height",
            ),
        }
        if public_height != public_heights["public_info_after"]:
            fail(f"legacy maintenance {node} public observation height differs")
        for label in ("public_info_before", "public_latest", "public_info_after"):
            expected_height_rows.append(
                {
                    "node": node,
                    "label": label,
                    "height": public_heights[label],
                    "evidence_sha256": artifacts["legacy_public_height_receipt"]["sha256"],
                }
            )
        for label in (
            "authenticated_info_before", "authenticated_latest",
            "authenticated_info_after", "authenticated_conservative_floor",
        ):
            expected_height_rows.append(
                {
                    "node": node,
                    "label": label,
                    "height": authenticated_heights[label],
                    "evidence_sha256": auth_row["proof_sha256"],
                }
            )
        expected_height_rows.extend(
            (
                {
                    "node": node,
                    "label": "initial_post_quarantine_head",
                    "height": initial_height,
                    "evidence_sha256": initial_evidence,
                },
                {
                    "node": node,
                    "label": "public_cross_info_after",
                    "height": public_height,
                    "evidence_sha256": public_evidence,
                },
                {
                    "node": node,
                    "label": "post_quarantine_head",
                    "height": later_height,
                    "evidence_sha256": later_evidence,
                },
                {
                    "node": node,
                    "label": "quarantine_stability_sample_0",
                    "height": stability_heights[0],
                    "evidence_sha256": stability_samples[0][1],
                },
                {
                    "node": node,
                    "label": "quarantine_stability_sample_1",
                    "height": stability_heights[1],
                    "evidence_sha256": stability_samples[1][1],
                },
                {
                    "node": node,
                    "label": "final_persisted_head",
                    "height": persisted_height,
                    "evidence_sha256": persisted_evidence,
                },
            )
        )

    height_labels = (
        "public_info_before", "public_latest", "public_info_after",
        "authenticated_info_before", "authenticated_latest", "authenticated_info_after",
        "authenticated_conservative_floor", "initial_post_quarantine_head",
        "public_cross_info_after", "post_quarantine_head",
        "quarantine_stability_sample_0", "quarantine_stability_sample_1",
        "final_persisted_head",
    )
    height_rows = boundary["evidence_heights"]
    if not isinstance(height_rows, list) or len(height_rows) != REQUIRED_VALIDATORS * len(height_labels):
        fail("legacy maintenance evidence-height ledger is not exact six-by-thirteen")
    observed_heights: list[int] = []
    for index, raw_height in enumerate(height_rows):
        row = require_keys(
            raw_height,
            f"legacy maintenance evidence-height row {index}",
            ("node", "label", "height", "evidence_sha256"),
        )
        node_index, label_index = divmod(index, len(height_labels))
        if (row["node"], row["label"]) != (
            PRODUCTION_FLEET[node_index][0],
            height_labels[label_index],
        ):
            fail("legacy maintenance evidence-height order differs")
        observed_heights.append(
            required_int(row["height"], f"legacy maintenance evidence-height row {index}")
        )
        exact_hash(
            row["evidence_sha256"],
            f"legacy maintenance evidence-height row {index} root",
        )
    if height_rows != expected_height_rows:
        fail("legacy maintenance evidence-height ledger differs from its retained proofs")
    cutoff = required_int(
        boundary["observed_cutoff_height"], "legacy maintenance observed cutoff"
    )
    if cutoff != max(observed_heights):
        fail("legacy maintenance observed cutoff is not the maximum evidence height")
    if boundary["legacy_public_max_height"] != cutoff + LEGACY_CONTINUITY_SAFETY_MARGIN:
        fail("legacy maintenance public maximum is not cutoff plus 128")

    expected_chain = {
        "legacy_maintenance_evidence_bundle_sha256": bundle_sha,
        "legacy_maintenance_boundary_sha256": boundary_sha,
        "legacy_observed_cutoff_height": cutoff,
        "legacy_continuity_safety_margin": LEGACY_CONTINUITY_SAFETY_MARGIN,
        "legacy_public_max_height": boundary["legacy_public_max_height"],
        "legacy_global_absence_claimed": False,
        "legacy_official_origins": list(LEGACY_OFFICIAL_ORIGINS),
        "legacy_reopening_policy": LEGACY_REOPENING_POLICY,
        "legacy_late_fork_circuit": LEGACY_LATE_FORK_CIRCUIT,
        "legacy_quarantine_threat_model": LEGACY_QUARANTINE_THREAT_MODEL,
    }
    if any(chain.get(field) != wanted for field, wanted in expected_chain.items()):
        fail("manifest chain legacy-maintenance projection differs from the boundary")

    source_set = require_keys(
        canonical_object(
            "legacy_late_fork_source_set", "legacy late-fork source set"
        ),
        "legacy late-fork source set",
        (
            "schema", "source_main_commit", "boundary_sha256",
            "observed_cutoff_height", "official_origins", "monitored_retired_origins",
            "monitored_community_origins", "poll_interval_seconds",
            "max_staleness_seconds", "validation_mode", "validation_tool_sha256",
            "global_absence_claimed",
        ),
    )
    source_set_sha = artifacts["legacy_late_fork_source_set"]["sha256"]
    tool_sha = artifacts["legacy_late_fork_interlock_tool"]["sha256"]
    if (
        source_set["schema"] != "arc.recovery.legacy-late-fork-source-set.v1"
        or source_set["source_main_commit"] != source_commit
        or source_set["boundary_sha256"] != boundary_sha
        or source_set["observed_cutoff_height"] != cutoff
        or source_set["poll_interval_seconds"] != 30
        or source_set["max_staleness_seconds"] != 90
        or source_set["validation_mode"]
        != "capture-bound-retirement-tripwire-offline-validation-required"
        or source_set["validation_tool_sha256"] != tool_sha
        or source_set["global_absence_claimed"] is not False
        or chain["legacy_late_fork_source_set_sha256"] != source_set_sha
    ):
        fail("legacy late-fork source set identity/policy differs")
    expected_source_origins = [
        {"name": node, "host": host, "origin": origin["origin"]}
        for (node, host), origin in zip(PRODUCTION_FLEET, LEGACY_OFFICIAL_ORIGINS)
    ]
    if source_set["official_origins"] != expected_source_origins:
        fail("legacy late-fork source set official origins differ from the exact six")
    expected_retired = [
        {"name": row["name"], "origin": row["origin"]}
        for row in expected_source_origins
    ]
    retired = source_set["monitored_retired_origins"]
    community = source_set["monitored_community_origins"]
    if (
        not isinstance(retired, list)
        or not isinstance(community, list)
        or retired[:REQUIRED_VALIDATORS] != expected_retired
    ):
        fail("legacy late-fork monitored source inventories differ")
    coordinates: set[tuple[str, str]] = set()
    for scope, rows in (("retired", retired), ("community", community)):
        for index, raw_row in enumerate(rows):
            row = require_keys(
                raw_row,
                f"legacy late-fork {scope} source {index}",
                ("name", "origin"),
            )
            name = required_string(
                row["name"], f"legacy late-fork {scope} source {index} name"
            )
            if re.fullmatch(r"[a-z0-9][a-z0-9-]{0,63}", name) is None:
                fail(f"legacy late-fork {scope} source {index} name is unsafe")
            origin = required_string(
                row["origin"], f"legacy late-fork {scope} source {index} origin"
            )
            parsed = urllib.parse.urlsplit(origin)
            official = scope == "retired" and index < REQUIRED_VALIDATORS
            if (
                parsed.scheme != ("http" if official else "https")
                or not parsed.hostname
                or parsed.port is None
                or parsed.username is not None
                or parsed.password is not None
                or parsed.path not in {"", "/"}
                or parsed.query
                or parsed.fragment
                or origin.rstrip("/") != origin
            ):
                fail(f"legacy late-fork {scope} source {index} origin is unsafe")
            coordinate = (name, origin)
            if coordinate in coordinates:
                fail("legacy late-fork source set repeats a monitored source")
            coordinates.add(coordinate)

    offline = require_keys(
        offline,
        "offline-stop evidence",
        (
            "schema", "source_main_commit", "freeze_plan_sha256",
            "freeze_plan_sidecar_sha256", "capture_id", "remote_helper_sha256",
            "remote_helper_path", "first_quarantine_started_at",
            "all_controlled_stopped_at", "legacy_height_cross_proof",
            "legacy_maintenance_boundary", "legacy_maintenance_boundary_sha256",
            "legacy_maintenance_evidence_bundle_sha256", "nodes",
        ),
    )
    if offline["schema"] != "arc.validator-vault.offline-stop-evidence.v2":
        fail("offline-stop evidence schema is unsupported")
    if any(offline.get(field) != expected for field, expected in common_identity.items()):
        fail("offline-stop evidence capture identity differs")
    if (
        offline["first_quarantine_started_at"] != first_quarantine
        or offline["all_controlled_stopped_at"] != all_stopped
        or offline["legacy_maintenance_boundary_sha256"] != boundary_sha
        or offline["legacy_maintenance_boundary"] != boundary
        or offline["legacy_maintenance_evidence_bundle_sha256"] != bundle_sha
        or offline["legacy_height_cross_proof"] != authenticated
    ):
        fail("offline-stop evidence does not embed the exact maintenance bundle/boundary")
    offline_nodes = offline["nodes"]
    if (
        not isinstance(offline_nodes, list)
        or len(offline_nodes) != REQUIRED_VALIDATORS
        or [(row.get("node"), row.get("host")) for row in offline_nodes]
        != list(PRODUCTION_FLEET)
    ):
        fail("offline-stop evidence topology differs from the exact six")


def load_legacy_interlock_interpreters(
    manifest: Mapping[str, Any],
) -> dict[str, dict[str, Any]]:
    """Reload the six bundle-pinned host interpreters without ambient lookup."""
    if manifest["mode"] != "production":
        return {}
    artifact = manifest["artifacts"]["legacy_maintenance_evidence_bundle"]
    path = Path(artifact["path"])
    try:
        descriptor = os.open(
            path,
            os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0),
        )
    except OSError as error:
        fail(f"cannot open legacy maintenance interpreter bundle: {error}")
    try:
        details = os.fstat(descriptor)
        visible = os.lstat(path)
        stable = lambda value: (
            value.st_dev, value.st_ino, value.st_mode, value.st_uid, value.st_gid,
            value.st_nlink, value.st_size, value.st_mtime_ns, value.st_ctime_ns,
        )
        if (
            not stat.S_ISREG(details.st_mode)
            or stat.S_ISLNK(visible.st_mode)
            or stable(details) != stable(visible)
            or details.st_uid != os.geteuid()
            or details.st_nlink != 1
            or not 0 < details.st_size <= 32 * 1024**2
        ):
            fail("legacy maintenance interpreter bundle identity differs")
        chunks: list[bytes] = []
        digest = hashlib.sha256()
        total = 0
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            chunks.append(chunk)
            digest.update(chunk)
            total += len(chunk)
        raw = b"".join(chunks)
        if total != details.st_size or stable(details) != stable(os.fstat(descriptor)):
            fail("legacy maintenance interpreter bundle changed while read")
        observed = digest.hexdigest()
    finally:
        os.close(descriptor)
    if (
        observed != artifact["sha256"]
        or stat.S_IMODE(details.st_mode) != 0o400
    ):
        fail("legacy maintenance interpreter bundle identity differs")
    try:
        bundle = json.loads(raw)
    except (UnicodeError, json.JSONDecodeError):
        fail("legacy maintenance interpreter bundle is invalid JSON")
    if not isinstance(bundle, dict) or raw != canonical_bytes(bundle):
        fail("legacy maintenance interpreter bundle is not canonical")
    rows = bundle.get("nodes")
    if not isinstance(rows, list) or len(rows) != REQUIRED_VALIDATORS:
        fail("legacy maintenance interpreter bundle omits the fixed six")
    result: dict[str, dict[str, Any]] = {}
    for index, ((node, host), row) in enumerate(zip(PRODUCTION_FLEET, rows)):
        if not isinstance(row, dict) or (row.get("node"), row.get("host")) != (node, host):
            fail(f"legacy maintenance interpreter bundle topology differs at {index}")
        wrapper = require_keys(
            row.get("quarantine_monitor"),
            f"legacy maintenance {node} monitor wrapper",
            ("value", "sha256"),
        )
        monitor = wrapper["value"]
        if (
            not isinstance(monitor, dict)
            or sha256_bytes(canonical_bytes(monitor)) != wrapper["sha256"]
            or monitor.get("schema")
            != "arc.recovery.legacy-network-quarantine-monitor.v1"
            or monitor.get("node") != node
            or monitor.get("network_quarantine_receipt_sha256")
            != row.get("quarantine_status", {}).get("value", {}).get("receipt_sha256")
        ):
            fail(f"legacy maintenance {node} monitor projection differs")
        interpreter = require_keys(
            monitor.get("semantic_interpreter"),
            f"legacy maintenance {node} semantic interpreter projection",
            (
                "normalized_path", "sha256", "device", "inode", "uid", "gid",
                "mode", "nlink", "isolated", "environment",
            ),
        )
        environment = require_keys(
            interpreter["environment"],
            f"legacy maintenance {node} semantic interpreter environment projection",
            ("PATH", "LC_ALL", "TZ", "PYTHONHASHSEED"),
        )
        if (
            not isinstance(interpreter["normalized_path"], str)
            or re.fullmatch(r"/usr/bin/python3\.[0-9]+", interpreter["normalized_path"])
            is None
            or not isinstance(interpreter["sha256"], str)
            or LOWER_HEX_32_RE.fullmatch(interpreter["sha256"]) is None
            or any(
                isinstance(interpreter[field], bool)
                or not isinstance(interpreter[field], int)
                or interpreter[field] <= 0
                for field in ("device", "inode")
            )
            or (interpreter["uid"], interpreter["gid"], interpreter["mode"], interpreter["nlink"])
            != (0, 0, 0o755, 1)
            or interpreter["isolated"] is not True
            or environment
            != {
                "PATH": "/usr/bin:/bin",
                "LC_ALL": "C",
                "TZ": "UTC",
                "PYTHONHASHSEED": "0",
            }
        ):
            fail(f"legacy maintenance {node} semantic interpreter projection differs")
        result[node] = dict(interpreter)
    return result


def verify_protected_pretag_stage_payloads(
    manifest: Mapping[str, Any], payloads: Mapping[str, bytes]
) -> None:
    def canonical_object(name: str, label: str) -> dict[str, Any]:
        raw = payloads[name]
        try:
            value = json.loads(raw)
        except (UnicodeError, json.JSONDecodeError):
            fail(f"{label} is invalid JSON")
        if not isinstance(value, dict) or raw != canonical_bytes(value):
            fail(f"{label} is not one canonical JSON object")
        return value

    provenance = manifest["provenance"]
    artifacts = manifest["artifacts"]
    groups = provenance["protected_pretag_artifact"]["groups"]
    input_set = require_keys(
        canonical_object("pretag_artifact_input_set", "protected pre-tag artifact input set"),
        "protected pre-tag artifact input set",
        ("schema", "repository", "commit", "run_id", "run_attempt", "artifacts"),
    )
    expected_set_header = {
        "schema": "arc.recovery.pretag-artifact-input-set.v1",
        "repository": "FerrumVir/arc-chain",
        "commit": provenance["source_main_commit"],
        "run_id": provenance["pretag_workflow_run_id"],
        "run_attempt": provenance["pretag_workflow_run_attempt"],
    }
    if any(input_set.get(field) != wanted for field, wanted in expected_set_header.items()):
        fail("protected pre-tag artifact input set header differs from rollout provenance")
    input_rows = input_set["artifacts"]
    if not isinstance(input_rows, list) or len(input_rows) != len(PRETAG_GROUPS):
        fail("protected pre-tag artifact input set must contain exactly nine rows")
    for index, ((kind, platform), raw_row, group) in enumerate(
        zip(PRETAG_GROUPS, input_rows, groups)
    ):
        row = require_keys(
            raw_row,
            f"protected pre-tag artifact input set row {index}",
            ("kind", "platform", "artifact_id", "raw_actions_zip"),
        )
        key = pretag_artifact_key(kind, platform)
        if (
            (row["kind"], row["platform"]) != (kind, platform)
            or required_int(row["artifact_id"], f"protected pre-tag row {index} artifact_id", minimum=1)
            != group["initial"]["live"]["artifact_id"]
            or row["raw_actions_zip"] != artifacts[key]["path"]
        ):
            fail(f"protected pre-tag artifact input row {kind}/{platform} is not the staged live tuple")
    initial_set = require_keys(
        canonical_object(
            "pretag_initial_live_provenance_set",
            "protected pre-tag initial live provenance set",
        ),
        "protected pre-tag initial live provenance set",
        ("schema", "repository", "commit", "run_id", "run_attempt", "artifacts"),
    )
    expected_initial_header = {
        "schema": "arc.protected-pretag-artifact-set.v1",
        "repository": "FerrumVir/arc-chain",
        "commit": provenance["source_main_commit"],
        "run_id": provenance["pretag_workflow_run_id"],
        "run_attempt": provenance["pretag_workflow_run_attempt"],
    }
    if any(initial_set.get(field) != wanted for field, wanted in expected_initial_header.items()):
        fail("protected pre-tag initial provenance set header differs from rollout provenance")
    if initial_set["artifacts"] != [group["initial"] for group in groups]:
        fail("protected pre-tag initial provenance set differs from the ordered window initials")

    metadata_raw = payloads["build_metadata"]
    try:
        metadata_value = json.loads(metadata_raw)
    except (UnicodeError, json.JSONDecodeError):
        fail("Linux headless BUILD-METADATA.json is invalid JSON")
    if (
        not isinstance(metadata_value, dict)
        or metadata_raw
        != (json.dumps(metadata_value, sort_keys=True, indent=2) + "\n").encode()
    ):
        fail("Linux headless BUILD-METADATA.json is not the exact packaged JSON form")
    metadata = require_keys(
        metadata_value,
        "Linux headless BUILD-METADATA.json",
        (
            "schema", "kind", "repository", "commit", "platform", "rust_target",
            "version", "workflow_run_id", "workflow_run_attempt", "files",
        ),
    )
    expected_metadata = {
        "schema": "arc.pretag.artifact.v1",
        "kind": "headless",
        "repository": "FerrumVir/arc-chain",
        "commit": provenance["source_main_commit"],
        "platform": "linux-x86_64",
        "rust_target": "x86_64-unknown-linux-gnu",
        "version": "0.8.0",
        "workflow_run_id": provenance["pretag_workflow_run_id"],
        "workflow_run_attempt": provenance["pretag_workflow_run_attempt"],
        "files": groups[0]["initial"]["artifact"]["files"],
    }
    if metadata != expected_metadata:
        fail("Linux headless BUILD-METADATA differs from the exact protected artifact tuple")
    validate_validator_receipt_chain(manifest, payloads)


def validate_validator_key_receipt_chain_value(
    value: Any, manifest: Mapping[str, Any]
) -> dict[str, Any]:
    chain = require_keys(
        value,
        "manifest.provenance.validator_key_receipt_chain",
        (
            "schema", "source_main_commit", "restore_receipt_sha256",
            "install_receipt_sha256", "linux_pretag_artifact_id",
            "linux_pretag_raw_actions_zip_sha256", "arc_cli_sha256", "genesis_sha256",
            "validator_public_keys_sha256", "freeze_plan_sha256",
            "offline_stop_evidence_sha256", "known_hosts_sha256", "ssh_identity_sha256",
            "ssh_sha256", "scp_sha256", "validators",
        ),
    )
    if chain["schema"] != "arc.recovery.validator-key-receipt-chain.v1":
        fail("validator key receipt-chain provenance schema is unsupported")
    if chain["source_main_commit"] != manifest["provenance"]["source_main_commit"]:
        fail("validator key receipt-chain source commit differs")
    for field in (
        "restore_receipt_sha256", "install_receipt_sha256",
        "linux_pretag_raw_actions_zip_sha256", "arc_cli_sha256", "genesis_sha256",
        "validator_public_keys_sha256", "freeze_plan_sha256",
        "offline_stop_evidence_sha256", "known_hosts_sha256", "ssh_identity_sha256",
        "ssh_sha256", "scp_sha256",
    ):
        bare_hash(chain[field], f"validator key receipt-chain {field}")
    required_int(
        chain["linux_pretag_artifact_id"],
        "validator key receipt-chain linux_pretag_artifact_id",
        minimum=1,
    )
    artifact_bindings = {
        "restore_receipt_sha256": "validator_vault_restore_receipt",
        "install_receipt_sha256": "validator_key_install_receipt",
        "linux_pretag_raw_actions_zip_sha256": "pretag_raw_headless_linux_x86_64",
        "arc_cli_sha256": "cli",
        "genesis_sha256": "genesis",
        "validator_public_keys_sha256": "validator_public_keys",
        "offline_stop_evidence_sha256": "offline_stop_evidence",
        "known_hosts_sha256": "ssh_known_hosts",
    }
    for field, artifact_key in artifact_bindings.items():
        if chain[field] != manifest["artifacts"][artifact_key]["sha256"]:
            fail(f"validator key receipt-chain {field} differs from its artifact")
    if chain["freeze_plan_sha256"] != manifest["archive"]["freeze_plan_sha256"]:
        fail("validator key receipt-chain freeze-plan hash differs")
    if chain["ssh_sha256"] != manifest["provenance"]["offline_stop_verification"]["ssh_sha256"]:
        fail("validator key receipt-chain SSH executable hash differs")
    linux_live = manifest["provenance"]["protected_pretag_artifact"]["groups"][0]["initial"]["live"]
    if chain["linux_pretag_artifact_id"] != linux_live["artifact_id"]:
        fail("validator key receipt-chain Linux artifact ID differs")
    rows = chain["validators"]
    if not isinstance(rows, list) or len(rows) != REQUIRED_VALIDATORS:
        fail("validator key receipt-chain must contain exactly six rows")
    hashes: set[str] = set()
    for index, (raw, (node, host), sealed) in enumerate(
        zip(rows, PRODUCTION_FLEET, manifest["validators"])
    ):
        row = require_keys(
            raw, f"validator key receipt-chain row {index}",
            ("node", "host", "address", "keyfile_sha256"),
        )
        digest = bare_hash(row["keyfile_sha256"], f"validator key receipt-chain row {index} key hash")
        if (
            (row["node"], row["host"], row["address"])
            != (node, host, sealed["address"])
            or digest in hashes
        ):
            fail("validator key receipt-chain fleet/address/hash mapping differs")
        hashes.add(digest)
    return chain


def validate_validator_installed_key_proof(
    value: Any, manifest: Mapping[str, Any]
) -> dict[str, Any]:
    """Validate the fresh, challenged six-host installed-key observation.

    The proof deliberately binds immutable stage/receipt roots instead of the
    final manifest digest, so a builder can obtain it from a provisional
    manifest and then add it to final provenance without a hash cycle.
    """

    proof = require_keys(
        value,
        "manifest.provenance.validator_installed_key_proof",
        (
            "schema", "source_main_commit", "production_input_stage_manifest_sha256",
            "freeze_plan_sha256", "offline_stop_evidence_sha256",
            "validator_install_receipt_sha256", "validator_public_keys_sha256",
            "arc_cli_sha256", "remote_helper_sha256", "remote_helper_path",
            "ssh_known_hosts_sha256", "ssh_identity_sha256", "ssh_path",
            "ssh_sha256", "scp_path", "scp_sha256", "challenge",
            "started_at_unix_ms", "completed_at_unix_ms", "validators",
        ),
    )
    if proof["schema"] != "arc.recovery.validator-installed-key-proof.v1":
        fail("validator installed-key proof schema is unsupported")
    provenance = manifest["provenance"]
    archive = manifest["archive"]
    artifacts = manifest["artifacts"]
    receipt_chain = provenance["validator_key_receipt_chain"]
    expected = {
        "source_main_commit": provenance["source_main_commit"],
        "production_input_stage_manifest_sha256": provenance[
            "production_input_stage_manifest_sha256"
        ],
        "freeze_plan_sha256": archive["freeze_plan_sha256"],
        "offline_stop_evidence_sha256": artifacts["offline_stop_evidence"]["sha256"],
        "validator_install_receipt_sha256": receipt_chain["install_receipt_sha256"],
        "validator_public_keys_sha256": artifacts["validator_public_keys"]["sha256"],
        "arc_cli_sha256": artifacts["cli"]["sha256"],
        "remote_helper_sha256": archive["remote_helper_sha256"],
        "remote_helper_path": (
            f"/root/.arc-recovery-helpers/{archive['remote_helper_sha256']}/archive-node.sh"
        ),
        "ssh_known_hosts_sha256": receipt_chain["known_hosts_sha256"],
        "ssh_identity_sha256": receipt_chain["ssh_identity_sha256"],
        "ssh_path": "/usr/bin/ssh",
        "ssh_sha256": receipt_chain["ssh_sha256"],
        "scp_path": "/usr/bin/scp",
        "scp_sha256": receipt_chain["scp_sha256"],
    }
    if any(proof.get(field) != wanted for field, wanted in expected.items()):
        fail("validator installed-key proof differs from its stage/receipt/transport tuple")
    challenge = bare_hash(proof["challenge"], "validator installed-key proof challenge")
    if challenge == provenance["offline_stop_verification"]["challenge"]:
        fail("validator installed-key proof must use a fresh challenge")
    started = required_int(
        proof["started_at_unix_ms"], "validator installed-key proof start", minimum=1
    )
    completed = required_int(
        proof["completed_at_unix_ms"], "validator installed-key proof completion", minimum=1
    )
    if completed < started or completed - started > 10 * 60 * 1000:
        fail("validator installed-key proof window is reversed or exceeds ten minutes")
    rows = proof["validators"]
    if not isinstance(rows, list) or len(rows) != REQUIRED_VALIDATORS:
        fail("validator installed-key proof must contain exactly six rows")
    response_roots: set[str] = set()
    for index, (raw, (node, host), receipt) in enumerate(
        zip(rows, PRODUCTION_FLEET, receipt_chain["validators"])
    ):
        row = require_keys(
            raw,
            f"validator installed-key proof row {index}",
            (
                "node", "host", "key_path", "address", "keyfile_sha256",
                "remote_response_sha256", "state",
            ),
        )
        response_root = bare_hash(
            row["remote_response_sha256"],
            f"validator installed-key proof row {index} response root",
        )
        if (
            (row["node"], row["host"]) != (node, host)
            or row["key_path"] != "/etc/arc-v3/validator-key.json"
            or row["state"] != "verified"
            or row["address"] != receipt["address"]
            or row["keyfile_sha256"] != receipt["keyfile_sha256"]
            or response_root in response_roots
        ):
            fail("validator installed-key proof fleet/key/response mapping differs")
        response_roots.add(response_root)
    return proof


def validate_validator_receipt_chain(
    manifest: Mapping[str, Any], payloads: Mapping[str, bytes]
) -> dict[str, Any]:
    """Validate and sanitize the staged restore -> install fixed-six key chain."""

    def canonical_object(name: str, label: str) -> dict[str, Any]:
        raw = payloads[name]
        try:
            value = json.loads(raw)
        except (UnicodeError, json.JSONDecodeError):
            fail(f"{label} is invalid JSON")
        if not isinstance(value, dict) or raw != canonical_bytes(value):
            fail(f"{label} is not one canonical JSON object")
        return value

    provenance = manifest["provenance"]
    artifacts = manifest["artifacts"]
    restore = require_keys(
        canonical_object("validator_vault_restore_receipt", "validator vault restore receipt"),
        "validator vault restore receipt",
        (
            "schema", "source_commit", "cms_sha256", "source_ciphertext_sha256",
            "restore_cert_sha256", "arc_cli_sha256", "genesis_sha256", "openssl_sha256",
            "openssl_libssl_sha256", "openssl_libcrypto_sha256",
            "pretag_initial_provenance", "pretag_final_provenance", "validators",
        ),
    )
    install = require_keys(
        canonical_object("validator_key_install_receipt", "validator key install receipt"),
        "validator key install receipt",
        (
            "schema", "source_commit", "cms_sha256", "arc_cli_sha256", "genesis_sha256",
            "known_hosts_sha256", "ssh_identity_sha256", "ssh_sha256", "scp_sha256",
            "freeze_plan_sha256", "offline_stop_evidence_sha256",
            "pretag_initial_provenance", "pretag_final_provenance", "validators",
        ),
    )
    if restore["schema"] != "arc.validator-vault.restore.v1":
        fail("validator vault restore receipt schema is unsupported")
    if install["schema"] != "arc.validator-vault.install.v1":
        fail("validator key install receipt schema is unsupported")
    source_commit = provenance["source_main_commit"]
    for receipt_name, receipt in (("restore", restore), ("install", install)):
        if receipt["source_commit"] != source_commit:
            fail(f"validator {receipt_name} receipt source commit differs from rollout")
        for field in (
            "cms_sha256", "arc_cli_sha256", "genesis_sha256",
            *(
                ("source_ciphertext_sha256", "restore_cert_sha256", "openssl_sha256",
                 "openssl_libssl_sha256", "openssl_libcrypto_sha256")
                if receipt_name == "restore"
                else ("known_hosts_sha256", "ssh_identity_sha256", "ssh_sha256", "scp_sha256",
                      "freeze_plan_sha256", "offline_stop_evidence_sha256")
            ),
        ):
            bare_hash(receipt[field], f"validator {receipt_name} receipt {field}")
    for field in ("source_commit", "cms_sha256", "arc_cli_sha256", "genesis_sha256"):
        if restore[field] != install[field]:
            fail(f"validator restore/install receipt {field} differs")
    expected_install = {
        "arc_cli_sha256": artifacts["cli"]["sha256"],
        "genesis_sha256": artifacts["genesis"]["sha256"],
        "known_hosts_sha256": artifacts["ssh_known_hosts"]["sha256"],
        "ssh_identity_sha256": sha256_bytes(payloads["ssh_identity"]),
        "ssh_sha256": provenance["offline_stop_verification"]["ssh_sha256"],
        "freeze_plan_sha256": manifest["archive"]["freeze_plan_sha256"],
        "offline_stop_evidence_sha256": artifacts["offline_stop_evidence"]["sha256"],
    }
    if any(install[field] != wanted for field, wanted in expected_install.items()):
        fail("validator install receipt artifact/freeze/offline/transport tuple differs")
    linux_group = provenance["protected_pretag_artifact"]["groups"][0]

    def validate_receipt_provenance(receipt: Mapping[str, Any], receipt_name: str) -> None:
        initial = _validate_protected_pretag_proof(
            receipt["pretag_initial_provenance"],
            label=f"validator {receipt_name} initial protected pre-tag proof",
            kind="headless", platform="linux-x86_64", source_commit=source_commit,
            run_id=provenance["pretag_workflow_run_id"],
            run_attempt=provenance["pretag_workflow_run_attempt"],
            response_labels=("workflow", "run", "artifact", "protected_main"),
        )
        final = _validate_protected_pretag_proof(
            receipt["pretag_final_provenance"],
            label=f"validator {receipt_name} final protected pre-tag proof",
            kind="headless", platform="linux-x86_64", source_commit=source_commit,
            run_id=provenance["pretag_workflow_run_id"],
            run_attempt=provenance["pretag_workflow_run_attempt"],
            response_labels=("workflow", "run", "artifact", "protected_main"),
        )
        initial_live = {key: value for key, value in initial["live"].items() if key != "api_verified_at_unix"}
        final_live = {key: value for key, value in final["live"].items() if key != "api_verified_at_unix"}
        window_live = {
            key: value
            for key, value in linux_group["initial"]["live"].items()
            if key != "api_verified_at_unix"
        }
        if (
            initial_live != final_live
            or initial_live != window_live
            or initial["artifact"] != final["artifact"]
            or initial["artifact"] != linux_group["initial"]["artifact"]
            or final["live"]["api_verified_at_unix"] < initial["live"]["api_verified_at_unix"]
        ):
            fail(f"validator {receipt_name} protected proof differs from the sealed Linux artifact")

    validate_receipt_provenance(restore, "restore")
    validate_receipt_provenance(install, "install")
    try:
        public_rows = json.loads(payloads["validator_public_keys"])
    except (UnicodeError, json.JSONDecodeError):
        fail("validator public-key manifest is invalid JSON")
    if canonical_bytes(public_rows) != payloads["validator_public_keys"]:
        fail("validator public-key manifest is not canonical")
    if not isinstance(public_rows, list) or len(public_rows) != REQUIRED_VALIDATORS:
        fail("validator public-key manifest must contain exactly six rows")
    restore_rows = restore["validators"]
    install_rows = install["validators"]
    if (
        not isinstance(restore_rows, list)
        or not isinstance(install_rows, list)
        or len(restore_rows) != REQUIRED_VALIDATORS
        or len(install_rows) != REQUIRED_VALIDATORS
    ):
        fail("validator receipts must contain exactly six rows")
    sanitized_rows: list[dict[str, str]] = []
    key_hashes: set[str] = set()
    public_keys: set[str] = set()
    for index, (fleet, restored_raw, installed_raw, public_raw, sealed) in enumerate(
        zip(PRODUCTION_FLEET, restore_rows, install_rows, public_rows, manifest["validators"])
    ):
        lower, host = fleet; upper = lower.upper()
        restored = require_keys(
            restored_raw, f"validator restore row {index}",
            ("node", "key_file", "address", "keyfile_sha256"),
        )
        installed = require_keys(
            installed_raw, f"validator install row {index}",
            ("node", "address", "keyfile_sha256", "destination", "state"),
        )
        public = require_keys(
            public_raw, f"validator public-key row {index}", ("address", "public_key", "stake")
        )
        address = bare_hash(sealed["address"], f"sealed validator {lower} address")
        key_sha = bare_hash(restored["keyfile_sha256"], f"validator {lower} key hash")
        public_key = bare_hash(public["public_key"], f"validator {lower} public key")
        if (
            (sealed["name"], sealed["host"]) != (lower, host)
            or (restored["node"], restored["key_file"], restored["address"])
            != (upper, f"keys/{upper}.validator-key.json", address)
            or (installed["node"], installed["address"], installed["keyfile_sha256"],
                installed["destination"], installed["state"])
            != (upper, address, key_sha, "/etc/arc-v3/validator-key.json", "verified")
            or public["address"] != address
            or public["stake"] != sealed["stake"]
        ):
            fail(f"validator receipt/public/sealed mapping differs for {upper}")
        if key_sha in key_hashes or public_key in public_keys:
            fail("validator receipts repeat a private-key hash or public key")
        key_hashes.add(key_sha); public_keys.add(public_key)
        sanitized_rows.append(
            {"node": lower, "host": host, "address": address, "keyfile_sha256": key_sha}
        )
    sanitized = {
        "schema": "arc.recovery.validator-key-receipt-chain.v1",
        "source_main_commit": source_commit,
        "restore_receipt_sha256": artifacts["validator_vault_restore_receipt"]["sha256"],
        "install_receipt_sha256": artifacts["validator_key_install_receipt"]["sha256"],
        "linux_pretag_artifact_id": linux_group["initial"]["live"]["artifact_id"],
        "linux_pretag_raw_actions_zip_sha256": linux_group["initial"]["artifact"]["raw_actions_zip_sha256"],
        "arc_cli_sha256": install["arc_cli_sha256"],
        "genesis_sha256": install["genesis_sha256"],
        "validator_public_keys_sha256": artifacts["validator_public_keys"]["sha256"],
        "freeze_plan_sha256": install["freeze_plan_sha256"],
        "offline_stop_evidence_sha256": install["offline_stop_evidence_sha256"],
        "known_hosts_sha256": install["known_hosts_sha256"],
        "ssh_identity_sha256": install["ssh_identity_sha256"],
        "ssh_sha256": install["ssh_sha256"],
        "scp_sha256": install["scp_sha256"],
        "validators": sanitized_rows,
    }
    sealed = provenance.get("validator_key_receipt_chain")
    if sealed is not None and sealed != sanitized:
        fail("sealed validator key receipt-chain provenance differs from staged receipts")
    return sanitized


def _exclusive_write(path: Path, payload: bytes, mode: int) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0), mode)
    try:
        with os.fdopen(descriptor, "wb", closefd=False) as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.fchmod(descriptor, mode)
    finally:
        os.close(descriptor)


def seal_manifest(draft_path: Path, output_path: Path) -> str:
    if output_path.suffix != ".json":
        fail("sealed manifest output must end in .json")
    try:
        draft = json.loads(draft_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"cannot read draft manifest: {error}")
    manifest = validate_manifest(draft)
    verify_artifacts(manifest)
    payload = canonical_bytes(manifest)
    digest = sha256_bytes(payload)
    sidecar = output_path.with_name(output_path.name + ".sha256")
    if output_path.exists() or sidecar.exists():
        fail("sealed manifest or checksum already exists; refusing replacement")
    output_path.parent.mkdir(parents=True, exist_ok=True)
    created: list[Path] = []
    try:
        _exclusive_write(output_path, payload, 0o444)
        created.append(output_path)
        _exclusive_write(sidecar, f"{digest}  {output_path.name}\n".encode(), 0o444)
        created.append(sidecar)
        directory_fd = os.open(output_path.parent, os.O_RDONLY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    except Exception:
        for path in reversed(created):
            try:
                path.chmod(0o600)
                path.unlink()
            except OSError:
                pass
        raise
    return digest


def load_sealed_manifest(
    path: Path, *, allow_provisional_installed_key_proof: bool = False
) -> tuple[dict[str, Any], str]:
    sidecar = path.with_name(path.name + ".sha256")
    for candidate, label in ((path, "manifest"), (sidecar, "checksum")):
        try:
            details = candidate.lstat()
        except OSError as error:
            fail(f"sealed {label} is unavailable: {error}")
        if stat.S_ISLNK(details.st_mode) or not stat.S_ISREG(details.st_mode):
            fail(f"sealed {label} must be a regular non-symlink file")
        if details.st_mode & 0o222:
            fail(f"sealed {label} must have no write bits")
    payload = path.read_bytes()
    try:
        parsed = json.loads(payload)
    except json.JSONDecodeError as error:
        fail(f"sealed manifest is invalid JSON: {error}")
    manifest = validate_manifest(
        parsed,
        allow_provisional_installed_key_proof=allow_provisional_installed_key_proof,
    )
    if payload != canonical_bytes(manifest):
        fail("sealed manifest is not canonical JSON; reseal the reviewed draft")
    digest = sha256_bytes(payload)
    expected_sidecar = f"{digest}  {path.name}\n"
    if sidecar.read_text(encoding="ascii") != expected_sidecar:
        fail("sealed manifest checksum sidecar is missing or does not match")
    return manifest, digest


def _read_canonical_read_only_json(path: Path, label: str) -> tuple[dict[str, Any], bytes, str]:
    try:
        details = path.lstat()
        if stat.S_ISLNK(details.st_mode) or not stat.S_ISREG(details.st_mode):
            fail(f"{label} must be a regular non-symlink file")
        if details.st_mode & 0o222:
            fail(f"{label} must have no write bits")
        payload = path.read_bytes()
        value = json.loads(payload)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"cannot read {label}: {error}")
    if not isinstance(value, dict) or payload != canonical_bytes(value):
        fail(f"{label} must be a canonical JSON object")
    return value, payload, sha256_bytes(payload)


def load_legacy_archive_fork_nodes(
    rollout: "RecoveryRollout", archive_manifest_path: Path, complete_path: Path
) -> list[dict[str, str]]:
    """Authenticate the exact archived node classification before config generation."""
    archive = rollout.manifest["archive"]
    manifest, _, manifest_sha = _read_canonical_read_only_json(
        archive_manifest_path, "ARCHIVE-MANIFEST.json"
    )
    complete, _, complete_sha = _read_canonical_read_only_json(
        complete_path, "COMPLETE.json"
    )
    if manifest_sha != archive["archive_manifest_sha256"]:
        fail("local ARCHIVE-MANIFEST.json differs from the finalized rollout root")
    if complete_sha != archive["complete_sha256"]:
        fail("local COMPLETE.json differs from the finalized rollout root")
    manifest_fields = (
        "schema",
        "freeze_plan_sha256",
        "capture_id",
        "rollout_manifest_sha256",
        "source_commit",
        "orchestrator_sha256",
        "remote_helper_sha256",
        "rollout_tool_sha256",
        "rollout_schema_sha256",
        "canonical_reference",
        "capture_classification_counts",
        "shared_inputs",
        "validator_bundles",
        "sha256sums",
    )
    complete_fields = (
        "schema",
        "freeze_plan_sha256",
        "capture_id",
        "rollout_manifest_sha256",
        "source_commit",
        "archive_manifest_sha256",
        "object_count_before_complete",
        "validator_bundle_count",
        "finalization_anchor",
    )
    require_keys(manifest, "archive manifest", manifest_fields)
    require_keys(complete, "archive COMPLETE", complete_fields)
    if manifest["schema"] != "arc.recovery.archive-manifest.v2":
        fail("archive manifest schema is unsupported")
    if complete["schema"] != "arc.recovery.archive-complete.v2":
        fail("archive COMPLETE schema is unsupported")
    anchor = require_keys(
        complete["finalization_anchor"],
        "archive COMPLETE finalization_anchor",
        (
            "intent_sha256",
            "gist_id",
            "gist_revision",
            "gist_file_sha256",
        ),
    )
    for field in ("intent_sha256", "gist_file_sha256"):
        if not isinstance(anchor[field], str) or re.fullmatch(
            r"[0-9a-f]{64}", anchor[field]
        ) is None or anchor[field] == "0" * 64:
            fail(f"archive COMPLETE finalization_anchor {field} is malformed")
    if anchor["intent_sha256"] != anchor["gist_file_sha256"]:
        fail("archive COMPLETE Gist file hash differs from finalization intent")
    if not isinstance(anchor["gist_id"], str) or re.fullmatch(
        r"[0-9a-f]{20,64}", anchor["gist_id"]
    ) is None:
        fail("archive COMPLETE finalization_anchor gist_id is malformed")
    if not isinstance(anchor["gist_revision"], str) or re.fullmatch(
        r"[0-9a-f]{40}", anchor["gist_revision"]
    ) is None:
        fail("archive COMPLETE finalization_anchor gist_revision is malformed")
    if complete["archive_manifest_sha256"] != manifest_sha:
        fail("archive COMPLETE does not bind ARCHIVE-MANIFEST.json")
    for field in ("freeze_plan_sha256", "capture_id", "rollout_manifest_sha256", "source_commit"):
        if complete[field] != manifest[field]:
            fail(f"archive COMPLETE {field} differs from ARCHIVE-MANIFEST.json")
    if manifest["capture_id"] != archive["capture_id"]:
        fail("archive manifest capture id differs from the finalized rollout")
    if manifest["rollout_manifest_sha256"] != archive["prearchive_rollout_sha256"]:
        fail("archive manifest prearchive rollout root differs")
    rows = manifest["validator_bundles"]
    if not isinstance(rows, list) or len(rows) != len(rollout.validators):
        fail("archive manifest must contain the complete six-validator classification")
    expected_nodes = {node["name"] for node in rollout.validators}
    classifications = {
        "valid_canonical",
        "valid_noncanonical_fork",
        "preserved_unclassified",
    }
    seen: set[str] = set()
    counts = {classification: 0 for classification in classifications}
    fork_nodes: list[dict[str, str]] = []
    for index, row in enumerate(rows):
        row = require_keys(
            row,
            f"archive validator_bundles[{index}]",
            ("node", "classification", "bundle", "inventory"),
        )
        node_name = required_string(row["node"], f"archive validator_bundles[{index}].node")
        classification = row["classification"]
        if node_name not in expected_nodes or node_name in seen:
            fail("archive manifest validator identities differ from the sealed rollout")
        if classification not in classifications:
            fail("archive manifest contains an unsupported classification")
        seen.add(node_name)
        counts[classification] += 1
        if classification == "valid_noncanonical_fork":
            bundle = require_keys(
                row["bundle"],
                f"archive validator_bundles[{index}].bundle",
                ("name", "size", "sha256", "sidecar_name", "sidecar_sha256"),
            )
            inventory = require_keys(
                row["inventory"],
                f"archive validator_bundles[{index}].inventory",
                ("name", "size", "sha256", "sidecar_name", "sidecar_sha256"),
            )
            fork_nodes.append(
                {
                    "node": node_name,
                    "bundle_name": required_string(
                        bundle["name"], f"archive {node_name} bundle name"
                    ),
                    "bundle_sha256": bare_hash(
                        bundle["sha256"], f"archive {node_name} bundle sha256"
                    ),
                    "inventory_name": required_string(
                        inventory["name"], f"archive {node_name} inventory name"
                    ),
                    "inventory_sha256": bare_hash(
                        inventory["sha256"], f"archive {node_name} inventory sha256"
                    ),
                }
            )
    if seen != expected_nodes or complete["validator_bundle_count"] != len(expected_nodes):
        fail("archive manifest/COMPLETE omits a sealed validator")
    if manifest["capture_classification_counts"] != counts:
        fail("archive manifest classification counts do not match its validator rows")
    return fork_nodes


def parse_legacy_archive_inventory(
    payload: bytes,
    *,
    node: str,
    capture_id: str,
    rollout_manifest_sha256: str,
) -> dict[str, str]:
    """Validate the small Drive object that binds a fork's local binding tree."""
    try:
        text = payload.decode("utf-8")
    except UnicodeDecodeError as error:
        fail(f"legacy archive inventory for {node} is not UTF-8: {error}")
    if not text.endswith("\n") or "\r" in text or "\x00" in text:
        fail(f"legacy archive inventory for {node} is not canonical line text")
    values: dict[str, str] = {}
    for line in text.splitlines():
        if line.count("=") != 1:
            fail(f"legacy archive inventory for {node} has a malformed line")
        key, value = line.split("=", 1)
        if (
            not re.fullmatch(r"[a-z][a-z0-9_]*", key)
            or not value
            or key in values
        ):
            fail(f"legacy archive inventory for {node} has unsafe or duplicate fields")
        values[key] = value
    common = {
        "manifest_sha256",
        "capture_id",
        "node",
        "classification",
        "canonical_match",
        "archive_scope",
        "capture_index_sha256",
        "binding_index_sha256",
    }
    scope_fields = {
        "complete-stopped-legacy-data-v3": {
            "complete_data_dir",
            "excluded_outside_data_dir_private_material",
            "excluded_service_environments",
            "excluded_build_models_and_git",
        },
        "complete-content-indexed-stopped-legacy-source-v4": {
            "source_tree_retained_locally",
            "model_excluded_and_bound_by_rollout",
            "source_index_sha256",
        },
    }
    scope = values.get("archive_scope", "")
    expected_fields = common | scope_fields.get(scope, set())
    if not scope or set(values) != expected_fields:
        fail(f"legacy archive inventory for {node} has an unsupported field set")
    expected = {
        "manifest_sha256": rollout_manifest_sha256,
        "capture_id": capture_id,
        "node": node,
        "classification": "valid_noncanonical_fork",
        "canonical_match": "false",
    }
    for field, wanted in expected.items():
        if values.get(field) != wanted:
            fail(f"legacy archive inventory for {node} has the wrong {field}")
    for field in ("capture_index_sha256", "binding_index_sha256", "source_index_sha256"):
        if field in values:
            values[field] = bare_hash(values[field], f"legacy archive {node} {field}")
    return values


def write_frontend_config(
    rollout: "RecoveryRollout",
    output_path: Path,
    archive_manifest_path: Path | None = None,
    archive_complete_path: Path | None = None,
    reward_evidence: Sequence[ReceiptEvidence] | None = None,
) -> str:
    if output_path.suffix != ".json":
        fail("frontend config output must end in .json")
    sidecar = output_path.with_name(output_path.name + ".sha256")
    if output_path.exists() or sidecar.exists():
        fail("frontend config or checksum already exists; refusing replacement")
    # A recovered config is the publication/reopen artifact. Never create it
    # from sealed metadata alone: repeat the live H/H+1, liveness, convergence,
    # visible-height, and reward-policy gates immediately before writing it.
    if rollout.manifest["checks"]["reward"]["mode"] == "receipt":
        if reward_evidence is None:
            fail("receipt-mode frontend publication requires --reward-evidence with two distinct mined receipts")
        validate_distinct_receipt_evidence(reward_evidence)
    elif reward_evidence is not None:
        fail("--reward-evidence is permitted only for a receipt-mode frontend publication")
    archive = rollout.manifest["archive"]
    finalized = all(archive[field] != "0" * 64 for field in ARCHIVE_FINALIZATION_FIELDS)
    if finalized:
        if (archive_manifest_path is None) != (archive_complete_path is None):
            fail("--archive-manifest and --archive-complete must be supplied together")
        if archive_manifest_path is None:
            verified_archive = rollout.verify_production_archive()
            if verified_archive != archive["archive_manifest_sha256"]:
                fail("frontend archive verification returned a different finalized root")
            rollout.load_production_archive_metadata()
            legacy_archive_nodes = [
                rollout.legacy_archive_forks[node["name"]]
                for node in rollout.validators
                if node["name"] in rollout.legacy_archive_forks
            ]
        else:
            assert archive_complete_path is not None
            legacy_archive_nodes = load_legacy_archive_fork_nodes(
                rollout, archive_manifest_path, archive_complete_path
            )
    else:
        if archive_manifest_path is not None or archive_complete_path is not None:
            fail("prearchive frontend publication cannot accept finalized archive files")
        legacy_archive_nodes = []
    # Remote archive verification can be long. Sample the live chain and
    # reward gates only after it, leaving the shortest possible gap before the
    # create-only publication bytes and their live archive provenance probes.
    rollout.verify_live(reward_evidence)
    payload = canonical_bytes(rollout.frontend_config(legacy_archive_nodes))
    digest = sha256_bytes(payload)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    created: list[Path] = []
    try:
        _exclusive_write(output_path, payload, 0o444)
        created.append(output_path)
        _exclusive_write(sidecar, f"{digest}  {output_path.name}\n".encode(), 0o444)
        created.append(sidecar)
        directory_fd = os.open(output_path.parent, os.O_RDONLY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    except Exception:
        for path in reversed(created):
            try:
                path.chmod(0o600)
                path.unlink()
            except OSError:
                pass
        raise
    return digest


def run_checked(
    argv: Sequence[str],
    *,
    stdin: str | None = None,
    timeout: int = 120,
    capture: bool = True,
    env: Mapping[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    try:
        result = subprocess.run(
            list(argv),
            input=stdin,
            text=True,
            capture_output=capture,
            timeout=timeout,
            check=False,
            env=dict(env) if env is not None else None,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        fail(f"command failed to run: {shlex.join(argv)}: {error}")
    if result.returncode != 0:
        detail = (result.stderr or result.stdout or "no diagnostic").strip()
        fail(f"command failed ({result.returncode}): {shlex.join(argv)}: {detail}")
    return result


def run_checked_bytes(
    argv: Sequence[str], *, timeout: int = 120, env: Mapping[str, str] | None = None
) -> subprocess.CompletedProcess[bytes]:
    """Run a bounded-output command without implicit text decoding."""
    try:
        result = subprocess.run(
            list(argv),
            capture_output=True,
            timeout=timeout,
            check=False,
            env=dict(env) if env is not None else None,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        fail(f"command failed to run: {shlex.join(argv)}: {error}")
    if result.returncode != 0:
        detail = (result.stderr or result.stdout or b"no diagnostic").decode(
            "utf-8", errors="replace"
        ).strip()
        fail(f"command failed ({result.returncode}): {shlex.join(argv)}: {detail}")
    return result


@dataclass(frozen=True)
class ReceiptEvidence:
    tx_hash: str
    job_id: str
    worker: str

    @classmethod
    def from_value(cls, value: Any) -> "ReceiptEvidence":
        body = require_keys(value, "reward evidence", ("tx_hash", "job_id", "worker"))
        return cls(*(f"0x{bare_hash(body[key], f'reward evidence.{key}')}" for key in ("tx_hash", "job_id", "worker")))


def validate_distinct_receipt_evidence(evidence: Sequence[ReceiptEvidence]) -> None:
    if len(evidence) != 2:
        fail("reward canary gate requires exactly two mined receipt evidence objects")
    if len({item.tx_hash for item in evidence}) != 2:
        fail("reward receipt evidence must contain two distinct transaction hashes")
    if len({item.job_id for item in evidence}) != 2:
        fail("reward receipt evidence must contain two distinct job ids")
    if len({item.worker for item in evidence}) != 1:
        fail("both reward receipts must belong to the same community worker")


def recovery_probe_id_for_rollout(rollout_sha256: str, ordinal: int) -> str:
    if not LOWER_HEX_32_RE.fullmatch(rollout_sha256):
        fail("rollout sha256 must be exactly 64 lowercase hexadecimal characters")
    if ordinal not in {1, 2}:
        fail("reward receipt probe ordinal must be 1 or 2")
    digest = hashlib.sha256(
        RECOVERY_PROBE_ID_DOMAIN
        + bytes.fromhex(rollout_sha256)
        + bytes([ordinal])
    ).digest()
    return f"0x{(RECOVERY_PROBE_PREFIX + digest[:16]).hex()}"


def reward_progress_payload(
    rollout_sha256: str, evidence: Sequence[ReceiptEvidence]
) -> bytes:
    if len(evidence) > 2:
        fail("reward evidence progress cannot contain more than two receipts")
    if len(evidence) == 2:
        validate_distinct_receipt_evidence(evidence)
    return canonical_bytes(
        {
            "schema": "arc.recovery.reward-evidence-progress.v1",
            "rollout_sha256": rollout_sha256,
            "receipts": [
                {
                    "ordinal": ordinal,
                    "recovery_probe_id": recovery_probe_id_for_rollout(
                        rollout_sha256, ordinal
                    ),
                    "tx_hash": item.tx_hash,
                    "job_id": item.job_id,
                    "worker": item.worker,
                }
                for ordinal, item in enumerate(evidence, 1)
            ],
        }
    )


def parse_reward_progress_payload(
    payload: bytes, expected_rollout_sha256: str
) -> list[ReceiptEvidence]:
    try:
        body = require_keys(
            json.loads(payload.decode("utf-8")),
            "reward evidence progress",
            ("schema", "rollout_sha256", "receipts"),
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"cannot read reward evidence progress: {error}")
    if body["schema"] != "arc.recovery.reward-evidence-progress.v1":
        fail("reward evidence progress schema is unsupported")
    if body["rollout_sha256"] != expected_rollout_sha256:
        fail("reward evidence progress is bound to a different rollout")
    rows = body["receipts"]
    if not isinstance(rows, list) or len(rows) > 2:
        fail("reward evidence progress receipts must be an array of length 0..2")
    evidence: list[ReceiptEvidence] = []
    for ordinal, row in enumerate(rows, 1):
        entry = require_keys(
            row,
            "reward evidence progress receipt",
            (
                "ordinal",
                "recovery_probe_id",
                "tx_hash",
                "job_id",
                "worker",
            ),
        )
        if entry["ordinal"] != ordinal:
            fail("reward evidence progress ordinals must be contiguous and ordered")
        if (
            entry["recovery_probe_id"]
            != recovery_probe_id_for_rollout(expected_rollout_sha256, ordinal)
        ):
            fail("reward evidence progress contains a foreign recovery probe identity")
        evidence.append(
            ReceiptEvidence.from_value(
                {key: entry[key] for key in ("tx_hash", "job_id", "worker")}
            )
        )
    if len(evidence) == 2:
        validate_distinct_receipt_evidence(evidence)
    if payload != reward_progress_payload(expected_rollout_sha256, evidence):
        fail("reward evidence progress is not canonical")
    return evidence


def parse_reward_evidence_payload(
    payload: bytes, expected_rollout_sha256: str
) -> list[ReceiptEvidence]:
    """Parse only the unique canonical byte representation for this rollout."""
    try:
        body = require_keys(
            json.loads(payload.decode("utf-8")),
            "reward evidence file",
            ("schema", "rollout_sha256", "receipts"),
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"cannot read reward evidence: {error}")
    if body["schema"] != "arc.recovery.reward-evidence.v1":
        fail("reward evidence file schema is unsupported")
    if body["rollout_sha256"] != expected_rollout_sha256:
        fail("reward evidence file is bound to a different rollout")
    rows = body["receipts"]
    if not isinstance(rows, list):
        fail("reward evidence file receipts must be an array")
    evidence = [ReceiptEvidence.from_value(row) for row in rows]
    validate_distinct_receipt_evidence(evidence)
    expected = canonical_bytes(
        {
            "schema": "arc.recovery.reward-evidence.v1",
            "rollout_sha256": expected_rollout_sha256,
            "receipts": [
                {
                    "tx_hash": item.tx_hash,
                    "job_id": item.job_id,
                    "worker": item.worker,
                }
                for item in evidence
            ],
        }
    )
    if payload != expected:
        fail("reward evidence file is not canonical")
    return evidence


class RecoveryRollout:
    def __init__(
        self,
        manifest: dict[str, Any],
        digest: str,
        *,
        output: Any = sys.stdout,
        reward_evidence_output: Path | None = None,
        rollback_journal: Path | None = None,
    ) -> None:
        self.manifest = manifest
        self.digest = digest
        self.output = output
        self.processes: dict[str, subprocess.Popen[str]] = {}
        self.logs: dict[str, Any] = {}
        self.started_production: set[str] = set()
        self.prepared_production: set[str] = set()
        self.production_service_baseline: dict[str, dict[str, bool]] = {}
        self.production_public_listener_baseline: dict[str, dict[str, int]] = {}
        self.archive_metadata_loaded = False
        self.production_archive_verified_root: str | None = None
        self.archive_manifest_payload: bytes | None = None
        self.archive_complete_payload: bytes | None = None
        self.legacy_archive_forks: dict[str, dict[str, Any]] = {}
        self.reward_evidence_output = reward_evidence_output
        self.reward_evidence_reservation: tuple[int, int] | None = None
        self.existing_reward_evidence: list[ReceiptEvidence] | None = None
        self.reward_evidence_progress: list[ReceiptEvidence] = []
        self.rollback_journal = rollback_journal
        self.rollback_journal_reserved = False
        self.rollback_journal_state = "unreserved"
        self.rollback_run = 0
        self.production_transport_ready = False
        self.production_transport_env: dict[str, str] = {}
        self.production_ssh_path: Path | None = None
        self.production_scp_path: Path | None = None
        self.production_rclone_path: Path | None = None
        self.production_rclone_config: Path | None = None
        self.production_known_hosts: Path | None = None
        self.production_ssh_identity: Path | None = None
        self.production_transport_pins: dict[str, str] = {}
        # True until this transaction replaces the pre-existing edge.  Once
        # the staged maintenance Caddyfile is installed, all semantic probes
        # use authenticated SSH+loopback until the six-node promotion gate
        # atomically installs the separately hashed live Caddyfile.
        self.production_public_gate_open = True
        self.public_gate_intent_sha256: str | None = None
        self.public_gate_receipt_sha256: str | None = None
        self.legacy_interlock_interpreters: dict[str, dict[str, Any]] = {}
        # Crossing the capture-bound network-retirement boundary is
        # intentionally one way: the legacy writer/start barriers remain in
        # force, but the owned full-host quarantine is removed so Caddy/ACME
        # and v3 QUIC can operate.  A rollback after this point may only
        # converge on the sealed maintenance edge; it must never resurrect
        # legacy nginx or a v0.7 writer.
        self.production_quarantine_retired: set[str] = set()
        self.production_gateway_security_receipts: dict[str, dict[str, Any]] = {}
        self.production_tls_evidence: dict[str, dict[str, dict[str, Any]]] = {
            "preflight": {},
            "post-rollout": {},
        }

    @property
    def binary(self) -> str:
        return self.manifest["artifacts"]["binary"]["path"]

    @property
    def chain(self) -> dict[str, Any]:
        return self.manifest["chain"]

    @property
    def validators(self) -> list[dict[str, Any]]:
        return self.manifest["validators"]

    @property
    def checks(self) -> dict[str, Any]:
        return self.manifest["checks"]

    def say(self, message: str) -> None:
        print(message, file=self.output, flush=True)

    def _legacy_late_fork_source_set(self) -> dict[str, Any]:
        artifact = self.manifest["artifacts"]["legacy_late_fork_source_set"]
        raw = self._validate_operator_file(
            Path(artifact["path"]),
            artifact["sha256"],
            "legacy late-fork source set",
            modes={0o400},
            owners={os.getuid()},
            nlink=1,
            maximum=4 * 1024 * 1024,
        )
        try:
            value = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            fail(f"legacy late-fork source set is not JSON: {error}")
        if not isinstance(value, dict) or raw != canonical_bytes(value):
            fail("legacy late-fork source set is not canonical JSON")
        return value

    def _validate_late_fork_status(
        self, raw: bytes, *, require_healthy: bool
    ) -> dict[str, Any]:
        source_set = self._legacy_late_fork_source_set()
        try:
            status = require_keys(
                json.loads(raw),
                "legacy late-fork interlock status",
                (
                    "schema", "source_main_commit", "boundary_sha256",
                    "source_set_sha256", "tool_sha256", "sampled_at", "expires_at",
                    "poll_interval_seconds", "max_staleness_seconds", "observations",
                    "state", "gate_reason", "incident_sha256",
                    "required_community_observations",
                    "healthy_community_observations", "global_absence_claimed",
                ),
            )
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            fail(f"legacy late-fork interlock status is invalid JSON: {error}")
        tool_sha = self.manifest["artifacts"]["legacy_late_fork_interlock_tool"][
            "sha256"
        ]
        if (
            status["schema"]
            != "arc.recovery.legacy-late-fork-interlock-status.v2"
            or status["source_main_commit"]
            != self.manifest["provenance"]["source_main_commit"]
            or status["boundary_sha256"]
            != self.chain["legacy_maintenance_boundary_sha256"]
            or status["source_set_sha256"]
            != self.chain["legacy_late_fork_source_set_sha256"]
            or status["tool_sha256"] != tool_sha
            or status["poll_interval_seconds"] != 30
            or status["max_staleness_seconds"] != 90
            or status["global_absence_claimed"] is not False
        ):
            fail("legacy late-fork interlock status identity/policy differs")
        try:
            sampled = dt.datetime.strptime(
                status["sampled_at"], "%Y-%m-%dT%H:%M:%SZ"
            ).replace(tzinfo=dt.timezone.utc)
            expires = dt.datetime.strptime(
                status["expires_at"], "%Y-%m-%dT%H:%M:%SZ"
            ).replace(tzinfo=dt.timezone.utc)
        except (TypeError, ValueError):
            fail("legacy late-fork interlock status timestamps are not canonical UTC")
        now = dt.datetime.now(dt.timezone.utc)
        if (
            expires - sampled != dt.timedelta(seconds=90)
            or sampled > now + dt.timedelta(seconds=5)
            or expires < now
        ):
            fail("legacy late-fork interlock status is stale or future-dated")
        observations = status["observations"]
        coordinates = [
            ("retired", row["name"], row["origin"])
            for row in source_set["monitored_retired_origins"]
        ] + [
            ("community", row["name"], row["origin"])
            for row in source_set["monitored_community_origins"]
        ]
        if not isinstance(observations, list) or len(observations) != len(coordinates):
            fail("legacy late-fork interlock observation inventory differs")
        normalized: list[dict[str, Any]] = []
        for index, (raw_row, coordinate) in enumerate(zip(observations, coordinates)):
            row = require_keys(
                raw_row,
                f"legacy late-fork observation {index}",
                (
                    "name", "origin", "scope", "outcome", "height",
                    "block_hash", "state_root", "response_sha256",
                ),
            )
            if (row["scope"], row["name"], row["origin"]) != coordinate:
                fail("legacy late-fork interlock observation coordinate differs")
            if row["outcome"] not in {"observed", "inconsistent", "unreachable"}:
                fail("legacy late-fork interlock observation outcome differs")
            if row["outcome"] == "observed":
                if (
                    isinstance(row["height"], bool)
                    or not isinstance(row["height"], int)
                    or row["height"] <= 0
                ):
                    fail("legacy late-fork observed height is invalid")
                bare_hash(row["block_hash"], "legacy late-fork block hash")
                bare_hash(row["state_root"], "legacy late-fork state root")
            elif any(
                row[field] is not None
                for field in ("height", "block_hash", "state_root")
            ):
                fail("legacy late-fork unavailable observation carries a commitment")
            response = row["response_sha256"]
            if row["outcome"] == "unreachable":
                if response is not None:
                    fail("legacy late-fork unreachable observation carries response hashes")
            else:
                response = require_keys(
                    response,
                    f"legacy late-fork observation {index} response roots",
                    ("info_before", "latest", "exact", "info_after"),
                )
                for label, value in response.items():
                    bare_hash(value, f"legacy late-fork observation {index} {label}")
            normalized.append(row)
        community = [row for row in normalized if row["scope"] == "community"]
        healthy_community = sum(row["outcome"] == "observed" for row in community)
        if (
            status["required_community_observations"] != len(community)
            or status["healthy_community_observations"] != healthy_community
        ):
            fail("legacy late-fork community observation counts differ")
        incident = status["incident_sha256"]
        if incident is not None:
            bare_hash(incident, "legacy late-fork incident sha256")
            expected_state = "MAINTENANCE"
            expected_reason = "latched-legacy-source-incident"
        elif healthy_community != len(community):
            expected_state = "MAINTENANCE"
            expected_reason = "community-source-observation-unavailable"
        else:
            expected_state = "HEALTHY"
            expected_reason = "capture-bound-retirement-tripwire-clear"
        cutoff = source_set["observed_cutoff_height"]
        requires_incident = any(
            (
                row["scope"] == "retired"
                and row["outcome"] in {"observed", "inconsistent"}
            )
            or (
                row["scope"] == "community"
                and row["outcome"] == "observed"
                and row["height"] > cutoff
            )
            for row in normalized
        )
        if requires_incident and incident is None:
            fail("legacy late-fork status omitted a required latched incident")
        if status["state"] != expected_state or status["gate_reason"] != expected_reason:
            fail("legacy late-fork status gate reason/state binding differs")
        if require_healthy and (expected_state != "HEALTHY" or incident is not None):
            fail("legacy late-fork interlock is not healthy at the publication boundary")
        if raw != canonical_bytes(status):
            fail("legacy late-fork interlock status is not canonical JSON")
        return status

    @staticmethod
    def _validate_operator_file(
        path: Path,
        expected_sha256: str,
        label: str,
        *,
        modes: set[int],
        owners: set[int],
        nlink: int | None,
        maximum: int,
    ) -> bytes:
        if not path.is_absolute() or os.path.normpath(os.fspath(path)) != os.fspath(path):
            fail(f"{label} path must be absolute and normalized")
        expected = bare_hash(expected_sha256, f"{label} sha256")
        try:
            fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0))
        except OSError as error:
            fail(f"cannot open {label} through a no-follow boundary: {error}")
        try:
            before = os.fstat(fd)
            visible = path.lstat()
            identity = lambda item: (
                item.st_dev, item.st_ino, item.st_mode, item.st_uid, item.st_gid,
                item.st_nlink, item.st_size, item.st_mtime_ns, item.st_ctime_ns,
            )
            if (
                stat.S_ISLNK(visible.st_mode)
                or not stat.S_ISREG(before.st_mode)
                or identity(before) != identity(visible)
                or stat.S_IMODE(before.st_mode) not in modes
                or before.st_uid not in owners
                or (nlink is not None and before.st_nlink != nlink)
                or before.st_size <= 0
                or before.st_size > maximum
            ):
                fail(f"{label} owner/type/mode/link/size identity differs")
            chunks: list[bytes] = []
            remaining = maximum + 1
            while remaining:
                chunk = os.read(fd, min(1024 * 1024, remaining))
                if not chunk:
                    break
                chunks.append(chunk)
                remaining -= len(chunk)
            payload = b"".join(chunks)
            if len(payload) != before.st_size or identity(before) != identity(os.fstat(fd)):
                fail(f"{label} changed while it was read")
            if sha256_bytes(payload) != expected:
                fail(f"{label} differs from its reviewed SHA-256")
            return payload
        finally:
            os.close(fd)

    def configure_production_transport(self) -> None:
        """Freeze every production transport input into one non-inheriting contract."""
        if self.manifest["mode"] != "production" or self.production_transport_ready:
            return
        rows, payloads = verify_production_input_stage(self.manifest)
        stage_root = Path(self.manifest["artifacts"]["production_input_stage_manifest"]["path"]).parent
        known = Path(self.manifest["artifacts"]["ssh_known_hosts"]["path"])
        identity = stage_root / rows["ssh_identity"]["path"]
        chain = self.manifest["provenance"]["validator_key_receipt_chain"]
        required_environment = (
            "ARC_RECOVERY_PYTHON_PATH", "ARC_RECOVERY_PYTHON_SHA256",
            "ARC_RECOVERY_SSH_KNOWN_HOSTS", "ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256",
            "ARC_RECOVERY_SSH_IDENTITY", "ARC_RECOVERY_SSH_IDENTITY_SHA256",
            "ARC_RECOVERY_SSH_SHA256", "ARC_RECOVERY_SCP_SHA256",
            "ARC_RECOVERY_RCLONE_PATH", "ARC_RECOVERY_RCLONE_SHA256",
            "ARC_RECOVERY_RCLONE_CONFIG",
        )
        missing = [name for name in required_environment if not os.environ.get(name)]
        if missing:
            fail("production transport pin environment is missing: " + ", ".join(missing))
        if os.environ.get("ARC_RECOVERY_SSH_USER", "root") != "root":
            fail("production transport requires ARC_RECOVERY_SSH_USER=root")
        exact_paths = {
            "ARC_RECOVERY_SSH_KNOWN_HOSTS": known,
            "ARC_RECOVERY_SSH_IDENTITY": identity,
        }
        for variable, wanted in exact_paths.items():
            if Path(os.environ[variable]) != wanted:
                fail(f"{variable} must equal the exact authenticated production-stage path")
        exact_hashes = {
            "ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256": self.manifest["artifacts"]["ssh_known_hosts"]["sha256"],
            "ARC_RECOVERY_SSH_IDENTITY_SHA256": chain["ssh_identity_sha256"],
            "ARC_RECOVERY_SSH_SHA256": chain["ssh_sha256"],
            "ARC_RECOVERY_SCP_SHA256": chain["scp_sha256"],
        }
        for variable, wanted in exact_hashes.items():
            if os.environ[variable] != wanted:
                fail(f"{variable} differs from the authenticated validator receipt chain")
        ssh_path = Path("/usr/bin/ssh")
        scp_path = Path("/usr/bin/scp")
        python_path = Path(os.environ["ARC_RECOVERY_PYTHON_PATH"])
        if re.fullmatch(r"/usr/bin/python3(?:\.[0-9]+)?", os.fspath(python_path)) is None:
            fail("ARC_RECOVERY_PYTHON_PATH must be one normalized /usr/bin/python3[.VERSION] path")
        try:
            freeze = json.loads(payloads["freeze_plan"])
        except (UnicodeError, json.JSONDecodeError):
            fail("staged freeze plan is invalid JSON during transport freeze")
        if (
            freeze.get("operator_python_path") != os.fspath(python_path)
            or freeze.get("operator_python_sha256") != os.environ["ARC_RECOVERY_PYTHON_SHA256"]
        ):
            fail("operator Python path/hash differs from the sealed freeze plan")
        operator_uid = os.geteuid()
        self._validate_operator_file(
            ssh_path, chain["ssh_sha256"], "production SSH executable",
            modes={0o555, 0o755}, owners={0}, nlink=None, maximum=64 * 1024 * 1024,
        )
        self._validate_operator_file(
            scp_path, chain["scp_sha256"], "production SCP executable",
            modes={0o555, 0o755}, owners={0}, nlink=None, maximum=64 * 1024 * 1024,
        )
        self._validate_operator_file(
            python_path, os.environ["ARC_RECOVERY_PYTHON_SHA256"], "production Python executable",
            modes={0o555, 0o755}, owners={0}, nlink=None, maximum=64 * 1024 * 1024,
        )
        known_payload = self._validate_operator_file(
            known, exact_hashes["ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256"], "production known-hosts",
            modes={0o400}, owners={operator_uid}, nlink=1, maximum=64 * 1024,
        )
        self._validate_operator_file(
            identity, exact_hashes["ARC_RECOVERY_SSH_IDENTITY_SHA256"], "production SSH identity",
            modes={0o400}, owners={operator_uid}, nlink=1, maximum=128 * 1024,
        )
        try:
            known_lines = known_payload.decode("ascii").splitlines()
        except UnicodeDecodeError:
            fail("production known-hosts is not ASCII")
        if len(known_lines) != REQUIRED_VALIDATORS or not known_payload.endswith(b"\n"):
            fail("production known-hosts must contain exactly six LF-terminated rows")
        seen_host_keys: set[bytes] = set()
        for index, (line, (_node, host)) in enumerate(zip(known_lines, PRODUCTION_FLEET)):
            fields = line.split()
            if len(fields) != 3 or fields[:2] != [host, "ssh-ed25519"]:
                fail(f"production known-hosts row {index} differs from the fixed Ed25519 fleet")
            try:
                blob = base64.b64decode(fields[2], validate=True)
            except binascii.Error:
                fail(f"production known-hosts row {index} is invalid base64")
            prefix = struct.pack(">I", 11) + b"ssh-ed25519" + struct.pack(">I", 32)
            if len(blob) != len(prefix) + 32 or not blob.startswith(prefix) or blob in seen_host_keys:
                fail(f"production known-hosts row {index} is not one unique Ed25519 key")
            seen_host_keys.add(blob)
        rclone_path = Path(os.environ["ARC_RECOVERY_RCLONE_PATH"])
        rclone_config = Path(os.environ["ARC_RECOVERY_RCLONE_CONFIG"])
        self._validate_operator_file(
            rclone_path, os.environ["ARC_RECOVERY_RCLONE_SHA256"], "production rclone executable",
            modes={0o500, 0o555, 0o700, 0o755}, owners={0, operator_uid}, nlink=1,
            maximum=256 * 1024 * 1024,
        )
        try:
            rclone_config_sha = sha256_bytes(rclone_config.read_bytes())
        except OSError as error:
            fail(f"cannot read production rclone config: {error}")
        self._validate_operator_file(
            rclone_config, rclone_config_sha, "production rclone config",
            modes={0o600}, owners={operator_uid}, nlink=1, maximum=1024 * 1024,
        )
        self.production_ssh_path = ssh_path
        self.production_scp_path = scp_path
        self.production_rclone_path = rclone_path
        self.production_rclone_config = rclone_config
        self.production_known_hosts = known
        self.production_ssh_identity = identity
        self.production_transport_env = {
            "HOME": os.fspath(stage_root / "private"),
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "LANG": "C", "LC_ALL": "C", "TZ": "UTC",
        }
        self.production_transport_pins = {
            "ssh": chain["ssh_sha256"],
            "scp": chain["scp_sha256"],
            "known_hosts": exact_hashes["ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256"],
            "identity": exact_hashes["ARC_RECOVERY_SSH_IDENTITY_SHA256"],
            "python_path": os.fspath(python_path),
            "python": os.environ["ARC_RECOVERY_PYTHON_SHA256"],
            "rclone": os.environ["ARC_RECOVERY_RCLONE_SHA256"],
            "rclone_config": rclone_config_sha,
        }
        self.production_transport_ready = True

    def _assert_production_ssh_transport(self) -> None:
        if not self.production_transport_ready or any(
            item is None
            for item in (
                self.production_ssh_path, self.production_scp_path,
                self.production_known_hosts, self.production_ssh_identity,
            )
        ):
            fail("production SSH transport is not frozen")
        operator_uid = os.geteuid()
        self._validate_operator_file(
            self.production_ssh_path, self.production_transport_pins["ssh"],
            "production SSH executable", modes={0o555, 0o755}, owners={0},
            nlink=None, maximum=64 * 1024 * 1024,
        )
        self._validate_operator_file(
            self.production_scp_path, self.production_transport_pins["scp"],
            "production SCP executable", modes={0o555, 0o755}, owners={0},
            nlink=None, maximum=64 * 1024 * 1024,
        )
        self._validate_operator_file(
            self.production_known_hosts, self.production_transport_pins["known_hosts"],
            "production known-hosts", modes={0o400}, owners={operator_uid},
            nlink=1, maximum=64 * 1024,
        )
        self._validate_operator_file(
            self.production_ssh_identity, self.production_transport_pins["identity"],
            "production SSH identity", modes={0o400}, owners={operator_uid},
            nlink=1, maximum=128 * 1024,
        )

    def _assert_production_rclone_transport(self) -> None:
        if (
            not self.production_transport_ready
            or self.production_rclone_path is None
            or self.production_rclone_config is None
        ):
            fail("production rclone transport is not frozen")
        operator_uid = os.geteuid()
        self._validate_operator_file(
            self.production_rclone_path, self.production_transport_pins["rclone"],
            "production rclone executable", modes={0o500, 0o555, 0o700, 0o755},
            owners={0, operator_uid}, nlink=1, maximum=256 * 1024 * 1024,
        )
        self._validate_operator_file(
            self.production_rclone_config, self.production_transport_pins["rclone_config"],
            "production rclone config", modes={0o600}, owners={operator_uid},
            nlink=1, maximum=1024 * 1024,
        )

    def _rollback_journal_write(self, name: str, value: Mapping[str, Any]) -> str:
        if (
            not self.rollback_journal_reserved
            or self.rollback_journal is None
            or re.fullmatch(r"[A-Z0-9][A-Z0-9_.-]{0,127}\.json", name) is None
        ):
            fail("rollback journal is not reserved or the record name is unsafe")
        payload = canonical_bytes(value)
        path = self.rollback_journal / name
        try:
            _exclusive_write(path, payload, 0o400)
            self._fsync_directory(self.rollback_journal)
        except (OSError, RolloutError) as error:
            fail(f"cannot durably append rollback journal record {name}: {error}")
        return sha256_bytes(payload)

    def _rollback_journal_read(self, name: str) -> dict[str, Any]:
        if self.rollback_journal is None or re.fullmatch(
            r"[A-Z0-9][A-Z0-9_.-]{0,127}\.json", name
        ) is None:
            fail("rollback journal record name is unsafe")
        path = self.rollback_journal / name
        try:
            details = path.lstat()
            if (
                stat.S_ISLNK(details.st_mode)
                or not stat.S_ISREG(details.st_mode)
                or details.st_uid != os.geteuid()
                or stat.S_IMODE(details.st_mode) != 0o400
                or details.st_nlink != 1
                or details.st_size <= 0
                or details.st_size > 4 * 1024 * 1024
            ):
                fail(f"rollback journal record is unsafe: {name}")
            payload = path.read_bytes()
            value = json.loads(payload)
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            fail(f"cannot read rollback journal record {name}: {error}")
        if not isinstance(value, dict) or payload != canonical_bytes(value):
            fail(f"rollback journal record is noncanonical: {name}")
        return value

    def _rollback_journal_bind(self, name: str, value: Mapping[str, Any]) -> str:
        """Create one deterministic journal root or exact-match its retry."""

        if self.rollback_journal is None:
            fail("rollback journal is unavailable")
        path = self.rollback_journal / name
        if path.exists() or path.is_symlink():
            existing = self._rollback_journal_read(name)
            if existing != dict(value):
                fail(f"existing rollback journal binding differs: {name}")
            return sha256_bytes(canonical_bytes(existing))
        return self._rollback_journal_write(name, value)

    def _rollback_journal_event(
        self,
        sequence: int,
        phase: str,
        state: str,
        *,
        node: Mapping[str, Any] | None = None,
    ) -> None:
        """Durably bracket every forward mutation phase.

        These records are deliberately create-only.  Their contents are not a
        resume cursor: once HEADER exists without SUCCESS, a later process can
        only restore the original HEADER baselines.  The records instead make
        the exact crash boundary independently auditable.
        """
        if not (1 <= sequence <= 999):
            fail("rollback journal forward sequence is outside its fixed range")
        if re.fullmatch(r"[A-Z][A-Z0-9-]{0,47}", phase) is None:
            fail("rollback journal forward phase is unsafe")
        if state not in {"STARTED", "COMPLETE"}:
            fail("rollback journal forward state is unsupported")
        value: dict[str, Any] = {
            "schema": "arc.recovery.production-forward-event.v1",
            "rollout_manifest_sha256": self.digest,
            "sequence": sequence,
            "phase": phase.lower(),
            "state": state.lower(),
        }
        if node is not None:
            value["node"] = node["name"]
            value["host"] = node["host"]
        self._rollback_journal_write(
            f"FORWARD-{sequence:03d}-{phase}-{state}.json", value
        )

    def _retirement_boundary_started(self) -> bool:
        """Return whether the one-way legacy network handoff is durable."""

        if self.rollback_journal is None:
            return bool(self.production_quarantine_retired)
        path = self.rollback_journal / "FORWARD-003-QUARANTINE-RETIRE-STARTED.json"
        if not path.exists() and not path.is_symlink():
            return bool(self.production_quarantine_retired)
        event = self._rollback_journal_read(path.name)
        expected = {
            "schema": "arc.recovery.production-forward-event.v1",
            "rollout_manifest_sha256": self.digest,
            "sequence": 3,
            "phase": "quarantine-retire",
            "state": "started",
        }
        if event != expected:
            fail("quarantine retirement boundary journal record differs")
        return True

    def _next_rollback_run(self) -> int:
        if not self.rollback_journal_reserved or self.rollback_journal is None:
            fail("rollback journal is not reserved")
        highest = 0
        try:
            entries = list(os.scandir(self.rollback_journal))
        except OSError as error:
            fail(f"cannot enumerate rollback journal: {error}")
        for entry in entries:
            match = re.fullmatch(r"ROLLBACK-RUN-([0-9]{4})-STARTED\.json", entry.name)
            if match is not None:
                try:
                    details = entry.stat(follow_symlinks=False)
                except OSError as error:
                    fail(f"cannot inspect rollback run record: {error}")
                if not stat.S_ISREG(details.st_mode) or stat.S_ISLNK(details.st_mode):
                    fail("rollback run record has an unsafe identity")
                highest = max(highest, int(match.group(1)))
        if highest >= 9999:
            fail("rollback journal exhausted its bounded recovery runs")
        return highest + 1

    def _validate_terminal_rollback_receipt(
        self, receipt: Mapping[str, Any]
    ) -> None:
        if receipt.get("schema") == "arc.recovery.production-retired-maintenance-receipt.v1":
            receipt = require_keys(
                receipt,
                "production retired-maintenance receipt",
                (
                    "schema", "rollout_manifest_sha256", "rollback_run",
                    "original_error_type", "original_error_sha256", "complete",
                    "preservation_policy", "header_sha256", "results",
                    "maintenance_intent_sha256", "rollback_mode",
                ),
            )
            if self.rollback_journal is None:
                fail("rollback journal is unavailable")
            maintenance_intent = self._rollback_journal_read(
                "RETIREMENT-SAFE-MAINTENANCE-INTENT.json"
            )
            if (
                receipt["rollout_manifest_sha256"] != self.digest
                or receipt["complete"] is not True
                or receipt["rollback_mode"] != "retired-maintenance-safe"
                or receipt["preservation_policy"]
                != "data-history-artifacts-preserved-maintenance-only-no-legacy-restart"
                or not isinstance(receipt["rollback_run"], int)
                or isinstance(receipt["rollback_run"], bool)
                or receipt["rollback_run"] < 1
                or not LOWER_HEX_32_RE.fullmatch(
                    str(receipt["original_error_sha256"])
                )
                or not LOWER_HEX_32_RE.fullmatch(str(receipt["header_sha256"]))
                or sha256_bytes(
                    (self.rollback_journal / "HEADER.json").read_bytes()
                )
                != receipt["header_sha256"]
                or sha256_bytes(canonical_bytes(maintenance_intent))
                != receipt["maintenance_intent_sha256"]
            ):
                fail("existing retired-maintenance completion receipt differs")
            results = receipt["results"]
            if not isinstance(results, list) or len(results) != REQUIRED_VALIDATORS:
                fail("retired-maintenance receipt omits an exact six-host result set")
            for ordinal, (result, node) in enumerate(
                zip(results, reversed(self.validators)), 1
            ):
                result = require_keys(
                    result,
                    f"retired-maintenance result {ordinal}",
                    (
                        "schema", "rollout_manifest_sha256", "rollback_run",
                        "ordinal", "node", "host", "state", "proof",
                        "proof_sha256",
                    ),
                )
                if (
                    result["schema"] != "arc.recovery.production-rollback-attempt.v2"
                    or result["rollout_manifest_sha256"] != self.digest
                    or result["rollback_run"] != receipt["rollback_run"]
                    or result["ordinal"] != ordinal
                    or (result["node"], result["host"])
                    != (node["name"], node["host"])
                    or result["state"] != "retired-maintenance-and-proved"
                    or sha256_bytes(canonical_bytes(result["proof"]))
                    != result["proof_sha256"]
                ):
                    fail("retired-maintenance result binding differs")
                proof = require_keys(
                    result["proof"],
                    f"retired-maintenance proof {ordinal}",
                    (
                        "schema", "node", "retirement_receipt_sha256",
                        "maintenance_intent_sha256", "states",
                        "public_listener_counts", "checks",
                    ),
                )
                expected_states = {
                    f"{service}_{state}"
                    for service in (
                        "validator", "gateway", "filter", "interlock", "archive", "nginx"
                    )
                    for state in ("active", "enabled")
                }
                if (
                    proof["schema"]
                    != "arc.recovery.production-retired-maintenance-host.v1"
                    or proof["node"] != node["name"]
                    or proof["maintenance_intent_sha256"]
                    != receipt["maintenance_intent_sha256"]
                    or not LOWER_HEX_32_RE.fullmatch(
                        str(proof["retirement_receipt_sha256"])
                    )
                    or not isinstance(proof["states"], dict)
                    or set(proof["states"]) != expected_states
                    or any(not isinstance(value, bool) for value in proof["states"].values())
                    or proof["states"]["validator_active"] is not False
                    or proof["states"]["validator_enabled"] is not False
                    or proof["states"]["gateway_active"] is not True
                    or proof["states"]["gateway_enabled"] is not True
                    or proof["states"]["filter_active"] is not True
                    or proof["states"]["filter_enabled"] is not True
                    or proof["states"]["interlock_active"] is not True
                    or proof["states"]["interlock_enabled"] is not True
                    or proof["states"]["nginx_active"] is not False
                    or proof["states"]["nginx_enabled"] is not False
                    or proof["public_listener_counts"] != {"80": 1, "443": 1}
                    or proof["checks"] != {
                        "interlock_gate_status": 204,
                        "maintenance_health_status": 503,
                        "legacy_start_barrier_active": True,
                        "quarantine_retired": True,
                    }
                ):
                    fail("retired-maintenance proof is not exact/fail-closed")
            return
        receipt = require_keys(
            receipt,
            "production rollback receipt",
            (
                "schema", "rollout_manifest_sha256", "rollback_run",
                "original_error_type", "original_error_sha256", "complete",
                "preservation_policy", "header_sha256", "results",
            ),
        )
        if (
            receipt["schema"] != "arc.recovery.production-rollback-receipt.v2"
            or receipt["rollout_manifest_sha256"] != self.digest
            or receipt["complete"] is not True
            or receipt["preservation_policy"]
            != "data-history-artifacts-configs-logs-preserved-no-deletion"
            or not isinstance(receipt["rollback_run"], int)
            or isinstance(receipt["rollback_run"], bool)
            or receipt["rollback_run"] < 1
            or not LOWER_HEX_32_RE.fullmatch(str(receipt["original_error_sha256"]))
            or not LOWER_HEX_32_RE.fullmatch(str(receipt["header_sha256"]))
        ):
            fail("existing rollback completion receipt differs")
        if self.rollback_journal is None:
            fail("rollback journal is unavailable")
        header_payload = (self.rollback_journal / "HEADER.json").read_bytes()
        if sha256_bytes(header_payload) != receipt["header_sha256"]:
            fail("rollback completion receipt header binding differs")
        results = receipt["results"]
        if not isinstance(results, list) or len(results) != REQUIRED_VALIDATORS:
            fail("rollback completion receipt omits an exact six-host result set")
        for ordinal, (result, node) in enumerate(
            zip(results, reversed(self.validators)), 1
        ):
            result = require_keys(
                result,
                f"rollback completion result {ordinal}",
                (
                    "schema", "rollout_manifest_sha256", "rollback_run",
                    "ordinal", "node", "host", "state", "proof",
                    "proof_sha256",
                ),
            )
            if (
                result["schema"] != "arc.recovery.production-rollback-attempt.v2"
                or result["rollout_manifest_sha256"] != self.digest
                or result["rollback_run"] != receipt["rollback_run"]
                or result["ordinal"] != ordinal
                or (result["node"], result["host"])
                != (node["name"], node["host"])
                or result["state"] != "restored-and-proved"
                or not isinstance(result["proof"], dict)
                or sha256_bytes(canonical_bytes(result["proof"]))
                != result["proof_sha256"]
            ):
                fail("rollback completion receipt result binding differs")
            proof = result["proof"]
            if set(proof) != {
                "schema", "node", "states", "public_listener_counts"
            } or not isinstance(proof.get("states"), dict) or not isinstance(
                proof.get("public_listener_counts"), dict
            ):
                fail("rollback completion receipt proof is inexact")
            raw_rows = [
                f"schema={proof['schema']}",
                f"node={proof['node']}",
                *(
                    f"{field}={'1' if value else '0'}"
                    for field, value in proof["states"].items()
                ),
                f"public_80_count={proof['public_listener_counts'].get('80')}",
                f"public_443_count={proof['public_listener_counts'].get('443')}",
            ]
            if self._parse_rollback_proof(
                "\n".join(raw_rows),
                node,
                self.production_service_baseline[node["name"]],
            ) != proof or proof["public_listener_counts"] != self.production_public_listener_baseline[node["name"]]:
                fail("rollback completion receipt proof is not reproducible")

    def _validate_success_receipt(self, receipt: Mapping[str, Any]) -> None:
        receipt = require_keys(
            receipt,
            "production rollout success receipt",
            (
                "schema", "rollout_manifest_sha256", "archive_manifest_sha256",
                "freeze_plan_sha256", "capture_id", "complete",
                "preservation_policy", "header_sha256",
                "public_gate_open_receipt_sha256",
                "quarantine_retirement_receipt_sha256",
                "gateway_security_receipt_sha256",
                "public_tls_preflight_evidence_sha256",
                "public_tls_post_rollout_evidence_sha256", "validators",
            ),
        )
        if self.rollback_journal is None:
            fail("rollback journal is unavailable")
        expected_rows = [
            {
                "node": node["name"],
                "host": node["host"],
                "state": "enabled-running-proved",
            }
            for node in self.validators
        ]
        if (
            receipt["schema"] != "arc.recovery.production-rollout-success.v2"
            or receipt["rollout_manifest_sha256"] != self.digest
            or receipt["archive_manifest_sha256"]
            != self.manifest["archive"]["archive_manifest_sha256"]
            or receipt["freeze_plan_sha256"]
            != self.manifest["archive"]["freeze_plan_sha256"]
            or receipt["capture_id"] != self.manifest["archive"]["capture_id"]
            or receipt["complete"] is not True
            or receipt["preservation_policy"]
            != "data-history-artifacts-configs-logs-preserved-no-deletion"
            or receipt["validators"] != expected_rows
            or sha256_bytes((self.rollback_journal / "HEADER.json").read_bytes())
            != receipt["header_sha256"]
            or not (self.rollback_journal / "PUBLIC-GATE-OPEN-RECEIPT.json").is_file()
            or sha256_bytes(
                (self.rollback_journal / "PUBLIC-GATE-OPEN-RECEIPT.json").read_bytes()
            )
            != receipt["public_gate_open_receipt_sha256"]
            or not (
                self.rollback_journal / "QUARANTINE-RETIREMENT-RECEIPT.json"
            ).is_file()
            or sha256_bytes(
                (
                    self.rollback_journal
                    / "QUARANTINE-RETIREMENT-RECEIPT.json"
                ).read_bytes()
            )
            != receipt["quarantine_retirement_receipt_sha256"]
            or not (self.rollback_journal / "GATEWAY-SECURITY-RECEIPT.json").is_file()
            or sha256_bytes(
                (self.rollback_journal / "GATEWAY-SECURITY-RECEIPT.json").read_bytes()
            )
            != receipt["gateway_security_receipt_sha256"]
            or not (
                self.rollback_journal / "PUBLIC-TLS-PREFLIGHT-EVIDENCE.json"
            ).is_file()
            or sha256_bytes(
                (
                    self.rollback_journal
                    / "PUBLIC-TLS-PREFLIGHT-EVIDENCE.json"
                ).read_bytes()
            )
            != receipt["public_tls_preflight_evidence_sha256"]
            or not (
                self.rollback_journal / "PUBLIC-TLS-POST-ROLLOUT-EVIDENCE.json"
            ).is_file()
            or sha256_bytes(
                (
                    self.rollback_journal
                    / "PUBLIC-TLS-POST-ROLLOUT-EVIDENCE.json"
                ).read_bytes()
            )
            != receipt["public_tls_post_rollout_evidence_sha256"]
        ):
            fail("existing production success receipt differs")

    def reserve_rollback_journal(self) -> str:
        """Create and fsync the immutable rollback transaction before mutation."""
        if self.manifest["mode"] != "production" or self.rollback_journal_reserved:
            return self.rollback_journal_state
        if self.rollback_journal is None:
            fail("production execution requires --rollback-journal")
        root = self.rollback_journal
        if not root.is_absolute() or root == Path(root.anchor):
            fail("rollback journal must be an absolute, non-root directory path")
        parent = root.parent
        try:
            details = parent.lstat()
        except OSError as error:
            fail(f"rollback journal parent is unavailable: {error}")
        if (
            not stat.S_ISDIR(details.st_mode)
            or stat.S_ISLNK(details.st_mode)
            or details.st_uid != os.geteuid()
            or stat.S_IMODE(details.st_mode) & 0o022
        ):
            fail("rollback journal parent must be a real operator-owned directory with no group/world write")
        if root.exists() or root.is_symlink():
            try:
                root_details = root.lstat()
            except OSError as error:
                fail(f"rollback journal is unavailable: {error}")
            if (
                stat.S_ISLNK(root_details.st_mode)
                or not stat.S_ISDIR(root_details.st_mode)
                or root_details.st_uid != os.geteuid()
                or stat.S_IMODE(root_details.st_mode) != 0o700
            ):
                fail("existing rollback journal has an unsafe identity")
            self.rollback_journal_reserved = True
            header = self._rollback_journal_read("HEADER.json")
            if set(header) != {
                "schema", "rollout_manifest_sha256", "source_main_commit",
                "freeze_plan_sha256", "capture_id", "validators",
            } or header.get("schema") != "arc.recovery.production-rollback-journal.v2":
                fail("existing rollback journal header is unsupported or inexact")
            expected_scalars = {
                "rollout_manifest_sha256": self.digest,
                "source_main_commit": self.manifest["provenance"]["source_main_commit"],
                "freeze_plan_sha256": self.manifest["archive"]["freeze_plan_sha256"],
                "capture_id": self.manifest["archive"]["capture_id"],
            }
            if any(header.get(field) != wanted for field, wanted in expected_scalars.items()):
                fail("existing rollback journal belongs to a different recovery transaction")
            rows = header["validators"]
            if not isinstance(rows, list) or len(rows) != REQUIRED_VALIDATORS:
                fail("existing rollback journal omits the fixed six baselines")
            baselines: dict[str, dict[str, bool]] = {}
            listener_baselines: dict[str, dict[str, int]] = {}
            service_fields = {
                f"{service}_{state}"
                for service in ("validator", "gateway", "filter", "interlock", "archive", "nginx")
                for state in ("active", "enabled")
            }
            for index, (row, node) in enumerate(zip(rows, self.validators)):
                row = require_keys(
                    row,
                    f"rollback journal validator {index}",
                    ("node", "host", "service_baseline", "public_listener_baseline"),
                )
                services = row["service_baseline"]
                listeners = row["public_listener_baseline"]
                if (
                    (row["node"], row["host"]) != (node["name"], node["host"])
                    or not isinstance(services, dict)
                    or set(services) != service_fields
                    or any(not isinstance(value, bool) for value in services.values())
                    or not isinstance(listeners, dict)
                    or set(listeners) != {"80", "443"}
                    or any(isinstance(value, bool) or not isinstance(value, int) or value < 0 for value in listeners.values())
                ):
                    fail("existing rollback journal baseline mapping differs")
                baselines[node["name"]] = dict(services)
                listener_baselines[node["name"]] = dict(listeners)
            self.production_service_baseline = baselines
            self.production_public_listener_baseline = listener_baselines
            has_success = (root / "SUCCESS-RECEIPT.json").exists()
            has_rollback = (root / "ROLLBACK-RECEIPT.json").exists()
            if has_success and has_rollback:
                fail("rollback journal contains mutually exclusive terminal receipts")
            if has_success:
                receipt = self._rollback_journal_read("SUCCESS-RECEIPT.json")
                self._validate_success_receipt(receipt)
                self.rollback_journal_state = "success"
            elif has_rollback:
                receipt = self._rollback_journal_read("ROLLBACK-RECEIPT.json")
                self._validate_terminal_rollback_receipt(receipt)
                self.rollback_journal_state = "rolled-back"
            else:
                self.rollback_journal_state = "resume-rollback"
            return self.rollback_journal_state
        try:
            os.mkdir(root, 0o700)
            os.chmod(root, 0o700)
            self._fsync_directory(parent)
        except OSError as error:
            fail(f"cannot reserve rollback journal: {error}")
        self.rollback_journal_reserved = True
        self.rollback_journal_state = "forward"
        expected_nodes = {node["name"] for node in self.validators}
        if (
            set(self.production_service_baseline) != expected_nodes
            or set(self.production_public_listener_baseline) != expected_nodes
        ):
            fail("cannot seal rollback journal without all six exact original baselines")
        header = {
            "schema": "arc.recovery.production-rollback-journal.v2",
            "rollout_manifest_sha256": self.digest,
            "source_main_commit": self.manifest["provenance"]["source_main_commit"],
            "freeze_plan_sha256": self.manifest["archive"]["freeze_plan_sha256"],
            "capture_id": self.manifest["archive"]["capture_id"],
            "validators": [{
                "node": node["name"], "host": node["host"],
                "service_baseline": self.production_service_baseline[node["name"]],
                "public_listener_baseline": self.production_public_listener_baseline[node["name"]],
            } for node in self.validators],
        }
        self._rollback_journal_write("HEADER.json", header)
        return self.rollback_journal_state

    @staticmethod
    def _parse_rollback_proof(
        raw: str, node: Mapping[str, Any], baseline: Mapping[str, bool]
    ) -> dict[str, Any]:
        rows: dict[str, str] = {}
        for line in raw.splitlines():
            if line.count("=") != 1:
                fail(f"{node['name']} rollback proof emitted a malformed row")
            key, value = line.split("=", 1)
            if key in rows or re.fullmatch(r"[a-z][a-z0-9_]{0,63}", key) is None:
                fail(f"{node['name']} rollback proof repeated or malformed a field")
            rows[key] = value
        state_fields = {
            f"{service}_{state}"
            for service in ("validator", "gateway", "filter", "interlock", "archive", "nginx")
            for state in ("active", "enabled")
        }
        expected = {"schema", "node", "public_80_count", "public_443_count", *state_fields}
        if set(rows) != expected:
            fail(f"{node['name']} rollback proof omitted an exact post-restore field")
        if rows["schema"] != "arc.recovery.production-rollback-host.v1" or rows["node"] != node["name"]:
            fail(f"{node['name']} rollback proof identity differs")
        for field in state_fields:
            wanted = "1" if baseline[field] else "0"
            if rows[field] != wanted:
                fail(f"{node['name']} rollback proof {field} differs from baseline")
        for field in ("public_80_count", "public_443_count"):
            if re.fullmatch(r"[0-9]+", rows[field]) is None:
                fail(f"{node['name']} rollback proof {field} is malformed")
        return {
            "schema": rows.pop("schema"),
            "node": rows.pop("node"),
            "states": {field: rows[field] == "1" for field in sorted(state_fields)},
            "public_listener_counts": {
                "80": int(rows["public_80_count"]),
                "443": int(rows["public_443_count"]),
            },
        }

    def _legacy_archive_sources(
        self, archived_forks: Sequence[Mapping[str, str]]
    ) -> list[dict[str, Any]]:
        """Derive alternate-source config only from a live, pinned provenance proof.

        The URL is routing metadata. Every archive identity/content field comes
        from the archive server after it has verified its local immutable
        COMPLETE -> ARCHIVE-MANIFEST -> inventory -> binding-index -> binding
        -> ARCCHKPT chain, and the two out-of-band roots are cross-checked
        against this finalized rollout manifest here.
        """
        if not archived_forks:
            return []
        archive = self.manifest["archive"]
        expected_fields = (
            "schema",
            "read_only",
            "classification",
            "capture_id",
            "node",
            "rollout_manifest_sha256",
            "archive_manifest_sha256",
            "complete_sha256",
            "bundle_sha256",
            "inventory_sha256",
            "binding_index_sha256",
            "binding_sha256",
            "checkpoint_sha256",
            "checkpoint_manifest_hash",
            "checkpoint_payload_hash",
            "canonical_checkpoint_height",
            "source_height",
            "source_block_hash",
            "source_state_root",
            "source_consensus_round",
            "recovery_epoch",
            "validator_set_id",
        )
        hash_fields = (
            "capture_id",
            "rollout_manifest_sha256",
            "archive_manifest_sha256",
            "complete_sha256",
            "bundle_sha256",
            "inventory_sha256",
            "binding_index_sha256",
            "binding_sha256",
            "checkpoint_sha256",
            "checkpoint_manifest_hash",
            "checkpoint_payload_hash",
            "source_block_hash",
            "source_state_root",
        )
        sources: list[dict[str, Any]] = []
        seen_nodes: set[str] = set()
        validators_by_name = {node["name"]: node for node in self.validators}
        for index, archived_fork in enumerate(archived_forks):
            requested_node = archived_fork.get("node", "")
            if requested_node not in validators_by_name:
                fail("legacy archive node is not a sealed rollout validator")
            # Reuse the node's already sealed HTTPS origin/certificate. The
            # only permitted archive routing suffix is derived, never supplied.
            base_url = (
                validators_by_name[requested_node]["rpc_url"]
                + f"/legacy/{requested_node}"
            )
            archive_node = {"name": f"legacy-archive-{index}", "rpc_url": base_url}
            proof = require_keys(
                self._http_json(
                    archive_node,
                    "/provenance",
                ),
                f"legacy archive {index} provenance",
                expected_fields,
            )
            self._prove_legacy_archive_browser_contract(archive_node)
            if proof["schema"] != "arc.legacy-archive.query.v1":
                fail(f"legacy archive {index} provenance schema is unsupported")
            if proof["read_only"] is not True or proof["classification"] != "valid_noncanonical_fork":
                fail(f"legacy archive {index} is not an immutable noncanonical fork")
            normalized = {
                field: bare_hash(proof[field], f"legacy archive {index} {field}")
                for field in hash_fields
            }
            node_name = required_string(proof["node"], f"legacy archive {index}.node")
            if not SAFE_ID_RE.fullmatch(node_name):
                fail(f"legacy archive {index}.node must be lowercase DNS-safe text")
            if node_name in seen_nodes:
                fail(f"legacy archive node is duplicated: {node_name}")
            if node_name != requested_node:
                fail(f"legacy archive {index} provenance node differs from its sealed route")
            seen_nodes.add(node_name)
            for field in (
                "canonical_checkpoint_height",
                "source_height",
                "source_consensus_round",
                "recovery_epoch",
                "validator_set_id",
            ):
                if isinstance(proof[field], bool) or not isinstance(proof[field], int) or proof[field] < 0:
                    fail(f"legacy archive {index}.{field} must be a non-negative integer")
            if normalized["capture_id"] != archive["capture_id"]:
                fail(f"legacy archive {index} capture id differs from the sealed rollout")
            if normalized["rollout_manifest_sha256"] != archive["prearchive_rollout_sha256"]:
                fail(f"legacy archive {index} prearchive rollout root differs")
            if normalized["archive_manifest_sha256"] != archive["archive_manifest_sha256"]:
                fail(f"legacy archive {index} archive-manifest root differs")
            if normalized["complete_sha256"] != archive["complete_sha256"]:
                fail(f"legacy archive {index} COMPLETE root differs")
            if normalized["bundle_sha256"] != archived_fork.get("bundle_sha256"):
                fail(f"legacy archive {index} bundle root differs from its sealed row")
            if normalized["inventory_sha256"] != archived_fork.get("inventory_sha256"):
                fail(f"legacy archive {index} inventory root differs from its sealed row")
            if proof["canonical_checkpoint_height"] != self.chain["source_height"]:
                fail(f"legacy archive {index} canonical checkpoint height differs")
            if proof["recovery_epoch"] != self.chain["recovery_epoch"]:
                fail(f"legacy archive {index} recovery epoch differs")
            if proof["validator_set_id"] != self.chain["validator_set_id"]:
                fail(f"legacy archive {index} validator set differs")
            sources.append(
                {
                    "id": f"legacy-fork-{node_name}",
                    "name": f"Preserved legacy fork · {node_name.upper()}",
                    "region": node_name.upper(),
                    "kind": "legacy-fork",
                    "baseUrl": base_url,
                    "enabled": True,
                    "replicaGroup": f"legacy-capture-{archive['capture_id']}",
                    "description": "Explicit immutable historical fork; diagnostic only and never canonical.",
                    "archive": {
                        "schema": "arc.legacy-archive.source.v1",
                        "readOnly": True,
                        "classification": "valid_noncanonical_fork",
                        "captureId": normalized["capture_id"],
                        "node": node_name,
                        "rolloutManifestSha256": normalized["rollout_manifest_sha256"],
                        "archiveManifestSha256": normalized["archive_manifest_sha256"],
                        "completeSha256": normalized["complete_sha256"],
                        "bundleSha256": normalized["bundle_sha256"],
                        "inventorySha256": normalized["inventory_sha256"],
                        "bindingIndexSha256": normalized["binding_index_sha256"],
                        "bindingSha256": normalized["binding_sha256"],
                        "checkpointSha256": normalized["checkpoint_sha256"],
                        "checkpointManifestHash": normalized["checkpoint_manifest_hash"],
                        "checkpointPayloadHash": normalized["checkpoint_payload_hash"],
                        "canonicalCheckpointHeight": proof["canonical_checkpoint_height"],
                        "sourceHeight": proof["source_height"],
                        "sourceBlockHash": normalized["source_block_hash"],
                        "sourceStateRoot": normalized["source_state_root"],
                        "provenancePath": "/provenance",
                    },
                }
            )
        return sources

    def frontend_config(
        self, archived_forks: Sequence[Mapping[str, str]] = ()
    ) -> dict[str, Any]:
        """Derive the public recovered-network config from the sealed manifest.

        The six v3 validators retain and serve blocks 0..H from the checkpoint
        payload, so the selected primary is intentionally both the historical
        and continuation source. No synthetic legacy API is invented.
        """
        if self.manifest["mode"] != "production":
            fail("recovered frontend config requires a production rollout manifest")
        sources = [
            {
                "id": f"v3-{node['name']}",
                "name": f"ARC v3 {node['name'].upper()}",
                "region": node["name"].upper(),
                "kind": "v3",
                "baseUrl": node["rpc_url"],
                "enabled": True,
                "replicaGroup": self.manifest["rollout_id"],
            }
            for node in self.validators
        ]
        sources.extend(self._legacy_archive_sources(archived_forks))
        primary = sources[0]["id"]
        chain = self.chain
        return {
            "schema": "arc.frontend.network.v1",
            "state": "recovered",
            "network": {"name": "ARC Testnet", "chainId": chain["chain_id"]},
            "checkpoint": {
                "height": chain["source_height"],
                "recoveryHeight": chain["transition_height"],
                "legacyPublicMaxHeight": chain["legacy_public_max_height"],
                "blockHash": bare_hash(chain["source_block_hash"], "source block hash"),
                "stateRoot": bare_hash(chain["source_state_root"], "source state root"),
                "manifestHash": bare_hash(
                    chain["approved_checkpoint_manifest_hash"], "checkpoint manifest hash"
                ),
                "boundaryBlockHash": bare_hash(
                    chain["transition_block_hash"], "transition block hash"
                ),
                "boundaryStateRoot": bare_hash(
                    chain["full_state_root"], "transition state root"
                ),
                "recoveryEpoch": chain["recovery_epoch"],
                "validatorSetId": chain["validator_set_id"],
                "protocolVersion": chain["protocol_version"],
                "recoveryDomain": bare_hash(chain["recovery_domain"], "recovery domain"),
                "legacySourceId": primary,
                "v3SourceId": primary,
            },
            "sources": sources,
            "services": {
                "maintenanceInterlock": {
                    "schema": "arc.frontend.maintenance-interlock.v1",
                    "path": "/maintenance/status",
                    "sourceMainCommit": self.manifest["provenance"][
                        "source_main_commit"
                    ],
                    "observedCutoffHeight": chain[
                        "legacy_observed_cutoff_height"
                    ],
                    "sourceSetSha256": chain["legacy_late_fork_source_set_sha256"],
                    "boundarySha256": chain["legacy_maintenance_boundary_sha256"],
                    "toolSha256": self.manifest["artifacts"][
                        "legacy_late_fork_interlock_tool"
                    ]["sha256"],
                    "requiredHealthyReplicas": REQUIRED_VALIDATORS,
                    "maxStalenessSeconds": 90,
                }
            },
            "notices": [
                "Recovered protocol-v3 network; every listed validator serves the retained canonical history and H+1 continuation.",
                *( ["Preserved noncanonical forks are explicit, immutable, read-only archive views and are never selected as canonical."]
                   if archived_forks else [] ),
            ],
        }

    def recovery_cli(
        self,
        action: str,
        node: Mapping[str, Any] | None = None,
        *,
        remote: bool = False,
        data_dir_override: str | None = None,
    ) -> list[str]:
        artifacts = self.manifest["artifacts"]
        if remote:
            if node is None:
                fail("remote recovery command requires a node")
            root = node["remote_root"]
            binary = f"{root}/arc-node"
            genesis = f"{root}/genesis.toml"
            checkpoint = f"{root}/recovery.arcchkpt"
        else:
            binary = artifacts["binary"]["path"]
            genesis = artifacts["genesis"]["path"]
            checkpoint = artifacts["checkpoint"]["path"]
        base = [binary, "recovery", action, "--checkpoint", checkpoint]
        if action == "inspect":
            return base
        base += [
            "--genesis",
            genesis,
            "--approved-manifest-hash",
            bare_hash(self.chain["approved_checkpoint_manifest_hash"], "approved manifest hash"),
            "--recovery-epoch",
            str(self.chain["recovery_epoch"]),
            "--validator-set-id",
            str(self.chain["validator_set_id"]),
        ]
        if action == "import":
            if node is None:
                fail("recovery import requires a node")
            base += ["--data-dir", data_dir_override or node["data_dir"]]
        return base

    def runtime_argv(self, node: Mapping[str, Any], *, remote: bool = False) -> list[str]:
        if remote:
            root = node["remote_root"]
            binary = f"{root}/arc-node"
            genesis = f"{root}/genesis.toml"
        else:
            binary = self.binary
            genesis = self.manifest["artifacts"]["genesis"]["path"]
        peers = [candidate["p2p_advertise"] for candidate in self.validators if candidate["name"] != node["name"]]
        community_origins = [candidate["rpc_url"] for candidate in self.validators]
        inference_args: list[str] = []
        if self.manifest["mode"] == "production":
            inference_args = ["--model", node["model_path"]]
            for start, end in node["shard_ranges"]:
                inference_args.extend(("--shard-range", f"{start}:{end}"))
        rpc_args = ["--rpc", node["rpc_listen"]]
        if remote and self.manifest["mode"] == "production":
            rpc_args = ["--rpc-unix", self.validator_rpc_socket(node)]
        return [
            binary,
            *rpc_args,
            "--p2p-port",
            str(node["p2p_port"]),
            "--stake",
            str(node["stake"]),
            "--data-dir",
            node["data_dir"],
            "--peers",
            ",".join(peers),
            "--genesis",
            genesis,
            "--validator-key-file",
            node["key_file"],
            "--recovery-epoch",
            str(self.chain["recovery_epoch"]),
            "--validator-set-id",
            str(self.chain["validator_set_id"]),
            *(flag for origin in community_origins for flag in ("--community-rpc-url", origin)),
            *inference_args,
            *node["extra_args"],
        ]

    def describe_plan(self) -> None:
        reward = self.checks["reward"]
        self.say("ARC protocol-v3 recovery rollout plan")
        self.say(f"  mode:                       {self.manifest['mode']}")
        self.say(f"  rollout:                    {self.manifest['rollout_id']}")
        self.say(f"  locked rollout sha256:      {self.digest}")
        self.say(f"  checkpoint manifest hash:   {bare_hash(self.chain['approved_checkpoint_manifest_hash'], 'hash')}")
        self.say(f"  preserved source:           #{self.chain['source_height']} {self.chain['source_block_hash']}")
        self.say(f"  recovery boundary:          #{self.chain['transition_height']} {self.chain['transition_block_hash']}")
        self.say(f"  legacy public height floor: must advance past #{self.chain['legacy_public_max_height']}")
        self.say(f"  validators:                 {len(self.validators)} (restart quorum proven in manifest)")
        self.say(
            "  reward gate:                "
            f"{reward['mode']} / protocol_active={str(reward['expect_protocol_active']).lower()} "
            f"/ issuance_ready={str(reward['expect_issuance_ready']).lower()}"
        )
        if self.manifest["mode"] == "production":
            self.say("  public RPC:                 pinned Caddy HTTPS IP gateways; node listeners stay loopback")
            self.say(
                f"  TLS gateway:                Caddy {CADDY_VERSION} linux-amd64 "
                f"sha256={CADDY_LINUX_AMD64_SHA256}"
            )
            self.say("  internal approvals:         six explicit HTTPS origins; no P2P-derived or raw :9090 RPC")
            self.say(
                "  inference topology:          canonical Llama-2-7B / 32 layers / exact 3x replication / 16 layers per validator"
            )
        for node in self.validators:
            shard_note = ""
            if self.manifest["mode"] == "production":
                shard_note = " shards=" + ",".join(
                    f"{start}:{end}" for start, end in node["shard_ranges"]
                )
            self.say(
                f"    {node['name']}: {node['address']} rpc={node['rpc_url']} data={node['data_dir']}{shard_note}"
            )
        self.say("  execute phases: verify artifacts/checkpoint; require fresh dirs; import; start 5+1; prove H/H+1; converge; restart 1-at-a-time; prove rewards")
        self.say("  rollback: stop only newly started v3 services/processes; preserve every data dir and log")

    def verify_checkpoint(self) -> None:
        verify_artifacts(self.manifest)
        inspect = run_checked(self.recovery_cli("inspect"), timeout=300).stdout
        verify = run_checked(self.recovery_cli("verify"), timeout=300).stdout
        try:
            inspected = json.loads(inspect)
            verified = json.loads(verify)
        except json.JSONDecodeError as error:
            fail(f"recovery CLI did not return JSON: {error}")
        expected = {
            "manifest_hash": self.chain["approved_checkpoint_manifest_hash"],
            "genesis_hash": self.chain["genesis_hash"],
            "full_state_root": self.chain["full_state_root"],
            "source_height": self.chain["source_height"],
            "source_consensus_round": self.chain["source_consensus_round"],
            "created_at_unix_ms": self.chain["created_at_unix_ms"],
            "source_block_hash": self.chain["source_block_hash"],
            "source_state_root": self.chain["source_state_root"],
            "transition_height": self.chain["transition_height"],
            "transition_block_hash": self.chain["transition_block_hash"],
            "recovery_domain": self.chain["recovery_domain"],
            "recovery_epoch": self.chain["recovery_epoch"],
            "validator_set_id": self.chain["validator_set_id"],
            "protocol_version": self.chain["protocol_version"],
            "validator_count": REQUIRED_VALIDATORS,
        }
        for output_name, body in (("inspect", inspected), ("verify", verified)):
            for key, wanted in expected.items():
                got = body.get(key)
                if key.endswith("hash") or key.endswith("root"):
                    if bare_hash(got, f"{output_name}.{key}") != bare_hash(wanted, f"manifest.{key}"):
                        fail(f"recovery {output_name} {key} differs from the locked manifest")
                elif got != wanted:
                    fail(f"recovery {output_name} {key} differs: expected {wanted!r}, got {got!r}")
        if verified.get("status") != "VERIFIED_QUORUM" or verified.get("signature_count", 0) < REQUIRED_APPROVALS:
            fail("checkpoint does not carry a verified 5-of-6 signature quorum")
        self.say("PASS checkpoint content, GO pin, v3 boundary, and 5-of-6 signature quorum")

    def verify_execution_provenance(self) -> None:
        if self.manifest["mode"] != "production":
            return
        archive = self.manifest["archive"]
        script_root = Path(__file__).resolve().parent
        expected = (
            (script_root / "archive-fleet-to-drive.sh", archive["archive_orchestrator_sha256"], "archive orchestrator"),
            (script_root / "archive-node.sh", archive["remote_helper_sha256"], "remote archive helper"),
            (Path(__file__).resolve(), archive["rollout_tool_sha256"], "executing rollout tool"),
            (script_root / "recovery-manifest.schema.json", archive["rollout_schema_sha256"], "rollout schema"),
        )
        for path, wanted, label in expected:
            details = path.lstat()
            if stat.S_ISLNK(details.st_mode) or not stat.S_ISREG(details.st_mode):
                fail(f"{label} is missing, non-regular, or a symlink: {path}")
            if sha256_file(path) != wanted:
                fail(f"{label} bytes differ from the SHA-256 sealed in the rollout manifest")

    def verify_production_archive(self, *, verify_live_captures: bool = False) -> str:
        if self.manifest["mode"] != "production":
            fail("remote archive verification is production-only")
        self.configure_production_transport()
        self._assert_production_ssh_transport()
        self._assert_production_rclone_transport()
        archive = self.manifest["archive"]
        if any(archive[field] == "0" * 64 for field in ARCHIVE_FINALIZATION_FIELDS):
            fail("production rollout execution requires a roots-only finalized archive manifest")
        self.verify_execution_provenance()
        verifier = Path(__file__).resolve().parent / "archive-fleet-to-drive.sh"
        command = [
                "/bin/bash",
                str(verifier),
                "verify-complete",
                "--destination",
                archive["destination"],
                "--expected-complete-sha256",
                archive["complete_sha256"],
                "--expected-archive-manifest-sha256",
                archive["archive_manifest_sha256"],
                "--expected-sha256sums-sha256",
                archive["sha256sums_sha256"],
                "--expected-prearchive-rollout-sha256",
                archive["prearchive_rollout_sha256"],
            ]
        for node in self.validators:
            command.extend(("--new-node-paths", node["name"], node["remote_root"], node["data_dir"]))
        if verify_live_captures:
            command.append("--verify-live-captures")
        assert self.production_known_hosts is not None
        assert self.production_ssh_identity is not None
        assert self.production_rclone_path is not None
        assert self.production_rclone_config is not None
        archive_environment = {
            **self.production_transport_env,
            "ARC_RECOVERY_SSH_USER": "root",
            "ARC_RECOVERY_PYTHON_PATH": self.production_transport_pins["python_path"],
            "ARC_RECOVERY_PYTHON_SHA256": self.production_transport_pins["python"],
            "ARC_RECOVERY_SSH_KNOWN_HOSTS": os.fspath(self.production_known_hosts),
            "ARC_RECOVERY_SSH_KNOWN_HOSTS_SHA256": self.production_transport_pins["known_hosts"],
            "ARC_RECOVERY_SSH_IDENTITY": os.fspath(self.production_ssh_identity),
            "ARC_RECOVERY_SSH_IDENTITY_SHA256": self.production_transport_pins["identity"],
            "ARC_RECOVERY_SSH_SHA256": self.production_transport_pins["ssh"],
            "ARC_RECOVERY_SCP_SHA256": self.production_transport_pins["scp"],
            "ARC_RECOVERY_RCLONE_PATH": os.fspath(self.production_rclone_path),
            "ARC_RECOVERY_RCLONE_SHA256": self.production_transport_pins["rclone"],
            "ARC_RECOVERY_RCLONE_CONFIG": os.fspath(self.production_rclone_config),
        }
        output = run_checked(
            command,
            timeout=24 * 60 * 60,
            env=archive_environment,
        ).stdout
        self._assert_production_ssh_transport()
        self._assert_production_rclone_transport()
        match = re.search(r"archive_manifest=([0-9a-f]{64})(?:\s|$)", output)
        if match is None:
            fail("archive verifier did not emit a canonical archive-manifest hash")
        self.production_archive_verified_root = match.group(1)
        self.say(
            f"PASS complete remote archive and every SHA-256-bound object ({match.group(1)})"
        )
        return match.group(1)

    def _rclone_cat_pinned_archive_object(
        self, name: str, expected_sha256: str, *, max_bytes: int
    ) -> bytes:
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}", name):
            fail(f"archive object name is unsafe: {name!r}")
        expected = bare_hash(expected_sha256, f"archive object {name} sha256")
        self.configure_production_transport()
        self._assert_production_rclone_transport()
        assert self.production_rclone_path is not None
        assert self.production_rclone_config is not None
        output = run_checked_bytes(
            [
                os.fspath(self.production_rclone_path),
                "--config",
                os.fspath(self.production_rclone_config),
                "cat",
                f"{self.manifest['archive']['destination']}/{name}",
                "--count",
                str(max_bytes + 1),
            ],
            timeout=600,
            env=self.production_transport_env,
        ).stdout
        self._assert_production_rclone_transport()
        payload = output
        if len(payload) > max_bytes:
            fail(f"archive object {name} exceeds its {max_bytes}-byte safety limit")
        actual = sha256_bytes(payload)
        if actual != expected:
            fail(f"archive object {name} changed after complete-archive verification")
        return payload

    def load_production_archive_metadata(self) -> None:
        """Fetch only hash-pinned small metadata needed to deploy fork readers.

        ``verify_production_archive`` authenticates the complete remote object
        set first. Every byte fetched again here is independently compared with
        the finalized rollout roots, so a mutable remote cannot redirect the
        subsequent local-disk deployment.
        """
        if self.manifest["mode"] != "production":
            fail("legacy archive deployment metadata is production-only")
        archive = self.manifest["archive"]
        if self.production_archive_verified_root != archive["archive_manifest_sha256"]:
            fail(
                "archive deployment metadata requires a same-process complete-archive verification"
            )
        manifest_payload = self._rclone_cat_pinned_archive_object(
            "ARCHIVE-MANIFEST.json",
            archive["archive_manifest_sha256"],
            max_bytes=4 * 1024 * 1024,
        )
        complete_payload = self._rclone_cat_pinned_archive_object(
            "COMPLETE.json",
            archive["complete_sha256"],
            max_bytes=4 * 1024 * 1024,
        )
        with tempfile.TemporaryDirectory(prefix="arc-recovery-archive-metadata-") as temporary:
            root = Path(temporary)
            archive_manifest_path = root / "ARCHIVE-MANIFEST.json"
            complete_path = root / "COMPLETE.json"
            _exclusive_write(archive_manifest_path, manifest_payload, 0o444)
            _exclusive_write(complete_path, complete_payload, 0o444)
            fork_rows = load_legacy_archive_fork_nodes(
                self, archive_manifest_path, complete_path
            )

        deployments: dict[str, dict[str, Any]] = {}
        for row in fork_rows:
            node = row["node"]
            expected_inventory_name = f"legacy-{node}.inventory"
            expected_bundle_name = f"legacy-{node}.tar.zst"
            if (
                row["inventory_name"] != expected_inventory_name
                or row["bundle_name"] != expected_bundle_name
            ):
                fail(f"legacy archive row for {node} uses noncanonical object names")
            inventory_payload = self._rclone_cat_pinned_archive_object(
                expected_inventory_name,
                row["inventory_sha256"],
                max_bytes=64 * 1024,
            )
            inventory = parse_legacy_archive_inventory(
                inventory_payload,
                node=node,
                capture_id=archive["capture_id"],
                rollout_manifest_sha256=archive["prearchive_rollout_sha256"],
            )
            deployments[node] = {
                **row,
                "inventory_payload": inventory_payload,
                "binding_index_sha256": inventory["binding_index_sha256"],
            }
        self.archive_manifest_payload = manifest_payload
        self.archive_complete_payload = complete_payload
        self.legacy_archive_forks = deployments
        self.archive_metadata_loaded = True
        self.say(
            "PASS downloaded and root-verified archive deployment metadata "
            f"({len(deployments)} explicit noncanonical fork(s))"
        )

    def preflight(self) -> str | None:
        self.verify_checkpoint()
        if self.manifest["mode"] == "local":
            self._preflight_local()
            return None
        archive_manifest_sha256 = self.verify_production_archive()
        self.load_production_archive_metadata()
        self._preflight_production()
        return archive_manifest_sha256

    def _preflight_local(self) -> None:
        for node in self.validators:
            key = Path(node["key_file"])
            if not key.is_file() or key.is_symlink():
                fail(f"{node['name']} key file is missing, non-regular, or a symlink")
            if stat.S_IMODE(key.stat().st_mode) != 0o600:
                fail(f"{node['name']} key file must be mode 0600")
            if Path(node["data_dir"]).exists():
                fail(f"{node['name']} data dir already exists; recovery import requires a fresh path")
        self.say("PASS all six local keyfiles are mode 0600 and all six data dirs are absent")

    def ssh(self, node: Mapping[str, Any], script: str, args: Sequence[str] = (), *, timeout: int = 180) -> str:
        for value in args:
            if not SAFE_REMOTE_RE.fullmatch(value):
                fail(f"unsafe remote argument for {node['name']}: {value!r}")
        self._assert_production_ssh_transport()
        assert self.production_ssh_path is not None
        assert self.production_known_hosts is not None
        assert self.production_ssh_identity is not None
        script_sha256 = sha256_bytes(script.encode("utf-8"))
        remote_wrapper = r'''set -eu
expected=$1
shift
case "$expected" in *[!0-9a-f]*|'') exit 1 ;; esac
test "${#expected}" = 64
umask 077
root=/root/.arc-recovery-rollout-helpers
if test -e "$root"; then
  test -d "$root" && test ! -L "$root"
  test "$(/usr/bin/stat -c %u:%g:%a "$root")" = 0:0:700
else
  /usr/bin/mkdir -m 700 -- "$root"
fi
target="$root/$expected.sh"
if test -e "$target"; then
  test -f "$target" && test ! -L "$target"
  test "$(/usr/bin/stat -c %u:%g:%a:%h "$target")" = 0:0:500:1
  test "$(/usr/bin/sha256sum "$target" | /usr/bin/cut -d' ' -f1)" = "$expected"
  /bin/cat >/dev/null
else
  temporary=$(/usr/bin/mktemp "$root/.upload.XXXXXX")
  trap '/bin/rm -f -- "$temporary"' EXIT HUP INT TERM
  /bin/cat > "$temporary"
  test "$(/usr/bin/sha256sum "$temporary" | /usr/bin/cut -d' ' -f1)" = "$expected"
  /bin/chmod 500 -- "$temporary"
  if /bin/ln -- "$temporary" "$target" 2>/dev/null; then :; else
    test -f "$target" && test ! -L "$target"
    test "$(/usr/bin/sha256sum "$target" | /usr/bin/cut -d' ' -f1)" = "$expected"
  fi
  /bin/rm -f -- "$temporary"
  trap - EXIT HUP INT TERM
  test "$(/usr/bin/stat -c %u:%g:%a:%h "$target")" = 0:0:500:1
fi
exec 9<"$target"
test -f /proc/self/fd/9
test "$(/usr/bin/sha256sum /proc/self/fd/9 | /usr/bin/cut -d' ' -f1)" = "$expected"
exec /usr/bin/env -i HOME=/root PATH=/usr/bin:/bin:/usr/sbin:/sbin LANG=C LC_ALL=C /bin/sh /proc/self/fd/9 "$@"
'''
        remote_command = shlex.join(
            [
                "/bin/sh", "-c", remote_wrapper, "/bin/sh",
                script_sha256, *args,
            ]
        )
        command = [
            os.fspath(self.production_ssh_path),
            "-F", "/dev/null",
            "-i", os.fspath(self.production_ssh_identity),
            "-o", f"UserKnownHostsFile={self.production_known_hosts}",
            "-o", "GlobalKnownHostsFile=/dev/null",
            "-o", "HostKeyAlgorithms=ssh-ed25519",
            "-o", "PubkeyAcceptedAlgorithms=ssh-ed25519",
            "-o", "UpdateHostKeys=no",
            "-o", "CanonicalizeHostname=no",
            "-o", "CheckHostIP=yes",
            "-o", "IdentityAgent=none",
            "-o", "IdentitiesOnly=yes",
            "-o", "ProxyCommand=none",
            "-o", "ProxyJump=none",
            "-o", "PasswordAuthentication=no",
            "-o", "PreferredAuthentications=publickey",
            "-o", "NumberOfPasswordPrompts=0",
            "-o", "KbdInteractiveAuthentication=no",
            "-o", "ChallengeResponseAuthentication=no",
            "-o", "GSSAPIAuthentication=no",
            "-o", "ForwardAgent=no",
            "-o", "ForwardX11=no",
            "-o", "ClearAllForwardings=yes",
            "-o", "PermitLocalCommand=no",
            "-o", "RequestTTY=no",
            "-o", "BatchMode=yes",
            "-o", "ConnectTimeout=10",
            "-o", "StrictHostKeyChecking=yes",
            f"{node['ssh_user']}@{node['host']}",
            remote_command,
        ]
        result = run_checked(
            command,
            stdin=script,
            timeout=timeout,
            env=self.production_transport_env,
        ).stdout
        self._assert_production_ssh_transport()
        return result

    def scp(self, node: Mapping[str, Any], local: str, remote: str) -> None:
        if not SAFE_REMOTE_RE.fullmatch(remote):
            fail(f"unsafe remote path for {node['name']}: {remote!r}")
        self._assert_production_ssh_transport()
        assert self.production_scp_path is not None
        assert self.production_ssh_path is not None
        assert self.production_known_hosts is not None
        assert self.production_ssh_identity is not None
        run_checked(
            [
                os.fspath(self.production_scp_path),
                "-S", os.fspath(self.production_ssh_path),
                "-F", "/dev/null",
                "-q",
                "-i", os.fspath(self.production_ssh_identity),
                "-o", f"UserKnownHostsFile={self.production_known_hosts}",
                "-o", "GlobalKnownHostsFile=/dev/null",
                "-o", "HostKeyAlgorithms=ssh-ed25519",
                "-o", "PubkeyAcceptedAlgorithms=ssh-ed25519",
                "-o", "UpdateHostKeys=no",
                "-o", "CanonicalizeHostname=no",
                "-o", "CheckHostIP=yes",
                "-o", "IdentityAgent=none",
                "-o", "IdentitiesOnly=yes",
                "-o", "ProxyCommand=none",
                "-o", "ProxyJump=none",
                "-o", "PasswordAuthentication=no",
                "-o", "PreferredAuthentications=publickey",
                "-o", "NumberOfPasswordPrompts=0",
                "-o", "KbdInteractiveAuthentication=no",
                "-o", "ChallengeResponseAuthentication=no",
                "-o", "GSSAPIAuthentication=no",
                "-o", "ForwardAgent=no",
                "-o", "ForwardX11=no",
                "-o", "ClearAllForwardings=yes",
                "-o", "PermitLocalCommand=no",
                "-o", "RequestTTY=no",
                "-o", "BatchMode=yes",
                "-o", "StrictHostKeyChecking=yes",
                local,
                f"{node['ssh_user']}@{node['host']}:{remote}",
            ],
            timeout=600,
            env=self.production_transport_env,
        )
        self._assert_production_ssh_transport()

    def _preflight_production(self) -> None:
        script = r"""
set -eu
root=$1 data=$2 key=$3 service=$4 gateway_service=$5 filter_service=$6 validator_rpc_socket=$7 model=$8 model_sha=$9 model_size=${10} digest=${11} transition=${12} checkpoint_manifest=${13} runtime_argv_sha=${14} archive_service=${15} archive_root=${16} archive_argv_sha=${17} archive_source=${18} archive_index_sha=${19} archive_filter_port=${20} archive_rpc_socket=${21} p2p_port=${22} archive_user=${23} identity_cli=${24} identity_cli_sha=${25} key_sha=${26} expected_address=${27} gateway_user=${28} interlock_service=${29} interlock_root=${30} interlock_user=${31} interlock_python=${32} interlock_python_sha=${33} filter_user=${34} filter_config_sha=${35} filter_preflight_sha=${36} nginx_version=${37} nginx_sha=${38} public_filter_socket=${39} archive_filter_socket=${40} interlock_socket=${41} gate_group=${42} origin_group=${43} validator_user=${44} retired_rpc_port=${45} retired_archive_rpc_port=${46} interlock_python_identity=${47} capture_id=${48} node_name=${49}
command -v sha256sum >/dev/null
command -v systemctl >/dev/null
command -v curl >/dev/null
command -v ss >/dev/null
command -v cmp >/dev/null
command -v nginx >/dev/null 2>&1 || command -v apt-get >/dev/null
test -f "$interlock_python" && test ! -L "$interlock_python"
test "$(stat -c %d:%i:%u:%g:%a:%h "$interlock_python")" = "$interlock_python_identity"
exec 9<"$interlock_python"
test "$(stat -Lc %d:%i:%u:%g:%a:%h /proc/self/fd/9)" = "$interlock_python_identity"
test "$(sha256sum /proc/self/fd/9 | cut -d' ' -f1)" = "$interlock_python_sha"
owner="$root/.arc-recovery-rollout-owner"
stage="$root/.arc-recovery-stage-complete"
partial="${data}.arc-recovery-import-${digest}"
partial_owner="${partial}.owner"
if test -e "$root"; then
  test -d "$root" && test ! -L "$root"
  test -f "$owner" && test ! -L "$owner"
  test "$(cat "$owner")" = "$digest"
else
  test ! -e "$data"
  test ! -e "$partial"
  test ! -e "$partial_owner"
fi
if test -e "$data"; then
  test -d "$data" && test ! -L "$data"
  /usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C TZ=UTC PYTHONHASHSEED=0 /proc/self/fd/9 -I - "$data/.arc-recovery-rollout.json" "$digest" "$transition" "$checkpoint_manifest" <<'PY'
import json,pathlib,sys
p=pathlib.Path(sys.argv[1]); v=json.loads(p.read_text(encoding="utf-8"))
e={"schema":"arc.recovery.import-complete.v1","rollout_manifest_sha256":sys.argv[2],"transition_height":int(sys.argv[3]),"checkpoint_manifest_hash":sys.argv[4]}
if p.is_symlink() or v != e or p.read_text(encoding="utf-8") != json.dumps(e,sort_keys=True,separators=(",",":"))+"\n": raise SystemExit("existing v3 data is not an exact completed import for this rollout")
PY
  test "$(sha256sum /proc/self/fd/9 | cut -d' ' -f1)" = "$interlock_python_sha"
elif test -e "$partial" || test -e "$partial_owner"; then
  test -f "$partial_owner" && test ! -L "$partial_owner"
  test "$(cat "$partial_owner")" = "$digest"
  test ! -e "$partial" || { test -d "$partial" && test ! -L "$partial"; }
fi
test -f "$key"
test ! -L "$key"
test "$(stat -c %U:%G:%a:%h "$key")" = root:root:600:1
test "$(sha256sum "$key" | cut -d' ' -f1)" = "$key_sha"
test -f "$identity_cli" && test ! -L "$identity_cli"
test "$(stat -c %U:%G:%a:%h "$identity_cli")" = root:root:500:1
exec 8<"$identity_cli"
test -f /proc/self/fd/8
test "$(sha256sum /proc/self/fd/8 | cut -d' ' -f1)" = "$identity_cli_sha"
derived=$(/usr/bin/env -i HOME=/root PATH=/usr/bin:/bin LANG=C LC_ALL=C /proc/self/fd/8 keygen --verify-keyfile "$key")
test "$derived" = "$expected_address" || { printf 'validator key derives a different sealed address\n' >&2; exit 1; }
test "$(sha256sum /proc/self/fd/8 | cut -d' ' -f1)" = "$identity_cli_sha"
test "$(sha256sum "$key" | cut -d' ' -f1)" = "$key_sha"
units="$service $gateway_service $filter_service $interlock_service"
if [ "$archive_service" != none ]; then units="$units $archive_service"; fi
for unit in $units; do
  installed="/etc/systemd/system/$unit"
  if test -e "$installed"; then
    test -f "$stage" && test ! -L "$stage" && test "$(cat "$stage")" = "$digest"
    test -f "$installed" && test ! -L "$installed"
    test "$(stat -c %U:%G:%a:%h "$installed")" = root:root:644:1
    cmp --silent "$root/$unit" "$installed"
    test "$(systemctl show "$unit" --property=FragmentPath --value)" = "$installed"
    test -z "$(systemctl show "$unit" --property=DropInPaths --value)"
  fi
done
test -f "$model"
test ! -L "$model"
test "$(stat -c %s "$model")" = "$model_size"
printf '%s  %s\n' "$model_sha" "$model" | sha256sum --check --strict
# BEGIN ARC PORT OWNERSHIP HELPER
listener_rows() {
  ss -H -ltnp | awk -v port="$1" '$4 ~ (":" port "$") { print }'
}
udp_listener_rows() {
  ss -H -lunp | awk -v port="$1" '$4 ~ (":" port "$") { print }'
}
assert_listener_owner() {
  port=$1 expected_pid=$2 label=$3
  rows=$(listener_rows "$port")
  if [ -z "$rows" ]; then
    if [ -n "$expected_pid" ]; then
      printf '%s service MainPID %s is active but TCP port %s is not listening\n' "$label" "$expected_pid" "$port" >&2
      return 1
    fi
    return 0
  fi
  if [ -z "$expected_pid" ]; then
    printf 'TCP port %s has a foreign listener; no same-rollout %s MainPID is active\n%s\n' "$port" "$label" "$rows" >&2
    return 1
  fi
  while IFS= read -r row; do
    case "$row" in
      *"pid=$expected_pid,"*) ;;
      *)
        printf 'TCP port %s is not owned by same-rollout %s MainPID %s\n%s\n' "$port" "$label" "$expected_pid" "$row" >&2
        return 1
        ;;
    esac
  done <<EOF
$rows
EOF
}
assert_udp_listener_owner() {
  port=$1 expected_pid=$2 label=$3
  rows=$(udp_listener_rows "$port")
  count=$(printf '%s\n' "$rows" | awk 'NF { count += 1 } END { print count + 0 }')
  if [ -z "$expected_pid" ]; then
    if [ "$count" -ne 0 ]; then
      printf 'UDP port %s has a foreign listener; no same-rollout %s MainPID is active\n%s\n' "$port" "$label" "$rows" >&2
      return 1
    fi
    return 0
  fi
  if [ "$count" -ne 1 ]; then
    printf '%s service MainPID %s must own exactly one UDP row for QUIC port %s; found %s\n%s\n' "$label" "$expected_pid" "$port" "$count" "$rows" >&2
    return 1
  fi
  case "$rows" in
    *"pid=$expected_pid,"*) ;;
    *)
      printf 'UDP QUIC port %s is not owned by same-rollout %s MainPID %s\n%s\n' "$port" "$label" "$expected_pid" "$rows" >&2
      return 1
      ;;
  esac
}
assert_unix_listener_owner() {
  socket=$1 expected_pid=$2 label=$3 expected_user=$4 expected_group=$5 expected_mode=$6
  rows=$(ss -H -lxnp | grep -F -- " $socket" || true)
  count=$(printf '%s\n' "$rows" | awk 'NF {n+=1} END {print n+0}')
  if [ -z "$expected_pid" ]; then
    test "$count" = 0 && test ! -e "$socket" && test ! -L "$socket"
    return
  fi
  test "$count" = 1
  case "$rows" in *"pid=$expected_pid,"*) ;; *) return 1 ;; esac
  test -S "$socket" && test ! -L "$socket"
  test "$(stat -c %U:%G:%a:%h "$socket")" = "$expected_user:$expected_group:$expected_mode:1"
}
# END ARC PORT OWNERSHIP HELPER
listeners=$(ss -ltnp | grep -E ':(80|443)[[:space:]]' || true)
if [ -n "$listeners" ] && printf '%s\n' "$listeners" | grep -Ev '(nginx|caddy)' >/dev/null; then
  printf 'ports 80/443 are occupied by an unapproved process\n%s\n' "$listeners" >&2
  exit 1
fi
if printf '%s\n' "$listeners" | grep caddy >/dev/null; then
  systemctl is-active --quiet "$gateway_service"
  test -f "/etc/systemd/system/$gateway_service"
  cmp --silent "$root/$gateway_service" "/etc/systemd/system/$gateway_service"
fi
gateway_pid=""
if systemctl is-active --quiet "$gateway_service"; then
  gateway_pid=$(systemctl show "$gateway_service" --property=MainPID --value)
  case "$gateway_pid" in ''|0|*[!0-9]*) exit 1 ;; esac
  gateway_uid=$(id -u "$gateway_user")
  test "$gateway_uid" != 0
  test "$(awk '/^Uid:/{print $2}' "/proc/$gateway_pid/status")" = "$gateway_uid"
  test -f "$stage" && test ! -L "$stage" && test "$(cat "$stage")" = "$digest"
  test -f "/etc/systemd/system/$gateway_service" && test ! -L "/etc/systemd/system/$gateway_service"
  cmp --silent "$root/$gateway_service" "/etc/systemd/system/$gateway_service"
  test "$(readlink "/proc/$gateway_pid/exe")" = "$root/caddy"
fi
filter_pid=""
if systemctl is-active --quiet "$filter_service"; then
  filter_pid=$(systemctl show "$filter_service" --property=MainPID --value)
  case "$filter_pid" in ''|0|*[!0-9]*) exit 1 ;; esac
  filter_uid=$(id -u "$filter_user")
  test "$filter_uid" != 0
  test "$(awk '/^Uid:/{print $2}' "/proc/$filter_pid/status")" = "$filter_uid"
  test -f "$stage" && test ! -L "$stage" && test "$(cat "$stage")" = "$digest"
  test -f "/etc/systemd/system/$filter_service" && test ! -L "/etc/systemd/system/$filter_service"
  cmp --silent "$root/$filter_service" "/etc/systemd/system/$filter_service"
  test "$(readlink -f "/proc/$filter_pid/exe")" = "$(readlink -f /usr/sbin/nginx)"
  test "$(/usr/bin/dpkg-query -W -f='${Version}' nginx)" = "$nginx_version"
  printf '%s  /usr/sbin/nginx\n' "$nginx_sha" | sha256sum --check --strict
  test -f "$root/nginx-filter.conf" && test ! -L "$root/nginx-filter.conf"
  test -f "$root/arc-nginx-filter-preflight" && test ! -L "$root/arc-nginx-filter-preflight"
  printf '%s  %s/nginx-filter.conf\n' "$filter_config_sha" "$root" | sha256sum --check --strict
  printf '%s  %s/arc-nginx-filter-preflight\n' "$filter_preflight_sha" "$root" | sha256sum --check --strict
fi
expected_pids=""
validator_pid=""
if systemctl is-active --quiet "$service"; then
  pid=$(systemctl show "$service" --property=MainPID --value)
  case "$pid" in ''|0|*[!0-9]*) exit 1 ;; esac
  test "$(readlink "/proc/$pid/exe")" = "$root/arc-node"
  test "$(sha256sum "/proc/$pid/cmdline" | cut -d' ' -f1)" = "$runtime_argv_sha"
  expected_pids="$pid"
  validator_pid=$pid
fi
for port in "$p2p_port" "$archive_filter_port" "$retired_rpc_port" "$retired_archive_rpc_port"; do
  case "$port" in ''|*[!0-9]*) printf 'invalid listener port\n' >&2; exit 1 ;; esac
done
assert_unix_listener_owner "$public_filter_socket" "$filter_pid" rpc-filter "$filter_user" "$gateway_user" 770
assert_listener_owner 18080 "" retired-rpc-filter-tcp
assert_unix_listener_owner "$validator_rpc_socket" "$validator_pid" validator-rpc "$validator_user" "$origin_group" 660
assert_listener_owner "$retired_rpc_port" "" retired-validator-rpc-tcp
assert_udp_listener_owner "$p2p_port" "$validator_pid" validator-quic-p2p
if [ -n "$validator_pid" ]; then
  validator_runtime=${validator_rpc_socket%/*}
  test -d "$validator_runtime" && test ! -L "$validator_runtime"
  test "$(stat -c %U:%G:%a "$validator_runtime")" = "$validator_user:$origin_group:750"
fi
# Caddy's runtime admin API is deliberately disabled.  A local validator or
# filter process therefore has no mutable configuration endpoint to attack.
assert_listener_owner 2019 "" caddy-admin-disabled
interlock_pid=""
if systemctl is-active --quiet "$interlock_service"; then
  interlock_pid=$(systemctl show "$interlock_service" --property=MainPID --value)
  case "$interlock_pid" in ''|0|*[!0-9]*) exit 1 ;; esac
  test -f "$stage" && test ! -L "$stage" && test "$(cat "$stage")" = "$digest"
  test -f "/etc/systemd/system/$interlock_service" && test ! -L "/etc/systemd/system/$interlock_service"
  cmp --silent "$root/$interlock_service" "/etc/systemd/system/$interlock_service"
  test "$(awk '/^Uid:/{print $2}' "/proc/$interlock_pid/status")" = "$(id -u "$interlock_user")"
  test "$(readlink "/proc/$interlock_pid/exe")" = "$interlock_python"
  printf '%s  %s\n' "$interlock_python_sha" "/proc/$interlock_pid/exe" | sha256sum --check --strict
  test -d "$interlock_root" && test ! -L "$interlock_root"
fi
assert_unix_listener_owner "$interlock_socket" "$interlock_pid" late-fork-interlock "$interlock_user" "$gate_group" 660
test -z "$(ss -H -ltnp | awk '$4 ~ /:18081$/ {print}')"
if [ -n "$interlock_pid" ]; then
  interlock_runtime=${interlock_socket%/*}
  test -d "$interlock_runtime" && test ! -L "$interlock_runtime"
  test "$(stat -c %U:%G:%a "$interlock_runtime")" = "$interlock_user:$gate_group:750"
fi
if [ -n "$gateway_pid" ]; then
  assert_listener_owner 80 "$gateway_pid" caddy-https
  assert_listener_owner 443 "$gateway_pid" caddy-https
fi
if [ "$archive_service" != none ]; then
  command -v useradd >/dev/null
  command -v getent >/dev/null
  command -v install >/dev/null
  command -v df >/dev/null
  archive_parent=${archive_root%/*}
  archive_base=/var/lib/arc-legacy-archive
  test -d /var && test ! -L /var
  test -d /var/lib && test ! -L /var/lib
  if test -e "$archive_base" || test -L "$archive_base"; then
    test -d "$archive_base" && test ! -L "$archive_base"
    test "$(stat -c %U:%G:%a "$archive_base")" = root:root:755
  fi
  if test -e "$archive_parent" || test -L "$archive_parent"; then
    test -d "$archive_parent" && test ! -L "$archive_parent"
    test "$(stat -c %U:%G:%a "$archive_parent")" = "root:$archive_user:750"
  fi
  if test -e "$archive_root" || test -L "$archive_root"; then
    test -d "$archive_root" && test ! -L "$archive_root"
    test -f "$archive_root/.arc-recovery-rollout-owner" && test ! -L "$archive_root/.arc-recovery-rollout-owner"
    test "$(cat "$archive_root/.arc-recovery-rollout-owner")" = "$digest"
    test "$(stat -c %U:%G:%a "$archive_root")" = "root:$archive_user:750"
    test "$(stat -c %U:%G:%a "$archive_root/.arc-recovery-rollout-owner")" = "root:$archive_user:440"
  else
    test ! -L "$archive_parent"
  fi
  test -d "$archive_source" && test ! -L "$archive_source"
  for source in binding.files.sha256 binding.json candidate.arcchkpt; do
    test -f "$archive_source/$source" && test ! -L "$archive_source/$source"
    test -z "$(find "$archive_source/$source" -maxdepth 0 -perm /0222 -print -quit)"
  done
  printf '%s  %s\n' "$archive_index_sha" "$archive_source/binding.files.sha256" | sha256sum --check --strict
  archive_hashes=$(/usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C TZ=UTC PYTHONHASHSEED=0 /proc/self/fd/9 -I - "$archive_source/binding.files.sha256" <<'PY'
import pathlib,re,sys
rows={}
for line in pathlib.Path(sys.argv[1]).read_text(encoding='utf-8').splitlines():
    match=re.fullmatch(r'([0-9a-f]{64})  ([A-Za-z0-9_.@/+:-]+)',line)
    if match is None or match.group(2) in rows: raise SystemExit('unsafe or duplicate binding index row')
    rows[match.group(2)]=match.group(1)
if 'binding.json' not in rows or 'candidate.arcchkpt' not in rows: raise SystemExit('binding index omits archive inputs')
print(rows['binding.json'],rows['candidate.arcchkpt'])
PY
  )
  set -- $archive_hashes
  printf '%s  %s\n' "$1" "$archive_source/binding.json" | sha256sum --check --strict
  printf '%s  %s\n' "$2" "$archive_source/candidate.arcchkpt" | sha256sum --check --strict
  test "$(sha256sum /proc/self/fd/9 | cut -d' ' -f1)" = "$interlock_python_sha"
  if test ! -e "$archive_root/candidate.arcchkpt"; then
    required=$(( $(stat -c %s "$archive_source/candidate.arcchkpt") + $(stat -c %s "$root/arc-node" 2>/dev/null || printf 0) + 536870912 ))
    available=$(df -PB1 /var/lib | awk 'NR==2 {print $4}')
    test "$available" -gt "$required" || { printf 'insufficient local disk for immutable fork reader\n' >&2; exit 1; }
  fi
  archive_pid=""
  if systemctl is-active --quiet "$archive_service"; then
    archive_pid=$(systemctl show "$archive_service" --property=MainPID --value)
    case "$archive_pid" in ''|0|*[!0-9]*) exit 1 ;; esac
    test "$(readlink "/proc/$archive_pid/exe")" = "$archive_root/arc-node"
    test "$(sha256sum "/proc/$archive_pid/cmdline" | cut -d' ' -f1)" = "$archive_argv_sha"
    expected_pids="$expected_pids $archive_pid"
  fi
  assert_unix_listener_owner "$archive_filter_socket" "$filter_pid" legacy-archive-filter "$filter_user" "$gateway_user" 770
  assert_listener_owner "$archive_filter_port" "" retired-legacy-archive-filter-tcp
  assert_unix_listener_owner "$archive_rpc_socket" "$archive_pid" legacy-archive-rpc "$archive_user" "$origin_group" 660
  assert_listener_owner "$retired_archive_rpc_port" "" retired-legacy-archive-rpc-tcp
  if [ -n "$archive_pid" ]; then
    archive_runtime=${archive_rpc_socket%/*}
    test -d "$archive_runtime" && test ! -L "$archive_runtime"
    test "$(stat -c %U:%G:%a "$archive_runtime")" = "$archive_user:$origin_group:750"
  fi
else
  test "$archive_filter_socket" = none
  test "$archive_rpc_socket" = none
  assert_listener_owner "$archive_filter_port" "" retired-legacy-archive-filter-tcp
  assert_listener_owner "$retired_archive_rpc_port" "" retired-legacy-archive-rpc-tcp
fi
for pid in $(pgrep -x arc-node || true); do
  case " $expected_pids " in *" $pid "*) ;; *) printf 'unowned arc-node PID %s\n' "$pid" >&2; exit 1 ;; esac
done
for pid in $expected_pids; do
  pgrep -x arc-node | grep -Fxq "$pid"
done
test ! -e /etc/arc-recovery/legacy-start-allowed
for retired in arc-self-heal.service arc-node.service arc-node-update.service; do
  fence="/etc/systemd/system/$retired.d/zzzz-arc-recovery-freeze.conf"
  test -f "$fence" && test ! -L "$fence"
  test "$(stat -c %U:%G:%a:%h "$fence")" = root:root:444:1
  test "$(cat "$fence")" = "[Unit]
ConditionPathExists=/etc/arc-recovery/legacy-start-allowed"
  arm="/etc/systemd/system/$retired.d/zzzx-arc-recovery-quarantine-arm.conf"
  test -f "$arm" && test ! -L "$arm"
  test "$(stat -c %U:%G:%a:%h "$arm")" = root:root:444:1
  test "$(cat "$arm")" = "[Unit]
ConditionPathExists=!/root/arc-recovery-stops/$capture_id/.$node_name.stop.partial/09-quarantine-restart-arm.json
ConditionPathExists=!/root/arc-recovery-stops/$capture_id/$node_name/09-quarantine-restart-arm.json"
  if systemctl is-active --quiet "$retired" || systemctl is-enabled --quiet "$retired"; then
    printf 'retired legacy service is not persistently fenced: %s\n' "$retired" >&2
    exit 1
  fi
done
! systemctl is-active --quiet arc-node-update.timer
! systemctl is-enabled --quiet arc-node-update.timer
"""
        receipt_rows = {
            row["node"]: row
            for row in self.manifest["provenance"]["validator_key_receipt_chain"]["validators"]
        }
        identity_cli_sha = self.manifest["artifacts"]["cli"]["sha256"]
        identity_stage = self.manifest["archive"]["prearchive_rollout_sha256"]
        for node in self.validators:
            receipt_row = receipt_rows[node["name"]]
            self.ssh(
                node,
                script,
                (
                    node["remote_root"],
                    node["data_dir"],
                    node["key_file"],
                    node["service_name"],
                    self.gateway_service_name(node),
                    self.filter_service_name(node),
                    self.validator_rpc_socket(node),
                    node["model_path"],
                    node["model_sha256"],
                    str(node["model_size_bytes"]),
                    self.digest,
                    str(self.chain["transition_height"]),
                    bare_hash(self.chain["approved_checkpoint_manifest_hash"], "checkpoint manifest hash"),
                    sha256_bytes(
                        b"\0".join(
                            item.encode("utf-8")
                            for item in self.runtime_argv(node, remote=True)
                        )
                        + b"\0"
                    ),
                    (
                        self.legacy_archive_service_name(node)
                        if self.legacy_archive_for(node) is not None
                        else "none"
                    ),
                    (
                        self.legacy_archive_root(node)
                        if self.legacy_archive_for(node) is not None
                        else "/var/lib/arc-legacy-archive/none"
                    ),
                    (
                        sha256_bytes(
                            b"\0".join(
                                item.encode("utf-8")
                                for item in self.legacy_archive_argv(node)
                            )
                            + b"\0"
                        )
                        if self.legacy_archive_for(node) is not None
                        else "none"
                    ),
                    (
                        f"/root/arc-recovery-bindings/{self.manifest['archive']['prearchive_rollout_sha256']}/{node['name']}"
                        if self.legacy_archive_for(node) is not None
                        else "/var/lib/arc-legacy-archive/none"
                    ),
                    (
                        self.legacy_archive_for(node)["binding_index_sha256"]
                        if self.legacy_archive_for(node) is not None
                        else "none"
                    ),
                    str(LEGACY_ARCHIVE_FILTER_PORT),
                    (
                        self.legacy_archive_rpc_socket(node)
                        if self.legacy_archive_for(node) is not None
                        else "none"
                    ),
                    str(node["p2p_port"]),
                    LEGACY_ARCHIVE_USER,
                    f"/root/arc-recovery-seal/{identity_stage}/{node['name']}/arc-cli",
                    identity_cli_sha,
                    receipt_row["keyfile_sha256"],
                    receipt_row["address"],
                    CADDY_USER,
                    self.late_fork_interlock_service_name(node),
                    self.late_fork_interlock_root(node),
                    LATE_FORK_INTERLOCK_USER,
                    self.late_fork_interlock_interpreter(node)["normalized_path"],
                    self.late_fork_interlock_interpreter(node)["sha256"],
                    NGINX_FILTER_USER,
                    sha256_bytes(self.nginx_filter(node).encode("utf-8")),
                    sha256_bytes(self.filter_preflight(node).encode("utf-8")),
                    NGINX_PACKAGE_VERSION,
                    NGINX_LINUX_AMD64_SHA256,
                    self.filter_public_socket(node),
                    (
                        self.filter_archive_socket(node)
                        if self.legacy_archive_for(node) is not None
                        else "none"
                    ),
                    self.late_fork_interlock_socket(node),
                    LATE_FORK_INTERLOCK_GROUP,
                    RPC_ORIGIN_GROUP,
                    node["service_user"],
                    node["rpc_listen"].rsplit(":", 1)[1],
                    str(LEGACY_ARCHIVE_RPC_PORT),
                    (
                        f"{self.late_fork_interlock_interpreter(node)['device']}:"
                        f"{self.late_fork_interlock_interpreter(node)['inode']}:"
                        f"{self.late_fork_interlock_interpreter(node)['uid']}:"
                        f"{self.late_fork_interlock_interpreter(node)['gid']}:"
                        f"{self.late_fork_interlock_interpreter(node)['mode']:o}:"
                        f"{self.late_fork_interlock_interpreter(node)['nlink']}"
                    ),
                    self.manifest["archive"]["capture_id"],
                    node["name"],
                ),
            )
            archive_service = (
                self.legacy_archive_service_name(node)
                if self.legacy_archive_for(node) is not None
                else "none"
            )
            baseline_output = self.ssh(
                node,
                r'''set -eu
archive_service=$4 interlock_service=$5
state() { if systemctl "$1" --quiet "$2"; then printf 1; else printf 0; fi; }
printf 'validator_active=%s\n' "$(state is-active "$1")"
printf 'validator_enabled=%s\n' "$(state is-enabled "$1")"
printf 'gateway_active=%s\n' "$(state is-active "$2")"
printf 'gateway_enabled=%s\n' "$(state is-enabled "$2")"
printf 'filter_active=%s\n' "$(state is-active "$3")"
printf 'filter_enabled=%s\n' "$(state is-enabled "$3")"
printf 'interlock_active=%s\n' "$(state is-active "$interlock_service")"
printf 'interlock_enabled=%s\n' "$(state is-enabled "$interlock_service")"
if [ "$archive_service" = none ]; then
  printf 'archive_active=0\narchive_enabled=0\n'
else
  printf 'archive_active=%s\n' "$(state is-active "$archive_service")"
  printf 'archive_enabled=%s\n' "$(state is-enabled "$archive_service")"
fi
printf 'nginx_active=%s\n' "$(state is-active nginx.service)"
printf 'nginx_enabled=%s\n' "$(state is-enabled nginx.service)"
public_rows=$(ss -H -ltnp | awk '$4 ~ /:(80|443)$/ { print }')
printf 'public_80_count=%s\n' "$(printf '%s\n' "$public_rows" | awk '$4 ~ /:80$/ { count += 1 } END { print count + 0 }')"
printf 'public_443_count=%s\n' "$(printf '%s\n' "$public_rows" | awk '$4 ~ /:443$/ { count += 1 } END { print count + 0 }')"
''',
                (
                    node["service_name"],
                    self.gateway_service_name(node),
                    self.filter_service_name(node),
                    archive_service,
                    self.late_fork_interlock_service_name(node),
                ),
            )
            baseline: dict[str, bool] = {}
            public_listener_counts: dict[str, int] = {}
            for line in baseline_output.splitlines():
                if line.count("=") != 1:
                    fail(f"{node['name']} emitted a malformed service baseline")
                key, value = line.split("=", 1)
                if key in {"public_80_count", "public_443_count"}:
                    if key in public_listener_counts or re.fullmatch(r"[0-9]+", value) is None:
                        fail(f"{node['name']} emitted an invalid public-listener baseline")
                    public_listener_counts[key.removeprefix("public_").removesuffix("_count")] = int(value)
                    continue
                if key in baseline or value not in {"0", "1"}:
                    fail(f"{node['name']} emitted an invalid service baseline")
                baseline[key] = value == "1"
            expected_baseline_fields = {
                f"{service}_{state}"
                for service in ("validator", "gateway", "filter", "interlock", "archive", "nginx")
                for state in ("active", "enabled")
            }
            if set(baseline) != expected_baseline_fields:
                fail(f"{node['name']} service baseline omitted a required state")
            if set(public_listener_counts) != {"80", "443"}:
                fail(f"{node['name']} public-listener baseline omitted an exact count")
            self.production_service_baseline[node["name"]] = baseline
            self.production_public_listener_baseline[node["name"]] = public_listener_counts
            self.ssh(
                node,
                r'''set -eu
expected_version=$1 expected_sha=$2
test "$(dpkg --print-architecture)" = amd64
. /etc/os-release
test "${ID:-}" = ubuntu
test "${VERSION_ID:-}" = 24.04
candidate=$(/usr/bin/apt-cache policy nginx | /usr/bin/awk '/Candidate:/{print $2; exit}')
test "$candidate" = "$expected_version"
if /usr/bin/dpkg-query -W -f='${Status}' nginx 2>/dev/null | grep -Fxq 'install ok installed'; then
  test "$(/usr/bin/dpkg-query -W -f='${Version}' nginx)" = "$expected_version"
  test -f /usr/sbin/nginx && test ! -L /usr/sbin/nginx
  test "$(stat -c %U:%G:%a:%h /usr/sbin/nginx)" = root:root:755:1
  printf '%s  /usr/sbin/nginx\n' "$expected_sha" | sha256sum --check --strict
  /usr/sbin/nginx -V 2>&1 | grep -Fq -- '--with-http_auth_request_module'
else
  test ! -e /usr/sbin/nginx
fi
''',
                (NGINX_PACKAGE_VERSION, NGINX_LINUX_AMD64_SHA256),
            )
            self.say(f"PASS {node['name']} remote fresh-dir/key/service/DNS preflight")

    def _http_json(self, node: Mapping[str, Any], path: str, *, timeout: int = 10) -> Any:
        if not path.startswith("/") or path.startswith("//"):
            fail(f"HTTP path must be source-relative: {path}")
        if self.manifest["mode"] == "production" and not self.production_public_gate_open:
            interpreter = self.late_fork_interlock_interpreter(node)
            interpreter_identity = (
                f"{interpreter['device']}:{interpreter['inode']}:"
                f"{interpreter['uid']}:{interpreter['gid']}:"
                f"{interpreter['mode']:o}:{interpreter['nlink']}"
            )
            try:
                payload = self.ssh(
                    node,
                    r'''set -eu
socket_path=$1 path=$2 maximum=$3 python=$4 python_identity=$5 python_sha=$6
case "$socket_path" in /run/*) ;; *) printf 'maintenance probe RPC socket is not protected runtime state\n' >&2; exit 1 ;; esac
test -S "$socket_path" && test ! -L "$socket_path"
test -f "$python" && test ! -L "$python"
test "$(stat -c %d:%i:%u:%g:%a:%h "$python")" = "$python_identity"
exec 8<"$python"
test "$(stat -Lc %d:%i:%u:%g:%a:%h /proc/self/fd/8)" = "$python_identity"
test "$(sha256sum /proc/self/fd/8 | cut -d' ' -f1)" = "$python_sha"
/usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C TZ=UTC PYTHONHASHSEED=0 \
  /proc/self/fd/8 -I - "$socket_path" "$path" "$maximum" <<'PY'
import http.client
import socket
import sys

socket_path, path, maximum_raw = sys.argv[1:]
maximum = int(maximum_raw)

class UnixHTTPConnection(http.client.HTTPConnection):
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(socket_path)

connection = UnixHTTPConnection("localhost", timeout=10)
try:
    connection.request(
        "GET",
        path,
        headers={
            "Accept": "application/json",
            "Host": "127.0.0.1",
            "User-Agent": "arc-recovery-maintenance-probe/2",
        },
    )
    response = connection.getresponse()
    if response.status < 200 or response.status >= 300:
        raise SystemExit(f"maintenance probe returned HTTP {response.status}")
    if response.getheader("Location") is not None:
        raise SystemExit("maintenance probe redirect is forbidden")
    payload = response.read(maximum + 1)
finally:
    connection.close()
if len(payload) > maximum:
    raise SystemExit("maintenance probe response is oversized")
sys.stdout.buffer.write(payload)
PY
test "$(sha256sum /proc/self/fd/8 | cut -d' ' -f1)" = "$python_sha"
''',
                    (
                        self.validator_rpc_socket(node),
                        path,
                        str(16 * 1024 * 1024),
                        interpreter["normalized_path"],
                        interpreter_identity,
                        interpreter["sha256"],
                    ),
                    timeout=max(timeout, 15),
                )
                return json.loads(payload)
            except (UnicodeError, json.JSONDecodeError) as error:
                raise RolloutError(
                    f"{node['name']} {path}: invalid loopback JSON during maintenance: {error}"
                ) from error
        url = node["rpc_url"] + path
        request = urllib.request.Request(url, headers={"Accept": "application/json", "User-Agent": "arc-recovery-rollout/1"})
        try:
            with urllib.request.urlopen(request, timeout=timeout, context=ssl.create_default_context()) as response:
                if response.geturl() != url:
                    fail(f"{node['name']} {path}: redirects are not permitted")
                payload = response.read(16 * 1024 * 1024 + 1)
                if len(payload) > 16 * 1024 * 1024:
                    fail(f"oversized response from {node['name']} {path}")
                return json.loads(payload)
        except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError, json.JSONDecodeError) as error:
            raise RolloutError(f"{node['name']} {path}: {error}") from error

    def _http_status_headers(
        self,
        node: Mapping[str, Any],
        path: str,
        *,
        method: str,
        origin: str | None = None,
        data: bytes | None = None,
        timeout: int = 20,
    ) -> tuple[int, Mapping[str, str]]:
        """Probe one exact public URL without following it into a trusted result."""
        if not path.startswith("/") or path.startswith("//"):
            fail(f"HTTP path must be source-relative: {path}")
        url = node["rpc_url"] + path
        headers = {"Accept": "application/json", "User-Agent": "arc-recovery-browser-gate/1"}
        if origin is not None:
            headers["Origin"] = origin
        request = urllib.request.Request(
            url,
            data=data if data is not None else b"" if method == "POST" else None,
            headers=headers,
            method=method,
        )
        try:
            with urllib.request.urlopen(
                request, timeout=timeout, context=ssl.create_default_context()
            ) as response:
                if response.geturl() != url:
                    fail(f"{node['name']} {path}: redirects are not permitted")
                if len(response.read(64 * 1024 + 1)) > 64 * 1024:
                    fail(f"oversized browser-gate response from {node['name']} {path}")
                return response.status, response.headers
        except urllib.error.HTTPError as error:
            if error.geturl() != url:
                fail(f"{node['name']} {path}: redirects are not permitted")
            error.read(64 * 1024 + 1)
            return error.code, error.headers
        except (urllib.error.URLError, TimeoutError) as error:
            raise RolloutError(
                f"{node['name']} browser gate {method} {path}: {error}"
            ) from error

    def _prove_legacy_archive_browser_contract(self, node: Mapping[str, Any]) -> None:
        """Require GET-only routing and exact Pages-only browser readability."""
        for method in ("HEAD", "POST", "OPTIONS"):
            status, _ = self._http_status_headers(
                node, "/provenance", method=method, origin=PUBLIC_BROWSER_ORIGIN
            )
            if status != 405:
                fail(f"{node['name']} archive {method} must return HTTP 405, got {status}")

        status, _ = self._http_status_headers(
            node,
            "/provenance",
            method="GET",
            origin=PUBLIC_BROWSER_ORIGIN,
            data=b"x" * 1025,
        )
        if status != 413:
            fail(f"{node['name']} archive oversized GET body must return HTTP 413, got {status}")

        status, headers = self._http_status_headers(
            node, "/provenance", method="GET", origin=PUBLIC_BROWSER_ORIGIN
        )
        if status != 200 or headers.get("Access-Control-Allow-Origin") != PUBLIC_BROWSER_ORIGIN:
            fail(f"{node['name']} archive is not readable by the exact Pages origin")
        vary = {part.strip().lower() for part in headers.get("Vary", "").split(",")}
        if "origin" not in vary:
            fail(f"{node['name']} archive omitted Vary: Origin")

        status, headers = self._http_status_headers(
            node, "/provenance", method="GET", origin="https://attacker.invalid"
        )
        leaked_origin = headers.get("Access-Control-Allow-Origin")
        # A public GET may still return 200 to curl. Browsers must be unable to
        # expose it to an unsealed origin: no wildcard and no attacker echo.
        if status not in {200, 403, 404} or leaked_origin is not None:
            fail(f"{node['name']} archive allowed an unsealed browser origin")

    def _assert_network_info(self, node: Mapping[str, Any], info: Mapping[str, Any]) -> None:
        expected_hash = bare_hash(self.chain["approved_checkpoint_manifest_hash"], "checkpoint hash")
        checks = {
            "chain_id": self.chain["chain_id"],
            "protocol_version": self.chain["protocol_version"],
            "recovery_active": True,
            "recovery_epoch": self.chain["recovery_epoch"],
            "validator_set_id": self.chain["validator_set_id"],
            "validators_active": REQUIRED_VALIDATORS,
        }
        for key, expected in checks.items():
            if info.get(key) != expected:
                fail(f"{node['name']} /network/info {key}: expected {expected!r}, got {info.get(key)!r}")
        if bare_hash(info.get("checkpoint_manifest_hash"), f"{node['name']} checkpoint_manifest_hash") != expected_hash:
            fail(f"{node['name']} reports an unapproved checkpoint manifest hash")
        if bare_hash(info.get("recovery_domain"), f"{node['name']} recovery_domain") != bare_hash(
            self.chain["recovery_domain"], "manifest recovery_domain"
        ):
            fail(f"{node['name']} reports a recovery domain outside the sealed manifest")

    def wait_nodes_ready(self, timeout: int | None = None) -> None:
        deadline = time.monotonic() + (timeout or self.checks["startup_timeout_seconds"])
        pending = {node["name"]: node for node in self.validators}
        last: dict[str, str] = {}
        while pending and time.monotonic() < deadline:
            for name, node in list(pending.items()):
                try:
                    info = self._http_json(node, "/network/info")
                    self._assert_network_info(node, info)
                    pending.pop(name)
                except RolloutError as error:
                    last[name] = str(error)
            if pending:
                time.sleep(self.checks["poll_interval_seconds"])
        if pending:
            fail("nodes did not become recovery-ready: " + "; ".join(f"{name}: {last.get(name, 'unreachable')}" for name in pending))
        self.say("PASS all six nodes report the exact chain, protocol v3, checkpoint, epoch, set, and active-validator count")

    @staticmethod
    def _unwrap_block(value: Any) -> Mapping[str, Any]:
        if not isinstance(value, dict):
            fail("block response is not an object")
        block = value.get("block", value)
        if not isinstance(block, dict):
            fail("block payload is not an object")
        return block

    @classmethod
    def _block_commitment(cls, value: Any) -> tuple[int, str, str, str | None]:
        block = cls._unwrap_block(value)
        header = block.get("header")
        if not isinstance(header, dict):
            fail("block response has no header")
        height = required_int(header.get("height", block.get("height")), "block.height")
        hash_value = header.get("hash", block.get("hash"))
        state_root = header.get("state_root", header.get("stateRoot", block.get("state_root")))
        parent = header.get("parent_hash", header.get("parentHash"))
        return (
            height,
            bare_hash(hash_value, "block.hash"),
            bare_hash(state_root, "block.state_root"),
            bare_hash(parent, "block.parent_hash") if parent is not None else None,
        )

    def prove_boundary(self) -> None:
        expected_source = bare_hash(self.chain["source_block_hash"], "source hash")
        expected_source_root = bare_hash(self.chain["source_state_root"], "source state root")
        expected_transition = bare_hash(self.chain["transition_block_hash"], "transition hash")
        expected_root = bare_hash(self.chain["full_state_root"], "state root")
        for node in self.validators:
            source = self._block_commitment(self._http_json(node, f"/block/{self.chain['source_height']}"))
            transition = self._block_commitment(self._http_json(node, f"/block/{self.chain['transition_height']}"))
            if source[:3] != (
                self.chain["source_height"], expected_source, expected_source_root
            ):
                fail(f"{node['name']} does not preserve the approved legacy block hash/root at H")
            if transition[:3] != (self.chain["transition_height"], expected_transition, expected_root):
                fail(f"{node['name']} does not expose the approved v3 transition block at H+1")
            if transition[3] != expected_source:
                fail(f"{node['name']} H+1 transition parent does not bind the preserved H hash")
        self.say("PASS every node preserves selected H and exposes the exact H+1 v3 continuation")

    def common_commitment(self) -> tuple[int, str, str]:
        heights: list[int] = []
        for node in self.validators:
            info = self._http_json(node, "/network/info")
            self._assert_network_info(node, info)
            height = info.get("last_block_height")
            if isinstance(height, bool) or not isinstance(height, int):
                fail(f"{node['name']} has no retained last_block_height")
            heights.append(height)
        common_height = min(heights)
        if common_height < self.chain["transition_height"]:
            fail("fleet common height is below the recovery transition")
        commitments: dict[str, tuple[int, str, str, str | None]] = {}
        for node in self.validators:
            commitments[node["name"]] = self._block_commitment(self._http_json(node, f"/block/{common_height}"))
        distinct = {(value[1], value[2]) for value in commitments.values()}
        if len(distinct) != 1:
            detail = ", ".join(f"{name}={value[1]}/{value[2]}" for name, value in commitments.items())
            fail(f"same-height fork at #{common_height}: {detail}")
        block_hash, state_root = next(iter(distinct))
        return common_height, block_hash, state_root

    def wait_convergence(self, *, minimum_height: int | None = None, timeout: int | None = None) -> tuple[int, str, str]:
        deadline = time.monotonic() + (timeout or self.checks["convergence_timeout_seconds"])
        last_error = "not sampled"
        while time.monotonic() < deadline:
            try:
                result = self.common_commitment()
                if minimum_height is None or result[0] >= minimum_height:
                    return result
                last_error = f"common height {result[0]} is below required {minimum_height}"
            except RolloutError as error:
                last_error = str(error)
            time.sleep(self.checks["poll_interval_seconds"])
        fail(f"fleet did not converge before timeout: {last_error}")

    def prove_visible_height_continuity(self) -> tuple[int, str, str]:
        minimum_height = self.chain["legacy_public_max_height"] + 1
        result = self.wait_convergence(minimum_height=minimum_height)
        self.say(
            "PASS all six v3 validators agree strictly above the sealed legacy "
            f"public maximum: #{result[0]} > #{self.chain['legacy_public_max_height']}"
        )
        return result

    def prove_advancing_convergence(
        self,
    ) -> tuple[tuple[int, str, str], tuple[int, str, str]]:
        initial = self.prove_visible_height_continuity()
        target = initial[0] + self.checks["min_height_advance"]
        deadline = time.monotonic() + self.checks["convergence_timeout_seconds"]
        not_before = time.monotonic() + self.checks["observation_seconds"]
        final = initial
        while time.monotonic() < deadline:
            final = self.wait_convergence(timeout=max(1, int(deadline - time.monotonic())))
            if final[0] >= target and time.monotonic() >= not_before:
                self.say(f"PASS advancing same-height convergence #{initial[0]} -> #{final[0]} hash={final[1]} root={final[2]}")
                return initial, final
            time.sleep(self.checks["poll_interval_seconds"])
        fail(f"fleet agreed but did not advance by {self.checks['min_height_advance']} blocks during the observation gate")

    def _policy(self, node: Mapping[str, Any]) -> Mapping[str, Any]:
        value = self._http_json(node, "/community/reward_policy")
        if not isinstance(value, dict):
            fail(f"{node['name']} reward policy is not an object")
        expected_ready = self.checks["reward"]["expect_issuance_ready"]
        exact = {
            "schema": "arc.community.reward-policy.v1",
            "tx_type": "0x25",
            "protocol_active": self.checks["reward"]["expect_protocol_active"],
            "issuance_ready": expected_ready,
            "active_validator_count": REQUIRED_VALIDATORS,
            "validator_set_size_required": REQUIRED_VALIDATORS,
            "validator_approvals_required": REQUIRED_APPROVALS,
            "configured_community_rpc_origins": REQUIRED_VALIDATORS,
            "recovery_epoch": self.chain["recovery_epoch"],
            "validator_set_id": self.chain["validator_set_id"],
            "worker_min_stake_base": 0,
            "stake_zero_eligible": True,
        }
        for key, expected in exact.items():
            if value.get(key) != expected:
                fail(f"{node['name']} reward policy {key}: expected {expected!r}, got {value.get(key)!r}")
        issuance_policy = value.get("issuance_policy")
        expected_issuance_policy = {
            "reward_amount": value.get("reward_base"),
            "epoch_blocks": 216_000,
            "max_per_block": 1,
            "max_per_epoch": 40,
            "max_per_worker_epoch": 8,
            "max_per_coordinator_epoch": 16,
        }
        if issuance_policy != expected_issuance_policy:
            fail(
                f"{node['name']} reward issuance policy differs from the sealed v3 promotional caps"
            )
        bare_hash(value.get("issuance_policy_hash"), f"{node['name']} reward issuance policy hash")
        if value.get("reward_program") != "protocol-capped testnet promotional compute subsidy":
            fail(f"{node['name']} does not label the reward as the promotional testnet subsidy")
        if value.get("reward_is_customer_demand") is not False:
            fail(f"{node['name']} incorrectly represents validator recomputation as customer demand")
        budget = value.get("prospective_budget")
        if not isinstance(budget, dict):
            fail(f"{node['name']} reward policy omits prospective consensus budget counters")
        for key in (
            "block_height",
            "epoch",
            "issued_this_block",
            "remaining_this_block",
            "issued_this_epoch",
            "remaining_this_epoch",
            "coordinator_issued_this_epoch",
            "coordinator_remaining_this_epoch",
        ):
            if not isinstance(budget.get(key), int) or isinstance(budget.get(key), bool) or budget[key] < 0:
                fail(f"{node['name']} reward prospective budget {key} is not a nonnegative integer")
        if budget.get("worker_issued_this_epoch") is not None or budget.get("worker_remaining_this_epoch") is not None:
            fail(f"{node['name']} policy invents a worker-specific counter without a worker identity")
        if expected_ready:
            for key in ("transaction_domain", "validator_set_commitment"):
                bare_hash(value.get(key), f"{node['name']} reward policy {key}")
            if value.get("readiness_unavailable_reason") is not None:
                fail(f"{node['name']} claims ready but reports a readiness failure")
            if value.get("treasury_rewards_remaining", 0) <= 0 or any(
                budget[key] <= 0
                for key in (
                    "remaining_this_block",
                    "remaining_this_epoch",
                    "coordinator_remaining_this_epoch",
                )
            ):
                fail(f"{node['name']} claims issuance-ready with an exhausted treasury or budget")
        elif value.get("readiness_unavailable_reason") in {None, ""}:
            fail(f"{node['name']} must explain why issuance is unavailable")
        return value

    def prove_reward_policy(self) -> None:
        policies = [self._policy(node) for node in self.validators]
        fields = (
            "transaction_domain",
            "validator_set_commitment",
            "reward_base",
            "reward_arc",
            "issuance_policy_hash",
            "issuance_policy",
        )
        if any(len({json.dumps(policy.get(key), sort_keys=True) for policy in policies}) != 1 for key in fields):
            fail("validators disagree on reward domain, set commitment, or amount")
        state = "issuance-ready" if self.checks["reward"]["expect_issuance_ready"] else "fail-closed disabled"
        self.say(f"PASS all six validators report one {state} 6-validator/5-approval reward policy")

    def obtain_receipt_evidence(self, ordinal: int = 1) -> ReceiptEvidence | None:
        reward = self.checks["reward"]
        if reward["mode"] != "receipt":
            return None
        if ordinal not in {1, 2}:
            fail("reward receipt probe ordinal must be 1 or 2")
        if "probe_argv" not in reward:
            return ReceiptEvidence.from_value(reward["receipts"][ordinal - 1])
        environment = dict(os.environ)
        environment.update(
            {
                "ARC_RECOVERY_RPC_URLS": json.dumps([node["rpc_url"] for node in self.validators], separators=(",", ":")),
                "ARC_RECOVERY_ROLLOUT_MANIFEST_SHA256": self.digest,
                "ARC_RECOVERY_CHECKPOINT_MANIFEST_HASH": bare_hash(self.chain["approved_checkpoint_manifest_hash"], "hash"),
            }
        )
        result = run_checked(
            [
                *reward["probe_argv"],
                "--probe-ordinal",
                str(ordinal),
                "--recovery-probe-id",
                recovery_probe_id_for_rollout(self.digest, ordinal),
            ],
            timeout=self.checks["convergence_timeout_seconds"],
            env=environment,
        )
        try:
            body = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            fail(f"reward probe did not emit one JSON evidence object: {error}")
        return ReceiptEvidence.from_value(body)

    def prove_reward_receipt(
        self, evidence: ReceiptEvidence, ordinal: int | None = None
    ) -> tuple[int, str]:
        reward = self.checks["reward"]
        expected_base = reward["expected_reward_base"]
        policies = [self._policy(node) for node in self.validators]
        expected_domain = bare_hash(policies[0]["transaction_domain"], "reward domain")
        expected_set = bare_hash(policies[0]["validator_set_commitment"], "validator set commitment")
        deadline = time.monotonic() + self.checks["convergence_timeout_seconds"]
        last_error = "not sampled"
        while time.monotonic() < deadline:
            try:
                receipts = [self._http_json(node, f"/community/reward_receipt/{evidence.tx_hash}") for node in self.validators]
                for node, receipt in zip(self.validators, receipts):
                    if not isinstance(receipt, dict):
                        fail(f"{node['name']} reward receipt is not an object")
                    exact = {
                        "status": "mined_success",
                        "tx_type": "0x25",
                        "included": True,
                        "confirmed": True,
                        "success": True,
                        "recovery_epoch": self.chain["recovery_epoch"],
                        "validator_set_id": self.chain["validator_set_id"],
                        "reward_base": expected_base,
                    }
                    for key, wanted in exact.items():
                        if receipt.get(key) != wanted:
                            fail(f"{node['name']} reward receipt {key}: expected {wanted!r}, got {receipt.get(key)!r}")
                    hashes = {
                        "tx_hash": evidence.tx_hash,
                        "job_id": evidence.job_id,
                        "worker": evidence.worker,
                        "transaction_domain": expected_domain,
                        "validator_set_commitment": expected_set,
                    }
                    if ordinal is not None:
                        hashes["assignment_epoch"] = recovery_probe_id_for_rollout(
                            self.digest, ordinal
                        )
                    for key, wanted in hashes.items():
                        if bare_hash(receipt.get(key), f"{node['name']} receipt {key}") != bare_hash(wanted, key):
                            fail(f"{node['name']} reward receipt {key} differs")
                    if required_int(receipt.get("validator_approvals"), "receipt.validator_approvals") < REQUIRED_APPROVALS:
                        fail(f"{node['name']} reward receipt has fewer than five validator approvals")
                    required_int(receipt.get("block_height"), "receipt.block_height", minimum=self.chain["transition_height"])
                    bare_hash(receipt.get("block_hash"), "receipt.block_hash")
                receipt_blocks = {(receipt["block_height"], bare_hash(receipt["block_hash"], "block hash")) for receipt in receipts}
                if len(receipt_blocks) != 1:
                    fail("validators disagree on the reward receipt block")
                receipt_block = next(iter(receipt_blocks))
                for node in self.validators:
                    earnings = self._http_json(node, f"/worker/earnings/{evidence.worker}")
                    if not isinstance(earnings, dict):
                        fail(f"{node['name']} worker earnings is not an object")
                    if required_int(earnings.get("confirmed_receipt_count"), "earnings.confirmed_receipt_count") < 1:
                        fail(f"{node['name']} worker earnings omitted the mined receipt")
                    if required_int(earnings.get("confirmed_gross_earnings_base"), "earnings.confirmed_gross_earnings_base") < expected_base:
                        fail(f"{node['name']} worker earnings undercount the confirmed reward")
                    rows = earnings.get("confirmed_receipts")
                    if not isinstance(rows, list) or not any(
                        isinstance(row, dict)
                        and row.get("success") is True
                        and bare_hash(row.get("tx_hash"), "earnings receipt tx_hash") == bare_hash(evidence.tx_hash, "tx_hash")
                        and bare_hash(row.get("job_id"), "earnings receipt job_id") == bare_hash(evidence.job_id, "job_id")
                        and required_int(row.get("block_height"), "earnings receipt block_height") == receipt_block[0]
                        and bare_hash(row.get("block_hash"), "earnings receipt block_hash") == receipt_block[1]
                        for row in rows
                    ):
                        fail(f"{node['name']} earnings lacks the exact successful 0x25 receipt")
                self.say(f"PASS mined 0x25 reward receipt {evidence.tx_hash} and worker earnings agree on all six validators")
                return receipt_block
            except RolloutError as error:
                last_error = str(error)
                time.sleep(self.checks["poll_interval_seconds"])
        fail(f"reward receipt did not converge before timeout: {last_error}")

    def prove_reward_projection(
        self,
        evidence: Sequence[ReceiptEvidence],
        receipt_blocks: Sequence[tuple[int, str]],
    ) -> None:
        validate_distinct_receipt_evidence(evidence)
        if (
            len(receipt_blocks) != 2
            or len(set(receipt_blocks)) != 2
            or len({height for height, _ in receipt_blocks}) != 2
        ):
            fail("the two reward receipts must be mined in two distinct blocks")
        expected_base = self.checks["reward"]["expected_reward_base"]
        expected_gross_base = 2 * expected_base
        expected_reward_arc = expected_base / 1_000_000_000
        expected_gross_arc = expected_gross_base / 1_000_000_000
        for node in self.validators:
            earnings = self._http_json(node, f"/worker/earnings/{evidence[0].worker}")
            if not isinstance(earnings, dict):
                fail(f"{node['name']} worker earnings is not an object")
            if required_int(
                earnings.get("confirmed_receipt_count"),
                "earnings.confirmed_receipt_count",
            ) != 2:
                fail(f"{node['name']} earnings must contain exactly the two rollout canary receipts")
            gross_base = required_int(
                earnings.get("confirmed_gross_earnings_base"),
                "earnings.confirmed_gross_earnings_base",
            )
            if gross_base != expected_gross_base:
                fail(f"{node['name']} earnings does not prove exactly 5 ARC gross from two canaries")
            gross_arc = earnings.get("confirmed_gross_earnings_arc")
            if (
                isinstance(gross_arc, bool)
                or not isinstance(gross_arc, (int, float))
                or float(gross_arc) != expected_gross_arc
            ):
                fail(f"{node['name']} earnings ARC gross does not equal the exact base-unit total")
            rows = earnings.get("confirmed_receipts")
            if not isinstance(rows, list) or len(rows) != 2:
                fail(f"{node['name']} earnings must expose exactly two confirmed receipt rows")
            normalized_rows: list[tuple[str, str, int, str, Mapping[str, Any]]] = []
            for row in rows:
                if not isinstance(row, dict):
                    fail(f"{node['name']} earnings contains a non-object receipt")
                normalized_rows.append(
                    (
                        bare_hash(row.get("tx_hash"), "earnings receipt tx_hash"),
                        bare_hash(row.get("job_id"), "earnings receipt job_id"),
                        required_int(row.get("block_height"), "earnings receipt block_height"),
                        bare_hash(row.get("block_hash"), "earnings receipt block_hash"),
                        row,
                    )
                )
            if len({row[0] for row in normalized_rows}) != len(normalized_rows):
                fail(f"{node['name']} earnings repeats a receipt transaction hash")
            if len({row[1] for row in normalized_rows}) != len(normalized_rows):
                fail(f"{node['name']} earnings repeats a receipt job id")
            for item, expected_block in zip(evidence, receipt_blocks):
                matches = [
                    row
                    for row in normalized_rows
                    if row[0] == bare_hash(item.tx_hash, "evidence tx_hash")
                    and row[1] == bare_hash(item.job_id, "evidence job_id")
                ]
                if len(matches) != 1:
                    fail(f"{node['name']} earnings lacks one unique exact demo receipt")
                row = matches[0]
                if (
                    (row[2], row[3])
                    != (expected_block[0], bare_hash(expected_block[1], "receipt block hash"))
                    or row[4].get("success") is not True
                    or required_int(row[4].get("reward_base"), "earnings receipt reward_base")
                    != expected_base
                    or isinstance(row[4].get("reward_arc"), bool)
                    or not isinstance(row[4].get("reward_arc"), (int, float))
                    or float(row[4]["reward_arc"]) != expected_reward_arc
                ):
                    fail(f"{node['name']} earnings demo receipt fields differ from mined evidence")
            rate = earnings.get("attestations_per_day_observed")
            projection = earnings.get("projected_daily_arc")
            if rate is not None:
                fail(f"{node['name']} invents an observed daily rate from two rollout canaries")
            if projection is not None:
                fail(f"{node['name']} invents projected_daily_arc from two rollout canaries")
            if earnings.get("attestations_per_day_unavailable_reason") != PROJECTION_COLLECTING_REASON:
                fail(f"{node['name']} observed rate lacks the canonical collecting-data reason")
            if earnings.get("projected_daily_unavailable_reason") != PROJECTION_COLLECTING_REASON:
                fail(f"{node['name']} projection lacks the canonical collecting-data reason")
        self.say(
            "PASS two distinct mined receipts prove exactly 5 ARC gross while the daily "
            "rate and projection remain canonical collecting-data nulls on all six validators"
        )

    def prove_two_reward_receipts(self) -> list[ReceiptEvidence]:
        if self.checks["reward"]["mode"] != "receipt":
            return []
        evidence = list(self.reward_evidence_progress)
        if len(evidence) == 2:
            validate_distinct_receipt_evidence(evidence)
        receipt_blocks = [
            self.prove_reward_receipt(item, ordinal)
            for ordinal, item in enumerate(evidence, 1)
        ]
        # Prove the first transaction mined before submitting the second. The
        # consensus policy permits one reward per block. Each discovered
        # transaction is fsynced into the 0/1/2 journal before its mined proof,
        # so every later crash window resumes without issuing a new identity.
        for ordinal in range(len(evidence) + 1, 3):
            item = self.obtain_receipt_evidence(ordinal)
            if item is None:
                fail("receipt mode did not produce receipt evidence")
            evidence.append(item)
            if len(evidence) == 2:
                validate_distinct_receipt_evidence(evidence)
            self.persist_reward_evidence_progress(evidence)
            receipt_blocks.append(self.prove_reward_receipt(item, ordinal))
        self.prove_reward_projection(evidence, receipt_blocks)
        return evidence

    def prove_or_resume_two_reward_receipts(self) -> list[ReceiptEvidence]:
        """Issue fresh probes only when no finalized evidence can be resumed."""
        if self.checks["reward"]["mode"] != "receipt":
            return []
        if self.existing_reward_evidence is None:
            return self.prove_two_reward_receipts()
        evidence = list(self.existing_reward_evidence)
        validate_distinct_receipt_evidence(evidence)
        receipt_blocks = [
            self.prove_reward_receipt(item, ordinal)
            for ordinal, item in enumerate(evidence, 1)
        ]
        self.prove_reward_projection(evidence, receipt_blocks)
        self.say(
            "PASS resumed the exact rollout-bound reward evidence; "
            "no duplicate reward probes were issued"
        )
        return evidence

    def _reward_evidence_payload(
        self, evidence: Sequence[ReceiptEvidence]
    ) -> tuple[bytes, str, bytes]:
        validate_distinct_receipt_evidence(evidence)
        if self.reward_evidence_output is None:
            fail("receipt-mode execution requires --reward-evidence-output")
        payload = canonical_bytes(
            {
                "schema": "arc.recovery.reward-evidence.v1",
                "rollout_sha256": self.digest,
                "receipts": [
                    {
                        "tx_hash": item.tx_hash,
                        "job_id": item.job_id,
                        "worker": item.worker,
                    }
                    for item in evidence
                ],
            }
        )
        digest = sha256_bytes(payload)
        sidecar_payload = (
            f"{digest}  {self.reward_evidence_output.name}\n".encode()
        )
        return payload, digest, sidecar_payload

    @staticmethod
    def _fsync_directory(path: Path) -> None:
        directory_fd = os.open(path, os.O_RDONLY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)

    @staticmethod
    def _read_open_regular(fd: int, path: Path, maximum: int) -> tuple[bytes, int]:
        details = os.fstat(fd)
        if not stat.S_ISREG(details.st_mode):
            fail(f"reward evidence target is not a regular file: {path}")
        if details.st_size > maximum:
            fail(f"reward evidence target is unexpectedly large: {path}")
        os.lseek(fd, 0, os.SEEK_SET)
        chunks: list[bytes] = []
        remaining = details.st_size
        while remaining:
            chunk = os.read(fd, min(remaining, 64 * 1024))
            if not chunk:
                fail(f"reward evidence target changed while reading: {path}")
            chunks.append(chunk)
            remaining -= len(chunk)
        return b"".join(chunks), stat.S_IMODE(details.st_mode)

    def _atomic_replace_reward_reservation(
        self,
        path: Path,
        reservation_fd: int,
        expected: bytes,
        payload: bytes,
        *,
        payload_mode: int = 0o444,
    ) -> None:
        """Atomically replace only the exact reservation inode owned by this run."""
        current, mode = self._read_open_regular(
            reservation_fd, path, max(len(expected), len(payload))
        )
        opened = os.fstat(reservation_fd)
        visible = path.lstat()
        if (
            stat.S_ISLNK(visible.st_mode)
            or not stat.S_ISREG(visible.st_mode)
            or (visible.st_dev, visible.st_ino) != (opened.st_dev, opened.st_ino)
            or mode != 0o600
            or current != expected
        ):
            fail("reward evidence reservation changed before finalization")
        temporary_fd, temporary_name = tempfile.mkstemp(
            prefix=f".{path.name}.", suffix=".pending", dir=path.parent
        )
        temporary = Path(temporary_name)
        try:
            view = memoryview(payload)
            while view:
                written = os.write(temporary_fd, view)
                if written <= 0:
                    fail("reward evidence final write made no progress")
                view = view[written:]
            os.fchmod(temporary_fd, payload_mode)
            os.fsync(temporary_fd)
            os.close(temporary_fd)
            temporary_fd = -1

            # Re-prove the visible path still names the exact reservation
            # immediately before the atomic rename. The output directory is
            # operator-controlled; no pre-existing non-reservation is replaced.
            visible = path.lstat()
            if (
                stat.S_ISLNK(visible.st_mode)
                or not stat.S_ISREG(visible.st_mode)
                or (visible.st_dev, visible.st_ino) != (opened.st_dev, opened.st_ino)
            ):
                fail("reward evidence reservation path changed before finalization")
            os.replace(temporary, path)
            self._fsync_directory(path.parent)
        finally:
            if temporary_fd >= 0:
                os.close(temporary_fd)
            try:
                temporary.unlink()
            except FileNotFoundError:
                pass

    def persist_reward_evidence(self, evidence: Sequence[ReceiptEvidence]) -> str:
        validate_distinct_receipt_evidence(evidence)
        if self.reward_evidence_output is None:
            fail("receipt-mode execution requires --reward-evidence-output")
        output = self.reward_evidence_output
        if output.suffix != ".json":
            fail("reward evidence output must end in .json")
        sidecar = output.with_name(output.name + ".sha256")
        if self.reward_evidence_reservation is None:
            self.reserve_reward_evidence_output()
        payload, digest, sidecar_payload = self._reward_evidence_payload(evidence)
        if self.existing_reward_evidence is not None:
            if list(evidence) != self.existing_reward_evidence:
                fail("finalized reward evidence differs from the supplied receipts")
            self.say(
                f"PASS existing rollout-bound reward evidence {output} sha256={digest}"
            )
            return digest
        if self.reward_evidence_reservation is None:
            fail("reward evidence reservation was not established")
        if list(evidence) != self.reward_evidence_progress:
            fail("final reward evidence differs from the durable progress journal")
        output_fd, sidecar_fd = self.reward_evidence_reservation
        markers = self._reward_evidence_reservation_markers(output, sidecar)
        self._atomic_replace_reward_reservation(
            output, output_fd, markers[0], payload
        )
        os.close(output_fd)
        self._atomic_replace_reward_reservation(
            sidecar,
            sidecar_fd,
            reward_progress_payload(self.digest, evidence),
            sidecar_payload,
        )
        os.close(sidecar_fd)
        self.reward_evidence_reservation = None
        self.existing_reward_evidence = list(evidence)
        self.say(f"PASS create-only rollout-bound reward evidence {output} sha256={digest}")
        return digest

    def persist_reward_evidence_progress(
        self, evidence: Sequence[ReceiptEvidence]
    ) -> None:
        if self.reward_evidence_output is None:
            fail("receipt-mode execution requires --reward-evidence-output")
        if self.reward_evidence_reservation is None:
            self.reserve_reward_evidence_output()
        if self.existing_reward_evidence is not None:
            if list(evidence) != self.existing_reward_evidence:
                fail("reward progress differs from finalized evidence")
            return
        if self.reward_evidence_reservation is None:
            fail("reward evidence reservation was not established")
        previous = list(self.reward_evidence_progress)
        next_evidence = list(evidence)
        if len(next_evidence) != len(previous) + 1 or next_evidence[:-1] != previous:
            fail("reward evidence progress must advance by exactly one receipt")
        if len(next_evidence) == 2:
            validate_distinct_receipt_evidence(next_evidence)
        output_fd, progress_fd = self.reward_evidence_reservation
        sidecar = self.reward_evidence_output.with_name(
            self.reward_evidence_output.name + ".sha256"
        )
        previous_payload = reward_progress_payload(self.digest, previous)
        next_payload = reward_progress_payload(self.digest, next_evidence)
        self._atomic_replace_reward_reservation(
            sidecar,
            progress_fd,
            previous_payload,
            next_payload,
            payload_mode=0o600,
        )
        os.close(progress_fd)
        nofollow = getattr(os, "O_NOFOLLOW", 0)
        try:
            replacement = os.open(sidecar, os.O_RDWR | nofollow)
        except OSError as error:
            fail(f"cannot reopen reward evidence progress {sidecar}: {error}")
        try:
            opened = os.fstat(replacement)
            visible = sidecar.lstat()
            contents, mode = self._read_open_regular(
                replacement, sidecar, 64 * 1024
            )
            if (
                stat.S_ISLNK(visible.st_mode)
                or not stat.S_ISREG(visible.st_mode)
                or (visible.st_dev, visible.st_ino)
                != (opened.st_dev, opened.st_ino)
                or mode != 0o600
                or contents != next_payload
            ):
                fail("reward evidence progress changed while reopening")
        except Exception:
            os.close(replacement)
            raise
        self.reward_evidence_reservation = (output_fd, replacement)
        self.reward_evidence_progress = next_evidence
        self.say(
            f"PASS fsynced reward evidence progress {len(next_evidence)}/2"
        )

    def _reward_evidence_reservation_markers(
        self, output: Path, sidecar: Path
    ) -> tuple[bytes, bytes]:
        return tuple(
            canonical_bytes(
                {
                    "schema": "arc.recovery.reward-evidence-reservation.v1",
                    "rollout_sha256": self.digest,
                    "target": path.name,
                }
            ).ljust(4096, b" ")
            for path in (output, sidecar)
        )  # type: ignore[return-value]

    def reserve_reward_evidence_output(self) -> None:
        """Reserve and preallocate both create-only outputs before mutation.

        Exact untouched reservations and exact finalized bytes are safely
        resumable. A crash after publishing the JSON but before publishing its
        sidecar is completed from that canonical JSON without issuing another
        reward. Anything else is refused rather than overwritten. Plan-only
        never calls this method.
        """
        if self.checks["reward"]["mode"] != "receipt":
            return
        if (
            self.reward_evidence_reservation is not None
            or self.existing_reward_evidence is not None
        ):
            return
        if self.reward_evidence_output is None:
            fail("receipt-mode execution requires --reward-evidence-output")
        output = self.reward_evidence_output
        if output.suffix != ".json":
            fail("reward evidence output must end in .json")
        output.parent.mkdir(parents=True, exist_ok=True)
        sidecar = output.with_name(output.name + ".sha256")
        paths = (output, sidecar)
        markers = self._reward_evidence_reservation_markers(output, sidecar)
        nofollow = getattr(os, "O_NOFOLLOW", 0)
        descriptors: list[int] = []
        states: list[tuple[bytes, int]] = []
        created: list[tuple[Path, bytes, int, int]] = []
        try:
            for path, marker in zip(paths, markers):
                try:
                    fd = os.open(
                        path,
                        os.O_RDWR | nofollow | os.O_CREAT | os.O_EXCL,
                        0o600,
                    )
                    view = memoryview(marker)
                    while view:
                        written = os.write(fd, view)
                        if written <= 0:
                            fail("reward evidence reservation write made no progress")
                        view = view[written:]
                    os.fsync(fd)
                    details = os.fstat(fd)
                    created.append(
                        (path, marker, details.st_dev, details.st_ino)
                    )
                    contents, mode = self._read_open_regular(
                        fd, path, 64 * 1024
                    )
                except FileExistsError:
                    try:
                        fd = os.open(path, os.O_RDONLY | nofollow)
                    except OSError as error:
                        fail(f"cannot safely open reward evidence target {path}: {error}")
                    contents, mode = self._read_open_regular(
                        fd, path, 64 * 1024
                    )
                    opened = os.fstat(fd)
                    visible = path.lstat()
                    if (
                        stat.S_ISLNK(visible.st_mode)
                        or not stat.S_ISREG(visible.st_mode)
                        or (visible.st_dev, visible.st_ino)
                        != (opened.st_dev, opened.st_ino)
                    ):
                        fail("reward evidence target changed while opening")
                descriptors.append(fd)
                states.append((contents, mode))
            self._fsync_directory(output.parent)

            output_bytes, output_mode = states[0]
            sidecar_bytes, sidecar_mode = states[1]
            output_is_marker = (
                output_mode == 0o600 and output_bytes == markers[0]
            )
            sidecar_is_marker = (
                sidecar_mode == 0o600 and sidecar_bytes == markers[1]
            )
            sidecar_progress: list[ReceiptEvidence] | None = None
            if (
                sidecar_mode == 0o600
                and not sidecar_is_marker
                and sidecar_bytes.startswith(b"{")
            ):
                sidecar_progress = parse_reward_progress_payload(
                    sidecar_bytes, self.digest
                )

            if output_is_marker and (sidecar_is_marker or sidecar_progress is not None):
                writable: list[int] = []
                expected_open = (
                    markers[0],
                    markers[1]
                    if sidecar_is_marker
                    else reward_progress_payload(self.digest, sidecar_progress or []),
                )
                for index, path in enumerate(paths):
                    fd = descriptors[index]
                    opened = os.fstat(fd)
                    try:
                        replacement = os.open(path, os.O_RDWR | nofollow)
                    except OSError as error:
                        fail(f"cannot reopen reward evidence reservation {path}: {error}")
                    replacement_details = os.fstat(replacement)
                    contents, mode = self._read_open_regular(
                        replacement, path, 64 * 1024
                    )
                    if (
                        (replacement_details.st_dev, replacement_details.st_ino)
                        != (opened.st_dev, opened.st_ino)
                        or mode != 0o600
                        or contents != expected_open[index]
                    ):
                        os.close(replacement)
                        fail("reward evidence reservation changed while reopening")
                    os.close(fd)
                    writable.append(replacement)
                descriptors = writable
                if sidecar_is_marker:
                    progress_zero = reward_progress_payload(self.digest, [])
                    self._atomic_replace_reward_reservation(
                        sidecar,
                        descriptors[1],
                        markers[1],
                        progress_zero,
                        payload_mode=0o600,
                    )
                    os.close(descriptors[1])
                    replacement = os.open(sidecar, os.O_RDWR | nofollow)
                    replacement_details = os.fstat(replacement)
                    visible = sidecar.lstat()
                    contents, mode = self._read_open_regular(
                        replacement, sidecar, 64 * 1024
                    )
                    if (
                        stat.S_ISLNK(visible.st_mode)
                        or not stat.S_ISREG(visible.st_mode)
                        or (visible.st_dev, visible.st_ino)
                        != (replacement_details.st_dev, replacement_details.st_ino)
                        or mode != 0o600
                        or contents != progress_zero
                    ):
                        os.close(replacement)
                        fail("reward evidence progress changed while initializing")
                    descriptors[1] = replacement
                    sidecar_progress = []
                self.reward_evidence_reservation = (
                    descriptors[0], descriptors[1]
                )
                self.reward_evidence_progress = list(sidecar_progress or [])
                return

            if output_is_marker:
                fail(
                    "reward evidence sidecar is neither a canonical progress journal nor its reservation"
                )
            if output_mode not in (0o444, 0o600):
                fail("finalized reward evidence JSON has unsafe permissions")
            evidence = parse_reward_evidence_payload(output_bytes, self.digest)
            expected_payload, digest, expected_sidecar = (
                self._reward_evidence_payload(evidence)
            )
            if output_bytes != expected_payload:
                fail("finalized reward evidence JSON differs from canonical bytes")

            if sidecar_progress is not None:
                if sidecar_progress != evidence:
                    fail("finalized reward evidence differs from its progress journal")
                progress_fd = descriptors[1]
                opened = os.fstat(progress_fd)
                replacement = os.open(sidecar, os.O_RDWR | nofollow)
                replacement_details = os.fstat(replacement)
                contents, mode = self._read_open_regular(
                    replacement, sidecar, 64 * 1024
                )
                expected_progress = reward_progress_payload(
                    self.digest, sidecar_progress
                )
                if (
                    (replacement_details.st_dev, replacement_details.st_ino)
                    != (opened.st_dev, opened.st_ino)
                    or mode != 0o600
                    or contents != expected_progress
                ):
                    os.close(replacement)
                    fail("reward evidence progress changed while reopening")
                os.close(progress_fd)
                descriptors[1] = replacement
                self._atomic_replace_reward_reservation(
                    sidecar, replacement, expected_progress, expected_sidecar
                )
                os.close(replacement)
                descriptors[1] = -1
            elif sidecar_is_marker:
                marker_fd = descriptors[1]
                opened = os.fstat(marker_fd)
                replacement = os.open(sidecar, os.O_RDWR | nofollow)
                replacement_details = os.fstat(replacement)
                contents, mode = self._read_open_regular(
                    replacement, sidecar, len(markers[1])
                )
                if (
                    (replacement_details.st_dev, replacement_details.st_ino)
                    != (opened.st_dev, opened.st_ino)
                    or mode != 0o600
                    or contents != markers[1]
                ):
                    os.close(replacement)
                    fail("reward evidence sidecar reservation changed while reopening")
                os.close(marker_fd)
                descriptors[1] = replacement
                self._atomic_replace_reward_reservation(
                    sidecar, replacement, markers[1], expected_sidecar
                )
                os.close(replacement)
                descriptors[1] = -1
            elif (
                sidecar_mode not in (0o444, 0o600)
                or sidecar_bytes != expected_sidecar
            ):
                fail("reward evidence sidecar does not bind the finalized JSON")

            # A crash can occur after exact final bytes are fsynced but before
            # the final chmod. Only those exact bytes may be repaired.
            for index, path in enumerate(paths):
                if descriptors[index] < 0:
                    continue
                if states[index][1] == 0o600:
                    os.fchmod(descriptors[index], 0o444)
                    os.fsync(descriptors[index])
            for fd in descriptors:
                if fd >= 0:
                    os.close(fd)
            descriptors = []
            self._fsync_directory(output.parent)
            self.existing_reward_evidence = evidence
            self.reward_evidence_progress = list(evidence)
            self.say(
                f"PASS resumed finalized rollout-bound reward evidence "
                f"{output} sha256={digest}"
            )
            return
        except Exception:
            for fd in descriptors:
                if fd >= 0:
                    os.close(fd)
            for path, marker, device, inode in reversed(created):
                try:
                    details = path.lstat()
                    if (
                        (details.st_dev, details.st_ino) == (device, inode)
                        and stat.S_ISREG(details.st_mode)
                        and path.read_bytes() == marker
                    ):
                        path.unlink()
                except OSError:
                    pass
            raise

    def verify_live(self, evidence: Sequence[ReceiptEvidence] | None = None) -> None:
        self.wait_nodes_ready()
        self.prove_boundary()
        self.prove_advancing_convergence()
        self.prove_reward_policy()
        if evidence is not None:
            validate_distinct_receipt_evidence(evidence)
            receipt_blocks = [
                self.prove_reward_receipt(item, ordinal)
                for ordinal, item in enumerate(evidence, 1)
            ]
            self.prove_reward_projection(evidence, receipt_blocks)
        # Re-check immediately before a caller may treat verification as a
        # publication signal; missing/lagging replicas fail closed.
        self.prove_visible_height_continuity()

    def import_local(self, node: Mapping[str, Any]) -> None:
        if Path(node["data_dir"]).exists():
            fail(f"{node['name']} data dir appeared before import; refusing reuse")
        result = run_checked(self.recovery_cli("import", node), timeout=600)
        try:
            body = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            fail(f"{node['name']} recovery import returned invalid JSON: {error}")
        if body.get("status") != "ACTIVATED" or body.get("height") != self.chain["transition_height"]:
            fail(f"{node['name']} recovery import did not activate exact H+1")
        if bare_hash(body.get("manifest_hash"), "import manifest hash") != bare_hash(self.chain["approved_checkpoint_manifest_hash"], "hash"):
            fail(f"{node['name']} imported an unapproved checkpoint")

    def start_local(self, node: Mapping[str, Any], run_dir: Path) -> None:
        name = node["name"]
        if name in self.processes and self.processes[name].poll() is None:
            fail(f"{name} is already running")
        log_path = run_dir / f"{name}.log"
        handle = log_path.open("ab", buffering=0)
        process = subprocess.Popen(
            self.runtime_argv(node),
            stdin=subprocess.DEVNULL,
            stdout=handle,
            stderr=subprocess.STDOUT,
            text=False,
            start_new_session=True,
        )
        self.logs[name] = handle
        self.processes[name] = process  # type: ignore[assignment]

    def stop_local(self, node: Mapping[str, Any], *, strict: bool = True) -> None:
        name = node["name"]
        process = self.processes.get(name)
        if process is None or process.poll() is not None:
            return
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=NODE_GRACEFUL_STOP_TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired:
            if strict:
                fail(f"{name} did not stop cleanly; refusing SIGKILL")
            self.say(f"WARN {name} did not stop cleanly; left running rather than SIGKILL")
        handle = self.logs.pop(name, None)
        if handle is not None:
            handle.close()

    def execute_local(self) -> None:
        run_dir = Path(self.validators[0]["data_dir"]).parent / f"{self.manifest['rollout_id']}-logs-{self.digest[:12]}"
        if run_dir.exists():
            fail(f"local run log dir already exists: {run_dir}")
        run_dir.mkdir(mode=0o700, parents=False)
        try:
            for node in self.validators:
                self.import_local(node)
                self.say(f"PASS {node['name']} imported checkpoint into a fresh data dir")
            for node in self.validators:
                self.start_local(node, run_dir)
            self.wait_nodes_ready()
            self.prove_boundary()
            self.prove_advancing_convergence()
            for node in self.validators:
                before = self.wait_convergence()[0]
                self.stop_local(node)
                self.start_local(node, run_dir)
                self.wait_nodes_ready(timeout=self.checks["restart_timeout_seconds"])
                after = self.wait_convergence(
                    minimum_height=before + self.checks["min_height_advance"],
                    timeout=self.checks["restart_timeout_seconds"],
                )
                self.say(f"PASS {node['name']} clean restart from pinned recovery state; fleet advanced #{before} -> #{after[0]}")
            self.prove_reward_policy()
            resumed_evidence = self.existing_reward_evidence is not None
            evidence = self.prove_or_resume_two_reward_receipts()
            if evidence and not resumed_evidence:
                self.persist_reward_evidence(evidence)
            complete = True
        finally:
            for node in reversed(self.validators):
                try:
                    self.stop_local(node, strict=False)
                except Exception as error:  # best-effort shutdown, data is preserved
                    self.say(f"WARN local shutdown {node['name']}: {error}")
        if complete:
            self.say(f"COMPLETE local rehearsal; processes stopped cleanly; data/logs preserved at {run_dir.parent}")

    @staticmethod
    def _systemd_escape_arg(value: str) -> str:
        return systemd_literal(value, "systemd argument")

    @staticmethod
    def gateway_service_name(node: Mapping[str, Any]) -> str:
        return node["service_name"].replace("arc-node-v3-", "arc-gateway-v3-", 1)

    @staticmethod
    def filter_service_name(node: Mapping[str, Any]) -> str:
        return node["service_name"].replace("arc-node-v3-", "arc-rpc-filter-v3-", 1)

    def filter_runtime_directory(self, node: Mapping[str, Any]) -> str:
        return f"arc-rpc-filter-{node['name']}-{self.digest[:16]}"

    def filter_public_socket(self, node: Mapping[str, Any]) -> str:
        return f"/run/{self.filter_runtime_directory(node)}/public.sock"

    def filter_archive_socket(self, node: Mapping[str, Any]) -> str:
        return f"/run/{self.filter_runtime_directory(node)}/archive.sock"

    def validator_rpc_runtime_directory(self, node: Mapping[str, Any]) -> str:
        return f"arc-v3-rpc-{node['name']}-{self.digest[:16]}"

    def validator_rpc_socket(self, node: Mapping[str, Any]) -> str:
        path = f"/run/{self.validator_rpc_runtime_directory(node)}/rpc.sock"
        if len(path.encode("utf-8")) > 100 or SAFE_REMOTE_RE.fullmatch(path) is None:
            fail("validator RPC Unix socket path is unsafe")
        return path

    @staticmethod
    def caddy_unix_upstream(path: str) -> str:
        if not path.startswith("/run/") or not SAFE_REMOTE_RE.fullmatch(path):
            fail("RPC filter Unix socket path is unsafe")
        return "unix/" + path

    @staticmethod
    def late_fork_interlock_service_name(node: Mapping[str, Any]) -> str:
        return node["service_name"].replace(
            "arc-node-v3-", "arc-late-fork-interlock-v3-", 1
        )

    def late_fork_interlock_root(self, node: Mapping[str, Any]) -> str:
        return f"/var/lib/arc-late-fork-interlock/{self.digest}/{node['name']}"

    def late_fork_interlock_runtime_directory(
        self, node: Mapping[str, Any]
    ) -> str:
        return f"arc-lfi-{node['name']}-{self.digest[:16]}"

    def late_fork_interlock_socket(self, node: Mapping[str, Any]) -> str:
        path = f"/run/{self.late_fork_interlock_runtime_directory(node)}/gate.sock"
        if len(path.encode("utf-8")) > 100 or SAFE_REMOTE_RE.fullmatch(path) is None:
            fail("late-fork interlock Unix socket path is unsafe")
        return path

    def late_fork_interlock_interpreter(
        self, node: Mapping[str, Any]
    ) -> dict[str, Any]:
        if not self.legacy_interlock_interpreters:
            self.legacy_interlock_interpreters = load_legacy_interlock_interpreters(
                self.manifest
            )
        try:
            return self.legacy_interlock_interpreters[node["name"]]
        except KeyError:
            fail(f"{node['name']} has no sealed semantic interpreter")

    def remote_semantic_python_prelude(
        self, node: Mapping[str, Any], *, descriptor: int = 9
    ) -> str:
        """Open the exact bundle-pinned host interpreter for one SSH script.

        Security-semantic remote Python must never resolve through PATH or load
        host/user Python configuration.  The descriptor remains open across
        every isolated invocation, so a pathname replacement cannot change the
        interpreter used midway through a production transaction.
        """

        if descriptor not in range(3, 10):
            fail("semantic interpreter descriptor is outside the reviewed range")
        interpreter = self.late_fork_interlock_interpreter(node)
        path = shlex.quote(interpreter["normalized_path"])
        identity = shlex.quote(
            f"{interpreter['device']}:{interpreter['inode']}:"
            f"{interpreter['uid']}:{interpreter['gid']}:"
            f"{interpreter['mode']:o}:{interpreter['nlink']}"
        )
        digest = shlex.quote(interpreter["sha256"])
        return f"""arc_semantic_python_path={path}
arc_semantic_python_identity={identity}
arc_semantic_python_sha256={digest}
test -f "$arc_semantic_python_path" && test ! -L "$arc_semantic_python_path"
test "$(/usr/bin/stat -c %d:%i:%u:%g:%a:%h "$arc_semantic_python_path")" = "$arc_semantic_python_identity"
exec {descriptor}<"$arc_semantic_python_path"
arc_semantic_python_revalidate() {{
  test "$(/usr/bin/stat -Lc %d:%i:%u:%g:%a:%h /proc/self/fd/{descriptor})" = "$arc_semantic_python_identity"
  test "$(/usr/bin/sha256sum /proc/self/fd/{descriptor} | /usr/bin/cut -d' ' -f1)" = "$arc_semantic_python_sha256"
}}
arc_semantic_python() {{
  /usr/bin/env -i HOME=/root PATH=/usr/bin:/bin LANG=C LC_ALL=C TZ=UTC PYTHONHASHSEED=0 /proc/self/fd/{descriptor} -I "$@"
}}
arc_semantic_python_revalidate
"""

    def late_fork_interlock_launcher(self, node: Mapping[str, Any]) -> str:
        interpreter = self.late_fork_interlock_interpreter(node)
        path = interpreter["normalized_path"]
        identity = (
            f"{interpreter['device']}:{interpreter['inode']}:"
            f"{interpreter['uid']}:{interpreter['gid']}:"
            f"{interpreter['mode']:o}:{interpreter['nlink']}"
        )
        root = self.late_fork_interlock_root(node)
        return f"""#!/bin/sh
set -eu
python={path}
test -f "$python" && test ! -L "$python"
test "$(/usr/bin/stat -c %d:%i:%u:%g:%a:%h "$python")" = {identity}
test "$(/usr/bin/sha256sum "$python" | /usr/bin/cut -d' ' -f1)" = {interpreter['sha256']}
exec /usr/bin/env -i PATH=/usr/bin:/bin LC_ALL=C TZ=UTC PYTHONHASHSEED=0 \
  "$python" -I {root}/legacy-late-fork-interlock.py serve \
  --source-set {root}/legacy-late-fork-source-set.json \
  --source-set-sha256 {self.chain['legacy_late_fork_source_set_sha256']} \
  --boundary-sha256 {self.chain['legacy_maintenance_boundary_sha256']} \
  --tool-sha256 {self.manifest['artifacts']['legacy_late_fork_interlock_tool']['sha256']} \
  --state-root {root}/state --listen-unix {self.late_fork_interlock_socket(node)}
"""

    def late_fork_interlock_unit(self, node: Mapping[str, Any]) -> str:
        root = self.late_fork_interlock_root(node)
        runtime = self.late_fork_interlock_runtime_directory(node)
        return f"""[Unit]
Description=ARC fail-closed legacy late-fork monitor {node['name']} ({self.manifest['rollout_id']})
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User={LATE_FORK_INTERLOCK_USER}
Group={LATE_FORK_INTERLOCK_GROUP}
UMask=0007
RuntimeDirectory={runtime}
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=no
ExecStart={root}/launch
Restart=always
RestartSec=2s
TimeoutStopSec=30s
KillSignal=SIGTERM
NoNewPrivileges=true
PrivateDevices=true
PrivateTmp=true
ProtectClock=true
ProtectControlGroups=true
ProtectHome=true
ProtectKernelLogs=true
ProtectKernelModules=true
ProtectKernelTunables=true
ProtectProc=invisible
ProtectSystem=strict
ProcSubset=pid
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
RestrictNamespaces=true
RestrictRealtime=true
RestrictSUIDSGID=true
LockPersonality=true
MemoryDenyWriteExecute=true
RemoveIPC=true
CapabilityBoundingSet=
AmbientCapabilities=
ReadOnlyPaths={root}
ReadWritePaths={root}/state /run/{runtime}
LimitNOFILE=4096

[Install]
WantedBy=multi-user.target
"""

    def legacy_archive_for(self, node: Mapping[str, Any]) -> dict[str, Any] | None:
        return self.legacy_archive_forks.get(node["name"])

    def legacy_archive_root(self, node: Mapping[str, Any]) -> str:
        return f"/var/lib/arc-legacy-archive/{self.digest}/{node['name']}"

    def legacy_archive_runtime_directory(self, node: Mapping[str, Any]) -> str:
        return f"arc-archive-rpc-{node['name']}-{self.digest[:16]}"

    def legacy_archive_rpc_socket(self, node: Mapping[str, Any]) -> str:
        path = f"/run/{self.legacy_archive_runtime_directory(node)}/archive.sock"
        if len(path.encode("utf-8")) > 100 or SAFE_REMOTE_RE.fullmatch(path) is None:
            fail("legacy archive RPC Unix socket path is unsafe")
        return path

    @staticmethod
    def legacy_archive_service_name(node: Mapping[str, Any]) -> str:
        return node["service_name"].replace(
            "arc-node-v3-", "arc-legacy-archive-v3-", 1
        )

    def legacy_archive_argv(self, node: Mapping[str, Any]) -> list[str]:
        if self.legacy_archive_for(node) is None:
            fail(f"{node['name']} is not a sealed noncanonical legacy fork")
        root = self.legacy_archive_root(node)
        archive = self.manifest["archive"]
        return [
            f"{root}/arc-node",
            "archive",
            "serve",
            "--archive-manifest",
            f"{root}/ARCHIVE-MANIFEST.json",
            "--complete",
            f"{root}/COMPLETE.json",
            "--inventory",
            f"{root}/legacy-{node['name']}.inventory",
            "--binding-index",
            f"{root}/binding.files.sha256",
            "--binding",
            f"{root}/binding.json",
            "--checkpoint",
            f"{root}/candidate.arcchkpt",
            "--expected-archive-manifest-sha256",
            archive["archive_manifest_sha256"],
            "--expected-complete-sha256",
            archive["complete_sha256"],
            "--node",
            node["name"],
            "--listen-unix",
            self.legacy_archive_rpc_socket(node),
        ]

    def legacy_archive_unit(self, node: Mapping[str, Any]) -> str:
        if self.legacy_archive_for(node) is None:
            fail(f"{node['name']} is not a sealed noncanonical legacy fork")
        root = self.legacy_archive_root(node)
        runtime = self.legacy_archive_runtime_directory(node)
        argv = " ".join(
            self._systemd_escape_arg(arg) for arg in self.legacy_archive_argv(node)
        )
        return f"""[Unit]
Description=ARC immutable legacy fork archive {node['name']} ({self.manifest['rollout_id']})
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User={LEGACY_ARCHIVE_USER}
Group={RPC_ORIGIN_GROUP}
SupplementaryGroups={LEGACY_ARCHIVE_USER}
UMask=0007
RuntimeDirectory={runtime}
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=no
ExecStart={argv}
Restart=on-failure
RestartSec=2s
TimeoutStopSec=30s
KillSignal=SIGTERM
NoNewPrivileges=true
PrivateDevices=true
PrivateTmp=true
ProtectClock=true
ProtectControlGroups=true
ProtectHome=true
ProtectKernelLogs=true
ProtectKernelModules=true
ProtectKernelTunables=true
ProtectSystem=strict
RestrictAddressFamilies=AF_UNIX
RestrictNamespaces=true
RestrictRealtime=true
RestrictSUIDSGID=true
LockPersonality=true
MemoryDenyWriteExecute=true
RemoveIPC=true
CapabilityBoundingSet=
AmbientCapabilities=
ReadOnlyPaths={root}
ReadWritePaths=/run/{runtime}
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
"""

    def systemd_unit(self, node: Mapping[str, Any]) -> str:
        argv = " ".join(self._systemd_escape_arg(arg) for arg in self.runtime_argv(node, remote=True))
        runtime = self.validator_rpc_runtime_directory(node)
        return f"""[Unit]
Description=ARC protocol-v3 validator {node['name']} ({self.manifest['rollout_id']})
After=network-online.target {self.filter_service_name(node)} {self.gateway_service_name(node)}
Wants=network-online.target
Requires={self.filter_service_name(node)} {self.gateway_service_name(node)}

[Service]
Type=simple
User={node['service_user']}
Group={RPC_ORIGIN_GROUP}
UMask=0007
RuntimeDirectory={runtime}
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=no
Environment=ARC_PUBLIC_SOCKET={self._systemd_escape_arg(node['rpc_url'])}
ExecStart={argv}
Restart=on-failure
RestartSec=2s
TimeoutStopSec={NODE_GRACEFUL_STOP_TIMEOUT_SECONDS}s
KillSignal=SIGTERM
NoNewPrivileges=true
PrivateTmp=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
ReadWritePaths=/run/{runtime}
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
"""

    def maintenance_caddyfile(self, node: Mapping[str, Any]) -> str:
        """TLS edge used until the all-six continuity proof is durable.

        Validator-only control traffic remains available from the exact fixed
        fleet, while every ordinary public route returns a deterministic 503.
        The live allowlist is staged under a different hash and cannot become
        active merely because Caddy or the host reboots.
        """

        validator_ips = " ".join(peer["host"] for peer in self.validators)
        public_filter = self.caddy_unix_upstream(self.filter_public_socket(node))
        status_proxy = f"""reverse_proxy {public_filter} {{
            header_up Host 127.0.0.1
            header_up X-Forwarded-For {{remote_host}}
        }}"""
        return f"""{{
    email {self.manifest['gateway']['acme_email']}
    admin off
}}

{node['host']} {{
    tls {{
        issuer acme {LETS_ENCRYPT_PRODUCTION_DIRECTORY} {{
            profile shortlived
            disable_tlsalpn_challenge
        }}
    }}

    header {{
        Strict-Transport-Security "max-age=31536000; includeSubDomains"
        X-Content-Type-Options "nosniff"
        X-Frame-Options "DENY"
        Referrer-Policy "no-referrer"
        Content-Security-Policy "default-src 'none'; frame-ancestors 'none'; base-uri 'none'"
        -Server
    }}

    @maintenanceStatusCors {{
        method GET
        path /maintenance/status
        header Origin {PUBLIC_BROWSER_ORIGIN}
    }}
    handle @maintenanceStatusCors {{
        header Access-Control-Allow-Origin "{PUBLIC_BROWSER_ORIGIN}"
        header Vary "Origin"
        {status_proxy}
    }}
    handle /maintenance/status {{
        {status_proxy}
    }}

    @validatorControl {{
        method POST
        path /internal/community/reward/approve /shards/announce /inference/forward_shard /inference/cleanup_shard
        remote_ip {validator_ips}
    }}
    handle @validatorControl {{
        request_body {{
            max_size 4MB
        }}
        reverse_proxy {public_filter} {{
            header_up Host 127.0.0.1
            header_up X-Forwarded-For {{remote_host}}
        }}
    }}

    # ACME/TLS readiness is distinguishable from application publication.
    handle /__arc_tls_probe__ {{
        respond "" 404
    }}

    handle {{
        header Content-Type "application/json"
        header Retry-After "60"
        respond `{{"schema":"arc.recovery.public-maintenance.v1","status":"maintenance"}}` 503
    }}
}}
"""

    def caddyfile(self, node: Mapping[str, Any]) -> str:
        get_paths = " ".join(self.manifest["gateway"]["public_get_paths"])
        post_paths = " ".join(self.manifest["gateway"]["public_post_paths"])
        validator_ips = " ".join(peer["host"] for peer in self.validators)
        public_filter = self.caddy_unix_upstream(self.filter_public_socket(node))
        archive_filter = self.caddy_unix_upstream(self.filter_archive_socket(node))
        status_proxy = f"""reverse_proxy {public_filter} {{
            header_up Host 127.0.0.1
            header_up X-Forwarded-For {{remote_host}}
        }}"""
        archive_route = ""
        if self.legacy_archive_for(node) is not None:
            prefix = f"/legacy/{node['name']}"
            archive_route = f"""
    # The fork archive is intentionally a separate GET-only origin behind the
    # same trusted certificate.  Strip only its sealed node-derived prefix.
    @legacyArchiveCors {{
        method GET
        header Origin {PUBLIC_BROWSER_ORIGIN}
        path {prefix}/*
    }}
    handle @legacyArchiveCors {{
        uri strip_prefix {prefix}
        header Access-Control-Allow-Origin "{PUBLIC_BROWSER_ORIGIN}"
        header Vary "Origin"
        request_body {{
            max_size 1KB
        }}
        reverse_proxy {archive_filter} {{
            header_up Host 127.0.0.1
            header_up X-Forwarded-For {{remote_host}}
        }}
    }}

    @legacyArchive {{
        path {prefix}/*
    }}
    handle @legacyArchive {{
        uri strip_prefix {prefix}
        request_body {{
            max_size 1KB
        }}
        reverse_proxy {archive_filter} {{
            header_up Host 127.0.0.1
            header_up X-Forwarded-For {{remote_host}}
        }}
    }}
"""
        return f"""{{
    email {self.manifest['gateway']['acme_email']}
    admin off
}}

{node['host']} {{
    # Public IPv4 certificates are six-day certificates. Pin Let's Encrypt's
    # short-lived ACME profile and HTTP-01 explicitly so rollout never depends
    # on a shared wildcard-DNS operator or Caddy's local-IP CA fallback.
    tls {{
        issuer acme {LETS_ENCRYPT_PRODUCTION_DIRECTORY} {{
            profile shortlived
            disable_tlsalpn_challenge
        }}
    }}

    header {{
        Strict-Transport-Security "max-age=31536000; includeSubDomains"
        X-Content-Type-Options "nosniff"
        X-Frame-Options "DENY"
        Referrer-Policy "no-referrer"
        Content-Security-Policy "default-src 'none'; frame-ancestors 'none'; base-uri 'none'"
        -Server
    }}

    @maintenanceStatusCors {{
        method GET
        path /maintenance/status
        header Origin {PUBLIC_BROWSER_ORIGIN}
    }}
    handle @maintenanceStatusCors {{
        header Access-Control-Allow-Origin "{PUBLIC_BROWSER_ORIGIN}"
        header Vary "Origin"
        {status_proxy}
    }}
    handle /maintenance/status {{
        {status_proxy}
    }}
{archive_route}

    # Browser preflight is answered at the public TLS edge. It is never
    # proxied to the node, and these matchers deliberately exclude every
    # inter-validator/internal route.
    @readPreflight {{
        method OPTIONS
        header Origin {PUBLIC_BROWSER_ORIGIN}
        header Access-Control-Request-Method GET
        path {get_paths} /block/* /tx/* /account/* /account/*/txs /worker/earnings/* /community/reward_receipt/* /community/reward_job/*
    }}
    handle @readPreflight {{
        header Access-Control-Allow-Origin "{PUBLIC_BROWSER_ORIGIN}"
        header Vary "Origin"
        header Access-Control-Allow-Methods "GET, OPTIONS"
        header Access-Control-Allow-Headers "Accept, Content-Type"
        header Access-Control-Max-Age "600"
        respond "" 204
    }}

    @writePreflight {{
        method OPTIONS
        header Origin {PUBLIC_BROWSER_ORIGIN}
        header Access-Control-Request-Method POST
        path {post_paths}
    }}
    handle @writePreflight {{
        header Access-Control-Allow-Origin "{PUBLIC_BROWSER_ORIGIN}"
        header Vary "Origin"
        header Access-Control-Allow-Methods "POST, OPTIONS"
        header Access-Control-Allow-Headers "Accept, Content-Type"
        header Access-Control-Max-Age "600"
        respond "" 204
    }}

    @corsRead {{
        method GET
        header Origin {PUBLIC_BROWSER_ORIGIN}
        path {get_paths} /block/* /tx/* /account/* /account/*/txs /worker/earnings/* /community/reward_receipt/* /community/reward_job/*
    }}
    handle @corsRead {{
        header Access-Control-Allow-Origin "{PUBLIC_BROWSER_ORIGIN}"
        header Vary "Origin"
        request_body {{
            max_size 1MB
        }}
        reverse_proxy {public_filter} {{
            header_up Host 127.0.0.1
            header_up X-Forwarded-For {{remote_host}}
        }}
    }}

    @read {{
        method GET
        path {get_paths} /block/* /tx/* /account/* /account/*/txs /worker/earnings/* /community/reward_receipt/* /community/reward_job/*
    }}
    handle @read {{
        request_body {{
            max_size 1MB
        }}
        reverse_proxy {public_filter} {{
            header_up Host 127.0.0.1
            header_up X-Forwarded-For {{remote_host}}
        }}
    }}

    @corsWrite {{
        method POST
        header Origin {PUBLIC_BROWSER_ORIGIN}
        path {post_paths}
    }}
    handle @corsWrite {{
        header Access-Control-Allow-Origin "{PUBLIC_BROWSER_ORIGIN}"
        header Vary "Origin"
        request_body {{
            max_size 1MB
        }}
        reverse_proxy {public_filter} {{
            header_up Host 127.0.0.1
            header_up X-Forwarded-For {{remote_host}}
        }}
    }}

    @write {{
        method POST
        path {post_paths}
    }}
    handle @write {{
        request_body {{
            max_size 1MB
        }}
        reverse_proxy {public_filter} {{
            header_up Host 127.0.0.1
            header_up X-Forwarded-For {{remote_host}}
        }}
    }}

    @validatorApproval {{
        method POST
        path /internal/community/reward/approve
        remote_ip {validator_ips}
    }}
    handle @validatorApproval {{
        request_body {{
            max_size 1MB
        }}
        reverse_proxy {public_filter} {{
            header_up Host 127.0.0.1
            header_up X-Forwarded-For {{remote_host}}
        }}
    }}

    @validatorShard {{
        method POST
        path /shards/announce /inference/forward_shard /inference/cleanup_shard
        remote_ip {validator_ips}
    }}
    handle @validatorShard {{
        request_body {{
            max_size 4MB
        }}
        reverse_proxy {public_filter} {{
            header_up Host 127.0.0.1
            header_up X-Forwarded-For {{remote_host}}
        }}
    }}

    handle {{
        respond "not found" 404
    }}
}}
"""

    def nginx_filter(self, node: Mapping[str, Any]) -> str:
        zone = re.sub(r"[^a-z0-9]", "", node["name"])[0:20]
        allow_lines = "\n".join(f"        allow {peer['host']};" for peer in self.validators)
        upstream = f"http://unix:{self.validator_rpc_socket(node)}"
        root = node["remote_root"]
        public_socket = self.filter_public_socket(node)
        archive_socket = self.filter_archive_socket(node)
        interlock_socket = self.late_fork_interlock_socket(node)
        gate_proxy = f"http://unix:{interlock_socket}:/gate"
        status_proxy = f"http://unix:{interlock_socket}:/maintenance/status"
        archive_server = ""
        if self.legacy_archive_for(node) is not None:
            archive_server = f"""
    # Dedicated loopback filter for an immutable fork reader. Non-GET methods
    # are an explicit 405 and all unreviewed archive paths fail closed.
    server {{
        listen unix:{archive_socket};
        if ($arc_loopback_transport = 0) {{ return 403; }}
        client_max_body_size 1k;
        if ($request_method != GET) {{ return 405; }}
        location = /__arc_interlock_gate {{
            internal;
            proxy_pass {gate_proxy};
            proxy_pass_request_body off;
            proxy_set_header Content-Length "";
            proxy_set_header X-Original-URI $request_uri;
        }}
        location ~ "^/(?:provenance|health|info|stats|validators|block/latest|blocks|block/[0-9]+(?:/txs)?|tx/(?:0x)?[0-9a-fA-F]{{64}}(?:/full|/occurrences)?|account/(?:0x)?[0-9a-fA-F]{{64}}(?:/txs)?)$" {{
            auth_request /__arc_interlock_gate;
            limit_req zone=arc_read_{zone} burst=60 nodelay;
            limit_conn arc_conn_{zone} 4;
            proxy_pass http://unix:{self.legacy_archive_rpc_socket(node)};
            proxy_http_version 1.1;
            proxy_read_timeout 60s;
        }}
        location / {{ return 404; }}
    }}
"""
        return f"""daemon off;
worker_processes 1;
pid {root}/rpc-filter-state/nginx-filter.pid;
error_log {root}/rpc-filter-state/nginx-filter-error.log warn;

events {{ worker_connections 4096; }}

http {{
    access_log {root}/rpc-filter-state/nginx-filter-access.log;
    server_tokens off;
    limit_req_zone $binary_remote_addr zone=arc_read_{zone}:10m rate=30r/s;
    limit_req_zone $binary_remote_addr zone=arc_write_{zone}:10m rate=30r/m;
    limit_req_zone $binary_remote_addr zone=arc_shard_{zone}:10m rate=100r/s;
    limit_conn_zone $binary_remote_addr zone=arc_conn_{zone}:10m;
    real_ip_header X-Forwarded-For;
    real_ip_recursive on;
    proxy_set_header Host 127.0.0.1;
    proxy_set_header X-Forwarded-For "";

    # Access is gated on the original Unix peer, not on the client address
    # supplied by the trusted proxy.  Caddy overwrites X-Forwarded-For with
    # its directly observed client, so $remote_addr remains safe for per-client
    # rate limits and the validator-only allowlists below.
    set_real_ip_from unix:;
    map $realip_remote_addr $arc_loopback_transport {{
        default 0;
        unix: 1;
    }}

    # Only Caddy can reach the public HTTP filter.  This preserves the real
    # client IP for per-IP limits while keeping the node itself on loopback.
    server {{
        listen unix:{public_socket};
        if ($arc_loopback_transport = 0) {{ return 403; }}
        client_max_body_size 1m;
        location = /__arc_interlock_gate {{
            internal;
            proxy_pass {gate_proxy};
            proxy_pass_request_body off;
            proxy_set_header Content-Length "";
            proxy_set_header X-Original-URI $request_uri;
        }}
        location = /maintenance/status {{
            limit_except GET {{ deny all; }}
            proxy_pass {status_proxy};
            proxy_pass_request_body off;
            proxy_set_header Content-Length "";
        }}
        location ~ ^/(?:health|info|network/info|stats|validators|block/latest|blocks|inference/attestations|economics/rewards|faucet/status|community/list|community/reward_policy|workers/scoreboard|shards|models|models/shards)$ {{
            auth_request /__arc_interlock_gate;
            limit_except GET OPTIONS {{ deny all; }}
            limit_req zone=arc_read_{zone} burst=60 nodelay;
            proxy_pass {upstream};
            proxy_http_version 1.1;
            proxy_read_timeout 60s;
        }}
        location ~ "^/(?:block/[0-9]+(?:/txs)?|tx/(?:0x)?[0-9a-fA-F]{{64}}(?:/full)?|account/(?:0x)?[0-9a-fA-F]{{64}}(?:/txs)?|worker/earnings/(?:0x)?[0-9a-fA-F]{{64}}|community/reward_(?:receipt|job)/(?:0x)?[0-9a-fA-F]{{64}})$" {{
            auth_request /__arc_interlock_gate;
            limit_except GET OPTIONS {{ deny all; }}
            limit_req zone=arc_read_{zone} burst=60 nodelay;
            proxy_pass {upstream};
            proxy_http_version 1.1;
            proxy_read_timeout 60s;
        }}
        location ~ ^/inference/run(?:_consensus)?$ {{
            auth_request /__arc_interlock_gate;
            limit_except POST OPTIONS {{ deny all; }}
            limit_req zone=arc_write_{zone} burst=10 nodelay;
            limit_conn arc_conn_{zone} 4;
            proxy_pass {upstream};
            proxy_http_version 1.1;
            proxy_read_timeout {PUBLIC_INFERENCE_TIMEOUT_SECONDS}s;
            proxy_send_timeout 60s;
        }}
        location = /community/submit_work {{
            auth_request /__arc_interlock_gate;
            limit_except POST OPTIONS {{ deny all; }}
            limit_req zone=arc_write_{zone} burst=10 nodelay;
            limit_conn arc_conn_{zone} 4;
            proxy_pass {upstream};
            proxy_http_version 1.1;
            proxy_read_timeout {WORKER_SUBMIT_TIMEOUT_SECONDS}s;
            proxy_send_timeout 60s;
        }}
        location ~ ^/(?:community/(?:register|heartbeat|claim_work)|tx/submit(?:_signed|_batch)?)$ {{
            auth_request /__arc_interlock_gate;
            limit_except POST OPTIONS {{ deny all; }}
            limit_req zone=arc_write_{zone} burst=10 nodelay;
            limit_conn arc_conn_{zone} 4;
            proxy_pass {upstream};
            proxy_http_version 1.1;
            proxy_read_timeout 120s;
            proxy_send_timeout 60s;
        }}
        location = /faucet/claim {{
            auth_request /__arc_interlock_gate;
            limit_except POST OPTIONS {{ deny all; }}
            limit_req zone=arc_write_{zone} burst=2 nodelay;
            limit_conn arc_conn_{zone} 2;
            proxy_pass {upstream};
            proxy_http_version 1.1;
            proxy_read_timeout 120s;
            proxy_send_timeout 30s;
        }}
        location = /internal/community/reward/approve {{
            auth_request /__arc_interlock_gate;
{allow_lines}
            deny all;
            limit_except POST {{ deny all; }}
            limit_req zone=arc_write_{zone} burst=10 nodelay;
            limit_conn arc_conn_{zone} 8;
            proxy_pass {upstream};
            proxy_http_version 1.1;
            proxy_read_timeout {VALIDATOR_APPROVAL_TIMEOUT_SECONDS}s;
        }}
        location ~ ^/(?:shards/announce|inference/(?:forward_shard|cleanup_shard))$ {{
            auth_request /__arc_interlock_gate;
{allow_lines}
            deny all;
            limit_except POST {{ deny all; }}
            client_max_body_size 4m;
            limit_req zone=arc_shard_{zone} burst=200 nodelay;
            proxy_pass {upstream};
            proxy_http_version 1.1;
            proxy_read_timeout 180s;
            proxy_send_timeout 180s;
        }}
        location / {{ return 404; }}
    }}
{archive_server}
}}
"""

    @staticmethod
    def validate_gateway_security_contract(
        maintenance: str, live: str, rpc_filter: str
    ) -> None:
        """Reject unsafe generated proxy combinations before remote staging.

        Caddy v2.11.4 is deliberately retained only with the vulnerable
        ``forward_auth``+``reverse_proxy`` composition removed completely.
        The fail-closed interlock is enforced by nginx ``auth_request`` at the
        sole loopback upstream instead, so Caddy can never race two upstream
        selections.  The admin API and dynamic/content-serving handlers stay
        disabled in both public configurations.
        """

        for label, config in (("maintenance Caddy", maintenance), ("live Caddy", live)):
            if config.count("admin off") != 1:
                fail(f"{label} must disable the Caddy admin API exactly once")
            for forbidden in (
                "forward_auth", "file_server", "templates", "php_fastcgi",
                "fastcgi", "cgi", "admin 127.", "admin localhost",
            ):
                if forbidden in config:
                    fail(f"{label} contains forbidden handler/configuration: {forbidden}")
        if "forward_auth" in rpc_filter:
            fail("RPC filter must not reintroduce a Caddy forward_auth handler")
        gate_proxy = "gate.sock:/gate;"
        gate_proxy_count = rpc_filter.count(gate_proxy)
        if gate_proxy_count not in {1, 2}:
            fail("RPC filter does not bind every active upstream class to the interlock")
        proxy_count = rpc_filter.count("proxy_pass http://")
        auth_count = rpc_filter.count("auth_request /__arc_interlock_gate;")
        # There is one unauthenticated status proxy on the public filter.  It
        # exposes only the interlock's own fail-closed status and cannot mutate
        # or reach either application origin.
        status_proxy_count = rpc_filter.count("gate.sock:/maintenance/status;")
        if (
            proxy_count not in {10, 12}
            or status_proxy_count != 1
            or auth_count != proxy_count - gate_proxy_count - status_proxy_count
        ):
            fail("RPC filter omits the fail-closed interlock from a public upstream class")
        # Nginx configuration is generated locally, but future template edits
        # must still fail closed.  Inspect every complete location block and
        # require exactly one auth_request whenever it proxies anywhere other
        # than the internal interlock subrequest itself.  Exact total counts
        # above prevent moving/duplicating an auth_request between locations.
        lines = rpc_filter.splitlines()
        for line in lines:
            if "{64}" in line and line.lstrip().startswith("location ~ "):
                if re.fullmatch(r'\s*location ~ "[^"]+" \{\s*', line) is None:
                    fail("RPC filter quantified regex must be one quoted nginx token")
        index = 0
        proxying_locations = 0
        while index < len(lines):
            if not lines[index].lstrip().startswith("location "):
                index += 1
                continue
            block: list[str] = []
            depth = 0
            while index < len(lines):
                line = lines[index]
                block.append(line)
                depth += line.count("{") - line.count("}")
                index += 1
                if depth == 0:
                    break
            text = "\n".join(block)
            block_proxy_count = text.count("proxy_pass http://")
            if block_proxy_count == 0:
                continue
            if block_proxy_count != 1:
                fail("RPC filter location has an inexact upstream topology")
            if gate_proxy in text:
                if text.count("auth_request /__arc_interlock_gate;") != 0:
                    fail("RPC filter internal interlock location is recursively gated")
            elif "location = /maintenance/status" in text:
                if text.count("auth_request /__arc_interlock_gate;") != 0:
                    fail("interlock status endpoint must remain observable during maintenance")
            else:
                proxying_locations += 1
                if text.count("auth_request /__arc_interlock_gate;") != 1:
                    fail("RPC filter proxying location is not exactly fail-closed")
        if proxying_locations != auth_count:
            fail("RPC filter proxy/location inventory differs")
        if (
            "listen 127.0.0.1:18080;" in rpc_filter
            or f"listen 127.0.0.1:{LEGACY_ARCHIVE_FILTER_PORT};" in rpc_filter
            or rpc_filter.count("listen unix:/run/arc-rpc-filter-")
            != gate_proxy_count
            or rpc_filter.count("set_real_ip_from unix:;") != 1
            or "set_real_ip_from 127." in rpc_filter
            or "unix: 1;" not in rpc_filter
            or "127.0.0.1:18081" in rpc_filter
            or rpc_filter.count("proxy_pass http://unix:/run/arc-lfi-")
            != gate_proxy_count + 1
            or rpc_filter.count("proxy_pass http://unix:/run/arc-v3-rpc-") != 8
            or rpc_filter.count("proxy_pass http://unix:/run/arc-archive-rpc-")
            != gate_proxy_count - 1
            or "proxy_pass http://127.0.0.1:" in rpc_filter
            or rpc_filter.count("real_ip_header X-Forwarded-For;") != 1
            or rpc_filter.count("real_ip_recursive on;") != 1
            or rpc_filter.count('proxy_set_header X-Forwarded-For "";') != 1
            or rpc_filter.count("proxy_set_header Host 127.0.0.1;") != 1
            or len(
                re.findall(
                    r"(?im)^\s*proxy_set_header\s+x-forwarded-for\b",
                    rpc_filter,
                )
            )
            != 1
        ):
            fail("RPC filter is not permission-sealed on exact Unix sockets")
        for label, config in (("maintenance Caddy", maintenance), ("live Caddy", live)):
            unix_upstreams = config.count("reverse_proxy unix//run/arc-rpc-filter-")
            if unix_upstreams < 1:
                fail(f"{label} does not use the permission-sealed RPC filter socket")
            all_proxy_lines = [
                line.strip()
                for line in config.splitlines()
                if line.strip().startswith("reverse_proxy ")
            ]
            if (
                len(all_proxy_lines) != unix_upstreams
                or any(
                    not line.startswith("reverse_proxy unix//run/arc-rpc-filter-")
                    for line in all_proxy_lines
                )
            ):
                fail(f"{label} contains an unsealed or unexpected upstream")
            caddy_lines = config.splitlines()
            index = 0
            inspected = 0
            while index < len(caddy_lines):
                line = caddy_lines[index]
                if "reverse_proxy unix//run/arc-rpc-filter-" not in line:
                    index += 1
                    continue
                if not line.rstrip().endswith("{"):
                    fail(f"{label} Unix upstream is not an explicit sealed block")
                block: list[str] = []
                depth = 0
                while index < len(caddy_lines):
                    current = caddy_lines[index]
                    block.append(current)
                    depth += current.count("{") - current.count("}")
                    index += 1
                    if depth == 0:
                        break
                if depth != 0:
                    fail(f"{label} Unix upstream block is unterminated")
                block_text = "\n".join(block)
                host_headers = [
                    candidate.strip()
                    for candidate in block
                    if re.match(r"(?i)^\s*header_up\s+host\b", candidate)
                ]
                xff_headers = [
                    candidate.strip()
                    for candidate in block
                    if re.match(
                        r"(?i)^\s*header_up\s+x-forwarded-for\b", candidate
                    )
                ]
                if host_headers != ["header_up Host 127.0.0.1"]:
                    fail(f"{label} Unix upstream omits the exact Host override")
                if xff_headers != ["header_up X-Forwarded-For {remote_host}"]:
                    fail(f"{label} Unix upstream does not exactly overwrite X-Forwarded-For")
                inspected += 1
            if inspected != unix_upstreams:
                fail(f"{label} Unix upstream inventory differs")

    def gateway_unit(self, node: Mapping[str, Any]) -> str:
        root = node["remote_root"]
        archive_dependency = ""
        if self.legacy_archive_for(node) is not None:
            archive_dependency = " " + self.legacy_archive_service_name(node)
        return f"""[Unit]
Description=ARC HTTPS gateway {node['name']} ({self.manifest['rollout_id']})
After=network-online.target {self.filter_service_name(node)} {self.late_fork_interlock_service_name(node)}{archive_dependency}
Wants=network-online.target{archive_dependency}
Requires={self.filter_service_name(node)} {self.late_fork_interlock_service_name(node)}

[Service]
Type=simple
User={CADDY_USER}
Group={CADDY_USER}
UMask=0077
Environment=XDG_DATA_HOME={root}/caddy-data
Environment=XDG_CONFIG_HOME={root}/caddy-config
ExecStart={root}/caddy run --config {root}/Caddyfile.active --adapter caddyfile
Restart=on-failure
RestartSec=2s
TimeoutStopSec=30s
KillSignal=SIGTERM
NoNewPrivileges=true
PrivateDevices=true
PrivateTmp=true
ProtectClock=true
ProtectControlGroups=true
ProtectHome=true
ProtectKernelLogs=true
ProtectKernelModules=true
ProtectKernelTunables=true
ProtectProc=invisible
ProtectSystem=strict
ProcSubset=pid
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
RestrictNamespaces=true
RestrictRealtime=true
RestrictSUIDSGID=true
LockPersonality=true
MemoryDenyWriteExecute=true
RemoveIPC=true
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
AmbientCapabilities=CAP_NET_BIND_SERVICE
ReadOnlyPaths={root}
ReadWritePaths={root}/caddy-data {root}/caddy-config
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
"""

    def filter_preflight(self, node: Mapping[str, Any]) -> str:
        root = node["remote_root"]
        config_sha = sha256_bytes(self.nginx_filter(node).encode("utf-8"))
        runtime = f"/run/{self.filter_runtime_directory(node)}"
        interlock_socket = self.late_fork_interlock_socket(node)
        interlock_runtime = os.path.dirname(interlock_socket)
        sockets = [self.filter_public_socket(node)]
        if self.legacy_archive_for(node) is not None:
            sockets.append(self.filter_archive_socket(node))
        socket_checks = "\n".join(
            f"test ! -e {path}\ntest ! -L {path}" for path in sockets
        )
        return f"""#!/bin/sh
set -eu
test -f /usr/sbin/nginx
test ! -L /usr/sbin/nginx
test "$(stat -c %U:%G:%a:%h /usr/sbin/nginx)" = root:root:755:1
test "$(/usr/bin/dpkg-query -W -f='${{Version}}' nginx)" = {NGINX_PACKAGE_VERSION}
printf '%s  %s\n' {NGINX_LINUX_AMD64_SHA256} /usr/sbin/nginx | /usr/bin/sha256sum --check --strict
/usr/sbin/nginx -V 2>&1 | /usr/bin/grep -Fq -- --with-http_auth_request_module
test -f {root}/nginx-filter.conf
test ! -L {root}/nginx-filter.conf
test "$(stat -c %U:%G:%a:%h {root}/nginx-filter.conf)" = root:{CADDY_USER}:440:1
printf '%s  %s\n' {config_sha} {root}/nginx-filter.conf | /usr/bin/sha256sum --check --strict
test -d {runtime}
test ! -L {runtime}
test "$(stat -c %U:%G:%a {runtime})" = {NGINX_FILTER_USER}:{CADDY_USER}:750
test -d {interlock_runtime}
test ! -L {interlock_runtime}
test "$(stat -c %U:%G:%a {interlock_runtime})" = {LATE_FORK_INTERLOCK_USER}:{LATE_FORK_INTERLOCK_GROUP}:750
interlock_ready=false
for _ in $(seq 1 60); do
  if test -S {interlock_socket} && test ! -L {interlock_socket}; then
    interlock_ready=true
    break
  fi
  systemctl is-active --quiet {self.late_fork_interlock_service_name(node)} || exit 1
  sleep 1
done
test "$interlock_ready" = true
test "$(stat -c %U:%G:%a:%h {interlock_socket})" = {LATE_FORK_INTERLOCK_USER}:{LATE_FORK_INTERLOCK_GROUP}:660:1
test -r {interlock_socket}
test -w {interlock_socket}
test -z "$(ss -H -ltnp | awk '$4 ~ /:18081$/ {{print}}')"
{socket_checks}
exec /usr/sbin/nginx -t -c {root}/nginx-filter.conf -p {root}/rpc-filter-state
"""

    def filter_unit(self, node: Mapping[str, Any]) -> str:
        root = node["remote_root"]
        runtime = self.filter_runtime_directory(node)
        return f"""[Unit]
Description=ARC loopback RPC policy filter {node['name']} ({self.manifest['rollout_id']})
After=network-online.target {self.late_fork_interlock_service_name(node)}
Wants=network-online.target
Requires={self.late_fork_interlock_service_name(node)}

[Service]
Type=simple
User={NGINX_FILTER_USER}
Group={CADDY_USER}
SupplementaryGroups={LATE_FORK_INTERLOCK_GROUP} {RPC_ORIGIN_GROUP}
UMask=0007
RuntimeDirectory={runtime}
RuntimeDirectoryMode=0750
RuntimeDirectoryPreserve=no
ExecStartPre={root}/arc-nginx-filter-preflight
ExecStart=/usr/sbin/nginx -c {root}/nginx-filter.conf -p {root}/rpc-filter-state
Restart=on-failure
RestartSec=2s
TimeoutStopSec=30s
KillSignal=SIGQUIT
NoNewPrivileges=true
PrivateDevices=true
PrivateTmp=true
ProtectClock=true
ProtectControlGroups=true
ProtectHome=true
ProtectKernelLogs=true
ProtectKernelModules=true
ProtectKernelTunables=true
ProtectProc=invisible
ProtectSystem=strict
ProcSubset=pid
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
RestrictNamespaces=true
RestrictRealtime=true
RestrictSUIDSGID=true
LockPersonality=true
RemoveIPC=true
CapabilityBoundingSet=
AmbientCapabilities=
ReadOnlyPaths=/usr/sbin/nginx {root}/nginx-filter.conf {root}/arc-nginx-filter-preflight
ReadWritePaths={root}/rpc-filter-state /run/{runtime}
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
"""

    def _stage_remote_archive_bytes(
        self,
        node: Mapping[str, Any],
        *,
        name: str,
        payload: bytes,
        expected_sha256: str,
    ) -> None:
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}", name):
            fail(f"unsafe legacy archive staging name: {name!r}")
        expected = bare_hash(expected_sha256, f"legacy archive {name} sha256")
        if sha256_bytes(payload) != expected:
            fail(f"in-memory legacy archive {name} differs before staging")
        root = self.legacy_archive_root(node)
        remote_temporary = self.ssh(
            node,
            r'''set -eu
root=$1 name=$2 digest=$3 user=$4
rollout_root=${root%/*}
base=${rollout_root%/*}
test -d /var && test ! -L /var
test -d /var/lib && test ! -L /var/lib
test -d "$base" && test ! -L "$base"
test -d "$rollout_root" && test ! -L "$rollout_root"
test -d "$root" && test ! -L "$root"
test "$(stat -c %U:%G:%a "$base")" = root:root:755
test "$(stat -c %U:%G:%a "$rollout_root")" = "root:$user:750"
test "$(stat -c %U:%G:%a "$root")" = "root:$user:750"
test -f "$root/.arc-recovery-rollout-owner" && test ! -L "$root/.arc-recovery-rollout-owner"
test "$(stat -c %U:%G:%a "$root/.arc-recovery-rollout-owner")" = "root:$user:440"
test "$(cat "$root/.arc-recovery-rollout-owner")" = "$digest"
for stale in "$root/.${name}.upload."*; do
  test ! -e "$stale" || {
    test -f "$stale" && test ! -L "$stale"
    test "$(stat -c %U "$stale")" = root
    rm -f -- "$stale"
  }
done
mktemp "$root/.${name}.upload.XXXXXX"
''',
            (root, name, self.digest, LEGACY_ARCHIVE_USER),
        ).strip()
        if not remote_temporary.startswith(root + "/.") or not SAFE_REMOTE_RE.fullmatch(
            remote_temporary
        ):
            fail(f"{node['name']} returned an unsafe archive upload path")
        with tempfile.TemporaryDirectory(prefix="arc-legacy-archive-upload-") as temporary:
            local = Path(temporary) / name
            _exclusive_write(local, payload, 0o400)
            self.scp(node, str(local), remote_temporary)
        self.ssh(
            node,
            "set -eu\n"
            + self.remote_semantic_python_prelude(node)
            + r'''temporary=$1 destination=$2 expected=$3 root=$4 digest=$5 user=$6
rollout_root=${root%/*}
base=${rollout_root%/*}
test -d /var && test ! -L /var
test -d /var/lib && test ! -L /var/lib
test -d "$base" && test ! -L "$base"
test -d "$rollout_root" && test ! -L "$rollout_root"
test -d "$root" && test ! -L "$root"
test "$(stat -c %U:%G:%a "$base")" = root:root:755
test "$(stat -c %U:%G:%a "$rollout_root")" = "root:$user:750"
test "$(stat -c %U:%G:%a "$root")" = "root:$user:750"
test -f "$root/.arc-recovery-rollout-owner" && test ! -L "$root/.arc-recovery-rollout-owner"
test "$(stat -c %U:%G:%a "$root/.arc-recovery-rollout-owner")" = "root:$user:440"
test "$(cat "$root/.arc-recovery-rollout-owner")" = "$digest"
test -f "$temporary" && test ! -L "$temporary"
printf '%s  %s\n' "$expected" "$temporary" | sha256sum --check --strict
chmod 0440 "$temporary"
chown root:"$user" "$temporary"
if test -e "$destination"; then
  test -f "$destination" && test ! -L "$destination"
  printf '%s  %s\n' "$expected" "$destination" | sha256sum --check --strict
  rm -f -- "$temporary"
else
  sync "$temporary"
  mv --no-clobber -T -- "$temporary" "$destination"
fi
test -f "$destination" && test ! -L "$destination"
printf '%s  %s\n' "$expected" "$destination" | sha256sum --check --strict
test "$(stat -c %U:%G:%a "$destination")" = "root:$user:440"
arc_semantic_python - "$root" "$digest" <<'PY'
import os,pathlib,sys
root=pathlib.Path(sys.argv[1]); marker=root/'.arc-recovery-rollout-owner'
if marker.read_text(encoding='ascii') != sys.argv[2]+'\n': raise SystemExit('archive owner marker differs')
for path in (root, marker, pathlib.Path(sys.argv[1]).parent):
    fd=os.open(path,os.O_RDONLY|getattr(os,'O_NOFOLLOW',0)|(getattr(os,'O_DIRECTORY',0) if path.is_dir() else 0))
    try: os.fsync(fd)
    finally: os.close(fd)
PY
arc_semantic_python_revalidate
''',
            (
                remote_temporary,
                f"{root}/{name}",
                expected,
                root,
                self.digest,
                LEGACY_ARCHIVE_USER,
            ),
        )

    def _stage_legacy_archive_node(self, node: Mapping[str, Any]) -> None:
        deployment = self.legacy_archive_for(node)
        if deployment is None:
            return
        if self.archive_manifest_payload is None or self.archive_complete_payload is None:
            fail("legacy archive metadata was not loaded before production staging")
        root = self.legacy_archive_root(node)
        self.ssh(
            node,
            "set -eu\n"
            + self.remote_semantic_python_prelude(node)
            + r'''root=$1 digest=$2 user=$3
if ! getent passwd "$user" >/dev/null; then
  useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin "$user"
fi
entry=$(getent passwd "$user")
uid=$(printf '%s' "$entry" | cut -d: -f3)
gid=$(printf '%s' "$entry" | cut -d: -f4)
home=$(printf '%s' "$entry" | cut -d: -f6)
shell=$(printf '%s' "$entry" | cut -d: -f7)
test "$uid" != 0
test "$gid" = "$(getent group "$user" | cut -d: -f3)"
test -z "$(getent group "$user" | cut -d: -f4)"
test "$(id -G "$user")" = "$gid"
test "$home" = /nonexistent
test "$shell" = /usr/sbin/nologin
base=/var/lib/arc-legacy-archive
rollout_root="$base/$digest"
test -d /var && test ! -L /var
test -d /var/lib && test ! -L /var/lib
if test -e "$base" || test -L "$base"; then
  test -d "$base" && test ! -L "$base"
else
  install -d -o root -g root -m 0755 "$base"
fi
test "$(stat -c %U:%G:%a "$base")" = root:root:755
if test -e "$rollout_root" || test -L "$rollout_root"; then
  test -d "$rollout_root" && test ! -L "$rollout_root"
else
  install -d -o root -g "$user" -m 0750 "$rollout_root"
fi
test "$(stat -c %U:%G:%a "$rollout_root")" = "root:$user:750"
if test -e "$root" || test -L "$root"; then
  test -d "$root" && test ! -L "$root"
  test -f "$root/.arc-recovery-rollout-owner" && test ! -L "$root/.arc-recovery-rollout-owner"
  test "$(cat "$root/.arc-recovery-rollout-owner")" = "$digest"
else
  temporary=$(mktemp -d "$rollout_root/.node.XXXXXX")
  printf '%s\n' "$digest" > "$temporary/.arc-recovery-rollout-owner"
  chmod 0440 "$temporary/.arc-recovery-rollout-owner"
  chown -R root:"$user" "$temporary"
  chmod 0750 "$temporary"
  mv --no-clobber -T -- "$temporary" "$root"
  test ! -e "$temporary"
fi
test "$(stat -c %U:%G:%a "$root")" = "root:$user:750"
test "$(stat -c %U:%G:%a "$root/.arc-recovery-rollout-owner")" = "root:$user:440"
arc_semantic_python - "$base" "$rollout_root" "$root" <<'PY'
import os,pathlib,stat,sys
paths=[pathlib.Path(value) for value in sys.argv[1:]]
paths.append(paths[-1]/'.arc-recovery-rollout-owner')
for path in paths:
    metadata=os.lstat(path)
    if stat.S_ISLNK(metadata.st_mode): raise SystemExit(f'symlink in archive staging path: {path}')
    flags=os.O_RDONLY|getattr(os,'O_NOFOLLOW',0)
    if stat.S_ISDIR(metadata.st_mode): flags |= getattr(os,'O_DIRECTORY',0)
    fd=os.open(path,flags)
    try: os.fsync(fd)
    finally: os.close(fd)
PY
arc_semantic_python_revalidate
''',
            (root, self.digest, LEGACY_ARCHIVE_USER),
        )
        archive = self.manifest["archive"]
        staged = (
            (
                "ARCHIVE-MANIFEST.json",
                self.archive_manifest_payload,
                archive["archive_manifest_sha256"],
            ),
            ("COMPLETE.json", self.archive_complete_payload, archive["complete_sha256"]),
            (
                f"legacy-{node['name']}.inventory",
                deployment["inventory_payload"],
                deployment["inventory_sha256"],
            ),
        )
        for name, payload, expected in staged:
            self._stage_remote_archive_bytes(
                node, name=name, payload=payload, expected_sha256=expected
            )

        self.ssh(
            node,
            "set -eu\n"
            + self.remote_semantic_python_prelude(node)
            + r'''root=$1 source_root=$2 expected_index=$3 binary=$4 binary_sha=$5 user=$6 digest=$7
rollout_root=${root%/*}
base=${rollout_root%/*}
test -d /var && test ! -L /var
test -d /var/lib && test ! -L /var/lib
test -d "$base" && test ! -L "$base"
test -d "$rollout_root" && test ! -L "$rollout_root"
test -d "$root" && test ! -L "$root"
test "$(stat -c %U:%G:%a "$base")" = root:root:755
test "$(stat -c %U:%G:%a "$rollout_root")" = "root:$user:750"
test "$(stat -c %U:%G:%a "$root")" = "root:$user:750"
test -f "$root/.arc-recovery-rollout-owner" && test ! -L "$root/.arc-recovery-rollout-owner"
test "$(stat -c %U:%G:%a "$root/.arc-recovery-rollout-owner")" = "root:$user:440"
test "$(cat "$root/.arc-recovery-rollout-owner")" = "$digest"
test -d "$source_root" && test ! -L "$source_root"
for source in binding.files.sha256 binding.json candidate.arcchkpt; do
  test -f "$source_root/$source" && test ! -L "$source_root/$source"
  test -z "$(find "$source_root/$source" -maxdepth 0 -perm /0222 -print -quit)"
done
printf '%s  %s\n' "$expected_index" "$source_root/binding.files.sha256" | sha256sum --check --strict
hashes=$(arc_semantic_python - "$source_root/binding.files.sha256" <<'PY'
import pathlib,re,sys
rows={}
for line in pathlib.Path(sys.argv[1]).read_text(encoding='utf-8').splitlines():
    match=re.fullmatch(r'([0-9a-f]{64})  ([A-Za-z0-9_.@/+:-]+)',line)
    if match is None or match.group(2) in rows: raise SystemExit('unsafe or duplicate binding index row')
    rows[match.group(2)]=match.group(1)
if 'binding.json' not in rows or 'candidate.arcchkpt' not in rows: raise SystemExit('binding index omits archive inputs')
print(rows['binding.json'],rows['candidate.arcchkpt'])
PY
)
set -- $hashes
binding_sha=$1 checkpoint_sha=$2
copy_pinned() {
  source=$1 destination=$2 expected=$3 mode=$4
  expected_mode=${mode#0}
  if test -e "$destination"; then
    test -f "$destination" && test ! -L "$destination"
    printf '%s  %s\n' "$expected" "$destination" | sha256sum --check --strict
    test "$(stat -c %U:%G:%a "$destination")" = "root:$user:$expected_mode"
    return
  fi
  temporary="$destination.partial"
  if test -e "$temporary"; then
    test -f "$temporary" && test ! -L "$temporary" || exit 1
    if ! printf '%s  %s\n' "$expected" "$temporary" | sha256sum --check --strict >/dev/null 2>&1; then
      rm -f -- "$temporary"
    fi
  fi
  if test ! -e "$temporary"; then cp --reflink=auto -- "$source" "$temporary"; fi
  printf '%s  %s\n' "$expected" "$temporary" | sha256sum --check --strict
  chmod "$mode" "$temporary"
  chown root:"$user" "$temporary"
  sync "$temporary"
  mv --no-clobber -T -- "$temporary" "$destination"
  test ! -e "$temporary"
  printf '%s  %s\n' "$expected" "$destination" | sha256sum --check --strict
  test "$(stat -c %U:%G:%a "$destination")" = "root:$user:$expected_mode"
}
copy_pinned "$source_root/binding.files.sha256" "$root/binding.files.sha256" "$expected_index" 0440
copy_pinned "$source_root/binding.json" "$root/binding.json" "$binding_sha" 0440
copy_pinned "$source_root/candidate.arcchkpt" "$root/candidate.arcchkpt" "$checkpoint_sha" 0440
copy_pinned "$binary" "$root/arc-node" "$binary_sha" 0550
find "$root" -maxdepth 1 -type l -print -quit | grep . && exit 1 || true
test "$(find "$root" -maxdepth 1 -type f -perm /0222 -print -quit)" = ""
arc_semantic_python - "$root" <<'PY'
import os,pathlib,sys
root=pathlib.Path(sys.argv[1])
for path in (root,root.parent):
    fd=os.open(path,os.O_RDONLY|getattr(os,'O_NOFOLLOW',0)|getattr(os,'O_DIRECTORY',0))
    try: os.fsync(fd)
    finally: os.close(fd)
PY
arc_semantic_python_revalidate
''',
            (
                root,
                f"/root/arc-recovery-bindings/{archive['prearchive_rollout_sha256']}/{node['name']}",
                deployment["binding_index_sha256"],
                f"{node['remote_root']}/arc-node",
                self.manifest["artifacts"]["binary"]["sha256"],
                LEGACY_ARCHIVE_USER,
                self.digest,
            ),
            timeout=3600,
        )
        self.say(
            f"PASS {node['name']} staged a hash-pinned local-disk legacy fork reader"
        )

    def _stage_remote_rollout_file(
        self,
        node: Mapping[str, Any],
        *,
        local: str,
        name: str,
        expected_sha256: str,
    ) -> None:
        """Create one rollout file or compare an existing same-rollout inode.

        SCP never targets the final path, so a resume cannot truncate a live
        validator, Caddy binary, config, or unit. The exact manifest/tool-bound
        digest decides whether a pre-existing final file is reusable.
        """
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}", name):
            fail(f"unsafe rollout staging name: {name!r}")
        expected = bare_hash(expected_sha256, f"staged rollout {name} sha256")
        local_path = Path(local)
        if not local_path.is_file() or local_path.is_symlink():
            fail(f"staged rollout source is missing, non-regular, or a symlink: {local}")
        if sha256_file(local_path) != expected:
            fail(f"staged rollout source changed before upload: {name}")
        root = node["remote_root"]
        remote_temporary = self.ssh(
            node,
            r'''set -eu
root=$1 name=$2 digest=$3
test -d "$root" && test ! -L "$root"
test -f "$root/.arc-recovery-rollout-owner" && test ! -L "$root/.arc-recovery-rollout-owner"
test "$(cat "$root/.arc-recovery-rollout-owner")" = "$digest"
for stale in "$root/.${name}.upload."*; do
  test ! -e "$stale" || {
    test -f "$stale" && test ! -L "$stale"
    test "$(stat -c %U "$stale")" = root
    rm -f -- "$stale"
  }
done
mktemp "$root/.${name}.upload.XXXXXX"
''',
            (root, name, self.digest),
        ).strip()
        if not remote_temporary.startswith(root + "/.") or not SAFE_REMOTE_RE.fullmatch(
            remote_temporary
        ):
            fail(f"{node['name']} returned an unsafe rollout upload path")
        self.scp(node, local, remote_temporary)
        self.ssh(
            node,
            "set -eu\n"
            + self.remote_semantic_python_prelude(node)
            + r'''temporary=$1 destination=$2 expected=$3 root=$4 digest=$5
test -f "$temporary" && test ! -L "$temporary"
test "$(cat "$root/.arc-recovery-rollout-owner")" = "$digest"
printf '%s  %s\n' "$expected" "$temporary" | sha256sum --check --strict
chmod 0400 "$temporary"
if test -e "$destination"; then
  test -f "$destination" && test ! -L "$destination"
  printf '%s  %s\n' "$expected" "$destination" | sha256sum --check --strict
  rm -f -- "$temporary"
else
  sync "$temporary"
  mv --no-clobber -T -- "$temporary" "$destination"
fi
test -f "$destination" && test ! -L "$destination"
printf '%s  %s\n' "$expected" "$destination" | sha256sum --check --strict
arc_semantic_python - "$root" <<'PY'
import os,sys
fd=os.open(sys.argv[1],os.O_RDONLY|getattr(os,'O_NOFOLLOW',0)|getattr(os,'O_DIRECTORY',0))
try: os.fsync(fd)
finally: os.close(fd)
PY
arc_semantic_python_revalidate
''',
            (
                remote_temporary,
                f"{root}/{name}",
                expected,
                root,
                self.digest,
            ),
        )

    def _stage_production_node(self, node: Mapping[str, Any]) -> None:
        root = node["remote_root"]
        self.ssh(
            node,
            r'''set -eu
umask 077
root=$1 digest=$2 parent=${1%/*}
test -d "$parent" && test ! -L "$parent"
if test -e "$root"; then
  test -d "$root" && test ! -L "$root"
  test -f "$root/.arc-recovery-rollout-owner" && test ! -L "$root/.arc-recovery-rollout-owner"
  test "$(cat "$root/.arc-recovery-rollout-owner")" = "$digest"
else
  temporary=$(mktemp -d "$parent/.arc-recovery-rollout.XXXXXX")
  printf '%s\n' "$digest" > "$temporary/.arc-recovery-rollout-owner"
  chmod 0400 "$temporary/.arc-recovery-rollout-owner"
  mv --no-clobber -T -- "$temporary" "$root"
  if test -e "$temporary"; then
    find "$temporary" -xdev -depth -delete
    test -d "$root" && test ! -L "$root"
    test "$(cat "$root/.arc-recovery-rollout-owner")" = "$digest"
  fi
fi
''',
            (root, self.digest),
        )
        self.ssh(
            node,
            r'''set -eu
root=$1
shift
if find "$root" -maxdepth 1 -type l -print -quit | grep . >/dev/null; then exit 1; fi
for name in arc-node genesis.toml recovery.arcchkpt legacy-validator-set-40m.json caddy Caddyfile.maintenance Caddyfile.live Caddyfile.active nginx-filter.conf arc-nginx-filter-preflight legacy-late-fork-source-set.json legacy-late-fork-source-set.json.sha256 legacy-late-fork-interlock.py legacy-late-fork-launch deployment-files.sha256 "$@"; do
  path="$root/$name"
  test ! -e "$path" || { test -f "$path" && test ! -L "$path"; }
done
''',
            (
                root,
                node["service_name"],
                self.gateway_service_name(node),
                self.filter_service_name(node),
                self.late_fork_interlock_service_name(node),
                *(
                    (self.legacy_archive_service_name(node),)
                    if self.legacy_archive_for(node) is not None
                    else ()
                ),
            ),
        )
        artifacts = self.manifest["artifacts"]
        for source, name, expected in (
            (artifacts["binary"]["path"], "arc-node", artifacts["binary"]["sha256"]),
            (artifacts["genesis"]["path"], "genesis.toml", artifacts["genesis"]["sha256"]),
            (
                artifacts["checkpoint"]["path"],
                "recovery.arcchkpt",
                artifacts["checkpoint"]["sha256"],
            ),
            (
                artifacts["legacy_validator_set"]["path"],
                "legacy-validator-set-40m.json",
                artifacts["legacy_validator_set"]["sha256"],
            ),
            (artifacts["caddy"]["path"], "caddy", artifacts["caddy"]["sha256"]),
            (
                artifacts["legacy_late_fork_source_set"]["path"],
                "legacy-late-fork-source-set.json",
                artifacts["legacy_late_fork_source_set"]["sha256"],
            ),
            (
                artifacts["legacy_late_fork_source_set_sidecar"]["path"],
                "legacy-late-fork-source-set.json.sha256",
                artifacts["legacy_late_fork_source_set_sidecar"]["sha256"],
            ),
            (
                artifacts["legacy_late_fork_interlock_tool"]["path"],
                "legacy-late-fork-interlock.py",
                artifacts["legacy_late_fork_interlock_tool"]["sha256"],
            ),
        ):
            self._stage_remote_rollout_file(
                node, local=source, name=name, expected_sha256=expected
            )
        self._stage_legacy_archive_node(node)
        with tempfile.TemporaryDirectory(prefix="arc-recovery-config-") as temporary:
            maintenance_caddy = self.maintenance_caddyfile(node)
            live_caddy = self.caddyfile(node)
            rpc_filter = self.nginx_filter(node)
            self.validate_gateway_security_contract(
                maintenance_caddy, live_caddy, rpc_filter
            )
            configs = {
                node["service_name"]: self.systemd_unit(node),
                self.gateway_service_name(node): self.gateway_unit(node),
                self.filter_service_name(node): self.filter_unit(node),
                self.late_fork_interlock_service_name(node): self.late_fork_interlock_unit(node),
                "legacy-late-fork-launch": self.late_fork_interlock_launcher(node),
                "Caddyfile.maintenance": maintenance_caddy,
                "Caddyfile.live": live_caddy,
                "nginx-filter.conf": rpc_filter,
                "arc-nginx-filter-preflight": self.filter_preflight(node),
            }
            if self.legacy_archive_for(node) is not None:
                configs[self.legacy_archive_service_name(node)] = self.legacy_archive_unit(
                    node
                )
            for name, contents in configs.items():
                path = Path(temporary) / name
                path.write_text(contents, encoding="utf-8")
                self._stage_remote_rollout_file(
                    node,
                    local=str(path),
                    name=name,
                    expected_sha256=sha256_file(path),
                )
            deployment_hashes = {
                "arc-node": artifacts["binary"]["sha256"],
                "genesis.toml": artifacts["genesis"]["sha256"],
                "recovery.arcchkpt": artifacts["checkpoint"]["sha256"],
                "legacy-validator-set-40m.json": artifacts["legacy_validator_set"]["sha256"],
                "caddy": artifacts["caddy"]["sha256"],
                "legacy-late-fork-source-set.json": artifacts[
                    "legacy_late_fork_source_set"
                ]["sha256"],
                "legacy-late-fork-source-set.json.sha256": artifacts[
                    "legacy_late_fork_source_set_sidecar"
                ]["sha256"],
                "legacy-late-fork-interlock.py": artifacts[
                    "legacy_late_fork_interlock_tool"
                ]["sha256"],
                **{
                    name: sha256_file(Path(temporary) / name)
                    for name in configs
                },
            }
            index = Path(temporary) / "deployment-files.sha256"
            index.write_text(
                "".join(f"{digest}  {name}\n" for name, digest in sorted(deployment_hashes.items())),
                encoding="ascii",
            )
            deployment_index_sha = sha256_file(index)
            self._stage_remote_rollout_file(
                node,
                local=str(index),
                name="deployment-files.sha256",
                expected_sha256=deployment_index_sha,
            )
        verify_script = (
            "set -eu\n"
            + self.remote_semantic_python_prelude(node)
            + r"""root=$1 binary_sha=$2 genesis_sha=$3 checkpoint_sha=$4 legacy_validators_sha=$5 caddy_sha=$6 model=$7 model_sha=$8 model_size=$9 digest=${10} deployment_index_sha=${11} caddy_version=${12} gateway_user=${13}
test -d "$root" && test ! -L "$root"
test "$(cat "$root/.arc-recovery-rollout-owner")" = "$digest"
test -f "$root/deployment-files.sha256" && test ! -L "$root/deployment-files.sha256"
printf '%s  %s/deployment-files.sha256\n' "$deployment_index_sha" "$root" | sha256sum --check --strict
if find "$root" -maxdepth 1 -type l -print -quit | grep . >/dev/null; then
  printf 'staged rollout root contains a symlink\n' >&2
  exit 1
fi
(cd "$root" && sha256sum --check --strict deployment-files.sha256)
printf '%s  %s/arc-node\n' "$binary_sha" "$root" | sha256sum --check --strict
printf '%s  %s/genesis.toml\n' "$genesis_sha" "$root" | sha256sum --check --strict
printf '%s  %s/recovery.arcchkpt\n' "$checkpoint_sha" "$root" | sha256sum --check --strict
printf '%s  %s/legacy-validator-set-40m.json\n' "$legacy_validators_sha" "$root" | sha256sum --check --strict
printf '%s  %s/caddy\n' "$caddy_sha" "$root" | sha256sum --check --strict
test -f "$model"
test ! -L "$model"
test "$(stat -c %s "$model")" = "$model_size"
printf '%s  %s\n' "$model_sha" "$model" | sha256sum --check --strict
chmod 0500 "$root/arc-node"
chmod 0400 "$root/genesis.toml" "$root/recovery.arcchkpt" "$root/legacy-validator-set-40m.json" "$root"/*.conf "$root"/*.service
if [ "$(stat -c %G "$root")" = "$gateway_user" ]; then
  # Same-rollout resume after gateway installation must not revoke the
  # dedicated unprivileged process's read/execute access.
  test "$(stat -c %U:%a "$root")" = root:750
  chown root:"$gateway_user" "$root/caddy" "$root/Caddyfile.maintenance" "$root/Caddyfile.live"
  chmod 0550 "$root/caddy"
  chmod 0440 "$root/Caddyfile.maintenance" "$root/Caddyfile.live"
  chown root:"$gateway_user" "$root/nginx-filter.conf" "$root/arc-nginx-filter-preflight"
  chmod 0440 "$root/nginx-filter.conf"
  chmod 0550 "$root/arc-nginx-filter-preflight"
else
  chmod 0500 "$root/caddy"
  chmod 0400 "$root/Caddyfile.maintenance" "$root/Caddyfile.live"
  chmod 0400 "$root/nginx-filter.conf" "$root/arc-nginx-filter-preflight"
fi
test "$("$root/caddy" version | awk '{print $1}')" = "$caddy_version"
"$root/caddy" validate --config "$root/Caddyfile.maintenance" --adapter caddyfile
"$root/caddy" validate --config "$root/Caddyfile.live" --adapter caddyfile
if test -e "$root/.arc-recovery-stage-complete"; then
  test -f "$root/.arc-recovery-stage-complete" && test ! -L "$root/.arc-recovery-stage-complete"
  test "$(cat "$root/.arc-recovery-stage-complete")" = "$digest"
else
  arc_semantic_python - "$root/.arc-recovery-stage-complete" "$digest" <<'PY'
import os,sys
fd=os.open(sys.argv[1],os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
with os.fdopen(fd,"w",encoding="ascii") as h: h.write(sys.argv[2]+"\n"); h.flush(); os.fsync(h.fileno())
PY
fi
arc_semantic_python_revalidate
"""
        )
        self.ssh(
            node,
            verify_script,
            (
                root,
                artifacts["binary"]["sha256"],
                artifacts["genesis"]["sha256"],
                artifacts["checkpoint"]["sha256"],
                artifacts["legacy_validator_set"]["sha256"],
                artifacts["caddy"]["sha256"],
                node["model_path"],
                node["model_sha256"],
                str(node["model_size_bytes"]),
                self.digest,
                deployment_index_sha,
                CADDY_VERSION,
                CADDY_USER,
            ),
        )
        remote_verify = self.recovery_cli("verify", node, remote=True)
        partial_data = f"{node['data_dir']}.arc-recovery-import-{self.digest}"
        remote_import = self.recovery_cli(
            "import", node, remote=True, data_dir_override=partial_data
        )
        self.ssh(node, 'set -eu\nexec "$@"\n', tuple(remote_verify), timeout=600)
        import_script = (
            "set -eu\n"
            + self.remote_semantic_python_prelude(node)
            + r'''data=$1 partial=$2 digest=$3 transition=$4 checkpoint_manifest=$5
owner="${partial}.owner"
marker_name=.arc-recovery-rollout.json
validate_marker() {
  arc_semantic_python - "$1/$marker_name" "$digest" "$transition" "$checkpoint_manifest" <<'PY'
import json,pathlib,sys
p=pathlib.Path(sys.argv[1]); v=json.loads(p.read_text(encoding="utf-8"))
e={"schema":"arc.recovery.import-complete.v1","rollout_manifest_sha256":sys.argv[2],"transition_height":int(sys.argv[3]),"checkpoint_manifest_hash":sys.argv[4]}
if p.is_symlink() or v != e or p.read_text(encoding="utf-8") != json.dumps(e,sort_keys=True,separators=(",",":"))+"\n": raise SystemExit("import completion marker differs")
PY
}
if test -e "$data"; then
  test -d "$data" && test ! -L "$data"
  validate_marker "$data"
  if test -e "$owner"; then test -f "$owner" && test ! -L "$owner" && test "$(cat "$owner")" = "$digest"; rm -f -- "$owner"; fi
  arc_semantic_python_revalidate
  exit 0
fi
resume_owned_partial=false
if test -e "$owner"; then
  test -f "$owner" && test ! -L "$owner" && test "$(cat "$owner")" = "$digest"
  resume_owned_partial=true
else
  test ! -e "$partial"
  arc_semantic_python - "$owner" "$digest" <<'PY'
import os,sys
fd=os.open(sys.argv[1],os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
with os.fdopen(fd,"w",encoding="ascii") as h: h.write(sys.argv[2]+"\n"); h.flush(); os.fsync(h.fileno())
PY
fi
if test -e "$partial"; then
  test "$resume_owned_partial" = true
  test -d "$partial" && test ! -L "$partial"
  if test -f "$partial/$marker_name" && test ! -L "$partial/$marker_name"; then
    validate_marker "$partial"
    mv --no-clobber -T -- "$partial" "$data"
    test ! -e "$partial"
    validate_marker "$data"
    rm -f -- "$owner"
    arc_semantic_python_revalidate
    exit 0
  fi
  find "$partial" -xdev -depth -delete
fi
shift 5
summary=$("$@")
arc_semantic_python - "$summary" "$transition" "$checkpoint_manifest" <<'PY'
import json,sys
v=json.loads(sys.argv[1]); h=str(v.get("manifest_hash","")).removeprefix("0x")
if v.get("status")!="ACTIVATED" or v.get("height")!=int(sys.argv[2]) or h!=sys.argv[3]: raise SystemExit("recovery import did not activate the exact approved H+1 checkpoint")
PY
arc_semantic_python - "$partial/$marker_name" "$digest" "$transition" "$checkpoint_manifest" <<'PY'
import json,os,sys
p=sys.argv[1]; v={"schema":"arc.recovery.import-complete.v1","rollout_manifest_sha256":sys.argv[2],"transition_height":int(sys.argv[3]),"checkpoint_manifest_hash":sys.argv[4]}; b=(json.dumps(v,sort_keys=True,separators=(",",":"))+"\n").encode(); fd=os.open(p,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
with os.fdopen(fd,"wb") as h: h.write(b); h.flush(); os.fsync(h.fileno())
PY
mv --no-clobber -T -- "$partial" "$data"
test ! -e "$partial"
validate_marker "$data"
rm -f -- "$owner"
arc_semantic_python_revalidate
'''
        )
        self.ssh(
            node,
            import_script,
            (
                node["data_dir"],
                partial_data,
                self.digest,
                str(self.chain["transition_height"]),
                bare_hash(self.chain["approved_checkpoint_manifest_hash"], "checkpoint manifest hash"),
                *remote_import,
            ),
            timeout=600,
        )
        self.say(f"PASS {node['name']} staged exact artifacts and imported checkpoint into fresh data")

    def _install_late_fork_interlock(self, node: Mapping[str, Any]) -> None:
        """Install and prove the capture-bound fail-closed publication monitor."""
        root = node["remote_root"]
        monitor_root = self.late_fork_interlock_root(node)
        service = self.late_fork_interlock_service_name(node)
        interpreter = self.late_fork_interlock_interpreter(node)
        source_sha = self.chain["legacy_late_fork_source_set_sha256"]
        sidecar_sha = self.manifest["artifacts"][
            "legacy_late_fork_source_set_sidecar"
        ]["sha256"]
        tool_sha = self.manifest["artifacts"]["legacy_late_fork_interlock_tool"][
            "sha256"
        ]
        launcher_sha = sha256_bytes(
            self.late_fork_interlock_launcher(node).encode("utf-8")
        )
        unit_sha = sha256_bytes(self.late_fork_interlock_unit(node).encode("utf-8"))
        output = self.ssh(
            node,
            r'''set -eu
stage=$1 root=$2 service=$3 user=$4 rollout=$5 source_sha=$6 sidecar_sha=$7
tool_sha=$8 launcher_sha=$9 unit_sha=${10} python=${11} python_sha=${12}
python_device=${13} python_inode=${14} gate_group=${15} gate_socket=${16}
runtime_name=${17} filter_user=${18}
test -d "$stage" && test ! -L "$stage"
test "$(cat "$stage/.arc-recovery-rollout-owner")" = "$rollout"
for tuple in \
  "$source_sha:legacy-late-fork-source-set.json" \
  "$sidecar_sha:legacy-late-fork-source-set.json.sha256" \
  "$tool_sha:legacy-late-fork-interlock.py" \
  "$launcher_sha:legacy-late-fork-launch" \
  "$unit_sha:$service"; do
  expected=${tuple%%:*}; name=${tuple#*:}; path="$stage/$name"
  test -f "$path" && test ! -L "$path"
  test "$(stat -c %U:%G:%a:%h "$path")" = root:root:400:1
  printf '%s  %s\n' "$expected" "$path" | sha256sum --check --strict
done
test "$(cat "$stage/legacy-late-fork-source-set.json.sha256")" = \
  "$source_sha  legacy-late-fork-source-set.json"
test -f "$python" && test ! -L "$python"
test "$(stat -c %d:%i:%U:%G:%a:%h "$python")" = \
  "$python_device:$python_inode:root:root:755:1"
printf '%s  %s\n' "$python_sha" "$python" | sha256sum --check --strict
if ! getent group "$gate_group" >/dev/null; then
  groupadd --system "$gate_group"
fi
gate_gid=$(getent group "$gate_group" | cut -d: -f3)
case "$gate_gid" in ''|0|*[!0-9]*) exit 1 ;; esac
if ! getent passwd "$user" >/dev/null; then
  useradd --system --gid "$gate_group" --no-create-home --home-dir /nonexistent \
    --shell /usr/sbin/nologin "$user"
fi
entry=$(getent passwd "$user"); uid=$(printf '%s' "$entry" | cut -d: -f3)
gid=$(printf '%s' "$entry" | cut -d: -f4)
test "$uid" != 0
test "$gid" = "$gate_gid"
test "$(id -G "$user")" = "$gid"
test "$(printf '%s' "$entry" | cut -d: -f6)" = /nonexistent
test "$(printf '%s' "$entry" | cut -d: -f7)" = /usr/sbin/nologin
test "$(getent passwd | awk -F: -v gid="$gate_gid" '$4 == gid {print $1}')" = "$user"
gate_members=$(getent group "$gate_group" | cut -d: -f4)
case "$gate_members" in ''|"$filter_user") ;; *) exit 1 ;; esac
base=/var/lib/arc-late-fork-interlock
parent=${root%/*}
for directory in "$base" "$parent"; do
  if test -e "$directory"; then test -d "$directory" && test ! -L "$directory"
  else mkdir --mode=0755 "$directory"; fi
done
if test -e "$root"; then
  test -d "$root" && test ! -L "$root"
else
  mkdir --mode=0750 "$root"
fi
chown root:"$gate_group" "$parent" "$root"
chmod 0750 "$parent" "$root"
owner="$root/.arc-recovery-rollout-owner"
if test -e "$owner"; then
  test -f "$owner" && test ! -L "$owner"
  test "$(cat "$owner")" = "$rollout"
else
  temporary=$(mktemp "$root/.owner.XXXXXX")
  printf '%s\n' "$rollout" > "$temporary"; chown root:"$gate_group" "$temporary"; chmod 0440 "$temporary"
  sync "$temporary"; mv --no-clobber -T -- "$temporary" "$owner"
  test ! -e "$temporary"
  /usr/bin/sync "$root"
fi
copy_exact() {
  source=$1 destination=$2 expected=$3 mode=$4
  if test -e "$destination"; then
    test -f "$destination" && test ! -L "$destination"
    printf '%s  %s\n' "$expected" "$destination" | sha256sum --check --strict
  else
    temporary=$(mktemp "$root/.install.XXXXXX")
    cp -- "$source" "$temporary"; chown root:"$gate_group" "$temporary"; chmod "$mode" "$temporary"
    printf '%s  %s\n' "$expected" "$temporary" | sha256sum --check --strict
    sync "$temporary"; mv --no-clobber -T -- "$temporary" "$destination"
    test ! -e "$temporary"; /usr/bin/sync "$root"
  fi
  test "$(stat -c %U:%G:%a:%h "$destination")" = "root:$gate_group:$mode:1"
}
copy_exact "$stage/legacy-late-fork-source-set.json" "$root/legacy-late-fork-source-set.json" "$source_sha" 440
copy_exact "$stage/legacy-late-fork-source-set.json.sha256" "$root/legacy-late-fork-source-set.json.sha256" "$sidecar_sha" 440
copy_exact "$stage/legacy-late-fork-interlock.py" "$root/legacy-late-fork-interlock.py" "$tool_sha" 440
copy_exact "$stage/legacy-late-fork-launch" "$root/launch" "$launcher_sha" 550
if test -e "$root/state"; then
  test -d "$root/state" && test ! -L "$root/state"
else
  install -d -o "$user" -g "$gate_group" -m 0700 "$root/state"
fi
chown "$user:$gate_group" "$root/state"; chmod 0700 "$root/state"
installed="/etc/systemd/system/$service"
for base in /etc/systemd/system /run/systemd/system /usr/local/lib/systemd/system /usr/lib/systemd/system /lib/systemd/system; do
  dropins="$base/$service.d"
  if test -e "$dropins" || test -L "$dropins"; then
    test -d "$dropins" && test ! -L "$dropins"
    test "$(stat -c %U:%G:%a "$dropins")" = root:root:755
    test -z "$(find "$dropins" -mindepth 1 -maxdepth 1 -print -quit)"
  fi
done
if test -e "$installed"; then
  test -f "$installed" && test ! -L "$installed"
  cmp --silent "$stage/$service" "$installed"
else
  cp --no-clobber "$stage/$service" "$installed"
fi
chown root:root "$installed"; chmod 0644 "$installed"
test "$(stat -c %U:%G:%a:%h "$installed")" = root:root:644:1
printf '%s  %s\n' "$unit_sha" "$installed" | sha256sum --check --strict
systemctl daemon-reload
test "$(systemctl show "$service" --property=FragmentPath --value)" = "$installed"
test -z "$(systemctl show "$service" --property=DropInPaths --value)"
test "$(systemctl show "$service" --property=User --value)" = "$user"
test "$(systemctl show "$service" --property=Group --value)" = "$gate_group"
test "$(systemctl show "$service" --property=NoNewPrivileges --value)" = yes
test "$(systemctl show "$service" --property=ProtectSystem --value)" = strict
test "$(systemctl show "$service" --property=ProtectHome --value)" = yes
test "$(systemctl show "$service" --property=RuntimeDirectory --value)" = "$runtime_name"
systemctl enable "$service"
systemctl restart "$service"
test "$(systemctl show "$service" --property=FragmentPath --value)" = "$installed"
test -z "$(systemctl show "$service" --property=DropInPaths --value)"
test "$(systemctl show "$service" --property=User --value)" = "$user"
test "$(systemctl show "$service" --property=Group --value)" = "$gate_group"
pid=$(systemctl show "$service" --property=MainPID --value)
case "$pid" in ''|0|*[!0-9]*) exit 1 ;; esac
test "$(awk '/^Uid:/{print $2}' "/proc/$pid/status")" = "$uid"
test "$(readlink "/proc/$pid/exe")" = "$python"
printf '%s  %s\n' "$python_sha" "/proc/$pid/exe" | sha256sum --check --strict
runtime="/run/$runtime_name"
test -d "$runtime" && test ! -L "$runtime"
test "$(stat -c %U:%G:%a "$runtime")" = "$user:$gate_group:750"
gate_ready=false
for _ in $(seq 1 60); do
  if test -S "$gate_socket" && test ! -L "$gate_socket"; then
    gate_ready=true
    break
  fi
  systemctl is-active --quiet "$service" || exit 1
  sleep 1
done
test "$gate_ready" = true
test "$(stat -c %U:%G:%a:%h "$gate_socket")" = "$user:$gate_group:660:1"
rows=$(ss -H -lxnp | grep -F -- " $gate_socket" || true)
test "$(printf '%s\n' "$rows" | awk 'NF {n+=1} END {print n+0}')" = 1
case "$rows" in *"pid=$pid,"*) ;; *) exit 1 ;; esac
test -z "$(ss -H -ltnp | awk '$4 ~ /:18081$/ {print}')"
body=''
for _ in $(seq 1 60); do
  code=$(curl --silent --show-error --output /dev/null --write-out '%{http_code}' \
    --unix-socket "$gate_socket" --connect-timeout 2 --max-time 5 \
    http://localhost/gate || true)
  if test "$code" = 204; then
    body=$(curl --silent --show-error --fail --connect-timeout 2 --max-time 5 \
      --unix-socket "$gate_socket" http://localhost/maintenance/status)
    break
  fi
  sleep 1
done
test -n "$body" || { journalctl --no-pager -u "$service" -n 80 >&2 || true; exit 1; }
printf '%s\n' "$body"
''',
            (
                root,
                monitor_root,
                service,
                LATE_FORK_INTERLOCK_USER,
                self.digest,
                source_sha,
                sidecar_sha,
                tool_sha,
                launcher_sha,
                unit_sha,
                interpreter["normalized_path"],
                interpreter["sha256"],
                str(interpreter["device"]),
                str(interpreter["inode"]),
                LATE_FORK_INTERLOCK_GROUP,
                self.late_fork_interlock_socket(node),
                self.late_fork_interlock_runtime_directory(node),
                NGINX_FILTER_USER,
            ),
            timeout=300,
        )
        self._validate_late_fork_status(output.encode("utf-8"), require_healthy=True)
        self.say(
            f"PASS {node['name']} late-fork interlock is hash-pinned, fresh, and fail-closed"
        )

    def _retire_legacy_network_quarantine(
        self, node: Mapping[str, Any]
    ) -> dict[str, Any]:
        """Cross the capture-bound one-way network handoff on one host.

        ``archive-node.sh`` owns the destructive details and its durable
        remote journal.  This wrapper re-hashes that exact helper immediately
        before execution, accepts only the canonical receipt, and appends the
        receipt separately to the operator rollback journal.  The retained
        legacy start barriers are deliberately distinct from the retired nft
        quarantine: they continue to make a v0.7 restart impossible while
        freeing the public/QUIC ports required by Caddy and v3.
        """

        archive = self.manifest["archive"]
        helper_sha = archive["remote_helper_sha256"]
        helper = f"/root/.arc-recovery-helpers/{helper_sha}/archive-node.sh"
        output = self.ssh(
            node,
            r'''set -eu
helper=$1 helper_sha=$2 capture=$3 node=$4 freeze=$5 rollout=$6 archive_manifest=$7
boundary=$8 bundle=$9
test -f "$helper" && test ! -L "$helper"
test "$(stat -c %U:%G:%a:%h "$helper")" = root:root:500:1
printf '%s  %s\n' "$helper_sha" "$helper" | sha256sum --check --strict
exec "$helper" quarantine-retire "$capture" "$node" "$freeze" "$rollout" \
  "$archive_manifest" "$boundary" "$bundle"
''',
            (
                helper,
                helper_sha,
                archive["capture_id"],
                node["name"],
                archive["freeze_plan_sha256"],
                self.digest,
                archive["archive_manifest_sha256"],
                self.chain["legacy_maintenance_boundary_sha256"],
                self.chain["legacy_maintenance_evidence_bundle_sha256"],
            ),
            timeout=300,
        )
        try:
            receipt = require_keys(
                json.loads(output),
                f"{node['name']} legacy quarantine retirement receipt",
                (
                    "schema", "capture_id", "node", "freeze_plan_sha256",
                    "rollout_manifest_sha256", "archive_manifest_sha256",
                    "legacy_maintenance_boundary_sha256",
                    "legacy_maintenance_evidence_bundle_sha256",
                    "network_quarantine_receipt_sha256",
                    "network_quarantine_monitor_sha256",
                    "quarantine_restart_arm_sha256",
                    "quarantine_restart_commit_sha256", "intent_sha256",
                    "preexisting_firewall_structural_sha256",
                    "owned_ruleset_stateless_sha256", "pinned_nft_sha256",
                    "legacy_start_barriers_sha256",
                    "quarantine_arm_barriers_sha256",
                    "nginx_retirement_barrier_sha256", "phases",
                    "table_absent", "fence_service_active",
                    "fence_service_enabled", "fence_dependencies_removed",
                    "legacy_start_barrier_active", "nginx_retired",
                    "automatic_legacy_restart", "rollback_policy",
                    "completed_at",
                ),
            )
        except json.JSONDecodeError as error:
            fail(
                f"{node['name']} legacy quarantine retirement receipt is invalid JSON: {error}"
            )
        expected = {
            "schema": "arc.recovery.legacy-network-quarantine-retirement.v1",
            "capture_id": archive["capture_id"],
            "node": node["name"],
            "freeze_plan_sha256": archive["freeze_plan_sha256"],
            "rollout_manifest_sha256": self.digest,
            "archive_manifest_sha256": archive["archive_manifest_sha256"],
            "legacy_maintenance_boundary_sha256": self.chain[
                "legacy_maintenance_boundary_sha256"
            ],
            "legacy_maintenance_evidence_bundle_sha256": self.chain[
                "legacy_maintenance_evidence_bundle_sha256"
            ],
            "table_absent": True,
            "fence_service_active": False,
            "fence_service_enabled": False,
            "fence_dependencies_removed": True,
            "legacy_start_barrier_active": True,
            "nginx_retired": True,
            "automatic_legacy_restart": False,
            "rollback_policy": "maintenance-only-no-legacy-restart",
        }
        if any(receipt.get(field) != wanted for field, wanted in expected.items()):
            fail(f"{node['name']} legacy quarantine retirement identity/policy differs")
        for field in (
            "network_quarantine_receipt_sha256",
            "network_quarantine_monitor_sha256",
            "quarantine_restart_arm_sha256",
            "quarantine_restart_commit_sha256",
            "intent_sha256",
            "preexisting_firewall_structural_sha256",
            "owned_ruleset_stateless_sha256",
            "pinned_nft_sha256",
            "nginx_retirement_barrier_sha256",
        ):
            bare_hash(receipt[field], f"{node['name']} retirement {field}")
        for field in (
            "legacy_start_barriers_sha256",
            "quarantine_arm_barriers_sha256",
        ):
            values = receipt[field]
            expected_units = {
                "arc-self-heal.service",
                "arc-node.service",
                "arc-node-update.service",
                "arc-node-update.timer",
            }
            if not isinstance(values, dict) or set(values) != expected_units:
                fail(f"{node['name']} retirement {field} topology differs")
            for unit, digest in values.items():
                bare_hash(digest, f"{node['name']} retirement {field}.{unit}")
        phases = receipt["phases"]
        expected_phases = (
            "legacy-public-retired",
            "fence-service-retired",
            "fence-dependencies-removed",
            "owned-table-removed",
        )
        if not isinstance(phases, list) or [row.get("phase") for row in phases] != list(
            expected_phases
        ):
            fail(f"{node['name']} retirement phase ledger differs")
        for index, row in enumerate(phases):
            if set(row) != {"phase", "receipt_sha256"}:
                fail(f"{node['name']} retirement phase {index} is inexact")
            bare_hash(
                row["receipt_sha256"],
                f"{node['name']} retirement phase {index} receipt",
            )
        if not isinstance(receipt["completed_at"], str) or re.fullmatch(
            r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z",
            receipt["completed_at"],
        ) is None:
            fail(f"{node['name']} retirement completion is not canonical UTC")
        if output.encode("utf-8") != canonical_bytes(receipt):
            fail(f"{node['name']} legacy quarantine retirement is noncanonical")
        self.production_quarantine_retired.add(node["name"])
        self.say(
            f"PASS {node['name']} retired only the owned quarantine; legacy restart remains impossible"
        )
        return receipt

    def _install_gateway_and_unit(
        self, node: Mapping[str, Any], *, retirement_committed: bool = False
    ) -> dict[str, Any]:
        root = node["remote_root"]
        effective_specs = [
            (node["service_name"], self.systemd_unit(node), node["service_user"], RPC_ORIGIN_GROUP, ""),
            (self.gateway_service_name(node), self.gateway_unit(node), CADDY_USER, CADDY_USER, ""),
            (
                self.filter_service_name(node),
                self.filter_unit(node),
                NGINX_FILTER_USER,
                CADDY_USER,
                f"{LATE_FORK_INTERLOCK_GROUP} {RPC_ORIGIN_GROUP}",
            ),
            (
                self.late_fork_interlock_service_name(node),
                self.late_fork_interlock_unit(node),
                LATE_FORK_INTERLOCK_USER,
                LATE_FORK_INTERLOCK_GROUP,
                "",
            ),
        ]
        if self.legacy_archive_for(node) is not None:
            effective_specs.append(
                (
                    self.legacy_archive_service_name(node),
                    self.legacy_archive_unit(node),
                    LEGACY_ARCHIVE_USER,
                    RPC_ORIGIN_GROUP,
                    LEGACY_ARCHIVE_USER,
                )
            )
        effective_lines: list[str] = []
        for unit_name, unit_text, unit_user, unit_group, supplementary_groups in effective_specs:
            exec_starts = [
                line.removeprefix("ExecStart=").split(" ", 1)[0]
                for line in unit_text.splitlines()
                if line.startswith("ExecStart=")
            ]
            if len(exec_starts) != 1 or not exec_starts[0]:
                fail(f"{unit_name} has an inexact ExecStart for the effective-unit receipt")
            effective_lines.extend(
                (
                    f"[{unit_name}]",
                    f"FragmentPath=/etc/systemd/system/{unit_name}",
                    "DropInPaths=",
                    f"UnitSHA256={sha256_bytes(unit_text.encode('utf-8'))}",
                    f"User={unit_user}",
                    f"Group={unit_group}",
                    f"SupplementaryGroups={supplementary_groups}",
                    f"ExecStartPath={exec_starts[0]}",
                )
            )
        effective_lines.extend(
            (
                f"FilterExecStartPrePath={root}/arc-nginx-filter-preflight",
                "FilterNoNewPrivileges=yes",
                "FilterProtectSystem=strict",
                "FilterProtectHome=yes",
            )
        )
        expected_effective_systemd_inventory_sha = sha256_bytes(
            ("\n".join(effective_lines) + "\n").encode("utf-8")
        )
        baseline = self.production_service_baseline.get(
            node["name"],
            {
                f"{service}_{state}": False
                for service in ("validator", "gateway", "filter", "interlock", "archive", "nginx")
                for state in ("active", "enabled")
            },
        )
        gateway_script = r"""
set -eu
root=$1 hostname=$2 service=$3 gateway_service=$4 filter_service=$5 digest=$6 archive_service=$7
validator_was_active=$8 validator_was_enabled=$9 gateway_was_active=${10} gateway_was_enabled=${11}
filter_was_active=${12} filter_was_enabled=${13} archive_was_active=${14} archive_was_enabled=${15}
nginx_was_active=${16} nginx_was_enabled=${17}
maintenance_sha=${18} live_sha=${19} gateway_user=${20} interlock_service=${21}
interlock_was_active=${22} interlock_was_enabled=${23}
retirement_committed=${24}
capture_id=${25} node_name=${26}
nginx_version=${27} nginx_sha=${28}
filter_user=${29} filter_config_sha=${30} filter_unit_sha=${31} filter_preflight_sha=${32}
validator_probe_ip=${33}
public_filter_socket=${34} archive_filter_socket=${35} attacker_user=${36}
validator_user=${37} interlock_user=${38} archive_user=${39} interlock_group=${40}
expected_effective_systemd_inventory_sha=${41}
origin_group=${42} interlock_socket=${43}
for flag in "$validator_was_active" "$validator_was_enabled" "$gateway_was_active" "$gateway_was_enabled" "$filter_was_active" "$filter_was_enabled" "$archive_was_active" "$archive_was_enabled" "$nginx_was_active" "$nginx_was_enabled" "$interlock_was_active" "$interlock_was_enabled" "$retirement_committed"; do
  case "$flag" in 0|1) ;; *) exit 1 ;; esac
done
export DEBIAN_FRONTEND=noninteractive
command -v runuser >/dev/null
test -d "$root" && test ! -L "$root"
test -f "$root/.arc-recovery-stage-complete" && test ! -L "$root/.arc-recovery-stage-complete"
test ! -e /etc/arc-recovery/legacy-start-allowed
for retired in arc-self-heal.service arc-node.service arc-node-update.service; do
  fence="/etc/systemd/system/$retired.d/zzzz-arc-recovery-freeze.conf"
  test -f "$fence" && test ! -L "$fence"
  test "$(stat -c %U:%G:%a:%h "$fence")" = root:root:444:1
  test "$(cat "$fence")" = "[Unit]
ConditionPathExists=/etc/arc-recovery/legacy-start-allowed"
  arm="/etc/systemd/system/$retired.d/zzzx-arc-recovery-quarantine-arm.conf"
  test -f "$arm" && test ! -L "$arm"
  test "$(stat -c %U:%G:%a:%h "$arm")" = root:root:444:1
  test "$(cat "$arm")" = "[Unit]
ConditionPathExists=!/root/arc-recovery-stops/$capture_id/.$node_name.stop.partial/09-quarantine-restart-arm.json
ConditionPathExists=!/root/arc-recovery-stops/$capture_id/$node_name/09-quarantine-restart-arm.json"
  ! systemctl is-active --quiet "$retired"
done
if [ "$retirement_committed" = 1 ]; then
  ! /usr/sbin/nft list table inet arc_legacy_maintenance_v1 >/dev/null 2>&1
  ! systemctl is-active --quiet arc-legacy-maintenance-fence.service
  ! systemctl is-enabled --quiet arc-legacy-maintenance-fence.service
fi
inventory="$root/pre-gateway.inventory"
if test -e "$inventory"; then
  test -f "$inventory" && test ! -L "$inventory"
  test "$(grep -c '^rollout_manifest_sha256=' "$inventory")" = 1
  grep -Fxq "rollout_manifest_sha256=$digest" "$inventory"
  test "$(grep -c '^nginx_active=' "$inventory")" = 1
  test "$(grep -c '^nginx_enabled=' "$inventory")" = 1
  test "$(grep -c '^public_80_count=' "$inventory")" = 1
  test "$(grep -c '^public_443_count=' "$inventory")" = 1
else
  temporary=$(mktemp "$root/.pre-gateway.XXXXXX")
  public_rows=$(ss -H -ltnp | awk '$4 ~ /:(80|443)$/ { print }')
  {
    printf 'captured_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'rollout_manifest_sha256=%s\n' "$digest"
    printf 'nginx_active=%s\n' "$(systemctl is-active nginx.service 2>/dev/null || true)"
    printf 'nginx_enabled=%s\n' "$(systemctl is-enabled nginx.service 2>/dev/null || true)"
    printf 'public_80_count=%s\n' "$(printf '%s\n' "$public_rows" | awk '$4 ~ /:80$/ { count += 1 } END { print count + 0 }')"
    printf 'public_443_count=%s\n' "$(printf '%s\n' "$public_rows" | awk '$4 ~ /:443$/ { count += 1 } END { print count + 0 }')"
    printf '%s\n' "$public_rows"
  } > "$temporary"
  chmod 0400 "$temporary"
  mv --no-clobber -T -- "$temporary" "$inventory"
  if test -e "$temporary"; then rm -f -- "$temporary"; fi
  test -f "$inventory" && test ! -L "$inventory"
  grep -Fxq "rollout_manifest_sha256=$digest" "$inventory"
fi
restore_unit() {
  unit=$1 was_active=$2 was_enabled=$3
  if [ "$was_enabled" = 1 ]; then systemctl enable "$unit" 2>/dev/null || true; else systemctl disable "$unit" 2>/dev/null || true; fi
  if [ "$was_active" = 1 ]; then systemctl start "$unit" 2>/dev/null || true; else systemctl stop "$unit" 2>/dev/null || true; fi
}
rollback_on_error() {
  status=$?
  if [ "$status" -ne 0 ]; then
    if [ "$retirement_committed" = 1 ]; then
      # The owned nft quarantine has crossed its one-way retirement boundary.
      # Keep v3 and legacy nginx closed; the outer aggregate rollback resumes
      # this installer until the exact maintenance Caddy edge is active.
      systemctl stop "$service" 2>/dev/null || true
      systemctl disable "$service" 2>/dev/null || true
      systemctl stop nginx.service 2>/dev/null || true
      systemctl disable nginx.service 2>/dev/null || true
    else
      if [ "$archive_service" != none ]; then restore_unit "$archive_service" "$archive_was_active" "$archive_was_enabled"; fi
      restore_unit "$filter_service" "$filter_was_active" "$filter_was_enabled"
      restore_unit "$gateway_service" "$gateway_was_active" "$gateway_was_enabled"
      restore_unit "$service" "$validator_was_active" "$validator_was_enabled"
      restore_unit "$interlock_service" "$interlock_was_active" "$interlock_was_enabled"
      restore_unit nginx.service "$nginx_was_active" "$nginx_was_enabled"
    fi
  fi
  exit "$status"
}
trap rollback_on_error EXIT
candidate=$(/usr/bin/apt-cache policy nginx | /usr/bin/awk '/Candidate:/{print $2; exit}')
test "$candidate" = "$nginx_version"
if ! command -v nginx >/dev/null 2>&1; then
  apt-get update >&2
  candidate=$(/usr/bin/apt-cache policy nginx | /usr/bin/awk '/Candidate:/{print $2; exit}')
  test "$candidate" = "$nginx_version"
  apt-get install -y --no-install-recommends "nginx=$nginx_version" ca-certificates curl >&2
fi
test "$(/usr/bin/dpkg-query -W -f='${Version}' nginx)" = "$nginx_version"
test -f /usr/sbin/nginx && test ! -L /usr/sbin/nginx
test "$(stat -c %U:%G:%a:%h /usr/sbin/nginx)" = root:root:755:1
printf '%s  /usr/sbin/nginx\n' "$nginx_sha" | sha256sum --check --strict
/usr/sbin/nginx -V 2>&1 | grep -Fq -- '--with-http_auth_request_module'
if /usr/bin/apt-mark showhold | /usr/bin/grep -Fxq nginx; then
  printf 'nginx package is held; review and explicitly unhold before the recovery rollout\n' >&2
  exit 1
fi
if ! getent passwd "$gateway_user" >/dev/null; then
  useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin "$gateway_user"
fi
gateway_entry=$(getent passwd "$gateway_user")
gateway_uid=$(printf '%s' "$gateway_entry" | cut -d: -f3)
gateway_gid=$(printf '%s' "$gateway_entry" | cut -d: -f4)
test "$gateway_uid" != 0
test "$gateway_gid" = "$(getent group "$gateway_user" | cut -d: -f3)"
test -z "$(getent group "$gateway_user" | cut -d: -f4)"
test "$(id -G "$gateway_user")" = "$gateway_gid"
test "$(printf '%s' "$gateway_entry" | cut -d: -f6)" = /nonexistent
test "$(printf '%s' "$gateway_entry" | cut -d: -f7)" = /usr/sbin/nologin
for group in "$interlock_group" "$origin_group"; do
  if ! getent group "$group" >/dev/null; then groupadd --system "$group"; fi
  gid=$(getent group "$group" | cut -d: -f3)
  case "$gid" in ''|0|*[!0-9]*) exit 1 ;; esac
done
interlock_gid=$(getent group "$interlock_group" | cut -d: -f3)
origin_gid=$(getent group "$origin_group" | cut -d: -f3)
test "$(getent passwd | awk -F: -v gid="$interlock_gid" '$4 == gid {print $1}')" = "$interlock_user"
test -z "$(getent passwd | awk -F: -v gid="$origin_gid" '$4 == gid {print $1}')"
if ! getent passwd "$filter_user" >/dev/null; then
  useradd --system --gid "$gateway_user" --groups "$interlock_group,$origin_group" \
    --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin "$filter_user"
fi
filter_entry=$(getent passwd "$filter_user")
filter_uid=$(printf '%s' "$filter_entry" | cut -d: -f3)
filter_gid=$(printf '%s' "$filter_entry" | cut -d: -f4)
test "$filter_uid" != 0
test "$filter_gid" = "$gateway_gid"
expected_filter_gids=$(printf '%s\n%s\n%s\n' "$gateway_gid" "$interlock_gid" "$origin_gid" | LC_ALL=C sort -n | xargs)
actual_filter_gids=$(id -G "$filter_user" | tr ' ' '\n' | LC_ALL=C sort -n | xargs)
test "$actual_filter_gids" = "$expected_filter_gids"
test "$(printf '%s' "$filter_entry" | cut -d: -f6)" = /nonexistent
test "$(printf '%s' "$filter_entry" | cut -d: -f7)" = /usr/sbin/nologin
if ! getent passwd "$attacker_user" >/dev/null; then
  useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin "$attacker_user"
fi
attacker_entry=$(getent passwd "$attacker_user")
attacker_uid=$(printf '%s' "$attacker_entry" | cut -d: -f3)
attacker_gid=$(printf '%s' "$attacker_entry" | cut -d: -f4)
test "$attacker_uid" != 0
test "$attacker_gid" != "$gateway_gid"
test "$(id -G "$attacker_user")" = "$attacker_gid"
test "$(printf '%s' "$attacker_entry" | cut -d: -f6)" = /nonexistent
test "$(printf '%s' "$attacker_entry" | cut -d: -f7)" = /usr/sbin/nologin
# BEGIN ARC FILTER GROUP IDENTITY HELPER
assert_exact_filter_group() {
  expected_filter_group_users=$(printf '%s\n%s\n' "$1" "$2" | LC_ALL=C sort)
  actual_filter_group_users=$(getent passwd | awk -F: -v gid="$3" '$4 == gid {print $1}' | LC_ALL=C sort)
  test "$actual_filter_group_users" = "$expected_filter_group_users"
  test -z "$(getent group "$1" | cut -d: -f4)"
}
# END ARC FILTER GROUP IDENTITY HELPER
assert_exact_filter_group "$gateway_user" "$filter_user" "$gateway_gid"
test "$(getent group "$interlock_group" | cut -d: -f4)" = "$filter_user"
test "$(getent group "$origin_group" | cut -d: -f4)" = "$filter_user"
test "$gateway_user" != "$interlock_user" && test "$gateway_user" != "$filter_user"
test -S "$interlock_socket" && test ! -L "$interlock_socket"
test "$(stat -c %U:%G:%a:%h "$interlock_socket")" = "$interlock_user:$interlock_group:660:1"
runuser -u "$gateway_user" -- test ! -r "$interlock_socket"
runuser -u "$attacker_user" -- test ! -r "$interlock_socket"
# Preserve the existing nginx configuration and replace only its running role;
# the dedicated filter below uses its own locked config and never reads /etc/nginx.
systemctl stop nginx.service 2>/dev/null || true
systemctl disable nginx.service 2>/dev/null || true
if ss -ltnp | grep -E ':(80|443)[[:space:]]' >/dev/null; then
  printf 'ports 80/443 remain occupied after stopping system nginx\n' >&2
  exit 1
fi
if test -e "$root/rpc-filter-state"; then
  test -d "$root/rpc-filter-state" && test ! -L "$root/rpc-filter-state"
else
  install -d -o "$filter_user" -g "$gateway_user" -m 0700 "$root/rpc-filter-state"
fi
chown "$filter_user:$gateway_user" "$root/rpc-filter-state"
chmod 0700 "$root/rpc-filter-state"
for directory in "$root/caddy-data" "$root/caddy-config"; do
  if test -e "$directory"; then
    test -d "$directory" && test ! -L "$directory"
  else
    install -d -o "$gateway_user" -g "$gateway_user" -m 0700 "$directory"
  fi
  chown "$gateway_user:$gateway_user" "$directory"
  chmod 0700 "$directory"
  test "$(stat -c %U:%G:%a "$directory")" = "$gateway_user:$gateway_user:700"
done
chown root:"$gateway_user" "$root"
chmod 0750 "$root"
chown root:"$gateway_user" "$root/caddy" "$root/Caddyfile.maintenance" "$root/Caddyfile.live"
chmod 0550 "$root/caddy"
chmod 0440 "$root/Caddyfile.maintenance" "$root/Caddyfile.live"
chown root:"$gateway_user" "$root/nginx-filter.conf" "$root/arc-nginx-filter-preflight"
chmod 0440 "$root/nginx-filter.conf"
chmod 0550 "$root/arc-nginx-filter-preflight"
test "$(stat -c %U:%G:%a "$root")" = "root:$gateway_user:750"
test "$(stat -c %U:%G:%a "$root/caddy")" = "root:$gateway_user:550"
test "$(stat -c %U:%G:%a:%h "$root/nginx-filter.conf")" = "root:$gateway_user:440:1"
test "$(stat -c %U:%G:%a:%h "$root/arc-nginx-filter-preflight")" = "root:$gateway_user:550:1"
printf '%s  %s/nginx-filter.conf\n' "$filter_config_sha" "$root" | sha256sum --check --strict
printf '%s  %s/arc-nginx-filter-preflight\n' "$filter_preflight_sha" "$root" | sha256sum --check --strict
printf '%s  %s/%s\n' "$filter_unit_sha" "$root" "$filter_service" | sha256sum --check --strict
runuser -u "$filter_user" -- "$root/arc-nginx-filter-preflight"
for tuple in "$maintenance_sha:Caddyfile.maintenance" "$live_sha:Caddyfile.live"; do
  expected=${tuple%%:*}; name=${tuple#*:}
  test -f "$root/$name" && test ! -L "$root/$name"
  printf '%s  %s/%s\n' "$expected" "$root" "$name" | sha256sum --check --strict
done
"$root/caddy" validate --config "$root/Caddyfile.maintenance" --adapter caddyfile
"$root/caddy" validate --config "$root/Caddyfile.live" --adapter caddyfile
active_tmp=$(mktemp "$root/.Caddyfile.active.XXXXXX")
cp -- "$root/Caddyfile.maintenance" "$active_tmp"
chown root:"$gateway_user" "$active_tmp"
chmod 0440 "$active_tmp"
sync "$active_tmp"
mv -T -- "$active_tmp" "$root/Caddyfile.active"
    /usr/bin/sync "$root"
test -f "$root/Caddyfile.active" && test ! -L "$root/Caddyfile.active"
test "$(stat -c %U:%G:%a:%h "$root/Caddyfile.active")" = "root:$gateway_user:440:1"
printf '%s  %s/Caddyfile.active\n' "$maintenance_sha" "$root" | sha256sum --check --strict
"$root/caddy" validate --config "$root/Caddyfile.active" --adapter caddyfile
units="$service $gateway_service $filter_service $interlock_service"
if [ "$archive_service" != none ]; then units="$units $archive_service"; fi
reject_unit_dropins() {
  unit=$1
  for base in /etc/systemd/system /run/systemd/system /usr/local/lib/systemd/system /usr/lib/systemd/system /lib/systemd/system; do
    directory="$base/$unit.d"
    if test -e "$directory" || test -L "$directory"; then
      test -d "$directory" && test ! -L "$directory"
      test "$(stat -c %U:%G:%a "$directory")" = root:root:755
      test -z "$(find "$directory" -mindepth 1 -maxdepth 1 -print -quit)"
    fi
  done
}
for unit in $units; do
  reject_unit_dropins "$unit"
  installed="/etc/systemd/system/$unit"
  if test -e "$installed"; then
    test -f "$installed" && test ! -L "$installed"
    test "$(stat -c %U:%G:%a:%h "$installed")" = root:root:644:1
    cmp --silent "$root/$unit" "$installed"
  else
    cp --no-clobber "$root/$unit" "$installed"
    chmod 0644 "$installed"
  fi
  test "$(stat -c %U:%G:%a:%h "$installed")" = root:root:644:1
done
systemctl daemon-reload
effective_inventory="$root/effective-systemd-security.inventory"
effective_temporary=$(mktemp "$root/.effective-systemd-security.XXXXXX")
: > "$effective_temporary"
assert_effective_unit() {
  unit=$1 expected_user=$2 expected_group=$3 expected_supplementary=$4
  installed="/etc/systemd/system/$unit"
  reject_unit_dropins "$unit"
  test -f "$installed" && test ! -L "$installed"
  test "$(stat -c %U:%G:%a:%h "$installed")" = root:root:644:1
  cmp --silent "$root/$unit" "$installed"
  test "$(systemctl show "$unit" --property=FragmentPath --value)" = "$installed"
  test -z "$(systemctl show "$unit" --property=DropInPaths --value)"
  test "$(systemctl show "$unit" --property=LoadState --value)" = loaded
  test "$(systemctl show "$unit" --property=Transient --value)" = no
  test "$(systemctl show "$unit" --property=User --value)" = "$expected_user"
  test "$(systemctl show "$unit" --property=Group --value)" = "$expected_group"
  test "$(systemctl show "$unit" --property=SupplementaryGroups --value)" = "$expected_supplementary"
  expected_exec=$(sed -n 's/^ExecStart=\([^ ]*\).*$/\1/p' "$root/$unit")
  test "$(grep -c '^ExecStart=' "$root/$unit")" = 1
  exec_start=$(systemctl show "$unit" --property=ExecStart --value)
  effective_exec=$(printf '%s\n' "$exec_start" | sed -n 's/^{ path=\([^ ;]*\) ;.*$/\1/p')
  test -n "$expected_exec" && test "$effective_exec" = "$expected_exec"
  unit_sha=$(sha256sum "$installed" | cut -d' ' -f1)
  {
    printf '[%s]\n' "$unit"
    printf 'FragmentPath=%s\nDropInPaths=\nUnitSHA256=%s\nUser=%s\nGroup=%s\nSupplementaryGroups=%s\nExecStartPath=%s\n' \
      "$installed" "$unit_sha" "$expected_user" "$expected_group" "$expected_supplementary" "$effective_exec"
  } >> "$effective_temporary"
}
assert_effective_unit "$service" "$validator_user" "$origin_group" ""
assert_effective_unit "$gateway_service" "$gateway_user" "$gateway_user" ""
assert_effective_unit "$filter_service" "$filter_user" "$gateway_user" "$interlock_group $origin_group"
assert_effective_unit "$interlock_service" "$interlock_user" "$interlock_group" ""
if [ "$archive_service" != none ]; then
  assert_effective_unit "$archive_service" "$archive_user" "$origin_group" "$archive_user"
fi
filter_exec_pre=$(systemctl show "$filter_service" --property=ExecStartPre --value)
filter_exec_pre_path=$(printf '%s\n' "$filter_exec_pre" | sed -n 's/^{ path=\([^ ;]*\) ;.*$/\1/p')
test "$filter_exec_pre_path" = "$root/arc-nginx-filter-preflight"
test "$(systemctl show "$filter_service" --property=NoNewPrivileges --value)" = yes
test "$(systemctl show "$filter_service" --property=ProtectSystem --value)" = strict
test "$(systemctl show "$filter_service" --property=ProtectHome --value)" = yes
printf 'FilterExecStartPrePath=%s\nFilterNoNewPrivileges=yes\nFilterProtectSystem=strict\nFilterProtectHome=yes\n' \
  "$filter_exec_pre_path" >> "$effective_temporary"
chmod 0400 "$effective_temporary"; sync "$effective_temporary"
if test -e "$effective_inventory"; then
  test -f "$effective_inventory" && test ! -L "$effective_inventory"
  cmp --silent "$effective_temporary" "$effective_inventory"
  rm -f -- "$effective_temporary"
else
  mv --no-clobber -T -- "$effective_temporary" "$effective_inventory"
  test ! -e "$effective_temporary"; /usr/bin/sync "$root"
fi
test "$(stat -c %U:%G:%a:%h "$effective_inventory")" = root:root:400:1
effective_systemd_inventory_sha=$(sha256sum "$effective_inventory" | cut -d' ' -f1)
test "$effective_systemd_inventory_sha" = "$expected_effective_systemd_inventory_sha"
if [ "$archive_service" != none ]; then
  systemctl enable "$archive_service"
  systemctl start "$archive_service"
fi
systemctl enable "$filter_service" "$gateway_service" "$service"
systemctl is-active --quiet "$interlock_service"
systemctl is-enabled --quiet "$interlock_service"
# A resumed partial rollout may already have started v3.  Return it to the
# maintenance-safe stopped state before issuing synthetic validator-only POSTs.
systemctl stop "$service"
! systemctl is-active --quiet "$service"
systemctl start "$filter_service"
filter_pid=$(systemctl show "$filter_service" --property=MainPID --value)
case "$filter_pid" in ''|0|*[!0-9]*) exit 1 ;; esac
test "$(awk '/^Uid:/{print $2}' "/proc/$filter_pid/status")" = "$filter_uid"
test "$(readlink -f "/proc/$filter_pid/exe")" = "$(readlink -f /usr/sbin/nginx)"
test -S "$public_filter_socket" && test ! -L "$public_filter_socket"
test "$(stat -c %U:%G:%a:%h "$public_filter_socket")" = "$filter_user:$gateway_user:770:1"
public_socket_rows=$(ss -H -lxnp | grep -F -- " $public_filter_socket" || true)
test "$(printf '%s\n' "$public_socket_rows" | awk 'NF {n+=1} END {print n+0}')" = 1
case "$public_socket_rows" in *"pid=$filter_pid,"*) ;; *) exit 1 ;; esac
if [ "$archive_filter_socket" != none ]; then
  test -S "$archive_filter_socket" && test ! -L "$archive_filter_socket"
  test "$(stat -c %U:%G:%a:%h "$archive_filter_socket")" = "$filter_user:$gateway_user:770:1"
  archive_socket_rows=$(ss -H -lxnp | grep -F -- " $archive_filter_socket" || true)
  test "$(printf '%s\n' "$archive_socket_rows" | awk 'NF {n+=1} END {print n+0}')" = 1
  case "$archive_socket_rows" in *"pid=$filter_pid,"*) ;; *) exit 1 ;; esac
fi
test -z "$(ss -H -ltnp | awk '$4 ~ /:(18080|18081|18090)$/ {print}')"
runuser -u "$attacker_user" -- test ! -r "$public_filter_socket"
if runuser -u "$attacker_user" -- curl --silent --show-error --fail \
  --unix-socket "$public_filter_socket" --connect-timeout 2 --max-time 5 \
  http://localhost/internal/community/reward/approve >/dev/null 2>&1; then
  printf 'unprivileged attacker connected to protected RPC filter socket\n' >&2
  exit 1
fi
# A real fail-closed runtime probe, before any validator/public service starts:
# stop the interlock and require both validator-only upstream classes to fail
# at auth_request (500).  Without their auth_request these calls would instead
# reach the deliberately absent v3 upstream and return 502.
systemctl stop "$interlock_service"
systemctl is-active --quiet "$interlock_service" && exit 1
reward_gate_failure_status=$(runuser -u "$gateway_user" -- curl --silent --show-error --output /dev/null --write-out '%{http_code}' \
  --unix-socket "$public_filter_socket" \
  --connect-timeout 2 --max-time 5 -H "X-Forwarded-For: $validator_probe_ip" \
  -H 'Content-Type: application/json' --data '{}' \
  http://localhost/internal/community/reward/approve || true)
shard_gate_failure_status=$(runuser -u "$gateway_user" -- curl --silent --show-error --output /dev/null --write-out '%{http_code}' \
  --unix-socket "$public_filter_socket" \
  --connect-timeout 2 --max-time 5 -H "X-Forwarded-For: $validator_probe_ip" \
  -H 'Content-Type: application/json' --data '{}' \
  http://localhost/shards/announce || true)
test "$reward_gate_failure_status" = 500
test "$shard_gate_failure_status" = 500
systemctl start "$interlock_service"
gate_recovered=false
for _ in $(seq 1 60); do
  status=$(runuser -u "$filter_user" -- curl --silent --show-error --output /dev/null --write-out '%{http_code}' \
    --unix-socket "$interlock_socket" --connect-timeout 2 --max-time 5 \
    http://localhost/gate || true)
  if [ "$status" = 204 ]; then gate_recovered=true; break; fi
  sleep 1
done
test "$gate_recovered" = true
caddy_identity_healthy_gate_status=$(runuser -u "$gateway_user" -- curl --silent --show-error --output /dev/null --write-out '%{http_code}' \
  --unix-socket "$public_filter_socket" --connect-timeout 2 --max-time 5 \
  -H "X-Forwarded-For: $validator_probe_ip" -H 'Content-Type: application/json' \
  --data '{}' http://localhost/internal/community/reward/approve || true)
test "$caddy_identity_healthy_gate_status" = 502
systemctl start "$gateway_service"
gateway_pid=$(systemctl show "$gateway_service" --property=MainPID --value)
case "$gateway_pid" in ''|0|*[!0-9]*) exit 1 ;; esac
test "$(awk '/^Uid:/{print $2}' "/proc/$gateway_pid/status")" = "$gateway_uid"
test -z "$(ss -H -ltnp | awk '$4 ~ /:2019$/ { print }')"
issued=false
for _ in $(seq 1 180); do
  status=$(curl --silent --show-error --output /dev/null --write-out '%{http_code}' --connect-timeout 5 --max-time 10 "https://$hostname/__arc_tls_probe__" || true)
  if [ "$status" = 404 ]; then
    issued=true
    break
  fi
  sleep 1
done
if [ "$issued" != true ]; then
  journalctl --no-pager -u "$gateway_service" -n 80 >&2 || true
  printf 'Caddy did not obtain a publicly trusted certificate; fail closed\n' >&2
  exit 1
fi
# The IPv4 short-lived certificate and protected certificate storage must
# survive an actual service restart before v3 can start.  This proves restart
# reuse only; this bounded rollout does not wait days or claim renewal occurred.
test "$(find "$root/caddy-data" -type f -size +0c -print -quit | wc -l)" = 1
systemctl restart "$gateway_service"
restarted=false
for _ in $(seq 1 60); do
  status=$(curl --silent --show-error --output /dev/null --write-out '%{http_code}' --connect-timeout 5 --max-time 10 "https://$hostname/__arc_tls_probe__" || true)
  if [ "$status" = 404 ]; then restarted=true; break; fi
  sleep 1
done
test "$restarted" = true
systemctl is-active --quiet "$filter_service"
systemctl is-active --quiet "$gateway_service"
if [ "$archive_service" != none ]; then systemctl is-active --quiet "$archive_service"; fi
security_receipt="$root/nginx-security-boundary.json"
payload=$(printf '{"archive_filter_socket_path":"%s","attacker_interlock_socket_denied":true,"attacker_socket_denied":true,"attacker_user":"%s","auth_request_module":true,"binary_path":"/usr/sbin/nginx","binary_sha256":"%s","caddy_identity_healthy_gate_status":502,"caddy_interlock_socket_denied":true,"caddy_restart_tls_probe_status":404,"certificate_storage_nonempty":true,"direct_tcp_filter_absent":true,"direct_tcp_interlock_absent":true,"effective_systemd_inventory_sha256":"%s","filter_config_sha256":"%s","filter_group_primary_users":["%s","%s"],"filter_group_supplementary_users":[],"filter_preflight_sha256":"%s","filter_socket_mode":"0770","filter_socket_path":"%s","filter_unit_sha256":"%s","filter_user":"%s","interlock_group":"%s","interlock_group_primary_users":["%s"],"interlock_group_supplementary_users":["%s"],"interlock_socket_mode":"0660","interlock_socket_path":"%s","node":"%s","origin_group":"%s","origin_group_primary_users":[],"origin_group_supplementary_users":["%s"],"package":"nginx","package_held":false,"package_version":"%s","reward_gate_failure_status":500,"rollout_manifest_sha256":"%s","schema":"arc.recovery.gateway-security-boundary.v1","shard_gate_failure_status":500}\n' \
  "$archive_filter_socket" "$attacker_user" "$nginx_sha" "$effective_systemd_inventory_sha" \
  "$filter_config_sha" "$gateway_user" "$filter_user" "$filter_preflight_sha" "$public_filter_socket" "$filter_unit_sha" \
  "$filter_user" "$interlock_group" "$interlock_user" "$filter_user" "$interlock_socket" \
  "$node_name" "$origin_group" "$filter_user" "$nginx_version" "$digest")
payload="$payload
"
if test -e "$security_receipt"; then
  test -f "$security_receipt" && test ! -L "$security_receipt"
  test "$(stat -c %U:%G:%a:%h "$security_receipt")" = root:root:400:1
  test "$(cat "$security_receipt")" = "${payload%?}"
else
  security_tmp=$(mktemp "$root/.nginx-security-boundary.XXXXXX")
  printf '%s' "$payload" > "$security_tmp"
  chmod 0400 "$security_tmp"; sync "$security_tmp"
  mv --no-clobber -T -- "$security_tmp" "$security_receipt"
  test ! -e "$security_tmp"; /usr/bin/sync "$root"
fi
trap - EXIT
printf '%s' "$payload"
"""
        self.prepared_production.add(node["name"])
        output = self.ssh(
            node,
            gateway_script,
            (
                root,
                node["host"],
                node["service_name"],
                self.gateway_service_name(node),
                self.filter_service_name(node),
                self.digest,
                (
                    self.legacy_archive_service_name(node)
                    if self.legacy_archive_for(node) is not None
                        else "none"
                ),
                "1" if baseline["validator_active"] else "0",
                "1" if baseline["validator_enabled"] else "0",
                "1" if baseline["gateway_active"] else "0",
                "1" if baseline["gateway_enabled"] else "0",
                "1" if baseline["filter_active"] else "0",
                "1" if baseline["filter_enabled"] else "0",
                "1" if baseline["archive_active"] else "0",
                "1" if baseline["archive_enabled"] else "0",
                "1" if baseline["nginx_active"] else "0",
                "1" if baseline["nginx_enabled"] else "0",
                sha256_bytes(self.maintenance_caddyfile(node).encode("utf-8")),
                sha256_bytes(self.caddyfile(node).encode("utf-8")),
                CADDY_USER,
                self.late_fork_interlock_service_name(node),
                "1" if baseline["interlock_active"] else "0",
                "1" if baseline["interlock_enabled"] else "0",
                "1" if retirement_committed else "0",
                self.manifest["archive"]["capture_id"],
                node["name"],
                NGINX_PACKAGE_VERSION,
                NGINX_LINUX_AMD64_SHA256,
                NGINX_FILTER_USER,
                sha256_bytes(self.nginx_filter(node).encode("utf-8")),
                sha256_bytes(self.filter_unit(node).encode("utf-8")),
                sha256_bytes(self.filter_preflight(node).encode("utf-8")),
                self.validators[0]["host"],
                self.filter_public_socket(node),
                (
                    self.filter_archive_socket(node)
                    if self.legacy_archive_for(node) is not None
                    else "none"
                ),
                NGINX_ATTACKER_USER,
                node["service_user"],
                LATE_FORK_INTERLOCK_USER,
                LEGACY_ARCHIVE_USER,
                LATE_FORK_INTERLOCK_GROUP,
                expected_effective_systemd_inventory_sha,
                RPC_ORIGIN_GROUP,
                self.late_fork_interlock_socket(node),
            ),
            timeout=900,
        )
        try:
            security = require_keys(
                json.loads(output),
                f"{node['name']} gateway security boundary",
                (
                    "schema", "rollout_manifest_sha256", "node", "package",
                    "package_version", "binary_path", "binary_sha256",
                    "auth_request_module", "certificate_storage_nonempty",
                    "caddy_restart_tls_probe_status", "filter_config_sha256",
                    "filter_unit_sha256", "filter_preflight_sha256",
                    "filter_user", "package_held", "reward_gate_failure_status",
                    "shard_gate_failure_status", "filter_socket_path",
                    "archive_filter_socket_path", "filter_socket_mode",
                    "attacker_user", "attacker_socket_denied",
                    "attacker_interlock_socket_denied",
                    "direct_tcp_filter_absent", "caddy_identity_healthy_gate_status",
                    "direct_tcp_interlock_absent", "caddy_interlock_socket_denied",
                    "effective_systemd_inventory_sha256",
                    "filter_group_primary_users",
                    "filter_group_supplementary_users",
                    "interlock_group", "interlock_group_primary_users",
                    "interlock_group_supplementary_users", "interlock_socket_mode",
                    "interlock_socket_path", "origin_group",
                    "origin_group_primary_users", "origin_group_supplementary_users",
                ),
            )
        except json.JSONDecodeError as error:
            fail(f"{node['name']} gateway security receipt is invalid JSON: {error}")
        if security != {
            "schema": "arc.recovery.gateway-security-boundary.v1",
            "rollout_manifest_sha256": self.digest,
            "node": node["name"],
            "package": "nginx",
            "package_version": NGINX_PACKAGE_VERSION,
            "binary_path": "/usr/sbin/nginx",
            "binary_sha256": NGINX_LINUX_AMD64_SHA256,
            "auth_request_module": True,
            "certificate_storage_nonempty": True,
            "caddy_restart_tls_probe_status": 404,
            "filter_config_sha256": sha256_bytes(
                self.nginx_filter(node).encode("utf-8")
            ),
            "filter_unit_sha256": sha256_bytes(
                self.filter_unit(node).encode("utf-8")
            ),
            "filter_preflight_sha256": sha256_bytes(
                self.filter_preflight(node).encode("utf-8")
            ),
            "filter_user": NGINX_FILTER_USER,
            "package_held": False,
            "reward_gate_failure_status": 500,
            "shard_gate_failure_status": 500,
            "filter_socket_path": self.filter_public_socket(node),
            "archive_filter_socket_path": (
                self.filter_archive_socket(node)
                if self.legacy_archive_for(node) is not None
                else "none"
            ),
            "filter_socket_mode": "0770",
            "attacker_user": NGINX_ATTACKER_USER,
            "attacker_socket_denied": True,
            "attacker_interlock_socket_denied": True,
            "direct_tcp_filter_absent": True,
            "direct_tcp_interlock_absent": True,
            "caddy_identity_healthy_gate_status": 502,
            "caddy_interlock_socket_denied": True,
            "effective_systemd_inventory_sha256": expected_effective_systemd_inventory_sha,
            "filter_group_primary_users": [CADDY_USER, NGINX_FILTER_USER],
            "filter_group_supplementary_users": [],
            "interlock_group": LATE_FORK_INTERLOCK_GROUP,
            "interlock_group_primary_users": [LATE_FORK_INTERLOCK_USER],
            "interlock_group_supplementary_users": [NGINX_FILTER_USER],
            "interlock_socket_mode": "0660",
            "interlock_socket_path": self.late_fork_interlock_socket(node),
            "origin_group": RPC_ORIGIN_GROUP,
            "origin_group_primary_users": [],
            "origin_group_supplementary_users": [NGINX_FILTER_USER],
        } or output.encode("utf-8") != canonical_bytes(security):
            fail(f"{node['name']} gateway security boundary differs")
        self.production_gateway_security_receipts[node["name"]] = security
        self.production_public_gate_open = False
        self.say(
            f"PASS {node['name']} issued trusted Caddy TLS with the durable maintenance-only edge"
        )
        return security

    def _prove_public_tls_evidence(
        self, node: Mapping[str, Any], *, phase: str
    ) -> dict[str, Any]:
        """Freshly prove the exact public IPv4 leaf and HTTPS edge over TLS."""

        if phase not in {"preflight", "post-rollout"}:
            fail("public TLS evidence phase is unsupported")
        root = node["remote_root"]
        output = self.ssh(
            node,
            "set -eu\n"
            + self.remote_semantic_python_prelude(node)
            + r'''host=$1 node=$2 rollout=$3 phase=$4 caddy=$5 caddy_sha=$6 caddy_version=$7
test -f "$caddy" && test ! -L "$caddy"
test "$(stat -c %U:%G:%a:%h "$caddy")" = root:arc-caddy:550:1
printf '%s  %s\n' "$caddy_sha" "$caddy" | sha256sum --check --strict
test "$("$caddy" version | awk '{print $1}')" = "$caddy_version"
arc_semantic_python - "$host" "$node" "$rollout" "$phase" "$caddy_sha" "$caddy_version" <<'PY'
import hashlib,ipaddress,json,re,socket,ssl,sys,time

host,node,rollout,phase,caddy_sha,caddy_version=sys.argv[1:]
if str(ipaddress.ip_address(host))!=host or ipaddress.ip_address(host).version!=4:
    raise SystemExit('TLS verification host is not canonical IPv4')
context=ssl.create_default_context(purpose=ssl.Purpose.SERVER_AUTH)
context.check_hostname=True
context.verify_mode=ssl.CERT_REQUIRED
context.minimum_version=ssl.TLSVersion.TLSv1_2
if hasattr(context,'hostname_checks_common_name'):
    context.hostname_checks_common_name=False
if context.cert_store_stats().get('x509_ca',0)<=0:
    raise SystemExit('public CA store is empty')
with socket.create_connection((host,443),timeout=10) as connection:
    with context.wrap_socket(connection,server_hostname=host) as tls:
        cert=tls.getpeercert()
        leaf=tls.getpeercert(binary_form=True)
        if not cert or not leaf:
            raise SystemExit('TLS peer omitted its verified leaf')
        san=cert.get('subjectAltName',())
        ip_sans=[value for kind,value in san if kind=='IP Address']
        dns_sans=[value for kind,value in san if kind=='DNS']
        if len(san)!=len(ip_sans)+len(dns_sans) or ip_sans!=[host] or dns_sans:
            raise SystemExit('TLS leaf SAN is not the exact validator IPv4')
        issuer_org=[value for rdn in cert.get('issuer',()) for key,value in rdn if key=='organizationName']
        if issuer_org!=["Let's Encrypt"] or cert.get('issuer')==cert.get('subject'):
            raise SystemExit('TLS leaf issuer is not the public Let\'s Encrypt chain')
        not_before=int(ssl.cert_time_to_seconds(cert['notBefore']))
        not_after=int(ssl.cert_time_to_seconds(cert['notAfter']))
        verified_at=int(time.time())
        lifetime=not_after-not_before
        remaining=not_after-verified_at
        if lifetime<=0 or lifetime>576000:
            raise SystemExit('TLS leaf exceeds the 160-hour short-lived maximum')
        if not_before>verified_at or remaining<172800:
            raise SystemExit('TLS leaf has inadequate remaining validity')
        request=(f'GET /__arc_tls_probe__ HTTP/1.1\r\nHost: {host}\r\n'
                 'User-Agent: arc-recovery-tls-proof/1\r\nConnection: close\r\n\r\n').encode('ascii')
        tls.sendall(request)
        response=b''
        while b'\r\n' not in response and len(response)<=8192:
            chunk=tls.recv(2048)
            if not chunk: break
            response+=chunk
        first=response.split(b'\r\n',1)[0]
        match=re.fullmatch(rb'HTTP/1\.[01] ([0-9]{3})(?: .*)?',first)
        if match is None or int(match.group(1))!=404:
            raise SystemExit('verified TLS HTTPS probe did not return 404')
value={
    'schema':'arc.recovery.public-tls-evidence.v1',
    'rollout_manifest_sha256':rollout,
    'phase':phase,
    'node':node,
    'host':host,
    'caddy_version':caddy_version,
    'caddy_binary_sha256':caddy_sha,
    'acme_directory':'https://acme-v02.api.letsencrypt.org/directory',
    'acme_profile':'shortlived',
    'verification_host':host,
    'san_ip_addresses':ip_sans,
    'san_dns_names':dns_sans,
    'issuer_organization':issuer_org[0],
    'leaf_sha256':hashlib.sha256(leaf).hexdigest(),
    'not_before_unix':not_before,
    'not_after_unix':not_after,
    'lifetime_seconds':lifetime,
    'remaining_validity_seconds':remaining,
    'verified_at_unix':verified_at,
    'hostname_verified':True,
    'public_trust_verified':True,
    'leaf_self_signed':False,
    'https_probe_status':404,
    'renewal_observed':False,
    'evidence_scope':'fresh-verified-handshake-and-https-probe-not-renewal',
}
sys.stdout.write(json.dumps(value,sort_keys=True,separators=(',',':'))+'\n')
PY
arc_semantic_python_revalidate
''',
            (
                node["host"],
                node["name"],
                self.digest,
                phase,
                f"{root}/caddy",
                CADDY_LINUX_AMD64_SHA256,
                CADDY_VERSION,
            ),
            timeout=60,
        )
        try:
            value = json.loads(output)
        except json.JSONDecodeError as error:
            fail(f"{node['name']} public TLS evidence is invalid JSON: {error}")
        evidence = validate_public_tls_evidence(
            value,
            rollout_sha256=self.digest,
            node=node["name"],
            host=node["host"],
            phase=phase,
        )
        if output.encode("utf-8") != canonical_bytes(evidence):
            fail(f"{node['name']} public TLS evidence is not canonical JSON")
        self.production_tls_evidence[phase][node["name"]] = evidence
        self.say(
            f"PASS {node['name']} {phase} TLS leaf has exact IPv4 SAN, public trust, "
            f"{evidence['lifetime_seconds']}s lifetime, and "
            f"{evidence['remaining_validity_seconds']}s remaining"
        )
        return evidence

    def _prove_public_tls_fleet(self, *, phase: str) -> dict[str, Any]:
        evidence: dict[str, dict[str, Any]] = {}
        with concurrent.futures.ThreadPoolExecutor(
            max_workers=REQUIRED_VALIDATORS
        ) as pool:
            futures = {
                pool.submit(self._prove_public_tls_evidence, node, phase=phase): node
                for node in self.validators
            }
            for future, node in futures.items():
                evidence[node["name"]] = future.result()
        if set(evidence) != {node["name"] for node in self.validators}:
            fail(f"{phase} TLS evidence omitted a fixed production validator")
        fleet = {
            "schema": "arc.recovery.public-tls-fleet-evidence.v1",
            "rollout_manifest_sha256": self.digest,
            "phase": phase,
            "maximum_leaf_lifetime_seconds": TLS_MAX_LEAF_LIFETIME_SECONDS,
            "minimum_remaining_validity_seconds": TLS_MIN_REMAINING_VALIDITY_SECONDS,
            "renewal_observed": False,
            "evidence_scope": "fresh-verified-handshake-and-https-probe-not-renewal",
            "nodes": [evidence[node["name"]] for node in self.validators],
        }
        self._rollback_journal_write(
            f"PUBLIC-TLS-{phase.upper()}-EVIDENCE.json", fleet
        )
        return fleet

    @staticmethod
    def _commitment_receipt(value: tuple[int, str, str]) -> dict[str, Any]:
        return {"height": value[0], "block_hash": value[1], "state_root": value[2]}

    def _set_public_gate_config(
        self,
        node: Mapping[str, Any],
        *,
        target: str,
        intent_sha256: str,
        final: tuple[int, str, str] | None = None,
    ) -> dict[str, Any]:
        if target not in {"live", "maintenance"}:
            fail("public gate target is unsupported")
        if target == "live" and final is None:
            fail("live public gate promotion requires a final six-node commitment")
        root = node["remote_root"]
        maintenance_sha = sha256_bytes(self.maintenance_caddyfile(node).encode("utf-8"))
        live_sha = sha256_bytes(self.caddyfile(node).encode("utf-8"))
        commitment = final or (0, "0" * 64, "0" * 64)
        output = self.ssh(
            node,
            "set -eu\n"
            + self.remote_semantic_python_prelude(node)
            + r'''root=$1 service=$2 rollout=$3 target=$4 maintenance_sha=$5 live_sha=$6 intent_sha=$7
height=$8 block_hash=$9 state_root=${10} hostname=${11} node_name=${12} gateway_user=${13}
case "$target" in live) source_name=Caddyfile.live; source_sha=$live_sha ;; maintenance) source_name=Caddyfile.maintenance; source_sha=$maintenance_sha ;; *) exit 1 ;; esac
case "$height" in ''|*[!0-9]*) exit 1 ;; esac
for value in "$rollout" "$maintenance_sha" "$live_sha" "$intent_sha" "$block_hash" "$state_root"; do case "$value" in *[!0-9a-f]*|'') exit 1 ;; esac; test "${#value}" = 64; done
test -d "$root" && test ! -L "$root"
test "$(cat "$root/.arc-recovery-rollout-owner")" = "$rollout"
for tuple in "$maintenance_sha:Caddyfile.maintenance" "$live_sha:Caddyfile.live"; do
  expected=${tuple%%:*}; name=${tuple#*:}
  test -f "$root/$name" && test ! -L "$root/$name"
  test "$(stat -c %U:%G:%a:%h "$root/$name")" = "root:$gateway_user:440:1"
  printf '%s  %s/%s\n' "$expected" "$root" "$name" | sha256sum --check --strict
done
gate="$root/public-gate"
if test -e "$gate"; then
  test -d "$gate" && test ! -L "$gate"
  test "$(stat -c %U:%G:%a "$gate")" = root:root:700
else
  mkdir --mode=0700 "$gate"
fi
temporary=$(mktemp "$root/.Caddyfile.active.XXXXXX")
trap 'rm -f -- "$temporary"' EXIT HUP INT TERM
cp -- "$root/$source_name" "$temporary"
chown root:"$gateway_user" "$temporary"
chmod 0440 "$temporary"
printf '%s  %s\n' "$source_sha" "$temporary" | sha256sum --check --strict
sync "$temporary"
mv -T -- "$temporary" "$root/Caddyfile.active"
/usr/bin/sync "$root"
trap - EXIT HUP INT TERM
test -f "$root/Caddyfile.active" && test ! -L "$root/Caddyfile.active"
test "$(stat -c %U:%G:%a:%h "$root/Caddyfile.active")" = "root:$gateway_user:440:1"
printf '%s  %s/Caddyfile.active\n' "$source_sha" "$root" | sha256sum --check --strict
"$root/caddy" validate --config "$root/Caddyfile.active" --adapter caddyfile
if systemctl is-active --quiet "$service"; then
  # With Caddy's admin API disabled, transitions restart from the exact
  # fsynced active inode instead of exposing a mutable local control socket.
  systemctl restart "$service"
  systemctl is-active --quiet "$service"
fi
if [ "$target" = live ]; then
  status=$(curl --silent --show-error --output /dev/null --write-out '%{http_code}' --connect-timeout 5 --max-time 15 --resolve "$hostname:443:127.0.0.1" "https://$hostname/health" || true)
  test "$status" = 200 || { printf 'live edge health returned HTTP %s\n' "$status" >&2; exit 1; }
else
  status=$(curl --silent --show-error --output /dev/null --write-out '%{http_code}' --connect-timeout 5 --max-time 15 --resolve "$hostname:443:127.0.0.1" "https://$hostname/health" || true)
  test "$status" = 503 || { printf 'maintenance edge health returned HTTP %s\n' "$status" >&2; exit 1; }
fi
receipt="$gate/${target}.json"
arc_semantic_python - "$receipt" "$rollout" "$target" "$source_sha" "$intent_sha" "$height" "$block_hash" "$state_root" "$node_name" "$hostname" <<'PY'
import hashlib,json,os,pathlib,stat,sys
path=pathlib.Path(sys.argv[1])
value={"schema":"arc.recovery.public-gate-host.v1","rollout_manifest_sha256":sys.argv[2],
       "state":sys.argv[3],"active_caddyfile_sha256":sys.argv[4],
       "promotion_intent_sha256":sys.argv[5],"height":int(sys.argv[6]),
       "block_hash":sys.argv[7],"state_root":sys.argv[8],"node":sys.argv[9],"host":sys.argv[10]}
payload=(json.dumps(value,sort_keys=True,separators=(",",":"))+"\n").encode()
if path.exists() or path.is_symlink():
    details=path.lstat()
    if (path.is_symlink() or not stat.S_ISREG(details.st_mode)
            or stat.S_IMODE(details.st_mode)!=0o400 or details.st_nlink!=1
            or path.read_bytes()!=payload):
        raise SystemExit("public gate host receipt differs")
else:
    fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
    with os.fdopen(fd,"wb") as handle:
        handle.write(payload);handle.flush();os.fsync(handle.fileno())
    directory=os.open(path.parent,os.O_RDONLY|getattr(os,"O_DIRECTORY",0)|getattr(os,"O_NOFOLLOW",0))
    try:os.fsync(directory)
    finally:os.close(directory)
sys.stdout.buffer.write(payload)
PY
arc_semantic_python_revalidate
''',
            (
                root,
                self.gateway_service_name(node),
                self.digest,
                target,
                maintenance_sha,
                live_sha,
                intent_sha256,
                str(commitment[0]),
                commitment[1],
                commitment[2],
                node["host"],
                node["name"],
                CADDY_USER,
            ),
            timeout=120,
        )
        try:
            receipt = require_keys(
                json.loads(output),
                f"{node['name']} public gate receipt",
                (
                    "schema", "rollout_manifest_sha256", "state",
                    "active_caddyfile_sha256", "promotion_intent_sha256",
                    "height", "block_hash", "state_root", "node", "host",
                ),
            )
        except json.JSONDecodeError as error:
            fail(f"{node['name']} public gate receipt is invalid JSON: {error}")
        expected = {
            "schema": "arc.recovery.public-gate-host.v1",
            "rollout_manifest_sha256": self.digest,
            "state": target,
            "active_caddyfile_sha256": live_sha if target == "live" else maintenance_sha,
            "promotion_intent_sha256": intent_sha256,
            "height": commitment[0],
            "block_hash": commitment[1],
            "state_root": commitment[2],
            "node": node["name"],
            "host": node["host"],
        }
        if receipt != expected or output.encode("utf-8") != canonical_bytes(receipt):
            fail(f"{node['name']} public gate receipt differs from the exact transition")
        return receipt

    def close_public_gate(self, intent_sha256: str) -> None:
        failures: list[str] = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=REQUIRED_VALIDATORS) as pool:
            futures = {
                pool.submit(
                    self._set_public_gate_config,
                    node,
                    target="maintenance",
                    intent_sha256=intent_sha256,
                ): node["name"]
                for node in self.validators
            }
            for future, name in futures.items():
                try:
                    future.result()
                except BaseException as error:
                    failures.append(f"{name}:{type(error).__name__}:{error}")
        self.production_public_gate_open = False
        if failures:
            fail("PUBLIC_GATE_MAINTENANCE_INCOMPLETE: " + "; ".join(failures))
        self.say("PASS every reachable production edge is durably maintenance-only")

    def open_public_gate(
        self,
        initial: tuple[int, str, str],
        final: tuple[int, str, str],
    ) -> str:
        legacy_max = self.chain["legacy_public_max_height"]
        if initial[0] <= legacy_max:
            fail("public gate opening proof is not strictly above the legacy maximum")
        if final[0] < initial[0] + self.checks["min_height_advance"]:
            fail("public gate opening proof does not include the required advance")
        for label, commitment in (("initial", initial), ("final", final)):
            bare_hash(commitment[1], f"public gate {label} block hash")
            bare_hash(commitment[2], f"public gate {label} state root")
        intent = {
            "schema": "arc.recovery.public-gate-open-intent.v1",
            "rollout_manifest_sha256": self.digest,
            "legacy_maintenance_evidence_bundle_sha256": self.chain[
                "legacy_maintenance_evidence_bundle_sha256"
            ],
            "legacy_maintenance_boundary_sha256": self.chain[
                "legacy_maintenance_boundary_sha256"
            ],
            "legacy_public_max_height": legacy_max,
            "required_height_relation": "strictly-greater-than-legacy_public_max_height",
            "minimum_height_advance": self.checks["min_height_advance"],
            "initial": self._commitment_receipt(initial),
            "final": self._commitment_receipt(final),
            "validators": [
                {"node": node["name"], "host": node["host"]}
                for node in self.validators
            ],
        }
        intent_sha = self._rollback_journal_write("PUBLIC-GATE-OPEN-INTENT.json", intent)
        self.public_gate_intent_sha256 = intent_sha
        receipts: dict[str, dict[str, Any]] = {}
        failures: list[str] = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=REQUIRED_VALIDATORS) as pool:
            futures = {
                pool.submit(
                    self._set_public_gate_config,
                    node,
                    target="live",
                    intent_sha256=intent_sha,
                    final=final,
                ): node["name"]
                for node in self.validators
            }
            for future, name in futures.items():
                try:
                    receipts[name] = future.result()
                except BaseException as error:
                    failures.append(f"{name}:{type(error).__name__}:{error}")
        if failures:
            try:
                self.close_public_gate(intent_sha)
            except BaseException as close_error:
                failures.append(f"maintenance-reclose:{type(close_error).__name__}:{close_error}")
            fail("PUBLIC_GATE_OPEN_INCOMPLETE: " + "; ".join(failures))
        if set(receipts) != {node["name"] for node in self.validators}:
            fail("public gate promotion receipt set is not the exact six")
        # Host receipts contain identical commitment/config roots; preserve
        # fleet order separately so a missing or duplicated result is visible.
        receipt = {
            "schema": "arc.recovery.public-gate-open-receipt.v1",
            "rollout_manifest_sha256": self.digest,
            "promotion_intent_sha256": intent_sha,
            "final": self._commitment_receipt(final),
            "nodes": [
                {
                    "node": node["name"],
                    "host": node["host"],
                    "receipt_sha256": sha256_bytes(canonical_bytes(host_receipt)),
                }
                for node in self.validators
                for host_receipt in (receipts[node["name"]],)
            ],
        }
        receipt_sha = self._rollback_journal_write("PUBLIC-GATE-OPEN-RECEIPT.json", receipt)
        self.public_gate_receipt_sha256 = receipt_sha
        self.production_public_gate_open = True
        self.say(
            "PASS journaled all-six public promotion opened exact live routes at "
            f"#{final[0]} hash={final[1]} root={final[2]}"
        )
        return receipt_sha

    def production_service(self, node: Mapping[str, Any], action: str) -> None:
        if action not in {"start", "stop", "restart"}:
            fail(f"unsupported service action {action}")
        timeout = {
            "start": NODE_SERVICE_START_TIMEOUT_SECONDS,
            "stop": NODE_SERVICE_STOP_TIMEOUT_SECONDS,
            "restart": NODE_SERVICE_RESTART_TIMEOUT_SECONDS,
        }[action]
        # A successful systemctl return is not a sufficient start boundary:
        # Type=simple returns before Rust necessarily binds QUIC/RPC. Poll within
        # the reviewed lifecycle timeout and require two consecutive samples
        # with one stable MainPID owning the sole UDP row and exact protected
        # Unix RPC socket. Foreign/duplicate rows fail immediately; an absent
        # listener may only consume the deadline.
        self.ssh(
            node,
            r'''set -eu
action=$1 service=$2 p2p_port=$3 ready_timeout=$4 rpc_socket=$5 rpc_user=$6 rpc_group=$7 retired_rpc_port=$8
systemctl "$action" "$service"
if [ "$action" = start ] || [ "$action" = restart ]; then
  case "$p2p_port" in ''|*[!0-9]*) printf 'invalid QUIC P2P port\n' >&2; exit 1 ;; esac
  case "$ready_timeout" in ''|0|*[!0-9]*) printf 'invalid QUIC readiness timeout\n' >&2; exit 1 ;; esac
  remaining=$ready_timeout
  stable_pid=""
  ready=false
  while [ "$remaining" -gt 0 ]; do
    state=$(systemctl show "$service" --property=ActiveState --value)
    case "$state" in failed|inactive|deactivating) printf 'validator entered %s during %s readiness\n' "$state" "$action" >&2; exit 1 ;; esac
    pid=$(systemctl show "$service" --property=MainPID --value)
    case "$pid" in
      ''|0|*[!0-9]*)
        stable_pid=""
        remaining=$((remaining - 1))
        sleep 1
        continue
        ;;
    esac
    udp_rows=$(ss -H -lunp | awk -v port="$p2p_port" '$4 ~ (":" port "$") { print }')
    udp_count=$(printf '%s\n' "$udp_rows" | awk 'NF { count += 1 } END { print count + 0 }')
    if [ "$udp_count" -gt 1 ]; then
      printf 'validator QUIC has duplicate UDP rows during %s readiness\n%s\n' "$action" "$udp_rows" >&2; exit 1
    fi
    unix_rows=$(ss -H -lxnp | grep -F -- " $rpc_socket" || true)
    unix_count=$(printf '%s\n' "$unix_rows" | awk 'NF { count += 1 } END { print count + 0 }')
    if [ "$unix_count" -gt 1 ]; then
      printf 'validator RPC has duplicate Unix rows during %s readiness\n%s\n' "$action" "$unix_rows" >&2; exit 1
    fi
    if [ -n "$(ss -H -ltnp | awk -v port="$retired_rpc_port" '$4 ~ (":" port "$") { print }')" ]; then
      printf 'validator exposed a forbidden TCP RPC listener during %s readiness\n' "$action" >&2; exit 1
    fi
    if [ "$udp_count" -eq 1 ] && [ "$unix_count" -eq 1 ]; then
      case "$udp_rows" in
        *"pid=$pid,"*) ;;
        *) printf 'UDP QUIC row is foreign during %s readiness (current MainPID %s)\n%s\n' "$action" "$pid" "$udp_rows" >&2; exit 1 ;;
      esac
      case "$unix_rows" in
        *"pid=$pid,"*) ;;
        *) printf 'Unix RPC row is foreign during %s readiness (current MainPID %s)\n%s\n' "$action" "$pid" "$unix_rows" >&2; exit 1 ;;
      esac
      test -S "$rpc_socket" && test ! -L "$rpc_socket"
      test "$(stat -c %U:%G:%a:%h "$rpc_socket")" = "$rpc_user:$rpc_group:660:1"
      rpc_runtime=${rpc_socket%/*}
      test -d "$rpc_runtime" && test ! -L "$rpc_runtime"
      test "$(stat -c %U:%G:%a "$rpc_runtime")" = "$rpc_user:$rpc_group:750"
      if [ "$stable_pid" = "$pid" ] && systemctl is-active --quiet "$service"; then ready=true; break; fi
      stable_pid="$pid"
    else
      stable_pid=""
    fi
    remaining=$((remaining - 1))
    sleep 1
  done
  if [ "$ready" != true ]; then
    printf 'validator QUIC/Unix RPC listeners did not become stably owned before %s timeout\n' "$action" >&2; exit 1
  fi
  final_pid=$(systemctl show "$service" --property=MainPID --value)
  [ "$final_pid" = "$stable_pid" ] || { printf 'validator MainPID changed at %s readiness boundary\n' "$action" >&2; exit 1; }
fi
''',
            (
                action,
                node["service_name"],
                str(node["p2p_port"]),
                str(max(1, timeout - 5)),
                self.validator_rpc_socket(node),
                node["service_user"],
                RPC_ORIGIN_GROUP,
                node["rpc_listen"].rsplit(":", 1)[1],
            ),
            timeout=timeout,
        )
        if action in {"start", "restart"}:
            self.started_production.add(node["name"])
        elif action == "stop":
            self.started_production.discard(node["name"])

    def _prove_production_listener(
        self, node: Mapping[str, Any], *, public_contract: bool = False
    ) -> None:
        script = r"""
set -eu
rpc_socket=$1 service=$2 rpc_user=$3 rpc_group=$4 retired_rpc_port=$5
systemctl is-active --quiet "$service"
pid=$(systemctl show "$service" --property=MainPID --value)
case "$pid" in ''|0|*[!0-9]*) printf 'validator has no exact MainPID\n' >&2; exit 1 ;; esac
rows=$(ss -H -lxnp | grep -F -- " $rpc_socket" || true)
test "$(printf '%s\n' "$rows" | awk 'NF {n+=1} END {print n+0}')" = 1
case "$rows" in *"pid=$pid,"*) ;; *) printf 'validator Unix RPC listener is foreign\n' >&2; exit 1 ;; esac
test -S "$rpc_socket" && test ! -L "$rpc_socket"
test "$(stat -c %U:%G:%a:%h "$rpc_socket")" = "$rpc_user:$rpc_group:660:1"
runtime=${rpc_socket%/*}
test -d "$runtime" && test ! -L "$runtime"
test "$(stat -c %U:%G:%a "$runtime")" = "$rpc_user:$rpc_group:750"
test -z "$(ss -H -ltnp | awk -v port="$retired_rpc_port" '$4 ~ (":" port "$") {print}')"
"""
        self.ssh(
            node,
            script,
            (
                self.validator_rpc_socket(node),
                node["service_name"],
                node["service_user"],
                RPC_ORIGIN_GROUP,
                node["rpc_listen"].rsplit(":", 1)[1],
            ),
        )
        self._http_json(node, "/network/info")
        if public_contract:
            self._prove_public_browser_contract(node)
        else:
            self._prove_maintenance_gateway_contract(node)

    def _prove_maintenance_gateway_contract(self, node: Mapping[str, Any]) -> None:
        status, headers = self._http_status_headers(
            node,
            "/health",
            method="GET",
            origin=PUBLIC_BROWSER_ORIGIN,
        )
        if status != 503:
            fail(
                f"{node['name']} public edge escaped maintenance before promotion: HTTP {status}"
            )
        if headers.get("Access-Control-Allow-Origin") is not None:
            fail(f"{node['name']} maintenance edge leaked browser-readable application data")
        probe_status, _headers = self._http_status_headers(
            node, "/__arc_tls_probe__", method="GET"
        )
        if probe_status != 404:
            fail(f"{node['name']} maintenance TLS probe differs: HTTP {probe_status}")

    def _prove_public_browser_contract(self, node: Mapping[str, Any]) -> None:
        """Exercise the browser contract at the public TLS edge.

        OPTIONS must terminate at Caddy with no node proxy. Public GET/POST
        routes expose CORS only to the exact GitHub Pages origin. The flat
        transfer route must reach the node and reject an unsigned payload,
        while validator-only approval and shard routes expose no browser CORS.
        """

        def request(
            method: str,
            path: str,
            *,
            origin: str,
            requested_method: str | None = None,
            body: bytes | None = None,
        ) -> tuple[int, Mapping[str, str], bytes]:
            headers = {
                "Accept": "application/json",
                "Origin": origin,
                "User-Agent": "arc-recovery-browser-gate/1",
            }
            if requested_method is not None:
                headers["Access-Control-Request-Method"] = requested_method
                headers["Access-Control-Request-Headers"] = "content-type"
            if body is not None:
                headers["Content-Type"] = "application/json"
            rpc_request = urllib.request.Request(
                node["rpc_url"] + path,
                data=body if body is not None else (b"" if method == "POST" else None),
                headers=headers,
                method=method,
            )
            try:
                with urllib.request.urlopen(
                    rpc_request,
                    timeout=20,
                    context=ssl.create_default_context(),
                ) as response:
                    response_body = response.read(1024)
                    return response.status, response.headers, response_body
            except urllib.error.HTTPError as error:
                try:
                    response_body = error.read(1024)
                    return error.code, error.headers, response_body
                finally:
                    error.close()
            except (urllib.error.URLError, TimeoutError) as error:
                raise RolloutError(
                    f"{node['name']} browser CORS gate {method} {path}: {error}"
                ) from error

        def require_allowed(status: int, headers: Mapping[str, str], expected: int) -> None:
            if status != expected:
                fail(f"{node['name']} public browser preflight returned HTTP {status}, expected {expected}")
            if headers.get("Access-Control-Allow-Origin") != PUBLIC_BROWSER_ORIGIN:
                fail(f"{node['name']} public browser route omitted the exact allow-origin")
            vary = {part.strip().lower() for part in headers.get("Vary", "").split(",")}
            if "origin" not in vary:
                fail(f"{node['name']} public browser route omitted Vary: Origin")

        status, headers, _ = request(
            "OPTIONS",
            "/network/info",
            origin=PUBLIC_BROWSER_ORIGIN,
            requested_method="GET",
        )
        require_allowed(status, headers, 204)
        status, headers, _ = request(
            "OPTIONS",
            "/inference/run",
            origin=PUBLIC_BROWSER_ORIGIN,
            requested_method="POST",
        )
        require_allowed(status, headers, 204)
        status, headers, _ = request("GET", "/network/info", origin=PUBLIC_BROWSER_ORIGIN)
        require_allowed(status, headers, 200)

        # The SDKs use the flat /tx/submit and /tx/submit_batch wire contracts.
        # Exercise both through the public TLS gateway with an intentionally
        # unsigned transfer. The single route's signing guidance and the batch
        # handler's zero-accepted rejection prove each route reached the node
        # while also proving unsigned mutation is still rejected. A gateway
        # 404, an accepted transaction, or a generic proxy failure stops the
        # rollout.
        probe_from = hashlib.sha256(
            f"arc-gateway-unsigned-probe:{self.manifest['rollout_id']}:{node['name']}:from".encode()
        ).hexdigest()
        probe_to = hashlib.sha256(
            f"arc-gateway-unsigned-probe:{self.manifest['rollout_id']}:{node['name']}:to".encode()
        ).hexdigest()
        unsigned_transfer = json.dumps(
            {
                "from": probe_from,
                "to": probe_to,
                "amount": 1,
                "nonce": 0,
                "fee": 1,
                "tx_type": "transfer",
            },
            sort_keys=True,
            separators=(",", ":"),
        ).encode()
        status, headers, response_body = request(
            "POST",
            "/tx/submit",
            origin=PUBLIC_BROWSER_ORIGIN,
            body=unsigned_transfer,
        )
        require_allowed(status, headers, 400)
        signing_error = response_body.decode("utf-8", errors="replace")
        if "signature" not in signing_error.lower() or "public_key" not in signing_error:
            fail(f"{node['name']} flat transaction route did not fail closed with signing guidance")

        unsigned_batch = json.dumps(
            {"transactions": [json.loads(unsigned_transfer)]},
            sort_keys=True,
            separators=(",", ":"),
        ).encode()
        status, headers, response_body = request(
            "POST",
            "/tx/submit_batch",
            origin=PUBLIC_BROWSER_ORIGIN,
            body=unsigned_batch,
        )
        require_allowed(status, headers, 200)
        try:
            batch_result = json.loads(response_body)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise RolloutError(
                f"{node['name']} batch transaction route returned invalid JSON"
            ) from error
        if (
            not isinstance(batch_result, dict)
            or batch_result.get("accepted") != 0
            or not isinstance(batch_result.get("rejected"), int)
            or batch_result["rejected"] < 1
            or batch_result.get("tx_hashes") != []
        ):
            fail(f"{node['name']} batch transaction route did not reject every unsigned transfer")

        oversized_batch = json.dumps(
            {
                "transactions": [
                    json.loads(unsigned_transfer)
                    for _ in range(PUBLIC_TX_SUBMIT_BATCH_MAX_ITEMS + 1)
                ]
            },
            sort_keys=True,
            separators=(",", ":"),
        ).encode()
        status, headers, _ = request(
            "POST",
            "/tx/submit_batch",
            origin=PUBLIC_BROWSER_ORIGIN,
            body=oversized_batch,
        )
        require_allowed(status, headers, 413)

        status, headers, _ = request(
            "OPTIONS",
            "/internal/community/reward/approve",
            origin=PUBLIC_BROWSER_ORIGIN,
            requested_method="POST",
        )
        if status != 404 or headers.get("Access-Control-Allow-Origin") is not None:
            fail(f"{node['name']} validator approval route leaked browser CORS")
        status, headers, _ = request(
            "OPTIONS",
            "/network/info",
            origin="https://attacker.invalid",
            requested_method="GET",
        )
        if status != 404 or headers.get("Access-Control-Allow-Origin") is not None:
            fail(f"{node['name']} public route allowed an unsealed browser origin")

    def _check_production_shard_topology(self) -> str:
        expected: dict[tuple[int, int], set[str]] = {}
        for validator in self.validators:
            for start, end in validator["shard_ranges"]:
                expected.setdefault((start, end), set()).add(validator["rpc_url"])

        model_ids: set[str] = set()
        for validator in self.validators:
            body = self._http_json(validator, "/shards", timeout=20)
            if body.get("total_layers") != CANONICAL_MODEL_LAYERS or body.get("fully_covered") is not True:
                fail(f"{validator['name']} does not report one fully covered 32-layer shard pipeline")
            if body.get("profile_bound") is not True:
                fail(f"{validator['name']} does not report a profile-bound shard pipeline")
            if body.get("execution_profile") != CANONICAL_EXECUTION_PROFILE:
                fail(
                    f"{validator['name']} shard pipeline is not sealed to the canonical INT8 execution profile"
                )
            model_id = bare_hash(body.get("model_id"), f"{validator['name']} /shards model_id")
            model_ids.add(model_id)
            actual: dict[tuple[int, int], set[str]] = {}
            shards = body.get("shards")
            if not isinstance(shards, list):
                fail(f"{validator['name']} /shards omitted the shard array")
            for index, shard in enumerate(shards):
                if not isinstance(shard, dict):
                    fail(f"{validator['name']} /shards[{index}] is not an object")
                if bare_hash(shard.get("model_id"), f"{validator['name']} shard model_id") != model_id:
                    fail(f"{validator['name']} mixed model artifacts in one shard registry")
                if shard.get("execution_profile") != CANONICAL_EXECUTION_PROFILE:
                    fail(
                        f"{validator['name']} shard registry contains a missing or mixed non-canonical execution profile"
                    )
                start = shard.get("start_layer")
                end = shard.get("end_layer")
                origin = shard.get("socket_addr")
                if (
                    not isinstance(start, int)
                    or isinstance(start, bool)
                    or not isinstance(end, int)
                    or isinstance(end, bool)
                    or not isinstance(origin, str)
                ):
                    fail(f"{validator['name']} /shards[{index}] has malformed range/origin fields")
                actual.setdefault((start, end), set()).add(origin.rstrip("/"))
            if actual != expected:
                fail(
                    f"{validator['name']} shard registry differs from the sealed exact 3x HTTPS topology"
                )
        if len(model_ids) != 1:
            fail("validator shard registries disagree on the exact BLAKE3 model artifact ID")
        return next(iter(model_ids))

    def prove_production_shard_topology(self) -> None:
        deadline = time.monotonic() + self.checks["convergence_timeout_seconds"]
        last_error: RolloutError | None = None
        while time.monotonic() < deadline:
            try:
                model_id = self._check_production_shard_topology()
                self.say(
                    "PASS all validators expose the sealed 32-layer/3x-replicated topology "
                    f"through validator-only HTTPS (model_id=0x{model_id})"
                )
                return
            except RolloutError as error:
                last_error = error
                time.sleep(self.checks["poll_interval_seconds"])
        raise RolloutError(f"production shard topology did not converge: {last_error}")

    def prove_production_runtime_inventory(self) -> None:
        script = r"""
set -eu
service=$1 argv_sha=$2 executable=$3 model=$4 model_sha=$5 model_size=$6 archive_service=$7 archive_root=$8 archive_argv_sha=$9 p2p_port=${10} key=${11} key_sha=${12} expected_address=${13} identity_cli=${14} identity_cli_sha=${15} validator_rpc_socket=${16} validator_user=${17} origin_group=${18} retired_rpc_port=${19} archive_rpc_socket=${20} archive_user=${21} retired_archive_rpc_port=${22}
systemctl is-active --quiet "$service"
pid=$(systemctl show "$service" --property=MainPID --value)
case "$pid" in ''|0|*[!0-9]*) printf 'service has no exact MainPID\n' >&2; exit 1 ;; esac
test -d "/proc/$pid"
test "$(cat "/proc/$pid/comm")" = arc-node
test "$(readlink "/proc/$pid/exe")" = "$executable"
test "$(sha256sum "/proc/$pid/cmdline" | cut -d' ' -f1)" = "$argv_sha"
rows=$(ss -H -lunp | awk -v port="$p2p_port" '$4 ~ (":" port "$") { print }')
count=$(printf '%s\n' "$rows" | awk 'NF { count += 1 } END { print count + 0 }')
test "$count" = 1 || { printf 'runtime MainPID %s must own exactly one UDP QUIC row; found %s\n%s\n' "$pid" "$count" "$rows" >&2; exit 1; }
case "$rows" in *"pid=$pid,"*) ;; *) printf 'runtime UDP QUIC row is foreign to MainPID %s\n%s\n' "$pid" "$rows" >&2; exit 1 ;; esac
exact_unix_listener() {
  socket=$1 expected_pid=$2 expected_user=$3 expected_group=$4 label=$5
  unix_rows=$(ss -H -lxnp | grep -F -- " $socket" || true)
  unix_count=$(printf '%s\n' "$unix_rows" | awk 'NF {n+=1} END {print n+0}')
  test "$unix_count" = 1 || { printf '%s must own exactly one Unix row; found %s\n%s\n' "$label" "$unix_count" "$unix_rows" >&2; exit 1; }
  case "$unix_rows" in *"pid=$expected_pid,"*) ;; *) printf '%s Unix row is foreign to MainPID %s\n%s\n' "$label" "$expected_pid" "$unix_rows" >&2; exit 1 ;; esac
  test -S "$socket" && test ! -L "$socket"
  test "$(stat -c %U:%G:%a:%h "$socket")" = "$expected_user:$expected_group:660:1"
  runtime=${socket%/*}
  test -d "$runtime" && test ! -L "$runtime"
  test "$(stat -c %U:%G:%a "$runtime")" = "$expected_user:$expected_group:750"
}
exact_unix_listener "$validator_rpc_socket" "$pid" "$validator_user" "$origin_group" validator-rpc
test -z "$(ss -H -ltnp | awk -v port="$retired_rpc_port" '$4 ~ (":" port "$") {print}')"
test -f "$key" && test ! -L "$key"
test "$(stat -c %U:%G:%a:%h "$key")" = root:root:600:1
test "$(sha256sum "$key" | cut -d' ' -f1)" = "$key_sha"
test -f "$identity_cli" && test ! -L "$identity_cli"
test "$(stat -c %U:%G:%a:%h "$identity_cli")" = root:root:500:1
exec 8<"$identity_cli"
test "$(sha256sum /proc/self/fd/8 | cut -d' ' -f1)" = "$identity_cli_sha"
derived=$(/usr/bin/env -i HOME=/root PATH=/usr/bin:/bin LANG=C LC_ALL=C /proc/self/fd/8 keygen --verify-keyfile "$key")
test "$derived" = "$expected_address" || { printf 'runtime validator key derives a different sealed address\n' >&2; exit 1; }
test "$(sha256sum /proc/self/fd/8 | cut -d' ' -f1)" = "$identity_cli_sha"
test "$(sha256sum "$key" | cut -d' ' -f1)" = "$key_sha"
expected_pids="$pid"
if [ "$archive_service" != none ]; then
  systemctl is-active --quiet "$archive_service"
  archive_pid=$(systemctl show "$archive_service" --property=MainPID --value)
  case "$archive_pid" in ''|0|*[!0-9]*) printf 'archive has no exact MainPID\n' >&2; exit 1 ;; esac
  test "$(cat "/proc/$archive_pid/comm")" = arc-node
  test "$(readlink "/proc/$archive_pid/exe")" = "$archive_root/arc-node"
  test "$(sha256sum "/proc/$archive_pid/cmdline" | cut -d' ' -f1)" = "$archive_argv_sha"
  exact_unix_listener "$archive_rpc_socket" "$archive_pid" "$archive_user" "$origin_group" legacy-archive-rpc
  expected_pids="$expected_pids $archive_pid"
else
  test "$archive_rpc_socket" = none
fi
test -z "$(ss -H -ltnp | awk -v port="$retired_archive_rpc_port" '$4 ~ (":" port "$") {print}')"
for observed in $(pgrep -x arc-node || true); do
  case " $expected_pids " in *" $observed "*) ;; *) printf 'unowned arc-node PID %s\n' "$observed" >&2; exit 1 ;; esac
done
for expected in $expected_pids; do pgrep -x arc-node | grep -Fxq "$expected"; done
test -f "$model"
test ! -L "$model"
test "$(stat -c %s "$model")" = "$model_size"
printf '%s  %s\n' "$model_sha" "$model" | sha256sum --check --strict
"""
        receipt_rows = {
            row["node"]: row
            for row in self.manifest["provenance"]["validator_key_receipt_chain"]["validators"]
        }
        identity_cli_sha = self.manifest["artifacts"]["cli"]["sha256"]
        identity_stage = self.manifest["archive"]["prearchive_rollout_sha256"]
        for node in self.validators:
            receipt_row = receipt_rows[node["name"]]
            argv_bytes = b"\0".join(
                item.encode("utf-8") for item in self.runtime_argv(node, remote=True)
            ) + b"\0"
            self.ssh(
                node,
                script,
                (
                    node["service_name"],
                    sha256_bytes(argv_bytes),
                    f"{node['remote_root']}/arc-node",
                    node["model_path"],
                    node["model_sha256"],
                    str(node["model_size_bytes"]),
                    (
                        self.legacy_archive_service_name(node)
                        if self.legacy_archive_for(node) is not None
                        else "none"
                    ),
                    (
                        self.legacy_archive_root(node)
                        if self.legacy_archive_for(node) is not None
                        else "/var/lib/arc-legacy-archive/none"
                    ),
                    (
                        sha256_bytes(
                            b"\0".join(
                                item.encode("utf-8")
                                for item in self.legacy_archive_argv(node)
                            )
                            + b"\0"
                        )
                        if self.legacy_archive_for(node) is not None
                        else "none"
                    ),
                    str(node["p2p_port"]),
                    node["key_file"],
                    receipt_row["keyfile_sha256"],
                    receipt_row["address"],
                    f"/root/arc-recovery-seal/{identity_stage}/{node['name']}/arc-cli",
                    identity_cli_sha,
                    self.validator_rpc_socket(node),
                    node["service_user"],
                    RPC_ORIGIN_GROUP,
                    node["rpc_listen"].rsplit(":", 1)[1],
                    (
                        self.legacy_archive_rpc_socket(node)
                        if self.legacy_archive_for(node) is not None
                        else "none"
                    ),
                    LEGACY_ARCHIVE_USER,
                    str(LEGACY_ARCHIVE_RPC_PORT),
                ),
            )
        self.say(
            "PASS all six validator MainPIDs and every optional archive MainPID use exact sealed argv/artifacts with no unowned arc-node process"
        )

    def prove_legacy_archive_deployments(self) -> None:
        if not self.legacy_archive_forks:
            self.say("PASS sealed archive contains no valid noncanonical fork requiring a reader")
            return
        listener_script = r'''
set -eu
service=$1 user=$2 origin_group=$3 rpc_socket=$4 filter_socket=$5 archive_root=$6 filter_user=$7 filter_group=$8 retired_rpc_port=$9
systemctl is-active --quiet "$service"
test "$(systemctl show "$service" --property=User --value)" = "$user"
test "$(systemctl show "$service" --property=Group --value)" = "$origin_group"
pid=$(systemctl show "$service" --property=MainPID --value)
case "$pid" in ''|0|*[!0-9]*) exit 1 ;; esac
test "$(readlink "/proc/$pid/exe")" = "$archive_root/arc-node"
test -S "$rpc_socket" && test ! -L "$rpc_socket"
test "$(stat -c %U:%G:%a:%h "$rpc_socket")" = "$user:$origin_group:660:1"
rows=$(ss -H -lxnp | grep -F -- " $rpc_socket" || true)
test "$(printf '%s\n' "$rows" | awk 'NF {n+=1} END {print n+0}')" = 1
case "$rows" in *"pid=$pid,"*) ;; *) printf 'legacy archive Unix RPC listener is foreign\n' >&2; exit 1 ;; esac
runtime=${rpc_socket%/*}
test -d "$runtime" && test ! -L "$runtime"
test "$(stat -c %U:%G:%a "$runtime")" = "$user:$origin_group:750"
archive_gid=$(id -g "$user")
awk -v gid="$archive_gid" '/^Groups:/{for(i=2;i<=NF;i++) if($i==gid) found=1} END{exit found?0:1}' "/proc/$pid/status"
test -z "$(ss -H -ltnp | awk -v port="$retired_rpc_port" '$4 ~ (":" port "$") {print}')"
test -S "$filter_socket" && test ! -L "$filter_socket"
test "$(stat -c %U:%G:%a:%h "$filter_socket")" = "$filter_user:$filter_group:770:1"
filter_rows=$(ss -H -lxnp | grep -F -- " $filter_socket" || true)
test "$(printf '%s\n' "$filter_rows" | awk 'NF {n+=1} END {print n+0}')" = 1
test -z "$(ss -H -ltnp | awk '$4 ~ /:18090$/ {print}')"
'''
        for node in self.validators:
            if self.legacy_archive_for(node) is not None:
                self.ssh(
                    node,
                    listener_script,
                    (
                        self.legacy_archive_service_name(node),
                        LEGACY_ARCHIVE_USER,
                        RPC_ORIGIN_GROUP,
                        self.legacy_archive_rpc_socket(node),
                        self.filter_archive_socket(node),
                        self.legacy_archive_root(node),
                        NGINX_FILTER_USER,
                        CADDY_USER,
                        str(LEGACY_ARCHIVE_RPC_PORT),
                    ),
                )
        deployed = [
            self.legacy_archive_forks[node["name"]]
            for node in self.validators
            if node["name"] in self.legacy_archive_forks
        ]
        sources = self._legacy_archive_sources(deployed)
        if len(sources) != len(deployed):
            fail("legacy archive deployment proof omitted a sealed fork")
        self.say(
            f"PASS {len(sources)} immutable legacy fork reader(s) are GET-only, "
            "Pages-readable, and root-bound end to end"
        )

    def _rollback_production_host(
        self, node: Mapping[str, Any], baseline: Mapping[str, bool]
    ) -> dict[str, Any]:
        listener_baseline = self.production_public_listener_baseline[node["name"]]
        archive_service = (
            self.legacy_archive_service_name(node)
            if self.legacy_archive_for(node) is not None
            else "none"
        )
        output = self.ssh(
            node,
            r'''set -eu
node=$1 validator_service=$2 gateway_service=$3 filter_service=$4 archive_service=$5
inventory=$6 root=$7 digest=$8
validator_was_active=$9 validator_was_enabled=${10} gateway_was_active=${11} gateway_was_enabled=${12}
filter_was_active=${13} filter_was_enabled=${14} archive_was_active=${15} archive_was_enabled=${16}
nginx_was_active=${17} nginx_was_enabled=${18} validator_rpc_socket=${19} retired_rpc_port=${20} p2p_port=${21}
archive_filter_port=${22} archive_rpc_socket=${23} retired_archive_rpc_port=${24} expected_80_count=${25} expected_443_count=${26}
readiness_seconds=${27}
public_filter_socket=${28} archive_filter_socket=${29} filter_user=${30} filter_group=${31}
interlock_service=${32} interlock_was_active=${33} interlock_was_enabled=${34}
interlock_socket=${35} interlock_user=${36} interlock_group=${37}
validator_user=${38} archive_user=${39} origin_group=${40}
for flag in "$validator_was_active" "$validator_was_enabled" "$gateway_was_active" "$gateway_was_enabled" "$filter_was_active" "$filter_was_enabled" "$archive_was_active" "$archive_was_enabled" "$nginx_was_active" "$nginx_was_enabled" "$interlock_was_active" "$interlock_was_enabled"; do
  case "$flag" in 0|1) ;; *) printf 'invalid rollback baseline flag\n' >&2; exit 1 ;; esac
done
for value in "$retired_rpc_port" "$p2p_port" "$archive_filter_port" "$retired_archive_rpc_port" "$expected_80_count" "$expected_443_count" "$readiness_seconds"; do
  case "$value" in ''|*[!0-9]*) printf 'invalid rollback port/count\n' >&2; exit 1 ;; esac
done
if test -e "$root"; then
  test -d "$root" && test ! -L "$root"
  test -f "$root/.arc-recovery-rollout-owner" && test ! -L "$root/.arc-recovery-rollout-owner"
  test "$(cat "$root/.arc-recovery-rollout-owner")" = "$digest"
fi
if test -e "$inventory"; then
  test -f "$inventory" && test ! -L "$inventory"
  grep -Fxq "rollout_manifest_sha256=$digest" "$inventory"
  inventory_80=$(sed -n 's/^public_80_count=//p' "$inventory")
  inventory_443=$(sed -n 's/^public_443_count=//p' "$inventory")
  test "$(grep -c '^public_80_count=' "$inventory")" = 1
  test "$(grep -c '^public_443_count=' "$inventory")" = 1
  test "$inventory_80" = "$expected_80_count"
  test "$inventory_443" = "$expected_443_count"
fi
restore_unit() {
  unit=$1 was_active=$2 was_enabled=$3
  installed="/etc/systemd/system/$unit"
  if test -e "$installed"; then
    test -f "$installed" && test ! -L "$installed"
    test -f "$root/$unit" && test ! -L "$root/$unit"
    cmp --silent "$root/$unit" "$installed"
    if [ "$was_enabled" = 1 ]; then systemctl enable "$unit" >/dev/null; else systemctl disable "$unit" >/dev/null; fi
    if [ "$was_active" = 1 ]; then systemctl start "$unit" >/dev/null; else systemctl stop "$unit" >/dev/null; fi
  else
    test "$was_active" = 0 && test "$was_enabled" = 0
  fi
}
if [ "$archive_service" != none ]; then restore_unit "$archive_service" "$archive_was_active" "$archive_was_enabled"; fi
restore_unit "$filter_service" "$filter_was_active" "$filter_was_enabled"
restore_unit "$gateway_service" "$gateway_was_active" "$gateway_was_enabled"
restore_unit "$validator_service" "$validator_was_active" "$validator_was_enabled"
restore_unit "$interlock_service" "$interlock_was_active" "$interlock_was_enabled"
if [ "$nginx_was_enabled" = 1 ]; then systemctl enable nginx.service >/dev/null; else systemctl disable nginx.service >/dev/null; fi
if [ "$nginx_was_active" = 1 ]; then systemctl start nginx.service >/dev/null; else systemctl stop nginx.service >/dev/null; fi
state() { if systemctl "$1" --quiet "$2"; then printf 1; else printf 0; fi; }
assert_state() {
  unit=$1 wanted_active=$2 wanted_enabled=$3 label=$4
  active=$(state is-active "$unit"); enabled=$(state is-enabled "$unit")
  test "$active" = "$wanted_active" && test "$enabled" = "$wanted_enabled"
  printf '%s_active=%s\n%s_enabled=%s\n' "$label" "$active" "$label" "$enabled"
}
main_pid() {
  pid=$(systemctl show "$1" --property=MainPID --value)
  case "$pid" in ''|0|*[!0-9]*) return 1 ;; esac
  test -d "/proc/$pid"
  printf '%s' "$pid"
}
tcp_rows() { ss -H -ltnp | awk -v port="$1" '$4 ~ (":" port "$") { print }'; }
udp_rows() { ss -H -lunp | awk -v port="$1" '$4 ~ (":" port "$") { print }'; }
exact_tcp_ready() {
  port=$1
  expected_count=$2
  expected_pid=$3
  label=$4
  rows=$(tcp_rows "$port")
  count=$(printf '%s\n' "$rows" | awk 'NF { count += 1 } END { print count + 0 }')
  if [ "$count" -gt "$expected_count" ]; then
    printf '%s listener count is duplicated on %s\n' "$label" "$port" >&2
    return 1
  fi
  [ "$count" = "$expected_count" ] || return 2
  if [ "$expected_count" -gt 0 ]; then
    test -n "$expected_pid"
    while IFS= read -r row; do case "$row" in *"pid=$expected_pid,"*) ;; *) printf '%s listener is foreign\n' "$label" >&2; return 1 ;; esac; done <<EOF
$rows
EOF
  fi
}
exact_unix_ready() {
  socket=$1 expected_count=$2 expected_pid=$3 label=$4 expected_user=$5 expected_group=$6 expected_mode=$7
  rows=$(ss -H -lxnp | grep -F -- " $socket" || true)
  count=$(printf '%s\n' "$rows" | awk 'NF {n+=1} END {print n+0}')
  [ "$count" = "$expected_count" ] || return 2
  if [ "$expected_count" = 0 ]; then
    test ! -e "$socket" && test ! -L "$socket"
    return
  fi
  case "$rows" in *"pid=$expected_pid,"*) ;; *) return 1 ;; esac
  test -S "$socket" && test ! -L "$socket"
  test "$(stat -c %U:%G:%a:%h "$socket")" = "$expected_user:$expected_group:$expected_mode:1"
  runtime=${socket%/*}
  test -d "$runtime" && test ! -L "$runtime"
  test "$(stat -c %U:%G:%a "$runtime")" = "$expected_user:$expected_group:750"
}
validator_udp_ready() {
  rows=$(udp_rows "$p2p_port")
  count=$(printf '%s\n' "$rows" | awk 'NF { count += 1 } END { print count + 0 }')
  if [ "$validator_was_active" = 1 ]; then
    if [ "$count" -gt 1 ]; then printf 'restored QUIC listener is duplicated\n' >&2; return 1; fi
    [ "$count" = 1 ] || return 2
    case "$rows" in *"pid=$validator_pid,"*) ;; *) printf 'restored QUIC listener is foreign\n' >&2; return 1 ;; esac
  else
    [ "$count" = 0 ] || { printf 'unintended QUIC listener after rollback\n' >&2; return 1; }
  fi
}
probe_ready() {
  validator_pid="" gateway_pid="" filter_pid="" interlock_pid="" archive_pid="" nginx_pid=""
  for tuple in \
    "$validator_service:$validator_was_active:$validator_was_enabled" \
    "$gateway_service:$gateway_was_active:$gateway_was_enabled" \
    "$filter_service:$filter_was_active:$filter_was_enabled" \
    "$interlock_service:$interlock_was_active:$interlock_was_enabled" \
    "nginx.service:$nginx_was_active:$nginx_was_enabled"; do
    unit=${tuple%%:*}; rest=${tuple#*:}; wanted_active=${rest%%:*}; wanted_enabled=${rest##*:}
    [ "$(state is-enabled "$unit")" = "$wanted_enabled" ] || { printf 'restored enabled state differs: %s\n' "$unit" >&2; return 1; }
    [ "$(state is-active "$unit")" = "$wanted_active" ] || return 2
  done
  if [ "$archive_service" != none ]; then
    [ "$(state is-enabled "$archive_service")" = "$archive_was_enabled" ] || { printf 'restored archive enabled state differs\n' >&2; return 1; }
    [ "$(state is-active "$archive_service")" = "$archive_was_active" ] || return 2
  fi
  if [ "$validator_was_active" = 1 ]; then validator_pid=$(main_pid "$validator_service") || return 2; fi
  if [ "$gateway_was_active" = 1 ]; then gateway_pid=$(main_pid "$gateway_service") || return 2; fi
  if [ "$filter_was_active" = 1 ]; then filter_pid=$(main_pid "$filter_service") || return 2; fi
  if [ "$interlock_was_active" = 1 ]; then interlock_pid=$(main_pid "$interlock_service") || return 2; fi
  if [ "$archive_was_active" = 1 ]; then test "$archive_service" != none; archive_pid=$(main_pid "$archive_service") || return 2; fi
  if [ "$nginx_was_active" = 1 ]; then nginx_pid=$(main_pid nginx.service) || return 2; fi
  validator_udp_ready || return $?
  if [ "$validator_was_active" = 1 ]; then exact_unix_ready "$validator_rpc_socket" 1 "$validator_pid" validator-rpc "$validator_user" "$origin_group" 660 || return $?; else exact_unix_ready "$validator_rpc_socket" 0 "" validator-rpc "$validator_user" "$origin_group" 660 || return $?; fi
  exact_tcp_ready "$retired_rpc_port" 0 "" retired-validator-rpc-tcp || return $?
  exact_tcp_ready 18080 0 "" retired-rpc-filter-tcp || return $?
  if [ "$filter_was_active" = 1 ]; then exact_unix_ready "$public_filter_socket" 1 "$filter_pid" rpc-filter "$filter_user" "$filter_group" 770 || return $?; else exact_unix_ready "$public_filter_socket" 0 "" rpc-filter "$filter_user" "$filter_group" 770 || return $?; fi
  if [ "$interlock_was_active" = 1 ]; then exact_unix_ready "$interlock_socket" 1 "$interlock_pid" late-fork-interlock "$interlock_user" "$interlock_group" 660 || return $?; else exact_unix_ready "$interlock_socket" 0 "" late-fork-interlock "$interlock_user" "$interlock_group" 660 || return $?; fi
  [ -z "$(tcp_rows 18081)" ] || { printf 'retired interlock TCP listener is present\n' >&2; return 1; }
  if [ "$archive_was_active" = 1 ]; then
    test "$filter_was_active" = 1
    exact_unix_ready "$archive_rpc_socket" 1 "$archive_pid" legacy-archive-rpc "$archive_user" "$origin_group" 660 || return $?
    exact_tcp_ready "$archive_filter_port" 0 "" retired-legacy-archive-filter-tcp || return $?
    exact_unix_ready "$archive_filter_socket" 1 "$filter_pid" legacy-archive-filter "$filter_user" "$filter_group" 770 || return $?
  else
    if [ "$archive_rpc_socket" != none ]; then exact_unix_ready "$archive_rpc_socket" 0 "" legacy-archive-rpc "$archive_user" "$origin_group" 660 || return $?; fi
    exact_tcp_ready "$archive_filter_port" 0 "" legacy-archive-filter || return $?
    if [ "$archive_filter_socket" != none ]; then exact_unix_ready "$archive_filter_socket" 0 "" legacy-archive-filter "$filter_user" "$filter_group" 770 || return $?; fi
  fi
  exact_tcp_ready "$retired_archive_rpc_port" 0 "" retired-legacy-archive-rpc-tcp || return $?
  if [ "$gateway_was_active" = 1 ] && [ "$nginx_was_active" = 1 ]; then printf 'baseline claimed two public listener owners\n' >&2; return 1; fi
  public_pid=""
  if [ "$gateway_was_active" = 1 ]; then public_pid=$gateway_pid; fi
  if [ "$nginx_was_active" = 1 ]; then public_pid=$nginx_pid; fi
  exact_tcp_ready 80 "$expected_80_count" "$public_pid" public-http || return $?
  exact_tcp_ready 443 "$expected_443_count" "$public_pid" public-https || return $?
  current_pids="$validator_pid:$gateway_pid:$filter_pid:$interlock_pid:$archive_pid:$nginx_pid"
  return 0
}
stable_pids="" ready=false remaining=$readiness_seconds
while [ "$remaining" -gt 0 ]; do
  if probe_ready; then
    if [ -n "$stable_pids" ] && [ "$stable_pids" = "$current_pids" ]; then ready=true; break; fi
    stable_pids=$current_pids
  else
    result=$?
    [ "$result" = 2 ] || exit "$result"
    stable_pids=""
  fi
  remaining=$((remaining - 1))
  sleep 1
done
[ "$ready" = true ] || { printf 'restored services/listeners did not become stably ready before rollback timeout\n' >&2; exit 1; }
probe_ready || { printf 'restored state changed at final rollback proof boundary\n' >&2; exit 1; }
[ "$current_pids" = "$stable_pids" ] || { printf 'restored MainPID set changed at final rollback proof boundary\n' >&2; exit 1; }
expected_arc_pids=""
if [ -n "$validator_pid" ]; then expected_arc_pids="$validator_pid"; fi
if [ -n "$archive_pid" ]; then expected_arc_pids="$expected_arc_pids $archive_pid"; fi
for observed in $(pgrep -x arc-node || true); do case " $expected_arc_pids " in *" $observed "*) ;; *) printf 'unintended arc-node PID after rollback: %s\n' "$observed" >&2; exit 1 ;; esac; done
for expected in $expected_arc_pids; do pgrep -x arc-node | grep -Fxq "$expected"; done
if [ "$gateway_was_active" = 0 ] && pgrep -x caddy >/dev/null; then printf 'unintended caddy PID after rollback\n' >&2; exit 1; fi
printf 'schema=arc.recovery.production-rollback-host.v1\nnode=%s\n' "$node"
assert_state "$validator_service" "$validator_was_active" "$validator_was_enabled" validator
assert_state "$gateway_service" "$gateway_was_active" "$gateway_was_enabled" gateway
assert_state "$filter_service" "$filter_was_active" "$filter_was_enabled" filter
assert_state "$interlock_service" "$interlock_was_active" "$interlock_was_enabled" interlock
if [ "$archive_service" = none ]; then
  printf 'archive_active=0\narchive_enabled=0\n'
else
  assert_state "$archive_service" "$archive_was_active" "$archive_was_enabled" archive
fi
assert_state nginx.service "$nginx_was_active" "$nginx_was_enabled" nginx
printf 'public_80_count=%s\npublic_443_count=%s\n' "$expected_80_count" "$expected_443_count"
''',
            (
                node["name"],
                node["service_name"],
                self.gateway_service_name(node),
                self.filter_service_name(node),
                archive_service,
                f"{node['remote_root']}/pre-gateway.inventory",
                node["remote_root"],
                self.digest,
                "1" if baseline["validator_active"] else "0",
                "1" if baseline["validator_enabled"] else "0",
                "1" if baseline["gateway_active"] else "0",
                "1" if baseline["gateway_enabled"] else "0",
                "1" if baseline["filter_active"] else "0",
                "1" if baseline["filter_enabled"] else "0",
                "1" if baseline["archive_active"] else "0",
                "1" if baseline["archive_enabled"] else "0",
                "1" if baseline["nginx_active"] else "0",
                "1" if baseline["nginx_enabled"] else "0",
                self.validator_rpc_socket(node),
                node["rpc_listen"].rsplit(":", 1)[1],
                str(node["p2p_port"]),
                str(LEGACY_ARCHIVE_FILTER_PORT),
                (
                    self.legacy_archive_rpc_socket(node)
                    if self.legacy_archive_for(node) is not None
                    else "none"
                ),
                str(LEGACY_ARCHIVE_RPC_PORT),
                str(listener_baseline["80"]),
                str(listener_baseline["443"]),
                str(max(1, PRODUCTION_ROLLBACK_TIMEOUT_SECONDS - 10)),
                self.filter_public_socket(node),
                (
                    self.filter_archive_socket(node)
                    if self.legacy_archive_for(node) is not None
                    else "none"
                ),
                NGINX_FILTER_USER,
                CADDY_USER,
                self.late_fork_interlock_service_name(node),
                "1" if baseline["interlock_active"] else "0",
                "1" if baseline["interlock_enabled"] else "0",
                self.late_fork_interlock_socket(node),
                LATE_FORK_INTERLOCK_USER,
                LATE_FORK_INTERLOCK_GROUP,
                node["service_user"],
                LEGACY_ARCHIVE_USER,
                RPC_ORIGIN_GROUP,
            ),
            timeout=PRODUCTION_ROLLBACK_TIMEOUT_SECONDS,
        )
        return self._parse_rollback_proof(output, node, baseline)

    def _rollback_retired_maintenance_host(
        self,
        node: Mapping[str, Any],
        maintenance_intent_sha256: str,
    ) -> dict[str, Any]:
        """Finish the one-way handoff and prove a fail-closed maintenance edge."""

        retirement = self._retire_legacy_network_quarantine(node)
        retirement_sha = sha256_bytes(canonical_bytes(retirement))
        # This installer is explicitly in post-retirement mode: its own error
        # trap cannot restore nginx or a legacy service baseline.
        self._install_gateway_and_unit(node, retirement_committed=True)
        self._set_public_gate_config(
            node,
            target="maintenance",
            intent_sha256=maintenance_intent_sha256,
        )
        archive_service = (
            self.legacy_archive_service_name(node)
            if self.legacy_archive_for(node) is not None
            else "none"
        )
        maintenance_sha = sha256_bytes(
            self.maintenance_caddyfile(node).encode("utf-8")
        )
        output = self.ssh(
            node,
            r'''set -eu
node=$1 validator=$2 gateway=$3 filter=$4 interlock=$5 archive=$6 root=$7
maintenance_sha=$8 hostname=$9
public_filter_socket=${10} filter_user=${11} filter_group=${12}
interlock_socket=${13} interlock_user=${14} interlock_group=${15}
archive_rpc_socket=${16} archive_user=${17} origin_group=${18} retired_archive_rpc_port=${19} archive_filter_socket=${20}
systemctl stop "$validator" 2>/dev/null || true
systemctl disable "$validator" 2>/dev/null || true
systemctl stop nginx.service 2>/dev/null || true
systemctl disable nginx.service 2>/dev/null || true
systemctl enable "$interlock" "$filter" "$gateway"
systemctl start "$interlock" "$filter" "$gateway"
if [ "$archive" != none ]; then
  systemctl enable "$archive"
  systemctl start "$archive"
  archive_ready=false
  for _ in $(seq 1 60); do
    archive_pid=$(systemctl show "$archive" --property=MainPID --value)
    case "$archive_pid" in ''|0|*[!0-9]*) archive_pid="" ;; esac
    archive_rows=$(ss -H -lxnp | grep -F -- " $archive_rpc_socket" || true)
    if [ -n "$archive_pid" ] && test -S "$archive_rpc_socket" && test ! -L "$archive_rpc_socket" \
       && [ "$(printf '%s\n' "$archive_rows" | awk 'NF {n+=1} END {print n+0}')" = 1 ]; then
      case "$archive_rows" in *"pid=$archive_pid,"*) archive_ready=true; break ;; esac
    fi
    systemctl is-active --quiet "$archive" || exit 1
    sleep 1
  done
  test "$archive_ready" = true
fi
test ! -e /etc/arc-recovery/legacy-start-allowed
for retired in arc-self-heal.service arc-node.service arc-node-update.service; do
  ! systemctl is-active --quiet "$retired"
done
! systemctl is-active --quiet "$validator"
! systemctl is-enabled --quiet "$validator"
! systemctl is-active --quiet nginx.service
! systemctl is-enabled --quiet nginx.service
test -f "$root/Caddyfile.active" && test ! -L "$root/Caddyfile.active"
test "$(stat -c %U:%G:%a:%h "$root/Caddyfile.active")" = root:arc-caddy:440:1
printf '%s  %s/Caddyfile.active\n' "$maintenance_sha" "$root" | sha256sum --check --strict
gateway_pid=$(systemctl show "$gateway" --property=MainPID --value)
filter_pid=$(systemctl show "$filter" --property=MainPID --value)
interlock_pid=$(systemctl show "$interlock" --property=MainPID --value)
for pid in "$gateway_pid" "$filter_pid" "$interlock_pid"; do case "$pid" in ''|0|*[!0-9]*) exit 1 ;; esac; done
exact_tcp() {
  port=$1 pid=$2 label=$3
  rows=$(ss -H -ltnp | awk -v p=":$port" '$4 ~ (p "$") {print}')
  test "$(printf '%s\n' "$rows" | awk 'NF {n+=1} END {print n+0}')" = 1
  case "$rows" in *"pid=$pid,"*) ;; *) printf '%s listener is foreign\n' "$label" >&2; exit 1 ;; esac
}
exact_tcp 80 "$gateway_pid" public-http
exact_tcp 443 "$gateway_pid" public-https
test -S "$public_filter_socket" && test ! -L "$public_filter_socket"
test "$(stat -c %U:%G:%a:%h "$public_filter_socket")" = "$filter_user:$filter_group:770:1"
socket_rows=$(ss -H -lxnp | grep -F -- " $public_filter_socket" || true)
test "$(printf '%s\n' "$socket_rows" | awk 'NF {n+=1} END {print n+0}')" = 1
case "$socket_rows" in *"pid=$filter_pid,"*) ;; *) exit 1 ;; esac
test -z "$(ss -H -ltnp | awk '$4 ~ /:18080$/ {print}')"
test -S "$interlock_socket" && test ! -L "$interlock_socket"
test "$(stat -c %U:%G:%a:%h "$interlock_socket")" = "$interlock_user:$interlock_group:660:1"
interlock_rows=$(ss -H -lxnp | grep -F -- " $interlock_socket" || true)
test "$(printf '%s\n' "$interlock_rows" | awk 'NF {n+=1} END {print n+0}')" = 1
case "$interlock_rows" in *"pid=$interlock_pid,"*) ;; *) exit 1 ;; esac
test -z "$(ss -H -ltnp | awk '$4 ~ /:18081$/ {print}')"
test -z "$(ss -H -ltnp | awk -v port="$retired_archive_rpc_port" '$4 ~ (":" port "$") {print}')"
if [ "$archive" != none ]; then
  systemctl is-active --quiet "$archive"
  systemctl is-enabled --quiet "$archive"
  archive_pid=$(systemctl show "$archive" --property=MainPID --value)
  case "$archive_pid" in ''|0|*[!0-9]*) printf 'legacy archive has no exact MainPID\n' >&2; exit 1 ;; esac
  test -S "$archive_rpc_socket" && test ! -L "$archive_rpc_socket"
  test "$(stat -c %U:%G:%a:%h "$archive_rpc_socket")" = "$archive_user:$origin_group:660:1"
  archive_rows=$(ss -H -lxnp | grep -F -- " $archive_rpc_socket" || true)
  test "$(printf '%s\n' "$archive_rows" | awk 'NF {n+=1} END {print n+0}')" = 1
  case "$archive_rows" in *"pid=$archive_pid,"*) ;; *) printf 'legacy archive Unix RPC listener is foreign\n' >&2; exit 1 ;; esac
  archive_runtime=${archive_rpc_socket%/*}
  test -d "$archive_runtime" && test ! -L "$archive_runtime"
  test "$(stat -c %U:%G:%a "$archive_runtime")" = "$archive_user:$origin_group:750"
  test -S "$archive_filter_socket" && test ! -L "$archive_filter_socket"
  test "$(stat -c %U:%G:%a:%h "$archive_filter_socket")" = "$filter_user:$filter_group:770:1"
  archive_filter_rows=$(ss -H -lxnp | grep -F -- " $archive_filter_socket" || true)
  test "$(printf '%s\n' "$archive_filter_rows" | awk 'NF {n+=1} END {print n+0}')" = 1
  case "$archive_filter_rows" in *"pid=$filter_pid,"*) ;; *) printf 'legacy archive filter Unix listener is foreign\n' >&2; exit 1 ;; esac
else
  test "$archive_rpc_socket" = none
  test "$archive_filter_socket" = none
fi
gate=$(curl --silent --show-error --output /dev/null --write-out '%{http_code}' \
  --unix-socket "$interlock_socket" --connect-timeout 3 --max-time 10 \
  http://localhost/gate || true)
health=$(curl --silent --show-error --output /dev/null --write-out '%{http_code}' \
  --connect-timeout 3 --max-time 10 --resolve "$hostname:443:127.0.0.1" \
  "https://$hostname/health" || true)
test "$gate" = 204
test "$health" = 503
if /usr/sbin/nft list table inet arc_legacy_maintenance_v1 >/dev/null 2>&1; then exit 1; fi
! systemctl is-active --quiet arc-legacy-maintenance-fence.service
! systemctl is-enabled --quiet arc-legacy-maintenance-fence.service
printf 'schema=arc.recovery.production-retired-maintenance-host.v1\n'
printf 'node=%s\n' "$node"
printf 'validator_active=0\nvalidator_enabled=0\n'
printf 'gateway_active=1\ngateway_enabled=1\n'
printf 'filter_active=1\nfilter_enabled=1\n'
printf 'interlock_active=1\ninterlock_enabled=1\n'
if [ "$archive" = none ]; then
  printf 'archive_active=0\narchive_enabled=0\n'
else
  printf 'archive_active=1\narchive_enabled=1\n'
fi
printf 'nginx_active=0\nnginx_enabled=0\n'
printf 'public_80_count=1\npublic_443_count=1\n'
printf 'interlock_gate_status=204\nmaintenance_health_status=503\n'
printf 'legacy_start_barrier_active=1\nquarantine_retired=1\n'
''',
            (
                node["name"],
                node["service_name"],
                self.gateway_service_name(node),
                self.filter_service_name(node),
                self.late_fork_interlock_service_name(node),
                archive_service,
                node["remote_root"],
                maintenance_sha,
                node["host"],
                self.filter_public_socket(node),
                NGINX_FILTER_USER,
                CADDY_USER,
                self.late_fork_interlock_socket(node),
                LATE_FORK_INTERLOCK_USER,
                LATE_FORK_INTERLOCK_GROUP,
                (
                    self.legacy_archive_rpc_socket(node)
                    if self.legacy_archive_for(node) is not None
                    else "none"
                ),
                LEGACY_ARCHIVE_USER,
                RPC_ORIGIN_GROUP,
                str(LEGACY_ARCHIVE_RPC_PORT),
                (
                    self.filter_archive_socket(node)
                    if self.legacy_archive_for(node) is not None
                    else "none"
                ),
            ),
            timeout=300,
        )
        rows: dict[str, str] = {}
        for line in output.splitlines():
            if line.count("=") != 1:
                fail(f"{node['name']} retired maintenance proof emitted a malformed row")
            key, value = line.split("=", 1)
            if key in rows or re.fullmatch(r"[a-z][a-z0-9_]{0,63}", key) is None:
                fail(f"{node['name']} retired maintenance proof repeated a field")
            rows[key] = value
        state_fields = {
            f"{service}_{state}"
            for service in (
                "validator", "gateway", "filter", "interlock", "archive", "nginx"
            )
            for state in ("active", "enabled")
        }
        exact_fields = {
            "schema", "node", *state_fields,
            "public_80_count", "public_443_count",
            "interlock_gate_status", "maintenance_health_status",
            "legacy_start_barrier_active", "quarantine_retired",
        }
        if set(rows) != exact_fields:
            fail(f"{node['name']} retired maintenance proof fields differ")
        if (
            rows["schema"]
            != "arc.recovery.production-retired-maintenance-host.v1"
            or rows["node"] != node["name"]
            or any(rows[field] not in {"0", "1"} for field in state_fields)
            or rows["validator_active"] != "0"
            or rows["validator_enabled"] != "0"
            or rows["gateway_active"] != "1"
            or rows["gateway_enabled"] != "1"
            or rows["filter_active"] != "1"
            or rows["filter_enabled"] != "1"
            or rows["interlock_active"] != "1"
            or rows["interlock_enabled"] != "1"
            or rows["nginx_active"] != "0"
            or rows["nginx_enabled"] != "0"
            or rows["public_80_count"] != "1"
            or rows["public_443_count"] != "1"
            or rows["interlock_gate_status"] != "204"
            or rows["maintenance_health_status"] != "503"
            or rows["legacy_start_barrier_active"] != "1"
            or rows["quarantine_retired"] != "1"
        ):
            fail(f"{node['name']} retired maintenance proof is not fail-closed")
        archive_expected = "1" if archive_service != "none" else "0"
        if (
            rows["archive_active"] != archive_expected
            or rows["archive_enabled"] != archive_expected
        ):
            fail(f"{node['name']} retired maintenance archive state differs")
        return {
            "schema": rows["schema"],
            "node": rows["node"],
            "retirement_receipt_sha256": retirement_sha,
            "maintenance_intent_sha256": maintenance_intent_sha256,
            "states": {
                field: rows[field] == "1" for field in sorted(state_fields)
            },
            "public_listener_counts": {"80": 1, "443": 1},
            "checks": {
                "interlock_gate_status": 204,
                "maintenance_health_status": 503,
                "legacy_start_barrier_active": True,
                "quarantine_retired": True,
            },
        }

    def _rollback_production(self, original: BaseException) -> None:
        retired_mode = self._retirement_boundary_started()
        if retired_mode:
            self.say(
                "ROLLBACK completing the one-way quarantine handoff and exactly "
                "re-proving every host maintenance-only; legacy writers/nginx remain retired"
            )
        else:
            self.say(
                "ROLLBACK restoring and exactly re-proving every host's pre-execution "
                "service state; all data, history, artifacts, configs, and logs remain preserved"
            )
        run = self._next_rollback_run()
        self.rollback_run = run
        run_prefix = f"ROLLBACK-RUN-{run:04d}"
        header_sha256 = sha256_bytes(
            (self.rollback_journal / "HEADER.json").read_bytes()
        ) if self.rollback_journal is not None else ""
        self._rollback_journal_write(
            f"{run_prefix}-STARTED.json",
            {
                "schema": "arc.recovery.production-rollback-run.v1",
                "rollout_manifest_sha256": self.digest,
                "rollback_run": run,
                "header_sha256": header_sha256,
                "state": "started",
                "original_error_type": type(original).__name__,
                "original_error_sha256": sha256_bytes(
                    f"{type(original).__name__}:{original}".encode(
                        "utf-8", errors="replace"
                    )
                ),
            },
        )
        maintenance_intent_sha = ""
        if retired_mode:
            maintenance_intent_sha = self._rollback_journal_bind(
                "RETIREMENT-SAFE-MAINTENANCE-INTENT.json",
                {
                    "schema": "arc.recovery.retired-maintenance-intent.v1",
                    "rollout_manifest_sha256": self.digest,
                    "archive_manifest_sha256": self.manifest["archive"][
                        "archive_manifest_sha256"
                    ],
                    "capture_id": self.manifest["archive"]["capture_id"],
                    "legacy_maintenance_boundary_sha256": self.chain[
                        "legacy_maintenance_boundary_sha256"
                    ],
                    "state": "maintenance-only",
                    "legacy_restart_allowed": False,
                    "quarantine_retirement_reversible": False,
                },
            )
        results: list[dict[str, Any]] = []
        failures: list[str] = []
        for ordinal, node in enumerate(reversed(self.validators), 1):
            name = node["name"]
            baseline = self.production_service_baseline[name]
            started = {
                "schema": "arc.recovery.production-rollback-attempt.v2",
                "rollout_manifest_sha256": self.digest,
                "rollback_run": run,
                "ordinal": ordinal,
                "node": name,
                "host": node["host"],
                "state": "started",
            }
            prefix = f"{run_prefix}-ATTEMPT-{ordinal:02d}-{name.upper()}"
            try:
                self._rollback_journal_write(f"{prefix}-STARTED.json", started)
            except BaseException as error:
                failures.append(f"{name}:journal-start:{type(error).__name__}")
            try:
                proof = (
                    self._rollback_retired_maintenance_host(
                        node, maintenance_intent_sha
                    )
                    if retired_mode
                    else self._rollback_production_host(node, baseline)
                )
                result = {
                    **started,
                    "state": (
                        "retired-maintenance-and-proved"
                        if retired_mode
                        else "restored-and-proved"
                    ),
                    "proof": proof,
                    "proof_sha256": sha256_bytes(canonical_bytes(proof)),
                }
            except BaseException as error:
                failure_hash = sha256_bytes(
                    f"{type(error).__name__}:{error}".encode("utf-8", errors="replace")
                )
                result = {
                    **started,
                    "state": "incomplete",
                    "error_type": type(error).__name__,
                    "error_sha256": failure_hash,
                }
                failures.append(f"{name}:restore-or-proof:{type(error).__name__}")
            results.append(result)
            try:
                self._rollback_journal_write(f"{prefix}-RESULT.json", result)
            except BaseException as error:
                failures.append(f"{name}:journal-result:{type(error).__name__}")
        receipt = {
            "schema": (
                "arc.recovery.production-retired-maintenance-receipt.v1"
                if retired_mode
                else "arc.recovery.production-rollback-receipt.v2"
            ),
            "rollout_manifest_sha256": self.digest,
            "rollback_run": run,
            "original_error_type": type(original).__name__,
            "original_error_sha256": sha256_bytes(
                f"{type(original).__name__}:{original}".encode("utf-8", errors="replace")
            ),
            "complete": not failures,
            "preservation_policy": (
                "data-history-artifacts-preserved-maintenance-only-no-legacy-restart"
                if retired_mode
                else "data-history-artifacts-configs-logs-preserved-no-deletion"
            ),
            "header_sha256": header_sha256,
            "results": results,
        }
        if retired_mode:
            receipt["maintenance_intent_sha256"] = maintenance_intent_sha
            receipt["rollback_mode"] = "retired-maintenance-safe"
        if failures:
            try:
                self._rollback_journal_write(f"{run_prefix}-FAILED.json", receipt)
            except BaseException as error:
                failures.append(f"failed-receipt:{type(error).__name__}")
            raise RolloutError(
                "EMERGENCY_ROLLBACK_INCOMPLETE: "
                + ", ".join(failures)
                + "; preserve all data/history/artifacts/configs/logs; no deletion or new rollout"
            ) from original
        self._rollback_journal_write("ROLLBACK-RECEIPT.json", receipt)
        self.rollback_journal_state = "rolled-back"
        self.say(
            "PASS durable rollback receipt proves all six hosts maintenance-only"
            if retired_mode
            else "PASS durable rollback receipt proves all six hosts restored exactly"
        )

    def _resume_existing_rollback_journal(self) -> bool:
        """Handle a previously fsynced transaction without forward resumption.

        Returns True only when a strict prior SUCCESS receipt proves the whole
        rollout already completed.  Every nonterminal journal is restored from
        its original v2 HEADER baselines before any new forward operation.
        """
        state = self.reserve_rollback_journal()
        if state == "success":
            self.say("PASS existing durable SUCCESS receipt proves this exact production rollout is complete")
            return True
        if state == "rolled-back":
            fail(
                "existing rollback journal proves the original fleet was restored; "
                "forward execution requires a new create-only --rollback-journal"
            )
        if state != "resume-rollback":
            return False
        self.configure_production_transport()
        interrupted = RolloutError(
            "interrupted production transaction recovered from its durable journal"
        )
        self._rollback_production(interrupted)
        fail(
            "interrupted production transaction was restored exactly; forward "
            "execution is forbidden until a new create-only rollback journal is authorized"
        )

    def _write_production_success_receipt(self) -> None:
        if self.rollback_journal is None:
            fail("production success receipt requires the reserved rollback journal")
        header_sha256 = sha256_bytes(
            (self.rollback_journal / "HEADER.json").read_bytes()
        )
        if self.public_gate_receipt_sha256 is None:
            fail("production success requires the journaled all-six public gate receipt")
        retirement_path = self.rollback_journal / "QUARANTINE-RETIREMENT-RECEIPT.json"
        if not retirement_path.is_file() or retirement_path.is_symlink():
            fail("production success requires the journaled all-six quarantine retirement")
        gateway_security_path = self.rollback_journal / "GATEWAY-SECURITY-RECEIPT.json"
        if not gateway_security_path.is_file() or gateway_security_path.is_symlink():
            fail("production success requires the journaled all-six gateway security proof")
        tls_preflight_path = (
            self.rollback_journal / "PUBLIC-TLS-PREFLIGHT-EVIDENCE.json"
        )
        tls_post_rollout_path = (
            self.rollback_journal / "PUBLIC-TLS-POST-ROLLOUT-EVIDENCE.json"
        )
        for label, path in (
            ("preflight", tls_preflight_path),
            ("post-rollout", tls_post_rollout_path),
        ):
            if not path.is_file() or path.is_symlink():
                fail(
                    "production success requires the journaled all-six "
                    f"{label} TLS evidence"
                )
        receipt = {
            "schema": "arc.recovery.production-rollout-success.v2",
            "rollout_manifest_sha256": self.digest,
            "archive_manifest_sha256": self.manifest["archive"]["archive_manifest_sha256"],
            "freeze_plan_sha256": self.manifest["archive"]["freeze_plan_sha256"],
            "capture_id": self.manifest["archive"]["capture_id"],
            "complete": True,
            "preservation_policy": "data-history-artifacts-configs-logs-preserved-no-deletion",
            "header_sha256": header_sha256,
            "public_gate_open_receipt_sha256": self.public_gate_receipt_sha256,
            "quarantine_retirement_receipt_sha256": sha256_bytes(
                retirement_path.read_bytes()
            ),
            "gateway_security_receipt_sha256": sha256_bytes(
                gateway_security_path.read_bytes()
            ),
            "public_tls_preflight_evidence_sha256": sha256_bytes(
                tls_preflight_path.read_bytes()
            ),
            "public_tls_post_rollout_evidence_sha256": sha256_bytes(
                tls_post_rollout_path.read_bytes()
            ),
            "validators": [
                {
                    "node": node["name"],
                    "host": node["host"],
                    "state": "enabled-running-proved",
                }
                for node in self.validators
            ],
        }
        self._rollback_journal_write("SUCCESS-RECEIPT.json", receipt)
        self.rollback_journal_state = "success"

    def execute_production(self) -> None:
        # Re-hash every executing rollout/archive component after authorization
        # and immediately before the first remote mutation.
        self.verify_execution_provenance()
        if (
            self.rollback_journal is not None
            and (self.rollback_journal.exists() or self.rollback_journal.is_symlink())
            and self._resume_existing_rollback_journal()
        ):
            return
        if not self.archive_metadata_loaded:
            fail("production execute requires preflight-loaded archive metadata")
        if set(self.production_service_baseline) != {
            node["name"] for node in self.validators
        }:
            fail("production execute requires a complete read-only service baseline")
        live_archive_hash = self.verify_production_archive(verify_live_captures=True)
        if live_archive_hash != self.manifest["archive"]["archive_manifest_sha256"]:
            fail("live capture verification returned a different archive root")
        # The complete Drive/live-capture verifier can be long. Re-run the
        # lightweight host/DNS/model/listener checks and replace the service
        # baseline immediately afterward so rollback never restores stale
        # activity observed hours before the first mutation.
        self.production_service_baseline.clear()
        self.production_public_listener_baseline.clear()
        self._preflight_production()
        if set(self.production_service_baseline) != {
            node["name"] for node in self.validators
        }:
            fail("production execute could not refresh every service baseline")
        if set(self.production_public_listener_baseline) != {
            node["name"] for node in self.validators
        }:
            fail("production execute could not refresh every public-listener baseline")
        state = self.reserve_rollback_journal()
        if state != "forward":
            fail("production rollback journal did not enter the forward state")
        try:
            self._rollback_journal_event(1, "STAGE", "STARTED")
            for node in self.validators:
                self._stage_production_node(node)
            self._rollback_journal_event(1, "STAGE", "COMPLETE")
            self._rollback_journal_event(2, "INTERLOCK-INSTALL", "STARTED")
            for node in self.validators:
                self._install_late_fork_interlock(node)
            self._rollback_journal_event(2, "INTERLOCK-INSTALL", "COMPLETE")
            # This is the irreversible network ownership handoff.  The exact
            # maintenance monitor is live first; then every host removes only
            # the capture-owned nft table while permanently retaining both
            # legacy restart barriers.  From STARTED onward, rollback is
            # maintenance-only and may never restore the v0.7 public edge.
            self._rollback_journal_event(3, "QUARANTINE-RETIRE", "STARTED")
            retirement_receipts: dict[str, dict[str, Any]] = {}
            with concurrent.futures.ThreadPoolExecutor(
                max_workers=REQUIRED_VALIDATORS
            ) as pool:
                futures = {
                    pool.submit(self._retire_legacy_network_quarantine, node): node
                    for node in self.validators
                }
                for future, node in futures.items():
                    receipt = future.result()
                    retirement_receipts[node["name"]] = receipt
                    self._rollback_journal_write(
                        f"QUARANTINE-RETIRE-{node['name'].upper()}.json", receipt
                    )
            if set(retirement_receipts) != {
                node["name"] for node in self.validators
            }:
                fail("legacy quarantine retirement omitted a fixed fleet host")
            retirement_fleet = {
                "schema": "arc.recovery.legacy-network-quarantine-retirement-fleet.v1",
                "rollout_manifest_sha256": self.digest,
                "archive_manifest_sha256": self.manifest["archive"][
                    "archive_manifest_sha256"
                ],
                "capture_id": self.manifest["archive"]["capture_id"],
                "rollback_policy": "maintenance-only-no-legacy-restart",
                "nodes": [
                    {
                        "node": node["name"],
                        "host": node["host"],
                        "receipt_sha256": sha256_bytes(
                            canonical_bytes(retirement_receipts[node["name"]])
                        ),
                    }
                    for node in self.validators
                ],
            }
            self._rollback_journal_write(
                "QUARANTINE-RETIREMENT-RECEIPT.json", retirement_fleet
            )
            self._rollback_journal_event(3, "QUARANTINE-RETIRE", "COMPLETE")
            self._rollback_journal_event(4, "GATEWAY-INSTALL", "STARTED")
            for node in self.validators:
                self._install_gateway_and_unit(node, retirement_committed=True)
            if set(self.production_gateway_security_receipts) != {
                node["name"] for node in self.validators
            }:
                fail("gateway security boundary omitted a fixed fleet host")
            self._rollback_journal_write(
                "GATEWAY-SECURITY-RECEIPT.json",
                {
                    "schema": "arc.recovery.gateway-security-fleet.v1",
                    "rollout_manifest_sha256": self.digest,
                    "nginx_package_version": NGINX_PACKAGE_VERSION,
                    "nginx_binary_sha256": NGINX_LINUX_AMD64_SHA256,
                    "caddy_version": CADDY_VERSION,
                    "caddy_binary_sha256": CADDY_LINUX_AMD64_SHA256,
                    "caddy_admin_disabled": True,
                    "caddy_forward_auth_handlers": 0,
                    "nodes": [
                        {
                            "node": node["name"],
                            "host": node["host"],
                            "receipt_sha256": sha256_bytes(
                                canonical_bytes(
                                    self.production_gateway_security_receipts[
                                        node["name"]
                                    ]
                                )
                            ),
                        }
                        for node in self.validators
                    ],
                },
            )
            # This is the final TLS boundary before any v3 process starts.
            # A fresh direct-IP handshake must validate against the public CA
            # store, match the exact IPv4 SAN, and leave at least 48 hours on
            # the <=160-hour short-lived leaf.  It deliberately does not claim
            # that certificate renewal was observed during this rollout.
            self._prove_public_tls_fleet(phase="preflight")
            self._rollback_journal_event(4, "GATEWAY-INSTALL", "COMPLETE")
            for node in self.validators:
                self._prove_maintenance_gateway_contract(node)
            self.say("PASS all six public TLS edges remain maintenance-only before v3 catch-up")
            first_quorum = self.validators[:REQUIRED_APPROVALS]
            self._rollback_journal_event(5, "QUORUM-START", "STARTED")
            with concurrent.futures.ThreadPoolExecutor(max_workers=REQUIRED_APPROVALS) as pool:
                futures = [pool.submit(self.production_service, node, "start") for node in first_quorum]
                for future in futures:
                    future.result()
            self._rollback_journal_event(5, "QUORUM-START", "COMPLETE")
            self._rollback_journal_event(6, "SIXTH-START", "STARTED")
            self.production_service(self.validators[-1], "start")
            self._rollback_journal_event(6, "SIXTH-START", "COMPLETE")
            self.wait_nodes_ready()
            self.prove_production_runtime_inventory()
            for node in self.validators:
                self._prove_production_listener(node, public_contract=False)
            self.say("PASS every arc-node RPC listener is loopback-only behind a maintenance TLS edge")
            self.prove_production_shard_topology()
            self.prove_boundary()
            self.prove_advancing_convergence()
            for restart_index, node in enumerate(self.validators, 10):
                before = self.wait_convergence()[0]
                self._rollback_journal_event(
                    restart_index, "RESTART", "STARTED", node=node
                )
                self.production_service(node, "restart")
                self.wait_nodes_ready(timeout=self.checks["restart_timeout_seconds"])
                self.prove_production_runtime_inventory()
                after = self.wait_convergence(
                    minimum_height=before + self.checks["min_height_advance"],
                    timeout=self.checks["restart_timeout_seconds"],
                )
                self._rollback_journal_event(
                    restart_index, "RESTART", "COMPLETE", node=node
                )
                self.say(f"PASS {node['name']} production restart; fleet advanced #{before} -> #{after[0]}")
            promotion_initial, promotion_final = self.prove_advancing_convergence()
            self.verify_production_archive(verify_live_captures=True)
            self._rollback_journal_event(30, "PUBLIC-OPEN", "STARTED")
            self.open_public_gate(promotion_initial, promotion_final)
            self._rollback_journal_event(30, "PUBLIC-OPEN", "COMPLETE")
            for node in self.validators:
                self._prove_production_listener(node, public_contract=True)
            self._prove_public_tls_fleet(phase="post-rollout")
            self.say("PASS all public sources use verified HTTPS and exact live route allowlists")
            self.prove_legacy_archive_deployments()
            self.prove_reward_policy()
            self._rollback_journal_event(20, "REWARD-PROOF", "STARTED")
            resumed_evidence = self.existing_reward_evidence is not None
            evidence = self.prove_or_resume_two_reward_receipts()
            if evidence and not resumed_evidence:
                self.persist_reward_evidence(evidence)
            self._rollback_journal_event(20, "REWARD-PROOF", "COMPLETE")
            self.prove_visible_height_continuity()
        except BaseException as original:
            rollback_cause: BaseException = original
            if self.public_gate_intent_sha256 is not None:
                try:
                    self.close_public_gate(self.public_gate_intent_sha256)
                except BaseException as close_error:
                    rollback_cause = RolloutError(
                        f"{original}; public maintenance reclose failed: {close_error}"
                    )
            self._rollback_production(rollback_cause)
            if rollback_cause is not original:
                raise rollback_cause from original
            raise
        # SUCCESS is the terminal commit record.  It is intentionally outside
        # the rollback-catching region: once its create-only fsynced bytes
        # exist, this transaction must never subsequently restore the old
        # fleet and leave contradictory terminal claims.
        self._write_production_success_receipt()
        self.say("COMPLETE production rollout; all six v3 validators remain enabled and running")

    def execute(self) -> None:
        if self.manifest["mode"] == "local":
            self.execute_local()
        else:
            self.execute_production()


def parse_evidence_file(
    path: Path, expected_rollout_sha256: str
) -> list[ReceiptEvidence]:
    try:
        payload = path.read_bytes()
    except OSError as error:
        fail(f"cannot read reward evidence: {error}")
    return parse_reward_evidence_payload(payload, expected_rollout_sha256)


def execution_authorization(
    manifest: Mapping[str, Any], digest: str, archive_manifest_sha256: str | None = None
) -> str:
    if manifest["mode"] == "production":
        if archive_manifest_sha256 is None or not LOWER_HEX_32_RE.fullmatch(archive_manifest_sha256):
            fail("production authorization requires the verified archive-manifest sha256")
        archive = manifest["archive"]
        destination_sha = sha256_bytes(archive["destination"].encode())
        policy = "UNBOUND" if archive["allow_unbound_legacy_wal"] else "BOUND"
        return (
            f"GO {digest} FREEZE {archive['freeze_plan_sha256']} "
            f"CAPTURE {archive['capture_id']} ARCHIVE {archive_manifest_sha256} "
            f"DEST {destination_sha} LEGACY_WAL {policy}"
        )
    return f"GO {digest}"


def require_go(
    manifest: Mapping[str, Any],
    digest: str,
    supplied_hash: str | None,
    supplied_archive_hash: str | None,
    verified_archive_hash: str | None,
) -> None:
    if supplied_hash != digest:
        fail(f"--go-hash must exactly equal the locked rollout sha256 {digest}")
    if manifest["mode"] == "production":
        if supplied_archive_hash != verified_archive_hash:
            fail(
                "--archive-manifest-sha256 must exactly equal the fully verified remote archive"
            )
    elif supplied_archive_hash is not None:
        fail("local rehearsals must not supply --archive-manifest-sha256")
    expected = execution_authorization(manifest, digest, verified_archive_hash)
    if os.environ.get("ARC_RECOVERY_GO") != expected:
        fail(f"execution requires ARC_RECOVERY_GO={expected!r}")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    seal = subparsers.add_parser("seal", help="canonicalize and create a read-only manifest plus SHA256 sidecar")
    seal.add_argument("--draft", required=True, type=Path)
    seal.add_argument("--output", required=True, type=Path)
    run = subparsers.add_parser("run", help="read-only plan/preflight by default; execute only behind the exact GO hash")
    run.add_argument("--manifest", required=True, type=Path)
    run.add_argument("--execute", action="store_true")
    run.add_argument("--go-hash")
    run.add_argument("--archive-manifest-sha256")
    run.add_argument(
        "--rollback-journal",
        type=Path,
        help="create-only absolute directory for the fsynced per-host rollback transaction",
    )
    run.add_argument(
        "--reward-evidence-output",
        type=Path,
        help="create-only output for the two rollout-proven mined 0x25 receipts",
    )
    verify = subparsers.add_parser("verify", help="read-only live convergence and optional mined-reward verification")
    verify.add_argument("--manifest", required=True, type=Path)
    verify.add_argument("--reward-evidence", type=Path)
    frontend = subparsers.add_parser(
        "frontend-config",
        help="derive a create-only recovered frontend config from the sealed production manifest",
    )
    frontend.add_argument("--manifest", required=True, type=Path)
    frontend.add_argument("--output", required=True, type=Path)
    frontend.add_argument(
        "--reward-evidence",
        type=Path,
        help="JSON file containing the same two distinct mined 0x25 receipts proven at rollout",
    )
    frontend.add_argument(
        "--archive-manifest",
        type=Path,
        help="optional local read-only ARCHIVE-MANIFEST.json cache; omit both archive flags to fully verify/fetch from sealed Drive",
    )
    frontend.add_argument(
        "--archive-complete",
        type=Path,
        help="optional paired local read-only COMPLETE.json cache",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        if args.command == "seal":
            digest = seal_manifest(args.draft, args.output)
            manifest, _ = load_sealed_manifest(args.output)
            print(f"SEALED {args.output} sha256={digest}")
            if manifest["mode"] == "production":
                print("Execution remains locked until the bound remote archive is complete and fully verified; run the read-only plan next.")
            else:
                authorization = execution_authorization(manifest, digest)
                print(f"Execution authorization: ARC_RECOVERY_GO='{authorization}' --go-hash {digest}")
            return 0
        manifest, digest = load_sealed_manifest(args.manifest)
        rollout = RecoveryRollout(
            manifest,
            digest,
            reward_evidence_output=getattr(args, "reward_evidence_output", None),
            rollback_journal=getattr(args, "rollback_journal", None),
        )
        if args.command == "run":
            if manifest["mode"] == "production":
                if args.rollback_journal is None or not args.rollback_journal.is_absolute():
                    fail("production run requires an absolute --rollback-journal create-only directory")
            elif args.rollback_journal is not None:
                fail("local rehearsal must not supply --rollback-journal")
        if (
            args.command == "run"
            and manifest["checks"]["reward"]["mode"] == "receipt"
            and args.reward_evidence_output is None
        ):
            fail("receipt-mode rollout requires --reward-evidence-output before preflight or mutation")
        if args.command == "frontend-config":
            evidence = (
                parse_evidence_file(args.reward_evidence, digest)
                if args.reward_evidence is not None
                else None
            )
            config_digest = write_frontend_config(
                rollout,
                args.output,
                args.archive_manifest,
                args.archive_complete,
                evidence,
            )
            print(
                f"FRONTEND CONFIG {args.output} sha256={config_digest} "
                f"rollout_sha256={digest}"
            )
            return 0
        if (
            args.command == "run"
            and args.execute
            and manifest["mode"] == "production"
            and args.rollback_journal is not None
            and (args.rollback_journal.exists() or args.rollback_journal.is_symlink())
        ):
            # Crash recovery cannot depend on a normal production preflight:
            # that preflight may correctly reject the deliberately partial
            # forward state left by SIGKILL or power loss.  Authenticate the
            # exact sealed transaction and GO tuple, then use only the original
            # baselines fsynced in the existing journal HEADER.
            sealed_archive_hash = manifest["archive"]["archive_manifest_sha256"]
            require_go(
                manifest,
                digest,
                args.go_hash,
                args.archive_manifest_sha256,
                sealed_archive_hash,
            )
            rollout.verify_execution_provenance()
            if rollout._resume_existing_rollback_journal():
                return 0
            fail("existing rollback journal did not enter a terminal recovery state")
        if args.command == "verify":
            evidence = (
                parse_evidence_file(args.reward_evidence, digest)
                if args.reward_evidence
                else None
            )
            if evidence is not None and manifest["checks"]["reward"]["mode"] != "receipt":
                fail("--reward-evidence requires a sealed receipt-mode manifest with an expected base reward")
            rollout.verify_live(evidence)
            print(f"VERIFIED locked rollout sha256={digest}")
            return 0
        rollout.describe_plan()
        archive_manifest_sha256 = rollout.preflight()
        if not args.execute:
            authorization = execution_authorization(manifest, digest, archive_manifest_sha256)
            print("PLAN ONLY: no directory, process, service, package, proxy, certificate, or remote file was changed")
            archive_arg = (
                f" --archive-manifest-sha256 {archive_manifest_sha256}"
                if archive_manifest_sha256 is not None
                else ""
            )
            evidence_output_arg = (
                f" --reward-evidence-output {shlex.quote(str(args.reward_evidence_output))}"
                if args.reward_evidence_output is not None
                else ""
            )
            rollback_arg = (
                f" --rollback-journal {shlex.quote(str(args.rollback_journal))}"
                if args.rollback_journal is not None
                else ""
            )
            print(f"To execute this exact plan: ARC_RECOVERY_GO='{authorization}' {Path(sys.argv[0]).name} run --manifest {shlex.quote(str(args.manifest))} --execute --go-hash {digest}{archive_arg}{evidence_output_arg}{rollback_arg}")
            return 0
        require_go(
            manifest,
            digest,
            args.go_hash,
            args.archive_manifest_sha256,
            archive_manifest_sha256,
        )
        # Execute-only: reserve and fsync both create-only evidence outputs
        # before the first local or remote mutation. Plan-only remains read-only.
        rollout.reserve_reward_evidence_output()
        rollout.execute()
        return 0
    except RolloutError as error:
        print(f"recovery rollout: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
