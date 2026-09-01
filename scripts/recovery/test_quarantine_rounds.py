#!/usr/bin/env python3

from __future__ import annotations

import copy
import datetime as dt
import importlib.util
from pathlib import Path
import unittest


MODULE_PATH = Path(__file__).with_name("quarantine_rounds.py")
SPEC = importlib.util.spec_from_file_location("quarantine_rounds", MODULE_PATH)
assert SPEC and SPEC.loader
qr = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(qr)

H = {str(index): f"{index:064x}"[-64:] for index in range(1, 100)}
CAPTURE = H["1"]
FREEZE = H["2"]
SOURCE = "3" * 40
BASE = dt.datetime(2026, 8, 31, 12, 0, tzinfo=dt.timezone.utc)


def utc(seconds: int) -> str:
    return (BASE + dt.timedelta(seconds=seconds)).strftime("%Y-%m-%dT%H:%M:%SZ")


def target_row(name: str) -> dict:
    index = [row[0] for row in qr.FLEET].index(name) + 1
    return {
        "node": name, "host": qr.FLEET_MAP[name],
        "boot_id": f"00000000-0000-0000-0000-{index:012d}",
        "writer_pid": 1000 + index, "writer_start_ticks": 2000 + index,
        "writer_cgroup_sha256": f"{10 + index:064x}",
    }


def public_receipt(targets: list[str], offset: int) -> dict:
    origins = []
    for index, name in enumerate(targets, start=1):
        height = 100 + offset + index
        origins.append({
            "name": name, "origin": f"http://{qr.FLEET_MAP[name]}:9090",
            "info_before_height": height, "latest_block_height": height,
            "info_after_height": height, "latest_block_hash": f"{30 + index:064x}",
            "info_before_body_sha256": f"{40 + index:064x}",
            "latest_block_body_sha256": f"{50 + index:064x}",
            "info_after_body_sha256": f"{60 + index:064x}",
        })
    return {
        "schema": qr.TARGET_HEIGHT_SCHEMA, "source_main_commit": SOURCE,
        "freeze_plan_sha256": FREEZE, "capture_id": CAPTURE,
        "started_at": utc(offset), "completed_at": utc(offset + 2), "duration_ms": 2000,
        "request_policy": {"redirects": "forbidden", "maximum_body_bytes": 1048576,
                           "timeout_seconds": 10, "proxy_environment": "ignored",
                           "sequence": ["/info", "/block/latest", "/info"]},
        "targets": [
            {
                "node": name,
                "host": qr.FLEET_MAP[name],
                "rpc_origin": f"http://{qr.FLEET_MAP[name]}:9090",
            }
            for name in targets
        ],
        "origins": origins,
        "legacy_public_max_height": max(row["info_after_height"] for row in origins),
    }


def cross_receipt(targets: list[str], public: dict, offset: int) -> dict:
    nodes = []
    public_by_name = {row["name"]: row for row in public["origins"]}
    for index, name in enumerate(targets, start=1):
        target = target_row(name);height = public_by_name[name]["info_after_height"]
        nodes.append({
            "node": name, "host": qr.FLEET_MAP[name],
            "writer_pid": target["writer_pid"], "writer_start_ticks": target["writer_start_ticks"],
            "boot_id": target["boot_id"], "writer_cgroup_sha256": target["writer_cgroup_sha256"],
            "public_info_after_height": height, "public_latest_block_height": height,
            "public_latest_block_hash": public_by_name[name]["latest_block_hash"],
            "loopback_info_before_height": height, "loopback_latest_height": height,
            "loopback_info_after_height": height,
            "loopback_latest_block_hash": public_by_name[name]["latest_block_hash"],
            "response_sha256": {"/info:before": f"{70 + index:064x}",
                                "/block/latest": f"{80 + index:064x}",
                                "/info:after": f"{90 + index:064x}"},
        })
    return {
        "schema": qr.TARGET_CROSS_SCHEMA, "source_main_commit": SOURCE,
        "freeze_plan_sha256": FREEZE, "capture_id": CAPTURE,
        "legacy_public_height_receipt_sha256": qr.digest(public), "challenge": H["4"],
        "started_at": utc(offset + 3), "completed_at": utc(offset + 5),
        "conservative_height_floor": min(row["loopback_info_before_height"] for row in nodes),
        "targets": copy.deepcopy(public["targets"]),
        "nodes": nodes,
    }


