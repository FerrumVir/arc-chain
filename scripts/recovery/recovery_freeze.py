"""Fail-closed recovery journal primitives for the ARC freeze v5 protocol.

This module deliberately contains no production host implementation.  It
validates pinned inputs, constructs and validates canonical journal events,
and delegates the four permitted host mutations to an explicitly supplied
adapter.  The default adapter refuses every mutation.

Canonical JSON in this protocol is UTF-8, has lexicographically sorted object
keys, uses compact separators, rejects duplicate keys/non-finite numbers, and
ends with exactly one LF byte.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import stat
from dataclasses import dataclass
from enum import Enum
from pathlib import PurePosixPath
from typing import Any, Mapping, Protocol, Sequence, runtime_checkable


FREEZE_PLAN_SCHEMA = "arc.recovery.freeze-plan.v5"
PREPARE_RECEIPT_SCHEMA = "arc.recovery.freeze-prepare-receipt.v1"
BARRIER_ARM_SCHEMA = "arc.recovery.restart-barrier-arm.v1"
BARRIER_COMMIT_SCHEMA = "arc.recovery.restart-barrier-commit.v1"
CGROUP_FREEZE_EVENT_SCHEMA = "arc.recovery.cgroup-freeze-event.v1"
PIDFD_TERM_EVENT_SCHEMA = "arc.recovery.pidfd-term-event.v1"
CGROUP_THAW_EVENT_SCHEMA = "arc.recovery.cgroup-thaw-event.v1"
OFFLINE_RECONCILIATION_SCHEMA = (
    "arc.recovery.zero-signal-offline-reconciliation.v1"
)

ARC_NODE_ORDER = ("nyc", "lax", "ams", "lhr", "nrt", "sgp")
ARC_SENTINEL_ORDER = ("nyc", "lax")
DEFAULT_ALLOW_MARKER_PATH = "/etc/arc-recovery/legacy-start-allowed"
MAX_JOURNAL_BYTES = 32 * 1024 * 1024

_HASH_RE = re.compile(r"[0-9a-f]{64}")
_TIMESTAMP_RE = re.compile(
    r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z"
)
_UUID_RE = re.compile(
    r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"
)
_WINDOW_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._:@+-]{0,127}")
_NAME_RE = re.compile(r"[a-z][a-z0-9-]{0,31}")
_HOST_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9.:-]{0,254}")
_CONTROL_GROUP_RE = re.compile(r"/[A-Za-z0-9._@/:-]+")
_COMMIT_RE = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})")
_INVOCATION_ID_RE = re.compile(r"[0-9a-f]{32}")

_PLAN_KEYS = frozenset(
    {
        "schema",
        "window",
        "created_at",
        "sentinels",
        "nodes",
        "remote_helper_sha256",
        "orchestrator_sha256",
        "rollout_tool_sha256",
        "rollout_schema_sha256",
        "operator_python_path",
        "operator_python_sha256",
        "source_commit",
        "legacy_validator_set_sha256",
        "writer_contracts_sha256",
        "drive_prefreeze",
        "quorum_proof",
    }
)

_NODE_KEYS = frozenset(
    {
        "name",
        "host",
        "boot_id",
        "writer_pid",
        "writer_start_ticks",
        "writer_cgroup_sha256",
        "writer_cgroup_path",
        "writer_cgroup_device",
        "writer_cgroup_inode",
        "writer_supervision_mode",
        "supervisor_unit",
        "supervisor_main_pid",
        "supervisor_start_ticks",
        "supervisor_executable_path",
        "supervisor_executable_sha256",
        "supervisor_argv_sha256",
        "supervisor_context",
        "supervisor_context_sha256",
        "prepare_barrier",
        "executable_path",
        "executable_sha256",
        "argv_sha256",
        "data_dir",
        "model_path",
        "model_sha256",
        "model_size_bytes",
        "shard_ranges",
        "data_device",
        "data_bytes",
        "data_files",
        "capture_device",
        "available_bytes",
        "available_inodes",
        "required_free_bytes",
        "required_free_inodes",
        "new_v3_headroom_bytes",
        "max_binding_temporary_bytes",
        "archive_stream_temporary_bytes",
        "validator_address",
        "stake",
        "rpc_origin",
        "observed_positive_validators",
        "observed_validator_error",
    }
)

_SUPERVISOR_CONTEXT_KEYS = frozenset(
    {
        "schema",
        "unit",
        "unit_configuration_sha256",
        "lifecycle_hooks",
        "automatic_lifecycle",
        "invocation_id",
        "control_group",
        "interpreter_payloads",
        "allowed_transient_sleep",
        "term_traps_rejected",
    }
)

_LIFECYCLE_HOOK_KEYS = frozenset(
    {
        "ExecReload",
        "ExecStop",
        "ExecStopPost",
        "OnFailure",
        "OnSuccess",
        "SuccessAction",
        "FailureAction",
        "JobTimeoutAction",
    }
)

_AUTOMATIC_LIFECYCLE_KEYS = frozenset(
    {
        "WatchdogUSec",
        "RuntimeMaxUSec",
        "RuntimeRandomizedExtraUSec",
        "StopWhenUnneeded",
        "BindsTo",
        "PartOf",
        "PropagatesStopTo",
        "OOMPolicy",
        "Requires",
        "Requisite",
        "Conflicts",
        "Upholds",
        "UpheldBy",
        "TriggeredBy",
        "RequiredBy",
        "WantedBy",
        "BoundBy",
        "ConflictedBy",
        "OnFailureOf",
        "OnSuccessOf",
        "CanReload",
        "StopPropagatedFrom",
        "ReloadPropagatedFrom",
    }
)

_PREPARE_BARRIER_UNITS = (
    "arc-self-heal.service",
    "arc-node.service",
    "arc-node-update.service",
    "arc-node-update.timer",
)
_PREPARE_BARRIER_UNIT_SET = frozenset(_PREPARE_BARRIER_UNITS)
_PREPARE_BARRIER_KEYS = frozenset(
    {
        "schema",
        "allow_marker",
        "persistent_start_barriers",
        "merged_unit_sources",
        "unit_states",
        "activation_closure",
        "boot_activation",
        "selected_unit",
        "selected_main_pid",
        "alternatives_inactive_no_jobs",
        "alternative_enablement_sync_completed",
        "writer_cgroup_relationship_sealed",
    }
)
_PREPARE_MARKER_KEYS = frozenset(
    {"path", "sha256", "mode", "uid", "gid", "device"}
)
_PREPARE_DROPIN_KEYS = frozenset({"path", "sha256", "mode", "uid", "gid"})
_PREPARE_SOURCE_KEYS = frozenset({"path", "sha256"})
_PREPARE_UNIT_STATE_KEYS = frozenset(
    {"active_state", "sub_state", "main_pid", "job", "enablement"}
)
_PREPARE_CLOSURE_KEYS = frozenset(
    {
        "Names",
        "Id",
        "Following",
        "ActiveState",
        "SubState",
        "MainPID",
        "Job",
        "ControlGroup",
        "FreezerState",
        "Restart",
        "KillMode",
        "SendSIGKILL",
        "OOMPolicy",
        "WatchdogUSec",
        "RuntimeMaxUSec",
        "RuntimeRandomizedExtraUSec",
        "CanReload",
        "StopWhenUnneeded",
        "BindsTo",
        "PartOf",
        "PropagatesStopTo",
        "StopPropagatedFrom",
        "ReloadPropagatedFrom",
        "Upholds",
        "UpheldBy",
        "TriggeredBy",
        "RequiredBy",
        "BoundBy",
        "ConflictedBy",
        "WantedBy",
        "OnFailureOf",
        "OnSuccessOf",
    }
)
_PREPARE_REVERSE_ACTIVATION_FIELDS = (
    "RequiredBy",
    "WantedBy",
    "BoundBy",
    "UpheldBy",
    "TriggeredBy",
    "OnFailureOf",
    "OnSuccessOf",
)
_TERMINAL_ENABLEMENT_STATES = frozenset(
    {
        "disabled",
        "masked",
        "masked-runtime",
        "static",
        "indirect",
        "generated",
        "transient",
        "not-found",
    }
)
_ALLOW_MARKER_PAYLOAD = b"schema=arc.recovery.legacy-start-allow.v1\n"
_CONDITION_ONLY_BARRIER = (
    b"[Unit]\nConditionPathExists=/etc/arc-recovery/legacy-start-allowed\n"
)
_ALLOW_MARKER_SHA256 = hashlib.sha256(_ALLOW_MARKER_PAYLOAD).hexdigest()
_CONDITION_ONLY_BARRIER_SHA256 = hashlib.sha256(_CONDITION_ONLY_BARRIER).hexdigest()

_DRIVE_KEYS = frozenset(
    {
        "gate_sha256",
        "remote_root",
        "remote_root_sha256",
        "oauth_client_id_sha256",
        "account_sha256",
        "daily_upload_budget_bytes",
        "dedicated_no_other_upload_writers_attested",
    }
)

_QUORUM_KEYS = frozenset(
    {
        "source_total_stake",
        "source_quorum_stake",
        "controlled_writer_stake",
        "maximum_source_stake_after_controlled_stop",
        "controlled_quorum_unavailable_after_all_stops",
        "global_legacy_halt_claimed",
        "external_source_validators",
        "untrusted_external_observations",
        "dynamic_membership_disagrees",
    }
)

_PREPARE_KEYS = frozenset(
    {
        "schema",
        "freeze_plan_sha256",
        "node",
        "host",
        "node_contract_sha256",
        "sealed_boot_id",
        "allow_marker_path",
        "allow_marker_present",
        "cgroups",
        "prepared_at",
    }
)

_CGROUP_IDENTITY_KEYS = frozenset({"role", "path", "device", "inode"})

_BARRIER_ARM_KEYS = frozenset(
    {
        "schema",
        "freeze_plan_sha256",
        "node",
        "prepare_receipt_sha256",
        "sealed_boot_id",
        "allow_marker_path",
        "allow_marker_observed_present",
        "armed_at",
    }
)

_BARRIER_COMMIT_KEYS = frozenset(
    {
        "schema",
        "freeze_plan_sha256",
        "node",
        "barrier_arm_sha256",
        "sealed_boot_id",
        "observed_boot_id",
        "allow_marker_path",
        "allow_marker_absent",
        "unlink_parent_fsynced",
        "durability_basis",
        "committed_at",
    }
)

_CGROUP_EVENT_KEYS = frozenset(
    {
        "schema",
        "freeze_plan_sha256",
        "node",
        "sealed_boot_id",
        "role",
        "cgroup_path",
        "cgroup_device",
        "cgroup_inode",
        "phase",
        "cgroup_freeze_value",
        "observed_frozen",
        "occurred_at",
    }
)

_TERM_EVENT_KEYS = frozenset(
    {
        "schema",
        "freeze_plan_sha256",
        "node",
        "sealed_boot_id",
        "target_role",
        "pid",
        "start_ticks",
        "phase",
        "signal",
        "delivery",
        "cgroups_frozen",
        "term_state",
        "recovery_sigkill_sent",
        "exit_cause",
        "occurred_at",
    }
)

_THAW_EVENT_KEYS = frozenset(
    {
        "schema",
        "freeze_plan_sha256",
        "node",
        "sealed_boot_id",
        "role",
        "cgroup_path",
        "cgroup_device",
        "cgroup_inode",
        "phase",
        "cgroup_freeze_value",
        "observed_frozen",
        "no_signal_replayed_after_own_stage_thaw_intent",
        "occurred_at",
    }
)

_TARGET_ABSENCE_KEYS = frozenset(
    {"role", "sealed_pid", "sealed_start_ticks", "state", "stable_checks"}
)
_CGROUP_ABSENCE_KEYS = frozenset(
    {"role", "path", "device", "inode", "state", "stable_checks"}
)
_OFFLINE_RECONCILIATION_KEYS = frozenset(
    {
        "schema",
        "freeze_plan_sha256",
        "node",
        "barrier_arm_sha256",
        "sealed_boot_id",
        "observed_boot_id",
        "barrier_state",
        "commit_durability_basis",
        "target_absence",
        "cgroup_absence",
        "persistent_restart_fence_verified",
        "service_enablement_verified",
        "signals_sent",
        "supervisor_pidfd_sigterm_state",
        "writer_pidfd_sigterm_state",
        "recovery_sigkill_sent",
        "exit_cause",
        "reconciled_at",
    }
)


class FreezeValidationError(ValueError):
    """A pinned input or journal value violates the v5 contract."""


class MutationRefused(RuntimeError):
    """A host mutation was attempted without an explicit reviewed adapter."""


class BarrierState(str, Enum):
    UNARMED = "unarmed"
    ARMED = "armed"
    COMMITTED = "committed"


@dataclass(frozen=True, slots=True)
class FreezeNode:
    name: str
    host: str
    boot_id: str
    validator_address: str
    stake: int
    writer_pid: int
    writer_start_ticks: int
    writer_supervision_mode: str
    writer_cgroup_path: str
    writer_cgroup_device: int
    writer_cgroup_inode: int
    supervisor_main_pid: int
    supervisor_start_ticks: int
    canonical_bytes: bytes
    sha256: str

    def value(self) -> dict[str, Any]:
        """Return a fresh decoded copy of the pinned node contract."""

        value = parse_canonical_json(self.canonical_bytes, label=f"node {self.name}")
        if not isinstance(value, dict):  # Defensive; construction already proved this.
            raise FreezeValidationError(f"node {self.name} is not an object")
        return value


@dataclass(frozen=True, slots=True)
class PinnedFreezePlan:
    sha256: str
    window: str
    created_at: str
    sentinels: tuple[str, ...]
    nodes: tuple[FreezeNode, ...]
    canonical_bytes: bytes

    def node(self, name: str) -> FreezeNode:
        matches = tuple(node for node in self.nodes if node.name == name)
        if len(matches) != 1:
            raise FreezeValidationError(f"freeze plan has no unique node named {name!r}")
        return matches[0]

    def value(self) -> dict[str, Any]:
        value = parse_canonical_json(self.canonical_bytes, label="freeze plan")
        if not isinstance(value, dict):  # Defensive; construction already proved this.
            raise FreezeValidationError("freeze plan is not an object")
        return value


@dataclass(frozen=True, slots=True)
class CgroupIdentity:
    role: str
    path: str
    device: int
    inode: int

    def value(self) -> dict[str, Any]:
        return {
            "role": self.role,
            "path": self.path,
            "device": self.device,
            "inode": self.inode,
        }


@dataclass(frozen=True, slots=True)
class TargetIdentity:
    role: str
    pid: int
    start_ticks: int


@dataclass(frozen=True, slots=True)
class DurableUnlinkEvidence:
    path: str
    marker_absent: bool
    parent_directory_fsynced: bool


@dataclass(frozen=True, slots=True)
class BarrierInference:
    state: BarrierState
    durability_basis: str | None
    sealed_boot_id: str
    observed_boot_id: str
    allow_marker_path: str
    unlink_parent_fsynced: bool


@runtime_checkable
class HostMutationAdapter(Protocol):
    """Reviewed host operations required by a recovery executor.

    Implementations are responsible for identity-safe, race-safe operations
    and must return only after the requested state has been read back.
    """

    def durable_unlink_allow_marker(self, path: str) -> DurableUnlinkEvidence:
        ...

    def set_cgroup_frozen(self, cgroup: CgroupIdentity, frozen: bool) -> None:
        ...

    def send_pidfd_sigterm(self, target: TargetIdentity) -> None:
        ...


class FailClosedHostMutationAdapter:
    """Default adapter: importing/using the library cannot mutate a host."""

    @staticmethod
    def _refuse(operation: str) -> None:
        raise MutationRefused(
            f"{operation} requires an explicitly supplied reviewed host adapter"
        )

    def durable_unlink_allow_marker(self, path: str) -> DurableUnlinkEvidence:
        self._refuse(f"unlink {path}")
        raise AssertionError("unreachable")

    def set_cgroup_frozen(self, cgroup: CgroupIdentity, frozen: bool) -> None:
        state = "freeze" if frozen else "thaw"
        self._refuse(f"{state} cgroup {cgroup.path}")

    def send_pidfd_sigterm(self, target: TargetIdentity) -> None:
        self._refuse(f"pidfd SIGTERM for {target.role} pid {target.pid}")


class RecoveryMutations:
    """Small injectable boundary around all host-changing operations."""

    def __init__(self, adapter: HostMutationAdapter | None = None) -> None:
        self._adapter = adapter if adapter is not None else FailClosedHostMutationAdapter()

    def durable_unlink_allow_marker(self, path: str) -> DurableUnlinkEvidence:
        _require_absolute_path(path, "allow marker path")
        evidence = self._adapter.durable_unlink_allow_marker(path)
        if not isinstance(evidence, DurableUnlinkEvidence):
            raise FreezeValidationError("unlink adapter returned malformed evidence")
        if evidence.path != path:
            raise FreezeValidationError("unlink adapter returned evidence for another path")
        if evidence.marker_absent is not True:
            raise FreezeValidationError("allow marker remains present after unlink")
        if evidence.parent_directory_fsynced is not True:
            raise FreezeValidationError("allow-marker parent directory was not fsynced")
        return evidence

    def freeze_cgroup(self, cgroup: CgroupIdentity) -> None:
        _validate_cgroup_identity(cgroup.value(), "cgroup")
        self._adapter.set_cgroup_frozen(cgroup, True)

    def send_pidfd_sigterm(self, target: TargetIdentity) -> None:
        _validate_target_identity(target, "target")
        self._adapter.send_pidfd_sigterm(target)

    def thaw_cgroup(self, cgroup: CgroupIdentity) -> None:
        _validate_cgroup_identity(cgroup.value(), "cgroup")
        self._adapter.set_cgroup_frozen(cgroup, False)


def _reject_duplicate_pairs(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise FreezeValidationError(f"duplicate JSON key {key!r}")
        value[key] = item
    return value


def _reject_nonfinite(token: str) -> Any:
    raise FreezeValidationError(f"non-finite JSON number {token!r} is forbidden")


def canonical_json_bytes(value: Any) -> bytes:
    """Encode a journal value in ARC's deterministic canonical form."""

    try:
        encoded = json.dumps(
            value,
            sort_keys=True,
            separators=(",", ":"),
            allow_nan=False,
            ensure_ascii=True,
        )
    except (TypeError, ValueError) as error:
        raise FreezeValidationError(f"value cannot be canonical JSON: {error}") from error
    return (encoded + "\n").encode("utf-8")


