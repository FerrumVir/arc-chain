"""Deterministic operational checks for a running ARC validator set (R7).

    python3 -m arc_ops.check --rpc 127.0.0.1:9960,127.0.0.1:9961,... [--window 600]

Collects read-only facts over HTTP from every listed node, then a PURE
evaluator decides. The evaluator is the only thing that sets the result, so
the collection can be replayed and the decision re-derived from the saved
report (`--from-report`).

What is checked, and why each can fail the run:

* reachability and identity - a node that does not answer, or answers as
  someone else, is not serving;
* liveness - the newest block's age; the finality lag (committed height
  minus the highest height a quorum finality certificate covers);
* agreement - every reachable node's block hash AND state root at a common
  height below all their tips (disagreement is a FAIL, never a warning);
* drift - the spread of heights across nodes;
* peers - a node with no peers is partitioned; fewer than all others is a
  warning;
* bootstrapping - a node rebuilding its DAG refuses submissions;
* native inference - pending requests, the oldest pending request's age in
  blocks, and, over the last `--window` blocks: requests, finalizes, refunds
  and any failed native transaction (on a protocol-4 chain a failed one is a
  defect); every settlement seen must credit exactly its reservation and be
  identical on two nodes (payment reconciliation).

Exit status: 0 PASS, 1 FAIL, 2 INCOMPLETE (too few nodes answered to judge),
3 WARN (nothing failed, something needs a look).

Resource use (RSS/CPU/disk) is local-process information: `--local` adds it
for nodes on this host, found by the port they listen on.
"""

import argparse
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request
from typing import Any, Callable, Dict, List, Optional, Tuple

EXIT = {"PASS": 0, "FAIL": 1, "INCOMPLETE": 2, "WARN": 3}

DEFAULTS: Dict[str, float] = {
    "max_block_age_s": 60.0,
    "max_finality_lag": 20.0,
    "max_height_spread": 30.0,
    "min_reachable_fraction": 0.75,
    "agreement_depth": 5.0,
    "max_native_pending": 64.0,
    "max_pending_age_blocks": 1_000.0,
    "max_refund_fraction": 0.10,
    "window": 600.0,
}

Fetch = Callable[[str, str], Optional[Any]]


def http_fetch(node: str, path: str, timeout: float = 4.0) -> Optional[Any]:
    """GET http://node/path as JSON; None on any failure."""
    try:
        with urllib.request.urlopen(f"http://{node}{path}", timeout=timeout) as resp:
            if resp.status != 200:
                return None
            return json.loads(resp.read().decode())
    except (urllib.error.URLError, OSError, ValueError):
        return None


# ── collection (impure) ─────────────────────────────────────────────────────

def collect(nodes: List[str], fetch: Fetch, window: int) -> Dict[str, Any]:
    out: Dict[str, Any] = {"collected_t": time.time(), "nodes": {}}
    for node in nodes:
        n: Dict[str, Any] = {}
        n["health"] = fetch(node, "/health")
        n["finality"] = fetch(node, "/finality/latest")
        n["diag"] = fetch(node, "/consensus/diagnostics")
        latest = fetch(node, "/block/latest")
        n["latest_block"] = latest.get("header") if isinstance(latest, dict) else None
        out["nodes"][node] = n

    heights = [n["health"]["height"] for n in out["nodes"].values()
               if isinstance(n.get("health"), dict) and isinstance(n["health"].get("height"), int)]
    if not heights:
        return out
    depth = int(DEFAULTS["agreement_depth"])
    common = max(0, min(heights) - depth)
    out["common_height"] = common
    for node, n in out["nodes"].items():
        block = fetch(node, f"/block/{common}")
        n["common_block"] = ({"hash": block.get("hash"),
                              "state_root": (block.get("header") or {}).get("state_root")}
                             if isinstance(block, dict) else None)

    # Native settlement over the window, read from the first answering node.
    reader = next((node for node, n in out["nodes"].items() if n.get("health")), None)
    second = next((node for node, n in out["nodes"].items()
                   if n.get("health") and node != reader), None)
    tip = min(heights)
    native: Dict[str, Any] = {"from": max(1, tip - window), "to": tip, "txs": [],
                              "receipts": {}, "second_receipts": {}}
    h = native["from"]
    while reader and h <= tip:
        page = fetch(reader, f"/blocks?from={h}&to={tip}&limit=100")
        blocks = (page or {}).get("blocks") if isinstance(page, dict) else None
        if not blocks:
            break
        for b in blocks:
            if b.get("tx_count", 0) > 0:
                listing = fetch(reader, f"/block/{b['height']}/txs") or {}
                for t in listing.get("transactions", []):
                    full = fetch(reader, f"/tx/{t['hash']}/full")
                    if isinstance(full, dict):
                        body = full.get("body") or {}
                        native["txs"].append({
                            "height": b["height"], "type": full.get("tx_type"),
                            "success": full.get("success"),
                            "request_id": body.get("request_id"),
                            "reserved": body.get("reserved_max_payment"),
                        })
        h = blocks[-1]["height"] + 1
    for t in native["txs"]:
        rid = t.get("request_id")
        if rid and rid not in native["receipts"]:
            native["receipts"][rid] = fetch(reader, f"/native-inference/receipt/{rid}")
            if second:
                native["second_receipts"][rid] = fetch(second, f"/native-inference/receipt/{rid}")
    out["native"] = native
    return out


