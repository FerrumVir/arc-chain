"""Tests for scripts/ci/export_privacy.py. Synthetic data only: no real export, host or history is read."""
from __future__ import annotations

import importlib.util
import json
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
FIXTURES = HERE / "fixtures" / "export-privacy"
SCRIPT = REPO / "scripts" / "ci" / "export_privacy.py"
spec = importlib.util.spec_from_file_location("export_privacy", SCRIPT)
ep = importlib.util.module_from_spec(spec)
assert spec.loader is not None
sys.modules["export_privacy"] = ep  # dataclasses resolve their module through sys.modules
spec.loader.exec_module(ep)


def run(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, str(SCRIPT), *args], capture_output=True, text=True)


class Rules(unittest.TestCase):
    # The node's own refusals (crates/arc-node/src/rpc.rs, community_nicknames_keep_safe_labels_and_refuse_the_rest
    # and community_public_name_redacts_legacy_hostname_registrations), restricted to the device-name cases.
    NODE_REFUSED_AS_DEVICE = [
        "Adas-MacBook-Pro", "adas-mbp", "Adas MacBook Air", "adas-imac", "adas mac mini",
        "DESKTOP-1A2B3C4", "LAPTOP-ABCD1234", "ADA-PC", "localhost", "rig.local",
    ]
    NODE_LEGACY = [
        "arc-1a2b3c4d (Adas-MacBook-Pro.local)", "arc-1a2b3c4d (DESKTOP-1A2B3C4)",
        "arc-1a2b3c4d (ada-ThinkPad-X1-Carbon)", "github-linux-x86_64-community-verification (fv-az123-456)",
        "LAX (vultr-lax-1)",
    ]
    NODE_ACCEPTED = ["basement rig 2", "A", "rig_01", "github-linux-x86_64-community-verification", "node-5e5e5e5e"]

    def test_device_names_the_node_refuses_are_flagged(self):
        for name in self.NODE_REFUSED_AS_DEVICE:
            self.assertIsNotNone(ep.personal_hostname_rule(name), name)

    def test_legacy_registrations_are_flagged_whatever_the_host(self):
        for name in self.NODE_LEGACY:
            self.assertEqual(ep.personal_hostname_rule(name), "legacy 'name (hostname)' registration", name)

    def test_safe_labels_pass(self):
        for name in self.NODE_ACCEPTED + ["arc-1a2b3c4d", "harbor-relay", "validator-nyc-01", "Macro economics desk"]:
            self.assertIsNone(ep.personal_hostname_rule(name), name)

    def test_lan_suffixes(self):
        for name in ["kitchen.lan", "Bens-Mac-mini.home", "box.localdomain", "printer.home.arpa", "rig.local."]:
            self.assertIsNotNone(ep.personal_hostname_rule(name), name)

    def test_prose_and_versions_pass(self):
        for text in ["0.8.16", "0.8.16+candle", "arc.workers.scoreboard.v1",
                     "Default Mac names such as <Name>s-MacBook-Pro are refused by the node; this is prose."]:
            self.assertIsNone(ep.personal_hostname_rule(text), text)

    def test_device_tokens_cover_the_node_list(self):
        # Drift guard: every token the node treats as a device word is one this check knows.
        source = (REPO / "crates" / "arc-node" / "src" / "rpc.rs").read_text(encoding="utf-8")
        block = re.search(r"const DEVICE_TOKENS: &\[&str\] = &\[(.*?)\];", source, re.S)
        self.assertIsNotNone(block, "DEVICE_TOKENS moved; update this guard and export_privacy.py together")
        node_tokens = set(re.findall(r'"([a-z]+)"', block.group(1)))
        self.assertTrue(node_tokens)
        self.assertLessEqual(node_tokens, ep.DEVICE_TOKENS)

    def test_path_rules(self):
        self.assertTrue(ep.path_findings("arc-state-export/2026-08-18T20-48-38Z/SUMMARY.md"))
        self.assertTrue(ep.path_findings("docs/evidence/LHR_workers_scoreboard.json"))
        self.assertEqual(ep.path_findings("scripts/arc-export-volatile-state.sh"), [])
        self.assertEqual(ep.path_findings("docs/NYC-notes.json"), [])


