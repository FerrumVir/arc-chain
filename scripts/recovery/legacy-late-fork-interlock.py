#!/usr/bin/env python3
"""Fail-closed publication interlock for declared post-recovery legacy sources.

The online monitor deliberately does not claim cryptographic proof that a
remote legacy source is valid. A coherent observation above the sealed cutoff
is treated as a fork *candidate*. Any response from a one-way-retired official
origin is also a retirement-integrity violation. Either condition immediately
creates an immutable incident and leaves public publication in maintenance
until an operator performs the separate offline disposition/GO procedure.
"""

from __future__ import annotations

import argparse
import contextlib
import datetime as dt
import hashlib
import http.server
import json
import os
import re
import socket
import socketserver
import stat
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, Mapping, NoReturn, Sequence


SOURCE_SET_SCHEMA = "arc.recovery.legacy-late-fork-source-set.v1"
STATUS_SCHEMA = "arc.recovery.legacy-late-fork-interlock-status.v2"
INCIDENT_SCHEMA = "arc.recovery.legacy-late-fork-incident.v1"
HASH_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
MAX_JSON_BYTES = 4 * 1024 * 1024
MAX_RESPONSE_BYTES = 8 * 1024 * 1024
UNIX_RUNTIME_ROOT = Path("/run")
OFFICIAL = (
    ("nyc", "149.28.32.76", "http://149.28.32.76:9090"),
    ("lax", "140.82.16.112", "http://140.82.16.112:9090"),
    ("ams", "136.244.109.1", "http://136.244.109.1:9090"),
    ("lhr", "104.238.171.11", "http://104.238.171.11:9090"),
    ("nrt", "202.182.107.41", "http://202.182.107.41:9090"),
    ("sgp", "149.28.153.31", "http://149.28.153.31:9090"),
)


class InterlockError(RuntimeError):
    pass


def fail(message: str) -> NoReturn:
    raise InterlockError(message)


def canonical(value: Any) -> bytes:
    try:
        return (
            json.dumps(
                value,
                sort_keys=True,
                separators=(",", ":"),
                ensure_ascii=True,
                allow_nan=False,
            )
            + "\n"
        ).encode("utf-8")
    except (TypeError, ValueError) as error:
        fail(f"value is not canonical JSON: {error}")


def digest(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def exact_object(value: Any, fields: set[str], label: str) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != fields:
        fail(f"{label} fields differ")
    return value


def exact_hash(value: Any, label: str) -> str:
    if not isinstance(value, str) or HASH_RE.fullmatch(value) is None:
        fail(f"{label} must be one lowercase SHA-256")
    return value


def exact_uint(value: Any, label: str, *, positive: bool = False) -> int:
    floor = 1 if positive else 0
    if isinstance(value, bool) or not isinstance(value, int) or value < floor:
        fail(f"{label} must be an integer >= {floor}")
    return value


def utc_now() -> dt.datetime:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0)


def utc_text(value: dt.datetime) -> str:
    return value.astimezone(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_utc(value: Any, label: str) -> dt.datetime:
    if not isinstance(value, str) or re.fullmatch(
        r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", value
    ) is None:
        fail(f"{label} is not canonical UTC seconds")
    try:
        return dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=dt.timezone.utc
        )
    except ValueError:
        fail(f"{label} is not a real UTC timestamp")


def read_locked(path: Path, label: str, *, maximum: int = MAX_JSON_BYTES) -> bytes:
    if not path.is_absolute() or os.path.normpath(os.fspath(path)) != os.fspath(path):
        fail(f"{label} path must be canonical absolute")
    try:
        fd = os.open(
            path,
            os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0),
        )
    except OSError as error:
        fail(f"cannot open {label}: {error}")
    try:
        before = os.fstat(fd)
        visible = path.lstat()
        identity = lambda item: (
            item.st_dev,
            item.st_ino,
            item.st_mode,
            item.st_uid,
            item.st_gid,
            item.st_nlink,
            item.st_size,
            item.st_mtime_ns,
            item.st_ctime_ns,
        )
        if (
            not stat.S_ISREG(before.st_mode)
            or stat.S_ISLNK(visible.st_mode)
            or identity(before) != identity(visible)
            or before.st_uid not in {0, os.geteuid()}
            or before.st_nlink != 1
            or stat.S_IMODE(before.st_mode) != 0o400
            or not 0 < before.st_size <= maximum
        ):
            fail(f"{label} file identity differs")
        chunks: list[bytes] = []
        remaining = maximum + 1
        while remaining:
            part = os.read(fd, min(1024 * 1024, remaining))
            if not part:
                break
            chunks.append(part)
            remaining -= len(part)
        raw = b"".join(chunks)
        if len(raw) != before.st_size or identity(os.fstat(fd)) != identity(before):
            fail(f"{label} changed while read")
        return raw
    finally:
        os.close(fd)