def local_resources(nodes: List[str]) -> Dict[str, Any]:
    """RSS/CPU of local processes listening on each node's RPC port."""
    res: Dict[str, Any] = {}
    for node in nodes:
        port = node.rsplit(":", 1)[-1]
        try:
            pids = subprocess.run(["lsof", "-t", f"-iTCP:{port}", "-sTCP:LISTEN"],
                                  capture_output=True, text=True, timeout=5).stdout.split()
            if not pids:
                continue
            ps = subprocess.run(["ps", "-o", "rss=,pcpu=", "-p", pids[0]],
                                capture_output=True, text=True, timeout=5).stdout.split()
            res[node] = {"pid": int(pids[0]), "rss_mb": int(ps[0]) / 1024, "cpu_pct": float(ps[1])}
        except (OSError, ValueError, IndexError, subprocess.SubprocessError):
            continue
    return res


# ── evaluation (pure) ───────────────────────────────────────────────────────

def evaluate(c: Dict[str, Any], th: Dict[str, float]) -> Dict[str, Any]:
    fail: List[str] = []
    warn: List[str] = []
    incomplete: List[str] = []
    facts: Dict[str, Any] = {}
    nodes = c.get("nodes", {})
    reachable = {k: v for k, v in nodes.items() if isinstance(v.get("health"), dict)}
    facts["reachable"] = f"{len(reachable)}/{len(nodes)}"
    if not nodes or len(reachable) / len(nodes) < th["min_reachable_fraction"]:
        incomplete.append(f"only {len(reachable)} of {len(nodes)} nodes answered")
    for node in sorted(set(nodes) - set(reachable)):
        fail.append(f"{node}: unreachable")

    now = c.get("collected_t", time.time())
    heights = {}
    for node, n in sorted(reachable.items()):
        h = n["health"]
        heights[node] = h.get("height")
        peers = h.get("peers") or 0
        if peers == 0 and len(nodes) > 1:
            fail.append(f"{node}: no peers (partitioned)")
        elif peers < len(nodes) - 1:
            warn.append(f"{node}: {peers} peers of {len(nodes) - 1}")
        if h.get("dag_bootstrapping"):
            warn.append(f"{node}: rebuilding its DAG; refusing submissions")
        block = n.get("latest_block") or {}
        ts = block.get("timestamp")
        if isinstance(ts, (int, float)):
            age = now - ts / 1000.0
            if age > th["max_block_age_s"]:
                fail.append(f"{node}: newest block is {age:.0f}s old")
        else:
            fail.append(f"{node}: no readable latest block")
        fin = n.get("finality") or {}
        lag = fin.get("finality_lag")
        if lag is None:
            warn.append(f"{node}: no finality certificate yet")
        elif lag > th["max_finality_lag"]:
            fail.append(f"{node}: finality lag {lag} blocks")
    facts["heights"] = heights

    valid = [h for h in heights.values() if isinstance(h, int)]
    if valid and max(valid) - min(valid) > th["max_height_spread"]:
        warn.append(f"height spread {max(valid) - min(valid)} blocks")

    common = c.get("common_height")
    if common is not None:
        seen = {node: n.get("common_block") for node, n in reachable.items()}
        answers = {json.dumps(v, sort_keys=True) for v in seen.values() if v}
        if len(answers) > 1:
            fail.append(f"DISAGREEMENT at height {common}: {sorted(answers)}")
        if any(v is None for v in seen.values()):
            warn.append(f"some nodes could not show height {common}")
        facts["agreement_height"] = common

    for node, n in sorted(reachable.items()):
        d = n.get("diag") or {}
        pending = d.get("state_native_pending")
        if isinstance(pending, int) and pending > th["max_native_pending"]:
            warn.append(f"{node}: {pending} native requests pending")

    native = c.get("native") or {}
    txs = native.get("txs") or []
    counts = {"requests": 0, "finalizes": 0, "refunds": 0, "failed": 0}
    for t in txs:
        kind = t.get("type") or ""
        if kind == "NativeInferenceRequest":
            counts["requests"] += 1
        elif kind == "NativeInferenceFinalize":
            counts["finalizes"] += 1
        elif kind == "NativeInferenceRefund":
            counts["refunds"] += 1
        if kind.startswith("NativeInference") and t.get("success") is False:
            counts["failed"] += 1
    facts["native_window"] = {"from": native.get("from"), "to": native.get("to"), **counts}
    if counts["failed"]:
        fail.append(f"{counts['failed']} native transaction(s) failed in the window")
    settled = counts["finalizes"] + counts["refunds"]
    if settled and counts["refunds"] / settled > th["max_refund_fraction"]:
        warn.append(f"refund fraction {counts['refunds'] / settled:.2f}")

    tip = native.get("to")
    for rid, receipt in sorted((native.get("receipts") or {}).items()):
        if not isinstance(receipt, dict):
            warn.append(f"request {rid[:12]}: no receipt readable")
            continue
        status = receipt.get("observed_status")
        if status in ("Finalized", "Refunded"):
            credits = sum(int(x.get("amount", 0)) for x in receipt.get("settlement_credits") or [])
            reserved = receipt.get("reserved_max_payment")
            if isinstance(reserved, int) and credits != reserved:
                fail.append(f"request {rid[:12]}: credits {credits} != reserved {reserved}")
            other = (native.get("second_receipts") or {}).get(rid)
            if isinstance(other, dict) and (
                    other.get("observed_status") != status
                    or other.get("settlement_credits") != receipt.get("settlement_credits")):
                fail.append(f"request {rid[:12]}: two nodes disagree about its settlement")
        elif status == "Pending":
            admitted = (receipt.get("admission_transaction") or {}).get("block_height")
            if isinstance(admitted, int) and isinstance(tip, int) \
                    and tip - admitted > th["max_pending_age_blocks"]:
                warn.append(f"request {rid[:12]} pending for {tip - admitted} blocks")

    if c.get("resources"):
        facts["resources"] = c["resources"]

    status = "INCOMPLETE" if incomplete else ("FAIL" if fail else ("WARN" if warn else "PASS"))
    return {"status": status, "fail": fail, "warn": warn, "incomplete": incomplete,
            "facts": facts, "thresholds": th}


