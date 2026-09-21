"""Fast tests for the growth fitter. Synthetic records, no nodes."""

import json
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(HERE)))

from arc_soak import growth  # noqa: E402


def write(run_dir, name, recs):
    with open(os.path.join(run_dir, name), "w") as fh:
        for r in recs:
            fh.write(json.dumps(r) + "\n")


class Fit(unittest.TestCase):
    def test_a_line_is_fitted_exactly(self):
        slope, r = growth.fit([(0, 1), (1, 3), (2, 5), (3, 7)])
        self.assertAlmostEqual(slope, 2.0)
        self.assertAlmostEqual(r, 1.0)

    def test_no_height_variation_is_not_a_fit(self):
        self.assertIsNone(growth.fit([(5, 1), (5, 2), (5, 3)]))


class Report(unittest.TestCase):
    def run_dir(self, restart=False):
        d = tempfile.mkdtemp()
        diag, samples = [], []
        for i in range(40):
            t = 1000.0 + 15 * i
            h = 100 * i
            inc = 1 if (restart and i >= 20) else 0
            base = 0 if inc == 0 else 5000  # a restart resets in-memory sizes
            diag.append({"t": t, "node": 0, "incarnation": inc,
                         "diag": {"height": h, "engine_da_commitments": base + 3 * h,
                                  "engine_dag_blocks": 4000, "state_blocks": h}})
            samples.append({"t": t, "node": 0, "incarnation": inc, "height": h,
                            "rss_kb": 100_000 + 2 * h * 1024 / 1000})
        write(d, "diag.jsonl", diag)
        write(d, "samples.jsonl", samples)
        return d

    def test_a_leak_is_named_and_a_flat_map_is_not(self):
        out = growth.report(self.run_dir(), skip_s=0)
        leak = [l for l in out.splitlines() if "engine_da_commitments" in l][0]
        self.assertIn("3000.00/1k", leak)
        self.assertIn("LEAK?", leak)
        self.assertNotIn("engine_dag_blocks", out)
        self.assertIn("history", [l for l in out.splitlines() if "state_blocks" in l][0])
        rss = [l for l in out.splitlines() if "rss_mb" in l][0]
        self.assertIn("2.00/1k", rss)

    def test_incarnations_are_fitted_separately(self):
        out = growth.report(self.run_dir(restart=True), skip_s=0)
        self.assertIn("node 0 incarnation 0", out)
        self.assertIn("node 0 incarnation 1", out)
        for line in out.splitlines():
            if "engine_da_commitments" in line:
                self.assertIn("3000.00/1k", line)


if __name__ == "__main__":
    unittest.main(verbosity=2)
