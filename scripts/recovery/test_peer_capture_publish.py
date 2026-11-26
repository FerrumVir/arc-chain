import fcntl
import hashlib
import os
import unittest
from unittest.mock import patch

from scripts.recovery import peer_capture as pc
from scripts.recovery import test_peer_capture as fixture


class PeerCapturePublishTests(unittest.TestCase):
    setUp = fixture.PeerCaptureTests.setUp
    tearDown = fixture.PeerCaptureTests.tearDown
    _populate_source_layout = fixture.PeerCaptureTests._populate_source_layout
    _write_precopy_candidate = fixture.PeerCaptureTests._write_precopy_candidate
    _capture_valid_bundle = fixture.PeerCaptureTests._capture_valid_bundle

    def inventory(self):
        return {
            path.relative_to(self.cand).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in self.cand.rglob("*") if path.is_file()
        }

    def publish(self, bundle, **kwargs):
        return pc.publish_bundle_into_candidate(
            candidate_dir=self.cand, bundle_dir=bundle,
            expected_recovery_manifest_hash=fixture.MANIFEST_HASH,
            min_reserve_bytes=1024, free_space_fn=lambda _: 10 * 1024**3,
            **kwargs,
        )

    def test_stale_destinations_are_rejected_without_modification(self):
        bundle, _, _ = self._capture_valid_bundle()
        for name in ["wal-00000001.bin", "recovery.active", pc.PUBLISH_MANIFEST_NAME + ".partial", "state.wal"]:
            with self.subTest(name=name):
                rogue = self.cand / name
                rogue.write_bytes(b"preserve")
                before = self.inventory()
                with self.assertRaises(pc.CaptureError):
                    self.publish(bundle)
                self.assertEqual(self.inventory(), before)
                rogue.unlink()
        rogue_dir = self.cand / f"dag-wal-recovery-{fixture.MANIFEST_HASH}"
        rogue_dir.mkdir()
        before = self.inventory()
        with self.assertRaises(pc.CaptureError):
            self.publish(bundle)
        self.assertTrue(rogue_dir.is_dir())
        self.assertEqual(self.inventory(), before)

    def test_changed_tail_rolls_back_copied_files_and_preserves_prefix(self):
        bundle, _, _ = self._capture_valid_bundle()
        before = self.inventory()
        for replacement in [self.tail[:-1], self.tail + b"extra"]:
            with self.subTest(length=len(replacement)):
                (bundle / pc.BUNDLE_TAIL_NAME).write_bytes(self.tail)
                with self.assertRaises(pc.CaptureError):
                    self.publish(bundle, on_before_tail_append=lambda: (bundle / pc.BUNDLE_TAIL_NAME).write_bytes(replacement))
                self.assertEqual(self.inventory(), before)
                self.assertEqual((self.cand / pc.PRECOPY_WAL_NAME).stat().st_ino, self.precopy_ino)

    def test_append_write_failure_restores_prefix_without_recopy(self):
        bundle, _, _ = self._capture_valid_bundle()
        before = self.inventory()
        original_write = pc.write_all

        def fail_wal_write(fd, data):
            if os.fstat(fd).st_ino == self.precopy_ino:
                os.write(fd, data[:3])
                raise OSError("injected append failure")
            return original_write(fd, data)

        with patch.object(pc, "write_all", side_effect=fail_wal_write):
            with self.assertRaisesRegex(OSError, "injected append failure"):
                self.publish(bundle)
        self.assertEqual(self.inventory(), before)
        self.assertEqual((self.cand / pc.PRECOPY_WAL_NAME).read_bytes(), self.prefix)
        self.publish(bundle)
        self.assertEqual((self.cand / "state.wal").read_bytes(), self.prefix + self.tail)

    def test_manifest_staging_failure_restores_candidate(self):
        bundle, _, _ = self._capture_valid_bundle()
        before = self.inventory()
        with patch.object(pc, "_stage_exclusive_json_partial", side_effect=OSError("injected manifest ENOSPC")):
            with self.assertRaisesRegex(OSError, "injected manifest ENOSPC"):
                self.publish(bundle)
        self.assertEqual(self.inventory(), before)

    def test_competing_prefix_lock_rejects_without_changes(self):
        bundle, _, _ = self._capture_valid_bundle()
        before = self.inventory()
        with (self.cand / pc.PRECOPY_WAL_NAME).open("rb") as held:
            fcntl.flock(held.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaisesRegex(pc.CaptureError, "advisory lock"):
                self.publish(bundle)
        self.assertEqual(self.inventory(), before)

    def test_post_link_failure_preserves_complete_candidate_bytes(self):
        bundle, _, _ = self._capture_valid_bundle()
        original_link = os.link

        def fail_manifest_link(source, destination, **kwargs):
            if destination == self.cand / pc.PUBLISH_MANIFEST_NAME:
                raise OSError("injected publish link failure")
            return original_link(source, destination, **kwargs)

        with patch.object(pc.os, "link", side_effect=fail_manifest_link):
            with self.assertRaisesRegex(pc.CaptureError, "interrupted publication"):
                self.publish(bundle)
        self.assertEqual((self.cand / "state.wal").read_bytes(), self.prefix + self.tail)
        self.assertEqual((self.cand / pc.PRECOPY_WAL_NAME).read_bytes(), self.prefix + self.tail)
        self.assertEqual((self.cand / "state.wal").stat().st_nlink, 2)
        self.assertTrue((self.cand / (pc.PUBLISH_MANIFEST_NAME + ".partial")).exists())
        with self.assertRaises(pc.CaptureError):
            self.publish(bundle)

    def test_prefix_lock_remains_held_until_publication_is_durable(self):
        bundle, _, _ = self._capture_valid_bundle()
        original_sync = pc.fsync_dir
        checks = []

        def check_lock(path):
            if path == self.cand and (self.cand / pc.PUBLISH_MANIFEST_NAME).exists():
                with (self.cand / "state.wal").open("rb") as competitor:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(competitor.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                checks.append(True)
            original_sync(path)

        with patch.object(pc, "fsync_dir", side_effect=check_lock):
            self.publish(bundle)
        self.assertGreaterEqual(len(checks), 2)
        with (self.cand / "state.wal").open("rb") as released:
            fcntl.flock(released.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_failed_json_write_removes_only_its_partial_file(self):
        destination = self.root / "receipt.json"
        with patch.object(pc, "write_all", side_effect=OSError("injected JSON failure")):
            with self.assertRaisesRegex(OSError, "injected JSON failure"):
                pc.write_exclusive_json(destination, {"status": "unqualified"})
        self.assertFalse(destination.exists())
        self.assertFalse(destination.with_name(destination.name + ".partial").exists())

    def test_shrunken_prefix_is_not_zero_extended_during_rollback(self):
        bundle, _, _ = self._capture_valid_bundle()
        real_fsync = pc.os.fsync
        shrunk = False

        def shrink_after_tail_append(fd):
            nonlocal shrunk
            real_fsync(fd)
            st = os.fstat(fd)
            if st.st_ino == self.precopy_ino and st.st_size > len(self.prefix) and not shrunk:
                shrunk = True
                os.ftruncate(fd, len(self.prefix) - 8)

        with patch.object(pc.os, "fsync", side_effect=shrink_after_tail_append):
            with self.assertRaisesRegex(pc.CaptureError, "recopy the verified source prefix"):
                self.publish(bundle)
        self.assertTrue(shrunk)
        self.assertEqual((self.cand / pc.PRECOPY_WAL_NAME).read_bytes(), self.prefix[:-8])
        self.assertFalse((self.cand / "state.wal").exists())
        self.assertFalse((self.cand / "recovery.active").exists())

    def test_mutated_prefix_is_not_claimed_restored_after_tail_rollback(self):
        bundle, _, _ = self._capture_valid_bundle()
        real_fsync = pc.os.fsync
        mutated = False

        def mutate_after_tail_append(fd):
            nonlocal mutated
            real_fsync(fd)
            st = os.fstat(fd)
            if st.st_ino == self.precopy_ino and st.st_size > len(self.prefix) and not mutated:
                mutated = True
                os.pwrite(fd, b"changed!", 0)

        with patch.object(pc.os, "fsync", side_effect=mutate_after_tail_append):
            with self.assertRaisesRegex(pc.CaptureError, "prefix bytes mutated.*cannot be restored"):
                self.publish(bundle)
        self.assertTrue(mutated)
        self.assertEqual((self.cand / pc.PRECOPY_WAL_NAME).read_bytes(), b"changed!" + self.prefix[8:])
        self.assertFalse((self.cand / "state.wal").exists())
        with self.assertRaisesRegex(pc.CaptureError, "prefix sha256"):
            self.publish(bundle)
