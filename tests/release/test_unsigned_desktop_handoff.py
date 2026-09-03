#!/usr/bin/env python3
"""Hermetic tests for the unsigned desktop build/signing-runner handoff."""

from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import subprocess
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
HELPER = REPO_ROOT / "scripts" / "release" / "unsigned-desktop-handoff.py"
REPOSITORY = "FerrumVir/arc-chain"
COMMIT = "0123456789abcdef0123456789abcdef01234567"
RUN_ID = 12345
RUN_ATTEMPT = 2
VERSION = "0.8.0"
TARGETS = {
    "linux-x86_64": (
        "x86_64-unknown-linux-gnu",
        (
            "arc-desktop-linux-x86_64.AppImage",
            "arc-desktop-linux-x86_64.deb",
            "arc-desktop-linux-x86_64.rpm",
        ),
    ),
    "macos-arm64": (
        "aarch64-apple-darwin",
        (
            "arc-desktop-macos-arm64.app.tar.gz",
            "arc-desktop-macos-arm64.dmg",
        ),
    ),
    "macos-x86_64": (
        "x86_64-apple-darwin",
        (
            "arc-desktop-macos-x86_64.app.tar.gz",
            "arc-desktop-macos-x86_64.dmg",
        ),
    ),
    "windows-x86_64": (
        "x86_64-pc-windows-msvc",
        (
            "arc-desktop-windows-x86_64-setup.exe",
            "arc-desktop-windows-x86_64.msi",
        ),
    ),
}

