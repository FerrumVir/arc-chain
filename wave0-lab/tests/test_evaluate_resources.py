#!/usr/bin/env python3
"""Tests for the ARC-83 criteria D and E part of wave0-lab/evaluate_stage_b.py.

Run from the repository root:  python3 -B wave0-lab/tests/test_evaluate_resources.py

Covers the resource slopes and bounds (RES-*), downloads, kernel OOM, VM memory, the binding to Wave 0 run
37750760170, the local freshness checks (FRESH-AGE, FRESH-DISTINCT), the public scoreboard check (PUBLIC-FRESHNESS), the
"resources" supplement profile and the --bounds-file option. The dataset builder of test_evaluate_stage_b.py supplies the
classic evidence; this file adds the new sample fields, scoreboard.jsonl, heartbeats.jsonl, kernel-oom.json and
binding.json. The 124 tests of test_evaluate_stage_b.py are not re-run from here and are not changed by this file.
"""

import contextlib
import copy
import email.utils
import io
import json
import math
import os
import re
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
sys.path.insert(0, HERE)

import evaluate_stage_b as E  # noqa: E402
import test_evaluate_stage_b as T  # noqa: E402  (only its dataset builder is used)

MIB = 1024 ** 2
GIB = 1024 ** 3
ANCHOR = T.BASE + 1.0     # the guest node heartbeats at ANCHOR + 15 s * n
INSTALLER = "66" * 32
IMAGE = "88" * 32
GOOD = dict(
    rss_mib=300.0, rss_slope=0.0,            # MiB, MiB per hour
    avail_mib=2300.0, avail_slope=0.0,
    data_b=50 * MIB, data_slope=50_000.0,    # bytes, bytes per hour
    log_b=2 * MIB, log_slope=10_000.0,
    cache_b=33_833_184, cache_files=2, largest=29_210_392, downloads=2,
    disk_total=20 * GIB, disk_free=12 * GIB, disk_slope=-300_000.0,
)
BOUNDS = {"rss_slope_mib_per_h_max": 100, "mem_available_min_mib": 512, "projection_fraction": 0.5,
          "projection_hours": 24, "disk_reserve_floor_b": 2147483648, "disk_reserve_fraction": 0.20,
          "disk_runway_h_min": 72, "min_points": 121}
SOURCE_BIND = ("last_registration_unix_ms is set by crates/arc-node/src/community_worker.rs:156-164 "
               "record_registration_round, which stores unix_ms_now() ONLY if accepted > 0 (line 160); it is called at "
               "crates/arc-node/src/main.rs:9181 once per scheduled round, and the next round is scheduled 15 s after "
               "the previous round completes (main.rs:9181-9183), so the period is 15 s plus round latency "
               "(COMMUNITY_PRESENCE_INTERVAL = 15 s, main.rs:5595; heartbeat every round, registration every 4th tick, "
               "main.rs:9103); accepted counts coordinators whose signed POST returned 2xx; source commit "
               "cd2344138b32a46eea9192cf0fa7344db6481420.")
TTL_BIND = ("rpc.rs:784 COMMUNITY_WORKER_TTL_SECS = 90; workers_scoreboard (rpc.rs:8470) serves a row only if now - "
            "last heartbeat <= TTL (rpc.rs:8514), so row present means seen <= 90 s at that read")
SUPPLEMENT_SENTENCE = ("SUPPLEMENT: combined evidence with Wave 0 run 37750760170 (four-hour forced battery and real reboot "
                       "there; resources, public freshness and kernel OOM here, on a 4096 MiB VM). Not a "
                       "re-instrumentation of that run. A behaviour-changing fix or a new failure requires reassessment.")
RESOURCE_IDS = ("RESOURCES-PRESENT", "RES-RSS", "RES-MEMAVAIL", "RES-PROJECTION", "RES-DISK", "RES-SWAP", "RES-DATA",
                "RES-HANDLES", "RES-CGROUP", "RES-DOWNLOADS", "RES-OOM", "VM-MEMORY", "BINDING-PRIOR-RUN")
FRESH_IDS = ("FRESH-AGE", "FRESH-DISTINCT", "PUBLIC-FRESHNESS")


# ------------------------------------------------------------------------------------------------------------------
# synthetic evidence for the new contract
# ------------------------------------------------------------------------------------------------------------------

def heartbeat_ms(moment):
    """The last_registration_unix_ms a node that heartbeats every 15 s shows at `moment`."""
    n = math.floor((moment - ANCHOR) / 15.0)
    return int(round((ANCHOR + 15.0 * n) * 1000))


def resource_fields(index, hours, p):
    wiggle = ((index * 7) % 11) - 5        # deterministic, zero-mean noise
    rss_mib = p["rss_mib"] + p["rss_slope"] * hours + 0.25 * wiggle
    avail_mib = p["avail_mib"] + p["avail_slope"] * hours + 0.5 * wiggle
    return {
        "node_rss_kb": int(rss_mib * 1024), "node_hwm_kb": int((rss_mib + 20) * 1024), "node_swap_kb": 0,
        "node_threads": 41, "node_fds": 63, "node_cpu_s": round(12.0 + hours * 90.0, 3),
        "mem_total_kb": 4_000_000, "mem_available_kb": int(avail_mib * 1024), "swap_total_kb": 0, "swap_free_kb": 0,
        "cg_mem_current_b": int((rss_mib + 30) * MIB), "cg_mem_peak_b": int((rss_mib + 60) * MIB), "cg_swap_current_b": 0,
        "disk_total_b": p["disk_total"], "disk_free_b": int(p["disk_free"] + p["disk_slope"] * hours),
        "log_bytes": int(p["log_b"] + p["log_slope"] * hours), "bridge_downloads": p["downloads"],
        "arc_dir_bytes": 120 * MIB, "largest_file_bytes": p["largest"], "legacy_data_bytes": 9 * MIB,
        "node_data_bytes": int(p["data_b"] + p["data_slope"] * hours),
        "release_cache_bytes": p["cache_b"], "release_cache_files": p["cache_files"], "models_bytes": 0,
        "partial_files": 0,
    }


def add_resources(samples, p):
    bridged = [s for s in samples if s.get("node_exe") == T.NODE_EXE]
    origin = bridged[0]["epoch"]
    for index, sample in enumerate(bridged):
        sample.update(resource_fields(index, (sample["epoch"] - origin) / 3600.0, p))
        if sample["health_ok"] and sample.get("coordinators_registered"):
            stamp = heartbeat_ms(sample["epoch"])
            sample["last_registration_unix_ms"] = stamp
            sample["registration_age_s"] = round(sample["epoch"] - stamp / 1000.0, 3)
        else:
            sample["last_registration_unix_ms"] = None
            sample["registration_age_s"] = None


def frange(low, high, step):
    value, out = low, []
    while value <= high:
        out.append(value)
        value += step
    return out


def make_polls(low, high, step=5.0):
    return [{"obs_epoch": round(m, 3), "ts_ms": heartbeat_ms(m), "registered": 3, "total": 6}
            for m in frange(low, high, step)]


def make_probes(low, high, step=31.0, found_origins=(0, 1, 2, 3, 4), origins=6):
    probes = []
    for number, moment in enumerate(frange(low, high, step), 1):
        results = []
        for origin in range(origins):
            found = origin in found_origins
            results.append({
                "origin": origin, "origin_sha8": "%08x" % (0xabc00000 + origin), "http": 200, "error": None,
                "found": found, "name": ("node-" + T.A64[:8]) if found else None,
                "registered_at": round(moment - 20.0, 3) if found else None,
                "worker_id": ("0x" + T.A64) if found else None,
                "server_date": email.utils.formatdate(moment, usegmt=True), "count_total": 7,
                "row": ({"name": "node-" + T.A64[:8], "worker_id": "0x" + T.A64} if found else None)})
        probes.append({"probe": number, "host_epoch": round(moment, 3), "address": T.A64,
                       "found_count": len(found_origins), "results": results})
    return probes


def make_oom(boots):
    scans = [{"boot": -(boots - 1 - i), "first_entry": "2026-10-08T09:00:00+0000", "last_entry": "2026-10-08T10:00:00+0000",
              "kernel_lines": 600 + i, "count": 0, "lines": []} for i in range(boots)]
    return {"schema": E.SCHEMA_OOM, "scans": scans, "total": 0}


UNIT_DIGESTS = {"arc-node.service": "a1" * 32, "arc-updater.service": "a2" * 32, "arc-updater.timer": "a3" * 32}
BASELINE = "baseline-result: legacy v0.7.7 node stranded, data fingerprint ok"


def make_binding():
    """The shape the host writes: prior_run is the committed claim, this_run is MEASURED in this run."""
    measured = {"launcher_sha256": T.LAUNCHER, "node_sha256": T.NODE, "legacy_node_sha256": T.V077,
                "installer_sha256": INSTALLER, "image_sha256": IMAGE, "baseline_result": BASELINE,
                "units": dict(UNIT_DIGESTS)}
    this = copy.deepcopy(dict(measured, run_id=None, run_attempt=1, commit="c" * 40, vm_memory_mb=4096, tag="v0.7.12",
                              launcher_source="published", legacy_tag_commit="d" * 40))
    prior = copy.deepcopy(dict(measured, run_id=37750760170, head_sha="e" * 40, verdict="WAVE0_PASS",
                               evidence_artifact_id=1234, evidence_artifact_digest="sha256:" + "99" * 32,
                               tag="v0.7.12", vm_memory_mb=6144, sources={}))
    return {"schema": E.SCHEMA_BINDING, "prior_run": prior, "this_run": this,
            "equal": dict((key, True) for key in measured), "all_equal": True,
            "disclosed_changes": {"vm_memory_mb": {"prior": 6144, "this": 4096}}}


def to_texts(ds):
    texts = ds.texts()

    def rows(items):
        return "".join(json.dumps(item, sort_keys=True) + "\n" for item in items)

    if getattr(ds, "scoreboard", None) is not None:
        texts["scoreboard.jsonl"] = rows(ds.scoreboard)
    if getattr(ds, "heartbeats", None) is not None:
        texts["heartbeats.jsonl"] = rows(ds.heartbeats)
    if getattr(ds, "oom", None) is not None:
        texts["kernel-oom.json"] = json.dumps(ds.oom, indent=2)
    if getattr(ds, "binding", None) is not None:
        texts["binding.json"] = json.dumps(ds.binding, indent=2)
    return texts


def build_full(live_network="allowed", **res):
    """A perfect full-profile (battery and reboot) dataset that follows the new resource and freshness contract."""
    ds = T.build("full", live_network=live_network)
    p = dict(GOOD)
    p.update(res)
    add_resources(ds.samples, p)
    ds.events.append(T.make_event(17, "vm_memory", T.BASE - 850, False, {"configured_mb": 4096, "mem_total_kb": 4_000_000,
                                                                       "swap_total_kb": 0}, guest=False))
    ds.events.append(T.make_event(18, "kernel_oom_scan", ds.E + 10, False, {"total": 0, "boots": [-1, 0]}, guest=False))
    ds.config.update({"resources_required": True, "scoreboard_required": live_network == "allowed",
                      "scoreboard_interval_s": 30, "battery": True, "settle_s": 0, "vm_memory_mb": 4096,
                      "resource_bounds": dict(BOUNDS)})
    ds.heartbeats = make_polls(ds.rec - 60, ds.E + 10)
    ds.scoreboard = make_probes(ds.rec - 200, ds.E + 30)
    ds.oom = make_oom(2)
    ds.binding = None
    ds.p = p
    return ds


def build_supp(**res):
    """A perfect "resources" supplement dataset: no battery, no reboot, 300 s settle, 8820 s from t0."""
    base = T.build("full")
    p = dict(GOOD)
    p.update(res)
    ds = T.Dataset()
    interval = 60.0
    ds.I = interval
    ds.fb = T.BASE + 6 * interval
    ds.t0 = ds.fb + 3.0
    ds.begin = ds.t0 + 300.0
    ds.E = ds.t0 + 8820.0
    samples = [T.v07_sample(T.BASE + k * interval, T.BOOT1, T.BOOT1_START) for k in range(-10, 6)]
    pstart = int(ds.fb - 4)
    moment = ds.fb
    while moment <= ds.E + E.EPS:
        samples.append(T.bridged_sample(moment, T.BOOT1, T.BOOT1_START, 2222, pstart))
        moment += interval
    for number, sample in enumerate(samples, 1):
        sample["seq"] = number
    add_resources(samples, p)
    ds.samples = samples
    ds.events = [
        T.make_event(1, "setup", T.BASE - 900, False),
        T.make_event(2, "vm_memory", T.BASE - 850, False, {"configured_mb": 4096, "mem_total_kb": 4_000_000,
                                                          "swap_total_kb": 0}, guest=False),
        T.make_event(3, "apply2", T.BASE + 5 * interval, True),
        T.make_event(4, "t0_bridged_healthy", ds.t0, False, {"address": T.A64, "legacy_fingerprint": T.LEGACY_FP}),
        T.make_event(5, "heartbeat", ds.begin + 300, False),
        T.make_event(6, "steady_begin", ds.begin, False, {"last_forced_event": "apply2"}),
        T.make_event(7, "steady_end", ds.E, False),
        T.make_event(8, "kernel_oom_scan", ds.E + 10, False, {"total": 0, "boots": [0]}, guest=False),
        T.make_event(9, "final_stop_begin", ds.E + 20, True, guest=False),
    ]
    ds.commands = [
        {"host_epoch": T.BASE - 800, "cmd": "sudo apt-get install -y jq", "rc": 0},
        {"host_epoch": T.BASE + 5 * interval + 1, "cmd": "bash /opt/arc-w0/canary-consume.sh --tag v0.7.12 --apply", "rc": 0},
        {"host_epoch": ds.begin + 600, "cmd": "cat /var/lib/arc-w0/samples.jsonl", "rc": 0},
        {"host_epoch": ds.E + 25, "cmd": "sudo systemctl stop arc-node", "rc": 0},
        {"host_epoch": ds.E + 40, "cmd": "~/.arc/bin/arc-node --legacy-bridge-rollback", "rc": 0},
    ]
    ds.before = copy.deepcopy(base.before)
    ds.after = copy.deepcopy(base.after)
    ds.after["boot_id"] = T.BOOT1
    ds.after["node"]["main_pid"] = 2222
    ds.live = [{"id": live_id, "title": "live check " + live_id, "result": "PASS", "detail": "ok"}
               for live_id in E.SUPPLEMENT_LIVE_IDS]
    ds.live.append({"id": "L04-interrupt-resumes", "title": "not applicable in the resources supplement", "result": "PASS",
                    "detail": "the consume left a verified cache"})
    ds.live.append({"id": "L99-note", "title": "an informational note", "result": "INFO", "detail": "kept verbatim"})
    bounds = dict(BOUNDS, min_points=136)
    ds.config = {
        "schema": E.SCHEMA_CONFIG, "profile": "resources", "launcher_source": "published", "tag": "v0.7.12",
        "expected_launcher_sha256": T.LAUNCHER, "node_tag": "v0.8.11", "node_sha256": T.NODE, "live_network": "allowed",
        "sample_interval_s": 60, "min_total_s": 8700, "min_steady_s": 8100, "min_steady_samples": 136,
        "post_reboot_healthy_s": 600, "max_gap_factor": 1.0834, "reboot_recovery_deadline_s": 300, "forced_grace_s": 90,
        "updater_runs": 2, "kickstarts": 3, "resources_required": True, "scoreboard_required": True,
        "scoreboard_interval_s": 30, "battery": False, "settle_s": 300, "vm_memory_mb": 4096, "resource_bounds": bounds}
    ds.heartbeats = make_polls(ds.t0 - 30, ds.E + 10)
    ds.scoreboard = make_probes(ds.begin - 100, ds.E + 30)
    ds.oom = make_oom(1)
    ds.binding = make_binding()
    ds.p = p
    return ds


