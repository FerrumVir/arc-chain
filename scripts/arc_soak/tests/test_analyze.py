"""Synthetic tests for the soak analyzer. No nodes, no network, milliseconds.

Every case Codex reproduced against the old reporter is here, and each one
asserts on the PROCESS EXIT STATUS, because a printed FAIL that exits 0 is the
defect being fixed:

  * agreement failure, throughput failure  -> must exit non-zero
  * missing / malformed / NaN / zero-baseline / thin throughput evidence
                                           -> must never exit 0
  * one valid hash plus three empty responses -> must not count as agreement
  * a SAFETY VIOLATION line                -> must fail whatever else holds
  * process readiness without the later recovery phases -> not recovery
  * provenance naming a different binary   -> must fail
  * an aborted run                         -> must not pass
"""

import json
import math
import os
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(HERE)))

from arc_soak import analyze  # noqa: E402

IDS = [f"{i:x}" * 64 for i in range(1, 5)]
IDS = [s[:64] for s in IDS]
SHA = "ab" * 32


def good_run(t0=1000.0, duration=900.0, faults=1, **extra):
    run = {
        "schema": "arc-soak-run/1",
        "mode": "test",
        "started_t": t0,
        "planned_duration_s": duration,
        "actual_duration_s": duration,
        "completed": True,
        "nodes": [{"index": i, "identity": IDS[i], "rpc": 9960 + i} for i in range(4)],
        "binary_sha256": SHA,
        "provenance": {"recorded_binary_sha256": SHA, "file": "p.txt"},
        "planned_faults": faults,
        "workload": {"required": False},
    }
    run.update(extra)
    return run


class RunDir:
    """Builds a synthetic run directory in a temp dir."""

    def __init__(self):
        self.path = tempfile.mkdtemp(prefix="arc-soak-test-")
        self.run = good_run()
        self.samples = []
        self.agreement = []
        self.faults = []
        self.workload = []
        self.logs = {i: "INFO normal line\n" for i in range(4)}

    def healthy(self, rate=5.0, start=1000.0, end=1900.0, step=10.0, kill=None,
                down_node=0, restart_after=20.0, rate_after=None):
        """Four nodes advancing at `rate`; optionally one scheduled kill."""
        t = start
        h = 0.0
        while t <= end:
            r = rate if (kill is None or t < kill) else (rate_after if rate_after is not None else rate)
            for n in range(4):
                down = kill is not None and kill <= t < kill + restart_after and n == down_node
                self.samples.append({
                    "t": t, "node": n, "alive": not down, "scheduled_down": down,
                    "http_ok": not down, "identity": IDS[n],
                    "height": None if down else int(h), "dag_round": None if down else int(h * 1.3),
                    "peers": 3,
                })
            t += step
            h += r * step
        return self

    def agree(self, t, target, hashes=None, scheduled=(), drop=(), bad_height=(),
              wrong_identity=()):
        reps = []
        for n in range(4):
            h = (hashes[n] if hashes else "cd" * 32)
            rep = {"node": n, "scheduled_down": n in scheduled, "ok": True,
                   "identity": IDS[n], "height": target, "hash": h,
                   "parent": "ee" * 32, "state_root": "ff" * 32}
            if n in drop:
                rep = {"node": n, "scheduled_down": False, "ok": False,
                       "error": "timeout"}
            if n in bad_height:
                rep["height"] = target + 1
            if n in wrong_identity:
                rep["identity"] = "99" * 32
            reps.append(rep)
        self.agreement.append({"t": t, "target": target, "replicas": reps})
        return self

    def fault(self, index=0, node=0, kill=1300.0, restart=1320.0, phases=None,
              identity_verified=True):
        f = {"index": index, "node": node, "roles": ["seed"], "kill_t": kill,
             "restart_t": restart, "identity_verified": identity_verified}
        base = {"process_ready_t": 5, "first_peer_t": 8, "caught_up_t": 30,
                "first_new_work_t": 40, "agreement_t": 50}
        if phases is not None:
            base.update(phases)
        for k, dt in base.items():
            f[k] = None if dt is None else restart + dt
        self.faults.append(f)
        return self

    def write(self):
        def dump(name, rows):
            with open(os.path.join(self.path, name), "w") as fh:
                for r in rows:
                    fh.write(json.dumps(r) + "\n")
        with open(os.path.join(self.path, "run.json"), "w") as fh:
            json.dump(self.run, fh)
        dump("samples.jsonl", self.samples)
        dump("agreement.jsonl", self.agreement)
        dump("faults.jsonl", self.faults)
        dump("workload.jsonl", self.workload)
        for n, text in self.logs.items():
            with open(os.path.join(self.path, f"node-{n}.log"), "w") as fh:
                fh.write(text)
        return self

    def verdict(self):
        self.write()
        code = analyze.main([self.path])
        with open(os.path.join(self.path, "verdict.json")) as fh:
            return code, json.load(fh)

    def cleanup(self):
        shutil.rmtree(self.path, ignore_errors=True)


