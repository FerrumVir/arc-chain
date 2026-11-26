#!/usr/bin/env python3
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import math
import os
import re
import select
import signal
import stat
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Protocol

PRECOPY_MANIFEST_SCHEMA = "arc.wal-precopy.manifest.v1"
BUNDLE_MANIFEST_SCHEMA = "arc.peer-capture.bundle.v1"
PUBLISH_MANIFEST_SCHEMA = "arc.peer-capture.publish.v1"
CANDIDATE_PREFIX = "peer-recovery-candidate-"
BUNDLE_PREFIX = "peer-capture-bundle-"
PRECOPY_WAL_NAME = "state.wal.precopy"
PRECOPY_MANIFEST_NAME = "state.wal.precopy.manifest.json"
BUNDLE_TAIL_NAME = "state.wal.tail"
BUNDLE_MANIFEST_NAME = "capture.manifest.json"
PUBLISH_MANIFEST_NAME = "state.wal.publish.manifest.json"
DAG_LOCK_NAME = ".WRITE.lock"
MAX_BUNDLE_BYTES = 512 * 1024 * 1024
MAX_BUNDLE_FILES = 128
MAX_MANIFEST_BYTES = 2 * 1024 * 1024
MAX_FREEZE_DEADLINE_S = 8.0
MAX_WATCHDOG_TIMEOUT_S = 10.0
WATCHDOG_READY_TIMEOUT_S = 2.0
DEFAULT_MIN_RESERVE_BYTES = 3 * 1024 * 1024 * 1024
CHUNK_BYTES = 1024 * 1024
HEX64_RE = re.compile(r"\A[0-9a-f]{64}\Z")
ACTIVE_FILE_RE = re.compile(r"\Aactive-[0-9a-f]{64}\.bin\Z")
GEN_DIR_RE = re.compile(r"\Agen-[0-9a-f]{64}\Z")
ROTATED_WAL_RE = re.compile(r"\Awal-[0-9]{8}\.bin\Z")
GEN_ALLOWED_LEAVES = frozenset({"manifest.json", "records.bin"})
WATCHDOG_SCRIPT = (
    "import os,select,sys,time\n"
    "p=sys.argv[1];end=float(sys.argv[2]);wfd=int(sys.argv[3]);pfd=int(sys.argv[4]);parent=int(sys.argv[5])\n"
    "ffd=os.open(p,os.O_WRONLY|getattr(os,'O_NOFOLLOW',0)|getattr(os,'O_CLOEXEC',0))\n"
    "try:\n"
    " os.write(wfd,b'READY\\n')\n"
    " os.close(wfd)\n"
    " wfd=-1\n"
    " while True:\n"
    "  if os.getppid()!=parent:\n"
    "   break\n"
    "  rem=end-time.monotonic()\n"
    "  if rem<=0:\n"
    "   break\n"
    "  r,_,_=select.select([pfd],[],[],min(rem,0.05))\n"
    "  if r:\n"
    "   break\n"
    " while True:\n"
    "  dead=os.getppid()!=parent\n"
    "  os.lseek(ffd,0,os.SEEK_SET)\n"
    "  os.write(ffd,b'0\\n')\n"
    "  if dead:\n"
    "   break\n"
    "  r,_,_=select.select([pfd],[],[],0.05)\n"
    "  if r or os.getppid()!=parent:\n"
    "   os.lseek(ffd,0,os.SEEK_SET)\n"
    "   os.write(ffd,b'0\\n')\n"
    "   break\n"
    "finally:\n"
    " if wfd>=0:\n"
    "  os.close(wfd)\n"
    " os.close(pfd)\n"
    " os.close(ffd)\n"
)


class CaptureError(RuntimeError):
    pass


@dataclass(frozen=True)
class WatchdogHandle:
    proc: subprocess.Popen[bytes]
    pid: int
    freeze_file: Path
    absolute_deadline: float
    pdeath_wfd: int


class FreezeBackend(Protocol):
    def verify_preflight(self) -> dict[str, Any]: ...
    def arm_watchdog(self, absolute_deadline: float) -> Any: ...
    def cancel_watchdog(self, handle: Any) -> None: ...
    def freeze_and_confirm(self, deadline: float, clock: Callable[[], float]) -> None: ...
    def assert_still_frozen(self) -> None: ...
    def thaw_and_confirm(self) -> None: ...


def default_free_space_bytes(path: Path) -> int:
    vfs = os.statvfs(path)
    return int(vfs.f_bavail * vfs.f_frsize)


def require_hex64(val: Any, label: str) -> str:
    if not isinstance(val, str) or not HEX64_RE.match(val):
        raise CaptureError(f"{label} must be 64 lowercase hex characters")
    return val


def require_int_value(val: Any, label: str, lo: int, hi: int | None = None) -> int:
    if type(val) is not int or val < lo or (hi is not None and val > hi):
        raise CaptureError(f"invalid integer {label}")
    return val


def require_int(obj: dict[str, Any], key: str, lo: int, hi: int | None = None) -> int:
    return require_int_value(obj.get(key), f"field {key}", lo, hi)


def require_finite_float_range(val: Any, label: str, lo_exclusive: float, hi_inclusive: float) -> float:
    if type(val) not in (int, float) or isinstance(val, bool):
        raise CaptureError(f"invalid numeric {label}")
    fval = float(val)
    if not math.isfinite(fval) or not (lo_exclusive < fval <= hi_inclusive):
        raise CaptureError(f"{label} must be finite within ({lo_exclusive}, {hi_inclusive}]")
    return fval


def require_no_symlink_ancestors(path: Path) -> None:
    if not path.is_absolute() or ".." in path.parts:
        raise CaptureError(f"path must be absolute without parent traversal: {path}")
    for parent in reversed(path.parents):
        try:
            st = os.lstat(parent)
        except OSError as exc:
            raise CaptureError(f"cannot stat ancestor {parent}: {exc}") from exc
        if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
            raise CaptureError(f"ancestor must be a real non-symlink directory: {parent}")


def require_disjoint_paths(left: Path, left_label: str, right: Path, right_label: str) -> None:
    r_left = left.parent.resolve() / left.name if not os.path.lexists(left) else left.resolve()
    r_right = right.parent.resolve() / right.name if not os.path.lexists(right) else right.resolve()
    if r_left == r_right or r_left.is_relative_to(r_right) or r_right.is_relative_to(r_left):
        raise CaptureError(f"{left_label} and {right_label} must not overlap or nest")


def require_real_dir(path: Path, label: str) -> os.stat_result:
    require_no_symlink_ancestors(path)
    try:
        st = os.lstat(path)
    except OSError as exc:
        raise CaptureError(f"cannot stat {label}: {exc}") from exc
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
        raise CaptureError(f"{label} must be a real non-symlink directory")
    return st


def require_regular_single_link(path: Path, label: str) -> os.stat_result:
    require_no_symlink_ancestors(path)
    try:
        st = os.lstat(path)
    except OSError as exc:
        raise CaptureError(f"cannot stat {label}: {exc}") from exc
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISREG(st.st_mode):
        raise CaptureError(f"{label} must be a regular non-symlink file")
    if st.st_nlink != 1:
        raise CaptureError(f"{label} must have exactly one hard link")
    return st


def open_readonly_nofollow(path: Path, label: str) -> tuple[int, os.stat_result]:
    st_path = require_regular_single_link(path, label)
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NONBLOCK", 0)
    try:
        fd = os.open(path, flags)
    except OSError as exc:
        raise CaptureError(f"failed to open {label}: {exc}") from exc
    st_fd = os.fstat(fd)
    if (
        not stat.S_ISREG(st_fd.st_mode)
        or st_fd.st_nlink != 1
        or (st_fd.st_dev, st_fd.st_ino) != (st_path.st_dev, st_path.st_ino)
    ):
        os.close(fd)
        raise CaptureError(f"{label} identity changed during open")
    return fd, st_fd


def stat_tuple(st: os.stat_result) -> tuple[int, int, int, int, int, int]:
    return (st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns, st.st_ctime_ns, st.st_nlink)


def write_all(fd: int, data: bytes) -> None:
    view = memoryview(data)
    written = 0
    while written < len(view):
        n = os.write(fd, view[written:])
        if n <= 0:
            raise CaptureError("short write")
        written += n


