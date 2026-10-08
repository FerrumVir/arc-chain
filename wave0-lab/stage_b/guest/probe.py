#!/usr/bin/env python3
"""Read-only probes of the v0.7 install and the bridged v0.8 node (runs inside the Wave 0 guest VM).

THROWAWAY LAB FILE. Nothing here starts, stops, restarts or changes anything: it reads /proc,
systemd properties, the node's local HTTP API and the bridge's state files. The sampler, the
state capture and the invariants collector all use these functions so they cannot disagree.
"""
from __future__ import annotations

import glob
import hashlib
import importlib.util
import json
import os
import re
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

RPC = "http://127.0.0.1:9944"
SNAPSHOT_TOOL = os.environ.get("ARC_W0_SNAPSHOT_TOOL", "/opt/arc-w0/tests/legacy-bridge/snapshot_tree.py")
HEX64 = re.compile(r"^[0-9a-f]{64}$")


def read_text(path: str) -> str | None:
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            return handle.read()
    except OSError:
        return None


def run(args: list[str], timeout: float = 8.0) -> tuple[int, str]:
    try:
        done = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout, check=False)
        return done.returncode, done.stdout.decode("utf-8", "replace")
    except (OSError, subprocess.SubprocessError) as error:
        return 255, f"{type(error).__name__}: {error}"


def systemd_props(unit: str, names: list[str]) -> dict[str, str]:
    args = ["systemctl", "show", unit]
    for name in names:
        args += ["-p", name]
    code, out = run(args)
    props: dict[str, str] = {}
    if code == 0:
        for line in out.splitlines():
            key, _, value = line.partition("=")
            props[key] = value
    return props


def http_json(path: str, timeout: float = 3.0) -> tuple[bool, object | None]:
    """(reachable with HTTP 200, parsed JSON or None)."""
    try:
        with urllib.request.urlopen(RPC + path, timeout=timeout) as response:
            body = response.read(1 << 20)
            if response.status != 200:
                return False, None
    except (urllib.error.URLError, OSError, ValueError):
        return False, None
    try:
        return True, json.loads(body.decode("utf-8", "replace"))
    except ValueError:
        return True, None


_HASH_CACHE: dict[tuple[str, int, int], str] = {}


def sha256_file(path: str) -> str | None:
    try:
        info = os.stat(path)
        key = (path, info.st_size, info.st_mtime_ns)
        cached = _HASH_CACHE.get(key)
        if cached:
            return cached
        digest = hashlib.sha256()
        with open(path, "rb") as handle:
            for chunk in iter(lambda: handle.read(1 << 20), b""):
                digest.update(chunk)
        _HASH_CACHE[key] = digest.hexdigest()
        return _HASH_CACHE[key]
    except OSError:
        return None


def boot_id() -> str:
    return (read_text("/proc/sys/kernel/random/boot_id") or "").strip()


def uptime_s() -> float | None:
    text = read_text("/proc/uptime")
    try:
        return float(text.split()[0]) if text else None
    except (ValueError, IndexError):
        return None


def proc_start_epoch(pid: int) -> int | None:
    stat = read_text(f"/proc/{pid}/stat")
    btime = None
    for line in (read_text("/proc/stat") or "").splitlines():
        if line.startswith("btime "):
            btime = int(line.split()[1])
    if not stat or btime is None:
        return None
    try:
        fields = stat.rsplit(")", 1)[1].split()
        return int(btime + int(fields[19]) / os.sysconf("SC_CLK_TCK"))
    except (IndexError, ValueError):
        return None


def exe_of(pid: int) -> str | None:
    try:
        return os.readlink(f"/proc/{pid}/exe").removesuffix(" (deleted)")
    except OSError:
        return None


def node_processes(arc_dir: str) -> list[tuple[int, str]]:
    """Every process whose executable is a node or the launcher under the ARC directory."""
    found: list[tuple[int, str]] = []
    prefix = arc_dir.rstrip("/") + "/"
    try:
        entries = os.listdir("/proc")
    except OSError:
        return found
    for entry in entries:
        if not entry.isdigit():
            continue
        exe = exe_of(int(entry))
        if exe is None or not exe.startswith(prefix):
            continue
        base = os.path.basename(exe)
        if base.startswith("arc-node") and not base.endswith(".prev"):
            found.append((int(entry), exe))
    return found


def legacy_fingerprint(root: str) -> str | None:
    """Cheap, stat-only fingerprint of the v0.7 data directory (the byte-level compare is authoritative)."""
    if not os.path.isdir(root):
        return None
    digest = hashlib.sha256()
    for current, dirnames, filenames in os.walk(root, followlinks=False):
        dirnames.sort()
        for name in sorted(dirnames) + sorted(filenames):
            path = os.path.join(current, name)
            try:
                info = os.lstat(path)
            except OSError:
                continue
            digest.update(f"{os.path.relpath(path, root)}\0{info.st_mode}\0{info.st_size}\0{info.st_mtime_ns}\n".encode())
    return digest.hexdigest()


