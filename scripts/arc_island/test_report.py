"""Regression checks for projection accounting; no weights or network calls."""
import unittest
from unittest.mock import patch
import report


class ProjectionAccounting(unittest.TestCase):
    def test_verify_pass_transfers_both_positions(self):
        # Isolate the link: two positions must take twice one position's
        # transfer time, so 1.85 output tokens cannot be a link-only speedup.
        with patch.object(report, "kimi_step_seconds", return_value=0):
            one = report.island_answer("RTX 5090 32 GB", 26, 0, 20e6)
            two = report.island_answer("RTX 5090 32 GB", 26, 0, 20e6, positions=2)
        self.assertAlmostEqual(two, 2 * one)
        self.assertLess(report.DRAFT_TOKENS_PER_PASS / two, 1 / one)

    def test_island_batch_transfer_cannot_exceed_link_capacity(self):
        with patch.object(report, "kimi_step_seconds", return_value=0):
            rows, _ = report.projection([], [])
        for row in rows["islands"]:
            bandwidth = {"TB5": 80e9, "10 GbE": 10e9, "25 GbE": 25e9}[row["link"]]
            payload_cap = bandwidth / (report.kimi_wire_bytes(row["stages"]) * 8)
            self.assertLessEqual(row["aggregate_tok_s"], payload_cap)

    def test_envelope_includes_more_than_activation_and_hashes(self):
        self.assertEqual(report.kimi_wire_bytes(26), 31990)
        self.assertGreater(report.kimi_wire_bytes(44), report.kimi_wire_bytes(26))

    def test_context_capacity_and_error_direction(self):
        # This configuration fits the 4k assumption but not the 8k sensitivity.
        self.assertTrue(report.kv_fit("RTX 5090 32 GB", 26, 702, 4096)[0])
        self.assertFalse(report.kv_fit("RTX 5090 32 GB", 26, 702, 8192)[0])
        b = {"compute": {"single_process_ms_per_position": 100}, "hops": [],
             "label": "fixture", "wan": [{"stages": 2, "one_way_ms": 0,
             "micro_batches": 2, "depth": 1, "wire_bytes_per_position": 0,
             "uplink_mbit": 100, "concurrency": 2, "generated_tokens": 64,
             "per_answer_decode_tok_s_mean": 5, "aggregate_steady_tok_s": 8,
             "aggregate_tok_s": 6, "bit_exact_vs_single_process": True}]}
        with patch.object(report, "hop_overhead_ms", return_value=0.000001):
            _, errors = report.wan_section(b)
        self.assertAlmostEqual(errors["per_answer"][0], -0.5, places=6)
        self.assertAlmostEqual(errors["aggregate_steady"][0], -0.6, places=6)
        self.assertAlmostEqual(errors["aggregate_wall"][0], -0.7, places=6)


if __name__ == "__main__":
    unittest.main()
