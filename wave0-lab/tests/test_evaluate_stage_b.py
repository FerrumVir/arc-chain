#!/usr/bin/env python3
"""Unit tests for wave0-lab/evaluate_stage_b.py.

Run from the repository root:  python3 -B wave0-lab/tests/test_evaluate_stage_b.py

The tests build a synthetic PERFECT Wave 0 dataset (full profile: 4 h 10 min of 60-second samples across a real
reboot; smoke profile: 10-second samples), prove it passes, and then flip exactly one thing per check family and prove
the matching check (and, where the mutation is independent, only that check) fails.
"""

import contextlib
import io
import json
import os
import re
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import evaluate_stage_b as E  # noqa: E402

BASE = 1_790_000_000.0
A64 = "ab" * 32
LAUNCHER = "11" * 32
NODE = "22" * 32
V077 = "77" * 32
LEGACY_FP = "33" * 32
SEED = "44" * 32
SNAP = "55" * 32
UNIT_HASHES = {"arc-node.service": "a1" * 32, "arc-updater.service": "a2" * 32, "arc-updater.timer": "a3" * 32}
BOOT1 = "11111111-1111-1111-1111-111111111111"
BOOT2 = "22222222-2222-2222-2222-222222222222"
BOOT1_START = BASE - 7000
NODE_EXE = "/home/arcw0/.arc/legacy-bridge/releases/v0.8.11/arc-node-linux-x86_64"
V07_EXE = "/home/arcw0/.arc/bin/arc-node"
NODE_DIR = "/home/arcw0/.arc/legacy-bridge/nodes/headless-0123456789abcdef"

FULL = dict(interval=60, total_s=15000, steady_s=13400, boot_a=50)
SMOKE = dict(interval=10, total_s=700, steady_s=400, boot_a=25)


# ------------------------------------------------------------------------------------------------------------------
# synthetic dataset
# ------------------------------------------------------------------------------------------------------------------

def v07_sample(t, boot, boot_start):
    return {"epoch": t, "boot_id": boot, "uptime_s": t - boot_start, "node_state": "active", "main_pid": 1111,
            "proc_start_epoch": int(BASE - 7000), "node_exe": V07_EXE, "node_exe_sha256": V077, "node_procs": 1,
            "n_restarts": 0, "health_ok": True, "chain_participation_enabled": None, "info_ok": False, "address": None,
            "stake": None, "node_version": None, "bridge_node_address": None, "bridge_compute": None,
            "compute_consent": None, "community_registration": None, "public_name": None, "coordinators_total": None,
            "coordinators_registered": None, "version_txt": "0.7.7", "launcher_sha256": V077,
            "updater_timer_active": True, "legacy_fingerprint": "ee" * 32, "legacy_byte_compare": None, "errors": []}


def bridged_sample(t, boot, boot_start, pid, pstart, healthy=True, registered=3, timer=True):
    sample = {"epoch": t, "boot_id": boot, "uptime_s": t - boot_start, "node_state": "active", "main_pid": pid,
              "proc_start_epoch": pstart, "node_exe": NODE_EXE, "node_exe_sha256": NODE, "node_procs": 1,
              "n_restarts": 0, "health_ok": healthy, "chain_participation_enabled": False if healthy else None,
              "info_ok": healthy, "address": A64 if healthy else None, "stake": 0 if healthy else None,
              "node_version": "0.8.11" if healthy else None, "bridge_node_address": A64,
              "bridge_compute": "off: no verified model", "compute_consent": "no",
              "community_registration": True, "public_name": ("node-" + A64[:8]) if healthy else None,
              "coordinators_total": 6 if healthy else None, "coordinators_registered": registered if healthy else None,
              "version_txt": "0.7.12", "launcher_sha256": LAUNCHER, "updater_timer_active": timer,
              "legacy_fingerprint": LEGACY_FP, "legacy_byte_compare": "same", "errors": []}
    return sample


def booting_sample(t, boot, boot_start):
    """First sample after the guest boots: the node is not up yet and the timers may not be either."""
    return {"epoch": t, "boot_id": boot, "uptime_s": t - boot_start, "node_state": "activating", "main_pid": 0,
            "proc_start_epoch": None, "node_exe": None, "node_exe_sha256": None, "node_procs": 0, "n_restarts": 0,
            "health_ok": False, "chain_participation_enabled": None, "info_ok": False, "address": None, "stake": None,
            "node_version": None, "bridge_node_address": None, "bridge_compute": None, "compute_consent": None,
            "community_registration": None, "public_name": None, "coordinators_total": None,
            "coordinators_registered": None, "version_txt": "0.7.12", "launcher_sha256": LAUNCHER,
            "updater_timer_active": False, "legacy_fingerprint": None, "legacy_byte_compare": None, "errors": []}


def make_event(seq, name, t, forced, detail=None, guest=True):
    return {"seq": seq, "name": name, "forced": forced, "host_epoch": t + 0.4,
            "guest_epoch": t if guest else None, "detail": detail or {}}


def set_event_time(event, t):
    event["guest_epoch"] = t
    event["host_epoch"] = t + 0.4


class Dataset(object):
    def texts(self):
        def rows(items):
            return "".join(json.dumps(item, sort_keys=True) + "\n" for item in items)
        return {
            "config-effective.json": json.dumps(self.config, indent=2),
            "samples.jsonl": rows(self.samples),
            "events.jsonl": rows(self.events),
            "commands.jsonl": rows(self.commands),
            "invariants-before.json": json.dumps(self.before),
            "invariants-after.json": json.dumps(self.after),
            "checks-live.jsonl": rows(self.live),
        }

    def event(self, name):
        return [e for e in self.events if e["name"] == name][0]

    def bridged(self):
        return [s for s in self.samples if s.get("node_exe") == NODE_EXE]

    def steady(self):
        return [s for s in self.bridged() if s["epoch"] >= self.rec - E.EPS]


