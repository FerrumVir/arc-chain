from concurrent.futures import FIRST_COMPLETED, Future, ThreadPoolExecutor, wait
from pathlib import Path
from typing import Any, Callable
import math, sys, time
import community_shard_six_machine as qual
import scaling_controller as sc
import overlap_transport

FIXED_WORKER_COUNT, FIXED_TILE_READY, MAX_ACTIVE_SESSIONS = 12, 2, 2
OVERLAP_DISPATCH_TIMEOUT_SECONDS = 310.0
READINESS_TIMEOUT_SECONDS = sc.READINESS_TIMEOUT_SECONDS
READINESS_POLL_INTERVAL_SECONDS = sc.READINESS_POLL_INTERVAL_SECONDS
SCHEDULER_POLL_INTERVAL_SECONDS = 0.25
SCHEDULER_TIMEOUT_SECONDS = 1800.0
DRAIN_TIMEOUT_SECONDS = 330.0
OVERLAP_ARM_SPECS: dict[str, tuple[int, int, int, int]] = {
    "baseline_before": (12, 2, 2, 6),
    "overlap": (12, 2, 4, 12),
    "baseline_after": (12, 2, 2, 6),
}

def _valid_seconds(value: Any, allow_zero: bool = False) -> bool:
    return type(value) in (int, float) and math.isfinite(value) and (value >= 0 if allow_zero else value > 0)

def _max_http_in_flight(intervals: list[tuple[int, int]]) -> int:
    events = sorted([(s, 1) for s, _ in intervals] + [(e, -1) for _, e in intervals])
    active = peak = 0
    for _, delta in events:
        active += delta
        peak = max(peak, active)
    return peak

def validate_live_overlap_status(status_payload: Any, expected_boot: str) -> tuple[dict[str, Any], int]:
    if not isinstance(status_payload, dict) or not isinstance(status_payload.get("status"), dict):
        raise qual.QualificationError("shards/status must be JSON object with status")
    inner = status_payload["status"]
    ready, prov, active = inner.get("ready_workers"), inner.get("provisioning_workers"), inner.get("active_sessions")
    if not (qual.is_strict_nonneg_int(ready) and qual.is_strict_nonneg_int(prov) and qual.is_strict_nonneg_int(active)):
        raise qual.QualificationError("shards/status worker/session counts invalid")
    if ready + prov > FIXED_WORKER_COUNT:
        raise qual.QualificationError(f"external worker interference: ready={ready} provisioning={prov} > {FIXED_WORKER_COUNT}")
    if active > MAX_ACTIVE_SESSIONS:
        raise qual.QualificationError(f"shards/status active_sessions={active} exceeds max {MAX_ACTIVE_SESSIONS}")
    if "retained_results" in inner and not qual.is_strict_nonneg_int(inner["retained_results"]):
        raise qual.QualificationError("shards/status retained_results must be non-negative int")
    validated = qual.validate_shard_status_payload(status_payload, FIXED_WORKER_COUNT, 0, active, FIXED_TILE_READY, 0)
    sc._verify_status_pins_and_health(validated, expected_boot)
    if not sc._tiles_at_target(validated, FIXED_TILE_READY):
        raise qual.QualificationError("shards/status tiles deviated from fixed target")
    return validated, active

def _default_read_status(deadline_s: float, get_monotonic: Callable[[], float]) -> Any:
    remaining = deadline_s - get_monotonic()
    if remaining <= 0:
        raise qual.QualificationError("scheduler deadline expired before status poll")
    return overlap_transport.get_json(qual.SHARD_STATUS_PATH, deadline_s, get_monotonic)