def fsync_dir(path: Path) -> None:
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    fd = os.open(path, flags)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def write_exclusive_json(path: Path, payload: dict[str, Any]) -> None:
    raw = (json.dumps(payload, indent=2, sort_keys=True) + "\n").encode("utf-8")
    if os.path.lexists(path):
        raise CaptureError(f"JSON destination already exists: {path.name}")
    partial = path.with_name(path.name + ".partial")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    try:
        fd = os.open(partial, flags, 0o600)
    except FileExistsError as exc:
        raise CaptureError(f"partial JSON file already exists: {partial.name}") from exc
    linked = False
    try:
        try:
            os.fchmod(fd, 0o600)
            write_all(fd, raw)
            os.fsync(fd)
        finally:
            os.close(fd)
        try:
            os.link(partial, path, follow_symlinks=False)
            linked = True
        except FileExistsError as exc:
            raise CaptureError(f"JSON destination already exists: {path.name}") from exc
        partial.unlink()
    except BaseException:
        if not linked:
            try:
                partial.unlink()
            except OSError:
                pass
        raise


def read_strict_json_file(path: Path, label: str, max_bytes: int = MAX_MANIFEST_BYTES) -> dict[str, Any]:
    fd, st = open_readonly_nofollow(path, label)
    try:
        if st.st_size <= 0 or st.st_size > max_bytes:
            raise CaptureError(f"{label} size {st.st_size} outside 1..={max_bytes}")
        chunks: list[bytes] = []
        total = 0
        while True:
            chunk = os.read(fd, min(CHUNK_BYTES, max_bytes - total + 1))
            if not chunk:
                break
            total += len(chunk)
            if total > max_bytes:
                raise CaptureError(f"{label} exceeded {max_bytes} bytes")
            chunks.append(chunk)
    finally:
        os.close(fd)

    def no_dups(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        out: dict[str, Any] = {}
        for k, v in pairs:
            if k in out:
                raise CaptureError(f"{label} duplicate JSON key {k!r}")
            out[k] = v
        return out

    try:
        obj = json.loads(b"".join(chunks).decode("utf-8"), object_pairs_hook=no_dups)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise CaptureError(f"invalid {label} JSON: {exc}") from exc
    if not isinstance(obj, dict):
        raise CaptureError(f"{label} must be a JSON object")
    return obj


def terminate_owned_process(proc: subprocess.Popen[bytes]) -> None:
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=2.0)


def spawn_setsid_thaw_watchdog(
    freeze_file: Path,
    absolute_deadline: float,
    *,
    ready_timeout_s: float = WATCHDOG_READY_TIMEOUT_S,
) -> WatchdogHandle:
    require_regular_single_link(freeze_file, "freeze_file")
    if type(absolute_deadline) not in (int, float) or isinstance(absolute_deadline, bool) or not math.isfinite(absolute_deadline):
        raise CaptureError("watchdog absolute_deadline must be finite")
    remaining_budget = float(absolute_deadline) - time.monotonic()
    if not 0 < remaining_budget <= MAX_WATCHDOG_TIMEOUT_S:
        raise CaptureError("watchdog absolute_deadline out of bounds")
    require_finite_float_range(ready_timeout_s, "ready_timeout_s", 0.0, 5.0)
    r_fd: int | None = None
    w_fd: int | None = None
    pdeath_rfd: int | None = None
    pdeath_wfd: int | None = None
    proc: subprocess.Popen[bytes] | None = None
    transferred_pdeath_wfd = False
    try:
        try:
            r_fd, w_fd = os.pipe()
            os.set_inheritable(r_fd, False)
            os.set_inheritable(w_fd, True)
            pdeath_rfd, pdeath_wfd = os.pipe()
            os.set_inheritable(pdeath_rfd, True)
            os.set_inheritable(pdeath_wfd, False)
        except OSError as exc:
            raise CaptureError(f"failed to create watchdog pipes: {exc}") from exc
        try:
            proc = subprocess.Popen(
                [
                    sys.executable,
                    "-I",
                    "-S",
                    "-c",
                    WATCHDOG_SCRIPT,
                    str(freeze_file),
                    repr(float(absolute_deadline)),
                    str(w_fd),
                    str(pdeath_rfd),
                    str(os.getpid()),
                ],
                start_new_session=True,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                close_fds=True,
                pass_fds=(w_fd, pdeath_rfd),
            )
        except OSError as exc:
            raise CaptureError(f"failed to spawn independent setsid watchdog: {exc}") from exc
        fd_close = w_fd
        w_fd = None
        os.close(fd_close)
        fd_close = pdeath_rfd
        pdeath_rfd = None
        os.close(fd_close)
        ready_buf = bytearray()
        ready_deadline = time.monotonic() + ready_timeout_s
        while len(ready_buf) < 6:
            wait = ready_deadline - time.monotonic()
            if wait <= 0:
                raise CaptureError("independent setsid watchdog ready handshake timed out")
            readable, _, _ = select.select([r_fd], [], [], wait)
            if not readable:
                raise CaptureError("independent setsid watchdog ready handshake timed out")
            try:
                chunk = os.read(r_fd, 6 - len(ready_buf))
            except OSError as exc:
                raise CaptureError(f"failed reading independent setsid watchdog ready pipe: {exc}") from exc
            if not chunk:
                raise CaptureError("independent setsid watchdog exited before signaling ready")
            ready_buf.extend(chunk)
        if bytes(ready_buf) != b"READY\n" or proc.poll() is not None or proc.pid <= 0:
            raise CaptureError("independent setsid watchdog failed ready verification")
        handle = WatchdogHandle(
            proc=proc,
            pid=proc.pid,
            freeze_file=freeze_file,
            absolute_deadline=float(absolute_deadline),
            pdeath_wfd=pdeath_wfd,
        )
        transferred_pdeath_wfd = True
        return handle
    except BaseException:
        if proc is not None:
            terminate_owned_process(proc)
        raise
    finally:
        for fd in (r_fd, w_fd, pdeath_rfd):
            if fd is not None:
                os.close(fd)
        if pdeath_wfd is not None and not transferred_pdeath_wfd:
            os.close(pdeath_wfd)


