#!/usr/bin/env python3
"""Adversarial tests for the exact-byte pre-tag artifact handoff."""

from __future__ import annotations

import copy
import hashlib
import io
import json
import os
import shutil
import stat
import struct
import subprocess
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
PACKAGER = REPO_ROOT / "scripts/release/package-pretag-artifact.py"
SELECTOR = REPO_ROOT / "scripts/release/select-pretag-artifacts.py"
MATERIALIZER = REPO_ROOT / "scripts/release/materialize-pretag-artifacts.py"
REPOSITORY = "FerrumVir/arc-chain"
COMMIT = "a" * 40
RUN_ID = 123456
RUN_ATTEMPT = 2
VERSION = "0.8.0"

GROUPS = (
    ("headless", "linux-x86_64", "x86_64-unknown-linux-gnu"),
    ("headless", "linux-arm64", "aarch64-unknown-linux-gnu"),
    ("headless", "macos-arm64", "aarch64-apple-darwin"),
    ("headless", "macos-x86_64", "x86_64-apple-darwin"),
    ("headless", "windows-x86_64", "x86_64-pc-windows-msvc"),
    ("desktop", "linux-x86_64", "x86_64-unknown-linux-gnu"),
    ("desktop", "macos-arm64", "aarch64-apple-darwin"),
    ("desktop", "macos-x86_64", "x86_64-apple-darwin"),
    ("desktop", "windows-x86_64", "x86_64-pc-windows-msvc"),
)

DESKTOP_FILES = {
    "linux-x86_64": (
        "arc-desktop-linux-x86_64.AppImage",
        "arc-desktop-linux-x86_64.AppImage.sig",
        "arc-desktop-linux-x86_64.deb",
        "arc-desktop-linux-x86_64.rpm",
    ),
    "macos-arm64": (
        "arc-desktop-macos-arm64.app.tar.gz",
        "arc-desktop-macos-arm64.app.tar.gz.sig",
        "arc-desktop-macos-arm64.dmg",
    ),
    "macos-x86_64": (
        "arc-desktop-macos-x86_64.app.tar.gz",
        "arc-desktop-macos-x86_64.app.tar.gz.sig",
        "arc-desktop-macos-x86_64.dmg",
    ),
    "windows-x86_64": (
        "arc-desktop-windows-x86_64-setup.exe",
        "arc-desktop-windows-x86_64-setup.exe.sig",
        "arc-desktop-windows-x86_64.msi",
    ),
}


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def files_for(kind: str, platform: str) -> tuple[str, ...]:
    if kind == "desktop":
        return DESKTOP_FILES[platform]
    suffix = ".exe" if platform == "windows-x86_64" else ""
    return (
        f"arc-node-{platform}{suffix}",
        f"arc-cli-{platform}{suffix}",
        "genesis.toml",
    )


