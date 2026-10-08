#!/usr/bin/env python3
"""Wave 0 lab sampler: one JSON line per interval, forever, across reboots (runs inside the guest VM).

THROWAWAY LAB FILE. An enabled systemd service runs this script. It is an observer only: it never
starts, stops or restarts anything, so a healthy sample right after a guest reboot proves the node
came back on its own. Lines are appended and fsynced; `seq` continues after a restart or reboot.
Absolute scheduling (start + n * interval) keeps the cadence from drifting.

Usage: sampler.py --out FILE --arc-dir DIR --interval SECONDS [--before-snapshot FILE] [--compare-every N]
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probe  # noqa: E402


def last_seq(path: str) -> int:
    try:
        with open(path, "rb") as handle:
            handle.seek(0, os.SEEK_END)
            size = handle.tell()
            handle.seek(max(0, size - 65536))
            lines = handle.read().decode("utf-8", "replace").splitlines()
    except OSError:
        return -1
    for line in reversed(lines):
        try:
            return int(json.loads(line)["seq"])
        except (ValueError, KeyError, TypeError):
            continue
    return -1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", required=True)
    parser.add_argument("--arc-dir", required=True)
    parser.add_argument("--interval", type=float, required=True)
    parser.add_argument("--before-snapshot", default="/var/lib/arc-w0/before-snapshot.json")
    parser.add_argument("--compare-every", type=int, default=10)
    parser.add_argument("--max-samples", type=int, default=0, help="stop after N samples (tests only)")
    args = parser.parse_args()

    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    seq = last_seq(args.out) + 1
    start = time.monotonic()
    count = 0
    while True:
        due = start + count * args.interval
        wait = due - time.monotonic()
        if wait > 0:
            time.sleep(wait)
        elif wait < -args.interval:
            # Far behind (the VM was paused, or a probe hung): skip ahead instead of bursting.
            count = int((time.monotonic() - start) / args.interval) + 1
            continue
        compare = args.compare_every > 0 and seq % args.compare_every == 0
        try:
            sample = probe.sample_once(args.arc_dir, seq, args.before_snapshot, compare)
        except Exception as error:  # noqa: BLE001 - the sampler must never die
            sample = {"seq": seq, "epoch": round(time.time(), 3), "boot_id": probe.boot_id(), "errors": [f"sample failed: {type(error).__name__}: {error}"]}
        with open(args.out, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(sample, sort_keys=True) + "\n")
            handle.flush()
            os.fsync(handle.fileno())
        seq += 1
        count += 1
        if args.max_samples and count >= args.max_samples:
            return 0


if __name__ == "__main__":
    sys.exit(main())
