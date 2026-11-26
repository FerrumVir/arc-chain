#!/usr/bin/env python3
import argparse
import json
import os
from pathlib import Path
import subprocess


EXPECTED_TESTS = {
    "tests::recovery_peer_generation_replays_for_different_absent_validator_compacts_and_restarts",
    "tests::recovery_peer_replay_rejects_unsigned_forged_and_wrong_domain_blocks",
    "tests::recovery_peer_replay_rejects_subquorum_forged_and_wrong_domain_skip_certificates",
}


def run_recorded(command, log, timeout):
    with log.open("x") as output:
        result = subprocess.run(command, stdout=output, stderr=subprocess.STDOUT, timeout=timeout)
    if result.returncode:
        print(log.read_text())
        raise SystemExit(f"Command failed with exit {result.returncode}: {command}")
    return log.read_text()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-dir", required=True, type=Path)
    args = parser.parse_args()
    args.evidence_dir.mkdir(parents=True, exist_ok=False)
    os.environ["CARGO_BUILD_JOBS"] = "1"
    compiled = run_recorded(
        ["cargo", "test", "--locked", "-p", "arc-node", "--bin", "arc-node",
         "--no-run", "--message-format=json-render-diagnostics"],
        args.evidence_dir / "compile.log", 3600,
    )
    executables = set()
    for line in compiled.splitlines():
        if not line.startswith("{"):
            continue
        row = json.loads(line)
        target = row.get("target", {})
        if (row.get("reason") == "compiler-artifact" and target.get("name") == "arc-node"
                and row.get("profile", {}).get("test") and row.get("executable")):
            executables.add(row["executable"])
    if len(executables) != 1:
        raise SystemExit(f"Expected one arc-node test binary, found {len(executables)}")
    binary = executables.pop()
    listing = run_recorded(
        [binary, "recovery_peer_", "--list"], args.evidence_dir / "selectors.log", 30,
    )
    selected = {line.removesuffix(": test") for line in listing.splitlines() if line.endswith(": test")}
    if selected != EXPECTED_TESTS:
        raise SystemExit(f"Recovery peer selectors differ: expected {sorted(EXPECTED_TESTS)}, found {sorted(selected)}")
    for index, test in enumerate(sorted(EXPECTED_TESTS)):
        output = run_recorded(
            [binary, test, "--exact", "--nocapture", "--test-threads=1"],
            args.evidence_dir / f"test-{index}.log", 120,
        )
        if output.count("test result: ok. 1 passed; 0 failed; 0 ignored;") != 1:
            raise SystemExit(f"Test did not prove exactly one non-ignored passing case: {test}")
        print(f"PASS {test}")
    with (args.evidence_dir / "result.json").open("x") as output:
        json.dump({"binary": binary, "passed": sorted(EXPECTED_TESTS)}, output, indent=2)
        output.write("\n")


if __name__ == "__main__":
    main()