def live_source_capture(
    target: dict,
    public_row: dict,
    cross_row: dict,
    *,
    round_number: int,
    public_sha256: str,
    cross_sha256: str,
    completed_at: str,
) -> dict:
    index = [row[0] for row in qr.FLEET].index(target["node"]) + 1
    seed = 20_000 + index * 100

    def directory(offset: int) -> dict:
        return {
            "device": seed + offset,
            "inode": seed + offset + 1,
            "mode": 0o40700,
            "uid": 0,
            "gid": 0,
            "nlink": 2,
            "mtime_ns": seed + offset + 2,
            "ctime_ns": seed + offset + 3,
        }

    def regular(offset: int, root: str) -> dict:
        return {
            "device": seed + offset,
            "inode": seed + offset + 1,
            "mode": 0o100400,
            "uid": 0,
            "gid": 0,
            "nlink": 1,
            "mtime_ns": seed + offset + 2,
            "ctime_ns": seed + offset + 3,
            "sha256": root,
            "size": 8,
        }

    head = {
        "height": max(
            public_row["latest_block_height"],
            public_row["info_after_height"],
            cross_row["loopback_latest_height"],
            cross_row["loopback_info_after_height"],
        ),
        "block_hash": cross_row["loopback_latest_block_hash"],
        "state_root": f"{seed + 1:064x}",
    }
    wal_root = f"{seed + 2:064x}"
    snapshot_root = f"{seed + 3:064x}"
    genesis_root = f"{seed + 4:064x}"
    legacy_root = f"{seed + 5:064x}"
    rust_capture = {
        "schema": qr.RUST_SOURCE_CAPTURE_SCHEMA,
        "captured_at_unix_ms": int(
            dt.datetime.strptime(completed_at, "%Y-%m-%dT%H:%M:%SZ")
            .replace(tzinfo=dt.timezone.utc)
            .timestamp()
            * 1000
        ),
        "head": head,
        "source_data_dir": directory(10),
        "source_wal_prefix": {
            "device": seed + 20,
            "inode": seed + 21,
            "mode": 0o100600,
            "uid": 0,
            "gid": 0,
            "nlink": 1,
            "loader_observed_bytes": 8,
            "copy_observed_bytes": 8,
            "accepted_prefix_bytes": 8,
            "accepted_prefix_sha256": wal_root,
            "quarantined_suffix_bytes_at_loader": 0,
            "loader_tail_reason": "clean-eof",
        },
        "source_snapshot": regular(30, snapshot_root),
        "genesis": regular(40, genesis_root),
        "legacy_validator_set": regular(50, legacy_root),
        "fixed_pair": {
            "data_dir": directory(60),
            "state_wal": regular(70, wal_root),
            "snapshot": regular(80, snapshot_root),
            "genesis_binding": regular(90, f"{seed + 6:064x}"),
            "strict_replay": True,
        },
        "allow_unbound_legacy_wal": False,
    }
    value = {
        "schema": qr.LIVE_SOURCE_CAPTURE_SCHEMA,
        "capture_id": CAPTURE,
        "freeze_plan_sha256": FREEZE,
        "source_main_commit": SOURCE,
        "round_number": round_number,
        "node": target["node"],
        "host": target["host"],
        "authorized_writer": {
            "boot_id": target["boot_id"],
            "pid": target["writer_pid"],
            "start_ticks": target["writer_start_ticks"],
            "cgroup_sha256": target["writer_cgroup_sha256"],
        },
        "rpc_origin": "http://127.0.0.1:9090",
        "public_height_receipt_sha256": public_sha256,
        "authenticated_height_cross_proof_sha256": cross_sha256,
        "snapshot_endpoint": "/sync/snapshot",
        "snapshot_listener": {
            "boot_id": target["boot_id"],
            "pid": target["writer_pid"],
            "start_ticks": target["writer_start_ticks"],
            "port": 9090,
            "socket_inode": seed + 7,
        },
        "capture_attempt_id": f"00000000-0000-4000-8000-{index:012d}",
        "capture_started_at": completed_at,
        "capture_completed_at": completed_at,
        "inspector_binary_sha256": f"{seed + 8:064x}",
        "genesis_sha256": genesis_root,
        "legacy_validator_set_sha256": legacy_root,
        "fixed_pair_path": (
            f"/root/arc-recovery-live-source-captures/{CAPTURE}/{target['node']}/"
            f"round-{round_number}/preauthorization-boundary/attempt-{index}/fixed-source"
        ),
        "snapshot_source": "sealed-writer-owned-loopback-/sync/snapshot",
        "existing_source_snapshot_used": False,
        "rust_capture": qr.wrap(rust_capture),
        "head": head,
        "ancestry_checks": [
            {
                "label": "public-latest",
                "height": public_row["latest_block_height"],
                "expected_block_hash": public_row["latest_block_hash"],
                "observed_block_hash": public_row["latest_block_hash"],
                "state_root": f"{seed + 9:064x}",
                "inspection_sha256": f"{seed + 10:064x}",
            },
            {
                "label": "authenticated-loopback-latest",
                "height": cross_row["loopback_latest_height"],
                "expected_block_hash": cross_row["loopback_latest_block_hash"],
                "observed_block_hash": cross_row["loopback_latest_block_hash"],
                "state_root": f"{seed + 11:064x}",
                "inspection_sha256": f"{seed + 12:064x}",
            },
        ],
        "content_sealed": True,
        "strict_offline_replay": True,
        "source_pair_role": "preauthorization-boundary",
        "minimum_height": max(
            public_row["info_after_height"], cross_row["loopback_info_after_height"]
        ),
        "expected_head": None,
        "boundary_proof_sha256": cross_sha256,
        "network_quarantine_receipt_sha256": None,
        "owned_ruleset_stateless_sha256": None,
    }
    return qr.wrap(value)


