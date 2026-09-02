#!/usr/bin/env python3
"""Derive and create the compact, exact-parent ARC recovery handoff commit."""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import NoReturn, Sequence


SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent
ASSEMBLER = SCRIPT_DIR / "assemble-cutover-assets.py"
DERIVED_VALIDATOR = SCRIPT_DIR / "validate-cutover-derived-assets.py"
PUBLIC_FILES = (
    "arc-cutover-policy.json",
    "arc-legacy-maintenance-boundary.json",
    "arc-recovery-checkpoint-descriptor.json",
)
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
REMOTE_RE = re.compile(r"^[A-Za-z0-9._-]+$")
ZERO_COMMIT = "0" * 40


class HandoffCommitError(RuntimeError):
    pass


def fail(message: str) -> NoReturn:
    raise HandoffCommitError(message)


def run_git(
    repository: Path,
    arguments: Sequence[str],
    *,
    environment: dict[str, str] | None = None,
    input_text: str | None = None,
) -> str:
    env = {
        "HOME": os.environ.get("HOME", "/var/empty"),
        "PATH": "/usr/bin:/bin:/usr/local/bin",
        "LANG": "C",
        "LC_ALL": "C",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_TERMINAL_PROMPT": "0",
    }
    if environment:
        env.update(environment)
    try:
        completed = subprocess.run(
            ["git", "-C", os.fspath(repository), *arguments],
            input=input_text,
            text=True,
            stdin=None if input_text is not None else subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            timeout=120,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        fail(f"git {' '.join(arguments[:2])} failed: {error}")
    if completed.returncode != 0:
        diagnostic = completed.stderr.strip()[:2000]
        fail(f"git {' '.join(arguments[:2])} rejected the handoff: {diagnostic}")
    return completed.stdout.strip()


def create_commit(
    repository: Path,
    assets_dir: Path,
    main_commit: str,
    ref_name: str,
    timestamp: str,
) -> tuple[str, str]:
    expected_ref = f"refs/arc-recovery-handoffs/{main_commit}"
    if ref_name != expected_ref:
        fail(f"handoff ref must be exactly {expected_ref}")
    resolved_main = run_git(repository, ["rev-parse", f"{main_commit}^{{commit}}"])
    if resolved_main != main_commit:
        fail("main commit does not resolve exactly")
    if run_git(repository, ["rev-parse", "--is-bare-repository"]) not in {"true", "false"}:
        fail("repository shape is unsupported")
    existing = subprocess.run(
        ["git", "-C", os.fspath(repository), "show-ref", "--verify", "--quiet", ref_name],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env={
            "HOME": os.environ.get("HOME", "/var/empty"),
            "PATH": "/usr/bin:/bin:/usr/local/bin",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": "/dev/null",
        },
        check=False,
    )
    if existing.returncode == 0:
        fail("create-only handoff ref already exists")
    if existing.returncode not in {1}:
        fail("cannot prove the handoff ref is absent")

    with tempfile.TemporaryDirectory(prefix="arc-cutover-index-") as temporary:
        index = Path(temporary) / "index"
        index_env = {"GIT_INDEX_FILE": os.fspath(index)}
        run_git(repository, ["read-tree", "--empty"], environment=index_env)
        for filename in PUBLIC_FILES:
            path = assets_dir / filename
            if not path.is_file() or path.is_symlink() or path.stat().st_size <= 0:
                fail(f"derived handoff asset is unavailable: {filename}")
            blob = run_git(
                repository,
                ["hash-object", "-w", "--no-filters", os.fspath(path)],
            )
            if not re.fullmatch(r"[0-9a-f]{40,64}", blob):
                fail(f"git returned an invalid blob ID for {filename}")
            run_git(
                repository,
                ["update-index", "--add", "--cacheinfo", f"100644,{blob},{filename}"],
                environment=index_env,
            )
        tree = run_git(repository, ["write-tree"], environment=index_env)
        identity_env = {
            "GIT_AUTHOR_NAME": "ARC Recovery Handoff",
            "GIT_AUTHOR_EMAIL": "recovery-handoff@arc.compute",
            "GIT_COMMITTER_NAME": "ARC Recovery Handoff",
            "GIT_COMMITTER_EMAIL": "recovery-handoff@arc.compute",
            "GIT_AUTHOR_DATE": timestamp,
            "GIT_COMMITTER_DATE": timestamp,
        }
        message = (
            "ARC compact recovery handoff\n\n"
            f"release-main: {main_commit}\n"
            "schema: arc-recovery-checkpoint-descriptor/v1\n"
        )
        commit = run_git(
            repository,
            ["-c", "commit.gpgsign=false", "commit-tree", tree, "-p", main_commit],
            environment=identity_env,
            input_text=message,
        )
    if COMMIT_RE.fullmatch(commit) is None:
        fail("git returned an invalid handoff commit ID")
    parent_line = run_git(repository, ["rev-list", "--parents", "-n", "1", commit])
    if parent_line != f"{commit} {main_commit}":
        fail("derived handoff commit does not have the sole exact-main parent")
    listing = run_git(repository, ["ls-tree", "-r", "--name-only", commit]).splitlines()
    if listing != list(PUBLIC_FILES):
        fail("derived handoff commit tree differs from the exact three assets")
    run_git(repository, ["update-ref", ref_name, commit, ZERO_COMMIT])
    return commit, tree


def run_checked(arguments: Sequence[str], label: str) -> None:
    try:
        completed = subprocess.run(
            list(arguments),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=600,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        fail(f"{label} failed: {error}")
    if completed.returncode != 0:
        diagnostic = (completed.stderr or completed.stdout).strip()[:4000]
        fail(f"{label} rejected the recovery handoff: {diagnostic}")


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--full-handoff-dir", required=True, type=Path)
    parser.add_argument("--verifier-binary", required=True, type=Path)
    parser.add_argument("--inspector-binary", required=True, type=Path)
    parser.add_argument("--genesis", required=True, type=Path)
    parser.add_argument("--main-commit", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--repository", default="FerrumVir/arc-chain")
    parser.add_argument("--push-remote")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        if COMMIT_RE.fullmatch(args.main_commit) is None:
            fail("main commit must be one full lowercase Git SHA")
        if args.repository != "FerrumVir/arc-chain":
            fail("handoff commits are restricted to FerrumVir/arc-chain")
        repository = args.repository_root.resolve()
        if not repository.is_dir() or not (repository / ".git").exists():
            fail("repository root is unavailable")
        ref_name = f"refs/arc-recovery-handoffs/{args.main_commit}"
        with contextlib.ExitStack() as stack:
            temporary = stack.enter_context(
                tempfile.TemporaryDirectory(prefix="arc-cutover-assets-")
            )
            assets = Path(temporary)
            run_checked(
                [
                    sys.executable,
                    os.fspath(ASSEMBLER),
                    "--handoff-dir",
                    os.fspath(args.full_handoff_dir),
                    "--output-dir",
                    os.fspath(assets),
                    "--verifier-binary",
                    os.fspath(args.verifier_binary),
                    "--inspector-binary",
                    os.fspath(args.inspector_binary),
                    "--genesis",
                    os.fspath(args.genesis),
                    "--repository",
                    args.repository,
                    "--tag",
                    args.tag,
                    "--commit",
                    args.main_commit,
                ],
                "full ARCCHKPT derivation",
            )
            run_checked(
                [
                    sys.executable,
                    os.fspath(DERIVED_VALIDATOR),
                    "--input-dir",
                    os.fspath(assets),
                    "--verifier-binary",
                    os.fspath(args.verifier_binary),
                    "--inspector-binary",
                    os.fspath(args.inspector_binary),
                    "--genesis",
                    os.fspath(args.genesis),
                    "--repository",
                    args.repository,
                    "--tag",
                    args.tag,
                    "--commit",
                    args.main_commit,
                ],
                "compact certificate validation",
            )
            policy = json.loads((assets / "arc-cutover-policy.json").read_bytes())
            timestamp = policy["all_controlled_stopped_at"]
            commit, tree = create_commit(
                repository, assets, args.main_commit, ref_name, timestamp
            )

        if args.push_remote is not None:
            if REMOTE_RE.fullmatch(args.push_remote) is None:
                fail("push remote name is unsafe")
            existing = run_git(repository, ["ls-remote", "--refs", args.push_remote, ref_name])
            if existing:
                fail("create-only remote handoff ref already exists")
            run_git(
                repository,
                [
                    "push",
                    "--atomic",
                    f"--force-with-lease={ref_name}:",
                    args.push_remote,
                    f"{commit}:{ref_name}",
                ],
            )
        print(
            json.dumps(
                {
                    "handoff_commit_sha": commit,
                    "handoff_ref": ref_name,
                    "handoff_tree_sha": tree,
                    "pushed_remote": args.push_remote,
                },
                sort_keys=True,
                separators=(",", ":"),
            )
        )
        return 0
    except (HandoffCommitError, OSError, json.JSONDecodeError, KeyError) as error:
        print(f"cutover handoff commit: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
