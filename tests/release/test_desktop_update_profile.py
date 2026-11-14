#!/usr/bin/env python3
"""Desktop release baseline authentication and unchanged quorum contracts."""
import copy
import hashlib
import importlib.util
import json
from argparse import Namespace
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


previous = load("previous", ROOT / "scripts/release/previous-validator-release.py")
existing = load("existing_tests", ROOT / "scripts/release/tests/test_existing_chain_update.py")
assembler = existing.mod
handoff = load("manifest_handoff", ROOT / "scripts/release/release-manifest-handoff.py")
acceptance = load("published_acceptance", ROOT / "scripts/release/published-artifact-acceptance.py")
materializer = load("update_materializer", ROOT / "scripts/release/materialize-update-handoff.py")


def release(tag="v1.0.0", **changes):
    value = dict(id=1, tag_name=tag, name="ARC Chain " + tag, target_commitish="a" * 40,
                 draft=False, prerelease=False, immutable=True, author={"login": "github-actions[bot]"})
    value.update(changes)
    return value


class PreviousReleaseTests(unittest.TestCase):
    def test_newest_qualifying_semver_not_global_latest_or_api_order(self):
        rows = [release("v0.9.0"), release("v1.0.0"), release("v9.0.0", immutable=False)]
        self.assertEqual(previous.select_release(rows, "v1.0.0", "v1.0.1")["tag_name"], "v1.0.0")
        with self.assertRaisesRegex(ValueError, "newest"):
            previous.select_release(rows, "v0.9.0", "v1.0.1")
        for mutation in ({"immutable": False}, {"author": {"login": "other"}},
                         {"name": "other"}, {"draft": True}, {"prerelease": True}):
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                previous.select_release([release(**mutation)], "v1.0.0", "v1.0.1")
        for candidate in ("v1.0.0", "v0.9.0"):
            with self.assertRaisesRegex(ValueError, "precede"):
                previous.select_release([release()], "v1.0.0", candidate)
        with self.assertRaisesRegex(ValueError, "newest"):
            previous.select_release([release(), release(id=2)], "v1.0.0", "v1.0.1")

    def test_real_namespaced_signature_and_published_hash(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            key = root / "ephemeral-test-key"
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)], check=True,
                           capture_output=True)
            public_key = key.with_suffix(".pub").read_text().strip()
            manifest = ("# ARC release manifest v1\n# repository=FerrumVir/arc-chain\n# tag=v1.0.0\n"
                        + "# commit=" + "a" * 40 + "\n" + "b" * 64 + "  arc-node-linux-x86_64\n").encode()
            path = root / "manifest"
            path.write_bytes(manifest)
            subprocess.run(["ssh-keygen", "-Y", "sign", "-f", str(key), "-n", previous.NAMESPACE,
                            str(path)], check=True, capture_output=True)
            signature = path.with_suffix(".sig").read_bytes()
            r = release()
            r["assets"] = [dict(name=name, id=i + 1, state="uploaded", size=len(payload),
                digest="sha256:" + digest, uploader={"login": "github-actions[bot]"},
                browser_download_url=f"https://github.com/FerrumVir/arc-chain/releases/download/v1.0.0/{name}")
                for i, (name, payload, digest) in enumerate([
                    ("SHA256SUMS", manifest, hashlib.sha256(manifest).hexdigest()),
                    ("SHA256SUMS.sig", signature, hashlib.sha256(signature).hexdigest()),
                    ("arc-node-linux-x86_64", b"binary", "b" * 64)])]
            with mock.patch.object(previous, "PUBLIC_KEY", public_key):
                self.assertEqual(previous.verify_manifest(r, manifest, signature)["binary_sha256"], "b" * 64)
                # Update the API digest too: the cryptographic check must still reject forgery.
                tampered = manifest.replace(b"b" * 64, b"c" * 64)
                forged = copy.deepcopy(r)
                forged["assets"][0]["digest"] = "sha256:" + hashlib.sha256(tampered).hexdigest()
                with self.assertRaisesRegex(ValueError, "signature"):
                    previous.verify_manifest(forged, tampered, signature)
                with mock.patch.object(previous, "NAMESPACE", "wrong-namespace"):
                    with self.assertRaisesRegex(ValueError, "signature"):
                        previous.verify_manifest(r, manifest, signature)
                bad = copy.deepcopy(r)
                bad["assets"][2]["digest"] = "sha256:" + "c" * 64
                with self.assertRaisesRegex(ValueError, "node hash"):
                    previous.verify_manifest(bad, manifest, signature)
            with self.assertRaisesRegex(ValueError, "signature"):
                previous.verify_manifest(r, manifest, signature)

    def test_two_hour_bound_and_reselection(self):
        prior = {"tag": "v1.0.0"}
        value = dict(profile=previous.PROFILE, validators_release=prior,
                     node_compatible_with_validators=True, generated_at_unix=1000)
        with mock.patch.object(previous, "authenticate", return_value=prior):
            previous.verify_attestation(value, "v1.0.1", 8200)
            for now in (8200.1, 999):
                with self.assertRaisesRegex(ValueError, "stale or future"):
                    previous.verify_attestation(value, "v1.0.1", now)
        with mock.patch.object(previous, "authenticate", return_value={"tag": "v1.0.1"}):
            with self.assertRaisesRegex(ValueError, "differs"):
                previous.verify_attestation(value, "v1.0.1", 1010)


