"""Runs the cost calculator's own self-test (M9) under unittest, so CI runs it."""
import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location(
    "project_inference_cost", os.path.join(HERE, "..", "project_inference_cost.py"))
cost = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(cost)


class SelfTest(unittest.TestCase):
    def test_the_calculator_self_test_passes(self):
        self.assertEqual(cost.self_test(), "ok")

    def test_a_missing_worker_cost_is_unknown_not_zero(self):
        self.assertEqual(cost.project(None, 1000, 10)["arc_cost_usd_per_1m_output_tokens"], "unknown")


if __name__ == "__main__":
    unittest.main()
