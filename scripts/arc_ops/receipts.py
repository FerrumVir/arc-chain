import argparse
import json
import os
import sys
from typing import Any, Callable, Dict, List, Optional

from arc_ops.check import http_fetch

Fetch = Callable[[str, str], Optional[Any]]

OPTIONAL_FIELDS = ("expires_at", "requester", "output_hex")
TERMINAL = ("Finalized", "Refunded")


def fingerprint(receipt: Dict[str, Any]) -> str:
    credits = sorted(f"{c.get('payee')}:{c.get('amount')}"
                     for c in receipt.get("settlement_credits") or [])
    terminal = (receipt.get("terminal_transaction") or {}).get("block_height")
    return "|".join([str(receipt.get("observed_status")), str(receipt.get("output_hash") or ""),
                     str(terminal if terminal is not None else ""), ",".join(credits)])


def optional_fields_agree(receipts: List[Dict[str, Any]]) -> bool:
    for field in OPTIONAL_FIELDS:
        reported = {json.dumps(r.get(field)) for r in receipts if r.get(field) not in (None, "")}
        if len(reported) > 1:
            return False
    return True


def arithmetic(receipt: Dict[str, Any]) -> List[str]:
    problems: List[str] = []
    status = receipt.get("observed_status")
    if status not in TERMINAL:
        return problems
    credits = receipt.get("settlement_credits") or []
    reserved = receipt.get("reserved_max_payment")
    price = receipt.get("execution_price")
    if not isinstance(reserved, int) or sum(int(c.get("amount", 0)) for c in credits) != reserved:
        problems.append("credits do not add up to the reservation")
    requester = receipt.get("requester")
    if requester and isinstance(reserved, int):
        back = sum(int(c.get("amount", 0)) for c in credits if c.get("payee") == requester)
        if status == "Refunded" and back != reserved:
            problems.append("a refund did not return the whole reservation to the requester")
        if status == "Finalized" and isinstance(price, int) and back < reserved - price:
            problems.append("the requester got back less than reservation - price")
    return problems


def reconcile(nodes: List[str], request_id: str, fetch: Fetch = http_fetch) -> Dict[str, Any]:
    answers = {}
    for node in nodes:
        receipt = fetch(node, f"/native-inference/receipt/{request_id}")
        answers[node] = receipt if isinstance(receipt, dict) else None
    known = {n: r for n, r in answers.items() if r is not None}
    problems: List[str] = []
    for node, receipt in known.items():
        if str(receipt.get("request_id", "")).removeprefix("0x") != request_id.removeprefix("0x"):
            problems.append(f"{node} answered for another request")
        problems.extend(f"{node}: {p}" for p in arithmetic(receipt))
    prints = {fingerprint(r) for r in known.values()}
    agree = len(prints) <= 1 and optional_fields_agree(list(known.values()))
    if known and not agree:
        problems.append("replicas record different settlements")
    enough = len(known) >= max(1, (len(nodes) + 1) // 2)
    sample = next(iter(known.values()), {})
    status = ("INCOMPLETE" if not enough else "FAIL" if problems else "PASS")
    return {
        "request_id": request_id,
        "status": status,
        "asked": len(nodes),
        "answered": len(known),
        "problems": problems,
        "observed_status": sample.get("observed_status"),
        "output_hash": sample.get("output_hash"),
        "execution_price": sample.get("execution_price"),
        "reserved_max_payment": sample.get("reserved_max_payment"),
        "settlement_credits": sample.get("settlement_credits"),
        "expires_at": sample.get("expires_at"),
    }


def request_ids_from_journal(path: str) -> List[str]:
    with open(path) as fh:
        journal = json.load(fh)
    seen: List[str] = []
    for record in journal.get("records", []):
        if record.get("kind") == "request" and record.get("request_id") not in seen:
            seen.append(record["request_id"])
    return seen


def main(argv: Optional[List[str]] = None) -> int:
    p = argparse.ArgumentParser(description="reconcile native receipts across replicas (read-only)")
    p.add_argument("--nodes", help="comma-separated host:port list")
    p.add_argument("--run", help="soak work directory (its run.json names the nodes)")
    p.add_argument("--journal", help="a desktop native-requests.json whose requests to reconcile")
    p.add_argument("request_ids", nargs="*")
    a = p.parse_args(argv)
    if a.nodes:
        nodes = [n.strip() for n in a.nodes.split(",") if n.strip()]
    elif a.run:
        with open(os.path.join(a.run, "run.json")) as fh:
            nodes = [f"127.0.0.1:{n['rpc']}" for n in json.load(fh).get("nodes", [])]
    else:
        p.error("give --nodes or --run")
    ids = list(a.request_ids)
    if a.journal:
        ids.extend(i for i in request_ids_from_journal(a.journal) if i not in ids)
    if not ids:
        p.error("no request ids to reconcile")
    results = [reconcile(nodes, request_id) for request_id in ids]
    print(json.dumps(results, indent=2, sort_keys=True))
    statuses = {r["status"] for r in results}
    return 1 if "FAIL" in statuses else 2 if "INCOMPLETE" in statuses else 0


if __name__ == "__main__":
    sys.exit(main())
