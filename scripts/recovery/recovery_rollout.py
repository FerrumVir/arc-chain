#!/usr/bin/env python3
"""Content-addressed six-validator ARC recovery rehearsal and rollout.

The command is deliberately read-only unless ``run --execute`` is supplied
with two exact copies of the sealed rollout-manifest hash.  It uses only the
Python standard library so the same artifact can be audited and run on a clean
operator workstation.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import ipaddress
import json
import os
import re
import shlex
import signal
import socket
import ssl
import stat
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
HEX_32_RE = re.compile(r"^(?:0x)?[0-9a-f]{64}$")
LOWER_HEX_32_RE = re.compile(r"^[0-9a-f]{64}$")
SAFE_ID_RE = re.compile(r"^[a-z][a-z0-9-]{0,62}$")
SAFE_REMOTE_RE = re.compile(r"^[A-Za-z0-9_./:@%+=,-]+$")
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
CANONICAL_MODEL_SHA256 = "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa"
CANONICAL_MODEL_SIZE_BYTES = 4_081_004_224
CANONICAL_MODEL_LAYERS = 32
REQUIRED_SHARD_REPLICATION = 3
REQUIRED_LAYERS_PER_VALIDATOR = (
    CANONICAL_MODEL_LAYERS * REQUIRED_SHARD_REPLICATION // REQUIRED_VALIDATORS
)
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
DEFAULT_PUBLIC_POST_PATHS = (
    "/inference/run",
    "/inference/run_consensus",
    "/community/register",
    "/community/heartbeat",
    "/community/claim_work",
    "/community/submit_work",
    "/tx/submit_signed",
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
    "/tx/submit",
    "/community/reward_approval/{job_id}",
    "/eth",
)
PUBLIC_INFERENCE_TIMEOUT_SECONDS = 4000
VALIDATOR_APPROVAL_TIMEOUT_SECONDS = 1500
WORKER_SUBMIT_TIMEOUT_SECONDS = 2700
CAPTURE_DOMAIN = b"ARC recovery capture v2\0"
ARCHIVE_FINALIZATION_FIELDS = (
    "complete_sha256",
    "archive_manifest_sha256",
    "sha256sums_sha256",
    "prearchive_rollout_sha256",
)


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


def paths_overlap(left: str, right: str) -> bool:
    common = os.path.commonpath((left, right))
    return common in {left, right}


def validate_artifact(value: Any, field: str) -> dict[str, Any]:
    artifact = require_keys(value, field, ("path", "sha256"))
    absolute_path(artifact["path"], f"{field}.path")
    if not isinstance(artifact["sha256"], str) or not LOWER_HEX_32_RE.fullmatch(artifact["sha256"]):
        fail(f"{field}.sha256 must be exactly 64 lowercase hexadecimal characters")
    return artifact


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


def embedded_ip_hostname(hostname: str) -> str | None:
    for suffix in (".nip.io", ".sslip.io"):
        if hostname.endswith(suffix):
            prefix = hostname[: -len(suffix)]
            for candidate in (prefix, prefix.replace("-", ".")):
                try:
                    return str(ipaddress.ip_address(candidate))
                except ValueError:
                    pass
    return None


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


def validate_manifest(value: Any) -> dict[str, Any]:
    manifest = require_keys(
        value,
        "manifest",
        ("schema", "rollout_id", "mode", "chain", "artifacts", "checks", "gateway", "validators"),
        ("archive",),
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
    elif "archive" in manifest:
        fail("local rehearsals must not contain manifest.archive")

    chain = require_keys(
        manifest["chain"],
        "manifest.chain",
        (
            "chain_id",
            "genesis_hash",
            "protocol_version",
            "recovery_epoch",
            "validator_set_id",
            "source_height",
            "source_consensus_round",
            "created_at_unix_ms",
            "source_block_hash",
            "source_state_root",
            "transition_height",
            "transition_block_hash",
            "full_state_root",
            "recovery_domain",
            "approved_checkpoint_manifest_hash",
        ),
    )
    required_string(chain["chain_id"], "manifest.chain.chain_id")
    version = required_string(chain["protocol_version"], "manifest.chain.protocol_version")
    if not re.fullmatch(r"3\.\d+\.\d+", version):
        fail("manifest.chain.protocol_version must be protocol v3")
    required_int(chain["recovery_epoch"], "manifest.chain.recovery_epoch", minimum=1)
    required_int(chain["validator_set_id"], "manifest.chain.validator_set_id", minimum=1)
    source_height = required_int(chain["source_height"], "manifest.chain.source_height")
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
        *(("source_snapshot", "source_wal", "caddy") if mode == "production" else ()),
    )
    artifacts = require_keys(manifest["artifacts"], "manifest.artifacts", artifact_names)
    for key in artifact_names:
        validate_artifact(artifacts[key], f"manifest.artifacts.{key}")

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
        ("probe_argv", "probe_sha256", "tx_hash", "job_id", "worker", "expected_reward_base"),
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
        fixed = all(key in reward for key in ("tx_hash", "job_id", "worker"))
        probe = "probe_argv" in reward
        if fixed == probe:
            fail("receipt mode requires exactly one of fixed tx/job/worker evidence or probe_argv")
        if fixed:
            for key in ("tx_hash", "job_id", "worker"):
                bare_hash(reward[key], f"manifest.checks.reward.{key}")
        else:
            argv = reward["probe_argv"]
            if not isinstance(argv, list) or not argv or not all(isinstance(arg, str) and arg for arg in argv):
                fail("manifest.checks.reward.probe_argv must be a non-empty string array")
            absolute_path(argv[0], "manifest.checks.reward.probe_argv[0]")
            if not isinstance(reward.get("probe_sha256"), str) or not LOWER_HEX_32_RE.fullmatch(reward["probe_sha256"]):
                fail("manifest.checks.reward.probe_sha256 must pin the executable probe")
        required_int(reward.get("expected_reward_base"), "manifest.checks.reward.expected_reward_base", minimum=1)
    else:
        forbidden = set(reward) & {"probe_argv", "probe_sha256", "tx_hash", "job_id", "worker", "expected_reward_base"}
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
            "public_hostname",
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
            if advertise_host != host:
                fail(f"{field}.p2p_advertise host must equal the production host")
            for key in ("ssh_user", "service_user"):
                token = required_string(node[key], f"{field}.{key}")
                if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_-]{0,31}", token):
                    fail(f"{field}.{key} is unsafe")
            if node["ssh_user"] != "root" or node["service_user"] != "root":
                fail(f"{field} currently requires audited root SSH/service ownership for low ports and mode-0600 keys")
            remote_root = absolute_path(node["remote_root"], f"{field}.remote_root")
            if not SAFE_REMOTE_RE.fullmatch(remote_root):
                fail(f"{field}.remote_root is unsafe")
            remote_roots.add(remote_root)
            if paths_overlap(remote_root, data_dir):
                fail(f"{field}.remote_root and data_dir must be disjoint, non-nested paths")
            service_name = required_string(node["service_name"], f"{field}.service_name")
            if not re.fullmatch(r"arc-node-v3-[a-z0-9-]+\.service", service_name):
                fail(f"{field}.service_name must be an arc-node-v3-*.service")
            service_names.add(service_name)
            public_hostname = required_string(node["public_hostname"], f"{field}.public_hostname")
            if not public_hostname.endswith((".nip.io", ".sslip.io")) or not SAFE_HOST_RE.fullmatch(public_hostname):
                fail(f"{field}.public_hostname must be an IP-derived nip.io or sslip.io hostname")
            try:
                approved_ip = str(ipaddress.ip_address(host))
            except ValueError:
                fail(f"{field}.host must be the validator's literal public IP")
            if embedded_ip_hostname(public_hostname) != approved_ip:
                fail(f"{field}.public_hostname must embed the exact approved host IP")
            if rpc_url != f"https://{public_hostname}":
                fail(f"{field}.rpc_url must be exactly https://{public_hostname}")
            absolute_path(node["model_path"], f"{field}.model_path")
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
            if held_layers != REQUIRED_LAYERS_PER_VALIDATOR:
                fail(
                    f"{field}.shard_ranges must hold exactly {REQUIRED_LAYERS_PER_VALIDATOR} layers, found {held_layers}"
                )
            for start, end in normalized_ranges:
                for layer in range(start, end):
                    shard_coverage[layer] += 1

    total_stake = sum(stakes)
    if any((total_stake - stake) * 3 <= total_stake * 2 for stake in stakes):
        fail("validator stakes do not preserve strict >2/3 quorum during every one-node restart")
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
    except (OSError, json.JSONDecodeError) as error:
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


def load_sealed_manifest(path: Path) -> tuple[dict[str, Any], str]:
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
    manifest = validate_manifest(parsed)
    if payload != canonical_bytes(manifest):
        fail("sealed manifest is not canonical JSON; reseal the reviewed draft")
    digest = sha256_bytes(payload)
    expected_sidecar = f"{digest}  {path.name}\n"
    if sidecar.read_text(encoding="ascii") != expected_sidecar:
        fail("sealed manifest checksum sidecar is missing or does not match")
    return manifest, digest


def write_frontend_config(rollout: "RecoveryRollout", output_path: Path) -> str:
    if output_path.suffix != ".json":
        fail("frontend config output must end in .json")
    payload = canonical_bytes(rollout.frontend_config())
    digest = sha256_bytes(payload)
    sidecar = output_path.with_name(output_path.name + ".sha256")
    if output_path.exists() or sidecar.exists():
        fail("frontend config or checksum already exists; refusing replacement")
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


@dataclass(frozen=True)
class ReceiptEvidence:
    tx_hash: str
    job_id: str
    worker: str

    @classmethod
    def from_value(cls, value: Any) -> "ReceiptEvidence":
        body = require_keys(value, "reward evidence", ("tx_hash", "job_id", "worker"))
        return cls(*(f"0x{bare_hash(body[key], f'reward evidence.{key}')}" for key in ("tx_hash", "job_id", "worker")))


class RecoveryRollout:
    def __init__(self, manifest: dict[str, Any], digest: str, *, output: Any = sys.stdout) -> None:
        self.manifest = manifest
        self.digest = digest
        self.output = output
        self.processes: dict[str, subprocess.Popen[str]] = {}
        self.logs: dict[str, Any] = {}
        self.started_production: set[str] = set()
        self.prepared_production: set[str] = set()

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

    def frontend_config(self) -> dict[str, Any]:
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
        primary = sources[0]["id"]
        chain = self.chain
        return {
            "schema": "arc.frontend.network.v1",
            "state": "recovered",
            "network": {"name": "ARC Testnet", "chainId": chain["chain_id"]},
            "checkpoint": {
                "height": chain["source_height"],
                "recoveryHeight": chain["transition_height"],
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
            "services": {},
            "notices": [
                "Recovered protocol-v3 network; every listed validator serves the retained canonical history and H+1 continuation."
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
        return [
            binary,
            "--rpc",
            node["rpc_listen"],
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
        self.say(f"  validators:                 {len(self.validators)} (restart quorum proven in manifest)")
        self.say(
            "  reward gate:                "
            f"{reward['mode']} / protocol_active={str(reward['expect_protocol_active']).lower()} "
            f"/ issuance_ready={str(reward['expect_issuance_ready']).lower()}"
        )
        if self.manifest["mode"] == "production":
            self.say("  public RPC:                 pinned Caddy HTTPS nip.io/sslip.io gateways; node listeners stay loopback")
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
        archive = self.manifest["archive"]
        if any(archive[field] == "0" * 64 for field in ARCHIVE_FINALIZATION_FIELDS):
            fail("production rollout execution requires a roots-only finalized archive manifest")
        self.verify_execution_provenance()
        verifier = Path(__file__).resolve().parent / "archive-fleet-to-drive.sh"
        command = [
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
        output = run_checked(
            command,
            timeout=24 * 60 * 60,
        ).stdout
        match = re.search(r"archive_manifest=([0-9a-f]{64})(?:\s|$)", output)
        if match is None:
            fail("archive verifier did not emit a canonical archive-manifest hash")
        self.say(
            f"PASS complete remote archive and every SHA-256-bound object ({match.group(1)})"
        )
        return match.group(1)

    def preflight(self) -> str | None:
        self.verify_checkpoint()
        if self.manifest["mode"] == "local":
            self._preflight_local()
            return None
        archive_manifest_sha256 = self.verify_production_archive()
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
        command = [
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "StrictHostKeyChecking=yes",
            f"{node['ssh_user']}@{node['host']}",
            "sh",
            "-s",
            "--",
            *args,
        ]
        return run_checked(command, stdin=script, timeout=timeout).stdout

    def scp(self, node: Mapping[str, Any], local: str, remote: str) -> None:
        if not SAFE_REMOTE_RE.fullmatch(remote):
            fail(f"unsafe remote path for {node['name']}: {remote!r}")
        run_checked(
            [
                "scp",
                "-q",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                local,
                f"{node['ssh_user']}@{node['host']}:{remote}",
            ],
            timeout=600,
        )

    def _preflight_production(self) -> None:
        script = r"""
