"""End-to-end check of the evidence contract: the orchestrator's real phase functions run against a SIMULATED guest on a
virtual clock, and the evaluator judges what they produced (THROWAWAY LAB FILE).

This does not prove the VM works (only a hosted runner can). It proves the producer (host_run.py + the sample/invariants
formats of the guest probes) and the consumer (evaluate_stage_b.py) agree about every file, field, event name and window."""
from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import _paths  # noqa: F401
import evaluate_stage_b as ev
import host_run as hr

CONFIG = json.loads((_paths.LAB / "config.json").read_text(encoding="utf-8"))
START = 1_800_000_000.0
NODE_EXE = "/home/arcw0/.arc/legacy-bridge/releases/v0.8.11/arc-node-linux-x86_64"


class Sim:
    """A guest on a virtual clock. Samples appear whenever the clock passes a multiple of the interval."""

    def __init__(self, interval, expect, node_sha, registered=3):
        self.registered = registered
        self.now = START
        self.interval = interval
        self.next_sample = START + interval
        self.seq = 0
        self.samples = []
        self.boot_id = "boot-0000-aaaa"
        self.boot_count = 0
        self.pid = 4000
        self.proc_start = int(START)
        self.up_at = START
        self.down_until = 0.0
        self.pending_reboot_at = None
        self.expect = expect
        self.node_sha = node_sha
        self.address = "ab" * 32
        self.reuse = 0
        self.bridged = False  # the v0.7.7 baseline until the consume succeeds
        self.timer_active = True
        self.extra_errors = []

    def advance(self, seconds):
        target = self.now + seconds
        while True:
            events = [t for t in (self.next_sample, self.pending_reboot_at) if t is not None and t <= target]
            if not events:
                break
            t = min(events)
            self.now = t
            if self.pending_reboot_at is not None and t == self.pending_reboot_at:
                self.do_reboot()
            else:
                self.emit()
                self.next_sample += self.interval
        self.now = target

    def do_reboot(self):
        self.pending_reboot_at = None
        self.boot_count += 1
        self.boot_id = f"boot-{self.boot_count:04d}-bbbb"
        self.down_until = self.now + 40
        self.up_at = self.now + 40 + 25
        self.pid += 1
        self.proc_start = int(self.up_at - 20)

    def restart(self):
        self.pid += 1
        self.proc_start = int(self.now)
        self.up_at = self.now + 15
        self.reuse += 1

    def emit(self):
        if self.now < self.down_until:
            return
        healthy = self.now >= self.up_at
        self.seq += 1
        sample = {
            "seq": self.seq, "epoch": round(self.now, 3), "boot_id": self.boot_id, "uptime_s": self.now - START + 5000 if self.boot_count == 0 else self.now - (self.down_until - 40) + 3,
            "node_state": "active", "main_pid": self.pid, "proc_start_epoch": self.proc_start,
            "node_exe": NODE_EXE if self.bridged else "/home/arcw0/.arc/bin/arc-node", "node_exe_sha256": self.node_sha if self.bridged else "1cfc3039786d023cde24ad0b452f35735b39f9e83aaf293e6ed0bf623a11b20c",
            "node_procs": 1, "n_restarts": 0, "health_ok": healthy, "chain_participation_enabled": False if healthy else None, "info_ok": healthy,
            "address": self.address if healthy else None, "stake": 0 if healthy else None, "node_version": "0.8.11" if healthy else None,
            "bridge_node_address": self.address, "bridge_compute": "off: no verified model", "compute_consent": "no", "community_registration": True,
            "public_name": ("node-" + self.address[:8]) if healthy else None, "coordinators_total": 6 if healthy else None,
            "coordinators_registered": self.registered if healthy else None, "version_txt": "0.7.12", "launcher_sha256": self.expect, "updater_timer_active": self.timer_active,
            "legacy_fingerprint": "f" * 64, "legacy_byte_compare": "same" if self.seq % 10 == 0 else None, "errors": list(self.extra_errors),
        }
        self.samples.append(sample)

    def capture(self):
        return {
            "guest_epoch": self.now, "boot_id": self.boot_id, "bin_arc_node_sha256": self.expect, "arc_node_prev_sha256": "p" * 64, "arc_node_new_size": None,
            "version_txt": "0.7.12", "main_pid": self.pid, "proc_start_epoch": self.proc_start, "node_exe": NODE_EXE, "node_exe_sha256": self.node_sha,
            "node_procs": 1, "node_state": "active", "health_ok": True, "info_ok": True, "address": self.address, "stake": 0, "bridge_node_address": self.address,
            "legacy_fingerprint": "f" * 64, "auto_update_log_lines": 10, "auto_update_rolled_back": 0, "bridge_log_lines": 20, "bridge_log_reuse_count": self.reuse,
            "updater_unit": {},
        }


