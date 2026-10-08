"""Scripted-guest tests of the consume, interruption and stop/rollback phases of host_run.py (THROWAWAY LAB FILE).

Every guest answer is scripted from the real output formats of canary-consume.sh, stat, jq and the lab scripts."""
from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import _paths  # noqa: F401
import host_run as hr

CONFIG = json.loads((_paths.LAB / "config.json").read_text(encoding="utf-8"))
PINS = json.loads((_paths.ROOT / "crates/arc-legacy-bridge/pins/active.json").read_text(encoding="utf-8"))
EXPECT = CONFIG["stage_b"]["expect_sha256"]
NODE_SHA = PINS["node_release"]["assets"]["arc-node-linux-x86_64"]["sha256"]
NODE_BYTES = PINS["node_release"]["assets"]["arc-node-linux-x86_64"]["size"]
ARC = "/home/arcw0/.arc"
PARTIAL = f"{ARC}/legacy-bridge/releases/v0.8.11/arc-node-linux-x86_64.partial"
FINAL = f"{ARC}/legacy-bridge/releases/v0.8.11/arc-node-linux-x86_64"
NODE_EXE = FINAL


def state(**overrides):
    base = {"bin_arc_node_sha256": hr.LEGACY_NODE_SHA256, "version_txt": "0.7.7", "main_pid": 10, "health_ok": True, "address": None, "bridge_node_address": None}
    base.update(overrides)
    return base


class ScriptedLab(hr.Lab):
    def __init__(self, cfg, evidence, work, script):
        super().__init__(cfg, evidence, work)
        self.script = script
        self.commands = []
        self.states = []
        self.epoch = 1_800_000_000.0

    def say(self, message):
        pass

    def guest(self, cmd, timeout=120, retries=0, log=True):
        self.commands.append(cmd)
        if log:
            self._append("commands.jsonl", {"host_epoch": 1.0, "cmd": cmd[:600], "rc": 0})
        for needle, answer in self.script:
            if needle in cmd:
                return answer(cmd) if callable(answer) else answer
        return hr.Result(0, "")

    def guest_epoch(self):
        self.epoch += 1
        return self.epoch

    def capture(self):
        return self.states.pop(0)

    def wait_health(self, seconds):
        return True

    def get_file(self, remote, local, timeout=300):
        local.parent.mkdir(parents=True, exist_ok=True)
        local.write_text("{}\n")
        return True


def make(script, source="artifact", live="allowed", profile="full"):
    tmp = tempfile.TemporaryDirectory()
    cfg = json.loads(json.dumps(CONFIG))
    cfg["stage_b"]["profile"] = profile  # independent of whichever profile the shipped config.json currently selects
    cfg["stage_b"]["launcher_source"] = source
    cfg["stage_b"]["live_network"] = live
    lab = ScriptedLab(cfg, Path(tmp.name) / "evidence", Path(tmp.name) / "work", script)
    lab.launcher_bytes = 2663184
    return lab, tmp


def checks_of(lab):
    path = lab.evidence / "checks-live.jsonl"
    return {json.loads(line)["id"]: json.loads(line) for line in path.read_text().splitlines()} if path.exists() else {}


def events_of(lab):
    path = lab.evidence / "events.jsonl"
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


ROLLED_BACK = (
    "APPLY_BEGIN 1790000000.1 mode=artifact tag=v0.7.12 dry=no\nCanary plan for /home/arcw0/.arc (currently v0.7.7)\n"
    "service restarted; waiting 30 s like the v0.7 updater\nnot healthy after 30 s: rolling back exactly like the v0.7 updater\n"
    "rolled back to v0.7.7; see /home/arcw0/.arc/node.log\nAPPLY_END 1790000041.9 rc=1\n"
)
APPLIED = "APPLY_BEGIN 1790000100.1 mode=artifact tag=v0.7.12 dry=no\nservice restarted; waiting 30 s like the v0.7 updater\nhealthy on port 9944. Bridge status:\nAPPLY_END 1790000132.0 rc=0\n"


class Apply1Tests(unittest.TestCase):
    def script(self, apply_output=ROLLED_BACK, stat_output=None, partial=12_345_678):
        stat_output = stat_output if stat_output is not None else f"{PARTIAL} {partial}\nstat: cannot statx '{FINAL}': No such file or directory\n"
        return [
            ("interrupt.sh arm", hr.Result(0, "armed")),
            ("apply.sh", hr.Result(1, apply_output)),
            ("interrupt.sh counters", hr.Result(0, "14012345\n")),
            ("interrupt.sh disarm", hr.Result(0, "disarmed")),
            ("stat -c", hr.Result(0, stat_output)),
        ]

    def run_phase(self, script, after=None):
        lab, tmp = make(script)
        self.addCleanup(tmp.cleanup)
        lab.states = [state(), after or state()]
        try:
            lab.phase_apply1()
        except hr.Fatal:
            pass
        return lab

    def test_interrupted_and_rolled_back(self):
        lab = self.run_phase(self.script())
        check = checks_of(lab)["L03-interrupt-took-effect"]
        self.assertEqual(check["result"], "PASS", check)
        event = next(e for e in events_of(lab) if e["name"] == "apply1")
        self.assertTrue(event["forced"])
        self.assertEqual(event["detail"]["rc"], 1)
        arm = next(c for c in lab.commands if "interrupt.sh arm" in c)
        quota = int(arm.rsplit(" ", 1)[1])
        self.assertEqual(quota, hr.interrupt_quota(0, NODE_BYTES), "artifact mode: the launcher is a local copy, so it adds no inbound bytes")

    def test_published_mode_counts_the_launcher_download(self):
        lab, tmp = make(self.script(), source="published")
        self.addCleanup(tmp.cleanup)
        lab.states = [state(), state()]
        lab.phase_apply1()
        arm = next(c for c in lab.commands if "interrupt.sh arm" in c)
        self.assertEqual(int(arm.rsplit(" ", 1)[1]), hr.interrupt_quota(2663184, NODE_BYTES))

    def test_the_download_finished_anyway(self):
        lab = self.run_phase(self.script(apply_output=ROLLED_BACK.replace("rc=1", "rc=0"), stat_output=f"{FINAL} {NODE_BYTES}\n"))
        self.assertEqual(checks_of(lab)["L03-interrupt-took-effect"]["result"], "FAIL")

    def test_no_partial_left(self):
        lab = self.run_phase(self.script(stat_output=f"stat: cannot statx '{PARTIAL}': No such file or directory\n"))
        self.assertEqual(checks_of(lab)["L03-interrupt-took-effect"]["result"], "FAIL")

    def test_not_rolled_back_to_v077(self):
        lab = self.run_phase(self.script(), after=state(bin_arc_node_sha256="x" * 64, version_txt="0.7.12"))
        self.assertEqual(checks_of(lab)["L03-interrupt-took-effect"]["result"], "FAIL")

    def test_watch_mode_is_started_in_the_background_before_the_apply(self):
        lab, tmp = make(self.script())
        self.addCleanup(tmp.cleanup)
        lab.interrupt_mode = "watch"
        lab.states = [state(), state()]
        lab.phase_apply1()
        watch = next(i for i, c in enumerate(lab.commands) if "interrupt.sh watch" in c)
        apply = next(i for i, c in enumerate(lab.commands) if "apply.sh" in c)
        self.assertLess(watch, apply)
        self.assertIn("nohup", lab.commands[watch])


