#!/usr/bin/env python3
"""Adversarial tests for metadata-only validator-vault tar validation."""

from __future__ import annotations

import io
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
VALIDATOR = REPO_ROOT / "scripts" / "release" / "validate-validator-vault.py"


class ValidatorVaultArchiveTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.counter = 0

    def archive(
        self,
        members: list[tuple[tarfile.TarInfo, bytes]],
        *,
        mode: str = "w:",
    ) -> Path:
        self.counter += 1
        path = self.root / f"vault-{self.counter}.tar"
        with tarfile.open(path, mode=mode) as archive:
            for member, payload in members:
                archive.addfile(member, io.BytesIO(payload) if member.isfile() else None)
        return path

    @staticmethod
    def regular(name: str, payload: bytes = b"private fixture bytes\n") -> tuple[tarfile.TarInfo, bytes]:
        member = tarfile.TarInfo(name)
        member.mode = 0o600
        member.size = len(payload)
        return member, payload

    def valid_members(self) -> list[tuple[tarfile.TarInfo, bytes]]:
        return [self.regular(f"validator-{index}.key") for index in range(1, 7)]

    def validate(self, path: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["python3", str(VALIDATOR), str(path)],
            text=True,
            capture_output=True,
            check=False,
        )

    def assert_rejected(self, path: Path, expected: str) -> None:
        result = self.validate(path)
        self.assertNotEqual(result.returncode, 0, result)
        self.assertIn(expected, result.stderr)

    def test_accepts_six_private_regular_files_without_output(self) -> None:
        result = self.validate(self.archive(self.valid_members()))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "")

    def test_rejects_traversal_absolute_windows_and_control_paths(self) -> None:
        for name in (
            "../private.key",
            "/private.key",
            "folder\\private.key",
            "C:private.key",
            "private\nkey",
        ):
            members = self.valid_members()
            members[0] = self.regular(name)
            self.assert_rejected(self.archive(members), "unsafe path" if "\n" not in name else "control character")

    def test_rejects_links_duplicates_permissions_and_oversized_members(self) -> None:
        symlink = tarfile.TarInfo("validator-link")
        symlink.type = tarfile.SYMTYPE
        symlink.linkname = "validator-1.key"
        self.assert_rejected(
            self.archive([*self.valid_members(), (symlink, b"")]),
            "not a regular file or directory",
        )

        duplicate = self.valid_members()
        duplicate.append(self.regular("validator-1.key"))
        self.assert_rejected(self.archive(duplicate), "duplicates another archive path")

        case_duplicate = self.valid_members()
        case_duplicate.append(self.regular("VALIDATOR-1.KEY"))
        self.assert_rejected(
            self.archive(case_duplicate), "duplicates another archive path"
        )

        permissive = self.valid_members()
        permissive[0][0].mode = 0o644
        self.assert_rejected(
            self.archive(permissive), "private file permissions"
        )

        oversized = self.valid_members()
        oversized[0] = self.regular("validator-1.key", b"x" * (128 * 1024 + 1))
        self.assert_rejected(self.archive(oversized), "size is outside")

    def test_rejects_compressed_invalid_and_too_small_archives(self) -> None:
        compressed = self.archive(self.valid_members(), mode="w:gz")
        self.assert_rejected(compressed, "bounded tar contract")

        invalid = self.root / "invalid.tar"
        invalid.write_bytes(b"not a tar" + b"\0" * (512 - len(b"not a tar")))
        self.assert_rejected(invalid, "valid plain tar")

        self.assert_rejected(
            self.archive(self.valid_members()[:5]),
            "fewer than six private vault files",
        )

    def test_errors_never_echo_member_names_or_payloads(self) -> None:
        private_name = "DO_NOT_PRINT_PRIVATE_NODE_NAME"
        private_payload = b"DO_NOT_PRINT_PRIVATE_KEY_CONTENT"
        result = self.validate(self.archive([self.regular(private_name, private_payload)]))
        self.assertNotEqual(result.returncode, 0)
        combined = result.stdout + result.stderr
        self.assertNotIn(private_name, combined)
        self.assertNotIn(private_payload.decode(), combined)


if __name__ == "__main__":
    unittest.main(verbosity=2)
