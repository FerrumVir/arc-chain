#!/usr/bin/env python3
"""Collect the Wave 0 invariants (runs inside the guest VM). Read-only.

THROWAWAY LAB FILE. Writes arc.legacy-bridge.wave0-lab.invariants.v1 JSON. The BEFORE record takes its
legacy-data digest from the ExecStartPre hook snapshot taken when the kept bridge run started (the exact
moment systemd had stopped v0.7), so BEFORE and AFTER compare the same thing the repository's own
acceptance harness compares. Secrets never leave this file: the v0.7 seed appears only as a SHA-256,
unit files are hashed with the seed replaced, and argv values after --validator-seed are redacted.

Usage: invariants.py collect --label before|after --arc-dir DIR --out FILE [--legacy-from HOOK_SNAPSHOT_JSON]
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probe  # noqa: E402

SCHEMA = "arc.legacy-bridge.wave0-lab.invariants.v1"
UNITS = ("arc-node.service", "arc-updater.service", "arc-updater.timer")


def sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def unit_hashes(seed: str | None) -> dict:
    hashes = {}
    for unit in UNITS:
        text = probe.read_text(f"/etc/systemd/system/{unit}")
        if text is None:
            hashes[unit] = "missing"
            continue
        if seed:
            text = text.replace(seed, "<seed>")
        hashes[unit] = sha256_text(text)
    return hashes


def redacted_argv(pid: int) -> list[str]:
    raw = probe.read_text(f"/proc/{pid}/cmdline") or ""
    argv = [part for part in raw.split("\0") if part != ""]
    out = []
    redact_next = False
    for part in argv:
        if redact_next:
            out.append("<redacted>")
            redact_next = False
            continue
        out.append(part)
        if part in ("--validator-seed", "--insecure-dev-validator-seed"):
            redact_next = True
    return out


def timer_state() -> dict:
    _, active = probe.run(["systemctl", "is-active", "arc-updater.timer"])
    _, enabled = probe.run(["systemctl", "is-enabled", "arc-updater.timer"])
    return {"active": active.strip() == "active", "enabled": enabled.strip() == "enabled"}


def collect(label: str, arc_dir: str, legacy_from: str | None) -> dict:
    seed = probe.read_text(os.path.join(arc_dir, "identity.seed"))
    seed = seed.strip() if seed else None
    if legacy_from:
        with open(legacy_from, encoding="utf-8") as handle:
            hook = json.load(handle)
        entries = hook["entries"]
        source = f"ExecStartPre hook snapshot {os.path.basename(legacy_from)} (binary_sha256 {hook.get('binary_sha256')})"
    else:
        entries = probe.snapshot_entries(os.path.join(arc_dir, "data"))
        source = "live byte-level snapshot of the v0.7 data directory"
    sample = probe.sample_once(arc_dir, 0)
    state, node_dir = probe.bridge_state(arc_dir)
    state = state or {}
    pid = sample["main_pid"]
    record: dict = {
        "schema": SCHEMA,
        "label": label,
        "guest_epoch": round(time.time(), 3),
        "boot_id": sample["boot_id"],
        "legacy_snapshot_sha256": probe.entries_digest(entries),
        "legacy_snapshot_source": source,
        "legacy_entries": len(entries),
        "v07_seed_sha256": hashlib.sha256(seed.encode("utf-8")).hexdigest() if seed else None,
        "unit_files": unit_hashes(seed),
        "updater_timer": timer_state(),
        "installed": {
            "version_txt": sample["version_txt"],
            "bin_arc_node_sha256": sample["launcher_sha256"],
            "arc_node_prev_sha256": probe.sha256_file(os.path.join(arc_dir, "bin", "arc-node.prev")),
        },
    }
    if label == "after" or sample["node_exe"] and "/legacy-bridge/releases/" in sample["node_exe"]:
        _, health = probe.http_json("/health")
        _, info = probe.http_json("/node/info")
        _, community = probe.http_json("/community/worker/status")
        info = info if isinstance(info, dict) else {}
        health = health if isinstance(health, dict) else {}
        community = community if isinstance(community, dict) else {}
        record.update(
            {
                "node": {
                    "main_pid": pid,
                    "exe": sample["node_exe"],
                    "exe_sha256": sample["node_exe_sha256"],
                    "argv": redacted_argv(pid) if pid else [],
                    "node_dirs": [os.path.basename(path) for path in probe.bridge_node_dirs(arc_dir)],
                    "node_procs": sample["node_procs"],
                },
                "node_info": {
                    "stake": info.get("stake"),
                    "version": info.get("version"),
                    "validator": str(info.get("validator", "")).lower().removeprefix("0x"),
                },
                "health": {"chain_participation_enabled": health.get("chain_participation_enabled")},
                "bridge_state": {
                    "stake": state.get("stake"),
                    "node_address": state.get("node_address"),
                    "legacy_kind": state.get("legacy_kind"),
                    "compute": state.get("compute"),
                    "community_registration": state.get("community_registration"),
                    "archive_generation": state.get("archive_generation"),
                },
                "compute_consent": sample["compute_consent"],
                "community_status": {
                    "public_name": community.get("public_name"),
                    "coordinators_total": community.get("coordinators_total"),
                    "coordinators_registered": community.get("coordinators_registered"),
                },
            }
        )
    return record


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    one = sub.add_parser("collect")
    one.add_argument("--label", choices=("before", "after"), required=True)
    one.add_argument("--arc-dir", required=True)
    one.add_argument("--out", required=True)
    one.add_argument("--legacy-from")
    args = parser.parse_args()
    record = collect(args.label, args.arc_dir, args.legacy_from)
    with open(args.out, "w", encoding="utf-8") as handle:
        json.dump(record, handle, indent=2, sort_keys=True)
        handle.write("\n")
    print(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
