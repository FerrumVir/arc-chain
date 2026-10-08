#!/usr/bin/env python3
"""Print one JSON object describing the install right now (runs inside the Wave 0 guest VM).

THROWAWAY LAB FILE. The host orchestrator calls this before and after every forced event and
compares the two objects field by field. Read-only.

Usage: capture_state.py --arc-dir DIR
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probe  # noqa: E402


def count_lines(path: str, needle: str | None = None) -> int:
    text = probe.read_text(path)
    if text is None:
        return 0
    lines = text.splitlines()
    return len(lines) if needle is None else sum(1 for line in lines if needle in line)


def capture(arc_dir: str) -> dict:
    sample = probe.sample_once(arc_dir, 0)
    updater = probe.systemd_props("arc-updater.service", ["Result", "ExecMainStatus", "ActiveState", "SubState"])
    bridge_log = os.path.join(arc_dir, "legacy-bridge", "bridge.log")
    update_log = os.path.join(arc_dir, "auto-update.log")
    new_path = os.path.join(arc_dir, "bin", "arc-node.new")
    return {
        "guest_epoch": round(time.time(), 3),
        "boot_id": sample["boot_id"],
        "bin_arc_node_sha256": sample["launcher_sha256"],
        "arc_node_prev_sha256": probe.sha256_file(os.path.join(arc_dir, "bin", "arc-node.prev")),
        "arc_node_new_size": os.path.getsize(new_path) if os.path.exists(new_path) else None,
        "version_txt": sample["version_txt"],
        "main_pid": sample["main_pid"],
        "proc_start_epoch": sample["proc_start_epoch"],
        "node_exe": sample["node_exe"],
        "node_exe_sha256": sample["node_exe_sha256"],
        "node_procs": sample["node_procs"],
        "node_state": sample["node_state"],
        "health_ok": sample["health_ok"],
        "info_ok": sample["info_ok"],
        "address": sample["address"],
        "stake": sample["stake"],
        "bridge_node_address": sample["bridge_node_address"],
        "legacy_fingerprint": sample["legacy_fingerprint"],
        "auto_update_log_lines": count_lines(update_log),
        "auto_update_rolled_back": count_lines(update_log, "ROLLED BACK"),
        "bridge_log_lines": count_lines(bridge_log),
        "bridge_log_reuse_count": count_lines(bridge_log, "reusing the verified"),
        "updater_unit": updater,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--arc-dir", required=True)
    args = parser.parse_args()
    print(json.dumps(capture(args.arc_dir), sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
