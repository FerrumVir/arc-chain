"""End-to-end check of the evidence contract: the orchestrator's real phase functions run against a SIMULATED guest on a
virtual clock, and the evaluator judges what they produced (THROWAWAY LAB FILE).

This does not prove the VM works (only a hosted runner can). It proves the producer (host_run.py + the sample/invariants
formats of the guest probes) and the consumer (evaluate_stage_b.py) agree about every file, field, event name and window."""
from __future__ import annotations

import email.utils
import json
import math
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
ROUND_S = 15.0        # the node's registration/heartbeat round (COMMUNITY_PRESENCE_INTERVAL)
ROUND_OFFSET = 7.0
POLL_S = 5.0          # the guest heartbeat poller
PROBE_S = 30.0        # the runner's public scoreboard reads


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
        # resource series (flat unless a test bends them) and freshness
        self.rss_base_kb = 300_000
        self.rss_slope_kb_per_s = 0.0
        self.mem_total_kb = 3_900_000
        self.mem_available_kb = 3_000_000
        self.swap_total_kb = 0
        self.disk_total_b = 20_000_000_000
        self.disk_free_b0 = 14_000_000_000
        self.disk_slope_b_per_s = -100.0
        self.bridged_since = None
        self.round_dropouts = []     # (from, to): no registration round succeeds
        self.poll_dropouts = []      # (from, to): the heartbeat poller records nothing
        self.public_dropouts = []    # (from, to): no coordinator serves the node's row
        self.next_poll = START + POLL_S
        self.heartbeats = []
        self.next_probe = None
        self.probe_hook = None
        self.oom_count = 0

    def advance(self, seconds):
        target = self.now + seconds
        while True:
            events = [t for t in (self.next_sample, self.pending_reboot_at, self.next_poll, self.next_probe) if t is not None and t <= target]
            if not events:
                break
            t = min(events)
            self.now = t
            if self.pending_reboot_at is not None and t == self.pending_reboot_at:
                self.do_reboot()
            elif t == self.next_poll:
                self.poll()
                self.next_poll += POLL_S
            elif self.next_probe is not None and t == self.next_probe:
                self.next_probe += PROBE_S
                if self.probe_hook is not None:
                    self.probe_hook()
            else:
                self.emit()
                self.next_sample += self.interval
        self.now = target

    # --- registration rounds, the poller and the public board -------------------------------------
    @staticmethod
    def inside(windows, t):
        return any(a <= t < b for a, b in windows)

    def serving(self, t):
        """True while the bridged node is up and a registration round can succeed."""
        return self.registered > 0 and self.bridged and self.bridged_since is not None and t >= self.bridged_since and t >= self.down_until and t >= self.up_at

    def last_round(self, t):
        if not self.serving(t):
            return None
        k = math.floor((t - START - ROUND_OFFSET) / ROUND_S)
        while k >= 0:
            moment = START + ROUND_OFFSET + ROUND_S * k
            if moment < self.bridged_since:
                return None
            if moment <= t and not self.inside(self.round_dropouts, moment):
                return moment
            k -= 1
        return None

    def poll(self):
        if self.inside(self.poll_dropouts, self.now):
            return
        last = self.last_round(self.now)
        up = self.now >= self.down_until
        if not up:
            return
        self.heartbeats.append({
            "obs_epoch": round(self.now, 3), "ts_ms": int(last * 1000) if last else None,
            "registered": self.registered if last else None, "total": 6 if last else None,
        })

    def public_row(self, t):
        """The scoreboard serves the row only while the last heartbeat is at most 90 s old (rpc.rs:8514)."""
        last = self.last_round(t)
        return last is not None and t - last <= 90.0 and not self.inside(self.public_dropouts, t)

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
        last = self.last_round(self.now) if healthy else None
        elapsed = self.now - START
        wobble = (self.seq * 7919) % 11 - 5      # deterministic noise
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
            "last_registration_unix_ms": int(last * 1000) if last else None,
            "registration_age_s": round(self.now - last, 3) if last else None,
            "node_rss_kb": int(self.rss_base_kb + self.rss_slope_kb_per_s * elapsed + wobble * 100), "node_hwm_kb": int(self.rss_base_kb + self.rss_slope_kb_per_s * elapsed + 3000),
            "node_swap_kb": 0, "node_threads": 41, "node_fds": 64, "node_cpu_s": round(elapsed * 0.02, 3),
            "mem_total_kb": self.mem_total_kb, "mem_available_kb": self.mem_available_kb + wobble * 1000, "swap_total_kb": self.swap_total_kb, "swap_free_kb": self.swap_total_kb,
            "cg_mem_current_b": 310_000_000, "cg_mem_peak_b": 330_000_000, "cg_swap_current_b": 0,
            "disk_total_b": self.disk_total_b, "disk_free_b": int(self.disk_free_b0 + self.disk_slope_b_per_s * elapsed),
            "log_bytes": 50_000 + int(elapsed * 2), "bridge_downloads": 1,
            "arc_dir_bytes": 60_000_000 + int(elapsed * 3), "largest_file_bytes": 29_210_392, "legacy_data_bytes": 1_000_000, "node_data_bytes": 400_000 + int(elapsed),
            "release_cache_bytes": 33_833_184, "release_cache_files": 2, "models_bytes": 0, "partial_files": 0,
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
        if "oom_scan.py" in cmd:
            # one scan per boot of the guest (index -boot_count .. 0), exactly like oom_scan.py; an OOM line, if any, is in the current boot
            scans = []
            for index in range(-sim.boot_count, 1):
                count = sim.oom_count if index == 0 else 0
                scans.append({"boot": index, "first_entry": "2026-10-08T08:00:00+00:00", "last_entry": "2026-10-08T11:00:00+00:00", "kernel_lines": 1500 + count,
                              "count": count, "lines": ["kernel: Out of memory: Killed process 4242 (arc-node)"] * count})
            return hr.Result(0, json.dumps({"schema": "arc.legacy-bridge.wave0-lab.kernel-oom.v1", "scans": scans, "total": sim.oom_count}) + "\n")
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
        (self.evidence / "heartbeats.jsonl").write_text("".join(json.dumps(h, sort_keys=True) + "\n" for h in self.sim.heartbeats), encoding="utf-8")


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


