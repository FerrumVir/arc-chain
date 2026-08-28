#!/usr/bin/env python3
from __future__ import annotations

import copy
import hashlib
import importlib.util
import io
import json
import os
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


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class ManifestFixture:
    def __init__(self, root: Path, *, production: bool = False, reward_receipt: bool = False) -> None:
        self.root = root
        artifact_names = ["binary", "genesis", "checkpoint", "legacy_validator_set"] + (
            ["source_snapshot", "source_wal", "caddy"] if production else []
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
            ip = f"192.0.2.{index + 1}" if production else f"127.0.0.{index + 11}"
            node = {
                "name": f"validator-{index + 1}",
                "address": f"{index + 1:064x}",
                "stake": 5_000_000,
                "key_file": str(key),
                "rpc_listen": "127.0.0.1:9944" if production else f"{ip}:9090",
                "rpc_url": f"https://{ip.replace('.', '-')}.nip.io" if production else f"http://{ip}:9090",
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
                        "public_hostname": f"{ip.replace('.', '-')}.nip.io",
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
                "tx_hash": "0x" + "a" * 64,
                "job_id": "0x" + "b" * 64,
                "worker": "0x" + "c" * 64,
                "expected_reward_base": 2_500_000,
            }
        gateway = {"mode": "none"}
        if production:
            gateway = {
                "mode": "caddy-nginx",
                "acme_email": "ops@example.test",
                "public_get_paths": list(rollout.DEFAULT_PUBLIC_GET_PATHS),
                "public_post_paths": list(rollout.DEFAULT_PUBLIC_POST_PATHS),
            }
        self.value = {
            "schema": rollout.SCHEMA,
            "rollout_id": "recovery-v3-test",
            "mode": "production" if production else "local",
            **(
                {
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
                "legacy_public_max_height": 110,
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

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def fixture(self, **kwargs):
        return ManifestFixture(self.root, **kwargs).value

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

    def test_manifest_rejects_public_listener_and_protected_override(self) -> None:
        value = self.fixture()
        value["validators"][0]["rpc_listen"] = "0.0.0.0:9090"
        with self.assertRaisesRegex(rollout.RolloutError, "bind loopback"):
            rollout.validate_manifest(value)
        value = self.fixture()
        value["validators"][0]["extra_args"] = ["--validator-seed=leak"]
        with self.assertRaisesRegex(rollout.RolloutError, "protected flag"):
            rollout.validate_manifest(value)

    def test_manifest_proves_one_validator_restart_quorum(self) -> None:
        value = self.fixture()
        value["validators"][0]["stake"] = 30_000_000
        with self.assertRaisesRegex(rollout.RolloutError, "restart"):
            rollout.validate_manifest(value)

    def test_production_requires_exact_https_embedded_ip_and_pinned_caddy(self) -> None:
        value = self.fixture(production=True)
        self.assertIs(rollout.validate_manifest(value), value)
        cleartext = copy.deepcopy(value)
        cleartext["validators"][0]["rpc_url"] = "http://192.0.2.1:9090"
        with self.assertRaisesRegex(rollout.RolloutError, "must use HTTPS"):
            rollout.validate_manifest(cleartext)
        wrong_dns = copy.deepcopy(value)
        wrong_dns["validators"][0]["public_hostname"] = "192-0-2-99.nip.io"
        wrong_dns["validators"][0]["rpc_url"] = "https://192-0-2-99.nip.io"
        with self.assertRaisesRegex(rollout.RolloutError, "embed the exact"):
            rollout.validate_manifest(wrong_dns)
        missing_caddy = copy.deepcopy(value)
        missing_caddy["artifacts"].pop("caddy")
        with self.assertRaisesRegex(rollout.RolloutError, "missing: caddy"):
            rollout.validate_manifest(missing_caddy)

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

    def test_production_requires_canonical_model_and_exact_balanced_3x_shards(self) -> None:
        value = self.fixture(production=True)
        self.assertIs(rollout.validate_manifest(value), value)

        wrong_model = copy.deepcopy(value)
        wrong_model["validators"][0]["model_sha256"] = "f" * 64
        with self.assertRaisesRegex(rollout.RolloutError, "canonical v0.8"):
            rollout.validate_manifest(wrong_model)

        unbalanced = copy.deepcopy(value)
        unbalanced["validators"][0]["shard_ranges"] = [[0, 6], [12, 17]]
        with self.assertRaisesRegex(rollout.RolloutError, "exactly 16 layers"):
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
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        harness.verify_execution_provenance()
        changed = copy.deepcopy(value)
        changed["archive"]["remote_helper_sha256"] = "f" * 64
        with self.assertRaisesRegex(rollout.RolloutError, "remote archive helper bytes differ"):
            rollout.RecoveryRollout(changed, "d" * 64).verify_execution_provenance()

        self.assertTrue(rollout.paths_overlap("/root/arc-data", "/root/arc-data/v3"))
        self.assertFalse(rollout.paths_overlap("/root/arc-data", "/var/lib/arc-v3"))
        nested = copy.deepcopy(value)
        nested["validators"][0]["remote_root"] = "/var/lib/arc-v3/release"
        with self.assertRaisesRegex(rollout.RolloutError, "disjoint, non-nested"):
            rollout.validate_manifest(nested)

        ssh_calls: list[tuple[str, tuple[str, ...]]] = []
        harness.ssh = mock.Mock(
            side_effect=lambda node, script, args=(), **kwargs: ssh_calls.append(
                (script, tuple(args))
            )
            or ""
        )
        harness.scp = mock.Mock()
        node = value["validators"][0]
        harness._stage_production_node(node)
        harness._stage_production_node(node)
        harness._install_gateway_and_unit(node)
        harness._install_gateway_and_unit(node)
        scripts = "\n".join(script for script, _ in ssh_calls)
        for required in (
            ".arc-recovery-rollout-owner",
            ".arc-recovery-stage-complete",
            "validate_marker",
            "deployment-files.sha256",
            'test "$(cat "$owner")" = "$digest"',
            'cmp --silent "$root/$unit" "$installed"',
        ):
            self.assertIn(required, scripts)
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

    def test_runtime_uses_six_explicit_origins_and_restart_omits_checkpoint(self) -> None:
        value = self.fixture()
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        argv = harness.runtime_argv(value["validators"][0])
        self.assertEqual(argv.count("--community-rpc-url"), 6)
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
        process.wait.assert_called_once_with(timeout=30)
        handle.close.assert_called_once_with()
        self.assertNotIn(name, harness.logs)

        timed_out = mock.Mock()
        timed_out.pid = 4343
        timed_out.poll.return_value = None
        timed_out.wait.side_effect = rollout.subprocess.TimeoutExpired("arc-node", 30)
        timed_out_handle = mock.Mock()
        harness.processes[name] = timed_out
        harness.logs[name] = timed_out_handle
        with mock.patch.object(rollout.os, "killpg") as killpg:
            with self.assertRaisesRegex(rollout.RolloutError, "refusing SIGKILL"):
                harness.stop_local(node)
        killpg.assert_called_once_with(4343, rollout.signal.SIGTERM)
        timed_out_handle.close.assert_not_called()
        self.assertIs(harness.logs[name], timed_out_handle)

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
            "expected_reward_base": 2_500_000,
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

        blocked = self.root / "frontend.blocked.json"
        harness.verify_live = mock.Mock(side_effect=rollout.RolloutError("height gate pending"))
        with self.assertRaisesRegex(rollout.RolloutError, "height gate pending"):
            rollout.write_frontend_config(harness, blocked)
        self.assertFalse(blocked.exists())
        self.assertFalse(Path(str(blocked) + ".sha256").exists())

        output = self.root / "frontend.lock.json"
        harness.verify_live = mock.Mock()
        digest_value = rollout.write_frontend_config(harness, output)
        harness.verify_live.assert_called_once_with()
        self.assertEqual(digest_value, digest(output))
        self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o444)
        self.assertEqual(stat.S_IMODE(Path(str(output) + ".sha256").stat().st_mode), 0o444)
        with self.assertRaisesRegex(rollout.RolloutError, "refusing replacement"):
            rollout.write_frontend_config(harness, output)
        harness.verify_live.assert_called_once_with()

    def test_gateway_is_https_only_loopback_limited_and_fail_closed(self) -> None:
        value = self.fixture(production=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        node = value["validators"][0]
        caddy = harness.caddyfile(node)
        nginx = harness.nginx_filter(node)
        runtime = harness.runtime_argv(node, remote=True)
        unit = harness.systemd_unit(node)
        self.assertIn("reverse_proxy 127.0.0.1:18080", caddy)
        self.assertIn("https://ferrumvir.github.io", caddy)
        self.assertIn('header Vary "Origin"', caddy)
        self.assertIn('respond "" 204', caddy)
        self.assertIn("header Access-Control-Request-Method GET", caddy)
        self.assertIn("header Access-Control-Request-Method POST", caddy)
        self.assertIn('Strict-Transport-Security "max-age=31536000', caddy)
        self.assertIn('respond "not found" 404', caddy)
        self.assertIn("max_size 1MB", caddy)
        self.assertIn("path /shards/announce /inference/forward_shard /inference/cleanup_shard", caddy)
        self.assertIn("remote_ip 192.0.2.1 192.0.2.2", caddy)
        self.assertIn("max_size 4MB", caddy)
        self.assertIn("limit_req zone=arc_write_", nginx)
        self.assertIn("limit_req zone=arc_shard_", nginx)
        self.assertIn("client_max_body_size 4m", nginx)
        self.assertIn("inference/(?:forward_shard|cleanup_shard)", nginx)
        self.assertIn("listen 127.0.0.1:18080", nginx)
        self.assertNotIn("listen 9090", nginx)
        self.assertIn("location = /internal/community/reward/approve", nginx)
        self.assertIn("location = /community/submit_work", nginx)
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
            "Environment=ARC_PUBLIC_SOCKET=https://192-0-2-1.nip.io",
            unit,
        )
        self.assertTrue(all(value.startswith("https://") for index, value in enumerate(runtime) if index and runtime[index - 1] == "--community-rpc-url"))
        for private_path in (
            "/shards/announce",
            "/inference/forward_shard",
            "/inference/cleanup_shard",
        ):
            self.assertNotIn(private_path, rollout.DEFAULT_PUBLIC_POST_PATHS)

    def test_schema_and_readme_exactly_match_the_sealed_public_api(self) -> None:
        schema = json.loads(MODULE_PATH.with_name("recovery-manifest.schema.json").read_text())
        gateway = schema["properties"]["gateway"]["oneOf"][1]["properties"]
        self.assertEqual(tuple(gateway["public_get_paths"]["const"]), rollout.DEFAULT_PUBLIC_GET_PATHS)
        self.assertEqual(tuple(gateway["public_post_paths"]["const"]), rollout.DEFAULT_PUBLIC_POST_PATHS)

        readme = (MODULE_PATH.parents[2] / "README.md").read_text(encoding="utf-8")

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
                        "socket_addr": source["rpc_url"],
                    }
                )
        healthy = {
            "total_layers": 32,
            "fully_covered": True,
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

    def test_successful_receipt_and_earnings_must_agree_on_all_six(self) -> None:
        value = self.fixture(reward_receipt=True)
        harness = rollout.RecoveryRollout(value, "d" * 64, output=io.StringIO())
        evidence = harness.obtain_receipt_evidence()
        assert evidence is not None
        domain = "0x" + "d" * 64
        commitment = "0x" + "e" * 64

        def response(node, path, timeout=10):
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
                    "reward_base": 2_500_000,
                    "reward_arc": 2.5,
                    "issuance_policy": {
                        "reward_amount": 2_500_000,
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
                    "included": True,
                    "confirmed": True,
                    "success": True,
                    "recovery_epoch": 7,
                    "validator_set_id": 9,
                    "validator_set_commitment": commitment,
                    "transaction_domain": domain,
                    "validator_approvals": 5,
                    "reward_base": 2_500_000,
                    "block_height": 120,
                    "block_hash": "0x" + "f" * 64,
                }
            return {
                "confirmed_receipt_count": 1,
                "confirmed_gross_earnings_base": 2_500_000,
                "confirmed_receipts": [{"tx_hash": evidence.tx_hash, "success": True}],
            }

        harness._http_json = response
        harness.prove_reward_receipt(evidence)


if __name__ == "__main__":
    unittest.main(verbosity=2)
