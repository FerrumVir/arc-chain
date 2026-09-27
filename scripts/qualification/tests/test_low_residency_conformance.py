"""Small offline runner tests. No model, network, compiler or resident weights."""
import copy
import importlib.util
import io
import json
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
             "fast_kernel_enabled": False, "fast_kernel_requested": False, "simd_available": True,
             "simd_projection_census": None, "warmup_runs": [],
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

    def test_scalar_reference_and_fast_coordinator_modes_may_differ(self):
        a, b = report("reference"), report("coordinator")
        b["fast_kernel_enabled"] = True
        self.assertEqual(runner.compare(a, b)["compared_positions"], 3)

    def test_every_position_digest_token_and_identity_mismatch_refuses(self):
        a = report("reference")
        for field in ("artifact_blake3", "profile", "prompt_token_ids", "binary_blake3"):
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


class PartialLayout(unittest.TestCase):
    def fixture(self):
        graph = dict(layers=2, width=7, kv_width=5, ff_width=11, vocab=13)
        specs, manifests = [], []
        for rank in range(3):
            worker = f"proof-{rank:03}"
            spec = dict(worker_id=worker, layers=[0, 1], include_output=True,
                        row_partition=f"{rank}/3", serialized_row_bytes=1500)
            files = []
            shapes = {"wq": 7, "wk": 5, "wv": 5, "wo": 7, "w_gate": 11, "w_up": 11, "w_down": 7}
            stages = [(layer, tensor, rows) for layer in range(2) for tensor, rows in shapes.items()]
            stages.append((None, "lm_head", 13))
            for layer, tensor, rows in stages:
                files.append(dict(bytes=100, assignment=dict(artifact_id="a" * 64, execution_profile=runner.PROFILE,
                    layer=layer, tensor=tensor, worker_id=worker, row_start=rows * rank // 3,
                    row_end=rows * (rank + 1) // 3)))
            manifests.append(dict(format="arc.tensor-row-offline-partition-bundle.v1",
                row_partition=dict(rank=rank, count=3), artifact_blake3="a" * 64,
                execution_profile=runner.PROFILE, worker_id=worker, serialized_row_bytes=1500, files=files))
            specs.append(spec)
        return dict(graph=graph, layout="partial-rows", row_partitions=3, bundles=specs,
                    total_row_file_bytes=4500), manifests

    def test_uneven_disjoint_layout_includes_output_head_on_every_worker(self):
        plan, manifests = self.fixture()
        runner.validate_layout_plan(plan, 3)
        for spec, manifest in zip(plan["bundles"], manifests):
            runner.validate_export_manifest(spec, manifest, plan, "a" * 64)
        for stage in range(15):
            assignments = [manifest["files"][stage]["assignment"] for manifest in manifests]
            self.assertEqual(assignments[0]["row_start"], 0)
            self.assertEqual(assignments[0]["row_end"], assignments[1]["row_start"])
            self.assertEqual(assignments[1]["row_end"], assignments[2]["row_start"])
            self.assertEqual(len({a["worker_id"] for a in assignments}), 3)
        self.assertEqual(assignments[-1]["tensor"], "lm_head")
        self.assertEqual(assignments[-1]["row_end"], 13)


def census(attempted=3, accepted=3):
    return {"attempted": attempted, "accepted": accepted, "refused_unavailable": 0,
            "refused_shape": 0, "refused_inner_dim_above_i32_bound": 0,
            "refused_activation_out_of_domain": 0, "refused_scale_multiply_would_overflow": 0}


class KernelQualification(unittest.TestCase):
    fixture = PartialLayout.fixture

    def test_default_modes_and_independent_child_environment(self):
        args = runner.parser().parse_args(["--binaries-dir", "/bin", "--model", "/model", "--output-dir", "/out"])
        self.assertEqual(args.kernel, "scalar")
        self.assertEqual(args.reference_kernel, "scalar")
        inherited = {"ARC_FAST_CANONICAL_KERNEL": "1", "ARC_QUALIFICATION_SIMD_CENSUS": "1"}
        scalar = runner.kernel_environment(inherited, "scalar", False)
        fast = runner.kernel_environment(inherited, "fast", True)
        self.assertEqual((scalar["ARC_FAST_CANONICAL_KERNEL"], scalar["ARC_QUALIFICATION_SIMD_CENSUS"]), ("0", "0"))
        self.assertEqual((fast["ARC_FAST_CANONICAL_KERNEL"], fast["ARC_QUALIFICATION_SIMD_CENSUS"]), ("1", "1"))

    def test_fast_distributed_requires_scalar_reference(self):
        runner.validate_kernel_selection("scalar", "fast")
        runner.validate_kernel_selection("fast", "scalar")
        with self.assertRaisesRegex(RuntimeError, "scalar reference"):
            runner.validate_kernel_selection("fast", "fast")

    def test_fast_report_requires_available_effective_backend_and_clean_census(self):
        value = report("coordinator")
        value.update(fast_kernel_requested=True, fast_kernel_enabled=True, simd_available=True,
                     simd_projection_census=census())
        observed = runner.validate_kernel_report(value, "fast", "coordinator", True)
        self.assertTrue(observed["effective_fast_kernel"])
        for fields in ({"simd_available": False}, {"fast_kernel_enabled": False},
                       {"simd_projection_census": census(attempted=0, accepted=0)},
                       {"simd_projection_census": census(attempted=3, accepted=2)}):
            bad = dict(value, **fields)
            with self.subTest(fields=fields), self.assertRaises(RuntimeError):
                runner.validate_kernel_report(bad, "fast", "coordinator", True)

    def test_fast_worker_final_stats_require_real_accepted_projection_calls(self):
        stats = {"worker_id": "proof-0", "fast_kernel_requested": True, "fast_kernel_enabled": True,
                 "simd_available": True, "projection_census": census(), "completed_calls": 3, "refused_calls": 0}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "daemon.log"
            path.write_text(json.dumps({"event": "row_service_stopped", "stats": stats}) + "\n")
            self.assertEqual(runner.read_daemon_simd_report(path, "proof-0", "fast", True)["completed_calls"], 3)
            stats["projection_census"] = census(attempted=2, accepted=1)
            path.write_text(json.dumps({"event": "row_service_stopped", "stats": stats}) + "\n")
            with self.assertRaisesRegex(RuntimeError, "refused"):
                runner.read_daemon_simd_report(path, "proof-0", "fast", True)


    def test_ignored_layout_flags_duplicate_worker_and_capacity_refuse(self):
        for case in range(6):
            plan, _ = self.fixture()
            if case == 0:
                plan["layout"] = "layers"
            elif case == 1:
                plan["bundles"][1]["worker_id"] = plan["bundles"][0]["worker_id"]
            elif case == 2:
                plan["bundles"][2]["row_partition"] = "0/3"
            elif case == 3:
                plan["bundles"][0]["serialized_row_bytes"] = runner.GIB + 1
            elif case == 4:
                plan["bundles"][0]["include_output"] = False
            else:
                plan["bundles"][0]["layers"] = [0]
            with self.subTest(case=case), self.assertRaises(RuntimeError):
                runner.validate_layout_plan(plan, 3)

    def test_exported_gaps_overlap_alias_missing_head_or_changed_identity_refuse(self):
        for case in range(10):
            plan, manifests = self.fixture()
            manifest = manifests[1]
            assignment = manifest["files"][0]["assignment"]
            if case == 0:
                assignment["row_start"] += 1
            elif case == 1:
                assignment["row_end"] += 1
            elif case == 2:
                assignment["worker_id"] = "alias"
            elif case == 3:
                assignment["artifact_id"] = "b" * 64
            elif case == 4:
                assignment["execution_profile"] = "wrong"
            elif case == 5:
                manifest["files"].pop()  # LMHead
            elif case == 6:
                manifest["files"].append(copy.deepcopy(manifest["files"][0]))
            elif case == 7:
                manifest["resident_layers"] = [[0, 2]]
            elif case == 8:
                manifest["row_partition"]["rank"] = 0
            else:
                manifest["format"] = "arc.tensor-row-low-residency-bundle.v1"
            with self.subTest(case=case), self.assertRaises(RuntimeError):
                runner.validate_export_manifest(plan["bundles"][1], manifest, plan, "a" * 64)

    def test_cli_defaults_to_layers_and_bounds_partial_proof(self):
        args = ["--binaries-dir", "/bin", "--model", "/tiny.gguf", "--output-dir", "/proof"]
        self.assertIsNone(runner.parser().parse_args(args).row_partitions)
        self.assertEqual(runner.parser().parse_args(args + ["--row-partitions", "7"]).row_partitions, 7)
        for count in (0, 1, 17, 32):
            with self.subTest(count=count), patch("sys.stderr", new=io.StringIO()), self.assertRaises(SystemExit):
                runner.parser().parse_args(args + ["--row-partitions", str(count)])


if __name__ == "__main__":
    unittest.main()
