#!/usr/bin/env python3
"""Focused tests for create-only legacy updater recovery anchors.

The production helper runs the embedded program as root against
``/etc/systemd/system``.  These tests execute those exact bytes in an isolated
temporary unit directory and never contact systemd or a production host.
"""

from __future__ import annotations

import os
import pathlib
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).with_name("archive-node.sh")
EXPECTED = {
    "arc-node-update.service": (
        b"[Unit]\n"
        b"Description=ARC recovery inert anchor for absent legacy updater service\n"
        b"DefaultDependencies=no\n"
        b"RefuseManualStart=yes\n"
        b"ConditionPathExists=!/dev/null\n\n"
        b"[Service]\nType=oneshot\nExecStart=/usr/bin/false\n"
    ),
    "arc-node-update.timer": (
        b"[Unit]\n"
        b"Description=ARC recovery inert anchor for absent legacy updater timer\n"
        b"DefaultDependencies=no\n"
        b"RefuseManualStart=yes\n"
        b"ConditionPathExists=!/dev/null\n\n"
        b"[Timer]\nOnActiveSec=3153600000s\n"
        b"Unit=arc-node-update.service\n"
    ),
}


def anchor_source() -> str:
    text = SCRIPT.read_text(encoding="utf-8")
    function = text.index("materialize_absent_legacy_updater_anchor()")
    marker = 'python3 - "$unit" "$anchor" <<\'PY\'\n'
    start = text.index(marker, function) + len(marker)
    end = text.index("\nPY\n}", start)
    return text[start:end]


class LegacyUpdaterAnchorTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.shell = SCRIPT.read_text(encoding="utf-8")
        cls.program = anchor_source()
        compile(cls.program, "archive-node-legacy-updater-anchor", "exec")

    def run_anchor(
        self, root: pathlib.Path, unit: str, *, check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        program = root / "anchor.py"
        program.write_text(self.program, encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(program), unit, str(root / unit)],
            check=check,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

    def test_absent_service_and_timer_are_create_only_and_inert(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            for unit, payload in EXPECTED.items():
                self.run_anchor(root, unit)
                target = root / unit
                details = target.lstat()
                self.assertEqual(target.read_bytes(), payload)
                self.assertTrue(stat.S_ISREG(details.st_mode))
                self.assertEqual(stat.S_IMODE(details.st_mode), 0o444)
                self.assertEqual(details.st_nlink, 1)
                first_identity = (details.st_dev, details.st_ino)
                self.run_anchor(root, unit)
                second = target.lstat()
                self.assertEqual((second.st_dev, second.st_ino), first_identity)
                self.assertFalse(
                    (root / f".{unit}.arc-recovery-anchor.partial").exists()
                )

    def test_crash_partials_resume_without_ambiguous_links(self) -> None:
        unit = "arc-node-update.service"
        payload = EXPECTED[unit]
        for partial_payload, mode in (
            (b"", 0o600),
            (payload[:37], 0o600),
            (payload, 0o444),
        ):
            with self.subTest(size=len(partial_payload), mode=oct(mode)):
                with tempfile.TemporaryDirectory() as temporary:
                    root = pathlib.Path(temporary)
                    partial = root / f".{unit}.arc-recovery-anchor.partial"
                    partial.write_bytes(partial_payload)
                    partial.chmod(mode)
                    self.run_anchor(root, unit)
                    target = root / unit
                    self.assertEqual(target.read_bytes(), payload)
                    self.assertEqual(target.lstat().st_nlink, 1)
                    self.assertFalse(partial.exists())

    def test_unowned_partial_shape_fails_closed(self) -> None:
        unit = "arc-node-update.timer"
        payload = EXPECTED[unit]
        cases = (
            (b"not-a-prefix", 0o600),
            (payload, 0o644),
            (payload[:13], 0o444),
        )
        for partial_payload, mode in cases:
            with self.subTest(mode=oct(mode)):
                with tempfile.TemporaryDirectory() as temporary:
                    root = pathlib.Path(temporary)
                    partial = root / f".{unit}.arc-recovery-anchor.partial"
                    partial.write_bytes(partial_payload)
                    partial.chmod(mode)
                    result = self.run_anchor(root, unit, check=False)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertFalse((root / unit).exists())

        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            partial = root / f".{unit}.arc-recovery-anchor.partial"
            partial.write_bytes(payload)
            os.link(partial, root / "second-link")
            partial.chmod(0o444)
            result = self.run_anchor(root, unit, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("partial inode is unsafe", result.stderr)

        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            (root / "partial-target").write_bytes(payload)
            partial = root / f".{unit}.arc-recovery-anchor.partial"
            partial.symlink_to("partial-target")
            result = self.run_anchor(root, unit, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("partial inode is unsafe", result.stderr)

    def test_existing_target_wrong_mode_and_hardlink_fail_closed(self) -> None:
        unit = "arc-node-update.service"
        payload = EXPECTED[unit]
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            target = root / unit
            target.write_bytes(payload)
            target.chmod(0o644)
            result = self.run_anchor(root, unit, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("anchor inode is unsafe", result.stderr)
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            target = root / unit
            target.write_bytes(payload)
            target.chmod(0o444)
            os.link(target, root / "second-link")
            result = self.run_anchor(root, unit, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("anchor inode is unsafe", result.stderr)

    def test_existing_different_bytes_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            target = root / "arc-node-update.service"
            target.write_bytes(b"[Unit]\nDescription=unexpected\n")
            target.chmod(0o444)
            result = self.run_anchor(root, target.name, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("anchor bytes differ", result.stderr)

    def test_symlink_and_unreviewed_name_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            (root / "target").write_bytes(b"x")
            (root / "arc-node-update.timer").symlink_to("target")
            symlink = self.run_anchor(root, "arc-node-update.timer", check=False)
            self.assertNotEqual(symlink.returncode, 0)
            self.assertIn("anchor inode is unsafe", symlink.stderr)
            unknown = self.run_anchor(root, "arc-other.service", check=False)
            self.assertNotEqual(unknown.returncode, 0)
            self.assertIn("target is not exact", unknown.stderr)

    def test_anchor_staging_precedes_disable_and_persistent_barrier_verification(self) -> None:
        stage_start = self.shell.index("stage_recovery_barrier()")
        stage_end = self.shell.index("\nstage_prefreeze_runtime_safety()", stage_start)
        stage = self.shell[stage_start:stage_end]
        materialize = stage.index("materialize_absent_legacy_updater_anchors")
        disable = stage.index('disable_and_verify_unit "$other_unit"')
        persistent = stage.index("persist_legacy_restart_fence_files")
        merged = stage.index(
            'verify_merged_legacy_start_barrier_config "$unit"'
        )
        self.assertLess(materialize, disable)
        self.assertLess(disable, persistent)
        self.assertLess(persistent, merged)
        self.assertIn(
            'ConditionPathExists=!/dev/null',
            self.program,
        )
        self.assertIn(
            'systemctl show "$unit" --property=LoadState --value',
            self.shell,
        )

    @unittest.skipUnless(shutil.which("systemd-analyze"), "systemd-analyze unavailable")
    def test_exact_anchor_units_pass_systemd_verification(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            for unit in EXPECTED:
                self.run_anchor(root, unit)
            environment = dict(os.environ)
            environment["SYSTEMD_UNIT_PATH"] = ":".join(
                (
                    str(root),
                    "/usr/local/lib/systemd/system",
                    "/usr/lib/systemd/system",
                    "/lib/systemd/system",
                )
            )
            subprocess.run(
                [
                    shutil.which("systemd-analyze") or "systemd-analyze",
                    "verify",
                    "arc-node-update.service",
                    "arc-node-update.timer",
                ],
                check=True,
                cwd=root,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )


if __name__ == "__main__":
    unittest.main()