set -eu
root=$1 data=$2 key=$3 service=$4 gateway_service=$5 filter_service=$6 rpc=$7 model=$8 model_sha=$9 model_size=${10} digest=${11} transition=${12} checkpoint_manifest=${13} runtime_argv_sha=${14}
command -v sha256sum >/dev/null
command -v systemctl >/dev/null
command -v curl >/dev/null
command -v ss >/dev/null
command -v python3 >/dev/null
command -v cmp >/dev/null
command -v nginx >/dev/null 2>&1 || command -v apt-get >/dev/null
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
  python3 - "$data/.arc-recovery-rollout.json" "$digest" "$transition" "$checkpoint_manifest" <<'PY'
import json,pathlib,sys
p=pathlib.Path(sys.argv[1]); v=json.loads(p.read_text(encoding="utf-8"))
e={"schema":"arc.recovery.import-complete.v1","rollout_manifest_sha256":sys.argv[2],"transition_height":int(sys.argv[3]),"checkpoint_manifest_hash":sys.argv[4]}
if p.is_symlink() or v != e or p.read_text(encoding="utf-8") != json.dumps(e,sort_keys=True,separators=(",",":"))+"\n": raise SystemExit("existing v3 data is not an exact completed import for this rollout")
PY
elif test -e "$partial" || test -e "$partial_owner"; then
  test -f "$partial_owner" && test ! -L "$partial_owner"
  test "$(cat "$partial_owner")" = "$digest"
  test ! -e "$partial" || { test -d "$partial" && test ! -L "$partial"; }
