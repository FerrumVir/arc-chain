#!/usr/bin/env python3
"""Validate the compact cutover trust assets without the full ARCCHKPT payload."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import re
import subprocess
import sys
from pathlib import Path
from typing import Any, Mapping, NoReturn, Sequence


SCRIPT_DIR = Path(__file__).resolve().parent
ASSEMBLER = SCRIPT_DIR / "assemble-cutover-assets.py"
SPEC = importlib.util.spec_from_file_location("arc_assemble_cutover_assets", ASSEMBLER)
if SPEC is None or SPEC.loader is None:
    raise SystemExit("derived cutover validation: cannot load cutover asset validator")
cutover = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = cutover
SPEC.loader.exec_module(cutover)


PUBLIC_FILES = {
    "arc-legacy-maintenance-boundary.json",
    "arc-recovery-checkpoint-descriptor.json",
    "arc-cutover-policy.json",
}
HASH_RE = re.compile(r"^[0-9a-f]{64}$")
SIGNATURE_RE = re.compile(r"^[0-9a-f]{128}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
TAG_RE = re.compile(r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")


class DerivedAssetError(RuntimeError):
    pass


def fail(message: str) -> NoReturn:
    raise DerivedAssetError(message)


def require_keys(value: Any, expected: set[str], label: str) -> Mapping[str, Any]:
    if not isinstance(value, dict) or set(value) != expected:
        fail(f"{label} fields differ from the exact v1 contract")
    return value


def require_hash(value: Any, label: str) -> str:
    if not isinstance(value, str) or HASH_RE.fullmatch(value) is None:
        fail(f"{label} must be one lowercase SHA-256/hash")
    return value


def require_int(
    value: Any, label: str, *, minimum: int = 0, maximum: int = 2**63 - 1
) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not minimum <= value <= maximum
    ):
        fail(f"{label} is outside its integer contract")
    return value


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_json(path: Path, label: str, maximum: int) -> tuple[dict[str, Any], bytes]:
    try:
        return cutover.read_canonical_json(path, label, maximum)
    except cutover.CutoverAssetError as error:
        fail(str(error))


def validate_certificate(
    certificate: Any,
    approved_validators: Sequence[Mapping[str, Any]],
) -> tuple[dict[str, Any], dict[str, Any]]:
    value = require_keys(
        certificate,
        {"signing_hash", "validators", "signatures"},
        "checkpoint certificate",
    )
    signing_hash = require_hash(value["signing_hash"], "checkpoint signing hash")
    raw_validators = value["validators"]
    if not isinstance(raw_validators, list) or len(raw_validators) != 6:
        fail("checkpoint certificate must contain six validators")
    validators: list[dict[str, Any]] = []
    by_address: dict[str, dict[str, Any]] = {}
    previous = ""
    for index, raw in enumerate(raw_validators):
        row = require_keys(
            raw, {"address", "public_key", "stake"}, f"certificate validator {index}"
        )
        address = require_hash(row["address"], f"certificate validator {index} address")
        public_key = require_hash(
            row["public_key"], f"certificate validator {index} public key"
        )
        stake = require_int(row["stake"], f"certificate validator {index} stake", minimum=1)
        if address <= previous or address in by_address:
            fail("checkpoint certificate validators are not strictly address sorted")
        normalized = {"address": address, "public_key": public_key, "stake": stake}
        validators.append(normalized)
        by_address[address] = normalized
        previous = address

    approved_stakes = {row["address"]: row["stake"] for row in approved_validators}
    if {address: row["stake"] for address, row in by_address.items()} != approved_stakes:
        fail("checkpoint certificate authority set differs from approved validators")

    raw_signatures = value["signatures"]
    if not isinstance(raw_signatures, list) or not 5 <= len(raw_signatures) <= 6:
        fail("checkpoint certificate must contain five or six signatures")
    signatures: list[dict[str, str]] = []
    signed_stake = 0
    previous = ""
    for index, raw in enumerate(raw_signatures):
        row = require_keys(
            raw,
            {"validator", "public_key", "signature"},
            f"checkpoint signature {index}",
        )
        validator = require_hash(row["validator"], f"checkpoint signer {index}")
        public_key = require_hash(
            row["public_key"], f"checkpoint signer {index} public key"
        )
        signature = row["signature"]
        authority = by_address.get(validator)
        if (
            not isinstance(signature, str)
            or SIGNATURE_RE.fullmatch(signature) is None
            or authority is None
            or authority["public_key"] != public_key
            or validator <= previous
        ):
            fail(f"checkpoint signature {index} differs from its ordered authority")
        signatures.append(
            {"validator": validator, "public_key": public_key, "signature": signature}
        )
        signed_stake += authority["stake"]
        previous = validator
    total_stake = sum(row["stake"] for row in validators)
    if signed_stake * 3 <= total_stake * 2:
        fail("checkpoint certificate lacks strict signed-stake supermajority")
    normalized_certificate = {
        "signing_hash": signing_hash,
        "validators": validators,
        "signatures": signatures,
    }
    quorum = {
        "status": "VERIFIED_QUORUM",
        "required_signatures": 5,
        "verified_signature_count": len(signatures),
        "validator_count": 6,
        "signed_validator_addresses": [row["validator"] for row in signatures],
        "signed_stake": signed_stake,
        "total_stake": total_stake,
    }
    return normalized_certificate, quorum


def validate_descriptor(
    descriptor: dict[str, Any], *, repository: str, tag: str, commit: str
) -> None:
    require_keys(
        descriptor,
        {
            "approved_validators",
            "canonical_inspection",
            "capture_id",
            "checkpoint_certificate",
            "checkpoint_file",
            "freeze_plan_sha256",
            "inspector_binary_sha256",
            "recovery_manifest_sha256",
            "release_commit",
            "release_tag",
            "repository",
            "schema_version",
            "verified_quorum",
        },
        "checkpoint descriptor",
    )
    if (
        descriptor["schema_version"] != "arc-recovery-checkpoint-descriptor/v1"
        or descriptor["repository"] != repository
        or descriptor["release_tag"] != tag
        or descriptor["release_commit"] != commit
    ):
        fail("checkpoint descriptor release identity differs")
    for key in (
        "capture_id",
        "freeze_plan_sha256",
        "inspector_binary_sha256",
        "recovery_manifest_sha256",
    ):
        require_hash(descriptor[key], f"checkpoint descriptor {key}")

    checkpoint_file = require_keys(
        descriptor["checkpoint_file"],
        {"filename", "size_bytes", "sha256"},
        "checkpoint file descriptor",
    )
    if checkpoint_file["filename"] != "recovery.arcchkpt":
        fail("checkpoint descriptor filename differs")
    require_int(
        checkpoint_file["size_bytes"],
        "checkpoint file size",
        minimum=1,
        maximum=8 * 1024**3,
    )
    require_hash(checkpoint_file["sha256"], "checkpoint file SHA-256")

    raw_approved = descriptor["approved_validators"]
    if not isinstance(raw_approved, list) or len(raw_approved) != 6:
        fail("checkpoint descriptor must contain six approved validators")
    approved: list[dict[str, Any]] = []
    expected_fleet = list(cutover.EXPECTED_RECOVERY_VALIDATORS)
    for index, (raw, (expected_name, expected_host, expected_address, expected_stake)) in enumerate(
        zip(raw_approved, expected_fleet)
    ):
        row = require_keys(
            raw,
            {"address", "host", "name", "origin", "stake"},
            f"approved validator {index}",
        )
        address = require_hash(row["address"], f"approved validator {index} address")
        stake = require_int(row["stake"], f"approved validator {index} stake", minimum=1)
        if (
            row["name"],
            row["host"],
            row["origin"],
            address,
            stake,
        ) != (
            expected_name,
            expected_host,
            f"http://{expected_host}:9090",
            expected_address,
            expected_stake,
        ):
            fail(f"approved validator {index} fleet identity differs")
        approved.append(
            {
                "address": address,
                "host": expected_host,
                "name": expected_name,
                "origin": f"http://{expected_host}:9090",
                "stake": stake,
            }
        )
    if len({row["address"] for row in approved}) != 6:
        fail("approved validator addresses are not unique")

    normalized_certificate, expected_quorum = validate_certificate(
        descriptor["checkpoint_certificate"], approved
    )
    if descriptor["checkpoint_certificate"] != normalized_certificate:
        fail("checkpoint certificate is not normalized")
    if descriptor["verified_quorum"] != expected_quorum:
        fail("checkpoint descriptor quorum projection differs from its certificate")

    inspection = require_keys(
        descriptor["canonical_inspection"],
        {
            "chain_id",
            "community_rewards_v1_activation_height",
            "created_at_unix_ms",
            "format_version",
            "full_state_root",
            "manifest_hash",
            "network_genesis_hash",
            "payload_hash",
            "protocol_version",
            "recovery_domain",
            "recovery_epoch",
            "source_block_hash",
            "source_consensus_round",
            "source_height",
            "source_state_root",
            "transition_block_hash",
            "transition_height",
            "validator_count",
            "validator_set_id",
        },
        "canonical checkpoint inspection",
    )
    if (
        inspection["format_version"] != 1
        or inspection["chain_id"] != "0x415243"
        or inspection["protocol_version"] != "3.0.0"
        or inspection["source_height"] != 137145
        or inspection["transition_height"] != 137146
        or inspection["community_rewards_v1_activation_height"] != 137146
        or inspection["recovery_epoch"] != 1
        or inspection["validator_set_id"] != 1
        or inspection["validator_count"] != 6
    ):
        fail("canonical checkpoint v3/H/H+1/activation/authority identity differs")
    for key in (
        "manifest_hash",
        "payload_hash",
        "network_genesis_hash",
        "full_state_root",
        "source_block_hash",
        "source_state_root",
        "transition_block_hash",
        "recovery_domain",
    ):
        require_hash(inspection[key], f"canonical inspection {key}")
    if inspection["network_genesis_hash"] == "0" * 64:
        fail("canonical inspection network genesis must not be zero")
    require_int(inspection["source_consensus_round"], "source consensus round")
    require_int(inspection["created_at_unix_ms"], "checkpoint creation time", minimum=1)


def validate_boundary(
    boundary: dict[str, Any], descriptor: Mapping[str, Any], commit: str
) -> None:
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
    require_keys(boundary, expected_fields, "legacy maintenance boundary")
    if (
        boundary["schema"] != "arc.recovery.legacy-maintenance-boundary.v1"
        or boundary["source_main_commit"] != commit
        or boundary["freeze_plan_sha256"] != descriptor["freeze_plan_sha256"]
        or boundary["capture_id"] != descriptor["capture_id"]
        or boundary["legacy_public_max_height"] != 137145
        or boundary["global_absence_claimed"] is not False
    ):
        fail("legacy maintenance boundary identity differs")
    expected_origins = [dict(row) for row in cutover.recovery.LEGACY_OFFICIAL_ORIGINS]
    if boundary["official_origin_scope"] != {
        "global_absence_claimed": False,
        "origins": expected_origins,
    }:
        fail("legacy maintenance boundary origin scope differs")
    for key in (
        "first_quarantine_started_at",
        "all_controlled_stopped_at",
        "created_at",
    ):
        try:
            cutover.exact_utc(boundary[key], f"legacy maintenance boundary {key}")
        except cutover.CutoverAssetError as error:
            fail(str(error))


def validate_policy(
    policy: dict[str, Any],
    descriptor: Mapping[str, Any],
    descriptor_sha256: str,
    boundary: Mapping[str, Any],
    boundary_sha256: str,
    *,
    repository: str,
    tag: str,
    commit: str,
) -> None:
    expected_fields = {
        "all_controlled_stopped_at",
        "canonical_boundary_height",
        "capture_id",
        "chain_id",
        "checkpoint_created_at_unix_ms",
        "checkpoint_format_version",
        "checkpoint_manifest_hash",
        "checkpoint_quorum",
        "checkpoint_source_consensus_round",
        "community_rewards_v1_activation_height",
        "first_quarantine_started_at",
        "freeze_plan_sha256",
        "full_state_root",
        "network_genesis_hash",
        "global_legacy_absence_claimed",
        "legacy_admission_cutoff_utc",
        "legacy_exit_clean_claimed",
        "legacy_maintenance_boundary_sha256",
        "legacy_restart_allowed",
        "legacy_validators",
        "legacy_worker_rpc",
        "offline_retirement_receipt_required",
        "payload_hash",
        "protocol_version",
        "recovery_checkpoint_descriptor_sha256",
        "recovery_checkpoint_file_sha256",
        "recovery_domain",
        "recovery_manifest_sha256",
        "release_commit",
        "release_tag",
        "repository",
        "required_post_cutover_min_height",
        "required_recovery_epoch",
        "required_validator_count",
        "required_validator_set_id",
        "schema_version",
        "source_block_hash",
        "source_state_root",
        "transition_block_hash",
        "uncompleted_job_disposition",
        "v08_start_requires_offline_receipt",
    }
    require_keys(policy, expected_fields, "cutover policy")
    identity = descriptor["canonical_inspection"]
    if (
        policy["schema_version"] != "arc-cutover-policy/v1"
        or (policy["repository"], policy["release_tag"], policy["release_commit"])
        != (repository, tag, commit)
        or policy["recovery_manifest_sha256"]
        != descriptor["recovery_manifest_sha256"]
        or policy["legacy_maintenance_boundary_sha256"] != boundary_sha256
        or policy["recovery_checkpoint_descriptor_sha256"] != descriptor_sha256
        or policy["recovery_checkpoint_file_sha256"]
        != descriptor["checkpoint_file"]["sha256"]
        or policy["freeze_plan_sha256"] != boundary["freeze_plan_sha256"]
        or policy["capture_id"] != boundary["capture_id"]
        or policy["first_quarantine_started_at"]
        != boundary["first_quarantine_started_at"]
        or policy["all_controlled_stopped_at"]
        != boundary["all_controlled_stopped_at"]
        or policy["legacy_admission_cutoff_utc"]
        != boundary["all_controlled_stopped_at"]
    ):
        fail("cutover policy provenance/hash/time binding differs")
    projected = {
        "canonical_boundary_height": "source_height",
        "required_post_cutover_min_height": "transition_height",
        "required_recovery_epoch": "recovery_epoch",
        "required_validator_set_id": "validator_set_id",
        "required_validator_count": "validator_count",
        "checkpoint_format_version": "format_version",
        "chain_id": "chain_id",
        "protocol_version": "protocol_version",
        "payload_hash": "payload_hash",
        "community_rewards_v1_activation_height": "community_rewards_v1_activation_height",
        "network_genesis_hash": "network_genesis_hash",
        "source_block_hash": "source_block_hash",
        "source_state_root": "source_state_root",
        "transition_block_hash": "transition_block_hash",
        "full_state_root": "full_state_root",
        "recovery_domain": "recovery_domain",
        "checkpoint_manifest_hash": "manifest_hash",
        "checkpoint_source_consensus_round": "source_consensus_round",
        "checkpoint_created_at_unix_ms": "created_at_unix_ms",
    }
    if any(policy[key] != identity[source] for key, source in projected.items()):
        fail("cutover policy checkpoint projection differs from its descriptor")
    if (
        policy["checkpoint_quorum"] != descriptor["verified_quorum"]
        or policy["legacy_validators"] != descriptor["approved_validators"]
        or policy["legacy_worker_rpc"]
        != {
            "claim_path": "/community/claim_work",
            "submit_path": "/community/submit_work",
            "listener_ports": [9090, 3001],
        }
        or policy["uncompleted_job_disposition"]
        != "expired_noncanonical_at_cutover"
        or policy["legacy_exit_clean_claimed"] is not False
        or policy["legacy_restart_allowed"] is not False
        or policy["global_legacy_absence_claimed"] is not False
        or policy["offline_retirement_receipt_required"] is not True
        or policy["v08_start_requires_offline_receipt"] is not True
    ):
        fail("cutover policy retirement/quorum/fleet contract differs")


def verify_descriptor_with_node(
    binary: Path, descriptor: Path, genesis: Path
) -> None:
    try:
        cutover.safe_regular(binary, "descriptor verifier binary", 2 * 1024**3, executable=True)
        cutover.safe_regular(genesis, "descriptor verifier genesis", 16 * 1024 * 1024)
    except cutover.CutoverAssetError as error:
        fail(str(error))
    environment = {
        "HOME": "/var/empty",
        "PATH": "/usr/bin:/bin",
        "LANG": "C",
        "LC_ALL": "C",
        "TZ": "UTC",
        "RUST_BACKTRACE": "0",
    }
    try:
        completed = subprocess.run(
            [
                os.fspath(binary),
                "recovery",
                "verify-descriptor",
                "--descriptor",
                os.fspath(descriptor),
                "--genesis",
                os.fspath(genesis),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd="/",
            env=environment,
            timeout=120,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        fail(f"compiled checkpoint descriptor verification failed: {error}")
    if completed.returncode != 0 or len(completed.stdout) > 1024 * 1024:
        diagnostic = completed.stderr.decode("utf-8", errors="replace")[:2000]
        fail(f"compiled checkpoint descriptor verifier rejected the certificate: {diagnostic}")
    try:
        result = json.loads(completed.stdout)
    except (UnicodeError, json.JSONDecodeError) as error:
        fail(f"compiled checkpoint descriptor verifier returned invalid JSON: {error}")
    if not isinstance(result, dict) or result.get("status") != "VERIFIED_DESCRIPTOR_QUORUM":
        fail("compiled checkpoint descriptor verifier did not prove quorum")


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input-dir", required=True, type=Path)
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--verifier-binary", required=True, type=Path)
    parser.add_argument("--inspector-binary", type=Path)
    parser.add_argument("--genesis", required=True, type=Path)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        if args.repository != "FerrumVir/arc-chain":
            fail("derived cutover assets are restricted to FerrumVir/arc-chain")
        if TAG_RE.fullmatch(args.tag) is None or COMMIT_RE.fullmatch(args.commit) is None:
            fail("derived cutover release identity is malformed")
        if not args.input_dir.is_dir() or args.input_dir.is_symlink():
            fail("derived cutover input directory is missing or symlinked")
        if {path.name for path in args.input_dir.iterdir()} != PUBLIC_FILES:
            fail("derived cutover input membership differs from the exact three-file contract")

        boundary_path = args.input_dir / "arc-legacy-maintenance-boundary.json"
        descriptor_path = args.input_dir / "arc-recovery-checkpoint-descriptor.json"
        policy_path = args.input_dir / "arc-cutover-policy.json"
        boundary, boundary_payload = read_json(
            boundary_path, "legacy maintenance boundary", 16 * 1024 * 1024
        )
        descriptor, descriptor_payload = read_json(
            descriptor_path, "checkpoint descriptor", 1024 * 1024
        )
        policy, _policy_payload = read_json(policy_path, "cutover policy", 1024 * 1024)
        validate_descriptor(
            descriptor, repository=args.repository, tag=args.tag, commit=args.commit
        )
        validate_boundary(boundary, descriptor, args.commit)
        validate_policy(
            policy,
            descriptor,
            hashlib.sha256(descriptor_payload).hexdigest(),
            boundary,
            hashlib.sha256(boundary_payload).hexdigest(),
            repository=args.repository,
            tag=args.tag,
            commit=args.commit,
        )
        if args.inspector_binary is not None:
            try:
                cutover.safe_regular(
                    args.inspector_binary,
                    "release inspector binary",
                    2 * 1024**3,
                    executable=True,
                )
            except cutover.CutoverAssetError as error:
                fail(str(error))
            if sha256_file(args.inspector_binary) != descriptor["inspector_binary_sha256"]:
                fail("release inspector binary differs from the checkpoint descriptor")
        verify_descriptor_with_node(
            args.verifier_binary, descriptor_path, args.genesis
        )

        if args.output_dir is not None:
            if not args.output_dir.is_dir() or args.output_dir.is_symlink():
                fail("derived cutover output directory is missing or symlinked")
            for filename in sorted(PUBLIC_FILES):
                try:
                    cutover.copy_create_only(
                        args.input_dir / filename, args.output_dir / filename
                    )
                except cutover.CutoverAssetError as error:
                    fail(str(error))
        print("derived cutover assets: verified canonical policy and ARCCHKPT certificate")
        return 0
    except DerivedAssetError as error:
        print(f"derived cutover assets: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