def fake_scoreboard_lookup(sim, lab):
    """The public board as the node serves it: a row only while the last heartbeat is at most 90 s old."""
    def lookup(origin, address, opener=None):
        index = lab.origins.index(origin)
        t = sim.now
        stamp = email.utils.formatdate(t, usegmt=True)
        if sim.public_row(t) and index not in sim.origins_without_row:
            row = {"worker_id": "0x" + address, "name": "node-" + address[:8], "registered_at": int(START)}
            return {"http": 200, "error": None, "found": True, "name": row["name"], "registered_at": row["registered_at"], "worker_id": row["worker_id"],
                    "server_date": stamp, "count_total": 7, "row": row, "elapsed_ms": 30}
        return {"http": 200, "error": None, "found": False, "name": None, "registered_at": None, "worker_id": None,
                "server_date": stamp, "count_total": 6, "row": None, "elapsed_ms": 30}
    return lookup


def simulate(profile_name, source="artifact", mutate=None, live="allowed", registered=None, prior=None):
    """Run the real phase functions on the virtual clock and return (verdict, checks, evidence dir, lab)."""
    tmp = tempfile.TemporaryDirectory()
    cfg = json.loads(json.dumps(CONFIG))
    cfg["stage_b"]["profile"] = profile_name
    cfg["stage_b"]["launcher_source"] = source
    cfg["stage_b"]["live_network"] = live
    profile = cfg["stage_b"]["profiles"][profile_name]
    supplement = not profile.get("battery", True)
    evidence = Path(tmp.name) / "evidence"
    pins = json.loads((_paths.ROOT / "crates/arc-legacy-bridge/pins/active.json").read_text())
    sim = Sim(profile["sample_interval_s"], cfg["stage_b"]["expect_sha256"], pins["node_release"]["assets"]["arc-node-linux-x86_64"]["sha256"],
              (3 if live == "allowed" else 0) if registered is None else registered)
    sim.origins_without_row = set()
    lab = SimLab(cfg, evidence, Path(tmp.name) / "work", sim)
    lab.launcher_bytes = 2663184
    (evidence / "config-effective.json").write_text(json.dumps(hr.effective_config(cfg, pins), indent=2) + "\n", encoding="utf-8")

    def fake_sleep(seconds):
        sim.advance(seconds)

    def fake_time():
        return sim.now

    with mock.patch.object(hr.time, "sleep", fake_sleep), mock.patch.object(hr.time, "time", fake_time), \
            mock.patch.object(hr, "scoreboard_lookup", fake_scoreboard_lookup(sim, lab)):
        lab.t_start = sim.now
        lab.deadline = sim.now + profile["deadline_min"] * 60
        lab.event("vm_memory", configured_mb=cfg["stage_b"]["vm"]["memory_mb"], mem_total_kb=sim.mem_total_kb, swap_total_kb=sim.swap_total_kb)
        # the interrupted attempt and the successful consume (their phases need a real guest; the events and checks are what matters here)
        sim.advance(profile["baseline_s"] + 20)
        if not supplement:
            lab.event("apply1", forced=True, guest_epoch=sim.now, rc=1)
            sim.advance(60)
        lab.event("apply2", forced=True, guest_epoch=sim.now, attempt=1)
        sim.bridged = True
        sim.pid += 1
        sim.proc_start = int(sim.now)
        sim.up_at = sim.now + 15
        sim.bridged_since = sim.up_at
        sim.advance(2 * profile["sample_interval_s"])
        first = next(s for s in sim.samples if s["health_ok"] and s["node_exe"] == NODE_EXE)
        lab.t0_address = first["address"]
        lab.t0_guest_epoch = first["epoch"]
        lab.event("t0_bridged_healthy", guest_epoch=first["epoch"], address=first["address"], legacy_fingerprint=first["legacy_fingerprint"], public_name=first["public_name"])
        if live == "allowed":
            sim.next_probe = sim.now + 20
            sim.probe_hook = lab.scoreboard_probe
        before_boot = sim.boot_id
        lab.check("L01-kvm", "simulated", "PASS", "simulated")
        lab.check("L02-consume-dry-run", "simulated", "PASS", "simulated")
        lab.check("L04-interrupt-resumes", "simulated", "PASS", "simulated")
        if not supplement:
            lab.check("L03-interrupt-took-effect", "simulated", "PASS", "simulated")
        (evidence / "invariants-before.json").write_text(json.dumps(invariants("before", sim, before_boot, sim.node_sha, sim.expect)), encoding="utf-8")
        if mutate:
            mutate("before_battery", lab, sim)
        if not supplement:
            lab.phase_updaters()
            lab.phase_kickstarts()
            lab.phase_reboot()
        if mutate:
            mutate("before_steady", lab, sim)
        lab.phase_steady()
        sim.next_probe = None
        lab.pull_samples()
        lab.kernel_oom_scan()
        (evidence / "invariants-after.json").write_text(json.dumps(invariants("after", sim, sim.boot_id, sim.node_sha, sim.expect)), encoding="utf-8")
        lab.event("final_stop_begin", forced=True, guest_epoch=sim.now)
        lab.check("L11-stop-rollback", "simulated", "PASS", "simulated")
        lab.event("node_stopped", detail={"state": "inactive"})
        if supplement:
            measured = prior if prior is not None else cfg["stage_b"]["prior_run"]
            for key in ("launcher_sha256", "node_sha256", "legacy_node_sha256", "installer_sha256", "image_sha256", "baseline_result"):
                lab.measured[key] = measured[key]
            lab.measured["units"] = dict(measured["units"])
            lab.write_binding()
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


