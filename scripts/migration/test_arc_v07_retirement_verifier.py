#!/usr/bin/env python3
"""Hermetic adversarial tests for arc-v07-retirement-verifier.py."""

from __future__ import annotations

import copy
import hashlib
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any


MODULE_PATH = Path(__file__).with_name("arc-v07-retirement-verifier.py")
SPEC = importlib.util.spec_from_file_location("arc_v07_retirement_verifier", MODULE_PATH)
assert SPEC and SPEC.loader
verifier = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = verifier
SPEC.loader.exec_module(verifier)


def canonical(value: Any) -> bytes:
    return verifier.canonical_bytes(value)


def digest(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def write(path: Path, payload: bytes, mode: int = 0o400) -> str:
    path.write_bytes(payload)
    path.chmod(mode)
    return digest(payload)


def ed25519_key_and_signature(seed: bytes, message: bytes) -> tuple[bytes, bytes]:
    """Tiny deterministic test signer; production code contains verification only."""

    expanded = hashlib.sha512(seed).digest()
    scalar_bytes = bytearray(expanded[:32])
    scalar_bytes[0] &= 248
    scalar_bytes[31] &= 63
    scalar_bytes[31] |= 64
    scalar = int.from_bytes(scalar_bytes, "little")
    public_key = verifier._ed25519_encode(
        verifier._ed25519_scalar_multiply(verifier._ED25519_BASE, scalar)
    )
    nonce = int.from_bytes(
        hashlib.sha512(expanded[32:] + message).digest(), "little"
    ) % verifier._ED25519_ORDER
    encoded_r = verifier._ed25519_encode(
        verifier._ed25519_scalar_multiply(verifier._ED25519_BASE, nonce)
    )
    challenge = int.from_bytes(
        hashlib.sha512(encoded_r + public_key + message).digest(), "little"
    ) % verifier._ED25519_ORDER
    signature = encoded_r + ((nonce + challenge * scalar) % verifier._ED25519_ORDER).to_bytes(
        32, "little"
    )
    return public_key, signature


class FakeRuntime:
    def __init__(self, observed: verifier.ProcessObservation | None) -> None:
        self.observed = observed
        self.replacements: list[verifier.ProcessObservation] = []
        self.active: set[tuple[str, str, int]] = set()

    def observe_process(self, pid: int) -> verifier.ProcessObservation | None:
        if self.observed is not None and self.observed.pid == pid:
            return self.observed
        return None

    def matching_processes(
        self, data_dir: str, executable_sha256: str
    ) -> list[verifier.ProcessObservation]:
        del data_dir, executable_sha256
        return list(self.replacements)

    def active_listener_endpoints(self) -> set[tuple[str, str, int]]:
        return set(self.active)


class Fixture:
    stamp = "2026-09-02T12:00:00Z"
    later = "2026-09-02T12:00:10Z"
    boot_id = "11111111-1111-4111-8111-111111111111"
    legacy_pid = 4242
    legacy_start = 987654
    tag = "v0.8.0"
    commit = "a" * 40
    linux_inspector_sha = "b" * 64
    recovery_manifest_sha = "c" * 64
    full_checkpoint_sha = "d" * 64
    manifest_hash = "1" * 64
    payload_hash = "a" * 64
    network_genesis_hash = "2" * 64
    full_state_root = "3" * 64
    source_block_hash = "4" * 64
    source_state_root = "5" * 64
    transition_block_hash = "6" * 64
    recovery_domain = "7" * 64

    def __init__(self, root: Path, *, mode: str = "term_only") -> None:
        self.root = root
        root.chmod(0o700)
        self.data = root / "legacy-data"
        self.data.mkdir(mode=0o700)
        self.wal = self.data / "state.wal"
        write(self.wal, b"ARC legacy WAL prefix\x00frames", 0o600)
        write(self.data / "dag.wal", b"legacy fork evidence", 0o600)
        self.v08_data = root / "data-v0.8"
        self.intent = root / "retirement-intent.json"
        self.receipt = root / "retirement-receipt.json"

        self.legacy_executable = root / "arc-node-v0.7.11"
        self.legacy_executable_sha = write(
            self.legacy_executable, b"sealed legacy executable", 0o500
        )
        self.supervisor_source = root / "arc-node.service"
        self.supervisor_source_sha = write(
            self.supervisor_source,
            b"ExecStart=/opt/arc/arc-node --stake 0 --min-stake 0\n",
        )
        self.argv = (
            os.fspath(self.legacy_executable),
            "--stake",
            "0",
            "--min-stake",
            "0",
            "--data-dir",
            os.fspath(self.data),
            "--community-mode",
        )
        supervisor = {
            "schema": "arc.migration.legacy-v07-supervisor-binding.v1",
            "kind": "systemd",
            "source_path": os.fspath(self.supervisor_source),
            "source_sha256": self.supervisor_source_sha,
            "executable_path": os.fspath(self.legacy_executable),
            "executable_sha256": self.legacy_executable_sha,
            "argv": list(self.argv),
        }
        self.supervisor = root / "legacy-supervisor-binding.json"
        self.supervisor_sha = write(self.supervisor, canonical(supervisor))

        self.boundary_value = {
            "schema": verifier.BOUNDARY_SCHEMA,
            "source_main_commit": "8" * 40,
            "freeze_plan_sha256": "9" * 64,
            "capture_id": "e" * 64,
            "first_quarantine_started_at": "2026-09-01T11:59:00Z",
            "all_controlled_stopped_at": "2026-09-01T12:00:00Z",
            "official_origin_scope": {
                "global_absence_claimed": False,
                "origins": [
                    {"node": name, "host": host, "origin": f"http://{host}:9090"}
                    for name, host in verifier.PRODUCTION_FLEET
                ],
            },
            "observed_cutoff_height": 137_017,
            "continuity_safety_margin": 128,
            "legacy_public_max_height": verifier.CANONICAL_BOUNDARY_HEIGHT,
            "global_absence_claimed": False,
            "threat_model": {"hostile_root_containment_claimed": False},
        }
        self.boundary = root / verifier.BOUNDARY_ASSET
        self.boundary_sha = write(self.boundary, canonical(self.boundary_value))

        signing_hash = verifier._blake3_derive_key(
            verifier._RECOVERY_APPROVAL_CONTEXT, bytes.fromhex(self.manifest_hash)
        )
        keys_and_signatures = [
            ed25519_key_and_signature(bytes([index + 1]) * 32, signing_hash)
            for index in range(6)
        ]
        self.validators = []
        certificate_validators = []
        signature_by_address: dict[str, dict[str, str]] = {}
        for index, ((name, host), (public_key, _signature)) in enumerate(
            zip(verifier.PRODUCTION_FLEET, keys_and_signatures)
        ):
            address = verifier._blake3_short(public_key).hex()
            stake = 100 + index
            self.validators.append(
                {
                    "name": name,
                    "host": host,
                    "origin": f"http://{host}:9090",
                    "address": address,
                    "stake": stake,
                }
            )
            certificate_validators.append(
                {"address": address, "public_key": public_key.hex(), "stake": stake}
            )
            signature_by_address[address] = {
                "validator": address,
                "public_key": public_key.hex(),
                "signature": keys_and_signatures[index][1].hex(),
            }
        certificate_validators.sort(key=lambda row: row["address"])
        certificate_signatures = [
            signature_by_address[row["address"]] for row in certificate_validators[:5]
        ]
        signed_stake = sum(row["stake"] for row in certificate_validators[:5])
        total_stake = sum(row["stake"] for row in certificate_validators)
        self.identity = {
            "format_version": 1,
            "chain_id": verifier.RECOVERY_CHAIN_ID,
            "manifest_hash": self.manifest_hash,
            "payload_hash": self.payload_hash,
            "network_genesis_hash": self.network_genesis_hash,
            "full_state_root": self.full_state_root,
            "source_height": verifier.CANONICAL_BOUNDARY_HEIGHT,
            "source_consensus_round": 77,
            "created_at_unix_ms": 1_788_000_000_000,
            "source_block_hash": self.source_block_hash,
            "source_state_root": self.source_state_root,
            "transition_height": verifier.REQUIRED_POST_CUTOVER_MIN_HEIGHT,
            "transition_block_hash": self.transition_block_hash,
            "recovery_domain": self.recovery_domain,
            "recovery_epoch": 1,
            "validator_set_id": 1,
            "protocol_version": "3.0.0",
            "validator_count": 6,
            "community_rewards_v1_activation_height": (
                verifier.REQUIRED_POST_CUTOVER_MIN_HEIGHT
            ),
        }
        self.descriptor_value = {
            "schema_version": verifier.CHECKPOINT_DESCRIPTOR_SCHEMA,
            "repository": verifier.REPOSITORY,
            "release_tag": self.tag,
            "release_commit": self.commit,
            "recovery_manifest_sha256": self.recovery_manifest_sha,
            "freeze_plan_sha256": self.boundary_value["freeze_plan_sha256"],
            "capture_id": self.boundary_value["capture_id"],
            "inspector_binary_sha256": self.linux_inspector_sha,
            "checkpoint_file": {
                "filename": "recovery.arcchkpt",
                "size_bytes": 3_000_000_000,
                "sha256": self.full_checkpoint_sha,
            },
            "canonical_inspection": self.identity,
            "checkpoint_certificate": {
                "signing_hash": signing_hash.hex(),
                "validators": certificate_validators,
                "signatures": certificate_signatures,
            },
            "approved_validators": self.validators,
            "verified_quorum": {
                "status": "VERIFIED_QUORUM",
                "required_signatures": 5,
                "verified_signature_count": 5,
                "validator_count": 6,
                "signed_validator_addresses": [
                    row["validator"] for row in certificate_signatures
                ],
                "signed_stake": signed_stake,
                "total_stake": total_stake,
            },
        }
        self.descriptor = root / verifier.CHECKPOINT_DESCRIPTOR_ASSET
        self.descriptor_sha = write(self.descriptor, canonical(self.descriptor_value))

        self.policy_value = {
            "schema_version": verifier.CUTOVER_POLICY_SCHEMA,
            "repository": verifier.REPOSITORY,
            "release_tag": self.tag,
            "release_commit": self.commit,
            "recovery_manifest_sha256": self.recovery_manifest_sha,
            "legacy_maintenance_boundary_sha256": self.boundary_sha,
            "recovery_checkpoint_descriptor_sha256": self.descriptor_sha,
            "recovery_checkpoint_file_sha256": self.full_checkpoint_sha,
            "freeze_plan_sha256": self.boundary_value["freeze_plan_sha256"],
            "capture_id": self.boundary_value["capture_id"],
            "first_quarantine_started_at": self.boundary_value["first_quarantine_started_at"],
            "all_controlled_stopped_at": self.boundary_value["all_controlled_stopped_at"],
            "legacy_admission_cutoff_utc": self.boundary_value["all_controlled_stopped_at"],
            "canonical_boundary_height": verifier.CANONICAL_BOUNDARY_HEIGHT,
            "required_post_cutover_min_height": verifier.REQUIRED_POST_CUTOVER_MIN_HEIGHT,
            "required_recovery_epoch": 1,
            "required_validator_set_id": 1,
            "required_validator_count": 6,
            "checkpoint_format_version": 1,
            "chain_id": verifier.RECOVERY_CHAIN_ID,
            "protocol_version": "3.0.0",
            "payload_hash": self.payload_hash,
            "community_rewards_v1_activation_height": (
                verifier.REQUIRED_POST_CUTOVER_MIN_HEIGHT
            ),
            "network_genesis_hash": self.network_genesis_hash,
            "source_block_hash": self.source_block_hash,
            "source_state_root": self.source_state_root,
            "transition_block_hash": self.transition_block_hash,
            "full_state_root": self.full_state_root,
            "recovery_domain": self.recovery_domain,
            "checkpoint_manifest_hash": self.manifest_hash,
            "checkpoint_source_consensus_round": self.identity["source_consensus_round"],
            "checkpoint_created_at_unix_ms": self.identity["created_at_unix_ms"],
            "checkpoint_quorum": copy.deepcopy(
                self.descriptor_value["verified_quorum"]
            ),
            "legacy_validators": self.validators,
            "legacy_worker_rpc": {
                "claim_path": "/community/claim_work",
                "submit_path": "/community/submit_work",
                "listener_ports": [9090, 3001],
            },
            "uncompleted_job_disposition": verifier.JOBS_DISPOSITION,
            "legacy_exit_clean_claimed": False,
            "legacy_restart_allowed": False,
            "global_legacy_absence_claimed": False,
            "offline_retirement_receipt_required": True,
            "v08_start_requires_offline_receipt": True,
        }
        self.policy = root / verifier.CUTOVER_POLICY_ASSET
        self.policy_sha = write(self.policy, canonical(self.policy_value))

        self.release_value = {
            "schema": verifier.INSTALLER_BINDING_SCHEMA,
            "repository": verifier.REPOSITORY,
            "tag": self.tag,
            "commit": self.commit,
            "signed_manifest_sha256": "f" * 64,
            "manifest_signature_sha256": "0" * 64,
            "files": {
                "arc-node-linux-x86_64": self.linux_inspector_sha,
                verifier.BOUNDARY_ASSET: self.boundary_sha,
                verifier.CHECKPOINT_DESCRIPTOR_ASSET: self.descriptor_sha,
                verifier.CUTOVER_POLICY_ASSET: self.policy_sha,
            },
        }
        self.release = root / "arc-release-installer-binding.json"
        self.release_sha = write(self.release, canonical(self.release_value))

        executable_record = {
            "path": os.fspath(self.legacy_executable),
            "sha256": self.legacy_executable_sha,
        }
        listener = {"family": "tcp4", "address_hex": "00000000", "port": 9090, "inode": 333}
        observed = verifier.ProcessObservation(
            pid=self.legacy_pid,
            boot_id=self.boot_id,
            start_ticks=self.legacy_start,
            uid=os.geteuid(),
            gid=os.getegid(),
            executable=executable_record,
            argv=self.argv,
            cwd=os.fspath(root),
            listeners=(listener,),
        )
        self.mode = mode
        self.runtime = FakeRuntime(observed if mode == "term_only" else None)

    def request(self) -> verifier.PrepareRequest:
        return verifier.PrepareRequest(
            intent_output=self.intent,
            target_release=self.release,
            target_release_sha256=self.release_sha,
            maintenance_boundary=self.boundary,
            maintenance_boundary_sha256=self.boundary_sha,
            cutover_policy=self.policy,
            cutover_policy_sha256=self.policy_sha,
            checkpoint=self.descriptor,
            checkpoint_sha256=self.descriptor_sha,
            inspector_binary=None,
            inspector_asset=None,
            inspector_sha256=None,
            retirement_mode=self.mode,
            legacy_pid=self.legacy_pid if self.mode == "term_only" else None,
            legacy_version="0.7.11",
            legacy_executable=self.legacy_executable,
            legacy_executable_sha256=self.legacy_executable_sha,
            supervisor_definition=self.supervisor,
            supervisor_definition_sha256=self.supervisor_sha,
            data_dir=self.data,
            v08_data_dir=self.v08_data,
            replay_mode="forensic-only",
        )

    def prepare(self) -> tuple[dict[str, Any], str]:
        return verifier.prepare_intent(
            self.request(),
            runtime=self.runtime,
            now=lambda: self.stamp,
            offline_stability_seconds=0,
            offline_samples=3,
            sleep=lambda _seconds: None,
        )

    def stop_evidence(self, intent_sha: str) -> tuple[Path, str]:
        offline = self.mode == "preexisting_offline"
        value = {
            "schema": (
                verifier.PREEXISTING_OFFLINE_EVIDENCE_SCHEMA
                if offline
                else verifier.STOP_EVIDENCE_SCHEMA
            ),
            "intent_sha256": intent_sha,
            "process_identity": (
                None
                if offline
                else {
                    "boot_id": self.boot_id,
                    "pid": self.legacy_pid,
                    "start_ticks": self.legacy_start,
                }
            ),
            "supervisor": {
                "mechanism": (
                    "preexisting-offline-verified-supervisor"
                    if offline
                    else "systemd-send-sigkill-no"
                ),
                "signals_sent": [] if offline else ["SIGTERM"],
                "send_sigkill_configured": False,
                "sigkill_sent": False,
                "escalation_used": False,
                "exit_status_observed": False,
            },
            "observation_started_at": self.stamp,
            "offline_observed_at": self.later,
            "legacy_exit_clean_claimed": False,
        }
        path = self.root / "offline-evidence.json"
        return path, write(path, canonical(value))

    def finalize(self) -> tuple[dict[str, Any], str]:
        _intent, intent_sha = self.prepare()
        self.runtime.observed = None
        evidence, evidence_sha = self.stop_evidence(intent_sha)
        return verifier.finalize(
            intent_path=self.intent,
            expected_intent_sha256=intent_sha,
            stop_evidence_path=evidence,
            expected_stop_evidence_sha256=evidence_sha,
            receipt_output=self.receipt,
            runtime=self.runtime,
            stability_seconds=0,
            samples=3,
            sleep=lambda _seconds: None,
            now=lambda: self.later,
        )


class RetirementTests(unittest.TestCase):
    def fixture(self, *, mode: str = "term_only") -> tuple[tempfile.TemporaryDirectory[str], Fixture]:
        temporary = tempfile.TemporaryDirectory()
        return temporary, Fixture(Path(temporary.name), mode=mode)

    def test_independent_crypto_primitives_match_public_vectors(self) -> None:
        self.assertEqual(
            verifier._blake3_short(b"").hex(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
        )
        self.assertEqual(
            verifier._blake3_short(b"abc").hex(),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85",
        )
        # RFC 8032, section 7.1, test vector 1 (empty message).
        public_key = bytes.fromhex(
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        )
        signature = bytes.fromhex(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155"
            "5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        )
        self.assertTrue(verifier._ed25519_verify(public_key, b"", signature))
        self.assertFalse(verifier._ed25519_verify(public_key, b"tampered", signature))

    def test_checkpoint_certificate_fails_closed_on_every_cryptographic_binding(self) -> None:
        mutations = (
            lambda value: value["checkpoint_certificate"].__setitem__(
                "signing_hash", "0" * 64
            ),
            lambda value: value["checkpoint_certificate"]["validators"][0].__setitem__(
                "public_key", "0" * 64
            ),
            lambda value: value["checkpoint_certificate"]["signatures"][0].__setitem__(
                "signature",
                "0"
                + value["checkpoint_certificate"]["signatures"][0]["signature"][1:],
            ),
            lambda value: value["checkpoint_certificate"]["signatures"].__setitem__(
                4, copy.deepcopy(value["checkpoint_certificate"]["signatures"][3])
            ),
            lambda value: value["verified_quorum"].__setitem__(
                "signed_stake", value["verified_quorum"]["signed_stake"] + 1
            ),
        )
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                temporary, fixture = self.fixture()
                with temporary:
                    candidate = copy.deepcopy(fixture.descriptor_value)
                    mutate(candidate)
                    release_binding = {
                        "tag": fixture.tag,
                        "commit": fixture.commit,
                        "files": {"arc-node-linux-x86_64": fixture.linux_inspector_sha},
                    }
                    with self.assertRaises(verifier.RetirementError):
                        verifier.validate_checkpoint_descriptor(
                            candidate,
                            release_binding=release_binding,
                            boundary=verifier.validate_boundary(fixture.boundary_value),
                        )

    def test_boundary_hashes_and_canonical_public_maximum_are_cross_bound(self) -> None:
        temporary, fixture = self.fixture()
        with temporary:
            boundary = verifier.validate_boundary(fixture.boundary_value)
            self.assertEqual(boundary["legacy_public_max_height"], 137_145)
            self.assertEqual(boundary["observed_cutoff_height"], 137_017)
            release_binding = {
                "tag": fixture.tag,
                "commit": fixture.commit,
                "files": {"arc-node-linux-x86_64": fixture.linux_inspector_sha},
            }
            verifier.validate_checkpoint_descriptor(
                fixture.descriptor_value,
                release_binding=release_binding,
                boundary=boundary,
            )
            for field in ("freeze_plan_sha256", "capture_id"):
                candidate = copy.deepcopy(fixture.descriptor_value)
                candidate[field] = "0" * 64
                with self.subTest(field=field), self.assertRaises(verifier.RetirementError):
                    verifier.validate_checkpoint_descriptor(
                        candidate,
                        release_binding=release_binding,
                        boundary=boundary,
                    )

    def test_forensic_retirement_is_honest_and_does_not_mutate_old_tree(self) -> None:
        temporary, fixture = self.fixture()
        with temporary:
            before = verifier.tree_snapshot(fixture.data)
            receipt, receipt_sha = fixture.finalize()
            after = verifier.tree_snapshot(fixture.data)
            self.assertEqual(before, after)
            self.assertEqual(receipt_sha, digest(fixture.receipt.read_bytes()))
            self.assertFalse(receipt["local_legacy_replay"]["performed"])
            self.assertEqual(
                receipt["retirement_result"]["legacy_data_disposition"],
                "preserved_noncanonical_forensic_not_migrated",
            )
            self.assertEqual(
                receipt["retirement_result"]["canonical_history_source"],
                "signed_recovery_checkpoint",
            )
            self.assertFalse(receipt["retirement_result"]["legacy_exit_clean_claimed"])
            self.assertFalse(receipt["retirement_result"]["old_wal_copied_to_v08"])
            self.assertFalse(fixture.v08_data.exists())

    def test_preexisting_offline_never_requires_a_restart_or_signal(self) -> None:
        temporary, fixture = self.fixture(mode="preexisting_offline")
        with temporary:
            receipt, _sha = fixture.finalize()
            self.assertEqual(receipt["old_process"]["retirement_mode"], "preexisting_offline")
            self.assertEqual(receipt["old_process"]["signals_sent"], [])
            self.assertIsNone(receipt["old_process"]["pid"])

    def test_running_broken_node_without_listener_can_still_be_retired(self) -> None:
        temporary, fixture = self.fixture()
        with temporary:
            observed = fixture.runtime.observed
            assert observed is not None
            fixture.runtime.observed = verifier.ProcessObservation(
                **{**observed.__dict__, "listeners": ()}
            )
            intent, _intent_sha = fixture.prepare()
            self.assertEqual(intent["old_process"]["listeners"], [])
            self.assertEqual(
                intent["old_process"]["required_absent_listener_ports"], [9090, 3001]
            )
            fixture.runtime.observed = None
            receipt, _receipt_sha = fixture.finalize()
            self.assertTrue(receipt["retirement_result"]["legacy_listeners_stably_absent"])

    def test_optional_canonical_replay_is_exact_hash_pinned_and_never_migrated(self) -> None:
        temporary, fixture = self.fixture()
        with temporary:
            local_inspector = fixture.root / "arc-node-linux-test"
            local_inspector_sha = write(local_inspector, b"local exact inspector", 0o500)
            snapshot = fixture.root / "state.snapshot.lz4"
            genesis = fixture.root / "genesis.toml"
            legacy_set = fixture.root / "legacy-validator-set.json"
            snapshot_sha = write(snapshot, b"snapshot")
            genesis_sha = write(genesis, b"genesis")
            legacy_set_sha = write(legacy_set, b"validators")
            asset = "arc-node-linux-test"
            fixture.release_value["files"][asset] = local_inspector_sha
            fixture.release.chmod(0o600)
            fixture.release_sha = write(fixture.release, canonical(fixture.release_value))
            base = fixture.request()
            request = verifier.PrepareRequest(
                **{
                    **base.__dict__,
                    "target_release_sha256": fixture.release_sha,
                    "inspector_binary": local_inspector,
                    "inspector_asset": asset,
                    "inspector_sha256": local_inspector_sha,
                    "replay_mode": "canonical-replay",
                    "snapshot": snapshot,
                    "snapshot_sha256": snapshot_sha,
                    "genesis": genesis,
                    "genesis_sha256": genesis_sha,
                    "legacy_validator_set": legacy_set,
                    "legacy_validator_set_sha256": legacy_set_sha,
                    "allow_unbound_legacy_wal": True,
                }
            )
            _intent, intent_sha = verifier.prepare_intent(
                request, runtime=fixture.runtime, now=lambda: fixture.stamp
            )
            fixture.runtime.observed = None
            evidence, evidence_sha = fixture.stop_evidence(intent_sha)

            def runner(
                binary: Path, expected_sha: str, argv: list[str], work_parent: Path
            ) -> tuple[dict[str, Any], str]:
                del work_parent
                self.assertEqual(binary, local_inspector)
                self.assertEqual(expected_sha, local_inspector_sha)
                self.assertEqual(argv[:2], ["recovery", "inspect-legacy-block"])
                tree = verifier.tree_snapshot(fixture.data)
                wal = next(row for row in tree["entries"] if row["path"] == "state.wal")
                input_roots: dict[str, Any] = {
                    "data_dir": {
                        key: tree["root"][key]
                        for key in ("device", "inode", "mode", "uid", "gid", "nlink", "mtime_ns", "ctime_ns")
                    },
                    "state_wal": {"sha256": wal["sha256"]},
                }
                for name, path in (
                    ("snapshot", snapshot),
                    ("genesis", genesis),
                    ("legacy_validator_set", legacy_set),
                ):
                    _raw, record = verifier.stable_file(path, name)
                    input_roots[name] = {"sha256": record["sha256"]}
                result = {
                    "schema": verifier.LEGACY_BLOCK_INSPECTION_SCHEMA,
                    "height": verifier.CANONICAL_BOUNDARY_HEIGHT,
                    "block_hash": fixture.source_block_hash,
                    "state_root": fixture.source_state_root,
                    "input_roots": input_roots,
                }
                return result, digest(canonical(result))

            receipt, _receipt_sha = verifier.finalize(
                intent_path=fixture.intent,
                expected_intent_sha256=intent_sha,
                stop_evidence_path=evidence,
                expected_stop_evidence_sha256=evidence_sha,
                receipt_output=fixture.receipt,
                runtime=fixture.runtime,
                runner=runner,
                stability_seconds=0,
                samples=3,
                sleep=lambda _: None,
                now=lambda: fixture.later,
            )
            self.assertTrue(receipt["local_legacy_replay"]["performed"])
            self.assertEqual(
                receipt["retirement_result"]["legacy_data_disposition"],
                "preserved_local_canonical_boundary_verified_not_migrated",
            )
            self.assertFalse(receipt["retirement_result"]["old_wal_copied_to_v08"])

    def test_intent_and_receipt_are_idempotent(self) -> None:
        temporary, fixture = self.fixture()
        with temporary:
            first, first_sha = fixture.prepare()
            second, second_sha = fixture.prepare()
            self.assertEqual((first, first_sha), (second, second_sha))
            fixture.runtime.observed = None
            evidence, evidence_sha = fixture.stop_evidence(first_sha)
            first_receipt, first_receipt_sha = verifier.finalize(
                intent_path=fixture.intent,
                expected_intent_sha256=first_sha,
                stop_evidence_path=evidence,
                expected_stop_evidence_sha256=evidence_sha,
                receipt_output=fixture.receipt,
                runtime=fixture.runtime,
                stability_seconds=0,
                samples=3,
                sleep=lambda _: None,
                now=lambda: fixture.later,
            )
            fixture.v08_data.mkdir(mode=0o700)
            second_receipt, second_receipt_sha = verifier.finalize(
                intent_path=fixture.intent,
                expected_intent_sha256=first_sha,
                stop_evidence_path=evidence,
                expected_stop_evidence_sha256=evidence_sha,
                receipt_output=fixture.receipt,
                runtime=fixture.runtime,
                stability_seconds=0,
                samples=3,
                sleep=lambda _: None,
            )
            self.assertEqual((first_receipt, first_receipt_sha), (second_receipt, second_receipt_sha))

    def test_existing_receipt_must_match_every_intent_binding(self) -> None:
        temporary, fixture = self.fixture()
        with temporary:
            _receipt, _receipt_sha = fixture.finalize()
            forged = json.loads(fixture.receipt.read_bytes())
            forged["target_release"]["commit"] = "0" * 40
            fixture.receipt.chmod(0o600)
            write(fixture.receipt, canonical(forged))
            intent_sha = digest(fixture.intent.read_bytes())
            evidence = fixture.root / "offline-evidence.json"
            evidence_sha = digest(evidence.read_bytes())
            with self.assertRaisesRegex(verifier.RetirementError, "release binding"):
                verifier.finalize(
                    intent_path=fixture.intent,
                    expected_intent_sha256=intent_sha,
                    stop_evidence_path=evidence,
                    expected_stop_evidence_sha256=evidence_sha,
                    receipt_output=fixture.receipt,
                    runtime=fixture.runtime,
                    stability_seconds=0,
                    samples=3,
                    sleep=lambda _: None,
                )

    def test_atomic_publication_survives_crash_before_and_after_link(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        with temporary:
            root = Path(temporary.name)
            root.chmod(0o700)
            before = root / "before.json"
            after = root / "after.json"
            value = {"schema": "test", "value": 1}

            def crash_before(point: str) -> None:
                if point == "after_file_fsync":
                    raise RuntimeError("crash")

            with self.assertRaisesRegex(RuntimeError, "crash"):
                verifier.publish_create_only_atomic(before, value, "test", fault=crash_before)
            self.assertFalse(before.exists())
            verifier.publish_create_only_atomic(before, value, "test")

            def crash_after(point: str) -> None:
                if point == "after_link":
                    raise RuntimeError("crash")

            with self.assertRaisesRegex(RuntimeError, "crash"):
                verifier.publish_create_only_atomic(after, value, "test", fault=crash_after)
            self.assertEqual(after.read_bytes(), canonical(value))
            verifier.publish_create_only_atomic(after, value, "test")

    def test_existing_create_only_output_cannot_be_replaced(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        with temporary:
            root = Path(temporary.name)
            root.chmod(0o700)
            output = root / "sealed.json"
            verifier.publish_create_only_atomic(output, {"x": 1}, "test")
            with self.assertRaisesRegex(verifier.RetirementError, "differs"):
                verifier.publish_create_only_atomic(output, {"x": 2}, "test")

    def test_running_exact_pid_blocks_finalize(self) -> None:
        temporary, fixture = self.fixture()
        with temporary:
            _intent, intent_sha = fixture.prepare()
            evidence, evidence_sha = fixture.stop_evidence(intent_sha)
            with self.assertRaisesRegex(verifier.RetirementError, "still running"):
                verifier.finalize(
                    intent_path=fixture.intent,
                    expected_intent_sha256=intent_sha,
                    stop_evidence_path=evidence,
                    expected_stop_evidence_sha256=evidence_sha,
                    receipt_output=fixture.receipt,
                    runtime=fixture.runtime,
                    stability_seconds=0,
                    samples=3,
                    sleep=lambda _: None,
                )

    def test_pid_reuse_is_not_mistaken_for_old_identity(self) -> None:
        temporary, fixture = self.fixture()
        with temporary:
            intent, intent_sha = fixture.prepare()
            reused = copy.copy(fixture.runtime.observed)
            assert reused is not None
            fixture.runtime.observed = verifier.ProcessObservation(
                **{**reused.__dict__, "start_ticks": reused.start_ticks + 1, "executable": {"sha256": "f" * 64}}
            )
            evidence, evidence_sha = fixture.stop_evidence(intent_sha)
            receipt, _sha = verifier.finalize(
                intent_path=fixture.intent,
                expected_intent_sha256=intent_sha,
                stop_evidence_path=evidence,
                expected_stop_evidence_sha256=evidence_sha,
                receipt_output=fixture.receipt,
                runtime=fixture.runtime,
                stability_seconds=0,
                samples=3,
                sleep=lambda _: None,
            )
            self.assertTrue(receipt["offline_stability"]["exact_process_identity_absent"])
            self.assertEqual(intent["old_process"]["start_ticks"], fixture.legacy_start)

    def test_replacement_writer_or_legacy_port_blocks_retirement(self) -> None:
        for scenario in ("writer", "port"):
            with self.subTest(scenario=scenario):
                temporary, fixture = self.fixture(mode="preexisting_offline")
                with temporary:
                    if scenario == "writer":
                        fixture.runtime.replacements = [
                            verifier.ProcessObservation(
                                pid=9000,
                                boot_id=fixture.boot_id,
                                start_ticks=1,
                                uid=os.geteuid(),
                                gid=os.getegid(),
                                executable={"sha256": fixture.legacy_executable_sha},
                                argv=fixture.argv,
                                cwd=os.fspath(fixture.root),
                                listeners=(),
                            )
                        ]
                    else:
                        fixture.runtime.active = {("tcp4", "0100007F", 3001)}
                    with self.assertRaises(verifier.RetirementError):
                        fixture.prepare()

    def test_sigkill_clean_exit_and_wrong_signal_evidence_are_rejected(self) -> None:
        mutations = (
            lambda value: value["supervisor"].__setitem__("sigkill_sent", True),
            lambda value: value.__setitem__("legacy_exit_clean_claimed", True),
            lambda value: value["supervisor"].__setitem__("signals_sent", ["SIGKILL"]),
        )
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                temporary, fixture = self.fixture()
                with temporary:
                    _intent, intent_sha = fixture.prepare()
                    fixture.runtime.observed = None
                    evidence_path, _evidence_sha = fixture.stop_evidence(intent_sha)
                    value = json.loads(evidence_path.read_bytes())
                    mutate(value)
                    evidence_path.chmod(0o600)
                    evidence_sha = write(evidence_path, canonical(value))
                    with self.assertRaises(verifier.RetirementError):
                        verifier.finalize(
                            intent_path=fixture.intent,
                            expected_intent_sha256=intent_sha,
                            stop_evidence_path=evidence_path,
                            expected_stop_evidence_sha256=evidence_sha,
                            receipt_output=fixture.receipt,
                            runtime=fixture.runtime,
                            stability_seconds=0,
                            samples=3,
                            sleep=lambda _: None,
                        )

    def test_wal_append_is_preserved_but_shrink_or_prefix_mutation_fails(self) -> None:
        for scenario in ("append", "shrink", "mutate"):
            with self.subTest(scenario=scenario):
                temporary, fixture = self.fixture()
                with temporary:
                    original = fixture.wal.read_bytes()
                    _intent, intent_sha = fixture.prepare()
                    fixture.runtime.observed = None
                    fixture.wal.chmod(0o600)
                    if scenario == "append":
                        fixture.wal.write_bytes(original + b"appended frame")
                    elif scenario == "shrink":
                        fixture.wal.write_bytes(original[:-1])
                    else:
                        fixture.wal.write_bytes(b"X" + original[1:])
                    evidence, evidence_sha = fixture.stop_evidence(intent_sha)
                    call = lambda: verifier.finalize(
                        intent_path=fixture.intent,
                        expected_intent_sha256=intent_sha,
                        stop_evidence_path=evidence,
                        expected_stop_evidence_sha256=evidence_sha,
                        receipt_output=fixture.receipt,
                        runtime=fixture.runtime,
                        stability_seconds=0,
                        samples=3,
                        sleep=lambda _: None,
                    )
                    if scenario == "append":
                        receipt, _sha = call()
                        self.assertEqual(receipt["old_data_tree"]["state_wal_sha256"], digest(original + b"appended frame"))
                    else:
                        with self.assertRaisesRegex(verifier.RetirementError, "WAL"):
                            call()

    def test_symlink_hardlink_wal_and_output_inside_old_tree_are_rejected(self) -> None:
        temporary, fixture = self.fixture()
        with temporary:
            target = fixture.root / "outside.wal"
            write(target, b"outside", 0o600)
            fixture.wal.unlink()
            fixture.wal.symlink_to(target)
            with self.assertRaisesRegex(verifier.RetirementError, "WAL"):
                fixture.prepare()
        temporary, fixture = self.fixture()
        with temporary:
            os.link(fixture.wal, fixture.root / "second-wal-link")
            with self.assertRaisesRegex(verifier.RetirementError, "hard link"):
                fixture.prepare()
        temporary, fixture = self.fixture()
        with temporary:
            request = fixture.request()
            request = verifier.PrepareRequest(**{**request.__dict__, "intent_output": fixture.data / "intent.json"})
            with self.assertRaisesRegex(verifier.RetirementError, "outside"):
                verifier.prepare_intent(request, runtime=fixture.runtime)

    def test_tampered_release_boundary_descriptor_policy_and_supervisor_fail_closed(self) -> None:
        targets = ("release", "boundary", "descriptor", "policy", "supervisor")
        for target in targets:
            with self.subTest(target=target):
                temporary, fixture = self.fixture()
                with temporary:
                    path = getattr(fixture, target)
                    path.chmod(0o600)
                    path.write_bytes(path.read_bytes() + b"tamper")
                    path.chmod(0o400)
                    with self.assertRaises(verifier.RetirementError):
                        fixture.prepare()

    def test_policy_semantics_and_descriptor_validator_order_are_strict(self) -> None:
        mutations = (
            ("policy", lambda value: value.__setitem__("legacy_restart_allowed", True)),
            ("policy", lambda value: value["legacy_worker_rpc"].__setitem__("listener_ports", [9090])),
            ("descriptor", lambda value: value["approved_validators"].reverse()),
            ("descriptor", lambda value: value["canonical_inspection"].__setitem__("source_height", 137144)),
        )
        for target, mutate in mutations:
            with self.subTest(target=target, mutate=mutate):
                temporary, fixture = self.fixture()
                with temporary:
                    path = getattr(fixture, target)
                    value = json.loads(path.read_bytes())
                    mutate(value)
                    path.chmod(0o600)
                    new_sha = write(path, canonical(value))
                    request = fixture.request()
                    if target == "policy":
                        request = verifier.PrepareRequest(**{**request.__dict__, "cutover_policy_sha256": new_sha})
                        fixture.release_value["files"][verifier.CUTOVER_POLICY_ASSET] = new_sha
                    else:
                        request = verifier.PrepareRequest(**{**request.__dict__, "checkpoint_sha256": new_sha})
                        fixture.release_value["files"][verifier.CHECKPOINT_DESCRIPTOR_ASSET] = new_sha
                    fixture.release.chmod(0o600)
                    release_sha = write(fixture.release, canonical(fixture.release_value))
                    request = verifier.PrepareRequest(**{**request.__dict__, "target_release_sha256": release_sha})
                    with self.assertRaises(verifier.RetirementError):
                        verifier.prepare_intent(request, runtime=fixture.runtime)

    def test_stake_zero_argv_rejects_config_duplicates_and_nonzero_values(self) -> None:
        temporary, fixture = self.fixture()
        with temporary:
            bad = (
                (*fixture.argv, "--config", "/tmp/node.toml"),
                (*fixture.argv, "--stake", "0"),
                tuple("5" if item == "0" and index == 2 else item for index, item in enumerate(fixture.argv)),
            )
            for argv in bad:
                with self.assertRaises(verifier.RetirementError):
                    verifier.parse_stake_zero_argv(argv, fixture.data)

    def test_fresh_v08_path_cannot_exist_or_overlap_old_tree(self) -> None:
        temporary, fixture = self.fixture()
        with temporary:
            fixture.v08_data.mkdir(mode=0o700)
            with self.assertRaisesRegex(verifier.RetirementError, "absent"):
                fixture.prepare()
        temporary, fixture = self.fixture()
        with temporary:
            request = fixture.request()
            request = verifier.PrepareRequest(**{**request.__dict__, "v08_data_dir": fixture.data / "v0.8"})
            with self.assertRaisesRegex(verifier.RetirementError, "disjoint"):
                verifier.prepare_intent(request, runtime=fixture.runtime)

    def test_cli_version_is_standalone(self) -> None:
        result = subprocess.run(
            [sys.executable, os.fspath(MODULE_PATH), "--version"],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "arc-v07-retirement-verifier 1\n")


if __name__ == "__main__":
    unittest.main(verbosity=2)