def steady_of(ds):
    """Bridged samples inside the steady window of the dataset (supplement: from steady_begin; full: from recovery)."""
    start = ds.begin if hasattr(ds, "begin") else ds.rec
    return [s for s in ds.samples if s.get("node_exe") == T.NODE_EXE and start - E.EPS <= s["epoch"] <= ds.E + E.EPS]


def judge(ds, bounds=None):
    ev = E.evidence_from_texts(to_texts(ds))
    checks, windows, _cfg = E.analyze(ev, bounds)
    return checks, E.verdict_for(checks, ev["config"], windows)


def table(checks):
    out = {}
    for item in checks:
        out.setdefault(item["id"], item)
    return out


def fails(checks):
    return sorted(c["id"] for c in checks if c["result"] == "FAIL")


class Base(unittest.TestCase):
    maxDiff = None

    def mutated(self, builder, mutate, **kwargs):
        ds = builder(**kwargs)
        mutate(ds)
        return ds

    def assertFailsOnly(self, ds, wanted, bounds=None):
        """The mutation FAILs exactly the `wanted` ids and the verdict is a FAIL."""
        checks, verdict = judge(ds, bounds)
        self.assertEqual(fails(checks), sorted(wanted), [(c["id"], c["detail"][:200]) for c in checks if c["result"] == "FAIL"])
        self.assertTrue(verdict["verdict"].endswith("_FAIL"), verdict["verdict"])
        return checks, verdict


def bridged_of(ds):
    return [s for s in ds.samples if s.get("node_exe") == T.NODE_EXE]


def set_linear(ds, field, base, per_hour, scale=1.0, steady_only=False):
    """field = (base + per_hour * hours since the first bridged sample) * scale, exactly linear (no noise)."""
    samples = steady_of(ds) if steady_only else bridged_of(ds)
    origin = bridged_of(ds)[0]["epoch"]
    for sample in samples:
        sample[field] = int((base + per_hour * (sample["epoch"] - origin) / 3600.0) * scale)


def drop_polls(ds, low, high):
    ds.heartbeats = [p for p in ds.heartbeats if not (low <= p["obs_epoch"] <= high)]


# ------------------------------------------------------------------------------------------------------------------
# registry and contract
# ------------------------------------------------------------------------------------------------------------------

class TestContract(Base):
    def test_classic_registry_is_untouched(self):
        self.assertEqual([c[0] for c in E.CHECKS], list(E.COMPUTED_IDS))
        self.assertEqual(len(E.COMPUTED_IDS), 25)

    def test_extra_registry_matches_the_declared_ids(self):
        self.assertEqual([c[0] for c in E.EXTRA_CHECKS], list(E.EXTRA_IDS))
        self.assertEqual(len(E.EXTRA_IDS), 16)
        self.assertFalse(set(E.EXTRA_IDS) & set(E.COMPUTED_IDS))
        self.assertEqual(set(E.EXTRA_IDS), set(RESOURCE_IDS) | set(FRESH_IDS))
        self.assertNotIn("FRESH-LOCAL", E.EXTRA_IDS)
        for check_id in RESOURCE_IDS:
            self.assertEqual(E.EXTRA_GROUP[check_id], "resources", check_id)
        for check_id in FRESH_IDS:
            self.assertEqual(E.EXTRA_GROUP[check_id], "freshness", check_id)

    def test_adopted_bounds_are_exactly_the_adopted_numbers(self):
        self.assertEqual(E.ADOPTED_BOUNDS, {
            "rss_slope_mib_per_h_max": 100.0, "mem_available_min_mib": 512.0, "projection_fraction": 0.5,
            "projection_hours": 24.0, "disk_reserve_floor_b": 2147483648.0, "disk_reserve_fraction": 0.20,
            "disk_runway_h_min": 72.0, "min_points": 121})
        self.assertEqual(set(E.BOUND_KEYS), set(E.ADOPTED_BOUNDS))
        for invented in ("provisional", "rss_max_mib", "mem_available_slope", "disk_free_min", "data_slope", "log_slope",
                         "fd_slope", "thread_slope", "cg_peak_max"):
            self.assertNotIn(invented, E.BOUND_KEYS)

    def test_info_series_ids_never_block(self):
        for check_id in ("RES-SWAP", "RES-DATA", "RES-HANDLES", "RES-CGROUP"):
            self.assertIn(check_id, E.INFO_OK_IDS)
        for check_id in ("RES-RSS", "RES-MEMAVAIL", "RES-PROJECTION", "RES-DISK", "RES-DOWNLOADS", "RES-OOM", "VM-MEMORY",
                         "RESOURCES-PRESENT", "BINDING-PRIOR-RUN", "FRESH-AGE", "FRESH-DISTINCT", "PUBLIC-FRESHNESS"):
            self.assertNotIn(check_id, E.INFO_OK_IDS)

    def test_the_binding_sentences_are_verbatim(self):
        self.assertEqual(E.SOURCE_BIND, SOURCE_BIND)
        self.assertEqual(E.TTL_BIND, TTL_BIND)
        self.assertEqual(E.SUPPLEMENT_STATEMENT, SUPPLEMENT_SENTENCE)

    def test_supplement_floors(self):
        self.assertEqual(E.RESOURCE_FLOOR_MIN, {"min_total_s": 8700, "min_steady_s": 8100, "min_steady_samples": 136})
        self.assertEqual(E.RESOURCE_GAP_MAX_S, 65.0)
        self.assertEqual(E.SETTLE_FLOOR_S, 300)

    def test_new_optional_files(self):
        self.assertEqual(dict(E.OPTIONAL_FILES), {"scoreboard": "scoreboard.jsonl", "heartbeats": "heartbeats.jsonl",
                                                   "oom": "kernel-oom.json", "binding": "binding.json"})
        self.assertEqual([name for _key, name in E.FILES], ["config-effective.json", "samples.jsonl", "events.jsonl",
                                                            "commands.jsonl", "invariants-before.json",
                                                            "invariants-after.json", "checks-live.jsonl"])


# ------------------------------------------------------------------------------------------------------------------
# perfect datasets
# ------------------------------------------------------------------------------------------------------------------

class TestPerfectDatasets(Base):
    def test_perfect_full_run_with_the_new_contract_passes(self):
        checks, verdict = judge(build_full())
        self.assertEqual(fails(checks), [])
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")
        self.assertEqual(verdict["failed_ids"], [])
        self.assertEqual(verdict["unmet_ids"], [])
        ids = [c["id"] for c in checks]
        self.assertEqual(ids[:25], list(E.COMPUTED_IDS))
        self.assertEqual(ids[25:41], list(E.EXTRA_IDS))
        self.assertEqual(ids[41:], list(E.REQUIRED_LIVE_IDS) + ["L99-note"])
        self.assertEqual(verdict["counts"], {"PASS": 48, "FAIL": 0, "INFO": 5, "SKIP": 1})
        rows = table(checks)
        self.assertEqual(rows["BINDING-PRIOR-RUN"]["result"], "SKIP")
        self.assertIn("profile resources only", rows["BINDING-PRIOR-RUN"]["detail"])
        for check_id in ("RES-SWAP", "RES-DATA", "RES-HANDLES", "RES-CGROUP"):
            self.assertEqual(rows[check_id]["result"], "INFO", check_id)
        self.assertEqual(verdict["not_applicable"], {"BINDING-PRIOR-RUN": "applies to profile resources only"})
        self.assertTrue(verdict["is_post_g0_wave0"])
        self.assertEqual(verdict["resource_bounds"]["source"], "config")
        self.assertEqual(verdict["resource_bounds"]["differs_from_adopted"], [])

    def test_perfect_supplement_passes_and_is_never_wave0(self):
        checks, verdict = judge(build_supp())
        self.assertEqual(fails(checks), [])
        self.assertEqual(verdict["verdict"], "SUPPLEMENT_PASS")
        self.assertNotIn("WAVE0", verdict["verdict"])
        self.assertFalse(verdict["is_post_g0_wave0"])
        self.assertEqual(verdict["profile"], "resources")
        ids = [c["id"] for c in checks]
        self.assertEqual(ids[:25], list(E.COMPUTED_IDS))
        self.assertEqual(ids[25:41], list(E.EXTRA_IDS))
        rows = table(checks)
        skipped = sorted(c["id"] for c in checks if c["result"] == "SKIP")
        battery_live_not_recorded = [i for i in E.BATTERY_LIVE_IDS if i != "L04-interrupt-resumes"]
        self.assertEqual(skipped, sorted(list(E.BATTERY_ONLY_IDS) + battery_live_not_recorded))
        for check_id in E.BATTERY_ONLY_IDS:
            self.assertIn("covered by Wave 0 run 37750760170", rows[check_id]["detail"])
        self.assertEqual(rows["L04-interrupt-resumes"]["result"], "PASS")     # the supplement's own L04 is judged verbatim
        self.assertEqual(verdict["counts"], {"PASS": 36, "FAIL": 0, "INFO": 5, "SKIP": 13})
        self.assertTrue(verdict["statement"].startswith(SUPPLEMENT_SENTENCE))
        self.assertIn("Supplement criteria met on the published tag v0.7.12", verdict["statement"])
        self.assertIn("this is not a Wave 0 pass", verdict["statement"])
        self.assertNotIn("Wave 0 criteria met", verdict["statement"])
        self.assertEqual(verdict["not_applicable"], {})
        self.assertEqual(verdict["resource_bounds"]["differs_from_adopted"], ["min_points"])    # 136 > 121: stricter

    def test_supplement_with_an_artifact_launcher_says_so(self):
        ds = build_supp()
        ds.config["launcher_source"] = "artifact"
        _checks, verdict = judge(ds)
        self.assertEqual(verdict["verdict"], "SUPPLEMENT_PASS")
        self.assertIn("rehearsal", verdict["statement"])
        self.assertIn("launcher was not consumed from the published tag", verdict["statement"])

    def test_blocked_network_skips_the_freshness_checks_only(self):
        ds = build_full(live_network="blocked")
        ds.scoreboard = None
        ds.heartbeats = None
        checks, verdict = judge(ds)
        rows = table(checks)
        self.assertEqual(fails(checks), [])
        for check_id in FRESH_IDS:
            self.assertEqual(rows[check_id]["result"], "SKIP", check_id)
            self.assertIn("scoreboard_required is false", rows[check_id]["detail"])
        self.assertEqual(rows["RES-RSS"]["result"], "PASS")
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")
        self.assertIn("NOT JUDGED: local and public freshness checks", verdict["statement"])

    def test_output_is_deterministic(self):
        first = judge(build_supp())
        second = judge(build_supp())
        self.assertEqual(E.dump_json(first[0]), E.dump_json(second[0]))
        self.assertEqual(E.dump_json(first[1]), E.dump_json(second[1]))
        self.assertEqual(E.render_report(*first), E.render_report(*second))

    def test_the_report_lists_bounds_and_not_judged(self):
        checks, verdict = judge(build_supp())
        report = E.render_report(checks, verdict)
        self.assertIn("## Resource bounds", report)
        self.assertIn("rss_slope_mib_per_h_max: 100.0 (adopted 100.0)", report)
        old = T.build()
        old_checks, old_verdict, _w = T.evaluate_ds(old)
        old_report = E.render_report(old_checks, old_verdict)
        self.assertIn("## Not judged", old_report)
        self.assertNotIn("## Resource bounds", old_report)


# ------------------------------------------------------------------------------------------------------------------
# evidence from before the contract, and explicit switches
# ------------------------------------------------------------------------------------------------------------------

class TestOlderEvidence(Base):
    def test_old_evidence_is_judged_exactly_as_before_and_says_what_it_does_not_judge(self):
        ds = T.build()
        checks, verdict, _windows = T.evaluate_ds(ds)
        ids = [c["id"] for c in checks]
        self.assertEqual(ids, list(E.COMPUTED_IDS) + list(E.REQUIRED_LIVE_IDS) + ["L99-note"])
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")
        self.assertEqual(verdict["counts"], {"PASS": 37, "FAIL": 0, "INFO": 1, "SKIP": 0})
        self.assertEqual(sorted(verdict["not_applicable"]), sorted(E.EXTRA_IDS))
        self.assertIsNone(verdict["resource_bounds"])
        self.assertIn("NOT JUDGED", verdict["statement"])
        self.assertIn("resource, download, OOM and VM-memory checks", verdict["statement"])
        self.assertIn("no resources_required key", verdict["statement"])
        self.assertIn("no scoreboard_required key", verdict["statement"])

    def test_explicit_false_gives_skip_rows_with_the_reason(self):
        ds = T.build()
        ds.config["resources_required"] = False
        ds.config["scoreboard_required"] = False
        checks, verdict = judge(ds)
        skipped = [c for c in checks if c["result"] == "SKIP"]
        self.assertEqual(sorted(c["id"] for c in skipped), sorted(E.EXTRA_IDS))
        self.assertTrue(all("required is false" in c["detail"] for c in skipped))
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")
        self.assertEqual(verdict["counts"]["SKIP"], 16)
        self.assertIn("resources_required=false", verdict["statement"])
        self.assertIn("scoreboard_required=false", verdict["statement"])

    def test_resources_true_alone_judges_resources_and_omits_freshness(self):
        ds = build_full()
        del ds.config["scoreboard_required"]
        ds.scoreboard = None
        ds.heartbeats = None
        checks, verdict = judge(ds)
        ids = [c["id"] for c in checks]
        self.assertIn("RES-RSS", ids)
        for check_id in FRESH_IDS:
            self.assertNotIn(check_id, ids)
        self.assertEqual(sorted(i for i in verdict["not_applicable"] if i in FRESH_IDS), sorted(FRESH_IDS))
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")

    def test_required_means_fail_closed_when_the_data_is_missing(self):
        ds = T.build()
        ds.config["resources_required"] = True
        ds.config["scoreboard_required"] = True
        checks, verdict = judge(ds)
        rows = table(checks)
        self.assertEqual(verdict["verdict"], "WAVE0_FAIL")
        for check_id in ("RESOURCES-PRESENT", "RES-RSS", "RES-MEMAVAIL", "RES-PROJECTION", "RES-DISK", "RES-DOWNLOADS",
                         "RES-OOM", "VM-MEMORY", "FRESH-AGE", "FRESH-DISTINCT", "PUBLIC-FRESHNESS"):
            self.assertEqual(rows[check_id]["result"], "FAIL", check_id)
        for check_id in ("RES-OOM", "FRESH-DISTINCT", "PUBLIC-FRESHNESS"):
            self.assertIn("UNPROVED", rows[check_id]["detail"], check_id)
        self.assertEqual(rows["BINDING-PRIOR-RUN"]["result"], "SKIP")

    def test_switch_values_must_be_booleans(self):
        ds = build_full()
        ds.config["resources_required"] = "yes"
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["FLOORS"]["result"], "FAIL")
        self.assertIn("resources_required must be a boolean", table(checks)["FLOORS"]["detail"])

    def test_context_failure_still_reports_the_extras(self):
        original = E.Ctx

        class Broken(object):
            def __init__(self, ev, bounds_override=None):
                raise RuntimeError("boom")

        E.Ctx = Broken
        try:
            ds = build_supp()
            checks, _windows, _cfg = E.analyze(E.evidence_from_texts(to_texts(ds)))
        finally:
            E.Ctx = original
        computed = [c for c in checks if c["id"] in E.COMPUTED_IDS or c["id"] in E.EXTRA_IDS]
        self.assertEqual(len(computed), 25 + 16)
        self.assertTrue(all(c["result"] == "FAIL" and "internal error" in c["detail"] for c in computed))


