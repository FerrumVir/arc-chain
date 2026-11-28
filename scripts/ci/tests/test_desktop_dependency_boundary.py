"""Tests for scripts/ci/desktop_dependency_boundary.py. Recorded synthetic cargo tree output: no cargo, no network."""
from __future__ import annotations

import importlib.util
import json
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
SCRIPT = REPO / "scripts" / "ci" / "desktop_dependency_boundary.py"
POLICY = json.loads((REPO / "scripts" / "ci" / "desktop-dependency-boundary.json").read_text(encoding="utf-8"))
spec = importlib.util.spec_from_file_location("desktop_dependency_boundary", SCRIPT)
ddb = importlib.util.module_from_spec(spec)
assert spec.loader is not None
sys.modules["desktop_dependency_boundary"] = ddb
spec.loader.exec_module(ddb)

DESKTOP_BASE = ["arc-desktop v0.8.16", "tauri v2.11.0", "reqwest v0.12.23", "rustls v0.23.31", "tokio v1.47.1"]
SIDECAR_BASE = ["arc-node v0.8.16", "arc-net v0.8.16", "quinn v0.11.11", "quinn-proto v0.11.18", "tokio v1.47.1"]


def record(tmp: Path, desktop_extra=(), sidecar_drop=()) -> Path:
    for graph in POLICY["graphs"]:
        for target in graph["targets"]:
            if graph["id"] == "desktop":
                lines = DESKTOP_BASE + list(desktop_extra)
            else:
                lines = [l for l in SIDECAR_BASE if l.split()[0] not in sidecar_drop]
            # cargo tree repeats shared crates with a (*) marker; the parser must cope.
            (tmp / f"{graph['id']}-{target}.txt").write_text("\n".join(lines + [lines[-1] + " (*)", ""]), encoding="utf-8")
    return tmp


def run(recorded: Path, policy: Path | None = None) -> subprocess.CompletedProcess:
    args = [sys.executable, str(SCRIPT), "--recorded", str(recorded)]
    if policy:
        args += ["--policy", str(policy)]
    return subprocess.run(args, capture_output=True, text=True)


class Boundary(unittest.TestCase):
    def test_current_boundary_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = run(record(Path(tmp)))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("every shipped graph matches the policy", result.stdout)

    def test_desktop_gaining_quic_fails_on_every_desktop_target(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = run(record(Path(tmp), desktop_extra=["quinn v0.11.11", "quinn-proto v0.11.18"]))
            self.assertEqual(result.returncode, 1)
            for target in POLICY["graphs"][0]["targets"]:
                self.assertIn(f"desktop {target}: now links quinn-proto", result.stderr)
            self.assertNotIn("sidecar", result.stderr)

    def test_sidecar_losing_quic_fails_too(self):
        # The policy also pins what the installed product does link, so "the desktop doesn't include it" can't
        # silently become "the product doesn't include it" or the reverse.
        with tempfile.TemporaryDirectory() as tmp:
            result = run(record(Path(tmp), sidecar_drop={"quinn", "quinn-proto"}))
            self.assertEqual(result.returncode, 1)
            self.assertIn("sidecar x86_64-pc-windows-msvc: no longer links quinn-proto", result.stderr)

    def test_missing_or_unreadable_output_is_an_error_not_a_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(run(Path(tmp)).returncode, 2, "no recorded files")
            rec = record(Path(tmp))
            (rec / "desktop-x86_64-apple-darwin.txt").write_text("garbage\n", encoding="utf-8")
            result = run(rec)
            self.assertEqual(result.returncode, 1)
            self.assertIn("empty dependency graph", result.stderr)

    def test_parser(self):
        self.assertEqual(ddb.crate_names("quinn-proto v0.11.18\ntokio v1.47.1 (*)\n\nnot a line\n"), {"quinn-proto", "tokio"})


class PolicyMatchesRelease(unittest.TestCase):
    """The policy's targets are the ones the release actually builds."""

    def setUp(self):
        self.workflow = (REPO / ".github" / "workflows" / "release-signing-preflight.yml").read_text(encoding="utf-8")
        self.graphs = {g["id"]: g for g in POLICY["graphs"]}

    def test_desktop_targets(self):
        shipped = set(re.findall(r"^\s+rust-target: (\S+)$", self.workflow, re.M))
        self.assertTrue(shipped)
        self.assertEqual(set(self.graphs["desktop"]["targets"]), shipped)

    def test_sidecar_targets_and_features(self):
        shipped = set(re.findall(r"^\s+target: ([a-z0-9_]+-[a-z0-9_-]+)$", self.workflow, re.M))
        self.assertTrue(shipped)
        self.assertEqual(set(self.graphs["sidecar"]["targets"]), shipped)
        self.assertIn("cargo build --release --locked -p arc-node --features candle", self.workflow)
        self.assertEqual(self.graphs["sidecar"]["cargo_args"], ["--features", "candle"])

    def test_desktop_default_features_are_what_ships(self):
        manifest = (REPO / "desktop" / "src-tauri" / "Cargo.toml").read_text(encoding="utf-8")
        self.assertIn('default = ["custom-protocol"]', manifest)
        self.assertEqual(self.graphs["desktop"]["cargo_args"], [])


if __name__ == "__main__":
    unittest.main()
