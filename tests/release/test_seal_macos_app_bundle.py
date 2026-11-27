#!/usr/bin/env python3

from __future__ import annotations

import hashlib
import importlib.util
import json
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
        self.expected_version = seal.expected_app_version()
        self.write_info_plist(version=self.expected_version)
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
        self._repo_counter = 0

    def write_info_plist(
        self,
        *,
        identifier: str = seal.APP_IDENTIFIER,
        version: str | None = None,
        executable: str = seal.EXECUTABLE_NAME,
    ) -> None:
        info = self.macos / seal.APP_NAME / "Contents/Info.plist"
        payload: dict[str, object] = {
            "CFBundleIdentifier": identifier,
            "CFBundleExecutable": executable,
        }
        if version is not None:
            payload["CFBundleShortVersionString"] = version
        info.write_bytes(plistlib.dumps(payload))

    def make_repo_fixture(self, content: str | None) -> Path:
        self._repo_counter += 1
        repo = self.root / f"repo-{self._repo_counter}"
        config = repo / "desktop/src-tauri/tauri.conf.json"
        config.parent.mkdir(parents=True, exist_ok=True)
        if content is not None:
            config.write_text(content, encoding="utf-8")
        return repo

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
            "CFBundleShortVersionString": self.expected_version,
            "CFBundleExecutable": seal.EXECUTABLE_NAME,
        }))
        with mock.patch.object(seal, "run", side_effect=self.fake_run):
            with self.assertRaisesRegex(RuntimeError, "identity/version/executable"):
                seal.seal_and_repackage(self.bundle)
        self.assertEqual(self.commands, [])
        self.assertEqual(self.archive.read_bytes(), b"old archive")
        self.assertEqual(self.dmg.read_bytes(), b"old disk image")

    def test_expected_app_version_derived_from_repo_tauri_config(self) -> None:
        config = json.loads((REPO_ROOT / "desktop/src-tauri/tauri.conf.json").read_text(encoding="utf-8"))
        self.assertEqual(seal.expected_app_version(), config["version"])
        self.assertFalse(hasattr(seal, "APP_VERSION"))

    def test_rejects_stale_and_new_version_mismatch_and_accepts_bumped_release(self) -> None:
        bumped_repo = self.make_repo_fixture(json.dumps({
            "productName": "ARC Node",
            "identifier": seal.APP_IDENTIFIER,
            "version": "0.8.24",
        }))
        stale_repo = self.make_repo_fixture(json.dumps({
            "productName": "ARC Node",
            "identifier": seal.APP_IDENTIFIER,
            "version": "0.8.23",
        }))

        for label, repo, bundle_version in (
            ("stale-bundle-vs-new-config", bumped_repo, "0.8.23"),
            ("new-bundle-vs-stale-config", stale_repo, "0.8.24"),
            ("ahead-bundle-vs-new-config", bumped_repo, "0.8.25"),
            ("missing-bundle-version", bumped_repo, None),
        ):
            with self.subTest(case=label):
                self.commands.clear()
                self.write_info_plist(version=bundle_version)
                with mock.patch.object(seal, "REPO_ROOT", repo):
                    with mock.patch.object(seal, "run", side_effect=self.fake_run):
                        with self.assertRaisesRegex(RuntimeError, "identity/version/executable"):
                            seal.seal_and_repackage(self.bundle)
                self.assertEqual(self.commands, [])
                self.assertEqual(self.archive.read_bytes(), b"old archive")
                self.assertEqual(self.dmg.read_bytes(), b"old disk image")

        self.commands.clear()
        self.write_info_plist(version="0.8.24")
        with mock.patch.object(seal, "REPO_ROOT", bumped_repo):
            with mock.patch.object(seal, "run", side_effect=self.fake_run):
                receipt = seal.seal_and_repackage(self.bundle)
        self.assertEqual(receipt["executable_sha256"], hashlib.sha256(b"fixture executable").hexdigest())
        self.assertNotEqual(self.archive.read_bytes(), b"old archive")
        self.assertNotEqual(self.dmg.read_bytes(), b"old disk image")

    def test_invalid_release_version_configuration_fails_closed(self) -> None:
        invalid_versions: list[tuple[str, object]] = [
            ("missing", ...),
            ("none", None),
            ("empty", ""),
            ("integer", 824),
            ("v-prefix", "v0.8.24"),
            ("two-part", "0.8"),
            ("four-part", "0.8.24.1"),
            ("prerelease", "0.8.24-rc.1"),
            ("build-metadata", "0.8.24+build.1"),
            ("leading-zero", "0.08.24"),
            ("trailing-newline", "0.8.24\n"),
            ("whitespace", " 0.8.24 "),
        ]
        for label, bad_version in invalid_versions:
            with self.subTest(version_case=label):
                self.commands.clear()
                document: dict[str, object] = {
                    "productName": "ARC Node",
                    "identifier": seal.APP_IDENTIFIER,
                }
                if bad_version is not ...:
                    document["version"] = bad_version
                repo = self.make_repo_fixture(json.dumps(document))
                self.write_info_plist(version=bad_version if isinstance(bad_version, str) else "0.8.24")
                with mock.patch.object(seal, "REPO_ROOT", repo):
                    with mock.patch.object(seal, "run", side_effect=self.fake_run):
                        with self.assertRaisesRegex(RuntimeError, "strict MAJOR.MINOR.PATCH"):
                            seal.seal_and_repackage(self.bundle)
                self.assertEqual(self.commands, [])
                self.assertEqual(self.archive.read_bytes(), b"old archive")
                self.assertEqual(self.dmg.read_bytes(), b"old disk image")

        self.write_info_plist(version="0.8.24")
        invalid_configs: list[tuple[str, str | None, str]] = [
            ("missing-file", None, "missing or symlinked"),
            ("invalid-json", "{invalid", "invalid JSON"),
            ("non-object-json", "[]", "JSON object"),
            ("missing-identifier", json.dumps({"productName": "ARC Node", "version": "0.8.24"}), "identity"),
            ("wrong-identifier", json.dumps({
                "productName": "ARC Node",
                "identifier": "network.arc.other",
                "version": "0.8.24",
            }), "identity"),
            ("missing-product-name", json.dumps({"identifier": seal.APP_IDENTIFIER, "version": "0.8.24"}), "identity"),
            ("wrong-product-name", json.dumps({
                "productName": "Other App",
                "identifier": seal.APP_IDENTIFIER,
                "version": "0.8.24",
            }), "identity"),
        ]
        for label, raw_content, pattern in invalid_configs:
            with self.subTest(config_case=label):
                self.commands.clear()
                repo = self.make_repo_fixture(raw_content)
                with mock.patch.object(seal, "REPO_ROOT", repo):
                    with mock.patch.object(seal, "run", side_effect=self.fake_run):
                        with self.assertRaisesRegex(RuntimeError, pattern):
                            seal.seal_and_repackage(self.bundle)
                self.assertEqual(self.commands, [])
                self.assertEqual(self.archive.read_bytes(), b"old archive")
                self.assertEqual(self.dmg.read_bytes(), b"old disk image")

        external_dir = self.root / "external-src-tauri"
        external_dir.mkdir()
        external_config = external_dir / "tauri.conf.json"
        external_config.write_text(json.dumps({
            "productName": "ARC Node",
            "identifier": seal.APP_IDENTIFIER,
            "version": "0.8.24",
        }), encoding="utf-8")

        symlink_file_repo = self.make_repo_fixture(None)
        (symlink_file_repo / "desktop/src-tauri/tauri.conf.json").symlink_to(external_config)

        symlink_dir_repo = self.root / "repo-symlink-dir"
        (symlink_dir_repo / "desktop").mkdir(parents=True)
        (symlink_dir_repo / "desktop/src-tauri").symlink_to(external_dir)

        for label, repo, pattern in (
            ("symlinked-config-file", symlink_file_repo, "missing or symlinked"),
            ("symlinked-config-parent", symlink_dir_repo, "repository boundary"),
        ):
            with self.subTest(symlink_case=label):
                self.commands.clear()
                with mock.patch.object(seal, "REPO_ROOT", repo):
                    with mock.patch.object(seal, "run", side_effect=self.fake_run):
                        with self.assertRaisesRegex(RuntimeError, pattern):
                            seal.seal_and_repackage(self.bundle)
                self.assertEqual(self.commands, [])
                self.assertEqual(self.archive.read_bytes(), b"old archive")
                self.assertEqual(self.dmg.read_bytes(), b"old disk image")

        with self.assertRaisesRegex(RuntimeError, "strict MAJOR.MINOR.PATCH"):
            seal.app_identity(self.macos / seal.APP_NAME, "v0.8.24")

    def test_malformed_or_symlinked_info_plist_fails_closed(self) -> None:
        info = self.macos / seal.APP_NAME / "Contents/Info.plist"
        external_plist = self.root / "external-Info.plist"
        external_plist.write_bytes(plistlib.dumps({
            "CFBundleIdentifier": seal.APP_IDENTIFIER,
            "CFBundleShortVersionString": self.expected_version,
            "CFBundleExecutable": seal.EXECUTABLE_NAME,
        }))

        for label, setup in (
            ("invalid-plist", lambda: info.write_bytes(b"not a plist")),
            ("non-dict-plist", lambda: info.write_bytes(plistlib.dumps([self.expected_version]))),
            ("symlinked-plist", lambda: (info.unlink(), info.symlink_to(external_plist))),
        ):
            with self.subTest(plist_case=label):
                self.commands.clear()
                setup()
                with mock.patch.object(seal, "run", side_effect=self.fake_run):
                    with self.assertRaisesRegex(RuntimeError, "Info.plist is missing or invalid"):
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