class Apply2Tests(unittest.TestCase):
    def script(self, apply_outputs=None, counters=9_000_000, node_sha=NODE_SHA, partial_exists=False, readlink=NODE_EXE):
        listings = iter(["start-001.json\n", "start-001.json start-002.json start-003.json\n", "start-001.json start-002.json start-003.json\n"])
        outputs = iter(apply_outputs or [APPLIED])
        sample = json.dumps({"seq": 40, "epoch": 1_800_000_150.0, "health_ok": True, "node_exe": NODE_EXE, "address": "ab" * 32, "legacy_fingerprint": "f" * 64, "public_name": "node-abababab"})
        return [
            ("ls /var/lib/arc-w0/snapshots", lambda cmd: hr.Result(0, next(listings))),
            ("interrupt.sh counters", hr.Result(0, f"{counters}\n")),
            ("interrupt.sh count", hr.Result(0, "counting")),
            ("apply.sh", lambda cmd: hr.Result(0, next(outputs))),
            ("interrupt.sh disarm", hr.Result(0, "disarmed")),
            ("sha256sum", hr.Result(0, node_sha + "\n")),
            ("test -e", hr.Result(0 if partial_exists else 1, "")),
            ("readlink /proc/", hr.Result(0, readlink + "\n")),
            ("disable --now arc-w0-live-block", hr.Result(0, "open    192.0.2.1\nopen    192.0.2.2\n")),
            ("jq -c --argjson since", hr.Result(0, sample + "\n")),
            ("jq -r .binary_sha256", lambda cmd: hr.Result(0, (EXPECT if ("start-003" in cmd or "start-002" in cmd) else "0" * 64) + "\n")),
            ("invariants.py collect", hr.Result(0, "wrote")),
        ]

    def run_phase(self, script, attempts_state=None):
        lab, tmp = make(script)
        self.addCleanup(tmp.cleanup)
        lab.partial_bytes = 12_345_678
        lab.states = [attempts_state or state(bin_arc_node_sha256=EXPECT, version_txt="0.7.12")]
        with mock.patch.object(hr.time, "sleep"):
            try:
                lab.phase_apply2()
            except hr.Fatal:
                pass
        return lab

    def test_resume_succeeds_and_t0_is_recorded(self):
        lab = self.run_phase(self.script())
        checks = checks_of(lab)
        self.assertEqual(checks["L04-interrupt-resumes"]["result"], "PASS", checks["L04-interrupt-resumes"])
        names = [e["name"] for e in events_of(lab)]
        for name in ("apply2", "attempts_after_interrupt", "live_network_open", "t0_bridged_healthy", "before_invariants"):
            self.assertIn(name, names)
        self.assertLess(names.index("live_network_open"), names.index("t0_bridged_healthy"))
        t0 = next(e for e in events_of(lab) if e["name"] == "t0_bridged_healthy")
        self.assertEqual(t0["detail"]["address"], "ab" * 32)
        self.assertEqual(lab.t0_address, "ab" * 32)
        before = next(c for c in lab.commands if "invariants.py collect --label before" in c)
        self.assertIn("--legacy-from /var/lib/arc-w0/snapshots/start-003.json", before, "the kept run is the last new snapshot carrying the launcher digest")
        self.assertIn("authorization", next(e for e in events_of(lab) if e["name"] == "live_network_open")["detail"])

    def test_a_blocked_run_never_lifts_the_block(self):
        lab, tmp = make(self.script(), live="blocked")
        self.addCleanup(tmp.cleanup)
        lab.partial_bytes = 12_345_678
        lab.states = [state(bin_arc_node_sha256=EXPECT, version_txt="0.7.12")]
        with mock.patch.object(hr.time, "sleep"):
            lab.phase_apply2()
        self.assertFalse(any("disable --now arc-w0-live-block" in c for c in lab.commands))
        self.assertNotIn("live_network_open", [e["name"] for e in events_of(lab)])
        self.assertIn("t0_bridged_healthy", [e["name"] for e in events_of(lab)])

    def test_the_network_opens_only_after_the_apply(self):
        lab = self.run_phase(self.script())
        apply = next(i for i, c in enumerate(lab.commands) if "apply.sh" in c)
        opened = next(i for i, c in enumerate(lab.commands) if "disable --now arc-w0-live-block" in c)
        self.assertLess(apply, opened, "the v0.7.7 process must be gone before the live network opens")

    def test_refuses_to_open_the_network_while_v077_still_runs(self):
        lab = self.run_phase(self.script(readlink=f"{ARC}/bin/arc-node"))
        self.assertNotIn("live_network_open", [e["name"] for e in events_of(lab)])
        self.assertNotIn("t0_bridged_healthy", [e["name"] for e in events_of(lab)])

    def test_a_restart_from_zero_is_not_a_resume(self):
        lab = self.run_phase(self.script(counters=hr.resume_bound(0, NODE_BYTES + 4_622_792, 12_345_678) + 5_000_000))
        self.assertEqual(checks_of(lab)["L04-interrupt-resumes"]["result"], "FAIL")

    def test_partial_left_behind_fails(self):
        lab = self.run_phase(self.script(partial_exists=True))
        self.assertEqual(checks_of(lab)["L04-interrupt-resumes"]["result"], "FAIL")

    def test_wrong_node_digest_fails(self):
        lab = self.run_phase(self.script(node_sha="0" * 64))
        self.assertEqual(checks_of(lab)["L04-interrupt-resumes"]["result"], "FAIL")

    def test_retries_a_slow_first_resume_then_succeeds(self):
        failed = APPLIED.replace("rc=0", "rc=1").replace("healthy on port 9944", "not healthy after 30 s")
        lab = self.run_phase(self.script(apply_outputs=[failed, APPLIED]))
        attempts = next(e for e in events_of(lab) if e["name"] == "attempts_after_interrupt")["detail"]["attempts"]
        self.assertEqual([a["rc"] for a in attempts], [1, 0])
        self.assertEqual(checks_of(lab)["L04-interrupt-resumes"]["result"], "PASS")

    def test_three_failures_are_fatal(self):
        failed = APPLIED.replace("rc=0", "rc=1")
        lab = self.run_phase(self.script(apply_outputs=[failed, failed, failed]))
        self.assertEqual(checks_of(lab)["L04-interrupt-resumes"]["result"], "FAIL")