def inspect_systemd_unit(unit_name: str) -> dict[str, str]:
    if not unit_name or unit_name.startswith("-") or any(ch.isspace() or ord(ch) < 32 for ch in unit_name):
        raise CaptureError("invalid systemd unit name")
    try:
        proc = subprocess.run(
            [
                "systemctl",
                "show",
                "--no-pager",
                "-p",
                "ControlGroup",
                "-p",
                "MainPID",
                "-p",
                "ActiveState",
                "-p",
                "WatchdogUSec",
                "--",
                unit_name,
            ],
            capture_output=True,
            text=True,
            check=False,
            timeout=5.0,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        raise CaptureError(f"failed to inspect systemd unit {unit_name}: {exc}") from exc
    if proc.returncode != 0:
        raise CaptureError(f"systemctl show failed for {unit_name}: {proc.stderr.strip()}")
    props: dict[str, str] = {}
    for line in proc.stdout.splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            props[key] = value
    return props


class CgroupV2FreezeBackend:
    def __init__(
        self,
        *,
        unit_name: str,
        expected_service_cgroup: str,
        expected_main_pid: int,
        expected_service_watchdog_usec: int,
        cgroup_root: Path = Path("/sys/fs/cgroup"),
        service_inspector: Callable[[str], dict[str, str]] = inspect_systemd_unit,
        poll_sleep: Callable[[float], None] = time.sleep,
        proc_cgroup_reader: Callable[[int], str] | None = None,
    ) -> None:
        if not unit_name or unit_name.startswith("-") or any(ch.isspace() for ch in unit_name):
            raise CaptureError("invalid unit_name")
        if (
            not isinstance(expected_service_cgroup, str)
            or not expected_service_cgroup.startswith("/")
            or len(expected_service_cgroup) <= 1
            or "//" in expected_service_cgroup
            or expected_service_cgroup.endswith("/")
            or any(ord(ch) < 32 for ch in expected_service_cgroup)
            or any(part in (".", "..") for part in expected_service_cgroup.split("/"))
        ):
            raise CaptureError("invalid expected_service_cgroup")
        self.unit_name = unit_name
        self.expected_service_cgroup = expected_service_cgroup
        self.expected_main_pid = require_int_value(expected_main_pid, "expected_main_pid", 2)
        self.expected_service_watchdog_usec = require_int_value(
            expected_service_watchdog_usec, "expected_service_watchdog_usec", 0
        )
        self.cgroup_root = cgroup_root
        self.cgroup_dir = cgroup_root / expected_service_cgroup.lstrip("/")
        self.service_inspector = service_inspector
        self.poll_sleep = poll_sleep
        self.freeze_file = self.cgroup_dir / "cgroup.freeze"
        self.events_file = self.cgroup_dir / "cgroup.events"
        self.procs_file = self.cgroup_dir / "cgroup.procs"
        self.proc_cgroup_reader = proc_cgroup_reader
        self._owned_watchdogs: dict[int, tuple[WatchdogHandle, subprocess.Popen[bytes], int, Path, int]] = {}

    def _read_small(self, path: Path, label: str, max_bytes: int = 4096) -> str:
        require_int_value(max_bytes, "max_bytes", 1)
        fd, _ = open_readonly_nofollow(path, label)
        try:
            chunks: list[bytes] = []
            total = 0
            while True:
                chunk = os.read(fd, min(CHUNK_BYTES, max_bytes - total + 1))
                if not chunk:
                    break
                total += len(chunk)
                if total > max_bytes:
                    raise CaptureError(f"{label} exceeded {max_bytes} bytes")
                chunks.append(chunk)
            return b"".join(chunks).decode("utf-8", errors="strict")
        except (OSError, UnicodeDecodeError) as exc:
            raise CaptureError(f"failed to read {label}: {exc}") from exc
        finally:
            os.close(fd)

    def _write_flag(self, value: bytes) -> None:
        st_path = require_regular_single_link(self.freeze_file, "cgroup.freeze")
        try:
            fd = os.open(
                self.freeze_file,
                os.O_WRONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0),
            )
        except OSError as exc:
            raise CaptureError(f"failed to open cgroup.freeze: {exc}") from exc
        try:
            st_fd = os.fstat(fd)
            if (
                not stat.S_ISREG(st_fd.st_mode)
                or st_fd.st_nlink != 1
                or (st_fd.st_dev, st_fd.st_ino) != (st_path.st_dev, st_path.st_ino)
            ):
                raise CaptureError("cgroup.freeze identity changed during open")
            write_all(fd, value)
        except OSError as exc:
            raise CaptureError(f"failed to write cgroup.freeze: {exc}") from exc
        finally:
            os.close(fd)

    def _events_frozen(self) -> int:
        for line in self._read_small(self.events_file, "cgroup.events").splitlines():
            parts = line.split()
            if len(parts) == 2 and parts[0] == "frozen" and parts[1] in ("0", "1"):
                return int(parts[1])
        raise CaptureError("cgroup.events missing frozen state")

    def _read_cgroup_procs(self) -> set[int]:
        raw_procs = self._read_small(self.procs_file, "cgroup.procs", max_bytes=65536)
        try:
            return {int(line.strip()) for line in raw_procs.splitlines() if line.strip()}
        except ValueError as exc:
            raise CaptureError(f"invalid cgroup.procs contents: {exc}") from exc

    @staticmethod
    def _parse_proc_cgroup_v2(raw: str, label: str) -> Path:
        if not isinstance(raw, str):
            raise CaptureError(f"invalid {label} cgroup data")
        matches = [line[3:] for line in raw.splitlines() if line.startswith("0::")]
        if len(matches) != 1:
            raise CaptureError(f"{label} missing unique cgroupv2 0:: entry")
        cg_path = matches[0]
        if (
            not cg_path.startswith("/")
            or "//" in cg_path
            or (len(cg_path) > 1 and cg_path.endswith("/"))
            or any(ord(ch) < 32 for ch in cg_path)
            or any(part in (".", "..") for part in cg_path.split("/"))
        ):
            raise CaptureError(f"invalid {label} cgroupv2 path: {cg_path!r}")
        return Path(cg_path)

    def _read_proc_cgroup_raw(self, pid: int) -> str:
        require_int_value(pid, "pid", 1)
        if self.proc_cgroup_reader is not None:
            return self.proc_cgroup_reader(pid)
        return self._read_small(Path(f"/proc/{pid}/cgroup"), f"/proc/{pid}/cgroup", max_bytes=4096)

    def _assert_pid_outside_target_cgroup(self, pid: int, label: str) -> str:
        try:
            raw = self._read_proc_cgroup_raw(pid)
        except OSError as exc:
            raise CaptureError(f"failed to read {label} cgroup: {exc}") from exc
        cg = self._parse_proc_cgroup_v2(raw, label)
        target = Path(self.expected_service_cgroup)
        if cg.is_relative_to(target):
            raise CaptureError(f"{label} cgroup {cg} is inside target cgroup {target}; refusing self-freeze")
        return str(cg)

    def verify_preflight(self) -> dict[str, Any]:
        if self.expected_service_watchdog_usec != 0:
            raise CaptureError("service WatchdogUSec must be 0 before freeze")
        if self.expected_main_pid == os.getpid():
            raise CaptureError("expected_main_pid must not equal helper PID")
        helper_cgroup = self._assert_pid_outside_target_cgroup(os.getpid(), "helper")
        require_real_dir(self.cgroup_root, "cgroup_root")
        require_real_dir(self.cgroup_dir, "cgroup_dir")
        if not self.cgroup_dir.resolve().is_relative_to(self.cgroup_root.resolve()):
            raise CaptureError("cgroup_dir escaped cgroup_root")
        props = self.service_inspector(self.unit_name)
        if props.get("ActiveState") != "active":
            raise CaptureError(f"service {self.unit_name} ActiveState is not active")
        if props.get("ControlGroup") != self.expected_service_cgroup:
            raise CaptureError("service ControlGroup differs from expected_service_cgroup")
        if props.get("MainPID") != str(self.expected_main_pid):
            raise CaptureError("service MainPID differs from expected_main_pid")
        if props.get("WatchdogUSec") not in ("0", "0s", "0us"):
            raise CaptureError(f"live service WatchdogUSec is non-zero: {props.get('WatchdogUSec')!r}")
        pids = self._read_cgroup_procs()
        if self.expected_main_pid not in pids:
            raise CaptureError("expected_main_pid not present in cgroup.procs")
        if os.getpid() in pids:
            raise CaptureError("helper PID is in target cgroup.procs; refusing self-freeze")
        freeze_val = self._read_small(self.freeze_file, "cgroup.freeze").strip()
        events_val = self._events_frozen()
        if freeze_val != "0" or events_val != 0:
            raise CaptureError(f"cgroup already frozen (freeze={freeze_val}, events={events_val})")
        return {
            "unit_name": self.unit_name,
            "cgroup_dir": str(self.cgroup_dir),
            "helper_cgroup": helper_cgroup,
            "main_pid": self.expected_main_pid,
            "freeze": 0,
            "events_frozen": 0,
            "watchdog_usec": 0,
        }

    def arm_watchdog(self, absolute_deadline: float) -> WatchdogHandle:
        handle = spawn_setsid_thaw_watchdog(self.freeze_file, absolute_deadline)
        try:
            self._assert_pid_outside_target_cgroup(handle.pid, "watchdog")
            if handle.pid == self.expected_main_pid:
                raise CaptureError("watchdog PID equals expected_main_pid")
        except BaseException:
            try:
                terminate_owned_process(handle.proc)
            finally:
                try:
                    os.close(handle.pdeath_wfd)
                except OSError:
                    pass
            raise
        self._owned_watchdogs[id(handle)] = (
            handle,
            handle.proc,
            handle.pid,
            handle.freeze_file,
            handle.pdeath_wfd,
        )
        return handle

    def cancel_watchdog(self, handle: Any) -> None:
        if not isinstance(handle, WatchdogHandle):
            raise CaptureError("refusing to cancel unowned watchdog handle")
        entry = self._owned_watchdogs.get(id(handle))
        if entry is None:
            raise CaptureError("refusing to cancel unowned watchdog handle")
        owned_handle, owned_proc, owned_pid, owned_freeze_file, owned_pdeath_wfd = entry
        if (
            owned_handle is not handle
            or handle.proc is not owned_proc
            or handle.pid != owned_pid
            or handle.proc.pid != owned_pid
            or handle.freeze_file != owned_freeze_file
            or handle.freeze_file != self.freeze_file
            or handle.pdeath_wfd != owned_pdeath_wfd
        ):
            raise CaptureError("refusing to cancel tampered watchdog handle")
        self._owned_watchdogs.pop(id(handle), None)
        try:
            terminate_owned_process(owned_proc)
        finally:
            try:
                os.close(owned_pdeath_wfd)
            except OSError:
                pass

    def freeze_and_confirm(self, deadline: float, clock: Callable[[], float]) -> None:
        if clock() >= deadline:
            raise CaptureError("capture deadline elapsed before freeze request")
        self._write_flag(b"1\n")
        while clock() <= deadline:
            if self._read_small(self.freeze_file, "cgroup.freeze").strip() == "1" and self._events_frozen() == 1:
                if self.expected_main_pid not in self._read_cgroup_procs():
                    raise CaptureError("expected_main_pid not present in cgroup.procs after freeze")
                return
            self.poll_sleep(0.02)
        raise CaptureError("cgroup freeze was not acknowledged before deadline")

    def assert_still_frozen(self) -> None:
        if self._read_small(self.freeze_file, "cgroup.freeze").strip() != "1" or self._events_frozen() != 1:
            raise CaptureError("cgroup did not remain frozen through end of capture")
        if self.expected_main_pid not in self._read_cgroup_procs():
            raise CaptureError("expected_main_pid disappeared from cgroup.procs while frozen")

    def thaw_and_confirm(self) -> None:
        self._write_flag(b"0\n")
        for _ in range(150):
            if self._read_small(self.freeze_file, "cgroup.freeze").strip() == "0" and self._events_frozen() == 0:
                return
            self.poll_sleep(0.02)
        raise CaptureError("cgroup thaw was not acknowledged")