class DesktopAssemblyTests(unittest.TestCase):
    def assemble(self, config, proof):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, value in (("config.json", config), ("proof.json", proof)):
                (root / name).write_text(json.dumps(value))
            (root / "approved.hash").write_text("7" * 64)
            for name in ("selection.json", "api.json", "selector.py"):
                (root / name).write_text("fixture")
            artifact = dict(binary_path=root / "arc-node", genesis_path=root / "genesis.toml",
                binary_sha256="d" * 64, genesis_sha256=config["genesis_file_sha256"],
                platform="linux-x86_64", artifact_id=1, artifact_digest="sha256:" + "e" * 64)
            certificate = dict(recovery_domain=config["recovery_domain"], protocol_version="3.0.0",
                validator_set_id=1, genesis_hash="7" * 64, checkpoint_sha256="8" * 64,
                validators=[dict(address=r["validator"], stake=r["stake"])
                            for r in config["hosts"] + config["declared_offline"]])
            args = Namespace(repository="FerrumVir/arc-chain", tag="v1.0.1", commit="c" * 40,
                main_sha="c" * 40, run_id=1, run_attempt=1, recovery_epoch=1, repository_root=root,
                artifact_platform="linux-x86_64", verifier_platform="linux-x86_64",
                host_config=root / "config.json", quorum_proof=root / "proof.json", checkpoint=root / "checkpoint",
                approved_genesis_network_hash=root / "approved.hash", rollout_validator=existing.validator_path,
                selection=root / "selection.json", api_artifacts=root / "api.json", selector=root / "selector.py",
                declared_offline=[r["site"] for r in config["declared_offline"]],
                proof_validation_time=1080, attestation_generated_at=1081, profile=previous.PROFILE,
                previous_release_tag="v1.0.0", confirm_node_compatible=True)
            original_loader = assembler.load_module
            def loader(path, name):
                if name == "previous_validator_release":
                    return previous
                return original_loader(path, name)
            with mock.patch.object(assembler, "validate_main_pin"), \
                 mock.patch.object(assembler, "select_and_materialize", return_value=(None,
                                   {"linux-x86_64": artifact}, {"sha256": "f" * 64})), \
                 mock.patch.object(assembler, "verify_checkpoint", return_value=certificate), \
                 mock.patch.object(assembler, "load_module", side_effect=loader), \
                 mock.patch.object(previous, "authenticate", return_value={"tag": "v1.0.0",
                                   "binary_sha256": existing.CANDIDATE}):
                return assembler.assemble(args)

    def test_all_live_hosts_run_previous_not_candidate(self):
        for offline in (0, 1, 2):
            config, proof = existing.synthetic_update_inputs(offline_count=offline)
            value = self.assemble(config, proof)
            self.assertEqual(value["profile"], previous.PROFILE)
            self.assertNotEqual(value["release"]["production_artifact"]["binary_sha256"],
                                value["validators_release"]["binary_sha256"])
            self.assertTrue(value["node_compatible_with_validators"])

    def test_each_wrong_live_binary_fails(self):
        for sample in (0, 1):
            for host in range(4):
                config, proof = existing.synthetic_update_inputs()
                proof["samples"][sample]["nodes"][host]["binary_sha256"] = "d" * 64
                with self.subTest(sample=sample, host=host), self.assertRaises(RuntimeError):
                    self.assemble(config, proof)

    def test_wrong_config_three_offline_and_stale_capture_fail(self):
        config, proof = existing.synthetic_update_inputs()
        config["hosts"][0]["baseline_binary_sha256"] = "d" * 64
        with self.assertRaises(RuntimeError):
            self.assemble(config, proof)
        config, proof = existing.synthetic_update_inputs((20, 20, 18, 1, 1, 1), offline_count=3)
        with self.assertRaises(RuntimeError):
            self.assemble(config, proof)
        config, proof = existing.synthetic_update_inputs()
        proof["captured_at_unix"] = 1
        with self.assertRaises(RuntimeError):
            self.assemble(config, proof)

    def test_distinct_signed_asset_contracts(self):
        files = set(handoff.expected_files(True, previous.PROFILE))
        self.assertEqual(len(files), 30)
        self.assertIn(previous.ASSET, files)
        self.assertNotIn("arc-existing-chain-update-attestation.json", files)
        self.assertEqual(acceptance.release_profile_for_assets(files), previous.PROFILE)
        with self.assertRaises(acceptance.AcceptanceError):
            acceptance.release_profile_for_assets(files | {"arc-existing-chain-update-attestation.json"})


