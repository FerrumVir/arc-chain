from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).parents[2]
WORKFLOW = ROOT / ".github/workflows/release.yml"


def version_check_shell() -> str:
    text = WORKFLOW.read_text()
    start = text.index('          WORKSPACE_VERSION=')
    end_marker = "          done\n"
    end = text.index(end_marker, start) + len(end_marker)
    return "\n".join(line[10:] if line.startswith("          ") else line for line in text[start:end].splitlines())


class ReleaseWorkflowVersionCheckTests(unittest.TestCase):
    def run_check(self, root: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", "-euo", "pipefail", "-c", version_check_shell()],
            cwd=root,
            env={**os.environ, "VERSION": "0.8.5", "TAG": "v0.8.5"},
            text=True,
            capture_output=True,
            check=False,
        )

    def test_actual_workflow_version_block_passes_repo_manifests(self) -> None:
        result = self.run_check(ROOT)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_actual_workflow_version_block_rejects_mismatch_fixture(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            fixture = Path(temp)
            for relative in (
                "Cargo.toml",
                "desktop/src-tauri/Cargo.toml",
                "desktop/src-tauri/tauri.conf.json",
                "desktop/package.json",
                "desktop/package-lock.json",
            ):
                destination = fixture / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(ROOT / relative, destination)
            package_json = fixture / "desktop/package.json"
            package = json.loads(package_json.read_text())
            # Previously published v0.8.0 bytes cannot satisfy the v0.8.5 tag.
            package["version"] = "0.8.0"
            package_json.write_text(json.dumps(package))

            result = self.run_check(fixture)
            self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
            self.assertIn("desktop-npm version is 0.8.0", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
