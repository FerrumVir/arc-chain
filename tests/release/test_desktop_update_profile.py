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


class ProfileBoundaryTests(unittest.TestCase):
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
