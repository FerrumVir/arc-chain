"""Offline tests of the guest-side scripts (THROWAWAY LAB FILE): syntax, lint, a fake iptables, and the probes' contract."""
from __future__ import annotations

import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import _paths  # noqa: F401
import capture_state
import heartbeat_poller
import invariants
import probe
import sampler

GUEST = _paths.LAB / "stage_b" / "guest"
SHELL = sorted(GUEST.glob("*.sh"))
PYTHON = sorted(GUEST.glob("*.py"))

FAKE_IPTABLES = r'''#!/usr/bin/env python3
import json, os, sys
path = os.environ["FAKE_IPT_STATE"]
state = json.load(open(path)) if os.path.exists(path) else {"INPUT": [], "OUTPUT": []}
a = sys.argv[1:]
def save():
    json.dump(state, open(path, "w"))
op = a[0]
if op == "-N":
    if a[1] in state:
        sys.exit(1)
    state[a[1]] = []
elif op == "-F":
    if a[1] not in state:
        sys.exit(1)
    state[a[1]] = []
elif op == "-X":
    if a[1] not in state or state[a[1]]:
        sys.exit(1)
    del state[a[1]]
elif op == "-A":
    state[a[1]].append(" ".join(a[2:]))
elif op == "-I":
    position = int(a[2]) if a[2].isdigit() else 1
    rule = " ".join(a[3:] if a[2].isdigit() else a[2:])
    state[a[1]].insert(position - 1, rule)
elif op == "-C":
    sys.exit(0 if a[1] in state and " ".join(a[2:]) in state[a[1]] else 1)
elif op == "-D":
    rule = " ".join(a[2:])
    if rule not in state.get(a[1], []):
        sys.exit(1)
    state[a[1]].remove(rule)
elif op == "-nvxL":
    print("Chain %s (policy ACCEPT 0 packets, 0 bytes)" % a[1])
    print("    pkts      bytes target     prot opt in     out     source               destination")
    for rule in state[a[1]]:
        counted = "w0-count" in rule
        print("%8d %10d %s" % (7 if counted else 0, 12345 if counted else 0, "all  --  *      *       0.0.0.0/0            0.0.0.0/0  " + rule + " /* ... */"))
else:
    sys.exit(2)
save()
'''


def fake_environment():
    tmp = tempfile.TemporaryDirectory()
    root = Path(tmp.name)
    binary = root / "bin"
    binary.mkdir()
    script = binary / "iptables"
    script.write_text(FAKE_IPTABLES, encoding="utf-8")
    script.chmod(script.stat().st_mode | stat.S_IXUSR)
    env = dict(os.environ, PATH=f"{binary}:{os.environ['PATH']}", FAKE_IPT_STATE=str(root / "state.json"))
    return tmp, root, env


def state_of(root):
    path = root / "state.json"
    return json.loads(path.read_text()) if path.exists() else {}


class LiveIpsTests(unittest.TestCase):
    def test_output_is_a_single_lf_terminated_line_without_carriage_returns(self):
        done = subprocess.run([sys.executable, "-B", str(_paths.LAB / "live_ips.py"), str(_paths.ROOT)], capture_output=True)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertTrue(done.stdout.endswith(b"\n"))
        self.assertNotIn(b"\r", done.stdout)
        self.assertEqual(done.stdout.count(b"\n"), 1)
        self.assertEqual(len(done.stdout.split()), 6)


class ShellTests(unittest.TestCase):
    def test_every_shell_script_parses(self):
        self.assertEqual(len(SHELL), 6)
        for script in SHELL:
            with self.subTest(script.name):
                self.assertEqual(subprocess.run(["bash", "-n", str(script)], capture_output=True).returncode, 0)
        guard = _paths.LAB / "guard.sh"
        self.assertEqual(subprocess.run(["bash", "-n", str(guard)], capture_output=True).returncode, 0)

    @unittest.skipUnless(shutil.which("shellcheck"), "shellcheck is not installed")
    def test_shellcheck_is_clean_at_warning_level(self):
        done = subprocess.run(["shellcheck", "-S", "warning", *map(str, SHELL), str(_paths.LAB / "guard.sh")], capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stdout)

    def test_scripts_are_executable_and_start_with_a_shebang(self):
        for script in SHELL + PYTHON:
            with self.subTest(script.name):
                self.assertTrue(os.access(script, os.X_OK), "not executable")
                self.assertTrue(script.read_text().startswith("#!"), "no shebang")

    def test_python_files_compile(self):
        for script in PYTHON + [_paths.LAB / "stage_b" / "host_run.py"]:
            with self.subTest(script.name):
                compile(script.read_text(), str(script), "exec")


