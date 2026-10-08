#!/usr/bin/env python3
"""Wave 0 Stage B evaluator: judge a soak from raw evidence.

Usage:
    python3 wave0-lab/evaluate_stage_b.py --evidence <dir> [--out <dir>] [--bounds-file <file>]

Reads (all optional on disk, but a missing or unparseable file FAILS every check that needs it):
    config-effective.json   the effective lab config of the run
    samples.jsonl           one guest sample per interval (survives the guest reboot)
    events.jsonl            host orchestration events (forced / not forced, t0, reboot, steady, final stop)
    commands.jsonl          every ssh command the orchestrator ran (proves no manual node start after the reboot)
    invariants-before.json  invariants collected before the first consume
    invariants-after.json   invariants collected at the end of the soak
    checks-live.jsonl       point-in-time checks the orchestrator already made (merged verbatim)
  and, for the ARC-83 criteria D and E (see below), optionally:
    scoreboard.jsonl        public scoreboard rounds read by the runner (all coordinators, every <= 60 s)
    heartbeats.jsonl        the node's own registration status polled every 5 s, failed polls included
    kernel-oom.json         the guest kernel OOM scan of every boot of the soak
    binding.json            the supplement's binding to Wave 0 run 37750760170 (profile resources)

Writes checks.json, verdict.json and REPORT.md into --out (default: the evidence directory), prints a compact table
and the verdict, and exits 0 iff the verdict is WAVE0_PASS, SMOKE_PASS or SUPPLEMENT_PASS (2 if the outputs cannot be
written or the --bounds-file is unusable).

Options:
    --bounds-file FILE      JSON object in the shape of config "resource_bounds"; its keys are laid over the effective
                            bounds so the same raw samples can be re-judged offline with other numbers. Under the full
                            and resources profiles a looser number than the adopted one is a FLOORS failure and the
                            adopted number is judged anyway (the same rule as Astra's floors); tighter numbers apply.

ARC-83 criteria D and E (reviewer text and Astra's decision of 2026-10-08T13:19Z). These checks are judged only when
config-effective.json says "resources_required": true / "scoreboard_required": true (or profile "resources", which
switches both on). Evidence that predates that contract (no such key) is judged EXACTLY as before: the checks are not
emitted, verdict.json lists them under "not_applicable" with the reason, the statement says what was NOT JUDGED, and the
verdict, counts and checks.json rows do not change. A key set to false emits SKIP rows. Every number is read from the
evidence; nothing is guessed. Coverage that is missing is UNPROVED and FAILS, it is never a PASS.

Resource formulas (every slope is least squares, sum((t-mean t)(y-mean y)) / sum((t-mean t)^2), t in hours, y in MiB):
    RES-RSS         slope of node_rss_kb/1024 over the steady window; FAIL if > rss_slope_mib_per_h_max (growth only)
    RES-MEMAVAIL    minimum of mem_available_kb/1024 over the steady window; FAIL if < mem_available_min_mib
    RES-PROJECTION  FAIL if projection_hours * max(slope, 0) > projection_fraction * headroom, where the conservative
                    headroom = first steady MemAvailable MiB - mem_available_min_mib (the plain variant is printed)
    RES-DISK        R = max(disk_reserve_floor_b, disk_reserve_fraction * disk_total_b); FAIL if any bridged sample has
                    disk_free_b <= R, or if the free-disk slope is negative and (last free - R) / |slope| < disk_runway_h_min
    RES-DOWNLOADS   structural FAILs regardless of slopes (release cache changed, a file entered the cache, partial files,
                    model files, a rising download counter, a model-sized file) plus the runway rule extended to log + node
                    data growth
    RES-OOM         kernel-oom.json: any OOM line in any boot FAILS; an empty scan, a missing boot or a wrong total is UNPROVED
    VM-MEMORY       the vm_memory event says the soak VM had 4096 MB and the guest's own MemTotal fits that
    RES-SWAP RES-DATA RES-HANDLES RES-CGROUP   INFO series lines (no threshold was adopted), they never fail
    BINDING-PRIOR-RUN  profile resources only: binding.json prior_run vs this_run, equality recomputed here (five sha256
                    fields, the three unit digests, the baseline line), run id 37750760170, VM memory 6144 -> 4096 MiB
                    disclosed, and this_run equal to the bytes the config pinned and the node that ran
Freshness (ARC-83 D):
    FRESH-AGE       every steady-window sample has last_registration_unix_ms (int) and registration_age_s <= 90 s,
                    consistent with epoch - last_registration_unix_ms/1000 within 1.5 s
    FRESH-DISTINCT  heartbeats.jsonl: poll coverage (first/last poll within 10 s of the window, no poll gap above 10 s), the
                    distinct timestamps strictly increase and are <= 90 s apart, the boundaries are bridged, and there are
                    at least 0.5 x window / 15 s of them (a frozen timestamp is a FAIL)
    PUBLIC-FRESHNESS  scoreboard.jsonl: rounds <= 60 s apart covering the steady window, every round lists the node on at
                    least one coordinator under node-<first 8 hex> and worker_id 0x<address> wherever it is served

Design rules:
  * Pure and deterministic: the evaluator reads no clock, no network and spawns nothing. Every number comes from the
    evidence. The same evidence yields byte-identical output files.
  * Astra's ARC-83 numbers are FLOORS under the "full" profile. If the config file lowers one, the check FLOORS fails
    and the evaluator keeps judging against the floor. A "smoke" profile can never yield WAVE0_PASS.
  * Bad input never crashes the evaluator. A missing file, an unparseable file, a line that is not a JSON object, or a
    record with a missing / wrongly typed required field is counted; every check that reads that source FAILS and says
    how many records were affected (and which lines).
  * Sample `errors` entries make a steady-state sample fail, except entries that start with "info:" which the
    producer may use for harmless remarks.
  * Time comes from `guest_epoch` when an event has one, otherwise from `host_epoch`. Samples use the guest clock;
    scoreboard rounds use the runner's clock and are placed with the host_epoch of the steady_begin / steady_end events.
  * A "bridged sample" is one whose node_exe contains /legacy-bridge/releases/ (the v0.8 node the launcher exec'ed).

Check ids computed here (live checks from the orchestrator are appended verbatim after them):
    EVIDENCE-FILES FLOORS EVENTS-ORDER SAMPLES-TOTAL SAMPLES-STEADY SAMPLE-GAPS STEADY-DURATION TOTAL-DURATION
    STEADY-UNINTERRUPTED HEALTH-BRIDGED IDENTITY-STABLE STAKE-ZERO COMPUTE-OFF ONE-NODE LEGACY-UNCHANGED-SAMPLES
    INVARIANTS-BEFORE-AFTER PRIVACY-SAFE-NAME REGISTRATION-LIVE REBOOT-BOOT-ID REBOOT-AUTO-RECOVERY
    REBOOT-UPDATER-TIMER REBOOT-HEALTHY-WINDOW REBOOT-SAME-LAUNCHER LIVE-REQUIRED LIVE-RESULTS
  and, between LIVE-RESULTS and the live checks, when judged (see above):
    RESOURCES-PRESENT RES-RSS RES-MEMAVAIL RES-PROJECTION RES-DISK RES-SWAP RES-DATA RES-HANDLES RES-CGROUP
    RES-DOWNLOADS RES-OOM VM-MEMORY BINDING-PRIOR-RUN FRESH-AGE FRESH-DISTINCT PUBLIC-FRESHNESS

Profile "resources" (config-only switch, battery false) is the SUPPLEMENT: no interruption, updater runs, kickstarts or
reboot. It needs a steady window of >= 8100 s measured first sample to last sample (135 minutes) after a settle of >= 300 s,
>= 136 samples, no gap between adjacent samples above 65 s, >= 8700 s from t0 and a sample interval <= 60 s. The REBOOT-*
checks and the battery live checks become SKIP rows ("covered by Wave 0 run 37750760170"); the steady window starts at
steady_begin (= t0 + settle_s). Verdicts SUPPLEMENT_PASS / SUPPLEMENT_FAIL, never WAVE0_PASS: it is combined evidence with
run 37750760170, not a re-instrumentation of it.
"""

from __future__ import annotations

import argparse
import datetime
import email.utils
import json
import math
import os
import re
import sys

SCHEMA_CONFIG = "arc.legacy-bridge.wave0-lab.stage-b-config.v1"
SCHEMA_INVARIANTS = "arc.legacy-bridge.wave0-lab.invariants.v1"
SCHEMA_VERDICT = "arc.legacy-bridge.wave0-lab.stage-b-verdict.v1"

FILES = (
    ("config", "config-effective.json"),
    ("samples", "samples.jsonl"),
    ("events", "events.jsonl"),
    ("commands", "commands.jsonl"),
    ("inv_before", "invariants-before.json"),
    ("inv_after", "invariants-after.json"),
    ("live", "checks-live.jsonl"),
)
JSON_KEYS = ("config", "inv_before", "inv_after")
# Optional evidence files (absent in older evidence). Kept apart from FILES: the seven mandatory files and the
# EVIDENCE-FILES message do not change.
OPTIONAL_FILES = (
    ("scoreboard", "scoreboard.jsonl"),
    ("heartbeats", "heartbeats.jsonl"),
    ("oom", "kernel-oom.json"),
    ("binding", "binding.json"),
)
OPTIONAL_JSON_KEYS = ("oom", "binding")
SCHEMA_OOM = "arc.legacy-bridge.wave0-lab.kernel-oom.v1"

# Astra's numbers. Under the "full" profile the config file may not go below these.
FLOOR_MIN = {
    "min_total_s": 14400,
    "min_steady_s": 7200,
    "min_steady_samples": 121,
    "post_reboot_healthy_s": 600,
    "updater_runs": 2,
    "kickstarts": 3,
}
FLOOR_INTERVAL_MAX = 60
DEFAULTS = {
    "sample_interval_s": 60,
    "min_total_s": 14400,
    "min_steady_s": 7200,
    "min_steady_samples": 121,
    "post_reboot_healthy_s": 600,
    "max_gap_factor": 2.5,
    "reboot_recovery_deadline_s": 300,
    "forced_grace_s": 90,
    "updater_runs": 2,
    "kickstarts": 3,
}

PROFILES = ("full", "smoke", "resources")
# The supplement ("resources" profile), Astra's ARC-83 decision of 2026-10-08T13:19Z: a 135-minute steady window measured
# first to last sample, >= 136 samples, no adjacent-sample gap above 65 s. The config file may not go below these.
RESOURCE_FLOOR_MIN = {"min_total_s": 8700, "min_steady_s": 8100, "min_steady_samples": 136}
RESOURCE_GAP_MAX_S = 65.0
RESOURCE_GAP_SLACK_S = 0.01
SETTLE_FLOOR_S = 300
COVERED_BY_RUN = "covered by Wave 0 run 37750760170 (battery and reboot not repeated in the supplement)"
BATTERY_ONLY_IDS = ("REBOOT-BOOT-ID", "REBOOT-AUTO-RECOVERY", "REBOOT-UPDATER-TIMER", "REBOOT-HEALTHY-WINDOW",
                    "REBOOT-SAME-LAUNCHER")

REQUIRED_LIVE_IDS = (
    "L01-kvm",
    "L02-consume-dry-run",
    "L03-interrupt-took-effect",
    "L04-interrupt-resumes",
    "L05-updater-1-noop",
    "L06-updater-2-noop",
    "L07-kickstart-1",
    "L08-kickstart-2",
    "L09-kickstart-3",
    "L10-reboot-boot-id-changed",
    "L11-stop-rollback",
    "L12-pre-post-boot-logs",
)

SUPPLEMENT_LIVE_IDS = ("L01-kvm", "L02-consume-dry-run", "L11-stop-rollback")
BATTERY_LIVE_IDS = tuple(i for i in REQUIRED_LIVE_IDS if i not in SUPPLEMENT_LIVE_IDS)

UNIT_FILES = ("arc-node.service", "arc-updater.service", "arc-updater.timer")
FORBIDDEN_ARGV = ("--model", "--validator-seed", "--insecure-dev-validator-seed", "--shard-range", "--no-community")
BRIDGED_MARK = "/legacy-bridge/releases/"
PUBLIC_NAME_RE = re.compile(r"node-[0-9a-f]{8}")
MANUAL_START_RE = re.compile(
    r"systemctl\s+(start|restart|try-restart|reload-or-restart|kickstart)\b[^;&|\n]*arc-node(\.service)?\b"
)
FORBIDDEN_COMMAND_WORDS = ("canary-consume", "kickstart", "legacy-bridge-rollback")
FAMILY_RE = {
    "updater_run": re.compile(r"^updater_run_(\d+)$"),
    "kickstart": re.compile(r"^kickstart_(\d+)$"),
}
FORCED_NAMES = ("apply1", "apply2", "reboot_issued", "final_stop_begin")
UNFORCED_NAMES = ("t0_bridged_healthy", "reboot_recovered", "steady_begin", "steady_end")

EPS = 1e-6          # float slack for window membership
ORDER_EPS = 1.0     # slack for "A before B" between events (guest_epoch may have one-second resolution)
FIRST_HEALTHY_EPS = 1.5

COMPUTED_IDS = (
    "EVIDENCE-FILES",
    "FLOORS",
    "EVENTS-ORDER",
    "SAMPLES-TOTAL",
    "SAMPLES-STEADY",
    "SAMPLE-GAPS",
    "STEADY-DURATION",
    "TOTAL-DURATION",
    "STEADY-UNINTERRUPTED",
    "HEALTH-BRIDGED",
    "IDENTITY-STABLE",
    "STAKE-ZERO",
    "COMPUTE-OFF",
    "ONE-NODE",
    "LEGACY-UNCHANGED-SAMPLES",
    "INVARIANTS-BEFORE-AFTER",
    "PRIVACY-SAFE-NAME",
    "REGISTRATION-LIVE",
    "REBOOT-BOOT-ID",
    "REBOOT-AUTO-RECOVERY",
    "REBOOT-UPDATER-TIMER",
    "REBOOT-HEALTHY-WINDOW",
    "REBOOT-SAME-LAUNCHER",
    "LIVE-REQUIRED",
    "LIVE-RESULTS",
)
# Checks of ARC-83 criteria D and E. They are registered separately (EXTRA_CHECKS) and judged only when the config
# asks for them; the 25 classic ids above keep their meaning and order for all older evidence.
EXTRA_IDS = (
    "RESOURCES-PRESENT",
    "RES-RSS",
    "RES-MEMAVAIL",
    "RES-PROJECTION",
    "RES-DISK",
    "RES-SWAP",
    "RES-DATA",
    "RES-HANDLES",
    "RES-CGROUP",
    "RES-DOWNLOADS",
    "RES-OOM",
    "VM-MEMORY",
    "BINDING-PRIOR-RUN",
    "FRESH-AGE",
    "FRESH-DISTINCT",
    "PUBLIC-FRESHNESS",
)
EXTRA_GROUP = {check_id: "resources" for check_id in EXTRA_IDS}
for _fresh in ("FRESH-AGE", "FRESH-DISTINCT", "PUBLIC-FRESHNESS"):
    EXTRA_GROUP[_fresh] = "freshness"
# Extras that only one profile judges; every other profile gets a SKIP row for them (when it judges the group at all).
PROFILE_ONLY = {"BINDING-PRIOR-RUN": "resources"}
# Ids whose result may legitimately be INFO without blocking a PASS verdict. The four RES-* series checks only print
# numbers (no swap, data, handle or cgroup threshold was adopted) and never fail.
INFO_OK_IDS = ("FLOORS", "REGISTRATION-LIVE", "RES-SWAP", "RES-DATA", "RES-HANDLES", "RES-CGROUP")

# Adopted resource bounds (ARC-83 independent review, criterion E). Only these keys exist; anything else is a typo.
# An absent key means the adopted number; under the full and resources profiles a looser value (or null) is a FLOORS
# failure and the adopted number is judged instead.
ADOPTED_BOUNDS = {
    "rss_slope_mib_per_h_max": 100.0,
    "mem_available_min_mib": 512.0,
    "projection_fraction": 0.5,
    "projection_hours": 24.0,
    "disk_reserve_floor_b": 2147483648.0,
    "disk_reserve_fraction": 0.20,
    "disk_runway_h_min": 72.0,
    "min_points": 121,
}
BOUND_ORDER = ("rss_slope_mib_per_h_max", "mem_available_min_mib", "projection_fraction", "projection_hours",
               "disk_reserve_floor_b", "disk_reserve_fraction", "disk_runway_h_min", "min_points")
BOUND_KEYS = BOUND_ORDER
BOUND_UPPER = ("rss_slope_mib_per_h_max", "projection_fraction")   # a larger value is the looser one; for the rest smaller is
# Fields that must be present in >= 99 % of the steady-window samples (RESOURCES-PRESENT): the quantities criterion E
# says to record (RSS, available memory, swap, data/cache/log sizes, free disk and the capacity that sets the reserve).
RES_PRESENT_FIELDS = ("node_rss_kb", "mem_available_kb", "swap_total_kb", "swap_free_kb", "node_data_bytes",
                      "release_cache_bytes", "log_bytes", "disk_free_b", "disk_total_b")
RES_PRESENT_MIN_FRACTION = 0.99
MIB = 1024.0 * 1024.0
MODEL_SIZED_BYTES = 100 * 1024 * 1024    # the pinned release assets are 28 MiB (node) and 4 MiB (cli); a model is gigabytes
CANARY_VM_MB = 4096
VM_MEMTOTAL_MIN_FRACTION = 0.80          # the guest's MemTotal of a 4096 MB VM is a little below 4096 MiB, never above
# Local freshness (Astra, ARC-83 2026-10-08T13:19Z): the age of the last successful registration, and its distinct values.
FRESH_AGE_MAX_S = 90.0
FRESH_AGE_TOLERANCE_S = 1.5
FRESH_MS = 90000
POLL_GAP_MAX_S = 10.0
POLL_EDGE_S = 10.0
POLL_BEFORE_S = 15.0
HEARTBEAT_ROUND_S = 15.0
# The node schedules the next round 15 s AFTER the previous round completes (main.rs:9183), so the real period is 15 s
# plus the round latency (up to 20 s when a coordinator runs into its 5 s timeout). Half the nominal rate means a new
# stamp at least every 30 s on average: a frozen or sparse value still fails, a healthy but slow node does not.
DISTINCT_MIN_FRACTION = 0.5
# Public freshness: the scoreboard is read at <= 60 s intervals (the node serves a row only within the 90 s TTL).
SCOREBOARD_INTERVAL_MAX_S = 60.0
SCOREBOARD_GAP_MAX_S = 60.0
SCOREBOARD_EDGE_S = 60.0
SCOREBOARD_DATE_SKEW_S = 120.0
# Binding of the supplement to the four-hour run (Wave 0 run 37750760170)
SCHEMA_BINDING = "arc.legacy-bridge.wave0-lab.binding.v1"
PRIOR_RUN_ID = 37750760170
PRIOR_VM_MEMORY_MB = 6144
BINDING_SHA_FIELDS = ("launcher_sha256", "node_sha256", "legacy_node_sha256", "installer_sha256", "image_sha256")
SOURCE_BIND = ("last_registration_unix_ms is set by crates/arc-node/src/community_worker.rs:156-164 "
               "record_registration_round, which stores unix_ms_now() ONLY if accepted > 0 (line 160); it is called at "
               "crates/arc-node/src/main.rs:9181 once per scheduled round, and the next round is scheduled 15 s after "
               "the previous round completes (main.rs:9181-9183), so the period is 15 s plus round latency "
               "(COMMUNITY_PRESENCE_INTERVAL = 15 s, main.rs:5595; heartbeat every round, registration every 4th tick, "
               "main.rs:9103); accepted counts coordinators whose signed POST returned 2xx; source commit "
               "cd2344138b32a46eea9192cf0fa7344db6481420.")
