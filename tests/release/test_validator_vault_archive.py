#!/usr/bin/env python3
"""Adversarial tests for validator-vault validation and canonicalization."""

from __future__ import annotations

import io
import struct
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
VALIDATOR = REPO_ROOT / "scripts" / "release" / "validate-validator-vault.py"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "validator-vault-rewrap.yml"
WORKFLOW_CANONICALIZER = '/usr/bin/python3 -I - "$plain_tar" "$canonical_tar" <<\'PY\'\n'


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
        format: int | None = None,
        pax_headers: dict[str, str] | None = None,
    ) -> Path:
        self.counter += 1
        path = self.root / f"vault-{self.counter}.tar"
        options: dict[str, object] = {}
        if format is not None:
            options["format"] = format
        if pax_headers is not None:
            options["pax_headers"] = pax_headers
        with tarfile.open(path, mode=mode, **options) as archive:
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

    @staticmethod
    def workflow_canonicalizer() -> str:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        start = workflow.index(WORKFLOW_CANONICALIZER) + len(WORKFLOW_CANONICALIZER)
        end = workflow.index("\n          PY\n", start)
        return textwrap.dedent(workflow[start:end]) + "\n"

    def canonicalize(
        self,
        source: Path,
        *,
        output: Path | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], Path]:
        source.chmod(0o600)
        destination = output or self.root / f"canonical-{self.counter}.tar"
        result = subprocess.run(
            [sys.executable, "-I", "-", str(source), str(destination)],
            input=self.workflow_canonicalizer(),
            text=True,
            capture_output=True,
            check=False,
        )
        return result, destination

    @staticmethod
    def appledouble(payload: bytes = b"synthetic macOS metadata") -> bytes:
        count = 2
        table_end = 26 + count * 12
        return b"".join(
            (
                struct.pack(">II", 0x00051607, 0x00020000),
                b"Mac OS X        ",
                struct.pack(">H", count),
                struct.pack(">III", 9, table_end, len(payload)),
                struct.pack(">III", 2, table_end + len(payload), 0),
                payload,
            )
        )

    @staticmethod
    def macos_metadata(member: tarfile.TarInfo) -> None:
        member.uid = 501
        member.gid = 20
        member.uname = "mac-operator"
        member.gname = "staff"
        member.mtime = 1_725_000_000.125
        member.pax_headers = {
            "mtime": "1725000000.125",
            "LIBARCHIVE.xattr.com.apple.provenance": "AgAAAAAAAABmacfixture",
            "SCHILY.xattr.com.apple.provenance": "AgAAAAAAAABmacfixture",
        }

    def macos_members(self) -> tuple[list[tuple[tarfile.TarInfo, bytes]], dict[str, bytes]]:
        directory = tarfile.TarInfo("private")
        directory.type = tarfile.DIRTYPE
        directory.mode = 0o700
        self.macos_metadata(directory)
        directory_sidecar, directory_sidecar_payload = self.regular(
            "._private", self.appledouble()
        )
        directory_sidecar.mode = 0o700
        members: list[tuple[tarfile.TarInfo, bytes]] = [
            (directory_sidecar, directory_sidecar_payload),
            (directory, b""),
        ]
        expected: dict[str, bytes] = {}
        for index in range(1, 7):
            name = f"private/validator-{index}.key"
            payload = f"private validator fixture {index}\n".encode()
            sidecar, sidecar_payload = self.regular(
                f"private/._validator-{index}.key", self.appledouble()
            )
            key, _ = self.regular(name, payload)
            self.macos_metadata(sidecar)
            self.macos_metadata(key)
            members.extend(((sidecar, sidecar_payload), (key, payload)))
            expected[name] = payload
        return members, expected

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

    def test_workflow_canonicalizes_macos_bsdtar_metadata_in_memory(self) -> None:
        members, expected = self.macos_members()
        source = self.archive(members, format=tarfile.PAX_FORMAT)
        # libarchive stores some extended attributes as binary PAX values;
        # Python exposes invalid UTF-8 octets through surrogateescape.
        marker = b"AgAAAAAAAABmacfixture"
        raw = source.read_bytes()
        self.assertIn(marker, raw)
        source.write_bytes(raw.replace(marker, b"\xff" + marker[1:]))
        result, output = self.canonicalize(source)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "")
        self.assertEqual(output.stat().st_mode & 0o7777, 0o600)
        with tarfile.open(fileobj=io.BytesIO(output.read_bytes()), mode="r:") as archive:
            self.assertEqual(archive.pax_headers, {})
            canonical = archive.getmembers()
            self.assertEqual([member.name for member in canonical], sorted(expected))
            self.assertEqual(len(canonical), 6)
            for member in canonical:
                self.assertEqual(member.type, tarfile.REGTYPE)
                self.assertEqual(member.mode & 0o7777, 0o600)
                self.assertEqual((member.uid, member.gid), (0, 0))
                self.assertEqual((member.uname, member.gname), ("", ""))
                self.assertEqual(member.mtime, 0)
                self.assertEqual(member.linkname, "")
                self.assertEqual(member.pax_headers, {})
                source = archive.extractfile(member)
                self.assertIsNotNone(source)
                self.assertEqual(source.read(), expected[member.name])

        plain = self.archive(
            [self.regular(name, payload) for name, payload in reversed(expected.items())]
        )
        plain_result, plain_output = self.canonicalize(plain)
        self.assertEqual(plain_result.returncode, 0, plain_result.stderr)
        self.assertEqual(plain_output.read_bytes(), output.read_bytes())

    def test_workflow_rejects_semantic_or_unknown_pax_metadata(self) -> None:
        for key, value in (
            ("path", "private/replaced.key"),
            ("linkpath", "private/replaced.key"),
            ("size", str(len(b"private fixture bytes\n"))),
            ("GNU.sparse.map", "0,1"),
            ("comment", "unsupported"),
        ):
            with self.subTest(key=key):
                members = self.valid_members()
                members[0][0].pax_headers = {key: value}
                result, output = self.canonicalize(
                    self.archive(members, format=tarfile.PAX_FORMAT)
                )
                self.assertNotEqual(result.returncode, 0, result)
                self.assertFalse(output.exists())

        result, output = self.canonicalize(
            self.archive(
                self.valid_members(),
                format=tarfile.PAX_FORMAT,
                pax_headers={"comment": "unsupported global metadata"},
            )
        )
        self.assertNotEqual(result.returncode, 0, result)
        self.assertFalse(output.exists())

    def test_workflow_rejects_unpaired_or_malformed_appledouble(self) -> None:
        for sidecar in (
            self.regular("._orphan.key", self.appledouble()),
            self.regular("._validator-1.key", b"not an AppleDouble structure"),
        ):
            with self.subTest(name=sidecar[0].name):
                result, output = self.canonicalize(
                    self.archive([sidecar, *self.valid_members()])
                )
                self.assertNotEqual(result.returncode, 0, result)
                self.assertFalse(output.exists())

    def test_workflow_preserves_all_archive_safety_rejections(self) -> None:
        cases: list[list[tuple[tarfile.TarInfo, bytes]]] = []

        traversal = self.valid_members()
        traversal[0] = self.regular("../DO_NOT_PRINT_PRIVATE_NODE_NAME")
        cases.append(traversal)

        symlink = tarfile.TarInfo("private-link")
        symlink.type = tarfile.SYMTYPE
        symlink.mode = 0o600
        symlink.linkname = "DO_NOT_PRINT_PRIVATE_LINK_TARGET"
        cases.append([*self.valid_members(), (symlink, b"")])

        duplicate = self.valid_members()
        duplicate.append(self.regular("validator-1.key"))
        cases.append(duplicate)

        case_duplicate = self.valid_members()
        case_duplicate.append(self.regular("VALIDATOR-1.KEY"))
        cases.append(case_duplicate)

        oversized = self.valid_members()
        oversized[0] = self.regular(
            "DO_NOT_PRINT_OVERSIZED_PRIVATE_KEY", b"S" * (16 * 1024 + 1)
        )
        cases.append(oversized)

        hierarchy = self.valid_members()
        hierarchy[0] = self.regular("private")
        hierarchy[1] = self.regular("private/validator-2.key")
        cases.append(hierarchy)

        for index, members in enumerate(cases):
            with self.subTest(case=index):
                result, output = self.canonicalize(self.archive(members))
                self.assertNotEqual(result.returncode, 0, result)
                self.assertFalse(output.exists())
                combined = result.stdout + result.stderr
                for secret in (
                    "DO_NOT_PRINT_PRIVATE_NODE_NAME",
                    "DO_NOT_PRINT_PRIVATE_LINK_TARGET",
                    "DO_NOT_PRINT_OVERSIZED_PRIVATE_KEY",
                ):
                    self.assertNotIn(secret, combined)

    def test_workflow_output_is_create_only_and_source_must_be_nofollow(self) -> None:
        source = self.archive(self.valid_members())
        output = self.root / "already-present.tar"
        output.write_bytes(b"sentinel")
        result, _ = self.canonicalize(source, output=output)
        self.assertNotEqual(result.returncode, 0, result)
        self.assertEqual(output.read_bytes(), b"sentinel")

        link = self.root / "source-link.tar"
        link.symlink_to(source)
        result, linked_output = self.canonicalize(link)
        self.assertNotEqual(result.returncode, 0, result)
        self.assertFalse(linked_output.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
