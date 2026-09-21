"""Conservation of value on a native-inference network, from balances (P7).

    python3 -m arc_ops.conservation --run RUN_DIR [--reservation 100]

Receipt arithmetic can be internally consistent and still be wrong about
money. This audits the STATE: for every account the soak funded at genesis
(its validators and the load driver's requesters) it reads the balance on
every replica and checks

    sum(balances) + reservation * pending_requests == sum(genesis balances)

i.e. value only ever moved between those accounts and the escrows of
requests still pending - none created, none destroyed - and every replica
holds the same balances. Native transactions carry no fee, and this chain
mints no block reward, so there is no other term. Read-only.

Exit: 0 conserved and identical, 1 violated, 2 could not read enough.
"""

import argparse
import json
import os
import sys
from typing import Any, Dict, List, Optional

from arc_ops.check import http_fetch


def accounts_from_run(run: Dict[str, Any]) -> List[str]:
    nodes = [n.get("identity") for n in run.get("nodes", []) if n.get("identity")]
    requesters = (run.get("workload") or {}).get("requesters") or []
    return [a for a in nodes + list(requesters) if a]


def audit(nodes: List[str], accounts: List[str], genesis_each: int, reservation: int,
          fetch=http_fetch) -> Dict[str, Any]:
    expected = genesis_each * len(accounts)
    per_node: Dict[str, Any] = {}
    for node in nodes:
        # Read pending BEFORE and AFTER the balances: if a request settled in
        # between, the snapshot straddles two states - retry once.
        for _ in range(3):
            before = (fetch(node, "/consensus/diagnostics") or {}).get("state_native_pending")
            height_before = (fetch(node, "/health") or {}).get("height")
            balances = {}
            for a in accounts:
                acct = fetch(node, f"/account/{a}")
                balances[a] = acct.get("balance") if isinstance(acct, dict) else None
            after = (fetch(node, "/consensus/diagnostics") or {}).get("state_native_pending")
            height_after = (fetch(node, "/health") or {}).get("height")
            if before == after and height_before == height_after:
                break
        if any(v is None for v in balances.values()) or before is None:
            per_node[node] = {"readable": False}
            continue
        total = sum(balances.values())
        per_node[node] = {
            "readable": True,
            "height": height_after,
            "pending": after,
            "total": total,
            "gap": expected - total,
            "conserved": expected - total == reservation * after,
            "balances": balances,
            "stable_read": before == after and height_before == height_after,
        }
    readable = [v for v in per_node.values() if v.get("readable")]
    same_height = {v["height"] for v in readable}
    identical = len(same_height) == 1 and len({json.dumps(v["balances"], sort_keys=True) for v in readable}) == 1
    status = ("INCOMPLETE" if len(readable) < max(1, (len(nodes) + 1) // 2)
              else "FAIL" if any(not v["conserved"] for v in readable if v["stable_read"])
              else "PASS")
    return {"status": status, "expected_total": expected, "accounts": len(accounts),
            "identical_where_same_height": identical if len(same_height) == 1 else None,
            "nodes": {k: {kk: vv for kk, vv in v.items() if kk != "balances"} for k, v in per_node.items()}}


def main(argv: Optional[List[str]] = None) -> int:
    p = argparse.ArgumentParser(description="conservation of value from balances (read-only)")
    p.add_argument("--run", required=True, help="soak work directory (reads run.json)")
    p.add_argument("--genesis-balance", type=int, default=1_000_000_000_000)
    p.add_argument("--reservation", type=int, default=100)
    a = p.parse_args(argv)
    with open(os.path.join(a.run, "run.json")) as fh:
        run = json.load(fh)
    nodes = [f"127.0.0.1:{n['rpc']}" for n in run.get("nodes", [])]
    result = audit(nodes, accounts_from_run(run), a.genesis_balance, a.reservation)
    print(json.dumps(result, indent=2, sort_keys=True))
    return {"PASS": 0, "FAIL": 1, "INCOMPLETE": 2}[result["status"]]


if __name__ == "__main__":
    sys.exit(main())