def prior_rows(results: list[dict], observed_at: str) -> list[dict]:
    by_name = {}
    for result in results:
        for wrapper in result["transitions"]:
            item = wrapper["value"]
            current = {
                "schema": qr.PRIOR_STATUS_SCHEMA, "capture_id": item["capture_id"],
                "freeze_plan_sha256": item["freeze_plan_sha256"], "node": item["node"],
                "host": item["host"], "node_transition_receipt_sha256": wrapper["sha256"],
                "observed_at": observed_at, "writer_state": "exact-live-fenced",
                "boot_id": item["boot_id"], "writer_pid": item["writer_pid"],
                "writer_start_ticks": item["writer_start_ticks"],
                "writer_cgroup_sha256": item["writer_cgroup_sha256"],
                "network_quarantine_receipt_sha256": item["network_quarantine_receipt_sha256"],
                "owned_ruleset_stateless_sha256": item[
                    "owned_ruleset_stateless_sha256"
                ],
                "stable_head": item["stable_head"], "active": True, "enabled": True,
                "persistent_restart_fence_sha256": None,
            }
            by_name[item["node"]] = {
                "node": item["node"], "host": item["host"],
                "node_transition_receipt_sha256": wrapper["sha256"],
                "transition_schema": item["schema"],
                "transitioned_at": item["nft_applied_at"],
                "stable_head": item["stable_head"],
                "persistent_restart_fence_sha256": item["persistent_restart_fence_sha256"],
                "current_status": qr.wrap(current),
            }
    return [by_name[name] for name, _host in qr.FLEET if name in by_name]


def authorization(number: int, results: list[dict], targets: list[str], offset: int) -> dict:
    public = public_receipt(targets, offset);cross = cross_receipt(targets, public, offset)
    public_sha = qr.digest(public)
    cross_sha = qr.digest(cross)
    return {
        "schema": qr.ROUND_AUTH_SCHEMA, "source_main_commit": SOURCE, "capture_id": CAPTURE,
        "freeze_plan_sha256": FREEZE, "round_number": number,
        "prior_round_result_sha256s": [qr.digest(result) for result in results],
        "prior_fenced": prior_rows(results, utc(offset + 5)),
        "targets": [target_row(name) for name in targets],
        "public_height_receipt": qr.wrap(public),
        "authenticated_height_cross_proof": qr.wrap(cross),
        "live_source_captures": [
            live_source_capture(
                target,
                next(row for row in public["origins"] if row["name"] == target["node"]),
                next(row for row in cross["nodes"] if row["node"] == target["node"]),
                round_number=number,
                public_sha256=public_sha,
                cross_sha256=cross_sha,
                completed_at=cross["completed_at"],
            )
            for target in (target_row(name) for name in targets)
        ],
        "authorized_at": utc(offset + 6), "authorization_deadline": utc(offset + 302),
    }


def target_readiness(auth: dict) -> dict:
    auth_sha = qr.digest(auth)
    rows = []
    for target in auth["targets"]:
        acceptance = {
            "schema": qr.AUTH_ACCEPTANCE_SCHEMA,
            "capture_id": CAPTURE, "freeze_plan_sha256": FREEZE,
            "round_number": auth["round_number"],
            "round_authorization_sha256": auth_sha,
            "node": target["node"], "host": target["host"],
            "accepted_at": auth["authorized_at"],
            "authorization_deadline": auth["authorization_deadline"],
        }
        rows.append({
            "node": target["node"], "host": target["host"],
            "authorization_acceptance": qr.wrap(acceptance),
        })
    return {
        "schema": qr.READINESS_SCHEMA, "capture_id": CAPTURE,
        "freeze_plan_sha256": FREEZE, "round_number": auth["round_number"],
        "round_authorization_sha256": auth_sha,
        "targets": rows, "completed_at": auth["authorized_at"],
        "authorization_deadline": auth["authorization_deadline"],
    }


