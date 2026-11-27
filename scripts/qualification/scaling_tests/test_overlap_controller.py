import ast, json, sys, tempfile, threading, unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import community_shard_six_machine as qual
import overlap_controller as oc
import overlap_transport

SYNTHETIC_BOOT, SYNTHETIC_HASH = "ab" * 32, "cd" * 32
WORKERS_12 = [f"{i:064x}" for i in range(1, 13)]

def _readiness_payload() -> dict[str, Any]:
    return {"schema": qual.EXPECTED_READINESS_SCHEMA, "mutation_free_observation": True, "enabled": True,
            "protocol_version": qual.EXPECTED_PROTOCOL_VERSION, "verified_dispatch_enabled": True,
            "sharded_pipeline_ready": True, "safe_to_dispatch": True, "rewards_enabled": False,
            "chat_template_supported": True, "coordinator_id": qual.COORDINATOR_ID,
            "transaction_domain": qual.TRANSACTION_DOMAIN, "model_id": qual.EXPECTED_MODEL_ID,
            "execution_profile": qual.CANONICAL_EXECUTION_PROFILE, "max_prompt_bytes": 1000,
            "chat_template_overhead_bytes": 15, "max_tokens_cap": 64}

def _status_payload(active: int = 0, ready: int = 12, prov: int = 0, boot: str = SYNTHETIC_BOOT, tile_ready: int = 2, **extra: Any) -> dict[str, Any]:
    tiles = [{"range": {"start_layer": s, "end_layer": e}, "ready_workers": tile_ready, "provisioning_workers": 0} for s, e in qual.EXPECTED_SIX_RANGES]
    inner: dict[str, Any] = {"boot_nonce": boot, "model_id": qual.EXPECTED_MODEL_ID, "execution_profile": qual.CANONICAL_EXECUTION_PROFILE,
                             "ready_workers": ready, "provisioning_workers": prov, "active_sessions": active, "retained_results": 0, "tiles": tiles, **extra}
    return {"enabled": True, "protocol_version": qual.EXPECTED_PROTOCOL_VERSION, "verified_dispatch_enabled": True, "rewards_enabled": False, "status": inner}

def _response_payload(seq: int, workers: list[str], out_hash: str = SYNTHETIC_HASH, boot: str = SYNTHETIC_BOOT) -> dict[str, Any]:
    return {"session_id": {"boot_nonce": boot, "seq": seq}, "generated_tokens": [11, 22], "tokens_generated": 2,
            "output": "ok", "output_hash": out_hash, "stopped_by_eos": False, "participants": [{"worker_id": w} for w in workers]}

class FakeClock:
    def __init__(self, on_sleep: Any = None) -> None:
        self._lock, self.s, self.ns, self._on_sleep = threading.Lock(), 100.0, 1_000_000_000, on_sleep
    def monotonic(self) -> float:
        with self._lock:
            return self.s
    def monotonic_ns(self) -> int:
        with self._lock:
            self.ns += 1_000
            return self.ns
    def sleep(self, dt: float) -> None:
        with self._lock:
            self.s, self.ns = self.s + dt, self.ns + int(dt * 1_000_000_000)
            now = self.s
        if self._on_sleep:
            self._on_sleep(now)

def _slice_workers(idx: int) -> list[str]:
    return WORKERS_12[:6] if idx % 2 == 1 else WORKERS_12[6:]

