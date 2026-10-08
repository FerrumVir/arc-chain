#!/usr/bin/env python3
"""Aggregate the Stage A records into stage-a-summary.json, the evidence the L6/L8 release scripts read.

THROWAWAY LAB FILE. The summary holds exactly the keys of the contract
(arc.legacy-bridge.wave0-lab.stage-a.v1): nothing else, so a strict reader never sees surprises.

Required, per the lab configuration:
  arc-node-linux-x86_64        consumed PASS by the headless harness AND by the desktop harness
  arc-node-windows-x86_64.exe  consumed PASS by the desktop harness (windows-latest)
  arc-node-macos-arm64         consumed PASS by the desktop harness (macos-15)
  arc-node-macos-x86_64        consumed PASS by the desktop harness (macos-15-intel)
  arc-node-linux-aarch64       the NAMED EXCEPTION only (executed as a smoke on ubuntu-24.04-arm)

Verdict: STAGE_A_PASS only when every required record is present and PASS; STAGE_A_FAIL when any
present record failed or carries other bytes than the pinned digest; STAGE_A_INCOMPLETE otherwise.

Usage: stage_a_summary.py build --config C --records DIR --out FILE
         [--lab-commit SHA --lab-run-id N --lab-run-attempt N --lab-branch B]  (defaults from GITHUB_*)
"""
from __future__ import annotations

import argparse
import json
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from stage_a_record import AARCH64, AARCH64_EXCEPTION_STATEMENT, AARCH64_RUNNER, EXCEPTION_KIND, SCHEMA as RECORD_SCHEMA  # noqa: E402

SUMMARY_SCHEMA = "arc.legacy-bridge.wave0-lab.stage-a.v1"
WORKFLOW_PATH = ".github/workflows/wave0-lab.yml"
HEX64 = re.compile(r"^[0-9a-f]{64}$")
RESULT_KEYS = ("asset", "sha256", "consumed", "result", "harness", "job", "runner", "sha256_before_run", "sha256_after_run", "exit_code")
EXCEPTION_KEYS = ("asset", "sha256", "kind", "statement", "executed", "smoke_result", "runner", "job", "sha256_before_run", "sha256_after_run")

# (asset, job) pairs that must each hold a consumed PASS record.
CONSUMED_REQUIRED = (
    ("arc-node-linux-x86_64", "stage-a-headless-linux"),
    ("arc-node-linux-x86_64", "stage-a-desktop-linux"),
    ("arc-node-windows-x86_64.exe", "stage-a-desktop-windows"),
    ("arc-node-macos-arm64", "stage-a-desktop-macos-arm64"),
    ("arc-node-macos-x86_64", "stage-a-desktop-macos-intel"),
)
SMOKE_JOB = "stage-a-linux-aarch64-smoke"


def load_records(directory: Path) -> tuple[list[dict], list[str]]:
    records: list[dict] = []
    problems: list[str] = []
    for path in sorted(directory.glob("*.json")):
        try:
            record = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            problems.append(f"{path.name}: unreadable ({error})")
            continue
        if not isinstance(record, dict) or record.get("schema") != RECORD_SCHEMA or record.get("kind") not in ("consumed", "exception"):
            problems.append(f"{path.name}: not a Stage A record")
            continue
        records.append(record)
    return records, problems