fi
test -f "$key"
test "$(stat -c %a "$key")" = 600
for unit in "$service" "$gateway_service" "$filter_service"; do
  installed="/etc/systemd/system/$unit"
  if test -e "$installed"; then
    test -f "$stage" && test ! -L "$stage" && test "$(cat "$stage")" = "$digest"
    test -f "$installed" && test ! -L "$installed"
    cmp --silent "$root/$unit" "$installed"
  fi
done
test -f "$model"
test ! -L "$model"
test "$(stat -c %s "$model")" = "$model_size"
printf '%s  %s\n' "$model_sha" "$model" | sha256sum --check --strict
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
pids=$(pgrep -x arc-node || true)
if [ -n "$pids" ]; then
  test "$(printf '%s\n' "$pids" | wc -l | tr -d ' ')" = 1
  pid=$(systemctl show "$service" --property=MainPID --value)
  test "$pid" = "$pids"
  systemctl is-active --quiet "$service"
  test "$(readlink "/proc/$pid/exe")" = "$root/arc-node"
  test "$(sha256sum "/proc/$pid/cmdline" | cut -d' ' -f1)" = "$runtime_argv_sha"
fi
test ! -e /root/.arc-recovery-legacy-start-allowed
for retired in arc-self-heal.service arc-node.service arc-node-update.service; do
  fence="/etc/systemd/system/$retired.d/arc-recovery-freeze.conf"
  test -f "$fence"
  test ! -L "$fence"
  grep -Fxq 'RefuseManualStart=yes' "$fence"
  grep -Fxq 'Restart=no' "$fence"
  if systemctl is-active --quiet "$retired" || systemctl is-enabled --quiet "$retired"; then
    printf 'retired legacy service is not persistently fenced: %s\n' "$retired" >&2
    exit 1
  fi