class SupplementIntegrationTests(unittest.TestCase):
    """The resources supplement end to end: the real phase functions, the real guest-format samples, polls and board reads on a
    virtual clock, judged by the real evaluator. Every mutation bends exactly one thing a real run could get wrong."""

    SUPPLEMENT_IDS = ("RES-RSS", "RES-MEMAVAIL", "RES-PROJECTION", "RES-DISK", "RES-DOWNLOADS", "RES-OOM", "VM-MEMORY", "BINDING-PRIOR-RUN",
                      "FRESH-AGE", "FRESH-DISTINCT", "PUBLIC-FRESHNESS")

    def run_sim(self, mutate=None, **kwargs):
        verdict, checks, evidence, lab, tmp = simulate("resources", source="published", mutate=mutate, **kwargs)
        self.addCleanup(tmp.cleanup)
        return verdict, {check["id"]: check for check in checks}, evidence, lab

    def failing(self, checks):
        return sorted(check_id for check_id, check in checks.items() if check["result"] == "FAIL")

    def at(self, lab, seconds):
        return lab.t0_guest_epoch + seconds

    def test_the_clean_supplement_passes_every_criterion(self):
        verdict, checks, evidence, lab = self.run_sim()
        self.assertEqual(self.failing(checks), [])
        self.assertEqual(verdict["verdict"], "SUPPLEMENT_PASS")
        self.assertEqual(verdict["exit_code"], 0)
        self.assertFalse(verdict["is_post_g0_wave0"], "a supplement is never the four-hour Wave 0")
        self.assertIn("not a Wave 0 pass", verdict["statement"])
        for check_id in self.SUPPLEMENT_IDS:
            self.assertEqual(checks[check_id]["result"], "PASS", checks[check_id])
        self.assertGreaterEqual(verdict["windows"]["steady_s"], 8100)
        self.assertGreaterEqual(verdict["windows"]["total_s"], 8700)
        self.assertGreaterEqual(int(checks["SAMPLES-STEADY"]["detail"].split()[0]), 136)
        for check_id in ("REBOOT-BOOT-ID", "REBOOT-AUTO-RECOVERY", "L05-updater-1-noop", "L10-reboot-boot-id-changed"):
            self.assertEqual(checks[check_id]["result"], "SKIP")
            self.assertIn("37750760170", checks[check_id]["detail"])

    def test_the_supplement_writes_every_file_the_evaluator_reads(self):
        verdict, checks, evidence, lab = self.run_sim()
        names = {path.name for path in evidence.iterdir()}
        for name in ("samples.jsonl", "heartbeats.jsonl", "scoreboard.jsonl", "kernel-oom.json", "binding.json", "events.jsonl", "invariants-before.json"):
            self.assertIn(name, names)
        events = [json.loads(line) for line in (evidence / "events.jsonl").read_text().splitlines()]
        names = [event["name"] for event in events]
        self.assertNotIn("apply1", names)
        self.assertNotIn("reboot_issued", names)
        self.assertLess(names.index("steady_end"), names.index("kernel_oom_scan"))
        self.assertEqual(names.count("vm_memory"), 1)
        scoreboard = [json.loads(line) for line in (evidence / "scoreboard.jsonl").read_text().splitlines()]
        self.assertTrue(all(len(record["results"]) == 6 for record in scoreboard))
        gaps = [b["host_epoch"] - a["host_epoch"] for a, b in zip(scoreboard, scoreboard[1:])]
        self.assertLessEqual(max(gaps), 30.5, "the public board is read every 30 s")

    def test_a_registration_outage_over_90_seconds_is_caught_three_ways(self):
        def mutate(stage, lab, sim):
            if stage == "before_steady":
                sim.round_dropouts.append((self.at(lab, 3000), self.at(lab, 3200)))

        verdict, checks, evidence, lab = self.run_sim(mutate)
        failing = self.failing(checks)
        for check_id in ("FRESH-AGE", "FRESH-DISTINCT", "PUBLIC-FRESHNESS"):
            self.assertIn(check_id, failing)
        self.assertEqual(verdict["verdict"], "SUPPLEMENT_FAIL")

    def test_a_dead_poller_is_unproved_not_pass(self):
        def mutate(stage, lab, sim):
            if stage == "before_steady":
                sim.poll_dropouts.append((self.at(lab, 3000), self.at(lab, 3060)))

        verdict, checks, evidence, lab = self.run_sim(mutate)
        self.assertEqual(self.failing(checks), ["FRESH-DISTINCT"])
        self.assertIn("UNPROVED", checks["FRESH-DISTINCT"]["detail"])

    def test_a_missing_heartbeat_log_fails_closed(self):
        verdict, checks, evidence, lab = self.run_sim()
        (evidence / "heartbeats.jsonl").unlink()
        import contextlib
        import io
        with contextlib.redirect_stdout(io.StringIO()):
            ev.main(["--evidence", str(evidence)])
        loaded = json.loads((evidence / "checks.json").read_text())
        rechecked = {check["id"]: check for check in (loaded["checks"] if isinstance(loaded, dict) else loaded)}
        self.assertEqual(rechecked["FRESH-DISTINCT"]["result"], "FAIL")
        self.assertEqual(json.loads((evidence / "verdict.json").read_text())["verdict"], "SUPPLEMENT_FAIL")

    def test_the_public_board_missing_the_node_in_one_round_is_caught(self):
        def mutate(stage, lab, sim):
            if stage == "before_steady":
                sim.public_dropouts.append((self.at(lab, 3000), self.at(lab, 3040)))

        verdict, checks, evidence, lab = self.run_sim(mutate)
        self.assertEqual(self.failing(checks), ["PUBLIC-FRESHNESS"])
        self.assertEqual(verdict["verdict"], "SUPPLEMENT_FAIL")

    def test_three_coordinators_without_a_row_is_still_fresh(self):
        def mutate(stage, lab, sim):
            sim.origins_without_row = {0, 2, 4}

        verdict, checks, evidence, lab = self.run_sim(mutate)
        self.assertEqual(self.failing(checks), [])
        self.assertIn("min 3, max 3", checks["PUBLIC-FRESHNESS"]["detail"])

    def test_a_kernel_oom_kill_fails_the_whole_window(self):
        def mutate(stage, lab, sim):
            sim.oom_count = 1

        verdict, checks, evidence, lab = self.run_sim(mutate)
        self.assertEqual(self.failing(checks), ["RES-OOM"])
        self.assertEqual(verdict["verdict"], "SUPPLEMENT_FAIL")

    def test_rss_growth_above_100_mib_per_hour_fails(self):
        def mutate(stage, lab, sim):
            sim.rss_slope_kb_per_s = 200 * 1024 / 3600.0

        verdict, checks, evidence, lab = self.run_sim(mutate)
        self.assertIn("RES-RSS", self.failing(checks))

    def test_available_memory_below_512_mib_fails(self):
        def mutate(stage, lab, sim):
            sim.mem_available_kb = 400 * 1024

        verdict, checks, evidence, lab = self.run_sim(mutate)
        self.assertIn("RES-MEMAVAIL", self.failing(checks))

    def test_a_disk_that_fills_within_72_hours_fails(self):
        def mutate(stage, lab, sim):
            sim.disk_slope_b_per_s = -2_000_000.0

        verdict, checks, evidence, lab = self.run_sim(mutate)
        self.assertIn("RES-DISK", self.failing(checks))

    def test_swap_is_a_series_never_a_failure(self):
        def mutate(stage, lab, sim):
            sim.swap_total_kb = 2_097_148

        verdict, checks, evidence, lab = self.run_sim(mutate)
        self.assertEqual(self.failing(checks), [])
        self.assertEqual(checks["RES-SWAP"]["result"], "INFO")

    def test_a_sampler_stall_above_65_seconds_fails_the_gap_rule(self):
        def mutate(stage, lab, sim):
            if stage == "before_steady":
                original = sim.emit
                stalled = {"done": False}

                def emit():
                    if not stalled["done"] and sim.now >= self.at(lab, 3000):
                        stalled["done"] = True
                        return          # one sample is lost: a 120 s hole
                    original()

                sim.emit = emit

        verdict, checks, evidence, lab = self.run_sim(mutate)
        self.assertIn("SAMPLE-GAPS", self.failing(checks))
        self.assertIn("65", checks["SAMPLE-GAPS"]["detail"])

    def test_other_bytes_than_the_four_hour_run_fail_the_binding(self):
        prior = json.loads(json.dumps(CONFIG["stage_b"]["prior_run"]))
        prior["node_sha256"] = "0" * 64
        verdict, checks, evidence, lab = self.run_sim(prior=prior)
        self.assertEqual(self.failing(checks), ["BINDING-PRIOR-RUN"])
        self.assertEqual(verdict["verdict"], "SUPPLEMENT_FAIL")

    def test_the_binding_file_discloses_the_memory_change(self):
        verdict, checks, evidence, lab = self.run_sim()
        record = json.loads((evidence / "binding.json").read_text())
        self.assertEqual(record["disclosed_changes"], {"vm_memory_mb": {"prior": 6144, "this": 4096}})
        self.assertTrue(record["all_equal"])
        self.assertIn("6144 -> 4096", checks["BINDING-PRIOR-RUN"]["detail"])


if __name__ == "__main__":
    unittest.main()
