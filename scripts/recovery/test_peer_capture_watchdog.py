from __future__ import annotations

import os
import tempfile
import time
import unittest
from pathlib import Path
from typing import Callable
from unittest import mock

from scripts.recovery import peer_capture as pc
from scripts.recovery import test_peer_capture as fixture

FakeFreezeBackend = fixture.FakeFreezeBackend
MANIFEST_HASH = fixture.MANIFEST_HASH


class PeerCaptureWatchdogTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="arc-peer-capture-wd-test-")
        self.root = Path(self.tmp.name).resolve()
        self.src = self.root / "source-lhr"
        self.src.mkdir(mode=0o700)
        self.prefix = b"wal-prefix-" * 512
        self.tail = b"wal-tail-" * 256
        self.src_wal = self.src / "state.wal"
        self.src_wal.write_bytes(self.prefix + self.tail)
        self._cg_seq = 0
        fixture.PeerCaptureTests._populate_source_layout(self)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def _make_cgroup_backend(
        self,
        *,
        proc_cgroup_reader: Callable[[int], str] | None = None,
        procs_text: str = "4242\n",
        freeze_text: str = "0\n",
        events_text: str = "populated 1\nfrozen 0\n",
    ) -> tuple[pc.CgroupV2FreezeBackend, Path]:
        self._cg_seq += 1
        cg_root = self.root / f"fake-cgroup-{self._cg_seq}"
        cg_dir = cg_root / "system.slice" / "arc-validator.service"
        cg_dir.mkdir(parents=True, mode=0o700)
        (cg_dir / "cgroup.freeze").write_text(freeze_text, encoding="ascii")
        (cg_dir / "cgroup.events").write_text(events_text, encoding="ascii")
        (cg_dir / "cgroup.procs").write_text(procs_text, encoding="ascii")
        good_props = {
            "ActiveState": "active",
            "ControlGroup": "/system.slice/arc-validator.service",
            "MainPID": "4242",
            "WatchdogUSec": "0",
        }
        backend = pc.CgroupV2FreezeBackend(
            unit_name="arc-validator.service",
            expected_service_cgroup="/system.slice/arc-validator.service",
            expected_main_pid=4242,
            expected_service_watchdog_usec=0,
            cgroup_root=cg_root,
            service_inspector=lambda _: dict(good_props),
            proc_cgroup_reader=proc_cgroup_reader,
        )
        return backend, cg_dir

    def test_preflight_rejects_target_descendant_and_invalid_proc_cgroup(self) -> None:
        for bad_cg in (
            "0::/system.slice/arc-validator.service\n",
            "0::/system.slice/arc-validator.service/sub\n",
            "12:memory:/user.slice/ok\n0::/system.slice/arc-validator.service/a/b\n",
            "12:memory:/user.slice/ok\n",
            "",
            "0::/system.slice/other/../arc-validator.service\n",
            "0::/user.slice/a\n0::/user.slice/b\n",
        ):
            backend, _ = self._make_cgroup_backend(proc_cgroup_reader=lambda _pid, cg=bad_cg: cg)
            with self.assertRaises(pc.CaptureError):
                backend.verify_preflight()
        missing_backend, _ = self._make_cgroup_backend(
            proc_cgroup_reader=lambda _pid: (_ for _ in ()).throw(FileNotFoundError("/proc/self/cgroup"))
        )
        with self.assertRaises(pc.CaptureError):
            missing_backend.verify_preflight()
        for ok_cg in (
            "0::/\n",
            "0::/user.slice/test.scope\n",
            "12:memory:/system.slice/arc-validator.service\n0::/system.slice/arc-validator.service-other\n",
        ):
            backend, _ = self._make_cgroup_backend(proc_cgroup_reader=lambda _pid, cg=ok_cg: cg)
            preflight = backend.verify_preflight()
            self.assertEqual(preflight["main_pid"], 4242)

    def test_arm_watchdog_rejects_descendant_cgroup_and_terminates_watchdog(self) -> None:
        observed_pids: list[int] = []

        def reader(pid: int) -> str:
            observed_pids.append(pid)
            if pid == os.getpid():
                return "0::/user.slice/test.scope\n"
            return "0::/system.slice/arc-validator.service/sub\n"

        backend, _ = self._make_cgroup_backend(proc_cgroup_reader=reader)
        self.assertEqual(backend.verify_preflight()["main_pid"], 4242)
        with self.assertRaises(pc.CaptureError):
            backend.arm_watchdog(time.monotonic() + 5.0)
        wd_pids = [pid for pid in observed_pids if pid != os.getpid()]
        self.assertEqual(len(wd_pids), 1)
        with self.assertRaises(ProcessLookupError):
            os.kill(wd_pids[0], 0)
        self.assertEqual(len(backend._owned_watchdogs), 0)

    def test_chunked_cgroup_read_finds_late_pid_and_rejects_overflow(self) -> None:
        backend, group = self._make_cgroup_backend(
            proc_cgroup_reader=lambda _: "0::/user.slice/test.scope\n",
            procs_text="100000\n" * 1300 + "4242\n",
        )
        original = os.read
        with mock.patch.object(pc.os, "read", side_effect=lambda fd, n: original(fd, min(n, 4096))):
            self.assertEqual(backend.verify_preflight()["main_pid"], 4242)
            (group / "cgroup.procs").write_text("100000\n" * 1300 + f"4242\n{os.getpid()}\n")
            with self.assertRaisesRegex(pc.CaptureError, "helper PID"):
                backend.verify_preflight()
            (group / "cgroup.procs").write_text("100000\n" * 10000)
            with self.assertRaisesRegex(pc.CaptureError, "exceeded 65536"):
                backend.verify_preflight()

    def test_frozen_capture_requires_original_service_pid(self) -> None:
        backend, group = self._make_cgroup_backend(
            proc_cgroup_reader=lambda _: "0::/user.slice/test.scope\n",
            freeze_text="1\n", events_text="populated 1\nfrozen 1\n",
        )
        backend.assert_still_frozen()
        (group / "cgroup.procs").write_text("99999\n")
        with self.assertRaisesRegex(pc.CaptureError, "disappeared"):
            backend.assert_still_frozen()

    def test_parent_pipe_eof_thaws_before_long_deadline(self) -> None:
        freeze = self.root / "sim.freeze"
        freeze.write_text("1\n")
        handle = pc.spawn_setsid_thaw_watchdog(freeze, time.monotonic() + 5)
        start = time.monotonic()
        os.close(handle.pdeath_wfd)
        try:
            handle.proc.wait(timeout=0.5)
            self.assertEqual(freeze.read_text(), "0\n")
            self.assertLess(time.monotonic() - start, 0.5)
        finally:
            pc.terminate_owned_process(handle.proc)

    def test_owned_cancel_closes_descriptor_and_rejects_foreign_handle(self) -> None:
        backend, _ = self._make_cgroup_backend(proc_cgroup_reader=lambda _: "0::/user.slice/test.scope\n")
        handle = backend.arm_watchdog(time.monotonic() + 5)
        other, _ = self._make_cgroup_backend(proc_cgroup_reader=lambda _: "0::/user.slice/test.scope\n")
        try:
            with self.assertRaisesRegex(pc.CaptureError, "unowned"):
                other.cancel_watchdog(handle)
        finally:
            backend.cancel_watchdog(handle)
        self.assertIsNotNone(handle.proc.poll())
        with self.assertRaises(OSError):
            os.fstat(handle.pdeath_wfd)

    def test_capture_deadline_leaves_watchdog_thaw_margin(self) -> None:
        now = [100.0]

        class SlowArm(FakeFreezeBackend):
            def arm_watchdog(inner, deadline):
                inner.watchdog_deadline = deadline
                now[0] += 1.9
                return super().arm_watchdog(deadline)

            def freeze_and_confirm(inner, deadline, clock):
                self.assertLessEqual(deadline, inner.watchdog_deadline - 1)
                super().freeze_and_confirm(deadline, clock)

        st = self.src_wal.stat()
        backend = SlowArm()
        pc.capture_source_tail_bundle(
            source_data_dir=self.src, bundle_dir=self.root / "peer-capture-bundle-slow",
            recovery_manifest_hash=MANIFEST_HASH, precopy_prefix_bytes=len(self.prefix),
            precopy_source_dev=st.st_dev, precopy_source_ino=st.st_ino,
            freeze_backend=backend, clock=lambda: now[0],
            free_space_fn=lambda _: 10 * 1024**3,
        )
        self.assertIn("thaw", backend.events)

