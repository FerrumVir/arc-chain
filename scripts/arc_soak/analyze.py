"""Judge a recorded ARC soak run. Pure function of the run directory.

    python3 -m arc_soak.analyze RUN_DIR        # writes verdict.json + report.txt

Exit status is the verdict, and nothing else decides it:

    0  PASS        every required criterion was met with sufficient evidence
    1  FAIL        a criterion was violated
    2  INCOMPLETE  the evidence needed to decide is missing, malformed or too thin

Missing evidence is never a pass. Every criterion below either proves its
property from the records or refuses to certify it.

Criteria, and why each exists
-----------------------------
completion   The run must have ended normally and lasted as planned. An aborted
             or truncated run tested less than it claims.
provenance   The binary that ran must be the binary the build record describes.
             A result about an unidentified artifact is not evidence.
safety       Any SAFETY VIOLATION or panic line in any node log fails the run,
             whatever every other counter says. A zero disagreement count
             cannot override a detected conflicting certificate.
liveness     No node may be down outside a scheduled fault, and the network as a
             whole may not stop advancing for longer than `max_stall_s`.
agreement    Every check must name exactly the intended replicas. A check only
             counts as agreement when every replica that is not scheduled down
             returned a coherent block at the requested height and all of them
             are identical. Missing or malformed responses are unavailability,
             never agreement. At least one full agreement check is required.
recovery     Each fault is tracked through distinct phases - process ready with
             the right identity, peers, caught up, new work committed and seen,
             agreement including the restarted node - within a time budget.
             Process readiness alone is not recovery.
throughput   A stable pre-fault baseline (after warm-up, enough samples, a
             meaningful window, low dispersion) is compared with the steady rate
             after each recovery under the same offered workload. Rates must be
             finite and the baseline positive; NaN, zero or too few samples make
             the verdict INCOMPLETE, never PASS.
workload     When required, work must actually have been offered and every item
             accounted for: finalized, refunded, rejected with a reason, or lost
             only because the node holding it was deliberately killed.
"""

import glob
import json
import math
import os
import re
import sys
from typing import Any, Dict, Iterable, List, Optional, Tuple

PASS, FAIL, INCOMPLETE = "PASS", "FAIL", "INCOMPLETE"
EXIT_CODES = {PASS: 0, FAIL: 1, INCOMPLETE: 2}

# Defaults. The run may override any of them in run.json "thresholds"; the
# values actually applied are written into verdict.json, so a verdict always
# says what it was judged against.
DEFAULT_THRESHOLDS = {
    # completion
    "min_duration_fraction": 0.98,
    # liveness
    "max_stall_s": 60.0,
    # agreement
    "min_full_agreement_checks": 1,
    "max_unscheduled_unavailable_fraction": 0.05,
    # recovery: restart -> every phase complete
    "recovery_budget_s": 180.0,
    # throughput. The baseline must be long and stable enough that a recovered
    # rate can be compared with it at all; 0.8 of baseline is the stable-rate
    # criterion because after recovery the network is in the SAME configuration
    # under the SAME offered load as during the baseline, so the steady rate
    # should return to it. 0.5 was an alarm, not an acceptance level.
    "baseline_warmup_s": 30.0,
    "min_baseline_samples": 5,
    "min_baseline_window_s": 60.0,
    "max_baseline_cv": 0.35,
    "recovery_settle_s": 30.0,
    "min_recovery_samples": 5,
    "min_recovery_window_s": 60.0,
    "min_recovered_ratio": 0.8,
    # workload
    "max_unresolved_items": 0,
    "max_invalid_fraction": 0.0,
    "max_failed_items": 0,
    # An accepted item is excused as lost to a fault only if its node was
    # killed within this long after accepting it - i.e. before it could
    # reasonably have been gossiped and included. A kill hours later excuses
    # nothing.
    "lost_to_fault_window_s": 30.0,
}

HEX64 = re.compile(r"^(0x)?[0-9a-fA-F]{64}$")
ANSI = re.compile(r"\x1b\[[0-9;]*m")


# ── reading ─────────────────────────────────────────────────────────────────