TTL_BIND = ("rpc.rs:784 COMMUNITY_WORKER_TTL_SECS = 90; workers_scoreboard (rpc.rs:8470) serves a row only if now - "
            "last heartbeat <= TTL (rpc.rs:8514), so row present means seen <= 90 s at that read")
# Every outcome of these checks (PASS, FAIL or UNPROVED) carries its source binding verbatim (see run_check).
DETAIL_BINDINGS = {"FRESH-AGE": SOURCE_BIND, "FRESH-DISTINCT": SOURCE_BIND, "PUBLIC-FRESHNESS": TTL_BIND}


# --------------------------------------------------------------------------------------------------------------------
# small helpers
# --------------------------------------------------------------------------------------------------------------------

def is_int(value):
    return isinstance(value, int) and not isinstance(value, bool)


def is_num(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def is_str(value):
    return isinstance(value, str)


def is_hex64(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def norm_addr(value):
    if not isinstance(value, str):
        return None
    value = value.strip().lower()
    if value.startswith("0x"):
        value = value[2:]
    return value


def short_list(items, limit=6):
    items = list(items)
    if len(items) <= limit:
        return ", ".join(str(i) for i in items)
    return ", ".join(str(i) for i in items[:limit]) + ", ... (+%d more)" % (len(items) - limit)


def fmt_dur(seconds):
    if seconds is None:
        return "n/a"
    total = int(round(seconds))
    sign = "-" if total < 0 else ""
    total = abs(total)
    hours, rem = divmod(total, 3600)
    minutes, secs = divmod(rem, 60)
    return "%s%dh%02dm%02ds" % (sign, hours, minutes, secs)


def fmt_ts(value):
    if value is None:
        return "n/a"
    try:
        stamp = datetime.datetime.fromtimestamp(float(value), datetime.timezone.utc)
    except (OverflowError, OSError, ValueError):
        return str(value)
    return stamp.strftime("%Y-%m-%dT%H:%M:%SZ")


def is_bridged(sample):
    exe = sample.get("node_exe")
    return isinstance(exe, str) and BRIDGED_MARK in exe


def in_window(samples, lo, hi):
    if lo is None or hi is None:
        return []
    return [s for s in samples if lo - EPS <= s["epoch"] <= hi + EPS]


# --------------------------------------------------------------------------------------------------------------------
# parsing and validation of the evidence files
# --------------------------------------------------------------------------------------------------------------------

TYPE_CHECKS = {
    "int": is_int,
    "num": is_num,
    "str": is_str,
    "bool": lambda v: isinstance(v, bool),
    "dict": lambda v: isinstance(v, dict),
    "strlist": lambda v: isinstance(v, list) and all(isinstance(x, str) for x in v),
}

SAMPLE_REQUIRED = {
    "seq": "int",
    "epoch": "num",
    "boot_id": "str",
    "node_state": "str",
    "main_pid": "int",
    "node_procs": "int",
    "health_ok": "bool",
    "info_ok": "bool",
    "updater_timer_active": "bool",
}
SAMPLE_OPTIONAL = {
    "uptime_s": "num",
    "proc_start_epoch": "num",
    "node_exe": "str",
    "node_exe_sha256": "str",
    "n_restarts": "int",
    "chain_participation_enabled": "bool",
    "address": "str",
    "stake": "int",
    "node_version": "str",
    "bridge_node_address": "str",
    "bridge_compute": "str",
    "compute_consent": "str",
    "community_registration": "bool",
    "public_name": "str",
    "coordinators_total": "int",
    "coordinators_registered": "int",
    "version_txt": "str",
    "launcher_sha256": "str",
    "legacy_fingerprint": "str",
    "legacy_byte_compare": "str",
    "errors": "strlist",
}
# Resource and freshness fields of the ARC-83 D/E contract (all optional: older evidence has none; null = not collected).
for _field in ("node_rss_kb", "node_hwm_kb", "node_swap_kb", "node_threads", "node_fds", "node_cpu_s",
               "mem_total_kb", "mem_available_kb", "swap_total_kb", "swap_free_kb",
               "cg_mem_current_b", "cg_mem_peak_b", "cg_swap_current_b",
               "disk_total_b", "disk_free_b", "log_bytes", "bridge_downloads",
               "arc_dir_bytes", "largest_file_bytes", "legacy_data_bytes", "node_data_bytes",
               "release_cache_bytes", "release_cache_files", "models_bytes", "partial_files",
               "last_registration_unix_ms", "registration_age_s"):
    SAMPLE_OPTIONAL[_field] = "num"
EVENT_REQUIRED = {"name": "str", "forced": "bool", "host_epoch": "num"}
EVENT_OPTIONAL = {"seq": "int", "guest_epoch": "num", "detail": "dict"}
COMMAND_REQUIRED = {"host_epoch": "num", "cmd": "str"}
COMMAND_OPTIONAL = {"rc": "int"}
LIVE_REQUIRED = {"id": "str", "result": "str"}
LIVE_OPTIONAL = {"title": "str", "detail": "str"}


def validate_fields(obj, required, optional):
    for key, kind in required.items():
        if key not in obj:
            return "missing %s" % key
        if not TYPE_CHECKS[kind](obj[key]):
            return "wrong type for %s" % key
    for key, kind in optional.items():
        if key in obj and obj[key] is not None and not TYPE_CHECKS[kind](obj[key]):
            return "wrong type for %s" % key
    return None


def validate_sample(obj):
    return validate_fields(obj, SAMPLE_REQUIRED, SAMPLE_OPTIONAL)


def validate_event(obj):
    reason = validate_fields(obj, EVENT_REQUIRED, EVENT_OPTIONAL)
    if reason:
        return reason
    if not obj["name"]:
        return "empty name"
    return None


def validate_command(obj):
    return validate_fields(obj, COMMAND_REQUIRED, COMMAND_OPTIONAL)


def validate_live(obj):
    reason = validate_fields(obj, LIVE_REQUIRED, LIVE_OPTIONAL)
    if reason:
        return reason
    if not obj["id"]:
        return "empty id"
    if obj["result"] not in ("PASS", "FAIL", "INFO"):
        return "result must be PASS, FAIL or INFO"
    return None


PROBE_REQUIRED = {"probe": "int", "host_epoch": "num", "address": "str", "found_count": "int"}
PROBE_RESULT_REQUIRED = {"origin": "int", "found": "bool"}
PROBE_RESULT_OPTIONAL = {"origin_sha8": "str", "http": "int", "error": "str", "name": "str", "registered_at": "num",
                         "worker_id": "str", "server_date": "str", "count_total": "int", "row": "dict"}
HEARTBEAT_REQUIRED = {"obs_epoch": "num"}
HEARTBEAT_OPTIONAL = {"ts_ms": "int", "registered": "int", "total": "int"}


def validate_probe(obj):
    reason = validate_fields(obj, PROBE_REQUIRED, {})
    if reason:
        return reason
    results = obj.get("results")
    if not isinstance(results, list):
        return "wrong type for results"
    for item in results:
        if not isinstance(item, dict):
            return "a results entry is not an object"
        reason = validate_fields(item, PROBE_RESULT_REQUIRED, PROBE_RESULT_OPTIONAL)
        if reason:
            return "results entry: " + reason
    return None


def validate_heartbeat(obj):
    return validate_fields(obj, HEARTBEAT_REQUIRED, HEARTBEAT_OPTIONAL)


VALIDATORS = {
    "samples": validate_sample,
    "events": validate_event,
    "commands": validate_command,
    "live": validate_live,
    "scoreboard": validate_probe,
    "heartbeats": validate_heartbeat,
}


def normalize_event(record):
    """Make the optional event fields safe to use: detail is always a dict, seq always an int."""
    if not isinstance(record.get("detail"), dict):
        record["detail"] = {}
    if not is_int(record.get("seq")):
        record["seq"] = 0
    return record


NORMALIZERS = {"events": normalize_event}


def parse_jsonl(text, validator, normalizer=None):
    """Return (records, bad_json_line_numbers, [(line_number, reason)]) for malformed records."""
    records, bad, malformed = [], [], []
    for number, line in enumerate(text.split("\n"), 1):
        line = line.rstrip("\r")
        if not line.strip():
            continue
        try:
            obj = json.loads(line)
        except (ValueError, RecursionError):
            bad.append(number)
            continue
        if not isinstance(obj, dict):
            bad.append(number)
            continue
        reason = validator(obj)
        if reason:
            malformed.append((number, reason))
            continue
        record = dict(obj)
        record["_line"] = number
        if normalizer is not None:
            record = normalizer(record)
        records.append(record)
    return records, bad, malformed


def evidence_from_texts(texts):
    """Build the evidence dict from {file name: text or None}. Never raises."""
    ev = {"src": {}, "config": None, "inv_before": None, "inv_after": None,
          "samples": [], "events": [], "commands": [], "live": [], "scoreboard": [], "heartbeats": [],
          "oom": None, "binding": None}
    for key, fname in FILES + OPTIONAL_FILES:
        text = texts.get(fname)
        info = {"file": fname, "present": text is not None, "error": None, "records": 0,
                "bad_json": [], "malformed": []}
        ev["src"][key] = info
        if text is None:
            info["error"] = "missing"
            continue
        if key in JSON_KEYS or key in OPTIONAL_JSON_KEYS:
            try:
                obj = json.loads(text)
            except (ValueError, RecursionError) as exc:
                info["error"] = "is not valid JSON (%s)" % str(exc)[:80]
                continue
            if not isinstance(obj, dict):
                info["error"] = "is not a JSON object"
                continue
            ev[key] = obj
            info["records"] = 1
        else:
            records, bad, malformed = parse_jsonl(text, VALIDATORS[key], NORMALIZERS.get(key))
            ev[key] = records
            info["records"] = len(records)
            info["bad_json"] = bad
            info["malformed"] = malformed
    return ev


def read_text(path):
    try:
        with open(path, "rb") as handle:
            raw = handle.read()
    except FileNotFoundError:
        return None, "missing"
    except OSError as exc:
        return None, "is unreadable (%s)" % exc
    return raw.decode("utf-8", errors="replace"), None


def load_evidence(directory):
    """Read the evidence directory. Never raises; problems are recorded in ev['src'][key]['error']."""
    texts, errors = {}, {}
    for key, fname in FILES + OPTIONAL_FILES:
        text, error = read_text(os.path.join(directory, fname))
        texts[fname] = text
        if error and error != "missing":
            errors[key] = error
    ev = evidence_from_texts(texts)
    for key, error in errors.items():
        ev["src"][key]["error"] = error
    ev["dir"] = directory
    return ev


# --------------------------------------------------------------------------------------------------------------------
# configuration
# --------------------------------------------------------------------------------------------------------------------

def parse_bounds(raw, label):
    """Return (bounds or None, issues). A key set to null means 'informational only'; an unknown key is an issue
    (a typo must never silently switch a bound off)."""
    issues = []
    if raw is None:
        return None, issues
    if not isinstance(raw, dict):
        return None, ["%s must be a JSON object" % label]
    bounds = {}
    for key in sorted(raw):
        value = raw[key]
        if key not in BOUND_KEYS:
            issues.append("%s has an unknown key %r (the adopted keys are %s)" % (label, key, ", ".join(BOUND_KEYS)))
        elif key == "min_points":
            if is_int(value) and value > 0:
                bounds[key] = value
            else:
                issues.append("%s.min_points must be a positive integer, got %r" % (label, value))
        elif value is None:
            bounds[key] = None
        elif is_num(value) and value >= 0:
            bounds[key] = float(value)
        else:
            issues.append("%s.%s must be a non-negative number or null, got %r" % (label, key, value))
    return bounds, issues


def merge_bounds(config_bounds, config_issues, override, profile):
    """Effective bounds = the adopted numbers, then the config's resource_bounds, then the --bounds-file keys.

    Under the full and resources profiles a value looser than the adopted number (or null) is an issue and the adopted
    number is judged instead (the same rule as Astra's floors); under smoke the value is taken as given and null means
    informational only. Returns (bounds, issues, source text, keys that differ from the adopted numbers)."""
    issues = list(config_issues or [])
    given, origin, sources = {}, {}, []
    if config_bounds:
        given.update(config_bounds)
        origin.update((key, "resource_bounds.") for key in config_bounds)
        sources.append("config")
    if override:
        given.update(override)
        origin.update((key, "--bounds-file ") for key in override)
        sources.append("--bounds-file")
    bounds = dict(ADOPTED_BOUNDS)
    differs = []
    for key in BOUND_ORDER:
        if key not in given:
            continue
        value, adopted = given[key], ADOPTED_BOUNDS[key]
        if value is None:
            looser = True
        elif key in BOUND_UPPER:
            looser = value > adopted
        else:
            looser = value < adopted
        if looser and profile != "smoke":
            issues.append("%s%s=%s is looser than the adopted ARC-83 number %s (judged against the adopted number)"
                          % (origin[key], key, "null" if value is None else fmt_num(value), fmt_num(adopted)))
            continue
        bounds[key] = value
        if value != adopted:
            differs.append(key)
    source = " + ".join(sources) if sources else "adopted defaults"
    return bounds, issues, source, differs


def effective_config(raw):
    """Return (cfg, issues, lowered). Under the full and resources profiles the floors are enforced whatever the file says."""
    cfg = dict(DEFAULTS)
    issues, lowered = [], []
    document = raw if isinstance(raw, dict) else {}
    if document and document.get("schema") != SCHEMA_CONFIG:
        issues.append("config schema is %r, expected %r" % (document.get("schema"), SCHEMA_CONFIG))
    profile = document.get("profile")
    if profile not in PROFILES:
        if document:
            issues.append("profile must be 'full', 'smoke' or 'resources', got %r (judged as full)" % (profile,))
        profile = "full"
    cfg["profile"] = profile
    if profile == "resources":
        # The supplement judges a 135-minute window; its defaults are its floors, not the 4 h of the full run.
        cfg.update(RESOURCE_FLOOR_MIN)
    for key in DEFAULTS:
        if key in document:
            value = document[key]
            if is_num(value) and value > 0:
                cfg[key] = value
            else:
                issues.append("%s must be a positive number, got %r" % (key, value))
    if document:
        if document.get("launcher_source") not in ("artifact", "published"):
            issues.append("launcher_source must be 'artifact' or 'published', got %r" % (document.get("launcher_source"),))
        if document.get("live_network") not in ("allowed", "blocked"):
            issues.append("live_network must be 'allowed' or 'blocked', got %r" % (document.get("live_network"),))
        tag = document.get("tag")
        if not (isinstance(tag, str) and re.fullmatch(r"v\d+\.\d+\.\d+", tag)):
            issues.append("tag must look like v0.7.12, got %r" % (tag,))
        if not is_hex64(document.get("expected_launcher_sha256")):
            issues.append("expected_launcher_sha256 must be a lowercase sha256 hex digest")
        node_sha = document.get("node_sha256")
        if node_sha is not None and not is_hex64(node_sha):
            issues.append("node_sha256 must be a lowercase sha256 hex digest when present")

    # battery: only the supplement may run without it; a full or smoke run without the battery is not judged as one.
    cfg["battery"] = True
    battery = document.get("battery")
    if battery is not None and not isinstance(battery, bool):
        issues.append("battery must be a boolean, got %r" % (battery,))
    elif battery is False and profile != "resources":
        issues.append("battery=false is only valid with profile 'resources' (a %s run without the battery is judged "
                      "with the battery)" % profile)
    if profile == "resources":
        cfg["battery"] = False
        if battery is True:
            issues.append("profile 'resources' runs without the battery; battery=true is contradictory")

    # resource / freshness switches and numbers
    for key, name in (("resources_required", "resources_key"), ("scoreboard_required", "scoreboard_key")):
        value = document.get(key)
        if value is None:
            cfg[name] = "absent"
        elif isinstance(value, bool):
            cfg[name] = "true" if value else "false"
        else:
            issues.append("%s must be a boolean, got %r" % (key, value))
            cfg[name] = "absent"
        if profile == "resources" and cfg[name] != "true":
            if cfg[name] == "false":
                issues.append("profile 'resources' requires %s=true" % key)
            cfg[name] = "true"
    cfg["resources_required"] = cfg["resources_key"] == "true"
    cfg["scoreboard_required"] = cfg["scoreboard_key"] == "true"
    interval = document.get("scoreboard_interval_s")
    cfg["scoreboard_interval_s"] = 30.0
    if interval is not None:
        if is_num(interval) and interval > 0:
            cfg["scoreboard_interval_s"] = float(interval)
        else:
            issues.append("scoreboard_interval_s must be a positive number, got %r" % (interval,))
    settle = document.get("settle_s")
    cfg["settle_s"] = float(SETTLE_FLOOR_S) if profile == "resources" else None
    if settle is not None:
        if is_num(settle) and settle >= 0:
            cfg["settle_s"] = float(settle)
        else:
            issues.append("settle_s must be a non-negative number, got %r" % (settle,))
    vm_mb = document.get("vm_memory_mb")
    cfg["vm_memory_mb"] = vm_mb if is_num(vm_mb) else None
    if vm_mb is not None and not is_num(vm_mb):
        issues.append("vm_memory_mb must be a number, got %r" % (vm_mb,))
    cfg["resource_bounds"], bound_issues = parse_bounds(document.get("resource_bounds"), "resource_bounds")
    cfg["resource_bounds_issues"] = bound_issues

    if profile == "full":
        for key, floor in sorted(FLOOR_MIN.items()):
            if cfg[key] < floor:
                lowered.append("%s=%s is below Astra's floor %s" % (key, cfg[key], floor))
                cfg[key] = floor
        if cfg["sample_interval_s"] > FLOOR_INTERVAL_MAX:
            lowered.append("sample_interval_s=%s is above Astra's ceiling %s" % (cfg["sample_interval_s"], FLOOR_INTERVAL_MAX))
            cfg["sample_interval_s"] = FLOOR_INTERVAL_MAX
    elif profile == "resources":
        for key, floor in sorted(RESOURCE_FLOOR_MIN.items()):
            if cfg[key] < floor:
                lowered.append("%s=%s is below the supplement floor %s" % (key, cfg[key], floor))
                cfg[key] = floor
        if cfg["settle_s"] < SETTLE_FLOOR_S:
            lowered.append("settle_s=%s is below the supplement floor %s" % (cfg["settle_s"], SETTLE_FLOOR_S))
            cfg["settle_s"] = float(SETTLE_FLOOR_S)
        if cfg["sample_interval_s"] > FLOOR_INTERVAL_MAX:
            lowered.append("sample_interval_s=%s is above the ceiling %s" % (cfg["sample_interval_s"], FLOOR_INTERVAL_MAX))
            cfg["sample_interval_s"] = FLOOR_INTERVAL_MAX
    # The largest adjacent-sample gap the sampling may show. The supplement's absolute ceiling is 65 s whatever the
    # configured factor says (the config carries 1.0834 x 60 s = 65.004 s; the 0.01 s is only that rounding).
    gap_limit = float(cfg["max_gap_factor"]) * float(cfg["sample_interval_s"])
    if profile == "resources":
        if "max_gap_factor" not in document:
            gap_limit = RESOURCE_GAP_MAX_S
        elif gap_limit > RESOURCE_GAP_MAX_S + RESOURCE_GAP_SLACK_S:
            lowered.append("max_gap_factor x sample_interval_s = %.3f s is above the supplement ceiling of %s s"
                           % (gap_limit, fmt_num(RESOURCE_GAP_MAX_S)))
        gap_limit = min(gap_limit, RESOURCE_GAP_MAX_S)
    cfg["gap_limit_s"] = gap_limit
    return cfg, issues, lowered


def fmt_num(value):
    """Short, stable number text: 100, 0.5, 2147483648 (no trailing .0)."""
    if is_num(value) and float(value) == int(value):
        return str(int(value))
    return "%g" % value


def extra_mode(cfg, group):
    """'run' | 'skip' | 'omit' for the checks of one group ('resources' or 'freshness').

    omit: the config says nothing about them (evidence from before the contract): not emitted at all, so the
    checks, the counts and the verdict of older evidence are unchanged. skip: the config switches them off."""
    if cfg["profile"] == "resources":
        return "run"
    state = cfg["resources_key"] if group == "resources" else cfg["scoreboard_key"]
    return {"true": "run", "false": "skip"}.get(state, "omit")


# --------------------------------------------------------------------------------------------------------------------
# context: everything the checks need, derived once
# --------------------------------------------------------------------------------------------------------------------

def ev_time(event):
    guest = event.get("guest_epoch")
    if is_num(guest):
        return float(guest)
    return float(event["host_epoch"])


class Ctx(object):
    def __init__(self, ev, bounds_override=None):
        self.ev = ev
        raw = ev.get("config")
        self.raw_cfg = raw if isinstance(raw, dict) else {}
        self.cfg, self.cfg_issues, self.cfg_lowered = effective_config(raw)
        self.bounds, self.bounds_issues, self.bounds_source, self.bounds_differ = merge_bounds(
            self.cfg.get("resource_bounds"), self.cfg.get("resource_bounds_issues"), bounds_override,
            self.cfg["profile"])
        self._steady = None
        self.interval = float(self.cfg["sample_interval_s"])
        self.samples = sorted(ev.get("samples", []), key=lambda s: (s["epoch"], s["seq"]))
        self.events = ev.get("events", [])
        self.events_by_name = {}
        for event in self.events:
            self.events_by_name.setdefault(event["name"], []).append(event)
        self.expected_launcher = self.raw_cfg.get("expected_launcher_sha256")
        if not is_hex64(self.expected_launcher):
            self.expected_launcher = None
        tag = self.raw_cfg.get("tag")
        self.tag = tag if isinstance(tag, str) and re.fullmatch(r"v\d+\.\d+\.\d+", tag) else None
        self.expected_version = self.tag[1:] if self.tag else None
        node_sha = self.raw_cfg.get("node_sha256")
        self.node_sha = node_sha if is_hex64(node_sha) else None
        self.live_network = self.raw_cfg.get("live_network")
        self.W = self._build_windows()
        stop = self.W["final_stop"]
        self.bridged = [s for s in self.samples if is_bridged(s) and (stop is None or s["epoch"] <= stop)]
        t0_event = self.first_event("t0_bridged_healthy")
        detail = t0_event.get("detail", {}) if t0_event is not None else {}
        address = norm_addr(detail.get("address")) if t0_event is not None else None
        self.t0_address = address if is_hex64(address) else None
        fingerprint = detail.get("legacy_fingerprint") if t0_event is not None else None
        self.t0_fingerprint = fingerprint if isinstance(fingerprint, str) and fingerprint else None
        issued = self.first_event("reboot_issued")
        recovered = self.first_event("reboot_recovered")
        self.boot_before = self._detail_str(issued, "boot_id_before")
        self.boot_after = self._detail_str(recovered, "boot_id_after")
        first_healthy = recovered.get("detail", {}).get("first_healthy_sample_epoch") if recovered is not None else None
        self.claimed_first_healthy = float(first_healthy) if is_num(first_healthy) else None
        self.issued_host_epoch = float(issued["host_epoch"]) if issued is not None else None

    @staticmethod
    def _detail_str(event, key):
        if event is None:
            return None
        value = event.get("detail", {}).get(key)
        return value if isinstance(value, str) and value else None

    def first_event(self, name):
        items = self.events_by_name.get(name)
        if not items:
            return None
        return min(items, key=lambda e: (ev_time(e), e.get("seq", 0)))

    def _build_windows(self):
        window = {"problems": [], "t0": None, "final_stop": None, "reboot_issued": None, "reboot_recovered": None,
                  "steady_begin": None, "steady_end": None, "last_forced": None, "last_forced_end": None,
                  "start": None, "end": None, "steady_s": None, "total_s": None, "forced": [],
                  "first_sample": None, "last_sample": None}
        t0 = self.first_event("t0_bridged_healthy")
        stop = self.first_event("final_stop_begin")
        issued = self.first_event("reboot_issued")
        recovered = self.first_event("reboot_recovered")
        begin = self.first_event("steady_begin")
        end_event = self.first_event("steady_end")
        if t0 is not None:
            window["t0"] = ev_time(t0)
        else:
            window["problems"].append("event t0_bridged_healthy is missing")
        if stop is not None:
            window["final_stop"] = ev_time(stop)
        else:
            window["problems"].append("event final_stop_begin is missing")
        if issued is not None:
            window["reboot_issued"] = ev_time(issued)
        if recovered is not None:
            window["reboot_recovered"] = ev_time(recovered)
        if begin is not None:
            window["steady_begin"] = ev_time(begin)
        if end_event is not None:
            window["steady_end"] = ev_time(end_event)
        cutoff = window["final_stop"]
        forced = []
        for event in self.events:
            if event["forced"] and event["name"] != "final_stop_begin":
                moment = ev_time(event)
                if cutoff is None or moment < cutoff:
                    forced.append((moment, event.get("seq", 0), event["name"]))
        forced.sort()
        window["forced"] = [(name, moment) for moment, _seq, name in forced]
        if not self.cfg["battery"]:
            # The supplement has no reboot: its steady window starts at steady_begin (= t0 + settle_s).
            begin_time = window["steady_begin"]
            if begin_time is None:
                window["problems"].append("event steady_begin is missing")
            else:
                window["start"] = begin_time
            before = [item for item in forced if begin_time is None or item[0] <= begin_time + ORDER_EPS]
            if before:
                moment, _seq, name = before[-1]
                window["last_forced"] = name
                window["last_forced_end"] = moment
            else:
                window["problems"].append("no forced event before steady_begin")
        elif forced:
            moment, _seq, name = forced[-1]
            window["last_forced"] = name
            finish = moment
            if name == "reboot_issued" and window["reboot_recovered"] is not None and window["reboot_recovered"] >= moment:
                finish = window["reboot_recovered"]
            window["last_forced_end"] = finish
            window["start"] = finish
        else:
            window["problems"].append("no forced event before final_stop_begin")
        if end_event is not None:
            finish = ev_time(end_event)
            if cutoff is not None:
                finish = min(finish, cutoff)
            window["end"] = finish
        else:
            candidates = [s["epoch"] for s in self.samples
                          if is_bridged(s) and (cutoff is None or s["epoch"] <= cutoff)]
            if candidates:
                window["end"] = max(candidates)
            else:
                window["problems"].append("no bridged sample to end the soak window")
        if window["start"] is not None and window["end"] is not None:
            window["steady_s"] = window["end"] - window["start"]
            inside = [s["epoch"] for s in self.samples
                      if is_bridged(s) and window["start"] - EPS <= s["epoch"] <= window["end"] + EPS]
            if inside:
                window["first_sample"], window["last_sample"] = min(inside), max(inside)
            if not self.cfg["battery"]:
                # The supplement's steady duration is measured first sample to last sample (Astra, ARC-83 13:19Z).
                if inside:
                    window["steady_s"] = window["last_sample"] - window["first_sample"]
                else:
                    window["steady_s"] = None
                    window["problems"].append("no bridged sample inside the steady window")
        if window["t0"] is not None and window["end"] is not None:
            window["total_s"] = window["end"] - window["t0"]
        return window

    def rel(self, moment):
        """Human-friendly time relative to t0."""
        if moment is None:
            return "n/a"
        if self.W["t0"] is not None:
            return "t0%+.0fs" % (moment - self.W["t0"])
        return "%.0f" % moment

    def window_problem(self):
        return "; ".join(self.W["problems"]) or "see EVENTS-ORDER"

    def steady_samples(self):
        """Bridged samples inside the steady window, or None if the window is undefined."""
        if self.W["start"] is None or self.W["end"] is None:
            return None
        if self._steady is None:
            self._steady = in_window(self.bridged, self.W["start"], self.W["end"])
        return self._steady

    def bound(self, key):
        """The effective bound, or None when it was switched off with null (smoke profile: informational only)."""
        return self.bounds.get(key)

    def min_points(self):
        return int(self.bounds["min_points"])

    def host_window(self):
        """(start, end) of the steady window on the HOST clock: the host_epoch of the steady_begin and steady_end events
        (final_stop_begin when there is no steady_end), never earlier than the host_epoch of t0. None if undefined."""
        begin = self.first_event("steady_begin")
        end = self.first_event("steady_end") or self.first_event("final_stop_begin")
        t0 = self.first_event("t0_bridged_healthy")
        if begin is None or end is None:
            return None
        start = float(begin["host_epoch"])
        if t0 is not None:
            start = max(start, float(t0["host_epoch"]))
        return start, float(end["host_epoch"])

    def source_problems(self, needs):
        problems = []
        for key in needs:
            info = self.ev["src"][key]
            name = info["file"]
            if info["error"] == "missing":
                optional = key in dict(OPTIONAL_FILES)
                problems.append("%s%s is missing" % ("UNPROVED: " if optional else "", name))
                continue
            if info["error"]:
                problems.append("%s %s" % (name, info["error"]))
                continue
            if info["bad_json"]:
                problems.append("%d line(s) of %s are not valid JSON objects (lines %s)"
                                % (len(info["bad_json"]), name, short_list(info["bad_json"], 8)))
            if info["malformed"]:
                lines = [number for number, _reason in info["malformed"]]
                problems.append("%d record(s) of %s miss required fields or have wrong types (lines %s; first: %s)"
                                % (len(lines), name, short_list(lines, 8), info["malformed"][0][1]))
        return problems

    def exempt(self, moment):
        """True if a sample at `moment` lies in the grace window of a forced event or in the reboot window."""
        grace = float(self.cfg["forced_grace_s"])
        for name, forced_at in self.W["forced"]:
            if name == "reboot_issued":
                finish = self.W["reboot_recovered"]
                finish = forced_at if finish is None or finish < forced_at else finish
                if forced_at - EPS <= moment <= finish + grace + EPS:
                    return True
            elif forced_at - grace - EPS <= moment <= forced_at + grace + EPS:
                return True
        issued, recovered = self.W["reboot_issued"], self.W["reboot_recovered"]
        if issued is not None and recovered is not None and issued - EPS <= moment <= recovered + grace + EPS:
            return True
        return False


# --------------------------------------------------------------------------------------------------------------------
# check plumbing
# --------------------------------------------------------------------------------------------------------------------

class Outcome(object):
    def __init__(self):
        self.fails = []
        self.passes = []
        self.level = "PASS"

    def fail(self, message):
        if message not in self.fails:
            self.fails.append(message)

    def ok(self, message):
        self.passes.append(message)

    def info(self, message):
        self.level = "INFO"
        self.passes.append(message)

    def finish(self):
        if self.fails:
            shown = self.fails[:8]
            text = "; ".join(shown)
            if len(self.fails) > len(shown):
                text += "; (+%d more)" % (len(self.fails) - len(shown))
            return "FAIL", text
        return self.level, "; ".join(self.passes) or "ok"


CHECKS = []


def check(check_id, title, needs):
    def decorator(func):
        CHECKS.append((check_id, title, tuple(needs), func))
        return func
    return decorator


def violation_summary(items, limit=4):
    """'N violation(s): a; b; c; ...' for a list of strings."""
    shown = items[:limit]
    text = "%d violation(s): %s" % (len(items), "; ".join(shown))
    if len(items) > limit:
        text += "; ..."
    return text


# --------------------------------------------------------------------------------------------------------------------
# the checks
# --------------------------------------------------------------------------------------------------------------------

@check("EVIDENCE-FILES", "Every evidence file is present and every record is well formed", [])
def chk_files(c, o):
    parts = []
    for key, _name in FILES:
        info = c.ev["src"][key]
        problems = c.source_problems([key])
        for problem in problems:
            o.fail(problem)
        if not problems:
            parts.append("%s: %d" % (info["file"], info["records"]))
    if not o.fails:
        o.ok("all 7 files present, no malformed records (%s)" % ", ".join(parts))


@check("FLOORS", "Config is valid and Astra's floors are not lowered", ["config"])
def chk_floors(c, o):
    for issue in c.cfg_issues:
        o.fail(issue)
    for issue in c.bounds_issues:
        o.fail(issue)
    cfg = c.cfg
    if c.cfg["profile"] == "resources":
        for message in c.cfg_lowered:
            o.fail("resources profile: " + message)
        o.ok("profile resources (supplement, no battery, no reboot; Astra, ARC-83 2026-10-08T13:19Z): total>=%ss "
             "steady (first to last sample)>=%ss steady samples>=%s settle>=%ss interval<=%ss adjacent sample gap<=%ss "
             "(grace %ss)"
             % (fmt_num(cfg["min_total_s"]), fmt_num(cfg["min_steady_s"]), fmt_num(cfg["min_steady_samples"]),
                fmt_num(cfg["settle_s"]), fmt_num(cfg["sample_interval_s"]), fmt_num(cfg["gap_limit_s"]),
                fmt_num(cfg["forced_grace_s"])))
    elif c.cfg["profile"] == "full":
        for message in c.cfg_lowered:
            o.fail("full profile: " + message)
        o.ok("profile full: total>=%ss steady>=%ss steady samples>=%s post-reboot healthy>=%ss interval<=%ss "
             "updater runs>=%s kickstarts>=%s (tolerances: gap x%s, reboot recovery %ss, grace %ss)"
             % (cfg["min_total_s"], cfg["min_steady_s"], cfg["min_steady_samples"], cfg["post_reboot_healthy_s"],
                cfg["sample_interval_s"], cfg["updater_runs"], cfg["kickstarts"], cfg["max_gap_factor"],
                cfg["reboot_recovery_deadline_s"], cfg["forced_grace_s"]))
    else:
        o.info("SMOKE profile: Astra's floors are NOT enforced and this run cannot satisfy them "
               "(total>=%ss steady>=%ss steady samples>=%s interval %ss)"
               % (cfg["min_total_s"], cfg["min_steady_s"], cfg["min_steady_samples"], cfg["sample_interval_s"]))


def events_order_supplement(c, o):
    """Event contract of the resources supplement: apply2 < t0 < steady_begin (= t0 + settle) < steady_end < final stop.

    Extra non-forced events (heartbeats, interruption notes, ...) are ignored. A forced event BEFORE steady_begin (a
    retried consume, a stray battery step) is tolerated: it ends before the window starts. A forced event INSIDE the
    steady window is a FAIL: the supplement claims an uninterrupted window."""
    by = c.events_by_name
    for name in ("apply2", "t0_bridged_healthy", "steady_begin", "final_stop_begin"):
        count = len(by.get(name, []))
        if count == 0:
            o.fail("event %s is missing" % name)
        elif count > 1:
            o.fail("event %s occurs %d times, expected exactly once" % (name, count))
    if len(by.get("steady_end", [])) > 1:
        o.fail("event steady_end occurs %d times, expected at most once" % len(by["steady_end"]))
    for name, wanted in (("apply2", True), ("final_stop_begin", True), ("t0_bridged_healthy", False),
                         ("steady_begin", False), ("steady_end", False)):
        for event in by.get(name, []):
            if event["forced"] != wanted:
                o.fail("event %s must be recorded forced=%s" % (name, "true" if wanted else "false"))

    def single(name):
        event = c.first_event(name)
        return None if event is None else ev_time(event)

    order = [("apply2", single("apply2")), ("t0_bridged_healthy", single("t0_bridged_healthy")),
             ("steady_begin", single("steady_begin")), ("steady_end", single("steady_end")),
             ("final_stop_begin", single("final_stop_begin"))]
    present = [(label, moment) for label, moment in order if moment is not None]
    for (label_a, at_a), (label_b, at_b) in zip(present, present[1:]):
        if at_a > at_b + ORDER_EPS:
            o.fail("%s (%s) is not before %s (%s)" % (label_a, c.rel(at_a), label_b, c.rel(at_b)))
    t0, begin = single("t0_bridged_healthy"), single("steady_begin")
    settle = float(c.cfg["settle_s"])
    if t0 is not None and begin is not None:
        if begin - t0 + ORDER_EPS < settle:
            o.fail("steady_begin is only %.0f s after t0, the settle period is %.0f s" % (begin - t0, settle))
        if begin - t0 + ORDER_EPS < SETTLE_FLOOR_S:
            o.fail("steady_begin is only %.0f s after t0, below the supplement floor of %s s" % (begin - t0, SETTLE_FLOOR_S))
    claimed = by["steady_begin"][0].get("detail", {}).get("last_forced_event") if by.get("steady_begin") else None
    if claimed is not None and claimed != "apply2":
        o.fail("steady_begin names %r as the last forced event, the supplement's only forced step is apply2" % (claimed,))
    window = c.W
    if window["start"] is not None:
        inside = [name for name, moment in window["forced"] if moment > window["start"] + ORDER_EPS]
        if inside:
            o.fail("forced event(s) inside the steady window: %s (the supplement claims an uninterrupted window)"
                   % short_list(inside, 4))
    if not o.fails:
        o.ok("apply2 < t0 < steady_begin (t0 %+.0f s, settle %.0f s) < steady_end < final_stop_begin; no forced event "
             "inside the steady window" % ((begin or 0.0) - (t0 or 0.0), settle))


@check("EVENTS-ORDER", "Required events exist once, are labelled forced/unforced correctly and come in order",
       ["events", "config"])
def chk_events_order(c, o):
    if not c.cfg["battery"]:
        events_order_supplement(c, o)
        return
    by = c.events_by_name
    for name in ("apply1", "apply2", "t0_bridged_healthy", "reboot_issued", "reboot_recovered", "steady_begin",
                 "final_stop_begin"):
        count = len(by.get(name, []))
        if count == 0:
            o.fail("event %s is missing" % name)
        elif count > 1:
            o.fail("event %s occurs %d times, expected exactly once" % (name, count))
    if len(by.get("steady_end", [])) > 1:
        o.fail("event steady_end occurs %d times, expected at most once" % len(by["steady_end"]))
    family_span = {}
    family_count = {}
    for family, rx in sorted(FAMILY_RE.items()):
        numbered = []
        for name, items in by.items():
            match = rx.match(name)
            if match:
                for event in items:
                    numbered.append((int(match.group(1)), event))
        numbers = sorted(n for n, _event in numbered)
        wanted = int(c.cfg["updater_runs"] if family == "updater_run" else c.cfg["kickstarts"])
        family_count[family] = len(numbers)
        if numbers != list(range(1, len(numbers) + 1)):
            o.fail("%s events are not numbered 1..n without gaps or repeats: %s" % (family, short_list(numbers)))
        if len(numbers) < wanted:
            o.fail("only %d %s event(s), at least %d required" % (len(numbers), family, wanted))
        times = [ev_time(e) for _n, e in sorted(numbered, key=lambda pair: pair[0])]
        if any(later + ORDER_EPS < earlier for earlier, later in zip(times, times[1:])):
            o.fail("%s events are not in time order of their numbers" % family)
        family_span[family] = (min(times), max(times)) if times else None
    for event in c.events:
        name = event["name"]
        is_family = any(rx.match(name) for rx in FAMILY_RE.values())
        if (name in FORCED_NAMES or is_family) and not event["forced"]:
            o.fail("event %s must be recorded forced=true" % name)
        if name in UNFORCED_NAMES and event["forced"]:
            o.fail("event %s must be recorded forced=false" % name)

    def single(name):
        event = c.first_event(name)
        if event is None:
            return None
        moment = ev_time(event)
        return (moment, moment)

    order = [("apply1", single("apply1")), ("apply2", single("apply2")),
             ("t0_bridged_healthy", single("t0_bridged_healthy")),
             ("updater_run_*", family_span.get("updater_run")), ("kickstart_*", family_span.get("kickstart")),
             ("reboot_issued", single("reboot_issued")), ("reboot_recovered", single("reboot_recovered")),
             ("steady_begin", single("steady_begin")), ("steady_end", single("steady_end")),
             ("final_stop_begin", single("final_stop_begin"))]
    present = [(label, span) for label, span in order if span is not None]
    for (label_a, span_a), (label_b, span_b) in zip(present, present[1:]):
        if span_a[1] > span_b[0] + ORDER_EPS:
            o.fail("%s (latest %s) is not before %s (earliest %s)"
                   % (label_a, c.rel(span_a[1]), label_b, c.rel(span_b[0])))
    begin = c.first_event("steady_begin")
    last_end = c.W["last_forced_end"]
    if begin is not None and last_end is not None:
        if ev_time(begin) + ORDER_EPS < last_end:
            o.fail("steady_begin (%s) is %.0f s before the last forced event (%s) ends (%s)"
                   % (c.rel(ev_time(begin)), last_end - ev_time(begin), c.W["last_forced"], c.rel(last_end)))
        claimed = begin.get("detail", {}).get("last_forced_event")
        acceptable = {c.W["last_forced"]}
        if c.W["last_forced"] == "reboot_issued":
            acceptable.add("reboot_recovered")
        if claimed is not None and claimed not in acceptable:
            o.fail("steady_begin names %r as the last forced event but the events show %r" % (claimed, c.W["last_forced"]))
    if not o.fails:
        o.ok("%d updater_run, %d kickstart event(s); last forced event %s ends %s; steady_begin follows it"
             % (family_count.get("updater_run", 0), family_count.get("kickstart", 0), c.W["last_forced"],
                c.rel(last_end)))


@check("SAMPLES-TOTAL", "Enough bridged samples across the total window", ["samples", "events", "config"])
def chk_samples_total(c, o):
    window = c.W
    if window["t0"] is None or window["end"] is None:
        o.fail("total window undefined: " + c.window_problem())
        return
    count = len(in_window(c.bridged, window["t0"], window["end"]))
    base = int(math.floor(c.cfg["min_total_s"] / c.interval + 1e-9)) + 1
    gap = 0.0
    if window["reboot_issued"] is not None and window["reboot_recovered"] is not None:
        gap = max(0.0, window["reboot_recovered"] - window["reboot_issued"])
    lost = int(math.ceil(gap / c.interval - 1e-9))
    required = max(base - lost, 0)
    text = ("%d bridged sample(s) in the total window (required >= %d = floor(%ss/%ss)+1 = %d, minus %d lost to the "
            "%.0f s reboot gap)" % (count, required, c.cfg["min_total_s"], c.cfg["sample_interval_s"], base, lost, gap))
    if count < required:
        o.fail(text)
    else:
        o.ok(text)


@check("SAMPLES-STEADY", "Enough bridged samples in the steady window", ["samples", "events", "config"])
def chk_samples_steady(c, o):
    window = c.W
    if window["start"] is None or window["end"] is None:
        o.fail("steady window undefined: " + c.window_problem())
        return
    count = len(in_window(c.bridged, window["start"], window["end"]))
    required = int(math.ceil(c.cfg["min_steady_samples"]))
    text = "%d bridged sample(s) in the steady window (required >= %d)" % (count, required)
    if count < required:
        o.fail(text)
    else:
        o.ok(text)


@check("SAMPLE-GAPS", "No hole in the 60-second sampling", ["samples", "events", "config"])
def chk_sample_gaps(c, o):
    window = c.W
    if window["t0"] is None or window["end"] is None:
        o.fail("total window undefined: " + c.window_problem())
        return
    t0, end = window["t0"], window["end"]
    ordered = c.ev["samples"]
    disorder = 0
    for before, after in zip(ordered, ordered[1:]):
        if after["epoch"] <= before["epoch"] or after["seq"] <= before["seq"]:
            disorder += 1
    if disorder:
        o.fail("%d sample(s) whose epoch or seq does not increase in file order (a second sampler or a reset?)" % disorder)
    pre = [s for s in c.samples if s["epoch"] < t0 - EPS]
    inside = in_window(c.samples, t0, end)
    if not inside:
        o.fail("no samples between t0 and the end of the soak")
        return
    points = []
    if pre:
        points.append((pre[-1]["epoch"], "seq %d" % pre[-1]["seq"]))
    else:
        points.append((t0, "t0"))
    for sample in inside:
        points.append((sample["epoch"], "seq %d" % sample["seq"]))
    if inside[-1]["epoch"] < end - EPS:
        points.append((end, "window end"))
    issued, recovered = window["reboot_issued"], window["reboot_recovered"]
    normal_limit = c.cfg["gap_limit_s"]
    reboot_limit = c.cfg["reboot_recovery_deadline_s"] + c.interval
    worst = (0.0, None, None, normal_limit)
    problems = []
    for (a, label_a), (b, label_b) in zip(points, points[1:]):
        gap = b - a
        limit = normal_limit
        if issued is not None and recovered is not None and a < recovered + c.interval and b > issued:
            limit = reboot_limit
        if gap > limit + EPS:
            problems.append("%.0f s between %s (%s) and %s (%s), limit %.0f s"
                            % (gap, label_a, c.rel(a), label_b, c.rel(b), limit))
        if gap / limit > worst[0] / worst[3]:
            worst = (gap, label_a, label_b, limit)
    if problems:
        o.fail(violation_summary(problems))
    else:
        if not c.cfg["battery"]:
            o.ok("%d sample(s) from t0 to the end; worst gap %.0f s between %s and %s (limit %.0f s; no reboot in the "
                 "supplement)" % (len(inside), worst[0], worst[1], worst[2], worst[3]))
        else:
            o.ok("%d sample(s) from t0 to the end; worst gap %.0f s between %s and %s (limit %.0f s; reboot pair limit %.0f s)"
                 % (len(inside), worst[0], worst[1], worst[2], worst[3], reboot_limit))


@check("STEADY-DURATION", "Uninterrupted steady state lasts long enough after the last forced event",
       ["events", "samples", "config"])
def chk_steady_duration(c, o):
    window = c.W
    if window["steady_s"] is None:
        o.fail("steady window undefined: " + c.window_problem())
        return
    anchor = ("steady_begin (t0 + settle), first to last sample" if not c.cfg["battery"]
              else "the last forced event (%s)" % window["last_forced"])
    text = ("steady window %s after %s = %.0f s (required >= %.0f s)"
            % (fmt_dur(window["steady_s"]), anchor, window["steady_s"], c.cfg["min_steady_s"]))
    if window["steady_s"] + EPS < c.cfg["min_steady_s"]:
        o.fail(text)
    else:
        o.ok(text)


@check("TOTAL-DURATION", "Total soak from t0 lasts long enough", ["events", "samples", "config"])
def chk_total_duration(c, o):
    window = c.W
    if window["total_s"] is None:
        o.fail("total window undefined: " + c.window_problem())
        return
    text = ("total window %s from t0 = %.0f s (required >= %.0f s)"
            % (fmt_dur(window["total_s"]), window["total_s"], c.cfg["min_total_s"]))
    if window["total_s"] + EPS < c.cfg["min_total_s"]:
        o.fail(text)
    else:
        o.ok(text)


@check("STEADY-UNINTERRUPTED", "The node ran untouched through the whole steady window", ["samples", "events", "config"])
def chk_steady_uninterrupted(c, o):
    window = c.W
    if window["start"] is None or window["end"] is None:
        o.fail("steady window undefined: " + c.window_problem())
        return
    samples = in_window(c.samples, window["start"], window["end"])
    if not samples:
        o.fail("no samples in the steady window")
        return
    base = samples[0]
    problems = []

    def note(sample, message):
        problems.append("seq %d (%s): %s" % (sample["seq"], c.rel(sample["epoch"]), message))

    if not is_bridged(base):
        note(base, "node_exe %r is not a bridged v0.8 node" % (base.get("node_exe"),))
    for field in ("proc_start_epoch", "node_exe", "node_exe_sha256"):
        if base.get(field) is None:
            note(base, "%s is missing" % field)
    if base["main_pid"] <= 0:
        note(base, "no node process (main_pid %d)" % base["main_pid"])
    if c.expected_launcher is None:
        o.fail("expected_launcher_sha256 is not configured")
    if c.expected_version is None:
        o.fail("tag is not configured")
    restarts = None
    for sample in samples:
        if sample["boot_id"] != base["boot_id"]:
            note(sample, "boot_id changed")
        for field in ("main_pid", "proc_start_epoch", "node_exe", "node_exe_sha256"):
            if sample.get(field) != base.get(field):
                note(sample, "%s changed from %r to %r" % (field, base.get(field), sample.get(field)))
        if sample["node_state"] != "active":
            note(sample, "node_state is %r" % sample["node_state"])
        if not sample["health_ok"]:
            note(sample, "health check failed")
        if sample["node_procs"] != 1:
            note(sample, "node_procs is %d" % sample["node_procs"])
        value = sample.get("n_restarts")
        if value is not None:
            if restarts is None:
                restarts = value
            elif value != restarts:
                note(sample, "systemd NRestarts changed from %d to %d" % (restarts, value))
        if c.expected_launcher is not None and sample.get("launcher_sha256") != c.expected_launcher:
            note(sample, "launcher_sha256 is %r, expected the consumed launcher" % (sample.get("launcher_sha256"),))
        if c.expected_version is not None and sample.get("version_txt") != c.expected_version:
            note(sample, "version_txt is %r, expected %r" % (sample.get("version_txt"), c.expected_version))
        errors = [e for e in sample.get("errors") or [] if not e.startswith("info:")]
        if errors:
            note(sample, "sample errors: %s" % short_list(errors, 2))
    if problems:
        o.fail(violation_summary(problems))
    else:
        o.ok("%d sample(s): one boot, one pid %d (started %s), one binary, always active and healthy, one process"
             % (len(samples), base["main_pid"], base["proc_start_epoch"]))


@check("HEALTH-BRIDGED", "Every bridged sample is healthy outside the grace of forced events and the reboot",
       ["samples", "events", "config"])
def chk_health(c, o):
    window = c.W
    if window["t0"] is None:
        o.fail("t0 undefined: " + c.window_problem())
        return
    pool = [s for s in c.bridged if s["epoch"] >= window["t0"] - EPS]
    if not pool:
        o.fail("no bridged samples after t0")
        return
    problems = []
    exempt = 0
    for sample in pool:
        if not sample["health_ok"]:
            if c.exempt(sample["epoch"]):
                exempt += 1
            else:
                problems.append("seq %d (%s) unhealthy outside any forced-event grace window"
                                % (sample["seq"], c.rel(sample["epoch"])))
    if problems:
        o.fail(violation_summary(problems))
    else:
        o.ok("%d bridged sample(s) after t0, %d unhealthy sample(s) all inside forced-event grace (%.0f s)"
             % (len(pool), exempt, c.cfg["forced_grace_s"]))


@check("IDENTITY-STABLE", "One stable node identity in every bridged sample", ["samples", "events"])
def chk_identity(c, o):
    if c.t0_address is None:
        o.fail("t0_bridged_healthy.detail.address is missing or not a 64-hex address")
        return
    seen = 0
    problems = []
    for sample in c.bridged:
        if not sample["info_ok"]:
            continue
        address = norm_addr(sample.get("address"))
        if address is None:
            problems.append("seq %d: info_ok without an address" % sample["seq"])
            continue
        seen += 1
        if address != c.t0_address:
            problems.append("seq %d (%s): address %s differs from the t0 address %s"
                            % (sample["seq"], c.rel(sample["epoch"]), address[:16], c.t0_address[:16]))
        bridge_address = norm_addr(sample.get("bridge_node_address"))
        if bridge_address is not None and bridge_address != address:
            problems.append("seq %d: bridge-state address %s differs from /node/info %s"
                            % (sample["seq"], bridge_address[:16], address[:16]))
    if seen == 0:
        o.fail("no bridged sample carries an identity")
    if problems:
        o.fail(violation_summary(problems))
    if not o.fails:
        o.ok("%d bridged sample(s) carry address %s...; /node/info and bridge-state agree" % (seen, c.t0_address[:16]))


@check("STAKE-ZERO", "Stake is 0 in every bridged sample", ["samples", "events"])
def chk_stake(c, o):
    seen = 0
    problems = []
    for sample in c.bridged:
        if not sample["info_ok"]:
            continue
        stake = sample.get("stake")
        seen += 1
        if not is_int(stake) or stake != 0:
            problems.append("seq %d (%s): stake %r" % (sample["seq"], c.rel(sample["epoch"]), stake))
    if seen == 0:
        o.fail("no bridged sample reports a stake")
    if problems:
        o.fail(violation_summary(problems))
    if not o.fails:
        o.ok("stake 0 in %d bridged sample(s)" % seen)


@check("COMPUTE-OFF", "Compute stays off: no consent, no participation", ["samples", "events"])
def chk_compute(c, o):
    problems = []
    consent_seen = state_seen = 0
    for sample in c.bridged:
        consent = sample.get("compute_consent")
        if consent is not None:
            consent_seen += 1
            if consent not in ("no", "absent"):
                problems.append("seq %d: compute_consent %r" % (sample["seq"], consent))
        compute = sample.get("bridge_compute")
        if compute is not None:
            state_seen += 1
            if not compute.startswith("off:"):
                problems.append("seq %d: bridge-state compute %r does not start with 'off:'" % (sample["seq"], compute))
        if sample.get("chain_participation_enabled") is True:
            problems.append("seq %d: chain participation is enabled" % sample["seq"])
    if consent_seen == 0 or state_seen == 0:
        o.fail("no compute evidence in the bridged samples (consent in %d, bridge-state in %d sample(s))"
               % (consent_seen, state_seen))
    if problems:
        o.fail(violation_summary(problems))
    if not o.fails:
        o.ok("consent no/absent in %d and bridge-state off: in %d bridged sample(s); chain participation never on"
             % (consent_seen, state_seen))


@check("ONE-NODE", "Exactly one node process whenever the node is active", ["samples", "events"])
def chk_one_node(c, o):
    window = c.W
    if window["t0"] is None:
        o.fail("t0 undefined: " + c.window_problem())
        return
    stop = window["final_stop"]
    pool = [s for s in c.samples if s["epoch"] >= window["t0"] - EPS and (stop is None or s["epoch"] <= stop)]
    problems = []
    active = 0
    for sample in pool:
        if sample["node_procs"] > 1:
            problems.append("seq %d (%s): %d node processes" % (sample["seq"], c.rel(sample["epoch"]), sample["node_procs"]))
        if is_bridged(sample) and sample["node_state"] == "active":
            active += 1
            if sample["node_procs"] != 1:
                problems.append("seq %d (%s): bridged node active with node_procs=%d"
                                % (sample["seq"], c.rel(sample["epoch"]), sample["node_procs"]))
    if active == 0:
        o.fail("no active bridged sample")
    if problems:
        o.fail(violation_summary(problems))
    if not o.fails:
        o.ok("node_procs == 1 in all %d active bridged sample(s); never more than one in %d sample(s)" % (active, len(pool)))


@check("LEGACY-UNCHANGED-SAMPLES", "The v0.7 data stays byte-identical in every bridged sample", ["samples", "events"])
def chk_legacy_samples(c, o):
    if c.t0_fingerprint is None:
        o.fail("t0_bridged_healthy.detail.legacy_fingerprint is missing")
        return
    problems = []
    seen = 0
    for sample in c.bridged:
        fingerprint = sample.get("legacy_fingerprint")
        if fingerprint is not None:
            seen += 1
            if fingerprint != c.t0_fingerprint:
                problems.append("seq %d (%s): fingerprint %s... differs from t0 %s..."
                                % (sample["seq"], c.rel(sample["epoch"]), fingerprint[:12], c.t0_fingerprint[:12]))
        compare = sample.get("legacy_byte_compare")
        if compare is not None and compare != "same":
            problems.append("seq %d (%s): byte compare says %r" % (sample["seq"], c.rel(sample["epoch"]), compare))
    if seen == 0:
        o.fail("no bridged sample carries a legacy fingerprint")
    if problems:
        o.fail(violation_summary(problems))
    if not o.fails:
        o.ok("legacy fingerprint equals the t0 value in %d bridged sample(s); byte compare never differed" % seen)


def pair_text(a, b):
    return "before=%r after=%r" % (a, b)


@check("INVARIANTS-BEFORE-AFTER", "Invariants BEFORE vs AFTER: data, identity, stake, compute, privacy, units",
       ["inv_before", "inv_after", "events", "config"])
def chk_invariants(c, o):
    before, after = c.ev["inv_before"], c.ev["inv_after"]
    if before is None or after is None:
        return
    for label, doc in (("before", before), ("after", after)):
        if doc.get("schema") != SCHEMA_INVARIANTS:
            o.fail("invariants-%s.json schema is %r, expected %r" % (label, doc.get("schema"), SCHEMA_INVARIANTS))
        if doc.get("label") != label:
            o.fail("invariants-%s.json label is %r" % (label, doc.get("label")))
    for key in ("legacy_snapshot_sha256", "v07_seed_sha256"):
        b, a = before.get(key), after.get(key)
        if not is_hex64(b) or not is_hex64(a):
            o.fail("%s is missing or not a sha256 digest (%s)" % (key, pair_text(b, a)))
        elif b != a:
            o.fail("%s changed: before %s..., after %s..." % (key, b[:16], a[:16]))
    units_b, units_a = before.get("unit_files"), after.get("unit_files")
    for unit in UNIT_FILES:
        b = units_b.get(unit) if isinstance(units_b, dict) else None
        a = units_a.get(unit) if isinstance(units_a, dict) else None
        if not is_hex64(b) or not is_hex64(a):
            o.fail("unit file hash for %s is missing (%s)" % (unit, pair_text(b, a)))
        elif b != a:
            o.fail("unit file %s changed: before %s..., after %s..." % (unit, b[:16], a[:16]))
    timer_b, timer_a = before.get("updater_timer"), after.get("updater_timer")
    if timer_b != timer_a:
        o.fail("updater timer state changed (%s)" % pair_text(timer_b, timer_a))
    if not (isinstance(timer_a, dict) and timer_a.get("active") is True and timer_a.get("enabled") is True):
        o.fail("updater timer is not active and enabled after the soak (%r)" % (timer_a,))
    node = after.get("node")
    if not isinstance(node, dict):
        o.fail("invariants-after has no node section")
    else:
        if node.get("node_procs") != 1:
            o.fail("node.node_procs is %r, expected 1" % (node.get("node_procs"),))
        dirs = node.get("node_dirs")
        if not isinstance(dirs, list) or len(dirs) != 1:
            o.fail("node.node_dirs must list exactly one node data directory, got %r" % (dirs,))
        exe = node.get("exe")
        if not isinstance(exe, str) or BRIDGED_MARK not in exe:
            o.fail("node.exe %r is not a bridged v0.8 node" % (exe,))
        if c.node_sha is not None and node.get("exe_sha256") != c.node_sha:
            o.fail("node.exe_sha256 %r differs from the pinned node digest" % (node.get("exe_sha256"),))
        argv = node.get("argv")
        if not isinstance(argv, list) or not all(isinstance(x, str) for x in argv):
            o.fail("node.argv is missing")
        else:
            for flag in ("--stake", "--min-stake"):
                values = [argv[i + 1] if i + 1 < len(argv) else None for i, x in enumerate(argv) if x == flag]
                if not values or any(v != "0" for v in values):
                    o.fail("argv %s values are %r, expected 0" % (flag, values))
            for flag in FORBIDDEN_ARGV:
                if flag in argv:
                    o.fail("argv contains forbidden %s" % flag)
            if "--community-mode" not in argv:
                o.fail("argv lacks --community-mode (privacy-safe registration)")
    info = after.get("node_info")
    if not isinstance(info, dict) or not is_int(info.get("stake")) or info.get("stake") != 0:
        o.fail("node_info.stake is %r, expected 0" % ((info or {}).get("stake") if isinstance(info, dict) else None,))
    if isinstance(info, dict):
        validator = norm_addr(info.get("validator"))
        if c.t0_address is None:
            o.fail("t0 address unknown, cannot compare node_info.validator")
        elif validator != c.t0_address:
            o.fail("node_info.validator %s... differs from the t0 address %s..." % ((validator or "?")[:16], c.t0_address[:16]))
    health = after.get("health")
    if not isinstance(health, dict) or health.get("chain_participation_enabled") is not False:
        o.fail("health.chain_participation_enabled is %r, expected false"
               % (health.get("chain_participation_enabled") if isinstance(health, dict) else None,))
    state = after.get("bridge_state")
    if not isinstance(state, dict):
        o.fail("invariants-after has no bridge_state")
    else:
        if not is_int(state.get("stake")) or state.get("stake") != 0:
            o.fail("bridge_state.stake is %r, expected 0" % (state.get("stake"),))
        compute = state.get("compute")
        if not isinstance(compute, str) or not compute.startswith("off:"):
            o.fail("bridge_state.compute is %r, expected off:..." % (compute,))
        if state.get("legacy_kind") != "headless":
            o.fail("bridge_state.legacy_kind is %r, expected headless" % (state.get("legacy_kind"),))
        if state.get("community_registration") is not None and state.get("community_registration") is not True:
            o.fail("bridge_state.community_registration is %r, expected true" % (state.get("community_registration"),))
        address = norm_addr(state.get("node_address"))
        if c.t0_address is not None and address is not None and address != c.t0_address:
            o.fail("bridge_state.node_address %s... differs from the t0 address %s..." % (address[:16], c.t0_address[:16]))
    if after.get("compute_consent") not in ("no", "absent"):
        o.fail("compute_consent is %r, expected no or absent" % (after.get("compute_consent"),))
    installed = after.get("installed")
    if not isinstance(installed, dict):
        o.fail("invariants-after has no installed section")
    else:
        if c.expected_launcher is None or installed.get("bin_arc_node_sha256") != c.expected_launcher:
            o.fail("installed bin/arc-node sha256 %r is not the expected launcher" % (installed.get("bin_arc_node_sha256"),))
        if c.expected_version is None or installed.get("version_txt") != c.expected_version:
            o.fail("installed version.txt %r, expected %r" % (installed.get("version_txt"), c.expected_version))
    status = after.get("community_status")
    if isinstance(status, dict) and status.get("public_name") is not None:
        reason = rule_public_name(status.get("public_name"), c.t0_address)
        if reason:
            o.fail("community_status: " + reason)
    if not c.cfg["battery"]:
        boot_b, boot_a = before.get("boot_id"), after.get("boot_id")
        if not (isinstance(boot_b, str) and boot_b and isinstance(boot_a, str) and boot_a) or boot_b != boot_a:
            o.fail("boot_id differs between invariants-before and invariants-after (%s): the supplement has no reboot"
                   % pair_text(boot_b, boot_a))
    if not o.fails:
        o.ok("legacy data %s..., seed hash, 3 unit files and timer unchanged; one node, stake 0, compute off, "
             "--community-mode, no forbidden flags; identity %s..." % (before["legacy_snapshot_sha256"][:12],
                                                                        (c.t0_address or "?")[:12]))


def rule_public_name(name, address):
    if not isinstance(name, str) or PUBLIC_NAME_RE.fullmatch(name) is None:
        return "public name %r is not node-xxxxxxxx" % (name,)
    if address is None:
        return "no address to check the public name %r against" % name
    if name != "node-" + address[:8]:
        return "public name %r is not node-%s (the first 8 hex characters of the address)" % (name, address[:8])
    return None


@check("PRIVACY-SAFE-NAME", "The scoreboard name is node-<first 8 hex of the address>", ["samples", "events"])
def chk_privacy(c, o):
    problems = []
    seen = 0
    for sample in c.bridged:
        name = sample.get("public_name")
        if name is None:
            continue
        seen += 1
        address = norm_addr(sample.get("address")) if sample["info_ok"] and sample.get("address") else c.t0_address
        reason = rule_public_name(name, address)
        if reason:
            problems.append("seq %d (%s): %s" % (sample["seq"], c.rel(sample["epoch"]), reason))
    after = c.ev.get("inv_after") or {}
    status = after.get("community_status") if isinstance(after, dict) else None
    if isinstance(status, dict) and status.get("public_name") is not None:
        reason = rule_public_name(status.get("public_name"), c.t0_address)
        if reason:
            problems.append("invariants-after community_status: " + reason)
    if seen == 0:
        o.fail("no bridged sample carries a public_name")
    if problems:
        o.fail(violation_summary(problems))
    if not o.fails:
        o.ok("public name is node-%s in %d bridged sample(s)" % ((c.t0_address or "?")[:8], seen))


@check("REGISTRATION-LIVE", "Live registration matches the live-network setting", ["samples", "config"])
def chk_registration(c, o):
    values = [s["coordinators_registered"] for s in c.bridged if s.get("coordinators_registered") is not None]
    totals = [s["coordinators_total"] for s in c.bridged if s.get("coordinators_total") is not None]
    peak = max(values) if values else None
    total = max(totals) if totals else None
    if c.live_network == "allowed":
        if peak is None:
            o.fail("no bridged sample reports coordinators_registered")
        elif peak < 1:
            o.fail("live network allowed but the node never registered (max coordinators_registered %d of %s)"
                   % (peak, total))
        else:
            o.ok("live network allowed: registered with up to %d of %s coordinator(s)" % (peak, total))
    elif c.live_network == "blocked":
        if peak is not None and peak > 0:
            o.fail("live network blocked but the node registered with %d coordinator(s): the isolation did not hold" % peak)
        else:
            o.info("live network blocked: registration is expected to be 0 (observed max %s)" % (peak,))
    else:
        o.fail("live_network is %r; cannot judge registration" % (c.live_network,))


def reboot_events(c, o):
    window = c.W
    ok = True
    if window["reboot_issued"] is None:
        o.fail("event reboot_issued is missing")
        ok = False
    if window["reboot_recovered"] is None:
        o.fail("event reboot_recovered is missing")
        ok = False
    return ok


@check("REBOOT-BOOT-ID", "A real guest reboot: the boot ID changed exactly once, inside the reboot window",
       ["samples", "events", "config"])
def chk_reboot_boot_id(c, o):
    window = c.W
    have_events = reboot_events(c, o)
    if not c.boot_before:
        o.fail("reboot_issued.detail.boot_id_before is missing")
    if not c.boot_after:
        o.fail("reboot_recovered.detail.boot_id_after is missing")
    if c.boot_before and c.boot_after and c.boot_before == c.boot_after:
        o.fail("boot ID did not change across the reboot (%s)" % c.boot_before)
    if window["t0"] is None:
        o.fail("t0 undefined: " + c.window_problem())
        return
    stop = window["final_stop"]
    pool = [s for s in c.samples if s["epoch"] >= window["t0"] - EPS and (stop is None or s["epoch"] <= stop)]
    changes = [(a, b) for a, b in zip(pool, pool[1:]) if a["boot_id"] != b["boot_id"]]
    if len(changes) != 1:
        o.fail("%d boot_id change(s) among the samples between t0 and the final stop, exactly 1 required" % len(changes))
    elif have_events:
        first, second = changes[0]
        if c.boot_before and first["boot_id"] != c.boot_before:
            o.fail("samples before the change carry boot %r, reboot_issued says %r" % (first["boot_id"], c.boot_before))
        if c.boot_after and second["boot_id"] != c.boot_after:
            o.fail("samples after the change carry boot %r, reboot_recovered says %r" % (second["boot_id"], c.boot_after))
        if not (window["reboot_issued"] - EPS <= second["epoch"] <= window["reboot_recovered"] + EPS):
            o.fail("the boot change is first seen at %s, outside the reboot window [%s, %s]"
                   % (c.rel(second["epoch"]), c.rel(window["reboot_issued"]), c.rel(window["reboot_recovered"])))
        uptime = second.get("uptime_s")
        limit = c.cfg["reboot_recovery_deadline_s"] + c.interval
        if is_num(uptime) and uptime > limit:
            o.fail("first sample of the new boot reports uptime %.0f s (> %.0f s): a reboot resets uptime" % (uptime, limit))
    for label, doc, expected in (("before", c.ev.get("inv_before"), c.boot_before),
                                 ("after", c.ev.get("inv_after"), c.boot_after)):
        if isinstance(doc, dict) and expected and isinstance(doc.get("boot_id"), str) and doc["boot_id"] != expected:
            o.fail("invariants-%s.json was taken in boot %r, expected %r" % (label, doc["boot_id"], expected))
    if not o.fails:
        o.ok("boot %s -> %s, one change, first seen %s, inside the reboot window" % (
            c.boot_before, c.boot_after, c.rel(changes[0][1]["epoch"])))


@check("REBOOT-AUTO-RECOVERY", "The node recovered by itself: healthy in time and no manual start after the reboot",
       ["samples", "events", "commands", "config"])
def chk_reboot_recovery(c, o):
    window = c.W
    if not reboot_events(c, o):
        return
    if not c.boot_after:
        o.fail("reboot_recovered.detail.boot_id_after is missing")
        return
    first_healthy = None
    for sample in c.bridged:
        if (sample["boot_id"] == c.boot_after and sample["health_ok"]
                and sample["epoch"] >= window["reboot_issued"] - EPS):
            first_healthy = sample
            break
    if first_healthy is None:
        o.fail("no healthy bridged sample with the new boot ID %s" % c.boot_after)
    else:
        delay = first_healthy["epoch"] - window["reboot_issued"]
        deadline = c.cfg["reboot_recovery_deadline_s"]
        if delay > deadline + EPS:
            o.fail("first healthy bridged sample of the new boot came %.0f s after reboot_issued (limit %.0f s)"
                   % (delay, deadline))
        if c.claimed_first_healthy is None:
            o.fail("reboot_recovered.detail.first_healthy_sample_epoch is missing")
        elif abs(c.claimed_first_healthy - first_healthy["epoch"]) > FIRST_HEALTHY_EPS:
            o.fail("reboot_recovered claims the first healthy sample at %s but the samples show %s"
                   % (c.rel(c.claimed_first_healthy), c.rel(first_healthy["epoch"])))
    if c.issued_host_epoch is None:
        return
    stop_event = c.first_event("final_stop_begin")
    stop_host = float(stop_event["host_epoch"]) if stop_event is not None else None
    manual = []
    scanned = 0
    for command in c.ev["commands"]:
        moment = command["host_epoch"]
        if moment < c.issued_host_epoch - EPS or (stop_host is not None and moment >= stop_host):
            continue
        scanned += 1
        text = command["cmd"]
        if MANUAL_START_RE.search(text) or any(word in text for word in FORBIDDEN_COMMAND_WORDS):
            manual.append("%r at host %.0f" % (text[:80], moment))
    if manual:
        o.fail("manual node start/interference after reboot_issued: %s" % short_list(manual, 3))
    if not o.fails and first_healthy is not None:
        o.ok("first healthy bridged sample %.0f s after reboot_issued (limit %.0f s); %d command(s) after the reboot, "
             "none starts or restarts the node" % (first_healthy["epoch"] - window["reboot_issued"],
                                                   c.cfg["reboot_recovery_deadline_s"], scanned))


@check("REBOOT-UPDATER-TIMER", "The updater timer is active in every bridged sample after the reboot", ["samples", "events"])
def chk_reboot_timer(c, o):
    if not c.boot_after:
        o.fail("reboot_recovered.detail.boot_id_after is missing")
        return
    pool = [s for s in c.bridged if s["boot_id"] == c.boot_after]
    if not pool:
        o.fail("no bridged sample with the new boot ID")
        return
    bad = [s for s in pool if not s["updater_timer_active"]]
    if bad:
        o.fail("updater timer inactive in %d of %d bridged sample(s) after the reboot (first: seq %d, %s)"
               % (len(bad), len(pool), bad[0]["seq"], c.rel(bad[0]["epoch"])))
    else:
        o.ok("updater timer active in all %d bridged sample(s) of the new boot" % len(pool))


@check("REBOOT-HEALTHY-WINDOW", "At least 10 minutes of healthy 60-second samples right after recovery",
       ["samples", "events", "config"])
def chk_reboot_window(c, o):
    if not c.boot_after:
        o.fail("reboot_recovered.detail.boot_id_after is missing")
        return
    pool = [s for s in c.bridged if s["boot_id"] == c.boot_after]
    start = None
    for index, sample in enumerate(pool):
        if sample["health_ok"] and sample["node_state"] == "active":
            start = index
            break
    if start is None:
        o.fail("no healthy bridged sample with the new boot ID")
        return
    limit = c.cfg["max_gap_factor"] * c.interval
    run = [pool[start]]
    for sample in pool[start + 1:]:
        if not (sample["health_ok"] and sample["node_state"] == "active"):
            break
        if sample["epoch"] - run[-1]["epoch"] > limit + EPS:
            break
        run.append(sample)
    span = run[-1]["epoch"] - run[0]["epoch"]
    need_span = c.cfg["post_reboot_healthy_s"]
    need_count = int(math.floor(need_span / c.interval + 1e-9)) + 1
    text = ("healthy run from recovery: %d sample(s) spanning %.0f s (required >= %d samples and >= %.0f s)"
            % (len(run), span, need_count, need_span))
    if span + EPS < need_span or len(run) < need_count:
        o.fail(text)
    else:
        o.ok(text)


@check("REBOOT-SAME-LAUNCHER", "The same launcher and node binary run before and after the reboot",
       ["samples", "events", "config"])
def chk_reboot_same(c, o):
    window = c.W
    if window["reboot_issued"] is None or not c.boot_after:
        o.fail("reboot events or the new boot ID are missing")
        return
    before = [s for s in c.bridged if s["epoch"] <= window["reboot_issued"] + EPS
              and s.get("launcher_sha256") and s.get("node_exe_sha256")]
    after = [s for s in c.bridged if s["boot_id"] == c.boot_after
             and s.get("launcher_sha256") and s.get("node_exe_sha256")]
    if not before or not after:
        o.fail("no bridged sample with both digests before (%d) or after (%d) the reboot" % (len(before), len(after)))
        return
    pre, post = before[-1], after[0]
    if pre["launcher_sha256"] != post["launcher_sha256"]:
        o.fail("launcher_sha256 changed across the reboot: %s... -> %s..." % (pre["launcher_sha256"][:12], post["launcher_sha256"][:12]))
    if pre["node_exe_sha256"] != post["node_exe_sha256"]:
        o.fail("node_exe_sha256 changed across the reboot: %s... -> %s..." % (pre["node_exe_sha256"][:12], post["node_exe_sha256"][:12]))
    if c.expected_launcher is not None and post["launcher_sha256"] != c.expected_launcher:
        o.fail("launcher after the reboot is not the consumed launcher")
    if c.node_sha is not None and post["node_exe_sha256"] != c.node_sha:
        o.fail("node binary after the reboot is not the pinned node")
    if not o.fails:
        o.ok("launcher %s... and node %s... identical before and after the reboot"
             % (pre["launcher_sha256"][:12], pre["node_exe_sha256"][:12]))


@check("LIVE-REQUIRED", "All required live checks were recorded by the orchestrator", ["live"])
def chk_live_required(c, o):
    present = set(r["id"] for r in c.ev["live"])
    wanted = REQUIRED_LIVE_IDS if c.cfg["battery"] else SUPPLEMENT_LIVE_IDS
    missing = [i for i in wanted if i not in present]
    if c.ev["src"]["live"]["error"] == "missing":
        o.fail("orchestrator wrote no live checks")
    elif missing:
        o.fail("missing live check id(s): %s" % ", ".join(missing))
    else:
        o.ok("all %d required live check ids present" % len(wanted))


@check("LIVE-RESULTS", "No live check failed", ["live"])
def chk_live_results(c, o):
    if c.ev["src"]["live"]["error"] == "missing":
        o.fail("orchestrator wrote no live checks")
        return
    records = c.ev["live"]
    counts = dict((k, 0) for k in ("PASS", "FAIL", "INFO"))
    failed = []
    for record in records:
        counts[record["result"]] += 1
        if record["result"] == "FAIL":
            failed.append(record["id"])
    clashes = sorted(set(r["id"] for r in records if r["id"] in COMPUTED_IDS or r["id"] in EXTRA_IDS))
    summary = "%d live check(s): %d PASS, %d FAIL, %d INFO" % (len(records), counts["PASS"], counts["FAIL"], counts["INFO"])
    if failed:
        o.fail("%s; failed: %s" % (summary, ", ".join(failed)))
    if clashes:
        o.fail("live check id(s) collide with evaluator ids: %s" % ", ".join(clashes))
    if not o.fails:
        o.ok(summary)


# --------------------------------------------------------------------------------------------------------------------
# ARC-83 criteria D and E: resource, download, OOM, VM-memory, binding and freshness checks (EXTRA_CHECKS)
# --------------------------------------------------------------------------------------------------------------------
# These are registered apart from CHECKS so that the 25 classic ids keep their meaning, order and counts for all older
# evidence; analyze() runs them only where the config asks (see extra_mode).

EXTRA_CHECKS = []


def extra_check(check_id, title, needs):
    def decorator(func):
        EXTRA_CHECKS.append((check_id, title, tuple(needs), func))
        return func
    return decorator


STRUCTURAL_FIELDS = ("release_cache_bytes", "release_cache_files", "models_bytes", "partial_files", "bridge_downloads",
                     "largest_file_bytes")


def num_field(sample, field):
    """The value of a numeric sample field, or None (absent, null, non-numeric or negative)."""
    value = sample.get(field)
    if is_num(value) and value >= 0:
        return float(value)
    return None


def ls_slope(points):
    """Least-squares slope sum((x-mean x)(y-mean y)) / sum((x-mean x)^2) of [(x, y)]; None for < 2 points or no spread."""
    count = len(points)
    if count < 2:
        return None
    mean_x = math.fsum(x for x, _y in points) / count
    mean_y = math.fsum(y for _x, y in points) / count
    spread = math.fsum((x - mean_x) ** 2 for x, _y in points)
    if spread <= 0.0:
        return None
    return math.fsum((x - mean_x) * (y - mean_y) for x, y in points) / spread


def series(samples, field, scale=1.0):
    """[(hours since the first point, value * scale, sample)] for the samples with a valid value, in time order.

    `field` is a sample field name or a function sample -> value or None."""
    getter = field if callable(field) else (lambda sample: num_field(sample, field))
    rows = []
    for sample in samples:
        value = getter(sample)
        if value is not None:
            rows.append((sample["epoch"], value * scale, sample))
    if not rows:
        return []
    origin = rows[0][0]
    return [((epoch - origin) / 3600.0, value, sample) for epoch, value, sample in rows]


def slope_of(rows):
    return ls_slope([(hours, value) for hours, value, _sample in rows])


def median(values):
    ordered = sorted(values)
    middle = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[middle]
    return (ordered[middle - 1] + ordered[middle]) / 2.0


def steady_rows(c, o):
    """The steady-window samples, or None after recording why the window cannot be judged (fail closed)."""
    steady = c.steady_samples()
    if steady is None:
        o.fail("UNPROVED: steady window undefined: " + c.window_problem())
        return None
    if not steady:
        o.fail("UNPROVED: no bridged sample in the steady window")
        return None
    return steady


def disk_reserve_b(c, sample):
    """R = max(disk_reserve_floor_b, disk_reserve_fraction * disk_total_b) for one sample, or None."""
    total = num_field(sample, "disk_total_b")
    floor_b, fraction = c.bound("disk_reserve_floor_b"), c.bound("disk_reserve_fraction")
    if total is None or floor_b is None or fraction is None:
        return None
    return max(floor_b, fraction * total)


def seq_at(c, sample):
    return "seq %d (%s)" % (sample["seq"], c.rel(sample["epoch"]))


@extra_check("RESOURCES-PRESENT", "The resource fields are recorded in >= 99 % of the steady-window samples",
             ["samples", "events", "config"])
def chk_resources_present(c, o):
    steady = steady_rows(c, o)
    if steady is None:
        return
    need = int(math.ceil(RES_PRESENT_MIN_FRACTION * len(steady) - 1e-9))
    counts = [(field, sum(1 for s in steady if num_field(s, field) is not None)) for field in RES_PRESENT_FIELDS]
    for field, count in counts:
        if count < need:
            o.fail("UNPROVED: %s is recorded in only %d of %d steady sample(s) (need >= %d = 99%%)"
                   % (field, count, len(steady), need))
    if not o.fails:
        o.ok("%d resource fields recorded in >= 99%% of %d steady sample(s) (fewest: %s in %d)"
             % (len(RES_PRESENT_FIELDS), len(steady), min(counts, key=lambda pair: (pair[1], pair[0]))[0],
                min(count for _field, count in counts)))


@extra_check("RES-RSS", "Node RSS slope over the steady window stays under the adopted bound",
             ["samples", "events", "config"])
def chk_res_rss(c, o):
    steady = steady_rows(c, o)
    if steady is None:
        return
    rows = series(steady, "node_rss_kb", 1.0 / 1024.0)
    if len(rows) < c.min_points():
        o.fail("UNPROVED: %d RSS point(s) in the steady window, the slope needs >= %d" % (len(rows), c.min_points()))
        return
    slope = slope_of(rows)
    if slope is None:
        o.fail("UNPROVED: the RSS slope cannot be computed (no spread in time)")
        return
    limit = c.bound("rss_slope_mib_per_h_max")
    text = ("RSS slope %+.3f MiB/h over %d point(s) (%.1f -> %.1f MiB; least squares of MiB on hours)"
            % (slope, len(rows), rows[0][1], rows[-1][1]))
    if limit is None:
        o.info(text + "; no bound configured (informational)")
    elif slope > limit:
        o.fail(text + "; above the bound of %s MiB/h" % fmt_num(limit))
    else:
        o.ok(text + "; bound %s MiB/h (growth only)" % fmt_num(limit))


@extra_check("RES-MEMAVAIL", "MemAvailable never drops under the adopted minimum", ["samples", "events", "config"])
def chk_res_memavail(c, o):
    steady = steady_rows(c, o)
    if steady is None:
        return
    rows = series(steady, "mem_available_kb", 1.0 / 1024.0)
    if not rows:
        o.fail("UNPROVED: no MemAvailable value in the steady window")
        return
    low = min(rows, key=lambda row: (row[1], row[2]["epoch"]))
    limit = c.bound("mem_available_min_mib")
    text = "minimum MemAvailable %.1f MiB at %s over %d point(s)" % (low[1], seq_at(c, low[2]), len(rows))
    if limit is None:
        o.info(text + "; no bound configured (informational)")
    elif low[1] < limit:
        o.fail(text + "; below the minimum of %s MiB" % fmt_num(limit))
    else:
        o.ok(text + "; minimum %s MiB" % fmt_num(limit))


@extra_check("RES-PROJECTION", "Projected RSS growth stays within the share of the starting headroom",
             ["samples", "events", "config"])
def chk_res_projection(c, o):
    steady = steady_rows(c, o)
    if steady is None:
        return
    rss = series(steady, "node_rss_kb", 1.0 / 1024.0)
    avail = series(steady, "mem_available_kb", 1.0 / 1024.0)
    if len(rss) < c.min_points():
        o.fail("UNPROVED: %d RSS point(s) in the steady window, the slope needs >= %d" % (len(rss), c.min_points()))
        return
    if not avail:
        o.fail("UNPROVED: no MemAvailable value in the steady window")
        return
    slope = slope_of(rss)
    if slope is None:
        o.fail("UNPROVED: the RSS slope cannot be computed (no spread in time)")
        return
    hours, fraction, floor = (c.bound(k) for k in ("projection_hours", "projection_fraction", "mem_available_min_mib"))
    start = avail[0][1]
    growth = (hours if hours is not None else 0.0) * max(slope, 0.0)
    if hours is None or fraction is None or floor is None:
        o.info("projected RSS growth needs projection_hours, projection_fraction and mem_available_min_mib; "
               "not all configured (informational): slope %+.3f MiB/h, starting MemAvailable %.1f MiB" % (slope, start))
        return
    headroom = start - floor
    limit = fraction * headroom
    plain = fraction * start
    text = ("%sh RSS growth %.1f MiB (slope %+.3f MiB/h, growth only) vs %s x conservative headroom %.1f MiB = %.1f MiB "
            "(first steady MemAvailable %.1f MiB - minimum %s MiB); INFO plain variant: %.1f MiB vs %s x %.1f MiB = %.1f MiB"
            % (fmt_num(hours), growth, slope, fmt_num(fraction), headroom, limit, start, fmt_num(floor), growth,
               fmt_num(fraction), start, plain))
    if growth > limit:
        o.fail(text)
    else:
        o.ok(text)


@extra_check("RES-DISK", "Free disk stays above the reserve and its trend leaves the adopted runway",
             ["samples", "events", "config"])
def chk_res_disk(c, o):
    steady = steady_rows(c, o)
    if steady is None:
        return
    pool = [s for s in c.bridged if num_field(s, "disk_free_b") is not None and num_field(s, "disk_total_b") is not None]
    if not pool:
        o.fail("UNPROVED: no bridged sample carries both disk_free_b and disk_total_b")
        return
    runway_min = c.bound("disk_runway_h_min")
    if disk_reserve_b(c, pool[0]) is None or runway_min is None:
        o.info("disk reserve and runway need disk_reserve_floor_b, disk_reserve_fraction and disk_runway_h_min; not all "
               "configured (informational)")
        return
    violations = []
    tightest = None
    for sample in pool:
        reserve = disk_reserve_b(c, sample)
        free = num_field(sample, "disk_free_b")
        margin = free - reserve
        if tightest is None or margin < tightest[0]:
            tightest = (margin, free, reserve, sample)
        if free <= reserve:
            violations.append("%s: free %.2f GiB <= reserve %.2f GiB" % (seq_at(c, sample), free / MIB / 1024.0,
                                                                          reserve / MIB / 1024.0))
    rows = series(steady, "disk_free_b", 1.0 / MIB)
    if len(rows) < c.min_points():
        o.fail("UNPROVED: %d free-disk point(s) in the steady window, the slope needs >= %d" % (len(rows), c.min_points()))
    slope = slope_of(rows) if len(rows) >= c.min_points() else None
    if slope is None and len(rows) >= c.min_points():
        o.fail("UNPROVED: the free-disk slope cannot be computed (no spread in time)")
    if violations:
        o.fail(violation_summary(violations))
    if slope is not None:
        reserve_last = disk_reserve_b(c, rows[-1][2]) or disk_reserve_b(c, pool[-1])
        room = rows[-1][1] - reserve_last / MIB
        if slope < 0:
            runway = room / -slope
            runway_text = "%.1f h" % runway
            if runway < runway_min:
                o.fail("free disk falls %.3f MiB/h with %.1f MiB left above the reserve: runway %.1f h < %s h"
                       % (-slope, room, runway, fmt_num(runway_min)))
        else:
            runway_text = "unbounded (free disk is not shrinking)"
        if not o.fails:
            o.ok("minimum free %.2f GiB vs reserve %.2f GiB (R = max(%s B, %s x total)) over %d bridged sample(s); free-disk "
                 "slope %+.3f MiB/h over %d point(s), runway %s (>= %s h)"
                 % (tightest[1] / MIB / 1024.0, tightest[2] / MIB / 1024.0,
                    fmt_num(c.bound("disk_reserve_floor_b")), fmt_num(c.bound("disk_reserve_fraction")), len(pool),
                    slope, len(rows), runway_text, fmt_num(runway_min)))


def info_line(label, rows, unit):
    """'label first -> last unit (min, max, slope per hour)' for [(hours, value, sample)] rows; never raises."""
    if not rows:
        return "%s: not recorded" % label
    values = [value for _hours, value, _sample in rows]
    slope = slope_of(rows)
    trend = ", slope %+.3f %s/h" % (slope, unit) if slope is not None else ""
    return "%s %.1f -> %.1f %s (min %.1f, max %.1f over %d point(s)%s)" % (label, values[0], values[-1], unit, min(values),
                                                                          max(values), len(values), trend)


def info_check(c, o, parts):
    """INFO series line: parts = [(label, field or getter, scale, unit)]. Prints numbers, never fails."""
    steady = c.steady_samples()
    if not steady:
        o.info("no steady-window samples to report (see SAMPLES-STEADY)")
        return
    o.info("; ".join(info_line(label, series(steady, field, scale), unit) for label, field, scale, unit in parts))


def swap_used_kb(sample):
    total, free = num_field(sample, "swap_total_kb"), num_field(sample, "swap_free_kb")
    if total is None or free is None:
        return None
    return max(total - free, 0.0)


@extra_check("RES-SWAP", "Swap use over the steady window (INFO; no swap threshold was adopted)", [])
def chk_res_swap(c, o):
    info_check(c, o, [("swap in use", swap_used_kb, 1.0 / 1024.0, "MiB"),
                      ("node VmSwap", "node_swap_kb", 1.0 / 1024.0, "MiB"),
                      ("cgroup swap", "cg_swap_current_b", 1.0 / MIB, "MiB")])


@extra_check("RES-DATA", "Data, cache, log and ARC directory sizes over the steady window (INFO)", [])
def chk_res_data(c, o):
    info_check(c, o, [("node data", "node_data_bytes", 1.0 / MIB, "MiB"),
                      ("release cache", "release_cache_bytes", 1.0 / MIB, "MiB"),
                      ("logs", "log_bytes", 1.0 / MIB, "MiB"),
                      ("legacy data", "legacy_data_bytes", 1.0 / MIB, "MiB"),
                      ("ARC dir", "arc_dir_bytes", 1.0 / MIB, "MiB")])


@extra_check("RES-HANDLES", "Open files and threads of the node over the steady window (INFO)", [])
def chk_res_handles(c, o):
    info_check(c, o, [("open fds", "node_fds", 1.0, "fds"), ("threads", "node_threads", 1.0, "threads")])


@extra_check("RES-CGROUP", "Cgroup memory peak, process high-water mark and CPU time over the steady window (INFO)", [])
def chk_res_cgroup(c, o):
    info_check(c, o, [("cgroup memory current", "cg_mem_current_b", 1.0 / MIB, "MiB"),
                      ("cgroup memory peak", "cg_mem_peak_b", 1.0 / MIB, "MiB"),
                      ("RSS high-water mark", "node_hwm_kb", 1.0 / 1024.0, "MiB"),
                      ("node CPU time", "node_cpu_s", 1.0, "s")])


@extra_check("RES-DOWNLOADS", "No unexpected, repeated or model-sized download and no cache or log duplication",
             ["samples", "events", "config"])
def chk_res_downloads(c, o):
    steady = steady_rows(c, o)
    if steady is None:
        return
    t0 = c.W["t0"]
    pool = [s for s in c.bridged if t0 is None or s["epoch"] >= t0 - EPS]
    if not pool:
        o.fail("UNPROVED: no bridged sample after t0")
        return

    def values(field):
        return [(s, num_field(s, field)) for s in pool if num_field(s, field) is not None]

    for field in STRUCTURAL_FIELDS:
        if not values(field):
            o.fail("UNPROVED: %s is not recorded in any bridged sample after t0" % field)
    problems = []
    for field in ("release_cache_bytes", "release_cache_files"):
        vals = values(field)
        if vals and len(set(v for _s, v in vals)) > 1:
            first, last = vals[0], vals[-1]
            changed = next(item for item in vals if item[1] != first[1])
            problems.append("%s changed from %s to %s at %s (%s at the end)"
                            % (field, fmt_num(first[1]), fmt_num(changed[1]), seq_at(c, changed[0]), fmt_num(last[1])))
    for field in ("models_bytes", "partial_files"):
        bad = [(s, v) for s, v in values(field) if v > 0]
        if bad:
            problems.append("%s is %s in %d sample(s), first at %s" % (field, fmt_num(bad[0][1]), len(bad),
                                                                      seq_at(c, bad[0][0])))
    vals = values("bridge_downloads")
    rises = [(b_sample, b - a) for (_a_sample, a), (b_sample, b) in zip(vals, vals[1:]) if b > a]
    if rises:
        problems.append("bridge_downloads rose by %s (a repeated download) first at %s"
                        % (fmt_num(sum(step for _s, step in rises)), seq_at(c, rises[0][0])))
    big = [(s, v) for s, v in values("largest_file_bytes") if v >= MODEL_SIZED_BYTES]
    if big:
        problems.append("a %.0f MiB file under the ARC directory (>= %d MiB is model-sized) at %s"
                        % (big[0][1] / MIB, MODEL_SIZED_BYTES // (1024 * 1024), seq_at(c, big[0][0])))
    if problems:
        o.fail(violation_summary(problems))
    # the runway rule extended to the node's own growth: log + node data
    runway_min = c.bound("disk_runway_h_min")
    combined = series(steady, lambda s: None if num_field(s, "log_bytes") is None or num_field(s, "node_data_bytes") is None
                      else num_field(s, "log_bytes") + num_field(s, "node_data_bytes"), 1.0 / MIB)
    growth = None
    if len(combined) < c.min_points():
        o.fail("UNPROVED: %d log + node data point(s) in the steady window, the growth slope needs >= %d"
               % (len(combined), c.min_points()))
    else:
        growth = slope_of(combined)
        if growth is None:
            o.fail("UNPROVED: the log + node data growth cannot be computed (no spread in time)")
    runway_text = "n/a"
    if growth is not None and runway_min is not None:
        last = steady[-1]
        free, reserve = num_field(last, "disk_free_b"), disk_reserve_b(c, last)
        if free is None or reserve is None:
            o.fail("UNPROVED: the last steady sample lacks disk_free_b or disk_total_b for the growth runway")
        elif growth > 0:
            runway = (free - reserve) / MIB / growth
            runway_text = "%.1f h" % runway
            if runway < runway_min:
                o.fail("log + node data grow %.3f MiB/h: runway %.1f h to the disk reserve < %s h"
                       % (growth, runway, fmt_num(runway_min)))
        else:
            runway_text = "unbounded (log + node data are not growing)"
    if not o.fails:
        cache = values("release_cache_bytes")
        o.ok("%d bridged sample(s) after t0: release cache constant at %.1f MiB in %s file(s), no partial file, no model "
             "file, no repeated download (bridge_downloads %s), largest file %.1f MiB (< %d MiB); log + node data grow "
             "%+.3f MiB/h, runway %s"
             % (len(pool), cache[0][1] / MIB, fmt_num(values("release_cache_files")[0][1]),
                fmt_num(values("bridge_downloads")[-1][1]), max(v for _s, v in values("largest_file_bytes")) / MIB,
                MODEL_SIZED_BYTES // (1024 * 1024), growth, runway_text))


@extra_check("RES-OOM", "No out-of-memory kill in the kernel journal of any boot of the soak",
             ["oom", "samples", "events"])
def chk_res_oom(c, o):
    scan = c.ev.get("oom")
    if scan is None:
        return          # missing or unreadable: reported as a source problem
    if scan.get("schema") != SCHEMA_OOM:
        o.fail("kernel-oom.json schema is %r, expected %r" % (scan.get("schema"), SCHEMA_OOM))
    scans = scan.get("scans")
    if not isinstance(scans, list) or not scans:
        error = scan.get("error")
        o.fail("UNPROVED: kernel-oom.json lists no boot scan%s" % ((" (scan error: %s)" % str(error)[:120]) if error else ""))
        return
    total, lines_seen, hits = 0, 0, []
    for position, item in enumerate(scans):
        if not isinstance(item, dict) or not is_int(item.get("count")) or item["count"] < 0:
            o.fail("UNPROVED: scan #%d of kernel-oom.json has no valid count" % position)
            continue
        kernel_lines = item.get("kernel_lines")
        if not is_int(kernel_lines) or kernel_lines <= 0:
            o.fail("UNPROVED: scan #%d (boot %s) read %s kernel journal line(s); an empty scan proves nothing"
                   % (position, item.get("boot"), kernel_lines))
        else:
            lines_seen += kernel_lines
        total += item["count"]
        if item["count"] > 0:
            sample_line = next((str(x)[:140] for x in item.get("lines") or [] if isinstance(x, str)), "")
            hits.append("boot %s: %d line(s), first: %s" % (item.get("boot"), item["count"], sample_line))
    if scan.get("total") != total:
        o.fail("kernel-oom.json total %r disagrees with the per-boot counts (%d)" % (scan.get("total"), total))
    t0 = c.W["t0"]
    boots = set(s["boot_id"] for s in c.samples if t0 is None or s["epoch"] >= t0 - EPS)
    if len(scans) < len(boots):
        o.fail("UNPROVED: %d boot(s) scanned but the samples after t0 show %d distinct boot id(s)" % (len(scans), len(boots)))
    if hits:
        o.fail("%d out-of-memory line(s): %s" % (total, "; ".join(hits[:3])))
    event = c.first_event("kernel_oom_scan")
    if event is not None:
        claimed = event.get("detail", {}).get("total")
        if claimed is not None and claimed != scan.get("total"):
            o.fail("event kernel_oom_scan says total %r but kernel-oom.json says %r" % (claimed, scan.get("total")))
        end_event = c.first_event("steady_end")
        if end_event is not None and event.get("seq", 0) > 0 and end_event.get("seq", 0) > 0 \
                and event["seq"] < end_event["seq"]:
            o.fail("the kernel OOM scan (event seq %d) ran before steady_end (seq %d): it does not cover the whole window"
                   % (event["seq"], end_event["seq"]))
    if not o.fails:
        o.ok("no OOM line in %d kernel journal line(s) over %d boot scan(s) (samples show %d boot id(s))"
             % (lines_seen, len(scans), len(boots)))


@extra_check("VM-MEMORY", "The soak VM was configured with 4096 MB, the canary the adopted bounds were written for",
             ["events", "samples", "config"])
def chk_vm_memory(c, o):
    events = c.events_by_name.get("vm_memory", [])
    if not events:
        o.fail("UNPROVED: event vm_memory is missing: the VM memory size is not recorded")
        return
    if len(events) > 1:
        o.fail("event vm_memory occurs %d times, expected exactly once" % len(events))
    detail = events[0].get("detail", {})
    configured = detail.get("configured_mb")
    if not is_num(configured):
        o.fail("event vm_memory has no numeric configured_mb (got %r)" % (configured,))
        return
    if configured != CANARY_VM_MB:
        o.fail("the VM was configured with %s MB, the adopted bounds were written for the %d MB canary"
               % (fmt_num(configured), CANARY_VM_MB))
    claimed = c.cfg.get("vm_memory_mb")
    if claimed is not None and claimed != configured:
        o.fail("config-effective.json says vm_memory_mb %s but the vm_memory event says %s" % (fmt_num(claimed), fmt_num(configured)))
    reported = []
    if is_num(detail.get("mem_total_kb")):
        reported.append(("event", float(detail["mem_total_kb"])))
    seen = [num_field(s, "mem_total_kb") for s in c.bridged if num_field(s, "mem_total_kb") is not None]
    if seen:
        reported.append(("samples", max(seen)))
    for where, kilobytes in reported:
        mib = kilobytes / 1024.0
        if mib > configured + 0.5 or mib < VM_MEMTOTAL_MIN_FRACTION * configured:
            o.fail("the guest reports MemTotal %.0f MiB (%s) which does not fit a %s MB VM" % (mib, where, fmt_num(configured)))
    if not o.fails:
        o.ok("vm_memory event: configured %s MB%s" % (fmt_num(configured), "; guest MemTotal " + ", ".join(
            "%.0f MiB (%s)" % (kb / 1024.0, where) for where, kb in reported) if reported else
            "; guest MemTotal not recorded"))


def norm_sha(value):
    """A sha256 digest with an optional 'sha256:' prefix, lowercase, or None if it is not one."""
    if not isinstance(value, str):
        return None
    value = value.strip().lower()
    if value.startswith("sha256:"):
        value = value[len("sha256:"):]
    return value if is_hex64(value) else None


@extra_check("BINDING-PRIOR-RUN", "The supplement is bound to the same published bytes and baseline as Wave 0 run "
             "37750760170", ["binding", "events", "samples", "config"])
def chk_binding(c, o):
    """binding.json: prior_run (the committed claim) against this_run (MEASURED in this run). Equality is recomputed
    here; the file's own equal / all_equal / disclosed_changes keys are not trusted."""
    document = c.ev.get("binding")
    if document is None:
        return          # missing or unreadable: reported as a source problem
    if document.get("schema") != SCHEMA_BINDING:
        o.fail("binding.json schema is %r, expected %r" % (document.get("schema"), SCHEMA_BINDING))
    this, prior = document.get("this_run"), document.get("prior_run")
    if not isinstance(this, dict) or not isinstance(prior, dict):
        o.fail("binding.json needs this_run and prior_run objects")
        return
    pairs = []
    for field in BINDING_SHA_FIELDS:
        a, b = norm_sha(this.get(field)), norm_sha(prior.get(field))
        if a is None or b is None:
            o.fail("%s: this_run=%r prior_run=%r are not both sha256 digests" % (field, this.get(field), prior.get(field)))
        elif a != b:
            o.fail("%s differs: this run %s... prior run %s..." % (field, a[:16], b[:16]))
            pairs.append("%s %s != %s" % (field, a[:16], b[:16]))
        else:
            pairs.append("%s %s == %s" % (field, a[:16], b[:16]))
    units_this, units_prior = this.get("units"), prior.get("units")
    if not isinstance(units_this, dict) or not isinstance(units_prior, dict):
        o.fail("units: this_run and prior_run must both carry the three unit file digests")
    else:
        for unit in UNIT_FILES:
            a, b = norm_sha(units_this.get(unit)), norm_sha(units_prior.get(unit))
            if a is None or b is None:
                o.fail("units.%s: this_run=%r prior_run=%r are not both sha256 digests" % (unit, units_this.get(unit),
                                                                                         units_prior.get(unit)))
            elif a != b:
                o.fail("units.%s differs: this run %s... prior run %s..." % (unit, a[:16], b[:16]))
            else:
                pairs.append("units.%s %s == %s" % (unit, a[:16], b[:16]))
        if set(units_this) != set(units_prior):
            o.fail("units: this_run lists %s, prior_run lists %s" % (sorted(units_this), sorted(units_prior)))
    baseline_this, baseline_prior = this.get("baseline_result"), prior.get("baseline_result")
    if not (isinstance(baseline_this, str) and baseline_this.strip() and isinstance(baseline_prior, str)
            and baseline_prior.strip()):
        o.fail("baseline_result: this_run=%r prior_run=%r are not both non-empty lines" % (baseline_this, baseline_prior))
    elif baseline_this != baseline_prior:
        o.fail("baseline_result differs: this run %r, prior run %r" % (baseline_this[:80], baseline_prior[:80]))
    else:
        pairs.append("baseline_result equal")
    if prior.get("run_id") != PRIOR_RUN_ID:
        o.fail("prior_run.run_id is %r, expected %d" % (prior.get("run_id"), PRIOR_RUN_ID))
    if this.get("vm_memory_mb") != CANARY_VM_MB:
        o.fail("this_run.vm_memory_mb is %r, expected %d" % (this.get("vm_memory_mb"), CANARY_VM_MB))
    if prior.get("vm_memory_mb") != PRIOR_VM_MEMORY_MB:
        o.fail("prior_run.vm_memory_mb is %r, expected %d" % (prior.get("vm_memory_mb"), PRIOR_VM_MEMORY_MB))
    configured = c.cfg.get("vm_memory_mb")
    if configured is None:
        o.fail("UNPROVED: config-effective.json has no vm_memory_mb to compare this_run.vm_memory_mb with")
    elif configured != this.get("vm_memory_mb"):
        o.fail("config-effective.json vm_memory_mb %s differs from this_run.vm_memory_mb %r"
               % (fmt_num(configured), this.get("vm_memory_mb")))
    # the measured bytes must be the bytes the config pinned and the node that ran
    launcher, node = norm_sha(this.get("launcher_sha256")), norm_sha(this.get("node_sha256"))
    if c.expected_launcher is None:
        o.fail("UNPROVED: config-effective.json has no expected_launcher_sha256")
    elif launcher is not None and launcher != c.expected_launcher:
        o.fail("this_run.launcher_sha256 differs from expected_launcher_sha256 of config-effective.json")
    if c.node_sha is None:
        o.fail("UNPROVED: config-effective.json has no node_sha256")
    elif node is not None and node != c.node_sha:
        o.fail("this_run.node_sha256 differs from node_sha256 of config-effective.json")
    steady = c.steady_samples() or []
    if steady:
        first = steady[0]
        if launcher is not None and first.get("launcher_sha256") != launcher:
            o.fail("this_run.launcher_sha256 differs from the launcher the node ran (%s)" % seq_at(c, first))
        if node is not None and first.get("node_exe_sha256") != node:
            o.fail("this_run.node_sha256 differs from the node binary that ran (%s)" % seq_at(c, first))
    else:
        o.fail("UNPROVED: no steady-window sample to compare this_run with the bytes that ran")
    events = c.events_by_name.get("vm_memory", [])
    if events and events[0].get("detail", {}).get("configured_mb") != this.get("vm_memory_mb"):
        o.fail("this_run.vm_memory_mb differs from the vm_memory event")
    if not o.fails:
        digest = prior.get("evidence_artifact_digest")
        o.ok("bound to run %d%s: %s; disclosed: VM memory changed %s -> %s MiB"
             % (PRIOR_RUN_ID, (" (evidence artifact %s)" % digest) if isinstance(digest, str) else "", "; ".join(pairs),
                fmt_num(prior.get("vm_memory_mb")), fmt_num(this.get("vm_memory_mb"))))


def parse_http_date(value):
    """Epoch seconds of an HTTP Date header value, or None if it does not parse (no clock is read)."""
    try:
        moment = email.utils.parsedate_to_datetime(value)
    except (TypeError, ValueError, IndexError, OverflowError):
        return None
    if moment is None:
        return None
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=datetime.timezone.utc)
    return moment.timestamp()


@extra_check("FRESH-AGE", "Every steady-window sample shows a successful registration younger than 90 s",
             ["samples", "events", "config"])
def chk_fresh_age(c, o):
    steady = steady_rows(c, o)
    if steady is None:
        return
    problems, ages, early = [], [], 0
    for sample in steady:
        where = seq_at(c, sample)
        stamp, age = sample.get("last_registration_unix_ms"), sample.get("registration_age_s")
        if not is_int(stamp) or stamp <= 0:
            problems.append("%s: UNPROVED: last_registration_unix_ms is %s" % (where, "missing" if stamp is None else
                                                                               "invalid (%r)" % (stamp,)))
            continue
        if not is_num(age):
            problems.append("%s: UNPROVED: registration_age_s is %s" % (where, "missing" if age is None else
                                                                        "invalid (%r)" % (age,)))
            continue
        ages.append(age)
        if age < -FRESH_AGE_TOLERANCE_S:
            problems.append("%s: registration_age_s %.3f is negative" % (where, age))
        elif age < 0:
            early += 1          # a heartbeat landed between the sample's start epoch and its status read
        elif age > FRESH_AGE_MAX_S:
            problems.append("%s: registration is stale, age %.1f s > %d s" % (where, age, FRESH_AGE_MAX_S))
        derived = sample["epoch"] - stamp / 1000.0
        if abs(age - derived) > FRESH_AGE_TOLERANCE_S:
            problems.append("%s: registration_age_s %.3f disagrees with epoch - last_registration_unix_ms/1000 = %.3f"
                            % (where, age, derived))
    if problems:
        o.fail(violation_summary(problems))
    else:
        o.ok("%d steady sample(s) checked, registration age max %.1f s (limit %d s), every age consistent with "
             "epoch - last_registration_unix_ms within %.1f s%s"
             % (len(steady), max(ages), FRESH_AGE_MAX_S, FRESH_AGE_TOLERANCE_S,
                (", %d age(s) between -%.1f and 0 s (a heartbeat landed after the sample started)"
                 % (early, FRESH_AGE_TOLERANCE_S)) if early else ""))


@extra_check("FRESH-DISTINCT", "The node's own registration timestamp keeps advancing: no frozen value, no gap above 90 s",
             ["heartbeats", "samples", "events", "config"])
def chk_fresh_distinct(c, o):
    window = c.W
    start = window["steady_begin"] if window["steady_begin"] is not None else window["start"]
    end = window["last_sample"]
    if start is None or end is None:
        o.fail("UNPROVED: steady window undefined: " + c.window_problem())
        return
    polls = sorted(c.ev["heartbeats"], key=lambda item: (item["obs_epoch"], item["_line"]))
    inside = [p for p in polls if start - EPS <= p["obs_epoch"] <= end + EPS]
    if not inside:
        o.fail("UNPROVED: no heartbeat poll inside the steady window")
        return
    problems = []
    # (a) poll coverage
    if inside[0]["obs_epoch"] > start + POLL_EDGE_S + EPS:
        problems.append("UNPROVED: first poll is %.1f s after the window start (limit %d s)"
                        % (inside[0]["obs_epoch"] - start, POLL_EDGE_S))
    if inside[-1]["obs_epoch"] < end - POLL_EDGE_S - EPS:
        problems.append("UNPROVED: last poll is %.1f s before the window end (limit %d s)"
                        % (end - inside[-1]["obs_epoch"], POLL_EDGE_S))
    widest = max(((b["obs_epoch"] - a["obs_epoch"], a) for a, b in zip(inside, inside[1:])), default=(0.0, inside[0]),
                 key=lambda pair: pair[0])
    if widest[0] > POLL_GAP_MAX_S + EPS:
        problems.append("UNPROVED: poll gap can hide a skipped success: %.1f s after %s (limit %d s)"
                        % (widest[0], c.rel(widest[1]["obs_epoch"]), POLL_GAP_MAX_S))
    # (b) distinct timestamps: strictly increasing, consecutive ones at most 90 s apart. The chain starts at the last
    # value seen before the window, so a step backwards across the start boundary is caught too.
    before = [p for p in polls if p["obs_epoch"] <= start + EPS]
    t_before = next((p["ts_ms"] for p in reversed(before) if is_int(p.get("ts_ms"))), None)
    observed = [(p["obs_epoch"], p["ts_ms"]) for p in inside if is_int(p.get("ts_ms"))]
    distinct = []
    for moment, stamp in observed:
        if not distinct or stamp != distinct[-1][1]:
            distinct.append((moment, stamp))
    chain = list(distinct)
    if t_before is not None and (not chain or chain[0][1] != t_before):
        chain.insert(0, (start, t_before))
    for (_m1, s1), (m2, s2) in zip(chain, chain[1:]):
        if s2 <= s1:
            problems.append("timestamp went backwards or repeated an older value (%d after %d) at %s" % (s2, s1, c.rel(m2)))
    gaps = [s2 - s1 for (_m1, s1), (_m2, s2) in zip(chain, chain[1:]) if s2 > s1]
    if gaps and max(gaps) > FRESH_MS:
        problems.append("gap of %d ms between consecutive distinct timestamps (limit %d ms)" % (max(gaps), FRESH_MS))
    if not distinct:
        problems.append("UNPROVED: no poll inside the window returned a timestamp")
    # (c) boundaries
    if not any(p["obs_epoch"] >= start - POLL_BEFORE_S - EPS for p in before):
        problems.append("UNPROVED: no poll within %d s before the window start" % POLL_BEFORE_S)
    t_after = next((p["ts_ms"] for p in reversed(polls) if p["obs_epoch"] <= end + EPS and is_int(p.get("ts_ms"))), None)
    start_age = end_age = bridge = None
    if t_before is None:
        problems.append("UNPROVED: no timestamp observed at or before the window start")
    else:
        start_age = start - t_before / 1000.0
        if start_age > FRESH_AGE_MAX_S + EPS:
            problems.append("the last timestamp before the window start is %.1f s old (limit %d s)" % (start_age, FRESH_AGE_MAX_S))
        first_new = next((p["ts_ms"] for p in polls if p["obs_epoch"] > start + EPS and is_int(p.get("ts_ms"))
                          and p["ts_ms"] > t_before), None)
        if first_new is None:
            problems.append("no new timestamp after the window start")
        else:
            bridge = first_new - t_before
            if bridge > FRESH_MS:
                problems.append("%d ms between the last timestamp before the window and the first one after it (limit %d ms)"
                                % (bridge, FRESH_MS))
    if t_after is None:
        problems.append("UNPROVED: no timestamp observed by the window end")
    else:
        end_age = end - t_after / 1000.0
        if end_age > FRESH_AGE_MAX_S + EPS:
            problems.append("the last timestamp by the window end is %.1f s old (limit %d s)" % (end_age, FRESH_AGE_MAX_S))
    # (e) a frozen or sparse value is not enough
    needed = DISTINCT_MIN_FRACTION * (end - start) / HEARTBEAT_ROUND_S
    if len(distinct) < needed - 1e-9:
        problems.append("only %d distinct timestamps, at least %.0f needed (0.5 x window / 15 s)" % (len(distinct), needed))
    failed_polls = sum(1 for p in inside if not is_int(p.get("ts_ms")))
    if problems:
        o.fail(violation_summary(problems, 6))
    else:
        o.ok("%d poll(s) in the window (%d without a timestamp), max poll gap %.1f s; %d distinct timestamp(s) (need >= %.0f), "
             "max gap %d ms, median gap %d ms (limit %d ms); age at window start %.1f s, at window end %.1f s, bridge over "
             "the start boundary %d ms"
             % (len(inside), failed_polls, widest[0], len(distinct), needed, max(gaps) if gaps else 0,
                median(gaps) if gaps else 0, FRESH_MS, start_age, end_age, bridge))


@extra_check("PUBLIC-FRESHNESS", "The public scoreboard lists the node under its privacy-safe name at least every 60 s",
             ["scoreboard", "events", "config"])
def chk_public_freshness(c, o):
    interval = c.cfg["scoreboard_interval_s"]
    if interval > SCOREBOARD_INTERVAL_MAX_S:
        o.fail("scoreboard_interval_s is %s s in config-effective.json, above the %d s ceiling (30..60 s accepted)"
               % (fmt_num(interval), SCOREBOARD_INTERVAL_MAX_S))
    if c.t0_address is None:
        o.fail("UNPROVED: t0_bridged_healthy.detail.address is missing, the probed address cannot be checked")
        return
    window = c.host_window()
    if window is None:
        o.fail("UNPROVED: the steady_begin and steady_end (or final_stop_begin) events are missing, the probe window "
               "cannot be placed")
        return
    start, end = window
    probes = sorted(c.ev["scoreboard"], key=lambda p: (p["host_epoch"], p["probe"]))
    inside = [p for p in probes if start - EPS <= p["host_epoch"] <= end + EPS]
    if not inside:
        o.fail("UNPROVED: no scoreboard probe inside the steady window (%d probe(s) in the file)" % len(probes))
        return
    problems = []
    if inside[0]["host_epoch"] > start + SCOREBOARD_EDGE_S + EPS:
        problems.append("UNPROVED: scoreboard coverage gap: first probe is %.1f s after the window start (limit %d s)"
                        % (inside[0]["host_epoch"] - start, SCOREBOARD_EDGE_S))
    if inside[-1]["host_epoch"] < end - SCOREBOARD_EDGE_S - EPS:
        problems.append("UNPROVED: scoreboard coverage gap: last probe is %.1f s before the window end (limit %d s)"
                        % (end - inside[-1]["host_epoch"], SCOREBOARD_EDGE_S))
    widest = max(((b["host_epoch"] - a["host_epoch"], a) for a, b in zip(inside, inside[1:])), default=(0.0, inside[0]),
                 key=lambda pair: pair[0])
    if widest[0] > SCOREBOARD_GAP_MAX_S + EPS:
        problems.append("UNPROVED: scoreboard coverage gap: %.1f s between probe %d and the next (limit %d s)"
                        % (widest[0], widest[1]["probe"], SCOREBOARD_GAP_MAX_S))
    numbers = [p["probe"] for p in inside]
    if len(set(numbers)) != len(numbers):
        problems.append("probe numbers repeat inside the window")
    expected_name = "node-" + c.t0_address[:8]
    expected_worker = "0x" + c.t0_address
    matrix = {}
    found_counts, skews = [], []
    for probe in inside:
        label = "probe %d" % probe["probe"]
        if norm_addr(probe["address"]) != c.t0_address:
            problems.append("%s looked up address %s..., not the node's %s..." % (label, norm_addr(probe["address"])[:12],
                                                                             c.t0_address[:12]))
        results = probe["results"]
        if not results:
            problems.append("%s probed no origin" % label)
            continue
        found = [r for r in results if r["found"]]
        found_counts.append(len(found))
        if probe["found_count"] != len(found):
            problems.append("%s says found_count %d but %d result(s) are found" % (label, probe["found_count"], len(found)))
        if not found:
            problems.append("%s: no origin lists the node (0 of %d)" % (label, len(results)))
        for result in results:
            cell_ = matrix.setdefault((result["origin"], result.get("origin_sha8", "")), [0, 0])
            cell_[1] += 1
            if result["found"]:
                cell_[0] += 1
                name, worker = result.get("name"), result.get("worker_id")
                row = result.get("row") if isinstance(result.get("row"), dict) else {}
                if name != expected_name:
                    problems.append("%s origin %d serves name %r, expected %s" % (label, result["origin"], name, expected_name))
                if not isinstance(worker, str) or worker.lower() != expected_worker:
                    problems.append("%s origin %d serves worker_id %r, expected %s" % (label, result["origin"], worker,
                                                                                         expected_worker))
                if row.get("name") is not None and row.get("name") != expected_name:
                    problems.append("%s origin %d row name is %r, expected %s" % (label, result["origin"], row.get("name"),
                                                                                  expected_name))
                if row.get("worker_id") is not None and str(row.get("worker_id")).lower() != expected_worker:
                    problems.append("%s origin %d row worker_id is %r, expected %s" % (label, result["origin"],
                                                                                       row.get("worker_id"), expected_worker))
            stamp = parse_http_date(result["server_date"]) if isinstance(result.get("server_date"), str) else None
            if stamp is not None:
                skews.append(abs(stamp - probe["host_epoch"]))
                if skews[-1] > SCOREBOARD_DATE_SKEW_S:
                    problems.append("%s origin %d: server Date differs from the host clock by %.0f s (limit %d s)"
                                    % (label, result["origin"], skews[-1], SCOREBOARD_DATE_SKEW_S))
    if problems:
        o.fail(violation_summary(problems, 6))
    else:
        grid = ", ".join("origin %s%s %d/%d" % (index, (" " + sha) if sha else "", hits, total)
                         for (index, sha), (hits, total) in sorted(matrix.items()))
        o.ok("%d probe(s) in the steady window, first %.0f s after its start, last %.0f s before its end, max gap %.0f s "
             "(limit %d s), every probe lists the node on >= 1 origin (min %d, max %d), name %s and worker_id on every "
             "origin that served it; matrix INFO: %s; server Date parsed in %d result(s), max skew %.0f s"
             % (len(inside), inside[0]["host_epoch"] - start, end - inside[-1]["host_epoch"], widest[0],
                SCOREBOARD_GAP_MAX_S, min(found_counts), max(found_counts), expected_name, grid, len(skews),
                max(skews) if skews else 0.0))


# --------------------------------------------------------------------------------------------------------------------
# evaluation, verdict, outputs
# --------------------------------------------------------------------------------------------------------------------

def live_records(ev):
    """The orchestrator's own checks, merged verbatim after the computed ones."""
    return [{"id": record["id"], "title": record.get("title", ""), "result": record["result"],
             "detail": record.get("detail", "")} for record in ev.get("live", [])]


SKIP_REASON = {
    "resources": "resources_required is false in config-effective.json: the resource, download, OOM and VM-memory "
                 "criteria (ARC-83 E) were not judged",
    "freshness": "scoreboard_required is false in config-effective.json: the local and public freshness criteria "
                 "(ARC-83 D) were not judged",
}
OMIT_REASON = {
    "resources": "config-effective.json has no resources_required key (evidence predates the ARC-83 resource contract): "
                 "the resource, download, OOM and VM-memory criteria (ARC-83 E) are not judged",
    "freshness": "config-effective.json has no scoreboard_required key (evidence predates the ARC-83 freshness contract): "
                 "the local and public freshness criteria (ARC-83 D) are not judged",
}


def skip_row(check_id, title, reason):
    return {"id": check_id, "title": title, "result": "SKIP", "detail": reason}


def run_check(c, check_id, title, needs, func):
    outcome = Outcome()
    for problem in c.source_problems(needs):
        outcome.fail(problem)
    try:
        func(c, outcome)
    except Exception as exc:  # defensive: a bug here must show up as a FAIL, never as a crash
        outcome.fail("internal error in %s: %r" % (check_id, exc))
    result, detail = outcome.finish()
    binding = DETAIL_BINDINGS.get(check_id)
    if binding is not None and binding not in detail:
        detail = detail + " " + binding
    return {"id": check_id, "title": title, "result": result, "detail": detail}


def not_judged(cfg):
    """{check id: reason} for the extra checks this config does not judge (omitted, switched off, or another profile's)."""
    reasons = {}
    for check_id in EXTRA_IDS:
        group = EXTRA_GROUP[check_id]
        mode = extra_mode(cfg, group)
        if mode == "omit":
            reasons[check_id] = OMIT_REASON[group]
        elif mode == "skip":
            reasons[check_id] = SKIP_REASON[group]
        elif check_id in PROFILE_ONLY and cfg["profile"] != PROFILE_ONLY[check_id]:
            reasons[check_id] = "applies to profile %s only" % PROFILE_ONLY[check_id]
    return reasons


def required_ids(cfg):
    """The computed check ids that must PASS (INFO_OK_IDS may be INFO) for this profile and these config switches."""
    ids = list(COMPUTED_IDS)
    if not cfg["battery"]:
        ids = [i for i in ids if i not in BATTERY_ONLY_IDS]
    for check_id in EXTRA_IDS:
        if check_id in PROFILE_ONLY and cfg["profile"] != PROFILE_ONLY[check_id]:
            continue
        if extra_mode(cfg, EXTRA_GROUP[check_id]) == "run":
            ids.append(check_id)
    return ids


def analyze(ev, bounds_override=None):
    """Return (checks, windows, effective_config). Never raises on bad evidence.

    bounds_override: optional {bound key: value} (the --bounds-file) laid over the config's resource_bounds."""
    try:
        c = Ctx(ev) if bounds_override is None else Ctx(ev, bounds_override)
    except Exception as exc:  # defensive: report, never crash
        try:
            cfg = effective_config(ev.get("config"))[0]
        except Exception:
            cfg = effective_config(None)[0]
        broken = [{"id": check_id, "title": title, "result": "FAIL",
                   "detail": "internal error building the evaluation context: %r" % (exc,)}
                  for check_id, title, _needs, _func in CHECKS]
        broken += [{"id": check_id, "title": title, "result": "FAIL",
                    "detail": "internal error building the evaluation context: %r" % (exc,)}
                   for check_id, title, _needs, _func in EXTRA_CHECKS
                   if extra_mode(cfg, EXTRA_GROUP[check_id]) == "run"
                   and not (check_id in PROFILE_ONLY and cfg["profile"] != PROFILE_ONLY[check_id])]
        return broken + list(live_records(ev)), {"problems": ["internal error"], "forced": []}, cfg
    results = []
    for check_id, title, needs, func in CHECKS:
        if not c.cfg["battery"] and check_id in BATTERY_ONLY_IDS:
            results.append(skip_row(check_id, title, COVERED_BY_RUN))
            continue
        results.append(run_check(c, check_id, title, needs, func))
    for check_id, title, needs, func in EXTRA_CHECKS:
        group = EXTRA_GROUP[check_id]
        mode = extra_mode(c.cfg, group)
        if mode == "omit":
            continue
        if mode == "skip":
            results.append(skip_row(check_id, title, SKIP_REASON[group]))
        elif check_id in PROFILE_ONLY and c.cfg["profile"] != PROFILE_ONLY[check_id]:
            results.append(skip_row(check_id, title, "applies to profile %s only" % PROFILE_ONLY[check_id]))
        else:
            results.append(run_check(c, check_id, title, needs, func))
    results.extend(live_records(ev))
    if not c.cfg["battery"]:
        recorded = set(r["id"] for r in ev.get("live", []))
        for live_id in BATTERY_LIVE_IDS:
            if live_id not in recorded:
                results.append(skip_row(live_id, "battery live check", COVERED_BY_RUN))
    if extra_mode(c.cfg, "resources") == "run":
        c.W["resource_bounds"] = {"source": c.bounds_source, "values": dict(c.bounds),
                                  "differs_from_adopted": list(c.bounds_differ), "adopted": dict(ADOPTED_BOUNDS)}
    return results, c.W, c.cfg


def evaluate(ev):
    return analyze(ev)[0]


SUPPLEMENT_STATEMENT = ("SUPPLEMENT: combined evidence with Wave 0 run 37750760170 (four-hour forced battery and real reboot "
                        "there; resources, public freshness and kernel OOM here, on a 4096 MiB VM). Not a re-instrumentation "
                        "of that run. A behaviour-changing fix or a new failure requires reassessment.")


def build_statement(verdict, profile, launcher_source, tag, launcher_sha, failed, unmet, unjudged=None):
    parts = []
    if profile == "smoke":
        parts.append("SMOKE PROFILE: this run does NOT satisfy Astra's thresholds (>= 4 h total, >= 2 h uninterrupted "
                     "steady state after the last forced event, >= 121 samples, >= 10 min healthy after the reboot); "
                     "it only exercises the machinery.")
    if profile == "resources":
        parts.append(SUPPLEMENT_STATEMENT)
    if launcher_source == "artifact":
        parts.append("LAUNCHER SOURCE artifact: this run consumed the Stage A artifact bytes by a local replay of "
                     "canary-consume.sh (rehearsal) and is NOT the post-G0 published-tag Wave 0.")
    if verdict == "WAVE0_PASS":
        if launcher_source == "published":
            parts.append("Wave 0 criteria met on the published tag %s (launcher sha256 %s)." % (tag, launcher_sha))
        else:
            parts.append("Every full-profile criterion was met, but the launcher was not consumed from the published tag.")
    elif verdict == "SUPPLEMENT_PASS":
        if launcher_source == "published":
            parts.append("Supplement criteria met on the published tag %s (launcher sha256 %s); this is not a Wave 0 pass."
                         % (tag, launcher_sha))
        else:
            parts.append("Every supplement criterion was met, but the launcher was not consumed from the published tag; "
                         "this is not a Wave 0 pass.")
    elif verdict in ("WAVE0_FAIL", "SMOKE_FAIL", "SUPPLEMENT_FAIL"):
        ids = list(failed) + [i for i in unmet if i not in failed]
        parts.append("Criteria NOT met; failed or unmet checks: %s." % (", ".join(ids) if ids else "none recorded"))
    if unjudged:
        parts.append("NOT JUDGED: %s. ARC-83 criteria D and E are not evidenced by this run for these items."
                     % "; ".join(unjudged))
    return " ".join(parts)


def verdict_for(checks, config, windows=None):
    """Combine the checks into verdict.json. `config` is the raw config dict (or None)."""
    cfg, _issues, _lowered = effective_config(config)
    document = config if isinstance(config, dict) else {}
    profile = cfg["profile"]
    counts = dict((k, 0) for k in ("PASS", "FAIL", "INFO", "SKIP"))
    for item in checks:
        counts[item["result"]] = counts.get(item["result"], 0) + 1
    failed = [i["id"] for i in checks if i["result"] == "FAIL"]
    first_by_id = {}
    for item in checks:
        first_by_id.setdefault(item["id"], item)
    unmet = []
    for check_id in required_ids(cfg):
        item = first_by_id.get(check_id)
        if item is None:
            unmet.append(check_id)
        elif item["result"] == "PASS":
            continue
        elif item["result"] == "INFO" and check_id in INFO_OK_IDS:
            continue
        elif item["result"] != "FAIL":
            unmet.append(check_id)
    passed = not failed and not unmet
    if profile == "smoke":
        verdict = "SMOKE_PASS" if passed else "SMOKE_FAIL"
    elif profile == "resources":
        verdict = "SUPPLEMENT_PASS" if passed else "SUPPLEMENT_FAIL"
    else:
        verdict = "WAVE0_PASS" if passed else "WAVE0_FAIL"
    launcher_source = document.get("launcher_source")
    window = windows or {}
    not_applicable = not_judged(cfg)
    unjudged = []
    for group, label, key in (("resources", "resource, download, OOM and VM-memory checks", "resources_required"),
                              ("freshness", "local and public freshness checks", "scoreboard_required")):
        mode = extra_mode(cfg, group)
        if mode == "omit":
            unjudged.append("%s: the evidence predates the ARC-83 contract (config-effective.json has no %s key)"
                            % (label, key))
        elif mode == "skip":
            unjudged.append("%s: config-effective.json says %s=false" % (label, key))
    result = {
        "schema": SCHEMA_VERDICT,
        "verdict": verdict,
        "profile": profile,
        "launcher_source": launcher_source,
        "tag": document.get("tag"),
        "expected_launcher_sha256": document.get("expected_launcher_sha256"),
        "live_network": document.get("live_network"),
        "counts": counts,
        "failed_ids": failed,
        "unmet_ids": unmet,
        "is_post_g0_wave0": bool(verdict == "WAVE0_PASS" and launcher_source == "published"),
        "windows": {
            "t0": window.get("t0"),
            "last_forced": window.get("last_forced"),
            "last_forced_end": window.get("last_forced_end"),
            "steady_begin": window.get("steady_begin"),
            "steady_window_start": window.get("start"),
            "end": window.get("end"),
            "steady_s": window.get("steady_s"),
            "total_s": window.get("total_s"),
        },
        "not_applicable": not_applicable,
        "resource_bounds": window.get("resource_bounds"),
        "statement": build_statement(verdict, profile, launcher_source, document.get("tag"),
                                     document.get("expected_launcher_sha256"), failed, unmet, unjudged),
    }
    return result


def cell(text):
    return str(text).replace("|", "\\|").replace("\r", " ").replace("\n", " ")


def render_report(checks, verdict):
    lines = ["# Wave 0 Stage B evaluation", "",
             "**Verdict: %s**" % verdict["verdict"], "",
             "- profile: %s" % verdict["profile"],
             "- launcher source: %s" % verdict["launcher_source"],
             "- tag: %s" % verdict["tag"],
             "- expected launcher sha256: %s" % verdict["expected_launcher_sha256"],
             "- live network: %s" % verdict["live_network"],
             "- counts: %s" % ", ".join("%s %d" % (k, verdict["counts"][k]) for k in ("PASS", "FAIL", "INFO", "SKIP")),
             "- failed ids: %s" % (", ".join(verdict["failed_ids"]) or "none"),
             "- unmet ids: %s" % (", ".join(verdict["unmet_ids"]) or "none"),
             "- post-G0 Wave 0: %s" % ("yes" if verdict["is_post_g0_wave0"] else "no"),
             "", verdict["statement"], "", "## Windows", ""]
    window = verdict["windows"]
    for key in ("t0", "last_forced_end", "steady_begin", "steady_window_start", "end"):
        lines.append("- %s: %s (%s)" % (key, window.get(key), fmt_ts(window.get(key))))
    lines.append("- last forced event: %s" % window.get("last_forced"))
    lines.append("- steady: %s (%s)" % (window.get("steady_s"), fmt_dur(window.get("steady_s"))))
    lines.append("- total: %s (%s)" % (window.get("total_s"), fmt_dur(window.get("total_s"))))
    bounds = verdict.get("resource_bounds")
    if bounds:
        lines += ["", "## Resource bounds", "", "- source: %s" % bounds.get("source"),
                  "- differs from the adopted numbers: %s" % (", ".join(bounds.get("differs_from_adopted") or []) or "no")]
        for key in BOUND_ORDER:
            lines.append("- %s: %s (adopted %s)" % (key, bounds.get("values", {}).get(key), bounds.get("adopted", {}).get(key)))
    unjudged = verdict.get("not_applicable")
    if unjudged:
        lines += ["", "## Not judged", ""]
        for check_id in sorted(unjudged):
            lines.append("- %s: %s" % (check_id, unjudged[check_id]))
    lines += ["", "## Checks", "", "| ID | Result | Title | Detail |", "|---|---|---|---|"]
    for item in checks:
        lines.append("| %s | %s | %s | %s |" % (cell(item["id"]), item["result"], cell(item["title"]), cell(item["detail"])))
    lines.append("")
    return "\n".join(lines)


def dump_json(value):
    return json.dumps(value, indent=2, sort_keys=True) + "\n"


def write_outputs(out_dir, checks, verdict):
    os.makedirs(out_dir, exist_ok=True)
    payloads = (("checks.json", dump_json(checks)), ("verdict.json", dump_json(verdict)),
                ("REPORT.md", render_report(checks, verdict)))
    for name, text in payloads:
        with open(os.path.join(out_dir, name), "w", encoding="utf-8") as handle:
            handle.write(text)


def print_table(checks, verdict, stream):
    width = max([len(i["id"]) for i in checks] + [10])
    for item in checks:
        detail = item["detail"] if len(item["detail"]) <= 110 else item["detail"][:107] + "..."
        stream.write("%-4s %-*s  %s\n" % (item["result"], width, item["id"], detail))
    stream.write("\nVERDICT: %s (profile %s, launcher_source %s)\n" % (verdict["verdict"], verdict["profile"],
                                                                     verdict["launcher_source"]))
    stream.write(verdict["statement"] + "\n")


def load_bounds_file(path):
    """Read the --bounds-file. Returns (bounds, problems); a bad file never yields a partial result."""
    text, error = read_text(path)
    if error:
        return None, ["--bounds-file %s %s" % (path, error)]
    try:
        raw = json.loads(text)
    except (ValueError, RecursionError) as exc:
        return None, ["--bounds-file %s is not valid JSON (%s)" % (path, str(exc)[:80])]
    bounds, issues = parse_bounds(raw, "--bounds-file")
    if bounds is None or issues:
        return None, issues or ["--bounds-file must be a JSON object"]
    return bounds, []


def main(argv=None):
    parser = argparse.ArgumentParser(description="Judge a Wave 0 Stage B soak from raw evidence.")
    parser.add_argument("--evidence", required=True, help="directory holding the evidence files")
    parser.add_argument("--out", help="directory for checks.json, verdict.json and REPORT.md (default: --evidence)")
    parser.add_argument("--bounds-file", help="JSON object in the shape of config resource_bounds; its keys override the "
                                              "effective resource bounds (re-judge the same samples with other numbers)")
    args = parser.parse_args(argv)
    override = None
    if args.bounds_file:
        override, problems = load_bounds_file(args.bounds_file)
        if override is None:
            for problem in problems:
                sys.stderr.write(problem + "\n")
            return 2
    ev = load_evidence(args.evidence)
    checks, windows, _cfg = analyze(ev, override)
    verdict = verdict_for(checks, ev.get("config"), windows)
    out_dir = args.out or args.evidence
    try:
        write_outputs(out_dir, checks, verdict)
    except OSError as exc:
        sys.stderr.write("cannot write outputs to %s: %s\n" % (out_dir, exc))
        print_table(checks, verdict, sys.stdout)
        return 2
    print_table(checks, verdict, sys.stdout)
    return 0 if verdict["verdict"] in ("WAVE0_PASS", "SMOKE_PASS", "SUPPLEMENT_PASS") else 1


if __name__ == "__main__":
    sys.exit(main())