def applied(auth: dict, name: str, second: int, height: int) -> dict:
    target = next(row for row in auth["targets"] if row["node"] == name)
    public_row = next(
        row for row in auth["public_height_receipt"]["value"]["origins"]
        if row["name"] == name
    )
    cross_row = next(
        row for row in auth["authenticated_height_cross_proof"]["value"]["nodes"]
        if row["node"] == name
    )
    policy_source_sha = H["5"]
    observed_ruleset_sha = H["6"]
    readiness_sha = qr.digest(target_readiness(auth))
    binding = {
        "schema": qr.TABLE_BINDING_SCHEMA, "capture_id": CAPTURE,
        "freeze_plan_sha256": FREEZE, "round_number": auth["round_number"],
        "round_authorization_sha256": qr.digest(auth),
        "round_readiness_sha256": readiness_sha,
        "authorization_deadline": auth["authorization_deadline"],
        "apply_helper_sha256": H["9"], "policy_sha256": policy_source_sha,
        "node": name, "host": qr.FLEET_MAP[name],
        "writer": {
            "boot_id": target["boot_id"], "pid": target["writer_pid"],
            "start_ticks": target["writer_start_ticks"],
            "cgroup_sha256": target["writer_cgroup_sha256"],
        },
    }
    binding_sha = qr.digest(binding)
    gate = {
        "schema": qr.NFT_GATE_SCHEMA, "capture_id": CAPTURE,
        "freeze_plan_sha256": FREEZE, "round_authorization_sha256": qr.digest(auth),
        "round_readiness_sha256": readiness_sha,
        "round_number": auth["round_number"], "node": name, "host": qr.FLEET_MAP[name],
        "authorization_deadline": auth["authorization_deadline"],
        "invoked_at": utc(second), "apply_helper_sha256": H["9"],
        "policy_sha256": policy_source_sha,
        "table_binding_sha256": binding_sha,
        "table_comment": (
            f"arc-recovery:round={auth['round_number']}:bind={binding_sha}:node={name}"
        ),
    }
    ancestry = {
        "schema": qr.ANCESTRY_SCHEMA, "capture_id": CAPTURE,
        "freeze_plan_sha256": FREEZE,
        "round_authorization_sha256": qr.digest(auth),
        "round_number": auth["round_number"], "node": name,
        "host": qr.FLEET_MAP[name],
        "checks": [
            {
                "label": "public-latest", "height": public_row["latest_block_height"],
                "expected_block_hash": public_row["latest_block_hash"],
                "observed_block_hash": public_row["latest_block_hash"],
                "response_sha256": H["10"],
            },
            {
                "label": "authenticated-loopback-latest",
                "height": cross_row["loopback_latest_height"],
                "expected_block_hash": cross_row["loopback_latest_block_hash"],
                "observed_block_hash": cross_row["loopback_latest_block_hash"],
                "response_sha256": H["11"],
            },
        ],
    }
    applied_commit = {
        "schema": "arc.recovery.quarantine-nft-applied-commit.v1",
        "capture_id": CAPTURE, "freeze_plan_sha256": FREEZE,
        "round_number": auth["round_number"],
        "round_authorization_sha256": qr.digest(auth),
        "round_readiness_sha256": readiness_sha,
        "node": name, "host": qr.FLEET_MAP[name],
        "nft_deadline_gate_sha256": qr.digest(gate),
        "table_binding_sha256": binding_sha,
        "table_comment": gate["table_comment"],
        "apply_helper_sha256": H["9"],
        "nft_policy_source_sha256": policy_source_sha,
        "owned_ruleset_stateless_sha256": observed_ruleset_sha,
        "nft_applied_at": utc(second),
    }
    apply_intent = {
        "schema": qr.NFT_INTENT_SCHEMA, "capture_id": CAPTURE,
        "freeze_plan_sha256": FREEZE, "source_main_commit": SOURCE,
        "round_number": auth["round_number"],
        "round_authorization_sha256": qr.digest(auth),
        "round_readiness_sha256": readiness_sha,
        "authorization_deadline": auth["authorization_deadline"],
        "node": name, "host": qr.FLEET_MAP[name], "writer": binding["writer"],
        "table_binding_sha256": binding_sha, "table_comment": gate["table_comment"],
        "apply_helper_sha256": H["9"], "nft_policy_source_sha256": policy_source_sha,
        "prepared_at": utc(second - 1),
    }
    restart_sha = H["20"]
    file_roots = {
        "authorization.json": qr.digest(auth),
        "readiness.json": readiness_sha,
        "contract.json": H["21"], "table-binding.json": binding_sha,
        "nft-apply-intent.json": qr.digest(apply_intent),
        "policy.nft": policy_source_sha, "apply": H["9"], "nft": H["13"],
        "nft-deadline-gate.json": qr.digest(gate),
        "applied.commit.json": qr.digest(applied_commit),
        "persistent-restart-fence.json": restart_sha,
        "rendered-policy.nft": H["22"],
        "/usr/local/libexec/arc-legacy-maintenance-fence": H["23"],
        "/etc/systemd/system/arc-legacy-maintenance-fence.service": H["24"],
        "/etc/systemd/system/arc-self-heal.service.d/zzzy-arc-recovery-network-fence.conf": H["25"],
        "/etc/systemd/system/arc-node.service.d/zzzy-arc-recovery-network-fence.conf": H["26"],
        "/etc/systemd/system/arc-node-update.service.d/zzzy-arc-recovery-network-fence.conf": H["27"],
        "/etc/systemd/system/arc-node-update.timer.d/zzzy-arc-recovery-network-fence.conf": H["28"],
    }
    stable_head = {"height": height, "block_hash": H["7"], "state_root": H["8"]}
    network_receipt = {
        "schema": "arc.recovery.legacy-network-quarantine.v1",
        "capture_id": CAPTURE, "node": name, "host": qr.FLEET_MAP[name],
        "freeze_plan_sha256": FREEZE, "source_main_commit": SOURCE,
        "round_number": auth["round_number"],
        "round_authorization_sha256": qr.digest(auth),
        "round_readiness_sha256": readiness_sha,
        "nft_deadline_gate_sha256": qr.digest(gate),
        "nft_apply_intent_sha256": qr.digest(apply_intent),
        "nft_apply_intent": qr.wrap(apply_intent),
        "nft_table_binding_sha256": binding_sha,
        "nft_table_binding": binding,
        "table_comment": gate["table_comment"],
        "nft_table_comment": gate["table_comment"],
        "nft_policy_source_sha256": policy_source_sha,
        "apply_helper_sha256": H["9"],
        "applied_commit_sha256": qr.digest(applied_commit),
        "authorization_ancestry_proof_sha256": qr.digest(ancestry),
        "boot_id": target["boot_id"],
        "writer": {
            "pid": target["writer_pid"], "start_ticks": target["writer_start_ticks"],
            "cgroup_sha256": target["writer_cgroup_sha256"],
        },
        "table": {
            "family": "inet", "name": "arc_legacy_maintenance_v1",
            "priority": -310, "hooks": ["prerouting", "input", "forward", "output"],
            "policy": "accept", "comment": gate["table_comment"],
            "loopback_retained": True,
        },
        "quarantine_policy": {
            "mode": "deny-all-nonloopback-except-host-maintenance",
            "families": ["ipv4", "ipv6"],
            "directions": ["input", "output", "forward"],
            "allowed": ["loopback", "ssh-tcp-22", "dhcpv4-67-68",
                        "dhcpv6-546-547", "icmpv6-ndp-ra-packet-too-big"],
            "priority_before_conntrack": True, "established_bypass": False,
            "legacy_rpc_p2p_web_dynamic_all_blocked": True,
        },
        "persistence": {
            "unit_path": "/etc/systemd/system/arc-legacy-maintenance-fence.service",
            "unit_enabled": True, "unit_active": True,
            "state_path": "/etc/arc-recovery/network-fence-rounds/test",
            "active_selector_path": "/run/arc-recovery/active-network-fence",
            "automatic_unfence": False,
        },
        "file_sha256": file_roots,
        "tool_sha256": {"/usr/sbin/nft": H["13"]},
        "owned_ruleset_stateless_sha256": observed_ruleset_sha,
        "loopback_head": {
            "rpc_origin": "http://127.0.0.1:9090",
            "info_before_height": height, "latest_height": height,
            "block_height": height, "info_after_height": height,
            "block_hash": H["7"], "state_root": H["8"],
            "response_sha256": {
                "/info:before": H["15"], "/block/latest": H["16"],
                f"/block/{height}": H["17"], "/health": H["18"],
                "/info:after": H["19"],
            },
            "stable_attempt": 1,
        },
        "stable_head": stable_head,
        "authorization_ancestry_proof": qr.wrap(ancestry),
        "nft_deadline_gate": qr.wrap(gate),
        "applied_commit": qr.wrap(applied_commit),
        "installed_at": utc(second + 1),
        "global_absence_claimed": False,
        "threat_model": {
            "legacy_binary": "reviewed-non-adversarial-exact-hash",
            "legacy_binary_sha256": H["14"],
        },
    }
    return {
        "schema": qr.NODE_APPLIED_SCHEMA, "capture_id": CAPTURE,
        "freeze_plan_sha256": FREEZE, "round_authorization_sha256": qr.digest(auth),
        "round_readiness_sha256": qr.digest(target_readiness(auth)),
        "round_number": auth["round_number"], "node": name, "host": qr.FLEET_MAP[name],
        "boot_id": target["boot_id"], "writer_pid": target["writer_pid"],
        "writer_start_ticks": target["writer_start_ticks"],
        "writer_cgroup_sha256": target["writer_cgroup_sha256"],
        "nft_policy_source_sha256": policy_source_sha,
        "owned_ruleset_stateless_sha256": observed_ruleset_sha,
        "nft_applied_at": utc(second),
        "nft_deadline_gate": qr.wrap(gate),
        "network_quarantine_receipt": qr.wrap(network_receipt),
        "network_quarantine_receipt_sha256": qr.digest(network_receipt),
        "stable_head": stable_head,
        "authorization_ancestry_proof": qr.wrap(ancestry),
        "persistent_restart_fence_sha256": restart_sha,
    }


