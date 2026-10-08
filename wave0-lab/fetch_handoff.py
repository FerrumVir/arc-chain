#!/usr/bin/env python3
"""Download the L6 handoff artifact and prove it is the exact one the config pins.

THROWAWAY LAB FILE. Every Stage A job calls this itself, so each job consumes
bytes that were digest-checked inside that job, never bytes passed between jobs.

Checks (all must hold, every failure is listed):
  * the artifact record (GET /actions/artifacts/<id>): name, not expired, the
    pinned digest and size, produced by the pinned handoff run at the base commit;
  * the downloaded zip hashes to the pinned digest;
  * the zip holds exactly the nine expected members and no path escapes;
  * v0.7.12/SHA256SUMS lists exactly the five launchers and equals the config;
  * every launcher file hashes to its config digest;
  * legacy-bridge-provenance.json has the handoff schema and is not eligible for latest.

Usage: python3 wave0-lab/fetch_handoff.py --config wave0-lab/config.json --out handoff
Needs the gh CLI and GH_TOKEN (the job token with actions: read).
"""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import re
import subprocess
import sys
import zipfile
from pathlib import Path
from typing import Callable

ASSETS = (
    "arc-node-linux-aarch64",
    "arc-node-linux-x86_64",
    "arc-node-macos-arm64",
    "arc-node-macos-x86_64",
    "arc-node-windows-x86_64.exe",
)
EXPECTED_MEMBERS = frozenset(
    ["RELEASE-NOTES.md", "legacy-bridge-provenance.json", "v0.7.12/SHA256SUMS", "v0.7.12/latest.json"]
    + [f"v0.7.12/{asset}" for asset in ASSETS]
)
PROVENANCE_SCHEMA = "arc.legacy-bridge.handoff.v1"
MAX_MEMBER_BYTES = 64 * 1024 * 1024
HEX64 = re.compile(r"^[0-9a-f]{64}$")

GhRunner = Callable[[list[str]], bytes]


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def run_gh(args: list[str]) -> bytes:
    """Plain GET through the gh CLI. Only read-only API paths are ever requested."""
    if len(args) < 2 or args[0] != "api" or any(flag in args for flag in ("-X", "--method", "-f", "-F", "--field", "--raw-field", "--input")):
        raise SystemExit(f"refusing a non-GET gh call: {args}")
    result = subprocess.run(["gh", *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if result.returncode != 0:
        raise SystemExit(f"gh {' '.join(args)} failed ({result.returncode}): {result.stderr.decode('utf-8', 'replace').strip()}")
    return result.stdout


def parse_sums(text: str) -> dict[str, str]:
    sums: dict[str, str] = {}
    for line in text.splitlines():
        if not line.strip():
            continue
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9._-]+)", line)
        if not match:
            raise ValueError(f"malformed SHA256SUMS line: {line!r}")
        if match.group(2) in sums:
            raise ValueError(f"duplicate SHA256SUMS entry for {match.group(2)}")
        sums[match.group(2)] = match.group(1)
    return sums


def safe_members(archive: zipfile.ZipFile) -> dict[str, zipfile.ZipInfo]:
    members: dict[str, zipfile.ZipInfo] = {}
    for info in archive.infolist():
        name = info.filename
        if info.is_dir():
            continue
        if name.startswith("/") or "\\" in name or ".." in name.split("/") or ":" in name.split("/")[0]:
            raise ValueError(f"unsafe member path {name!r}")
        if (info.external_attr >> 16) & 0o170000 == 0o120000:
            raise ValueError(f"symbolic link member {name!r}")
        if info.file_size > MAX_MEMBER_BYTES:
            raise ValueError(f"member {name!r} is larger than {MAX_MEMBER_BYTES} bytes")
        if name in members:
            raise ValueError(f"duplicate member {name!r}")
        members[name] = info
    return members


