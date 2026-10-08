#!/usr/bin/env python3
"""Write one Stage A record: which launcher bytes an existing harness consumed, and whether it passed.

THROWAWAY LAB FILE. Each Stage A job runs one acceptance harness against the launcher bytes of the
digest-checked handoff artifact and then calls this script, which hashes the launcher file AFTER the
run and compares it with the hash taken BEFORE the run and with the digest the config pins.

  consumed   a v0.7 consumer harness ran with these bytes (PASS only if the harness exited 0 and
             before == after == pinned digest)
  smoke      the named exception for arc-node-linux-aarch64: no v0.7 linux-aarch64 baseline exists,
             so the asset is only EXECUTED as a smoke on ubuntu-24.04-arm

Usage:
  stage_a_record.py consumed --config C --asset A --launcher F --before SHA --harness H --job J \
      --runner R --exit-code N --out FILE [--installed FILE]
  stage_a_record.py smoke --config C --launcher F --before SHA --job J --runner R --smoke-exit N \
      --out FILE [--file-text FILE] [--version-text FILE]
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
from pathlib import Path

SCHEMA = "arc.legacy-bridge.wave0-lab.stage-a-record.v1"
AARCH64 = "arc-node-linux-aarch64"
AARCH64_RUNNER = "ubuntu-24.04-arm"
# Approved by work-99 (ARC-83 informed). Kept byte for byte equal to the L6/L8 constant
# AARCH64_EXCEPTION_STATEMENT in v0712_release_lib.py; a test compares the two.
AARCH64_EXCEPTION_STATEMENT = (
    "NOT CONSUMED: no v0.7 linux-aarch64 baseline exists (v0.7.7 shipped none), so no stranded v0.7 install "
    "can ever fetch this asset; EXECUTED as a smoke on ubuntu-24.04-arm."
)
EXCEPTION_KIND = "NOT_CONSUMED_NO_V07_BASELINE"
ENV_KEYS = ("RUNNER_OS", "RUNNER_ARCH", "RUNNER_NAME", "ImageOS", "ImageVersion", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_SHA")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def pinned_digest(config: dict, asset: str) -> str:
    return config["handoff"]["launchers"][asset]


def environment() -> dict:
    return {key: os.environ.get(key) for key in ENV_KEYS}


def build_consumed(config: dict, asset: str, launcher: Path, before: str, harness: str, job: str, runner: str, exit_code: int, installed: Path | None) -> dict:
    pinned = pinned_digest(config, asset)
    after = sha256_file(launcher)
    installed_sha = sha256_file(installed) if installed is not None and installed.is_file() else None
    passed = exit_code == 0 and before == after == pinned
    return {
        "schema": SCHEMA,
        "kind": "consumed",
        "asset": asset,
        "sha256": after,
        "expect_sha256": pinned,
        "consumed": True,
        "result": "PASS" if passed else "FAIL",
        "harness": harness,
        "job": job,
        "runner": runner,
        "sha256_before_run": before,
        "sha256_after_run": after,
        "exit_code": exit_code,
        "installed_sha256": installed_sha,
        "environment": environment(),
    }


def build_smoke(config: dict, launcher: Path, before: str, job: str, runner: str, smoke_exit: int, file_text: str, version_text: str) -> dict:
    pinned = pinned_digest(config, AARCH64)
    after = sha256_file(launcher)
    passed = smoke_exit == 0 and before == after == pinned and runner == AARCH64_RUNNER
    return {
        "schema": SCHEMA,
        "kind": "exception",
        "asset": AARCH64,
        "sha256": after,
        "expect_sha256": pinned,
        "exception_kind": EXCEPTION_KIND,
        "statement": AARCH64_EXCEPTION_STATEMENT,
        "executed": True,
        "smoke_result": "PASS" if passed else "FAIL",
        "runner": runner,
        "job": job,
        "sha256_before_run": before,
        "sha256_after_run": after,
        "smoke_exit_code": smoke_exit,
        "file_output": file_text,
        "version_output": version_text,
        "environment": environment(),
    }


def read_text(path: Path | None) -> str:
    if path is None or not path.is_file():
        return ""
    return path.read_text(encoding="utf-8", errors="replace").strip()[:2000]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="kind", required=True)
    consumed = sub.add_parser("consumed")
    consumed.add_argument("--config", type=Path, required=True)
    consumed.add_argument("--asset", required=True)
    consumed.add_argument("--launcher", type=Path, required=True)
    consumed.add_argument("--before", required=True)
    consumed.add_argument("--harness", required=True)
    consumed.add_argument("--job", required=True)
    consumed.add_argument("--runner", required=True)
    consumed.add_argument("--exit-code", type=int, required=True)
    consumed.add_argument("--installed", type=Path)
    consumed.add_argument("--out", type=Path, required=True)
    smoke = sub.add_parser("smoke")
    smoke.add_argument("--config", type=Path, required=True)
    smoke.add_argument("--launcher", type=Path, required=True)
    smoke.add_argument("--before", required=True)
    smoke.add_argument("--job", required=True)
    smoke.add_argument("--runner", required=True)
    smoke.add_argument("--smoke-exit", type=int, required=True)
    smoke.add_argument("--file-text", type=Path)
    smoke.add_argument("--version-text", type=Path)
    smoke.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    config = json.loads(args.config.read_text(encoding="utf-8"))
    if args.kind == "consumed":
        if args.asset not in config["handoff"]["launchers"]:
            print(f"unknown asset {args.asset}", file=sys.stderr)
            return 2
        if args.asset == AARCH64:
            print("arc-node-linux-aarch64 has no consuming harness; use the smoke record", file=sys.stderr)
            return 2
        record = build_consumed(config, args.asset, args.launcher, args.before, args.harness, args.job, args.runner, args.exit_code, args.installed)
        verdict = record["result"]
    else:
        record = build_smoke(config, args.launcher, args.before, args.job, args.runner, args.smoke_exit, read_text(args.file_text), read_text(args.version_text))
        verdict = record["smoke_result"]
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"{verdict} {record['asset']} {record['sha256']} ({args.out})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
