#!/usr/bin/env python3
"""Generate or check crates/arc-legacy-bridge/pins/active.json.

The bridge installs exactly one v0.8 release. This tool derives every pin
from that release's public GitHub metadata and its owner-signed SHA256SUMS,
never from memory:

1. The release must be published, non-draft, non-prerelease, immutable, and
   authored by github-actions[bot] (what the v0.8 installers require).
2. SHA256SUMS.sig must verify with `ssh-keygen -Y verify` for principal
   `arc-release`, namespace `arc-release-manifest-v1`, and the release key
   that install.sh embeds; the manifest header must name the repository,
   the exact tag, and the tag's commit.
3. Each pinned asset's manifest digest must equal GitHub's own asset digest.
4. `worker_names_privacy_safe` is true only if the tag's arc-node no longer
   registers workers under "name (hostname)" (PR #134).
5. The community RPC origins come from the tag's install.sh, and the model
   pin from the tag's arc-node TESTNET_MODEL_SHA256.

Usage:
  pin-release.py --tag v0.8.10 --write   # regenerate the pin file
  pin-release.py --check                 # recompute and compare (CI)
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path

REPOSITORY = "FerrumVir/arc-chain"
REPO_ROOT = Path(__file__).resolve().parents[2]
PINS = REPO_ROOT / "crates" / "arc-legacy-bridge" / "pins" / "active.json"
SIGNER = "arc-release"
NAMESPACE = "arc-release-manifest-v1"
PLATFORM_ASSETS = [
    "arc-cli-linux-arm64",
    "arc-cli-linux-x86_64",
    "arc-cli-macos-arm64",
    "arc-cli-macos-x86_64",
    "arc-cli-windows-x86_64.exe",
    "arc-node-linux-arm64",
    "arc-node-linux-x86_64",
    "arc-node-macos-arm64",
    "arc-node-macos-x86_64",
    "arc-node-windows-x86_64.exe",
    "genesis.toml",
    "testnet-seeds.txt",
]
HOSTNAME_REGISTRATION = 'format!("{} ({})", public_node_name_c, hostname_c)'
MODEL_SIZE = 4_081_004_224  # docs/HEADLESS_INSTALL.md, exact supported artifact size


def fetch(url: str) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": "arc-legacy-bridge-pin-release"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return response.read()


def git_show(tag: str, path: str) -> str:
    return subprocess.run(
        ["git", "-C", str(REPO_ROOT), "show", f"{tag}:{path}"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout


def release_key() -> str:
    install = (REPO_ROOT / "install.sh").read_text(encoding="utf-8")
    match = re.search(
        r"'arc-release namespaces=\"arc-release-manifest-v1\" (ssh-ed25519 [A-Za-z0-9+/=]+) arc-release-manifest-v1'",
        install,
    )
    if not match:
        raise SystemExit("install.sh no longer embeds the release-manifest key")
    return match.group(1)


def verify_manifest(manifest: bytes, signature: bytes, key: str) -> None:
    with tempfile.TemporaryDirectory() as scratch:
        root = Path(scratch)
        (root / "allowed").write_text(f'{SIGNER} namespaces="{NAMESPACE}" {key} {NAMESPACE}\n', encoding="utf-8")
        (root / "SHA256SUMS.sig").write_bytes(signature)
        result = subprocess.run(
            [
                "ssh-keygen", "-Y", "verify", "-f", str(root / "allowed"), "-I", SIGNER,
                "-n", NAMESPACE, "-s", str(root / "SHA256SUMS.sig"),
            ],
            input=manifest,
            capture_output=True,
        )
        if result.returncode != 0:
            raise SystemExit(f"SHA256SUMS.sig does not verify: {result.stderr.decode(errors='replace')}")


def community_origins(tag: str) -> list[str]:
    install = git_show(tag, "install.sh")
    block = re.search(r"COMMUNITY_RPC_ORIGINS=\(\n(.*?)\n\)", install, re.S)
    if not block:
        raise SystemExit(f"{tag}: install.sh has no COMMUNITY_RPC_ORIGINS block")
    origins = [line.strip() for line in block.group(1).splitlines() if line.strip()]
    if not origins or not all(origin.startswith("https://") for origin in origins):
        raise SystemExit(f"{tag}: unexpected community origins {origins}")
    return origins


def model_pin(tag: str, previous: dict | None) -> dict:
    main = git_show(tag, "crates/arc-node/src/main.rs")
    sha = re.search(r'const TESTNET_MODEL_SHA256: &str =\s*"([0-9a-f]{64})"', main)
    url = re.search(r'const DEFAULT_MODEL_SOURCES: &\[&str\] = &\[\s*"(https://[^"]+)"', main)
    if not sha or not url:
        raise SystemExit(f"{tag}: arc-node no longer declares its pinned model")
    pin = {
        "file_name": url.group(1).rsplit("/", 1)[1],
        "sha256": sha.group(1),
        "size": MODEL_SIZE,
        "url": url.group(1),
    }
    if previous and previous.get("sha256") == pin["sha256"]:
        pin["size"] = previous["size"]
    return pin


def build_pins(tag: str, bridge_version: str, previous: dict | None) -> dict:
    release = json.loads(fetch(f"https://api.github.com/repos/{REPOSITORY}/releases/tags/{tag}"))
    problems = []
    if release.get("draft") or release.get("prerelease"):
        problems.append("draft or prerelease")
    if release.get("immutable") is not True:
        problems.append("not immutable")
    if (release.get("author") or {}).get("login") != "github-actions[bot]":
        problems.append("not authored by github-actions[bot]")
    if problems:
        raise SystemExit(f"{tag} cannot be pinned: {', '.join(problems)}")
    assets = {asset["name"]: asset for asset in release["assets"]}
    base = f"https://github.com/{REPOSITORY}/releases/download/{tag}"
    manifest = fetch(f"{base}/SHA256SUMS")
    signature = fetch(f"{base}/SHA256SUMS.sig")
    verify_manifest(manifest, signature, release_key())
    lines = manifest.decode("utf-8").splitlines()
    commit = subprocess.run(
        ["git", "-C", str(REPO_ROOT), "rev-list", "-n", "1", tag], capture_output=True, text=True, check=True
    ).stdout.strip()
    expected_header = ["# ARC release manifest v1", f"# repository={REPOSITORY}", f"# tag={tag}", f"# commit={commit}"]
    if lines[:4] != expected_header:
        raise SystemExit(f"{tag}: manifest header {lines[:4]} != {expected_header}")
    digests = dict(reversed(line.split("  ", 1)) for line in lines[4:])
    pinned_assets = {}
    for name in PLATFORM_ASSETS:
        asset = assets.get(name)
        if asset is None or name not in digests:
            raise SystemExit(f"{tag}: {name} is missing from the release or its manifest")
        if asset.get("digest") != f"sha256:{digests[name]}":
            raise SystemExit(f"{tag}: GitHub's digest for {name} differs from the signed manifest")
        pinned_assets[name] = {"sha256": digests[name], "size": asset["size"]}
    privacy_safe = HOSTNAME_REGISTRATION not in git_show(tag, "crates/arc-node/src/main.rs")
    return {
        "schema": "arc.legacy-bridge.pins.v1",
        "bridge_version": bridge_version,
        "repository": REPOSITORY,
        "node_release": {
            "tag": tag,
            "version": tag.removeprefix("v"),
            "commit": commit,
            "manifest": {"name": "SHA256SUMS", "sha256": hashlib.sha256(manifest).hexdigest(), "size": len(manifest)},
            "manifest_signature": {
                "name": "SHA256SUMS.sig",
                "sha256": hashlib.sha256(signature).hexdigest(),
                "size": len(signature),
            },
            "worker_names_privacy_safe": privacy_safe,
            "assets": pinned_assets,
        },
        "manifest_signer": {"principal": SIGNER, "namespace": NAMESPACE, "public_key": release_key()},
        "community_rpc_origins": community_origins(tag),
        "model": model_pin(tag, (previous or {}).get("model")),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--tag", help="exact v0.8 release tag (default: the tag in the current pins)")
    parser.add_argument("--bridge-version", help="bridge release version (default: keep the current one)")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    args = parser.parse_args()
    previous = json.loads(PINS.read_text(encoding="utf-8"))
    tag = args.tag or previous["node_release"]["tag"]
    subprocess.run(
        ["git", "-C", str(REPO_ROOT), "fetch", "--no-tags", "--depth=1", "origin", f"refs/tags/{tag}:refs/tags/{tag}"],
        check=False,
        capture_output=True,
    )
    pins = build_pins(tag, args.bridge_version or previous["bridge_version"], previous)
    rendered = json.dumps(pins, indent=2) + "\n"
    if args.write:
        PINS.write_text(rendered, encoding="utf-8")
        print(f"wrote {PINS.relative_to(REPO_ROOT)} for {tag} (privacy-safe worker names: {pins['node_release']['worker_names_privacy_safe']})")
        return 0
    if json.loads(rendered) != previous:
        print("the committed pins differ from what the live release yields:", file=sys.stderr)
        for key in sorted(set(pins) | set(previous)):
            if pins.get(key) != previous.get(key):
                print(f"  {key}: committed {json.dumps(previous.get(key))[:200]} != derived {json.dumps(pins.get(key))[:200]}", file=sys.stderr)
        return 1
    print(f"committed pins match {tag}: signed manifest, GitHub digests, origins, model, privacy flag")
    return 0


if __name__ == "__main__":
    sys.exit(main())
