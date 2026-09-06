#!/usr/bin/env python3

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


CANONICAL_PROFILE = "INT8 integer (per-row, cross-platform deterministic)"
WORKER = "0x" + "1" * 64
TX_HASH = "0x" + "2" * 64
JOB_ID = "0x" + "3" * 64
MODEL_HASH = "0x" + "4" * 64
INPUT_HASH = "0x" + "5" * 64
OUTPUT_HASH = "0x" + "6" * 64


class ProbeHandler(BaseHTTPRequestHandler):
    profile = CANONICAL_PROFILE
    replay = False
    replay_mined = False
    settlement_patch: dict[str, Any] = {}
    settlement_drop: set[str] = set()
    scoreboard_worker = WORKER
    post_requests: list[dict[str, Any]] = []

    def log_message(self, _format: str, *_args: Any) -> None:
        pass

    def send_json(self, value: Any) -> None:
        encoded = json.dumps(value, separators=(",", ":")).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def do_GET(self) -> None:
        if self.path == "/community/reward_policy":
            self.send_json({"issuance_ready": True})
        elif self.path == f"/workers/scoreboard?limit=1&worker_id={WORKER}":
            self.send_json(
                {
                    "eligible_inference_workers": 1,
                    "coordinator_model_id": MODEL_HASH,
                    "workers": [
                        {
                            "worker_id": self.scoreboard_worker,
                            "capabilities": ["inference"],
                            "model_id": MODEL_HASH,
                            "execution_profile": CANONICAL_PROFILE,
                        }
                    ],
                }
            )
        else:
            self.send_error(404)

    def do_POST(self) -> None:
        if self.path != "/inference/run":
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        request = json.loads(self.rfile.read(length))
        if (
            request.get("max_tokens") != 1
            or not request.get("input")
            or not isinstance(request.get("recovery_probe_id"), str)
        ):
            self.send_error(400)
            return
        self.post_requests.append(request)
        if self.replay:
            settlement = {
                "status": "mined_success" if self.replay_mined else "pending_mined_receipt",
                "tx_type": "0x25",
                "tx_hash": TX_HASH,
                "job_id": JOB_ID,
                "worker": WORKER,
                "validator_approvals": 5,
                "submitted": True,
                "included": self.replay_mined,
                "confirmed": self.replay_mined,
                "success": True if self.replay_mined else None,
                "receipt_url": f"/community/reward_receipt/{TX_HASH}",
            }
            settlement.update(self.settlement_patch)
            for field in self.settlement_drop:
                settlement.pop(field, None)
            self.send_json(
                {
                    "success": True,
                    "idempotent_replay": True,
                    "recovery_probe_id": request["recovery_probe_id"],
                    "job_id": JOB_ID,
                    "settlement": settlement,
                }
            )
            return
        settlement = {
            "status": "pending_mined_receipt",
            "tx_type": "0x25",
            "tx_hash": TX_HASH,
            "job_id": JOB_ID,
            "worker": WORKER,
            "validator_approvals": 5,
            "required_validator_approvals": 5,
            "submitted": True,
            "included": False,
            "confirmed": False,
            "success": None,
            "receipt_url": f"/community/reward_receipt/{TX_HASH}",
        }
        settlement.update(self.settlement_patch)
        for field in self.settlement_drop:
            settlement.pop(field, None)
        self.send_json(
            {
                "success": True,
                "recovery_probe_id": request["recovery_probe_id"],
                "routed_via": f"community:{WORKER}",
                "worker": {"worker_id": WORKER, "live_workers_at_dispatch": 1},
                "inference": {
                    "engine": self.profile,
                    "model_hash": MODEL_HASH,
                    "input_hash": INPUT_HASH,
                    "output_hash": OUTPUT_HASH,
                    "tokens_generated": 1,
                    "output": "ok",
                },
                "verification": {
                    "method": "authenticated_shard_quorum_2_of_3_per_range",
                    "ranges": 6,
                    "range_position_quorums": 42,
                    "signatures_required_per_quorum": 2,
                    "replicas_contacted_per_quorum": 3,
                },
                "settlement": settlement,
            }
        )


class CommunityRewardProbeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.servers: list[ThreadingHTTPServer] = []
        self.threads: list[threading.Thread] = []
        for _ in range(6):
            server = ThreadingHTTPServer(("127.0.0.1", 0), ProbeHandler)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            self.servers.append(server)
            self.threads.append(thread)

    def tearDown(self) -> None:
        for server in self.servers:
            server.shutdown()
            server.server_close()
        for thread in self.threads:
            thread.join(timeout=2)
        ProbeHandler.profile = CANONICAL_PROFILE
        ProbeHandler.replay = False
        ProbeHandler.replay_mined = False
        ProbeHandler.settlement_patch = {}
        ProbeHandler.settlement_drop = set()
        ProbeHandler.scoreboard_worker = WORKER
        ProbeHandler.post_requests = []

    def run_probe(self, *extra_args: str) -> subprocess.CompletedProcess[str]:
        origins = [f"http://127.0.0.1:{server.server_port}" for server in self.servers]
        environment = dict(os.environ)
        environment.update(
            {
                "ARC_RECOVERY_RPC_URLS": json.dumps(origins, separators=(",", ":")),
                "ARC_RECOVERY_ROLLOUT_MANIFEST_SHA256": "a" * 64,
                "ARC_RECOVERY_CHECKPOINT_MANIFEST_HASH": "b" * 64,
            }
        )
        probe = Path(__file__).with_name("community-reward-probe.py")
        return subprocess.run(
            [
                sys.executable,
                str(probe),
                "--http-timeout-seconds",
                "5",
                "--expected-worker",
                WORKER,
                *extra_args,
            ],
            text=True,
            capture_output=True,
            timeout=20,
            check=False,
            env=environment,
        )

    def test_real_probe_shape_emits_only_receipt_evidence(self) -> None:
        result = self.run_probe()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout),
            {"tx_hash": TX_HASH, "job_id": JOB_ID, "worker": WORKER},
        )
        self.assertEqual(result.stderr, "")
        self.assertEqual(len(ProbeHandler.post_requests), 1)
        self.assertTrue(
            ProbeHandler.post_requests[0]["recovery_probe_id"].startswith(
                "0x" + b"ARC-RCV-PROBE1\0\0".hex()
            )
        )
        self.assertEqual(ProbeHandler.post_requests[0]["expected_worker"], WORKER)

    def test_retry_discovers_same_job_without_creating_another(self) -> None:
        ProbeHandler.replay = True
        result = self.run_probe()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout),
            {"tx_hash": TX_HASH, "job_id": JOB_ID, "worker": WORKER},
        )
        self.assertEqual(len(ProbeHandler.post_requests), 1)

    def test_retry_accepts_only_a_complete_successful_mined_state(self) -> None:
        ProbeHandler.replay = True
        ProbeHandler.replay_mined = True
        result = self.run_probe()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout),
            {"tx_hash": TX_HASH, "job_id": JOB_ID, "worker": WORKER},
        )

    def test_retry_rejects_receipt_unavailable_without_waiting_for_timeout(self) -> None:
        ProbeHandler.replay = True
        ProbeHandler.settlement_patch = {
            "status": "receipt_unavailable",
            "included": True,
            "confirmed": False,
            "success": None,
        }
        started = time.monotonic()
        result = self.run_probe()
        elapsed = time.monotonic() - started
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("receipt is unavailable", result.stderr)
        self.assertLess(elapsed, 3.0, "terminal receipt loss must fail before polling timeout")

    def test_inconsistent_transaction_evidence_fails_closed(self) -> None:
        cases = [
            (False, {"submitted": False}, set()),
            (False, {"confirmed": True}, set()),
            (False, {"success": True}, set()),
            (False, {"receipt_url": f"/community/reward_receipt/0x{'9' * 64}"}, set()),
            (False, {"worker": "0x" + "9" * 64}, set()),
            (False, {}, {"worker"}),
            (False, {}, {"success"}),
            (True, {"tx_type": "0x16"}, set()),
            (True, {"included": True}, set()),
            (True, {"success": False}, set()),
        ]
        for replay, patch, dropped in cases:
            with self.subTest(replay=replay, patch=patch, dropped=dropped):
                ProbeHandler.replay = replay
                ProbeHandler.settlement_patch = patch
                ProbeHandler.settlement_drop = dropped
                ProbeHandler.post_requests = []
                result = self.run_probe()
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")

    def test_tampered_probe_identity_fails_before_network_mutation(self) -> None:
        result = self.run_probe("--recovery-probe-id", "0x" + "f" * 64)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("does not match", result.stderr)
        self.assertEqual(ProbeHandler.post_requests, [])

    def test_i16_worker_config_drift_fails_without_evidence(self) -> None:
        ProbeHandler.profile = "INT16 integer (per-row, cross-platform deterministic)"
        result = self.run_probe()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertIn("differs from", result.stderr)

    def test_accepted_worker_must_be_visible_and_must_receive_the_job(self) -> None:
        ProbeHandler.scoreboard_worker = "0x" + "9" * 64
        result = self.run_probe()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("accepted worker", result.stderr)
        self.assertEqual(ProbeHandler.post_requests, [])

        ProbeHandler.scoreboard_worker = WORKER
        ProbeHandler.settlement_patch = {"worker": "0x" + "9" * 64}
        result = self.run_probe()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("settlement.worker differs", result.stderr)


if __name__ == "__main__":
    unittest.main()