def run(command: list[str], *, success: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(command, text=True, capture_output=True, check=False)
    if success and result.returncode != 0:
        raise AssertionError(
            f"command failed ({result.returncode}): {' '.join(command)}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def write_outer(path: Path, entries: list[tuple[str | zipfile.ZipInfo, bytes]]) -> None:
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as outer:
        for name, value in entries:
            outer.writestr(name, value)


class Fixture:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.downloads = root / "downloads"
        self.downloads.mkdir(parents=True)
        self.api_path = root / "artifacts.json"
        self.selection: dict[str, dict[str, dict]] = {}
        artifacts: list[dict] = []

        for offset, (kind, platform, rust_target) in enumerate(GROUPS, start=1):
            group = f"{kind}-{platform}"
            stage = root / "staging" / group
            payload = stage / "payload"
            payload.mkdir(parents=True)
            for name in files_for(kind, platform):
                (payload / name).write_bytes(f"fixture:{group}:{name}\n".encode())

            package = run(
                [
                    "python3",
                    str(PACKAGER),
                    "--kind",
                    kind,
                    "--platform",
                    platform,
                    "--repository",
                    REPOSITORY,
                    "--commit",
                    COMMIT,
                    "--run-id",
                    str(RUN_ID),
                    "--run-attempt",
                    str(RUN_ATTEMPT),
                    "--version",
                    VERSION,
                    "--rust-target",
                    rust_target,
                    "--payload-dir",
                    str(payload),
                    "--stage-dir",
                    str(stage),
                ]
            )
            artifact_name = package.stdout.strip()
            archives = list(stage.glob("*.tar.gz"))
            if len(archives) != 1:
                raise AssertionError(f"expected one inner archive for {group}")
            archive = archives[0]
            artifact_dir = self.downloads / artifact_name
            artifact_dir.mkdir()
            artifact_zip = artifact_dir / "artifact.zip"
            write_outer(
                artifact_zip,
                [
                    (archive.name, archive.read_bytes()),
                    ("SHA256SUMS", (stage / "SHA256SUMS").read_bytes()),
                ],
            )
            artifacts.append(
                {
                    "id": 1000 + offset,
                    "name": artifact_name,
                    "size_in_bytes": artifact_zip.stat().st_size,
                    "digest": f"sha256:{digest(artifact_zip)}",
                    "expired": False,
                    "workflow_run": {"id": RUN_ID},
                }
            )

        self.api_path.write_text(json.dumps({"artifacts": artifacts}), encoding="utf-8")
        selection_path = root / "selection.json"
        run(
            [
                "python3",
                str(SELECTOR),
                "--api-json",
                str(self.api_path),
                "--repository",
                REPOSITORY,
                "--commit",
                COMMIT,
                "--run-id",
                str(RUN_ID),
                "--run-attempt",
                str(RUN_ATTEMPT),
                "--output",
                str(selection_path),
            ]
        )
        self.selection = json.loads(selection_path.read_text(encoding="utf-8"))[
            "artifacts"
        ]
        self.output_number = 0

    def entry(self, kind: str, platform: str) -> dict:
        return self.selection[platform][kind]

    def artifact_dir(self, kind: str, platform: str) -> Path:
        return self.downloads / self.entry(kind, platform)["name"]

    def artifact_zip(self, kind: str, platform: str) -> Path:
        files = list(self.artifact_dir(kind, platform).iterdir())
        if len(files) != 1:
            raise AssertionError("test fixture raw artifact membership changed")
        return files[0]

    def refresh_outer_selection(self, kind: str, platform: str) -> None:
        artifact_zip = self.artifact_zip(kind, platform)
        entry = self.entry(kind, platform)
        entry["digest"] = f"sha256:{digest(artifact_zip)}"
        entry["size_in_bytes"] = artifact_zip.stat().st_size

    def materialize(
        self,
        *,
        only: str | None = None,
        retain_build_metadata: bool = False,
        success: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        self.output_number += 1
        command = [
            "python3",
            str(MATERIALIZER),
            "--downloads-root",
            str(self.downloads),
            "--output-dir",
            str(self.root / f"output-{self.output_number}"),
            "--repository",
            REPOSITORY,
            "--commit",
            COMMIT,
            "--run-id",
            str(RUN_ID),
            "--run-attempt",
            str(RUN_ATTEMPT),
            "--version",
            VERSION,
            "--selection-json",
            json.dumps(self.selection, separators=(",", ":")),
        ]
        if only:
            command.extend(("--only", only))
        if retain_build_metadata:
            command.append("--retain-build-metadata")
        return run(command, success=success)

    def replace_outer(
        self,
        kind: str,
        platform: str,
        entries: list[tuple[str | zipfile.ZipInfo, bytes]],
    ) -> None:
        artifact_zip = self.artifact_zip(kind, platform)
        write_outer(artifact_zip, entries)
        self.refresh_outer_selection(kind, platform)

    def outer_entries(self, kind: str, platform: str) -> dict[str, bytes]:
        with zipfile.ZipFile(self.artifact_zip(kind, platform), "r") as outer:
            return {name: outer.read(name) for name in outer.namelist()}

    def replace_inner(self, kind: str, platform: str, archive_bytes: bytes) -> None:
        entry = self.entry(kind, platform)
        old_dir = self.artifact_dir(kind, platform)
        stem = (
            f"arc-pretag-{kind}-{platform}-{COMMIT}-{RUN_ID}-{RUN_ATTEMPT}"
        )
        archive_name = f"{stem}.tar.gz"
        archive_hash = hashlib.sha256(archive_bytes).hexdigest()
        checksum = "\n".join(
            (
                "# ARC pre-tag artifact v1",
                f"# kind={kind}",
                f"# repository={REPOSITORY}",
                f"# commit={COMMIT}",
                f"# run_id={RUN_ID}",
                f"# run_attempt={RUN_ATTEMPT}",
                f"# platform={platform}",
                f"{archive_hash}  {archive_name}",
                "",
            )
        ).encode()
        new_name = f"{stem}-{archive_hash}"
        new_dir = self.downloads / new_name
        old_dir.rename(new_dir)
        entry["name"] = new_name
        entry["archive_sha256"] = archive_hash
        artifact_zip = self.artifact_zip(kind, platform)
        write_outer(
            artifact_zip,
            [(archive_name, archive_bytes), ("SHA256SUMS", checksum)],
        )
        self.refresh_outer_selection(kind, platform)


def tar_bytes(entries: list[tuple[tarfile.TarInfo, bytes]]) -> bytes:
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as archive:
        for info, value in entries:
            if info.isreg():
                info.size = len(value)
                archive.addfile(info, io.BytesIO(value))
            else:
                archive.addfile(info)
    return output.getvalue()


class PretagArtifactTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="arc-pretag-tests-")
        self.fixture = Fixture(Path(self.temporary.name))

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def assert_materialize_fails(self, contains: str, only: str | None = None) -> None:
        result = self.fixture.materialize(only=only, success=False)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(contains, result.stdout + result.stderr)

    def test_all_nine_groups_round_trip_exact_bytes(self) -> None:
        result = self.fixture.materialize()
        self.assertIn("materialized 9 exact", result.stdout)
        files = [
            path
            for path in (Path(self.temporary.name) / "output-1").rglob("*")
            if path.is_file()
        ]
        self.assertEqual(28, len(files))

    def test_runtime_canary_can_retain_already_verified_build_metadata(self) -> None:
        self.fixture.materialize(
            only="headless:macos-arm64", retain_build_metadata=True
        )
        output = (
            Path(self.temporary.name)
            / "output-1"
            / "headless-macos-arm64"
        )
        metadata = json.loads(
            (output / "BUILD-METADATA.json").read_text(encoding="utf-8")
        )
        self.assertEqual("arc.pretag.artifact.v1", metadata["schema"])
        self.assertEqual(COMMIT, metadata["commit"])
        self.assertEqual("macos-arm64", metadata["platform"])
        self.assertEqual(
            set(files_for("headless", "macos-arm64")), set(metadata["files"])
        )
        receipt_path = output / "MATERIALIZATION-RECEIPT.json"
        receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
        selected = self.fixture.entry("headless", "macos-arm64")
        self.assertEqual("arc.pretag.materialization.v1", receipt["schema"])
        self.assertEqual(COMMIT, receipt["commit"])
        self.assertEqual(RUN_ID, receipt["run_id"])
        self.assertEqual(selected, receipt["artifact"])
        self.assertEqual(digest(output / "BUILD-METADATA.json"), receipt["build_metadata_sha256"])
        self.assertEqual(0o444, stat.S_IMODE(receipt_path.stat().st_mode))

    def test_selected_server_digest_is_checked_against_raw_zip(self) -> None:
        with self.fixture.artifact_zip("headless", "linux-x86_64").open("ab") as handle:
            handle.write(b"tamper")
        self.assert_materialize_fails(
            "does not match the selected artifact.digest",
            "headless:linux-x86_64",
        )

    def test_selection_rejects_missing_group_and_duplicate_id(self) -> None:
        del self.fixture.selection["linux-x86_64"]["desktop"]
        self.assert_materialize_fails("selection groups differ")

        self.tearDown()
        self.setUp()
        self.fixture.entry("desktop", "linux-x86_64")["id"] = self.fixture.entry(
            "headless", "linux-x86_64"
        )["id"]
        self.assert_materialize_fails("selection reuses artifact ID")

    def test_selector_rejects_missing_duplicate_expired_and_unknown_artifacts(self) -> None:
        base = json.loads(self.fixture.api_path.read_text(encoding="utf-8"))["artifacts"]
        cases: list[tuple[str, list[dict], str]] = []
        cases.append(("missing", base[1:], "found 0"))
        duplicate = copy.deepcopy(base)
        copy_value = copy.deepcopy(base[0])
        copy_value["id"] = 9999
        duplicate.append(copy_value)
        cases.append(("duplicate", duplicate, "found 2"))
        expired = copy.deepcopy(base)
        expired[0]["expired"] = True
        cases.append(("expired", expired, "expired or has unknown expiry"))
        unknown = copy.deepcopy(base)
        unknown.append(
            {
                "id": 9998,
                "name": (
                    f"arc-pretag-unknown-linux-x86_64-{COMMIT}-"
                    f"{RUN_ID}-{RUN_ATTEMPT}-{'b' * 64}"
                ),
                "size_in_bytes": 1,
                "digest": f"sha256:{'c' * 64}",
                "expired": False,
            }
        )
        cases.append(("unknown", unknown, "unexpected current-attempt"))

        for name, artifacts, expected in cases:
            with self.subTest(name=name):
                api_path = Path(self.temporary.name) / f"{name}.json"
                api_path.write_text(json.dumps({"artifacts": artifacts}), encoding="utf-8")
                result = run(
                    [
                        "python3",
                        str(SELECTOR),
                        "--api-json",
                        str(api_path),
                        "--repository",
                        REPOSITORY,
                        "--commit",
                        COMMIT,
                        "--run-id",
                        str(RUN_ID),
                        "--run-attempt",
                        str(RUN_ATTEMPT),
                    ],
                    success=False,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, result.stdout + result.stderr)

    def test_outer_zip_rejects_traversal_absolute_unknown_and_duplicate(self) -> None:
        cases = (
            ("traversal", "../SHA256SUMS", "unsafe"),
            ("absolute", "/SHA256SUMS", "unsafe"),
            ("windows", "..\\SHA256SUMS", "unsafe"),
            ("unknown", "UNKNOWN", "membership differs"),
        )
        for name, replacement, expected in cases:
            with self.subTest(name=name):
                self.tearDown()
                self.setUp()
                entries = self.fixture.outer_entries("headless", "linux-x86_64")
                archive_name = next(key for key in entries if key.endswith(".tar.gz"))
                self.fixture.replace_outer(
                    "headless",
                    "linux-x86_64",
                    [
                        (archive_name, entries[archive_name]),
                        (replacement, entries["SHA256SUMS"]),
                    ],
                )
                self.assert_materialize_fails(expected, "headless:linux-x86_64")

        self.tearDown()
        self.setUp()
        entries = self.fixture.outer_entries("headless", "linux-x86_64")
        self.fixture.replace_outer(
            "headless",
            "linux-x86_64",
            [
                ("SHA256SUMS", entries["SHA256SUMS"]),
                ("SHA256SUMS", entries["SHA256SUMS"]),
            ],
        )
        self.assert_materialize_fails("duplicate entry", "headless:linux-x86_64")

    def test_outer_zip_rejects_encrypted_and_symlink_entries(self) -> None:
        artifact_zip = self.fixture.artifact_zip("headless", "linux-x86_64")
        raw = bytearray(artifact_zip.read_bytes())
        for signature, flag_offset in ((b"PK\x03\x04", 6), (b"PK\x01\x02", 8)):
            start = 0
            while True:
                start = raw.find(signature, start)
                if start < 0:
                    break
                flags = struct.unpack_from("<H", raw, start + flag_offset)[0]
                struct.pack_into("<H", raw, start + flag_offset, flags | 0x1)
                start += 4
        artifact_zip.write_bytes(raw)
        self.fixture.refresh_outer_selection("headless", "linux-x86_64")
        self.assert_materialize_fails("encrypted", "headless:linux-x86_64")

        self.tearDown()
        self.setUp()
        entries = self.fixture.outer_entries("headless", "linux-x86_64")
        archive_name = next(key for key in entries if key.endswith(".tar.gz"))
        symlink = zipfile.ZipInfo("SHA256SUMS")
        symlink.create_system = 3
        symlink.external_attr = 0o120777 << 16
        self.fixture.replace_outer(
            "headless",
            "linux-x86_64",
            [
                (archive_name, entries[archive_name]),
                (symlink, entries["SHA256SUMS"]),
            ],
        )
        self.assert_materialize_fails("non-regular", "headless:linux-x86_64")

    def test_outer_zip_rejects_declared_decompression_bomb(self) -> None:
        artifact_zip = self.fixture.artifact_zip("headless", "linux-x86_64")
        raw = bytearray(artifact_zip.read_bytes())
        central = raw.find(b"PK\x01\x02")
        self.assertGreaterEqual(central, 0)
        struct.pack_into("<I", raw, central + 24, 512 * 1024 * 1024)
        artifact_zip.write_bytes(raw)
        self.fixture.refresh_outer_selection("headless", "linux-x86_64")
        self.assert_materialize_fails("expansion bound", "headless:linux-x86_64")

    def test_inner_tar_rejects_traversal_symlink_unknown_and_duplicate(self) -> None:
        cases: list[tuple[str, list[tuple[tarfile.TarInfo, bytes]], str]] = []
        traversal = tarfile.TarInfo("../arc-node-linux-x86_64")
        cases.append(("traversal", [(traversal, b"x")], "unsafe path"))
        symlink = tarfile.TarInfo("arc-node-linux-x86_64")
        symlink.type = tarfile.SYMTYPE
        symlink.linkname = "elsewhere"
        cases.append(("symlink", [(symlink, b"")], "non-regular member"))
        unknown = tarfile.TarInfo("UNKNOWN")
        cases.append(("unknown", [(unknown, b"x")], "membership differs"))
        duplicate_a = tarfile.TarInfo("arc-node-linux-x86_64")
        duplicate_b = tarfile.TarInfo("arc-node-linux-x86_64")
        cases.append(
            (
                "duplicate",
                [(duplicate_a, b"a"), (duplicate_b, b"b")],
                "duplicate member",
            )
        )

        for name, members, expected in cases:
            with self.subTest(name=name):
                self.tearDown()
                self.setUp()
                self.fixture.replace_inner(
                    "headless", "linux-x86_64", tar_bytes(members)
                )
                self.assert_materialize_fails(expected, "headless:linux-x86_64")

    def test_inner_tar_rejects_decompression_bomb(self) -> None:
        bomb_root = Path(self.temporary.name) / "bomb"
        bomb_root.mkdir()
        required = (*files_for("headless", "linux-x86_64"), "BUILD-METADATA.json")
        for name in required:
            path = bomb_root / name
            with path.open("wb") as handle:
                if name == "arc-node-linux-x86_64":
                    handle.truncate(70 * 1024 * 1024)
                else:
                    handle.write(b"x")
        archive_path = bomb_root / "bomb.tar.gz"
        with tarfile.open(archive_path, "w:gz") as archive:
            for name in required:
                archive.add(bomb_root / name, arcname=name, recursive=False)
        self.fixture.replace_inner(
            "headless", "linux-x86_64", archive_path.read_bytes()
        )
        self.assert_materialize_fails("expansion bound", "headless:linux-x86_64")

    def test_inner_payload_hash_is_enforced_after_both_archive_digests(self) -> None:
        outer = self.fixture.outer_entries("headless", "linux-x86_64")
        archive_name = next(key for key in outer if key.endswith(".tar.gz"))
        original = io.BytesIO(outer[archive_name])
        values: list[tuple[tarfile.TarInfo, bytes]] = []
        with tarfile.open(fileobj=original, mode="r:gz") as archive:
            for member in archive.getmembers():
                extracted = archive.extractfile(member)
                value = extracted.read() if extracted is not None else b""
                if member.name == "arc-node-linux-x86_64":
                    value += b"tamper"
                replacement = tarfile.TarInfo(member.name)
                replacement.mode = member.mode
                values.append((replacement, value))
        self.fixture.replace_inner(
            "headless", "linux-x86_64", tar_bytes(values)
        )
        self.assert_materialize_fails("payload hash mismatch", "headless:linux-x86_64")

    def test_packager_rejects_wrong_target_and_extra_payload(self) -> None:
        stage = Path(self.temporary.name) / "bad-package"
        payload = stage / "payload"
        payload.mkdir(parents=True)
        for name in files_for("headless", "linux-x86_64"):
            (payload / name).write_bytes(b"x")
        command = [
            "python3",
            str(PACKAGER),
            "--kind",
            "headless",
            "--platform",
            "linux-x86_64",
            "--repository",
            REPOSITORY,
            "--commit",
            COMMIT,
            "--run-id",
            str(RUN_ID),
            "--run-attempt",
            str(RUN_ATTEMPT),
            "--version",
            VERSION,
            "--rust-target",
            "aarch64-unknown-linux-gnu",
            "--payload-dir",
            str(payload),
            "--stage-dir",
            str(stage),
        ]
        result = run(command, success=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires Rust target", result.stdout + result.stderr)

        command[command.index("aarch64-unknown-linux-gnu")] = (
            "x86_64-unknown-linux-gnu"
        )
        (payload / "unexpected").write_bytes(b"x")
        result = run(command, success=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("payload membership differs", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
