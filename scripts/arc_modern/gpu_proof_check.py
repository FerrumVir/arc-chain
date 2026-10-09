#!/usr/bin/env python3
"""Exercise gpu-check proof output on the tiny model, without certifying it.

The tiny CPU/GPU golden matches, but it is NOT the published SmolLM3 golden.
Proof output must therefore remain MISMATCH even with a matching challenge.
The wrong-reference case regresses false CLI PASS/exit 0; the error case
documents the current fail-closed, missing-output behavior. No real weights,
network services or Proof Kit integration are involved.
"""

import argparse
import json
import math
import re
import subprocess
from pathlib import Path


TINY_DIGEST = "98cc9928cd019679b729a0983e15b3689e1429efe7ca795be89b05d7eb240918"
PUBLISHED_DIGEST = "3e43f342c00cf3e3be3072e654e9d1c43f547a73b8a4fc6e906b7119e5cb49f2"
RUN_KEYS = {
    "backend", "isa", "verdict", "golden_digest", "challenge_digest",
    "prefill_tok_s", "decode_tok_s", "threads", "vector_projections",
    "adapter", "divergence",
}


def read(path):
    return json.loads(path.read_text(encoding="utf-8"))


def write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def validate_entry(entry, challenge_digest):
    assert set(entry) == RUN_KEYS, entry
    assert entry["backend"] == "gpu-wgpu"
    assert entry["verdict"] == "MISMATCH"
    assert entry["golden_digest"] == TINY_DIGEST != PUBLISHED_DIGEST
    assert entry["challenge_digest"] == challenge_digest
    for key in ("golden_digest", "challenge_digest"):
        assert re.fullmatch(r"[0-9a-f]{64}", entry[key])
    for key in ("isa", "threads", "vector_projections"):
        assert entry[key] is None
    for key in ("prefill_tok_s", "decode_tok_s"):
        value = entry[key]
        assert isinstance(value, (int, float)) and math.isfinite(value)
        assert 0 <= value <= 100_000
    adapter = entry["adapter"]
    assert set(adapter) == {"vendor", "device", "backend", "driver"}
    assert adapter["backend"] in {"vulkan", "metal", "dx12", "gl", None}
    for key in ("vendor", "device", "driver"):
        label = adapter[key]
        assert label is None or re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9 ()@.,+/_-]{0,63}", label)
    assert set(entry["divergence"]) == {"case", "position", "layer", "op"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--package", type=Path, required=True)
    parser.add_argument("--cases", type=Path, required=True)
    parser.add_argument("--golden", type=Path, required=True)
    parser.add_argument("--out-dir", type=Path, required=True)
    args = parser.parse_args()
    out = args.out_dir
    # Each invocation owns fresh files, so stale output cannot pass a check.
    out.mkdir(parents=True, exist_ok=False)
    binary = str(args.binary.resolve())
    assert read(args.golden)["matrix_digest"] == TINY_DIGEST
    challenge = out / "challenge.json"
    write(challenge, {"schema": "arc.modern-cases.v1", "cases": [{
        "id": "challenge", "prompt_tokens": [11, 42], "max_tokens": 2,
        "eos": [], "selection": "argmax",
    }]})
    reference_path = out / "challenge-cpu.json"
    subprocess.run([
        binary, "golden", "--package", str(args.package), "--cases", str(challenge),
        "--kernel", "scalar", "--out", str(reference_path),
    ], check=True)
    reference = read(reference_path)["matrix_digest"]
    wrong = ("0" if reference[0] != "0" else "1") + reference[1:]
    invalid = out / "invalid-challenge.json"
    invalid_doc = read(challenge)
    invalid_doc["cases"][0]["prompt_tokens"] = [4_294_967_295]
    write(invalid, invalid_doc)
    summary = []
    # Exercise the actual CLI with fresh output paths. Only omission is the
    # valid no-proof mode; malformed requests must fail before GPU work.
    for name, extra, valid in [
        ("omitted-proof-flag", [], True),
        ("trailing-proof-flag", ["--proof-run-out"], False),
        ("option-as-proof-value", ["--proof-run-out", "--trace-forwards", "0"], False),
    ]:
        result_path = out / f"{name}.result.json"
        result = subprocess.run([
            binary, "gpu-check", "--package", str(args.package),
            "--cases", str(args.cases), "--golden", str(args.golden),
            "--out", str(result_path), "--self-test-rounds", "1", *extra,
        ], capture_output=True, text=True, check=False)
        (out / f"{name}.stdout.txt").write_text(result.stdout, encoding="utf-8")
        (out / f"{name}.stderr.txt").write_text(result.stderr, encoding="utf-8")
        if valid:
            assert result.returncode == 0, (name, result.stdout, result.stderr)
            report = read(result_path)
            assert report["pass"] is True and report["proof_run"] is None
            assert report["golden"]["match"] is True
            assert report["golden"]["matrix_digest"] == TINY_DIGEST
            assert "gpu-check: PASS" in result.stdout
        else:
            assert result.returncode != 0, (name, result.stdout, result.stderr)
            assert "argument --proof-run-out requires a filename value" in result.stderr
            assert "gpu-check: PASS" not in result.stdout
            assert "self-test:" not in result.stderr
            assert not result_path.exists(), (name, read(result_path))
        summary.append({"case": name, "exit_code": result.returncode,
                        "result_written": result_path.exists(),
                        "regression_passed": True})
    for name, cases, expected in [
        ("matching-challenge-unpublished-golden", challenge, reference),
        ("wrong-challenge-reference", challenge, wrong),
        ("challenge-execution-error", invalid, reference),
    ]:
        result_path = out / f"{name}.result.json"
        entry_path = out / f"{name}.entry.json"
        result = subprocess.run([
            binary, "gpu-check", "--package", str(args.package),
            "--cases", str(args.cases), "--golden", str(args.golden),
            "--out", str(result_path), "--proof-run-out", str(entry_path),
            "--challenge-cases", str(cases), "--reference-challenge-digest", expected,
            "--self-test-rounds", "1",
        ], capture_output=True, text=True, check=False)
        (out / f"{name}.stdout.txt").write_text(result.stdout, encoding="utf-8")
        (out / f"{name}.stderr.txt").write_text(result.stderr, encoding="utf-8")
        assert result.returncode != 0, (name, result.stdout, result.stderr)
        assert "gpu-check: PASS" not in result.stdout, (name, result.stdout)
        if cases == invalid:
            assert "GPU challenge case challenge failed:" in result.stderr, result.stderr
            # Known limitation: challenge execution errors return before either
            # output is written. The exit code and stderr remain authoritative.
            assert not result_path.exists() and not entry_path.exists()
        else:
            report, entry = read(result_path), read(entry_path)
            validate_entry(entry, reference)
            assert report["golden"]["match"] is True
            assert report["golden"]["matrix_digest"] == TINY_DIGEST
            assert report["self_test"]["pass"] is True
            assert report["proof_run"] == entry
            assert report["pass"] is False
            assert "gpu-check: FAIL" in result.stdout
            if expected == wrong:
                assert entry["divergence"]["case"] == "challenge"
                assert "DIFFERS" in result.stderr
        summary.append({"case": name, "exit_code": result.returncode,
                        "result_written": result_path.exists(),
                        "entry_written": entry_path.exists(), "regression_passed": True})
    write(out / "summary.json", summary)
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
