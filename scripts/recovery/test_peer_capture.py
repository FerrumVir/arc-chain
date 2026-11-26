#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import io
import json
import math
import os
import select
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from typing import Any, Callable

from scripts.recovery import peer_capture as pc
from scripts.recovery import wal_precopy as wp

MANIFEST_HASH = "9c6aa3ec659b101751bf58a8a24c655f0e4fba79596e9b44f87a30f98f08acc0"
GEN_HASH = "980cfafff4e3db3f5e89e01ca61264edb2a62bdea1b89ad31ae773cc7fbe3178"


class FakeFreezeBackend:
    def __init__(
        self,
        *,
        ack_freeze: bool = True,
        still_frozen: bool = True,
        fail_thaw: bool = False,
        fail_watchdog: bool = False,
        watchdog_usec: int = 0,
        on_after_freeze: Callable[[], None] | None = None,
    ) -> None:
        self.ack_freeze = ack_freeze
        self.still_frozen = still_frozen
        self.fail_thaw = fail_thaw
        self.fail_watchdog = fail_watchdog
        self.watchdog_usec = watchdog_usec
        self.on_after_freeze = on_after_freeze
        self.events: list[str] = []
        self.write1_occurred = False

    def verify_preflight(self) -> dict[str, Any]:
        if self.watchdog_usec != 0:
            raise pc.CaptureError("non-zero WatchdogUSec")
        self.events.append("preflight")
        return {"freeze": 0, "events_frozen": 0, "watchdog_usec": 0}

    def arm_watchdog(self, absolute_deadline: float) -> str:
        self.events.append("arm")
        if self.fail_watchdog:
            raise pc.CaptureError("watchdog spawn failed")
        return "wd-handle"

    def cancel_watchdog(self, handle: Any) -> None:
        self.events.append(f"cancel:{handle}")

    def freeze_and_confirm(self, deadline: float, clock: Callable[[], float]) -> None:
        self.events.append("freeze")
        self.write1_occurred = True
        if not self.ack_freeze or clock() > deadline:
            raise pc.CaptureError("freeze unacknowledged")
        if self.on_after_freeze is not None:
            self.on_after_freeze()

    def assert_still_frozen(self) -> None:
        self.events.append("assert_frozen")
        if not self.still_frozen:
            raise pc.CaptureError("cgroup thawed mid-capture")

    def thaw_and_confirm(self) -> None:
        self.events.append("thaw")
        if self.fail_thaw:
            raise pc.CaptureError("thaw not acknowledged")


class PeerCaptureTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="arc-peer-capture-test-")
        self.root = Path(self.tmp.name).resolve()
        self.src = self.root / "source-lhr"
        self.src.mkdir(mode=0o700)
        self.prefix = b"wal-prefix-" * 512
        self.tail = b"wal-tail-" * 256
        self.src_wal = self.src / "state.wal"
        self.src_wal.write_bytes(self.prefix)
        self._populate_source_layout()
        self.cand = self.root / "peer-recovery-candidate-sgp"
        self._write_precopy_candidate()
        with open(self.src_wal, "ab") as fh:
            fh.write(self.tail)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def _populate_source_layout(self) -> None:
        dag = self.src / f"dag-wal-recovery-{MANIFEST_HASH}"
        gen = dag / f"gen-{GEN_HASH}"
        gen.mkdir(parents=True, mode=0o700)
        (self.src / ".arc-node.lock").write_text("lock", encoding="utf-8")
        (self.src / "consensus-signing-record.bin").write_bytes(b"source-signing-record")
        (self.src / "validator.key").write_bytes(b"source-private-key")
        (self.src / "state-snapshot.bin").write_bytes(b"unused-snapshot")
        (self.src / "known_peers.json").write_text("[]", encoding="utf-8")
        (self.src / "recovery.active").write_text(f"{MANIFEST_HASH}\n", encoding="ascii")
        (self.src / "genesis.network-hash").write_text(f"{'a' * 64}\n", encoding="ascii")
        (self.src / f"recovery-{MANIFEST_HASH}.arcchkpt").write_bytes(b"chkpt-bytes")
        (self.src / f"recovery-dag.{MANIFEST_HASH}.pin.json").write_text('{"seq":1}\n', encoding="utf-8")
        (dag / ".WRITE.lock").write_text("dag-lock", encoding="utf-8")
        (dag / "CURRENT").write_text(GEN_HASH, encoding="ascii")
        (dag / "GC-ANCHOR.json").write_text('{"anchor":1}\n', encoding="utf-8")
        (dag / "recovery-dag.binding.json").write_text('{"bind":1}\n', encoding="utf-8")
        (dag / f"active-{GEN_HASH}.bin").write_bytes(b"active-delta")
        (gen / "manifest.json").write_text('{"gen":1}\n', encoding="utf-8")
        (gen / "records.bin").write_bytes(b"dag-records")

    def _write_precopy_candidate(self) -> None:
        buf = io.BytesIO()
        footer = wp.stream_source(self.src_wal, buf, chunk_bytes=1024, rate_bytes_per_sec=0)
        self.assertEqual(footer["status"], "PRECOPY_ONLY")
        receipt = wp.receive_stream(
            self.cand,
            io.BytesIO(buf.getvalue()),
            min_reserve_bytes=1024,
            reserve_check_bytes=1024,
            free_space_fn=lambda _: 10_000_000,
        )
        self.assertEqual(receipt["status"], "PRECOPY_ONLY_UNQUALIFIED")
        self.assertFalse(receipt["qualified_for_recovery"])
        self.assertFalse(receipt["source_atomicity_claimed"])
        self.precopy_ino = os.lstat(self.cand / pc.PRECOPY_WAL_NAME).st_ino
        (self.cand / "consensus-signing-record.bin").write_bytes(b"target-preserved-record")

    def _capture_valid_bundle(self, name: str = "peer-capture-bundle-1") -> tuple[Path, FakeFreezeBackend, dict[str, Any]]:
        src_st = os.lstat(self.src_wal)
        bundle = self.root / name
        backend = FakeFreezeBackend()
        res = pc.capture_source_tail_bundle(
            source_data_dir=self.src,
            bundle_dir=bundle,
            recovery_manifest_hash=MANIFEST_HASH,
            precopy_prefix_bytes=len(self.prefix),
            precopy_source_dev=src_st.st_dev,
            precopy_source_ino=src_st.st_ino,
            freeze_backend=backend,
            deadline_s=8.0,
            free_space_fn=lambda _: pc.DEFAULT_MIN_RESERVE_BYTES + 2 * pc.MAX_BUNDLE_BYTES,
        )
        return bundle, backend, res

    def test_happy_capture_excludes_locks_keys_and_publishes_in_place(self) -> None:
        bundle, backend, cap = self._capture_valid_bundle()
        self.assertEqual(backend.events, ["preflight", "arm", "freeze", "assert_frozen", "thaw", "cancel:wd-handle"])
        self.assertFalse(cap["qualified_for_recovery"])
        self.assertFalse((bundle / "validator.key").exists())
        self.assertFalse((bundle / "consensus-signing-record.bin").exists())
        self.assertFalse((bundle / "state-snapshot.bin").exists())
        self.assertFalse((bundle / f"dag-wal-recovery-{MANIFEST_HASH}/.WRITE.lock").exists())
        pub = pc.publish_bundle_into_candidate(
            candidate_dir=self.cand,
            bundle_dir=bundle,
            expected_recovery_manifest_hash=MANIFEST_HASH,
            min_reserve_bytes=1024,
            free_space_fn=lambda _: 10_000_000,
        )
        state_wal = self.cand / "state.wal"
        self.assertFalse((self.cand / pc.PRECOPY_WAL_NAME).exists())
        self.assertEqual(os.lstat(state_wal).st_ino, self.precopy_ino)
        self.assertEqual(state_wal.read_bytes(), self.prefix + self.tail)
        self.assertEqual(pub["state_wal_sha256"], hashlib.sha256(self.prefix + self.tail).hexdigest())
        self.assertFalse(pub["qualified_for_recovery"])
        self.assertFalse(pub["certified_for_activation"])
        self.assertEqual((self.cand / "consensus-signing-record.bin").read_bytes(), b"target-preserved-record")

    def test_freeze_ack_failure_thaw_failure_and_sigkill_watchdog_contract(self) -> None:
        src_st = os.lstat(self.src_wal)
        bad_wd = FakeFreezeBackend(fail_watchdog=True)
        with self.assertRaises(pc.CaptureError):
            pc.capture_source_tail_bundle(
                source_data_dir=self.src,
                bundle_dir=self.root / "peer-capture-bundle-nowd",
                recovery_manifest_hash=MANIFEST_HASH,
                precopy_prefix_bytes=len(self.prefix),
                precopy_source_dev=src_st.st_dev,
                precopy_source_ino=src_st.st_ino,
                freeze_backend=bad_wd,
                free_space_fn=lambda _: pc.DEFAULT_MIN_RESERVE_BYTES + 2 * pc.MAX_BUNDLE_BYTES,
            )
        self.assertNotIn("freeze", bad_wd.events)

        ack_fail = FakeFreezeBackend(ack_freeze=False)
        with self.assertRaises(pc.CaptureError):
            pc.capture_source_tail_bundle(
                source_data_dir=self.src,
                bundle_dir=self.root / "peer-capture-bundle-ackfail",
                recovery_manifest_hash=MANIFEST_HASH,
                precopy_prefix_bytes=len(self.prefix),
                precopy_source_dev=src_st.st_dev,
                precopy_source_ino=src_st.st_ino,
                freeze_backend=ack_fail,
                free_space_fn=lambda _: pc.DEFAULT_MIN_RESERVE_BYTES + 2 * pc.MAX_BUNDLE_BYTES,
            )
        self.assertTrue(ack_fail.write1_occurred)
        self.assertIn("thaw", ack_fail.events)
        self.assertIn("cancel:wd-handle", ack_fail.events)

        thaw_fail = FakeFreezeBackend(ack_freeze=False, fail_thaw=True)
        with self.assertRaises(pc.CaptureError):
            pc.capture_source_tail_bundle(
                source_data_dir=self.src,
                bundle_dir=self.root / "peer-capture-bundle-thawfail",
                recovery_manifest_hash=MANIFEST_HASH,
                precopy_prefix_bytes=len(self.prefix),
                precopy_source_dev=src_st.st_dev,
                precopy_source_ino=src_st.st_ino,
                freeze_backend=thaw_fail,
                free_space_fn=lambda _: pc.DEFAULT_MIN_RESERVE_BYTES + 2 * pc.MAX_BUNDLE_BYTES,
            )
        self.assertIn("thaw", thaw_fail.events)
        self.assertNotIn("cancel:wd-handle", thaw_fail.events)

        with self.assertRaises(pc.CaptureError):
            pc.capture_source_tail_bundle(
                source_data_dir=self.src,
                bundle_dir=self.root / "peer-capture-bundle-thawedmid",
                recovery_manifest_hash=MANIFEST_HASH,
                precopy_prefix_bytes=len(self.prefix),
                precopy_source_dev=src_st.st_dev,
                precopy_source_ino=src_st.st_ino,
                freeze_backend=FakeFreezeBackend(still_frozen=False),
                free_space_fn=lambda _: pc.DEFAULT_MIN_RESERVE_BYTES + 2 * pc.MAX_BUNDLE_BYTES,
            )

        oversize_backend = FakeFreezeBackend()
        with self.assertRaises(pc.CaptureError):
            pc.capture_source_tail_bundle(
                source_data_dir=self.src,
                bundle_dir=self.root / "peer-capture-bundle-big",
                recovery_manifest_hash=MANIFEST_HASH,
                precopy_prefix_bytes=len(self.prefix),
                precopy_source_dev=src_st.st_dev,
                precopy_source_ino=src_st.st_ino,
                freeze_backend=oversize_backend,
                max_bundle_bytes=16,
                free_space_fn=lambda _: pc.DEFAULT_MIN_RESERVE_BYTES + 2 * pc.MAX_BUNDLE_BYTES,
            )
        self.assertIn("thaw", oversize_backend.events)
        self.assertFalse((self.root / "peer-capture-bundle-big" / pc.BUNDLE_MANIFEST_NAME).exists())

        freeze_sim = self.root / "sim.freeze"
        freeze_sim.write_text("1\n", encoding="ascii")
        repo_root = str(Path(__file__).resolve().parents[2])
        parent_script = (
            "import sys,time\n"
            "from pathlib import Path\n"
            "from scripts.recovery import peer_capture as pc\n"
            "h=pc.spawn_setsid_thaw_watchdog(Path(sys.argv[1]),time.monotonic()+0.4)\n"
            "print(h.pid,flush=True)\n"
            "time.sleep(30)\n"
        )
        env = dict(os.environ, PYTHONPATH=repo_root)
        mid = subprocess.Popen(
            [sys.executable, "-c", parent_script, str(freeze_sim)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
        )
        try:
            self.assertIsNotNone(mid.stdout)
            readable, _, _ = select.select([mid.stdout], [], [], 3.0)
            self.assertTrue(readable, "watchdog parent did not report readiness within three seconds")
            line = mid.stdout.readline().strip()
            self.assertTrue(line.isdigit(), "watchdog parent did not report a child PID")
            time.sleep(0.55)
            self.assertEqual(freeze_sim.read_text(encoding="ascii"), "0\n")
            freeze_sim.write_text("1\n", encoding="ascii")
            os.kill(mid.pid, signal.SIGKILL)
            mid.wait(timeout=3.0)
            wait_until = time.monotonic() + 3.0
            while time.monotonic() < wait_until and freeze_sim.read_text(encoding="ascii") != "0\n":
                time.sleep(0.05)
            self.assertEqual(freeze_sim.read_text(encoding="ascii"), "0\n")
        finally:
            if mid.poll() is None:
                os.kill(mid.pid, signal.SIGKILL)
                mid.wait(timeout=3.0)
            if mid.stdout is not None:
                mid.stdout.close()
            if mid.stderr is not None:
                mid.stderr.close()

    def test_cgroup_backend_enforces_systemd_binding_and_procs(self) -> None:
        cg_root = self.root / "fake-cgroup"
        cg_dir = cg_root / "system.slice" / "arc-validator.service"
        cg_dir.mkdir(parents=True, mode=0o700)
        (cg_dir / "cgroup.freeze").write_text("0\n", encoding="ascii")
        (cg_dir / "cgroup.events").write_text("populated 1\nfrozen 0\n", encoding="ascii")
        (cg_dir / "cgroup.procs").write_text("4242\n", encoding="ascii")
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
            proc_cgroup_reader=lambda _: "0::/user.slice/test.scope\n",
        )
        self.assertEqual(backend.verify_preflight()["main_pid"], 4242)
        for bad in (
            {**good_props, "ActiveState": "inactive"},
            {**good_props, "ControlGroup": "/system.slice/other.service"},
            {**good_props, "MainPID": "9999"},
            {**good_props, "WatchdogUSec": "30s"},
        ):
            bad_backend = pc.CgroupV2FreezeBackend(
                unit_name="arc-validator.service",
                expected_service_cgroup="/system.slice/arc-validator.service",
                expected_main_pid=4242,
                expected_service_watchdog_usec=0,
                cgroup_root=cg_root,
                service_inspector=lambda _, p=bad: dict(p),
                proc_cgroup_reader=lambda _: "0::/user.slice/test.scope\n",
            )
            with self.assertRaises(pc.CaptureError):
                bad_backend.verify_preflight()
        (cg_dir / "cgroup.procs").write_text(f"4242\n{os.getpid()}\n", encoding="ascii")
        with self.assertRaises(pc.CaptureError):
            backend.verify_preflight()
        (cg_dir / "cgroup.procs").write_text("9999\n", encoding="ascii")
        with self.assertRaises(pc.CaptureError):
            backend.verify_preflight()

    def test_source_validation_strict_allowlist_and_full_tree_stability(self) -> None:
        src_st = os.lstat(self.src_wal)
        for bad_rel in (
            f"dag-wal-recovery-{MANIFEST_HASH}/gen-{GEN_HASH}/validator-key.json",
            f"dag-wal-recovery-{MANIFEST_HASH}//CURRENT",
            "./recovery.active",
            "/recovery.active",
            f"dag-wal-recovery-{MANIFEST_HASH}/gen-{GEN_HASH}/../CURRENT",
        ):
            self.assertFalse(pc.is_allowed_bundle_relpath(bad_rel, MANIFEST_HASH))

        rotated = self.src / "wal-00000001.bin"
        rotated.write_bytes(b"seg")
        b1 = FakeFreezeBackend()
        with self.assertRaises(pc.CaptureError):
            pc.capture_source_tail_bundle(
                source_data_dir=self.src,
                bundle_dir=self.root / "peer-capture-bundle-seg",
                recovery_manifest_hash=MANIFEST_HASH,
                precopy_prefix_bytes=len(self.prefix),
                precopy_source_dev=src_st.st_dev,
                precopy_source_ino=src_st.st_ino,
                freeze_backend=b1,
                free_space_fn=lambda _: pc.DEFAULT_MIN_RESERVE_BYTES + 2 * pc.MAX_BUNDLE_BYTES,
            )
        self.assertIn("thaw", b1.events)
        rotated.unlink()

        bad_dag_key = self.src / f"dag-wal-recovery-{MANIFEST_HASH}/gen-{GEN_HASH}/validator-key.json"
        bad_dag_key.write_bytes(b"forbidden")
        with self.assertRaises(pc.CaptureError):
            self._capture_valid_bundle("peer-capture-bundle-keyjson")
        bad_dag_key.unlink()

        (self.src / "recovery.active").write_text(f"{'0' * 64}\n", encoding="ascii")
        with self.assertRaises(pc.CaptureError):
            self._capture_valid_bundle("peer-capture-bundle-active")
        (self.src / "recovery.active").write_text(f"{MANIFEST_HASH}\n", encoding="ascii")

        def mutate_earlier_file(rel: str) -> None:
            if rel.endswith("/CURRENT"):
                (self.src / "recovery.active").write_text(f"{'1' * 64}\n", encoding="ascii")

        b_early = FakeFreezeBackend()
        with self.assertRaises(pc.CaptureError):
            pc.capture_source_tail_bundle(
                source_data_dir=self.src,
                bundle_dir=self.root / "peer-capture-bundle-earlymut",
                recovery_manifest_hash=MANIFEST_HASH,
                precopy_prefix_bytes=len(self.prefix),
                precopy_source_dev=src_st.st_dev,
                precopy_source_ino=src_st.st_ino,
                freeze_backend=b_early,
                free_space_fn=lambda _: pc.DEFAULT_MIN_RESERVE_BYTES + 2 * pc.MAX_BUNDLE_BYTES,
                on_after_file_copy=mutate_earlier_file,
            )
        self.assertIn("thaw", b_early.events)
        (self.src / "recovery.active").write_text(f"{MANIFEST_HASH}\n", encoding="ascii")

        for bad_deadline in (True, math.nan, math.inf, 0.0, 9.0):
            with self.assertRaises(pc.CaptureError):
                pc.capture_source_tail_bundle(
                    source_data_dir=self.src,
                    bundle_dir=self.root / "peer-capture-bundle-baddl",
                    recovery_manifest_hash=MANIFEST_HASH,
                    precopy_prefix_bytes=len(self.prefix),
                    precopy_source_dev=src_st.st_dev,
                    precopy_source_ino=src_st.st_ino,
                    freeze_backend=FakeFreezeBackend(),
                    deadline_s=bad_deadline,
                    free_space_fn=lambda _: pc.DEFAULT_MIN_RESERVE_BYTES + 2 * pc.MAX_BUNDLE_BYTES,
                )

    def test_publish_rejects_same_inode_mutation_nesting_flags_and_missing_mandatory(self) -> None:
        bundle, _, _ = self._capture_valid_bundle("peer-capture-bundle-pubfail")
        for bad_reserve in (True, -1):
            with self.assertRaises(pc.CaptureError):
                pc.publish_bundle_into_candidate(
                    candidate_dir=self.cand,
                    bundle_dir=bundle,
                    expected_recovery_manifest_hash=MANIFEST_HASH,
                    min_reserve_bytes=bad_reserve,
                    free_space_fn=lambda _: 10_000_000,
                )

        nested_bundle = self.cand / "peer-capture-bundle-nested"
        nested_bundle.mkdir(mode=0o700)
        with self.assertRaises(pc.CaptureError):
            pc.publish_bundle_into_candidate(
                candidate_dir=self.cand,
                bundle_dir=nested_bundle,
                expected_recovery_manifest_hash=MANIFEST_HASH,
                min_reserve_bytes=1024,
                free_space_fn=lambda _: 10_000_000,
            )
        nested_bundle.rmdir()

        bman_path = bundle / pc.BUNDLE_MANIFEST_NAME
        orig_bman = json.loads(bman_path.read_text(encoding="utf-8"))
        claimed = {**orig_bman, "qualified_for_recovery": True}
        bman_path.write_text(json.dumps(claimed), encoding="utf-8")
        with self.assertRaises(pc.CaptureError):
            pc.publish_bundle_into_candidate(
                candidate_dir=self.cand,
                bundle_dir=bundle,
                expected_recovery_manifest_hash=MANIFEST_HASH,
                min_reserve_bytes=1024,
                free_space_fn=lambda _: 10_000_000,
            )
        bman_path.write_text(json.dumps(orig_bman), encoding="utf-8")

        extra_unlisted = bundle / "validator.key"
        extra_unlisted.write_bytes(b"sneaky-key")
        with self.assertRaises(pc.CaptureError):
            pc.publish_bundle_into_candidate(
                candidate_dir=self.cand,
                bundle_dir=bundle,
                expected_recovery_manifest_hash=MANIFEST_HASH,
                min_reserve_bytes=1024,
                free_space_fn=lambda _: 10_000_000,
            )
        extra_unlisted.unlink()

        precopy_path = self.cand / pc.PRECOPY_WAL_NAME

        def mutate_same_inode_same_size() -> None:
            st0 = os.lstat(precopy_path)
            fd = os.open(precopy_path, os.O_RDWR)
            try:
                os.pwrite(fd, b"Z" * 16, 0)
            finally:
                os.close(fd)
            os.utime(precopy_path, ns=(st0.st_atime_ns, st0.st_mtime_ns))

        with self.assertRaises(pc.CaptureError):
            pc.publish_bundle_into_candidate(
                candidate_dir=self.cand,
                bundle_dir=bundle,
                expected_recovery_manifest_hash=MANIFEST_HASH,
                min_reserve_bytes=1024,
                free_space_fn=lambda _: 10_000_000,
                on_before_tail_append=mutate_same_inode_same_size,
            )
        self.assertFalse((self.cand / "state.wal").exists())
        self.assertFalse((self.cand / pc.PUBLISH_MANIFEST_NAME).exists())
        with self.assertRaises(pc.CaptureError):
            pc.publish_bundle_into_candidate(
                candidate_dir=self.cand,
                bundle_dir=bundle,
                expected_recovery_manifest_hash=MANIFEST_HASH,
                min_reserve_bytes=10_000_000,
                free_space_fn=lambda _: 1024,
            )
        self.assertFalse((self.cand / "state.wal").exists())

        (bundle / pc.BUNDLE_TAIL_NAME).write_bytes(b"corrupted-tail")
        with self.assertRaises(pc.CaptureError):
            pc.publish_bundle_into_candidate(
                candidate_dir=self.cand,
                bundle_dir=bundle,
                expected_recovery_manifest_hash=MANIFEST_HASH,
                min_reserve_bytes=1024,
                free_space_fn=lambda _: 10_000_000,
            )
        self.assertFalse((self.cand / "state.wal").exists())
        self.assertEqual((self.cand / pc.PRECOPY_WAL_NAME).read_bytes(), b"Z" * 16 + self.prefix[16:])

        bundle2, _, _ = self._capture_valid_bundle("peer-capture-bundle-pubfail2")
        (self.cand / pc.PRECOPY_WAL_NAME).write_bytes(b"X" * len(self.prefix))
        with self.assertRaises(pc.CaptureError):
            pc.publish_bundle_into_candidate(
                candidate_dir=self.cand,
                bundle_dir=bundle2,
                expected_recovery_manifest_hash=MANIFEST_HASH,
                min_reserve_bytes=1024,
                free_space_fn=lambda _: 10_000_000,
            )
        self.assertFalse((self.cand / "state.wal").exists())
        self.assertFalse((self.cand / pc.PUBLISH_MANIFEST_NAME).exists())


if __name__ == "__main__":
    unittest.main()