class SimLab(hr.Lab):
    def __init__(self, cfg, evidence, work, sim):
        super().__init__(cfg, evidence, work)
        self.sim = sim
        self.commands = []

    def say(self, message):
        pass

    def guest(self, cmd, timeout=120, retries=0, log=True):
        sim = self.sim
        self.commands.append(cmd)
        if log:
            self._append("commands.jsonl", {"host_epoch": sim.now, "cmd": cmd[:600], "rc": 0})
        if "kernel/random/boot_id" in cmd:
            if sim.now < sim.down_until or sim.pending_reboot_at is not None and sim.now >= sim.pending_reboot_at:
                return hr.Result(255, "")
            return hr.Result(0, sim.boot_id + "\n")
        if "systemctl reboot" in cmd:
            sim.pending_reboot_at = sim.now + 2
            return hr.Result(0, "")
        if "systemctl restart arc-node" in cmd:
            sim.restart()
            return hr.Result(0, "")
        if "systemctl start arc-updater.service" in cmd:
            sim.advance(2)
            return hr.Result(1, "Job for arc-updater.service failed")
        if "systemctl show arc-updater" in cmd:
            return hr.Result(0, "Result=exit-code\nExecMainStatus=22\nActiveState=failed\n")
        if "auto-update.log" in cmd:
            return hr.Result(0, "[x] new version available: 0.7.12 → 0.7.11. Downloading.\ncurl: (22) 404\n")
        if "journalctl" in cmd:
            return hr.Result(0, "journal-pre-boot.txt journal-post-boot.txt 10")
        if "is-active arc-updater.timer" in cmd:
            return hr.Result(0, "active\nenabled\n")
        return hr.Result(0, "")

    def guest_epoch(self):
        return self.sim.now

    def capture(self):
        return self.sim.capture()

    def wait_health(self, seconds):
        self.sim.advance(max(0, self.sim.up_at - self.sim.now))
        return True

    def samples_tail(self, count=5):
        return list(self.sim.samples[-count:])

    def pull_samples(self):
        (self.evidence / "samples.jsonl").write_text("".join(json.dumps(s, sort_keys=True) + "\n" for s in self.sim.samples), encoding="utf-8")


def invariants(label, sim, boot_id, node_sha, expect, tag="0.7.12"):  # noqa: C901
    record = {
        "schema": "arc.legacy-bridge.wave0-lab.invariants.v1", "label": label, "guest_epoch": sim.now, "boot_id": boot_id,
        "legacy_snapshot_sha256": "a" * 64, "legacy_snapshot_source": "ExecStartPre hook snapshot start-003.json", "legacy_entries": 12, "v07_seed_sha256": "b" * 64,
        "unit_files": {"arc-node.service": "c" * 64, "arc-updater.service": "d" * 64, "arc-updater.timer": "e" * 64},
        "updater_timer": {"active": True, "enabled": True},
        "installed": {"version_txt": tag, "bin_arc_node_sha256": expect, "arc_node_prev_sha256": "p" * 64},
    }
    if label == "after":
        record.update({
            "node": {"main_pid": sim.pid, "exe": NODE_EXE, "exe_sha256": node_sha, "node_dirs": ["headless-abc"], "node_procs": 1,
                     "argv": [NODE_EXE, "--rpc", "127.0.0.1:9944", "--stake", "0", "--min-stake", "0", "--community-mode", "--community-rpc-url", "http://x"]},
            "node_info": {"stake": 0, "version": "0.8.11", "validator": sim.address},
            "health": {"chain_participation_enabled": False},
            "bridge_state": {"stake": 0, "node_address": sim.address, "legacy_kind": "headless", "compute": "off: no verified model", "community_registration": True, "archive_generation": 1},
            "compute_consent": "no",
            "community_status": {"public_name": "node-" + sim.address[:8], "coordinators_total": 6, "coordinators_registered": sim.registered},
        })
    return record


