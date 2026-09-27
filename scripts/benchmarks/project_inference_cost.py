#!/usr/bin/env python3
"""Project ARC inference cost against a published provider baseline.

This is deliberately a calculator: it makes no API calls and never treats a
missing cost or throughput measurement as zero.
"""
import argparse, json, math, sys

def project(total_worker_hourly_usd=None, output_tokens=None, benchmark_duration_seconds=None,
            total_measured_egress_bytes=None, egress_usd_per_gb=None,
            verification_worker_seconds=None, verification_hourly_rate=None,
            concurrency=1, verification_included=False):
    vals = [output_tokens, benchmark_duration_seconds, concurrency]
    if any(v is None or not math.isfinite(float(v)) or v <= 0 for v in vals):
        raise ValueError("output tokens, duration, and concurrency must be finite and positive")
    for name, value in (("worker cost", total_worker_hourly_usd), ("egress bytes", total_measured_egress_bytes), ("egress rate", egress_usd_per_gb), ("verification seconds", verification_worker_seconds), ("verification rate", verification_hourly_rate)):
        if value is not None and (not math.isfinite(float(value)) or value < 0): raise ValueError("invalid " + name)
    if (total_measured_egress_bytes is None) != (egress_usd_per_gb is None): raise ValueError("egress bytes and price are both required")
    if verification_included and (verification_worker_seconds is not None or verification_hourly_rate is not None): raise ValueError("verification already included")
    if (verification_worker_seconds is None) != (verification_hourly_rate is None): raise ValueError("verification time and rate are both required")
    excluded = []
    if total_worker_hourly_usd is None:
        return {"arc_cost_usd_per_1m_output_tokens": "unknown", "excluded_cost_components": ["worker", "egress", "verification"], "reason": "worker hourly USD is missing"}
    if total_measured_egress_bytes is None: excluded.append("egress")
    if verification_worker_seconds is None and not verification_included: excluded.append("verification")
    bench = total_worker_hourly_usd * benchmark_duration_seconds / 3600
    if total_measured_egress_bytes is not None: bench += total_measured_egress_bytes / 1e9 * egress_usd_per_gb
    if verification_worker_seconds is not None: bench += verification_worker_seconds / 3600 * verification_hourly_rate
    result = {"concurrency": concurrency, "output_tokens": output_tokens,
              "benchmark_duration_seconds": benchmark_duration_seconds,
              "measured_output_tokens_per_sec": output_tokens / benchmark_duration_seconds,
              "benchmark_cost_usd": bench,
              "excluded_cost_components": excluded,
              "arc_cost_usd_per_1m_output_tokens": bench / output_tokens * 1e6}
    return result

def self_test():
    assert math.isclose(project(3, 3600, 3600, concurrency=1)["benchmark_cost_usd"], 3)
    assert math.isclose(project(3, 3600, 3600, 1000000000, 2, 3600, 4)["benchmark_cost_usd"], 9)
    assert project(None, 3600, 60)["arc_cost_usd_per_1m_output_tokens"] == "unknown"
    assert project(3, 3600, 60, concurrency=1)["measured_output_tokens_per_sec"] == project(3, 3600, 60, concurrency=8)["measured_output_tokens_per_sec"]
    for args in ((3, 0, 60), (3, 1, float("nan")), (3, 1, float("inf"))):
        try: project(*args)
        except ValueError: pass
        else: raise AssertionError("zero/negative throughput must fail")
    for args in ((3, 100, 10, 1, None), (3, 100, 10, None, 2)):
        try: project(*args)
        except ValueError: pass
        else: raise AssertionError("partial optional costs must fail")
    for kw in ({"total_measured_egress_bytes": -1, "egress_usd_per_gb": 1}, {"total_measured_egress_bytes": float("nan"), "egress_usd_per_gb": 1}, {"total_measured_egress_bytes": 1, "egress_usd_per_gb": float("inf")}, {"verification_worker_seconds": -1, "verification_hourly_rate": 1}, {"verification_worker_seconds": 1, "verification_hourly_rate": float("nan")}):
        try: project(3, 100, 10, **kw)
        except ValueError: pass
        else: raise AssertionError("invalid optional cost must fail")
    try: project(3, 100, 10, verification_included=True, verification_worker_seconds=1, verification_hourly_rate=1)
    except ValueError: pass
    else: raise AssertionError("included verification conflict must fail")
    return "ok"

def main():
    p = argparse.ArgumentParser()
    p.add_argument("--total-worker-hourly-usd", type=float)
    p.add_argument("--output-tokens", type=float)
    p.add_argument("--benchmark-duration-seconds", type=float)
    p.add_argument("--concurrency", type=int, default=1)
    p.add_argument("--egress-usd-per-gb", type=float)
    p.add_argument("--total-measured-egress-bytes", type=float)
    p.add_argument("--verification-worker-seconds", type=float)
    p.add_argument("--verification-hourly-rate", type=float)
    p.add_argument("--verification-included", action="store_true")
    p.add_argument("--self-test", action="store_true")
    a = p.parse_args()
    if a.self_test: print(self_test()); return
    if a.output_tokens is None or a.benchmark_duration_seconds is None: p.error("--output-tokens and --benchmark-duration-seconds are required")
    print(json.dumps(project(a.total_worker_hourly_usd, a.output_tokens, a.benchmark_duration_seconds,
                             a.total_measured_egress_bytes, a.egress_usd_per_gb,
                             a.verification_worker_seconds, a.verification_hourly_rate,
                             a.concurrency, a.verification_included), indent=2, sort_keys=True))
if __name__ == "__main__": main()