def mandatory_bundle_relpaths(manifest_hash: str) -> set[str]:
    return {
        "recovery.active",
        "genesis.network-hash",
        f"recovery-{manifest_hash}.arcchkpt",
        f"recovery-dag.{manifest_hash}.pin.json",
        f"dag-wal-recovery-{manifest_hash}/CURRENT",
    }


def is_allowed_bundle_relpath(relpath: str, manifest_hash: str) -> bool:
    if (
        not isinstance(relpath, str)
        or not relpath
        or "\x00" in relpath
        or "\\" in relpath
        or "//" in relpath
        or relpath.startswith("/")
        or relpath.endswith("/")
    ):
        return False
    parsed = Path(relpath)
    if parsed.is_absolute() or str(parsed) != relpath or "/".join(parsed.parts) != relpath:
        return False
    if any(part in ("", ".", "..") for part in parsed.parts):
        return False
    dag_root = f"dag-wal-recovery-{manifest_hash}"
    exact_top = {
        "recovery.active",
        "genesis.network-hash",
        f"recovery-{manifest_hash}.arcchkpt",
        f"recovery-dag.{manifest_hash}.pin.json",
    }
    if relpath in exact_top:
        return True
    parts = parsed.parts
    if parts[0] != dag_root:
        return False
    if len(parts) == 2:
        name = parts[1]
        return name in {"CURRENT", "GC-ANCHOR.json", "recovery-dag.binding.json"} or bool(ACTIVE_FILE_RE.match(name))
    if len(parts) == 3 and GEN_DIR_RE.match(parts[1]):
        return parts[2] in GEN_ALLOWED_LEAVES
    return False


def collect_allowlisted_source_files(source_dir: Path, manifest_hash: str) -> list[str]:
    with os.scandir(source_dir) as entries:
        for count, entry in enumerate(entries, 1):
            if count > 4096:
                raise CaptureError("source directory exceeds bounded entry inventory")
            if ROTATED_WAL_RE.match(entry.name):
                raise CaptureError(f"rotated WAL segment present in source dir: {entry.name}")
    dag_root = f"dag-wal-recovery-{manifest_hash}"
    dag_dir = source_dir / dag_root
    require_real_dir(dag_dir, "source recovery DAG directory")
    relpaths = [
        "recovery.active",
        "genesis.network-hash",
        f"recovery-{manifest_hash}.arcchkpt",
        f"recovery-dag.{manifest_hash}.pin.json",
    ]
    has_current = False
    with os.scandir(dag_dir) as entries:
        dag_entries = []
        for entry in entries:
            dag_entries.append(entry)
            if len(dag_entries) > MAX_BUNDLE_FILES:
                raise CaptureError("source recovery DAG directory exceeds bounded entry inventory")
        dag_entries.sort(key=lambda e: e.name)
    for entry in dag_entries:
        if entry.name == DAG_LOCK_NAME:
            continue
        est = os.lstat(entry.path)
        if stat.S_ISLNK(est.st_mode):
            raise CaptureError(f"symlink inside recovery DAG directory: {entry.name}")
        if stat.S_ISDIR(est.st_mode):
            if not GEN_DIR_RE.match(entry.name):
                raise CaptureError(f"unexpected subdirectory in recovery DAG dir: {entry.name}")
            with os.scandir(entry.path) as entries:
                gen_entries = []
                for leaf in entries:
                    gen_entries.append(leaf)
                    if len(gen_entries) > len(GEN_ALLOWED_LEAVES):
                        raise CaptureError(f"unexpected extra generation files in {entry.name}")
                gen_entries.sort(key=lambda e: e.name)
            if {leaf.name for leaf in gen_entries} != GEN_ALLOWED_LEAVES:
                raise CaptureError(f"generation {entry.name} must contain manifest.json and records.bin")
            for leaf in gen_entries:
                lst = os.lstat(leaf.path)
                if stat.S_ISLNK(lst.st_mode) or not stat.S_ISREG(lst.st_mode) or lst.st_nlink != 1:
                    raise CaptureError(f"invalid entry in {entry.name}: {leaf.name}")
                rel = f"{dag_root}/{entry.name}/{leaf.name}"
                if not is_allowed_bundle_relpath(rel, manifest_hash):
                    raise CaptureError(f"disallowed file in {entry.name}: {leaf.name}")
                relpaths.append(rel)
        elif stat.S_ISREG(est.st_mode):
            if est.st_nlink != 1:
                raise CaptureError(f"hardlinked file in recovery DAG dir: {entry.name}")
            rel = f"{dag_root}/{entry.name}"
            if not is_allowed_bundle_relpath(rel, manifest_hash):
                raise CaptureError(f"unexpected file in recovery DAG dir: {entry.name}")
            if entry.name == "CURRENT":
                has_current = True
            relpaths.append(rel)
        else:
            raise CaptureError(f"non-regular entry in recovery DAG dir: {entry.name}")
        if len(relpaths) > MAX_BUNDLE_FILES - 1:
            raise CaptureError(f"source allowlist exceeds {MAX_BUNDLE_FILES} total files")
    if not has_current:
        raise CaptureError("source recovery DAG directory missing CURRENT")
    return relpaths


def copy_range_checked(
    src_path: Path,
    dst_path: Path,
    *,
    offset: int,
    length: int,
    deadline: float,
    clock: Callable[[], float],
    budget_remaining: int,
    expected_st: os.stat_result,
) -> tuple[str, os.stat_result]:
    if length < 0 or length > budget_remaining:
        raise CaptureError("capture bundle exceeds 512 MiB limit")
    src_fd, st_before = open_readonly_nofollow(src_path, str(src_path.name))
    if stat_tuple(st_before) != stat_tuple(expected_st) or offset + length != st_before.st_size:
        os.close(src_fd)
        raise CaptureError(f"{src_path.name} identity or exact size changed before capture")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    dst_fd = None
    hasher = hashlib.sha256()
    try:
        dst_fd = os.open(dst_path, flags, 0o600)
        os.fchmod(dst_fd, 0o600)
        if offset:
            os.lseek(src_fd, offset, os.SEEK_SET)
        remaining = length
        while remaining > 0:
            if clock() > deadline:
                raise CaptureError("capture deadline exceeded during copy")
            chunk = os.read(src_fd, min(CHUNK_BYTES, remaining))
            if not chunk:
                raise CaptureError(f"{src_path.name} truncated during copy")
            write_all(dst_fd, chunk)
            hasher.update(chunk)
            remaining -= len(chunk)
        st_fd_after = os.fstat(src_fd)
        st_path_after = require_regular_single_link(src_path, str(src_path.name))
        if stat_tuple(st_fd_after) != stat_tuple(st_before) or stat_tuple(st_path_after) != stat_tuple(st_before):
            raise CaptureError(f"{src_path.name} changed while frozen")
        return hasher.hexdigest(), st_before
    finally:
        if dst_fd is not None:
            os.close(dst_fd)
        os.close(src_fd)