class InterruptTests(unittest.TestCase):
    def run_script(self, env, *args):
        return subprocess.run(["bash", str(GUEST / "interrupt.sh"), *args], capture_output=True, text=True, env=env)

    def test_arm_counters_disarm(self):
        tmp, root, env = fake_environment()
        self.addCleanup(tmp.cleanup)
        done = self.run_script(env, "arm", "14000000")
        self.assertEqual(done.returncode, 0, done.stderr)
        state = state_of(root)
        self.assertEqual(state["W0QUOTA"], ["-m quota --quota 14000000 -j ACCEPT", "-j DROP"])
        self.assertEqual(state["INPUT"][0], "-p tcp --sport 443 -m comment --comment w0-count", "the counter rule is first and has no target")
        self.assertEqual(state["INPUT"][1], "-p tcp --sport 443 -j W0QUOTA")
        counters = self.run_script(env, "counters")
        self.assertEqual(counters.stdout.strip(), "12345")
        self.assertEqual(self.run_script(env, "disarm").returncode, 0)
        state = state_of(root)
        self.assertEqual(state["INPUT"], [])
        self.assertNotIn("W0QUOTA", state)
        self.assertEqual(self.run_script(env, "disarm").returncode, 0, "disarm is idempotent")

    def test_arm_twice_does_not_stack_rules(self):
        tmp, root, env = fake_environment()
        self.addCleanup(tmp.cleanup)
        self.run_script(env, "arm", "1000")
        self.run_script(env, "arm", "2000")
        state = state_of(root)
        self.assertEqual(len(state["INPUT"]), 2)
        self.assertEqual(state["W0QUOTA"][0], "-m quota --quota 2000 -j ACCEPT")

    def test_count_only_adds_the_counter(self):
        tmp, root, env = fake_environment()
        self.addCleanup(tmp.cleanup)
        self.assertEqual(self.run_script(env, "count").returncode, 0)
        self.assertEqual(state_of(root)["INPUT"], ["-p tcp --sport 443 -m comment --comment w0-count"])

    def test_counters_without_rules_print_zero(self):
        tmp, root, env = fake_environment()
        self.addCleanup(tmp.cleanup)
        self.assertEqual(self.run_script(env, "counters").stdout.strip(), "0")

    def test_selftest_leaves_nothing_behind(self):
        tmp, root, env = fake_environment()
        self.addCleanup(tmp.cleanup)
        self.assertEqual(self.run_script(env, "selftest").returncode, 0)
        state = state_of(root)
        self.assertEqual(state["INPUT"], [])
        self.assertNotIn("W0QUOTA", state)

    def test_usage(self):
        tmp, root, env = fake_environment()
        self.addCleanup(tmp.cleanup)
        self.assertEqual(self.run_script(env).returncode, 64)

    @unittest.skipUnless(sys.platform.startswith("linux"), "needs GNU stat")
    def test_watch_cuts_when_the_partial_reaches_the_threshold(self):
        tmp, root, env = fake_environment()
        self.addCleanup(tmp.cleanup)
        partial = root / "x.partial"
        partial.write_bytes(b"x" * 100)
        done = self.run_script(env, "watch", str(partial), "50", "5")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("-p tcp --sport 443 -m comment --comment w0-cut -j DROP", state_of(root)["INPUT"])
        self.run_script(env, "disarm")
        self.assertEqual(state_of(root)["INPUT"], [])


