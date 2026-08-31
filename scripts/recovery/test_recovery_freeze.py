#!/usr/bin/env python3
"""Focused unit tests for the standalone recovery_freeze v5 primitives."""

from __future__ import annotations

import copy
import hashlib
import importlib.util
import os
import pathlib
import stat
import sys
import tempfile
import unittest
from types import SimpleNamespace


MODULE_PATH = pathlib.Path(__file__).with_name("recovery_freeze.py")
SPEC = importlib.util.spec_from_file_location("arc_recovery_freeze", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
rf = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = rf
SPEC.loader.exec_module(rf)


SEALED_BOOT = "11111111-1111-1111-1111-111111111111"
REBOOTED_BOOT = "22222222-2222-2222-2222-222222222222"
STAMP = "2026-08-27T12:00:00Z"


def digest(character: str = "a") -> str:
    return character * 64


def supervisor_context(unit: str) -> dict[str, object]:
    return {
        "schema": "arc.recovery.supervisor-context.v1",
        "unit": unit,
        "unit_configuration_sha256": digest("b"),
        "lifecycle_hooks": {
            "ExecReload": "",
            "ExecStop": "",
            "ExecStopPost": "",
            "OnFailure": "",
            "OnSuccess": "",
            "SuccessAction": "none",
            "FailureAction": "none",
            "JobTimeoutAction": "none",
        },
        "automatic_lifecycle": {
            "WatchdogUSec": "0",
            "RuntimeMaxUSec": "infinity",
            "RuntimeRandomizedExtraUSec": "0",
            "StopWhenUnneeded": "no",
            "BindsTo": "",
            "PartOf": "",
            "PropagatesStopTo": "",
            "OOMPolicy": "continue",
            "Requires": "-.mount system.slice sysinit.target",
            "Requisite": "",
            "Conflicts": "shutdown.target",
            "Upholds": "",
            "UpheldBy": "",
            "TriggeredBy": "",
            "RequiredBy": "",
            "WantedBy": "",
            "BoundBy": "",
            "ConflictedBy": "",
            "OnFailureOf": "",
            "OnSuccessOf": "",
            "CanReload": "no",
            "StopPropagatedFrom": "",
            "ReloadPropagatedFrom": "",
        },
        "invocation_id": "c" * 32,
        "control_group": f"/system.slice/{unit}",
        "interpreter_payloads": [],
        "allowed_transient_sleep": (
            {
                "path": "/usr/bin/sleep",
                "sha256": digest("9"),
                "argv_policy": "sleep-duration-max-60s-v1",
                "max_seconds": 60,
            }
            if unit == "arc-self-heal.service"
            else None
        ),
        "term_traps_rejected": True,
    }


def prepare_closure_row(
    unit: str,
    *,
    active_state: str,
    sub_state: str,
    main_pid: int,
    control_group: str,
) -> dict[str, str]:
    return {
        "Names": unit,
        "Id": unit,
        "Following": "",
        "ActiveState": active_state,
        "SubState": sub_state,
        "MainPID": str(main_pid),
        "Job": "0",
        "ControlGroup": control_group,
        "FreezerState": "running" if active_state == "active" else "",
        "Restart": "no",
        "KillMode": "process",
        "SendSIGKILL": "no",
        "OOMPolicy": "continue",
        "WatchdogUSec": "0",
        "RuntimeMaxUSec": "infinity",
        "RuntimeRandomizedExtraUSec": "0",
        "CanReload": "no",
        "StopWhenUnneeded": "no",
        "BindsTo": "",
        "PartOf": "",
        "PropagatesStopTo": "",
        "StopPropagatedFrom": "",
        "ReloadPropagatedFrom": "",
        "Upholds": "",
        "UpheldBy": "",
        "TriggeredBy": (
            "arc-node-update.timer" if unit == "arc-node-update.service" else ""
        ),
        "RequiredBy": "",
        "BoundBy": "",
        "ConflictedBy": "",
        "WantedBy": "",
        "OnFailureOf": "",
        "OnSuccessOf": "",
    }


def prepare_barrier(
    selected_unit: str, selected_pid: int, control_group: str
) -> dict[str, object]:
    units = (
        "arc-self-heal.service",
        "arc-node.service",
        "arc-node-update.service",
        "arc-node-update.timer",
    )
    condition = (
        b"[Unit]\nConditionPathExists=/etc/arc-recovery/legacy-start-allowed\n"
    )
    condition_sha = hashlib.sha256(condition).hexdigest()
    marker_sha = hashlib.sha256(
        b"schema=arc.recovery.legacy-start-allow.v1\n"
    ).hexdigest()
    barriers: dict[str, object] = {}
    sources: dict[str, object] = {}
    states: dict[str, object] = {}
    closure: dict[str, object] = {}
    for unit in units:
        barrier_path = (
            f"/etc/systemd/system/{unit}.d/zzzz-arc-recovery-freeze.conf"
        )
        barriers[unit] = {
            "path": barrier_path,
            "sha256": condition_sha,
            "mode": 0o444,
            "uid": 0,
            "gid": 0,
        }
        sources[unit] = [
            {"path": f"/usr/lib/systemd/system/{unit}", "sha256": digest("7")},
            {"path": barrier_path, "sha256": condition_sha},
        ]
        selected = unit == selected_unit
        active_state = "active" if selected else "inactive"
        sub_state = "running" if selected else "dead"
        main_pid = selected_pid if selected else 0
        states[unit] = {
            "active_state": active_state,
            "sub_state": sub_state,
            "main_pid": main_pid,
            "job": "0",
            "enablement": "enabled" if selected else "disabled",
        }
        closure[unit] = prepare_closure_row(
            unit,
            active_state=active_state,
            sub_state=sub_state,
            main_pid=main_pid,
            control_group=control_group if selected else "",
        )
        if selected:
            closure[unit]["WantedBy"] = "multi-user.target"
    return {
        "schema": "arc.recovery.prepare-barrier.v1",
        "allow_marker": {
            "path": rf.DEFAULT_ALLOW_MARKER_PATH,
            "sha256": marker_sha,
            "mode": 0o400,
            "uid": 0,
            "gid": 0,
            "device": 300,
        },
        "persistent_start_barriers": barriers,
        "merged_unit_sources": sources,
        "unit_states": states,
        "activation_closure": closure,
        "boot_activation": {
            "default_target": "graphical.target",
            "default_target_projection": {
                "Names": "graphical.target default.target",
                "Id": "graphical.target",
                "Following": "",
                "LoadState": "loaded",
                "FragmentPath": "/usr/lib/systemd/system/graphical.target",
                "Requires": "multi-user.target",
                "Wants": "display-manager.service",
            },
            "default_target_symlink": {
                "path": "/usr/lib/systemd/system/default.target",
                "target": "graphical.target",
                "device": 300,
                "inode": 400,
                "uid": 0,
                "gid": 0,
            },
            "selected_enablement_symlink": {
                "path": f"/etc/systemd/system/multi-user.target.wants/{selected_unit}",
                "target": f"/etc/systemd/system/{selected_unit}",
                "device": 300,
                "inode": 401,
                "uid": 0,
                "gid": 0,
                "resolved_path": f"/etc/systemd/system/{selected_unit}",
                "resolved_sha256": digest("7"),
            },
            "selected_reached_from_multi_user": True,
            "precommit_reboot_fail_open": True,
        },
        "selected_unit": selected_unit,
        "selected_main_pid": selected_pid,
        "alternatives_inactive_no_jobs": True,
        "alternative_enablement_sync_completed": True,
        "writer_cgroup_relationship_sealed": True,
    }


def node_row(index: int, name: str) -> dict[str, object]:
    unit = "arc-node.service"
    executable_hash = digest("d")
    argv_hash = digest("e")
    context = supervisor_context(unit)
    pid = 1000 + index
    return {
        "name": name,
        "host": f"node-{name}.arc.example",
        "boot_id": SEALED_BOOT,
        "writer_pid": pid,
        "writer_start_ticks": 10_000 + index,
        "writer_cgroup_sha256": digest("f"),
        "writer_cgroup_path": "/system.slice/arc-node.service",
        "writer_cgroup_device": 10,
        "writer_cgroup_inode": 20,
        "writer_supervision_mode": "systemd-unit",
        "supervisor_unit": unit,
        "supervisor_main_pid": pid,
        "supervisor_start_ticks": 10_000 + index,
        "supervisor_executable_path": "/opt/arc/bin/arc-node",
        "supervisor_executable_sha256": executable_hash,
        "supervisor_argv_sha256": argv_hash,
        "supervisor_context": context,
        "supervisor_context_sha256": rf.canonical_sha256(context),
        "prepare_barrier": prepare_barrier(
            unit, pid, "/system.slice/arc-node.service"
        ),
        "executable_path": "/opt/arc/bin/arc-node",
        "executable_sha256": executable_hash,
        "argv_sha256": argv_hash,
        "data_dir": f"/var/lib/arc/{name}",
        "model_path": "/opt/arc/models/model.gguf",
        "model_sha256": digest("1"),
        "model_size_bytes": 4_081_004_224,
        "shard_ranges": [[index, index + 1]],
        "data_device": 100,
        "data_bytes": 1024**3,
        "data_files": 100,
        "capture_device": 200,
        "available_bytes": 20 * 1024**3,
        "available_inodes": 100_000,
        "required_free_bytes": 4 * 1024**3,
        "required_free_inodes": 10_100,
        "new_v3_headroom_bytes": 1024**3,
        "max_binding_temporary_bytes": 3 * 1024**3,
        "archive_stream_temporary_bytes": 0,
        "validator_address": f"{index + 1:064x}",
        "stake": 5_000_000,
        "rpc_origin": "http://127.0.0.1:9090",
        "observed_positive_validators": [],
        "observed_validator_error": None,
    }


def plan_value() -> dict[str, object]:
    remote_root = "arc-recovery-google-drive:ARC-Recovery"
    nodes = [node_row(index, name) for index, name in enumerate(rf.ARC_NODE_ORDER)]
    return {
        "schema": rf.FREEZE_PLAN_SCHEMA,
        "window": "recovery-2026-08-27",
        "created_at": STAMP,
        "sentinels": list(rf.ARC_SENTINEL_ORDER),
        "nodes": nodes,
        "remote_helper_sha256": digest("2"),
        "orchestrator_sha256": digest("3"),
        "rollout_tool_sha256": digest("4"),
        "rollout_schema_sha256": digest("5"),
        "operator_python_path": "/usr/bin/python3",
        "operator_python_sha256": digest("0"),
        "source_commit": "6" * 40,
        "legacy_validator_set_sha256": digest("7"),
        "writer_contracts_sha256": digest("8"),
        "drive_prefreeze": {
            "gate_sha256": digest("9"),
            "remote_root": remote_root,
            "remote_root_sha256": hashlib.sha256(remote_root.encode()).hexdigest(),
            "oauth_client_id_sha256": digest("a"),
            "account_sha256": digest("b"),
            "daily_upload_budget_bytes": 100 * 1024**3,
            "dedicated_no_other_upload_writers_attested": True,
        },
        "quorum_proof": {
            "source_total_stake": 40_000_000,
            "source_quorum_stake": 26_666_667,
            "controlled_writer_stake": 30_000_000,
            "maximum_source_stake_after_controlled_stop": 10_000_000,
            "controlled_quorum_unavailable_after_all_stops": True,
            "global_legacy_halt_claimed": False,
            "external_source_validators": [
                {"address": f"{7:064x}", "stake": 5_000_000},
                {"address": f"{8:064x}", "stake": 5_000_000},
            ],
            "untrusted_external_observations": [],
            "dynamic_membership_disagrees": False,
        },
    }


def detached_plan_value() -> dict[str, object]:
    value = plan_value()
    row = value["nodes"][0]
    unit = "arc-self-heal.service"
    supervisor_pid = 9000
    control_group = "/system.slice/arc-self-heal.service"
    context = supervisor_context(unit)
    row.update(
        {
            "writer_supervision_mode": "detached-root-session",
            "writer_cgroup_path": "/user.slice/user-0.slice/session-42.scope",
            "writer_cgroup_device": 11,
            "writer_cgroup_inode": 21,
            "supervisor_unit": unit,
            "supervisor_main_pid": supervisor_pid,
            "supervisor_start_ticks": 90_000,
            "supervisor_executable_path": "/usr/bin/bash",
            "supervisor_executable_sha256": digest("c"),
            "supervisor_argv_sha256": digest("b"),
            "supervisor_context": context,
            "supervisor_context_sha256": rf.canonical_sha256(context),
            "prepare_barrier": prepare_barrier(
                unit, supervisor_pid, control_group
            ),
        }
    )
    return value


def pinned_plan(value: dict[str, object] | None = None) -> rf.PinnedFreezePlan:
    raw = rf.canonical_json_bytes(plan_value() if value is None else value)
    return rf.validate_pinned_freeze_plan(raw, hashlib.sha256(raw).hexdigest())


def cgroups() -> tuple[rf.CgroupIdentity, rf.CgroupIdentity]:
    return (
        rf.CgroupIdentity("supervisor", "/system.slice/arc-node.service", 10, 20),
        rf.CgroupIdentity("writer", "/system.slice/arc-node.service", 10, 20),
    )


def prepared_and_armed() -> tuple[rf.PinnedFreezePlan, dict[str, object], dict[str, object]]:
    plan = pinned_plan()
    receipt = rf.make_prepare_receipt(
        plan,
        "nyc",
        sealed_boot_id=SEALED_BOOT,
        cgroups=cgroups(),
        prepared_at=STAMP,
    )
    arm = rf.make_barrier_arm_event(receipt, armed_at=STAMP)
    return plan, receipt, arm


class CanonicalJsonTests(unittest.TestCase):
    def test_canonical_json_is_sorted_compact_and_lf_terminated(self) -> None:
        raw = rf.canonical_json_bytes({"z": 1, "a": [True, None]})
        self.assertEqual(raw, b'{"a":[true,null],"z":1}\n')
        self.assertEqual(rf.parse_canonical_json(raw), {"a": [True, None], "z": 1})

    def test_noncanonical_duplicate_and_nonfinite_json_are_rejected(self) -> None:
        for raw in (
            b'{ "a":1}\n',
            b'{"a":1,"a":2}\n',
            b'{"a":NaN}\n',
            b'{"a":1}',
        ):
            with self.subTest(raw=raw), self.assertRaises(rf.FreezeValidationError):
                rf.parse_canonical_json(raw)


class SecureFileTests(unittest.TestCase):
    def test_stat_requires_regular_expected_owner_and_no_write_bits(self) -> None:
        good = SimpleNamespace(st_mode=stat.S_IFREG | 0o444, st_uid=0)
        rf.validate_secure_stat(good)
        for details in (
            SimpleNamespace(st_mode=stat.S_IFDIR | 0o555, st_uid=0),
            SimpleNamespace(st_mode=stat.S_IFREG | 0o444, st_uid=501),
            SimpleNamespace(st_mode=stat.S_IFREG | 0o644, st_uid=0),
        ):
            with self.subTest(details=details), self.assertRaises(rf.FreezeValidationError):
                rf.validate_secure_stat(details)

    def test_nofollow_reader_reads_locked_file_and_rejects_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source = root / "journal.json"
            source.write_bytes(b"locked\n")
            source.chmod(0o444)
            self.assertEqual(
                rf.read_regular_nofollow(source, expected_uid=os.getuid()), b"locked\n"
            )
            alias = root / "alias.json"
            alias.symlink_to(source)
            with self.assertRaises(rf.FreezeValidationError):
                rf.read_regular_nofollow(alias, expected_uid=os.getuid())

    def test_secure_loader_validates_plan_from_locked_inode(self) -> None:
        raw = rf.canonical_json_bytes(plan_value())
        expected = hashlib.sha256(raw).hexdigest()
        with tempfile.TemporaryDirectory() as temporary:
            path = pathlib.Path(temporary) / "freeze-plan.json"
            path.write_bytes(raw)
            path.chmod(0o444)
            plan = rf.load_pinned_freeze_plan(path, expected, expected_uid=os.getuid())
        self.assertEqual(tuple(node.name for node in plan.nodes), rf.ARC_NODE_ORDER)


class FreezePlanTests(unittest.TestCase):
    def test_full_plan_extracts_unique_immutable_node_contracts(self) -> None:
        plan = pinned_plan()
        self.assertEqual(tuple(node.name for node in plan.nodes), rf.ARC_NODE_ORDER)
        self.assertEqual(plan.sentinels, rf.ARC_SENTINEL_ORDER)
        self.assertEqual(plan.node("nyc").host, "node-nyc.arc.example")
        self.assertEqual(
            hashlib.sha256(plan.node("nyc").canonical_bytes).hexdigest(),
            plan.node("nyc").sha256,
        )
        first = plan.node("nyc").value()
        first["host"] = "mutated.example"
        self.assertEqual(plan.node("nyc").host, "node-nyc.arc.example")

    def test_digest_is_checked_before_json_contract(self) -> None:
        raw = rf.canonical_json_bytes(plan_value())
        with self.assertRaisesRegex(rf.FreezeValidationError, "digest differs"):
            rf.validate_pinned_freeze_plan(raw + b" ", hashlib.sha256(raw).hexdigest())

    def test_unknown_fields_and_duplicate_node_identities_fail_closed(self) -> None:
        variants: list[dict[str, object]] = []
        unknown = plan_value()
        unknown["future"] = True
        variants.append(unknown)
        duplicate_host = plan_value()
        duplicate_host["nodes"][1]["host"] = duplicate_host["nodes"][0]["host"]
        variants.append(duplicate_host)
        duplicate_validator = plan_value()
        duplicate_validator["nodes"][1]["validator_address"] = duplicate_validator["nodes"][0][
            "validator_address"
        ]
        variants.append(duplicate_validator)
        for value in variants:
            raw = rf.canonical_json_bytes(value)
            with self.subTest(value=value), self.assertRaises(rf.FreezeValidationError):
                rf.validate_pinned_freeze_plan(raw, hashlib.sha256(raw).hexdigest())

    def test_cross_field_quorum_and_node_contract_drift_is_rejected(self) -> None:
        bad_quorum = plan_value()
        bad_quorum["quorum_proof"]["controlled_writer_stake"] = 25_000_000
        bad_context = plan_value()
        bad_context["nodes"][0]["supervisor_context"]["term_traps_rejected"] = False
        for value in (bad_quorum, bad_context):
            raw = rf.canonical_json_bytes(value)
            with self.subTest(value=value), self.assertRaises(rf.FreezeValidationError):
                rf.validate_pinned_freeze_plan(raw, hashlib.sha256(raw).hexdigest())

    def test_exact_detached_root_session_contract_is_supported(self) -> None:
        plan = pinned_plan(detached_plan_value())
        node = plan.node("nyc")
        self.assertEqual(node.writer_supervision_mode, "detached-root-session")
        self.assertEqual(
            node.writer_cgroup_path,
            "/user.slice/user-0.slice/session-42.scope",
        )
        receipt = rf.make_prepare_receipt(
            plan,
            "nyc",
            sealed_boot_id=SEALED_BOOT,
            cgroups=(
                rf.CgroupIdentity(
                    "supervisor", "/system.slice/arc-self-heal.service", 12, 22
                ),
                rf.CgroupIdentity(
                    "writer", "/user.slice/user-0.slice/session-42.scope", 11, 21
                ),
            ),
            prepared_at=STAMP,
        )
        rf.validate_prepare_receipt(receipt, plan=plan)

    def test_detached_root_session_shape_is_exact(self) -> None:
        malformed_path = detached_plan_value()
        malformed_path["nodes"][0]["writer_cgroup_path"] = "/user.slice/user-1000.slice/session-42.scope"
        selected_direct_unit = detached_plan_value()
        row = selected_direct_unit["nodes"][0]
        row["supervisor_unit"] = "arc-node.service"
        row["supervisor_context"] = supervisor_context("arc-node.service")
        row["supervisor_context_sha256"] = rf.canonical_sha256(row["supervisor_context"])
        row["prepare_barrier"] = prepare_barrier(
            "arc-node.service", row["supervisor_main_pid"], "/system.slice/arc-node.service"
        )
        nested_cgroup = detached_plan_value()
        nested_cgroup["nodes"][0]["writer_cgroup_path"] = (
            "/system.slice/arc-self-heal.service/detached.scope"
        )
        for value in (malformed_path, selected_direct_unit, nested_cgroup):
            raw = rf.canonical_json_bytes(value)
            with self.subTest(value=value), self.assertRaises(rf.FreezeValidationError):
                rf.validate_pinned_freeze_plan(raw, hashlib.sha256(raw).hexdigest())

    def test_writer_cgroup_inode_fields_are_required_and_validated(self) -> None:
        variants = []
        for field in (
            "writer_cgroup_path",
            "writer_cgroup_device",
            "writer_cgroup_inode",
        ):
            value = plan_value()
            del value["nodes"][0][field]
            variants.append(value)
        root_cgroup = plan_value()
        root_cgroup["nodes"][0]["writer_cgroup_path"] = "/"
        variants.append(root_cgroup)
        for value in variants:
            raw = rf.canonical_json_bytes(value)
            with self.subTest(value=value), self.assertRaises(rf.FreezeValidationError):
                rf.validate_pinned_freeze_plan(raw, hashlib.sha256(raw).hexdigest())

    def test_prepare_barrier_marker_and_four_condition_files_are_exact(self) -> None:
        marker_drift = plan_value()
        marker_drift["nodes"][0]["prepare_barrier"]["allow_marker"]["sha256"] = digest("0")
        marker_boolean_owner = plan_value()
        marker_boolean_owner["nodes"][0]["prepare_barrier"]["allow_marker"]["uid"] = False
        missing_unit = plan_value()
        del missing_unit["nodes"][0]["prepare_barrier"]["persistent_start_barriers"][
            "arc-node-update.timer"
        ]
        barrier_drift = plan_value()
        barrier_drift["nodes"][0]["prepare_barrier"]["persistent_start_barriers"][
            "arc-node-update.service"
        ]["sha256"] = digest("0")
        unmerged_barrier = plan_value()
        unmerged_barrier["nodes"][0]["prepare_barrier"]["merged_unit_sources"][
            "arc-node-update.timer"
        ].pop()
        later_override = plan_value()
        later_override["nodes"][0]["prepare_barrier"]["merged_unit_sources"][
            "arc-node.service"
        ].append(
            {
                "path": "/etc/systemd/system/arc-node.service.d/zzzzz-override.conf",
                "sha256": digest("3"),
            }
        )
        for value in (
            marker_drift,
            marker_boolean_owner,
            missing_unit,
            barrier_drift,
            unmerged_barrier,
            later_override,
        ):
            raw = rf.canonical_json_bytes(value)
            with self.subTest(value=value), self.assertRaises(rf.FreezeValidationError):
                rf.validate_pinned_freeze_plan(raw, hashlib.sha256(raw).hexdigest())

    def test_prepare_selected_and_alternative_states_are_sealed(self) -> None:
        selected_stopped = plan_value()
        selected_stopped["nodes"][0]["prepare_barrier"]["unit_states"][
            "arc-node.service"
        ]["active_state"] = "inactive"
        alternative_active = plan_value()
        alternative_active["nodes"][0]["prepare_barrier"]["unit_states"][
            "arc-self-heal.service"
        ]["active_state"] = "active"
        alternative_enabled = plan_value()
        alternative_enabled["nodes"][0]["prepare_barrier"]["unit_states"][
            "arc-node-update.timer"
        ]["enablement"] = "enabled"
        reverse_edge = plan_value()
        reverse_edge["nodes"][0]["prepare_barrier"]["activation_closure"][
            "arc-self-heal.service"
        ]["WantedBy"] = "multi-user.target"
        alias_closure = plan_value()
        alias_closure["nodes"][0]["prepare_barrier"]["activation_closure"][
            "arc-node-update.service"
        ]["Names"] = "arc-node-update.service update-alias.service"
        relationship_unsealed = plan_value()
        relationship_unsealed["nodes"][0]["prepare_barrier"][
            "writer_cgroup_relationship_sealed"
        ] = False
        enablement_not_durable = plan_value()
        enablement_not_durable["nodes"][0]["prepare_barrier"][
            "alternative_enablement_sync_completed"
        ] = False
        for value in (
            selected_stopped,
            alternative_active,
            alternative_enabled,
            reverse_edge,
            alias_closure,
            relationship_unsealed,
            enablement_not_durable,
        ):
            raw = rf.canonical_json_bytes(value)
            with self.subTest(value=value), self.assertRaises(rf.FreezeValidationError):
                rf.validate_pinned_freeze_plan(raw, hashlib.sha256(raw).hexdigest())


class PrepareAndBarrierTests(unittest.TestCase):
    def test_prepare_receipt_is_exact_and_bound_to_plan_node_and_cgroups(self) -> None:
        plan, receipt, _arm = prepared_and_armed()
        rf.validate_prepare_receipt(receipt, plan=plan)
        self.assertEqual(receipt["node_contract_sha256"], plan.node("nyc").sha256)
        extra = dict(receipt)
        extra["unchecked"] = True
        with self.assertRaises(rf.FreezeValidationError):
            rf.validate_prepare_receipt(extra, plan=plan)
        absent_marker = dict(receipt)
        absent_marker["allow_marker_present"] = False
        with self.assertRaises(rf.FreezeValidationError):
            rf.validate_prepare_receipt(absent_marker, plan=plan)
        wrong_writer_inode = copy.deepcopy(receipt)
        wrong_writer_inode["cgroups"][1]["inode"] = 999
        with self.assertRaises(rf.FreezeValidationError):
            rf.validate_prepare_receipt(wrong_writer_inode, plan=plan)

    def test_barrier_state_requires_durable_marker_unlink(self) -> None:
        _plan, receipt, arm = prepared_and_armed()
        unarmed = rf.infer_barrier_state(
            prepare_receipt=receipt,
            arm_event=None,
            observed_boot_id=SEALED_BOOT,
            allow_marker_exists=True,
        )
        armed = rf.infer_barrier_state(
            prepare_receipt=receipt,
            arm_event=arm,
            observed_boot_id=SEALED_BOOT,
            allow_marker_exists=True,
        )
        self.assertIs(unarmed.state, rf.BarrierState.UNARMED)
        self.assertIs(armed.state, rf.BarrierState.ARMED)
        with self.assertRaisesRegex(rf.FreezeValidationError, "parent-fsync"):
            rf.infer_barrier_state(
                prepare_receipt=receipt,
                arm_event=arm,
                observed_boot_id=SEALED_BOOT,
                allow_marker_exists=False,
            )
        evidence = rf.DurableUnlinkEvidence(
            receipt["allow_marker_path"], True, True
        )
        committed = rf.infer_barrier_state(
            prepare_receipt=receipt,
            arm_event=arm,
            observed_boot_id=SEALED_BOOT,
            allow_marker_exists=False,
            durable_unlink=evidence,
        )
        self.assertIs(committed.state, rf.BarrierState.COMMITTED)
        self.assertEqual(committed.durability_basis, "unlink-and-parent-fsync")
        event = rf.make_barrier_commit_event(arm, committed, committed_at=STAMP)
        rf.validate_barrier_commit_event(event, arm_event=arm)

    def test_reboot_survival_can_prove_commit_but_not_an_unarmed_absence(self) -> None:
        _plan, receipt, arm = prepared_and_armed()
        committed = rf.infer_barrier_state(
            prepare_receipt=receipt,
            arm_event=arm,
            observed_boot_id=REBOOTED_BOOT,
            allow_marker_exists=False,
        )
        self.assertEqual(committed.durability_basis, "absence-survived-reboot")
        event = rf.make_barrier_commit_event(arm, committed, committed_at=STAMP)
        self.assertFalse(event["unlink_parent_fsynced"])
        with self.assertRaises(rf.FreezeValidationError):
            rf.infer_barrier_state(
                prepare_receipt=receipt,
                arm_event=None,
                observed_boot_id=REBOOTED_BOOT,
                allow_marker_exists=False,
            )


class ExactEventTests(unittest.TestCase):
    def setUp(self) -> None:
        self.plan = pinned_plan()
        self.cgroup = cgroups()[0]
        self.target = rf.TargetIdentity("supervisor", 1000, 10_000)

    def test_cgroup_freeze_and_thaw_events_have_exact_cross_checked_schemas(self) -> None:
        freeze = rf.make_cgroup_freeze_event(
            freeze_plan_sha256=self.plan.sha256,
            node="nyc",
            sealed_boot_id=SEALED_BOOT,
            cgroup=self.cgroup,
            phase="confirmed",
            occurred_at=STAMP,
        )
        thaw = rf.make_cgroup_thaw_event(
            freeze_plan_sha256=self.plan.sha256,
            node="nyc",
            sealed_boot_id=SEALED_BOOT,
            cgroup=self.cgroup,
            phase="confirmed",
            occurred_at=STAMP,
        )
        rf.validate_cgroup_freeze_event(freeze)
        rf.validate_cgroup_thaw_event(thaw)
        self.assertEqual(freeze["cgroup_freeze_value"], 1)
        self.assertEqual(thaw["cgroup_freeze_value"], 0)
        bad_freeze = dict(freeze)
        bad_freeze["observed_frozen"] = False
        bad_thaw = dict(thaw)
        bad_thaw["no_signal_replayed_after_own_stage_thaw_intent"] = False
        for event, validator in (
            (bad_freeze, rf.validate_cgroup_freeze_event),
            (bad_thaw, rf.validate_cgroup_thaw_event),
        ):
            with self.assertRaises(rf.FreezeValidationError):
                validator(event)

    def test_term_events_are_pidfd_sigterm_only_and_never_claim_exit_cause(self) -> None:
        intent = rf.make_pidfd_term_event(
            freeze_plan_sha256=self.plan.sha256,
            node="nyc",
            sealed_boot_id=SEALED_BOOT,
            target=self.target,
            phase="intent",
            occurred_at=STAMP,
        )
        sent = rf.make_pidfd_term_event(
            freeze_plan_sha256=self.plan.sha256,
            node="nyc",
            sealed_boot_id=SEALED_BOOT,
            target=self.target,
            phase="sent",
            occurred_at=STAMP,
        )
        self.assertEqual(intent["term_state"], "indeterminate")
        self.assertEqual(sent["term_state"], "confirmed")
        self.assertEqual(sent["exit_cause"], "unknown")
        for field, unsafe in (
            ("signal", "SIGKILL"),
            ("delivery", "kill(2)"),
            ("recovery_sigkill_sent", True),
            ("exit_cause", "SIGTERM"),
        ):
            altered = dict(sent)
            altered[field] = unsafe
            with self.subTest(field=field), self.assertRaises(rf.FreezeValidationError):
                rf.validate_pidfd_term_event(altered)

    def test_unknown_event_field_is_rejected(self) -> None:
        event = rf.make_cgroup_freeze_event(
            freeze_plan_sha256=self.plan.sha256,
            node="nyc",
            sealed_boot_id=SEALED_BOOT,
            cgroup=self.cgroup,
            phase="intent",
            occurred_at=STAMP,
        )
        event["future"] = "not-reviewed"
        with self.assertRaises(rf.FreezeValidationError):
            rf.validate_cgroup_freeze_event(event)


class OfflineReconciliationTests(unittest.TestCase):
    def setUp(self) -> None:
        _plan, self.receipt, self.arm = prepared_and_armed()
        self.barrier = rf.infer_barrier_state(
            prepare_receipt=self.receipt,
            arm_event=self.arm,
            observed_boot_id=REBOOTED_BOOT,
            allow_marker_exists=False,
        )
        self.targets = [
            {
                "role": "supervisor",
                "sealed_pid": 1000,
                "sealed_start_ticks": 10_000,
                "state": "absent",
                "stable_checks": 2,
            },
            {
                "role": "writer",
                "sealed_pid": 1000,
                "sealed_start_ticks": 10_000,
                "state": "absent",
                "stable_checks": 2,
            },
        ]
        self.cgroup_rows = [
            {**cgroups()[0].value(), "state": "absent", "stable_checks": 2},
            {**cgroups()[1].value(), "state": "absent", "stable_checks": 2},
        ]

    def make_event(self, **overrides: object) -> dict[str, object]:
        arguments = {
            "arm_event": self.arm,
            "barrier": self.barrier,
            "target_absence": self.targets,
            "cgroup_absence": self.cgroup_rows,
            "persistent_restart_fence_verified": True,
            "service_enablement_verified": True,
            "signals_sent": 0,
            "reconciled_at": STAMP,
        }
        arguments.update(overrides)
        return rf.make_zero_signal_offline_reconciliation(**arguments)

    def test_post_commit_reboot_absence_yields_zero_signal_unknown_cause(self) -> None:
        event = self.make_event()
        rf.validate_zero_signal_offline_reconciliation(event, arm_event=self.arm)
        self.assertEqual(event["signals_sent"], 0)
        self.assertEqual(event["supervisor_pidfd_sigterm_state"], "none")
        self.assertEqual(event["writer_pidfd_sigterm_state"], "none")
        self.assertEqual(event["exit_cause"], "unknown")

    def test_signal_fence_or_absence_drift_is_rejected(self) -> None:
        base = self.make_event()
        variants = []
        signaled = copy.deepcopy(base)
        signaled["signals_sent"] = 1
        variants.append(signaled)
        process_present = copy.deepcopy(base)
        process_present["target_absence"][1]["state"] = "present"
        variants.append(process_present)
        unstable = copy.deepcopy(base)
        unstable["cgroup_absence"][0]["stable_checks"] = 1
        variants.append(unstable)
        fence_missing = copy.deepcopy(base)
        fence_missing["persistent_restart_fence_verified"] = False
        variants.append(fence_missing)
        same_boot = copy.deepcopy(base)
        same_boot["observed_boot_id"] = SEALED_BOOT
        variants.append(same_boot)
        for event in variants:
            with self.subTest(event=event), self.assertRaises(rf.FreezeValidationError):
                rf.validate_zero_signal_offline_reconciliation(event, arm_event=self.arm)


class RecordingAdapter:
    def __init__(self) -> None:
        self.calls: list[tuple[object, ...]] = []

    def durable_unlink_allow_marker(self, path: str) -> rf.DurableUnlinkEvidence:
        self.calls.append(("unlink", path))
        return rf.DurableUnlinkEvidence(path, True, True)

    def set_cgroup_frozen(self, cgroup: rf.CgroupIdentity, frozen: bool) -> None:
        self.calls.append(("cgroup", cgroup, frozen))

    def send_pidfd_sigterm(self, target: rf.TargetIdentity) -> None:
        self.calls.append(("term", target))


class MutationBoundaryTests(unittest.TestCase):
    def test_default_adapter_refuses_every_host_mutation(self) -> None:
        mutations = rf.RecoveryMutations()
        group = cgroups()[0]
        target = rf.TargetIdentity("supervisor", 1000, 10_000)
        operations = (
            lambda: mutations.durable_unlink_allow_marker(
                rf.DEFAULT_ALLOW_MARKER_PATH
            ),
            lambda: mutations.freeze_cgroup(group),
            lambda: mutations.send_pidfd_sigterm(target),
            lambda: mutations.thaw_cgroup(group),
        )
        for operation in operations:
            with self.assertRaises(rf.MutationRefused):
                operation()

    def test_explicit_adapter_receives_only_validated_exact_operations(self) -> None:
        adapter = RecordingAdapter()
        mutations = rf.RecoveryMutations(adapter)
        group = cgroups()[0]
        target = rf.TargetIdentity("supervisor", 1000, 10_000)
        evidence = mutations.durable_unlink_allow_marker(
            rf.DEFAULT_ALLOW_MARKER_PATH
        )
        mutations.freeze_cgroup(group)
        mutations.send_pidfd_sigterm(target)
        mutations.thaw_cgroup(group)
        self.assertTrue(evidence.parent_directory_fsynced)
        self.assertEqual(
            adapter.calls,
            [
                ("unlink", rf.DEFAULT_ALLOW_MARKER_PATH),
                ("cgroup", group, True),
                ("term", target),
                ("cgroup", group, False),
            ],
        )

    def test_adapter_evidence_must_be_for_exact_marker_and_durable(self) -> None:
        class UnsafeAdapter(RecordingAdapter):
            def durable_unlink_allow_marker(self, path: str) -> rf.DurableUnlinkEvidence:
                return rf.DurableUnlinkEvidence("/wrong/path", True, False)

        with self.assertRaises(rf.FreezeValidationError):
            rf.RecoveryMutations(UnsafeAdapter()).durable_unlink_allow_marker(
                rf.DEFAULT_ALLOW_MARKER_PATH
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)
