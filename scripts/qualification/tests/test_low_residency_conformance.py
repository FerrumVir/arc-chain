"""Small offline runner tests. No model, network, compiler or resident weights."""
import copy
import importlib.util
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "low_residency_runner", Path(__file__).resolve().parents[1] / "run_low_residency_conformance.py")
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


def report(mode):
    value = {"schema": "arc.low-residency-conformance.v1", "mode": mode,
             "artifact_blake3": "a" * 64, "artifact_bytes": 100, "profile": runner.PROFILE,
             "graph": {"bos": 1, "eos": [2], "vocab": 5, "max_seq": 8}, "prompt_token_ids": [4], "max_tokens": 2,
             "generation_semantics": "generation-v2/BOS-once/repetition-penalty/EOS-included", "warmup_count": 0, "binary_blake3": "b" * 64,
             "fast_kernel_enabled": False, "warmup_runs": [],
             "measured": {"output_token_ids": [3, 2], "output_hash": "c" * 64,
                          "positions": [{"position": i, "input_token": token, "logit_count": 5,
                                         "logits_blake3_le_i64": "d" * 64, "kv_blake3": "e" * 64}
                                        for i, token in enumerate([1, 4, 3])],
                          "forward_ms": [1.0, 1.0, 1.0]}}
    if mode == "coordinator":
        value.update(non_answer_events=0, local_primary_rows=0, row_answers=3)
    return value


class Comparison(unittest.TestCase):
    def test_exact_trace_passes_without_comparing_timing(self):
        a, b = report("reference"), report("coordinator")
        b["measured"]["forward_ms"] = [5000, 5000, 5000]
        self.assertEqual(runner.compare(a, b)["compared_positions"], 3)

    def test_every_position_digest_token_and_identity_mismatch_refuses(self):
        a = report("reference")
        for field in ("artifact_blake3", "profile", "prompt_token_ids", "binary_blake3", "fast_kernel_enabled"):
            b = report("coordinator")
            b[field] = "wrong"
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                runner.compare(a, b)
        for field in ("logits_blake3_le_i64", "kv_blake3", "input_token", "position"):
            b = report("coordinator")
            b["measured"]["positions"][0][field] = "wrong"
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                runner.compare(a, b)
        for field in ("output_token_ids", "output_hash"):
            b = report("coordinator")
            b["measured"][field] = []
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                runner.compare(a, b)

    def test_empty_trace_fallback_and_missing_warmup_refuse(self):
        for field, value in (("non_answer_events", 1), ("local_primary_rows", 1), ("row_answers", 0)):
            b = report("coordinator")
            b[field] = value
            with self.assertRaises(RuntimeError):
                runner.compare(report("reference"), b)
        a, b = report("reference"), report("coordinator")
        a["measured"]["positions"] = b["measured"]["positions"] = []
        with self.assertRaises(RuntimeError):
            runner.compare(a, b)
        a, b = report("reference"), report("coordinator")
        a["warmup_count"] = b["warmup_count"] = 1
        with self.assertRaises(RuntimeError):
            runner.compare(a, b)
        a["warmup_runs"] = [copy.deepcopy(a["measured"])]
        b["warmup_runs"] = [copy.deepcopy(b["measured"])]
        self.assertTrue(runner.compare(a, b)["exact_output_tokens_and_hash"])

    def test_identically_malformed_traces_cannot_false_pass(self):
        def changed(field, value):
            def mutate(trace):
                trace[field] = value
            return mutate

        def position(field, value):
            def mutate(trace):
                trace["positions"][1][field] = value
            return mutate

        mutations = [
            changed("positions", report("reference")["measured"]["positions"][:-1]),
            changed("output_token_ids", [3]),  # short output without EOS
            changed("output_token_ids", [2, 3]),  # continued after EOS
            changed("output_token_ids", [3, 3, 2]),  # budget exceeded
            changed("output_token_ids", [3, 9]),  # outside vocabulary
            changed("output_hash", "z" * 64),
            changed("forward_ms", [1]),
            changed("forward_ms", [1, float("nan"), 1]),
            position("position", 2), position("input_token", 3),
            position("position", True),
            position("logit_count", 4), position("kv_blake3", "00"),
            position("logits_blake3_le_i64", "not-a-digest"),
        ]
        for index, mutate in enumerate(mutations):
            a, b = report("reference"), report("coordinator")
            mutate(a["measured"])
            b["measured"] = copy.deepcopy(a["measured"])
            with self.subTest(index=index), self.assertRaises(RuntimeError):
                runner.compare(a, b)

    def test_complete_non_eos_trace_requires_the_final_selected_forward(self):
        a, b = report("reference"), report("coordinator")
        for report_ in (a, b):
            report_["measured"]["output_token_ids"] = [3, 4]
        with self.assertRaisesRegex(RuntimeError, "position count"):
            runner.compare(a, b)
        for report_ in (a, b):
            final = copy.deepcopy(report_["measured"]["positions"][-1])
            final.update(position=3, input_token=4)
            report_["measured"]["positions"].append(final)
            report_["measured"]["forward_ms"].append(1)
        self.assertEqual(runner.compare(a, b)["compared_positions"], 4)


