#!/usr/bin/env python3
"""Focused fail-closed tests for the macOS package provenance controller."""

from __future__ import annotations

import hashlib
import importlib.util
import io
import json
import platform
import plistlib
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
from argparse import Namespace
from pathlib import Path
from unittest import mock


HELPER = Path(__file__).with_name("build-macos-package-provenance.py")
SPEC = importlib.util.spec_from_file_location("arc_macos_package_provenance", HELPER)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot load {HELPER}")
gate = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = gate
SPEC.loader.exec_module(gate)


def digest(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def tar_member(name: str, kind: str, raw: bytes = b"", *, target: str = "") -> tuple[tarfile.TarInfo, bytes]:
    member = tarfile.TarInfo(name)
    if kind == "directory":
        member.type = tarfile.DIRTYPE
        member.mode = 0o755
        member.size = 0
    elif kind == "file":
        member.type = tarfile.REGTYPE
        member.mode = 0o755 if name.endswith("arc-desktop") else 0o644
        member.size = len(raw)
    elif kind == "symlink":
        member.type = tarfile.SYMTYPE
        member.mode = 0o777
        member.linkname = target
        member.size = 0
    elif kind == "hardlink":
        member.type = tarfile.LNKTYPE
        member.mode = 0o644
        member.linkname = target
        member.size = 0
    elif kind == "fifo":
        member.type = tarfile.FIFOTYPE
        member.mode = 0o644
        member.size = 0
    else:
        raise AssertionError(kind)
    return member, raw


def write_tar(path: Path, rows: list[tuple[tarfile.TarInfo, bytes]]) -> None:
    with tarfile.open(path, "w:gz") as archive:
        for member, raw in rows:
            archive.addfile(member, io.BytesIO(raw) if member.isfile() else None)
    path.chmod(0o400)


def valid_tar_rows() -> list[tuple[tarfile.TarInfo, bytes]]:
    prefix = gate.APP_BUNDLE_NAME
    plist = plistlib.dumps(
        {
            "CFBundleExecutable": "arc-desktop",
            "CFBundleIdentifier": "network.arc.desktop",
        },
        fmt=plistlib.FMT_BINARY,
    )
    return [
        tar_member(prefix, "directory"),
        tar_member(f"{prefix}/Contents", "directory"),
        tar_member(f"{prefix}/Contents/MacOS", "directory"),
        tar_member(f"{prefix}/Contents/Resources", "directory"),
        tar_member(f"{prefix}/Contents/Info.plist", "file", plist),
        tar_member(f"{prefix}/Contents/MacOS/arc-desktop", "file", b"exact-mach-o"),
        tar_member(f"{prefix}/Contents/Resources/data", "file", b"resource"),
        tar_member(
            f"{prefix}/Contents/Resources/data-link",
            "symlink",
            target="data",
        ),
    ]


class BindingTests(unittest.TestCase):
    def test_duplicate_required_macos_asset_ids_fail(self) -> None:
        with tempfile.TemporaryDirectory(dir=HELPER.parent) as raw:
            root = Path(raw)
            root.chmod(0o700)
            commit = "ab" * 20
            assets = {
                name: {"id": 1, "sha256": marker * 64, "size": 1}
                for name, marker in (
                    (gate.APP_ARCHIVE_NAME, "1"),
                    (gate.APP_SIGNATURE_NAME, "2"),
                    (gate.DMG_NAME, "3"),
                )
            }
            binding = {
                "assets": assets,
                "commit": commit,
                "legacy_source": {},
                "published_evidence": {},
                "release": {"id": 1, "immutable": True},
                "release_workflow": {
                    "event": "workflow_dispatch",
                    "head_branch": "main",
                    "head_sha": commit,
                    "id": 1,
                    "jobs_sha256": "4" * 64,
                    "path": ".github/workflows/release.yml",
                    "run_attempt": 1,
                    "run_id": 1,
                },
                "repository": gate.REPOSITORY,
                "schema": gate.RELEASE_SCHEMA,
                "tag": gate.TAG,
            }
            path = root / "release-binding.json"
            path.write_bytes(gate.canonical_json(binding))
            path.chmod(0o400)
            with self.assertRaisesRegex(gate.ProvenanceError, "IDs are not distinct"):
                gate.load_release_binding(path)


class ExtractionTests(unittest.TestCase):
    def setUp(self) -> None:
        # macOS maps /var through a root symlink.  The production controller
        # deliberately rejects symlinked input ancestry, so keep extraction
        # fixtures under the real checked-out source directory too.
        self.temporary = tempfile.TemporaryDirectory(dir=HELPER.parent)
        self.root = Path(self.temporary.name)
        self.root.chmod(0o700)
        self.archive = self.root / gate.APP_ARCHIVE_NAME

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def test_safe_extraction_is_create_only_bounded_and_tree_hashed(self) -> None:
        write_tar(self.archive, valid_tar_rows())
        destination = self.root / "extracted"
        evidence = gate.safe_extract_archive(self.archive, destination)
        self.assertEqual(evidence["manifest"]["memberCount"], 8)
        self.assertEqual(evidence["manifest"]["regularFileCount"], 3)
        self.assertTrue(evidence["safety"]["descriptorRelativeCreateOnly"])
        app = destination / gate.APP_BUNDLE_NAME
        self.assertEqual((app / "Contents/Resources/data-link").read_bytes(), b"resource")
        self.assertEqual(evidence["appBundleTreeSha256"], gate.app_bundle_tree(app, "fixture")["sha256"])
        with self.assertRaisesRegex(gate.ProvenanceError, "must not already exist"):
            gate.safe_extract_archive(self.archive, destination)

    def test_traversal_is_rejected_before_destination_creation(self) -> None:
        rows = valid_tar_rows()
        rows.append(tar_member(f"{gate.APP_BUNDLE_NAME}/../escape", "file", b"bad"))
        write_tar(self.archive, rows)
        destination = self.root / "extracted"
        with self.assertRaisesRegex(gate.ProvenanceError, "traversal"):
            gate.safe_extract_archive(self.archive, destination)
        self.assertFalse(destination.exists())

    def test_absolute_and_wrong_root_paths_are_rejected(self) -> None:
        for name in ("/tmp/escape", "Other.app/file"):
            with self.subTest(name=name):
                write_tar(self.archive, [tar_member(name, "file", b"bad")])
                with self.assertRaises(gate.ProvenanceError):
                    gate.archive_manifest(self.archive)
                self.archive.chmod(0o600)
                self.archive.unlink()

    def test_hardlinks_and_special_entries_are_rejected(self) -> None:
        for row in (
            tar_member(f"{gate.APP_BUNDLE_NAME}/hard", "hardlink", target="target"),
            tar_member(f"{gate.APP_BUNDLE_NAME}/fifo", "fifo"),
        ):
            with self.subTest(kind=row[0].type):
                write_tar(self.archive, [tar_member(gate.APP_BUNDLE_NAME, "directory"), row])
                with self.assertRaisesRegex(gate.ProvenanceError, "hardlink or special"):
                    gate.archive_manifest(self.archive)
                self.archive.chmod(0o600)
                self.archive.unlink()

    def test_escape_and_nonportable_symlinks_are_rejected(self) -> None:
        for target in ("../../../outside", "/Applications", "t\narget"):
            with self.subTest(target=target):
                rows = valid_tar_rows()[:-1]
                rows.append(
                    tar_member(
                        f"{gate.APP_BUNDLE_NAME}/Contents/Resources/link",
                        "symlink",
                        target=target,
                    )
                )
                write_tar(self.archive, rows)
                with self.assertRaises(gate.ProvenanceError):
                    gate.archive_manifest(self.archive)
                self.archive.chmod(0o600)
                self.archive.unlink()

    def test_casefold_collision_is_rejected(self) -> None:
        rows = valid_tar_rows()
        rows.extend(
            [
                tar_member(f"{gate.APP_BUNDLE_NAME}/Contents/Resources/A", "file", b"a"),
                tar_member(f"{gate.APP_BUNDLE_NAME}/Contents/Resources/a", "file", b"b"),
            ]
        )
        write_tar(self.archive, rows)
        with self.assertRaisesRegex(gate.ProvenanceError, "colliding"):
            gate.archive_manifest(self.archive)

    def test_member_and_total_size_caps_fail_closed(self) -> None:
        rows = valid_tar_rows()
        rows.append(tar_member(f"{gate.APP_BUNDLE_NAME}/Contents/Resources/large", "file", b"12345"))
        write_tar(self.archive, rows)
        with mock.patch.object(gate, "MAX_ARCHIVE_MEMBER_BYTES", 4):
            with self.assertRaisesRegex(gate.ProvenanceError, "size bound"):
                gate.archive_manifest(self.archive)
        with mock.patch.object(gate, "MAX_ARCHIVE_TOTAL_BYTES", 4):
            with self.assertRaisesRegex(gate.ProvenanceError, "total uncompressed"):
                gate.archive_manifest(self.archive)

    def test_missing_explicit_parent_is_rejected(self) -> None:
        write_tar(
            self.archive,
            [
                tar_member(gate.APP_BUNDLE_NAME, "directory"),
                tar_member(f"{gate.APP_BUNDLE_NAME}/Contents/file", "file", b"bad"),
            ],
        )
        with self.assertRaisesRegex(gate.ProvenanceError, "explicit parent"):
            gate.archive_manifest(self.archive)


class MountParsingTests(unittest.TestCase):
    def test_attach_source_device_disk_and_flags_cross_bind(self) -> None:
        mount = Path("/private/tmp/arc mount")
        dmg = Path("/private/tmp/release.dmg")
        device = "/dev/disk42s1"
        attach = {"system-entities": [{"dev-entry": device, "mount-point": str(mount)}]}
        self.assertEqual(gate.mounted_entity(attach, mount)["dev-entry"], device)
        with mock.patch.object(Path, "resolve", return_value=dmg):
            source = gate.validate_hdiutil_source(
                {
                    "images": [
                        {
                            "image-path": str(dmg),
                            "writeable": False,
                            "system-entities": [
                                {"dev-entry": device, "mount-point": str(mount)}
                            ],
                        }
                    ]
                },
                dmg,
                mount,
                device,
            )
        self.assertTrue(source["imagePathMatched"])
        disk = gate.validate_diskutil_info(
            {
                "DeviceNode": device,
                "FilesystemType": "apfs",
                "MountPoint": str(mount),
                "Owners": False,
                "ReadOnlyMedia": True,
                "ReadOnlyVolume": True,
            },
            mount,
            device,
        )
        self.assertTrue(disk["ownersDisabled"])
        mounted = gate.validate_mount_output(
            f"{device} on {mount} (apfs, local, read-only, noowners, nobrowse)\n".encode(),
            mount,
            device,
        )
        self.assertIn("read-only", mounted["options"])

    def test_any_writable_or_ambiguous_mount_claim_is_rejected(self) -> None:
        mount = Path("/private/tmp/mount")
        device = "/dev/disk4s1"
        with self.assertRaises(gate.ProvenanceError):
            gate.mounted_entity(
                {
                    "system-entities": [
                        {"dev-entry": device, "mount-point": str(mount)},
                        {"dev-entry": "/dev/disk5s1", "mount-point": str(mount)},
                    ]
                },
                mount,
            )
        with self.assertRaisesRegex(gate.ProvenanceError, "read-only"):
            gate.validate_diskutil_info(
                {
                    "DeviceNode": device,
                    "FilesystemType": "apfs",
                    "MountPoint": str(mount),
                    "Owners": False,
                    "ReadOnlyMedia": True,
                    "ReadOnlyVolume": False,
                },
                mount,
                device,
            )
        with self.assertRaisesRegex(gate.ProvenanceError, "nobrowse"):
            gate.validate_mount_output(
                f"{device} on {mount} (apfs, read-only, noowners)\n".encode(), mount, device
            )


class CodeSignatureTests(unittest.TestCase):
    @unittest.skipUnless(platform.system() == "Darwin", "requires macOS codesign")
    def test_real_adhoc_app_bundle_matches_parser_contract(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            app = root / gate.APP_BUNDLE_NAME
            executable = app / "Contents/MacOS/arc-desktop"
            executable.parent.mkdir(parents=True)
            shutil.copyfile("/usr/bin/true", executable)
            executable.chmod(0o755)
            (app / "Contents/Info.plist").write_bytes(
                plistlib.dumps(
                    {
                        "CFBundleExecutable": "arc-desktop",
                        "CFBundleIdentifier": "network.arc.desktop",
                    }
                )
            )
            subprocess.run(
                ["/usr/bin/codesign", "--force", "--deep", "--sign", "-", str(app)],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            identity = gate.code_signature(
                app,
                {"HOME": str(root), "LANG": "C", "LC_ALL": "C", "PATH": gate.FIXED_PATH, "TMPDIR": str(root)},
            )
            self.assertEqual(identity["identifier"], "network.arc.desktop")
            self.assertEqual(identity["kind"], "adhoc")
            self.assertEqual(identity["teamIdentifier"], None)
            self.assertTrue(identity["verifyDeepStrict"])

    def test_only_truthful_ad_hoc_signature_is_accepted(self) -> None:
        display = (
            b"Executable=/Volumes/ARC Node/ARC Node.app/Contents/MacOS/arc-desktop\n"
            b"Identifier=network.arc.desktop\n"
            b"Format=app bundle with Mach-O thin (arm64)\n"
            b"CodeDirectory v=20400 size=100 flags=0x2(adhoc) hashes=1+3 location=embedded\n"
            b"Signature=adhoc\n"
            b"TeamIdentifier=not set\n"
        )
        requirement = b'designated => identifier "network.arc.desktop"\n'
        results = [
            subprocess.CompletedProcess([], 0, b"", b"valid\n"),
            subprocess.CompletedProcess([], 0, b"", display),
            subprocess.CompletedProcess([], 0, b"", requirement),
        ]
        plist = plistlib.dumps(
            {"CFBundleExecutable": "arc-desktop", "CFBundleIdentifier": "network.arc.desktop"}
        )
        with (
            mock.patch.object(gate, "run_command", side_effect=results),
            mock.patch.object(
                gate,
                "executable_identity",
                return_value={"path": "/usr/bin/codesign", "resolvedPath": "/usr/bin/codesign", "sha256": "a" * 64, "size": 1},
            ),
            mock.patch.object(gate, "read_bytes", return_value=plist),
        ):
            identity = gate.code_signature(Path("/Volumes/ARC Node/ARC Node.app"), {})
        self.assertEqual(identity["kind"], "adhoc")
        self.assertFalse(identity["appleDeveloperIdSigned"])
        self.assertFalse(identity["notarizationAssessed"])
        self.assertEqual(identity["authorities"], [])

    def test_developer_id_authority_is_not_mislabeled_ad_hoc(self) -> None:
        display = (
            b"Identifier=network.arc.desktop\n"
            b"CodeDirectory v=20400 flags=0x10000(runtime)\n"
            b"Signature=Developer ID\n"
            b"Authority=Developer ID Application: Example\n"
            b"TeamIdentifier=ABCDE12345\n"
        )
        results = [
            subprocess.CompletedProcess([], 0, b"", b""),
            subprocess.CompletedProcess([], 0, b"", display),
            subprocess.CompletedProcess([], 0, b"", b'designated => identifier "network.arc.desktop"\n'),
        ]
        with (
            mock.patch.object(gate, "run_command", side_effect=results),
            mock.patch.object(gate, "executable_identity", return_value={}),
        ):
            with self.assertRaisesRegex(gate.ProvenanceError, "ad-hoc"):
                gate.code_signature(Path("/Volumes/ARC Node/ARC Node.app"), {})


class DurableEvidenceTests(unittest.TestCase):
    def test_create_file_is_canonical_boundary_and_refuses_replacement(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            root.chmod(0o700)
            output = root / "receipt.json"
            gate.create_file(output, b"evidence\n", "fixture")
            info = output.lstat()
            self.assertEqual(stat.S_IMODE(info.st_mode), 0o400)
            self.assertEqual(info.st_nlink, 1)
            with self.assertRaises(FileExistsError):
                gate.create_file(output, b"replacement\n", "fixture")
            self.assertEqual(output.read_bytes(), b"evidence\n")

    def test_native_process_has_exact_env_and_outer_timeout_evidence(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            script = root / "runner"
            script.write_text("#!/bin/sh\nprintf '%s' \"$HOME|$TMPDIR|$PATH|$LANG|$LC_ALL\"\n")
            script.chmod(0o700)
            native_input = root / "DESKTOP-LIVE-INPUT.json"
            native_input.write_text("{}\n")
            evidence = gate.run_native_process(
                script,
                native_input,
                root / "PACKAGED-NATIVE-ACCEPTANCE.json",
                {
                    "HOME": "/private/isolated-home",
                    "TMPDIR": "/private/isolated-home/tmp",
                    "PATH": gate.FIXED_PATH,
                    "LANG": "C",
                    "LC_ALL": "C",
                },
            )
            self.assertEqual(evidence["returnCode"], 0)
            self.assertFalse(evidence["timedOut"])
            self.assertEqual(evidence["innerMaxSeconds"], 4300)
            self.assertEqual(evidence["outerTimeoutSeconds"], 4360)

    def test_native_timeout_is_terminal_and_kills_process_group(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            script = root / "runner"
            script.write_text("#!/bin/sh\n/bin/sleep 5\n")
            script.chmod(0o700)
            native_input = root / "DESKTOP-LIVE-INPUT.json"
            native_input.write_text("{}\n")
            with mock.patch.object(gate, "NATIVE_OUTER_TIMEOUT_SECONDS", 1):
                started = time.monotonic()
                with self.assertRaisesRegex(gate.ProvenanceError, "terminal/ambiguous"):
                    gate.run_native_process(script, native_input, root / "out", {})
                self.assertLess(time.monotonic() - started, 3)


class NativeInputTests(unittest.TestCase):
    def test_build_input_derives_release_assets_bundle_and_fresh_challenge(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            root.chmod(0o700)
            output = root / "DESKTOP-LIVE-INPUT.json"
            assets = {
                gate.APP_ARCHIVE_NAME: {"id": 1, "name": gate.APP_ARCHIVE_NAME, "sha256": "1" * 64, "size": 10},
                gate.APP_SIGNATURE_NAME: {"id": 2, "name": gate.APP_SIGNATURE_NAME, "sha256": "2" * 64, "size": 11},
                gate.DMG_NAME: {"id": 3, "name": gate.DMG_NAME, "sha256": "3" * 64, "size": 12},
            }
            binding = {
                "commit": "a" * 40,
                "release": {"id": 44},
                "release_workflow": {"run_id": 55, "run_attempt": 2},
            }
            bundle = {
                "appBundleTreeSha256": "4" * 64,
                "executableRelativePath": gate.EXECUTABLE_RELATIVE_PATH,
                "executableSha256": "5" * 64,
                "executableSize": 123,
            }
            inspection = {
                "assets": {
                    "appArchive": assets[gate.APP_ARCHIVE_NAME],
                    "appArchiveSignature": assets[gate.APP_SIGNATURE_NAME],
                    "dmg": assets[gate.DMG_NAME],
                },
                "bundle": bundle,
            }
            args = Namespace(
                expected_model_id="0x" + "6" * 64,
                frontend_commit="b" * 40,
                frontend_config_sha256="7" * 64,
                inspection=root / "inspection",
                maximum_block_age_seconds=300,
                minimum_peers=5,
                output=output,
                recovery_epoch=9,
                rollout_manifest_sha256="8" * 64,
                transaction_domain="0x" + "9" * 64,
                validator_set_commitment="0x" + "c" * 64,
                validator_set_id=4,
                validity_seconds=7200,
            )
            with (
                mock.patch.object(
                    gate,
                    "common_inputs",
                    return_value=(binding, b"binding\n", assets, {"sha256": "d" * 64}, b"key\n", {}, b"signature\n"),
                ),
                mock.patch.object(gate, "validate_inspection", return_value=(inspection, b"inspection\n")),
                mock.patch.object(gate, "validate_native_input", wraps=gate.validate_native_input),
            ):
                value = gate.build_native_input(args)
            self.assertEqual(value["assets"], inspection["assets"])
            self.assertEqual(value["expectedBundle"], bundle)
            self.assertRegex(value["challenge"], r"^[0-9a-f]{64}$")
            self.assertEqual(value["expiresAtUnix"] - value["issuedAtUnix"], 7200)
            self.assertEqual(output.read_bytes(), gate.canonical_json(value))
            self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o400)


class SignatureReceiptTests(unittest.TestCase):
    def test_guest_signature_receipt_requires_snapshot_tool_and_exact_inputs(self) -> None:
        guest = {
            "apt": {
                "installStderrSha256": "1" * 64,
                "installStdoutSha256": "2" * 64,
                "snapshot": gate.APT_SNAPSHOT,
                "sourceSha256": digest(gate.APT_SOURCE.encode("ascii")),
                "updateStderrSha256": "3" * 64,
                "updateStdoutSha256": "4" * 64,
            },
            "archiveSha256": "5" * 64,
            "completedAt": "2026-09-06T00:00:00Z",
            "helperSha256": "6" * 64,
            "minisign": {
                "binary": {
                    "path": "/usr/bin/minisign",
                    "resolvedPath": "/usr/bin/minisign",
                    "sha256": "7" * 64,
                    "size": 42,
                },
                "package": {"architecture": "amd64", "version": "0.11-1"},
            },
            "publicKeySha256": "8" * 64,
            "schema": gate.GUEST_SIGNATURE_SCHEMA,
            "signatureSha256": "9" * 64,
            "verification": {
                "stderrSha256": "a" * 64,
                "stdoutSha256": "b" * 64,
                "verified": True,
            },
        }
        gate.validate_guest_signature(
            guest,
            archive_sha256="5" * 64,
            signature_sha256="9" * 64,
            public_key_sha256="8" * 64,
            helper_sha256="6" * 64,
        )
        for mutation in (
            lambda value: value["apt"].__setitem__("snapshot", "latest"),
            lambda value: value["verification"].__setitem__("verified", False),
            lambda value: value.__setitem__("archiveSha256", "0" * 64),
            lambda value: value["minisign"]["binary"].__setitem__("path", "/tmp/minisign"),
        ):
            changed = json.loads(json.dumps(guest))
            mutation(changed)
            with self.assertRaises(gate.ProvenanceError):
                gate.validate_guest_signature(
                    changed,
                    archive_sha256="5" * 64,
                    signature_sha256="9" * 64,
                    public_key_sha256="8" * 64,
                    helper_sha256="6" * 64,
                )


if __name__ == "__main__":
    unittest.main()
