from pathlib import Path
from typing import Any, Callable
from unittest.mock import patch
import json, os, tempfile, threading, unittest
import community_shard_six_machine as qual
import scaling_controller as sc

BOOT_NONCE, OTHER_NONCE, OUTPUT_HASH, ALT_HASH = "a" * 64, "b" * 64, "c" * 64, "d" * 64
PRODUCTION_FIXTURE_PATH = Path(__file__).resolve().parent / "fixtures" / "six_machine_production_response.json"

def make_workers(count: int, offset: int = 0) -> list[str]:
    return [f"{i:064x}" for i in range(1 + offset, 1 + offset + count)]

def make_readiness(ready: bool = True) -> dict[str, Any]:
    return {
        "schema": qual.EXPECTED_READINESS_SCHEMA, "mutation_free_observation": True, "enabled": True,
        "protocol_version": qual.EXPECTED_PROTOCOL_VERSION, "verified_dispatch_enabled": True,
        "sharded_pipeline_ready": ready, "safe_to_dispatch": ready, "rewards_enabled": False,
        "chat_template_supported": True, "coordinator_id": qual.COORDINATOR_ID,
        "transaction_domain": qual.TRANSACTION_DOMAIN, "model_id": qual.EXPECTED_MODEL_ID,
        "execution_profile": qual.CANONICAL_EXECUTION_PROFILE,
        "max_prompt_bytes": max(256, len(qual.DISPATCH_PAYLOAD["input"].encode("utf-8"))),
        "chat_template_overhead_bytes": 0, "max_tokens_cap": max(16, qual.DISPATCH_PAYLOAD["max_tokens"]),
    }

def make_status(
    ready: int, prov: int, active: int, tile_ready: int, tile_prov: int = 0, boot_nonce: str | None = BOOT_NONCE,
    model_id: str | None = qual.EXPECTED_MODEL_ID, execution_profile: str | None = qual.CANONICAL_EXECUTION_PROFILE,
    extra_inner: dict[str, Any] | None = None,
) -> dict[str, Any]:
    inner: dict[str, Any] = {
        "ready_workers": ready, "provisioning_workers": prov, "active_sessions": active,
        "tiles": [{"range": {"start_layer": s, "end_layer": e}, "ready_workers": tile_ready, "provisioning_workers": tile_prov} for s, e in qual.EXPECTED_SIX_RANGES],
    }
    if boot_nonce is not None:
        inner["boot_nonce"] = boot_nonce
    if extra_inner:
        inner.update(extra_inner)
    payload: dict[str, Any] = {
        "enabled": True, "protocol_version": qual.EXPECTED_PROTOCOL_VERSION,
        "verified_dispatch_enabled": True, "rewards_enabled": False, "status": inner,
    }
    if model_id is not None:
        payload["model_id"] = model_id
    if execution_profile is not None:
        payload["execution_profile"] = execution_profile
    return payload

def make_dispatch_response(
    workers: list[str], seq: int = 1, boot_nonce: str = BOOT_NONCE, output_hash: str = OUTPUT_HASH,
    output: str = "ok", stopped_by_eos: bool = False,
) -> dict[str, Any]:
    toks = [11] * qual.DISPATCH_PAYLOAD["max_tokens"]
    return {
        "protocol_version": qual.EXPECTED_PROTOCOL_VERSION, "session_id": {"boot_nonce": boot_nonce, "seq": seq},
        "model_name": "canonical-model", "model_id": qual.EXPECTED_MODEL_ID, "execution_profile": qual.CANONICAL_EXECUTION_PROFILE,
        "generated_tokens": toks, "tokens_generated": len(toks), "stopped_by_eos": stopped_by_eos, "output": output, "output_hash": output_hash,
        "total_ms": 999999, "participant_identity_basis": qual.EXPECTED_PARTICIPANT_IDENTITY_BASIS, "rewards_enabled": False, "reward_eligible": False,
        "participants": [{"worker_id": wid, "lease_id": {"boot_nonce": boot_nonce, "seq": i + 1}, "lease_epoch": 1, "range": {"start_layer": s, "end_layer": e}} for i, (wid, (s, e)) in enumerate(zip(workers, qual.EXPECTED_SIX_RANGES))],
        "verification": {
            "method": qual.VERIFICATION_METHOD_2_OF_3, "profile_bound": True, "quorum_verified": True, "execution_profile": qual.CANONICAL_EXECUTION_PROFILE,
            "output_hash": output_hash, "tokens_generated": len(toks), "ranges": len(qual.EXPECTED_SIX_RANGES),
            "range_position_quorums": len(qual.EXPECTED_SIX_RANGES) * len(toks), "signatures_required_per_quorum": 2, "replicas_contacted_per_quorum": 3,
        },
    }

class ScalingControllerTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self._arm_seq = 0

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def next_evidence_path(self, label: str) -> Path:
        self._arm_seq += 1
        return Path(self._tmp.name) / f"{label}_{self._arm_seq}.json"

    def make_ns_clock(self) -> Callable[[], int]:
        counter, lock = [1000], threading.Lock()
        def next_ns() -> int:
            with lock:
                counter[0] += 500
                return counter[0]
        return next_ns

    def test_actual_qual_api_surface_and_saved_production_fixture(self) -> None:
        self.assertEqual(qual.READINESS_PATH, "/inference/community_shards/dispatch_readiness")
        self.assertEqual(qual.SHARD_STATUS_PATH, "/community/shards/status")
        self.assertTrue(callable(qual.coordinator_request_once))
        self.assertEqual(sc.DISPATCH_TIMEOUT_SECONDS, 90.0)
        workers = make_workers(6)
        qual.validate_two_dispatch_responses(make_dispatch_response(workers, seq=1), make_dispatch_response(workers, seq=2), set(workers))
        fixture_env = os.environ.get("ARC_SIX_MACHINE_RESPONSE_FIXTURE")
        fixture_path = Path(fixture_env) if fixture_env else PRODUCTION_FIXTURE_PATH
        self.assertTrue(fixture_path.is_file(), f"restore the captured production response fixture: {fixture_path}")
        loaded = json.loads(fixture_path.read_text(encoding="utf-8"))
        responses = [d["response"] for d in loaded["dispatches"]] if isinstance(loaded, dict) and "dispatches" in loaded else [loaded]
        for resp in responses:
            expected_ids = {p["worker_id"] for p in resp["participants"]}
            qual.validate_dispatch_response(resp, expected_ids)
        if len(responses) >= 2:
            qual.validate_two_dispatch_responses(responses[0], responses[1], {p["worker_id"] for p in responses[0]["participants"]})

    def test_existing_file_and_dangling_symlink_rejected_zero_network(self) -> None:
        workers = make_workers(6)
        existing = self.next_evidence_path("existing")
        existing.write_text("keep\n", encoding="utf-8")
        dangling_target = Path(self._tmp.name) / "missing_target.json"
        dangling_link = self.next_evidence_path("dangling")
        dangling_link.symlink_to(dangling_target)
        with patch.object(sc.qual, "coordinator_get_json_with_retries") as mock_get, \
             patch.object(sc.qual, "coordinator_request_once") as mock_post:
            for bad_path in (existing, dangling_link):
                with self.assertRaises(qual.QualificationError):
                    sc.run_arm("before", set(workers), bad_path, BOOT_NONCE)
        self.assertEqual((mock_get.call_count, mock_post.call_count, existing.read_text(encoding="utf-8"), dangling_target.exists()), (0, 0, "keep\n", False))

    def test_after_waits_for_added_workers_to_leave_before_dispatch_readiness(self) -> None:
        clock = [0.0]
        statuses = [make_status(12, 0, 0, 2), make_status(6, 0, 0, 1)]
        with patch.object(sc.qual, 'coordinator_get_json_with_retries', side_effect=lambda p, d: make_readiness() if p == qual.READINESS_PATH else statuses.pop(0)), patch.object(sc.qual, 'coordinator_request_once') as post:
            _, status = sc.wait_for_arm_readiness('after', BOOT_NONCE, monotonic_fn=lambda: clock[0], sleep_fn=lambda dt: clock.__setitem__(0, clock[0] + dt))
        self.assertEqual(status['status']['ready_workers'], 6)
        self.assertGreater(clock[0], 0)
        post.assert_not_called()
        with patch.object(sc.qual, 'coordinator_get_json_with_retries', side_effect=lambda p, d: make_readiness() if p == qual.READINESS_PATH else make_status(13, 0, 0, 2)):
            with self.assertRaisesRegex(qual.QualificationError, 'unexpected extra workers'):
                sc.wait_for_arm_readiness('after', BOOT_NONCE)

    def test_expanded_two_concurrency_and_client_timing(self) -> None:
        workers, ev_path, clock = make_workers(12), self.next_evidence_path("expanded"), [0.0]
        barrier, lock, active, max_active, seq = threading.Barrier(2, timeout=5.0), threading.Lock(), [0], [0], [0]
        polls = [make_status(6, 6, 0, 1, 1), make_status(12, 0, 0, 2, 0)]
        def fake_post(method: str, path: str, timeout: float, json_body: Any = None) -> tuple[int, Any, int]:
            self.assertEqual((method, path, timeout, json_body), ("POST", qual.DISPATCH_PATH, 90.0, qual.DISPATCH_PAYLOAD))
            with lock:
                active[0] += 1
                max_active[0] = max(max_active[0], active[0])
                seq[0] += 1
                idx = seq[0]
            barrier.wait()
            with lock:
                active[0] -= 1
            return 200, make_dispatch_response(workers[:6] if idx % 2 else workers[6:], seq=idx), 123
        with patch.object(sc.qual, "coordinator_get_json_with_retries", side_effect=lambda p, d: make_readiness() if p == qual.READINESS_PATH else polls.pop(0)), \
             patch.object(sc.qual, "coordinator_request_once", side_effect=fake_post):
            arm = sc.run_arm("expanded", set(workers), ev_path, BOOT_NONCE,
                             monotonic_fn=lambda: clock[0], monotonic_ns_fn=self.make_ns_clock(), sleep_fn=lambda dt: clock.__setitem__(0, clock[0] + dt))
        self.assertEqual((max_active[0], seq[0], arm["workers"], len(arm["requests"])), (2, 6, sorted(workers), 6))
        for i, req in enumerate(arm["requests"], 1):
            self.assertEqual((req["id"], req["success"], req["output_hash"], req["server_verification_validated"]), (f"expanded-{i}", True, OUTPUT_HASH, True))
            self.assertTrue(0 < req["start_ns"] < req["end_ns"] and req["end_ns"] - req["start_ns"] < 5000 and "total_ms" not in req)
        raw = json.loads(ev_path.read_text(encoding="utf-8"))
        self.assertEqual((raw["classification"], raw["counts_toward_operator_target"], raw["verification_basis"], len(raw["attempts"])), ("ephemeral_ci", False, "server_reported_only", 6))

    def test_validation_failure_http_error_foreign_and_timing_no_retry(self) -> None:
        workers = make_workers(6)
        ready_get = lambda p, d: make_readiness() if p == qual.READINESS_PATH else make_status(6, 0, 0, 1)
        ev1 = self.next_evidence_path("bad_val")
        bad, seen_net, seen_val, real_val = make_dispatch_response(workers, seq=1), [False], [False], qual.validate_dispatch_response
        bad["verification"]["quorum_verified"] = False
        def spy_val(resp: Any, expected: set[str]) -> dict[str, Any]:
            saved = json.loads(ev1.read_text(encoding="utf-8"))
            seen_val[0] = saved["attempts"][0]["parsed_response"] == bad and saved["attempts"][0]["status"] == "received"
            return real_val(resp, expected)
        def fake_bad_post(*args: Any, **kwargs: Any) -> tuple[int, Any, int]:
            saved = json.loads(ev1.read_text(encoding="utf-8"))
            seen_net[0] = saved["attempts"][0]["id"] == "before-1" and saved["attempts"][0]["parsed_response"] is None and saved["attempts"][0]["dispatch_uncertain"] is True
            return 200, bad, 50
        with patch.object(sc.qual, "coordinator_get_json_with_retries", side_effect=ready_get), \
             patch.object(sc.qual, "coordinator_request_once", side_effect=fake_bad_post) as mp, \
             patch.object(sc.qual, "validate_dispatch_response", side_effect=spy_val):
            with self.assertRaises(qual.QualificationError):
                sc.run_arm("before", set(workers), ev1, BOOT_NONCE, monotonic_ns_fn=self.make_ns_clock())
        self.assertEqual((mp.call_count, seen_net[0], seen_val[0], json.loads(ev1.read_text(encoding="utf-8"))["attempts"][0]["parsed_response"]), (1, True, True, bad))
        ev2 = self.next_evidence_path("http_err")
        with patch.object(sc.qual, "coordinator_get_json_with_retries", side_effect=ready_get), \
             patch.object(sc.qual, "coordinator_request_once", side_effect=qual.QualificationError("coordinator POST returned HTTP 502: gateway error")) as mp:
            with self.assertRaises(qual.QualificationError):
                sc.run_arm("before", set(workers), ev2, BOOT_NONCE, monotonic_ns_fn=self.make_ns_clock())
        s2 = json.loads(ev2.read_text(encoding="utf-8"))["attempts"][0]
        self.assertEqual((mp.call_count, s2["parsed_response"], s2["dispatch_uncertain"]), (1, None, True))
        ev3 = self.next_evidence_path("foreign")
        foreign_resp = make_dispatch_response(workers[:5] + make_workers(1, offset=99), seq=1)
        with patch.object(sc.qual, "coordinator_get_json_with_retries", side_effect=ready_get), \
             patch.object(sc.qual, "coordinator_request_once", return_value=(200, foreign_resp, 20)) as mp, \
             patch.object(sc.qual, "validate_dispatch_response", wraps=qual.validate_dispatch_response) as sv:
            with self.assertRaises(qual.QualificationError):
                sc.run_arm("before", set(workers), ev3, BOOT_NONCE, monotonic_ns_fn=self.make_ns_clock())
        self.assertEqual((mp.call_count, sv.call_count, json.loads(ev3.read_text(encoding="utf-8"))["attempts"][0]["parsed_response"]), (1, 0, foreign_resp))
        ev4 = self.next_evidence_path("bad_ns")
        with patch.object(sc.qual, "coordinator_get_json_with_retries", side_effect=ready_get), \
             patch.object(sc.qual, "coordinator_request_once", return_value=(200, make_dispatch_response(workers, seq=1), 10)) as mp:
            with self.assertRaises(qual.QualificationError):
                sc.run_arm("before", set(workers), ev4, BOOT_NONCE, monotonic_ns_fn=lambda: 5000)
        self.assertEqual(mp.call_count, 1)

    def test_concurrent_wave_failure_waits_and_persists_second_thread(self) -> None:
        workers, ev_path = make_workers(12), self.next_evidence_path("peer_wait")
        barrier, lock, slot_num = threading.Barrier(2, timeout=5.0), threading.Lock(), [0]
        first_failed, second_done, real_update = threading.Event(), threading.Event(), sc.EvidenceStore.update_attempt
        def spy_update(store_self: sc.EvidenceStore, req_id: str, **fields: Any) -> None:
            real_update(store_self, req_id, **fields)
            if fields.get("status") == "failed":
                first_failed.set()
        def fake_post(*args: Any, **kwargs: Any) -> tuple[int, Any, int]:
            with lock:
                slot = slot_num[0]
                slot_num[0] += 1
            barrier.wait()
            if slot == 0:
                return 200, make_dispatch_response(workers[:6], seq=1, boot_nonce=OTHER_NONCE), 10
            self.assertTrue(first_failed.wait(timeout=5.0))
            second_done.set()
            return 200, make_dispatch_response(workers[6:], seq=2, boot_nonce=BOOT_NONCE), 15
        with patch.object(sc.qual, "coordinator_get_json_with_retries", side_effect=lambda p, d: make_readiness() if p == qual.READINESS_PATH else make_status(12, 0, 0, 2)), \
             patch.object(sc.qual, "coordinator_request_once", side_effect=fake_post), \
             patch.object(sc.EvidenceStore, "update_attempt", spy_update):
            with self.assertRaises(qual.QualificationError):
                sc.run_arm("expanded", set(workers), ev_path, BOOT_NONCE, monotonic_ns_fn=self.make_ns_clock())
        saved = json.loads(ev_path.read_text(encoding="utf-8"))
        self.assertEqual((slot_num[0], second_done.is_set(), sorted(a["status"] for a in saved["attempts"])), (2, True, ["failed", "succeeded"]))
        self.assertTrue(all(a["parsed_response"] is not None for a in saved["attempts"]))

    def test_second_wave_dupe_session_and_drift_stop_subsequent_waves(self) -> None:
        workers6, workers12 = make_workers(6), make_workers(12)
        for label, resp2_kwargs, expected_sub in (("serial_dupe", {"seq": 1}, "duplicate session_id"), ("serial_hash", {"seq": 2, "output_hash": ALT_HASH}, "output_hash")):
            ev, seq = self.next_evidence_path(label), [0]
            def fake_serial(*args: Any, kw: dict[str, Any] = resp2_kwargs, c: list[int] = seq, **kwargs: Any) -> tuple[int, Any, int]:
                c[0] += 1
                return 200, make_dispatch_response(workers6, seq=1) if c[0] == 1 else make_dispatch_response(workers6, **kw), 10
            with patch.object(sc.qual, "coordinator_get_json_with_retries", side_effect=lambda p, d: make_readiness() if p == qual.READINESS_PATH else make_status(6, 0, 0, 1)), \
                 patch.object(sc.qual, "coordinator_request_once", side_effect=fake_serial):
                with self.assertRaises(qual.QualificationError) as ctx:
                    sc.run_arm("before", set(workers6), ev, BOOT_NONCE, monotonic_ns_fn=self.make_ns_clock())
            saved = json.loads(ev.read_text(encoding="utf-8"))
            self.assertEqual((seq[0], len(saved["attempts"]), saved["attempts"][1]["status"]), (2, 2, "failed"))
            self.assertIn(expected_sub, str(ctx.exception))
            self.assertIn(expected_sub, saved["attempts"][1]["error"])
        for label, bad_idx, bad_kw, expected_sub in (("exp_dupe", 3, {"seq": 1}, "duplicate session_id"), ("exp_drift", 4, {"seq": 4, "output_hash": ALT_HASH}, "output_hash")):
            ev, seq, lock = self.next_evidence_path(label), [0], threading.Lock()
            def fake_exp(*args: Any, bi: int = bad_idx, bkw: dict[str, Any] = bad_kw, c: list[int] = seq, lk: threading.Lock = lock, **kwargs: Any) -> tuple[int, Any, int]:
                with lk:
                    c[0] += 1
                    n = c[0]
                w = workers12[:6] if n % 2 else workers12[6:]
                return 200, make_dispatch_response(w, **bkw) if n == bi else make_dispatch_response(w, seq=n), 10
            with patch.object(sc.qual, "coordinator_get_json_with_retries", side_effect=lambda p, d: make_readiness() if p == qual.READINESS_PATH else make_status(12, 0, 0, 2)), \
                 patch.object(sc.qual, "coordinator_request_once", side_effect=fake_exp):
                with self.assertRaises(qual.QualificationError) as ctx:
                    sc.run_arm("expanded", set(workers12), ev, BOOT_NONCE, monotonic_ns_fn=self.make_ns_clock())
            saved = json.loads(ev.read_text(encoding="utf-8"))
            self.assertEqual((seq[0], len(saved["attempts"]), saved["arm_status"]), (4, 4, "failed"))
            self.assertIn(expected_sub, str(ctx.exception))
            self.assertEqual(saved["error"], str(ctx.exception))

    def test_readiness_pins_drift_timeout_and_coverage(self) -> None:
        workers12, clock = make_workers(12), [0.0]
        with patch.object(sc.qual, "coordinator_get_json_with_retries", side_effect=lambda p, d: make_readiness(False) if p == qual.READINESS_PATH else make_status(4, 2, 0, 1)):
            with self.assertRaises(qual.QualificationError):
                sc.wait_for_arm_readiness("before", BOOT_NONCE, monotonic_fn=lambda: clock[0], sleep_fn=lambda dt: clock.__setitem__(0, clock[0] + dt))
        self.assertGreaterEqual(clock[0], 300.0)
        bad_statuses = (
            {**make_status(6, 0, 0, 1, boot_nonce=None), "boot_nonce": BOOT_NONCE},
            make_status(6, 0, 0, 1, boot_nonce=OTHER_NONCE), make_status(7, 0, 0, 1),
            make_status(6, 0, 0, 1, model_id=OTHER_NONCE), make_status(6, 0, 0, 1, execution_profile="wrong"),
            make_status(6, 0, 0, 1, extra_inner={"quarantined_workers": 1}), make_status(6, 0, 0, 1, extra_inner={"cancelled_sessions": 1}),
        )
        for bad_st in bad_statuses:
            with patch.object(sc.qual, "coordinator_get_json_with_retries", side_effect=lambda p, d, s=bad_st: make_readiness() if p == qual.READINESS_PATH else s):
                with self.assertRaises(qual.QualificationError):
                    sc.wait_for_arm_readiness("before", BOOT_NONCE, monotonic_fn=lambda: 0.0, sleep_fn=lambda dt: self.fail("unexpected sleep"))
        ev, seq, lock = self.next_evidence_path("coverage"), [0], threading.Lock()
        def only_six(*args: Any, **kwargs: Any) -> tuple[int, Any, int]:
            with lock:
                seq[0] += 1
                n = seq[0]
            return 200, make_dispatch_response(workers12[:6], seq=n), 10
        with patch.object(sc.qual, "coordinator_get_json_with_retries", side_effect=lambda p, d: make_readiness() if p == qual.READINESS_PATH else make_status(12, 0, 0, 2)), \
             patch.object(sc.qual, "coordinator_request_once", side_effect=only_six):
            with self.assertRaises(qual.QualificationError):
                sc.run_arm("expanded", set(workers12), ev, BOOT_NONCE, monotonic_ns_fn=self.make_ns_clock())
        self.assertEqual((json.loads(ev.read_text(encoding="utf-8"))["arm_status"], seq[0]), ("failed", 6))
