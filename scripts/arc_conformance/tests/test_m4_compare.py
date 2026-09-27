import unittest

from arc_conformance import m4_compare as m4

MANIFEST = {"artifact": {"blake3": "ab" * 32},
            "execution": {"profile": "arc.gguf-llama.i8-per-row.rope-interleaved.v1"}}
ARC = {"artifact_blake3": "0x" + "ab" * 32, "profile": "arc.gguf-llama.i8-per-row.rope-interleaved.v1",
       "tokenizer": "arc.gguf-llama.spm-score-merge.v1", "n_ctx": 512, "chunks": 20,
       "scored_tokens": 5100, "ppl": 6.3}
LOG = "...\n[20]5.9123,\nFinal estimate: PPL = 6.0000 +/- 0.1234\n"


class Compare(unittest.TestCase):
    def test_agreeing_runs_report_the_quality_gap_without_judging_it(self):
        summary = m4.summarize(ARC, LOG, [1, 2, 3], [1, 2, 3], MANIFEST)
        self.assertEqual(summary["problems"], [])
        self.assertTrue(summary["artifact_matches_manifest"])
        self.assertAlmostEqual(summary["ppl_ratio_arc_over_reference"], 1.05)
        self.assertTrue(summary["tokenizer"]["identical"])

    def test_token_differences_and_foreign_artifacts_are_problems(self):
        summary = m4.summarize(dict(ARC, artifact_blake3="cd" * 32), LOG, [1, 2, 9, 4], [1, 2, 3, 4], MANIFEST)
        self.assertIn("token streams differ (M3)", summary["problems"])
        self.assertIn("the measured artifact is not the pinned package", summary["problems"])
        self.assertEqual(summary["tokenizer"]["first_difference"], 2)
        truncated = m4.compare_tokens([1, 2], [1, 2, 3])
        self.assertEqual(truncated["first_difference"], 2)

    def test_reference_formats_parse(self):
        self.assertEqual(m4.read_tokens("[1, 450, 29871]"), [1, 450, 29871])
        self.assertEqual(m4.read_tokens("1\n450\n29871\n"), [1, 450, 29871])
        with self.assertRaises(ValueError):
            m4.llama_ppl("no estimate here")


if __name__ == "__main__":
    unittest.main()