# ------------------------------------------------------------------------------------------------------------------
# resource checks: one mutation per rule
# ------------------------------------------------------------------------------------------------------------------

class TestResourceChecks(Base):
    def test_resources_present_needs_99_percent_of_every_field(self):
        contract = ("node_rss_kb", "mem_available_kb", "swap_total_kb", "swap_free_kb", "node_data_bytes",
                    "release_cache_bytes", "log_bytes", "disk_free_b", "disk_total_b")
        self.assertEqual(E.RES_PRESENT_FIELDS, contract)
        for field in contract:
            ds = build_supp()
            steady = steady_of(ds)
            for sample in steady[::10]:               # 10 % missing
                sample[field] = None
            checks, _verdict = judge(ds)
            self.assertEqual(table(checks)["RESOURCES-PRESENT"]["result"], "FAIL", field)
            self.assertIn("UNPROVED: " + field, table(checks)["RESOURCES-PRESENT"]["detail"])

    def test_resources_present_tolerates_one_missing_sample_in_a_hundred(self):
        ds = build_supp()
        steady = steady_of(ds)
        steady[7]["node_rss_kb"] = None              # 1 of 142 = 0.7 %
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["RESOURCES-PRESENT"]["result"], "PASS")
        self.assertEqual(table(checks)["RES-RSS"]["result"], "PASS")

    def test_rss_slope_above_100_mib_per_hour_fails(self):
        ds = build_supp(avail_mib=100000.0)           # a huge headroom: only the slope rule can fire
        set_linear(ds, "node_rss_kb", 300.0, 100.5, 1024.0)
        self.assertFailsOnly(ds, ["RES-RSS"])

    def test_rss_slope_below_the_bound_passes(self):
        ds = build_supp(avail_mib=100000.0)
        set_linear(ds, "node_rss_kb", 300.0, 99.5, 1024.0)
        checks, _verdict = judge(ds)
        self.assertEqual(fails(checks), [])
        self.assertIn("RSS slope +99.5", table(checks)["RES-RSS"]["detail"])

    def test_a_shrinking_rss_never_fails(self):
        ds = build_supp()
        set_linear(ds, "node_rss_kb", 3000.0, -400.0, 1024.0)
        checks, _verdict = judge(ds)
        self.assertEqual(fails(checks), [])
        self.assertIn("RSS slope -400.0", table(checks)["RES-RSS"]["detail"])

    def test_rss_slope_needs_min_points(self):
        ds = build_supp()
        ds.config["resource_bounds"]["min_points"] = 150      # stricter than the 142 steady samples: allowed
        checks, _verdict = judge(ds)
        rows = table(checks)
        for check_id in ("RES-RSS", "RES-PROJECTION", "RES-DISK", "RES-DOWNLOADS"):
            self.assertEqual(rows[check_id]["result"], "FAIL", check_id)
            self.assertIn("needs >= 150", rows[check_id]["detail"], check_id)
            self.assertIn("UNPROVED", rows[check_id]["detail"], check_id)

    def test_mem_available_minimum(self):
        ds = build_supp()
        steady_of(ds)[40]["mem_available_kb"] = 511 * 1024
        self.assertFailsOnly(ds, ["RES-MEMAVAIL"])
        ds = build_supp()
        steady_of(ds)[40]["mem_available_kb"] = 512 * 1024    # exactly the minimum passes
        checks, _verdict = judge(ds)
        self.assertEqual(fails(checks), [])
        ds = build_supp()
        steady_of(ds)[40]["mem_available_kb"] = 511 * 1024 + 1000    # 511.98 MiB is under 512
        self.assertFailsOnly(ds, ["RES-MEMAVAIL"])

    def test_mem_available_outside_the_steady_window_is_not_judged(self):
        ds = build_supp()
        bridged_of(ds)[0]["mem_available_kb"] = 100 * 1024     # before steady_begin
        checks, _verdict = judge(ds)
        self.assertEqual(fails(checks), [])

    def test_projection_uses_the_conservative_headroom(self):
        # start 1500 MiB available: headroom 1500 - 512 = 988, half is 494. 20 MiB/h x 24 h = 480 passes, 21 -> 504 fails.
        ds = build_supp(avail_mib=1500.0)
        set_linear(ds, "node_rss_kb", 300.0, 20.0, 1024.0)
        set_linear(ds, "mem_available_kb", 1500.0, 0.0, 1024.0)
        checks, _verdict = judge(ds)
        self.assertEqual(fails(checks), [])
        self.assertIn("24h RSS growth 480.0 MiB", table(checks)["RES-PROJECTION"]["detail"])
        ds = build_supp(avail_mib=1500.0)
        set_linear(ds, "node_rss_kb", 300.0, 21.0, 1024.0)
        set_linear(ds, "mem_available_kb", 1500.0, 0.0, 1024.0)
        self.assertFailsOnly(ds, ["RES-PROJECTION"])          # RSS slope 21 < 100 and MemAvailable stays at 1500

    def test_the_starting_headroom_is_the_first_steady_memavailable(self):
        # MemAvailable rises from 600 MiB (first steady sample) to 2000 MiB; 10 MiB/h x 24 h = 240 against 0.5 x 88 = 44
        ds = build_supp(avail_mib=600.0)
        set_linear(ds, "node_rss_kb", 300.0, 10.0, 1024.0)
        steady = steady_of(ds)
        for index, sample in enumerate(steady):
            sample["mem_available_kb"] = int((600.0 + 1400.0 * index / (len(steady) - 1)) * 1024)
        checks, _verdict = self.assertFailsOnly(ds, ["RES-PROJECTION"])
        self.assertIn("(first steady MemAvailable 600.0 MiB - minimum 512 MiB)", table(checks)["RES-PROJECTION"]["detail"])

    def test_the_plain_variant_is_only_printed(self):
        # start 600: conservative half-headroom is 44, the plain variant would allow 300. 2 MiB/h -> 48 > 44 fails.
        ds = build_supp(avail_mib=600.0)
        set_linear(ds, "node_rss_kb", 300.0, 2.0, 1024.0)
        set_linear(ds, "mem_available_kb", 600.0, 0.0, 1024.0)
        checks, _verdict = self.assertFailsOnly(ds, ["RES-PROJECTION"])
        detail = table(checks)["RES-PROJECTION"]["detail"]
        self.assertIn("conservative headroom 88.0 MiB = 44.0 MiB", detail)
        self.assertIn("INFO plain variant: 48.0 MiB vs 0.5 x 600.0 MiB = 300.0 MiB", detail)

    def test_projection_growth_only(self):
        ds = build_supp(avail_mib=600.0)
        set_linear(ds, "node_rss_kb", 3000.0, -50.0, 1024.0)
        set_linear(ds, "mem_available_kb", 600.0, 0.0, 1024.0)
        checks, _verdict = judge(ds)
        self.assertEqual(fails(checks), [])
        self.assertIn("24h RSS growth 0.0 MiB (slope -50.0", table(checks)["RES-PROJECTION"]["detail"])

    def test_disk_reserve_is_the_larger_of_2_gib_and_20_percent(self):
        ds = build_supp()                                     # 20 GiB disk: reserve max(2, 4) = 4 GiB
        steady_of(ds)[30]["disk_free_b"] = 3 * GIB            # above 2 GiB, below the 20 % reserve
        checks, _verdict = self.assertFailsOnly(ds, ["RES-DISK"])
        self.assertIn("reserve 4.00 GiB", table(checks)["RES-DISK"]["detail"])
        ds = build_supp()
        steady_of(ds)[30]["disk_free_b"] = 4 * GIB + 1
        self.assertEqual(fails(judge(ds)[0]), [])
        ds = build_supp()
        steady_of(ds)[30]["disk_free_b"] = 4 * GIB            # equal to the reserve is not above it
        self.assertFailsOnly(ds, ["RES-DISK"])
        ds = build_supp(disk_total=5 * GIB, disk_free=3 * GIB)    # small disk: the 2 GiB floor rules (20 % is 1 GiB)
        steady_of(ds)[30]["disk_free_b"] = 2 * GIB
        self.assertFailsOnly(ds, ["RES-DISK"])
        ds = build_supp(disk_total=5 * GIB, disk_free=3 * GIB)
        steady_of(ds)[30]["disk_free_b"] = 2 * GIB + 1
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_disk_reserve_applies_to_every_bridged_sample(self):
        ds = build_supp()
        bridged_of(ds)[1]["disk_free_b"] = GIB                # in the settle period, before the steady window
        self.assertFailsOnly(ds, ["RES-DISK"])

    def test_disk_runway_must_be_72_hours(self):
        # 6 GiB free, reserve 4 GiB: ~2 GiB of room. -40 MiB/h leaves ~49 h, -20 MiB/h ~100 h.
        ds = build_supp(disk_free=6 * GIB)
        set_linear(ds, "disk_free_b", 6 * 1024.0, -40.0, float(MIB))
        checks, _verdict = self.assertFailsOnly(ds, ["RES-DISK"])
        self.assertIn("runway 4", table(checks)["RES-DISK"]["detail"])
        ds = build_supp(disk_free=6 * GIB)
        set_linear(ds, "disk_free_b", 6 * 1024.0, -20.0, float(MIB))
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_a_growing_free_disk_has_no_runway_limit(self):
        ds = build_supp(disk_free=6 * GIB)
        set_linear(ds, "disk_free_b", 6 * 1024.0, 500.0, float(MIB))
        checks, _verdict = judge(ds)
        self.assertEqual(fails(checks), [])
        self.assertIn("unbounded", table(checks)["RES-DISK"]["detail"])

    def test_downloads_structural_rules(self):
        cases = [
            ("the release cache changed", lambda s: s.__setitem__("release_cache_bytes", s["release_cache_bytes"] + 4096)),
            ("a file entered the cache", lambda s: s.__setitem__("release_cache_files", 3)),
            ("a partial file", lambda s: s.__setitem__("partial_files", 1)),
            ("a model file", lambda s: s.__setitem__("models_bytes", 4 * GIB)),
            ("a repeated download", lambda s: s.__setitem__("bridge_downloads", 3)),
            ("a model-sized file", lambda s: s.__setitem__("largest_file_bytes", 100 * MIB)),
        ]
        for label, mutate in cases:
            ds = build_supp()
            mutate(steady_of(ds)[60])
            checks, _verdict = self.assertFailsOnly(ds, ["RES-DOWNLOADS"])
            self.assertIn("violation", table(checks)["RES-DOWNLOADS"]["detail"], label)

    def test_downloads_just_under_the_model_size_pass(self):
        ds = build_supp()
        steady_of(ds)[60]["largest_file_bytes"] = 100 * MIB - 1
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_a_log_rotation_is_not_a_download_but_a_rise_after_it_is(self):
        ds = build_supp()
        for sample in steady_of(ds)[50:]:
            sample["bridge_downloads"] = 0                    # the log was rotated
        self.assertEqual(fails(judge(ds)[0]), [])
        ds = build_supp()
        for sample in steady_of(ds)[50:]:
            sample["bridge_downloads"] = 0
        for sample in steady_of(ds)[90:]:
            sample["bridge_downloads"] = 1                    # and then one download was logged
        self.assertFailsOnly(ds, ["RES-DOWNLOADS"])

    def test_downloads_before_t0_are_expected(self):
        ds = build_supp()
        first = bridged_of(ds)[0]
        self.assertLess(first["epoch"], ds.t0)
        first["bridge_downloads"] = 0                         # the consume itself downloaded: before t0
        first["release_cache_bytes"] = 0
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_a_missing_structural_field_is_unproved(self):
        ds = build_supp()
        for sample in bridged_of(ds):
            sample["bridge_downloads"] = None
        checks, _verdict = self.assertFailsOnly(ds, ["RES-DOWNLOADS"])
        self.assertIn("UNPROVED: bridge_downloads", table(checks)["RES-DOWNLOADS"]["detail"])

    def test_log_and_node_data_growth_runway(self):
        # 12 GiB free, reserve 4 GiB: 8192 MiB of room. 200 MiB/h of logs leaves ~41 h, 100 MiB/h ~82 h.
        ds = build_supp(log_slope=200.0 * MIB)
        checks, _verdict = self.assertFailsOnly(ds, ["RES-DOWNLOADS"])
        self.assertIn("runway", table(checks)["RES-DOWNLOADS"]["detail"])
        ds = build_supp(log_slope=100.0 * MIB)
        self.assertEqual(fails(judge(ds)[0]), [])
        ds = build_supp(data_slope=200.0 * MIB)
        self.assertFailsOnly(ds, ["RES-DOWNLOADS"])

    def test_info_series_never_fail(self):
        ds = build_supp()
        for index, sample in enumerate(steady_of(ds)):
            sample.update({"swap_total_kb": 8_000_000, "swap_free_kb": 8_000_000 - index * 50_000, "node_swap_kb": index * 40_000,
                           "cg_swap_current_b": index * GIB // 10, "node_fds": 1000 + index * 90, "node_threads": 40 + index * 3,
                           "cg_mem_peak_b": 4 * GIB + index * MIB, "node_cpu_s": 1e6 * (index + 1)})
        checks, verdict = judge(ds)
        rows = table(checks)
        for check_id in ("RES-SWAP", "RES-DATA", "RES-HANDLES", "RES-CGROUP"):
            self.assertEqual(rows[check_id]["result"], "INFO", check_id)
        self.assertEqual(fails(checks), [])
        self.assertEqual(verdict["verdict"], "SUPPLEMENT_PASS")
        self.assertIn("swap in use", rows["RES-SWAP"]["detail"])
        self.assertIn("open fds", rows["RES-HANDLES"]["detail"])
        self.assertIn("cgroup memory peak", rows["RES-CGROUP"]["detail"])

    def test_info_series_survive_missing_data(self):
        ds = build_supp()
        for sample in ds.samples:
            for field in ("node_fds", "node_threads", "cg_mem_peak_b", "cg_mem_current_b", "node_hwm_kb", "node_cpu_s",
                          "swap_total_kb", "swap_free_kb", "node_swap_kb", "cg_swap_current_b"):
                sample.pop(field, None)
        checks, _verdict = judge(ds)
        rows = table(checks)
        for check_id in ("RES-SWAP", "RES-HANDLES", "RES-CGROUP"):
            self.assertIn(rows[check_id]["result"], ("INFO",), check_id)
            self.assertIn("not recorded", rows[check_id]["detail"])

    def test_no_steady_window_means_unproved_not_a_pass(self):
        ds = build_supp()
        ds.events = [e for e in ds.events if e["name"] != "steady_begin"]
        checks, _verdict = judge(ds)
        rows = table(checks)
        for check_id in ("RESOURCES-PRESENT", "RES-RSS", "RES-MEMAVAIL", "RES-PROJECTION", "RES-DISK", "RES-DOWNLOADS",
                         "FRESH-AGE"):
            self.assertEqual(rows[check_id]["result"], "FAIL", check_id)
            self.assertIn("UNPROVED", rows[check_id]["detail"], check_id)

    def test_wrongly_typed_resource_field_is_a_malformed_record(self):
        ds = build_supp()
        steady_of(ds)[5]["node_rss_kb"] = "300000"
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["EVIDENCE-FILES"]["result"], "FAIL")
        self.assertEqual(table(checks)["RES-RSS"]["result"], "FAIL")


# ------------------------------------------------------------------------------------------------------------------
# kernel OOM, VM memory, binding
# ------------------------------------------------------------------------------------------------------------------