def passing():
    """A run that genuinely meets every criterion."""
    d = RunDir().healthy(kill=1300.0)
    for t in (1100.0, 1200.0, 1500.0, 1700.0):
        d.agree(t, int((t - 1000) * 5) - 20)
    d.fault()
    return d


class AnalyzerTests(unittest.TestCase):
    def setUp(self):
        self.dirs = []
        import io
        self._stdout = sys.stdout
        sys.stdout = io.StringIO()   # keep the test output readable

    def tearDown(self):
        sys.stdout = self._stdout
        for d in self.dirs:
            d.cleanup()

    def run_case(self, d):
        self.dirs.append(d)
        return d.verdict()

    # ── the baseline: a genuine pass exits 0 ────────────────────────────────
    def test_a_run_meeting_every_criterion_passes_with_exit_zero(self):
        code, v = self.run_case(passing())
        self.assertEqual(v["status"], "PASS", v)
        self.assertEqual(code, 0)

    def test_per_node_probe_jitter_does_not_split_a_sample(self):
        # The live orchestrator probes each node at a slightly different
        # instant. Grouping by that per-probe time made the chain series
        # alternate between nodes and the baseline dispersion undefined; the
        # tick identity must hold a sample together.
        d = passing()
        for s in d.samples:
            s["tick_t"] = s["t"]
            s["t"] = s["t"] + 0.013 * s["node"]
        # a laggard, so alternating between nodes WOULD matter
        for s in d.samples:
            if s["node"] == 3 and s["height"] is not None:
                s["height"] = max(0, s["height"] - 40)
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "PASS", v)
        self.assertIsNotNone(v["facts"]["throughput"]["baseline"]["cv"])

    # ── Codex item 1: a printed failure must change the exit status ─────────
    def test_agreement_failure_exits_nonzero(self):
        d = passing()
        d.agree(1800.0, 3980, hashes=["cd" * 32, "cd" * 32, "cd" * 32, "01" * 32])
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")
        self.assertEqual(code, 1)

    def test_throughput_failure_exits_nonzero(self):
        d = RunDir().healthy(kill=1300.0, rate_after=0.3)
        for t in (1100.0, 1200.0, 1500.0, 1700.0):
            d.agree(t, 100)
        d.fault()
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL", v)
        self.assertEqual(code, 1)
        self.assertTrue(any("steady rate" in r for r in v["fail"]), v["fail"])

    def test_an_aborted_run_is_not_a_pass(self):
        d = passing()
        d.run["completed"] = False
        d.run["abort_reason"] = "orchestrator raised KeyError"
        code, v = self.run_case(d)
        self.assertNotEqual(code, 0)
        self.assertEqual(v["status"], "INCOMPLETE")

    def test_a_truncated_run_is_not_a_pass(self):
        d = passing()
        d.run["actual_duration_s"] = 100.0
        code, _ = self.run_case(d)
        self.assertNotEqual(code, 0)

    def test_a_missing_run_record_is_not_a_pass(self):
        d = passing()
        d.write()
        os.remove(os.path.join(d.path, "run.json"))
        self.dirs.append(d)
        code = analyze.main([d.path])
        self.assertNotEqual(code, 0)

    # ── Codex item 2: missing / malformed performance evidence ──────────────
    def test_no_samples_is_incomplete_not_pass(self):
        d = passing()
        d.samples = []
        code, v = self.run_case(d)
        self.assertNotEqual(code, 0)

    def test_malformed_sample_lines_are_reported_and_block_a_pass(self):
        d = passing()
        d.write()
        with open(os.path.join(d.path, "samples.jsonl"), "a") as fh:
            fh.write("{not json\n")
        self.dirs.append(d)
        code = analyze.main([d.path])
        self.assertNotEqual(code, 0)

    def test_nan_heights_do_not_produce_a_rate(self):
        d = passing()
        for s in d.samples:
            s["height"] = float("nan")
        code, v = self.run_case(d)
        self.assertNotEqual(code, 0)

    def test_a_zero_baseline_is_incomplete(self):
        d = RunDir().healthy(rate=0.0, kill=1300.0)
        # heights stay at zero, so "every node has a height > 0" never holds
        d.agree(1100.0, 0)
        d.fault()
        code, v = self.run_case(d)
        self.assertNotEqual(code, 0)

    def test_too_few_baseline_samples_is_incomplete(self):
        d = RunDir().healthy(kill=1060.0, end=1900.0)
        for t in (1500.0, 1700.0):
            d.agree(t, 100)
        d.fault(kill=1060.0, restart=1080.0)
        code, v = self.run_case(d)
        self.assertNotEqual(code, 0)
        self.assertTrue(any("baseline has" in r for r in v["incomplete"]), v)

    def test_an_unstable_baseline_is_incomplete(self):
        # Monotonic but uneven: alternate small and large steps inside the
        # baseline, so ONLY the dispersion check can object. (An earlier version
        # of this test made heights go backwards and was caught by the stall
        # detector instead - passing for the wrong reason.)
        d = RunDir()
        h = 0
        t = 1000.0
        while t <= 1900.0:
            step = (1 if int(t) % 20 == 0 else 99) if t < 1300 else 50
            h += step
            for n in range(4):
                down = 1300 <= t < 1320 and n == 0
                d.samples.append({"t": t, "node": n, "alive": not down,
                                  "scheduled_down": down, "identity": IDS[n],
                                  "height": None if down else h, "peers": 3})
            t += 10.0
        for tt in (1100.0, 1500.0, 1700.0):
            d.agree(tt, 100)
        d.fault()
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "INCOMPLETE", v)
        self.assertTrue(any("not stable" in r for r in v["incomplete"]), v["incomplete"])
        self.assertFalse(any("stopped advancing" in r for r in v["fail"]), v["fail"])

    def test_a_running_node_whose_height_goes_backwards_fails(self):
        d = passing()
        for s in d.samples:
            if s["node"] == 2 and s["t"] == 1600.0:
                s["height"] = 5
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")
        self.assertTrue(any("height regression" in r for r in v["fail"]), v["fail"])

    def test_a_restarted_node_starting_lower_is_not_a_regression(self):
        d = passing()
        # node 0 is scheduled down 1300-1320; its first reading after that is
        # compared with nothing, because it is a new process.
        for s in d.samples:
            if s["node"] == 0 and s["t"] == 1320.0:
                s["height"] = s["height"] - 40
        code, v = self.run_case(d)
        self.assertEqual(v["facts"]["height_regressions"], 0)

    def test_no_executed_fault_when_faults_were_planned_is_incomplete(self):
        d = RunDir().healthy()
        d.agree(1100.0, 100)
        d.run["planned_faults"] = 1
        code, v = self.run_case(d)
        self.assertNotEqual(code, 0)

    def test_a_recovery_window_too_short_to_measure_is_incomplete(self):
        d = RunDir().healthy(kill=1300.0, end=1420.0)
        d.run["planned_duration_s"] = d.run["actual_duration_s"] = 420.0
        for t in (1100.0, 1200.0, 1400.0):
            d.agree(t, 100)
        d.fault()
        code, v = self.run_case(d)
        self.assertNotEqual(code, 0)

    # ── Codex item 3: missing replicas must not count as agreement ──────────
    def test_one_valid_hash_and_three_failed_responses_is_not_agreement(self):
        d = RunDir().healthy(kill=1300.0)
        d.agree(1100.0, 100, drop=(1, 2, 3))
        d.agree(1500.0, 100, drop=(1, 2, 3))
        d.fault()
        code, v = self.run_case(d)
        self.assertNotEqual(code, 0)
        self.assertEqual(v["facts"]["agreement"]["full"], 0)

    def test_a_replica_answering_a_different_height_is_not_agreement(self):
        d = RunDir().healthy(kill=1300.0)
        d.agree(1100.0, 100, bad_height=(2,))
        d.fault()
        code, v = self.run_case(d)
        self.assertEqual(v["facts"]["agreement"]["full"], 0)
        self.assertNotEqual(code, 0)

    def test_a_replica_with_the_wrong_identity_is_not_agreement(self):
        d = RunDir().healthy(kill=1300.0)
        d.agree(1100.0, 100, wrong_identity=(3,))
        d.fault()
        code, v = self.run_case(d)
        self.assertEqual(v["facts"]["agreement"]["full"], 0)
        self.assertNotEqual(code, 0)

    def test_a_check_missing_an_intended_replica_is_not_agreement(self):
        d = passing()
        d.agreement[0]["replicas"] = d.agreement[0]["replicas"][:3]
        code, v = self.run_case(d)
        self.assertNotEqual(code, 0)

    def test_scheduled_downtime_is_partial_not_agreement_and_not_a_failure(self):
        d = passing()
        d.agree(1310.0, 1500, scheduled=(0,))
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "PASS", v)
        self.assertEqual(v["facts"]["agreement"]["partial_scheduled"], 1)

    def test_up_replicas_disagreeing_during_scheduled_downtime_still_fails(self):
        d = passing()
        d.agree(1310.0, 1500, scheduled=(0,),
                hashes=["00" * 32, "cd" * 32, "cd" * 32, "01" * 32])
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")

    def test_frequent_unscheduled_unavailability_fails(self):
        d = passing()
        for t in range(1400, 1900, 10):
            d.agree(float(t), 100, drop=(2,))
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")

    # ── Codex item 4: safety violations decide the verdict ──────────────────
    def test_a_safety_violation_fails_a_run_that_otherwise_passes(self):
        d = passing()
        d.logs[2] = ("INFO fine\n2026-09-21T00:00:00Z ERROR arc_node::consensus: "
                     "SAFETY VIOLATION: two quorum finality certificates at one height\n")
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")
        self.assertEqual(code, 1)
        self.assertEqual(v["facts"]["safety_violations"]["node-2.log"], 1)
        self.assertTrue(v["facts"]["safety_evidence"])

    def test_a_panic_fails_the_run(self):
        d = passing()
        d.logs[1] = "thread 'tokio' panicked at crates/x.rs:1:1:\n"
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")

    def test_missing_logs_make_safety_unverifiable(self):
        d = passing()
        d.logs = {}
        code, v = self.run_case(d)
        self.assertNotEqual(code, 0)

    # ── Codex item 5: readiness is not recovery ─────────────────────────────
    def test_process_ready_without_catch_up_is_not_recovery(self):
        d = RunDir().healthy(kill=1300.0)
        for t in (1100.0, 1200.0, 1500.0, 1700.0):
            d.agree(t, 100)
        d.fault(phases={"caught_up_t": None, "first_new_work_t": None, "agreement_t": None})
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")
        self.assertTrue(any("never reached" in r for r in v["fail"]), v["fail"])

    def test_an_unverified_identity_is_not_recovery(self):
        d = passing()
        d.faults[0]["identity_verified"] = False
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")

    def test_recovery_over_budget_fails(self):
        d = passing()
        d.faults[0]["agreement_t"] = d.faults[0]["restart_t"] + 500
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")

    def test_an_unscheduled_process_death_fails(self):
        d = passing()
        d.samples.append({"t": 1600.0, "node": 3, "alive": False, "scheduled_down": False,
                          "height": None})
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")

    def test_a_host_pause_leaves_the_run_incomplete_although_heights_rose_across_it(self):
        # The 2026-09-22 soak: the host hibernated for 71 minutes, and the
        # first sample after it showed higher heights than the last one before
        # it, so no stall was seen. 150 s without a sample here.
        d = passing()
        d.samples = [s for s in d.samples if not 1750 < s["t"] < 1900]
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "INCOMPLETE", v)
        self.assertNotEqual(code, 0)
        self.assertTrue(any(r.startswith("coverage:") for r in v["incomplete"]), v)
        self.assertEqual([g["seconds"] for g in v["facts"]["sample_gaps"]], [150.0])
        self.assertEqual(v["facts"]["unobserved_s"], 150.0)

    def test_the_coverage_limit_follows_the_configured_sample_interval(self):
        # At a 20 s interval, ten intervals (200 s) is the limit: 150 s is not
        # a gap, and the run passes.
        d = passing()
        d.run["sample_interval_s"] = 20.0
        d.samples = [s for s in d.samples if not 1750 < s["t"] < 1900]
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "PASS", v)
        self.assertEqual(v["facts"]["sample_gaps"], [])

    def test_a_network_wide_stall_fails(self):
        d = passing()
        for s in d.samples:
            if 1600 <= s["t"] <= 1800 and s["height"] is not None:
                s["height"] = 3000
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")

    # ── Codex item 6: workload accounting ───────────────────────────────────
    def test_a_required_workload_with_nothing_offered_fails(self):
        d = passing()
        d.run["workload"] = {"required": True}
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")

    def test_accepted_work_that_never_settles_fails(self):
        d = passing()
        d.run["workload"] = {"required": True}
        d.workload = [
            {"id": "a", "final_status": "finalized", "latency_s": 2.0,
             "accepted_by": 1, "submitted_t": 1100.0},
            {"id": "b", "final_status": "pending", "accepted_by": 1, "submitted_t": 1400.0},
        ]
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")

    def test_work_lost_only_because_its_node_was_killed_is_excused(self):
        d = passing()
        d.run["workload"] = {"required": True}
        d.workload = [
            {"id": "a", "final_status": "finalized", "latency_s": 2.0,
             "accepted_by": 1, "submitted_t": 1100.0},
            {"id": "b", "final_status": "pending", "accepted_by": 0, "submitted_t": 1290.0},
        ]
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "PASS", v)
        self.assertEqual(v["facts"]["workload"]["lost_to_fault"], 1)

    def test_a_kill_long_after_acceptance_does_not_excuse_a_lost_item(self):
        d = passing()
        d.run["workload"] = {"required": True}
        d.workload = [
            {"id": "a", "final_status": "finalized", "latency_s": 2.0,
             "accepted_by": 1, "submitted_t": 1100.0},
            # node 0 is killed at 1300 - two hundred seconds later. That is
            # not why this was lost.
            {"id": "b", "final_status": "pending", "accepted_by": 0, "submitted_t": 1100.0},
        ]
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")
        self.assertEqual(v["facts"]["workload"]["unresolved"], 1)

    def test_an_included_item_that_failed_to_execute_fails_the_run(self):
        d = passing()
        d.run["workload"] = {"required": True}
        d.workload = [
            {"id": "a", "final_status": "finalized", "latency_s": 2.0,
             "accepted_by": 1, "submitted_t": 1100.0},
            {"id": "b", "final_status": "failed", "reason": "insufficient balance",
             "accepted_by": 1, "submitted_t": 1100.0},
        ]
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")

    def test_a_rejection_without_a_reason_fails(self):
        d = passing()
        d.run["workload"] = {"required": True}
        d.workload = [
            {"id": "a", "final_status": "finalized", "latency_s": 2.0,
             "accepted_by": 1, "submitted_t": 1100.0},
            {"id": "b", "final_status": "rejected", "submitted_t": 1100.0},
        ]
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")

    # ── Codex item 7: provenance bound to the binary that ran ───────────────
    def test_provenance_for_a_different_binary_fails(self):
        d = passing()
        d.run["provenance"]["recorded_binary_sha256"] = "cd" * 32
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")

    def test_missing_provenance_fails(self):
        d = passing()
        del d.run["provenance"]
        code, v = self.run_case(d)
        self.assertEqual(v["status"], "FAIL")

    # ── robustness of the judge itself ──────────────────────────────────────
    def test_the_verdict_file_and_the_exit_status_always_agree(self):
        for build in (passing, lambda: RunDir()):
            d = build()
            code, v = self.run_case(d)
            self.assertEqual(code, analyze.EXIT_CODES[v["status"]])

    def test_thresholds_actually_applied_are_recorded(self):
        d = passing()
        d.run["thresholds"] = {"min_recovered_ratio": 0.9}
        code, v = self.run_case(d)
        self.assertEqual(v["facts"]["thresholds"]["min_recovered_ratio"], 0.9)

    def test_a_non_finite_threshold_override_is_ignored(self):
        d = passing()
        d.run["thresholds"] = {"min_recovered_ratio": float("nan")}
        code, v = self.run_case(d)
        self.assertEqual(v["facts"]["thresholds"]["min_recovered_ratio"],
                         analyze.DEFAULT_THRESHOLDS["min_recovered_ratio"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