class FinalPhaseTests(unittest.TestCase):
    def script(self, stop="state=inactive\nprocs=0\nrpc=closed\n", rollback=None, rebridge=None, compute="compute_rc=78\nvalidator_rc=78\nls: cannot access\n"):
        rollback = rollback or f"rc=0\n{hr.LEGACY_NODE_SHA256}\n"
        rebridge = rebridge or f"{EXPECT}\n"
        addresses = iter(["aa\n", "aa\n", "aa\n"])
        return [
            ("oom_scan.py", hr.Result(0, json.dumps({"schema": "arc.legacy-bridge.wave0-lab.kernel-oom.v1", "scans": [{"boot": 0, "count": 0, "lines": []}], "total": 0}) + "\n")),
            ("invariants.py collect --label after", hr.Result(0, "wrote")),
            ("--legacy-bridge-compute on", hr.Result(0, compute)),
            ("jq -r .node_address", lambda cmd: hr.Result(0, next(addresses))),
            ("systemctl stop arc-node; sleep 2", hr.Result(0, stop)),
            ("--legacy-bridge-rollback", hr.Result(0, rollback)),
            ("systemctl start arc-node", hr.Result(0, "")),
            ("readlink /proc/", hr.Result(0, f"{ARC}/bin/arc-node\n")),
            ("arc-node-bridge-", hr.Result(0, rebridge)),
            ("systemctl is-active arc-node", hr.Result(0, "inactive\n")),
        ]

    def run_phase(self, script):
        lab, tmp = make(script)
        self.addCleanup(tmp.cleanup)
        lab.phase_final()
        return lab

    def test_stop_rollback_rebridge(self):
        lab = self.run_phase(self.script())
        checks = checks_of(lab)
        self.assertEqual(checks["L11-stop-rollback"]["result"], "PASS", checks["L11-stop-rollback"])
        self.assertEqual(checks["L14-compute-refusal"]["result"], "PASS")
        names = [e["name"] for e in events_of(lab)]
        self.assertEqual(names, ["kernel_oom_scan", "final_stop_begin", "node_stopped"])
        oom = json.loads((lab.evidence / "kernel-oom.json").read_text())
        self.assertEqual(oom["total"], 0)

    def test_final_stop_begin_precedes_every_stop_and_rollback_command(self):
        lab = self.run_phase(self.script())
        begin = next(e for e in events_of(lab) if e["name"] == "final_stop_begin")
        self.assertTrue(begin["forced"])
        first_stop = next(i for i, c in enumerate(lab.commands) if "arc-w0-live-block" in c or "systemctl stop arc-node" in c)
        before_stop = lab.commands[:first_stop]
        for command in before_stop:
            self.assertNotRegex(command, r"systemctl\s+(start|restart)\b[^;&|]*arc-node|kickstart|canary-consume|legacy-bridge-rollback")

    def test_unclean_stop_fails(self):
        lab = self.run_phase(self.script(stop="state=active\nprocs=0\nrpc=open\n"))
        self.assertEqual(checks_of(lab)["L11-stop-rollback"]["result"], "FAIL")

    def test_a_leftover_node_process_fails_the_stop(self):
        lab = self.run_phase(self.script(stop="state=inactive\nprocs=1\nrpc=closed\n"))
        self.assertEqual(checks_of(lab)["L11-stop-rollback"]["result"], "FAIL")

    def test_the_stop_command_cannot_match_its_own_shell(self):
        lab = self.run_phase(self.script())
        stop = next(c for c in lab.commands if "systemctl stop arc-node; sleep 2" in c)
        self.assertNotIn("pgrep", stop)
        self.assertIn("count_nodes.py", stop)

    def test_rollback_that_does_not_restore_v077_fails(self):
        lab = self.run_phase(self.script(rollback="rc=0\n" + "0" * 64 + "\n"))
        self.assertEqual(checks_of(lab)["L11-stop-rollback"]["result"], "FAIL")

    def test_rebridge_with_other_bytes_fails(self):
        lab = self.run_phase(self.script(rebridge="0" * 64 + "\n"))
        self.assertEqual(checks_of(lab)["L11-stop-rollback"]["result"], "FAIL")

    def test_compute_not_refused_fails(self):
        lab = self.run_phase(self.script(compute="compute_rc=0\nvalidator_rc=78\n"))
        self.assertEqual(checks_of(lab)["L14-compute-refusal"]["result"], "FAIL")