class TestOomVmBinding(Base):
    def test_oom_missing_file_is_unproved(self):
        ds = build_supp()
        ds.oom = None
        checks, _verdict = self.assertFailsOnly(ds, ["RES-OOM"])
        self.assertIn("UNPROVED: kernel-oom.json is missing", table(checks)["RES-OOM"]["detail"])

    def test_oom_unreadable_file_fails(self):
        ds = build_supp()
        texts = to_texts(ds)
        texts["kernel-oom.json"] = "{not json"
        ev = E.evidence_from_texts(texts)
        checks, _w, _c = E.analyze(ev)
        self.assertEqual(table(checks)["RES-OOM"]["result"], "FAIL")
        self.assertIn("kernel-oom.json is not valid JSON", table(checks)["RES-OOM"]["detail"])

    def test_one_oom_kill_in_any_boot_fails(self):
        ds = build_full()
        ds.oom["scans"][0]["count"] = 1
        ds.oom["scans"][0]["lines"] = ["Oct  8 09:12:01 kernel: Out of memory: Killed process 1234 (arc-node)"]
        ds.oom["total"] = 1
        checks, _verdict = self.assertFailsOnly(ds, ["RES-OOM"])
        self.assertIn("1 out-of-memory line(s): boot -1", table(checks)["RES-OOM"]["detail"])
        self.assertIn("Out of memory", table(checks)["RES-OOM"]["detail"])

    def test_oom_totals_must_add_up(self):
        ds = build_supp()
        ds.oom["total"] = 0
        ds.oom["scans"][0]["count"] = 2
        self.assertFailsOnly(ds, ["RES-OOM"])
        for wrong in (None, 5, "0"):
            ds = build_supp()
            ds.events = [e for e in ds.events if e["name"] != "kernel_oom_scan"]     # nothing else can notice the total
            ds.oom["total"] = wrong
            checks, _verdict = self.assertFailsOnly(ds, ["RES-OOM"])
            self.assertIn("disagrees with the per-boot counts (0)", table(checks)["RES-OOM"]["detail"])

    def test_an_empty_scan_proves_nothing(self):
        ds = build_supp()
        ds.oom["scans"][0]["kernel_lines"] = 0
        checks, _verdict = self.assertFailsOnly(ds, ["RES-OOM"])
        self.assertIn("UNPROVED", table(checks)["RES-OOM"]["detail"])
        ds = build_supp()
        ds.oom = {"schema": E.SCHEMA_OOM, "scans": [], "total": None, "error": "journalctl: permission denied"}
        checks, _verdict = self.assertFailsOnly(ds, ["RES-OOM"])
        self.assertIn("permission denied", table(checks)["RES-OOM"]["detail"])

    def test_every_boot_of_the_soak_must_be_scanned(self):
        ds = build_full()
        ds.oom = make_oom(1)                                  # the samples show two boots (the real reboot)
        checks, _verdict = self.assertFailsOnly(ds, ["RES-OOM"])
        self.assertIn("1 boot(s) scanned but the samples after t0 show 2", table(checks)["RES-OOM"]["detail"])

    def test_oom_schema_and_event_cross_checks(self):
        ds = build_supp()
        ds.oom["schema"] = "something.else"
        self.assertFailsOnly(ds, ["RES-OOM"])
        ds = build_supp()
        for event in ds.events:
            if event["name"] == "kernel_oom_scan":
                event["detail"]["total"] = 3
        self.assertFailsOnly(ds, ["RES-OOM"])
        ds = build_supp()
        for event in ds.events:
            if event["name"] == "kernel_oom_scan":
                event["seq"] = 3                              # before steady_end (seq 7): the scan did not cover the window
        checks, _verdict = self.assertFailsOnly(ds, ["RES-OOM"])
        self.assertIn("before steady_end", table(checks)["RES-OOM"]["detail"])

    def test_vm_memory_must_be_the_4096_mb_canary(self):
        ds = build_full()
        for event in ds.events:
            if event["name"] == "vm_memory":
                event["detail"]["configured_mb"] = 6144
                event["detail"]["mem_total_kb"] = 6_000_000
        ds.config["vm_memory_mb"] = 6144
        for sample in bridged_of(ds):
            sample["mem_total_kb"] = 6_000_000
        checks, _verdict = self.assertFailsOnly(ds, ["VM-MEMORY"])
        self.assertIn("6144 MB", table(checks)["VM-MEMORY"]["detail"])

    def test_vm_memory_event_is_required_and_unique(self):
        ds = build_full()
        ds.events = [e for e in ds.events if e["name"] != "vm_memory"]
        checks, _verdict = self.assertFailsOnly(ds, ["VM-MEMORY"])
        self.assertIn("UNPROVED", table(checks)["VM-MEMORY"]["detail"])
        ds = build_full()
        ds.events.append(copy.deepcopy([e for e in ds.events if e["name"] == "vm_memory"][0]))
        self.assertFailsOnly(ds, ["VM-MEMORY"])

    def test_the_guest_must_see_a_4096_mb_machine(self):
        ds = build_full()
        for sample in bridged_of(ds):
            sample["mem_total_kb"] = 5_900_000                # the guest sees almost 6 GiB while the event says 4096
        checks, _verdict = self.assertFailsOnly(ds, ["VM-MEMORY"])
        self.assertIn("does not fit a 4096 MB VM", table(checks)["VM-MEMORY"]["detail"])
        ds = build_full()
        for event in ds.events:
            if event["name"] == "vm_memory":
                event["detail"]["mem_total_kb"] = 1_000_000
        self.assertFailsOnly(ds, ["VM-MEMORY"])

    def test_vm_memory_must_agree_with_the_config(self):
        ds = build_full()
        ds.config["vm_memory_mb"] = 8192
        self.assertFailsOnly(ds, ["VM-MEMORY"])

    def test_binding_pass_text_discloses_the_memory_change(self):
        checks, _verdict = judge(build_supp())
        detail = table(checks)["BINDING-PRIOR-RUN"]["detail"]
        self.assertIn("disclosed: VM memory changed 6144 -> 4096 MiB", detail)
        for field in E.BINDING_SHA_FIELDS:
            self.assertIn(field, detail)
        for unit in E.UNIT_FILES:
            self.assertIn("units." + unit, detail)
        self.assertIn("baseline_result equal", detail)
        self.assertIn("sha256:" + "99" * 32, detail)
        self.assertEqual(detail.count("=="), 8)

    def test_binding_file_is_required_for_the_supplement(self):
        ds = build_supp()
        ds.binding = None
        checks, verdict = self.assertFailsOnly(ds, ["BINDING-PRIOR-RUN"])
        self.assertIn("UNPROVED: binding.json is missing", table(checks)["BINDING-PRIOR-RUN"]["detail"])
        self.assertEqual(verdict["verdict"], "SUPPLEMENT_FAIL")

    def test_binding_every_sha_must_match(self):
        for field in E.BINDING_SHA_FIELDS:
            ds = build_supp()
            ds.binding["prior_run"][field] = "ee" * 32
            checks, _verdict = self.assertFailsOnly(ds, ["BINDING-PRIOR-RUN"])
            self.assertIn("%s differs" % field, table(checks)["BINDING-PRIOR-RUN"]["detail"])

    def test_binding_equality_is_recomputed_not_trusted(self):
        ds = build_supp()
        ds.binding["prior_run"]["image_sha256"] = "ee" * 32
        # the file still claims everything is equal
        self.assertTrue(ds.binding["all_equal"])
        self.assertTrue(all(ds.binding["equal"].values()))
        self.assertFailsOnly(ds, ["BINDING-PRIOR-RUN"])

    def test_binding_a_failed_measurement_is_a_failure(self):
        for field in E.BINDING_SHA_FIELDS + ("baseline_result", "units"):
            ds = build_supp()
            ds.binding["this_run"][field] = None            # "a measured value can be null when a measurement failed"
            self.assertFailsOnly(ds, ["BINDING-PRIOR-RUN"])

    def test_binding_units_and_baseline_must_match(self):
        for unit in E.UNIT_FILES:
            ds = build_supp()
            ds.binding["this_run"]["units"][unit] = "ee" * 32
            checks, _verdict = self.assertFailsOnly(ds, ["BINDING-PRIOR-RUN"])
            self.assertIn("units.%s differs" % unit, table(checks)["BINDING-PRIOR-RUN"]["detail"])
        ds = build_supp()
        del ds.binding["prior_run"]["units"]["arc-updater.timer"]
        self.assertFailsOnly(ds, ["BINDING-PRIOR-RUN"])
        ds = build_supp()
        ds.binding["this_run"]["units"]["extra.service"] = "ee" * 32
        self.assertFailsOnly(ds, ["BINDING-PRIOR-RUN"])
        ds = build_supp()
        ds.binding["this_run"]["baseline_result"] = BASELINE + " changed"
        checks, _verdict = self.assertFailsOnly(ds, ["BINDING-PRIOR-RUN"])
        self.assertIn("baseline_result differs", table(checks)["BINDING-PRIOR-RUN"]["detail"])
        ds = build_supp()
        ds.binding["this_run"]["baseline_result"] = ""
        ds.binding["prior_run"]["baseline_result"] = ""
        self.assertFailsOnly(ds, ["BINDING-PRIOR-RUN"])

    def test_binding_malformed_digest_fails(self):
        ds = build_supp()
        ds.binding["this_run"]["installer_sha256"] = "not-a-digest"
        self.assertFailsOnly(ds, ["BINDING-PRIOR-RUN"])
        ds = build_supp()
        ds.binding["prior_run"]["image_sha256"] = None
        self.assertFailsOnly(ds, ["BINDING-PRIOR-RUN"])

    def test_binding_accepts_the_sha256_prefix(self):
        ds = build_supp()
        ds.binding["prior_run"]["image_sha256"] = "sha256:" + IMAGE.upper()
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_binding_memory_sizes_and_run_id(self):
        for path, value in ((("this_run", "vm_memory_mb"), 6144), (("prior_run", "vm_memory_mb"), 4096),
                            (("prior_run", "run_id"), 123), (("prior_run", "run_id"), None), (("schema",), "other")):
            ds = build_supp()
            target = ds.binding
            for key in path[:-1]:
                target = target[key]
            target[path[-1]] = value
            checks, _verdict = judge(ds)
            self.assertEqual(table(checks)["BINDING-PRIOR-RUN"]["result"], "FAIL", path)

    def test_binding_vm_memory_must_equal_the_config(self):
        ds = build_supp()
        ds.config["vm_memory_mb"] = 6144
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["BINDING-PRIOR-RUN"]["result"], "FAIL")
        ds = build_supp()
        del ds.config["vm_memory_mb"]
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["BINDING-PRIOR-RUN"]["result"], "FAIL")
        self.assertIn("UNPROVED", table(checks)["BINDING-PRIOR-RUN"]["detail"])

    def test_binding_must_describe_the_bytes_the_config_pinned(self):
        ds = build_supp()
        ds.binding["this_run"]["node_sha256"] = "ee" * 32
        ds.binding["prior_run"]["node_sha256"] = "ee" * 32      # pairwise equal, but not the node the config pinned
        checks, _verdict = self.assertFailsOnly(ds, ["BINDING-PRIOR-RUN"])
        self.assertIn("differs from node_sha256 of config-effective.json", table(checks)["BINDING-PRIOR-RUN"]["detail"])
        ds = build_supp()
        ds.binding["this_run"]["launcher_sha256"] = "ee" * 32
        ds.binding["prior_run"]["launcher_sha256"] = "ee" * 32
        checks, _verdict = self.assertFailsOnly(ds, ["BINDING-PRIOR-RUN"])
        self.assertIn("differs from expected_launcher_sha256", table(checks)["BINDING-PRIOR-RUN"]["detail"])
        ds = build_supp()
        ds.config.pop("node_sha256")                            # the config must pin the node: fail closed
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["BINDING-PRIOR-RUN"]["result"], "FAIL")
        self.assertIn("UNPROVED: config-effective.json has no node_sha256", table(checks)["BINDING-PRIOR-RUN"]["detail"])

    def test_binding_must_describe_the_node_that_ran(self):
        ds = build_supp()
        for sample in bridged_of(ds):
            sample["node_exe_sha256"] = "ee" * 32               # the config and the binding agree, the node differs
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["BINDING-PRIOR-RUN"]["result"], "FAIL")
        self.assertIn("differs from the node binary that ran", table(checks)["BINDING-PRIOR-RUN"]["detail"])

    def test_binding_this_run_must_be_the_4096_mb_canary_even_when_everything_agrees(self):
        ds = build_supp()
        ds.binding["this_run"]["vm_memory_mb"] = 6144           # this run claims 6144 and the config and the event agree
        ds.config["vm_memory_mb"] = 6144
        for event in ds.events:
            if event["name"] == "vm_memory":
                event["detail"]["configured_mb"] = 6144
                event["detail"]["mem_total_kb"] = 6_000_000
        for sample in bridged_of(ds):
            sample["mem_total_kb"] = 6_000_000
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["BINDING-PRIOR-RUN"]["result"], "FAIL")
        self.assertIn("this_run.vm_memory_mb is 6144, expected 4096", table(checks)["BINDING-PRIOR-RUN"]["detail"])
        self.assertEqual(table(checks)["VM-MEMORY"]["result"], "FAIL")

    def test_binding_must_describe_the_launcher_that_ran(self):
        ds = build_supp()
        for sample in bridged_of(ds):
            sample["launcher_sha256"] = "ee" * 32               # the binding and the config agree, the node ran another launcher
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["BINDING-PRIOR-RUN"]["result"], "FAIL")
        self.assertIn("differs from the launcher the node ran", table(checks)["BINDING-PRIOR-RUN"]["detail"])

    def test_binding_memory_must_match_the_vm_event(self):
        ds = build_supp()
        for event in ds.events:
            if event["name"] == "vm_memory":
                event["detail"]["configured_mb"] = 3072
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["BINDING-PRIOR-RUN"]["result"], "FAIL")
        self.assertEqual(table(checks)["VM-MEMORY"]["result"], "FAIL")

    def test_other_profiles_skip_the_binding(self):
        for builder in (build_full,):
            checks, _verdict = judge(builder())
            self.assertEqual(table(checks)["BINDING-PRIOR-RUN"]["result"], "SKIP")
        ds = build_full()
        ds.binding = make_binding()
        ds.binding["prior_run"]["node_sha256"] = "ee" * 32      # irrelevant for the full profile
        self.assertEqual(table(judge(ds)[0])["BINDING-PRIOR-RUN"]["result"], "SKIP")


# ------------------------------------------------------------------------------------------------------------------
# freshness: the node's own registration (FRESH-AGE, FRESH-DISTINCT) and the public scoreboard
# ------------------------------------------------------------------------------------------------------------------

def set_registration(sample, age):
    """Make `sample` show a registration that is `age` seconds old (consistently)."""
    stamp = int(round((sample["epoch"] - age) * 1000))
    sample["last_registration_unix_ms"] = stamp
    sample["registration_age_s"] = round(sample["epoch"] - stamp / 1000.0, 3)