def result(auth: dict, items: list[dict], completed: int) -> dict:
    transitioned_names = {item["node"] for item in items}
    return {
        "schema": qr.ROUND_RESULT_SCHEMA, "capture_id": CAPTURE,
        "freeze_plan_sha256": FREEZE, "round_number": auth["round_number"],
        "round_authorization_sha256": qr.digest(auth),
        "target_readiness": qr.wrap(target_readiness(auth)),
        "transitions": [qr.wrap(item) for item in items],
        "remaining_targets": [row["node"] for row in auth["targets"]
                              if row["node"] not in transitioned_names],
        "completed_at": utc(completed),
    }


def authorized_height(auth: dict, name: str) -> int:
    nodes = auth["authenticated_height_cross_proof"]["value"]["nodes"]
    return next(row["loopback_info_after_height"] for row in nodes if row["node"] == name) + 1


def ledger_for_first_successes(count: int) -> dict:
    names = [name for name, _host in qr.FLEET]
    results: list[dict] = [];round_rows = []
    if count == 0:
        auth1 = authorization(1, results, names, 400)
        first_items = [applied(auth1, name, 420 + index, authorized_height(auth1, name))
                       for index, name in enumerate(names)]
        result1 = result(auth1, first_items, 730)
        round_rows.append({"authorization": qr.wrap(auth1), "result": qr.wrap(result1)})
        results.append(result1)
        remaining = []
    else:
        auth1 = authorization(1, results, names, 0)
        first_items = [applied(auth1, name, 20 + index, authorized_height(auth1, name))
                       for index, name in enumerate(names[:count])]
        result1 = result(auth1, first_items, 330)
        round_rows.append({"authorization": qr.wrap(auth1), "result": qr.wrap(result1)})
        results.append(result1)
        remaining = names[count:]
    if remaining:
        auth2 = authorization(2, results, remaining, 400)
        second_items = [applied(auth2, name, 420 + index, authorized_height(auth2, name))
                        for index, name in enumerate(remaining)]
        result2 = result(auth2, second_items, 730)
        round_rows.append({"authorization": qr.wrap(auth2), "result": qr.wrap(result2)})
        results.append(result2)
    all_items = [
        wrapper["value"] for row in round_rows
        for wrapper in row["result"]["value"]["transitions"]
    ]
    projections = [qr.validate_node_transition(item) for item in all_items]
    public_maxima = [row["authorization"]["value"]["public_height_receipt"]["value"]
                     ["legacy_public_max_height"] for row in round_rows]
    return {
        "schema": qr.LEDGER_SCHEMA, "capture_id": CAPTURE, "freeze_plan_sha256": FREEZE,
        "fleet": [{"node": name, "host": host} for name, host in qr.FLEET],
        "rounds": round_rows,
        "first_secured_at": min(
            item["secured_at"] for item in projections
        ).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "all_nodes_secured_at": max(
            item["verified_at"] for item in projections
        ).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "legacy_cutoff_height": max([*public_maxima, *(item["stable_head"]["height"] for item in all_items)]),
    }


