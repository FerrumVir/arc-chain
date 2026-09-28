#!/usr/bin/env python3
"""Focused regression tests for ad-hoc sealing macOS desktop packages."""

from __future__ import annotations

import hashlib
import importlib.util
import plistlib
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts/release/seal_macos_app_bundle.py"
SPEC = importlib.util.spec_from_file_location("seal_macos_app_bundle", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
seal = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(seal)


class BundleSealingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bundle = self.root / "bundle"
        self.macos = self.bundle / "macos"
        self.dmg_dir = self.bundle / "dmg"
        app = self.macos / seal.APP_NAME
        executable = app / "Contents/MacOS/arc-desktop"
        executable.parent.mkdir(parents=True)
        executable.write_bytes(b"fixture executable")
        executable.chmod(0o755)
        (app / "Contents/Resources").mkdir()
        (app / "Contents/Resources/config.txt").write_text("resource\n")
        (app / "Contents/Info.plist").write_bytes(plistlib.dumps({
            "CFBundleIdentifier": seal.APP_IDENTIFIER,
            "CFBundleShortVersionString": seal.APP_VERSION,
            "CFBundleExecutable": seal.EXECUTABLE_NAME,
        }))
        self.archive = self.macos / "ARC Node.app.tar.gz"
        self.archive.write_bytes(b"old archive")
        self.dmg_dir.mkdir()
        self.dmg = self.dmg_dir / "ARC Node.dmg"
        self.dmg.write_bytes(b"old disk image")
        self.tools = self.root / "tools"
        self.tools.mkdir()
        seal.CODE_SIGN = self.tools / "codesign"
        seal.HDITIL = self.tools / "hdiutil"
        seal.TAR = Path("/usr/bin/tar")
        seal.CODE_SIGN.touch()
        seal.HDITIL.touch()
        self.commands: list[list[str]] = []
        self.dmg_source: Path | None = None

    def fake_run(self, command: list[str], *, env=None):
        self.commands.append(command)
        if command[0] == str(seal.CODE_SIGN):
            if "--sign" in command:
                app = Path(command[-1])
                code_resources = app / "Contents/_CodeSignature/CodeResources"
                code_resources.parent.mkdir(parents=True, exist_ok=True)
                code_resources.write_text("adhoc resource seal\n")
            else:
                app = Path(command[-1])
                self.assertTrue((app / "Contents/_CodeSignature/CodeResources").is_file())
            return subprocess.CompletedProcess(command, 0, b"", b"")
        if command[0] == str(seal.TAR):
            return subprocess.run(command, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                  timeout=30, env=env)
        if command[0] == str(seal.HDITIL):
            if command[1] == "create":
                src = Path(command[command.index("-srcfolder") + 1])
                target = Path(command[-1])
                self.dmg_source = src
                target.write_bytes(b"new verified disk image")
            elif command[1] == "verify":
                self.assertTrue(Path(command[-1]).is_file())
            elif command[1] == "attach":
                mount = Path(command[command.index("-mountpoint") + 1])
                assert self.dmg_source is not None
                shutil.copytree(self.dmg_source / seal.APP_NAME, mount / seal.APP_NAME,
                                symlinks=True)
            elif command[1] != "detach":
                self.fail(f"unexpected hdiutil command: {command}")
            return subprocess.CompletedProcess(command, 0, b"", b"")
        self.fail(f"unexpected command: {command}")

    def test_seals_before_rebuilding_archive_and_dmg_and_verifies_all_copies(self) -> None:
        with mock.patch.object(seal, "run", side_effect=self.fake_run):
            receipt = seal.seal_and_repackage(self.bundle)

        self.assertEqual(receipt["executable_sha256"], hashlib.sha256(b"fixture executable").hexdigest())
        self.assertNotEqual(self.archive.read_bytes(), b"old archive")
        self.assertNotEqual(self.dmg.read_bytes(), b"old disk image")
        self.assertTrue((self.macos / seal.APP_NAME / "Contents/_CodeSignature/CodeResources").exists())
        sign_at = next(i for i, command in enumerate(self.commands) if "--sign" in command)
        archive_create_at = next(i for i, command in enumerate(self.commands)
                                 if command[0] == str(seal.TAR) and "-czf" in command)
        dmg_create_at = next(i for i, command in enumerate(self.commands)
                             if command[0] == str(seal.HDITIL) and command[1] == "create")
        self.assertLess(sign_at, archive_create_at)
        self.assertLess(sign_at, dmg_create_at)
        verify_targets = [Path(command[-1]) for command in self.commands
                          if command[0] == str(seal.CODE_SIGN) and "--verify" in command]
        self.assertGreaterEqual(len(verify_targets), 4)
        self.assertEqual({target.name for target in verify_targets}, {seal.APP_NAME})

    def test_wrong_app_identity_fails_before_signing_or_replacing_packages(self) -> None:
        info = self.macos / seal.APP_NAME / "Contents/Info.plist"
        info.write_bytes(plistlib.dumps({
            "CFBundleIdentifier": "wrong.identifier",
            "CFBundleShortVersionString": seal.APP_VERSION,
            "CFBundleExecutable": seal.EXECUTABLE_NAME,
        }))
        with mock.patch.object(seal, "run", side_effect=self.fake_run):
            with self.assertRaisesRegex(RuntimeError, "identity/version/executable"):
                seal.seal_and_repackage(self.bundle)
        self.assertEqual(self.commands, [])
        self.assertEqual(self.archive.read_bytes(), b"old archive")
        self.assertEqual(self.dmg.read_bytes(), b"old disk image")

    def test_signing_failure_keeps_existing_package_outputs(self) -> None:
        def fail_sign(command: list[str], *, env=None):
            self.commands.append(command)
            raise subprocess.CalledProcessError(1, command, stderr=b"codesign failed")

        with mock.patch.object(seal, "run", side_effect=fail_sign):
            with self.assertRaises(subprocess.CalledProcessError):
                seal.seal_and_repackage(self.bundle)
        self.assertEqual(self.archive.read_bytes(), b"old archive")
        self.assertEqual(self.dmg.read_bytes(), b"old disk image")

    @unittest.skipUnless(sys.platform == "darwin", "requires native macOS codesign and hdiutil")
    def test_native_codesign_archive_and_dmg_round_trip(self) -> None:
        if not all(Path(tool).is_file() for tool in ("/usr/bin/codesign", "/usr/bin/hdiutil", "/usr/bin/tar")):
            self.skipTest("native macOS packaging tools are unavailable")
        executable = self.macos / seal.APP_NAME / "Contents/MacOS/arc-desktop"
        shutil.copyfile("/usr/bin/true", executable)
        executable.chmod(0o755)
        seal.CODE_SIGN = Path("/usr/bin/codesign")
        seal.HDITIL = Path("/usr/bin/hdiutil")
        seal.TAR = Path("/usr/bin/tar")

        receipt = seal.seal_and_repackage(self.bundle)

        self.assertTrue(receipt["executable_sha256"])
        self.assertGreater(self.archive.stat().st_size, len(b"old archive"))
        self.assertGreater(self.dmg.stat().st_size, len(b"old disk image"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