def build(profile="full", live_network="allowed", launcher_source="published", post_unhealthy=0, **overrides):
    params = dict(FULL if profile == "full" else SMOKE)
    params.update(overrides)
    interval = float(params["interval"])
    total_s, steady_s, boot_a = params["total_s"], params["steady_s"], params["boot_a"]
    ds = Dataset()
    ds.I = interval
    ds.fb = BASE + 6 * interval
    ds.t0 = ds.fb + 3.0
    ds.E = ds.t0 + total_s
    ds.rec = ds.t0 + (total_s - steady_s)
    down = boot_a + (post_unhealthy + 1) * interval + 5
    ds.issued = ds.rec - down
    ds.first_post = ds.issued + boot_a
    ds.first_healthy = ds.first_post + (post_unhealthy + 1) * interval
    battery = ds.issued - ds.t0
    ds.up_times = [ds.t0 + 0.10 * battery, ds.t0 + 0.18 * battery]
    ds.kick_times = [ds.t0 + 0.45 * battery, ds.t0 + 0.60 * battery, ds.t0 + 0.75 * battery]
    boot2_start = ds.issued + 8

    samples = []
    for k in range(-10, 6):
        samples.append(v07_sample(BASE + k * interval, BOOT1, BOOT1_START))
    pre_times = []
    moment = ds.fb
    while moment < ds.issued - 1:
        pre_times.append(moment)
        moment += interval
    transient = {}
    for kick in ds.kick_times:
        for index, grid in enumerate(pre_times):
            if grid > kick:
                transient[index] = grid
                break
    pid, pstart = 2222, int(ds.fb - 4)
    for index, grid in enumerate(pre_times):
        if index in transient:
            pid += 1000
            pstart = int(grid) - 1
            samples.append(bridged_sample(grid, BOOT1, BOOT1_START, pid, pstart, healthy=False))
        else:
            samples.append(bridged_sample(grid, BOOT1, BOOT1_START, pid, pstart))
    samples.append(booting_sample(ds.first_post, BOOT2, boot2_start))
    post_pid, post_start = 4242, int(ds.first_post + 1)
    for index in range(post_unhealthy):
        samples.append(bridged_sample(ds.first_post + (index + 1) * interval, BOOT2, boot2_start, post_pid, post_start,
                                      healthy=False))
    moment = ds.first_healthy
    while moment <= ds.E + E.EPS:
        samples.append(bridged_sample(moment, BOOT2, boot2_start, post_pid, post_start,
                                      registered=3 if live_network == "allowed" else 0))
        moment += interval
    if live_network == "blocked":
        for sample in samples:
            if sample["coordinators_registered"] is not None and sample["node_exe"] == NODE_EXE:
                sample["coordinators_registered"] = 0
    for number, sample in enumerate(samples, 1):
        sample["seq"] = number
    ds.samples = samples

    events = [
        make_event(1, "setup", BASE - 900, False),
        make_event(2, "apply1", BASE, True, guest=False),
        make_event(3, "interrupt_block_start", BASE + 2, True),
        make_event(4, "apply2", BASE + 5 * interval, True),
        make_event(5, "t0_bridged_healthy", ds.t0, False, {"address": A64, "legacy_fingerprint": LEGACY_FP}),
        make_event(6, "updater_run_1", ds.up_times[0], True),
        make_event(7, "updater_run_2", ds.up_times[1], True),
        make_event(8, "kickstart_1", ds.kick_times[0], True),
        make_event(9, "kickstart_2", ds.kick_times[1], True),
        make_event(10, "kickstart_3", ds.kick_times[2], True),
        make_event(11, "reboot_issued", ds.issued, True, {"boot_id_before": BOOT1}),
        make_event(12, "reboot_recovered", ds.rec, False,
                   {"boot_id_after": BOOT2, "first_healthy_sample_epoch": ds.first_healthy}),
        make_event(13, "heartbeat", ds.rec + 300, False),
        make_event(14, "steady_begin", ds.rec + 2, False, {"last_forced_event": "reboot_issued"}),
        make_event(15, "steady_end", ds.E, False),
        make_event(16, "final_stop_begin", ds.E + 20, True, guest=False),
    ]
    ds.events = events

    def host(t):
        return t + 0.4

    commands = [
        {"host_epoch": host(BASE - 800), "cmd": "sudo apt-get install -y jq", "rc": 0},
        {"host_epoch": host(BASE - 100), "cmd": "sudo systemctl start arc-node", "rc": 0},
        {"host_epoch": host(BASE + 1), "cmd": "bash /opt/arc-w0/canary-consume.sh --tag v0.7.12 --apply", "rc": 1},
    ]
    for moment in ds.up_times:
        commands.append({"host_epoch": host(moment + 0.5), "cmd": "sudo systemctl start arc-updater.service", "rc": 0})
    for moment in ds.kick_times:
        commands.append({"host_epoch": host(moment + 0.5), "cmd": "sudo systemctl restart arc-node", "rc": 0})
    commands += [
        {"host_epoch": host(ds.issued + 0.5), "cmd": "sudo systemctl reboot", "rc": 255},
        {"host_epoch": host(ds.issued + 130), "cmd": "systemctl is-active arc-node", "rc": 0},
        {"host_epoch": host(ds.issued + 135), "cmd": "journalctl -b -u arc-node --no-pager | tail -n 50", "rc": 0},
        {"host_epoch": host(ds.rec + 60), "cmd": "sudo systemctl start arc-updater.service && systemctl is-active arc-node", "rc": 0},
        {"host_epoch": host(ds.rec + 600), "cmd": "cat /var/lib/arc-w0/samples.jsonl", "rc": 0},
        {"host_epoch": host(ds.E + 25), "cmd": "sudo systemctl stop arc-node", "rc": 0},
        {"host_epoch": host(ds.E + 40), "cmd": "~/.arc/bin/arc-node --legacy-bridge-rollback", "rc": 0},
        {"host_epoch": host(ds.E + 80), "cmd": "sudo systemctl restart arc-node", "rc": 0},
    ]
    ds.commands = commands

    ds.before = {"schema": E.SCHEMA_INVARIANTS, "label": "before", "guest_epoch": BASE - 100, "boot_id": BOOT1,
                 "legacy_snapshot_sha256": SNAP, "legacy_entries": 12, "v07_seed_sha256": SEED,
                 "unit_files": dict(UNIT_HASHES), "updater_timer": {"active": True, "enabled": True}}
    argv = [NODE_EXE, "--rpc", "127.0.0.1:9944", "--p2p-port", "9945", "--data-dir", NODE_DIR + "/data",
            "--stake", "0", "--min-stake", "0", "--community-mode"]
    for origin in range(6):
        argv += ["--community-rpc-url", "https://origin%d.example" % origin]
    ds.after = {"schema": E.SCHEMA_INVARIANTS, "label": "after", "guest_epoch": ds.E + 5, "boot_id": BOOT2,
                "legacy_snapshot_sha256": SNAP, "legacy_entries": 12, "v07_seed_sha256": SEED,
                "unit_files": dict(UNIT_HASHES), "updater_timer": {"active": True, "enabled": True},
                "node": {"main_pid": post_pid, "exe": NODE_EXE, "exe_sha256": NODE, "argv": argv,
                         "node_dirs": [NODE_DIR], "node_procs": 1},
                "node_info": {"stake": 0, "version": "0.8.11", "validator": "0x" + A64},
                "health": {"chain_participation_enabled": False},
                "bridge_state": {"stake": 0, "node_address": A64, "legacy_kind": "headless",
                                 "compute": "off: no verified model", "community_registration": True,
                                 "archive_generation": 1},
                "compute_consent": "no",
                "installed": {"version_txt": "0.7.12", "bin_arc_node_sha256": LAUNCHER},
                "community_status": {"public_name": "node-" + A64[:8], "coordinators_total": 6,
                                     "coordinators_registered": 3 if live_network == "allowed" else 0}}

    ds.live = [{"id": live_id, "title": "live check " + live_id, "result": "PASS", "detail": "ok"}
               for live_id in E.REQUIRED_LIVE_IDS]
    ds.live.append({"id": "L99-note", "title": "an informational note", "result": "INFO", "detail": "kept verbatim"})

    config = {"schema": E.SCHEMA_CONFIG, "profile": profile, "launcher_source": launcher_source, "tag": "v0.7.12",
              "expected_launcher_sha256": LAUNCHER, "node_tag": "v0.8.11", "node_sha256": NODE,
              "live_network": live_network, "sample_interval_s": params["interval"], "max_gap_factor": 2.5,
              "reboot_recovery_deadline_s": 300, "forced_grace_s": 90, "updater_runs": 2, "kickstarts": 3}
    if profile == "full":
        config.update({"min_total_s": 14400, "min_steady_s": 7200, "min_steady_samples": 121,
                       "post_reboot_healthy_s": 600})
    else:
        config.update({"min_total_s": 600, "min_steady_s": 300, "min_steady_samples": 31,
                       "post_reboot_healthy_s": 120})
    ds.config = config
    return ds


def read_file(path, mode="r"):
    with open(path, mode) as handle:
        return handle.read()


def evaluate_ds(ds):
    ev = E.evidence_from_texts(ds.texts())
    checks, windows, _cfg = E.analyze(ev)
    verdict = E.verdict_for(checks, ev["config"], windows)
    return checks, verdict, windows


def evaluate_texts(texts):
    ev = E.evidence_from_texts(texts)
    checks, windows, _cfg = E.analyze(ev)
    return checks, E.verdict_for(checks, ev["config"], windows), windows


def index(checks):
    return dict((c["id"], c) for c in checks if c["id"] in E.COMPUTED_IDS)


def failing(checks):
    return sorted(c["id"] for c in checks if c["result"] == "FAIL")


class Base(unittest.TestCase):
    maxDiff = None

    def expect(self, ds, fails=(), passes=(), only=False):
        """Evaluate and assert: every id in `fails` FAILs, every id in `passes` PASSes (or INFO where allowed),
        the verdict is a FAIL, and with only=True nothing else fails. Returns (checks, verdict)."""
        checks, verdict, _windows = evaluate_ds(ds)
        table = index(checks)
        for check_id in fails:
            self.assertEqual(table[check_id]["result"], "FAIL", "%s should FAIL: %s" % (check_id, table[check_id]["detail"]))
        for check_id in passes:
            self.assertIn(table[check_id]["result"], ("PASS", "INFO"),
                          "%s should pass: %s" % (check_id, table[check_id]["detail"]))
        if only:
            self.assertEqual(failing(checks), sorted(fails), "exactly these checks should fail")
        self.assertEqual(verdict["verdict"], "WAVE0_FAIL")
        self.assertNotIn("internal error", " ".join(c["detail"] for c in checks))
        return checks, verdict


# ------------------------------------------------------------------------------------------------------------------
# the perfect datasets
# ------------------------------------------------------------------------------------------------------------------

