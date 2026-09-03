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
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator, NoReturn, Sequence


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
OBJECT_RE = re.compile(r"^[0-9a-f]{40,64}$")
REMOTE_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
ZERO_COMMIT = "0" * 40
HANDOFF_AUTHOR_NAME = "ARC Recovery Handoff"
HANDOFF_AUTHOR_EMAIL = "recovery-handoff@arc.compute"
HANDOFF_SCHEMA = "arc-recovery-checkpoint-descriptor/v1"
SENSITIVE_ENVIRONMENT_NAMES = (
    "GH_TOKEN",
    "GITHUB_TOKEN",
)


class HandoffCommitError(RuntimeError):
    pass


class GitCommandError(HandoffCommitError):
    pass


class RemoteProbeError(HandoffCommitError):
    pass


@dataclass(frozen=True)
class CommitContract:
    commit: str
    tree: str
    parent: str
    commit_payload: str
    tree_entries: tuple[str, ...]


def fail(message: str) -> NoReturn:
    raise HandoffCommitError(message)


def safe_diagnostic(value: str) -> str:
    redacted = value
    for name in SENSITIVE_ENVIRONMENT_NAMES:
        secret = os.environ.get(name)
        if secret:
            redacted = redacted.replace(secret, "<redacted>")
    redacted = re.sub(r"(https?://)[^/@\s]+@", r"\1<redacted>@", redacted)
    return redacted.strip()[:2000]


def git_environment(
    environment: dict[str, str] | None = None,
) -> dict[str, str]:
    result = {
        "HOME": os.environ.get("HOME", "/var/empty"),
        "PATH": "/usr/bin:/bin:/usr/local/bin",
        "LANG": "C",
        "LC_ALL": "C",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_TERMINAL_PROMPT": "0",
    }
    if environment:
        result.update(environment)
    return result


def write_create_only_executable(path: Path, payload: bytes) -> None:
    descriptor = os.open(
        path,
        os.O_WRONLY
        | os.O_CREAT
        | os.O_EXCL
        | getattr(os, "O_NOFOLLOW", 0),
        0o700,
    )
    try:
        offset = 0
        while offset < len(payload):
            written = os.write(descriptor, payload[offset:])
            if written <= 0:
                fail("credential helper write made no progress")
            offset += written
        os.fchmod(descriptor, 0o700)
    finally:
        os.close(descriptor)


@contextlib.contextmanager
def remote_push_environment(push_url: str) -> Iterator[dict[str, str]]:
    if not push_url.startswith("https://"):
        fail("authenticated handoff publication requires the exact HTTPS remote")
    config = (
        ("credential.helper", ""),
        ("http.extraHeader", ""),
        ("http.https://github.com/.extraHeader", ""),
        ("http.sslVerify", "true"),
        ("protocol.allow", "never"),
        ("protocol.https.allow", "always"),
    )
    environment = {
        "GIT_CONFIG_COUNT": str(len(config)),
        **{f"GIT_CONFIG_KEY_{index}": key for index, (key, _value) in enumerate(config)},
        **{
            f"GIT_CONFIG_VALUE_{index}": value
            for index, (_key, value) in enumerate(config)
        },
    }
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token is None or re.fullmatch(r"[A-Za-z0-9_]{20,1024}", token) is None:
        fail("an HTTPS push requires one bounded GH_TOKEN or GITHUB_TOKEN")
    with tempfile.TemporaryDirectory(prefix="arc-cutover-askpass-") as temporary:
        directory = Path(temporary)
        directory.chmod(0o700)
        askpass = directory / "askpass"
        write_create_only_executable(
            askpass,
            b"#!/bin/sh\n"
            b"case \"$1\" in\n"
            b"  Username*) /usr/bin/printf '%s\\n' x-access-token ;;\n"
            b"  Password*) /usr/bin/printf '%s\\n' \"$ARC_HANDOFF_PUSH_TOKEN\" ;;\n"
            b"  *) exit 1 ;;\n"
            b"esac\n",
        )
        environment.update(
            {
                "ARC_HANDOFF_PUSH_TOKEN": token,
                "GIT_ASKPASS": os.fspath(askpass),
                "GIT_ASKPASS_REQUIRE": "force",
            }
        )
        yield environment