def capture_source_tail_bundle(
    *,
    source_data_dir: Path,
    bundle_dir: Path,
    recovery_manifest_hash: str,
    precopy_prefix_bytes: int,
    precopy_source_dev: int,
    precopy_source_ino: int,
    freeze_backend: FreezeBackend,
    deadline_s: float = MAX_FREEZE_DEADLINE_S,
    clock: Callable[[], float] = time.monotonic,
    free_space_fn: Callable[[Path], int] = default_free_space_bytes,
    max_bundle_bytes: int = MAX_BUNDLE_BYTES,
    min_reserve_bytes: int = DEFAULT_MIN_RESERVE_BYTES,
    on_after_file_copy: Callable[[str], None] | None = None,
) -> dict[str, Any]:
    manifest_hash = require_hex64(recovery_manifest_hash, "recovery_manifest_hash")
    precopy_prefix_bytes = require_int_value(precopy_prefix_bytes, "precopy_prefix_bytes", 1)
    precopy_source_dev = require_int_value(precopy_source_dev, "precopy_source_dev", 0)
    precopy_source_ino = require_int_value(precopy_source_ino, "precopy_source_ino", 0)
    max_bundle_bytes = require_int_value(max_bundle_bytes, "max_bundle_bytes", 1, MAX_BUNDLE_BYTES)
    min_reserve_bytes = require_int_value(min_reserve_bytes, "min_reserve_bytes", 1)
    deadline_s = require_finite_float_range(deadline_s, "deadline_s", 0.0, MAX_FREEZE_DEADLINE_S)
    require_real_dir(source_data_dir, "source_data_dir")
    if not bundle_dir.name.startswith(BUNDLE_PREFIX) or len(bundle_dir.name) <= len(BUNDLE_PREFIX):
        raise CaptureError(f"bundle_dir basename must start with {BUNDLE_PREFIX!r}")
    require_no_symlink_ancestors(bundle_dir)
    if os.path.lexists(bundle_dir):
        raise CaptureError("bundle_dir already exists")
    require_disjoint_paths(source_data_dir, "source_data_dir", bundle_dir, "bundle_dir")
    if free_space_fn(bundle_dir.parent) < max_bundle_bytes + MAX_MANIFEST_BYTES + min_reserve_bytes:
        raise CaptureError("insufficient free space for bounded capture bundle")
    preflight = freeze_backend.verify_preflight()
    os.mkdir(bundle_dir, 0o700)
    os.chmod(bundle_dir, 0o700)
    watchdog_timeout = min(deadline_s + 2.0, MAX_WATCHDOG_TIMEOUT_S)
    watchdog_deadline = clock() + watchdog_timeout
    watchdog_handle: Any = None
    freeze_attempted = False
    thawed_confirmed = False
    in_finally = False
    prior_handlers: list[tuple[int, Any]] = []

    def _handle_term_signal(signum: int, _frame: Any) -> None:
        if not in_finally:
            raise CaptureError(f"capture interrupted by signal {signum}")

    copied_entries: list[dict[str, Any]] = []
    recorded_stats: dict[str, tuple[int, int, int, int, int, int]] = {}
    created_dirs: set[Path] = {bundle_dir}
    try:
        for sig_name in ("SIGTERM", "SIGHUP"):
            sig = getattr(signal, sig_name, None)
            if sig is not None:
                try:
                    prior_handlers.append((sig, signal.signal(sig, _handle_term_signal)))
                except ValueError:
                    pass
        watchdog_handle = freeze_backend.arm_watchdog(watchdog_deadline)
        now = clock()
        deadline = min(now + deadline_s, watchdog_deadline - 1.0)
        if deadline <= now:
            raise CaptureError("watchdog startup consumed capture deadline budget")
        freeze_attempted = True
        freeze_backend.freeze_and_confirm(deadline, clock)
        wal_path = source_data_dir / "state.wal"
        wal_st = require_regular_single_link(wal_path, "source state.wal")
        if (wal_st.st_dev, wal_st.st_ino) != (precopy_source_dev, precopy_source_ino):
            raise CaptureError("source state.wal dev/ino differs from precopy receipt")
        if wal_st.st_size < precopy_prefix_bytes:
            raise CaptureError("source state.wal shrank below precopy_prefix_bytes")
        tail_bytes = wal_st.st_size - precopy_prefix_bytes
        budget = max_bundle_bytes
        tail_sha, wal_st_verified = copy_range_checked(
            wal_path,
            bundle_dir / BUNDLE_TAIL_NAME,
            offset=precopy_prefix_bytes,
            length=tail_bytes,
            deadline=deadline,
            clock=clock,
            budget_remaining=budget,
            expected_st=wal_st,
        )
        budget -= tail_bytes
        relpaths = collect_allowlisted_source_files(source_data_dir, manifest_hash)
        for rel in relpaths:
            src_file = source_data_dir / rel
            dst_file = bundle_dir / rel
            for d in reversed(dst_file.relative_to(bundle_dir).parents):
                if str(d) != ".":
                    target_d = bundle_dir / d
                    if target_d not in created_dirs:
                        os.mkdir(target_d, 0o700)
                        os.chmod(target_d, 0o700)
                        created_dirs.add(target_d)
            st_item = require_regular_single_link(src_file, rel)
            digest, st_verified = copy_range_checked(
                src_file,
                dst_file,
                offset=0,
                length=st_item.st_size,
                deadline=deadline,
                clock=clock,
                budget_remaining=budget,
                expected_st=st_item,
            )
            budget -= st_item.st_size
            recorded_stats[rel] = stat_tuple(st_verified)
            copied_entries.append(
                {
                    "relpath": rel,
                    "size": st_verified.st_size,
                    "sha256": digest,
                    "source_dev": st_verified.st_dev,
                    "source_ino": st_verified.st_ino,
                    "source_mtime_ns": st_verified.st_mtime_ns,
                    "source_ctime_ns": st_verified.st_ctime_ns,
                }
            )
            if on_after_file_copy is not None:
                on_after_file_copy(rel)
        if clock() > deadline:
            raise CaptureError("capture deadline exceeded before stability check")
        freeze_backend.assert_still_frozen()
        if collect_allowlisted_source_files(source_data_dir, manifest_hash) != relpaths:
            raise CaptureError("source allowlisted file set changed while frozen")
        wal_st_final = require_regular_single_link(wal_path, "source state.wal")
        if stat_tuple(wal_st_final) != stat_tuple(wal_st_verified):
            raise CaptureError("source state.wal changed before thaw")
        for rel, expected_st in recorded_stats.items():
            if stat_tuple(require_regular_single_link(source_data_dir / rel, rel)) != expected_st:
                raise CaptureError(f"source file {rel} changed while frozen")
        if clock() > deadline:
            raise CaptureError("capture deadline exceeded before thaw")
    finally:
        in_finally = True
        try:
            if freeze_attempted:
                try:
                    freeze_backend.thaw_and_confirm()
                    thawed_confirmed = True
                except BaseException as exc:
                    if watchdog_handle is not None:
                        try:
                            setattr(exc, "watchdog_handle", watchdog_handle)
                        except (AttributeError, TypeError):
                            pass
                    raise
            if watchdog_handle is not None and (thawed_confirmed or not freeze_attempted):
                freeze_backend.cancel_watchdog(watchdog_handle)
        finally:
            for sig, prev in reversed(prior_handlers):
                try:
                    signal.signal(sig, prev)
                except ValueError:
                    pass
    validate_recovery_active(bundle_dir / "recovery.active", manifest_hash)
    validate_genesis_network_hash(bundle_dir / "genesis.network-hash")
    read_strict_json_file(bundle_dir / f"recovery-dag.{manifest_hash}.pin.json", "recovery DAG pin")
    for rel_file in [BUNDLE_TAIL_NAME] + [item["relpath"] for item in copied_entries]:
        fd = os.open(bundle_dir / rel_file, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0))
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    for d in sorted(created_dirs, key=lambda p: len(p.parts), reverse=True):
        fsync_dir(d)
    bundle_manifest = {
        "schema": BUNDLE_MANIFEST_SCHEMA,
        "status": "CAPTURE_BUNDLE_UNQUALIFIED",
        "qualified_for_recovery": False,
        "certified_for_activation": False,
        "recovery_manifest_hash": manifest_hash,
        "source_dev": precopy_source_dev,
        "source_ino": precopy_source_ino,
        "prefix_bytes": precopy_prefix_bytes,
        "source_frozen_size": precopy_prefix_bytes + tail_bytes,
        "tail_bytes": tail_bytes,
        "tail_sha256": tail_sha,
        "total_bundle_bytes": max_bundle_bytes - budget,
        "freeze_preflight": preflight,
        "files": copied_entries,
    }
    write_exclusive_json(bundle_dir / BUNDLE_MANIFEST_NAME, bundle_manifest)
    fsync_dir(bundle_dir)
    return bundle_manifest