def fetch(config: dict, out: Path, gh: GhRunner = run_gh) -> dict:
    """Return the verification record; raise ValueError listing every failed condition."""
    repository = config["repository"]
    handoff = config["handoff"]
    problems: list[str] = []

    record = json.loads(gh(["api", f"repos/{repository}/actions/artifacts/{handoff['artifact_id']}"]).decode("utf-8"))
    run = record.get("workflow_run") or {}
    if record.get("id") != handoff["artifact_id"]:
        problems.append(f"artifact id {record.get('id')} != {handoff['artifact_id']}")
    if record.get("name") != handoff["artifact_name"]:
        problems.append(f"artifact name {record.get('name')!r} != {handoff['artifact_name']!r}")
    if record.get("expired") is not False:
        problems.append("the artifact is expired")
    if record.get("digest") != handoff["artifact_digest"]:
        problems.append(f"artifact digest {record.get('digest')} != pinned {handoff['artifact_digest']}")
    if record.get("size_in_bytes") != handoff["artifact_size"]:
        problems.append(f"artifact size {record.get('size_in_bytes')} != pinned {handoff['artifact_size']}")
    if run.get("id") != handoff["run_id"]:
        problems.append(f"artifact run {run.get('id')} != pinned run {handoff['run_id']}")
    if run.get("head_sha") != config["base_commit"]:
        problems.append(f"artifact run head {run.get('head_sha')} != base commit {config['base_commit']}")

    zip_bytes = gh(["api", f"repos/{repository}/actions/artifacts/{handoff['artifact_id']}/zip"])
    zip_digest = "sha256:" + sha256_bytes(zip_bytes)
    if zip_digest != handoff["artifact_digest"]:
        problems.append(f"downloaded zip hashes to {zip_digest}, pinned {handoff['artifact_digest']}")
    if len(zip_bytes) != handoff["artifact_size"]:
        problems.append(f"downloaded zip is {len(zip_bytes)} bytes, pinned {handoff['artifact_size']}")

    consumed: dict[str, str] = {}
    provenance: dict = {}
    if not problems:
        try:
            archive = zipfile.ZipFile(io.BytesIO(zip_bytes))
            members = safe_members(archive)
        except (zipfile.BadZipFile, ValueError) as error:
            problems.append(f"bad zip: {error}")
            members = {}
        if members and set(members) != EXPECTED_MEMBERS:
            problems.append(
                "zip members differ from the expected nine: extra="
                f"{sorted(set(members) - EXPECTED_MEMBERS)} missing={sorted(EXPECTED_MEMBERS - set(members))}"
            )
        if members and set(members) == EXPECTED_MEMBERS:
            data = {name: archive.read(info) for name, info in members.items()}
            try:
                sums = parse_sums(data["v0.7.12/SHA256SUMS"].decode("utf-8"))
            except ValueError as error:
                problems.append(str(error))
                sums = {}
            if sums and sums != handoff["launchers"]:
                problems.append("v0.7.12/SHA256SUMS differs from the five pinned launcher digests")
            for asset in ASSETS:
                actual = sha256_bytes(data[f"v0.7.12/{asset}"])
                consumed[asset] = actual
                if actual != handoff["launchers"][asset]:
                    problems.append(f"{asset} hashes to {actual}, pinned {handoff['launchers'][asset]}")
            try:
                provenance = json.loads(data["legacy-bridge-provenance.json"].decode("utf-8"))
            except ValueError as error:
                problems.append(f"provenance is not JSON: {error}")
            if provenance:
                if provenance.get("schema") != PROVENANCE_SCHEMA:
                    problems.append(f"provenance schema {provenance.get('schema')!r} != {PROVENANCE_SCHEMA!r}")
                if provenance.get("eligible_for_latest") is not False:
                    problems.append("provenance says eligible_for_latest is not false")
            if not problems:
                out.mkdir(parents=True, exist_ok=True)
                for name, payload in data.items():
                    target = out / name
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_bytes(payload)
    if problems:
        raise ValueError("; ".join(problems))
    verified = {
        "schema": "arc.legacy-bridge.wave0-lab.handoff-verified.v1",
        "repository": repository,
        "run_id": handoff["run_id"],
        "artifact_id": handoff["artifact_id"],
        "artifact_digest": handoff["artifact_digest"],
        "artifact_size": handoff["artifact_size"],
        "launchers": consumed,
        "provenance_sha256": sha256_bytes(json.dumps(provenance, sort_keys=True).encode("utf-8")),
    }
    (out / "handoff-verified.json").write_text(json.dumps(verified, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return verified


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    config = json.loads(args.config.read_text(encoding="utf-8"))
    try:
        verified = fetch(config, args.out)
    except ValueError as error:
        print(f"HANDOFF REFUSED: {error}", file=sys.stderr)
        return 1
    print(f"handoff run {verified['run_id']} artifact {verified['artifact_id']} {verified['artifact_digest']}")
    for asset, digest in verified["launchers"].items():
        print(f"  {digest}  {asset}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
