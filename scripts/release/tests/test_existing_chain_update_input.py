import copy
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
        self.attestation = copy.deepcopy(attestation)
        values["update-attestation.json"] = json.dumps(attestation).encode()
        for name, value in values.items():
            (self.inputs / name).write_bytes(value)

    def five_live_fixture(self):
        """The real six-host fixtures with ams declared offline and the other five live."""
        fixtures = SCRIPT.parent / "tests" / "fixtures"
        config = json.loads((fixtures / "host-config-six-current-20260927.json").read_text())
        proof = json.loads((fixtures / "six-host-before-cfd-20260927.json").read_text())
        offline = next(host for host in config["hosts"] if host["site"] == "ams")
        declared = {key: offline[key] for key in ("site", "ip", "validator", "stake")}
        config["hosts"] = [host for host in config["hosts"] if host["site"] != "ams"]
        config["quorum_sites"] = [site for site in config["quorum_sites"] if site != "ams"]
        config["unobserved_stake"] = offline["stake"]
        config["declared_offline"] = [declared]
        for sample in proof["samples"]:
            sample["nodes"] = [node for node in sample["nodes"] if node["site"] != "ams"]
            sample["declared_offline"] = [{"site": "ams", "ip": offline["ip"], "validator": offline["validator"],
                "health_url": "https://" + offline["ip"] + "/health", "http_status": 502, "binary_sha256": None}]
        proof["common_block"]["hosts"] = [row for row in proof["common_block"]["hosts"] if row["site"] != "ams"]
        return config, proof, declared

    def write_declared_inputs(self, mutate=None):
        """Owner inputs for a five-live update, bound the way the assembler binds them."""
        config, proof, declared = self.five_live_fixture()
        (self.inputs / "host-config.json").write_text(json.dumps(config))
        (self.inputs / "quorum-proof.json").write_text(json.dumps(proof))
        sha = lambda name: hashlib.sha256((self.inputs / name).read_bytes()).hexdigest()
        attestation = copy.deepcopy(self.attestation)
        summary = {"proof_sha256": sha("quorum-proof.json")}
        del attestation["fresh_six_host_proof"]
        attestation["fresh_live_host_proof"] = summary
        attestation["declared_offline_validators"] = [declared]
        attestation["network"]["host_config_sha256"] = sha("host-config.json")
        if mutate is not None:
            mutate(attestation, summary)
        (self.inputs / "update-attestation.json").write_text(json.dumps(attestation))

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

    def test_declared_offline_fields_have_closed_public_contracts(self):
        config, proof, _ = self.five_live_fixture()
        mod.validate_bytes("host-config.json", json.dumps(config).encode())
        mod.validate_bytes("quorum-proof.json", json.dumps(proof).encode())
        config_mutations = (
            lambda c: c["declared_offline"][0].update(reason="text"),
            lambda c: c.update(declared_offline=c["declared_offline"][0]),
            lambda c: c.update(declared_offline=["ams"]),
        )
        for index, mutate in enumerate(config_mutations):
            with self.subTest(config_mutation=index):
                c = copy.deepcopy(config)
                mutate(c)
                with self.assertRaises(mod.InputError):
                    mod.validate_bytes("host-config.json", json.dumps(c).encode())
        proof_mutations = (
            lambda p: p["samples"][0]["declared_offline"][0].update(note="text"),
            lambda p: p["samples"][0].update(declared_offline=p["samples"][0]["declared_offline"][0]),
        )
        for index, mutate in enumerate(proof_mutations):
            with self.subTest(proof_mutation=index):
                p = copy.deepcopy(proof)
                mutate(p)
                with self.assertRaises(mod.InputError):
                    mod.validate_bytes("quorum-proof.json", json.dumps(p).encode())

    def test_declared_offline_attestation_binds_the_host_config_declaration(self):
        self.write_declared_inputs()
        commit = mod.create(self.repo, self.inputs, self.main)
        payloads = mod.read_commit(self.repo, commit, self.main)
        self.assertEqual(json.loads(payloads["host-config.json"])["declared_offline"][0]["site"], "ams")
        cases = {
            "six-host proof key on a declared update": (
                lambda v, s: (v.pop("fresh_live_host_proof"), v.update(fresh_six_host_proof=s)), "proof differs"),
            "both proof keys": (lambda v, s: v.update(fresh_six_host_proof=s), "proof differs"),
            "attestation leaves the declaration out": (
                lambda v, s: (v.pop("declared_offline_validators"), v.pop("fresh_live_host_proof"),
                              v.update(fresh_six_host_proof=s)), "declared-offline validators differ"),
            "attestation names another validator": (
                lambda v, s: v["declared_offline_validators"][0].update(site="nrt"),
                "declared-offline validators differ"),
        }
        for label, (mutate, message) in cases.items():
            with self.subTest(label):
                self.write_declared_inputs(mutate)
                with self.assertRaisesRegex(mod.InputError, message):
                    mod.create(self.repo, self.inputs, self.main)

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


    def write_two_offline_inputs(self, *, reverse_config=False, mutate=None):
        rows = [{"site": "eee", "ip": "192.0.2.5", "validator": "5" * 64, "stake": 10},
                {"site": "fff", "ip": "192.0.2.6", "validator": "6" * 64, "stake": 10}]
        config = {"declared_offline": list(reversed(rows)) if reverse_config else rows}
        proof = {"samples": [{"declared_offline": [
            {**{key: row[key] for key in ("site", "ip", "validator")},
             "health_url": "https://" + row["ip"] + "/health", "http_status": 502, "binary_sha256": None}
            for row in rows]} for _ in range(2)]}
        (self.inputs / "host-config.json").write_text(json.dumps(config))
        (self.inputs / "quorum-proof.json").write_text(json.dumps(proof))
        attestation = copy.deepcopy(self.attestation)
        attestation["fresh_live_host_proof"] = attestation.pop("fresh_six_host_proof")
        attestation["fresh_live_host_proof"]["proof_sha256"] = hashlib.sha256((self.inputs / "quorum-proof.json").read_bytes()).hexdigest()
        attestation["network"]["host_config_sha256"] = hashlib.sha256((self.inputs / "host-config.json").read_bytes()).hexdigest()
        attestation["declared_offline_validators"] = copy.deepcopy(config["declared_offline"])
        if mutate:
            mutate(attestation["declared_offline_validators"])
        (self.inputs / "update-attestation.json").write_text(json.dumps(attestation))
        return config, proof

    def test_two_offline_input_roundtrip_preserves_sorted_rows_and_both_probes(self):
        config, proof = self.write_two_offline_inputs()
        commit = mod.create(self.repo, self.inputs, self.main)
        output = self.root / "two-offline-output"
        mod.materialize(self.repo, commit, self.main, output)
        self.assertEqual(json.loads((output / "host-config.json").read_text()), config)
        self.assertEqual(json.loads((output / "quorum-proof.json").read_text()), proof)
        attestation = json.loads((output / "update-attestation.json").read_text())
        self.assertEqual(attestation["declared_offline_validators"], config["declared_offline"])
        self.assertEqual([row["site"] for row in attestation["declared_offline_validators"]], ["eee", "fff"])

    def test_two_offline_attestation_list_must_match_exactly_in_site_order(self):
        mutations = {"reversed": lambda rows: rows.reverse(), "missing": lambda rows: rows.pop(),
                     "duplicate": lambda rows: rows.append(copy.deepcopy(rows[0]))}
        for field, value in (("site", "ggg"), ("ip", "192.0.2.7"), ("validator", "7" * 64), ("stake", 11)):
            mutations[field] = lambda rows, key=field, val=value: rows[1].update({key: val})
        for name, mutate in mutations.items():
            with self.subTest(mutation=name):
                self.write_two_offline_inputs(mutate=mutate)
                with self.assertRaisesRegex(mod.InputError, "declared-offline validators differ"):
                    mod.create(self.repo, self.inputs, self.main)
        self.write_two_offline_inputs(reverse_config=True)
        with self.assertRaisesRegex(mod.InputError, "sorted by site"):
            mod.create(self.repo, self.inputs, self.main)


if __name__ == "__main__":
    unittest.main()