class Fixtures(unittest.TestCase):
    def test_hostname_bearing_export_fails(self):
        result = run(str(FIXTURES / "leaky-content"))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("/workers/0/name (name): mDNS/LAN host suffix", result.stderr)
        self.assertIn("/workers/1/name (name): operating-system default computer name", result.stderr)
        self.assertIn("/workers/2/name (name): legacy 'name (hostname)' registration", result.stderr)
        self.assertIn("/host/hostname (hostname): operating-system default computer name", result.stderr)
        self.assertNotIn("/workers/3/", result.stderr, "the hashed default label passes")
        # Findings never print the value they matched.
        for secret in ["Adas-MacBook-Pro", "DESKTOP-1A2B3C4", "Bens-Mac-mini", "CAROL-PC"]:
            self.assertNotIn(secret, result.stderr + result.stdout)

    def test_export_shaped_path_fails_even_when_content_is_clean(self):
        result = run(str(FIXTURES / "leaky-export"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("file under an exported-state root", result.stderr)
        self.assertIn("named like an exported seed endpoint", result.stderr)

    def test_sanitized_export_passes(self):
        result = run(str(FIXTURES / "clean"))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_non_json_files_are_not_read(self):
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, "notes.txt").write_text("Synth-MacBook-Pro.local", encoding="utf-8")
            self.assertEqual(run(tmp).returncode, 0)

    def test_missing_path_is_a_usage_error(self):
        self.assertEqual(run(str(FIXTURES / "does-not-exist")).returncode, 2)


class MarkerlessWorkers(unittest.TestCase):
    """workers[] supplies record context even without worker_id/address/etc."""

    def test_default_device_name_fails_without_echoing_values_or_private_keys(self):
        device = "SYNTHETIC-MacBook-Pro"
        private_key = "synthetic-private-account-key"
        record = {"name": device, "runs": 1}
        self.assertFalse(ep.WORKER_MARKERS.intersection(record))
        for wrapped in (False, True):
            with self.subTest(wrapped=wrapped), tempfile.TemporaryDirectory() as tmp:
                doc = {"workers": [record]}
                if wrapped:
                    doc = {private_key: doc}
                path = Path(tmp, "scoreboard.json")
                path.write_text(json.dumps(doc), encoding="utf-8")
                result = run(str(path))  # the real CLI, not just the name heuristic
                self.assertEqual(result.returncode, 1, result.stderr)
                pointer = ("/<key 0>" if wrapped else "") + "/workers/0/name"
                self.assertEqual(result.stderr,
                    f"export-privacy: {path}: {pointer} (name): operating-system default computer name\n"
                    "export-privacy: 1 finding(s). Do not commit exported node state; see scripts/ci/export_privacy.py.\n")
                self.assertEqual(result.stdout, "")
                for secret in (device, private_key):
                    self.assertNotIn(secret, result.stdout + result.stderr)

    def test_anonymized_name_passes_without_worker_markers(self):
        for wrapped in (False, True):
            with self.subTest(wrapped=wrapped), tempfile.TemporaryDirectory() as tmp:
                record = {"name": "node-1a2b3c4d", "runs": 1}
                self.assertFalse(ep.WORKER_MARKERS.intersection(record))
                doc = {"workers": [record]}
                if wrapped:
                    doc = {"synthetic-private-account-key": doc}
                path = Path(tmp, "scoreboard.json")
                path.write_text(json.dumps(doc), encoding="utf-8")
                result = run(str(path))
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stderr, "")
                self.assertEqual(result.stdout,
                    "export-privacy: no exported node state or personal device names in the checked files.\n")