def decode_canonical(raw: bytes, label: str) -> dict[str, Any]:
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"{label} is not JSON: {error}")
    if not isinstance(value, dict) or canonical(value) != raw:
        fail(f"{label} is not one canonical JSON object")
    return value


def validate_origin(origin: Any, *, official: bool, label: str) -> str:
    if not isinstance(origin, str):
        fail(f"{label} origin must be a string")
    parsed = urllib.parse.urlsplit(origin)
    allowed_scheme = "http" if official else "https"
    if (
        parsed.scheme != allowed_scheme
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.path not in {"", "/"}
        or parsed.query
        or parsed.fragment
        or parsed.port is None
        or origin.rstrip("/") != origin
    ):
        fail(f"{label} origin is not an exact {allowed_scheme.upper()} authority")
    return origin


def load_source_set(
    path: Path,
    *,
    expected_sha256: str,
    expected_boundary_sha256: str,
    expected_tool_sha256: str,
) -> tuple[dict[str, Any], str]:
    raw = read_locked(path, "legacy late-fork source set")
    observed_sha = digest(raw)
    if observed_sha != exact_hash(expected_sha256, "expected source-set sha256"):
        fail("legacy late-fork source-set bytes differ from the selected root")
    value = exact_object(
        decode_canonical(raw, "legacy late-fork source set"),
        {
            "schema",
            "source_main_commit",
            "boundary_sha256",
            "observed_cutoff_height",
            "official_origins",
            "monitored_retired_origins",
            "monitored_community_origins",
            "poll_interval_seconds",
            "max_staleness_seconds",
            "validation_mode",
            "validation_tool_sha256",
            "global_absence_claimed",
        },
        "legacy late-fork source set",
    )
    if (
        value.get("schema") != SOURCE_SET_SCHEMA
        or not isinstance(value.get("source_main_commit"), str)
        or COMMIT_RE.fullmatch(value["source_main_commit"]) is None
        or value.get("boundary_sha256")
        != exact_hash(expected_boundary_sha256, "expected maintenance boundary sha256")
        or value.get("validation_tool_sha256")
        != exact_hash(expected_tool_sha256, "expected interlock tool sha256")
        or value.get("validation_mode")
        != "capture-bound-retirement-tripwire-offline-validation-required"
        or value.get("global_absence_claimed") is not False
    ):
        fail("legacy late-fork source-set identity/policy differs")
    exact_uint(value.get("observed_cutoff_height"), "source-set observed cutoff", positive=True)
    poll = exact_uint(value.get("poll_interval_seconds"), "source-set poll interval", positive=True)
    staleness = exact_uint(
        value.get("max_staleness_seconds"), "source-set max staleness", positive=True
    )
    if poll != 30 or staleness != 90 or staleness < poll * 2:
        fail("legacy late-fork source-set timing policy differs")
    official_rows = value.get("official_origins")
    expected_official = [
        {"name": name, "host": host, "origin": origin}
        for name, host, origin in OFFICIAL
    ]
    if official_rows != expected_official:
        fail("legacy late-fork source set official origins differ")
    retired = value.get("monitored_retired_origins")
    community = value.get("monitored_community_origins")
    if not isinstance(retired, list) or not isinstance(community, list):
        fail("legacy late-fork monitored source inventories must be arrays")
    source_fields = {"name", "origin"}
    expected_retired = [
        {"name": name, "origin": origin} for name, _host, origin in OFFICIAL
    ]
    if retired[: len(expected_retired)] != expected_retired:
        fail("legacy late-fork retired inventory must begin with the official six")
    coordinates: set[tuple[str, str]] = set()
    for scope, rows in (("retired", retired), ("community", community)):
        for index, raw_row in enumerate(rows):
            row = exact_object(raw_row, source_fields, f"{scope} source {index}")
            name = row.get("name")
            if not isinstance(name, str) or re.fullmatch(r"[a-z0-9][a-z0-9-]{0,63}", name) is None:
                fail(f"{scope} source {index} name is unsafe")
            origin = validate_origin(
                row.get("origin"),
                official=scope == "retired" and index < len(OFFICIAL),
                label=f"{scope} source {name}",
            )
            coordinate = (name, origin)
            if coordinate in coordinates:
                fail("legacy late-fork source set repeats a source")
            coordinates.add(coordinate)
    return value, observed_sha


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # type: ignore[no-untyped-def]
        return None


