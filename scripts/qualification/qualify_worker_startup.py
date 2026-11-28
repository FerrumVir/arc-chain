#!/usr/bin/env python3
from __future__ import annotations

import argparse
from collections import deque
from dataclasses import dataclass
import datetime
import errno
import hashlib
import http.client
import http.server
import ipaddress
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import socket
import stat
import subprocess
import sys
import threading
import time
from typing import Any, Callable
from urllib.parse import urlparse

try:
    import tomllib
except ModuleNotFoundError:
    tomllib = None


GIB_BYTES = 1024 ** 3
MIN_NOMINAL_RAM_GB = 16
MIN_AVAILABLE_RAM_BYTES = 12 * GIB_BYTES
MIN_FREE_DISK_BYTES = 10 * GIB_BYTES
MARKETED_RAM_SIZES_GB = (
    2,
    3,
    4,
    6,
    8,
    12,
    16,
    18,
    20,
    24,
    32,
    36,
    48,
    64,
    96,
    128,
    192,
    256,
    384,
    512,
    768,
    1024,
    1536,
    2048,
)
MAX_RESERVED_RAM_PERCENT = 15

CANONICAL_EXECUTION_PROFILE = "INT8 integer (per-row, cross-platform deterministic)"
EXACT_READINESS_KEYS = frozenset(
    {
        "schema",
        "safe_to_dispatch",
        "community_dispatch_ready",
        "local_model_ready",
        "sharded_pipeline_ready",
        "live_community_workers",
        "model_id",
        "required_community_execution_profile",
        "mutation_free_observation",
    }
)
WORKER_STATUS_PATH = "/community/worker/status"
WORKER_STATUS_SCHEMA = "arc.community.worker-status.v1"
FAKE_COORDINATOR_ADDRESS_HEX = "aa" * 32
EXPECTED_PRODUCTION_IPV4_COUNT = 6
MAX_LOADING_HEALTH_SECS = 30.0
MAX_HTTP_RESPONSE_BYTES = 131072
MAX_COORDINATOR_REQUEST_BODY_BYTES = 65536
MAX_CAPTURED_COORDINATOR_REQUESTS = 200
DEFAULT_COORDINATOR_REQUEST_TIMEOUT_SECS = 5.0
MAX_EVIDENCE_LOG_BYTES = 262144
CLEANUP_FAILURES = (OSError, RuntimeError, ValueError, subprocess.SubprocessError)
MAX_RUNTIME_LOG_BYTES = 16 * 1024 * 1024
MAX_SINGLE_LINE_BYTES = 16384
MAX_DRAIN_BYTES_PER_CALL = 262144
TRUNCATION_MARKER = b"\n--- TRUNCATED MIDDLE BY QUALIFICATION HARNESS ---\n"

MODEL_PREPARED_SOURCE_FORMAT = (
    "Model loaded in {:.1}s - {} layers held / {} total, {} MB shard weights, vocab {}"
)
MODEL_PREPARED_LOG_RE = re.compile(
    r"Model loaded in (\d+\.\d+)s - (\d+) layers held / (\d+) total, (\d+) MB shard weights, vocab (\d+)"
)

RUN1_REQUIRED_LOG_MARKERS = (
    "Early worker startup RPC listening in loading state before canonical model preparation",
    "Loading model from",
    "SIGTERM received",
    "shutdown requested during initialization; persistent state is durable and later runtime stages were skipped",
)
RUN1_FORBIDDEN_LOG_MARKERS = (
    "Model loaded in ",
    "Community-mode HTTP registration started",
)
RUN2_REQUIRED_LOG_MARKERS = (
    "Early worker startup RPC listening in loading state before canonical model preparation",
    "Model loaded in ",
    "Community-mode HTTP registration started",
    "RPC handlers drained, node writers joined, WAL durability barrier completed",
)
ALL_TRACKED_LOG_MARKERS = tuple(
    dict.fromkeys(
        RUN1_REQUIRED_LOG_MARKERS + RUN1_FORBIDDEN_LOG_MARKERS + RUN2_REQUIRED_LOG_MARKERS
    )
)

HEX64_RE = re.compile(r"^[0-9a-f]{64}$")
PREFIXED_HEX64_RE = re.compile(r"^0x[0-9a-f]{64}$")
IPV4_RE = re.compile(r"(?<![0-9.])(?:\d{1,3}\.){3}\d{1,3}(?![0-9.])")
SEMVER_RE = re.compile(r"^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$")


class QualificationError(RuntimeError):
    pass


class QualificationInterrupted(BaseException):
    def __init__(self, signum: int) -> None:
        self.signum = signum
        try:
            sig_name = signal.Signals(signum).name
        except ValueError:
            sig_name = f"SIG{signum}"
        self.signal_name = sig_name
        super().__init__(f"qualification interrupted by {sig_name} ({signum})")


class TerminationSignalGuard:
    def __init__(
        self,
        signals: tuple[int, ...] = (signal.SIGTERM, signal.SIGINT),
    ) -> None:
        self._signals = signals
        self._previous: dict[int, Any] = {}
        self.received_signal: int | None = None
        self.received_signal_name: str | None = None
        self.additional_signals: list[int] = []

    def _handle_signal(self, signum: int, _frame: Any) -> None:
        if self.received_signal is None:
            self.received_signal = signum
            try:
                self.received_signal_name = signal.Signals(signum).name
            except ValueError:
                self.received_signal_name = f"SIG{signum}"
            raise QualificationInterrupted(signum)
        self.additional_signals.append(signum)

    def __enter__(self) -> TerminationSignalGuard:
        if threading.current_thread() is not threading.main_thread():
            raise QualificationError(
                "TerminationSignalGuard must be installed on the main thread"
            )
        installed: list[int] = []
        try:
            for sig in self._signals:
                prev = signal.signal(sig, self._handle_signal)
                self._previous[sig] = prev
                installed.append(sig)
        except (OSError, RuntimeError, ValueError, TypeError):
            for sig in installed:
                signal.signal(sig, self._previous[sig])
            self._previous.clear()
            raise
        return self

    def __exit__(self, _exc_type: Any, _exc: Any, _tb: Any) -> bool:
        for sig, prev in list(self._previous.items()):
            signal.signal(sig, prev)
        self._previous.clear()
        return False


@dataclass(frozen=True)
class RepoPins:
    candidate_version: str
    model_file_name: str
    model_size: int
    model_sha256: str
    model_blake3: str
    model_url: str
    production_ipv4s: tuple[str, ...]
    canonical_execution_profile: str


@dataclass(frozen=True)
class ResourceSnapshot:
    mem_total_bytes: int
    mem_available_bytes: int
    cgroup_limit_bytes: int | None
    cgroup_available_bytes: int | None
    effective_total_bytes: int
    effective_available_bytes: int
    nominal_ram_gb: int
    disk_path: str
    disk_total_bytes: int
    disk_free_bytes: int

    def to_dict(self) -> dict[str, Any]:
        return {
            "mem_total_bytes": self.mem_total_bytes,
            "mem_available_bytes": self.mem_available_bytes,
            "cgroup_limit_bytes": self.cgroup_limit_bytes,
            "cgroup_available_bytes": self.cgroup_available_bytes,
            "effective_total_bytes": self.effective_total_bytes,
            "effective_available_bytes": self.effective_available_bytes,
            "nominal_ram_gb": self.nominal_ram_gb,
            "disk_path": self.disk_path,
            "disk_total_bytes": self.disk_total_bytes,
            "disk_free_bytes": self.disk_free_bytes,
        }


@dataclass(frozen=True)
class HttpResult:
    status: int
    headers: dict[str, str]
    raw_body: bytes
    json_body: Any


@dataclass(frozen=True)
class PersistentIdentity:
    keyfile_path: Path
    address_hex: str
    public_key_hex: str
    secret_key_hex: str


def utc_now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def is_hex64(value: Any) -> bool:
    return isinstance(value, str) and HEX64_RE.fullmatch(value) is not None


def normalize_bare_hex64(value: Any, label: str) -> str:
    if not isinstance(value, str) or HEX64_RE.fullmatch(value) is None:
        raise QualificationError(
            f"{label} must be 64 lowercase hexadecimal characters without 0x prefix, got {value!r}"
        )
    return value


def normalize_prefixed_hex64(value: Any, label: str) -> str:
    if not isinstance(value, str) or PREFIXED_HEX64_RE.fullmatch(value) is None:
        raise QualificationError(
            f"{label} must be canonical 0x-prefixed 64 lowercase hexadecimal characters, got {value!r}"
        )
    return value[2:]