def sha256_file_checked(path: Path, expected_size: int, expected_sha256: str, label: str) -> None:
    fd, st = open_readonly_nofollow(path, label)
    try:
        if st.st_size != expected_size:
            raise CaptureError(f"{label} size mismatch ({st.st_size} != {expected_size})")
        hasher = hashlib.sha256()
        remaining = expected_size
        while remaining:
            chunk = os.read(fd, min(CHUNK_BYTES, remaining))
            if not chunk:
                raise CaptureError(f"{label} truncated during hashing")
            hasher.update(chunk)
            remaining -= len(chunk)
        if os.read(fd, 1):
            raise CaptureError(f"{label} grew during hashing")
        if stat_tuple(os.fstat(fd)) != stat_tuple(st) or stat_tuple(require_regular_single_link(path, label)) != stat_tuple(st):
            raise CaptureError(f"{label} changed during hashing")
        if hasher.hexdigest() != expected_sha256:
            raise CaptureError(f"{label} sha256 mismatch")
    finally:
        os.close(fd)


def validate_recovery_active(path: Path, manifest_hash: str) -> None:
    fd, initial = open_readonly_nofollow(path, "recovery.active")
    try:
        expected = f"{manifest_hash}\n".encode("ascii")
        if initial.st_size != len(expected) or os.read(fd, len(expected) + 1) != expected:
            raise CaptureError("recovery.active does not match exact expected recovery manifest hash")
        if stat_tuple(os.fstat(fd)) != stat_tuple(initial):
            raise CaptureError("recovery.active changed during validation")
    finally:
        os.close(fd)


def validate_genesis_network_hash(path: Path) -> str:
    fd, initial = open_readonly_nofollow(path, "genesis.network-hash")
    try:
        raw = os.read(fd, 129)
        if len(raw) > 128 or len(raw) != initial.st_size:
            raise CaptureError("genesis.network-hash exceeds bounded file size")
        try:
            value = raw.strip().decode("ascii", errors="strict")
        except UnicodeDecodeError as error:
            raise CaptureError("genesis.network-hash must contain an ASCII hash") from error
        return require_hex64(value, "genesis.network-hash")
    finally:
        os.close(fd)


def audit_bundle_tree_matches_manifest(bundle_dir: Path, expected_relpaths: set[str]) -> None:
    expected_all = expected_relpaths | {BUNDLE_TAIL_NAME, BUNDLE_MANIFEST_NAME}
    expected_dirs = {
        parent.as_posix()
        for rel in expected_all
        for parent in Path(rel).parents
        if str(parent) != "."
    }
    found: set[str] = set()
    found_dirs: set[str] = set()
    pending: list[tuple[Path, int]] = [(bundle_dir, 0)]
    visited = 0
    while pending:
        current, depth = pending.pop()
        if depth > 2:
            raise CaptureError("bundle directory nesting exceeds maximum depth")
        with os.scandir(current) as entries:
            current_entries = []
            for entry in entries:
                current_entries.append(entry)
                if len(current_entries) + visited > len(expected_all) + len(expected_dirs):
                    raise CaptureError("bundle directory exceeds maximum entry count")
        for entry in current_entries:
            visited += 1
            if visited > len(expected_all) + len(expected_dirs):
                raise CaptureError("bundle directory exceeds maximum entry count")
            path = Path(entry.path)
            st = os.lstat(path)
            if stat.S_ISLNK(st.st_mode):
                raise CaptureError(f"symlink inside bundle directory: {path}")
            if stat.S_ISDIR(st.st_mode):
                rel = path.relative_to(bundle_dir).as_posix()
                if rel not in expected_dirs:
                    raise CaptureError(f"unexpected directory inside capture bundle: {rel}")
                found_dirs.add(rel)
                pending.append((path, depth + 1))
            elif stat.S_ISREG(st.st_mode):
                if st.st_nlink != 1:
                    raise CaptureError(f"hardlinked file inside bundle directory: {path}")
                rel = path.relative_to(bundle_dir).as_posix()
                found.add(rel)
            else:
                raise CaptureError(f"non-regular entry inside bundle directory: {path}")
    if found != expected_all or found_dirs != expected_dirs:
        raise CaptureError("bundle directory files do not match exact pinned manifest")


def _check_candidate_publish_destinations(
    candidate_dir: Path,
    manifest_hash: str,
    target_relpaths: set[str] | tuple[str, ...],
) -> None:
    require_real_dir(candidate_dir, "candidate_dir")
    dag_root = f"dag-wal-recovery-{manifest_hash}"
    reserved_names = {
        "state.wal",
        PUBLISH_MANIFEST_NAME,
        f"{PUBLISH_MANIFEST_NAME}.partial",
        dag_root,
    }
    try:
        with os.scandir(candidate_dir) as entries:
            for entry in entries:
                if entry.name in reserved_names:
                    raise CaptureError(f"candidate_dir already contains {entry.name}")
                if ROTATED_WAL_RE.match(entry.name):
                    raise CaptureError(f"candidate_dir contains rotated WAL segment: {entry.name}")
    except OSError as exc:
        raise CaptureError(f"failed to scan candidate_dir: {exc}") from exc
    checked_paths: set[Path] = set()
    for rel in target_relpaths:
        dst = candidate_dir / rel
        for parent in dst.relative_to(candidate_dir).parents:
            if str(parent) != ".":
                parent_path = candidate_dir / parent
                if parent_path not in checked_paths:
                    checked_paths.add(parent_path)
                    if os.path.lexists(parent_path):
                        raise CaptureError(f"candidate_dir already contains destination path: {parent.as_posix()}")
        if dst not in checked_paths:
            checked_paths.add(dst)
            if os.path.lexists(dst):
                raise CaptureError(f"candidate_dir already contains destination file: {rel}")


def _stage_exclusive_json_partial(path: Path, payload: dict[str, Any]) -> Path:
    raw = (json.dumps(payload, indent=2, sort_keys=True) + "\n").encode("utf-8")
    if os.path.lexists(path):
        raise CaptureError(f"JSON destination already exists: {path.name}")
    partial = path.with_name(path.name + ".partial")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    try:
        fd = os.open(partial, flags, 0o600)
    except FileExistsError as exc:
        raise CaptureError(f"partial JSON file already exists: {partial.name}") from exc
    try:
        try:
            os.fchmod(fd, 0o600)
            write_all(fd, raw)
            os.fsync(fd)
        finally:
            os.close(fd)
    except BaseException:
        try:
            partial.unlink()
        except OSError:
            pass
        raise
    return partial


def _copy_bounded_file(
    *,
    src_path: Path,
    dst_path: Path,
    rel: str,
    expected_size: int,
    expected_sha: str,
    created_files: list[Path],
) -> None:
    create_flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0)
    src_fd, src_st = open_readonly_nofollow(src_path, rel)
    dst_fd: int | None = None
    try:
        if src_st.st_size != expected_size:
            raise CaptureError(f"{rel} size changed before target copy")
        try:
            dst_fd = os.open(dst_path, create_flags, 0o600)
        except FileExistsError as exc:
            raise CaptureError(f"destination file already exists: {rel}") from exc
        created_files.append(dst_path)
        os.fchmod(dst_fd, 0o600)
        hasher = hashlib.sha256()
        remaining = expected_size
        copied = 0
        while remaining > 0:
            chunk = os.read(src_fd, min(CHUNK_BYTES, remaining))
            if not chunk:
                raise CaptureError(f"{rel} truncated during target copy")
            if len(chunk) > remaining:
                raise CaptureError(f"{rel} exceeded expected size during target copy")
            write_all(dst_fd, chunk)
            hasher.update(chunk)
            copied += len(chunk)
            remaining -= len(chunk)
        if copied != expected_size or os.read(src_fd, 1) or os.fstat(src_fd).st_size != expected_size:
            raise CaptureError(f"{rel} size changed during target copy")
        if hasher.hexdigest() != expected_sha:
            raise CaptureError(f"{rel} changed during target copy")
        if os.fstat(dst_fd).st_size != expected_size:
            raise CaptureError(f"{rel} destination size mismatch after target copy")
        os.fsync(dst_fd)
    finally:
        try:
            if dst_fd is not None:
                os.close(dst_fd)
        finally:
            os.close(src_fd)


