#!/usr/bin/env python3
"""Desktop release baseline authentication and unchanged quorum contracts."""
import copy
import hashlib
import importlib.util
import json
import re
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


class ConsecutiveDesktopUpdateTests(unittest.TestCase):
    """All API reads are fixtures; manifests use real ephemeral SSH signatures."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.key = self.root / "test-key"
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(self.key)],
                       check=True, capture_output=True)
        patch = mock.patch.object(previous, "PUBLIC_KEY", self.key.with_suffix(".pub").read_text().strip())
        patch.start()
        self.addCleanup(patch.stop)
        self.rows = []
        self.payloads = {}
        self.reads = []
        patch = mock.patch.object(previous, "gh", side_effect=self.api)
        patch.start()
        self.addCleanup(patch.stop)
        self.baseline = self.add_release("v0.8.19")
        self.desktop = self.add_release("v0.8.20", self.desktop_value())

    def desktop_value(self):
        return dict(profile=previous.PROFILE, node_compatible_with_validators=True,
                    validators_release=dict(tag="v0.8.19", binary_sha256="b" * 64))

    def api(self, *args):
        self.reads.append(args[-1])
        if args[-1].endswith("/releases?per_page=100"):
            # API order and page boundaries must not determine version order.
            return json.dumps([[r] for r in reversed(self.rows)]).encode()
        return self.payloads[int(args[-1].rsplit("/", 1)[1])]

    def add_release(self, tag, value=None, manifest_hash=None, omit_attestation=False,
                    namespace=None, raw_attestation=None):
        rid = len(self.rows) + 1
        r = release(tag, id=rid, target_commitish=f"{rid:040x}")
        body = raw_attestation if raw_attestation is not None else (
            json.dumps(value).encode() if value is not None else None)
        manifest = ("# ARC release manifest v1\n# repository=FerrumVir/arc-chain\n"
                    f"# tag={tag}\n# commit={r['target_commitish']}\n"
                    + "b" * 64 + "  arc-node-linux-x86_64\n").encode()
        if body is not None and not omit_attestation:
            digest = manifest_hash or hashlib.sha256(body).hexdigest()
            manifest += f"{digest}  {previous.ASSET}\n".encode()
        path = self.root / f"manifest-{rid}"
        path.write_bytes(manifest)
        subprocess.run(["ssh-keygen", "-Y", "sign", "-f", str(self.key),
                        "-n", namespace or previous.NAMESPACE, str(path)],
                       check=True, capture_output=True)
        signature = path.with_suffix(".sig").read_bytes()
        files = [("SHA256SUMS", manifest), ("SHA256SUMS.sig", signature),
                 ("arc-node-linux-x86_64", b"binary")]
        if body is not None:
            files.append((previous.ASSET, body))
        r["assets"] = []
        for index, (name, payload) in enumerate(files):
            aid = rid * 10 + index
            digest = "b" * 64 if name == "arc-node-linux-x86_64" else hashlib.sha256(payload).hexdigest()
            r["assets"].append(dict(name=name, id=aid, state="uploaded", size=len(payload),
                digest="sha256:" + digest, uploader={"login": "github-actions[bot]"},
                browser_download_url=f"https://github.com/FerrumVir/arc-chain/releases/download/{tag}/{name}"))
            self.payloads[aid] = payload
        self.rows.append(r)
        return r

    def asset(self, name=previous.ASSET, row=None):
        return next(a for a in (row or self.desktop)["assets"] if a["name"] == name)

    def select(self, candidate="v0.8.21", baseline="v0.8.19"):
        return previous.authenticate(baseline, candidate)

    def refused(self):
        with self.assertRaisesRegex(ValueError, previous.NOT_NEWEST):
            self.select()

    def replace_desktop(self, value, **kwargs):
        self.rows.pop()
        # Fresh filenames avoid any interactive ssh-keygen overwrite prompt.
        self.root = self.root / "replacement"
        self.root.mkdir()
        self.desktop = self.add_release("v0.8.20", value, **kwargs)

    def test_one_skipped_desktop_update(self):
        result = self.select()
        self.assertEqual(result["tag"], "v0.8.19")
        self.assertEqual(result["binary_sha256"], "b" * 64)
        self.assertEqual(result[previous.SKIPPED], [dict(tag="v0.8.20", release_id=2,
            commit=self.desktop["target_commitish"],
            attestation_sha256=hashlib.sha256(self.payloads[self.asset()["id"]]).hexdigest())])

    def test_two_skipped_updates_in_ascending_version_order(self):
        self.add_release("v0.8.21", self.desktop_value())
        result = self.select("v0.8.22")
        self.assertEqual([r["tag"] for r in result[previous.SKIPPED]], ["v0.8.20", "v0.8.21"])

    def test_newest_baseline_has_empty_list_and_original_evidence(self):
        self.rows.pop()
        result = self.select()
        self.assertEqual(result.pop(previous.SKIPPED), [])
        assets = {a["name"]: a for a in self.baseline["assets"]}
        expected = previous.verify_manifest(self.baseline,
            self.payloads[assets["SHA256SUMS"]["id"]], self.payloads[assets["SHA256SUMS.sig"]["id"]])
        self.assertEqual(result, expected)

    def test_missing_attestation_asset(self):
        self.desktop["assets"].pop()
        self.refused()

    def test_other_validator_tag(self):
        value = self.desktop_value()
        value["validators_release"]["tag"] = "v0.8.18"
        self.replace_desktop(value)
        self.refused()

    def test_other_validator_binary_hash(self):
        value = self.desktop_value()
        value["validators_release"]["binary_sha256"] = "c" * 64
        self.replace_desktop(value)
        self.refused()

    def test_wrong_profile(self):
        value = self.desktop_value()
        value["profile"] = assembler.UPDATE_PROFILE
        self.replace_desktop(value)
        self.refused()

    def test_compatibility_must_be_boolean_true(self):
        for compatible in (False, 1, "true", None):
            with self.subTest(compatible=compatible):
                value = self.desktop_value()
                value["node_compatible_with_validators"] = compatible
                self.replace_desktop(value)
                self.refused()

    def test_attestation_uploader_must_be_bot(self):
        self.asset()["uploader"]["login"] = "other"
        self.refused()

    def test_attestation_digest_mismatch(self):
        self.asset()["digest"] = "sha256:" + "0" * 64
        self.refused()

    def test_attestation_size_mismatch(self):
        self.asset()["size"] += 1
        self.refused()

    def test_missing_attestation_digest(self):
        del self.asset()["digest"]
        self.refused()

    def test_attestation_missing_from_manifest(self):
        self.replace_desktop(self.desktop_value(), omit_attestation=True)
        self.refused()

    def test_attestation_hash_differs_from_signed_manifest(self):
        self.replace_desktop(self.desktop_value(), manifest_hash="0" * 64)
        self.refused()

    def test_bad_manifest_signature_with_correct_api_digest(self):
        asset = self.asset("SHA256SUMS.sig")
        payload = b"forged manifest signature\n"
        asset["size"] = len(payload)
        self.payloads[asset["id"]] = payload
        asset["digest"] = "sha256:" + hashlib.sha256(payload).hexdigest()
        self.refused()

    def test_wrong_manifest_signature_namespace(self):
        self.replace_desktop(self.desktop_value(), namespace="wrong")
        self.refused()

    def test_oversize_attestation_is_refused_before_download(self):
        asset = self.asset()
        asset["size"] = previous.MAX_ATTESTATION_BYTES + 1
        self.refused()
        self.assertFalse(any(p.endswith(f"/assets/{asset['id']}") for p in self.reads))

    def test_actual_oversize_attestation_is_refused(self):
        asset = self.asset()
        self.payloads[asset["id"]] = b" " * (previous.MAX_ATTESTATION_BYTES + 1)
        self.refused()

    def test_non_object_attestation(self):
        self.replace_desktop([self.desktop_value()])
        self.refused()

    def test_signed_malformed_json_is_refused(self):
        self.replace_desktop(None, raw_attestation=b"{broken")
        self.refused()

    def test_signed_duplicate_json_keys_are_refused(self):
        body = json.dumps(self.desktop_value()).encode()
        body = body[:-1] + b', "node_compatible_with_validators": true}'
        self.replace_desktop(None, raw_attestation=body)
        self.refused()

    def test_baseline_must_precede_candidate(self):
        for candidate in ("v0.8.19", "v0.8.18"):
            with self.subTest(candidate=candidate), self.assertRaisesRegex(ValueError, "precede"):
                self.select(candidate)

    def test_baseline_must_qualify(self):
        for changes in ({"immutable": False}, {"draft": True}, {"prerelease": True},
                        {"author": {"login": "other"}}, {"name": "other"}):
            with self.subTest(changes=changes):
                original = copy.deepcopy(self.baseline)
                self.baseline.update(changes)
                self.refused()
                self.baseline.update(original)

    def test_candidate_or_later_qualifying_release_refuses_old_handoff(self):
        self.add_release("v0.8.21", self.desktop_value())
        self.refused()

    def test_duplicate_intervening_version_is_refused(self):
        self.rows.append(copy.deepcopy(self.desktop))
        self.refused()

    def test_unqualifying_newer_releases_are_ignored(self):
        self.rows.append(release("v9.0.0", immutable=False))
        self.rows.append(release("v10.0.0", name="Legacy channel"))
        self.assertEqual(len(self.select()[previous.SKIPPED]), 1)

    def test_skipped_release_identity_must_bind_id_and_commit(self):
        for changes in ({"id": 0}, {"id": True}, {"target_commitish": "main"}):
            with self.subTest(changes=changes):
                original = copy.deepcopy(self.desktop)
                self.desktop.update(changes)
                self.refused()
                self.desktop.update(original)

    def test_unavailable_read_refuses_selection(self):
        with mock.patch.object(previous, "gh", side_effect=ValueError("GitHub release read failed")):
            with self.assertRaisesRegex(ValueError, "read failed"):
                self.select()

    def test_rederived_skipped_list_must_match_every_field_and_order(self):
        self.add_release("v0.8.21", self.desktop_value())
        prior, skipped = previous.attestation_evidence("v0.8.19", "v0.8.22")
        value = dict(profile=previous.PROFILE, node_compatible_with_validators=True,
                     validators_release=prior, generated_at_unix=1000,
                     validators_release_skipped_desktop_updates=skipped)
        previous.verify_attestation(value, "v0.8.22", 1010)
        altered = [[], list(reversed(skipped)), skipped[:-1], None]
        for key, replacement in (("tag", "v0.8.18"), ("release_id", 999), ("release_id", 2.0),
                                 ("commit", "f" * 40), ("attestation_sha256", "f" * 64)):
            rows = copy.deepcopy(skipped)
            rows[0][key] = replacement
            altered.append(rows)
        for rows in altered:
            with self.subTest(rows=rows), self.assertRaisesRegex(ValueError, "skipped.*differs"):
                previous.verify_attestation({**value, previous.SKIPPED: rows}, "v0.8.22", 1010)
        del value[previous.SKIPPED]
        with self.assertRaisesRegex(ValueError, "skipped.*differs"):
            previous.verify_attestation(value, "v0.8.22", 1010)

    def test_new_desktop_release_invalidates_old_handoff(self):
        prior, skipped = previous.attestation_evidence("v0.8.19", "v0.8.22")
        value = dict(profile=previous.PROFILE, node_compatible_with_validators=True,
                     validators_release=prior, generated_at_unix=1000,
                     validators_release_skipped_desktop_updates=skipped)
        self.add_release("v0.8.21", self.desktop_value())
        with self.assertRaisesRegex(ValueError, "skipped.*differs"):
            previous.verify_attestation(value, "v0.8.22", 1010)


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

    def test_assembly_records_empty_or_authenticated_skip_list(self):
        config, proof = existing.synthetic_update_inputs()
        self.assertEqual(self.assemble(config, proof)[previous.SKIPPED], [])
        skipped = [dict(tag="v0.8.20", release_id=2, commit="a" * 40, attestation_sha256="c" * 64)]
        prior = dict(tag="v0.8.19", binary_sha256=existing.CANDIDATE)
        with mock.patch.object(previous, "attestation_evidence", return_value=(prior, skipped)):
            value = self.assemble(config, proof)
        self.assertEqual(value["validators_release"], prior)
        self.assertEqual(value[previous.SKIPPED], skipped)

    def test_owner_reverification_compares_skips_and_preserves_legacy_empty_list(self):
        config, proof = existing.synthetic_update_inputs()
        owner = self.assemble(config, proof)
        verifier = owner["release"]["checkpoint_verifier_artifact"]
        selection = {verifier["platform"]: {"headless": {
            "id": verifier["artifact_id"], "digest": verifier["artifact_digest"]}}}
        skip = dict(tag="v0.8.20", release_id=2, commit="a" * 40, attestation_sha256="b" * 64)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "selection.json").write_text("{}")
            args = Namespace(rollout_validator=existing.validator_path, selection=root / "selection.json",
                             commit="c" * 40, run_id=1, run_attempt=1)
            for old, new, accepted in (([], [], True), (None, [], True),
                                       ([skip], [skip], True), ([], [skip], False),
                                       (None, [skip], False), ([skip], [], False)):
                with self.subTest(old=old, new=new):
                    supplied, recomputed = copy.deepcopy(owner), copy.deepcopy(owner)
                    if old is None:
                        del supplied[previous.SKIPPED]
                    else:
                        supplied[previous.SKIPPED] = old
                    recomputed[previous.SKIPPED] = new
                    (root / "owner.json").write_text(json.dumps(supplied))
                    with mock.patch.object(assembler, "assemble", return_value=recomputed), \
                         mock.patch.object(assembler, "validate_selection", return_value=selection), \
                         mock.patch.object(assembler, "verify_owner_verifier"):
                        if accepted:
                            result = assembler.validate_owner_attestation(args, root / "owner.json", now=1100)
                            self.assertEqual(result[previous.SKIPPED], new)
                        else:
                            with self.assertRaisesRegex(assembler.ProfileError, "differs from independently"):
                                assembler.validate_owner_attestation(args, root / "owner.json", now=1100)

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


class DesktopWorkflowNotesTests(unittest.TestCase):
    def test_release_notes_and_handoff_name_actual_baseline_and_skips(self):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        expression = re.search(r"skipped_tags=.*?jq -er '([^']+)'", workflow).group(1)
        handoff_text = (ROOT / ".github/workflows/existing-chain-update-handoff.yml").read_text()
        summary = re.search(r"jq -r '(if \.profile == .*?else empty end)'", handoff_text, re.S).group(1)
        for tags in ([], ["v0.8.20"], ["v0.8.20", "v0.8.21"]):
            with self.subTest(tags=tags):
                value = dict(profile=previous.PROFILE,
                             validators_release=dict(tag="v0.8.19", binary_sha256="b" * 64),
                             validators_release_skipped_desktop_updates=[dict(tag=t) for t in tags])
                expected = ", ".join(tags) if tags else "none"
                result = subprocess.run(["jq", "-er", expression], input=json.dumps(value),
                                        text=True, capture_output=True, check=True)
                self.assertEqual(result.stdout.strip(), expected)
                rendered = subprocess.run(["jq", "-r", summary], input=json.dumps(value),
                                          text=True, capture_output=True, check=True).stdout
                self.assertIn("Validators remain on v0.8.19", rendered)
                self.assertIn("b" * 64, rendered)
                self.assertIn(f"Skipped desktop updates: {expected}.", rendered)
                value["profile"] = assembler.UPDATE_PROFILE
                rendered = subprocess.run(["jq", "-r", summary], input=json.dumps(value),
                                          text=True, capture_output=True, check=True).stdout
                self.assertEqual(rendered, "")
        self.assertIn("Skipped desktop updates: $skipped_tags.", workflow)


class PracticeSuffixTests(unittest.TestCase):
    SUFFIXES = (
        ["--community-twin-execution", "--community-demand-pump", "--community-demand-dry-run"],
        ["--community-twin-execution", "--community-demand-pump"],
        ["--community-twin-execution", "--community-demand-pump", "--community-demand-dry-run",
         "--community-demand-interval-secs", "30", "--community-demand-max-groups", "32",
         "--community-demand-reserve-workers", "2", "--community-demand-groups-per-hour", "720"],
        ["--community-twin-execution", "--community-demand-pump",
         "--community-demand-interval-secs", "30", "--community-demand-max-groups", "32",
         "--community-demand-reserve-workers", "2", "--community-demand-groups-per-hour", "720"],
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

    def test_all_four_suffixes_with_and_without_native_pins_reach_assembler(self):
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
                for suffix in self.SUFFIXES:
                    for native in (False, True):
                        config, proof = self.fixture(suffix, native)
                        with self.subTest(profile=profile, direct=direct, suffix=suffix, native=native), \
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

    def test_fanout_suffix_rejects_each_missing_malformed_or_unpinned_value(self):
        for suffix in self.SUFFIXES[2:]:
            start = suffix.index("--community-demand-interval-secs")
            invalid = [("reordered pairs", suffix[:start] + suffix[start + 2:] + suffix[start:start + 2])]
            for index in range(start, len(suffix), 2):
                flag, value = suffix[index:index + 2]
                invalid.extend([
                    (flag + " missing flag", suffix[:index] + suffix[index + 1:]),
                    (flag + " missing value", suffix[:index + 1] + suffix[index + 2:]),
                    (flag + " missing pair", suffix[:index] + suffix[index + 2:]),
                    (flag + " duplicated pair", suffix[:index] + suffix[index:index + 2] + suffix[index:]),
                    (flag + " duplicated flag", suffix[:index] + [flag] + suffix[index:]),
                    (flag + " duplicated value", suffix[:index + 1] + [value] + suffix[index + 1:]),
                    (flag + " equals form", suffix[:index] + [flag + "=" + value] + suffix[index + 2:]),
                    (flag + " unknown", suffix[:index] + [flag + "-unknown"] + suffix[index + 1:]),
                ])
                values = ["", "0", "1", "2", "30", "32", "720", "3600", "721", "-1",
                          "+" + value, "0" + value, value + ".0", "1e3", " " + value, value + " ",
                          "true", "--community-demand-unknown", int(value), True, None, {}, []]
                for bad in values:
                    if bad != value:
                        invalid.append((flag + " value " + repr(bad),
                                        suffix[:index + 1] + [bad] + suffix[index + 2:]))
            for name, bad_suffix in invalid:
                for native in (False, True):
                    # A matching observed tail cannot make a malformed owner pin valid.
                    for direct in (False, True):
                        config, proof = self.fixture(bad_suffix, native)
                        with self.subTest(mutation=name, suffix=suffix, native=native, direct=direct), \
                             self.assertRaisesRegex(existing.validator.GateError, "argv_suffix"):
                            self.check(config, proof, direct=direct)
                    # A valid pin also rejects a changed tail in either proof sample.
                    for sample in (0, 1):
                        config, proof = self.fixture(suffix, native)
                        row = proof["samples"][sample]["nodes"][0]
                        row["argv_redacted"] = row["argv_redacted"][:-len(suffix)] + bad_suffix
                        with self.subTest(mutation=name, suffix=suffix, native=native, sample=sample), \
                             self.assertRaisesRegex(existing.validator.GateError, "argv_suffix"):
                            self.check(config, proof)

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
        for suffix in self.SUFFIXES:
            for name, mutate in mutations.items():
                config, proof = self.fixture(suffix)
                mutate(proof["samples"][1]["nodes"][0])
                with self.subTest(suffix=suffix, mutation=name), self.assertRaisesRegex(existing.validator.GateError, "lhr"):
                    self.check(config, proof)

    def test_unpinned_hosts_cannot_add_practice_flags(self):
        for suffix in self.SUFFIXES:
            for index in (1, 2, 3):
                config, proof = self.fixture(suffix)
                row = proof["samples"][1]["nodes"][index]
                row["argv_redacted"].extend(suffix)
                with self.subTest(suffix=suffix, site=row["site"]), \
                     self.assertRaisesRegex(existing.validator.GateError, row["site"]):
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
