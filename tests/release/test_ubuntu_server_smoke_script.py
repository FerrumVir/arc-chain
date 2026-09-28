from __future__ import annotations

import os
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path


ROOT = Path(__file__).parents[2]
WORKFLOW = ROOT / ".github/workflows/release.yml"
STEP_NAME = "Execute both binaries without a display on supported Ubuntu servers"
PLATFORMS = {
    "linux-x86_64": ("22.04", "24.04", "26.04"),
    "linux-arm64": ("24.04", "26.04"),
}


def run_block() -> str:
    lines = WORKFLOW.read_text(encoding="utf-8").splitlines()
    step = next(i for i, line in enumerate(lines) if line.strip() == f"- name: {STEP_NAME}")
    run = next(i for i in range(step, len(lines)) if lines[i].strip() == "run: |")
    body = []
    for line in lines[run + 1 :]:
        if line.strip() and len(line) - len(line.lstrip()) < 10:
            break
        body.append(line[10:] if line.startswith("          ") else line)
    return "\n".join(body) + "\n"


def container_script(platform: str) -> str:
    block = run_block().replace("${{ matrix.platform }}", platform)
    marker = "# Boot the real headless node"
    lines = block.splitlines()
    start = next(i for i, line in enumerate(lines)
                 if i > next(j for j, item in enumerate(lines) if marker in item)
                 and "docker run --rm" in line)
    end = next(i for i in range(start + 1, len(lines)) if lines[i].strip() == "'")
    docker_call = "\n".join(lines[start : end + 1])
    return textwrap.dedent(docker_call)


class UbuntuServerSmokeScriptTests(unittest.TestCase):
    def test_rendered_workflow_shell_parses(self) -> None:
        for platform in PLATFORMS:
            with self.subTest(platform=platform):
                script = run_block().replace("${{ matrix.platform }}", platform)
                result = subprocess.run(["bash", "-n"], input=script, text=True,
                                        capture_output=True, check=False)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_exact_nested_container_script_parses_for_every_matrix_image(self) -> None:
        for platform, ubuntu_versions in PLATFORMS.items():
            for ubuntu in ubuntu_versions:
                with self.subTest(platform=platform, ubuntu=ubuntu), tempfile.TemporaryDirectory() as temp:
                    capture = Path(temp) / "container-script.sh"
                    wrapper = (
                        "smoke_root=release-smoke/headless-" + platform + "\n"
                        "ubuntu=" + ubuntu + "\n"
                        "docker() { printf '%s' \"${@: -1}\" > \"$CAPTURE_INNER\"; }\n"
                        + container_script(platform)
                    )
                    # The mock docker function captures the real shell argument; no
                    # container, binary, network, or service is executed.
                    env = {**os.environ, "CAPTURE_INNER": str(capture)}
                    result = subprocess.run(["bash", "-euo", "pipefail", "-c", wrapper],
                                            env=env, text=True, capture_output=True, check=False)
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertTrue(capture.is_file(), "mock docker did not capture the container script")
                    inner = capture.read_text(encoding="utf-8")
                    parsed = subprocess.run(["bash", "-n"], input=inner, text=True,
                                             capture_output=True, check=False)
                    self.assertEqual(parsed.returncode, 0, parsed.stderr + "\n" + inner)
                    self.assertIn("/dev/tcp/127.0.0.1/19944", inner)
                    self.assertIn('grep -Eq "\\\"status\\\":\\\"(ok|degraded)\\\""', inner)


if __name__ == "__main__":
    unittest.main()