def invoke_git(
    repository: Path,
    arguments: Sequence[str],
    *,
    environment: dict[str, str] | None = None,
    input_text: str | None = None,
    timeout: int = 120,
) -> subprocess.CompletedProcess[str]:
    command = [
        "git",
        "-C",
        os.fspath(repository),
        "-c",
        "core.hooksPath=/dev/null",
        *arguments,
    ]
    try:
        return subprocess.run(
            command,
            input=input_text,
            text=True,
            stdin=None if input_text is not None else subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=git_environment(environment),
            timeout=timeout,
            check=False,
        )
    except OSError as error:
        raise GitCommandError(
            f"git {arguments[0] if arguments else 'command'} could not start: {error}"
        ) from error
    except subprocess.TimeoutExpired as error:
        raise GitCommandError(
            f"git {arguments[0] if arguments else 'command'} timed out"
        ) from error


def run_git(
    repository: Path,
    arguments: Sequence[str],
    *,
    environment: dict[str, str] | None = None,
    input_text: str | None = None,
    timeout: int = 120,
) -> str:
    completed = invoke_git(
        repository,
        arguments,
        environment=environment,
        input_text=input_text,
        timeout=timeout,
    )
    if completed.returncode != 0:
        diagnostic = safe_diagnostic(completed.stderr or completed.stdout)
        raise GitCommandError(
            f"git {' '.join(arguments[:2])} rejected the handoff: {diagnostic}"
        )
    return completed.stdout.strip()


def create_commit(
    repository: Path,
    assets_dir: Path,
    main_commit: str,
    ref_name: str,
    timestamp: str,
) -> tuple[CommitContract, str]:
    expected_ref = f"refs/arc-recovery-handoffs/{main_commit}"
    if ref_name != expected_ref:
        fail(f"handoff ref must be exactly {expected_ref}")
    resolved_main = run_git(repository, ["rev-parse", f"{main_commit}^{{commit}}"])
    if resolved_main != main_commit:
        fail("main commit does not resolve exactly")
    if run_git(repository, ["rev-parse", "--is-bare-repository"]) not in {"true", "false"}:
        fail("repository shape is unsupported")

    with tempfile.TemporaryDirectory(prefix="arc-cutover-index-") as temporary:
        index = Path(temporary) / "index"
        index_env = {"GIT_INDEX_FILE": os.fspath(index)}
        run_git(repository, ["read-tree", "--empty"], environment=index_env)
        tree_entries: list[str] = []
        for filename in PUBLIC_FILES:
            path = assets_dir / filename
            if not path.is_file() or path.is_symlink() or path.stat().st_size <= 0:
                fail(f"derived handoff asset is unavailable: {filename}")
            blob = run_git(
                repository,
                ["hash-object", "-w", "--no-filters", os.fspath(path)],
            )
            if OBJECT_RE.fullmatch(blob) is None:
                fail(f"git returned an invalid blob ID for {filename}")
            run_git(
                repository,
                ["update-index", "--add", "--cacheinfo", f"100644,{blob},{filename}"],
                environment=index_env,
            )
            tree_entries.append(f"100644 blob {blob}\t{filename}")
        tree = run_git(repository, ["write-tree"], environment=index_env)
        identity_env = {
            "GIT_AUTHOR_NAME": HANDOFF_AUTHOR_NAME,
            "GIT_AUTHOR_EMAIL": HANDOFF_AUTHOR_EMAIL,
            "GIT_COMMITTER_NAME": HANDOFF_AUTHOR_NAME,
            "GIT_COMMITTER_EMAIL": HANDOFF_AUTHOR_EMAIL,
            "GIT_AUTHOR_DATE": timestamp,
            "GIT_COMMITTER_DATE": timestamp,
        }
        message = (
            "ARC compact recovery handoff\n\n"
            f"release-main: {main_commit}\n"
            f"schema: {HANDOFF_SCHEMA}\n"
        )
        commit = run_git(
            repository,
            ["-c", "commit.gpgsign=false", "commit-tree", tree, "-p", main_commit],
            environment=identity_env,
            input_text=message,
        )
    if COMMIT_RE.fullmatch(commit) is None:
        fail("git returned an invalid handoff commit ID")
    contract = CommitContract(
        commit=commit,
        tree=tree,
        parent=main_commit,
        commit_payload=run_git(repository, ["cat-file", "commit", commit]),
        tree_entries=tuple(tree_entries),
    )
    validate_commit_contract(repository, commit, contract)
    local_ref_state = ensure_local_ref(repository, ref_name, contract)
    return contract, local_ref_state