class ReviewCorrections(unittest.TestCase):
    """Reviewer findings on eec3cccc: values printed through keys, unreadable JSON passing, and skip-list bypasses."""

    def test_hostname_used_as_a_key_is_found_and_never_printed(self):
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, "export.json").write_text(json.dumps(
                {"workers": {"Synth-MacBook-Pro.local": {"hostname": "DESKTOP-AB12CD3"}, "node-1a2b3c4d": {"runs": 1}}}), encoding="utf-8")
            result = run(tmp)
            self.assertEqual(result.returncode, 1)
            self.assertIn("/workers/<key 0> (key): mDNS/LAN host suffix", result.stderr)
            self.assertIn("/workers/<key 0>/hostname (hostname): operating-system default computer name", result.stderr)
            self.assertNotIn("/workers/<key 1>", result.stderr, "a hashed default key passes")
            for value in ["Synth-MacBook-Pro", "DESKTOP-AB12CD3"]:
                self.assertNotIn(value, result.stdout + result.stderr)

    def test_lists_of_hostnames_are_checked(self):
        found = ep.content_findings("x.json", json.dumps({"hostnames": ["validator-nyc-01", "Synth-iMac.lan"], "devices": ["SYNTH-PC"]}).encode())
        self.assertEqual([f.where for f in found], ["/hostnames/1 (item)", "/devices/0 (item)"])

    def test_truncated_json_is_a_finding(self):
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, "cut.json").write_text('{"workers":[{"worker_id":"w1","name":"seed (Synth-MacBook-Pro.local)"}, {"hostname": "Synth', encoding="utf-8")
            result = run(tmp)
            self.assertEqual(result.returncode, 1)
            self.assertIn("unreadable JSON", result.stderr)
            self.assertNotIn("Synth", result.stderr)

    def test_oversized_json_is_a_finding(self):
        with tempfile.TemporaryDirectory() as tmp:
            big = Path(tmp, "big.json")
            big.write_text(json.dumps({"hostname": "DESKTOP-AB12CD3", "pad": "x" * (ep.MAX_JSON_BYTES + 1024)}), encoding="utf-8")
            result = run(tmp)
            self.assertEqual(result.returncode, 1)
            self.assertIn("larger than 8 MiB was not scanned", result.stderr)

    def test_only_the_one_known_unparseable_file_is_allowed(self):
        allowed = next(iter(ep.UNPARSEABLE_ALLOWED))
        self.assertEqual(ep.content_findings(allowed, b""), [])
        self.assertTrue(ep.content_findings("docs/arc-chain-v2/another.json", b""))

    def test_no_directory_is_a_bypass(self):
        # The former substring skips (vendor/, target/, node_modules/) and the fixture-directory skip, in a throwaway repository.
        leaky = json.dumps({"workers": [{"worker_id": "w", "name": "Synth-MacBook-Pro"}]})
        with tempfile.TemporaryDirectory() as tmp:
            subprocess.run(["git", "init", "-q", tmp], check=True)
            files = {
                "arc-state-export/vendor/NYC_workers_scoreboard.json": json.dumps({"workers": []}),
                "docs/target/notes.json": leaky,
                "node_modules/pkg/data.json": leaky,
                "scripts/ci/tests/fixtures/export-privacy/extra/AMS_workers_scoreboard.json": json.dumps({"workers": []}),
            }
            for rel, body in files.items():
                Path(tmp, rel).parent.mkdir(parents=True, exist_ok=True)
                Path(tmp, rel).write_text(body, encoding="utf-8")
            subprocess.run(["git", "-C", tmp, "add", "-f", "."], check=True)
            flagged = {f.path for f in ep.scan_tree(Path(tmp))}
            self.assertEqual(flagged, set(files), "every planted file is found")


