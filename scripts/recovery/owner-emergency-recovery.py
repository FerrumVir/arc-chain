#!/usr/bin/env python3
"""Verify one GitHub-owner-authenticated ARC emergency-recovery approval.

This helper deliberately handles public recovery identities and hashes only. It
never opens a validator key, seed, checkpoint payload, OAuth configuration, or
other secret-bearing input. The repository owner dispatches a no-secret
protected-main workflow; this helper authenticates that exact successful run
attempt and its GitHub artifact, then materializes a create-only mode-0400
receipt immediately before the offline signer is invoked.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import io
import json
import os
import re
import stat
import sys
import zipfile
from pathlib import Path
from typing import Any, NoReturn, Sequence


RECEIPT_SCHEMA = "arc.recovery.owner-emergency-recovery.v2"
REPOSITORY = "FerrumVir/arc-chain"
PROTECTED_BRANCH = "main"
WORKFLOW_PATH = ".github/workflows/owner-emergency-recovery-approval.yml"
ARTIFACT_MEMBER = "OWNER-EMERGENCY-RECOVERY.json"
OWNER_LOGIN = "FerrumVir"
OWNER_USER_ID = 111036403
AUTHORIZATION_KIND = "owner_emergency_recovery"
AUTHORITY_BASIS = (
    "repository-owner emergency authorization authenticated by an exact GitHub "
    "Actions run; not approval by six independent humans"
)
REASON_CODE = "legacy_fleet_divergence_history_preserving_v080_cutover"
REASON = (
    "The divergent public validator fleet requires the reviewed, "
    "history-preserving ARC v0.8 emergency cutover."
)
RISK_ACKNOWLEDGEMENT = (
    "I authorize five reviewed ARC validator identities to sign the recovery "
    "checkpoint rooted at preserved source block 137145 and transition block "
    "137146. I understand that the six legacy forks remain preserved read-only, "
    "that rollback cannot rewrite signed history, and that this receipt does not "
    "represent approval by six independent humans."
)
SOURCE_HEIGHT = 137_145
TRANSITION_HEIGHT = 137_146
RECOVERY_EPOCH = 1
VALIDATOR_SET_ID = 1
SIGNATURES_REQUIRED = 5
TOTAL_STAKE = 40_000_000
MINIMUM_SIGNED_STAKE = TOTAL_STAKE * 2 // 3 + 1
MAX_JSON_BYTES = 256 * 1024
DEFAULT_MAX_AGE_SECONDS = 900
FUTURE_TOLERANCE_SECONDS = 30
HASH_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
UTC_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")
SAFE_PATH_COMPONENT_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")


# The order is the checked-in recovered genesis order and the physical signing
# order.  SGP remains the unused recovery member for this five-signature pass.
PRODUCTION_VALIDATORS: tuple[tuple[str, str, int], ...] = (
    ("NYC", "adf4ff16f997c871c16f3897e67881311d08f975f28ebdcf79e86ea9e3b99d0f", 6_666_667),
    ("LAX", "44d20543df6e76696da2ebbbd79e4243cd41729fa5b890e2618991e489314780", 6_666_667),
    ("AMS", "5772741c93d8a4b04ec39007cb568a31e13ffba0d3e786596d1900d30e529f21", 6_666_667),
    ("LHR", "228787281308d6c1a560848c2c168814bde1b6153e9e65a286d7211f04628fdd", 6_666_667),
    ("NRT", "f03cbab49cf553a05541ddebc09b32a4c5507efb157d354b6d7f8c6682c32f5f", 6_666_666),
    ("SGP", "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b", 6_666_666),
)
AUTHORIZED_SIGNER_ORDER = tuple(row[0] for row in PRODUCTION_VALIDATORS[:SIGNATURES_REQUIRED])
UNUSED_RECOVERY_MEMBER = PRODUCTION_VALIDATORS[-1][0]


class ApprovalError(ValueError):
    """The proposed or sealed owner authorization is unsafe or inconsistent."""


def fail(message: str) -> NoReturn:
    raise ApprovalError(message)


def canonical_json_bytes(value: object) -> bytes:
    try:
        encoded = json.dumps(
            value,
            allow_nan=False,
            ensure_ascii=True,
            separators=(",", ":"),
            sort_keys=True,
        )
    except (TypeError, ValueError) as error:
        fail(f"value cannot be represented as canonical JSON: {error}")
    return (encoded + "\n").encode("utf-8")


def _reject_duplicate_pairs(pairs: Sequence[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, child in pairs:
        if key in value:
            fail(f"duplicate JSON key is forbidden: {key}")
        value[key] = child
    return value


def _reject_nonfinite(value: str) -> NoReturn:
    fail(f"non-finite JSON number is forbidden: {value}")


def parse_canonical_json(raw: bytes, label: str) -> Any:
    try:
        value = json.loads(
            raw.decode("utf-8", errors="strict"),
            object_pairs_hook=_reject_duplicate_pairs,
            parse_constant=_reject_nonfinite,
        )
    except ApprovalError:
        raise
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"{label} is not valid UTF-8 JSON: {error}")
    if canonical_json_bytes(value) != raw:
        fail(f"{label} is not canonical JSON")
    return value


def sha256_bytes(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def utc_now() -> dt.datetime:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0)


def parse_utc(value: object, label: str) -> dt.datetime:
    if not isinstance(value, str) or UTC_RE.fullmatch(value) is None:
        fail(f"{label} must be canonical UTC seconds")
    try:
        return dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=dt.timezone.utc
        )
    except ValueError as error:
        fail(f"{label} is invalid: {error}")


def require_hash(value: object, label: str) -> str:
    if not isinstance(value, str) or HASH_RE.fullmatch(value) is None:
        fail(f"{label} must be 64 lowercase hexadecimal characters")
    return value


def require_checkpoint_hash(value: object, label: str) -> str:
    if not isinstance(value, str):
        fail(f"{label} must be a checkpoint manifest hash")
    bare = value[2:] if value.startswith("0x") else value
    return "0x" + require_hash(bare, label)


def require_commit(value: object, label: str) -> str:
    if not isinstance(value, str) or COMMIT_RE.fullmatch(value) is None:
        fail(f"{label} must be one full lowercase Git commit")
    return value


def require_positive_int(value: object, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        fail(f"{label} must be a positive integer")
    return value


def exact_object(value: object, fields: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != fields:
        fail(f"{label} has missing, unknown, or unsupported fields")
    return value


def _path_parts(path: Path) -> tuple[str, ...]:
    if not path.is_absolute() or path.name in {"", ".", ".."}:
        fail(f"secure path must be absolute and name one file: {path}")
    if any(part in {"", ".", ".."} for part in path.parts[1:]):
        fail(f"secure path traversal is forbidden: {path}")
    if any(SAFE_PATH_COMPONENT_RE.fullmatch(part) is None for part in path.parts[1:]):
        fail(f"secure path contains an unsafe component: {path}")
    return path.parts[1:]


def _open_parent(path: Path) -> tuple[int, str]:
    parts = _path_parts(path)
    if not hasattr(os, "O_NOFOLLOW"):
        fail("O_NOFOLLOW is unavailable; refusing unsafe operator path")
    flags = (
        os.O_RDONLY
        | os.O_NOFOLLOW
        | getattr(os, "O_DIRECTORY", 0)
        | getattr(os, "O_CLOEXEC", 0)
    )
    descriptor = os.open("/", flags)
    operator_uid = os.geteuid()
    try:
        for component in parts[:-1]:
            try:
                child = os.open(component, flags, dir_fd=descriptor)
            except OSError as error:
                fail(f"cannot securely open path component {component}: {error}")
            try:
                details = os.fstat(child)
            except Exception:
                os.close(child)
                raise
            if not stat.S_ISDIR(details.st_mode):
                os.close(child)
                fail(f"secure path component is not a directory: {component}")
            if details.st_uid not in {0, operator_uid}:
                os.close(child)
                fail(f"secure path component is not root/operator owned: {component}")
            mode = stat.S_IMODE(details.st_mode)
            sticky_root_scratch = (
                details.st_uid == 0
                and bool(mode & stat.S_ISVTX)
                and bool(mode & 0o022)
            )
            if mode & 0o022 and not sticky_root_scratch:
                os.close(child)
                fail(f"secure path component has unsafe group/world write access: {component}")
            os.close(descriptor)
            descriptor = child
        parent = os.fstat(descriptor)
        if parent.st_uid != operator_uid:
            fail("secure output/input parent must be owned by the executing operator")
        if stat.S_IMODE(parent.st_mode) != 0o700:
            fail("secure output/input parent must have exact mode 0700")
        return descriptor, parts[-1]
    except Exception:
        os.close(descriptor)
        raise


def read_secure_file(
    path: Path,
    *,
    label: str,
    exact_modes: set[int],
    maximum_bytes: int = MAX_JSON_BYTES,
) -> bytes:
    parent_fd, name = _open_parent(path)
    descriptor = -1
    try:
        descriptor = os.open(
            name,
            os.O_RDONLY | os.O_NOFOLLOW | getattr(os, "O_CLOEXEC", 0),
            dir_fd=parent_fd,
        )
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1:
            fail(f"{label} must be a singly linked regular file")
        if before.st_uid != os.geteuid():
            fail(f"{label} must be owned by the executing operator")
        mode = stat.S_IMODE(before.st_mode)
        if mode not in exact_modes:
            expected = "/".join(f"{item:04o}" for item in sorted(exact_modes))
            fail(f"{label} mode must be {expected}, got {mode:04o}")
        chunks: list[bytes] = []
        size = 0
        while True:
            chunk = os.read(descriptor, min(64 * 1024, maximum_bytes + 1 - size))
            if not chunk:
                break
            chunks.append(chunk)
            size += len(chunk)
            if size > maximum_bytes:
                fail(f"{label} exceeds {maximum_bytes} bytes")
        after = os.fstat(descriptor)
        identity = lambda item: (
            item.st_dev,
            item.st_ino,
            item.st_size,
            item.st_mtime_ns,
            item.st_ctime_ns,
            item.st_nlink,
        )
        if identity(before) != identity(after):
            fail(f"{label} changed while it was read")
        return b"".join(chunks)
    except OSError as error:
        fail(f"cannot securely read {label}: {error}")
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        os.close(parent_fd)


def _entry_absent(parent_fd: int, name: str, label: str) -> None:
    try:
        os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    except FileNotFoundError:
        return
    except OSError as error:
        fail(f"cannot check create-only {label}: {error}")
    fail(f"create-only {label} already exists")


def _create_at(parent_fd: int, name: str, raw: bytes, mode: int, label: str) -> None:
    flags = (
        os.O_WRONLY
        | os.O_CREAT
        | os.O_EXCL
        | os.O_NOFOLLOW
        | getattr(os, "O_CLOEXEC", 0)
    )
    descriptor = -1
    try:
        descriptor = os.open(name, flags, mode, dir_fd=parent_fd)
        os.fchmod(descriptor, mode)
        offset = 0
        while offset < len(raw):
            written = os.write(descriptor, raw[offset:])
            if written <= 0:
                fail(f"short write while creating {label}")
            offset += written
        os.fsync(descriptor)
        details = os.fstat(descriptor)
        if (
            not stat.S_ISREG(details.st_mode)
            or details.st_nlink != 1
            or details.st_uid != os.geteuid()
            or stat.S_IMODE(details.st_mode) != mode
            or details.st_size != len(raw)
        ):
            fail(f"create-only {label} failed its owner/mode/type/size check")
    except FileExistsError:
        fail(f"create-only {label} already exists")
    except OSError as error:
        fail(f"cannot create {label}: {error}")
    finally:
        if descriptor >= 0:
            os.close(descriptor)


def create_one(path: Path, raw: bytes, mode: int, label: str) -> None:
    parent_fd, name = _open_parent(path)
    try:
        _entry_absent(parent_fd, name, label)
        _create_at(parent_fd, name, raw, mode, label)
        os.fsync(parent_fd)
    finally:
        os.close(parent_fd)


def create_receipt_pair(path: Path, raw: bytes) -> str:
    parent_fd, name = _open_parent(path)
    sidecar_name = name + ".sha256"
    digest = sha256_bytes(raw)
    sidecar = f"{digest}  {name}\n".encode("ascii")
    try:
        _entry_absent(parent_fd, name, "owner emergency-recovery receipt")
        _entry_absent(parent_fd, sidecar_name, "owner emergency-recovery receipt sidecar")
        # The sidecar is staged first and the receipt is the completion marker.
        # A crash or race leaves evidence behind and never permits overwrite.
        _create_at(
            parent_fd,
            sidecar_name,
            sidecar,
            0o400,
            "owner emergency-recovery receipt sidecar",
        )
        os.fsync(parent_fd)
        _create_at(parent_fd, name, raw, 0o400, "owner emergency-recovery receipt")
        os.fsync(parent_fd)
    finally:
        os.close(parent_fd)
    return digest


def load_validator_public_keys(path: Path, expected_sha256: str) -> tuple[list[dict[str, Any]], str]:
    expected = require_hash(expected_sha256, "validator public-key manifest SHA-256")
    raw = read_secure_file(
        path,
        label="validator public-key manifest",
        exact_modes={0o400, 0o444},
    )
    if sha256_bytes(raw) != expected:
        fail("validator public-key manifest differs from its explicit SHA-256")
    value = parse_canonical_json(raw, "validator public-key manifest")
    if not isinstance(value, list) or len(value) != len(PRODUCTION_VALIDATORS):
        fail("validator public-key manifest must contain the reviewed six identities")
    result: list[dict[str, Any]] = []
    seen_public_keys: set[str] = set()
    for ordinal, (expected_row, candidate) in enumerate(
        zip(PRODUCTION_VALIDATORS, value), start=1
    ):
        node, address, stake = expected_row
        row = exact_object(candidate, {"address", "public_key", "stake"}, f"validator {ordinal}")
        public_key = require_hash(row["public_key"], f"validator {ordinal} public key")
        if row["address"] != address or row["stake"] != stake:
            fail("validator public-key manifest differs from recovered genesis order/stake")
        if public_key in seen_public_keys:
            fail("validator public-key manifest contains duplicate public keys")
        seen_public_keys.add(public_key)
        result.append(
            {
                "address": address,
                "node": node,
                "ordinal": ordinal,
                "public_key": public_key,
                "stake": stake,
            }
        )
    return result, expected


def signing_policy(validators: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "authorized_signer_order": list(AUTHORIZED_SIGNER_ORDER),
        "minimum_signed_stake": MINIMUM_SIGNED_STAKE,
        "ordered_validators": validators,
        "signatures_required": SIGNATURES_REQUIRED,
        "strict_stake_supermajority_required": True,
        "total_stake": TOTAL_STAKE,
        "unused_recovery_member": UNUSED_RECOVERY_MEMBER,
        "validator_count": len(PRODUCTION_VALIDATORS),
    }


def validate_signing_policy(value: object) -> dict[str, Any]:
    policy = exact_object(
        value,
        {
            "authorized_signer_order",
            "minimum_signed_stake",
            "ordered_validators",
            "signatures_required",
            "strict_stake_supermajority_required",
            "total_stake",
            "unused_recovery_member",
            "validator_count",
        },
        "signing policy",
    )
    if (
        policy["authorized_signer_order"] != list(AUTHORIZED_SIGNER_ORDER)
        or policy["minimum_signed_stake"] != MINIMUM_SIGNED_STAKE
        or policy["signatures_required"] != SIGNATURES_REQUIRED
        or policy["strict_stake_supermajority_required"] is not True
        or policy["total_stake"] != TOTAL_STAKE
        or policy["unused_recovery_member"] != UNUSED_RECOVERY_MEMBER
        or policy["validator_count"] != len(PRODUCTION_VALIDATORS)
    ):
        fail("signing policy differs from the reviewed five-of-six strict-stake threshold")
    validators = policy["ordered_validators"]
    if not isinstance(validators, list) or len(validators) != len(PRODUCTION_VALIDATORS):
        fail("signing policy must contain exactly six ordered validators")
    public_keys: set[str] = set()
    for ordinal, (expected, candidate) in enumerate(
        zip(PRODUCTION_VALIDATORS, validators), start=1
    ):
        node, address, stake = expected
        row = exact_object(
            candidate,
            {"address", "node", "ordinal", "public_key", "stake"},
            f"receipt validator {ordinal}",
        )
        if (
            row["node"] != node
            or row["address"] != address
            or row["stake"] != stake
            or row["ordinal"] != ordinal
        ):
            fail("receipt validator order/identity/stake differs from recovered genesis")
        public_key = require_hash(row["public_key"], f"receipt validator {ordinal} public key")
        if public_key in public_keys:
            fail("receipt validators contain duplicate public keys")
        public_keys.add(public_key)
    selected_stake = sum(row[2] for row in PRODUCTION_VALIDATORS[:SIGNATURES_REQUIRED])
    if selected_stake < MINIMUM_SIGNED_STAKE:
        fail("authorized signer order does not satisfy strict stake supermajority")
    return policy


def scope_value(
    source_main_sha: str,
    checkpoint_manifest_hash: str,
    validator_public_keys_sha256: str,
) -> dict[str, Any]:
    return {
        "checkpoint_manifest_hash": require_checkpoint_hash(
            checkpoint_manifest_hash, "checkpoint manifest hash"
        ),
        "protected_branch": PROTECTED_BRANCH,
        "recovery_epoch": RECOVERY_EPOCH,
        "repository": REPOSITORY,
        "source_height": SOURCE_HEIGHT,
        "source_main_sha": require_commit(source_main_sha, "protected-main commit"),
        "transition_height": TRANSITION_HEIGHT,
        "validator_public_keys_sha256": require_hash(
            validator_public_keys_sha256, "validator public-key manifest SHA-256"
        ),
        "validator_set_id": VALIDATOR_SET_ID,
    }


def validate_scope(value: object) -> dict[str, Any]:
    scope = exact_object(
        value,
        {
            "checkpoint_manifest_hash",
            "protected_branch",
            "recovery_epoch",
            "repository",
            "source_height",
            "source_main_sha",
            "transition_height",
            "validator_public_keys_sha256",
            "validator_set_id",
        },
        "authorization scope",
    )
    if (
        scope["repository"] != REPOSITORY
        or scope["protected_branch"] != PROTECTED_BRANCH
        or scope["source_height"] != SOURCE_HEIGHT
        or scope["transition_height"] != TRANSITION_HEIGHT
        or scope["recovery_epoch"] != RECOVERY_EPOCH
        or scope["validator_set_id"] != VALIDATOR_SET_ID
    ):
        fail("authorization scope differs from the reviewed ARC v0.8 recovery boundary")
    require_commit(scope["source_main_sha"], "authorization protected-main commit")
    normalized_checkpoint = require_checkpoint_hash(
        scope["checkpoint_manifest_hash"], "authorization checkpoint hash"
    )
    if scope["checkpoint_manifest_hash"] != normalized_checkpoint:
        fail("authorization checkpoint hash must use canonical lowercase 0x encoding")
    require_hash(
        scope["validator_public_keys_sha256"],
        "authorization validator public-key manifest SHA-256",
    )
    return scope


def validate_decision(value: object) -> dict[str, Any]:
    decision = exact_object(
        value,
        {
            "approver",
            "approved_at",
            "authority_basis",
            "authorization_kind",
            "reason",
            "reason_code",
            "risk_acknowledgement",
        },
        "owner decision",
    )
    approver = exact_object(
        decision["approver"],
        {"github_login", "github_user_id", "repository_role"},
        "owner approver",
    )
    if approver != {
        "github_login": OWNER_LOGIN,
        "github_user_id": OWNER_USER_ID,
        "repository_role": "owner",
    }:
        fail("approver identity is not the pinned ARC repository owner")
    if (
        decision["authority_basis"] != AUTHORITY_BASIS
        or decision["authorization_kind"] != AUTHORIZATION_KIND
        or decision["reason"] != REASON
        or decision["reason_code"] != REASON_CODE
        or decision["risk_acknowledgement"] != RISK_ACKNOWLEDGEMENT
    ):
        fail("owner decision changes the emergency authority or risk acknowledgement")
    parse_utc(decision["approved_at"], "owner approval time")
    return decision


def validate_receipt(value: object) -> dict[str, Any]:
    receipt = exact_object(
        value,
        {"decision", "github_authentication", "schema", "scope", "signing_policy"},
        "owner emergency-recovery receipt",
    )
    if receipt["schema"] != RECEIPT_SCHEMA:
        fail("owner emergency-recovery receipt schema is unsupported")
    validate_decision(receipt["decision"])
    validate_scope(receipt["scope"])
    validate_signing_policy(receipt["signing_policy"])
    authentication = exact_object(
        receipt["github_authentication"],
        {
            "actor",
            "event",
            "head_branch",
            "head_sha",
            "repository",
            "run_attempt",
            "run_id",
            "triggering_actor",
            "workflow_path",
        },
        "GitHub authentication",
    )
    if (
        authentication["repository"] != REPOSITORY
        or authentication["workflow_path"] != WORKFLOW_PATH
        or authentication["event"] != "workflow_dispatch"
        or authentication["head_branch"] != PROTECTED_BRANCH
    ):
        fail("receipt GitHub authentication does not name the protected owner workflow")
    require_commit(authentication["head_sha"], "GitHub authentication head SHA")
    require_positive_int(authentication["run_id"], "GitHub authentication run ID")
    require_positive_int(authentication["run_attempt"], "GitHub authentication run attempt")
    expected_actor = {"login": OWNER_LOGIN, "user_id": OWNER_USER_ID}
    for field in ("actor", "triggering_actor"):
        actor = exact_object(authentication[field], {"login", "user_id"}, field)
        if actor != expected_actor:
            fail(f"receipt {field} is not the pinned repository owner")
    return receipt


def parse_api_json(raw: bytes, label: str) -> dict[str, Any]:
    try:
        value = json.loads(
            raw.decode("utf-8", errors="strict"),
            object_pairs_hook=_reject_duplicate_pairs,
            parse_constant=_reject_nonfinite,
        )
    except ApprovalError:
        raise
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"{label} is not valid JSON: {error}")
    if not isinstance(value, dict):
        fail(f"{label} must be a JSON object")
    return value


def artifact_receipt(zip_raw: bytes) -> bytes:
    try:
        with zipfile.ZipFile(io.BytesIO(zip_raw), "r") as archive:
            members = archive.infolist()
            if len(members) != 1 or members[0].filename != ARTIFACT_MEMBER:
                fail(f"approval artifact must contain only {ARTIFACT_MEMBER}")
            member = members[0]
            if member.is_dir() or member.flag_bits & 0x1:
                fail("approval artifact member is a directory or encrypted")
            if member.file_size <= 0 or member.file_size > MAX_JSON_BYTES:
                fail("approval artifact receipt has an unsupported size")
            if member.compress_size <= 0 or member.file_size > member.compress_size * 200 + 4096:
                fail("approval artifact receipt has an unsafe compression ratio")
            raw = archive.read(member)
            if len(raw) != member.file_size:
                fail("approval artifact receipt was not read completely")
            return raw
    except (OSError, zipfile.BadZipFile, RuntimeError) as error:
        fail(f"approval artifact is not a safe ZIP: {error}")


def _github_actor(value: object, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be a GitHub actor object")
    actor = {"login": value.get("login"), "id": value.get("id")}
    if actor != {"login": OWNER_LOGIN, "id": OWNER_USER_ID}:
        fail(f"{label} is not the pinned repository owner")
    return actor


def verify_github_artifact(args: argparse.Namespace) -> str:
    run_id = require_positive_int(args.run_id, "approval run ID")
    run_attempt = require_positive_int(args.run_attempt, "approval run attempt")
    artifact_id = require_positive_int(args.artifact_id, "approval artifact ID")
    digest_value = args.artifact_digest
    if not isinstance(digest_value, str) or not digest_value.startswith("sha256:"):
        fail("approval artifact digest must have one sha256: prefix")
    artifact_digest = require_hash(digest_value[7:], "approval artifact digest")
    workflow = parse_api_json(
        read_secure_file(args.workflow_json, label="approval workflow API JSON", exact_modes={0o400}),
        "approval workflow API JSON",
    )
    run = parse_api_json(
        read_secure_file(args.run_json, label="approval run API JSON", exact_modes={0o400}),
        "approval run API JSON",
    )
    jobs = parse_api_json(
        read_secure_file(
            args.jobs_json,
            label="approval exact-attempt jobs API JSON",
            exact_modes={0o400},
        ),
        "approval exact-attempt jobs API JSON",
    )
    artifact = parse_api_json(
        read_secure_file(args.artifact_json, label="approval artifact API JSON", exact_modes={0o400}),
        "approval artifact API JSON",
    )
    zip_raw = read_secure_file(
        args.artifact_zip,
        label="approval artifact ZIP",
        exact_modes={0o400},
        maximum_bytes=4 * 1024 * 1024,
    )
    workflow_id = require_positive_int(workflow.get("id"), "approval workflow ID")
    if workflow.get("path") != WORKFLOW_PATH:
        fail("approval workflow API object has another path")
    if (
        run.get("id") != run_id
        or run.get("run_attempt") != run_attempt
        or run.get("workflow_id") != workflow_id
        or run.get("path") != WORKFLOW_PATH
        or run.get("event") != "workflow_dispatch"
        or run.get("head_branch") != PROTECTED_BRANCH
        or run.get("head_sha") != args.source_main_sha
        or not isinstance(run.get("head_repository"), dict)
        or run["head_repository"].get("full_name") != REPOSITORY
        or run.get("status") != "completed"
        or run.get("conclusion") != "success"
    ):
        fail("approval run is not the successful exact protected-main workflow attempt")
    _github_actor(run.get("actor"), "approval run actor")
    _github_actor(run.get("triggering_actor"), "approval run triggering actor")
    job_rows = jobs.get("jobs")
    if jobs.get("total_count") != 1 or not isinstance(job_rows, list) or len(job_rows) != 1:
        fail("approval exact-attempt jobs do not contain exactly one authorization job")
    job = job_rows[0]
    if not isinstance(job, dict):
        fail("approval exact-attempt job is not a JSON object")
    if (
        job.get("run_id") != run_id
        or job.get("run_attempt") != run_attempt
        or job.get("head_sha") != args.source_main_sha
        or job.get("name") != "authenticate owner and seal exact recovery decision"
        or job.get("status") != "completed"
        or job.get("conclusion") != "success"
        or job.get("started_at") is None
        or job.get("completed_at") is None
    ):
        fail("approval authorization job is not the successful exact workflow attempt")
    expected_artifact_name = (
        f"arc-owner-emergency-recovery-{args.source_main_sha}-{run_id}-attempt-{run_attempt}"
    )
    workflow_run = artifact.get("workflow_run")
    if (
        artifact.get("id") != artifact_id
        or artifact.get("name") != expected_artifact_name
        or artifact.get("digest") != digest_value
        or artifact.get("expired") is not False
        or isinstance(artifact.get("size_in_bytes"), bool)
        or not isinstance(artifact.get("size_in_bytes"), int)
        or artifact["size_in_bytes"] <= 0
        or artifact["size_in_bytes"] > 4 * 1024 * 1024
        or artifact["size_in_bytes"] != len(zip_raw)
        or not isinstance(workflow_run, dict)
        or workflow_run.get("id") != run_id
        or workflow_run.get("head_sha") != args.source_main_sha
    ):
        fail("approval artifact API identity differs from the exact run attempt")
    if sha256_bytes(zip_raw) != artifact_digest:
        fail("downloaded approval artifact differs from the GitHub server digest")
    receipt_raw = artifact_receipt(zip_raw)
    receipt = validate_receipt(
        parse_canonical_json(receipt_raw, "owner emergency-recovery receipt")
    )
    expected_scope = scope_value(
        args.source_main_sha,
        args.checkpoint_manifest_hash,
        args.validator_public_keys_sha256,
    )
    if receipt["scope"] != expected_scope:
        fail("owner emergency-recovery receipt does not authorize these exact signing inputs")
    validators, public_digest = load_validator_public_keys(
        args.validator_public_keys, args.validator_public_keys_sha256
    )
    if public_digest != receipt["scope"]["validator_public_keys_sha256"]:
        fail("receipt validator public-key digest differs from the signing input")
    if receipt["signing_policy"] != signing_policy(validators):
        fail("receipt validator identities/order differ from the signing input")
    authentication = receipt["github_authentication"]
    if authentication != {
        "actor": {"login": OWNER_LOGIN, "user_id": OWNER_USER_ID},
        "event": "workflow_dispatch",
        "head_branch": PROTECTED_BRANCH,
        "head_sha": args.source_main_sha,
        "repository": REPOSITORY,
        "run_attempt": run_attempt,
        "run_id": run_id,
        "triggering_actor": {"login": OWNER_LOGIN, "user_id": OWNER_USER_ID},
        "workflow_path": WORKFLOW_PATH,
    }:
        fail("receipt is not bound to the selected owner-authenticated run attempt")
    max_age = require_positive_int(args.max_age_seconds, "maximum receipt age")
    now = utc_now()
    approved_at = parse_utc(receipt["decision"]["approved_at"], "owner approval time")
    age = (now - approved_at).total_seconds()
    if age < -FUTURE_TOLERANCE_SECONDS:
        fail("owner approval time is unreasonably in the future")
    if age > max_age:
        fail("owner emergency-recovery receipt is stale; create and review a fresh receipt")
    started_at = parse_utc(run.get("run_started_at"), "approval run start time")
    updated_at = parse_utc(run.get("updated_at"), "approval run update time")
    job_started_at = parse_utc(job.get("started_at"), "approval job start time")
    job_completed_at = parse_utc(job.get("completed_at"), "approval job completion time")
    if job_started_at < started_at - dt.timedelta(seconds=FUTURE_TOLERANCE_SECONDS):
        fail("approval job predates the authenticated workflow attempt")
    if job_completed_at > updated_at + dt.timedelta(seconds=FUTURE_TOLERANCE_SECONDS):
        fail("approval job completion postdates the authenticated workflow attempt")
    if job_completed_at < job_started_at:
        fail("approval job completion predates its start")
    if approved_at < started_at - dt.timedelta(seconds=FUTURE_TOLERANCE_SECONDS):
        fail("owner approval predates the authenticated workflow attempt")
    if approved_at > updated_at + dt.timedelta(seconds=FUTURE_TOLERANCE_SECONDS):
        fail("owner approval postdates the authenticated workflow attempt")
    receipt_digest = sha256_bytes(receipt_raw)
    create_receipt_pair(args.output, receipt_raw)
    return receipt_digest


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(
        description="Verify and materialize a GitHub-owner-authenticated ARC recovery receipt"
    )
    subcommands = command.add_subparsers(dest="command", required=True)
    verifier = subcommands.add_parser(
        "verify-github-artifact",
        help="verify one exact GitHub owner workflow artifact immediately before signing",
    )
    verifier.add_argument("--workflow-json", required=True, type=Path)
    verifier.add_argument("--run-json", required=True, type=Path)
    verifier.add_argument("--jobs-json", required=True, type=Path)
    verifier.add_argument("--artifact-json", required=True, type=Path)
    verifier.add_argument("--artifact-zip", required=True, type=Path)
    verifier.add_argument("--run-id", required=True, type=int)
    verifier.add_argument("--run-attempt", required=True, type=int)
    verifier.add_argument("--artifact-id", required=True, type=int)
    verifier.add_argument("--artifact-digest", required=True)
    verifier.add_argument("--source-main-sha", required=True)
    verifier.add_argument("--checkpoint-manifest-hash", required=True)
    verifier.add_argument("--validator-public-keys", required=True, type=Path)
    verifier.add_argument("--validator-public-keys-sha256", required=True)
    verifier.add_argument("--output", required=True, type=Path)
    verifier.add_argument(
        "--max-age-seconds", type=int, default=DEFAULT_MAX_AGE_SECONDS
    )
    verifier.set_defaults(handler=verify_github_artifact)
    return command


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        digest = args.handler(args)
    except ApprovalError as error:
        print(f"owner-emergency-recovery: ERROR: {error}", file=sys.stderr)
        return 1
    status = "VERIFIED_GITHUB_OWNER_EMERGENCY_RECOVERY"
    print(canonical_json_bytes({"sha256": digest, "status": status}).decode("ascii"), end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