def request_json(origin: str, path: str, timeout: float) -> tuple[dict[str, Any], str]:
    url = origin + path
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/json",
            "Connection": "close",
            "User-Agent": "arc-legacy-late-fork-interlock/1",
        },
    )
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    with opener.open(request, timeout=timeout) as response:
        if response.status != 200 or response.geturl() != url:
            fail(f"source returned unexpected status for {path}")
        raw = response.read(MAX_RESPONSE_BYTES + 1)
    if len(raw) > MAX_RESPONSE_BYTES:
        fail(f"source response is oversized for {path}")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError):
        fail(f"source response is not JSON for {path}")
    if not isinstance(value, dict):
        fail(f"source response is not an object for {path}")
    return value, digest(raw)


def observe_source(source: Mapping[str, Any], scope: str) -> dict[str, Any]:
    name = source["name"]
    origin = source["origin"]
    base = {"name": name, "origin": origin, "scope": scope}
    try:
        before, before_sha = request_json(origin, "/info", 5.0)
        latest, latest_sha = request_json(origin, "/block/latest", 5.0)
        header = latest.get("header")
        if not isinstance(header, dict):
            fail("latest block omits its header")
        height = exact_uint(header.get("height"), "legacy source latest height", positive=True)
        block_hash = exact_hash(latest.get("hash"), "legacy source latest block hash")
        state_root = exact_hash(header.get("state_root"), "legacy source latest state root")
        exact, exact_sha = request_json(origin, f"/block/{height}", 5.0)
        after, after_sha = request_json(origin, "/info", 5.0)
        coherent = (
            before.get("block_height") == height
            and after.get("block_height") == height
            and isinstance(exact.get("header"), dict)
            and exact["header"].get("height") == height
            and exact.get("hash") == block_hash
            and exact["header"].get("state_root") == state_root
        )
        if not coherent:
            return {
                **base,
                "outcome": "inconsistent",
                "height": None,
                "block_hash": None,
                "state_root": None,
                "response_sha256": {
                    "info_before": before_sha,
                    "latest": latest_sha,
                    "exact": exact_sha,
                    "info_after": after_sha,
                },
            }
        return {
            **base,
            "outcome": "observed",
            "height": height,
            "block_hash": block_hash,
            "state_root": state_root,
            "response_sha256": {
                "info_before": before_sha,
                "latest": latest_sha,
                "exact": exact_sha,
                "info_after": after_sha,
            },
        }
    except (InterlockError, OSError, TimeoutError, urllib.error.URLError, urllib.error.HTTPError):
        return {
            **base,
            "outcome": "unreachable",
            "height": None,
            "block_hash": None,
            "state_root": None,
            "response_sha256": None,
        }


def safe_state_root(path: Path) -> tuple[Path, Path]:
    if not path.is_absolute() or os.path.normpath(os.fspath(path)) != os.fspath(path):
        fail("interlock state root must be canonical absolute")
    if not path.exists():
        path.mkdir(mode=0o700)
    details = path.lstat()
    if (
        path.is_symlink()
        or not stat.S_ISDIR(details.st_mode)
        or details.st_uid not in {0, os.geteuid()}
        or stat.S_IMODE(details.st_mode) != 0o700
    ):
        fail("interlock state root identity differs")
    incidents = path / "incidents"
    if not incidents.exists():
        incidents.mkdir(mode=0o700)
    incident_details = incidents.lstat()
    if (
        incidents.is_symlink()
        or not stat.S_ISDIR(incident_details.st_mode)
        or incident_details.st_uid not in {0, os.geteuid()}
        or stat.S_IMODE(incident_details.st_mode) != 0o700
    ):
        fail("interlock incident directory identity differs")
    return path / "STATUS.json", incidents


