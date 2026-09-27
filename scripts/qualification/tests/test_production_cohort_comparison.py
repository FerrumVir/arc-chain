"""False-pass regressions for the production cohort report gate; no model/SSH."""
import copy
import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("cohort_runner", Path(__file__).resolve().parents[1] / "run_low_residency_conformance.py")
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


def reports():
    reference = {"schema": "arc.low-residency-conformance.v1", "mode": "reference",
                 "artifact_blake3": "a" * 64, "artifact_bytes": 100, "profile": runner.PROFILE,
                 "graph": {"layers": 1, "bos": 1, "eos": [2], "vocab": 5, "max_seq": 8},
                 "prompt_token_ids": [4], "max_tokens": 2, "warmup_count": 0, "warmup_runs": [],
                 "generation_semantics": "generation-v2/BOS-once/repetition-penalty/EOS-included",
                 "binary_blake3": "b" * 64,
                 "measured": {"output_token_ids": [3, 2], "output_hash": "c" * 64,
                              "positions": [{"position": i, "input_token": token, "logit_count": 5,
                                             "logits_blake3_le_i64": "d" * 64, "kv_blake3": "e" * 64}
                                            for i, token in enumerate([1, 4, 3])],
                              "forward_ms": [1, 2, 3]}}
    view = {"low_residency": True, "partial_row_residency": True, "local_fallback_enabled": False,
            "placement_timing_prediction_available": False, "coordinator_macs_per_s": 0, "reservations": 0,
            "machines": [{"id": name, "connected": True, "excluded": False,
                          "measured_macs_per_s": 10, "free_slots": 1} for name in ("a", "b")]}
    actual = {key: copy.deepcopy(reference[key]) for key in ("artifact_blake3", "artifact_bytes", "profile",
              "graph", "prompt_token_ids", "max_tokens", "warmup_count", "generation_semantics")}
    actual.update(schema="arc.production-partial-cohort-conformance.v1", mode="production-cohort",
                  qualification_flags_set=False, chain_clock_used=False, offline_observation_height=1,
                  binary_blake3="f" * 64, assignment_hash="1" * 64,
                  ready_view=copy.deepcopy(view), final_view=copy.deepcopy(view), warmup_runs=[],
                  measured={"output_token_ids": [3, 2], "output_hash": "c" * 64,
                            "cohort_record": {"request_id": "2" * 64, "certificate": "3" * 64,
                                              "machines": ["a", "b"], "answered": 8 * 3 * 2,
                                              "fallbacks": 0, "skipped": 0, "faults": 0,
                                              "outcome": "partitioned fixed resident rows; canonical checks"}})
    return reference, actual


class ProductionComparison(unittest.TestCase):
    def test_distinct_binaries_compare_tokens_without_claiming_position_trace(self):
        result = runner.compare_production_cohort(*reports(), ["a", "b"])
        self.assertTrue(result["production_cohort_constructor_readiness_generate"])
        self.assertFalse(result["exact_position_logit_and_kv_digests"])

    def test_each_output_identity_and_scope_mismatch_refuses(self):
        for field, value in (("artifact_blake3", "9" * 64), ("profile", "other"),
                             ("qualification_flags_set", True), ("chain_clock_used", True),
                             ("offline_observation_height", True), ("binary_blake3", "invalid"),
                             ("assignment_hash", "bad"), ("prompt_token_ids", [3]), ("max_tokens", 3)):
            a, b = reports()
            b[field] = value
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                runner.compare_production_cohort(a, b, ["a", "b"])
        for field, value in (("output_hash", "0" * 64), ("output_token_ids", [3]), ("output_token_ids", [True, 2])):
            a, b = reports()
            b["measured"][field] = value
            with self.assertRaises(RuntimeError):
                runner.compare_production_cohort(a, b, ["a", "b"])

    def test_missing_unmeasured_faulty_owner_or_leaked_reservation_refuses(self):
        for key in ("ready_view", "final_view"):
            for field, value in (("connected", False), ("excluded", True),
                                 ("measured_macs_per_s", None), ("measured_macs_per_s", 0), ("free_slots", 0)):
                a, b = reports()
                b[key]["machines"][0][field] = value
                with self.subTest(key=key, field=field), self.assertRaises(RuntimeError):
                    runner.compare_production_cohort(a, b, ["a", "b"])
            for field, value in (("reservations", 1), ("local_fallback_enabled", True), ("machines", [])):
                a, b = reports()
                b[key][field] = value
                with self.assertRaises(RuntimeError):
                    runner.compare_production_cohort(a, b, ["a", "b"])

    def test_certificate_missing_calls_fallback_and_duplicate_owner_refuse(self):
        for field, value in (("certificate", "0" * 64), ("certificate", None),
                             ("answered", 47), ("answered", 0), ("fallbacks", 1), ("skipped", 1),
                             ("faults", 1), ("machines", ["a", "a"]), ("outcome", "local")):
            a, b = reports()
            b["measured"]["cohort_record"][field] = value
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                runner.compare_production_cohort(a, b, ["a", "b"])

    def test_malformed_reference_and_missing_warmup_cannot_false_pass(self):
        a, b = reports()
        a["measured"]["positions"].pop()
        with self.assertRaises(RuntimeError):
            runner.compare_production_cohort(a, b, ["a", "b"])
        a, b = reports()
        a["warmup_count"] = b["warmup_count"] = 1
        with self.assertRaises(RuntimeError):
            runner.compare_production_cohort(a, b, ["a", "b"])
        a["warmup_runs"] = [copy.deepcopy(a["measured"])]
        b["warmup_runs"] = [copy.deepcopy(b["measured"])]
        self.assertTrue(runner.compare_production_cohort(a, b, ["a", "b"])["exact_output_tokens_and_hash"])


if __name__ == "__main__":
    unittest.main()