class SecondReview(unittest.TestCase):
    """Reviewer findings on 992e8a20: custom keys printed as schema, and exceptions trusted by path alone."""

    def test_custom_machine_names_as_keys_are_never_printed(self):
        for doc, custom in [
            ({"workers": {"synthhomebox": {"hostname": "DESKTOP-AB12CD3"}}}, "synthhomebox"),
            ({"data": {"synthalice": {"hostname": "Synth-MBP"}}}, "synthalice"),
        ]:
            with tempfile.TemporaryDirectory() as tmp:
                Path(tmp, "export.json").write_text(json.dumps(doc), encoding="utf-8")
                result = run(tmp)
                self.assertEqual(result.returncode, 1, doc)
                self.assertNotIn(custom, result.stdout + result.stderr, "an undetected custom name is not printed either")
                self.assertIn("/<key 0>/hostname (hostname)" if "data" in doc else "/workers/<key 0>/hostname (hostname)", result.stderr)

    def test_only_listed_keys_print(self):
        self.assertEqual(ep._segment("hostname", 3, False), "hostname")
        self.assertEqual(ep._segment("workers", 0, False), "workers")
        self.assertEqual(ep._segment("synthalice", 2, False), "<key 2>")
        self.assertEqual(ep._segment("hostname", 1, True), "<key 1>", "inside a host container every key is a machine name")

    def test_pins_match_the_committed_files(self):
        for rel, digest in {**ep.FIXTURE_FILES, **ep.UNPARSEABLE_ALLOWED}.items():
            self.assertEqual(ep.sha256((REPO / rel).read_bytes()), digest, rel)

    def test_changed_bytes_at_excepted_paths_are_scanned(self):
        allowed = next(iter(ep.UNPARSEABLE_ALLOWED))
        clean_fixture = "scripts/ci/tests/fixtures/export-privacy/clean/scoreboard.json"
        with tempfile.TemporaryDirectory() as tmp:
            subprocess.run(["git", "init", "-q", tmp], check=True)
            planted = {
                allowed: '{"workers":[{"worker_id":"w1","hostname":"DESKTOP-AB12CD3"}',
                clean_fixture: json.dumps({"workers": [{"worker_id": "w", "name": "Synth-MacBook-Pro"}]}),
            }
            for rel, body in planted.items():
                Path(tmp, rel).parent.mkdir(parents=True, exist_ok=True)
                Path(tmp, rel).write_text(body, encoding="utf-8")
            subprocess.run(["git", "-C", tmp, "add", "-f", "."], check=True)
            found = ep.scan_tree(Path(tmp))
            rules = {(f.path, f.rule) for f in found}
            self.assertIn((allowed, "unreadable JSON: it does not parse, so this check cannot vouch for it"), rules)
            self.assertIn((clean_fixture, "excepted fixture changed since review, so it is scanned like any other file"), rules)
            self.assertIn((clean_fixture, "operating-system default computer name"), rules)
        # The reviewed bytes themselves stay excepted.
        self.assertEqual(ep.content_findings(allowed, b""), [])


class Tree(unittest.TestCase):
    def test_tracked_tree_scan_skips_only_the_fixture_dir(self):
        listed = ep.tracked_files(REPO)
        self.assertTrue(set(ep.FIXTURE_FILES) <= set(listed), "the three fixtures are tracked")
        self.assertEqual(ep.scan_tree(REPO), [], "the committed tree is clean")

    def test_a_forced_add_of_an_export_would_fail(self):
        # Simulate the tree scan on a repository that tracks an export, without touching this one.
        with tempfile.TemporaryDirectory() as tmp:
            subprocess.run(["git", "init", "-q", tmp], check=True)
            export = Path(tmp, "arc-state-export", "2026-01-01T00-00-00Z")
            export.mkdir(parents=True)
            Path(export, "AMS_workers_scoreboard.json").write_text(
                (FIXTURES / "leaky-content" / "scoreboard.json").read_text(encoding="utf-8"), encoding="utf-8")
            subprocess.run(["git", "-C", tmp, "add", "-f", "."], check=True)
            findings = ep.scan_tree(Path(tmp))
            rules = {f.rule for f in findings}
            self.assertIn("file under an exported-state root (arc-state-export/)", rules)
            self.assertIn("operating-system default computer name", rules)


if __name__ == "__main__":
    unittest.main()