class ScoreboardAndSupplementTests(unittest.TestCase):
    ADDRESS = "ab" * 32

    def fake_lookup(self, found_on=(0, 1, 2, 3, 4, 5), name=None):
        def lookup(origin, address, opener=None):
            index = self.lab.origins.index(origin)
            if index in found_on:
                row = {"worker_id": "0x" + address, "name": name or "node-" + address[:8], "registered_at": 1791449000}
                return {"http": 200, "error": None, "found": True, "name": row["name"], "registered_at": 1791449000, "worker_id": "0x" + address,
                        "server_date": "Thu, 08 Oct 2026 14:00:00 GMT", "count_total": 7, "row": row, "elapsed_ms": 40}
            return {"http": 200, "error": None, "found": False, "name": None, "registered_at": None, "worker_id": None,
                    "server_date": "Thu, 08 Oct 2026 14:00:00 GMT", "count_total": 6, "row": None, "elapsed_ms": 40}
        return lookup

    def make_lab(self, live="allowed"):
        lab, tmp = make([], live=live)
        self.addCleanup(tmp.cleanup)
        lab.t0_address = self.ADDRESS
        self.lab = lab
        return lab

    def test_probe_records_the_matrix_without_the_origin_addresses(self):
        lab = self.make_lab()
        with mock.patch.object(hr, "scoreboard_lookup", self.fake_lookup(found_on=(0, 2, 5))):
            record = lab.scoreboard_probe()
        self.assertEqual(record["found_count"], 3)
        self.assertEqual(len(record["results"]), 6)
        line = (lab.evidence / "scoreboard.jsonl").read_text().strip()
        row = json.loads(line)
        self.assertEqual(row["probe"], 1)
        self.assertEqual(row["address"], self.ADDRESS)
        self.assertEqual([r["origin"] for r in row["results"]], [0, 1, 2, 3, 4, 5])
        for origin in lab.origins:
            self.assertNotIn(origin, line, "the evidence must carry an index and a short hash, never the address of a coordinator")
        self.assertEqual(len({r["origin_sha8"] for r in row["results"]}), 6)
        self.assertTrue(all(r["server_date"] and r["count_total"] is not None for r in row["results"]), "every answer keeps the server's own time and table size")
        self.assertEqual(row["results"][0]["row"]["name"], "node-" + self.ADDRESS[:8], "our row itself is kept, not only a boolean")
        self.assertIsNone(row["results"][1]["row"])
        self.assertLessEqual(row["host_epoch"], row["finished_epoch"])

    def test_probe_rounds_read_all_coordinators_in_parallel(self):
        lab = self.make_lab()
        started = []

        def slow(origin, address, opener=None):
            started.append(__import__("time").monotonic())
            __import__("time").sleep(0.3)
            return self.fake_lookup()(origin, address)

        began = __import__("time").monotonic()
        with mock.patch.object(hr, "scoreboard_lookup", slow):
            lab.scoreboard_probe()
        took = __import__("time").monotonic() - began
        self.assertEqual(len(started), 6)
        self.assertLess(max(started) - min(started), 0.25, "all six reads start together")
        self.assertLess(took, 1.2, "a round of six 0.3 s reads must not take 1.8 s one after another")

    def test_lookup_parses_the_public_shape_and_never_calls_community_list(self):
        seen = []

        class Response:
            status = 200
            headers = {"Date": "Thu, 08 Oct 2026 14:00:07 GMT", "Content-Type": "application/json"}

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

            def read(self, n=-1):
                return json.dumps({"workers": [{"worker_id": "0x" + "ab" * 32, "name": "node-abababab", "registered_at": 7}, {"worker_id": "0x" + "cd" * 32, "name": "node-cdcdcdcd"}], "count_total": 2}).encode()

        def opener(request, timeout=None):
            seen.append(request.full_url)
            return Response()

        found = hr.scoreboard_lookup("https://203.0.113.9", "ab" * 32, opener=opener)
        self.assertTrue(found["found"])
        self.assertEqual(found["name"], "node-abababab")
        self.assertEqual(found["server_date"], "Thu, 08 Oct 2026 14:00:07 GMT")
        self.assertEqual(found["count_total"], 2)
        self.assertEqual(found["row"], {"worker_id": "0x" + "ab" * 32, "name": "node-abababab", "registered_at": 7})
        self.assertIsInstance(found["elapsed_ms"], int)
        self.assertEqual(seen, ["https://203.0.113.9/workers/scoreboard?limit=5&worker_id=0x" + "ab" * 32])
        self.assertNotIn("community", seen[0])
        missing = hr.scoreboard_lookup("https://203.0.113.9", "ee" * 32, opener=opener)
        self.assertFalse(missing["found"])
        self.assertIsNone(missing["row"])
        self.assertEqual(missing["count_total"], 2, "an absent row still tells how big the table was at that server time")
        self.assertEqual(missing["server_date"], "Thu, 08 Oct 2026 14:00:07 GMT")

    def test_lookup_failure_is_a_recorded_miss(self):
        def opener(request, timeout=None):
            raise TimeoutError("slow")

        result = hr.scoreboard_lookup("https://203.0.113.9", "ab" * 32, opener=opener)
        self.assertFalse(result["found"])
        self.assertEqual(result["error"], "TimeoutError")
        self.assertIsNone(result["server_date"])
        self.assertIsNone(result["row"])

    def test_http_errors_keep_their_status_and_server_date(self):
        import email.message
        import urllib.error

        def opener(request, timeout=None):
            headers = email.message.Message()
            headers["Date"] = "Thu, 08 Oct 2026 14:01:00 GMT"
            raise urllib.error.HTTPError(request.full_url, 503, "unavailable", headers, None)

        result = hr.scoreboard_lookup("https://203.0.113.9", "ab" * 32, opener=opener)
        self.assertEqual((result["http"], result["error"], result["found"]), (503, "HTTPError", False))
        self.assertEqual(result["server_date"], "Thu, 08 Oct 2026 14:01:00 GMT")

    def test_garbage_bodies_are_a_miss_not_a_crash(self):
        class Response:
            status = 200
            headers = {}

            def __init__(self, body):
                self.body = body

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

            def read(self, n=-1):
                return self.body

        for body in (b"not json", b"[]", b'{"workers": "x"}', b'{"workers": [1, null, "a"], "count_total": "7"}'):
            with self.subTest(body=body):
                result = hr.scoreboard_lookup("https://203.0.113.9", "ab" * 32, opener=lambda request, timeout=None, b=body: Response(b))
                self.assertFalse(result["found"])
                self.assertIsNone(result["row"])
                self.assertIsNone(result["count_total"], "a count that is not an integer is not recorded")

    def test_the_prober_keeps_a_fixed_rate_even_when_a_round_is_slow(self):
        lab = self.make_lab()
        lab.scoreboard_interval = 30
        lab.probe_margin_s = 0
        lab.samples_tail = lambda count=5: [{"coordinators_registered": 6}]
        waits = []

        def spy(timeout=None):
            waits.append(timeout)
            return len(waits) >= 3  # stop after three rounds

        original = lab.scoreboard_probe

        def slow_round():
            __import__("time").sleep(0.2)
            return original()

        lab.probe_stop.wait = spy
        lab.scoreboard_probe = slow_round
        with mock.patch.object(hr, "scoreboard_lookup", self.fake_lookup()):
            lab.start_scoreboard()
            lab.probe_thread.join(timeout=10)
        self.assertEqual(len(waits), 3)
        self.assertEqual(waits[0], 0, "the margin wait before the first round")
        self.assertTrue(29.0 < waits[1] < 29.95, f"the wait after round 1 is the rest of the 30 s interval, not a fresh full interval: {waits[1]}")

    def test_prober_thread_probes_now_then_on_the_interval_and_stops(self):
        lab = self.make_lab()
        lab.scoreboard_interval = 0.05
        lab.probe_margin_s = 0
        lab.samples_tail = lambda count=5: [{"coordinators_registered": 6}]
        with mock.patch.object(hr, "scoreboard_lookup", self.fake_lookup()):
            lab.start_scoreboard()
            deadline = __import__("time").time() + 3
            while lab.probe_count < 3 and __import__("time").time() < deadline:
                __import__("time").sleep(0.02)
            lab.stop_scoreboard()
        self.assertGreaterEqual(lab.probe_count, 3)
        count = lab.probe_count
        __import__("time").sleep(0.2)
        self.assertEqual(lab.probe_count, count, "no probe after stop")

    def test_the_first_public_read_waits_for_the_nodes_own_registration(self):
        lab = self.make_lab()
        lab.scoreboard_interval = 3600
        lab.probe_margin_s = 0
        answers = iter([[{"coordinators_registered": 0}], [{"coordinators_registered": 0}], [{"coordinators_registered": 6}]])
        calls = {"n": 0}

        def tail(count=5):
            calls["n"] += 1
            return next(answers, [{"coordinators_registered": 6}])

        lab.samples_tail = tail
        real_wait = lab.probe_stop.wait
        lab.probe_stop.wait = lambda timeout=None: real_wait(0.001)
        with mock.patch.object(hr, "scoreboard_lookup", self.fake_lookup()):
            lab.start_scoreboard()
            deadline = __import__("time").time() + 3
            while lab.probe_count < 1 and __import__("time").time() < deadline:
                __import__("time").sleep(0.01)
            lab.stop_scoreboard()
        self.assertGreaterEqual(calls["n"], 3, "two unregistered samples were seen before the first public read")
        self.assertGreaterEqual(lab.probe_count, 1)

    def test_no_prober_when_the_live_network_is_blocked(self):
        lab = self.make_lab(live="blocked")
        lab.start_scoreboard()
        self.assertIsNone(lab.probe_thread)

    def test_kernel_oom_scan_writes_the_evidence_file(self):
        lab, tmp = make([("oom_scan.py", hr.Result(0, 'noise\n{"schema":"arc.legacy-bridge.wave0-lab.kernel-oom.v1","scans":[{"boot":-1,"count":1,"lines":["x"]},{"boot":0,"count":0,"lines":[]}],"total":1}\n'))])
        self.addCleanup(tmp.cleanup)
        lab.kernel_oom_scan()
        data = json.loads((lab.evidence / "kernel-oom.json").read_text())
        self.assertEqual(data["total"], 1)
        self.assertIn("sudo", lab.commands[0])

    def test_kernel_oom_scan_records_failure_to_scan(self):
        lab, tmp = make([("oom_scan.py", hr.Result(1, "journalctl: command not found"))])
        self.addCleanup(tmp.cleanup)
        lab.kernel_oom_scan()
        data = json.loads((lab.evidence / "kernel-oom.json").read_text())
        self.assertIsNone(data["total"])
        self.assertIn("error", data)

    def test_effective_config_carries_the_criteria(self):
        cfg = json.loads(json.dumps(CONFIG))
        cfg["stage_b"]["profile"] = "full"
        cfg["stage_b"]["live_network"] = "allowed"
        eff = hr.effective_config(cfg, PINS)
        self.assertTrue(eff["resources_required"])
        self.assertTrue(eff["scoreboard_required"])
        self.assertEqual(eff["scoreboard_interval_s"], CONFIG["stage_b"]["scoreboard_interval_s"])
        self.assertLessEqual(eff["scoreboard_interval_s"], 60)
        self.assertTrue(eff["battery"])
        self.assertEqual(eff["vm_memory_mb"], 4096)
        self.assertEqual(eff["resource_bounds"]["min_points"], 121)
        self.assertEqual(eff["resource_bounds"]["rss_slope_mib_per_h_max"], 100)
        cfg["stage_b"]["live_network"] = "blocked"
        self.assertFalse(hr.effective_config(cfg, PINS)["scoreboard_required"])

    def test_supplement_profile_skips_the_battery_and_needs_three_live_ids(self):
        cfg = json.loads(json.dumps(CONFIG))
        cfg["stage_b"]["profile"] = "resources"
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        lab = ScriptedLab(cfg, Path(tmp.name) / "evidence", Path(tmp.name) / "work", [])
        self.assertFalse(lab.battery)
        self.assertEqual(lab.required_ids, ("L01-kvm", "L02-consume-dry-run", "L11-stop-rollback"))
        eff = hr.effective_config(cfg, PINS)
        self.assertFalse(eff["battery"])
        self.assertEqual(eff["settle_s"], 600)
        self.assertEqual(eff["profile"], "resources")

    def test_supplement_steady_waits_for_the_settle_and_labels_the_last_forced_event(self):
        cfg = json.loads(json.dumps(CONFIG))
        cfg["stage_b"]["profile"] = "resources"
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        lab = ScriptedLab(cfg, Path(tmp.name) / "evidence", Path(tmp.name) / "work", [])
        lab.t0_guest_epoch = lab.epoch
        self.assertIsNone(lab.last_forced_end_guest, "the supplement has no reboot: nothing sets it before the steady phase (the real situation)")
        lab.pull_samples = lambda: None
        lab.samples_tail = lambda count=5: []
        with mock.patch.object(hr.time, "sleep"):
            lab.phase_steady()
        events = {e["name"]: e for e in events_of(lab)}
        begin = events["steady_begin"]
        self.assertEqual(begin["detail"]["last_forced_event"], "apply2")
        self.assertGreaterEqual(begin["guest_epoch"], lab.t0_guest_epoch + lab.settle_s)
        self.assertNotIn("reboot_issued", events)
        profile = lab.profile
        self.assertEqual(
            begin["detail"]["target_guest_epoch"],
            lab.t0_guest_epoch + max(lab.settle_s + profile["min_steady_s"], profile["min_total_s"]) + 3 * profile["sample_interval_s"],
            "the supplement runs three sample intervals past the nominal end so first-to-last is >= min_steady_s whatever the sampler phase",
        )
        self.assertGreaterEqual(profile["min_steady_s"], 8100)

    def test_the_full_profile_keeps_its_two_interval_margin(self):
        lab, tmp = make([])
        self.addCleanup(tmp.cleanup)
        lab.t0_guest_epoch = lab.epoch
        lab.last_forced_end_guest = lab.epoch + 100
        lab.pull_samples = lambda: None
        lab.samples_tail = lambda count=5: []
        with mock.patch.object(hr.time, "sleep"):
            lab.phase_steady()
        begin = next(e for e in events_of(lab) if e["name"] == "steady_begin")
        profile = lab.profile
        self.assertEqual(
            begin["detail"]["target_guest_epoch"],
            max(lab.last_forced_end_guest + profile["min_steady_s"], lab.t0_guest_epoch + profile["min_total_s"]) + 2 * profile["sample_interval_s"],
        )

    def test_pull_samples_also_pulls_the_heartbeat_log(self):
        lab, tmp = make([])
        self.addCleanup(tmp.cleanup)
        asked = []
        lab.get_file = lambda remote, local, timeout=300: asked.append((remote, local.name)) or True
        lab.pull_samples()
        self.assertEqual(asked, [("/var/lib/arc-w0/samples.jsonl", "samples.jsonl"), ("/var/lib/arc-w0/heartbeats.jsonl", "heartbeats.jsonl")])


