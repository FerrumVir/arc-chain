"""Tests for lib/fswatch.py, the file-write evidence of the desktop lab (THROWAWAY LAB FILE)."""
from __future__ import annotations

import contextlib
import hashlib
import io
import json
import os
import sys
import tempfile
import time
import unittest
from pathlib import Path

import _paths  # noqa: F401
import fswatch


class SnapshotTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name) / "app"
        (self.root / "data").mkdir(parents=True)
        (self.root / "data" / "settings.json").write_text('{"a": 1}\n')
        (self.root / "logs").mkdir()
        (self.root / "logs" / "app.log").write_text("started\n")

    def test_entries_describe_files_and_directories(self):
        snap = fswatch.snapshot([self.root])
        settings = str(self.root / "data" / "settings.json")
        self.assertEqual(snap[settings]["type"], "file")
        self.assertEqual(snap[settings]["size"], 9)
        self.assertEqual(snap[settings]["sha256"], hashlib.sha256(b'{"a": 1}\n').hexdigest())
        self.assertEqual(snap[str(self.root)]["type"], "dir")
        self.assertEqual(snap[str(self.root / "data")]["type"], "dir")
        self.assertIsNone(snap[str(self.root / "data")]["sha256"])
        self.assertEqual(sorted(snap), sorted([str(self.root), str(self.root / "data"), settings, str(self.root / "logs"), str(self.root / "logs" / "app.log")]))

    def test_large_files_are_not_hashed_but_are_recorded(self):
        big = self.root / "big.bin"
        big.write_bytes(b"x" * (fswatch.HASH_LIMIT + 1))
        entry = fswatch.snapshot([self.root])[str(big)]
        self.assertEqual(entry["size"], fswatch.HASH_LIMIT + 1)
        self.assertIsNone(entry["sha256"])
        edge = self.root / "edge.bin"
        edge.write_bytes(b"y" * fswatch.HASH_LIMIT)
        self.assertIsNotNone(fswatch.snapshot([self.root])[str(edge)]["sha256"])

    def test_missing_roots_are_ignored_and_symlinks_are_not_followed(self):
        os.symlink(str(self.root / "data"), str(self.root / "link"))
        snap = fswatch.snapshot([self.root, Path(self.tmp.name) / "nowhere"])
        self.assertEqual(snap[str(self.root / "link")]["type"], "symlink")
        self.assertNotIn(str(self.root / "link" / "settings.json"), snap, "a symlinked directory is recorded, not walked")
        self.assertEqual(fswatch.snapshot([Path(self.tmp.name) / "nowhere"]), {})

    def test_exclude_prefixes_and_patterns(self):
        snap = fswatch.snapshot([self.root], exclude=[str(self.root / "logs"), "*.json"])
        self.assertNotIn(str(self.root / "logs"), snap)
        self.assertNotIn(str(self.root / "logs" / "app.log"), snap)
        self.assertNotIn(str(self.root / "data" / "settings.json"), snap)
        self.assertIn(str(self.root / "data"), snap)

    def test_diff_reports_added_removed_and_changed(self):
        before = fswatch.snapshot([self.root])
        (self.root / "data" / "new.json").write_text("{}\n")
        (self.root / "logs" / "app.log").unlink()
        (self.root / "data" / "settings.json").write_text('{"a": 2}\n')
        delta = fswatch.diff(before, fswatch.snapshot([self.root]))
        self.assertEqual(delta["added"], [str(self.root / "data" / "new.json")])
        self.assertEqual(delta["removed"], [str(self.root / "logs" / "app.log")])
        self.assertIn(str(self.root / "data" / "settings.json"), delta["changed"])

    def test_diff_sees_a_same_size_edit_and_a_touch(self):
        before = fswatch.snapshot([self.root])
        settings = self.root / "data" / "settings.json"
        settings.write_text('{"a": 9}\n')  # same length, different bytes
        os.utime(str(self.root / "logs" / "app.log"), ns=(1_000_000_000, 1_000_000_000))  # content untouched, mtime changed
        delta = fswatch.diff(before, fswatch.snapshot([self.root]))
        self.assertIn(str(settings), delta["changed"])
        self.assertIn(str(self.root / "logs" / "app.log"), delta["changed"])

    def test_diff_of_identical_snapshots_is_empty(self):
        snap = fswatch.snapshot([self.root])
        self.assertEqual(fswatch.diff(snap, dict(snap)), {"added": [], "removed": [], "changed": []})

    def test_a_directory_is_not_changed_just_because_an_entry_below_it_was_added_or_removed(self):
        before = fswatch.snapshot([self.root])
        (self.root / "data" / "extra.json").write_text("{}\n")
        (self.root / "logs" / "app.log").unlink()
        delta = fswatch.diff(before, fswatch.snapshot([self.root]))
        self.assertNotIn(str(self.root / "data"), delta["changed"])
        self.assertNotIn(str(self.root / "logs"), delta["changed"])
        self.assertNotIn(str(self.root), delta["changed"])
        self.assertEqual(delta["added"], [str(self.root / "data" / "extra.json")])
        self.assertEqual(delta["removed"], [str(self.root / "logs" / "app.log")])

    def test_a_file_replaced_by_a_directory_is_a_change(self):
        before = fswatch.snapshot([self.root])
        target = self.root / "logs" / "app.log"
        target.unlink()
        target.mkdir()
        self.assertIn(str(target), fswatch.diff(before, fswatch.snapshot([self.root]))["changed"])


