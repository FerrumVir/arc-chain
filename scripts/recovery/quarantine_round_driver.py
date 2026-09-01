#!/usr/bin/env python3
"""Create and verify local artifacts for crash-safe quarantine rounds.

The fleet shell owns the authenticated SSH transport.  This helper owns the
canonical JSON state machine on the operator disk, so retries cannot silently
replace an authorization, readiness receipt, applied receipt, or transition.
Zero-progress attempts remain under their attempt directory; only positive
single-node transitions are copied into ``round-N`` and the final ledger.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys
from typing import Any, Mapping, Sequence

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import quarantine_rounds as rounds  # noqa: E402


class DriverError(ValueError):
    pass


def fail(message: str) -> None:
    raise DriverError(message)


def canonical(value: Any) -> bytes:
    return rounds.canonical_bytes(value)


def digest_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def read_canonical(path: Path, label: str, *, mode: int = 0o400) -> dict[str, Any]:
    if not path.is_absolute() or os.fspath(path) != os.path.normpath(os.fspath(path)):
        fail(f"{label} path is unsafe")
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        before = os.fstat(fd)
        identity = lambda item: (
            item.st_dev, item.st_ino, item.st_mode, item.st_uid, item.st_gid,
            item.st_nlink, item.st_size, item.st_mtime_ns, item.st_ctime_ns,
        )
        if (
            not stat.S_ISREG(before.st_mode)
            or before.st_uid not in {0, os.geteuid()}
            or stat.S_IMODE(before.st_mode) != mode
            or before.st_nlink != 1
            or before.st_size <= 0
            or before.st_size > 32 * 1024 * 1024
        ):
            fail(f"{label} identity differs")
        chunks: list[bytes] = []
        while True:
            chunk = os.read(fd, 1024 * 1024)
            if not chunk:
                break
            chunks.append(chunk)
        raw = b"".join(chunks)
        if len(raw) != before.st_size or identity(os.fstat(fd)) != identity(before):
            fail(f"{label} changed while read")
    finally:
        os.close(fd)
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise DriverError(f"{label} is invalid JSON") from error
    if not isinstance(value, dict) or canonical(value) != raw:
        fail(f"{label} is not canonical JSON")
    return value


def publish(path: Path, value: Mapping[str, Any], label: str) -> str:
    """Publish canonical bytes without replacing a concurrently-created final."""
    if not path.is_absolute() or path.suffix != ".json":
        fail(f"{label} output path is unsafe")
    payload = canonical(value)
    parent = path.parent
    details = parent.lstat()
    if (
        parent.is_symlink()
        or not stat.S_ISDIR(details.st_mode)
        or details.st_uid != os.geteuid()
        or stat.S_IMODE(details.st_mode) != 0o700
    ):
        fail(f"{label} output directory is unsafe")
    partial = path.with_name(path.name + ".partial")
    dfd = os.open(parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0))

    def read_name(name: str, modes: set[int], links: set[int], allow_empty: bool = False) -> bytes:
        fd = os.open(name, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=dfd)
        try:
            info = os.fstat(fd)
            if (
                not stat.S_ISREG(info.st_mode)
                or info.st_uid != os.geteuid()
                or stat.S_IMODE(info.st_mode) not in modes
                or info.st_nlink not in links
                or info.st_size < (0 if allow_empty else 1)
                or info.st_size > 32 * 1024 * 1024
            ):
                fail(f"{label} publication identity differs")
            raw = b""
            while len(raw) <= 32 * 1024 * 1024:
                chunk = os.read(fd, min(1024 * 1024, 32 * 1024 * 1024 + 1 - len(raw)))
                if not chunk:
                    break
                raw += chunk
            if len(raw) != info.st_size:
                fail(f"{label} changed while read")
            return raw
        finally:
            os.close(fd)

    try:
        if path.exists() or path.is_symlink():
            same = bool((partial.exists() or partial.is_symlink()) and os.path.samefile(path, partial))
            current = read_name(path.name, {0o400}, {2} if same else {1})
            if current != payload:
                fail(f"existing {label} differs")
            if partial.exists() or partial.is_symlink():
                fragment = read_name(partial.name, {0o400, 0o600}, {1, 2}, True)
                if fragment and fragment != payload:
                    fail(f"{label} canonical partial conflicts with terminal")
                os.unlink(partial.name, dir_fd=dfd)
                os.fsync(dfd)
            return digest_bytes(payload)

        promote = False
        if partial.exists() or partial.is_symlink():
            fragment = read_name(partial.name, {0o400, 0o600}, {1}, True)
            if fragment == payload:
                os.chmod(partial.name, 0o400, dir_fd=dfd, follow_symlinks=False)
                fd = os.open(partial.name, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0), dir_fd=dfd)
                try:
                    os.fsync(fd)
                finally:
                    os.close(fd)
                promote = True
            else:
                if fragment:
                    try:
                        decoded = json.loads(fragment)
                    except (UnicodeDecodeError, json.JSONDecodeError):
                        decoded = None
                    if isinstance(decoded, dict) and fragment == canonical(decoded):
                        fail(f"{label} canonical partial has conflicting identity")
                os.unlink(partial.name, dir_fd=dfd)
                os.fsync(dfd)
        if not promote:
            fd = os.open(
                partial.name,
                os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
                0o600,
                dir_fd=dfd,
            )
            with os.fdopen(fd, "wb") as handle:
                handle.write(payload)
                handle.flush()
                os.fchmod(handle.fileno(), 0o400)
                os.fsync(handle.fileno())
        try:
            os.link(
                partial.name, path.name, src_dir_fd=dfd, dst_dir_fd=dfd,
                follow_symlinks=False,
            )
        except FileExistsError:
            terminal = read_name(path.name, {0o400}, {1, 2})
            if terminal != payload:
                fail(f"concurrent {label} terminal differs")
        os.unlink(partial.name, dir_fd=dfd)
        os.fsync(dfd)
        return digest_bytes(payload)
    finally:
        os.close(dfd)


def load_freeze(path: Path, expected: str) -> dict[str, Any]:
    value = read_canonical(path, "freeze plan")
    raw = canonical(value)
    if digest_bytes(raw) != expected or value.get("schema") != "arc.recovery.freeze-plan.v5":
        fail("freeze plan root/schema differs")
    if [(row.get("name"), row.get("host")) for row in value.get("nodes", [])] != list(rounds.FLEET):
        fail("freeze plan fleet differs")
    return value


def load_prefix(root: Path, through: int) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    authorizations: list[dict[str, Any]] = []
    results: list[dict[str, Any]] = []
    for number in range(1, through + 1):
        directory = root / f"round-{number}"
        authorization = read_canonical(directory / "authorization.json", f"round {number} authorization")
        result = read_canonical(directory / "result.json", f"round {number} result")
        transitions = [
            rounds.validate_wrapper(item, "prefix transition")[0]
            for item in result.get("transitions", [])
        ]
        rounds.validate_round_result(
            result, authorization=authorization, prior_results=results,
            transition_receipts=transitions,
        )
        authorizations.append(authorization)
        results.append(result)
    return authorizations, results


def build_cross(args: argparse.Namespace) -> dict[str, Any]:
    freeze = load_freeze(args.freeze_plan, args.freeze_plan_sha256)
    public = read_canonical(args.public, "target public-height receipt")
    targets = args.targets.split(",")
    expected_targets = [name for name, _host in rounds.FLEET if name in set(targets)]
    if not targets or targets != expected_targets or len(targets) != len(set(targets)):
        fail("target nodes differ from the fixed fleet order")
    rounds.validate_target_height_receipt(
        public, capture_id=args.capture_id, freeze_sha256=args.freeze_plan_sha256,
        targets=targets,
    )
    fields = {
        "schema", "capture_id", "node", "freeze_plan_sha256", "challenge",
        "rpc_origin", "writer_pid", "writer_start_ticks", "boot_id",
        "executable_sha256", "argv_sha256", "started_at", "completed_at",
        "public_info_before_height", "public_latest_block_height",
        "public_info_after_height", "public_latest_block_hash",
        "authenticated_info_before_height", "authenticated_latest_block_height",
        "authenticated_info_after_height", "authenticated_latest_block_hash",
        "authenticated_info_before_body_sha256", "authenticated_latest_block_body_sha256",
        "authenticated_info_after_body_sha256", "conservative_height_floor",
    }
    nodes = []
    brackets = []
    for target in targets:
        bracket = read_canonical(
            args.bracket_root / f"{target}.json", f"{target} authenticated target bracket"
        )
        brackets.append(bracket)
        public_row = next(row for row in public["origins"] if row["name"] == target)
        frozen = next(row for row in freeze["nodes"] if row["name"] == target)
        expected = {
            "schema": "arc.recovery.authenticated-legacy-height-bracket.v1",
            "capture_id": args.capture_id, "node": target,
            "freeze_plan_sha256": args.freeze_plan_sha256,
            "rpc_origin": frozen["rpc_origin"], "writer_pid": frozen["writer_pid"],
            "writer_start_ticks": frozen["writer_start_ticks"], "boot_id": frozen["boot_id"],
            "executable_sha256": frozen["executable_sha256"],
            "argv_sha256": frozen["argv_sha256"],
            "public_info_before_height": public_row["info_before_height"],
            "public_latest_block_height": public_row["latest_block_height"],
            "public_info_after_height": public_row["info_after_height"],
            "public_latest_block_hash": public_row["latest_block_hash"],
        }
        if set(bracket) != fields or any(
            bracket.get(key) != value for key, value in expected.items()
        ):
            fail(f"{target} authenticated target bracket identity/fields differ")
        if bracket.get("conservative_height_floor") != max(
            public_row["info_after_height"], bracket.get("authenticated_info_after_height", -1)
        ):
            fail(f"{target} authenticated target conservative floor differs")
        nodes.append({
            "node": target, "host": rounds.FLEET_MAP[target],
            "writer_pid": frozen["writer_pid"],
            "writer_start_ticks": frozen["writer_start_ticks"],
            "boot_id": frozen["boot_id"],
            "writer_cgroup_sha256": frozen["writer_cgroup_sha256"],
            "public_info_after_height": public_row["info_after_height"],
            "public_latest_block_height": public_row["latest_block_height"],
            "public_latest_block_hash": public_row["latest_block_hash"],
            "loopback_info_before_height": bracket["authenticated_info_before_height"],
            "loopback_latest_height": bracket["authenticated_latest_block_height"],
            "loopback_info_after_height": bracket["authenticated_info_after_height"],
            "loopback_latest_block_hash": bracket["authenticated_latest_block_hash"],
            "response_sha256": {
                "/info:before": bracket["authenticated_info_before_body_sha256"],
                "/block/latest": bracket["authenticated_latest_block_body_sha256"],
                "/info:after": bracket["authenticated_info_after_body_sha256"],
            },
        })
    value = {
        "schema": rounds.TARGET_CROSS_SCHEMA,
        "source_main_commit": freeze["source_commit"],
        "freeze_plan_sha256": args.freeze_plan_sha256,
        "capture_id": args.capture_id,
        "legacy_public_height_receipt_sha256": rounds.digest(public),
        "challenge": brackets[0]["challenge"],
        "started_at": min(item["started_at"] for item in brackets),
        "completed_at": max(item["completed_at"] for item in brackets),
        "conservative_height_floor": min(item["loopback_info_before_height"] for item in nodes),
        "targets": public["targets"], "nodes": nodes,
    }
    if any(item["challenge"] != value["challenge"] for item in brackets):
        fail("authenticated target brackets use different challenges")
    rounds.validate_target_cross_receipt(
        value, capture_id=args.capture_id, freeze_sha256=args.freeze_plan_sha256,
        targets=targets, public_sha256=rounds.digest(public), public_receipt=public,
    )
    return value


def build_authorization(args: argparse.Namespace) -> dict[str, Any]:
    freeze = load_freeze(args.freeze_plan, args.freeze_plan_sha256)
    _previous_auth, previous_results = load_prefix(args.round_root, args.round_number - 1)
    public = read_canonical(args.public, "round target public receipt")
    cross = read_canonical(args.cross, "round target authenticated cross proof")
    targets = [row.get("node") for row in public.get("targets", [])]
    if not targets:
        fail("production quarantine round has no live targets")
    public_started, public_completed, _maximum = rounds.validate_target_height_receipt(
        public, capture_id=args.capture_id, freeze_sha256=args.freeze_plan_sha256,
        targets=targets,
    )
    rounds.validate_target_cross_receipt(
        cross, capture_id=args.capture_id, freeze_sha256=args.freeze_plan_sha256,
        targets=targets, public_sha256=rounds.digest(public), public_receipt=public,
    )
    del public_started
    transitions_by_node: dict[str, dict[str, Any]] = {}
    for result in previous_results:
        for wrapper in result["transitions"]:
            transition, transition_sha = rounds.validate_wrapper(
                wrapper, "prior transition"
            )
            transitions_by_node[transition["node"]] = {
                **transition, "_sha256": transition_sha,
            }
    prior_rows = []
    for name, host in rounds.FLEET:
        if name not in transitions_by_node:
            continue
        transition = transitions_by_node[name]
        projection = rounds.validate_node_transition(transition)
        status = read_canonical(args.prior_status_root / f"{name}.json", f"{name} current fenced status")
        prior_rows.append({
            "node": name, "host": host,
            "node_transition_receipt_sha256": transition["_sha256"],
            "transition_schema": projection["schema"],
            "transitioned_at": projection["secured_at_raw"],
            "stable_head": projection["stable_head"],
            "persistent_restart_fence_sha256":
                projection["persistent_restart_fence_sha256"],
            "current_status": rounds.wrap(status),
        })
    remaining = [name for name, _host in rounds.FLEET if name not in transitions_by_node]
    if targets != remaining:
        fail("round targets are not the complete fixed-order still-live partition")
    live_source_captures = [
        read_canonical(
            args.source_capture_root / f"{target}.json",
            f"{target} live source capture",
        )
        for target in targets
    ]
    authorized_at = utc_now()
    deadline = public_completed + dt.timedelta(seconds=rounds.MAX_WINDOW_SECONDS)
    if rounds.parse_utc(authorized_at, "authorization now") > deadline:
        fail("target public receipt expired before round authorization")
    value = {
        "schema": rounds.ROUND_AUTH_SCHEMA,
        "capture_id": args.capture_id,
        "freeze_plan_sha256": args.freeze_plan_sha256,
        "round_number": args.round_number,
        "source_main_commit": freeze["source_commit"],
        "prior_round_result_sha256s": [rounds.digest(value) for value in previous_results],
        "prior_fenced": prior_rows,
        "targets": [
            {
                "node": target, "host": rounds.FLEET_MAP[target],
                "boot_id": frozen["boot_id"], "writer_pid": frozen["writer_pid"],
                "writer_start_ticks": frozen["writer_start_ticks"],
                "writer_cgroup_sha256": frozen["writer_cgroup_sha256"],
            }
            for target in targets
            for frozen in freeze["nodes"] if frozen["name"] == target
        ],
        "public_height_receipt": rounds.wrap(public),
        "authenticated_height_cross_proof": rounds.wrap(cross),
        "live_source_captures": [rounds.wrap(item) for item in live_source_captures],
        "authorized_at": authorized_at,
        "authorization_deadline": deadline.strftime("%Y-%m-%dT%H:%M:%SZ"),
    }
    rounds.validate_round_authorization(value, prior_results=previous_results)
    return value


def build_readiness(args: argparse.Namespace) -> dict[str, Any]:
    authorization = read_canonical(args.authorization, "round authorization")
    _previous_auth, previous_results = load_prefix(args.round_root, args.round_number - 1)
    state = rounds.validate_round_authorization(authorization, prior_results=previous_results)
    targets = state["target_names"]
    acceptances = {
        name: read_canonical(
            args.acceptance_root / f"{name}.json", f"{name} round authorization acceptance"
        )
        for name in targets
    }
    value = {
        "schema": rounds.READINESS_SCHEMA,
        "capture_id": state["capture_id"], "freeze_plan_sha256": state["freeze_plan_sha256"],
        "round_number": state["round_number"],
        "round_authorization_sha256": rounds.digest(authorization),
        "targets": [
            {
                "node": name, "host": rounds.FLEET_MAP[name],
                "authorization_acceptance": rounds.wrap(acceptances[name]),
            }
            for name in targets
        ],
        "completed_at": utc_now(),
        "authorization_deadline": authorization["authorization_deadline"],
    }
    probe = {
        "schema": rounds.ROUND_RESULT_SCHEMA,
        "capture_id": state["capture_id"], "freeze_plan_sha256": state["freeze_plan_sha256"],
        "round_number": state["round_number"],
        "round_authorization_sha256": rounds.digest(authorization),
        "target_readiness": rounds.wrap(value), "transitions": [],
        "remaining_targets": targets,
        "completed_at": authorization["authorization_deadline"],
    }
    rounds.validate_round_result(
        probe, authorization=authorization, prior_results=previous_results,
        transition_receipts=[],
    )
    return value


def build_result(args: argparse.Namespace) -> dict[str, Any]:
    authorization = read_canonical(args.authorization, "round authorization")
    readiness = read_canonical(args.readiness, "round readiness")
    _previous_auth, previous_results = load_prefix(args.round_root, args.round_number - 1)
    state = rounds.validate_round_authorization(
        authorization, prior_results=previous_results
    )
    transitions = []
    for name in state["target_names"]:
        path = args.applied_root / f"{name}.json"
        if path.exists() and not path.is_symlink():
            transitions.append(read_canonical(path, f"{name} round transition receipt"))
    transitioned_names = {item["node"] for item in transitions}
    value = {
        "schema": rounds.ROUND_RESULT_SCHEMA,
        "capture_id": authorization["capture_id"],
        "freeze_plan_sha256": authorization["freeze_plan_sha256"],
        "round_number": args.round_number,
        "round_authorization_sha256": rounds.digest(authorization),
        "target_readiness": rounds.wrap(readiness),
        "transitions": [rounds.wrap(item) for item in transitions],
        "remaining_targets": [
            name for name in state["target_names"] if name not in transitioned_names
        ],
        "completed_at": utc_now(),
    }
    rounds.validate_round_result(
        value, authorization=authorization, prior_results=previous_results,
        transition_receipts=transitions,
    )
    return value


def build_ledger(args: argparse.Namespace) -> dict[str, Any]:
    count = 0
    for number in range(1, len(rounds.FLEET) + 1):
        result = args.round_root / f"round-{number}" / "result.json"
        if result.exists() and not result.is_symlink():
            if number != count + 1:
                fail("quarantine transition rounds are not contiguous")
            count = number
        else:
            break
    if count == 0:
        fail("quarantine transition prefix is empty")
    authorizations, results = load_prefix(args.round_root, count)
    transitions = [
        rounds.validate_wrapper(wrapper, "ledger transition")[0]
        for result in results for wrapper in result["transitions"]
    ]
    projections = [rounds.validate_node_transition(item) for item in transitions]
    first = min(item["secured_at"] for item in projections)
    all_secured = max(item["verified_at"] for item in projections)
    public_maxima = [
        authorization["public_height_receipt"]["value"]["legacy_public_max_height"]
        for authorization in authorizations
    ]
    value = {
        "schema": rounds.LEDGER_SCHEMA,
        "capture_id": args.capture_id, "freeze_plan_sha256": args.freeze_plan_sha256,
        "fleet": [{"node": name, "host": host} for name, host in rounds.FLEET],
        "rounds": [
            {"authorization": rounds.wrap(authorization), "result": rounds.wrap(result)}
            for authorization, result in zip(authorizations, results)
        ],
        "first_secured_at": first.strftime("%Y-%m-%dT%H:%M:%SZ"),
        "all_nodes_secured_at": all_secured.strftime("%Y-%m-%dT%H:%M:%SZ"),
        "legacy_cutoff_height": max(
            [*public_maxima, *[item["height"] for item in projections]]
        ),
    }
    rounds.validate_generation_ledger(value)
    return value


def command_extract(args: argparse.Namespace) -> int:
    ledger = read_canonical(args.ledger, "quarantine generation ledger")
    rounds.validate_generation_ledger(ledger)
    found = []
    for row in ledger["rounds"]:
        authorization = row["authorization"]["value"]
        readiness = row["result"]["value"]["target_readiness"]["value"]
        for wrapper in row["result"]["value"]["transitions"]:
            if wrapper["value"]["node"] == args.node:
                found.append((authorization, readiness, wrapper["value"], wrapper["sha256"]))
    if len(found) != 1:
        fail("ledger node transition receipt is missing or ambiguous")
    authorization, readiness, transition, transition_sha = found[0]
    if args.kind == "refs":
        sys.stdout.write(
            f"{authorization['round_number']} {rounds.digest(authorization)} "
            f"{rounds.digest(readiness)} {transition_sha}\n"
        )
    elif args.kind == "transition":
        sys.stdout.buffer.write(canonical(transition))
    elif args.kind == "network":
        if transition.get("schema") != rounds.NODE_APPLIED_SCHEMA:
            fail("persistently-stopped transition has no network-quarantine receipt")
        sys.stdout.buffer.write(
            canonical(transition["network_quarantine_receipt"]["value"])
        )
    else:
        sys.stdout.write(rounds.validate_node_transition(transition)["kind"] + "\n")
    return 0


def command_prefix_ref(args: argparse.Namespace) -> int:
    authorizations, results = load_prefix(args.round_root, args.through)
    found = []
    for authorization, result in zip(authorizations, results):
        readiness = result["target_readiness"]["value"]
        for wrapper in result["transitions"]:
            if wrapper["value"]["node"] == args.node:
                found.append((authorization, readiness, wrapper["sha256"]))
    if len(found) != 1:
        fail("transition prefix node receipt is missing or ambiguous")
    authorization, readiness, transition_sha = found[0]
    sys.stdout.write(
        f"{authorization['round_number']} {rounds.digest(authorization)} "
        f"{rounds.digest(readiness)} {transition_sha}\n"
    )
    return 0


def build_first_boundary(args: argparse.Namespace) -> dict[str, Any]:
    ledger = read_canonical(args.ledger, "quarantine generation ledger")
    state = rounds.validate_generation_ledger(ledger)
    all_wrappers = [
        wrapper for row in ledger["rounds"]
        for wrapper in row["result"]["value"]["transitions"]
    ]
    first_wrapper = next(
        (wrapper for wrapper in all_wrappers
         if rounds.validate_node_transition(wrapper["value"])["secured_at_raw"]
            == ledger["first_secured_at"]),
        None,
    )
    if first_wrapper is None:
        fail("transition ledger does not derive the first secured boundary")
    return {
        "schema": "arc.recovery.stop-boundary-timestamp.v2",
        "boundary": "first-quarantine-started",
        "freeze_plan_sha256": state["freeze_plan_sha256"],
        "capture_id": state["capture_id"],
        "timestamp": ledger["first_secured_at"],
        "quarantine_generation_ledger_sha256": rounds.digest(ledger),
        "first_node_transition_sha256": first_wrapper["sha256"],
    }


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    commands = root.add_subparsers(dest="command", required=True)

    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--freeze-plan", required=True, type=Path)
    common.add_argument("--freeze-plan-sha256", required=True)
    common.add_argument("--capture-id", required=True)

    cross = commands.add_parser("build-cross", parents=[common])
    cross.add_argument("--targets", required=True)
    cross.add_argument("--public", required=True, type=Path)
    cross.add_argument("--bracket-root", required=True, type=Path)
    cross.add_argument("--output", required=True, type=Path)

    authorization = commands.add_parser("build-authorization", parents=[common])
    authorization.add_argument("--round-number", type=int, required=True)
    authorization.add_argument("--round-root", type=Path, required=True)
    authorization.add_argument("--public", type=Path, required=True)
    authorization.add_argument("--cross", type=Path, required=True)
    authorization.add_argument("--prior-status-root", type=Path, required=True)
    authorization.add_argument("--source-capture-root", type=Path, required=True)
    authorization.add_argument("--output", type=Path, required=True)

    readiness = commands.add_parser("build-readiness")
    readiness.add_argument("--round-number", type=int, required=True)
    readiness.add_argument("--round-root", type=Path, required=True)
    readiness.add_argument("--authorization", type=Path, required=True)
    readiness.add_argument("--acceptance-root", type=Path, required=True)
    readiness.add_argument("--output", type=Path, required=True)

    result = commands.add_parser("build-result")
    result.add_argument("--round-number", type=int, required=True)
    result.add_argument("--round-root", type=Path, required=True)
    result.add_argument("--authorization", type=Path, required=True)
    result.add_argument("--readiness", type=Path, required=True)
    result.add_argument("--applied-root", type=Path, required=True)
    result.add_argument("--output", type=Path, required=True)

    ledger = commands.add_parser("build-ledger")
    ledger.add_argument("--round-root", type=Path, required=True)
    ledger.add_argument("--freeze-plan-sha256", required=True)
    ledger.add_argument("--capture-id", required=True)
    ledger.add_argument("--output", type=Path, required=True)

    extract = commands.add_parser("extract")
    extract.add_argument("--ledger", type=Path, required=True)
    extract.add_argument("--node", required=True)
    extract.add_argument(
        "--kind", choices=("refs", "transition", "network", "transition-kind"),
        required=True,
    )

    prefix = commands.add_parser("prefix-ref")
    prefix.add_argument("--round-root", type=Path, required=True)
    prefix.add_argument("--through", type=int, required=True)
    prefix.add_argument("--node", required=True)

    boundary = commands.add_parser("build-first-boundary")
    boundary.add_argument("--ledger", type=Path, required=True)
    boundary.add_argument("--output", type=Path, required=True)
    return root


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        builders = {
            "build-cross": build_cross,
            "build-authorization": build_authorization,
            "build-readiness": build_readiness,
            "build-result": build_result,
            "build-ledger": build_ledger,
        }
        if args.command == "extract":
            return command_extract(args)
        if args.command == "prefix-ref":
            return command_prefix_ref(args)
        if args.command == "build-first-boundary":
            value = build_first_boundary(args)
            sha = publish(args.output, value, "first quarantine boundary")
            print(json.dumps({"output": str(args.output), "sha256": sha}, sort_keys=True, separators=(",", ":")))
            return 0
        value = builders[args.command](args)
        sha = publish(args.output, value, args.command.replace("-", " "))
        print(json.dumps({"output": str(args.output), "sha256": sha}, sort_keys=True, separators=(",", ":")))
        return 0
    except (DriverError, rounds.QuarantineRoundError, KeyError, OSError, ValueError) as error:
        print(f"quarantine round driver: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