class SetupPhaseTests(unittest.TestCase):
    def test_vm_phase_builds_the_expected_commands(self):
        lab, tmp = make([("cloud-init status", hr.Result(0, "status: done")), ("test -f", hr.Result(0, "")), ("uname -a", hr.Result(0, "Linux arc-wave0 6.8")),
                         ("MemTotal", hr.Result(0, "MemTotal:        3921412 kB\nSwapTotal:             0 kB\n"))])
        self.addCleanup(tmp.cleanup)
        base = lab.work / "base.img"
        base.write_bytes(b"image")
        lab.sb["image"]["size"] = 5
        lab.sb["image"]["sha256"] = hr.sha256_file(base)
        calls = []

        def fake_run(argv, timeout=600, check=True, **kwargs):
            calls.append(list(argv))
            if argv[0] == "ssh-keygen":
                Path(argv[-1]).write_text("private")
                Path(argv[-1] + ".pub").write_text("ssh-ed25519 AAAA test@lab\n")
            return subprocess.CompletedProcess(argv, 0, b"", None)

        with mock.patch.object(lab, "run_process", side_effect=fake_run), mock.patch.object(hr.time, "sleep"):
            lab.phase_vm()
        names = [call[0] for call in calls]
        self.assertEqual(names, ["ssh-keygen", "qemu-img", "genisoimage", "qemu-system-x86_64"])
        self.assertEqual(checks_of(lab)["L17-vm-image-digest"]["result"], "PASS")
        user_data = (lab.work / "seed" / "user-data").read_text()
        self.assertIn("ssh-ed25519 AAAA test@lab", user_data)
        self.assertEqual([e["name"] for e in events_of(lab)], ["vm_started", "vm_ready", "vm_memory"])
        memory = next(e for e in events_of(lab) if e["name"] == "vm_memory")["detail"]
        self.assertEqual(memory["configured_mb"], lab.sb["vm"]["memory_mb"])
        self.assertEqual(memory["mem_total_kb"], 3921412)

    def test_ship_phase_measures_the_installer_the_tag_commit_and_the_published_launcher(self):
        import hashlib

        launcher_bytes = b"the published launcher"
        installer_bytes = b"#!/bin/sh\n# v0.7.11 installer\n"
        lab, tmp = make([], source="published")
        self.addCleanup(tmp.cleanup)
        lab.expect = hashlib.sha256(launcher_bytes).hexdigest()
        release = {
            "tag_name": "v0.7.12", "id": 1, "draft": False, "prerelease": False, "immutable": True, "target_commitish": "main", "published_at": "2026-10-08T08:13:00Z",
            "assets": [{"name": "arc-node-linux-x86_64", "size": len(launcher_bytes), "digest": "sha256:" + lab.expect}],
        }
        calls = []

        def fake_run(argv, timeout=600, check=True, **kwargs):
            calls.append(list(argv))
            out = b""
            if argv[:2] == ["git", "-C"] and "show" in argv:
                out = installer_bytes if argv[-1].endswith("install-community-node.sh") else b"other source file\n"
            elif "rev-parse" in argv:
                out = b"d60632afdf38c1d5671bff463ae6f9c278d84860\n"
            elif argv[0] == "curl":
                Path(argv[argv.index("-o") + 1]).write_bytes(launcher_bytes)
            return subprocess.CompletedProcess(argv, 0, out, None)

        with mock.patch.object(lab, "run_process", side_effect=fake_run), mock.patch.object(lab, "put_tree"), \
                mock.patch.object(hr.fetch_handoff, "run_gh", return_value=json.dumps(release).encode()), \
                mock.patch.object(hr.live_ips, "load", return_value=["192.0.2.1"]):
            lab.phase_ship()
        self.assertEqual(lab.measured["installer_sha256"], hashlib.sha256(installer_bytes).hexdigest())
        self.assertEqual(lab.measured["legacy_tag_commit"], "d60632afdf38c1d5671bff463ae6f9c278d84860")
        self.assertEqual(lab.measured["launcher_sha256"], lab.expect)
        self.assertEqual(checks_of(lab)["L15-published-release"]["result"], "PASS")
        shipped = lab.work / "ship" / "legacy-source" / "install-community-node.sh"
        self.assertEqual(shipped.read_bytes(), installer_bytes, "the guest installs exactly the bytes that were hashed")
        for name in ("heartbeat_poller.py", "oom_scan.py", "sampler.py", "probe.py"):
            self.assertTrue((lab.work / "ship" / "lab" / name).is_file(), name)

    def test_ship_phase_refuses_a_published_launcher_with_other_bytes(self):
        import hashlib

        lab, tmp = make([], source="published")
        self.addCleanup(tmp.cleanup)
        good = b"expected launcher"
        lab.expect = hashlib.sha256(good).hexdigest()
        release = {"tag_name": "v0.7.12", "assets": [{"name": "arc-node-linux-x86_64", "size": 5, "digest": "sha256:" + lab.expect}]}

        def fake_run(argv, timeout=600, check=True, **kwargs):
            if argv[0] == "curl":
                Path(argv[argv.index("-o") + 1]).write_bytes(b"tampered")
            return subprocess.CompletedProcess(argv, 0, b"x", None)

        with mock.patch.object(lab, "run_process", side_effect=fake_run), mock.patch.object(lab, "put_tree"), \
                mock.patch.object(hr.fetch_handoff, "run_gh", return_value=json.dumps(release).encode()), \
                mock.patch.object(hr.live_ips, "load", return_value=["192.0.2.1"]):
            with self.assertRaises(hr.Fatal):
                lab.phase_ship()
        self.assertEqual(checks_of(lab)["L15-published-release"]["result"], "FAIL")
        self.assertNotEqual(lab.measured["launcher_sha256"], lab.expect, "the measured value is the tampered file's, so the binding would also differ")

    def test_vm_phase_refuses_a_wrong_image(self):
        lab, tmp = make([])
        self.addCleanup(tmp.cleanup)
        (lab.work / "base.img").write_bytes(b"not the image")
        with self.assertRaises(hr.Fatal):
            lab.phase_vm()
        self.assertEqual(checks_of(lab)["L17-vm-image-digest"]["result"], "FAIL")

    def test_baseline_phase_installs_the_block_first_and_checks_the_sampler(self):
        sample = json.dumps({"seq": 0})
        lab, tmp = make([
            ("install-units.sh --units live-block", hr.Result(0, "blocked 192.0.2.1")),
            ("baseline.sh", hr.Result(0, "PASS: stranded")),
            ("install-units.sh --units sampler", hr.Result(0, "active")),
            ("tail -n 1", hr.Result(0, sample + "\n")),
            ("wc -l <", hr.Result(0, "12\n")),
            ("sha256sum /home/arcw0/.arc/bin/arc-node", hr.Result(0, hr.LEGACY_NODE_SHA256 + "\n")),
            ("baseline-result.txt", hr.Result(0, "installer=v0.7.11 legacy_node=v0.7.7 bridge_tag=v0.7.12 legacy_node_sha256=" + hr.LEGACY_NODE_SHA256 + " v07_pid=4321\n")),
            ("interrupt.sh selftest", hr.Result(0, "quota match works")),
        ])
        self.addCleanup(tmp.cleanup)
        with mock.patch.object(hr.time, "sleep"):
            lab.phase_baseline()
        order = [c for c in lab.commands if "install-units.sh" in c or "baseline.sh" in c]
        self.assertIn("--units live-block", order[0])
        self.assertIn("baseline.sh", order[1])
        self.assertIn("--units sampler", order[2])
        self.assertEqual(lab.interrupt_mode, "quota")
        checks = checks_of(lab)
        self.assertEqual(checks["L13-baseline"]["result"], "PASS")
        self.assertEqual(checks["L20-sampler-running"]["result"], "PASS")
        self.assertEqual(checks["L21-heartbeat-poller-running"]["result"], "PASS")
        self.assertEqual(lab.measured["legacy_node_sha256"], hr.LEGACY_NODE_SHA256)
        self.assertEqual(
            lab.measured["baseline_result"],
            "installer=v0.7.11 legacy_node=v0.7.7 bridge_tag=v0.7.12 legacy_node_sha256=" + hr.LEGACY_NODE_SHA256,
            "the run-specific pid of the v0.7.7 node is not part of the baseline's identity",
        )

    def test_a_silent_heartbeat_poller_stops_the_run_before_the_consume(self):
        lab, tmp = make([
            ("install-units.sh", hr.Result(0, "ok")), ("baseline.sh", hr.Result(0, "ok")), ("tail -n 1", hr.Result(0, '{"seq": 0}\n')),
            ("wc -l <", hr.Result(0, "0\n")),
        ])
        self.addCleanup(tmp.cleanup)
        started = __import__("time").monotonic()
        with mock.patch.object(hr.time, "sleep"), self.assertRaises(hr.Fatal):
            lab.phase_baseline()
        self.assertLess(__import__("time").monotonic() - started, 5, "the wait is bounded by iterations, not by a busy loop on the wall clock")
        self.assertEqual(checks_of(lab)["L21-heartbeat-poller-running"]["result"], "FAIL")

    def test_baseline_falls_back_to_watch_mode_without_the_quota_match(self):
        lab, tmp = make([
            ("install-units.sh", hr.Result(0, "ok")), ("baseline.sh", hr.Result(0, "ok")), ("tail -n 1", hr.Result(0, '{"seq": 0}\n')),
            ("wc -l <", hr.Result(0, "5\n")),
            ("interrupt.sh selftest", hr.Result(1, "Couldn't load match `quota'")),
        ])
        self.addCleanup(tmp.cleanup)
        with mock.patch.object(hr.time, "sleep"):
            lab.phase_baseline()
        self.assertEqual(lab.interrupt_mode, "watch")

    def test_dry_run_checks_the_plan(self):
        plan = (f"APPLY_BEGIN 1.0 mode=artifact\nCanary plan for /home/arcw0/.arc (currently v0.7.7)\n  1. download https://github.com/FerrumVir/arc-chain/releases/download/v0.7.12/arc-node-linux-x86_64\n"
                f"  2. require SHA-256 {EXPECT}\nDry run only. Re-run with --apply to perform these steps.\nAPPLY_END 2.0 rc=0\n")
        lab, tmp = make([("apply.sh", hr.Result(0, plan))])
        self.addCleanup(tmp.cleanup)
        lab.phase_dry_run()
        self.assertEqual(checks_of(lab)["L02-consume-dry-run"]["result"], "PASS")
        self.assertTrue(any(c.endswith("--dry-run") for c in lab.commands))
        lab2, tmp2 = make([("apply.sh", hr.Result(0, plan.replace("Dry run only", "Applied")))])
        self.addCleanup(tmp2.cleanup)
        with self.assertRaises(hr.Fatal):
            lab2.phase_dry_run()
        self.assertEqual(checks_of(lab2)["L02-consume-dry-run"]["result"], "FAIL")

    def test_run_records_not_reached_for_every_required_check_when_the_vm_cannot_start(self):
        lab, tmp = make([])
        self.addCleanup(tmp.cleanup)
        with mock.patch.object(Path, "exists", lambda self: False if str(self) == "/dev/kvm" else Path.is_file(self) or Path.is_dir(self)), \
                mock.patch.object(lab, "collect"):
            code = lab.run()
        self.assertEqual(code, 1)
        checks = checks_of(lab)
        self.assertEqual(set(hr.REQUIRED_LIVE_IDS) - set(checks), set())
        self.assertTrue(all(checks[i]["result"] == "FAIL" for i in hr.REQUIRED_LIVE_IDS))
        self.assertIn("stopped_early", [e["name"] for e in events_of(lab)])


