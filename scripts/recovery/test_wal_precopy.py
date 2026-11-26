#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import io
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts.recovery import wal_precopy as wp


class NoEofStream:
    def __init__(self, data: bytes) -> None:
        self._buf = data
        self._pos = 0

    def readline(self, limit: int = -1) -> bytes:
        if self._pos >= len(self._buf):
            raise AssertionError("receiver attempted read past footer / waited for EOF")
        nl = self._buf.find(b"\n", self._pos)
        end = len(self._buf) if nl == -1 else nl + 1
        if limit >= 0:
            end = min(end, self._pos + limit)
        out = self._buf[self._pos : end]
        self._pos = end
        return out

    def read(self, size: int = -1) -> bytes:
        if self._pos >= len(self._buf):
            raise AssertionError("receiver attempted read past footer / waited for EOF")
        end = len(self._buf) if size < 0 else min(len(self._buf), self._pos + size)
        out = self._buf[self._pos : end]
        self._pos = end
        return out


class WalPrecopyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory(prefix="arc-precopy-test-")
        self.root = Path(self.tmp.name).resolve()
        self.src_dir = self.root / "src"
        self.src_dir.mkdir()
        self.src_wal = self.src_dir / "state.wal"
        self.payload = b"arc-wal-frame-" * 512
        self.src_wal.write_bytes(self.payload)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def _produce(self, **kwargs: object) -> tuple[bytes, dict[str, object]]:
        buf = io.BytesIO()
        footer = wp.stream_source(
            self.src_wal, buf, chunk_bytes=1024, rate_bytes_per_sec=0, **kwargs
        )
        return buf.getvalue(), footer

    def test_happy_path_exact_framing_no_eof_and_private_modes(self) -> None:
        raw, footer = self._produce()
        cand = self.root / "peer-recovery-candidate-sgp"
        manifest = wp.receive_stream(
            cand,
            NoEofStream(raw),
            min_reserve_bytes=1024,
            reserve_check_bytes=1024,
            free_space_fn=lambda _: 10_000_000,
        )
        wal_out = cand / wp.TARGET_WAL_NAME
        man_out = cand / wp.TARGET_MANIFEST_NAME
        self.assertEqual(wal_out.read_bytes(), self.payload)
        self.assertEqual(manifest["sha256"], hashlib.sha256(self.payload).hexdigest())
        self.assertEqual(manifest["sha256"], footer["sha256"])
        self.assertFalse(manifest["qualified_for_recovery"])
        self.assertFalse(manifest["source_atomicity_claimed"])
        self.assertEqual(manifest["status"], "PRECOPY_ONLY_UNQUALIFIED")
        self.assertEqual(os.stat(cand).st_mode & 0o777, 0o700)
        self.assertEqual(os.stat(wal_out).st_mode & 0o777, 0o600)
        self.assertEqual(os.stat(man_out).st_mode & 0o777, 0o600)
        self.assertEqual(json.loads(man_out.read_text("utf-8")), manifest)

    def test_live_append_during_stream_succeeds_with_exact_prefix(self) -> None:
        appended = b"tail-append-bytes" * 64

        def append_more() -> None:
            with open(self.src_wal, "ab") as fh:
                fh.write(appended)

        raw, footer = self._produce(on_after_first_chunk=append_more)
        self.assertEqual(footer["bytes_sent"], len(self.payload))
        self.assertEqual(footer["source_end_size"], len(self.payload) + len(appended))
        cand = self.root / "peer-recovery-candidate-append"
        manifest = wp.receive_stream(
            cand, io.BytesIO(raw), min_reserve_bytes=1024, free_space_fn=lambda _: 10_000_000
        )
        self.assertEqual((cand / wp.TARGET_WAL_NAME).read_bytes(), self.payload)
        self.assertEqual(manifest["source_end_size"], len(self.payload) + len(appended))

    def test_source_truncation_and_replacement_rejected(self) -> None:
        buf = io.BytesIO()
        with self.assertRaises(wp.PrecopyError):
            wp.stream_source(
                self.src_wal,
                buf,
                chunk_bytes=1024,
                rate_bytes_per_sec=0,
                on_after_first_chunk=lambda: os.truncate(self.src_wal, 64),
            )
        cand = self.root / "peer-recovery-candidate-trunc"
        with self.assertRaises(wp.PrecopyError):
            wp.receive_stream(
                cand, io.BytesIO(buf.getvalue()), min_reserve_bytes=1024, free_space_fn=lambda _: 10_000_000
            )
        self.assertFalse((cand / wp.TARGET_MANIFEST_NAME).exists())

        self.src_wal.write_bytes(self.payload)
        replacement = self.src_dir / "state.wal.new"
        replacement.write_bytes(self.payload + b"extra")
        with self.assertRaises(wp.PrecopyError):
            wp.stream_source(
                self.src_wal,
                io.BytesIO(),
                chunk_bytes=1024,
                rate_bytes_per_sec=0,
                on_after_first_chunk=lambda: os.replace(replacement, self.src_wal),
            )

        bad_name = self.src_dir / "other.wal"
        bad_name.write_bytes(b"x")
        with self.assertRaises(wp.PrecopyError):
            wp.stream_source(bad_name, io.BytesIO(), rate_bytes_per_sec=0)
        hardlink = self.src_dir / "state.wal.link"
        os.link(self.src_wal, hardlink)
        with self.assertRaises(wp.PrecopyError):
            wp.stream_source(self.src_wal, io.BytesIO(), rate_bytes_per_sec=0)
        hardlink.unlink()

    def test_target_symlink_existing_dir_and_free_space_guards(self) -> None:
        raw, _ = self._produce()
        existing = self.root / "peer-recovery-candidate-exists"
        existing.mkdir()
        with self.assertRaises(wp.PrecopyError):
            wp.receive_stream(existing, io.BytesIO(raw), min_reserve_bytes=1024, free_space_fn=lambda _: 10_000_000)
        sym_parent = self.root / "sym-parent"
        os.symlink(self.src_dir, sym_parent)
        with self.assertRaises(wp.PrecopyError):
            wp.receive_stream(
                sym_parent / "peer-recovery-candidate-sym",
                io.BytesIO(raw),
                min_reserve_bytes=1024,
                free_space_fn=lambda _: 10_000_000,
            )
        with self.assertRaises(wp.PrecopyError):
            wp.receive_stream(
                self.root / "peer-recovery-candidate-nospace",
                io.BytesIO(raw),
                min_reserve_bytes=5000,
                free_space_fn=lambda _: len(self.payload) + 1000,
            )
        calls = iter([10_000_000, 10_000_000, 100])
        mid_cand = self.root / "peer-recovery-candidate-midspace"
        with self.assertRaises(wp.PrecopyError):
            wp.receive_stream(
                mid_cand,
                io.BytesIO(raw),
                min_reserve_bytes=1024,
                reserve_check_bytes=1024,
                free_space_fn=lambda _: next(calls, 100),
            )
        self.assertTrue((mid_cand / wp.TARGET_WAL_NAME).exists())
        self.assertFalse((mid_cand / wp.TARGET_MANIFEST_NAME).exists())

    def test_unsafe_header_truncated_payload_and_bad_footer_rejected(self) -> None:
        raw, _ = self._produce()
        hdr_line, rest = raw.split(b"\n", 1)
        hdr = json.loads(hdr_line.decode("utf-8"))
        for idx, mutated in enumerate(
            [
                {**hdr, "source_path": "/etc/passwd"},
                {**hdr, "protocol_version": 2},
                {**hdr, "protocol_version": True},
                {**hdr, "prefix_bytes": True},
                {**hdr, "prefix_bytes": wp.MAX_PREFIX_BYTES + 1},
            ]
        ):
            bad_stream = (json.dumps(mutated) + "\n").encode("utf-8") + rest
            with self.assertRaises(wp.PrecopyError):
                wp.receive_stream(
                    self.root / f"peer-recovery-candidate-hdr-{idx}",
                    io.BytesIO(bad_stream),
                    min_reserve_bytes=1024,
                    free_space_fn=lambda _: 10_000_000,
                )
        oversized_hdr = b"{" + b" " * wp.MAX_CONTROL_LINE_BYTES + b"}\n"
        with self.assertRaises(wp.PrecopyError):
            wp.receive_stream(
                self.root / "peer-recovery-candidate-bighdr",
                io.BytesIO(oversized_hdr),
                min_reserve_bytes=1024,
                free_space_fn=lambda _: 10_000_000,
            )
        trunc_cand = self.root / "peer-recovery-candidate-short"
        with self.assertRaises(wp.PrecopyError):
            wp.receive_stream(
                trunc_cand,
                io.BytesIO(hdr_line + b"\n" + self.payload[:100]),
                min_reserve_bytes=1024,
                free_space_fn=lambda _: 10_000_000,
            )
        self.assertTrue((trunc_cand / wp.TARGET_WAL_NAME).exists())
        self.assertFalse((trunc_cand / wp.TARGET_MANIFEST_NAME).exists())

        payload_and_hdr = hdr_line + b"\n" + self.payload
        ftr = json.loads(raw[len(payload_and_hdr) :].decode("utf-8"))
        for idx, bad_ftr in enumerate(
            [
                {**ftr, "sha256": "0" * 64},
                {**ftr, "protocol_version": True},
                {**ftr, "source_end_size": len(self.payload) - 1},
                {**ftr, "source_ino": ftr["source_ino"] + 1},
            ]
        ):
            bad_cand = self.root / f"peer-recovery-candidate-ftr-{idx}"
            with self.assertRaises(wp.PrecopyError):
                wp.receive_stream(
                    bad_cand,
                    io.BytesIO(payload_and_hdr + (json.dumps(bad_ftr) + "\n").encode("utf-8")),
                    min_reserve_bytes=1024,
                    free_space_fn=lambda _: 10_000_000,
                )
            self.assertFalse((bad_cand / wp.TARGET_MANIFEST_NAME).exists())

    def test_throttle_deadline_and_dry_run_without_side_effects(self) -> None:
        now = [0.0]
        sleeps: list[float] = []

        def fake_sleep(dt: float) -> None:
            sleeps.append(dt)
            now[0] += dt

        buf = io.BytesIO()
        wp.stream_source(
            self.src_wal,
            buf,
            chunk_bytes=1024,
            rate_bytes_per_sec=2048,
            deadline_s=60.0,
            clock=lambda: now[0],
            sleeper=fake_sleep,
        )
        self.assertTrue(sleeps and all(dt > 0 for dt in sleeps))
        with self.assertRaises(wp.PrecopyError):
            wp.stream_source(
                self.src_wal,
                io.BytesIO(),
                chunk_bytes=1024,
                rate_bytes_per_sec=1024,
                deadline_s=0.1,
                clock=lambda: now[0],
                sleeper=fake_sleep,
            )
        dry_cand = self.root / "peer-recovery-candidate-dry"
        report = wp.run_dry_run(
            source_wal=self.src_wal,
            candidate_dir=dry_cand,
            min_reserve_bytes=1024,
            free_space_fn=lambda _: 10_000_000,
        )
        self.assertEqual(report["status"], "DRY_RUN_METADATA_ONLY")
        self.assertFalse(dry_cand.exists())

    def test_nested_alias_and_invalid_deadlines_are_rejected_before_writes(self) -> None:
        alias = self.root / "alias"
        alias.symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(wp.PrecopyError):
            wp.stream_source(alias / "src" / "state.wal", io.BytesIO())
        raw, _ = self._produce()
        for index, deadline in enumerate([float("nan"), float("inf"), 0.0, 3601.0]):
            candidate = self.root / f"peer-recovery-candidate-deadline-{index}"
            with self.assertRaises(wp.PrecopyError):
                wp.receive_stream(candidate, io.BytesIO(raw), deadline_s=deadline)
            self.assertFalse(candidate.exists())
            with self.assertRaises(wp.PrecopyError):
                wp.stream_source(self.src_wal, io.BytesIO(), deadline_s=deadline)

    def test_manifest_is_not_published_if_its_durability_barrier_fails(self) -> None:
        raw, _ = self._produce()
        candidate = self.root / "peer-recovery-candidate-manifest-failure"
        with mock.patch.object(wp.os, "fsync", side_effect=[None, None, OSError("fsync failed")]):
            with self.assertRaisesRegex(OSError, "fsync failed"):
                wp.receive_stream(
                    candidate, io.BytesIO(raw), min_reserve_bytes=1024,
                    free_space_fn=lambda _: 10_000_000,
                )
        self.assertTrue((candidate / wp.TARGET_WAL_NAME).exists())
        self.assertFalse((candidate / wp.TARGET_MANIFEST_NAME).exists())


if __name__ == "__main__":
    unittest.main()
