from concurrent.futures import ThreadPoolExecutor, wait
from pathlib import Path
from typing import Any, Callable
import copy, errno, http.client, json, os, socket, ssl, tempfile, threading, time, urllib.error
import community_shard_six_machine as qual

REQUEST_COUNT, PARTICIPANT_COUNT = 6, 6
READINESS_TIMEOUT_SECONDS, READINESS_POLL_INTERVAL_SECONDS, DISPATCH_TIMEOUT_SECONDS = 300.0, 2.0, 90.0
ARM_SPECS: dict[str, tuple[int, int, int]] = {"before": (6, 1, 1), "expanded": (12, 2, 2), "after": (6, 1, 1)}
TRANSPORT_EXCEPTIONS = (
    urllib.error.URLError, http.client.HTTPException, TimeoutError,
    ConnectionError, ssl.SSLError, socket.gaierror, socket.herror, json.JSONDecodeError,
)
EXPECTED_EXCEPTIONS = (qual.QualificationError, *TRANSPORT_EXCEPTIONS)

def _create_exclusive_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    rendered = json.dumps(payload, indent=2, sort_keys=True) + "\n"
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
    try:
        fd = os.open(str(path), flags, 0o600)
    except (FileExistsError, IsADirectoryError) as exc:
        raise qual.QualificationError(f"evidence path already exists: {path}") from exc
    except OSError as exc:
        if exc.errno in (errno.EEXIST, errno.ELOOP):
            raise qual.QualificationError(f"evidence path already exists or is symlink: {path}") from exc
        raise
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        handle.write(rendered)
        handle.flush()
        os.fsync(handle.fileno())

def atomic_write_json(path: Path, payload: dict[str, Any]) -> None:
    rendered = json.dumps(payload, indent=2, sort_keys=True) + "\n"
    fd, tmp_name = tempfile.mkstemp(dir=str(path.parent), prefix=f".{path.name}.", suffix=".tmp")
    tmp_path, replaced = Path(tmp_name), False
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(rendered)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(tmp_path, path)
        replaced = True
    finally:
        if not replaced and tmp_path.exists():
            tmp_path.unlink()

class EvidenceStore:
    def __init__(self, path: Path, name: str, max_conc: int, workers: list[str], boot_nonce: str) -> None:
        self._path, self._lock, self._attempts = path, threading.Lock(), {}
        self._record: dict[str, Any] = {
            "arm": name, "arm_status": "running", "classification": "ephemeral_ci",
            "counts_toward_operator_target": False, "verification_basis": "server_reported_only",
            "expected_boot_nonce": boot_nonce, "expected_worker_ids": list(workers),
            "max_concurrency": max_conc, "started_at_utc": qual.utc_now_iso(),
            "completed_at_utc": None, "error": None, "readiness": None, "status": None, "attempts": [],
        }
        with self._lock:
            _create_exclusive_json(self._path, self._record)

    def _persist_locked(self) -> None:
        ordered = sorted(self._attempts.values(), key=lambda item: item["request_index"])
        self._record["attempts"] = [copy.deepcopy(item) for item in ordered]
        atomic_write_json(self._path, self._record)

    def set_fields(self, **fields: Any) -> None:
        with self._lock:
            for key, value in fields.items():
                self._record[key] = copy.deepcopy(value)
            self._persist_locked()

    def start_attempt(self, req_id: str, index: int, start_ns: int, payload: dict[str, Any]) -> None:
        with self._lock:
            self._attempts[req_id] = {
                "id": req_id, "request_index": index, "start_ns": start_ns, "end_ns": None,
                "request_payload": copy.deepcopy(payload), "http_status": None,
                "parsed_response": None, "dispatch_uncertain": True, "status": "in_flight", "error": None,
            }
            self._persist_locked()

    def update_attempt(self, req_id: str, **fields: Any) -> None:
        with self._lock:
            entry = self._attempts[req_id]
            for key, value in fields.items():
                entry[key] = copy.deepcopy(value)
            self._persist_locked()

def _verify_boot_nonce(raw_value: Any, label: str, expected: str) -> str:
    normalized = qual.normalize_hex64(raw_value, label)
    if normalized != expected:
        raise qual.QualificationError(f"{label}={normalized} != expected {expected}")
    return normalized

def _assert_no_drift_fields(container: dict[str, Any], label: str) -> None:
    for key, value in container.items():
        lower = key.lower()
        if "quarantin" not in lower and "cancel" not in lower:
            continue
        if isinstance(value, bool):
            drifted = value
        elif qual.is_strict_int(value):
            drifted = value != 0
        elif isinstance(value, (list, dict, set)):
            drifted = bool(value)
        else:
            drifted = value is not None
        if drifted:
            raise qual.QualificationError(f"{label}.{key} indicates drift: {value!r}")