def parse_model_prepared_line(line: str) -> dict[str, Any] | None:
    match = MODEL_PREPARED_LOG_RE.search(line)
    if match is None:
        return None
    elapsed_logged = float(match.group(1))
    layers_held = int(match.group(2))
    total_layers = int(match.group(3))
    shard_weights_mb = int(match.group(4))
    vocab_size = int(match.group(5))
    if layers_held <= 0 or layers_held != total_layers:
        return None
    return {
        "elapsed_secs_logged": elapsed_logged,
        "layers_held": layers_held,
        "total_layers": total_layers,
        "shard_weights_mb": shard_weights_mb,
        "vocab_size": vocab_size,
        "line": line[:512],
    }


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(4 * 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def ensure_regular_non_symlink(path: Path, label: str) -> os.stat_result:
    try:
        info = os.lstat(path)
    except FileNotFoundError as error:
        raise QualificationError(f"{label} does not exist: {path}") from error
    if stat.S_ISLNK(info.st_mode):
        raise QualificationError(f"{label} must not be a symlink: {path}")
    if not stat.S_ISREG(info.st_mode):
        raise QualificationError(f"{label} must be a regular file: {path}")
    return info


def parse_workspace_version(cargo_toml_path: Path) -> str:
    raw = cargo_toml_path.read_text(encoding="utf-8")
    if tomllib is not None:
        parsed = tomllib.loads(raw)
        version = parsed.get("workspace", {}).get("package", {}).get("version")
    else:
        in_section = False
        version = None
        for line in raw.splitlines():
            stripped = line.strip()
            if stripped.startswith("[") and stripped.endswith("]"):
                in_section = stripped == "[workspace.package]"
                continue
            if in_section and stripped.startswith("version"):
                match = re.match(r'^version\s*=\s*"([^"]+)"\s*$', stripped)
                if match:
                    version = match.group(1)
                    break
    if not isinstance(version, str) or not SEMVER_RE.fullmatch(version):
        raise QualificationError(
            f"invalid or missing [workspace.package].version in {cargo_toml_path}"
        )
    return version


def extract_non_loopback_ipv4s(text: str) -> set[str]:
    found: set[str] = set()
    for token in IPV4_RE.findall(text):
        try:
            address = ipaddress.IPv4Address(token)
        except ValueError:
            continue
        if address.is_loopback or address.is_unspecified:
            continue
        found.add(str(address))
    return found


def load_repo_pins(repo_root: Path) -> RepoPins:
    cargo_toml = repo_root / "Cargo.toml"
    manifest_path = repo_root / "docs" / "protocol" / "packages" / "llama-2-7b-q4km.manifest.json"
    active_pins_path = repo_root / "crates" / "arc-legacy-bridge" / "pins" / "active.json"
    genesis_path = repo_root / "genesis.toml"
    seeds_path = repo_root / "testnet-seeds.txt"

    for required, label in (
        (cargo_toml, "Cargo.toml"),
        (manifest_path, "model package manifest"),
        (active_pins_path, "active release pins"),
        (genesis_path, "genesis.toml"),
        (seeds_path, "testnet-seeds.txt"),
    ):
        ensure_regular_non_symlink(required, label)

    candidate_version = parse_workspace_version(cargo_toml)
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    active_pins = json.loads(active_pins_path.read_text(encoding="utf-8"))

    artifact = manifest.get("artifact")
    if not isinstance(artifact, dict):
        raise QualificationError("model package manifest is missing object 'artifact'")
    manifest_bytes = artifact.get("bytes")
    if not isinstance(manifest_bytes, int) or manifest_bytes <= 0:
        raise QualificationError("model package manifest artifact.bytes must be a positive integer")
    manifest_sha256 = normalize_bare_hex64(
        artifact.get("sha256"),
        "model package manifest artifact.sha256",
    )
    manifest_blake3 = normalize_bare_hex64(
        artifact.get("blake3"),
        "model package manifest artifact.blake3",
    )

    model_pin = active_pins.get("model")
    if not isinstance(model_pin, dict):
        raise QualificationError("active pins file is missing object 'model'")
    pin_file_name = model_pin.get("file_name")
    pin_size = model_pin.get("size")
    pin_url = model_pin.get("url")
    if not isinstance(pin_file_name, str) or not pin_file_name or "/" in pin_file_name:
        raise QualificationError("active pins model.file_name must be a plain filename")
    if not isinstance(pin_size, int) or pin_size <= 0:
        raise QualificationError("active pins model.size must be a positive integer")
    pin_sha256 = normalize_bare_hex64(model_pin.get("sha256"), "active pins model.sha256")
    if not isinstance(pin_url, str) or not pin_url.startswith("https://"):
        raise QualificationError("active pins model.url must be an https URL")
    if not pin_url.endswith(f"/{pin_file_name}"):
        raise QualificationError("active pins model.url must end with model.file_name")

    if manifest_bytes != pin_size:
        raise QualificationError(
            f"model size mismatch between manifest ({manifest_bytes}) and active pins ({pin_size})"
        )
    if manifest_sha256 != pin_sha256:
        raise QualificationError("model sha256 mismatch between manifest and active pins")

    origins = active_pins.get("community_rpc_origins")
    if not isinstance(origins, list) or not origins:
        raise QualificationError("active pins community_rpc_origins must be a non-empty list")
    origin_ips: set[str] = set()
    for origin in origins:
        if not isinstance(origin, str):
            raise QualificationError("community_rpc_origins entries must be strings")
        parsed = urlparse(origin)
        if parsed.scheme != "https" or not parsed.hostname:
            raise QualificationError(f"invalid community RPC origin in pins: {origin!r}")
        try:
            ip_obj = ipaddress.IPv4Address(parsed.hostname)
        except ValueError as error:
            raise QualificationError(
                f"community RPC origin must be an IPv4 address: {origin!r}"
            ) from error
        if ip_obj.is_loopback or ip_obj.is_unspecified:
            raise QualificationError(f"unexpected loopback/unspecified origin: {origin!r}")
        origin_ips.add(str(ip_obj))

    genesis_ips = extract_non_loopback_ipv4s(genesis_path.read_text(encoding="utf-8"))
    seeds_ips = extract_non_loopback_ipv4s(seeds_path.read_text(encoding="utf-8"))
    all_ips = origin_ips | genesis_ips | seeds_ips
    if len(origin_ips) != EXPECTED_PRODUCTION_IPV4_COUNT:
        raise QualificationError(
            f"expected {EXPECTED_PRODUCTION_IPV4_COUNT} production IPv4 origins in pins, found {len(origin_ips)}"
        )
    if all_ips != origin_ips:
        extra = sorted(all_ips - origin_ips)
        raise QualificationError(
            f"genesis.toml or testnet-seeds.txt contains production IPv4 addresses absent from pins: {extra}"
        )

    tx_rs = repo_root / "crates" / "arc-types" / "src" / "transaction.rs"
    if tx_rs.is_file() and not tx_rs.is_symlink():
        tx_text = tx_rs.read_text(encoding="utf-8")
        if f'"{CANONICAL_EXECUTION_PROFILE}"' not in tx_text:
            raise QualificationError(
                "CANONICAL_REWARD_INFERENCE_PROFILE in crates/arc-types/src/transaction.rs "
                "does not match expected canonical execution profile"
            )

    node_main_rs = repo_root / "crates" / "arc-node" / "src" / "main.rs"
    if node_main_rs.is_file() and not node_main_rs.is_symlink():
        node_main_text = node_main_rs.read_text(encoding="utf-8")
        for required_literal in (
            MODEL_PREPARED_SOURCE_FORMAT,
            "Early worker startup RPC listening in loading state before canonical model preparation",
            "shutdown requested during initialization; persistent state is durable and later runtime stages were skipped",
            "RPC handlers drained, node writers joined, WAL durability barrier completed",
        ):
            if required_literal not in node_main_text:
                raise QualificationError(
                    f"crates/arc-node/src/main.rs missing bound qualification log literal: {required_literal!r}"
                )

    return RepoPins(
        candidate_version=candidate_version,
        model_file_name=pin_file_name,
        model_size=pin_size,
        model_sha256=pin_sha256,
        model_blake3=manifest_blake3,
        model_url=pin_url,
        production_ipv4s=tuple(sorted(origin_ips)),
        canonical_execution_profile=CANONICAL_EXECUTION_PROFILE,
    )


def validate_caller_environment(env: dict[str, str]) -> None:
    inherited_arc = sorted(key for key in env if key.upper().startswith("ARC_"))
    if inherited_arc:
        raise QualificationError(
            f"refusing inherited ARC_* environment variables: {', '.join(inherited_arc)}"
        )


def build_isolated_child_env(
    base_env: dict[str, str],
    isolated_home: Path,
    isolated_tmp: Path,
) -> dict[str, str]:
    validate_caller_environment(base_env)
    child_env: dict[str, str] = {}
    for key in ("PATH", "LANG", "LC_ALL", "TZ", "TERM", "USER", "LOGNAME"):
        if key in base_env and base_env[key]:
            child_env[key] = base_env[key]
    if "PATH" not in child_env:
        child_env["PATH"] = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    child_env["HOME"] = str(isolated_home)
    child_env["TMPDIR"] = str(isolated_tmp)
    child_env["RAYON_NUM_THREADS"] = "1"
    child_env["NO_PROXY"] = "127.0.0.1,localhost"
    child_env["no_proxy"] = "127.0.0.1,localhost"
    child_env["RUST_LOG"] = "arc=info"
    return child_env


def nominal_ram_gb(visible_bytes: int) -> int:
    if visible_bytes <= 0:
        return 0
    for size_gb in MARKETED_RAM_SIZES_GB:
        size_bytes = size_gb * GIB_BYTES
        if visible_bytes > size_bytes:
            continue
        tolerance = max(GIB_BYTES, (size_bytes // 100) * MAX_RESERVED_RAM_PERCENT)
        if size_bytes - visible_bytes <= tolerance:
            return size_gb
        break
    return visible_bytes // GIB_BYTES


def parse_linux_meminfo(meminfo_path: Path) -> tuple[int, int]:
    mem_total: int | None = None
    mem_available: int | None = None
    mem_free: int | None = None
    buffers = 0
    cached = 0
    for raw_line in meminfo_path.read_text(encoding="utf-8").splitlines():
        parts = raw_line.split()
        if len(parts) < 3 or parts[2] != "kB":
            continue
        key = parts[0]
        try:
            kib = int(parts[1])
        except ValueError:
            continue
        if key == "MemTotal:":
            mem_total = kib * 1024
        elif key == "MemAvailable:":
            mem_available = kib * 1024
        elif key == "MemFree:":
            mem_free = kib * 1024
        elif key == "Buffers:":
            buffers = kib * 1024
        elif key == "Cached:":
            cached = kib * 1024
    if mem_total is None or mem_total <= 0:
        raise QualificationError(f"cannot determine MemTotal from {meminfo_path}")
    if mem_available is None:
        if mem_free is None:
            raise QualificationError(f"cannot determine MemAvailable from {meminfo_path}")
        mem_available = mem_free + buffers + cached
    return mem_total, mem_available


def read_cgroup_memory_bounds(cgroup_dir: Path) -> tuple[int | None, int | None]:
    max_file = cgroup_dir / "memory.max"
    current_file = cgroup_dir / "memory.current"
    stat_file = cgroup_dir / "memory.stat"
    if not max_file.is_file() or not current_file.is_file():
        return None, None
    raw_max = max_file.read_text(encoding="utf-8").strip()
    if raw_max == "max":
        return None, None
    try:
        limit_bytes = int(raw_max)
        current_bytes = int(current_file.read_text(encoding="utf-8").strip())
    except ValueError as error:
        raise QualificationError(f"invalid cgroup memory numbers in {cgroup_dir}") from error
    if limit_bytes <= 0 or limit_bytes >= (1 << 60):
        return None, None
    inactive_file = 0
    if stat_file.is_file():
        for line in stat_file.read_text(encoding="utf-8").splitlines():
            parts = line.split()
            if len(parts) == 2 and parts[0] == "inactive_file":
                try:
                    inactive_file = int(parts[1])
                except ValueError:
                    inactive_file = 0
                break
    working_bytes = max(0, current_bytes - inactive_file)
    available_bytes = max(0, limit_bytes - working_bytes)
    return limit_bytes, available_bytes


def measure_runner_resources(
    disk_path: Path,
    meminfo_path: Path = Path("/proc/meminfo"),
    cgroup_dir: Path = Path("/sys/fs/cgroup"),
    disk_usage_fn: Callable[[Path], Any] = shutil.disk_usage,
) -> ResourceSnapshot:
    mem_total, mem_available = parse_linux_meminfo(meminfo_path)
    cgroup_limit, cgroup_available = read_cgroup_memory_bounds(cgroup_dir)
    effective_total = mem_total if cgroup_limit is None else min(mem_total, cgroup_limit)
    effective_available = (
        mem_available if cgroup_available is None else min(mem_available, cgroup_available)
    )
    usage = disk_usage_fn(disk_path)
    return ResourceSnapshot(
        mem_total_bytes=mem_total,
        mem_available_bytes=mem_available,
        cgroup_limit_bytes=cgroup_limit,
        cgroup_available_bytes=cgroup_available,
        effective_total_bytes=effective_total,
        effective_available_bytes=effective_available,
        nominal_ram_gb=nominal_ram_gb(effective_total),
        disk_path=str(disk_path),
        disk_total_bytes=int(usage.total),
        disk_free_bytes=int(usage.free),
    )


def enforce_runner_resource_floor(snapshot: ResourceSnapshot) -> None:
    if snapshot.nominal_ram_gb < MIN_NOMINAL_RAM_GB:
        raise QualificationError(
            f"host nominal RAM {snapshot.nominal_ram_gb} GB is below required {MIN_NOMINAL_RAM_GB} GB "
            f"(effective_total_bytes={snapshot.effective_total_bytes})"
        )
    if snapshot.effective_available_bytes < MIN_AVAILABLE_RAM_BYTES:
        raise QualificationError(
            f"effective available RAM {snapshot.effective_available_bytes} bytes is below required "
            f"{MIN_AVAILABLE_RAM_BYTES} bytes (12 GiB)"
        )
    if snapshot.disk_free_bytes < MIN_FREE_DISK_BYTES:
        raise QualificationError(
            f"free disk space {snapshot.disk_free_bytes} bytes on {snapshot.disk_path} is below "
            f"required {MIN_FREE_DISK_BYTES} bytes (10 GiB)"
        )


def verify_model_artifact(model_path: Path, pins: RepoPins) -> dict[str, Any]:
    info = ensure_regular_non_symlink(model_path, "pinned GGUF model")
    if info.st_size != pins.model_size:
        raise QualificationError(
            f"model byte size mismatch: expected {pins.model_size}, found {info.st_size}"
        )
    actual_sha256 = sha256_file(model_path)
    if actual_sha256 != pins.model_sha256:
        raise QualificationError(
            f"model sha256 mismatch: expected {pins.model_sha256}, found {actual_sha256}"
        )
    return {
        "path": str(model_path.resolve()),
        "bytes": info.st_size,
        "sha256": actual_sha256,
        "expected_blake3": pins.model_blake3,
    }


def verify_binary_artifact(binary_path: Path, label: str) -> dict[str, Any]:
    info = ensure_regular_non_symlink(binary_path, label)
    if not os.access(binary_path, os.X_OK):
        raise QualificationError(f"{label} is not executable: {binary_path}")
    resolved = binary_path.resolve()
    return {
        "path": str(resolved),
        "bytes": info.st_size,
        "sha256": sha256_file(resolved),
    }


def verify_production_ipv4_blocked(
    production_ipv4s: tuple[str, ...],
    port: int = 443,
    timeout_secs: float = 2.0,
    connect_fn: Callable[[tuple[str, int], float], None] | None = None,
) -> list[dict[str, Any]]:
    def default_connect(target: tuple[str, int], timeout: float) -> None:
        with socket.create_connection(target, timeout=timeout):
            return None

    probe = connect_fn or default_connect
    results: list[dict[str, Any]] = []
    for ip_str in production_ipv4s:
        try:
            probe((ip_str, port), timeout_secs)
        except OSError as error:
            err_name = errno.errorcode.get(error.errno, type(error).__name__)
            results.append(
                {
                    "ip": ip_str,
                    "port": port,
                    "blocked": True,
                    "error": err_name,
                }
            )
            continue
        raise QualificationError(
            f"production validator IPv4 {ip_str}:{port} is still reachable; aborting qualification"
        )
    return results


def pick_ephemeral_loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def verify_loopback_port_released(port: int, timeout_secs: float = 5.0) -> None:
    deadline = time.monotonic() + timeout_secs
    last_error: OSError | None = None
    while time.monotonic() < deadline:
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                sock.bind(("127.0.0.1", port))
                sock.listen(1)
            return
        except OSError as error:
            last_error = error
            time.sleep(0.1)
    raise QualificationError(
        f"loopback TCP port 127.0.0.1:{port} remained bound after exit: {last_error}"
    )


def verify_process_executable(pid: int, expected_binary: Path) -> str | None:
    proc_exe = Path(f"/proc/{pid}/exe")
    if not proc_exe.exists() and not proc_exe.is_symlink():
        return None
    actual = Path(os.path.realpath(proc_exe))
    expected = expected_binary.resolve()
    if actual != expected:
        raise QualificationError(
            f"running process pid={pid} executable {actual} differs from expected {expected}"
        )
    return str(actual)


class BoundedPipeLogCapture:
    def __init__(
        self,
        pipe_stream: Any,
        phase_name: str,
        max_retained_bytes: int = MAX_EVIDENCE_LOG_BYTES,
        max_total_bytes: int = MAX_RUNTIME_LOG_BYTES,
        tracked_markers: tuple[str, ...] = ALL_TRACKED_LOG_MARKERS,
        on_overflow: Callable[[], None] | None = None,
    ) -> None:
        if max_retained_bytes < len(TRUNCATION_MARKER) + 16:
            raise ValueError("max_retained_bytes is too small for bounded head/tail retention")
        if max_total_bytes < max_retained_bytes:
            raise ValueError("max_total_bytes must be at least max_retained_bytes")

        self.phase_name = phase_name
        self.max_retained_bytes = max_retained_bytes
        self.max_total_bytes = max_total_bytes
        self._tracked_markers = tracked_markers
        self._on_overflow = on_overflow

        budget = max_retained_bytes - len(TRUNCATION_MARKER)
        self._head_limit = budget // 2
        self._tail_limit = budget - self._head_limit

        self._pipe_stream = pipe_stream
        self._pipe_fd = pipe_stream.fileno()
        os.set_blocking(self._pipe_fd, False)
        self._stop_r, self._stop_w = os.pipe()
        os.set_blocking(self._stop_r, False)
        os.set_blocking(self._stop_w, False)

        self._lock = threading.Lock()
        self._eof_event = threading.Event()
        self._done_event = threading.Event()
        self._closed = False
        self._stop_requested = False
        self._pipe_eof = False
        self._overflowed = False
        self._overflow_reason: str | None = None
        self._overflow_notified = False

        self._head = bytearray()
        self._tail = bytearray()
        self._truncated = False
        self._partial_line = bytearray()
        self._partial_line_offset = 0
        self._total_bytes = 0
        self._total_lines = 0
        self._sha256 = hashlib.sha256()
        self._seen_markers: dict[str, dict[str, Any]] = {}
        self._model_prepared_record: dict[str, Any] | None = None
        self._recent_lines: deque[str] = deque(maxlen=8)

        self._thread = threading.Thread(
            target=self._reader_loop,
            name=f"arc-log-capture-{phase_name}",
            daemon=True,
        )
        self._thread.start()

    def _consume_line_locked(self, raw_line: bytes, byte_offset: int) -> None:
        self._total_lines += 1
        text = raw_line.decode("utf-8", errors="replace").rstrip("\r\n")
        if text:
            self._recent_lines.append(text[:240])
        now_mono = time.monotonic()
        now_utc = utc_now()
        for marker in self._tracked_markers:
            if marker not in self._seen_markers and marker in text:
                self._seen_markers[marker] = {
                    "line_number": self._total_lines,
                    "byte_offset": byte_offset,
                    "monotonic_secs": now_mono,
                    "utc": now_utc,
                    "line": text[:512],
                }
        if self._model_prepared_record is None:
            parsed = parse_model_prepared_line(text)
            if parsed is not None:
                parsed["line_number"] = self._total_lines
                parsed["byte_offset"] = byte_offset
                parsed["monotonic_secs"] = now_mono
                parsed["utc"] = now_utc
                self._model_prepared_record = parsed

    def _append_retained_locked(self, chunk: bytes) -> None:
        if not self._truncated:
            if len(self._head) + len(chunk) <= self.max_retained_bytes:
                self._head.extend(chunk)
                return
            self._truncated = True
            retained = bytes(self._head) + chunk
            self._head = bytearray(retained[: self._head_limit])
            self._tail = bytearray(retained[-self._tail_limit :])
            return
        if len(chunk) >= self._tail_limit:
            self._tail = bytearray(chunk[-self._tail_limit :])
        else:
            self._tail.extend(chunk)
            excess = len(self._tail) - self._tail_limit
            if excess > 0:
                del self._tail[:excess]

    def _ingest_chunk_locked(self, chunk: bytes) -> bool:
        chunk_start_offset = self._total_bytes
        self._total_bytes += len(chunk)
        self._sha256.update(chunk)
        self._append_retained_locked(chunk)

        pos = 0
        chunk_len = len(chunk)
        while pos < chunk_len:
            newline_idx = chunk.find(b"\n", pos)
            if newline_idx == -1:
                segment = chunk[pos:]
                if not self._partial_line:
                    self._partial_line_offset = chunk_start_offset + pos
                space = MAX_SINGLE_LINE_BYTES - len(self._partial_line)
                if len(segment) <= space:
                    self._partial_line.extend(segment)
                else:
                    self._partial_line.extend(segment[:space])
                    self._consume_line_locked(bytes(self._partial_line), self._partial_line_offset)
                    self._partial_line.clear()
                    remaining_pos = pos + space
                    self._partial_line_offset = chunk_start_offset + remaining_pos
                    tail_seg = chunk[remaining_pos:]
                    if len(tail_seg) > MAX_SINGLE_LINE_BYTES:
                        tail_seg = tail_seg[-MAX_SINGLE_LINE_BYTES:]
                    self._partial_line.extend(tail_seg)
                break
            line_part = chunk[pos : newline_idx + 1]
            if not self._partial_line:
                self._partial_line_offset = chunk_start_offset + pos
            space = MAX_SINGLE_LINE_BYTES - len(self._partial_line)
            if space > 0:
                self._partial_line.extend(line_part[:space])
            self._consume_line_locked(bytes(self._partial_line), self._partial_line_offset)
            self._partial_line.clear()
            pos = newline_idx + 1

        if self._total_bytes > self.max_total_bytes and not self._overflowed:
            self._overflowed = True
            self._overflow_reason = (
                f"{self.phase_name} log output exceeded runtime limit of {self.max_total_bytes} bytes "
                f"(observed {self._total_bytes} bytes)"
            )
            return True
        return False

    def _flush_partial_line_locked(self) -> None:
        if self._partial_line:
            self._consume_line_locked(bytes(self._partial_line), self._partial_line_offset)
            self._partial_line.clear()

    def _drain_available(self, max_bytes: int = MAX_DRAIN_BYTES_PER_CALL) -> bool:
        trigger_overflow = False
        read_total = 0
        with self._lock:
            if self._closed or self._pipe_eof or self._overflowed:
                return False
            while read_total < max_bytes:
                to_read = min(8192, max_bytes - read_total)
                try:
                    chunk = os.read(self._pipe_fd, to_read)
                except BlockingIOError:
                    break
                except OSError as error:
                    if error.errno in (errno.EAGAIN, errno.EWOULDBLOCK):
                        break
                    self._pipe_eof = True
                    self._flush_partial_line_locked()
                    self._eof_event.set()
                    self._done_event.set()
                    break
                if not chunk:
                    self._pipe_eof = True
                    self._flush_partial_line_locked()
                    self._eof_event.set()
                    self._done_event.set()
                    break
                read_total += len(chunk)
                if self._ingest_chunk_locked(chunk):
                    self._done_event.set()
                    if not self._overflow_notified:
                        self._overflow_notified = True
                        trigger_overflow = True
                    break
        if trigger_overflow and self._on_overflow is not None:
            self._on_overflow()
        return trigger_overflow

    def _reader_loop(self) -> None:
        try:
            while True:
                with self._lock:
                    if self._stop_requested or self._pipe_eof or self._overflowed or self._closed:
                        break
                    pipe_fd = self._pipe_fd
                    stop_r = self._stop_r
                try:
                    ready, _, _ = select.select([pipe_fd, stop_r], [], [], 0.1)
                except (OSError, ValueError):
                    break
                if stop_r in ready:
                    try:
                        os.read(stop_r, 64)
                    except OSError:
                        pass
                    self._drain_available(max_bytes=MAX_DRAIN_BYTES_PER_CALL)
                    break
                if pipe_fd in ready:
                    self._drain_available(max_bytes=65536)
        finally:
            self._done_event.set()

    def snapshot_after_drain(self) -> dict[str, Any]:
        self._drain_available(max_bytes=MAX_DRAIN_BYTES_PER_CALL)
        with self._lock:
            return {
                "phase": self.phase_name,
                "total_bytes_seen": self._total_bytes,
                "total_lines_seen": self._total_lines,
                "overflowed": self._overflowed,
                "overflow_reason": self._overflow_reason,
                "pipe_eof": self._pipe_eof,
                "model_prepared_seen": self._model_prepared_record is not None,
                "model_prepared_record": (
                    dict(self._model_prepared_record)
                    if self._model_prepared_record is not None
                    else None
                ),
                "recent_lines": list(self._recent_lines),
            }

    def drain_until_eof(self, timeout_secs: float = 3.0) -> bool:
        with self._lock:
            if self._pipe_eof:
                return True
            if self._closed or self._overflowed:
                return False
        self._done_event.wait(timeout=timeout_secs)
        self._drain_available(max_bytes=MAX_DRAIN_BYTES_PER_CALL)
        return self.pipe_eof

    @property
    def overflowed(self) -> bool:
        with self._lock:
            return self._overflowed

    @property
    def overflow_reason(self) -> str | None:
        with self._lock:
            return self._overflow_reason

    @property
    def pipe_eof(self) -> bool:
        with self._lock:
            return self._pipe_eof

    @property
    def total_bytes_seen(self) -> int:
        with self._lock:
            return self._total_bytes

    def seen_markers(self) -> dict[str, dict[str, Any]]:
        with self._lock:
            return {key: dict(value) for key, value in self._seen_markers.items()}

    def model_prepared_record(self) -> dict[str, Any] | None:
        with self._lock:
            return (
                dict(self._model_prepared_record)
                if self._model_prepared_record is not None
                else None
            )

    def retained_bytes(self) -> bytes:
        with self._lock:
            if not self._truncated:
                return bytes(self._head)
            return bytes(self._head) + TRUNCATION_MARKER + bytes(self._tail)

    def write_retained_log(self, dest_path: Path) -> dict[str, Any]:
        payload = self.retained_bytes()
        dest_path.write_bytes(payload)
        with self._lock:
            return {
                "phase": self.phase_name,
                "original_bytes": self._total_bytes,
                "total_lines": self._total_lines,
                "retained_bytes": len(payload),
                "truncated": self._truncated,
                "overflowed": self._overflowed,
                "overflow_reason": self._overflow_reason,
                "pipe_eof": self._pipe_eof,
                "sha256": self._sha256.hexdigest(),
                "seen_markers": sorted(self._seen_markers.keys()),
                "model_prepared_record": (
                    dict(self._model_prepared_record)
                    if self._model_prepared_record is not None
                    else None
                ),
            }

    def close(self, join_timeout_secs: float = 3.0) -> None:
        with self._lock:
            if self._closed:
                return
            self._stop_requested = True
            stop_w = self._stop_w
        try:
            os.write(stop_w, b"x")
        except OSError:
            pass
        self._thread.join(timeout=join_timeout_secs)
        alive = self._thread.is_alive()
        with self._lock:
            self._closed = True
            self._done_event.set()
            if not alive:
                self._flush_partial_line_locked()
                try:
                    self._pipe_stream.close()
                except OSError:
                    pass
                for fd in (self._stop_r, self._stop_w):
                    try:
                        os.close(fd)
                    except OSError:
                        pass
        if alive:
            raise QualificationError(
                f"bounded log reader thread for {self.phase_name} did not join within {join_timeout_secs:.1f}s"
            )


def summarize_coordinator_request_body(raw_body: bytes) -> dict[str, Any] | None:
    if not raw_body:
        return None
    try:
        parsed = json.loads(raw_body.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return {"parse_error": True, "body_bytes": len(raw_body)}
    if not isinstance(parsed, dict):
        return {"non_object_json": True, "body_bytes": len(raw_body)}
    summary: dict[str, Any] = {
        "version": parsed.get("version"),
        "target_coordinator": parsed.get("target_coordinator"),
        "transaction_domain": parsed.get("transaction_domain"),
        "timestamp_unix_ms": parsed.get("timestamp_unix_ms"),
        "nonce_present": is_hex64(parsed.get("nonce")),
        "payload_hash": parsed.get("payload_hash"),
        "signature_present": isinstance(parsed.get("signature"), dict),
    }
    payload = parsed.get("payload")
    if isinstance(payload, dict):
        summary["payload"] = {
            "worker_id": payload.get("worker_id"),
            "name": payload.get("name"),
            "capabilities": payload.get("capabilities"),
            "model": payload.get("model"),
            "model_id": payload.get("model_id"),
            "execution_profile": payload.get("execution_profile"),
            "platform": payload.get("platform"),
        }
    return summary


class LoopbackCommunityCoordinator:
    def __init__(
        self,
        coordinator_address_hex: str = FAKE_COORDINATOR_ADDRESS_HEX,
        request_timeout_secs: float = DEFAULT_COORDINATOR_REQUEST_TIMEOUT_SECS,
        max_body_bytes: int = MAX_COORDINATOR_REQUEST_BODY_BYTES,
        max_captured_requests: int = MAX_CAPTURED_COORDINATOR_REQUESTS,
    ) -> None:
        normalized_addr = normalize_bare_hex64(coordinator_address_hex, "coordinator_address_hex")
        if normalized_addr == "00" * 32:
            raise QualificationError("coordinator_address_hex must be non-zero")
        if request_timeout_secs <= 0:
            raise ValueError("request_timeout_secs must be positive")
        if max_body_bytes <= 0:
            raise ValueError("max_body_bytes must be positive")
        if max_captured_requests <= 0:
            raise ValueError("max_captured_requests must be positive")

        self.coordinator_address_hex = normalized_addr
        self.request_timeout_secs = request_timeout_secs
        self.max_body_bytes = max_body_bytes
        self.max_captured_requests = max_captured_requests

        self._lock = threading.Lock()
        self._requests: list[dict[str, Any]] = []
        self._total_requests = 0
        self._overflowed = False
        self._log_observer: BoundedPipeLogCapture | None = None
        self._allow_requests_after_prepared = True

        self._conn_lock = threading.Lock()
        self._active_sockets: set[socket.socket] = set()
        self._active_threads: set[threading.Thread] = set()
        self._closing = False
        self._closed = False

        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def setup(self) -> None:
                super().setup()
                self.connection.settimeout(owner.request_timeout_secs)
                self.close_connection = True

            def log_message(self, format_str: str, *args: Any) -> None:
                return None

            def _send_json(self, status_code: int, payload: dict[str, Any]) -> None:
                self.close_connection = True
                encoded = json.dumps(payload).encode("utf-8")
                self.send_response(status_code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(encoded)))
                self.send_header("Connection", "close")
                try:
                    self.end_headers()
                    if self.command != "HEAD":
                        self.wfile.write(encoded)
                except (BrokenPipeError, ConnectionResetError):
                    return

            def _read_strict_bounded_body(
                self,
                deadline: float,
            ) -> tuple[bytes, int, int | None, str | None]:
                if self.headers.get("Transfer-Encoding") is not None:
                    return b"", -1, 400, "Transfer-Encoding is not permitted"
                raw_values = self.headers.get_all("Content-Length") or []
                if not raw_values:
                    return b"", 0, None, None
                if len(raw_values) > 1:
                    return b"", -1, 400, "duplicate Content-Length headers"
                raw_cl = raw_values[0].strip()
                if not raw_cl or not raw_cl.isascii() or not raw_cl.isdigit():
                    return b"", -1, 400, "malformed Content-Length header"
                length = int(raw_cl)
                if length > owner.max_body_bytes:
                    return (
                        b"",
                        length,
                        413,
                        f"Content-Length {length} exceeds bounded limit {owner.max_body_bytes}",
                    )
                if length == 0:
                    return b"", 0, None, None
                buf = bytearray()
                while len(buf) < length:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        return b"", length, 408, "request body read exceeded connection deadline"
                    self.connection.settimeout(remaining)
                    to_read = min(4096, length - len(buf))
                    try:
                        chunk = self.rfile.read1(to_read)
                    except (socket.timeout, TimeoutError):
                        return b"", length, 408, "request body read timed out"
                    except OSError:
                        return b"", length, 400, "connection closed during request body read"
                    if not chunk:
                        return b"", length, 400, "truncated request body before Content-Length"
                    buf.extend(chunk)
                return bytes(buf), length, None, None

            def _record_and_respond(
                self,
                method: str,
                clean_path: str,
                content_length: int,
                response_status: int,
                response_payload: dict[str, Any],
                summary: dict[str, Any] | None,
                prepared_before_request: bool,
                log_snapshot: dict[str, Any] | None,
                rejected_reason: str | None = None,
            ) -> None:
                record: dict[str, Any] = {
                    "monotonic_secs": time.monotonic(),
                    "utc": utc_now(),
                    "method": method,
                    "path": clean_path,
                    "content_length": content_length,
                    "response_status": response_status,
                    "prepared_before_request": prepared_before_request,
                    "rejected_reason": rejected_reason,
                    "summary": summary,
                    "log_snapshot": log_snapshot,
                }
                with owner._lock:
                    owner._total_requests += 1
                    record["index"] = owner._total_requests
                    if len(owner._requests) < owner.max_captured_requests:
                        owner._requests.append(record)
                    else:
                        owner._overflowed = True
                self._send_json(response_status, response_payload)

            def _handle_request(self, method: str) -> None:
                deadline = time.monotonic() + owner.request_timeout_secs
                clean_path = urlparse(self.path).path
                log_snapshot, allow_after_prepared = owner._capture_causal_log_snapshot()
                if log_snapshot is None:
                    prepared_before_request = allow_after_prepared
                else:
                    prepared_before_request = bool(
                        allow_after_prepared
                        and log_snapshot["model_prepared_seen"]
                        and not log_snapshot["overflowed"]
                    )

                raw_body = b""
                content_length = 0
                if method in ("POST", "PUT", "PATCH") or self.headers.get("Content-Length") is not None or self.headers.get("Transfer-Encoding") is not None:
                    raw_body, content_length, err_status, err_msg = self._read_strict_bounded_body(
                        deadline
                    )
                    if err_status is not None:
                        self._record_and_respond(
                            method=method,
                            clean_path=clean_path,
                            content_length=content_length,
                            response_status=err_status,
                            response_payload={"error": err_msg or "invalid request"},
                            summary=None,
                            prepared_before_request=prepared_before_request,
                            log_snapshot=log_snapshot,
                            rejected_reason=err_msg,
                        )
                        return

                summary = summarize_coordinator_request_body(raw_body) if raw_body else None

                if not prepared_before_request:
                    self._record_and_respond(
                        method=method,
                        clean_path=clean_path,
                        content_length=content_length,
                        response_status=409,
                        response_payload={
                            "error": "premature coordinator request before canonical model preparation marker"
                        },
                        summary=summary,
                        prepared_before_request=False,
                        log_snapshot=log_snapshot,
                        rejected_reason="premature_before_model_prepared",
                    )
                    return

                if method == "GET" and clean_path == "/network/info":
                    self._record_and_respond(
                        method=method,
                        clean_path=clean_path,
                        content_length=content_length,
                        response_status=200,
                        response_payload={
                            "validator_address": f"0x{owner.coordinator_address_hex}",
                            "recovery_active": False,
                            "transaction_domain": None,
                        },
                        summary=None,
                        prepared_before_request=True,
                        log_snapshot=log_snapshot,
                    )
                    return

                self._record_and_respond(
                    method=method,
                    clean_path=clean_path,
                    content_length=content_length,
                    response_status=503,
                    response_payload={
                        "error": "qualification loopback coordinator does not accept registrations or serve work"
                    },
                    summary=summary,
                    prepared_before_request=True,
                    log_snapshot=log_snapshot,
                )

            def do_GET(self) -> None:
                self._handle_request("GET")

            def do_POST(self) -> None:
                self._handle_request("POST")

            def do_PUT(self) -> None:
                self._handle_request("PUT")

            def do_DELETE(self) -> None:
                self._handle_request("DELETE")

            def do_PATCH(self) -> None:
                self._handle_request("PATCH")

            def do_HEAD(self) -> None:
                self._handle_request("HEAD")

        class TrackedLoopbackServer(http.server.ThreadingHTTPServer):
            allow_reuse_address = True
            daemon_threads = False
            block_on_close = False

            def process_request(self, request: Any, client_address: Any) -> None:
                with owner._conn_lock:
                    if owner._closing:
                        try:
                            request.close()
                        except OSError:
                            pass
                        return
                    owner._active_sockets.add(request)
                    worker = threading.Thread(
                        target=self.process_request_thread,
                        args=(request, client_address),
                        daemon=False,
                    )
                    owner._active_threads.add(worker)
                    worker.start()

            def process_request_thread(self, request: Any, client_address: Any) -> None:
                current = threading.current_thread()
                try:
                    super().process_request_thread(request, client_address)
                except OSError:
                    pass
                finally:
                    with owner._conn_lock:
                        owner._active_sockets.discard(request)
                        owner._active_threads.discard(current)

        self._server = TrackedLoopbackServer(("127.0.0.1", 0), Handler)
        host, port = self._server.server_address
        self.host = str(host)
        self.port = int(port)
        self.base_url = f"http://{self.host}:{self.port}"
        self._thread = threading.Thread(
            target=self._server.serve_forever,
            kwargs={"poll_interval": 0.05},
            daemon=False,
        )
        self._thread.start()

    def attach_log_observer(
        self,
        capture: BoundedPipeLogCapture | None,
        allow_requests_after_prepared: bool,
    ) -> None:
        with self._lock:
            self._log_observer = capture
            self._allow_requests_after_prepared = allow_requests_after_prepared

    def _capture_causal_log_snapshot(self) -> tuple[dict[str, Any] | None, bool]:
        with self._lock:
            observer = self._log_observer
            allow_after_prepared = self._allow_requests_after_prepared
        if observer is None:
            return None, allow_after_prepared
        return observer.snapshot_after_drain(), allow_after_prepared

    def count(self) -> int:
        with self._lock:
            return self._total_requests

    def overflowed(self) -> bool:
        with self._lock:
            return self._overflowed

    def active_thread_count(self) -> int:
        with self._conn_lock:
            return sum(1 for thread in self._active_threads if thread.is_alive())

    def snapshot_requests(self) -> list[dict[str, Any]]:
        with self._lock:
            return json.loads(json.dumps(self._requests))

    def snapshot_state(self) -> tuple[list[dict[str, Any]], int, bool]:
        with self._lock:
            return (
                json.loads(json.dumps(self._requests)),
                self._total_requests,
                self._overflowed,
            )

    def close(self, timeout_secs: float = 5.0) -> None:
        deadline = time.monotonic() + timeout_secs
        with self._conn_lock:
            if self._closed:
                return
            self._closing = True
            sockets_to_abort = list(self._active_sockets)
        self._server.shutdown()
        for sock in sockets_to_abort:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                sock.close()
            except OSError:
                pass
        self._server.server_close()
        remaining = max(0.05, deadline - time.monotonic())
        self._thread.join(timeout=remaining)
        with self._conn_lock:
            threads_to_join = list(self._active_threads)
        for worker in threads_to_join:
            remaining = max(0.05, deadline - time.monotonic())
            worker.join(timeout=remaining)
        lingering = [worker for worker in threads_to_join if worker.is_alive()]
        with self._conn_lock:
            if not self._thread.is_alive() and not lingering:
                self._closed = True
        if self._thread.is_alive() or lingering:
            raise QualificationError(
                f"loopback coordinator failed to finish all request threads within {timeout_secs:.1f}s "
                f"(server_alive={self._thread.is_alive()}, lingering_handlers={len(lingering)})"
            )


def http_request(
    port: int,
    method: str,
    path: str,
    json_payload: dict[str, Any] | None = None,
    timeout_secs: float = 5.0,
) -> HttpResult:
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout_secs)
    try:
        headers: dict[str, str] = {"Connection": "close"}
        body_bytes: bytes | None = None
        if json_payload is not None:
            body_bytes = json.dumps(json_payload).encode("utf-8")
            headers["Content-Type"] = "application/json"
            headers["Content-Length"] = str(len(body_bytes))
        conn.request(method, path, body=body_bytes, headers=headers)
        response = conn.getresponse()
        raw = response.read(MAX_HTTP_RESPONSE_BYTES + 1)
        if len(raw) > MAX_HTTP_RESPONSE_BYTES:
            raise QualificationError(
                f"HTTP response from {method} {path} exceeded {MAX_HTTP_RESPONSE_BYTES} bytes"
            )
        lower_headers = {key.lower(): value for key, value in response.getheaders()}
        parsed: Any = None
        if raw:
            try:
                parsed = json.loads(raw.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                parsed = None
        return HttpResult(
            status=response.status,
            headers=lower_headers,
            raw_body=raw,
            json_body=parsed,
        )
    finally:
        conn.close()


def validate_loading_health(
    result: HttpResult,
    expected_version: str,
    expected_binary_sha256: str,
    elapsed_secs: float,
    max_loading_health_secs: float = MAX_LOADING_HEALTH_SECS,
) -> None:
    if elapsed_secs >= max_loading_health_secs:
        raise QualificationError(
            f"first loading /health arrived in {elapsed_secs:.3f}s, exceeding {max_loading_health_secs:.1f}s limit"
        )
    if result.status != 200:
        raise QualificationError(f"loading /health returned HTTP {result.status}, expected 200")
    if result.headers.get("connection", "").lower() != "close":
        raise QualificationError(
            f"loading /health must send Connection: close, got {result.headers.get('connection')!r}"
        )
    body = result.json_body
    if not isinstance(body, dict):
        raise QualificationError("loading /health body is not a JSON object")
    if body.get("status") != "loading":
        raise QualificationError(
            f"loading /health status must be 'loading', got {body.get('status')!r}"
        )
    if body.get("startup_phase") != "loading_model":
        raise QualificationError(
            f"loading /health startup_phase must be 'loading_model', got {body.get('startup_phase')!r}"
        )
    if body.get("version") != expected_version:
        raise QualificationError(
            f"loading /health version mismatch: expected {expected_version!r}, got {body.get('version')!r}"
        )
    if body.get("chain_participation_enabled") is not False:
        raise QualificationError("loading /health must report chain_participation_enabled=false")
    if body.get("peers") != 0:
        raise QualificationError(f"loading /health must report peers=0, got {body.get('peers')!r}")
    degraded_reason = body.get("degraded_reason")
    if not isinstance(degraded_reason, str) or "full_integer_worker" not in degraded_reason:
        raise QualificationError(
            f"loading /health degraded_reason must mention full_integer_worker, got {degraded_reason!r}"
        )
    binary_sha256 = body.get("binary_sha256")
    if binary_sha256 is not None:
        observed_sha = normalize_bare_hex64(binary_sha256, "loading /health binary_sha256")
        if observed_sha != expected_binary_sha256:
            raise QualificationError(
                f"loading /health binary_sha256 mismatch: expected {expected_binary_sha256}, got {observed_sha}"
            )
    features = body.get("features")
    if not isinstance(features, list) or "candle" not in features:
        raise QualificationError(f"loading /health features must include 'candle', got {features!r}")
    for forbidden in ("benchmark-tools", "native-test-executor"):
        if forbidden in features:
            raise QualificationError(f"candidate binary must not enable test feature {forbidden!r}")


def validate_loading_readiness(result: HttpResult) -> None:
    if result.status != 503:
        raise QualificationError(
            f"loading /inference/readiness returned HTTP {result.status}, expected 503"
        )
    if result.headers.get("retry-after") != "5":
        raise QualificationError(
            f"loading /inference/readiness Retry-After must be '5', got {result.headers.get('retry-after')!r}"
        )
    if result.headers.get("connection", "").lower() != "close":
        raise QualificationError("loading /inference/readiness must send Connection: close")
    body = result.json_body
    if not isinstance(body, dict):
        raise QualificationError("loading /inference/readiness body is not a JSON object")
    if set(body.keys()) != EXACT_READINESS_KEYS:
        raise QualificationError(
            f"loading /inference/readiness keys mismatch: got {sorted(body.keys())}"
        )
    if body["schema"] != "arc.inference.readiness.v1":
        raise QualificationError(f"unexpected readiness schema: {body['schema']!r}")
    for false_field in (
        "safe_to_dispatch",
        "community_dispatch_ready",
        "local_model_ready",
        "sharded_pipeline_ready",
    ):
        if body[false_field] is not False:
            raise QualificationError(f"loading /inference/readiness {false_field} must be false")
    if body["live_community_workers"] != 0:
        raise QualificationError("loading /inference/readiness live_community_workers must be 0")
    if body["model_id"] is not None:
        raise QualificationError(
            f"loading /inference/readiness model_id must be null, got {body['model_id']!r}"
        )
    if body["required_community_execution_profile"] != CANONICAL_EXECUTION_PROFILE:
        raise QualificationError(
            "loading /inference/readiness required_community_execution_profile mismatch"
        )
    if body["mutation_free_observation"] is not True:
        raise QualificationError("loading /inference/readiness mutation_free_observation must be true")


def validate_loading_worker_status(result: HttpResult) -> None:
    if result.status != 503:
        raise QualificationError(
            f"loading {WORKER_STATUS_PATH} returned HTTP {result.status}, expected 503"
        )
    if result.headers.get("retry-after") != "5":
        raise QualificationError(f"loading {WORKER_STATUS_PATH} Retry-After must be '5'")
    if result.headers.get("connection", "").lower() != "close":
        raise QualificationError(f"loading {WORKER_STATUS_PATH} must send Connection: close")
    body = result.json_body
    if not isinstance(body, dict):
        raise QualificationError(f"loading {WORKER_STATUS_PATH} body is not a JSON object")
    if body.get("status") != "loading":
        raise QualificationError(
            f"loading {WORKER_STATUS_PATH} status must be 'loading', got {body.get('status')!r}"
        )
    if body.get("role") != "full_integer_worker":
        raise QualificationError(
            f"loading {WORKER_STATUS_PATH} role must be 'full_integer_worker', got {body.get('role')!r}"
        )


def validate_loading_unavailable_route(route_label: str, result: HttpResult) -> None:
    if result.status != 503:
        raise QualificationError(
            f"loading {route_label} returned HTTP {result.status}, expected 503"
        )
    if result.headers.get("retry-after") != "5":
        raise QualificationError(f"loading {route_label} Retry-After must be '5'")
    if result.headers.get("connection", "").lower() != "close":
        raise QualificationError(f"loading {route_label} must send Connection: close")
    body = result.json_body
    if not isinstance(body, dict) or not isinstance(body.get("error"), str) or not body["error"]:
        raise QualificationError(f"loading {route_label} must return JSON error message")


def validate_ready_health(
    result: HttpResult,
    expected_version: str,
    expected_binary_sha256: str,
) -> None:
    if result.status != 200:
        raise QualificationError(f"ready /health returned HTTP {result.status}, expected 200")
    body = result.json_body
    if not isinstance(body, dict):
        raise QualificationError("ready /health body is not a JSON object")
    status_value = body.get("status")
    if not isinstance(status_value, str) or not status_value or status_value == "loading":
        raise QualificationError(f"ready /health did not leave loading state: {status_value!r}")
    if body.get("startup_phase") is not None:
        raise QualificationError(
            f"ready /health must omit startup_phase after handoff, got {body.get('startup_phase')!r}"
        )
    if body.get("version") != expected_version:
        raise QualificationError(
            f"ready /health version mismatch: expected {expected_version!r}, got {body.get('version')!r}"
        )
    if body.get("chain_participation_enabled") is not False:
        raise QualificationError("ready /health must report chain_participation_enabled=false")
    observed_sha = normalize_bare_hex64(body.get("binary_sha256"), "ready /health binary_sha256")
    if observed_sha != expected_binary_sha256:
        raise QualificationError(
            f"ready /health binary_sha256 mismatch: expected {expected_binary_sha256}, got {observed_sha}"
        )
    features = body.get("features")
    if not isinstance(features, list) or "candle" not in features:
        raise QualificationError(f"ready /health features must include 'candle', got {features!r}")
    for forbidden in ("benchmark-tools", "native-test-executor"):
        if forbidden in features:
            raise QualificationError(f"candidate binary must not enable test feature {forbidden!r}")


def validate_ready_node_info(
    result: HttpResult,
    expected_version: str,
    expected_address_hex: str,
) -> None:
    expected_addr = normalize_bare_hex64(expected_address_hex, "expected_address_hex")
    if result.status != 200:
        raise QualificationError(f"ready /node/info returned HTTP {result.status}, expected 200")
    body = result.json_body
    if not isinstance(body, dict):
        raise QualificationError("ready /node/info body is not a JSON object")
    if body.get("version") != expected_version:
        raise QualificationError(
            f"ready /node/info version mismatch: expected {expected_version!r}, got {body.get('version')!r}"
        )
    if body.get("stake") != 0:
        raise QualificationError(f"ready /node/info stake must be 0, got {body.get('stake')!r}")
    observed_validator = normalize_bare_hex64(body.get("validator"), "ready /node/info validator")
    if observed_validator != expected_addr:
        raise QualificationError(
            f"ready /node/info validator mismatch: expected {expected_addr!r}, got {observed_validator!r}"
        )


def validate_ready_readiness(result: HttpResult, expected_model_blake3: str) -> None:
    expected_blake3 = normalize_bare_hex64(expected_model_blake3, "expected_model_blake3")
    if result.status != 200:
        raise QualificationError(
            f"ready /inference/readiness returned HTTP {result.status}, expected 200"
        )
    body = result.json_body
    if not isinstance(body, dict):
        raise QualificationError("ready /inference/readiness body is not a JSON object")
    if set(body.keys()) != EXACT_READINESS_KEYS:
        raise QualificationError(
            f"ready /inference/readiness keys mismatch: got {sorted(body.keys())}"
        )
    if body["schema"] != "arc.inference.readiness.v1":
        raise QualificationError(f"ready /inference/readiness schema mismatch: {body['schema']!r}")
    if body["local_model_ready"] is not True:
        raise QualificationError("ready /inference/readiness local_model_ready must be true")
    if body["safe_to_dispatch"] is not True:
        raise QualificationError("ready /inference/readiness safe_to_dispatch must be true")
    observed_model_id = normalize_prefixed_hex64(
        body["model_id"],
        "ready /inference/readiness model_id",
    )
    if observed_model_id != expected_blake3:
        raise QualificationError(
            f"ready /inference/readiness model_id mismatch: expected 0x{expected_blake3}, "
            f"got {body['model_id']!r}"
        )
    if body["required_community_execution_profile"] != CANONICAL_EXECUTION_PROFILE:
        raise QualificationError(
            "ready /inference/readiness required_community_execution_profile mismatch"
        )
    if body["mutation_free_observation"] is not True:
        raise QualificationError("ready /inference/readiness mutation_free_observation must be true")


def validate_ready_worker_status(result: HttpResult, expected_address_hex: str) -> None:
    expected_addr = normalize_bare_hex64(expected_address_hex, "expected_address_hex")
    if result.status != 200:
        raise QualificationError(
            f"ready {WORKER_STATUS_PATH} returned HTTP {result.status}, expected 200"
        )
    body = result.json_body
    if not isinstance(body, dict):
        raise QualificationError(f"ready {WORKER_STATUS_PATH} body is not a JSON object")
    if body.get("schema") != WORKER_STATUS_SCHEMA:
        raise QualificationError(
            f"ready {WORKER_STATUS_PATH} schema mismatch: got {body.get('schema')!r}"
        )
    observed_worker_id = normalize_prefixed_hex64(
        body.get("worker_id"),
        f"ready {WORKER_STATUS_PATH} worker_id",
    )
    if observed_worker_id != expected_addr:
        raise QualificationError(
            f"ready {WORKER_STATUS_PATH} worker_id mismatch: expected 0x{expected_addr}, "
            f"got {body.get('worker_id')!r}"
        )
    if body.get("coordinators_total") != 1:
        raise QualificationError(
            f"ready {WORKER_STATUS_PATH} coordinators_total must be 1, got {body.get('coordinators_total')!r}"
        )
    if body.get("jobs_claimed") != 0 or body.get("jobs_completed") != 0:
        raise QualificationError(
            f"ready {WORKER_STATUS_PATH} must report zero claimed/completed jobs"
        )


def validate_causal_prepared_request_record(req: dict[str, Any]) -> dict[str, Any]:
    if req.get("prepared_before_request") is not True:
        raise QualificationError(
            f"coordinator request {req.get('method')} {req.get('path')} arrived before "
            f"canonical model preparation completed (reason={req.get('rejected_reason')!r})"
        )
    if req.get("rejected_reason") is not None:
        raise QualificationError(
            f"coordinator rejected request {req.get('method')} {req.get('path')}: {req.get('rejected_reason')!r}"
        )
    log_snapshot = req.get("log_snapshot")
    if not isinstance(log_snapshot, dict):
        raise QualificationError(
            f"coordinator request {req.get('method')} {req.get('path')} is missing causal log_snapshot"
        )
    if log_snapshot.get("overflowed") is True:
        raise QualificationError("node log overflowed before coordinator request")
    if log_snapshot.get("model_prepared_seen") is not True:
        raise QualificationError(
            f"coordinator request {req.get('method')} {req.get('path')} lacked causal model_prepared_seen"
        )
    prepared_record = log_snapshot.get("model_prepared_record")
    if not isinstance(prepared_record, dict):
        raise QualificationError(
            f"coordinator request {req.get('method')} {req.get('path')} lacked model_prepared_record"
        )
    layers_held = prepared_record.get("layers_held")
    total_layers = prepared_record.get("total_layers")
    if (
        not isinstance(layers_held, int)
        or not isinstance(total_layers, int)
        or layers_held <= 0
        or layers_held != total_layers
    ):
        raise QualificationError(
            f"causal model_prepared_record reports incomplete layers: held={layers_held}, total={total_layers}"
        )
    line = prepared_record.get("line")
    if not isinstance(line, str) or parse_model_prepared_line(line) is None:
        raise QualificationError(
            f"causal model_prepared_record line does not match canonical source marker: {line!r}"
        )
    return prepared_record


def validate_post_ready_registration(
    requests: list[dict[str, Any]],
    total_requests: int,
    coordinator_overflowed: bool,
    expected_address_hex: str,
    expected_model_blake3: str,
    expected_coordinator_hex: str,
) -> dict[str, Any]:
    expected_addr = normalize_bare_hex64(expected_address_hex, "expected_address_hex")
    expected_blake3 = normalize_bare_hex64(expected_model_blake3, "expected_model_blake3")
    expected_coord = normalize_bare_hex64(expected_coordinator_hex, "expected_coordinator_hex")

    if coordinator_overflowed or total_requests != len(requests):
        raise QualificationError(
            f"loopback coordinator request log overflowed (observed total={total_requests}, "
            f"retained={len(requests)}); refusing unobserved traffic"
        )
    if not requests:
        raise QualificationError("no outbound coordinator requests captured after model preparation")

    causal_prepared_evidence: dict[str, Any] | None = None
    for req in requests:
        prepared_record = validate_causal_prepared_request_record(req)
        if causal_prepared_evidence is None:
            causal_prepared_evidence = prepared_record
        method = req.get("method")
        path = req.get("path")
        status = req.get("response_status")
        if method == "GET" and path == "/network/info":
            if status != 200:
                raise QualificationError(
                    f"expected HTTP 200 for GET /network/info, got {status}"
                )
        elif status != 503:
            raise QualificationError(
                f"fake loopback coordinator must return HTTP 503 for {method} {path}, got {status}"
            )

    network_info_reqs = [
        req for req in requests if req.get("method") == "GET" and req.get("path") == "/network/info"
    ]
    register_reqs = [
        req
        for req in requests
        if req.get("method") == "POST" and req.get("path") == "/community/register"
    ]
    if not network_info_reqs:
        raise QualificationError("worker did not query GET /network/info after model preparation")
    if not register_reqs:
        raise QualificationError(
            "worker did not attempt POST /community/register after model preparation"
        )

    first_register = register_reqs[0]
    summary = first_register.get("summary")
    if not isinstance(summary, dict):
        raise QualificationError("captured POST /community/register lacked JSON summary")
    observed_coord = normalize_bare_hex64(
        summary.get("target_coordinator"),
        "POST /community/register target_coordinator",
    )
    if observed_coord != expected_coord:
        raise QualificationError(
            f"POST /community/register target_coordinator mismatch: "
            f"expected {expected_coord!r}, got {observed_coord!r}"
        )
    if summary.get("transaction_domain") is not None:
        raise QualificationError(
            f"POST /community/register expected null transaction_domain, got {summary.get('transaction_domain')!r}"
        )
    if summary.get("nonce_present") is not True:
        raise QualificationError("POST /community/register must include 64-hex nonce")
    normalize_bare_hex64(summary.get("payload_hash"), "POST /community/register payload_hash")
    if summary.get("signature_present") is not True:
        raise QualificationError("POST /community/register must include Ed25519 signature envelope")
    payload = summary.get("payload")
    if not isinstance(payload, dict):
        raise QualificationError("POST /community/register missing payload object")
    observed_worker_id = normalize_prefixed_hex64(
        payload.get("worker_id"),
        "POST /community/register payload.worker_id",
    )
    if observed_worker_id != expected_addr:
        raise QualificationError(
            f"POST /community/register worker_id mismatch: expected 0x{expected_addr}, "
            f"got {payload.get('worker_id')!r}"
        )
    if payload.get("capabilities") != ["inference"]:
        raise QualificationError(
            f"POST /community/register capabilities must be ['inference'], got {payload.get('capabilities')!r}"
        )
    observed_model_id = normalize_prefixed_hex64(
        payload.get("model_id"),
        "POST /community/register payload.model_id",
    )
    if observed_model_id != expected_blake3:
        raise QualificationError(
            f"POST /community/register model_id mismatch: expected 0x{expected_blake3}, "
            f"got {payload.get('model_id')!r}"
        )
    if payload.get("execution_profile") != CANONICAL_EXECUTION_PROFILE:
        raise QualificationError(
            f"POST /community/register execution_profile mismatch: got {payload.get('execution_profile')!r}"
        )
    if not isinstance(payload.get("model"), str) or not payload["model"]:
        raise QualificationError("POST /community/register must include non-empty model display name")

    return {
        "total_requests": len(requests),
        "network_info_count": len(network_info_reqs),
        "register_count": len(register_reqs),
        "causal_prepared_marker_evidence": causal_prepared_evidence,
        "first_register_summary": summary,
    }


def process_group_alive(pgid: int) -> bool:
    try:
        os.killpg(pgid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def list_live_pids_in_process_group(pgid: int) -> list[int]:
    proc_root = Path("/proc")
    if not proc_root.is_dir():
        return [pgid] if process_group_alive(pgid) else []
    live: list[int] = []
    for entry in proc_root.iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        try:
            stat_text = (entry / "stat").read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        rparen = stat_text.rfind(")")
        if rparen == -1:
            continue
        fields = stat_text[rparen + 2 :].split()
        if len(fields) < 3:
            continue
        state = fields[0]
        try:
            entry_pgid = int(fields[2])
        except ValueError:
            continue
        if entry_pgid == pgid and state != "Z":
            live.append(pid)
    return sorted(live)


def terminate_process_group(
    proc: subprocess.Popen[Any],
    grace_secs: float = 15.0,
) -> tuple[int, bool]:
    pgid = proc.pid
    forced_kill = False
    if proc.poll() is None:
        try:
            os.kill(proc.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            proc.wait(timeout=grace_secs)
        except subprocess.TimeoutExpired:
            forced_kill = True
            try:
                os.killpg(pgid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait(timeout=10.0)
    deadline = time.monotonic() + 5.0
    while time.monotonic() < deadline:
        if not process_group_alive(pgid) and not list_live_pids_in_process_group(pgid):
            break
        time.sleep(0.1)
    if process_group_alive(pgid) or list_live_pids_in_process_group(pgid):
        forced_kill = True
        try:
            os.killpg(pgid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            if not process_group_alive(pgid) and not list_live_pids_in_process_group(pgid):
                break
            time.sleep(0.1)
    return int(proc.returncode if proc.returncode is not None else -1), forced_kill


def build_worker_command(
    node_binary: Path,
    rpc_port: int,
    data_dir: Path,
    genesis_path: Path,
    keyfile_path: Path,
    model_path: Path,
    coordinator_url: str,
) -> list[str]:
    parsed = urlparse(coordinator_url)
    if parsed.scheme != "http" or parsed.hostname != "127.0.0.1" or not parsed.port:
        raise QualificationError(
            f"coordinator URL must be literal loopback http://127.0.0.1:<port>, got {coordinator_url!r}"
        )
    return [
        str(node_binary),
        "--rpc",
        f"127.0.0.1:{rpc_port}",
        "--p2p-port",
        "0",
        "--stake",
        "0",
        "--min-stake",
        "0",
        "--data-dir",
        str(data_dir),
        "--genesis",
        str(genesis_path),
        "--validator-key-file",
        str(keyfile_path),
        "--model",
        str(model_path),
        "--full-integer-worker",
        "--community-mode",
        "--community-rpc-url",
        coordinator_url,
        "--threads",
        "1",
    ]


def ensure_persistent_keyfile(
    private_dir: Path,
    output_dir: Path,
    arc_cli_binary: Path | None,
    existing_keyfile: Path | None,
    child_env: dict[str, str],
) -> PersistentIdentity:
    resolved_output = output_dir.resolve()
    if existing_keyfile is not None:
        keyfile_path = existing_keyfile.resolve()
        if resolved_output in keyfile_path.parents or keyfile_path == resolved_output:
            raise QualificationError(
                "validator keyfile must live outside the uploaded evidence output directory"
            )
    else:
        if arc_cli_binary is None:
            raise QualificationError(
                "provide either --arc-cli-binary or --validator-key-file to establish persistent Ed25519 identity"
            )
        verify_binary_artifact(arc_cli_binary, "arc CLI binary")
        keyfile_path = (private_dir / "worker-identity.json").resolve()
        if keyfile_path.exists() or keyfile_path.is_symlink():
            raise QualificationError(f"refusing to overwrite existing keyfile: {keyfile_path}")
        if resolved_output in keyfile_path.parents or keyfile_path == resolved_output:
            raise QualificationError(
                "private directory must be outside the uploaded evidence output directory"
            )
        completed = subprocess.run(
            [
                str(arc_cli_binary),
                "keygen",
                "--scheme",
                "ed25519",
                "--output",
                str(keyfile_path),
            ],
            env=child_env,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        if completed.returncode != 0:
            raise QualificationError(
                f"arc keygen failed (rc={completed.returncode}): {completed.stderr.strip()}"
            )
        verify_completed = subprocess.run(
            [
                str(arc_cli_binary),
                "keygen",
                "--verify-keyfile",
                str(keyfile_path),
            ],
            env=child_env,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        if verify_completed.returncode != 0:
            raise QualificationError(
                f"arc keygen --verify-keyfile failed (rc={verify_completed.returncode}): "
                f"{verify_completed.stderr.strip()}"
            )

    info = ensure_regular_non_symlink(keyfile_path, "validator keyfile")
    if stat.S_IMODE(info.st_mode) != 0o600:
        raise QualificationError(
            f"validator keyfile must have mode 0600, found {oct(stat.S_IMODE(info.st_mode))}"
        )
    data = json.loads(keyfile_path.read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise QualificationError("validator keyfile JSON must be an object")
    if data.get("scheme") != "ed25519":
        raise QualificationError(
            f"validator keyfile scheme must be ed25519, got {data.get('scheme')!r}"
        )
    address_hex = normalize_bare_hex64(data.get("address"), "validator keyfile address")
    public_key_hex = normalize_bare_hex64(data.get("public_key"), "validator keyfile public_key")
    secret_key_hex = normalize_bare_hex64(data.get("secret_key"), "validator keyfile secret_key")
    return PersistentIdentity(
        keyfile_path=keyfile_path,
        address_hex=address_hex,
        public_key_hex=public_key_hex,
        secret_key_hex=secret_key_hex,
    )


def snapshot_data_dir_state(data_dir: Path) -> dict[str, dict[str, Any]]:
    if not data_dir.is_dir() or data_dir.is_symlink():
        raise QualificationError(f"data directory missing or symlink: {data_dir}")
    snapshot: dict[str, dict[str, Any]] = {}
    for name in ("state.wal", "genesis.network-hash"):
        target = data_dir / name
        info = ensure_regular_non_symlink(target, f"persistent state file {name}")
        if info.st_size <= 0:
            raise QualificationError(f"persistent state file {name} is empty")
        snapshot[name] = {
            "bytes": info.st_size,
            "sha256": sha256_file(target),
        }
    return snapshot


def assert_no_secrets_in_directory(evidence_dir: Path, forbidden_secrets: tuple[str, ...]) -> None:
    lowered_secrets = [secret.lower().encode("ascii") for secret in forbidden_secrets if secret]
    for path in sorted(evidence_dir.rglob("*")):
        if path.is_symlink():
            raise QualificationError(f"evidence directory must not contain symlinks: {path}")
        if not path.is_file():
            continue
        blob = path.read_bytes().lower()
        for secret in lowered_secrets:
            if secret in blob:
                raise QualificationError(
                    f"secret material leaked into evidence file {path.name}; refusing artifact"
                )
        if b'"secret_key"' in blob:
            raise QualificationError(
                f"keyfile secret_key field leaked into evidence file {path.name}; refusing artifact"
            )


def write_json_atomic(path: Path, payload: Any) -> None:
    encoded = json.dumps(payload, indent=2, sort_keys=True) + "\n"
    path.write_text(encoded, encoding="utf-8")


def wait_for_initial_loading_health(
    proc: subprocess.Popen[Any],
    capture: BoundedPipeLogCapture,
    rpc_port: int,
    started_monotonic: float,
    expected_version: str,
    expected_binary_sha256: str,
    max_loading_health_secs: float,
) -> tuple[float, HttpResult]:
    deadline = started_monotonic + max_loading_health_secs
    last_error = "no response yet"
    while time.monotonic() < deadline:
        if capture.overflowed:
            raise QualificationError(capture.overflow_reason or "node log overflowed during startup")
        rc = proc.poll()
        if rc is not None:
            raise QualificationError(
                f"arc-node exited prematurely with code {rc} before answering /health"
            )
        try:
            result = http_request(rpc_port, "GET", "/health", timeout_secs=2.0)
            elapsed = time.monotonic() - started_monotonic
            validate_loading_health(
                result,
                expected_version,
                expected_binary_sha256,
                elapsed,
                max_loading_health_secs=max_loading_health_secs,
            )
            return elapsed, result
        except (OSError, http.client.HTTPException) as error:
            last_error = str(error)
            time.sleep(0.15)
    raise QualificationError(
        f"arc-node did not answer loading /health within {max_loading_health_secs:.1f}s: {last_error}"
    )


def probe_loading_surface(rpc_port: int) -> dict[str, Any]:
    readiness = http_request(rpc_port, "GET", "/inference/readiness", timeout_secs=3.0)
    validate_loading_readiness(readiness)

    worker_status = http_request(rpc_port, "GET", WORKER_STATUS_PATH, timeout_secs=3.0)
    validate_loading_worker_status(worker_status)

    node_info = http_request(rpc_port, "GET", "/node/info", timeout_secs=3.0)
    validate_loading_unavailable_route("GET /node/info", node_info)

    mutation_results: dict[str, int] = {}
    for path in ("/inference/run", "/tx/submit", "/community/register"):
        resp = http_request(rpc_port, "POST", path, json_payload={}, timeout_secs=3.0)
        validate_loading_unavailable_route(f"POST {path}", resp)
        mutation_results[path] = resp.status

    return {
        "inference_readiness": {
            "status": readiness.status,
            "retry_after": readiness.headers.get("retry-after"),
            "connection": readiness.headers.get("connection"),
            "body": readiness.json_body,
        },
        "worker_status": {
            "status": worker_status.status,
            "retry_after": worker_status.headers.get("retry-after"),
            "connection": worker_status.headers.get("connection"),
            "body": worker_status.json_body,
        },
        "node_info_status": node_info.status,
        "mutating_post_statuses": mutation_results,
    }


def run_sigterm_during_loading_phase(
    node_binary: Path,
    pins: RepoPins,
    binary_info: dict[str, Any],
    data_dir: Path,
    genesis_path: Path,
    identity: PersistentIdentity,
    model_path: Path,
    coordinator: LoopbackCommunityCoordinator,
    child_env: dict[str, str],
    evidence_log_path: Path,
    max_loading_health_secs: float,
    sigterm_timeout_secs: float,
    settle_secs: float,
    pre_sigterm_delay_secs: float = 1.0,
) -> tuple[dict[str, Any], dict[str, Any], int]:
    rpc_port = pick_ephemeral_loopback_port()
    cmd = build_worker_command(
        node_binary=node_binary,
        rpc_port=rpc_port,
        data_dir=data_dir,
        genesis_path=genesis_path,
        keyfile_path=identity.keyfile_path,
        model_path=model_path,
        coordinator_url=coordinator.base_url,
    )
    proc: subprocess.Popen[Any] | None = None
    capture: BoundedPipeLogCapture | None = None
    log_meta: dict[str, Any] = {}
    try:
        started_monotonic = time.monotonic()
        proc = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            env=child_env,
            start_new_session=True,
            close_fds=True,
        )
        assert proc.stdout is not None
        child_pgid = proc.pid

        def abort_on_log_overflow() -> None:
            try:
                os.killpg(child_pgid, signal.SIGTERM)
            except ProcessLookupError:
                pass

        capture = BoundedPipeLogCapture(
            pipe_stream=proc.stdout,
            phase_name="run1",
            on_overflow=abort_on_log_overflow,
        )
        coordinator.attach_log_observer(capture, allow_requests_after_prepared=False)

        first_health_elapsed, first_health = wait_for_initial_loading_health(
            proc=proc,
            capture=capture,
            rpc_port=rpc_port,
            started_monotonic=started_monotonic,
            expected_version=pins.candidate_version,
            expected_binary_sha256=binary_info["sha256"],
            max_loading_health_secs=max_loading_health_secs,
        )
        verified_exe = verify_process_executable(proc.pid, node_binary)
        loading_surface = probe_loading_surface(rpc_port)
        if coordinator.count() != 0:
            raise QualificationError(
                f"Run 1 observed {coordinator.count()} coordinator request(s) during loading"
            )

        if pre_sigterm_delay_secs > 0:
            time.sleep(pre_sigterm_delay_secs)
        if capture.overflowed:
            raise QualificationError(capture.overflow_reason or "Run 1 log overflowed")
        pre_sigterm_health = http_request(rpc_port, "GET", "/health", timeout_secs=3.0)
        validate_loading_health(
            pre_sigterm_health,
            pins.candidate_version,
            binary_info["sha256"],
            time.monotonic() - started_monotonic,
            max_loading_health_secs=max(max_loading_health_secs, 60.0),
        )
        if proc.poll() is not None:
            raise QualificationError("Run 1 exited before SIGTERM was sent")

        sigterm_sent_monotonic = time.monotonic()
        exit_code, forced_kill = terminate_process_group(proc, grace_secs=sigterm_timeout_secs)
        sigterm_exit_elapsed = time.monotonic() - sigterm_sent_monotonic
        proc = None

        pipe_eof = capture.drain_until_eof(timeout_secs=3.0)
        coordinator.attach_log_observer(None, allow_requests_after_prepared=False)
        capture.close(join_timeout_secs=3.0)
        log_meta = capture.write_retained_log(evidence_log_path)

        if capture.overflowed:
            raise QualificationError(capture.overflow_reason or "Run 1 log overflowed")
        if not pipe_eof:
            raise QualificationError(
                "Run 1 stdout pipe did not reach EOF after process exit; descendant kept pipe open"
            )
        if forced_kill:
            raise QualificationError(
                f"Run 1 did not exit cleanly within {sigterm_timeout_secs:.1f}s after SIGTERM"
            )
        if exit_code != 0:
            raise QualificationError(
                f"Run 1 exited with non-zero status {exit_code} after SIGTERM during loading"
            )
        surviving = list_live_pids_in_process_group(child_pgid)
        if process_group_alive(child_pgid) or surviving:
            raise QualificationError(
                f"Run 1 left surviving process-group members after exit: pgid={child_pgid} pids={surviving}"
            )
        verify_loopback_port_released(rpc_port)

        state_after_exit = snapshot_data_dir_state(data_dir)
        time.sleep(settle_secs)
        state_after_settle = snapshot_data_dir_state(data_dir)
        if state_after_exit != state_after_settle:
            raise QualificationError("Run 1 persistent state files mutated after process exit")
        if coordinator.count() != 0 or coordinator.overflowed():
            raise QualificationError(
                f"Run 1 observed {coordinator.count()} coordinator request(s) after SIGTERM"
            )

        seen_markers = capture.seen_markers()
        for marker in RUN1_REQUIRED_LOG_MARKERS:
            if marker not in seen_markers:
                raise QualificationError(
                    f"Run 1 log missing required clean-cancel durability marker: {marker!r}"
                )
        for forbidden in RUN1_FORBIDDEN_LOG_MARKERS:
            if forbidden in seen_markers:
                raise QualificationError(
                    f"Run 1 log contains forbidden post-load marker after SIGTERM: {forbidden!r}"
                )
        if capture.model_prepared_record() is not None:
            raise QualificationError("Run 1 completed model preparation despite SIGTERM cancellation")

        return (
            {
                "rpc_port": rpc_port,
                "verified_proc_exe": verified_exe,
                "first_loading_health_secs": round(first_health_elapsed, 4),
                "first_loading_health": first_health.json_body,
                "loading_surface": loading_surface,
                "sigterm_exit_secs": round(sigterm_exit_elapsed, 4),
                "exit_code": exit_code,
                "forced_kill": forced_kill,
                "pipe_eof": pipe_eof,
                "surviving_pgid_pids": surviving,
                "coordinator_requests_after_run1": coordinator.count(),
                "data_dir_state_after_exit": state_after_exit,
                "post_exit_settle_secs": settle_secs,
                "post_exit_state_unchanged": True,
                "required_log_markers_present": list(RUN1_REQUIRED_LOG_MARKERS),
                "forbidden_log_markers_absent": list(RUN1_FORBIDDEN_LOG_MARKERS),
            },
            log_meta,
            rpc_port,
        )
    finally:
        cleanup_errors: list[str] = []
        coordinator.attach_log_observer(None, allow_requests_after_prepared=False)
        if proc is not None:
            try:
                pgid = proc.pid
                terminate_process_group(proc, grace_secs=5.0)
                surviving_cleanup = list_live_pids_in_process_group(pgid)
                if process_group_alive(pgid) or surviving_cleanup:
                    cleanup_errors.append(
                        f"child process group {pgid} still alive after cleanup (pids={surviving_cleanup})"
                    )
            except CLEANUP_FAILURES as cleanup_err:
                cleanup_errors.append(f"process-group cleanup failed: {cleanup_err}")
        if capture is not None:
            try:
                capture.drain_until_eof(timeout_secs=1.0)
                capture.close(join_timeout_secs=3.0)
            except CLEANUP_FAILURES as cleanup_err:
                cleanup_errors.append(f"bounded log capture cleanup failed: {cleanup_err}")
            try:
                if not evidence_log_path.exists():
                    capture.write_retained_log(evidence_log_path)
            except CLEANUP_FAILURES as cleanup_err:
                cleanup_errors.append(f"retained log write failed: {cleanup_err}")
        if cleanup_errors:
            active_exc = sys.exc_info()[1]
            joined = "; ".join(cleanup_errors)
            if active_exc is None:
                raise QualificationError(joined)
            raise QualificationError(f"{active_exc}; cleanup errors: {joined}") from active_exc


def run_complete_load_and_handoff_phase(
    node_binary: Path,
    pins: RepoPins,
    binary_info: dict[str, Any],
    data_dir: Path,
    genesis_path: Path,
    identity: PersistentIdentity,
    model_path: Path,
    coordinator: LoopbackCommunityCoordinator,
    child_env: dict[str, str],
    evidence_log_path: Path,
    rpc_port: int,
    max_loading_health_secs: float,
    load_timeout_secs: float,
    registration_timeout_secs: float,
    sigterm_timeout_secs: float,
) -> tuple[dict[str, Any], dict[str, Any]]:
    verify_loopback_port_released(rpc_port)
    cmd = build_worker_command(
        node_binary=node_binary,
        rpc_port=rpc_port,
        data_dir=data_dir,
        genesis_path=genesis_path,
        keyfile_path=identity.keyfile_path,
        model_path=model_path,
        coordinator_url=coordinator.base_url,
    )
    proc: subprocess.Popen[Any] | None = None
    capture: BoundedPipeLogCapture | None = None
    log_meta: dict[str, Any] = {}
    try:
        started_monotonic = time.monotonic()
        proc = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            env=child_env,
            start_new_session=True,
            close_fds=True,
        )
        assert proc.stdout is not None
        child_pgid = proc.pid

        def abort_on_log_overflow() -> None:
            try:
                os.killpg(child_pgid, signal.SIGTERM)
            except ProcessLookupError:
                pass

        capture = BoundedPipeLogCapture(
            pipe_stream=proc.stdout,
            phase_name="run2",
            on_overflow=abort_on_log_overflow,
        )
        coordinator.attach_log_observer(capture, allow_requests_after_prepared=True)

        first_health_elapsed, first_health = wait_for_initial_loading_health(
            proc=proc,
            capture=capture,
            rpc_port=rpc_port,
            started_monotonic=started_monotonic,
            expected_version=pins.candidate_version,
            expected_binary_sha256=binary_info["sha256"],
            max_loading_health_secs=max_loading_health_secs,
        )
        verified_exe = verify_process_executable(proc.pid, node_binary)
        loading_surface = probe_loading_surface(rpc_port)

        loading_polls = 1
        handoff_health: HttpResult | None = None
        ready_observed_monotonic = 0.0
        deadline = started_monotonic + load_timeout_secs

        while time.monotonic() < deadline:
            if capture.overflowed:
                raise QualificationError(capture.overflow_reason or "Run 2 log overflowed")
            if coordinator.overflowed():
                raise QualificationError("loopback coordinator request log overflowed during Run 2")
            for observed in coordinator.snapshot_requests():
                validate_causal_prepared_request_record(observed)
            rc = proc.poll()
            if rc is not None:
                raise QualificationError(
                    f"Run 2 exited prematurely with code {rc} during model loading"
                )
            poll_result = http_request(rpc_port, "GET", "/health", timeout_secs=5.0)
            if poll_result.status != 200 or not isinstance(poll_result.json_body, dict):
                raise QualificationError(
                    f"Run 2 /health failed across loading/handoff boundary: HTTP {poll_result.status}"
                )
            if poll_result.json_body.get("status") == "loading":
                loading_polls += 1
                time.sleep(0.25)
                continue
            ready_observed_monotonic = time.monotonic()
            handoff_health = poll_result
            break

        if handoff_health is None:
            raise QualificationError(
                f"Run 2 did not finish loading within {load_timeout_secs:.1f}s"
            )

        validate_ready_health(
            handoff_health,
            expected_version=pins.candidate_version,
            expected_binary_sha256=binary_info["sha256"],
        )
        ready_node_info = http_request(rpc_port, "GET", "/node/info", timeout_secs=5.0)
        validate_ready_node_info(
            ready_node_info,
            expected_version=pins.candidate_version,
            expected_address_hex=identity.address_hex,
        )
        ready_readiness = http_request(rpc_port, "GET", "/inference/readiness", timeout_secs=5.0)
        validate_ready_readiness(ready_readiness, pins.model_blake3)
        ready_worker_status = http_request(rpc_port, "GET", WORKER_STATUS_PATH, timeout_secs=5.0)
        validate_ready_worker_status(ready_worker_status, identity.address_hex)

        reg_deadline = time.monotonic() + registration_timeout_secs
        captured_requests: list[dict[str, Any]] = []
        captured_total = 0
        captured_overflowed = False
        while time.monotonic() < reg_deadline:
            if capture.overflowed:
                raise QualificationError(capture.overflow_reason or "Run 2 log overflowed")
            if coordinator.overflowed():
                raise QualificationError("loopback coordinator request log overflowed during Run 2")
            rc = proc.poll()
            if rc is not None:
                raise QualificationError(
                    f"Run 2 exited prematurely with code {rc} while waiting for post-ready registration"
                )
            captured_requests, captured_total, captured_overflowed = coordinator.snapshot_state()
            if any(
                req.get("method") == "POST" and req.get("path") == "/community/register"
                for req in captured_requests
            ):
                break
            time.sleep(0.25)

        if not captured_requests and not captured_overflowed:
            captured_requests, captured_total, captured_overflowed = coordinator.snapshot_state()

        registration_summary = validate_post_ready_registration(
            requests=captured_requests,
            total_requests=captured_total,
            coordinator_overflowed=captured_overflowed,
            expected_address_hex=identity.address_hex,
            expected_model_blake3=pins.model_blake3,
            expected_coordinator_hex=coordinator.coordinator_address_hex,
        )

        sigterm_sent_monotonic = time.monotonic()
        exit_code, forced_kill = terminate_process_group(proc, grace_secs=sigterm_timeout_secs)
        sigterm_exit_elapsed = time.monotonic() - sigterm_sent_monotonic
        proc = None

        pipe_eof = capture.drain_until_eof(timeout_secs=3.0)
        coordinator.attach_log_observer(None, allow_requests_after_prepared=False)
        capture.close(join_timeout_secs=3.0)
        log_meta = capture.write_retained_log(evidence_log_path)

        if capture.overflowed:
            raise QualificationError(capture.overflow_reason or "Run 2 log overflowed")
        if not pipe_eof:
            raise QualificationError(
                "Run 2 stdout pipe did not reach EOF after process exit; descendant kept pipe open"
            )
        if forced_kill:
            raise QualificationError(
                f"Run 2 did not exit cleanly within {sigterm_timeout_secs:.1f}s after SIGTERM"
            )
        if exit_code != 0:
            raise QualificationError(
                f"Run 2 exited with non-zero status {exit_code} after graceful SIGTERM"
            )
        surviving = list_live_pids_in_process_group(child_pgid)
        if process_group_alive(child_pgid) or surviving:
            raise QualificationError(
                f"Run 2 left surviving process-group members after exit: pgid={child_pgid} pids={surviving}"
            )
        verify_loopback_port_released(rpc_port)
        state_after_exit = snapshot_data_dir_state(data_dir)

        seen_markers = capture.seen_markers()
        for marker in RUN2_REQUIRED_LOG_MARKERS:
            if marker not in seen_markers:
                raise QualificationError(
                    f"Run 2 log missing required lifecycle marker: {marker!r}"
                )
        prepared_record = capture.model_prepared_record()
        if prepared_record is None:
            raise QualificationError("Run 2 log lacked canonical model preparation completion marker")

        return (
            {
                "rpc_port": rpc_port,
                "verified_proc_exe": verified_exe,
                "first_loading_health_secs": round(first_health_elapsed, 4),
                "first_loading_health": first_health.json_body,
                "loading_surface": loading_surface,
                "loading_polls_before_handoff": loading_polls,
                "handoff_elapsed_secs": round(ready_observed_monotonic - started_monotonic, 4),
                "model_prepared_record": prepared_record,
                "ready_health": handoff_health.json_body,
                "ready_node_info": ready_node_info.json_body,
                "ready_inference_readiness": ready_readiness.json_body,
                "ready_worker_status": ready_worker_status.json_body,
                "post_ready_registration": registration_summary,
                "sigterm_exit_secs": round(sigterm_exit_elapsed, 4),
                "exit_code": exit_code,
                "forced_kill": forced_kill,
                "pipe_eof": pipe_eof,
                "surviving_pgid_pids": surviving,
                "data_dir_state_after_exit": state_after_exit,
                "required_log_markers_present": list(RUN2_REQUIRED_LOG_MARKERS),
            },
            log_meta,
        )
    finally:
        cleanup_errors: list[str] = []
        coordinator.attach_log_observer(None, allow_requests_after_prepared=False)
        if proc is not None:
            try:
                pgid = proc.pid
                terminate_process_group(proc, grace_secs=5.0)
                surviving_cleanup = list_live_pids_in_process_group(pgid)
                if process_group_alive(pgid) or surviving_cleanup:
                    cleanup_errors.append(
                        f"child process group {pgid} still alive after cleanup (pids={surviving_cleanup})"
                    )
            except CLEANUP_FAILURES as cleanup_err:
                cleanup_errors.append(f"process-group cleanup failed: {cleanup_err}")
        if capture is not None:
            try:
                capture.drain_until_eof(timeout_secs=1.0)
                capture.close(join_timeout_secs=3.0)
            except CLEANUP_FAILURES as cleanup_err:
                cleanup_errors.append(f"bounded log capture cleanup failed: {cleanup_err}")
            try:
                if not evidence_log_path.exists():
                    capture.write_retained_log(evidence_log_path)
            except CLEANUP_FAILURES as cleanup_err:
                cleanup_errors.append(f"retained log write failed: {cleanup_err}")
        if cleanup_errors:
            active_exc = sys.exc_info()[1]
            joined = "; ".join(cleanup_errors)
            if active_exc is None:
                raise QualificationError(joined)
            raise QualificationError(f"{active_exc}; cleanup errors: {joined}") from active_exc


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Remote-only real-model full integer worker startup qualification harness"
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parents[2],
    )
    parser.add_argument("--print-pins-json", action="store_true")
    parser.add_argument("--check-resources", action="store_true")
    parser.add_argument("--disk-path", type=Path, default=None)
    parser.add_argument("--write-resource-report", type=Path, default=None)
    parser.add_argument("--node-binary", type=Path, default=None)
    parser.add_argument("--arc-cli-binary", type=Path, default=None)
    parser.add_argument("--validator-key-file", type=Path, default=None)
    parser.add_argument("--model", type=Path, default=None)
    parser.add_argument("--output-dir", type=Path, default=None)
    parser.add_argument("--private-dir", type=Path, default=None)
    parser.add_argument("--candidate-commit", type=str, default=None)
    parser.add_argument("--require-blocked-ipv4", action="store_true")
    parser.add_argument("--max-loading-health-secs", type=float, default=MAX_LOADING_HEALTH_SECS)
    parser.add_argument("--load-timeout-secs", type=float, default=360.0)
    parser.add_argument("--registration-timeout-secs", type=float, default=30.0)
    parser.add_argument("--sigterm-timeout-secs", type=float, default=30.0)
    parser.add_argument("--post-exit-settle-secs", type=float, default=1.5)
    parser.add_argument("--pre-sigterm-delay-secs", type=float, default=1.0)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    repo_root = args.repo_root.resolve()
    pins = load_repo_pins(repo_root)

    if args.print_pins_json:
        print(
            json.dumps(
                {
                    "candidate_version": pins.candidate_version,
                    "model_file_name": pins.model_file_name,
                    "model_size": pins.model_size,
                    "model_sha256": pins.model_sha256,
                    "model_blake3": pins.model_blake3,
                    "model_url": pins.model_url,
                    "production_ipv4s": list(pins.production_ipv4s),
                    "canonical_execution_profile": pins.canonical_execution_profile,
                },
                indent=2,
                sort_keys=True,
            )
        )
        return 0

    if args.check_resources:
        disk_target = (args.disk_path or repo_root).resolve()
        snapshot = measure_runner_resources(disk_target)
        if args.write_resource_report is not None:
            args.write_resource_report.parent.mkdir(parents=True, exist_ok=True)
            write_json_atomic(args.write_resource_report, snapshot.to_dict())
        enforce_runner_resource_floor(snapshot)
        print(json.dumps(snapshot.to_dict(), indent=2, sort_keys=True))
        return 0

    for required_name, required_val in (
        ("--node-binary", args.node_binary),
        ("--model", args.model),
        ("--output-dir", args.output_dir),
        ("--private-dir", args.private_dir),
    ):
        if required_val is None:
            raise QualificationError(f"{required_name} is required")

    validate_caller_environment(dict(os.environ))

    output_dir = args.output_dir.resolve()
    private_dir = args.private_dir.resolve()
    if output_dir.exists() or output_dir.is_symlink():
        raise QualificationError(f"refusing to reuse existing evidence directory: {output_dir}")
    if output_dir == private_dir or output_dir in private_dir.parents or private_dir in output_dir.parents:
        raise QualificationError(
            "private_dir and output_dir must be disjoint directories so keys never enter artifacts"
        )

    output_dir.mkdir(parents=True, exist_ok=False)
    private_dir.mkdir(parents=True, exist_ok=True)
    os.chmod(private_dir, 0o700)

    isolated_home = private_dir / "home"
    isolated_tmp = private_dir / "tmp"
    data_dir = private_dir / "data"
    isolated_home.mkdir(mode=0o700, exist_ok=True)
    isolated_tmp.mkdir(mode=0o700, exist_ok=True)
    data_dir.mkdir(mode=0o700, exist_ok=True)

    child_env = build_isolated_child_env(dict(os.environ), isolated_home, isolated_tmp)

    summary: dict[str, Any] = {
        "schema": "arc.qualification.worker-startup.v1",
        "started_utc": utc_now(),
        "pass": False,
        "scope_note": (
            "Isolated GitHub runner real-model startup qualification only; "
            "this is pre-release candidate evidence, not deployed fleet activation."
        ),
        "candidate_commit": args.candidate_commit,
        "candidate_version": pins.candidate_version,
        "canonical_execution_profile": pins.canonical_execution_profile,
    }

    identity: PersistentIdentity | None = None
    coordinator: LoopbackCommunityCoordinator | None = None
    run1_evidence_log = output_dir / "run1.log"
    run2_evidence_log = output_dir / "run2.log"

    exit_code = 1
    with TerminationSignalGuard() as signal_guard:
        try:
            resources = measure_runner_resources(args.model.resolve().parent)
            write_json_atomic(output_dir / "resources.json", resources.to_dict())
            enforce_runner_resource_floor(resources)
            summary["resources"] = resources.to_dict()

            binary_info = verify_binary_artifact(args.node_binary, "arc-node candidate binary")
            model_info = verify_model_artifact(args.model, pins)
            genesis_path = (repo_root / "genesis.toml").resolve()
            genesis_info = {
                "path": str(genesis_path),
                "bytes": genesis_path.stat().st_size,
                "sha256": sha256_file(genesis_path),
            }
            provenance = {
                "candidate_commit": args.candidate_commit,
                "candidate_version": pins.candidate_version,
                "arc_node": binary_info,
                "model": model_info,
                "genesis": genesis_info,
            }
            write_json_atomic(output_dir / "provenance.json", provenance)
            summary["provenance"] = provenance

            if args.require_blocked_ipv4:
                blocked_checks = verify_production_ipv4_blocked(pins.production_ipv4s)
                summary["production_ipv4_isolation"] = blocked_checks
            else:
                summary["production_ipv4_isolation"] = "not_probed_by_flag"

            identity = ensure_persistent_keyfile(
                private_dir=private_dir,
                output_dir=output_dir,
                arc_cli_binary=args.arc_cli_binary,
                existing_keyfile=args.validator_key_file,
                child_env=child_env,
            )
            summary["worker_identity"] = {
                "address": identity.address_hex,
                "worker_id": f"0x{identity.address_hex}",
                "public_key": identity.public_key_hex,
            }

            coordinator = LoopbackCommunityCoordinator()
            summary["loopback_coordinator"] = {
                "base_url": coordinator.base_url,
                "coordinator_address": coordinator.coordinator_address_hex,
                "max_captured_requests": coordinator.max_captured_requests,
            }

            run1_report, run1_log_meta, released_port = run_sigterm_during_loading_phase(
                node_binary=args.node_binary.resolve(),
                pins=pins,
                binary_info=binary_info,
                data_dir=data_dir,
                genesis_path=genesis_path,
                identity=identity,
                model_path=args.model.resolve(),
                coordinator=coordinator,
                child_env=child_env,
                evidence_log_path=run1_evidence_log,
                max_loading_health_secs=args.max_loading_health_secs,
                sigterm_timeout_secs=args.sigterm_timeout_secs,
                settle_secs=args.post_exit_settle_secs,
                pre_sigterm_delay_secs=args.pre_sigterm_delay_secs,
            )
            summary["run1_sigterm_during_loading"] = run1_report
            summary["run1_log"] = run1_log_meta

            run2_report, run2_log_meta = run_complete_load_and_handoff_phase(
                node_binary=args.node_binary.resolve(),
                pins=pins,
                binary_info=binary_info,
                data_dir=data_dir,
                genesis_path=genesis_path,
                identity=identity,
                model_path=args.model.resolve(),
                coordinator=coordinator,
                child_env=child_env,
                evidence_log_path=run2_evidence_log,
                rpc_port=released_port,
                max_loading_health_secs=args.max_loading_health_secs,
                load_timeout_secs=args.load_timeout_secs,
                registration_timeout_secs=args.registration_timeout_secs,
                sigterm_timeout_secs=args.sigterm_timeout_secs,
            )
            summary["run2_complete_load_and_handoff"] = run2_report
            summary["run2_log"] = run2_log_meta
            summary["pass"] = True
            exit_code = 0
        except QualificationInterrupted as interrupted:
            summary["pass"] = False
            summary["interrupted"] = True
            summary["interrupted_signal"] = interrupted.signal_name
            summary["interrupted_signum"] = interrupted.signum
            summary["error"] = f"{type(interrupted).__name__}: {interrupted}"
            print(f"qualification interrupted: {summary['error']}", file=sys.stderr)
            exit_code = 128 + interrupted.signum
        except Exception as error:
            summary["pass"] = False
            summary["error"] = f"{type(error).__name__}: {error}"
            print(f"qualification failed: {summary['error']}", file=sys.stderr)
            exit_code = 1
        finally:
            cleanup_errors: list[str] = []
            if coordinator is not None:
                try:
                    req_list, req_total, req_overflow = coordinator.snapshot_state()
                    write_json_atomic(
                        output_dir / "coordinator-requests.json",
                        {
                            "base_url": coordinator.base_url,
                            "total_requests": req_total,
                            "max_captured_requests": coordinator.max_captured_requests,
                            "overflowed": req_overflow,
                            "requests": req_list,
                        },
                    )
                except CLEANUP_FAILURES as err:
                    cleanup_errors.append(f"write coordinator-requests.json failed: {err}")
                try:
                    coordinator.close()
                except CLEANUP_FAILURES as err:
                    cleanup_errors.append(f"coordinator.close failed: {err}")
            if signal_guard.received_signal is not None:
                summary["pass"] = False
                summary["interrupted"] = True
                summary.setdefault("interrupted_signal", signal_guard.received_signal_name)
                summary.setdefault("interrupted_signum", signal_guard.received_signal)
                if exit_code == 0:
                    exit_code = 128 + signal_guard.received_signal
            if cleanup_errors:
                summary["pass"] = False
                summary["cleanup_errors"] = cleanup_errors
                print(
                    f"qualification cleanup failed: {'; '.join(cleanup_errors)}",
                    file=sys.stderr,
                )
                if exit_code == 0:
                    exit_code = 1
            summary["finished_utc"] = utc_now()
            write_json_atomic(output_dir / "summary.json", summary)
            forbidden = (identity.secret_key_hex,) if identity is not None else ()
            assert_no_secrets_in_directory(output_dir, forbidden)
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
