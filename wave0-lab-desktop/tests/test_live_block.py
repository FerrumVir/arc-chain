"""Tests for lib/live_block.py, the command builders that keep the app away from every ARC address (THROWAWAY LAB FILE)."""
from __future__ import annotations

import json
import os
import re
import subprocess
import unittest

import _paths  # noqa: F401
import live_block as lb

IPS = ["149.28.32.76", "140.82.16.112", "136.244.109.1", "104.238.171.11", "202.182.107.41", "149.28.153.31"]
WORKFLOW = "env:\n  LIVE_NETWORK_IPS: 149.28.32.76 140.82.16.112 136.244.109.1 104.238.171.11 202.182.107.41 149.28.153.31\n"
HARNESS = "set -e\nlive_ips=(149.28.32.76 140.82.16.112 136.244.109.1 104.238.171.11 202.182.107.41 149.28.153.31)\n"
NAMES = lb.blocked_names()


class AddressListTests(unittest.TestCase):
    def test_the_repository_list_is_read_and_cross_checked(self):
        self.assertEqual(lb.load_live_ips(_paths.ROOT), IPS)

    def test_parse_requires_the_two_sources_to_agree(self):
        self.assertEqual(lb.parse_live_ips(WORKFLOW, HARNESS), IPS)
        with self.assertRaises(lb.LiveBlockError):
            lb.parse_live_ips(WORKFLOW, HARNESS.replace("149.28.153.31", "149.28.153.32"))
        with self.assertRaises(lb.LiveBlockError):
            lb.parse_live_ips("nothing", HARNESS)
        with self.assertRaises(lb.LiveBlockError):
            lb.parse_live_ips(WORKFLOW, "nothing")

    def test_validate_ips(self):
        self.assertEqual(lb.validate_ips(IPS), IPS)
        for bad in ([], ["1.2.3.4", "1.2.3.4"], ["1.2.3"], ["a.b.c.d"], ["999.1.1.1"], ["1.2.3.4/32"], ["::1"]):
            with self.subTest(bad):
                with self.assertRaises(lb.LiveBlockError):
                    lb.validate_ips(bad)


class NamesTests(unittest.TestCase):
    def test_blocked_names_cover_arc_and_the_apps_third_party_hosts(self):
        for name in ("arc.ai", "testnet.arc.ai", "huggingface.co", "rsms.me"):
            self.assertIn(name, NAMES)

    def test_github_names_can_never_be_blackholed(self):
        for name in ("github.com", "api.github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"):
            with self.subTest(name):
                with self.assertRaises(lb.LiveBlockError):
                    lb.blocked_names([name])

    def test_extra_names_are_added_once_and_validated(self):
        self.assertEqual(lb.blocked_names(["Seed.Example.org", "seed.example.org"]).count("seed.example.org"), 1)
        with self.assertRaises(lb.LiveBlockError):
            lb.blocked_names(["bad name"])

    def test_names_in_the_seed_and_genesis_files_of_the_released_app(self):
        seeds = "# comment\n149.28.32.76:9091 # NYC\nseed1.example.org:9091\n[::1]:443\nseed2.example.org\n"
        genesis = 'url = "https://rpc.example.net/health"\nip = "http://149.28.32.76:9090"\n'
        self.assertEqual(lb.names_in_seed_files(seeds, genesis), ["rpc.example.net", "seed1.example.org", "seed2.example.org"])

    def test_the_released_v0711_files_hold_addresses_only(self):
        def show(path):
            if os.environ.get("WAVE0_TEST_REAL_REPO") != "1":
                return None  # opt-in: a default local run never reads the shared repository
            done = subprocess.run(["git", "show", "v0.7.11:" + path], cwd=str(_paths.ROOT), capture_output=True, text=True)
            return done.stdout if done.returncode == 0 else None

        seeds, genesis = show("desktop/src-tauri/resources/testnet-seeds.txt"), show("desktop/src-tauri/resources/genesis.toml")
        if seeds is None or genesis is None:
            self.skipTest("the v0.7.11 tag is not available in this checkout")
        self.assertEqual(lb.names_in_seed_files(seeds, genesis), [])

    def test_hosts_lines_cover_both_loopback_families_and_carry_the_marker(self):
        lines = lb.hosts_lines(["a.example.com", "B.example.com"])
        self.assertEqual(lines, [
            "127.0.0.1 a.example.com # wave0-lab-desktop", "::1 a.example.com # wave0-lab-desktop",
            "127.0.0.1 b.example.com # wave0-lab-desktop", "::1 b.example.com # wave0-lab-desktop",
        ])
        with self.assertRaises(lb.LiveBlockError):
            lb.hosts_lines(["not a name"])

    def test_loopback_hosts_lines_for_the_github_names(self):
        lines = lb.loopback_hosts_lines(["github.com", "api.github.com"])
        self.assertEqual(len(lines), 4)
        self.assertTrue(all(line.endswith(lb.MARKER) for line in lines))