class PerfectDatasets(Base):
    def test_perfect_full_profile_is_wave0_pass(self):
        ds = build()
        checks, verdict, windows = evaluate_ds(ds)
        self.assertEqual(failing(checks), [])
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")
        table = index(checks)
        for check_id in E.COMPUTED_IDS:
            self.assertIn(check_id, table, check_id)
            self.assertEqual(table[check_id]["result"], "PASS", "%s: %s" % (check_id, table[check_id]["detail"]))
        self.assertEqual(verdict["counts"]["FAIL"], 0)
        self.assertEqual(verdict["failed_ids"], [])
        self.assertEqual(verdict["unmet_ids"], [])
        self.assertTrue(verdict["is_post_g0_wave0"])
        self.assertNotIn("internal error", " ".join(c["detail"] for c in checks))
        self.assertAlmostEqual(windows["total_s"], 15000.0)
        self.assertAlmostEqual(windows["steady_s"], 13400.0)
        self.assertEqual(windows["last_forced"], "reboot_issued")
        self.assertAlmostEqual(windows["last_forced_end"], ds.rec)

    def test_checks_are_ordered_and_live_checks_are_merged_verbatim(self):
        ds = build()
        checks, _verdict, _windows = evaluate_ds(ds)
        ids = [c["id"] for c in checks]
        self.assertEqual(ids[:len(E.COMPUTED_IDS)], list(E.COMPUTED_IDS))
        self.assertEqual(ids[len(E.COMPUTED_IDS):], [r["id"] for r in ds.live])
        for original, merged in zip(ds.live, checks[len(E.COMPUTED_IDS):]):
            self.assertEqual(original, merged)

    def test_perfect_smoke_profile_is_smoke_pass_and_never_wave0(self):
        ds = build(profile="smoke")
        checks, verdict, _windows = evaluate_ds(ds)
        self.assertEqual(failing(checks), [])
        self.assertEqual(verdict["verdict"], "SMOKE_PASS")
        self.assertNotEqual(verdict["verdict"], "WAVE0_PASS")
        self.assertEqual(index(checks)["FLOORS"]["result"], "INFO")
        self.assertIn("does NOT satisfy Astra's thresholds", verdict["statement"])
        self.assertFalse(verdict["is_post_g0_wave0"])

    def test_smoke_dataset_judged_as_full_fails_the_floors(self):
        ds = build(profile="smoke")
        ds.config["profile"] = "full"
        checks, verdict, _windows = evaluate_ds(ds)
        self.assertEqual(verdict["verdict"], "WAVE0_FAIL")
        table = index(checks)
        self.assertEqual(table["FLOORS"]["result"], "FAIL")
        self.assertEqual(table["TOTAL-DURATION"]["result"], "FAIL")
        self.assertEqual(table["STEADY-DURATION"]["result"], "FAIL")

    def test_artifact_source_statement_says_rehearsal(self):
        ds = build(launcher_source="artifact")
        checks, verdict, _windows = evaluate_ds(ds)
        self.assertEqual(failing(checks), [])
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")
        self.assertIn("local replay of canary-consume.sh (rehearsal)", verdict["statement"])
        self.assertIn("NOT the post-G0 published-tag Wave 0", verdict["statement"])
        self.assertFalse(verdict["is_post_g0_wave0"])

    def test_published_full_pass_statement(self):
        _checks, verdict, _windows = evaluate_ds(build())
        self.assertIn("published tag v0.7.12", verdict["statement"])
        self.assertIn(LAUNCHER, verdict["statement"])

    def test_smoke_artifact_has_both_disclaimers(self):
        _checks, verdict, _windows = evaluate_ds(build(profile="smoke", launcher_source="artifact"))
        self.assertEqual(verdict["verdict"], "SMOKE_PASS")
        self.assertIn("does NOT satisfy Astra's thresholds", verdict["statement"])
        self.assertIn("rehearsal", verdict["statement"])

    def test_blocked_live_network_is_info_and_passes(self):
        ds = build(live_network="blocked")
        checks, verdict, _windows = evaluate_ds(ds)
        self.assertEqual(failing(checks), [])
        self.assertEqual(index(checks)["REGISTRATION-LIVE"]["result"], "INFO")
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")

    def test_blocked_but_registered_means_the_isolation_failed(self):
        ds = build(live_network="blocked")
        for sample in ds.bridged():
            sample["coordinators_registered"] = 2
        self.expect(ds, fails=["REGISTRATION-LIVE"], only=True)

    def test_events_without_guest_epoch_use_host_epoch(self):
        ds = build()
        checks, _verdict, windows = evaluate_ds(ds)
        self.assertEqual(failing(checks), [])
        self.assertAlmostEqual(windows["end"], ds.E)
        stop = ds.event("final_stop_begin")
        self.assertIsNone(stop["guest_epoch"])

    def test_unknown_event_names_and_late_forced_events_are_handled(self):
        ds = build()
        # a forced event after final_stop_begin is not part of the soak
        ds.events.append(make_event(99, "post_stop_probe", ds.E + 500, True))
        checks, verdict, _windows = evaluate_ds(ds)
        self.assertEqual(failing(checks), [])
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")


# ------------------------------------------------------------------------------------------------------------------
# one mutation per check family
# ------------------------------------------------------------------------------------------------------------------

class SampleCountAndGaps(Base):
    def test_too_few_samples(self):
        ds = build()
        kept, counter = [], 0
        for sample in ds.samples:
            if sample["epoch"] >= ds.first_healthy + 12 * ds.I:
                counter += 1
                if counter % 2 == 0:
                    continue
            kept.append(sample)
        ds.samples = kept
        self.expect(ds, fails=["SAMPLES-TOTAL", "SAMPLES-STEADY"],
                    passes=["SAMPLE-GAPS", "STEADY-DURATION", "TOTAL-DURATION", "STEADY-UNINTERRUPTED",
                            "REBOOT-HEALTHY-WINDOW", "EVENTS-ORDER"], only=True)

    def test_five_minute_hole_in_the_steady_window(self):
        ds = build()
        lo, hi = ds.rec + 3600, ds.rec + 3900
        ds.samples = [s for s in ds.samples if not (lo < s["epoch"] < hi)]
        checks, _verdict = self.expect(ds, fails=["SAMPLE-GAPS"],
                                       passes=["SAMPLES-STEADY", "SAMPLES-TOTAL", "STEADY-UNINTERRUPTED",
                                               "HEALTH-BRIDGED"], only=True)
        self.assertIn("limit 150 s", index(checks)["SAMPLE-GAPS"]["detail"])

    def test_a_second_sampler_cannot_inflate_the_count(self):
        ds = build()
        clones = [dict(s, epoch=s["epoch"] + 30.0) for s in ds.steady()[:40]]
        ds.samples = ds.samples + clones          # same seq numbers again, written later in the file
        checks, _verdict = self.expect(ds, fails=["SAMPLE-GAPS"])
        self.assertIn("does not increase in file order", index(checks)["SAMPLE-GAPS"]["detail"])

    def test_gap_across_the_reboot_is_tolerated_up_to_the_deadline(self):
        ds = build()
        # the guest takes 5 minutes to boot: the reboot pair may be as wide as deadline + interval
        checks, _verdict, _windows = evaluate_ds(build(boot_a=235))
        self.assertEqual(index(checks)["SAMPLE-GAPS"]["result"], "PASS", index(checks)["SAMPLE-GAPS"]["detail"])
        self.assertEqual(index(checks)["REBOOT-AUTO-RECOVERY"]["result"], "PASS")
        _ = ds

    def test_a_hole_hidden_next_to_the_reboot_is_not_forgiven(self):
        ds = build()
        # remove the healthy samples right after recovery: a long hole that starts after the reboot window
        lo, hi = ds.rec + 120, ds.rec + 600
        ds.samples = [s for s in ds.samples if not (lo < s["epoch"] < hi)]
        self.expect(ds, fails=["SAMPLE-GAPS", "REBOOT-HEALTHY-WINDOW"])


class Durations(Base):
    def test_steady_window_one_hour_fifty_nine(self):
        ds = build(steady_s=7140, total_s=15000)
        checks, _verdict = self.expect(ds, fails=["STEADY-DURATION"],
                                       passes=["TOTAL-DURATION", "SAMPLE-GAPS", "EVENTS-ORDER",
                                               "STEADY-UNINTERRUPTED", "REBOOT-BOOT-ID"])
        self.assertIn("7140", index(checks)["STEADY-DURATION"]["detail"])
        self.assertIn("7200", index(checks)["STEADY-DURATION"]["detail"])

    def test_total_three_hours_fifty_nine(self):
        ds = build(steady_s=13000, total_s=14340)
        checks, _verdict = self.expect(ds, fails=["TOTAL-DURATION"], passes=["STEADY-DURATION", "SAMPLE-GAPS"])
        self.assertIn("14340", index(checks)["TOTAL-DURATION"]["detail"])

    def test_exactly_the_floors_pass(self):
        ds = build(steady_s=7260, total_s=14460)
        checks, verdict, _windows = evaluate_ds(ds)
        table = index(checks)
        self.assertEqual(table["STEADY-DURATION"]["result"], "PASS")
        self.assertEqual(table["TOTAL-DURATION"]["result"], "PASS")
        _ = verdict