done
! systemctl is-active --quiet arc-node-update.timer
! systemctl is-enabled --quiet arc-node-update.timer
case "$rpc" in 127.*|localhost:*) ;; *) printf 'RPC is not loopback\n' >&2; exit 1 ;; esac
"""
        for node in self.validators:
            hostname = node["public_hostname"]
            resolved = {entry[4][0] for entry in socket.getaddrinfo(hostname, 443, type=socket.SOCK_STREAM)}
            if node["host"] not in resolved:
                fail(f"{node['name']} {hostname} does not resolve to approved host {node['host']}")
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
                    node["rpc_listen"],
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
                ),
            )
            self.say(f"PASS {node['name']} remote fresh-dir/key/service/DNS preflight")

    def _http_json(self, node: Mapping[str, Any], path: str, *, timeout: int = 10) -> Any:
        if not path.startswith("/") or path.startswith("//"):
            fail(f"HTTP path must be source-relative: {path}")
        url = node["rpc_url"] + path
        request = urllib.request.Request(url, headers={"Accept": "application/json", "User-Agent": "arc-recovery-rollout/1"})
        try:
            with urllib.request.urlopen(request, timeout=timeout, context=ssl.create_default_context()) as response:
                payload = response.read(16 * 1024 * 1024 + 1)
                if len(payload) > 16 * 1024 * 1024:
                    fail(f"oversized response from {node['name']} {path}")
                return json.loads(payload)
        except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError, json.JSONDecodeError) as error:
            raise RolloutError(f"{node['name']} {path}: {error}") from error

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
        expected_transition = bare_hash(self.chain["transition_block_hash"], "transition hash")
        expected_root = bare_hash(self.chain["full_state_root"], "state root")
        for node in self.validators:
            source = self._block_commitment(self._http_json(node, f"/block/{self.chain['source_height']}"))
            transition = self._block_commitment(self._http_json(node, f"/block/{self.chain['transition_height']}"))
            if source[0] != self.chain["source_height"] or source[1] != expected_source:
                fail(f"{node['name']} does not preserve the approved legacy block at H")
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

    def prove_advancing_convergence(self) -> None:
        initial = self.wait_convergence()
        target = initial[0] + self.checks["min_height_advance"]
        deadline = time.monotonic() + self.checks["convergence_timeout_seconds"]
        not_before = time.monotonic() + self.checks["observation_seconds"]
        final = initial
        while time.monotonic() < deadline:
            final = self.wait_convergence(timeout=max(1, int(deadline - time.monotonic())))
            if final[0] >= target and time.monotonic() >= not_before:
                self.say(f"PASS advancing same-height convergence #{initial[0]} -> #{final[0]} hash={final[1]} root={final[2]}")
                return
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

    def obtain_receipt_evidence(self) -> ReceiptEvidence | None:
        reward = self.checks["reward"]
        if reward["mode"] != "receipt":
            return None
        if "probe_argv" not in reward:
            return ReceiptEvidence.from_value({key: reward[key] for key in ("tx_hash", "job_id", "worker")})
        environment = dict(os.environ)
        environment.update(
            {
                "ARC_RECOVERY_RPC_URLS": json.dumps([node["rpc_url"] for node in self.validators], separators=(",", ":")),
                "ARC_RECOVERY_ROLLOUT_MANIFEST_SHA256": self.digest,
                "ARC_RECOVERY_CHECKPOINT_MANIFEST_HASH": bare_hash(self.chain["approved_checkpoint_manifest_hash"], "hash"),
            }
        )
        result = run_checked(reward["probe_argv"], timeout=self.checks["convergence_timeout_seconds"], env=environment)
        try:
            body = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            fail(f"reward probe did not emit one JSON evidence object: {error}")
        return ReceiptEvidence.from_value(body)

    def prove_reward_receipt(self, evidence: ReceiptEvidence) -> None:
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
                        for row in rows
                    ):
                        fail(f"{node['name']} earnings lacks the exact successful 0x25 receipt")
                self.say(f"PASS mined 0x25 reward receipt {evidence.tx_hash} and worker earnings agree on all six validators")
                return
            except RolloutError as error:
                last_error = str(error)
                time.sleep(self.checks["poll_interval_seconds"])
        fail(f"reward receipt did not converge before timeout: {last_error}")

    def verify_live(self, evidence: ReceiptEvidence | None = None) -> None:
        self.wait_nodes_ready()
        self.prove_boundary()
        self.prove_advancing_convergence()
        self.prove_reward_policy()
        if evidence is not None:
            self.prove_reward_receipt(evidence)

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
            process.wait(timeout=30)
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
        complete = False
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
            evidence = self.obtain_receipt_evidence()
            if evidence is not None:
                self.prove_reward_receipt(evidence)
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
        if not SAFE_REMOTE_RE.fullmatch(value):
            fail(f"systemd argument contains an unsupported character: {value!r}")
        return value

    @staticmethod
    def gateway_service_name(node: Mapping[str, Any]) -> str:
        return node["service_name"].replace("arc-node-v3-", "arc-gateway-v3-", 1)

    @staticmethod
    def filter_service_name(node: Mapping[str, Any]) -> str:
        return node["service_name"].replace("arc-node-v3-", "arc-rpc-filter-v3-", 1)

    def systemd_unit(self, node: Mapping[str, Any]) -> str:
        argv = " ".join(self._systemd_escape_arg(arg) for arg in self.runtime_argv(node, remote=True))
        return f"""[Unit]
Description=ARC protocol-v3 validator {node['name']} ({self.manifest['rollout_id']})
After=network-online.target {self.filter_service_name(node)} {self.gateway_service_name(node)}
Wants=network-online.target
Requires={self.filter_service_name(node)} {self.gateway_service_name(node)}

[Service]
Type=simple
User={node['service_user']}
UMask=0077
Environment=ARC_PUBLIC_SOCKET={self._systemd_escape_arg(node['rpc_url'])}
ExecStart={argv}
Restart=on-failure
RestartSec=2s
TimeoutStopSec=30s
KillSignal=SIGTERM
NoNewPrivileges=true
PrivateTmp=true
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
"""

    def caddyfile(self, node: Mapping[str, Any]) -> str:
        get_paths = " ".join(self.manifest["gateway"]["public_get_paths"])
        post_paths = " ".join(self.manifest["gateway"]["public_post_paths"])
        validator_ips = " ".join(peer["host"] for peer in self.validators)
        return f"""{{
    email {self.manifest['gateway']['acme_email']}
    admin 127.0.0.1:2019
}}