class PlanTests(unittest.TestCase):
    def check_plan_shape(self, plan):
        self.assertEqual(sorted(plan), ["commands", "hosts_path", "shell", "verify"])
        self.assertTrue(plan["commands"] and all(isinstance(command, str) for command in plan["commands"]))
        self.assertIsInstance(plan["verify"], str)
        json.dumps(plan)
        joined = " ".join(plan["commands"]) + " " + plan["verify"]
        self.assertNotRegex(joined, r"\brm\b|\brmdir\b|Remove-Item")

    def test_linux_rejects_every_address_and_adds_the_hosts_lines(self):
        plan = lb.linux_commands(IPS, NAMES, lb.loopback_hosts_lines(["github.com"]))
        self.check_plan_shape(plan)
        rules = [command for command in plan["commands"] if command.startswith("sudo iptables")]
        self.assertEqual(rules, ["sudo iptables -I OUTPUT -d %s -j REJECT" % ip for ip in IPS])
        hosts = plan["commands"][-1]
        self.assertIn("sudo tee -a /etc/hosts", hosts)
        self.assertIn("'127.0.0.1 github.com # wave0-lab-desktop'", hosts)
        self.assertIn("'::1 arc.ai # wave0-lab-desktop'", hosts)
        self.assertEqual(plan["shell"], "bash")

    def test_linux_verify_checks_each_rule_before_it_ever_connects(self):
        verify = lb.linux_commands(IPS, NAMES)["verify"]
        first_connect = verify.index("curl")
        for ip in IPS:
            self.assertIn("-d %s/32 -j REJECT" % ip, verify[:first_connect])
        self.assertIn("exit 1", verify)

    def test_windows_blocks_outbound_to_every_address(self):
        plan = lb.windows_commands(IPS, NAMES)
        self.check_plan_shape(plan)
        rule = plan["commands"][0]
        self.assertIn("New-NetFirewallRule", rule)
        self.assertIn("-Direction Outbound -Action Block", rule)
        for ip in IPS:
            self.assertIn("'%s'" % ip, rule)
        self.assertIn("Set-NetFirewallProfile -All -Enabled True", plan["commands"])
        self.assertIn("drivers\\etc\\hosts", plan["commands"][2])
        self.assertEqual(plan["shell"], "pwsh")
        self.assertLess(plan["verify"].index("Get-NetFirewallRule"), plan["verify"].index("ConnectAsync"))

    def test_macos_loads_one_pf_rule_per_address_before_enabling_pf(self):
        plan = lb.macos_commands(IPS, NAMES, "/tmp/arc-pf.conf")
        self.check_plan_shape(plan)
        lines = [command for command in plan["commands"] if command.startswith("echo 'block drop out quick to")]
        self.assertEqual(lines, ["echo 'block drop out quick to %s' >> /tmp/arc-pf.conf" % ip for ip in IPS])
        load = plan["commands"].index("sudo pfctl -f /tmp/arc-pf.conf")
        enable = plan["commands"].index("sudo pfctl -e || sudo pfctl -s info | grep -q 'Status: Enabled'")
        self.assertLess(max(plan["commands"].index(line) for line in lines), load)
        self.assertLess(load, enable)
        verify = plan["verify"]
        for ip in IPS:
            self.assertLess(verify.index("block drop out quick to %s" % ip), verify.index("curl"))

    def test_build_dispatches_and_refuses_unknown_systems(self):
        for name, shell in (("linux", "bash"), ("windows", "pwsh"), ("macos", "bash"), ("macos-arm64", "bash"), ("macos_intel", "bash")):
            self.assertEqual(lb.build(name, IPS, NAMES)["shell"], shell)
        with self.assertRaises(lb.LiveBlockError):
            lb.build("plan9", IPS, NAMES)

    def test_a_bad_address_stops_every_builder(self):
        for builder in (lb.linux_commands, lb.windows_commands):
            with self.assertRaises(lb.LiveBlockError):
                builder(["1.2.3"], NAMES)
        with self.assertRaises(lb.LiveBlockError):
            lb.macos_commands(["1.2.3"], NAMES, "x")

    def test_commands_with_unusual_paths_are_quoted(self):
        plan = lb.macos_commands(IPS, NAMES, "/tmp/my dir/pf.conf")
        self.assertTrue(any("'/tmp/my dir/pf.conf'" in command for command in plan["commands"]))
        self.assertIsNone(re.search(r"pfctl -f /tmp/my dir", " ".join(plan["commands"])))

    def test_cli_prints_a_plan(self):
        import contextlib
        import io

        with contextlib.redirect_stdout(io.StringIO()) as buffer:
            self.assertEqual(lb.main(["linux", "--repo", str(_paths.ROOT), "--github-hosts", "github.com"]), 0)
        plan = json.loads(buffer.getvalue())
        self.assertEqual(len([c for c in plan["commands"] if "iptables" in c]), 6)


if __name__ == "__main__":
    unittest.main()