def simulate(profile_name, source="artifact", mutate=None, live="allowed", registered=None):
    """Run the real phase functions on the virtual clock and return (verdict, checks, evidence dir, lab)."""
    tmp = tempfile.TemporaryDirectory()
    cfg = json.loads(json.dumps(CONFIG))
    cfg["stage_b"]["profile"] = profile_name
    cfg["stage_b"]["launcher_source"] = source
    cfg["stage_b"]["live_network"] = live
    profile = cfg["stage_b"]["profiles"][profile_name]
    evidence = Path(tmp.name) / "evidence"
    pins = json.loads((_paths.ROOT / "crates/arc-legacy-bridge/pins/active.json").read_text())
    sim = Sim(profile["sample_interval_s"], cfg["stage_b"]["expect_sha256"], pins["node_release"]["assets"]["arc-node-linux-x86_64"]["sha256"],
              (3 if live == "allowed" else 0) if registered is None else registered)
    lab = SimLab(cfg, evidence, Path(tmp.name) / "work", sim)
    lab.launcher_bytes = 2663184
    (evidence / "config-effective.json").write_text(json.dumps(hr.effective_config(cfg, pins), indent=2) + "\n", encoding="utf-8")

    def fake_sleep(seconds):
        sim.advance(seconds)

    def fake_time():
        return sim.now

    with mock.patch.object(hr.time, "sleep", fake_sleep), mock.patch.object(hr.time, "time", fake_time):
        lab.t_start = sim.now
        lab.deadline = sim.now + profile["deadline_min"] * 60
        # the interrupted attempt and the successful consume (their phases need a real guest; the events and checks are what matters here)
        sim.advance(profile["baseline_s"] + 20)
        lab.event("apply1", forced=True, guest_epoch=sim.now, rc=1)
        sim.advance(60)
        lab.event("apply2", forced=True, guest_epoch=sim.now, attempt=1)
        sim.bridged = True
        sim.pid += 1
        sim.proc_start = int(sim.now)
        sim.up_at = sim.now + 15
        sim.advance(2 * profile["sample_interval_s"])
        first = next(s for s in sim.samples if s["health_ok"] and s["node_exe"] == NODE_EXE)
        lab.t0_address = first["address"]
        lab.t0_guest_epoch = first["epoch"]
        lab.event("t0_bridged_healthy", guest_epoch=first["epoch"], address=first["address"], legacy_fingerprint=first["legacy_fingerprint"], public_name=first["public_name"])
        before_boot = sim.boot_id
        for check_id in ("L02-consume-dry-run", "L03-interrupt-took-effect", "L04-interrupt-resumes"):
            lab.check(check_id, "simulated", "PASS", "simulated")
        (evidence / "invariants-before.json").write_text(json.dumps(invariants("before", sim, before_boot, sim.node_sha, sim.expect)), encoding="utf-8")
        lab.check("L01-kvm", "simulated", "PASS", "simulated")
        if mutate:
            mutate("before_battery", lab, sim)
        lab.phase_updaters()
        lab.phase_kickstarts()
        lab.phase_reboot()
        if mutate:
            mutate("before_steady", lab, sim)
        lab.phase_steady()
        (evidence / "invariants-after.json").write_text(json.dumps(invariants("after", sim, sim.boot_id, sim.node_sha, sim.expect)), encoding="utf-8")
        lab.event("final_stop_begin", forced=True, guest_epoch=sim.now)
        lab.check("L11-stop-rollback", "simulated", "PASS", "simulated")
    lab.pull_samples()
    import contextlib
    import io
    with contextlib.redirect_stdout(io.StringIO()):
        exit_code = ev.main(["--evidence", str(evidence)])
    verdict = json.loads((evidence / "verdict.json").read_text(encoding="utf-8"))
    verdict["exit_code"] = exit_code
    checks = json.loads((evidence / "checks.json").read_text(encoding="utf-8"))
    if isinstance(checks, dict):
        checks = checks.get("checks", [])
    return verdict, checks, evidence, lab, tmp