class PracticeSuffixTests(unittest.TestCase):
    SUFFIXES = (
        ["--community-twin-execution", "--community-demand-pump", "--community-demand-dry-run"],
        ["--community-twin-execution", "--community-demand-pump"],
    )

    def fixture(self, suffix, native=True):
        config, proof = existing.synthetic_update_inputs()
        host = config["hosts"][0]
        old_site = host["site"]
        host.update(site="lhr", hostname="arc-node-lhr", unit="arc-node-v3-lhr.service")
        config["quorum_sites"][0] = "lhr"
        proof["common_block"]["hosts"][0]["site"] = "lhr"
        pin = dict(argv_tail=list(existing.ExistingChainUpdateTests.NATIVE_TAIL),
                   files_sha256=dict(existing.ExistingChainUpdateTests.NATIVE_FILES),
                   context_commitment="d0" * 32)
        if native:
            for row in config["hosts"]:
                row["native"] = copy.deepcopy(pin)
        for sample in proof["samples"]:
            for row in sample["nodes"]:
                if row["site"] == old_site:
                    row.update(site="lhr", hostname="arc-node-lhr")
                if native:
                    row["argv_redacted"].extend(pin["argv_tail"])
                    row["native"] = copy.deepcopy(pin)
                    row["native_files_sha256"] = dict(pin["files_sha256"])
            sample["nodes"][0]["argv_redacted"].extend(suffix)
        host["argv_suffix"] = list(suffix)
        return config, proof

    def check(self, config, proof, profile=previous.PROFILE, direct=False):
        validator = existing.validator
        hosts = {row["site"]: row for row in config["hosts"]}
        total, quorum = config["total_stake"], config["quorum_stake"]
        if not direct:
            hosts, total, quorum = validator.validate_host_config(config, profile=profile)
        return validator.validate_quorum_proof(proof, hosts, total, quorum,
            profile=profile, expected_network=config, now=1080,
            declared_offline={row["site"]: row for row in config["declared_offline"]})

    def test_both_suffixes_with_and_without_native_pins_reach_assembler(self):
        for suffix in self.SUFFIXES:
            for native in (False, True):
                with self.subTest(suffix=suffix, native=native):
                    config, proof = self.fixture(suffix, native)
                    self.assertTrue(all("argv_suffix" not in h for h in config["hosts"][1:]))
                    self.assertEqual(self.check(config, proof)["host_count"], 4)
                    result = DesktopAssemblyTests().assemble(config, proof)
                    self.assertEqual(result["profile"], previous.PROFILE)
                    self.assertEqual(result[assembler.LIVE_HOST_PROOF_KEY]["host_count"], 4)

    def test_running_unpinned_practice_flags_fail_in_both_profiles(self):
        for profile in (previous.PROFILE, assembler.UPDATE_PROFILE):
            for native in (False, True):
                for suffix in self.SUFFIXES:
                    config, proof = self.fixture(suffix, native)
                    del config["hosts"][0]["argv_suffix"]
                    with self.subTest(profile=profile, native=native, suffix=suffix), \
                         self.assertRaisesRegex(existing.validator.GateError, "lhr"):
                        self.check(config, proof, profile)

    def test_missing_reordered_extended_and_prefixed_runtime_suffixes_fail(self):
        for native in (False, True):
            for suffix in self.SUFFIXES:
                for sample in (0, 1):
                    mutations = {
                        "absent": lambda argv: argv[:-len(suffix)],
                        "wrong order": lambda argv: argv[:-len(suffix)] + list(reversed(suffix)),
                        "extra": lambda argv: argv + ["--community-demand-pump"],
                        "unknown": lambda argv: argv + ["--community-demand-unknown"],
                        "unrelated": lambda argv: argv + ["--unexpected"],
                        "value": lambda argv: argv + ["true"],
                        "native in suffix": lambda argv: argv + ["--native-inference-runtime"],
                        "practice prefix": lambda argv: argv[:1] + ["--community-twin-execution"] + argv[1:],
                        "unknown practice prefix": lambda argv: argv[:1] + ["--community-demand-unknown=true"] + argv[1:],
                        "native prefix": lambda argv: argv[:1] + ["--native-inference-runtime"] + argv[1:],
                    }
                    for name, mutate in mutations.items():
                        config, proof = self.fixture(suffix, native)
                        row = proof["samples"][sample]["nodes"][0]
                        row["argv_redacted"] = mutate(row["argv_redacted"])
                        with self.subTest(native=native, suffix=suffix, sample=sample, mutation=name), \
                             self.assertRaisesRegex(existing.validator.GateError, "lhr"):
                            self.check(config, proof)

    def test_malformed_suffix_pins_fail_even_when_runtime_matches(self):
        invalid = [None, [], "--community-twin-execution", {},
                   list(reversed(self.SUFFIXES[1])), ["--community-demand-pump"],
                   self.SUFFIXES[1] + ["--community-demand-unknown"],
                   self.SUFFIXES[1] + ["--unexpected"],
                   self.SUFFIXES[1] + ["true"],
                   self.SUFFIXES[1] + ["--native-inference-runtime"],
                   self.SUFFIXES[1] + ["--community-demand-pump"],
                   ["--community-twin-execution=true", "--community-demand-pump"]]
        for suffix in invalid:
            for direct in (False, True):
                config, proof = self.fixture([])
                config["hosts"][0]["argv_suffix"] = suffix
                for sample in proof["samples"]:
                    sample["nodes"][0]["argv_redacted"].extend(suffix if isinstance(suffix, list) else [])
                with self.subTest(suffix=suffix, direct=direct), \
                     self.assertRaisesRegex(existing.validator.GateError, "lhr"):
                    self.check(config, proof, direct=direct)

    def test_suffix_pins_fail_under_validator_default_and_unknown_profiles(self):
        for profile in (assembler.UPDATE_PROFILE, "unknown", None):
            for direct in (False, True):
                config, proof = self.fixture(self.SUFFIXES[1])
                with self.subTest(profile=profile, direct=direct), \
                     self.assertRaisesRegex(existing.validator.GateError, "desktop update profile at lhr"):
                    self.check(config, proof, profile, direct=direct)
        with self.assertRaisesRegex(existing.validator.GateError, "lhr"):
            existing.validator.validate_host_config(config)
        with self.assertRaisesRegex(existing.validator.GateError, "lhr"):
            existing.validator.declared_offline_hosts(config)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "config.json").write_text(json.dumps(config))
            (root / "proof.json").write_text(json.dumps(proof))
            with self.assertRaisesRegex(RuntimeError, "lhr"):
                assembler.strict_proof(root / "proof.json", root / "config.json",
                    existing.validator_path, existing.CANDIDATE, now=1080,
                    declared_offline_sites=[row["site"] for row in config["declared_offline"]])

    def test_suffix_cannot_mask_native_hash_context_or_other_host_checks(self):
        mutations = {
            "binary": lambda row: row.update(binary_sha256="0" * 64),
            "identity": lambda row: row.update(validator="0" * 64),
            "genesis": lambda row: row.update(genesis_sha256="0" * 64),
            "data dir": lambda row: row["argv_redacted"].__setitem__(2, "/wrong"),
            "wire": lambda row: row["argv_redacted"].insert(1, "--legacy-v3-wire"),
            "native files": lambda row: row.update(native_files_sha256={}),
            "context": lambda row: row["native"].update(context_commitment="0" * 64),
            "stake": lambda row: row.update(active_stake=1),
            "height": lambda row: row.update(height=100),
        }
        for name, mutate in mutations.items():
            config, proof = self.fixture(self.SUFFIXES[1])
            mutate(proof["samples"][1]["nodes"][0])
            with self.subTest(mutation=name), self.assertRaisesRegex(existing.validator.GateError, "lhr"):
                self.check(config, proof)

    def test_unpinned_hosts_cannot_add_practice_flags(self):
        for index in (1, 2, 3):
            config, proof = self.fixture(self.SUFFIXES[1])
            row = proof["samples"][1]["nodes"][index]
            row["argv_redacted"].extend(self.SUFFIXES[1])
            with self.subTest(site=row["site"]), self.assertRaisesRegex(existing.validator.GateError, row["site"]):
                self.check(config, proof)


