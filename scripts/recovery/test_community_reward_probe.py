#!/usr/bin/env python3

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
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
        elif self.path == "/workers/scoreboard?limit=1":
            self.send_json({"eligible_inference_workers": 1})
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
            self.send_json(
                {
                    "success": True,
                    "idempotent_replay": True,
                    "recovery_probe_id": request["recovery_probe_id"],
                    "job_id": JOB_ID,
                    "settlement": {
                        "status": "pending_mined_receipt",
                        "tx_type": "0x25",
                        "tx_hash": TX_HASH,
                        "job_id": JOB_ID,
                        "worker": WORKER,
                        "validator_approvals": 5,
                        "submitted": True,
                        "included": False,
                    },
                }
            )
            return
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
                "settlement": {
                    "status": "pending_mined_receipt",
                    "tx_type": "0x25",
                    "tx_hash": TX_HASH,
                    "job_id": JOB_ID,
                    "validator_approvals": 5,
                    "required_validator_approvals": 5,
                    "submitted": True,
                    "included": False,
                },
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

    def test_retry_discovers_same_job_without_creating_another(self) -> None:
        ProbeHandler.replay = True
        result = self.run_probe()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout),
            {"tx_hash": TX_HASH, "job_id": JOB_ID, "worker": WORKER},
        )
        self.assertEqual(len(ProbeHandler.post_requests), 1)

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


if __name__ == "__main__":
    unittest.main()
