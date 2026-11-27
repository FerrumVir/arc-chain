import argparse
import copy
import hashlib
import io
import json
import os
import re
from pathlib import Path
import signal
import stat
import subprocess
import sys
import tempfile
import unittest
from typing import Any
from unittest import mock
import urllib.error
import urllib.request
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import community_shard_six_machine as qual


def make_worker_id(slot: int) -> str:
    return f"{slot:02x}" * 32


def make_readiness(ready: bool, with_0x: bool = True) -> dict:
    prefix = "0x" if with_0x else ""
    return {
        "schema": "arc.community_shard.dispatch_readiness.v1",
        "mutation_free_observation": True,
        "enabled": True,
        "protocol_version": 1,
        "verified_dispatch_enabled": True,
        "sharded_pipeline_ready": ready,
        "safe_to_dispatch": ready,
        "rewards_enabled": False,
        "chat_template_supported": True,
        "coordinator_id": prefix + qual.COORDINATOR_ID,
        "transaction_domain": prefix + qual.TRANSACTION_DOMAIN,
        "model_id": prefix + qual.EXPECTED_MODEL_ID,
        "execution_profile": qual.CANONICAL_EXECUTION_PROFILE,
        "max_prompt_bytes": 4096,
        "chat_template_overhead_bytes": 256,
        "max_tokens_cap": 64,
    }


def make_shard_status(ready_per_tile: int, active_sessions: int = 0) -> dict:
    tiles = []
    for start, end in qual.EXPECTED_SIX_RANGES:
        tiles.append(
            {
                "range": {"start_layer": start, "end_layer": end},
                "ready_workers": ready_per_tile,
                "provisioning_workers": 0,
            }
        )
    return {
        "enabled": True,
        "protocol_version": 1,
        "verified_dispatch_enabled": True,
        "rewards_enabled": False,
        "status": {
            "ready_workers": ready_per_tile * 6,
            "provisioning_workers": 0,
            "active_sessions": active_sessions,
            "tiles": tiles,
        },
    }


def make_identities(
    run_id: str = "9001",
    attempt: str = "1",
    cand_run_id: str = "8001",
    cand_attempt: str = "1",
    cand_sha: str = "7145f682" + "a" * 32,
) -> list[dict]:
    records = []
    for slot in qual.EXPECTED_SLOTS:
        records.append(
            {
                "schema": "arc.community_shard_qual.worker_identity.v1",
                "cohort_class": qual.COHORT_CLASS,
                "counts_toward_operator_target": False,
                "slot": slot,
                "worker_name": qual.make_worker_name(run_id, attempt, slot),
                "worker_id": make_worker_id(slot),
                "runner_name": f"GitHub Actions {100 + slot}",
                "hostname": "shared-image-hostname",
                "boot_id": f"00000000-0000-4000-8000-{slot:012d}",
                "workflow_run_id": run_id,
                "workflow_run_attempt": attempt,
                "candidate_run_id": cand_run_id,
                "candidate_run_attempt": cand_attempt,
                "candidate_sha": cand_sha,
                "created_at_utc": "2026-11-26T00:00:00Z",
            }
        )
    return records


def make_exit_report(slot: int, run_id: str = "9001", attempt: str = "1") -> dict:
    return {
        "worker_id": make_worker_id(slot),
        "worker_name": qual.make_worker_name(run_id, attempt, slot),
        "coordinator_url": qual.COORDINATOR_URL,
        "coordinator_id": qual.COORDINATOR_ID,
        "transaction_domain": qual.TRANSACTION_DOMAIN,
        "model_id": qual.EXPECTED_MODEL_ID,
        "model_name": "llama-2-7b-chat",
        "admitted_max_resident_layers": 6,
        "threads": 2,
        "summary": {
            "registrations": 1,
            "leases_acquired": 1,
            "models_loaded": 1,
            "heartbeats_sent": 4,
            "steps_executed": 10,
            "steps_submitted": 10,
            "submit_retries": 0,
            "submit_stale_conflicts": 0,
            "cancellations_applied": 0,
            "rebalances_completed": 0,
            "clean_release_sent": True,
        },
    }


def make_dispatch_response(seq: int) -> dict:
    output_hash = "ab" * 32
    participants = []
    for slot, (start, end) in zip(qual.EXPECTED_SLOTS, qual.EXPECTED_SIX_RANGES):
        participants.append(
            {
                "worker_id": "0x" + make_worker_id(slot),
                "lease_id": {"boot_nonce": "ef" * 32, "seq": slot},
                "lease_epoch": 1,
                "range": {"start_layer": start, "end_layer": end},
            }
        )
    return {
        "protocol_version": 1,
        "session_id": {"boot_nonce": "ef" * 32, "seq": seq},
        "model_name": "llama-2-7b-chat",
        "model_id": "0x" + qual.EXPECTED_MODEL_ID,
        "execution_profile": qual.CANONICAL_EXECUTION_PROFILE,
        "generated_tokens": [29892, 920],
        "tokens_generated": 2,
        "output": ", how",
        "output_hash": "0x" + output_hash,
        "stopped_by_eos": False,
        "total_ms": 14200,
        "participants": participants,
        "participant_identity_basis": "node_key_not_proof_of_independent_operator",
        "verification": {
            "method": "authenticated_shard_quorum_2_of_2_per_range",
            "profile_bound": True,
            "quorum_verified": True,
            "execution_profile": qual.CANONICAL_EXECUTION_PROFILE,
            "output_hash": "0x" + output_hash,
            "tokens_generated": 2,
            "ranges": 3,
            "range_position_quorums": 6,
            "signatures_required_per_quorum": 2,
            "replicas_contacted_per_quorum": 2,
            "ranges_verified_2_of_2": [[0, 11], [11, 22], [22, 32]],
        },
        "rewards_enabled": False,
        "reward_eligible": False,
    }


