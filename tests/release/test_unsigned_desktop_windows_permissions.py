#!/usr/bin/env python3
"""Native-Windows and hostile tests for the desktop signer permission gate."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import os
import stat
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[2]
HELPER = REPO_ROOT / "scripts" / "release" / "unsigned-desktop-handoff.py"
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "release-signing-preflight.yml"
CI_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "ci.yml"
REPOSITORY = "FerrumVir/arc-chain"
COMMIT = "0123456789abcdef0123456789abcdef01234567"
RUN_ID = 12345
RUN_ATTEMPT = 2
VERSION = "0.8.0"
PLATFORM = "windows-x86_64"
TARGET = "x86_64-pc-windows-msvc"
PAYLOAD_NAMES = (
    "arc-desktop-windows-x86_64-setup.exe",
    "arc-desktop-windows-x86_64.msi",
)
UPDATER_NAME = PAYLOAD_NAMES[0]

spec = importlib.util.spec_from_file_location("unsigned_handoff_windows", HELPER)
assert spec is not None and spec.loader is not None
HANDOFF = importlib.util.module_from_spec(spec)
spec.loader.exec_module(HANDOFF)


class PermissionModelTests(unittest.TestCase):
    def test_posix_contract_remains_exact_owner_read_only(self) -> None:
        for mode, accepted in ((0o400, True), (0o444, False), (0o600, False)):
            with self.subTest(mode=oct(mode)):
                metadata = SimpleNamespace(st_mode=stat.S_IFREG | mode)
                self.assertEqual(
                    HANDOFF.signer_input_permissions_are_read_only(
                        metadata, windows=False
                    ),
                    accepted,
                )

    def test_windows_contract_requires_attribute_and_rejects_writes_and_reparse(self) -> None:
        readonly = stat.FILE_ATTRIBUTE_READONLY
        reparse = stat.FILE_ATTRIBUTE_REPARSE_POINT
        cases = (
            (0o444, readonly, True),
            (0o444, 0, False),
            (0o644, readonly, False),
            (0o444, readonly | reparse, False),
            (0o444, None, False),
        )
        for mode, attributes, accepted in cases:
            with self.subTest(mode=oct(mode), attributes=attributes):
                values = {"st_mode": stat.S_IFREG | mode}
                if attributes is not None:
                    values["st_file_attributes"] = attributes
                metadata = SimpleNamespace(**values)
                self.assertEqual(
                    HANDOFF.signer_input_permissions_are_read_only(
                        metadata, windows=True
                    ),
                    accepted,
                )

    def test_open_file_mode_rejects_descriptor_identity_change(self) -> None:
        with tempfile.TemporaryDirectory(
            prefix="arc-open-file-mode-identity."
        ) as temporary:
            path = Path(temporary) / "receipt"
            descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            try:
                original = os.fstat(descriptor)
                replacement = SimpleNamespace(
                    st_mode=original.st_mode,
                    st_dev=original.st_dev,
                    st_ino=original.st_ino + 1,
                    st_nlink=1,
                )
                with mock.patch.object(
                    HANDOFF.os, "fstat", side_effect=(original, replacement)
                ):
                    with self.assertRaisesRegex(
                        SystemExit, "identity changed during chmod"
                    ):
                        HANDOFF.set_open_file_mode(path, descriptor, 0o400)
            finally:
                os.close(descriptor)

    def test_protected_workflow_freezes_a_native_windows_python(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        start = workflow.index(
            "- name: Validate the sealed handoff and make payloads non-executable"
        )
        end = workflow.index(
            "- name: Rehydrate the exact locked signer after materialization", start
        )
        validator = workflow[start:end]
        self.assertIn('python_candidate="$(command -v python.exe)"', validator)
        self.assertIn(
            '[ -f "$python_bin" ] && [ ! -L "$python_bin" ] && [ -x "$python_bin" ]',
            validator,
        )
        self.assertIn('current_python_sha256', validator)
        self.assertIn('[ "$current_python_sha256" = "$python_sha256" ]', validator)
        self.assertIn('"$python_bin" -I - "$GITHUB_REPOSITORY"', validator)
        self.assertNotIn(
            '/usr/bin/python3 -I - "$GITHUB_REPOSITORY"', validator
        )

    def test_windows_headless_cleanup_is_scoped_bounded_and_nonblocking(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        start = workflow.index(
            "- name: Boot the headless node and require a truthful health response"
        )
        end = workflow.index(
            "- name: Package a hash-bound validator-staging artifact", start
        )
        cleanup = workflow[start:end]
        for literal in (
            "cleanup_rc=$?",
            "MSYS_NO_PATHCONV=1 taskkill.exe",
            "for _ in {1..40}; do",
            "kill -KILL \"$node_pid\"",
            "for _ in {1..20}; do",
            "The Windows headless smoke process could not be terminated.",
            "exit \"$cleanup_rc\"",
        ):
            with self.subTest(literal=literal):
                self.assertIn(literal, cleanup)
        self.assertNotIn("taskkill.exe /PID", cleanup)

    def test_required_windows_ci_executes_native_permission_and_process_probes(self) -> None:
        workflow = CI_WORKFLOW.read_text(encoding="utf-8")
        start = workflow.index("  desktop-tauri-test:")
        end = workflow.index("  desktop-e2e:", start)
        job = workflow[start:end]
        for literal in (
            "os: [ubuntu-latest, macos-15, macos-15-intel, windows-latest]",
            "- name: Native Windows signer handoff permission boundary",
            "node-version: 24.20.0",
            "- name: Native Windows protected signer runtime boundary",
            "if: runner.os == 'Windows'",
            "test_unsigned_desktop_windows_permissions.py -v",
            "npm ci --prefix desktop --ignore-scripts",
            'node_bin="$(command -v node.exe)"',
            'tauri_cli="$PWD/desktop/node_modules/@tauri-apps/cli/tauri.js"',
            '0dd6ec63c7c63a993fde20955e291d833c03f3760e63e0ee21e83482f6c0b43a',
            "MSYS_NO_PATHCONV=1 taskkill.exe",
            "kill -KILL \"$probe_pid\"",
            "Native Windows cleanup did not terminate the probe.",
        ):
            with self.subTest(literal=literal):
                self.assertIn(literal, job)


@unittest.skipUnless(os.name == "nt", "requires native Windows chmod semantics")
class NativeWindowsPermissionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(
            prefix="arc-unsigned-desktop-windows-permissions."
        )
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        # A rejected workspace intentionally retains read-only files. Clear the
        # attribute so TemporaryDirectory can remove them on every CPython.
        for path in self.root.rglob("*"):
            if path.is_file():
                try:
                    os.chmod(path, stat.S_IREAD | stat.S_IWRITE)
                except OSError:
                    pass
        self.temporary.cleanup()

    def make_workspace(self, name: str) -> tuple[Path, argparse.Namespace]:
        workspace = self.root / name
        workspace.mkdir()
        hashes: dict[str, str] = {}
        for index, filename in enumerate(PAYLOAD_NAMES, start=1):
            payload = f"ARC native Windows payload {index}\n".encode()
            path = workspace / filename
            path.write_bytes(payload)
            hashes[filename] = hashlib.sha256(payload).hexdigest()
        receipt = {
            "schema": "arc.unsigned-desktop-materialization.v1",
            "repository": REPOSITORY,
            "commit": COMMIT,
            "workflow_run_id": RUN_ID,
            "workflow_run_attempt": RUN_ATTEMPT,
            "platform": PLATFORM,
            "rust_target": TARGET,
            "version": VERSION,
            "files": hashes,
            "modes": {filename: 0o644 for filename in PAYLOAD_NAMES},
            "updater_file": UPDATER_NAME,
        }
        (workspace / "HANDOFF-RECEIPT.json").write_bytes(
            HANDOFF.canonical_json(receipt)
        )
        (workspace / f"{UPDATER_NAME}.sig").write_bytes(b"fixture signature")
        for filename in PAYLOAD_NAMES:
            os.chmod(workspace / filename, stat.S_IREAD)
        args = argparse.Namespace(
            repository=REPOSITORY,
            commit=COMMIT,
            run_id=RUN_ID,
            run_attempt=RUN_ATTEMPT,
            platform=PLATFORM,
            rust_target=TARGET,
            version=VERSION,
            workspace=workspace,
            github_output=None,
        )
        return workspace, args

    def test_native_python312_fallback_packages_exact_read_only_handoff(self) -> None:
        package_root = self.root / "package-python312"
        payload_dir = package_root / "payload"
        payload_dir.mkdir(parents=True)
        for index, filename in enumerate(PAYLOAD_NAMES, start=1):
            (payload_dir / filename).write_bytes(
                f"ARC native Windows package payload {index}\n".encode()
            )
        stage_dir = package_root / "stage"
        github_output = package_root / "github-output"
        args = argparse.Namespace(
            repository=REPOSITORY,
            commit=COMMIT,
            run_id=RUN_ID,
            run_attempt=RUN_ATTEMPT,
            platform=PLATFORM,
            rust_target=TARGET,
            version=VERSION,
            payload_dir=payload_dir,
            stage_dir=stage_dir,
            github_output=github_output,
        )

        # Python 3.12 on the hosted Windows image has no os.fchmod. Keep this
        # exact fallback exercised even after the runner eventually advances.
        with mock.patch.object(HANDOFF.os, "fchmod", None, create=True):
            HANDOFF.package(args)

        outputs = dict(
            line.split("=", 1)
            for line in github_output.read_text(encoding="utf-8").splitlines()
        )
        expected_members = {
            "BUILD-METADATA.json",
            "SHA256SUMS",
            outputs["archive_name"],
        }
        self.assertEqual({path.name for path in stage_dir.iterdir()}, expected_members)
        for path in stage_dir.iterdir():
            metadata = path.lstat()
            self.assertEqual(
                metadata.st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT, 0
            )
            self.assertNotEqual(
                metadata.st_file_attributes & stat.FILE_ATTRIBUTE_READONLY, 0
            )

    def test_native_read_only_payload_passes_and_release_mode_is_restored(self) -> None:
        workspace, args = self.make_workspace("accepted")
        for filename in PAYLOAD_NAMES:
            metadata = (workspace / filename).lstat()
            self.assertNotEqual(
                metadata.st_file_attributes & stat.FILE_ATTRIBUTE_READONLY, 0
            )
            self.assertTrue(
                HANDOFF.signer_input_permissions_are_read_only(
                    metadata, windows=True
                )
            )

        HANDOFF.verify_signed(args)

        for filename in PAYLOAD_NAMES:
            metadata = (workspace / filename).lstat()
            self.assertEqual(
                metadata.st_file_attributes & stat.FILE_ATTRIBUTE_READONLY, 0
            )

    def test_native_writable_payload_is_rejected_with_identical_bytes(self) -> None:
        workspace, args = self.make_workspace("writable")
        hostile = workspace / PAYLOAD_NAMES[1]
        os.chmod(hostile, stat.S_IREAD | stat.S_IWRITE)
        metadata = hostile.lstat()
        self.assertEqual(
            metadata.st_file_attributes & stat.FILE_ATTRIBUTE_READONLY, 0
        )

        with self.assertRaisesRegex(SystemExit, "regained permissions"):
            HANDOFF.verify_signed(args)


if __name__ == "__main__":
    unittest.main()