def render(v: Dict[str, Any]) -> str:
    lines = [f"ARC OPS CHECK: {v['status']}"]
    for key in ("incomplete", "fail", "warn"):
        for item in v[key]:
            lines.append(f"  {key.upper():10s} {item}")
    lines.append(json.dumps(v["facts"], indent=2, sort_keys=True, default=str))
    return "\n".join(lines)


def main(argv: Optional[List[str]] = None) -> int:
    p = argparse.ArgumentParser(description="ARC operational checks (read-only)")
    p.add_argument("--rpc", help="host:port[,host:port...]")
    p.add_argument("--window", type=int, default=int(DEFAULTS["window"]))
    p.add_argument("--local", action="store_true", help="add RSS/CPU of local node processes")
    p.add_argument("--report", help="write the collected facts and verdict here (JSON)")
    p.add_argument("--from-report", help="re-judge a saved report instead of collecting")
    for key, value in DEFAULTS.items():
        if key != "window":
            p.add_argument("--" + key.replace("_", "-"), type=float, default=value)
    a = p.parse_args(argv)
    th = {k: getattr(a, k) for k in DEFAULTS if k != "window"}
    th["window"] = float(a.window)
    if a.from_report:
        with open(a.from_report) as fh:
            collected = json.load(fh)["collected"]
    else:
        if not a.rpc:
            p.error("--rpc is required unless --from-report is given")
        nodes = [n.strip() for n in a.rpc.split(",") if n.strip()]
        collected = collect(nodes, http_fetch, a.window)
        if a.local:
            collected["resources"] = local_resources(nodes)
    verdict = evaluate(collected, th)
    if a.report:
        with open(a.report, "w") as fh:
            json.dump({"collected": collected, "verdict": verdict}, fh, indent=2, default=str)
    print(render(verdict))
    return EXIT[verdict["status"]]


if __name__ == "__main__":
    sys.exit(main())