class ProfileBoundaryTests(unittest.TestCase):
    def test_platform_provenance_helpers_accept_only_supported_update_profiles(self):
        linux = load("appimage_fixtures", ROOT / "tests/release/test_packaged_appimage_live_gate.py").GATE
        mac = load("macos_fixtures", ROOT / "scripts/recovery/test_build_macos_package_provenance.py").gate
        for gate, names, validate, error in (
                (linux, (linux.APPIMAGE_NAME, linux.APPIMAGE_SIGNATURE_NAME, linux.NODE_NAME),
                 linux.validate_release_binding, linux.GateError),
                (mac, (mac.APP_ARCHIVE_NAME, mac.APP_SIGNATURE_NAME, mac.DMG_NAME),
                 mac.load_release_binding, mac.ProvenanceError)):
            with self.subTest(platform=gate.__name__), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                root.chmod(0o700)
                binding = dict(assets={name: dict(id=i, sha256=str(i) * 64, size=1)
                                      for i, name in enumerate(names, 1)},
                    commit="a" * 40, legacy_source={}, published_evidence={},
                    release=dict(id=1, immutable=True),
                    release_workflow=dict(event="workflow_dispatch", head_branch="main", head_sha="a" * 40,
                        id=1, jobs_sha256="b" * 64, path=".github/workflows/release.yml", run_attempt=1, run_id=1),
                    repository=gate.REPOSITORY, schema=gate.UPDATE_RELEASE_SCHEMA, tag=gate.TAG)
                for i, profile in enumerate((previous.PROFILE, handoff.PROFILE_EXISTING_UPDATE_V1,
                                             "cutover-v1", "unsupported")):
                    binding["profile"] = profile
                    path = root / f"binding-{i}.json"
                    path.write_bytes(gate.canonical_json(binding))
                    path.chmod(0o400)
                    if i < 2:
                        self.assertEqual(validate(path)[0]["profile"], profile)
                    else:
                        with self.assertRaises(error):
                            validate(path)


    def test_preflight_run_attempt_and_artifact_swap_rejected(self):
        artifact = dict(platform="linux-x86_64", artifact_id=42, artifact_digest="sha256:" + "a" * 64)
        value = {"release": dict(preflight_run_id=10, preflight_run_attempt=2,
                                production_artifact=artifact, checkpoint_verifier_artifact=artifact)}
        selection = {"linux-x86_64": {"headless": {"id": 42, "digest": artifact["artifact_digest"]}}}
        previous.verify_preflight_binding(value, 10, 2, selection)
        for run, attempt in ((11, 2), (10, 3)):
            with self.assertRaisesRegex(ValueError, "run/attempt"):
                previous.verify_preflight_binding(value, run, attempt, selection)
        for field, wrong in (("id", 43), ("digest", "sha256:" + "b" * 64)):
            changed = copy.deepcopy(selection)
            changed["linux-x86_64"]["headless"][field] = wrong
            with self.assertRaisesRegex(ValueError, "ID/digest"):
                previous.verify_preflight_binding(value, 10, 2, changed)

    def test_published_binding_accepts_desktop_and_refuses_profile_downgrade(self):
        fixtures = load("acceptance_fixtures", ROOT / "tests/release/test_published_artifact_acceptance.py")
        case = fixtures.PublishedArtifactAcceptanceTests()
        case.setUp()
        try:
            binding = json.loads(case.bind().read_text())
            binding["schema"] = "arc.published-release-binding.v2"
            binding["profile"] = previous.PROFILE
            for name in ("arc-legacy-maintenance-boundary.json", "arc-recovery-checkpoint-descriptor.json",
                         "arc-cutover-policy.json"):
                binding["assets"].pop(name)
            binding["assets"][previous.ASSET] = binding["assets"]["latest.json"]
            fixtures.acceptance.validate_binding(binding)
            binding["profile"] = handoff.PROFILE_EXISTING_UPDATE_V1
            with self.assertRaises(fixtures.acceptance.AcceptanceError):
                fixtures.acceptance.validate_binding(binding)
        finally:
            case.doCleanups()

    def test_materializer_requires_matching_explicit_profile(self):
        fixtures = load("materializer_fixtures", ROOT / "scripts/release/tests/test_materialize_update_handoff.py")
        case = fixtures.UpdateMaterializeTests()
        case.setUp()
        try:
            value = json.loads(case.payload)
            value["profile"] = previous.PROFILE
            case.payload = json.dumps(value).encode()
            args = case.package()
            with self.assertRaisesRegex(ValueError, "profile differs"):
                materializer.materialize(args)
            args.profile = previous.PROFILE
            materializer.materialize(args)
        finally:
            case.doCleanups()

    def test_unsigned_and_sealed_handoffs_reject_profile_swaps(self):
        fixtures = load("handoff_fixtures", ROOT / "tests/release/test_release_manifest_handoff.py")
        case = fixtures.ReleaseManifestHandoffTests()
        case.setUp()
        try:
            for sealed in (False, True):
                stage, output = case.stage("desktop-" + str(sealed), sealed=sealed, profile=previous.PROFILE)
                args = ["--handoff-dir", str(stage), "--expected-metadata-sha", output["metadata_sha"]]
                if sealed:
                    args.append("--sealed")
                case.invoke("verify", "--profile", previous.PROFILE, *args)
                case.invoke("verify", "--profile", handoff.PROFILE_EXISTING_UPDATE_V1, *args, succeeds=False)
                self.assertIn("arc-release-desktop-update-", output["artifact_name"])
        finally:
            case.tearDown()


if __name__ == "__main__":
    unittest.main()
