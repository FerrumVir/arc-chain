"""FIXTURE tests for scripts/benchmarks/native_request_latency.py (M8).

Every record below is synthetic: it has the shape soak_native_load writes,
with round numbers so each figure in the report can be checked exactly.
"""
import importlib.util
import json
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location(
    "native_request_latency", os.path.join(HERE, "..", "native_request_latency.py"))
latency = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(latency)

DET = "deterministic-test-executor"
REAL = "canonical-i8-real-model qualification=/q.json"


def item(id_, status, submitted, accepted=None, settled=None, attempts=1, executor=DET, **extra):
    record = {"id": id_, "kind": "native_inference", "executor": executor,
              "submitted_t": submitted, "attempts": attempts, "final_status": status}
    if accepted is not None:
        record["accepted_t"] = accepted
    if settled is not None:
        record["settled_t"] = settled
        record["latency_s"] = settled - submitted
    record.update(extra)
    return record


FIXTURE = [
    item("f1", "finalized", 1000.0, 1000.5, 1002.0),
    item("f2", "finalized", 1010.0, 1010.5, 1014.0),
    item("f3", "finalized", 1020.0, 1020.5, 1026.0, attempts=2),
    item("r1", "refunded", 1030.0, 1030.5, 1160.0, presumed_lost_t=1100.0),
    item("x1", "rejected", 1040.0, attempts=3, reason="node 0: HTTP 400: bad"),
    item("e1", "submit_error", 1050.0, attempts=4, reason="node 0: no HTTP response"),
    item("p1", "pending", 1060.0, 1060.5),
    {"id": "faucet-1", "kind": "faucet", "final_status": "finalized", "latency_s": 1.0},
]
RUN = {"completed": True, "workload": {"profile": "native", "native_rate_per_s": 0.05,
                                       "native_requesters": 4, "native_workers": 4,
                                       "executor": DET}}


class Distribution(unittest.TestCase):
    def test_nearest_rank(self):
        d = latency.distribution([float(v) for v in range(10, 0, -1)])
        self.assertEqual((d["n"], d["min"], d["p50"], d["p90"], d["p95"], d["p99"], d["max"]),
                         (10, 1.0, 5.0, 9.0, 10.0, 10.0, 10.0))
        self.assertEqual(d["mean"], 5.5)
        self.assertEqual(latency.distribution([7.0])["p99"], 7.0)

    def test_no_samples_is_none_not_zero(self):
        self.assertIsNone(latency.distribution([]))


class Report(unittest.TestCase):
    def test_every_figure_of_the_fixture(self):
        out = latency.report(FIXTURE, RUN, faults=0)
        self.assertEqual(out["schema"], "arc.m8.native-request-latency.v1")
        self.assertEqual(out["problems"], [])
        self.assertEqual(out["ignored_non_native_records"], 1, "the faucet record is not a native request")
        self.assertEqual(list(out["executors"]), [DET])
        g = out["executors"][DET]
        self.assertEqual(g["outcomes"], {"finalized": 3, "refunded": 1, "rejected": 1,
                                         "submit_error": 1, "pending": 1})
        self.assertEqual(g["finalize_latency_s"]["n"], 3)
        self.assertEqual((g["finalize_latency_s"]["min"], g["finalize_latency_s"]["p50"],
                          g["finalize_latency_s"]["p95"], g["finalize_latency_s"]["mean"]),
                         (2.0, 4.0, 6.0, 4.0))
        self.assertEqual(g["refund_latency_s"]["max"], 130.0, "refunds are never pooled with finalizations")
        self.assertEqual(g["accept_latency_s"]["n"], 5, "only requests a node accepted")
        self.assertEqual(g["accept_latency_s"]["max"], 0.5)
        self.assertEqual(g["attempts_per_accepted_request"], {"1": 4, "2": 1})
        self.assertEqual(g["retried"], 1)
        self.assertEqual(g["presumed_lost_then_settled"], 1)
        self.assertEqual(g["span_s"], 160.0)
        self.assertEqual(g["settled_per_min"], 1.5)
        self.assertEqual(g["finalized_per_min"], 1.125)
        self.assertEqual(out["run"]["offered_rate_per_s"], 0.05)
        self.assertEqual(out["run"]["faults_recorded"], 0)
        self.assertEqual(out["observation_resolution_s"], 0.25)

    def test_executors_are_never_pooled_and_a_label_mismatch_is_reported(self):
        records = FIXTURE + [item("q1", "finalized", 2000.0, 2000.5, 2030.0, executor=REAL)]
        out = latency.report(records, RUN)
        self.assertEqual(sorted(out["executors"]), sorted([DET, REAL]))
        self.assertEqual(out["executors"][REAL]["finalize_latency_s"]["n"], 1)
        self.assertEqual(out["executors"][DET]["finalize_latency_s"]["n"], 3)
        self.assertTrue(any("q1" in p and "differs from run.json" in p for p in out["problems"]))

    def test_unusable_times_are_problems_not_samples(self):
        bad = [item("n1", "finalized", 1000.0, 1000.5, 990.0),
               item("n2", "finalized", 1000.0, 999.0, 1001.0),
               dict(item("n3", "finalized", 1000.0, 1000.5, 1001.0), latency_s=float("nan")),
               dict(item("n4", "finalized", 1000.0), executor=None)]
        out = latency.report(bad, RUN)
        g = out["executors"][DET]
        self.assertEqual(g["finalize_latency_s"]["n"], 1, "only n2's latency is usable")
        self.assertEqual(g["accept_latency_s"]["n"], 2, "n2 was accepted before it was submitted")
        text = " ".join(out["problems"])
        for expected in ("n1: settled without a usable latency", "n2: accepted before it was submitted",
                         "n3: settled without a usable latency", "n4: record has no executor label"):
            self.assertIn(expected, text)
        self.assertIn("unlabelled", out["executors"])

    def test_a_run_directory_with_run_and_faults(self):
        with tempfile.TemporaryDirectory() as work:
            with open(os.path.join(work, "workload.jsonl"), "w") as fh:
                for record in FIXTURE:
                    fh.write(json.dumps(record) + "\n")
                fh.write("not json\n")
            with open(os.path.join(work, "run.json"), "w") as fh:
                json.dump(RUN, fh)
            with open(os.path.join(work, "faults.jsonl"), "w") as fh:
                fh.write(json.dumps({"kind": "kill"}) + "\n" + json.dumps({"kind": "kill"}) + "\n")
            out = latency.report_for_run(work)
            self.assertEqual(out["run"]["faults_recorded"], 2, "latency under faults is labelled, not filtered")
            self.assertEqual(out["executors"][DET]["outcomes"]["finalized"], 3)
            self.assertTrue(any("line 9: not JSON" in p for p in out["problems"]))
            os.remove(os.path.join(work, "run.json"))
            self.assertTrue(any("run.json is missing" in p for p in latency.report_for_run(work)["problems"]))
            target = os.path.join(work, "out.json")
            self.assertEqual(latency.main(["--run", work, "--json-out", target]), 0)
            with open(target) as fh:
                self.assertEqual(json.load(fh)["schema"], latency.SCHEMA)
        self.assertEqual(latency.main(["--run", os.path.join(work, "gone")]), 2)


if __name__ == "__main__":
    unittest.main()