def wait_for_overlap_arm_readiness(
    name: str, expected_boot_nonce: str, timeout_seconds: float = READINESS_TIMEOUT_SECONDS,
    poll_interval_seconds: float = READINESS_POLL_INTERVAL_SECONDS,
    monotonic_fn: Callable[[], float] | None = None, sleep_fn: Callable[[float], None] | None = None,
    readiness_fn: Callable[[float], Any] | None = None, status_fn: Callable[[float], Any] | None = None,
) -> tuple[dict[str, Any], dict[str, Any]]:
    if name not in OVERLAP_ARM_SPECS or not _valid_seconds(timeout_seconds) or not _valid_seconds(poll_interval_seconds):
        raise qual.QualificationError(f"invalid readiness wait config for arm {name!r}")
    expected_boot = qual.normalize_hex64(expected_boot_nonce, "expected_boot_nonce")
    expected_workers, tile_ready, _, _ = OVERLAP_ARM_SPECS[name]
    get_monotonic = time.monotonic if monotonic_fn is None else monotonic_fn
    do_sleep = time.sleep if sleep_fn is None else sleep_fn
    get_readiness = (lambda dl: overlap_transport.get_json(qual.READINESS_PATH, dl, get_monotonic)) if readiness_fn is None else readiness_fn
    get_status = (lambda dl: _default_read_status(dl, get_monotonic)) if status_fn is None else status_fn
    deadline, last_counts = get_monotonic() + timeout_seconds, None
    while True:
        if deadline - get_monotonic() <= 0:
            raise qual.QualificationError(f"arm {name!r} readiness timed out: last_counts={last_counts}")
        readiness = get_readiness(deadline)
        status = get_status(deadline)
        last_counts = qual.assert_status_no_external_interference(status, max_workers=expected_workers)
        sc._verify_status_pins_and_health(status, expected_boot)
        if isinstance(readiness, dict) and "boot_nonce" in readiness:
            sc._verify_boot_nonce(readiness["boot_nonce"], "dispatch_readiness.boot_nonce", expected_boot)
        expect_ready = isinstance(readiness, dict) and readiness.get("sharded_pipeline_ready") is True
        valid_readiness = qual.validate_readiness_payload(readiness, expect_ready=expect_ready)
        if expect_ready and last_counts == (expected_workers, 0, 0) and sc._tiles_at_target(status, tile_ready):
            return valid_readiness, qual.validate_shard_status_payload(status, expected_workers, 0, 0, tile_ready, 0)
        sleep_for = min(poll_interval_seconds, max(0.0, deadline - get_monotonic()))
        if sleep_for <= 0:
            raise qual.QualificationError(f"arm {name!r} readiness timed out: last_counts={last_counts}")
        do_sleep(sleep_for)

def _bounded_dispatch(*args: Any) -> dict[str, Any]:
    return sc._dispatch_one(*args, request_once_fn=overlap_transport.request_once)

def _wait_for_poll_or_completion(
    in_flight: set[Future[dict[str, Any]]], slice_s: float, sleep_fn: Callable[[float], None] | None,
) -> None:
    if slice_s <= 0 or any(fut.done() for fut in in_flight):
        return
    if sleep_fn is not None:
        if in_flight:
            wait(in_flight, timeout=0.001, return_when=FIRST_COMPLETED)
            if any(fut.done() for fut in in_flight):
                return
        sleep_fn(slice_s)
    elif in_flight:
        wait(in_flight, timeout=slice_s, return_when=FIRST_COMPLETED)
    else:
        time.sleep(slice_s)

def _normalize_prior_sessions(prior_session_keys: set[tuple[str, int]] | None) -> set[tuple[str, int]]:
    if prior_session_keys is None:
        return set()
    if not isinstance(prior_session_keys, set):
        raise qual.QualificationError("prior_session_keys must be a set")
    out: set[tuple[str, int]] = set()
    for item in prior_session_keys:
        if not isinstance(item, tuple) or len(item) != 2 or not qual.is_strict_nonneg_int(item[1]) or item[1] > 2**64 - 1:
            raise qual.QualificationError("invalid prior session key")
        out.add((qual.normalize_hex64(item[0], "prior_session.boot_nonce"), item[1]))
    return out

