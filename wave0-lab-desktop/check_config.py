#!/usr/bin/env python3
"""Validate wave0-lab-desktop/config.json and print the job outputs the desktop workflow branches on.

THROWAWAY LAB FILE. The workflow reads this file because a push-triggered workflow cannot take inputs: to change what runs
(probe or full, which operating systems), commit a new config on the desktop lab branch.

Usage:
  python3 wave0-lab-desktop/check_config.py --config wave0-lab-desktop/config.json [--github-output FILE]

Exit status 0 when the config is valid, 1 otherwise (every problem is listed).
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Dict, List

SCHEMA = "arc.legacy-bridge.wave0-lab.desktop-config.v1"
REPOSITORY = "FerrumVir/arc-chain"
MODES = ("probe", "full")
APP_TAG = "v0.7.11"
LATEST_TAG = "v0.7.12"
BAIT_TAG = "v0.8.11"
PLUGIN_VERSION = "2.10.1"
MANIFEST_URL = "https://github.com/FerrumVir/arc-chain/releases/latest/download/latest.json"
SCENARIOS = ("latest-404", "latest-404-direct", "bait-0.8.11", "api-latest")
CASES = ("clean", "cached-bait")
REQUIRED_ASSETS = (
    "linux_deb",
    "linux_appimage",
    "linux_rpm",
    "windows_nsis",
    "windows_msi",
    "macos_arm64_dmg",
    "macos_arm64_app_tar",
    "macos_x64_dmg",
    "macos_x64_app_tar",
)
RUNNERS = {
    "linux": "ubuntu-24.04",
    "windows": "windows-latest",
    "macos_arm64": "macos-15",
    "macos_intel": "macos-15-intel",
}
HEX64 = re.compile(r"^[0-9a-f]{64}$")
HEX40 = re.compile(r"^[0-9a-f]{40}$")
HOSTNAME = re.compile(r"^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$")


def _is_int(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def _check_asset(path: str, asset: object, need) -> None:
    if not isinstance(asset, dict):
        need(path, False, "must be an object")
        return
    need(path + ".name", isinstance(asset.get("name"), str) and bool(asset.get("name")), "must be a file name")
    need(path + ".size", _is_int(asset.get("size")) and asset["size"] > 0, "must be a positive integer")
    need(path + ".sha256", bool(HEX64.match(str(asset.get("sha256", "")))), "must be 64 lowercase hex")
    need(path + ".release_digest", asset.get("release_digest") == "sha256:" + str(asset.get("sha256", "")), "must be sha256:<sha256>")


def validate(cfg: object) -> List[str]:
    errors: List[str] = []
    if not isinstance(cfg, dict):
        return ["config is not a JSON object"]

    def need(path: str, ok: bool, message: str) -> None:
        if not ok:
            errors.append("%s: %s" % (path, message))

    need("schema", cfg.get("schema") == SCHEMA, "must be %s" % SCHEMA)
    need("repository", cfg.get("repository") == REPOSITORY, "must be %s" % REPOSITORY)
    need("base_commit", bool(HEX40.match(str(cfg.get("base_commit", "")))), "must be 40 lowercase hex")
    need("base_tree", bool(HEX40.match(str(cfg.get("base_tree", "")))), "must be 40 lowercase hex")
    need("mode", cfg.get("mode") in MODES, "must be one of %s" % ", ".join(MODES))

    app = cfg.get("app")
    if not isinstance(app, dict):
        errors.append("app: missing object")
        app = {}
    need("app.tag", app.get("tag") == APP_TAG, "must be %s (the released app under test)" % APP_TAG)
    assets = app.get("assets")
    if not isinstance(assets, dict) or sorted(assets) != sorted(REQUIRED_ASSETS):
        errors.append("app.assets: must hold exactly %s" % ", ".join(REQUIRED_ASSETS))
        assets = {}
    for key, asset in assets.items():
        _check_asset("app.assets.%s" % key, asset, need)
    names = [asset.get("name") for asset in assets.values() if isinstance(asset, dict)]
    need("app.assets", len(set(names)) == len(names), "asset names must be distinct")
    _check_asset("app.manifest", app.get("manifest"), need)

    need("latest_tag", cfg.get("latest_tag") == LATEST_TAG, "must be %s (the launcher release that becomes Latest)" % LATEST_TAG)
    need("bait_tag", cfg.get("bait_tag") == BAIT_TAG, "must be %s" % BAIT_TAG)
    plugin = cfg.get("plugin")
    if not isinstance(plugin, dict):
        errors.append("plugin: missing object")
        plugin = {}
    need("plugin.name", plugin.get("name") == "tauri-plugin-updater", "must be tauri-plugin-updater")
    need("plugin.version", plugin.get("version") == PLUGIN_VERSION, "must be %s (pinned from the released binaries)" % PLUGIN_VERSION)
    need("manifest_url", cfg.get("manifest_url") == MANIFEST_URL, "must be %s" % MANIFEST_URL)

    hosts = cfg.get("hosts")
    if not isinstance(hosts, list) or not hosts:
        errors.append("hosts: must be a non-empty list")
        hosts = []
    for index, host in enumerate(hosts):
        need("hosts[%d]" % index, isinstance(host, str) and bool(HOSTNAME.match(host)), "must be a plain lowercase DNS name")
    need("hosts", "github.com" in hosts and "api.github.com" in hosts, "must contain github.com and api.github.com")
    need("hosts", len(set(hosts)) == len(hosts), "must not repeat a name")

    scenarios = cfg.get("scenarios")
    need("scenarios", isinstance(scenarios, list) and bool(scenarios) and all(item in SCENARIOS for item in scenarios), "must be a non-empty subset of %s" % ", ".join(SCENARIOS))
    cases = cfg.get("cases")
    need("cases", isinstance(cases, list) and sorted(cases) == sorted(CASES), "must be exactly %s" % ", ".join(CASES))
    need("interception_scope", isinstance(cfg.get("interception_scope"), str) and "NOT interceptable" in cfg.get("interception_scope", ""), "must state what the interception does NOT cover")

    jobs = cfg.get("jobs")
    if not isinstance(jobs, dict) or sorted(jobs) != sorted(RUNNERS):
        errors.append("jobs: must hold exactly %s" % ", ".join(sorted(RUNNERS)))
        jobs = {}
    for name, job in jobs.items():
        if not isinstance(job, dict):
            errors.append("jobs.%s: must be an object" % name)
            continue
        need("jobs.%s.runner" % name, job.get("runner") == RUNNERS[name], "must be %s" % RUNNERS[name])
        need("jobs.%s.enabled" % name, isinstance(job.get("enabled"), bool), "must be a boolean")
        need("jobs.%s.optional" % name, isinstance(job.get("optional"), bool), "must be a boolean")
    return errors


def outputs(cfg: Dict[str, object]) -> Dict[str, str]:
    def flag(value: object) -> str:
        return "true" if value else "false"

    jobs = cfg["jobs"]  # type: ignore[index]
    result = {"mode": str(cfg["mode"])}
    for name in ("linux", "windows", "macos_arm64", "macos_intel"):
        result[name] = flag(jobs[name]["enabled"])  # type: ignore[index]
    return result


def main(argv: List[str] = None) -> int:  # type: ignore[assignment]
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args(argv)
    try:
        cfg = json.loads(args.config.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        print("cannot read %s: %s" % (args.config, error), file=sys.stderr)
        return 1
    errors = validate(cfg)
    if errors:
        for error in errors:
            print("CONFIG ERROR %s" % error, file=sys.stderr)
        return 1
    values = outputs(cfg)
    for key, value in values.items():
        print("%s=%s" % (key, value))
    if args.github_output:
        with args.github_output.open("a", encoding="utf-8", newline="\n") as handle:
            for key, value in values.items():
                handle.write("%s=%s\n" % (key, value))
    return 0


if __name__ == "__main__":
    sys.exit(main())
