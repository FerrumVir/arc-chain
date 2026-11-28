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
5. The community RPC origins and shard worker pins come from the tag's
   install.sh, and the model pin from the tag's arc-node TESTNET_MODEL_SHA256.

Usage:
  pin-release.py --tag v0.8.13 --write   # regenerate the pin file
  pin-release.py --check                 # recompute and compare (CI)
"""

from __future__ import annotations

import argparse
import hashlib
import ipaddress
import json
import os
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
MODEL_SIZE = 4_081_004_224
SHARD_REQUIRED_ADMITTED_LAYERS_FLOOR = 6


def fetch(url: str) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": "arc-legacy-bridge-pin-release"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return response.read()


class RejectMetadataRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def release_metadata(tag: str) -> dict:
    headers = {"User-Agent": "arc-legacy-bridge-pin-release", "Accept": "application/vnd.github+json"}
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(
        f"https://api.github.com/repos/{REPOSITORY}/releases/tags/{tag}",
        headers=headers,
    )
    opener = urllib.request.build_opener(RejectMetadataRedirects())
    with opener.open(request, timeout=60) as response:
        payload = response.read(4 * 1024 * 1024 + 1)
    if len(payload) > 4 * 1024 * 1024:
        raise ValueError("Release metadata exceeds 4 MiB; inspect the release assets before pinning")
    return json.loads(payload)


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


def is_valid_port(port: str) -> bool:
    return (
        bool(port)
        and not port.startswith("0")
        and port.isascii()
        and port.isdigit()
        and 1 <= int(port) <= 65535
    )


def is_valid_host(host: str) -> bool:
    if not host or host.startswith(".") or host.endswith(".") or ".." in host or not host.isascii():
        return False
    if all(ch.isdigit() or ch == "." for ch in host):
        try:
            ipaddress.IPv4Address(host)
            return True
        except ValueError:
            return False
    for label in host.split("."):
        if not label or len(label) > 63 or label.startswith("-") or label.endswith("-"):
            return False
        if not all(ch.isalnum() or ch == "-" for ch in label):
            return False
    return True


def is_valid_https_origin(origin: str) -> bool:
    if not origin.startswith("https://"):
        return False
    authority = origin[len("https://"):]
    if not authority or any(ord(ch) < 33 or ord(ch) == 127 or ch in "/?#@\\" for ch in authority):
        return False
    if authority.startswith("["):
        if "]" not in authority:
            return False
        address, after = authority[1:].split("]", 1)
        try:
            ipaddress.IPv6Address(address)
        except ValueError:
            return False
        if not after:
            return True
        if not after.startswith(":"):
            return False
        return is_valid_port(after[1:])
    if ":" in authority:
        host, port = authority.split(":", 1)
        if ":" in port:
            return False
        return is_valid_host(host) and is_valid_port(port)
    return is_valid_host(authority)


def is_nonzero_lower_hex(value: str, length: int) -> bool:
    return (
        len(value) == length
        and all(ch in "0123456789abcdef" for ch in value)
        and any(ch != "0" for ch in value)
    )


def extract_shell_assignment(install_text: str, name: str, tag: str) -> str:
    matches = re.findall(rf"^{re.escape(name)}=([^\r\n]+)$", install_text, re.MULTILINE)
    if len(matches) != 1:
        raise ValueError(f"{tag}: install.sh must define {name} exactly once (found {len(matches)})")
    value = matches[0]
    if len(value) >= 2 and value[0] == value[-1] and value[0] in ("'", '"'):
        value = value[1:-1]
    return value


def parse_shard_worker_pin(install_text: str, tag: str) -> dict:
    coordinator_url = extract_shell_assignment(install_text, "SHARD_COORDINATOR_URL", tag)
    coordinator_id = extract_shell_assignment(install_text, "SHARD_COORDINATOR_ID", tag)
    transaction_domain = extract_shell_assignment(install_text, "SHARD_TRANSACTION_DOMAIN", tag)
    expected_model_id = extract_shell_assignment(install_text, "SHARD_EXPECTED_MODEL_ID", tag)
    raw_layers = extract_shell_assignment(install_text, "SHARD_REQUIRED_ADMITTED_LAYERS", tag)

    if not is_valid_https_origin(coordinator_url):
        raise ValueError(f"{tag}: invalid SHARD_COORDINATOR_URL {coordinator_url!r}")
    for field_name, digest in (
        ("SHARD_COORDINATOR_ID", coordinator_id),
        ("SHARD_TRANSACTION_DOMAIN", transaction_domain),
        ("SHARD_EXPECTED_MODEL_ID", expected_model_id),
    ):
        if not is_nonzero_lower_hex(digest, 64):
            raise ValueError(f"{tag}: invalid {field_name} {digest!r}")
    if (
        not raw_layers.isascii()
        or not raw_layers.isdigit()
        or (len(raw_layers) > 1 and raw_layers.startswith("0"))
    ):
        raise ValueError(f"{tag}: invalid SHARD_REQUIRED_ADMITTED_LAYERS {raw_layers!r}")
    required_layers = int(raw_layers)
    if required_layers < SHARD_REQUIRED_ADMITTED_LAYERS_FLOOR:
        raise ValueError(
            f"{tag}: SHARD_REQUIRED_ADMITTED_LAYERS={required_layers} is below floor {SHARD_REQUIRED_ADMITTED_LAYERS_FLOOR}"
        )
    return {
        "coordinator_url": coordinator_url,
        "coordinator_id": coordinator_id,
        "transaction_domain": transaction_domain,
        "expected_model_id": expected_model_id,
        "required_admitted_layers": required_layers,
    }


def shard_worker_pin(tag: str) -> dict:
    try:
        return parse_shard_worker_pin(git_show(tag, "install.sh"), tag)
    except ValueError as error:
        raise SystemExit(str(error)) from error


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
    release = release_metadata(tag)
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
        "shard_worker": shard_worker_pin(tag),
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
    print(f"committed pins match {tag}: signed manifest, GitHub digests, origins, model, shard worker, privacy flag")
    return 0


if __name__ == "__main__":
    sys.exit(main())
