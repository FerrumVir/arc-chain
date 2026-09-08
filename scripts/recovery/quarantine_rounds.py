#!/usr/bin/env python3
"""Canonical validators for crash-safe, mixed-state legacy quarantine rounds.

The recovery transaction cannot atomically fence six remote hosts.  This
module models that fact explicitly: every immutable round starts from an exact
partition of already-fenced and still-live nodes, freshly authorizes only the
live targets, and records the subset that actually crossed the nft boundary.
At most six rounds are possible because a node may transition live->fenced
exactly once.
"""

from __future__ import annotations

import datetime as dt
import hashlib
import json
import re
from typing import Any, Mapping, Sequence


FLEET = (
    ("nyc", "149.28.32.76"),
    ("lax", "140.82.16.112"),
    ("ams", "136.244.109.1"),
    ("lhr", "104.238.171.11"),
    ("nrt", "202.182.107.41"),
    ("sgp", "149.28.153.31"),
)
FLEET_MAP = dict(FLEET)
HASH_RE = re.compile(r"[0-9a-f]{64}")
COMMIT_RE = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})")
UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
UTC_RE = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z")
ROUND_AUTH_SCHEMA = "arc.recovery.quarantine-round-authorization.v1"
NODE_APPLIED_SCHEMA = "arc.recovery.quarantine-node-nft-applied.v1"
NODE_STOPPED_PRECOMMIT_SCHEMA = (
    "arc.recovery.quarantine-node-persistently-stopped-precommit.v1"
)
ROUND_RESULT_SCHEMA = "arc.recovery.quarantine-round-result.v3"
INERT_NODE_PROOF_SCHEMA = "arc.recovery.quarantine-round-zero-progress-node-proof.v1"
ZERO_PROGRESS_RELEASE_SCHEMA = "arc.recovery.quarantine-round-zero-progress-release.v1"
LEDGER_SCHEMA = "arc.recovery.quarantine-generation-ledger.v2"
TARGET_HEIGHT_SCHEMA = "arc.recovery.legacy-public-height-targets.v1"
TARGET_CROSS_SCHEMA = "arc.recovery.authenticated-legacy-height-targets.v1"
LIVE_SOURCE_CAPTURE_SCHEMA = "arc.recovery.quarantine-live-source-capture.v1"
NORMALIZED_LIVE_SOURCE_CAPTURE_SCHEMA = "arc.recovery.quarantine-live-source-capture.v2"
DURABLE_LIVE_SOURCE_CAPTURE_SCHEMA = "arc.recovery.quarantine-live-source-capture.v3"
RUST_SOURCE_CAPTURE_SCHEMA = "arc.recovery.live-legacy-source-capture.v1"
RUST_DURABLE_SOURCE_CAPTURE_SCHEMA = "arc.recovery.live-legacy-source-capture.v2"
WAL_NORMALIZATION_SCHEMA = "arc.recovery.legacy-wal-normalization.v1"
WAL_NORMALIZATION_SCHEMA_V2 = "arc.recovery.legacy-wal-normalization.v2"
WAL_NORMALIZATION_PLAN_SCHEMA_V2 = "arc.recovery.legacy-wal-normalization-plan.v2"
WAL_APPEND_MODE = "strict-crc32-sequence-contiguous-full-frame-suffix"
WAL_SNAPSHOT_MODE = "capture-bind-and-require-rust-exact-boundary"
WAL_TRANSFORM_V2 = (
    "copy-reviewed-base-runs-rewrite-sequence-and-crc32-then-append-"
    "strict-contiguous-frames"
)
DURABLE_WAL_BOUNDARY_PLAN_SCHEMA_V1 = "arc.recovery.durable-wal-boundary-plan.v1"
DURABLE_WAL_BOUNDARY_PLAN_SCHEMA_V2 = "arc.recovery.durable-wal-boundary-plan.v2"
DURABLE_WAL_BOUNDARY_SELECTION_POLICY_V1 = (
    "strict-latest-complete-final-frame-wal-boundary"
)
DURABLE_WAL_BOUNDARY_SELECTION_POLICY_V2 = (
    "reviewed-exact-snapshot-for-unchanged-current-head-wal"
)
MAX_WAL_APPEND_BYTES = 64 * 1024 * 1024
MAX_WAL_APPEND_FRAMES = 1_000_000
MAX_LEGACY_SNAPSHOT_BYTES = 256 * 1024 * 1024
PERSISTED_LEGACY_HEAD_SCHEMA_V3 = "arc.recovery.persisted-legacy-head.v3"
PERSISTED_LEGACY_HEAD_SCHEMA_V4 = "arc.recovery.persisted-legacy-head.v4"
PERSISTED_LEGACY_HEAD_SCHEMA_V5 = "arc.recovery.persisted-legacy-head.v5"
PERSISTED_LEGACY_HEAD_COMMON_FIELDS = frozenset({
    "schema", "source_main_commit", "capture_id", "node", "freeze_plan_sha256",
    "boot_id", "inspector_binary_sha256", "genesis_sha256",
    "validator_public_keys_sha256", "legacy_validator_set_sha256",
    "source_pair_role", "final_source_capture_sha256", "selected_source_head",
    "stop_after_round_receipt_sha256", "network_quarantine_receipt_sha256",
    "stop_complete_sha256", "stop_files_sha256", "capture_complete_sha256",
    "capture_files_sha256", "capture_source_sha256", "source_data_index_sha256",
    "state_wal_sha256", "state_wal_size", "snapshot_sha256", "snapshot_size",
    "source_file_identity", "archived_final_wal", "staged_file_contract",
    "export_summary_sha256", "inspect_summary_sha256", "wal_boundary_sha256",
    "export_status", "head", "candidate_checkpoint_sha256",
    "candidate_checkpoint_size", "snapshot_path", "state_wal_path",
    "export_contract", "legacy_dag_round", "trusted_anchor_ancestry",
    "completed_at", "rerun_reexecutes_export", "writer_stopped",
    "restart_barrier_active", "network_quarantine_active",
    "global_absence_claimed",
})
PRIOR_STATUS_SCHEMA = "arc.recovery.quarantine-prior-fenced-status.v1"
STOPPED_STATUS_SCHEMA = "arc.recovery.quarantine-prior-persistently-stopped-status.v1"
NFT_GATE_SCHEMA = "arc.recovery.quarantine-nft-deadline-gate.v1"
NFT_INTENT_SCHEMA = "arc.recovery.quarantine-nft-apply-intent.v1"
ANCESTRY_SCHEMA = "arc.recovery.quarantine-post-fence-ancestry.v1"
STOPPED_ANCESTRY_SCHEMA = "arc.recovery.quarantine-stopped-precommit-ancestry.v1"
PERSISTED_STOPPED_SCHEMA = "arc.recovery.persisted-legacy-head-stopped-precommit.v2"
PERSISTENCE_PLAN_SCHEMA = "arc.recovery.quarantine-persistence-plan.v1"
PERSISTENT_FENCE_SCHEMA = "arc.recovery.quarantine-persistent-restart-fence.v1"
PRECOMMIT_STATUS_SCHEMA = "arc.recovery.quarantine-precommit-stopped-status.v1"
AUTH_ACCEPTANCE_SCHEMA = "arc.recovery.quarantine-round-authorization-acceptance.v1"
READINESS_SCHEMA = "arc.recovery.quarantine-round-readiness.v1"
TABLE_BINDING_SCHEMA = "arc.recovery.quarantine-nft-table-binding.v1"
ACTIVE_TRANSITION_KIND = "active-network-quarantine"
STOPPED_PRECOMMIT_TRANSITION_KIND = "persistently-stopped-precommit"
MAX_ROUNDS = len(FLEET)
MAX_WINDOW_SECONDS = 300


class QuarantineRoundError(ValueError):
    """An immutable quarantine-round artifact is malformed or inconsistent."""


def fail(message: str) -> None:
    raise QuarantineRoundError(message)


def canonical_bytes(value: Any) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


def digest(value: Any) -> str:
    return hashlib.sha256(canonical_bytes(value)).hexdigest()


def require_hash(value: Any, label: str) -> str:
    if not isinstance(value, str) or HASH_RE.fullmatch(value) is None:
        fail(f"{label} must be a lowercase sha256")
    return value


def require_commit(value: Any, label: str) -> str:
    if not isinstance(value, str) or COMMIT_RE.fullmatch(value) is None:
        fail(f"{label} must be a lowercase Git object id")
    return value


def require_uint(value: Any, label: str, *, positive: bool = False) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < (1 if positive else 0):
        fail(f"{label} must be a {'positive' if positive else 'non-negative'} integer")
    return value


def validate_legacy_dag_round(value: Any, label: str) -> Mapping[str, Any]:
    """Validate the self-contained, read-only durable DAG cursor proof.

    The outer scalar projection is intentionally redundant.  Consumers must
    recompute both roots from the embedded inspector output so a release can be
    selected without trusting a live node or reopening archived WAL bytes.
    """

    fields = {
        "source_consensus_round", "namespace_sha256", "inspection",
        "inspection_sha256",
    }
    if not isinstance(value, dict) or set(value) != fields:
        fail(f"{label} durable DAG round fields differ")
    source_round = require_uint(
        value.get("source_consensus_round"), f"{label} source consensus round",
        positive=True,
    )
    namespace_sha = require_hash(
        value.get("namespace_sha256"), f"{label} DAG namespace"
    )
    inspection_sha = require_hash(
        value.get("inspection_sha256"), f"{label} DAG inspection"
    )
    inspection = value.get("inspection")
    inspection_fields = {
        "schema", "status", "source_consensus_round", "first_segment",
        "last_segment", "segment_count", "inspected_first_segment",
        "inspected_segment_count", "inspected_entry_count", "namespace_sha256",
        "namespace", "read_only",
    }
    if not isinstance(inspection, dict) or set(inspection) != inspection_fields:
        fail(f"{label} embedded DAG inspection fields differ")
    if (
        inspection.get("schema")
        != "arc.recovery.legacy-dag-round-inspection.v1"
        or inspection.get("status") != "VERIFIED_STOPPED_DAG_CURSOR"
        or inspection.get("read_only") is not True
        or inspection_sha != digest(inspection)
        or inspection.get("source_consensus_round") != source_round
        or inspection.get("namespace_sha256") != namespace_sha
    ):
        fail(f"{label} embedded DAG inspection identity differs")
    first = require_uint(
        inspection.get("first_segment"), f"{label} first DAG WAL segment"
    )
    last = require_uint(
        inspection.get("last_segment"), f"{label} last DAG WAL segment"
    )
    segment_count = require_uint(
        inspection.get("segment_count"), f"{label} DAG WAL segment count",
        positive=True,
    )
    inspected_first = require_uint(
        inspection.get("inspected_first_segment"),
        f"{label} first inspected DAG WAL segment",
    )
    inspected_count = require_uint(
        inspection.get("inspected_segment_count"),
        f"{label} inspected DAG WAL segment count", positive=True,
    )
    require_uint(
        inspection.get("inspected_entry_count"),
        f"{label} inspected DAG WAL entry count", positive=True,
    )
    if (
        last < first
        or segment_count != last - first + 1
        or inspected_count != min(3, segment_count)
        or inspected_first != last - inspected_count + 1
    ):
        fail(f"{label} DAG WAL segment arithmetic differs")
    namespace = inspection.get("namespace")
    if not isinstance(namespace, dict) or set(namespace) != {
        "schema", "segment_names", "inspected_tail"
    }:
        fail(f"{label} DAG WAL namespace fields differ")
    segment_names = namespace.get("segment_names")
    expected_names = [f"wal-{index:08d}.bin" for index in range(first, last + 1)]
    tail = namespace.get("inspected_tail")
    if (
        namespace.get("schema") != "arc.recovery.legacy-dag-wal-namespace.v1"
        or segment_names != expected_names
        or not isinstance(tail, list)
        or len(tail) != inspected_count
    ):
        fail(f"{label} DAG WAL namespace differs")
    for index, row in enumerate(tail):
        if not isinstance(row, dict) or set(row) != {"name", "sha256", "size"}:
            fail(f"{label} inspected DAG WAL tail fields differ")
        if row.get("name") != expected_names[-inspected_count + index]:
            fail(f"{label} inspected DAG WAL tail order differs")
        require_hash(row.get("sha256"), f"{label} inspected DAG WAL tail root")
        require_uint(row.get("size"), f"{label} inspected DAG WAL tail size")
    namespace_bytes = json.dumps(
        namespace, sort_keys=True, separators=(",", ":")
    ).encode()
    if hashlib.sha256(namespace_bytes).hexdigest() != namespace_sha:
        fail(f"{label} DAG WAL namespace root is not reproducible")
    return value


def parse_utc(value: Any, label: str) -> dt.datetime:
    if not isinstance(value, str) or UTC_RE.fullmatch(value) is None:
        fail(f"{label} must be canonical UTC seconds")
    try:
        return dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=dt.timezone.utc
        )
    except ValueError as error:
        raise QuarantineRoundError(f"{label} is invalid") from error


def validate_wrapper(wrapper: Any, label: str) -> tuple[Mapping[str, Any], str]:
    if not isinstance(wrapper, dict) or set(wrapper) != {"sha256", "value"}:
        fail(f"{label} wrapper fields differ")
    wanted = require_hash(wrapper.get("sha256"), f"{label} sha256")
    value = wrapper.get("value")
    if not isinstance(value, dict) or digest(value) != wanted:
        fail(f"{label} wrapper hash differs")
    return value, wanted


def wrap(value: Mapping[str, Any]) -> dict[str, Any]:
    return {"sha256": digest(value), "value": dict(value)}


def validate_stable_head(value: Any, label: str) -> tuple[dict[str, Any], int]:
    if not isinstance(value, dict) or set(value) != {"height", "block_hash", "state_root"}:
        fail(f"{label} fields differ")
    height = require_uint(value.get("height"), f"{label} height", positive=True)
    require_hash(value.get("block_hash"), f"{label} block hash")
    require_hash(value.get("state_root"), f"{label} state root")
    return dict(value), height