{node['public_hostname']} {{
    header {{
        Strict-Transport-Security "max-age=31536000; includeSubDomains"
        X-Content-Type-Options "nosniff"
        X-Frame-Options "DENY"
        Referrer-Policy "no-referrer"
        Content-Security-Policy "default-src 'none'; frame-ancestors 'none'; base-uri 'none'"
        -Server
    }}

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
        reverse_proxy 127.0.0.1:18080
    }}

    @read {{
        method GET
        path {get_paths} /block/* /tx/* /account/* /account/*/txs /worker/earnings/* /community/reward_receipt/* /community/reward_job/*
    }}
    handle @read {{
        request_body {{
            max_size 1MB
        }}
        reverse_proxy 127.0.0.1:18080
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
        reverse_proxy 127.0.0.1:18080
    }}

    @write {{
        method POST
        path {post_paths}
    }}
    handle @write {{
        request_body {{
            max_size 1MB
        }}
        reverse_proxy 127.0.0.1:18080
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
        reverse_proxy 127.0.0.1:18080
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
        reverse_proxy 127.0.0.1:18080
    }}

    handle {{
        respond "not found" 404
    }}
}}
"""

    def nginx_filter(self, node: Mapping[str, Any]) -> str:
        zone = re.sub(r"[^a-z0-9]", "", node["name"])[0:20]
        allow_lines = "\n".join(f"        allow {peer['host']};" for peer in self.validators)
        upstream = node["rpc_listen"]
        root = node["remote_root"]
        return f"""daemon off;
worker_processes 1;
pid {root}/nginx-filter.pid;
error_log {root}/nginx-filter-error.log warn;

events {{ worker_connections 4096; }}

http {{
    access_log {root}/nginx-filter-access.log;
    server_tokens off;
    limit_req_zone $binary_remote_addr zone=arc_read_{zone}:10m rate=30r/s;
    limit_req_zone $binary_remote_addr zone=arc_write_{zone}:10m rate=30r/m;
    limit_req_zone $binary_remote_addr zone=arc_shard_{zone}:10m rate=100r/s;
    limit_conn_zone $binary_remote_addr zone=arc_conn_{zone}:10m;
    set_real_ip_from 127.0.0.1;
    real_ip_header X-Forwarded-For;
    real_ip_recursive on;

    # Only Caddy can reach the public HTTP filter.  This preserves the real
    # client IP for per-IP limits while keeping the node itself on loopback.
    server {{
        listen 127.0.0.1:18080;
        allow 127.0.0.1;
        deny all;
        client_max_body_size 1m;
        location ~ ^/(?:health|info|network/info|stats|validators|block/latest|blocks|inference/attestations|economics/rewards|faucet/status|community/list|community/reward_policy|workers/scoreboard|shards|models|models/shards)$ {{
            limit_except GET OPTIONS {{ deny all; }}
            limit_req zone=arc_read_{zone} burst=60 nodelay;
            proxy_pass http://{upstream};
            proxy_http_version 1.1;
            proxy_read_timeout 60s;
        }}
        location ~ ^/(?:block/[0-9]+(?:/txs)?|tx/(?:0x)?[0-9a-fA-F]{{64}}(?:/full)?|account/(?:0x)?[0-9a-fA-F]{{64}}(?:/txs)?|worker/earnings/(?:0x)?[0-9a-fA-F]{{64}}|community/reward_(?:receipt|job)/(?:0x)?[0-9a-fA-F]{{64}})$ {{
            limit_except GET OPTIONS {{ deny all; }}
            limit_req zone=arc_read_{zone} burst=60 nodelay;
            proxy_pass http://{upstream};
            proxy_http_version 1.1;
            proxy_read_timeout 60s;
        }}
        location ~ ^/inference/run(?:_consensus)?$ {{
            limit_except POST OPTIONS {{ deny all; }}
            limit_req zone=arc_write_{zone} burst=10 nodelay;
            limit_conn arc_conn_{zone} 4;
            proxy_pass http://{upstream};
            proxy_http_version 1.1;
            proxy_read_timeout {PUBLIC_INFERENCE_TIMEOUT_SECONDS}s;
            proxy_send_timeout 60s;
        }}
        location = /community/submit_work {{
            limit_except POST OPTIONS {{ deny all; }}
            limit_req zone=arc_write_{zone} burst=10 nodelay;
            limit_conn arc_conn_{zone} 4;
            proxy_pass http://{upstream};
            proxy_http_version 1.1;
            proxy_read_timeout {WORKER_SUBMIT_TIMEOUT_SECONDS}s;
            proxy_send_timeout 60s;
        }}
        location ~ ^/(?:community/(?:register|heartbeat|claim_work)|tx/submit_signed)$ {{
            limit_except POST OPTIONS {{ deny all; }}
            limit_req zone=arc_write_{zone} burst=10 nodelay;
            limit_conn arc_conn_{zone} 4;
            proxy_pass http://{upstream};
            proxy_http_version 1.1;
            proxy_read_timeout 120s;
            proxy_send_timeout 60s;
        }}
        location = /faucet/claim {{
            limit_except POST OPTIONS {{ deny all; }}
            limit_req zone=arc_write_{zone} burst=2 nodelay;
            limit_conn arc_conn_{zone} 2;
            proxy_pass http://{upstream};
            proxy_http_version 1.1;
            proxy_read_timeout 120s;
            proxy_send_timeout 30s;
        }}
        location = /internal/community/reward/approve {{
{allow_lines}
            deny all;
            limit_except POST {{ deny all; }}
            limit_req zone=arc_write_{zone} burst=10 nodelay;
            limit_conn arc_conn_{zone} 8;
            proxy_pass http://{upstream};
            proxy_http_version 1.1;
            proxy_read_timeout {VALIDATOR_APPROVAL_TIMEOUT_SECONDS}s;
        }}
        location ~ ^/(?:shards/announce|inference/(?:forward_shard|cleanup_shard))$ {{
{allow_lines}
            deny all;
            limit_except POST {{ deny all; }}
            client_max_body_size 4m;
            limit_req zone=arc_shard_{zone} burst=200 nodelay;
            proxy_pass http://{upstream};
            proxy_http_version 1.1;
            proxy_read_timeout 180s;
            proxy_send_timeout 180s;
        }}
        location / {{ return 404; }}
    }}
}}
"""

    def gateway_unit(self, node: Mapping[str, Any]) -> str:
        root = node["remote_root"]
        return f"""[Unit]
Description=ARC HTTPS gateway {node['name']} ({self.manifest['rollout_id']})
After=network-online.target {self.filter_service_name(node)}
Wants=network-online.target
Requires={self.filter_service_name(node)}

[Service]
Type=simple
UMask=0077
Environment=XDG_DATA_HOME={root}/caddy-data
Environment=XDG_CONFIG_HOME={root}/caddy-config
ExecStart={root}/caddy run --config {root}/Caddyfile --adapter caddyfile
ExecReload={root}/caddy reload --config {root}/Caddyfile --adapter caddyfile --force
Restart=on-failure
RestartSec=2s
TimeoutStopSec=30s
KillSignal=SIGTERM
NoNewPrivileges=true
PrivateTmp=true
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
"""

    def filter_unit(self, node: Mapping[str, Any]) -> str:
        root = node["remote_root"]
        return f"""[Unit]
Description=ARC loopback RPC policy filter {node['name']} ({self.manifest['rollout_id']})
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
UMask=0077
ExecStart=/usr/sbin/nginx -c {root}/nginx-filter.conf -p {root}/nginx-state
Restart=on-failure
RestartSec=2s
TimeoutStopSec=30s
KillSignal=SIGQUIT
NoNewPrivileges=true
PrivateTmp=true
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
"""

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
for name in arc-node genesis.toml recovery.arcchkpt legacy-validator-set-40m.json caddy Caddyfile nginx-filter.conf deployment-files.sha256 "$@"; do
  path="$root/$name"
  test ! -e "$path" || { test -f "$path" && test ! -L "$path"; }
done
''',
            (
                root,
                node["service_name"],
                self.gateway_service_name(node),
                self.filter_service_name(node),
            ),
        )
        artifacts = self.manifest["artifacts"]
        for source, destination in (
            (artifacts["binary"]["path"], f"{root}/arc-node"),
            (artifacts["genesis"]["path"], f"{root}/genesis.toml"),
            (artifacts["checkpoint"]["path"], f"{root}/recovery.arcchkpt"),
            (
                artifacts["legacy_validator_set"]["path"],
                f"{root}/legacy-validator-set-40m.json",
            ),
            (artifacts["caddy"]["path"], f"{root}/caddy"),
        ):
            self.scp(node, source, destination)
        with tempfile.TemporaryDirectory(prefix="arc-recovery-config-") as temporary:
            configs = {
                node["service_name"]: self.systemd_unit(node),
                self.gateway_service_name(node): self.gateway_unit(node),
                self.filter_service_name(node): self.filter_unit(node),
                "Caddyfile": self.caddyfile(node),
                "nginx-filter.conf": self.nginx_filter(node),
            }
            for name, contents in configs.items():
                path = Path(temporary) / name
                path.write_text(contents, encoding="utf-8")
                self.scp(node, str(path), f"{root}/{name}")
            deployment_hashes = {
                "arc-node": artifacts["binary"]["sha256"],
                "genesis.toml": artifacts["genesis"]["sha256"],
                "recovery.arcchkpt": artifacts["checkpoint"]["sha256"],
                "legacy-validator-set-40m.json": artifacts["legacy_validator_set"]["sha256"],
                "caddy": artifacts["caddy"]["sha256"],
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
            self.scp(node, str(index), f"{root}/deployment-files.sha256")
        verify_script = r"""
set -eu
root=$1 binary_sha=$2 genesis_sha=$3 checkpoint_sha=$4 legacy_validators_sha=$5 caddy_sha=$6 model=$7 model_sha=$8 model_size=$9 digest=${10} deployment_index_sha=${11}
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
chmod 0500 "$root/arc-node" "$root/caddy"
chmod 0400 "$root/genesis.toml" "$root/recovery.arcchkpt" "$root/legacy-validator-set-40m.json" "$root/Caddyfile" "$root"/*.conf "$root"/*.service
"$root/caddy" validate --config "$root/Caddyfile" --adapter caddyfile
if test -e "$root/.arc-recovery-stage-complete"; then
  test -f "$root/.arc-recovery-stage-complete" && test ! -L "$root/.arc-recovery-stage-complete"
  test "$(cat "$root/.arc-recovery-stage-complete")" = "$digest"
else
  python3 - "$root/.arc-recovery-stage-complete" "$digest" <<'PY'
import os,sys
fd=os.open(sys.argv[1],os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
with os.fdopen(fd,"w",encoding="ascii") as h: h.write(sys.argv[2]+"\n"); h.flush(); os.fsync(h.fileno())
PY
fi
"""
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
            ),
        )
        remote_verify = self.recovery_cli("verify", node, remote=True)
        partial_data = f"{node['data_dir']}.arc-recovery-import-{self.digest}"
        remote_import = self.recovery_cli(
            "import", node, remote=True, data_dir_override=partial_data
        )
        self.ssh(node, 'set -eu\nexec "$@"\n', tuple(remote_verify), timeout=600)
        import_script = r'''
set -eu
data=$1 partial=$2 digest=$3 transition=$4 checkpoint_manifest=$5
owner="${partial}.owner"
marker_name=.arc-recovery-rollout.json
validate_marker() {
  python3 - "$1/$marker_name" "$digest" "$transition" "$checkpoint_manifest" <<'PY'
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
  exit 0
fi
resume_owned_partial=false
if test -e "$owner"; then
  test -f "$owner" && test ! -L "$owner" && test "$(cat "$owner")" = "$digest"
  resume_owned_partial=true
else
  test ! -e "$partial"
  python3 - "$owner" "$digest" <<'PY'
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
    exit 0
  fi
  find "$partial" -xdev -depth -delete
fi
shift 5
summary=$("$@")
python3 - "$summary" "$transition" "$checkpoint_manifest" <<'PY'
import json,sys
v=json.loads(sys.argv[1]); h=str(v.get("manifest_hash","")).removeprefix("0x")
if v.get("status")!="ACTIVATED" or v.get("height")!=int(sys.argv[2]) or h!=sys.argv[3]: raise SystemExit("recovery import did not activate the exact approved H+1 checkpoint")
PY
python3 - "$partial/$marker_name" "$digest" "$transition" "$checkpoint_manifest" <<'PY'
import json,os,sys
p=sys.argv[1]; v={"schema":"arc.recovery.import-complete.v1","rollout_manifest_sha256":sys.argv[2],"transition_height":int(sys.argv[3]),"checkpoint_manifest_hash":sys.argv[4]}; b=(json.dumps(v,sort_keys=True,separators=(",",":"))+"\n").encode(); fd=os.open(p,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o400)
with os.fdopen(fd,"wb") as h: h.write(b); h.flush(); os.fsync(h.fileno())
PY
mv --no-clobber -T -- "$partial" "$data"
test ! -e "$partial"
validate_marker "$data"
rm -f -- "$owner"
'''
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

    def _install_gateway_and_unit(self, node: Mapping[str, Any]) -> None:
        root = node["remote_root"]
        gateway_script = r"""
set -eu
root=$1 hostname=$2 service=$3 gateway_service=$4 filter_service=$5 digest=$6
export DEBIAN_FRONTEND=noninteractive
test -d "$root" && test ! -L "$root"
test -f "$root/.arc-recovery-stage-complete" && test ! -L "$root/.arc-recovery-stage-complete"
test ! -e /root/.arc-recovery-legacy-start-allowed
for retired in arc-self-heal.service arc-node.service arc-node-update.service; do
  fence="/etc/systemd/system/$retired.d/arc-recovery-freeze.conf"
  test -f "$fence"
  grep -Fxq 'RefuseManualStart=yes' "$fence"
  grep -Fxq 'Restart=no' "$fence"
  ! systemctl is-active --quiet "$retired"
  ! systemctl is-enabled --quiet "$retired"
done
inventory="$root/pre-gateway.inventory"
if test -e "$inventory"; then
  test -f "$inventory" && test ! -L "$inventory"
  test "$(grep -c '^rollout_manifest_sha256=' "$inventory")" = 1
  grep -Fxq "rollout_manifest_sha256=$digest" "$inventory"
  test "$(grep -c '^nginx_active=' "$inventory")" = 1
  test "$(grep -c '^nginx_enabled=' "$inventory")" = 1
else
  temporary=$(mktemp "$root/.pre-gateway.XXXXXX")
  {
    printf 'captured_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'rollout_manifest_sha256=%s\n' "$digest"
    printf 'nginx_active=%s\n' "$(systemctl is-active nginx.service 2>/dev/null || true)"
    printf 'nginx_enabled=%s\n' "$(systemctl is-enabled nginx.service 2>/dev/null || true)"
    ss -ltnp | grep -E ':(80|443)[[:space:]]' || true
  } > "$temporary"
  chmod 0400 "$temporary"
  mv --no-clobber -T -- "$temporary" "$inventory"
  if test -e "$temporary"; then rm -f -- "$temporary"; fi
  test -f "$inventory" && test ! -L "$inventory"
  grep -Fxq "rollout_manifest_sha256=$digest" "$inventory"
fi
old_nginx_active=$(sed -n 's/^nginx_active=//p' "$inventory")
old_nginx_enabled=$(sed -n 's/^nginx_enabled=//p' "$inventory")
rollback_on_error() {
  status=$?
  if [ "$status" -ne 0 ]; then
    systemctl disable --now "$service" "$gateway_service" "$filter_service" 2>/dev/null || true
    if [ "$old_nginx_enabled" = enabled ]; then systemctl enable nginx.service 2>/dev/null || true; fi
    if [ "$old_nginx_active" = active ]; then systemctl start nginx.service 2>/dev/null || true; fi
  fi
  exit "$status"
}
trap rollback_on_error EXIT
if ! command -v nginx >/dev/null 2>&1; then
  apt-get update
  apt-get install -y --no-install-recommends nginx ca-certificates curl
fi
# Preserve the existing nginx configuration and replace only its running role;
# the dedicated filter below uses its own locked config and never reads /etc/nginx.
systemctl stop nginx.service 2>/dev/null || true
systemctl disable nginx.service 2>/dev/null || true
if ss -ltnp | grep -E ':(80|443)[[:space:]]' >/dev/null; then
  printf 'ports 80/443 remain occupied after stopping system nginx\n' >&2
  exit 1
fi
for directory in "$root/nginx-state" "$root/caddy-data" "$root/caddy-config"; do
  if test -e "$directory"; then test -d "$directory" && test ! -L "$directory"; else mkdir --mode=0700 "$directory"; fi
done
/usr/sbin/nginx -t -c "$root/nginx-filter.conf" -p "$root/nginx-state"
"$root/caddy" validate --config "$root/Caddyfile" --adapter caddyfile
for unit in "$service" "$gateway_service" "$filter_service"; do
  installed="/etc/systemd/system/$unit"
  if test -e "$installed"; then
    test -f "$installed" && test ! -L "$installed"
    cmp --silent "$root/$unit" "$installed"
  else
    cp --no-clobber "$root/$unit" "$installed"
  fi
  chmod 0644 "$installed"
done
systemctl daemon-reload
systemctl enable "$filter_service" "$gateway_service" "$service"
systemctl start "$filter_service"
systemctl start "$gateway_service"
issued=false
for _ in $(seq 1 180); do
  if curl --silent --show-error --output /dev/null --connect-timeout 5 --max-time 10 "https://$hostname/health"; then
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
systemctl is-active --quiet "$filter_service"
systemctl is-active --quiet "$gateway_service"
trap - EXIT
"""
        self.prepared_production.add(node["name"])
        self.ssh(
            node,
            gateway_script,
            (
                root,
                node["public_hostname"],
                node["service_name"],
                self.gateway_service_name(node),
                self.filter_service_name(node),
                self.digest,
            ),
            timeout=900,
        )
        self.say(f"PASS {node['name']} issued trusted Caddy TLS and installed locked gateway/filter/node units")

    def production_service(self, node: Mapping[str, Any], action: str) -> None:
        if action not in {"start", "stop", "restart"}:
            fail(f"unsupported service action {action}")
        self.ssh(node, 'set -eu\nsystemctl "$1" "$2"\n', (action, node["service_name"]), timeout=90)
        if action in {"start", "restart"}:
            self.started_production.add(node["name"])
        elif action == "stop":
            self.started_production.discard(node["name"])

    def _prove_production_listener(self, node: Mapping[str, Any]) -> None:
        script = r"""
set -eu
rpc=$1 service=$2
systemctl is-active --quiet "$service"
host=${rpc%:*}
port=${rpc##*:}
case "$host" in 127.*|localhost) ;; *) exit 1 ;; esac
ss -ltnp | grep -E "[[:space:]]${host}:${port}[[:space:]]" >/dev/null
if ss -ltnp | grep -E "[[:space:]](0\.0\.0\.0|\[::\]):${port}[[:space:]].*arc-node" >/dev/null; then
  printf 'arc-node RPC is publicly bound\n' >&2
  exit 1
fi
"""
        self.ssh(node, script, (node["rpc_listen"], node["service_name"]))
        self._http_json(node, "/network/info")
        self._prove_public_browser_contract(node)

    def _prove_public_browser_contract(self, node: Mapping[str, Any]) -> None:
        """Exercise the browser contract at the public TLS edge.

        OPTIONS must terminate at Caddy with no node proxy. Public GET/POST
        routes expose CORS only to the exact GitHub Pages origin, while the
        validator-only approval and shard routes expose no browser CORS at all.
        """

        def request(
            method: str,
            path: str,
            *,
            origin: str,
            requested_method: str | None = None,
        ) -> tuple[int, Mapping[str, str]]:
            headers = {
                "Accept": "application/json",
                "Origin": origin,
                "User-Agent": "arc-recovery-browser-gate/1",
            }
            if requested_method is not None:
                headers["Access-Control-Request-Method"] = requested_method
                headers["Access-Control-Request-Headers"] = "content-type"
            rpc_request = urllib.request.Request(
                node["rpc_url"] + path,
                data=b"" if method == "POST" else None,
                headers=headers,
                method=method,
            )
            try:
                with urllib.request.urlopen(
                    rpc_request,
                    timeout=20,
                    context=ssl.create_default_context(),
                ) as response:
                    response.read(1024)
                    return response.status, response.headers
            except urllib.error.HTTPError as error:
                error.read(1024)
                return error.code, error.headers
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

        status, headers = request(
            "OPTIONS",
            "/network/info",
            origin=PUBLIC_BROWSER_ORIGIN,
            requested_method="GET",
        )
        require_allowed(status, headers, 204)
        status, headers = request(
            "OPTIONS",
            "/inference/run",
            origin=PUBLIC_BROWSER_ORIGIN,
            requested_method="POST",
        )
        require_allowed(status, headers, 204)
        status, headers = request("GET", "/network/info", origin=PUBLIC_BROWSER_ORIGIN)
        require_allowed(status, headers, 200)

        status, headers = request(
            "OPTIONS",
            "/internal/community/reward/approve",
            origin=PUBLIC_BROWSER_ORIGIN,
            requested_method="POST",
        )
        if status != 404 or headers.get("Access-Control-Allow-Origin") is not None:
            fail(f"{node['name']} validator approval route leaked browser CORS")
        status, headers = request(
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
service=$1 argv_sha=$2 executable=$3 model=$4 model_sha=$5 model_size=$6
pid=$(systemctl show "$service" --property=MainPID --value)
case "$pid" in ''|0|*[!0-9]*) printf 'service has no exact MainPID\n' >&2; exit 1 ;; esac
test -d "/proc/$pid"
test "$(cat "/proc/$pid/comm")" = arc-node
test "$(pgrep -x arc-node)" = "$pid"
test "$(readlink "/proc/$pid/exe")" = "$executable"
test "$(sha256sum "/proc/$pid/cmdline" | cut -d' ' -f1)" = "$argv_sha"
test -f "$model"
test ! -L "$model"
test "$(stat -c %s "$model")" = "$model_size"
printf '%s  %s\n' "$model_sha" "$model" | sha256sum --check --strict
"""
        for node in self.validators:
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
                ),
            )
        self.say(
            "PASS all six systemd MainPIDs use the sealed absolute model path, exact model bytes/size, and per-node shard argv"
        )

    def execute_production(self) -> None:
        # Re-hash every executing rollout/archive component after authorization
        # and immediately before the first remote mutation.
        self.verify_execution_provenance()
        complete = False
        try:
            for node in self.validators:
                self._stage_production_node(node)
            for node in self.validators:
                self._install_gateway_and_unit(node)
            first_quorum = self.validators[:REQUIRED_APPROVALS]
            with concurrent.futures.ThreadPoolExecutor(max_workers=REQUIRED_APPROVALS) as pool:
                futures = [pool.submit(self.production_service, node, "start") for node in first_quorum]
                for future in futures:
                    future.result()
            self.production_service(self.validators[-1], "start")
            self.wait_nodes_ready()
            self.prove_production_runtime_inventory()
            for node in self.validators:
                self._prove_production_listener(node)
            self.say("PASS all public sources use verified HTTPS and every arc-node RPC listener is loopback-only")
            self.prove_production_shard_topology()
            self.prove_boundary()
            self.prove_advancing_convergence()
            for node in self.validators:
                before = self.wait_convergence()[0]
                self.production_service(node, "restart")
                self.wait_nodes_ready(timeout=self.checks["restart_timeout_seconds"])
                self.prove_production_runtime_inventory()
                after = self.wait_convergence(
                    minimum_height=before + self.checks["min_height_advance"],
                    timeout=self.checks["restart_timeout_seconds"],
                )
                self.say(f"PASS {node['name']} production restart; fleet advanced #{before} -> #{after[0]}")
            self.prove_reward_policy()
            evidence = self.obtain_receipt_evidence()
            if evidence is not None:
                self.prove_reward_receipt(evidence)
            self.verify_production_archive(verify_live_captures=True)
            complete = True
        finally:
            if not complete:
                self.say("ROLLBACK stopping only newly started v3 services; all imported data, artifacts, configs, and logs remain preserved")
                for node in reversed(self.validators):
                    if node["name"] in self.started_production or node["name"] in self.prepared_production:
                        try:
                            self.ssh(
                                node,
                                r'''set -eu
inventory=$4
root=$5 digest=$6
test -f "$root/.arc-recovery-rollout-owner" && test ! -L "$root/.arc-recovery-rollout-owner"
test "$(cat "$root/.arc-recovery-rollout-owner")" = "$digest"
test -f "$inventory" && test ! -L "$inventory"
grep -Fxq "rollout_manifest_sha256=$digest" "$inventory"
for unit in "$1" "$2" "$3"; do
  installed="/etc/systemd/system/$unit"
  if test -e "$installed"; then
    test -f "$installed" && test ! -L "$installed"
    cmp --silent "$root/$unit" "$installed"
    systemctl disable --now "$unit"
  else
    ! systemctl is-active --quiet "$unit"
  fi
done
old_active=$(sed -n 's/^nginx_active=//p' "$inventory")
old_enabled=$(sed -n 's/^nginx_enabled=//p' "$inventory")
if [ "$old_enabled" = enabled ]; then systemctl enable nginx.service; fi
if [ "$old_active" = active ]; then systemctl start nginx.service; fi
''',
                                (
                                    node["service_name"],
                                    self.gateway_service_name(node),
                                    self.filter_service_name(node),
                                    f"{node['remote_root']}/pre-gateway.inventory",
                                    node["remote_root"],
                                    self.digest,
                                ),
                                timeout=90,
                            )
                        except Exception as error:
                            self.say(f"WARN could not stop {node['name']}: {error}")
        if complete:
            self.say("COMPLETE production rollout; all six v3 validators remain enabled and running")

    def execute(self) -> None:
        if self.manifest["mode"] == "local":
            self.execute_local()
        else:
            self.execute_production()


def parse_evidence_file(path: Path) -> ReceiptEvidence:
    try:
        return ReceiptEvidence.from_value(json.loads(path.read_text(encoding="utf-8")))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot read reward evidence: {error}")


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
    verify = subparsers.add_parser("verify", help="read-only live convergence and optional mined-reward verification")
    verify.add_argument("--manifest", required=True, type=Path)
    verify.add_argument("--reward-evidence", type=Path)
    frontend = subparsers.add_parser(
        "frontend-config",
        help="derive a create-only recovered frontend config from the sealed production manifest",
    )
    frontend.add_argument("--manifest", required=True, type=Path)
    frontend.add_argument("--output", required=True, type=Path)
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
        rollout = RecoveryRollout(manifest, digest)
        if args.command == "frontend-config":
            config_digest = write_frontend_config(rollout, args.output)
            print(
                f"FRONTEND CONFIG {args.output} sha256={config_digest} "
                f"rollout_sha256={digest}"
            )
            return 0
        if args.command == "verify":
            evidence = parse_evidence_file(args.reward_evidence) if args.reward_evidence else None
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
            print(f"To execute this exact plan: ARC_RECOVERY_GO='{authorization}' {Path(sys.argv[0]).name} run --manifest {shlex.quote(str(args.manifest))} --execute --go-hash {digest}{archive_arg}")
            return 0
        require_go(
            manifest,
            digest,
            args.go_hash,
            args.archive_manifest_sha256,
            archive_manifest_sha256,
        )
        rollout.execute()
        return 0
    except RolloutError as error:
        print(f"recovery rollout: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
