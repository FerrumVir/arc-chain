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
import stat as stat_module
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

RPC = "http://127.0.0.1:9944"
SNAPSHOT_TOOL = os.environ.get("ARC_W0_SNAPSHOT_TOOL", "/opt/arc-w0/tests/legacy-bridge/snapshot_tree.py")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
# An unfinished DOWNLOAD (the launcher's resume file is `<asset>.partial`). The node's own atomic-write temp files (`.tmp`,
# `.new`) exist for milliseconds and are not downloads, so they are not counted.
PARTIAL_SUFFIXES = (".partial", ".part", ".download", ".crdownload")


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


# ---- resource series (ARC-83 criteria E and D): RSS, memory, swap, disk, data/cache/log bytes, registration continuity ----

def parse_kb(text: str | None, name: str) -> int | None:
    """`Name:   12345 kB` from /proc/<pid>/status or /proc/meminfo."""
    if not text:
        return None
    match = re.search(r"^" + re.escape(name) + r":\s+(\d+)\s*kB\s*$", text, re.MULTILINE)
    return int(match.group(1)) if match else None


def read_int_file(path: str) -> int | None:
    text = read_text(path)
    try:
        return int(text.strip()) if text and text.strip().isdigit() else None
    except ValueError:
        return None


def cgroup_dir(pid: int) -> str | None:
    """cgroup v2 directory of a process (the line `0::/system.slice/arc-node.service`)."""
    text = read_text(f"/proc/{pid}/cgroup")
    if not text:
        return None
    for line in text.splitlines():
        if line.startswith("0::"):
            return "/sys/fs/cgroup" + line[3:]
    return None


