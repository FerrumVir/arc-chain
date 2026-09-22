#!/usr/bin/env python3
"""M8, protocol half: how long native paid requests take to settle.

Reads a soak run's workload.jsonl (written by soak_native_load), and its
run.json and faults.jsonl when present, and reports, per executor label:

  * outcomes: finalized, refunded, rejected, submit_error, pending, and
    requests presumed lost that settled after all;
  * submit -> accepted latency (a node took the signed request);
  * submit -> settled latency, separately for finalized and for refunded
    requests: n, min, p50, p90, p95, p99, max and mean (nearest rank);
  * retries: how many submission attempts each accepted request needed;
  * throughput: settled requests per minute from the first submission to
    the last settlement, beside the offered rate from run.json.

It reports and judges nothing. The driver sees a settlement by polling
receipts every 250 ms or so, so every settled latency includes up to one
poll interval of observation delay. The report states that resolution
rather than subtracting it.

Labels matter. A deterministic-test-executor group measures the protocol
path (admission, votes, certificate, settlement) and says nothing about
model latency, which `serving_latency` measures on the real artifact.
Records of different executors are never pooled, and a record whose label
differs from run.json's is reported as a problem. A run with faults
measures latency under faults: faults.jsonl is counted and reported, not
filtered out.

    python3 scripts/benchmarks/native_request_latency.py --run <soak work dir> [--json-out f]
"""
import argparse
import json
import math
import os
import sys
from typing import Any, Dict, List, Optional, Tuple

SCHEMA = "arc.m8.native-request-latency.v1"
OUTCOMES = ("finalized", "refunded", "rejected", "submit_error", "pending")
# soak_native_load's main loop: poll every in-flight request, then sleep 250 ms.
OBSERVATION_RESOLUTION_S = 0.25


def _number(value: Any) -> Optional[float]:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value) if math.isfinite(value) else None


def distribution(samples: List[float]) -> Optional[Dict[str, Any]]:
    """Nearest-rank summary: pXX is the smallest sample with at least XX% of
    the samples at or below it. None for no samples, never zeros."""
    if not samples:
        return None
    ordered = sorted(samples)
    n = len(ordered)

    def rank(p: float) -> float:
        return ordered[max(0, math.ceil(p * n) - 1)]

    return {"n": n, "min": ordered[0], "p50": rank(0.50), "p90": rank(0.90),
            "p95": rank(0.95), "p99": rank(0.99), "max": ordered[-1],
            "mean": sum(ordered) / n}


def read_jsonl(path: str) -> Tuple[List[Dict[str, Any]], List[str]]:
    records: List[Dict[str, Any]] = []
    problems: List[str] = []
    with open(path) as fh:
        for lineno, line in enumerate(fh, 1):
            line = line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError as error:
                problems.append(f"{os.path.basename(path)} line {lineno}: not JSON ({error.msg})")
                continue
            if not isinstance(record, dict):
                problems.append(f"{os.path.basename(path)} line {lineno}: not an object")
                continue
            records.append(record)
    return records, problems


