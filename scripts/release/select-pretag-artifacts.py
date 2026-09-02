#!/usr/bin/env python3
"""Select the exact immutable pre-tag artifacts for one successful run.

The GitHub Actions artifact service assigns an immutable ID and SHA-256 digest
to every upload.  Artifact names additionally bind the candidate commit, run,
attempt, and the SHA-256 of the inner tarball.  This selector turns the API
response into the one small JSON value consumed by the tag workflow and fails
closed on missing, duplicate, expired, empty, or ambiguously named artifacts.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path


EXPECTED_GROUPS = (
    ("headless", "linux-x86_64"),
    ("headless", "linux-arm64"),
    ("headless", "macos-arm64"),
    ("headless", "macos-x86_64"),
    ("headless", "windows-x86_64"),
    ("desktop", "linux-x86_64"),
    ("desktop", "macos-arm64"),
    ("desktop", "macos-x86_64"),
    ("desktop", "windows-x86_64"),
)


def fail(message: str) -> "NoReturn":
    raise SystemExit(f"pre-tag artifact selection: {message}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--api-json", required=True, type=Path)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--run-id", required=True, type=int)
    parser.add_argument("--run-attempt", required=True, type=int)
    parser.add_argument("--github-output", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--expected-artifacts-json")
    return parser.parse_args()


def load_artifacts(path: Path) -> list[dict]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot read artifact API response {path}: {error}")
    artifacts = value.get("artifacts") if isinstance(value, dict) else value
    if not isinstance(artifacts, list):
        fail("artifact API response must contain an artifacts array")
    return artifacts


def validate_scalar_inputs(args: argparse.Namespace) -> None:
    if args.repository != "FerrumVir/arc-chain":
        fail(f"unexpected repository {args.repository!r}")
    if re.fullmatch(r"[0-9a-f]{40}", args.commit) is None:
        fail("commit must be one full lowercase Git SHA")
    if args.run_id <= 0 or args.run_attempt <= 0:
        fail("run ID and attempt must be positive integers")


def select(args: argparse.Namespace, artifacts: list[dict]) -> dict:
    validate_scalar_inputs(args)
    selected: dict[str, dict[str, dict]] = {}
    selected_ids: set[int] = set()

    for kind, platform in EXPECTED_GROUPS:
        pattern = re.compile(
            rf"arc-pretag-{re.escape(kind)}-{re.escape(platform)}-"
            rf"{re.escape(args.commit)}-{args.run_id}-{args.run_attempt}-"
            r"([0-9a-f]{64})"
        )
        matches: list[tuple[dict, str]] = []
        for artifact in artifacts:
            if not isinstance(artifact, dict):
                fail("artifact API entry is not an object")
            name = artifact.get("name")
            if not isinstance(name, str):
                fail("artifact API entry has no string name")
            match = pattern.fullmatch(name)
            if match:
                matches.append((artifact, match.group(1)))

        if len(matches) != 1:
            fail(
                f"expected exactly one {kind}/{platform} artifact for run "
                f"{args.run_id} attempt {args.run_attempt}; found {len(matches)}"
            )

        artifact, archive_sha256 = matches[0]
        artifact_id = artifact.get("id")
        size = artifact.get("size_in_bytes")
        digest = artifact.get("digest")
        if not isinstance(artifact_id, int) or artifact_id <= 0:
            fail(f"{kind}/{platform} artifact has an invalid ID")
        if artifact_id in selected_ids:
            fail(f"artifact ID {artifact_id} is reused across candidate groups")
        if artifact.get("expired") is not False:
            fail(f"{kind}/{platform} artifact is expired or has unknown expiry state")
        if not isinstance(size, int) or size <= 0:
            fail(f"{kind}/{platform} artifact is empty or has unknown size")
        if not isinstance(digest, str) or re.fullmatch(r"sha256:[0-9a-f]{64}", digest) is None:
            fail(f"{kind}/{platform} artifact has no exact server SHA-256 digest")
        workflow_run = artifact.get("workflow_run")
        if isinstance(workflow_run, dict) and workflow_run.get("id") not in (None, args.run_id):
            fail(f"{kind}/{platform} artifact belongs to another workflow run")

        selected_ids.add(artifact_id)
        selected.setdefault(platform, {})[kind] = {
            "id": artifact_id,
            "name": artifact["name"],
            "digest": digest,
            "archive_sha256": archive_sha256,
            "size_in_bytes": size,
        }

    current_attempt_marker = f"-{args.commit}-{args.run_id}-{args.run_attempt}-"
    expected_names = {
        value["name"]
        for platform in selected.values()
        for value in platform.values()
    }
    unexpected = sorted(
        artifact.get("name", "")
        for artifact in artifacts
        if isinstance(artifact, dict)
        and isinstance(artifact.get("name"), str)
        and artifact["name"].startswith("arc-pretag-")
        and current_attempt_marker in artifact["name"]
        and artifact["name"] not in expected_names
    )
    if unexpected:
        fail(f"unexpected current-attempt pre-tag artifacts: {', '.join(unexpected)}")

    return {
        "schema": "arc.pretag.selection.v1",
        "repository": args.repository,
        "commit": args.commit,
        "run_id": args.run_id,
        "run_attempt": args.run_attempt,
        "artifacts": selected,
    }


def main() -> None:
    args = parse_args()
    result = select(args, load_artifacts(args.api_json))
    if args.expected_artifacts_json is not None:
        try:
            expected_artifacts = json.loads(args.expected_artifacts_json)
        except json.JSONDecodeError as error:
            fail(f"expected artifact selection JSON is invalid: {error}")
        if expected_artifacts != result["artifacts"]:
            fail(
                "live run artifact IDs, names, digests, sizes, or archive hashes "
                "differ from the validated selection"
            )
    compact = json.dumps(result["artifacts"], sort_keys=True, separators=(",", ":"))
    artifact_ids = ",".join(
        str(result["artifacts"][platform][kind]["id"])
        for kind, platform in EXPECTED_GROUPS
    )
    rendered = json.dumps(result, sort_keys=True, indent=2) + "\n"

    if args.output:
        args.output.write_text(rendered, encoding="utf-8")
    if args.github_output:
        with args.github_output.open("a", encoding="utf-8") as handle:
            handle.write(f"pretag_artifacts={compact}\n")
            handle.write(f"pretag_artifact_ids={artifact_ids}\n")
            handle.write(f"pretag_run_id={args.run_id}\n")
            handle.write(f"pretag_run_attempt={args.run_attempt}\n")
    sys.stdout.write(rendered)


if __name__ == "__main__":
    main()
