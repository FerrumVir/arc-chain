#!/usr/bin/env python3
"""Validate protected recovery inputs and emit the three public cutover assets.

The recovery handoff is intentionally not copied wholesale into a release.  It
contains the sealed production rollout manifest (and sidecar), the canonical
legacy-maintenance boundary, and the quorum-signed ARCCHKPT file.  This helper
reuses the recovery manifest validator, verifies the checkpoint with a compiled
verifier, separately binds the exact release inspector binary, and re-checks
the cross-artifact hashes before emitting the compact public trust surface. The
multi-gigabyte checkpoint remains in the protected validator handoff;
community clients receive a canonical descriptor instead.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
from pathlib import Path
from typing import Any, Mapping, NoReturn, Sequence


SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent
RECOVERY_DIR = REPO_ROOT / "scripts" / "recovery"
if str(RECOVERY_DIR) not in sys.path:
    sys.path.insert(0, str(RECOVERY_DIR))

import recovery_rollout as recovery  # noqa: E402


HANDOFF_FILES = {
    "arc-recovery-final.lock.json",
    "arc-recovery-final.lock.json.sha256",
    "legacy-maintenance-boundary.json",
    "recovery.arcchkpt",
}
BOUNDARY_OUTPUT = "arc-legacy-maintenance-boundary.json"
CHECKPOINT_DESCRIPTOR_OUTPUT = "arc-recovery-checkpoint-descriptor.json"
POLICY_OUTPUT = "arc-cutover-policy.json"
CANONICAL_BOUNDARY_HEIGHT = 137_145
REQUIRED_POST_CUTOVER_MIN_HEIGHT = 137_146
OLD_CLAIM_SUBMIT_LISTENER_PORTS = [9090, 3001]
EXPECTED_RECOVERY_VALIDATORS = (
    (
        "nyc",
        "149.28.32.76",
        "adf4ff16f997c871c16f3897e67881311d08f975f28ebdcf79e86ea9e3b99d0f",
        6_666_667,
    ),
    (
        "lax",
        "140.82.16.112",
        "44d20543df6e76696da2ebbbd79e4243cd41729fa5b890e2618991e489314780",
        6_666_667,
    ),
    (
        "ams",
        "136.244.109.1",
        "5772741c93d8a4b04ec39007cb568a31e13ffba0d3e786596d1900d30e529f21",
        6_666_667,
    ),
    (
        "lhr",
        "104.238.171.11",
        "228787281308d6c1a560848c2c168814bde1b6153e9e65a286d7211f04628fdd",
        6_666_667,
    ),
    (
        "nrt",
        "202.182.107.41",
        "f03cbab49cf553a05541ddebc09b32a4c5507efb157d354b6d7f8c6682c32f5f",
        6_666_666,
    ),
    (
        "sgp",
        "149.28.153.31",
        "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b",
        6_666_666,
    ),
)
HASH_RE = re.compile(r"^[0-9a-f]{64}$")
SIGNATURE_RE = re.compile(r"^[0-9a-f]{128}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
TAG_RE = re.compile(r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
UTC_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")


class CutoverAssetError(RuntimeError):
    """A protected handoff or derived release policy failed closed."""


def fail(message: str) -> NoReturn:
    raise CutoverAssetError(message)


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonical_json(value: Any) -> bytes:
    return (
        json.dumps(
            value,
            sort_keys=True,
            separators=(",", ":"),
            ensure_ascii=False,
            allow_nan=False,
        )
        + "\n"
    ).encode("utf-8")


def safe_regular(path: Path, label: str, maximum: int, *, executable: bool = False) -> None:
    try:
        details = path.lstat()
    except OSError as error:
        fail(f"{label} is unavailable: {error}")
    if (
        stat.S_ISLNK(details.st_mode)
        or not stat.S_ISREG(details.st_mode)
        or details.st_size <= 0
        or details.st_size > maximum
        or details.st_nlink != 1
    ):
        fail(f"{label} must be one bounded, non-symlink regular file")
    if executable and not os.access(path, os.X_OK):
        fail(f"{label} is not executable")


def read_canonical_json(path: Path, label: str, maximum: int) -> tuple[dict[str, Any], bytes]:
    safe_regular(path, label, maximum)
    try:
        payload = path.read_bytes()
        value = json.loads(payload)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"{label} is invalid JSON: {error}")
    if not isinstance(value, dict) or payload != canonical_json(value):
        fail(f"{label} must be one canonical JSON object")
    return value, payload


def bare_hash(value: Any, label: str) -> str:
    try:
        return recovery.bare_hash(value, label)
    except recovery.RolloutError as error:
        fail(str(error))


def exact_hash(value: Any, label: str) -> str:
    if not isinstance(value, str) or HASH_RE.fullmatch(value) is None:
        fail(f"{label} must be one lowercase SHA-256")
    return value


def exact_utc(value: Any, label: str) -> dt.datetime:
    if not isinstance(value, str) or UTC_RE.fullmatch(value) is None:
        fail(f"{label} must use canonical UTC seconds")
    try:
        return dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=dt.timezone.utc
        )
    except ValueError as error:
        fail(f"{label} is invalid: {error}")


def require_exact_keys(value: Any, expected: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    actual = set(value)
    if actual != expected:
        fail(
            f"{label} fields differ: missing={sorted(expected - actual)}, "
            f"unknown={sorted(actual - expected)}"
        )
    return value


def validate_boundary(
    boundary: dict[str, Any],
    boundary_payload: bytes,
    manifest: Mapping[str, Any],
    manifest_sha256: str,
    release_commit: str,
) -> tuple[str, str]:
    expected_fields = {
        "schema",
        "source_main_commit",
        "freeze_plan_sha256",
        "capture_id",
        "first_quarantine_started_at",
        "all_controlled_stopped_at",
        "created_at",
        "official_origin_scope",
        "legacy_public_height_receipt",
        "authenticated_prefence_height_cross_proof_sha256",
        "legacy_live_observation_selection_sha256",
        "legacy_live_observation_generation",
        "observation_generation_receipt_sha256",
        "drive_prefreeze_receipt_sha256",
        "quarantine_generation_ledger_sha256",
        "legacy_maintenance_evidence_bundle_sha256",
        "network_quarantine_stability_proof_sha256",
        "network_quarantine_challenge",
        "tools",
        "nodes",
        "evidence_heights",
        "observed_cutoff_height",
        "continuity_safety_margin",
        "continuity_safety_margin_policy",
        "legacy_public_max_height",
        "global_absence_claimed",
        "reopening_policy",
        "late_fork_circuit",
        "threat_model",
    }
    require_exact_keys(boundary, expected_fields, "legacy maintenance boundary")
    if boundary["schema"] != "arc.recovery.legacy-maintenance-boundary.v1":
        fail("legacy maintenance boundary schema is unsupported")
    if boundary["source_main_commit"] != release_commit:
        fail("legacy maintenance boundary source commit differs from the release")

    provenance = manifest["provenance"]
    archive = manifest["archive"]
    chain = manifest["chain"]
    artifacts = manifest["artifacts"]
    boundary_sha256 = sha256_bytes(boundary_payload)
    if (
        boundary_sha256 != artifacts["legacy_maintenance_boundary"]["sha256"]
        or boundary_sha256 != chain["legacy_maintenance_boundary_sha256"]
    ):
        fail("legacy maintenance boundary bytes differ from the sealed recovery roots")
    freeze_sha256 = exact_hash(
        boundary["freeze_plan_sha256"], "legacy maintenance freeze-plan SHA-256"
    )
    if freeze_sha256 != archive["freeze_plan_sha256"]:
        fail("legacy maintenance boundary freeze plan differs from the rollout")
    capture_id = exact_hash(boundary["capture_id"], "legacy maintenance capture ID")
    if (
        capture_id != archive["capture_id"]
        or capture_id != recovery.capture_id_for_freeze_plan_hash(freeze_sha256)
    ):
        fail("legacy maintenance boundary capture ID is not freeze-plan derived")
    if provenance["source_main_commit"] != release_commit:
        fail("recovery manifest source commit differs from the release")

    first = exact_utc(
        boundary["first_quarantine_started_at"], "first quarantine timestamp"
    )
    stopped = exact_utc(
        boundary["all_controlled_stopped_at"], "all-controlled-stopped timestamp"
    )
    created = exact_utc(boundary["created_at"], "maintenance-boundary timestamp")
    if not first <= stopped <= created:
        fail("legacy maintenance boundary timestamps are not ordered")

    expected_origins = [dict(row) for row in recovery.LEGACY_OFFICIAL_ORIGINS]
    scope = require_exact_keys(
        boundary["official_origin_scope"],
        {"global_absence_claimed", "origins"},
        "legacy official-origin scope",
    )
    if scope != {"global_absence_claimed": False, "origins": expected_origins}:
        fail("legacy maintenance boundary official origins differ from the exact six")
    if chain["legacy_official_origins"] != expected_origins:
        fail("recovery manifest official origins differ from the boundary")

    if (
        boundary["global_absence_claimed"] is not False
        or chain["legacy_global_absence_claimed"] is not False
    ):
        fail("the recovery evidence must not claim global legacy absence")
    if (
        boundary["continuity_safety_margin"]
        != recovery.LEGACY_CONTINUITY_SAFETY_MARGIN
        or boundary["continuity_safety_margin_policy"]
        != recovery.LEGACY_CONTINUITY_SAFETY_MARGIN_POLICY
        or boundary["reopening_policy"] != recovery.LEGACY_REOPENING_POLICY
        or boundary["late_fork_circuit"] != recovery.LEGACY_LATE_FORK_CIRCUIT
        or boundary["threat_model"] != recovery.LEGACY_QUARANTINE_THREAT_MODEL
    ):
        fail("legacy maintenance continuity/retirement policy differs")

    observed_cutoff = boundary["observed_cutoff_height"]
    legacy_public_max = boundary["legacy_public_max_height"]
    if (
        isinstance(observed_cutoff, bool)
        or not isinstance(observed_cutoff, int)
        or observed_cutoff < 0
        or isinstance(legacy_public_max, bool)
        or not isinstance(legacy_public_max, int)
        or legacy_public_max
        != observed_cutoff + recovery.LEGACY_CONTINUITY_SAFETY_MARGIN
        or observed_cutoff != chain["legacy_observed_cutoff_height"]
        or legacy_public_max != chain["legacy_public_max_height"]
    ):
        fail("legacy maintenance height ceiling differs from the recovery manifest")
    evidence_heights = boundary["evidence_heights"]
    if not isinstance(evidence_heights, list) or not evidence_heights:
        fail("legacy maintenance boundary has no evidence-height inventory")
    heights: list[int] = []
    allowed_nodes = {node for node, _host in recovery.PRODUCTION_FLEET}
    for index, raw in enumerate(evidence_heights):
        row = require_exact_keys(
            raw,
            {"node", "label", "height", "evidence_sha256"},
            f"legacy evidence height {index}",
        )
        if row["node"] not in allowed_nodes or not isinstance(row["label"], str) or not row["label"]:
            fail("legacy evidence-height identity differs")
        if isinstance(row["height"], bool) or not isinstance(row["height"], int) or row["height"] < 0:
            fail("legacy evidence height must be a non-negative integer")
        exact_hash(row["evidence_sha256"], "legacy evidence-height root")
        heights.append(row["height"])
    if max(heights) != observed_cutoff:
        fail("legacy observed cutoff is not the evidence-height maximum")

    boundary_nodes = boundary["nodes"]
    if not isinstance(boundary_nodes, list) or len(boundary_nodes) != recovery.REQUIRED_VALIDATORS:
        fail("legacy maintenance boundary must contain exactly six nodes")
    for index, ((name, host), raw) in enumerate(zip(recovery.PRODUCTION_FLEET, boundary_nodes)):
        if not isinstance(raw, dict) or (
            raw.get("node"), raw.get("host"), raw.get("origin")
        ) != (name, host, f"http://{host}:9090"):
            fail(f"legacy maintenance node {index} topology differs")

    # The final sealed manifest itself is an input to the owner-signed policy;
    # keep the argument used so static analyzers cannot mistake it for an
    # unbound parse.
    exact_hash(manifest_sha256, "recovery manifest SHA-256")
    return freeze_sha256, capture_id


def run_checkpoint_cli(
    binary: Path,
    checkpoint: Path,
    genesis: Path,
    chain: Mapping[str, Any],
) -> tuple[dict[str, Any], dict[str, Any]]:
    safe_regular(binary, "release Linux x86_64 node", 4 * 1024**3, executable=True)
    safe_regular(checkpoint, "recovery checkpoint", 8 * 1024**3)
    safe_regular(genesis, "release genesis", 16 * 1024 * 1024)
    manifest_hash = bare_hash(
        chain["approved_checkpoint_manifest_hash"], "approved checkpoint manifest hash"
    )
    commands = (
        (
            "inspect",
            [os.fspath(binary), "recovery", "inspect", "--checkpoint", os.fspath(checkpoint)],
        ),
        (
            "verify",
            [
                os.fspath(binary),
                "recovery",
                "verify",
                "--checkpoint",
                os.fspath(checkpoint),
                "--genesis",
                os.fspath(genesis),
                "--approved-manifest-hash",
                manifest_hash,
                "--recovery-epoch",
                str(chain["recovery_epoch"]),
                "--validator-set-id",
                str(chain["validator_set_id"]),
            ],
        ),
    )
    results: list[dict[str, Any]] = []
    environment = {
        "HOME": "/var/empty",
        "PATH": "/usr/bin:/bin",
        "LANG": "C",
        "LC_ALL": "C",
        "TZ": "UTC",
        "RUST_BACKTRACE": "0",
    }
    for label, command in commands:
        try:
            completed = subprocess.run(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd="/",
                env=environment,
                timeout=300,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            fail(f"checkpoint {label} command failed: {error}")
        if completed.returncode != 0:
            diagnostic = completed.stderr.decode("utf-8", errors="replace")[:2000]
            fail(f"checkpoint {label} command rejected the handoff: {diagnostic}")
        if len(completed.stdout) > 4 * 1024 * 1024:
            fail(f"checkpoint {label} output is oversized")
        try:
            value = json.loads(completed.stdout)
        except (UnicodeError, json.JSONDecodeError) as error:
            fail(f"checkpoint {label} output is not JSON: {error}")
        if not isinstance(value, dict):
            fail(f"checkpoint {label} output is not one object")
        results.append(value)
    return results[0], results[1]


def validate_checkpoint_outputs(
    inspected: Mapping[str, Any],
    verified: Mapping[str, Any],
    manifest: Mapping[str, Any],
    chain: Mapping[str, Any],
) -> tuple[int, dict[str, Any], dict[str, Any]]:
    expected = {
        "format_version": 1,
        "chain_id": chain["chain_id"],
        "manifest_hash": chain["approved_checkpoint_manifest_hash"],
        "payload_hash": None,
        "genesis_hash": chain["genesis_hash"],
        "full_state_root": chain["full_state_root"],
        "source_height": chain["source_height"],
        "source_consensus_round": chain["source_consensus_round"],
        "created_at_unix_ms": chain["created_at_unix_ms"],
        "source_block_hash": chain["source_block_hash"],
        "source_state_root": chain["source_state_root"],
        "transition_height": chain["transition_height"],
        "transition_block_hash": chain["transition_block_hash"],
        "recovery_domain": chain["recovery_domain"],
        "recovery_epoch": chain["recovery_epoch"],
        "validator_set_id": chain["validator_set_id"],
        "protocol_version": chain["protocol_version"],
        "validator_count": recovery.REQUIRED_VALIDATORS,
        "community_rewards_v1_activation_height": REQUIRED_POST_CUTOVER_MIN_HEIGHT,
    }
    hash_fields = {
        "manifest_hash",
        "payload_hash",
        "genesis_hash",
        "full_state_root",
        "source_block_hash",
        "source_state_root",
        "transition_block_hash",
        "recovery_domain",
    }
    normalized_inspections = []
    for label, value in (("inspect", inspected), ("verify", verified)):
        normalized_inspection = {}
        for field, wanted in expected.items():
            got = value.get(field)
            if field in hash_fields:
                normalized = bare_hash(got, f"checkpoint {label}.{field}")
                if field != "payload_hash" and normalized != bare_hash(
                    wanted, f"manifest chain.{field}"
                ):
                    fail(f"checkpoint {label} {field} differs from the recovery manifest")
                normalized_inspection[field] = normalized
            elif got != wanted:
                fail(f"checkpoint {label} {field} differs from the recovery manifest")
            else:
                normalized_inspection[field] = got
        normalized_inspections.append(normalized_inspection)
    if normalized_inspections[0] != normalized_inspections[1]:
        fail("checkpoint inspect/verify canonical projections differ")
    signature_count = verified.get("signature_count")
    if (
        verified.get("status") != "VERIFIED_QUORUM"
        or isinstance(signature_count, bool)
        or not isinstance(signature_count, int)
        or not recovery.REQUIRED_APPROVALS
        <= signature_count
        <= recovery.REQUIRED_VALIDATORS
    ):
        fail("checkpoint does not carry a verified 5-of-6 signature quorum")

    certificates = []
    expected_validators = manifest["validators"]
    expected_stake_by_address = {
        bare_hash(row["address"], f"manifest validator {index} address"): row["stake"]
        for index, row in enumerate(expected_validators)
    }
    for label, value in (("inspect", inspected), ("verify", verified)):
        raw_certificate = {
            key: value.get(key) for key in ("signing_hash", "validators", "signatures")
        }
        signing_hash = bare_hash(
            raw_certificate["signing_hash"],
            f"checkpoint {label} certificate signing hash",
        )
        raw_validators = raw_certificate["validators"]
        if not isinstance(raw_validators, list) or len(raw_validators) != recovery.REQUIRED_VALIDATORS:
            fail(f"checkpoint {label} certificate must contain six validators")
        validators = []
        validator_by_address: dict[str, dict[str, Any]] = {}
        previous_address = ""
        for index, raw in enumerate(raw_validators):
            row = require_exact_keys(
                raw,
                {"address", "public_key", "stake"},
                f"checkpoint {label} certificate validator {index}",
            )
            address = bare_hash(
                row["address"], f"checkpoint {label} validator {index} address"
            )
            public_key = exact_hash(
                row["public_key"], f"checkpoint {label} validator {index} public key"
            )
            stake = row["stake"]
            if (
                isinstance(stake, bool)
                or not isinstance(stake, int)
                or stake <= 0
                or expected_stake_by_address.get(address) != stake
                or address in validator_by_address
                or address <= previous_address
            ):
                fail(
                    f"checkpoint {label} certificate validator {index} "
                    "differs from the sealed authority set"
                )
            normalized = {
                "address": address,
                "public_key": public_key,
                "stake": stake,
            }
            validators.append(normalized)
            validator_by_address[address] = normalized
            previous_address = address
        if set(validator_by_address) != set(expected_stake_by_address):
            fail(f"checkpoint {label} certificate authority membership differs")

        raw_signatures = raw_certificate["signatures"]
        if not isinstance(raw_signatures, list) or len(raw_signatures) != signature_count:
            fail(f"checkpoint {label} certificate signature count differs")
        signatures = []
        seen_signers: set[str] = set()
        signed_stake = 0
        previous_signer = ""
        for index, raw in enumerate(raw_signatures):
            row = require_exact_keys(
                raw,
                {"validator", "public_key", "signature"},
                f"checkpoint {label} certificate signature {index}",
            )
            validator = bare_hash(
                row["validator"],
                f"checkpoint {label} certificate signer {index}",
            )
            public_key = exact_hash(
                row["public_key"],
                f"checkpoint {label} certificate signer {index} public key",
            )
            signature = row["signature"]
            authority = validator_by_address.get(validator)
            if (
                not isinstance(signature, str)
                or SIGNATURE_RE.fullmatch(signature) is None
                or authority is None
                or authority["public_key"] != public_key
                or validator in seen_signers
                or validator <= previous_signer
            ):
                fail(f"checkpoint {label} certificate signer {index} differs")
            seen_signers.add(validator)
            signed_stake += authority["stake"]
            previous_signer = validator
            signatures.append(
                {
                    "validator": validator,
                    "public_key": public_key,
                    "signature": signature,
                }
            )
        total_stake = sum(row["stake"] for row in validators)
        if len(seen_signers) < recovery.REQUIRED_APPROVALS or signed_stake * 3 <= total_stake * 2:
            fail(f"checkpoint {label} certificate lacks strict identity/stake quorum")
        certificates.append(
            {
                "signing_hash": signing_hash,
                "validators": validators,
                "signatures": signatures,
            }
        )
    if certificates[0] != certificates[1]:
        fail("checkpoint inspect/verify certificate projections differ")
    return signature_count, certificates[1], normalized_inspections[1]


def canonical_validators(manifest: Mapping[str, Any]) -> list[dict[str, Any]]:
    validators: list[dict[str, Any]] = []
    for index, (node, expected) in enumerate(
        zip(manifest["validators"], EXPECTED_RECOVERY_VALIDATORS)
    ):
        name, host, address, stake = expected
        normalized_address = bare_hash(
            node["address"], f"validator {node['name']} address"
        )
        if (node["name"], node["host"], normalized_address, node["stake"]) != expected:
            fail(f"validator {index} differs from the fixed ARC recovery authority")
        validators.append(
            {
                "name": name,
                "host": host,
                "origin": f"http://{host}:9090",
                "address": address,
                "stake": stake,
            }
        )
    if len(validators) != len(EXPECTED_RECOVERY_VALIDATORS):
        fail("recovery manifest does not contain the exact six ARC validators")
    return validators


def build_checkpoint_descriptor(
    *,
    repository: str,
    tag: str,
    commit: str,
    manifest_sha256: str,
    checkpoint_sha256: str,
    checkpoint_size_bytes: int,
    inspector_binary_sha256: str,
    boundary: Mapping[str, Any],
    manifest: Mapping[str, Any],
    signature_count: int,
    checkpoint_certificate: Mapping[str, Any],
    checkpoint_inspection: Mapping[str, Any],
) -> dict[str, Any]:
    validators = canonical_validators(manifest)
    checkpoint_identity = {
        "format_version": checkpoint_inspection["format_version"],
        "chain_id": checkpoint_inspection["chain_id"],
        "manifest_hash": checkpoint_inspection["manifest_hash"],
        "payload_hash": checkpoint_inspection["payload_hash"],
        "network_genesis_hash": checkpoint_inspection["genesis_hash"],
        "full_state_root": checkpoint_inspection["full_state_root"],
        "source_height": checkpoint_inspection["source_height"],
        "source_consensus_round": checkpoint_inspection["source_consensus_round"],
        "created_at_unix_ms": checkpoint_inspection["created_at_unix_ms"],
        "source_block_hash": checkpoint_inspection["source_block_hash"],
        "source_state_root": checkpoint_inspection["source_state_root"],
        "transition_height": checkpoint_inspection["transition_height"],
        "transition_block_hash": checkpoint_inspection["transition_block_hash"],
        "recovery_domain": checkpoint_inspection["recovery_domain"],
        "recovery_epoch": checkpoint_inspection["recovery_epoch"],
        "validator_set_id": checkpoint_inspection["validator_set_id"],
        "protocol_version": checkpoint_inspection["protocol_version"],
        "validator_count": checkpoint_inspection["validator_count"],
        "community_rewards_v1_activation_height": checkpoint_inspection[
            "community_rewards_v1_activation_height"
        ],
    }
    return {
        "schema_version": "arc-recovery-checkpoint-descriptor/v1",
        "repository": repository,
        "release_tag": tag,
        "release_commit": commit,
        "recovery_manifest_sha256": manifest_sha256,
        "freeze_plan_sha256": boundary["freeze_plan_sha256"],
        "capture_id": boundary["capture_id"],
        "inspector_binary_sha256": inspector_binary_sha256,
        "checkpoint_file": {
            "filename": "recovery.arcchkpt",
            "size_bytes": checkpoint_size_bytes,
            "sha256": checkpoint_sha256,
        },
        "canonical_inspection": checkpoint_identity,
        "checkpoint_certificate": checkpoint_certificate,
        "approved_validators": validators,
        "verified_quorum": {
            "status": "VERIFIED_QUORUM",
            "required_signatures": recovery.REQUIRED_APPROVALS,
            "verified_signature_count": signature_count,
            "validator_count": recovery.REQUIRED_VALIDATORS,
            "signed_validator_addresses": [
                row["validator"] for row in checkpoint_certificate["signatures"]
            ],
            "signed_stake": sum(
                next(
                    validator["stake"]
                    for validator in checkpoint_certificate["validators"]
                    if validator["address"] == signature["validator"]
                )
                for signature in checkpoint_certificate["signatures"]
            ),
            "total_stake": sum(
                row["stake"] for row in checkpoint_certificate["validators"]
            ),
        },
    }


def copy_create_only(source: Path, destination: Path, mode: int = 0o644) -> None:
    if destination.exists() or destination.is_symlink():
        fail(f"refusing to replace derived release asset: {destination.name}")
    source_handle = source.open("rb")
    try:
        descriptor = os.open(
            destination,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
            mode,
        )
        try:
            with os.fdopen(descriptor, "wb", closefd=False) as output:
                shutil.copyfileobj(source_handle, output, 1024 * 1024)
                output.flush()
                os.fsync(output.fileno())
            os.fchmod(descriptor, mode)
        finally:
            os.close(descriptor)
    finally:
        source_handle.close()


def write_create_only(destination: Path, payload: bytes, mode: int = 0o644) -> None:
    if destination.exists() or destination.is_symlink():
        fail(f"refusing to replace derived release asset: {destination.name}")
    descriptor = os.open(
        destination,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
        mode,
    )
    try:
        offset = 0
        while offset < len(payload):
            written = os.write(descriptor, payload[offset:])
            if written <= 0:
                fail("cutover policy write made no progress")
            offset += written
        os.fchmod(descriptor, mode)
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def build_policy(
    *,
    repository: str,
    tag: str,
    commit: str,
    manifest_sha256: str,
    boundary_sha256: str,
    checkpoint_descriptor_sha256: str,
    checkpoint_file_sha256: str,
    checkpoint_descriptor: Mapping[str, Any],
    boundary: Mapping[str, Any],
    manifest: Mapping[str, Any],
) -> dict[str, Any]:
    validators = canonical_validators(manifest)
    identity = checkpoint_descriptor["canonical_inspection"]
    quorum = checkpoint_descriptor["verified_quorum"]
    return {
        "schema_version": "arc-cutover-policy/v1",
        "repository": repository,
        "release_tag": tag,
        "release_commit": commit,
        "recovery_manifest_sha256": manifest_sha256,
        "legacy_maintenance_boundary_sha256": boundary_sha256,
        "recovery_checkpoint_descriptor_sha256": checkpoint_descriptor_sha256,
        "recovery_checkpoint_file_sha256": checkpoint_file_sha256,
        "freeze_plan_sha256": boundary["freeze_plan_sha256"],
        "capture_id": boundary["capture_id"],
        "first_quarantine_started_at": boundary["first_quarantine_started_at"],
        "all_controlled_stopped_at": boundary["all_controlled_stopped_at"],
        "legacy_admission_cutoff_utc": boundary["all_controlled_stopped_at"],
        "canonical_boundary_height": identity["source_height"],
        "required_post_cutover_min_height": identity["transition_height"],
        "required_recovery_epoch": identity["recovery_epoch"],
        "required_validator_set_id": identity["validator_set_id"],
        "required_validator_count": identity["validator_count"],
        "checkpoint_format_version": identity["format_version"],
        "chain_id": identity["chain_id"],
        "protocol_version": identity["protocol_version"],
        "payload_hash": identity["payload_hash"],
        "community_rewards_v1_activation_height": identity[
            "community_rewards_v1_activation_height"
        ],
        "network_genesis_hash": identity["network_genesis_hash"],
        "source_block_hash": identity["source_block_hash"],
        "source_state_root": identity["source_state_root"],
        "transition_block_hash": identity["transition_block_hash"],
        "full_state_root": identity["full_state_root"],
        "recovery_domain": identity["recovery_domain"],
        "checkpoint_manifest_hash": identity["manifest_hash"],
        "checkpoint_source_consensus_round": identity["source_consensus_round"],
        "checkpoint_created_at_unix_ms": identity["created_at_unix_ms"],
        "checkpoint_quorum": dict(quorum),
        "legacy_validators": validators,
        "legacy_worker_rpc": {
            "claim_path": "/community/claim_work",
            "submit_path": "/community/submit_work",
            "listener_ports": OLD_CLAIM_SUBMIT_LISTENER_PORTS,
        },
        "uncompleted_job_disposition": "expired_noncanonical_at_cutover",
        "legacy_exit_clean_claimed": False,
        "legacy_restart_allowed": False,
        "global_legacy_absence_claimed": False,
        "offline_retirement_receipt_required": True,
        "v08_start_requires_offline_receipt": True,
    }


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--handoff-dir", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--verifier-binary", required=True, type=Path)
    parser.add_argument("--inspector-binary", required=True, type=Path)
    parser.add_argument("--genesis", required=True, type=Path)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        if args.repository != "FerrumVir/arc-chain":
            fail("cutover assets are restricted to FerrumVir/arc-chain")
        tag_match = TAG_RE.fullmatch(args.tag)
        if tag_match is None:
            fail("release tag must be strict vX.Y.Z")
        if COMMIT_RE.fullmatch(args.commit) is None:
            fail("release commit must be one full lowercase Git SHA")
        if not args.handoff_dir.is_dir() or args.handoff_dir.is_symlink():
            fail("cutover handoff directory is missing or symlinked")
        if {path.name for path in args.handoff_dir.iterdir()} != HANDOFF_FILES:
            fail("cutover handoff membership differs from the exact four-file contract")
        if not args.output_dir.is_dir() or args.output_dir.is_symlink():
            fail("release output staging directory is missing or symlinked")

        manifest_path = args.handoff_dir / "arc-recovery-final.lock.json"
        boundary_path = args.handoff_dir / "legacy-maintenance-boundary.json"
        checkpoint_path = args.handoff_dir / "recovery.arcchkpt"
        safe_regular(manifest_path, "sealed recovery manifest", 32 * 1024 * 1024)
        safe_regular(
            args.handoff_dir / "arc-recovery-final.lock.json.sha256",
            "sealed recovery manifest sidecar",
            512,
        )
        safe_regular(boundary_path, "legacy maintenance boundary", 16 * 1024 * 1024)
        safe_regular(checkpoint_path, "recovery checkpoint", 8 * 1024**3)
        for path in args.handoff_dir.iterdir():
            if path.lstat().st_mode & 0o222:
                fail(f"protected handoff file retains write bits: {path.name}")

        try:
            manifest, manifest_sha256 = recovery.load_sealed_manifest(manifest_path)
        except recovery.RolloutError as error:
            fail(f"sealed recovery manifest is invalid: {error}")
        if manifest["mode"] != "production":
            fail("cutover release requires a sealed production recovery manifest")
        if manifest["provenance"]["pretag_repository"] != args.repository:
            fail("recovery manifest repository differs from the release")
        version = ".".join(tag_match.groups())
        if manifest["provenance"]["pretag_version"] != version:
            fail("recovery manifest version differs from the release tag")
        if any(
            manifest["archive"][field] == "0" * 64
            for field in recovery.ARCHIVE_FINALIZATION_FIELDS
        ):
            fail("cutover release requires the roots-only finalized recovery manifest")

        boundary, boundary_payload = read_canonical_json(
            boundary_path, "legacy maintenance boundary", 16 * 1024 * 1024
        )
        validate_boundary(
            boundary,
            boundary_payload,
            manifest,
            manifest_sha256,
            args.commit,
        )

        checkpoint_details = checkpoint_path.lstat()
        checkpoint_sha256 = sha256_file(checkpoint_path)
        safe_regular(
            args.inspector_binary,
            "checkpoint inspector binary",
            4 * 1024**3,
            executable=True,
        )
        binary_sha256 = sha256_file(args.inspector_binary)
        genesis_sha256 = sha256_file(args.genesis)
        artifacts = manifest["artifacts"]
        if checkpoint_sha256 != artifacts["checkpoint"]["sha256"]:
            fail("checkpoint bytes differ from the sealed recovery artifact")
        if binary_sha256 != artifacts["binary"]["sha256"]:
            fail("release verifier binary differs from the sealed recovery artifact")
        if genesis_sha256 != artifacts["genesis"]["sha256"]:
            fail("release genesis differs from the sealed recovery artifact")
        if boundary["tools"]["inspector_binary_sha256"] != binary_sha256:
            fail("maintenance boundary inspector binary differs from the release")
        if boundary["tools"]["genesis_sha256"] != genesis_sha256:
            fail("maintenance boundary genesis differs from the release")

        chain = manifest["chain"]
        if (
            chain["source_height"] != CANONICAL_BOUNDARY_HEIGHT
            or chain["legacy_public_max_height"] != CANONICAL_BOUNDARY_HEIGHT
            or chain["transition_height"] != REQUIRED_POST_CUTOVER_MIN_HEIGHT
            or chain["recovery_epoch"] != 1
            or chain["validator_set_id"] != 1
            or chain["protocol_version"] != "3.0.0"
            or len(manifest["validators"]) != recovery.REQUIRED_VALIDATORS
        ):
            fail(
                "recovery manifest does not bind canonical v3.0.0 H/H+1, "
                "legacy ceiling H, epoch 1, set 1, and six validators"
            )

        inspected, verified = run_checkpoint_cli(
            args.verifier_binary, checkpoint_path, args.genesis, chain
        )
        (
            signature_count,
            checkpoint_certificate,
            checkpoint_inspection,
        ) = validate_checkpoint_outputs(
            inspected, verified, manifest, chain
        )
        checkpoint_details_after = checkpoint_path.lstat()
        if (
            checkpoint_details_after.st_dev,
            checkpoint_details_after.st_ino,
            checkpoint_details_after.st_size,
            checkpoint_details_after.st_mtime_ns,
            sha256_file(checkpoint_path),
        ) != (
            checkpoint_details.st_dev,
            checkpoint_details.st_ino,
            checkpoint_details.st_size,
            checkpoint_details.st_mtime_ns,
            checkpoint_sha256,
        ):
            fail("checkpoint input changed while it was being inspected")
        boundary_sha256 = sha256_bytes(boundary_payload)
        checkpoint_descriptor = build_checkpoint_descriptor(
            repository=args.repository,
            tag=args.tag,
            commit=args.commit,
            manifest_sha256=manifest_sha256,
            checkpoint_sha256=checkpoint_sha256,
            checkpoint_size_bytes=checkpoint_details.st_size,
            inspector_binary_sha256=binary_sha256,
            boundary=boundary,
            manifest=manifest,
            signature_count=signature_count,
            checkpoint_certificate=checkpoint_certificate,
            checkpoint_inspection=checkpoint_inspection,
        )
        checkpoint_descriptor_payload = canonical_json(checkpoint_descriptor)
        if len(checkpoint_descriptor_payload) > 1024 * 1024:
            fail("checkpoint descriptor exceeds the one-MiB public contract")
        checkpoint_descriptor_sha256 = sha256_bytes(checkpoint_descriptor_payload)
        policy = build_policy(
            repository=args.repository,
            tag=args.tag,
            commit=args.commit,
            manifest_sha256=manifest_sha256,
            boundary_sha256=boundary_sha256,
            checkpoint_descriptor_sha256=checkpoint_descriptor_sha256,
            checkpoint_file_sha256=checkpoint_sha256,
            checkpoint_descriptor=checkpoint_descriptor,
            boundary=boundary,
            manifest=manifest,
        )

        copy_create_only(boundary_path, args.output_dir / BOUNDARY_OUTPUT)
        write_create_only(
            args.output_dir / CHECKPOINT_DESCRIPTOR_OUTPUT,
            checkpoint_descriptor_payload,
        )
        write_create_only(args.output_dir / POLICY_OUTPUT, canonical_json(policy))
        directory_fd = os.open(args.output_dir, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
        print(
            "cutover release assets: verified finalized recovery, canonical boundary, "
            f"and {signature_count}-of-6 ARCCHKPT quorum"
        )
        return 0
    except CutoverAssetError as error:
        print(f"cutover release assets: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