def parse_canonical_json(raw: bytes, *, label: str = "journal") -> Any:
    """Decode canonical JSON, rejecting duplicates and alternate encodings."""

    if not isinstance(raw, bytes):
        raise TypeError("canonical JSON input must be bytes")
    try:
        text = raw.decode("utf-8", errors="strict")
        value = json.loads(
            text,
            object_pairs_hook=_reject_duplicate_pairs,
            parse_constant=_reject_nonfinite,
        )
    except FreezeValidationError:
        raise
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise FreezeValidationError(f"{label} is not valid UTF-8 JSON: {error}") from error
    if canonical_json_bytes(value) != raw:
        raise FreezeValidationError(f"{label} is not canonical JSON")
    return value


def canonical_sha256(value: Any) -> str:
    return hashlib.sha256(canonical_json_bytes(value)).hexdigest()


def validate_secure_stat(
    details: os.stat_result,
    *,
    label: str = "file",
    expected_uid: int = 0,
) -> None:
    """Require a regular, pinned-owner, immutable-by-mode input inode."""

    if not stat.S_ISREG(details.st_mode):
        raise FreezeValidationError(f"{label} is not a regular file")
    if details.st_uid != expected_uid:
        raise FreezeValidationError(
            f"{label} owner uid {details.st_uid} differs from required uid {expected_uid}"
        )
    if details.st_mode & 0o222:
        raise FreezeValidationError(f"{label} has a write permission bit set")


def open_regular_nofollow(
    path: str | os.PathLike[str],
    *,
    expected_uid: int = 0,
    label: str = "file",
) -> int:
    """Open a secure input and return its descriptor to the caller."""

    if not hasattr(os, "O_NOFOLLOW"):
        raise FreezeValidationError("O_NOFOLLOW is unavailable; refusing unsafe open")
    flags = os.O_RDONLY | os.O_NOFOLLOW | getattr(os, "O_CLOEXEC", 0)
    try:
        descriptor = os.open(os.fspath(path), flags)
    except OSError as error:
        raise FreezeValidationError(f"cannot securely open {label}: {error}") from error
    try:
        validate_secure_stat(os.fstat(descriptor), label=label, expected_uid=expected_uid)
    except Exception:
        os.close(descriptor)
        raise
    return descriptor


