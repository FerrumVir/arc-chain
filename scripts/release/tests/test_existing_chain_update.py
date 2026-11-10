import importlib.util
import contextlib
import io
import json
import os
from pathlib import Path
import shlex
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

    NATIVE_TAIL = ["--native-inference-activation", "/etc/arc/native/activation.json",
                   "--native-inference-runtime", "--native-row-workers", "/etc/arc/native/cohort.json",
                   "--native-kv-budget-bytes", "67108864", "--enable-native-inference-requests"]
    NATIVE_FILES = {"/etc/arc/native/activation.json": "a" * 64, "/etc/arc/native/cohort.json": "b" * 64}

    def native_fixture(self):
        config = load("host-config-six-current-20260927.json")
        proof = load("six-host-before-cfd-20260927.json")
        native = {"argv_tail": list(self.NATIVE_TAIL), "files_sha256": dict(self.NATIVE_FILES),
                  "context_commitment": "d0" * 32}
        for host in config["hosts"]:
            host["native"] = copy.deepcopy(native)
        for sample in proof["samples"]:
            for node in sample["nodes"]:
                node["argv_redacted"] = node["argv_redacted"] + list(self.NATIVE_TAIL)
                node["native"] = copy.deepcopy(native)
                node["native_files_sha256"] = dict(self.NATIVE_FILES)
        expected = {key: config[key] for key in ("genesis_file_sha256", "genesis_network_hash",
            "recovery_domain", "checkpoint_manifest_hash", "validator_set_id")}
        return config, proof, expected

    def check(self, config, proof, expected):
        hosts, total, quorum = validator.validate_host_config(config)
        return validator.validate_quorum_proof(proof, hosts, total, quorum,
            expected_network=expected, now=proof["captured_at_unix"] + 60)

    def test_native_pins_accept_only_the_exact_tail_with_matching_evidence(self):
        config, proof, expected = self.native_fixture()
        self.assertEqual(self.check(config, proof, expected)["host_count"], 6)

        def nodes(p):
            return [node for sample in p["samples"] for node in sample["nodes"]]

        proof_mutations = [
            lambda p: nodes(p)[0]["argv_redacted"].__setitem__(-1, "--native-inference-runtime"),
            lambda p: nodes(p)[0]["argv_redacted"].insert(1, "--native-low-residency"),
            lambda p: nodes(p)[0]["argv_redacted"].pop(),
            lambda p: nodes(p)[0]["native_files_sha256"].__setitem__("/etc/arc/native/cohort.json", "c" * 64),
            lambda p: nodes(p)[0]["native"].__setitem__("context_commitment", "e0" * 32),
            lambda p: nodes(p)[0].pop("native"),
        ]
        for index, mutate in enumerate(proof_mutations):
            with self.subTest(proof_mutation=index):
                c, p, e = self.native_fixture()
                mutate(p)
                with self.assertRaises(validator.GateError):
                    self.check(c, p, e)
        config_mutations = [
            lambda c: c["hosts"][0].pop("native"),
            lambda c: c["hosts"][0]["native"].__setitem__("context_commitment", "e0" * 32),
            lambda c: c["hosts"][0]["native"]["argv_tail"].append("--stake"),
            lambda c: c["hosts"][0]["native"].__setitem__("argv_tail", ["67108864"]),
            lambda c: c["hosts"][0]["native"]["files_sha256"].__setitem__("relative/path", "a" * 64),
            lambda c: c["hosts"][0]["native"].__setitem__("extra", True),
        ]
        for index, mutate in enumerate(config_mutations):
            with self.subTest(config_mutation=index):
                c, p, e = self.native_fixture()
                mutate(c)
                with self.assertRaises(validator.GateError):
                    self.check(c, p, e)

    def test_native_flags_without_pins_stay_forbidden(self):
        config = load("host-config-six-current-20260927.json")
        proof = load("six-host-before-cfd-20260927.json")
        expected = {key: config[key] for key in ("genesis_file_sha256", "genesis_network_hash",
            "recovery_domain", "checkpoint_manifest_hash", "validator_set_id")}
        for flag in ("--native-inference-activation", "--enable-native-inference-requests"):
            with self.subTest(flag=flag):
                p = copy.deepcopy(proof)
                p["samples"][0]["nodes"][0]["argv_redacted"].append(flag)
                with self.assertRaises(validator.GateError):
                    self.check(config, p, expected)
        p = copy.deepcopy(proof)
        p["samples"][0]["nodes"][0]["native_files_sha256"] = dict(self.NATIVE_FILES)
        with self.assertRaises(validator.GateError):
            self.check(config, p, expected)

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

    def test_checkpoint_verifier_resolves_relative_inputs_before_changing_directory(self):
        verified = load("verified-checkpoint.json")
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            inputs = root / "inputs"
            inputs.mkdir()
            checkpoint = inputs / "recovery.arcchkpt"
            checkpoint.write_bytes(b"bounded test checkpoint")
            genesis = inputs / "genesis.toml"
            genesis.write_text("[genesis]\n", encoding="utf-8")
            response = root / "verified.json"
            response.write_text(json.dumps(verified), encoding="utf-8")
            binary = root / "arc-node-test"
            binary.write_text(
                "#!/bin/sh\n"
                "set -eu\n"
                "checkpoint=\n"
                "genesis=\n"
                "while [ \"$#\" -gt 0 ]; do\n"
                "  case \"$1\" in\n"
                "    --checkpoint) shift; checkpoint=\"$1\" ;;\n"
                "    --genesis) shift; genesis=\"$1\" ;;\n"
                "  esac\n"
                "  shift\n"
                "done\n"
                "case \"$checkpoint\" in /*) ;; *) echo relative-checkpoint >&2; exit 20 ;; esac\n"
                "[ -f \"$checkpoint\" ] || { echo missing-checkpoint >&2; exit 21; }\n"
                "if [ -n \"$genesis\" ]; then\n"
                "  case \"$genesis\" in /*) ;; *) echo relative-genesis >&2; exit 22 ;; esac\n"
                "  [ -f \"$genesis\" ] || { echo missing-genesis >&2; exit 23; }\n"
                "fi\n"
                f"cat {shlex.quote(str(response))}\n",
                encoding="utf-8")
            binary.chmod(0o755)
            prior_cwd = Path.cwd()
            os.chdir(root)
            try:
                result = mod.verify_checkpoint(binary,
                    Path("inputs/recovery.arcchkpt"), Path("inputs/genesis.toml"),
                    mod.norm_hash(verified["manifest_hash"], "manifest_hash"), verified["recovery_epoch"],
                    verified["validator_set_id"], mod.norm_hash(verified["recovery_domain"], "recovery_domain"))
            finally:
                os.chdir(prior_cwd)
            self.assertEqual(result["checkpoint_sha256"], mod.sha_file(checkpoint))
            self.assertEqual(result["transition_height"], verified["transition_height"])

    def test_checkpoint_verifier_still_rejects_missing_and_symlink_inputs(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            binary = root / "arc-node-test"
            binary.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            binary.chmod(0o755)
            linked_binary = root / "linked-arc-node"
            linked_binary.symlink_to(binary)
            checkpoint = root / "checkpoint"
            checkpoint.write_bytes(b"checkpoint")
            genesis = root / "genesis"
            genesis.write_bytes(b"genesis")
            linked_checkpoint = root / "linked-checkpoint"
            linked_checkpoint.symlink_to(checkpoint)
            linked_genesis = root / "linked-genesis"
            linked_genesis.symlink_to(genesis)
            with mock.patch.object(mod.subprocess, "run") as run:
                for supplied_checkpoint, supplied_genesis in (
                    (root / "missing-checkpoint", genesis),
                    (linked_checkpoint, genesis),
                    (checkpoint, linked_genesis),
                ):
                    with self.subTest(checkpoint=supplied_checkpoint, genesis=supplied_genesis):
                        with self.assertRaisesRegex(mod.ProfileError, "regular files"):
                            mod.verify_checkpoint(binary, supplied_checkpoint, supplied_genesis,
                                "a" * 64, 1, 1, "b" * 64)
                with self.assertRaisesRegex(mod.ProfileError, "selected verifier binary"):
                    mod.verify_checkpoint(linked_binary, checkpoint, genesis, "a" * 64, 1, 1, "b" * 64)
                run.assert_not_called()

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


OFFLINE_SITE = "ams"
CANDIDATE = "c" * 64
NETWORK_KEYS = ("genesis_file_sha256", "genesis_network_hash", "recovery_domain",
                "checkpoint_manifest_hash", "validator_set_id")


def offline_probe(pin, status=502, binary=None):
    return {"site": pin["site"], "ip": pin["ip"], "validator": pin["validator"],
            "health_url": "https://" + pin["ip"] + "/health", "http_status": status, "binary_sha256": binary}


def five_live_inputs(candidate=CANDIDATE, status=502):
    """The real six-host fixtures with ams declared offline and the other five live.

    Returns the five-host config and proof, the declared row, and the two rows ams
    reported in the original capture (on its own, non-candidate binary).
    """
    config = load("host-config-six-current-20260927.json")
    proof = load("six-host-before-cfd-20260927.json")
    offline = next(host for host in config["hosts"] if host["site"] == OFFLINE_SITE)
    declared = {key: offline[key] for key in ("site", "ip", "validator", "stake")}
    answering_rows = [node for sample in proof["samples"] for node in sample["nodes"]
                      if node["site"] == OFFLINE_SITE]
    config["hosts"] = [host for host in config["hosts"] if host["site"] != OFFLINE_SITE]
    config["quorum_sites"] = [site for site in config["quorum_sites"] if site != OFFLINE_SITE]
    config["unobserved_stake"] = offline["stake"]
    config["declared_offline"] = [declared]
    for sample in proof["samples"]:
        sample["nodes"] = [node for node in sample["nodes"] if node["site"] != OFFLINE_SITE]
        sample["declared_offline"] = [offline_probe(declared, status)]
    proof["common_block"]["hosts"] = [row for row in proof["common_block"]["hosts"]
                                      if row["site"] != OFFLINE_SITE]
    if candidate is not None:
        for host in config["hosts"]:
            host["baseline_binary_sha256"] = candidate
        for sample in proof["samples"]:
            for node in sample["nodes"]:
                node["binary_sha256"] = candidate
    return config, proof, declared, answering_rows


def check_quorum(config, proof, declared=None):
    hosts, total, quorum = validator.validate_host_config(config)
    declared = validator.declared_offline_hosts(config) if declared is None else declared
    return validator.validate_quorum_proof(proof, hosts, total, quorum,
        expected_network={key: config[key] for key in NETWORK_KEYS},
        now=proof["captured_at_unix"] + 60, declared_offline=declared)


def assembler_proof(config, proof, sites=(OFFLINE_SITE,), candidate=CANDIDATE):
    """The assembler's own gate over files on disk, as the owner and the producer run it."""
    with tempfile.TemporaryDirectory() as td:
        config_path, proof_path = Path(td) / "host-config.json", Path(td) / "quorum-proof.json"
        config_path.write_text(json.dumps(config))
        proof_path.write_text(json.dumps(proof))
        return mod.strict_proof(proof_path, config_path, validator_path, candidate,
                                now=proof["captured_at_unix"] + 60, declared_offline_sites=sites)


def verified_certificate():
    verified = load("verified-checkpoint.json")
    return {"recovery_domain": mod.norm_hash(verified["recovery_domain"], "recovery_domain"),
            "protocol_version": verified["protocol_version"], "validator_set_id": verified["validator_set_id"],
            "validators": verified["validators"],
            "genesis_hash": mod.norm_hash(verified["genesis_hash"], "genesis_hash")}


class DeclaredOfflineValidatorTests(unittest.TestCase):
    def test_five_live_with_one_declared_offline_passes(self):
        config, proof, declared, _ = five_live_inputs()
        summary = check_quorum(config, proof)
        self.assertEqual(summary["host_count"], 5)
        self.assertEqual(summary["online_stake"], 33333333)
        self.assertEqual(summary["declared_offline_sites"], [OFFLINE_SITE])
        self.assertEqual(summary["declared_offline_stake"], declared["stake"])
        self.assertEqual(summary["online_stake"] + summary["declared_offline_stake"], config["total_stake"])
        for status in (None, 503, 504):
            with self.subTest(status=status):
                c, p, _, _ = five_live_inputs(status=status)
                self.assertEqual(check_quorum(c, p)["host_count"], 5)
        result, checked = assembler_proof(config, proof)
        self.assertEqual(checked, config)
        self.assertEqual((result["host_count"], result["declared_offline_sites"]), (5, [OFFLINE_SITE]))
        hosts = {row["site"]: row for row in config["hosts"]}
        approved = (FIXTURES / "approved-genesis.network-hash").read_text().strip()
        mod.validate_crossbindings(verified_certificate(), hosts, config, config["genesis_file_sha256"],
                                   CANDIDATE, proof, approved)

    def test_five_live_hosts_keep_the_four_peer_floor(self):
        for peers, passes in ((4, True), (5, True), (3, False)):
            with self.subTest(peers=peers):
                config, proof, _, _ = five_live_inputs()
                for sample in proof["samples"]:
                    for node in sample["nodes"]:
                        node["health"]["peers"] = peers
                if passes:
                    self.assertEqual(check_quorum(config, proof)["host_count"], 5)
                else:
                    with self.assertRaisesRegex(validator.GateError, "host health/peer/progress gate failed"):
                        check_quorum(config, proof)

    def test_five_live_with_none_declared_fails(self):
        config, proof, _, _ = five_live_inputs()
        anonymous = copy.deepcopy(config)
        del anonymous["declared_offline"]
        with self.assertRaisesRegex(validator.GateError, "unobserved stake must be exactly"):
            validator.validate_host_config(anonymous)
        undeclared = copy.deepcopy(anonymous)
        undeclared["unobserved_stake"] = 0
        with self.assertRaisesRegex(validator.GateError, "observed and explicitly unobserved stake"):
            validator.validate_host_config(undeclared)
        unprobed = copy.deepcopy(proof)
        for sample in unprobed["samples"]:
            del sample["declared_offline"]
        for supplied in (anonymous, undeclared):
            with self.subTest(unobserved_stake=supplied["unobserved_stake"]):
                with self.assertRaisesRegex(RuntimeError, "unobserved stake"):
                    assembler_proof(supplied, unprobed, sites=())
        hosts = {row["site"]: row for row in anonymous["hosts"]}
        with self.assertRaisesRegex(mod.ProfileError, "authority differs"):
            mod.validate_certificate_authority(verified_certificate(), hosts, anonymous,
                                               anonymous["genesis_file_sha256"])
        # The declaration needs its explicit confirmation, and a confirmation needs the declaration.
        for sites in ((), ("nrt",), (OFFLINE_SITE, OFFLINE_SITE)):
            with self.subTest(confirmed=sites):
                with self.assertRaisesRegex(mod.ProfileError, "explicit --declared-offline confirmation"):
                    assembler_proof(config, proof, sites=sites)

    def test_declared_offline_host_answering_on_a_different_binary_fails(self):
        _, _, _, answering_rows = five_live_inputs()
        old_binary = answering_rows[0]["binary_sha256"]
        self.assertNotEqual(old_binary, CANDIDATE)

        def health(**update):
            return lambda p: p["samples"][1]["declared_offline"][0].update(update)

        answers = {
            "health answers on a different binary": health(http_status=200, binary_sha256=old_binary),
            "health answers on the candidate binary": health(http_status=200, binary_sha256=CANDIDATE),
            "health answers without a binary hash": health(http_status=200),
            "health answers with another status": health(http_status=404),
            "a binary behind a failing front": health(http_status=502, binary_sha256=old_binary),
        }
        for label, mutate in answers.items():
            with self.subTest(label):
                c, p, _, _ = five_live_inputs()
                mutate(p)
                with self.assertRaisesRegex(validator.GateError, "declared-offline validator ams"):
                    check_quorum(c, p)
                with self.assertRaisesRegex(RuntimeError, "declared-offline validator ams"):
                    assembler_proof(c, p)
        # The capturer reached ams and sampled it, still on its old binary.
        c, p, _, rows = five_live_inputs()
        for sample, row in zip(p["samples"], rows):
            sample["nodes"].append(row)
        with self.assertRaisesRegex(validator.GateError, "answered the capture as a live host in the samples"):
            check_quorum(c, p)
        with self.assertRaisesRegex(RuntimeError, "answered the capture as a live host"):
            assembler_proof(c, p)
        c, p, _, _ = five_live_inputs()
        p["common_block"]["hosts"].append({**p["common_block"]["hosts"][0], "site": OFFLINE_SITE})
        with self.assertRaisesRegex(validator.GateError, "answered the capture as a live host in the common block"):
            check_quorum(c, p)

    def test_live_stake_below_quorum_fails(self):
        config, proof, _, _ = five_live_inputs()
        # Same identities and total stake, but the declared validator holds enough that
        # the five live hosts fall one short of the quorum.
        moves = {"nyc": 5666667, "lhr": 1000000}
        for host in config["hosts"]:
            host["stake"] -= moves.get(host["site"], 0)
        for sample in proof["samples"]:
            for node in sample["nodes"]:
                node["stake"] -= moves.get(node["site"], 0)
        config["declared_offline"][0]["stake"] += sum(moves.values())
        config["unobserved_stake"] = config["declared_offline"][0]["stake"]
        live = sum(host["stake"] for host in config["hosts"])
        self.assertEqual((live, live + config["unobserved_stake"]),
                         (config["quorum_stake"] - 1, config["total_stake"]))
        with self.assertRaisesRegex(validator.GateError, "cannot meet the stake quorum"):
            validator.validate_host_config(config)
        hosts = {host["site"]: host for host in config["hosts"]}
        with self.assertRaisesRegex(validator.GateError, "below threshold"):
            validator.validate_quorum_proof(proof, hosts, config["total_stake"], config["quorum_stake"],
                expected_network={key: config[key] for key in NETWORK_KEYS},
                now=proof["captured_at_unix"] + 60,
                declared_offline={OFFLINE_SITE: config["declared_offline"][0]})
        with self.assertRaisesRegex(RuntimeError, "cannot meet the stake quorum"):
            assembler_proof(config, proof)

    def test_offline_set_is_never_inferred_from_a_silent_host(self):
        six = load("host-config-six-current-20260927.json")
        _, silent, _, _ = five_live_inputs(candidate=None)
        hosts, total, quorum = validator.validate_host_config(six)
        self.assertEqual(validator.declared_offline_hosts(six), {})
        expected = {key: six[key] for key in NETWORK_KEYS}
        with self.assertRaisesRegex(validator.GateError, "must not carry offline probes"):
            validator.validate_quorum_proof(silent, hosts, total, quorum, expected_network=expected,
                                            now=silent["captured_at_unix"] + 60)
        for sample in silent["samples"]:
            del sample["declared_offline"]
        with self.assertRaisesRegex(validator.GateError, "sample missing or duplicating a quorum host"):
            validator.validate_quorum_proof(silent, hosts, total, quorum, expected_network=expected,
                                            now=silent["captured_at_unix"] + 60)
        # With ams declared, a second host that does not answer is a refusal, never a second offline validator.
        config, proof, _, _ = five_live_inputs()
        for sample in proof["samples"]:
            sample["nodes"] = [node for node in sample["nodes"] if node["site"] != "nrt"]
        with self.assertRaisesRegex(validator.GateError, "sample missing or duplicating a quorum host"):
            check_quorum(config, proof)
        with self.assertRaisesRegex(RuntimeError, "sample missing or duplicating a quorum host"):
            assembler_proof(config, proof)

    def test_declaration_binds_identity_certificate_and_probe(self):
        config, _, declared, _ = five_live_inputs()
        hosts = {row["site"]: row for row in config["hosts"]}
        mod.validate_certificate_authority(verified_certificate(), hosts, config, config["genesis_file_sha256"])
        for field, value in (("validator", "e" * 64), ("stake", declared["stake"] - 1)):
            with self.subTest(certificate_field=field):
                c = copy.deepcopy(config)
                c["declared_offline"][0][field] = value
                with self.assertRaisesRegex(mod.ProfileError, "authority differs"):
                    mod.validate_certificate_authority(verified_certificate(), hosts, c, c["genesis_file_sha256"])
        live = config["hosts"][0]
        config_mutations = {
            "declared site is live": lambda c: c["declared_offline"][0].update(site=live["site"]),
            "declared identity is live": lambda c: c["declared_offline"][0].update(validator=live["validator"]),
            "declared IP is live": lambda c: c["declared_offline"][0].update(ip=live["ip"]),
            "extra declared field": lambda c: c["declared_offline"][0].update(reason="text"),
            "declaration is not a list": lambda c: c.update(declared_offline=c["declared_offline"][0]),
        }
        for label, mutate in config_mutations.items():
            with self.subTest(config=label):
                c = copy.deepcopy(config)
                mutate(c)
                with self.assertRaises(validator.GateError):
                    validator.validate_host_config(c)
        probe_mutations = {
            "missing in one sample": lambda p: p["samples"][0].pop("declared_offline"),
            "empty in one sample": lambda p: p["samples"][1].update(declared_offline=[]),
            "repeated": lambda p: p["samples"][0]["declared_offline"].append(
                dict(p["samples"][0]["declared_offline"][0])),
            "another IP": lambda p: p["samples"][0]["declared_offline"][0].update(ip="192.0.2.1"),
            "plain HTTP": lambda p: p["samples"][0]["declared_offline"][0].update(
                health_url="http://" + declared["ip"] + "/health"),
            "another host's endpoint": lambda p: p["samples"][0]["declared_offline"][0].update(
                health_url="https://" + live["ip"] + "/health"),
            "extra probe field": lambda p: p["samples"][0]["declared_offline"][0].update(note="text"),
            "status as text": lambda p: p["samples"][0]["declared_offline"][0].update(http_status="502"),
        }
        for label, mutate in probe_mutations.items():
            with self.subTest(probe=label):
                c, p, _, _ = five_live_inputs()
                mutate(p)
                with self.assertRaises(validator.GateError):
                    check_quorum(c, p)

    def test_six_host_update_keeps_its_strict_shape(self):
        config = load("host-config-six-current-20260927.json")
        proof = load("six-host-before-cfd-20260927.json")
        for host in config["hosts"]:
            host["baseline_binary_sha256"] = CANDIDATE
        for sample in proof["samples"]:
            for node in sample["nodes"]:
                node["binary_sha256"] = CANDIDATE
        summary, _ = assembler_proof(config, proof, sites=())
        self.assertEqual(summary["host_count"], 6)
        self.assertFalse({"declared_offline_sites", "declared_offline_stake"} & set(summary))
        with self.assertRaisesRegex(mod.ProfileError, "explicit --declared-offline confirmation"):
            assembler_proof(config, proof, sites=(OFFLINE_SITE,))
        probed = copy.deepcopy(proof)
        ams = next(host for host in config["hosts"] if host["site"] == OFFLINE_SITE)
        probed["samples"][0]["declared_offline"] = [offline_probe(ams)]
        with self.assertRaisesRegex(RuntimeError, "must not carry offline probes"):
            assembler_proof(config, probed, sites=())

    def run_assemble(self, config, proof, sites):
        """assemble() with only artifact materialization and the checkpoint CLI stubbed."""
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            config_path, proof_path = root / "host-config.json", root / "quorum-proof.json"
            config_path.write_text(json.dumps(config))
            proof_path.write_text(json.dumps(proof))
            artifact = {"binary_path": root / "arc-node", "genesis_path": root / "genesis.toml",
                        "binary_sha256": CANDIDATE, "genesis_sha256": config["genesis_file_sha256"],
                        "artifact_id": 1, "artifact_digest": "sha256:" + "a" * 64, "archive_sha256": "b" * 64,
                        "build_metadata_sha256": "d" * 64, "materialization_receipt_sha256": "e" * 64}
            args = Namespace(repository="FerrumVir/arc-chain", tag="v0.8.17", commit="c" * 40, main_sha="c" * 40,
                             run_id=1, run_attempt=1, recovery_epoch=1, repository_root=root,
                             artifact_platform="linux-x86_64", verifier_platform="linux-x86_64",
                             host_config=config_path, quorum_proof=proof_path, checkpoint=root / "recovery.arcchkpt",
                             approved_genesis_network_hash=FIXTURES / "approved-genesis.network-hash",
                             rollout_validator=validator_path, selection=FIXTURES / "selection-live-all-nine.json",
                             api_artifacts=FIXTURES / "artifacts-live.json", selector=ROOT / "select-pretag-artifacts.py",
                             declared_offline=list(sites) or None,
                             proof_validation_time=proof["captured_at_unix"] + 60,
                             attestation_generated_at=proof["captured_at_unix"] + 61)
            with mock.patch.object(mod, "validate_main_pin"), \
                 mock.patch.object(mod, "select_and_materialize",
                                   return_value=(None, {"linux-x86_64": artifact}, {"sha256": "f" * 64})), \
                 mock.patch.object(mod, "verify_checkpoint", return_value=verified_certificate()) as verify:
                try:
                    return mod.assemble(args), verify
                except mod.ProfileError as error:
                    return error, verify

    def test_attestation_records_the_declared_offline_validator(self):
        config, proof, declared, _ = five_live_inputs()
        result, verify = self.run_assemble(config, proof, (OFFLINE_SITE,))
        self.assertIsInstance(result, dict)
        self.assertEqual(result["claim"], {"existing_recovered_chain_update": True,
            "original_cutover_ceremony_verified": False, "original_cutover_archive_present": False,
            "validator_retirement_authorized": False})
        self.assertEqual(result["declared_offline_validators"], [declared])
        self.assertNotIn(mod.SIX_HOST_PROOF_KEY, result)
        live = result[mod.LIVE_HOST_PROOF_KEY]
        self.assertEqual((live["host_count"], live["online_stake"], live["declared_offline_sites"],
                          live["declared_offline_stake"]), (5, 33333333, [OFFLINE_SITE], declared["stake"]))
        self.assertEqual(mod.attested_proof_key(result), mod.LIVE_HOST_PROOF_KEY)
        # Without the explicit confirmation the declaration is refused before the slow checkpoint check.
        refused, verify = self.run_assemble(config, proof, ())
        self.assertIsInstance(refused, mod.ProfileError)
        self.assertIn("explicit --declared-offline confirmation", str(refused))
        verify.assert_not_called()
        # A six-host update keeps exactly today's shape.
        six = load("host-config-six-current-20260927.json")
        six_proof = load("six-host-before-cfd-20260927.json")
        for host in six["hosts"]:
            host["baseline_binary_sha256"] = CANDIDATE
        for sample in six_proof["samples"]:
            for node in sample["nodes"]:
                node["binary_sha256"] = CANDIDATE
        strict, _ = self.run_assemble(six, six_proof, ())
        self.assertIsInstance(strict, dict)
        self.assertFalse({"declared_offline_validators", mod.LIVE_HOST_PROOF_KEY} & set(strict))
        self.assertEqual(strict[mod.SIX_HOST_PROOF_KEY]["host_count"], 6)
        self.assertFalse({"declared_offline_sites", "declared_offline_stake"} & set(strict[mod.SIX_HOST_PROOF_KEY]))
        refused, verify = self.run_assemble(six, six_proof, (OFFLINE_SITE,))
        self.assertIsInstance(refused, mod.ProfileError)
        verify.assert_not_called()

    def test_attestation_proof_key_follows_the_declaration(self):
        six = {mod.SIX_HOST_PROOF_KEY: {"common_height": 1}}
        live = {"declared_offline_validators": [{"site": OFFLINE_SITE}], mod.LIVE_HOST_PROOF_KEY: {"common_height": 1}}
        self.assertEqual(mod.attested_proof_key(six), mod.SIX_HOST_PROOF_KEY)
        self.assertEqual(mod.attested_proof_key(live), mod.LIVE_HOST_PROOF_KEY)
        for bad in ({}, {**six, **live}, {mod.LIVE_HOST_PROOF_KEY: {}},
                    {"declared_offline_validators": [{"site": OFFLINE_SITE}], **six}):
            with self.subTest(keys=sorted(bad)), self.assertRaises(mod.ProfileError):
                mod.attested_proof_key(bad)

    def test_producer_reverification_marks_the_declared_offline_scope(self):
        with tempfile.TemporaryDirectory() as td:
            config, _, declared, _ = five_live_inputs()
            path = Path(td) / "owner.json"
            owner = {"schema": mod.SCHEMA, "generated_at_unix": 1000,
                     "release": {"production_artifact": {"platform": "linux-x86_64", "binary_sha256": "a" * 64},
                                 "checkpoint_verifier_artifact": {"platform": "macos-arm64", "binary_sha256": "b" * 64,
                                     "artifact_id": 1, "artifact_digest": "sha256:" + "a" * 64},
                                 "selection_sha256": "c" * 64, "api_artifact_response_sha256": "d" * 64},
                     "checkpoint": {"checkpoint_sha256": "d" * 64},
                     "network": {"host_config_sha256": "e" * 64},
                     "declared_offline_validators": config["declared_offline"],
                     mod.LIVE_HOST_PROOF_KEY: {"proof_sha256": "f" * 64, "captured_at_unix": 990}}
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
            with mock.patch.object(mod, "assemble", side_effect=lambda supplied: copy.deepcopy(producer)), \
                 mock.patch.object(mod, "verify_owner_verifier"):
                result = mod.validate_owner_attestation(args, path, now=1500)
            reverification = result["producer_reverification"]
            self.assertEqual(reverification["proof_scope"], mod.LIVE_HOST_PROOF_SCOPE)
            self.assertIn("not a live publication-time assertion", reverification["proof_scope"])
            self.assertEqual(reverification["freshness_at_capture_unix"], 990)
            self.assertEqual(result["declared_offline_validators"], [declared])
            # The producer derives the offline set from the host config itself; an owner
            # declaration that the producer does not reproduce exactly is refused.
            producer.pop("declared_offline_validators")
            with mock.patch.object(mod, "assemble", side_effect=lambda supplied: copy.deepcopy(producer)), \
                 mock.patch.object(mod, "verify_owner_verifier"):
                with self.assertRaisesRegex(mod.ProfileError, "differs from independently reverified"):
                    mod.validate_owner_attestation(args, path, now=1500)



def synthetic_update_inputs(stakes=(11, 10, 10, 10, 10, 10), offline_count=2):
    """Six synthetic validators, documentation IPs, and no captured fleet data."""
    sites = ("aaa", "bbb", "ccc", "ddd", "eee", "fff")
    total = sum(stakes)
    config = {"schema": validator.HOST_SCHEMA, "total_stake": total,
              "quorum_stake": 1 if total == 0 else total - (total - 1) // 3, "validator_set_id": 1,
              "genesis_file_sha256": "1" * 64, "genesis_network_hash": "2" * 64,
              "recovery_domain": "3" * 64, "checkpoint_manifest_hash": "4" * 64}
    all_hosts = []
    for index, (site, stake) in enumerate(zip(sites, stakes), 1):
        all_hosts.append({"site": site, "ip": f"192.0.2.{index}", "hostname": "arc-node-" + site,
                          "validator": f"{index:064x}", "stake": stake,
                          "unit": f"arc-node-v3-{site}.service", "data_dir": "/data/" + site,
                          "genesis_path": "/config/genesis.toml", "genesis_sha256": "1" * 64,
                          "baseline_binary_sha256": CANDIDATE, "legacy_v3_wire": False})
    config["hosts"] = all_hosts[:6 - offline_count]
    config["quorum_sites"] = [host["site"] for host in config["hosts"]]
    offline = all_hosts[6 - offline_count:]
    config["unobserved_stake"] = sum(host["stake"] for host in offline)
    config["declared_offline"] = [{key: host[key] for key in ("site", "ip", "validator", "stake")}
                                  for host in offline]
    proof = {"schema": validator.QUORUM_SCHEMA,
             "transport": "StrictHostKeyChecking=yes IdentitiesOnly=yes", "captured_at_unix": 1020,
             "total_stake": total, "quorum_stake": config["quorum_stake"],
             **{key: config[key] for key in NETWORK_KEYS}, "samples": []}
    for sample_index, timestamp in enumerate((1000, 1010)):
        nodes = []
        for index, host in enumerate(config["hosts"], 1):
            nodes.append({**{key: host[key] for key in ("site", "ip", "hostname", "validator", "stake")},
                          "active": "active", "binary_sha256": CANDIDATE,
                          "argv_redacted": ["/bin/arc-node.synthetic", "--data-dir", host["data_dir"],
                                            "--genesis", host["genesis_path"]],
                          "network_total_stake": total, "active_stake": total - config["unobserved_stake"],
                          "genesis_sha256": config["genesis_file_sha256"],
                          "network_genesis_hash": config["genesis_network_hash"],
                          **{key: config[key] for key in NETWORK_KEYS[2:]},
                          "pid": 100 + index, "restarts": 0, "invocation_id": f"{index:032x}",
                          "height": 100 + sample_index, "last_block_height": 100 + sample_index,
                          "health": {"status": "ok", "chain_advancing": True,
                                     "peers": len(config["hosts"]) - 1,
                                     "validators": 6, "last_block_age_secs": 1}})
        sample = {"captured_at_unix": timestamp, "nodes": nodes}
        if offline:
            sample["declared_offline"] = [offline_probe(host) for host in offline]
        proof["samples"].append(sample)
    block = {"height": 99, "hash": "5" * 64, "state_root": "6" * 64, "protocol_major": 3}
    proof["common_block"] = {**block, "hosts": [{"site": host["site"], **block} for host in config["hosts"]]}
    return config, proof


class TwoDeclaredOfflineTests(unittest.TestCase):
    def test_two_offline_at_threshold_and_above_pass(self):
        for stakes in ((11, 10, 10, 10, 10, 10), (12, 11, 10, 10, 9, 9)):
            with self.subTest(stakes=stakes):
                config, proof = synthetic_update_inputs(stakes)
                summary = check_quorum(config, proof)
                self.assertEqual(summary["host_count"], 4)
                self.assertEqual(summary["declared_offline_sites"], ["eee", "fff"])
                self.assertGreaterEqual(summary["online_stake"], config["quorum_stake"])
                self.assertEqual(summary["online_stake"] + summary["declared_offline_stake"], sum(stakes))
                result, _ = assembler_proof(config, proof, sites=("fff", "eee"))
                self.assertEqual(result["online_stake"], sum(stakes[:4]))

    def test_four_live_hosts_each_need_the_other_three_as_peers(self):
        config, proof = synthetic_update_inputs()
        self.assertEqual({node["health"]["peers"] for sample in proof["samples"] for node in sample["nodes"]}, {3})
        self.assertEqual(check_quorum(config, proof)["host_count"], 4)
        for peers in (2, 0, None, "3", 3.0, True):
            with self.subTest(peers=peers):
                c, p = synthetic_update_inputs()
                p["samples"][1]["nodes"][2]["health"]["peers"] = peers
                with self.assertRaisesRegex(validator.GateError, "host health/peer/progress gate failed"):
                    check_quorum(c, p)
        c, p = synthetic_update_inputs()
        del p["samples"][0]["nodes"][0]["health"]["peers"]
        with self.assertRaisesRegex(validator.GateError, "host health/peer/progress gate failed"):
            check_quorum(c, p)

    def test_two_offline_one_below_threshold_fails(self):
        config, proof = synthetic_update_inputs((10, 10, 10, 10, 11, 10))
        self.assertEqual(sum(host["stake"] for host in config["hosts"]), config["quorum_stake"] - 1)
        with self.assertRaisesRegex(validator.GateError, "cannot meet the stake quorum"):
            check_quorum(config, proof)
        with self.assertRaisesRegex(RuntimeError, "cannot meet the stake quorum"):
            assembler_proof(config, proof, sites=("eee", "fff"))

    def test_three_offline_fail_even_with_enough_live_stake(self):
        config, proof = synthetic_update_inputs((20, 20, 18, 1, 1, 1), offline_count=3)
        hosts = {host["site"]: host for host in config["hosts"]}
        declared = {row["site"]: row for row in config["declared_offline"]}
        with self.assertRaisesRegex(validator.GateError, "at most two"):
            validator.validate_host_config(config)
        with self.assertRaisesRegex(validator.GateError, "at most two"):
            validator.validate_quorum_proof(proof, hosts, config["total_stake"], config["quorum_stake"],
                                            now=1080, declared_offline=declared)
        with self.assertRaisesRegex(RuntimeError, "at most two"):
            assembler_proof(config, proof, sites=tuple(declared))
        with self.assertRaisesRegex(mod.ProfileError, "more than two"):
            mod.declared_offline_rows(config)

    def test_duplicate_and_live_site_ip_or_identity_are_refused(self):
        for field in ("site", "ip", "validator"):
            for source in ("declared_offline", "hosts"):
                with self.subTest(field=field, source=source):
                    config, proof = synthetic_update_inputs()
                    config["declared_offline"][1][field] = config[source][0][field]
                    with self.assertRaises(validator.GateError):
                        check_quorum(config, proof)

    def test_either_offline_probe_missing_or_repeated_in_either_sample_is_refused(self):
        for sample_index in (0, 1):
            for offline_index in (0, 1):
                for repeated in (False, True):
                    with self.subTest(sample=sample_index, offline=offline_index, repeated=repeated):
                        config, proof = synthetic_update_inputs()
                        probes = proof["samples"][sample_index]["declared_offline"]
                        if repeated:
                            probes[offline_index] = copy.deepcopy(probes[1 - offline_index])
                        else:
                            probes.pop(offline_index)
                        with self.assertRaisesRegex(validator.GateError, "exactly once|repeated site"):
                            check_quorum(config, proof)
                        with self.assertRaisesRegex(RuntimeError, "exactly once|repeated site"):
                            assembler_proof(config, proof, sites=("eee", "fff"))

    def test_either_offline_validator_answering_or_captured_live_is_refused(self):
        for offline_index in (0, 1):
            for sample_index in (0, 1):
                for status, binary in ((200, None), (404, None), (502, CANDIDATE), (503, "a" * 64)):
                    with self.subTest(offline=offline_index, sample=sample_index, status=status, binary=binary):
                        config, proof = synthetic_update_inputs()
                        proof["samples"][sample_index]["declared_offline"][offline_index].update(
                            http_status=status, binary_sha256=binary)
                        with self.assertRaisesRegex(validator.GateError, "answer"):
                            check_quorum(config, proof)
                        with self.assertRaisesRegex(RuntimeError, "answer"):
                            assembler_proof(config, proof, sites=("eee", "fff"))
            for location in ("sample", "common"):
                config, proof = synthetic_update_inputs()
                rows = proof["samples"][0]["nodes"] if location == "sample" else proof["common_block"]["hosts"]
                rows.append(dict(config["declared_offline"][offline_index]))
                with self.subTest(offline=offline_index, location=location):
                    with self.assertRaisesRegex(validator.GateError, "answered the capture"):
                        check_quorum(config, proof)

    def test_two_offline_confirmations_must_match_exactly(self):
        config, proof = synthetic_update_inputs()
        for sites in ((), ("eee",), ("eee", "eee"), ("eee", "ddd"), ("eee", "fff", "aaa")):
            with self.subTest(sites=sites), self.assertRaisesRegex(mod.ProfileError, "explicit --declared-offline"):
                assembler_proof(config, proof, sites=sites)

    def assemble_synthetic(self, config, proof):
        """Exercise real assembly/certificate bindings with synthetic artifact and CLI outputs."""
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            config_path, proof_path = root / "config.json", root / "proof.json"
            config_path.write_text(json.dumps(config))
            proof_path.write_text(json.dumps(proof))
            approved = root / "approved.hash"
            approved.write_text("7" * 64)
            selection, api, selector = (root / name for name in ("selection.json", "api.json", "selector.py"))
            for path in (selection, api, selector):
                path.write_text("synthetic")
            artifact = {"binary_path": root / "arc-node", "genesis_path": root / "genesis.toml",
                        "binary_sha256": CANDIDATE, "genesis_sha256": config["genesis_file_sha256"],
                        "platform": "linux-x86_64", "artifact_id": 1, "artifact_digest": "sha256:" + "a" * 64}
            certificate = {"recovery_domain": config["recovery_domain"], "protocol_version": "3.0.0",
                           "validator_set_id": 1, "genesis_hash": "7" * 64, "checkpoint_sha256": "8" * 64,
                           "validators": [{"address": row["validator"], "stake": row["stake"]}
                                          for row in config["hosts"] + config["declared_offline"]]}
            args = Namespace(repository="FerrumVir/arc-chain", tag="v1.0.0", commit="c" * 40, main_sha="c" * 40,
                             run_id=1, run_attempt=1, recovery_epoch=1, repository_root=root,
                             artifact_platform="linux-x86_64", verifier_platform="linux-x86_64",
                             host_config=config_path, quorum_proof=proof_path, checkpoint=root / "checkpoint",
                             approved_genesis_network_hash=approved, rollout_validator=validator_path,
                             selection=selection, api_artifacts=api, selector=selector,
                             declared_offline=[row["site"] for row in config["declared_offline"]],
                             proof_validation_time=1080, attestation_generated_at=1081)
            with mock.patch.object(mod, "validate_main_pin"), \
                 mock.patch.object(mod, "select_and_materialize",
                                   return_value=(None, {"linux-x86_64": artifact}, {"sha256": "f" * 64})), \
                 mock.patch.object(mod, "verify_checkpoint", return_value=certificate):
                return mod.assemble(args)

    def test_zero_margin_and_warning_in_assembly_cli_and_handoff(self):
        cases = (((11, 10, 10, 10, 10, 10), 2, True),
                 ((12, 11, 10, 10, 9, 9), 2, True),
                 ((15, 14, 14, 14, 2, 2), 2, False),
                 # Removing a smallest live stake leaves exactly quorum: still a margin.
                 ((14, 14, 13, 13, 4, 3), 2, False),
                 # Losing only the largest live validator breaks quorum: not "any single".
                 ((30, 9, 9, 9, 2, 2), 2, False),
                 ((11, 10, 10, 10, 10, 10), 0, False))
        workflow = (ROOT.parents[1] / ".github/workflows/existing-chain-update-handoff.yml").read_text()
        summary_script = workflow.split("      - name: Record the immutable handoff tuple", 1)[1].split("        run: |\n", 1)[1]
        for stakes, count, zero_margin in cases:
            with self.subTest(stakes=stakes, count=count):
                config, proof = synthetic_update_inputs(stakes, count)
                self.assertIs(check_quorum(config, proof)["zero_margin"], zero_margin)
                value = self.assemble_synthetic(config, proof)
                self.assertIs(value[mod.attested_proof_key(value)]["zero_margin"], zero_margin)
                self.assertEqual(value.get("warnings", []), [mod.ZERO_MARGIN_WARNING] if zero_margin else [])
                self.assertEqual(value.get("declared_offline_validators", []), config["declared_offline"])
                with tempfile.TemporaryDirectory() as td:
                    root = Path(td)
                    output = root / "update-handoff/arc-existing-chain-update-attestation.json"
                    argv = ["assemble-existing-chain-update.py"]
                    for option, argument in {"repository": "FerrumVir/arc-chain", "repository-root": td,
                            "tag": "v1.0.0", "commit": "c" * 40, "main-sha": "c" * 40, "run-id": "1",
                            "run-attempt": "1", "version": "1.0.0", "selection": "unused", "api-artifacts": "unused",
                            "downloads-root": "unused", "selector": "unused", "materializer": "unused",
                            "host-config": "unused", "quorum-proof": "unused", "artifact-platform": "linux-x86_64",
                            "verifier-platform": "linux-x86_64", "checkpoint": "unused", "recovery-epoch": "1",
                            "approved-genesis-network-hash": "unused", "output": str(output)}.items():
                        argv.extend(("--" + option, argument))
                    stdout, stderr = io.StringIO(), io.StringIO()
                    with mock.patch.object(mod, "assemble", return_value=value), mock.patch.object(mod.sys, "argv", argv), \
                         contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                        self.assertEqual(mod.main(), 0)
                    self.assertTrue(json.loads(stdout.getvalue())["pass"])
                    self.assertEqual(stderr.getvalue().splitlines(), [mod.ZERO_MARGIN_WARNING] if zero_margin else [])
                    self.assertEqual(json.loads(output.read_text()), value)
                    env = dict(os.environ, ARTIFACT_ID="1", ARTIFACT_DIGEST="a" * 64, GITHUB_SHA="c" * 40,
                               GITHUB_RUN_ID="1", GITHUB_RUN_ATTEMPT="1", DECLARED_OFFLINE_SITE="eee,fff",
                               GITHUB_STEP_SUMMARY=str(root / "summary"))
                    result = subprocess.run(["bash", "-c", summary_script], cwd=root, env=env, capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual((root / "summary").read_text().splitlines().count(mod.ZERO_MARGIN_WARNING), int(zero_margin))

    def test_assembly_requires_sorted_site_order(self):
        config, proof = synthetic_update_inputs()
        config["declared_offline"].reverse()
        with self.assertRaisesRegex(mod.ProfileError, "sorted by site"):
            self.assemble_synthetic(config, proof)

    def test_owner_reverification_refuses_a_different_or_reordered_offline_list(self):
        config, proof = synthetic_update_inputs()
        owner = self.assemble_synthetic(config, proof)
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            path, selection = root / "owner.json", root / "selection.json"
            selection.write_text(json.dumps({"schema": "arc.pretag.selection.v1", "repository": "FerrumVir/arc-chain",
                "commit": "c" * 40, "run_id": 1, "run_attempt": 1,
                "artifacts": {"linux-x86_64": {"headless": {"id": 1, "digest": "sha256:" + "a" * 64}}}}))
            args = Namespace(rollout_validator=validator_path, commit="c" * 40, run_id=1, run_attempt=1, selection=selection)
            for mutation in (None, "reverse", "stake", "missing"):
                supplied = copy.deepcopy(owner)
                if mutation == "reverse":
                    supplied["declared_offline_validators"].reverse()
                elif mutation == "stake":
                    supplied["declared_offline_validators"][1]["stake"] += 1
                elif mutation == "missing":
                    supplied["declared_offline_validators"].pop()
                path.write_text(json.dumps(supplied))
                with self.subTest(mutation=mutation), mock.patch.object(mod, "assemble", return_value=owner), \
                     mock.patch.object(mod, "verify_owner_verifier"):
                    if mutation is None:
                        self.assertEqual(mod.validate_owner_attestation(args, path, now=1100)["declared_offline_validators"],
                                         config["declared_offline"])
                    else:
                        with self.assertRaisesRegex(mod.ProfileError, "differs from independently reverified"):
                            mod.validate_owner_attestation(args, path, now=1100)

    def test_workflow_accepts_only_empty_one_or_two_distinct_sites_and_forwards_each(self):
        workflow = (ROOT.parents[1] / ".github/workflows/existing-chain-update-handoff.yml").read_text()
        validation = workflow.split('          [[ "$INPUT_COMMIT" =~ ^[0-9a-f]{40}$ ]]\n', 1)[1].split('          [ "$GITHUB_EVENT_NAME"', 1)[0]
        forwarding = workflow.split('          declared_offline=()\n', 1)[1].split('          mkdir update-handoff', 1)[0]
        script = 'set -Eeuo pipefail\n' + validation + '\ndeclared_offline=()\n' + forwarding
        script += '\nif ((${#declared_offline[@]})); then printf "%s\\n" "${declared_offline[@]}"; fi\n'
        for value in ("", "eee", "eee,fff", "fff,eee", "eee,eee", "eee,fff,aaa", "ee", "EEE", "eee,", ",eee", "eee, fff", "eee\n"):
            with self.subTest(value=value):
                result = subprocess.run(["bash", "-c", script], env=dict(os.environ, DECLARED_OFFLINE_SITE=value),
                                        capture_output=True, text=True)
                if value in ("", "eee", "eee,fff", "fff,eee"):
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout.splitlines(),
                                     [arg for site in value.split(",") if site for arg in ("--declared-offline", site)])
                else:
                    self.assertEqual(result.returncode, 1)
                    self.assertEqual(result.stdout, "::error::declared_offline_site must be empty, one three-letter site, "
                                     "or two distinct three-letter sites separated by a comma.\n")

    def test_threshold_equals_node_rule_through_u64_max(self):
        # Exact rule in crates/arc-types/src/lib.rs; cover every power-of-two
        # boundary and its neighbours, including floating-point precision limits.
        totals = set(range(10_001))
        totals.update(2**bits + offset for bits in range(1, 65) for offset in (-1, 0, 1)
                      if 0 <= 2**bits + offset <= 2**64 - 1)
        totals.update((2**64 - 3, 2**64 - 2, 2**64 - 1))
        for total in sorted(totals):
            with self.subTest(total=total):
                node = 1 if total == 0 else total - (total - 1) // 3
                self.assertEqual(validator.strict_supermajority_threshold(total), node)
                self.assertEqual(node, (2 * total) // 3 + 1)
                if total >= 6:
                    config, _ = synthetic_update_inputs((total - 5, 1, 1, 1, 1, 1), offline_count=0)
                    _, checked_total, checked_quorum = validator.validate_host_config(config)
                    self.assertEqual((checked_total, checked_quorum), (total, node))
        config, _ = synthetic_update_inputs((0, 0, 0, 0, 0, 0), offline_count=0)
        with self.assertRaisesRegex(validator.GateError, "invalid total validator stake"):
            validator.validate_host_config(config)

    def test_exactly_two_thirds_live_stake_is_refused(self):
        config, proof = synthetic_update_inputs((10, 10, 10, 10, 10, 10))
        self.assertEqual(config["total_stake"], 60)
        self.assertEqual(config["quorum_stake"], 41)
        self.assertEqual(sum(host["stake"] for host in config["hosts"]), 40)
        with self.assertRaisesRegex(validator.GateError, "cannot meet the stake quorum"):
            check_quorum(config, proof)
        with self.assertRaisesRegex(RuntimeError, "cannot meet the stake quorum"):
            assembler_proof(config, proof, sites=("eee", "fff"))
        # Supplying the old rounded-up threshold cannot authorize the update.
        config["quorum_stake"] = proof["quorum_stake"] = 40
        with self.assertRaisesRegex(validator.GateError, "strict supermajority threshold"):
            check_quorum(config, proof)
        with self.assertRaisesRegex(RuntimeError, "strict supermajority threshold"):
            assembler_proof(config, proof, sites=("eee", "fff"))

    def test_divisible_total_at_strict_threshold_passes(self):
        config, proof = synthetic_update_inputs((11, 10, 10, 10, 10, 9))
        self.assertEqual(config["total_stake"], 60)
        summary = check_quorum(config, proof)
        self.assertEqual(summary["online_stake"], 41)
        self.assertEqual(summary["online_stake"], summary["quorum_stake"])
        self.assertTrue(summary["zero_margin"])
        assembled, _ = assembler_proof(config, proof, sites=("eee", "fff"))
        self.assertEqual(assembled["online_stake"], assembled["quorum_stake"])



if __name__ == "__main__":
    unittest.main()