class SteadyState(Base):
    def test_restart_with_a_new_pid_at_hour_three(self):
        ds = build()
        for sample in ds.steady():
            if sample["epoch"] >= ds.rec + 10800:
                sample["main_pid"] = 5555
                sample["proc_start_epoch"] = int(ds.rec + 10800)
        checks, _verdict = self.expect(ds, fails=["STEADY-UNINTERRUPTED"], only=True)
        self.assertIn("main_pid changed", index(checks)["STEADY-UNINTERRUPTED"]["detail"])

    def test_unhealthy_sample_in_the_steady_window(self):
        ds = build()
        ds.steady()[100]["health_ok"] = False
        self.expect(ds, fails=["STEADY-UNINTERRUPTED", "HEALTH-BRIDGED"], passes=["SAMPLE-GAPS"])

    def test_unhealthy_sample_inside_a_kickstart_grace_is_tolerated(self):
        ds = build()
        checks, _verdict, _windows = evaluate_ds(ds)
        # the perfect dataset already contains three bridged, unhealthy samples right after the kickstarts
        unhealthy = [s for s in ds.bridged() if not s["health_ok"]]
        self.assertEqual(len(unhealthy), 3)
        self.assertEqual(index(checks)["HEALTH-BRIDGED"]["result"], "PASS")

    def test_unhealthy_sample_just_outside_the_grace_fails(self):
        ds = build()
        kick = ds.kick_times[0]
        victims = [s for s in ds.bridged() if kick + 95 < s["epoch"] < kick + 160]
        self.assertTrue(victims)
        victims[0]["health_ok"] = False
        self.expect(ds, fails=["HEALTH-BRIDGED"])

    def test_sample_error_in_the_steady_window(self):
        ds = build()
        ds.steady()[10]["errors"] = ["node_info: connection refused"]
        self.expect(ds, fails=["STEADY-UNINTERRUPTED"], only=True)

    def test_info_prefixed_errors_are_ignorable(self):
        ds = build()
        ds.steady()[10]["errors"] = ["info: legacy compare skipped this round"]
        checks, verdict, _windows = evaluate_ds(ds)
        self.assertEqual(failing(checks), [])
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")

    def test_wrong_launcher_or_version_in_the_steady_window(self):
        ds = build()
        ds.steady()[5]["launcher_sha256"] = "99" * 32
        self.expect(ds, fails=["STEADY-UNINTERRUPTED"])
        ds = build()
        ds.steady()[5]["version_txt"] = "0.7.11"
        self.expect(ds, fails=["STEADY-UNINTERRUPTED"])

    def test_systemd_nrestarts_changing_is_a_restart(self):
        ds = build()
        for sample in ds.steady():
            if sample["epoch"] > ds.rec + 5000:
                sample["n_restarts"] = 1
        self.expect(ds, fails=["STEADY-UNINTERRUPTED"], only=True)

    def test_node_exe_not_bridged_in_the_steady_window(self):
        ds = build()
        for sample in ds.steady():
            if sample["epoch"] > ds.rec + 3000:
                sample["node_exe"] = V07_EXE
        self.expect(ds, fails=["STEADY-UNINTERRUPTED"])


class Invariants(Base):
    def test_identity_change(self):
        ds = build()
        other = A64[:8] + "cd" * 28
        for sample in ds.steady():
            if sample["epoch"] >= ds.rec + 3600:
                sample["address"] = other
                sample["bridge_node_address"] = other
        self.expect(ds, fails=["IDENTITY-STABLE"], only=True)

    def test_bridge_state_and_node_info_disagree(self):
        ds = build()
        ds.steady()[7]["bridge_node_address"] = "cd" * 32
        self.expect(ds, fails=["IDENTITY-STABLE"], only=True)

    def test_stake_one(self):
        ds = build()
        ds.steady()[50]["stake"] = 1
        self.expect(ds, fails=["STAKE-ZERO"], only=True)

    def test_compute_consent_yes(self):
        ds = build()
        ds.steady()[50]["compute_consent"] = "yes"
        self.expect(ds, fails=["COMPUTE-OFF"], only=True)

    def test_compute_bridge_state_on(self):
        ds = build()
        ds.steady()[50]["bridge_compute"] = "on: model verified"
        self.expect(ds, fails=["COMPUTE-OFF"], only=True)

    def test_chain_participation_enabled(self):
        ds = build()
        ds.steady()[50]["chain_participation_enabled"] = True
        self.expect(ds, fails=["COMPUTE-OFF"], only=True)

    def test_two_node_processes(self):
        ds = build()
        ds.steady()[120]["node_procs"] = 2
        self.expect(ds, fails=["ONE-NODE", "STEADY-UNINTERRUPTED"])

    def test_two_node_processes_in_a_non_bridged_sample_after_t0(self):
        ds = build()
        victim = [s for s in ds.samples if s["epoch"] == ds.first_post][0]
        victim["node_procs"] = 2
        self.expect(ds, fails=["ONE-NODE"], only=True)

    def test_legacy_fingerprint_change(self):
        ds = build()
        ds.steady()[80]["legacy_fingerprint"] = "ff" * 32
        self.expect(ds, fails=["LEGACY-UNCHANGED-SAMPLES"], only=True)

    def test_legacy_byte_compare_diff(self):
        ds = build()
        ds.steady()[80]["legacy_byte_compare"] = "diff"
        self.expect(ds, fails=["LEGACY-UNCHANGED-SAMPLES"], only=True)

    def test_before_after_snapshot_digest_differs(self):
        ds = build()
        ds.after["legacy_snapshot_sha256"] = "66" * 32
        checks, _verdict = self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)
        self.assertIn("legacy_snapshot_sha256 changed", index(checks)["INVARIANTS-BEFORE-AFTER"]["detail"])

    def test_unit_file_hash_differs(self):
        ds = build()
        ds.after["unit_files"]["arc-updater.timer"] = "66" * 32
        checks, _verdict = self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)
        self.assertIn("arc-updater.timer changed", index(checks)["INVARIANTS-BEFORE-AFTER"]["detail"])

    def test_seed_hash_differs(self):
        ds = build()
        ds.after["v07_seed_sha256"] = "66" * 32
        self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)

    def test_updater_timer_state_differs_or_is_off(self):
        ds = build()
        ds.after["updater_timer"] = {"active": False, "enabled": True}
        self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)

    def test_argv_with_a_model(self):
        ds = build()
        ds.after["node"]["argv"] = ds.after["node"]["argv"] + ["--model", "/models/x.gguf"]
        checks, _verdict = self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)
        self.assertIn("--model", index(checks)["INVARIANTS-BEFORE-AFTER"]["detail"])

    def test_argv_with_other_forbidden_flags(self):
        for flag in ("--validator-seed", "--insecure-dev-validator-seed", "--shard-range", "--no-community"):
            ds = build()
            ds.after["node"]["argv"] = ds.after["node"]["argv"] + [flag, "x"]
            self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)

    def test_argv_without_community_mode(self):
        ds = build()
        ds.after["node"]["argv"] = [a for a in ds.after["node"]["argv"] if a != "--community-mode"]
        checks, _verdict = self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)
        self.assertIn("--community-mode", index(checks)["INVARIANTS-BEFORE-AFTER"]["detail"])

    def test_argv_stake_not_zero(self):
        ds = build()
        argv = ds.after["node"]["argv"]
        argv[argv.index("--stake") + 1] = "5000000"
        self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)

    def test_two_node_directories_or_processes_in_the_after_invariants(self):
        ds = build()
        ds.after["node"]["node_dirs"] = [NODE_DIR, NODE_DIR + "-2"]
        self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)
        ds = build()
        ds.after["node"]["node_procs"] = 2
        self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)

    def test_after_validator_differs_from_t0_address(self):
        ds = build()
        ds.after["node_info"]["validator"] = "0x" + "cd" * 32
        self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)

    def test_after_installed_launcher_or_version_wrong(self):
        ds = build()
        ds.after["installed"]["bin_arc_node_sha256"] = "99" * 32
        self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)
        ds = build()
        ds.after["installed"]["version_txt"] = "0.7.11"
        self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)

    def test_after_compute_and_participation(self):
        ds = build()
        ds.after["compute_consent"] = "yes"
        self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)
        ds = build()
        ds.after["health"]["chain_participation_enabled"] = True
        self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)

    def test_wrong_schema_or_label(self):
        ds = build()
        ds.before["schema"] = "something.else"
        self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)
        ds = build()
        ds.after["label"] = "before"
        self.expect(ds, fails=["INVARIANTS-BEFORE-AFTER"], only=True)