class TestFreshAge(Base):
    def test_every_steady_sample_must_be_fresh(self):
        ds = build_supp()
        set_registration(steady_of(ds)[20], 91.0)
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-AGE"])
        self.assertIn("registration is stale, age 91.0 s > 90 s", table(checks)["FRESH-AGE"]["detail"])

    def test_exactly_90_seconds_still_passes(self):
        ds = build_supp()
        set_registration(steady_of(ds)[20], 90.0)
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_missing_timestamp_is_unproved(self):
        ds = build_supp()
        steady_of(ds)[33]["last_registration_unix_ms"] = None
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-AGE"])
        self.assertIn("UNPROVED: last_registration_unix_ms is missing", table(checks)["FRESH-AGE"]["detail"])
        ds = build_supp()
        steady_of(ds)[33]["registration_age_s"] = None
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-AGE"])
        self.assertIn("UNPROVED: registration_age_s is missing", table(checks)["FRESH-AGE"]["detail"])

    def test_zero_or_invalid_timestamp_fails(self):
        ds = build_supp()
        steady_of(ds)[33]["last_registration_unix_ms"] = 0
        self.assertFailsOnly(ds, ["FRESH-AGE"])
        ds = build_supp()
        steady_of(ds)[33]["last_registration_unix_ms"] = 1_790_000_000_000.5      # not an integer
        self.assertFailsOnly(ds, ["FRESH-AGE"])

    def test_a_negative_age_beyond_the_jitter_fails(self):
        ds = build_supp()
        sample = steady_of(ds)[40]
        set_registration(sample, -3.0)                        # the timestamp is in the future
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-AGE"])
        self.assertIn("is negative", table(checks)["FRESH-AGE"]["detail"])

    def test_a_heartbeat_landing_after_the_sample_started_is_not_a_failure(self):
        ds = build_supp()
        set_registration(steady_of(ds)[40], -0.4)
        checks, _verdict = judge(ds)
        self.assertEqual(fails(checks), [])
        self.assertIn("1 age(s) between -1.5 and 0 s", table(checks)["FRESH-AGE"]["detail"])

    def test_the_reported_age_must_match_the_timestamp(self):
        ds = build_supp()
        sample = steady_of(ds)[40]
        sample["registration_age_s"] = sample["registration_age_s"] + 1.6
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-AGE"])
        self.assertIn("disagrees with epoch - last_registration_unix_ms/1000", table(checks)["FRESH-AGE"]["detail"])
        ds = build_supp()
        sample = steady_of(ds)[40]
        sample["registration_age_s"] = sample["registration_age_s"] + 1.4         # within the 1.5 s tolerance
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_a_non_finite_age_is_a_malformed_record_and_fails(self):
        ds = build_supp()
        steady_of(ds)[20]["registration_age_s"] = float("nan")
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["FRESH-AGE"]["result"], "FAIL")
        self.assertIn("miss required fields or have wrong types", table(checks)["FRESH-AGE"]["detail"])
        self.assertEqual(table(checks)["EVIDENCE-FILES"]["result"], "FAIL")
        ds = build_supp()
        steady_of(ds)[20]["registration_age_s"] = float("inf")
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["FRESH-AGE"]["result"], "FAIL")

    def test_a_frozen_sample_timestamp_goes_stale(self):
        ds = build_supp()
        steady = steady_of(ds)
        frozen = steady[10]["last_registration_unix_ms"]
        for sample in steady[10:]:
            sample["last_registration_unix_ms"] = frozen
            sample["registration_age_s"] = round(sample["epoch"] - frozen / 1000.0, 3)
        self.assertFailsOnly(ds, ["FRESH-AGE"])

    def test_only_the_steady_window_is_judged(self):
        ds = build_supp()
        set_registration(bridged_of(ds)[2], 500.0)            # during the settle period
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_source_bind_sentence_is_in_every_outcome(self):
        checks, _verdict = judge(build_supp())
        self.assertEqual(table(checks)["FRESH-AGE"]["detail"].count(SOURCE_BIND), 1)
        ds = build_supp()
        set_registration(steady_of(ds)[20], 120.0)
        ds2 = build_supp()
        ds2.events = [e for e in ds2.events if e["name"] != "steady_begin"]
        for broken in (ds, ds2):
            checks, _verdict = judge(broken)
            self.assertEqual(table(checks)["FRESH-AGE"]["result"], "FAIL")
            self.assertEqual(table(checks)["FRESH-AGE"]["detail"].count(SOURCE_BIND), 1)


class TestFreshDistinct(Base):
    def window(self, ds):
        return ds.begin, steady_of(ds)[-1]["epoch"]

    def test_a_frozen_timestamp_fails_even_when_adjacent_differences_would_pass(self):
        ds = build_supp()
        low, high = self.window(ds)
        frozen = heartbeat_ms(low + 1000)
        for poll in ds.heartbeats:
            if poll["obs_epoch"] >= low + 1000:
                poll["ts_ms"] = frozen                        # the node stopped registering but polls keep answering
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-DISTINCT"])
        detail = table(checks)["FRESH-DISTINCT"]["detail"]
        self.assertTrue("old (limit 90 s)" in detail or "only" in detail or "gap of" in detail, detail)

    def test_a_gap_above_90_seconds_between_distinct_timestamps_fails(self):
        ds = build_supp()
        low, _high = self.window(ds)
        start = low + 2000
        held = heartbeat_ms(start)
        for poll in ds.heartbeats:
            if start <= poll["obs_epoch"] < start + 100:
                poll["ts_ms"] = held                          # no new heartbeat accepted for 100 s
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-DISTINCT"])
        self.assertIn("ms between consecutive distinct timestamps (limit 90000 ms)", table(checks)["FRESH-DISTINCT"]["detail"])

    def test_a_gap_of_exactly_90_seconds_passes(self):
        ds = build_supp()
        low, _high = self.window(ds)
        start = low + 2000
        held = heartbeat_ms(start)
        for poll in ds.heartbeats:
            if start <= poll["obs_epoch"] < start + 75:       # 75 s held + the 15 s tick = 90 s between distinct values
                poll["ts_ms"] = held
        checks, _verdict = judge(ds)
        self.assertEqual(fails(checks), [])

    def test_a_poll_gap_can_hide_a_skipped_success(self):
        ds = build_supp()
        low, _high = self.window(ds)
        drop_polls(ds, low + 3000, low + 3016)                # 21 s without a poll
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-DISTINCT"])
        self.assertIn("UNPROVED: poll gap can hide a skipped success", table(checks)["FRESH-DISTINCT"]["detail"])

    def test_a_poll_gap_of_10_seconds_passes(self):
        ds = build_supp()
        low, _high = self.window(ds)
        drop_polls(ds, low + 3000, low + 3001)                # one poll lost: neighbours are 10 s apart
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_polls_must_cover_the_start_and_the_end(self):
        ds = build_supp()
        low, high = self.window(ds)
        drop_polls(ds, low - 1, low + 40)
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["FRESH-DISTINCT"]["result"], "FAIL")
        self.assertIn("UNPROVED: first poll is", table(checks)["FRESH-DISTINCT"]["detail"])
        ds = build_supp()
        drop_polls(ds, high - 40, high + 100)
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["FRESH-DISTINCT"]["result"], "FAIL")
        self.assertIn("UNPROVED: last poll is", table(checks)["FRESH-DISTINCT"]["detail"])

    def test_a_poll_just_before_the_window_start_is_required(self):
        ds = build_supp()
        low, _high = self.window(ds)
        drop_polls(ds, low - 40, low + 0.001)                 # a poll exactly at the start would count as "before"
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-DISTINCT"])
        self.assertIn("UNPROVED: no poll within 15 s before the window start", table(checks)["FRESH-DISTINCT"]["detail"])

    def test_the_timestamp_at_the_window_start_must_be_fresh(self):
        ds = build_supp()
        low, _high = self.window(ds)
        old = heartbeat_ms(low - 100)
        for poll in ds.heartbeats:
            if poll["obs_epoch"] <= low:
                poll["ts_ms"] = old                           # the last value seen before the window is 100 s old
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-DISTINCT"])
        self.assertIn("before the window start is", table(checks)["FRESH-DISTINCT"]["detail"])

    def test_the_bridge_over_the_start_boundary_is_bounded(self):
        ds = build_supp()
        low, _high = self.window(ds)
        before = heartbeat_ms(low - 20)
        for poll in ds.heartbeats:
            if low - 25 <= poll["obs_epoch"] <= low + 75:
                poll["ts_ms"] = before                        # nothing new until 75 s after the start
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["FRESH-DISTINCT"]["result"], "FAIL")
        self.assertIn("between the last timestamp before the window and the first one after it", table(checks)["FRESH-DISTINCT"]["detail"])

    def test_the_timestamp_at_the_window_end_must_be_fresh(self):
        ds = build_supp()
        _low, high = self.window(ds)
        stale = heartbeat_ms(high - 95)
        for poll in ds.heartbeats:
            if poll["obs_epoch"] >= high - 95:
                poll["ts_ms"] = stale
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["FRESH-DISTINCT"]["result"], "FAIL")
        self.assertIn("by the window end is", table(checks)["FRESH-DISTINCT"]["detail"])

    def test_timestamps_must_strictly_increase(self):
        ds = build_supp()
        low, _high = self.window(ds)
        victims = [p for p in ds.heartbeats if low + 4000 <= p["obs_epoch"] < low + 4005]
        self.assertEqual(len(victims), 1)
        victims[0]["ts_ms"] = heartbeat_ms(low + 3000)        # A, B, A: an old value comes back
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-DISTINCT"])
        self.assertIn("went backwards", table(checks)["FRESH-DISTINCT"]["detail"])

    def test_a_step_backwards_across_the_start_boundary_is_caught(self):
        ds = build_supp()
        low, _high = self.window(ds)
        stale = heartbeat_ms(low - 60)                        # older than the last value seen before the window
        drop_polls(ds, low - 0.5, low + 0.5)                  # the first poll inside the window is now the regression
        victims = [p for p in ds.heartbeats if low < p["obs_epoch"] <= low + 5]
        self.assertEqual(len(victims), 1)
        victims[0]["ts_ms"] = stale
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-DISTINCT"])
        self.assertIn("went backwards", table(checks)["FRESH-DISTINCT"]["detail"])

    def set_period(self, ds, period):
        """The node registers every `period` seconds (15 s plus the round latency)."""
        for poll in ds.heartbeats:
            poll["ts_ms"] = int(round((ANCHOR + period * math.floor((poll["obs_epoch"] - ANCHOR) / period)) * 1000))

    def test_too_few_distinct_timestamps_fail(self):
        ds = build_supp()
        self.set_period(ds, 45.0)                             # a new stamp only every 45 s: gaps are fine, the count is not
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-DISTINCT"])
        self.assertIn("distinct timestamps, at least", table(checks)["FRESH-DISTINCT"]["detail"])
        self.assertIn("(0.5 x window / 15 s)", table(checks)["FRESH-DISTINCT"]["detail"])

    def test_a_healthy_slow_node_passes_the_count(self):
        # the next round is scheduled 15 s AFTER the previous one completes: 16 s normally, 20 s when a coordinator hangs
        for period in (16.0, 20.0, 29.0):
            ds = build_supp()
            self.set_period(ds, period)
            checks, _verdict = judge(ds)
            self.assertEqual(table(checks)["FRESH-DISTINCT"]["result"], "PASS", period)
            self.assertEqual(fails(checks), [], period)

    def test_the_count_floor_is_half_the_nominal_rate(self):
        low, high = self.window(build_supp())
        needed = 0.5 * (high - low) / 15.0
        ds = build_supp()
        self.set_period(ds, 31.0)                             # about 267 stamps against 0.5 x 8517 / 15 = 284
        self.assertLess((high - low) / 31.0, needed)
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-DISTINCT"])
        self.assertIn("only", table(checks)["FRESH-DISTINCT"]["detail"])
        self.assertEqual(E.DISTINCT_MIN_FRACTION, 0.5)

    def test_failed_polls_are_counted_but_do_not_fail_when_the_gaps_hold(self):
        ds = build_supp()
        for index, poll in enumerate(ds.heartbeats):
            if index % 7 == 3:
                poll["ts_ms"] = None
                poll["registered"] = None
                poll["error"] = "timeout"
        checks, _verdict = judge(ds)
        self.assertEqual(fails(checks), [])
        self.assertRegex(table(checks)["FRESH-DISTINCT"]["detail"], r"\(\d+ without a timestamp\)")

    def test_no_timestamp_at_all_is_unproved(self):
        ds = build_supp()
        for poll in ds.heartbeats:
            poll["ts_ms"] = None
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["FRESH-DISTINCT"]["result"], "FAIL")
        self.assertIn("UNPROVED", table(checks)["FRESH-DISTINCT"]["detail"])

    def test_missing_poll_file_is_unproved(self):
        ds = build_supp()
        ds.heartbeats = None
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-DISTINCT"])
        self.assertIn("UNPROVED: heartbeats.jsonl is missing", table(checks)["FRESH-DISTINCT"]["detail"])

    def test_a_poll_file_without_the_window_is_unproved(self):
        ds = build_supp()
        ds.heartbeats = [p for p in ds.heartbeats if p["obs_epoch"] < ds.begin - 100]
        checks, _verdict = self.assertFailsOnly(ds, ["FRESH-DISTINCT"])
        self.assertIn("UNPROVED: no heartbeat poll inside the steady window", table(checks)["FRESH-DISTINCT"]["detail"])

    def test_a_malformed_poll_line_is_reported(self):
        texts = to_texts(build_supp())
        texts["heartbeats.jsonl"] += "{\"ts_ms\": 5}\nnot json\n"
        ev = E.evidence_from_texts(texts)
        checks, _w, _c = E.analyze(ev)
        detail = table(checks)["FRESH-DISTINCT"]["detail"]
        self.assertEqual(table(checks)["FRESH-DISTINCT"]["result"], "FAIL")
        self.assertIn("miss required fields", detail)
        self.assertIn("not valid JSON objects", detail)

    def test_source_bind_sentence_is_in_every_outcome(self):
        checks, _verdict = judge(build_supp())
        self.assertIn(SOURCE_BIND, table(checks)["FRESH-DISTINCT"]["detail"])
        ds = build_supp()
        ds.heartbeats = [p for p in ds.heartbeats if p["obs_epoch"] < ds.begin + 100]
        ds2 = build_supp()
        ds2.heartbeats = None
        ds3 = build_supp()
        ds3.events = [e for e in ds3.events if e["name"] != "steady_begin"]
        for broken in (ds, ds2, ds3):
            checks, _verdict = judge(broken)
            self.assertEqual(table(checks)["FRESH-DISTINCT"]["result"], "FAIL")
            self.assertEqual(table(checks)["FRESH-DISTINCT"]["detail"].count(SOURCE_BIND), 1)

    def test_the_full_profile_uses_the_recovery_as_the_window_start(self):
        ds = build_full()
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["FRESH-DISTINCT"]["result"], "PASS")
        low = ds.rec + 2
        for poll in ds.heartbeats:
            if poll["obs_epoch"] >= low + 2000:
                poll["ts_ms"] = heartbeat_ms(low + 2000)
        self.assertFailsOnly(ds, ["FRESH-DISTINCT"])


def host_window(ds):
    begin = [e for e in ds.events if e["name"] == "steady_begin"][0]["host_epoch"]
    end = [e for e in ds.events if e["name"] == "steady_end"][0]["host_epoch"]
    return begin, end


