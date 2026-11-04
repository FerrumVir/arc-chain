"""Tests for scripts/ci/export_privacy.py. Synthetic data only: no real export, host or history is read."""
from __future__ import annotations

import importlib.util
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

    def test_unreadable_or_non_json_is_not_a_finding(self):
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, "broken.json").write_text("{not json", encoding="utf-8")
            Path(tmp, "notes.txt").write_text("Adas-MacBook-Pro.local", encoding="utf-8")
            self.assertEqual(run(tmp).returncode, 0)

    def test_missing_path_is_a_usage_error(self):
        self.assertEqual(run(str(FIXTURES / "does-not-exist")).returncode, 2)


class Tree(unittest.TestCase):
    def test_tracked_tree_scan_skips_only_the_fixture_dir(self):
        listed = ep.tracked_files(REPO)
        self.assertTrue(any(p.startswith(ep.FIXTURE_DIR) for p in listed), "fixtures are tracked")
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
