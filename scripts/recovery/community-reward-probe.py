#!/usr/bin/env python3
"""Create one real ARC community inference job and emit reward evidence.

The recovery orchestrator hash-pins this executable and supplies the six
reviewed validator RPC origins through ``ARC_RECOVERY_RPC_URLS``.  Success is
exactly one JSON object on stdout containing tx/job/worker hashes; diagnostics
go to stderr.  The caller subsequently waits for that exact transaction to be
mined successfully and checks receipt-backed earnings on every validator.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, NoReturn


HASH_RE = re.compile(r"^(?:0x)?[0-9a-f]{64}$")
CANONICAL_PROFILE = "INT8 integer (per-row, cross-platform deterministic)"


class ProbeError(RuntimeError):
    pass


def fail(message: str) -> NoReturn:
    raise ProbeError(message)


def require_object(value: Any, field: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{field} must be a JSON object")
    return value


def require_hash(value: Any, field: str) -> str:
    if not isinstance(value, str) or not HASH_RE.fullmatch(value):
        fail(f"{field} must be exactly 32 lowercase hexadecimal bytes")
    return f"0x{value.removeprefix('0x')}"


def require_int(value: Any, field: str, minimum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        fail(f"{field} must be an integer >= {minimum}")
    return value


def normalize_origin(value: Any) -> str:
    if not isinstance(value, str) or not value:
        fail("RPC origins must be non-empty strings")
    parsed = urllib.parse.urlsplit(value)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname:
        fail(f"invalid RPC origin: {value!r}")
    if parsed.username or parsed.password or parsed.path not in {"", "/"} or parsed.query or parsed.fragment:
        fail(f"RPC origin may contain only scheme, host, and optional port: {value!r}")
    if parsed.scheme == "http" and parsed.hostname not in {"127.0.0.1", "::1", "localhost"}:
        fail(f"remote reward probe origin must use HTTPS: {value!r}")
    return value.rstrip("/")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # type: ignore[no-untyped-def]
        return None


OPENER = urllib.request.build_opener(NoRedirect)


def request_json(origin: str, path: str, timeout: float, body: dict[str, Any] | None = None) -> Any:
    url = f"{origin}{path}"
    data = None
    method = "GET"
    headers = {"Accept": "application/json"}
    if body is not None:
        data = json.dumps(body, separators=(",", ":")).encode("utf-8")
        method = "POST"
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with OPENER.open(request, timeout=timeout) as response:
            if response.status != 200:
                fail(f"{url} returned HTTP {response.status}")
            raw = response.read(2 * 1024 * 1024 + 1)
    except (OSError, urllib.error.URLError, urllib.error.HTTPError) as error:
        fail(f"{url} failed: {error}")
    if len(raw) > 2 * 1024 * 1024:
        fail(f"{url} response exceeded 2 MiB")
    try:
        return json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"{url} returned invalid JSON: {error}")


def rpc_origins(cli_origins: list[str]) -> list[str]:
    values: Any = cli_origins
    if not values:
        encoded = os.environ.get("ARC_RECOVERY_RPC_URLS")
        if not encoded:
            fail("ARC_RECOVERY_RPC_URLS is missing")
        try:
            values = json.loads(encoded)
        except json.JSONDecodeError as error:
            fail(f"ARC_RECOVERY_RPC_URLS is invalid JSON: {error}")
    if not isinstance(values, list) or len(values) != 6:
        fail("reward probe requires exactly six validator RPC origins")
    origins = [normalize_origin(value) for value in values]
    if len(set(origins)) != 6:
        fail("reward probe RPC origins must be unique")
    return origins


def eligible_coordinators(origins: list[str], timeout: float) -> list[str]:
    eligible: list[str] = []
    diagnostics: list[str] = []
    for origin in origins:
        try:
            policy = require_object(
                request_json(origin, "/community/reward_policy", timeout),
                f"{origin} reward policy",
            )
            if policy.get("issuance_ready") is not True:
                diagnostics.append(f"{origin}: reward issuance is not ready")
                continue
            scoreboard = require_object(
                request_json(origin, "/workers/scoreboard?limit=1", timeout),
                f"{origin} worker scoreboard",
            )
            if require_int(
                scoreboard.get("eligible_inference_workers"),
                f"{origin} eligible_inference_workers",
                0,
            ) > 0:
                eligible.append(origin)
            else:
                diagnostics.append(f"{origin}: no eligible full-model worker")
        except ProbeError as error:
            diagnostics.append(str(error))
    if not eligible:
        fail("no issuance-ready coordinator sees the worker: " + "; ".join(diagnostics))
    return eligible


def evidence_from_inference(value: Any, origin: str) -> dict[str, str]:
    body = require_object(value, f"{origin} inference response")
    if body.get("success") is not True:
        fail(f"{origin} inference did not succeed: {body.get('error')!r}")

    worker = require_object(body.get("worker"), f"{origin} worker evidence")
    worker_id = require_hash(worker.get("worker_id"), "worker.worker_id")
    routed_via = body.get("routed_via")
    if routed_via != f"community:{worker.get('worker_id')}":
        fail(f"{origin} did not route through the evidenced community worker")

    inference = require_object(body.get("inference"), f"{origin} inference evidence")
    if inference.get("engine") != CANONICAL_PROFILE:
        fail(
            f"{origin} worker profile {inference.get('engine')!r} differs from {CANONICAL_PROFILE!r}"
        )
    require_hash(inference.get("model_hash"), "inference.model_hash")
    require_hash(inference.get("input_hash"), "inference.input_hash")
    require_hash(inference.get("output_hash"), "inference.output_hash")
    require_int(inference.get("tokens_generated"), "inference.tokens_generated", 1)
    if not isinstance(inference.get("output"), str):
        fail("inference.output must be a string")

    verification = require_object(body.get("verification"), f"{origin} verification evidence")
    if verification.get("method") != "authenticated_shard_quorum_2_of_3_per_range":
        fail(f"{origin} omitted authenticated shard-quorum verification")
    require_int(verification.get("ranges"), "verification.ranges", 1)
    require_int(verification.get("range_position_quorums"), "verification.range_position_quorums", 1)
    if verification.get("signatures_required_per_quorum") != 2:
        fail("verification did not require two authenticated validator signatures per range/position")
    if verification.get("replicas_contacted_per_quorum") != 3:
        fail("verification did not contact all three sealed replicas per range/position")

    settlement = require_object(body.get("settlement"), f"{origin} settlement evidence")
    exact = {
        "status": "pending_mined_receipt",
        "tx_type": "0x25",
        "submitted": True,
        "included": False,
    }
    for field, expected in exact.items():
        if settlement.get(field) != expected:
            fail(f"settlement.{field} expected {expected!r}, got {settlement.get(field)!r}")
    require_int(settlement.get("validator_approvals"), "settlement.validator_approvals", 5)
    if settlement.get("required_validator_approvals") != 5:
        fail("settlement does not commit to the five-of-six approval rule")

    return {
        "tx_hash": require_hash(settlement.get("tx_hash"), "settlement.tx_hash"),
        "job_id": require_hash(settlement.get("job_id"), "settlement.job_id"),
        "worker": worker_id,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rpc-url", action="append", default=[], help="validator RPC origin (repeat exactly six times)")
    parser.add_argument("--input", default=None, help="short deterministic probe prompt")
    parser.add_argument("--max-tokens", type=int, default=1)
    parser.add_argument("--http-timeout-seconds", type=float, default=700.0)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if not 1 <= args.max_tokens <= 256:
        fail("--max-tokens must be in 1..=256")
    if not 1 <= args.http_timeout_seconds <= 7200:
        fail("--http-timeout-seconds must be in 1..=7200")
    origins = rpc_origins(args.rpc_url)
    manifest = os.environ.get("ARC_RECOVERY_ROLLOUT_MANIFEST_SHA256", "unsealed")
    checkpoint = os.environ.get("ARC_RECOVERY_CHECKPOINT_MANIFEST_HASH", "unbound")
    prompt = args.input or f"ARC receipt probe {manifest[:12]} {checkpoint[:12]}"
    if not prompt or len(prompt.encode("utf-8")) > 512 or "\0" in prompt:
        fail("probe input must be 1..512 UTF-8 bytes without NUL")

    errors: list[str] = []
    for origin in eligible_coordinators(origins, min(args.http_timeout_seconds, 30.0)):
        try:
            response = request_json(
                origin,
                "/inference/run",
                args.http_timeout_seconds,
                {"input": prompt, "max_tokens": args.max_tokens},
            )
            evidence = evidence_from_inference(response, origin)
            sys.stdout.write(json.dumps(evidence, sort_keys=True, separators=(",", ":")) + "\n")
            return 0
        except ProbeError as error:
            errors.append(str(error))
    fail("every eligible coordinator rejected or failed the real inference probe: " + "; ".join(errors))


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ProbeError as error:
        sys.stderr.write(f"community reward probe failed: {error}\n")
        raise SystemExit(1)
