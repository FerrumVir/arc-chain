import hashlib
import os
import unittest
from unittest.mock import patch

from scripts.recovery import peer_capture as pc
from scripts.recovery import test_peer_capture as fixture


class CaptureBoundaryTests(unittest.TestCase):
    setUp = fixture.PeerCaptureTests.setUp
    tearDown = fixture.PeerCaptureTests.tearDown
    _populate_source_layout = fixture.PeerCaptureTests._populate_source_layout
    _write_precopy_candidate = fixture.PeerCaptureTests._write_precopy_candidate
    _capture_valid_bundle = fixture.PeerCaptureTests._capture_valid_bundle

    def test_hash_reads_are_bounded_when_file_grows(self):
        target = self.root / "hash-growth"
        target.write_bytes(b"original")
        original_read = os.read
        reads = []

        def grow_after_read(fd, size):
            result = original_read(fd, size)
            reads.append(len(result))
            if len(reads) == 1:
                with target.open("ab") as output:
                    output.write(b"growth" * 100)
            return result

        with patch.object(pc.os, "read", side_effect=grow_after_read):
            with self.assertRaisesRegex(pc.CaptureError, "grew during hashing"):
                pc.sha256_file_checked(target, 8, hashlib.sha256(b"original").hexdigest(), "fixture")
        self.assertEqual(sum(reads), 9)

    def test_recovery_marker_rejects_oversized_input_before_read(self):
        marker = self.root / "oversized-marker"
        marker.write_bytes(b"x" * 4096)
        with patch.object(pc.os, "read", side_effect=AssertionError("oversized marker must not be read")):
            with self.assertRaisesRegex(pc.CaptureError, "exact expected recovery manifest"):
                pc.validate_recovery_active(marker, fixture.MANIFEST_HASH)

    def test_exact_source_identity_and_size_prevent_truncated_capture(self):
        original = self.src_wal.stat()
        with self.src_wal.open("ab") as stream:
            stream.write(b"growth")
        with self.assertRaisesRegex(pc.CaptureError, "identity or exact size"):
            pc.copy_range_checked(
                self.src_wal, self.root / "growth-copy", offset=0,
                length=original.st_size, expected_st=original,
                deadline=100, clock=lambda: 0, budget_remaining=original.st_size,
            )
        self.assertFalse((self.root / "growth-copy").exists())
        replacement = self.root / "replacement"
        replacement.write_bytes(b"x" * original.st_size)
        replacement.replace(self.src_wal)
        with self.assertRaisesRegex(pc.CaptureError, "identity or exact size"):
            pc.copy_range_checked(
                self.src_wal, self.root / "replacement-copy", offset=0,
                length=original.st_size, expected_st=original,
                deadline=100, clock=lambda: 0, budget_remaining=original.st_size,
            )

    def test_destination_failure_closes_source_descriptor(self):
        destination = self.root / "existing"
        destination.write_bytes(b"keep")
        opened = []
        original_open = pc.open_readonly_nofollow

        def track_open(path, label):
            result = original_open(path, label)
            opened.append(result[0])
            return result

        original = self.src_wal.stat()
        with patch.object(pc, "open_readonly_nofollow", side_effect=track_open):
            with self.assertRaises(FileExistsError):
                pc.copy_range_checked(
                    self.src_wal, destination, offset=0, length=original.st_size,
                    expected_st=original, deadline=100, clock=lambda: 0,
                    budget_remaining=original.st_size,
                )
        self.assertEqual(destination.read_bytes(), b"keep")
        self.assertEqual(len(opened), 1)
        with self.assertRaises(OSError):
            os.fstat(opened[0])

    def test_fifty_generations_publish_and_extra_empty_directory_is_rejected(self):
        dag = self.src / f"dag-wal-recovery-{fixture.MANIFEST_HASH}"
        for index in range(49):
            generation = dag / f"gen-{index:064x}"
            generation.mkdir()
            (generation / "manifest.json").write_bytes(b"{}")
            (generation / "records.bin").write_bytes(b"record")
        bundle, _, manifest = self._capture_valid_bundle()
        expected = {entry["relpath"] for entry in manifest["files"]}
        pc.audit_bundle_tree_matches_manifest(bundle, expected)
        rogue = bundle / "rogue"
        rogue.mkdir()
        with self.assertRaisesRegex(pc.CaptureError, "unexpected directory|maximum entry"):
            pc.audit_bundle_tree_matches_manifest(bundle, expected)
        rogue.rmdir()
        result = pc.publish_bundle_into_candidate(
            candidate_dir=self.cand, bundle_dir=bundle,
            expected_recovery_manifest_hash=fixture.MANIFEST_HASH,
            min_reserve_bytes=1024, free_space_fn=lambda _: 10_000_000,
        )
        self.assertFalse(result["qualified_for_recovery"])
        self.assertEqual((self.cand / "state.wal").read_bytes(), self.prefix + self.tail)

    def test_empty_generation_fails_and_thaws(self):
        dag = self.src / f"dag-wal-recovery-{fixture.MANIFEST_HASH}"
        (dag / f"gen-{'b' * 64}").mkdir()
        before = self.src_wal.stat()
        backend = fixture.FakeFreezeBackend()
        with self.assertRaisesRegex(pc.CaptureError, "must contain manifest.json"):
            pc.capture_source_tail_bundle(
                source_data_dir=self.src, bundle_dir=self.root / "peer-capture-bundle-empty",
                recovery_manifest_hash=fixture.MANIFEST_HASH,
                precopy_prefix_bytes=len(self.prefix), precopy_source_dev=before.st_dev,
                precopy_source_ino=before.st_ino, freeze_backend=backend,
                free_space_fn=lambda _: 10 * 1024**3,
            )
        self.assertIn("thaw", backend.events)

    def test_reserve_rejects_before_freeze_and_non_ascii_hash_fails_cleanly(self):
        before = self.src_wal.stat()
        backend = fixture.FakeFreezeBackend()
        with self.assertRaisesRegex(pc.CaptureError, "insufficient free space"):
            pc.capture_source_tail_bundle(
                source_data_dir=self.src, bundle_dir=self.root / "peer-capture-bundle-space",
                recovery_manifest_hash=fixture.MANIFEST_HASH,
                precopy_prefix_bytes=len(self.prefix), precopy_source_dev=before.st_dev,
                precopy_source_ino=before.st_ino, freeze_backend=backend,
                free_space_fn=lambda _: pc.MAX_BUNDLE_BYTES,
            )
        self.assertEqual(backend.events, [])
        path = self.src / "genesis.network-hash"
        path.write_bytes(b"\xff\xfe\n")
        with self.assertRaisesRegex(pc.CaptureError, "ASCII"):
            pc.validate_genesis_network_hash(path)

    def test_hash_and_path_allowlists_reject_trailing_newlines(self):
        with self.assertRaises(pc.CaptureError):
            pc.require_hex64(fixture.MANIFEST_HASH + "\n", "manifest")
        dag = f"dag-wal-recovery-{fixture.MANIFEST_HASH}"
        for rel in [f"{dag}/active-{fixture.GEN_HASH}.bin\n", f"{dag}/gen-{fixture.GEN_HASH}\n/manifest.json"]:
            self.assertFalse(pc.is_allowed_bundle_relpath(rel, fixture.MANIFEST_HASH))


if __name__ == "__main__":
    unittest.main()