def _append_tail_bounded(
    *,
    tail_path: Path,
    wal_fd: int,
    prefix_bytes: int,
    tail_bytes: int,
    tail_sha: str,
    on_append_start: Callable[[], None],
) -> None:
    tail_fd, tail_st = open_readonly_nofollow(tail_path, BUNDLE_TAIL_NAME)
    tail_hasher = hashlib.sha256()
    try:
        if tail_st.st_size != tail_bytes:
            raise CaptureError("tail size changed before append")
        os.lseek(wal_fd, prefix_bytes, os.SEEK_SET)
        remaining = tail_bytes
        appended = 0
        started = False
        while remaining > 0:
            chunk = os.read(tail_fd, min(CHUNK_BYTES, remaining))
            if not chunk:
                raise CaptureError("tail truncated during append")
            if len(chunk) > remaining:
                raise CaptureError("tail exceeded expected size during append")
            if not started:
                on_append_start()
                started = True
            write_all(wal_fd, chunk)
            tail_hasher.update(chunk)
            appended += len(chunk)
            remaining -= len(chunk)
        if appended != tail_bytes or os.read(tail_fd, 1) or os.fstat(tail_fd).st_size != tail_bytes:
            raise CaptureError("tail changed or truncated during append")
        if tail_hasher.hexdigest() != tail_sha:
            raise CaptureError("tail changed or truncated during append")
        os.fsync(wal_fd)
    finally:
        os.close(tail_fd)


def _rollback_precommit_publish(
    *,
    candidate_dir: Path,
    wal_fd: int,
    prefix_bytes: int,
    tail_append_started: bool,
    created_files: list[Path],
    created_dirs: set[Path],
    staged_manifest_partial: Path | None,
) -> list[str]:
    errors: list[str] = []
    if staged_manifest_partial is not None:
        try:
            staged_manifest_partial.unlink()
        except FileNotFoundError:
            pass
        except OSError as exc:
            errors.append(f"unlink {staged_manifest_partial.name}: {exc}")
    if tail_append_started:
        try:
            current_size = os.fstat(wal_fd).st_size
            if current_size < prefix_bytes:
                errors.append(f"precopy shrank below prefix_bytes ({current_size} < {prefix_bytes}); recopy the verified source prefix instead of extending missing bytes")
            else:
                os.ftruncate(wal_fd, prefix_bytes)
                os.fsync(wal_fd)
        except OSError as exc:
            errors.append(f"restore precopy length to {prefix_bytes}: {exc}")
    for path in reversed(created_files):
        try:
            path.unlink()
        except FileNotFoundError:
            pass
        except OSError as exc:
            errors.append(f"unlink {path.name}: {exc}")
    for d in sorted(created_dirs, key=lambda p: len(p.parts), reverse=True):
        try:
            d.rmdir()
        except FileNotFoundError:
            pass
        except OSError as exc:
            errors.append(f"rmdir {d.name}: {exc}")
    if created_files or created_dirs or staged_manifest_partial is not None:
        try:
            fsync_dir(candidate_dir)
        except OSError as exc:
            errors.append(f"fsync candidate_dir: {exc}")
    return errors