class Evidence:
    """Everything read from a run directory, plus what could not be read."""

    def __init__(self) -> None:
        self.run: Optional[Dict[str, Any]] = None
        self.samples: List[Dict[str, Any]] = []
        self.agreement: List[Dict[str, Any]] = []
        self.faults: List[Dict[str, Any]] = []
        self.workload: List[Dict[str, Any]] = []
        self.problems: List[str] = []   # unreadable / malformed evidence


def _read_jsonl(path: str, ev: Evidence, name: str) -> List[Dict[str, Any]]:
    out: List[Dict[str, Any]] = []
    if not os.path.exists(path):
        return out
    with open(path, errors="replace") as fh:
        for lineno, line in enumerate(fh, 1):
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError as exc:
                ev.problems.append(f"{name}:{lineno} is not JSON ({exc.msg})")
                continue
            if not isinstance(obj, dict):
                ev.problems.append(f"{name}:{lineno} is not an object")
                continue
            out.append(obj)
    return out


def load(run_dir: str) -> Evidence:
    ev = Evidence()
    run_path = os.path.join(run_dir, "run.json")
    try:
        with open(run_path) as fh:
            run = json.load(fh)
        if isinstance(run, dict):
            ev.run = run
        else:
            ev.problems.append("run.json is not an object")
    except FileNotFoundError:
        ev.problems.append("run.json is missing")
    except json.JSONDecodeError as exc:
        ev.problems.append(f"run.json is not JSON ({exc.msg})")
    ev.samples = _read_jsonl(os.path.join(run_dir, "samples.jsonl"), ev, "samples.jsonl")
    ev.agreement = _read_jsonl(os.path.join(run_dir, "agreement.jsonl"), ev, "agreement.jsonl")
    ev.faults = _read_jsonl(os.path.join(run_dir, "faults.jsonl"), ev, "faults.jsonl")
    ev.workload = _read_jsonl(os.path.join(run_dir, "workload.jsonl"), ev, "workload.jsonl")
    return ev


def _finite(value: Any) -> Optional[float]:
    """A finite float, or None. NaN and infinities are not numbers here."""
    if isinstance(value, bool) or value is None:
        return None
    try:
        f = float(value)
    except (TypeError, ValueError):
        return None
    return f if math.isfinite(f) else None


def _int(value: Any) -> Optional[int]:
    f = _finite(value)
    if f is None or f != int(f):
        return None
    return int(f)


# ── the judgement ───────────────────────────────────────────────────────────

class Verdict:
    def __init__(self) -> None:
        self.fail: List[str] = []
        self.incomplete: List[str] = []
        self.facts: Dict[str, Any] = {}

    @property
    def status(self) -> str:
        # FAIL outranks INCOMPLETE: a violation observed in partial evidence is
        # still a violation.
        if self.fail:
            return FAIL
        if self.incomplete:
            return INCOMPLETE
        return PASS


def _thresholds(run: Dict[str, Any]) -> Dict[str, float]:
    th = dict(DEFAULT_THRESHOLDS)
    override = run.get("thresholds") if isinstance(run, dict) else None
    if isinstance(override, dict):
        for key, value in override.items():
            if key in th and _finite(value) is not None:
                th[key] = float(value)
    return th


def _node_indices(run: Dict[str, Any]) -> Optional[List[int]]:
    nodes = run.get("nodes")
    if not isinstance(nodes, list) or not nodes:
        return None
    out = []
    for node in nodes:
        if not isinstance(node, dict) or _int(node.get("index")) is None:
            return None
        if not isinstance(node.get("identity"), str) or not HEX64.match(node["identity"]):
            return None
        out.append(int(node["index"]))
    if len(set(out)) != len(out):
        return None
    return sorted(out)


def _identities(run: Dict[str, Any]) -> Dict[int, str]:
    return {int(n["index"]): n["identity"].lower().replace("0x", "")
            for n in run.get("nodes", []) if isinstance(n, dict)}


