import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "existing-chain-update-input.py"
SPEC = importlib.util.spec_from_file_location("update_input", SCRIPT)
mod = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(mod)


class UpdateInputTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        subprocess.run(["git", "init", "-q", str(self.repo)], check=True)
        subprocess.run(["git", "-C", str(self.repo), "-c", "user.name=Test", "-c",
                        "user.email=test@example.invalid", "-c", "commit.gpgsign=false",
                        "commit", "--allow-empty", "-qm", "main"], check=True)
        self.main = mod.git(self.repo, "rev-parse", "HEAD").decode().strip()
        self.inputs = self.root / "inputs"
        self.inputs.mkdir()
        values = {"approved-genesis.network-hash": b"a" * 64 + b"\n",
                  "host-config.json": b"{}\n", "quorum-proof.json": b"{}\n",
                  "recovery.arcchkpt": b"test-only opaque checkpoint"}
        sha = lambda name: hashlib.sha256(values[name]).hexdigest()
        attestation = {"schema": "arc.existing-recovered-chain-update/v1",
            "release": {"repository": "FerrumVir/arc-chain", "commit": self.main, "main_sha": self.main},
            "claim": {"existing_recovered_chain_update": True, "original_cutover_ceremony_verified": False,
                      "original_cutover_archive_present": False, "validator_retirement_authorized": False},
            "checkpoint": {"checkpoint_sha256": sha("recovery.arcchkpt"),
                           "checkpoint_size": len(values["recovery.arcchkpt"])},
            "fresh_six_host_proof": {"proof_sha256": sha("quorum-proof.json")},
            "network": {"host_config_sha256": sha("host-config.json"),
                        "approved_genesis_network_hash_file_sha256": sha("approved-genesis.network-hash")}}
        values["update-attestation.json"] = json.dumps(attestation).encode()
        for name, value in values.items():
            (self.inputs / name).write_bytes(value)

    def test_roundtrip_preserves_exact_files_without_changing_checkout_or_refs(self):
        before = mod.git(self.repo, "show-ref")
        commit = mod.create(self.repo, self.inputs, self.main)
        output = self.root / "out"
        mod.materialize(self.repo, commit, self.main, output)
        self.assertEqual(mod.git(self.repo, "show-ref"), before)
        self.assertEqual(mod.git(self.repo, "status", "--porcelain"), b"")
        for name in mod.FILES:
            self.assertEqual((output / name).read_bytes(), (self.inputs / name).read_bytes())
            self.assertEqual((output / name).stat().st_mode & 0o777, 0o444)

    def test_unbound_checkpoint_rejected_before_commit_creation(self):
        (self.inputs / "recovery.arcchkpt").write_bytes(b"swapped")
        with self.assertRaisesRegex(mod.InputError, "checkpoint differs"):
            mod.create(self.repo, self.inputs, self.main)

    def test_unbound_proof_rejected(self):
        (self.inputs / "quorum-proof.json").write_text('{"scope": "changed"}')
        with self.assertRaisesRegex(mod.InputError, "proof differs"):
            mod.create(self.repo, self.inputs, self.main)

    def test_extra_key_file_never_enters_public_input_tree(self):
        (self.inputs / "validator-key.json").write_text("test secret")
        with self.assertRaisesRegex(mod.InputError, "membership differs"):
            mod.create(self.repo, self.inputs, self.main)

    def test_credential_field_inside_allowed_filename_is_refused(self):
        (self.inputs / "host-config.json").write_text('{"nested": {"private_key": "test secret"}}')
        with self.assertRaisesRegex(mod.InputError, "credential field"):
            mod.create(self.repo, self.inputs, self.main)

    def test_host_config_substitution_is_refused(self):
        (self.inputs / "host-config.json").write_text('{"lax_access": "different host"}')
        with self.assertRaisesRegex(mod.InputError, "host configuration differs"):
            mod.create(self.repo, self.inputs, self.main)

    def test_encoded_credentials_and_unrecognized_extra_fields_are_refused(self):
        for field in ("ssh_private_key_b64", "bearer_token", "arbitrary_extra"):
            with self.subTest(field=field):
                with self.assertRaises(mod.InputError):
                    mod.validate_bytes("host-config.json", json.dumps({field: "c2VjcmV0"}).encode())
                with self.assertRaises(mod.InputError):
                    mod.validate_bytes("quorum-proof.json", json.dumps({
                        "samples": [{"nodes": [{field: "c2VjcmV0"}]}]}).encode())

    def test_real_public_config_and_proof_fields_are_accepted(self):
        fixtures = SCRIPT.parent / "tests" / "fixtures"
        for name, fixture in (("host-config.json", "host-config-six-current-20260927.json"),
                              ("quorum-proof.json", "six-host-before-cfd-20260927.json")):
            mod.validate_bytes(name, (fixtures / fixture).read_bytes())

    def test_native_pins_and_evidence_are_public_fields_with_closed_contracts(self):
        fixtures = SCRIPT.parent / "tests" / "fixtures"
        native = {"argv_tail": ["--native-inference-activation", "/etc/arc/native/activation.json"],
                  "files_sha256": {"/etc/arc/native/activation.json": "a" * 64},
                  "context_commitment": "d0" * 32}
        config = json.loads((fixtures / "host-config-six-current-20260927.json").read_text())
        proof = json.loads((fixtures / "six-host-before-cfd-20260927.json").read_text())
        for host in config["hosts"]:
            host["native"] = dict(native)
        for sample in proof["samples"]:
            for node in sample["nodes"]:
                node["native"] = dict(native)
                node["native_files_sha256"] = dict(native["files_sha256"])
        mod.validate_bytes("host-config.json", json.dumps(config).encode())
        mod.validate_bytes("quorum-proof.json", json.dumps(proof).encode())
        config["hosts"][0]["native"]["extra"] = 1
        with self.assertRaises(mod.InputError):
            mod.validate_bytes("host-config.json", json.dumps(config).encode())
        proof["samples"][0]["nodes"][0]["native"]["extra"] = 1
        with self.assertRaises(mod.InputError):
            mod.validate_bytes("quorum-proof.json", json.dumps(proof).encode())

    def test_symlink_and_wrong_parent_rejected(self):
        commit = mod.create(self.repo, self.inputs, self.main)
        with self.assertRaisesRegex(mod.InputError, "sole parent"):
            mod.read_commit(self.repo, commit, "0" * 40)
        path = self.inputs / "host-config.json"
        path.unlink()
        path.symlink_to(self.inputs / "quorum-proof.json")
        with self.assertRaisesRegex(mod.InputError, "unsafe input"):
            mod.create(self.repo, self.inputs, self.main)

    def test_update_cannot_claim_legacy_retirement(self):
        path = self.inputs / "update-attestation.json"
        value = json.loads(path.read_text())
        value["claim"]["validator_retirement_authorized"] = True
        path.write_text(json.dumps(value))
        with self.assertRaisesRegex(mod.InputError, "ceremony or retirement"):
            mod.create(self.repo, self.inputs, self.main)


if __name__ == "__main__":
    unittest.main()