def _verify_status_pins_and_health(status_payload: dict[str, Any], expected_boot: str) -> None:
    inner = status_payload["status"]
    if "boot_nonce" not in inner:
        raise qual.QualificationError("shards/status.status missing boot_nonce")
    _verify_boot_nonce(inner["boot_nonce"], "shards/status.status.boot_nonce", expected_boot)
    if "boot_nonce" in status_payload:
        _verify_boot_nonce(status_payload["boot_nonce"], "shards/status.boot_nonce", expected_boot)
    model_vals = [
        (lbl, obj["model_id"]) for lbl, obj in (("shards/status.model_id", status_payload), ("shards/status.status.model_id", inner))
        if "model_id" in obj
    ]
    if not model_vals:
        raise qual.QualificationError("shards/status missing model_id")
    for lbl, raw_mid in model_vals:
        if qual.normalize_hex64(raw_mid, lbl) != qual.EXPECTED_MODEL_ID:
            raise qual.QualificationError(f"{lbl} pin mismatch")
    prof_vals = [
        (lbl, obj["execution_profile"]) for lbl, obj in (("shards/status.execution_profile", status_payload), ("shards/status.status.execution_profile", inner))
        if "execution_profile" in obj
    ]
    if not prof_vals:
        raise qual.QualificationError("shards/status missing execution_profile")
    for lbl, raw_prof in prof_vals:
        if raw_prof != qual.CANONICAL_EXECUTION_PROFILE:
            raise qual.QualificationError(f"{lbl} pin mismatch")
    _assert_no_drift_fields(status_payload, "shards/status")
    _assert_no_drift_fields(inner, "shards/status.status")
    tiles = inner.get("tiles")
    if isinstance(tiles, list):
        for idx, tile in enumerate(tiles):
            if isinstance(tile, dict):
                _assert_no_drift_fields(tile, f"shards/status.status.tiles[{idx}]")

def _tiles_at_target(status_payload: dict[str, Any], tile_ready: int) -> bool:
    tiles = status_payload["status"].get("tiles")
    return isinstance(tiles, list) and len(tiles) == len(qual.EXPECTED_SIX_RANGES) and all(
        isinstance(t, dict) and qual.is_strict_int(t.get("ready_workers")) and t["ready_workers"] == tile_ready
        and qual.is_strict_int(t.get("provisioning_workers")) and t["provisioning_workers"] == 0
        for t in tiles
    )

def wait_for_arm_readiness(
    name: str, expected_boot_nonce: str, timeout_seconds: float = READINESS_TIMEOUT_SECONDS,
    poll_interval_seconds: float = READINESS_POLL_INTERVAL_SECONDS,
    monotonic_fn: Callable[[], float] | None = None, sleep_fn: Callable[[float], None] | None = None,
) -> tuple[dict[str, Any], dict[str, Any]]:
    if name not in ARM_SPECS or timeout_seconds <= 0 or poll_interval_seconds <= 0:
        raise qual.QualificationError(f"invalid readiness wait config for arm {name!r}")
    expected_boot = qual.normalize_hex64(expected_boot_nonce, "expected_boot_nonce")
    expected_workers, tile_ready, _ = ARM_SPECS[name]
    get_monotonic = time.monotonic if monotonic_fn is None else monotonic_fn
    do_sleep = time.sleep if sleep_fn is None else sleep_fn
    deadline, last_counts = get_monotonic() + timeout_seconds, None
    while True:
        if deadline - get_monotonic() <= 0:
            raise qual.QualificationError(f"arm {name!r} readiness timed out: last_counts={last_counts}")
        readiness = qual.coordinator_get_json_with_retries(qual.READINESS_PATH, deadline)
        status = qual.coordinator_get_json_with_retries(qual.SHARD_STATUS_PATH, deadline)
        transitioning_workers = ARM_SPECS['expanded'][0] if name == 'after' else expected_workers
        last_counts = qual.assert_status_no_external_interference(status, max_workers=transitioning_workers)
        _verify_status_pins_and_health(status, expected_boot)
        if isinstance(readiness, dict) and "boot_nonce" in readiness:
            _verify_boot_nonce(readiness["boot_nonce"], "dispatch_readiness.boot_nonce", expected_boot)
        expect_ready = isinstance(readiness, dict) and readiness.get("sharded_pipeline_ready") is True
        valid_readiness = qual.validate_readiness_payload(readiness, expect_ready=expect_ready)
        if expect_ready and last_counts == (expected_workers, 0, 0) and _tiles_at_target(status, tile_ready):
            return valid_readiness, qual.validate_shard_status_payload(status, expected_workers, 0, 0, tile_ready, 0)
        sleep_for = min(poll_interval_seconds, max(0.0, deadline - get_monotonic()))
        if sleep_for <= 0:
            raise qual.QualificationError(f"arm {name!r} readiness timed out: last_counts={last_counts}")
        do_sleep(sleep_for)

