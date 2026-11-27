from collections.abc import Mapping

_HEX = frozenset("0123456789abcdef")
_ARM_KEYS = frozenset(("workers", "requests"))
_REQ_KEYS = frozenset(("id", "start_ns", "end_ns", "success", "output_hash", "worker_ids", "server_verification_validated"))
_NS_PER_S = 1_000_000_000


def _hex64(value: object, label: str) -> str:
    if not isinstance(value, str) or len(value) != 64 or any(c not in _HEX for c in value):
        raise ValueError(f"{label} must be 64 lowercase hex chars")
    return value


def _worker_set(value: object, count: int, label: str) -> frozenset[str]:
    if not isinstance(value, (list, tuple)) or len(value) != count:
        raise ValueError(f"{label} must have {count} IDs")
    ids = frozenset(_hex64(item, label) for item in value)
    if len(ids) != count:
        raise ValueError(f"{label} must have {count} unique IDs")
    return ids


def _percentile(sorted_vals: list[float], pct: float) -> float:
    pos = (pct / 100.0) * (len(sorted_vals) - 1)
    lo, hi = int(pos), min(int(pos) + 1, len(sorted_vals) - 1)
    return sorted_vals[lo] + (pos - lo) * (sorted_vals[hi] - sorted_vals[lo])


def _max_concurrency(intervals: list[tuple[int, int]]) -> int:
    events = sorted([(s, 1) for s, _ in intervals] + [(e, -1) for _, e in intervals])
    active = peak = 0
    for _, delta in events:
        active += delta
        peak = max(peak, active)
    return peak


def _analyze_arm(name: str, arm: Mapping[str, object], worker_count: int, cap: int, seen_ids: set[str], request_count: int = 6) -> tuple[frozenset[str], int, int, str, dict[str, int | float]]:
    if not isinstance(arm, Mapping) or set(arm.keys()) != _ARM_KEYS:
        raise ValueError(f"{name} arm keys must equal {sorted(_ARM_KEYS)}")
    workers = _worker_set(arm["workers"], worker_count, f"{name}.workers")
    requests = arm["requests"]
    if not isinstance(requests, (list, tuple)) or len(requests) != request_count:
        raise ValueError(f"{name}.requests must have exactly {request_count} records")
    intervals: list[tuple[int, int]] = []
    participating: set[str] = set()
    hashes: set[str] = set()
    for i, req in enumerate(requests):
        tag = f"{name}.requests[{i}]"
        if not isinstance(req, Mapping) or set(req.keys()) != _REQ_KEYS:
            raise ValueError(f"{tag} keys must equal {sorted(_REQ_KEYS)}")
        req_id = req["id"]
        if not isinstance(req_id, str) or not req_id or req_id != req_id.strip() or req_id in seen_ids:
            raise ValueError(f"{tag}.id must be a unique non-empty stripped string")
        seen_ids.add(req_id)
        for flag in ("success", "server_verification_validated"):
            if type(req[flag]) is not bool or not req[flag]:
                raise ValueError(f"{tag}.{flag} must be True")
        start_ns, end_ns = req["start_ns"], req["end_ns"]
        if type(start_ns) is not int or type(end_ns) is not int or start_ns <= 0 or end_ns <= start_ns:
            raise ValueError(f"{tag} requires exact positive int timestamps with end_ns > start_ns")
        hashes.add(_hex64(req["output_hash"], f"{tag}.output_hash"))
        req_workers = _worker_set(req["worker_ids"], 6, f"{tag}.worker_ids")
        if not req_workers.issubset(workers):
            raise ValueError(f"{tag}.worker_ids must be a subset of {name}.workers")
        participating.update(req_workers)
        intervals.append((start_ns, end_ns))
    if len(hashes) != 1:
        raise ValueError(f"{name} requests must share one deterministic output_hash")
    if participating != workers:
        raise ValueError(f"all {name} workers must participate")
    observed_max = _max_concurrency(intervals)
    if observed_max > cap:
        raise ValueError(f"{name} concurrency {observed_max} exceeds cap {cap}")
    first_start, last_end = min(s for s, _ in intervals), max(e for _, e in intervals)
    wall_ns = last_end - first_start
    latencies = sorted((e - s) / _NS_PER_S for s, e in intervals)
    metrics: dict[str, int | float] = {
        "worker_count": worker_count, "participating_worker_count": len(participating),
        "completed_requests": request_count, "wall_seconds": wall_ns / _NS_PER_S,
        "throughput_rps": (request_count * _NS_PER_S) / wall_ns, "observed_max_concurrency": observed_max,
        "median_latency_s": _percentile(latencies, 50.0), "p95_latency_s": _percentile(latencies, 95.0),
    }
    return workers, first_start, last_end, next(iter(hashes)), metrics


