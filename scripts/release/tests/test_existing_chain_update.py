import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
import copy
from argparse import Namespace
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests" / "fixtures"
SPEC = importlib.util.spec_from_file_location("existing_chain_update", ROOT / "assemble-existing-chain-update.py")
mod = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(mod)
validator_path = ROOT / "update-quorum-validator.py"
validator = mod.load_module(validator_path, "rollout_fixture_for_update_tests")


def load(name):
    return json.loads((FIXTURES / name).read_text())


class ExistingChainUpdateTests(unittest.TestCase):
    def test_real_six_host_proof_uses_exact_rollout_validator(self):
        config = load("host-config-six-current-20260927.json")
        proof = load("six-host-before-cfd-20260927.json")
        hosts, total, quorum = validator.validate_host_config(config)
        captured = proof["captured_at_unix"]
        expected = {key: config[key] for key in ("genesis_file_sha256", "genesis_network_hash",
            "recovery_domain", "checkpoint_manifest_hash", "validator_set_id")}
        result = validator.validate_quorum_proof(proof, hosts, total, quorum,
            expected_network=expected, now=captured + 60)
        self.assertEqual(result["host_count"], 6)
        self.assertEqual(result["common_height"], proof["common_block"]["height"])

    def test_real_proof_rejects_host_swap_and_stale_timestamp(self):
        config = load("host-config-six-current-20260927.json")
        proof = load("six-host-before-cfd-20260927.json")
        hosts, total, quorum = validator.validate_host_config(config)
        expected = {key: config[key] for key in ("genesis_file_sha256", "genesis_network_hash",
            "recovery_domain", "checkpoint_manifest_hash", "validator_set_id")}
        swapped = copy.deepcopy(proof)
        swapped["common_block"]["hosts"][0]["hash"] = "0" * 64
        with self.assertRaises(validator.GateError):
            validator.validate_quorum_proof(swapped, hosts, total, quorum,
                expected_network=expected, now=proof["captured_at_unix"] + 60)
        with self.assertRaises(validator.GateError):
            validator.validate_quorum_proof(proof, hosts, total, quorum,
                expected_network=expected, now=proof["captured_at_unix"] + 121)

    def test_saved_selection_is_recomputed_from_exact_api_rows(self):
        commit = "cfd70e09ab1ea8ee7c7d948e86b47d3509e49119"
        selected, groups = mod.revalidate_api_selection(FIXTURES / "selection-live-all-nine.json",
            FIXTURES / "artifacts-live.json", ROOT / "select-pretag-artifacts.py",
            "FerrumVir/arc-chain", commit, 36336334198, 1)
        self.assertEqual(selected["run_id"], 36336334198)
        self.assertEqual(groups["macos-arm64"]["headless"]["id"], 10937288947)
        tampered = copy.deepcopy(selected)
        tampered["artifacts"]["macos-arm64"]["headless"]["id"] += 1
        path = FIXTURES / "selection-tampered-test.json"
        try:
            path.write_text(json.dumps(tampered))
            with self.assertRaisesRegex(mod.ProfileError, "does not reproduce exact selection"):
                mod.revalidate_api_selection(path, FIXTURES / "artifacts-live.json",
                    ROOT / "select-pretag-artifacts.py", "FerrumVir/arc-chain", commit, 36336334198, 1)
        finally:
            path.unlink(missing_ok=True)

    def test_verified_checkpoint_certificate_crossbinds_to_six_live_validators(self):
        config = load("host-config-six-current-20260927.json")
        verified = load("verified-checkpoint.json")
        hosts = {row["site"]: row for row in config["hosts"]}
        checkpoint = {"recovery_domain": mod.norm_hash(verified["recovery_domain"], "recovery_domain"),
            "protocol_version": verified["protocol_version"], "validator_set_id": verified["validator_set_id"],
            "validators": verified["validators"],
            "genesis_hash": mod.norm_hash(verified["genesis_hash"], "genesis_hash")}
        mod.validate_certificate_authority(checkpoint, hosts, config, config["genesis_file_sha256"])
        bad = copy.deepcopy(verified)
        bad["validators"][0]["stake"] += 1
        checkpoint["validators"] = bad["validators"]
        with self.assertRaisesRegex(mod.ProfileError, "authority differs"):
            mod.validate_certificate_authority(checkpoint, hosts, config, config["genesis_file_sha256"])

    def test_candidate_artifact_swap_cannot_satisfy_all_host_pin(self):
        config = load("host-config-six-current-20260927.json")
        proof = load("six-host-before-cfd-20260927.json")
        verified = load("verified-checkpoint.json")
        hosts = {row["site"]: row for row in config["hosts"]}
        checkpoint = {"recovery_domain": mod.norm_hash(verified["recovery_domain"], "recovery_domain"),
            "protocol_version": verified["protocol_version"], "validator_set_id": verified["validator_set_id"],
            "validators": verified["validators"],
            "genesis_hash": mod.norm_hash(verified["genesis_hash"], "genesis_hash")}
        with self.assertRaisesRegex(mod.ProfileError, "not pinned to the selected production candidate binary"):
            mod.validate_crossbindings(checkpoint, hosts, config, config["genesis_file_sha256"],
                "b4c038840204725cd0630f3647c044949b4f2f46b763bc26b349161e4f5740a6", proof,
                "97e45b43086d1fdcd69accc9c1bb5020496983f3739b7ede3c4d24c03f608eb3")

    def test_checkpoint_genesis_hash_is_distinct_from_rpc_legacy_field(self):
        config = load("host-config-six-current-20260927.json")
        verified = load("verified-checkpoint.json")
        from_file = (FIXTURES / "approved-genesis.network-hash").read_text().strip()
        checkpoint_hash = mod.norm_hash(verified["genesis_hash"], "genesis_hash")
        self.assertEqual(checkpoint_hash, from_file)
        self.assertNotEqual(checkpoint_hash, config["genesis_network_hash"])

    def test_hash_prefix_normalization_is_only_hex_prefix(self):
        self.assertEqual(mod.norm_hash("0x" + "a" * 64, "hash"), "a" * 64)
        with self.assertRaises(mod.ProfileError):
            mod.norm_hash("0x" + "g" * 64, "hash")

    def test_attestation_artifact_rows_do_not_leak_temporary_paths(self):
        row = mod.public_artifact("macos-arm64", {"binary_path": Path("/tmp/private/arc-node"),
            "genesis_path": Path("/tmp/private/genesis.toml"), "binary_sha256": "a" * 64,
            "artifact_id": 123})
        self.assertEqual(row["platform"], "macos-arm64")
        self.assertEqual(row["binary_sha256"], "a" * 64)
        self.assertNotIn("binary_path", row)
        self.assertNotIn("genesis_path", row)
        json.dumps(row)

    def test_profile_can_be_prepared_before_the_release_tag_exists(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            subprocess.run(["git", "-C", str(root), "config", "user.email", "test@example.invalid"], check=True)
            subprocess.run(["git", "-C", str(root), "config", "user.name", "Test"], check=True)
            (root / "file").write_text("main")
            subprocess.run(["git", "-C", str(root), "add", "file"], check=True)
            subprocess.run(["git", "-C", str(root), "commit", "-qm", "main"], check=True)
            sha = subprocess.check_output(["git", "-C", str(root), "rev-parse", "HEAD"], text=True).strip()
            self.assertNotEqual(subprocess.run(["git", "-C", str(root), "show-ref", "--verify", "refs/tags/v0.8.0"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode, 0)
            mod.validate_main_pin(root, sha, sha)
            with self.assertRaisesRegex(mod.ProfileError, "protected-main commit"):
                mod.validate_main_pin(root, sha, "0" * 40)

    def test_producer_reverification_preserves_owner_time_and_marks_scope(self):
        with tempfile.TemporaryDirectory() as td:
            path = Path(td) / "owner.json"
            owner = {"schema": mod.SCHEMA, "generated_at_unix": 1000,
                     "release": {"production_artifact": {"platform": "linux-x86_64", "binary_sha256": "a" * 64},
                                 "checkpoint_verifier_artifact": {"platform": "macos-arm64", "binary_sha256": "b" * 64,
                                     "artifact_id": 1, "artifact_digest": "sha256:" + "a" * 64},
                                 "selection_sha256": "c" * 64, "api_artifact_response_sha256": "d" * 64},
                     "checkpoint": {"checkpoint_sha256": "d" * 64},
                     "network": {"host_config_sha256": "e" * 64},
                     "fresh_six_host_proof": {"proof_sha256": "f" * 64}}
            path.write_text(json.dumps(owner))
            selection_path = Path(td) / "selection.json"
            selection_path.write_text(json.dumps({"schema": "arc.pretag.selection.v1",
                "repository": "FerrumVir/arc-chain", "commit": "c" * 40, "run_id": 123, "run_attempt": 1,
                "artifacts": {"macos-arm64": {"headless": {"id": 1, "digest": "sha256:" + "a" * 64}}}}))
            args = Namespace(rollout_validator=ROOT / "update-quorum-validator.py", commit="c" * 40,
                             run_id=123, run_attempt=1, selection=selection_path)
            producer = copy.deepcopy(owner)
            producer["release"]["checkpoint_verifier_artifact"] = {
                "platform": "linux-x86_64", "binary_sha256": "1" * 64}
            producer["release"]["api_artifact_response_sha256"] = "2" * 64
            with mock.patch.object(mod, "assemble", side_effect=lambda supplied: dict(producer)), \
                 mock.patch.object(mod, "verify_owner_verifier") as reverify:
                result = mod.validate_owner_attestation(args, path, now=1500)
            reverify.assert_called_once_with(args, owner["release"]["checkpoint_verifier_artifact"])
            self.assertEqual(args.proof_validation_time, 1000.0)
            self.assertEqual(args.attestation_generated_at, 1000.0)
            self.assertEqual(result["producer_reverification"]["observed_at_unix"], 1000.0)
            self.assertEqual(result["producer_reverification"]["independently_reverified_at_unix"], 1500)
            self.assertIn("not a live publication-time assertion",
                          result["producer_reverification"]["proof_scope"])
            self.assertEqual(result["producer_reverification"]["owner_checkpoint_verifier_platform"], "macos-arm64")
            self.assertEqual(result["producer_reverification"]["producer_checkpoint_verifier_platform"], "linux-x86_64")
            self.assertEqual(result["producer_reverification"]["owner_api_artifact_response_sha256"], "d" * 64)
            self.assertEqual(result["producer_reverification"]["independently_reverified_api_artifact_response_sha256"], "2" * 64)
            selection = json.loads(selection_path.read_text())
            selection["artifacts"]["macos-arm64"]["headless"]["digest"] = "sha256:" + "f" * 64
            selection_path.write_text(json.dumps(selection))
            with mock.patch.object(mod, "assemble", side_effect=lambda supplied: dict(producer)):
                with self.assertRaisesRegex(mod.ProfileError, "not selected by the exact preflight tuple"):
                    mod.validate_owner_attestation(args, path, now=1500)

    def test_producer_reverification_rejects_stale_future_or_changed_inputs(self):
        with tempfile.TemporaryDirectory() as td:
            path = Path(td) / "owner.json"
            selection_path = Path(td) / "selection.json"
            args = Namespace(rollout_validator=ROOT / "update-quorum-validator.py", commit="c" * 40,
                             run_id=123, run_attempt=1, selection=selection_path)
            for generated, now, message in ((1000, 8201, "two-hour"), (2000, 1000, "future")):
                path.write_text(json.dumps({"generated_at_unix": generated}))
                with self.subTest(generated=generated), self.assertRaisesRegex(mod.ProfileError, message):
                    mod.validate_owner_attestation(args, path, now=now)

            owner = {"schema": mod.SCHEMA, "generated_at_unix": 1000,
                     "release": {"checkpoint_verifier_artifact": {"platform": "macos-arm64",
                                  "artifact_id": 1, "artifact_digest": "sha256:" + "a" * 64},
                                 "production_artifact": {}, "selection_sha256": "b" * 64,
                                 "api_artifact_response_sha256": "c" * 64},
                     "checkpoint": {}, "network": {}, "fresh_six_host_proof": {}}
            path.write_text(json.dumps(owner))
            selection_path.write_text(json.dumps({"schema": "arc.pretag.selection.v1",
                    "repository": "FerrumVir/arc-chain", "commit": "c" * 40, "run_id": 123, "run_attempt": 1,
                    "artifacts": {"macos-arm64": {"headless": {"id": 1, "digest": "sha256:" + "a" * 64}}}}))
            with \
                 mock.patch.object(mod, "verify_owner_verifier"), \
                 mock.patch.object(mod, "assemble", return_value={"schema": mod.SCHEMA,
                    "generated_at_unix": 1000,
                    "release": {"checkpoint_verifier_artifact": {"platform": "linux-x86_64"},
                                "production_artifact": {}, "selection_sha256": "b" * 64,
                                "api_artifact_response_sha256": "c" * 64},
                    "checkpoint": {"checkpoint_sha256": "b" * 64}, "network": {}, "fresh_six_host_proof": {}}):
                with self.assertRaisesRegex(mod.ProfileError, "differs from independently reverified"):
                    mod.validate_owner_attestation(args, path, now=1500)

    def test_owner_verifier_rejects_payload_and_metadata_substitutions(self):
        artifact = {"binary_path": Path("/tmp/node"), "genesis_path": Path("/tmp/genesis"),
                    "binary_sha256": "a" * 64, "genesis_sha256": "b" * 64,
                    "artifact_id": 123, "artifact_digest": "sha256:" + "c" * 64,
                    "archive_sha256": "d" * 64, "build_metadata_sha256": "e" * 64,
                    "materialization_receipt_sha256": "f" * 64}
        owner = mod.public_artifact("macos-arm64", artifact)
        args = Namespace(artifact_platform="linux-x86_64", verifier_platform="linux-x86_64")
        with mock.patch.object(mod, "select_and_materialize", return_value=(None, {"macos-arm64": artifact}, None)):
            mod.verify_owner_verifier(args, owner)
            for field in ("binary_sha256", "genesis_sha256", "archive_sha256",
                          "build_metadata_sha256", "materialization_receipt_sha256", "extra"):
                with self.subTest(field=field), self.assertRaisesRegex(mod.ProfileError, "owner verifier bytes"):
                    mod.verify_owner_verifier(args, {**owner, field: "0" * 64})


if __name__ == "__main__":
    unittest.main()