class BindingTests(unittest.TestCase):
    """binding.json: what this run measured next to what the committed prior_run block records for run 37750760170."""

    def lab(self):
        lab, tmp = make([], source="published", profile="resources")
        self.addCleanup(tmp.cleanup)
        return lab

    def measured_like_the_prior_run(self, lab):
        prior = lab.sb["prior_run"]
        for key in ("launcher_sha256", "node_sha256", "legacy_node_sha256", "installer_sha256", "image_sha256", "baseline_result"):
            lab.measured[key] = prior[key]
        lab.measured["units"] = dict(prior["units"])
        lab.measured["legacy_tag_commit"] = "d" * 40

    def binding(self, lab):
        return json.loads((lab.evidence / "binding.json").read_text(encoding="utf-8"))

    def test_identical_bytes_are_all_equal_and_the_memory_change_is_disclosed(self):
        lab = self.lab()
        self.measured_like_the_prior_run(lab)
        with mock.patch.dict(hr.os.environ, {"GITHUB_RUN_ID": "37760000001", "GITHUB_RUN_ATTEMPT": "1", "GITHUB_SHA": "a" * 40}):
            lab.write_binding()
        record = self.binding(lab)
        self.assertEqual(record["schema"], "arc.legacy-bridge.wave0-lab.binding.v1")
        self.assertTrue(record["all_equal"], record["equal"])
        self.assertEqual(set(record["equal"]), {"launcher_sha256", "node_sha256", "legacy_node_sha256", "installer_sha256", "image_sha256", "units", "baseline_result"})
        self.assertEqual(record["prior_run"]["run_id"], 37750760170)
        self.assertEqual(record["this_run"]["run_id"], 37760000001)
        self.assertEqual(record["this_run"]["commit"], "a" * 40)
        self.assertEqual(record["this_run"]["vm_memory_mb"], 4096)
        self.assertEqual(record["disclosed_changes"], {"vm_memory_mb": {"prior": 6144, "this": 4096}})

    def test_one_different_byte_is_not_equal(self):
        for key in ("launcher_sha256", "node_sha256", "legacy_node_sha256", "installer_sha256", "image_sha256"):
            with self.subTest(key=key):
                lab = self.lab()
                self.measured_like_the_prior_run(lab)
                lab.measured[key] = "0" * 64
                lab.write_binding()
                record = self.binding(lab)
                self.assertFalse(record["all_equal"])
                self.assertFalse(record["equal"][key])

    def test_a_measurement_that_failed_is_null_and_never_equal(self):
        lab = self.lab()
        self.measured_like_the_prior_run(lab)
        del lab.measured["node_sha256"]
        del lab.measured["units"]
        lab.write_binding()
        record = self.binding(lab)
        self.assertIsNone(record["this_run"]["node_sha256"])
        self.assertFalse(record["equal"]["node_sha256"])
        self.assertFalse(record["equal"]["units"])
        self.assertFalse(record["all_equal"])

    def test_changed_unit_files_are_not_the_same_baseline(self):
        lab = self.lab()
        self.measured_like_the_prior_run(lab)
        lab.measured["units"]["arc-updater.timer"] = "9" * 64
        lab.write_binding()
        self.assertFalse(self.binding(lab)["equal"]["units"])

    def test_unit_digests_come_from_the_collected_copies(self):
        lab = self.lab()
        folder = lab.evidence / "guest" / "units"
        folder.mkdir(parents=True)
        for name in hr.BASELINE_UNIT_FILES:
            (folder / name).write_text(f"[Unit]\nDescription={name}\n", encoding="utf-8")
        (folder / "arc-w0-sampler.service").write_text("lab observer\n", encoding="utf-8")
        lab.measure_units()
        self.assertEqual(sorted(lab.measured["units"]), sorted(hr.BASELINE_UNIT_FILES), "only the files the v0.7.11 installer wrote are compared")
        self.assertEqual(lab.measured["units"]["arc-node.service"], hr.sha256_file(folder / "arc-node.service"))

    def test_no_prior_run_block_writes_nothing_and_never_raises(self):
        lab = self.lab()
        del lab.sb["prior_run"]
        lab.write_binding()
        self.assertFalse((lab.evidence / "binding.json").exists())

    def test_the_binding_is_written_when_collect_runs_even_without_the_guest(self):
        lab = self.lab()
        self.measured_like_the_prior_run(lab)
        lab.guest = lambda cmd, timeout=120, retries=0, log=True: hr.Result(255, "")
        with mock.patch.object(lab, "dump_console"):
            lab.collect()
        self.assertTrue((lab.evidence / "COLLECT_FAILED.txt").exists())

    def test_collect_with_a_reachable_guest_writes_binding_and_pulls_heartbeats(self):
        lab = self.lab()
        self.measured_like_the_prior_run(lab)
        pulled = []
        lab.get_file = lambda remote, local, timeout=300: pulled.append(remote) or False
        with mock.patch.object(lab, "run_process"):
            lab.collect()
        self.assertIn("/var/lib/arc-w0/heartbeats.jsonl", pulled)
        self.assertTrue((lab.evidence / "binding.json").exists())


if __name__ == "__main__":
    unittest.main()