class TestPublicFreshness(Base):
    def test_interval_above_60_seconds_fails(self):
        ds = build_supp()
        ds.config["scoreboard_interval_s"] = 300
        checks, _verdict = self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        self.assertIn("scoreboard_interval_s is 300 s in config-effective.json, above the 60 s ceiling",
                      table(checks)["PUBLIC-FRESHNESS"]["detail"])

    def test_interval_between_30_and_60_is_accepted(self):
        for value in (30, 45, 60):
            ds = build_supp()
            ds.config["scoreboard_interval_s"] = value
            self.assertEqual(fails(judge(ds)[0]), [], value)

    def test_the_default_interval_is_30_seconds(self):
        self.assertEqual(E.effective_config({})[0]["scoreboard_interval_s"], 30.0)

    def test_five_minute_snapshots_cannot_prove_the_90_second_condition(self):
        ds = build_supp()
        low, high = host_window(ds)
        ds.scoreboard = make_probes(low - 100, high + 30, step=300.0)
        checks, _verdict = self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        self.assertIn("UNPROVED: scoreboard coverage gap", table(checks)["PUBLIC-FRESHNESS"]["detail"])

    def test_a_gap_of_60_seconds_passes_and_61_fails(self):
        for step, ok in ((60.0, True), (61.0, False)):
            ds = build_supp()
            low, high = host_window(ds)
            ds.scoreboard = make_probes(low - 10, high + 30, step=step)
            checks, _verdict = judge(ds)
            self.assertEqual(table(checks)["PUBLIC-FRESHNESS"]["result"], "PASS" if ok else "FAIL", step)

    def test_the_first_and_the_last_round_must_be_near_the_window_edges(self):
        ds = build_supp()
        low, high = host_window(ds)
        ds.scoreboard = make_probes(low + 61, high + 30)
        checks, _verdict = self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        self.assertIn("first probe is", table(checks)["PUBLIC-FRESHNESS"]["detail"])
        ds = build_supp()
        ds.scoreboard = make_probes(low - 100, high - 61)
        checks, _verdict = self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        self.assertIn("last probe is", table(checks)["PUBLIC-FRESHNESS"]["detail"])

    def test_every_round_in_the_window_must_find_the_node(self):
        ds = build_supp()
        low, high = host_window(ds)
        target = [p for p in ds.scoreboard if low + 3000 <= p["host_epoch"] <= low + 3031][0]
        for result in target["results"]:
            result.update({"found": False, "name": None, "worker_id": None, "row": None, "registered_at": None})
        target["found_count"] = 0
        checks, _verdict = self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        self.assertIn("no origin lists the node (0 of 6)", table(checks)["PUBLIC-FRESHNESS"]["detail"])

    def test_one_coordinator_is_enough(self):
        ds = build_supp()
        ds.scoreboard = make_probes(*host_window(ds), found_origins=(3,))
        low, high = host_window(ds)
        ds.scoreboard = make_probes(low - 100, high + 30, found_origins=(3,))
        checks, _verdict = judge(ds)
        self.assertEqual(fails(checks), [])
        self.assertIn("(min 1, max 1)", table(checks)["PUBLIC-FRESHNESS"]["detail"])

    def test_a_miss_before_the_steady_window_is_not_a_failure(self):
        ds = build_supp()
        low, _high = host_window(ds)
        early = [p for p in ds.scoreboard if p["host_epoch"] < low - 10]
        self.assertTrue(early)
        for probe in early:
            for result in probe["results"]:
                result.update({"found": False, "name": None, "worker_id": None, "row": None})
            probe["found_count"] = 0
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_the_matrix_is_information_only(self):
        checks, _verdict = judge(build_supp())
        detail = table(checks)["PUBLIC-FRESHNESS"]["detail"]
        self.assertEqual(table(checks)["PUBLIC-FRESHNESS"]["result"], "PASS")
        self.assertIn("matrix INFO: origin 0 abc00000 275/275", detail)
        self.assertIn("origin 5 abc00005 0/275", detail)

    def test_the_exact_privacy_safe_name_is_required_wherever_it_is_served(self):
        ds = build_supp()
        low, _high = host_window(ds)
        probe = [p for p in ds.scoreboard if p["host_epoch"] > low + 500][0]
        probe["results"][4]["name"] = "node-deadbeef"
        checks, _verdict = self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        self.assertIn("serves name 'node-deadbeef', expected node-abababab", table(checks)["PUBLIC-FRESHNESS"]["detail"])
        ds = build_supp()
        probe = [p for p in ds.scoreboard if p["host_epoch"] > low + 500][0]
        probe["results"][1]["name"] = "my-laptop"
        self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        ds = build_supp()
        probe = [p for p in ds.scoreboard if p["host_epoch"] > low + 500][0]
        probe["results"][2]["name"] = None
        self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])

    def test_the_worker_id_must_be_0x_and_the_address(self):
        ds = build_supp()
        low, _high = host_window(ds)
        probe = [p for p in ds.scoreboard if p["host_epoch"] > low + 500][0]
        probe["results"][0]["worker_id"] = "0x" + "cd" * 32
        checks, _verdict = self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        self.assertIn("serves worker_id", table(checks)["PUBLIC-FRESHNESS"]["detail"])
        ds = build_supp()
        probe = [p for p in ds.scoreboard if p["host_epoch"] > low + 500][0]
        probe["results"][0]["worker_id"] = T.A64                   # no 0x prefix
        self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        ds = build_supp()
        probe = [p for p in ds.scoreboard if p["host_epoch"] > low + 500][0]
        probe["results"][0]["worker_id"] = "0x" + T.A64.upper()   # the same worker in upper case is the same worker
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_the_served_row_must_match_too(self):
        ds = build_supp()
        low, _high = host_window(ds)
        probe = [p for p in ds.scoreboard if p["host_epoch"] > low + 500][0]
        probe["results"][0]["row"]["name"] = "node-00000000"
        checks, _verdict = self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        self.assertIn("row name is", table(checks)["PUBLIC-FRESHNESS"]["detail"])

    def test_a_probe_with_zero_origins_fails(self):
        ds = build_supp()
        low, _high = host_window(ds)
        probe = [p for p in ds.scoreboard if p["host_epoch"] > low + 500][0]
        probe["results"] = []
        probe["found_count"] = 0
        checks, _verdict = self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        self.assertIn("probed no origin", table(checks)["PUBLIC-FRESHNESS"]["detail"])

    def test_found_count_must_agree_with_the_results(self):
        ds = build_supp()
        low, _high = host_window(ds)
        probe = [p for p in ds.scoreboard if p["host_epoch"] > low + 500][0]
        probe["found_count"] = 6
        checks, _verdict = self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        self.assertIn("says found_count 6 but 5 result(s) are found", table(checks)["PUBLIC-FRESHNESS"]["detail"])

    def test_the_probe_must_look_up_the_nodes_address(self):
        ds = build_supp()
        low, _high = host_window(ds)
        probe = [p for p in ds.scoreboard if p["host_epoch"] > low + 500][0]
        probe["address"] = "cd" * 32
        checks, _verdict = self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        self.assertIn("looked up address", table(checks)["PUBLIC-FRESHNESS"]["detail"])

    def test_server_date_must_agree_with_the_runner_clock(self):
        ds = build_supp()
        low, _high = host_window(ds)
        probe = [p for p in ds.scoreboard if p["host_epoch"] > low + 500][0]
        probe["results"][0]["server_date"] = email.utils.formatdate(probe["host_epoch"] + 200, usegmt=True)
        checks, _verdict = self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        self.assertIn("server Date differs from the host clock by 200 s", table(checks)["PUBLIC-FRESHNESS"]["detail"])
        ds = build_supp()
        probe = [p for p in ds.scoreboard if p["host_epoch"] > low + 500][0]
        probe["results"][0]["server_date"] = email.utils.formatdate(probe["host_epoch"] + 119, usegmt=True)
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_an_unparsable_or_missing_server_date_is_ignored(self):
        ds = build_supp()
        for probe in ds.scoreboard:
            for result in probe["results"]:
                result["server_date"] = "yesterday-ish" if result["origin"] % 2 else None
        checks, _verdict = judge(ds)
        self.assertEqual(fails(checks), [])
        self.assertIn("server Date parsed in 0 result(s)", table(checks)["PUBLIC-FRESHNESS"]["detail"])

    def test_missing_scoreboard_file_is_unproved(self):
        ds = build_supp()
        ds.scoreboard = None
        checks, _verdict = self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        self.assertIn("UNPROVED: scoreboard.jsonl is missing", table(checks)["PUBLIC-FRESHNESS"]["detail"])

    def test_probes_outside_the_window_do_not_count(self):
        ds = build_supp()
        low, high = host_window(ds)
        ds.scoreboard = make_probes(high + 100, high + 400)
        checks, _verdict = self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        self.assertIn("UNPROVED: no scoreboard probe inside the steady window", table(checks)["PUBLIC-FRESHNESS"]["detail"])

    def test_probe_numbers_must_not_repeat(self):
        ds = build_supp()
        low, _high = host_window(ds)
        inside = [p for p in ds.scoreboard if p["host_epoch"] > low + 500]
        inside[1]["probe"] = inside[0]["probe"]
        self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])

    def test_the_ttl_binding_is_in_every_outcome(self):
        checks, _verdict = judge(build_supp())
        self.assertIn(TTL_BIND, table(checks)["PUBLIC-FRESHNESS"]["detail"])
        broken = []
        ds = build_supp()
        ds.scoreboard = None                                  # unproved: the file is missing
        broken.append(ds)
        ds = build_supp()
        ds.config["scoreboard_interval_s"] = 120              # fails early
        broken.append(ds)
        ds = build_supp()
        ds.events = [e for e in ds.events if e["name"] != "steady_begin"]      # the window cannot be placed
        broken.append(ds)
        ds = build_supp()
        ds.scoreboard[100]["results"] = []                    # a problem found while judging the rounds
        broken.append(ds)
        for ds in broken:
            checks, _verdict = judge(ds)
            self.assertEqual(table(checks)["PUBLIC-FRESHNESS"]["result"], "FAIL")
            self.assertEqual(table(checks)["PUBLIC-FRESHNESS"]["detail"].count(TTL_BIND), 1)

    def test_the_host_clock_is_the_runners_not_the_guests(self):
        ds = build_supp()
        for probe in ds.scoreboard:
            probe["finished_epoch"] = probe["host_epoch"] + 2.5      # the new optional field is accepted
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_the_full_profile_judges_the_window_after_the_reboot(self):
        ds = build_full()
        low, _high = host_window(ds)
        miss = [p for p in ds.scoreboard if low + 1000 <= p["host_epoch"] <= low + 1031][0]
        for result in miss["results"]:
            result.update({"found": False, "name": None, "worker_id": None, "row": None})
        miss["found_count"] = 0
        self.assertFailsOnly(ds, ["PUBLIC-FRESHNESS"])
        ds = build_full()
        reboot_gap = [p for p in ds.scoreboard if p["host_epoch"] < ds.rec - 20]
        for probe in reboot_gap:                              # the node was down during the reboot: not judged
            for result in probe["results"]:
                result.update({"found": False, "name": None, "worker_id": None, "row": None})
            probe["found_count"] = 0
        self.assertEqual(fails(judge(ds)[0]), [])


# ------------------------------------------------------------------------------------------------------------------
# the "resources" supplement profile
# ------------------------------------------------------------------------------------------------------------------

def shift_sample(sample, seconds):
    """Move a sample in time and keep its registration fields consistent."""
    sample["epoch"] = sample["epoch"] + seconds
    sample["uptime_s"] = sample["uptime_s"] + seconds
    stamp = heartbeat_ms(sample["epoch"])
    sample["last_registration_unix_ms"] = stamp
    sample["registration_age_s"] = round(sample["epoch"] - stamp / 1000.0, 3)


def move_event(ds, name, t, guest=True):
    for event in ds.events:
        if event["name"] == name:
            T.set_event_time(event, t)
            if not guest:
                event["guest_epoch"] = None
            return event
    raise KeyError(name)


