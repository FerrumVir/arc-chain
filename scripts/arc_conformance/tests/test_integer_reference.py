import json
import unittest

from arc_conformance import integer_reference as ref
from arc_conformance import kat, mutations, operator_vectors, rope_margin


class Arithmetic(unittest.TestCase):
    def test_division_truncates_toward_zero_and_shift_floors(self):
        self.assertEqual(ref.tdiv(-7, 2), -3)
        self.assertEqual(ref.tdiv(7, -2), -3)
        self.assertEqual(ref.trem(-7, 2), -1)
        self.assertEqual(ref.shr(-7, 1), -4)
        self.assertEqual(ref.shr(-1), -1)

    def test_leaving_the_64_bit_domain_is_refused_not_wrapped(self):
        with self.assertRaises(ref.DomainError):
            ref.mul(1 << 40, 1 << 30)
        rows = [[127, 127]]
        with self.assertRaises(ref.DomainError):
            ref.matmul_rows(rows, [1], [1 << 62, 1 << 62])

    def test_exp_table_is_the_recurrence_not_rounded_exp(self):
        self.assertEqual(ref.EXP_LUT[4096], ref.ONE)
        self.assertEqual(ref.EXP_LUT[4095], 65281)
        self.assertEqual(ref.integer_exp(0), ref.ONE)
        self.assertEqual(ref.integer_exp(-16 * ref.ONE), 0)
        self.assertTrue(all(a <= b for a, b in zip(ref.EXP_LUT, ref.EXP_LUT[1:])))

    def test_isqrt_is_the_five_step_newton_iterate(self):
        # Not round(2**16 / sqrt(128)) = 5793: the contract is the algorithm.
        self.assertEqual(ref.integer_isqrt(128 * ref.ONE), 5795)
        self.assertEqual(ref.integer_isqrt(0), 100 * ref.ONE)


class KnownAnswers(unittest.TestCase):
    def test_the_committed_rust_kat_is_reproduced_independently(self):
        fixture = json.loads(kat.DEFAULT_FIXTURE.read_text())
        self.assertEqual(kat.compare(fixture), [])

    def test_the_committed_operator_vectors_are_reproduced(self):
        document = json.loads(operator_vectors.DEFAULT_PATH.read_text())
        self.assertEqual(operator_vectors.verify(document), [])

    def test_every_mutated_clause_is_caught_by_a_known_answer(self):
        fixture = json.loads(kat.DEFAULT_FIXTURE.read_text())
        document = json.loads(operator_vectors.DEFAULT_PATH.read_text())
        for label, make in mutations.mutations():
            with self.subTest(label), make():
                caught = False
                for check in (lambda: kat.compare(fixture),
                              lambda: operator_vectors.verify(document)):
                    try:
                        caught = caught or bool(check())
                    except ref.DomainError:
                        caught = True
                self.assertTrue(caught, label)

    def test_profiles_are_distinct_on_the_same_weights(self):
        fixture = json.loads(kat.DEFAULT_FIXTURE.read_text())
        legacy, _ = kat.build(fixture, ref.LEGACY_SPLIT_HALF)
        interleaved, _ = kat.build(fixture, ref.GGUF_INTERLEAVED)
        tokens = fixture["sequence_tokens"]
        self.assertNotEqual(kat.run_sequence(legacy, tokens)["logits_hashes"],
                            kat.run_sequence(interleaved, tokens)["logits_hashes"])

    def test_generation_v2_owns_bos_and_stops_after_emitting_eos(self):
        fixture = json.loads(kat.DEFAULT_FIXTURE.read_text())
        model, _ = kat.build(fixture, ref.GGUF_INTERLEAVED)
        free = model.generate_v2([2, 7], 10, [])
        stopped = model.generate_v2([2, 7], 10, [free[1]])
        self.assertEqual(stopped, free[:2])

    def test_out_of_vocabulary_tokens_are_refused(self):
        fixture = json.loads(kat.DEFAULT_FIXTURE.read_text())
        model, _ = kat.build(fixture)
        with self.assertRaises(ref.DomainError):
            model.forward(fixture["vocab_size"], model.new_cache())


class RopeMargin(unittest.TestCase):
    def test_a_small_table_matches_exact_rounding(self):
        report = rope_margin.analyse(8, 16, 10000.0)
        self.assertEqual(report["host_table_differs_from_exact_rounding"], 0)
        self.assertGreater(report["tolerated_cos_sin_error_ulps"], 1000)


if __name__ == "__main__":
    unittest.main()
