#!/usr/bin/env python3
"""Authenticate the published validator baseline for an explicit desktop update.

Only GitHub release reads occur here; this helper never contacts validators.
The source-pinned manifest key is the same one used by install.sh.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tempfile

PROFILE = "desktop-update-existing-validators-v1"
ASSET = "arc-desktop-update-attestation.json"
REPOSITORY = "FerrumVir/arc-chain"
TAG = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")
PUBLIC_KEY = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPs2NAiDRXit9EM96A2GdXZgRqvXtl0lvryEAEAEjQfY"
NAMESPACE = "arc-release-manifest-v1"
MAX_MANIFEST_BYTES = 1024 * 1024


def need(ok, message):
    if not ok:
        raise ValueError(message)


def version(tag):
    match = TAG.fullmatch(str(tag))
    need(match is not None, "invalid release tag")
    return tuple(map(int, match.groups()))


def select_release(releases, tag, candidate):
    need(isinstance(releases, list), "release listing must be an array")
    version(tag)
    need(version(tag) < version(candidate), "validator release must precede the candidate")
    qualifying = [r for r in releases if isinstance(r, dict)
                  and TAG.fullmatch(str(r.get("tag_name", "")))
                  and r.get("name") == "ARC Chain " + r["tag_name"]
                  and r.get("draft") is False and r.get("prerelease") is False
                  and r.get("immutable") is True
                  and isinstance(r.get("author"), dict)
                  and r["author"].get("login") == "github-actions[bot]"]
    need(bool(qualifying), "no qualifying immutable bot-authored release")
    newest = max(version(r["tag_name"]) for r in qualifying)
    matches = [r for r in qualifying if version(r["tag_name"]) == newest]
    need(len(matches) == 1 and matches[0]["tag_name"] == tag,
         "named validator release is not the newest qualifying release")
    release = matches[0]
    need(type(release.get("id")) is int and release["id"] > 0,
         "invalid published release ID")
    need(re.fullmatch(r"[0-9a-f]{40}", str(release.get("target_commitish", ""))),
         "published release must bind an exact commit")
    return release


def gh(*args):
    result = subprocess.run(["gh", "api", *args], capture_output=True, timeout=120)
    need(result.returncode == 0, "GitHub release read failed")
    return result.stdout


def published_assets(release):
    rows = release.get("assets")
    need(isinstance(rows, list) and all(isinstance(a, dict) for a in rows), "invalid release assets")
    assets = {a.get("name"): a for a in rows}
    need(len(assets) == len(rows), "duplicate release asset names")
    for name in ("SHA256SUMS", "SHA256SUMS.sig", "arc-node-linux-x86_64"):
        a = assets.get(name, {})
        need(type(a.get("id")) is int and a["id"] > 0 and a.get("state") == "uploaded"
             and type(a.get("size")) is int and a["size"] > 0
             and isinstance(a.get("uploader"), dict)
             and a["uploader"].get("login") == "github-actions[bot]"
             and re.fullmatch(r"sha256:[0-9a-f]{64}", str(a.get("digest", "")))
             and a.get("browser_download_url") ==
                 f"https://github.com/{REPOSITORY}/releases/download/{release['tag_name']}/{name}",
             "invalid published asset: " + name)
    return assets


def verify_manifest(release, manifest, signature):
    assets = published_assets(release)
    for name, payload in (("SHA256SUMS", manifest), ("SHA256SUMS.sig", signature)):
        need(0 < len(payload) <= MAX_MANIFEST_BYTES and len(payload) == assets[name]["size"]
             and "sha256:" + hashlib.sha256(payload).hexdigest() == assets[name]["digest"],
             "published manifest asset size or digest differs: " + name)
    with tempfile.TemporaryDirectory(prefix="arc-previous-manifest-") as directory:
        root = Path(directory)
        (root / "allowed-signers").write_text(f'arc-release namespaces="{NAMESPACE}" {PUBLIC_KEY}\n')
        (root / "SHA256SUMS.sig").write_bytes(signature)
        verified = subprocess.run(["ssh-keygen", "-Y", "verify", "-f", str(root / "allowed-signers"),
                                   "-I", "arc-release", "-n", NAMESPACE,
                                   "-s", str(root / "SHA256SUMS.sig")], input=manifest, capture_output=True)
        need(verified.returncode == 0, "previous release SHA256SUMS signature verification failed")
    lines = manifest.decode("utf-8").splitlines()
    need(lines[:4] == ["# ARC release manifest v1", f"# repository={REPOSITORY}",
                      f"# tag={release['tag_name']}", f"# commit={release['target_commitish']}"],
         "previous manifest identity differs from published release")
    hashes = {}
    for line in lines[4:]:
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9_.-]+)", line)
        need(match is not None and match[2] not in hashes, "invalid or duplicate manifest record")
        hashes[match[2]] = match[1]
    binary_hash = hashes.get("arc-node-linux-x86_64")
    need(binary_hash and "sha256:" + binary_hash == assets["arc-node-linux-x86_64"]["digest"],
         "previous Linux node hash is missing or differs from published asset")
    return {"tag": release["tag_name"], "release_id": release["id"],
            "commit": release["target_commitish"], "binary_sha256": binary_hash,
            "manifest_sha256": hashlib.sha256(manifest).hexdigest(),
            "manifest_signature_sha256": hashlib.sha256(signature).hexdigest(),
            "manifest_namespace": NAMESPACE}


def authenticate(tag, candidate):
    # Paginate the whole listing. Never trust /releases/latest (legacy channel).
    pages = json.loads(gh("--paginate", "--slurp", f"repos/{REPOSITORY}/releases?per_page=100"))
    need(isinstance(pages, list) and all(isinstance(page, list) for page in pages), "invalid release pages")
    release = select_release([r for page in pages for r in page], tag, candidate)
    assets = published_assets(release)
    payloads = []
    for name in ("SHA256SUMS", "SHA256SUMS.sig"):
        need(assets[name]["size"] <= MAX_MANIFEST_BYTES, "published manifest asset exceeds size bound")
        payloads.append(gh("-H", "Accept: application/octet-stream",
                           f"repos/{REPOSITORY}/releases/assets/{assets[name]['id']}"))
    return verify_manifest(release, *payloads)


def verify_preflight_binding(value, run_id, attempt, selection):
    release = value.get("release", {})
    need(type(run_id) is int and run_id > 0 and type(attempt) is int and attempt > 0
         and release.get("preflight_run_id") == run_id
         and release.get("preflight_run_attempt") == attempt,
         "desktop attestation preflight run/attempt differs from release dispatch")
    for role in ("production_artifact", "checkpoint_verifier_artifact"):
        artifact = release.get(role, {})
        selected = selection.get(artifact.get("platform"), {}).get("headless", {})
        need(type(selected.get("id")) is int and selected["id"] > 0
             and artifact.get("artifact_id") == selected["id"]
             and artifact.get("artifact_digest") == selected.get("digest"),
             "desktop attestation artifact ID/digest differs from release preflight selection")


def verify_attestation(value, candidate, now):
    need(value.get("profile") == PROFILE and value.get("node_compatible_with_validators") is True,
         "desktop update requires explicit compatibility declaration")
    prior = value.get("validators_release")
    need(isinstance(prior, dict), "validator release attestation missing")
    need(authenticate(prior.get("tag"), candidate) == prior, "validator release evidence differs")
    observed = value.get("generated_at_unix")
    need(type(observed) in (int, float) and 0 <= now - observed <= 7200,
         "desktop update observation is stale or future-dated")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--candidate", required=True)
    args = parser.parse_args()
    print(json.dumps(authenticate(args.tag, args.candidate), sort_keys=True))
