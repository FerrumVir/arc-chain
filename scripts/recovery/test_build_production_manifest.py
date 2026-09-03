#!/usr/bin/env python3
"""Hermetic adversarial tests for the production manifest builder/finalizer."""

from __future__ import annotations

import argparse
import base64
import copy
import contextlib
import datetime as dt
import hashlib
import importlib.util
import json
import os
import pathlib
import stat
import struct
import sys
import tempfile
import types
import unittest
from unittest import mock


SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent


def load(name: str, path: pathlib.Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


builder = load("arc_build_production_manifest", SCRIPT_DIR / "build-production-manifest.py")
freeze_test = load("arc_freeze_test_fixture", SCRIPT_DIR / "test_recovery_freeze.py")


def canonical(value) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


def sha(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def write(path: pathlib.Path, payload: bytes, mode: int) -> None:
    if path.exists() and not path.is_symlink():
        path.chmod(0o600)
    path.write_bytes(payload)
    path.chmod(mode)


class Fixture:
    commit = "6" * 40
    run_id = 424242
    run_attempt = 3

    def live_source_capture(
        self,
        *,
        capture_id: str,
        round_number: int,
        target: dict,
        public_row: dict,
        cross_row: dict,
        public_sha256: str,
        cross_sha256: str,
        captured_at: str,
        index: int,
    ) -> dict:
        """Build the exact preauthorization source pair consumed by a round."""

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

        def regular(offset: int, root: str, size: int = 8) -> dict:
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
                "size": size,
            }

        public_latest = public_row["latest_block_height"]
        cross_latest = cross_row["loopback_latest_height"]
        head_height = max(
            public_latest,
            cross_latest,
            public_row["info_after_height"],
            cross_row["loopback_info_after_height"],
        )
        head = {
            "height": head_height,
            "block_hash": cross_row["loopback_latest_block_hash"],
            "state_root": f"{seed + 10:064x}",
        }
        wal_root = f"{seed + 11:064x}"
        snapshot_root = f"{seed + 12:064x}"
        genesis_root = sha(self.genesis.read_bytes())
        legacy_root = sha(self.legacy.read_bytes())
        rust_capture = {
            "schema": builder.quarantine_rounds.RUST_SOURCE_CAPTURE_SCHEMA,
            "captured_at_unix_ms": int(
                dt.datetime.strptime(captured_at, "%Y-%m-%dT%H:%M:%SZ")
                .replace(tzinfo=dt.timezone.utc)
                .timestamp()
                * 1000
            ),
            "head": head,
            "source_data_dir": directory(20),
            "source_wal_prefix": {
                "device": seed + 30,
                "inode": seed + 31,
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
            "source_snapshot": regular(40, snapshot_root),
            "genesis": regular(50, genesis_root),
            "legacy_validator_set": regular(60, legacy_root),
            "fixed_pair": {
                "data_dir": directory(70),
                "state_wal": regular(80, wal_root),
                "snapshot": regular(90, snapshot_root),
                "genesis_binding": regular(100, f"{seed + 13:064x}"),
                "strict_replay": True,
            },
            "allow_unbound_legacy_wal": False,
        }
        value = {
            "schema": builder.quarantine_rounds.LIVE_SOURCE_CAPTURE_SCHEMA,
            "capture_id": capture_id,
            "freeze_plan_sha256": self.freeze_sha,
            "source_main_commit": self.commit,
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
                "socket_inode": seed + 14,
            },
            "capture_attempt_id": f"00000000-0000-4000-8000-{index + 1:012d}",
            "capture_started_at": captured_at,
            "capture_completed_at": captured_at,
            "inspector_binary_sha256": sha(self.binary.read_bytes()),
            "genesis_sha256": genesis_root,
            "legacy_validator_set_sha256": legacy_root,
            "fixed_pair_path": (
                f"/root/arc-recovery-live-source-captures/{capture_id}/"
                f"{target['node']}/round-{round_number}/preauthorization-boundary/"
                f"attempt-{index + 1}/fixed-source"
            ),
            "snapshot_source": "sealed-writer-owned-loopback-/sync/snapshot",
            "existing_source_snapshot_used": False,
            "rust_capture": builder.quarantine_rounds.wrap(rust_capture),
            "head": head,
            "ancestry_checks": [
                {
                    "label": "public-latest",
                    "height": public_latest,
                    "expected_block_hash": public_row["latest_block_hash"],
                    "observed_block_hash": public_row["latest_block_hash"],
                    "state_root": f"{seed + 15:064x}",
                    "inspection_sha256": f"{seed + 16:064x}",
                },
                {
                    "label": "authenticated-loopback-latest",
                    "height": cross_latest,
                    "expected_block_hash": cross_row["loopback_latest_block_hash"],
                    "observed_block_hash": cross_row["loopback_latest_block_hash"],
                    "state_root": f"{seed + 17:064x}",
                    "inspection_sha256": f"{seed + 18:064x}",
                },
            ],
            "content_sealed": True,
            "strict_offline_replay": True,
            "source_pair_role": "preauthorization-boundary",
            "minimum_height": max(
                public_row["info_after_height"],
                cross_row["loopback_info_after_height"],
            ),
            "expected_head": None,
            "boundary_proof_sha256": cross_sha256,
            "network_quarantine_receipt_sha256": None,
            "owned_ruleset_stateless_sha256": None,
        }
        return builder.quarantine_rounds.wrap(value)

    def __init__(self, root: pathlib.Path) -> None:
        self.root = root
        self.archive_counter = 0
        self.stage_counter = 0
        self.last_stage_root: pathlib.Path | None = None
        self.proof_now = int(dt.datetime.now(dt.timezone.utc).timestamp())
        self.output = root / "prearchive.json"
        self.summary = self.checkpoint_summary()
        self.summary_path = root / "checkpoint-summary.json"
        write(self.summary_path, canonical(self.summary), 0o444)
        self.binary = root / "arc-node-linux-x86_64"
        fake = f"""#!{sys.executable}
import json,pathlib,sys
root=pathlib.Path({str(root)!r})
value=json.loads((root/'checkpoint-summary.json').read_text())
argv=sys.argv[1:]
action=argv[1] if len(argv)>1 and argv[0]=='recovery' else ''
if action=='export':
    output=pathlib.Path(argv[argv.index('--output')+1])
    output.write_bytes(b'reproduced-checkpoint')
    value['status']='EXPORTED_UNSIGNED'; value['signature_count']=0
elif action=='verify':
    if (root/'verify-fail').exists():
        print('forced verify failure',file=sys.stderr); raise SystemExit(9)
    value['status']='VERIFIED_QUORUM'; value['signature_count']=5
elif action=='inspect':
    value['status']='UNTRUSTED_INSPECTION'; value['signature_count']=5
else:
    print('unsupported fake command',file=sys.stderr); raise SystemExit(8)
print(json.dumps(value,sort_keys=True))
""".encode()
        write(self.binary, fake, 0o555)
        self.cli = root / "arc-cli-linux-x86_64"
        write(self.cli, b"#!/bin/sh\nexit 0\n", 0o555)
        self.genesis = root / "genesis.toml"
        validators = [
            {"address": f"{index + 101:064x}", "stake": 6_666_667 if index < 4 else 6_666_666}
            for index in range(6)
        ]
        genesis_lines = [
            "[chain]",
            'name = "arc-testnet"',
            'chain_id = "0x415243"',
            "validator_set_complete = true",
            "community_rewards_v1_activation_height = 137146",
            "",
        ]
        for row in validators:
            genesis_lines += [
                "[[accounts]]",
                f'address = "{row["address"]}"',
                "balance = 0",
                "",
            ]
        for row in validators:
            genesis_lines += [
                "[[validators]]",
                f'address = "{row["address"]}"',
                f'stake = {row["stake"]}',
                "",
            ]
        write(self.genesis, "\n".join(genesis_lines).encode(), 0o444)
        self.build_metadata = root / "BUILD-METADATA.json"
        metadata = {
            "schema": "arc.pretag.artifact.v1",
            "kind": "headless",
            "repository": builder.REPOSITORY,
            "commit": self.commit,
            "platform": builder.PRETAG_PLATFORM,
            "rust_target": builder.PRETAG_TARGET,
            "version": builder.VERSION,
            "workflow_run_id": self.run_id,
            "workflow_run_attempt": self.run_attempt,
            "files": {
                "arc-node-linux-x86_64": sha(self.binary.read_bytes()),
                "arc-cli-linux-x86_64": sha(self.cli.read_bytes()),
                "genesis.toml": sha(self.genesis.read_bytes()),
            },
        }
        write(self.build_metadata, canonical(metadata), 0o444)
        self.raw_actions_zips: dict[tuple[str, str], pathlib.Path] = {}
        self.artifact_ids: dict[tuple[str, str], int] = {}
        input_rows = []
        for index, (kind, platform) in enumerate(builder.PRETAG_GROUPS):
            raw_path = root / f"{kind}-{platform}.actions.zip"
            write(raw_path, f"test-only-{kind}-{platform}-raw-actions-zip\n".encode(), 0o400)
            artifact_id = 515151 + index
            self.raw_actions_zips[(kind, platform)] = raw_path
            self.artifact_ids[(kind, platform)] = artifact_id
            input_rows.append(
                {
                    "kind": kind,
                    "platform": platform,
                    "artifact_id": artifact_id,
                    "raw_actions_zip": str(raw_path),
                }
            )
        self.pretag_artifact_input_set = root / "PRETAG-ARTIFACT-INPUT-SET.json"
        write(
            self.pretag_artifact_input_set,
            canonical(
                {
                    "schema": builder.PRETAG_INPUT_SET_SCHEMA,
                    "repository": builder.REPOSITORY,
                    "commit": self.commit,
                    "run_id": self.run_id,
                    "run_attempt": self.run_attempt,
                    "artifacts": input_rows,
                }
            ),
            0o400,
        )
        self.curl = root / "curl"
        write(self.curl, b"#!/bin/sh\nexit 99\n", 0o555)
        self.ca_bundle = root / "ca.pem"
        write(self.ca_bundle, b"test-only-ca\n", 0o444)
        self.public_keys = root / "validator-public-keys.json"
        public = [
            {"address": row["address"], "public_key": f"{index + 201:064x}", "stake": row["stake"]}
            for index, row in enumerate(validators)
        ]
        write(self.public_keys, canonical(public), 0o444)
        self.legacy = root / "legacy-validator-set-40m.json"
        legacy = [{"address": f"{index + 1:064x}", "stake": 5_000_000} for index in range(8)]
        write(self.legacy, canonical(legacy), 0o444)
        self.checkpoint = root / "signed.arcchkpt"
        write(self.checkpoint, b"signed-checkpoint", 0o444)
        self.reference = root / "reference"
        self.reference.mkdir(mode=0o700)
        self.snapshot = self.reference / "state.snapshot.lz4"
        self.wal = self.reference / "state.wal"
        write(self.snapshot, b"snapshot", 0o444)
        write(self.wal, b"wal", 0o444)
        self.caddy = root / "caddy"
        write(self.caddy, b"reviewed-caddy", 0o555)
        self.reward_probe = SCRIPT_DIR / "community-reward-probe.py"
        self.freeze = root / "freeze.json"
        self.freeze_sha = self.write_freeze()
        self.height = root / "legacy-height.json"
        self.write_height()
        self.bundle = root / "legacy-maintenance-evidence-bundle.json"
        self.boundary = root / "legacy-maintenance-boundary.json"
        self.offline_stop = root / "offline-stop-evidence.json"
        self.write_offline_stop()
        self.late_fork_source_set = root / "legacy-late-fork-source-set.json"
        self.rebuild_late_fork_source_set()
        self.known_hosts = root / "known_hosts"
        known_lines = []
        for index, (_name, host) in enumerate(builder.FLEET):
            blob = (
                struct.pack(">I", 11)
                + b"ssh-ed25519"
                + struct.pack(">I", 32)
                + bytes([index + 1]) * 32
            )
            known_lines.append(f"{host} ssh-ed25519 {base64.b64encode(blob).decode()}\n")
        write(self.known_hosts, "".join(known_lines).encode("ascii"), 0o400)
        self.ssh_identity = root / "id_ed25519"
        write(self.ssh_identity, b"test-only-private-identity\n", 0o400)
        self.validator_vault_restore_receipt = root / "RESTORE-RECEIPT.json"
        self.validator_key_install_receipt = root / "INSTALL-RECEIPT.json"
        self.write_validator_receipts()

    @staticmethod
    def checkpoint_summary() -> dict:
        return {
            "status": "UNTRUSTED_INSPECTION",
            "manifest_hash": "0x" + "4" * 64,
            "payload_hash": "0x" + "7" * 64,
            "full_state_root": "0x" + "3" * 64,
            "chain_id": "0x415243",
            "genesis_hash": "0x" + "0" * 64,
            "source_height": 137145,
            "source_block_hash": "0x" + "1" * 64,
            "source_state_root": "0x" + "5" * 64,
            "source_consensus_round": 9_774_808,
            "created_at_unix_ms": 1_787_857_623_000,
            "transition_height": 137146,
            "transition_block_hash": "0x" + "2" * 64,
            "recovery_domain": "0x" + "6" * 64,
            "recovery_epoch": 1,
            "validator_set_id": 1,
            "protocol_version": "3.0.0",
            "validator_count": 6,
            "signature_count": 5,
            "source_validator_count": 8,
            "source_validator_stake": 40_000_000,
            "source_validator_set_hash": "0x" + "8" * 64,
            "community_reward_issuance_policy_hash": "0x" + "9" * 64,
        }

    def write_freeze(self, mutate=None) -> str:
        value = freeze_test.plan_value()
        value["source_commit"] = self.commit
        value["legacy_validator_set_sha256"] = sha(self.legacy.read_bytes())
        value["drive_prefreeze"]["remote_root"] = "arc-drive-arc:ARC Chain Recovery v0.8"
        value["drive_prefreeze"]["remote_root_sha256"] = sha(
            value["drive_prefreeze"]["remote_root"].encode()
        )
        provenance = {
            "orchestrator_sha256": SCRIPT_DIR / "archive-fleet-to-drive.sh",
            "remote_helper_sha256": SCRIPT_DIR / "archive-node.sh",
            "rollout_tool_sha256": SCRIPT_DIR / "recovery_rollout.py",
            "rollout_schema_sha256": SCRIPT_DIR / "recovery-manifest.schema.json",
        }
        for field, path in provenance.items():
            value[field] = sha(path.read_bytes())
        for index, ((name, host), row) in enumerate(zip(builder.FLEET, value["nodes"])):
            row["name"] = name
            row["host"] = host
            row["model_sha256"] = builder.rollout.CANONICAL_MODEL_SHA256
            row["model_size_bytes"] = builder.rollout.CANONICAL_MODEL_SIZE_BYTES
            row["shard_ranges"] = copy.deepcopy(builder.SHARDS[name])
            row["validator_address"] = f"{index + 1:064x}"
        if mutate is not None:
            mutate(value)
        payload = canonical(value)
        digest = sha(payload)
        self.freeze.chmod(0o600) if self.freeze.exists() else None
        write(self.freeze, payload, 0o400)
        sidecar = self.freeze.with_name(self.freeze.name + ".sha256")
        if sidecar.exists():
            sidecar.chmod(0o600)
        write(sidecar, f"{digest}  {self.freeze.name}\n".encode(), 0o400)
        return digest

    def write_height(
        self,
        *,
        stale: bool = False,
        completed_at: dt.datetime | None = None,
    ) -> None:
        if stale and completed_at is not None:
            raise ValueError("stale and completed_at are mutually exclusive fixture controls")
        now = (completed_at or dt.datetime.now(dt.timezone.utc)).replace(microsecond=0)
        if stale:
            now -= dt.timedelta(seconds=301)
        timestamp = now.isoformat().replace("+00:00", "Z")
        rows = []
        for index, (name, _host, origin) in enumerate(builder.legacy_height.FLEET):
            height = 137500 + index
            rows.append(
                {
                    "name": name,
                    "origin": origin,
                    "info_before_height": height,
                    "latest_block_height": height,
                    "info_after_height": height,
                    "latest_block_hash": f"{index + 1:064x}",
                    "info_before_body_sha256": "a" * 64,
                    "latest_block_body_sha256": "b" * 64,
                    "info_after_body_sha256": "c" * 64,
                }
            )
        receipt = {
            "schema": builder.legacy_height.SCHEMA,
            "source_main_commit": self.commit,
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": builder.legacy_height.capture_id(self.freeze_sha),
            "started_at": timestamp,
            "completed_at": timestamp,
            "duration_ms": 1,
            "request_policy": {
                "redirects": "forbidden",
                "maximum_body_bytes": builder.legacy_height.MAX_BODY_BYTES,
                "timeout_seconds": 10.0,
                "proxy_environment": "ignored",
                "sequence": ["/info", "/block/latest", "/info"],
            },
            "origins": rows,
            "legacy_public_max_height": max(row["info_after_height"] for row in rows),
        }
        if self.height.exists():
            self.height.chmod(0o600)
        write(self.height, canonical(receipt), 0o400)

    def write_offline_stop(
        self,
        *,
        fleet_started_at: dt.datetime | None = None,
        fleet_completed_at: dt.datetime | None = None,
        first_quarantine_at: dt.datetime | None = None,
        all_stopped_at: dt.datetime | None = None,
        boundary_created_at: dt.datetime | None = None,
    ) -> None:
        freeze = json.loads(self.freeze.read_text())
        capture = builder.rollout.capture_id_for_freeze_plan_hash(self.freeze_sha)
        public_receipt = json.loads(self.height.read_text())
        public_sha = sha(self.height.read_bytes())
        now = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
        fleet_started_value = (fleet_started_at or now).replace(microsecond=0)
        fleet_completed_value = (fleet_completed_at or fleet_started_value).replace(
            microsecond=0
        )
        first_quarantine_value = (first_quarantine_at or fleet_completed_value).replace(
            microsecond=0
        )
        all_stopped_value = (all_stopped_at or (
            first_quarantine_value + dt.timedelta(seconds=1)
        )).replace(microsecond=0)
        boundary_created_value = (boundary_created_at or (
            all_stopped_value + dt.timedelta(seconds=1)
        )).replace(microsecond=0)
        fleet_started = fleet_started_value.isoformat().replace("+00:00", "Z")
        fleet_completed = fleet_completed_value.isoformat().replace("+00:00", "Z")
        first_quarantine = first_quarantine_value.isoformat().replace("+00:00", "Z")
        all_stopped = all_stopped_value.isoformat().replace("+00:00", "Z")
        boundary_created = boundary_created_value.isoformat().replace("+00:00", "Z")
        challenge = "d" * 64
        observation_generation = "6" * 64
        operator_public_completed = dt.datetime.strptime(
            public_receipt["completed_at"], "%Y-%m-%dT%H:%M:%SZ"
        ).replace(tzinfo=dt.timezone.utc)
        observation_selected_at = operator_public_completed.strftime(
            "%Y-%m-%dT%H:%M:%S.000000Z"
        )
        generation_created_at = (
            operator_public_completed - dt.timedelta(seconds=10)
        ).strftime("%Y-%m-%dT%H:%M:%S.000000Z")
        drive_receipt_value = {
            "schema": "arc.recovery.drive-prefreeze.v1",
            "mode": "execute", "freeze_plan_sha256": self.freeze_sha,
            "capture_id": capture, "remote_root_sha256": "1" * 64,
            "client_id_sha256": "2" * 64, "account_sha256": "3" * 64,
            "permission_id_sha256": "4" * 64, "rclone_version": "v1.75.0",
            "source_bytes": 1, "archive_reservation_bytes": 2,
            "largest_object_reservation_bytes": 1,
            "daily_upload_budget_bytes": 2,
            "daily_upload_budget_basis": "operator-reviewed-remaining-dedicated-account",
            "available_bytes_before": 8 * 1024 * 1024 + 2,
            "available_bytes_after": 2, "canary_bytes": 8 * 1024 * 1024,
            "canary_verified": True, "canary_deleted": True,
        }
        drive_receipt_sha = sha(canonical(drive_receipt_value))
        observation_generation_receipt = {
            "schema": "arc.recovery.legacy-live-observation-generation.v1",
            "source_main_commit": self.commit,
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": capture,
            "observation_generation": observation_generation,
            "created_at": generation_created_at,
            "max_selection_age_seconds": 300,
            "drive_prefreeze_receipt": {
                "path": "/private/drive-prefreeze.json",
                "sha256": drive_receipt_sha,
                "value": drive_receipt_value,
            },
        }
        observation_generation_receipt_sha = sha(
            canonical(observation_generation_receipt)
        )
        observation_selection = {
            "schema": "arc.recovery.legacy-live-observation-selection.v1",
            "source_main_commit": self.commit,
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": capture,
            "observation_generation": observation_generation,
            "observation_generation_receipt": observation_generation_receipt,
            "observation_generation_receipt_path": (
                f"/private/live-observation-generations/{observation_generation}.json"
            ),
            "observation_generation_receipt_sha256": (
                observation_generation_receipt_sha
            ),
            "drive_prefreeze_receipt_path": "/private/drive-prefreeze.json",
            "drive_prefreeze_receipt_sha256": drive_receipt_sha,
            "generation_created_at": generation_created_at,
            "selected_at": observation_selected_at,
            "max_selection_age_seconds": 300,
            "labels": ["diagnostic", "noncanonical", "nonreward"],
            "nodes": [
                {
                    "node": name,
                    "created_at": generation_created_at,
                    "completed_at": observation_selected_at,
                    "root_sha256": f"{index + 7000:064x}",
                    "receipt_sha256": f"{index + 7100:064x}",
                }
                for index, (name, _host) in enumerate(builder.FLEET)
            ],
        }
        observation_selection_sha = sha(canonical(observation_selection))
        status_fields = (
            "validator_address", "stake", "writer_pid", "writer_start_ticks", "boot_id",
            "writer_cgroup_sha256", "writer_supervision_mode", "supervisor_unit",
            "supervisor_main_pid", "supervisor_start_ticks", "supervisor_executable_path",
            "supervisor_executable_sha256", "supervisor_argv_sha256",
            "supervisor_context_sha256", "executable_path", "executable_sha256",
            "argv_sha256", "data_dir",
        )
        target_public_early = {
            **copy.deepcopy(public_receipt),
            "schema": builder.quarantine_rounds.TARGET_HEIGHT_SCHEMA,
            "targets": [
                {
                    "node": name, "host": host,
                    "rpc_origin": next(
                        row["origin"] for row in public_receipt["origins"]
                        if row["name"] == name
                    ),
                }
                for name, host in builder.FLEET
            ],
        }
        target_cross_nodes_early = []
        for index, ((name, host), frozen, public) in enumerate(
            zip(builder.FLEET, freeze["nodes"], public_receipt["origins"])
        ):
            loop_height = public["info_after_height"] + 1
            target_cross_nodes_early.append({
                "node": name, "host": host,
                "writer_pid": frozen["writer_pid"],
                "writer_start_ticks": frozen["writer_start_ticks"],
                "boot_id": frozen["boot_id"],
                "writer_cgroup_sha256": frozen["writer_cgroup_sha256"],
                "public_info_after_height": public["info_after_height"],
                "public_latest_block_height": public["latest_block_height"],
                "public_latest_block_hash": public["latest_block_hash"],
                "loopback_info_before_height": loop_height,
                "loopback_latest_height": loop_height,
                "loopback_info_after_height": loop_height,
                "loopback_latest_block_hash": public["latest_block_hash"],
                "response_sha256": {
                    "/info:before": f"{index + 130:064x}",
                    "/block/latest": f"{index + 140:064x}",
                    "/info:after": f"{index + 150:064x}",
                },
            })
        target_cross_early = {
            "schema": builder.quarantine_rounds.TARGET_CROSS_SCHEMA,
            "source_main_commit": self.commit,
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": capture,
            "legacy_public_height_receipt_sha256": sha(canonical(target_public_early)),
            "challenge": challenge,
            "started_at": fleet_started,
            "completed_at": fleet_completed,
            "conservative_height_floor": min(
                row["loopback_info_before_height"] for row in target_cross_nodes_early
            ),
            "targets": copy.deepcopy(target_public_early["targets"]),
            "nodes": target_cross_nodes_early,
        }
        public_completed_early = dt.datetime.strptime(
            public_receipt["completed_at"], "%Y-%m-%dT%H:%M:%SZ"
        ).replace(tzinfo=dt.timezone.utc)
        generation_authorization_early = {
            "schema": builder.quarantine_rounds.ROUND_AUTH_SCHEMA,
            "source_main_commit": self.commit,
            "capture_id": capture, "freeze_plan_sha256": self.freeze_sha,
            "live_observation_selection_sha256": observation_selection_sha,
            "live_observation_generation": observation_generation,
            "observation_generation_receipt_sha256": (
                observation_generation_receipt_sha
            ),
            "drive_prefreeze_receipt_sha256": drive_receipt_sha,
            "live_observation_selected_at": observation_selected_at,
            "round_number": 1, "prior_round_result_sha256s": [], "prior_fenced": [],
            "targets": [{
                "node": name, "host": host, "boot_id": frozen["boot_id"],
                "writer_pid": frozen["writer_pid"],
                "writer_start_ticks": frozen["writer_start_ticks"],
                "writer_cgroup_sha256": frozen["writer_cgroup_sha256"],
            } for (name, host), frozen in zip(builder.FLEET, freeze["nodes"])],
            "public_height_receipt": builder.quarantine_rounds.wrap(target_public_early),
            "authenticated_height_cross_proof": builder.quarantine_rounds.wrap(
                target_cross_early
            ),
            "authorized_at": fleet_completed,
            "authorization_deadline": (
                public_completed_early + dt.timedelta(seconds=300)
            ).strftime("%Y-%m-%dT%H:%M:%SZ"),
        }
        generation_authorization_early["live_source_captures"] = [
            self.live_source_capture(
                capture_id=capture,
                round_number=1,
                target=target,
                public_row=next(
                    row for row in target_public_early["origins"]
                    if row["name"] == target["node"]
                ),
                cross_row=next(
                    row for row in target_cross_early["nodes"]
                    if row["node"] == target["node"]
                ),
                public_sha256=builder.quarantine_rounds.digest(target_public_early),
                cross_sha256=builder.quarantine_rounds.digest(target_cross_early),
                captured_at=fleet_completed,
                index=index,
            )
            for index, target in enumerate(generation_authorization_early["targets"])
        ]
        generation_authorization_sha_early = builder.quarantine_rounds.digest(
            generation_authorization_early
        )
        generation_readiness_early = {
            "schema": builder.quarantine_rounds.READINESS_SCHEMA,
            "capture_id": capture, "freeze_plan_sha256": self.freeze_sha,
            "round_number": 1,
            "round_authorization_sha256": generation_authorization_sha_early,
            "targets": [{
                "node": name, "host": host,
                "authorization_acceptance": builder.quarantine_rounds.wrap({
                    "schema": builder.quarantine_rounds.AUTH_ACCEPTANCE_SCHEMA,
                    "capture_id": capture, "freeze_plan_sha256": self.freeze_sha,
                    "round_number": 1,
                    "round_authorization_sha256": generation_authorization_sha_early,
                    "node": name, "host": host, "accepted_at": fleet_completed,
                    "accepted_monotonic_ns": 1_000_000_000,
                    "accepted_boot_id": next(
                        row["boot_id"] for row in generation_authorization_early["targets"]
                        if row["node"] == name
                    ),
                    "authorization_deadline": generation_authorization_early[
                        "authorization_deadline"
                    ],
                }),
            } for name, host in builder.FLEET],
            "completed_at": fleet_completed,
            "authorization_deadline": generation_authorization_early[
                "authorization_deadline"
            ],
            "max_elapsed_since_acceptance_ns": 300_000_000_000,
        }
        generation_readiness_sha_early = builder.quarantine_rounds.digest(
            generation_readiness_early
        )
        nodes = []
        cross_nodes = []
        boundary_nodes = []
        evidence_node_values = []
        stability_nodes = []
        stability_heads = []
        evidence_heights = []
        labels = (
            "public_info_before", "public_latest", "public_info_after",
            "authenticated_info_before", "authenticated_latest",
            "authenticated_info_after", "authenticated_conservative_floor",
            "initial_post_quarantine_head", "public_cross_info_after",
            "post_quarantine_head", "quarantine_stability_sample_0",
            "quarantine_stability_sample_1", "final_persisted_head",
        )
        for index, ((name, host), frozen, public) in enumerate(
            zip(builder.FLEET, freeze["nodes"], public_receipt["origins"])
        ):
            complete_sha = f"{index + 20:064x}"
            files_sha = f"{index + 40:064x}"
            status = {
                "capture_id": capture,
                "freeze_plan_sha256": self.freeze_sha,
                "node": name,
                "restart_fenced": True,
                "schema": "arc.recovery.offline-stop-status.v1",
                "stake": frozen["stake"],
                "stop_complete_sha256": complete_sha,
                "stop_files_sha256": files_sha,
                "stop_schema": "arc.recovery.offline-stop.v4",
                "stopped": True,
                "validator_address": frozen["validator_address"],
            }
            argv = ["stopped-status", capture, name, self.freeze_sha]
            argv.extend(str(frozen[field]) for field in status_fields)
            nodes.append({
                "host": host,
                "node": name,
                "stake": frozen["stake"],
                "stop_complete_sha256": complete_sha,
                "stop_files_sha256": files_sha,
                "stopped_status_argv_sha256": sha(canonical(argv)),
                "stopped_status_sha256": sha(canonical(status)),
                "validator_address": frozen["validator_address"],
            })
            authenticated_before = public["info_before_height"]
            authenticated_latest = public["latest_block_height"] + 1
            authenticated_after = public["info_after_height"] + 1
            proof = {
                "schema": "arc.recovery.authenticated-legacy-height-bracket.v1",
                "capture_id": capture,
                "node": name,
                "freeze_plan_sha256": self.freeze_sha,
                "challenge": challenge,
                "rpc_origin": frozen["rpc_origin"],
                "writer_pid": frozen["writer_pid"],
                "writer_start_ticks": frozen["writer_start_ticks"],
                "boot_id": frozen["boot_id"],
                "executable_sha256": frozen["executable_sha256"],
                "argv_sha256": frozen["argv_sha256"],
                "started_at": fleet_started,
                "completed_at": fleet_completed,
                "public_info_before_height": public["info_before_height"],
                "public_latest_block_height": public["latest_block_height"],
                "public_info_after_height": public["info_after_height"],
                "public_latest_block_hash": public["latest_block_hash"],
                "authenticated_info_before_height": authenticated_before,
                "authenticated_latest_block_height": authenticated_latest,
                "authenticated_info_after_height": authenticated_after,
                "authenticated_latest_block_hash": public["latest_block_hash"],
                "authenticated_info_before_body_sha256": f"{index + 130:064x}",
                "authenticated_latest_block_body_sha256": f"{index + 140:064x}",
                "authenticated_info_after_body_sha256": f"{index + 150:064x}",
                "conservative_height_floor": authenticated_after,
            }
            proof_sha = sha(canonical(proof))
            cross_nodes.append(
                {"node": name, "host": host, "proof": proof, "proof_sha256": proof_sha}
            )
            public_tuple = {
                "height": public["info_after_height"],
                "block_hash": public["latest_block_hash"],
                "state_root": f"{index + 160:064x}",
            }
            initial_tuple = {
                "height": authenticated_after,
                "block_hash": public["latest_block_hash"],
                "state_root": f"{index + 180:064x}",
            }
            later_tuple = copy.deepcopy(initial_tuple)
            persisted_tuple = {
                "height": authenticated_after + 1,
                "block_hash": f"{index + 190:064x}",
                "state_root": f"{index + 200:064x}",
            }
            round_policy_source_sha = f"{index + 204:064x}"
            round_binding = {
                "schema": builder.quarantine_rounds.TABLE_BINDING_SCHEMA,
                "capture_id": capture, "freeze_plan_sha256": self.freeze_sha,
                "round_number": 1,
                "round_authorization_sha256": generation_authorization_sha_early,
                "round_readiness_sha256": generation_readiness_sha_early,
                "authorization_deadline": generation_authorization_early[
                    "authorization_deadline"
                ],
                "apply_helper_sha256": freeze["remote_helper_sha256"],
                "policy_sha256": round_policy_source_sha,
                "node": name, "host": host,
                "writer": {
                    "boot_id": frozen["boot_id"], "pid": frozen["writer_pid"],
                    "start_ticks": frozen["writer_start_ticks"],
                    "cgroup_sha256": frozen["writer_cgroup_sha256"],
                },
            }
            round_binding_sha = builder.quarantine_rounds.digest(round_binding)
            round_gate = {
                "schema": builder.quarantine_rounds.NFT_GATE_SCHEMA,
                "capture_id": capture, "freeze_plan_sha256": self.freeze_sha,
                "round_authorization_sha256": generation_authorization_sha_early,
                "round_readiness_sha256": generation_readiness_sha_early,
                "round_number": 1, "node": name, "host": host,
                "authorization_deadline": generation_authorization_early[
                    "authorization_deadline"
                ],
                "invoked_at": first_quarantine,
                "apply_helper_sha256": freeze["remote_helper_sha256"],
                "policy_sha256": round_policy_source_sha,
                "table_binding_sha256": round_binding_sha,
                "table_comment": (
                    f"arc-recovery:round=1:bind={round_binding_sha}:node={name}"
                ),
            }
            round_ancestry = {
                "schema": builder.quarantine_rounds.ANCESTRY_SCHEMA,
                "capture_id": capture, "freeze_plan_sha256": self.freeze_sha,
                "round_authorization_sha256": generation_authorization_sha_early,
                "round_number": 1, "node": name, "host": host,
                "checks": [
                    {
                        "label": "public-latest", "height": public["latest_block_height"],
                        "expected_block_hash": public["latest_block_hash"],
                        "observed_block_hash": public["latest_block_hash"],
                        "response_sha256": f"{int(name.encode().hex(), 16) % (1 << 256):064x}",
                    },
                    {
                        "label": "authenticated-loopback-latest",
                        "height": authenticated_after,
                        "expected_block_hash": proof["authenticated_latest_block_hash"],
                        "observed_block_hash": proof["authenticated_latest_block_hash"],
                        "response_sha256": proof["authenticated_latest_block_body_sha256"],
                    },
                ],
            }
            round_intent = {
                "schema": builder.quarantine_rounds.NFT_INTENT_SCHEMA,
                "capture_id": capture, "freeze_plan_sha256": self.freeze_sha,
                "source_main_commit": self.commit, "round_number": 1,
                "round_authorization_sha256": generation_authorization_sha_early,
                "round_readiness_sha256": generation_readiness_sha_early,
                "authorization_deadline": generation_authorization_early[
                    "authorization_deadline"
                ],
                "node": name, "host": host, "writer": round_binding["writer"],
                "table_binding_sha256": round_binding_sha,
                "table_comment": round_gate["table_comment"],
                "apply_helper_sha256": freeze["remote_helper_sha256"],
                "nft_policy_source_sha256": round_policy_source_sha,
                "prepared_at": fleet_completed,
            }
            round_commit = {
                "schema": "arc.recovery.quarantine-nft-applied-commit.v1",
                "capture_id": capture, "freeze_plan_sha256": self.freeze_sha,
                "round_number": 1,
                "round_authorization_sha256": generation_authorization_sha_early,
                "round_readiness_sha256": generation_readiness_sha_early,
                "node": name, "host": host,
                "nft_deadline_gate_sha256": builder.quarantine_rounds.digest(round_gate),
                "table_binding_sha256": round_binding_sha,
                "table_comment": round_gate["table_comment"],
                "apply_helper_sha256": freeze["remote_helper_sha256"],
                "nft_policy_source_sha256": round_policy_source_sha,
                "owned_ruleset_stateless_sha256": f"{index + 211:064x}",
                "nft_applied_at": first_quarantine,
            }
            round_restart_sha = f"{index + 213:064x}"
            network_quarantine_receipt = {
                "schema": "arc.recovery.legacy-network-quarantine.v1",
                "capture_id": capture, "node": name, "host": host,
                "freeze_plan_sha256": self.freeze_sha,
                "source_main_commit": self.commit,
                "round_number": 1,
                "round_authorization_sha256": generation_authorization_sha_early,
                "round_readiness_sha256": generation_readiness_sha_early,
                "nft_deadline_gate_sha256": builder.quarantine_rounds.digest(round_gate),
                "nft_apply_intent_sha256": builder.quarantine_rounds.digest(round_intent),
                "nft_apply_intent": builder.quarantine_rounds.wrap(round_intent),
                "nft_table_binding_sha256": round_binding_sha,
                "nft_table_binding": round_binding,
                "table_comment": round_gate["table_comment"],
                "nft_table_comment": round_gate["table_comment"],
                "nft_policy_source_sha256": round_policy_source_sha,
                "apply_helper_sha256": freeze["remote_helper_sha256"],
                "applied_commit_sha256": builder.quarantine_rounds.digest(round_commit),
                "authorization_ancestry_proof_sha256": builder.quarantine_rounds.digest(
                    round_ancestry
                ),
                "boot_id": frozen["boot_id"],
                "writer": {
                    "pid": frozen["writer_pid"],
                    "start_ticks": frozen["writer_start_ticks"],
                    "cgroup_sha256": frozen["writer_cgroup_sha256"],
                },
                "table": {
                    "family": "inet", "name": "arc_legacy_maintenance_v1",
                    "priority": -310,
                    "hooks": ["prerouting", "input", "forward", "output"],
                    "policy": "accept", "comment": round_gate["table_comment"],
                    "loopback_retained": True,
                },
                "quarantine_policy": {
                    "mode": "deny-all-nonloopback-except-host-maintenance",
                    "families": ["ipv4", "ipv6"],
                    "directions": ["input", "output", "forward"],
                    "priority_before_conntrack": True,
                    "established_bypass": False,
                    "allowed": [
                        "loopback", "ssh-tcp-22", "dhcpv4-67-68",
                        "dhcpv6-546-547", "icmpv6-ndp-ra-packet-too-big",
                    ],
                    "legacy_rpc_p2p_web_dynamic_all_blocked": True,
                },
                "persistence": {
                    "unit_path": "/etc/systemd/system/arc-legacy-maintenance-fence.service",
                    "unit_enabled": True, "unit_active": True,
                    "state_path": "/etc/arc-recovery/network-fence-rounds/fixture",
                    "active_selector_path": "/run/arc-recovery/active-network-fence",
                    "automatic_unfence": False,
                },
                "tool_sha256": {"/usr/sbin/nft": f"{index + 203:064x}"},
                "file_sha256": {
                    "authorization.json": generation_authorization_sha_early,
                    "readiness.json": generation_readiness_sha_early,
                    "contract.json": f"{index + 214:064x}",
                    "table-binding.json": round_binding_sha,
                    "nft-apply-intent.json": builder.quarantine_rounds.digest(
                        round_intent
                    ),
                    "policy.nft": round_policy_source_sha,
                    "apply": freeze["remote_helper_sha256"],
                    "nft": f"{index + 203:064x}",
                    "nft-deadline-gate.json": builder.quarantine_rounds.digest(round_gate),
                    "applied.commit.json": builder.quarantine_rounds.digest(round_commit),
                    "persistent-restart-fence.json": round_restart_sha,
                    "rendered-policy.nft": f"{index + 215:064x}",
                    "/usr/local/libexec/arc-legacy-maintenance-fence": f"{index + 216:064x}",
                    "/etc/systemd/system/arc-legacy-maintenance-fence.service": f"{index + 217:064x}",
                    "/etc/systemd/system/arc-self-heal.service.d/zzzy-arc-recovery-network-fence.conf": f"{index + 218:064x}",
                    "/etc/systemd/system/arc-node.service.d/zzzy-arc-recovery-network-fence.conf": f"{index + 219:064x}",
                    "/etc/systemd/system/arc-node-update.service.d/zzzy-arc-recovery-network-fence.conf": f"{index + 220:064x}",
                    "/etc/systemd/system/arc-node-update.timer.d/zzzy-arc-recovery-network-fence.conf": f"{index + 221:064x}",
                },
                "owned_ruleset_stateless_sha256": f"{index + 211:064x}",
                "installed_at": first_quarantine,
                "loopback_head": {
                    "rpc_origin": frozen["rpc_origin"],
                    "info_before_height": initial_tuple["height"],
                    "latest_height": initial_tuple["height"],
                    "block_height": initial_tuple["height"],
                    "info_after_height": initial_tuple["height"],
                    "block_hash": initial_tuple["block_hash"],
                    "state_root": initial_tuple["state_root"],
                    "response_sha256": {
                        "/info:before": f"{index + 207:064x}",
                        "/block/latest": f"{index + 208:064x}",
                        f"/block/{initial_tuple['height']}": f"{index + 209:064x}",
                        "/health": f"{index + 210:064x}",
                        "/info:after": f"{index + 212:064x}",
                    },
                    "stable_attempt": 1,
                },
                "stable_head": copy.deepcopy(initial_tuple),
                "authorization_ancestry_proof": builder.quarantine_rounds.wrap(
                    round_ancestry
                ),
                "nft_deadline_gate": builder.quarantine_rounds.wrap(round_gate),
                "applied_commit": builder.quarantine_rounds.wrap(round_commit),
                "global_absence_claimed": False,
                "threat_model": {
                    "legacy_binary": "reviewed-non-adversarial-exact-hash",
                    "legacy_binary_sha256": frozen["executable_sha256"],
                },
            }
            quarantine_receipt_sha = sha(canonical(network_quarantine_receipt))
            counters = {
                "arc-recovery:prerouting:iifname:deny": {
                    "packets": 10 + index,
                    "bytes": 320 + index,
                }
            }
            quarantine_status = {
                "schema": "arc.recovery.legacy-network-quarantine-status.v1",
                "capture_id": capture,
                "node": name,
                "freeze_plan_sha256": self.freeze_sha,
                "receipt_sha256": quarantine_receipt_sha,
                "table": {
                    "family": "inet",
                    "name": "arc_legacy_maintenance_v1",
                    "priority": -310,
                },
                "rule_counters": counters,
                "counter_snapshot_sha256": sha(canonical(counters)),
                "owned_ruleset_stateless_sha256": f"{index + 211:064x}",
                "listener_inventory": [],
                "loopback_head": {
                    "latest_height": initial_tuple["height"],
                    "info_after_height": initial_tuple["height"],
                    "block_hash": initial_tuple["block_hash"],
                    "state_root": initial_tuple["state_root"],
                    "rpc_origin": "http://127.0.0.1:9090",
                },
                "quarantine_policy": {"default": "drop"},
                "active": True,
                "enabled": True,
            }
            post_status = copy.deepcopy(quarantine_status)
            quarantine_status_sha = sha(canonical(quarantine_status))
            post_status_sha = sha(canonical(post_status))
            quarantine_monitor = {
                "schema": "arc.recovery.legacy-network-quarantine-monitor.v1",
                "capture_id": capture,
                "node": name,
                "freeze_plan_sha256": self.freeze_sha,
                "network_quarantine_receipt_sha256": quarantine_receipt_sha,
                "monitor_contract_sha256": f"{index + 212:064x}",
                "semantic_interpreter": {
                    "normalized_path": "/usr/bin/python3.12",
                    "sha256": f"{index + 213:064x}",
                    "device": 100 + index,
                    "inode": 200 + index,
                    "uid": 0,
                    "gid": 0,
                    "mode": 0o755,
                    "nlink": 1,
                    "isolated": True,
                    "environment": {
                        "PATH": "/usr/bin:/bin",
                        "LC_ALL": "C",
                        "TZ": "UTC",
                        "PYTHONHASHSEED": "0",
                    },
                },
                "firewall_loader_inventory": [
                    {
                        "unit": "netfilter-persistent.service",
                        "enablement": "enabled",
                        "unit_configuration_sha256": f"{index + 214:064x}",
                        "sources": [
                            {
                                "path": "/usr/lib/systemd/system/netfilter-persistent.service",
                                "sha256": f"{index + 215:064x}",
                            }
                        ],
                    }
                ],
                "file_sha256": {
                    "/root/.arc-recovery-network-quarantine/monitor-contract.json": f"{index + 216:064x}",
                },
                "unit": {
                    "name": "arc-legacy-maintenance-fence.service",
                    "active": True,
                    "enabled": True,
                    "continuous_poll_interval_milliseconds": 100,
                    "full_loader_revalidation_interval_seconds": 10,
                },
                "legacy_exec_start_pre": {
                    "arc-node.service": "/root/.arc-recovery-network-quarantine/validate",
                },
                "incident_latched": False,
                "continuous_fail_closed": True,
                "automatic_unfence": False,
                "global_absence_claimed": False,
            }
            challenge_payload_sha = sha(bytes.fromhex(challenge))
            targets = {"tcp": [443, 9090], "udp": [443, 9091]}
            results = [
                {
                    "protocol": "tcp",
                    "port": port,
                    "connect_succeeded": False,
                    "connect_errno": 110,
                }
                for port in targets["tcp"]
            ] + [
                {
                    "protocol": "udp",
                    "port": port,
                    "payload_sha256": challenge_payload_sha,
                    "bytes_sent": 32,
                }
                for port in targets["udp"]
            ]
            external = {
                "schema": "arc.recovery.legacy-network-quarantine-external-proof.v1",
                "capture_id": capture,
                "node": name,
                "host": host,
                "freeze_plan_sha256": self.freeze_sha,
                "challenge": challenge,
                "started_at": first_quarantine,
                "completed_at": first_quarantine,
                "operator_source_address": "192.0.2.1",
                "listener_inventory": [],
                "targets": targets,
                "results": results,
                "network_quarantine_receipt_sha256": quarantine_receipt_sha,
                "before_status_sha256": quarantine_status_sha,
                "after_status_sha256": post_status_sha,
                "after_status": post_status,
                "deny_counter": {
                    "comment": "arc-recovery:prerouting:iifname:deny",
                    "before_packets": 10 + index,
                    "after_packets": 14 + index,
                    "minimum_delta": 4,
                },
                "ssh_status_reproved": True,
                "global_absence_claimed": False,
            }
            external_sha = sha(canonical(external))
            public_after_block = {
                **public_tuple,
                "response_sha256": f"{index + 241:064x}",
            }
            public_latest_block = {
                "height": public["latest_block_height"],
                "block_hash": public["latest_block_hash"],
                "state_root": f"{index + 242:064x}",
                "response_sha256": f"{index + 243:064x}",
            }
            public_cross = {
                "schema": "arc.recovery.legacy-network-quarantine-public-cross-proof.v1",
                "capture_id": capture,
                "node": name,
                "freeze_plan_sha256": self.freeze_sha,
                "challenge": challenge,
                "network_quarantine_receipt_sha256": quarantine_receipt_sha,
                "quarantine_status_sha256": post_status_sha,
                "quarantine_status": post_status,
                "rule_counters": post_status["rule_counters"],
                "public_info_after_block": public_after_block,
                "public_latest_block": public_latest_block,
                "fenced_head": later_tuple,
                "fenced_head_covers_public_info_after": True,
                "public_latest_hash_matches": True,
                "global_absence_claimed": False,
            }
            cross_sha = sha(canonical(public_cross))
            stability_samples = []
            for sample_index in (0, 1):
                stability_head = {
                    **later_tuple,
                    "response_sha256": {
                        "info_before": f"{index + 300 + sample_index * 4:064x}",
                        "latest": f"{index + 301 + sample_index * 4:064x}",
                        "exact": f"{index + 302 + sample_index * 4:064x}",
                        "info_after": f"{index + 303 + sample_index * 4:064x}",
                    },
                    "stable_attempt": 1,
                }
                stability_sample = {
                    "schema": "arc.recovery.legacy-network-quarantine-stability-sample.v1",
                    "capture_id": capture,
                    "node": name,
                    "freeze_plan_sha256": self.freeze_sha,
                    "challenge": challenge,
                    "sample_index": sample_index,
                    "started_at": first_quarantine,
                    "completed_at": first_quarantine,
                    "quarantine_status_before": copy.deepcopy(quarantine_status),
                    "quarantine_status_before_sha256": quarantine_status_sha,
                    "quarantine_status_after": copy.deepcopy(post_status),
                    "quarantine_status_after_sha256": post_status_sha,
                    "writer": {
                        "pid": frozen["writer_pid"],
                        "start_ticks": frozen["writer_start_ticks"],
                        "executable_sha256": frozen["executable_sha256"],
                        "argv_sha256": frozen["argv_sha256"],
                        "cgroup_sha256": frozen["writer_cgroup_sha256"],
                    },
                    "listener_ownership": {
                        "rpc_tcp_9090_ss_sha256": f"{index + 330:064x}",
                        "p2p_udp_9091_ss_sha256": f"{index + 340:064x}",
                        "writer_pid": frozen["writer_pid"],
                    },
                    "head": stability_head,
                    "output_deny_packets": 20 + index + sample_index,
                    "ss_sha256": f"{index + 350 + sample_index:064x}",
                    "global_absence_claimed": False,
                }
                stability_sample_payload = canonical(stability_sample)
                stability_samples.append(
                    {"value": stability_sample, "sha256": sha(stability_sample_payload)}
                )
            stability_nodes.append(
                {
                    "node": name,
                    "host": host,
                    "samples": stability_samples,
                    "output_deny_packets": {
                        "sample_0": 20 + index,
                        "sample_1": 21 + index,
                    },
                }
            )
            stability_heads.append(
                {"node": name, "host": host, "head": copy.deepcopy(later_tuple)}
            )
            persisted = {
                "schema": "arc.recovery.persisted-legacy-head.v1",
                "source_main_commit": self.commit,
                "capture_id": capture,
                "node": name,
                "freeze_plan_sha256": self.freeze_sha,
                "boot_id": frozen["boot_id"],
                "inspector_binary_sha256": sha(self.binary.read_bytes()),
                "genesis_sha256": sha(self.genesis.read_bytes()),
                "validator_public_keys_sha256": sha(self.public_keys.read_bytes()),
                "legacy_validator_set_sha256": sha(self.legacy.read_bytes()),
                "source_pair_role": "post-quarantine-final-export",
                "final_source_capture_sha256": f"{index + 500:064x}",
                "selected_source_head": copy.deepcopy(persisted_tuple),
                "stop_after_round_receipt_sha256": f"{index + 510:064x}",
                "network_quarantine_receipt_sha256": quarantine_receipt_sha,
                "stop_complete_sha256": complete_sha,
                "stop_files_sha256": files_sha,
                "capture_complete_sha256": f"{index + 244:064x}",
                "capture_files_sha256": f"{index + 245:064x}",
                "capture_source_sha256": f"{index + 246:064x}",
                "source_data_index_sha256": f"{index + 247:064x}",
                "state_wal_sha256": f"{index + 248:064x}",
                "state_wal_size": 3,
                "snapshot_sha256": f"{index + 249:064x}",
                "snapshot_size": 8,
                "source_file_identity": {"state_wal": {}, "snapshot": {}},
                "archived_final_wal": {
                    "path": f"/private/archive/{name}/state.wal",
                    "sha256": f"{index + 520:064x}",
                    "size": 3,
                    "file_identity": {
                        "device": 500 + index,
                        "inode": 600 + index,
                        "size": 3,
                        "mode": 0o100600,
                    },
                    "selected_prefix_bytes": 3,
                    "selected_prefix_sha256": f"{index + 248:064x}",
                    "post_capture_suffix_bytes": 0,
                    "post_capture_suffix_sha256": None,
                    "post_capture_suffix_classification": "none",
                    "preserved_by": (
                        "complete-content-indexed-stopped-legacy-source-v4"
                    ),
                },
                "staged_file_contract": {
                    "state_wal": {
                        "sha256": f"{index + 248:064x}",
                        "size": 3,
                        "mode": 0o100400,
                        "uid": 0,
                        "gid": 0,
                        "nlink": 1,
                    },
                    "snapshot": {
                        "sha256": f"{index + 249:064x}",
                        "size": 8,
                        "mode": 0o100400,
                        "uid": 0,
                        "gid": 0,
                        "nlink": 1,
                    },
                    "ephemeral_inode_receipted": False,
                },
                "export_summary_sha256": f"{index + 250:064x}",
                "inspect_summary_sha256": f"{index + 260:064x}",
                "wal_boundary_sha256": f"{index + 270:064x}",
                "export_status": "EXPORTED_UNSIGNED",
                "head": persisted_tuple,
                "candidate_checkpoint_sha256": f"{index + 280:064x}",
                "candidate_checkpoint_size": 16,
                "snapshot_path": "/private/capture/state.snapshot.lz4",
                "state_wal_path": "/private/capture/state.wal",
                "export_contract": {
                    "binary_path": "/proc/self/fd/8",
                    "exit_code": 0,
                    "source_consensus_round": 0,
                    "created_at_unix_ms": 0,
                    "recovery_epoch": 1,
                    "validator_set_id": 1,
                    "allow_unbound_legacy_wal": True,
                    "read_only": True,
                },
                "completed_at": all_stopped,
                "rerun_reexecutes_export": True,
                "writer_stopped": True,
                "restart_barrier_active": True,
                "network_quarantine_active": True,
                "global_absence_claimed": False,
            }
            persisted_sha = sha(canonical(persisted))
            evidence_node_values.append(
                {
                    "node": name,
                    "host": host,
                    "stopped_status": status,
                    "quarantine_status": quarantine_status,
                    "network_quarantine_receipt": network_quarantine_receipt,
                    "nft_deadline_gate": round_gate,
                    "quarantine_monitor": quarantine_monitor,
                    "post_proof_quarantine_status": post_status,
                    "external_quarantine_proof": external,
                    "public_cross_proof": public_cross,
                    "persisted_head": persisted,
                }
            )
            boundary_nodes.append(
                {
                    "node": name,
                    "host": host,
                    "origin": public["origin"],
                    "public_observation": {
                        "tuple": public_tuple,
                        "evidence_sha256": cross_sha,
                    },
                    "authenticated_prefence_proof_sha256": proof_sha,
                    "network_quarantine_receipt_sha256": quarantine_receipt_sha,
                    "quarantine_status_sha256": quarantine_status_sha,
                    "post_proof_quarantine_status_sha256": post_status_sha,
                    "external_quarantine_proof_sha256": external_sha,
                    "public_cross_proof_sha256": cross_sha,
                    "initial_post_quarantine_head": {
                        "tuple": initial_tuple,
                        "evidence_sha256": quarantine_status_sha,
                    },
                    "post_quarantine_head": {
                        "tuple": later_tuple,
                        "evidence_sha256": cross_sha,
                    },
                    "final_persisted_head": {
                        "tuple": persisted_tuple,
                        "evidence_sha256": persisted_sha,
                    },
                }
            )
            heights = (
                public["info_before_height"], public["latest_block_height"],
                public["info_after_height"], authenticated_before, authenticated_latest,
                authenticated_after, authenticated_after, initial_tuple["height"],
                public_tuple["height"], later_tuple["height"], later_tuple["height"],
                later_tuple["height"], persisted_tuple["height"],
            )
            roots = (
                public_sha, public_sha, public_sha, proof_sha, proof_sha, proof_sha,
                proof_sha, quarantine_status_sha, cross_sha, cross_sha,
                stability_samples[0]["sha256"], stability_samples[1]["sha256"],
                persisted_sha,
            )
            evidence_heights.extend(
                {
                    "node": name,
                    "label": label,
                    "height": height,
                    "evidence_sha256": evidence_sha,
                }
                for label, height, evidence_sha in zip(labels, heights, roots)
            )
        cross_proof = {
            "schema": "arc.recovery.authenticated-legacy-height-fleet.v1",
            "source_main_commit": self.commit,
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": capture,
            "legacy_public_height_receipt_sha256": public_sha,
            "challenge": challenge,
            "started_at": fleet_started,
            "completed_at": fleet_completed,
            "conservative_height_floor": max(
                row["proof"]["conservative_height_floor"] for row in cross_nodes
            ),
            "nodes": cross_nodes,
        }
        challenge_receipt = {
            "schema": "arc.recovery.legacy-network-quarantine-challenge.v1",
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": capture,
            "challenge": challenge,
        }
        stability_proof = {
            "schema": "arc.recovery.legacy-network-quarantine-stability.v1",
            "source_main_commit": self.commit,
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": capture,
            "challenge": challenge,
            "interval_seconds": 120,
            "sample_count": 2,
            "started_at": first_quarantine,
            "completed_at": all_stopped,
            "monotonic_elapsed_ns": 120_000_000_000,
            "fleet_heads": stability_heads,
            "nodes": stability_nodes,
            "global_absence_claimed": False,
        }
        target_public = {
            **copy.deepcopy(public_receipt),
            "schema": builder.quarantine_rounds.TARGET_HEIGHT_SCHEMA,
            "targets": [
                {
                    "node": name, "host": host,
                    "rpc_origin": next(
                        row["origin"] for row in public_receipt["origins"]
                        if row["name"] == name
                    ),
                }
                for name, host in builder.FLEET
            ],
        }
        target_cross_nodes = []
        for (name, host), frozen, public, cross_row in zip(
            builder.FLEET, freeze["nodes"], public_receipt["origins"], cross_nodes
        ):
            proof = cross_row["proof"]
            loop_height = proof["authenticated_info_after_height"]
            target_cross_nodes.append({
                "node": name, "host": host,
                "writer_pid": frozen["writer_pid"],
                "writer_start_ticks": frozen["writer_start_ticks"],
                "boot_id": frozen["boot_id"],
                "writer_cgroup_sha256": frozen["writer_cgroup_sha256"],
                "public_info_after_height": public["info_after_height"],
                "public_latest_block_height": public["latest_block_height"],
                "public_latest_block_hash": public["latest_block_hash"],
                "loopback_info_before_height": loop_height,
                "loopback_latest_height": loop_height,
                "loopback_info_after_height": loop_height,
                "loopback_latest_block_hash": proof["authenticated_latest_block_hash"],
                "response_sha256": {
                    "/info:before": proof["authenticated_info_before_body_sha256"],
                    "/block/latest": proof["authenticated_latest_block_body_sha256"],
                    "/info:after": proof["authenticated_info_after_body_sha256"],
                },
            })
        target_cross = {
            "schema": builder.quarantine_rounds.TARGET_CROSS_SCHEMA,
            "source_main_commit": self.commit,
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": capture,
            "legacy_public_height_receipt_sha256": sha(canonical(target_public)),
            "challenge": challenge,
            "started_at": fleet_started,
            "completed_at": fleet_completed,
            "conservative_height_floor": min(
                row["loopback_info_before_height"] for row in target_cross_nodes
            ),
            "targets": copy.deepcopy(target_public["targets"]),
            "nodes": target_cross_nodes,
        }
        public_completed_value = dt.datetime.strptime(
            public_receipt["completed_at"], "%Y-%m-%dT%H:%M:%SZ"
        ).replace(tzinfo=dt.timezone.utc)
        generation_authorization = {
            "schema": builder.quarantine_rounds.ROUND_AUTH_SCHEMA,
            "source_main_commit": self.commit,
            "capture_id": capture, "freeze_plan_sha256": self.freeze_sha,
            "live_observation_selection_sha256": observation_selection_sha,
            "live_observation_generation": observation_generation,
            "observation_generation_receipt_sha256": (
                observation_generation_receipt_sha
            ),
            "drive_prefreeze_receipt_sha256": drive_receipt_sha,
            "live_observation_selected_at": observation_selected_at,
            "round_number": 1, "prior_round_result_sha256s": [], "prior_fenced": [],
            "targets": [{
                "node": name, "host": host, "boot_id": frozen["boot_id"],
                "writer_pid": frozen["writer_pid"],
                "writer_start_ticks": frozen["writer_start_ticks"],
                "writer_cgroup_sha256": frozen["writer_cgroup_sha256"],
            } for (name, host), frozen in zip(builder.FLEET, freeze["nodes"])],
            "public_height_receipt": builder.quarantine_rounds.wrap(target_public),
            "authenticated_height_cross_proof": builder.quarantine_rounds.wrap(target_cross),
            "authorized_at": fleet_completed,
            "authorization_deadline": (
                public_completed_value + dt.timedelta(seconds=300)
            ).strftime("%Y-%m-%dT%H:%M:%SZ"),
        }
        generation_authorization["live_source_captures"] = [
            self.live_source_capture(
                capture_id=capture,
                round_number=1,
                target=target,
                public_row=next(
                    row for row in target_public["origins"]
                    if row["name"] == target["node"]
                ),
                cross_row=next(
                    row for row in target_cross["nodes"]
                    if row["node"] == target["node"]
                ),
                public_sha256=builder.quarantine_rounds.digest(target_public),
                cross_sha256=builder.quarantine_rounds.digest(target_cross),
                captured_at=fleet_completed,
                index=index,
            )
            for index, target in enumerate(generation_authorization["targets"])
        ]
        generation_authorization_sha = builder.quarantine_rounds.digest(
            generation_authorization
        )
        generation_readiness = {
            "schema": builder.quarantine_rounds.READINESS_SCHEMA,
            "capture_id": capture, "freeze_plan_sha256": self.freeze_sha,
            "round_number": 1,
            "round_authorization_sha256": generation_authorization_sha,
            "targets": [
                {
                    "node": name, "host": host,
                    "authorization_acceptance": builder.quarantine_rounds.wrap({
                        "schema": builder.quarantine_rounds.AUTH_ACCEPTANCE_SCHEMA,
                        "capture_id": capture,
                        "freeze_plan_sha256": self.freeze_sha,
                        "round_number": 1,
                        "round_authorization_sha256": generation_authorization_sha,
                        "node": name, "host": host,
                        "accepted_at": fleet_completed,
                        "accepted_monotonic_ns": 1_000_000_000,
                        "accepted_boot_id": next(
                            row["boot_id"] for row in generation_authorization["targets"]
                            if row["node"] == name
                        ),
                        "authorization_deadline": generation_authorization[
                            "authorization_deadline"
                        ],
                    }),
                }
                for name, host in builder.FLEET
            ],
            "completed_at": fleet_completed,
            "authorization_deadline": generation_authorization[
                "authorization_deadline"
            ],
            "max_elapsed_since_acceptance_ns": 300_000_000_000,
        }
        generation_readiness_sha = builder.quarantine_rounds.digest(
            generation_readiness
        )
        applied_values = []
        for (name, host), frozen, evidence in zip(
            builder.FLEET, freeze["nodes"], evidence_node_values
        ):
            final_head = evidence["persisted_head"]["head"]
            round_public_row = next(
                row for row in target_public["origins"] if row["name"] == name
            )
            round_cross_row = next(
                row for row in target_cross["nodes"] if row["node"] == name
            )
            gate = copy.deepcopy(evidence["nft_deadline_gate"])
            ancestry = {
                "schema": builder.quarantine_rounds.ANCESTRY_SCHEMA,
                "capture_id": capture, "freeze_plan_sha256": self.freeze_sha,
                "round_authorization_sha256": builder.quarantine_rounds.digest(
                    generation_authorization
                ),
                "round_number": 1, "node": name, "host": host,
                "checks": [
                    {
                        "label": "public-latest",
                        "height": round_public_row["latest_block_height"],
                        "expected_block_hash": round_public_row["latest_block_hash"],
                        "observed_block_hash": round_public_row["latest_block_hash"],
                        "response_sha256": f"{int(name.encode().hex(), 16) % (1 << 256):064x}",
                    },
                    {
                        "label": "authenticated-loopback-latest",
                        "height": round_cross_row["loopback_latest_height"],
                        "expected_block_hash": round_cross_row[
                            "loopback_latest_block_hash"
                        ],
                        "observed_block_hash": round_cross_row[
                            "loopback_latest_block_hash"
                        ],
                        "response_sha256": round_cross_row["response_sha256"]
                            ["/block/latest"],
                    },
                ],
            }
            applied_values.append({
                "schema": builder.quarantine_rounds.NODE_APPLIED_SCHEMA,
                "capture_id": capture, "freeze_plan_sha256": self.freeze_sha,
                "round_authorization_sha256": builder.quarantine_rounds.digest(
                    generation_authorization
                ),
                "round_readiness_sha256": generation_readiness_sha,
                "round_number": 1, "node": name, "host": host,
                "boot_id": frozen["boot_id"], "writer_pid": frozen["writer_pid"],
                "writer_start_ticks": frozen["writer_start_ticks"],
                "writer_cgroup_sha256": frozen["writer_cgroup_sha256"],
                "nft_policy_source_sha256": evidence["network_quarantine_receipt"]
                    ["file_sha256"]["policy.nft"],
                "owned_ruleset_stateless_sha256": evidence["quarantine_status"]
                    ["owned_ruleset_stateless_sha256"],
                "nft_applied_at": first_quarantine,
                "nft_deadline_gate": builder.quarantine_rounds.wrap(gate),
                "network_quarantine_receipt": builder.quarantine_rounds.wrap(
                    evidence["network_quarantine_receipt"]
                ),
                "network_quarantine_receipt_sha256": evidence["quarantine_status"]
                    ["receipt_sha256"],
                "stable_head": {
                    "height": evidence["network_quarantine_receipt"]["loopback_head"]
                        ["latest_height"],
                    "block_hash": evidence["network_quarantine_receipt"]["loopback_head"]
                        ["block_hash"],
                    "state_root": evidence["network_quarantine_receipt"]["loopback_head"]
                        ["state_root"],
                },
                "authorization_ancestry_proof": builder.quarantine_rounds.wrap(
                    ancestry
                ),
                "persistent_restart_fence_sha256": evidence["network_quarantine_receipt"]
                    ["file_sha256"]["persistent-restart-fence.json"],
            })
        generation_dispatch = {
            "schema": "arc.recovery.quarantine-mutation-dispatch.v1",
            "capture_id": capture,
            "freeze_plan_sha256": self.freeze_sha,
            "round_number": 1,
            "round_authorization_sha256": builder.quarantine_rounds.digest(
                generation_authorization
            ),
            "round_readiness_sha256": builder.quarantine_rounds.digest(
                generation_readiness
            ),
            "live_observation_selection_sha256": observation_selection_sha,
            "live_observation_generation": observation_generation,
            "observation_generation_receipt_sha256": (
                observation_generation_receipt_sha
            ),
            "drive_prefreeze_receipt_sha256": drive_receipt_sha,
            "targets": [
                {"node": row["node"], "host": row["host"]}
                for row in generation_authorization["targets"]
            ],
            "dispatched_at": generation_authorization["authorized_at"],
        }
        generation_result = {
            "schema": builder.quarantine_rounds.ROUND_RESULT_SCHEMA,
            "capture_id": capture, "freeze_plan_sha256": self.freeze_sha,
            "round_number": 1,
            "round_authorization_sha256": builder.quarantine_rounds.digest(
                generation_authorization
            ),
            "target_readiness": builder.quarantine_rounds.wrap(
                generation_readiness
            ),
            "transitions": [
                builder.quarantine_rounds.wrap(item) for item in applied_values
            ],
            "mutation_dispatch": builder.quarantine_rounds.wrap(
                generation_dispatch
            ),
            "remaining_target_inert_proofs": [],
            "remaining_targets": [], "completed_at": all_stopped,
        }
        generation_ledger = {
            "schema": builder.quarantine_rounds.LEDGER_SCHEMA,
            "capture_id": capture, "freeze_plan_sha256": self.freeze_sha,
            "live_observation_selection_sha256": observation_selection_sha,
            "live_observation_generation": observation_generation,
            "observation_generation_receipt_sha256": (
                observation_generation_receipt_sha
            ),
            "drive_prefreeze_receipt_sha256": drive_receipt_sha,
            "fleet": [{"node": name, "host": host} for name, host in builder.FLEET],
            "rounds": [{
                "authorization": builder.quarantine_rounds.wrap(generation_authorization),
                "result": builder.quarantine_rounds.wrap(generation_result),
            }],
            "first_secured_at": first_quarantine,
            "all_nodes_secured_at": first_quarantine,
            "legacy_cutoff_height": max(
                [public_receipt["legacy_public_max_height"]]
                + [item["stable_head"]["height"] for item in applied_values]
            ),
        }
        stability_proof["quarantine_generation_ledger_sha256"] = (
            builder.quarantine_rounds.digest(generation_ledger)
        )
        stability_proof["active_transition_sha256s"] = [
            {
                "node": item["node"],
                "sha256": builder.quarantine_rounds.digest(item),
            }
            for item in applied_values
        ]
        inventory = []

        def sealed(value: dict, node: str, role: str) -> dict:
            payload = canonical(value)
            digest = sha(payload)
            inventory.append(
                {"node": node, "role": role, "sha256": digest, "size": len(payload)}
            )
            return {"value": value, "sha256": digest}

        authenticated_wrapper = sealed(
            cross_proof, "fleet", "authenticated-prefence-height-cross-proof"
        )
        observation_selection_wrapper = sealed(
            observation_selection, "fleet", "live-observation-selection"
        )
        generation_wrapper = sealed(
            generation_ledger, "fleet", "quarantine-generation-ledger"
        )
        challenge_wrapper = sealed(
            challenge_receipt, "fleet", "network-quarantine-challenge"
        )
        stability_wrapper = sealed(
            stability_proof, "fleet", "network-quarantine-stability-proof"
        )
        bundle_nodes = []
        for row in evidence_node_values:
            name = row["node"]
            bundle_nodes.append(
                {
                    "node": name,
                    "host": row["host"],
                    "stopped_status": sealed(
                        row["stopped_status"], name, "stopped-status"
                    ),
                    "network_quarantine_receipt": sealed(
                        row["network_quarantine_receipt"],
                        name,
                        "network-quarantine-receipt",
                    ),
                    "quarantine_status": sealed(
                        row["quarantine_status"], name, "quarantine-status"
                    ),
                    "quarantine_monitor": sealed(
                        row["quarantine_monitor"], name, "network-quarantine-monitor"
                    ),
                    "post_proof_quarantine_status": sealed(
                        row["post_proof_quarantine_status"],
                        name,
                        "post-proof-quarantine-status",
                    ),
                    "external_quarantine_proof": sealed(
                        row["external_quarantine_proof"],
                        name,
                        "external-quarantine-proof",
                    ),
                    "public_cross_proof": sealed(
                        row["public_cross_proof"], name, "public-cross-proof"
                    ),
                    "persisted_head": sealed(
                        row["persisted_head"], name, "persisted-head"
                    ),
                }
            )
        inventory_root = sha(
            canonical(
                {
                    "schema": "arc.recovery.legacy-maintenance-evidence-inventory.v1",
                    "objects": inventory,
                }
            )
        )
        bundle = {
            "schema": "arc.recovery.legacy-maintenance-evidence-bundle.v1",
            "source_main_commit": self.commit,
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": capture,
            "first_quarantine_started_at": first_quarantine,
            "all_controlled_stopped_at": all_stopped,
            "challenge": challenge,
            "authenticated_prefence_height_cross_proof": authenticated_wrapper,
            "live_observation_selection": observation_selection_wrapper,
            "quarantine_generation_ledger": generation_wrapper,
            "network_quarantine_challenge": challenge_wrapper,
            "quarantine_stability_proof": stability_wrapper,
            "nodes": bundle_nodes,
            "object_inventory": inventory,
            "aggregate_root_sha256": inventory_root,
        }
        bundle_payload = canonical(bundle)
        bundle_sha = sha(bundle_payload)
        write(self.bundle, bundle_payload, 0o400)
        write(
            self.bundle.with_name(self.bundle.name + ".sha256"),
            f"{bundle_sha}  {self.bundle.name}\n".encode(),
            0o400,
        )
        cutoff = max(row["height"] for row in evidence_heights)
        boundary = {
            "schema": "arc.recovery.legacy-maintenance-boundary.v1",
            "source_main_commit": self.commit,
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": capture,
            "first_quarantine_started_at": first_quarantine,
            "all_controlled_stopped_at": all_stopped,
            "created_at": boundary_created,
            "official_origin_scope": {
                "global_absence_claimed": False,
                "origins": [
                    {"node": name, "host": host, "origin": public["origin"]}
                    for (name, host), public in zip(builder.FLEET, public_receipt["origins"])
                ],
            },
            "legacy_public_height_receipt": {
                "schema": builder.legacy_height.SCHEMA,
                "sha256": public_sha,
                "completed_at": public_receipt["completed_at"],
                "observed_max_height": public_receipt["legacy_public_max_height"],
            },
            "authenticated_prefence_height_cross_proof_sha256": sha(canonical(cross_proof)),
            "legacy_live_observation_selection_sha256": (
                observation_selection_wrapper["sha256"]
            ),
            "legacy_live_observation_generation": observation_generation,
            "observation_generation_receipt_sha256": (
                observation_generation_receipt_sha
            ),
            "drive_prefreeze_receipt_sha256": drive_receipt_sha,
            "quarantine_generation_ledger_sha256": generation_wrapper["sha256"],
            "legacy_maintenance_evidence_bundle_sha256": bundle_sha,
            "network_quarantine_stability_proof_sha256": stability_wrapper["sha256"],
            "network_quarantine_challenge": challenge,
            "tools": {
                "remote_helper_sha256": freeze["remote_helper_sha256"],
                "inspector_binary_sha256": sha(self.binary.read_bytes()),
                "genesis_sha256": sha(self.genesis.read_bytes()),
                "validator_public_keys_sha256": sha(self.public_keys.read_bytes()),
                "legacy_validator_set_sha256": sha(self.legacy.read_bytes()),
                "orchestrator_sha256": freeze["orchestrator_sha256"],
                "rollout_tool_sha256": freeze["rollout_tool_sha256"],
                "rollout_schema_sha256": freeze["rollout_schema_sha256"],
            },
            "nodes": boundary_nodes,
            "evidence_heights": evidence_heights,
            "observed_cutoff_height": cutoff,
            "continuity_safety_margin": 128,
            "continuity_safety_margin_policy": {
                "prune_depth": 100,
                "commit_rule_rounds": 2,
                "operational_headroom": 26,
                "cryptographic_global_absence_proof": False,
            },
            "legacy_public_max_height": cutoff + 128,
            "global_absence_claimed": False,
            "reopening_policy": {
                "required_validator_count": 6,
                "height_relation": "strictly-greater-than-legacy_public_max_height",
                "required_equal_fields": ["block_hash", "state_root"],
            },
            "late_fork_circuit": {
                "monitor_scope": "retired-and-community-legacy-sources",
                "trigger": "self-consistent-legacy-fork-candidate-above-observed-cutoff-height",
                "action": "enter-maintenance-preserve-and-offline-validate",
                "rewrite_v3_history_allowed": False,
            },
            "threat_model": {
                "trusted_host_root_required": True,
                "sealed_reviewed_legacy_binary_non_adversarial": True,
                "quarantine_purpose": "operational-network-isolation",
                "hostile_root_containment_claimed": False,
            },
        }
        boundary_payload = canonical(boundary)
        write(self.boundary, boundary_payload, 0o400)
        write(
            self.boundary.with_name(self.boundary.name + ".sha256"),
            f"{sha(boundary_payload)}  {self.boundary.name}\n".encode(),
            0o400,
        )
        freeze_sidecar = self.freeze.with_name(self.freeze.name + ".sha256").read_bytes()
        receipt = {
            "all_controlled_stopped_at": all_stopped,
            "capture_id": capture,
            "first_quarantine_started_at": first_quarantine,
            "freeze_plan_sha256": self.freeze_sha,
            "freeze_plan_sidecar_sha256": sha(freeze_sidecar),
            "legacy_height_cross_proof": cross_proof,
            "legacy_maintenance_boundary": boundary,
            "legacy_maintenance_boundary_sha256": sha(boundary_payload),
            "legacy_maintenance_evidence_bundle_sha256": bundle_sha,
            "quarantine_generation_ledger_sha256": generation_wrapper["sha256"],
            "legacy_live_observation_selection_sha256": (
                observation_selection_wrapper["sha256"]
            ),
            "legacy_live_observation_generation": observation_generation,
            "observation_generation_receipt_sha256": (
                observation_generation_receipt_sha
            ),
            "drive_prefreeze_receipt_sha256": drive_receipt_sha,
            "nodes": nodes,
            "remote_helper_path": f"/root/.arc-recovery-helpers/{freeze['remote_helper_sha256']}/archive-node.sh",
            "remote_helper_sha256": freeze["remote_helper_sha256"],
            "schema": "arc.validator-vault.offline-stop-evidence.v2",
            "source_main_commit": self.commit,
        }
        payload = canonical(receipt)
        self.remote_stop_nodes = copy.deepcopy(nodes)
        write(self.offline_stop, payload, 0o400)
        write(
            self.offline_stop.with_name(self.offline_stop.name + ".sha256"),
            f"{sha(payload)}  {self.offline_stop.name}\n".encode(),
            0o400,
        )

    def rebuild_late_fork_source_set(self) -> None:
        sidecar = self.late_fork_source_set.with_name(
            self.late_fork_source_set.name + ".sha256"
        )
        for path in (self.late_fork_source_set, sidecar):
            if path.exists() or path.is_symlink():
                path.unlink()
        builder.late_fork.build_source_set(
            self.boundary,
            sha(self.boundary.read_bytes()),
            self.late_fork_source_set,
            sha((SCRIPT_DIR / "legacy-late-fork-interlock.py").read_bytes()),
        )

    def rewrite_boundary(self, mutate, *, update_embedded: bool = True) -> None:
        boundary = json.loads(self.boundary.read_text())
        mutate(boundary)
        boundary_payload = canonical(boundary)
        write(self.boundary, boundary_payload, 0o400)
        write(
            self.boundary.with_name(self.boundary.name + ".sha256"),
            f"{sha(boundary_payload)}  {self.boundary.name}\n".encode(),
            0o400,
        )
        # The late-fork monitor is intentionally pinned to the exact maintenance
        # boundary. Keep that dependent fixture coherent so boundary-focused
        # negative tests reach the validator they are meant to exercise.
        source_sidecar = self.late_fork_source_set.with_name(
            self.late_fork_source_set.name + ".sha256"
        )
        self.late_fork_source_set.unlink()
        source_sidecar.unlink()
        builder.late_fork.build_source_set(
            self.boundary,
            sha(boundary_payload),
            self.late_fork_source_set,
            sha((SCRIPT_DIR / "legacy-late-fork-interlock.py").read_bytes()),
        )
        if update_embedded:
            receipt = json.loads(self.offline_stop.read_text())
            receipt["legacy_maintenance_boundary"] = boundary
            receipt["legacy_maintenance_boundary_sha256"] = sha(boundary_payload)
            payload = canonical(receipt)
            write(self.offline_stop, payload, 0o400)
            write(
                self.offline_stop.with_name(self.offline_stop.name + ".sha256"),
                f"{sha(payload)}  {self.offline_stop.name}\n".encode(),
                0o400,
            )

    def write_union_maintenance(self, stopped_names: set[str]) -> None:
        """Rewrite the canonical active fixture as an exact mixed-state union."""

        bundle = json.loads(self.bundle.read_text())
        boundary = json.loads(self.boundary.read_text())
        generation = bundle["quarantine_generation_ledger"]["value"]
        result_value = generation["rounds"][0]["result"]["value"]
        stopped_artifacts = {}
        for index, transition_wrapper in enumerate(result_value["transitions"]):
            active_transition = transition_wrapper["value"]
            name = active_transition["node"]
            if name not in stopped_names:
                continue
            stable_head = copy.deepcopy(active_transition["stable_head"])
            fence = builder.quarantine_rounds.wrap(
                {
                    "schema": "arc.recovery.fixture-persistent-restart-fence.v1",
                    "node": name,
                    "host": active_transition["host"],
                }
            )
            live_capture_sha = f"{index + 6100:064x}"
            persisted = {
                "schema": builder.quarantine_rounds.PERSISTED_STOPPED_SCHEMA,
                "node": name,
                "host": active_transition["host"],
                "head": stable_head,
                "source_pair_role": "preauthorization-boundary",
                "live_source_capture_sha256": live_capture_sha,
                "source_inputs": {
                    "source_pair_role": "preauthorization-boundary",
                    "live_source_capture_sha256": live_capture_sha,
                },
            }
            persisted_wrapper = builder.quarantine_rounds.wrap(persisted)
            transition = {
                "schema": builder.quarantine_rounds.NODE_STOPPED_PRECOMMIT_SCHEMA,
                "node": name,
                "host": active_transition["host"],
                "stable_head": stable_head,
                "persistent_restart_fence": fence,
                "persisted_head": persisted_wrapper,
            }
            transition_wrapper = builder.quarantine_rounds.wrap(transition)
            result_value["transitions"][index] = transition_wrapper
            current = builder.quarantine_rounds.wrap(
                {
                    "stable_head": stable_head,
                    "persistent_restart_fence_sha256": fence["sha256"],
                }
            )
            stopped_artifacts[name] = {
                "transition": transition_wrapper,
                "persisted": persisted_wrapper,
                "current": current,
                "fence_sha256": fence["sha256"],
            }
        generation["rounds"][0]["result"] = builder.quarantine_rounds.wrap(
            result_value
        )
        bundle["quarantine_generation_ledger"] = builder.quarantine_rounds.wrap(
            generation
        )
        transition_wrappers = {
            wrapper["value"]["node"]: wrapper
            for wrapper in result_value["transitions"]
        }
        active_names = [
            name for name, _host in builder.FLEET if name not in stopped_names
        ]
        stability = bundle["quarantine_stability_proof"]["value"]
        stability["nodes"] = [
            row for row in stability["nodes"] if row["node"] in active_names
        ]
        stability["fleet_heads"] = [
            row for row in stability["fleet_heads"] if row["node"] in active_names
        ]
        stability["quarantine_generation_ledger_sha256"] = bundle[
            "quarantine_generation_ledger"
        ]["sha256"]
        stability["active_transition_sha256s"] = [
            {"node": name, "sha256": transition_wrappers[name]["sha256"]}
            for name in active_names
        ]
        if active_names:
            stability.update(
                {
                    "interval_seconds": 120,
                    "sample_count": 2,
                    "monotonic_elapsed_ns": 120_000_000_000,
                }
            )
        else:
            stability.update(
                {
                    "interval_seconds": 0,
                    "sample_count": 0,
                    "monotonic_elapsed_ns": 0,
                }
            )
        bundle["quarantine_stability_proof"] = builder.quarantine_rounds.wrap(
            stability
        )

        bundle_nodes = {row["node"]: row for row in bundle["nodes"]}
        for name in stopped_names:
            artifacts = stopped_artifacts[name]
            bundle_nodes[name] = {
                "node": name,
                "host": dict(builder.FLEET)[name],
                "transition_kind": (
                    builder.quarantine_rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
                ),
                "transition_receipt": artifacts["transition"],
                "current_status": artifacts["current"],
                "persisted_head": artifacts["persisted"],
            }
        bundle["nodes"] = [bundle_nodes[name] for name, _host in builder.FLEET]

        inventory = []

        def reseal(wrapper: dict, node: str, role: str) -> dict:
            value = wrapper["value"]
            payload = canonical(value)
            root = sha(payload)
            inventory.append(
                {"node": node, "role": role, "sha256": root, "size": len(payload)}
            )
            return {"value": value, "sha256": root}

        for field, role in (
            ("authenticated_prefence_height_cross_proof", "authenticated-prefence-height-cross-proof"),
            ("live_observation_selection", "live-observation-selection"),
            ("quarantine_generation_ledger", "quarantine-generation-ledger"),
            ("network_quarantine_challenge", "network-quarantine-challenge"),
            ("quarantine_stability_proof", "network-quarantine-stability-proof"),
        ):
            bundle[field] = reseal(bundle[field], "fleet", role)
        active_specs = (
            ("stopped_status", "stopped-status"),
            ("network_quarantine_receipt", "network-quarantine-receipt"),
            ("quarantine_status", "quarantine-status"),
            ("quarantine_monitor", "network-quarantine-monitor"),
            ("post_proof_quarantine_status", "post-proof-quarantine-status"),
            ("external_quarantine_proof", "external-quarantine-proof"),
            ("public_cross_proof", "public-cross-proof"),
            ("persisted_head", "persisted-head"),
        )
        for node_row in bundle["nodes"]:
            specs = (
                (
                    ("transition_receipt", "transition-receipt"),
                    ("current_status", "current-status"),
                    ("persisted_head", "persisted-head"),
                )
                if node_row.get("transition_kind")
                == builder.quarantine_rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
                else active_specs
            )
            for field, role in specs:
                node_row[field] = reseal(node_row[field], node_row["node"], role)
        bundle["object_inventory"] = inventory
        bundle["aggregate_root_sha256"] = sha(
            canonical(
                {
                    "schema": "arc.recovery.legacy-maintenance-evidence-inventory.v1",
                    "objects": inventory,
                }
            )
        )
        bundle_payload = canonical(bundle)
        bundle_sha = sha(bundle_payload)
        write(self.bundle, bundle_payload, 0o400)
        write(
            self.bundle.with_name(self.bundle.name + ".sha256"),
            f"{bundle_sha}  {self.bundle.name}\n".encode(),
            0o400,
        )

        boundary_nodes = {row["node"]: row for row in boundary["nodes"]}
        for name in stopped_names:
            prior = boundary_nodes[name]
            artifacts = stopped_artifacts[name]
            stable_head = artifacts["transition"]["value"]["stable_head"]
            boundary_nodes[name] = {
                "node": name,
                "host": prior["host"],
                "origin": prior["origin"],
                "transition_kind": (
                    builder.quarantine_rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
                ),
                "authenticated_prefence_proof_sha256": prior[
                    "authenticated_prefence_proof_sha256"
                ],
                "transition_receipt_sha256": artifacts["transition"]["sha256"],
                "current_status_sha256": artifacts["current"]["sha256"],
                "persistent_restart_fence_sha256": artifacts["fence_sha256"],
                "stable_head": {
                    "tuple": stable_head,
                    "evidence_sha256": artifacts["transition"]["sha256"],
                },
                "final_persisted_head": {
                    "tuple": stable_head,
                    "evidence_sha256": artifacts["persisted"]["sha256"],
                },
            }
        boundary["nodes"] = [boundary_nodes[name] for name, _host in builder.FLEET]
        rows_by_node = {}
        for row in boundary["evidence_heights"]:
            rows_by_node.setdefault(row["node"], {})[row["label"]] = row
        common_labels = (
            "public_info_before", "public_latest", "public_info_after",
            "authenticated_info_before", "authenticated_latest",
            "authenticated_info_after", "authenticated_conservative_floor",
        )
        evidence_heights = []
        for name, _host in builder.FLEET:
            if name not in stopped_names:
                evidence_heights.extend(rows_by_node[name].values())
                continue
            evidence_heights.extend(
                copy.deepcopy(rows_by_node[name][label]) for label in common_labels
            )
            artifacts = stopped_artifacts[name]
            stable_height = artifacts["transition"]["value"]["stable_head"]["height"]
            evidence_heights.extend(
                (
                    {
                        "node": name,
                        "label": "transition_stable_head",
                        "height": stable_height,
                        "evidence_sha256": artifacts["transition"]["sha256"],
                    },
                    {
                        "node": name,
                        "label": "final_persisted_head",
                        "height": stable_height,
                        "evidence_sha256": artifacts["persisted"]["sha256"],
                    },
                )
            )
        boundary["evidence_heights"] = evidence_heights
        boundary["quarantine_generation_ledger_sha256"] = bundle[
            "quarantine_generation_ledger"
        ]["sha256"]
        boundary["network_quarantine_stability_proof_sha256"] = bundle[
            "quarantine_stability_proof"
        ]["sha256"]
        boundary["legacy_maintenance_evidence_bundle_sha256"] = bundle_sha
        boundary["observed_cutoff_height"] = max(
            row["height"] for row in evidence_heights
        )
        boundary["legacy_public_max_height"] = boundary["observed_cutoff_height"] + 128
        boundary_payload = canonical(boundary)
        boundary_sha = sha(boundary_payload)
        write(self.boundary, boundary_payload, 0o400)
        write(
            self.boundary.with_name(self.boundary.name + ".sha256"),
            f"{boundary_sha}  {self.boundary.name}\n".encode(),
            0o400,
        )
        self.rebuild_late_fork_source_set()

        offline = json.loads(self.offline_stop.read_text())
        offline_nodes = {row["node"]: row for row in offline["nodes"]}
        final_bundle_nodes = {row["node"]: row for row in bundle["nodes"]}
        for name in stopped_names:
            bundle_node = final_bundle_nodes[name]
            offline_nodes[name] = {
                "node": name,
                "host": dict(builder.FLEET)[name],
                "transition_kind": (
                    builder.quarantine_rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
                ),
                "transition_receipt_sha256": bundle_node[
                    "transition_receipt"
                ]["sha256"],
                "current_status_sha256": bundle_node["current_status"]["sha256"],
                "persisted_head_sha256": bundle_node["persisted_head"]["sha256"],
            }
        offline["nodes"] = [offline_nodes[name] for name, _host in builder.FLEET]
        offline["legacy_maintenance_boundary"] = boundary
        offline["legacy_maintenance_boundary_sha256"] = boundary_sha
        offline["legacy_maintenance_evidence_bundle_sha256"] = bundle_sha
        offline["quarantine_generation_ledger_sha256"] = bundle[
            "quarantine_generation_ledger"
        ]["sha256"]
        offline_payload = canonical(offline)
        write(self.offline_stop, offline_payload, 0o400)
        write(
            self.offline_stop.with_name(self.offline_stop.name + ".sha256"),
            f"{sha(offline_payload)}  {self.offline_stop.name}\n".encode(),
            0o400,
        )
        self.write_validator_receipts()

    def remote_verification(
        self,
        args: argparse.Namespace,
        freeze_sha: str,
        evidence_sha: str,
        known_hosts_sha: str,
        challenge: str,
        **_staged,
    ) -> dict:
        freeze = json.loads(self.freeze.read_text())
        bundle = json.loads(self.bundle.read_text())
        bundle_by_node = {row["node"]: row for row in bundle["nodes"]}
        capture = builder.rollout.capture_id_for_freeze_plan_hash(freeze_sha)
        now = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
        timestamp = now.strftime("%Y-%m-%dT%H:%M:%SZ")
        rows = []
        for (node, host), frozen, remote in zip(
            builder.FLEET, freeze["nodes"], self.remote_stop_nodes
        ):
            bundle_node = bundle_by_node[node]
            if (
                bundle_node.get("transition_kind")
                == builder.quarantine_rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
            ):
                current = copy.deepcopy(bundle_node["current_status"]["value"])
                current["observed_at"] = timestamp
                status = {
                    "schema": (
                        "arc.recovery.quarantine-persistently-stopped-"
                        "challenged-status.v1"
                    ),
                    "capture_id": capture,
                    "freeze_plan_sha256": freeze_sha,
                    "node": node,
                    "host": host,
                    "transition_kind": (
                        builder.quarantine_rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
                    ),
                    "transition_receipt": copy.deepcopy(
                        bundle_node["transition_receipt"]
                    ),
                    "current_status": builder.quarantine_rounds.wrap(current),
                    "challenge": challenge,
                }
            else:
                status = {
                    "schema": "arc.recovery.offline-stop-challenged-status.v1",
                    "capture_id": capture,
                    "node": node,
                    "host": host,
                    "freeze_plan_sha256": freeze_sha,
                    "validator_address": frozen["validator_address"],
                    "stake": frozen["stake"],
                    "stopped": True,
                    "restart_fenced": True,
                    "stop_schema": "arc.recovery.offline-stop.v4",
                    "stop_complete_sha256": remote["stop_complete_sha256"],
                    "stop_files_sha256": remote["stop_files_sha256"],
                    "challenge": challenge,
                }
            rows.append(
                {
                    "node": node,
                    "host": host,
                    "status": status,
                    "status_sha256": sha(canonical(status)),
                }
            )
        helper_sha = freeze["remote_helper_sha256"]
        return {
            "schema": "arc.recovery.offline-stop-remote-verification.v1",
            "source_main_commit": self.commit,
            "freeze_plan_sha256": freeze_sha,
            "capture_id": capture,
            "remote_helper_sha256": helper_sha,
            "remote_helper_path": f"/root/.arc-recovery-helpers/{helper_sha}/archive-node.sh",
            "offline_stop_evidence_sha256": evidence_sha,
            "ssh_known_hosts_sha256": known_hosts_sha,
            "ssh_path": str(builder.SYSTEM_SSH),
            "ssh_sha256": sha(builder.SYSTEM_SSH.read_bytes()),
            "challenge": challenge,
            "started_at": timestamp,
            "completed_at": timestamp,
            "duration_ms": 1,
            "nodes": rows,
        }

    def installed_key_proof(
        self, _args: argparse.Namespace, manifest: dict
    ) -> dict:
        provenance = manifest["provenance"]
        archive = manifest["archive"]
        artifacts = manifest["artifacts"]
        chain = provenance["validator_key_receipt_chain"]
        now_ms = int(dt.datetime.now(dt.timezone.utc).timestamp() * 1000)
        return {
            "schema": "arc.recovery.validator-installed-key-proof.v1",
            "source_main_commit": provenance["source_main_commit"],
            "production_input_stage_manifest_sha256": provenance[
                "production_input_stage_manifest_sha256"
            ],
            "freeze_plan_sha256": archive["freeze_plan_sha256"],
            "offline_stop_evidence_sha256": artifacts["offline_stop_evidence"]["sha256"],
            "validator_install_receipt_sha256": chain["install_receipt_sha256"],
            "validator_public_keys_sha256": artifacts["validator_public_keys"]["sha256"],
            "arc_cli_sha256": artifacts["cli"]["sha256"],
            "remote_helper_sha256": archive["remote_helper_sha256"],
            "remote_helper_path": (
                f"/root/.arc-recovery-helpers/{archive['remote_helper_sha256']}/archive-node.sh"
            ),
            "ssh_known_hosts_sha256": chain["known_hosts_sha256"],
            "ssh_identity_sha256": chain["ssh_identity_sha256"],
            "ssh_path": "/usr/bin/ssh",
            "ssh_sha256": chain["ssh_sha256"],
            "scp_path": "/usr/bin/scp",
            "scp_sha256": chain["scp_sha256"],
            "challenge": "d" * 64,
            "started_at_unix_ms": now_ms - 1,
            "completed_at_unix_ms": now_ms,
            "validators": [
                {
                    "node": receipt["node"],
                    "host": receipt["host"],
                    "key_path": "/etc/arc-v3/validator-key.json",
                    "address": receipt["address"],
                    "keyfile_sha256": receipt["keyfile_sha256"],
                    "remote_response_sha256": f"{index + 301:064x}",
                    "state": "verified",
                }
                for index, receipt in enumerate(chain["validators"])
            ],
        }

    def args(self, output: pathlib.Path | None = None) -> argparse.Namespace:
        self.stage_counter += 1
        self.last_stage_root = self.root / f"production-input-stage-{self.stage_counter}"
        verified_rows = [
            {
                "kind": kind,
                "platform": platform,
                "artifact_id": self.artifact_ids[(kind, platform)],
                "raw_actions_zip": self.raw_actions_zips[(kind, platform)],
                "provenance": self.make_provenance(kind, platform),
            }
            for kind, platform in builder.PRETAG_GROUPS
        ]
        return argparse.Namespace(
            source_main_sha=self.commit,
            pretag_run_id=self.run_id,
            pretag_run_attempt=self.run_attempt,
            pretag_artifact_input_set=self.pretag_artifact_input_set,
            verified_pretag_artifacts=verified_rows,
            curl=self.curl,
            curl_sha256=sha(self.curl.read_bytes()),
            ca_bundle=self.ca_bundle,
            ca_bundle_sha256=sha(self.ca_bundle.read_bytes()),
            freeze_plan=self.freeze,
            freeze_plan_sha256=self.freeze_sha,
            legacy_public_height_receipt=self.height,
            legacy_maintenance_evidence_bundle=self.bundle,
            legacy_maintenance_boundary=self.boundary,
            legacy_late_fork_source_set=self.late_fork_source_set,
            offline_stop_evidence=self.offline_stop,
            ssh_known_hosts=self.known_hosts,
            ssh_identity=self.ssh_identity,
            validator_vault_restore_receipt=self.validator_vault_restore_receipt,
            validator_key_install_receipt=self.validator_key_install_receipt,
            binary=self.binary,
            cli=self.cli,
            build_metadata=self.build_metadata,
            genesis=self.genesis,
            validator_public_keys=self.public_keys,
            legacy_validator_set=self.legacy,
            checkpoint=self.checkpoint,
            source_snapshot=self.snapshot,
            source_wal=self.wal,
            caddy=self.caddy,
            reward_probe=self.reward_probe,
            stage_root=self.last_stage_root,
            acme_email=builder.APPROVED_ACME_EMAIL,
            output=output or self.output,
        )

    def make_provenance(self, kind: str, platform: str, *, final: bool = False) -> dict:
        group = (kind, platform)
        raw_path = self.raw_actions_zips[group]
        artifact_id = self.artifact_ids[group]
        raw_sha = sha(raw_path.read_bytes())
        archive_sha = sha(f"{kind}/{platform}/archive".encode())
        # All nine artifacts are proven from one four-request API root.  Keep
        # the fixture's shared root stable even when a test spans a wall-clock
        # second boundary.
        now = self.proof_now + (1 if final else 0)
        if group == ("headless", builder.PRETAG_PLATFORM):
            metadata = json.loads(self.build_metadata.read_text())
            files = metadata["files"]
            metadata_sha = sha(self.build_metadata.read_bytes())
        elif kind == "headless":
            suffix = ".exe" if platform == "windows-x86_64" else ""
            names = (
                f"arc-node-{platform}{suffix}",
                f"arc-cli-{platform}{suffix}",
                "genesis.toml",
            )
            files = {name: sha(f"{kind}/{platform}/{name}".encode()) for name in names}
            metadata_sha = sha(canonical(files))
        else:
            names = builder.protected_pretag.DESKTOP_FILES[platform]
            files = {name: sha(f"{kind}/{platform}/{name}".encode()) for name in names}
            metadata_sha = sha(canonical(files))
        return {
            "schema": builder.protected_pretag.PROVENANCE_SCHEMA,
            "live": {
                "repository": builder.REPOSITORY,
                "protected_branch": "main",
                "commit": self.commit,
                "workflow_id": 919191,
                "workflow_path": ".github/workflows/release-signing-preflight.yml",
                "run_id": self.run_id,
                "run_attempt": self.run_attempt,
                "artifact_id": artifact_id,
                "artifact_name": (
                    f"arc-pretag-{kind}-{platform}-{self.commit}-"
                    f"{self.run_id}-{self.run_attempt}-{archive_sha}"
                ),
                "artifact_digest": f"sha256:{raw_sha}",
                "artifact_size_in_bytes": raw_path.stat().st_size,
                "api_verified_at_unix": now,
            },
            "api": {
                "origin": builder.protected_pretag.API_ORIGIN,
                "anonymous": True,
                "redirects_followed": False,
                "max_age_seconds": builder.protected_pretag.MAX_API_AGE_SECONDS,
                "curl_sha256": sha(self.curl.read_bytes()),
                "ca_bundle_sha256": sha(self.ca_bundle.read_bytes()),
                "responses": [
                    {
                        "label": label,
                        "body_sha256": sha(f"shared/{label}/{final}".encode()),
                        "response_unix": now,
                        "request_id": f"ABCDEF00-{index + 1:02d}",
                        "cache_control": "public, max-age=60, s-maxage=60",
                        "age": 0,
                    }
                    for index, label in enumerate(("workflow", "run", "artifact_set", "protected_main"))
                ],
            },
            "artifact": {
                "kind": kind,
                "platform": platform,
                "version": builder.VERSION,
                "raw_actions_zip_sha256": raw_sha,
                "raw_actions_zip_size": raw_path.stat().st_size,
                "archive_sha256": archive_sha,
                "build_metadata_sha256": metadata_sha,
                "files": files,
            },
        }

    def make_single_linux_provenance(self, *, final: bool = False) -> dict:
        value = self.make_provenance("headless", builder.PRETAG_PLATFORM, final=final)
        for index, response in enumerate(value["api"]["responses"]):
            label = ("workflow", "run", "artifact", "protected_main")[index]
            response["label"] = label
            response["body_sha256"] = sha(f"single/{label}/{final}".encode())
        return value

    def write_validator_receipts(self) -> None:
        public_rows = json.loads(self.public_keys.read_text())
        restore_rows = []
        install_rows = []
        for index, ((lower, _host), public) in enumerate(zip(builder.FLEET, public_rows)):
            upper = lower.upper()
            key_sha = sha(f"validator-key-{upper}".encode())
            restore_rows.append(
                {
                    "node": upper,
                    "key_file": f"keys/{upper}.validator-key.json",
                    "address": public["address"],
                    "keyfile_sha256": key_sha,
                }
            )
            install_rows.append(
                {
                    "node": upper,
                    "address": public["address"],
                    "keyfile_sha256": key_sha,
                    "destination": "/etc/arc-v3/validator-key.json",
                    "state": "verified",
                }
            )
        common = {
            "source_commit": self.commit,
            "cms_sha256": "1" * 64,
            "arc_cli_sha256": sha(self.cli.read_bytes()),
            "genesis_sha256": sha(self.genesis.read_bytes()),
            "pretag_initial_provenance": self.make_single_linux_provenance(),
            "pretag_final_provenance": self.make_single_linux_provenance(final=True),
        }
        restore = {
            "schema": "arc.validator-vault.restore.v1",
            **common,
            "source_ciphertext_sha256": "2" * 64,
            "restore_cert_sha256": "3" * 64,
            "openssl_sha256": "4" * 64,
            "openssl_libssl_sha256": "5" * 64,
            "openssl_libcrypto_sha256": "6" * 64,
            "validators": restore_rows,
        }
        install = {
            "schema": "arc.validator-vault.install.v1",
            **common,
            "known_hosts_sha256": sha(self.known_hosts.read_bytes()),
            "ssh_identity_sha256": sha(self.ssh_identity.read_bytes()),
            "ssh_sha256": sha(builder.SYSTEM_SSH.read_bytes()),
            "scp_sha256": "7" * 64,
            "freeze_plan_sha256": self.freeze_sha,
            "offline_stop_evidence_sha256": sha(self.offline_stop.read_bytes()),
            "validators": install_rows,
        }
        write(self.validator_vault_restore_receipt, canonical(restore), 0o600)
        write(self.validator_key_install_receipt, canonical(install), 0o444)

    @contextlib.contextmanager
    def protected_artifact_proof(self, **kwargs):
        kind, platform, raw_path = self.assert_protected_artifact_call(kwargs)
        provenance = self.make_provenance(kind, platform)
        provenance_bytes = canonical(provenance)
        provenance_path = self.root / f"live-provenance-{kind}-{platform}-{self.stage_counter}.json"
        write(provenance_path, provenance_bytes, 0o400)
        yield types.SimpleNamespace(
            raw_actions_zip=raw_path,
            provenance_path=provenance_path,
            payloads={
                "arc-node-linux-x86_64": self.binary,
                "arc-cli-linux-x86_64": self.cli,
                "genesis.toml": self.genesis,
            },
            build_metadata_path=self.build_metadata,
            provenance=provenance,
            provenance_bytes=provenance_bytes,
        )

    @contextlib.contextmanager
    def protected_artifact_set_proof(self, **kwargs):
        rows = kwargs.pop("rows")
        common = {
            "expected_commit": self.commit,
            "expected_run_id": self.run_id,
            "expected_run_attempt": self.run_attempt,
            "expected_version": builder.VERSION,
            "curl": self.curl,
            "curl_sha256": sha(self.curl.read_bytes()),
            "ca_bundle": self.ca_bundle,
            "ca_bundle_sha256": sha(self.ca_bundle.read_bytes()),
        }
        if kwargs != common or len(rows) != len(builder.PRETAG_GROUPS):
            raise AssertionError("protected artifact set common tuple differs")
        with contextlib.ExitStack() as stack:
            proofs = []
            for row, (kind, platform) in zip(rows, builder.PRETAG_GROUPS):
                expected_row = {
                    "raw_actions_zip": self.raw_actions_zips[(kind, platform)],
                    "expected_artifact_id": self.artifact_ids[(kind, platform)],
                    "kind": kind,
                    "platform": platform,
                }
                if row != expected_row:
                    raise AssertionError("protected artifact set rows differ")
                proofs.append(
                    stack.enter_context(
                        self.protected_artifact_proof(**row, **common)
                    )
                )
            yield types.SimpleNamespace(artifacts=proofs, api_request_count=4)

    def assert_protected_artifact_call(self, kwargs: dict) -> tuple[str, str, pathlib.Path]:
        kind = kwargs.get("kind")
        platform = kwargs.get("platform")
        if (kind, platform) not in builder.PRETAG_GROUPS:
            raise AssertionError("protected artifact call used an unknown group")
        supplied_raw = pathlib.Path(kwargs.pop("raw_actions_zip"))
        expected_raw = self.raw_actions_zips[(kind, platform)]
        if supplied_raw.read_bytes() != expected_raw.read_bytes():
            raise AssertionError("protected artifact call used different raw Actions ZIP bytes")
        expected = {
            "expected_commit": self.commit,
            "expected_run_id": self.run_id,
            "expected_run_attempt": self.run_attempt,
            "expected_artifact_id": self.artifact_ids[(kind, platform)],
            "kind": kind,
            "platform": platform,
            "expected_version": builder.VERSION,
            "curl": self.curl,
            "curl_sha256": sha(self.curl.read_bytes()),
            "ca_bundle": self.ca_bundle,
            "ca_bundle_sha256": sha(self.ca_bundle.read_bytes()),
        }
        if kwargs != expected:
            raise AssertionError(f"protected artifact call differs: {kwargs!r}")
        return kind, platform, supplied_raw

    def final_live_reproof(self, **kwargs):
        initial = json.loads(bytes(kwargs.pop("initial_provenance_bytes")))
        kind = kwargs["kind"]
        platform = kwargs["platform"]
        expected = self.make_provenance(kind, platform)
        if initial["live"]["artifact_id"] != expected["live"]["artifact_id"]:
            raise AssertionError("final live reproof initial artifact differs")
        kwargs_with_raw = dict(kwargs)
        kwargs_with_raw["raw_actions_zip"] = self.raw_actions_zips[(kind, platform)]
        self.assert_protected_artifact_call(kwargs_with_raw)
        final = self.make_provenance(kind, platform, final=True)
        return types.SimpleNamespace(value=final, canonical_bytes=canonical(final), path=None)

    def final_live_set_reproof(self, **kwargs):
        initial_values = kwargs.pop("initial_provenance_bytes_list")
        artifact_ids = kwargs.pop("expected_artifact_ids")
        common = {
            "expected_commit": self.commit,
            "expected_run_id": self.run_id,
            "expected_run_attempt": self.run_attempt,
            "expected_version": builder.VERSION,
            "curl": self.curl,
            "curl_sha256": sha(self.curl.read_bytes()),
            "ca_bundle": self.ca_bundle,
            "ca_bundle_sha256": sha(self.ca_bundle.read_bytes()),
        }
        if kwargs != common or artifact_ids != [self.artifact_ids[group] for group in builder.PRETAG_GROUPS]:
            raise AssertionError("final protected artifact set common tuple differs")
        proofs = []
        for initial, (kind, platform) in zip(initial_values, builder.PRETAG_GROUPS):
            proofs.append(
                self.final_live_reproof(
                    initial_provenance_bytes=initial,
                    expected_artifact_id=self.artifact_ids[(kind, platform)],
                    kind=kind,
                    platform=platform,
                    **common,
                )
            )
        return types.SimpleNamespace(proofs=proofs, api_request_count=4)

    def build(self) -> tuple[dict, str]:
        digest = builder.prearchive(self.args())
        value = json.loads(self.output.read_text())
        return value, digest

    def archive_evidence(self, prearchive: dict, prearchive_sha: str) -> argparse.Namespace:
        self.archive_counter += 1
        archive_dir = self.root / f"archive-{self.archive_counter}"
        archive_dir.mkdir(mode=0o700)
        artifacts = prearchive["artifacts"]

        def art(name: str, archive_name: str) -> dict:
            path = pathlib.Path(artifacts[name]["path"])
            return {"name": archive_name, "size": path.stat().st_size, "sha256": artifacts[name]["sha256"]}

        shared = {
            "arc-node": art("binary", "arc-node"),
            "arc-cli": art("cli", "arc-cli"),
            "build-metadata.json": art("build_metadata", "build-metadata.json"),
            "genesis.toml": art("genesis", "genesis.toml"),
            "validator-public-keys.json": art("validator_public_keys", "validator-public-keys.json"),
            "legacy-public-height.json": art("legacy_public_height_receipt", "legacy-public-height.json"),
            "legacy-maintenance-evidence-bundle.json": art(
                "legacy_maintenance_evidence_bundle",
                "legacy-maintenance-evidence-bundle.json",
            ),
            "legacy-maintenance-evidence-bundle.json.sha256": art(
                "legacy_maintenance_evidence_bundle_sidecar",
                "legacy-maintenance-evidence-bundle.json.sha256",
            ),
            "legacy-maintenance-boundary.json": art(
                "legacy_maintenance_boundary", "legacy-maintenance-boundary.json"
            ),
            "legacy-maintenance-boundary.json.sha256": art(
                "legacy_maintenance_boundary_sidecar",
                "legacy-maintenance-boundary.json.sha256",
            ),
            "legacy-late-fork-source-set.json": art(
                "legacy_late_fork_source_set",
                "legacy-late-fork-source-set.json",
            ),
            "legacy-late-fork-source-set.json.sha256": art(
                "legacy_late_fork_source_set_sidecar",
                "legacy-late-fork-source-set.json.sha256",
            ),
            "legacy-late-fork-interlock.py": art(
                "legacy_late_fork_interlock_tool",
                "legacy-late-fork-interlock.py",
            ),
            "offline-stop-evidence.json": art("offline_stop_evidence", "offline-stop-evidence.json"),
            "offline-stop-evidence.json.sha256": art(
                "offline_stop_evidence_sidecar", "offline-stop-evidence.json.sha256"
            ),
            "ssh-known-hosts": art("ssh_known_hosts", "ssh-known-hosts"),
            "legacy-validator-set-40m.json": art("legacy_validator_set", "legacy-validator-set-40m.json"),
            "source.snapshot.lz4": art("source_snapshot", "source.snapshot.lz4"),
            "source.state.wal": art("source_wal", "source.state.wal"),
            "recovery.arcchkpt": art("checkpoint", "recovery.arcchkpt"),
            "caddy": art("caddy", "caddy"),
            "community-reward-probe.py": art("reward_probe", "community-reward-probe.py"),
            "PRETAG-ARTIFACT-INPUT-SET.json": art(
                "pretag_artifact_input_set", "PRETAG-ARTIFACT-INPUT-SET.json"
            ),
            "PRETAG-INITIAL-LIVE-PROVENANCE-SET.json": art(
                "pretag_initial_live_provenance_set",
                "PRETAG-INITIAL-LIVE-PROVENANCE-SET.json",
            ),
            "PRODUCTION-INPUT-STAGE-MANIFEST.json": art(
                "production_input_stage_manifest",
                "PRODUCTION-INPUT-STAGE-MANIFEST.json",
            ),
            "VALIDATOR-VAULT-RESTORE-RECEIPT.json": art(
                "validator_vault_restore_receipt",
                "VALIDATOR-VAULT-RESTORE-RECEIPT.json",
            ),
            "VALIDATOR-KEY-INSTALL-RECEIPT.json": art(
                "validator_key_install_receipt",
                "VALIDATOR-KEY-INSTALL-RECEIPT.json",
            ),
        }
        freeze = json.loads(self.freeze.read_text())
        drive = freeze["drive_prefreeze"]
        source_bytes = sum(row["data_bytes"] for row in freeze["nodes"])
        archive_reservation = 3 * source_bytes + 32 * 1024**3
        largest_reservation = (
            3 * max(row["data_bytes"] for row in freeze["nodes"])
            + 4 * 1024**3
        )
        drive_receipt = {
            "schema": "arc.recovery.drive-prefreeze.v1",
            "mode": "execute",
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": prearchive["archive"]["capture_id"],
            "remote_root_sha256": drive["remote_root_sha256"],
            "client_id_sha256": drive["oauth_client_id_sha256"],
            "account_sha256": drive["account_sha256"],
            "permission_id_sha256": "d" * 64,
            "rclone_version": "v1.75.0",
            "source_bytes": source_bytes,
            "archive_reservation_bytes": archive_reservation,
            "largest_object_reservation_bytes": largest_reservation,
            "daily_upload_budget_bytes": drive["daily_upload_budget_bytes"],
            "daily_upload_budget_basis": (
                "operator-reviewed-remaining-dedicated-account"
            ),
            "available_bytes_before": archive_reservation + 8 * 1024**2,
            "available_bytes_after": archive_reservation,
            "canary_bytes": 8 * 1024**2,
            "canary_verified": True,
            "canary_deleted": True,
        }
        drive_receipt_path = archive_dir / "drive-archive-seal-prefreeze.json"
        drive_receipt_payload = canonical(drive_receipt)
        write(drive_receipt_path, drive_receipt_payload, 0o400)
        shared["drive-archive-seal-prefreeze.json"] = {
            "name": "drive-archive-seal-prefreeze.json",
            "size": len(drive_receipt_payload),
            "sha256": sha(drive_receipt_payload),
        }
        drive_attempt = {
            "schema": "arc.recovery.drive-archive-seal-attempt.v1",
            "phase": "archive-seal",
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": prearchive["archive"]["capture_id"],
            "attempt_nonce": "e" * 64,
            "started_at_unix_ns": 1_787_857_623_000_000_000,
            "completed_at_unix_ns": 1_787_857_623_100_000_000,
            "completed_at": dt.datetime.now(dt.timezone.utc)
            .replace(microsecond=0)
            .strftime("%Y-%m-%dT%H:%M:%SZ"),
            "drive_prefreeze_receipt": drive_receipt,
            "drive_prefreeze_receipt_sha256": sha(drive_receipt_payload),
            "rclone_path": "/usr/local/bin/rclone",
            "rclone_sha256": "f" * 64,
            "rclone_config_sha256": "a" * 64,
            "selected_immediately_before_first_archive_upload": True,
        }
        drive_attempt_path = archive_dir / "drive-archive-seal-attempt.json"
        drive_attempt_payload = canonical(drive_attempt)
        write(drive_attempt_path, drive_attempt_payload, 0o400)
        shared["drive-archive-seal-attempt.json"] = {
            "name": "drive-archive-seal-attempt.json",
            "size": len(drive_attempt_payload),
            "sha256": sha(drive_attempt_payload),
        }
        gist_challenge = "c" * 64
        gist_content = (
            f"freeze_plan_sha256={self.freeze_sha}\n"
            f"capture_id={prearchive['archive']['capture_id']}\n"
            f"challenge={gist_challenge}\n"
        ).encode()
        gist_canary = {
            "schema": "arc.recovery.github-gist-write-canary.v1",
            "provider": "github.com",
            "owner_login": "FerrumVir",
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": prearchive["archive"]["capture_id"],
            "challenge": gist_challenge,
            "gist_id": "d" * 32,
            "gist_revision": "e" * 40,
            "gist_filename": f"arc-recovery-gist-canary-{gist_challenge}.txt",
            "gist_content_sha256": sha(gist_content),
            "github_cli_path": str(self.binary.resolve()),
            "github_cli_sha256": sha(self.binary.read_bytes()),
            "create_verified": True,
            "revision_read_verified": True,
            "delete_verified": True,
            "completed_at": dt.datetime.fromtimestamp(
                self.proof_now, tz=dt.timezone.utc
            ).strftime("%Y-%m-%dT%H:%M:%SZ"),
        }
        gist_canary_path = archive_dir / "github-gist-write-canary.json"
        gist_canary_payload = canonical(gist_canary)
        write(gist_canary_path, gist_canary_payload, 0o400)
        shared["github-gist-write-canary.json"] = {
            "name": "github-gist-write-canary.json",
            "size": len(gist_canary_payload),
            "sha256": sha(gist_canary_payload),
        }
        for kind, platform in builder.PRETAG_GROUPS:
            key = builder.pretag_artifact_key(kind, platform)
            name = f"pretag-{kind}-{platform}.actions.zip"
            shared[name] = art(key, name)
        validator_chain_payload = canonical(
            prearchive["provenance"]["validator_key_receipt_chain"]
        )
        shared["VALIDATOR-KEY-RECEIPT-CHAIN.json"] = {
            "name": "VALIDATOR-KEY-RECEIPT-CHAIN.json",
            "size": len(validator_chain_payload),
            "sha256": sha(validator_chain_payload),
        }
        for field, name, path in (
            ("archive_orchestrator_sha256", "archive-fleet-to-drive.sh", SCRIPT_DIR / "archive-fleet-to-drive.sh"),
            ("remote_helper_sha256", "archive-node.sh", SCRIPT_DIR / "archive-node.sh"),
            ("rollout_tool_sha256", "recovery_rollout.py", SCRIPT_DIR / "recovery_rollout.py"),
            ("rollout_schema_sha256", "recovery-manifest.schema.json", SCRIPT_DIR / "recovery-manifest.schema.json"),
        ):
            shared[name] = {"name": name, "size": path.stat().st_size, "sha256": prearchive["archive"][field]}
        freeze_payload = self.freeze.read_bytes()
        freeze_sidecar = self.freeze.with_name(self.freeze.name + ".sha256").read_bytes()
        prearchive_payload = self.output.read_bytes()
        prearchive_sidecar = self.output.with_name(self.output.name + ".sha256").read_bytes()
        for name, payload in (
            ("freeze-plan.json", freeze_payload),
            ("freeze-plan.json.sha256", freeze_sidecar),
            ("rollout-manifest.json", prearchive_payload),
            ("rollout-manifest.json.sha256", prearchive_sidecar),
            ("source-commit.txt", (self.commit + "\n").encode()),
            ("capture-id.txt", (prearchive["archive"]["capture_id"] + "\n").encode()),
        ):
            shared[name] = {"name": name, "size": len(payload), "sha256": sha(payload)}
        reference = {
            "schema": "arc.recovery.canonical-reference.v1",
            "independently_verified": True,
            "allow_unbound_legacy_wal": True,
            "verifier_binary": shared["arc-node"],
            "genesis": shared["genesis.toml"],
            "validator_public_keys": shared["validator-public-keys.json"],
            "legacy_validator_set": shared["legacy-validator-set-40m.json"],
            "source_snapshot": shared["source.snapshot.lz4"],
            "source_wal": shared["source.state.wal"],
            "selected_checkpoint": shared["recovery.arcchkpt"],
            "source_height": prearchive["chain"]["source_height"],
            "source_block_hash": prearchive["chain"]["source_block_hash"].removeprefix("0x"),
            "source_state_root": prearchive["chain"]["source_state_root"].removeprefix("0x"),
            "transition_state_root": prearchive["chain"]["full_state_root"].removeprefix("0x"),
            "checkpoint_manifest_hash": prearchive["chain"]["approved_checkpoint_manifest_hash"].removeprefix("0x"),
            "source_consensus_round": prearchive["chain"]["source_consensus_round"],
            "created_at_unix_ms": prearchive["chain"]["created_at_unix_ms"],
            "recovery_epoch": prearchive["chain"]["recovery_epoch"],
            "validator_set_id": prearchive["chain"]["validator_set_id"],
        }
        for name, payload in (
            ("canonical-reference.json", canonical(reference)),
            ("archive-seal-options.json", canonical({"allow_unbound_legacy_wal": True})),
        ):
            shared[name] = {"name": name, "size": len(payload), "sha256": sha(payload)}
        observations = canonical(
            {
                "schema": "arc.recovery.legacy-live-observations-fleet.v1",
                "capture_id": prearchive["archive"]["capture_id"],
                "freeze_plan_sha256": self.freeze_sha,
                "receipt_schema": "arc.recovery.legacy-live-observations.v1",
                "labels": ["diagnostic", "noncanonical", "nonreward"],
                "nodes": [
                    {"node": name, "root_sha256": f"{index + 10:064x}", "receipt_sha256": f"{index + 20:064x}"}
                    for index, (name, _host) in enumerate(builder.FLEET)
                ],
            }
        )
        shared["legacy-live-observations.json"] = {
            "name": "legacy-live-observations.json",
            "size": len(observations),
            "sha256": sha(observations),
        }
        bundles = []
        sums = {name: item["sha256"] for name, item in shared.items()}
        for index, (node, _host) in enumerate(builder.FLEET):
            row = {"node": node, "classification": "preserved_unclassified"}
            for label, suffix in (("bundle", ".tar.zst"), ("inventory", ".inventory")):
                name = f"legacy-{node}{suffix}"
                digest = f"{index + (30 if label == 'bundle' else 40):064x}"
                sidecar_payload = f"{digest}  {name}\n".encode()
                item = {
                    "name": name,
                    "size": 100 + index,
                    "sha256": digest,
                    "sidecar_name": name + ".sha256",
                    "sidecar_sha256": sha(sidecar_payload),
                }
                row[label] = item
                sums[name] = digest
                sums[name + ".sha256"] = item["sidecar_sha256"]
            bundles.append(row)
        sums_payload = "".join(f"{digest}  {name}\n" for name, digest in sorted(sums.items())).encode()
        sums_path = archive_dir / "SHA256SUMS"
        write(sums_path, sums_payload, 0o400)
        manifest = {
            "schema": "arc.recovery.archive-manifest.v2",
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": prearchive["archive"]["capture_id"],
            "rollout_manifest_sha256": prearchive_sha,
            "source_commit": self.commit,
            "orchestrator_sha256": prearchive["archive"]["archive_orchestrator_sha256"],
            "remote_helper_sha256": prearchive["archive"]["remote_helper_sha256"],
            "rollout_tool_sha256": prearchive["archive"]["rollout_tool_sha256"],
            "rollout_schema_sha256": prearchive["archive"]["rollout_schema_sha256"],
            "canonical_reference": reference,
            "capture_classification_counts": {
                "valid_canonical": 0,
                "valid_noncanonical_fork": 0,
                "preserved_unclassified": 6,
            },
            "shared_inputs": [shared[name] for name in sorted(shared)],
            "validator_bundles": bundles,
            "sha256sums": {"name": "SHA256SUMS", "size": len(sums_payload), "sha256": sha(sums_payload)},
        }
        manifest_path = archive_dir / "ARCHIVE-MANIFEST.json"
        manifest_payload = canonical(manifest)
        write(manifest_path, manifest_payload, 0o400)
        manifest_sha = sha(manifest_payload)
        manifest_sidecar = archive_dir / "ARCHIVE-MANIFEST.json.sha256"
        write(manifest_sidecar, f"{manifest_sha}  ARCHIVE-MANIFEST.json\n".encode(), 0o400)
        complete = {
            "schema": "arc.recovery.archive-complete.v2",
            "freeze_plan_sha256": self.freeze_sha,
            "capture_id": prearchive["archive"]["capture_id"],
            "rollout_manifest_sha256": prearchive_sha,
            "source_commit": self.commit,
            "archive_manifest_sha256": manifest_sha,
            "object_count_before_complete": len(shared) + 24 + 3,
            "validator_bundle_count": 6,
            "finalization_anchor": {
                "intent_sha256": "9" * 64,
                "gist_id": "a" * 32,
                "gist_revision": "b" * 40,
                "gist_file_sha256": "9" * 64,
            },
        }
        complete_path = archive_dir / "COMPLETE.json"
        complete_payload = canonical(complete)
        write(complete_path, complete_payload, 0o400)
        return argparse.Namespace(
            prearchive=self.output,
            complete=complete_path,
            complete_sha256=sha(complete_payload),
            archive_manifest=manifest_path,
            archive_manifest_sidecar=manifest_sidecar,
            archive_manifest_sha256=manifest_sha,
            sha256sums=sums_path,
            sha256sums_sha256=sha(sums_payload),
            drive_archive_seal_prefreeze=drive_receipt_path,
            drive_archive_seal_attempt=drive_attempt_path,
            github_gist_write_canary=gist_canary_path,
            output=self.root / "final.json",
        )


class ProductionManifestBuilderTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(
            prefix=".arc-production-builder-test-", dir=REPO_ROOT
        )
        self.fixture = Fixture(pathlib.Path(self.temporary.name))
        self.caddy_digest = sha(self.fixture.caddy.read_bytes())
        self.patches = (
            mock.patch.object(builder.rollout, "CADDY_LINUX_AMD64_SHA256", self.caddy_digest),
            mock.patch.object(builder.rollout, "CADDY_VERSION", "v2.11.4-test"),
            mock.patch.object(
                builder,
                "execute_remote_stop_verifier",
                side_effect=self.fixture.remote_verification,
            ),
            mock.patch.object(
                builder.protected_pretag,
                "pretag_actions_set_proof",
                side_effect=self.fixture.protected_artifact_set_proof,
            ),
            mock.patch.object(
                builder.protected_pretag,
                "final_live_set_reproof",
                side_effect=self.fixture.final_live_set_reproof,
            ),
            mock.patch.object(
                builder,
                "execute_installed_key_verifier",
                side_effect=self.fixture.installed_key_proof,
            ),
            mock.patch.object(builder, "validate_protected_main_commit"),
        )
        for patch in self.patches:
            patch.start()

    def tearDown(self) -> None:
        for patch in reversed(self.patches):
            patch.stop()
        self.temporary.cleanup()

    def test_protected_main_commit_is_verified_against_the_executing_worktree(self) -> None:
        source_patch = self.patches[-1]
        source_patch.stop()
        try:
            with self.assertRaisesRegex(builder.BuilderError, "worktree HEAD differs"):
                builder.validate_protected_main_commit("0" * 40)
        finally:
            source_patch.start()

    def test_prearchive_derives_every_field_and_is_private_create_only(self) -> None:
        value, digest = self.fixture.build()
        self.assertEqual(digest, sha(self.fixture.output.read_bytes()))
        self.assertEqual(stat.S_IMODE(self.fixture.output.stat().st_mode), 0o400)
        self.assertEqual(
            stat.S_IMODE(self.fixture.output.with_name(self.fixture.output.name + ".sha256").stat().st_mode),
            0o400,
        )
        self.assertEqual(value["chain"]["legacy_observed_cutoff_height"], 137507)
        self.assertEqual(value["chain"]["legacy_continuity_safety_margin"], 128)
        self.assertEqual(value["chain"]["legacy_public_max_height"], 137635)
        self.assertFalse(value["chain"]["legacy_global_absence_claimed"])
        self.assertEqual(
            value["chain"]["legacy_maintenance_boundary_sha256"],
            value["artifacts"]["legacy_maintenance_boundary"]["sha256"],
        )
        self.assertEqual(value["chain"]["source_height"], 137145)
        self.assertEqual(value["provenance"]["source_main_commit"], self.fixture.commit)
        installed = value["provenance"]["validator_installed_key_proof"]
        self.assertEqual(installed["schema"], "arc.recovery.validator-installed-key-proof.v1")
        self.assertEqual(
            [(row["node"], row["host"]) for row in installed["validators"]],
            list(builder.FLEET),
        )
        self.assertEqual(len({row["keyfile_sha256"] for row in installed["validators"]}), 6)
        proof_set = value["provenance"]["protected_pretag_artifact"]
        self.assertEqual(proof_set["schema"], builder.PRETAG_WINDOW_SET_SCHEMA)
        self.assertEqual(
            [(row["kind"], row["platform"]) for row in proof_set["groups"]],
            list(builder.PRETAG_GROUPS),
        )
        self.assertEqual(len({row["initial"]["live"]["artifact_id"] for row in proof_set["groups"]}), 9)
        for kind, platform in builder.PRETAG_GROUPS:
            key = builder.pretag_artifact_key(kind, platform)
            self.assertIn(key, value["artifacts"])
            self.assertEqual(
                value["artifacts"][key]["sha256"],
                self.fixture.make_provenance(kind, platform)["artifact"]["raw_actions_zip_sha256"],
            )
        self.assertEqual(value["validators"][0]["host"], builder.FLEET[0][1])
        self.assertEqual(value["validators"][0]["shard_ranges"], builder.SHARDS["nyc"])
        self.assertEqual(value["archive"]["complete_sha256"], builder.ZERO_HASH)
        assert self.fixture.last_stage_root is not None
        stage_root = self.fixture.last_stage_root
        self.assertEqual(stat.S_IMODE(stage_root.stat().st_mode), 0o500)
        self.assertEqual(stat.S_IMODE((stage_root / "source").stat().st_mode), 0o500)
        self.assertEqual(
            value["artifacts"]["production_input_stage_manifest"]["sha256"],
            value["provenance"]["production_input_stage_manifest_sha256"],
        )
        self.assertTrue(
            all(
                pathlib.Path(row["path"]).is_relative_to(stage_root)
                for row in value["artifacts"].values()
                if "path" in row
            )
        )
        with self.assertRaisesRegex(builder.BuilderError, "already exists"):
            builder.prearchive(self.fixture.args())

    def build_union_fixture(self, stopped_names: set[str]) -> tuple[dict, str]:
        self.fixture.write_union_maintenance(stopped_names)
        boundary = json.loads(self.fixture.boundary.read_text())
        first = dt.datetime.strptime(
            boundary["first_quarantine_started_at"], "%Y-%m-%dT%H:%M:%SZ"
        ).replace(tzinfo=dt.timezone.utc)
        all_stopped = dt.datetime.strptime(
            boundary["all_controlled_stopped_at"], "%Y-%m-%dT%H:%M:%SZ"
        ).replace(tzinfo=dt.timezone.utc)
        bundle = json.loads(self.fixture.bundle.read_text())
        selection = bundle["live_observation_selection"]
        state = {
            "capture_id": builder.rollout.capture_id_for_freeze_plan_hash(
                self.fixture.freeze_sha
            ),
            "freeze_plan_sha256": self.fixture.freeze_sha,
            "first_secured_at": first,
            "all_nodes_secured_at": first,
            "legacy_cutoff_height": 0,
            "live_observation_selection_sha256": selection["sha256"],
            "live_observation_generation": selection["value"][
                "observation_generation"
            ],
            "observation_generation_receipt_sha256": selection["value"][
                "observation_generation_receipt_sha256"
            ],
            "drive_prefreeze_receipt_sha256": selection["value"][
                "drive_prefreeze_receipt_sha256"
            ],
        }
        with (
            mock.patch.object(
                builder.quarantine_rounds,
                "validate_generation_ledger",
                return_value=state,
            ),
            mock.patch.object(
                builder.quarantine_rounds,
                "validate_prior_fenced_status",
                return_value=all_stopped,
            ),
            mock.patch.object(
                builder.quarantine_rounds,
                "validate_node_transition",
                side_effect=lambda transition, **_kwargs: {
                    "kind": (
                        builder.quarantine_rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
                    ),
                    "node": transition["node"],
                    "host": transition["host"],
                },
            ),
        ):
            return self.fixture.build()

    def test_prearchive_accepts_exact_three_active_three_stopped_union(self) -> None:
        value, _digest = self.build_union_fixture({"lhr", "nrt", "sgp"})
        bundle = json.loads(self.fixture.bundle.read_text())
        stability = bundle["quarantine_stability_proof"]["value"]
        self.assertEqual(
            [row["node"] for row in stability["nodes"]],
            ["nyc", "lax", "ams"],
        )
        self.assertEqual(
            value["chain"]["legacy_maintenance_evidence_bundle_sha256"],
            sha(self.fixture.bundle.read_bytes()),
        )

    def test_prearchive_accepts_exact_all_stopped_zero_sample_union(self) -> None:
        value, _digest = self.build_union_fixture(
            {name for name, _host in builder.FLEET}
        )
        bundle = json.loads(self.fixture.bundle.read_text())
        stability = bundle["quarantine_stability_proof"]["value"]
        self.assertEqual(stability["nodes"], [])
        self.assertEqual(stability["fleet_heads"], [])
        self.assertEqual(stability["active_transition_sha256s"], [])
        self.assertEqual(
            (
                stability["interval_seconds"],
                stability["sample_count"],
                stability["monotonic_elapsed_ns"],
            ),
            (0, 0, 0),
        )
        self.assertEqual(
            value["chain"]["legacy_maintenance_evidence_bundle_sha256"],
            sha(self.fixture.bundle.read_bytes()),
        )

    def test_private_input_stage_is_create_only_and_breaks_caller_path_replacement(self) -> None:
        args = self.fixture.args()
        original_binary = self.fixture.binary.read_bytes()
        staged, manifest_sha = builder.stage_prearchive_inputs(args)
        self.assertRegex(manifest_sha, r"^[0-9a-f]{64}$")
        self.assertEqual(staged.binary.read_bytes(), original_binary)
        self.assertEqual(stat.S_IMODE(staged.binary.stat().st_mode), 0o500)
        self.assertEqual(staged.binary.stat().st_nlink, 1)
        stage_manifest = json.loads(staged.stage_manifest.read_text())
        stage_rows = {row["name"]: row for row in stage_manifest["files"]}
        self.assertEqual(stage_rows["ssh_identity"]["sha256"], sha(self.fixture.ssh_identity.read_bytes()))
        self.assertIn("validator_vault_restore_receipt", stage_rows)
        self.assertIn("validator_key_install_receipt", stage_rows)
        for kind, platform in builder.PRETAG_GROUPS:
            self.assertIn(builder.pretag_artifact_key(kind, platform), stage_rows)
        self.fixture.binary.chmod(0o700)
        self.fixture.binary.write_bytes(b"#!/bin/sh\nexit 99\n")
        self.assertEqual(staged.binary.read_bytes(), original_binary)
        self.assertNotEqual(staged.binary.read_bytes(), self.fixture.binary.read_bytes())
        with self.assertRaisesRegex(builder.BuilderError, "already exists"):
            builder.stage_prearchive_inputs(args)

    def test_private_stage_copy_ignores_ctime_only_noise_but_rejects_security_identity_change(
        self,
    ) -> None:
        source = self.fixture.root / "stage-copy-source.bin"
        payload = b"stable-stage-copy-source\n"
        write(source, payload, 0o600)
        destination = self.fixture.root / "stage-copy-destination"
        destination.mkdir(mode=0o700)
        source_details = source.stat()
        real_fstat = builder.os.fstat

        def staged_fstat(changed_field: str):
            source_calls = 0

            def inspect(descriptor: int):
                nonlocal source_calls
                details = real_fstat(descriptor)
                if (
                    details.st_dev == source_details.st_dev
                    and details.st_ino == source_details.st_ino
                ):
                    source_calls += 1
                    if source_calls == 2:
                        fields = {
                            name: getattr(details, name)
                            for name in (
                                "st_dev", "st_ino", "st_mode", "st_uid", "st_gid",
                                "st_nlink", "st_size", "st_atime_ns", "st_mtime_ns",
                                "st_ctime_ns",
                            )
                        }
                        if changed_field == "ctime":
                            fields["st_ctime_ns"] += 1
                        elif changed_field == "mode":
                            fields["st_mode"] ^= stat.S_IWUSR
                        return types.SimpleNamespace(**fields)
                return details

            return inspect

        destination_fd = os.open(
            destination,
            os.O_RDONLY | getattr(os, "O_DIRECTORY", 0),
        )
        try:
            with mock.patch.object(
                builder.os, "fstat", side_effect=staged_fstat("ctime")
            ), mock.patch.object(
                builder.os, "lseek", wraps=builder.os.lseek
            ) as ctime_recheck:
                copied = builder._stage_copy(
                    source,
                    destination_fd,
                    "ctime-noise.bin",
                    label="ctime-noise private stage source",
                    maximum_bytes=1024,
                    mode=0o400,
                )
            ctime_recheck.assert_called_once()
            self.assertEqual(copied["sha256"], sha(payload))
            self.assertEqual((destination / "ctime-noise.bin").read_bytes(), payload)

            with mock.patch.object(
                builder.os, "fstat", side_effect=staged_fstat("mode")
            ):
                with self.assertRaisesRegex(
                    builder.BuilderError, "changed identity: mode"
                ):
                    builder._stage_copy(
                        source,
                        destination_fd,
                        "mode-change.bin",
                        label="mode-change private stage source",
                        maximum_bytes=1024,
                        mode=0o400,
                    )
        finally:
            os.close(destination_fd)

    def test_secure_readers_reprove_ctime_only_noise_and_reject_moving_identity(
        self,
    ) -> None:
        source = self.fixture.root / "secure-reader-source.bin"
        payload = b"stable-secure-reader-source\n"
        write(source, payload, 0o400)
        source_details = source.stat()
        real_fstat = builder.os.fstat

        def staged_fstat(
            ctime_deltas: tuple[int, ...], *, change_mode_on_call: int | None = None
        ):
            source_calls = 0

            def inspect(descriptor: int):
                nonlocal source_calls
                details = real_fstat(descriptor)
                if (
                    details.st_dev == source_details.st_dev
                    and details.st_ino == source_details.st_ino
                ):
                    source_calls += 1
                    fields = {
                        name: getattr(details, name)
                        for name in (
                            "st_dev",
                            "st_ino",
                            "st_mode",
                            "st_uid",
                            "st_gid",
                            "st_nlink",
                            "st_size",
                            "st_atime_ns",
                            "st_mtime_ns",
                            "st_ctime_ns",
                        )
                    }
                    delta_index = min(source_calls - 1, len(ctime_deltas) - 1)
                    fields["st_ctime_ns"] += ctime_deltas[delta_index]
                    if source_calls == change_mode_on_call:
                        fields["st_mode"] ^= stat.S_IWUSR
                    return types.SimpleNamespace(**fields)
                return details

            return inspect

        for operation in ("read", "hash"):
            with mock.patch.object(
                builder.os,
                "fstat",
                side_effect=staged_fstat((0, 1, 1)),
            ), mock.patch.object(
                builder.os, "lseek", wraps=builder.os.lseek
            ) as ctime_recheck:
                if operation == "read":
                    observed, _details = builder.read_secure(
                        source,
                        label="stable secure reader",
                        maximum_bytes=1024,
                        exact_mode=0o400,
                        require_read_only=True,
                    )
                    self.assertEqual(observed, payload)
                else:
                    observed_sha, observed_size = builder.hash_secure(
                        source, "stable secure hasher"
                    )
                    self.assertEqual(
                        (observed_sha, observed_size), (sha(payload), len(payload))
                    )
            ctime_recheck.assert_called_once()

        for operation in ("read", "hash"):
            with mock.patch.object(
                builder.os,
                "fstat",
                side_effect=staged_fstat((0, 0), change_mode_on_call=2),
            ):
                with self.assertRaisesRegex(
                    builder.BuilderError,
                    f"changed while it was {'read' if operation == 'read' else 'hashed'}",
                ):
                    if operation == "read":
                        builder.read_secure(
                            source,
                            label="mode-changing secure reader",
                            maximum_bytes=1024,
                        )
                    else:
                        builder.hash_secure(source, "mode-changing secure hasher")

        real_read = builder.os.read

        def changed_content_read():
            source_reads = 0

            def read(descriptor: int, maximum_bytes: int) -> bytes:
                nonlocal source_reads
                details = real_fstat(descriptor)
                chunk = real_read(descriptor, maximum_bytes)
                if (
                    details.st_dev == source_details.st_dev
                    and details.st_ino == source_details.st_ino
                ):
                    source_reads += 1
                    if source_reads == 3 and chunk:
                        return bytes([chunk[0] ^ 1]) + chunk[1:]
                return chunk

            return read

        for operation in ("read", "hash"):
            with mock.patch.object(
                builder.os,
                "fstat",
                side_effect=staged_fstat((0, 1, 1)),
            ), mock.patch.object(
                builder.os, "read", side_effect=changed_content_read()
            ):
                with self.assertRaisesRegex(
                    builder.BuilderError,
                    f"changed during its ctime-only {operation} recheck",
                ):
                    if operation == "read":
                        builder.read_secure(
                            source,
                            label="content-changing secure reader",
                            maximum_bytes=1024,
                        )
                    else:
                        builder.hash_secure(source, "content-changing secure hasher")

        for operation in ("read", "hash"):
            with mock.patch.object(
                builder.os,
                "fstat",
                side_effect=staged_fstat((0, 1, 2)),
            ):
                with self.assertRaisesRegex(
                    builder.BuilderError,
                    f"changed during its ctime-only {operation} recheck",
                ):
                    if operation == "read":
                        builder.read_secure(
                            source,
                            label="moving-ctime secure reader",
                            maximum_bytes=1024,
                        )
                    else:
                        builder.hash_secure(source, "moving-ctime secure hasher")

    def test_maintenance_boundary_is_standalone_exact_and_cutoff_complete(self) -> None:
        def move_created(value: dict) -> None:
            created = dt.datetime.strptime(value["created_at"], "%Y-%m-%dT%H:%M:%SZ")
            value["created_at"] = (created + dt.timedelta(seconds=1)).strftime(
                "%Y-%m-%dT%H:%M:%SZ"
            )

        self.fixture.rewrite_boundary(move_created, update_embedded=False)
        with self.assertRaisesRegex(
            builder.BuilderError,
            "standalone maintenance boundary|embedded maintenance boundary|"
            "late-fork source-set identity/policy differs",
        ):
            builder.prearchive(self.fixture.args())

        self.tearDown(); self.setUp()

        def raise_only_authenticated_height(value: dict) -> None:
            row = next(
                item
                for item in value["evidence_heights"]
                if item["node"] == "nyc" and item["label"] == "authenticated_latest"
            )
            row["height"] = 999_999
            value["observed_cutoff_height"] = 999_999
            value["legacy_public_max_height"] = 1_000_127

        self.fixture.rewrite_boundary(raise_only_authenticated_height)
        with self.assertRaisesRegex(
            builder.BuilderError,
            "evidence-height ledger differs from its retained proofs",
        ):
            builder.prearchive(self.fixture.args())

        self.tearDown(); self.setUp()
        self.fixture.rewrite_boundary(
            lambda value: value.__setitem__(
                "observed_cutoff_height", value["observed_cutoff_height"] - 1
            )
        )
        with self.assertRaisesRegex(builder.BuilderError, "maximum enumerated evidence height"):
            builder.prearchive(self.fixture.args())

        self.tearDown(); self.setUp()
        receipt = json.loads(self.fixture.offline_stop.read_text())
        receipt["schema"] = "arc.validator-vault.offline-stop-evidence.v1"
        payload = canonical(receipt)
        write(self.fixture.offline_stop, payload, 0o400)
        write(
            self.fixture.offline_stop.with_name(self.fixture.offline_stop.name + ".sha256"),
            f"{sha(payload)}  {self.fixture.offline_stop.name}\n".encode(),
            0o400,
        )
        with self.assertRaisesRegex(builder.BuilderError, "schema differs"):
            builder.prearchive(self.fixture.args())

    def test_post_freeze_builder_accepts_historical_receipt_at_exact_300_second_boundary(self) -> None:
        public_completed = (
            dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
            - dt.timedelta(hours=2)
        )
        self.fixture.write_height(completed_at=public_completed)
        self.fixture.write_offline_stop(
            fleet_started_at=public_completed + dt.timedelta(seconds=1),
            fleet_completed_at=public_completed + dt.timedelta(seconds=2),
            first_quarantine_at=public_completed + dt.timedelta(seconds=300),
            all_stopped_at=public_completed + dt.timedelta(seconds=301),
            boundary_created_at=public_completed + dt.timedelta(seconds=302),
        )
        self.fixture.rebuild_late_fork_source_set()
        self.fixture.write_validator_receipts()

        value, _digest = self.fixture.build()
        self.assertEqual(
            value["chain"]["legacy_public_max_height"], 137635
        )

    def test_post_freeze_builder_does_not_cross_order_remote_utc_at_301_seconds(self) -> None:
        public_completed = (
            dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
            - dt.timedelta(hours=2)
        )
        self.fixture.write_height(completed_at=public_completed)
        # Remote UTC is audit-only: the operator-authored dispatch and each
        # node's same-boot CLOCK_BOOTTIME lease are the mutation safety gate.
        self.fixture.write_offline_stop(
            fleet_started_at=public_completed + dt.timedelta(seconds=1),
            fleet_completed_at=public_completed + dt.timedelta(seconds=2),
            first_quarantine_at=public_completed + dt.timedelta(seconds=301),
            all_stopped_at=public_completed + dt.timedelta(seconds=302),
            boundary_created_at=public_completed + dt.timedelta(seconds=303),
        )
        self.fixture.rebuild_late_fork_source_set()
        self.fixture.write_validator_receipts()

        value, _digest = self.fixture.build()
        self.assertEqual(value["chain"]["legacy_public_max_height"], 137635)

    def test_post_freeze_builder_rejects_after_quarantine_and_reordered_brackets(self) -> None:
        base = (
            dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
            - dt.timedelta(hours=2)
        )
        public_completed = base + dt.timedelta(seconds=3)
        self.fixture.write_height(completed_at=public_completed)
        self.fixture.write_offline_stop(
            fleet_started_at=public_completed,
            fleet_completed_at=public_completed,
            first_quarantine_at=base + dt.timedelta(seconds=2),
            all_stopped_at=base + dt.timedelta(seconds=4),
            boundary_created_at=base + dt.timedelta(seconds=5),
        )
        self.fixture.rebuild_late_fork_source_set()
        with self.assertRaisesRegex(
            builder.BuilderError,
            "outside its authorized deadline|timeline is not ordered|"
            "nft apply intent was prepared after",
        ):
            builder.prearchive(self.fixture.args())

        self.tearDown(); self.setUp()
        self.fixture.write_height(completed_at=base)
        self.fixture.write_offline_stop(
            fleet_started_at=base + dt.timedelta(seconds=2),
            fleet_completed_at=base + dt.timedelta(seconds=1),
            first_quarantine_at=base + dt.timedelta(seconds=3),
            all_stopped_at=base + dt.timedelta(seconds=4),
            boundary_created_at=base + dt.timedelta(seconds=5),
        )
        self.fixture.rebuild_late_fork_source_set()
        with self.assertRaisesRegex(
            builder.BuilderError,
            "timestamps are reversed|completed before start",
        ):
            builder.prearchive(self.fixture.args())

    def test_capture_timeline_rejects_mismatched_receipt_binding_with_recomputed_wrapper(self) -> None:
        height = json.loads(self.fixture.height.read_text())
        bundle = json.loads(self.fixture.bundle.read_text())
        boundary = json.loads(self.fixture.boundary.read_text())
        offline = json.loads(self.fixture.offline_stop.read_text())
        cross = copy.deepcopy(offline["legacy_height_cross_proof"])
        cross["legacy_public_height_receipt_sha256"] = "f" * 64
        cross_sha = sha(canonical(cross))
        offline["legacy_height_cross_proof"] = copy.deepcopy(cross)
        bundle["authenticated_prefence_height_cross_proof"] = {
            "value": copy.deepcopy(cross),
            "sha256": cross_sha,
        }
        boundary["authenticated_prefence_height_cross_proof_sha256"] = cross_sha

        with self.assertRaisesRegex(builder.BuilderError, "capture timeline binding differs"):
            builder.validate_sealed_legacy_height_capture_timeline(
                args=self.fixture.args(),
                freeze_sha=self.fixture.freeze_sha,
                height_receipt=height,
                height_receipt_sha=sha(self.fixture.height.read_bytes()),
                evidence_bundle=bundle,
                boundary=boundary,
                offline_stop=offline,
            )

    def test_final_height_recheck_rejects_same_maximum_byte_substitution(self) -> None:
        original_side_effect = builder.execute_installed_key_verifier.side_effect

        def mutate_same_maximum(args, manifest):
            value = json.loads(args.legacy_public_height_receipt.read_text())
            value["origins"][0]["info_before_body_sha256"] = "e" * 64
            write(args.legacy_public_height_receipt, canonical(value), 0o400)
            return self.fixture.installed_key_proof(args, manifest)

        builder.execute_installed_key_verifier.side_effect = mutate_same_maximum
        try:
            with self.assertRaisesRegex(
                builder.BuilderError,
                "legacy public-height receipt changed before prearchive publication",
            ):
                builder.prearchive(self.fixture.args())
        finally:
            builder.execute_installed_key_verifier.side_effect = original_side_effect

    def test_prearchive_rejects_tampered_duplicate_symlink_and_wrong_commit(self) -> None:
        metadata = json.loads(self.fixture.build_metadata.read_text())
        metadata["commit"] = "7" * 40
        write(self.fixture.build_metadata, canonical(metadata), 0o444)
        with self.assertRaisesRegex(builder.BuilderError, "metadata commit differs"):
            builder.prearchive(self.fixture.args())

        self.tearDown(); self.setUp()
        payload = self.fixture.public_keys.read_text()
        self.fixture.public_keys.chmod(0o600)
        self.fixture.public_keys.write_text(payload.replace('"address":', '"address":"0","address":', 1))
        self.fixture.public_keys.chmod(0o444)
        with self.assertRaisesRegex(builder.BuilderError, "duplicate JSON key"):
            builder.prearchive(self.fixture.args())

        self.tearDown(); self.setUp()
        target = self.fixture.root / "real-checkpoint"
        self.fixture.checkpoint.rename(target)
        self.fixture.checkpoint.symlink_to(target)
        with self.assertRaisesRegex(builder.BuilderError, "securely|symlink|Too many"):
            builder.prearchive(self.fixture.args())

        self.tearDown(); self.setUp()
        receipt = json.loads(self.fixture.offline_stop.read_text())
        receipt["nodes"][0]["stop_complete_sha256"] = "f" * 64
        frozen = json.loads(self.fixture.freeze.read_text())["nodes"][0]
        forged_status = {
            "capture_id": receipt["capture_id"],
            "freeze_plan_sha256": receipt["freeze_plan_sha256"],
            "node": "nyc",
            "restart_fenced": True,
            "schema": "arc.recovery.offline-stop-status.v1",
            "stake": frozen["stake"],
            "stop_complete_sha256": "f" * 64,
            "stop_files_sha256": receipt["nodes"][0]["stop_files_sha256"],
            "stop_schema": "arc.recovery.offline-stop.v4",
            "stopped": True,
            "validator_address": frozen["validator_address"],
        }
        receipt["nodes"][0]["stopped_status_sha256"] = sha(canonical(forged_status))
        payload = canonical(receipt)
        write(self.fixture.offline_stop, payload, 0o400)
        write(
            self.fixture.offline_stop.with_name(self.fixture.offline_stop.name + ".sha256"),
            f"{sha(payload)}  {self.fixture.offline_stop.name}\n".encode(),
            0o400,
        )
        with self.assertRaisesRegex(
            builder.BuilderError,
            "offline-stop evidence nyc status differs from the evidence bundle|fresh offline-stop nyc status differs",
        ):
            builder.prearchive(self.fixture.args())

    def test_prearchive_requires_one_ordered_unique_all_nine_artifact_input_set(self) -> None:
        value = json.loads(self.fixture.pretag_artifact_input_set.read_text())
        value["artifacts"].pop()
        write(self.fixture.pretag_artifact_input_set, canonical(value), 0o400)
        with self.assertRaisesRegex(builder.BuilderError, "exactly nine"):
            builder.prearchive(self.fixture.args())

        self.tearDown(); self.setUp()
        value = json.loads(self.fixture.pretag_artifact_input_set.read_text())
        value["artifacts"][0], value["artifacts"][1] = value["artifacts"][1], value["artifacts"][0]
        write(self.fixture.pretag_artifact_input_set, canonical(value), 0o400)
        with self.assertRaisesRegex(builder.BuilderError, "out of order"):
            builder.prearchive(self.fixture.args())

        self.tearDown(); self.setUp()
        value = json.loads(self.fixture.pretag_artifact_input_set.read_text())
        value["artifacts"][1]["artifact_id"] = value["artifacts"][0]["artifact_id"]
        write(self.fixture.pretag_artifact_input_set, canonical(value), 0o400)
        with self.assertRaisesRegex(builder.BuilderError, "IDs must be unique"):
            builder.prearchive(self.fixture.args())

    def identity_window_fixture(self, name: str) -> tuple[dict, pathlib.Path, pathlib.Path]:
        stage_root = self.fixture.root / name
        stage_root.mkdir(mode=0o700)
        source = stage_root / "source"
        private = stage_root / "private"
        source.mkdir(mode=0o700)
        private.mkdir(mode=0o700)
        staged_file = source / "artifact.bin"
        payload = b"stable-artifact-bytes\n"
        write(staged_file, payload, 0o400)
        stage_value = {
            "schema": "arc.recovery.production-input-stage.v1",
            "source_main_commit": self.fixture.commit,
            "files": [
                {
                    "name": "fixture_artifact",
                    "path": "source/artifact.bin",
                    "sha256": sha(payload),
                    "size_bytes": len(payload),
                    "mode": "0400",
                }
            ],
        }
        stage_payload = canonical(stage_value)
        stage_manifest = stage_root / "STAGE-MANIFEST.json"
        write(stage_manifest, stage_payload, 0o400)
        source.chmod(0o500)
        private.chmod(0o500)
        stage_root.chmod(0o500)
        manifest = {
            "artifacts": {
                "production_input_stage_manifest": {
                    "path": str(stage_manifest),
                    "sha256": sha(stage_payload),
                }
            }
        }
        return manifest, staged_file, stage_manifest

    def test_final_identity_window_is_deterministic_under_ctime_only_noise(self) -> None:
        # APFS can publish a delayed ctime after create/chmod.  Cross-window
        # safety is the held no-follow descriptor plus stable identity fields;
        # ctime is checked only in the post-hash descriptor/path snapshot.
        for index in range(20):
            manifest, staged_file, stage_manifest = self.identity_window_fixture(
                f"identity-stable-{index}"
            )
            with builder.stable_artifact_identity_window(manifest) as recheck:
                staged_file.chmod(0o400)
                stage_manifest.chmod(0o400)
                recheck()

    def test_final_identity_window_rejects_mode_and_path_swap_mutations(self) -> None:
        manifest, staged_file, _stage_manifest = self.identity_window_fixture(
            "identity-mode-mutation"
        )
        with self.assertRaisesRegex(builder.BuilderError, "changed during the final live reproof"):
            with builder.stable_artifact_identity_window(manifest) as recheck:
                staged_file.chmod(0o600)
                recheck()

        manifest, staged_file, _stage_manifest = self.identity_window_fixture(
            "identity-path-swap"
        )
        with self.assertRaisesRegex(builder.BuilderError, "changed during the final live reproof"):
            with builder.stable_artifact_identity_window(manifest) as recheck:
                source = staged_file.parent
                source.chmod(0o700)
                staged_file.rename(source / "artifact.original")
                write(staged_file, b"stable-artifact-bytes\n", 0o400)
                source.chmod(0o500)
                recheck()

    def test_final_live_reproof_holds_every_staged_file_and_directory_identity(self) -> None:
        def mutate_after_live_reproof(**kwargs):
            result = self.fixture.final_live_set_reproof(**kwargs)
            assert self.fixture.last_stage_root is not None
            binary = self.fixture.last_stage_root / "arc-node-linux-x86_64"
            original = binary.read_bytes()
            binary.chmod(0o700)
            binary.write_bytes(original[:-1] + bytes([original[-1] ^ 1]))
            binary.chmod(0o500)
            return result

        with mock.patch.object(
            builder.protected_pretag,
            "final_live_set_reproof",
            side_effect=mutate_after_live_reproof,
        ):
            with self.assertRaisesRegex(builder.BuilderError, "changed during the final live reproof"):
                builder.prearchive(self.fixture.args())
        self.assertFalse(self.fixture.output.exists())

    def test_fabricated_local_stop_receipt_cannot_authorize_without_live_hosts(self) -> None:
        with mock.patch.object(
            builder,
            "execute_remote_stop_verifier",
            side_effect=builder.BuilderError("trusted remote verifier unavailable"),
        ):
            with self.assertRaisesRegex(builder.BuilderError, "remote verifier unavailable"):
                builder.prearchive(self.fixture.args())

    def test_installed_key_proof_rejects_replay_swaps_partial_and_forged_tools(self) -> None:
        def rejected(mutator, message):
            def response(args, manifest):
                value = self.fixture.installed_key_proof(args, manifest)
                mutator(value, manifest)
                return value

            with mock.patch.object(
                builder, "execute_installed_key_verifier", side_effect=response
            ):
                with self.assertRaisesRegex(builder.BuilderError, message):
                    builder.prearchive(self.fixture.args())

        rejected(
            lambda value, manifest: value.__setitem__(
                "challenge",
                manifest["provenance"]["offline_stop_verification"]["challenge"],
            ),
            "fresh challenge",
        )
        rejected(
            lambda value, _manifest: value["validators"][0].__setitem__(
                "host", "192.0.2.99"
            ),
            "fleet/key/response mapping differs",
        )
        rejected(
            lambda value, _manifest: value["validators"][0].__setitem__(
                "keyfile_sha256", value["validators"][1]["keyfile_sha256"]
            ),
            "fleet/key/response mapping differs",
        )
        rejected(
            lambda value, _manifest: value["validators"].pop(),
            "exactly six rows",
        )
        rejected(
            lambda value, _manifest: value.__setitem__("ssh_sha256", "e" * 64),
            "stage/receipt/transport tuple",
        )
        rejected(
            lambda value, _manifest: value["validators"][1].__setitem__(
                "remote_response_sha256",
                value["validators"][0]["remote_response_sha256"],
            ),
            "fleet/key/response mapping differs",
        )

    def test_remote_stop_verification_rejects_replay_partial_wrong_host_and_duplicate_key(self) -> None:
        def reset_fixture() -> None:
            # Every failed prearchive attempt intentionally leaves its private,
            # content-addressed input stage behind.  Adversarial cases must use
            # fresh source bytes and a fresh stage so one rejection cannot mask
            # the next case with the stage-identity guard.
            self.tearDown()
            self.setUp()

        def rejected(mutator, message):
            reset_fixture()

            def response(*args, **kwargs):
                value = self.fixture.remote_verification(*args, **kwargs)
                mutator(value)
                return value

            with mock.patch.object(builder, "execute_remote_stop_verifier", side_effect=response):
                with self.assertRaisesRegex(builder.BuilderError, message):
                    builder.prearchive(self.fixture.args())

        rejected(
            lambda value: value.update(
                {
                    "started_at": (
                        dt.datetime.now(dt.timezone.utc) - dt.timedelta(seconds=301)
                    ).replace(microsecond=0).strftime("%Y-%m-%dT%H:%M:%SZ"),
                    "completed_at": (
                        dt.datetime.now(dt.timezone.utc) - dt.timedelta(seconds=301)
                    ).replace(microsecond=0).strftime("%Y-%m-%dT%H:%M:%SZ"),
                }
            ),
            "stale",
        )
        rejected(lambda value: value.__setitem__("challenge", "0" * 64), "challenge differs")
        rejected(lambda value: value["nodes"].pop(), "all six ordered hosts")
        rejected(
            lambda value: value["nodes"][0].__setitem__("host", "192.0.2.99"),
            "topology differs",
        )

        reset_fixture()
        original_known_hosts = self.fixture.known_hosts.read_bytes()
        lines = original_known_hosts.decode("ascii").splitlines()
        fields = lines[1].split(" ")
        fields[2] = lines[0].split(" ")[2]
        lines[1] = " ".join(fields)
        write(self.fixture.known_hosts, ("\n".join(lines) + "\n").encode(), 0o400)
        with self.assertRaisesRegex(builder.BuilderError, "repeats an Ed25519 host key"):
            builder.prearchive(self.fixture.args())

        reset_fixture()
        original_known_hosts = self.fixture.known_hosts.read_bytes()
        wrong_ip = original_known_hosts.replace(b"149.28.32.76 ", b"192.0.2.99 ", 1)
        write(self.fixture.known_hosts, wrong_ip, 0o400)
        with self.assertRaisesRegex(builder.BuilderError, "nyc address differs"):
            builder.prearchive(self.fixture.args())

        reset_fixture()
        original_known_hosts = self.fixture.known_hosts.read_bytes()
        malformed = original_known_hosts.decode("ascii").splitlines()
        malformed[0] = " ".join((*malformed[0].split(" ")[:2], "not-base64!"))
        write(self.fixture.known_hosts, ("\n".join(malformed) + "\n").encode(), 0o400)
        with self.assertRaisesRegex(builder.BuilderError, "not canonical base64"):
            builder.prearchive(self.fixture.args())

        reset_fixture()
        real_known_hosts = self.fixture.root / "real-known-hosts"
        self.fixture.known_hosts.rename(real_known_hosts)
        self.fixture.known_hosts.symlink_to(real_known_hosts)
        with self.assertRaisesRegex(builder.BuilderError, "securely|symlink|Too many"):
            builder.prearchive(self.fixture.args())

    def test_remote_verifier_uses_closed_environment_and_rejects_multiple_output(self) -> None:
        verifier_patch = self.patches[-5]
        verifier_patch.stop()
        calls = []

        def fake_run(command, **kwargs):
            calls.append((command, kwargs))
            return builder.subprocess.CompletedProcess(command, 0, "{}\n{}\n", "")

        try:
            with mock.patch.object(builder, "_system_bash", return_value=(pathlib.Path("/usr/bin/bash"), "b" * 64)), \
                    mock.patch.object(builder, "validate_root_system_tool", return_value="a" * 64), \
                    mock.patch.object(builder.subprocess, "run", side_effect=fake_run):
                with self.assertRaisesRegex(builder.BuilderError, "one JSON object|multiple output"):
                    builder.execute_remote_stop_verifier(
                        self.fixture.args(),
                        self.fixture.freeze_sha,
                        sha(self.fixture.offline_stop.read_bytes()),
                        sha(self.fixture.known_hosts.read_bytes()),
                        "9" * 64,
                        python_path=pathlib.Path("/usr/bin/python3"),
                        python_sha="c" * 64,
                        ssh_sha="a" * 64,
                        freeze_payload=self.fixture.freeze.read_bytes(),
                        freeze_sidecar=self.fixture.freeze.with_name(self.fixture.freeze.name + ".sha256").read_bytes(),
                        evidence_payload=self.fixture.offline_stop.read_bytes(),
                        evidence_sidecar=self.fixture.offline_stop.with_name(self.fixture.offline_stop.name + ".sha256").read_bytes(),
                        known_hosts_payload=self.fixture.known_hosts.read_bytes(),
                        identity_payload=self.fixture.ssh_identity.read_bytes(),
                    )
            self.assertEqual(calls[0][0][0], "/usr/bin/bash")
            self.assertIn("--python-path", calls[0][0])
            self.assertIn("/usr/bin/python3", calls[0][0])
            self.assertIn("--python-sha256", calls[0][0])
            self.assertIn("c" * 64, calls[0][0])
            self.assertIn("--ssh-sha256", calls[0][0])
            self.assertEqual(calls[0][1]["env"]["PATH"], "/usr/bin:/bin")
            self.assertNotIn("SSH_AUTH_SOCK", calls[0][1]["env"])
            self.assertNotIn("PYTHONPATH", calls[0][1]["env"])
            self.assertTrue(calls[0][1]["close_fds"])
            self.assertTrue(calls[0][1]["start_new_session"])
        finally:
            verifier_patch.start()

    def test_installed_key_verifier_uses_provisional_seal_and_closed_environment(self) -> None:
        final, _digest = self.fixture.build()
        provisional = copy.deepcopy(final)
        provisional["provenance"].pop("validator_installed_key_proof")
        stage_root = pathlib.Path(
            provisional["artifacts"]["production_input_stage_manifest"]["path"]
        ).parent
        args = argparse.Namespace(
            freeze_plan=stage_root / self.fixture.freeze.name,
            cli=pathlib.Path(provisional["artifacts"]["cli"]["path"]),
            validator_public_keys=pathlib.Path(
                provisional["artifacts"]["validator_public_keys"]["path"]
            ),
            validator_key_install_receipt=pathlib.Path(
                provisional["artifacts"]["validator_key_install_receipt"]["path"]
            ),
            validator_vault_restore_receipt=pathlib.Path(
                provisional["artifacts"]["validator_vault_restore_receipt"]["path"]
            ),
            ssh_known_hosts=pathlib.Path(
                provisional["artifacts"]["ssh_known_hosts"]["path"]
            ),
            ssh_identity=stage_root / "private" / "id_ed25519",
        )
        installed_patch = self.patches[-2]
        installed_patch.stop()
        calls = []

        def fake_run(command, **kwargs):
            calls.append((command, kwargs))
            manifest_index = command.index("--manifest") + 1
            sealed = pathlib.Path(command[manifest_index])
            self.assertTrue(sealed.is_file())
            self.assertTrue(sealed.with_name(sealed.name + ".sha256").is_file())
            self.assertFalse(sealed.stat().st_mode & 0o222)
            challenge = command[command.index("--challenge") + 1]
            proof = self.fixture.installed_key_proof(args, provisional)
            proof["challenge"] = challenge
            return builder.subprocess.CompletedProcess(
                command, 0, canonical(proof).decode(), "six-host proof PASS\n"
            )

        chain = provisional["provenance"]["validator_key_receipt_chain"]

        def system_hash(path, _label, **_kwargs):
            if path == builder.SYSTEM_SSH:
                return chain["ssh_sha256"]
            if path == builder.SYSTEM_SCP:
                return chain["scp_sha256"]
            raise AssertionError(f"unexpected system tool: {path}")

        try:
            with mock.patch.object(
                builder, "_system_python", return_value=(pathlib.Path("/usr/bin/python3"), "c" * 64)
            ), mock.patch.object(
                builder, "_system_bash", return_value=(pathlib.Path("/usr/bin/bash"), "b" * 64)
            ), mock.patch.object(
                builder, "validate_root_system_tool", side_effect=system_hash
            ), mock.patch.object(builder.subprocess, "run", side_effect=fake_run):
                proof = builder.execute_installed_key_verifier(args, provisional)
            self.assertEqual(proof["schema"], "arc.recovery.validator-installed-key-proof.v1")
            command, kwargs = calls[0]
            self.assertEqual(command[0], "/usr/bin/bash")
            self.assertIn("verify-installed-keys", command)
            self.assertEqual(kwargs["env"]["PATH"], "/usr/bin:/bin")
            self.assertEqual(kwargs["env"]["ARC_RECOVERY_SSH_USER"], "root")
            self.assertEqual(
                kwargs["env"]["ARC_RECOVERY_SSH_IDENTITY"], os.fspath(args.ssh_identity)
            )
            self.assertNotIn("SSH_AUTH_SOCK", kwargs["env"])
            self.assertNotIn("PYTHONPATH", kwargs["env"])
            self.assertNotIn("GH_TOKEN", kwargs["env"])
            self.assertTrue(kwargs["close_fds"])
            self.assertTrue(kwargs["start_new_session"])
        finally:
            installed_patch.start()

    def test_system_python_is_portable_hash_bound_and_never_uses_platform_stat(self) -> None:
        with mock.patch.object(
            builder.subprocess,
            "run",
            side_effect=AssertionError("system Python validation must not shell out to stat/readlink"),
        ):
            path, digest = builder._system_python()
        self.assertEqual(path.parent, pathlib.Path("/usr/bin"))
        self.assertRegex(path.name, r"^python3(?:\.[0-9]+)?$")
        self.assertFalse(path.is_symlink())
        self.assertEqual(digest, sha(path.read_bytes()))
        self.assertEqual(path.stat().st_uid, 0)
        self.assertFalse(path.stat().st_mode & 0o022)
        self.assertGreaterEqual(path.stat().st_nlink, 1)
        if sys.platform == "darwin":
            # Apple's signed system Python is intentionally one of many hard links.
            self.assertGreater(path.stat().st_nlink, 1)
            bsd_stat = builder.subprocess.run(
                ["/usr/bin/stat", "-c", "%u:%h:%a", os.fspath(path)],
                stdin=builder.subprocess.DEVNULL,
                stdout=builder.subprocess.PIPE,
                stderr=builder.subprocess.PIPE,
                check=False,
            )
            self.assertNotEqual(bsd_stat.returncode, 0)

        # Debian/Ubuntu's protected /usr/bin/python3 -> python3.N shape is the
        # second explicitly supported production layout.
        linux_stats = {
            "/usr/bin/python3": types.SimpleNamespace(
                st_mode=stat.S_IFLNK | 0o777, st_uid=0
            ),
            "/usr/bin/python3.12": types.SimpleNamespace(
                st_mode=stat.S_IFREG | 0o755, st_uid=0
            ),
        }
        resolved = builder._resolve_system_python_entrypoint(
            lstat_fn=lambda raw: linux_stats[raw],
            readlink_fn=lambda raw: "python3.12",
        )
        self.assertEqual(resolved, pathlib.Path("/usr/bin/python3.12"))
        with self.assertRaisesRegex(builder.BuilderError, "outside protected /usr/bin"):
            builder._resolve_system_python_entrypoint(
                lstat_fn=lambda raw: linux_stats["/usr/bin/python3"],
                readlink_fn=lambda raw: "/tmp/python3.12",
            )

    def test_prearchive_rejects_checkpoint_verify_topology_model_genesis_and_caddy(self) -> None:
        write(self.fixture.root / "verify-fail", b"1", 0o400)
        with self.assertRaisesRegex(builder.BuilderError, "checkpoint|failed"):
            builder.prearchive(self.fixture.args())

        self.tearDown(); self.setUp()
        self.fixture.freeze_sha = self.fixture.write_freeze(
            lambda value: value["nodes"][0].__setitem__("host", "192.0.2.99")
        )
        self.fixture.write_height()
        with self.assertRaisesRegex(builder.BuilderError, "topology"):
            builder.prearchive(self.fixture.args())

        self.tearDown(); self.setUp()
        self.fixture.freeze_sha = self.fixture.write_freeze(
            lambda value: value["nodes"][0].__setitem__("model_sha256", "f" * 64)
        )
        self.fixture.write_height()
        with self.assertRaisesRegex(builder.BuilderError, "model or shard"):
            builder.prearchive(self.fixture.args())

        self.tearDown(); self.setUp()
        public = json.loads(self.fixture.public_keys.read_text())
        public[0]["address"] = "f" * 64
        write(self.fixture.public_keys, canonical(public), 0o444)
        with self.assertRaisesRegex(builder.BuilderError, "address/stake"):
            builder.prearchive(self.fixture.args())

        self.tearDown(); self.setUp()
        self.fixture.genesis.chmod(0o600)
        with self.fixture.genesis.open("ab") as handle:
            handle.write(b"\n# tampered genesis\n")
        self.fixture.genesis.chmod(0o444)
        with self.assertRaisesRegex(builder.BuilderError, "genesis.toml differs"):
            builder.prearchive(self.fixture.args())

        self.tearDown(); self.setUp()
        alternate_probe = self.fixture.root / "alternate-reward-probe.py"
        write(alternate_probe, self.fixture.reward_probe.read_bytes(), 0o555)
        probe_args = self.fixture.args()
        probe_args.reward_probe = alternate_probe
        with self.assertRaisesRegex(builder.BuilderError, "exact protected-main recovery probe"):
            builder.prearchive(probe_args)

        self.tearDown(); self.setUp()
        with mock.patch.object(builder.rollout, "CADDY_LINUX_AMD64_SHA256", "f" * 64):
            with self.assertRaisesRegex(builder.BuilderError, "Caddy"):
                builder.prearchive(self.fixture.args())

    def test_finalizer_changes_only_four_roots_and_reuses_archive_validator(self) -> None:
        prearchive, prearchive_sha = self.fixture.build()
        args = self.fixture.archive_evidence(prearchive, prearchive_sha)
        final_sha = builder.finalize(args)
        final = json.loads(args.output.read_text())
        self.assertEqual(final_sha, sha(args.output.read_bytes()))
        projected = copy.deepcopy(final)
        for field in builder.rollout.ARCHIVE_FINALIZATION_FIELDS:
            projected["archive"][field] = builder.ZERO_HASH
        self.assertEqual(canonical(projected), self.fixture.output.read_bytes())
        self.assertEqual(final["archive"]["prearchive_rollout_sha256"], prearchive_sha)
        self.assertTrue(all(final["archive"][field] != builder.ZERO_HASH for field in builder.rollout.ARCHIVE_FINALIZATION_FIELDS))
        with self.assertRaisesRegex(builder.BuilderError, "already exists"):
            builder.finalize(args)

    def test_finalizer_requires_exact_execute_drive_archive_seal_receipt(self) -> None:
        prearchive, prearchive_sha = self.fixture.build()

        def rejected(mutator, message: str) -> None:
            args = self.fixture.archive_evidence(prearchive, prearchive_sha)
            value = json.loads(args.drive_archive_seal_prefreeze.read_text())
            mutator(value)
            write(
                args.drive_archive_seal_prefreeze,
                canonical(value),
                0o400,
            )
            with self.assertRaisesRegex(builder.BuilderError, message):
                builder.finalize(args)

        rejected(
            lambda value: value.__setitem__("mode", "preflight"),
            "mode differs",
        )
        rejected(
            lambda value: value.__setitem__("canary_deleted", False),
            "canary_deleted differs",
        )
        rejected(
            lambda value: value.__setitem__("permission_id_sha256", "0" * 64),
            "permission identity must be nonzero",
        )
        rejected(
            lambda value: value.__setitem__(
                "available_bytes_after",
                value["archive_reservation_bytes"] - 1,
            ),
            "capacity is below",
        )
        rejected(
            lambda value: value.__setitem__("client_id_sha256", "e" * 64),
            "client_id_sha256 differs",
        )

        missing = self.fixture.archive_evidence(prearchive, prearchive_sha)
        missing.drive_archive_seal_prefreeze.unlink()
        with self.assertRaisesRegex(builder.BuilderError, "securely read|No such"):
            builder.finalize(missing)

        linked = self.fixture.archive_evidence(prearchive, prearchive_sha)
        target = linked.drive_archive_seal_prefreeze.with_name("real-drive.json")
        linked.drive_archive_seal_prefreeze.rename(target)
        linked.drive_archive_seal_prefreeze.symlink_to(target)
        with self.assertRaisesRegex(
            builder.BuilderError, "securely read|symlink|Too many"
        ):
            builder.finalize(linked)

    def test_finalizer_requires_fresh_drive_attempt_binding(self) -> None:
        prearchive, prearchive_sha = self.fixture.build()

        def rejected(mutator, message: str) -> None:
            args = self.fixture.archive_evidence(prearchive, prearchive_sha)
            value = json.loads(args.drive_archive_seal_attempt.read_text())
            mutator(value)
            write(args.drive_archive_seal_attempt, canonical(value), 0o400)
            with self.assertRaisesRegex(builder.BuilderError, message):
                builder.finalize(args)

        rejected(
            lambda value: value.__setitem__(
                "drive_prefreeze_receipt_sha256", "f" * 64
            ),
            "exact fresh execute receipt",
        )
        rejected(
            lambda value: value.__setitem__("attempt_nonce", "0" * 64),
            "nonce must be nonzero",
        )
        rejected(
            lambda value: value.__setitem__(
                "completed_at_unix_ns", value["started_at_unix_ns"] - 1
            ),
            "interval is reversed",
        )
        rejected(
            lambda value: value.__setitem__(
                "selected_immediately_before_first_archive_upload", False
            ),
            "exact fresh execute receipt",
        )
        rejected(
            lambda value: value.__setitem__("rclone_path", "rclone"),
            "not canonical absolute",
        )

        missing = self.fixture.archive_evidence(prearchive, prearchive_sha)
        missing.drive_archive_seal_attempt.unlink()
        with self.assertRaisesRegex(builder.BuilderError, "securely read|No such"):
            builder.finalize(missing)

    def test_finalizer_requires_exact_github_gist_write_canary(self) -> None:
        prearchive, prearchive_sha = self.fixture.build()

        def rejected(mutator, message: str) -> None:
            args = self.fixture.archive_evidence(prearchive, prearchive_sha)
            value = json.loads(args.github_gist_write_canary.read_text())
            mutator(value)
            write(args.github_gist_write_canary, canonical(value), 0o400)
            with self.assertRaisesRegex(builder.BuilderError, message):
                builder.finalize(args)

        rejected(
            lambda value: value.__setitem__(
                "schema", "arc.recovery.github-gist-write-canary.v0"
            ),
            "schema differs",
        )
        rejected(
            lambda value: value.__setitem__("owner_login", "someone-else"),
            "owner_login differs",
        )
        rejected(
            lambda value: value.__setitem__("freeze_plan_sha256", "f" * 64),
            "freeze_plan_sha256 differs",
        )
        rejected(
            lambda value: value.__setitem__("capture_id", "f" * 64),
            "capture_id differs",
        )
        rejected(
            lambda value: value.__setitem__("revision_read_verified", False),
            "revision_read_verified differs",
        )
        rejected(
            lambda value: value.__setitem__("challenge", "0" * 64),
            "challenge must be nonzero",
        )
        rejected(
            lambda value: value.__setitem__("gist_id", "D" * 32),
            "id is malformed",
        )
        rejected(
            lambda value: value.__setitem__("gist_revision", "e" * 39),
            "revision is malformed",
        )
        rejected(
            lambda value: value.__setitem__("gist_filename", "wrong.txt"),
            "filename differs",
        )
        rejected(
            lambda value: value.__setitem__("gist_content_sha256", "f" * 64),
            "content hash differs",
        )
        rejected(
            lambda value: value.__setitem__("github_cli_sha256", "f" * 64),
            "CLI hash differs",
        )
        rejected(
            lambda value: value.__setitem__("github_cli_path", "relative/gh"),
            "CLI path is not a normalized real path",
        )
        rejected(
            lambda value: value.__setitem__(
                "completed_at", "2999-01-01T00:00:00Z"
            ),
            "completion is in the future",
        )
        rejected(
            lambda value: value.__setitem__("unexpected", True),
            "fields differ",
        )

        missing = self.fixture.archive_evidence(prearchive, prearchive_sha)
        missing.github_gist_write_canary.unlink()
        with self.assertRaisesRegex(builder.BuilderError, "securely read|No such"):
            builder.finalize(missing)

        linked = self.fixture.archive_evidence(prearchive, prearchive_sha)
        target = linked.github_gist_write_canary.with_name("real-gist-canary.json")
        linked.github_gist_write_canary.rename(target)
        linked.github_gist_write_canary.symlink_to(target)
        with self.assertRaisesRegex(
            builder.BuilderError, "securely read|symlink|Too many"
        ):
            builder.finalize(linked)

    def test_finalizer_requires_exact_independent_gist_anchor(self) -> None:
        prearchive, prearchive_sha = self.fixture.build()

        def rejected(mutator, message: str) -> None:
            args = self.fixture.archive_evidence(prearchive, prearchive_sha)
            value = json.loads(args.complete.read_text())
            mutator(value)
            payload = canonical(value)
            write(args.complete, payload, 0o400)
            args.complete_sha256 = sha(payload)
            with self.assertRaisesRegex(builder.BuilderError, message):
                builder.finalize(args)

        rejected(
            lambda value: value.__setitem__(
                "schema", "arc.recovery.archive-complete.v1"
            ),
            "schema is unsupported",
        )
        rejected(
            lambda value: value.pop("finalization_anchor"),
            "COMPLETE.json fields differ",
        )
        rejected(
            lambda value: value["finalization_anchor"].__setitem__(
                "gist_file_sha256", "8" * 64
            ),
            "Gist file hash differs",
        )
        rejected(
            lambda value: value["finalization_anchor"].__setitem__(
                "intent_sha256", "0" * 64
            ),
            "intent sha256 must be nonzero",
        )
        rejected(
            lambda value: value["finalization_anchor"].__setitem__(
                "gist_id", "A" * 32
            ),
            "Gist id is malformed",
        )
        rejected(
            lambda value: value["finalization_anchor"].__setitem__(
                "gist_revision", "b" * 39
            ),
            "Gist revision is malformed",
        )
        rejected(
            lambda value: value["finalization_anchor"].__setitem__("extra", True),
            "finalization_anchor fields differ",
        )

    def test_finalizer_rejects_wrong_roots_tamper_duplicate_missing_symlink_and_projection_binding(self) -> None:
        prearchive, prearchive_sha = self.fixture.build()
        args = self.fixture.archive_evidence(prearchive, prearchive_sha)
        args.complete_sha256 = "f" * 64
        with self.assertRaisesRegex(builder.BuilderError, "trust root"):
            builder.finalize(args)

        args = self.fixture.archive_evidence(prearchive, prearchive_sha)
        manifest = json.loads(args.archive_manifest.read_text())
        manifest["source_commit"] = "7" * 40
        args.archive_manifest.chmod(0o600)
        payload = canonical(manifest)
        write(args.archive_manifest, payload, 0o400)
        args.archive_manifest_sha256 = sha(payload)
        write(args.archive_manifest_sidecar, f"{args.archive_manifest_sha256}  ARCHIVE-MANIFEST.json\n".encode(), 0o400)
        with self.assertRaisesRegex(builder.BuilderError, "source_commit"):
            builder.finalize(args)

        self.tearDown(); self.setUp()
        prearchive, prearchive_sha = self.fixture.build()
        args = self.fixture.archive_evidence(prearchive, prearchive_sha)
        args.sha256sums.chmod(0o600)
        with args.sha256sums.open("ab") as handle:
            handle.write(args.sha256sums.read_bytes().splitlines(keepends=True)[0])
        args.sha256sums.chmod(0o400)
        args.sha256sums_sha256 = sha(args.sha256sums.read_bytes())
        with self.assertRaisesRegex(builder.BuilderError, "duplicate"):
            builder.finalize(args)

        self.tearDown(); self.setUp()
        prearchive, prearchive_sha = self.fixture.build()
        args = self.fixture.archive_evidence(prearchive, prearchive_sha)
        args.complete.unlink()
        with self.assertRaisesRegex(builder.BuilderError, "securely read|No such"):
            builder.finalize(args)

        self.tearDown(); self.setUp()
        prearchive, prearchive_sha = self.fixture.build()
        args = self.fixture.archive_evidence(prearchive, prearchive_sha)
        target = args.archive_manifest.with_name("real-manifest.json")
        args.archive_manifest.rename(target)
        args.archive_manifest.symlink_to(target)
        with self.assertRaisesRegex(builder.BuilderError, "securely read|symlink|Too many"):
            builder.finalize(args)

        self.tearDown(); self.setUp()
        prearchive, prearchive_sha = self.fixture.build()
        args = self.fixture.archive_evidence(prearchive, prearchive_sha)
        with mock.patch.object(
            builder.rollout, "prearchive_projection_digest", return_value="f" * 64
        ):
            with self.assertRaisesRegex(builder.BuilderError, "prearchive manifest|project"):
                builder.finalize(args)


if __name__ == "__main__":
    unittest.main(verbosity=2)
