#!/usr/bin/env python3
"""Create or read a public, exact-parent operational input commit.

This utility never pushes a ref, changes a checkout, or declares readiness.
The protected producer independently verifies every operational input.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess

COMMIT = re.compile(r"[0-9a-f]{40}\Z")
FILES = {
    "approved-genesis.network-hash": 65,
    "host-config.json": 128 * 1024,
    "quorum-proof.json": 1024 * 1024,
    "recovery.arcchkpt": 64 * 1024 * 1024,
    "update-attestation.json": 2 * 1024 * 1024,
}


class InputError(RuntimeError):
    pass


def require(condition, message):
    if not condition:
        raise InputError(message)


def git(root: Path, *arguments: str, data: bytes | None = None) -> bytes:
    env = dict(os.environ)
    # Do not repurpose HOME or the user's index/configuration.
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
               GIT_TERMINAL_PROMPT="0", GIT_NO_REPLACE_OBJECTS="1")
    for name in ("GIT_INDEX_FILE", "GIT_DIR", "GIT_WORK_TREE", "GIT_OBJECT_DIRECTORY",
                 "GIT_ALTERNATE_OBJECT_DIRECTORIES"):
        env.pop(name, None)
    result = subprocess.run(["git", "-C", str(root), *arguments], input=data,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            env=env, timeout=120, check=False)
    require(result.returncode == 0, "git object operation failed: " +
            result.stderr.decode("utf-8", errors="replace")[-1000:])
    return result.stdout


def validate_bytes(name: str, payload: bytes):
    require(name in FILES and 0 < len(payload) <= FILES[name],
            "input is empty, oversized, or outside the exact public contract: " + name)
    if name == "approved-genesis.network-hash":
        require(re.fullmatch(rb"[0-9a-f]{64}\n?", payload) is not None,
                "approved genesis hash is malformed")
    if name.endswith(".json"):
        try:
            value = json.loads(payload)
        except (UnicodeError, ValueError) as error:
            raise InputError("invalid input JSON: " + name) from error
        require(isinstance(value, dict), "input JSON must be an object: " + name)
        reject_secret_fields(value)
        validate_public_fields(name, value)


def reject_secret_fields(value, depth=0):
    require(depth <= 24, "operational JSON nesting exceeds the bounded contract")
    if isinstance(value, dict):
        for key, item in value.items():
            normalized = re.sub(r"[^a-z]", "", key.lower())
            require(not any(word in normalized for word in (
                    "privatekey", "seedphrase", "mnemonic", "apikey", "apitoken",
                    "accesstoken", "bearertoken", "refreshtoken", "password", "secret")),
                    "credential field is not an operational public input")
            reject_secret_fields(item, depth + 1)
    elif isinstance(value, list):
        for item in value:
            reject_secret_fields(item, depth + 1)
    elif isinstance(value, str):
        require(re.search(r"-----BEGIN [^-]*PRIVATE KEY-----", value) is None,
                "private key material is not an operational public input")


def validate_public_fields(name, value):
    """Reject extra operational fields before creating any public Git objects.

    Semantic completeness is independently checked by the protected producer.
    This is a closed field contract, not a general detector for encoded secrets.
    """
    def fields(row, allowed):
        require(isinstance(row, dict) and set(row) <= set(allowed.split()),
                "unexpected public operational field in " + name)

    if name == "host-config.json":
        fields(value, "schema source_reference total_stake quorum_stake quorum_sites unobserved_stake "
               "genesis_file_sha256 genesis_network_hash recovery_domain checkpoint_manifest_hash "
               "validator_set_id lax_access hosts")
        if "source_reference" in value:
            fields(value["source_reference"], "source_sha workflow_run_id recovery_proof recovery_proof_sha256 "
                   "full_state_copy_qualification full_state_copy_qualification_sha256 scope")
        for host in value.get("hosts", []):
            fields(host, "site ip hostname validator stake unit data_dir genesis_path genesis_sha256 "
                   "baseline_binary_sha256 legacy_v3_wire")
    elif name == "quorum-proof.json":
        fields(value, "schema scope transport captured_at_unix checkpoint_manifest_hash common_block "
               "genesis_file_sha256 genesis_network_hash quorum_stake recovery_domain samples total_stake validator_set_id")
        block = value.get("common_block", {})
        fields(block, "hash height hosts protocol_major state_root")
        for host in block.get("hosts", []):
            fields(host, "hash height protocol_major site state_root")
        for sample in value.get("samples", []):
            fields(sample, "captured_at_unix nodes")
            for node in sample.get("nodes", []):
                fields(node, "active active_stake argv_redacted binary_sha256 checkpoint_manifest_hash "
                       "genesis_sha256 health height hostname invocation_id ip last_block_height network_genesis_hash "
                       "network_total_stake pid recovery_domain restarts site stake validator validator_set_id")
                fields(node.get("health", {}), "chain_advancing dag_committed dag_round height last_block_age_secs "
                       "peers status uptime_secs validators version binary_sha256 dag_bootstrapping "
                       "extended_consensus_wire_enabled features legacy_v3_wire wire_messages_suppressed "
                       "chain_participation_enabled")


def validate_bindings(payloads: dict[str, bytes], main: str):
    attestation = json.loads(payloads["update-attestation.json"])
    release = attestation.get("release", {})
    require(attestation.get("schema") == "arc.existing-recovered-chain-update/v1" and
            release.get("repository") == "FerrumVir/arc-chain" and
            release.get("commit") == main and release.get("main_sha") == main,
            "owner attestation does not bind exact protected main")
    require(attestation.get("claim") == {
        "existing_recovered_chain_update": True,
        "original_cutover_ceremony_verified": False,
        "original_cutover_archive_present": False,
        "validator_retirement_authorized": False,
    }, "update input cannot assert an original ceremony or retirement")
    checkpoint = attestation.get("checkpoint", {})
    require(checkpoint.get("checkpoint_sha256") == hashlib.sha256(payloads["recovery.arcchkpt"]).hexdigest()
            and checkpoint.get("checkpoint_size") == len(payloads["recovery.arcchkpt"]),
            "raw checkpoint differs from owner attestation")
    require(attestation.get("fresh_six_host_proof", {}).get("proof_sha256") ==
            hashlib.sha256(payloads["quorum-proof.json"]).hexdigest(),
            "quorum proof differs from owner attestation")
    require(attestation.get("network", {}).get("host_config_sha256") == hashlib.sha256(payloads["host-config.json"]).hexdigest(),
            "host configuration differs from owner attestation")
    require(attestation.get("network", {}).get("approved_genesis_network_hash_file_sha256") ==
            hashlib.sha256(payloads["approved-genesis.network-hash"]).hexdigest(),
            "approved genesis hash file differs from owner attestation")


def create(root: Path, source: Path, main: str) -> str:
    require(COMMIT.fullmatch(main) is not None, "main must be an exact Git commit")
    require(git(root, "rev-parse", "HEAD").decode().strip() == main,
            "creator checkout must be the exact main candidate")
    require(source.is_dir() and not source.is_symlink(), "input directory must be real")
    require({p.name for p in source.iterdir()} == set(FILES), "input directory membership differs")
    payloads = {}
    for name in FILES:
        path = source / name
        metadata = path.lstat()
        require(stat.S_ISREG(metadata.st_mode) and not path.is_symlink() and
                0 < metadata.st_size <= FILES[name], "unsafe input file: " + name)
        payloads[name] = path.read_bytes()
        validate_bytes(name, payloads[name])
    validate_bindings(payloads, main)
    entries = []
    for name, payload in payloads.items():
        object_id = git(root, "hash-object", "-w", "--stdin", data=payload).decode().strip()
        entries.append(f"100644 blob {object_id}\t{name}\n")
    tree = git(root, "mktree", data="".join(entries).encode()).decode().strip()
    return git(root, "-c", "user.name=ARC Update Handoff", "-c",
               "user.email=release-handoff@arc.compute", "-c", "commit.gpgsign=false",
               "commit-tree", tree, "-p", main,
               data=b"Public operational inputs for existing recovered-chain update\n").decode().strip()


def read_commit(root: Path, commit: str, main: str) -> dict[str, bytes]:
    require(COMMIT.fullmatch(commit) is not None and COMMIT.fullmatch(main) is not None,
            "handoff and main must be exact Git commits")
    require(int(git(root, "cat-file", "-s", commit).strip()) <= 16 * 1024,
            "input commit object exceeds the bounded contract")
    parents = git(root, "rev-list", "--parents", "-n", "1", commit).decode().strip()
    require(parents == f"{commit} {main}", "input commit must have exact main as its sole parent")
    require(int(git(root, "cat-file", "-s", f"{commit}^{{tree}}").strip()) <= 2048,
            "input root tree exceeds the bounded contract")
    # Only flat root blobs are allowed. Never traverse an attacker-supplied
    # subtree before enforcing its mode and exact membership.
    rows = git(root, "ls-tree", "-z", "--full-tree", commit).split(b"\0")
    payloads = {}
    for row in filter(None, rows):
        metadata, name = row.split(b"\t", 1)
        mode, kind, object_id = metadata.decode("ascii").split()
        name = name.decode("utf-8")
        require(name in FILES and name not in payloads and mode == "100644" and kind == "blob",
                "input commit contains an unexpected path, mode, or object")
        size = int(git(root, "cat-file", "-s", object_id).strip())
        require(0 < size <= FILES[name], "input blob is oversized or empty: " + name)
        payloads[name] = git(root, "cat-file", "blob", object_id)
        validate_bytes(name, payloads[name])
    require(set(payloads) == set(FILES), "input commit has incomplete membership")
    validate_bindings(payloads, main)
    return payloads


def materialize(root: Path, commit: str, main: str, output: Path):
    payloads = read_commit(root, commit, main)
    require(not output.exists() and not output.is_symlink(), "output must be absent")
    output.mkdir(mode=0o700)
    for name, payload in payloads.items():
        fd = os.open(output / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o444)
        with os.fdopen(fd, "wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
    fd = os.open(output, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("create", "materialize"))
    parser.add_argument("--repository-root", type=Path, required=True)
    parser.add_argument("--main-commit", required=True)
    parser.add_argument("--input-dir", type=Path)
    parser.add_argument("--handoff-commit")
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()
    try:
        if args.operation == "create":
            require(args.input_dir is not None, "create requires --input-dir")
            commit = create(args.repository_root, args.input_dir, args.main_commit)
            print(json.dumps({"commit": commit, "parent": args.main_commit,
                              "pushed": False, "readiness_verified": False}))
        else:
            require(args.handoff_commit is not None and args.output_dir is not None,
                    "materialize requires --handoff-commit and --output-dir")
            materialize(args.repository_root, args.handoff_commit, args.main_commit, args.output_dir)
            print(json.dumps({"materialized": True, "readiness_verified": False}))
        return 0
    except (InputError, OSError, ValueError, subprocess.TimeoutExpired) as error:
        print(json.dumps({"pass": False, "error": str(error)}))
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