def validate_commit_contract(
    repository: Path,
    commit: str,
    expected: CommitContract,
) -> None:
    if commit != expected.commit or COMMIT_RE.fullmatch(commit) is None:
        fail("existing local handoff ref differs from the freshly derived commit")
    resolved = run_git(repository, ["rev-parse", f"{commit}^{{commit}}"])
    if resolved != commit:
        fail("handoff commit does not resolve exactly as a commit")
    parent_line = run_git(repository, ["rev-list", "--parents", "-n", "1", commit])
    if parent_line != f"{commit} {expected.parent}":
        fail("handoff commit does not have the sole exact-main parent")
    actual_tree = run_git(repository, ["show", "-s", "--format=%T", commit])
    if actual_tree != expected.tree:
        fail("handoff commit tree differs from the freshly derived tree")
    listing = tuple(
        run_git(repository, ["ls-tree", "-r", "--full-tree", commit]).splitlines()
    )
    if listing != expected.tree_entries:
        fail("handoff commit assets or modes differ from the exact three-file contract")
    payload = run_git(repository, ["cat-file", "commit", commit])
    if payload != expected.commit_payload:
        fail("handoff commit identity, timestamp, or message metadata differs")


def local_ref_target(repository: Path, ref_name: str) -> str | None:
    symbolic = invoke_git(repository, ["symbolic-ref", "--quiet", ref_name])
    if symbolic.returncode == 0:
        fail("local handoff ref is symbolic; refusing indirect replacement")
    if symbolic.returncode != 1:
        diagnostic = safe_diagnostic(symbolic.stderr or symbolic.stdout)
        fail(f"cannot prove the local handoff ref is direct: {diagnostic}")
    completed = invoke_git(
        repository,
        ["show-ref", "--verify", "--quiet", ref_name],
    )
    if completed.returncode == 1:
        return None
    if completed.returncode != 0:
        diagnostic = safe_diagnostic(completed.stderr or completed.stdout)
        fail(f"cannot prove the local handoff ref state: {diagnostic}")
    values = run_git(repository, ["rev-parse", "--verify", ref_name]).splitlines()
    if len(values) != 1 or COMMIT_RE.fullmatch(values[0]) is None:
        fail("local handoff ref resolved ambiguously or to an invalid object ID")
    return values[0]


def ensure_local_ref(
    repository: Path,
    ref_name: str,
    expected: CommitContract,
) -> str:
    existing = local_ref_target(repository, ref_name)
    if existing is not None:
        validate_commit_contract(repository, existing, expected)
        return "reused"
    try:
        run_git(
            repository,
            ["update-ref", "--no-deref", ref_name, expected.commit, ZERO_COMMIT],
        )
    except GitCommandError as update_error:
        raced = local_ref_target(repository, ref_name)
        if raced == expected.commit:
            validate_commit_contract(repository, raced, expected)
            return "reused"
        if raced is None:
            fail(
                "create-only local handoff ref update failed while the ref remained absent; "
                f"safe to retry: {safe_diagnostic(str(update_error))}"
            )
        fail("local handoff ref changed to a different commit; refusing replacement")
    created = local_ref_target(repository, ref_name)
    if created != expected.commit:
        fail("created local handoff ref does not resolve to the freshly derived commit")
    validate_commit_contract(repository, created, expected)
    return "created"


def isolated_tool_environment(safe_home: Path) -> dict[str, str]:
    if not safe_home.is_dir() or safe_home.is_symlink():
        fail("isolated handoff-tool home is unavailable")
    return {
        "HOME": os.fspath(safe_home),
        "PATH": "/usr/bin:/bin",
        "LANG": "C",
        "LC_ALL": "C",
        "TZ": "UTC",
    }