class QuarantineRoundTests(unittest.TestCase):
    def test_authorization_requires_exact_ordered_live_source_captures(self) -> None:
        names = [name for name, _host in qr.FLEET]
        valid = authorization(1, [], names, 0)
        self.assertEqual(
            qr.validate_round_authorization(valid)["target_names"], names
        )

        missing_field = copy.deepcopy(valid)
        missing_field.pop("live_source_captures")
        with self.assertRaisesRegex(qr.QuarantineRoundError, "fields/schema"):
            qr.validate_round_authorization(missing_field)

        missing_node = copy.deepcopy(valid)
        missing_node["live_source_captures"].pop()
        with self.assertRaisesRegex(qr.QuarantineRoundError, "capture set"):
            qr.validate_round_authorization(missing_node)

        reordered = copy.deepcopy(valid)
        reordered["live_source_captures"][0], reordered["live_source_captures"][1] = (
            reordered["live_source_captures"][1],
            reordered["live_source_captures"][0],
        )
        with self.assertRaisesRegex(qr.QuarantineRoundError, "identity|order"):
            qr.validate_round_authorization(reordered)

        wrong_role = copy.deepcopy(valid)
        capture = wrong_role["live_source_captures"][0]["value"]
        capture["source_pair_role"] = "post-quarantine-final-export"
        wrong_role["live_source_captures"][0] = qr.wrap(capture)
        with self.assertRaisesRegex(qr.QuarantineRoundError, "identity/policy"):
            qr.validate_round_authorization(wrong_role)

    def test_mixed_state_success_counts(self) -> None:
        for count in (0, 1, 2, 3, 5):
            with self.subTest(first_round_successes=count):
                state = qr.validate_generation_ledger(ledger_for_first_successes(count))
                self.assertEqual(state["round_count"], 1 if count == 0 else 2)

    def test_zero_progress_attempt_is_valid_evidence_but_not_a_ledger_transition(self) -> None:
        names = [name for name, _host in qr.FLEET]
        auth = authorization(1, [], names, 0)
        empty = result(auth, [], 303)
        state = qr.validate_round_result(
            empty, authorization=auth, prior_results=[], transition_receipts=[]
        )
        self.assertEqual(state["transitioned_names"], [])
        ledger = ledger_for_first_successes(6)
        ledger["rounds"][0] = {"authorization": qr.wrap(auth), "result": qr.wrap(empty)}
        with self.assertRaisesRegex(qr.QuarantineRoundError, "zero-progress"):
            qr.validate_generation_ledger(ledger)

    def test_reordered_transition_receipts_rejected(self) -> None:
        value = ledger_for_first_successes(3)
        transition_rows = value["rounds"][0]["result"]["value"]["transitions"]
        transition_rows[0], transition_rows[1] = transition_rows[1], transition_rows[0]
        value["rounds"][0]["result"] = qr.wrap(value["rounds"][0]["result"]["value"])
        with self.assertRaisesRegex(qr.QuarantineRoundError, "order"):
            qr.validate_generation_ledger(value)

    def test_duplicate_node_transition_rejected(self) -> None:
        value = ledger_for_first_successes(3)
        second = value["rounds"][1]["result"]["value"]
        duplicate = copy.deepcopy(
            value["rounds"][0]["result"]["value"]["transitions"][0]
        )
        second["transitions"][0] = duplicate
        second["remaining_targets"] = [row["node"] for row in value["rounds"][1]
                                       ["authorization"]["value"]["targets"]][1:]
        value["rounds"][1]["result"] = qr.wrap(second)
        with self.assertRaises(qr.QuarantineRoundError):
            qr.validate_generation_ledger(value)

    def test_substituted_prior_root_rejected_even_when_rehashed(self) -> None:
        value = ledger_for_first_successes(2)
        auth2 = value["rounds"][1]["authorization"]["value"]
        auth2["prior_fenced"][0]["node_transition_receipt_sha256"] = H["99"]
        value["rounds"][1]["authorization"] = qr.wrap(auth2)
        with self.assertRaisesRegex(qr.QuarantineRoundError, "re-prove|derive"):
            qr.validate_generation_ledger(value)

    def test_apply_after_round_deadline_rejected(self) -> None:
        value = ledger_for_first_successes(1)
        first = value["rounds"][0]
        item = first["result"]["value"]["transitions"][0]["value"]
        item["nft_applied_at"] = utc(303)
        gate = item["nft_deadline_gate"]["value"]
        gate["invoked_at"] = utc(303)
        item["nft_deadline_gate"] = qr.wrap(gate)
        network = item["network_quarantine_receipt"]["value"]
        network["installed_at"] = utc(304)
        network["nft_deadline_gate_sha256"] = qr.digest(gate)
        network["nft_deadline_gate"] = qr.wrap(gate)
        network["file_sha256"]["nft-deadline-gate.json"] = qr.digest(gate)
        commit = network["applied_commit"]["value"]
        commit["nft_deadline_gate_sha256"] = qr.digest(gate)
        commit["nft_applied_at"] = utc(303)
        network["applied_commit"] = qr.wrap(commit)
        network["applied_commit_sha256"] = qr.digest(commit)
        network["file_sha256"]["applied.commit.json"] = qr.digest(commit)
        item["network_quarantine_receipt"] = qr.wrap(network)
        item["network_quarantine_receipt_sha256"] = qr.digest(network)
        first["result"]["value"]["transitions"][0] = qr.wrap(item)
        first["result"] = qr.wrap(first["result"]["value"])
        with self.assertRaisesRegex(qr.QuarantineRoundError, "deadline"):
            qr.validate_generation_ledger(value)

    def test_reboot_or_writer_identity_drift_rejected(self) -> None:
        for field, replacement in (
            ("boot_id", "ffffffff-ffff-ffff-ffff-ffffffffffff"),
            ("writer_start_ticks", 999999),
            ("writer_cgroup_sha256", H["99"]),
        ):
            value = ledger_for_first_successes(1)
            first = value["rounds"][0]
            item = first["result"]["value"]["transitions"][0]["value"]
            item[field] = replacement
            first["result"]["value"]["transitions"][0] = qr.wrap(item)
            first["result"] = qr.wrap(first["result"]["value"])
            with self.subTest(field=field), self.assertRaisesRegex(
                qr.QuarantineRoundError, "writer identity|table binding"
            ):
                qr.validate_generation_ledger(value)

    def test_result_may_complete_after_deadline_if_each_apply_was_timely(self) -> None:
        value = ledger_for_first_successes(6)
        value["rounds"][0]["result"]["value"]["completed_at"] = utc(10000)
        value["rounds"][0]["result"] = qr.wrap(value["rounds"][0]["result"]["value"])
        state = qr.validate_generation_ledger(value)
        self.assertEqual(state["round_count"], 1)

    def test_partial_result_cannot_close_while_old_helpers_remain_authorized(self) -> None:
        names = [name for name, _host in qr.FLEET]
        auth = authorization(1, [], names, 0)
        one = applied(auth, names[0], 20, authorized_height(auth, names[0]))
        premature = result(auth, [one], 100)
        with self.assertRaisesRegex(qr.QuarantineRoundError, "before.*expired"):
            qr.validate_round_result(
                premature, authorization=auth, prior_results=[], transition_receipts=[one]
            )

    def test_extra_prior_fenced_row_is_rejected_before_mutation(self) -> None:
        value = ledger_for_first_successes(1)
        auth2 = value["rounds"][1]["authorization"]["value"]
        fabricated = copy.deepcopy(auth2["prior_fenced"][0])
        fabricated["node"] = "lax"
        fabricated["host"] = qr.FLEET_MAP["lax"]
        auth2["prior_fenced"].append(fabricated)
        with self.assertRaises(qr.QuarantineRoundError):
            qr.validate_round_authorization(
                auth2,
                prior_results=[value["rounds"][0]["result"]["value"]],
            )

    def test_ancestry_policy_and_install_timeline_are_cross_bound(self) -> None:
        mutations = []
        ancestry = ledger_for_first_successes(6)
        item = ancestry["rounds"][0]["result"]["value"]["transitions"][0]["value"]
        proof = item["authorization_ancestry_proof"]["value"]
        proof["checks"][0]["observed_block_hash"] = H["99"]
        item["authorization_ancestry_proof"] = qr.wrap(proof)
        mutations.append(ancestry)

        policy = ledger_for_first_successes(6)
        item = policy["rounds"][0]["result"]["value"]["transitions"][0]["value"]
        item["nft_policy_source_sha256"] = item["owned_ruleset_stateless_sha256"]
        mutations.append(policy)

        installed = ledger_for_first_successes(6)
        item = installed["rounds"][0]["result"]["value"]["transitions"][0]["value"]
        receipt = item["network_quarantine_receipt"]["value"]
        receipt["installed_at"] = utc(19)
        item["network_quarantine_receipt"] = qr.wrap(receipt)
        item["network_quarantine_receipt_sha256"] = qr.digest(receipt)
        mutations.append(installed)

        for value in mutations:
            result_value = value["rounds"][0]["result"]["value"]
            result_value["transitions"][0] = qr.wrap(
                result_value["transitions"][0]["value"]
            )
            value["rounds"][0]["result"] = qr.wrap(result_value)
            with self.assertRaises(qr.QuarantineRoundError):
                qr.validate_generation_ledger(value)


if __name__ == "__main__":
    unittest.main()
