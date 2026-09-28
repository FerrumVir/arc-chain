#!/usr/bin/env python3
"""Emit a bounded readiness attestation for an existing recovered ARC chain.

The profile does not assert the historic cutover ceremony or authorize retirement.
Artifacts are re-materialized from their selected raw Actions ZIPs; checkpoint
signatures are checked only by the selected release arc-node's recovery verifier.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

SCHEMA = "arc.existing-recovered-chain-update/v1"
SHA = re.compile(r"[0-9a-f]{64}\Z")
COMMIT = re.compile(r"[0-9a-f]{40}\Z")
TAG = re.compile(r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\Z")
PLATFORMS = ("linux-x86_64", "linux-arm64", "macos-arm64", "macos-x86_64", "windows-x86_64")


class ProfileError(RuntimeError):
    pass


def need(ok: bool, message: str) -> None:
    if not ok:
        raise ProfileError(message)


def sha_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def regular(path: Path) -> bool:
    return path.is_file() and not path.is_symlink()


def read_json(path: Path) -> dict:
    need(regular(path), f"expected regular JSON file: {path}")
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as e:
        raise ProfileError(f"invalid JSON {path}: {e}") from e
    need(isinstance(value, dict), f"expected JSON object: {path}")
    return value


def load_module(path: Path, name: str):
    need(regular(path), f"required source script is missing or symlinked: {path}")
    spec = importlib.util.spec_from_file_location(name, path)
    need(spec is not None and spec.loader is not None, f"cannot load script: {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def strict_proof(proof_path: Path, config_path: Path, validator_path: Path,
                 expected_binary_sha256: str, now: float | None = None) -> tuple[dict, dict]:
    """Delegate all quorum semantics to the deployed, tested rollout validator."""
    proof, config = read_json(proof_path), read_json(config_path)
    validator = load_module(validator_path, "arc_rollout_current_profile_validator")
    hosts, total, quorum = validator.validate_host_config(config)
    expected = {key: config[key] for key in ("genesis_file_sha256", "genesis_network_hash",
        "recovery_domain", "checkpoint_manifest_hash", "validator_set_id")}
    summary = validator.validate_quorum_proof(proof, hosts, total, quorum,
        expected_network=expected, **({"now": now} if now is not None else {}))
    need(len(hosts) == 6 and len(proof.get("common_block", {}).get("hosts", [])) == 6,
         "update readiness requires the complete six-host quorum")
    for sample in proof["samples"]:
        for row in sample["nodes"]:
            need(row["binary_sha256"] == expected_binary_sha256,
                 f"host {row['site']} is not running the selected candidate binary")
    return {"schema": proof["schema"], "proof_sha256": sha_file(proof_path), **summary}, config


def validate_selection(selection: dict, commit: str, run_id: int, attempt: int) -> dict:
    need(selection.get("schema") == "arc.pretag.selection.v1" and
         selection.get("repository") == "FerrumVir/arc-chain" and selection.get("commit") == commit and
         selection.get("run_id") == run_id and selection.get("run_attempt") == attempt,
         "selected pre-tag API tuple does not match repository/commit/run/attempt")
    groups = selection.get("artifacts")
    need(isinstance(groups, dict), "selection artifact map is missing")
    return groups


def revalidate_api_selection(selection_path: Path, api_path: Path, selector: Path,
                             repository: str, commit: str, run_id: int, attempt: int) -> tuple[dict, dict]:
    need(regular(selector), "pre-tag API selector is missing or symlinked")
    selection = read_json(selection_path)
    groups = validate_selection(selection, commit, run_id, attempt)
    selected = json.dumps(groups, sort_keys=True, separators=(",", ":"))
    result = subprocess.run([sys.executable, str(selector), "--api-json", str(api_path),
        "--repository", repository, "--commit", commit, "--run-id", str(run_id),
        "--run-attempt", str(attempt), "--expected-artifacts-json", selected],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    stderr = result.stderr.decode("utf-8", errors="replace") if isinstance(result.stderr, bytes) else str(result.stderr)
    need(result.returncode == 0, "saved API artifact response does not reproduce exact selection: " + stderr[-1000:])
    try:
        selected_api = json.loads(result.stdout)
    except (UnicodeError, json.JSONDecodeError) as e:
        raise ProfileError(f"selector output is invalid: {e}") from e
    need(selected_api == selection, "API-revalidated selection differs from supplied selection")
    return selection, groups


def select_and_materialize(args: argparse.Namespace, output: Path) -> tuple[dict, dict, dict]:
    need(args.artifact_platform in PLATFORMS and args.verifier_platform in PLATFORMS,
         "unsupported production or verifier platform")
    need(args.tag[1:] == args.version, "tag version and pre-tag artifact version differ")
    selection, groups = revalidate_api_selection(args.selection, args.api_artifacts, args.selector,
        args.repository, args.commit, args.run_id, args.run_attempt)

    need(args.downloads_root.is_dir() and not args.downloads_root.is_symlink(),
         "downloads root must be a real directory")
    need(regular(args.materializer), "pre-tag materializer is missing")
    materializer_sha = sha_file(args.materializer)
    command = [sys.executable, str(args.materializer), "--downloads-root", str(args.downloads_root),
            "--output-dir", str(output), "--repository", args.repository, "--commit", args.commit,
            "--run-id", str(args.run_id), "--run-attempt", str(args.run_attempt), "--version", args.version,
            "--selection-json", json.dumps(groups, sort_keys=True, separators=(",", ":")),
            "--retain-build-metadata"]
    for platform in dict.fromkeys((args.artifact_platform, args.verifier_platform)):
        command.extend(("--only", f"headless:{platform}"))
    materialized = subprocess.run(command, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    need(materialized.returncode == 0, "protected pre-tag materializer rejected selected ZIP/payload: " +
             materialized.stderr.decode("utf-8", errors="replace")[-1500:])
    groups_out = {}
    for platform in dict.fromkeys((args.artifact_platform, args.verifier_platform)):
        directory = output / f"headless-{platform}"
        receipt, metadata = read_json(directory / "MATERIALIZATION-RECEIPT.json"), read_json(directory / "BUILD-METADATA.json")
        need(receipt.get("schema") == "arc.pretag.materialization.v1" and
                 receipt.get("artifact") == groups[platform]["headless"] and
                 receipt.get("repository") == args.repository and receipt.get("commit") == args.commit and
                 receipt.get("run_id") == args.run_id and receipt.get("run_attempt") == args.run_attempt and
                 metadata.get("schema") == "arc.pretag.artifact.v1" and metadata.get("commit") == args.commit and
                 metadata.get("platform") == platform and metadata.get("workflow_run_id") == args.run_id and
                 metadata.get("workflow_run_attempt") == args.run_attempt,
                 f"materialized metadata/receipt cross-binding failed: {platform}")
        binary_name = f"arc-node-{platform}"
        need(regular(directory / binary_name) and sha_file(directory / binary_name) == receipt["files"][binary_name],
                 f"materialized candidate binary hash mismatch: {platform}")
        need(regular(directory / "genesis.toml") and
                 sha_file(directory / "genesis.toml") == receipt["files"]["genesis.toml"],
                 f"materialized genesis hash mismatch: {platform}")
        groups_out[platform] = {"binary_path": directory / binary_name,
                "binary_sha256": receipt["files"][binary_name], "genesis_path": directory / "genesis.toml",
                "genesis_sha256": receipt["files"]["genesis.toml"],
                "artifact_id": receipt["artifact"]["id"], "artifact_digest": receipt["artifact"]["digest"],
                "archive_sha256": receipt["artifact"]["archive_sha256"],
                "build_metadata_sha256": receipt["build_metadata_sha256"],
                "materialization_receipt_sha256": sha_file(directory / "MATERIALIZATION-RECEIPT.json")}
    return selection, groups_out, {"sha256": materializer_sha}


def norm_hash(value: object, label: str) -> str:
    text = str(value)
    if text.startswith("0x"):
        text = text[2:]
    need(SHA.fullmatch(text) is not None, f"malformed CLI hash: {label}")
    return text


def verify_checkpoint(binary: Path, checkpoint: Path, genesis: Path, manifest_hash: str,
                      epoch: int, set_id: int, expected_domain: str) -> dict:
    # The verifier runs with cwd="/". Normalize lexically before that chdir;
    # abspath preserves a symlink at the final component for `regular()` to reject.
    binary = Path(os.path.abspath(binary))
    checkpoint = Path(os.path.abspath(checkpoint))
    genesis = Path(os.path.abspath(genesis))
    need(regular(binary) and binary.stat().st_mode & 0o111, "selected verifier binary is not executable")
    need(regular(checkpoint) and regular(genesis), "checkpoint and genesis must be regular files")
    before = (checkpoint.stat().st_dev, checkpoint.stat().st_ino, checkpoint.stat().st_size,
              checkpoint.stat().st_mtime_ns, sha_file(checkpoint))
    env = dict(os.environ)
    env.update({"PATH": "/usr/bin:/bin", "LANG": "C", "LC_ALL": "C", "TZ": "UTC", "RUST_BACKTRACE": "0"})
    commands = ([str(binary), "recovery", "inspect", "--checkpoint", str(checkpoint)],
        [str(binary), "recovery", "verify", "--checkpoint", str(checkpoint), "--genesis", str(genesis),
         "--approved-manifest-hash", manifest_hash, "--recovery-epoch", str(epoch), "--validator-set-id", str(set_id)])
    outputs = []
    for command in commands:
        try:
            proc = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                  cwd="/", env=env, timeout=300, check=False)
        except (OSError, subprocess.TimeoutExpired) as e:
            raise ProfileError(f"arc-node recovery verifier failed: {e}") from e
        stderr = proc.stderr.decode("utf-8", errors="replace") if isinstance(proc.stderr, bytes) else str(proc.stderr)
        need(proc.returncode == 0, "arc-node recovery inspect/verify rejected checkpoint: " + stderr[-1000:])
        try:
            value = json.loads(proc.stdout)
        except (UnicodeError, json.JSONDecodeError) as e:
            raise ProfileError(f"recovery verifier output is not JSON: {e}") from e
        need(isinstance(value, dict), "recovery verifier output must be one JSON object")
        outputs.append(value)
    after = (checkpoint.stat().st_dev, checkpoint.stat().st_ino, checkpoint.stat().st_size,
             checkpoint.stat().st_mtime_ns, sha_file(checkpoint))
    need(before == after, "checkpoint changed while verifier read it")
    inspected, verified = outputs
    hash_fields = ("manifest_hash", "payload_hash", "genesis_hash", "full_state_root", "source_block_hash",
        "source_state_root", "transition_block_hash", "recovery_domain", "signing_hash")
    for field in hash_fields:
        need(norm_hash(inspected.get(field), f"inspect.{field}") == norm_hash(verified.get(field), f"verify.{field}"),
             f"inspect/verify hash projection differs: {field}")
    fields = ("format_version", "chain_id", "source_height", "source_consensus_round", "created_at_unix_ms",
        "transition_height", "recovery_epoch", "validator_set_id", "protocol_version", "validator_count",
        "community_rewards_v1_activation_height", "signature_count", "validators", "signatures")
    for field in fields:
        need(inspected.get(field) == verified.get(field), f"inspect/verify projection differs: {field}")
    need(verified.get("status") == "VERIFIED_QUORUM" and verified.get("recovery_epoch") == epoch and
         verified.get("validator_set_id") == set_id and norm_hash(verified.get("manifest_hash"), "manifest_hash") == manifest_hash and
         norm_hash(verified.get("recovery_domain"), "recovery_domain") == expected_domain and
         verified.get("validator_count") == 6 and type(verified.get("signature_count")) is int and
         5 <= verified["signature_count"] <= 6 and isinstance(verified.get("signatures"), list) and
         len(verified["signatures"]) == verified["signature_count"] and
         isinstance(verified.get("validators"), list) and len(verified["validators"]) == 6 and
         type(verified.get("source_height")) is int and
         verified.get("transition_height") == verified["source_height"] + 1,
         "existing CLI did not verify the pinned 5-of-6 checkpoint")
    return {"checkpoint_sha256": before[4], "checkpoint_size": before[2],
        **{k: norm_hash(verified[k], k) for k in hash_fields},
        "source_height": verified["source_height"], "transition_height": verified["transition_height"],
        "source_consensus_round": verified["source_consensus_round"],
        "created_at_unix_ms": verified["created_at_unix_ms"], "chain_id": verified["chain_id"],
        "protocol_version": verified["protocol_version"], "recovery_epoch": epoch,
        "validator_set_id": set_id, "signature_count": verified["signature_count"],
        "validators": verified["validators"], "signatures": verified["signatures"],
        "verification_status": "VERIFIED_QUORUM"}


def validate_certificate_authority(checkpoint: dict, hosts: dict[str, dict], config: dict,
                                  genesis_file_sha256: str) -> None:
    need(genesis_file_sha256 == config["genesis_file_sha256"], "selected release genesis differs from host network pin")
    need(checkpoint["recovery_domain"] == config["recovery_domain"] and
         checkpoint["protocol_version"] == "3.0.0" and checkpoint["validator_set_id"] == config["validator_set_id"],
         "checkpoint domain/protocol/set differs from live network profile")
    cert = checkpoint["validators"]
    need(isinstance(cert, list) and len(cert) == 6, "verified certificate must describe six validators")
    certificate_authority = {}
    for row in cert:
        need(isinstance(row, dict), "malformed verifier validator row")
        address = norm_hash(row.get("address"), "validator.address")
        need(address not in certificate_authority and type(row.get("stake")) is int and row["stake"] > 0,
             "duplicate or invalid certificate authority")
        certificate_authority[address] = row["stake"]
    live_authority = {row["validator"]: row["stake"] for row in hosts.values()}
    need(certificate_authority == live_authority, "verified checkpoint certificate authority differs from six live validator pins")


def validate_crossbindings(checkpoint: dict, hosts: dict[str, dict], config: dict,
                           genesis_file_sha256: str, artifact_binary_sha256: str,
                           proof: dict, approved_genesis_network_hash: str) -> None:
    validate_certificate_authority(checkpoint, hosts, config, genesis_file_sha256)
    need(checkpoint["genesis_hash"] == approved_genesis_network_hash,
         "verified checkpoint genesis hash differs from approved genesis.network-hash")
    need(all(h["baseline_binary_sha256"] == artifact_binary_sha256 for h in hosts.values()),
         "host config is not pinned to the selected production candidate binary")
    for sample in proof["samples"]:
        need(all(row["binary_sha256"] == artifact_binary_sha256 for row in sample["nodes"]),
             "not all six hosts run the selected production candidate")


def public_artifact(platform: str, value: dict) -> dict:
    return {"platform": platform, **{key: item for key, item in value.items()
        if key not in {"binary_path", "genesis_path"}}}


def verify_owner_verifier(args: argparse.Namespace, owner_verifier: dict) -> None:
    """Re-materialize even a foreign-platform verifier; it need not be executed."""
    owner_args = argparse.Namespace(**vars(args))
    owner_args.verifier_platform = owner_verifier["platform"]
    with tempfile.TemporaryDirectory(prefix="arc-owner-verifier-") as td:
        _, artifacts, _ = select_and_materialize(owner_args, Path(td) / "materialized")
        need(owner_verifier == public_artifact(owner_args.verifier_platform,
                                               artifacts[owner_args.verifier_platform]),
             "owner verifier bytes or metadata differ from independently materialized artifact")


def validate_main_pin(repository_root: Path, commit: str, main_sha: str) -> None:
    need(COMMIT.fullmatch(commit) and main_sha == commit, "tag commit must equal the protected-main commit")
    head = subprocess.run(["git", "-C", str(repository_root), "rev-parse", "HEAD"],
                          capture_output=True, text=True, check=False)
    need(head.returncode == 0 and head.stdout.strip() == main_sha,
         "helper checkout must be the exact protected-main commit")


def assemble(args: argparse.Namespace) -> dict:
    need(args.repository == "FerrumVir/arc-chain" and TAG.fullmatch(args.tag) and
         COMMIT.fullmatch(args.commit) and args.main_sha == args.commit, "invalid repository/tag/main binding")
    need(args.run_id > 0 and args.run_attempt > 0 and args.recovery_epoch == 1,
         "invalid preflight run or recovery epoch")
    validate_main_pin(args.repository_root, args.commit, args.main_sha)

    with tempfile.TemporaryDirectory(prefix="arc-existing-chain-profile-") as td:
        materialized_root = Path(td) / "materialized"
        selection, artifacts, materializer = select_and_materialize(args, materialized_root)
        selected = artifacts[args.artifact_platform]
        verifier = artifacts[args.verifier_platform]
        need(selected["genesis_sha256"] == verifier["genesis_sha256"],
             "production and verifier platform artifacts carry different genesis bytes")
        config = read_json(args.host_config)
        checkpoint = verify_checkpoint(verifier["binary_path"], args.checkpoint, verifier["genesis_path"],
            config["checkpoint_manifest_hash"], args.recovery_epoch, config["validator_set_id"], config["recovery_domain"])
        try:
            approved_genesis_hash = args.approved_genesis_network_hash.read_text(encoding="ascii").strip()
        except (OSError, UnicodeError) as e:
            raise ProfileError(f"cannot read approved genesis.network-hash: {e}") from e
        need(SHA.fullmatch(approved_genesis_hash) is not None,
             "approved genesis.network-hash must be one bare SHA-256")
        proof = read_json(args.quorum_proof)
        # CLI genesis_hash is computed from the signed genesis; RPC's legacy
        # genesis_network_hash is a distinct pin and is retained separately.
        validate_crossbindings(checkpoint, {h["site"]: h for h in config["hosts"]}, config,
                               selected["genesis_sha256"], selected["binary_sha256"], proof,
                               approved_genesis_hash)
        # Run the strict freshness/liveness validator last: full checkpoint
        # verification is allowed to take several minutes.
        proof_now = getattr(args, "proof_validation_time", None)
        proof_summary, checked_config = strict_proof(args.quorum_proof, args.host_config,
            args.rollout_validator, selected["binary_sha256"], now=proof_now)
        need(checked_config == config, "host config changed during qualification")
    proof_summary["rpc_genesis_network_hash"] = config["genesis_network_hash"]
    return {"schema": SCHEMA, "claim": {"existing_recovered_chain_update": True,
        "original_cutover_ceremony_verified": False, "original_cutover_archive_present": False,
        "validator_retirement_authorized": False},
        "release": {"repository": args.repository, "tag": args.tag, "commit": args.commit,
            "main_sha": args.main_sha, "preflight_run_id": args.run_id, "preflight_run_attempt": args.run_attempt,
            "selection_sha256": sha_file(args.selection), "api_artifact_response_sha256": sha_file(args.api_artifacts),
            "selector_sha256": sha_file(args.selector), "materializer_sha256": materializer["sha256"],
            "production_artifact": public_artifact(args.artifact_platform, selected),
            "checkpoint_verifier_artifact": public_artifact(args.verifier_platform, verifier)},
        "network": {"genesis_file_sha256": config["genesis_file_sha256"],
            "host_config_sha256": sha_file(args.host_config),
            "rpc_genesis_network_hash": config["genesis_network_hash"],
            "checkpoint_signed_genesis_hash": checkpoint["genesis_hash"],
            "approved_genesis_network_hash_file_sha256": sha_file(args.approved_genesis_network_hash),
            "recovery_domain": config["recovery_domain"], "checkpoint_manifest_hash": config["checkpoint_manifest_hash"],
            "validator_set_id": config["validator_set_id"], "total_stake": config["total_stake"],
            "quorum_stake": config["quorum_stake"]},
        "checkpoint": checkpoint, "fresh_six_host_proof": proof_summary,
        "rollout_validator_sha256": sha_file(args.rollout_validator),
        "generated_at_unix": getattr(args, "attestation_generated_at", None) or time.time()}


def validate_owner_attestation(args: argparse.Namespace, owner_path: Path, now: float | None = None) -> dict:
    """Recompute the owner's public evidence and preserve its real observation time."""
    now = time.time() if now is None else now
    owner = read_json(owner_path)
    generated_at = owner.get("generated_at_unix")
    need(type(generated_at) in (int, float) and 0 < generated_at <= now,
         "owner attestation timestamp is invalid or in the future")
    need(now - generated_at <= 7200,
         "owner attestation is older than the two-hour operational evidence bound")
    expected_validator = Path(__file__).with_name("update-quorum-validator.py").resolve()
    need(args.rollout_validator.resolve() == expected_validator,
         "producer re-verification must use the tracked quorum validator")
    args.proof_validation_time = float(generated_at)
    args.attestation_generated_at = float(generated_at)
    recomputed = assemble(args)
    owner_release = owner.get("release", {})
    producer_release = recomputed.get("release", {})
    owner_verifier = owner_release.get("checkpoint_verifier_artifact")
    producer_verifier = producer_release.get("checkpoint_verifier_artifact")
    need(isinstance(owner_verifier, dict) and isinstance(producer_verifier, dict) and
         owner_verifier.get("platform") in PLATFORMS and producer_verifier.get("platform") in PLATFORMS,
         "owner and producer must identify selected checkpoint verifier artifacts")
    selection = read_json(args.selection)
    groups = validate_selection(selection, args.commit, args.run_id, args.run_attempt)
    selected_owner_verifier = groups.get(owner_verifier["platform"], {}).get("headless", {})
    need(owner_verifier.get("artifact_id") == selected_owner_verifier.get("id") and
         owner_verifier.get("artifact_digest") == selected_owner_verifier.get("digest"),
         "owner checkpoint verifier artifact is not selected by the exact preflight tuple")
    verify_owner_verifier(args, owner_verifier)
    owner_projection = json.loads(json.dumps(owner))
    producer_projection = json.loads(json.dumps(recomputed))
    owner_projection["release"].pop("checkpoint_verifier_artifact", None)
    producer_projection["release"].pop("checkpoint_verifier_artifact", None)
    owner_api_sha256 = owner_projection["release"].pop("api_artifact_response_sha256", None)
    producer_api_sha256 = producer_projection["release"].pop("api_artifact_response_sha256", None)
    need(SHA.fullmatch(str(owner_api_sha256 or "")) is not None and
         SHA.fullmatch(str(producer_api_sha256 or "")) is not None,
         "owner and producer API response hashes are malformed")
    need(owner_projection == producer_projection,
         "owner attestation differs from independently reverified production artifact, checkpoint, network, or proof inputs")
    result = dict(owner)
    result["producer_reverification"] = {
        "freshness_at_capture_unix": owner.get("fresh_six_host_proof", {}).get("captured_at_unix"),
        "observed_at_unix": float(generated_at),
        "independently_reverified_at_unix": now,
        "owner_attestation_age_seconds": int(now - generated_at),
        "maximum_owner_attestation_age_seconds": 7200,
        "owner_api_artifact_response_sha256": owner_api_sha256,
        "independently_reverified_api_artifact_response_sha256": producer_api_sha256,
        "owner_checkpoint_verifier_platform": owner_verifier["platform"],
        "producer_checkpoint_verifier_platform": producer_verifier["platform"],
        "proof_scope": "owner-attested six-host advancing proof at owner_observed_at_unix; independently reverified against pinned inputs, not a live publication-time assertion",
    }
    return result


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--repository", required=True); p.add_argument("--repository-root", type=Path, required=True)
    p.add_argument("--tag", required=True); p.add_argument("--commit", required=True); p.add_argument("--main-sha", required=True)
    p.add_argument("--run-id", type=int, required=True); p.add_argument("--run-attempt", type=int, required=True)
    p.add_argument("--version", required=True); p.add_argument("--selection", type=Path, required=True)
    p.add_argument("--api-artifacts", type=Path, required=True); p.add_argument("--downloads-root", type=Path, required=True)
    p.add_argument("--selector", type=Path, required=True); p.add_argument("--materializer", type=Path, required=True)
    p.add_argument("--host-config", type=Path, required=True); p.add_argument("--quorum-proof", type=Path, required=True)
    p.add_argument("--rollout-validator", type=Path,
                   default=Path(__file__).with_name("update-quorum-validator.py"))
    p.add_argument("--artifact-platform", choices=PLATFORMS, required=True,
                   help="deployed candidate platform; all six hosts must currently run these exact bytes")
    p.add_argument("--verifier-platform", choices=PLATFORMS, required=True,
                   help="selected platform executable on this qualification host")
    p.add_argument("--checkpoint", type=Path, required=True); p.add_argument("--recovery-epoch", type=int, required=True)
    p.add_argument("--approved-genesis-network-hash", type=Path, required=True,
                   help="approved checkpoint-source genesis.network-hash file; distinct from RPC genesis pin")
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--owner-attestation", type=Path,
                   help="reverify an immutable owner attestation for protected production packaging")
    args = p.parse_args()
    try:
        need(not args.output.exists() and not args.output.is_symlink(), "refusing to overwrite attestation")
        value = (validate_owner_attestation(args, args.owner_attestation)
                 if args.owner_attestation else assemble(args))
        args.output.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(value, f, sort_keys=True, indent=2); f.write("\n"); f.flush(); os.fsync(f.fileno())
        dfd = os.open(args.output.parent, os.O_RDONLY | os.O_DIRECTORY)
        try: os.fsync(dfd)
        finally: os.close(dfd)
        print(json.dumps({"pass": True, "output": str(args.output), "schema": SCHEMA,
            "checkpoint_sha256": value["checkpoint"]["checkpoint_sha256"],
            "common_height": value["fresh_six_host_proof"]["common_height"]}, sort_keys=True))
        return 0
    except (ProfileError, OSError, ValueError, KeyError) as e:
        print(json.dumps({"pass": False, "halt": True, "error": f"{type(e).__name__}: {e}"}), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
