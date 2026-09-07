#!/usr/bin/env python3
"""Focused, production-free tests for the legacy WAL normalization gate."""

from __future__ import annotations

import copy
import hashlib
import importlib.util
import json
import os
import pathlib
import stat
import struct
import tempfile
import unittest
import unittest.mock
import zlib


SCRIPT = pathlib.Path(__file__).with_name("normalize-legacy-wal.py")
SPEC = importlib.util.spec_from_file_location("normalize_legacy_wal", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
normalizer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(normalizer)


def digest(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def frame(height: int, sequence: int, marker: int) -> bytes:
    # The stable bincode WalEntry envelope is u64 height, u64 sequence, encoded
    # operation bytes, then little-endian CRC32 of every preceding payload byte.
    payload = struct.pack("<QQI", height, sequence, marker)
    payload += struct.pack("<I", zlib.crc32(payload) & 0xFFFFFFFF)
    return struct.pack("<I", len(payload)) + payload


def rewrite_sequence(raw_frame: bytes, sequence: int) -> bytes:
    length = struct.unpack_from("<I", raw_frame)[0]
    payload = bytearray(raw_frame[4 : 4 + length])
    struct.pack_into("<Q", payload, 8, sequence)
    struct.pack_into("<I", payload, len(payload) - 4, zlib.crc32(payload[:-4]) & 0xFFFFFFFF)
    return raw_frame[:4] + payload


def file_projection(path: pathlib.Path) -> tuple[int, ...]:
    details = path.lstat()
    return (
        details.st_dev,
        details.st_ino,
        details.st_mode,
        details.st_uid,
        details.st_gid,
        details.st_nlink,
        details.st_size,
        details.st_mtime_ns,
        details.st_ctime_ns,
    )


class LegacyWalNormalizerTests(unittest.TestCase):
    def fixture(self, root: pathlib.Path) -> tuple[pathlib.Path, pathlib.Path, dict, bytes]:
        source = root / "state.wal"
        snapshot = root / "state.snapshot.lz4"
        original_frames = [
            frame(0, 0, 10),
            frame(1, 2, 11),
            frame(1, 1, 12),
            frame(2, 3, 13),
        ]
        offsets: list[int] = []
        cursor = 0
        for item in original_frames:
            offsets.append(cursor)
            cursor += len(item)
        source_raw = b"".join(original_frames)
        expected = b"".join(
            (
                original_frames[0],
                rewrite_sequence(original_frames[1], 1),
                rewrite_sequence(original_frames[2], 2),
                original_frames[3],
            )
        )
        source.write_bytes(source_raw)
        snapshot.write_bytes(b"exact-snapshot")
        plan = {
            "schema": normalizer.SCHEMA,
            "node": "lax",
            "source_wal": {"sha256": digest(source_raw), "size": len(source_raw)},
            "source_snapshot": {
                "sha256": digest(snapshot.read_bytes()),
                "size": snapshot.stat().st_size,
            },
            "derivative_wal": {"sha256": digest(expected), "size": len(expected)},
            "head": {
                "height": 2,
                "block_hash": "1" * 64,
                "state_root": "2" * 64,
            },
            "partition": [
                {
                    "kind": "selected-frame-run",
                    "source_start": 0,
                    "source_end": len(source_raw),
                    "sha256": digest(source_raw),
                    "frame_count": 4,
                    "source_first_sequence": 0,
                    "source_last_sequence": 3,
                    "sequence_delta": 0,
                    "derivative_first_sequence": 0,
                    "derivative_last_sequence": 3,
                }
            ],
            "sequence_rewrites": [
                {
                    "source_offset": offsets[1],
                    "source_sequence": 2,
                    "derivative_sequence": 1,
                },
                {
                    "source_offset": offsets[2],
                    "source_sequence": 1,
                    "derivative_sequence": 2,
                },
            ],
        }
        return source, snapshot, plan, expected

    def test_exact_transform_closes_every_opened_descriptor(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source, snapshot, plan, expected = self.fixture(root)
            source_before = file_projection(source)
            snapshot_before = file_projection(snapshot)
            opened: set[int] = set()
            real_open, real_close = os.open, os.close

            def tracked_open(*args, **kwargs):
                descriptor = real_open(*args, **kwargs)
                opened.add(descriptor)
                return descriptor

            def tracked_close(descriptor):
                opened.discard(descriptor)
                return real_close(descriptor)

            with unittest.mock.patch.object(normalizer.os, "open", tracked_open), \
                    unittest.mock.patch.object(normalizer.os, "close", tracked_close):
                receipt = normalizer.normalize(
                    plan,
                    "3" * 64,
                    source,
                    snapshot,
                    root / "normalized-source",
                )

            self.assertEqual(opened, set())
            derivative = root / "normalized-source" / "state.wal"
            self.assertEqual(derivative.read_bytes(), expected)
            self.assertEqual(receipt["derivative_wal"]["sha256"], digest(expected))
            self.assertEqual(receipt["selected_frame_count"], 4)
            self.assertEqual(receipt["excluded_bytes"], 0)
            self.assertTrue(receipt["source_unchanged"])
            self.assertEqual(file_projection(source), source_before)
            self.assertEqual(file_projection(snapshot), snapshot_before)
            self.assertEqual(stat.S_IMODE(derivative.lstat().st_mode), 0o400)
            self.assertEqual(
                stat.S_IMODE((root / "normalized-source").lstat().st_mode), 0o500
            )

    def test_failed_output_is_never_promoted_and_sources_remain_immutable(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source, snapshot, plan, _expected = self.fixture(root)
            source_raw, snapshot_raw = source.read_bytes(), snapshot.read_bytes()
            source_before, snapshot_before = file_projection(source), file_projection(snapshot)
            bad = copy.deepcopy(plan)
            bad["derivative_wal"]["sha256"] = "0" * 64
            output = root / "failed-normalization"
            with self.assertRaisesRegex(
                normalizer.NormalizationError, "derivative differs"
            ):
                normalizer.normalize(bad, "4" * 64, source, snapshot, output)
            self.assertFalse((output / "state.wal").exists())
            partial = output / "state.wal.partial"
            self.assertTrue(partial.is_file())
            self.assertEqual(stat.S_IMODE(partial.lstat().st_mode), 0o400)
            self.assertEqual(source.read_bytes(), source_raw)
            self.assertEqual(snapshot.read_bytes(), snapshot_raw)
            self.assertEqual(file_projection(source), source_before)
            self.assertEqual(file_projection(snapshot), snapshot_before)
            with self.assertRaisesRegex(
                normalizer.NormalizationError, "output directory already exists"
            ):
                normalizer.normalize(bad, "4" * 64, source, snapshot, output)

    def test_plan_rejects_rewrite_outside_selected_partition(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            _source, _snapshot, plan, _expected = self.fixture(root)
            plan["sequence_rewrites"][0]["source_offset"] = plan["source_wal"]["size"]
            raw = normalizer.canonical_bytes(plan)
            with self.assertRaisesRegex(
                normalizer.NormalizationError, "outside every selected frame run"
            ):
                normalizer.parse_plan(raw, digest(raw))

    def test_zero_fill_and_stale_forks_are_excluded_by_exact_hash(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            snapshot = root / "state.snapshot.lz4"
            snapshot.write_bytes(b"snapshot")
            prefix = frame(0, 0, 10) + frame(1, 1, 11)
            zeros = b"\0" * 12
            stale = frame(2, 5, 20) + frame(3, 6, 21)
            current_frames = (frame(2, 5, 30), frame(3, 6, 31))
            current = b"".join(current_frames)
            source_raw = prefix + zeros + stale + current
            source = root / "state.wal"
            source.write_bytes(source_raw)
            expected = prefix + b"".join(
                rewrite_sequence(item, sequence)
                for item, sequence in zip(current_frames, (2, 3))
            )
            first_end = len(prefix)
            zero_end = first_end + len(zeros)
            stale_end = zero_end + len(stale)
            plan = {
                "schema": normalizer.SCHEMA,
                "node": "ams",
                "source_wal": {"sha256": digest(source_raw), "size": len(source_raw)},
                "source_snapshot": {
                    "sha256": digest(snapshot.read_bytes()),
                    "size": snapshot.stat().st_size,
                },
                "derivative_wal": {"sha256": digest(expected), "size": len(expected)},
                "head": {
                    "height": 3,
                    "block_hash": "1" * 64,
                    "state_root": "2" * 64,
                },
                "partition": [
                    {
                        "kind": "selected-frame-run",
                        "source_start": 0,
                        "source_end": first_end,
                        "sha256": digest(prefix),
                        "frame_count": 2,
                        "source_first_sequence": 0,
                        "source_last_sequence": 1,
                        "sequence_delta": 0,
                        "derivative_first_sequence": 0,
                        "derivative_last_sequence": 1,
                    },
                    {
                        "kind": "excluded-zero-fill",
                        "source_start": first_end,
                        "source_end": zero_end,
                        "sha256": digest(zeros),
                        "classification": "interior-zero-fill-gap",
                    },
                    {
                        "kind": "excluded-valid-frame-run",
                        "source_start": zero_end,
                        "source_end": stale_end,
                        "sha256": digest(stale),
                        "classification": "stale-valid-fork",
                        "frame_count": 2,
                        "first_sequence": 5,
                        "last_sequence": 6,
                    },
                    {
                        "kind": "selected-frame-run",
                        "source_start": stale_end,
                        "source_end": len(source_raw),
                        "sha256": digest(current),
                        "frame_count": 2,
                        "source_first_sequence": 5,
                        "source_last_sequence": 6,
                        "sequence_delta": -3,
                        "derivative_first_sequence": 2,
                        "derivative_last_sequence": 3,
                    },
                ],
                "sequence_rewrites": [],
            }
            receipt = normalizer.normalize(
                plan, "5" * 64, source, snapshot, root / "normalized-source"
            )
            self.assertEqual(
                (root / "normalized-source" / "state.wal").read_bytes(), expected
            )
            self.assertEqual(receipt["excluded_bytes"], len(zeros) + len(stale))
            self.assertEqual(receipt["selected_frame_count"], 4)

    def test_repository_manifests_are_canonical_and_exact(self) -> None:
        for node in ("lax", "ams"):
            with self.subTest(node=node):
                path = SCRIPT.with_name(f"legacy-wal-normalization-{node}.json")
                raw = path.read_bytes()
                plan = normalizer.parse_plan(raw, digest(raw))
                self.assertEqual(plan["node"], node)
                self.assertEqual(
                    sum(
                        row["source_end"] - row["source_start"]
                        for row in plan["partition"]
                        if row["kind"] == "selected-frame-run"
                    ),
                    plan["derivative_wal"]["size"],
                )


if __name__ == "__main__":
    unittest.main()
