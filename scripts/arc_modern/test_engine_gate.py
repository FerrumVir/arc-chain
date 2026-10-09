#!/usr/bin/env python3
"""Tests of engine_gate.py check: a run passes only with identical digests and
sufficient dispatch evidence (ARC-54 finding 4: a missing, zero or malformed
census used to pass). Standard library only:

    python3 scripts/arc_modern/test_engine_gate.py -v
"""

from __future__ import annotations

import copy
import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import engine_gate  # noqa: E402

GOLDEN = {
    "package": {"sha256": "ab" * 32},
    "matrix_digest": "cd" * 32,
    "cases": [
        {
            "id": "c1",
            "tokens": [1, 2],
            "output_hash": "ee" * 32,
            "logits_digest": "ff" * 32,
            "logits_hashes": ["01" * 32, "02" * 32, "03" * 32],
            "prompt_tokens": [7],
        }
    ],
}

CENSUS = {
    "attempted": 10,
    "accepted": 10,
    "refused_unavailable": 0,
    "refused_shape": 0,
    "refused_inner_dim_above_i32_bound": 0,
    "refused_activation_out_of_domain": 0,
    "refused_scale_multiply_would_overflow": 0,
}


def run(spec: str = "fast:avx2", engine: str = "fast", kernel_path: str = "avx2") -> dict:
    out = {
        "schema": "arc.modern-run.v1",
        "package": copy.deepcopy(GOLDEN["package"]),
        "matrix_digest": GOLDEN["matrix_digest"],
        "cases": copy.deepcopy(GOLDEN["cases"]),
        "spec": spec,
        "engine": engine,
        "kernel_path": kernel_path,
        "threads": 4,
        "census": dict(CENSUS),
        "attention_census": {"simd_heads": 64, "reference_heads": 0},
        "timing": {"decode_tok_s": 1.0, "prefill_tok_s": 2.0},
    }
    if engine != "fast" or kernel_path in ("scalar", "legacy"):
        out["attention_census"] = None
    if kernel_path == "scalar":
        out["census"] = dict(CENSUS, attempted=0, accepted=0)
    return out


def check(runs: list[dict], *flags: str) -> tuple[int, str]:
    with tempfile.TemporaryDirectory() as tmp:
        golden = Path(tmp) / "golden.json"
        golden.write_text(json.dumps(GOLDEN))
        paths = []
        for i, r in enumerate(runs):
            path = Path(tmp) / f"run{i}.json"
            path.write_text(json.dumps(r))
            paths.append(str(path))
        err = io.StringIO()
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
            code = engine_gate.main(["check", "--golden", str(golden), *flags, *paths])
        return code, err.getvalue()


class CheckTests(unittest.TestCase):
    def test_complete_evidence_passes(self):
        runs = [
            run("ref:scalar", "ref", "scalar"),
            run("ref:legacy", "ref", "legacy"),
            run("ref:avx2", "ref", "avx2"),
            run(),
        ]
        self.assertEqual(check(runs, "--fail-on-refusal", "--expect-kernel", "avx2"), (0, ""))

    def test_a_changed_logits_hash_fails(self):
        bad = run()
        bad["cases"][0]["logits_hashes"][1] = "99" * 32
        code, err = check([bad])
        self.assertEqual(code, 1)
        self.assertIn("first at forward call 1", err)

    def test_missing_zero_or_malformed_census_fails(self):
        for census in (None, {}, {"attempted": 0, "accepted": 0}, CENSUS | {"accepted": "10"},
                       CENSUS | {"attempted": 0, "accepted": 0}, CENSUS | {"accepted": True}):
            for spec, engine, path in (("fast:avx2", "fast", "avx2"), ("ref:legacy", "ref", "legacy")):
                r = run(spec, engine, path)
                if census is None:
                    r.pop("census")
                else:
                    r["census"] = census
                code, err = check([r])
                self.assertEqual(code, 1, f"{spec} census {census!r}")
                self.assertIn("census", err)

    def test_inconsistent_census_fails(self):
        r = run()
        r["census"] = dict(CENSUS, attempted=11)
        code, err = check([r])
        self.assertEqual(code, 1)
        self.assertIn("inconsistent", err)

    def test_refusals_fail_only_with_fail_on_refusal(self):
        r = run()
        r["census"] = dict(CENSUS, attempted=11, refused_activation_out_of_domain=1)
        self.assertEqual(check([r])[0], 0)
        code, err = check([r], "--fail-on-refusal")
        self.assertEqual(code, 1)
        self.assertIn("1 SIMD refusals", err)

    def test_fast_simd_runs_need_the_attention_census(self):
        for attention in (None, {}, {"simd_heads": 0, "reference_heads": 0}, {"simd_heads": -1, "reference_heads": 0}):
            r = run()
            r["attention_census"] = attention
            code, err = check([r])
            self.assertEqual(code, 1, f"{attention!r}")
            self.assertIn("attention", err)
        r = run()
        r["attention_census"] = {"simd_heads": 60, "reference_heads": 4}
        self.assertEqual(check([r])[0], 0)
        code, err = check([r], "--fail-on-refusal")
        self.assertEqual(code, 1)
        self.assertIn("fell back to the reference", err)

    def test_the_expected_kernel_must_be_used(self):
        code, err = check([run(kernel_path="neon", spec="fast:neon")], "--expect-kernel", "avx2")
        self.assertEqual(code, 1)
        self.assertIn("expected avx2", err)
        code, err = check([run("ref:scalar", "ref", "scalar")], "--expect-kernel", "avx2")
        self.assertEqual(code, 1)
        self.assertIn("no run used the avx2 kernel", err)

    def test_scalar_runs_need_no_dispatch_evidence(self):
        r = run("ref:scalar", "ref", "scalar")
        r.pop("census")
        self.assertEqual(check([r], "--fail-on-refusal")[0], 0)


if __name__ == "__main__":
    unittest.main()
