#!/usr/bin/env python3
"""Poll the node's LOCAL community status every few seconds and log every poll (runs inside the Wave 0 guest VM).

THROWAWAY LAB FILE. ARC-83 criterion D: the gaps between DISTINCT successful registration/heartbeat timestamps must be
bounded, and a 60 s sample can skip an intermediate success. The node schedules its next presence round 15 s after the
previous one completed (crates/arc-node/src/main.rs:5595, :9181, :9183), so the period is 15 s plus the round latency, and
stamps `last_registration_unix_ms` only when at least one coordinator accepted (crates/arc-node/src/community_worker.rs:156-164).
Polling every 5 s therefore observes EVERY distinct timestamp.
Every poll is written, including failed ones, so the evaluator can prove the poller itself had no coverage gap.

Observer only: it never starts or restarts anything.

Usage: heartbeat_poller.py --out FILE --interval SECONDS [--max-polls N]
Line:  {"obs_epoch": float, "ts_ms": int|null, "registered": int|null, "total": int|null}
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probe  # noqa: E402


def poll_once() -> dict:
    obs = time.time()
    ok, status = probe.http_json("/community/worker/status", timeout=2.0)
    status = status if (ok and isinstance(status, dict)) else {}
    stamp = status.get("last_registration_unix_ms")
    return {
        "obs_epoch": round(obs, 3),
        "ts_ms": stamp if isinstance(stamp, int) and not isinstance(stamp, bool) else None,
        "registered": status.get("coordinators_registered"),
        "total": status.get("coordinators_total"),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", required=True)
    parser.add_argument("--interval", type=float, default=5.0)
    parser.add_argument("--max-polls", type=int, default=0, help="stop after N polls (tests only)")
    args = parser.parse_args()
    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    start = time.monotonic()
    count = 0
    while True:
        due = start + count * args.interval
        wait = due - time.monotonic()
        if wait > 0:
            time.sleep(wait)
        elif wait < -args.interval:
            count = int((time.monotonic() - start) / args.interval) + 1
            continue
        try:
            record = poll_once()
        except Exception as error:  # noqa: BLE001 - the poller must never die
            record = {"obs_epoch": round(time.time(), 3), "ts_ms": None, "registered": None, "total": None, "error": type(error).__name__}
        with open(args.out, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(record, sort_keys=True) + "\n")
            handle.flush()
            os.fsync(handle.fileno())
        count += 1
        if args.max_polls and count >= args.max_polls:
            return 0


if __name__ == "__main__":
    sys.exit(main())