def _execute_continuous_overlap(
    store: sc.EvidenceStore, name: str, offered_cap: int, request_count: int, expected_workers: set[str],
    expected_boot: str, get_monotonic: Callable[[], float], get_ns: Callable[[], int],
    sleep_fn: Callable[[float], None] | None, read_status: Callable[[float], Any],
    dispatch_fn: Callable[..., dict[str, Any]], dispatch_timeout_seconds: float,
    scheduler_timeout_seconds: float, poll_interval_seconds: float, drain_timeout_seconds: float,
    expected_baseline_response: dict[str, Any] | None, prior_session_keys: set[tuple[str, int]],
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    outcomes: list[dict[str, Any]] = []
    seen_sessions: dict[tuple[str, int], str] = {key: "previous arm" for key in prior_session_keys}
    baseline_resp, baseline_id = expected_baseline_response, "previous arm"
    in_flight: set[Future[dict[str, Any]]] = set()
    future_meta: dict[Future[dict[str, Any]], dict[str, Any]] = {}
    pacing_by_index: dict[int, dict[str, Any]] = {}
    pending_launch: tuple[Future[dict[str, Any]], int, int] | None = None
    first_error: qual.QualificationError | None = None
    status_polls = status_rise_unblocks = completion_unblocks = 0
    arm_queued_ns = get_ns()
    if not qual.is_strict_positive_int(arm_queued_ns):
        raise qual.QualificationError(f"{name}: arm_queued_ns must be strict positive int")
    next_hol_ns, next_index = arm_queued_ns, 1
    deadline = get_monotonic() + scheduler_timeout_seconds

    def process_future(fut: Future[dict[str, Any]]) -> None:
        nonlocal baseline_resp, baseline_id, pending_launch, first_error, completion_unblocks, next_hol_ns
        meta = future_meta[fut]
        failure = fut.exception()
        if failure is not None:
            err = qual.QualificationError(f"{name}-{meta['index']}: dispatch callback failed: {type(failure).__name__}: {failure}")
            err.__cause__ = failure
            store.fail_if_started(f"{name}-{meta['index']}", get_ns(), str(err))
            item = {"index": meta["index"], "error": err}
        else:
            item = fut.result()
        if pending_launch is not None and fut is pending_launch[0]:
            pending_launch = None
            completion_unblocks += 1
            meta["unblocked_by"] = "future_completed"
            next_hol_ns = get_ns()
        if item["error"] is not None:
            first_error = item["error"] if first_error is None else first_error
            outcomes.append(item)
            return
        req_id, skey = item["request"]["id"], item["session_key"]
        val_err: qual.QualificationError | None = None
        if skey in seen_sessions:
            val_err = qual.QualificationError(f"{req_id}: duplicate session_id {skey} seen on {seen_sessions[skey]}")
        else:
            seen_sessions[skey] = req_id
            if baseline_resp is None:
                baseline_resp, baseline_id = item["validated"], req_id
            else:
                val_err = sc._check_consistency(baseline_resp, item["validated"], baseline_id, req_id)
        req = item["request"]
        if val_err is None and not arm_queued_ns <= meta["hol_ns"] <= meta["submitted_ns"] <= req["start_ns"] < req["end_ns"]:
            val_err = qual.QualificationError(f"{req_id}: invalid queue and HTTP timing order")
        if val_err is not None:
            store.update_attempt(req_id, status="failed", error=str(val_err))
            item["error"] = val_err
            first_error = val_err if first_error is None else first_error
        else:
            store.update_attempt(req_id, status="succeeded")
            pacing_by_index[meta["index"]] = {
                "id": req_id, "queued_at_ns": arm_queued_ns, "head_of_line_at_ns": meta["hol_ns"],
                "submitted_at_ns": meta["submitted_ns"], "http_start_ns": req["start_ns"], "http_end_ns": req["end_ns"],
                "client_queue_wait_ns": req["start_ns"] - arm_queued_ns,
                "head_of_line_wait_ns": meta["submitted_ns"] - meta["hol_ns"],
                "http_interval_ns": req["end_ns"] - req["start_ns"],
                "prelaunch_active_sessions": meta["prelaunch_active"], "unblocked_by": meta["unblocked_by"],
            }
        outcomes.append(item)

    def harvest_done() -> None:
        done = sorted((f for f in in_flight if f.done()), key=lambda f: future_meta[f]["index"])
        for fut in done:
            in_flight.remove(fut)
            process_future(fut)

    executor = ThreadPoolExecutor(max_workers=offered_cap)
    completed = False
    pending_submission = None
    store.set_fields(undrained_in_flight_count=0, untracked_submission_possible=False)
    try:
        while len(outcomes) < request_count:
            harvest_done()
            if first_error is not None:
                raise first_error
            if len(outcomes) == request_count:
                break
            remaining = deadline - get_monotonic()
            if remaining <= 0:
                raise qual.QualificationError(f"arm {name!r} exceeded scheduling deadline")
            raw_status = read_status(deadline)
            status_polls += 1
            harvest_done()
            if first_error is not None:
                raise first_error
            _, active_sessions = validate_live_overlap_status(raw_status, expected_boot)
            if len(outcomes) == request_count:
                break
            if get_monotonic() >= deadline:
                raise qual.QualificationError(f"arm {name!r} status poll exhausted scheduling deadline")
            if pending_launch is not None and active_sessions > pending_launch[2]:
                p_fut = pending_launch[0]
                pending_launch = None
                status_rise_unblocks += 1
                future_meta[p_fut]["unblocked_by"] = "status_active_rise"
                next_hol_ns = get_ns()
            if next_index <= request_count and pending_launch is None and active_sessions < MAX_ACTIVE_SESSIONS and len(in_flight) < offered_cap:
                idx = next_index
                next_index += 1
                hol_ns, submitted_ns = next_hol_ns, get_ns()
                is_final = idx == request_count
                meta = {
                    "index": idx, "hol_ns": hol_ns, "submitted_ns": submitted_ns,
                    "prelaunch_active": active_sessions,
                    "unblocked_by": "final_request" if is_final else "pending",
                }
                pending_submission = idx
                fut = executor.submit(
                    dispatch_fn, store, name, idx,
                    expected_workers, expected_boot, get_ns, dispatch_timeout_seconds,
                )
                future_meta[fut] = meta
                in_flight.add(fut)
                pending_submission = None
                if not is_final:
                    pending_launch = (fut, idx, active_sessions)
            slice_s = min(poll_interval_seconds, max(0.0, deadline - get_monotonic()))
            _wait_for_poll_or_completion(in_flight, slice_s, sleep_fn)
        completed = True
    finally:
        pending_error = sys.exception() if not completed else None
        try:
            if not completed:
                done_drain, still_running = wait(in_flight, timeout=drain_timeout_seconds) if in_flight else (set(), set())
                store.set_fields(undrained_in_flight_count=len(still_running))
                for fut in sorted(done_drain, key=lambda f: future_meta[f]["index"]):
                    in_flight.remove(fut)
                    process_future(fut)
                if still_running and isinstance(pending_error, Exception):
                    raise qual.QualificationError(
                        f"{pending_error}; {len(still_running)} in-flight callback(s) exceeded drain bound {drain_timeout_seconds}s"
                    ) from pending_error
        finally:
            running = sum(not future.done() for future in in_flight)
            untracked = pending_submission is not None and not any(future in in_flight and meta["index"] == pending_submission for future, meta in future_meta.items())
            try:
                if not completed:
                    store.set_fields(undrained_in_flight_count=None if untracked else running,
                                     untracked_submission_possible=untracked)
            finally:
                executor.shutdown(wait=(running == 0 and not untracked), cancel_futures=True)
    ordered = sorted(outcomes, key=lambda item: item["index"])
    intervals = [(item["request"]["start_ns"], item["request"]["end_ns"]) for item in ordered]
    observed_peak = _max_http_in_flight(intervals)
    if observed_peak > offered_cap:
        raise qual.QualificationError(f"arm {name!r} observed HTTP peak {observed_peak} > cap {offered_cap}")
    pacing = {
        "arm": name, "offered_concurrency_cap": offered_cap, "request_count": request_count,
        "max_active_shard_sessions": MAX_ACTIVE_SESSIONS, "dispatch_timeout_seconds": dispatch_timeout_seconds,
        "client_timeout_is_not_server_deadline": True, "http_intervals_are_not_server_admission": True,
        "observed_client_http_in_flight_peak": observed_peak, "status_poll_count": status_polls,
        "status_rise_unblocks": status_rise_unblocks, "future_completion_unblocks": completion_unblocks,
        "requests": [pacing_by_index[idx] for idx in range(1, request_count + 1)],
    }
    return ordered, pacing

def run_overlap_arm(
    name: str, expected_worker_ids: set[str], evidence_path: Path, expected_boot_nonce: str, *,
    monotonic_fn: Callable[[], float] | None = None, monotonic_ns_fn: Callable[[], int] | None = None,
    sleep_fn: Callable[[float], None] | None = None,
    expected_baseline_response: dict[str, Any] | None = None,
    prior_session_keys: set[tuple[str, int]] | None = None,
    readiness_fn: Callable[[float], Any] | None = None, status_fn: Callable[[float], Any] | None = None,
    dispatch_fn: Callable[..., dict[str, Any]] | None = None,
    dispatch_timeout_seconds: float = OVERLAP_DISPATCH_TIMEOUT_SECONDS,
    scheduler_timeout_seconds: float = SCHEDULER_TIMEOUT_SECONDS,
    poll_interval_seconds: float = SCHEDULER_POLL_INTERVAL_SECONDS,
    drain_timeout_seconds: float = DRAIN_TIMEOUT_SECONDS,
) -> dict[str, Any]:
    if name not in OVERLAP_ARM_SPECS or not isinstance(expected_worker_ids, set) or not isinstance(evidence_path, Path):
        raise qual.QualificationError(f"invalid run_overlap_arm arguments for arm {name!r}")
    if not (_valid_seconds(dispatch_timeout_seconds) and dispatch_timeout_seconds <= OVERLAP_DISPATCH_TIMEOUT_SECONDS and _valid_seconds(scheduler_timeout_seconds) and _valid_seconds(poll_interval_seconds) and _valid_seconds(drain_timeout_seconds, allow_zero=True)):
        raise qual.QualificationError(f"invalid timeout/cadence config for arm {name!r}")
    expected_workers, _, offered_cap, request_count = OVERLAP_ARM_SPECS[name]
    normalized_workers = {qual.normalize_hex64(wid, "expected_worker_id") for wid in expected_worker_ids}
    if len(normalized_workers) != expected_workers:
        raise qual.QualificationError(f"arm {name!r} requires {expected_workers} distinct worker IDs")
    normalized_boot = qual.normalize_hex64(expected_boot_nonce, "expected_boot_nonce")
    normalized_prior = _normalize_prior_sessions(prior_session_keys)
    get_monotonic = time.monotonic if monotonic_fn is None else monotonic_fn
    get_ns = time.monotonic_ns if monotonic_ns_fn is None else monotonic_ns_fn
    read_status = (lambda dl: _default_read_status(dl, get_monotonic)) if status_fn is None else status_fn
    do_dispatch = _bounded_dispatch if dispatch_fn is None else dispatch_fn
    store = sc.EvidenceStore(evidence_path, name, offered_cap, sorted(normalized_workers), normalized_boot)
    store.set_fields(
        request_target_count=request_count, dispatch_timeout_seconds=dispatch_timeout_seconds,
        experiment_mode="overlap",
        client_timeout_is_not_server_deadline=True, http_intervals_are_not_server_admission=True,
    )
    completed = False
    try:
        readiness, status = wait_for_overlap_arm_readiness(
            name, normalized_boot, monotonic_fn=get_monotonic, sleep_fn=sleep_fn,
            readiness_fn=readiness_fn, status_fn=status_fn,
        )
        store.set_fields(readiness=readiness, status=status)
        ordered, pacing = _execute_continuous_overlap(
            store, name, offered_cap, request_count, normalized_workers, normalized_boot,
            get_monotonic, get_ns, sleep_fn, read_status, do_dispatch, dispatch_timeout_seconds,
            scheduler_timeout_seconds, poll_interval_seconds, drain_timeout_seconds,
            expected_baseline_response, normalized_prior,
        )
        if len(ordered) != request_count or any(item["error"] is not None for item in ordered):
            raise qual.QualificationError(f"arm {name!r} incomplete dispatches")
        participated = {wid for item in ordered for wid in item["request"]["worker_ids"]}
        if participated != normalized_workers:
            raise qual.QualificationError(f"missing arm workers: {sorted(normalized_workers - participated)}")
        completed = True
    finally:
        if not completed:
            failure = sys.exception()
            store.set_fields(arm_status="failed", error=f"{type(failure).__name__}: {failure}", completed_at_utc=qual.utc_now_iso())
    store.set_fields(arm_status="succeeded", completed_at_utc=qual.utc_now_iso(), pacing=pacing)
    return {"workers": sorted(normalized_workers), "requests": [item["request"] for item in ordered]}