class TestSupplementProfile(Base):
    def test_steady_duration_is_measured_first_to_last_sample(self):
        # end - steady_begin would be 8156 s (enough), first to last sample is 8040 s (not enough)
        ds = build_supp()
        move_event(ds, "steady_end", ds.t0 + 8456)
        checks, _verdict = judge(ds)
        rows = table(checks)
        self.assertEqual(rows["STEADY-DURATION"]["result"], "FAIL")
        self.assertIn("= 8040 s (required >= 8100 s)", rows["STEADY-DURATION"]["detail"])
        self.assertIn("first to last sample", rows["STEADY-DURATION"]["detail"])
        self.assertEqual(rows["SAMPLES-STEADY"]["result"], "FAIL")      # 135 samples
        # one more sample: exactly 8100 s and exactly 136 samples pass those two checks (the total is still short)
        ds = build_supp()
        move_event(ds, "steady_end", ds.t0 + 8462)
        checks, _verdict = judge(ds)
        rows = table(checks)
        self.assertEqual(rows["STEADY-DURATION"]["result"], "PASS")
        self.assertEqual(rows["SAMPLES-STEADY"]["result"], "PASS")
        self.assertIn("136 bridged sample(s)", rows["SAMPLES-STEADY"]["detail"])
        self.assertEqual(rows["TOTAL-DURATION"]["result"], "FAIL")      # 8462 < 8700 from t0

    def test_fewer_than_136_samples_fail(self):
        ds = build_supp()
        steady = steady_of(ds)
        ds.samples.remove(steady[70])                                   # a hole: 141 samples, and a 120 s gap
        checks, _verdict = judge(ds)
        rows = table(checks)
        self.assertEqual(rows["SAMPLE-GAPS"]["result"], "FAIL")
        self.assertEqual(rows["SAMPLES-STEADY"]["result"], "PASS")
        ds = build_supp()
        keep = steady_of(ds)[:135]
        ds.samples = [s for s in ds.samples if s.get("node_exe") != T.NODE_EXE or s in keep or s["epoch"] < ds.begin]
        move_event(ds, "steady_end", keep[-1]["epoch"] + 1)
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["SAMPLES-STEADY"]["result"], "FAIL")
        self.assertIn("135 bridged sample(s) in the steady window (required >= 136)", table(checks)["SAMPLES-STEADY"]["detail"])

    def test_adjacent_gap_ceiling_is_65_seconds(self):
        ds = build_supp()
        shift_sample(steady_of(ds)[50], 5.0)                  # 65 s before it, 55 s after
        self.assertEqual(fails(judge(ds)[0]), [])
        ds = build_supp()
        shift_sample(steady_of(ds)[50], 6.0)                  # 66 s
        checks, _verdict = self.assertFailsOnly(ds, ["SAMPLE-GAPS"])
        self.assertIn("limit 65 s", table(checks)["SAMPLE-GAPS"]["detail"])

    def test_a_looser_gap_factor_cannot_lift_the_65_second_ceiling(self):
        ds = build_supp()
        ds.config["max_gap_factor"] = 2.5
        shift_sample(steady_of(ds)[50], 10.0)                 # 70 s
        checks, _verdict = judge(ds)
        rows = table(checks)
        self.assertEqual(rows["FLOORS"]["result"], "FAIL")
        self.assertIn("above the supplement ceiling of 65 s", rows["FLOORS"]["detail"])
        self.assertEqual(rows["SAMPLE-GAPS"]["result"], "FAIL")
        ds = build_supp()
        del ds.config["max_gap_factor"]                       # absent: the ceiling itself
        shift_sample(steady_of(ds)[50], 6.0)
        self.assertEqual(table(judge(ds)[0])["SAMPLE-GAPS"]["result"], "FAIL")

    def test_the_configured_65_004_second_gap_is_the_ceiling_and_not_a_lowering(self):
        cfg, issues, lowered = E.effective_config(build_supp().config)
        self.assertEqual(lowered, [])
        self.assertEqual(issues, [])
        self.assertEqual(cfg["gap_limit_s"], 65.0)
        self.assertEqual(E.effective_config({"profile": "resources"})[0]["gap_limit_s"], 65.0)
        self.assertEqual(E.effective_config({"profile": "full"})[0]["gap_limit_s"], 150.0)

    def test_the_floors_cannot_be_lowered(self):
        for key, value in (("min_total_s", 7800), ("min_steady_s", 7200), ("min_steady_samples", 121), ("settle_s", 120)):
            ds = build_supp()
            ds.config[key] = value
            checks, _verdict = judge(ds)
            rows = table(checks)
            self.assertEqual(rows["FLOORS"]["result"], "FAIL", key)
            self.assertIn("below the supplement floor", rows["FLOORS"]["detail"], key)
        ds = build_supp()
        ds.config["sample_interval_s"] = 120
        self.assertEqual(table(judge(ds)[0])["FLOORS"]["result"], "FAIL")

    def test_a_lowered_floor_is_still_judged_against_the_floor(self):
        ds = build_supp()
        ds.config["min_steady_s"] = 7200
        move_event(ds, "steady_end", ds.t0 + 8456)            # 8040 s first to last: enough for 7200, not for 8100
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["STEADY-DURATION"]["result"], "FAIL")
        self.assertEqual(table(checks)["FLOORS"]["result"], "FAIL")

    def test_the_supplement_requires_its_switches(self):
        for key, value in (("resources_required", False), ("scoreboard_required", False), ("battery", True)):
            ds = build_supp()
            ds.config[key] = value
            checks, verdict = judge(ds)
            self.assertEqual(table(checks)["FLOORS"]["result"], "FAIL", key)
            self.assertEqual(verdict["verdict"], "SUPPLEMENT_FAIL", key)
        ds = build_supp()
        del ds.config["resources_required"]
        del ds.config["scoreboard_required"]
        del ds.config["battery"]
        checks, verdict = judge(ds)
        self.assertEqual(verdict["verdict"], "SUPPLEMENT_PASS")           # the profile name alone switches everything on
        self.assertEqual(fails(checks), [])

    def test_battery_false_is_only_valid_for_the_resources_profile(self):
        ds = T.build()
        ds.config["battery"] = False
        checks, _verdict = judge(ds)
        self.assertIn("battery=false is only valid with profile 'resources'", table(checks)["FLOORS"]["detail"])
        self.assertEqual(table(checks)["REBOOT-BOOT-ID"]["result"], "PASS")     # still judged with the battery

    def test_a_forced_event_inside_the_steady_window_fails(self):
        ds = build_supp()
        ds.events.append(T.make_event(20, "kickstart_1", ds.begin + 1000, True))
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["EVENTS-ORDER"]["result"], "FAIL")
        self.assertIn("forced event(s) inside the steady window: kickstart_1", table(checks)["EVENTS-ORDER"]["detail"])

    def test_a_forced_event_before_steady_begin_is_tolerated(self):
        ds = build_supp()
        ds.events.append(T.make_event(20, "interrupt_block_start", ds.t0 + 100, True))
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_the_settle_period_is_enforced(self):
        ds = build_supp()
        move_event(ds, "steady_begin", ds.t0 + 200)
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["EVENTS-ORDER"]["result"], "FAIL")
        self.assertIn("only 200 s after t0", table(checks)["EVENTS-ORDER"]["detail"])
        ds = build_supp()
        ds.config["settle_s"] = 600                           # the producer's default settle is longer than the floor
        move_event(ds, "steady_begin", ds.t0 + 300)
        self.assertEqual(table(judge(ds)[0])["EVENTS-ORDER"]["result"], "FAIL")

    def test_the_event_contract_of_the_supplement(self):
        for name in ("apply2", "t0_bridged_healthy", "steady_begin", "final_stop_begin"):
            ds = build_supp()
            ds.events = [e for e in ds.events if e["name"] != name]
            checks, _verdict = judge(ds)
            self.assertEqual(table(checks)["EVENTS-ORDER"]["result"], "FAIL", name)
        ds = build_supp()
        ds.events.append(T.make_event(21, "apply2", ds.t0 - 50, True))
        self.assertEqual(table(judge(ds)[0])["EVENTS-ORDER"]["result"], "FAIL")
        ds = build_supp()
        [e for e in ds.events if e["name"] == "steady_begin"][0]["detail"]["last_forced_event"] = "reboot_issued"
        self.assertEqual(table(judge(ds)[0])["EVENTS-ORDER"]["result"], "FAIL")
        ds = build_supp()
        [e for e in ds.events if e["name"] == "steady_begin"][0]["forced"] = True
        self.assertEqual(table(judge(ds)[0])["EVENTS-ORDER"]["result"], "FAIL")

    def test_there_is_no_reboot_in_the_supplement(self):
        ds = build_supp()
        ds.after["boot_id"] = T.BOOT2
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["INVARIANTS-BEFORE-AFTER"]["result"], "FAIL")
        self.assertIn("the supplement has no reboot", table(checks)["INVARIANTS-BEFORE-AFTER"]["detail"])
        ds = build_supp()
        for sample in steady_of(ds)[60:]:
            sample["boot_id"] = T.BOOT2                       # a reboot in the steady window
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["STEADY-UNINTERRUPTED"]["result"], "FAIL")

    def test_only_the_three_supplement_live_checks_are_required(self):
        for live_id in E.SUPPLEMENT_LIVE_IDS:
            ds = build_supp()
            ds.live = [r for r in ds.live if r["id"] != live_id]
            checks, _verdict = judge(ds)
            self.assertEqual(table(checks)["LIVE-REQUIRED"]["result"], "FAIL", live_id)
            self.assertIn(live_id, table(checks)["LIVE-REQUIRED"]["detail"])
        ds = build_supp()
        ds.live = [r for r in ds.live if r["id"] in E.SUPPLEMENT_LIVE_IDS]
        self.assertEqual(fails(judge(ds)[0]), [])

    def test_extra_live_checks_are_merged_verbatim_and_can_fail_the_supplement(self):
        ds = build_supp()
        ds.live.append({"id": "L21-heartbeat-poller-running", "title": "poller", "result": "FAIL", "detail": "the poller died"})
        ds.live.append({"id": "L16-apply2-attempts", "title": "attempts", "result": "INFO", "detail": "1 attempt(s)"})
        checks, verdict = judge(ds)
        rows = table(checks)
        self.assertEqual(rows["L21-heartbeat-poller-running"]["detail"], "the poller died")
        self.assertEqual(rows["LIVE-RESULTS"]["result"], "FAIL")
        self.assertEqual(verdict["verdict"], "SUPPLEMENT_FAIL")
        self.assertTrue(verdict["statement"].startswith(SUPPLEMENT_SENTENCE))
        self.assertIn("Criteria NOT met; failed or unmet checks: LIVE-RESULTS, L21-heartbeat-poller-running", verdict["statement"])

    def test_a_live_check_may_not_reuse_an_evaluator_id(self):
        ds = build_supp()
        ds.live.append({"id": "RES-RSS", "title": "x", "result": "PASS", "detail": "x"})
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["LIVE-RESULTS"]["result"], "FAIL")
        self.assertIn("collide with evaluator ids: RES-RSS", table(checks)["LIVE-RESULTS"]["detail"])

    def test_the_supplement_never_yields_a_wave0_verdict(self):
        for mutate in (lambda d: None, lambda d: d.config.update({"launcher_source": "artifact"}),
                       lambda d: setattr(d, "oom", None)):
            ds = build_supp()
            mutate(ds)
            _checks, verdict = judge(ds)
            self.assertIn(verdict["verdict"], ("SUPPLEMENT_PASS", "SUPPLEMENT_FAIL"))
            self.assertFalse(verdict["is_post_g0_wave0"])

    def test_the_battery_only_checks_are_skipped_not_passed(self):
        checks, _verdict = judge(build_supp())
        rows = table(checks)
        for check_id in E.BATTERY_ONLY_IDS:
            self.assertEqual(rows[check_id]["result"], "SKIP")
        self.assertEqual(rows["HEALTH-BRIDGED"]["result"], "PASS")
        self.assertEqual(rows["REGISTRATION-LIVE"]["result"], "PASS")

    def test_live_registration_is_still_required(self):
        ds = build_supp()
        for sample in ds.samples:
            if sample.get("coordinators_registered") is not None:
                sample["coordinators_registered"] = 0
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["REGISTRATION-LIVE"]["result"], "FAIL")


# ------------------------------------------------------------------------------------------------------------------
# bounds: config, --bounds-file, floors
# ------------------------------------------------------------------------------------------------------------------

def build_smoke(**res):
    """A smoke-profile dataset that follows the new contract (10 s samples, no Astra floors)."""
    ds = T.build("smoke")
    p = dict(GOOD)
    p.update(res)
    add_resources(ds.samples, p)
    ds.events.append(T.make_event(17, "vm_memory", T.BASE - 850, False, {"configured_mb": 4096, "mem_total_kb": 4_000_000},
                                  guest=False))
    ds.config.update({"resources_required": True, "battery": True, "vm_memory_mb": 4096,
                      "resource_bounds": dict(BOUNDS, min_points=31)})
    ds.oom = make_oom(2)
    ds.p = p
    return ds


class TestBounds(Base):
    def test_absent_bounds_mean_the_adopted_numbers(self):
        ds = build_full()
        del ds.config["resource_bounds"]
        checks, verdict = judge(ds)
        self.assertEqual(fails(checks), [])
        self.assertEqual(verdict["resource_bounds"]["source"], "adopted defaults")
        self.assertEqual(verdict["resource_bounds"]["values"], E.ADOPTED_BOUNDS)

    def test_a_partial_bounds_object_falls_back_per_key(self):
        ds = build_full()
        ds.config["resource_bounds"] = {"rss_slope_mib_per_h_max": 50}
        _checks, verdict = judge(ds)
        self.assertEqual(verdict["resource_bounds"]["values"]["rss_slope_mib_per_h_max"], 50.0)
        self.assertEqual(verdict["resource_bounds"]["values"]["mem_available_min_mib"], 512.0)
        self.assertEqual(verdict["resource_bounds"]["differs_from_adopted"], ["rss_slope_mib_per_h_max"])

    def test_a_stricter_config_bound_is_applied(self):
        ds = build_full(avail_mib=100000.0)
        set_linear(ds, "node_rss_kb", 300.0, 30.0, 1024.0)
        ds.config["resource_bounds"] = dict(BOUNDS, rss_slope_mib_per_h_max=25)
        self.assertFailsOnly(ds, ["RES-RSS"])

    def test_a_looser_bound_is_a_floors_failure_and_the_adopted_number_is_judged(self):
        ds = build_full(avail_mib=100000.0)
        set_linear(ds, "node_rss_kb", 300.0, 150.0, 1024.0)
        ds.config["resource_bounds"] = dict(BOUNDS, rss_slope_mib_per_h_max=500)
        checks, verdict = judge(ds)
        rows = table(checks)
        self.assertEqual(rows["FLOORS"]["result"], "FAIL")
        self.assertIn("resource_bounds.rss_slope_mib_per_h_max=500 is looser than the adopted ARC-83 number 100",
                      rows["FLOORS"]["detail"])
        self.assertEqual(rows["RES-RSS"]["result"], "FAIL")             # still judged against 100
        self.assertEqual(verdict["verdict"], "WAVE0_FAIL")

    def test_every_adopted_number_is_a_floor(self):
        loosened = {"rss_slope_mib_per_h_max": 101, "mem_available_min_mib": 511, "projection_fraction": 0.6,
                    "projection_hours": 23, "disk_reserve_floor_b": 2147483647, "disk_reserve_fraction": 0.19,
                    "disk_runway_h_min": 71, "min_points": 120}
        for key, value in loosened.items():
            ds = build_full()
            ds.config["resource_bounds"] = dict(BOUNDS, **{key: value})
            checks, _verdict = judge(ds)
            self.assertEqual(table(checks)["FLOORS"]["result"], "FAIL", key)
            self.assertIn("resource_bounds.%s=" % key, table(checks)["FLOORS"]["detail"])
        tighter = {"rss_slope_mib_per_h_max": 99, "mem_available_min_mib": 513, "projection_fraction": 0.4,
                   "projection_hours": 25, "disk_reserve_floor_b": 2147483649, "disk_reserve_fraction": 0.21,
                   "disk_runway_h_min": 73, "min_points": 122}
        for key, value in tighter.items():
            ds = build_full()
            ds.config["resource_bounds"] = dict(BOUNDS, **{key: value})
            self.assertEqual(table(judge(ds)[0])["FLOORS"]["result"], "PASS", key)

    def test_null_cannot_switch_a_bound_off_under_the_full_profile(self):
        ds = build_full()
        ds.config["resource_bounds"] = dict(BOUNDS, rss_slope_mib_per_h_max=None)
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["FLOORS"]["result"], "FAIL")
        self.assertIn("rss_slope_mib_per_h_max=null is looser", table(checks)["FLOORS"]["detail"])

    def test_unknown_keys_are_typos_not_silence(self):
        for key in ("rss_max_mib", "provisional", "disk_free_min", "mem_available_slope"):
            ds = build_full()
            ds.config["resource_bounds"] = dict(BOUNDS, **{key: 1})
            checks, _verdict = judge(ds)
            self.assertEqual(table(checks)["FLOORS"]["result"], "FAIL", key)
            self.assertIn("unknown key '%s'" % key, table(checks)["FLOORS"]["detail"])

    def test_bad_bound_values_are_floors_failures(self):
        for value in (-1, "100", True, [100]):
            ds = build_full()
            ds.config["resource_bounds"] = dict(BOUNDS, rss_slope_mib_per_h_max=value)
            checks, _verdict = judge(ds)
            self.assertEqual(table(checks)["FLOORS"]["result"], "FAIL", value)
        ds = build_full()
        ds.config["resource_bounds"] = dict(BOUNDS, min_points=0)
        self.assertEqual(table(judge(ds)[0])["FLOORS"]["result"], "FAIL")
        ds = build_full()
        ds.config["resource_bounds"] = [1, 2]
        self.assertEqual(table(judge(ds)[0])["FLOORS"]["result"], "FAIL")

    def test_the_smoke_profile_may_use_other_numbers_and_null(self):
        ds = build_smoke(avail_mib=100000.0)
        set_linear(ds, "node_rss_kb", 300.0, 150.0, 1024.0)
        checks, verdict = judge(ds)
        self.assertEqual(table(checks)["RES-RSS"]["result"], "FAIL")
        ds.config["resource_bounds"] = dict(BOUNDS, min_points=31, rss_slope_mib_per_h_max=500)
        checks, verdict = judge(ds)
        self.assertEqual(table(checks)["FLOORS"]["result"], "INFO")
        self.assertEqual(table(checks)["RES-RSS"]["result"], "PASS")
        self.assertEqual(verdict["resource_bounds"]["differs_from_adopted"], ["rss_slope_mib_per_h_max", "min_points"])
        ds.config["resource_bounds"] = dict(BOUNDS, min_points=31, rss_slope_mib_per_h_max=None)
        checks, verdict = judge(ds)
        self.assertEqual(table(checks)["RES-RSS"]["result"], "INFO")
        self.assertIn("no bound configured (informational)", table(checks)["RES-RSS"]["detail"])
        self.assertEqual(verdict["verdict"], "SMOKE_FAIL")                # an informational bound cannot pass a criterion
        self.assertIn("RES-RSS", verdict["unmet_ids"])

    def test_the_smoke_dataset_passes_with_the_adopted_numbers(self):
        checks, verdict = judge(build_smoke())
        self.assertEqual(fails(checks), [])
        self.assertEqual(verdict["verdict"], "SMOKE_PASS")

    def test_bounds_override_argument(self):
        ds = build_supp()
        set_linear(ds, "node_rss_kb", 300.0, 0.02, 1024.0)
        checks, verdict = judge(ds, {"rss_slope_mib_per_h_max": 0.01})
        self.assertEqual(table(checks)["RES-RSS"]["result"], "FAIL")
        self.assertEqual(verdict["resource_bounds"]["source"], "config + --bounds-file")
        checks, verdict = judge(ds, {"rss_slope_mib_per_h_max": 500})      # looser: floors failure, adopted number judged
        self.assertEqual(table(checks)["FLOORS"]["result"], "FAIL")
        self.assertIn("--bounds-file rss_slope_mib_per_h_max=500 is looser than the adopted ARC-83 number 100",
                      table(checks)["FLOORS"]["detail"])
        self.assertEqual(table(checks)["RES-RSS"]["result"], "PASS")

    def test_min_points_from_the_config_is_used(self):
        ds = build_supp()
        self.assertEqual(E.Ctx(E.evidence_from_texts(to_texts(ds))).min_points(), 136)
        ds = build_full()
        self.assertEqual(E.Ctx(E.evidence_from_texts(to_texts(ds))).min_points(), 121)