def build(config: dict, records: list[dict], lab: dict) -> tuple[dict, list[str]]:
    """Return (summary, notes). notes explain every gap or failure; empty when the verdict is PASS."""
    launchers = config["handoff"]["launchers"]
    notes: list[str] = []
    results: list[dict] = []
    exceptions: list[dict] = []
    seen: set[tuple[str, str]] = set()
    failed = False

    for record in records:
        asset = str(record.get("asset"))
        job = str(record.get("job"))
        key = (asset, job)
        if key in seen:
            notes.append(f"duplicate record for {asset} from {job}")
            failed = True
            continue
        seen.add(key)
        pinned = launchers.get(asset)
        if pinned is None:
            notes.append(f"record for unknown asset {asset}")
            failed = True
            continue
        digests = (record.get("sha256"), record.get("sha256_before_run"), record.get("sha256_after_run"))
        if not all(isinstance(d, str) and HEX64.match(d) for d in digests) or len(set(digests)) != 1 or digests[0] != pinned:
            notes.append(f"{asset} from {job}: bytes differ from the pinned digest or from each other before/after the run")
            failed = True
        if record["kind"] == "consumed":
            if asset == AARCH64:
                notes.append("arc-node-linux-aarch64 may only appear as the named exception, never as consumed")
                failed = True
                continue
            if record.get("result") != "PASS":
                notes.append(f"{asset} from {job}: harness result {record.get('result')} (exit {record.get('exit_code')})")
                failed = True
            results.append({name: record.get(name) for name in RESULT_KEYS})
        else:
            if asset != AARCH64:
                notes.append(f"a named exception is only valid for {AARCH64}, not {asset}")
                failed = True
                continue
            if record.get("statement") != AARCH64_EXCEPTION_STATEMENT or record.get("exception_kind") != EXCEPTION_KIND:
                notes.append("the aarch64 exception statement or kind is not the approved one")
                failed = True
            if record.get("executed") is not True or record.get("smoke_result") != "PASS" or record.get("runner") != AARCH64_RUNNER:
                notes.append(f"the aarch64 smoke did not pass on {AARCH64_RUNNER}")
                failed = True
            entry = {name: record.get(name) for name in EXCEPTION_KEYS if name != "kind"}
            entry["kind"] = record.get("exception_kind")
            exceptions.append({name: entry.get(name) for name in EXCEPTION_KEYS})

    missing = [f"{asset} from {job}" for asset, job in CONSUMED_REQUIRED if (asset, job) not in seen]
    if (AARCH64, SMOKE_JOB) not in seen:
        missing.append(f"{AARCH64} smoke exception from {SMOKE_JOB}")
    if missing:
        notes.append("missing records: " + "; ".join(missing))
    verdict = "STAGE_A_FAIL" if failed else ("STAGE_A_INCOMPLETE" if missing else "STAGE_A_PASS")

    results.sort(key=lambda entry: (str(entry["asset"]), str(entry["job"])))
    summary = {
        "schema": SUMMARY_SCHEMA,
        "repository": config["repository"],
        "workflow_path": WORKFLOW_PATH,
        "lab_branch": lab["branch"],
        "lab_commit": lab["commit"],
        "lab_run_id": lab["run_id"],
        "lab_run_attempt": lab["run_attempt"],
        "base_commit": config["base_commit"],
        "handoff": {
            "run_id": config["handoff"]["run_id"],
            "artifact_id": config["handoff"]["artifact_id"],
            "artifact_name": config["handoff"]["artifact_name"],
            "artifact_digest": config["handoff"]["artifact_digest"],
            "tag": config["handoff"]["tag"],
            "launchers": dict(sorted(launchers.items())),
        },
        "results": results,
        "exceptions": exceptions,
        "verdict": verdict,
    }
    return summary, notes


def render_table(summary: dict, notes: list[str]) -> str:
    lines = [f"Stage A verdict: {summary['verdict']}", ""]
    for entry in summary["results"]:
        lines.append(f"  {entry['result']:4}  consumed   {entry['asset']:30} {entry['sha256'][:12]}  {entry['job']} ({entry['runner']})")
    for entry in summary["exceptions"]:
        lines.append(f"  {entry['smoke_result']:4}  EXCEPTION  {entry['asset']:30} {entry['sha256'][:12]}  {entry['job']} ({entry['runner']})")
        lines.append(f"        {entry['statement']}")
    for note in notes:
        lines.append(f"  NOTE  {note}")
    return "\n".join(lines) + "\n"


def lab_from_args(args: argparse.Namespace) -> dict:
    def pick(value, env_name):
        return value if value is not None else os.environ.get(env_name)

    commit = pick(args.lab_commit, "GITHUB_SHA")
    run_id = pick(args.lab_run_id, "GITHUB_RUN_ID")
    attempt = pick(args.lab_run_attempt, "GITHUB_RUN_ATTEMPT")
    branch = pick(args.lab_branch, "GITHUB_REF_NAME")
    if not (commit and run_id and attempt and branch):
        raise SystemExit("lab commit, run id, attempt and branch are required (arguments or GITHUB_* environment)")
    return {"commit": str(commit), "run_id": int(run_id), "run_attempt": int(attempt), "branch": str(branch)}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    build_cmd = sub.add_parser("build")
    build_cmd.add_argument("--config", type=Path, required=True)
    build_cmd.add_argument("--records", type=Path, required=True)
    build_cmd.add_argument("--out", type=Path, required=True)
    build_cmd.add_argument("--lab-commit")
    build_cmd.add_argument("--lab-run-id")
    build_cmd.add_argument("--lab-run-attempt")
    build_cmd.add_argument("--lab-branch")
    args = parser.parse_args(argv)

    config = json.loads(args.config.read_text(encoding="utf-8"))
    records, problems = load_records(args.records)
    summary, notes = build(config, records, lab_from_args(args))
    notes = problems + notes
    if problems and summary["verdict"] == "STAGE_A_PASS":
        summary["verdict"] = "STAGE_A_FAIL"
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    table = render_table(summary, notes)
    sys.stdout.write(table)
    step_summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if step_summary:
        with open(step_summary, "a", encoding="utf-8") as handle:
            handle.write("```\n" + table + "```\n")
    return 0 if summary["verdict"] == "STAGE_A_PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