def _extract_participant_subset(response: Any, expected_workers: set[str]) -> set[str]:
    if not isinstance(response, dict):
        raise qual.QualificationError("dispatch response must be JSON object")
    participants = response.get("participants")
    if not isinstance(participants, list) or len(participants) != PARTICIPANT_COUNT:
        raise qual.QualificationError(f"dispatch response must have {PARTICIPANT_COUNT} participants")
    seen: set[str] = set()
    for item in participants:
        if not isinstance(item, dict) or "worker_id" not in item:
            raise qual.QualificationError("dispatch response malformed participant entry")
        wid = qual.normalize_hex64(item["worker_id"], "participant.worker_id")
        if wid in seen:
            raise qual.QualificationError(f"duplicate participant worker_id: {wid}")
        seen.add(wid)
    foreign = sorted(seen - expected_workers)
    if foreign:
        raise qual.QualificationError(f"foreign participant worker_ids: {foreign}")
    return seen

def _check_consistency(base: dict[str, Any], cur: dict[str, Any], base_id: str, req_id: str) -> qual.QualificationError | None:
    if cur["generated_tokens"] != base["generated_tokens"]:
        return qual.QualificationError(f"{req_id}: generated_tokens differed from {base_id}")
    if cur["tokens_generated"] != base["tokens_generated"]:
        return qual.QualificationError(f"{req_id}: tokens_generated differed from {base_id}")
    if cur["output"] != base["output"]:
        return qual.QualificationError(f"{req_id}: output differed from {base_id}")
    if qual.normalize_hex64(cur["output_hash"], "cur.output_hash") != qual.normalize_hex64(base["output_hash"], "base.output_hash"):
        return qual.QualificationError(f"{req_id}: output_hash differed from {base_id}")
    if cur["stopped_by_eos"] != base["stopped_by_eos"]:
        return qual.QualificationError(f"{req_id}: stopped_by_eos differed from {base_id}")
    return None

def _dispatch_one(
    store: EvidenceStore, arm_name: str, index: int, expected_workers: set[str],
    expected_boot: str, get_monotonic_ns: Callable[[], int],
) -> dict[str, Any]:
    req_id, payload = f"{arm_name}-{index}", copy.deepcopy(qual.DISPATCH_PAYLOAD)
    start_ns = get_monotonic_ns()
    store.start_attempt(req_id, index, start_ns, payload)
    if not qual.is_strict_positive_int(start_ns):
        err = qual.QualificationError(f"{req_id}: start_ns must be strict positive int")
        store.update_attempt(req_id, status="failed", error=str(err))
        return {"index": index, "error": err}
    try:
        http_status, parsed_response, _ = qual.coordinator_request_once(
            "POST", qual.DISPATCH_PATH, DISPATCH_TIMEOUT_SECONDS, json_body=payload,
        )
    except EXPECTED_EXCEPTIONS as exc:
        end_ns = get_monotonic_ns()
        err = exc if isinstance(exc, qual.QualificationError) else qual.QualificationError(f"{req_id}: {type(exc).__name__}: {exc}")
        if err is not exc:
            err.__cause__ = exc
        store.update_attempt(req_id, end_ns=end_ns, dispatch_uncertain=True, status="failed", error=str(err))
        return {"index": index, "error": err}
    end_ns = get_monotonic_ns()
    store.update_attempt(req_id, end_ns=end_ns, http_status=http_status, parsed_response=parsed_response, dispatch_uncertain=False, status="received")
    try:
        if not (qual.is_strict_positive_int(end_ns) and end_ns > start_ns):
            raise qual.QualificationError(f"{req_id}: client timing requires strict positive end_ns > start_ns")
        if not qual.is_strict_int(http_status) or http_status != 200:
            raise qual.QualificationError(f"{req_id}: unexpected HTTP status {http_status!r}")
        participant_ids = _extract_participant_subset(parsed_response, expected_workers)
        validated = qual.validate_dispatch_response(parsed_response, participant_ids)
        session_boot = _verify_boot_nonce(validated["session_id"]["boot_nonce"], "session_id.boot_nonce", expected_boot)
        output_hash = qual.normalize_hex64(validated["output_hash"], "response.output_hash")
        session_key = (session_boot, validated["session_id"]["seq"])
    except qual.QualificationError as exc:
        store.update_attempt(req_id, status="failed", error=str(exc))
        return {"index": index, "error": exc}
    store.update_attempt(req_id, status="response_validated")
    return {
        "index": index, "error": None, "session_key": session_key, "validated": validated,
        "request": {"id": req_id, "start_ns": start_ns, "end_ns": end_ns, "success": True,
                    "output_hash": output_hash, "worker_ids": sorted(participant_ids), "server_verification_validated": True},
    }

