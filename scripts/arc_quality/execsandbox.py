"""Run a model-written Python program against its tests, in a throwaway process.

This is the HumanEval/MBPP pass@1 check. It is NOT a security boundary: the
program runs as the current user with a timeout, a fresh temporary directory,
an empty environment and (on Linux) memory, file-size and process limits. Run
it only on disposable machines such as GitHub-hosted runners; the harness
refuses to execute anything unless `--allow-exec` is passed.
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
from pathlib import Path

TIMEOUT_SECONDS = 10.0


def _limits() -> None:  # pragma: no cover - runs in the child
    try:
        import resource

        gib = 1 << 30
        for name, value in (
            ("RLIMIT_AS", 2 * gib),
            ("RLIMIT_FSIZE", 16 << 20),
            ("RLIMIT_NPROC", 64),
            ("RLIMIT_CORE", 0),
        ):
            limit = getattr(resource, name, None)
            if limit is not None:
                try:
                    resource.setrlimit(limit, (value, value))
                except (ValueError, OSError):
                    pass
    except ImportError:
        pass
    os.setsid()


def run_program(program: str, timeout: float = TIMEOUT_SECONDS) -> dict:
    """Execute `program`; passed means exit status 0 within the timeout."""
    with tempfile.TemporaryDirectory(prefix="arc-quality-exec-") as workdir:
        path = Path(workdir) / "candidate.py"
        path.write_text(program, encoding="utf-8")
        env = {"PATH": os.environ.get("PATH", ""), "PYTHONHASHSEED": "0", "PYTHONDONTWRITEBYTECODE": "1"}
        if os.name == "nt":
            env["SYSTEMROOT"] = os.environ.get("SYSTEMROOT", "")
        try:
            done = subprocess.run(
                [sys.executable, "-I", str(path)],
                cwd=workdir,
                env=env,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                timeout=timeout,
                preexec_fn=_limits if os.name == "posix" else None,
            )
        except subprocess.TimeoutExpired:
            return {"passed": False, "status": "timeout"}
        tail = done.stderr.decode("utf-8", "replace").strip().splitlines()[-1:] if done.returncode else []
        return {
            "passed": done.returncode == 0,
            "status": "passed" if done.returncode == 0 else "failed",
            "error": tail[0][:300] if tail else None,
        }