class PrivacyAndRegistration(Base):
    def test_public_name_not_node_xxxxxxxx(self):
        ds = build()
        for sample in ds.steady()[:5]:
            sample["public_name"] = "arc-wave0-runner"
        checks, _verdict = self.expect(ds, fails=["PRIVACY-SAFE-NAME"], only=True)
        self.assertIn("not node-xxxxxxxx", index(checks)["PRIVACY-SAFE-NAME"]["detail"])

    def test_public_name_with_the_wrong_shape_even_if_it_starts_with_the_right_prefix(self):
        ds = build()
        ds.steady()[4]["public_name"] = "node-" + A64[:8] + "00"
        checks, _verdict = self.expect(ds, fails=["PRIVACY-SAFE-NAME"], only=True)
        self.assertIn("not node-xxxxxxxx", index(checks)["PRIVACY-SAFE-NAME"]["detail"])
        ds = build()
        ds.steady()[4]["public_name"] = "node-" + A64[:8].upper()
        self.expect(ds, fails=["PRIVACY-SAFE-NAME"], only=True)

    def test_public_name_does_not_match_the_address(self):
        ds = build()
        ds.steady()[3]["public_name"] = "node-deadbeef"
        checks, _verdict = self.expect(ds, fails=["PRIVACY-SAFE-NAME"], only=True)
        self.assertIn("first 8 hex", index(checks)["PRIVACY-SAFE-NAME"]["detail"])

    def test_no_public_name_at_all(self):
        ds = build()
        for sample in ds.samples:
            sample["public_name"] = None
        self.expect(ds, fails=["PRIVACY-SAFE-NAME"], only=True)

    def test_after_community_status_name_is_checked_too(self):
        ds = build()
        ds.after["community_status"]["public_name"] = "Adas-MacBook-Pro"
        self.expect(ds, fails=["PRIVACY-SAFE-NAME", "INVARIANTS-BEFORE-AFTER"], only=True)

    def test_live_registration_zero_when_allowed(self):
        ds = build()
        for sample in ds.bridged():
            if sample["coordinators_registered"] is not None:
                sample["coordinators_registered"] = 0
        self.expect(ds, fails=["REGISTRATION-LIVE"], only=True)


class RebootChecks(Base):
    def test_boot_id_unchanged_after_the_reboot(self):
        ds = build()
        for sample in ds.samples:
            if sample["boot_id"] == BOOT2:
                sample["boot_id"] = BOOT1
        ds.event("reboot_recovered")["detail"]["boot_id_after"] = BOOT1
        ds.after["boot_id"] = BOOT1
        checks, _verdict = self.expect(ds, fails=["REBOOT-BOOT-ID"],
                                       passes=["STEADY-UNINTERRUPTED", "EVENTS-ORDER", "REBOOT-AUTO-RECOVERY"])
        self.assertIn("did not change", index(checks)["REBOOT-BOOT-ID"]["detail"])

    def test_boot_id_in_samples_changes_twice(self):
        ds = build()
        for sample in ds.steady()[100:]:
            sample["boot_id"] = "33333333-3333-3333-3333-333333333333"
        self.expect(ds, fails=["REBOOT-BOOT-ID", "STEADY-UNINTERRUPTED"])

    def test_events_and_samples_disagree_about_the_new_boot(self):
        ds = build()
        ds.event("reboot_recovered")["detail"]["boot_id_after"] = "44444444-4444-4444-4444-444444444444"
        self.expect(ds, fails=["REBOOT-BOOT-ID"])

    def test_container_style_restart_keeps_uptime(self):
        ds = build()
        for sample in ds.samples:
            if sample["boot_id"] == BOOT2:
                sample["uptime_s"] = sample["uptime_s"] + 100000
        checks, _verdict = self.expect(ds, fails=["REBOOT-BOOT-ID"])
        self.assertIn("uptime", index(checks)["REBOOT-BOOT-ID"]["detail"])

    def test_invariants_taken_in_the_wrong_boot(self):
        ds = build()
        ds.after["boot_id"] = BOOT1
        self.expect(ds, fails=["REBOOT-BOOT-ID"], only=True)

    def test_first_healthy_sample_ten_minutes_after_the_reboot(self):
        ds = build(post_unhealthy=10)
        checks, _verdict = self.expect(ds, fails=["REBOOT-AUTO-RECOVERY"],
                                       passes=["REBOOT-BOOT-ID", "REBOOT-HEALTHY-WINDOW", "HEALTH-BRIDGED",
                                               "STEADY-UNINTERRUPTED", "SAMPLE-GAPS", "EVENTS-ORDER"], only=True)
        self.assertIn("limit 300", index(checks)["REBOOT-AUTO-RECOVERY"]["detail"])

    def test_orchestrator_first_healthy_claim_must_match_the_samples(self):
        ds = build()
        ds.event("reboot_recovered")["detail"]["first_healthy_sample_epoch"] = ds.first_healthy - 120
        self.expect(ds, fails=["REBOOT-AUTO-RECOVERY"], only=True)

    def test_manual_node_restart_after_the_reboot(self):
        ds = build()
        ds.commands.append({"host_epoch": ds.issued + 200.4, "cmd": "sudo systemctl restart arc-node", "rc": 0})
        checks, _verdict = self.expect(ds, fails=["REBOOT-AUTO-RECOVERY"], only=True)
        self.assertIn("manual node start", index(checks)["REBOOT-AUTO-RECOVERY"]["detail"])

    def test_other_ways_to_start_the_node_by_hand(self):
        for text in ("sudo systemctl start arc-node.service", "sudo systemctl try-restart arc-node",
                     "sudo systemctl kickstart arc-node", "bash canary-consume.sh --tag v0.7.12 --apply",
                     "~/.arc/bin/arc-node --legacy-bridge-rollback", "launchctl kickstart -k gui/501/x"):
            ds = build()
            ds.commands.append({"host_epoch": ds.rec + 400.4, "cmd": text, "rc": 0})
            self.expect(ds, fails=["REBOOT-AUTO-RECOVERY"], only=True)

    def test_reading_commands_after_the_reboot_are_fine(self):
        ds = build()
        for text in ("systemctl show -p MainPID --value arc-node", "systemctl status arc-node --no-pager",
                     "sudo systemctl start arc-updater.service", "journalctl -u arc-node -b -1 --no-pager",
                     "sudo systemctl restart arc-w0-sampler"):
            ds.commands.append({"host_epoch": ds.rec + 400.4, "cmd": text, "rc": 0})
        checks, verdict, _windows = evaluate_ds(ds)
        self.assertEqual(failing(checks), [])
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")

    def test_commands_after_the_final_stop_are_ignored(self):
        ds = build()
        ds.commands.append({"host_epoch": ds.E + 100.4, "cmd": "sudo systemctl restart arc-node", "rc": 0})
        checks, _verdict, _windows = evaluate_ds(ds)
        self.assertEqual(failing(checks), [])

    def test_updater_timer_inactive_after_the_reboot(self):
        ds = build()
        victims = [s for s in ds.bridged() if s["boot_id"] == BOOT2][:5]
        for sample in victims:
            sample["updater_timer_active"] = False
        self.expect(ds, fails=["REBOOT-UPDATER-TIMER"], only=True)

    def test_only_nine_minutes_healthy_after_the_reboot(self):
        ds = build()
        post = [s for s in ds.bridged() if s["boot_id"] == BOOT2]
        post[10]["health_ok"] = False
        checks, _verdict = self.expect(ds, fails=["REBOOT-HEALTHY-WINDOW"])
        self.assertIn("spanning 540 s", index(checks)["REBOOT-HEALTHY-WINDOW"]["detail"])

    def test_ten_minutes_healthy_is_enough(self):
        ds = build()
        post = [s for s in ds.bridged() if s["boot_id"] == BOOT2]
        post[11]["health_ok"] = False
        checks, _verdict = self.expect(ds, passes=["REBOOT-HEALTHY-WINDOW"])
        _ = checks

    def test_a_gap_inside_the_post_reboot_window(self):
        ds = build()
        lo, hi = ds.first_healthy + 120, ds.first_healthy + 300
        ds.samples = [s for s in ds.samples if not (lo < s["epoch"] < hi)]
        self.expect(ds, fails=["REBOOT-HEALTHY-WINDOW", "SAMPLE-GAPS"])

    def test_launcher_changes_across_the_reboot(self):
        ds = build()
        for sample in ds.samples:
            if sample["boot_id"] == BOOT2 and sample["launcher_sha256"]:
                sample["launcher_sha256"] = "99" * 32
        self.expect(ds, fails=["REBOOT-SAME-LAUNCHER", "STEADY-UNINTERRUPTED"])

    def test_a_different_launcher_ran_before_the_reboot(self):
        ds = build()
        for sample in ds.samples:
            if sample["boot_id"] == BOOT1 and sample["node_exe"] == NODE_EXE:
                sample["launcher_sha256"] = "99" * 32
        checks, _verdict = self.expect(ds, fails=["REBOOT-SAME-LAUNCHER"], only=True)
        self.assertIn("launcher_sha256 changed across the reboot", index(checks)["REBOOT-SAME-LAUNCHER"]["detail"])

    def test_node_binary_changes_across_the_reboot(self):
        ds = build()
        for sample in ds.samples:
            if sample["boot_id"] == BOOT2 and sample["node_exe_sha256"]:
                sample["node_exe_sha256"] = "98" * 32
        self.expect(ds, fails=["REBOOT-SAME-LAUNCHER"])

    def test_missing_reboot_events(self):
        ds = build()
        ds.events = [e for e in ds.events if e["name"] not in ("reboot_issued", "reboot_recovered")]
        self.expect(ds, fails=["EVENTS-ORDER", "REBOOT-BOOT-ID", "REBOOT-AUTO-RECOVERY"])


