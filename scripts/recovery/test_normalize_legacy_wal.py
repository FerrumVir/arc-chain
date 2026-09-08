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

    def append_aware_plan(
        self,
        exact_plan: dict,
        *,
        maximum_bytes: int = 1024 * 1024,
        maximum_frames: int = 1024,
    ) -> dict:
        terminal = exact_plan["partition"][-1]
        self.assertEqual(terminal["kind"], "selected-frame-run")
        return {
            "schema": normalizer.SCHEMA_V2,
            "node": exact_plan["node"],
            "base_source_wal": exact_plan["source_wal"],
            "base_source_snapshot": exact_plan["source_snapshot"],
            "base_derivative_wal": exact_plan["derivative_wal"],
            "base_head": exact_plan["head"],
            "partition": exact_plan["partition"],
            "sequence_rewrites": exact_plan["sequence_rewrites"],
            "append_policy": {
                "mode": normalizer.APPEND_MODE,
                "maximum_bytes": maximum_bytes,
                "maximum_frames": maximum_frames,
                "source_first_sequence": terminal["source_last_sequence"] + 1,
                "derivative_first_sequence": terminal[
                    "derivative_last_sequence"
                ]
                + 1,
                "sequence_delta": terminal["sequence_delta"],
            },
            "snapshot_policy": {
                "mode": normalizer.SNAPSHOT_MODE,
                "maximum_size": normalizer.MAX_SNAPSHOT_BYTES,
            },
        }

    def parsed_plan(self, plan: dict) -> tuple[dict, str]:
        raw = normalizer.canonical_bytes(plan)
        plan_sha256 = digest(raw)
        return normalizer.parse_plan(raw, plan_sha256), plan_sha256

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

    def test_v1_remains_exact_and_rejects_an_append(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source, snapshot, plan, _expected = self.fixture(root)
            source.write_bytes(source.read_bytes() + frame(3, 4, 14))
            with self.assertRaisesRegex(
                normalizer.NormalizationError,
                "source WAL differs from the content-addressed plan",
            ):
                normalizer.normalize(
                    plan, "4" * 64, source, snapshot, root / "normalized-source"
                )

    def test_v2_accepts_only_a_fully_proved_append_and_binds_live_snapshot(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source, snapshot, exact_plan, expected_base = self.fixture(root)
            plan, plan_sha256 = self.parsed_plan(
                self.append_aware_plan(exact_plan)
            )
            suffix_frames = (frame(3, 4, 14), frame(4, 5, 15))
            suffix = b"".join(suffix_frames)
            source.write_bytes(source.read_bytes() + suffix)
            live_snapshot = b"new-live-snapshot-at-a-later-head"
            snapshot.write_bytes(live_snapshot)

            receipt = normalizer.normalize(
                plan,
                plan_sha256,
                source,
                snapshot,
                root / "normalized-source",
            )

            derivative = expected_base + suffix
            self.assertEqual(
                (root / "normalized-source" / "state.wal").read_bytes(), derivative
            )
            self.assertEqual(receipt["schema"], normalizer.RECEIPT_SCHEMA_V2)
            self.assertNotIn("head", receipt)
            self.assertEqual(receipt["base_head"], exact_plan["head"])
            self.assertEqual(receipt["base_source_wal"], exact_plan["source_wal"])
            self.assertEqual(
                receipt["base_source_snapshot"], exact_plan["source_snapshot"]
            )
            self.assertEqual(receipt["source_wal"]["sha256"], digest(source.read_bytes()))
            self.assertEqual(receipt["source_snapshot"]["sha256"], digest(live_snapshot))
            self.assertEqual(receipt["derivative_wal"]["sha256"], digest(derivative))
            self.assertEqual(receipt["base_selected_frame_count"], 4)
            self.assertEqual(receipt["selected_frame_count"], 6)
            self.assertEqual(
                receipt["transform"],
                "copy-reviewed-base-runs-rewrite-sequence-and-crc32-then-append-"
                "strict-contiguous-frames",
            )
            self.assertEqual(
                receipt["appended_suffix"],
                {
                    "source_start": exact_plan["source_wal"]["size"],
                    "source_end": source.stat().st_size,
                    "source_bytes": len(suffix),
                    "source_sha256": digest(suffix),
                    "derivative_start": len(expected_base),
                    "derivative_end": len(derivative),
                    "derivative_bytes": len(suffix),
                    "derivative_sha256": digest(suffix),
                    "frame_count": 2,
                    "source_first_sequence": 4,
                    "source_last_sequence": 5,
                    "derivative_first_sequence": 4,
                    "derivative_last_sequence": 5,
                    "sequence_delta": 0,
                },
            )

    def test_v2_extends_using_terminal_selected_run_delta(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source = root / "state.wal"
            snapshot = root / "state.snapshot.lz4"
            base_frames = (frame(10, 178, 20), frame(11, 179, 21))
            base_source = b"".join(base_frames)
            base_derivative = b"".join(
                rewrite_sequence(item, sequence)
                for item, sequence in zip(base_frames, (0, 1))
            )
            source.write_bytes(base_source)
            snapshot.write_bytes(b"reviewed-snapshot")
            exact_plan = {
                "schema": normalizer.SCHEMA_V1,
                "node": "ams",
                "source_wal": {
                    "sha256": digest(base_source),
                    "size": len(base_source),
                },
                "source_snapshot": {
                    "sha256": digest(snapshot.read_bytes()),
                    "size": snapshot.stat().st_size,
                },
                "derivative_wal": {
                    "sha256": digest(base_derivative),
                    "size": len(base_derivative),
                },
                "head": {
                    "height": 11,
                    "block_hash": "1" * 64,
                    "state_root": "2" * 64,
                },
                "partition": [
                    {
                        "kind": "selected-frame-run",
                        "source_start": 0,
                        "source_end": len(base_source),
                        "sha256": digest(base_source),
                        "frame_count": 2,
                        "source_first_sequence": 178,
                        "source_last_sequence": 179,
                        "sequence_delta": -178,
                        "derivative_first_sequence": 0,
                        "derivative_last_sequence": 1,
                    }
                ],
                "sequence_rewrites": [],
            }
            plan, plan_sha256 = self.parsed_plan(
                self.append_aware_plan(exact_plan)
            )
            appended_frames = (frame(12, 180, 22), frame(13, 181, 23))
            source.write_bytes(base_source + b"".join(appended_frames))

            receipt = normalizer.normalize(
                plan,
                plan_sha256,
                source,
                snapshot,
                root / "normalized-source",
            )

            appended_derivative = b"".join(
                rewrite_sequence(item, sequence)
                for item, sequence in zip(appended_frames, (2, 3))
            )
            self.assertEqual(
                (root / "normalized-source" / "state.wal").read_bytes(),
                base_derivative + appended_derivative,
            )
            self.assertEqual(receipt["appended_suffix"]["sequence_delta"], -178)
            self.assertEqual(
                receipt["appended_suffix"]["derivative_first_sequence"], 2
            )
            self.assertEqual(
                receipt["appended_suffix"]["derivative_last_sequence"], 3
            )
            self.assertEqual(
                receipt["appended_suffix"]["derivative_sha256"],
                digest(appended_derivative),
            )

    def test_v2_records_an_empty_append_without_inventing_sequence_endpoints(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source, snapshot, exact_plan, expected = self.fixture(root)
            plan, plan_sha256 = self.parsed_plan(
                self.append_aware_plan(exact_plan)
            )
            receipt = normalizer.normalize(
                plan,
                plan_sha256,
                source,
                snapshot,
                root / "normalized-source",
            )
            empty_hash = digest(b"")
            suffix = receipt["appended_suffix"]
            self.assertEqual(suffix["source_bytes"], 0)
            self.assertEqual(suffix["derivative_bytes"], 0)
            self.assertEqual(suffix["source_sha256"], empty_hash)
            self.assertEqual(suffix["derivative_sha256"], empty_hash)
            self.assertEqual(suffix["frame_count"], 0)
            self.assertIsNone(suffix["source_first_sequence"])
            self.assertIsNone(suffix["source_last_sequence"])
            self.assertIsNone(suffix["derivative_first_sequence"])
            self.assertIsNone(suffix["derivative_last_sequence"])
            self.assertEqual(receipt["derivative_wal"]["sha256"], digest(expected))

    def test_v2_enforces_content_addressed_byte_and_frame_bounds(self) -> None:
        for label, maximum_bytes, maximum_frames, error in (
            ("bytes", 1, 1024, "append exceeds the content-addressed safety bound"),
            ("frames", 1024, 1, "append exceeds the frame-count safety bound"),
        ):
            with self.subTest(label=label), tempfile.TemporaryDirectory() as temporary:
                root = pathlib.Path(temporary)
                source, snapshot, exact_plan, _expected = self.fixture(root)
                plan, plan_sha256 = self.parsed_plan(
                    self.append_aware_plan(
                        exact_plan,
                        maximum_bytes=maximum_bytes,
                        maximum_frames=maximum_frames,
                    )
                )
                source.write_bytes(
                    source.read_bytes() + frame(3, 4, 14) + frame(4, 5, 15)
                )
                with self.assertRaisesRegex(normalizer.NormalizationError, error):
                    normalizer.normalize(
                        plan,
                        plan_sha256,
                        source,
                        snapshot,
                        root / "normalized-source",
                    )

    def test_v2_rejects_changed_reviewed_prefix(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source, snapshot, exact_plan, _expected = self.fixture(root)
            plan, plan_sha256 = self.parsed_plan(
                self.append_aware_plan(exact_plan)
            )
            current = bytearray(source.read_bytes())
            current[0] ^= 1
            source.write_bytes(current + frame(3, 4, 14))
            with self.assertRaisesRegex(
                normalizer.NormalizationError, "reviewed base prefix differs"
            ):
                normalizer.normalize(
                    plan,
                    plan_sha256,
                    source,
                    snapshot,
                    root / "normalized-source",
                )

    def test_v2_rejects_truncated_invalid_and_noncontiguous_appends(self) -> None:
        cases = {
            "truncated": (
                frame(3, 4, 14)[:-1],
                "crosses its manifest run boundary",
            ),
            "invalid-crc": (
                frame(3, 4, 14)[:-1] + bytes([frame(3, 4, 14)[-1] ^ 1]),
                "checksum differs",
            ),
            "sequence-gap": (frame(3, 5, 14), "not sequence-contiguous"),
        }
        for label, (suffix, error) in cases.items():
            with self.subTest(label=label), tempfile.TemporaryDirectory() as temporary:
                root = pathlib.Path(temporary)
                source, snapshot, exact_plan, _expected = self.fixture(root)
                plan, plan_sha256 = self.parsed_plan(
                    self.append_aware_plan(exact_plan)
                )
                source.write_bytes(source.read_bytes() + suffix)
                with self.assertRaisesRegex(normalizer.NormalizationError, error):
                    normalizer.normalize(
                        plan,
                        plan_sha256,
                        source,
                        snapshot,
                        root / "normalized-source",
                    )

    def test_v2_rejects_concurrent_source_append(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source, snapshot, exact_plan, _expected = self.fixture(root)
            plan, plan_sha256 = self.parsed_plan(
                self.append_aware_plan(exact_plan)
            )
            base_size = source.stat().st_size
            source.write_bytes(source.read_bytes() + frame(3, 4, 14))
            real_read_frame = normalizer.read_frame
            mutation_done = False

            def read_frame_then_append(descriptor, offset, run_end):
                nonlocal mutation_done
                result = real_read_frame(descriptor, offset, run_end)
                if offset >= base_size and not mutation_done:
                    mutation_done = True
                    with source.open("ab") as handle:
                        handle.write(frame(4, 5, 15))
                        handle.flush()
                        os.fsync(handle.fileno())
                return result

            with unittest.mock.patch.object(
                normalizer, "read_frame", read_frame_then_append
            ), self.assertRaisesRegex(
                normalizer.NormalizationError, "identity changed during normalization"
            ):
                normalizer.normalize(
                    plan,
                    plan_sha256,
                    source,
                    snapshot,
                    root / "normalized-source",
                )

    def test_v2_rejects_concurrent_snapshot_change(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            source, snapshot, exact_plan, _expected = self.fixture(root)
            plan, plan_sha256 = self.parsed_plan(
                self.append_aware_plan(exact_plan)
            )
            source.write_bytes(source.read_bytes() + frame(3, 4, 14))
            real_write_all = normalizer.write_all
            mutation_done = False

            def write_then_change_snapshot(handle, raw):
                nonlocal mutation_done
                real_write_all(handle, raw)
                if not mutation_done:
                    mutation_done = True
                    snapshot.write_bytes(b"concurrently-changed-snapshot")

            with unittest.mock.patch.object(
                normalizer, "write_all", write_then_change_snapshot
            ), self.assertRaisesRegex(
                normalizer.NormalizationError,
                "snapshot identity changed during normalization",
            ):
                normalizer.normalize(
                    plan,
                    plan_sha256,
                    source,
                    snapshot,
                    root / "normalized-source",
                )

    def test_v2_plan_rejects_a_policy_that_does_not_continue_the_base(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            _source, _snapshot, exact_plan, _expected = self.fixture(root)
            plan = self.append_aware_plan(exact_plan)
            plan["append_policy"]["source_first_sequence"] += 1
            raw = normalizer.canonical_bytes(plan)
            with self.assertRaisesRegex(
                normalizer.NormalizationError,
                "append policy does not continue the reviewed base",
            ):
                normalizer.parse_plan(raw, digest(raw))

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
                self.assertEqual(plan["schema"], normalizer.SCHEMA_V2)
                self.assertEqual(
                    sum(
                        row["source_end"] - row["source_start"]
                        for row in plan["partition"]
                        if row["kind"] == "selected-frame-run"
                    ),
                    plan["base_derivative_wal"]["size"],
                )


if __name__ == "__main__":
    unittest.main()
