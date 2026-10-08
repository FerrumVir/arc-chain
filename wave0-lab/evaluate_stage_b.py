#!/usr/bin/env python3
"""Wave 0 Stage B evaluator: judge a soak from raw evidence.

Usage:
    python3 wave0-lab/evaluate_stage_b.py --evidence <dir> [--out <dir>]

Reads (all optional on disk, but a missing or unparseable file FAILS every check that needs it):
    config-effective.json   the effective lab config of the run
    samples.jsonl           one guest sample per interval (survives the guest reboot)
    events.jsonl            host orchestration events (forced / not forced, t0, reboot, steady, final stop)
    commands.jsonl          every ssh command the orchestrator ran (proves no manual node start after the reboot)
    invariants-before.json  invariants collected before the first consume
    invariants-after.json   invariants collected at the end of the soak
    checks-live.jsonl       point-in-time checks the orchestrator already made (merged verbatim)

Writes checks.json, verdict.json and REPORT.md into --out (default: the evidence directory), prints a compact table
and the verdict, and exits 0 iff the verdict is WAVE0_PASS or SMOKE_PASS.

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
  * Time comes from `guest_epoch` when an event has one, otherwise from `host_epoch`. Samples use the guest clock.
  * A "bridged sample" is one whose node_exe contains /legacy-bridge/releases/ (the v0.8 node the launcher exec'ed).

Check ids computed here (live checks from the orchestrator are appended verbatim after them):
    EVIDENCE-FILES FLOORS EVENTS-ORDER SAMPLES-TOTAL SAMPLES-STEADY SAMPLE-GAPS STEADY-DURATION TOTAL-DURATION
    STEADY-UNINTERRUPTED HEALTH-BRIDGED IDENTITY-STABLE STAKE-ZERO COMPUTE-OFF ONE-NODE LEGACY-UNCHANGED-SAMPLES
    INVARIANTS-BEFORE-AFTER PRIVACY-SAFE-NAME REGISTRATION-LIVE REBOOT-BOOT-ID REBOOT-AUTO-RECOVERY
    REBOOT-UPDATER-TIMER REBOOT-HEALTHY-WINDOW REBOOT-SAME-LAUNCHER LIVE-REQUIRED LIVE-RESULTS
"""

from __future__ import annotations

import argparse
import datetime
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
# Ids whose result may legitimately be INFO without blocking a PASS verdict.
INFO_OK_IDS = ("FLOORS", "REGISTRATION-LIVE")


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


VALIDATORS = {
    "samples": validate_sample,
    "events": validate_event,
    "commands": validate_command,
    "live": validate_live,
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
          "samples": [], "events": [], "commands": [], "live": []}
    for key, fname in FILES:
        text = texts.get(fname)
        info = {"file": fname, "present": text is not None, "error": None, "records": 0,
                "bad_json": [], "malformed": []}
        ev["src"][key] = info
        if text is None:
            info["error"] = "missing"
            continue
        if key in JSON_KEYS:
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
    for key, fname in FILES:
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

def effective_config(raw):
    """Return (cfg, issues, lowered). Under the full profile the floors are enforced whatever the file says."""
    cfg = dict(DEFAULTS)
    issues, lowered = [], []
    document = raw if isinstance(raw, dict) else {}
    if document and document.get("schema") != SCHEMA_CONFIG:
        issues.append("config schema is %r, expected %r" % (document.get("schema"), SCHEMA_CONFIG))
    profile = document.get("profile")
    if profile not in ("full", "smoke"):
        if document:
            issues.append("profile must be 'full' or 'smoke', got %r (judged as full)" % (profile,))
        profile = "full"
    cfg["profile"] = profile
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
    if profile == "full":
        for key, floor in sorted(FLOOR_MIN.items()):
            if cfg[key] < floor:
                lowered.append("%s=%s is below Astra's floor %s" % (key, cfg[key], floor))
                cfg[key] = floor
        if cfg["sample_interval_s"] > FLOOR_INTERVAL_MAX:
            lowered.append("sample_interval_s=%s is above Astra's ceiling %s" % (cfg["sample_interval_s"], FLOOR_INTERVAL_MAX))
            cfg["sample_interval_s"] = FLOOR_INTERVAL_MAX
    return cfg, issues, lowered


# --------------------------------------------------------------------------------------------------------------------
# context: everything the checks need, derived once
# --------------------------------------------------------------------------------------------------------------------

def ev_time(event):
    guest = event.get("guest_epoch")
    if is_num(guest):
        return float(guest)
    return float(event["host_epoch"])


class Ctx(object):
    def __init__(self, ev):
        self.ev = ev
        raw = ev.get("config")
        self.raw_cfg = raw if isinstance(raw, dict) else {}
        self.cfg, self.cfg_issues, self.cfg_lowered = effective_config(raw)
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
                  "start": None, "end": None, "steady_s": None, "total_s": None, "forced": []}
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
        if forced:
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

    def source_problems(self, needs):
        problems = []
        for key in needs:
            info = self.ev["src"][key]
            name = info["file"]
            if info["error"] == "missing":
                problems.append("%s is missing" % name)
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
    cfg = c.cfg
    if c.cfg["profile"] == "full":
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


@check("EVENTS-ORDER", "Required events exist once, are labelled forced/unforced correctly and come in order",
       ["events", "config"])
def chk_events_order(c, o):
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
    normal_limit = c.cfg["max_gap_factor"] * c.interval
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
        o.ok("%d sample(s) from t0 to the end; worst gap %.0f s between %s and %s (limit %.0f s; reboot pair limit %.0f s)"
             % (len(inside), worst[0], worst[1], worst[2], worst[3], reboot_limit))