def analyze_overlap(before: Mapping[str, object], overlap: Mapping[str, object], after: Mapping[str, object]) -> dict[str, object]:
    seen_ids: set[str] = set()
    b_w, _, b_end, b_h, b_m = _analyze_arm("baseline_before", before, 12, 2, seen_ids, 6)
    e_w, e_start, e_end, e_h, e_m = _analyze_arm("overlap", overlap, 12, 4, seen_ids, 12)
    a_w, a_start, _, a_h, a_m = _analyze_arm("baseline_after", after, 12, 2, seen_ids, 6)
    if a_w != b_w or e_w != b_w:
        raise ValueError("all overlap arms must use the same twelve workers")
    if len({b_h, e_h, a_h}) != 1:
        raise ValueError("all arms must share one deterministic output_hash")
    if b_end > e_start or e_end > a_start:
        raise ValueError("overlap arms must finish in order without overlapping windows")
    for metrics in (b_m, e_m, a_m):
        metrics["observed_client_in_flight_peak"] = metrics.pop("observed_max_concurrency")
    b_rps, e_rps, a_rps = float(b_m["throughput_rps"]), float(e_m["throughput_rps"]), float(a_m["throughput_rps"])
    baseline = (b_rps + a_rps) / 2.0
    return {
        "status": "measured_only", "experiment": "fixed_twelve_worker_verification_overlap",
        "no_real_operator_count": True, "throughput_ratio_is_measurement_only": True,
        "client_intervals_are_not_server_admission": True, "verification_basis": "server_reported_only",
        "output_hash": b_h, "offered_concurrency_caps": [2, 4, 2],
        "arms": {"baseline_before": b_m, "overlap": e_m, "baseline_after": a_m},
        "comparison": {"baseline_mean_throughput_rps": baseline, "overlap_throughput_rps": e_rps,
                       "throughput_ratio": e_rps / baseline, "baseline_drift": abs(a_rps - b_rps) / baseline},
    }
def analyze_scaling(before: Mapping[str, object], expanded: Mapping[str, object], after: Mapping[str, object]) -> dict[str, object]:
    seen_ids: set[str] = set()
    b_w, _, b_end, b_h, b_m = _analyze_arm("before", before, 6, 1, seen_ids)
    e_w, e_start, e_end, e_h, e_m = _analyze_arm("expanded", expanded, 12, 2, seen_ids)
    a_w, a_start, _, a_h, a_m = _analyze_arm("after", after, 6, 1, seen_ids)
    if a_w != b_w or not b_w.issubset(e_w):
        raise ValueError("after must match before workers and expanded must include all baseline workers")
    if len({b_h, e_h, a_h}) != 1:
        raise ValueError("all arms must share one deterministic output_hash")
    if b_end > e_start or e_end > a_start:
        raise ValueError("arms must finish in before -> expanded -> after order without overlap")
    b_rps, e_rps, a_rps = float(b_m["throughput_rps"]), float(e_m["throughput_rps"]), float(a_m["throughput_rps"])
    mean_base = (b_rps + a_rps) / 2.0
    return {
        "status": "measured_only",
        "no_real_operator_count": True,
        "throughput_ratio_is_measurement_only": True,
        "verification_basis": "server_reported_only",
        "output_hash": b_h,
        "arms": {"before": b_m, "expanded": e_m, "after": a_m},
        "comparison": {
            "baseline_mean_throughput_rps": mean_base, "expanded_throughput_rps": e_rps,
            "throughput_ratio": e_rps / mean_base, "baseline_drift": abs(a_rps - b_rps) / mean_base,
        },
    }