def check_completion(ev: Evidence, v: Verdict, th: Dict[str, float]) -> None:
    run = ev.run or {}
    if run.get("completed") is not True:
        reason = run.get("abort_reason") or "the run did not record a normal completion"
        v.incomplete.append(f"completion: {reason}")
    planned = _finite(run.get("planned_duration_s"))
    actual = _finite(run.get("actual_duration_s"))
    if planned is None or planned <= 0:
        v.incomplete.append("completion: planned duration missing or not positive")
    elif actual is None:
        v.incomplete.append("completion: actual duration missing")
    elif actual < planned * th["min_duration_fraction"]:
        v.incomplete.append(
            f"completion: ran {actual:.0f}s of a planned {planned:.0f}s")
    v.facts["planned_duration_s"] = planned
    v.facts["actual_duration_s"] = actual


def check_provenance(ev: Evidence, v: Verdict) -> None:
    prov = (ev.run or {}).get("provenance")
    if not isinstance(prov, dict):
        v.fail.append("provenance: no build record bound to the binary")
        return
    used = str((ev.run or {}).get("binary_sha256", "")).lower()
    recorded = str(prov.get("recorded_binary_sha256", "")).lower()
    if not HEX64.match(used) or not HEX64.match(recorded):
        v.fail.append("provenance: binary digest or recorded digest missing/malformed")
    elif used != recorded:
        v.fail.append(
            f"provenance: the binary that ran ({used[:16]}...) is not the one the "
            f"build record describes ({recorded[:16]}...)")
    v.facts["binary_sha256"] = used or None


def scan_logs(run_dir: str) -> Tuple[Dict[str, int], Dict[str, int], List[str]]:
    """Stream every node log once. Returns safety and panic counts per log and
    up to 50 evidence lines. Streaming, because a day of logs does not fit."""
    safety: Dict[str, int] = {}
    panics: Dict[str, int] = {}
    evidence: List[str] = []
    for path in sorted(glob.glob(os.path.join(run_dir, "node-*.log"))):
        name = os.path.basename(path)
        s = p = 0
        with open(path, errors="replace") as fh:
            for raw in fh:
                if "SAFETY VIOLATION" in raw:
                    s += 1
                    if len(evidence) < 50:
                        evidence.append(f"{name}: {ANSI.sub('', raw).strip()[:300]}")
                elif "panicked at" in raw:
                    p += 1
                    if len(evidence) < 50:
                        evidence.append(f"{name}: {ANSI.sub('', raw).strip()[:300]}")
        safety[name] = s
        panics[name] = p
    return safety, panics, evidence


def check_safety(run_dir: str, v: Verdict) -> None:
    safety, panics, evidence = scan_logs(run_dir)
    if not safety:
        v.incomplete.append("safety: no node logs to scan")
    total_s = sum(safety.values())
    total_p = sum(panics.values())
    if total_s:
        v.fail.append(f"safety: {total_s} SAFETY VIOLATION line(s) in node logs")
    if total_p:
        v.fail.append(f"safety: {total_p} panic line(s) in node logs")
    v.facts["safety_violations"] = safety
    v.facts["panics"] = panics
    v.facts["safety_evidence"] = evidence


def _samples_by_time(ev: Evidence) -> Dict[float, List[Dict[str, Any]]]:
    by: Dict[float, List[Dict[str, Any]]] = {}
    for s in ev.samples:
        t = _finite(s.get("t"))
        if t is None:
            continue
        by.setdefault(t, []).append(s)
    return by


