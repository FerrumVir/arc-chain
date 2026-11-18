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
MAX_ATTESTATION_BYTES = 1024 * 1024
SKIPPED = "validators_release_skipped_desktop_updates"
NOT_NEWEST = "named validator release is not the newest qualifying release"


def need(ok, message):
    if not ok:
        raise ValueError(message)


def version(tag):
    match = TAG.fullmatch(str(tag))
    need(match is not None, "invalid release tag")
    return tuple(map(int, match.groups()))


def release_identity(release):
    need(type(release.get("id")) is int and release["id"] > 0,
         "invalid published release ID")
    need(re.fullmatch(r"[0-9a-f]{40}", str(release.get("target_commitish", ""))),
         "published release must bind an exact commit")


def select_release(releases, tag, candidate, verify_skipped=None):
    """Select a baseline; older baselines require authenticated skip evidence.

    verify_skipped(baseline, release) must raise unless the intervening release
    is an authenticated desktop update over baseline. With no verifier, retain
    the original newest-only selection contract.
    """
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
    matches = [r for r in qualifying if r["tag_name"] == tag]
    need(len(matches) == 1, NOT_NEWEST)
    release = matches[0]
    release_identity(release)
    newer = sorted((r for r in qualifying if version(r["tag_name"]) > version(tag)),
                   key=lambda r: version(r["tag_name"]))
    # Preserve the refusal of an already published candidate or later release.
    need(all(version(r["tag_name"]) < version(candidate) for r in newer), NOT_NEWEST)
    need(len({r["tag_name"] for r in newer}) == len(newer), NOT_NEWEST)
    for skipped in newer:
        need(verify_skipped is not None, NOT_NEWEST)
        try:
            release_identity(skipped)
            verify_skipped(release, skipped)
        except (ValueError, TypeError, RecursionError, OSError, subprocess.SubprocessError) as exc:
            raise ValueError(NOT_NEWEST) from exc
    return release


def gh(*args):
    result = subprocess.run(["gh", "api", *args], capture_output=True, timeout=120)
    need(result.returncode == 0, "GitHub release read failed")
    return result.stdout


def published_assets(release, desktop=False):
    rows = release.get("assets")
    need(isinstance(rows, list) and all(isinstance(a, dict) for a in rows), "invalid release assets")
    assets = {a.get("name"): a for a in rows}
    need(len(assets) == len(rows), "duplicate release asset names")
    names = ("SHA256SUMS", "SHA256SUMS.sig", "arc-node-linux-x86_64")
    for name in names + ((ASSET,) if desktop else ()):
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


def verify_manifest(release, manifest, signature, attestation=None):
    assets = published_assets(release, desktop=attestation is not None)
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
    evidence = {"tag": release["tag_name"], "release_id": release["id"],
            "commit": release["target_commitish"], "binary_sha256": binary_hash,
            "manifest_sha256": hashlib.sha256(manifest).hexdigest(),
            "manifest_signature_sha256": hashlib.sha256(signature).hexdigest(),
            "manifest_namespace": NAMESPACE}
    if attestation is not None:
        digest = hashlib.sha256(attestation).hexdigest()
        need(0 < len(attestation) <= MAX_ATTESTATION_BYTES
             and len(attestation) == assets[ASSET]["size"]
             and "sha256:" + digest == assets[ASSET]["digest"],
             "published attestation asset size or digest differs")
        need(hashes.get(ASSET) == digest, "attestation hash missing or differs in SHA256SUMS")
        evidence["attestation_sha256"] = digest
    return evidence


def download_evidence(release, desktop=False):
    assets = published_assets(release, desktop=desktop)
    limits = [("SHA256SUMS", MAX_MANIFEST_BYTES), ("SHA256SUMS.sig", MAX_MANIFEST_BYTES)]
    if desktop:
        limits.append((ASSET, MAX_ATTESTATION_BYTES))
    # Validate every advertised bound before downloading any of the assets.
    for name, limit in limits:
        need(assets[name]["size"] <= limit, "published asset exceeds size bound: " + name)
    payloads = [gh("-H", "Accept: application/octet-stream",
                   f"repos/{REPOSITORY}/releases/assets/{assets[name]['id']}") for name, _ in limits]
    return verify_manifest(release, *payloads), payloads


def read_desktop_attestation(payload):
    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            need(key not in result, "duplicate desktop attestation key")
            result[key] = value
        return result

    return json.loads(payload, object_pairs_hook=unique_object)


def authenticate(tag, candidate):
    # Paginate the whole listing. Never trust /releases/latest (legacy channel).
    pages = json.loads(gh("--paginate", "--slurp", f"repos/{REPOSITORY}/releases?per_page=100"))
    need(isinstance(pages, list) and all(isinstance(page, list) for page in pages), "invalid release pages")
    prior = None
    skipped = []

    def verify_skipped(baseline, release):
        nonlocal prior
        if prior is None:
            prior, _ = download_evidence(baseline)
        evidence, payloads = download_evidence(release, desktop=True)
        value = read_desktop_attestation(payloads[-1])
        need(isinstance(value, dict) and value.get("profile") == PROFILE
             and value.get("node_compatible_with_validators") is True,
             "intervening release is not a compatible desktop update")
        validators = value.get("validators_release")
        need(isinstance(validators, dict) and validators.get("tag") == prior["tag"]
             and validators.get("binary_sha256") == prior["binary_sha256"],
             "intervening desktop update names another validator baseline")
        skipped.append({key: evidence[key] for key in
                        ("tag", "release_id", "commit", "attestation_sha256")})

    release = select_release([r for page in pages for r in page], tag, candidate, verify_skipped)
    if prior is None:
        prior, _ = download_evidence(release)
    return {**prior, SKIPPED: skipped}


def attestation_evidence(tag, candidate):
    """Split authenticated baseline and skips for the public attestation schema."""
    prior = dict(authenticate(tag, candidate))
    skipped = prior.pop(SKIPPED, [])
    return prior, skipped


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
    authenticated, skipped = attestation_evidence(prior.get("tag"), candidate)
    need(authenticated == prior, "validator release evidence differs")
    # Legacy newest-only attestations omitted this field. Absence can only mean
    # an empty list; it can never hide an intervening release.
    recorded = value.get(SKIPPED, [])
    need(isinstance(recorded, list)
         and all(isinstance(row, dict) and type(row.get("release_id")) is int for row in recorded)
         and recorded == skipped, "skipped desktop update evidence differs")
    observed = value.get("generated_at_unix")
    need(type(observed) in (int, float) and 0 <= now - observed <= 7200,
         "desktop update observation is stale or future-dated")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--candidate", required=True)
    args = parser.parse_args()
    print(json.dumps(authenticate(args.tag, args.candidate), sort_keys=True))