def summarize_group(records: List[Dict[str, Any]], problems: List[str], label: str) -> Dict[str, Any]:
    outcomes: Dict[str, int] = {name: 0 for name in OUTCOMES}
    other: Dict[str, int] = {}
    accept: List[float] = []
    finalize: List[float] = []
    refund: List[float] = []
    attempts: Dict[str, int] = {}
    late = 0
    first_submit: Optional[float] = None
    last_settle: Optional[float] = None
    for record in records:
        status = record.get("final_status")
        if status in outcomes:
            outcomes[status] += 1
        else:
            other[str(status)] = other.get(str(status), 0) + 1
        submitted = _number(record.get("submitted_t"))
        accepted = _number(record.get("accepted_t"))
        settled = _number(record.get("settled_t"))
        if submitted is not None:
            first_submit = submitted if first_submit is None else min(first_submit, submitted)
        if submitted is not None and accepted is not None:
            if accepted < submitted:
                problems.append(f"{label} {record.get('id')}: accepted before it was submitted")
            else:
                accept.append(accepted - submitted)
        if status in ("finalized", "refunded"):
            latency = _number(record.get("latency_s"))
            if latency is None or latency < 0:
                problems.append(f"{label} {record.get('id')}: settled without a usable latency")
            else:
                (finalize if status == "finalized" else refund).append(latency)
            if settled is not None:
                last_settle = settled if last_settle is None else max(last_settle, settled)
            if record.get("presumed_lost_t") is not None:
                late += 1
        if accepted is not None:
            count = record.get("attempts")
            key = str(count) if isinstance(count, int) and not isinstance(count, bool) and count >= 1 else "unrecorded"
            attempts[key] = attempts.get(key, 0) + 1
    settled_count = outcomes["finalized"] + outcomes["refunded"]
    span = (last_settle - first_submit) if first_submit is not None and last_settle is not None else None
    per_min = (lambda count: count / span * 60.0) if span and span > 0 else (lambda count: None)
    return {
        "records": len(records),
        "outcomes": outcomes,
        "other_outcomes": other,
        "presumed_lost_then_settled": late,
        "accept_latency_s": distribution(accept),
        "finalize_latency_s": distribution(finalize),
        "refund_latency_s": distribution(refund),
        "attempts_per_accepted_request": dict(sorted(attempts.items())),
        "retried": sum(n for key, n in attempts.items() if key not in ("1", "unrecorded")),
        "span_s": span,
        "settled_per_min": per_min(settled_count),
        "finalized_per_min": per_min(outcomes["finalized"]),
    }


def report(records: List[Dict[str, Any]], run: Optional[Dict[str, Any]] = None,
           faults: Optional[int] = None, problems: Optional[List[str]] = None) -> Dict[str, Any]:
    problems = list(problems or [])
    workload = (run or {}).get("workload") if isinstance((run or {}).get("workload"), dict) else {}
    run_label = workload.get("executor")
    groups: Dict[str, List[Dict[str, Any]]] = {}
    ignored = 0
    for record in records:
        if record.get("kind") != "native_inference":
            ignored += 1
            continue
        label = record.get("executor")
        if not isinstance(label, str) or not label:
            label = "unlabelled"
            problems.append(f"{record.get('id')}: record has no executor label")
        if run_label and label != run_label:
            problems.append(f"{record.get('id')}: executor {label!r} differs from run.json's {run_label!r}")
        groups.setdefault(label, []).append(record)
    return {
        "schema": SCHEMA,
        "run": {
            "completed": (run or {}).get("completed"),
            "executor": run_label,
            "offered_rate_per_s": workload.get("native_rate_per_s"),
            "requesters": workload.get("native_requesters"),
            "native_workers": workload.get("native_workers"),
            "faults_recorded": faults,
        },
        "observation_resolution_s": OBSERVATION_RESOLUTION_S,
        "ignored_non_native_records": ignored,
        "executors": {label: summarize_group(items, problems, label)
                      for label, items in sorted(groups.items())},
        "problems": problems,
        "note": ("Reports; judges nothing. A deterministic-test-executor group measures the "
                 "protocol path only, not model latency."),
    }


def report_for_run(run_dir: str) -> Dict[str, Any]:
    records, problems = read_jsonl(os.path.join(run_dir, "workload.jsonl"))
    run = None
    run_path = os.path.join(run_dir, "run.json")
    if os.path.exists(run_path):
        with open(run_path) as fh:
            run = json.load(fh)
    else:
        problems.append("run.json is missing: the offered rate and executor label are unknown")
    faults = None
    faults_path = os.path.join(run_dir, "faults.jsonl")
    if os.path.exists(faults_path):
        fault_records, fault_problems = read_jsonl(faults_path)
        faults = len(fault_records)
        problems.extend(fault_problems)
    return report(records, run, faults, problems)


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--run", required=True, help="a soak work directory with workload.jsonl")
    parser.add_argument("--json-out")
    args = parser.parse_args(argv)
    try:
        result = report_for_run(args.run)
    except (OSError, json.JSONDecodeError) as error:
        print(f"cannot read {args.run}: {error}", file=sys.stderr)
        return 2
    text = json.dumps(result, indent=2, sort_keys=True)
    if args.json_out:
        with open(args.json_out, "w") as fh:
            fh.write(text + "\n")
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