class EventOrder(Base):
    def test_steady_begin_before_the_last_forced_event(self):
        ds = build()
        set_event_time(ds.event("steady_begin"), ds.rec - 1800)
        checks, _verdict = self.expect(ds, fails=["EVENTS-ORDER"], only=True)
        self.assertIn("before the last forced event", index(checks)["EVENTS-ORDER"]["detail"])

    def test_steady_begin_names_the_wrong_forced_event(self):
        ds = build()
        ds.event("steady_begin")["detail"]["last_forced_event"] = "kickstart_3"
        self.expect(ds, fails=["EVENTS-ORDER"], only=True)

    def test_missing_required_events(self):
        for name in ("apply1", "apply2", "t0_bridged_healthy", "steady_begin", "final_stop_begin"):
            ds = build()
            ds.events = [e for e in ds.events if e["name"] != name]
            checks, _verdict = self.expect(ds, fails=["EVENTS-ORDER"])
            self.assertIn("event %s is missing" % name, index(checks)["EVENTS-ORDER"]["detail"])

    def test_too_few_updater_runs_or_kickstarts(self):
        ds = build()
        ds.events = [e for e in ds.events if e["name"] != "updater_run_2"]
        self.expect(ds, fails=["EVENTS-ORDER"], only=True)
        ds = build()
        ds.events = [e for e in ds.events if e["name"] != "kickstart_3"]
        # the unhealthy sample that followed that kickstart is no longer explained by any forced event
        self.expect(ds, fails=["EVENTS-ORDER", "HEALTH-BRIDGED"])

    def test_numbering_gap(self):
        ds = build()
        ds.event("kickstart_2")["name"] = "kickstart_5"
        self.expect(ds, fails=["EVENTS-ORDER"])

    def test_kickstart_before_updater(self):
        ds = build()
        set_event_time(ds.event("updater_run_2"), ds.kick_times[1] + 5)
        self.expect(ds, fails=["EVENTS-ORDER"], only=True)

    def test_forced_flag_mislabelled(self):
        ds = build()
        ds.event("kickstart_1")["forced"] = False
        self.expect(ds, fails=["EVENTS-ORDER"])
        ds = build()
        ds.event("reboot_recovered")["forced"] = True
        self.expect(ds, fails=["EVENTS-ORDER"])

    def test_duplicate_unique_event(self):
        ds = build()
        ds.events.append(make_event(98, "t0_bridged_healthy", ds.t0 + 100, False, {"address": A64, "legacy_fingerprint": LEGACY_FP}))
        self.expect(ds, fails=["EVENTS-ORDER"])

    def test_forced_event_after_the_reboot_moves_the_steady_window(self):
        ds = build()
        ds.events.append(make_event(97, "kickstart_4", ds.rec + 4000, True))
        checks, _verdict, windows = evaluate_ds(ds)
        # the extra kickstart after the reboot is out of order, and the steady window now starts after it
        self.assertEqual(windows["last_forced"], "kickstart_4")
        self.assertEqual(index(checks)["EVENTS-ORDER"]["result"], "FAIL")


class LiveChecks(Base):
    def test_a_live_check_fails(self):
        ds = build()
        ds.live[3]["result"] = "FAIL"
        ds.live[3]["detail"] = "partial file never grew"
        checks, verdict = self.expect(ds, fails=["LIVE-RESULTS"], only=False)
        self.assertIn(ds.live[3]["id"], index(checks)["LIVE-RESULTS"]["detail"])
        self.assertIn(ds.live[3]["id"], verdict["failed_ids"])
        self.assertEqual(failing(checks), sorted(["LIVE-RESULTS", ds.live[3]["id"]]))

    def test_a_required_live_id_is_missing(self):
        ds = build()
        ds.live = [r for r in ds.live if r["id"] != "L10-reboot-boot-id-changed"]
        checks, _verdict = self.expect(ds, fails=["LIVE-REQUIRED"], only=True)
        self.assertIn("L10-reboot-boot-id-changed", index(checks)["LIVE-REQUIRED"]["detail"])

    def test_live_info_never_fails(self):
        ds = build()
        ds.live[0]["result"] = "INFO"
        checks, verdict, _windows = evaluate_ds(ds)
        self.assertEqual(failing(checks), [])
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")

    def test_live_id_colliding_with_an_evaluator_id(self):
        ds = build()
        ds.live.append({"id": "FLOORS", "title": "x", "result": "PASS", "detail": "x"})
        self.expect(ds, fails=["LIVE-RESULTS"], only=True)


class ConfigFloors(Base):
    def test_config_lowering_a_floor_under_the_full_profile(self):
        for key, value in (("min_total_s", 7200), ("min_steady_s", 3600), ("min_steady_samples", 61),
                           ("post_reboot_healthy_s", 300), ("updater_runs", 1), ("kickstarts", 1),
                           ("sample_interval_s", 120)):
            ds = build()
            ds.config[key] = value
            checks, _verdict = self.expect(ds, fails=["FLOORS"])
            self.assertIn(key, index(checks)["FLOORS"]["detail"])

    def test_floors_keep_being_enforced_when_the_file_lowers_them(self):
        ds = build(steady_s=3600, total_s=7300)
        ds.config.update({"min_total_s": 7200, "min_steady_s": 3600, "min_steady_samples": 61})
        checks, verdict = self.expect(ds, fails=["FLOORS", "STEADY-DURATION", "TOTAL-DURATION"])
        self.assertEqual(verdict["profile"], "full")
        _ = checks

    def test_invalid_config_values(self):
        for key, value in (("profile", "turbo"), ("live_network", "maybe"), ("launcher_source", "git"),
                           ("tag", "0.7.12"), ("expected_launcher_sha256", "abc"), ("max_gap_factor", -1),
                           ("schema", "other.schema")):
            ds = build()
            ds.config[key] = value
            self.expect(ds, fails=["FLOORS"])

    def test_smoke_profile_never_enforces_floors_but_keeps_the_thresholds_it_names(self):
        ds = build(profile="smoke")
        ds.config["min_steady_s"] = 5000        # the smoke run is only 400 s long
        checks, verdict = self.expect_smoke_fail(ds)
        self.assertEqual(verdict["verdict"], "SMOKE_FAIL")
        self.assertEqual(index(checks)["STEADY-DURATION"]["result"], "FAIL")

    def expect_smoke_fail(self, ds):
        checks, verdict, _windows = evaluate_ds(ds)
        self.assertNotIn("WAVE0", verdict["verdict"])
        return checks, verdict


