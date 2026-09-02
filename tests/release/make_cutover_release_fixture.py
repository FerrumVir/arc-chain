#!/usr/bin/env python3
"""Create a deterministic, recovery-validator-backed cutover release fixture."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
RECOVERY_TEST = REPO_ROOT / "scripts" / "recovery" / "test_recovery_rollout.py"
EXPECTED_RECOVERY_VALIDATORS = (
    ("nyc", "adf4ff16f997c871c16f3897e67881311d08f975f28ebdcf79e86ea9e3b99d0f", 6_666_667),
    ("lax", "44d20543df6e76696da2ebbbd79e4243cd41729fa5b890e2618991e489314780", 6_666_667),
    ("ams", "5772741c93d8a4b04ec39007cb568a31e13ffba0d3e786596d1900d30e529f21", 6_666_667),
    ("lhr", "228787281308d6c1a560848c2c168814bde1b6153e9e65a286d7211f04628fdd", 6_666_667),
    ("nrt", "f03cbab49cf553a05541ddebc09b32a4c5507efb157d354b6d7f8c6682c32f5f", 6_666_666),
    ("sgp", "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b", 6_666_666),
)


def load_recovery_test_module():
    spec = importlib.util.spec_from_file_location("arc_cutover_recovery_fixture", RECOVERY_TEST)
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load the existing recovery fixture")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def canonical(value: object) -> bytes:
    return (
        json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
        + "\n"
    ).encode("utf-8")


def replace_prefix(value, old: str, new: str):
    if isinstance(value, dict):
        return {key: replace_prefix(item, old, new) for key, item in value.items()}
    if isinstance(value, list):
        return [replace_prefix(item, old, new) for item in value]
    if isinstance(value, str) and value.startswith(old):
        return new + value[len(old) :]
    return value


def replace_validator_authorities(value, replacements, stake_by_address):
    if isinstance(value, dict):
        replaced = {
            key: replace_validator_authorities(item, replacements, stake_by_address)
            for key, item in value.items()
        }
        address = replaced.get("address") or replaced.get("validator_address")
        if address in stake_by_address and "stake" in replaced:
            replaced["stake"] = stake_by_address[address]
        return replaced
    if isinstance(value, list):
        return [replace_validator_authorities(item, replacements, stake_by_address) for item in value]
    if isinstance(value, str):
        return replacements.get(value.removeprefix("0x"), value)
    return value


def write_exact(path: Path, payload: bytes, mode: int) -> None:
    descriptor = os.open(
        path,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
        mode,
    )
    try:
        offset = 0
        while offset < len(payload):
            written = os.write(descriptor, payload[offset:])
            if written <= 0:
                raise RuntimeError("fixture write made no progress")
            offset += written
        os.fchmod(descriptor, mode)
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--handoff-dir", required=True, type=Path)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--genesis", required=True, type=Path)
    args = parser.parse_args()
    if args.handoff_dir.exists() or args.handoff_dir.is_symlink():
        raise SystemExit("fixture handoff directory must be absent")
    if not args.genesis.is_file() or args.genesis.is_symlink():
        raise SystemExit("fixture genesis is unavailable")

    module = load_recovery_test_module()
    actual_caddy_sha256 = module.rollout.CADDY_LINUX_AMD64_SHA256
    case = module.RecoveryRolloutTests(methodName="test_checkpoint_cli_output_must_match_every_locked_commitment")
    case.setUp()
    try:
        value, _rows, payloads = case.maintenance_stage_fixture()
        temporary_prefix = str(case.root)
    finally:
        case.tearDown()
    value = replace_prefix(value, temporary_prefix, "/fixture")
    boundary = json.loads(payloads["legacy_maintenance_boundary"])

    replacements = {
        validator["address"].removeprefix("0x"): expected_address
        for validator, (_name, expected_address, _stake) in zip(
            value["validators"], EXPECTED_RECOVERY_VALIDATORS
        )
    }
    stake_by_address = {
        address: stake for _name, address, stake in EXPECTED_RECOVERY_VALIDATORS
    }
    value = replace_validator_authorities(value, replacements, stake_by_address)
    for node in value["provenance"]["offline_stop_verification"]["nodes"]:
        node["status_sha256"] = hashlib.sha256(
            canonical(node["status"])
        ).hexdigest()

    chain = value["chain"]
    chain.update(
        {
            "chain_id": "0x415243",
            "genesis_hash": "0x" + "1" * 64,
            "source_height": 137145,
            "transition_height": 137146,
            "recovery_epoch": 1,
            "validator_set_id": 1,
            "legacy_observed_cutoff_height": 137017,
            "legacy_public_max_height": 137145,
        }
    )
    boundary["observed_cutoff_height"] = 137017
    boundary["legacy_public_max_height"] = 137145
    boundary["evidence_heights"][-1]["height"] = 137017

    checkpoint = b"ARCCHKPT deterministic release fixture v1\n"
    checkpoint_sha256 = hashlib.sha256(checkpoint).hexdigest()
    genesis_sha256 = sha256(args.genesis)

    certificate_validators = []
    for index, validator in enumerate(value["validators"]):
        certificate_validators.append(
            {
                "address": "0x" + validator["address"].removeprefix("0x"),
                "public_key": hashlib.sha256(
                    f"fixture-validator-public-key-{index}".encode()
                ).hexdigest(),
                "stake": validator["stake"],
            }
        )
    certificate_validators.sort(key=lambda row: row["address"])
    certificate_signatures = [
        {
            "validator": row["address"],
            "public_key": row["public_key"],
            "signature": hashlib.sha512(
                f"fixture-checkpoint-signature-{index}".encode()
            ).hexdigest(),
        }
        for index, row in enumerate(certificate_validators[:5])
    ]

    output = {
        "status": "VERIFIED_QUORUM",
        "signature_count": 5,
        "format_version": 1,
        "chain_id": chain["chain_id"],
        "manifest_hash": chain["approved_checkpoint_manifest_hash"],
        "payload_hash": "0x" + "8" * 64,
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
        "validator_count": 6,
        "community_rewards_v1_activation_height": 137146,
        "signing_hash": "0x" + "7" * 64,
        "validators": certificate_validators,
        "signatures": certificate_signatures,
    }
    output_json = json.dumps(output, sort_keys=True, separators=(",", ":"))
    descriptor_result = json.dumps(
        {"status": "VERIFIED_DESCRIPTOR_QUORUM"},
        sort_keys=True,
        separators=(",", ":"),
    )
    binary = (
        "#!/bin/sh\n"
        "set -eu\n"
        "case \"${1:-}:${2:-}\" in\n"
        "  recovery:inspect|recovery:verify) "
        f"printf '%s\\n' '{output_json}'; exit 0 ;;\n"
        "  recovery:verify-descriptor) "
        f"printf '%s\\n' '{descriptor_result}'; exit 0 ;;\n"
        "  *) exit 64 ;;\n"
        "esac\n"
    ).encode("utf-8")
    args.binary.parent.mkdir(parents=True, exist_ok=True)
    if args.binary.exists() or args.binary.is_symlink():
        args.binary.unlink()
    write_exact(args.binary, binary, 0o755)
    binary_sha256 = sha256(args.binary)

    artifacts = value["artifacts"]
    artifacts["binary"]["sha256"] = binary_sha256
    artifacts["binary"]["path"] = "/fixture/artifacts/binary"
    artifacts["genesis"]["sha256"] = genesis_sha256
    artifacts["genesis"]["path"] = "/fixture/artifacts/genesis"
    artifacts["checkpoint"]["sha256"] = checkpoint_sha256
    artifacts["checkpoint"]["path"] = "/fixture/artifacts/checkpoint"
    artifacts["caddy"]["sha256"] = actual_caddy_sha256
    value["checks"]["reward"]["probe_argv"][0] = artifacts["reward_probe"]["path"]

    groups = value["provenance"]["protected_pretag_artifact"]["groups"]
    linux = next(
        item
        for item in groups
        if (item["kind"], item["platform"]) == ("headless", "linux-x86_64")
    )
    for window in ("initial", "final"):
        linux[window]["artifact"]["files"]["arc-node-linux-x86_64"] = binary_sha256
        linux[window]["artifact"]["files"]["genesis.toml"] = genesis_sha256
    value["provenance"]["validator_key_receipt_chain"]["genesis_sha256"] = genesis_sha256
    value["provenance"]["validator_key_receipt_chain"][
        "offline_stop_evidence_sha256"
    ] = artifacts["offline_stop_evidence"]["sha256"]
    value["provenance"]["validator_key_receipt_chain"][
        "freeze_plan_sha256"
    ] = value["archive"]["freeze_plan_sha256"]
    installed = value["provenance"]["validator_installed_key_proof"]
    installed["freeze_plan_sha256"] = value["archive"]["freeze_plan_sha256"]
    installed["offline_stop_evidence_sha256"] = artifacts[
        "offline_stop_evidence"
    ]["sha256"]

    boundary["tools"]["inspector_binary_sha256"] = binary_sha256
    boundary["tools"]["genesis_sha256"] = genesis_sha256
    boundary_payload = canonical(boundary)
    boundary_sha256 = hashlib.sha256(boundary_payload).hexdigest()
    artifacts["legacy_maintenance_boundary"]["sha256"] = boundary_sha256
    chain["legacy_maintenance_boundary_sha256"] = boundary_sha256

    for field, digit in (
        ("complete_sha256", "a"),
        ("archive_manifest_sha256", "b"),
        ("sha256sums_sha256", "c"),
    ):
        value["archive"][field] = digit * 64
    value["archive"]["prearchive_rollout_sha256"] = "0" * 64
    value["archive"]["prearchive_rollout_sha256"] = module.rollout.prearchive_projection_digest(value)

    # Validate the generated production manifest with the same strict recovery
    # validator used by release assembly before writing any handoff bytes.
    module.rollout.validate_manifest(value)
    manifest_payload = module.rollout.canonical_bytes(value)
    manifest_sha256 = hashlib.sha256(manifest_payload).hexdigest()

    args.handoff_dir.mkdir(parents=True, mode=0o700)
    write_exact(
        args.handoff_dir / "arc-recovery-final.lock.json", manifest_payload, 0o444
    )
    write_exact(
        args.handoff_dir / "arc-recovery-final.lock.json.sha256",
        f"{manifest_sha256}  arc-recovery-final.lock.json\n".encode("ascii"),
        0o444,
    )
    write_exact(
        args.handoff_dir / "legacy-maintenance-boundary.json", boundary_payload, 0o444
    )
    write_exact(args.handoff_dir / "recovery.arcchkpt", checkpoint, 0o444)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