def validate_writer(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != {
        "boot_id", "pid", "start_ticks", "cgroup_sha256",
    }:
        fail(f"{label} fields differ")
    if not isinstance(value.get("boot_id"), str) or UUID_RE.fullmatch(value["boot_id"]) is None:
        fail(f"{label} boot id differs")
    require_uint(value.get("pid"), f"{label} pid", positive=True)
    require_uint(value.get("start_ticks"), f"{label} start ticks", positive=True)
    require_hash(value.get("cgroup_sha256"), f"{label} cgroup")
    return dict(value)


def validate_file_identity(value: Any, label: str, *, directory: bool = False) -> dict[str, Any]:
    fields = {
        "path", "device", "inode", "mode", "uid", "gid", "nlink", "size",
        "mtime_ns", "ctime_ns",
    }
    if not directory:
        fields.add("sha256")
    if not isinstance(value, dict) or set(value) != fields:
        fail(f"{label} identity fields differ")
    path = value.get("path")
    if not isinstance(path, str) or not path.startswith("/") or "\x00" in path:
        fail(f"{label} path differs")
    for key in fields - {"path", "sha256"}:
        require_uint(value.get(key), f"{label} {key}")
    if value.get("inode") == 0 or value.get("nlink") == 0:
        fail(f"{label} inode/link count differs")
    if not directory:
        require_hash(value.get("sha256"), f"{label} sha256")
    return dict(value)


def validate_rust_input_identity(
    value: Any, label: str, *, directory: bool = False
) -> dict[str, Any]:
    fields = {
        "device", "inode", "mode", "uid", "gid", "nlink", "mtime_ns", "ctime_ns",
    }
    if not directory:
        fields |= {"sha256", "size"}
    if not isinstance(value, dict) or set(value) != fields:
        fail(f"{label} identity fields differ")
    for key in fields - {"sha256"}:
        require_uint(value.get(key), f"{label} {key}")
    if value.get("inode") == 0 or (
        value.get("nlink", 0) == 0 if directory else value.get("nlink") != 1
    ):
        fail(f"{label} inode/link count differs")
    if not directory:
        require_hash(value.get("sha256"), f"{label} sha256")
        if value.get("size") == 0:
            fail(f"{label} is empty")
    return dict(value)


def validate_content_pin(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != {"sha256", "size"}:
        fail(f"{label} fields differ")
    require_hash(value.get("sha256"), f"{label} sha256")
    require_uint(value.get("size"), f"{label} size", positive=True)
    return dict(value)


def validate_head(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != {"height", "block_hash", "state_root"}:
        fail(f"{label} fields differ")
    require_uint(value.get("height"), f"{label} height", positive=True)
    require_hash(value.get("block_hash"), f"{label} block hash")
    require_hash(value.get("state_root"), f"{label} state root")
    return dict(value)


def validate_wal_normalization_v2_plan(
    value: Any, label: str
) -> dict[str, Any]:
    fields = {
        "schema", "node", "base_source_wal", "base_source_snapshot",
        "base_derivative_wal", "base_head", "partition", "sequence_rewrites",
        "append_policy", "snapshot_policy",
    }
    if not isinstance(value, dict) or set(value) != fields:
        fail(f"{label} fields differ")
    if value.get("schema") != WAL_NORMALIZATION_PLAN_SCHEMA_V2:
        fail(f"{label} schema differs")
    if value.get("node") not in {"lax", "ams"}:
        fail(f"{label} node differs")
    base_source = validate_content_pin(value.get("base_source_wal"), f"{label} base source WAL")
    base_snapshot = validate_content_pin(
        value.get("base_source_snapshot"), f"{label} base source snapshot"
    )
    base_derivative = validate_content_pin(
        value.get("base_derivative_wal"), f"{label} base derivative WAL"
    )
    validate_head(value.get("base_head"), f"{label} base head")
    partition = value.get("partition")
    if not isinstance(partition, list) or not partition:
        fail(f"{label} partition differs")
    expected_start = 0
    expected_derivative_sequence = 0
    selected_bytes = 0
    selected_frames = 0
    selected_ranges: list[tuple[int, int]] = []
    for index, row in enumerate(partition):
        if not isinstance(row, dict):
            fail(f"{label} partition row {index} differs")
        kind = row.get("kind")
        common = {"kind", "source_start", "source_end", "sha256"}
        if kind == "selected-frame-run":
            expected_fields = common | {
                "frame_count", "source_first_sequence", "source_last_sequence",
                "sequence_delta", "derivative_first_sequence",
                "derivative_last_sequence",
            }
        elif kind == "excluded-zero-fill":
            expected_fields = common | {"classification"}
            if row.get("classification") != "interior-zero-fill-gap":
                fail(f"{label} zero-fill classification differs")
        elif kind == "excluded-valid-frame-run":
            expected_fields = common | {
                "classification", "frame_count", "first_sequence", "last_sequence",
            }
            if row.get("classification") != "stale-valid-fork":
                fail(f"{label} excluded-frame classification differs")
        else:
            fail(f"{label} partition row {index} kind differs")
        if set(row) != expected_fields:
            fail(f"{label} partition row {index} fields differ")
        start = require_uint(row.get("source_start"), f"{label} partition start")
        end = require_uint(row.get("source_end"), f"{label} partition end", positive=True)
        require_hash(row.get("sha256"), f"{label} partition hash")
        if start != expected_start or end <= start:
            fail(f"{label} partition byte accounting differs")
        expected_start = end
        if kind == "selected-frame-run":
            count = require_uint(row.get("frame_count"), f"{label} selected frames", positive=True)
            source_first = require_uint(
                row.get("source_first_sequence"), f"{label} selected first sequence"
            )
            source_last = require_uint(
                row.get("source_last_sequence"), f"{label} selected last sequence"
            )
            derivative_first = require_uint(
                row.get("derivative_first_sequence"), f"{label} derivative first sequence"
            )
            derivative_last = require_uint(
                row.get("derivative_last_sequence"), f"{label} derivative last sequence"
            )
            delta = row.get("sequence_delta")
            if isinstance(delta, bool) or not isinstance(delta, int):
                fail(f"{label} sequence delta differs")
            if (
                source_last - source_first + 1 != count
                or derivative_last - derivative_first + 1 != count
                or derivative_first != expected_derivative_sequence
            ):
                fail(f"{label} selected sequence accounting differs")
            expected_derivative_sequence = derivative_last + 1
            selected_bytes += end - start
            selected_frames += count
            selected_ranges.append((start, end))
        elif kind == "excluded-valid-frame-run":
            count = require_uint(row.get("frame_count"), f"{label} excluded frames", positive=True)
            first = require_uint(row.get("first_sequence"), f"{label} excluded first sequence")
            last = require_uint(row.get("last_sequence"), f"{label} excluded last sequence")
            if last - first + 1 != count:
                fail(f"{label} excluded sequence accounting differs")
    if expected_start != base_source["size"] or selected_bytes != base_derivative["size"]:
        fail(f"{label} base byte accounting differs")
    terminal = partition[-1]
    if terminal.get("kind") != "selected-frame-run":
        fail(f"{label} base does not end in a selected frame run")
    rewrites = value.get("sequence_rewrites")
    if not isinstance(rewrites, list):
        fail(f"{label} sequence rewrites differ")
    previous_offset = -1
    for index, rewrite in enumerate(rewrites):
        if not isinstance(rewrite, dict) or set(rewrite) != {
            "source_offset", "source_sequence", "derivative_sequence",
        }:
            fail(f"{label} sequence rewrite {index} fields differ")
        offset = require_uint(rewrite.get("source_offset"), f"{label} rewrite offset")
        require_uint(rewrite.get("source_sequence"), f"{label} rewrite source")
        require_uint(rewrite.get("derivative_sequence"), f"{label} rewrite derivative")
        if offset <= previous_offset or not any(start <= offset < end for start, end in selected_ranges):
            fail(f"{label} sequence rewrite placement differs")
        previous_offset = offset
    append_policy = value.get("append_policy")
    if not isinstance(append_policy, dict) or set(append_policy) != {
        "mode", "maximum_bytes", "maximum_frames", "source_first_sequence",
        "derivative_first_sequence", "sequence_delta",
    }:
        fail(f"{label} append policy fields differ")
    maximum_bytes = require_uint(
        append_policy.get("maximum_bytes"), f"{label} maximum append bytes", positive=True
    )
    maximum_frames = require_uint(
        append_policy.get("maximum_frames"), f"{label} maximum append frames", positive=True
    )
    source_first = require_uint(
        append_policy.get("source_first_sequence"), f"{label} append source sequence"
    )
    derivative_first = require_uint(
        append_policy.get("derivative_first_sequence"), f"{label} append derivative sequence"
    )
    append_delta = append_policy.get("sequence_delta")
    if isinstance(append_delta, bool) or not isinstance(append_delta, int):
        fail(f"{label} append sequence delta differs")
    if (
        append_policy.get("mode") != WAL_APPEND_MODE
        or maximum_bytes > MAX_WAL_APPEND_BYTES
        or maximum_frames > MAX_WAL_APPEND_FRAMES
        or source_first != terminal["source_last_sequence"] + 1
        or derivative_first != terminal["derivative_last_sequence"] + 1
        or derivative_first != selected_frames
        or append_delta != terminal["sequence_delta"]
        or source_first + append_delta != derivative_first
    ):
        fail(f"{label} append policy differs from the reviewed base")
    snapshot_policy = value.get("snapshot_policy")
    if not isinstance(snapshot_policy, dict) or set(snapshot_policy) != {"mode", "maximum_size"}:
        fail(f"{label} snapshot policy fields differ")
    maximum_snapshot = require_uint(
        snapshot_policy.get("maximum_size"), f"{label} maximum snapshot size", positive=True
    )
    if (
        snapshot_policy.get("mode") != WAL_SNAPSHOT_MODE
        or maximum_snapshot > MAX_LEGACY_SNAPSHOT_BYTES
        or maximum_snapshot < base_snapshot["size"]
    ):
        fail(f"{label} snapshot policy differs")
    return {
        "plan": dict(value),
        "base_source_wal": base_source,
        "base_source_snapshot": base_snapshot,
        "base_derivative_wal": base_derivative,
        "base_selected_frame_count": selected_frames,
        "append_policy": dict(append_policy),
    }


def validate_wal_normalization_v2_receipt(
    value: Any, label: str
) -> dict[str, Any]:
    fields = {
        "schema", "plan_sha256", "node", "base_source_wal",
        "base_source_snapshot", "base_derivative_wal", "base_head", "partition",
        "sequence_rewrites", "append_policy", "snapshot_policy", "source_wal",
        "source_snapshot", "derivative_wal", "appended_suffix",
        "base_selected_frame_count", "selected_frame_count", "excluded_bytes",
        "semantic_stream_sha256", "transform", "source_unchanged",
    }
    if not isinstance(value, dict) or set(value) != fields:
        fail(f"{label} fields differ")
    if value.get("schema") != WAL_NORMALIZATION_SCHEMA_V2:
        fail(f"{label} schema differs")
    plan_projection = validate_wal_normalization_v2_plan(
        {
            "schema": WAL_NORMALIZATION_PLAN_SCHEMA_V2,
            "node": value.get("node"),
            "base_source_wal": value.get("base_source_wal"),
            "base_source_snapshot": value.get("base_source_snapshot"),
            "base_derivative_wal": value.get("base_derivative_wal"),
            "base_head": value.get("base_head"),
            "partition": value.get("partition"),
            "sequence_rewrites": value.get("sequence_rewrites"),
            "append_policy": value.get("append_policy"),
            "snapshot_policy": value.get("snapshot_policy"),
        },
        f"{label} embedded plan",
    )
    require_hash(value.get("plan_sha256"), f"{label} plan sha256")
    source_wal = validate_rust_input_identity(value.get("source_wal"), f"{label} source WAL")
    source_snapshot = validate_rust_input_identity(
        value.get("source_snapshot"), f"{label} source snapshot"
    )
    derivative = validate_rust_input_identity(
        value.get("derivative_wal"), f"{label} derivative WAL"
    )
    base_source = plan_projection["base_source_wal"]
    base_derivative = plan_projection["base_derivative_wal"]
    appended = value.get("appended_suffix")
    append_fields = {
        "source_start", "source_end", "source_bytes", "source_sha256",
        "derivative_start", "derivative_end", "derivative_bytes", "derivative_sha256",
        "frame_count", "source_first_sequence", "source_last_sequence",
        "derivative_first_sequence", "derivative_last_sequence", "sequence_delta",
    }
    if not isinstance(appended, dict) or set(appended) != append_fields:
        fail(f"{label} appended suffix fields differ")
    for key in (
        "source_start", "source_end", "source_bytes", "derivative_start",
        "derivative_end", "derivative_bytes", "frame_count",
    ):
        require_uint(appended.get(key), f"{label} appended {key}")
    require_hash(appended.get("source_sha256"), f"{label} appended source root")
    require_hash(appended.get("derivative_sha256"), f"{label} appended derivative root")
    frame_count = appended["frame_count"]
    no_append = frame_count == 0
    policy = plan_projection["append_policy"]
    if (
        appended["source_start"] != base_source["size"]
        or appended["source_end"] != source_wal["size"]
        or appended["source_bytes"] != source_wal["size"] - base_source["size"]
        or appended["derivative_start"] != base_derivative["size"]
        or appended["derivative_end"] != derivative["size"]
        or appended["derivative_bytes"]
            != derivative["size"] - base_derivative["size"]
        or appended["source_bytes"] != appended["derivative_bytes"]
        or appended["source_bytes"] > policy["maximum_bytes"]
        or frame_count > policy["maximum_frames"]
        or source_snapshot["size"]
            > value["snapshot_policy"]["maximum_size"]
        or appended.get("sequence_delta") != policy["sequence_delta"]
        or value.get("base_selected_frame_count")
            != plan_projection["base_selected_frame_count"]
        or value.get("selected_frame_count")
            != plan_projection["base_selected_frame_count"] + frame_count
        or require_uint(value.get("excluded_bytes"), f"{label} excluded bytes") < 0
        or value.get("transform") != WAL_TRANSFORM_V2
        or value.get("source_unchanged") is not True
    ):
        fail(f"{label} append/base accounting differs")
    empty_sha256 = hashlib.sha256(b"").hexdigest()
    sequence_fields = (
        "source_first_sequence", "source_last_sequence",
        "derivative_first_sequence", "derivative_last_sequence",
    )
    if no_append:
        if (
            any(appended.get(key) is not None for key in sequence_fields)
            or appended["source_sha256"] != empty_sha256
            or appended["derivative_sha256"] != empty_sha256
            or appended["source_bytes"] != 0
        ):
            fail(f"{label} empty append inventory differs")
    else:
        for key in sequence_fields:
            require_uint(appended.get(key), f"{label} appended {key}")
        if (
            appended["source_bytes"] == 0
            or appended["source_first_sequence"] != policy["source_first_sequence"]
            or appended["derivative_first_sequence"]
                != policy["derivative_first_sequence"]
            or appended["source_last_sequence"]
                != appended["source_first_sequence"] + frame_count - 1
            or appended["derivative_last_sequence"]
                != appended["derivative_first_sequence"] + frame_count - 1
        ):
            fail(f"{label} appended sequence accounting differs")
    require_hash(value.get("semantic_stream_sha256"), f"{label} semantic stream")
    return {
        "source_wal": source_wal,
        "source_snapshot": source_snapshot,
        "derivative_wal": derivative,
        "appended_suffix": dict(appended),
        **plan_projection,
    }


def validate_durable_wal_boundary_plan(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} fields differ")
    schema = value.get("schema")
    expected_fields = {
        "node", "schema", "selection_policy", "source_snapshot", "source_wal",
    }
    if schema == DURABLE_WAL_BOUNDARY_PLAN_SCHEMA_V2:
        expected_fields.add("recovery_snapshot")
    if set(value) != expected_fields or value.get("node") != "sgp":
        fail(f"{label} policy differs")
    if schema == DURABLE_WAL_BOUNDARY_PLAN_SCHEMA_V1:
        if value.get("selection_policy") != DURABLE_WAL_BOUNDARY_SELECTION_POLICY_V1:
            fail(f"{label} v1 policy differs")
    elif schema == DURABLE_WAL_BOUNDARY_PLAN_SCHEMA_V2:
        if value.get("selection_policy") != DURABLE_WAL_BOUNDARY_SELECTION_POLICY_V2:
            fail(f"{label} v2 policy differs")
    else:
        fail(f"{label} schema differs")
    source_wal = validate_content_pin(value.get("source_wal"), f"{label} source WAL")
    source_snapshot = value.get("source_snapshot")
    if not isinstance(source_snapshot, dict) or set(source_snapshot) != {
        "height", "sha256", "size", "state_root",
    }:
        fail(f"{label} source snapshot fields differ")
    require_uint(source_snapshot.get("height"), f"{label} source snapshot height", positive=True)
    require_hash(source_snapshot.get("sha256"), f"{label} source snapshot sha256")
    require_uint(source_snapshot.get("size"), f"{label} source snapshot size", positive=True)
    require_hash(source_snapshot.get("state_root"), f"{label} source snapshot state root")
    result = {"source_wal": source_wal, "source_snapshot": dict(source_snapshot)}
    if schema == DURABLE_WAL_BOUNDARY_PLAN_SCHEMA_V2:
        recovery = value.get("recovery_snapshot")
        recovery_fields = {
            "path", "height", "block_hash", "state_root", "sha256", "size",
            "source_receipt",
        }
        if not isinstance(recovery, dict) or set(recovery) != recovery_fields:
            fail(f"{label} recovery snapshot fields differ")
        recovery_path = recovery.get("path")
        receipt = recovery.get("source_receipt")
        if (
            not isinstance(recovery_path, str)
            or not recovery_path.startswith(
                "/root/arc-recovery-live-source-captures/"
            )
            or not recovery_path.endswith("/fixed-source/state.snapshot.lz4")
            or ".." in recovery_path
            or "\x00" in recovery_path
            or not isinstance(receipt, dict)
            or set(receipt) != {"path", "sha256", "size"}
            or not isinstance(receipt.get("path"), str)
            or receipt["path"] != recovery_path.removesuffix(
                "/fixed-source/state.snapshot.lz4"
            ) + "/receipt.json"
        ):
            fail(f"{label} recovery snapshot provenance path differs")
        require_uint(recovery.get("height"), f"{label} recovery height", positive=True)
        require_hash(recovery.get("block_hash"), f"{label} recovery block hash")
        require_hash(recovery.get("state_root"), f"{label} recovery state root")
        require_hash(recovery.get("sha256"), f"{label} recovery snapshot sha256")
        require_uint(recovery.get("size"), f"{label} recovery snapshot size", positive=True)
        require_hash(receipt.get("sha256"), f"{label} recovery receipt sha256")
        require_uint(receipt.get("size"), f"{label} recovery receipt size", positive=True)
        if (
            (recovery["height"], recovery["state_root"])
            == (source_snapshot["height"], source_snapshot["state_root"])
        ):
            fail(f"{label} recovery snapshot does not differ from the observed snapshot")
        result["recovery_snapshot"] = dict(recovery)
    return result


def validate_durable_wal_boundary_capture(
    value: Any, plan_sha256: Any, label: str
) -> dict[str, Any]:
    """Validate the content-addressed Rust SGP durable-boundary proof."""

    plan_sha = require_hash(plan_sha256, f"{label} plan sha256")
    if not isinstance(value, dict) or set(value) != {
        "source_plan", "fixed_plan", "plan", "selected_boundary",
        "preserved_source_snapshot", "replay_derived_snapshot",
    }:
        fail(f"{label} fields differ")
    source_plan = validate_rust_input_identity(
        value.get("source_plan"), f"{label} source plan"
    )
    fixed_plan = validate_rust_input_identity(
        value.get("fixed_plan"), f"{label} fixed plan"
    )
    plan = value.get("plan")
    plan_projection = validate_durable_wal_boundary_plan(
        plan, f"{label} embedded plan"
    )
    preserved_snapshot = validate_rust_input_identity(
        value.get("preserved_source_snapshot"),
        f"{label} preserved source snapshot",
    )
    replay_snapshot = validate_rust_input_identity(
        value.get("replay_derived_snapshot"),
        f"{label} replay-derived snapshot",
    )
    selected_boundary = value.get("selected_boundary")
    if not isinstance(selected_boundary, dict) or set(selected_boundary) != {
        "height", "block_hash", "state_root", "checkpoint_sequence",
    }:
        fail(f"{label} selected boundary fields differ")
    validate_head(
        {
            key: selected_boundary.get(key)
            for key in ("height", "block_hash", "state_root")
        },
        f"{label} selected boundary",
    )
    require_uint(
        selected_boundary.get("checkpoint_sequence"),
        f"{label} selected checkpoint sequence",
    )
    plan_size = len(canonical_bytes(plan))
    if (
        digest(plan) != plan_sha
        or source_plan["sha256"] != plan_sha
        or source_plan["size"] != plan_size
        or fixed_plan["sha256"] != plan_sha
        or fixed_plan["size"] != plan_size
    ):
        fail(f"{label} plan content binding differs")
    return {
        "plan": dict(plan),
        "plan_projection": plan_projection,
        "source_plan": source_plan,
        "fixed_plan": fixed_plan,
        "selected_boundary": dict(selected_boundary),
        "preserved_source_snapshot": preserved_snapshot,
        "replay_derived_snapshot": replay_snapshot,
    }


def validate_sgp_persisted_head_v5(
    value: Any, expected_plan_sha256: Any, label: str
) -> dict[str, Any]:
    """Validate the exact persisted SGP durable-WAL exception and its archive relation."""

    expected_plan = require_hash(expected_plan_sha256, f"{label} expected plan")
    fields = set(PERSISTED_LEGACY_HEAD_COMMON_FIELDS) | {"durable_wal_boundary"}
    if not isinstance(value, dict) or set(value) != fields:
        fail(f"{label} fields differ")
    if (
        value.get("schema") != PERSISTED_LEGACY_HEAD_SCHEMA_V5
        or value.get("node") != "sgp"
        or value.get("source_pair_role") != "post-quarantine-final-export"
        or value.get("export_status") != "EXPORTED_UNSIGNED"
        or value.get("rerun_reexecutes_export") is not True
        or value.get("writer_stopped") is not True
        or value.get("restart_barrier_active") is not True
        or value.get("network_quarantine_active") is not True
        or value.get("global_absence_claimed") is not False
    ):
        fail(f"{label} fixed policy differs")
    require_commit(value.get("source_main_commit"), f"{label} source commit")
    if not isinstance(value.get("boot_id"), str) or UUID_RE.fullmatch(value["boot_id"]) is None:
        fail(f"{label} boot id differs")
    for key in (
        "capture_id", "freeze_plan_sha256", "inspector_binary_sha256",
        "genesis_sha256", "validator_public_keys_sha256",
        "legacy_validator_set_sha256", "final_source_capture_sha256",
        "stop_after_round_receipt_sha256", "network_quarantine_receipt_sha256",
        "stop_complete_sha256", "stop_files_sha256", "capture_complete_sha256",
        "capture_files_sha256", "capture_source_sha256", "source_data_index_sha256",
        "state_wal_sha256", "snapshot_sha256", "export_summary_sha256",
        "inspect_summary_sha256", "wal_boundary_sha256", "candidate_checkpoint_sha256",
    ):
        require_hash(value.get(key), f"{label} {key}")
    wal_size = require_uint(value.get("state_wal_size"), f"{label} WAL size", positive=True)
    snapshot_size = require_uint(
        value.get("snapshot_size"), f"{label} snapshot size", positive=True
    )
    require_uint(
        value.get("candidate_checkpoint_size"), f"{label} checkpoint size", positive=True
    )
    head = validate_head(value.get("head"), f"{label} head")
    if validate_head(value.get("selected_source_head"), f"{label} selected head") != head:
        fail(f"{label} selected head differs")
    parse_utc(value.get("completed_at"), f"{label} completion")
    for key in ("snapshot_path", "state_wal_path"):
        path = value.get(key)
        if not isinstance(path, str) or not path.startswith("/") or "\x00" in path:
            fail(f"{label} {key} differs")

    def compact_identity(candidate: Any, identity_label: str) -> dict[str, Any]:
        identity_fields = {"device", "inode", "size", "mode"}
        if not isinstance(candidate, dict) or set(candidate) != identity_fields:
            fail(f"{identity_label} fields differ")
        for key in identity_fields:
            require_uint(candidate.get(key), f"{identity_label} {key}")
        if candidate.get("inode") == 0:
            fail(f"{identity_label} inode differs")
        return dict(candidate)

    source_identity = value.get("source_file_identity")
    if not isinstance(source_identity, dict) or set(source_identity) != {"state_wal", "snapshot"}:
        fail(f"{label} source-file identity fields differ")
    source_wal_identity = compact_identity(
        source_identity.get("state_wal"), f"{label} source WAL identity"
    )
    source_snapshot_identity = compact_identity(
        source_identity.get("snapshot"), f"{label} source snapshot identity"
    )
    if source_wal_identity["size"] != wal_size or source_snapshot_identity["size"] != snapshot_size:
        fail(f"{label} source-file size differs")

    staged = value.get("staged_file_contract")
    if not isinstance(staged, dict) or set(staged) != {
        "state_wal", "snapshot", "ephemeral_inode_receipted",
    } or staged.get("ephemeral_inode_receipted") is not False:
        fail(f"{label} staged-file contract fields differ")
    staged_fields = {"sha256", "size", "mode", "uid", "gid", "nlink"}
    for key, expected_sha, expected_size in (
        ("state_wal", value["state_wal_sha256"], wal_size),
        ("snapshot", value["snapshot_sha256"], snapshot_size),
    ):
        item = staged.get(key)
        if (
            not isinstance(item, dict)
            or set(item) != staged_fields
            or item.get("sha256") != expected_sha
            or item.get("size") != expected_size
            or item.get("mode") != 0o100400
            or item.get("uid") != 0
            or item.get("gid") != 0
            or item.get("nlink") != 1
        ):
            fail(f"{label} staged {key} differs")

    export_contract = value.get("export_contract")
    if not isinstance(export_contract, dict) or export_contract != {
        "binary_path": "/proc/self/fd/8", "exit_code": 0,
        "source_consensus_round": 0, "created_at_unix_ms": 0,
        "recovery_epoch": 1, "validator_set_id": 1,
        "allow_unbound_legacy_wal": True, "read_only": True,
    }:
        fail(f"{label} export contract differs")

    wrapper = value.get("durable_wal_boundary")
    if not isinstance(wrapper, dict) or set(wrapper) != {"plan_sha256", "capture"}:
        fail(f"{label} durable WAL wrapper fields differ")
    if wrapper.get("plan_sha256") != expected_plan:
        fail(f"{label} durable WAL plan pin differs")
    durable = validate_durable_wal_boundary_capture(
        wrapper.get("capture"), expected_plan, f"{label} durable WAL capture"
    )
    plan_wal = durable["plan_projection"]["source_wal"]
    plan_snapshot = durable["plan_projection"]["source_snapshot"]
    recovery_snapshot = durable["plan_projection"].get("recovery_snapshot")
    selected = durable["selected_boundary"]
    preserved = durable["preserved_source_snapshot"]
    replay = durable["replay_derived_snapshot"]
    if (
        {key: selected[key] for key in ("height", "block_hash", "state_root")} != head
        or plan_wal != {"sha256": value["state_wal_sha256"], "size": wal_size}
        or preserved["sha256"] != plan_snapshot["sha256"]
        or preserved["size"] != plan_snapshot["size"]
        or replay["sha256"] != value["snapshot_sha256"]
        or replay["size"] != snapshot_size
        or (plan_snapshot["height"], plan_snapshot["state_root"])
            == (head["height"], head["state_root"])
        or (
            recovery_snapshot is not None
            and {key: recovery_snapshot[key] for key in (
                "height", "block_hash", "state_root"
            )} != head
        )
    ):
        fail(f"{label} durable WAL content relation differs")

    archived = value.get("archived_final_wal")
    archived_fields = {
        "path", "sha256", "size", "file_identity", "source_relation",
        "durable_wal_boundary_plan_sha256", "selected_prefix_bytes",
        "selected_prefix_sha256", "post_capture_suffix_bytes",
        "post_capture_suffix_sha256", "post_capture_suffix_classification",
        "preserved_by",
    }
    if not isinstance(archived, dict) or set(archived) != archived_fields:
        fail(f"{label} archived WAL fields differ")
    archive_path = archived.get("path")
    archive_size = require_uint(
        archived.get("size"), f"{label} archived WAL size", positive=True
    )
    suffix_bytes = require_uint(
        archived.get("post_capture_suffix_bytes"), f"{label} archived WAL suffix"
    )
    archive_identity = compact_identity(
        archived.get("file_identity"), f"{label} archived WAL identity"
    )
    archive_sha = require_hash(
        archived.get("sha256"), f"{label} archived WAL sha256"
    )
    if (
        not isinstance(archive_path, str) or not archive_path.startswith("/")
        or "\x00" in archive_path
        or archive_identity["size"] != archive_size
        or archived.get("source_relation")
            != "exact-content-pinned-durable-wal-source-prefix"
        or archived.get("durable_wal_boundary_plan_sha256") != expected_plan
        or archived.get("selected_prefix_bytes") != wal_size
        or archived.get("selected_prefix_sha256") != value["state_wal_sha256"]
        or archive_size != wal_size + suffix_bytes
        or archived.get("post_capture_suffix_classification")
            != "durable_wal_boundary_from_content_pinned_archived_original"
        or archived.get("preserved_by")
            != "complete-content-indexed-stopped-legacy-source-v4"
    ):
        fail(f"{label} archived WAL relation differs")
    suffix_sha = archived.get("post_capture_suffix_sha256")
    if suffix_bytes == 0:
        if suffix_sha is not None or archive_sha != value["state_wal_sha256"]:
            fail(f"{label} archived WAL suffix hash differs")
    elif require_hash(
        suffix_sha, f"{label} archived WAL suffix sha256"
    ) != suffix_sha:
        fail(f"{label} archived WAL suffix hash differs")
    return durable


def validate_live_source_capture(
    value: Any,
    *,
    capture_id: str,
    freeze_sha256: str,
    source_main_commit: str,
    round_number: int,
    target: Mapping[str, Any],
    public_row: Mapping[str, Any],
    cross_row: Mapping[str, Any],
    public_sha256: str,
    cross_sha256: str,
) -> tuple[dt.datetime, dict[str, Any]]:
    fields = {
        "schema", "capture_id", "freeze_plan_sha256", "source_main_commit",
        "round_number", "node", "host", "authorized_writer", "rpc_origin",
        "public_height_receipt_sha256", "authenticated_height_cross_proof_sha256",
        "snapshot_endpoint", "snapshot_listener", "capture_attempt_id",
        "capture_started_at", "capture_completed_at", "inspector_binary_sha256",
        "genesis_sha256", "legacy_validator_set_sha256", "fixed_pair_path",
        "snapshot_source", "existing_source_snapshot_used", "rust_capture",
        "head", "ancestry_checks", "content_sealed", "strict_offline_replay",
        "source_pair_role", "minimum_height", "expected_head",
        "boundary_proof_sha256", "network_quarantine_receipt_sha256",
        "owned_ruleset_stateless_sha256",
    }
    schema = value.get("schema") if isinstance(value, dict) else None
    normalized = schema == NORMALIZED_LIVE_SOURCE_CAPTURE_SCHEMA
    durable = schema == DURABLE_LIVE_SOURCE_CAPTURE_SCHEMA
    if normalized:
        fields.add("wal_normalization")
    if durable:
        fields.add("durable_wal_boundary")
    if not isinstance(value, dict) or set(value) != fields:
        fail("live source capture fields differ")
    node = target.get("node")
    if (
        schema not in {
            LIVE_SOURCE_CAPTURE_SCHEMA,
            NORMALIZED_LIVE_SOURCE_CAPTURE_SCHEMA,
            DURABLE_LIVE_SOURCE_CAPTURE_SCHEMA,
        }
        or (normalized and node not in {"lax", "ams"})
        or (durable and node != "sgp")
        or (
            value.get("capture_id"), value.get("freeze_plan_sha256"),
            value.get("source_main_commit"), value.get("round_number"),
            value.get("node"), value.get("host"),
        )
        != (
            capture_id, freeze_sha256, source_main_commit, round_number,
            node, FLEET_MAP.get(str(node)),
        )
        or value.get("public_height_receipt_sha256") != public_sha256
        or value.get("authenticated_height_cross_proof_sha256") != cross_sha256
        or value.get("snapshot_endpoint") != "/sync/snapshot"
        or value.get("snapshot_source")
            != "sealed-writer-owned-loopback-/sync/snapshot"
        or value.get("existing_source_snapshot_used") is not False
        or value.get("content_sealed") is not True
        or value.get("strict_offline_replay") is not True
        or value.get("source_pair_role") != "preauthorization-boundary"
        or value.get("expected_head") is not None
        or value.get("boundary_proof_sha256") != cross_sha256
        or value.get("network_quarantine_receipt_sha256") is not None
        or value.get("owned_ruleset_stateless_sha256") is not None
    ):
        fail("live source capture identity/policy differs")
    require_commit(value.get("source_main_commit"), "live source capture commit")
    for key in ("inspector_binary_sha256", "genesis_sha256", "legacy_validator_set_sha256"):
        require_hash(value.get(key), f"live source capture {key}")
    attempt_id = value.get("capture_attempt_id")
    if not isinstance(attempt_id, str) or UUID_RE.fullmatch(attempt_id) is None:
        fail("live source capture attempt id differs")
    rpc_origin = value.get("rpc_origin")
    if not isinstance(rpc_origin, str) or not re.fullmatch(
        r"http://127\.0\.0\.1:[1-9][0-9]{0,4}", rpc_origin
    ):
        fail("live source capture RPC origin differs")
    fixed_path = value.get("fixed_pair_path")
    expected_prefix = f"/root/arc-recovery-live-source-captures/{capture_id}/{node}/round-{round_number}/"
    if (
        not isinstance(fixed_path, str)
        or not fixed_path.startswith(expected_prefix)
        or not fixed_path.endswith("/fixed-source")
        or ".." in fixed_path
    ):
        fail("live source fixed-pair path differs")

    writer = value.get("authorized_writer")
    expected_writer = {
        "boot_id": target.get("boot_id"), "pid": target.get("writer_pid"),
        "start_ticks": target.get("writer_start_ticks"),
        "cgroup_sha256": target.get("writer_cgroup_sha256"),
    }
    if writer != expected_writer:
        fail("live source capture writer differs from authorization target")
    validate_writer(writer, "live source capture writer")
    listener = value.get("snapshot_listener")
    if not isinstance(listener, dict) or set(listener) != {
        "boot_id", "pid", "start_ticks", "port", "socket_inode",
    }:
        fail("live source snapshot listener fields differ")
    if (
        listener.get("boot_id") != writer["boot_id"]
        or listener.get("pid") != writer["pid"]
        or listener.get("start_ticks") != writer["start_ticks"]
        or require_uint(listener.get("port"), "snapshot listener port", positive=True) > 65535
        or require_uint(
            listener.get("socket_inode"), "snapshot listener socket inode", positive=True
        ) == 0
        or rpc_origin != f"http://127.0.0.1:{listener['port']}"
    ):
        fail("live source snapshot listener is not owned by the sealed writer")

    head = value.get("head")
    if not isinstance(head, dict) or set(head) != {"height", "block_hash", "state_root"}:
        fail("live source capture head fields differ")
    head_height = require_uint(head.get("height"), "live source capture head", positive=True)
    require_hash(head.get("block_hash"), "live source capture block hash")
    require_hash(head.get("state_root"), "live source capture state root")
    authorization_floor = max(
        require_uint(
            public_row.get("info_after_height"),
            "live source capture public info-after height",
        ),
        require_uint(
            cross_row.get("loopback_info_after_height"),
            "live source capture authenticated info-after height",
        ),
    )
    if (
        value.get("minimum_height") != authorization_floor
        or head_height < authorization_floor
    ):
        fail("live source capture head/floor is below the fresh authorization")
    checks = value.get("ancestry_checks")
    expected_checks = (
        ("public-latest", public_row.get("latest_block_height"), public_row.get("latest_block_hash")),
        (
            "authenticated-loopback-latest", cross_row.get("loopback_latest_height"),
            cross_row.get("loopback_latest_block_hash"),
        ),
    )
    if not isinstance(checks, list) or len(checks) != len(expected_checks):
        fail("live source capture ancestry checks differ")
    for check, (label, height, block_hash) in zip(checks, expected_checks):
        if not isinstance(check, dict) or set(check) != {
            "label", "height", "expected_block_hash", "observed_block_hash",
            "state_root", "inspection_sha256",
        }:
            fail("live source capture ancestry-check fields differ")
        if (
            check.get("label") != label
            or check.get("height") != height
            or check.get("expected_block_hash") != block_hash
            or check.get("observed_block_hash") != block_hash
            or require_uint(check.get("height"), f"{label} height", positive=True) > head_height
        ):
            fail("live source capture ancestry differs from fresh height evidence")
        require_hash(check.get("state_root"), f"{label} state root")
        require_hash(check.get("inspection_sha256"), f"{label} inspection")

    rust, rust_sha = validate_wrapper(value.get("rust_capture"), "Rust live source capture")
    rust_fields = {
        "schema", "captured_at_unix_ms", "head", "source_data_dir",
        "source_wal_prefix", "source_snapshot", "genesis", "legacy_validator_set",
        "fixed_pair", "allow_unbound_legacy_wal",
    }
    if durable:
        rust_fields.add("durable_wal_boundary")
    if not isinstance(rust, dict) or set(rust) != rust_fields:
        fail("Rust live source capture fields differ")
    expected_rust_schema = (
        RUST_DURABLE_SOURCE_CAPTURE_SCHEMA if durable else RUST_SOURCE_CAPTURE_SCHEMA
    )
    if rust.get("schema") != expected_rust_schema or rust.get("head") != head:
        fail("Rust live source capture head/schema differs")
    if (normalized or durable) and rust.get("allow_unbound_legacy_wal") is not True:
        fail("exceptional Rust live source did not use the explicit unbound-WAL policy")
    require_uint(rust.get("captured_at_unix_ms"), "Rust live capture timestamp", positive=True)
    validate_rust_input_identity(rust.get("source_data_dir"), "Rust source directory", directory=True)
    source_wal = rust.get("source_wal_prefix")
    if not isinstance(source_wal, dict) or set(source_wal) != {
        "device", "inode", "mode", "uid", "gid", "nlink",
        "loader_observed_bytes", "copy_observed_bytes", "accepted_prefix_bytes",
        "accepted_prefix_sha256", "quarantined_suffix_bytes_at_loader",
        "loader_tail_reason",
    }:
        fail("Rust source WAL-prefix fields differ")
    for key in (
        "device", "inode", "mode", "uid", "gid", "nlink", "loader_observed_bytes",
        "copy_observed_bytes", "accepted_prefix_bytes", "quarantined_suffix_bytes_at_loader",
    ):
        require_uint(source_wal.get(key), f"Rust source WAL {key}")
    require_hash(source_wal.get("accepted_prefix_sha256"), "Rust accepted WAL prefix")
    if (
        source_wal.get("inode") == 0
        or source_wal.get("nlink") != 1
        or source_wal.get("accepted_prefix_bytes", 0) == 0
        or source_wal.get("loader_observed_bytes", 0)
            < source_wal.get("accepted_prefix_bytes", 0)
        or source_wal.get("copy_observed_bytes", 0)
            < source_wal.get("accepted_prefix_bytes", 0)
        or source_wal.get("quarantined_suffix_bytes_at_loader")
            != source_wal.get("loader_observed_bytes")
                - source_wal.get("accepted_prefix_bytes")
        or not isinstance(source_wal.get("loader_tail_reason"), str)
    ):
        fail("Rust source WAL-prefix accounting differs")
    source_snapshot = validate_rust_input_identity(
        rust.get("source_snapshot"), "Rust source snapshot"
    )
    genesis = validate_rust_input_identity(rust.get("genesis"), "Rust genesis")
    legacy = validate_rust_input_identity(
        rust.get("legacy_validator_set"), "Rust legacy validator set"
    )
    if (
        (not durable and source_snapshot["sha256"]
            != rust.get("fixed_pair", {}).get("snapshot", {}).get("sha256"))
        or genesis["sha256"] != value.get("genesis_sha256")
        or legacy["sha256"] != value.get("legacy_validator_set_sha256")
    ):
        fail("Rust live source capture static input roots differ")
    fixed = rust.get("fixed_pair")
    if not isinstance(fixed, dict) or set(fixed) != {
        "data_dir", "state_wal", "snapshot", "genesis_binding", "strict_replay",
    }:
        fail("Rust fixed source-pair fields differ")
    validate_rust_input_identity(fixed.get("data_dir"), "Rust fixed directory", directory=True)
    fixed_wal = validate_rust_input_identity(fixed.get("state_wal"), "Rust fixed WAL")
    fixed_snapshot = validate_rust_input_identity(fixed.get("snapshot"), "Rust fixed snapshot")
    validate_rust_input_identity(fixed.get("genesis_binding"), "Rust fixed genesis binding")
    if (
        fixed.get("strict_replay") is not True
        or fixed_wal["sha256"] != source_wal["accepted_prefix_sha256"]
        or fixed_wal["size"] != source_wal["accepted_prefix_bytes"]
        or (not durable and fixed_snapshot["sha256"] != source_snapshot["sha256"])
    ):
        fail("Rust fixed pair is not the exact replayed source prefix/snapshot")
    if normalized:
        normalization = value.get("wal_normalization")
        if not isinstance(normalization, dict) or set(normalization) != {
            "normalizer_sha256", "plan_sha256", "receipt",
        }:
            fail("legacy WAL normalization wrapper fields differ")
        require_hash(
            normalization.get("normalizer_sha256"), "legacy WAL normalizer"
        )
        plan_sha = require_hash(
            normalization.get("plan_sha256"), "legacy WAL normalization plan"
        )
        normalization_receipt, _normalization_sha = validate_wrapper(
            normalization.get("receipt"), "legacy WAL normalization receipt"
        )
        normalization_schema = (
            normalization_receipt.get("schema")
            if isinstance(normalization_receipt, dict)
            else None
        )
        if normalization_schema == WAL_NORMALIZATION_SCHEMA:
            normalization_fields = {
                "schema", "plan_sha256", "node", "source_wal", "source_snapshot",
                "partition", "sequence_rewrites", "derivative_wal", "head",
                "selected_frame_count", "excluded_bytes", "semantic_stream_sha256",
                "transform", "source_unchanged",
            }
            if (
                set(normalization_receipt) != normalization_fields
                or normalization_receipt.get("plan_sha256") != plan_sha
                or normalization_receipt.get("node") != node
                or normalization_receipt.get("head") != head
                or normalization_receipt.get("transform")
                    != "copy-selected-runs-rewrite-sequence-and-crc32"
                or normalization_receipt.get("source_unchanged") is not True
                or not isinstance(normalization_receipt.get("partition"), list)
                or not normalization_receipt.get("partition")
                or not isinstance(normalization_receipt.get("sequence_rewrites"), list)
            ):
                fail("legacy WAL v1 normalization receipt identity differs")
            validate_rust_input_identity(
                normalization_receipt.get("source_wal"),
                "legacy WAL v1 normalization source",
            )
            normalization_snapshot = validate_rust_input_identity(
                normalization_receipt.get("source_snapshot"),
                "legacy WAL v1 normalization snapshot",
            )
            derivative = validate_rust_input_identity(
                normalization_receipt.get("derivative_wal"),
                "legacy WAL v1 normalization derivative",
            )
            if (
                normalization_snapshot["sha256"] != source_snapshot["sha256"]
                or derivative["sha256"] != source_wal["accepted_prefix_sha256"]
                or derivative["size"] != source_wal["accepted_prefix_bytes"]
                or source_wal["quarantined_suffix_bytes_at_loader"] != 0
                or source_wal["loader_tail_reason"] != "none"
                or require_uint(
                    normalization_receipt.get("selected_frame_count"),
                    "legacy WAL v1 selected frame count",
                    positive=True,
                ) == 0
                or require_uint(
                    normalization_receipt.get("excluded_bytes"),
                    "legacy WAL v1 excluded bytes",
                ) < 0
            ):
                fail("v1 normalized derivative is not the exact strict Rust capture source")
            require_hash(
                normalization_receipt.get("semantic_stream_sha256"),
                "legacy WAL v1 semantic stream",
            )
        elif normalization_schema == WAL_NORMALIZATION_SCHEMA_V2:
            projection = validate_wal_normalization_v2_receipt(
                normalization_receipt, "legacy WAL v2 normalization receipt"
            )
            derivative = projection["derivative_wal"]
            normalization_snapshot = projection["source_snapshot"]
            tail_bytes = source_wal["quarantined_suffix_bytes_at_loader"]
            tail_reason = source_wal["loader_tail_reason"]
            if (
                normalization_receipt.get("plan_sha256") != plan_sha
                or normalization_receipt.get("node") != node
                or normalization_snapshot["sha256"] != source_snapshot["sha256"]
                or normalization_snapshot["size"] != source_snapshot["size"]
                or derivative["size"] != source_wal["loader_observed_bytes"]
                or (tail_bytes == 0 and (
                    tail_reason != "none"
                    or derivative["sha256"] != source_wal["accepted_prefix_sha256"]
                ))
                or (tail_bytes > 0 and not tail_reason.startswith(
                    "valid_entries_after_selected_snapshot_boundary:"
                ))
            ):
                fail("v2 normalized derivative is not the exact Rust snapshot-bound source")
        else:
            fail("legacy WAL normalization receipt schema differs")
    if durable:
        wrapper = value.get("durable_wal_boundary")
        if not isinstance(wrapper, dict) or set(wrapper) != {"plan_sha256"}:
            fail("durable WAL boundary wrapper fields differ")
        plan_sha = require_hash(wrapper.get("plan_sha256"), "durable WAL boundary plan")
        durable_receipt = rust.get("durable_wal_boundary")
        durable_projection = validate_durable_wal_boundary_capture(
            durable_receipt, plan_sha, "Rust durable WAL boundary"
        )
        plan_projection = durable_projection["plan_projection"]
        preserved_snapshot = durable_projection["preserved_source_snapshot"]
        replay_snapshot = durable_projection["replay_derived_snapshot"]
        selected_boundary = durable_projection["selected_boundary"]
        plan_snapshot = plan_projection["source_snapshot"]
        plan_wal = plan_projection["source_wal"]
        recovery_snapshot = plan_projection.get("recovery_snapshot")
        if (
            selected_boundary.get("height") != head["height"]
            or selected_boundary.get("block_hash") != head["block_hash"]
            or selected_boundary.get("state_root") != head["state_root"]
            or (plan_snapshot["height"], plan_snapshot["state_root"])
                == (head["height"], head["state_root"])
            or plan_wal["sha256"] != source_wal["accepted_prefix_sha256"]
            or plan_wal["size"] != source_wal["accepted_prefix_bytes"]
            or source_wal["loader_observed_bytes"] != source_wal["accepted_prefix_bytes"]
            or source_wal["quarantined_suffix_bytes_at_loader"] != 0
            or source_wal["loader_tail_reason"] != "none"
            or plan_snapshot["sha256"] != source_snapshot["sha256"]
            or plan_snapshot["size"] != source_snapshot["size"]
            or preserved_snapshot["sha256"] != source_snapshot["sha256"]
            or preserved_snapshot["size"] != source_snapshot["size"]
            or replay_snapshot != fixed_snapshot
            or rust.get("allow_unbound_legacy_wal") is not True
            or (
                recovery_snapshot is not None
                and {key: recovery_snapshot[key] for key in (
                    "height", "block_hash", "state_root"
                )} != head
            )
        ):
            fail("durable WAL boundary content proof differs")
    del rust_sha
    started = parse_utc(value.get("capture_started_at"), "live source capture start")
    completed = parse_utc(value.get("capture_completed_at"), "live source capture completion")
    if completed < started:
        fail("live source capture completed before it started")
    return completed, {
        "head": dict(head), "fixed_pair_path": fixed_path,
        "source_pair_role": value["source_pair_role"],
    }


def ordered_nodes(rows: Sequence[Mapping[str, Any]], label: str) -> list[str]:
    names: list[str] = []
    for row in rows:
        if not isinstance(row, dict):
            fail(f"{label} row is not an object")
        name, host = row.get("node"), row.get("host")
        if name not in FLEET_MAP or host != FLEET_MAP.get(name):
            fail(f"{label} topology differs")
        names.append(name)
    expected = [name for name, _host in FLEET if name in set(names)]
    if names != expected or len(names) != len(set(names)):
        fail(f"{label} order/uniqueness differs")
    return names


def validate_target_height_receipt(
    value: Any, *, capture_id: str, freeze_sha256: str, targets: Sequence[str]
) -> tuple[dt.datetime, dt.datetime, int]:
    fields = {
        "schema", "source_main_commit", "freeze_plan_sha256", "capture_id",
        "started_at", "completed_at", "duration_ms", "request_policy",
        "targets", "origins", "legacy_public_max_height",
    }
    if not isinstance(value, dict) or set(value) != fields:
        fail("target public-height receipt fields differ")
    if (value.get("schema") != TARGET_HEIGHT_SCHEMA
            or value.get("capture_id") != require_hash(capture_id, "capture id")
            or value.get("freeze_plan_sha256") != require_hash(freeze_sha256, "freeze sha256")):
        fail("target public-height receipt identity differs")
    require_commit(value.get("source_main_commit"), "target public source commit")
    target_rows = value.get("targets")
    if not isinstance(target_rows, list):
        fail("target public-height targets are missing")
    names = ordered_nodes(target_rows, "target public-height targets")
    if names != list(targets) or not names:
        fail("target public-height target set differs")
    for row in target_rows:
        if set(row) != {"node", "host", "rpc_origin"}:
            fail("target public-height target fields differ")
        if row.get("rpc_origin") != f"http://{FLEET_MAP[row['node']]}:9090":
            fail("target public-height target RPC origin differs from the fixed fleet")
    origins = value.get("origins")
    if not isinstance(origins, list) or len(origins) != len(names):
        fail("target public-height origin count differs")
    origin_fields = {
        "name", "origin", "info_before_height", "latest_block_height",
        "info_after_height", "latest_block_hash", "info_before_body_sha256",
        "latest_block_body_sha256", "info_after_body_sha256",
    }
    maxima: list[int] = []
    for row, name in zip(origins, names):
        if not isinstance(row, dict) or set(row) != origin_fields or row.get("name") != name:
            fail("target public-height origin fields/order differ")
        before = require_uint(row.get("info_before_height"), f"{name} before height")
        latest = require_uint(row.get("latest_block_height"), f"{name} latest height")
        after = require_uint(row.get("info_after_height"), f"{name} after height")
        if not before <= latest <= after:
            fail(f"{name} target public-height bracket differs")
        target = next(target for target in target_rows if target["node"] == name)
        if row.get("origin") != target.get("rpc_origin"):
            fail(f"{name} target public-height origin differs from its pinned target")
        for key in (
            "latest_block_hash", "info_before_body_sha256",
            "latest_block_body_sha256", "info_after_body_sha256",
        ):
            require_hash(row.get(key), f"{name} {key}")
        maxima.append(after)
    maximum = require_uint(value.get("legacy_public_max_height"), "target public maximum")
    if maximum != max(maxima):
        fail("target public-height maximum differs")
    started = parse_utc(value.get("started_at"), "target public start")
    completed = parse_utc(value.get("completed_at"), "target public completion")
    if completed < started:
        fail("target public-height completed before start")
    require_uint(value.get("duration_ms"), "target public duration")
    policy = value.get("request_policy")
    if (not isinstance(policy, dict)
            or set(policy) != {
                "redirects", "maximum_body_bytes", "timeout_seconds",
                "proxy_environment", "sequence",
            }
            or policy.get("sequence") != ["/info", "/block/latest", "/info"]
            or policy.get("redirects") != "forbidden"
            or policy.get("maximum_body_bytes") != 1024 * 1024
            or isinstance(policy.get("timeout_seconds"), bool)
            or not isinstance(policy.get("timeout_seconds"), (int, float))
            or not 0 < policy["timeout_seconds"] <= 30
            or policy.get("proxy_environment") != "ignored"):
        fail("target public-height request policy differs")
    return started, completed, maximum


def validate_target_cross_receipt(
    value: Any,
    *,
    capture_id: str,
    freeze_sha256: str,
    targets: Sequence[str],
    public_sha256: str,
    public_receipt: Mapping[str, Any],
) -> tuple[dt.datetime, dt.datetime]:
    fields = {
        "schema", "source_main_commit", "freeze_plan_sha256", "capture_id",
        "legacy_public_height_receipt_sha256", "challenge", "started_at",
        "completed_at", "conservative_height_floor", "targets", "nodes",
    }
    if not isinstance(value, dict) or set(value) != fields:
        fail("target authenticated-height receipt fields differ")
    if (value.get("schema") != TARGET_CROSS_SCHEMA
            or value.get("capture_id") != capture_id
            or value.get("freeze_plan_sha256") != freeze_sha256
            or value.get("legacy_public_height_receipt_sha256") != public_sha256):
        fail("target authenticated-height identity differs")
    if value.get("source_main_commit") != public_receipt.get("source_main_commit"):
        fail("target authenticated-height source commit differs")
    require_commit(value.get("source_main_commit"), "target authenticated source commit")
    require_hash(value.get("challenge"), "target authenticated-height challenge")
    target_rows = value.get("targets")
    nodes = value.get("nodes")
    if not isinstance(target_rows, list) or not isinstance(nodes, list):
        fail("target authenticated-height topology is missing")
    names = ordered_nodes(target_rows, "target authenticated-height targets")
    if any(set(row) != {"node", "host", "rpc_origin"} for row in target_rows):
        fail("target authenticated-height target fields differ")
    if names != list(targets) or len(nodes) != len(names):
        fail("target authenticated-height target set differs")
    if target_rows != public_receipt.get("targets"):
        fail("target authenticated-height pinned target set differs from public receipt")
    node_fields = {
        "node", "host", "writer_pid", "writer_start_ticks", "boot_id",
        "writer_cgroup_sha256", "public_info_after_height",
        "public_latest_block_height", "public_latest_block_hash",
        "loopback_info_before_height", "loopback_latest_height",
        "loopback_info_after_height", "loopback_latest_block_hash",
        "response_sha256",
    }
    floors: list[int] = []
    public_by_name = {row["name"]: row for row in public_receipt["origins"]}
    for row, name in zip(nodes, names):
        if (not isinstance(row, dict) or set(row) != node_fields
                or row.get("node") != name or row.get("host") != FLEET_MAP[name]):
            fail("target authenticated-height node fields/order differ")
        require_uint(row.get("writer_pid"), f"{name} writer pid", positive=True)
        require_uint(row.get("writer_start_ticks"), f"{name} writer start", positive=True)
        require_hash(row.get("writer_cgroup_sha256"), f"{name} writer cgroup")
        if not isinstance(row.get("boot_id"), str) or UUID_RE.fullmatch(row["boot_id"]) is None:
            fail(f"{name} authenticated-height boot id differs")
        public_after = require_uint(row.get("public_info_after_height"), f"{name} public after")
        public_latest = require_uint(row.get("public_latest_block_height"), f"{name} public latest")
        loop_before = require_uint(row.get("loopback_info_before_height"), f"{name} loop before")
        loop_latest = require_uint(row.get("loopback_latest_height"), f"{name} loop latest")
        loop_after = require_uint(row.get("loopback_info_after_height"), f"{name} loop after")
        if not public_latest <= public_after <= loop_before <= loop_latest <= loop_after:
            fail(f"{name} authenticated-height bracket differs")
        require_hash(row.get("public_latest_block_hash"), f"{name} public hash")
        require_hash(row.get("loopback_latest_block_hash"), f"{name} loopback hash")
        public_row = public_by_name[name]
        if (row.get("public_info_after_height") != public_row["info_after_height"]
                or row.get("public_latest_block_height") != public_row["latest_block_height"]
                or row.get("public_latest_block_hash") != public_row["latest_block_hash"]):
            fail(f"{name} authenticated-height public tuple differs from sampled origin")
        responses = row.get("response_sha256")
        if (not isinstance(responses, dict)
                or set(responses) != {"/info:before", "/block/latest", "/info:after"}):
            fail(f"{name} authenticated-height response roots are missing")
        for key, response_sha in responses.items():
            if not isinstance(key, str):
                fail(f"{name} authenticated-height response key differs")
            require_hash(response_sha, f"{name} authenticated-height response")
        floors.append(loop_before)
    floor = require_uint(value.get("conservative_height_floor"), "target conservative floor")
    if floor != min(floors):
        fail("target authenticated-height conservative floor differs")
    started = parse_utc(value.get("started_at"), "target authenticated start")
    completed = parse_utc(value.get("completed_at"), "target authenticated completion")
    if completed < started:
        fail("target authenticated-height completed before start")
    return started, completed


def validate_node_applied(
    value: Any,
    *,
    authorization_sha256: str | None = None,
    node: str | None = None,
) -> tuple[dt.datetime, int]:
    fields = {
        "schema", "capture_id", "freeze_plan_sha256", "round_authorization_sha256",
        "round_readiness_sha256", "round_number", "node", "host", "boot_id", "writer_pid",
        "writer_start_ticks", "writer_cgroup_sha256", "nft_policy_source_sha256",
        "owned_ruleset_stateless_sha256",
        "nft_applied_at", "nft_deadline_gate", "network_quarantine_receipt",
        "network_quarantine_receipt_sha256", "stable_head",
        "authorization_ancestry_proof", "persistent_restart_fence_sha256",
    }
    if not isinstance(value, dict) or set(value) != fields:
        fail("node nft-applied receipt fields differ")
    if value.get("schema") != NODE_APPLIED_SCHEMA:
        fail("node nft-applied schema differs")
    name = value.get("node")
    if name not in FLEET_MAP or value.get("host") != FLEET_MAP[name] or (node and name != node):
        fail("node nft-applied topology differs")
    require_hash(value.get("capture_id"), "node nft-applied capture")
    require_hash(value.get("freeze_plan_sha256"), "node nft-applied freeze")
    auth_sha = require_hash(value.get("round_authorization_sha256"), "node nft-applied auth")
    require_hash(value.get("round_readiness_sha256"), "node nft-applied readiness")
    if authorization_sha256 is not None and auth_sha != authorization_sha256:
        fail("node nft-applied authorization root differs")
    require_uint(value.get("round_number"), "node nft-applied round", positive=True)
    require_uint(value.get("writer_pid"), "node nft-applied writer pid", positive=True)
    require_uint(value.get("writer_start_ticks"), "node nft-applied writer start", positive=True)
    require_hash(value.get("writer_cgroup_sha256"), "node nft-applied writer cgroup")
    require_hash(value.get("nft_policy_source_sha256"), "node nft-applied policy source")
    require_hash(
        value.get("owned_ruleset_stateless_sha256"),
        "node nft-applied observed stateless ruleset",
    )
    network_receipt, network_sha = validate_wrapper(
        value.get("network_quarantine_receipt"), "node network-quarantine receipt"
    )
    network_fields = {
        "schema", "capture_id", "node", "host", "freeze_plan_sha256",
        "source_main_commit", "round_number", "round_authorization_sha256",
        "round_readiness_sha256", "nft_deadline_gate_sha256",
        "nft_apply_intent_sha256", "nft_apply_intent",
        "nft_table_binding_sha256", "nft_table_binding", "table_comment",
        "nft_table_comment", "nft_policy_source_sha256", "apply_helper_sha256",
        "applied_commit_sha256", "authorization_ancestry_proof_sha256",
        "boot_id", "writer", "table", "quarantine_policy", "persistence",
        "file_sha256", "tool_sha256", "owned_ruleset_stateless_sha256",
        "loopback_head", "stable_head", "authorization_ancestry_proof",
        "nft_deadline_gate", "applied_commit", "installed_at",
        "global_absence_claimed", "threat_model",
    }
    if (network_sha != require_hash(
            value.get("network_quarantine_receipt_sha256"), "node network receipt"
        ) or set(network_receipt) != network_fields
            or network_receipt.get("schema") != "arc.recovery.legacy-network-quarantine.v1"
            or (network_receipt.get("capture_id"), network_receipt.get("freeze_plan_sha256"),
                network_receipt.get("node"), network_receipt.get("host"),
                network_receipt.get("round_number")) != (
                    value.get("capture_id"), value.get("freeze_plan_sha256"), name,
                    value.get("host"), value.get("round_number")
                ) or network_receipt.get("owned_ruleset_stateless_sha256")
                    != value.get("owned_ruleset_stateless_sha256")
                or not isinstance(network_receipt.get("file_sha256"), dict)
                or network_receipt["file_sha256"].get("policy.nft")
                    != value.get("nft_policy_source_sha256")):
        fail("node network-quarantine receipt roots differ")
    require_commit(network_receipt.get("source_main_commit"), "network receipt source commit")
    if network_receipt.get("global_absence_claimed") is not False:
        fail("node network-quarantine receipt claims global absence")
    if (
        network_receipt.get("boot_id") != value.get("boot_id")
        or network_receipt.get("writer") != {
            "pid": value.get("writer_pid"),
            "start_ticks": value.get("writer_start_ticks"),
            "cgroup_sha256": value.get("writer_cgroup_sha256"),
        }
    ):
        fail("node network-quarantine writer identity differs")
    expected_table = {
        "family": "inet", "name": "arc_legacy_maintenance_v1", "priority": -310,
        "hooks": ["prerouting", "input", "forward", "output"],
        "policy": "accept", "comment": network_receipt.get("nft_table_comment"),
        "loopback_retained": True,
    }
    if network_receipt.get("table") != expected_table:
        fail("node network-quarantine table contract differs")
    policy = network_receipt.get("quarantine_policy")
    if (
        not isinstance(policy, dict)
        or set(policy) != {
            "mode", "families", "directions", "allowed", "priority_before_conntrack",
            "established_bypass", "legacy_rpc_p2p_web_dynamic_all_blocked",
        }
        or policy.get("mode") != "deny-all-nonloopback-except-host-maintenance"
        or policy.get("families") != ["ipv4", "ipv6"]
        or policy.get("directions") != ["input", "output", "forward"]
        or policy.get("allowed") != [
            "loopback", "ssh-tcp-22", "dhcpv4-67-68", "dhcpv6-546-547",
            "icmpv6-ndp-ra-packet-too-big",
        ]
        or policy.get("priority_before_conntrack") is not True
        or policy.get("established_bypass") is not False
        or policy.get("legacy_rpc_p2p_web_dynamic_all_blocked") is not True
        or not isinstance(policy.get("allowed"), list)
    ):
        fail("node network-quarantine policy differs")
    persistence = network_receipt.get("persistence")
    if (
        not isinstance(persistence, dict)
        or set(persistence) != {
            "unit_path", "unit_enabled", "unit_active", "state_path",
            "active_selector_path", "automatic_unfence",
        }
        or persistence.get("unit_path")
            != "/etc/systemd/system/arc-legacy-maintenance-fence.service"
        or persistence.get("unit_enabled") is not True
        or persistence.get("unit_active") is not True
        or persistence.get("active_selector_path")
            != "/run/arc-recovery/active-network-fence"
        or persistence.get("automatic_unfence") is not False
        or not isinstance(persistence.get("state_path"), str)
    ):
        fail("node network-quarantine persistence differs")
    for collection in ("file_sha256", "tool_sha256"):
        roots = network_receipt.get(collection)
        if not isinstance(roots, dict) or not roots:
            fail(f"node network-quarantine {collection} differs")
        for root in roots.values():
            require_hash(root, f"node network-quarantine {collection} root")
    threat = network_receipt.get("threat_model")
    if (
        not isinstance(threat, dict)
        or set(threat) != {"legacy_binary", "legacy_binary_sha256"}
        or threat.get("legacy_binary") != "reviewed-non-adversarial-exact-hash"
    ):
        fail("node network-quarantine threat model differs")
    require_hash(threat.get("legacy_binary_sha256"), "node legacy binary root")
    restart = require_hash(
        value.get("persistent_restart_fence_sha256"), "node persistent restart fence"
    )
    head = value.get("stable_head")
    if not isinstance(head, dict) or set(head) != {"height", "block_hash", "state_root"}:
        fail("node nft-applied stable head differs")
    height = require_uint(head.get("height"), "node nft-applied stable height", positive=True)
    require_hash(head.get("block_hash"), "node nft-applied block hash")
    require_hash(head.get("state_root"), "node nft-applied state root")
    loopback = network_receipt.get("loopback_head")
    if (not isinstance(loopback, dict)
            or set(loopback) != {
                "rpc_origin", "info_before_height", "latest_height", "block_height",
                "info_after_height", "block_hash", "state_root", "response_sha256",
                "stable_attempt",
            }
            or (loopback.get("latest_height"), loopback.get("block_hash"), loopback.get("state_root"))
                != (head["height"], head["block_hash"], head["state_root"])):
        fail("node nft-applied stable head does not derive from network receipt")
    if (
        len({require_uint(loopback.get(field), f"node loopback {field}") for field in (
            "info_before_height", "latest_height", "block_height", "info_after_height",
        )}) != 1
        or not isinstance(loopback.get("rpc_origin"), str)
        or require_uint(loopback.get("stable_attempt"), "node loopback stable attempt", positive=True) > 10
    ):
        fail("node network-quarantine loopback bracket differs")
    responses = loopback.get("response_sha256")
    if not isinstance(responses, dict) or set(responses) != {
        "/info:before", "/block/latest", f"/block/{loopback['latest_height']}",
        "/health", "/info:after",
    }:
        fail("node network-quarantine loopback response roots differ")
    for root in responses.values():
        require_hash(root, "node network-quarantine loopback response root")
    gate, gate_sha = validate_wrapper(value.get("nft_deadline_gate"), "node nft deadline gate")
    gate_fields = {
        "schema", "capture_id", "freeze_plan_sha256", "round_authorization_sha256",
        "round_readiness_sha256", "round_number", "node", "host",
        "authorization_deadline", "invoked_at",
        "apply_helper_sha256", "policy_sha256", "table_binding_sha256", "table_comment",
    }
    if (set(gate) != gate_fields or gate.get("schema") != NFT_GATE_SCHEMA
            or (gate.get("capture_id"), gate.get("freeze_plan_sha256"),
                gate.get("round_authorization_sha256"), gate.get("round_number"),
                gate.get("round_readiness_sha256"), gate.get("node"), gate.get("host"),
                gate.get("policy_sha256")) != (
                    value.get("capture_id"), value.get("freeze_plan_sha256"), auth_sha,
                    value.get("round_number"), value.get("round_readiness_sha256"),
                    name, value.get("host"),
                    value.get("nft_policy_source_sha256")
                )):
        fail("node nft deadline gate binding differs")
    helper_sha = require_hash(gate.get("apply_helper_sha256"), "node nft apply helper")
    binding = {
        "schema": TABLE_BINDING_SCHEMA,
        "capture_id": value.get("capture_id"),
        "freeze_plan_sha256": value.get("freeze_plan_sha256"),
        "round_number": value.get("round_number"),
        "round_authorization_sha256": auth_sha,
        "round_readiness_sha256": value.get("round_readiness_sha256"),
        "authorization_deadline": gate.get("authorization_deadline"),
        "apply_helper_sha256": helper_sha,
        "policy_sha256": value.get("nft_policy_source_sha256"),
        "node": name,
        "host": value.get("host"),
        "writer": {
            "boot_id": value.get("boot_id"), "pid": value.get("writer_pid"),
            "start_ticks": value.get("writer_start_ticks"),
            "cgroup_sha256": value.get("writer_cgroup_sha256"),
        },
    }
    binding_sha = digest(binding)
    if (
        gate.get("table_binding_sha256") != binding_sha
        or gate.get("table_comment")
            != f"arc-recovery:round={value.get('round_number')}:bind={binding_sha}:node={name}"
    ):
        fail("node nft table binding/comment differs")
    if (network_receipt.get("nft_table_binding") != binding
            or network_receipt.get("table_comment") != gate.get("table_comment")
            or network_receipt.get("nft_policy_source_sha256")
                != value.get("nft_policy_source_sha256")
            or network_receipt.get("apply_helper_sha256") != helper_sha):
        fail("node network-quarantine table/helper binding differs")
    network_files = network_receipt.get("file_sha256")
    base_file_keys = {
        "authorization.json", "readiness.json", "contract.json", "table-binding.json",
        "nft-apply-intent.json", "policy.nft", "apply", "nft",
        "nft-deadline-gate.json", "applied.commit.json",
        "persistent-restart-fence.json", "rendered-policy.nft",
        "/usr/local/libexec/arc-legacy-maintenance-fence",
        "/etc/systemd/system/arc-legacy-maintenance-fence.service",
        "/etc/systemd/system/arc-self-heal.service.d/zzzy-arc-recovery-network-fence.conf",
        "/etc/systemd/system/arc-node.service.d/zzzy-arc-recovery-network-fence.conf",
        "/etc/systemd/system/arc-node-update.service.d/zzzy-arc-recovery-network-fence.conf",
        "/etc/systemd/system/arc-node-update.timer.d/zzzy-arc-recovery-network-fence.conf",
    }
    if (
        not isinstance(network_files, dict)
        or set(network_files) not in (base_file_keys, base_file_keys | {"preexisting-ruleset.json"})
        or network_files.get("apply") != helper_sha
        or network_files.get("policy.nft") != value.get("nft_policy_source_sha256")
        or network_files.get("authorization.json") != auth_sha
        or network_files.get("readiness.json") != value.get("round_readiness_sha256")
        or network_files.get("table-binding.json") != binding_sha
        or network_files.get("nft-deadline-gate.json") != gate_sha
        or network_files.get("applied.commit.json")
            != network_receipt.get("applied_commit_sha256")
        or network_files.get("persistent-restart-fence.json") != restart
        or network_files.get("nft")
            != (network_receipt.get("tool_sha256") or {}).get("/usr/sbin/nft")
        or network_receipt.get("round_authorization_sha256") != auth_sha
        or network_receipt.get("round_readiness_sha256")
            != value.get("round_readiness_sha256")
        or network_receipt.get("nft_deadline_gate_sha256") != gate_sha
        or network_receipt.get("nft_table_binding_sha256") != binding_sha
        or network_receipt.get("nft_table_comment") != gate.get("table_comment")
    ):
        fail("node network-quarantine receipt does not bind the gated nft apply")
    intent, intent_sha = validate_wrapper(
        network_receipt.get("nft_apply_intent"), "network receipt nft apply intent"
    )
    intent_fields = {
        "schema", "capture_id", "freeze_plan_sha256", "source_main_commit",
        "round_number", "round_authorization_sha256", "round_readiness_sha256",
        "authorization_deadline", "node", "host", "writer",
        "table_binding_sha256", "table_comment", "apply_helper_sha256",
        "nft_policy_source_sha256", "prepared_at",
    }
    if (
        set(intent) != intent_fields
        or intent.get("schema") != NFT_INTENT_SCHEMA
        or (
            intent.get("capture_id"), intent.get("freeze_plan_sha256"),
            intent.get("source_main_commit"), intent.get("round_number"),
            intent.get("round_authorization_sha256"),
            intent.get("round_readiness_sha256"), intent.get("authorization_deadline"),
            intent.get("node"), intent.get("host"), intent.get("writer"),
            intent.get("table_binding_sha256"), intent.get("table_comment"),
            intent.get("apply_helper_sha256"), intent.get("nft_policy_source_sha256"),
        ) != (
            network_receipt.get("capture_id"), network_receipt.get("freeze_plan_sha256"),
            network_receipt.get("source_main_commit"), value.get("round_number"), auth_sha,
            value.get("round_readiness_sha256"), gate.get("authorization_deadline"),
            name, value.get("host"), binding["writer"], binding_sha,
            gate.get("table_comment"), helper_sha, value.get("nft_policy_source_sha256"),
        )
        or network_receipt.get("nft_apply_intent_sha256") != intent_sha
        or network_files.get("nft-apply-intent.json") != intent_sha
    ):
        fail("node network-quarantine nft apply intent roots differ")
    prepared_at = parse_utc(intent.get("prepared_at"), "node nft apply-intent preparation")
    applied_at = parse_utc(value.get("nft_applied_at"), "node nft-applied time")
    if prepared_at > applied_at:
        fail("node nft apply intent was prepared after its gated invocation")
    if parse_utc(gate.get("invoked_at"), "node nft gate invocation") != applied_at:
        fail("node nft-applied time differs from gated invocation")
    installed_at = parse_utc(
        network_receipt.get("installed_at"), "node network-quarantine installed time"
    )
    if installed_at < applied_at:
        fail("node network-quarantine receipt predates the gated nft invocation")
    ancestry, _ancestry_sha = validate_wrapper(
        value.get("authorization_ancestry_proof"),
        "node post-fence authorization ancestry proof",
    )
    if (
        set(ancestry) != {
            "schema", "capture_id", "freeze_plan_sha256",
            "round_authorization_sha256", "round_number", "node", "host", "checks",
        }
        or ancestry.get("schema") != ANCESTRY_SCHEMA
        or (
            ancestry.get("capture_id"), ancestry.get("freeze_plan_sha256"),
            ancestry.get("round_authorization_sha256"), ancestry.get("round_number"),
            ancestry.get("node"), ancestry.get("host"),
        ) != (
            value.get("capture_id"), value.get("freeze_plan_sha256"), auth_sha,
            value.get("round_number"), name, value.get("host"),
        )
    ):
        fail("node post-fence authorization ancestry identity differs")
    checks = ancestry.get("checks")
    if not isinstance(checks, list) or len(checks) != 2:
        fail("node post-fence authorization ancestry checks differ")
    for check in checks:
        if not isinstance(check, dict) or set(check) != {
            "label", "height", "expected_block_hash", "observed_block_hash",
            "response_sha256",
        }:
            fail("node post-fence authorization ancestry check fields differ")
        require_uint(check.get("height"), "node ancestry height", positive=True)
        expected = require_hash(check.get("expected_block_hash"), "node ancestry expected hash")
        if require_hash(check.get("observed_block_hash"), "node ancestry observed hash") != expected:
            fail("node post-fence authorization ancestry hash differs")
        require_hash(check.get("response_sha256"), "node ancestry response root")
    embedded_gate, embedded_gate_sha = validate_wrapper(
        network_receipt.get("nft_deadline_gate"), "network receipt deadline gate"
    )
    embedded_ancestry, embedded_ancestry_sha = validate_wrapper(
        network_receipt.get("authorization_ancestry_proof"),
        "network receipt ancestry proof",
    )
    if (embedded_gate != gate or embedded_gate_sha != gate_sha
            or embedded_ancestry != ancestry or embedded_ancestry_sha != _ancestry_sha
            or network_receipt.get("authorization_ancestry_proof_sha256")
                != _ancestry_sha):
        fail("node network-quarantine embedded gate/ancestry roots differ")
    commit, commit_sha = validate_wrapper(
        network_receipt.get("applied_commit"), "network receipt applied commit"
    )
    commit_fields = {
        "schema", "capture_id", "freeze_plan_sha256", "round_number",
        "round_authorization_sha256", "round_readiness_sha256", "node", "host",
        "nft_deadline_gate_sha256", "table_binding_sha256", "table_comment",
        "apply_helper_sha256", "nft_policy_source_sha256",
        "owned_ruleset_stateless_sha256", "nft_applied_at",
    }
    if (set(commit) != commit_fields
            or commit.get("schema") != "arc.recovery.quarantine-nft-applied-commit.v1"
            or (commit.get("capture_id"), commit.get("freeze_plan_sha256"),
                commit.get("round_number"), commit.get("round_authorization_sha256"),
                commit.get("round_readiness_sha256"), commit.get("node"),
                commit.get("host")) != (
                    value.get("capture_id"), value.get("freeze_plan_sha256"),
                    value.get("round_number"), auth_sha,
                    value.get("round_readiness_sha256"), name, value.get("host")
                )
            or commit.get("nft_deadline_gate_sha256") != gate_sha
            or commit.get("table_binding_sha256") != binding_sha
            or commit.get("table_comment") != gate.get("table_comment")
            or commit.get("apply_helper_sha256") != helper_sha
            or commit.get("nft_policy_source_sha256")
                != value.get("nft_policy_source_sha256")
            or commit.get("owned_ruleset_stateless_sha256")
                != value.get("owned_ruleset_stateless_sha256")
            or commit.get("nft_applied_at") != value.get("nft_applied_at")
            or network_receipt.get("applied_commit_sha256") != commit_sha):
        fail("node network-quarantine applied commit roots differ")
    if network_receipt.get("stable_head") != head:
        fail("node network-quarantine stable-head projection differs")
    return applied_at, height


def validate_node_stopped_precommit(
    value: Any,
    *,
    authorization_sha256: str | None = None,
    node: str | None = None,
) -> tuple[dt.datetime, dt.datetime, int]:
    """Validate the fail-closed stopped-writer path without a live RPC claim.

    This terminal is deliberately distinct from an nft-applied receipt.  It
    proves the authorized writer is stably gone after authorization expiry, every automatic
    restart source is pinned behind the exact fail-closed unit, and an offline
    inspector derives the final head plus both authorization ancestry checks
    from stable source inputs.  No late nft application is permitted.
    """

    fields = {
        "schema", "capture_id", "freeze_plan_sha256", "source_main_commit",
        "round_number", "round_authorization_sha256", "round_readiness_sha256",
        "node", "host", "authorized_writer", "authorization_deadline",
        "nft_apply_intent", "nft_deadline_gate", "applied_commit", "nft_table_binding",
        "apply_helper_sha256", "nft_policy_source_sha256", "persistence_plan",
        "persistent_restart_fence", "precommit_status", "persisted_head",
        "stable_head", "authorization_ancestry_proof", "current_boot_id",
        "restart_fence_armed_at", "secured_at", "writer_state",
        "network_quarantine_active", "nft_table_absent", "applied_commit_absent",
        "active_selector_absent", "automatic_legacy_restart",
    }
    if not isinstance(value, dict) or set(value) != fields:
        fail("persistently-stopped transition fields differ")
    if value.get("schema") != NODE_STOPPED_PRECOMMIT_SCHEMA:
        fail("persistently-stopped transition schema differs")
    name = value.get("node")
    if name not in FLEET_MAP or value.get("host") != FLEET_MAP[name] or (node and name != node):
        fail("persistently-stopped transition topology differs")
    capture = require_hash(value.get("capture_id"), "persistently-stopped capture")
    freeze = require_hash(value.get("freeze_plan_sha256"), "persistently-stopped freeze")
    source = require_commit(value.get("source_main_commit"), "persistently-stopped source")
    auth_sha = require_hash(
        value.get("round_authorization_sha256"), "persistently-stopped authorization"
    )
    ready_sha = require_hash(
        value.get("round_readiness_sha256"), "persistently-stopped readiness"
    )
    if authorization_sha256 is not None and auth_sha != authorization_sha256:
        fail("persistently-stopped authorization root differs")
    round_number = require_uint(
        value.get("round_number"), "persistently-stopped round", positive=True
    )
    authorized_writer = validate_writer(
        value.get("authorized_writer"), "persistently-stopped authorized writer"
    )
    current_boot = value.get("current_boot_id")
    if (
        not isinstance(current_boot, str)
        or UUID_RE.fullmatch(current_boot) is None
    ):
        fail("persistently-stopped transition boot id differs")
    reboot_after_intent = current_boot != authorized_writer["boot_id"]
    deadline = parse_utc(
        value.get("authorization_deadline"), "persistently-stopped authorization deadline"
    )
    if (
        value.get("writer_state") != "persistently-stopped"
        or value.get("network_quarantine_active") is not False
        or value.get("nft_table_absent") is not True
        or value.get("applied_commit_absent") is not (value.get("applied_commit") is None)
        or value.get("active_selector_absent") is not True
        or value.get("automatic_legacy_restart") is not False
    ):
        fail("persistently-stopped transition state differs")
    helper_sha = require_hash(
        value.get("apply_helper_sha256"), "persistently-stopped apply helper"
    )
    policy_sha = require_hash(
        value.get("nft_policy_source_sha256"), "persistently-stopped policy source"
    )

    binding, binding_sha = validate_wrapper(
        value.get("nft_table_binding"), "persistently-stopped table binding"
    )
    binding_fields = {
        "schema", "capture_id", "freeze_plan_sha256", "round_number",
        "round_authorization_sha256", "round_readiness_sha256",
        "authorization_deadline", "apply_helper_sha256", "policy_sha256",
        "node", "host", "writer",
    }
    expected_binding = {
        "schema": TABLE_BINDING_SCHEMA,
        "capture_id": capture,
        "freeze_plan_sha256": freeze,
        "round_number": round_number,
        "round_authorization_sha256": auth_sha,
        "round_readiness_sha256": ready_sha,
        "authorization_deadline": value["authorization_deadline"],
        "apply_helper_sha256": helper_sha,
        "policy_sha256": policy_sha,
        "node": name,
        "host": value["host"],
        "writer": authorized_writer,
    }
    if set(binding) != binding_fields or binding != expected_binding:
        fail("persistently-stopped table binding differs")
    table_comment = f"arc-recovery:round={round_number}:bind={binding_sha}:node={name}"

    intent, intent_sha = validate_wrapper(
        value.get("nft_apply_intent"), "persistently-stopped nft apply intent"
    )
    intent_fields = {
        "schema", "capture_id", "freeze_plan_sha256", "source_main_commit",
        "round_number", "round_authorization_sha256", "round_readiness_sha256",
        "authorization_deadline", "node", "host", "writer",
        "table_binding_sha256", "table_comment", "apply_helper_sha256",
        "nft_policy_source_sha256", "prepared_at",
    }
    if (
        set(intent) != intent_fields
        or intent.get("schema") != NFT_INTENT_SCHEMA
        or (
            intent.get("capture_id"), intent.get("freeze_plan_sha256"),
            intent.get("source_main_commit"), intent.get("round_number"),
            intent.get("round_authorization_sha256"),
            intent.get("round_readiness_sha256"), intent.get("authorization_deadline"),
            intent.get("node"), intent.get("host"), intent.get("writer"),
            intent.get("table_binding_sha256"), intent.get("table_comment"),
            intent.get("apply_helper_sha256"), intent.get("nft_policy_source_sha256"),
        )
        != (
            capture, freeze, source, round_number, auth_sha, ready_sha,
            value["authorization_deadline"], name, value["host"], authorized_writer,
            binding_sha, table_comment, helper_sha, policy_sha,
        )
    ):
        fail("persistently-stopped nft apply intent differs")
    intent_prepared = parse_utc(
        intent.get("prepared_at"), "persistently-stopped apply-intent preparation"
    )

    gate_wrapper = value.get("nft_deadline_gate")
    if gate_wrapper is not None:
        gate, _gate_sha = validate_wrapper(
            gate_wrapper, "persistently-stopped nft deadline gate"
        )
        gate_fields = {
            "schema", "capture_id", "freeze_plan_sha256",
            "round_authorization_sha256", "round_readiness_sha256",
            "round_number", "node", "host", "authorization_deadline",
            "invoked_at", "apply_helper_sha256", "policy_sha256",
            "table_binding_sha256", "table_comment",
        }
        if (
            set(gate) != gate_fields
            or gate.get("schema") != NFT_GATE_SCHEMA
            or (
                gate.get("capture_id"), gate.get("freeze_plan_sha256"),
                gate.get("round_authorization_sha256"),
                gate.get("round_readiness_sha256"), gate.get("round_number"),
                gate.get("node"), gate.get("host"), gate.get("authorization_deadline"),
                gate.get("apply_helper_sha256"), gate.get("policy_sha256"),
                gate.get("table_binding_sha256"), gate.get("table_comment"),
            )
            != (
                capture, freeze, auth_sha, ready_sha, round_number, name, value["host"],
                value["authorization_deadline"], helper_sha, policy_sha,
                binding_sha, table_comment,
            )
        ):
            fail("persistently-stopped nft deadline gate differs")
        gate_invoked = parse_utc(
            gate.get("invoked_at"), "persistently-stopped nft gate invocation"
        )
        if not intent_prepared <= gate_invoked:
            fail("persistently-stopped nft gate chronology differs")

    commit_wrapper = value.get("applied_commit")
    commit_sha: str | None = None
    if commit_wrapper is not None:
        commit, commit_sha = validate_wrapper(
            commit_wrapper, "persistently-stopped applied commit"
        )
        commit_fields = {
            "schema", "capture_id", "freeze_plan_sha256", "round_number",
            "round_authorization_sha256", "round_readiness_sha256", "node", "host",
            "nft_deadline_gate_sha256", "table_binding_sha256", "table_comment",
            "apply_helper_sha256", "nft_policy_source_sha256",
            "owned_ruleset_stateless_sha256", "nft_applied_at",
        }
        if (
            gate_wrapper is None
            or set(commit) != commit_fields
            or commit.get("schema") != "arc.recovery.quarantine-nft-applied-commit.v1"
            or (
                commit.get("capture_id"), commit.get("freeze_plan_sha256"),
                commit.get("round_number"), commit.get("round_authorization_sha256"),
                commit.get("round_readiness_sha256"), commit.get("node"),
                commit.get("host"), commit.get("table_binding_sha256"),
                commit.get("table_comment"), commit.get("apply_helper_sha256"),
                commit.get("nft_policy_source_sha256"),
            )
            != (
                capture, freeze, round_number, auth_sha, ready_sha, name,
                value["host"], binding_sha, table_comment, helper_sha, policy_sha,
            )
            or commit.get("nft_deadline_gate_sha256") != gate_wrapper["sha256"]
            or commit.get("nft_applied_at") != gate_wrapper["value"].get("invoked_at")
        ):
            fail("persistently-stopped applied commit differs")
        require_hash(
            commit.get("owned_ruleset_stateless_sha256"),
            "persistently-stopped applied ruleset",
        )

    plan, plan_sha = validate_wrapper(
        value.get("persistence_plan"), "persistently-stopped persistence plan"
    )
    plan_fields = {
        "schema", "capture_id", "freeze_plan_sha256", "source_main_commit",
        "round_number", "round_authorization_sha256", "round_readiness_sha256",
        "authorization_deadline", "node", "host", "authorized_writer",
        "authorized_supervisor", "legacy_start_allow_path",
        "legacy_start_allow_absent",
        "nft_apply_intent_sha256", "table_binding_sha256", "apply_helper_sha256",
        "nft_policy_source_sha256", "files", "fence_unit", "fence_unit_enabled",
        "active_selector_published_only_after_applied_commit",
        "missing_selector_behavior", "automatic_unfence", "prepared_at",
    }
    if (
        set(plan) != plan_fields
        or plan.get("schema") != PERSISTENCE_PLAN_SCHEMA
        or (
            plan.get("capture_id"), plan.get("freeze_plan_sha256"),
            plan.get("source_main_commit"), plan.get("round_number"),
            plan.get("round_authorization_sha256"), plan.get("round_readiness_sha256"),
            plan.get("authorization_deadline"), plan.get("node"), plan.get("host"),
            plan.get("authorized_writer"), plan.get("nft_apply_intent_sha256"),
            plan.get("table_binding_sha256"), plan.get("apply_helper_sha256"),
            plan.get("nft_policy_source_sha256"),
        )
        != (
            capture, freeze, source, round_number, auth_sha, ready_sha,
            value["authorization_deadline"], name, value["host"], authorized_writer,
            intent_sha, binding_sha, helper_sha, policy_sha,
        )
        or plan.get("fence_unit") != "arc-legacy-maintenance-fence.service"
        or plan.get("legacy_start_allow_path")
            != "/etc/arc-recovery/quarantine-round-legacy-start.allow"
        or plan.get("legacy_start_allow_absent") is not True
        or plan.get("fence_unit_enabled") is not True
        or plan.get("active_selector_published_only_after_applied_commit") is not True
        or plan.get("missing_selector_behavior") != "fail-closed-without-nft-apply"
        or plan.get("automatic_unfence") is not False
    ):
        fail("persistently-stopped persistence plan differs")
    plan_prepared = parse_utc(
        plan.get("prepared_at"), "persistently-stopped persistence-plan preparation"
    )
    if not intent_prepared <= plan_prepared:
        fail("persistently-stopped persistence-plan chronology differs")
    supervisor = plan.get("authorized_supervisor")
    supervisor_fields = {
        "mode", "unit", "main_pid", "start_ticks", "executable_path",
        "executable_sha256", "argv_sha256", "context_sha256",
        "prepare_barrier_sha256",
    }
    if (
        not isinstance(supervisor, dict)
        or set(supervisor) != supervisor_fields
        or supervisor.get("mode") not in {"systemd-unit", "detached-root-session"}
        or supervisor.get("unit") not in {"arc-self-heal.service", "arc-node.service"}
        or require_uint(
            supervisor.get("main_pid"), "persistently-stopped supervisor pid", positive=True
        ) == 0
        or require_uint(
            supervisor.get("start_ticks"), "persistently-stopped supervisor start", positive=True
        ) == 0
        or not isinstance(supervisor.get("executable_path"), str)
        or not supervisor["executable_path"].startswith("/")
    ):
        fail("persistently-stopped authorized supervisor differs")
    for key in (
        "executable_sha256", "argv_sha256", "context_sha256", "prepare_barrier_sha256"
    ):
        require_hash(supervisor.get(key), f"persistently-stopped supervisor {key}")
    files = plan.get("files")
    if not isinstance(files, dict) or set(files) != {"dispatcher", "unit", "dependencies"}:
        fail("persistently-stopped persistence file inventory differs")
    dispatcher = files.get("dispatcher")
    unit = files.get("unit")
    dependencies = files.get("dependencies")
    if (
        not isinstance(dispatcher, dict)
        or set(dispatcher) != {"path", "sha256", "mode"}
        or dispatcher.get("path") != "/usr/local/libexec/arc-legacy-maintenance-fence"
        or dispatcher.get("mode") != 0o500
        or not isinstance(unit, dict)
        or set(unit) != {"path", "sha256", "mode"}
        or unit.get("path")
        != "/etc/systemd/system/arc-legacy-maintenance-fence.service"
        or unit.get("mode") != 0o400
    ):
        fail("persistently-stopped dispatcher/unit inventory differs")
    require_hash(dispatcher.get("sha256"), "persistently-stopped dispatcher")
    require_hash(unit.get("sha256"), "persistently-stopped fence unit")
    legacy_units = [supervisor["unit"]]
    legacy_units.extend(legacy for legacy in (
        "arc-self-heal.service", "arc-node.service",
        "arc-node-update.service", "arc-node-update.timer",
    ) if legacy not in legacy_units)
    expected_dependency_paths = [
        f"/etc/systemd/system/{legacy}.d/zzzy-arc-recovery-network-fence.conf"
        for legacy in legacy_units
    ]
    if (
        not isinstance(dependencies, list)
        or [row.get("path") if isinstance(row, dict) else None for row in dependencies]
        != expected_dependency_paths
    ):
        fail("persistently-stopped legacy activation-source inventory differs")
    for row in dependencies:
        if set(row) != {"path", "sha256", "mode"} or row.get("mode") != 0o400:
            fail("persistently-stopped dependency inventory fields differ")
        require_hash(row.get("sha256"), "persistently-stopped dependency root")

    barrier, barrier_sha = validate_wrapper(
        value.get("persistent_restart_fence"),
        "persistently-stopped persistent restart fence",
    )
    barrier_fields = {
        "schema", "capture_id", "freeze_plan_sha256", "source_main_commit",
        "round_number", "round_authorization_sha256", "round_readiness_sha256",
        "node", "host", "dispatcher_sha256", "authorization_deadline",
        "nft_apply_intent_sha256", "persistence_plan_sha256", "authorized_writer",
        "unit_sha256", "dependency_sha256", "automatic_unfence",
        "precommit_missing_selector_behavior", "armed_at", "arming_mode",
        "armed_boot_id",
    }
    dependency_roots = {row["path"]: row["sha256"] for row in dependencies}
    if (
        set(barrier) != barrier_fields
        or barrier.get("schema") != PERSISTENT_FENCE_SCHEMA
        or (
            barrier.get("capture_id"), barrier.get("freeze_plan_sha256"),
            barrier.get("source_main_commit"), barrier.get("round_number"),
            barrier.get("round_authorization_sha256"),
            barrier.get("round_readiness_sha256"), barrier.get("node"),
            barrier.get("host"), barrier.get("dispatcher_sha256"),
            barrier.get("authorization_deadline"),
            barrier.get("nft_apply_intent_sha256"),
            barrier.get("persistence_plan_sha256"), barrier.get("authorized_writer"),
            barrier.get("unit_sha256"), barrier.get("dependency_sha256"),
        )
        != (
            capture, freeze, source, round_number, auth_sha, ready_sha, name,
            value["host"], dispatcher["sha256"], value["authorization_deadline"],
            intent_sha, plan_sha, authorized_writer, unit["sha256"], dependency_roots,
        )
        or barrier.get("automatic_unfence") is not False
        or barrier.get("precommit_missing_selector_behavior")
        != "fail-closed-without-nft-apply"
    ):
        fail("persistently-stopped restart-fence binding differs")
    armed_at = parse_utc(barrier.get("armed_at"), "persistently-stopped restart-fence arm")
    if barrier.get("arming_mode") == "initial-live-window":
        if barrier.get("armed_boot_id") != authorized_writer["boot_id"]:
            fail("persistently-stopped initial restart-fence chronology differs")
    elif barrier.get("arming_mode") == "post-reboot-fail-closed-reconciliation":
        if (
            not reboot_after_intent
            or barrier.get("armed_boot_id") != current_boot
        ):
            fail("persistently-stopped reconciled restart-fence chronology differs")
    elif barrier.get("arming_mode") == "same-boot-fail-closed-reconciliation":
        if (
            reboot_after_intent
            or barrier.get("armed_boot_id") != authorized_writer["boot_id"]
        ):
            fail("persistently-stopped same-boot restart-fence chronology differs")
    else:
        fail("persistently-stopped restart-fence mode differs")
    if value.get("restart_fence_armed_at") != barrier.get("armed_at"):
        fail("persistently-stopped restart-fence time differs")

    def validate_absence_sample(sample: Any, label: str) -> dt.datetime:
        sample_fields = {
            "observed_at", "writer_pids", "active_selector_absent",
            "legacy_start_allow_absent", "nft_table_absent", "fence_unit_enabled",
            "fence_unit_active", "legacy_units",
        }
        if (
            not isinstance(sample, dict)
            or set(sample) != sample_fields
            or sample.get("writer_pids") != []
            or sample.get("active_selector_absent") is not True
            or sample.get("legacy_start_allow_absent") is not True
            or sample.get("nft_table_absent") is not True
            or sample.get("fence_unit_enabled") is not True
            or sample.get("fence_unit_active") is not False
        ):
            fail(f"{label} stable absence state differs")
        rows = sample.get("legacy_units")
        expected_units = [
            "arc-self-heal.service", "arc-node.service",
            "arc-node-update.service", "arc-node-update.timer",
        ]
        if not isinstance(rows, list) or [
            row.get("unit") if isinstance(row, dict) else None for row in rows
        ] != expected_units:
            fail(f"{label} stable absence unit order differs")
        for row in rows:
            if (
                set(row) != {
                    "unit", "active_state", "job", "main_pid", "dropin_effective"
                }
                or row.get("active_state") not in {"inactive", "failed"}
                or row.get("job") not in {"", "0"}
                or (row["unit"].endswith(".service") and row.get("main_pid") != "0")
                or row.get("dropin_effective") is not True
            ):
                fail(f"{label} stable absence unit state differs")
        return parse_utc(sample.get("observed_at"), f"{label} stable absence time")

    precommit, precommit_sha = validate_wrapper(
        value.get("precommit_status"), "persistently-stopped precommit status"
    )
    precommit_fields = {
        "schema", "capture_id", "freeze_plan_sha256", "source_main_commit",
        "round_number", "round_authorization_sha256", "round_readiness_sha256",
        "node", "host", "authorized_writer", "nft_apply_intent",
        "persistence_plan", "nft_deadline_gate", "applied_commit", "nft_table_binding",
        "apply_helper_sha256", "nft_policy_source_sha256",
        "persistent_restart_fence", "restart_fence_armed_at",
        "restart_fence_arming_mode", "reconciliation_started_at", "observed_at",
        "authorization_deadline", "recorded_boot_id", "current_boot_id",
        "reboot_after_intent", "writer_exit_cause", "writer_exit_signal",
        "recovery_signal_sent", "stable_absence_samples", "writer_state", "nft_table_absent",
        "applied_commit_absent", "active_selector_absent", "fence_unit_enabled",
        "fence_unit_active",
    }
    if (
        set(precommit) != precommit_fields
        or precommit.get("schema") != PRECOMMIT_STATUS_SCHEMA
        or (
            precommit.get("capture_id"), precommit.get("freeze_plan_sha256"),
            precommit.get("source_main_commit"), precommit.get("round_number"),
            precommit.get("round_authorization_sha256"),
            precommit.get("round_readiness_sha256"), precommit.get("node"),
            precommit.get("host"), precommit.get("authorized_writer"),
            precommit.get("nft_apply_intent"), precommit.get("persistence_plan"),
            precommit.get("nft_deadline_gate"), precommit.get("applied_commit"),
            precommit.get("nft_table_binding"),
            precommit.get("apply_helper_sha256"),
            precommit.get("nft_policy_source_sha256"),
            precommit.get("persistent_restart_fence"),
            precommit.get("restart_fence_armed_at"),
            precommit.get("restart_fence_arming_mode"),
            precommit.get("authorization_deadline"), precommit.get("recorded_boot_id"),
            precommit.get("current_boot_id"),
        )
        != (
            capture, freeze, source, round_number, auth_sha, ready_sha, name,
            value["host"], authorized_writer, value["nft_apply_intent"],
            value["persistence_plan"], gate_wrapper, commit_wrapper,
            value["nft_table_binding"],
            helper_sha, policy_sha, value["persistent_restart_fence"],
            barrier["armed_at"], barrier["arming_mode"], value["authorization_deadline"],
            authorized_writer["boot_id"], current_boot,
        )
        or precommit.get("reboot_after_intent") is not reboot_after_intent
        or precommit.get("writer_exit_cause") != "unknown"
        or precommit.get("writer_exit_signal") is not None
        or precommit.get("recovery_signal_sent") is not False
        or precommit.get("writer_state") != "persistently-stopped"
        or precommit.get("nft_table_absent") is not True
        or precommit.get("applied_commit_absent") is not (commit_wrapper is None)
        or precommit.get("active_selector_absent") is not True
        or precommit.get("fence_unit_enabled") is not True
        or precommit.get("fence_unit_active") is not False
    ):
        fail("persistently-stopped precommit status differs")
    reconciliation_started = parse_utc(
        precommit.get("reconciliation_started_at"),
        "persistently-stopped reconciliation start",
    )
    precommit_observed = parse_utc(
        precommit.get("observed_at"), "persistently-stopped precommit observation"
    )
    samples = precommit.get("stable_absence_samples")
    if not isinstance(samples, list) or len(samples) != 2:
        fail("persistently-stopped stable absence sample count differs")
    sample_times = [
        validate_absence_sample(sample, "persistently-stopped precommit")
        for sample in samples
    ]
    if not (reconciliation_started <= sample_times[0] < sample_times[1]
            <= precommit_observed):
        fail("persistently-stopped reconciliation chronology differs")

    persisted, persisted_sha = validate_wrapper(
        value.get("persisted_head"), "persistently-stopped persisted head"
    )
    persisted_fields = {
        "schema", "source_main_commit", "capture_id", "node", "host",
        "freeze_plan_sha256", "round_number", "round_authorization_sha256",
        "round_readiness_sha256", "current_boot_id", "precommit_status_sha256",
        "nft_apply_intent_sha256", "applied_commit_sha256", "persistence_plan_sha256",
        "persistent_restart_fence_sha256", "inspector_binary_sha256",
        "genesis_sha256", "validator_public_keys_sha256",
        "legacy_validator_set_sha256", "source_inputs", "staged_inputs",
        "source_pair_role",
        "live_source_capture_sha256", "final_absence_sample",
        "export_summary_sha256", "inspect_summary_sha256",
        "legacy_dag_round", "trusted_anchor_ancestry",
        "candidate_checkpoint_sha256", "candidate_checkpoint_size",
        "authorization_ancestry_proof_sha256", "head", "allow_unbound_legacy_wal",
        "completed_at", "writer_stopped", "restart_barrier_active",
        "network_quarantine_active", "nft_table_absent", "applied_commit_absent",
        "active_selector_absent", "global_absence_claimed",
    }
    if (
        set(persisted) != persisted_fields
        or persisted.get("schema") != PERSISTED_STOPPED_SCHEMA
        or (
            persisted.get("source_main_commit"), persisted.get("capture_id"),
            persisted.get("node"), persisted.get("host"),
            persisted.get("freeze_plan_sha256"), persisted.get("round_number"),
            persisted.get("round_authorization_sha256"),
            persisted.get("round_readiness_sha256"), persisted.get("current_boot_id"),
            persisted.get("precommit_status_sha256"),
            persisted.get("nft_apply_intent_sha256"),
            persisted.get("persistence_plan_sha256"),
            persisted.get("persistent_restart_fence_sha256"),
        )
        != (
            source, capture, name, value["host"], freeze, round_number, auth_sha,
            ready_sha, current_boot, precommit_sha, intent_sha, plan_sha, barrier_sha,
        )
        or persisted.get("writer_stopped") is not True
        or persisted.get("restart_barrier_active") is not True
        or persisted.get("network_quarantine_active") is not False
        or persisted.get("nft_table_absent") is not True
        or persisted.get("applied_commit_absent") is not (commit_wrapper is None)
        or persisted.get("active_selector_absent") is not True
        or persisted.get("global_absence_claimed") is not False
        or persisted.get("source_pair_role") != "preauthorization-boundary"
        or not isinstance(persisted.get("allow_unbound_legacy_wal"), bool)
        or persisted.get("applied_commit_sha256") != commit_sha
    ):
        fail("persistently-stopped persisted-head identity/state differs")
    for label in (
        "inspector_binary_sha256", "genesis_sha256", "validator_public_keys_sha256",
        "legacy_validator_set_sha256", "export_summary_sha256", "inspect_summary_sha256",
        "candidate_checkpoint_sha256", "authorization_ancestry_proof_sha256",
    ):
        require_hash(persisted.get(label), f"persistently-stopped {label}")
    require_uint(
        persisted.get("candidate_checkpoint_size"),
        "persistently-stopped candidate checkpoint size", positive=True,
    )
    source_inputs = persisted.get("source_inputs")
    source_input_fields = {
        "original_data_dir", "legacy_dag_wal_dir", "final_state_wal", "fixed_data_dir",
        "fixed_state_wal", "fixed_snapshot", "fixed_genesis_binding",
        "live_source_capture_sha256", "rust_live_source_capture_sha256",
        "source_pair_role",
    }
    if (
        not isinstance(source_inputs, dict)
        or set(source_inputs) not in (
            source_input_fields, source_input_fields | {"wal_normalization"}
        )
    ):
        fail("persistently-stopped source-input inventory differs")
    validate_file_identity(
        source_inputs["original_data_dir"], "persistently-stopped original data dir",
        directory=True,
    )
    validate_file_identity(
        source_inputs["legacy_dag_wal_dir"],
        "persistently-stopped legacy DAG WAL dir",
        directory=True,
    )
    validate_file_identity(
        source_inputs["final_state_wal"], "persistently-stopped final full state WAL"
    )
    validate_file_identity(
        source_inputs["fixed_data_dir"], "persistently-stopped fixed data dir",
        directory=True,
    )
    validate_file_identity(
        source_inputs["fixed_state_wal"], "persistently-stopped fixed state WAL"
    )
    validate_file_identity(
        source_inputs["fixed_snapshot"], "persistently-stopped fixed snapshot"
    )
    validate_file_identity(
        source_inputs["fixed_genesis_binding"],
        "persistently-stopped fixed genesis binding",
    )
    for label in ("live_source_capture_sha256", "rust_live_source_capture_sha256"):
        require_hash(source_inputs.get(label), f"persistently-stopped {label}")
    if source_inputs.get("source_pair_role") != "preauthorization-boundary":
        fail("persistently-stopped source-pair role differs")
    normalization = source_inputs.get("wal_normalization")
    if normalization is not None:
        normalization_fields = {
            "normalizer_sha256", "plan_sha256", "receipt_sha256",
            "normalized_data_dir", "normalized_state_wal", "source_snapshot",
            "semantic_stream_sha256",
        }
        if not isinstance(normalization, dict) or set(normalization) != normalization_fields:
            fail("persistently-stopped WAL normalization evidence differs")
        for label in (
            "normalizer_sha256", "plan_sha256", "receipt_sha256",
            "semantic_stream_sha256",
        ):
            require_hash(
                normalization.get(label),
                f"persistently-stopped normalization {label}",
            )
        validate_file_identity(
            normalization["normalized_data_dir"],
            "persistently-stopped normalized source directory",
            directory=True,
        )
        normalized_wal = validate_file_identity(
            normalization["normalized_state_wal"],
            "persistently-stopped normalized source WAL",
        )
        normalization_snapshot = validate_file_identity(
            normalization["source_snapshot"],
            "persistently-stopped normalization source snapshot",
        )
        if (
            (normalized_wal["sha256"], normalized_wal["size"])
            != (
                source_inputs["fixed_state_wal"]["sha256"],
                source_inputs["fixed_state_wal"]["size"],
            )
            or (normalization_snapshot["sha256"], normalization_snapshot["size"])
            != (
                source_inputs["fixed_snapshot"]["sha256"],
                source_inputs["fixed_snapshot"]["size"],
            )
        ):
            fail("persistently-stopped normalized/fixed pair content differs")
    if (
        persisted.get("live_source_capture_sha256")
        != source_inputs["live_source_capture_sha256"]
    ):
        fail("persistently-stopped live-source capture root differs")
    require_hash(
        persisted.get("live_source_capture_sha256"),
        "persistently-stopped live-source capture",
    )
    final_absence_time = validate_absence_sample(
        persisted.get("final_absence_sample"), "persistently-stopped final"
    )
    staged_inputs = persisted.get("staged_inputs")
    if not isinstance(staged_inputs, dict) or set(staged_inputs) != {
        "inspector", "genesis", "validator_public_keys", "legacy_validator_set",
    }:
        fail("persistently-stopped staged-input inventory differs")
    for label, root_field in (
        ("inspector", "inspector_binary_sha256"),
        ("genesis", "genesis_sha256"),
        ("validator_public_keys", "validator_public_keys_sha256"),
        ("legacy_validator_set", "legacy_validator_set_sha256"),
    ):
        row = validate_file_identity(staged_inputs[label], f"persistently-stopped staged {label}")
        if row["sha256"] != persisted[root_field]:
            fail(f"persistently-stopped staged {label} root differs")
    expected_input_roots = {
        "data_dir": {
            key: source_inputs["fixed_data_dir"][key]
            for key in (
                "device", "inode", "mode", "uid", "gid", "nlink", "mtime_ns", "ctime_ns",
            )
        },
        "state_wal": {
            key: source_inputs["fixed_state_wal"][key]
            for key in (
                "device", "inode", "mode", "uid", "gid", "nlink", "sha256",
                "size", "mtime_ns", "ctime_ns",
            )
        },
        "snapshot": {
            key: source_inputs["fixed_snapshot"][key]
            for key in (
                "device", "inode", "mode", "uid", "gid", "nlink", "sha256",
                "size", "mtime_ns", "ctime_ns",
            )
        },
        "genesis": {
            key: staged_inputs["genesis"][key]
            for key in (
                "device", "inode", "mode", "uid", "gid", "nlink", "sha256",
                "size", "mtime_ns", "ctime_ns",
            )
        },
        "legacy_validator_set": {
            key: staged_inputs["legacy_validator_set"][key]
            for key in (
                "device", "inode", "mode", "uid", "gid", "nlink", "sha256",
                "size", "mtime_ns", "ctime_ns",
            )
        },
    }
    head, height = validate_stable_head(
        persisted.get("head"), "persistently-stopped persisted head"
    )
    if value.get("stable_head") != head:
        fail("persistently-stopped stable-head projection differs")
    validate_legacy_dag_round(
        persisted.get("legacy_dag_round"), "persistently-stopped"
    )
    anchor = persisted.get("trusted_anchor_ancestry")
    anchor_fields = {
        "anchor_height", "anchor_block_hash", "anchor_state_root",
        "classification", "inspection", "inspection_sha256",
    }
    anchor_inspection = anchor.get("inspection") if isinstance(anchor, dict) else None
    if (
        not isinstance(anchor, dict)
        or set(anchor) != anchor_fields
        or anchor.get("anchor_height") != 137145
        or anchor.get("anchor_block_hash")
            != "8fac459a8de0164b28e30d3f67adf6aefe01054912a3d1ae5c53765e59935a90"
        or anchor.get("anchor_state_root")
            != "d300a2bb8dbe7f6da9596b550f31efd36eb842a1861e294c25740a19c8e3bc6d"
        or anchor.get("inspection_sha256") != digest(anchor_inspection)
    ):
        fail("persistently-stopped trusted anchor binding differs")
    if height < anchor["anchor_height"]:
        if (
            anchor.get("classification") != "below_trusted_anchor"
            or anchor_inspection is not None
        ):
            fail("persistently-stopped below-anchor classification differs")
    else:
        inspection_fields = {
            "schema", "height", "block_hash", "state_root", "input_roots",
        }
        if (
            not isinstance(anchor_inspection, dict)
            or set(anchor_inspection) != inspection_fields
            or anchor_inspection.get("schema")
                != "arc.recovery.legacy-block-inspection.v1"
            or anchor_inspection.get("height") != anchor["anchor_height"]
            or anchor_inspection.get("input_roots") != expected_input_roots
        ):
            fail("persistently-stopped trusted anchor inspection differs")
        observed_hash = require_hash(
            anchor_inspection.get("block_hash"),
            "persistently-stopped observed anchor block",
        )
        observed_state = require_hash(
            anchor_inspection.get("state_root"),
            "persistently-stopped observed anchor state",
        )
        matches = (
            observed_hash == anchor["anchor_block_hash"]
            and observed_state == anchor["anchor_state_root"]
        )
        expected_classification = (
            "valid_anchor_descendant" if matches else "conflicting_anchor"
        )
        if anchor.get("classification") != expected_classification:
            fail("persistently-stopped trusted anchor classification differs")
    completed = parse_utc(
        persisted.get("completed_at"), "persistently-stopped persisted-head completion"
    )
    if (
        final_absence_time < precommit_observed
        or completed < final_absence_time
        or value.get("secured_at") != persisted.get("completed_at")
    ):
        fail("persistently-stopped completion chronology differs")

    ancestry, ancestry_sha = validate_wrapper(
        value.get("authorization_ancestry_proof"),
        "persistently-stopped authorization ancestry",
    )
    ancestry_fields = {
        "schema", "capture_id", "freeze_plan_sha256", "round_number",
        "round_authorization_sha256", "round_readiness_sha256", "node", "host",
        "persisted_head", "checks", "loader_contract",
    }
    if (
        set(ancestry) != ancestry_fields
        or ancestry.get("schema") != STOPPED_ANCESTRY_SCHEMA
        or (
            ancestry.get("capture_id"), ancestry.get("freeze_plan_sha256"),
            ancestry.get("round_number"), ancestry.get("round_authorization_sha256"),
            ancestry.get("round_readiness_sha256"), ancestry.get("node"),
            ancestry.get("host"), ancestry.get("persisted_head"),
        )
        != (capture, freeze, round_number, auth_sha, ready_sha, name, value["host"], head)
        or ancestry.get("loader_contract")
        != "continuous-canonical-parent-chain-through-persisted-head"
        or ancestry_sha != persisted.get("authorization_ancestry_proof_sha256")
    ):
        fail("persistently-stopped authorization ancestry identity differs")
    checks = ancestry.get("checks")
    if not isinstance(checks, list) or [
        item.get("label") if isinstance(item, dict) else None for item in checks
    ] != ["public-latest", "authenticated-loopback-latest"]:
        fail("persistently-stopped authorization ancestry checks differ")
    for check in checks:
        if set(check) != {
            "label", "height", "expected_block_hash", "observed_block_hash",
            "state_root", "inspection_sha256", "input_roots",
        }:
            fail("persistently-stopped authorization ancestry check fields differ")
        check_height = require_uint(
            check.get("height"), "persistently-stopped ancestry height", positive=True
        )
        expected_hash = require_hash(
            check.get("expected_block_hash"), "persistently-stopped expected ancestry hash"
        )
        if require_hash(
            check.get("observed_block_hash"), "persistently-stopped observed ancestry hash"
        ) != expected_hash:
            fail("persistently-stopped authorization ancestry hash differs")
        require_hash(check.get("state_root"), "persistently-stopped ancestry state root")
        require_hash(check.get("inspection_sha256"), "persistently-stopped inspection root")
        if check_height > height or check.get("input_roots") != expected_input_roots:
            fail("persistently-stopped ancestry input/head binding differs")
    return completed, completed, height


def validate_node_transition(
    value: Any,
    *,
    authorization_sha256: str | None = None,
    node: str | None = None,
) -> dict[str, Any]:
    """Validate one positive live-to-secured transition and normalize its proof."""

    if not isinstance(value, dict):
        fail("quarantine node transition is not an object")
    schema = value.get("schema")
    if schema == NODE_APPLIED_SCHEMA:
        secured_at, height = validate_node_applied(
            value, authorization_sha256=authorization_sha256, node=node
        )
        return {
            "kind": ACTIVE_TRANSITION_KIND,
            "schema": schema,
            "node": value["node"],
            "host": value["host"],
            "secured_at": secured_at,
            "secured_at_raw": value["nft_applied_at"],
            "authorization_deadline": value["nft_deadline_gate"]["value"]
                ["authorization_deadline"],
            "writer_identity": {
                "boot_id": value["boot_id"],
                "writer_pid": value["writer_pid"],
                "writer_start_ticks": value["writer_start_ticks"],
                "writer_cgroup_sha256": value["writer_cgroup_sha256"],
            },
            "authorization_ancestry_proof": value["authorization_ancestry_proof"],
            "verified_at": parse_utc(
                value["network_quarantine_receipt"]["value"]["installed_at"],
                "node active-quarantine verification time",
            ),
            "height": height,
            "stable_head": value["stable_head"],
            "persistent_restart_fence_sha256":
                value["persistent_restart_fence_sha256"],
            "source_main_commit": value["network_quarantine_receipt"]["value"]
                ["source_main_commit"],
        }
    if schema == NODE_STOPPED_PRECOMMIT_SCHEMA:
        secured_at, verified_at, height = validate_node_stopped_precommit(
            value, authorization_sha256=authorization_sha256, node=node
        )
        return {
            "kind": STOPPED_PRECOMMIT_TRANSITION_KIND,
            "schema": schema,
            "node": value["node"],
            "host": value["host"],
            "secured_at": secured_at,
            "secured_at_raw": value["secured_at"],
            "authorization_deadline": value["authorization_deadline"],
            "writer_identity": {
                "boot_id": value["authorized_writer"]["boot_id"],
                "writer_pid": value["authorized_writer"]["pid"],
                "writer_start_ticks": value["authorized_writer"]["start_ticks"],
                "writer_cgroup_sha256": value["authorized_writer"]["cgroup_sha256"],
            },
            "authorization_ancestry_proof": value["authorization_ancestry_proof"],
            "verified_at": verified_at,
            "height": height,
            "stable_head": value["stable_head"],
            "persistent_restart_fence_sha256":
                value["persistent_restart_fence"]["sha256"],
            "source_main_commit": value["source_main_commit"],
        }
    fail("quarantine node transition schema differs")


def validate_prior_fenced_status(
    value: Any, *, transition: Mapping[str, Any], transition_sha256: str
) -> dt.datetime:
    if transition.get("schema") == NODE_STOPPED_PRECOMMIT_SCHEMA:
        fields = {
            "schema", "capture_id", "freeze_plan_sha256", "node", "host",
            "node_transition_receipt_sha256", "transition_schema", "transitioned_at",
            "observed_at", "writer_state", "current_boot_id", "stable_head",
            "persistent_restart_fence_sha256", "precommit_status_sha256",
            "source_inputs", "nft_table_absent", "applied_commit_absent",
            "active_selector_absent", "fence_unit_enabled", "fence_unit_active",
            "automatic_legacy_restart",
        }
        if (
            not isinstance(value, dict)
            or set(value) != fields
            or value.get("schema") != STOPPED_STATUS_SCHEMA
            or (
                value.get("capture_id"), value.get("freeze_plan_sha256"),
                value.get("node"), value.get("host"),
                value.get("node_transition_receipt_sha256"),
                value.get("transition_schema"), value.get("transitioned_at"),
                value.get("stable_head"), value.get("persistent_restart_fence_sha256"),
                value.get("precommit_status_sha256"), value.get("source_inputs"),
            )
            != (
                transition.get("capture_id"), transition.get("freeze_plan_sha256"),
                transition.get("node"), transition.get("host"), transition_sha256,
                NODE_STOPPED_PRECOMMIT_SCHEMA, transition.get("secured_at"),
                transition.get("stable_head"),
                (transition.get("persistent_restart_fence") or {}).get("sha256"),
                (transition.get("precommit_status") or {}).get("sha256"),
                (transition.get("persisted_head") or {}).get("value", {}).get("source_inputs"),
            )
            or value.get("writer_state") != "persistently-stopped"
            or value.get("nft_table_absent") is not True
            or value.get("applied_commit_absent") is not (
                transition.get("applied_commit") is None
            )
            or value.get("active_selector_absent") is not True
            or value.get("fence_unit_enabled") is not True
            or value.get("fence_unit_active") is not False
            or value.get("automatic_legacy_restart") is not False
        ):
            fail("prior persistently-stopped status does not re-prove its transition")
        current_boot = value.get("current_boot_id")
        if (
            not isinstance(current_boot, str)
            or UUID_RE.fullmatch(current_boot) is None
        ):
            fail("prior persistently-stopped status boot identity differs")
        observed = parse_utc(value.get("observed_at"), "prior stopped current observation")
        if observed < parse_utc(
            transition.get("secured_at"), "prior stopped historical transition"
        ):
            fail("prior stopped current observation predates its transition")
        return observed

    fields = {
        "schema", "capture_id", "freeze_plan_sha256", "node", "host",
        "node_transition_receipt_sha256", "observed_at", "writer_state", "boot_id",
        "writer_pid", "writer_start_ticks", "writer_cgroup_sha256",
        "network_quarantine_receipt_sha256", "owned_ruleset_stateless_sha256",
        "stable_head", "active", "enabled", "persistent_restart_fence_sha256",
    }
    if not isinstance(value, dict) or set(value) != fields or value.get("schema") != PRIOR_STATUS_SCHEMA:
        fail("prior-fenced current status fields/schema differ")
    if ((value.get("capture_id"), value.get("freeze_plan_sha256"), value.get("node"), value.get("host"))
            != (transition.get("capture_id"), transition.get("freeze_plan_sha256"),
                transition.get("node"), transition.get("host"))
            or value.get("node_transition_receipt_sha256") != transition_sha256
            or value.get("network_quarantine_receipt_sha256")
                != transition.get("network_quarantine_receipt_sha256")
            or value.get("owned_ruleset_stateless_sha256")
                != transition.get("owned_ruleset_stateless_sha256")
            or value.get("stable_head") != transition.get("stable_head")
            or value.get("active") is not True or value.get("enabled") is not True):
        fail("prior-fenced current status does not re-prove the applied quarantine")
    state = value.get("writer_state")
    if state == "exact-live-fenced":
        if (value.get("boot_id"), value.get("writer_pid"), value.get("writer_start_ticks"),
                value.get("writer_cgroup_sha256")) != (
                    transition.get("boot_id"), transition.get("writer_pid"),
                    transition.get("writer_start_ticks"), transition.get("writer_cgroup_sha256")
                ) or value.get("persistent_restart_fence_sha256") is not None:
            fail("prior-fenced live writer identity drifted")
    elif state == "persistently-stopped":
        restart = require_hash(
            value.get("persistent_restart_fence_sha256"), "prior-fenced persistent stop root"
        )
        if restart != transition.get("persistent_restart_fence_sha256"):
            fail("prior-fenced persistent stop root differs")
    else:
        fail("prior-fenced writer state differs")
    observed = parse_utc(value.get("observed_at"), "prior-fenced current observation")
    if observed < parse_utc(
        transition.get("nft_applied_at"), "prior-fenced historical transition"
    ):
        fail("prior-fenced current observation predates the applied quarantine")
    return observed


def validate_round_authorization(
    value: Any,
    *,
    prior_results: Sequence[Mapping[str, Any]] = (),
) -> dict[str, Any]:
    fields = {
        "schema", "capture_id", "freeze_plan_sha256", "round_number",
        "source_main_commit",
        "live_observation_selection_sha256", "live_observation_generation",
        "observation_generation_receipt_sha256", "drive_prefreeze_receipt_sha256",
        "live_observation_selected_at",
        "prior_round_result_sha256s", "prior_fenced", "targets",
        "public_height_receipt", "authenticated_height_cross_proof",
        "live_source_captures",
        "authorized_at", "authorization_deadline",
    }
    if not isinstance(value, dict) or set(value) != fields or value.get("schema") != ROUND_AUTH_SCHEMA:
        fail("quarantine-round authorization fields/schema differ")
    capture = require_hash(value.get("capture_id"), "round capture")
    freeze = require_hash(value.get("freeze_plan_sha256"), "round freeze")
    source = require_commit(value.get("source_main_commit"), "round source commit")
    selection_sha = require_hash(
        value.get("live_observation_selection_sha256"),
        "round live-observation selection",
    )
    observation_generation = require_hash(
        value.get("live_observation_generation"),
        "round live-observation generation",
    )
    generation_receipt_sha = require_hash(
        value.get("observation_generation_receipt_sha256"),
        "round observation-generation receipt",
    )
    drive_receipt_sha = require_hash(
        value.get("drive_prefreeze_receipt_sha256"),
        "round Drive prefreeze receipt",
    )
    try:
        observation_selected_at = dt.datetime.strptime(
            str(value.get("live_observation_selected_at")),
            "%Y-%m-%dT%H:%M:%S.%fZ",
        ).replace(tzinfo=dt.timezone.utc)
    except ValueError as error:
        raise QuarantineRoundError(
            "round live-observation selection timestamp differs"
        ) from error
    number = require_uint(value.get("round_number"), "round number", positive=True)
    if number > MAX_ROUNDS:
        fail("quarantine round exceeds the six-node bound")
    prior_hashes = value.get("prior_round_result_sha256s")
    if not isinstance(prior_hashes, list) or len(prior_hashes) != number - 1:
        fail("prior quarantine-round root count differs")
    expected_prior = [digest(item) for item in prior_results]
    if prior_hashes != expected_prior:
        fail("prior quarantine-round roots differ")
    prior = value.get("prior_fenced")
    targets = value.get("targets")
    if not isinstance(prior, list) or not isinstance(targets, list):
        fail("quarantine-round partition is missing")
    prior_names = ordered_nodes(prior, "prior-fenced partition")
    target_names = ordered_nodes(targets, "live-target partition")
    if not target_names or set(prior_names) & set(target_names):
        fail("quarantine-round partition overlaps or has no targets")
    if prior_names + target_names != [name for name, _host in FLEET]:
        # Each side is already fleet ordered; compare the union independently.
        if set(prior_names) | set(target_names) != set(FLEET_MAP):
            fail("quarantine-round partition does not cover the fixed fleet")
    prior_fields = {
        "node", "host", "node_transition_receipt_sha256", "transition_schema",
        "transitioned_at",
        "stable_head", "persistent_restart_fence_sha256", "current_status",
    }
    for row in prior:
        if set(row) != prior_fields:
            fail("prior-fenced row fields differ")
        require_hash(
            row.get("node_transition_receipt_sha256"),
            "prior-fenced transition root",
        )
        if row.get("transition_schema") not in {
            NODE_APPLIED_SCHEMA, NODE_STOPPED_PRECOMMIT_SCHEMA,
        }:
            fail("prior-fenced transition schema differs")
        parse_utc(row.get("transitioned_at"), "prior-fenced transition time")
        head = row.get("stable_head")
        if not isinstance(head, dict) or set(head) != {"height", "block_hash", "state_root"}:
            fail("prior-fenced stable head differs")
        require_uint(head.get("height"), "prior-fenced height", positive=True)
        require_hash(head.get("block_hash"), "prior-fenced block hash")
        require_hash(head.get("state_root"), "prior-fenced state root")
        restart = row.get("persistent_restart_fence_sha256")
        if restart is not None:
            require_hash(restart, "prior-fenced restart root")
    derived_prior: dict[str, dict[str, Any]] = {}
    for result in prior_results:
        if not isinstance(result, dict) or result.get("schema") != ROUND_RESULT_SCHEMA:
            fail("prior quarantine-round result schema differs")
        wrappers = result.get("transitions")
        if not isinstance(wrappers, list):
            fail("prior quarantine-round transition set differs")
        for wrapper in wrappers:
            item, item_sha = validate_wrapper(wrapper, "prior node transition")
            projection = validate_node_transition(item)
            name = item["node"]
            if name in derived_prior:
                fail("prior quarantine rounds secure a node more than once")
            derived_prior[name] = {
                "node": name, "host": item["host"],
                "node_transition_receipt_sha256": item_sha,
                "transition_schema": projection["schema"],
                "transitioned_at": projection["secured_at_raw"],
                "stable_head": item["stable_head"],
                "persistent_restart_fence_sha256":
                    projection["persistent_restart_fence_sha256"],
                "current_status": None,
            }
    expected_prior_rows = [derived_prior[name] for name, _host in FLEET if name in derived_prior]
    if len(prior) != len(expected_prior_rows):
        fail("prior-fenced row count does not derive from prior applied receipts")
    prior_observed_times: list[dt.datetime] = []
    for row, expected in zip(prior, expected_prior_rows):
        status_wrapper = row.get("current_status")
        status, status_sha = validate_wrapper(
            status_wrapper, f"{row['node']} prior-fenced current status"
        )
        historical = next(
            validate_wrapper(wrapper, "prior node transition")[0]
            for result in prior_results for wrapper in result["transitions"]
            if wrapper["value"]["node"] == row["node"]
        )
        observed = validate_prior_fenced_status(
            status, transition=historical,
            transition_sha256=row["node_transition_receipt_sha256"],
        )
        prior_observed_times.append(observed)
        expected["current_status"] = {"sha256": status_sha, "value": status}
        if row != expected:
            fail("prior-fenced rows do not derive from prior applied receipts/current status")
    for row in targets:
        if set(row) != {"node", "host", "boot_id", "writer_pid", "writer_start_ticks", "writer_cgroup_sha256"}:
            fail("live-target row fields differ")
        require_uint(row.get("writer_pid"), "live-target writer pid", positive=True)
        require_uint(row.get("writer_start_ticks"), "live-target writer start", positive=True)
        require_hash(row.get("writer_cgroup_sha256"), "live-target cgroup")
        if not isinstance(row.get("boot_id"), str) or UUID_RE.fullmatch(row["boot_id"]) is None:
            fail("live-target boot id differs")
    public, public_sha = validate_wrapper(value.get("public_height_receipt"), "round public height")
    public_started, public_completed, public_max = validate_target_height_receipt(
        public, capture_id=capture, freeze_sha256=freeze, targets=target_names
    )
    if public.get("source_main_commit") != source:
        fail("round public-height source commit differs")
    cross, _cross_sha = validate_wrapper(
        value.get("authenticated_height_cross_proof"), "round authenticated height"
    )
    cross_started, cross_completed = validate_target_cross_receipt(
        cross, capture_id=capture, freeze_sha256=freeze, targets=target_names,
        public_sha256=public_sha, public_receipt=public,
    )
    cross_by_name = {row["node"]: row for row in cross["nodes"]}
    for row in targets:
        cross_row = cross_by_name[row["node"]]
        if any(cross_row.get(field) != row.get(field) for field in (
            "host", "boot_id", "writer_pid", "writer_start_ticks", "writer_cgroup_sha256"
        )):
            fail("target authenticated-height writer identity differs from authorization")
    capture_wrappers = value.get("live_source_captures")
    if not isinstance(capture_wrappers, list) or len(capture_wrappers) != len(target_names):
        fail("round live-source capture set differs")
    capture_completed: list[dt.datetime] = []
    capture_rows: dict[str, dict[str, Any]] = {}
    for wrapper, name in zip(capture_wrappers, target_names):
        live_capture, live_capture_sha = validate_wrapper(
            wrapper, f"{name} live source capture"
        )
        target = next(row for row in targets if row["node"] == name)
        completed, projection = validate_live_source_capture(
            live_capture,
            capture_id=capture,
            freeze_sha256=freeze,
            source_main_commit=source,
            round_number=number,
            target=target,
            public_row=next(row for row in public["origins"] if row["name"] == name),
            cross_row=cross_by_name[name],
            public_sha256=public_sha,
            cross_sha256=_cross_sha,
        )
        if live_capture.get("node") != name:
            fail("round live-source capture order differs")
        capture_completed.append(completed)
        capture_rows[name] = {
            **projection,
            "sha256": live_capture_sha,
            "value": live_capture,
        }
    authorized = parse_utc(value.get("authorized_at"), "round authorization time")
    deadline = parse_utc(value.get("authorization_deadline"), "round deadline")
    # UTC across the operator and remote nodes is audit-only.  Exact wrapper
    # roots establish public -> cross -> source-capture causality; each source
    # only has to be internally non-regressing.
    if (public_started > public_completed or cross_started > cross_completed
            or min(capture_completed) > max(capture_completed) or authorized > deadline):
        fail("quarantine-round authorization audit timeline differs")
    if number == 1 and (
        authorized < observation_selected_at
        or deadline > observation_selected_at + dt.timedelta(seconds=MAX_WINDOW_SECONDS)
    ):
        fail("first quarantine mutation exceeds the live-observation selection window")
    # Prior remote UTC observations are not compared to operator UTC; their
    # exact transition/status roots bind the causal predecessor.
    return {
        "capture_id": capture, "freeze_plan_sha256": freeze, "round_number": number,
        "source_main_commit": source,
        "live_observation_selection_sha256": selection_sha,
        "live_observation_generation": observation_generation,
        "observation_generation_receipt_sha256": generation_receipt_sha,
        "drive_prefreeze_receipt_sha256": drive_receipt_sha,
        "live_observation_selected_at": observation_selected_at,
        "prior_names": prior_names, "target_names": target_names,
        "target_rows": {row["node"]: row for row in targets},
        "public_rows": {row["name"]: row for row in public["origins"]},
        "cross_rows": cross_by_name,
        "source_capture_rows": capture_rows,
        "authorized_at": authorized, "deadline": deadline, "public_max": public_max,
    }


def validate_round_result(
    value: Any,
    *,
    authorization: Mapping[str, Any],
    prior_results: Sequence[Mapping[str, Any]],
    transition_receipts: Sequence[Mapping[str, Any]],
) -> dict[str, Any]:
    fields = {
        "schema", "capture_id", "freeze_plan_sha256", "round_number",
        "round_authorization_sha256", "target_readiness", "transitions",
        "mutation_dispatch", "remaining_target_inert_proofs",
        "remaining_targets", "completed_at",
    }
    if not isinstance(value, dict) or set(value) != fields or value.get("schema") != ROUND_RESULT_SCHEMA:
        fail("quarantine-round result fields/schema differ")
    auth = validate_round_authorization(authorization, prior_results=prior_results)
    if (value.get("capture_id"), value.get("freeze_plan_sha256"), value.get("round_number"),
            value.get("round_authorization_sha256")) != (
                auth["capture_id"], auth["freeze_plan_sha256"], auth["round_number"], digest(authorization)
            ):
        fail("quarantine-round result identity differs")
    readiness, readiness_sha = validate_wrapper(
        value.get("target_readiness"), "quarantine-round target readiness"
    )
    if (
        set(readiness) != {
            "schema", "capture_id", "freeze_plan_sha256", "round_number",
            "round_authorization_sha256", "targets", "completed_at",
            "authorization_deadline", "max_elapsed_since_acceptance_ns",
        }
        or readiness.get("schema") != READINESS_SCHEMA
        or (
            readiness.get("capture_id"), readiness.get("freeze_plan_sha256"),
            readiness.get("round_number"), readiness.get("round_authorization_sha256"),
        ) != (
            auth["capture_id"], auth["freeze_plan_sha256"], auth["round_number"],
            digest(authorization),
        )
        or readiness.get("authorization_deadline")
            != auth["deadline"].strftime("%Y-%m-%dT%H:%M:%SZ")
    ):
        fail("quarantine-round target readiness identity differs")
    readiness_rows = readiness.get("targets")
    if not isinstance(readiness_rows, list) or len(readiness_rows) != len(auth["target_names"]):
        fail("quarantine-round target readiness count differs")
    max_elapsed_since_acceptance_ns = require_uint(
        readiness.get("max_elapsed_since_acceptance_ns"),
        "quarantine-round monotonic mutation lease",
        positive=True,
    )
    if max_elapsed_since_acceptance_ns > MAX_WINDOW_SECONDS * 1_000_000_000:
        fail("quarantine-round monotonic mutation lease exceeds its bound")
    readiness_acceptances: dict[str, Mapping[str, Any]] = {}
    for row, name in zip(readiness_rows, auth["target_names"]):
        if not isinstance(row, dict) or set(row) != {
            "node", "host", "authorization_acceptance",
        } or (row.get("node"), row.get("host")) != (name, FLEET_MAP[name]):
            fail("quarantine-round target readiness topology differs")
        acceptance, _acceptance_sha = validate_wrapper(
            row.get("authorization_acceptance"),
            f"{name} round authorization acceptance",
        )
        if (
            set(acceptance) != {
                "schema", "capture_id", "freeze_plan_sha256", "round_number",
                "round_authorization_sha256", "node", "host", "accepted_at",
                "authorization_deadline", "accepted_monotonic_ns", "accepted_boot_id",
            }
            or acceptance.get("schema") != AUTH_ACCEPTANCE_SCHEMA
            or (
                acceptance.get("capture_id"), acceptance.get("freeze_plan_sha256"),
                acceptance.get("round_number"),
                acceptance.get("round_authorization_sha256"), acceptance.get("node"),
                acceptance.get("host"), acceptance.get("authorization_deadline"),
            ) != (
                auth["capture_id"], auth["freeze_plan_sha256"], auth["round_number"],
                digest(authorization), name, FLEET_MAP[name],
                auth["deadline"].strftime("%Y-%m-%dT%H:%M:%SZ"),
            )
        ):
            fail("quarantine-round authorization acceptance differs")
        parse_utc(
            acceptance.get("accepted_at"), f"{name} round authorization acceptance time"
        )
        require_uint(
            acceptance.get("accepted_monotonic_ns"),
            f"{name} round authorization acceptance monotonic time",
            positive=True,
        )
        expected_boot = auth["target_rows"][name]["boot_id"]
        if acceptance.get("accepted_boot_id") != expected_boot:
            fail("quarantine-round authorization acceptance boot differs")
        readiness_acceptances[name] = acceptance
    readiness_completed = parse_utc(
        readiness.get("completed_at"), "quarantine-round readiness completion"
    )
    if (
        readiness_completed < auth["authorized_at"]
        or readiness_completed > auth["deadline"]
    ):
        fail("quarantine-round target readiness timeline differs")
    wrappers = value.get("transitions")
    if not isinstance(wrappers, list) or len(wrappers) != len(transition_receipts):
        fail("quarantine-round transition receipt count differs")
    transitioned_names: list[str] = []
    secured_times: list[dt.datetime] = []
    for wrapper, expected in zip(wrappers, transition_receipts):
        item, _sha = validate_wrapper(wrapper, "round node transition")
        if item != expected:
            fail("quarantine-round node transition wrapper differs")
        projection = validate_node_transition(
            item, authorization_sha256=digest(authorization)
        )
        secured_at = projection["secured_at"]
        name = item["node"]
        if item.get("round_readiness_sha256") != readiness_sha:
            fail("quarantine-round node transition readiness root differs")
        if projection["source_main_commit"] != auth["source_main_commit"]:
            fail("quarantine-round node transition source commit differs")
        if name not in auth["target_names"] or name in transitioned_names:
            fail("quarantine-round node transition is outside target set or duplicated")
        if projection["kind"] == ACTIVE_TRANSITION_KIND:
            pass
        if projection["authorization_deadline"] != auth["deadline"].strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ):
            fail("quarantine-round node transition deadline differs")
        target = auth["target_rows"][name]
        writer = projection["writer_identity"]
        if ((item.get("capture_id"), item.get("freeze_plan_sha256"), item.get("round_number"))
                != (auth["capture_id"], auth["freeze_plan_sha256"], auth["round_number"])
                or item.get("host") != target.get("host")
                or writer.get("boot_id") != target.get("boot_id")
                or writer.get("writer_pid") != target.get("writer_pid")
                or writer.get("writer_start_ticks") != target.get("writer_start_ticks")
                or writer.get("writer_cgroup_sha256")
                    != target.get("writer_cgroup_sha256")):
            fail("quarantine-round transition writer identity differs from its live target")
        head = item["stable_head"]
        cross_row = auth["cross_rows"][name]
        public_row = auth["public_rows"][name]
        minimum = max(
            public_row["info_after_height"], cross_row["loopback_info_after_height"]
        )
        if head["height"] < minimum:
            fail("quarantine-round transition head is below its fresh authorization")
        if (head["height"] == cross_row["loopback_latest_height"]
                and head["block_hash"] != cross_row["loopback_latest_block_hash"]):
            fail("quarantine-round same-height transition head hash differs")
        ancestry_checks = projection["authorization_ancestry_proof"]["value"]["checks"]
        expected_checks = [
            (
                "public-latest", public_row["latest_block_height"],
                public_row["latest_block_hash"],
            ),
            (
                "authenticated-loopback-latest", cross_row["loopback_latest_height"],
                cross_row["loopback_latest_block_hash"],
            ),
        ]
        if [
            (check.get("label"), check.get("height"), check.get("expected_block_hash"))
            for check in ancestry_checks
        ] != expected_checks:
            fail("quarantine-round post-fence ancestry does not bind fresh authorization")
        transitioned_names.append(name);secured_times.append(secured_at)
    expected_order = [
        name for name in auth["target_names"] if name in set(transitioned_names)
    ]
    if transitioned_names != expected_order:
        fail("quarantine-round transition node order differs")
    remaining = value.get("remaining_targets")
    if remaining != [
        name for name in auth["target_names"] if name not in set(transitioned_names)
    ]:
        fail("quarantine-round remaining target set differs")
    dispatch, dispatch_sha = validate_wrapper(
        value.get("mutation_dispatch"), "quarantine-round mutation dispatch"
    )
    dispatch_fields = {
        "schema", "capture_id", "freeze_plan_sha256", "round_number",
        "round_authorization_sha256", "round_readiness_sha256",
        "live_observation_selection_sha256", "live_observation_generation",
        "observation_generation_receipt_sha256", "drive_prefreeze_receipt_sha256",
        "targets", "dispatched_at",
    }
    if (
        set(dispatch) != dispatch_fields
        or dispatch.get("schema") != "arc.recovery.quarantine-mutation-dispatch.v1"
        or (
            dispatch.get("capture_id"), dispatch.get("freeze_plan_sha256"),
            dispatch.get("round_number"), dispatch.get("round_authorization_sha256"),
            dispatch.get("round_readiness_sha256"),
            dispatch.get("live_observation_selection_sha256"),
            dispatch.get("live_observation_generation"),
            dispatch.get("observation_generation_receipt_sha256"),
            dispatch.get("drive_prefreeze_receipt_sha256"),
        ) != (
            auth["capture_id"], auth["freeze_plan_sha256"], auth["round_number"],
            digest(authorization), readiness_sha,
            auth["live_observation_selection_sha256"],
            auth["live_observation_generation"],
            auth["observation_generation_receipt_sha256"],
            auth["drive_prefreeze_receipt_sha256"],
        )
        or dispatch.get("targets") != [
            {"node": name, "host": FLEET_MAP[name]} for name in auth["target_names"]
        ]
    ):
        fail("quarantine-round mutation dispatch differs")
    parse_utc(dispatch.get("dispatched_at"), "quarantine-round mutation dispatch time")
    inert_wrappers = value.get("remaining_target_inert_proofs")
    if not isinstance(inert_wrappers, list):
        fail("quarantine-round remaining-target inert proof set differs")
    if remaining and transitioned_names:
        if len(inert_wrappers) != len(remaining):
            fail("partial quarantine-round result lacks every remaining-target inert proof")
        proof_fields = {
            "schema", "capture_id", "freeze_plan_sha256", "observation_generation",
            "round_number", "round_authorization_sha256", "round_readiness_sha256",
            "mutation_dispatch_sha256", "challenge", "node", "boot_id",
            "writer_live_unfenced", "apply_state_present",
            "restart_effective_mutation_absent", "active_selector_absent",
            "quarantine_nft_absent", "authorization_accepted", "readiness_present",
            "accepted_boottime_ns", "elapsed_since_acceptance_ns",
            "observed_boottime_ns", "observed_at",
        }
        challenges: set[str] = set()
        for wrapper, name in zip(inert_wrappers, remaining):
            proof, _proof_sha = validate_wrapper(
                wrapper, f"{name} remaining-target inert proof"
            )
            accepted = proof.get("accepted_boottime_ns")
            elapsed = proof.get("elapsed_since_acceptance_ns")
            observed = proof.get("observed_boottime_ns")
            challenge = require_hash(
                proof.get("challenge"), f"{name} remaining-target inert challenge"
            )
            acceptance = readiness_acceptances[name]
            target = auth["target_rows"][name]
            if (
                set(proof) != proof_fields
                or proof.get("schema") != INERT_NODE_PROOF_SCHEMA
                or (
                    proof.get("capture_id"), proof.get("freeze_plan_sha256"),
                    proof.get("observation_generation"), proof.get("round_number"),
                    proof.get("round_authorization_sha256"),
                    proof.get("round_readiness_sha256"),
                    proof.get("mutation_dispatch_sha256"), proof.get("node"),
                    proof.get("boot_id"),
                ) != (
                    auth["capture_id"], auth["freeze_plan_sha256"],
                    auth["live_observation_generation"], auth["round_number"],
                    digest(authorization), readiness_sha, dispatch_sha, name,
                    target["boot_id"],
                )
                or proof.get("authorization_accepted") is not True
                or proof.get("writer_live_unfenced") is not True
                or proof.get("restart_effective_mutation_absent") is not True
                or proof.get("active_selector_absent") is not True
                or proof.get("quarantine_nft_absent") is not True
                or not isinstance(proof.get("apply_state_present"), bool)
                or not isinstance(proof.get("readiness_present"), bool)
                or any(
                    isinstance(number, bool) or not isinstance(number, int) or number <= 0
                    for number in (accepted, elapsed, observed)
                )
                or accepted != acceptance.get("accepted_monotonic_ns")
                or proof.get("boot_id") != acceptance.get("accepted_boot_id")
                or observed - accepted != elapsed
                or elapsed <= MAX_WINDOW_SECONDS * 1_000_000_000
            ):
                fail(f"{name} remaining-target inert proof differs")
            parse_utc(proof.get("observed_at"), f"{name} inert-proof observation")
            challenges.add(challenge)
        if len(challenges) != 1:
            fail("partial quarantine-round inert proofs do not share one challenge")
    elif inert_wrappers:
        fail("quarantine-round result has unexpected remaining-target inert proofs")
    completed = parse_utc(value.get("completed_at"), "quarantine-round result completion")
    if remaining and not transitioned_names and completed < auth["deadline"]:
        fail("partial quarantine-round result closed before its authorization expired")
    return {
        **auth,
        "transitioned_names": transitioned_names,
        "remaining_names": remaining,
    }


def validate_zero_progress_attempt(
    *,
    authorization: Mapping[str, Any],
    readiness: Mapping[str, Any],
    dispatch: Mapping[str, Any],
    prior_authorizations: Sequence[Mapping[str, Any]] = (),
    prior_results: Sequence[Mapping[str, Any]] = (),
    result: Mapping[str, Any] | None = None,
    expected_capture_id: str | None = None,
    expected_freeze_sha256: str | None = None,
    containing_round_name: str | None = None,
) -> dict[str, Any]:
    """Validate an inert dispatched attempt and its complete positive prefix.

    A current authorization hash transitively binds the immutable result roots,
    but that is useful only after every prefix result has been validated in
    order.  This shared contract is used before issuing remote challenges and
    by every consumer of a zero-progress release receipt.
    """
    if len(prior_authorizations) != len(prior_results):
        fail("zero-progress prior quarantine-round pair count differs")
    validated_results: list[Mapping[str, Any]] = []
    prefix_states: list[dict[str, Any]] = []
    for number, (prior_authorization, prior_result) in enumerate(
        zip(prior_authorizations, prior_results), start=1
    ):
        wrappers = prior_result.get("transitions")
        if not isinstance(wrappers, list):
            fail("zero-progress prior quarantine-round transition set differs")
        transitions = [
            validate_wrapper(wrapper, "zero-progress prefix transition")[0]
            for wrapper in wrappers
        ]
        state = validate_round_result(
            prior_result,
            authorization=prior_authorization,
            prior_results=validated_results,
            transition_receipts=transitions,
        )
        if state["round_number"] != number or not state["transitioned_names"]:
            fail("zero-progress prior quarantine-round prefix differs")
        prefix_states.append(state)
        validated_results.append(prior_result)

    state = validate_round_authorization(
        authorization, prior_results=validated_results
    )
    number = state["round_number"]
    if number != len(validated_results) + 1:
        fail("zero-progress current quarantine round is not contiguous")
    if containing_round_name is not None and containing_round_name != f"round-{number}":
        fail("zero-progress attempt directory round differs")
    if expected_capture_id is not None and state["capture_id"] != expected_capture_id:
        fail("zero-progress attempt capture differs")
    if (
        expected_freeze_sha256 is not None
        and state["freeze_plan_sha256"] != expected_freeze_sha256
    ):
        fail("zero-progress attempt freeze root differs")
    observation_identity = (
        state["live_observation_selection_sha256"],
        state["live_observation_generation"],
        state["observation_generation_receipt_sha256"],
        state["drive_prefreeze_receipt_sha256"],
    )
    for prefix in prefix_states:
        if (
            prefix["capture_id"] != state["capture_id"]
            or prefix["freeze_plan_sha256"] != state["freeze_plan_sha256"]
            or prefix["source_main_commit"] != state["source_main_commit"]
            or (
                prefix["live_observation_selection_sha256"],
                prefix["live_observation_generation"],
                prefix["observation_generation_receipt_sha256"],
                prefix["drive_prefreeze_receipt_sha256"],
            )
            != observation_identity
        ):
            fail("zero-progress prior quarantine-round identity differs")

    auth_sha = digest(authorization)
    readiness_sha = digest(readiness)
    dispatch_sha = digest(dispatch)
    probe = {
        "schema": ROUND_RESULT_SCHEMA,
        "capture_id": state["capture_id"],
        "freeze_plan_sha256": state["freeze_plan_sha256"],
        "round_number": number,
        "round_authorization_sha256": auth_sha,
        "target_readiness": wrap(readiness),
        "transitions": [],
        "mutation_dispatch": wrap(dispatch),
        "remaining_target_inert_proofs": [],
        "remaining_targets": state["target_names"],
        "completed_at": authorization["authorization_deadline"],
    }
    validate_round_result(
        probe,
        authorization=authorization,
        prior_results=validated_results,
        transition_receipts=[],
    )
    if result is not None:
        if result.get("transitions") != []:
            fail("zero-progress attempt result transitioned")
        validate_round_result(
            result,
            authorization=authorization,
            prior_results=validated_results,
            transition_receipts=[],
        )
    return {
        **state,
        "authorization_sha256": auth_sha,
        "readiness_sha256": readiness_sha,
        "dispatch_sha256": dispatch_sha,
        "readiness_acceptances": {
            row["node"]: row["authorization_acceptance"]["value"]
            for row in readiness["targets"]
        },
    }


def validate_zero_progress_release(
    value: Any,
    *,
    authorization: Mapping[str, Any],
    readiness: Mapping[str, Any],
    dispatch: Mapping[str, Any],
    prior_authorizations: Sequence[Mapping[str, Any]] = (),
    prior_results: Sequence[Mapping[str, Any]] = (),
    result: Mapping[str, Any] | None = None,
    expected_capture_id: str | None = None,
    expected_freeze_sha256: str | None = None,
    containing_round_name: str | None = None,
) -> dict[str, Any]:
    """Validate a challenged release for an expired zero-progress dispatch."""
    state = validate_zero_progress_attempt(
        authorization=authorization,
        readiness=readiness,
        dispatch=dispatch,
        prior_authorizations=prior_authorizations,
        prior_results=prior_results,
        result=result,
        expected_capture_id=expected_capture_id,
        expected_freeze_sha256=expected_freeze_sha256,
        containing_round_name=containing_round_name,
    )
    fields = {
        "schema", "capture_id", "freeze_plan_sha256", "round_number",
        "round_authorization_sha256", "round_readiness_sha256",
        "mutation_dispatch_sha256", "live_observation_selection_sha256",
        "live_observation_generation", "observation_generation_receipt_sha256",
        "drive_prefreeze_receipt_sha256", "challenge", "released_at", "nodes",
    }
    if not isinstance(value, dict) or set(value) != fields:
        fail("zero-progress release fields differ")
    challenge = require_hash(value.get("challenge"), "zero-progress release challenge")
    parse_utc(value.get("released_at"), "zero-progress release time")
    if (
        value.get("schema") != ZERO_PROGRESS_RELEASE_SCHEMA
        or (
            value.get("capture_id"),
            value.get("freeze_plan_sha256"),
            value.get("round_number"),
            value.get("round_authorization_sha256"),
            value.get("round_readiness_sha256"),
            value.get("mutation_dispatch_sha256"),
            value.get("live_observation_selection_sha256"),
            value.get("live_observation_generation"),
            value.get("observation_generation_receipt_sha256"),
            value.get("drive_prefreeze_receipt_sha256"),
        )
        != (
            state["capture_id"],
            state["freeze_plan_sha256"],
            state["round_number"],
            state["authorization_sha256"],
            state["readiness_sha256"],
            state["dispatch_sha256"],
            state["live_observation_selection_sha256"],
            state["live_observation_generation"],
            state["observation_generation_receipt_sha256"],
            state["drive_prefreeze_receipt_sha256"],
        )
    ):
        fail("zero-progress release identity differs")
    wrappers = value.get("nodes")
    if not isinstance(wrappers, list) or len(wrappers) != len(state["target_names"]):
        fail("zero-progress release node proof count differs")
    proof_fields = {
        "schema", "capture_id", "freeze_plan_sha256", "observation_generation",
        "round_number", "round_authorization_sha256", "round_readiness_sha256",
        "mutation_dispatch_sha256", "challenge", "node", "boot_id",
        "writer_live_unfenced", "apply_state_present",
        "restart_effective_mutation_absent", "active_selector_absent",
        "quarantine_nft_absent", "authorization_accepted", "readiness_present",
        "accepted_boottime_ns", "elapsed_since_acceptance_ns",
        "observed_boottime_ns", "observed_at",
    }
    for wrapper, name in zip(wrappers, state["target_names"]):
        proof, _proof_sha = validate_wrapper(
            wrapper, f"{name} zero-progress release proof"
        )
        accepted = proof.get("accepted_boottime_ns")
        elapsed = proof.get("elapsed_since_acceptance_ns")
        observed = proof.get("observed_boottime_ns")
        acceptance = state["readiness_acceptances"][name]
        target = state["target_rows"][name]
        if (
            set(proof) != proof_fields
            or proof.get("schema") != INERT_NODE_PROOF_SCHEMA
            or (
                proof.get("capture_id"),
                proof.get("freeze_plan_sha256"),
                proof.get("observation_generation"),
                proof.get("round_number"),
                proof.get("round_authorization_sha256"),
                proof.get("round_readiness_sha256"),
                proof.get("mutation_dispatch_sha256"),
                proof.get("challenge"),
                proof.get("node"),
                proof.get("boot_id"),
            )
            != (
                state["capture_id"],
                state["freeze_plan_sha256"],
                state["live_observation_generation"],
                state["round_number"],
                state["authorization_sha256"],
                state["readiness_sha256"],
                state["dispatch_sha256"],
                challenge,
                name,
                target["boot_id"],
            )
            or proof.get("writer_live_unfenced") is not True
            or proof.get("restart_effective_mutation_absent") is not True
            or proof.get("active_selector_absent") is not True
            or proof.get("quarantine_nft_absent") is not True
            or proof.get("authorization_accepted") is not True
            or not isinstance(proof.get("apply_state_present"), bool)
            or not isinstance(proof.get("readiness_present"), bool)
            or any(
                isinstance(number, bool) or not isinstance(number, int) or number <= 0
                for number in (accepted, elapsed, observed)
            )
            or accepted != acceptance.get("accepted_monotonic_ns")
            or proof.get("boot_id") != acceptance.get("accepted_boot_id")
            or observed - accepted != elapsed
            or elapsed <= MAX_WINDOW_SECONDS * 1_000_000_000
        ):
            fail(f"{name} zero-progress release proof differs")
        parse_utc(proof.get("observed_at"), f"{name} zero-progress proof time")
    return state


def validate_generation_ledger(value: Any) -> dict[str, Any]:
    fields = {
        "schema", "capture_id", "freeze_plan_sha256", "fleet", "rounds",
        "first_secured_at", "all_nodes_secured_at", "legacy_cutoff_height",
        "live_observation_selection_sha256", "live_observation_generation",
        "observation_generation_receipt_sha256", "drive_prefreeze_receipt_sha256",
    }
    if not isinstance(value, dict) or set(value) != fields or value.get("schema") != LEDGER_SCHEMA:
        fail("quarantine generation-ledger fields/schema differ")
    capture = require_hash(value.get("capture_id"), "generation ledger capture")
    freeze = require_hash(value.get("freeze_plan_sha256"), "generation ledger freeze")
    if value.get("fleet") != [{"node": name, "host": host} for name, host in FLEET]:
        fail("quarantine generation-ledger topology differs")
    rounds = value.get("rounds")
    if not isinstance(rounds, list) or not 1 <= len(rounds) <= MAX_ROUNDS:
        fail("quarantine generation-ledger round count differs")
    prior_results: list[Mapping[str, Any]] = []
    secured: dict[str, Mapping[str, Any]] = {}
    secured_times: list[dt.datetime] = []
    verified_times: list[dt.datetime] = []
    heights: list[int] = []
    source_commit: str | None = None
    observation_identity: tuple[str, str, str, str] | None = None
    for index, row in enumerate(rounds, start=1):
        if not isinstance(row, dict) or set(row) != {"authorization", "result"}:
            fail("quarantine generation-ledger round wrapper differs")
        authorization, _auth_sha = validate_wrapper(row["authorization"], "ledger authorization")
        result, _result_sha = validate_wrapper(row["result"], "ledger result")
        transition_values = [
            validate_wrapper(item, "ledger transition")[0]
            for item in result.get("transitions", [])
        ]
        state = validate_round_result(
            result, authorization=authorization, prior_results=prior_results,
            transition_receipts=transition_values,
        )
        if state["round_number"] != index:
            fail("quarantine generation-ledger round number differs")
        if (
            state["capture_id"] != capture
            or state["freeze_plan_sha256"] != freeze
            or (source_commit is not None and state["source_main_commit"] != source_commit)
        ):
            fail("quarantine generation-ledger round identity/source differs")
        source_commit = state["source_main_commit"]
        current_observation_identity = (
            state["live_observation_selection_sha256"],
            state["live_observation_generation"],
            state["observation_generation_receipt_sha256"],
            state["drive_prefreeze_receipt_sha256"],
        )
        if observation_identity is not None and current_observation_identity != observation_identity:
            fail("quarantine generation-ledger live-observation identity changed")
        observation_identity = current_observation_identity
        if not state["transitioned_names"]:
            fail("zero-progress quarantine attempts must remain outside the transition ledger")
        expected_prior = [name for name, _host in FLEET if name in secured]
        if state["prior_names"] != expected_prior:
            fail("quarantine generation-ledger prior-secured transition differs")
        for item in transition_values:
            name = item["node"]
            if name in secured:
                fail("quarantine generation-ledger secures a node more than once")
            secured[name] = item
            projection = validate_node_transition(item)
            secured_times.append(projection["secured_at"])
            verified_times.append(projection["verified_at"])
            heights.append(projection["height"])
        prior_results.append(result)
    if set(secured) != set(FLEET_MAP):
        fail("quarantine generation-ledger does not secure all six nodes")
    expected_observation_identity = (
        require_hash(value.get("live_observation_selection_sha256"),
                     "ledger live-observation selection"),
        require_hash(value.get("live_observation_generation"),
                     "ledger live-observation generation"),
        require_hash(value.get("observation_generation_receipt_sha256"),
                     "ledger observation-generation receipt"),
        require_hash(value.get("drive_prefreeze_receipt_sha256"),
                     "ledger Drive prefreeze receipt"),
    )
    if observation_identity != expected_observation_identity:
        fail("quarantine generation-ledger live-observation root differs")
    first = parse_utc(value.get("first_secured_at"), "ledger first secured transition")
    all_secured = parse_utc(value.get("all_nodes_secured_at"), "ledger all-secured time")
    if first != min(secured_times) or all_secured != max(verified_times):
        fail("quarantine generation-ledger boundary times differ")
    cutoff = require_uint(value.get("legacy_cutoff_height"), "ledger cutoff", positive=True)
    public_maxima = [validate_round_authorization(
        validate_wrapper(row["authorization"], "ledger authorization")[0],
        prior_results=[validate_wrapper(previous["result"], "prior result")[0]
                       for previous in rounds[:position]],
    )["public_max"] for position, row in enumerate(rounds)]
    if cutoff != max([*heights, *public_maxima]):
        fail("quarantine generation-ledger cutoff height differs")
    return {
        "capture_id": capture, "freeze_plan_sha256": freeze,
        "round_count": len(rounds), "first_secured_at": first,
        "all_nodes_secured_at": all_secured, "legacy_cutoff_height": cutoff,
        "live_observation_selection_sha256": expected_observation_identity[0],
        "live_observation_generation": expected_observation_identity[1],
        "observation_generation_receipt_sha256": expected_observation_identity[2],
        "drive_prefreeze_receipt_sha256": expected_observation_identity[3],
    }