class ResourcesAndProcesses(unittest.TestCase):
    def test_insufficient_memory_and_disk_refuse(self):
        snapshot = {"effective_available_bytes": 100, "disk_free_bytes": 200}
        runner.require_capacity(snapshot, 100, 200)
        for memory, disk in ((101, 0), (0, 201)):
            with self.assertRaisesRegex(RuntimeError, "insufficient"):
                runner.require_capacity(snapshot, memory, disk)

    def test_cgroup_and_physical_constraints_both_apply(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(runner, "read_numbers", return_value={"MemAvailable": 1200}), \
                patch.object(runner.sys, "platform", "linux"), \
                patch.object(runner.os, "sysconf", side_effect=[2, 1000]), \
                patch.object(runner, "cgroup_snapshot", return_value={"max": 1000, "current": 600, "inactive_file": 100}):
            self.assertEqual(runner.resources(directory)["effective_available_bytes"], 500)

    def test_proc_status_parser_preserves_numeric_rss(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "status"
            path.write_text("Name:\tpython\nState:\tS (sleeping)\nVmRSS:\t123 kB\nThreads:\t2\n")
            self.assertEqual(runner.read_numbers(path), {"VmRSS": 123 * 1024, "Threads": 2})

    def test_unreadable_cgroup_constraint_refuses(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(runner, "read_numbers", return_value={"MemAvailable": 1200}), \
                patch.object(runner.sys, "platform", "linux"), \
                patch.object(runner.os, "sysconf", side_effect=[2, 1000]), \
                patch.object(runner, "cgroup_snapshot", return_value=None):
            with self.assertRaisesRegex(RuntimeError, "constraint unreadable"):
                runner.resources(directory)

    def test_child_timeout_cleanup_reaps_process(self):
        with tempfile.TemporaryDirectory() as directory:
            children = runner.Children(Path(directory), dict(os.environ))
            child = children.start("fixture", [sys.executable, "-c", "import time; time.sleep(30)"])
            try:
                with self.assertRaisesRegex(RuntimeError, "deadline"):
                    children.wait(child, 0.02)
            finally:
                children.close()
            self.assertIsNotNone(child.poll())
            self.assertIsNotNone(children.records[0]["returncode"])
            with self.assertRaisesRegex(RuntimeError, "child exit"):
                runner.require_clean_exits(children.records)

    def test_nonzero_and_forced_cleanup_prevent_pass(self):
        for status in (1, -9, -15, None):
            with self.subTest(status=status), self.assertRaisesRegex(RuntimeError, "daemon"):
                runner.require_clean_exits([{"name": "reference", "returncode": 0},
                                           {"name": "daemon", "returncode": status}])
        runner.require_clean_exits([{"name": "reference", "returncode": 0},
                                   {"name": "daemon", "returncode": 0}])


if __name__ == "__main__":
    unittest.main()