class OverlapControllerTests(unittest.TestCase):
    def test_scheduler_evidence_reconstructs_with_real_response_validator(self) -> None:
        import experiment_profile as profiles
        import scaling_run
        from test_scaling_controller import make_dispatch_response
        clock = FakeClock()
        sequence = [0]
        def post(_method, _path, _timeout, **_kwargs):
            sequence[0] += 1
            return 200, make_dispatch_response(_slice_workers(sequence[0]), seq=sequence[0], boot_nonce=SYNTHETIC_BOOT), 10
        with tempfile.TemporaryDirectory() as td:
            path = Path(td) / 'attempts.json'
            with patch.object(overlap_transport, 'request_once', side_effect=post):
                arm = oc.run_overlap_arm('overlap', set(WORKERS_12), path, SYNTHETIC_BOOT,
                    monotonic_fn=clock.monotonic, monotonic_ns_fn=clock.monotonic_ns,
                    sleep_fn=clock.sleep, readiness_fn=lambda _dl: _readiness_payload(),
                    status_fn=lambda _dl: _status_payload())
            evidence = json.loads(path.read_text())
            self.assertEqual(arm, scaling_run.reconstruct_arm('overlap', evidence, set(WORKERS_12), SYNTHETIC_BOOT, profiles.OVERLAP))
            self.assertEqual(len(arm['requests']), 12)

    def test_expired_status_observation_cannot_authorize_a_post(self) -> None:
        clock = FakeClock()
        calls = [0]
        def status(_deadline):
            calls[0] += 1
            if calls[0] == 2:
                clock.sleep(1)
            return _status_payload()
        with tempfile.TemporaryDirectory() as td:
            path = Path(td) / 'expired.json'
            with patch.object(overlap_transport, 'request_once') as post:
                with self.assertRaisesRegex(qual.QualificationError, 'status poll exhausted'):
                    self._run(path, 'overlap', status, post, clock=clock, scheduler_timeout_seconds=0.5)
                post.assert_not_called()
            evidence = json.loads(path.read_text())
            self.assertEqual(evidence['attempts'], [])
            self.assertEqual(evidence['arm_status'], 'failed')

    def test_overlap_helpers_do_not_swallow_unknown_exceptions(self) -> None:
        root = Path(__file__).resolve().parents[1]
        for path in root.glob('overlap_*.py'):
            tree = ast.parse(path.read_text())
            for node in ast.walk(tree):
                if isinstance(node, ast.ExceptHandler):
                    names = {item.id for item in ast.walk(node.type) if isinstance(item, ast.Name)} if node.type else {'BaseException'}
                    self.assertFalse(names & {'Exception', 'BaseException'}, f'{path.name}:{node.lineno}: preserve failures and drain using finally instead of broad catches')

    def _run(self, path: Path, arm: str, status_fn: Any, post_fn: Any, clock: FakeClock | None = None, **kw: Any) -> dict[str, Any]:
        clk = FakeClock() if clock is None else clock
        with patch.object(overlap_transport, "request_once", side_effect=post_fn), patch.object(qual, "validate_dispatch_response", side_effect=lambda r, _p: r):
            result = oc.run_overlap_arm(arm, set(WORKERS_12), path, SYNTHETIC_BOOT, monotonic_fn=clk.monotonic, monotonic_ns_fn=clk.monotonic_ns,
                                       sleep_fn=clk.sleep, readiness_fn=lambda _d: _readiness_payload(), status_fn=status_fn, **kw)
        self.assertEqual(set(result), {"workers", "requests"})
        evidence = json.loads(path.read_text())
        self.assertEqual(evidence["experiment_mode"], "overlap")
        return {**result, "pacing": evidence["pacing"]}

    def test_overlap_refills_before_slow_http_ends_without_initial_burst(self) -> None:
        clock = FakeClock()
        entered, release = {i: threading.Event() for i in range(1, 5)}, {i: threading.Event() for i in range(1, 5)}
        started, timeouts, lock, seq_box = [], [], threading.Lock(), [0]
        def fake_post(_m: str, _p: str, timeout_s: float, **_: Any) -> tuple[int, Any, int]:
            with lock:
                seq_box[0] += 1
                idx = seq_box[0]
                started.append(idx)
                timeouts.append(timeout_s)
            if idx <= 4:
                entered[idx].set()
                self.assertTrue(release[idx].wait(5.0))
                clock.sleep(0.05)
            return 200, _response_payload(idx, _slice_workers(idx)), 50
        polls = [0]
        def fake_status(_dl: float) -> dict[str, Any]:
            polls[0] += 1
            p = polls[0]
            if p == 3:
                self.assertTrue(entered[1].wait(5.0))
                self.assertEqual(started, [1])
            elif p == 4:
                self.assertTrue(entered[2].wait(5.0))
                self.assertEqual(started, [1, 2])
            elif p == 5:
                self.assertEqual(started, [1, 2])
            elif p == 6:
                self.assertTrue(entered[3].wait(5.0))
            elif p == 8:
                self.assertTrue(entered[4].wait(5.0))
                for ev in release.values():
                    ev.set()
            return _status_payload({1: 0, 2: 0, 3: 1, 4: 2, 5: 1, 6: 2, 7: 1, 8: 2}.get(p, 1 if p % 2 == 0 else 0))
        with tempfile.TemporaryDirectory() as td:
            tmp = Path(td)
            res = self._run(tmp / "overlap.json", "overlap", fake_status, fake_post, clock=clock)
            b1 = self._run(tmp / "before.json", "baseline_before", lambda _d: _status_payload(0), fake_post, prior_session_keys={(SYNTHETIC_BOOT, i) for i in range(1, 13)})
            b2 = self._run(tmp / "after.json", "baseline_after", lambda _d: _status_payload(0), fake_post, prior_session_keys={(SYNTHETIC_BOOT, i) for i in range(1, 19)})
        self.assertEqual([len(b1["requests"]), len(res["requests"]), len(b2["requests"])], [6, 12, 6])
        self.assertTrue(all(t == 310.0 for t in timeouts))
        self.assertEqual(res["pacing"]["observed_client_http_in_flight_peak"], 4)
        self.assertIs(res["pacing"]["http_intervals_are_not_server_admission"], True)
        self.assertGreater(res["pacing"]["requests"][2]["client_queue_wait_ns"], 0)

    def test_conservative_missing_rise_waits_for_just_launched_future(self) -> None:
        entered, release = {1: threading.Event(), 2: threading.Event()}, {1: threading.Event(), 2: threading.Event()}
        done_1, started, lock, seq_box = threading.Event(), [], threading.Lock(), [0]
        def fake_post(_m: str, _p: str, _t: float, **_: Any) -> tuple[int, Any, int]:
            with lock:
                seq_box[0] += 1
                idx = seq_box[0]
                started.append(idx)
            if idx <= 2:
                entered[idx].set()
                self.assertTrue(release[idx].wait(5.0))
            if idx == 1:
                done_1.set()
            return 200, _response_payload(idx, _slice_workers(idx)), 10
        polls = [0]
        def fake_status(_dl: float) -> dict[str, Any]:
            polls[0] += 1
            p = polls[0]
            if p == 3:
                self.assertTrue(entered[1].wait(5.0))
            elif p == 4:
                self.assertTrue(entered[2].wait(5.0))
            elif p == 5:
                self.assertEqual(started, [1, 2])
                release[1].set()
                self.assertTrue(done_1.wait(5.0))
            elif p == 6:
                self.assertEqual(started, [1, 2])
                release[2].set()
            return _status_payload(1 if p in (3, 4) else 0)
        with tempfile.TemporaryDirectory() as td:
            res = self._run(Path(td) / "cons.json", "baseline_before", fake_status, fake_post)
        self.assertEqual(res["pacing"]["requests"][1]["unblocked_by"], "future_completed")

    def test_stop_drain_on_failures_guards_deadline_coverage_and_interrupt(self) -> None:
        for bad_st in (_status_payload(boot="11" * 32), _status_payload(ready=13), _status_payload(prov=1), _status_payload(active=3),
                       _status_payload(tile_ready=1), _status_payload(quarantined_workers=1), _status_payload(cancelled_sessions=1)):
            with self.assertRaises(qual.QualificationError):
                oc.validate_live_overlap_status(bad_st, SYNTHETIC_BOOT)
        with tempfile.TemporaryDirectory() as td:
            tmp = Path(td)
            base_resp = _response_payload(99, WORKERS_12[:6])
            outcomes = (qual.QualificationError("coordinator POST returned HTTP 429"), TimeoutError("310s timeout"),
                        RuntimeError("unexpected callback error"), _response_payload(99, _slice_workers(2)),
                        _response_payload(2, _slice_workers(2), out_hash="ef" * 32))
            for idx_case, outcome in enumerate(outcomes):
                e1, r1, started, polls = threading.Event(), threading.Event(), [], [0]
                def post_case(_m: str, _p: str, _t: float, _o=outcome, _e1=e1, _r1=r1, _st=started, **_: Any) -> tuple[int, Any, int]:
                    _st.append(len(_st) + 1)
                    if len(_st) == 1:
                        _e1.set()
                        _r1.wait(5.0)
                        return 200, _response_payload(1, _slice_workers(1)), 10
                    _r1.set()
                    if isinstance(_o, Exception):
                        raise _o
                    return 200, _o, 10
                def st_case(_dl: float, _p=polls, _e1=e1) -> dict[str, Any]:
                    _p[0] += 1
                    return (_e1.wait(5.0) and _status_payload(1)) if _p[0] == 3 else _status_payload(0)
                ev_path = tmp / f"case-{idx_case}.json"
                with self.assertRaises(qual.QualificationError):
                    self._run(ev_path, "overlap", st_case, post_case, expected_baseline_response=base_resp, prior_session_keys={(SYNTHETIC_BOOT, 99)})
                saved = json.loads(ev_path.read_text(encoding="utf-8"))
                self.assertEqual((started, saved["undrained_in_flight_count"], saved["attempts"][0]["status"]), ([1, 2], 0, "succeeded"))
            for mode, exc_cls in (("boot", qual.QualificationError), ("deadline", qual.QualificationError), ("interrupt", KeyboardInterrupt)):
                e1, r1, started, polls = threading.Event(), threading.Event(), [], [0]
                clk = FakeClock(on_sleep=lambda now, _r1=r1: _r1.set() if now >= 100.4 else None)
                def post_1(_m: str, _p: str, _t: float, _e1=e1, _r1=r1, _st=started, **_: Any) -> tuple[int, Any, int]:
                    _st.append(len(_st) + 1)
                    _e1.set()
                    _r1.wait(5.0)
                    return 200, _response_payload(1, _slice_workers(1)), 10
                def st_1(_dl: float, _m=mode, _p=polls, _e1=e1, _r1=r1) -> dict[str, Any]:
                    _p[0] += 1
                    if _p[0] >= 3:
                        _e1.wait(5.0)
                        if _m == "boot":
                            _r1.set()
                            return _status_payload(1, boot="11" * 32)
                        if _m == "interrupt":
                            _r1.set()
                            raise KeyboardInterrupt("caller stop")
                        return _status_payload(2)
                    return _status_payload(0)
                ev_path = tmp / f"mode-{mode}.json"
                with self.assertRaises(exc_cls):
                    self._run(ev_path, "overlap", st_1, post_1, clock=clk, scheduler_timeout_seconds=0.4)
                saved = json.loads(ev_path.read_text(encoding="utf-8"))
                self.assertEqual((started, saved["undrained_in_flight_count"], saved["attempts"][0]["status"]), ([1], 0, "succeeded"))
            seq = [0]
            with self.assertRaises(qual.QualificationError):
                self._run(tmp / "cov.json", "baseline_before", lambda _d: _status_payload(0), lambda _m, _p, _t, **_: (200, _response_payload(seq.__setitem__(0, seq[0] + 1) or seq[0], WORKERS_12[:6]), 10))

if __name__ == "__main__":
    unittest.main()