class BadInput(Base):
    def test_malformed_sample_lines_are_counted_and_fail_the_dependent_checks(self):
        ds = build()
        texts = ds.texts()
        lines = texts["samples.jsonl"].split("\n")
        broken = dict(json.loads(lines[40]))
        del broken["epoch"]
        lines.insert(10, "{not json")
        lines.insert(20, json.dumps(broken))
        lines.insert(30, "[1, 2, 3]")
        texts["samples.jsonl"] = "\n".join(lines)
        checks, verdict, _windows = evaluate_texts(texts)
        table = index(checks)
        self.assertEqual(verdict["verdict"], "WAVE0_FAIL")
        self.assertEqual(table["EVIDENCE-FILES"]["result"], "FAIL")
        self.assertIn("2 line(s) of samples.jsonl are not valid JSON objects", table["EVIDENCE-FILES"]["detail"])
        self.assertIn("1 record(s) of samples.jsonl miss required fields", table["EVIDENCE-FILES"]["detail"])
        for check_id in ("SAMPLES-TOTAL", "SAMPLES-STEADY", "SAMPLE-GAPS", "STEADY-UNINTERRUPTED", "HEALTH-BRIDGED",
                         "IDENTITY-STABLE", "STAKE-ZERO", "COMPUTE-OFF", "ONE-NODE", "LEGACY-UNCHANGED-SAMPLES",
                         "PRIVACY-SAFE-NAME", "REGISTRATION-LIVE", "REBOOT-BOOT-ID", "REBOOT-AUTO-RECOVERY",
                         "REBOOT-UPDATER-TIMER", "REBOOT-HEALTHY-WINDOW", "REBOOT-SAME-LAUNCHER"):
            self.assertEqual(table[check_id]["result"], "FAIL", check_id)
            self.assertIn("samples.jsonl", table[check_id]["detail"], check_id)
        # checks that do not read the samples are unaffected
        for check_id in ("FLOORS", "EVENTS-ORDER", "INVARIANTS-BEFORE-AFTER", "LIVE-REQUIRED", "LIVE-RESULTS"):
            self.assertEqual(table[check_id]["result"], "PASS", check_id)

    def test_malformed_event_missing_required_field(self):
        ds = build()
        texts = ds.texts()
        broken = dict(ds.events[3])
        del broken["forced"]
        texts["events.jsonl"] = texts["events.jsonl"] + json.dumps(broken) + "\n"
        checks, verdict, _windows = evaluate_texts(texts)
        table = index(checks)
        self.assertEqual(verdict["verdict"], "WAVE0_FAIL")
        self.assertIn("1 record(s) of events.jsonl miss required fields", table["EVENTS-ORDER"]["detail"])
        self.assertIn("forced", table["EVIDENCE-FILES"]["detail"])

    def test_wrong_types_count_as_malformed(self):
        ds = build()
        ds.samples[5]["health_ok"] = "yes"
        ds.samples[6]["seq"] = True
        ds.samples[7]["epoch"] = float("nan")
        texts = ds.texts()
        checks, _verdict, _windows = evaluate_texts(texts)
        self.assertIn("3 record(s) of samples.jsonl", index(checks)["EVIDENCE-FILES"]["detail"])

    def test_each_file_missing_alone(self):
        expectations = {
            "config-effective.json": ["FLOORS", "EVIDENCE-FILES"],
            "samples.jsonl": ["SAMPLES-TOTAL", "STEADY-UNINTERRUPTED", "EVIDENCE-FILES"],
            "events.jsonl": ["EVENTS-ORDER", "SAMPLES-TOTAL", "REBOOT-BOOT-ID", "EVIDENCE-FILES"],
            "commands.jsonl": ["REBOOT-AUTO-RECOVERY", "EVIDENCE-FILES"],
            "invariants-before.json": ["INVARIANTS-BEFORE-AFTER", "EVIDENCE-FILES"],
            "invariants-after.json": ["INVARIANTS-BEFORE-AFTER", "EVIDENCE-FILES"],
            "checks-live.jsonl": ["LIVE-REQUIRED", "LIVE-RESULTS", "EVIDENCE-FILES"],
        }
        for name, expected in sorted(expectations.items()):
            texts = build().texts()
            texts[name] = None
            checks, verdict, _windows = evaluate_texts(texts)
            table = index(checks)
            self.assertEqual(verdict["verdict"], "WAVE0_FAIL", name)
            for check_id in expected:
                self.assertEqual(table[check_id]["result"], "FAIL", "%s without %s" % (check_id, name))
                self.assertIn("missing", table[check_id]["detail"] + "no live checks", check_id)
            self.assertNotIn("internal error", " ".join(c["detail"] for c in checks), name)

    def test_live_file_missing_says_the_orchestrator_wrote_none(self):
        texts = build().texts()
        texts["checks-live.jsonl"] = None
        checks, _verdict, _windows = evaluate_texts(texts)
        self.assertIn("orchestrator wrote no live checks", index(checks)["LIVE-REQUIRED"]["detail"])

    def test_unparseable_json_documents(self):
        for name in ("config-effective.json", "invariants-before.json", "invariants-after.json"):
            texts = build().texts()
            texts[name] = "{ definitely not json"
            checks, verdict, _windows = evaluate_texts(texts)
            self.assertEqual(verdict["verdict"], "WAVE0_FAIL", name)
            self.assertIn("is not valid JSON", index(checks)["EVIDENCE-FILES"]["detail"], name)
            texts[name] = "[1, 2]"
            checks, verdict, _windows = evaluate_texts(texts)
            self.assertIn("is not a JSON object", index(checks)["EVIDENCE-FILES"]["detail"], name)

    def test_empty_directory_and_empty_files_never_crash(self):
        checks, verdict, _windows = evaluate_texts({})
        self.assertEqual(verdict["verdict"], "WAVE0_FAIL")
        self.assertNotIn("internal error", " ".join(c["detail"] for c in checks))
        texts = dict((name, "") for _key, name in E.FILES)
        checks, verdict, _windows = evaluate_texts(texts)
        self.assertEqual(verdict["verdict"], "WAVE0_FAIL")
        self.assertNotIn("internal error", " ".join(c["detail"] for c in checks))

    def test_binary_garbage_is_survived(self):
        with tempfile.TemporaryDirectory() as tmp:
            for _key, name in E.FILES:
                with open(os.path.join(tmp, name), "wb") as handle:
                    handle.write(b"\xff\xfe\x00garbage\n\x80\x81\n")
            ev = E.load_evidence(tmp)
            checks, _windows, _cfg = E.analyze(ev)
            self.assertNotIn("internal error", " ".join(c["detail"] for c in checks))
            self.assertEqual(E.verdict_for(checks, ev["config"])["verdict"], "WAVE0_FAIL")

    def test_non_dict_event_detail_and_odd_values_do_not_crash(self):
        ds = build()
        texts = ds.texts()
        weird = make_event(60, "t0_bridged_healthy", ds.t0 + 1, False)
        weird["detail"] = {"address": 12345, "legacy_fingerprint": ["x"]}
        texts["events.jsonl"] += json.dumps(weird) + "\n"
        checks, _verdict, _windows = evaluate_texts(texts)
        self.assertNotIn("internal error", " ".join(c["detail"] for c in checks))


class Robustness(Base):
    def test_event_detail_null_and_seq_null_do_not_crash(self):
        ds = build()
        texts = ds.texts()
        odd = make_event(70, "heartbeat", ds.t0 + 5, False)
        odd["detail"] = None
        odd["seq"] = None
        texts["events.jsonl"] += json.dumps(odd) + "\n"
        for name in ("reboot_recovered", "steady_begin"):
            event = dict(ds.event(name))
            event["detail"] = None
            event["name"] = "noise_" + name
            texts["events.jsonl"] += json.dumps(event) + "\n"
        checks, verdict, _windows = evaluate_texts(texts)
        self.assertNotIn("internal error", " ".join(c["detail"] for c in checks))
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")

    def test_context_failure_is_reported_not_raised(self):
        original = E.Ctx

        class Broken(object):
            def __init__(self, ev):
                raise RuntimeError("boom")

        E.Ctx = Broken
        try:
            checks, _windows, _cfg = E.analyze(E.evidence_from_texts(build().texts()))
        finally:
            E.Ctx = original
        computed = [c for c in checks if c["id"] in E.COMPUTED_IDS]
        self.assertEqual(len(computed), len(E.COMPUTED_IDS))
        self.assertTrue(all(c["result"] == "FAIL" and "internal error" in c["detail"] for c in computed))
        self.assertEqual(E.verdict_for(checks, build().config)["verdict"], "WAVE0_FAIL")

    def test_a_crashing_check_is_reported_as_fail(self):
        saved = list(E.CHECKS)

        def boom(c, o):
            raise ValueError("kaboom")

        E.CHECKS[3] = (saved[3][0], saved[3][1], saved[3][2], boom)
        try:
            checks, _windows, _cfg = E.analyze(E.evidence_from_texts(build().texts()))
        finally:
            E.CHECKS[:] = saved
        table = index(checks)
        self.assertEqual(table[saved[3][0]]["result"], "FAIL")
        self.assertIn("internal error in %s" % saved[3][0], table[saved[3][0]]["detail"])
        self.assertEqual(sum(1 for c in checks if c["result"] == "FAIL"), 1)

    def test_no_steady_end_event_uses_the_last_bridged_sample(self):
        ds = build()
        ds.events = [e for e in ds.events if e["name"] != "steady_end"]
        checks, verdict, windows = evaluate_ds(ds)
        self.assertEqual(failing(checks), [])
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")
        self.assertLessEqual(windows["end"], ds.E)
        self.assertGreater(windows["end"], ds.E - ds.I)

    def test_steady_end_claiming_more_than_the_samples_cover_fails_the_gap_check(self):
        ds = build()
        set_event_time(ds.event("steady_end"), ds.E + 600)
        set_event_time(ds.event("final_stop_begin"), ds.E + 700)
        for name in ("steady_end",):
            ds.event(name)["guest_epoch"] = ds.E + 600
        self.expect(ds, fails=["SAMPLE-GAPS"])

    def test_samples_after_the_final_stop_are_ignored(self):
        ds = build()
        stop = ds.E + 20.4
        extra = []
        for number in range(5):
            sample = v07_sample(stop + 60 * (number + 1), BOOT2, ds.issued + 8)
            sample["seq"] = 100000 + number
            sample["stake"] = 7
            sample["node_procs"] = 3
            extra.append(sample)
        bad_bridged = bridged_sample(stop + 400, BOOT2, ds.issued + 8, 4242, 1, healthy=False)
        bad_bridged["seq"] = 100010
        bad_bridged["stake"] = 9
        bad_bridged["info_ok"] = True
        extra.append(bad_bridged)
        ds.samples = ds.samples + extra
        checks, verdict, _windows = evaluate_ds(ds)
        self.assertEqual(failing(checks), [])
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")

    def test_host_clock_skew_does_not_matter_when_guest_epochs_exist(self):
        ds = build()
        for event in ds.events:
            event["host_epoch"] += 3600.0
        for command in ds.commands:
            command["host_epoch"] += 3600.0
        for event in ds.events:
            if event["guest_epoch"] is None:
                event["guest_epoch"] = event["host_epoch"] - 3600.4
        checks, verdict, _windows = evaluate_ds(ds)
        self.assertEqual(failing(checks), [])
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")

    def test_verdict_document_has_the_agreed_keys(self):
        ds = build()
        _checks, verdict, _windows = evaluate_ds(ds)
        for key in ("schema", "verdict", "profile", "launcher_source", "tag", "expected_launcher_sha256", "counts",
                    "failed_ids", "windows", "statement"):
            self.assertIn(key, verdict)
        for key in ("t0", "last_forced_end", "steady_begin", "end", "steady_s", "total_s"):
            self.assertIn(key, verdict["windows"])
        self.assertEqual(verdict["schema"], "arc.legacy-bridge.wave0-lab.stage-b-verdict.v1")
        self.assertEqual(verdict["tag"], "v0.7.12")
        self.assertEqual(verdict["expected_launcher_sha256"], LAUNCHER)
        self.assertEqual(verdict["counts"], {"PASS": len(E.COMPUTED_IDS) + len(E.REQUIRED_LIVE_IDS), "FAIL": 0,
                                             "INFO": 1, "SKIP": 0})
        self.assertAlmostEqual(verdict["windows"]["steady_begin"], ds.rec + 2)

    def test_report_escapes_table_cells(self):
        checks = [{"id": "X-1", "title": "a | b", "result": "FAIL", "detail": "line one\nline | two"}]
        verdict = E.verdict_for(checks, build().config)
        report = E.render_report(checks, verdict)
        self.assertIn("a \\| b", report)
        self.assertIn("line one line \\| two", report)

    def test_fail_details_carry_the_numbers(self):
        ds = build(steady_s=7140, total_s=15000)
        checks, _verdict, _windows = evaluate_ds(ds)
        detail = index(checks)["STEADY-DURATION"]["detail"]
        self.assertIn("1h59m00s", detail)
        self.assertIn("required >= 7200", detail)