def check_liveness(ev: Evidence, v: Verdict, th: Dict[str, float]) -> None:
    if not ev.samples:
        v.incomplete.append("liveness: no samples recorded")
        return
    unscheduled_down = [
        s for s in ev.samples
        if s.get("alive") is False and s.get("scheduled_down") is not True
    ]
    if unscheduled_down:
        nodes = sorted({str(s.get("node")) for s in unscheduled_down})
        v.fail.append(
            f"liveness: {len(unscheduled_down)} sample(s) found node(s) {', '.join(nodes)} "
            "down outside a scheduled fault")
    # Network-wide stall: the longest interval over which no up node's height
    # increased. Measured in seconds, not sample counts, so the criterion does
    # not change meaning when the sample interval does.
    by = _samples_by_time(ev)
    times = sorted(by)
    best = None
    stall_start = None
    longest = 0.0
    for t in times:
        heights = [_int(s.get("height")) for s in by[t]
                   if s.get("scheduled_down") is not True]
        heights = [h for h in heights if h is not None]
        if not heights:
            continue
        top = max(heights)
        if best is None or top > best:
            best = top
            stall_start = t
        elif stall_start is not None:
            longest = max(longest, t - stall_start)
    if longest > th["max_stall_s"]:
        v.fail.append(
            f"liveness: the whole network stopped advancing for {longest:.0f}s "
            f"(limit {th['max_stall_s']:.0f}s)")
    v.facts["longest_network_stall_s"] = longest

    # A live node's committed height never goes backwards. Committed blocks are
    # durable before the height advances, so a lower reading from the SAME
    # uninterrupted process is a correctness fault, not noise. Across a
    # scheduled restart the node is a new process and the comparison restarts.
    regressions = []
    last: Dict[Any, int] = {}
    for t in times:
        for s in by[t]:
            node = s.get("node")
            if s.get("scheduled_down") is True or s.get("alive") is False:
                last.pop(node, None)
                continue
            h = _int(s.get("height"))
            if h is None:
                continue
            if node in last and h < last[node]:
                regressions.append((node, last[node], h, t))
            last[node] = h
    if regressions:
        node, before, after, t = regressions[0]
        v.fail.append(
            f"liveness: {len(regressions)} height regression(s) on a running node; "
            f"first: node {node} went from {before} to {after}")
    v.facts["height_regressions"] = len(regressions)


def _coherent(replica: Dict[str, Any], target: int, identity: str) -> Optional[str]:
    """Why this replica's response is NOT usable agreement evidence, or None."""
    if replica.get("ok") is not True:
        return replica.get("error") or "no response"
    got_identity = str(replica.get("identity") or "").lower().replace("0x", "")
    if got_identity != identity:
        return "answered with a different validator identity"
    if _int(replica.get("height")) != target:
        return f"returned height {replica.get('height')} for requested {target}"
    for field in ("hash", "parent", "state_root"):
        if not isinstance(replica.get(field), str) or not HEX64.match(replica[field]):
            return f"missing or malformed {field}"
    return None


def check_agreement(ev: Evidence, v: Verdict, th: Dict[str, float]) -> None:
    run = ev.run or {}
    intended = _node_indices(run)
    if intended is None:
        v.incomplete.append("agreement: run.json does not name the intended replicas")
        return
    identities = _identities(run)
    full = partial = unavailable = disagree = malformed = 0
    for check in ev.agreement:
        target = _int(check.get("target"))
        replicas = check.get("replicas")
        if target is None or not isinstance(replicas, list):
            malformed += 1
            continue
        by_node = {}
        for r in replicas:
            if isinstance(r, dict) and _int(r.get("node")) is not None:
                by_node[int(r["node"])] = r
        if sorted(by_node) != intended:
            # A check that does not name exactly the intended replicas cannot
            # say anything about agreement among them.
            malformed += 1
            continue
        scheduled = [n for n in intended if by_node[n].get("scheduled_down") is True]
        valid: Dict[int, Tuple[str, str, str]] = {}
        unscheduled_missing = False
        for n in intended:
            if n in scheduled:
                continue
            why = _coherent(by_node[n], target, identities.get(n, ""))
            if why is None:
                r = by_node[n]
                valid[n] = (r["hash"].lower(), r["parent"].lower(), r["state_root"].lower())
            else:
                unscheduled_missing = True
        # Disagreement among the replicas that DID answer coherently is a
        # failure regardless of who else was missing.
        if len(set(valid.values())) > 1:
            disagree += 1
            v.fail.append(
                f"agreement: {len(set(valid.values()))} distinct blocks at height {target}")
            continue
        if unscheduled_missing:
            unavailable += 1
        elif scheduled:
            partial += 1
        elif len(valid) == len(intended):
            full += 1
    total = len(ev.agreement)
    v.facts["agreement"] = {
        "checks": total, "full": full, "partial_scheduled": partial,
        "unscheduled_unavailable": unavailable, "disagreeing": disagree,
        "malformed": malformed,
    }
    if malformed:
        v.incomplete.append(f"agreement: {malformed} malformed check record(s)")
    if full < th["min_full_agreement_checks"]:
        v.fail.append(
            f"agreement: {full} full agreement check(s); at least "
            f"{int(th['min_full_agreement_checks'])} required")
    checks_counted = full + partial + unavailable
    if checks_counted and unavailable / checks_counted > th["max_unscheduled_unavailable_fraction"]:
        v.fail.append(
            f"agreement: replicas were unavailable outside a scheduled fault in "
            f"{unavailable} of {checks_counted} checks")