@check("STEADY-DURATION", "Uninterrupted steady state lasts long enough after the last forced event",
       ["events", "samples", "config"])
def chk_steady_duration(c, o):
    window = c.W
    if window["steady_s"] is None:
        o.fail("steady window undefined: " + c.window_problem())
        return
    text = ("steady window %s after the last forced event (%s) = %.0f s (required >= %.0f s)"
            % (fmt_dur(window["steady_s"]), window["last_forced"], window["steady_s"], c.cfg["min_steady_s"]))
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
    missing = [i for i in REQUIRED_LIVE_IDS if i not in present]
    if c.ev["src"]["live"]["error"] == "missing":
        o.fail("orchestrator wrote no live checks")
    elif missing:
        o.fail("missing live check id(s): %s" % ", ".join(missing))
    else:
        o.ok("all %d required live check ids present" % len(REQUIRED_LIVE_IDS))


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
    clashes = sorted(set(r["id"] for r in records if r["id"] in COMPUTED_IDS))
    summary = "%d live check(s): %d PASS, %d FAIL, %d INFO" % (len(records), counts["PASS"], counts["FAIL"], counts["INFO"])
    if failed:
        o.fail("%s; failed: %s" % (summary, ", ".join(failed)))
    if clashes:
        o.fail("live check id(s) collide with evaluator ids: %s" % ", ".join(clashes))
    if not o.fails:
        o.ok(summary)


# --------------------------------------------------------------------------------------------------------------------
# evaluation, verdict, outputs
# --------------------------------------------------------------------------------------------------------------------

def live_records(ev):
    """The orchestrator's own checks, merged verbatim after the computed ones."""
    return [{"id": record["id"], "title": record.get("title", ""), "result": record["result"],
             "detail": record.get("detail", "")} for record in ev.get("live", [])]


def analyze(ev):
    """Return (checks, windows, effective_config). Never raises on bad evidence."""
    try:
        c = Ctx(ev)
    except Exception as exc:  # defensive: report, never crash
        cfg = effective_config(None)[0]
        broken = [{"id": check_id, "title": title, "result": "FAIL",
                   "detail": "internal error building the evaluation context: %r" % (exc,)}
                  for check_id, title, _needs, _func in CHECKS]
        return broken + list(live_records(ev)), {"problems": ["internal error"], "forced": []}, cfg
    results = []
    for check_id, title, needs, func in CHECKS:
        outcome = Outcome()
        for problem in c.source_problems(needs):
            outcome.fail(problem)
        try:
            func(c, outcome)
        except Exception as exc:  # defensive: a bug here must show up as a FAIL, never as a crash
            outcome.fail("internal error in %s: %r" % (check_id, exc))
        result, detail = outcome.finish()
        results.append({"id": check_id, "title": title, "result": result, "detail": detail})
    results.extend(live_records(ev))
    return results, c.W, c.cfg


def evaluate(ev):
    return analyze(ev)[0]


def build_statement(verdict, profile, launcher_source, tag, launcher_sha, failed, unmet):
    parts = []
    if profile == "smoke":
        parts.append("SMOKE PROFILE: this run does NOT satisfy Astra's thresholds (>= 4 h total, >= 2 h uninterrupted "
                     "steady state after the last forced event, >= 121 samples, >= 10 min healthy after the reboot); "
                     "it only exercises the machinery.")
    if launcher_source == "artifact":
        parts.append("LAUNCHER SOURCE artifact: this run consumed the Stage A artifact bytes by a local replay of "
                     "canary-consume.sh (rehearsal) and is NOT the post-G0 published-tag Wave 0.")
    if verdict == "WAVE0_PASS":
        if launcher_source == "published":
            parts.append("Wave 0 criteria met on the published tag %s (launcher sha256 %s)." % (tag, launcher_sha))
        else:
            parts.append("Every full-profile criterion was met, but the launcher was not consumed from the published tag.")
    elif verdict in ("WAVE0_FAIL", "SMOKE_FAIL"):
        ids = list(failed) + [i for i in unmet if i not in failed]
        parts.append("Criteria NOT met; failed or unmet checks: %s." % (", ".join(ids) if ids else "none recorded"))
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
    for check_id in COMPUTED_IDS:
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
    else:
        verdict = "WAVE0_PASS" if passed else "WAVE0_FAIL"
    launcher_source = document.get("launcher_source")
    window = windows or {}
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
        "statement": build_statement(verdict, profile, launcher_source, document.get("tag"),
                                     document.get("expected_launcher_sha256"), failed, unmet),
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


def main(argv=None):
    parser = argparse.ArgumentParser(description="Judge a Wave 0 Stage B soak from raw evidence.")
    parser.add_argument("--evidence", required=True, help="directory holding the evidence files")
    parser.add_argument("--out", help="directory for checks.json, verdict.json and REPORT.md (default: --evidence)")
    args = parser.parse_args(argv)
    ev = load_evidence(args.evidence)
    checks, windows, _cfg = analyze(ev)
    verdict = verdict_for(checks, ev.get("config"), windows)
    out_dir = args.out or args.evidence
    try:
        write_outputs(out_dir, checks, verdict)
    except OSError as exc:
        sys.stderr.write("cannot write outputs to %s: %s\n" % (out_dir, exc))
        print_table(checks, verdict, sys.stdout)
        return 2
    print_table(checks, verdict, sys.stdout)
    return 0 if verdict["verdict"] in ("WAVE0_PASS", "SMOKE_PASS") else 1


if __name__ == "__main__":
    sys.exit(main())
