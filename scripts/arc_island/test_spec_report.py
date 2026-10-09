"""Checks for the speculation report; no weights, processes or network."""
import json
import os
import tempfile
import unittest

import spec_report


def run(mode, rate, rows=1, depth=1, nominal=None, exact=True, drafter="none"):
    speculation = None
    if mode != "plain":
        speculation = {"passes": 10, "positions": 60, "rollbacks": 3,
                       "accepted": 7, "rejected": 3}
    return {
        "stages": 8, "uplink_mbit": 100.0, "hop_ms": 16.0, "mode": mode,
        "drafter": drafter, "acceptance_nominal": nominal, "rows": rows,
        "depth": depth, "answers": 2, "generated_tokens": 42,
        "per_answer_decode_tok_s_mean": rate, "per_answer_decode_tok_s_median": rate,
        "answer_seconds_mean": 1.5, "speculation": speculation,
        "acceptance_measured": 0.7 if speculation else None,
        "stage_rolled_back_positions": 12, "stage_skipped_positions": 4,
        "bit_exact_vs_single_process": exact, "ledger_equal_to_plain": exact,
    }


def bench(rows, failures=()):
    return {
        "schema": "arc-island-spec-bench-v1", "label": "fixture",
        "scope": "EMULATED fixture.", "failures": list(failures),
        "model": {"config": {"n_layers": 8, "d_model": 256}},
        "assumed": {"requests": 2, "max_tokens": 21, "prompt_len": 8,
                    "wire_bytes_per_position": 28672, "jitter_fraction_of_hop": 0.1},
        "rows": rows,
    }


class SpecReport(unittest.TestCase):
    def write(self, data):
        handle, path = tempfile.mkstemp(suffix=".json")
        with os.fdopen(handle, "w") as out:
            json.dump(data, out)
        self.addCleanup(os.remove, path)
        return path

    def test_best_shapes_and_ratios(self):
        rows = [
            run("plain", 2.0),
            run("sync", 3.0, rows=4, nominal=0.7, drafter="scripted acceptance 0.7"),
            run("sync", 2.5, rows=8, nominal=0.7, drafter="scripted acceptance 0.7"),
            run("async", 6.0, rows=1, depth=8, nominal=0.7, drafter="scripted acceptance 0.7"),
            run("async", 4.0, rows=2, depth=8, nominal=0.7, drafter="scripted acceptance 0.7"),
        ]
        loaded, runs = spec_report.load([self.write(bench(rows))])
        (row,) = spec_report.summary(loaded)
        self.assertEqual(row["drafter"], "scripted a=0.7")
        self.assertEqual(spec_report.shape(row["sync"]), "k=3")
        self.assertEqual(spec_report.shape(row["async"]), "1x8")
        # 60 positions sent for 42 - 2 decode tokens.
        self.assertAlmostEqual(spec_report.waste(row["async"]), 1.5)
        text = spec_report.render(loaded, runs)
        self.assertIn("Emulated measurements on CI runners", text)
        self.assertIn("| 3.00x | 2.00x |", text)

    def test_inexact_runs_are_refused(self):
        rows = [run("plain", 2.0), run("async", 9.0, depth=4, nominal=0.9, exact=False)]
        with self.assertRaises(ValueError):
            spec_report.load([self.write(bench(rows))])
        with self.assertRaises(ValueError):
            spec_report.load([self.write(bench([run("plain", 2.0)], ["differs"]))])


if __name__ == "__main__":
    unittest.main()