def cpu_seconds(pid: int) -> float | None:
    stat = read_text(f"/proc/{pid}/stat")
    if not stat:
        return None
    try:
        fields = stat.rsplit(")", 1)[1].split()
        return round((int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK"), 3)
    except (IndexError, ValueError):
        return None


def count_fds(pid: int) -> int | None:
    try:
        return len(os.listdir(f"/proc/{pid}/fd"))
    except OSError:
        return None


def tree_stats(arc_dir: str) -> dict:
    """One walk of the ARC directory: bytes per category, the largest file, partial files. Never raises."""
    stats = {"arc_dir_bytes": 0, "largest_file_bytes": 0, "legacy_data_bytes": 0, "node_data_bytes": 0,
             "release_cache_bytes": 0, "release_cache_files": 0, "models_bytes": 0, "partial_files": 0}
    if not os.path.isdir(arc_dir):
        return {key: None for key in stats}
    for current, _dirs, files in os.walk(arc_dir, followlinks=False):
        relative_dir = os.path.relpath(current, arc_dir)
        for name in files:
            try:
                info = os.lstat(os.path.join(current, name))
            except OSError:
                continue
            if not stat_module.S_ISREG(info.st_mode):
                continue
            size = info.st_size
            parts = ([] if relative_dir == "." else relative_dir.split(os.sep)) + [name]
            stats["arc_dir_bytes"] += size
            stats["largest_file_bytes"] = max(stats["largest_file_bytes"], size)
            if parts[0] == "data":
                stats["legacy_data_bytes"] += size
            if parts[:2] == ["legacy-bridge", "nodes"]:
                stats["node_data_bytes"] += size
            if parts[:2] == ["legacy-bridge", "releases"]:
                stats["release_cache_bytes"] += size
                stats["release_cache_files"] += 1
            if "models" in parts[:-1] or name.endswith(".gguf"):
                stats["models_bytes"] += size
            if name.endswith(PARTIAL_SUFFIXES):
                stats["partial_files"] += 1
    return stats


def file_size(path: str) -> int:
    try:
        return os.stat(path).st_size
    except OSError:
        return 0


def bridge_download_lines(arc_dir: str) -> int | None:
    text = read_text(os.path.join(arc_dir, "legacy-bridge", "bridge.log"))
    if text is None:
        return None
    return sum(1 for line in text.splitlines() if "downloaded and verified" in line or re.search(r"download of .+ failed", line))


def resource_fields(arc_dir: str, pid: int) -> dict:
    """Every resource field of a sample; None where unavailable. Never raises."""
    fields: dict = {key: None for key in (
        "node_rss_kb", "node_hwm_kb", "node_swap_kb", "node_threads", "node_fds", "node_cpu_s",
        "mem_total_kb", "mem_available_kb", "swap_total_kb", "swap_free_kb",
        "cg_mem_current_b", "cg_mem_peak_b", "cg_swap_current_b",
        "disk_total_b", "disk_free_b", "log_bytes", "bridge_downloads",
    )}
    try:
        meminfo = read_text("/proc/meminfo")
        for key, name in (("mem_total_kb", "MemTotal"), ("mem_available_kb", "MemAvailable"), ("swap_total_kb", "SwapTotal"), ("swap_free_kb", "SwapFree")):
            fields[key] = parse_kb(meminfo, name)
        if pid:
            status = read_text(f"/proc/{pid}/status")
            fields["node_rss_kb"] = parse_kb(status, "VmRSS")
            fields["node_hwm_kb"] = parse_kb(status, "VmHWM")
            fields["node_swap_kb"] = parse_kb(status, "VmSwap")
            threads = re.search(r"^Threads:\s+(\d+)", status or "", re.MULTILINE)
            fields["node_threads"] = int(threads.group(1)) if threads else None
            fields["node_fds"] = count_fds(pid)
            fields["node_cpu_s"] = cpu_seconds(pid)
            group = cgroup_dir(pid)
            if group:
                fields["cg_mem_current_b"] = read_int_file(os.path.join(group, "memory.current"))
                fields["cg_mem_peak_b"] = read_int_file(os.path.join(group, "memory.peak"))
                fields["cg_swap_current_b"] = read_int_file(os.path.join(group, "memory.swap.current"))
        try:
            usage = os.statvfs("/")
            fields["disk_total_b"] = usage.f_frsize * usage.f_blocks
            fields["disk_free_b"] = usage.f_frsize * usage.f_bavail
        except OSError:
            pass
        fields["log_bytes"] = (
            file_size(os.path.join(arc_dir, "node.log"))
            + file_size(os.path.join(arc_dir, "legacy-bridge", "bridge.log"))
            + file_size(os.path.join(arc_dir, "auto-update.log"))
        )
        fields["bridge_downloads"] = bridge_download_lines(arc_dir)
    except Exception:  # noqa: BLE001 - a probe must never raise
        pass
    fields.update(tree_stats(arc_dir))
    return fields


def sample_once(arc_dir: str, seq: int, before_snapshot_json: str | None = None, compare: bool = False) -> dict:
    """One sample with exactly the fields the evaluator reads. Never raises."""
    errors: list[str] = []
    now = time.time()
    # The node's local registration status is read FIRST, so registration_age_s = epoch - last_registration_unix_ms/1000 is
    # measured within milliseconds of the sample epoch (a heartbeat landing between the epoch and the read would make it negative).
    _, community = http_json("/community/worker/status")
    community = community if isinstance(community, dict) else {}
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
    sample["public_name"] = community.get("public_name")
    sample["coordinators_total"] = community.get("coordinators_total")
    sample["coordinators_registered"] = community.get("coordinators_registered")
    registration_ms = community.get("last_registration_unix_ms")
    sample["last_registration_unix_ms"] = registration_ms if isinstance(registration_ms, int) and not isinstance(registration_ms, bool) else None
    # AGE of the last SUCCESSFUL registration/heartbeat round at the moment of this sample (ARC-83 D): null when the node reports none.
    sample["registration_age_s"] = round(now - sample["last_registration_unix_ms"] / 1000.0, 3) if sample["last_registration_unix_ms"] is not None else None
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
    sample.update(resource_fields(arc_dir, pid))
    sample["errors"] = errors
    return sample