def publish_bundle_into_candidate(
    *,
    candidate_dir: Path,
    bundle_dir: Path,
    expected_recovery_manifest_hash: str,
    min_reserve_bytes: int = DEFAULT_MIN_RESERVE_BYTES,
    free_space_fn: Callable[[Path], int] = default_free_space_bytes,
    on_before_tail_append: Callable[[], None] | None = None,
) -> dict[str, Any]:
    manifest_hash = require_hex64(expected_recovery_manifest_hash, "expected_recovery_manifest_hash")
    min_reserve_bytes = require_int_value(min_reserve_bytes, "min_reserve_bytes", 1)
    if not candidate_dir.name.startswith(CANDIDATE_PREFIX) or len(candidate_dir.name) <= len(CANDIDATE_PREFIX):
        raise CaptureError(f"candidate_dir basename must start with {CANDIDATE_PREFIX!r}")
    if not bundle_dir.name.startswith(BUNDLE_PREFIX) or len(bundle_dir.name) <= len(BUNDLE_PREFIX):
        raise CaptureError(f"bundle_dir basename must start with {BUNDLE_PREFIX!r}")
    require_real_dir(candidate_dir, "candidate_dir")
    require_real_dir(bundle_dir, "bundle_dir")
    require_disjoint_paths(candidate_dir, "candidate_dir", bundle_dir, "bundle_dir")
    state_wal_path = candidate_dir / "state.wal"
    publish_manifest_path = candidate_dir / PUBLISH_MANIFEST_NAME
    publish_partial_path = publish_manifest_path.with_name(publish_manifest_path.name + ".partial")
    _check_candidate_publish_destinations(
        candidate_dir,
        manifest_hash,
        mandatory_bundle_relpaths(manifest_hash),
    )
    precopy_manifest = read_strict_json_file(candidate_dir / PRECOPY_MANIFEST_NAME, "precopy manifest")
    if (
        precopy_manifest.get("schema") != PRECOPY_MANIFEST_SCHEMA
        or precopy_manifest.get("status") != "PRECOPY_ONLY_UNQUALIFIED"
        or precopy_manifest.get("qualified_for_recovery") is not False
        or precopy_manifest.get("source_atomicity_claimed") is not False
        or precopy_manifest.get("wal_filename") != PRECOPY_WAL_NAME
    ):
        raise CaptureError("invalid precopy manifest schema or status")
    prefix_bytes = require_int(precopy_manifest, "prefix_bytes", 1)
    prefix_sha = require_hex64(precopy_manifest.get("sha256"), "precopy sha256")
    source_dev = require_int(precopy_manifest, "source_dev", 0)
    source_ino = require_int(precopy_manifest, "source_ino", 0)
    target_dev = require_int(precopy_manifest, "target_dev", 0)
    target_ino = require_int(precopy_manifest, "target_ino", 0)
    bundle_manifest = read_strict_json_file(bundle_dir / BUNDLE_MANIFEST_NAME, "bundle manifest")
    if (
        bundle_manifest.get("schema") != BUNDLE_MANIFEST_SCHEMA
        or bundle_manifest.get("status") != "CAPTURE_BUNDLE_UNQUALIFIED"
        or bundle_manifest.get("qualified_for_recovery") is not False
        or bundle_manifest.get("certified_for_activation") is not False
        or bundle_manifest.get("recovery_manifest_hash") != manifest_hash
    ):
        raise CaptureError("bundle manifest schema, status, or recovery manifest mismatch")
    if (
        require_int(bundle_manifest, "source_dev", 0) != source_dev
        or require_int(bundle_manifest, "source_ino", 0) != source_ino
        or require_int(bundle_manifest, "prefix_bytes", 1) != prefix_bytes
    ):
        raise CaptureError("bundle manifest does not bind precopy source dev/ino/prefix_bytes")
    tail_bytes = require_int(bundle_manifest, "tail_bytes", 0, MAX_BUNDLE_BYTES)
    if require_int(bundle_manifest, "source_frozen_size", prefix_bytes) != prefix_bytes + tail_bytes:
        raise CaptureError("bundle manifest frozen size mismatch")
    tail_sha = require_hex64(bundle_manifest.get("tail_sha256"), "tail_sha256")
    files = bundle_manifest.get("files")
    if not isinstance(files, list) or not files or len(files) > MAX_BUNDLE_FILES - 1:
        raise CaptureError("bundle manifest files list is empty")
    seen_rels: set[str] = set()
    small_total = 0
    for item in files:
        if not isinstance(item, dict) or not isinstance(item.get("relpath"), str):
            raise CaptureError("invalid file entry in bundle manifest")
        rel = item["relpath"]
        if rel in seen_rels or not is_allowed_bundle_relpath(rel, manifest_hash):
            raise CaptureError(f"disallowed or duplicate bundle file: {rel}")
        seen_rels.add(rel)
        fsize = require_int(item, "size", 0, MAX_BUNDLE_BYTES)
        fsha = require_hex64(item.get("sha256"), f"{rel} sha256")
        sha256_file_checked(bundle_dir / rel, fsize, fsha, rel)
        small_total += fsize
    if not mandatory_bundle_relpaths(manifest_hash).issubset(seen_rels):
        raise CaptureError("bundle manifest is missing one or more mandatory recovery files")
    if small_total + tail_bytes > MAX_BUNDLE_BYTES:
        raise CaptureError("bundle payload exceeds 512 MiB limit")
    sha256_file_checked(bundle_dir / BUNDLE_TAIL_NAME, tail_bytes, tail_sha, BUNDLE_TAIL_NAME)
    audit_bundle_tree_matches_manifest(bundle_dir, seen_rels)
    validate_recovery_active(bundle_dir / "recovery.active", manifest_hash)
    validate_genesis_network_hash(bundle_dir / "genesis.network-hash")
    read_strict_json_file(bundle_dir / f"recovery-dag.{manifest_hash}.pin.json", "bundle recovery DAG pin")
    if free_space_fn(candidate_dir) < tail_bytes + small_total + min_reserve_bytes:
        raise CaptureError("insufficient candidate free space for tail and small files plus reserve")
    _check_candidate_publish_destinations(candidate_dir, manifest_hash, seen_rels)
    precopy_path = candidate_dir / PRECOPY_WAL_NAME
    initial_path_stat = require_regular_single_link(precopy_path, PRECOPY_WAL_NAME)
    wal_fd = os.open(precopy_path, os.O_RDWR | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NONBLOCK", 0))
    locked = False
    created_files: list[Path] = []
    created_dirs: set[Path] = set()
    tail_append_started = False
    staged_manifest: Path | None = None
    commit_started = False
    try:
        try:
            fcntl.flock(wal_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            locked = True
        except OSError as error:
            raise CaptureError(f"failed to acquire advisory lock on {PRECOPY_WAL_NAME}: {error}") from error
        _check_candidate_publish_destinations(candidate_dir, manifest_hash, seen_rels)
        initial = os.fstat(wal_fd)
        if (
            stat_tuple(initial) != stat_tuple(initial_path_stat)
            or (initial.st_dev, initial.st_ino) != (target_dev, target_ino)
            or initial.st_size != prefix_bytes
            or initial.st_nlink != 1
        ):
            raise CaptureError("precopy file identity or size no longer matches receipt")
        prefix_hasher = hashlib.sha256()
        remaining = prefix_bytes
        while remaining:
            chunk = os.read(wal_fd, min(CHUNK_BYTES, remaining))
            if not chunk:
                raise CaptureError("precopy file truncated during prefix verification")
            prefix_hasher.update(chunk)
            remaining -= len(chunk)
        if prefix_hasher.hexdigest() != prefix_sha or os.read(wal_fd, 1):
            raise CaptureError("precopy file full prefix sha256 or length mismatch")
        for item in files:
            rel = item["relpath"]
            destination = candidate_dir / rel
            for parent in reversed(destination.relative_to(candidate_dir).parents):
                if str(parent) == ".":
                    continue
                directory = candidate_dir / parent
                if directory not in created_dirs:
                    if os.path.lexists(directory):
                        raise CaptureError(f"unowned destination directory appeared: {parent}")
                    directory.mkdir(mode=0o700)
                    created_dirs.add(directory)
            _copy_bounded_file(
                src_path=bundle_dir / rel, dst_path=destination, rel=rel,
                expected_size=item["size"], expected_sha=item["sha256"], created_files=created_files,
            )
        for directory in sorted(created_dirs, key=lambda path: len(path.parts), reverse=True):
            fsync_dir(directory)
        if on_before_tail_append is not None:
            on_before_tail_append()
        if stat_tuple(os.fstat(wal_fd)) != stat_tuple(initial) or stat_tuple(require_regular_single_link(precopy_path, PRECOPY_WAL_NAME)) != stat_tuple(initial):
            raise CaptureError("precopy file stat changed before tail append")
        if any(os.path.lexists(path) for path in (state_wal_path, publish_manifest_path, publish_partial_path)):
            raise CaptureError("publish destination appeared before tail append")

        def mark_append_started() -> None:
            nonlocal tail_append_started
            tail_append_started = True

        _append_tail_bounded(
            tail_path=bundle_dir / BUNDLE_TAIL_NAME, wal_fd=wal_fd,
            prefix_bytes=prefix_bytes, tail_bytes=tail_bytes, tail_sha=tail_sha,
            on_append_start=mark_append_started,
        )
        final_size = prefix_bytes + tail_bytes
        appended_stat = os.fstat(wal_fd)
        if appended_stat.st_size != final_size or appended_stat.st_nlink != 1:
            raise CaptureError("published state.wal size mismatch after append")
        os.lseek(wal_fd, 0, os.SEEK_SET)
        verify_prefix = hashlib.sha256()
        verify_tail = hashlib.sha256()
        verify_full = hashlib.sha256()
        verified = 0
        while verified < final_size:
            read_size = min(CHUNK_BYTES, final_size - verified)
            if verified < prefix_bytes:
                read_size = min(read_size, prefix_bytes - verified)
            chunk = os.read(wal_fd, read_size)
            if not chunk:
                raise CaptureError("state.wal truncated during post-append verification")
            (verify_prefix if verified < prefix_bytes else verify_tail).update(chunk)
            verify_full.update(chunk)
            verified += len(chunk)
        if verify_prefix.hexdigest() != prefix_sha:
            raise CaptureError("precopy prefix bytes mutated during publish and cannot be restored; recopy the verified source prefix")
        if os.read(wal_fd, 1) or verify_tail.hexdigest() != tail_sha:
            raise CaptureError("tail bytes mutated during publish")
        if stat_tuple(os.fstat(wal_fd)) != stat_tuple(appended_stat) or stat_tuple(require_regular_single_link(precopy_path, PRECOPY_WAL_NAME)) != stat_tuple(appended_stat):
            raise CaptureError("precopy file changed during post-append verification")
        publish_manifest = {
            "schema": PUBLISH_MANIFEST_SCHEMA,
            "status": "PUBLISHED_UNQUALIFIED_CANDIDATE",
            "qualified_for_recovery": False,
            "certified_for_activation": False,
            "recovery_manifest_hash": manifest_hash,
            "source_dev": source_dev,
            "source_ino": source_ino,
            "prefix_bytes": prefix_bytes,
            "tail_bytes": tail_bytes,
            "tail_sha256": tail_sha,
            "state_wal_bytes": final_size,
            "state_wal_sha256": verify_full.hexdigest(),
            "state_wal_ino": target_ino,
            "published_files": [item["relpath"] for item in files],
        }
        staged_manifest = _stage_exclusive_json_partial(publish_manifest_path, publish_manifest)
        fsync_dir(candidate_dir)
        os.link(precopy_path, state_wal_path, follow_symlinks=False)
        commit_started = True
        os.link(staged_manifest, publish_manifest_path, follow_symlinks=False)
        fsync_dir(candidate_dir)
        precopy_path.unlink()
        staged_manifest.unlink()
        fsync_dir(candidate_dir)
        return publish_manifest
    except BaseException as error:
        if commit_started:
            raise CaptureError("interrupted publication after state.wal link; retained candidate and precopy bytes are unqualified; inspect publication links and manifests before any retry") from error
        if locked:
            rollback_errors = _rollback_precommit_publish(
                candidate_dir=candidate_dir, wal_fd=wal_fd, prefix_bytes=prefix_bytes,
                tail_append_started=tail_append_started, created_files=created_files,
                created_dirs=created_dirs, staged_manifest_partial=staged_manifest,
            )
            if rollback_errors:
                raise CaptureError("publication failed and cleanup is incomplete: " + "; ".join(rollback_errors)) from error
        raise
    finally:
        try:
            if locked:
                fcntl.flock(wal_fd, fcntl.LOCK_UN)
        finally:
            os.close(wal_fd)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="peer_capture.py")
    sub = parser.add_subparsers(dest="cmd", required=True)
    p_pub = sub.add_parser("publish-target")
    p_pub.add_argument("--candidate-dir", required=True, type=Path)
    p_pub.add_argument("--bundle-dir", required=True, type=Path)
    p_pub.add_argument("--expected-recovery-manifest-hash", required=True)
    p_pub.add_argument("--min-reserve-gib", type=int, default=3)
    args = parser.parse_args(argv)
    try:
        if args.min_reserve_gib < 1:
            raise CaptureError("min-reserve-gib must be >= 1")
        result = publish_bundle_into_candidate(
            candidate_dir=args.candidate_dir,
            bundle_dir=args.bundle_dir,
            expected_recovery_manifest_hash=args.expected_recovery_manifest_hash,
            min_reserve_bytes=args.min_reserve_gib * 1024 * 1024 * 1024,
        )
        sys.stdout.write(json.dumps(result, indent=2, sort_keys=True) + "\n")
        return 0
    except (CaptureError, OSError) as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