def load_snapshot_tool():
    spec = importlib.util.spec_from_file_location("snapshot_tree", SNAPSHOT_TOOL)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def snapshot_entries(root: str) -> dict:
    return load_snapshot_tool().snapshot(Path(root), False)


def entries_digest(entries: dict) -> str:
    return hashlib.sha256(json.dumps(entries, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def byte_compare(before_snapshot_json: str, root: str) -> str | None:
    """'same' or 'diff' against the hook snapshot taken at the start of the kept bridge run."""
    try:
        with open(before_snapshot_json, encoding="utf-8") as handle:
            before = json.load(handle)["entries"]
    except (OSError, ValueError, KeyError):
        return None
    try:
        return "same" if snapshot_entries(root) == before else "diff"
    except Exception:  # noqa: BLE001 - a probe must never raise
        return None


def bridge_node_dirs(arc_dir: str) -> list[str]:
    return sorted(glob.glob(os.path.join(arc_dir, "legacy-bridge", "nodes", "headless-*")))


def bridge_state(arc_dir: str) -> tuple[dict | None, str | None]:
    dirs = bridge_node_dirs(arc_dir)
    if not dirs:
        return None, None
    node_dir = max(dirs, key=lambda d: os.path.getmtime(d))
    text = read_text(os.path.join(node_dir, "bridge-state.json"))
    try:
        return (json.loads(text) if text else None), node_dir
    except ValueError:
        return None, node_dir


def sample_once(arc_dir: str, seq: int, before_snapshot_json: str | None = None, compare: bool = False) -> dict:
    """One sample with exactly the fields the evaluator reads. Never raises."""
    errors: list[str] = []
    now = time.time()
    unit = systemd_props("arc-node", ["ActiveState", "MainPID", "NRestarts"])
    if not unit:
        errors.append("systemctl show arc-node failed")
    pid = int(unit.get("MainPID") or 0) if (unit.get("MainPID") or "0").isdigit() else 0
    restarts = unit.get("NRestarts")
    sample: dict = {
        "seq": seq,
        "epoch": round(now, 3),
        "boot_id": boot_id(),
        "uptime_s": uptime_s(),
        "node_state": unit.get("ActiveState") or None,
        "main_pid": pid,
        "proc_start_epoch": proc_start_epoch(pid) if pid else None,
        "node_exe": exe_of(pid) if pid else None,
        "node_exe_sha256": None,
        "node_procs": len(node_processes(arc_dir)),
        "n_restarts": int(restarts) if restarts and restarts.isdigit() else None,
    }
    if sample["node_exe"]:
        sample["node_exe_sha256"] = sha256_file(f"/proc/{pid}/exe")
    health_ok, health = http_json("/health")
    info_ok, info = http_json("/node/info")
    sample["health_ok"] = bool(health_ok)
    sample["chain_participation_enabled"] = health.get("chain_participation_enabled") if isinstance(health, dict) else None
    sample["info_ok"] = bool(info_ok and isinstance(info, dict))
    validator = info.get("validator") if isinstance(info, dict) else None
    sample["address"] = str(validator).lower().removeprefix("0x") if isinstance(validator, str) else None
    sample["stake"] = info.get("stake") if isinstance(info, dict) else None
    sample["node_version"] = info.get("version") if isinstance(info, dict) else None
    _, community = http_json("/community/worker/status")
    community = community if isinstance(community, dict) else {}
    sample["public_name"] = community.get("public_name")
    sample["coordinators_total"] = community.get("coordinators_total")
    sample["coordinators_registered"] = community.get("coordinators_registered")
    state, node_dir = bridge_state(arc_dir)
    state = state or {}
    sample["bridge_node_address"] = state.get("node_address")
    sample["bridge_compute"] = state.get("compute")
    sample["community_registration"] = state.get("community_registration")
    consent = None
    if node_dir:
        text = read_text(os.path.join(node_dir, "compute-consent"))
        consent = text.strip() if text is not None else "absent"
    sample["compute_consent"] = consent
    version = read_text(os.path.join(arc_dir, "version.txt"))
    sample["version_txt"] = version.strip() if version is not None else None
    sample["launcher_sha256"] = sha256_file(os.path.join(arc_dir, "bin", "arc-node"))
    code, active = run(["systemctl", "is-active", "arc-updater.timer"])
    sample["updater_timer_active"] = active.strip() == "active"
    sample["legacy_fingerprint"] = legacy_fingerprint(os.path.join(arc_dir, "data"))
    bridged = bool(sample["node_exe"] and "/legacy-bridge/releases/" in sample["node_exe"])
    sample["legacy_byte_compare"] = byte_compare(before_snapshot_json, os.path.join(arc_dir, "data")) if (compare and bridged and before_snapshot_json) else None
    sample["errors"] = errors
    return sample