class IntegrationTests(unittest.TestCase):
    def failing(self, checks):
        return {check["id"]: check["detail"] for check in checks if check["result"] == "FAIL"}

    def test_smoke_run_is_judged_smoke_pass(self):
        verdict, checks, evidence, lab, tmp = simulate("smoke")
        self.addCleanup(tmp.cleanup)
        self.assertEqual(self.failing(checks), {})
        self.assertEqual(verdict["verdict"], "SMOKE_PASS")
        self.assertEqual(verdict["exit_code"], 0)
        self.assertIn("does NOT satisfy", verdict["statement"])
        self.assertIn("rehearsal", verdict["statement"])

    def test_full_run_on_the_artifact_is_a_rehearsal_pass(self):
        verdict, checks, evidence, lab, tmp = simulate("full")
        self.addCleanup(tmp.cleanup)
        self.assertEqual(self.failing(checks), {})
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")
        self.assertFalse(verdict["is_post_g0_wave0"], "an artifact run is a rehearsal, never the post-G0 Wave 0")
        self.assertGreaterEqual(verdict["windows"]["steady_s"], 7200)
        self.assertGreaterEqual(verdict["windows"]["total_s"], 14400)

    def test_full_run_on_the_published_release_is_the_post_g0_wave0(self):
        verdict, checks, evidence, lab, tmp = simulate("full", source="published")
        self.addCleanup(tmp.cleanup)
        self.assertEqual(self.failing(checks), {})
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")
        self.assertTrue(verdict["is_post_g0_wave0"])

    def test_a_blocked_network_run_expects_no_registration(self):
        verdict, checks, evidence, lab, tmp = simulate("smoke", live="blocked")
        self.addCleanup(tmp.cleanup)
        self.assertEqual(self.failing(checks), {})
        self.assertEqual(verdict["verdict"], "SMOKE_PASS")
        self.assertEqual(verdict["live_network"], "blocked")

    def test_a_registration_while_blocked_is_caught(self):
        verdict, checks, evidence, lab, tmp = simulate("smoke", live="blocked", registered=2)
        self.addCleanup(tmp.cleanup)
        self.assertIn("REGISTRATION-LIVE", self.failing(checks))

    def test_no_registration_while_allowed_is_caught(self):
        verdict, checks, evidence, lab, tmp = simulate("smoke", live="allowed")
        self.addCleanup(tmp.cleanup)
        self.assertEqual(self.failing(checks), {})

        verdict, checks, evidence, lab, tmp2 = simulate("smoke", live="allowed", registered=0)
        self.addCleanup(tmp2.cleanup)
        self.assertIn("REGISTRATION-LIVE", self.failing(checks))

    def test_a_restart_in_the_steady_window_is_caught(self):
        def mutate(stage, lab, sim):
            if stage == "before_steady":
                original = sim.advance
                state = {"done": False}

                def advance(seconds):
                    original(seconds)
                    if not state["done"] and sim.now > sim.down_until + 3 * 3600:
                        state["done"] = True
                        sim.restart()
                sim.advance = advance

        verdict, checks, evidence, lab, tmp = simulate("full", mutate=mutate)
        self.addCleanup(tmp.cleanup)
        self.assertIn("STEADY-UNINTERRUPTED", self.failing(checks))
        self.assertEqual(verdict["verdict"], "WAVE0_FAIL")
        self.assertEqual(verdict["exit_code"], 1)

    def test_the_timer_inactive_after_reboot_is_caught(self):
        def mutate(stage, lab, sim):
            if stage == "before_steady":
                sim.timer_active = False

        verdict, checks, evidence, lab, tmp = simulate("smoke", mutate=mutate)
        self.addCleanup(tmp.cleanup)
        self.assertIn("REBOOT-UPDATER-TIMER", self.failing(checks))

    def test_the_commands_audit_has_no_start_after_the_reboot(self):
        verdict, checks, evidence, lab, tmp = simulate("smoke")
        self.addCleanup(tmp.cleanup)
        issued = next(json.loads(line) for line in (evidence / "events.jsonl").read_text().splitlines() if json.loads(line)["name"] == "reboot_issued")
        later = [json.loads(line) for line in (evidence / "commands.jsonl").read_text().splitlines() if json.loads(line)["host_epoch"] > issued["host_epoch"] + 3]
        self.assertTrue(later, "the steady phase issues read-only commands")
        for command in later:
            self.assertNotRegex(command["cmd"], r"systemctl\s+(start|restart)")


if __name__ == "__main__":
    unittest.main()