def run_checked(
    arguments: Sequence[str],
    label: str,
    *,
    environment: dict[str, str],
) -> None:
    try:
        completed = subprocess.run(
            list(arguments),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=environment,
            timeout=600,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        fail(f"{label} failed: {error}")
    if completed.returncode != 0:
        diagnostic = safe_diagnostic(completed.stderr or completed.stdout)
        fail(f"{label} rejected the recovery handoff: {diagnostic}")


def run_isolated_python(
    script: Path,
    arguments: Sequence[str],
    label: str,
    *,
    safe_home: Path,
) -> None:
    run_checked(
        [sys.executable, "-I", os.fspath(script), *arguments],
        label,
        environment=isolated_tool_environment(safe_home),
    )


def allowed_remote_urls(repository_name: str) -> frozenset[str]:
    return frozenset({f"https://github.com/{repository_name}.git"})


def verify_remote_identity(
    repository: Path,
    remote_name: str,
    repository_name: str,
) -> tuple[str, str]:
    if REMOTE_RE.fullmatch(remote_name) is None:
        fail("push remote name is unsafe")
    allowed = allowed_remote_urls(repository_name)
    fetch_urls = run_git(
        repository, ["remote", "get-url", "--all", remote_name]
    ).splitlines()
    push_urls = run_git(
        repository, ["remote", "get-url", "--push", "--all", remote_name]
    ).splitlines()
    if len(fetch_urls) != 1 or len(push_urls) != 1:
        fail("push remote must have one unambiguous fetch URL and one push URL")
    if fetch_urls[0] not in allowed or push_urls[0] not in allowed:
        fail(
            "push remote does not identify the exact credential-free "
            f"github.com/{repository_name}.git repository"
        )
    return fetch_urls[0], push_urls[0]


def probe_remote_ref(
    repository: Path,
    remote_name: str,
    repository_name: str,
    ref_name: str,
    *,
    expected_identity: tuple[str, str] | None = None,
) -> tuple[str | None, tuple[str, str]]:
    try:
        identity = verify_remote_identity(repository, remote_name, repository_name)
        if expected_identity is not None and identity != expected_identity:
            raise RemoteProbeError("push remote identity changed during publication")
        output = run_git(
            repository,
            ["ls-remote", "--refs", remote_name, ref_name],
            timeout=120,
        )
    except GitCommandError as error:
        raise RemoteProbeError(
            "remote handoff ref probe is unavailable; absence was not proven: "
            f"{safe_diagnostic(str(error))}"
        ) from error
    if not output:
        return None, identity
    rows = output.splitlines()
    if len(rows) != 1:
        raise RemoteProbeError("remote handoff ref probe returned multiple records")
    fields = rows[0].split("\t")
    if (
        len(fields) != 2
        or COMMIT_RE.fullmatch(fields[0]) is None
        or fields[1] != ref_name
    ):
        raise RemoteProbeError("remote handoff ref probe returned a malformed record")
    return fields[0], identity


def publish_remote_ref(
    repository: Path,
    remote_name: str,
    repository_name: str,
    ref_name: str,
    commit: str,
) -> str:
    existing, identity = probe_remote_ref(
        repository,
        remote_name,
        repository_name,
        ref_name,
    )
    if existing is not None:
        if existing == commit:
            return "reused"
        fail(
            "remote handoff ref already identifies a different commit; "
            "refusing to move or replace it"
        )

    push_error: GitCommandError | None = None
    try:
        with remote_push_environment(identity[1]) as push_environment:
            run_git(
                repository,
                [
                    "push",
                    "--atomic",
                    "--receive-pack=git-receive-pack",
                    f"--force-with-lease={ref_name}:",
                    remote_name,
                    f"{commit}:{ref_name}",
                ],
                environment=push_environment,
                timeout=120,
            )
    except GitCommandError as error:
        push_error = error

    try:
        observed, _identity = probe_remote_ref(
            repository,
            remote_name,
            repository_name,
            ref_name,
            expected_identity=identity,
        )
    except RemoteProbeError as probe_error:
        detail = " after a push error" if push_error is not None else " after push success"
        fail(
            "remote handoff publication outcome could not be proven"
            f"{detail}; the exact local ref is intact and the operation is safe to retry: "
            f"{safe_diagnostic(str(probe_error))}"
        )
    if observed == commit:
        return "created"
    if observed is None:
        detail = (
            safe_diagnostic(str(push_error))
            if push_error is not None
            else "push returned success but the ref remained absent"
        )
        fail(
            "remote handoff ref remains absent; the exact local ref is intact and "
            f"the operation is safe to retry: {detail}"
        )
    fail(
        "remote handoff ref changed to a different commit during publication; "
        "treat this as an incident and do not replace it"
    )


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
            isolated_home_name = stack.enter_context(
                tempfile.TemporaryDirectory(prefix="arc-cutover-home-")
            )
            assets = Path(temporary)
            isolated_home = Path(isolated_home_name)
            isolated_home.chmod(0o700)
            run_isolated_python(
                ASSEMBLER,
                [
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
                safe_home=isolated_home,
            )
            run_isolated_python(
                DERIVED_VALIDATOR,
                [
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
                safe_home=isolated_home,
            )
            policy = json.loads((assets / "arc-cutover-policy.json").read_bytes())
            timestamp = policy["all_controlled_stopped_at"]
            if not isinstance(timestamp, str) or not timestamp or len(timestamp) > 64:
                fail("cutover policy contains an invalid commit timestamp")
            contract, local_ref_state = create_commit(
                repository, assets, args.main_commit, ref_name, timestamp
            )

        remote_ref_state: str | None = None
        if args.push_remote is not None:
            remote_ref_state = publish_remote_ref(
                repository,
                args.push_remote,
                args.repository,
                ref_name,
                contract.commit,
            )
        print(
            json.dumps(
                {
                    "handoff_commit_sha": contract.commit,
                    "handoff_ref": ref_name,
                    "handoff_tree_sha": contract.tree,
                    "local_ref_state": local_ref_state,
                    "pushed_remote": args.push_remote,
                    "remote_ref_state": remote_ref_state,
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
