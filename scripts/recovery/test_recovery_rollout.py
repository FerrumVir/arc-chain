#!/usr/bin/env python3
from __future__ import annotations

import copy
import datetime as dt
import hashlib
import importlib.util
import io
import json
import os
import socket
import stat
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock


MODULE_PATH = Path(__file__).with_name("recovery_rollout.py")
SPEC = importlib.util.spec_from_file_location("recovery_rollout", MODULE_PATH)
assert SPEC and SPEC.loader
rollout = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = rollout
SPEC.loader.exec_module(rollout)
quarantine_rounds = rollout.quarantine_rounds
PRODUCTION_CADDY_SHA256 = rollout.CADDY_LINUX_AMD64_SHA256


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class ManifestFixture:
    def __init__(self, root: Path, *, production: bool = False, reward_receipt: bool = False) -> None:
        self.root = root
        artifact_names = ["binary", "genesis", "checkpoint", "legacy_validator_set"] + (
            [
                "cli",
                "build_metadata",
                "pretag_artifact_input_set",
                "pretag_initial_live_provenance_set",
                "production_input_stage_manifest",
                "validator_vault_restore_receipt",
                "validator_key_install_receipt",
                "validator_public_keys",
                "legacy_public_height_receipt",
                "legacy_maintenance_evidence_bundle",
                "legacy_maintenance_evidence_bundle_sidecar",
                "legacy_maintenance_boundary",
                "legacy_maintenance_boundary_sidecar",
                "legacy_late_fork_source_set",
                "legacy_late_fork_source_set_sidecar",
                "legacy_late_fork_interlock_tool",
                "offline_stop_evidence",
                "offline_stop_evidence_sidecar",
                "ssh_known_hosts",
                "reward_probe",
                "source_snapshot",
                "source_wal",
                "caddy",
                *(
                    rollout.pretag_artifact_key(kind, platform)
                    for kind, platform in rollout.PRETAG_GROUPS
                ),
            ]
            if production
            else []
        )
        artifacts = {}
        for name in artifact_names:
            path = root / name
            if name == "legacy_validator_set":
                path.write_text(
                    json.dumps(
                        [
                            {"address": f"{index + 101:064x}", "stake": 5_000_000}
                            for index in range(8)
                        ],
                        sort_keys=True,
                        separators=(",", ":"),
                    )
                    + "\n",
                    encoding="utf-8",
                )
            else:
                path.write_bytes(f"arc-{name}".encode())
            path.chmod(0o700 if name in {"binary", "caddy"} else 0o600)
            artifacts[name] = {"path": str(path), "sha256": digest(path)}
        validators = []
        balanced_ranges = (
            [[0, 6], [12, 17], [17, 22]],
            [[0, 6], [12, 17], [22, 27]],
            [[0, 6], [17, 22], [27, 32]],
            [[6, 12], [12, 17], [27, 32]],
            [[6, 12], [17, 22], [22, 27]],
            [[6, 12], [22, 27], [27, 32]],
        )
        for index in range(6):
            key = root / f"key-{index}.json"
            key.write_text("{}", encoding="utf-8")
            key.chmod(0o600)
            if production:
                production_name, ip = rollout.PRODUCTION_FLEET[index]
            else:
                production_name, ip = f"validator-{index + 1}", f"127.0.0.{index + 11}"
            node = {
                "name": production_name,
                "address": f"{index + 1:064x}",
                "stake": 5_000_000,
                "key_file": str(key),
                "rpc_listen": "127.0.0.1:9944" if production else f"{ip}:9090",
                "rpc_url": f"https://{ip}" if production else f"http://{ip}:9090",
                "p2p_port": 10001 + index,
                "p2p_advertise": f"{ip}:{10001 + index}",
                "data_dir": "/var/lib/arc-v3" if production else str(root / f"data-{index}"),
                "extra_args": ["--enable-community-rewards-v1"] if reward_receipt else [],
            }
            if production:
                node.update(
                    {
                        "host": ip,
                        "ssh_user": "root",
                        "remote_root": "/opt/arc/recovery-v3",
                        "service_user": "root",
                        "service_name": "arc-node-v3-recovery.service",
                        "model_path": "/opt/arc-models/llama2-7b.gguf",
                        "model_sha256": rollout.CANONICAL_MODEL_SHA256,
                        "model_size_bytes": rollout.CANONICAL_MODEL_SIZE_BYTES,
                        "shard_ranges": balanced_ranges[index],
                    }
                )
            validators.append(node)
        reward = {
            "mode": "policy",
            "expect_protocol_active": False,
            "expect_issuance_ready": False,
        }
        if reward_receipt:
            reward = {
                "mode": "receipt",
                "expect_protocol_active": True,
                "expect_issuance_ready": True,
                "receipts": [
                    {
                        "tx_hash": "0x" + "a" * 64,
                        "job_id": "0x" + "b" * 64,
                        "worker": "0x" + "c" * 64,
                    },
                    {
                        "tx_hash": "0x" + "d" * 64,
                        "job_id": "0x" + "e" * 64,
                        "worker": "0x" + "c" * 64,
                    },
                ],
                "expected_reward_base": 2_500_000_000,
            }
        elif production:
            reward = {
                "mode": "receipt",
                "expect_protocol_active": True,
                "expect_issuance_ready": True,
                "probe_argv": [
                    artifacts["reward_probe"]["path"],
                    "--max-tokens",
                    "1",
                ],
                "probe_sha256": artifacts["reward_probe"]["sha256"],
                "expected_reward_base": 2_500_000_000,
            }
        gateway = {"mode": "none"}
        if production:
            gateway = {
                "mode": "caddy-nginx",
                "acme_email": "ops@example.test",
                "public_get_paths": list(rollout.DEFAULT_PUBLIC_GET_PATHS),
                "public_post_paths": list(rollout.DEFAULT_PUBLIC_POST_PATHS),
            }
        offline_stop_verification = None
        protected_pretag_artifact = None
        validator_key_receipt_chain = None
        validator_installed_key_proof = None
        if production:
            freeze_sha = "e" * 64
            capture_id = rollout.capture_id_for_freeze_plan_hash(freeze_sha)
            challenge = "7" * 64
            stop_nodes = []
            for index, ((name, host), validator) in enumerate(
                zip(rollout.PRODUCTION_FLEET, validators)
            ):
                status = {
                    "schema": "arc.recovery.offline-stop-challenged-status.v1",
                    "capture_id": capture_id,
                    "node": name,
                    "host": host,
                    "freeze_plan_sha256": freeze_sha,
                    "validator_address": validator["address"],
                    "stake": validator["stake"],
                    "stopped": True,
                    "restart_fenced": True,
                    "stop_schema": "arc.recovery.offline-stop.v4",
                    "stop_complete_sha256": f"{index + 20:064x}",
                    "stop_files_sha256": f"{index + 40:064x}",
                    "challenge": challenge,
                }
                stop_nodes.append(
                    {
                        "node": name,
                        "host": host,
                        "status": status,
                        "status_sha256": hashlib.sha256(rollout.canonical_bytes(status)).hexdigest(),
                    }
                )
            offline_stop_verification = {
                "schema": "arc.recovery.offline-stop-remote-verification.v1",
                "source_main_commit": "9" * 40,
                "freeze_plan_sha256": freeze_sha,
                "capture_id": capture_id,
                "remote_helper_sha256": "a" * 64,
                "remote_helper_path": "/root/.arc-recovery-helpers/" + "a" * 64 + "/archive-node.sh",
                "offline_stop_evidence_sha256": artifacts["offline_stop_evidence"]["sha256"],
                "ssh_known_hosts_sha256": artifacts["ssh_known_hosts"]["sha256"],
                "ssh_path": "/usr/bin/ssh",
                "ssh_sha256": "6" * 64,
                "challenge": challenge,
                "started_at": "2026-08-28T12:00:00Z",
                "completed_at": "2026-08-28T12:00:01Z",
                "duration_ms": 1001,
                "nodes": stop_nodes,
            }

            def proof_api(now: int, *, receipt: bool = False) -> dict:
                third = "artifact" if receipt else "artifact_set"
                return {
                    "origin": "https://api.github.com",
                    "anonymous": True,
                    "redirects_followed": False,
                    "max_age_seconds": 300,
                    "curl_sha256": "a" * 64,
                    "ca_bundle_sha256": "b" * 64,
                    "responses": [
                        {
                            "label": label,
                            "body_sha256": f"{index + 30:064x}",
                            "response_unix": now,
                            "request_id": f"ABCDEF00-{index + 1:02d}",
                            "cache_control": "public, max-age=60",
                            "age": 0,
                        }
                        for index, label in enumerate(
                            ("workflow", "run", third, "protected_main")
                        )
                    ],
                }

            initial_api = proof_api(1_800_000_000)
            final_api = proof_api(1_800_000_001)
            groups = []
            for artifact_index, (kind, platform) in enumerate(rollout.PRETAG_GROUPS):
                raw_key = rollout.pretag_artifact_key(kind, platform)
                raw_sha = artifacts[raw_key]["sha256"]
                archive_sha = f"{artifact_index + 80:064x}"
                if kind == "headless":
                    suffix = ".exe" if platform == "windows-x86_64" else ""
                    names = (
                        f"arc-node-{platform}{suffix}",
                        f"arc-cli-{platform}{suffix}",
                        "genesis.toml",
                    )
                else:
                    names = rollout.PRETAG_DESKTOP_FILES[platform]
                files = {
                    name: f"{artifact_index * 10 + file_index + 120:064x}"
                    for file_index, name in enumerate(names)
                }
                if (kind, platform) == ("headless", "linux-x86_64"):
                    files = {
                        "arc-node-linux-x86_64": artifacts["binary"]["sha256"],
                        "arc-cli-linux-x86_64": artifacts["cli"]["sha256"],
                        "genesis.toml": artifacts["genesis"]["sha256"],
                    }
                artifact = {
                    "kind": kind,
                    "platform": platform,
                    "version": "0.8.0",
                    "raw_actions_zip_sha256": raw_sha,
                    "raw_actions_zip_size": len(f"arc-{raw_key}".encode()),
                    "archive_sha256": archive_sha,
                    "build_metadata_sha256": (
                        artifacts["build_metadata"]["sha256"]
                        if artifact_index == 0
                        else f"{artifact_index + 210:064x}"
                    ),
                    "files": files,
                }
                live = {
                    "repository": "FerrumVir/arc-chain",
                    "protected_branch": "main",
                    "commit": "9" * 40,
                    "workflow_id": 919191,
                    "workflow_path": ".github/workflows/release-signing-preflight.yml",
                    "run_id": 123,
                    "run_attempt": 1,
                    "artifact_id": 1000 + artifact_index,
                    "artifact_name": (
                        f"arc-pretag-{kind}-{platform}-{'9' * 40}-123-1-{archive_sha}"
                    ),
                    "artifact_digest": f"sha256:{raw_sha}",
                    "artifact_size_in_bytes": artifact["raw_actions_zip_size"],
                    "api_verified_at_unix": 1_800_000_000,
                }
                initial = {
                    "schema": "arc.protected-pretag-artifact.v1",
                    "live": live,
                    "api": copy.deepcopy(initial_api),
                    "artifact": artifact,
                }
                final = copy.deepcopy(initial)
                final["live"]["api_verified_at_unix"] = 1_800_000_001
                final["api"] = copy.deepcopy(final_api)
                groups.append(
                    {"kind": kind, "platform": platform, "initial": initial, "final": final}
                )
            protected_pretag_artifact = {
                "schema": "arc.protected-pretag-artifact-window-set.v1",
                "groups": groups,
            }
            validator_key_receipt_chain = {
                "schema": "arc.recovery.validator-key-receipt-chain.v1",
                "source_main_commit": "9" * 40,
                "restore_receipt_sha256": artifacts["validator_vault_restore_receipt"]["sha256"],
                "install_receipt_sha256": artifacts["validator_key_install_receipt"]["sha256"],
                "linux_pretag_artifact_id": groups[0]["initial"]["live"]["artifact_id"],
                "linux_pretag_raw_actions_zip_sha256": artifacts[
                    "pretag_raw_headless_linux_x86_64"
                ]["sha256"],
                "arc_cli_sha256": artifacts["cli"]["sha256"],
                "genesis_sha256": artifacts["genesis"]["sha256"],
                "validator_public_keys_sha256": artifacts["validator_public_keys"]["sha256"],
                "freeze_plan_sha256": "e" * 64,
                "offline_stop_evidence_sha256": artifacts["offline_stop_evidence"]["sha256"],
                "known_hosts_sha256": artifacts["ssh_known_hosts"]["sha256"],
                "ssh_identity_sha256": "5" * 64,
                "ssh_sha256": "6" * 64,
                "scp_sha256": "7" * 64,
                "validators": [
                    {
                        "node": name,
                        "host": host,
                        "address": validators[index]["address"],
                        "keyfile_sha256": f"{index + 240:064x}",
                    }
                    for index, (name, host) in enumerate(rollout.PRODUCTION_FLEET)
                ],
            }
            installed_challenge = "4" * 64
            validator_installed_key_proof = {
                "schema": "arc.recovery.validator-installed-key-proof.v1",
                "source_main_commit": "9" * 40,
                "production_input_stage_manifest_sha256": artifacts[
                    "production_input_stage_manifest"
                ]["sha256"],
                "freeze_plan_sha256": "e" * 64,
                "offline_stop_evidence_sha256": artifacts["offline_stop_evidence"]["sha256"],
                "validator_install_receipt_sha256": artifacts[
                    "validator_key_install_receipt"
                ]["sha256"],
                "validator_public_keys_sha256": artifacts["validator_public_keys"]["sha256"],
                "arc_cli_sha256": artifacts["cli"]["sha256"],
                "remote_helper_sha256": "a" * 64,
                "remote_helper_path": "/root/.arc-recovery-helpers/" + "a" * 64 + "/archive-node.sh",
                "ssh_known_hosts_sha256": artifacts["ssh_known_hosts"]["sha256"],
                "ssh_identity_sha256": "5" * 64,
                "ssh_path": "/usr/bin/ssh",
                "ssh_sha256": "6" * 64,
                "scp_path": "/usr/bin/scp",
                "scp_sha256": "7" * 64,
                "challenge": installed_challenge,
                "started_at_unix_ms": 1_800_000_002_000,
                "completed_at_unix_ms": 1_800_000_003_000,
                "validators": [
                    {
                        "node": row["node"],
                        "host": row["host"],
                        "key_path": "/etc/arc-v3/validator-key.json",
                        "address": row["address"],
                        "keyfile_sha256": row["keyfile_sha256"],
                        "remote_response_sha256": f"{index + 250:064x}",
                        "state": "verified",
                    }
                    for index, row in enumerate(validator_key_receipt_chain["validators"])
                ],
            }
        self.value = {
            "schema": rollout.SCHEMA,
            "rollout_id": "recovery-v3-test",
            "mode": "production" if production else "local",
            **(
                {
                    "provenance": {
                        "source_main_commit": "9" * 40,
                        "pretag_repository": "FerrumVir/arc-chain",
                        "pretag_version": "0.8.0",
                        "pretag_workflow_run_id": 123,
                        "pretag_workflow_run_attempt": 1,
                        "protected_pretag_artifact": protected_pretag_artifact,
                        "production_input_stage_manifest_sha256": artifacts[
                            "production_input_stage_manifest"
                        ]["sha256"],
                        "validator_key_receipt_chain": validator_key_receipt_chain,
                        "validator_installed_key_proof": validator_installed_key_proof,
                        "freeze_plan_sidecar_sha256": "8" * 64,
                        "offline_stop_verification": offline_stop_verification,
                    },
                    "archive": {
                        "freeze_plan_sha256": "e" * 64,
                        "capture_id": rollout.capture_id_for_freeze_plan_hash("e" * 64),
                        "destination": "arc-drive:ARC Chain Recovery/captures/"
                        + rollout.capture_id_for_freeze_plan_hash("e" * 64),
                        "allow_unbound_legacy_wal": False,
                        "archive_orchestrator_sha256": "d" * 64,
                        "remote_helper_sha256": "a" * 64,
                        "rollout_tool_sha256": "b" * 64,
                        "rollout_schema_sha256": "c" * 64,
                        "complete_sha256": "0" * 64,
                        "archive_manifest_sha256": "0" * 64,
                        "sha256sums_sha256": "0" * 64,
                        "prearchive_rollout_sha256": "0" * 64,
                    }
                }
                if production
                else {}
            ),
            "chain": {
                "chain_id": "arc-recovery-test",
                "genesis_hash": "0x" + "0" * 64,
                "protocol_version": "3.0.0",
                "recovery_epoch": 7,
                "validator_set_id": 9,
                "source_height": 100,
                "legacy_public_max_height": 238 if production else 110,
                **(
                    {
                        "legacy_maintenance_evidence_bundle_sha256": artifacts[
                            "legacy_maintenance_evidence_bundle"
                        ]["sha256"],
                        "legacy_maintenance_boundary_sha256": artifacts[
                            "legacy_maintenance_boundary"
                        ]["sha256"],
                        "legacy_late_fork_source_set_sha256": artifacts[
                            "legacy_late_fork_source_set"
                        ]["sha256"],
                        "legacy_observed_cutoff_height": 110,
                        "legacy_continuity_safety_margin": 128,
                        "legacy_global_absence_claimed": False,
                        "legacy_official_origins": [
                            dict(row) for row in rollout.LEGACY_OFFICIAL_ORIGINS
                        ],
                        "legacy_reopening_policy": copy.deepcopy(
                            rollout.LEGACY_REOPENING_POLICY
                        ),
                        "legacy_late_fork_circuit": copy.deepcopy(
                            rollout.LEGACY_LATE_FORK_CIRCUIT
                        ),
                        "legacy_quarantine_threat_model": copy.deepcopy(
                            rollout.LEGACY_QUARANTINE_THREAT_MODEL
                        ),
                    }
                    if production
                    else {}
                ),
                "source_consensus_round": 9876,
                "created_at_unix_ms": 1_787_857_623_000,
                "source_block_hash": "0x" + "1" * 64,
                "source_state_root": "0x" + "5" * 64,
                "transition_height": 101,
                "transition_block_hash": "0x" + "2" * 64,
                "full_state_root": "0x" + "3" * 64,
                "recovery_domain": "0x" + "6" * 64,
                "approved_checkpoint_manifest_hash": "0x" + "4" * 64,
            },
            "artifacts": artifacts,
            "checks": {
                "startup_timeout_seconds": 10,
                "convergence_timeout_seconds": 10,
                "observation_seconds": 1,
                "restart_timeout_seconds": 10,
                "poll_interval_seconds": 1,
                "min_height_advance": 1,
                "reward": reward,
            },
            "gateway": gateway,
            "validators": validators,
        }


class RecoveryRolloutTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.caddy_sha_patch = mock.patch.object(
            rollout,
            "CADDY_LINUX_AMD64_SHA256",
            hashlib.sha256(b"arc-caddy").hexdigest(),
        )
        self.caddy_sha_patch.start()

    def tearDown(self) -> None:
        self.caddy_sha_patch.stop()
        self.temporary.cleanup()

    def fixture(self, **kwargs):
        return ManifestFixture(self.root, **kwargs).value

    @staticmethod
    def reward_receipt_row(
        item,
        block,
        *,
        worker=None,
        recovery_epoch=1,
        validator_set_id=1,
        transaction_domain=None,
    ):
        worker = worker or item.worker
        transaction_domain = transaction_domain or ("0x" + "5" * 64)
        return {
            "tx_type": "0x25",
            "tx_hash": item.tx_hash,
            "job_id": item.job_id,
            "worker": worker,
            "model_id": "0x" + "1" * 64,
            "input_hash": "0x" + "2" * 64,
            "output_hash": "0x" + "3" * 64,
            "assignment_epoch": "0x" + "4" * 64,
            "recovery_epoch": recovery_epoch,
            "validator_set_id": validator_set_id,
            "transaction_domain": transaction_domain,
            "block_height": block[0],
            "block_hash": block[1],
            "index": block[2],
            "submitted": True,
            "included": True,
            "confirmed": True,
            "success": True,
            "receipt_url": f"/community/reward_receipt/{item.tx_hash}",
            "reward_base": 2_500_000_000,
            "reward_arc": 2.5,
        }

    @staticmethod
    def two_receipt_evidence() -> list[rollout.ReceiptEvidence]:
        return [
            rollout.ReceiptEvidence.from_value(
                {
                    "tx_hash": "0x" + tx * 64,
                    "job_id": "0x" + job * 64,
                    "worker": "0x" + "c" * 64,
                }
            )
            for tx, job in (("a", "b"), ("d", "e"))
        ]

    @staticmethod
    def reward_earnings(worker, rows, **overrides):
        count = len(rows)
        gross_base = count * 2_500_000_000
        body = {
            "address": worker,
            "archive_mode": True,
            "history_complete_since_recovery": True,
            "history_scope": rollout.ARCHIVE_EARNINGS_SCOPE,
            "history_domain": rollout.EARNINGS_HISTORY_DOMAIN,
            "confirmed_receipt_count": count,
            "confirmed_gross_earnings_base": gross_base,
            "confirmed_gross_earnings_arc": gross_base / 1_000_000_000,
            "confirmed_receipts": copy.deepcopy(rows),
            "attestations_per_day_observed": None,
            "attestations_per_day_unavailable_reason": (
                rollout.PROJECTION_COLLECTING_REASON
                if count < 3
                else rollout.PROJECTION_WINDOW_COLLECTING_REASON
            ),
            "projected_daily_arc": None,
            "projected_daily_unavailable_reason": (
                rollout.PROJECTION_COLLECTING_REASON
                if count < 3
                else rollout.PROJECTION_WINDOW_COLLECTING_REASON
            ),
            "observed_window_first_timestamp_ms": None,
            "observed_window_last_timestamp_ms": None,
        }
        body.update(overrides)
        return body

    @classmethod
    def install_reward_baseline(cls, harness, worker, rows=()):
        baseline = rollout.RewardEarningsBaseline.from_earnings(
            cls.reward_earnings(worker, list(rows)), worker=worker
        )
        harness.reward_earnings_baselines = {worker: baseline}
        harness.reward_earnings_baseline = baseline
        return baseline

    @classmethod
    def persist_reward_baseline(cls, harness, worker, rows=()):
        baseline = rollout.RewardEarningsBaseline.from_earnings(
            cls.reward_earnings(worker, list(rows)), worker=worker
        )
        harness.persist_reward_earnings_baselines([baseline])
        return baseline

    @staticmethod
    def prime_interlock_interpreters(harness):
        harness.legacy_interlock_interpreters = {
            node["name"]: {
                "normalized_path": "/usr/bin/python3.12",
                "sha256": f"{index + 1800:064x}",
                "device": 100 + index,
                "inode": 200 + index,
                "uid": 0,
                "gid": 0,
                "mode": 0o755,
                "nlink": 1,
                "isolated": True,
                "environment": {
                    "PATH": "/usr/bin:/bin", "LC_ALL": "C", "TZ": "UTC",
                    "PYTHONHASHSEED": "0",
                },
            }
            for index, node in enumerate(harness.validators)
        }

    @staticmethod
    def public_tls_evidence(
        harness,
        node,
        *,
        phase: str = "preflight",
        now_unix: int | None = None,
    ) -> dict:
        now = int(rollout.time.time()) if now_unix is None else now_unix
        not_before = now - 3_600
        not_after = not_before + 159 * 60 * 60
        return {
            "schema": "arc.recovery.public-tls-evidence.v1",
            "rollout_manifest_sha256": harness.digest,
            "phase": phase,
            "node": node["name"],
            "host": node["host"],
            "caddy_version": rollout.CADDY_VERSION,
            "caddy_binary_sha256": rollout.CADDY_LINUX_AMD64_SHA256,
            "acme_directory": rollout.LETS_ENCRYPT_PRODUCTION_DIRECTORY,
            "acme_profile": "shortlived",
            "renewal_window_ratio": rollout.TLS_RENEWAL_WINDOW_RATIO,
            "verification_host": node["host"],
            "san_ip_addresses": [node["host"]],
            "san_dns_names": [],
            "issuer_organization": "Let's Encrypt",
            "leaf_sha256": "9" * 64,
            "not_before_unix": not_before,
            "not_after_unix": not_after,
            "lifetime_seconds": not_after - not_before,
            "remaining_validity_seconds": not_after - now,
            "verified_at_unix": now,
            "hostname_verified": True,
            "public_trust_verified": True,
            "leaf_self_signed": False,
            "https_probe_status": 404,
            "renewal_observed": False,
            "evidence_scope": "fresh-verified-handshake-and-https-probe-not-renewal",
        }

    def maintenance_stage_fixture(self):
        """Build a compact canonical bundle/boundary/offline cross-binding set."""

        value = self.fixture(production=True)
        canonical = rollout.canonical_bytes
        sha_value = lambda item: hashlib.sha256(canonical(item)).hexdigest()
        source_commit = value["provenance"]["source_main_commit"]
        freeze = {
            "schema": "arc.recovery.freeze-plan.v5",
            "source_commit": source_commit,
            "nodes": [
                {"name": node, "host": host}
                for node, host in rollout.PRODUCTION_FLEET
            ],
        }
        freeze_sha = sha_value(freeze)
        capture_id = rollout.capture_id_for_freeze_plan_hash(freeze_sha)
        value["archive"]["freeze_plan_sha256"] = freeze_sha
        value["archive"]["capture_id"] = capture_id
        value["archive"]["destination"] = (
            f"arc-drive:ARC Chain Recovery/captures/{capture_id}"
        )
        remote_verification = value["provenance"]["offline_stop_verification"]
        remote_verification["freeze_plan_sha256"] = freeze_sha
        remote_verification["capture_id"] = capture_id
        for row in remote_verification["nodes"]:
            row["status"]["freeze_plan_sha256"] = freeze_sha
            row["status"]["capture_id"] = capture_id
            row["status_sha256"] = sha_value(row["status"])
        challenge = "7" * 64
        public_origins = []
        authenticated_nodes = []
        for index, (node, host) in enumerate(rollout.PRODUCTION_FLEET):
            height = 110 + index
            public_origins.append(
                {
                    "name": node,
                    "origin": f"http://{host}:9090",
                    "info_before_height": height - 2,
                    "latest_block_height": height - 1,
                    "info_after_height": height,
                }
            )
            proof = {
                "capture_id": capture_id,
                "node": node,
                "freeze_plan_sha256": freeze_sha,
                "challenge": challenge,
                "authenticated_info_before_height": height + 1,
                "authenticated_latest_block_height": height + 2,
                "authenticated_info_after_height": height + 3,
                "conservative_height_floor": height + 3,
            }
            authenticated_nodes.append(
                {
                    "node": node,
                    "host": host,
                    "proof": proof,
                    "proof_sha256": sha_value(proof),
                }
            )
        public = {
            "schema": "arc.recovery.legacy-public-height.v1",
            "source_main_commit": source_commit,
            "freeze_plan_sha256": freeze_sha,
            "capture_id": capture_id,
            "completed_at": "2026-08-28T11:59:59Z",
            "origins": public_origins,
            "legacy_public_max_height": max(
                row["info_after_height"] for row in public_origins
            ),
        }
        public_sha = sha_value(public)
        authenticated = {
            "schema": "arc.recovery.authenticated-legacy-height-fleet.v1",
            "source_main_commit": source_commit,
            "freeze_plan_sha256": freeze_sha,
            "capture_id": capture_id,
            "nodes": authenticated_nodes,
        }
        challenge_value = {
            "schema": "arc.recovery.legacy-network-quarantine-challenge.v1",
            "freeze_plan_sha256": freeze_sha,
            "capture_id": capture_id,
            "challenge": challenge,
        }
        first = "2026-08-28T12:00:00Z"
        stopped_at = "2026-08-28T12:02:02Z"
        inventory = []

        def tuple_at(height, salt):
            return {
                "height": height,
                "block_hash": f"{salt:064x}",
                "state_root": f"{salt + 1000:064x}",
            }

        def sealed(inner, node, role):
            raw = canonical(inner)
            root = hashlib.sha256(raw).hexdigest()
            inventory.append(
                {"node": node, "role": role, "sha256": root, "size": len(raw)}
            )
            return {"value": inner, "sha256": root}

        observation_generation = "d" * 64
        generation_receipt = {
            "schema": "arc.recovery.legacy-live-observation-generation.v1",
            "source_main_commit": source_commit,
            "freeze_plan_sha256": freeze_sha,
            "capture_id": capture_id,
            "observation_generation": observation_generation,
            "created_at": "2026-08-28T11:59:50.000000Z",
            "max_selection_age_seconds": 300,
            "drive_prefreeze_receipt": {
                "path": "/private/drive-prefreeze.json",
                "sha256": "e" * 64,
                "value": {
                    "schema": "arc.recovery.drive-prefreeze.v1",
                    "mode": "execute", "freeze_plan_sha256": freeze_sha,
                    "capture_id": capture_id, "remote_root_sha256": "1" * 64,
                    "client_id_sha256": "2" * 64, "account_sha256": "3" * 64,
                    "permission_id_sha256": "4" * 64, "rclone_version": "v1.75.0",
                    "source_bytes": 1, "archive_reservation_bytes": 2,
                    "largest_object_reservation_bytes": 1,
                    "daily_upload_budget_bytes": 2,
                    "daily_upload_budget_basis": "operator-reviewed-remaining-dedicated-account",
                    "available_bytes_before": 8 * 1024 * 1024 + 2,
                    "available_bytes_after": 2, "canary_bytes": 8 * 1024 * 1024,
                    "canary_verified": True, "canary_deleted": True,
                },
            },
        }
        generation_receipt["drive_prefreeze_receipt"]["sha256"] = sha_value(
            generation_receipt["drive_prefreeze_receipt"]["value"]
        )
        observation_selection = {
            "schema": "arc.recovery.legacy-live-observation-selection.v1",
            "source_main_commit": source_commit,
            "freeze_plan_sha256": freeze_sha,
            "capture_id": capture_id,
            "observation_generation": observation_generation,
            "observation_generation_receipt": generation_receipt,
            "observation_generation_receipt_path": (
                f"/private/live-observation-generations/{observation_generation}.json"
            ),
            "observation_generation_receipt_sha256": sha_value(generation_receipt),
            "drive_prefreeze_receipt_path": "/private/drive-prefreeze.json",
            "drive_prefreeze_receipt_sha256": generation_receipt[
                "drive_prefreeze_receipt"
            ]["sha256"],
            "generation_created_at": generation_receipt["created_at"],
            "selected_at": "2026-08-28T11:59:54.000000Z",
            "max_selection_age_seconds": 300,
            "labels": ["diagnostic", "noncanonical", "nonreward"],
            "nodes": [
                {"node": node, "created_at": "2026-08-28T11:59:51.000000Z",
                 "completed_at": "2026-08-28T11:59:53.000000Z",
                 "root_sha256": f"{index + 7000:064x}",
                 "receipt_sha256": f"{index + 7100:064x}"}
                for index, (node, _host) in enumerate(rollout.PRODUCTION_FLEET)
            ],
        }
        observation_selection_sealed = {
            "value": observation_selection,
            "sha256": sha_value(observation_selection),
        }

        target_public_origins = []
        for index, row in enumerate(public_origins):
            target_public_origins.append({
                **row,
                "latest_block_hash": f"{index + 2000:064x}",
                "info_before_body_sha256": f"{index + 2100:064x}",
                "latest_block_body_sha256": f"{index + 2200:064x}",
                "info_after_body_sha256": f"{index + 2300:064x}",
            })
        target_rows = [
            {"node": node, "host": host, "rpc_origin": f"http://{host}:9090"}
            for node, host in rollout.PRODUCTION_FLEET
        ]
        target_public = {
            "schema": quarantine_rounds.TARGET_HEIGHT_SCHEMA,
            "source_main_commit": source_commit,
            "freeze_plan_sha256": freeze_sha,
            "capture_id": capture_id,
            "started_at": "2026-08-28T11:59:50Z",
            "completed_at": "2026-08-28T11:59:52Z",
            "duration_ms": 2000,
            "request_policy": {
                "redirects": "forbidden", "maximum_body_bytes": 1048576,
                "timeout_seconds": 10, "proxy_environment": "ignored",
                "sequence": ["/info", "/block/latest", "/info"],
            },
            "targets": target_rows,
            "origins": target_public_origins,
            "legacy_public_max_height": max(
                row["info_after_height"] for row in target_public_origins
            ),
        }
        target_cross_nodes = []
        live_targets = []
        for index, ((node, host), public_row) in enumerate(
            zip(rollout.PRODUCTION_FLEET, target_public_origins)
        ):
            boot_id = f"00000000-0000-0000-0000-{index + 1:012d}"
            writer = {
                "node": node, "host": host, "boot_id": boot_id,
                "writer_pid": 1000 + index, "writer_start_ticks": 2000 + index,
                "writer_cgroup_sha256": f"{index + 900:064x}",
            }
            live_targets.append(writer)
            target_cross_nodes.append({
                **writer,
                "public_info_after_height": public_row["info_after_height"],
                "public_latest_block_height": public_row["latest_block_height"],
                "public_latest_block_hash": public_row["latest_block_hash"],
                "loopback_info_before_height": public_row["info_after_height"] + 3,
                "loopback_latest_height": public_row["info_after_height"] + 3,
                "loopback_info_after_height": public_row["info_after_height"] + 3,
                "loopback_latest_block_hash": public_row["latest_block_hash"],
                "response_sha256": {
                    "/info:before": f"{index + 2400:064x}",
                    "/block/latest": f"{index + 2500:064x}",
                    "/info:after": f"{index + 2600:064x}",
                },
            })
        target_cross = {
            "schema": quarantine_rounds.TARGET_CROSS_SCHEMA,
            "source_main_commit": source_commit,
            "freeze_plan_sha256": freeze_sha,
            "capture_id": capture_id,
            "legacy_public_height_receipt_sha256": sha_value(target_public),
            "challenge": challenge,
            "started_at": "2026-08-28T11:59:53Z",
            "completed_at": "2026-08-28T11:59:54Z",
            "conservative_height_floor": min(
                row["loopback_info_before_height"] for row in target_cross_nodes
            ),
            "targets": copy.deepcopy(target_rows),
            "nodes": target_cross_nodes,
        }

        def preauthorization_capture(index, target, public_row, cross_row):
            seed = 20_000 + index * 100

            def directory(offset):
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

            def regular(offset, root, size=8):
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
            head = {
                "height": max(
                    public_latest,
                    cross_latest,
                    public_row["info_after_height"],
                    cross_row["loopback_info_after_height"],
                ),
                "block_hash": cross_row["loopback_latest_block_hash"],
                "state_root": f"{seed + 10:064x}",
            }
            wal_root = f"{seed + 11:064x}"
            snapshot_root = f"{seed + 12:064x}"
            genesis_root = f"{seed + 13:064x}"
            legacy_root = f"{seed + 14:064x}"
            rust_capture = {
                "schema": quarantine_rounds.RUST_SOURCE_CAPTURE_SCHEMA,
                "captured_at_unix_ms": 1_777_000_000_000 + index,
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
                    "genesis_binding": regular(100, f"{seed + 15:064x}"),
                    "strict_replay": True,
                },
                "allow_unbound_legacy_wal": False,
            }
            capture = {
                "schema": quarantine_rounds.LIVE_SOURCE_CAPTURE_SCHEMA,
                "capture_id": capture_id,
                "freeze_plan_sha256": freeze_sha,
                "source_main_commit": source_commit,
                "round_number": 1,
                "node": target["node"],
                "host": target["host"],
                "authorized_writer": {
                    "boot_id": target["boot_id"],
                    "pid": target["writer_pid"],
                    "start_ticks": target["writer_start_ticks"],
                    "cgroup_sha256": target["writer_cgroup_sha256"],
                },
                "rpc_origin": "http://127.0.0.1:9090",
                "public_height_receipt_sha256": sha_value(target_public),
                "authenticated_height_cross_proof_sha256": sha_value(target_cross),
                "snapshot_endpoint": "/sync/snapshot",
                "snapshot_listener": {
                    "boot_id": target["boot_id"],
                    "pid": target["writer_pid"],
                    "start_ticks": target["writer_start_ticks"],
                    "port": 9090,
                    "socket_inode": seed + 16,
                },
                "capture_attempt_id": f"00000000-0000-4000-8000-{index + 1:012d}",
                "capture_started_at": "2026-08-28T11:59:54Z",
                "capture_completed_at": "2026-08-28T11:59:54Z",
                "inspector_binary_sha256": f"{seed + 17:064x}",
                "genesis_sha256": genesis_root,
                "legacy_validator_set_sha256": legacy_root,
                "fixed_pair_path": (
                    f"/root/arc-recovery-live-source-captures/{capture_id}/"
                    f"{target['node']}/round-1/preauthorization-boundary/"
                    f"attempt-{index + 1}/fixed-source"
                ),
                "snapshot_source": "sealed-writer-owned-loopback-/sync/snapshot",
                "existing_source_snapshot_used": False,
                "rust_capture": quarantine_rounds.wrap(rust_capture),
                "head": head,
                "ancestry_checks": [
                    {
                        "label": "public-latest",
                        "height": public_latest,
                        "expected_block_hash": public_row["latest_block_hash"],
                        "observed_block_hash": public_row["latest_block_hash"],
                        "state_root": f"{seed + 18:064x}",
                        "inspection_sha256": f"{seed + 19:064x}",
                    },
                    {
                        "label": "authenticated-loopback-latest",
                        "height": cross_latest,
                        "expected_block_hash": cross_row["loopback_latest_block_hash"],
                        "observed_block_hash": cross_row["loopback_latest_block_hash"],
                        "state_root": f"{seed + 20:064x}",
                        "inspection_sha256": f"{seed + 21:064x}",
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
                "boundary_proof_sha256": sha_value(target_cross),
                "network_quarantine_receipt_sha256": None,
                "owned_ruleset_stateless_sha256": None,
            }
            return quarantine_rounds.wrap(capture)

        round_authorization = {
            "schema": quarantine_rounds.ROUND_AUTH_SCHEMA,
            "source_main_commit": source_commit,
            "capture_id": capture_id,
            "freeze_plan_sha256": freeze_sha,
            "live_observation_selection_sha256": observation_selection_sealed["sha256"],
            "live_observation_generation": observation_generation,
            "observation_generation_receipt_sha256": observation_selection[
                "observation_generation_receipt_sha256"
            ],
            "drive_prefreeze_receipt_sha256": observation_selection[
                "drive_prefreeze_receipt_sha256"
            ],
            "live_observation_selected_at": observation_selection["selected_at"],
            "round_number": 1,
            "prior_round_result_sha256s": [],
            "prior_fenced": [],
            "targets": live_targets,
            "public_height_receipt": quarantine_rounds.wrap(target_public),
            "authenticated_height_cross_proof": quarantine_rounds.wrap(target_cross),
            "live_source_captures": [
                preauthorization_capture(index, target, public_row, cross_row)
                for index, (target, public_row, cross_row) in enumerate(
                    zip(live_targets, target_public_origins, target_cross_nodes)
                )
            ],
            "authorized_at": "2026-08-28T11:59:55Z",
            "authorization_deadline": "2026-08-28T12:04:52Z",
        }
        round_auth_sha = sha_value(round_authorization)
        round_readiness = {
            "schema": quarantine_rounds.READINESS_SCHEMA,
            "capture_id": capture_id, "freeze_plan_sha256": freeze_sha,
            "round_number": 1, "round_authorization_sha256": round_auth_sha,
            "targets": [
                {
                    "node": target["node"], "host": target["host"],
                    "authorization_acceptance": quarantine_rounds.wrap({
                        "schema": quarantine_rounds.AUTH_ACCEPTANCE_SCHEMA,
                        "capture_id": capture_id, "freeze_plan_sha256": freeze_sha,
                        "round_number": 1,
                        "round_authorization_sha256": round_auth_sha,
                        "node": target["node"], "host": target["host"],
                        "accepted_at": round_authorization["authorized_at"],
                        "accepted_monotonic_ns": 1_000_000_000,
                        "accepted_boot_id": target["boot_id"],
                        "authorization_deadline": round_authorization[
                            "authorization_deadline"
                        ],
                    }),
                }
                for target in live_targets
            ],
            "completed_at": round_authorization["authorized_at"],
            "authorization_deadline": round_authorization["authorization_deadline"],
            "max_elapsed_since_acceptance_ns": 300_000_000_000,
        }
        round_readiness_sha = sha_value(round_readiness)
        network_receipts_by_node = {}
        applied_values = []
        for index, ((node, host), target, public_row) in enumerate(
            zip(rollout.PRODUCTION_FLEET, live_targets, target_public_origins)
        ):
            head = tuple_at(public_row["info_after_height"] + 3, index + 20)
            head["block_hash"] = public_row["latest_block_hash"]
            network_receipt = {
                "schema": "arc.recovery.legacy-network-quarantine.v1",
                "capture_id": capture_id, "node": node, "host": host,
                "freeze_plan_sha256": freeze_sha,
                "source_main_commit": source_commit,
                "owned_ruleset_stateless_sha256": f"{index + 600:064x}",
                "file_sha256": {
                    "policy.nft": f"{index + 2700:064x}",
                    "apply": value["archive"]["remote_helper_sha256"],
                },
                "installed_at": first,
                "loopback_head": {
                    "latest_height": head["height"],
                    "info_after_height": head["height"],
                    "block_hash": head["block_hash"],
                    "state_root": head["state_root"],
                },
            }
            binding = {
                "schema": quarantine_rounds.TABLE_BINDING_SCHEMA,
                "capture_id": capture_id, "freeze_plan_sha256": freeze_sha,
                "round_number": 1, "round_authorization_sha256": round_auth_sha,
                "round_readiness_sha256": round_readiness_sha,
                "authorization_deadline": round_authorization[
                    "authorization_deadline"
                ],
                "apply_helper_sha256": value["archive"]["remote_helper_sha256"],
                "policy_sha256": network_receipt["file_sha256"]["policy.nft"],
                "node": node, "host": host,
                "writer": {
                    "boot_id": target["boot_id"], "pid": target["writer_pid"],
                    "start_ticks": target["writer_start_ticks"],
                    "cgroup_sha256": target["writer_cgroup_sha256"],
                },
            }
            binding_sha = sha_value(binding)
            gate = {
                "schema": quarantine_rounds.NFT_GATE_SCHEMA,
                "capture_id": capture_id, "freeze_plan_sha256": freeze_sha,
                "round_authorization_sha256": round_auth_sha,
                "round_readiness_sha256": round_readiness_sha,
                "round_number": 1, "node": node, "host": host,
                "authorization_deadline": round_authorization["authorization_deadline"],
                "invoked_at": first,
                "apply_helper_sha256": value["archive"]["remote_helper_sha256"],
                "policy_sha256": network_receipt["file_sha256"]["policy.nft"],
                "table_binding_sha256": binding_sha,
                "table_comment": (
                    f"arc-recovery:round=1:bind={binding_sha}:node={node}"
                ),
            }
            cross_row = next(
                row for row in target_cross_nodes if row["node"] == node
            )
            ancestry = {
                "schema": quarantine_rounds.ANCESTRY_SCHEMA,
                "capture_id": capture_id, "freeze_plan_sha256": freeze_sha,
                "round_authorization_sha256": round_auth_sha,
                "round_number": 1, "node": node, "host": host,
                "checks": [
                    {
                        "label": "public-latest",
                        "height": public_row["latest_block_height"],
                        "expected_block_hash": public_row["latest_block_hash"],
                        "observed_block_hash": public_row["latest_block_hash"],
                        "response_sha256": f"{index + 2800:064x}",
                    },
                    {
                        "label": "authenticated-loopback-latest",
                        "height": cross_row["loopback_latest_height"],
                        "expected_block_hash": cross_row["loopback_latest_block_hash"],
                        "observed_block_hash": cross_row["loopback_latest_block_hash"],
                        "response_sha256": f"{index + 2900:064x}",
                    },
                ],
            }
            applied_commit = {
                "schema": "arc.recovery.quarantine-nft-applied-commit.v1",
                "capture_id": capture_id, "freeze_plan_sha256": freeze_sha,
                "round_number": 1, "round_authorization_sha256": round_auth_sha,
                "round_readiness_sha256": round_readiness_sha,
                "node": node, "host": host,
                "nft_deadline_gate_sha256": sha_value(gate),
                "table_binding_sha256": binding_sha,
                "table_comment": gate["table_comment"],
                "apply_helper_sha256": value["archive"]["remote_helper_sha256"],
                "nft_policy_source_sha256": network_receipt["file_sha256"]["policy.nft"],
                "owned_ruleset_stateless_sha256": network_receipt[
                    "owned_ruleset_stateless_sha256"
                ],
                "nft_applied_at": first,
            }
            apply_intent = {
                "schema": quarantine_rounds.NFT_INTENT_SCHEMA,
                "capture_id": capture_id, "freeze_plan_sha256": freeze_sha,
                "source_main_commit": source_commit, "round_number": 1,
                "round_authorization_sha256": round_auth_sha,
                "round_readiness_sha256": round_readiness_sha,
                "authorization_deadline": round_authorization["authorization_deadline"],
                "node": node, "host": host, "writer": binding["writer"],
                "table_binding_sha256": binding_sha,
                "table_comment": gate["table_comment"],
                "apply_helper_sha256": value["archive"]["remote_helper_sha256"],
                "nft_policy_source_sha256": network_receipt["file_sha256"]["policy.nft"],
                "prepared_at": "2026-08-28T11:59:56Z",
            }
            restart_sha = f"{index + 3600:064x}"
            network_receipt.update({
                "round_number": 1,
                "round_authorization_sha256": round_auth_sha,
                "round_readiness_sha256": round_readiness_sha,
                "nft_deadline_gate_sha256": sha_value(gate),
                "nft_apply_intent_sha256": sha_value(apply_intent),
                "nft_apply_intent": quarantine_rounds.wrap(apply_intent),
                "nft_table_binding_sha256": binding_sha,
                "nft_table_binding": binding,
                "table_comment": gate["table_comment"],
                "nft_table_comment": gate["table_comment"],
                "nft_policy_source_sha256": network_receipt["file_sha256"]["policy.nft"],
                "apply_helper_sha256": value["archive"]["remote_helper_sha256"],
                "applied_commit_sha256": sha_value(applied_commit),
                "authorization_ancestry_proof_sha256": sha_value(ancestry),
                "boot_id": target["boot_id"],
                "writer": {
                    "pid": target["writer_pid"], "start_ticks": target["writer_start_ticks"],
                    "cgroup_sha256": target["writer_cgroup_sha256"],
                },
                "table": {
                    "family": "inet", "name": "arc_legacy_maintenance_v1",
                    "priority": -310,
                    "hooks": ["prerouting", "input", "forward", "output"],
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
                    "state_path": "/etc/arc-recovery/network-fence-rounds/fixture",
                    "active_selector_path": "/run/arc-recovery/active-network-fence",
                    "automatic_unfence": False,
                },
                "tool_sha256": {"/usr/sbin/nft": f"{index + 3000:064x}"},
                "stable_head": head,
                "authorization_ancestry_proof": quarantine_rounds.wrap(ancestry),
                "nft_deadline_gate": quarantine_rounds.wrap(gate),
                "applied_commit": quarantine_rounds.wrap(applied_commit),
                "global_absence_claimed": False,
                "threat_model": {
                    "legacy_binary": "reviewed-non-adversarial-exact-hash",
                    "legacy_binary_sha256": value["archive"]["remote_helper_sha256"],
                },
            })
            network_receipt["file_sha256"] = {
                "authorization.json": round_auth_sha,
                "readiness.json": round_readiness_sha,
                "contract.json": f"{index + 3700:064x}",
                "table-binding.json": binding_sha,
                "nft-apply-intent.json": sha_value(apply_intent),
                "policy.nft": network_receipt["nft_policy_source_sha256"],
                "apply": network_receipt["apply_helper_sha256"],
                "nft": network_receipt["tool_sha256"]["/usr/sbin/nft"],
                "nft-deadline-gate.json": sha_value(gate),
                "applied.commit.json": sha_value(applied_commit),
                "persistent-restart-fence.json": restart_sha,
                "rendered-policy.nft": f"{index + 3800:064x}",
                "/usr/local/libexec/arc-legacy-maintenance-fence": f"{index + 3900:064x}",
                "/etc/systemd/system/arc-legacy-maintenance-fence.service": f"{index + 4000:064x}",
                "/etc/systemd/system/arc-self-heal.service.d/zzzy-arc-recovery-network-fence.conf": f"{index + 4100:064x}",
                "/etc/systemd/system/arc-node.service.d/zzzy-arc-recovery-network-fence.conf": f"{index + 4200:064x}",
                "/etc/systemd/system/arc-node-update.service.d/zzzy-arc-recovery-network-fence.conf": f"{index + 4300:064x}",
                "/etc/systemd/system/arc-node-update.timer.d/zzzy-arc-recovery-network-fence.conf": f"{index + 4400:064x}",
            }
            network_receipt["loopback_head"].update({
                "rpc_origin": "http://127.0.0.1:9090",
                "info_before_height": head["height"],
                "block_height": head["height"],
                "response_sha256": {
                    "/info:before": f"{index + 3100:064x}",
                    "/block/latest": f"{index + 3200:064x}",
                    f"/block/{head['height']}": f"{index + 3300:064x}",
                    "/health": f"{index + 3400:064x}",
                    "/info:after": f"{index + 3500:064x}",
                },
                "stable_attempt": 1,
            })
            network_receipts_by_node[node] = network_receipt
            applied_values.append({
                "schema": quarantine_rounds.NODE_APPLIED_SCHEMA,
                "capture_id": capture_id, "freeze_plan_sha256": freeze_sha,
                "round_authorization_sha256": round_auth_sha,
                "round_readiness_sha256": round_readiness_sha,
                "round_number": 1, "node": node, "host": host,
                "boot_id": target["boot_id"], "writer_pid": target["writer_pid"],
                "writer_start_ticks": target["writer_start_ticks"],
                "writer_cgroup_sha256": target["writer_cgroup_sha256"],
                "nft_policy_source_sha256": network_receipt["file_sha256"]["policy.nft"],
                "owned_ruleset_stateless_sha256": network_receipt[
                    "owned_ruleset_stateless_sha256"
                ],
                "nft_applied_at": first,
                "nft_deadline_gate": quarantine_rounds.wrap(gate),
                "network_quarantine_receipt": quarantine_rounds.wrap(network_receipt),
                "network_quarantine_receipt_sha256": sha_value(network_receipt),
                "stable_head": head,
                "authorization_ancestry_proof": quarantine_rounds.wrap(ancestry),
                "persistent_restart_fence_sha256": restart_sha,
            })
        mutation_dispatch = {
            "schema": "arc.recovery.quarantine-mutation-dispatch.v1",
            "capture_id": capture_id,
            "freeze_plan_sha256": freeze_sha,
            "round_number": 1,
            "round_authorization_sha256": round_auth_sha,
            "round_readiness_sha256": round_readiness_sha,
            "live_observation_selection_sha256": observation_selection_sealed[
                "sha256"
            ],
            "live_observation_generation": observation_generation,
            "observation_generation_receipt_sha256": observation_selection[
                "observation_generation_receipt_sha256"
            ],
            "drive_prefreeze_receipt_sha256": observation_selection[
                "drive_prefreeze_receipt_sha256"
            ],
            "targets": [
                {"node": row["node"], "host": row["host"]}
                for row in round_authorization["targets"]
            ],
            "dispatched_at": round_authorization["authorized_at"],
        }
        round_result = {
            "schema": quarantine_rounds.ROUND_RESULT_SCHEMA,
            "capture_id": capture_id, "freeze_plan_sha256": freeze_sha,
            "round_number": 1, "round_authorization_sha256": round_auth_sha,
            "target_readiness": quarantine_rounds.wrap(round_readiness),
            "transitions": [quarantine_rounds.wrap(item) for item in applied_values],
            "mutation_dispatch": quarantine_rounds.wrap(mutation_dispatch),
            "remaining_target_inert_proofs": [],
            "remaining_targets": [], "completed_at": stopped_at,
        }
        generation_ledger = {
            "schema": quarantine_rounds.LEDGER_SCHEMA,
            "capture_id": capture_id, "freeze_plan_sha256": freeze_sha,
            "live_observation_selection_sha256": observation_selection_sealed["sha256"],
            "live_observation_generation": observation_generation,
            "observation_generation_receipt_sha256": observation_selection[
                "observation_generation_receipt_sha256"
            ],
            "drive_prefreeze_receipt_sha256": observation_selection[
                "drive_prefreeze_receipt_sha256"
            ],
            "fleet": [
                {"node": node, "host": host}
                for node, host in rollout.PRODUCTION_FLEET
            ],
            "rounds": [{
                "authorization": quarantine_rounds.wrap(round_authorization),
                "result": quarantine_rounds.wrap(round_result),
            }],
            "first_secured_at": first,
            "all_nodes_secured_at": first,
            "legacy_cutoff_height": max(
                [target_public["legacy_public_max_height"]]
                + [item["stable_head"]["height"] for item in applied_values]
            ),
        }
        authenticated_sealed = sealed(
            authenticated, "fleet", "authenticated-prefence-height-cross-proof"
        )
        observation_selection_sealed = sealed(
            observation_selection, "fleet", "live-observation-selection"
        )
        generation_sealed = sealed(
            generation_ledger, "fleet", "quarantine-generation-ledger"
        )
        challenge_sealed = sealed(
            challenge_value, "fleet", "network-quarantine-challenge"
        )
        stability_nodes = []
        stability_heads = []
        for index, (node, host) in enumerate(rollout.PRODUCTION_FLEET):
            receipt_sha = sha_value(network_receipts_by_node[node])
            stable_head = tuple_at(public_origins[index]["info_after_height"] + 5, index + 40)
            samples = []
            counters = []
            for sample_index in (0, 1):
                status = {
                    "schema": "arc.recovery.legacy-network-quarantine-status.v1",
                    "capture_id": capture_id,
                    "node": node,
                    "freeze_plan_sha256": freeze_sha,
                    "receipt_sha256": receipt_sha,
                    "table": "inet arc_legacy_quarantine_v1",
                    "rule_counters": [],
                    "counter_snapshot_sha256": f"{index + 500:064x}",
                    "owned_ruleset_stateless_sha256": f"{index + 600:064x}",
                    "listener_inventory": [],
                    "loopback_head": {
                        "latest_height": stable_head["height"],
                        "block_hash": stable_head["block_hash"],
                        "state_root": stable_head["state_root"],
                    },
                    "quarantine_policy": {},
                    "active": True,
                    "enabled": True,
                }
                status_sha = sha_value(status)
                counter = 10 + index + sample_index
                sample = {
                    "schema": "arc.recovery.legacy-network-quarantine-stability-sample.v1",
                    "capture_id": capture_id,
                    "node": node,
                    "freeze_plan_sha256": freeze_sha,
                    "challenge": challenge,
                    "sample_index": sample_index,
                    "started_at": (
                        "2026-08-28T12:00:00Z"
                        if sample_index == 0
                        else "2026-08-28T12:02:00Z"
                    ),
                    "completed_at": (
                        "2026-08-28T12:00:01Z"
                        if sample_index == 0
                        else "2026-08-28T12:02:01Z"
                    ),
                    "quarantine_status_before": copy.deepcopy(status),
                    "quarantine_status_before_sha256": status_sha,
                    "quarantine_status_after": copy.deepcopy(status),
                    "quarantine_status_after_sha256": status_sha,
                    "writer": {
                        "pid": 1000 + index,
                        "start_ticks": 2000 + index,
                        "executable_sha256": f"{index + 700:064x}",
                        "argv_sha256": f"{index + 800:064x}",
                        "cgroup_sha256": f"{index + 900:064x}",
                    },
                    "listener_ownership": {
                        "rpc_tcp_9090_ss_sha256": f"{index + 1000:064x}",
                        "p2p_udp_9091_ss_sha256": f"{index + 1100:064x}",
                        "writer_pid": 1000 + index,
                    },
                    "head": {
                        **stable_head,
                        "response_sha256": {
                            "info_before": f"{index + 1200:064x}",
                            "latest": f"{index + 1300:064x}",
                            "exact": f"{index + 1400:064x}",
                            "info_after": f"{index + 1500:064x}",
                        },
                        "stable_attempt": 1,
                    },
                    "output_deny_packets": counter,
                    "ss_sha256": f"{index + 1600:064x}",
                    "global_absence_claimed": False,
                }
                samples.append({"value": sample, "sha256": sha_value(sample)})
                counters.append(counter)
            stability_nodes.append(
                {
                    "node": node,
                    "host": host,
                    "samples": samples,
                    "output_deny_packets": {
                        "sample_0": counters[0],
                        "sample_1": counters[1],
                    },
                }
            )
            stability_heads.append({"node": node, "host": host, "head": stable_head})
        stability = {
            "schema": "arc.recovery.legacy-network-quarantine-stability.v1",
            "source_main_commit": source_commit,
            "freeze_plan_sha256": freeze_sha,
            "capture_id": capture_id,
            "challenge": challenge,
            "interval_seconds": 120,
            "sample_count": 2,
            "started_at": first,
            "completed_at": "2026-08-28T12:02:01Z",
            "monotonic_elapsed_ns": 120_000_000_000,
            "fleet_heads": stability_heads,
            "nodes": stability_nodes,
            "quarantine_generation_ledger_sha256": generation_sealed["sha256"],
            "active_transition_sha256s": [
                {"node": item["node"], "sha256": sha_value(item)}
                for item in applied_values
            ],
            "global_absence_claimed": False,
        }
        stability_sealed = sealed(
            stability, "fleet", "network-quarantine-stability-proof"
        )
        bundle_nodes = []
        retained_by_node = {}
        wrapper_specs = (
            ("stopped_status", "stopped-status"),
            ("network_quarantine_receipt", "network-quarantine-receipt"),
            ("quarantine_status", "quarantine-status"),
            ("quarantine_monitor", "network-quarantine-monitor"),
            ("post_proof_quarantine_status", "post-proof-quarantine-status"),
            ("external_quarantine_proof", "external-quarantine-proof"),
            ("public_cross_proof", "public-cross-proof"),
            ("persisted_head", "persisted-head"),
        )
        for index, (node, host) in enumerate(rollout.PRODUCTION_FLEET):
            identity = {
                "capture_id": capture_id,
                "node": node,
                "freeze_plan_sha256": freeze_sha,
            }
            receipt_sha = sha_value(network_receipts_by_node[node])
            persisted_head = tuple_at(
                public_origins[index]["info_after_height"] + 6, index + 60
            )
            objects = {
                "stopped_status": {
                    "schema": "arc.recovery.offline-stop-status.v1",
                    **identity,
                    "validator_address": value["validators"][index]["address"],
                    "stake": value["validators"][index]["stake"],
                    "stopped": True,
                    "restart_fenced": True,
                    "stop_schema": "arc.recovery.offline-stop.v4",
                    "stop_complete_sha256": f"{index + 20:064x}",
                    "stop_files_sha256": f"{index + 40:064x}",
                },
                "network_quarantine_receipt": network_receipts_by_node[node],
                "quarantine_status": {
                    "schema": "arc.recovery.legacy-network-quarantine-status.v1",
                    **identity,
                    "receipt_sha256": receipt_sha,
                    "owned_ruleset_stateless_sha256": network_receipts_by_node[node][
                        "owned_ruleset_stateless_sha256"
                    ],
                    "active": True,
                    "enabled": True,
                },
                "quarantine_monitor": {
                    "schema": "arc.recovery.legacy-network-quarantine-monitor.v1",
                    **identity,
                    "network_quarantine_receipt_sha256": receipt_sha,
                    "monitor_contract_sha256": f"{index + 1700:064x}",
                    "semantic_interpreter": {
                        "normalized_path": "/usr/bin/python3.12",
                        "sha256": f"{index + 1800:064x}",
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
                    "firewall_loader_inventory": [],
                    "file_sha256": {},
                    "unit": {
                        "name": "arc-legacy-maintenance-fence.service",
                        "active": True,
                        "enabled": True,
                        "continuous_poll_interval_milliseconds": 100,
                        "full_loader_revalidation_interval_seconds": 10,
                    },
                    "legacy_exec_start_pre": {},
                    "incident_latched": False,
                    "continuous_fail_closed": True,
                    "automatic_unfence": False,
                    "global_absence_claimed": False,
                },
                "post_proof_quarantine_status": {
                    "schema": "arc.recovery.legacy-network-quarantine-status.v1",
                    **identity,
                    "receipt_sha256": receipt_sha,
                    "active": True,
                    "enabled": True,
                },
                "external_quarantine_proof": {
                    "schema": "arc.recovery.legacy-network-quarantine-external-proof.v1",
                    **identity,
                    "network_quarantine_receipt_sha256": receipt_sha,
                },
                "public_cross_proof": {
                    "schema": "arc.recovery.legacy-network-quarantine-public-cross-proof.v1",
                    **identity,
                    "network_quarantine_receipt_sha256": receipt_sha,
                },
                "persisted_head": {
                    "schema": "arc.recovery.persisted-legacy-head.v1",
                    **identity,
                    "source_main_commit": source_commit,
                    "source_pair_role": "post-quarantine-final-export",
                    "final_source_capture_sha256": f"{index + 5000:064x}",
                    "selected_source_head": copy.deepcopy(persisted_head),
                    "stop_after_round_receipt_sha256": f"{index + 5100:064x}",
                    "network_quarantine_receipt_sha256": receipt_sha,
                    "state_wal_sha256": f"{index + 5200:064x}",
                    "state_wal_size": 8,
                    "head": persisted_head,
                    "archived_final_wal": {
                        "path": f"/private/archive/{node}/state.wal",
                        "sha256": f"{index + 5300:064x}",
                        "size": 8,
                        "file_identity": {
                            "device": 500 + index,
                            "inode": 600 + index,
                            "size": 8,
                            "mode": 0o100600,
                        },
                        "selected_prefix_bytes": 8,
                        "selected_prefix_sha256": f"{index + 5200:064x}",
                        "post_capture_suffix_bytes": 0,
                        "post_capture_suffix_sha256": None,
                        "post_capture_suffix_classification": "none",
                        "preserved_by": (
                            "complete-content-indexed-stopped-legacy-source-v4"
                        ),
                    },
                    "writer_stopped": True,
                    "restart_barrier_active": True,
                    "network_quarantine_active": True,
                    "global_absence_claimed": False,
                },
            }
            retained_by_node[node] = objects
            bundle_nodes.append(
                {
                    "node": node,
                    "host": host,
                    **{
                        field: sealed(objects[field], node, role)
                        for field, role in wrapper_specs
                    },
                }
            )
        bundle = {
            "schema": "arc.recovery.legacy-maintenance-evidence-bundle.v1",
            "source_main_commit": source_commit,
            "freeze_plan_sha256": freeze_sha,
            "capture_id": capture_id,
            "first_quarantine_started_at": first,
            "all_controlled_stopped_at": stopped_at,
            "challenge": challenge,
            "authenticated_prefence_height_cross_proof": authenticated_sealed,
            "live_observation_selection": observation_selection_sealed,
            "quarantine_generation_ledger": generation_sealed,
            "network_quarantine_challenge": challenge_sealed,
            "quarantine_stability_proof": stability_sealed,
            "nodes": bundle_nodes,
            "object_inventory": inventory,
            "aggregate_root_sha256": sha_value(
                {
                    "schema": "arc.recovery.legacy-maintenance-evidence-inventory.v1",
                    "objects": inventory,
                }
            ),
        }
        bundle_sha = sha_value(bundle)
        boundary_nodes = []
        height_rows = []
        for index, ((node, host), public_origin, authenticated_row) in enumerate(
            zip(rollout.PRODUCTION_FLEET, public_origins, authenticated_nodes)
        ):
            wrappers = bundle_nodes[index]
            public_height = public_origin["info_after_height"]
            initial_height = public_height + 4
            later_height = public_height + 5
            persisted_height = public_height + 6
            boundary_nodes.append(
                {
                    "node": node,
                    "host": host,
                    "origin": f"http://{host}:9090",
                    "public_observation": {
                        "tuple": tuple_at(public_height, index + 1),
                        "evidence_sha256": wrappers["public_cross_proof"]["sha256"],
                    },
                    "authenticated_prefence_proof_sha256": authenticated_row[
                        "proof_sha256"
                    ],
                    "network_quarantine_receipt_sha256": retained_by_node[node][
                        "quarantine_status"
                    ]["receipt_sha256"],
                    "quarantine_status_sha256": wrappers["quarantine_status"]["sha256"],
                    "post_proof_quarantine_status_sha256": wrappers[
                        "post_proof_quarantine_status"
                    ]["sha256"],
                    "external_quarantine_proof_sha256": wrappers[
                        "external_quarantine_proof"
                    ]["sha256"],
                    "public_cross_proof_sha256": wrappers["public_cross_proof"]["sha256"],
                    "initial_post_quarantine_head": {
                        "tuple": tuple_at(initial_height, index + 20),
                        "evidence_sha256": wrappers["quarantine_status"]["sha256"],
                    },
                    "post_quarantine_head": {
                        "tuple": tuple_at(later_height, index + 40),
                        "evidence_sha256": wrappers["public_cross_proof"]["sha256"],
                    },
                    "final_persisted_head": {
                        "tuple": tuple_at(persisted_height, index + 60),
                        "evidence_sha256": wrappers["persisted_head"]["sha256"],
                    },
                }
            )
            proof = authenticated_row["proof"]
            sources = (
                ("public_info_before", public_origin["info_before_height"], public_sha),
                ("public_latest", public_origin["latest_block_height"], public_sha),
                ("public_info_after", public_height, public_sha),
                (
                    "authenticated_info_before",
                    proof["authenticated_info_before_height"],
                    authenticated_row["proof_sha256"],
                ),
                (
                    "authenticated_latest",
                    proof["authenticated_latest_block_height"],
                    authenticated_row["proof_sha256"],
                ),
                (
                    "authenticated_info_after",
                    proof["authenticated_info_after_height"],
                    authenticated_row["proof_sha256"],
                ),
                (
                    "authenticated_conservative_floor",
                    proof["conservative_height_floor"],
                    authenticated_row["proof_sha256"],
                ),
                (
                    "initial_post_quarantine_head",
                    initial_height,
                    wrappers["quarantine_status"]["sha256"],
                ),
                (
                    "public_cross_info_after",
                    public_height,
                    wrappers["public_cross_proof"]["sha256"],
                ),
                (
                    "post_quarantine_head",
                    later_height,
                    wrappers["public_cross_proof"]["sha256"],
                ),
                (
                    "quarantine_stability_sample_0",
                    stability_nodes[index]["samples"][0]["value"]["head"]["height"],
                    stability_nodes[index]["samples"][0]["sha256"],
                ),
                (
                    "quarantine_stability_sample_1",
                    stability_nodes[index]["samples"][1]["value"]["head"]["height"],
                    stability_nodes[index]["samples"][1]["sha256"],
                ),
                (
                    "final_persisted_head",
                    persisted_height,
                    wrappers["persisted_head"]["sha256"],
                ),
            )
            height_rows.extend(
                {
                    "node": node,
                    "label": label,
                    "height": height,
                    "evidence_sha256": root,
                }
                for label, height, root in sources
            )
        cutoff = max(row["height"] for row in height_rows)
        boundary = {
            "schema": "arc.recovery.legacy-maintenance-boundary.v1",
            "source_main_commit": source_commit,
            "freeze_plan_sha256": freeze_sha,
            "capture_id": capture_id,
            "first_quarantine_started_at": first,
            "all_controlled_stopped_at": stopped_at,
            "created_at": "2026-08-28T12:02:03Z",
            "official_origin_scope": {
                "global_absence_claimed": False,
                "origins": [dict(row) for row in rollout.LEGACY_OFFICIAL_ORIGINS],
            },
            "legacy_public_height_receipt": {
                "schema": public["schema"],
                "sha256": public_sha,
                "completed_at": public["completed_at"],
                "observed_max_height": public["legacy_public_max_height"],
            },
            "authenticated_prefence_height_cross_proof_sha256": sha_value(authenticated),
            "legacy_live_observation_selection_sha256": observation_selection_sealed["sha256"],
            "legacy_live_observation_generation": observation_generation,
            "observation_generation_receipt_sha256": observation_selection[
                "observation_generation_receipt_sha256"
            ],
            "drive_prefreeze_receipt_sha256": observation_selection[
                "drive_prefreeze_receipt_sha256"
            ],
            "quarantine_generation_ledger_sha256": generation_sealed["sha256"],
            "legacy_maintenance_evidence_bundle_sha256": bundle_sha,
            "network_quarantine_stability_proof_sha256": stability_sealed["sha256"],
            "network_quarantine_challenge": challenge,
            "tools": {
                "remote_helper_sha256": value["archive"]["remote_helper_sha256"],
                "inspector_binary_sha256": value["artifacts"]["binary"]["sha256"],
                "genesis_sha256": value["artifacts"]["genesis"]["sha256"],
                "validator_public_keys_sha256": value["artifacts"][
                    "validator_public_keys"
                ]["sha256"],
                "legacy_validator_set_sha256": value["artifacts"][
                    "legacy_validator_set"
                ]["sha256"],
                "orchestrator_sha256": value["archive"]["archive_orchestrator_sha256"],
                "rollout_tool_sha256": value["archive"]["rollout_tool_sha256"],
                "rollout_schema_sha256": value["archive"]["rollout_schema_sha256"],
            },
            "nodes": boundary_nodes,
            "evidence_heights": height_rows,
            "observed_cutoff_height": cutoff,
            "continuity_safety_margin": 128,
            "continuity_safety_margin_policy": copy.deepcopy(
                rollout.LEGACY_CONTINUITY_SAFETY_MARGIN_POLICY
            ),
            "legacy_public_max_height": cutoff + 128,
            "global_absence_claimed": False,
            "reopening_policy": copy.deepcopy(rollout.LEGACY_REOPENING_POLICY),
            "late_fork_circuit": copy.deepcopy(rollout.LEGACY_LATE_FORK_CIRCUIT),
            "threat_model": copy.deepcopy(rollout.LEGACY_QUARANTINE_THREAT_MODEL),
        }
        boundary_sha = sha_value(boundary)
        late_fork_source_set = {
            "schema": "arc.recovery.legacy-late-fork-source-set.v1",
            "source_main_commit": source_commit,
            "boundary_sha256": boundary_sha,
            "observed_cutoff_height": cutoff,
            "official_origins": [
                {"name": row["node"], "host": row["host"], "origin": row["origin"]}
                for row in rollout.LEGACY_OFFICIAL_ORIGINS
            ],
            "monitored_retired_origins": [
                {"name": row["node"], "origin": row["origin"]}
                for row in rollout.LEGACY_OFFICIAL_ORIGINS
            ],
            "monitored_community_origins": [],
            "poll_interval_seconds": 30,
            "max_staleness_seconds": 90,
            "validation_mode": (
                "capture-bound-retirement-tripwire-offline-validation-required"
            ),
            "validation_tool_sha256": value["artifacts"][
                "legacy_late_fork_interlock_tool"
            ]["sha256"],
            "global_absence_claimed": False,
        }
        late_fork_source_set_sha = sha_value(late_fork_source_set)
        offline = {
            "schema": "arc.validator-vault.offline-stop-evidence.v2",
            "source_main_commit": source_commit,
            "freeze_plan_sha256": freeze_sha,
            "freeze_plan_sidecar_sha256": "8" * 64,
            "capture_id": capture_id,
            "remote_helper_sha256": value["archive"]["remote_helper_sha256"],
            "remote_helper_path": value["provenance"]["offline_stop_verification"][
                "remote_helper_path"
            ],
            "first_quarantine_started_at": first,
            "all_controlled_stopped_at": stopped_at,
            "legacy_height_cross_proof": authenticated,
            "legacy_maintenance_boundary": boundary,
            "legacy_maintenance_boundary_sha256": boundary_sha,
            "legacy_maintenance_evidence_bundle_sha256": bundle_sha,
            "quarantine_generation_ledger_sha256": generation_sealed["sha256"],
            "legacy_live_observation_selection_sha256": observation_selection_sealed[
                "sha256"
            ],
            "legacy_live_observation_generation": observation_generation,
            "observation_generation_receipt_sha256": observation_selection[
                "observation_generation_receipt_sha256"
            ],
            "drive_prefreeze_receipt_sha256": observation_selection[
                "drive_prefreeze_receipt_sha256"
            ],
            "nodes": [
                {
                    "node": node,
                    "host": host,
                    "validator_address": value["validators"][index]["address"],
                    "stake": value["validators"][index]["stake"],
                    "stop_complete_sha256": f"{index + 20:064x}",
                    "stop_files_sha256": f"{index + 40:064x}",
                    "stopped_status_sha256": sha_value(
                        retained_by_node[node]["stopped_status"]
                    ),
                    "stopped_status_argv_sha256": f"{index + 60:064x}",
                }
                for index, (node, host) in enumerate(rollout.PRODUCTION_FLEET)
            ],
        }
        payloads = {
            "freeze_plan": canonical(freeze),
            "legacy_public_height_receipt": canonical(public),
            "legacy_maintenance_evidence_bundle": canonical(bundle),
            "legacy_maintenance_boundary": canonical(boundary),
            "legacy_late_fork_source_set": canonical(late_fork_source_set),
            "offline_stop_evidence": canonical(offline),
        }
        object_paths = {
            "legacy_maintenance_evidence_bundle": "legacy-maintenance-evidence-bundle.json",
            "legacy_maintenance_boundary": "legacy-maintenance-boundary.json",
            "legacy_late_fork_source_set": "legacy-late-fork-source-set.json",
            "offline_stop_evidence": "offline-stop-evidence.json",
        }
        stage_rows = {
            "freeze_plan": {"path": "freeze-plan.json", "sha256": freeze_sha},
            "legacy_public_height_receipt": {
                "path": "legacy-public-height.json",
                "sha256": public_sha,
            },
        }
        for name, path in object_paths.items():
            root = hashlib.sha256(payloads[name]).hexdigest()
            stage_rows[name] = {"path": path, "sha256": root}
            sidecar_name = (
                "offline_stop_sidecar"
                if name == "offline_stop_evidence"
                else name + "_sidecar"
            )
            payloads[sidecar_name] = f"{root}  {path}\n".encode("ascii")
        artifact_names = (
            "legacy_maintenance_evidence_bundle",
            "legacy_maintenance_boundary",
            "legacy_late_fork_source_set",
            "offline_stop_evidence",
        )
        for name in artifact_names:
            value["artifacts"][name]["sha256"] = stage_rows[name]["sha256"]
        for artifact_name, payload_name in (
            (
                "legacy_maintenance_evidence_bundle_sidecar",
                "legacy_maintenance_evidence_bundle_sidecar",
            ),
            ("legacy_maintenance_boundary_sidecar", "legacy_maintenance_boundary_sidecar"),
            ("legacy_late_fork_source_set_sidecar", "legacy_late_fork_source_set_sidecar"),
            ("offline_stop_evidence_sidecar", "offline_stop_sidecar"),
        ):
            value["artifacts"][artifact_name]["sha256"] = hashlib.sha256(
                payloads[payload_name]
            ).hexdigest()
        value["artifacts"]["legacy_public_height_receipt"]["sha256"] = public_sha
        value["provenance"]["offline_stop_verification"][
            "offline_stop_evidence_sha256"
        ] = stage_rows["offline_stop_evidence"]["sha256"]
        value["chain"].update(
            {
                "legacy_maintenance_evidence_bundle_sha256": bundle_sha,
                "legacy_maintenance_boundary_sha256": boundary_sha,
                "legacy_late_fork_source_set_sha256": late_fork_source_set_sha,
                "legacy_observed_cutoff_height": cutoff,
                "legacy_continuity_safety_margin": 128,
                "legacy_public_max_height": cutoff + 128,
                "legacy_global_absence_claimed": False,
                "legacy_official_origins": [
                    dict(row) for row in rollout.LEGACY_OFFICIAL_ORIGINS
                ],
                "legacy_reopening_policy": copy.deepcopy(
                    rollout.LEGACY_REOPENING_POLICY
                ),
                "legacy_late_fork_circuit": copy.deepcopy(
                    rollout.LEGACY_LATE_FORK_CIRCUIT
                ),
                "legacy_quarantine_threat_model": copy.deepcopy(
                    rollout.LEGACY_QUARANTINE_THREAT_MODEL
                ),
            }
        )
        return value, stage_rows, payloads

    @staticmethod
    def repack_maintenance_stage(value, rows, payloads, bundle, boundary):
        """Recompute every outer seal after a semantic union-fixture rewrite."""

        canonical = rollout.canonical_bytes
        sha_value = lambda item: hashlib.sha256(canonical(item)).hexdigest()

        wrapper_specs = (
            ("stopped_status", "stopped-status"),
            ("network_quarantine_receipt", "network-quarantine-receipt"),
            ("quarantine_status", "quarantine-status"),
            ("quarantine_monitor", "network-quarantine-monitor"),
            ("post_proof_quarantine_status", "post-proof-quarantine-status"),
            ("external_quarantine_proof", "external-quarantine-proof"),
            ("public_cross_proof", "public-cross-proof"),
            ("persisted_head", "persisted-head"),
        )

        inventory = []

        def reseal(wrapper, node, role):
            inner = wrapper["value"]
            root = sha_value(inner)
            inventory.append(
                {
                    "node": node,
                    "role": role,
                    "sha256": root,
                    "size": len(canonical(inner)),
                }
            )
            return {"value": inner, "sha256": root}

        for field, role in (
            ("authenticated_prefence_height_cross_proof", "authenticated-prefence-height-cross-proof"),
            ("live_observation_selection", "live-observation-selection"),
            ("quarantine_generation_ledger", "quarantine-generation-ledger"),
            ("network_quarantine_challenge", "network-quarantine-challenge"),
            ("quarantine_stability_proof", "network-quarantine-stability-proof"),
        ):
            bundle[field] = reseal(bundle[field], "fleet", role)
        for node_row in bundle["nodes"]:
            node = node_row["node"]
            specs = (
                (
                    ("transition_receipt", "transition-receipt"),
                    ("current_status", "current-status"),
                    ("persisted_head", "persisted-head"),
                )
                if node_row.get("transition_kind")
                == quarantine_rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
                else wrapper_specs
            )
            for field, role in specs:
                node_row[field] = reseal(node_row[field], node, role)
        bundle["object_inventory"] = inventory
        bundle["aggregate_root_sha256"] = sha_value(
            {
                "schema": "arc.recovery.legacy-maintenance-evidence-inventory.v1",
                "objects": inventory,
            }
        )
        bundle_sha = sha_value(bundle)
        boundary["quarantine_generation_ledger_sha256"] = bundle[
            "quarantine_generation_ledger"
        ]["sha256"]
        selection = bundle["live_observation_selection"]
        boundary["legacy_live_observation_selection_sha256"] = selection["sha256"]
        boundary["legacy_live_observation_generation"] = selection["value"][
            "observation_generation"
        ]
        boundary["observation_generation_receipt_sha256"] = selection["value"][
            "observation_generation_receipt_sha256"
        ]
        boundary["drive_prefreeze_receipt_sha256"] = selection["value"][
            "drive_prefreeze_receipt_sha256"
        ]
        boundary["network_quarantine_stability_proof_sha256"] = bundle[
            "quarantine_stability_proof"
        ]["sha256"]
        boundary["legacy_maintenance_evidence_bundle_sha256"] = bundle_sha
        boundary["observed_cutoff_height"] = max(
            row["height"] for row in boundary["evidence_heights"]
        )
        boundary["legacy_public_max_height"] = (
            boundary["observed_cutoff_height"] + 128
        )
        boundary_sha = sha_value(boundary)

        late_fork = copy.deepcopy(
            json.loads(payloads["legacy_late_fork_source_set"])
        )
        late_fork["boundary_sha256"] = boundary_sha
        late_fork["observed_cutoff_height"] = boundary["observed_cutoff_height"]
        late_fork_sha = sha_value(late_fork)

        offline = copy.deepcopy(json.loads(payloads["offline_stop_evidence"]))
        offline["legacy_maintenance_boundary"] = boundary
        offline["legacy_maintenance_boundary_sha256"] = boundary_sha
        offline["legacy_maintenance_evidence_bundle_sha256"] = bundle_sha
        offline["quarantine_generation_ledger_sha256"] = bundle[
            "quarantine_generation_ledger"
        ]["sha256"]
        offline["legacy_live_observation_selection_sha256"] = selection["sha256"]
        offline["legacy_live_observation_generation"] = selection["value"][
            "observation_generation"
        ]
        offline["observation_generation_receipt_sha256"] = selection["value"][
            "observation_generation_receipt_sha256"
        ]
        offline["drive_prefreeze_receipt_sha256"] = selection["value"][
            "drive_prefreeze_receipt_sha256"
        ]
        offline_sha = sha_value(offline)

        payloads = dict(payloads)
        objects = {
            "legacy_maintenance_evidence_bundle": (
                bundle,
                "legacy-maintenance-evidence-bundle.json",
            ),
            "legacy_maintenance_boundary": (
                boundary,
                "legacy-maintenance-boundary.json",
            ),
            "legacy_late_fork_source_set": (
                late_fork,
                "legacy-late-fork-source-set.json",
            ),
            "offline_stop_evidence": (offline, "offline-stop-evidence.json"),
        }
        object_roots = {
            "legacy_maintenance_evidence_bundle": bundle_sha,
            "legacy_maintenance_boundary": boundary_sha,
            "legacy_late_fork_source_set": late_fork_sha,
            "offline_stop_evidence": offline_sha,
        }
        sidecar_payload_keys = {
            "legacy_maintenance_evidence_bundle": (
                "legacy_maintenance_evidence_bundle_sidecar",
                "legacy_maintenance_evidence_bundle_sidecar",
            ),
            "legacy_maintenance_boundary": (
                "legacy_maintenance_boundary_sidecar",
                "legacy_maintenance_boundary_sidecar",
            ),
            "legacy_late_fork_source_set": (
                "legacy_late_fork_source_set_sidecar",
                "legacy_late_fork_source_set_sidecar",
            ),
            "offline_stop_evidence": (
                "offline_stop_sidecar",
                "offline_stop_evidence_sidecar",
            ),
        }
        rows = copy.deepcopy(rows)
        for name, (inner, filename) in objects.items():
            payloads[name] = canonical(inner)
            root = object_roots[name]
            rows[name]["sha256"] = root
            value["artifacts"][name]["sha256"] = root
            payload_key, artifact_key = sidecar_payload_keys[name]
            payloads[payload_key] = f"{root}  {filename}\n".encode("ascii")
            value["artifacts"][artifact_key]["sha256"] = hashlib.sha256(
                payloads[payload_key]
            ).hexdigest()

        value["provenance"]["offline_stop_verification"][
            "offline_stop_evidence_sha256"
        ] = offline_sha
        value["chain"].update(
            {
                "legacy_maintenance_evidence_bundle_sha256": bundle_sha,
                "legacy_maintenance_boundary_sha256": boundary_sha,
                "legacy_late_fork_source_set_sha256": late_fork_sha,
                "legacy_observed_cutoff_height": boundary[
                    "observed_cutoff_height"
                ],
                "legacy_public_max_height": boundary["legacy_public_max_height"],
            }
        )
        return value, rows, payloads

    def union_maintenance_stage_fixture(self, stopped_names):
        value, rows, payloads = self.maintenance_stage_fixture()
        bundle = copy.deepcopy(
            json.loads(payloads["legacy_maintenance_evidence_bundle"])
        )
        boundary = copy.deepcopy(json.loads(payloads["legacy_maintenance_boundary"]))
        generation = bundle["quarantine_generation_ledger"]["value"]
        result_wrapper = generation["rounds"][0]["result"]
        result_value = result_wrapper["value"]
        stopped_names = set(stopped_names)
        stopped_artifacts = {}

        for index, transition_wrapper in enumerate(result_value["transitions"]):
            active_transition = transition_wrapper["value"]
            node = active_transition["node"]
            if node not in stopped_names:
                continue
            stable_head = copy.deepcopy(active_transition["stable_head"])
            fence_value = {
                "schema": "arc.recovery.fixture-persistent-restart-fence.v1",
                "node": node,
                "host": active_transition["host"],
            }
            fence_wrapper = quarantine_rounds.wrap(fence_value)
            live_capture_sha = f"{index + 6100:064x}"
            persisted = {
                "schema": quarantine_rounds.PERSISTED_STOPPED_SCHEMA,
                "node": node,
                "host": active_transition["host"],
                "head": stable_head,
                "source_pair_role": "preauthorization-boundary",
                "live_source_capture_sha256": live_capture_sha,
                "source_inputs": {
                    "source_pair_role": "preauthorization-boundary",
                    "live_source_capture_sha256": live_capture_sha,
                },
            }
            persisted_wrapper = quarantine_rounds.wrap(persisted)
            transition = {
                "schema": quarantine_rounds.NODE_STOPPED_PRECOMMIT_SCHEMA,
                "node": node,
                "host": active_transition["host"],
                "stable_head": stable_head,
                "persistent_restart_fence": fence_wrapper,
                "persisted_head": persisted_wrapper,
            }
            transition_wrapper = quarantine_rounds.wrap(transition)
            result_value["transitions"][index] = transition_wrapper
            current_status = {
                "stable_head": stable_head,
                "persistent_restart_fence_sha256": fence_wrapper["sha256"],
            }
            current_wrapper = quarantine_rounds.wrap(current_status)
            stopped_artifacts[node] = {
                "transition": transition_wrapper,
                "current": current_wrapper,
                "persisted": persisted_wrapper,
                "fence_sha256": fence_wrapper["sha256"],
            }
        generation["rounds"][0]["result"] = quarantine_rounds.wrap(result_value)
        bundle["quarantine_generation_ledger"] = quarantine_rounds.wrap(generation)

        transition_wrappers = {
            wrapper["value"]["node"]: wrapper
            for wrapper in result_value["transitions"]
        }
        active_names = [
            node for node, _host in rollout.PRODUCTION_FLEET
            if node not in stopped_names
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
            {"node": node, "sha256": transition_wrappers[node]["sha256"]}
            for node in active_names
        ]
        if active_names:
            stability["interval_seconds"] = 120
            stability["sample_count"] = 2
            stability["monotonic_elapsed_ns"] = 120_000_000_000
        else:
            stability["interval_seconds"] = 0
            stability["sample_count"] = 0
            stability["monotonic_elapsed_ns"] = 0
        bundle["quarantine_stability_proof"] = quarantine_rounds.wrap(stability)

        bundle_nodes = {row["node"]: row for row in bundle["nodes"]}
        for node in stopped_names:
            host = dict(rollout.PRODUCTION_FLEET)[node]
            artifacts = stopped_artifacts[node]
            bundle_nodes[node] = {
                "node": node,
                "host": host,
                "transition_kind": (
                    quarantine_rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
                ),
                "transition_receipt": artifacts["transition"],
                "current_status": artifacts["current"],
                "persisted_head": artifacts["persisted"],
            }
        bundle["nodes"] = [
            bundle_nodes[node] for node, _host in rollout.PRODUCTION_FLEET
        ]

        boundary_nodes = {row["node"]: row for row in boundary["nodes"]}
        for node in stopped_names:
            prior = boundary_nodes[node]
            artifacts = stopped_artifacts[node]
            transition = artifacts["transition"]
            persisted = artifacts["persisted"]
            stable_head = transition["value"]["stable_head"]
            boundary_nodes[node] = {
                "node": node,
                "host": prior["host"],
                "origin": prior["origin"],
                "transition_kind": (
                    quarantine_rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
                ),
                "authenticated_prefence_proof_sha256": prior[
                    "authenticated_prefence_proof_sha256"
                ],
                "transition_receipt_sha256": transition["sha256"],
                "current_status_sha256": artifacts["current"]["sha256"],
                "persistent_restart_fence_sha256": artifacts["fence_sha256"],
                "stable_head": {
                    "tuple": stable_head,
                    "evidence_sha256": transition["sha256"],
                },
                "final_persisted_head": {
                    "tuple": stable_head,
                    "evidence_sha256": persisted["sha256"],
                },
            }
        boundary["nodes"] = [
            boundary_nodes[node] for node, _host in rollout.PRODUCTION_FLEET
        ]

        rows_by_node = {}
        for row in boundary["evidence_heights"]:
            rows_by_node.setdefault(row["node"], {})[row["label"]] = row
        rewritten_heights = []
        common_labels = (
            "public_info_before", "public_latest", "public_info_after",
            "authenticated_info_before", "authenticated_latest",
            "authenticated_info_after", "authenticated_conservative_floor",
        )
        for node, _host in rollout.PRODUCTION_FLEET:
            if node not in stopped_names:
                rewritten_heights.extend(rows_by_node[node].values())
                continue
            rewritten_heights.extend(
                copy.deepcopy(rows_by_node[node][label]) for label in common_labels
            )
            artifacts = stopped_artifacts[node]
            stable_height = artifacts["transition"]["value"]["stable_head"]["height"]
            rewritten_heights.extend(
                (
                    {
                        "node": node,
                        "label": "transition_stable_head",
                        "height": stable_height,
                        "evidence_sha256": artifacts["transition"]["sha256"],
                    },
                    {
                        "node": node,
                        "label": "final_persisted_head",
                        "height": stable_height,
                        "evidence_sha256": artifacts["persisted"]["sha256"],
                    },
                )
            )
        boundary["evidence_heights"] = rewritten_heights
        offline = copy.deepcopy(json.loads(payloads["offline_stop_evidence"]))
        offline_nodes = {row["node"]: row for row in offline["nodes"]}
        remote_nodes = {
            row["node"]: row
            for row in value["provenance"]["offline_stop_verification"]["nodes"]
        }
        for node in stopped_names:
            host = dict(rollout.PRODUCTION_FLEET)[node]
            artifacts = stopped_artifacts[node]
            offline_nodes[node] = {
                "node": node,
                "host": host,
                "transition_kind": (
                    quarantine_rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
                ),
                "transition_receipt_sha256": artifacts["transition"]["sha256"],
                "current_status_sha256": artifacts["current"]["sha256"],
                "persisted_head_sha256": artifacts["persisted"]["sha256"],
            }
            challenged = {
                "schema": (
                    "arc.recovery.quarantine-persistently-stopped-"
                    "challenged-status.v1"
                ),
                "capture_id": value["archive"]["capture_id"],
                "freeze_plan_sha256": value["archive"]["freeze_plan_sha256"],
                "node": node,
                "host": host,
                "transition_kind": (
                    quarantine_rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
                ),
                "transition_receipt": artifacts["transition"],
                "current_status": artifacts["current"],
                "challenge": value["provenance"]["offline_stop_verification"][
                    "challenge"
                ],
            }
            remote_nodes[node]["status"] = challenged
            remote_nodes[node]["status_sha256"] = hashlib.sha256(
                rollout.canonical_bytes(challenged)
            ).hexdigest()
        offline["nodes"] = [
            offline_nodes[node] for node, _host in rollout.PRODUCTION_FLEET
        ]
        payloads = dict(payloads)
        payloads["offline_stop_evidence"] = rollout.canonical_bytes(offline)
        return self.repack_maintenance_stage(value, rows, payloads, bundle, boundary)

    def test_manifest_requires_exactly_six_and_h_plus_one(self) -> None:
        valid = self.fixture()
        self.assertIs(rollout.validate_manifest(valid), valid)
        five = copy.deepcopy(valid)
        five["validators"].pop()
        with self.assertRaisesRegex(rollout.RolloutError, "exactly 6"):
            rollout.validate_manifest(five)
        wrong_boundary = copy.deepcopy(valid)
        wrong_boundary["chain"]["transition_height"] = 102
        with self.assertRaisesRegex(rollout.RolloutError, r"exactly source_height \+ 1"):
            rollout.validate_manifest(wrong_boundary)

    def test_production_manifest_requires_sealed_real_inference_reward_probe(self) -> None:
        valid = self.fixture(production=True)
        self.assertIs(rollout.validate_manifest(valid), valid)

        policy_only = copy.deepcopy(valid)
        policy_only["checks"]["reward"] = {
            "mode": "policy",
            "expect_protocol_active": False,
            "expect_issuance_ready": False,
        }
        with self.assertRaisesRegex(
            rollout.RolloutError, "production requires receipt reward mode"
        ):
            rollout.validate_manifest(policy_only)

        fixed_receipts = self.fixture(production=True, reward_receipt=True)
        with self.assertRaisesRegex(
            rollout.RolloutError, "production requires the sealed real-inference probe"
        ):
            rollout.validate_manifest(fixed_receipts)

        multi_token = copy.deepcopy(valid)
        multi_token["checks"]["reward"]["probe_argv"][-1] = "2"
        with self.assertRaisesRegex(
            rollout.RolloutError, "exactly bind the staged probe and one-token canary"
        ):
            rollout.validate_manifest(multi_token)

        foreign_probe = copy.deepcopy(valid)
        foreign_probe["checks"]["reward"]["probe_sha256"] = "f" * 64
        with self.assertRaisesRegex(
            rollout.RolloutError, "must equal the staged reward-probe artifact"
        ):
            rollout.validate_manifest(foreign_probe)

    def test_manifest_seals_a_legacy_public_height_not_below_source(self) -> None:
        valid = self.fixture()
        self.assertEqual(valid["chain"]["legacy_public_max_height"], 110)

        missing = copy.deepcopy(valid)
        missing["chain"].pop("legacy_public_max_height")
        with self.assertRaisesRegex(rollout.RolloutError, "legacy_public_max_height"):
            rollout.validate_manifest(missing)

        below_source = copy.deepcopy(valid)
        below_source["chain"]["legacy_public_max_height"] = 99
        with self.assertRaisesRegex(rollout.RolloutError, "at least source_height"):
            rollout.validate_manifest(below_source)

    def test_production_manifest_seals_exact_legacy_continuity_projection(self) -> None:
        valid = self.fixture(production=True)
        self.assertIs(rollout.validate_manifest(valid), valid)
        schema = json.loads(
            MODULE_PATH.with_name("recovery-manifest.schema.json").read_text(
                encoding="utf-8"
            )
        )
        production = schema["allOf"][0]["then"]["properties"]
        self.assertEqual(
            set(production["chain"]["required"]),
            {
                "legacy_maintenance_evidence_bundle_sha256",
                "legacy_maintenance_boundary_sha256",
                "legacy_late_fork_source_set_sha256",
                "legacy_observed_cutoff_height",
                "legacy_continuity_safety_margin",
                "legacy_global_absence_claimed",
                "legacy_official_origins",
                "legacy_reopening_policy",
                "legacy_late_fork_circuit",
                "legacy_quarantine_threat_model",
            },
        )
        self.assertTrue(
            {
                "legacy_maintenance_evidence_bundle",
                "legacy_maintenance_evidence_bundle_sidecar",
                "legacy_maintenance_boundary",
                "legacy_maintenance_boundary_sidecar",
                "legacy_late_fork_source_set",
                "legacy_late_fork_source_set_sidecar",
                "legacy_late_fork_interlock_tool",
                "offline_stop_evidence_sidecar",
            }.issubset(production["artifacts"]["required"])
        )
        mutations = (
            (
                "legacy_continuity_safety_margin",
                127,
                "continuity_safety_margin must be exactly 128",
            ),
            (
                "legacy_public_max_height",
                valid["chain"]["legacy_public_max_height"] + 1,
                r"observed_cutoff_height \+ 128",
            ),
            ("legacy_global_absence_claimed", True, "must be false"),
            (
                "legacy_official_origins",
                list(reversed(valid["chain"]["legacy_official_origins"])),
                "exact ordered six",
            ),
            (
                "legacy_reopening_policy",
                {**valid["chain"]["legacy_reopening_policy"], "required_validator_count": 5},
                "reopening_policy",
            ),
            (
                "legacy_late_fork_circuit",
                {**valid["chain"]["legacy_late_fork_circuit"], "rewrite_v3_history_allowed": True},
                "late_fork_circuit",
            ),
            (
                "legacy_quarantine_threat_model",
                {**valid["chain"]["legacy_quarantine_threat_model"], "hostile_root_containment_claimed": True},
                "quarantine_threat_model",
            ),
        )
        for field, replacement, message in mutations:
            hostile = copy.deepcopy(valid)
            hostile["chain"][field] = replacement
            with self.subTest(field=field), self.assertRaisesRegex(
                rollout.RolloutError, message
            ):
                rollout.validate_manifest(hostile)

        missing_artifact = copy.deepcopy(valid)
        missing_artifact["artifacts"].pop("legacy_maintenance_evidence_bundle_sidecar")
        with self.assertRaisesRegex(
            rollout.RolloutError, "legacy_maintenance_evidence_bundle_sidecar"
        ):
            rollout.validate_manifest(missing_artifact)

    def test_legacy_maintenance_stage_cross_binds_exact_canonical_objects(self) -> None:
        value, rows, payloads = self.maintenance_stage_fixture()
        rollout.verify_legacy_maintenance_stage_payloads(value, rows, payloads)

        bad_sidecar = dict(payloads)
        bad_sidecar["legacy_maintenance_boundary_sidecar"] = b"0" * 64 + b"  wrong.json\n"
        with self.assertRaisesRegex(rollout.RolloutError, "boundary sidecar"):
            rollout.verify_legacy_maintenance_stage_payloads(value, rows, bad_sidecar)

        bad_bundle = copy.deepcopy(
            json.loads(payloads["legacy_maintenance_evidence_bundle"])
        )
        bad_bundle["nodes"][0]["persisted_head"]["value"]["writer_stopped"] = False
        hostile_payloads = dict(payloads)
        hostile_payloads["legacy_maintenance_evidence_bundle"] = rollout.canonical_bytes(
            bad_bundle
        )
        with self.assertRaisesRegex(rollout.RolloutError, "hash is not reproducible"):
            rollout.verify_legacy_maintenance_stage_payloads(value, rows, hostile_payloads)

        bad_offline = copy.deepcopy(json.loads(payloads["offline_stop_evidence"]))
        bad_offline["legacy_maintenance_boundary"]["global_absence_claimed"] = True
        hostile_payloads = dict(payloads)
        hostile_payloads["offline_stop_evidence"] = rollout.canonical_bytes(bad_offline)
        with self.assertRaisesRegex(rollout.RolloutError, "exact maintenance bundle/boundary"):
            rollout.verify_legacy_maintenance_stage_payloads(value, rows, hostile_payloads)

    def test_live_observation_selection_rejects_semantically_rehashed_forgery(self) -> None:
        value, _rows, payloads = self.maintenance_stage_fixture()
        bundle = json.loads(payloads["legacy_maintenance_evidence_bundle"])
        original = bundle["live_observation_selection"]["value"]
        expected = {
            "source_main_commit": original["source_main_commit"],
            "freeze_plan_sha256": original["freeze_plan_sha256"],
            "capture_id": original["capture_id"],
        }

        recovered_capacity = copy.deepcopy(original)
        recovered_drive = recovered_capacity["observation_generation_receipt"][
            "drive_prefreeze_receipt"
        ]
        recovered_drive["value"]["available_bytes_after"] = recovered_drive["value"][
            "available_bytes_before"
        ]
        recovered_drive["sha256"] = rollout.sha256_bytes(
            rollout.canonical_bytes(recovered_drive["value"])
        )
        recovered_capacity["drive_prefreeze_receipt_sha256"] = recovered_drive[
            "sha256"
        ]
        recovered_capacity["observation_generation_receipt_sha256"] = (
            rollout.sha256_bytes(
                rollout.canonical_bytes(
                    recovered_capacity["observation_generation_receipt"]
                )
            )
        )
        rollout.validate_live_observation_selection(recovered_capacity, **expected)

        def rejected(mutator) -> None:
            selection = copy.deepcopy(original)
            mutator(selection)
            generation = selection["observation_generation_receipt"]
            drive = generation["drive_prefreeze_receipt"]
            drive["sha256"] = rollout.sha256_bytes(rollout.canonical_bytes(drive["value"]))
            selection["drive_prefreeze_receipt_sha256"] = drive["sha256"]
            selection["observation_generation_receipt_sha256"] = rollout.sha256_bytes(
                rollout.canonical_bytes(generation)
            )
            with self.assertRaises(rollout.RolloutError):
                rollout.validate_live_observation_selection(selection, **expected)

        rejected(lambda selection: selection["observation_generation_receipt"].__setitem__(
            "schema", "arc.recovery.legacy-live-observation-generation.v0"
        ))
        rejected(lambda selection: selection["observation_generation_receipt"].__setitem__(
            "capture_id", "0" * 64
        ))
        rejected(lambda selection: selection["observation_generation_receipt"]
                 ["drive_prefreeze_receipt"]["value"].__setitem__("mode", "preflight"))
        rejected(lambda selection: selection["observation_generation_receipt"]
                 ["drive_prefreeze_receipt"]["value"].__setitem__("canary_verified", False))
        rejected(lambda selection: selection["nodes"].pop())
        rejected(lambda selection: selection["nodes"][0].__setitem__("root_sha256", "bad"))

    def verify_union_maintenance_stage(self, value, rows, payloads) -> None:
        bundle = json.loads(payloads["legacy_maintenance_evidence_bundle"])
        selection = bundle["live_observation_selection"]
        generation_state = {
            "capture_id": value["archive"]["capture_id"],
            "freeze_plan_sha256": value["archive"]["freeze_plan_sha256"],
            "first_secured_at": dt.datetime(
                2026, 8, 28, 12, 0, 0, tzinfo=dt.timezone.utc
            ),
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
        observed = dt.datetime(
            2026, 8, 28, 12, 2, 2, tzinfo=dt.timezone.utc
        )
        with (
            mock.patch.object(
                rollout.quarantine_rounds,
                "validate_generation_ledger",
                return_value=generation_state,
            ),
            mock.patch.object(
                rollout.quarantine_rounds,
                "validate_prior_fenced_status",
                return_value=observed,
            ),
        ):
            rollout.verify_legacy_maintenance_stage_payloads(value, rows, payloads)

    def test_mixed_active_stopped_union_uses_exact_active_subset_and_stopped_evidence(
        self,
    ) -> None:
        stopped = {"lhr", "nrt", "sgp"}
        value, rows, payloads = self.union_maintenance_stage_fixture(stopped)
        self.verify_union_maintenance_stage(value, rows, payloads)

        bundle = json.loads(payloads["legacy_maintenance_evidence_bundle"])
        stability = bundle["quarantine_stability_proof"]["value"]
        self.assertEqual(
            [row["node"] for row in stability["nodes"]],
            ["nyc", "lax", "ams"],
        )
        self.assertEqual(
            [row["node"] for row in stability["active_transition_sha256s"]],
            ["nyc", "lax", "ams"],
        )
        by_node = {row["node"]: row for row in bundle["nodes"]}
        self.assertEqual(
            set(by_node["lhr"]),
            {
                "node", "host", "transition_kind", "transition_receipt",
                "current_status", "persisted_head",
            },
        )
        self.assertNotIn("transition_kind", by_node["nyc"])

    def test_all_stopped_union_is_explicit_zero_sample_non_stability(self) -> None:
        stopped = {node for node, _host in rollout.PRODUCTION_FLEET}
        value, rows, payloads = self.union_maintenance_stage_fixture(stopped)
        self.verify_union_maintenance_stage(value, rows, payloads)

        bundle = json.loads(payloads["legacy_maintenance_evidence_bundle"])
        stability = bundle["quarantine_stability_proof"]["value"]
        self.assertEqual(stability["nodes"], [])
        self.assertEqual(stability["fleet_heads"], [])
        self.assertEqual(stability["active_transition_sha256s"], [])
        self.assertEqual(stability["interval_seconds"], 0)
        self.assertEqual(stability["sample_count"], 0)
        self.assertEqual(stability["monotonic_elapsed_ns"], 0)

    def test_union_rejects_swapped_transition_evidence_and_stopped_stability_claim(
        self,
    ) -> None:
        value, rows, payloads = self.union_maintenance_stage_fixture(
            {"lhr", "nrt", "sgp"}
        )
        bundle = json.loads(payloads["legacy_maintenance_evidence_bundle"])
        boundary = json.loads(payloads["legacy_maintenance_boundary"])
        stopped_nodes = [
            row for row in bundle["nodes"]
            if row.get("transition_kind")
            == quarantine_rounds.STOPPED_PRECOMMIT_TRANSITION_KIND
        ]
        stopped_nodes[0]["transition_receipt"], stopped_nodes[1]["transition_receipt"] = (
            stopped_nodes[1]["transition_receipt"],
            stopped_nodes[0]["transition_receipt"],
        )
        hostile = self.repack_maintenance_stage(
            value, rows, payloads, bundle, boundary
        )
        with self.assertRaisesRegex(
            rollout.RolloutError, "does not byte-match the generation ledger"
        ):
            self.verify_union_maintenance_stage(*hostile)

        value, rows, payloads = self.union_maintenance_stage_fixture(
            {node for node, _host in rollout.PRODUCTION_FLEET}
        )
        bundle = json.loads(payloads["legacy_maintenance_evidence_bundle"])
        boundary = json.loads(payloads["legacy_maintenance_boundary"])
        bundle["quarantine_stability_proof"]["value"]["interval_seconds"] = 120
        hostile = self.repack_maintenance_stage(
            value, rows, payloads, bundle, boundary
        )
        with self.assertRaisesRegex(
            rollout.RolloutError, "must not claim an active stability sample"
        ):
            self.verify_union_maintenance_stage(*hostile)

    def test_legacy_maintenance_stage_rejects_omitted_high_authenticated_height(self) -> None:
        value, rows, payloads = self.maintenance_stage_fixture()
        boundary = copy.deepcopy(json.loads(payloads["legacy_maintenance_boundary"]))
        target = next(
            row
            for row in boundary["evidence_heights"]
            if row["node"] == "sgp" and row["label"] == "authenticated_info_after"
        )
        target["height"] -= 50
        # Recompute the attacker's claimed cutoff/ceiling and all outer hashes;
        # the retained authenticated proof must still make this fail closed.
        boundary["observed_cutoff_height"] = max(
            row["height"] for row in boundary["evidence_heights"]
        )
        boundary["legacy_public_max_height"] = boundary["observed_cutoff_height"] + 128
        hostile_boundary = rollout.canonical_bytes(boundary)
        hostile_sha = hashlib.sha256(hostile_boundary).hexdigest()
        hostile_value = copy.deepcopy(value)
        hostile_value["artifacts"]["legacy_maintenance_boundary"]["sha256"] = hostile_sha
        hostile_value["chain"].update(
            {
                "legacy_maintenance_boundary_sha256": hostile_sha,
                "legacy_observed_cutoff_height": boundary["observed_cutoff_height"],
                "legacy_public_max_height": boundary["legacy_public_max_height"],
            }
        )
        hostile_rows = copy.deepcopy(rows)
        hostile_rows["legacy_maintenance_boundary"]["sha256"] = hostile_sha
        hostile_payloads = dict(payloads)
        hostile_payloads["legacy_maintenance_boundary"] = hostile_boundary
        hostile_payloads["legacy_maintenance_boundary_sidecar"] = (
            f"{hostile_sha}  legacy-maintenance-boundary.json\n".encode("ascii")
        )
        hostile_value["artifacts"]["legacy_maintenance_boundary_sidecar"][
            "sha256"
        ] = hashlib.sha256(
            hostile_payloads["legacy_maintenance_boundary_sidecar"]
        ).hexdigest()
        offline = copy.deepcopy(json.loads(payloads["offline_stop_evidence"]))
        offline["legacy_maintenance_boundary"] = boundary
        offline["legacy_maintenance_boundary_sha256"] = hostile_sha
        hostile_payloads["offline_stop_evidence"] = rollout.canonical_bytes(offline)
        with self.assertRaisesRegex(rollout.RolloutError, "ledger differs"):
            rollout.verify_legacy_maintenance_stage_payloads(
                hostile_value, hostile_rows, hostile_payloads
            )

    def test_public_gate_stays_maintenance_until_journaled_six_node_promotion(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        self.prime_interlock_interpreters(harness)
        self.assertIn("public-maintenance.v1", harness.maintenance_caddyfile(value["validators"][0]))
        self.assertNotIn("@corsRead", harness.maintenance_caddyfile(value["validators"][0]))
        self.assertIn("@corsRead", harness.caddyfile(value["validators"][0]))
        self.assertIn(
            "renewal_window_ratio 0.5",
            harness.maintenance_caddyfile(value["validators"][0]),
        )
        self.assertIn(
            "renewal_window_ratio 0.5",
            harness.caddyfile(value["validators"][0]),
        )
        self.assertIn("admin off", harness.maintenance_caddyfile(value["validators"][0]))
        self.assertIn("admin off", harness.caddyfile(value["validators"][0]))
        gateway_unit = harness.gateway_unit(value["validators"][0])
        self.assertIn("Caddyfile.active", gateway_unit)
        self.assertIn("User=arc-caddy", gateway_unit)
        self.assertIn("Group=arc-caddy", gateway_unit)
        self.assertIn("CapabilityBoundingSet=CAP_NET_BIND_SERVICE", gateway_unit)
        self.assertIn("AmbientCapabilities=CAP_NET_BIND_SERVICE", gateway_unit)
        self.assertIn("ProtectSystem=strict", gateway_unit)
        self.assertNotIn("ExecReload=", gateway_unit)
        validator_unit = harness.systemd_unit(value["validators"][0])
        self.assertIn(f"Group={rollout.RPC_ORIGIN_GROUP}", validator_unit)
        self.assertIn("--rpc-unix /run/arc-v3-rpc-", validator_unit)
        self.assertNotIn("--rpc 127.0.0.1:", validator_unit)
        filter_unit = harness.filter_unit(value["validators"][0])
        interlock_service = harness.late_fork_interlock_service_name(
            value["validators"][0]
        )
        for exact in (
            "User=arc-rpc-filter",
            "Group=arc-caddy",
            "UMask=0007",
            "RuntimeDirectory=arc-rpc-filter-nyc-",
            "ExecStartPre=/opt/arc/recovery-v3/arc-nginx-filter-preflight",
            "ProtectSystem=strict",
            "ProtectHome=true",
            "NoNewPrivileges=true",
            "CapabilityBoundingSet=\n",
            "AmbientCapabilities=\n",
            "ReadWritePaths=/opt/arc/recovery-v3/rpc-filter-state",
            f"After=network-online.target {interlock_service}",
            f"Requires={interlock_service}",
        ):
            self.assertIn(exact, filter_unit)
        filter_preflight = harness.filter_preflight(value["validators"][0])
        for exact in (
            rollout.NGINX_PACKAGE_VERSION,
            rollout.NGINX_LINUX_AMD64_SHA256,
            "--with-http_auth_request_module",
            "nginx-filter.conf",
            "sha256sum --check --strict",
            "interlock_ready=false",
            f"systemctl is-active --quiet {interlock_service}",
        ):
            self.assertIn(exact, filter_preflight)
        source = MODULE_PATH.read_text(encoding="utf-8")
        self.assertGreaterEqual(source.count('/usr/bin/sync "$root"'), 2)
        for marker in (
            'mv -T -- "$active_tmp" "$root/Caddyfile.active"',
            'mv -T -- "$temporary" "$root/Caddyfile.active"',
        ):
            offset = source.index(marker)
            self.assertIn('/usr/bin/sync "$root"', source[offset : offset + 180])
        self.assertIn('systemctl restart "$service"', source)
        self.assertNotIn('systemctl reload "$service"', source)
        self.assertIn('test "$gateway_uid" != 0', source)
        self.assertIn("caddy-admin-disabled", source)
        maintenance_config = harness.maintenance_caddyfile(value["validators"][0])
        live_config = harness.caddyfile(value["validators"][0])
        filter_config = harness.nginx_filter(value["validators"][0])
        self.assertNotIn("forward_auth", maintenance_config)
        self.assertNotIn("forward_auth", live_config)
        self.assertEqual(
            filter_config.count("auth_request /__arc_interlock_gate;"), 8
        )
        self.assertIn("gate.sock:/gate;", filter_config)
        self.assertIn("gate.sock:/maintenance/status;", filter_config)
        self.assertNotIn("127.0.0.1:18081", filter_config)
        self.assertNotIn("listen 127.0.0.1:18080", filter_config)
        self.assertIn("listen unix:/run/arc-rpc-filter-", filter_config)
        self.assertIn("set_real_ip_from unix:;", filter_config)
        self.assertIn("unix: 1;", filter_config)
        for config in (maintenance_config, live_config):
            self.assertIn("reverse_proxy unix//run/arc-rpc-filter-", config)
            self.assertIn("header_up Host 127.0.0.1", config)
            self.assertIn("header_up X-Forwarded-For {remote_host}", config)
        harness.validate_gateway_security_contract(
            maintenance_config, live_config, filter_config
        )
        with self.assertRaisesRegex(rollout.RolloutError, "forward_auth"):
            harness.validate_gateway_security_contract(
                maintenance_config,
                live_config + "\nforward_auth 127.0.0.1:18081\n",
                filter_config,
            )
        first_proxy = live_config.index("reverse_proxy unix//run/arc-rpc-filter-")
        first_proxy_end = live_config.index("        }", first_proxy)
        xff_line = "            header_up X-Forwarded-For {remote_host}\n"
        first_xff = live_config.index(xff_line, first_proxy)
        self.assertLess(first_xff, first_proxy_end)
        moved_xff = (
            live_config[:first_xff]
            + live_config[first_xff + len(xff_line) :]
            + "\nheader_up X-Forwarded-For {remote_host}\n"
        )
        with self.assertRaisesRegex(
            rollout.RolloutError, "does not exactly overwrite X-Forwarded-For"
        ):
            harness.validate_gateway_security_contract(
                maintenance_config, moved_xff, filter_config
            )
        duplicate_xff = live_config.replace(
            "            header_up X-Forwarded-For {remote_host}\n",
            "            header_up X-Forwarded-For {remote_host}\n"
            "            header_up x-forwarded-for {http.request.header.X-Forwarded-For}\n",
            1,
        )
        with self.assertRaisesRegex(
            rollout.RolloutError, "does not exactly overwrite X-Forwarded-For"
        ):
            harness.validate_gateway_security_contract(
                maintenance_config, duplicate_xff, filter_config
            )
        with self.assertRaisesRegex(
            rollout.RolloutError, "unsealed or unexpected upstream"
        ):
            harness.validate_gateway_security_contract(
                maintenance_config,
                live_config + "\nreverse_proxy 127.0.0.1:9090\n",
                filter_config,
            )
        unquoted_regex = filter_config.replace(
            'location ~ "^/(?:block/', 'location ~ ^/(?:block/', 1
        ).replace(')$" {', ')$ {', 1)
        with self.assertRaisesRegex(rollout.RolloutError, "quoted nginx token"):
            harness.validate_gateway_security_contract(
                maintenance_config, live_config, unquoted_regex
            )
        for hostile_filter in (
            filter_config.replace("real_ip_recursive on;", "real_ip_recursive off;"),
            filter_config.replace(
                'proxy_set_header X-Forwarded-For "";',
                'proxy_set_header X-Forwarded-For "";\n'
                "    proxy_set_header x-forwarded-for $http_x_forwarded_for;",
            ),
        ):
            with self.assertRaisesRegex(
                rollout.RolloutError, "permission-sealed on exact Unix sockets"
            ):
                harness.validate_gateway_security_contract(
                    maintenance_config, live_config, hostile_filter
                )
        for marker in (
            "location = /internal/community/reward/approve {",
            "location ~ ^/(?:shards/announce|inference/(?:forward_shard|cleanup_shard))$ {",
        ):
            start = filter_config.index(marker)
            gated = filter_config.index(
                "            auth_request /__arc_interlock_gate;", start
            )
            hostile = filter_config[:gated] + filter_config[
                gated + len("            auth_request /__arc_interlock_gate;\n") :
            ]
            with self.assertRaisesRegex(
                rollout.RolloutError,
                "omits the fail-closed interlock|not exactly fail-closed",
            ):
                harness.validate_gateway_security_contract(
                    maintenance_config, live_config, hostile
                )
        source = MODULE_PATH.read_text(encoding="utf-8")
        self.assertNotIn("apt-mark hold nginx", source)
        self.assertIn("apt-mark showhold", source)
        for exact in (
            'systemctl stop "$interlock_service"',
            '--unix-socket "$public_filter_socket"',
            "http://localhost/internal/community/reward/approve",
            "http://localhost/shards/announce",
            'test "$reward_gate_failure_status" = 500',
            'test "$shard_gate_failure_status" = 500',
            'test "$gate_recovered" = true',
            'runuser -u "$attacker_user"',
            'test -z "$(ss -H -ltnp',
            'assert_exact_filter_group "$gateway_user" "$filter_user" "$gateway_gid"',
            '--property=FragmentPath --value',
            '--property=DropInPaths --value',
            '--property=Transient --value',
            'test "$(stat -c %U:%G:%a:%h "$installed")" = root:root:644:1',
            'test "$effective_systemd_inventory_sha" = "$expected_effective_systemd_inventory_sha"',
            'test "$gate_ready" = true',
            '--property=SupplementaryGroups --value',
        ):
            self.assertIn(exact, source)

        # The same public-gate transaction also requires a fresh, exact v2
        # retirement tripwire; any resurrected retired source latches maintenance.
        value, _rows, payloads = self.maintenance_stage_fixture()
        source_artifact = value["artifacts"]["legacy_late_fork_source_set"]
        source_path = Path(source_artifact["path"])
        source_path.write_bytes(payloads["legacy_late_fork_source_set"])
        source_path.chmod(0o400)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        now = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
        status = {
            "schema": "arc.recovery.legacy-late-fork-interlock-status.v2",
            "source_main_commit": value["provenance"]["source_main_commit"],
            "boundary_sha256": value["chain"][
                "legacy_maintenance_boundary_sha256"
            ],
            "source_set_sha256": value["chain"][
                "legacy_late_fork_source_set_sha256"
            ],
            "tool_sha256": value["artifacts"][
                "legacy_late_fork_interlock_tool"
            ]["sha256"],
            "sampled_at": now.strftime("%Y-%m-%dT%H:%M:%SZ"),
            "expires_at": (now + dt.timedelta(seconds=90)).strftime(
                "%Y-%m-%dT%H:%M:%SZ"
            ),
            "poll_interval_seconds": 30,
            "max_staleness_seconds": 90,
            "observations": [
                {
                    "name": row["node"],
                    "origin": row["origin"],
                    "scope": "retired",
                    "outcome": "unreachable",
                    "height": None,
                    "block_hash": None,
                    "state_root": None,
                    "response_sha256": None,
                }
                for row in rollout.LEGACY_OFFICIAL_ORIGINS
            ],
            "state": "HEALTHY",
            "gate_reason": "capture-bound-retirement-tripwire-clear",
            "incident_sha256": None,
            "required_community_observations": 0,
            "healthy_community_observations": 0,
            "global_absence_claimed": False,
        }
        raw = rollout.canonical_bytes(status)
        self.assertEqual(
            harness._validate_late_fork_status(raw, require_healthy=True), status
        )

        stale_schema = copy.deepcopy(status)
        stale_schema["schema"] = "arc.recovery.legacy-late-fork-interlock-status.v1"
        with self.assertRaisesRegex(rollout.RolloutError, "identity/policy differs"):
            harness._validate_late_fork_status(
                rollout.canonical_bytes(stale_schema), require_healthy=True
            )

        resurrected = copy.deepcopy(status)
        resurrected["observations"][0].update(
            {
                "outcome": "observed",
                "height": value["chain"]["legacy_observed_cutoff_height"],
                "block_hash": "1" * 64,
                "state_root": "2" * 64,
                "response_sha256": {
                    label: f"{index + 3:064x}"
                    for index, label in enumerate(
                        ("info_before", "latest", "exact", "info_after")
                    )
                },
            }
        )
        with self.assertRaisesRegex(rollout.RolloutError, "required latched incident"):
            harness._validate_late_fork_status(
                rollout.canonical_bytes(resurrected), require_healthy=False
            )
        resurrected.update(
            {
                "state": "MAINTENANCE",
                "gate_reason": "latched-legacy-source-incident",
                "incident_sha256": "a" * 64,
            }
        )
        harness._validate_late_fork_status(
            rollout.canonical_bytes(resurrected), require_healthy=False
        )
        with self.assertRaisesRegex(rollout.RolloutError, "not healthy"):
            harness._validate_late_fork_status(
                rollout.canonical_bytes(resurrected), require_healthy=True
            )

        journal_hashes = {
            "PUBLIC-GATE-OPEN-INTENT.json": "a" * 64,
            "PUBLIC-GATE-OPEN-RECEIPT.json": "b" * 64,
        }
        harness._rollback_journal_write = mock.Mock(
            side_effect=lambda name, _value: journal_hashes[name]
        )

        def transition(node, *, target, intent_sha256, final=None):
            commitment = final or (0, "0" * 64, "0" * 64)
            active = hashlib.sha256(
                (
                    harness.caddyfile(node)
                    if target == "live"
                    else harness.maintenance_caddyfile(node)
                ).encode("utf-8")
            ).hexdigest()
            return {
                "schema": "arc.recovery.public-gate-host.v1",
                "rollout_manifest_sha256": harness.digest,
                "state": target,
                "active_caddyfile_sha256": active,
                "promotion_intent_sha256": intent_sha256,
                "height": commitment[0],
                "block_hash": commitment[1],
                "state_root": commitment[2],
                "node": node["name"],
                "host": node["host"],
            }

        harness._set_public_gate_config = mock.Mock(side_effect=transition)
        harness.production_public_gate_open = False
        initial_height = value["chain"]["legacy_public_max_height"] + 1
        final_height = initial_height + value["checks"]["min_height_advance"]
        receipt_sha = harness.open_public_gate(
            (initial_height, "1" * 64, "2" * 64),
            (final_height, "3" * 64, "4" * 64),
        )
        self.assertEqual(receipt_sha, "b" * 64)
        self.assertTrue(harness.production_public_gate_open)
        self.assertEqual(harness._set_public_gate_config.call_count, 6)
        promoted = [
            call.args[0]["name"]
            for call in harness._set_public_gate_config.call_args_list
        ]
        self.assertEqual(len(promoted), 6)
        self.assertEqual(set(promoted), {node["name"] for node in value["validators"]})
        intent = harness._rollback_journal_write.call_args_list[0].args[1]
        self.assertGreater(
            intent["initial"]["height"], value["chain"]["legacy_public_max_height"]
        )
        self.assertGreaterEqual(
            intent["final"]["height"],
            intent["initial"]["height"] + value["checks"]["min_height_advance"],
        )

    def test_filter_group_identity_rejects_primary_or_supplementary_intruders(self) -> None:
        source = MODULE_PATH.read_text(encoding="utf-8")
        helper = source.split("# BEGIN ARC FILTER GROUP IDENTITY HELPER", 1)[1].split(
            "# END ARC FILTER GROUP IDENTITY HELPER", 1
        )[0]

        def exercise(passwd_rows: str, supplementary: str = ""):
            shell = f'''set -eu
getent() {{
  if [ "$1" = passwd ]; then
    printf '%s\n' "$ARC_TEST_PASSWD"
  else
    printf 'arc-caddy:x:4242:%s\n' "$ARC_TEST_SUPPLEMENTARY"
  fi
}}
{helper}
assert_exact_filter_group arc-caddy arc-rpc-filter 4242
'''
            environment = dict(os.environ)
            environment.update(
                ARC_TEST_PASSWD=passwd_rows,
                ARC_TEST_SUPPLEMENTARY=supplementary,
            )
            return rollout.subprocess.run(
                ["/bin/sh", "-c", shell],
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )

        exact = "\n".join(
            (
                "arc-caddy:x:2001:4242::/nonexistent:/usr/sbin/nologin",
                "arc-rpc-filter:x:2002:4242::/nonexistent:/usr/sbin/nologin",
                "arc-unrelated:x:2003:5000::/nonexistent:/usr/sbin/nologin",
            )
        )
        self.assertEqual(exercise(exact).returncode, 0)
        self.assertNotEqual(
            exercise(exact + "\narc-intruder:x:2004:4242::/tmp:/bin/sh").returncode,
            0,
        )
        self.assertNotEqual(exercise(exact, "arc-intruder").returncode, 0)
        self.assertNotEqual(
            exercise(exact.replace("arc-rpc-filter:x:2002:4242", "arc-rpc-filter:x:2002:5001")).returncode,
            0,
        )

    def test_public_gate_receipt_uses_pinned_python_and_valid_shell(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        self.prime_interlock_interpreters(harness)
        node = value["validators"][0]
        captured: list[str] = []

        def fake_ssh(_node, script, args=(), **_kwargs):
            captured.append(script)
            return rollout.canonical_bytes(
                {
                    "schema": "arc.recovery.public-gate-host.v1",
                    "rollout_manifest_sha256": args[2],
                    "state": args[3],
                    "active_caddyfile_sha256": args[4],
                    "promotion_intent_sha256": args[6],
                    "height": int(args[7]),
                    "block_hash": args[8],
                    "state_root": args[9],
                    "node": args[11],
                    "host": args[10],
                }
            ).decode("utf-8")

        harness.ssh = mock.Mock(side_effect=fake_ssh)
        harness._set_public_gate_config(
            node,
            target="maintenance",
            intent_sha256="a" * 64,
        )
        self.assertEqual(len(captured), 1)
        self.assertIn("arc_semantic_python -", captured[0])
        self.assertNotIn("python3 -", captured[0])
        syntax = rollout.subprocess.run(
            ["/bin/sh", "-n"],
            input=captured[0],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(syntax.returncode, 0, syntax.stderr)

    def test_partial_public_gate_promotion_recloses_every_host_to_maintenance(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        self.prime_interlock_interpreters(harness)
        harness.production_public_gate_open = False
        harness._rollback_journal_write = mock.Mock(return_value="a" * 64)
        calls = []

        def transition(node, *, target, intent_sha256, final=None):
            calls.append((target, node["name"]))
            if target == "live" and node["name"] == "ams":
                raise rollout.RolloutError("simulated live reload failure")
            commitment = final or (0, "0" * 64, "0" * 64)
            return {
                "schema": "arc.recovery.public-gate-host.v1",
                "rollout_manifest_sha256": harness.digest,
                "state": target,
                "active_caddyfile_sha256": "5" * 64,
                "promotion_intent_sha256": intent_sha256,
                "height": commitment[0],
                "block_hash": commitment[1],
                "state_root": commitment[2],
                "node": node["name"],
                "host": node["host"],
            }

        harness._set_public_gate_config = mock.Mock(side_effect=transition)
        with self.assertRaisesRegex(rollout.RolloutError, "PUBLIC_GATE_OPEN_INCOMPLETE"):
            harness.open_public_gate(
                (239, "1" * 64, "2" * 64),
                (240, "3" * 64, "4" * 64),
            )
        self.assertFalse(harness.production_public_gate_open)
        maintenance_names = [name for target, name in calls if target == "maintenance"]
        self.assertEqual(len(maintenance_names), 6)
        self.assertEqual(
            set(maintenance_names), {node["name"] for node in value["validators"]}
        )

    def test_manifest_rejects_public_listener_and_protected_override(self) -> None:
        value = self.fixture()
        value["validators"][0]["rpc_listen"] = "0.0.0.0:9090"
        with self.assertRaisesRegex(rollout.RolloutError, "bind loopback"):
            rollout.validate_manifest(value)
        value = self.fixture()
        value["validators"][0]["extra_args"] = ["--validator-seed=leak"]
        with self.assertRaisesRegex(rollout.RolloutError, "protected flag"):
            rollout.validate_manifest(value)

    def test_production_rejects_systemd_specifiers_before_any_subprocess(self) -> None:
        cases = (
            ("data_dir", "/var/lib/arc-%n"),
            ("key_file", "/root/arc-key-%n.json"),
            ("model_path", "/opt/arc-models/model-%n.gguf"),
            ("remote_root", "/opt/arc/recovery-%n"),
            ("extra_args", ["--node-name=%n"]),
        )
        with mock.patch.object(rollout.subprocess, "run") as process:
            for field, invalid in cases:
                value = self.fixture(production=True)
                value["validators"][0][field] = invalid
                with self.subTest(field=field), self.assertRaisesRegex(
                    rollout.RolloutError, "systemd percent specifiers"
                ):
                    rollout.validate_manifest(value)
            process.assert_not_called()

        with self.assertRaisesRegex(rollout.RolloutError, "systemd percent specifiers"):
            rollout.RecoveryRollout._systemd_escape_arg("/var/lib/arc-%n")

    def test_manifest_proves_one_validator_restart_quorum(self) -> None:
        value = self.fixture()
        value["validators"][0]["stake"] = 30_000_000
        with self.assertRaisesRegex(rollout.RolloutError, "restart"):
            rollout.validate_manifest(value)

    def test_production_requires_exact_https_ip_and_pinned_caddy(self) -> None:
        value = self.fixture(production=True)
        self.assertIs(rollout.validate_manifest(value), value)
        cleartext = copy.deepcopy(value)
        cleartext["validators"][0]["rpc_url"] = "http://192.0.2.1:9090"
        with self.assertRaisesRegex(rollout.RolloutError, "must use HTTPS"):
            rollout.validate_manifest(cleartext)
        wrong_ip = copy.deepcopy(value)
        wrong_ip["validators"][0]["rpc_url"] = "https://192.0.2.99"
        with self.assertRaisesRegex(rollout.RolloutError, "must be exactly"):
            rollout.validate_manifest(wrong_ip)
        missing_caddy = copy.deepcopy(value)
        missing_caddy["artifacts"].pop("caddy")
        with self.assertRaisesRegex(rollout.RolloutError, "missing: caddy"):
            rollout.validate_manifest(missing_caddy)
        with mock.patch.object(
            rollout, "CADDY_LINUX_AMD64_SHA256", PRODUCTION_CADDY_SHA256
        ), self.assertRaisesRegex(rollout.RolloutError, "Caddy v2.11.4 linux-amd64"):
            rollout.validate_manifest(value)

        missing_archive = copy.deepcopy(value)
        missing_archive.pop("archive")
        with self.assertRaisesRegex(rollout.RolloutError, "manifest.archive"):
            rollout.validate_manifest(missing_archive)
        malformed_archive = copy.deepcopy(value)
        malformed_archive["archive"]["capture_id"] = "F" * 64
        with self.assertRaisesRegex(rollout.RolloutError, "capture_id"):
            rollout.validate_manifest(malformed_archive)

        local_archive = self.fixture()
        local_archive["archive"] = {"freeze_plan_sha256": "e" * 64, "capture_id": "f" * 64}
        with self.assertRaisesRegex(rollout.RolloutError, "must not contain"):
            rollout.validate_manifest(local_archive)

    def test_protected_pretag_window_rejects_mixed_or_swapped_api_roots(self) -> None:
        value = self.fixture(production=True)
        self.assertIs(rollout.validate_manifest(value), value)
        mixed = copy.deepcopy(value)
        mixed["provenance"]["protected_pretag_artifact"]["groups"][1]["initial"][
            "api"
        ]["responses"][2]["body_sha256"] = "f" * 64
        with self.assertRaisesRegex(rollout.RolloutError, "exact set-level API roots"):
            rollout.validate_manifest(mixed)
        swapped = copy.deepcopy(value)
        group = swapped["provenance"]["protected_pretag_artifact"]["groups"][2]
        group["initial"]["api"], group["final"]["api"] = (
            group["final"]["api"],
            group["initial"]["api"],
        )
        with self.assertRaisesRegex(
            rollout.RolloutError, "final proof changed|set-level API roots|freshness"
        ):
            rollout.validate_manifest(swapped)

    def test_schema_and_runtime_both_forbid_local_provenance(self) -> None:
        schema = json.loads(
            MODULE_PATH.with_name("recovery-manifest.schema.json").read_text(
                encoding="utf-8"
            )
        )
        forbidden = {
            tuple(row["required"])
            for row in schema["allOf"][0]["else"]["not"]["anyOf"]
        }
        self.assertEqual(forbidden, {("archive",), ("provenance",)})
        value = self.fixture()
        value["provenance"] = {}
        with self.assertRaisesRegex(rollout.RolloutError, "must not contain.*provenance"):
            rollout.validate_manifest(value)

    def test_production_requires_canonical_model_and_exact_balanced_3x_shards(self) -> None:
        value = self.fixture(production=True)
        self.assertIs(rollout.validate_manifest(value), value)

        wrong_model = copy.deepcopy(value)
        wrong_model["validators"][0]["model_sha256"] = "f" * 64
        with self.assertRaisesRegex(rollout.RolloutError, "canonical v0.8"):
            rollout.validate_manifest(wrong_model)

        unbalanced = copy.deepcopy(value)
        unbalanced["validators"][0]["shard_ranges"] = [[0, 6], [12, 17]]
        with self.assertRaisesRegex(rollout.RolloutError, r"15\.\.17 layers"):
            rollout.validate_manifest(unbalanced)

        wrong_replication = copy.deepcopy(value)
        wrong_replication["validators"][0]["shard_ranges"] = [[0, 6], [12, 17], [22, 27]]
        with self.assertRaisesRegex(rollout.RolloutError, "exact 3x coverage"):
            rollout.validate_manifest(wrong_replication)

        for forbidden in ("--shard-range=0:32", "--enable-i16"):
            override = copy.deepcopy(value)
            override["validators"][0]["extra_args"] = [forbidden]
            with self.assertRaisesRegex(rollout.RolloutError, "protected flag"):
                rollout.validate_manifest(override)

    def test_final_archive_manifest_is_roots_only_projection_of_prearchive(self) -> None:
        prearchive = self.fixture(production=True)
        prearchive_digest = rollout.sha256_bytes(rollout.canonical_bytes(prearchive))
        self.assertEqual(rollout.prearchive_projection_digest(prearchive), prearchive_digest)
        final = copy.deepcopy(prearchive)
        final["archive"].update(
            {
                "complete_sha256": "1" * 64,
                "archive_manifest_sha256": "2" * 64,
                "sha256sums_sha256": "3" * 64,
                "prearchive_rollout_sha256": prearchive_digest,
            }
        )
        self.assertIs(rollout.validate_manifest(final), final)
        with self.assertRaisesRegex(rollout.RolloutError, "exact prearchive manifest"):
            rollout.require_prearchive_manifest(final)
        rollout.require_prearchive_manifest(prearchive)

        mutations = (
            lambda value: value["validators"][0].__setitem__("host", "192.0.2.99"),
            lambda value: value["artifacts"]["binary"].__setitem__("sha256", "9" * 64),
            lambda value: value["checks"].__setitem__("observation_seconds", 2),
            lambda value: value["chain"].__setitem__("legacy_public_max_height", 111),
            lambda value: value["validators"][0].__setitem__("shard_ranges", [[0, 6], [12, 17], [22, 27]]),
        )
        for mutate in mutations:
            changed = copy.deepcopy(final)
            mutate(changed)
            with self.assertRaisesRegex(rollout.RolloutError, "outside the four archive finalization roots"):
                rollout.validate_manifest(changed)

        partial = copy.deepcopy(prearchive)
        partial["archive"]["complete_sha256"] = "1" * 64
        with self.assertRaisesRegex(rollout.RolloutError, "either all-zero prearchive or all nonzero"):
            rollout.validate_manifest(partial)

    def test_production_resume_markers_provenance_and_path_separation(self) -> None:
        value = self.fixture(production=True)
        script_root = Path(rollout.__file__).resolve().parent
        value["archive"].update(
            {
                "archive_orchestrator_sha256": digest(script_root / "archive-fleet-to-drive.sh"),
                "remote_helper_sha256": digest(script_root / "archive-node.sh"),
                "rollout_tool_sha256": digest(Path(rollout.__file__).resolve()),
                "rollout_schema_sha256": digest(script_root / "recovery-manifest.schema.json"),
            }
        )
        value["provenance"]["offline_stop_verification"]["remote_helper_sha256"] = value["archive"]["remote_helper_sha256"]
        value["provenance"]["offline_stop_verification"]["remote_helper_path"] = (
            "/root/.arc-recovery-helpers/"
            + value["archive"]["remote_helper_sha256"]
            + "/archive-node.sh"
        )
        harness = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
        )
        self.prime_interlock_interpreters(harness)
        harness.verify_execution_provenance()
        changed = copy.deepcopy(value)
        changed["archive"]["remote_helper_sha256"] = "f" * 64
        changed["provenance"]["offline_stop_verification"]["remote_helper_sha256"] = "f" * 64
        changed["provenance"]["offline_stop_verification"]["remote_helper_path"] = (
            "/root/.arc-recovery-helpers/" + "f" * 64 + "/archive-node.sh"
        )
        with self.assertRaisesRegex(rollout.RolloutError, "remote archive helper bytes differ"):
            rollout.RecoveryRollout(changed, "d" * 64).verify_execution_provenance()

        self.assertTrue(rollout.paths_overlap("/root/arc-data", "/root/arc-data/v3"))
        self.assertFalse(rollout.paths_overlap("/root/arc-data", "/var/lib/arc-v3"))
        nested = copy.deepcopy(value)
        nested["validators"][0]["remote_root"] = "/var/lib/arc-v3/release"
        with self.assertRaisesRegex(rollout.RolloutError, "disjoint, non-nested"):
            rollout.validate_manifest(nested)

        ssh_calls: list[tuple[str, tuple[str, ...]]] = []
        def fake_ssh(node, script, args=(), **kwargs):
            ssh_calls.append((script, tuple(args)))
            if 'mktemp "$root/.${name}.upload.XXXXXX"' in script:
                return f"{args[0]}/.{args[1]}.upload.TEST01\n"
            if 'security_receipt="$root/nginx-security-boundary.json"' in script:
                return rollout.canonical_bytes(
                    {
                        "schema": "arc.recovery.gateway-security-boundary.v1",
                        "rollout_manifest_sha256": args[5],
                        "node": args[25],
                        "package": "nginx",
                        "package_version": rollout.NGINX_PACKAGE_VERSION,
                        "binary_path": "/usr/sbin/nginx",
                        "binary_sha256": rollout.NGINX_LINUX_AMD64_SHA256,
                        "auth_request_module": True,
                        "certificate_storage_nonempty": True,
                        "caddy_restart_tls_probe_status": 404,
                        "filter_config_sha256": args[29],
                        "filter_unit_sha256": args[30],
                        "filter_preflight_sha256": args[31],
                        "filter_user": args[28],
                        "package_held": False,
                        "reward_gate_failure_status": 500,
                        "shard_gate_failure_status": 500,
                        "filter_socket_path": args[33],
                        "archive_filter_socket_path": args[34],
                        "filter_socket_mode": "0770",
                        "attacker_user": args[35],
                        "attacker_socket_denied": True,
                        "attacker_interlock_socket_denied": True,
                        "direct_tcp_filter_absent": True,
                        "direct_tcp_interlock_absent": True,
                        "caddy_identity_healthy_gate_status": 502,
                        "caddy_interlock_socket_denied": True,
                        "effective_systemd_inventory_sha256": args[40],
                        "filter_group_primary_users": [
                            rollout.CADDY_USER,
                            rollout.NGINX_FILTER_USER,
                        ],
                        "filter_group_supplementary_users": [],
                        "interlock_group": rollout.LATE_FORK_INTERLOCK_GROUP,
                        "interlock_group_primary_users": [
                            rollout.LATE_FORK_INTERLOCK_USER,
                        ],
                        "interlock_group_supplementary_users": [
                            rollout.NGINX_FILTER_USER,
                        ],
                        "interlock_socket_mode": "0660",
                        "interlock_socket_path": args[42],
                        "origin_group": rollout.RPC_ORIGIN_GROUP,
                        "origin_group_primary_users": [],
                        "origin_group_supplementary_users": [
                            rollout.NGINX_FILTER_USER,
                        ],
                    }
                ).decode("utf-8")
            return ""

        harness.ssh = mock.Mock(side_effect=fake_ssh)
        harness.scp = mock.Mock()
        node = value["validators"][0]
        harness._stage_production_node(node)
        harness._stage_production_node(node)
        harness._install_gateway_and_unit(node)
        harness._install_gateway_and_unit(node)
        for remote_script, _ in ssh_calls:
            syntax = rollout.subprocess.run(
                ["/bin/sh", "-n"],
                input=remote_script,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(
                syntax.returncode,
                0,
                f"remote rollout script is not POSIX-shell syntax: {syntax.stderr}",
            )
        scripts = "\n".join(script for script, _ in ssh_calls)
        for required in (
            ".arc-recovery-rollout-owner",
            ".arc-recovery-stage-complete",
            "validate_marker",
            "deployment-files.sha256",
            'test "$(cat "$owner")" = "$digest"',
            'cmp --silent "$root/$unit" "$installed"',
            'mv --no-clobber -T -- "$temporary" "$destination"',
        ):
            self.assertIn(required, scripts)
        self.assertTrue(harness.scp.call_args_list)
        for call in harness.scp.call_args_list:
            remote = call.args[2]
            self.assertIn("/.", remote)
            self.assertIn(".upload.", remote)
        self.assertFalse(
            any(call.args[2].endswith("/arc-node") for call in harness.scp.call_args_list)
        )
        self.assertTrue(
            any(
                f".arc-recovery-import-{harness.digest}" in argument
                for _, arguments in ssh_calls
                for argument in arguments
            )
        )
        self.assertGreaterEqual(scripts.count("validate_marker"), 2)

        source = Path(rollout.__file__).read_text(encoding="utf-8")
        self.assertIn("verify_production_archive(verify_live_captures=True)", source)

    def test_seal_is_canonical_read_only_hash_bound_and_create_only(self) -> None:
        draft = self.root / "draft.json"
        sealed = self.root / "locked.json"
        draft.write_text(json.dumps(self.fixture(), indent=2), encoding="utf-8")
        locked_hash = rollout.seal_manifest(draft, sealed)
        loaded, loaded_hash = rollout.load_sealed_manifest(sealed)
        self.assertEqual(locked_hash, loaded_hash)
        self.assertEqual(loaded["schema"], rollout.SCHEMA)
        self.assertEqual(stat.S_IMODE(sealed.stat().st_mode), 0o444)
        self.assertEqual(stat.S_IMODE(Path(str(sealed) + ".sha256").stat().st_mode), 0o444)
        with self.assertRaisesRegex(rollout.RolloutError, "refusing replacement"):
            rollout.seal_manifest(draft, sealed)
        sealed.chmod(0o644)
        with self.assertRaisesRegex(rollout.RolloutError, "no write bits"):
            rollout.load_sealed_manifest(sealed)

    def test_exact_go_requires_argument_and_independent_phrase(self) -> None:
        locked_hash = "a" * 64
        local = self.fixture()
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(rollout.RolloutError, "--go-hash"):
                rollout.require_go(local, locked_hash, None, None, None)
        with mock.patch.dict(os.environ, {"ARC_RECOVERY_GO": f"GO {locked_hash}"}, clear=True):
            rollout.require_go(local, locked_hash, locked_hash, None, None)
        with mock.patch.dict(os.environ, {"ARC_RECOVERY_GO": f"GO {'b' * 64}"}, clear=True):
            with self.assertRaisesRegex(rollout.RolloutError, "ARC_RECOVERY_GO"):
                rollout.require_go(local, locked_hash, locked_hash, None, None)

        production = self.fixture(production=True)
        prearchive_digest = rollout.prearchive_projection_digest(production)
        production["archive"].update(
            {
                "complete_sha256": "1" * 64,
                "archive_manifest_sha256": "2" * 64,
                "sha256sums_sha256": "3" * 64,
                "prearchive_rollout_sha256": prearchive_digest,
            }
        )
        rollout.validate_manifest(production)
        production_phrase = rollout.execution_authorization(production, locked_hash, "2" * 64)
        self.assertIn(f"FREEZE {'e' * 64}", production_phrase)
        self.assertIn(f"CAPTURE {production['archive']['capture_id']}", production_phrase)
        self.assertIn(f"ARCHIVE {'2' * 64}", production_phrase)
        self.assertIn("LEGACY_WAL BOUND", production_phrase)
        with mock.patch.dict(os.environ, {"ARC_RECOVERY_GO": production_phrase}, clear=True):
            rollout.require_go(production, locked_hash, locked_hash, "2" * 64, "2" * 64)
        with mock.patch.dict(os.environ, {"ARC_RECOVERY_GO": f"GO {locked_hash}"}, clear=True):
            with self.assertRaisesRegex(rollout.RolloutError, "ARC_RECOVERY_GO"):
                rollout.require_go(production, locked_hash, locked_hash, "2" * 64, "2" * 64)

    def test_read_only_ssh_streams_exact_script_without_installing_helper(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        node = value["validators"][0]
        harness.production_ssh_path = Path("/usr/bin/ssh")
        harness.production_known_hosts = Path("/secure/known_hosts")
        harness.production_ssh_identity = Path("/secure/identity")
        harness.production_transport_env = {
            "HOME": "/secure/private",
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "LANG": "C",
            "LC_ALL": "C",
            "TZ": "UTC",
        }
        harness._assert_production_ssh_transport = mock.Mock()
        probe = "set -eu\nprintf '%s:%s\\n' \"$1\" \"$2\"\n"
        arguments = ("/var/lib/arc-v3", "name:value")
        completed = SimpleNamespace(stdout="/var/lib/arc-v3:name:value\n")

        with mock.patch.object(rollout, "run_checked", return_value=completed) as checked:
            result = harness.ssh_read_only(
                node, probe, arguments, timeout=37
            )

        self.assertEqual(result, completed.stdout)
        checked.assert_called_once()
        command = checked.call_args.args[0]
        self.assertEqual(command[0], "/usr/bin/ssh")
        self.assertEqual(command[-2], f"root@{node['host']}")
        self.assertEqual(
            command[-1],
            rollout.shlex.join(
                [
                    "/usr/bin/env", "-i",
                    "HOME=/root",
                    "PATH=/usr/bin:/bin:/usr/sbin:/sbin",
                    "LANG=C", "LC_ALL=C",
                    "/bin/sh", "-s", "--", *arguments,
                ]
            ),
        )
        for option in (
            "UserKnownHostsFile=/secure/known_hosts",
            "GlobalKnownHostsFile=/dev/null",
            "HostKeyAlgorithms=ssh-ed25519",
            "PubkeyAcceptedAlgorithms=ssh-ed25519",
            "IdentityAgent=none",
            "IdentitiesOnly=yes",
            "ProxyCommand=none",
            "ProxyJump=none",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "ForwardAgent=no",
            "ForwardX11=no",
            "ClearAllForwardings=yes",
            "PermitLocalCommand=no",
            "RequestTTY=no",
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
        ):
            self.assertIn(option, command)
        self.assertEqual(checked.call_args.kwargs["stdin"], probe)
        self.assertEqual(checked.call_args.kwargs["timeout"], 37)
        self.assertEqual(
            checked.call_args.kwargs["env"], harness.production_transport_env
        )
        for forbidden in (
            ".arc-recovery-rollout-helpers",
            "mkdir",
            "mktemp",
            "/bin/ln",
            "/bin/chmod",
        ):
            self.assertNotIn(forbidden, command[-1])
        self.assertEqual(harness._assert_production_ssh_transport.call_count, 2)

        with mock.patch.object(rollout, "run_checked") as unchecked:
            with self.assertRaisesRegex(rollout.RolloutError, "unsafe remote argument"):
                harness.ssh_read_only(node, "exit 0\n", ("unsafe value",))
            unchecked.assert_not_called()

        harness._assert_production_ssh_transport.reset_mock()
        with mock.patch.object(rollout, "run_checked", return_value=completed) as persisted:
            harness.ssh(node, "exit 0\n")
        self.assertIn(
            "/root/.arc-recovery-rollout-helpers",
            persisted.call_args.args[0][-1],
        )
        self.assertEqual(harness._assert_production_ssh_transport.call_count, 2)

    def test_run_plan_and_invalid_go_never_enter_execute_boundary(self) -> None:
        value = self.fixture(production=True)
        locked_hash = "d" * 64
        archive_hash = "2" * 64
        rollback_journal = self.root / "rollback-plan-boundary"
        evidence_output = self.root / "reward-evidence.json"
        base_args = [
            "run",
            "--manifest", str(self.root / "locked.json"),
            "--rollback-journal", str(rollback_journal),
            "--reward-evidence-output", str(evidence_output),
        ]

        def invoke(arguments, environment):
            harness = mock.Mock()
            harness.preflight.return_value = archive_hash
            stdout = io.StringIO()
            stderr = io.StringIO()
            with (
                mock.patch.object(
                    rollout, "load_sealed_manifest", return_value=(value, locked_hash)
                ),
                mock.patch.object(rollout, "RecoveryRollout", return_value=harness),
                mock.patch.dict(os.environ, environment, clear=True),
                mock.patch.object(sys, "stdout", stdout),
                mock.patch.object(sys, "stderr", stderr),
            ):
                result = rollout.main(arguments)
            return result, harness, stdout.getvalue(), stderr.getvalue()

        result, planned, stdout, stderr = invoke(base_args, {})
        self.assertEqual(result, 0, stderr)
        planned.preflight.assert_called_once_with()
        planned.reserve_reward_evidence_output.assert_not_called()
        planned.execute.assert_not_called()
        self.assertIn("no persistent recovery-managed change", stdout)
        self.assertFalse(rollback_journal.exists())
        self.assertFalse(evidence_output.exists())

        execute_args = [
            *base_args,
            "--execute",
            "--go-hash", locked_hash,
            "--archive-manifest-sha256", archive_hash,
        ]
        result, rejected, _stdout, stderr = invoke(
            execute_args, {"ARC_RECOVERY_GO": "wrong"}
        )
        self.assertEqual(result, 1)
        self.assertIn("execution requires ARC_RECOVERY_GO", stderr)
        rejected.preflight.assert_called_once_with()
        rejected.reserve_reward_evidence_output.assert_not_called()
        rejected.execute.assert_not_called()
        self.assertFalse(rollback_journal.exists())
        self.assertFalse(evidence_output.exists())

        authorization = rollout.execution_authorization(
            value, locked_hash, archive_hash
        )
        result, approved, _stdout, stderr = invoke(
            execute_args, {"ARC_RECOVERY_GO": authorization}
        )
        self.assertEqual(result, 0, stderr)
        self.assertEqual(
            approved.method_calls,
            [
                mock.call.describe_plan(),
                mock.call.preflight(),
                mock.call.reserve_reward_evidence_output(),
                mock.call.execute(),
            ],
        )

    def test_runtime_uses_six_explicit_origins_and_restart_omits_checkpoint(self) -> None:
        value = self.fixture()
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        argv = harness.runtime_argv(value["validators"][0])
        self.assertEqual(argv.count("--community-rpc-url"), 6)
        self.assertNotIn("--archive", argv)
        self.assertNotIn("--recovery-checkpoint", argv)
        self.assertNotIn("--approved-recovery-manifest-hash", argv)
        imported = harness.recovery_cli("import", value["validators"][0])
        self.assertIn("--approved-manifest-hash", imported)
        self.assertIn("--data-dir", imported)

    def test_local_stop_is_term_only_and_fails_closed_on_timeout(self) -> None:
        value = self.fixture()
        output = io.StringIO()
        harness = rollout.RecoveryRollout(value, "d" * 64, output=output)
        node = value["validators"][0]
        name = node["name"]

        process = mock.Mock()
        process.pid = 4242
        process.poll.return_value = None
        process.wait.return_value = 0
        handle = mock.Mock()
        harness.processes[name] = process
        harness.logs[name] = handle

        with mock.patch.object(rollout.os, "killpg") as killpg:
            harness.stop_local(node)
        killpg.assert_called_once_with(4242, rollout.signal.SIGTERM)
        process.wait.assert_called_once_with(
            timeout=rollout.NODE_GRACEFUL_STOP_TIMEOUT_SECONDS
        )
        handle.close.assert_called_once_with()
        self.assertNotIn(name, harness.logs)

        timed_out = mock.Mock()
        timed_out.pid = 4343
        timed_out.poll.return_value = None
        timed_out.wait.side_effect = rollout.subprocess.TimeoutExpired(
            "arc-node", rollout.NODE_GRACEFUL_STOP_TIMEOUT_SECONDS
        )
        timed_out_handle = mock.Mock()
        harness.processes[name] = timed_out
        harness.logs[name] = timed_out_handle
        with mock.patch.object(rollout.os, "killpg") as killpg:
            with self.assertRaisesRegex(rollout.RolloutError, "refusing SIGKILL"):
                harness.stop_local(node)
        killpg.assert_called_once_with(4343, rollout.signal.SIGTERM)
        timed_out_handle.close.assert_not_called()
        self.assertIs(harness.logs[name], timed_out_handle)

    def test_production_service_lifecycle_timeouts_cover_full_node_drain(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        node = value["validators"][0]
        observed: list[tuple[str, int]] = []

        def delayed_ssh(remote_node, script, args=(), *, timeout=180):
            self.assertIs(remote_node, node)
            self.assertIn('systemctl "$action" "$service"', script)
            self.assertIn("ss -H -lunp", script)
            self.assertIn("stably owned before %s timeout", script)
            self.assertEqual(
                args[1:3], (node["service_name"], str(node["p2p_port"]))
            )
            self.assertEqual(int(args[3]), timeout - 5)
            observed.append((args[0], timeout))
            return ""

        harness.ssh = delayed_ssh
        for action in ("start", "stop", "restart"):
            harness.production_service(node, action)

        self.assertEqual(
            observed,
            [
                ("start", rollout.NODE_SERVICE_START_TIMEOUT_SECONDS),
                ("stop", rollout.NODE_SERVICE_STOP_TIMEOUT_SECONDS),
                ("restart", rollout.NODE_SERVICE_RESTART_TIMEOUT_SECONDS),
            ],
        )
        self.assertEqual(rollout.NODE_SERVICE_START_TIMEOUT_SECONDS, 90)
        self.assertEqual(rollout.COMMUNITY_LATE_SUBMIT_GRACE_SECONDS, 300)
        self.assertEqual(rollout.NODE_GRACEFUL_STOP_TIMEOUT_SECONDS, 4420)
        self.assertGreater(
            rollout.NODE_SERVICE_STOP_TIMEOUT_SECONDS,
            rollout.NODE_GRACEFUL_STOP_TIMEOUT_SECONDS,
        )
        self.assertGreater(
            rollout.NODE_SERVICE_RESTART_TIMEOUT_SECONDS,
            rollout.NODE_GRACEFUL_STOP_TIMEOUT_SECONDS,
        )
        self.assertGreater(
            rollout.PRODUCTION_ROLLBACK_TIMEOUT_SECONDS,
            rollout.NODE_GRACEFUL_STOP_TIMEOUT_SECONDS,
        )

    def test_production_start_and_restart_reject_bad_udp_quic_ownership(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        node = value["validators"][0]
        captured: dict[str, str] = {}

        def capture(_node, script, _args=(), **_kwargs):
            captured["script"] = script
            return ""

        harness.ssh = mock.Mock(side_effect=capture)
        harness.production_service(node, "start")
        script = captured["script"]

        def exercise(
            action: str, udp_rows: str, unix_rows: str, *, main_pid: str = "4242"
        ):
            rpc_socket = self.root / f"{action}-rpc.sock"
            if rpc_socket.exists():
                rpc_socket.unlink()
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(os.fspath(rpc_socket))
            shell = f'''systemctl() {{
  case "$*" in
    *ActiveState*) printf 'active\\n' ;;
    *MainPID*) printf '%s\\n' "$ARC_TEST_MAIN_PID" ;;
    *) return 0 ;;
  esac
}}
ss() {{
  case " $* " in
    *" -lunp "*) printf '%s\\n' "$ARC_TEST_UDP_ROWS" ;;
    *" -lxnp "*) printf '%s\\n' "$ARC_TEST_UNIX_ROWS" ;;
    *) printf '\\n' ;;
  esac
}}
stat() {{
  case "$*" in
    *%U:%G:%a:%h*) printf 'root:{rollout.RPC_ORIGIN_GROUP}:660:1\\n' ;;
    *) printf 'root:{rollout.RPC_ORIGIN_GROUP}:750\\n' ;;
  esac
}}
sleep() {{ :; }}
{script}
'''
            environment = dict(
                os.environ,
                ARC_TEST_UDP_ROWS=udp_rows,
                ARC_TEST_UNIX_ROWS=unix_rows,
                ARC_TEST_MAIN_PID=main_pid,
            )
            try:
                return rollout.subprocess.run(
                    [
                        "/bin/sh",
                        "-c",
                        shell,
                        "sh",
                        action,
                        node["service_name"],
                        str(node["p2p_port"]),
                        "3",
                        os.fspath(rpc_socket),
                        "root",
                        rollout.RPC_ORIGIN_GROUP,
                        node["rpc_listen"].rsplit(":", 1)[1],
                    ],
                    env=environment,
                    text=True,
                    capture_output=True,
                    check=False,
                )
            finally:
                listener.close()
                if rpc_socket.exists():
                    rpc_socket.unlink()

        owned = 'UNCONN 0 0 0.0.0.0:10001 0.0.0.0:* users:(("arc-node",pid=4242,fd=9))'
        foreign = 'UNCONN 0 0 0.0.0.0:10001 0.0.0.0:* users:(("foreign",pid=9999,fd=4))'
        for action in ("start", "restart"):
            socket_path = self.root / f"{action}-rpc.sock"
            unix_owned = f'u_str LISTEN 0 128 {socket_path} 0 * users:(("arc-node",pid=4242,fd=10))'
            unix_foreign = f'u_str LISTEN 0 128 {socket_path} 0 * users:(("foreign",pid=9999,fd=10))'
            self.assertEqual(exercise(action, owned, unix_owned).returncode, 0)
            missing = exercise(action, "", "")
            self.assertNotEqual(missing.returncode, 0)
            self.assertIn("did not become stably owned", missing.stderr)
            wrong = exercise(action, foreign, unix_owned)
            self.assertNotEqual(wrong.returncode, 0)
            self.assertIn("foreign", wrong.stderr)
            duplicate = exercise(action, f"{owned}\n{foreign}", unix_owned)
            self.assertNotEqual(duplicate.returncode, 0)
            self.assertIn("duplicate UDP rows", duplicate.stderr)
            unix_wrong = exercise(action, owned, unix_foreign)
            self.assertNotEqual(unix_wrong.returncode, 0)
            self.assertIn("Unix RPC row is foreign", unix_wrong.stderr)
            unix_duplicate = exercise(
                action, owned, f"{unix_owned}\n{unix_foreign}"
            )
            self.assertNotEqual(unix_duplicate.returncode, 0)
            self.assertIn("duplicate Unix rows", unix_duplicate.stderr)
            zero_pid = exercise(action, owned, unix_owned, main_pid="0")
            self.assertNotEqual(zero_pid.returncode, 0)
            self.assertIn("did not become stably owned", zero_pid.stderr)

    def test_runtime_inventory_reproves_current_quic_pid_and_validator_key(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        captured: list[tuple[str, tuple[str, ...]]] = []
        harness.ssh = mock.Mock(
            side_effect=lambda _node, script, args=(), **_kwargs: captured.append(
                (script, tuple(args))
            )
            or ""
        )
        harness.prove_production_runtime_inventory()
        self.assertEqual(len(captured), 6)
        script, args = captured[0]
        node = value["validators"][0]
        receipt = value["provenance"]["validator_key_receipt_chain"]["validators"][0]
        self.assertIn("ss -H -lunp", script)
        self.assertIn("exactly one UDP QUIC row", script)
        self.assertIn('keygen --verify-keyfile "$key"', script)
        self.assertIn('stat -c %U:%G:%a:%h "$key"', script)
        self.assertEqual(args[9], str(node["p2p_port"]))
        self.assertEqual(args[10:13], (node["key_file"], receipt["keyfile_sha256"], receipt["address"]))
        self.assertEqual(args[13], f"/root/arc-recovery-seal/{value['archive']['prearchive_rollout_sha256']}/{node['name']}/arc-cli")
        self.assertEqual(args[14], value["artifacts"]["cli"]["sha256"])

        quic = "rows=$(ss -H -lunp" + script.split("rows=$(ss -H -lunp", 1)[1].split(
            "exact_unix_listener()", 1
        )[0]

        def exercise(rows: str):
            shell = f'''set -eu
pid=4242
p2p_port=10001
ss() {{ printf '%s\\n' "$ARC_TEST_ROWS"; }}
{quic}
'''
            return rollout.subprocess.run(
                ["/bin/sh", "-c", shell],
                env=dict(os.environ, ARC_TEST_ROWS=rows),
                text=True,
                capture_output=True,
                check=False,
            )

        owned = 'UNCONN 0 0 0.0.0.0:10001 0.0.0.0:* users:(("arc-node",pid=4242,fd=9))'
        foreign = 'UNCONN 0 0 0.0.0.0:10001 0.0.0.0:* users:(("foreign",pid=9999,fd=4))'
        self.assertEqual(exercise(owned).returncode, 0)
        self.assertNotEqual(exercise("").returncode, 0)
        self.assertNotEqual(exercise(foreign).returncode, 0)
        self.assertNotEqual(exercise(f"{owned}\n{foreign}").returncode, 0)

        key_gate = 'test -f "$key"' + script.split('test -f "$key"', 1)[1].split(
            'test -f "$identity_cli"', 1
        )[0]
        key = self.root / "swapped-valid-mode-key.json"
        key.write_bytes(b"swapped")
        key.chmod(0o600)
        key_shell = f'''set -eu
key=$ARC_TEST_KEY
key_sha={'a' * 64}
stat() {{ printf 'root:root:600:1\\n'; }}
sha256sum() {{ printf '%s  %s\\n' "{'b' * 64}" "$1"; }}
{key_gate}
'''
        swapped = rollout.subprocess.run(
            ["/bin/sh", "-c", key_shell],
            env=dict(os.environ, ARC_TEST_KEY=str(key)),
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertNotEqual(swapped.returncode, 0)

    def test_local_rehearsal_restarts_all_six_and_requires_post_restart_advance(self) -> None:
        value = self.fixture()
        output = io.StringIO()
        harness = rollout.RecoveryRollout(value, "d" * 64, output=output)
        validators = value["validators"]

        harness.import_local = mock.Mock()
        harness.start_local = mock.Mock()
        harness.stop_local = mock.Mock()
        harness.wait_nodes_ready = mock.Mock()
        harness.prove_boundary = mock.Mock()
        harness.prove_advancing_convergence = mock.Mock()
        harness.prove_reward_policy = mock.Mock()
        harness.obtain_receipt_evidence = mock.Mock(return_value=None)
        convergence = []
        expected_waits = []
        for index in range(6):
            before = 200 + index * 2
            convergence.extend(
                [
                    (before, "a" * 64, "b" * 64),
                    (before + 1, "c" * 64, "b" * 64),
                ]
            )
            expected_waits.extend(
                [
                    mock.call(),
                    mock.call(
                        minimum_height=before + value["checks"]["min_height_advance"],
                        timeout=value["checks"]["restart_timeout_seconds"],
                    ),
                ]
            )
        harness.wait_convergence = mock.Mock(side_effect=convergence)

        harness.execute_local()

        self.assertEqual(
            harness.import_local.call_args_list,
            [mock.call(node) for node in validators],
        )
        for node in validators:
            self.assertEqual(
                [call.args[0] for call in harness.start_local.call_args_list].count(node),
                2,
            )
        self.assertEqual(
            harness.wait_nodes_ready.call_args_list,
            [mock.call()]
            + [
                mock.call(timeout=value["checks"]["restart_timeout_seconds"])
                for _ in validators
            ],
        )
        self.assertEqual(harness.wait_convergence.call_args_list, expected_waits)
        self.assertEqual(
            harness.stop_local.call_args_list[:6],
            [mock.call(node) for node in validators],
        )
        self.assertEqual(
            harness.stop_local.call_args_list[6:],
            [mock.call(node, strict=False) for node in reversed(validators)],
        )
        harness.prove_reward_policy.assert_called_once_with()
        self.assertIn("COMPLETE local rehearsal", output.getvalue())

    def test_dynamic_reward_probe_is_hash_pinned(self) -> None:
        value = self.fixture(reward_receipt=True)
        probe = self.root / "reward-probe"
        probe.write_text("#!/bin/sh\nprintf '{}\\n'\n", encoding="utf-8")
        probe.chmod(0o700)
        value["checks"]["reward"] = {
            "mode": "receipt",
            "expect_protocol_active": True,
            "expect_issuance_ready": True,
            "probe_argv": [str(probe)],
            "probe_sha256": digest(probe),
            "expected_reward_base": 2_500_000_000,
        }
        rollout.validate_manifest(value)
        rollout.verify_artifacts(value)
        probe.write_text("changed", encoding="utf-8")
        with self.assertRaisesRegex(rollout.RolloutError, "reward probe sha256 mismatch"):
            rollout.verify_artifacts(value)

    def test_checkpoint_cli_output_must_match_every_locked_commitment(self) -> None:
        value = self.fixture()
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        body = {
            "status": "VERIFIED_QUORUM",
            "manifest_hash": value["chain"]["approved_checkpoint_manifest_hash"],
            "genesis_hash": value["chain"]["genesis_hash"],
            "full_state_root": value["chain"]["full_state_root"],
            "source_height": 100,
            "source_consensus_round": 9876,
            "created_at_unix_ms": 1_787_857_623_000,
            "source_block_hash": value["chain"]["source_block_hash"],
            "source_state_root": value["chain"]["source_state_root"],
            "transition_height": 101,
            "transition_block_hash": value["chain"]["transition_block_hash"],
            "recovery_domain": value["chain"]["recovery_domain"],
            "recovery_epoch": 7,
            "validator_set_id": 9,
            "protocol_version": "3.0.0",
            "validator_count": 6,
            "signature_count": 5,
        }
        inspected = dict(body, status="UNTRUSTED_INSPECTION")
        with mock.patch.object(rollout, "run_checked", side_effect=[SimpleNamespace(stdout=json.dumps(inspected)), SimpleNamespace(stdout=json.dumps(body))]):
            harness.verify_checkpoint()
        bad = dict(body, transition_height=102)
        with mock.patch.object(rollout, "run_checked", side_effect=[SimpleNamespace(stdout=json.dumps(inspected)), SimpleNamespace(stdout=json.dumps(bad))]):
            with self.assertRaisesRegex(rollout.RolloutError, "transition_height differs"):
                harness.verify_checkpoint()

    def test_same_height_hash_or_root_disagreement_is_a_fork(self) -> None:
        value = self.fixture()
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())

        def response(node, path, timeout=10):
            if path == "/network/info":
                return {
                    "chain_id": value["chain"]["chain_id"],
                    "protocol_version": "3.0.0",
                    "recovery_active": True,
                    "recovery_epoch": 7,
                    "validator_set_id": 9,
                    "validators_active": 6,
                    "checkpoint_manifest_hash": value["chain"]["approved_checkpoint_manifest_hash"],
                    "recovery_domain": value["chain"]["recovery_domain"],
                    "last_block_height": 110,
                }
            return {
                "header": {
                    "height": 110,
                    "hash": "0x" + "a" * 64,
                    "state_root": "0x" + (("b" if node["name"] != "validator-6" else "c") * 64),
                }
            }

        harness._http_json = response
        with self.assertRaisesRegex(rollout.RolloutError, "same-height fork"):
            harness.common_commitment()

    def test_boundary_requires_exact_sealed_source_state_root(self) -> None:
        value = self.fixture()
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        wrong_node: str | None = None

        def response(node, path, timeout=10):
            height = int(path.rsplit("/", 1)[1])
            if height == value["chain"]["source_height"]:
                root = (
                    "0x" + "f" * 64
                    if node["name"] == wrong_node
                    else value["chain"]["source_state_root"]
                )
                return {
                    "header": {
                        "height": height,
                        "hash": value["chain"]["source_block_hash"],
                        "state_root": root,
                    }
                }
            return {
                "header": {
                    "height": height,
                    "hash": value["chain"]["transition_block_hash"],
                    "state_root": value["chain"]["full_state_root"],
                    "parent_hash": value["chain"]["source_block_hash"],
                }
            }

        harness._http_json = mock.Mock(side_effect=response)
        harness.prove_boundary()
        wrong_node = value["validators"][3]["name"]
        with self.assertRaisesRegex(rollout.RolloutError, "block hash/root at H"):
            harness.prove_boundary()

    def test_advancing_convergence_starts_strictly_above_legacy_public_maximum(self) -> None:
        value = self.fixture()
        output = io.StringIO()
        harness = rollout.RecoveryRollout(value, "d" * 64, output=output)
        harness.wait_convergence = mock.Mock(
            side_effect=[
                (111, "a" * 64, "b" * 64),
                (112, "c" * 64, "d" * 64),
            ]
        )

        with mock.patch.object(rollout.time, "monotonic", side_effect=[0, 0, 0, 0, 2]):
            harness.prove_advancing_convergence()

        self.assertEqual(
            harness.wait_convergence.call_args_list,
            [mock.call(minimum_height=111), mock.call(timeout=10)],
        )
        self.assertIn("#111 > #110", output.getvalue())
        self.assertIn("#111 -> #112", output.getvalue())

    def test_frontend_config_binds_source_boundary_domain_and_all_six_v3_replicas(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        reward_evidence = self.two_receipt_evidence()
        config = harness.frontend_config()
        checkpoint = config["checkpoint"]
        self.assertEqual(checkpoint["height"], value["chain"]["source_height"])
        self.assertEqual(checkpoint["recoveryHeight"], value["chain"]["transition_height"])
        self.assertEqual(
            checkpoint["legacyPublicMaxHeight"],
            value["chain"]["legacy_public_max_height"],
        )
        self.assertEqual(
            checkpoint["boundaryBlockHash"], value["chain"]["transition_block_hash"].removeprefix("0x")
        )
        self.assertEqual(
            checkpoint["boundaryStateRoot"], value["chain"]["full_state_root"].removeprefix("0x")
        )
        self.assertEqual(checkpoint["recoveryEpoch"], 7)
        self.assertEqual(checkpoint["validatorSetId"], 9)
        self.assertEqual(checkpoint["protocolVersion"], "3.0.0")
        self.assertEqual(
            checkpoint["recoveryDomain"], value["chain"]["recovery_domain"].removeprefix("0x")
        )
        self.assertEqual(checkpoint["legacySourceId"], checkpoint["v3SourceId"])
        self.assertEqual(len(config["sources"]), 6)
        self.assertTrue(all(source["kind"] == "v3" for source in config["sources"]))
        self.assertEqual(
            {source["baseUrl"] for source in config["sources"]},
            {node["rpc_url"] for node in value["validators"]},
        )
        self.assertEqual(
            config["services"]["maintenanceInterlock"],
            {
                "schema": "arc.frontend.maintenance-interlock.v1",
                "path": "/maintenance/status",
                "sourceMainCommit": value["provenance"]["source_main_commit"],
                "observedCutoffHeight": value["chain"][
                    "legacy_observed_cutoff_height"
                ],
                "sourceSetSha256": value["chain"][
                    "legacy_late_fork_source_set_sha256"
                ],
                "boundarySha256": value["chain"][
                    "legacy_maintenance_boundary_sha256"
                ],
                "toolSha256": value["artifacts"][
                    "legacy_late_fork_interlock_tool"
                ]["sha256"],
                "requiredHealthyReplicas": 6,
                "maxStalenessSeconds": 90,
            },
        )

        fork_value = copy.deepcopy(value)
        fork_value["archive"].update(
            {
                "archive_manifest_sha256": "a" * 64,
                "complete_sha256": "b" * 64,
                "prearchive_rollout_sha256": "c" * 64,
            }
        )
        fork_harness = rollout.RecoveryRollout(fork_value, "d" * 64, output=io.StringIO())
        proof = {
            "schema": "arc.legacy-archive.query.v1",
            "read_only": True,
            "classification": "valid_noncanonical_fork",
            "capture_id": fork_value["archive"]["capture_id"],
            "node": "nyc",
            "rollout_manifest_sha256": "c" * 64,
            "archive_manifest_sha256": "a" * 64,
            "complete_sha256": "b" * 64,
            "bundle_sha256": "1" * 64,
            "inventory_sha256": "2" * 64,
            "binding_index_sha256": "3" * 64,
            "binding_sha256": "4" * 64,
            "checkpoint_sha256": "5" * 64,
            "checkpoint_manifest_hash": "6" * 64,
            "checkpoint_payload_hash": "7" * 64,
            "canonical_checkpoint_height": fork_value["chain"]["source_height"],
            "source_height": fork_value["chain"]["legacy_public_max_height"],
            "source_block_hash": "8" * 64,
            "source_state_root": "9" * 64,
            "source_consensus_round": 9_999,
            "recovery_epoch": fork_value["chain"]["recovery_epoch"],
            "validator_set_id": fork_value["chain"]["validator_set_id"],
        }
        archived_fork = {
            "node": "nyc",
            "bundle_sha256": "1" * 64,
            "inventory_sha256": "2" * 64,
        }
        def archive_browser_boundary(
            node, path, *, method, origin=None, data=None, timeout=20
        ):
            if method in {"HEAD", "POST", "OPTIONS"}:
                return 405, {}
            if method == "GET" and data is not None and len(data) > 1024:
                return 413, {}
            if origin == rollout.PUBLIC_BROWSER_ORIGIN:
                return 200, {
                    "Access-Control-Allow-Origin": rollout.PUBLIC_BROWSER_ORIGIN,
                    "Vary": "Origin",
                }
            return 200, {}

        fork_harness._http_json = mock.Mock(return_value=proof)
        fork_harness._http_status_headers = mock.Mock(side_effect=archive_browser_boundary)
        fork_config = fork_harness.frontend_config([archived_fork])
        self.assertEqual(len(fork_config["sources"]), 7)
        fork_source = fork_config["sources"][-1]
        self.assertEqual(fork_source["kind"], "legacy-fork")
        self.assertEqual(
            fork_source["baseUrl"],
            value["validators"][0]["rpc_url"] + "/legacy/nyc",
        )
        self.assertEqual(fork_source["archive"]["completeSha256"], "b" * 64)
        self.assertEqual(fork_source["archive"]["bindingSha256"], "4" * 64)
        self.assertEqual(
            fork_source["archive"]["canonicalCheckpointHeight"],
            fork_value["chain"]["source_height"],
        )
        calls = fork_harness._http_status_headers.call_args_list
        self.assertTrue(any(call.kwargs.get("method") == "OPTIONS" for call in calls))
        self.assertTrue(
            any(
                call.kwargs.get("method") == "GET"
                and len(call.kwargs.get("data") or b"") == 1025
                for call in calls
            )
        )
        for archive_height in (
            fork_value["chain"]["source_height"],
            fork_value["chain"]["source_height"] - 1,
        ):
            equal_or_lower = copy.deepcopy(proof)
            equal_or_lower["source_height"] = archive_height
            fork_harness._http_json.return_value = equal_or_lower
            exposed = fork_harness.frontend_config([archived_fork])["sources"][-1]
            self.assertEqual(exposed["archive"]["sourceHeight"], archive_height)
            self.assertEqual(
                exposed["archive"]["canonicalCheckpointHeight"],
                fork_value["chain"]["source_height"],
            )
        incomplete = copy.deepcopy(proof)
        incomplete.pop("binding_sha256")
        fork_harness._http_json = mock.Mock(return_value=incomplete)
        with self.assertRaisesRegex(rollout.RolloutError, "missing: binding_sha256"):
            fork_harness.frontend_config([archived_fork])
        wrong_root = copy.deepcopy(proof)
        wrong_root["complete_sha256"] = "f" * 64
        fork_harness._http_json = mock.Mock(return_value=wrong_root)
        with self.assertRaisesRegex(rollout.RolloutError, "COMPLETE root differs"):
            fork_harness.frontend_config([archived_fork])
        forged_bundle = copy.deepcopy(proof)
        forged_bundle["bundle_sha256"] = "f" * 64
        fork_harness._http_json = mock.Mock(return_value=forged_bundle)
        with self.assertRaisesRegex(rollout.RolloutError, "bundle root differs"):
            fork_harness.frontend_config([archived_fork])
        fork_harness._http_json = mock.Mock(return_value=proof)
        fork_harness._http_status_headers = mock.Mock(
            side_effect=lambda node, path, *, method, origin=None, data=None, timeout=20: (
                (405, {})
                if method in {"HEAD", "POST", "OPTIONS"}
                else (413, {})
                if method == "GET" and data is not None and len(data) > 1024
                else (
                    200,
                    {
                        "Access-Control-Allow-Origin": (
                            rollout.PUBLIC_BROWSER_ORIGIN
                            if origin == rollout.PUBLIC_BROWSER_ORIGIN
                            else "https://attacker.invalid"
                        ),
                        "Vary": "Origin",
                    },
                )
            )
        )
        with self.assertRaisesRegex(rollout.RolloutError, "unsealed browser origin"):
            fork_harness.frontend_config([archived_fork])

        blocked = self.root / "frontend.blocked.json"
        harness.verify_live = mock.Mock(side_effect=rollout.RolloutError("height gate pending"))
        with self.assertRaisesRegex(rollout.RolloutError, "height gate pending"):
            rollout.write_frontend_config(
                harness, blocked, reward_evidence=reward_evidence
            )
        self.assertFalse(blocked.exists())
        self.assertFalse(Path(str(blocked) + ".sha256").exists())

        output = self.root / "frontend.lock.json"
        harness.verify_live = mock.Mock()
        digest_value = rollout.write_frontend_config(
            harness, output, reward_evidence=reward_evidence
        )
        harness.verify_live.assert_called_once_with(reward_evidence)
        self.assertEqual(digest_value, digest(output))
        self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o444)
        self.assertEqual(stat.S_IMODE(Path(str(output) + ".sha256").stat().st_mode), 0o444)
        with self.assertRaisesRegex(rollout.RolloutError, "refusing replacement"):
            rollout.write_frontend_config(harness, output)
        harness.verify_live.assert_called_once_with(reward_evidence)

    def test_finalized_archive_files_derive_safe_path_fork_sources(self) -> None:
        value = self.fixture(production=True)
        reward_evidence = self.two_receipt_evidence()
        value["archive"]["prearchive_rollout_sha256"] = "c" * 64
        rows = [
            {
                "node": node["name"],
                "classification": (
                    "valid_noncanonical_fork" if index == 0 else "valid_canonical"
                ),
                "bundle": {
                    "name": f"legacy-{node['name']}.tar.zst",
                    "size": 1,
                    "sha256": "1" * 64,
                    "sidecar_name": f"legacy-{node['name']}.tar.zst.sha256",
                    "sidecar_sha256": "a" * 64,
                },
                "inventory": {
                    "name": f"legacy-{node['name']}.inventory",
                    "size": 1,
                    "sha256": "2" * 64,
                    "sidecar_name": f"legacy-{node['name']}.inventory.sha256",
                    "sidecar_sha256": "b" * 64,
                },
            }
            for index, node in enumerate(value["validators"])
        ]
        archive_manifest = {
            "schema": "arc.recovery.archive-manifest.v2",
            "freeze_plan_sha256": value["archive"]["freeze_plan_sha256"],
            "capture_id": value["archive"]["capture_id"],
            "rollout_manifest_sha256": "c" * 64,
            "source_commit": "1" * 40,
            "orchestrator_sha256": "2" * 64,
            "remote_helper_sha256": "3" * 64,
            "rollout_tool_sha256": "4" * 64,
            "rollout_schema_sha256": "5" * 64,
            "canonical_reference": {},
            "capture_classification_counts": {
                "valid_canonical": 5,
                "valid_noncanonical_fork": 1,
                "preserved_unclassified": 0,
            },
            "shared_inputs": [],
            "validator_bundles": rows,
            "sha256sums": {},
        }
        archive_path = self.root / "ARCHIVE-MANIFEST.json"
        archive_path.write_bytes(rollout.canonical_bytes(archive_manifest))
        archive_path.chmod(0o444)
        archive_sha = digest(archive_path)
        complete = {
            "schema": "arc.recovery.archive-complete.v2",
            "freeze_plan_sha256": archive_manifest["freeze_plan_sha256"],
            "capture_id": archive_manifest["capture_id"],
            "rollout_manifest_sha256": archive_manifest["rollout_manifest_sha256"],
            "source_commit": archive_manifest["source_commit"],
            "archive_manifest_sha256": archive_sha,
            "object_count_before_complete": 1,
            "validator_bundle_count": 6,
            "finalization_anchor": {
                "intent_sha256": "7" * 64,
                "gist_id": "8" * 32,
                "gist_revision": "9" * 40,
                "gist_file_sha256": "7" * 64,
            },
        }
        complete_path = self.root / "COMPLETE.json"
        complete_path.write_bytes(rollout.canonical_bytes(complete))
        complete_path.chmod(0o444)
        value["archive"]["archive_manifest_sha256"] = archive_sha
        value["archive"]["complete_sha256"] = digest(complete_path)
        value["archive"]["sha256sums_sha256"] = "6" * 64

        hostile_anchors = {
            "zero": ({**complete["finalization_anchor"], "intent_sha256": "0" * 64,
                      "gist_file_sha256": "0" * 64}, "intent_sha256 is malformed"),
            "mismatch": ({**complete["finalization_anchor"], "gist_file_sha256": "a" * 64},
                         "Gist file hash differs"),
            "gist-id": ({**complete["finalization_anchor"], "gist_id": "8" * 19},
                        "gist_id is malformed"),
            "revision": ({**complete["finalization_anchor"], "gist_revision": "9" * 39},
                         "gist_revision is malformed"),
        }
        for label, (anchor, message) in hostile_anchors.items():
            hostile_complete = copy.deepcopy(complete)
            hostile_complete["finalization_anchor"] = anchor
            hostile_path = self.root / f"COMPLETE-{label}.json"
            hostile_path.write_bytes(rollout.canonical_bytes(hostile_complete))
            hostile_path.chmod(0o444)
            hostile_value = copy.deepcopy(value)
            hostile_value["archive"]["complete_sha256"] = digest(hostile_path)
            hostile_harness = rollout.RecoveryRollout(
                hostile_value, "d" * 64, output=io.StringIO()
            )
            with self.subTest(anchor=label), self.assertRaisesRegex(
                rollout.RolloutError, message
            ):
                rollout.load_legacy_archive_fork_nodes(
                    hostile_harness, archive_path, hostile_path
                )

        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        harness.verify_live = mock.Mock()
        proof = {
            "schema": "arc.legacy-archive.query.v1",
            "read_only": True,
            "classification": "valid_noncanonical_fork",
            "capture_id": value["archive"]["capture_id"],
            "node": value["validators"][0]["name"],
            "rollout_manifest_sha256": "c" * 64,
            "archive_manifest_sha256": archive_sha,
            "complete_sha256": value["archive"]["complete_sha256"],
            "bundle_sha256": "1" * 64,
            "inventory_sha256": "2" * 64,
            "binding_index_sha256": "3" * 64,
            "binding_sha256": "4" * 64,
            "checkpoint_sha256": "5" * 64,
            "checkpoint_manifest_hash": "6" * 64,
            "checkpoint_payload_hash": "7" * 64,
            "canonical_checkpoint_height": value["chain"]["source_height"],
            "source_height": value["chain"]["legacy_public_max_height"],
            "source_block_hash": "8" * 64,
            "source_state_root": "9" * 64,
            "source_consensus_round": 9_999,
            "recovery_epoch": value["chain"]["recovery_epoch"],
            "validator_set_id": value["chain"]["validator_set_id"],
        }
        harness._http_json = mock.Mock(return_value=proof)
        harness._http_status_headers = mock.Mock(
            side_effect=lambda node, path, *, method, origin=None, data=None, timeout=20: (
                (405, {})
                if method in {"HEAD", "POST", "OPTIONS"}
                else (413, {})
                if method == "GET" and data is not None and len(data) > 1024
                else (
                    200,
                    {
                        "Access-Control-Allow-Origin": rollout.PUBLIC_BROWSER_ORIGIN,
                        "Vary": "Origin",
                    },
                )
                if origin == rollout.PUBLIC_BROWSER_ORIGIN
                else (200, {})
            )
        )
        output = self.root / "archive-derived-frontend.json"
        rollout.write_frontend_config(
            harness,
            output,
            archive_manifest_path=archive_path,
            archive_complete_path=complete_path,
            reward_evidence=reward_evidence,
        )
        config = json.loads(output.read_text())
        fork = [source for source in config["sources"] if source["kind"] == "legacy-fork"]
        self.assertEqual(len(fork), 1)
        self.assertEqual(
            fork[0]["baseUrl"],
            value["validators"][0]["rpc_url"] + "/legacy/nyc",
        )

    def test_legacy_archive_inventory_and_generated_deployment_are_fail_closed(self) -> None:
        value = self.fixture(production=True)
        node = value["validators"][0]
        inventory = (
            f"manifest_sha256={'c' * 64}\n"
            f"capture_id={value['archive']['capture_id']}\n"
            f"node={node['name']}\n"
            "classification=valid_noncanonical_fork\n"
            "canonical_match=false\n"
            "archive_scope=complete-content-indexed-stopped-legacy-source-v4\n"
            "source_tree_retained_locally=true\n"
            "model_excluded_and_bound_by_rollout=true\n"
            f"capture_index_sha256={'1' * 64}\n"
            f"source_index_sha256={'2' * 64}\n"
            f"binding_index_sha256={'3' * 64}\n"
        ).encode()
        parsed = rollout.parse_legacy_archive_inventory(
            inventory,
            node=node["name"],
            capture_id=value["archive"]["capture_id"],
            rollout_manifest_sha256="c" * 64,
        )
        self.assertEqual(parsed["binding_index_sha256"], "3" * 64)
        with self.assertRaisesRegex(rollout.RolloutError, "wrong classification"):
            rollout.parse_legacy_archive_inventory(
                inventory.replace(
                    b"classification=valid_noncanonical_fork",
                    b"classification=valid_canonical",
                ),
                node=node["name"],
                capture_id=value["archive"]["capture_id"],
                rollout_manifest_sha256="c" * 64,
            )

        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        harness.legacy_archive_forks[node["name"]] = {
            "node": node["name"],
            "bundle_sha256": "4" * 64,
            "inventory_sha256": "5" * 64,
        }
        caddy = harness.caddyfile(node)
        nginx = harness.nginx_filter(node)
        unit = harness.legacy_archive_unit(node)
        self.assertIn(f"path /legacy/{node['name']}/*", caddy)
        self.assertIn(f"uri strip_prefix /legacy/{node['name']}", caddy)
        self.assertIn(
            f"reverse_proxy unix/{harness.filter_archive_socket(node)}",
            caddy,
        )
        self.assertEqual(caddy.count("max_size 1KB"), 2)
        self.assertIn("if ($request_method != GET) { return 405; }", nginx)
        self.assertIn("client_max_body_size 1k;", nginx)
        self.assertIn("limit_conn arc_conn_", nginx)
        self.assertIn(f"listen unix:{harness.filter_archive_socket(node)}", nginx)
        self.assertEqual(
            nginx.count("if ($arc_loopback_transport = 0) { return 403; }"),
            2,
        )
        self.assertNotIn("allow 127.0.0.1;", nginx)
        self.assertIn(
            f"proxy_pass http://unix:{harness.legacy_archive_rpc_socket(node)}",
            nginx,
        )
        self.assertNotIn(f"127.0.0.1:{rollout.LEGACY_ARCHIVE_RPC_PORT}", nginx)
        self.assertEqual(caddy.count("header_up X-Forwarded-For {remote_host}"), 10)
        self.assertIn(f"User={rollout.LEGACY_ARCHIVE_USER}", unit)
        self.assertIn(f"Group={rollout.RPC_ORIGIN_GROUP}", unit)
        self.assertIn(f"SupplementaryGroups={rollout.LEGACY_ARCHIVE_USER}", unit)
        self.assertIn("--listen-unix /run/arc-archive-rpc-", unit)
        self.assertIn("ProtectSystem=strict", unit)
        self.assertIn("CapabilityBoundingSet=", unit)
        self.assertIn("archive serve", unit)
        self.assertNotIn("--validator-key-file", unit)
        gateway_unit = harness.gateway_unit(node)
        self.assertIn(harness.legacy_archive_service_name(node), gateway_unit)

    def test_archive_staging_rejects_symlink_ancestors_and_pins_metadata(self) -> None:
        value = self.fixture(production=True)
        node = value["validators"][0]
        archive_manifest = b'{"archive":"sealed"}\n'
        complete = b'{"complete":"sealed"}\n'
        inventory = b'manifest_sha256=' + b"c" * 64 + b"\n"
        value["archive"]["archive_manifest_sha256"] = hashlib.sha256(
            archive_manifest
        ).hexdigest()
        value["archive"]["complete_sha256"] = hashlib.sha256(complete).hexdigest()
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        self.prime_interlock_interpreters(harness)
        harness.archive_manifest_payload = archive_manifest
        harness.archive_complete_payload = complete
        harness.legacy_archive_forks[node["name"]] = {
            "node": node["name"],
            "bundle_sha256": "4" * 64,
            "inventory_payload": inventory,
            "inventory_sha256": hashlib.sha256(inventory).hexdigest(),
            "binding_index_sha256": "3" * 64,
        }
        remote_scripts: list[str] = []

        def fake_ssh(remote_node, script, args=(), *, timeout=180):
            remote_scripts.append(script)
            if 'mktemp "$root/.${name}.upload.XXXXXX"' in script:
                return f"{args[0]}/.{args[1]}.upload.ABC123\n"
            return ""

        harness.ssh = mock.Mock(side_effect=fake_ssh)
        harness.scp = mock.Mock()
        harness._stage_legacy_archive_node(node)
        generated = "\n".join(remote_scripts)
        self.assertIn("test -d /var && test ! -L /var", generated)
        self.assertIn("test -d /var/lib && test ! -L /var/lib", generated)
        self.assertIn('test -e "$base" || test -L "$base"', generated)
        self.assertIn('root:root:755', generated)
        self.assertIn('root:$user:750', generated)
        self.assertIn('root:$user:440', generated)
        self.assertIn("expected_mode=${mode#0}", generated)
        self.assertIn('root:$user:$expected_mode', generated)
        self.assertIn("os.fsync(fd)", generated)
        self.assertIn("arc_semantic_python_revalidate", generated)
        self.assertIn("/usr/bin/env -i HOME=/root", generated)
        self.assertNotIn("python3 -", generated)

    def test_every_remote_semantic_python_uses_the_sealed_interpreter_fd(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        self.prime_interlock_interpreters(harness)
        node = value["validators"][0]
        prelude = harness.remote_semantic_python_prelude(node)
        interpreter = harness.late_fork_interlock_interpreter(node)
        self.assertIn(interpreter["normalized_path"], prelude)
        self.assertIn(interpreter["sha256"], prelude)
        self.assertIn("exec 9<\"$arc_semantic_python_path\"", prelude)
        self.assertIn("/proc/self/fd/9 -I", prelude)
        self.assertIn("/usr/bin/env -i HOME=/root", prelude)
        self.assertIn("PYTHONHASHSEED=0", prelude)
        source = MODULE_PATH.read_text(encoding="utf-8")
        self.assertNotIn("python3 -", source)

    def test_production_preflight_rejects_foreign_later_port_listeners(self) -> None:
        value = self.fixture(production=True)
        first = value["validators"][0]
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        self.prime_interlock_interpreters(harness)
        harness.legacy_archive_forks[first["name"]] = {
            "node": first["name"],
            "binding_index_sha256": "3" * 64,
        }
        baseline = "".join(
            f"{service}_{state}=0\n"
            for service in ("validator", "gateway", "filter", "interlock", "archive", "nginx")
            for state in ("active", "enabled")
        ) + "public_80_count=0\npublic_443_count=0\n"
        preflights: list[tuple[str, tuple[str, ...]]] = []

        def fake_ssh(remote_node, script, args=(), *, timeout=180):
            if "# BEGIN ARC PORT OWNERSHIP HELPER" in script:
                preflights.append((script, tuple(args)))
                return ""
            return baseline

        harness.ssh_read_only = mock.Mock(side_effect=fake_ssh)
        harness.ssh = mock.Mock(
            side_effect=AssertionError("production preflight used persistent SSH")
        )

        harness._preflight_production()
        self.assertEqual(harness.ssh_read_only.call_count, 18)
        harness.ssh.assert_not_called()
        self.assertEqual(len(preflights), 6)
        script, args = preflights[0]
        self.assertEqual(
            args[19:23],
            (
                str(rollout.LEGACY_ARCHIVE_FILTER_PORT),
                harness.legacy_archive_rpc_socket(first),
                str(first["p2p_port"]),
                rollout.LEGACY_ARCHIVE_USER,
            ),
        )
        receipt = value["provenance"]["validator_key_receipt_chain"]["validators"][0]
        self.assertEqual(
            args[23:27],
            (
                f"/root/arc-recovery-seal/{value['archive']['prearchive_rollout_sha256']}/{first['name']}/arc-cli",
                value["artifacts"]["cli"]["sha256"],
                receipt["keyfile_sha256"],
                receipt["address"],
            ),
        )
        self.assertIn('keygen --verify-keyfile "$key"', script)
        self.assertIn('stat -c %U:%G:%a:%h "$key"', script)
        self.assertIn("zzzz-arc-recovery-freeze.conf", script)
        self.assertIn("zzzx-arc-recovery-quarantine-arm.conf", script)
        self.assertIn("ConditionPathExists=/etc/arc-recovery/legacy-start-allowed", script)
        self.assertIn("test ! -e /etc/arc-recovery/legacy-start-allowed", script)
        self.assertNotIn("/root/.arc-recovery-legacy-start-allowed", script)
        self.assertEqual(
            args[47:49],
            (value["archive"]["capture_id"], first["name"]),
        )
        for invocation in (
            "assert_listener_owner 18080",
            'assert_unix_listener_owner "$validator_rpc_socket"',
            'assert_listener_owner "$retired_rpc_port"',
            'assert_udp_listener_owner "$p2p_port"',
            'assert_listener_owner 2019 "" caddy-admin-disabled',
            'assert_listener_owner "$archive_filter_port"',
            'assert_unix_listener_owner "$archive_rpc_socket"',
            'assert_listener_owner "$retired_archive_rpc_port"',
        ):
            self.assertIn(invocation, script)

        helper = script.split("# BEGIN ARC PORT OWNERSHIP HELPER", 1)[1].split(
            "# END ARC PORT OWNERSHIP HELPER", 1
        )[0]

        def exercise(rows: str, expected_pid: str):
            shell = f'''set -eu
ss() {{ printf '%s\\n' "$ARC_TEST_SS_ROWS"; }}
{helper}
assert_listener_owner 18080 "$ARC_TEST_EXPECTED_PID" rpc-filter
'''
            environment = dict(os.environ)
            environment.update(
                ARC_TEST_SS_ROWS=rows,
                ARC_TEST_EXPECTED_PID=expected_pid,
            )
            return rollout.subprocess.run(
                ["/bin/sh", "-c", shell],
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )

        owned = 'LISTEN 0 128 127.0.0.1:18080 0.0.0.0:* users:(("nginx",pid=4242,fd=5))'
        foreign = 'LISTEN 0 128 127.0.0.1:18080 0.0.0.0:* users:(("nginx",pid=9999,fd=5))'
        self.assertEqual(exercise(owned, "4242").returncode, 0)
        mismatch = exercise(foreign, "4242")
        self.assertNotEqual(mismatch.returncode, 0)
        self.assertIn("not owned by same-rollout", mismatch.stderr)
        unowned = exercise(foreign, "")
        self.assertNotEqual(unowned.returncode, 0)
        self.assertIn("foreign listener", unowned.stderr)

        def exercise_udp(rows: str, expected_pid: str):
            shell = f'''set -eu
ss() {{
  case " $* " in
    *" -lunp "*) printf '%s\\n' "$ARC_TEST_UDP_ROWS" ;;
    *) printf '\\n' ;;
  esac
}}
{helper}
assert_udp_listener_owner 10001 "$ARC_TEST_EXPECTED_PID" validator-quic-p2p
'''
            environment = dict(os.environ)
            environment.update(
                ARC_TEST_UDP_ROWS=rows,
                ARC_TEST_EXPECTED_PID=expected_pid,
            )
            return rollout.subprocess.run(
                ["/bin/sh", "-c", shell],
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )

        udp_owned = 'UNCONN 0 0 0.0.0.0:10001 0.0.0.0:* users:(("arc-node",pid=4242,fd=9))'
        udp_foreign = 'UNCONN 0 0 0.0.0.0:10001 0.0.0.0:* users:(("foreign",pid=9999,fd=4))'
        self.assertEqual(exercise_udp(udp_owned, "4242").returncode, 0)
        resume_without_process = exercise_udp("", "")
        self.assertEqual(resume_without_process.returncode, 0)
        foreign_udp = exercise_udp(udp_foreign, "")
        self.assertNotEqual(foreign_udp.returncode, 0)
        self.assertIn("foreign listener", foreign_udp.stderr)
        duplicate_udp = exercise_udp(f"{udp_owned}\n{udp_foreign}", "4242")
        self.assertNotEqual(duplicate_udp.returncode, 0)
        self.assertIn("exactly one UDP row", duplicate_udp.stderr)

    def test_frontend_config_can_fully_verify_and_fetch_archive_metadata(self) -> None:
        value = self.fixture(production=True)
        reward_evidence = self.two_receipt_evidence()
        value["archive"].update(
            {
                "complete_sha256": "1" * 64,
                "archive_manifest_sha256": "2" * 64,
                "sha256sums_sha256": "3" * 64,
                "prearchive_rollout_sha256": "4" * 64,
            }
        )
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        events: list[str] = []
        harness.verify_production_archive = mock.Mock(
            side_effect=lambda: events.append("archive-verified") or "2" * 64
        )

        def load_metadata():
            events.append("metadata-loaded")
            harness.archive_metadata_loaded = True
            harness.legacy_archive_forks = {}

        harness.load_production_archive_metadata = mock.Mock(side_effect=load_metadata)
        harness.verify_live = mock.Mock(
            side_effect=lambda evidence: events.append("live-verified")
        )
        output = self.root / "auto-fetched-frontend.json"
        rollout.write_frontend_config(
            harness, output, reward_evidence=reward_evidence
        )
        self.assertEqual(
            events, ["archive-verified", "metadata-loaded", "live-verified"]
        )
        self.assertTrue(output.exists())

        with self.assertRaisesRegex(rollout.RolloutError, "supplied together"):
            rollout.write_frontend_config(
                harness,
                self.root / "one-sided-archive-input.json",
                archive_manifest_path=self.root / "only-manifest.json",
                reward_evidence=reward_evidence,
            )

        bad_root_output = self.root / "bad-archive-root-frontend.json"
        harness.verify_production_archive = mock.Mock(return_value="9" * 64)
        with self.assertRaisesRegex(rollout.RolloutError, "different finalized root"):
            rollout.write_frontend_config(
                harness, bad_root_output, reward_evidence=reward_evidence
            )
        self.assertFalse(bad_root_output.exists())

    def test_archive_metadata_reads_are_binary_and_bounded_before_parsing(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        harness.configure_production_transport = mock.Mock()
        harness._assert_production_rclone_transport = mock.Mock()
        harness.production_rclone_path = Path("/reviewed/rclone")
        harness.production_rclone_config = Path("/secure/rclone.conf")
        harness.production_transport_env = {"PATH": "/usr/bin:/bin"}
        with self.assertRaisesRegex(rollout.RolloutError, "same-process"):
            harness.load_production_archive_metadata()
        oversized = SimpleNamespace(stdout=b"x" * 9)
        with mock.patch.object(rollout, "run_checked_bytes", return_value=oversized) as run:
            with self.assertRaisesRegex(rollout.RolloutError, "8-byte safety limit"):
                harness._rclone_cat_pinned_archive_object(
                    "ARCHIVE-MANIFEST.json", "1" * 64, max_bytes=8
                )
        self.assertEqual(run.call_args.args[0][-2:], ["--count", "9"])

        invalid = self.root / "invalid-utf8.json"
        invalid.write_bytes(b"\xff")
        invalid.chmod(0o444)
        with self.assertRaisesRegex(rollout.RolloutError, "cannot read archive metadata"):
            rollout._read_canonical_read_only_json(invalid, "archive metadata")

    def test_production_execute_reverifies_live_captures_before_first_mutation(self) -> None:
        value = self.fixture(production=True)
        value["archive"].update(
            {
                "complete_sha256": "1" * 64,
                "archive_manifest_sha256": "2" * 64,
                "sha256sums_sha256": "3" * 64,
                "prearchive_rollout_sha256": "4" * 64,
            }
        )
        harness = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            rollback_journal=self.root / "first-mutation-rollback",
        )
        harness.archive_metadata_loaded = True
        empty_baseline = {
            f"{service}_{state}": False
            for service in ("validator", "gateway", "filter", "interlock", "archive", "nginx")
            for state in ("active", "enabled")
        }
        harness.production_service_baseline = {
            node["name"]: dict(empty_baseline) for node in value["validators"]
        }
        def refresh_first_baseline():
            harness.production_service_baseline.update(
                {node["name"]: dict(empty_baseline) for node in value["validators"]}
            )
            harness.production_public_listener_baseline.update(
                {node["name"]: {"80": 0, "443": 0} for node in value["validators"]}
            )

        harness._preflight_production = mock.Mock(side_effect=refresh_first_baseline)
        harness.verify_execution_provenance = mock.Mock()
        harness.verify_production_archive = mock.Mock(return_value="2" * 64)
        def fail_first_mutation(_node):
            self.assertTrue(harness.rollback_journal_reserved)
            self.assertEqual(harness.rollback_journal_state, "forward")
            self.assertTrue(
                (self.root / "first-mutation-rollback" / "HEADER.json").is_file()
            )
            raise rollout.RolloutError("sentinel first mutation")

        harness._stage_production_node = mock.Mock(side_effect=fail_first_mutation)
        harness._rollback_production = mock.Mock()
        with self.assertRaisesRegex(rollout.RolloutError, "sentinel first mutation"):
            harness.execute_production()
        harness.verify_production_archive.assert_called_once_with(
            verify_live_captures=True
        )
        harness._stage_production_node.assert_called_once()
        harness._rollback_production.assert_called_once()

    def test_late_failure_restores_exact_preexecution_service_baseline(self) -> None:
        value = self.fixture(production=True)
        value["archive"].update(
            {
                "complete_sha256": "1" * 64,
                "archive_manifest_sha256": "2" * 64,
                "sha256sums_sha256": "3" * 64,
                "prearchive_rollout_sha256": "4" * 64,
            }
        )
        rollback_root = self.root / "late-failure-rollback"
        harness = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            rollback_journal=rollback_root,
        )
        harness.archive_metadata_loaded = True
        baseline = {
            f"{service}_{state}": False
            for service in ("validator", "gateway", "filter", "interlock", "archive", "nginx")
            for state in ("active", "enabled")
        }
        baseline.update(
            {
                "validator_active": True,
                "validator_enabled": True,
                "gateway_active": True,
                "gateway_enabled": True,
                "filter_active": True,
                "filter_enabled": True,
            }
        )
        harness.production_service_baseline = {
            node["name"]: dict(baseline) for node in value["validators"]
        }
        def refresh_late_baseline():
            harness.production_service_baseline.update(
                {node["name"]: dict(baseline) for node in value["validators"]}
            )
            harness.production_public_listener_baseline.update(
                {node["name"]: {"80": 1, "443": 1} for node in value["validators"]}
            )

        harness._preflight_production = mock.Mock(side_effect=refresh_late_baseline)
        harness.verify_execution_provenance = mock.Mock()
        harness.verify_production_archive = mock.Mock(return_value="2" * 64)
        harness._stage_production_node = mock.Mock()
        def fail_install(_node):
            raise rollout.RolloutError("sentinel late failure")

        # The original-baseline rollback remains valid only before the
        # journaled one-way quarantine-retirement boundary.
        harness._install_late_fork_interlock = mock.Mock(side_effect=fail_install)
        harness._install_gateway_and_unit = mock.Mock()
        def restored_proof(node, expected):
            return {
                "schema": "arc.recovery.production-rollback-host.v1",
                "node": node["name"],
                "states": {field: expected[field] for field in sorted(expected)},
                "public_listener_counts": {"80": 1, "443": 1},
            }

        harness._rollback_production_host = mock.Mock(side_effect=restored_proof)
        with self.assertRaisesRegex(rollout.RolloutError, "sentinel late failure"):
            harness.execute_production()
        self.assertEqual(harness._rollback_production_host.call_count, 6)
        self.assertEqual(
            [call.args[0]["name"] for call in harness._rollback_production_host.call_args_list],
            [node["name"] for node in reversed(value["validators"])],
        )
        receipt_path = rollback_root / "ROLLBACK-RECEIPT.json"
        receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
        self.assertTrue(receipt["complete"])
        self.assertEqual(len(receipt["results"]), 6)
        self.assertTrue(all(row["state"] == "restored-and-proved" for row in receipt["results"]))
        self.assertEqual(stat.S_IMODE(receipt_path.stat().st_mode), 0o400)
        self.assertEqual(
            len(list(rollback_root.glob("ROLLBACK-RUN-0001-ATTEMPT-*-STARTED.json"))),
            6,
        )
        self.assertEqual(
            len(list(rollback_root.glob("ROLLBACK-RUN-0001-ATTEMPT-*-RESULT.json"))),
            6,
        )
        self.assertEqual(receipt["schema"], "arc.recovery.production-rollback-receipt.v2")
        self.assertEqual(receipt["rollback_run"], 1)

    def test_lax_nginx_baseline_is_passed_to_installer_and_exactly_restored(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        self.prime_interlock_interpreters(harness)
        lax = next(node for node in value["validators"] if node["name"] == "lax")
        baseline = {
            f"{service}_{state}": False
            for service in ("validator", "gateway", "filter", "interlock", "archive", "nginx")
            for state in ("active", "enabled")
        }
        baseline.update({"nginx_active": True, "nginx_enabled": True})
        harness.production_service_baseline = {"lax": baseline}
        harness.production_public_listener_baseline = {
            "lax": {"80": 1, "443": 0}
        }
        calls = []

        def fake_ssh(node, script, args=(), **_kwargs):
            args = tuple(args)
            calls.append((node["name"], script, args))
            if 'security_receipt="$root/nginx-security-boundary.json"' in script:
                return rollout.canonical_bytes(
                    {
                        "schema": "arc.recovery.gateway-security-boundary.v1",
                        "rollout_manifest_sha256": args[5],
                        "node": args[25],
                        "package": "nginx",
                        "package_version": rollout.NGINX_PACKAGE_VERSION,
                        "binary_path": "/usr/sbin/nginx",
                        "binary_sha256": rollout.NGINX_LINUX_AMD64_SHA256,
                        "auth_request_module": True,
                        "certificate_storage_nonempty": True,
                        "caddy_restart_tls_probe_status": 404,
                        "filter_config_sha256": args[29],
                        "filter_unit_sha256": args[30],
                        "filter_preflight_sha256": args[31],
                        "filter_user": args[28],
                        "package_held": False,
                        "reward_gate_failure_status": 500,
                        "shard_gate_failure_status": 500,
                        "filter_socket_path": args[33],
                        "archive_filter_socket_path": args[34],
                        "filter_socket_mode": "0770",
                        "attacker_user": args[35],
                        "attacker_socket_denied": True,
                        "attacker_interlock_socket_denied": True,
                        "direct_tcp_filter_absent": True,
                        "direct_tcp_interlock_absent": True,
                        "caddy_identity_healthy_gate_status": 502,
                        "caddy_interlock_socket_denied": True,
                        "effective_systemd_inventory_sha256": args[40],
                        "filter_group_primary_users": [
                            rollout.CADDY_USER,
                            rollout.NGINX_FILTER_USER,
                        ],
                        "filter_group_supplementary_users": [],
                        "interlock_group": rollout.LATE_FORK_INTERLOCK_GROUP,
                        "interlock_group_primary_users": [
                            rollout.LATE_FORK_INTERLOCK_USER
                        ],
                        "interlock_group_supplementary_users": [
                            rollout.NGINX_FILTER_USER
                        ],
                        "interlock_socket_mode": "0660",
                        "interlock_socket_path": args[42],
                        "origin_group": rollout.RPC_ORIGIN_GROUP,
                        "origin_group_primary_users": [],
                        "origin_group_supplementary_users": [
                            rollout.NGINX_FILTER_USER
                        ],
                    }
                ).decode("utf-8")
            if "schema=arc.recovery.production-rollback-host.v1" in script:
                return "\n".join(
                    (
                        "schema=arc.recovery.production-rollback-host.v1",
                        "node=lax",
                        *(
                            f"{field}={'1' if baseline[field] else '0'}"
                            for field in sorted(baseline)
                        ),
                        "public_80_count=1",
                        "public_443_count=0",
                    )
                )
            raise AssertionError("unexpected LAX service-baseline SSH call")

        harness.ssh = mock.Mock(side_effect=fake_ssh)
        harness._install_gateway_and_unit(lax)
        install_call = calls[0]
        self.assertEqual(install_call[0], "lax")
        self.assertEqual(install_call[2][15:17], ("1", "1"))
        self.assertIn("systemctl stop nginx.service", install_call[1])
        self.assertIn("systemctl disable nginx.service", install_call[1])

        proof = harness._rollback_production_host(lax, baseline)
        rollback_call = calls[1]
        self.assertEqual(rollback_call[2][16:18], ("1", "1"))
        self.assertEqual(rollback_call[2][24:26], ("1", "0"))
        self.assertIn("systemctl enable nginx.service", rollback_call[1])
        self.assertIn("systemctl start nginx.service", rollback_call[1])
        self.assertTrue(proof["states"]["nginx_active"])
        self.assertTrue(proof["states"]["nginx_enabled"])
        self.assertEqual(proof["public_listener_counts"], {"80": 1, "443": 0})

    def test_unreachable_rollback_is_durable_aggregate_emergency(self) -> None:
        value = self.fixture(production=True)
        rollback_root = self.root / "unreachable-rollback"
        harness = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            rollback_journal=rollback_root,
        )
        baseline = {
            f"{service}_{state}": False
            for service in ("validator", "gateway", "filter", "interlock", "archive", "nginx")
            for state in ("active", "enabled")
        }
        harness.production_service_baseline = {
            node["name"]: dict(baseline) for node in value["validators"]
        }
        harness.production_public_listener_baseline = {
            node["name"]: {"80": 0, "443": 0} for node in value["validators"]
        }
        harness.reserve_rollback_journal()

        calls = 0
        def partial(node, expected):
            nonlocal calls
            calls += 1
            if calls == 1:
                raise rollout.RolloutError("host unreachable")
            return {
                "schema": "arc.recovery.production-rollback-host.v1",
                "node": node["name"],
                "states": {field: expected[field] for field in sorted(expected)},
                "public_listener_counts": {"80": 0, "443": 0},
            }

        harness._rollback_production_host = mock.Mock(side_effect=partial)
        with self.assertRaisesRegex(
            rollout.RolloutError,
            "EMERGENCY_ROLLBACK_INCOMPLETE.*preserve all data/history",
        ):
            harness._rollback_production(rollout.RolloutError("original rollout failure"))
        self.assertEqual(harness._rollback_production_host.call_count, 6)
        self.assertFalse((rollback_root / "ROLLBACK-RECEIPT.json").exists())
        receipt = json.loads((rollback_root / "ROLLBACK-RUN-0001-FAILED.json").read_text(encoding="utf-8"))
        self.assertFalse(receipt["complete"])
        self.assertEqual(len(receipt["results"]), 6)
        self.assertEqual(sum(row["state"] == "incomplete" for row in receipt["results"]), 1)
        self.assertIn("no deletion", receipt["preservation_policy"].replace("-", " "))

    def test_quarantine_retirement_is_exact_capture_bound_and_canonical(self) -> None:
        value = self.fixture(production=True)
        node = value["validators"][0]
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        unit_names = (
            "arc-self-heal.service", "arc-node.service",
            "arc-node-update.service", "arc-node-update.timer",
        )
        receipt = {
            "schema": "arc.recovery.legacy-network-quarantine-retirement.v1",
            "capture_id": value["archive"]["capture_id"],
            "node": node["name"],
            "freeze_plan_sha256": value["archive"]["freeze_plan_sha256"],
            "rollout_manifest_sha256": harness.digest,
            "archive_manifest_sha256": value["archive"]["archive_manifest_sha256"],
            "legacy_maintenance_boundary_sha256": value["chain"]["legacy_maintenance_boundary_sha256"],
            "legacy_maintenance_evidence_bundle_sha256": value["chain"]["legacy_maintenance_evidence_bundle_sha256"],
            "network_quarantine_receipt_sha256": "1" * 64,
            "network_quarantine_monitor_sha256": "2" * 64,
            "quarantine_restart_arm_sha256": "3" * 64,
            "quarantine_restart_commit_sha256": "4" * 64,
            "intent_sha256": "5" * 64,
            "preexisting_firewall_structural_sha256": "6" * 64,
            "owned_ruleset_stateless_sha256": "7" * 64,
            "pinned_nft_sha256": "8" * 64,
            "legacy_start_barriers_sha256": {
                name: f"{index + 10:064x}" for index, name in enumerate(unit_names)
            },
            "quarantine_arm_barriers_sha256": {
                name: f"{index + 20:064x}" for index, name in enumerate(unit_names)
            },
            "nginx_retirement_barrier_sha256": "9" * 64,
            "phases": [
                {"phase": phase, "receipt_sha256": f"{index + 30:064x}"}
                for index, phase in enumerate((
                    "legacy-public-retired", "fence-service-retired",
                    "fence-dependencies-removed", "owned-table-removed",
                ))
            ],
            "table_absent": True,
            "fence_service_active": False,
            "fence_service_enabled": False,
            "fence_dependencies_removed": True,
            "legacy_start_barrier_active": True,
            "nginx_retired": True,
            "automatic_legacy_restart": False,
            "rollback_policy": "maintenance-only-no-legacy-restart",
            "completed_at": "2026-08-31T12:00:00Z",
        }
        harness.ssh = mock.Mock(
            return_value=rollout.canonical_bytes(receipt).decode("utf-8")
        )
        self.assertEqual(harness._retire_legacy_network_quarantine(node), receipt)
        self.assertEqual(harness.production_quarantine_retired, {node["name"]})
        remote_args = harness.ssh.call_args.args[2]
        self.assertIn("quarantine-retire", harness.ssh.call_args.args[1])
        self.assertEqual(remote_args[2], value["archive"]["capture_id"])
        self.assertEqual(remote_args[5], harness.digest)
        hostile = copy.deepcopy(receipt)
        hostile["automatic_legacy_restart"] = True
        harness.ssh.return_value = rollout.canonical_bytes(hostile).decode("utf-8")
        with self.assertRaisesRegex(rollout.RolloutError, "identity/policy differs"):
            harness._retire_legacy_network_quarantine(node)

    def test_post_retirement_rollback_is_durable_maintenance_not_legacy_restore(self) -> None:
        value = self.fixture(production=True)
        rollback_root = self.root / "retired-maintenance-rollback"
        baseline = {
            f"{service}_{state}": False
            for service in ("validator", "gateway", "filter", "interlock", "archive", "nginx")
            for state in ("active", "enabled")
        }
        harness = rollout.RecoveryRollout(
            value, "d" * 64, output=io.StringIO(), rollback_journal=rollback_root
        )
        harness.production_service_baseline = {
            node["name"]: dict(baseline) for node in value["validators"]
        }
        harness.production_public_listener_baseline = {
            node["name"]: {"80": 0, "443": 0} for node in value["validators"]
        }
        harness.reserve_rollback_journal()
        harness._rollback_journal_event(3, "QUARANTINE-RETIRE", "STARTED")

        def safe_proof(node, intent):
            return {
                "schema": "arc.recovery.production-retired-maintenance-host.v1",
                "node": node["name"],
                "retirement_receipt_sha256": "a" * 64,
                "maintenance_intent_sha256": intent,
                "states": {
                    **{
                        f"{service}_{state}": True
                        for service in ("gateway", "filter", "interlock")
                        for state in ("active", "enabled")
                    },
                    **{
                        f"{service}_{state}": False
                        for service in ("validator", "archive", "nginx")
                        for state in ("active", "enabled")
                    },
                },
                "public_listener_counts": {"80": 1, "443": 1},
                "checks": {
                    "interlock_gate_status": 204,
                    "maintenance_health_status": 503,
                    "legacy_start_barrier_active": True,
                    "quarantine_retired": True,
                },
            }

        harness._rollback_retired_maintenance_host = mock.Mock(side_effect=safe_proof)
        harness._rollback_production(rollout.RolloutError("post-retirement failure"))
        receipt = json.loads(
            (rollback_root / "ROLLBACK-RECEIPT.json").read_text(encoding="utf-8")
        )
        self.assertEqual(
            receipt["schema"], "arc.recovery.production-retired-maintenance-receipt.v1"
        )
        self.assertEqual(receipt["rollback_mode"], "retired-maintenance-safe")
        self.assertTrue(all(
            row["state"] == "retired-maintenance-and-proved"
            for row in receipt["results"]
        ))
        resumed = rollout.RecoveryRollout(
            value, "d" * 64, output=io.StringIO(), rollback_journal=rollback_root
        )
        self.assertEqual(resumed.reserve_rollback_journal(), "rolled-back")

    def test_interrupted_forward_and_rollback_reuse_original_v2_baselines(self) -> None:
        value = self.fixture(production=True)
        rollback_root = self.root / "crash-resume-rollback"
        baseline = {
            f"{service}_{state}": False
            for service in ("validator", "gateway", "filter", "interlock", "archive", "nginx")
            for state in ("active", "enabled")
        }
        original = rollout.RecoveryRollout(
            value, "d" * 64, output=io.StringIO(), rollback_journal=rollback_root
        )
        original.production_service_baseline = {
            node["name"]: {**baseline, "validator_active": index % 2 == 0}
            for index, node in enumerate(value["validators"])
        }
        original.production_public_listener_baseline = {
            node["name"]: {"80": index, "443": index + 1}
            for index, node in enumerate(value["validators"])
        }
        original.reserve_rollback_journal()
        original._rollback_journal_event(1, "STAGE", "STARTED")

        resumed = rollout.RecoveryRollout(
            value, "d" * 64, output=io.StringIO(), rollback_journal=rollback_root
        )
        self.assertEqual(resumed.reserve_rollback_journal(), "resume-rollback")
        self.assertEqual(
            resumed.production_service_baseline,
            original.production_service_baseline,
        )
        self.assertEqual(
            resumed.production_public_listener_baseline,
            original.production_public_listener_baseline,
        )
        resumed.configure_production_transport = mock.Mock()
        calls = 0
        def interrupted_once(node, expected):
            nonlocal calls
            calls += 1
            if calls == 1:
                raise rollout.RolloutError("power loss during rollback")
            return {
                "schema": "arc.recovery.production-rollback-host.v1",
                "node": node["name"],
                "states": {field: expected[field] for field in sorted(expected)},
                "public_listener_counts": resumed.production_public_listener_baseline[node["name"]],
            }
        resumed._rollback_production_host = mock.Mock(side_effect=interrupted_once)
        with self.assertRaisesRegex(rollout.RolloutError, "EMERGENCY_ROLLBACK_INCOMPLETE"):
            resumed._resume_existing_rollback_journal()
        self.assertTrue((rollback_root / "ROLLBACK-RUN-0001-FAILED.json").is_file())
        self.assertFalse((rollback_root / "ROLLBACK-RECEIPT.json").exists())

        second = rollout.RecoveryRollout(
            value, "d" * 64, output=io.StringIO(), rollback_journal=rollback_root
        )
        self.assertEqual(second.reserve_rollback_journal(), "resume-rollback")
        second.configure_production_transport = mock.Mock()
        second._rollback_production_host = mock.Mock(
            side_effect=lambda node, expected: {
                "schema": "arc.recovery.production-rollback-host.v1",
                "node": node["name"],
                "states": {field: expected[field] for field in sorted(expected)},
                "public_listener_counts": second.production_public_listener_baseline[node["name"]],
            }
        )
        with self.assertRaisesRegex(rollout.RolloutError, "forward execution is forbidden"):
            second._resume_existing_rollback_journal()
        terminal = json.loads(
            (rollback_root / "ROLLBACK-RECEIPT.json").read_text(encoding="utf-8")
        )
        self.assertEqual(terminal["rollback_run"], 2)
        self.assertTrue(terminal["complete"])
        self.assertTrue((rollback_root / "ROLLBACK-RUN-0002-STARTED.json").is_file())

    def test_success_terminal_is_strict_and_mutually_exclusive(self) -> None:
        value = self.fixture(production=True)
        rollback_root = self.root / "success-terminal"
        reward_output = self.root / "success-reward-evidence.json"
        baseline = {
            f"{service}_{state}": False
            for service in ("validator", "gateway", "filter", "interlock", "archive", "nginx")
            for state in ("active", "enabled")
        }
        harness = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            rollback_journal=rollback_root,
            reward_evidence_output=reward_output,
        )
        harness.production_service_baseline = {
            node["name"]: dict(baseline) for node in value["validators"]
        }
        harness.production_public_listener_baseline = {
            node["name"]: {"80": 0, "443": 0} for node in value["validators"]
        }
        harness.reserve_rollback_journal()
        evidence = self.two_receipt_evidence()
        harness.reserve_reward_evidence_output()
        self.persist_reward_baseline(harness, evidence[0].worker)
        harness.persist_reward_evidence_progress(evidence[:1])
        harness.persist_reward_evidence_progress(evidence)
        reward_sha256 = harness.persist_reward_evidence(evidence)
        gate_payload = rollout.canonical_bytes(
            {
                "schema": "arc.recovery.public-gate-open-receipt.v1",
                "rollout_manifest_sha256": harness.digest,
            }
        )
        gate_receipt = rollback_root / "PUBLIC-GATE-OPEN-RECEIPT.json"
        gate_receipt.write_bytes(gate_payload)
        gate_receipt.chmod(0o400)
        harness.public_gate_receipt_sha256 = hashlib.sha256(gate_payload).hexdigest()
        retirement_payload = rollout.canonical_bytes(
            {
                "schema": "arc.recovery.legacy-network-quarantine-retirement-fleet.v1",
                "rollout_manifest_sha256": harness.digest,
            }
        )
        retirement_receipt = rollback_root / "QUARANTINE-RETIREMENT-RECEIPT.json"
        retirement_receipt.write_bytes(retirement_payload)
        retirement_receipt.chmod(0o400)
        gateway_payload = rollout.canonical_bytes(
            {
                "schema": "arc.recovery.gateway-security-fleet.v1",
                "rollout_manifest_sha256": harness.digest,
            }
        )
        gateway_receipt = rollback_root / "GATEWAY-SECURITY-RECEIPT.json"
        gateway_receipt.write_bytes(gateway_payload)
        gateway_receipt.chmod(0o400)
        for name, phase in (
            ("PUBLIC-TLS-PREFLIGHT-EVIDENCE.json", "preflight"),
            ("PUBLIC-TLS-POST-ROLLOUT-EVIDENCE.json", "post-rollout"),
        ):
            tls_receipt = rollback_root / name
            tls_receipt.write_bytes(
                rollout.canonical_bytes(
                    {
                        "schema": "arc.recovery.public-tls-fleet-evidence.v1",
                        "rollout_manifest_sha256": harness.digest,
                        "phase": phase,
                    }
                )
            )
            tls_receipt.chmod(0o400)
        harness._write_production_success_receipt()
        resumed = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            rollback_journal=rollback_root,
            reward_evidence_output=reward_output,
        )
        self.assertEqual(resumed.reserve_rollback_journal(), "success")
        success = json.loads(
            (rollback_root / "SUCCESS-RECEIPT.json").read_text(encoding="utf-8")
        )
        self.assertEqual(
            success["schema"], "arc.recovery.production-rollout-success.v3"
        )
        self.assertEqual(success["reward_evidence_sha256"], reward_sha256)
        self.assertEqual(
            success["public_tls_preflight_evidence_sha256"],
            digest(rollback_root / "PUBLIC-TLS-PREFLIGHT-EVIDENCE.json"),
        )
        self.assertEqual(
            success["public_tls_post_rollout_evidence_sha256"],
            digest(rollback_root / "PUBLIC-TLS-POST-ROLLOUT-EVIDENCE.json"),
        )

        reward_payload = reward_output.read_bytes()
        reward_sidecar = reward_output.with_name(reward_output.name + ".sha256")
        sidecar_payload = reward_sidecar.read_bytes()

        reward_output.unlink()
        missing = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            rollback_journal=rollback_root,
            reward_evidence_output=reward_output,
        )
        with self.assertRaisesRegex(
            rollout.RolloutError, "cannot open production reward evidence"
        ):
            missing.reserve_rollback_journal()
        reward_output.write_bytes(reward_payload)
        reward_output.chmod(0o444)

        reward_output.chmod(0o600)
        reward_output.write_bytes(reward_payload + b" ")
        reward_output.chmod(0o444)
        tampered = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            rollback_journal=rollback_root,
            reward_evidence_output=reward_output,
        )
        with self.assertRaisesRegex(
            rollout.RolloutError, "production reward evidence differs"
        ):
            tampered.reserve_rollback_journal()
        reward_output.chmod(0o600)
        reward_output.write_bytes(reward_payload)
        reward_output.chmod(0o444)

        reward_sidecar.chmod(0o600)
        reward_sidecar.write_bytes(b"0" * len(sidecar_payload))
        reward_sidecar.chmod(0o444)
        wrong_sidecar = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            rollback_journal=rollback_root,
            reward_evidence_output=reward_output,
        )
        with self.assertRaisesRegex(
            rollout.RolloutError, "production reward evidence sidecar differs"
        ):
            wrong_sidecar.reserve_rollback_journal()
        reward_sidecar.chmod(0o600)
        reward_sidecar.write_bytes(sidecar_payload)
        reward_sidecar.chmod(0o444)

        exact_again = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            rollback_journal=rollback_root,
            reward_evidence_output=reward_output,
        )
        self.assertEqual(exact_again.reserve_rollback_journal(), "success")

        contradictory = rollback_root / "ROLLBACK-RECEIPT.json"
        contradictory.write_bytes(rollout.canonical_bytes({"complete": True}))
        contradictory.chmod(0o400)
        ambiguous = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            rollback_journal=rollback_root,
            reward_evidence_output=reward_output,
        )
        with self.assertRaisesRegex(rollout.RolloutError, "mutually exclusive"):
            ambiguous.reserve_rollback_journal()

    def test_rollback_proof_rejects_partial_or_foreign_state(self) -> None:
        value = self.fixture(production=True)
        node = value["validators"][0]
        baseline = {
            f"{service}_{state}": False
            for service in ("validator", "gateway", "filter", "interlock", "archive", "nginx")
            for state in ("active", "enabled")
        }
        complete = {
            "schema": "arc.recovery.production-rollback-host.v1",
            "node": node["name"],
            **{field: "0" for field in baseline},
            "public_80_count": "0",
            "public_443_count": "0",
        }
        raw = "".join(f"{key}={value}\n" for key, value in complete.items())
        parsed = rollout.RecoveryRollout._parse_rollback_proof(raw, node, baseline)
        self.assertEqual(parsed["node"], node["name"])
        partial = raw.replace("gateway_enabled=0\n", "")
        with self.assertRaisesRegex(rollout.RolloutError, "omitted"):
            rollout.RecoveryRollout._parse_rollback_proof(partial, node, baseline)
        foreign = raw.replace("validator_active=0", "validator_active=1")
        with self.assertRaisesRegex(rollout.RolloutError, "differs from baseline"):
            rollout.RecoveryRollout._parse_rollback_proof(foreign, node, baseline)

    def test_receipt_mode_frontend_publication_requires_two_bound_receipts(self) -> None:
        value = self.fixture(production=True, reward_receipt=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        harness.verify_live = mock.Mock()
        with self.assertRaisesRegex(rollout.RolloutError, "requires --reward-evidence"):
            rollout.write_frontend_config(harness, self.root / "missing-evidence.json")
        harness.verify_live.assert_not_called()

        first = rollout.ReceiptEvidence.from_value(
            value["checks"]["reward"]["receipts"][0]
        )
        with self.assertRaisesRegex(rollout.RolloutError, "distinct transaction hashes"):
            rollout.write_frontend_config(
                harness,
                self.root / "duplicate-evidence.json",
                reward_evidence=[first, first],
            )
        harness.verify_live.assert_not_called()

        evidence = [
            rollout.ReceiptEvidence.from_value(row)
            for row in value["checks"]["reward"]["receipts"]
        ]
        output = self.root / "receipt-gated-frontend.json"
        rollout.write_frontend_config(harness, output, reward_evidence=evidence)
        harness.verify_live.assert_called_once_with(evidence)
        self.assertTrue(output.exists())

    def test_public_tls_evidence_rejects_wrong_san_untrusted_long_or_expiring_leaf(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        node = value["validators"][0]
        now = 2_000_000_000
        valid = self.public_tls_evidence(harness, node, now_unix=now)
        self.assertEqual(
            rollout.validate_public_tls_evidence(
                valid,
                rollout_sha256=harness.digest,
                node=node["name"],
                host=node["host"],
                phase="preflight",
                now_unix=now,
            ),
            valid,
        )

        hostile_cases = []
        wrong_san = copy.deepcopy(valid)
        wrong_san["san_ip_addresses"] = ["203.0.113.10"]
        hostile_cases.append(("wrong SAN", wrong_san, "identity/trust"))

        private_issuer = copy.deepcopy(valid)
        private_issuer["issuer_organization"] = "ARC Private Test CA"
        hostile_cases.append(("private issuer", private_issuer, "identity/trust"))

        self_signed = copy.deepcopy(valid)
        self_signed["leaf_self_signed"] = True
        hostile_cases.append(("self-signed leaf", self_signed, "identity/trust"))

        untrusted = copy.deepcopy(valid)
        untrusted["public_trust_verified"] = False
        hostile_cases.append(("untrusted chain", untrusted, "identity/trust"))

        late_renewal = copy.deepcopy(valid)
        late_renewal["renewal_window_ratio"] = 1 / 3
        hostile_cases.append(("late renewal window", late_renewal, "identity/trust"))

        excessive_lifetime = copy.deepcopy(valid)
        excessive_lifetime["not_after_unix"] = (
            excessive_lifetime["not_before_unix"]
            + rollout.TLS_MAX_LEAF_LIFETIME_SECONDS
            + 1
        )
        excessive_lifetime["lifetime_seconds"] = (
            excessive_lifetime["not_after_unix"]
            - excessive_lifetime["not_before_unix"]
        )
        excessive_lifetime["remaining_validity_seconds"] = (
            excessive_lifetime["not_after_unix"] - now
        )
        hostile_cases.append(("excessive lifetime", excessive_lifetime, "160-hour"))

        near_expiry = copy.deepcopy(valid)
        near_expiry["not_after_unix"] = (
            now + rollout.TLS_MIN_REMAINING_VALIDITY_SECONDS - 1
        )
        near_expiry["lifetime_seconds"] = (
            near_expiry["not_after_unix"] - near_expiry["not_before_unix"]
        )
        near_expiry["remaining_validity_seconds"] = (
            near_expiry["not_after_unix"] - now
        )
        hostile_cases.append(("near expiry", near_expiry, "too near expiry"))

        for label, hostile, message in hostile_cases:
            with self.subTest(label=label):
                with self.assertRaisesRegex(rollout.RolloutError, message):
                    rollout.validate_public_tls_evidence(
                        hostile,
                        rollout_sha256=harness.digest,
                        node=node["name"],
                        host=node["host"],
                        phase="preflight",
                        now_unix=now,
                    )

    def test_public_tls_remote_proof_uses_public_ca_ip_hostname_and_https_probe(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        self.prime_interlock_interpreters(harness)
        node = value["validators"][0]
        evidence = self.public_tls_evidence(
            harness, node, phase="post-rollout"
        )
        harness.ssh = mock.Mock(
            return_value=rollout.canonical_bytes(evidence).decode("utf-8")
        )
        self.assertEqual(
            harness._prove_public_tls_evidence(node, phase="post-rollout"),
            evidence,
        )
        script = harness.ssh.call_args.args[1]
        arguments = harness.ssh.call_args.args[2]
        syntax = rollout.subprocess.run(
            ["/bin/sh", "-n"],
            input=script,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(syntax.returncode, 0, syntax.stderr)
        for exact in (
            "ssl.create_default_context(purpose=ssl.Purpose.SERVER_AUTH)",
            "context.check_hostname=True",
            "context.verify_mode=ssl.CERT_REQUIRED",
            "context.minimum_version=ssl.TLSVersion.TLSv1_2",
            "context.wrap_socket(connection,server_hostname=host)",
            "ip_sans!=[host]",
            "issuer_org!=[\"Let's Encrypt\"]",
            "lifetime>576000",
            "remaining<172800",
            "GET /__arc_tls_probe__ HTTP/1.1",
            "int(match.group(1))!=404",
            "'renewal_observed':False",
            "arc_semantic_python_revalidate",
        ):
            self.assertIn(exact, script)
        self.assertEqual(arguments[0], node["host"])
        self.assertEqual(arguments[1], node["name"])
        self.assertEqual(arguments[3], "post-rollout")
        self.assertEqual(arguments[5], rollout.CADDY_LINUX_AMD64_SHA256)
        self.assertEqual(arguments[6], rollout.CADDY_VERSION)
        self.assertEqual(arguments[7], str(rollout.TLS_RENEWAL_WINDOW_RATIO))

        source = MODULE_PATH.read_text(encoding="utf-8")
        before_start = source.index('self._rollback_journal_event(5, "QUORUM-START"')
        preflight = source.index('self._prove_public_tls_fleet(phase="preflight")')
        public_open = source.index('self.open_public_gate(promotion_initial, promotion_final)')
        post_rollout = source.index(
            'self._prove_public_tls_fleet(phase="post-rollout")'
        )
        self.assertLess(preflight, before_start)
        self.assertLess(public_open, post_rollout)

    def test_gateway_is_https_only_loopback_limited_and_fail_closed(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        node = value["validators"][0]
        caddy = harness.caddyfile(node)
        nginx = harness.nginx_filter(node)
        runtime = harness.runtime_argv(node, remote=True)
        self.assertEqual(runtime.count("--archive"), 1)
        unit = harness.systemd_unit(node)
        self.assertIn(
            f"reverse_proxy unix/{harness.filter_public_socket(node)}", caddy
        )
        self.assertNotIn("reverse_proxy 127.0.0.1:18080", caddy)
        self.assertIn("https://ferrumvir.github.io", caddy)
        self.assertIn('header Vary "Origin"', caddy)
        self.assertIn('respond "" 204', caddy)
        self.assertIn("header Access-Control-Request-Method GET", caddy)
        self.assertIn("header Access-Control-Request-Method POST", caddy)
        self.assertIn('Strict-Transport-Security "max-age=31536000', caddy)
        self.assertIn(f"issuer acme {rollout.LETS_ENCRYPT_PRODUCTION_DIRECTORY}", caddy)
        self.assertIn("profile shortlived", caddy)
        self.assertIn(
            f"renewal_window_ratio {rollout.TLS_RENEWAL_WINDOW_RATIO}", caddy
        )
        self.assertIn("disable_tlsalpn_challenge", caddy)
        self.assertIn(f"\n{node['host']} {{\n", caddy)
        self.assertNotIn("nip.io", caddy)
        self.assertNotIn("sslip.io", caddy)
        self.assertIn('respond "not found" 404', caddy)
        self.assertIn("max_size 1MB", caddy)
        self.assertIn("path /shards/announce /inference/forward_shard /inference/cleanup_shard", caddy)
        self.assertIn("remote_ip 149.28.32.76 140.82.16.112", caddy)
        self.assertIn("max_size 4MB", caddy)
        self.assertIn("limit_req zone=arc_write_", nginx)
        self.assertIn("limit_req zone=arc_shard_", nginx)
        self.assertIn("client_max_body_size 4m", nginx)
        self.assertIn("inference/(?:forward_shard|cleanup_shard)", nginx)
        self.assertIn(f"listen unix:{harness.filter_public_socket(node)}", nginx)
        self.assertNotIn("listen 127.0.0.1:18080", nginx)
        self.assertNotIn("listen 9090", nginx)
        self.assertIn("set_real_ip_from unix:;", nginx)
        self.assertNotIn("set_real_ip_from 127.0.0.1;", nginx)
        self.assertIn("map $realip_remote_addr $arc_loopback_transport", nginx)
        self.assertEqual(
            nginx.count("if ($arc_loopback_transport = 0) { return 403; }"),
            1,
        )
        self.assertNotIn("allow 127.0.0.1;", nginx)
        self.assertEqual(caddy.count("header_up X-Forwarded-For {remote_host}"), 8)
        self.assertIn("location = /internal/community/reward/approve", nginx)
        self.assertIn("location = /community/submit_work", nginx)
        self.assertIn("tx/submit(?:_signed|_batch)?", nginx)
        self.assertIn("/tx/submit", value["gateway"]["public_post_paths"])
        self.assertIn("/tx/submit_signed", value["gateway"]["public_post_paths"])
        self.assertIn("/tx/submit_batch", value["gateway"]["public_post_paths"])
        self.assertIn(
            "path " + " ".join(value["gateway"]["public_post_paths"]),
            caddy,
        )
        for peer in value["validators"]:
            self.assertIn(f"allow {peer['host']};", nginx)
        self.assertIn(f"proxy_read_timeout {rollout.PUBLIC_INFERENCE_TIMEOUT_SECONDS}s", nginx)
        self.assertIn(f"proxy_read_timeout {rollout.WORKER_SUBMIT_TIMEOUT_SECONDS}s", nginx)
        self.assertIn(f"proxy_read_timeout {rollout.VALIDATOR_APPROVAL_TIMEOUT_SECONDS}s", nginx)
        self.assertNotIn("proxy_read_timeout 3700s", nginx)
        self.assertIn("health|info|network/info", nginx)
        self.assertIn("account/(?:0x)?[0-9a-fA-F]{64}(?:/txs)?", nginx)
        internal = caddy[caddy.index("@validatorApproval"):]
        self.assertNotIn("Access-Control-Allow-Origin", internal)
        self.assertEqual(runtime.count("--community-rpc-url"), 6)
        self.assertEqual(runtime.count("--model"), 1)
        self.assertEqual(runtime.count("--shard-range"), 3)
        self.assertIn("0:6", runtime)
        self.assertIn("12:17", runtime)
        self.assertIn("17:22", runtime)
        self.assertIn(
            "Environment=ARC_PUBLIC_SOCKET=https://149.28.32.76",
            unit,
        )
        self.assertIn(
            f"TimeoutStopSec={rollout.NODE_GRACEFUL_STOP_TIMEOUT_SECONDS}s",
            unit,
        )
        self.assertTrue(all(value.startswith("https://") for index, value in enumerate(runtime) if index and runtime[index - 1] == "--community-rpc-url"))
        for private_path in (
            "/shards/announce",
            "/inference/forward_shard",
            "/inference/cleanup_shard",
        ):
            self.assertNotIn(private_path, rollout.DEFAULT_PUBLIC_POST_PATHS)

    def test_public_gateway_probe_reaches_signed_ingress_and_unsigned_fails_closed(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        node = value["validators"][0]
        allowed_headers = {
            "Access-Control-Allow-Origin": rollout.PUBLIC_BROWSER_ORIGIN,
            "Vary": "Origin",
        }
        seen_flat_submit = []
        seen_batch_submit = []
        seen_oversized_batch = []

        class Response:
            def __init__(self, status: int, headers=None, body: bytes = b"") -> None:
                self.status = status
                self.headers = headers or {}
                self.body = body

            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return False

            def read(self, size: int = -1) -> bytes:
                return self.body[:size] if size >= 0 else self.body

        def urlopen(request, **_kwargs):
            method = request.get_method()
            path = request.full_url.removeprefix(node["rpc_url"])
            origin = request.get_header("Origin")
            if method == "POST" and path == "/tx/submit":
                payload = json.loads(request.data)
                self.assertEqual(request.get_header("Content-type"), "application/json")
                self.assertEqual(payload["tx_type"], "transfer")
                self.assertEqual(payload["fee"], 1)
                self.assertNotIn("signature", payload)
                self.assertNotIn("public_key", payload)
                seen_flat_submit.append(payload)
                raise rollout.urllib.error.HTTPError(
                    request.full_url,
                    400,
                    "Bad Request",
                    allowed_headers,
                    io.BytesIO(
                        b"Signature required. Provide both 'signature' and 'public_key'."
                    ),
                )
            if method == "POST" and path == "/tx/submit_batch":
                payload = json.loads(request.data)
                self.assertEqual(request.get_header("Content-type"), "application/json")
                if len(payload["transactions"]) > rollout.PUBLIC_TX_SUBMIT_BATCH_MAX_ITEMS:
                    self.assertEqual(
                        len(payload["transactions"]),
                        rollout.PUBLIC_TX_SUBMIT_BATCH_MAX_ITEMS + 1,
                    )
                    seen_oversized_batch.append(payload)
                    raise rollout.urllib.error.HTTPError(
                        request.full_url,
                        413,
                        "Payload Too Large",
                        allowed_headers,
                        io.BytesIO(b'{"error":"transaction batch exceeds maximum"}'),
                    )
                self.assertEqual(len(payload["transactions"]), 1)
                self.assertNotIn("signature", payload["transactions"][0])
                self.assertNotIn("public_key", payload["transactions"][0])
                seen_batch_submit.append(payload)
                return Response(
                    200,
                    allowed_headers,
                    json.dumps(
                        {"accepted": 0, "rejected": 1, "tx_hashes": []},
                        separators=(",", ":"),
                    ).encode(),
                )
            if origin != rollout.PUBLIC_BROWSER_ORIGIN or path.startswith("/internal/"):
                raise rollout.urllib.error.HTTPError(
                    request.full_url,
                    404,
                    "Not Found",
                    {},
                    io.BytesIO(b"not found"),
                )
            return Response(204 if method == "OPTIONS" else 200, allowed_headers)

        with mock.patch.object(rollout.urllib.request, "urlopen", side_effect=urlopen):
            harness._prove_public_browser_contract(node)

        self.assertEqual(len(seen_flat_submit), 1)
        self.assertEqual(len(seen_batch_submit), 1)
        self.assertEqual(len(seen_oversized_batch), 1)

        for missing_path, expected_status in (("/tx/submit", 400), ("/tx/submit_batch", 200)):
            def gateway_404(request, **kwargs):
                if request.get_method() == "POST" and request.full_url.endswith(missing_path):
                    raise rollout.urllib.error.HTTPError(
                        request.full_url,
                        404,
                        "Not Found",
                        {},
                        io.BytesIO(b"not found"),
                    )
                return urlopen(request, **kwargs)

            with self.subTest(missing_path=missing_path):
                with mock.patch.object(
                    rollout.urllib.request,
                    "urlopen",
                    side_effect=gateway_404,
                ):
                    with self.assertRaisesRegex(
                        rollout.RolloutError,
                        f"public browser preflight returned HTTP 404, expected {expected_status}",
                    ):
                        harness._prove_public_browser_contract(node)

        def accepts_unsigned_batch(request, **kwargs):
            if request.get_method() == "POST" and request.full_url.endswith("/tx/submit_batch"):
                return Response(
                    200,
                    allowed_headers,
                    json.dumps(
                        {"accepted": 1, "rejected": 0, "tx_hashes": ["0" * 64]},
                        separators=(",", ":"),
                    ).encode(),
                )
            return urlopen(request, **kwargs)

        with mock.patch.object(
            rollout.urllib.request,
            "urlopen",
            side_effect=accepts_unsigned_batch,
        ):
            with self.assertRaisesRegex(
                rollout.RolloutError,
                "batch transaction route did not reject every unsigned transfer",
            ):
                harness._prove_public_browser_contract(node)

        def accepts_oversized_batch(request, **kwargs):
            if request.get_method() == "POST" and request.full_url.endswith("/tx/submit_batch"):
                payload = json.loads(request.data)
                if len(payload["transactions"]) > rollout.PUBLIC_TX_SUBMIT_BATCH_MAX_ITEMS:
                    return Response(
                        200,
                        allowed_headers,
                        json.dumps(
                            {
                                "accepted": 0,
                                "rejected": len(payload["transactions"]),
                                "tx_hashes": [],
                            },
                            separators=(",", ":"),
                        ).encode(),
                    )
            return urlopen(request, **kwargs)

        with mock.patch.object(
            rollout.urllib.request,
            "urlopen",
            side_effect=accepts_oversized_batch,
        ):
            with self.assertRaisesRegex(
                rollout.RolloutError,
                "public browser preflight returned HTTP 200, expected 413",
            ):
                harness._prove_public_browser_contract(node)

    def test_schema_and_readme_exactly_match_the_sealed_public_api(self) -> None:
        schema = json.loads(MODULE_PATH.with_name("recovery-manifest.schema.json").read_text())
        gateway = schema["properties"]["gateway"]["oneOf"][1]["properties"]
        self.assertEqual(tuple(gateway["public_get_paths"]["const"]), rollout.DEFAULT_PUBLIC_GET_PATHS)
        self.assertEqual(tuple(gateway["public_post_paths"]["const"]), rollout.DEFAULT_PUBLIC_POST_PATHS)

        readme = (MODULE_PATH.parents[2] / "README.md").read_text(encoding="utf-8")
        node_rpc = (
            MODULE_PATH.parents[2] / "crates" / "arc-node" / "src" / "rpc.rs"
        ).read_text(encoding="utf-8")

        def documented(begin: str, end: str) -> tuple[str, ...]:
            body = readme.split(begin, 1)[1].split(end, 1)[0]
            return tuple(
                line[1:-1]
                for line in body.splitlines()
                if line.startswith("`") and line.endswith("`")
            )

        self.assertEqual(
            documented("<!-- ARC_PUBLIC_GET_BEGIN -->", "<!-- ARC_PUBLIC_GET_END -->"),
            rollout.DEFAULT_PUBLIC_GET_PATHS,
        )
        self.assertEqual(
            documented(
                "<!-- ARC_PUBLIC_PARAMETERIZED_GET_BEGIN -->",
                "<!-- ARC_PUBLIC_PARAMETERIZED_GET_END -->",
            ),
            rollout.PUBLIC_PARAMETERIZED_GET_PATHS,
        )
        self.assertEqual(
            documented("<!-- ARC_PUBLIC_POST_BEGIN -->", "<!-- ARC_PUBLIC_POST_END -->"),
            rollout.DEFAULT_PUBLIC_POST_PATHS,
        )
        for path in rollout.INTERNAL_VALIDATOR_POST_PATHS + rollout.SOURCE_ONLY_NOT_PUBLIC_PATHS:
            self.assertIn(f"`{path}`", readme)
            self.assertNotIn(path, rollout.DEFAULT_PUBLIC_POST_PATHS)
        self.assertIn("4,000-second", readme)
        self.assertIn("2,700-second", readme)
        self.assertIn("1,500-second", readme)
        self.assertIn("64-item", readme)
        self.assertIn(
            f"const MAX_TX_SUBMIT_BATCH_SIZE: usize = {rollout.PUBLIC_TX_SUBMIT_BATCH_MAX_ITEMS};",
            node_rpc,
        )

    def test_operator_docs_bind_two_exact_rewards_and_real_desktop_name(self) -> None:
        recovery_readme = MODULE_PATH.with_name("README.md").read_text(encoding="utf-8")
        validator_runbook = (MODULE_PATH.parents[2] / "docs" / "VALIDATOR-FLEET-ROLLOUT.md").read_text(
            encoding="utf-8"
        )
        walkthrough = (MODULE_PATH.parents[2] / "docs" / "COMMUNITY-NODE-WALKTHROUGH.md").read_text(
            encoding="utf-8"
        )
        getting_started = (MODULE_PATH.parents[2] / "docs" / "GETTING_STARTED.md").read_text(
            encoding="utf-8"
        )
        node_rpc = (MODULE_PATH.parents[2] / "crates" / "arc-node" / "src" / "rpc.rs").read_text(
            encoding="utf-8"
        )
        for operator_doc in (recovery_readme, validator_runbook):
            self.assertIn("2,500,000,000", operator_doc)
            self.assertIn("5 ARC", operator_doc)
            self.assertIn("--reward-evidence-output", operator_doc)
            self.assertIn(rollout.PROJECTION_COLLECTING_REASON, operator_doc)
            self.assertIn("null", operator_doc)
        self.assertIn("two distinct", recovery_readme.lower())
        self.assertIn("two distinct", validator_runbook.lower())
        self.assertIn("edited 2–3 minute", walkthrough)
        self.assertIn('"max_tokens":1', walkthrough)
        self.assertIn("mined_success", walkthrough)
        self.assertIn("command -v jq", walkthrough)
        self.assertIn("/node/info", walkthrough)
        self.assertIn(".projected_daily_arc==null", walkthrough)
        self.assertIn(".attestations_per_day_observed==null", walkthrough)
        self.assertIn(rollout.PROJECTION_COLLECTING_REASON, walkthrough)
        self.assertNotIn("positive non-null projection", walkthrough)
        self.assertIn(f'"{rollout.PROJECTION_COLLECTING_REASON}"', node_rpc)
        self.assertNotIn("arc-node-desktop", getting_started)
        self.assertIn("arc-desktop", getting_started)

    def test_production_shard_gate_requires_exact_origin_bound_3x_view_on_every_node(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        model_id = "0x" + "a" * 64
        shards = []
        for source in value["validators"]:
            for start, end in source["shard_ranges"]:
                shards.append(
                    {
                        "start_layer": start,
                        "end_layer": end,
                        "total_layers": 32,
                        "model_id": model_id,
                        "execution_profile": rollout.CANONICAL_EXECUTION_PROFILE,
                        "socket_addr": source["rpc_url"],
                    }
                )
        healthy = {
            "total_layers": 32,
            "fully_covered": True,
            "profile_bound": True,
            "execution_profile": rollout.CANONICAL_EXECUTION_PROFILE,
            "model_id": model_id,
            "shards": shards,
        }
        harness._http_json = lambda node, path, timeout=10: copy.deepcopy(healthy)
        self.assertEqual(harness._check_production_shard_topology(), "a" * 64)

        poisoned = copy.deepcopy(healthy)
        poisoned["shards"][0]["socket_addr"] = "http://169.254.169.254"
        harness._http_json = lambda node, path, timeout=10: copy.deepcopy(poisoned)
        with self.assertRaisesRegex(rollout.RolloutError, "sealed exact 3x HTTPS"):
            harness._check_production_shard_topology()

        missing_profile = copy.deepcopy(healthy)
        missing_profile.pop("execution_profile")
        harness._http_json = lambda node, path, timeout=10: copy.deepcopy(missing_profile)
        with self.assertRaisesRegex(rollout.RolloutError, "canonical INT8 execution profile"):
            harness._check_production_shard_topology()

        mixed_profile = copy.deepcopy(healthy)
        mixed_profile["shards"][0]["execution_profile"] = (
            "INT16 integer (per-row, cross-platform deterministic)"
        )
        harness._http_json = lambda node, path, timeout=10: copy.deepcopy(mixed_profile)
        with self.assertRaisesRegex(rollout.RolloutError, "missing or mixed non-canonical"):
            harness._check_production_shard_topology()

    def test_successful_receipt_and_earnings_must_agree_on_all_six(self) -> None:
        value = self.fixture(reward_receipt=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        evidence = harness.obtain_receipt_evidence()
        assert evidence is not None
        domain = "0x" + "d" * 64
        commitment = "0x" + "e" * 64
        calls = []

        def response(node, path, timeout=10):
            calls.append((node["name"], path))
            if path == "/community/reward_policy":
                return {
                    "schema": "arc.community.reward-policy.v1",
                    "tx_type": "0x25",
                    "protocol_active": True,
                    "issuance_ready": True,
                    "readiness_unavailable_reason": None,
                    "active_validator_count": 6,
                    "validator_set_size_required": 6,
                    "validator_approvals_required": 5,
                    "configured_community_rpc_origins": 6,
                    "recovery_epoch": 7,
                    "validator_set_id": 9,
                    "worker_min_stake_base": 0,
                    "stake_zero_eligible": True,
                    "transaction_domain": domain,
                    "validator_set_commitment": commitment,
                    "reward_base": 2_500_000_000,
                    "reward_arc": 2.5,
                    "issuance_policy": {
                        "reward_amount": 2_500_000_000,
                        "epoch_blocks": 216_000,
                        "max_per_block": 1,
                        "max_per_epoch": 40,
                        "max_per_worker_epoch": 8,
                        "max_per_coordinator_epoch": 16,
                    },
                    "issuance_policy_hash": "0x" + "f" * 64,
                    "prospective_budget": {
                        "block_height": 138_000,
                        "epoch": 0,
                        "issued_this_block": 0,
                        "remaining_this_block": 1,
                        "issued_this_epoch": 1,
                        "remaining_this_epoch": 39,
                        "coordinator_issued_this_epoch": 1,
                        "coordinator_remaining_this_epoch": 15,
                        "worker_issued_this_epoch": None,
                        "worker_remaining_this_epoch": None,
                    },
                    "treasury_rewards_remaining": 39,
                    "reward_program": "protocol-capped testnet promotional compute subsidy",
                    "reward_is_customer_demand": False,
                }
            if path.startswith("/community/reward_receipt/"):
                return {
                    "status": "mined_success",
                    "tx_type": "0x25",
                    "tx_hash": evidence.tx_hash,
                    "job_id": evidence.job_id,
                    "worker": evidence.worker,
                    "submitted": True,
                    "included": True,
                    "confirmed": True,
                    "success": True,
                    "recovery_epoch": 7,
                    "validator_set_id": 9,
                    "validator_set_commitment": commitment,
                    "transaction_domain": domain,
                    "validator_approvals": 5,
                    "reward_base": 2_500_000_000,
                    "reward_arc": 2.5,
                    "block_height": 120,
                    "block_hash": "0x" + "f" * 64,
                    "index": 0,
                    "receipt_url": f"/community/reward_receipt/{evidence.tx_hash}",
                }
            if path == "/block/120":
                return {
                    "header": {"height": 120, "tx_count": 1},
                    "tx_hashes": [evidence.tx_hash],
                    "hash": "0x" + "f" * 64,
                }
            if path == "/block/120/txs?offset=0&limit=1":
                return {
                    "block_height": 120,
                    "tx_count": 1,
                    "offset": 0,
                    "limit": 1,
                    "returned": 1,
                    "transactions": [{"index": 0, "hash": evidence.tx_hash}],
                }
            return {
                "address": evidence.worker,
                "archive_mode": True,
                "history_complete_since_recovery": True,
                "history_scope": rollout.ARCHIVE_EARNINGS_SCOPE,
                "history_domain": rollout.EARNINGS_HISTORY_DOMAIN,
                "confirmed_receipt_count": 1,
                "confirmed_gross_earnings_base": 2_500_000_000,
                "confirmed_receipts": [
                    {
                        "tx_type": "0x25",
                        "tx_hash": evidence.tx_hash,
                        "job_id": evidence.job_id,
                        "block_height": 120,
                        "block_hash": "0x" + "f" * 64,
                        "index": 0,
                        "submitted": True,
                        "included": True,
                        "confirmed": True,
                        "success": True,
                        "receipt_url": f"/community/reward_receipt/{evidence.tx_hash}",
                        "reward_base": 2_500_000_000,
                        "reward_arc": 2.5,
                    }
                ],
            }

        harness._http_json = response
        harness.prove_reward_receipt(evidence)
        validator_names = {node["name"] for node in value["validators"]}
        self.assertEqual(
            {
                name
                for name, path in calls
                if path == "/block/120"
            },
            validator_names,
        )
        self.assertEqual(
            {
                name
                for name, path in calls
                if path == "/block/120/txs?offset=0&limit=1"
            },
            validator_names,
        )

        corruptions = (
            (
                "wrong worker",
                lambda path, body: body.update(address="0x" + "7" * 64)
                if path.startswith("/worker/earnings/")
                else None,
                "address differs from the requested worker",
            ),
            (
                "wrong reward-row transaction type",
                lambda path, body: body["confirmed_receipts"][0].update(tx_type="0x01")
                if path.startswith("/worker/earnings/")
                else None,
                "lacks the exact successful 0x25 receipt",
            ),
            (
                "wrong reward-row base amount",
                lambda path, body: body["confirmed_receipts"][0].update(
                    reward_base=2_500_000_001
                )
                if path.startswith("/worker/earnings/")
                else None,
                "lacks the exact successful 0x25 receipt",
            ),
            (
                "wrong reward-row ARC amount",
                lambda path, body: body["confirmed_receipts"][0].update(
                    reward_arc=2.6
                )
                if path.startswith("/worker/earnings/")
                else None,
                "lacks the exact successful 0x25 receipt",
            ),
            (
                "wrong reward-row transaction index",
                lambda path, body: body["confirmed_receipts"][0].update(index=1)
                if path.startswith("/worker/earnings/")
                else None,
                "lacks the exact successful 0x25 receipt",
            ),
            (
                "wrong receipt base amount",
                lambda path, body: body.update(reward_base=2_500_000_001)
                if path.startswith("/community/reward_receipt/")
                else None,
                "reward receipt reward_base",
            ),
            (
                "wrong receipt ARC amount",
                lambda path, body: body.update(reward_arc=2.6)
                if path.startswith("/community/reward_receipt/")
                else None,
                "ARC amount differs from its exact base-unit amount",
            ),
            (
                "wrong canonical block hash",
                lambda path, body: body.update(hash="0x" + "8" * 64)
                if path == "/block/120"
                else None,
                "block hash differs from the canonical block",
            ),
            (
                "wrong canonical block index",
                lambda path, body: body.update(tx_hashes=["0x" + "8" * 64])
                if path == "/block/120"
                else None,
                "does not contain the transaction at receipt.index",
            ),
            (
                "wrong transaction-page index",
                lambda path, body: body["transactions"][0].update(index=1)
                if path == "/block/120/txs?offset=0&limit=1"
                else None,
                "does not bind receipt.index to the transaction",
            ),
        )
        for label, corrupt, message in corruptions:
            with self.subTest(corruption=label):
                def hostile_response(node, path, timeout=10):
                    body = copy.deepcopy(response(node, path, timeout))
                    corrupt(path, body)
                    return body

                harness._http_json = hostile_response
                with (
                    mock.patch.object(
                        rollout.time,
                        "monotonic",
                        side_effect=[0, 0, 11],
                    ),
                    mock.patch.object(rollout.time, "sleep"),
                    self.assertRaisesRegex(rollout.RolloutError, message),
                ):
                    harness.prove_reward_receipt(evidence)

    def test_two_distinct_receipts_gate_exact_gross_and_null_projection_on_all_six(self) -> None:
        value = self.fixture(reward_receipt=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        evidence = [
            harness.obtain_receipt_evidence(1),
            harness.obtain_receipt_evidence(2),
        ]
        assert all(item is not None for item in evidence)
        evidence = [item for item in evidence if item is not None]
        blocks = [(120, "f" * 64, 0), (121, "9" * 64, 0)]
        self.install_reward_baseline(harness, evidence[0].worker)

        def response(node, path, timeout=10):
            self.assertEqual(path, f"/worker/earnings/{evidence[0].worker}")
            return self.reward_earnings(
                evidence[0].worker,
                [
                    self.reward_receipt_row(item, block)
                    for item, block in zip(evidence, blocks)
                ],
            )

        harness._http_json = response
        harness.prove_reward_projection(evidence, blocks)

        duplicate = [evidence[0], evidence[0]]
        with self.assertRaisesRegex(rollout.RolloutError, "distinct transaction hashes"):
            harness.prove_reward_projection(duplicate, blocks)
        with self.assertRaisesRegex(rollout.RolloutError, "two distinct blocks"):
            harness.prove_reward_projection(
                evidence,
                [(120, "f" * 64, 0), (120, "9" * 64, 0)],
            )

    def test_nonempty_all_v3_baseline_is_retained_and_projection_uses_full_history(self) -> None:
        value = self.fixture(reward_receipt=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        evidence = [
            rollout.ReceiptEvidence.from_value(row)
            for row in value["checks"]["reward"]["receipts"]
        ]
        blocks = [(120, "f" * 64, 0), (121, "9" * 64, 0)]
        historical = rollout.ReceiptEvidence(
            "0x" + "6" * 64,
            "0x" + "7" * 64,
            evidence[0].worker,
        )
        historical_row = self.reward_receipt_row(
            historical,
            (20, "8" * 64, 0),
            recovery_epoch=3,
            validator_set_id=2,
            transaction_domain="0x" + "a" * 64,
        )
        self.install_reward_baseline(harness, evidence[0].worker, [historical_row])
        rows = [
            historical_row,
            *[
                self.reward_receipt_row(
                    item,
                    block,
                    recovery_epoch=7,
                    validator_set_id=9,
                    transaction_domain="0x" + "b" * 64,
                )
                for item, block in zip(evidence, blocks)
            ],
        ]
        numeric = self.reward_earnings(
            evidence[0].worker,
            rows,
            observed_window_first_timestamp_ms=1_700_000_000_000,
            observed_window_last_timestamp_ms=1_700_086_400_000,
            attestations_per_day_observed=2.0,
            attestations_per_day_unavailable_reason=None,
            projected_daily_arc=5.0,
            projected_daily_unavailable_reason=None,
        )
        harness._http_json = mock.Mock(return_value=numeric)
        harness.prove_reward_projection(evidence, blocks)
        self.assertEqual(harness._http_json.call_count, 6)

        short_window = self.reward_earnings(evidence[0].worker, rows)
        harness._http_json = mock.Mock(return_value=short_window)
        harness.prove_reward_projection(evidence, blocks)

        changed_history = copy.deepcopy(numeric)
        changed_history["confirmed_receipts"][0]["block_hash"] = "0x" + "0" * 64
        harness._http_json = mock.Mock(return_value=changed_history)
        with self.assertRaisesRegex(rollout.RolloutError, "dropped or changed"):
            harness.prove_reward_projection(evidence, blocks)

        def fleet_disagreement(node, path, timeout=10):
            body = copy.deepcopy(numeric)
            if node == value["validators"][-1]:
                body["confirmed_receipts"][-1]["output_hash"] = "0x" + "c" * 64
            return body

        harness._http_json = fleet_disagreement
        with self.assertRaisesRegex(rollout.RolloutError, "validators disagree"):
            harness.prove_reward_projection(evidence, blocks)

        extra = rollout.ReceiptEvidence(
            "0x" + "0" * 63 + "1",
            "0x" + "0" * 63 + "2",
            evidence[0].worker,
        )
        extra_row = self.reward_receipt_row(extra, (122, "0" * 63 + "3", 0))
        extra_history = self.reward_earnings(evidence[0].worker, [*rows, extra_row])
        harness._http_json = mock.Mock(return_value=extra_history)
        with self.assertRaisesRegex(rollout.RolloutError, "exactly two new"):
            harness.prove_reward_projection(evidence, blocks)

    def test_pre_canary_baseline_requires_six_node_agreement_and_survives_resume(self) -> None:
        value = self.fixture(reward_receipt=True)
        worker = value["checks"]["reward"]["receipts"][0]["worker"]
        historical = rollout.ReceiptEvidence(
            "0x" + "6" * 64,
            "0x" + "7" * 64,
            worker,
        )
        historical_row = self.reward_receipt_row(
            historical,
            (20, "8" * 64, 0),
            recovery_epoch=2,
            validator_set_id=3,
            transaction_domain="0x" + "a" * 64,
        )
        output = self.root / "pre-canary-baseline.json"
        first = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        first._http_json = mock.Mock(
            return_value=self.reward_earnings(worker, [historical_row])
        )
        first.capture_reward_earnings_baselines()
        self.assertEqual(first._http_json.call_count, 6)
        baseline = first.reward_earnings_baselines[worker]
        self.assertEqual(baseline.confirmed_receipt_count, 1)
        self.assertEqual(baseline.confirmed_gross_earnings_base, 2_500_000_000)
        assert first.reward_evidence_reservation is not None
        for fd in first.reward_evidence_reservation:
            os.close(fd)
        first.reward_evidence_reservation = None

        resumed = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        resumed.reserve_reward_evidence_output()
        self.assertEqual(resumed.reward_earnings_baselines, {worker: baseline})
        resumed._http_json = mock.Mock(
            side_effect=AssertionError("resume must not move the pre-canary baseline")
        )
        resumed.capture_reward_earnings_baselines()
        resumed._http_json.assert_not_called()
        resumed._http_json = mock.Mock(
            return_value=self.reward_earnings(worker, [historical_row])
        )
        resumed.reprove_reward_earnings_baselines_before_probe()
        self.assertEqual(resumed._http_json.call_count, 6)
        resumed._http_json = mock.Mock(
            return_value=self.reward_earnings(worker, [])
        )
        with self.assertRaisesRegex(rollout.RolloutError, "changed before probe"):
            resumed.reprove_reward_earnings_baselines_before_probe()

        divergent = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=self.root / "divergent-baseline.json",
        )

        def disagreement(node, path, timeout=10):
            rows = [] if node == value["validators"][-1] else [historical_row]
            return self.reward_earnings(worker, rows)

        divergent._http_json = disagreement
        with self.assertRaisesRegex(rollout.RolloutError, "validators disagree"):
            divergent.capture_reward_earnings_baselines()
        self.assertFalse(divergent.reward_earnings_baselines)

    def test_dynamic_probe_snapshots_every_worker_the_sealed_coordinator_can_select(self) -> None:
        value = self.fixture(reward_receipt=True)
        value["checks"]["reward"].pop("receipts")
        value["checks"]["reward"]["probe_argv"] = ["/pinned/reward-probe"]
        workers = ["0x" + "6" * 64, "0x" + "7" * 64]
        harness = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=self.root / "dynamic-baselines.json",
        )
        include_new_worker = [False]

        def response(node, path, timeout=10):
            if path == "/workers/scoreboard?limit=500":
                return {
                    "eligible_inference_workers": 3 if include_new_worker[0] else 2,
                    "coordinator_model_id": "0x" + "1" * 64,
                    "count_visible": 3,
                    "workers": [
                        {
                            "worker_id": workers[0],
                            "capabilities": ["inference"],
                            "model_id": "0x" + "1" * 64,
                            "execution_profile": rollout.CANONICAL_EXECUTION_PROFILE,
                            "work_completed": 1,
                        },
                        {
                            "worker_id": workers[1],
                            "capabilities": ["inference"],
                            "model_id": "0x" + "1" * 64,
                            "execution_profile": rollout.CANONICAL_EXECUTION_PROFILE,
                            "work_completed": 2,
                        },
                        {
                            "worker_id": "0x" + "8" * 64,
                            "capabilities": ["inference"],
                            "model_id": "0x" + "1" * 64,
                            "work_completed": 0,
                            "execution_profile": (
                                rollout.CANONICAL_EXECUTION_PROFILE
                                if include_new_worker[0]
                                else "noncanonical"
                            ),
                        },
                    ],
                }
            worker = path.removeprefix("/worker/earnings/")
            self.assertIn(worker, workers)
            return self.reward_earnings(worker, [])

        harness._http_json = response
        harness.capture_reward_earnings_baselines()
        self.assertEqual(set(harness.reward_earnings_baselines), set(workers))
        harness.reprove_reward_earnings_baselines_before_probe()
        include_new_worker[0] = True
        with self.assertRaisesRegex(rollout.RolloutError, "newly selectable"):
            harness.reprove_reward_earnings_baselines_before_probe()
        outsider = rollout.ReceiptEvidence(
            "0x" + "9" * 64,
            "0x" + "a" * 64,
            "0x" + "8" * 64,
        )
        with self.assertRaisesRegex(rollout.RolloutError, "was not sealed"):
            harness._select_reward_earnings_baseline([outsider])

    def test_two_canaries_reject_numeric_projection_wrong_reason_or_nonexact_gross(self) -> None:
        value = self.fixture(reward_receipt=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        evidence = [
            rollout.ReceiptEvidence.from_value(row)
            for row in value["checks"]["reward"]["receipts"]
        ]
        blocks = [(120, "f" * 64, 0), (121, "9" * 64, 0)]
        self.install_reward_baseline(harness, evidence[0].worker)

        def valid_earnings():
            return self.reward_earnings(
                evidence[0].worker,
                [
                    self.reward_receipt_row(item, block)
                    for item, block in zip(evidence, blocks)
                ],
            )

        invalid = (
            ("address", "0x" + "7" * 64, "address differs from the requested worker"),
            ("archive_mode", False, "durable archive history"),
            ("history_complete_since_recovery", False, "complete canonical all-v3"),
            ("history_scope", "retained window", "complete canonical all-v3"),
            ("history_domain", "current recovery epoch only", "complete canonical all-v3"),
            ("attestations_per_day_observed", 2.0, "invents a rate or projection"),
            ("projected_daily_arc", 5.0, "invents a rate or projection"),
            (
                "attestations_per_day_unavailable_reason",
                "insufficient observations",
                "canonical collecting-data reason",
            ),
            (
                "projected_daily_unavailable_reason",
                None,
                "canonical collecting-data reason",
            ),
            ("confirmed_gross_earnings_base", 5_000_000_001, "differs from its canonical rows"),
            ("confirmed_gross_earnings_arc", 5.1, "exact base-unit total"),
            ("confirmed_receipt_count", 3, "differs from its canonical rows"),
        )
        for field, replacement, message in invalid:
            with self.subTest(field=field):
                body = valid_earnings()
                body[field] = replacement
                harness._http_json = mock.Mock(return_value=body)
                with self.assertRaisesRegex(rollout.RolloutError, message):
                    harness.prove_reward_projection(evidence, blocks)

        for field, replacement in (
            ("tx_type", "0x01"),
            ("submitted", False),
            ("included", False),
            ("confirmed", False),
            ("success", False),
            ("receipt_url", "/community/reward_receipt/0x" + "8" * 64),
            ("index", 1),
            ("reward_base", 2_500_000_001),
            ("reward_arc", 2.6),
        ):
            with self.subTest(receipt_field=field):
                body = valid_earnings()
                body["confirmed_receipts"][0][field] = replacement
                harness._http_json = mock.Mock(return_value=body)
                with self.assertRaises(rollout.RolloutError):
                    harness.prove_reward_projection(evidence, blocks)

    def test_reward_probes_are_proved_sequentially_before_projection(self) -> None:
        value = self.fixture(reward_receipt=True)
        harness = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=self.root / "sequential-reward-evidence.json",
        )
        first = rollout.ReceiptEvidence.from_value(value["checks"]["reward"]["receipts"][0])
        second = rollout.ReceiptEvidence.from_value(value["checks"]["reward"]["receipts"][1])
        self.persist_reward_baseline(harness, first.worker)
        manager = mock.Mock()
        manager.obtain.side_effect = [first, second]
        manager.prove.side_effect = [(120, "f" * 64, 0), (121, "9" * 64, 0)]
        harness.obtain_receipt_evidence = manager.obtain
        harness.prove_reward_receipt = manager.prove
        harness.prove_reward_projection = manager.project
        harness.capture_reward_earnings_baselines = manager.baseline
        harness.reprove_reward_earnings_baselines_before_probe = manager.reprove

        harness.prove_two_reward_receipts()

        self.assertEqual(
            manager.mock_calls,
            [
                mock.call.baseline(),
                mock.call.reprove(),
                mock.call.obtain(1),
                mock.call.prove(first, 1),
                mock.call.obtain(2),
                mock.call.prove(second, 2),
                mock.call.project(
                    [first, second],
                    [(120, "f" * 64, 0), (121, "9" * 64, 0)],
                ),
            ],
        )

    def test_production_reward_history_is_reproved_after_all_six_restarts(self) -> None:
        value = self.fixture(production=True, reward_receipt=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        evidence = [
            rollout.ReceiptEvidence.from_value(row)
            for row in value["checks"]["reward"]["receipts"]
        ]
        heights = []
        for index in range(6):
            heights.extend(((200 + index,), (201 + index,)))
        harness.wait_convergence = mock.Mock(side_effect=heights)
        harness.production_service = mock.Mock()
        harness.wait_nodes_ready = mock.Mock()
        harness.prove_production_runtime_inventory = mock.Mock()
        harness._rollback_journal_event = mock.Mock()
        harness.verify_live = mock.Mock()

        harness.prove_reward_history_survives_production_restarts(evidence)

        self.assertEqual(harness.production_service.call_count, 6)
        for node, call in zip(value["validators"], harness.production_service.call_args_list):
            self.assertEqual(call.args, (node, "restart"))
        self.assertEqual(harness.prove_production_runtime_inventory.call_count, 6)
        self.assertEqual(harness._rollback_journal_event.call_count, 12)
        harness.verify_live.assert_called_once_with(evidence)

    def test_reward_progress_recovers_chain_accept_before_probe_stdout(self) -> None:
        value = self.fixture(reward_receipt=True)
        value["checks"]["reward"]["probe_argv"] = ["/pinned/reward-probe"]
        output = self.root / "accepted-before-stdout.json"
        first = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        first.reserve_reward_evidence_output()
        observed: list[list[str]] = []

        def accepted_then_crash(argv, **_kwargs):
            observed.append(list(argv))
            raise rollout.RolloutError("simulated client crash after chain acceptance")

        with mock.patch.object(rollout, "run_checked", side_effect=accepted_then_crash):
            with self.assertRaisesRegex(rollout.RolloutError, "chain acceptance"):
                first.obtain_receipt_evidence(1)
        self.assertEqual(first.reward_evidence_progress, [])
        assert first.reward_evidence_reservation is not None
        for fd in first.reward_evidence_reservation:
            os.close(fd)
        first.reward_evidence_reservation = None

        evidence = rollout.ReceiptEvidence.from_value(
            value["checks"]["reward"]["receipts"][0]
        )
        resumed = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        resumed.reserve_reward_evidence_output()

        def rediscovered(argv, **_kwargs):
            observed.append(list(argv))
            return SimpleNamespace(
                stdout=json.dumps(
                    {
                        "tx_hash": evidence.tx_hash,
                        "job_id": evidence.job_id,
                        "worker": evidence.worker,
                    }
                )
            )

        with mock.patch.object(rollout, "run_checked", side_effect=rediscovered):
            self.assertEqual(resumed.obtain_receipt_evidence(1), evidence)
        expected_id = rollout.recovery_probe_id_for_rollout("d" * 64, 1)
        self.assertEqual(
            [argv[argv.index("--recovery-probe-id") + 1] for argv in observed],
            [expected_id, expected_id],
        )

    def test_reward_progress_resumes_after_ordinal_one_proof(self) -> None:
        value = self.fixture(reward_receipt=True)
        output = self.root / "ordinal-one-proof-crash.json"
        receipts = [
            rollout.ReceiptEvidence.from_value(row)
            for row in value["checks"]["reward"]["receipts"]
        ]
        first = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        self.persist_reward_baseline(first, receipts[0].worker)
        first.reprove_reward_earnings_baselines_before_probe = mock.Mock()
        first.obtain_receipt_evidence = mock.Mock(
            side_effect=[receipts[0], rollout.RolloutError("crash after ordinal one proof")]
        )
        first.prove_reward_receipt = mock.Mock(return_value=(120, "f" * 64))
        with self.assertRaisesRegex(rollout.RolloutError, "ordinal one"):
            first.prove_two_reward_receipts()
        self.assertEqual(first.reward_evidence_progress, receipts[:1])
        assert first.reward_evidence_reservation is not None
        for fd in first.reward_evidence_reservation:
            os.close(fd)
        first.reward_evidence_reservation = None

        resumed = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        resumed.reserve_reward_evidence_output()
        resumed.obtain_receipt_evidence = mock.Mock(return_value=receipts[1])
        resumed.prove_reward_receipt = mock.Mock(
            side_effect=[(120, "f" * 64), (121, "9" * 64)]
        )
        resumed.prove_reward_projection = mock.Mock()
        self.assertEqual(resumed.prove_two_reward_receipts(), receipts)
        resumed.obtain_receipt_evidence.assert_called_once_with(2)

    def test_reward_progress_resumes_after_ordinal_two_proof(self) -> None:
        value = self.fixture(reward_receipt=True)
        output = self.root / "ordinal-two-proof-crash.json"
        receipts = [
            rollout.ReceiptEvidence.from_value(row)
            for row in value["checks"]["reward"]["receipts"]
        ]
        first = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        self.persist_reward_baseline(first, receipts[0].worker)
        first.reprove_reward_earnings_baselines_before_probe = mock.Mock()
        first.obtain_receipt_evidence = mock.Mock(side_effect=receipts)
        first.prove_reward_receipt = mock.Mock(
            side_effect=[(120, "f" * 64), (121, "9" * 64)]
        )
        first.prove_reward_projection = mock.Mock(
            side_effect=rollout.RolloutError("crash after ordinal two proof")
        )
        with self.assertRaisesRegex(rollout.RolloutError, "ordinal two"):
            first.prove_two_reward_receipts()
        self.assertEqual(first.reward_evidence_progress, receipts)
        assert first.reward_evidence_reservation is not None
        for fd in first.reward_evidence_reservation:
            os.close(fd)
        first.reward_evidence_reservation = None

        resumed = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        resumed.reserve_reward_evidence_output()
        resumed.obtain_receipt_evidence = mock.Mock(
            side_effect=AssertionError("must not create a third reward")
        )
        resumed.prove_reward_receipt = mock.Mock(
            side_effect=[(120, "f" * 64), (121, "9" * 64)]
        )
        resumed.prove_reward_projection = mock.Mock()
        self.assertEqual(resumed.prove_two_reward_receipts(), receipts)
        resumed.obtain_receipt_evidence.assert_not_called()

    def test_reward_progress_resumes_when_projection_crashes_mid_check(self) -> None:
        value = self.fixture(reward_receipt=True)
        output = self.root / "projection-crash.json"
        receipts = [
            rollout.ReceiptEvidence.from_value(row)
            for row in value["checks"]["reward"]["receipts"]
        ]
        first = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        self.persist_reward_baseline(first, receipts[0].worker)
        first.reprove_reward_earnings_baselines_before_probe = mock.Mock()
        first.obtain_receipt_evidence = mock.Mock(side_effect=receipts)
        first.prove_reward_receipt = mock.Mock(
            side_effect=[(120, "f" * 64), (121, "9" * 64)]
        )
        projection_started: list[bool] = []

        def crash_during_projection(*_args):
            projection_started.append(True)
            raise rollout.RolloutError("crash during projection")

        first.prove_reward_projection = crash_during_projection
        with self.assertRaisesRegex(rollout.RolloutError, "during projection"):
            first.prove_two_reward_receipts()
        self.assertEqual(projection_started, [True])
        assert first.reward_evidence_reservation is not None
        for fd in first.reward_evidence_reservation:
            os.close(fd)
        first.reward_evidence_reservation = None

        resumed = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        resumed.reserve_reward_evidence_output()
        resumed.obtain_receipt_evidence = mock.Mock(
            side_effect=AssertionError("projection resume must not issue rewards")
        )
        resumed.prove_reward_receipt = mock.Mock(
            side_effect=[(120, "f" * 64), (121, "9" * 64)]
        )
        resumed.prove_reward_projection = mock.Mock()
        self.assertEqual(resumed.prove_two_reward_receipts(), receipts)
        resumed.obtain_receipt_evidence.assert_not_called()

    def test_reward_progress_rejects_tamper_and_cross_rollout_reuse(self) -> None:
        value = self.fixture(reward_receipt=True)
        receipt = rollout.ReceiptEvidence.from_value(
            value["checks"]["reward"]["receipts"][0]
        )
        baseline = rollout.RewardEarningsBaseline.from_earnings(
            self.reward_earnings(receipt.worker, []), worker=receipt.worker
        )
        payload = rollout.reward_progress_payload(
            "d" * 64, [receipt], [baseline]
        )
        self.assertEqual(
            rollout.parse_reward_progress_payload(payload, "d" * 64),
            rollout.RewardProgressState((baseline,), (receipt,)),
        )
        with self.assertRaisesRegex(rollout.RolloutError, "different rollout"):
            rollout.parse_reward_progress_payload(payload, "e" * 64)
        body = json.loads(payload)
        body["receipts"][0]["recovery_probe_id"] = "0x" + "f" * 64
        with self.assertRaisesRegex(rollout.RolloutError, "foreign recovery"):
            rollout.parse_reward_progress_payload(
                rollout.canonical_bytes(body), "d" * 64
            )

    def test_reward_evidence_is_create_only_and_rollout_bound(self) -> None:
        value = self.fixture(reward_receipt=True)
        output = self.root / "reward-evidence.json"
        harness = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        evidence = [
            rollout.ReceiptEvidence.from_value(row)
            for row in value["checks"]["reward"]["receipts"]
        ]
        baseline = self.persist_reward_baseline(harness, evidence[0].worker)
        harness.persist_reward_evidence_progress(evidence[:1])
        harness.persist_reward_evidence_progress(evidence)
        evidence_digest = harness.persist_reward_evidence(evidence)
        self.assertEqual(evidence_digest, digest(output))
        self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o444)
        parsed = rollout.parse_evidence_file(output, "d" * 64)
        self.assertEqual(list(parsed.receipts), evidence)
        self.assertEqual(parsed.earnings_baseline, baseline)
        with self.assertRaisesRegex(rollout.RolloutError, "different rollout"):
            rollout.parse_evidence_file(output, "e" * 64)
        self.assertEqual(harness.persist_reward_evidence(evidence), evidence_digest)

        resumed_output = self.root / "reward-evidence-resumed.json"
        first_attempt = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=resumed_output,
        )
        first_attempt.reserve_reward_evidence_output()
        self.assertEqual(resumed_output.stat().st_size, 4096)
        assert first_attempt.reward_evidence_reservation is not None
        for fd in first_attempt.reward_evidence_reservation:
            os.close(fd)
        first_attempt.reward_evidence_reservation = None

        resumed = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=resumed_output,
        )
        resumed.reserve_reward_evidence_output()
        self.persist_reward_baseline(resumed, evidence[0].worker)
        resumed.persist_reward_evidence_progress(evidence[:1])
        resumed.persist_reward_evidence_progress(evidence)
        resumed.persist_reward_evidence(evidence)
        self.assertEqual(
            list(rollout.parse_evidence_file(resumed_output, "d" * 64).receipts),
            evidence,
        )

    def test_reward_evidence_recovers_after_json_publish_without_reissuing(self) -> None:
        value = self.fixture(reward_receipt=True)
        output = self.root / "reward-evidence-first-file-crash.json"
        first = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        evidence = [
            rollout.ReceiptEvidence.from_value(row)
            for row in value["checks"]["reward"]["receipts"]
        ]
        first.reserve_reward_evidence_output()
        self.persist_reward_baseline(first, evidence[0].worker)
        first.persist_reward_evidence_progress(evidence[:1])
        first.persist_reward_evidence_progress(evidence)
        assert first.reward_evidence_reservation is not None
        output_fd, sidecar_fd = first.reward_evidence_reservation
        sidecar = output.with_name(output.name + ".sha256")
        markers = first._reward_evidence_reservation_markers(output, sidecar)
        payload, expected_digest, expected_sidecar = first._reward_evidence_payload(
            evidence
        )
        first._atomic_replace_reward_reservation(
            output, output_fd, markers[0], payload
        )
        os.close(output_fd)
        os.close(sidecar_fd)
        first.reward_evidence_reservation = None

        resumed = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        resumed.reserve_reward_evidence_output()
        self.assertEqual(resumed.existing_reward_evidence, evidence)
        self.assertIsNone(resumed.reward_evidence_reservation)
        self.assertEqual(sidecar.read_bytes(), expected_sidecar)
        self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o444)
        self.assertEqual(stat.S_IMODE(sidecar.stat().st_mode), 0o444)
        self.assertEqual(digest(output), expected_digest)

        manager = mock.Mock()
        manager.obtain.side_effect = AssertionError("must not issue another reward")
        manager.prove.side_effect = [(120, "f" * 64, 0), (121, "9" * 64, 0)]
        resumed.obtain_receipt_evidence = manager.obtain
        resumed.prove_reward_receipt = manager.prove
        resumed.prove_reward_projection = manager.project
        self.assertEqual(resumed.prove_or_resume_two_reward_receipts(), evidence)
        manager.obtain.assert_not_called()
        manager.project.assert_called_once()

    def test_reward_evidence_recovers_after_persist_before_final_gates(self) -> None:
        value = self.fixture(reward_receipt=True)
        output = self.root / "reward-evidence-post-persist-crash.json"
        evidence = [
            rollout.ReceiptEvidence.from_value(row)
            for row in value["checks"]["reward"]["receipts"]
        ]
        first = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        self.persist_reward_baseline(first, evidence[0].worker)
        first.persist_reward_evidence_progress(evidence[:1])
        first.persist_reward_evidence_progress(evidence)
        first.persist_reward_evidence(evidence)
        sidecar = output.with_name(output.name + ".sha256")
        # Model a crash after exact final bytes reached both files but before
        # their read-only chmods completed.
        os.chmod(output, 0o600)
        os.chmod(sidecar, 0o600)

        resumed = rollout.RecoveryRollout(
            value,
            "d" * 64,
            output=io.StringIO(),
            reward_evidence_output=output,
        )
        resumed.reserve_reward_evidence_output()
        self.assertEqual(resumed.existing_reward_evidence, evidence)
        self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o444)
        self.assertEqual(stat.S_IMODE(sidecar.stat().st_mode), 0o444)
        manager = mock.Mock()
        manager.obtain.side_effect = AssertionError("must not issue another reward")
        manager.prove.side_effect = [(120, "f" * 64, 0), (121, "9" * 64, 0)]
        resumed.obtain_receipt_evidence = manager.obtain
        resumed.prove_reward_receipt = manager.prove
        resumed.prove_reward_projection = manager.project
        self.assertEqual(resumed.prove_or_resume_two_reward_receipts(), evidence)
        manager.obtain.assert_not_called()
        manager.project.assert_called_once()


if __name__ == "__main__":
    unittest.main(verbosity=2)
