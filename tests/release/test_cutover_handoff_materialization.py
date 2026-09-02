#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import os
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
HELPER = REPO_ROOT / "scripts" / "release" / "materialize-cutover-handoff.py"
COMMIT = "9" * 40
ARTIFACT_NAME = f"arc-recovery-release-handoff-{COMMIT}"
FILES = {
    "arc-cutover-policy.json": b'{"schema_version":"fixture"}\n',
    "arc-legacy-maintenance-boundary.json": b'{"schema":"fixture"}\n',
    "arc-recovery-checkpoint-descriptor.json": b'{"schema_version":"fixture"}\n',
}


class CutoverHandoffMaterializationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.download = self.root / ARTIFACT_NAME
        self.download.mkdir()
        self.archive = self.download / "artifact.zip"

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write_archive(self, files=FILES) -> None:
        with zipfile.ZipFile(self.archive, "w", compression=zipfile.ZIP_STORED) as archive:
            for name, payload in files.items():
                info = zipfile.ZipInfo(name)
                info.external_attr = (0o100444 << 16)
                archive.writestr(info, payload)

    def run_helper(self, *, digest: str | None = None) -> subprocess.CompletedProcess[str]:
        actual = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        return subprocess.run(
            [
                sys.executable,
                str(HELPER),
                "--downloads-root",
                str(self.root),
                "--output-dir",
                str(self.root / "output"),
                "--artifact-id",
                "12345",
                "--artifact-name",
                ARTIFACT_NAME,
                "--artifact-digest",
                digest or f"sha256:{actual}",
                "--artifact-size",
                str(self.archive.stat().st_size),
                "--commit",
                COMMIT,
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

    def test_exact_immutable_archive_materializes_create_only_files(self) -> None:
        self.write_archive()
        result = self.run_helper()
        self.assertEqual(result.returncode, 0, result.stderr)
        output = self.root / "output"
        self.assertEqual({path.name for path in output.iterdir()}, set(FILES))
        for name, payload in FILES.items():
            path = output / name
            self.assertEqual(path.read_bytes(), payload)
            self.assertEqual(path.stat().st_mode & 0o777, 0o444)
            self.assertEqual(path.stat().st_nlink, 1)

    def test_server_digest_mismatch_fails_before_materialization(self) -> None:
        self.write_archive()
        result = self.run_helper(digest="sha256:" + "0" * 64)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("differs from the selected ID/digest/size tuple", result.stderr)
        self.assertFalse((self.root / "output").exists())

    def test_missing_or_traversing_members_fail_closed(self) -> None:
        self.write_archive(
            {
                key: value
                for key, value in FILES.items()
                if key != "arc-recovery-checkpoint-descriptor.json"
            }
        )
        missing = self.run_helper()
        self.assertNotEqual(missing.returncode, 0)
        self.assertIn("entry count differs", missing.stderr)

        self.archive.unlink()
        traversing_files = dict(FILES)
        traversing_files.pop("arc-recovery-checkpoint-descriptor.json")
        traversing_files["../escape"] = b"forbidden"
        self.write_archive(traversing_files)
        traversing = self.run_helper()
        self.assertNotEqual(traversing.returncode, 0)
        self.assertIn("unsafe or oversized entry", traversing.stderr)
        self.assertFalse((self.root / "escape").exists())


if __name__ == "__main__":
    unittest.main()