class VerdictLogic(Base):
    def test_skip_never_counts_as_pass(self):
        checks = [{"id": check_id, "title": "t", "result": "PASS", "detail": ""} for check_id in E.COMPUTED_IDS]
        config = build().config
        self.assertEqual(E.verdict_for(checks, config)["verdict"], "WAVE0_PASS")
        checks[3]["result"] = "SKIP"
        verdict = E.verdict_for(checks, config)
        self.assertEqual(verdict["verdict"], "WAVE0_FAIL")
        self.assertEqual(verdict["unmet_ids"], [checks[3]["id"]])

    def test_info_only_allowed_on_designated_ids(self):
        checks = [{"id": check_id, "title": "t", "result": "PASS", "detail": ""} for check_id in E.COMPUTED_IDS]
        config = build().config
        checks[E.COMPUTED_IDS.index("FLOORS")]["result"] = "INFO"
        checks[E.COMPUTED_IDS.index("REGISTRATION-LIVE")]["result"] = "INFO"
        self.assertEqual(E.verdict_for(checks, config)["verdict"], "WAVE0_PASS")
        checks[E.COMPUTED_IDS.index("SAMPLES-TOTAL")]["result"] = "INFO"
        self.assertEqual(E.verdict_for(checks, config)["verdict"], "WAVE0_FAIL")

    def test_a_missing_computed_check_blocks_a_pass(self):
        checks = [{"id": check_id, "title": "t", "result": "PASS", "detail": ""} for check_id in E.COMPUTED_IDS[1:]]
        self.assertEqual(E.verdict_for(checks, build().config)["verdict"], "WAVE0_FAIL")

    def test_missing_config_is_judged_as_full_and_fails(self):
        checks = [{"id": check_id, "title": "t", "result": "PASS", "detail": ""} for check_id in E.COMPUTED_IDS]
        verdict = E.verdict_for(checks, None)
        self.assertEqual(verdict["profile"], "full")
        self.assertIn(verdict["verdict"], ("WAVE0_PASS", "WAVE0_FAIL"))
        self.assertNotIn("SMOKE", verdict["verdict"])

    def test_smoke_can_never_yield_wave0(self):
        for profile in ("smoke",):
            config = build(profile=profile).config
            checks = [{"id": check_id, "title": "t", "result": "PASS", "detail": ""} for check_id in E.COMPUTED_IDS]
            self.assertEqual(E.verdict_for(checks, config)["verdict"], "SMOKE_PASS")
            checks[0]["result"] = "FAIL"
            self.assertEqual(E.verdict_for(checks, config)["verdict"], "SMOKE_FAIL")


class Cli(Base):
    def write_dir(self, ds, path):
        for name, text in ds.texts().items():
            if text is not None:
                with open(os.path.join(path, name), "w", encoding="utf-8") as handle:
                    handle.write(text)

    def run_cli(self, *argv):
        buffer = io.StringIO()
        with contextlib.redirect_stdout(buffer):
            code = E.main(list(argv))
        return code, buffer.getvalue()

    def test_cli_pass_writes_the_three_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            evidence = os.path.join(tmp, "evidence")
            os.makedirs(evidence)
            self.write_dir(build(), evidence)
            code, output = self.run_cli("--evidence", evidence)
            self.assertEqual(code, 0)
            self.assertIn("VERDICT: WAVE0_PASS", output)
            for name in ("checks.json", "verdict.json", "REPORT.md"):
                self.assertTrue(os.path.isfile(os.path.join(evidence, name)), name)
            checks = json.loads(read_file(os.path.join(evidence, "checks.json")))
            verdict = json.loads(read_file(os.path.join(evidence, "verdict.json")))
            report = read_file(os.path.join(evidence, "REPORT.md"))
            self.assertEqual(verdict["schema"], E.SCHEMA_VERDICT)
            self.assertEqual(verdict["verdict"], "WAVE0_PASS")
            for item in checks:
                self.assertIn(item["id"], report)
            for live_id in E.REQUIRED_LIVE_IDS:
                self.assertIn(live_id, report)
            self.assertIn("Verdict: WAVE0_PASS", report)

    def test_cli_smoke_pass_exits_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.write_dir(build(profile="smoke"), tmp)
            code, output = self.run_cli("--evidence", tmp)
            self.assertEqual(code, 0)
            self.assertIn("VERDICT: SMOKE_PASS", output)
            self.assertIn("does NOT satisfy Astra's thresholds", output)

    def test_cli_fail_exits_one_and_names_the_failed_check(self):
        with tempfile.TemporaryDirectory() as tmp:
            ds = build()
            ds.steady()[50]["stake"] = 1
            self.write_dir(ds, tmp)
            out = os.path.join(tmp, "out")
            code, output = self.run_cli("--evidence", tmp, "--out", out)
            self.assertEqual(code, 1)
            self.assertIn("FAIL STAKE-ZERO", output.replace("  ", " "))
            verdict = json.loads(read_file(os.path.join(out, "verdict.json")))
            self.assertEqual(verdict["failed_ids"], ["STAKE-ZERO"])
            self.assertEqual(verdict["verdict"], "WAVE0_FAIL")

    def test_cli_on_a_missing_directory_fails_without_crashing(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "out")
            code, output = self.run_cli("--evidence", os.path.join(tmp, "nope"), "--out", out)
            self.assertEqual(code, 1)
            self.assertTrue(os.path.isfile(os.path.join(out, "verdict.json")))
            self.assertIn("VERDICT: WAVE0_FAIL", output)

    def test_output_is_deterministic(self):
        with tempfile.TemporaryDirectory() as tmp:
            evidence = os.path.join(tmp, "evidence")
            os.makedirs(evidence)
            self.write_dir(build(), evidence)
            first, second = os.path.join(tmp, "a"), os.path.join(tmp, "b")
            self.run_cli("--evidence", evidence, "--out", first)
            self.run_cli("--evidence", evidence, "--out", second)
            for name in ("checks.json", "verdict.json", "REPORT.md"):
                self.assertEqual(read_file(os.path.join(first, name), "rb"), read_file(os.path.join(second, name), "rb"), name)

    def test_evaluator_reads_no_clock(self):
        source = read_file(os.path.join(os.path.dirname(HERE), "evaluate_stage_b.py"))
        for pattern in (r"\btime\.time\b", r"datetime\.(now|utcnow|today)\b", r"\btime\.monotonic\b",
                        r"\bsubprocess\b", r"\bsocket\b", r"\burllib\b", r"\brequests\b", r"\bos\.system\b"):
            self.assertIsNone(re.search(pattern, source), pattern)

    def test_registration_order_matches_the_declared_ids(self):
        self.assertEqual([entry[0] for entry in E.CHECKS], list(E.COMPUTED_IDS))
        self.assertEqual(len(set(E.COMPUTED_IDS)), len(E.COMPUTED_IDS))


if __name__ == "__main__":
    unittest.main(verbosity=1)
