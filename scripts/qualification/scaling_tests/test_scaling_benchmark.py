import unittest
from collections.abc import Callable
from typing import Any

from scaling_benchmark import analyze_scaling

_HASH_A, _HASH_B, _S = "a" * 64, "b" * 64, 1_000_000_000


def _wid(i: int) -> str:
    return f"{i:064x}"


def _req(rid: str, start: object, end: object, workers: list[str], out_hash: str = _HASH_A) -> dict[str, Any]:
    return {"id": rid, "start_ns": start, "end_ns": end, "success": True, "output_hash": out_hash, "worker_ids": list(workers), "server_verification_validated": True}


def _arms() -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
    base, extra = [_wid(i) for i in range(6)], [_wid(i) for i in range(6, 12)]
    before = {"workers": list(base), "requests": [_req(f"b{i}", (10 + i) * _S, (11 + i) * _S, base) for i in range(6)]}
    expanded = {"workers": base + extra, "requests": [_req(f"e{i}", (20 + i // 2) * _S, (21 + i // 2) * _S, base if i % 2 == 0 else extra) for i in range(6)]}
    after = {"workers": list(base), "requests": [_req(f"a{i}", (30 + i) * _S, (31 + i) * _S, base) for i in range(6)]}
    return before, expanded, after


class ScalingBenchmarkTests(unittest.TestCase):
    def _assert_invalid(self, mutate: Callable[[dict[str, Any], dict[str, Any], dict[str, Any]], object]) -> None:
        before, expanded, after = _arms()
        mutate(before, expanded, after)
        with self.assertRaises(ValueError):
            analyze_scaling(before, expanded, after)

    def test_known_2x_scenario_touching_drift_and_percentiles(self) -> None:
        report: Any = analyze_scaling(*_arms())
        self.assertEqual((report["status"], report["verification_basis"], report["output_hash"]), ("measured_only", "server_reported_only", _HASH_A))
        self.assertIs(report["no_real_operator_count"], True)
        self.assertIs(report["throughput_ratio_is_measurement_only"], True)
        self.assertEqual([report["arms"][k]["throughput_rps"] for k in ("before", "expanded", "after")], [1.0, 2.0, 1.0])
        self.assertEqual([report["arms"][k]["observed_max_concurrency"] for k in ("before", "expanded", "after")], [1, 2, 1])
        self.assertEqual((report["comparison"]["throughput_ratio"], report["comparison"]["baseline_drift"]), (2.0, 0.0))
        before, expanded, after = _arms()
        t, after["requests"] = 30 * _S, []
        for i, dur in enumerate((1, 2, 3, 4, 5, 6)):
            after["requests"].append(_req(f"a{i}", t, t + dur * _S, before["workers"]))
            t += dur * _S
        r2: Any = analyze_scaling(before, expanded, after)
        self.assertAlmostEqual(r2["arms"]["after"]["median_latency_s"], 3.5)
        self.assertAlmostEqual(r2["arms"]["after"]["p95_latency_s"], 5.75)
        mean_rps = (1.0 + 6 / 21) / 2.0
        self.assertAlmostEqual(r2["comparison"]["baseline_drift"], abs(6 / 21 - 1.0) / mean_rps)
        self.assertAlmostEqual(r2["comparison"]["throughput_ratio"], 2.0 / mean_rps)

    def test_bad_hash(self) -> None:
        for bad in (_HASH_B, "A" * 64, "a" * 63):
            self._assert_invalid(lambda b, e, a, h=bad: a["requests"][0].__setitem__("output_hash", h))
        self._assert_invalid(lambda b, e, a: [r.__setitem__("output_hash", _HASH_B) for r in a["requests"]])

    def test_nonparticipation(self) -> None:
        self._assert_invalid(lambda b, e, a: [r.__setitem__("worker_ids", list(b["workers"])) for r in e["requests"]])

    def test_overlap_over_cap_and_phase_order(self) -> None:
        self._assert_invalid(lambda b, e, a: b["requests"][1].__setitem__("start_ns", b["requests"][0]["end_ns"] - 1))
        self._assert_invalid(lambda b, e, a: e["requests"][2].__setitem__("start_ns", e["requests"][0]["end_ns"] - 1))
        self._assert_invalid(lambda b, e, a: e["requests"][0].__setitem__("start_ns", b["requests"][-1]["end_ns"] - 1))
        self._assert_invalid(lambda b, e, a: a["requests"][0].__setitem__("start_ns", e["requests"][-1]["end_ns"] - 1))

    def test_clock_bool_and_bounds(self) -> None:
        for s, e_ns in ((False, _S), (1, True), (0, _S), (5 * _S, 5 * _S), (1.0, 2.0)):
            self._assert_invalid(lambda b, e, a, s=s, e_ns=e_ns: b["requests"][0].update({"start_ns": s, "end_ns": e_ns}))

    def test_wrong_identities(self) -> None:
        self._assert_invalid(lambda b, e, a: (a["workers"].__setitem__(5, _wid(99)), [r["worker_ids"].__setitem__(5, _wid(99)) for r in a["requests"]]))
        self._assert_invalid(lambda b, e, a: (e["workers"].__setitem__(0, _wid(99)), [e["requests"][i]["worker_ids"].__setitem__(0, _wid(99)) for i in (0, 2, 4)]))
        self._assert_invalid(lambda b, e, a: b["requests"][0]["worker_ids"].__setitem__(0, _wid(99)))
        self._assert_invalid(lambda b, e, a: b["requests"][0]["worker_ids"].__setitem__(1, b["requests"][0]["worker_ids"][0]))

    def test_duplicate_ids(self) -> None:
        self._assert_invalid(lambda b, e, a: a["requests"][0].__setitem__("id", b["requests"][0]["id"]))
        self._assert_invalid(lambda b, e, a: b["requests"][1].__setitem__("id", b["requests"][0]["id"]))

    def test_failed_and_unvalidated_post(self) -> None:
        for k, v in (("success", False), ("success", 1), ("server_verification_validated", False), ("server_verification_validated", 1)):
            self._assert_invalid(lambda b, e, a, k=k, v=v: b["requests"][0].__setitem__(k, v))

    def test_strict_api_counts_and_keys(self) -> None:
        self._assert_invalid(lambda b, e, a: b["requests"].pop())
        self._assert_invalid(lambda b, e, a: b["requests"].append(_req("b6", 16 * _S, 17 * _S, b["workers"])))
        self._assert_invalid(lambda b, e, a: e["workers"].pop())
        self._assert_invalid(lambda b, e, a: b["requests"][0]["worker_ids"].pop())
        self._assert_invalid(lambda b, e, a: b["requests"][0].__setitem__("extra", 1))
        self._assert_invalid(lambda b, e, a: b.pop("workers"))


if __name__ == "__main__":
    unittest.main()
