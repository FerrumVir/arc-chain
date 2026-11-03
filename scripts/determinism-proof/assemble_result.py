#!/usr/bin/env python3
"""Assemble one determinism-proof leg's result.json (standard library only).

A leg is one runner, one kernel and one prompt shard. This reads the driver's
run JSON and transcript from --out-dir, computes the SHA-256 of the transcript
bytes with Python's hashlib (independently of the code under test), adds what
only the host can report (CPU model, memory, runner image) and writes
result.json beside them. It always exits 0: the run step's own status fails
the job, and the compare job judges the evidence.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import subprocess
from pathlib import Path

SCHEMA = "arc-determinism-proof-result-v1"
GITHUB_KEYS = (
    "GITHUB_REPOSITORY",
    "GITHUB_RUN_ID",
    "GITHUB_RUN_ATTEMPT",
    "GITHUB_SHA",
    "GITHUB_JOB",
    "RUNNER_OS",
    "RUNNER_ARCH",
    "RUNNER_NAME",
    "ImageOS",
    "ImageVersion",
)


def capture(*command: str) -> str | None:
    try:
        completed = subprocess.run(
            command, capture_output=True, text=True, check=True, timeout=30
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return completed.stdout.strip() or None


def cpu_model() -> str | None:
    system = platform.system()
    if system == "Darwin":
        return capture("sysctl", "-n", "machdep.cpu.brand_string")
    if system == "Linux":
        try:
            text = Path("/proc/cpuinfo").read_text(encoding="utf-8", errors="replace")
        except OSError:
            return None
        for line in text.splitlines():
            key, _, value = line.partition(":")
            if key.strip() in ("model name", "Model"):
                return value.strip()
        return None
    if system == "Windows":
        try:
            import winreg

            with winreg.OpenKey(
                winreg.HKEY_LOCAL_MACHINE,
                r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
            ) as key:
                return str(winreg.QueryValueEx(key, "ProcessorNameString")[0]).strip()
        except OSError:
            return None
    return None


def memory_bytes() -> int | None:
    system = platform.system()
    if system == "Darwin":
        value = capture("sysctl", "-n", "hw.memsize")
        return int(value) if value and value.isdigit() else None
    if system == "Linux":
        try:
            for line in Path("/proc/meminfo").read_text(encoding="utf-8").splitlines():
                if line.startswith("MemTotal:"):
                    return int(line.split()[1]) * 1024
        except (OSError, ValueError, IndexError):
            return None
        return None
    if system == "Windows":
        import ctypes

        class MemoryStatus(ctypes.Structure):
            _fields_ = [
                ("dwLength", ctypes.c_ulong),
                ("dwMemoryLoad", ctypes.c_ulong),
                ("ullTotalPhys", ctypes.c_ulonglong),
                ("ullAvailPhys", ctypes.c_ulonglong),
                ("ullTotalPageFile", ctypes.c_ulonglong),
                ("ullAvailPageFile", ctypes.c_ulonglong),
                ("ullTotalVirtual", ctypes.c_ulonglong),
                ("ullAvailVirtual", ctypes.c_ulonglong),
                ("ullAvailExtendedVirtual", ctypes.c_ulonglong),
            ]

        status = MemoryStatus()
        status.dwLength = ctypes.sizeof(MemoryStatus)
        if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(status)):
            return int(status.ullTotalPhys)
    return None


def only(directory: Path, pattern: str) -> Path | None:
    matches = sorted(directory.glob(pattern))
    return matches[0] if len(matches) == 1 else None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--runner-label", required=True)
    parser.add_argument("--kernel", required=True, choices=("scalar", "simd"))
    parser.add_argument("--shard", default="0/1")
    args = parser.parse_args()

    out_dir: Path = args.out_dir
    out_dir.mkdir(parents=True, exist_ok=True)
    problems: list[str] = []

    run = None
    run_path = only(out_dir, "run-*.json")
    if run_path is None:
        problems.append("no single driver run JSON (the driver did not finish)")
    else:
        try:
            run = json.loads(run_path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            problems.append(f"driver run JSON unreadable: {error}")

    transcript = None
    transcript_path = only(out_dir, "transcript-*.txt")
    if transcript_path is None:
        problems.append("no single transcript file")
    else:
        data = transcript_path.read_bytes()
        lines = data.decode("utf-8", errors="replace").splitlines()
        terminator = lines[-1] if lines else ""
        transcript = {
            "file": transcript_path.name,
            "bytes": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
            "terminator": terminator,
        }
        if terminator != "end" and not terminator.startswith("end-of-shard "):
            problems.append(f"transcript ends with {terminator!r}, not a completion marker")

    if run is not None:
        if run.get("complete") is not True:
            problems.append("the driver reported an incomplete run")
        if run.get("engine_crosscheck_ok") is not True:
            problems.append("the engine API cross-check failed")
        written = (run.get("transcript") or {}).get("bytes")
        if transcript is not None and written != transcript["bytes"]:
            problems.append("transcript size differs from what the driver wrote")

    host = {
        "cpu_model": cpu_model(),
        "memory_bytes": memory_bytes(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": platform.python_version(),
    }
    timing = (run or {}).get("timing") or {}
    whole = args.shard in ("", "0/1")
    summary = {
        "os": os.environ.get("RUNNER_OS") or platform.system(),
        "os_version": host["platform"],
        "cpu_model": host["cpu_model"],
        "kernel_requested": args.kernel,
        "kernel_path": ((run or {}).get("kernel") or {}).get("effective"),
        "decode_tokens_per_second_ci_runner": timing.get("decode_tokens_per_second"),
        "prefill_tokens_per_second_ci_runner": timing.get("prefill_tokens_per_second"),
        "tokens_per_second_note": (
            "GitHub-hosted CI runner measurement (shared virtual machine), forward "
            "passes only; not a product benchmark"
        ),
        "combined_sha256": transcript["sha256"] if transcript and whole else None,
        "shard_transcript_sha256": transcript["sha256"] if transcript and not whole else None,
    }
    result = {
        "schema": SCHEMA,
        "status": "complete" if not problems else "failed",
        "problems": problems,
        "runner_label": args.runner_label,
        "kernel": args.kernel,
        "shard": args.shard,
        "summary": summary,
        "transcript": transcript,
        "host": host,
        "github": {key: os.environ.get(key) for key in GITHUB_KEYS},
        "run": run,
    }
    (out_dir / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")

    sha = transcript["sha256"] if transcript else "none"
    line = (
        f"{args.runner_label} / {args.kernel} / shard {args.shard}: "
        f"{result['status']}; transcript SHA-256 {sha}"
    )
    print(line)
    for problem in problems:
        print(f"  problem: {problem}")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as handle:
            handle.write(f"- {line}\n")
            for problem in problems:
                handle.write(f"  - problem: {problem}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