def check_recovery(ev: Evidence, v: Verdict, th: Dict[str, float]) -> List[Dict[str, Any]]:
    run = ev.run or {}
    planned = _int(run.get("planned_faults"))
    if planned is None:
        v.incomplete.append("recovery: run.json does not state how many faults were planned")
        planned = 0
    if planned and not ev.faults:
        v.incomplete.append("recovery: faults were planned but none was executed")
    elif len(ev.faults) < planned:
        v.incomplete.append(f"recovery: {len(ev.faults)} of {planned} planned faults executed")
    phases = ["process_ready_t", "first_peer_t", "caught_up_t",
              "first_new_work_t", "agreement_t"]
    recovered: List[Dict[str, Any]] = []
    summary = []
    for f in ev.faults:
        k = f.get("index")
        restart = _finite(f.get("restart_t"))
        kill = _finite(f.get("kill_t"))
        row = {"index": k, "node": f.get("node"), "roles": f.get("roles")}
        if restart is None or kill is None or restart < kill:
            v.incomplete.append(f"recovery: fault {k} has no coherent kill/restart times")
            summary.append(row)
            continue
        missing = [p for p in phases if _finite(f.get(p)) is None]
        if f.get("identity_verified") is not True:
            v.fail.append(f"recovery: fault {k} - the restarted process was not verified "
                          "as the intended validator identity")
        if missing:
            v.fail.append(f"recovery: fault {k} on node {f.get('node')} never reached "
                          f"{', '.join(missing)}")
            summary.append(dict(row, missing=missing))
            continue
        times = {p: float(f[p]) for p in phases}
        if any(times[p] < restart for p in phases):
            v.incomplete.append(f"recovery: fault {k} has a phase timestamped before restart")
            summary.append(row)
            continue
        complete = max(times.values())
        took = complete - restart
        row.update({p.replace("_t", "_after_s"): round(times[p] - restart, 1) for p in phases})
        row["recovered_after_s"] = round(took, 1)
        summary.append(row)
        if took > th["recovery_budget_s"]:
            v.fail.append(
                f"recovery: fault {k} took {took:.0f}s to recover "
                f"(budget {th['recovery_budget_s']:.0f}s)")
            continue
        recovered.append({"fault": f, "kill_t": kill, "complete_t": complete})
    v.facts["faults"] = summary
    return recovered


def _rate(points: List[Tuple[float, float]]) -> Optional[float]:
    if len(points) < 2:
        return None
    (t0, h0), (t1, h1) = points[0], points[-1]
    if t1 <= t0:
        return None
    r = (h1 - h0) / (t1 - t0)
    return r if math.isfinite(r) else None


def _chain_series(ev: Evidence) -> List[Tuple[float, float]]:
    """(t, highest height among nodes not scheduled down) per sample time."""
    by = _samples_by_time(ev)
    out = []
    for t in sorted(by):
        hs = [_int(s.get("height")) for s in by[t] if s.get("scheduled_down") is not True]
        hs = [h for h in hs if h is not None]
        if hs:
            out.append((t, float(max(hs))))
    return out


