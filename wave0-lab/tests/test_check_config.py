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


def without(path: list):
    cfg = copy.deepcopy(CONFIG)
    node = cfg
    for key in path[:-1]:
        node = node[key]
    del node[path[-1]]
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
            "vm memory not 4 GiB": mutated(["stage_b", "vm", "memory_mb"], 6144),
            "scoreboard interval too long": mutated(["stage_b", "scoreboard_interval_s"], 600),
            "rss bound weakened": mutated(["stage_b", "resource_bounds", "rss_slope_mib_per_h_max"], 150),
            "memory floor weakened": mutated(["stage_b", "resource_bounds", "mem_available_min_mib"], 256),
            "projection weakened": mutated(["stage_b", "resource_bounds", "projection_fraction"], 0.9),
            "disk reserve weakened": mutated(["stage_b", "resource_bounds", "disk_reserve_floor_b"], 1073741824),
            "disk runway weakened": mutated(["stage_b", "resource_bounds", "disk_runway_h_min"], 24),
            "an invented extra bound": mutated(["stage_b", "resource_bounds", "swap_used_max_kb"], 0),
            "supplement with a battery": mutated(["stage_b", "profiles", "resources", "battery"], True),
            "supplement too short": mutated(["stage_b", "profiles", "resources", "min_steady_s"], 3600),
            "supplement settle too short": mutated(["stage_b", "profiles", "resources", "settle_s"], 60),
            "supplement without margin": mutated(["stage_b", "profiles", "resources", "deadline_min"], 150),
            "supplement window below 135 minutes": mutated(["stage_b", "profiles", "resources", "min_steady_s"], 8099),
            "supplement samples below 136": mutated(["stage_b", "profiles", "resources", "min_steady_samples"], 135),
            "supplement total below 8700": mutated(["stage_b", "profiles", "resources", "min_total_s"], 8699),
            "supplement gap above 65 s": mutated(["stage_b", "profiles", "resources", "max_gap_factor"], 1.2),
            "supplement interval above 60 s": mutated(["stage_b", "profiles", "resources", "sample_interval_s"], 61),
            "scoreboard interval above a minute": mutated(["stage_b", "scoreboard_interval_s"], 61),
            "scoreboard interval hammering the coordinators": mutated(["stage_b", "scoreboard_interval_s"], 5),
            "prior run missing": without(["stage_b", "prior_run"]),
            "prior run id": mutated(["stage_b", "prior_run", "run_id"], 1),
            "prior vm memory": mutated(["stage_b", "prior_run", "vm_memory_mb"], 4096),
            "prior artifact digest": mutated(["stage_b", "prior_run", "evidence_artifact_digest"], "sha256:xyz"),
            "prior launcher is not the launcher under test": mutated(["stage_b", "prior_run", "launcher_sha256"], "2" * 64),
            "prior installer digest": mutated(["stage_b", "prior_run", "installer_sha256"], "short"),
            "prior units incomplete": without(["stage_b", "prior_run", "units", "arc-updater.timer"]),
            "prior baseline line": mutated(["stage_b", "prior_run", "baseline_result"], "something else"),
        }
        for name, cfg in cases.items():
            with self.subTest(name):
                self.assertNotEqual(check_config.validate(cfg), [], name)

    def test_allowed_network_with_the_recorded_authorization_is_valid(self):
        self.assertEqual(check_config.validate(with_live("allowed", CONFIG["stage_b"]["live_network_authorization"])), [])

    def test_the_shipped_config_is_one_of_the_runs_the_lab_is_for(self):
        stage_b = CONFIG["stage_b"]
        self.assertIn(stage_b["profile"], ("smoke", "full", "resources"))
        if stage_b["live_network"] == "allowed":
            self.assertIn(stage_b["profile"], ("full", "resources"), "only the real Wave 0 and its resources supplement may register on the live network")
            self.assertEqual(stage_b["launcher_source"], "published")
            self.assertIn("TJ authorized one stake-0 test node", stage_b["live_network_authorization"])

    def test_the_supplement_profile_meets_astras_decision(self):
        profile = CONFIG["stage_b"]["profiles"]["resources"]
        self.assertFalse(profile["battery"])
        self.assertGreaterEqual(profile["min_steady_s"], 8100)
        self.assertGreaterEqual(profile["min_steady_samples"], 136)
        self.assertLessEqual(profile["sample_interval_s"] * profile["max_gap_factor"], 65.01)
        self.assertGreaterEqual(profile["settle_s"], 300)
        self.assertGreaterEqual(profile["min_total_s"], profile["min_steady_s"] + profile["settle_s"])
        self.assertLessEqual(CONFIG["stage_b"]["scoreboard_interval_s"], 60)
        self.assertEqual(CONFIG["stage_b"]["vm"]["memory_mb"], 4096)

    def test_the_prior_run_block_matches_the_four_hour_run(self):
        prior = CONFIG["stage_b"]["prior_run"]
        self.assertEqual(prior["run_id"], 37750760170)
        self.assertEqual(prior["vm_memory_mb"], 6144)
        self.assertEqual(prior["launcher_sha256"], CONFIG["stage_b"]["expect_sha256"])
        self.assertEqual(sorted(prior["units"]), ["arc-node.service", "arc-updater.service", "arc-updater.timer"])
        self.assertNotIn("v07_pid", prior["baseline_result"], "the pid of the v0.7.7 node is run-specific")

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