spec = importlib.util.spec_from_file_location("unsigned_handoff", HELPER)
assert spec is not None and spec.loader is not None
HANDOFF = importlib.util.module_from_spec(spec)
spec.loader.exec_module(HANDOFF)


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class UnsignedDesktopHandoffTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(
            prefix="arc-unsigned-desktop-handoff-test."
        )
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def common(self, platform: str) -> list[str]:
        target, _ = TARGETS[platform]
        return [
            "--repository",
            REPOSITORY,
            "--commit",
            COMMIT,
            "--run-id",
            str(RUN_ID),
            "--run-attempt",
            str(RUN_ATTEMPT),
            "--platform",
            platform,
            "--rust-target",
            target,
            "--version",
            VERSION,
        ]

    def invoke(
        self, command: str, platform: str, *extra: str, succeeds: bool = True
    ) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            ["python3", str(HELPER), command, *self.common(platform), *extra],
            cwd=REPO_ROOT,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if succeeds and result.returncode != 0:
            self.fail(f"helper failed: {result.stderr}")
        if not succeeds and result.returncode == 0:
            self.fail("helper unexpectedly accepted an invalid handoff")
        return result

    def packaged(self, platform: str) -> dict[str, object]:
        root = self.root / platform
        payload = root / "payload"
        payload.mkdir(parents=True)
        payload_bytes = {}
        for index, name in enumerate(TARGETS[platform][1], start=1):
            content = f"ARC {platform} payload {index}\n".encode()
            (payload / name).write_bytes(content)
            payload_bytes[name] = content
        stage = root / "stage"
        package_output = root / "package.out"
        self.invoke(
            "package",
            platform,
            "--payload-dir",
            str(payload),
            "--stage-dir",
            str(stage),
            "--github-output",
            str(package_output),
        )
        outputs = dict(
            line.split("=", 1)
            for line in package_output.read_text(encoding="utf-8").splitlines()
        )
        raw_root = root / "raw"
        artifact_root = raw_root / outputs["artifact_name"]
        artifact_root.mkdir(parents=True)
        actions_zip = artifact_root / "artifact.zip"
        with zipfile.ZipFile(actions_zip, "w", compression=zipfile.ZIP_STORED) as archive:
            archive.write(stage / "SHA256SUMS", "SHA256SUMS")
            archive.write(stage / outputs["archive_name"], outputs["archive_name"])
        artifact = {
            "id": 987654,
            "name": outputs["artifact_name"],
            "digest": f"sha256:{digest(actions_zip)}",
            "size_in_bytes": actions_zip.stat().st_size,
            "expired": False,
            "workflow_run": {"id": RUN_ID},
        }
        api = root / "api.json"
        api.write_text(json.dumps({"artifacts": [artifact]}), encoding="utf-8")
        selection_output = root / "selection.out"
        self.invoke(
            "select",
            platform,
            "--api-json",
            str(api),
            "--github-output",
            str(selection_output),
        )
        selection = dict(
            line.split("=", 1)
            for line in selection_output.read_text(encoding="utf-8").splitlines()
        )
        return {
            "root": root,
            "raw_root": raw_root,
            "actions_zip": actions_zip,
            "artifact": artifact,
            "api": api,
            "artifact_json": selection["artifact_json"],
            "selection": selection,
            "payload_bytes": payload_bytes,
        }

    def materialized(self, platform: str) -> tuple[dict[str, object], Path]:
        fixture = self.packaged(platform)
        output = Path(fixture["root"]) / "materialized"
        self.invoke(
            "materialize",
            platform,
            "--artifact-json",
            str(fixture["artifact_json"]),
            "--expected-artifact-id",
            str(fixture["selection"]["artifact_id"]),
            "--expected-artifact-digest",
            str(fixture["selection"]["artifact_digest"]),
            "--expected-archive-sha256",
            str(fixture["selection"]["archive_sha"]),
            "--downloads-root",
            str(fixture["raw_root"]),
            "--output-dir",
            str(output),
        )
        return fixture, output

    def test_all_four_platforms_round_trip_exact_normalized_payloads(self) -> None:
        for platform, (_, payload_names) in TARGETS.items():
            with self.subTest(platform=platform):
                fixture, output = self.materialized(platform)
                for name in payload_names:
                    self.assertEqual(
                        (output / name).read_bytes(), fixture["payload_bytes"][name]
                    )
                    self.assertEqual((output / name).stat().st_mode & 0o777, 0o400)
                receipt = json.loads(
                    (output / "HANDOFF-RECEIPT.json").read_text(encoding="utf-8")
                )
                self.assertEqual(receipt["artifact"]["id"], 987654)
                self.assertEqual(receipt["commit"], COMMIT)

    def test_selection_rejects_duplicate_expired_and_cross_run_artifacts(self) -> None:
        fixture = self.packaged("linux-x86_64")
        original = dict(fixture["artifact"])
        for mutation in (
            [original, dict(original)],
            [{**original, "expired": True}],
            [{**original, "workflow_run": {"id": RUN_ID + 1}}],
        ):
            with self.subTest(mutation=mutation):
                api = Path(fixture["root"]) / f"invalid-{len(mutation)}-{mutation[0].get('expired')}.json"
                api.write_text(json.dumps({"artifacts": mutation}), encoding="utf-8")
                self.invoke(
                    "select",
                    "linux-x86_64",
                    "--api-json",
                    str(api),
                    succeeds=False,
                )

        for old, new in (
            (COMMIT, "f" * 40),
            (f"-{RUN_ID}-{RUN_ATTEMPT}-", f"-{RUN_ID}-{RUN_ATTEMPT + 1}-"),
        ):
            cross_boundary = {**original, "name": str(original["name"]).replace(old, new)}
            api = Path(fixture["root"]) / f"cross-boundary-{new[-3:]}.json"
            api.write_text(json.dumps({"artifacts": [cross_boundary]}), encoding="utf-8")
            self.invoke(
                "select",
                "linux-x86_64",
                "--api-json",
                str(api),
                succeeds=False,
            )

    def test_materialization_rejects_substituted_id_digest_and_archive_hash(self) -> None:
        fixture = self.packaged("linux-x86_64")
        selection = fixture["selection"]
        for field, value in (
            ("artifact_id", "987655"),
            ("artifact_digest", f"sha256:{'f' * 64}"),
            ("archive_sha", "f" * 64),
        ):
            with self.subTest(field=field):
                arguments = {
                    "artifact_id": str(selection["artifact_id"]),
                    "artifact_digest": str(selection["artifact_digest"]),
                    "archive_sha": str(selection["archive_sha"]),
                }
                arguments[field] = value
                self.invoke(
                    "materialize",
                    "linux-x86_64",
                    "--artifact-json",
                    str(fixture["artifact_json"]),
                    "--expected-artifact-id",
                    arguments["artifact_id"],
                    "--expected-artifact-digest",
                    arguments["artifact_digest"],
                    "--expected-archive-sha256",
                    arguments["archive_sha"],
                    "--downloads-root",
                    str(fixture["raw_root"]),
                    "--output-dir",
                    str(Path(fixture["root"]) / f"substituted-{field}"),
                    succeeds=False,
                )

    def test_materialization_rejects_tampered_raw_actions_zip(self) -> None:
        fixture = self.packaged("linux-x86_64")
        with Path(fixture["actions_zip"]).open("ab") as handle:
            handle.write(b"tamper")
        result = self.invoke(
            "materialize",
            "linux-x86_64",
            "--artifact-json",
            str(fixture["artifact_json"]),
            "--expected-artifact-id",
            str(fixture["selection"]["artifact_id"]),
            "--expected-artifact-digest",
            str(fixture["selection"]["artifact_digest"]),
            "--expected-archive-sha256",
            str(fixture["selection"]["archive_sha"]),
            "--downloads-root",
            str(fixture["raw_root"]),
            "--output-dir",
            str(Path(fixture["root"]) / "tampered-output"),
            succeeds=False,
        )
        self.assertIn("Actions ZIP size differs", result.stderr)

    def test_materialization_rejects_unsafe_outer_members_even_with_matching_digest(self) -> None:
        fixture = self.packaged("linux-x86_64")
        actions_zip = Path(fixture["actions_zip"])
        with zipfile.ZipFile(actions_zip, "a", compression=zipfile.ZIP_STORED) as archive:
            archive.writestr("../escape", b"blocked")
        selection = json.loads(str(fixture["artifact_json"]))
        selection["digest"] = f"sha256:{digest(actions_zip)}"
        selection["size_in_bytes"] = actions_zip.stat().st_size
        result = self.invoke(
            "materialize",
            "linux-x86_64",
            "--artifact-json",
            json.dumps(selection, sort_keys=True, separators=(",", ":")),
            "--expected-artifact-id",
            str(fixture["selection"]["artifact_id"]),
            "--expected-artifact-digest",
            f"sha256:{digest(actions_zip)}",
            "--expected-archive-sha256",
            str(fixture["selection"]["archive_sha"]),
            "--downloads-root",
            str(fixture["raw_root"]),
            "--output-dir",
            str(Path(fixture["root"]) / "unsafe-output"),
            succeeds=False,
        )
        self.assertIn("entry count differs", result.stderr)

    def test_package_rejects_symlink_missing_extra_and_cross_platform_payloads(self) -> None:
        root = self.root / "symlink"
        payload = root / "payload"
        payload.mkdir(parents=True)
        expected = TARGETS["linux-x86_64"][1]
        for name in expected:
            (payload / name).write_bytes(name.encode())
        real_appimage = root / "real-appimage"
        real_appimage.write_bytes(b"appimage")
        (payload / expected[0]).unlink()
        os.symlink(real_appimage, payload / expected[0])
        self.invoke(
            "package",
            "linux-x86_64",
            "--payload-dir",
            str(payload),
            "--stage-dir",
            str(root / "stage-one"),
            succeeds=False,
        )

        (payload / expected[0]).unlink()
        (payload / expected[0]).write_bytes(b"appimage")
        for index, mutation in enumerate((
            (expected[-1], None),
            (None, "extra.bin"),
            (expected[-1], "arc-desktop-windows-x86_64-setup.exe"),
        ), start=2):
            removed, added = mutation
            if removed:
                (payload / removed).unlink()
            if added:
                (payload / added).write_bytes(b"substitution")
            self.invoke(
                "package",
                "linux-x86_64",
                "--payload-dir",
                str(payload),
                "--stage-dir",
                str(root / f"stage-{index}"),
                succeeds=False,
            )
            if added:
                (payload / added).unlink()
            if removed:
                (payload / removed).write_bytes(removed.encode())

    def test_inner_archive_rejects_duplicate_and_materializer_tauri_substitution(self) -> None:
        for hostile_name in (
            "arc-desktop-linux-x86_64.AppImage",
            "node_modules/@tauri-apps/cli/tauri.js",
        ):
            with self.subTest(hostile_name=hostile_name):
                archive_path = self.root / f"hostile-{len(hostile_name)}.tar"
                with tarfile.open(archive_path, "w", format=tarfile.USTAR_FORMAT) as archive:
                    for name in (*TARGETS["linux-x86_64"][1], "BUILD-METADATA.json"):
                        info = tarfile.TarInfo(name)
                        payload = b"fixture"
                        info.size = len(payload)
                        info.mode = (
                            0o600 if name == "BUILD-METADATA.json"
                            else 0o755 if name.endswith(".AppImage") else 0o644
                        )
                        archive.addfile(info, HANDOFF._BytesReader(payload))
                    info = tarfile.TarInfo(hostile_name)
                    info.size = 7
                    info.mode = 0o644
                    archive.addfile(info, HANDOFF._BytesReader(b"hostile"))
                with tarfile.open(archive_path, "r") as archive:
                    with self.assertRaises(SystemExit):
                        HANDOFF.safe_tar_members(archive, "linux-x86_64")

    def test_post_sign_gate_proves_inputs_unchanged_and_only_one_signature(self) -> None:
        fixture, output = self.materialized("linux-x86_64")
        signature = output / "arc-desktop-linux-x86_64.AppImage.sig"
        signature.write_bytes(b"fixture signature")
        self.invoke(
            "verify-signed",
            "linux-x86_64",
            "--workspace",
            str(output),
        )
        for name in TARGETS["linux-x86_64"][1]:
            expected_mode = 0o755 if name.endswith(".AppImage") else 0o644
            self.assertEqual((output / name).stat().st_mode & 0o777, expected_mode)
            self.assertEqual((output / name).read_bytes(), fixture["payload_bytes"][name])

        _, changed = self.materialized("macos-arm64")
        updater = changed / "arc-desktop-macos-arm64.app.tar.gz"
        updater.chmod(0o600)
        updater.write_bytes(b"changed")
        updater.chmod(0o400)
        (changed / "arc-desktop-macos-arm64.app.tar.gz.sig").write_bytes(b"sig")
        self.invoke(
            "verify-signed",
            "macos-arm64",
            "--workspace",
            str(changed),
            succeeds=False,
        )

        _, writable = self.materialized("macos-x86_64")
        (writable / "arc-desktop-macos-x86_64.dmg").chmod(0o600)
        (writable / "arc-desktop-macos-x86_64.app.tar.gz.sig").write_bytes(b"sig")
        result = self.invoke(
            "verify-signed",
            "macos-x86_64",
            "--workspace",
            str(writable),
            succeeds=False,
        )
        self.assertIn("regained permissions", result.stderr)

        _, extra = self.materialized("windows-x86_64")
        (extra / "arc-desktop-windows-x86_64-setup.exe.sig").write_bytes(b"sig")
        (extra / "unexpected.sig").write_bytes(b"extra")
        self.invoke(
            "verify-signed",
            "windows-x86_64",
            "--workspace",
            str(extra),
            succeeds=False,
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
