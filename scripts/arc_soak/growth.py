"""Which in-memory collection grows with height, and by how much.

    python3 -m arc_soak.growth RUN_DIR [--skip-s 120]

Reads a run's diag.jsonl (every node's /consensus/diagnostics each tick)
and samples.jsonl (height, RSS, disk, log bytes), and fits each series
against the node's height over the steady window: after `--skip-s` of
warm-up, and only within one process incarnation (a restart resets every
in-memory size, which would otherwise read as shrinkage).

Output, per node incarnation: the slope of every gauge_*, engine_*,
state_* and mempool series in entries per 1,000 heights, RSS in MB per
1,000 heights, and disk/log bytes the same way. A soak needs every
collection that is not the chain's own history (blocks, receipts, bodies,
indexes) flat; this names the ones that are not.

Pure function of the records - it never talks to a node.
"""

import argparse
import json
import math
import os
import sys
from collections import defaultdict
from typing import Dict, List, Optional, Tuple

HISTORY = {"state_blocks", "state_receipts", "state_tx_index", "state_full_transactions",
           "state_account_txs_entries", "state_event_log_heights", "state_event_log_entries"}
PREFIXES = ("gauge_", "engine_", "state_", "mempool_", "dag_blocks", "finality_certificates_held",
            "pending_blocks_now")


def _read(path: str) -> List[dict]:
    out = []
    if not os.path.exists(path):
        return out
    with open(path) as fh:
        for line in fh:
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    return out


def fit(points: List[Tuple[float, float]]) -> Optional[Tuple[float, float]]:
    """Least-squares slope and r for (x, y); None when x does not vary."""
    n = len(points)
    if n < 3:
        return None
    mx = sum(p[0] for p in points) / n
    my = sum(p[1] for p in points) / n
    sxx = sum((p[0] - mx) ** 2 for p in points)
    syy = sum((p[1] - my) ** 2 for p in points)
    sxy = sum((p[0] - mx) * (p[1] - my) for p in points)
    if sxx <= 0:
        return None
    slope = sxy / sxx
    r = sxy / math.sqrt(sxx * syy) if syy > 0 else 0.0
    return slope, r


def series(run_dir: str, skip_s: float) -> Dict[Tuple[int, int], Dict[str, List[Tuple[float, float]]]]:
    """(node, incarnation) -> metric -> [(height, value)] over the steady window."""
    diag = _read(os.path.join(run_dir, "diag.jsonl"))
    samples = _read(os.path.join(run_dir, "samples.jsonl"))
    first_t: Dict[Tuple[int, int], float] = {}
    for rec in diag + samples:
        key = (rec.get("node"), rec.get("incarnation", 0))
        t = rec.get("t")
        if isinstance(t, (int, float)):
            first_t[key] = min(first_t.get(key, t), t)
    out: Dict[Tuple[int, int], Dict[str, List[Tuple[float, float]]]] = defaultdict(lambda: defaultdict(list))
    for rec in diag:
        key = (rec.get("node"), rec.get("incarnation", 0))
        d = rec.get("diag") or {}
        h = d.get("height")
        if not isinstance(h, (int, float)) or rec.get("t", 0) < first_t[key] + skip_s:
            continue
        for name, value in d.items():
            if name.startswith(PREFIXES) and isinstance(value, (int, float)):
                out[key][name].append((float(h), float(value)))
    for rec in samples:
        key = (rec.get("node"), rec.get("incarnation", 0))
        h = rec.get("height")
        if not isinstance(h, (int, float)) or rec.get("t", 0) < first_t.get(key, 0) + skip_s:
            continue
        for name, scale in (("rss_kb", 1 / 1024), ("disk_bytes", 1 / 1048576),
                            ("log_bytes", 1 / 1048576)):
            v = rec.get(name)
            if isinstance(v, (int, float)):
                label = {"rss_kb": "rss_mb", "disk_bytes": "disk_mb", "log_bytes": "log_mb"}[name]
                out[key][label].append((float(h), float(v) * scale))
    return out


def report(run_dir: str, skip_s: float = 120.0, min_slope: float = 0.5) -> str:
    lines = [f"growth per 1,000 heights over the steady window (skip {skip_s:.0f} s)"]
    for key, metrics in sorted(series(run_dir, skip_s).items(), key=lambda kv: str(kv[0])):
        node, inc = key
        heights = sorted({h for pts in metrics.values() for h, _ in pts})
        if len(heights) < 3:
            lines.append(f"node {node} incarnation {inc}: too few samples")
            continue
        lines.append(f"node {node} incarnation {inc}: heights {heights[0]:.0f}..{heights[-1]:.0f}")
        rows = []
        for name, pts in metrics.items():
            f = fit(pts)
            if f is None:
                continue
            slope, r = f
            per_k = slope * 1000
            last = pts[-1][1]
            rows.append((abs(per_k), name, per_k, r, last))
        rows.sort(reverse=True)
        for _, name, per_k, r, last in rows:
            if abs(per_k) < min_slope and name not in ("rss_mb", "disk_mb", "log_mb"):
                continue
            tag = "history" if name in HISTORY else ("resource" if name.endswith("_mb") else "LEAK?")
            lines.append(f"  {name:40s} {per_k:12.2f}/1k   r={r:5.2f}   last={last:,.1f}   {tag}")
    return "\n".join(lines)


def main(argv: Optional[List[str]] = None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("run_dir")
    p.add_argument("--skip-s", type=float, default=120.0)
    p.add_argument("--min-slope", type=float, default=0.5,
                   help="hide collections growing slower than this many entries per 1k heights")
    a = p.parse_args(argv)
    print(report(a.run_dir, a.skip_s, a.min_slope))
    return 0


if __name__ == "__main__":
    sys.exit(main())