def publish_status(path: Path, payload: bytes) -> None:
    parent_fd = os.open(
        path.parent,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    temporary = f".{path.name}.{os.getpid()}.{time.time_ns()}.partial"
    fd = -1
    try:
        fd = os.open(
            temporary,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
            0o600,
            dir_fd=parent_fd,
        )
        offset = 0
        while offset < len(payload):
            written = os.write(fd, payload[offset:])
            if written <= 0:
                fail("interlock status write made no progress")
            offset += written
        os.fsync(fd)
        os.fchmod(fd, 0o400)
        os.close(fd)
        fd = -1
        os.replace(temporary, path.name, src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
        os.fsync(parent_fd)
    finally:
        if fd >= 0:
            os.close(fd)
        with contextlib.suppress(FileNotFoundError):
            os.unlink(temporary, dir_fd=parent_fd)
        os.close(parent_fd)


def publish_create_only(path: Path, payload: bytes, label: str) -> None:
    if not path.is_absolute() or os.path.normpath(os.fspath(path)) != os.fspath(path):
        fail(f"{label} output path must be canonical absolute")
    parent = path.parent
    details = parent.lstat()
    if (
        parent.is_symlink()
        or not stat.S_ISDIR(details.st_mode)
        or details.st_uid not in {0, os.geteuid()}
        or details.st_mode & 0o022
    ):
        fail(f"{label} output parent is unsafe")
    parent_fd = os.open(
        parent,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    try:
        try:
            descriptor = os.open(
                path.name,
                os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
                0o400,
                dir_fd=parent_fd,
            )
        except FileExistsError:
            existing = read_locked(path, f"existing {label}", maximum=len(payload) + 1)
            if existing != payload:
                fail(f"existing {label} differs")
            return
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.fsync(parent_fd)
    finally:
        os.close(parent_fd)


def build_source_set(
    boundary_path: Path,
    boundary_sha: str,
    output: Path,
    tool_sha: str,
) -> tuple[dict[str, Any], str]:
    boundary_raw = read_locked(boundary_path, "legacy maintenance boundary")
    if digest(boundary_raw) != exact_hash(boundary_sha, "maintenance boundary sha256"):
        fail("legacy maintenance boundary bytes differ from the selected root")
    boundary = decode_canonical(boundary_raw, "legacy maintenance boundary")
    source_commit = boundary.get("source_main_commit")
    cutoff = boundary.get("observed_cutoff_height")
    if (
        boundary.get("schema") != "arc.recovery.legacy-maintenance-boundary.v1"
        or not isinstance(source_commit, str)
        or COMMIT_RE.fullmatch(source_commit) is None
    ):
        fail("legacy maintenance boundary identity differs")
    exact_uint(cutoff, "legacy maintenance observed cutoff", positive=True)
    official = [
        {"name": name, "host": host, "origin": origin}
        for name, host, origin in OFFICIAL
    ]
    if boundary.get("official_origin_scope") != {
        "global_absence_claimed": False,
        "origins": [
            {"node": name, "host": host, "origin": origin}
            for name, host, origin in OFFICIAL
        ],
    }:
        fail("legacy maintenance boundary official origin scope differs")
    value = {
        "schema": SOURCE_SET_SCHEMA,
        "source_main_commit": source_commit,
        "boundary_sha256": boundary_sha,
        "observed_cutoff_height": cutoff,
        "official_origins": official,
        "monitored_retired_origins": [
            {"name": name, "origin": origin} for name, _host, origin in OFFICIAL
        ],
        "monitored_community_origins": [],
        "poll_interval_seconds": 30,
        "max_staleness_seconds": 90,
        "validation_mode": (
            "capture-bound-retirement-tripwire-offline-validation-required"
        ),
        "validation_tool_sha256": tool_sha,
        "global_absence_claimed": False,
    }
    payload = canonical(value)
    source_sha = digest(payload)
    publish_create_only(output, payload, "legacy late-fork source set")
    publish_create_only(
        output.with_name(output.name + ".sha256"),
        f"{source_sha}  {output.name}\n".encode("ascii"),
        "legacy late-fork source-set sidecar",
    )
    return value, source_sha


def publish_incident(incidents: Path, value: dict[str, Any]) -> str:
    payload = canonical(value)
    incident_sha = digest(payload)
    path = incidents / f"{incident_sha}.json"
    if path.exists() or path.is_symlink():
        raw = read_locked(path, "existing late-fork incident")
        if raw != payload:
            fail("existing late-fork incident bytes differ")
        return incident_sha
    directory_fd = os.open(
        incidents,
        os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
    )
    try:
        fd = os.open(
            path.name,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0),
            0o400,
            dir_fd=directory_fd,
        )
        with os.fdopen(fd, "wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
    return incident_sha


def existing_incident(incidents: Path) -> str | None:
    rows = sorted(incidents.iterdir())
    for path in rows:
        if path.is_symlink() or not path.is_file() or re.fullmatch(r"[0-9a-f]{64}\.json", path.name) is None:
            fail("interlock incident directory contains an unsafe entry")
        raw = read_locked(path, "retained late-fork incident")
        value = exact_object(
            decode_canonical(raw, "retained late-fork incident"),
            {
                "schema",
                "source_main_commit",
                "boundary_sha256",
                "source_set_sha256",
                "observed_cutoff_height",
                "detected_at",
                "candidate",
                "global_absence_claimed",
            },
            "retained late-fork incident",
        )
        if value.get("schema") != INCIDENT_SCHEMA or value.get("global_absence_claimed") is not False:
            fail("retained late-fork incident policy differs")
        if digest(raw) != path.stem:
            fail("retained late-fork incident filename does not bind its bytes")
    return rows[0].stem if rows else None


def sample_once(
    source_set: Mapping[str, Any],
    source_set_sha: str,
    tool_sha: str,
    state_root: Path,
) -> dict[str, Any]:
    status_path, incidents = safe_state_root(state_root)
    observations = [
        observe_source(row, "retired")
        for row in source_set["monitored_retired_origins"]
    ] + [
        observe_source(row, "community")
        for row in source_set["monitored_community_origins"]
    ]
    cutoff = source_set["observed_cutoff_height"]
    incident_sha = existing_incident(incidents)
    candidates = [
        row
        for row in observations
        if row["outcome"] == "observed" and row["height"] > cutoff
    ]
    # A retired origin is required to stay unreachable after the capture-bound,
    # one-way service retirement. Any coherent or inconsistent response is a
    # resurrection/retirement-integrity incident, regardless of height.
    responding_retired = [
        row
        for row in observations
        if row["scope"] == "retired" and row["outcome"] != "unreachable"
    ]
    sampled_at = utc_now()
    if incident_sha is None and (candidates or responding_retired):
        candidate = sorted(
            candidates,
            key=lambda row: (-row["height"], row["scope"], row["name"]),
        )[0] if candidates else sorted(
            responding_retired,
            key=lambda row: (row["scope"], row["name"]),
        )[0]
        incident_sha = publish_incident(
            incidents,
            {
                "schema": INCIDENT_SCHEMA,
                "source_main_commit": source_set["source_main_commit"],
                "boundary_sha256": source_set["boundary_sha256"],
                "source_set_sha256": source_set_sha,
                "observed_cutoff_height": cutoff,
                "detected_at": utc_text(sampled_at),
                "candidate": candidate,
                "global_absence_claimed": False,
            },
        )
    expires_at = sampled_at + dt.timedelta(
        seconds=source_set["max_staleness_seconds"]
    )
    community = [row for row in observations if row["scope"] == "community"]
    community_observed = sum(row["outcome"] == "observed" for row in community)
    community_ready = community_observed == len(community)
    if incident_sha is not None:
        state = "MAINTENANCE"
        gate_reason = "latched-legacy-source-incident"
    elif not community_ready:
        # Community origins are declared as expected-reachable monitors.  A
        # missing or incoherent observation is transient maintenance, not a
        # permanent incident and never a claim that no external fork exists.
        state = "MAINTENANCE"
        gate_reason = "community-source-observation-unavailable"
    else:
        state = "HEALTHY"
        gate_reason = "capture-bound-retirement-tripwire-clear"
    value = {
        "schema": STATUS_SCHEMA,
        "source_main_commit": source_set["source_main_commit"],
        "boundary_sha256": source_set["boundary_sha256"],
        "source_set_sha256": source_set_sha,
        "tool_sha256": tool_sha,
        "sampled_at": utc_text(sampled_at),
        "expires_at": utc_text(expires_at),
        "poll_interval_seconds": source_set["poll_interval_seconds"],
        "max_staleness_seconds": source_set["max_staleness_seconds"],
        "observations": observations,
        "state": state,
        "gate_reason": gate_reason,
        "incident_sha256": incident_sha,
        "required_community_observations": len(community),
        "healthy_community_observations": community_observed,
        "global_absence_claimed": False,
    }
    publish_status(status_path, canonical(value))
    return value


def validate_status(
    raw: bytes,
    *,
    source_set: Mapping[str, Any],
    source_set_sha: str,
    tool_sha: str,
    now: dt.datetime | None = None,
) -> dict[str, Any]:
    status = exact_object(
        decode_canonical(raw, "legacy late-fork status"),
        {
            "schema",
            "source_main_commit",
            "boundary_sha256",
            "source_set_sha256",
            "tool_sha256",
            "sampled_at",
            "expires_at",
            "poll_interval_seconds",
            "max_staleness_seconds",
            "observations",
            "state",
            "gate_reason",
            "incident_sha256",
            "required_community_observations",
            "healthy_community_observations",
            "global_absence_claimed",
        },
        "legacy late-fork status",
    )
    expected = {
        "schema": STATUS_SCHEMA,
        "source_main_commit": source_set["source_main_commit"],
        "boundary_sha256": source_set["boundary_sha256"],
        "source_set_sha256": source_set_sha,
        "tool_sha256": tool_sha,
        "poll_interval_seconds": source_set["poll_interval_seconds"],
        "max_staleness_seconds": source_set["max_staleness_seconds"],
        "global_absence_claimed": False,
    }
    if any(status.get(field) != wanted for field, wanted in expected.items()):
        fail("legacy late-fork status identity/policy differs")
    sampled = parse_utc(status.get("sampled_at"), "late-fork status sampled_at")
    expires = parse_utc(status.get("expires_at"), "late-fork status expires_at")
    if expires - sampled != dt.timedelta(seconds=source_set["max_staleness_seconds"]):
        fail("legacy late-fork status expiry interval differs")
    if (now or utc_now()) > expires:
        fail("legacy late-fork status is expired")
    if status.get("state") not in {"HEALTHY", "MAINTENANCE"}:
        fail("legacy late-fork status state differs")
    observations = status.get("observations")
    if not isinstance(observations, list):
        fail("legacy late-fork status observations are not an array")
    expected_sources = [
        ("retired", row["name"], row["origin"])
        for row in source_set["monitored_retired_origins"]
    ] + [
        ("community", row["name"], row["origin"])
        for row in source_set["monitored_community_origins"]
    ]
    if len(observations) != len(expected_sources):
        fail("legacy late-fork status observation inventory differs")
    normalized: list[dict[str, Any]] = []
    observation_fields = {
        "name", "origin", "scope", "outcome", "height", "block_hash",
        "state_root", "response_sha256",
    }
    response_fields = {"info_before", "latest", "exact", "info_after"}
    for index, (raw_row, coordinate) in enumerate(zip(observations, expected_sources)):
        row = exact_object(raw_row, observation_fields, f"late-fork observation {index}")
        if (row["scope"], row["name"], row["origin"]) != coordinate:
            fail("legacy late-fork status observation coordinate differs")
        if row["outcome"] not in {"observed", "inconsistent", "unreachable"}:
            fail("legacy late-fork status observation outcome differs")
        if row["outcome"] == "observed":
            exact_uint(row["height"], "late-fork observed height", positive=True)
            exact_hash(row["block_hash"], "late-fork observed block hash")
            exact_hash(row["state_root"], "late-fork observed state root")
        elif any(row[field] is not None for field in ("height", "block_hash", "state_root")):
            fail("legacy late-fork unavailable observation carries a commitment")
        response = row["response_sha256"]
        if row["outcome"] == "unreachable":
            if response is not None:
                fail("legacy late-fork unreachable observation carries response hashes")
        else:
            response = exact_object(
                response, response_fields, f"late-fork observation {index} response hashes"
            )
            for field in sorted(response_fields):
                exact_hash(response[field], f"late-fork observation {index} {field}")
        normalized.append(row)

    community = [row for row in normalized if row["scope"] == "community"]
    community_observed = sum(row["outcome"] == "observed" for row in community)
    if (
        status.get("required_community_observations") != len(community)
        or status.get("healthy_community_observations") != community_observed
    ):
        fail("legacy late-fork status community observation counts differ")
    incident = status.get("incident_sha256")
    if incident is not None:
        exact_hash(incident, "legacy late-fork status incident sha256")
        expected_state = "MAINTENANCE"
        expected_reason = "latched-legacy-source-incident"
    elif community_observed != len(community):
        expected_state = "MAINTENANCE"
        expected_reason = "community-source-observation-unavailable"
    else:
        expected_state = "HEALTHY"
        expected_reason = "capture-bound-retirement-tripwire-clear"
    cutoff = source_set["observed_cutoff_height"]
    requires_incident = any(
        (row["outcome"] == "observed" and row["height"] > cutoff)
        or (row["scope"] == "retired" and row["outcome"] != "unreachable")
        for row in normalized
    )
    if requires_incident and incident is None:
        fail("legacy late-fork status omitted a required latched incident")
    if status["state"] != expected_state or status.get("gate_reason") != expected_reason:
        fail("legacy late-fork status gate reason/state binding differs")
    return status


if os.name == "posix":
    class ThreadedUnixServer(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
        daemon_threads = True
        allow_reuse_address = False


def prepare_unix_listener(path: Path) -> None:
    """Validate a permission-sealed RuntimeDirectory and remove only our stale socket."""

    raw = os.fspath(path)
    try:
        path.relative_to(UNIX_RUNTIME_ROOT)
        below_runtime_root = path != UNIX_RUNTIME_ROOT
    except ValueError:
        below_runtime_root = False
    if (
        os.name != "posix"
        or not path.is_absolute()
        or os.path.normpath(raw) != raw
        or not below_runtime_root
        or len(os.fsencode(raw)) > 100
        or path.name in {"", ".", ".."}
    ):
        fail("interlock Unix listener path is unsafe")
    parent = path.parent
    try:
        details = parent.lstat()
    except OSError as error:
        fail(f"interlock Unix listener parent is unavailable: {error}")
    if (
        parent.is_symlink()
        or not stat.S_ISDIR(details.st_mode)
        or details.st_uid != os.geteuid()
        or details.st_gid != os.getegid()
        or stat.S_IMODE(details.st_mode) != 0o750
    ):
        fail("interlock Unix listener parent identity differs")
    if not os.path.lexists(path):
        return
    stale = path.lstat()
    if (
        path.is_symlink()
        or not stat.S_ISSOCK(stale.st_mode)
        or stale.st_uid != os.geteuid()
        or stale.st_gid != os.getegid()
        or stale.st_nlink != 1
        or stat.S_IMODE(stale.st_mode) != 0o660
    ):
        fail("existing interlock Unix listener identity differs")
    probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        probe.settimeout(0.25)
        try:
            probe.connect(raw)
        except (ConnectionRefusedError, FileNotFoundError):
            pass
        except OSError as error:
            fail(f"cannot prove existing interlock Unix listener is stale: {error}")
        else:
            fail("existing interlock Unix listener is still accepting connections")
    finally:
        probe.close()
    # The parent is writable only by this process identity, so no gate-group
    # peer can exchange the inode between lstat and this exact-name unlink.
    path.unlink()


def verify_unix_listener(path: Path) -> None:
    details = path.lstat()
    if (
        path.is_symlink()
        or not stat.S_ISSOCK(details.st_mode)
        or details.st_uid != os.geteuid()
        or details.st_gid != os.getegid()
        or details.st_nlink != 1
        or stat.S_IMODE(details.st_mode) != 0o660
    ):
        fail("live interlock Unix listener identity differs")


def created_unix_listener_identity(path: Path) -> tuple[int, int, int, int]:
    details = path.lstat()
    if (
        path.is_symlink()
        or not stat.S_ISSOCK(details.st_mode)
        or details.st_uid != os.geteuid()
        or details.st_gid != os.getegid()
        or details.st_nlink != 1
    ):
        fail("new interlock Unix listener identity differs")
    return (details.st_dev, details.st_ino, details.st_uid, details.st_gid)


def remove_unix_listener(
    path: Path, expected_identity: tuple[int, int, int, int] | None = None
) -> None:
    if not os.path.lexists(path):
        return
    if expected_identity is None:
        verify_unix_listener(path)
    else:
        details = path.lstat()
        observed = (details.st_dev, details.st_ino, details.st_uid, details.st_gid)
        if (
            path.is_symlink()
            or not stat.S_ISSOCK(details.st_mode)
            or details.st_nlink != 1
            or observed != expected_identity
        ):
            fail("interlock Unix listener changed before exact-inode cleanup")
    path.unlink()


def serve(
    *,
    listen_unix: Path,
    source_set: dict[str, Any],
    source_set_sha: str,
    tool_sha: str,
    state_root: Path,
) -> None:
    prepare_unix_listener(listen_unix)
    status_path, _incidents = safe_state_root(state_root)
    stop = threading.Event()

    def poll() -> None:
        while not stop.is_set():
            try:
                sample_once(source_set, source_set_sha, tool_sha, state_root)
            except Exception:
                # Do not refresh the prior receipt. The gate fails closed when
                # it expires; retaining the last exact bytes aids diagnosis.
                pass
            stop.wait(source_set["poll_interval_seconds"])

    class Handler(http.server.BaseHTTPRequestHandler):
        server_version = ""
        sys_version = ""

        def log_message(self, _format: str, *_args: Any) -> None:
            return

        def do_GET(self) -> None:
            if self.path not in {"/gate", "/maintenance/status"}:
                self.send_error(404)
                return
            try:
                raw = read_locked(status_path, "live legacy late-fork status")
                status = validate_status(
                    raw,
                    source_set=source_set,
                    source_set_sha=source_set_sha,
                    tool_sha=tool_sha,
                )
                healthy = status["state"] == "HEALTHY"
            except Exception:
                raw = canonical(
                    {
                        "schema": "arc.recovery.publication-interlock-unavailable.v1",
                        "state": "MAINTENANCE",
                    }
                )
                healthy = False
            if self.path == "/gate" and healthy:
                self.send_response(204)
                self.send_header("Cache-Control", "no-store")
                self.end_headers()
                return
            self.send_response(200 if self.path == "/maintenance/status" else 503)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

    worker = threading.Thread(target=poll, name="arc-late-fork-poll", daemon=True)
    worker.start()
    server: ThreadedUnixServer | None = None
    listener_identity: tuple[int, int, int, int] | None = None
    try:
        prior_umask = os.umask(0o007)
        try:
            server = ThreadedUnixServer(
                os.fspath(listen_unix), Handler, bind_and_activate=False
            )
            server.server_bind()
        finally:
            os.umask(prior_umask)
        listener_identity = created_unix_listener_identity(listen_unix)
        os.chmod(listen_unix, 0o660)
        verify_unix_listener(listen_unix)
        server.server_activate()
        server.serve_forever(poll_interval=0.5)
    finally:
        if server is not None:
            server.server_close()
        stop.set()
        worker.join(timeout=5)
        if listener_identity is None and os.path.lexists(listen_unix):
            # bind_and_activate=False ensures this can only be a socket created
            # by server_bind after prepare_unix_listener removed the old name.
            listener_identity = created_unix_listener_identity(listen_unix)
        if listener_identity is not None:
            remove_unix_listener(listen_unix, listener_identity)


def parser() -> argparse.ArgumentParser:
    # Security-sensitive listener flags must be exact.  In particular,
    # argparse's default prefix matching would otherwise accept the removed
    # ``--listen`` TCP flag as an abbreviation for ``--listen-unix``.
    result = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    commands = result.add_subparsers(dest="command", required=True)
    build = commands.add_parser("build-source-set", allow_abbrev=False)
    build.add_argument("--boundary", required=True, type=Path)
    build.add_argument("--boundary-sha256", required=True)
    build.add_argument("--output", required=True, type=Path)
    for name in ("run-once", "serve"):
        command = commands.add_parser(name, allow_abbrev=False)
        command.add_argument("--source-set", required=True, type=Path)
        command.add_argument("--source-set-sha256", required=True)
        command.add_argument("--boundary-sha256", required=True)
        command.add_argument("--tool-sha256", required=True)
        command.add_argument("--state-root", required=True, type=Path)
        if name == "serve":
            command.add_argument("--listen-unix", required=True, type=Path)
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        tool_path = Path(__file__).resolve()
        tool_raw = tool_path.read_bytes()
        tool_sha = digest(tool_raw)
        if args.command == "build-source-set":
            _value, source_sha = build_source_set(
                args.boundary,
                args.boundary_sha256,
                args.output,
                tool_sha,
            )
            print(
                json.dumps(
                    {
                        "schema": "arc.recovery.legacy-late-fork-source-set-build.v1",
                        "source_set_sha256": source_sha,
                        "output": os.fspath(args.output),
                    },
                    sort_keys=True,
                    separators=(",", ":"),
                )
            )
            return 0
        if tool_sha != exact_hash(args.tool_sha256, "selected interlock tool sha256"):
            fail("running interlock tool differs from the selected protected bytes")
        source_set, source_set_sha = load_source_set(
            args.source_set,
            expected_sha256=args.source_set_sha256,
            expected_boundary_sha256=args.boundary_sha256,
            expected_tool_sha256=tool_sha,
        )
        if args.command == "run-once":
            value = sample_once(source_set, source_set_sha, tool_sha, args.state_root)
            sys.stdout.buffer.write(canonical(value))
        else:
            serve(
                listen_unix=args.listen_unix,
                source_set=source_set,
                source_set_sha=source_set_sha,
                tool_sha=tool_sha,
                state_root=args.state_root,
            )
        return 0
    except InterlockError as error:
        print(f"legacy late-fork interlock: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