def _execute_waves(
    store: EvidenceStore, name: str, max_conc: int, expected_workers: set[str],
    expected_boot: str, get_monotonic_ns: Callable[[], int],
    expected_baseline_response: dict[str, Any] | None = None,
    prior_session_keys: set[tuple[str, int]] | None = None,
) -> list[dict[str, Any]]:
    outcomes: list[dict[str, Any]] = []
    seen_sessions: dict[tuple[str, int], str] = {key: "previous arm" for key in (prior_session_keys or set())}
    baseline_resp, baseline_id = expected_baseline_response, "previous arm"
    indices = list(range(1, REQUEST_COUNT + 1))
    with ThreadPoolExecutor(max_workers=max_conc) as executor:
        for pos in range(0, REQUEST_COUNT, max_conc):
            futures = [
                executor.submit(_dispatch_one, store, name, idx, expected_workers, expected_boot, get_monotonic_ns)
                for idx in indices[pos : pos + max_conc]
            ]
            wait(futures)
            wave_outcomes = [future.result() for future in futures]
            first_error: qual.QualificationError | None = None
            for item in wave_outcomes:
                if item["error"] is not None:
                    first_error = item["error"] if first_error is None else first_error
                    continue
                req_id, skey = item["request"]["id"], item["session_key"]
                wave_err: qual.QualificationError | None = None
                if skey in seen_sessions:
                    wave_err = qual.QualificationError(f"{req_id}: duplicate session_id {skey} seen on {seen_sessions[skey]}")
                else:
                    seen_sessions[skey] = req_id
                    if baseline_resp is None:
                        baseline_resp, baseline_id = item["validated"], req_id
                    else:
                        wave_err = _check_consistency(baseline_resp, item["validated"], baseline_id, req_id)
                if wave_err is not None:
                    store.update_attempt(req_id, status="failed", error=str(wave_err))
                    item["error"] = wave_err
                    first_error = wave_err if first_error is None else first_error
                else:
                    store.update_attempt(req_id, status="succeeded")
            outcomes.extend(wave_outcomes)
            if first_error is not None:
                raise first_error
    return outcomes

def run_arm(
    name: str, expected_worker_ids: set[str], evidence_path: Path, expected_boot_nonce: str, *,
    monotonic_fn: Callable[[], float] | None = None, monotonic_ns_fn: Callable[[], int] | None = None,
    sleep_fn: Callable[[float], None] | None = None,
    expected_baseline_response: dict[str, Any] | None = None,
    prior_session_keys: set[tuple[str, int]] | None = None,
) -> dict[str, Any]:
    if name not in ARM_SPECS or not isinstance(expected_worker_ids, set) or not isinstance(evidence_path, Path):
        raise qual.QualificationError(f"invalid run_arm arguments for arm {name!r}")
    expected_workers, _, max_conc = ARM_SPECS[name]
    normalized_workers = {qual.normalize_hex64(wid, "expected_worker_id") for wid in expected_worker_ids}
    if len(normalized_workers) != expected_workers:
        raise qual.QualificationError(f"arm {name!r} requires {expected_workers} distinct worker IDs")
    normalized_boot = qual.normalize_hex64(expected_boot_nonce, "expected_boot_nonce")
    get_ns = time.monotonic_ns if monotonic_ns_fn is None else monotonic_ns_fn
    store = EvidenceStore(evidence_path, name, max_conc, sorted(normalized_workers), normalized_boot)
    try:
        readiness, status = wait_for_arm_readiness(name, normalized_boot, monotonic_fn=monotonic_fn, sleep_fn=sleep_fn)
        store.set_fields(readiness=readiness, status=status)
        outcomes = _execute_waves(store, name, max_conc, normalized_workers, normalized_boot, get_ns, expected_baseline_response, prior_session_keys)
        if len(outcomes) != REQUEST_COUNT or any(item["error"] is not None for item in outcomes):
            raise qual.QualificationError(f"arm {name!r} incomplete dispatches")
        ordered = sorted(outcomes, key=lambda item: item["index"])
        participated = {wid for item in ordered for wid in item["request"]["worker_ids"]}
        if participated != normalized_workers:
            raise qual.QualificationError(f"missing arm workers: {sorted(normalized_workers - participated)}")
    except EXPECTED_EXCEPTIONS as exc:
        err = exc if isinstance(exc, qual.QualificationError) else qual.QualificationError(f"arm {name!r}: {type(exc).__name__}: {exc}")
        store.set_fields(arm_status="failed", error=str(err), completed_at_utc=qual.utc_now_iso())
        if err is exc:
            raise
        raise err from exc
    store.set_fields(arm_status="succeeded", completed_at_utc=qual.utc_now_iso())
    return {"workers": sorted(normalized_workers), "requests": [item["request"] for item in ordered]}