# ------------------------------------------------------------------------------------------------------------------
# independent recomputation (the reviewer's audit formulas, stdlib only, written separately from the evaluator)
# ------------------------------------------------------------------------------------------------------------------

def audit_slope(points):
    """ARC-83 audit formula: sum((t-mean(t))*(y-mean(y))) / sum((t-mean(t))^2), t in hours, y in MiB."""
    count = len(points)
    mean_t = sum(t for t, _y in points) / count
    mean_y = sum(y for _t, y in points) / count
    return (sum((t - mean_t) * (y - mean_y) for t, y in points)
            / sum((t - mean_t) ** 2 for t, _y in points))


def audit_points(samples, field):
    origin = samples[0]["epoch"]
    return [((s["epoch"] - origin) / 3600.0, s[field] / 1024.0) for s in samples]


def jitter_epochs(ds):
    """Move every bridged sample by up to +-2 s (deterministic) so the time axis is not a perfect grid."""
    for index, sample in enumerate(bridged_of(ds)):
        shift_sample(sample, 2.0 * math.sin(index * 1.7))


class TestIndependentRecomputation(Base):
    def test_rss_slope_matches_the_audit_formula(self):
        ds = build_supp(avail_mib=100000.0)
        jitter_epochs(ds)
        for sample in bridged_of(ds):
            hours = (sample["epoch"] - ds.t0) / 3600.0
            sample["node_rss_kb"] = int(round((300.0 + 12.5 * hours + 6.0 * math.sin(hours * 9.0)) * 1024.0))
        steady = steady_of(ds)
        expected = audit_slope(audit_points(steady, "node_rss_kb"))
        checks, _verdict = judge(ds)
        self.assertEqual(fails(checks), [])
        self.assertIn("RSS slope %+.3f MiB/h over %d point(s)" % (expected, len(steady)), table(checks)["RES-RSS"]["detail"])
        # the evaluator's own function agrees to rounding error
        origin = steady[0]["epoch"]
        points = [((s["epoch"] - origin) / 3600.0, s["node_rss_kb"] / 1024.0) for s in steady]
        self.assertAlmostEqual(E.ls_slope(points), expected, delta=1e-9 * max(1.0, abs(expected)))
        self.assertGreater(expected, 5.0)                      # a real trend, not a degenerate case

    def test_ls_slope_matches_the_audit_formula_on_many_series(self):
        import random
        rng = random.Random(20261008)
        for _case in range(60):
            count = rng.randint(3, 220)
            hours, moment = [], 0.0
            for _ in range(count):
                moment += rng.uniform(50.0, 70.0) / 3600.0
                hours.append(moment + 1_790_000_000.0 / 3600.0 * rng.choice((0, 0, 1)) * 0)     # offsets vanish by centring
            points = [(t, rng.uniform(-500.0, 5000.0) + 20.0 * t) for t in hours]
            expected = audit_slope(points)
            self.assertAlmostEqual(E.ls_slope(points), expected, delta=1e-8 * max(1.0, abs(expected)))
        self.assertIsNone(E.ls_slope([]))
        self.assertIsNone(E.ls_slope([(1.0, 2.0)]))
        self.assertIsNone(E.ls_slope([(1.0, 2.0), (1.0, 5.0)]))                  # no spread in time

    def test_ls_slope_is_stable_with_epoch_sized_inputs(self):
        base = 1_790_000_000.0
        samples = [{"epoch": base + 60.0 * k, "seq": k, "node_rss_kb": int((300.0 + 7.0 * (60.0 * k / 3600.0)) * 1024)}
                   for k in range(200)]
        rows = E.series(samples, "node_rss_kb", 1.0 / 1024.0)
        self.assertAlmostEqual(E.slope_of(rows), 7.0, delta=0.01)
        self.assertEqual(rows[0][0], 0.0)

    def test_disk_runway_matches_the_audit_formula(self):
        ds = build_supp(disk_total=20 * GIB, disk_free=9 * GIB)
        jitter_epochs(ds)
        for sample in bridged_of(ds):
            hours = (sample["epoch"] - ds.t0) / 3600.0
            free_mib = 7000.0 - 30.0 * hours + 4.0 * math.sin(hours * 5.0)
            sample["disk_free_b"] = int(round(free_mib * MIB))
        steady = steady_of(ds)
        slope = audit_slope([((s["epoch"] - steady[0]["epoch"]) / 3600.0, s["disk_free_b"] / float(MIB)) for s in steady])
        reserve = max(2147483648.0, 0.20 * steady[-1]["disk_total_b"])
        runway = ((steady[-1]["disk_free_b"] - reserve) / MIB) / (-slope)
        self.assertLess(slope, 0.0)
        self.assertGreater(runway, 72.0)
        checks, _verdict = judge(ds)
        self.assertEqual(fails(checks), [])
        import re
        found = re.search(r"free-disk slope ([+-][0-9.]+) MiB/h over \d+ point\(s\), runway ([0-9.]+) h", table(checks)["RES-DISK"]["detail"])
        self.assertIsNotNone(found)
        self.assertEqual(found.group(1), "%+.3f" % slope)
        self.assertAlmostEqual(float(found.group(2)), runway, delta=0.051)

    def test_disk_runway_failure_agrees_with_the_audit_formula(self):
        ds = build_supp(disk_total=20 * GIB, disk_free=6 * GIB)
        jitter_epochs(ds)
        for sample in bridged_of(ds):
            hours = (sample["epoch"] - ds.t0) / 3600.0
            sample["disk_free_b"] = int(round((6000.0 - 35.0 * hours + 3.0 * math.sin(hours * 4.0)) * MIB))
        steady = steady_of(ds)
        slope = audit_slope([((s["epoch"] - steady[0]["epoch"]) / 3600.0, s["disk_free_b"] / float(MIB)) for s in steady])
        runway = ((steady[-1]["disk_free_b"] - 4 * GIB) / MIB) / (-slope)
        self.assertLess(runway, 72.0)
        checks, _verdict = self.assertFailsOnly(ds, ["RES-DISK"])
        import re
        found = re.search(r"runway ([0-9.]+) h < 72 h", table(checks)["RES-DISK"]["detail"])
        self.assertIsNotNone(found)
        self.assertAlmostEqual(float(found.group(1)), runway, delta=0.051)

    def test_projection_matches_the_audit_formula(self):
        ds = build_supp(avail_mib=1500.0)
        jitter_epochs(ds)
        for sample in bridged_of(ds):
            hours = (sample["epoch"] - ds.t0) / 3600.0
            sample["node_rss_kb"] = int(round((300.0 + 15.0 * hours + 2.0 * math.sin(hours * 7.0)) * 1024.0))
            sample["mem_available_kb"] = int(round(1500.0 * 1024.0))
        steady = steady_of(ds)
        slope = audit_slope(audit_points(steady, "node_rss_kb"))
        growth = 24.0 * max(slope, 0.0)
        limit = 0.5 * (steady[0]["mem_available_kb"] / 1024.0 - 512.0)
        checks, _verdict = judge(ds)
        self.assertEqual(table(checks)["RES-PROJECTION"]["result"], "PASS" if growth <= limit else "FAIL")
        self.assertIn("24h RSS growth %.1f MiB (slope %+.3f MiB/h" % (growth, slope), table(checks)["RES-PROJECTION"]["detail"])
        self.assertIn("= %.1f MiB (first steady MemAvailable" % limit, table(checks)["RES-PROJECTION"]["detail"])

    def test_the_slope_uses_the_samples_time_not_their_order(self):
        ds = build_supp(avail_mib=100000.0)
        steady = steady_of(ds)
        # accelerate only the time axis: same RSS values, samples closer together after the half-way point
        for index, sample in enumerate(steady):
            sample["node_rss_kb"] = int((300.0 + index) * 1024)          # +1 MiB per sample
        checks, _verdict = judge(ds)
        # one MiB per 60 s sample is 60 MiB/h: the evaluator must report hours, not samples
        self.assertIn("RSS slope +60.0", table(checks)["RES-RSS"]["detail"])


# ------------------------------------------------------------------------------------------------------------------
# command line
# ------------------------------------------------------------------------------------------------------------------

def write_evidence(ds, directory):
    for name, text in to_texts(ds).items():
        with open(os.path.join(directory, name), "w", encoding="utf-8") as handle:
            handle.write(text)


def run_main(argv):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = E.main(argv)
    return code, out.getvalue(), err.getvalue()


def read_json(path):
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


class TestCommandLine(Base):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def evidence(self, ds, name="evidence"):
        directory = os.path.join(self.tmp.name, name)
        os.makedirs(directory)
        write_evidence(ds, directory)
        return directory

    def test_a_supplement_run_end_to_end(self):
        directory = self.evidence(build_supp())
        out = os.path.join(self.tmp.name, "out")
        code, stdout, stderr = run_main(["--evidence", directory, "--out", out])
        self.assertEqual(code, 0, stdout + stderr)
        self.assertIn("VERDICT: SUPPLEMENT_PASS (profile resources, launcher_source published)", stdout)
        self.assertIn("SUPPLEMENT: combined evidence with Wave 0 run 37750760170", stdout)
        verdict = read_json(os.path.join(out, "verdict.json"))
        checks = read_json(os.path.join(out, "checks.json"))
        self.assertEqual(verdict["verdict"], "SUPPLEMENT_PASS")
        self.assertEqual([c["id"] for c in checks][:25], list(E.COMPUTED_IDS))
        self.assertEqual([c["id"] for c in checks][25:41], list(E.EXTRA_IDS))
        with open(os.path.join(out, "REPORT.md"), encoding="utf-8") as handle:
            report = handle.read()
        self.assertIn("**Verdict: SUPPLEMENT_PASS**", report)
        self.assertIn("## Resource bounds", report)
        for name in ("scoreboard.jsonl", "heartbeats.jsonl", "kernel-oom.json", "binding.json"):
            self.assertTrue(os.path.exists(os.path.join(directory, name)), name)
        self.assertFalse(os.path.exists(os.path.join(directory, "verdict.json")))     # --out was given

    def test_a_failing_supplement_exits_1(self):
        ds = build_supp()
        ds.oom = None
        directory = self.evidence(ds)
        code, stdout, _stderr = run_main(["--evidence", directory])
        self.assertEqual(code, 1)
        self.assertIn("VERDICT: SUPPLEMENT_FAIL", stdout)
        self.assertEqual(read_json(os.path.join(directory, "verdict.json"))["failed_ids"], ["RES-OOM"])

    def test_a_full_run_with_the_new_contract_exits_0(self):
        directory = self.evidence(build_full())
        code, stdout, _stderr = run_main(["--evidence", directory])
        self.assertEqual(code, 0)
        self.assertIn("VERDICT: WAVE0_PASS", stdout)

    def test_older_evidence_without_the_optional_files(self):
        directory = self.evidence(T.build())
        code, stdout, _stderr = run_main(["--evidence", directory])
        self.assertEqual(code, 0)
        verdict = read_json(os.path.join(directory, "verdict.json"))
        self.assertEqual(verdict["verdict"], "WAVE0_PASS")
        self.assertEqual(len(verdict["not_applicable"]), 16)
        self.assertIn("NOT JUDGED", stdout)

    def test_bounds_file_overrides_the_effective_bounds(self):
        ds = build_supp()
        set_linear(ds, "node_rss_kb", 300.0, 0.02, 1024.0)
        directory = self.evidence(ds)
        bounds = os.path.join(self.tmp.name, "bounds.json")
        with open(bounds, "w") as handle:
            json.dump({"rss_slope_mib_per_h_max": 0.01}, handle)
        code, stdout, _stderr = run_main(["--evidence", directory, "--bounds-file", bounds])
        self.assertEqual(code, 1)
        verdict = read_json(os.path.join(directory, "verdict.json"))
        self.assertEqual(verdict["failed_ids"], ["RES-RSS"])
        self.assertEqual(verdict["resource_bounds"]["source"], "config + --bounds-file")
        self.assertEqual(verdict["resource_bounds"]["values"]["rss_slope_mib_per_h_max"], 0.01)
        rows = dict((c["id"], c) for c in read_json(os.path.join(directory, "checks.json")))
        self.assertIn("above the bound of 0.01 MiB/h", rows["RES-RSS"]["detail"])
        self.assertIn("FAIL RES-RSS", stdout)
        code, _stdout, _stderr = run_main(["--evidence", directory])              # without the file: the adopted 100
        self.assertEqual(code, 0)

    def test_a_looser_bounds_file_cannot_make_a_full_run_pass_looser(self):
        ds = build_full(avail_mib=100000.0)
        set_linear(ds, "node_rss_kb", 300.0, 150.0, 1024.0)
        directory = self.evidence(ds)
        bounds = os.path.join(self.tmp.name, "bounds.json")
        with open(bounds, "w") as handle:
            json.dump({"rss_slope_mib_per_h_max": 500}, handle)
        code, stdout, _stderr = run_main(["--evidence", directory, "--bounds-file", bounds])
        self.assertEqual(code, 1)
        self.assertIn("--bounds-file rss_slope_mib_per_h_max=500 is looser than the adopted ARC-83 number 100", stdout)

    def test_a_bad_bounds_file_exits_2_and_writes_nothing(self):
        directory = self.evidence(build_supp())
        bad_files = {"invalid.json": "{not json", "array.json": "[1, 2]", "unknown.json": '{"rss_max_mib": 5}',
                     "negative.json": '{"rss_slope_mib_per_h_max": -3}', "string.json": '{"min_points": "121"}'}
        for name, text in bad_files.items():
            path = os.path.join(self.tmp.name, name)
            with open(path, "w") as handle:
                handle.write(text)
            code, stdout, stderr = run_main(["--evidence", directory, "--bounds-file", path])
            self.assertEqual(code, 2, name)
            self.assertEqual(stdout, "", name)
            self.assertIn("--bounds-file", stderr, name)
            self.assertFalse(os.path.exists(os.path.join(directory, "verdict.json")), name)
        code, _stdout, stderr = run_main(["--evidence", directory, "--bounds-file", os.path.join(self.tmp.name, "absent.json")])
        self.assertEqual(code, 2)
        self.assertIn("missing", stderr)

    def test_a_missing_evidence_directory_does_not_crash(self):
        code, stdout, _stderr = run_main(["--evidence", os.path.join(self.tmp.name, "nowhere"), "--out",
                                          os.path.join(self.tmp.name, "out2")])
        self.assertEqual(code, 1)
        self.assertIn("WAVE0_FAIL", stdout)

    def test_two_runs_write_identical_files(self):
        directory = self.evidence(build_supp())
        first, second = os.path.join(self.tmp.name, "o1"), os.path.join(self.tmp.name, "o2")
        run_main(["--evidence", directory, "--out", first])
        run_main(["--evidence", directory, "--out", second])
        for name in ("checks.json", "verdict.json", "REPORT.md"):
            with open(os.path.join(first, name), "rb") as a, open(os.path.join(second, name), "rb") as b:
                self.assertEqual(a.read(), b.read(), name)

    def test_the_evaluator_still_reads_no_clock_and_spawns_nothing(self):
        source = read_file(os.path.join(os.path.dirname(HERE), "evaluate_stage_b.py"))
        for pattern in (r"\btime\.time\b", r"\btime\.monotonic\b", r"\btime\.perf_counter\b", r"datetime\.(now|utcnow|today)\b",
                        r"\bsubprocess\b", r"\bsocket\b", r"\burllib\b", r"\brequests\b", r"\bos\.(system|popen)\b",
                        r"\bimport random\b", r"\bimport time\b", r"\bimport shutil\b"):
            self.assertIsNone(re.search(pattern, source), pattern)


def read_file(path):
    with open(path, "r", encoding="utf-8") as handle:
        return handle.read()


if __name__ == "__main__":
    unittest.main(verbosity=1)