class ClassifyTests(unittest.TestCase):
    def test_posix_prefixes(self):
        expected = ["/home/runner/.local/share/network.arc.desktop", "/home/runner/.config/ARC Node"]
        self.assertEqual(fswatch.classify("/home/runner/.local/share/network.arc.desktop", expected), "expected")
        self.assertEqual(fswatch.classify("/home/runner/.local/share/network.arc.desktop/logs/app.log", expected), "expected")
        self.assertEqual(fswatch.classify("/home/runner/.config/ARC Node/settings.json", expected), "expected")
        self.assertEqual(fswatch.classify("/home/runner/.local/share/network.arc.desktop-evil/x", expected), "unexpected", "a prefix match must stop at a path component")
        self.assertEqual(fswatch.classify("/home/runner/.local/share/other/x", expected), "unexpected")
        self.assertEqual(fswatch.classify("/home/runner/.local/share/network.arc.desktop/../other/x", expected), "unexpected", "dot-dot is resolved before comparing")

    def test_windows_prefixes_on_any_host(self):
        expected = [r"C:\Users\runneradmin\AppData\Roaming\network.arc.desktop"]
        self.assertEqual(fswatch.classify(r"C:\Users\runneradmin\AppData\Roaming\network.arc.desktop\logs\a.log", expected), "expected")
        self.assertEqual(fswatch.classify(r"c:\users\RUNNERADMIN\appdata\roaming\NETWORK.ARC.DESKTOP\x", expected), "expected", "Windows paths compare case-insensitively")
        self.assertEqual(fswatch.classify("C:/Users/runneradmin/AppData/Roaming/network.arc.desktop/x", expected), "expected", "forward slashes are accepted")
        self.assertEqual(fswatch.classify(r"C:\Users\runneradmin\AppData\Roaming\network.arc.desktop-evil\x", expected), "unexpected")
        self.assertEqual(fswatch.classify(r"C:\Users\runneradmin\AppData\Local\Temp\bundle.exe", expected), "unexpected")

    def test_the_prefix_itself_is_expected_and_no_prefixes_means_unexpected(self):
        self.assertEqual(fswatch.classify("/a/b", ["/a/b"]), "expected")
        self.assertEqual(fswatch.classify("/a/b", []), "unexpected")
        self.assertEqual(fswatch.classify("/a/b/", ["/a/b"]), "expected")

    def test_unexpected_lists_only_the_unexpected_sorted(self):
        paths = ["/x/z", "/ok/a", "/x/a", "/ok"]
        self.assertEqual(fswatch.unexpected(paths, ["/ok"]), ["/x/a", "/x/z"])


class PollerTests(unittest.TestCase):
    def wait_for(self, poller, predicate, timeout=5.0):
        end = time.time() + timeout
        while time.time() < end:
            hits = [e for e in poller.events() if predicate(e)]
            if hits:
                return hits
            time.sleep(0.01)
        self.fail("no matching event in %s" % poller.events())

    def test_records_added_changed_and_removed_paths_with_first_seen_times(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "app"
            root.mkdir()
            (root / "old.txt").write_text("already there")
            out = Path(tmp) / "out" / "writes.jsonl"
            poller = fswatch.Poller([root], interval=0.02, out_path=str(out))
            poller.start()
            try:
                self.assertEqual(poller.events(), [], "what was already there is not a write of the case")
                new = root / "download.partial"
                new.write_text("abc")
                added = self.wait_for(poller, lambda e: e["event"] == "added" and e["path"] == str(new))
                self.assertGreater(added[0]["t"], 1.7e9)
                new.write_text("abcdef")
                self.wait_for(poller, lambda e: e["event"] == "changed" and e["path"] == str(new))
                new.unlink()
                self.wait_for(poller, lambda e: e["event"] == "removed" and e["path"] == str(new))
            finally:
                poller.stop()
            lines = [json.loads(line) for line in out.read_text().splitlines()]
            self.assertEqual(lines, poller.events())
            self.assertEqual([item["event"] for item in lines if item["path"].endswith("download.partial")], ["added", "changed", "removed"])
            self.assertGreaterEqual(poller.scans, 3)

    def test_a_file_created_just_before_stop_is_not_lost(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            poller = fswatch.Poller([root], interval=60)
            poller.start()
            (root / "late.bin").write_text("x")
            poller.stop()
            self.assertEqual([e["path"] for e in poller.events()], [str(root / "late.bin")])

    def test_new_directories_and_their_files_are_recorded(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            poller = fswatch.Poller([root], interval=60)
            poller.start()
            (root / "cache").mkdir()
            (root / "cache" / "x.dat").write_text("1")
            poller.stop()
            self.assertEqual(sorted(e["path"] for e in poller.events()), sorted([str(root / "cache"), str(root / "cache" / "x.dat")]))


class CliTests(unittest.TestCase):
    def test_snapshot_and_diff_commands(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "app"
            root.mkdir()
            (root / "a.txt").write_text("1")
            before, after = Path(tmp) / "before.json", Path(tmp) / "after.json"
            self.assertEqual(fswatch.main(["snapshot", "--root", str(root), "--out", str(before)]), 0)
            (root / "b.txt").write_text("2")
            self.assertEqual(fswatch.main(["snapshot", "--root", str(root), "--out", str(after)]), 0)
            with contextlib.redirect_stdout(io.StringIO()) as buffer:
                code = fswatch.main(["diff", "--before", str(before), "--after", str(after), "--expected", str(root)])
            self.assertEqual(code, 0, "the new file is under an expected prefix")
            self.assertEqual(json.loads(buffer.getvalue())["added"], [str(root / "b.txt")])
            with contextlib.redirect_stdout(io.StringIO()) as buffer:
                code = fswatch.main(["diff", "--before", str(before), "--after", str(after), "--expected", str(Path(tmp) / "elsewhere")])
            self.assertEqual(code, 1, "an added file outside the expected prefixes fails")
            self.assertEqual(json.loads(buffer.getvalue())["unexpected_added"], [str(root / "b.txt")])


if __name__ == "__main__":
    unittest.main()