def check_throughput(ev: Evidence, v: Verdict, th: Dict[str, float],
                     recovered: List[Dict[str, Any]]) -> None:
    series = _chain_series(ev)
    if not series:
        v.incomplete.append("throughput: no usable height samples")
        return
    run = ev.run or {}
    started = _finite(run.get("started_t"))
    kills = sorted(_finite(f.get("kill_t")) for f in ev.faults
                   if _finite(f.get("kill_t")) is not None)
    first_kill = kills[0] if kills else None
    # Warm-up: the baseline starts only once every node has produced a height
    # AND a further warm-up interval has passed, so start-up ramp is not
    # mistaken for the steady rate.
    all_up = None
    by = _samples_by_time(ev)
    for t in sorted(by):
        hs = [_int(s.get("height")) for s in by[t]]
        if hs and all(h is not None and h > 0 for h in hs):
            all_up = t
            break
    if all_up is None:
        v.incomplete.append("throughput: never observed every node with a height")
        return
    base_start = all_up + th["baseline_warmup_s"]
    base_end = first_kill if first_kill is not None else series[-1][0]
    base = [(t, h) for t, h in series if base_start <= t <= base_end]
    base_rate = _rate(base)
    window = (base[-1][0] - base[0][0]) if len(base) >= 2 else 0.0
    facts: Dict[str, Any] = {
        "baseline": {"samples": len(base), "window_s": round(window, 1),
                     "rate_heights_per_s": base_rate}}
    v.facts["throughput"] = facts
    if len(base) < th["min_baseline_samples"] or window < th["min_baseline_window_s"]:
        v.incomplete.append(
            f"throughput: baseline has {len(base)} samples over {window:.0f}s; needs "
            f"{int(th['min_baseline_samples'])} over {th['min_baseline_window_s']:.0f}s")
        return
    if base_rate is None or base_rate <= 0:
        v.incomplete.append("throughput: baseline rate is not a positive finite number")
        return
    # Dispersion of the per-interval rates inside the baseline. A baseline that
    # itself swings widely cannot anchor a comparison.
    per = []
    for (t0, h0), (t1, h1) in zip(base, base[1:]):
        if t1 > t0:
            per.append((h1 - h0) / (t1 - t0))
    mean = sum(per) / len(per) if per else 0.0
    if per and mean > 0:
        var = sum((x - mean) ** 2 for x in per) / len(per)
        cv = math.sqrt(var) / mean
    else:
        cv = float("inf")
    facts["baseline"]["cv"] = round(cv, 3) if math.isfinite(cv) else None
    if not math.isfinite(cv) or cv > th["max_baseline_cv"]:
        v.incomplete.append(
            f"throughput: baseline is not stable (coefficient of variation "
            f"{cv if math.isfinite(cv) else 'undefined'}, limit {th['max_baseline_cv']})")
        return
    if not ev.faults:
        return
    measured = 0
    rows = []
    for item in recovered:
        start = item["complete_t"] + th["recovery_settle_s"]
        later = [k for k in kills if k > item["kill_t"]]
        end = later[0] if later else series[-1][0]
        win = [(t, h) for t, h in series if start <= t <= end]
        span = (win[-1][0] - win[0][0]) if len(win) >= 2 else 0.0
        rate = _rate(win)
        row = {"fault": item["fault"].get("index"), "samples": len(win),
               "window_s": round(span, 1), "rate_heights_per_s": rate}
        rows.append(row)
        if len(win) < th["min_recovery_samples"] or span < th["min_recovery_window_s"]:
            continue
        if rate is None:
            v.incomplete.append(f"throughput: fault {row['fault']} recovered rate is not finite")
            continue
        ratio = rate / base_rate
        row["ratio"] = round(ratio, 3)
        measured += 1
        if ratio < th["min_recovered_ratio"]:
            v.fail.append(
                f"throughput: after fault {row['fault']} the steady rate was "
                f"{ratio:.0%} of baseline (criterion {th['min_recovered_ratio']:.0%})")
    facts["recovered"] = rows
    if recovered and measured == 0:
        v.incomplete.append(
            "throughput: no recovered fault had a long enough steady window to measure")