class TestNormalizationAndHttpGuards(unittest.TestCase):
    def test_lifecycle_budgets_cover_slow_download_and_match_workflow(self) -> None:
        qual.validate_lifecycle_time_budgets()
        source = Path(qual.__file__).resolve()
        workflow = source.parent / "community-shard-six-machine.yml"
        if not workflow.is_file():
            workflow = source.parents[2] / ".github/workflows/community-shard-six-machine.yml"
        text = workflow.read_text(encoding="utf-8")
        worker = text.split("\n  shard-worker:\n", 1)[1].split("\n  controller:\n", 1)[0]
        controller = text.split("\n  controller:\n", 1)[1].split("\n  aggregate:\n", 1)[0]
        aggregate = text.split("\n  aggregate:\n", 1)[1]
        self.assertEqual(re.findall(r"timeout-minutes: (\d+)", worker), [str(qual.WORKER_JOB_TIMEOUT_MINUTES)])
        self.assertEqual(re.findall(r"timeout-minutes: (\d+)", controller), [str(qual.CONTROLLER_JOB_TIMEOUT_MINUTES)])
        for flag, expected in [("--max-time", qual.MODEL_DOWNLOAD_ATTEMPT_SECONDS), ("--retry-max-time", qual.MODEL_DOWNLOAD_RETRY_WINDOW_SECONDS), ("--retry", qual.MODEL_DOWNLOAD_RETRIES), ("--max-seconds", qual.DEFAULT_WORKER_SUPERVISOR_MAX_SECONDS)]:
            self.assertEqual(re.findall(re.escape(flag) + r" (\d+)", worker), [str(expected)], flag)
        self.assertEqual(re.findall(r"timeout --kill-after=(\d+) (\d+) curl", worker), [(str(qual.MODEL_DOWNLOAD_KILL_GRACE_SECONDS), str(qual.MODEL_DOWNLOAD_TOTAL_SECONDS))])
        self.assertEqual(re.findall(r"--cleanup-wait-seconds (\d+)", aggregate), [str(qual.DEFAULT_CLEANUP_WAIT_SECONDS)])
        for constant, value, message in [
            ("MODEL_DOWNLOAD_RETRY_WINDOW_SECONDS", 0, "keep the retry window positive"),
            ("MODEL_DOWNLOAD_RETRY_WINDOW_SECONDS", 600, "keep the retry window positive"),
            ("MODEL_DOWNLOAD_TOTAL_SECONDS", 599, "increase the outer download deadline"),
            ("ARTIFACT_AND_CLEANUP_GRACE_SECONDS", 74, "increase cleanup grace"),
            ("CONTROLLER_IDENTITY_TIMEOUT_SECONDS", 420, "increase identity wait"),
            ("DEFAULT_WORKER_SUPERVISOR_MAX_SECONDS", 1620, "increase worker lifetime"),
            ("DEFAULT_WORKER_SUPERVISOR_MAX_SECONDS", 2220, "increase worker lifetime"),
            ("MAX_WORKER_SUPERVISOR_SECONDS", 1800, "worker lifetime exceeds the hard supervisor bound"),
            ("WORKER_JOB_TIMEOUT_MINUTES", 40, "increase worker job timeout"),
            ("CONTROLLER_JOB_TIMEOUT_MINUTES", 32, "increase controller job timeout"),
        ]:
            with self.subTest(constant=constant), mock.patch.object(qual, constant, value):
                with self.assertRaisesRegex(qual.QualificationError, message):
                    qual.validate_lifecycle_time_budgets()

    def test_kernel_boot_id_read_is_bounded_and_validated(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "boot_id"
            expected = "550e8400-e29b-41d4-a716-446655440000"
            path.write_text(expected + "\n", encoding="ascii")
            self.assertEqual(qual.read_kernel_boot_id(path), expected)
            for invalid in ["", "0" * 36, "00000000-0000-0000-0000-000000000000", expected.replace("41d4", "11d4"), expected.replace("a716", "1716"), expected.upper(), expected + "x" * 100]:
                path.write_text(invalid, encoding="ascii")
                with self.assertRaises(qual.QualificationError):
                    qual.read_kernel_boot_id(path)
            path.write_bytes(b"\xff" * 16)
            with self.assertRaisesRegex(qual.QualificationError, "ASCII"):
                qual.read_kernel_boot_id(path)
            path.unlink()
            with self.assertRaisesRegex(qual.QualificationError, "failed to read"):
                qual.read_kernel_boot_id(path)

    def test_normalize_hex64(self) -> None:
        raw = "0x" + "AB" * 32
        self.assertEqual(qual.normalize_hex64(raw, "pin"), "ab" * 32)
        self.assertEqual(qual.normalize_hex64("cd" * 32, "pin"), "cd" * 32)
        with self.assertRaises(qual.QualificationError):
            qual.normalize_hex64("0" * 64, "pin")
        with self.assertRaises(qual.QualificationError):
            qual.normalize_hex64("0x1234", "pin")
        with self.assertRaises(qual.QualificationError):
            qual.normalize_hex64("zz" * 32, "pin")

    def test_url_and_redirect_guards(self) -> None:
        qual.assert_pinned_https_url("https://149.28.153.31/community/shards/status", qual.COORDINATOR_NETLOC)
        with self.assertRaises(qual.QualificationError):
            qual.assert_pinned_https_url("http://149.28.153.31/community/shards/status", qual.COORDINATOR_NETLOC)
        with self.assertRaises(qual.QualificationError):
            qual.assert_pinned_https_url("https://evil.example/status", qual.COORDINATOR_NETLOC)
        with self.assertRaises(qual.QualificationError):
            qual.assert_pinned_https_url("https://149.28.153.31/repos", qual.GITHUB_API_NETLOC)
        handler = qual.NoRedirectHandler()
        req = urllib.request.Request("https://149.28.153.31/community/shards/status")
        with self.assertRaises(qual.QualificationError):
            handler.redirect_request(req, None, 302, "Found", {}, "https://other.example/")

    def test_strict_opener_disables_proxy_and_error_previews_truncate(self) -> None:
        with mock.patch.dict(os.environ, {"HTTPS_PROXY": "http://127.0.0.1:1", "HTTP_PROXY": "http://127.0.0.1:1"}):
            opener = qual.build_strict_opener()
        proxy_handlers = [h for h in opener.handlers if isinstance(h, urllib.request.ProxyHandler)]
        self.assertTrue(all(not handler.proxies for handler in proxy_handlers))
        self.assertTrue(any(isinstance(h, urllib.request.HTTPSHandler) for h in opener.handlers))

        class FailingOpener:
            def __init__(self, exc: Exception) -> None:
                self.exc = exc

            def open(self, req: Any, timeout: float = 0.0) -> Any:
                raise self.exc

        big_coord_err = urllib.error.HTTPError(
            "https://149.28.153.31/community/shards/status",
            502,
            "Bad Gateway",
            {},
            io.BytesIO(b"X" * 9000),
        )
        with self.assertRaises(qual.QualificationError) as coord_ctx:
            qual.coordinator_request_once(
                "GET",
                "/community/shards/status",
                5.0,
                opener=FailingOpener(big_coord_err),
            )
        self.assertIn("HTTP 502", str(coord_ctx.exception))
        self.assertIn("...(truncated)", str(coord_ctx.exception))

        big_gh_err = urllib.error.HTTPError(
            "https://api.github.com/repos/FerrumVir/arc-chain/actions/runs/1/artifacts",
            403,
            "Forbidden",
            {"Retry-After": "45"},
            io.BytesIO(b"leaked tok-secret-9999 " + b"Y" * 5000),
        )
        with self.assertRaises(qual.GitHubApiHttpError) as gh_ctx:
            qual.github_api_get_json_conditional(
                "/repos/FerrumVir/arc-chain/actions/runs/1/artifacts",
                "tok-secret-9999",
                opener=FailingOpener(big_gh_err),
            )
        self.assertEqual(gh_ctx.exception.status_code, 403)
        self.assertEqual(gh_ctx.exception.retry_after_seconds, 45)
        self.assertIn("HTTP 403", str(gh_ctx.exception))
        self.assertIn("...(truncated)", str(gh_ctx.exception))
        self.assertNotIn("tok-secret-9999", str(gh_ctx.exception))

        not_mod = urllib.error.HTTPError(
            "https://api.github.com/repos/FerrumVir/arc-chain/actions/runs/1/artifacts",
            304,
            "Not Modified",
            {"ETag": '"etag-next"'},
            io.BytesIO(b""),
        )
        payload, etag, is_304 = qual.github_api_get_json_conditional(
            "/repos/FerrumVir/arc-chain/actions/runs/1/artifacts",
            "tok-secret-9999",
            etag='"etag-prev"',
            opener=FailingOpener(not_mod),
        )
        self.assertIsNone(payload)
        self.assertEqual(etag, '"etag-next"')
        self.assertTrue(is_304)


class TestReadinessAndStatusValidation(unittest.TestCase):
    def test_readiness_valid_and_strict_16_keys(self) -> None:
        qual.validate_readiness_payload(make_readiness(ready=False, with_0x=True), expect_ready=False)
        qual.validate_readiness_payload(make_readiness(ready=True, with_0x=False), expect_ready=True)
        extra = make_readiness(ready=False)
        extra["ready_complete_pipelines"] = 0
        with self.assertRaises(qual.QualificationError):
            qual.validate_readiness_payload(extra, expect_ready=False)
        bad_proto = make_readiness(ready=False)
        bad_proto["protocol_version"] = True
        with self.assertRaises(qual.QualificationError):
            qual.validate_readiness_payload(bad_proto, expect_ready=False)
        bad_rewards = make_readiness(ready=True)
        bad_rewards["rewards_enabled"] = True
        with self.assertRaises(qual.QualificationError):
            qual.validate_readiness_payload(bad_rewards, expect_ready=True)
        bad_schema = make_readiness(ready=False)
        bad_schema["schema"] = "arc.community_shard.dispatch_readiness.v2"
        with self.assertRaises(qual.QualificationError):
            qual.validate_readiness_payload(bad_schema, expect_ready=False)

    def test_shard_status_validation(self) -> None:
        qual.validate_shard_status_payload(make_shard_status(0), 0, 0, 0, 0, 0)
        qual.validate_shard_status_payload(make_shard_status(1), 6, 0, 0, 1, 0)
        bad_tile = make_shard_status(1)
        bad_tile["status"]["tiles"][0]["range"] = {"start_layer": 0, "end_layer": 7}
        with self.assertRaises(qual.QualificationError):
            qual.validate_shard_status_payload(bad_tile, 6, 0, 0, 1, 0)
        extra_worker = make_shard_status(1)
        extra_worker["status"]["ready_workers"] = 7
        with self.assertRaises(qual.QualificationError):
            qual.assert_status_no_external_interference(extra_worker, max_workers=6)


class TestExitReportParsing(unittest.TestCase):
    def test_extracts_exact_final_json_after_tracing_lines(self) -> None:
        report = make_exit_report(1)
        stdout = (
            "2026-11-26T00:00:01Z INFO arc_node: registered worker {\"ignored\": true}\n"
            "2026-11-26T00:00:02Z INFO arc_node: brace { unclosed\n"
            + json.dumps(report, indent=2)
            + "\n"
        )
        parsed = qual.extract_and_validate_exit_report(stdout, make_worker_id(1), report["worker_name"])
        self.assertEqual(parsed["summary"]["steps_submitted"], 10)

    def test_rejects_trailing_garbage_or_missing_keys(self) -> None:
        report = make_exit_report(1)
        trailing = json.dumps(report, indent=2) + "\nextra trailing line\n"
        with self.assertRaises(qual.QualificationError):
            qual.extract_and_validate_exit_report(trailing, make_worker_id(1), report["worker_name"])
        missing_field = copy.deepcopy(report)
        del missing_field["summary"]["clean_release_sent"]
        with self.assertRaises(qual.QualificationError):
            qual.extract_and_validate_exit_report(
                json.dumps(missing_field, indent=2),
                make_worker_id(1),
                report["worker_name"],
            )


class TestDispatchAndAggregateProofs(unittest.TestCase):
    def test_verification_contract_matches_actual_rust_serializer(self) -> None:
        root = Path(os.environ.get("ARC_QUALIFICATION_SOURCE_ROOT", str(Path(qual.__file__).resolve().parents[2])))
        rpc = (root / "crates/arc-node/src/rpc.rs").read_text(encoding="utf-8")
        body = rpc.split("struct CommunityVerificationSummary {", 1)[1].split("\n}", 1)[0]
        fields = set(re.findall(r"^\s+(\w+):", body, re.MULTILINE))
        optional = set(re.findall(r'#\[serde\(skip_serializing_if = "Vec::is_empty"\)\]\s+(\w+):', body))
        self.assertEqual(qual.VERIFICATION_OPTIONAL_KEYS, optional)
        self.assertEqual(qual.VERIFICATION_REQUIRED_KEYS, fields - optional)
        for suffix in ["2_OF_3", "2_OF_2", "MIXED"]:
            actual = re.findall(r"const COMMUNITY_VERIFICATION_METHOD_" + suffix + r': &str =\s*"([^"]+)";', rpc)
            self.assertEqual(actual, [getattr(qual, "VERIFICATION_METHOD_" + suffix)])

    def test_three_replica_and_mixed_verification_match_wire_contract(self) -> None:
        ids = {make_worker_id(slot) for slot in qual.EXPECTED_SLOTS}
        all_three = make_dispatch_response(1)
        verification = all_three["verification"]
        verification["method"] = qual.VERIFICATION_METHOD_2_OF_3
        verification["replicas_contacted_per_quorum"] = 3
        del verification["ranges_verified_2_of_2"]
        qual.validate_dispatch_response(all_three, ids)
        mixed = make_dispatch_response(2)
        mixed["verification"]["method"] = qual.VERIFICATION_METHOD_MIXED
        mixed["verification"]["ranges_verified_2_of_2"] = [[11, 22]]
        qual.validate_dispatch_response(mixed, ids)
        invalid = []
        for field, value in [("replicas_contacted_per_quorum", 2), ("ranges_verified_2_of_2", []), ("method", "teacher_forced_2_of_2"), ("range_position_quorums", 5), ("range_position_quorums", 3), ("extra", True)]:
            bad = copy.deepcopy(all_three)
            bad["verification"][field] = value
            invalid.append(bad)
        for ranges in [[], [[0, 11], [10, 22]], [[0, 33]], [[11, 11]], [[0, 11], [11, 22], [22, 32]]]:
            bad = copy.deepcopy(mixed)
            bad["verification"]["ranges_verified_2_of_2"] = ranges
            invalid.append(bad)
        missing = copy.deepcopy(mixed)
        del missing["verification"]["ranges_verified_2_of_2"]
        invalid.append(missing)
        for index, bad in enumerate(invalid):
            with self.subTest(index=index), self.assertRaises(qual.QualificationError):
                qual.validate_dispatch_response(bad, ids)

    def test_dispatch_attempt_preserves_response_on_validation_failure_without_retry(self) -> None:
        ids = {make_worker_id(slot) for slot in qual.EXPECTED_SLOTS}
        response = make_dispatch_response(1)
        response["verification"]["quorum_verified"] = False
        record = {}
        with mock.patch.object(qual, "coordinator_request_once", return_value=(200, response, 123)) as request:
            with self.assertRaises(qual.QualificationError):
                qual.perform_and_record_dispatch_once(record, 1, ids)
        request.assert_called_once_with("POST", qual.DISPATCH_PATH, float(qual.DISPATCH_TIMEOUT_SECONDS), json_body=qual.DISPATCH_PAYLOAD)
        self.assertEqual(len(record["dispatches"]), 1)
        self.assertEqual(record["dispatches"][0]["response"], response)
        self.assertEqual(record["dispatches"][0]["monotonic_latency_ms"], 123)

    def test_ambiguous_dispatch_failure_records_one_attempt_without_retry(self) -> None:
        record = {}
        with mock.patch.object(qual, "coordinator_request_once", side_effect=qual.QualificationError("response timeout")) as request:
            with self.assertRaisesRegex(qual.QualificationError, "response timeout"):
                qual.perform_and_record_dispatch_once(record, 1, set())
        self.assertEqual(request.call_count, 1)
        self.assertEqual(len(record["dispatches"]), 1)
        self.assertIsNone(record["dispatches"][0]["response"])
        self.assertEqual(record["dispatches"][0]["request_payload"], qual.DISPATCH_PAYLOAD)

    def test_two_dispatches_pass_and_reject_tampering(self) -> None:
        expected_ids = {make_worker_id(slot) for slot in qual.EXPECTED_SLOTS}
        r1 = make_dispatch_response(1)
        r2 = make_dispatch_response(2)
        qual.validate_two_dispatch_responses(r1, r2, expected_ids)

        same_session = make_dispatch_response(1)
        with self.assertRaises(qual.QualificationError):
            qual.validate_two_dispatch_responses(r1, same_session, expected_ids)

        diff_tokens = make_dispatch_response(2)
        diff_tokens["generated_tokens"] = [29892, 921]
        with self.assertRaises(qual.QualificationError):
            qual.validate_two_dispatch_responses(r1, diff_tokens, expected_ids)

        bad_quorum = make_dispatch_response(1)
        bad_quorum["verification"]["quorum_verified"] = False
        with self.assertRaises(qual.QualificationError):
            qual.validate_dispatch_response(bad_quorum, expected_ids)

        bad_sigs = make_dispatch_response(1)
        bad_sigs["verification"]["signatures_required_per_quorum"] = 1
        with self.assertRaises(qual.QualificationError):
            qual.validate_dispatch_response(bad_sigs, expected_ids)

        gap_verified = make_dispatch_response(1)
        gap_verified["verification"]["ranges_verified_2_of_2"] = [[0, 11], [12, 22], [22, 32]]
        with self.assertRaises(qual.QualificationError):
            qual.validate_dispatch_response(gap_verified, expected_ids)

        extra_reward = make_dispatch_response(1)
        extra_reward["settlement_tx"] = "0x" + "11" * 32
        with self.assertRaises(qual.QualificationError):
            qual.validate_dispatch_response(extra_reward, expected_ids)

        wrong_worker = make_dispatch_response(1)
        wrong_worker["participants"][0]["worker_id"] = "0x" + "99" * 32
        with self.assertRaises(qual.QualificationError):
            qual.validate_dispatch_response(wrong_worker, expected_ids)

        wrong_basis = make_dispatch_response(1)
        wrong_basis["participant_identity_basis"] = "signed_ed25519_worker_address"
        with self.assertRaises(qual.QualificationError):
            qual.validate_dispatch_response(wrong_basis, expected_ids)

        mismatched_boot = make_dispatch_response(1)
        mismatched_boot["participants"][0]["lease_id"]["boot_nonce"] = "cd" * 32
        with self.assertRaises(qual.QualificationError):
            qual.validate_dispatch_response(mismatched_boot, expected_ids)

        zero_lease_seq = make_dispatch_response(1)
        zero_lease_seq["participants"][0]["lease_id"]["seq"] = 0
        with self.assertRaises(qual.QualificationError):
            qual.validate_dispatch_response(zero_lease_seq, expected_ids)

        zero_epoch = make_dispatch_response(1)
        zero_epoch["participants"][0]["lease_epoch"] = 0
        with self.assertRaises(qual.QualificationError):
            qual.validate_dispatch_response(zero_epoch, expected_ids)

        dup_lease_seq = make_dispatch_response(1)
        dup_lease_seq["participants"][1]["lease_id"]["seq"] = dup_lease_seq["participants"][0]["lease_id"]["seq"]
        with self.assertRaises(qual.QualificationError):
            qual.validate_dispatch_response(dup_lease_seq, expected_ids)

    def test_identities_allow_cloned_hostname_but_reject_duplicate_boot_or_secret(self) -> None:
        ids = make_identities()
        qual.validate_worker_identities(ids, "9001", "1", "8001", "1", "7145f682" + "a" * 32)
        dup_host = copy.deepcopy(ids)
        dup_host[1]["boot_id"] = dup_host[0]["boot_id"]
        with self.assertRaises(qual.QualificationError):
            qual.validate_worker_identities(dup_host, "9001", "1", "8001", "1", "7145f682" + "a" * 32)
        leaked = copy.deepcopy(ids)
        leaked[0]["secret_key"] = "00" * 32
        with self.assertRaises(qual.QualificationError):
            qual.validate_worker_identities(leaked, "9001", "1", "8001", "1", "7145f682" + "a" * 32)

    def test_aggregate_proof_pass_and_failure_modes(self) -> None:
        cand_sha = "7145f682" + "a" * 32
        identities = make_identities(cand_sha=cand_sha)
        evidences = []
        for ident in identities:
            slot = ident["slot"]
            evidences.append(
                {
                    "cohort_class": qual.COHORT_CLASS,
                    "counts_toward_operator_target": False,
                    "slot": slot,
                    "worker_id": ident["worker_id"],
                    "worker_name": ident["worker_name"],
                    "runner_name": ident["runner_name"],
                    "hostname": ident["hostname"],
                    "boot_id": ident["boot_id"],
                    "workflow_run_id": "9001",
                    "workflow_run_attempt": "1",
                    "candidate_run_id": "8001",
                    "candidate_run_attempt": "1",
                    "candidate_sha": cand_sha,
                    "stop_reason": "controller_finished",
                    "exit_code": 0,
                    "key_deleted": True,
                    "stdout_truncated": False,
                    "stderr_truncated": False,
                    "systemd_memory_peak_bytes": 3200000000,
                    "exit_report": make_exit_report(slot),
                    "passed": True,
                }
            )
        ctrl = {
            "cohort_class": qual.COHORT_CLASS,
            "counts_toward_operator_target": False,
            "verification_basis": qual.VERIFICATION_BASIS,
            "workflow_run_id": "9001",
            "workflow_run_attempt": "1",
            "candidate_run_id": "8001",
            "candidate_run_attempt": "1",
            "candidate_sha": cand_sha,
            "status": "passed",
            "error": None,
            "dispatches": [
                {"request_index": 1, "monotonic_latency_ms": 15000, "response": make_dispatch_response(1)},
                {"request_index": 2, "monotonic_latency_ms": 14800, "response": make_dispatch_response(2)},
            ],
        }
        summary = qual.evaluate_aggregate(
            identities=identities,
            worker_evidences=evidences,
            controller_evidence=ctrl,
            cleanup_readiness=make_readiness(ready=False),
            cleanup_status=make_shard_status(0),
            cleanup_error=None,
            expected_run_id="9001",
            expected_attempt="1",
            expected_candidate_run_id="8001",
            expected_candidate_attempt="1",
            expected_candidate_sha=cand_sha,
        )
        self.assertEqual(summary["overall_status"], "passed")
        self.assertIs(summary["counts_toward_operator_target"], False)
        self.assertEqual(summary["real_operators_proven"], 0)
        self.assertTrue(summary["cleanup_verified_zero_workers"])
        self.assertEqual(summary["machine_identity_basis"], "distinct_linux_kernel_boot_ids_and_github_runner_names")
        self.assertEqual([row["boot_id"] for row in summary["workers"]], [row["boot_id"] for row in identities])

        for field, value in [("boot_id", identities[1]["boot_id"]), ("boot_id", None), ("hostname", "different")]:
            with self.subTest(field=field, value=value):
                changed = copy.deepcopy(evidences)
                if value is None:
                    changed[0].pop(field)
                else:
                    changed[0][field] = value
                failure = qual.evaluate_aggregate(
                    identities=identities,
                    worker_evidences=changed,
                    controller_evidence=ctrl,
                    cleanup_readiness=make_readiness(ready=False),
                    cleanup_status=make_shard_status(0),
                    cleanup_error=None,
                    expected_run_id="9001",
                    expected_attempt="1",
                    expected_candidate_run_id="8001",
                    expected_candidate_attempt="1",
                    expected_candidate_sha=cand_sha,
                )
                self.assertEqual(failure["overall_status"], "failed")
                self.assertTrue(any("mismatch" in error for error in failure["failures"]))

        unclean = copy.deepcopy(evidences)
        unclean[2]["exit_report"]["summary"]["clean_release_sent"] = False
        failed_summary = qual.evaluate_aggregate(
            identities=identities,
            worker_evidences=unclean,
            controller_evidence=ctrl,
            cleanup_readiness=make_readiness(ready=False),
            cleanup_status=make_shard_status(0),
            cleanup_error=None,
            expected_run_id="9001",
            expected_attempt="1",
            expected_candidate_run_id="8001",
            expected_candidate_attempt="1",
            expected_candidate_sha=cand_sha,
        )
        self.assertEqual(failed_summary["overall_status"], "failed")

        lingering = qual.evaluate_aggregate(
            identities=identities,
            worker_evidences=evidences,
            controller_evidence=ctrl,
            cleanup_readiness=make_readiness(ready=True),
            cleanup_status=make_shard_status(1),
            cleanup_error="coordinator still reports ready_workers=6 after 65s",
            expected_run_id="9001",
            expected_attempt="1",
            expected_candidate_run_id="8001",
            expected_candidate_attempt="1",
            expected_candidate_sha=cand_sha,
        )
        self.assertEqual(lingering["overall_status"], "failed")
        self.assertFalse(lingering["cleanup_verified_zero_workers"])

        bool_exit = copy.deepcopy(evidences)
        bool_exit[0]["exit_code"] = False
        bool_summary = qual.evaluate_aggregate(
            identities=identities,
            worker_evidences=bool_exit,
            controller_evidence=ctrl,
            cleanup_readiness=make_readiness(ready=False),
            cleanup_status=make_shard_status(0),
            cleanup_error=None,
            expected_run_id="9001",
            expected_attempt="1",
            expected_candidate_run_id="8001",
            expected_candidate_attempt="1",
            expected_candidate_sha=cand_sha,
        )
        self.assertEqual(bool_summary["overall_status"], "failed")

        wrong_stop = copy.deepcopy(evidences)
        wrong_stop[0]["stop_reason"] = "deadline_reached"
        stop_summary = qual.evaluate_aggregate(
            identities=identities,
            worker_evidences=wrong_stop,
            controller_evidence=ctrl,
            cleanup_readiness=make_readiness(ready=False),
            cleanup_status=make_shard_status(0),
            cleanup_error=None,
            expected_run_id="9001",
            expected_attempt="1",
            expected_candidate_run_id="8001",
            expected_candidate_attempt="1",
            expected_candidate_sha=cand_sha,
        )
        self.assertEqual(stop_summary["overall_status"], "failed")

        bad_ctrl_cand = copy.deepcopy(ctrl)
        bad_ctrl_cand["candidate_run_id"] = "9999"
        ctrl_summary = qual.evaluate_aggregate(
            identities=identities,
            worker_evidences=evidences,
            controller_evidence=bad_ctrl_cand,
            cleanup_readiness=make_readiness(ready=False),
            cleanup_status=make_shard_status(0),
            cleanup_error=None,
            expected_run_id="9001",
            expected_attempt="1",
            expected_candidate_run_id="8001",
            expected_candidate_attempt="1",
            expected_candidate_sha=cand_sha,
        )
        self.assertEqual(ctrl_summary["overall_status"], "failed")

    def test_verify_evidence_dir_no_secrets(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "ok.json").write_text('{"worker_id":"aa"}\n', encoding="utf-8")
            qual.verify_evidence_dir_no_secrets(root)
            (root / "bad.json").write_text('{"secret_key":"00"}\n', encoding="utf-8")
            with self.assertRaises(qual.QualificationError):
                qual.verify_evidence_dir_no_secrets(root)
            self.assertEqual(list(root.iterdir()), [])

            (root / "ok2.json").write_text('{"ok":true}\n', encoding="utf-8")
            (root / "tok.json").write_text('{"err":"ghs_abcdef123456"}\n', encoding="utf-8")
            with self.assertRaises(qual.QualificationError):
                qual.verify_evidence_dir_no_secrets(root)
            self.assertEqual(list(root.iterdir()), [])

            (root / "live.json").write_text('{"err":"live-gh-token-value-777"}\n', encoding="utf-8")
            with mock.patch.dict(os.environ, {"GH_TOKEN": "live-gh-token-value-777"}, clear=False):
                with self.assertRaises(qual.QualificationError):
                    qual.verify_evidence_dir_no_secrets(root)
            self.assertEqual(list(root.iterdir()), [])

            (root / "worker-stdout.log").write_text("raw stdout\n", encoding="utf-8")
            with self.assertRaises(qual.QualificationError):
                qual.verify_evidence_dir_no_secrets(root)
            self.assertEqual(list(root.iterdir()), [])

            raw_secret = b"deadbeef" * 8
            (root / "leak.json").write_bytes(b'{"note":"' + raw_secret + b'"}\n')
            with self.assertRaises(qual.QualificationError):
                qual.verify_evidence_dir_no_secrets(root, extra_secret_values=(raw_secret,))
            self.assertEqual(list(root.iterdir()), [])


class TestProvenanceAndArtifactIntegrity(unittest.TestCase):
    def test_provenance_unique_pins_and_repeatable_metadata(self) -> None:
        sha = "7145f682d5b688b9b4e2805d0cb6d351b61863c6"
        valid_text = "\n".join(
            [
                f"repository={qual.EXPECTED_REPO}",
                f"commit={sha}",
                "workflow_run_id=45921473787",
                "workflow_run_attempt=1",
                "build=cargo build --release -p arc-node",
                "build=cargo build --release -p arc-cli",
                "tests=cargo test -p arc-node",
                "tests=cargo test -p arc-inference",
                "check=clippy",
                "check=fmt",
                "rustc 1.96.0-nightly (1e2183119 2026-03-15)",
                "binary: rustc",
                "commit-hash: 1e2183119f0ee19cc26df899e26b04ad0de3475d",
                "commit-date: 2026-03-15",
                "host: x86_64-unknown-linux-gnu",
                "release: 1.96.0-nightly",
                "LLVM version: 22.1.0",
                "cargo 1.96.0-nightly (cbb9bb8bd 2026-03-13)",
            ]
        )
        pins = qual.validate_provenance_text(valid_text, sha, "45921473787", "1")
        self.assertEqual(pins["commit"], sha)
        invalid_footers = [
            valid_text.split("rustc ", 1)[0],
            valid_text.rsplit("cargo ", 1)[0],
            valid_text.replace("host: x86_64-unknown-linux-gnu", "host: aarch64-apple-darwin"),
            valid_text.replace("binary: rustc", "repository=unexpected/repository"),
            valid_text.replace("binary: rustc", "host: x86_64-unknown-linux-gnu\nbinary: rustc"),
            valid_text + "\ntrailing content",
        ]
        for malformed in invalid_footers:
            with self.subTest(malformed=malformed[-120:]), self.assertRaises(qual.QualificationError):
                qual.validate_provenance_text(malformed, sha, "45921473787", "1")

        for dup_line in (
            f"repository={qual.EXPECTED_REPO}",
            f"commit={sha}",
            "workflow_run_id=45921473787",
            "workflow_run_attempt=1",
        ):
            with self.assertRaises(qual.QualificationError):
                qual.validate_provenance_text(valid_text + "\n" + dup_line, sha, "45921473787", "1")

        with self.assertRaises(qual.QualificationError):
            qual.validate_provenance_text(valid_text + "\nunknown prose", sha, "45921473787", "1")

        dup_other = valid_text + "\nrustc=1.80\nrustc=1.81"
        with self.assertRaises(qual.QualificationError):
            qual.validate_provenance_text(dup_other, sha, "45921473787", "1")

    def test_validate_candidate_run_rejects_missing_head_repository(self) -> None:
        sha = "7145f682d5b688b9b4e2805d0cb6d351b61863c6"
        run_payload = {
            "id": 45921473787,
            "run_attempt": 1,
            "repository": {"full_name": qual.EXPECTED_REPO},
            "head_repository": None,
            "path": qual.EXPECTED_CANDIDATE_WORKFLOW_PATH,
            "event": "push",
            "head_sha": sha,
            "status": "completed",
            "conclusion": "success",
        }
        with mock.patch.object(qual, "github_api_get_json", return_value=run_payload):
            with self.assertRaises(qual.QualificationError) as ctx:
                qual.validate_candidate_run_api("45921473787", "1", sha, "tok-validate-1234")
        self.assertIn("head_repository", str(ctx.exception))

    def test_id_pinned_artifact_download_digest_and_zip_bounds(self) -> None:
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, "w", compression=zipfile.ZIP_DEFLATED) as zf:
            zf.writestr("worker-identity.json", '{"slot":1}\n')
        zip_bytes = buf.getvalue()
        zip_sha = hashlib.sha256(zip_bytes).hexdigest()
        meta = {
            "id": 777,
            "name": "qual-worker-identity-r9001-a1-slot-1",
            "expired": False,
            "size_in_bytes": len(zip_bytes),
            "digest": f"sha256:{zip_sha}",
            "workflow_run": {"id": 9001},
        }
        captured: dict[str, object] = {}

        class FakeZipProc:
            def __init__(self, payload: bytes) -> None:
                self.stdout = io.BytesIO(payload)
                self.stderr = io.BytesIO(b"")
                self._rc = 0

            def poll(self) -> int:
                return self._rc

            def wait(self, timeout: float | None = None) -> int:
                return self._rc

            def kill(self) -> None:
                self._rc = -9

        def fake_popen(cmd: list[str], **kwargs: object) -> FakeZipProc:
            captured["cmd"] = cmd
            captured["env"] = kwargs.get("env")
            return FakeZipProc(zip_bytes)

        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "slot-1"
            with mock.patch.dict(
                os.environ,
                {"HOME": "/real/home/runner", "GH_DEBUG": "api", "HTTPS_PROXY": "http://proxy.invalid:8080"},
                clear=False,
            ):
                with mock.patch.object(subprocess, "Popen", side_effect=fake_popen):
                    qual.download_validated_artifact_by_id(
                        "9001",
                        meta["name"],
                        meta,
                        dest,
                        qual.MAX_IDENTITY_ARTIFACT_BYTES,
                        "tok-dl-secret-5555",
                    )
            self.assertEqual((dest / "worker-identity.json").read_text(encoding="utf-8"), '{"slot":1}\n')
            cmd = captured["cmd"]
            assert isinstance(cmd, list)
            self.assertEqual(cmd[:2], ["gh", "api"])
            self.assertEqual(cmd[-1], f"/repos/{qual.EXPECTED_REPO}/actions/artifacts/777/zip")
            env = captured["env"]
            assert isinstance(env, dict)
            self.assertEqual(env.get("GH_TOKEN"), "tok-dl-secret-5555")
            self.assertEqual(env.get("HOME"), "/real/home/runner")
            self.assertNotIn("GH_DEBUG", env)
            self.assertNotIn("HTTPS_PROXY", env)

            bad_digest_meta = dict(meta, digest="sha256:" + "11" * 32)
            bad_dest = Path(tmp) / "bad-digest"
            with mock.patch.object(subprocess, "Popen", side_effect=fake_popen):
                with self.assertRaises(qual.QualificationError):
                    qual.download_validated_artifact_by_id(
                        "9001",
                        meta["name"],
                        bad_digest_meta,
                        bad_dest,
                        qual.MAX_IDENTITY_ARTIFACT_BYTES,
                        "tok-dl-secret-5555",
                    )
            self.assertFalse((bad_dest / "worker-identity.json").exists())

            bad_size_meta = dict(meta, size_in_bytes=len(zip_bytes) + 1)
            with mock.patch.object(subprocess, "Popen", side_effect=fake_popen):
                with self.assertRaises(qual.QualificationError):
                    qual.download_validated_artifact_by_id(
                        "9001",
                        meta["name"],
                        bad_size_meta,
                        Path(tmp) / "bad-size",
                        qual.MAX_IDENTITY_ARTIFACT_BYTES,
                        "tok-dl-secret-5555",
                    )

    def test_extract_validated_zip_archive_rejects_traversal_symlink_count_and_bomb(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            traversal_zip = root / "trav.zip"
            with zipfile.ZipFile(traversal_zip, "w") as zf:
                zf.writestr("../escape.txt", "nope")
            with self.assertRaises(qual.QualificationError):
                qual.extract_validated_zip_archive(traversal_zip, root / "out1", 4096)

            abs_zip = root / "abs.zip"
            with zipfile.ZipFile(abs_zip, "w") as zf:
                zf.writestr("/etc/passwd", "nope")
            with self.assertRaises(qual.QualificationError):
                qual.extract_validated_zip_archive(abs_zip, root / "out2", 4096)

            symlink_zip = root / "sym.zip"
            with zipfile.ZipFile(symlink_zip, "w") as zf:
                zi = zipfile.ZipInfo("bad-link")
                zi.external_attr = (stat.S_IFLNK | 0o777) << 16
                zf.writestr(zi, "/etc/passwd")
            with self.assertRaises(qual.QualificationError):
                qual.extract_validated_zip_archive(symlink_zip, root / "out3", 4096)

            many_zip = root / "many.zip"
            with zipfile.ZipFile(many_zip, "w") as zf:
                for idx in range(5):
                    zf.writestr(f"f{idx}.txt", "ok")
            with self.assertRaises(qual.QualificationError):
                qual.extract_validated_zip_archive(many_zip, root / "out4", 4096, max_files=4)

            bomb_zip = root / "bomb.zip"
            with zipfile.ZipFile(bomb_zip, "w", compression=zipfile.ZIP_DEFLATED) as zf:
                zf.writestr("big.bin", b"A" * 4096)
            with self.assertRaises(qual.QualificationError):
                qual.extract_validated_zip_archive(bomb_zip, root / "out5", 1024)


class TestWorkerEnvAndLifecycleSignals(unittest.TestCase):
    def setUp(self) -> None:
        patcher = mock.patch.object(qual, "read_kernel_boot_id", return_value="00000000-0000-4000-8000-000000000001")
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_host_identity_is_validated_before_key_generation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            candidate = root / "candidate"
            candidate.mkdir()
            (candidate / "arc").write_bytes(b"test")
            key = root / "private" / "key.json"
            args = argparse.Namespace(slot=1, workflow_run_id="9001", workflow_run_attempt="1", candidate_run_id="8001", candidate_run_attempt="1", candidate_sha="7145f682" + "a" * 32, candidate_dir=str(candidate), private_key_path=str(key))
            with mock.patch.object(subprocess, "run") as generate:
                with mock.patch.dict(os.environ, {"RUNNER_NAME": " "}):
                    with self.assertRaisesRegex(qual.QualificationError, "RUNNER_NAME"):
                        qual.cmd_worker_create_identity(args)
                with mock.patch.dict(os.environ, {"RUNNER_NAME": "runner"}):
                    with mock.patch.object(qual, "read_kernel_boot_id", side_effect=qual.QualificationError("invalid boot")):
                        with self.assertRaisesRegex(qual.QualificationError, "invalid boot"):
                            qual.cmd_worker_create_identity(args)
                generate.assert_not_called()
                self.assertFalse(key.parent.exists())

    def test_worker_rejects_missing_or_malformed_boot_before_launch(self) -> None:
        for value in [None, "invalid"]:
            with tempfile.TemporaryDirectory() as tmp:
                args, key, work, evidence, identity = self._setup_worker_fixture(Path(tmp))
                if value is None:
                    identity.pop("boot_id")
                else:
                    identity["boot_id"] = value
                qual.write_json_file(Path(args.identity_dir) / "worker-identity.json", identity)
                with mock.patch.object(subprocess, "Popen") as launch:
                    with self.assertRaisesRegex(qual.QualificationError, "boot_id"):
                        qual.cmd_worker_run(args)
                    launch.assert_not_called()

    def test_worker_rejects_identity_from_another_machine_boot(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            args, key, work, evidence, identity = self._setup_worker_fixture(Path(tmp))
            with mock.patch.object(qual, "read_kernel_boot_id", return_value="00000000-0000-4000-8000-000000000002"):
                with mock.patch.object(subprocess, "Popen") as launch:
                    with self.assertRaisesRegex(qual.QualificationError, "different machine boot"):
                        qual.cmd_worker_run(args)
                    launch.assert_not_called()

    def _setup_worker_fixture(self, root: Path, slot: int = 1) -> tuple[argparse.Namespace, Path, Path, Path, dict]:
        cand_dir = root / "cand"
        cand_dir.mkdir()
        (cand_dir / "arc-node").write_bytes(b"#!/bin/sh\n")
        (cand_dir / "arc").write_bytes(b"#!/bin/sh\n")
        model_path = root / "model.gguf"
        model_path.write_bytes(b"gguf")
        key_dir = root / "private-worker-key"
        key_dir.mkdir(mode=0o700)
        key_path = key_dir / "worker-key.json"
        key_path.write_text(json.dumps({"secret_key": "fe" * 32}), encoding="utf-8")
        os.chmod(key_path, 0o600)
        id_dir = root / "identity"
        id_dir.mkdir()
        ident = make_identities()[slot - 1]
        qual.write_json_file(id_dir / "worker-identity.json", ident)
        work_dir = root / "work"
        ev_dir = root / "evidence"
        args = argparse.Namespace(
            workflow_run_id="9001",
            workflow_run_attempt="1",
            slot=slot,
            candidate_dir=str(cand_dir),
            model_path=str(model_path),
            private_key_path=str(key_path),
            identity_dir=str(id_dir),
            work_dir=str(work_dir),
            evidence_dir=str(ev_dir),
            max_seconds=120,
        )
        return args, key_path, work_dir, ev_dir, ident

    def test_preflight_and_keygen_preserve_inherited_home(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cand_dir = root / "cand"
            cand_dir.mkdir()
            (cand_dir / "arc-node").write_bytes(b"bin")
            (cand_dir / "arc").write_bytes(b"bin")
            model_path = root / "model.gguf"
            model_path.write_bytes(b"gguf")
            ev_dir = root / "evidence"
            id_dir = root / "identity"
            key_path = root / "private-worker-key" / "worker-key.json"
            captured_envs: list[dict[str, str]] = []

            def fake_preflight_run(cmd: list[str], **kwargs: object) -> subprocess.CompletedProcess:
                env = kwargs.get("env")
                assert isinstance(env, dict)
                captured_envs.append(env)
                report = {
                    "worker_name": qual.make_worker_name("9001", "1", 1),
                    "coordinator_url": qual.COORDINATOR_URL,
                    "coordinator_id": qual.COORDINATOR_ID,
                    "transaction_domain": qual.TRANSACTION_DOMAIN,
                    "model_id": qual.EXPECTED_MODEL_ID,
                    "execution_profile": qual.CANONICAL_EXECUTION_PROFILE,
                    "n_layers": qual.EXPECTED_TOTAL_LAYERS,
                    "threads": qual.WORKER_THREADS,
                    "admission": {"admitted_max_resident_layers": qual.WORKER_MAX_RESIDENT_LAYERS},
                }
                return subprocess.CompletedProcess(cmd, 0, json.dumps(report).encode("utf-8"), b"")

            pre_args = argparse.Namespace(
                workflow_run_id="9001",
                workflow_run_attempt="1",
                slot=1,
                candidate_dir=str(cand_dir),
                model_path=str(model_path),
                evidence_dir=str(ev_dir),
            )
            with mock.patch.dict(os.environ, {"HOME": "/inherited/home/dir", "RUNNER_NAME": "gh-1"}, clear=False):
                with mock.patch.object(subprocess, "run", side_effect=fake_preflight_run):
                    self.assertEqual(qual.cmd_worker_preflight(pre_args), 0)
            self.assertEqual(captured_envs[0].get("HOME"), "/inherited/home/dir")
            self.assertNotEqual(captured_envs[0].get("HOME"), str(ev_dir.resolve()))

            keygen_envs: list[dict[str, str]] = []

            def fake_keygen_run(cmd: list[str], **kwargs: object) -> subprocess.CompletedProcess:
                env = kwargs.get("env")
                assert isinstance(env, dict)
                keygen_envs.append(env)
                if "--output" in cmd:
                    key_path.write_text(json.dumps({"secret_key": "cc" * 32}), encoding="utf-8")
                    return subprocess.CompletedProcess(cmd, 0, b"", b"")
                return subprocess.CompletedProcess(cmd, 0, make_worker_id(1) + "\n", "")

            id_args = argparse.Namespace(
                workflow_run_id="9001",
                workflow_run_attempt="1",
                candidate_run_id="8001",
                candidate_run_attempt="1",
                candidate_sha="7145f682" + "a" * 32,
                slot=1,
                candidate_dir=str(cand_dir),
                private_key_path=str(key_path),
                identity_dir=str(id_dir),
            )
            with mock.patch.dict(os.environ, {"HOME": "/inherited/home/dir", "RUNNER_NAME": "gh-1"}, clear=False):
                with mock.patch.object(subprocess, "run", side_effect=fake_keygen_run):
                    self.assertEqual(qual.cmd_worker_create_identity(id_args), 0)
            self.assertEqual(len(keygen_envs), 2)
            for env in keygen_envs:
                self.assertEqual(env.get("HOME"), "/inherited/home/dir")
                self.assertNotEqual(env.get("HOME"), str(key_path.parent.resolve()))

    def test_worker_run_runtime_bound_no_home_override_no_stdout_upload_and_poll_diag(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            args, key_path, work_dir, ev_dir, _ = self._setup_worker_fixture(root)
            exit_report = make_exit_report(1)
            captured: dict[str, object] = {}

            class FakeWorkerProc:
                def __init__(self) -> None:
                    self.stdout = io.BytesIO((json.dumps(exit_report) + "\n").encode("utf-8"))
                    self.stderr = io.BytesIO(b"worker stderr\n")
                    self._rc: int | None = None

                def poll(self) -> int | None:
                    return self._rc

                def wait(self, timeout: float | None = None) -> int:
                    if self._rc is None:
                        raise subprocess.TimeoutExpired("systemd-run", timeout or 0.1)
                    return self._rc

                def kill(self) -> None:
                    self._rc = -9

            fake_proc = FakeWorkerProc()

            def fake_popen(cmd: list[str], **kwargs: object) -> FakeWorkerProc:
                captured["cmd"] = cmd
                captured["env"] = kwargs.get("env")
                return fake_proc

            def fake_stop(unit_name: str) -> bool:
                fake_proc._rc = 0
                return True

            poll_seq = iter(
                [
                    (None, '"e1"', False),
                    qual.GitHubApiHttpError(403, "rate limit tok-worker-secret-888", retry_after_seconds=0),
                    (None, '"e1"', True),
                    ({"name": qual.make_controller_finished_artifact_name("9001", "1"), "expired": False}, '"e2"', False),
                ]
            )

            def fake_find(*_a: object, **_k: object) -> tuple[dict | None, str | None, bool]:
                item = next(poll_seq)
                if isinstance(item, Exception):
                    raise item
                return item

            with mock.patch.dict(os.environ, {"GH_TOKEN": "tok-worker-secret-888", "HOME": "/real/home"}, clear=False):
                with mock.patch.object(qual, "WORKER_POLL_INTERVAL_SECONDS", 0.01):
                    with mock.patch.object(qual, "MAX_POLL_BACKOFF_SECONDS", 0.02):
                        with mock.patch.object(qual, "find_unexpired_run_artifact", side_effect=fake_find):
                            with mock.patch.object(qual, "query_systemd_memory_bytes", return_value=(1000, 2000, "active")):
                                with mock.patch.object(qual, "stop_owned_systemd_unit", side_effect=fake_stop):
                                    with mock.patch.object(subprocess, "Popen", side_effect=fake_popen):
                                        self.assertEqual(qual.cmd_worker_run(args), 0)
            cmd = captured["cmd"]
            assert isinstance(cmd, list)
            self.assertIn("--property=RuntimeMaxSec=180s", cmd)
            self.assertFalse(any(arg.startswith("--setenv=HOME=") for arg in cmd))
            env = captured["env"]
            assert isinstance(env, dict)
            self.assertEqual(env.get("HOME"), "/real/home")
            self.assertNotIn("GH_TOKEN", env)
            self.assertFalse(key_path.exists())
            self.assertFalse((ev_dir / "worker-stdout.log").exists())
            self.assertFalse((ev_dir / "worker-stderr.log").exists())
            self.assertTrue((work_dir / "worker-stdout.log").is_file())
            ev = json.loads((ev_dir / "worker-evidence.json").read_text(encoding="utf-8"))
            self.assertTrue(ev["passed"])
            self.assertEqual(ev["boot_id"], "00000000-0000-4000-8000-000000000001")
            self.assertEqual(ev["runtime_max_seconds"], 180)
            self.assertEqual(ev["poll_error_count"], 1)
            self.assertEqual(ev["poll_not_modified_count"], 1)
            self.assertNotIn("tok-worker-secret-888", ev["last_poll_error"])

    def test_worker_run_pre_spawn_controller_finished_skips_spawn(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            args, key_path, _, ev_dir, _ = self._setup_worker_fixture(root)
            done_meta = {"name": qual.make_controller_finished_artifact_name("9001", "1"), "expired": False}
            with mock.patch.dict(os.environ, {"GH_TOKEN": "tok-pre-1234"}, clear=False):
                with mock.patch.object(qual, "find_unexpired_run_artifact", return_value=(done_meta, '"e1"', False)):
                    with mock.patch.object(subprocess, "Popen", side_effect=AssertionError("must not spawn")):
                        with self.assertRaises(qual.QualificationError):
                            qual.cmd_worker_run(args)
            self.assertFalse(key_path.exists())
            ev = json.loads((ev_dir / "worker-evidence.json").read_text(encoding="utf-8"))
            self.assertEqual(ev["stop_reason"], "controller_already_finished")
            self.assertFalse(ev["passed"])

    def test_worker_run_handles_sigterm_and_sigint_cleanly(self) -> None:
        for sig in (signal.SIGTERM, signal.SIGINT):
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                args, key_path, _, ev_dir, _ = self._setup_worker_fixture(root)
                exit_report = make_exit_report(1)
                orig_handler = signal.getsignal(sig)
                stop_called = {"count": 0}

                class SignalProc:
                    def __init__(self) -> None:
                        self.stdout = io.BytesIO((json.dumps(exit_report) + "\n").encode("utf-8"))
                        self.stderr = io.BytesIO(b"")
                        self._rc: int | None = None
                        self._raised = False

                    def poll(self) -> int | None:
                        if not self._raised:
                            self._raised = True
                            signal.raise_signal(sig)
                        return self._rc

                    def wait(self, timeout: float | None = None) -> int:
                        if self._rc is None:
                            raise subprocess.TimeoutExpired("systemd-run", timeout or 0.1)
                        return self._rc

                    def kill(self) -> None:
                        self._rc = -9

                proc_obj = SignalProc()

                def fake_stop(_unit: str) -> bool:
                    stop_called["count"] += 1
                    proc_obj._rc = 0
                    return True

                with mock.patch.dict(os.environ, {"GH_TOKEN": "tok-sig-1234"}, clear=False):
                    with mock.patch.object(qual, "find_unexpired_run_artifact", return_value=(None, None, False)):
                        with mock.patch.object(qual, "query_systemd_memory_bytes", return_value=(None, None, "active")):
                            with mock.patch.object(qual, "stop_owned_systemd_unit", side_effect=fake_stop):
                                with mock.patch.object(subprocess, "Popen", return_value=proc_obj):
                                    with self.assertRaises(qual.QualificationError):
                                        qual.cmd_worker_run(args)
                self.assertEqual(stop_called["count"], 1)
                self.assertFalse(key_path.exists())
                self.assertIs(signal.getsignal(sig), orig_handler)
                self.assertFalse((ev_dir / "worker-stdout.log").exists())
                ev = json.loads((ev_dir / "worker-evidence.json").read_text(encoding="utf-8"))
                self.assertEqual(ev["stop_reason"], f"signal_{int(sig)}")
                self.assertFalse(ev["passed"])

    def test_terminate_unit_gracefully_retries_before_unit_loaded(self) -> None:
        attempts = {"count": 0}

        class DelayedStopProc:
            def __init__(self) -> None:
                self._rc: int | None = None

            def poll(self) -> int | None:
                return self._rc

            def wait(self, timeout: float | None = None) -> int:
                if self._rc is not None:
                    return self._rc
                raise subprocess.TimeoutExpired("systemd-run", timeout or 0.01)

            def kill(self) -> None:
                self._rc = -9

        proc_obj = DelayedStopProc()

        def fake_stop(_unit: str) -> bool:
            attempts["count"] += 1
            if attempts["count"] >= 3:
                proc_obj._rc = 0
                return True
            return False

        with mock.patch.object(qual, "stop_owned_systemd_unit", side_effect=fake_stop):
            with mock.patch.object(qual, "kill_owned_systemd_unit", side_effect=AssertionError("must not sigkill")):
                rc = qual.terminate_unit_gracefully("arc-qual-worker-9001-1-s1.service", proc_obj, wait_seconds=5.0)
        self.assertEqual(rc, 0)
        self.assertEqual(attempts["count"], 3)



class TestPublicEvidenceUploadGate(unittest.TestCase):
    def test_cli_rejects_missing_directory_and_live_secret(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            evidence = root / "public"
            args = qual.build_parser().parse_args(["verify-public-evidence", "--evidence-dir", str(evidence)])
            with self.assertRaises(qual.QualificationError):
                args.func(args)
            evidence.mkdir()
            artifact = evidence / "result.json"
            artifact.write_text('{"result": "safe"}')
            self.assertEqual(args.func(args), 0)
            with mock.patch.dict(os.environ, {"GH_TOKEN": "private-live-test-token"}):
                artifact.write_text('{"result": "private-live-test-token"}')
                with self.assertRaises(qual.QualificationError):
                    args.func(args)
                self.assertEqual(list(evidence.iterdir()), [])

if __name__ == "__main__":
    unittest.main()