def read_regular_nofollow(
    path: str | os.PathLike[str],
    *,
    expected_uid: int = 0,
    label: str = "file",
    maximum_bytes: int = MAX_JOURNAL_BYTES,
) -> bytes:
    """Read a stable secure regular file without following a final symlink."""

    if isinstance(maximum_bytes, bool) or not isinstance(maximum_bytes, int) or maximum_bytes <= 0:
        raise ValueError("maximum_bytes must be a positive integer")
    descriptor = open_regular_nofollow(path, expected_uid=expected_uid, label=label)
    try:
        before = os.fstat(descriptor)
        if before.st_size > maximum_bytes:
            raise FreezeValidationError(f"{label} exceeds {maximum_bytes} bytes")
        chunks: list[bytes] = []
        remaining = maximum_bytes + 1
        while remaining:
            chunk = os.read(descriptor, min(1024 * 1024, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        raw = b"".join(chunks)
        if len(raw) > maximum_bytes:
            raise FreezeValidationError(f"{label} exceeds {maximum_bytes} bytes")
        after = os.fstat(descriptor)
        identity_before = (
            before.st_dev,
            before.st_ino,
            before.st_size,
            before.st_mtime_ns,
            before.st_ctime_ns,
        )
        identity_after = (
            after.st_dev,
            after.st_ino,
            after.st_size,
            after.st_mtime_ns,
            after.st_ctime_ns,
        )
        if identity_before != identity_after or len(raw) != before.st_size:
            raise FreezeValidationError(f"{label} changed while it was read")
        return raw
    finally:
        os.close(descriptor)


def _exact_keys(value: Any, expected: frozenset[str], label: str) -> Mapping[str, Any]:
    if not isinstance(value, dict):
        raise FreezeValidationError(f"{label} must be an object")
    observed = frozenset(value)
    if observed != expected:
        missing = sorted(expected - observed)
        unknown = sorted(observed - expected)
        raise FreezeValidationError(
            f"{label} fields differ (missing={missing}, unknown={unknown})"
        )
    return value


def _require_string(value: Any, label: str, *, nonempty: bool = True) -> str:
    if not isinstance(value, str) or (nonempty and not value):
        raise FreezeValidationError(f"{label} must be a{' non-empty' if nonempty else ''} string")
    if "\x00" in value:
        raise FreezeValidationError(f"{label} contains NUL")
    return value


def _require_hash(value: Any, label: str) -> str:
    if not isinstance(value, str) or not _HASH_RE.fullmatch(value):
        raise FreezeValidationError(f"{label} must be a lowercase SHA-256 digest")
    return value


def _require_timestamp(value: Any, label: str) -> str:
    if not isinstance(value, str) or not _TIMESTAMP_RE.fullmatch(value):
        raise FreezeValidationError(f"{label} must be a whole-second UTC timestamp")
    return value


def _require_uuid(value: Any, label: str) -> str:
    if not isinstance(value, str) or not _UUID_RE.fullmatch(value):
        raise FreezeValidationError(f"{label} must be a lowercase UUID")
    return value


def _require_uint(value: Any, label: str, *, positive: bool = False) -> int:
    minimum = 1 if positive else 0
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        qualifier = "positive" if positive else "non-negative"
        raise FreezeValidationError(f"{label} must be a {qualifier} integer")
    return value


def _require_absolute_path(value: Any, label: str) -> str:
    path = _require_string(value, label)
    pure = PurePosixPath(path)
    if not pure.is_absolute() or ".." in pure.parts or path != str(pure):
        raise FreezeValidationError(f"{label} must be a normalized absolute path")
    return path


def _validate_validator_rows(value: Any, label: str) -> None:
    if not isinstance(value, list):
        raise FreezeValidationError(f"{label} must be an array")
    identities: set[str] = set()
    for index, row in enumerate(value):
        fields = _exact_keys(row, frozenset({"address", "stake"}), f"{label}[{index}]")
        address = _require_hash(fields["address"], f"{label}[{index}].address")
        _require_uint(fields["stake"], f"{label}[{index}].stake", positive=True)
        if address in identities:
            raise FreezeValidationError(f"{label} contains duplicate validator {address}")
        identities.add(address)


def _validate_supervisor_context(value: Any, node_name: str, expected_unit: str) -> None:
    context = _exact_keys(
        value, _SUPERVISOR_CONTEXT_KEYS, f"node {node_name} supervisor_context"
    )
    if context["schema"] != "arc.recovery.supervisor-context.v1":
        raise FreezeValidationError(f"node {node_name} supervisor context schema differs")
    if context["unit"] != expected_unit:
        raise FreezeValidationError(f"node {node_name} supervisor context unit differs")
    _require_hash(
        context["unit_configuration_sha256"],
        f"node {node_name} supervisor unit configuration hash",
    )
    if (
        not isinstance(context["invocation_id"], str)
        or not _INVOCATION_ID_RE.fullmatch(context["invocation_id"])
    ):
        raise FreezeValidationError(f"node {node_name} invocation id is malformed")
    control_group = _require_string(
        context["control_group"], f"node {node_name} supervisor control group"
    )
    if not _CONTROL_GROUP_RE.fullmatch(control_group) or ".." in PurePosixPath(control_group).parts:
        raise FreezeValidationError(f"node {node_name} supervisor control group is malformed")
    hooks = _exact_keys(
        context["lifecycle_hooks"],
        _LIFECYCLE_HOOK_KEYS,
        f"node {node_name} lifecycle hooks",
    )
    if any(not isinstance(item, str) or item not in {"", "none"} for item in hooks.values()):
        raise FreezeValidationError(f"node {node_name} has an active lifecycle hook")
    automatic = _exact_keys(
        context["automatic_lifecycle"],
        _AUTOMATIC_LIFECYCLE_KEYS,
        f"node {node_name} automatic lifecycle",
    )
    if any(not isinstance(item, str) for item in automatic.values()):
        raise FreezeValidationError(f"node {node_name} automatic lifecycle values must be strings")
    if (
        automatic["WatchdogUSec"] != "0"
        or automatic["RuntimeMaxUSec"] != "infinity"
        or automatic["RuntimeRandomizedExtraUSec"] != "0"
        or automatic["StopWhenUnneeded"] != "no"
        or automatic["BindsTo"]
        or automatic["PartOf"]
        or automatic["PropagatesStopTo"]
        or set(automatic["Requires"].split())
        != {"-.mount", "system.slice", "sysinit.target"}
        or automatic["Requisite"]
        or set(automatic["Conflicts"].split()) != {"shutdown.target"}
        or any(
            automatic[field]
            for field in (
                "Upholds",
                "UpheldBy",
                "TriggeredBy",
                "RequiredBy",
                "BoundBy",
                "ConflictedBy",
                "StopPropagatedFrom",
                "ReloadPropagatedFrom",
            )
        )
        or automatic["CanReload"] != "no"
        or automatic["OOMPolicy"] not in {"continue", "stop"}
    ):
        raise FreezeValidationError(
            f"node {node_name} has an unreviewed automatic lifecycle source"
        )
    payloads = context["interpreter_payloads"]
    if not isinstance(payloads, list):
        raise FreezeValidationError(f"node {node_name} interpreter_payloads must be an array")
    payload_paths: set[str] = set()
    for index, item in enumerate(payloads):
        payload = _exact_keys(
            item,
            frozenset({"path", "sha256"}),
            f"node {node_name} interpreter payload[{index}]",
        )
        path = _require_absolute_path(
            payload["path"], f"node {node_name} interpreter payload path"
        )
        _require_hash(payload["sha256"], f"node {node_name} interpreter payload hash")
        if path in payload_paths:
            raise FreezeValidationError(f"node {node_name} repeats an interpreter payload")
        payload_paths.add(path)
    transient = context["allowed_transient_sleep"]
    if transient is not None:
        transient = _exact_keys(
            transient,
            frozenset({"path", "sha256", "argv_policy", "max_seconds"}),
            f"node {node_name} allowed_transient_sleep",
        )
        _require_absolute_path(transient["path"], f"node {node_name} transient sleep path")
        _require_hash(transient["sha256"], f"node {node_name} transient sleep hash")
        if transient["argv_policy"] != "sleep-duration-max-60s-v1":
            raise FreezeValidationError(f"node {node_name} transient sleep policy differs")
        if transient["max_seconds"] != 60:
            raise FreezeValidationError(f"node {node_name} transient sleep maximum differs")
    if (expected_unit == "arc-self-heal.service") is not (transient is not None):
        raise FreezeValidationError(
            f"node {node_name} transient sleep contract differs from supervisor unit"
        )
    if context["term_traps_rejected"] is not True:
        raise FreezeValidationError(f"node {node_name} does not reject supervisor TERM traps")


def _validate_prepare_barrier(
    value: Any,
    *,
    node_name: str,
    supervisor_unit: str,
    supervisor_pid: int,
    supervisor_context: Mapping[str, Any],
    writer_mode: str,
    writer_pid: int,
    writer_cgroup_path: str,
) -> None:
    prepare = _exact_keys(
        value, _PREPARE_BARRIER_KEYS, f"node {node_name} prepare barrier"
    )
    if prepare["schema"] != "arc.recovery.prepare-barrier.v1":
        raise FreezeValidationError(f"node {node_name} prepare barrier schema differs")
    if prepare["selected_unit"] != supervisor_unit:
        raise FreezeValidationError(f"node {node_name} prepare selected unit differs")
    if prepare["selected_main_pid"] != supervisor_pid:
        raise FreezeValidationError(f"node {node_name} prepare selected PID differs")
    if prepare["alternatives_inactive_no_jobs"] is not True:
        raise FreezeValidationError(f"node {node_name} alternatives are not sealed inactive")
    if prepare["alternative_enablement_sync_completed"] is not True:
        raise FreezeValidationError(
            f"node {node_name} alternative enablement changes are not sealed durable"
        )
    if prepare["writer_cgroup_relationship_sealed"] is not True:
        raise FreezeValidationError(f"node {node_name} writer cgroup relationship is not sealed")

    marker = _exact_keys(
        prepare["allow_marker"],
        _PREPARE_MARKER_KEYS,
        f"node {node_name} prepare allow marker",
    )
    if (
        marker["path"] != DEFAULT_ALLOW_MARKER_PATH
        or marker["sha256"] != _ALLOW_MARKER_SHA256
        or marker["mode"] != 0o400
    ):
        raise FreezeValidationError(f"node {node_name} prepare allow marker differs")
    if (
        _require_uint(marker["uid"], f"node {node_name} prepare marker uid") != 0
        or _require_uint(marker["gid"], f"node {node_name} prepare marker gid") != 0
    ):
        raise FreezeValidationError(f"node {node_name} prepare allow marker is not root-owned")
    _require_uint(
        marker["device"], f"node {node_name} prepare allow marker device", positive=True
    )

    barriers = _exact_keys(
        prepare["persistent_start_barriers"],
        _PREPARE_BARRIER_UNIT_SET,
        f"node {node_name} persistent start barriers",
    )
    for unit in _PREPARE_BARRIER_UNITS:
        barrier = _exact_keys(
            barriers[unit],
            _PREPARE_DROPIN_KEYS,
            f"node {node_name} barrier {unit}",
        )
        expected_path = (
            f"/etc/systemd/system/{unit}.d/zzzz-arc-recovery-freeze.conf"
        )
        mode = _require_uint(barrier["mode"], f"node {node_name} barrier {unit} mode")
        if (
            barrier["path"] != expected_path
            or barrier["sha256"] != _CONDITION_ONLY_BARRIER_SHA256
            or mode > 0o777
            or mode & 0o222
        ):
            raise FreezeValidationError(
                f"node {node_name} barrier {unit} is not the exact root-owned condition-only file"
            )
        if (
            _require_uint(barrier["uid"], f"node {node_name} barrier {unit} uid") != 0
            or _require_uint(barrier["gid"], f"node {node_name} barrier {unit} gid") != 0
        ):
            raise FreezeValidationError(
                f"node {node_name} barrier {unit} is not root-owned"
            )

    sources = _exact_keys(
        prepare["merged_unit_sources"],
        _PREPARE_BARRIER_UNIT_SET,
        f"node {node_name} merged unit sources",
    )
    for unit in _PREPARE_BARRIER_UNITS:
        rows = sources[unit]
        if not isinstance(rows, list) or not rows:
            raise FreezeValidationError(
                f"node {node_name} merged source manifest is empty for {unit}"
            )
        seen_paths: set[str] = set()
        expected_path = barriers[unit]["path"]
        expected_seen = False
        for index, item in enumerate(rows):
            row = _exact_keys(
                item,
                _PREPARE_SOURCE_KEYS,
                f"node {node_name} merged source {unit}[{index}]",
            )
            path = _require_absolute_path(
                row["path"], f"node {node_name} merged source {unit}[{index}] path"
            )
            _require_hash(
                row["sha256"], f"node {node_name} merged source {unit}[{index}] hash"
            )
            if path in seen_paths:
                raise FreezeValidationError(
                    f"node {node_name} merged source manifest repeats {path}"
                )
            seen_paths.add(path)
            if path == expected_path:
                if row["sha256"] != _CONDITION_ONLY_BARRIER_SHA256:
                    raise FreezeValidationError(
                        f"node {node_name} merged barrier hash differs for {unit}"
                    )
                expected_seen = True
            pure_path = PurePosixPath(path)
            if (
                pure_path.parent.name == f"{unit}.d"
                and pure_path.name.endswith(".conf")
                and pure_path.name > "zzzz-arc-recovery-freeze.conf"
            ):
                raise FreezeValidationError(
                    f"node {node_name} has a later systemd drop-in after the barrier for {unit}"
                )
        if not expected_seen:
            raise FreezeValidationError(
                f"node {node_name} condition-only barrier is absent from merged sources for {unit}"
            )

    states = _exact_keys(
        prepare["unit_states"],
        _PREPARE_BARRIER_UNIT_SET,
        f"node {node_name} prepare unit states",
    )
    for unit in _PREPARE_BARRIER_UNITS:
        row = _exact_keys(
            states[unit],
            _PREPARE_UNIT_STATE_KEYS,
            f"node {node_name} prepare state {unit}",
        )
        active_state = _require_string(
            row["active_state"], f"node {node_name} {unit} active state"
        )
        sub_state = _require_string(row["sub_state"], f"node {node_name} {unit} sub state")
        main_pid = _require_uint(row["main_pid"], f"node {node_name} {unit} main PID")
        if row["job"] != "0":
            raise FreezeValidationError(f"node {node_name} {unit} has a pending job")
        enablement = _require_string(
            row["enablement"], f"node {node_name} {unit} enablement"
        )
        if unit == supervisor_unit:
            if (active_state != "active" or sub_state != "running"
                    or main_pid != supervisor_pid or enablement != "enabled"):
                raise FreezeValidationError(
                    f"node {node_name} selected supervisor state differs"
                )
        else:
            if active_state not in {"inactive", "failed"} or main_pid != 0:
                raise FreezeValidationError(
                    f"node {node_name} alternative {unit} is not process-free"
                )
            if enablement not in _TERMINAL_ENABLEMENT_STATES:
                raise FreezeValidationError(
                    f"node {node_name} alternative {unit} remains enabled"
                )

    closure = _exact_keys(
        prepare["activation_closure"],
        _PREPARE_BARRIER_UNIT_SET,
        f"node {node_name} activation closure",
    )
    for unit in _PREPARE_BARRIER_UNITS:
        row = _exact_keys(
            closure[unit],
            _PREPARE_CLOSURE_KEYS,
            f"node {node_name} activation closure {unit}",
        )
        if any(not isinstance(item, str) for item in row.values()):
            raise FreezeValidationError(
                f"node {node_name} activation closure {unit} has a non-string value"
            )
        if row["Names"] != unit or row["Id"] != unit or row["Following"]:
            raise FreezeValidationError(
                f"node {node_name} activation closure {unit} has an alias or following target"
            )
        state = states[unit]
        # MainPID is a service-only property.  Older prepared contracts can
        # contain systemd's empty rendering for an inert timer, while the
        # canonical unit-state row represents the same quiescent value as 0.
        # Normalize only this exact timer field; an empty service MainPID still
        # fails closed below.
        closure_main_pid = row["MainPID"]
        if unit == "arc-node-update.timer" and closure_main_pid == "":
            closure_main_pid = "0"
        if (
            row["ActiveState"] != state["active_state"]
            or row["SubState"] != state["sub_state"]
            or closure_main_pid != str(state["main_pid"])
            or (row["Job"] or "0") != state["job"]
        ):
            raise FreezeValidationError(
                f"node {node_name} activation closure state differs for {unit}"
            )
        if unit == supervisor_unit:
            if (row["ControlGroup"] != supervisor_context["control_group"]
                    or "multi-user.target" not in row["WantedBy"].split()):
                raise FreezeValidationError(
                    f"node {node_name} selected ControlGroup differs"
                )
            continue
        for field in _PREPARE_REVERSE_ACTIVATION_FIELDS:
            observed = row[field]
            internal_timer_edge = (
                unit == "arc-node-update.service"
                and field == "TriggeredBy"
                and set(observed.split()) == {"arc-node-update.timer"}
            )
            if observed and not internal_timer_edge:
                raise FreezeValidationError(
                    f"node {node_name} alternative {unit} has reverse activation edge {field}"
                )

    boot = _exact_keys(
        prepare["boot_activation"],
        frozenset({
            "default_target", "default_target_projection", "default_target_symlink",
            "selected_enablement_symlink", "selected_reached_from_multi_user",
            "precommit_reboot_fail_open",
        }),
        f"node {node_name} boot activation",
    )
    default_target = _require_string(
        boot["default_target"], f"node {node_name} default target"
    )
    if default_target not in {"multi-user.target", "graphical.target"}:
        raise FreezeValidationError(f"node {node_name} default target is unsupported")
    projection = _exact_keys(
        boot["default_target_projection"],
        frozenset({"Names", "Id", "Following", "LoadState", "FragmentPath", "Requires", "Wants"}),
        f"node {node_name} default target projection",
    )
    if (any(not isinstance(value, str) for value in projection.values())
            or projection["Id"] != default_target
            or default_target not in projection["Names"].split()
            or projection["Following"] or projection["LoadState"] != "loaded"
            or not PurePosixPath(projection["FragmentPath"]).is_absolute()
            or (default_target == "graphical.target"
                and "multi-user.target" not in projection["Requires"].split())):
        raise FreezeValidationError(f"node {node_name} default target does not reach multi-user")
    default_link = _exact_keys(
        boot["default_target_symlink"],
        frozenset({"path", "target", "device", "inode", "uid", "gid"}),
        f"node {node_name} default target symlink",
    )
    default_path = _require_absolute_path(default_link["path"], f"node {node_name} default symlink path")
    if (default_path not in {
            "/etc/systemd/system/default.target",
            "/usr/local/lib/systemd/system/default.target",
            "/usr/lib/systemd/system/default.target",
        } or not isinstance(default_link["target"], str) or not default_link["target"]
            or _require_uint(default_link["device"], f"node {node_name} default link device") <= 0
            or _require_uint(default_link["inode"], f"node {node_name} default link inode") <= 0
            or default_link["uid"] != 0 or default_link["gid"] != 0):
        raise FreezeValidationError(f"node {node_name} default target symlink differs")
    selected_link = _exact_keys(
        boot["selected_enablement_symlink"],
        frozenset({
            "path", "target", "device", "inode", "uid", "gid",
            "resolved_path", "resolved_sha256",
        }),
        f"node {node_name} selected enablement symlink",
    )
    if (_require_absolute_path(selected_link["path"], f"node {node_name} selected link path")
            != f"/etc/systemd/system/multi-user.target.wants/{supervisor_unit}"
            or not isinstance(selected_link["target"], str) or not selected_link["target"]
            or _require_uint(selected_link["device"], f"node {node_name} selected link device") <= 0
            or _require_uint(selected_link["inode"], f"node {node_name} selected link inode") <= 0
            or selected_link["uid"] != 0 or selected_link["gid"] != 0
            or not _require_absolute_path(
                selected_link["resolved_path"], f"node {node_name} selected resolved unit path"
            )
            or not _require_hash(
                selected_link["resolved_sha256"], f"node {node_name} selected resolved unit hash"
            )
            or boot["selected_reached_from_multi_user"] is not True
            or boot["precommit_reboot_fail_open"] is not True):
        raise FreezeValidationError(f"node {node_name} selected boot activation differs")

    supervisor_cgroup = supervisor_context["control_group"]
    if writer_mode == "systemd-unit":
        if writer_cgroup_path != supervisor_cgroup:
            raise FreezeValidationError(
                f"node {node_name} systemd writer is outside its selected supervisor cgroup"
            )
    elif writer_mode == "detached-root-session":
        if (
            supervisor_unit != "arc-self-heal.service"
            or writer_pid == supervisor_pid
            or not re.fullmatch(
                r"/user\.slice/user-0\.slice/session-[1-9][0-9]*\.scope",
                writer_cgroup_path,
            )
            or writer_cgroup_path == supervisor_cgroup
            or writer_cgroup_path.startswith(supervisor_cgroup.rstrip("/") + "/")
            or supervisor_cgroup.startswith(writer_cgroup_path.rstrip("/") + "/")
        ):
            raise FreezeValidationError(
                f"node {node_name} detached writer relationship is not the exact root-session shape"
            )
    else:  # The caller validates this first; retain a local fail-closed guard.
        raise FreezeValidationError(f"node {node_name} supervision mode is unreviewed")


def _validate_node(row: Any, expected_name: str) -> FreezeNode:
    node = _exact_keys(row, _NODE_KEYS, f"node {expected_name}")
    name = _require_string(node["name"], f"node {expected_name} name")
    if name != expected_name or not _NAME_RE.fullmatch(name):
        raise FreezeValidationError(f"node name {name!r} differs from expected {expected_name!r}")
    host = _require_string(node["host"], f"node {name} host")
    if not _HOST_RE.fullmatch(host):
        raise FreezeValidationError(f"node {name} host is malformed")
    boot_id = _require_uuid(node["boot_id"], f"node {name} boot id")
    writer_pid = _require_uint(node["writer_pid"], f"node {name} writer pid", positive=True)
    writer_start = _require_uint(
        node["writer_start_ticks"], f"node {name} writer start ticks", positive=True
    )
    supervisor_pid = _require_uint(
        node["supervisor_main_pid"], f"node {name} supervisor pid", positive=True
    )
    supervisor_start = _require_uint(
        node["supervisor_start_ticks"],
        f"node {name} supervisor start ticks",
        positive=True,
    )
    for field in (
        "writer_cgroup_sha256",
        "supervisor_executable_sha256",
        "supervisor_argv_sha256",
        "supervisor_context_sha256",
        "executable_sha256",
        "argv_sha256",
        "model_sha256",
    ):
        _require_hash(node[field], f"node {name} {field}")
    writer_mode = _require_string(
        node["writer_supervision_mode"], f"node {name} supervision mode"
    )
    if writer_mode not in {"systemd-unit", "detached-root-session"}:
        raise FreezeValidationError(f"node {name} supervision mode differs")
    writer_cgroup_path = _require_absolute_path(
        node["writer_cgroup_path"], f"node {name} writer cgroup path"
    )
    if writer_cgroup_path == "/" or not _CONTROL_GROUP_RE.fullmatch(writer_cgroup_path):
        raise FreezeValidationError(f"node {name} writer cgroup path is unsafe")
    writer_cgroup_device = _require_uint(
        node["writer_cgroup_device"],
        f"node {name} writer cgroup device",
        positive=True,
    )
    writer_cgroup_inode = _require_uint(
        node["writer_cgroup_inode"],
        f"node {name} writer cgroup inode",
        positive=True,
    )
    supervisor_unit = _require_string(node["supervisor_unit"], f"node {name} supervisor unit")
    if supervisor_unit not in {"arc-node.service", "arc-self-heal.service"}:
        raise FreezeValidationError(f"node {name} supervisor unit is unreviewed")
    for field in (
        "supervisor_executable_path",
        "executable_path",
        "data_dir",
        "model_path",
    ):
        _require_absolute_path(node[field], f"node {name} {field}")
    _validate_supervisor_context(node["supervisor_context"], name, supervisor_unit)
    context_sha = canonical_sha256(node["supervisor_context"])
    if context_sha != node["supervisor_context_sha256"]:
        raise FreezeValidationError(f"node {name} supervisor context hash differs")
    _validate_prepare_barrier(
        node["prepare_barrier"],
        node_name=name,
        supervisor_unit=supervisor_unit,
        supervisor_pid=supervisor_pid,
        supervisor_context=node["supervisor_context"],
        writer_mode=writer_mode,
        writer_pid=writer_pid,
        writer_cgroup_path=writer_cgroup_path,
    )
    if supervisor_pid == writer_pid:
        if (
            supervisor_start != writer_start
            or node["supervisor_executable_path"] != node["executable_path"]
            or node["supervisor_executable_sha256"] != node["executable_sha256"]
            or node["supervisor_argv_sha256"] != node["argv_sha256"]
        ):
            raise FreezeValidationError(f"node {name} shared supervisor identity conflicts")
    if supervisor_unit == "arc-node.service" and (
        writer_mode != "systemd-unit" or supervisor_pid != writer_pid
    ):
        raise FreezeValidationError(f"node {name} arc-node.service must directly own the writer")
    _require_uint(node["model_size_bytes"], f"node {name} model size", positive=True)
    ranges = node["shard_ranges"]
    if not isinstance(ranges, list) or not ranges:
        raise FreezeValidationError(f"node {name} shard ranges must be a non-empty array")
    for index, pair in enumerate(ranges):
        if (
            not isinstance(pair, list)
            or len(pair) != 2
            or isinstance(pair[0], bool)
            or isinstance(pair[1], bool)
            or not isinstance(pair[0], int)
            or not isinstance(pair[1], int)
            or pair[0] < 0
            or pair[0] >= pair[1]
        ):
            raise FreezeValidationError(f"node {name} shard range {index} is malformed")
    for field in (
        "data_device",
        "data_bytes",
        "data_files",
        "capture_device",
        "available_bytes",
        "available_inodes",
        "required_free_bytes",
        "required_free_inodes",
        "new_v3_headroom_bytes",
        "max_binding_temporary_bytes",
        "archive_stream_temporary_bytes",
    ):
        _require_uint(
            node[field],
            f"node {name} {field}",
            positive=field in {"data_device", "data_bytes", "capture_device"},
        )
    if node["available_bytes"] < node["required_free_bytes"]:
        raise FreezeValidationError(f"node {name} has insufficient byte headroom")
    if node["available_inodes"] < node["required_free_inodes"]:
        raise FreezeValidationError(f"node {name} has insufficient inode headroom")
    validator = _require_hash(node["validator_address"], f"node {name} validator address")
    stake = _require_uint(node["stake"], f"node {name} stake", positive=True)
    rpc_origin = _require_string(node["rpc_origin"], f"node {name} RPC origin")
    if not re.fullmatch(r"http://127\.0\.0\.1:[1-9][0-9]{0,4}", rpc_origin):
        raise FreezeValidationError(f"node {name} RPC origin is not loopback HTTP")
    _validate_validator_rows(
        node["observed_positive_validators"], f"node {name} observed validators"
    )
    if node["observed_validator_error"] is not None:
        _require_string(node["observed_validator_error"], f"node {name} validator error")
    row_bytes = canonical_json_bytes(node)
    return FreezeNode(
        name=name,
        host=host,
        boot_id=boot_id,
        validator_address=validator,
        stake=stake,
        writer_pid=writer_pid,
        writer_start_ticks=writer_start,
        writer_supervision_mode=writer_mode,
        writer_cgroup_path=writer_cgroup_path,
        writer_cgroup_device=writer_cgroup_device,
        writer_cgroup_inode=writer_cgroup_inode,
        supervisor_main_pid=supervisor_pid,
        supervisor_start_ticks=supervisor_start,
        canonical_bytes=row_bytes,
        sha256=hashlib.sha256(row_bytes).hexdigest(),
    )


def validate_pinned_freeze_plan(
    raw: bytes,
    expected_sha256: str,
    *,
    expected_node_names: Sequence[str] = ARC_NODE_ORDER,
    expected_sentinels: Sequence[str] = ARC_SENTINEL_ORDER,
) -> PinnedFreezePlan:
    """Validate a complete canonical v5 plan pinned by an out-of-band digest."""

    expected_sha256 = _require_hash(expected_sha256, "expected freeze-plan digest")
    observed_sha256 = hashlib.sha256(raw).hexdigest()
    if observed_sha256 != expected_sha256:
        raise FreezeValidationError(
            f"freeze plan digest differs: expected {expected_sha256}, observed {observed_sha256}"
        )
    value = parse_canonical_json(raw, label="freeze plan")
    plan = _exact_keys(value, _PLAN_KEYS, "freeze plan")
    if plan["schema"] != FREEZE_PLAN_SCHEMA:
        raise FreezeValidationError(f"freeze plan schema must be {FREEZE_PLAN_SCHEMA}")
    window = _require_string(plan["window"], "freeze plan window")
    if not _WINDOW_RE.fullmatch(window):
        raise FreezeValidationError("freeze plan window is malformed")
    created_at = _require_timestamp(plan["created_at"], "freeze plan created_at")
    expected_names = tuple(expected_node_names)
    if (
        not expected_names
        or len(set(expected_names)) != len(expected_names)
        or any(not isinstance(name, str) or not _NAME_RE.fullmatch(name) for name in expected_names)
    ):
        raise ValueError("expected_node_names must contain unique canonical node names")
    sentinels = plan["sentinels"]
    if not isinstance(sentinels, list) or tuple(sentinels) != tuple(expected_sentinels):
        raise FreezeValidationError("freeze plan sentinel order differs")
    if len(set(sentinels)) != len(sentinels) or not set(sentinels).issubset(expected_names):
        raise FreezeValidationError("freeze plan sentinels are not unique fleet members")
    raw_nodes = plan["nodes"]
    if not isinstance(raw_nodes, list) or len(raw_nodes) != len(expected_names):
        raise FreezeValidationError("freeze plan node count differs from reviewed topology")
    nodes = tuple(_validate_node(row, name) for row, name in zip(raw_nodes, expected_names))
    for label, items in (
        ("node name", (node.name for node in nodes)),
        ("host", (node.host for node in nodes)),
        ("validator", (node.validator_address for node in nodes)),
        (
            "writer process identity",
            ((node.host, node.boot_id, node.writer_pid, node.writer_start_ticks) for node in nodes),
        ),
    ):
        materialized = tuple(items)
        if len(set(materialized)) != len(materialized):
            raise FreezeValidationError(f"freeze plan contains a duplicate {label}")
    for field in (
        "remote_helper_sha256",
        "orchestrator_sha256",
        "rollout_tool_sha256",
        "rollout_schema_sha256",
        "operator_python_sha256",
        "legacy_validator_set_sha256",
        "writer_contracts_sha256",
    ):
        _require_hash(plan[field], f"freeze plan {field}")
    operator_python_path = _require_absolute_path(
        plan["operator_python_path"], "freeze plan operator_python_path"
    )
    if re.fullmatch(r"/usr/bin/python3(?:\.[0-9]+)?", operator_python_path) is None:
        raise FreezeValidationError("freeze plan operator Python escaped /usr/bin/python3[.VERSION]")
    if not isinstance(plan["source_commit"], str) or not _COMMIT_RE.fullmatch(plan["source_commit"]):
        raise FreezeValidationError("freeze plan source_commit is malformed")
    drive = _exact_keys(plan["drive_prefreeze"], _DRIVE_KEYS, "Drive prefreeze")
    for field in (
        "gate_sha256",
        "remote_root_sha256",
        "oauth_client_id_sha256",
        "account_sha256",
    ):
        _require_hash(drive[field], f"Drive prefreeze {field}")
    remote_root = _require_string(drive["remote_root"], "Drive remote root")
    if remote_root.startswith("arc-drive:") or ":" not in remote_root:
        raise FreezeValidationError("Drive remote root is legacy or malformed")
    if hashlib.sha256(remote_root.encode("utf-8")).hexdigest() != drive["remote_root_sha256"]:
        raise FreezeValidationError("Drive remote-root hash differs")
    budget = _require_uint(
        drive["daily_upload_budget_bytes"], "Drive daily upload budget", positive=True
    )
    if budget > 700_000_000_000:
        raise FreezeValidationError(
            "Drive daily upload budget exceeds the 700 GB decimal operational ceiling"
        )
    if drive["dedicated_no_other_upload_writers_attested"] is not True:
        raise FreezeValidationError("Drive uploader exclusivity is not attested")
    if 3 * sum(node.value()["data_bytes"] for node in nodes) + 32 * 1024**3 > budget:
        raise FreezeValidationError("Drive archive reservation exceeds the sealed budget")
    if 3 * max(node.value()["data_bytes"] for node in nodes) + 4 * 1024**3 > 5_000_000_000_000:
        raise FreezeValidationError("largest archive object reservation exceeds Drive limit")
    proof = _exact_keys(plan["quorum_proof"], _QUORUM_KEYS, "quorum proof")
    total = _require_uint(proof["source_total_stake"], "source total stake", positive=True)
    quorum = _require_uint(proof["source_quorum_stake"], "source quorum stake", positive=True)
    controlled = _require_uint(
        proof["controlled_writer_stake"], "controlled writer stake", positive=True
    )
    maximum = _require_uint(
        proof["maximum_source_stake_after_controlled_stop"],
        "maximum source stake after controlled stop",
    )
    if controlled != sum(node.stake for node in nodes):
        raise FreezeValidationError("controlled writer stake differs from node contracts")
    if quorum != total * 2 // 3 + 1 or maximum != total - controlled:
        raise FreezeValidationError("quorum arithmetic differs")
    if controlled * 3 <= total or maximum >= quorum:
        raise FreezeValidationError("controlled stop does not remove sealed-source quorum")
    if proof["controlled_quorum_unavailable_after_all_stops"] is not True:
        raise FreezeValidationError("quorum-unavailable conclusion is absent")
    if proof["global_legacy_halt_claimed"] is not False:
        raise FreezeValidationError("plan overclaims a global legacy halt")
    _validate_validator_rows(proof["external_source_validators"], "external validators")
    _validate_validator_rows(
        proof["untrusted_external_observations"], "untrusted validator observations"
    )
    if not isinstance(proof["dynamic_membership_disagrees"], bool):
        raise FreezeValidationError("dynamic membership disagreement must be boolean")
    external = proof["external_source_validators"]
    if sum(row["stake"] for row in external) != maximum:
        raise FreezeValidationError("external validator stake differs from remaining stake")
    controlled_addresses = {node.validator_address for node in nodes}
    if any(row["address"] in controlled_addresses for row in external):
        raise FreezeValidationError("external validators overlap controlled writers")
    return PinnedFreezePlan(
        sha256=observed_sha256,
        window=window,
        created_at=created_at,
        sentinels=tuple(sentinels),
        nodes=nodes,
        canonical_bytes=raw,
    )


def load_pinned_freeze_plan(
    path: str | os.PathLike[str],
    expected_sha256: str,
    *,
    expected_uid: int = 0,
    expected_node_names: Sequence[str] = ARC_NODE_ORDER,
    expected_sentinels: Sequence[str] = ARC_SENTINEL_ORDER,
) -> PinnedFreezePlan:
    raw = read_regular_nofollow(
        path, expected_uid=expected_uid, label="sealed freeze plan"
    )
    return validate_pinned_freeze_plan(
        raw,
        expected_sha256,
        expected_node_names=expected_node_names,
        expected_sentinels=expected_sentinels,
    )


def _validate_cgroup_identity(value: Any, label: str) -> CgroupIdentity:
    row = _exact_keys(value, _CGROUP_IDENTITY_KEYS, label)
    role = row["role"]
    if role not in {"supervisor", "writer"}:
        raise FreezeValidationError(f"{label} role is unreviewed")
    path = _require_absolute_path(row["path"], f"{label} path")
    if not _CONTROL_GROUP_RE.fullmatch(path):
        raise FreezeValidationError(f"{label} path is malformed")
    return CgroupIdentity(
        role=role,
        path=path,
        device=_require_uint(row["device"], f"{label} device", positive=True),
        inode=_require_uint(row["inode"], f"{label} inode", positive=True),
    )


def _validate_cgroups(value: Any, label: str) -> tuple[CgroupIdentity, ...]:
    if not isinstance(value, list) or len(value) != 2:
        raise FreezeValidationError(f"{label} must have supervisor and writer rows")
    rows = tuple(_validate_cgroup_identity(row, f"{label}[{index}]") for index, row in enumerate(value))
    if tuple(row.role for row in rows) != ("supervisor", "writer"):
        raise FreezeValidationError(f"{label} role order differs")
    if rows[0].path == rows[1].path and (
        rows[0].device != rows[1].device or rows[0].inode != rows[1].inode
    ):
        raise FreezeValidationError(f"{label} aliases one path with conflicting inode identity")
    return rows


def make_prepare_receipt(
    plan: PinnedFreezePlan,
    node_name: str,
    *,
    sealed_boot_id: str,
    cgroups: Sequence[CgroupIdentity],
    prepared_at: str,
    allow_marker_path: str = DEFAULT_ALLOW_MARKER_PATH,
) -> dict[str, Any]:
    node = plan.node(node_name)
    if sealed_boot_id != node.boot_id:
        raise FreezeValidationError("prepare boot differs from the pinned node boot")
    value = {
        "schema": PREPARE_RECEIPT_SCHEMA,
        "freeze_plan_sha256": plan.sha256,
        "node": node.name,
        "host": node.host,
        "node_contract_sha256": node.sha256,
        "sealed_boot_id": sealed_boot_id,
        "allow_marker_path": _require_absolute_path(
            allow_marker_path, "allow marker path"
        ),
        "allow_marker_present": True,
        "cgroups": [cgroup.value() for cgroup in cgroups],
        "prepared_at": prepared_at,
    }
    validate_prepare_receipt(value, plan=plan)
    return value


def validate_prepare_receipt(
    value: Any, *, plan: PinnedFreezePlan | None = None
) -> Mapping[str, Any]:
    receipt = _exact_keys(value, _PREPARE_KEYS, "prepare receipt")
    if receipt["schema"] != PREPARE_RECEIPT_SCHEMA:
        raise FreezeValidationError("prepare receipt schema differs")
    _require_hash(receipt["freeze_plan_sha256"], "prepare freeze-plan hash")
    name = _require_string(receipt["node"], "prepare node")
    _require_string(receipt["host"], "prepare host")
    _require_hash(receipt["node_contract_sha256"], "prepare node-contract hash")
    _require_uuid(receipt["sealed_boot_id"], "prepare sealed boot id")
    _require_absolute_path(receipt["allow_marker_path"], "prepare allow marker path")
    if receipt["allow_marker_path"] != DEFAULT_ALLOW_MARKER_PATH:
        raise FreezeValidationError("prepare allow marker path differs from the v5 contract")
    if receipt["allow_marker_present"] is not True:
        raise FreezeValidationError("prepare must be fail-open with the allow marker present")
    receipt_cgroups = _validate_cgroups(receipt["cgroups"], "prepare cgroups")
    _require_timestamp(receipt["prepared_at"], "prepare timestamp")
    if plan is not None:
        if receipt["freeze_plan_sha256"] != plan.sha256:
            raise FreezeValidationError("prepare receipt belongs to another freeze plan")
        node = plan.node(name)
        if (
            receipt["host"] != node.host
            or receipt["node_contract_sha256"] != node.sha256
            or receipt["sealed_boot_id"] != node.boot_id
        ):
            raise FreezeValidationError("prepare receipt differs from pinned node identity")
        supervisor_cgroup, writer_cgroup = receipt_cgroups
        node_value = node.value()
        if (
            writer_cgroup.path != node.writer_cgroup_path
            or writer_cgroup.device != node.writer_cgroup_device
            or writer_cgroup.inode != node.writer_cgroup_inode
        ):
            raise FreezeValidationError("prepare writer cgroup differs from the pinned inode")
        if supervisor_cgroup.path != node_value["supervisor_context"]["control_group"]:
            raise FreezeValidationError("prepare supervisor cgroup differs from the pinned unit")
        if node.writer_supervision_mode == "systemd-unit" and (
            supervisor_cgroup.path != writer_cgroup.path
            or supervisor_cgroup.device != writer_cgroup.device
            or supervisor_cgroup.inode != writer_cgroup.inode
        ):
            raise FreezeValidationError("systemd writer prepare cgroups are not the same inode")
        if node.writer_supervision_mode == "detached-root-session" and (
            supervisor_cgroup.path == writer_cgroup.path
            or supervisor_cgroup.path.startswith(writer_cgroup.path.rstrip("/") + "/")
            or writer_cgroup.path.startswith(supervisor_cgroup.path.rstrip("/") + "/")
        ):
            raise FreezeValidationError("detached prepare cgroups are not disjoint")
    return receipt


def make_barrier_arm_event(
    prepare_receipt: Mapping[str, Any], *, armed_at: str
) -> dict[str, Any]:
    receipt = validate_prepare_receipt(prepare_receipt)
    value = {
        "schema": BARRIER_ARM_SCHEMA,
        "freeze_plan_sha256": receipt["freeze_plan_sha256"],
        "node": receipt["node"],
        "prepare_receipt_sha256": canonical_sha256(receipt),
        "sealed_boot_id": receipt["sealed_boot_id"],
        "allow_marker_path": receipt["allow_marker_path"],
        "allow_marker_observed_present": True,
        "armed_at": armed_at,
    }
    validate_barrier_arm_event(value, prepare_receipt=receipt)
    return value


def validate_barrier_arm_event(
    value: Any, *, prepare_receipt: Mapping[str, Any] | None = None
) -> Mapping[str, Any]:
    arm = _exact_keys(value, _BARRIER_ARM_KEYS, "barrier arm")
    if arm["schema"] != BARRIER_ARM_SCHEMA:
        raise FreezeValidationError("barrier arm schema differs")
    _require_hash(arm["freeze_plan_sha256"], "barrier arm freeze-plan hash")
    _require_string(arm["node"], "barrier arm node")
    _require_hash(arm["prepare_receipt_sha256"], "barrier arm prepare hash")
    _require_uuid(arm["sealed_boot_id"], "barrier arm sealed boot id")
    _require_absolute_path(arm["allow_marker_path"], "barrier arm allow marker")
    if arm["allow_marker_observed_present"] is not True:
        raise FreezeValidationError("barrier cannot arm after the allow marker disappeared")
    _require_timestamp(arm["armed_at"], "barrier arm timestamp")
    if prepare_receipt is not None:
        receipt = validate_prepare_receipt(prepare_receipt)
        if (
            arm["freeze_plan_sha256"] != receipt["freeze_plan_sha256"]
            or arm["node"] != receipt["node"]
            or arm["prepare_receipt_sha256"] != canonical_sha256(receipt)
            or arm["sealed_boot_id"] != receipt["sealed_boot_id"]
            or arm["allow_marker_path"] != receipt["allow_marker_path"]
        ):
            raise FreezeValidationError("barrier arm differs from prepare receipt")
    return arm


def infer_barrier_state(
    *,
    prepare_receipt: Mapping[str, Any],
    arm_event: Mapping[str, Any] | None,
    observed_boot_id: str,
    allow_marker_exists: bool,
    durable_unlink: DurableUnlinkEvidence | None = None,
) -> BarrierInference:
    """Infer the barrier state from marker truth, never from a commit claim.

    Same-boot commit requires evidence that the allow marker was unlinked and
    its parent directory fsynced.  After a reboot, marker absence itself proves
    that the unlink survived durable storage.  Marker absence without either
    proof is ambiguous and fails closed.
    """

    receipt = validate_prepare_receipt(prepare_receipt)
    observed_boot_id = _require_uuid(observed_boot_id, "observed boot id")
    if not isinstance(allow_marker_exists, bool):
        raise FreezeValidationError("allow_marker_exists must be boolean")
    if arm_event is None:
        if not allow_marker_exists:
            raise FreezeValidationError("allow marker is absent without a durable arm event")
        return BarrierInference(
            state=BarrierState.UNARMED,
            durability_basis=None,
            sealed_boot_id=receipt["sealed_boot_id"],
            observed_boot_id=observed_boot_id,
            allow_marker_path=receipt["allow_marker_path"],
            unlink_parent_fsynced=False,
        )
    arm = validate_barrier_arm_event(arm_event, prepare_receipt=receipt)
    if allow_marker_exists:
        if durable_unlink is not None:
            raise FreezeValidationError("unlink evidence conflicts with a present allow marker")
        return BarrierInference(
            state=BarrierState.ARMED,
            durability_basis=None,
            sealed_boot_id=arm["sealed_boot_id"],
            observed_boot_id=observed_boot_id,
            allow_marker_path=arm["allow_marker_path"],
            unlink_parent_fsynced=False,
        )
    if durable_unlink is not None:
        if not isinstance(durable_unlink, DurableUnlinkEvidence):
            raise FreezeValidationError("durable unlink evidence is malformed")
        if (
            durable_unlink.path != arm["allow_marker_path"]
            or durable_unlink.marker_absent is not True
        ):
            raise FreezeValidationError("durable unlink evidence conflicts with marker truth")
    if observed_boot_id != arm["sealed_boot_id"]:
        basis = "absence-survived-reboot"
        parent_fsynced = bool(
            durable_unlink is not None and durable_unlink.parent_directory_fsynced
        )
    else:
        if (
            durable_unlink is None
            or durable_unlink.path != arm["allow_marker_path"]
            or durable_unlink.marker_absent is not True
            or durable_unlink.parent_directory_fsynced is not True
        ):
            raise FreezeValidationError(
                "same-boot marker absence lacks durable unlink and parent-fsync evidence"
            )
        basis = "unlink-and-parent-fsync"
        parent_fsynced = True
    return BarrierInference(
        state=BarrierState.COMMITTED,
        durability_basis=basis,
        sealed_boot_id=arm["sealed_boot_id"],
        observed_boot_id=observed_boot_id,
        allow_marker_path=arm["allow_marker_path"],
        unlink_parent_fsynced=parent_fsynced,
    )


def make_barrier_commit_event(
    arm_event: Mapping[str, Any],
    inference: BarrierInference,
    *,
    committed_at: str,
) -> dict[str, Any]:
    arm = validate_barrier_arm_event(arm_event)
    if inference.state is not BarrierState.COMMITTED:
        raise FreezeValidationError("barrier commit event requires a committed inference")
    if (
        inference.sealed_boot_id != arm["sealed_boot_id"]
        or inference.allow_marker_path != arm["allow_marker_path"]
        or inference.durability_basis
        not in {"unlink-and-parent-fsync", "absence-survived-reboot"}
    ):
        raise FreezeValidationError("barrier inference differs from arm event")
    value = {
        "schema": BARRIER_COMMIT_SCHEMA,
        "freeze_plan_sha256": arm["freeze_plan_sha256"],
        "node": arm["node"],
        "barrier_arm_sha256": canonical_sha256(arm),
        "sealed_boot_id": arm["sealed_boot_id"],
        "observed_boot_id": inference.observed_boot_id,
        "allow_marker_path": arm["allow_marker_path"],
        "allow_marker_absent": True,
        "unlink_parent_fsynced": inference.unlink_parent_fsynced,
        "durability_basis": inference.durability_basis,
        "committed_at": committed_at,
    }
    validate_barrier_commit_event(value, arm_event=arm)
    return value


def validate_barrier_commit_event(
    value: Any, *, arm_event: Mapping[str, Any] | None = None
) -> Mapping[str, Any]:
    commit = _exact_keys(value, _BARRIER_COMMIT_KEYS, "barrier commit")
    if commit["schema"] != BARRIER_COMMIT_SCHEMA:
        raise FreezeValidationError("barrier commit schema differs")
    _require_hash(commit["freeze_plan_sha256"], "barrier commit freeze-plan hash")
    _require_string(commit["node"], "barrier commit node")
    _require_hash(commit["barrier_arm_sha256"], "barrier commit arm hash")
    sealed = _require_uuid(commit["sealed_boot_id"], "barrier commit sealed boot")
    observed = _require_uuid(commit["observed_boot_id"], "barrier commit observed boot")
    _require_absolute_path(commit["allow_marker_path"], "barrier commit allow marker")
    if commit["allow_marker_absent"] is not True:
        raise FreezeValidationError("barrier commit requires an absent allow marker")
    basis = commit["durability_basis"]
    if basis == "unlink-and-parent-fsync":
        if commit["unlink_parent_fsynced"] is not True or observed != sealed:
            raise FreezeValidationError("same-boot barrier durability evidence conflicts")
    elif basis == "absence-survived-reboot":
        if not isinstance(commit["unlink_parent_fsynced"], bool) or observed == sealed:
            raise FreezeValidationError("reboot barrier durability evidence conflicts")
    else:
        raise FreezeValidationError("barrier durability basis is unreviewed")
    _require_timestamp(commit["committed_at"], "barrier commit timestamp")
    if arm_event is not None:
        arm = validate_barrier_arm_event(arm_event)
        if (
            commit["freeze_plan_sha256"] != arm["freeze_plan_sha256"]
            or commit["node"] != arm["node"]
            or commit["barrier_arm_sha256"] != canonical_sha256(arm)
            or commit["sealed_boot_id"] != arm["sealed_boot_id"]
            or commit["allow_marker_path"] != arm["allow_marker_path"]
        ):
            raise FreezeValidationError("barrier commit differs from arm event")
    return commit


def _validate_event_identity(event: Mapping[str, Any], label: str) -> None:
    _require_hash(event["freeze_plan_sha256"], f"{label} freeze-plan hash")
    _require_string(event["node"], f"{label} node")
    _require_uuid(event["sealed_boot_id"], f"{label} sealed boot")
    _require_timestamp(event["occurred_at"], f"{label} timestamp")


def make_cgroup_freeze_event(
    *,
    freeze_plan_sha256: str,
    node: str,
    sealed_boot_id: str,
    cgroup: CgroupIdentity,
    phase: str,
    occurred_at: str,
) -> dict[str, Any]:
    value = {
        "schema": CGROUP_FREEZE_EVENT_SCHEMA,
        "freeze_plan_sha256": freeze_plan_sha256,
        "node": node,
        "sealed_boot_id": sealed_boot_id,
        "role": cgroup.role,
        "cgroup_path": cgroup.path,
        "cgroup_device": cgroup.device,
        "cgroup_inode": cgroup.inode,
        "phase": phase,
        "cgroup_freeze_value": 1,
        "observed_frozen": phase == "confirmed",
        "occurred_at": occurred_at,
    }
    validate_cgroup_freeze_event(value)
    return value


def validate_cgroup_freeze_event(value: Any) -> Mapping[str, Any]:
    event = _exact_keys(value, _CGROUP_EVENT_KEYS, "cgroup freeze event")
    if event["schema"] != CGROUP_FREEZE_EVENT_SCHEMA:
        raise FreezeValidationError("cgroup freeze event schema differs")
    _validate_event_identity(event, "cgroup freeze event")
    _validate_cgroup_identity(
        {
            "role": event["role"],
            "path": event["cgroup_path"],
            "device": event["cgroup_device"],
            "inode": event["cgroup_inode"],
        },
        "cgroup freeze identity",
    )
    if event["phase"] not in {"intent", "confirmed"}:
        raise FreezeValidationError("cgroup freeze phase is unreviewed")
    if event["cgroup_freeze_value"] != 1:
        raise FreezeValidationError("cgroup freeze event must write/read value 1")
    if event["observed_frozen"] is not (event["phase"] == "confirmed"):
        raise FreezeValidationError("cgroup freeze observation conflicts with phase")
    return event


def _validate_target_identity(target: TargetIdentity, label: str) -> None:
    if target.role not in {"supervisor", "writer"}:
        raise FreezeValidationError(f"{label} role is unreviewed")
    _require_uint(target.pid, f"{label} pid", positive=True)
    _require_uint(target.start_ticks, f"{label} start ticks", positive=True)


def make_pidfd_term_event(
    *,
    freeze_plan_sha256: str,
    node: str,
    sealed_boot_id: str,
    target: TargetIdentity,
    phase: str,
    occurred_at: str,
) -> dict[str, Any]:
    state = "indeterminate" if phase == "intent" else "confirmed"
    value = {
        "schema": PIDFD_TERM_EVENT_SCHEMA,
        "freeze_plan_sha256": freeze_plan_sha256,
        "node": node,
        "sealed_boot_id": sealed_boot_id,
        "target_role": target.role,
        "pid": target.pid,
        "start_ticks": target.start_ticks,
        "phase": phase,
        "signal": "SIGTERM",
        "delivery": "pidfd",
        "cgroups_frozen": True,
        "term_state": state,
        "recovery_sigkill_sent": False,
        "exit_cause": "unknown",
        "occurred_at": occurred_at,
    }
    validate_pidfd_term_event(value)
    return value


def validate_pidfd_term_event(value: Any) -> Mapping[str, Any]:
    event = _exact_keys(value, _TERM_EVENT_KEYS, "pidfd TERM event")
    if event["schema"] != PIDFD_TERM_EVENT_SCHEMA:
        raise FreezeValidationError("pidfd TERM event schema differs")
    _validate_event_identity(event, "pidfd TERM event")
    target = TargetIdentity(
        role=event["target_role"], pid=event["pid"], start_ticks=event["start_ticks"]
    )
    _validate_target_identity(target, "pidfd TERM target")
    if event["phase"] not in {"intent", "sent", "pending-observed"}:
        raise FreezeValidationError("pidfd TERM phase is unreviewed")
    expected_state = "indeterminate" if event["phase"] == "intent" else "confirmed"
    if (
        event["signal"] != "SIGTERM"
        or event["delivery"] != "pidfd"
        or event["cgroups_frozen"] is not True
        or event["term_state"] != expected_state
        or event["recovery_sigkill_sent"] is not False
        or event["exit_cause"] != "unknown"
    ):
        raise FreezeValidationError("pidfd TERM event safety fields conflict")
    return event


def make_cgroup_thaw_event(
    *,
    freeze_plan_sha256: str,
    node: str,
    sealed_boot_id: str,
    cgroup: CgroupIdentity,
    phase: str,
    occurred_at: str,
) -> dict[str, Any]:
    value = {
        "schema": CGROUP_THAW_EVENT_SCHEMA,
        "freeze_plan_sha256": freeze_plan_sha256,
        "node": node,
        "sealed_boot_id": sealed_boot_id,
        "role": cgroup.role,
        "cgroup_path": cgroup.path,
        "cgroup_device": cgroup.device,
        "cgroup_inode": cgroup.inode,
        "phase": phase,
        "cgroup_freeze_value": 0,
        "observed_frozen": phase != "confirmed",
        "no_signal_replayed_after_own_stage_thaw_intent": True,
        "occurred_at": occurred_at,
    }
    validate_cgroup_thaw_event(value)
    return value


def validate_cgroup_thaw_event(value: Any) -> Mapping[str, Any]:
    event = _exact_keys(value, _THAW_EVENT_KEYS, "cgroup thaw event")
    if event["schema"] != CGROUP_THAW_EVENT_SCHEMA:
        raise FreezeValidationError("cgroup thaw event schema differs")
    _validate_event_identity(event, "cgroup thaw event")
    _validate_cgroup_identity(
        {
            "role": event["role"],
            "path": event["cgroup_path"],
            "device": event["cgroup_device"],
            "inode": event["cgroup_inode"],
        },
        "cgroup thaw identity",
    )
    if event["phase"] not in {"intent", "confirmed"}:
        raise FreezeValidationError("cgroup thaw phase is unreviewed")
    if event["cgroup_freeze_value"] != 0:
        raise FreezeValidationError("cgroup thaw event must write/read value 0")
    if event["observed_frozen"] is not (event["phase"] == "intent"):
        raise FreezeValidationError("cgroup thaw observation conflicts with phase")
    if event["no_signal_replayed_after_own_stage_thaw_intent"] is not True:
        raise FreezeValidationError("cgroup thaw permits a signal replay")
    return event


def _validate_target_absence(value: Any) -> tuple[Mapping[str, Any], ...]:
    if not isinstance(value, list) or len(value) != 2:
        raise FreezeValidationError("target absence must have supervisor and writer rows")
    rows: list[Mapping[str, Any]] = []
    for index, item in enumerate(value):
        row = _exact_keys(item, _TARGET_ABSENCE_KEYS, f"target absence[{index}]")
        expected_role = ("supervisor", "writer")[index]
        if row["role"] != expected_role:
            raise FreezeValidationError("target absence role order differs")
        _require_uint(row["sealed_pid"], f"target absence {expected_role} pid", positive=True)
        _require_uint(
            row["sealed_start_ticks"],
            f"target absence {expected_role} start ticks",
            positive=True,
        )
        if isinstance(row["stable_checks"], bool) or not isinstance(row["stable_checks"], int):
            raise FreezeValidationError("target absence stable_checks must be an integer")
        if row["state"] != "absent" or row["stable_checks"] < 2:
            raise FreezeValidationError("target absence is not stable across two checks")
        rows.append(row)
    return tuple(rows)


def _validate_cgroup_absence(value: Any) -> tuple[Mapping[str, Any], ...]:
    if not isinstance(value, list) or len(value) != 2:
        raise FreezeValidationError("cgroup absence must have supervisor and writer rows")
    rows: list[Mapping[str, Any]] = []
    for index, item in enumerate(value):
        row = _exact_keys(item, _CGROUP_ABSENCE_KEYS, f"cgroup absence[{index}]")
        expected_role = ("supervisor", "writer")[index]
        identity = _validate_cgroup_identity(
            {
                "role": row["role"],
                "path": row["path"],
                "device": row["device"],
                "inode": row["inode"],
            },
            f"cgroup absence {expected_role}",
        )
        if identity.role != expected_role:
            raise FreezeValidationError("cgroup absence role order differs")
        if isinstance(row["stable_checks"], bool) or not isinstance(row["stable_checks"], int):
            raise FreezeValidationError("cgroup absence stable_checks must be an integer")
        if row["state"] not in {"absent", "empty-thawed"} or row["stable_checks"] < 2:
            raise FreezeValidationError("cgroup absence is not stable across two checks")
        rows.append(row)
    return tuple(rows)


def make_zero_signal_offline_reconciliation(
    *,
    arm_event: Mapping[str, Any],
    barrier: BarrierInference,
    target_absence: Sequence[Mapping[str, Any]],
    cgroup_absence: Sequence[Mapping[str, Any]],
    persistent_restart_fence_verified: bool,
    service_enablement_verified: bool,
    signals_sent: int,
    reconciled_at: str,
) -> dict[str, Any]:
    arm = validate_barrier_arm_event(arm_event)
    if barrier.state is not BarrierState.COMMITTED:
        raise FreezeValidationError("offline reconciliation requires a committed barrier")
    if barrier.observed_boot_id == barrier.sealed_boot_id:
        raise FreezeValidationError("zero-signal reconciliation requires a post-commit reboot")
    if (
        barrier.sealed_boot_id != arm["sealed_boot_id"]
        or barrier.allow_marker_path != arm["allow_marker_path"]
        or barrier.durability_basis != "absence-survived-reboot"
    ):
        raise FreezeValidationError("offline barrier evidence differs from arm event")
    value = {
        "schema": OFFLINE_RECONCILIATION_SCHEMA,
        "freeze_plan_sha256": arm["freeze_plan_sha256"],
        "node": arm["node"],
        "barrier_arm_sha256": canonical_sha256(arm),
        "sealed_boot_id": arm["sealed_boot_id"],
        "observed_boot_id": barrier.observed_boot_id,
        "barrier_state": "committed",
        "commit_durability_basis": "absence-survived-reboot",
        "target_absence": list(target_absence),
        "cgroup_absence": list(cgroup_absence),
        "persistent_restart_fence_verified": persistent_restart_fence_verified,
        "service_enablement_verified": service_enablement_verified,
        "signals_sent": signals_sent,
        "supervisor_pidfd_sigterm_state": "none",
        "writer_pidfd_sigterm_state": "none",
        "recovery_sigkill_sent": False,
        "exit_cause": "unknown",
        "reconciled_at": reconciled_at,
    }
    validate_zero_signal_offline_reconciliation(value, arm_event=arm)
    return value


def validate_zero_signal_offline_reconciliation(
    value: Any, *, arm_event: Mapping[str, Any] | None = None
) -> Mapping[str, Any]:
    event = _exact_keys(
        value, _OFFLINE_RECONCILIATION_KEYS, "offline reconciliation"
    )
    if event["schema"] != OFFLINE_RECONCILIATION_SCHEMA:
        raise FreezeValidationError("offline reconciliation schema differs")
    _require_hash(event["freeze_plan_sha256"], "offline freeze-plan hash")
    _require_string(event["node"], "offline node")
    _require_hash(event["barrier_arm_sha256"], "offline barrier-arm hash")
    sealed = _require_uuid(event["sealed_boot_id"], "offline sealed boot")
    observed = _require_uuid(event["observed_boot_id"], "offline observed boot")
    if observed == sealed:
        raise FreezeValidationError("offline reconciliation is not post-reboot")
    if (
        event["barrier_state"] != "committed"
        or event["commit_durability_basis"] != "absence-survived-reboot"
    ):
        raise FreezeValidationError("offline reconciliation lacks durable barrier proof")
    _validate_target_absence(event["target_absence"])
    _validate_cgroup_absence(event["cgroup_absence"])
    if event["persistent_restart_fence_verified"] is not True:
        raise FreezeValidationError("persistent restart fence is not verified")
    if event["service_enablement_verified"] is not True:
        raise FreezeValidationError("service enablement is not verified")
    if event["signals_sent"] != 0:
        raise FreezeValidationError("post-reboot reconciliation attempted a signal")
    if (
        event["supervisor_pidfd_sigterm_state"] != "none"
        or event["writer_pidfd_sigterm_state"] != "none"
        or event["recovery_sigkill_sent"] is not False
        or event["exit_cause"] != "unknown"
    ):
        raise FreezeValidationError("offline signal/exit state is not zero-signal unknown-cause")
    _require_timestamp(event["reconciled_at"], "offline reconciliation timestamp")
    if arm_event is not None:
        arm = validate_barrier_arm_event(arm_event)
        if (
            event["freeze_plan_sha256"] != arm["freeze_plan_sha256"]
            or event["node"] != arm["node"]
            or event["barrier_arm_sha256"] != canonical_sha256(arm)
            or event["sealed_boot_id"] != arm["sealed_boot_id"]
        ):
            raise FreezeValidationError("offline reconciliation differs from barrier arm")
    return event


__all__ = [
    "ARC_NODE_ORDER",
    "ARC_SENTINEL_ORDER",
    "BARRIER_ARM_SCHEMA",
    "BARRIER_COMMIT_SCHEMA",
    "BarrierInference",
    "BarrierState",
    "CGROUP_FREEZE_EVENT_SCHEMA",
    "CGROUP_THAW_EVENT_SCHEMA",
    "CgroupIdentity",
    "DEFAULT_ALLOW_MARKER_PATH",
    "DurableUnlinkEvidence",
    "FREEZE_PLAN_SCHEMA",
    "FailClosedHostMutationAdapter",
    "FreezeNode",
    "FreezeValidationError",
    "HostMutationAdapter",
    "MutationRefused",
    "OFFLINE_RECONCILIATION_SCHEMA",
    "PIDFD_TERM_EVENT_SCHEMA",
    "PREPARE_RECEIPT_SCHEMA",
    "PinnedFreezePlan",
    "RecoveryMutations",
    "TargetIdentity",
    "canonical_json_bytes",
    "canonical_sha256",
    "infer_barrier_state",
    "load_pinned_freeze_plan",
    "make_barrier_arm_event",
    "make_barrier_commit_event",
    "make_cgroup_freeze_event",
    "make_cgroup_thaw_event",
    "make_pidfd_term_event",
    "make_prepare_receipt",
    "make_zero_signal_offline_reconciliation",
    "open_regular_nofollow",
    "parse_canonical_json",
    "read_regular_nofollow",
    "validate_barrier_arm_event",
    "validate_barrier_commit_event",
    "validate_cgroup_freeze_event",
    "validate_cgroup_thaw_event",
    "validate_pidfd_term_event",
    "validate_pinned_freeze_plan",
    "validate_prepare_receipt",
    "validate_secure_stat",
    "validate_zero_signal_offline_reconciliation",
]