def check_workload(ev: Evidence, v: Verdict, th: Dict[str, float]) -> None:
    run = ev.run or {}
    spec = run.get("workload") if isinstance(run.get("workload"), dict) else {}
    required = spec.get("required") is True
    items = ev.workload
    kills = [(f.get("node"), _finite(f.get("kill_t"))) for f in ev.faults]
    counts = {"submitted": 0, "finalized": 0, "refunded": 0, "rejected": 0,
              "failed": 0, "submit_error": 0, "lost_to_fault": 0, "unresolved": 0,
              "invalid": 0}
    latencies: List[float] = []
    for it in items:
        counts["submitted"] += 1
        status = it.get("final_status")
        if status == "finalized":
            counts["finalized"] += 1
            lat = _finite(it.get("latency_s"))
            if lat is not None:
                latencies.append(lat)
        elif status == "refunded":
            counts["refunded"] += 1
        elif status == "rejected":
            if it.get("reason"):
                counts["rejected"] += 1
            else:
                counts["invalid"] += 1
        elif status == "failed":
            # Included and executed, but the execution failed. The workload
            # offers only work that should succeed, so this is a defect.
            counts["failed"] += 1
        elif status == "submit_error":
            counts["submit_error"] += 1
        else:
            # Accepted but never settled. Allowed ONLY when the node that held
            # it was deliberately killed shortly after accepting it.
            node = it.get("accepted_by")
            at = _finite(it.get("submitted_t"))
            window = th["lost_to_fault_window_s"]
            excused = at is not None and any(
                n == node and k is not None and at <= k <= at + window for n, k in kills)
            if excused:
                counts["lost_to_fault"] += 1
            else:
                counts["unresolved"] += 1
    latencies.sort()
    facts: Dict[str, Any] = dict(counts)
    if latencies:
        facts["latency_p50_s"] = latencies[len(latencies) // 2]
        facts["latency_p95_s"] = latencies[min(len(latencies) - 1, int(len(latencies) * 0.95))]
    v.facts["workload"] = facts
    if not required:
        return
    if counts["submitted"] == 0:
        v.fail.append("workload: required, but no work was offered")
        return
    if counts["finalized"] + counts["refunded"] == 0:
        v.fail.append("workload: nothing that was offered ever settled")
    if counts["unresolved"] > th["max_unresolved_items"]:
        v.fail.append(f"workload: {counts['unresolved']} accepted item(s) never settled and "
                      "were not lost to a deliberate fault")
    if counts["failed"] > th["max_failed_items"]:
        v.fail.append(f"workload: {counts['failed']} item(s) were included but failed to execute")
    invalid_frac = counts["invalid"] / counts["submitted"]
    if invalid_frac > th["max_invalid_fraction"]:
        v.fail.append(f"workload: {counts['invalid']} item(s) rejected without a reason")


def judge(run_dir: str) -> Verdict:
    ev = load(run_dir)
    v = Verdict()
    for p in ev.problems:
        v.incomplete.append(f"evidence: {p}")
    if ev.run is None:
        v.incomplete.append("evidence: cannot judge a run without run.json")
        return v
    th = _thresholds(ev.run)
    v.facts["thresholds"] = th
    check_completion(ev, v, th)
    check_provenance(ev, v)
    check_safety(run_dir, v)
    check_liveness(ev, v, th)
    check_agreement(ev, v, th)
    recovered = check_recovery(ev, v, th)
    check_throughput(ev, v, th, recovered)
    check_workload(ev, v, th)
    return v


def render(v: Verdict) -> str:
    lines = ["================ ARC SOAK VERDICT ================",
             f"VERDICT: {v.status}"]
    for r in v.fail:
        lines.append(f"  FAIL        {r}")
    for r in v.incomplete:
        lines.append(f"  INCOMPLETE  {r}")
    lines.append("")
    lines.append(json.dumps(v.facts, indent=2, sort_keys=True, default=str))
    return "\n".join(lines) + "\n"


def main(argv: Optional[List[str]] = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    if len(argv) != 1:
        print("usage: python3 -m arc_soak.analyze RUN_DIR", file=sys.stderr)
        return EXIT_CODES[INCOMPLETE]
    run_dir = argv[0]
    try:
        v = judge(run_dir)
    except Exception as exc:  # the analyzer failing is never a pass
        v = Verdict()
        v.incomplete.append(f"analyzer crashed: {type(exc).__name__}: {exc}")
    text = render(v)
    try:
        with open(os.path.join(run_dir, "verdict.json"), "w") as fh:
            json.dump({"status": v.status, "fail": v.fail, "incomplete": v.incomplete,
                       "facts": v.facts}, fh, indent=2, sort_keys=True, default=str)
        with open(os.path.join(run_dir, "report.txt"), "w") as fh:
            fh.write(text)
    except OSError as exc:
        sys.stderr.write(f"could not write the verdict: {exc}\n")
        return EXIT_CODES[INCOMPLETE]
    sys.stdout.write(text)
    return EXIT_CODES[v.status]


if __name__ == "__main__":
    sys.exit(main())
