#!/usr/bin/env python3
"""Validate wave0-lab/config.json and print the job outputs the workflows branch on.

THROWAWAY LAB FILE. This branch is never merged. The workflows read this file
because a push-triggered workflow cannot take inputs: to change what runs,
commit a new config on the lab branch.

Usage:
  python3 wave0-lab/check_config.py --config wave0-lab/config.json [--github-output FILE]

Exit status 0 when the config is valid, 1 otherwise (every problem is listed).
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

SCHEMA = "arc.legacy-bridge.wave0-lab.config.v1"
REPOSITORY = "FerrumVir/arc-chain"
ASSETS = (
    "arc-node-linux-aarch64",
    "arc-node-linux-x86_64",
    "arc-node-macos-arm64",
    "arc-node-macos-x86_64",
    "arc-node-windows-x86_64.exe",
)
X86_ASSET = "arc-node-linux-x86_64"
HEX64 = re.compile(r"^[0-9a-f]{64}$")
HEX40 = re.compile(r"^[0-9a-f]{40}$")
TAG = re.compile(r"^v0\.7\.[0-9]+$")
IMAGE_URL = re.compile(r"^https://cloud-images\.ubuntu\.com/releases/noble/release-[0-9]{8}/ubuntu-24\.04-server-cloudimg-amd64\.img$")

# Astra's ARC-83 numbers. A profile named "full" may never be weaker than these.
FULL_FLOORS = {
    "min_total_s": 14400,
    "min_steady_s": 7200,
    "min_steady_samples": 121,
    "post_reboot_healthy_s": 600,
    "updater_runs": 2,
    "kickstarts": 3,
}
FULL_MAX_INTERVAL_S = 60
PROFILE_INT_KEYS = (
    "sample_interval_s",
    "baseline_s",
    "min_total_s",
    "min_steady_s",
    "min_steady_samples",
    "post_reboot_healthy_s",
    "reboot_recovery_deadline_s",
    "forced_grace_s",
    "updater_runs",
    "kickstarts",
    "kickstart_gap_s",
    "heartbeat_s",
    "pull_samples_s",
    "deadline_min",
)
# The hosted-runner job limit is 360 minutes; the soak step must leave room to collect.
MAX_DEADLINE_MIN = 330


def _is_int(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def validate(cfg: object) -> list[str]:
    errors: list[str] = []
    if not isinstance(cfg, dict):
        return ["config is not a JSON object"]

    def need(path: str, ok: bool, message: str) -> None:
        if not ok:
            errors.append(f"{path}: {message}")

    need("schema", cfg.get("schema") == SCHEMA, f"must be {SCHEMA}")
    need("repository", cfg.get("repository") == REPOSITORY, f"must be {REPOSITORY}")
    need("base_commit", bool(HEX40.match(str(cfg.get("base_commit", "")))), "must be 40 lowercase hex")
    need("base_tree", bool(HEX40.match(str(cfg.get("base_tree", "")))), "must be 40 lowercase hex")

    handoff = cfg.get("handoff")
    if not isinstance(handoff, dict):
        errors.append("handoff: missing object")
        handoff = {}
    need("handoff.run_id", _is_int(handoff.get("run_id")) and handoff["run_id"] > 0, "must be a positive integer")
    need("handoff.artifact_id", _is_int(handoff.get("artifact_id")) and handoff["artifact_id"] > 0, "must be a positive integer")
    need("handoff.artifact_name", handoff.get("artifact_name") == "legacy-bridge-release-handoff", "must be legacy-bridge-release-handoff")
    digest = str(handoff.get("artifact_digest", ""))
    need("handoff.artifact_digest", digest.startswith("sha256:") and bool(HEX64.match(digest[7:])), "must be sha256:<64 hex>")
    need("handoff.artifact_size", _is_int(handoff.get("artifact_size")) and handoff["artifact_size"] > 0, "must be a positive integer")
    need("handoff.tag", bool(TAG.match(str(handoff.get("tag", "")))), "must look like v0.7.N")
    launchers = handoff.get("launchers")
    if not isinstance(launchers, dict) or sorted(launchers) != sorted(ASSETS):
        errors.append(f"handoff.launchers: must hold exactly {', '.join(ASSETS)}")
        launchers = {}
    for asset, value in launchers.items():
        need(f"handoff.launchers.{asset}", bool(HEX64.match(str(value))), "must be 64 lowercase hex")

    stage_a = cfg.get("stage_a")
    if not isinstance(stage_a, dict):
        errors.append("stage_a: missing object")
        stage_a = {}
    for key in ("enabled", "macos_arm64", "macos_intel", "aarch64_smoke"):
        need(f"stage_a.{key}", isinstance(stage_a.get(key), bool), "must be a boolean")

    stage_b = cfg.get("stage_b")
    if not isinstance(stage_b, dict):
        errors.append("stage_b: missing object")
        stage_b = {}
    need("stage_b.enabled", isinstance(stage_b.get("enabled"), bool), "must be a boolean")
    profiles = stage_b.get("profiles")
    if not isinstance(profiles, dict) or not {"smoke", "full"} <= set(profiles):
        errors.append("stage_b.profiles: must define smoke and full")
        profiles = {}
    need("stage_b.profile", stage_b.get("profile") in profiles, "must name a defined profile")
    need("stage_b.launcher_source", stage_b.get("launcher_source") in ("artifact", "published"), "must be artifact or published")
    need("stage_b.tag", stage_b.get("tag") == handoff.get("tag"), "must equal handoff.tag")
    expect = str(stage_b.get("expect_sha256", ""))
    need("stage_b.expect_sha256", bool(HEX64.match(expect)), "must be 64 lowercase hex")
    need(
        "stage_b.expect_sha256",
        expect == launchers.get(X86_ASSET),
        "must equal the handoff arc-node-linux-x86_64 digest (the final launcher)",
    )
    live = stage_b.get("live_network")
    need("stage_b.live_network", live in ("allowed", "blocked"), "must be allowed or blocked")
    if live == "allowed":
        authorization = str(stage_b.get("live_network_authorization", ""))
        need(
            "stage_b.live_network_authorization",
            len(authorization) >= 80 and "stake-0" in authorization and "TJ" in authorization,
            "live registration needs the recorded authorization text (who authorized, stake-0, clean environment)",
        )
    image = stage_b.get("image")
    if not isinstance(image, dict):
        errors.append("stage_b.image: missing object")
        image = {}
    need("stage_b.image.url", bool(IMAGE_URL.match(str(image.get("url", "")))), "must be a dated Ubuntu 24.04 amd64 cloud image URL")
    need("stage_b.image.sha256", bool(HEX64.match(str(image.get("sha256", "")))), "must be 64 lowercase hex")
    need("stage_b.image.size", _is_int(image.get("size")) and image["size"] > 0, "must be a positive integer")
    vm = stage_b.get("vm")
    if not isinstance(vm, dict):
        errors.append("stage_b.vm: missing object")
        vm = {}
    for key, low, high in (("cpus", 1, 4), ("memory_mb", 2048, 12288), ("disk_gb", 10, 60)):
        need(f"stage_b.vm.{key}", _is_int(vm.get(key)) and low <= vm[key] <= high, f"must be an integer from {low} to {high}")

    for name, profile in profiles.items():
        if not isinstance(profile, dict):
            errors.append(f"stage_b.profiles.{name}: must be an object")
            continue
        for key in PROFILE_INT_KEYS:
            need(f"stage_b.profiles.{name}.{key}", _is_int(profile.get(key)) and profile[key] > 0, "must be a positive integer")
        factor = profile.get("max_gap_factor")
        need(f"stage_b.profiles.{name}.max_gap_factor", isinstance(factor, (int, float)) and not isinstance(factor, bool) and 1 < factor <= 5, "must be a number above 1 and at most 5")
        deadline = profile.get("deadline_min")
        if _is_int(deadline):
            need(f"stage_b.profiles.{name}.deadline_min", deadline <= MAX_DEADLINE_MIN, f"must be at most {MAX_DEADLINE_MIN}")
        if name == "full":
            for key, floor in FULL_FLOORS.items():
                value = profile.get(key)
                need(f"stage_b.profiles.full.{key}", _is_int(value) and value >= floor, f"may not be below Astra's floor {floor}")
            interval = profile.get("sample_interval_s")
            need("stage_b.profiles.full.sample_interval_s", _is_int(interval) and interval <= FULL_MAX_INTERVAL_S, f"may not exceed {FULL_MAX_INTERVAL_S}")
            if _is_int(profile.get("min_total_s")) and _is_int(deadline):
                need(
                    "stage_b.profiles.full.deadline_min",
                    deadline * 60 >= profile["min_total_s"] + 1800,
                    "must leave at least 30 minutes beyond min_total_s for setup, the battery and collection",
                )
    return errors


def outputs(cfg: dict) -> dict[str, str]:
    stage_a = cfg["stage_a"]
    stage_b = cfg["stage_b"]

    def flag(value: bool) -> str:
        return "true" if value else "false"

    return {
        "stage_a": flag(stage_a["enabled"]),
        "stage_a_macos_arm64": flag(stage_a["enabled"] and stage_a["macos_arm64"]),
        "stage_a_macos_intel": flag(stage_a["enabled"] and stage_a["macos_intel"]),
        "stage_a_aarch64": flag(stage_a["enabled"] and stage_a["aarch64_smoke"]),
        "stage_b": flag(stage_b["enabled"]),
        "stage_b_profile": stage_b["profile"],
        "stage_b_launcher_source": stage_b["launcher_source"],
        "stage_b_live_network": stage_b["live_network"],
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args(argv)
    try:
        cfg = json.loads(args.config.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        print(f"cannot read {args.config}: {error}", file=sys.stderr)
        return 1
    errors = validate(cfg)
    if errors:
        for error in errors:
            print(f"CONFIG ERROR {error}", file=sys.stderr)
        return 1
    values = outputs(cfg)
    for key, value in values.items():
        print(f"{key}={value}")
    if args.github_output:
        with args.github_output.open("a", encoding="utf-8") as handle:
            for key, value in values.items():
                handle.write(f"{key}={value}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
