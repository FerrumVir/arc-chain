"""Collect a finished soak's evidence into a sealed bundle (post-soak runbook step 0).

    python3 -m arc_soak.collect_evidence --run RUN_DIR --out OUT_DIR [--pid PID]
                                          [--skip-s 700] [--include-node-logs]

Run it only after the orchestrator has exited. The analyzer writes
verdict.json and report.txt on every orchestrator exit path, so a run
directory without them is unfinished or was never judged, and this refuses
it: it never runs the analyzer, never decides a verdict, and never writes,
moves or deletes anything in RUN_DIR.

OUT_DIR (new, or an existing empty directory) receives:

  the run's records, copied as they are, when present - run.json,
    verdict.json, report.txt, events.log, faults.jsonl, agreement.jsonl,
    samples.jsonl, workload.jsonl, diag.jsonl, activation.json, genesis.toml,
    load-driver.log and every ident-*.txt;
  node-N.M.log.gz for every node log, only with --include-node-logs (they can
    be large); node data directories (node-N/) are never copied;
  growth.txt       what `python3 -m arc_soak.growth RUN_DIR --skip-s S` prints;
  summary.json     the verdict fields exactly as verdict.json records them, the
                   completed flag, record counts, and the binary identity and
                   provenance from run.json;
  MANIFEST.sha256  `sha256  relative/path` for every file above, sorted,
                   written last - a bundle without it is incomplete.

Exit: 0 collected, 2 refused (the orchestrator is still alive, the verdict or
report is missing, or OUT_DIR is not empty or lies inside RUN_DIR).
"""

import argparse
import datetime
import gzip
import hashlib
import json
import os
import re
import shutil
import sys
from collections import Counter
from typing import Any, Dict, List, Optional, Tuple

from arc_soak import growth

# Copied when present, byte for byte. Nothing else in the run directory is:
# node data directories, backup archives and the ident-N/ directories stay put.
RECORDS = ("run.json", "verdict.json", "report.txt", "events.log", "faults.jsonl",
           "agreement.jsonl", "samples.jsonl", "workload.jsonl", "diag.jsonl",
           "activation.json", "genesis.toml", "load-driver.log")
IDENT_TXT = re.compile(r"^ident-[^/]+\.txt$")
NODE_LOG = re.compile(r"^node-\d+\.\d+\.log$")
# The analyzer's own fields; repeated as recorded, never recomputed here.
VERDICT_FIELDS = ("status", "exit", "exit_code")
REFUSED = 2


class Refused(Exception):
    """The collector will not run; the reason is printed and the exit is 2."""


