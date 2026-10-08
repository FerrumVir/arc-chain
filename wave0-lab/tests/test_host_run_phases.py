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


def make(script, source="artifact", live="allowed"):
    tmp = tempfile.TemporaryDirectory()
    cfg = json.loads(json.dumps(CONFIG))
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
        self.assertEqual(names, ["final_stop_begin", "node_stopped"])

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


class SetupPhaseTests(unittest.TestCase):
    def test_vm_phase_builds_the_expected_commands(self):
        lab, tmp = make([("cloud-init status", hr.Result(0, "status: done")), ("test -f", hr.Result(0, "")), ("uname -a", hr.Result(0, "Linux arc-wave0 6.8"))])
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
        self.assertEqual([e["name"] for e in events_of(lab)], ["vm_started", "vm_ready"])

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

    def test_baseline_falls_back_to_watch_mode_without_the_quota_match(self):
        lab, tmp = make([
            ("install-units.sh", hr.Result(0, "ok")), ("baseline.sh", hr.Result(0, "ok")), ("tail -n 1", hr.Result(0, '{"seq": 0}\n')),
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


if __name__ == "__main__":
    unittest.main()
