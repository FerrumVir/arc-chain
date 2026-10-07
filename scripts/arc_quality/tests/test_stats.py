"""Paired statistics against hand-checked values."""

from __future__ import annotations

import random
import unittest

from arc_quality import stats


def pairs(both: int, only_ref: int, only_arc: int, neither: int):
    return ([(True, True)] * both + [(True, False)] * only_ref
            + [(False, True)] * only_arc + [(False, False)] * neither)


class Wilson(unittest.TestCase):
    def test_known_value(self):
        # Wilson 95% interval for 81/263 (Newcombe 1998, table I example).
        low, high = stats.wilson(81, 263, stats.z_value(0.95))
        self.assertAlmostEqual(low, 0.2553, places=3)
        self.assertAlmostEqual(high, 0.3662, places=3)

    def test_edges(self):
        low, high = stats.wilson(0, 10, 1.96)
        self.assertEqual(low, 0.0)
        self.assertGreater(high, 0.25)
        self.assertEqual(stats.wilson(0, 0, 1.96), (0.0, 1.0))


class Newcombe(unittest.TestCase):
    def test_coverage(self):
        # Simulated paired samples with a known difference (ARC 3 points
        # worse): the 95% interval must contain it about 95% of the time.
        rng = random.Random(7)
        cells = [(0.60, (True, True)), (0.06, (True, False)), (0.03, (False, True)), (0.31, (False, False))]

        def draw():
            r, acc = rng.random(), 0.0
            for p, cell in cells:
                acc += p
                if r < acc:
                    return cell
            return cells[-1][1]

        trials = 1000
        covered = 0
        for _ in range(trials):
            low, high = stats.newcombe_paired(stats.paired_table([draw() for _ in range(200)]))
            covered += low <= -0.03 <= high
        self.assertGreater(covered / trials, 0.93)
        self.assertLess(covered / trials, 0.97)

    def test_identical_engines(self):
        summary = stats.summarize(pairs(60, 0, 0, 40))
        self.assertEqual(summary["delta"], 0.0)
        low, high = summary["delta_ci"]
        self.assertLess(low, 0.0)
        self.assertGreater(high, 0.0)
        self.assertEqual(summary["mcnemar_p"], 1.0)
        # More identical items give a tighter interval.
        wide = stats.summarize(pairs(60, 0, 0, 40))["delta_ci"]
        tight = stats.summarize(pairs(600, 0, 0, 400))["delta_ci"]
        self.assertLess(tight[1] - tight[0], wide[1] - wide[0])

    def test_direction(self):
        worse = stats.summarize(pairs(50, 30, 0, 20))
        self.assertAlmostEqual(worse["delta"], -0.3)
        self.assertLess(worse["delta_ci"][1], 0.0)
        self.assertAlmostEqual(worse["reference_accuracy"], 0.8)
        self.assertAlmostEqual(worse["arc_accuracy"], 0.5)


class McNemar(unittest.TestCase):
    def test_values(self):
        self.assertEqual(stats.mcnemar_exact(0, 0), 1.0)
        self.assertAlmostEqual(stats.mcnemar_exact(0, 5), 0.0625)
        self.assertAlmostEqual(stats.mcnemar_exact(9, 2), 0.06543, places=4)
        self.assertEqual(stats.mcnemar_exact(3, 3), 1.0)


class Sizing(unittest.TestCase):
    def test_items_needed(self):
        # 5% discordant, 1.5-point margin: 0.05 * (1.96 / 0.015)^2 = 853.7.
        self.assertEqual(stats.items_needed(0.05, 0.015), 854)
        self.assertGreater(stats.items_needed(0.10, 0.015), stats.items_needed(0.05, 0.015))
        with self.assertRaises(ValueError):
            stats.items_needed(0.1, 0.0)


if __name__ == "__main__":
    unittest.main()
