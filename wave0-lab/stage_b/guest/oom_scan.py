#!/usr/bin/env python3
"""Scan the KERNEL journal of every boot for out-of-memory kills and print one JSON object (runs inside the Wave 0 guest VM).

THROWAWAY LAB FILE. ARC-83 criterion E: "any OOM" fails, and the grep must cover the WHOLE window, not just the unit's
own log. Needs root to read the kernel journal: run it with sudo. Read-only.

Usage: sudo python3 oom_scan.py
Output: {"schema": "arc.legacy-bridge.wave0-lab.kernel-oom.v1", "scans": [{"boot", "first_entry", "last_entry", "kernel_lines", "count", "lines"}], "total": N}
"""
from __future__ import annotations

import json
import re
import subprocess
import sys

PATTERN = re.compile(r"out of memory|oom-kill|oom_reaper|killed process|invoked oom-killer", re.IGNORECASE)
SCHEMA = "arc.legacy-bridge.wave0-lab.kernel-oom.v1"


def journal(args: list[str]) -> str:
    try:
        done = subprocess.run(["journalctl", *args, "--no-pager"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, check=False)
    except OSError:
        return ""
    return done.stdout.decode("utf-8", "replace")


def boot_indexes() -> list[int]:
    indexes = []
    for line in journal(["--list-boots"]).splitlines():
        token = line.split()[:1]
        if token and re.fullmatch(r"-?\d+", token[0]):
            indexes.append(int(token[0]))
    return indexes or [0]


def scan_boot(index: int) -> dict:
    lines = [line for line in journal(["-k", "-b", str(index), "-o", "short-iso"]).splitlines() if line and not line.startswith("--")]
    hits = [line for line in lines if PATTERN.search(line)]
    return {
        "boot": index,
        "first_entry": lines[0][:40] if lines else None,
        "last_entry": lines[-1][:40] if lines else None,
        "kernel_lines": len(lines),
        "count": len(hits),
        "lines": hits[:50],
    }


def main() -> int:
    scans = [scan_boot(index) for index in boot_indexes()]
    print(json.dumps({"schema": SCHEMA, "scans": scans, "total": sum(scan["count"] for scan in scans)}, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
