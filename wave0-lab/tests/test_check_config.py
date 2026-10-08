"""Tests for wave0-lab/check_config.py (THROWAWAY LAB FILE)."""
from __future__ import annotations

import copy
import json
import unittest

import _paths  # noqa: F401
import check_config

CONFIG = json.loads((_paths.LAB / "config.json").read_text(encoding="utf-8"))


def mutated(path: list, value):
    cfg = copy.deepcopy(CONFIG)
    node = cfg
    for key in path[:-1]:
        node = node[key]
    node[path[-1]] = value
    return cfg


def with_live(mode, authorization):
    cfg = copy.deepcopy(CONFIG)
    cfg["stage_b"]["live_network"] = mode
    cfg["stage_b"]["live_network_authorization"] = authorization
    return cfg


class CheckConfigTests(unittest.TestCase):
    def test_the_shipped_config_is_valid(self):
        self.assertEqual(check_config.validate(CONFIG), [])

    def test_outputs(self):
        both = copy.deepcopy(CONFIG)
        both["stage_a"]["enabled"] = True
        both["stage_b"]["enabled"] = True
        out = check_config.outputs(both)
        self.assertEqual(out["stage_a"], "true")
        self.assertEqual(out["stage_b"], "true")
        self.assertEqual(out["stage_b_profile"], CONFIG["stage_b"]["profile"])
        self.assertEqual(out["stage_b_launcher_source"], CONFIG["stage_b"]["launcher_source"])
        off = mutated(["stage_a", "enabled"], False)
        self.assertEqual(check_config.outputs(off)["stage_a_macos_intel"], "false")
        self.assertEqual(check_config.outputs(off)["stage_a"], "false")

    def test_rejections(self):
        cases = {
            "schema": mutated(["schema"], "x"),
            "repository": mutated(["repository"], "other/repo"),
            "base commit": mutated(["base_commit"], "abc"),
            "digest format": mutated(["handoff", "artifact_digest"], "d1f5"),
            "latest.json digest": mutated(["handoff", "latest_json_sha256"], "xyz"),
            "launcher missing": mutated(["handoff", "launchers"], {"arc-node-linux-x86_64": "0" * 64}),
            "launcher hex": mutated(["handoff", "launchers", "arc-node-macos-arm64"], "XYZ"),
            "tag": mutated(["handoff", "tag"], "v0.8.0"),
            "expect differs from handoff": mutated(["stage_b", "expect_sha256"], "1" * 64),
            "profile name": mutated(["stage_b", "profile"], "huge"),
            "source": mutated(["stage_b", "launcher_source"], "mirror"),
            "live value": mutated(["stage_b", "live_network"], "yes"),
            "live authorization missing": with_live("allowed", ""),
            "live authorization without the stake-0 statement": with_live("allowed", "x" * 120),
            "image url": mutated(["stage_b", "image", "url"], "https://example.com/x.img"),
            "vm cpus": mutated(["stage_b", "vm", "cpus"], 64),
            "full below floor total": mutated(["stage_b", "profiles", "full", "min_total_s"], 14399),
            "full below floor steady": mutated(["stage_b", "profiles", "full", "min_steady_s"], 7199),
            "full below floor samples": mutated(["stage_b", "profiles", "full", "min_steady_samples"], 120),
            "full below floor reboot": mutated(["stage_b", "profiles", "full", "post_reboot_healthy_s"], 599),
            "full interval": mutated(["stage_b", "profiles", "full", "sample_interval_s"], 61),
            "full kickstarts": mutated(["stage_b", "profiles", "full", "kickstarts"], 2),
            "deadline too long": mutated(["stage_b", "profiles", "full", "deadline_min"], 340),
            "deadline too short for the total": mutated(["stage_b", "profiles", "full", "deadline_min"], 250),
            "gap factor": mutated(["stage_b", "profiles", "smoke", "max_gap_factor"], 0.5),
            "bool as int": mutated(["stage_b", "profiles", "smoke", "updater_runs"], True),
        }
        for name, cfg in cases.items():
            with self.subTest(name):
                self.assertNotEqual(check_config.validate(cfg), [], name)

    def test_allowed_network_with_the_recorded_authorization_is_valid(self):
        self.assertEqual(check_config.validate(with_live("allowed", CONFIG["stage_b"]["live_network_authorization"])), [])

    def test_the_shipped_config_is_one_of_the_two_runs_the_lab_is_for(self):
        stage_b = CONFIG["stage_b"]
        self.assertIn(stage_b["profile"], ("smoke", "full"))
        if stage_b["live_network"] == "allowed":
            self.assertEqual(stage_b["profile"], "full", "only the real Wave 0 may register on the live network")
            self.assertEqual(stage_b["launcher_source"], "published")
            self.assertIn("TJ authorized one stake-0 test node", stage_b["live_network_authorization"])

    def test_blocked_network_needs_no_authorization(self):
        cfg = mutated(["stage_b", "live_network"], "blocked")
        cfg["stage_b"]["live_network_authorization"] = ""
        self.assertEqual(check_config.validate(cfg), [])

    def test_smoke_may_be_lighter_than_full(self):
        self.assertLess(CONFIG["stage_b"]["profiles"]["smoke"]["min_total_s"], 14400)

    def test_not_an_object(self):
        self.assertEqual(check_config.validate([]), ["config is not a JSON object"])


if __name__ == "__main__":
    unittest.main()