def alive(pid: int) -> bool:
    """True while a process with this id exists (signal 0 delivers nothing)."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:  # it exists, owned by someone else
        return True
    return True


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def _json(path: str) -> Optional[Any]:
    try:
        with open(path, encoding="utf-8") as fh:
            return json.load(fh)
    except (OSError, ValueError):
        return None


def _jsonl(path: str) -> Tuple[List[dict], int]:
    """The JSON-object records of a .jsonl file, and how many non-blank lines were not."""
    records: List[dict] = []
    bad = 0
    if not os.path.isfile(path):
        return records, bad
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            if not line.strip():
                continue
            try:
                rec = json.loads(line)
            except ValueError:
                bad += 1
                continue
            if isinstance(rec, dict):
                records.append(rec)
            else:
                bad += 1
    return records, bad


def _tally(values) -> Dict[str, int]:
    return dict(sorted(Counter("(none)" if v is None else str(v) for v in values).items()))


def summarize(run_dir: str) -> Dict[str, Any]:
    """Repeat what the run recorded; decide nothing."""
    run = _json(os.path.join(run_dir, "run.json"))
    verdict = _json(os.path.join(run_dir, "verdict.json"))
    agreement, agreement_bad = _jsonl(os.path.join(run_dir, "agreement.jsonl"))
    faults, faults_bad = _jsonl(os.path.join(run_dir, "faults.jsonl"))
    workload, workload_bad = _jsonl(os.path.join(run_dir, "workload.jsonl"))
    run_d = run if isinstance(run, dict) else {}
    if isinstance(verdict, dict):
        recorded = {k: verdict[k] for k in VERDICT_FIELDS if k in verdict}
    else:
        recorded = {"unreadable": True}
    return {
        "verdict": recorded,
        "run_json_readable": isinstance(run, dict),
        "completed": run_d.get("completed"),
        "binary_sha256": run_d.get("binary_sha256"),
        "provenance": run_d.get("provenance"),
        "counts": {
            "agreement_records": len(agreement),
            "fault_records": len(faults),
            "faults_recovered": sum(1 for f in faults if f.get("outcome") == "recovered"),
            "fault_outcomes": _tally(f.get("outcome") for f in faults),
            "workload_records": len(workload),
            "workload_by_final_status": _tally(w.get("final_status") for w in workload),
        },
        "unparsable_lines": {name: n for name, n in (("agreement.jsonl", agreement_bad),
                                                     ("faults.jsonl", faults_bad),
                                                     ("workload.jsonl", workload_bad)) if n},
    }


def _inside(path: str, root: str) -> bool:
    path, root = os.path.realpath(path), os.path.realpath(root)
    return path == root or path.startswith(root.rstrip(os.sep) + os.sep)


def _regular(path: str) -> bool:
    return os.path.isfile(path) and not os.path.islink(path)


def collect(run_dir: str, out_dir: str, pid: Optional[int] = None, skip_s: float = 700.0,
            include_node_logs: bool = False) -> Dict[str, Any]:
    run_dir, out_dir = os.path.abspath(run_dir), os.path.abspath(out_dir)
    if pid is not None:
        if pid <= 0:
            raise Refused(f"--pid must be a positive process id, not {pid}")
        if alive(pid):
            raise Refused(f"process {pid} is still running; collect only after the orchestrator has exited")
    if not os.path.isdir(run_dir):
        raise Refused(f"no run directory at {run_dir}")
    missing = [n for n in ("verdict.json", "report.txt") if not _regular(os.path.join(run_dir, n))]
    if missing:
        raise Refused(f"{' and '.join(missing)} missing in {run_dir}: the analyzer has not judged this run "
                      "(python3 -m arc_soak.analyze RUN_DIR writes both), and this collector never runs it")
    if _inside(out_dir, run_dir):
        raise Refused(f"{out_dir} is inside the run directory, which is never written")
    if os.path.lexists(out_dir):
        if os.path.islink(out_dir) or not os.path.isdir(out_dir) or os.listdir(out_dir):
            raise Refused(f"{out_dir} exists and is not an empty directory")
    else:
        os.makedirs(out_dir)

    names = sorted(os.listdir(run_dir))
    written: List[str] = []
    skipped: List[Dict[str, str]] = []
    for name in list(RECORDS) + [n for n in names if IDENT_TXT.match(n)]:
        src = os.path.join(run_dir, name)
        if not os.path.lexists(src):
            continue
        if not _regular(src):
            skipped.append({"file": name, "reason": "not a regular file"})
            continue
        shutil.copy2(src, os.path.join(out_dir, name))
        written.append(name)
    if include_node_logs:
        for name in names:
            if not NODE_LOG.match(name):
                continue
            src = os.path.join(run_dir, name)
            if not _regular(src):
                skipped.append({"file": name, "reason": "not a regular file"})
                continue
            # mtime=0 keeps the archive a function of the log's bytes alone.
            with open(src, "rb") as fin, open(os.path.join(out_dir, name + ".gz"), "wb") as raw:
                with gzip.GzipFile(filename=name, mode="wb", fileobj=raw, mtime=0) as gz:
                    shutil.copyfileobj(fin, gz, 1 << 20)
            written.append(name + ".gz")

    with open(os.path.join(out_dir, "growth.txt"), "w") as fh:
        fh.write(growth.report(run_dir, skip_s) + "\n")
    written.append("growth.txt")

    summary = summarize(run_dir)
    summary.update({
        "run_dir": run_dir,
        "collected_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "growth_skip_s": skip_s,
        "node_logs_included": include_node_logs,
        "files": sorted(written + ["summary.json"]),
        "skipped": skipped,
    })
    with open(os.path.join(out_dir, "summary.json"), "w") as fh:
        json.dump(summary, fh, indent=2, sort_keys=True, default=str)
        fh.write("\n")
    written.append("summary.json")

    # Last: a bundle is complete exactly when its manifest exists.
    lines = [f"{sha256_file(os.path.join(out_dir, rel))}  {rel}" for rel in sorted(written)]
    with open(os.path.join(out_dir, "MANIFEST.sha256"), "w") as fh:
        fh.write("\n".join(lines) + "\n")
    return {"out_dir": out_dir, "written": sorted(written) + ["MANIFEST.sha256"], "summary": summary}


def main(argv: Optional[List[str]] = None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--run", required=True, help="the soak work directory (read-only)")
    p.add_argument("--out", required=True, help="a new or empty directory for the bundle")
    p.add_argument("--pid", type=int, help="the orchestrator's pid; refuse while it is alive")
    p.add_argument("--skip-s", type=float, default=700.0,
                   help="warm-up excluded from the growth fit, in seconds")
    p.add_argument("--include-node-logs", action="store_true",
                   help="also copy node-N.M.log, gzip-compressed")
    a = p.parse_args(argv)
    try:
        result = collect(a.run, a.out, a.pid, a.skip_s, a.include_node_logs)
    except Refused as exc:
        print(f"REFUSED: {exc}", file=sys.stderr)
        return REFUSED
    print(json.dumps({"out": result["out_dir"], "files": len(result["written"]),
                      "verdict": result["summary"]["verdict"]}, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