class LiveBlockTests(unittest.TestCase):
    def test_on_off_status(self):
        tmp, root, env = fake_environment()
        self.addCleanup(tmp.cleanup)
        ips = root / "ips.txt"
        ips.write_text("192.0.2.1\n192.0.2.2\n")
        env["ARC_W0_LIVE_IPS"] = str(ips)

        def run(action):
            return subprocess.run(["bash", str(GUEST / "live-block.sh"), action], capture_output=True, text=True, env=env)

        self.assertEqual(run("on").returncode, 0)
        self.assertEqual(run("on").returncode, 0)
        self.assertEqual(state_of(root)["OUTPUT"], ["-d 192.0.2.2 -j REJECT", "-d 192.0.2.1 -j REJECT"], "idempotent: one rule per address")
        self.assertEqual(run("status").stdout.count("blocked"), 2)
        self.assertEqual(run("off").returncode, 0)
        self.assertEqual(state_of(root)["OUTPUT"], [])
        self.assertEqual(run("status").stdout.count("open"), 2)
        self.assertEqual(run("bogus").returncode, 64)


class ProbeContractTests(unittest.TestCase):
    EVALUATOR_SAMPLE_KEYS = {
        "seq", "epoch", "boot_id", "uptime_s", "node_state", "main_pid", "proc_start_epoch", "node_exe", "node_exe_sha256", "node_procs", "n_restarts",
        "health_ok", "chain_participation_enabled", "info_ok", "address", "stake", "node_version", "bridge_node_address", "bridge_compute",
        "compute_consent", "community_registration", "public_name", "coordinators_total", "coordinators_registered", "version_txt", "launcher_sha256",
        "updater_timer_active", "legacy_fingerprint", "legacy_byte_compare", "errors",
        # resource series and registration continuity (ARC-83 D and E)
        "last_registration_unix_ms", "registration_age_s", "node_rss_kb", "node_hwm_kb", "node_swap_kb", "node_threads", "node_fds", "node_cpu_s",
        "mem_total_kb", "mem_available_kb", "swap_total_kb", "swap_free_kb", "cg_mem_current_b", "cg_mem_peak_b", "cg_swap_current_b",
        "disk_total_b", "disk_free_b", "log_bytes", "bridge_downloads", "arc_dir_bytes", "largest_file_bytes", "legacy_data_bytes",
        "node_data_bytes", "release_cache_bytes", "release_cache_files", "models_bytes", "partial_files",
    }

    def test_a_sample_has_exactly_the_contract_keys_and_never_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            sample = probe.sample_once(tmp, 7)
        self.assertEqual(set(sample), self.EVALUATOR_SAMPLE_KEYS)
        self.assertEqual(sample["seq"], 7)
        json.dumps(sample)

    def test_sample_with_a_fake_install(self):
        with tempfile.TemporaryDirectory() as tmp:
            arc = Path(tmp)
            (arc / "bin").mkdir()
            (arc / "bin" / "arc-node").write_bytes(b"launcher")
            (arc / "version.txt").write_text("0.7.12\n")
            (arc / "data").mkdir()
            (arc / "data" / "state.wal").write_bytes(b"wal")
            node = arc / "legacy-bridge" / "nodes" / "headless-abc"
            node.mkdir(parents=True)
            (node / "bridge-state.json").write_text(json.dumps({"node_address": "aa", "compute": "off: no model", "community_registration": True}))
            (node / "compute-consent").write_text("no\n")
            sample = probe.sample_once(str(arc), 1)
        self.assertEqual(sample["version_txt"], "0.7.12")
        self.assertEqual(sample["bridge_node_address"], "aa")
        self.assertEqual(sample["bridge_compute"], "off: no model")
        self.assertEqual(sample["compute_consent"], "no")
        self.assertEqual(sample["community_registration"], True)
        self.assertEqual(len(sample["launcher_sha256"]), 64)
        self.assertEqual(len(sample["legacy_fingerprint"]), 64)

    def test_the_byte_compare_runs_only_for_the_bridged_node(self):
        with tempfile.TemporaryDirectory() as tmp:
            arc = Path(tmp)
            (arc / "data").mkdir()
            (arc / "data" / "state.wal").write_bytes(b"wal")
            hook = arc / "before.json"
            hook.write_text(json.dumps({"binary_sha256": "x", "entries": probe.snapshot_entries(str(arc / "data"))}))
            sample = probe.sample_once(str(arc), 1, str(hook), True)
            self.assertIsNone(sample["legacy_byte_compare"], "no bridged node process here, so no compare")

    def test_legacy_fingerprint_changes_when_the_data_changes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "a").write_bytes(b"1")
            first = probe.legacy_fingerprint(str(root))
            self.assertEqual(first, probe.legacy_fingerprint(str(root)))
            (root / "a").write_bytes(b"22")
            self.assertNotEqual(first, probe.legacy_fingerprint(str(root)))
        self.assertIsNone(probe.legacy_fingerprint("/nonexistent-dir"))

    def test_entries_digest_is_canonical(self):
        self.assertEqual(probe.entries_digest({"b": 1, "a": 2}), probe.entries_digest({"a": 2, "b": 1}))

    def test_byte_compare_uses_the_hook_snapshot_format(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "data"
            root.mkdir()
            (root / "state.wal").write_bytes(b"wal")
            entries = probe.snapshot_entries(str(root))
            hook = Path(tmp) / "before.json"
            hook.write_text(json.dumps({"binary_sha256": "x", "entries": entries}))
            self.assertEqual(probe.byte_compare(str(hook), str(root)), "same")
            (root / "state.wal").write_bytes(b"other")
            self.assertEqual(probe.byte_compare(str(hook), str(root)), "diff")
            self.assertIsNone(probe.byte_compare(str(Path(tmp) / "missing.json"), str(root)))

    def test_sampler_last_seq_and_continuation(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "s.jsonl"
            self.assertEqual(sampler.last_seq(str(out)), -1)
            out.write_text(json.dumps({"seq": 4}) + "\n" + "garbage\n" + json.dumps({"seq": 5}) + "\n")
            self.assertEqual(sampler.last_seq(str(out)), 5)
            with mock.patch.object(sys, "argv", ["sampler.py", "--out", str(out), "--arc-dir", tmp, "--interval", "0.01", "--max-samples", "2"]):
                self.assertEqual(sampler.main(), 0)
            lines = [json.loads(line) for line in out.read_text().splitlines() if line.startswith("{")]
            self.assertEqual([line["seq"] for line in lines], [4, 5, 6, 7], "seq continues after the last line")

    def test_redacted_argv_hides_seed_values(self):
        cmdline = "/x/arc-node\0--stake\x000\0--validator-seed\0community-secret\0--community-mode\0"
        with mock.patch.object(probe, "read_text", return_value=cmdline):
            argv = invariants.redacted_argv(1)
        self.assertNotIn("community-secret", argv)
        self.assertEqual(argv, ["/x/arc-node", "--stake", "0", "--validator-seed", "<redacted>", "--community-mode"])

    def test_unit_hashes_ignore_the_seed(self):
        text_a = "ExecStart=/x --validator-seed community-aaa --stake 0\n"
        text_b = "ExecStart=/x --validator-seed community-bbb --stake 0\n"
        with mock.patch.object(probe, "read_text", return_value=text_a):
            first = invariants.unit_hashes("community-aaa")
        with mock.patch.object(probe, "read_text", return_value=text_b):
            second = invariants.unit_hashes("community-bbb")
        self.assertEqual(first, second)

    MEMINFO = (
        "MemTotal:        3921412 kB\nMemFree:         3401000 kB\nMemAvailable:    3500123 kB\nBuffers:           10000 kB\n"
        "SwapTotal:       2097148 kB\nSwapFree:        2097100 kB\n"
    )
    STATUS = "Name:\tarc-node\nVmPeak:\t  900000 kB\nVmHWM:\t   52000 kB\nVmRSS:\t   41232 kB\nVmSwap:\t      48 kB\nThreads:\t17\n"

    def test_resource_fields_parse_the_real_proc_formats(self):
        def fake_read(path):
            if path == "/proc/meminfo":
                return self.MEMINFO
            if path == "/proc/4242/status":
                return self.STATUS
            return None

        with mock.patch.object(probe, "read_text", side_effect=fake_read), mock.patch.object(probe, "count_fds", return_value=33), \
                mock.patch.object(probe, "cpu_seconds", return_value=12.5), mock.patch.object(probe, "cgroup_dir", return_value=None):
            with tempfile.TemporaryDirectory() as tmp:
                fields = probe.resource_fields(tmp, 4242)
        self.assertEqual((fields["mem_total_kb"], fields["mem_available_kb"], fields["swap_total_kb"], fields["swap_free_kb"]), (3921412, 3500123, 2097148, 2097100))
        self.assertEqual((fields["node_rss_kb"], fields["node_hwm_kb"], fields["node_swap_kb"], fields["node_threads"], fields["node_fds"], fields["node_cpu_s"]), (41232, 52000, 48, 17, 33, 12.5))
        self.assertGreater(fields["disk_total_b"], fields["disk_free_b"] - 1)

    def test_cgroup_counters_come_from_the_units_cgroup_v2_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            group = Path(tmp)
            (group / "memory.current").write_text("12345678\n")
            (group / "memory.peak").write_text("44666666\n")
            (group / "memory.swap.current").write_text("0\n")
            with mock.patch.object(probe, "cgroup_dir", return_value=str(group)):
                fields = probe.resource_fields(tmp, 1)
        self.assertEqual((fields["cg_mem_current_b"], fields["cg_mem_peak_b"], fields["cg_swap_current_b"]), (12345678, 44666666, 0))
        self.assertEqual(probe.parse_kb(self.MEMINFO, "MemAvailable"), 3500123)
        self.assertIsNone(probe.parse_kb(self.MEMINFO, "Nope"))
        self.assertIsNone(probe.parse_kb(None, "MemTotal"))

    def test_oom_scan_counts_only_oom_lines_in_every_boot(self):
        import oom_scan

        outputs = {
            ("--list-boots",): " -1 abc Thu 2026-10-08 08:35:40 UTC Thu 2026-10-08 08:54:04 UTC\n  0 def Thu 2026-10-08 08:54:19 UTC Thu 2026-10-08 12:45:41 UTC\n",
            ("-k", "-b", "-1", "-o", "short-iso"): "2026-10-08T08:40:00+0000 kernel: Out of memory: Killed process 99 (x)\n2026-10-08T08:41:00+0000 kernel: normal line\n",
            ("-k", "-b", "0", "-o", "short-iso"): "-- No entries --\n2026-10-08T09:00:00+0000 kernel: normal\n",
        }
        with mock.patch.object(oom_scan, "journal", side_effect=lambda args: outputs[tuple(args)]):
            self.assertEqual(oom_scan.boot_indexes(), [-1, 0])
            scans = [oom_scan.scan_boot(i) for i in oom_scan.boot_indexes()]
        self.assertEqual([scan["count"] for scan in scans], [1, 0])
        self.assertEqual(scans[0]["boot"], -1)
        self.assertEqual(scans[1]["kernel_lines"], 1, "the '-- No entries --' marker is not a kernel line")

    def test_registration_age_is_the_sample_time_minus_the_last_successful_round(self):
        status = {"public_name": "node-abcd1234", "coordinators_total": 6, "coordinators_registered": 5, "last_registration_unix_ms": 1_800_000_000_000}

        def fake_http(path, timeout=3.0):
            return (True, status) if path == "/community/worker/status" else (False, None)

        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(probe, "http_json", side_effect=fake_http), \
                mock.patch.object(probe.time, "time", return_value=1_800_000_012.25):
            sample = probe.sample_once(tmp, 3)
        self.assertEqual(sample["last_registration_unix_ms"], 1_800_000_000_000)
        self.assertEqual(sample["registration_age_s"], 12.25)

    def test_registration_age_is_null_when_the_node_reports_no_round_yet(self):
        for status in ({"coordinators_registered": 0}, {"last_registration_unix_ms": None}, {"last_registration_unix_ms": "soon"}, {"last_registration_unix_ms": True}):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as tmp, \
                    mock.patch.object(probe, "http_json", side_effect=lambda path, timeout=3.0, s=status: (True, s) if path == "/community/worker/status" else (False, None)):
                sample = probe.sample_once(tmp, 3)
            self.assertIsNone(sample["last_registration_unix_ms"])
            self.assertIsNone(sample["registration_age_s"])

    def test_heartbeat_poll_records_the_node_timestamp_and_every_failure(self):
        good = {"coordinators_registered": 6, "coordinators_total": 6, "last_registration_unix_ms": 1_800_000_015_000}
        with mock.patch.object(probe, "http_json", return_value=(True, good)):
            record = heartbeat_poller.poll_once()
        self.assertEqual((record["ts_ms"], record["registered"], record["total"]), (1_800_000_015_000, 6, 6))
        self.assertIsInstance(record["obs_epoch"], float)
        for answer in ((False, None), (True, None), (True, [1, 2]), (True, {"last_registration_unix_ms": "x"})):
            with self.subTest(answer=answer), mock.patch.object(probe, "http_json", return_value=answer):
                record = heartbeat_poller.poll_once()
            self.assertIsNone(record["ts_ms"], "a failed poll is still a line, with no timestamp")
            self.assertIn("obs_epoch", record)

    def test_heartbeat_poller_writes_one_line_per_poll_and_survives_a_crashing_poll(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "sub" / "heartbeats.jsonl"
            calls = {"n": 0}

            def flaky(path, timeout=3.0):
                calls["n"] += 1
                if calls["n"] == 2:
                    raise RuntimeError("boom")
                return True, {"coordinators_registered": 6, "coordinators_total": 6, "last_registration_unix_ms": 1_800_000_000_000 + calls["n"] * 15_000}

            with mock.patch.object(probe, "http_json", side_effect=flaky), \
                    mock.patch.object(sys, "argv", ["heartbeat_poller.py", "--out", str(out), "--interval", "0.01", "--max-polls", "4"]):
                self.assertEqual(heartbeat_poller.main(), 0)
            lines = [json.loads(line) for line in out.read_text().splitlines()]
        self.assertEqual(len(lines), 4)
        self.assertEqual(lines[1]["error"], "RuntimeError")
        self.assertIsNone(lines[1]["ts_ms"])
        self.assertEqual([line["ts_ms"] for line in lines if line["ts_ms"]], [1_800_000_015_000, 1_800_000_045_000, 1_800_000_060_000])

    def test_the_heartbeat_poller_is_a_unit_polling_every_five_seconds_and_is_collected(self):
        units = (GUEST / "install-units.sh").read_text()
        self.assertIn("arc-w0-heartbeat.service", units)
        self.assertIn("heartbeat_poller.py --out /var/lib/arc-w0/heartbeats.jsonl --interval 5", units)
        self.assertIn("enable --now arc-w0-heartbeat.service", units)
        self.assertIn('"$work"/heartbeats.jsonl', (GUEST / "collect.sh").read_text())

    def test_count_nodes_prints_a_single_integer(self):
        with tempfile.TemporaryDirectory() as tmp:
            done = subprocess.run([sys.executable, "-B", str(GUEST / "count_nodes.py"), "--arc-dir", tmp], capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertRegex(done.stdout, r"^\d+\n$")

    def test_capture_state_has_the_fields_the_orchestrator_compares(self):
        with tempfile.TemporaryDirectory() as tmp:
            state = capture_state.capture(tmp)
        for name in ("bin_arc_node_sha256", "version_txt", "main_pid", "proc_start_epoch", "node_exe_sha256", "address", "bridge_node_address",
                     "legacy_fingerprint", "auto_update_rolled_back", "health_ok", "bridge_log_reuse_count", "updater_unit", "boot_id"):
            self.assertIn(name, state)

    def test_invariants_record_has_the_contract_keys(self):
        with tempfile.TemporaryDirectory() as tmp:
            arc = Path(tmp)
            (arc / "data").mkdir()
            (arc / "data" / "state.wal").write_bytes(b"wal")
            (arc / "identity.seed").write_text("community-test-1234abcd\n")
            record = invariants.collect("after", str(arc), None)
        for key in ("schema", "label", "guest_epoch", "boot_id", "legacy_snapshot_sha256", "legacy_entries", "v07_seed_sha256", "unit_files", "updater_timer",
                    "installed", "node", "node_info", "health", "bridge_state", "compute_consent", "community_status"):
            self.assertIn(key, record)
        self.assertNotIn("community-test-1234abcd", json.dumps(record), "the seed must never appear in the record")
        self.assertEqual(sorted(record["unit_files"]), ["arc-node.service", "arc-updater.service", "arc-updater.timer"])


if __name__ == "__main__":
    unittest.main()
