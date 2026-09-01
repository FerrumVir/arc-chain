#!/usr/bin/env python3
from __future__ import annotations

import datetime as dt
import hashlib
import importlib.util
import json
import os
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock


MODULE_PATH = Path(__file__).with_name("legacy-public-height.py")
SPEC = importlib.util.spec_from_file_location("legacy_public_height", MODULE_PATH)
assert SPEC and SPEC.loader
height = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = height
SPEC.loader.exec_module(height)


def digest_body(value: object) -> str:
    return hashlib.sha256(json.dumps(value).encode()).hexdigest()


def valid_info(block_height: int = 123) -> dict[str, object]:
    return {
        "chain": "ARC Chain",
        "version": "0.7.9",
        "block_height": block_height,
    }


def valid_block(block_height: int = 123) -> dict[str, object]:
    return {
        "header": {"height": block_height, "state_root": "b" * 64},
        "hash": "a" * 64,
    }


def fetch_sequence(values: list[object]):
    remaining = iter(values)

    def fetch(_origin: str, _path: str, _timeout: float):
        value = next(remaining)
        return value, digest_body(value)

    return fetch


class QuietServer(ThreadingHTTPServer):
    def handle_error(self, request, client_address):  # type: ignore[no-untyped-def]
        pass


class ResponseHandler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):  # type: ignore[no-untyped-def]
        pass

    def do_GET(self):  # type: ignore[no-untyped-def]
        if self.path == "/redirect":
            self.send_response(302)
            self.send_header("Location", "/valid")
            self.end_headers()
            return
        if self.path == "/missing":
            self.send_response(404)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            return
        if self.path == "/slow":
            time.sleep(0.2)
        if self.path == "/invalid":
            payload = b"{not-json"
        elif self.path == "/oversize":
            payload = b"{" + b" " * height.MAX_BODY_BYTES + b"}"
        else:
            payload = b'{"ok":true}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        try:
            self.wfile.write(payload)
        except BrokenPipeError:
            pass


class LegacyPublicHeightTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        # macOS spells this location through the /var -> /private/var symlink;
        # production inputs intentionally reject symlinked path ancestry.
        self.root = Path(self.temporary.name).resolve()
        self.freeze_validator = mock.patch.object(
            height, "validate_pinned_freeze_plan", return_value=object()
        )
        self.freeze_validator.start()

    def tearDown(self) -> None:
        self.freeze_validator.stop()
        self.temporary.cleanup()

    def freeze_plan(self, *, source: str = "1" * 40, nodes=None, name: str = "freeze.json"):
        if nodes is None:
            nodes = [
                {"name": node_name, "host": host}
                for node_name, host, _origin in height.FLEET
            ]
        value = {
            "schema": height.FREEZE_PLAN_SCHEMA,
            "source_commit": source,
            "nodes": nodes,
        }
        payload = height.canonical_bytes(value)
        path = self.root / name
        path.write_bytes(payload)
        path.chmod(0o444)
        sidecar = path.with_name(path.name + ".sha256")
        digest = hashlib.sha256(payload).hexdigest()
        sidecar.write_text(f"{digest}  {path.name}\n", encoding="ascii")
        sidecar.chmod(0o444)
        return path, digest

    @staticmethod
    def receipt(now: dt.datetime | None = None):
        current = (now or dt.datetime.now(dt.timezone.utc)).replace(microsecond=0)
        timestamp = current.strftime("%Y-%m-%dT%H:%M:%SZ")
        freeze_sha = "2" * 64
        rows = []
        for index, (name, _host, origin) in enumerate(height.FLEET):
            observed = 100 + index
            rows.append(
                {
                    "name": name,
                    "origin": origin,
                    "info_before_height": observed,
                    "latest_block_height": observed,
                    "info_after_height": observed,
                    "latest_block_hash": f"{index + 1:064x}",
                    "info_before_body_sha256": "a" * 64,
                    "latest_block_body_sha256": "b" * 64,
                    "info_after_body_sha256": "c" * 64,
                }
            )
        return {
            "schema": height.SCHEMA,
            "source_main_commit": "1" * 40,
            "freeze_plan_sha256": freeze_sha,
            "capture_id": height.capture_id(freeze_sha),
            "started_at": timestamp,
            "completed_at": timestamp,
            "duration_ms": 5,
            "request_policy": {
                "redirects": "forbidden",
                "maximum_body_bytes": height.MAX_BODY_BYTES,
                "timeout_seconds": 10.0,
                "proxy_environment": "ignored",
                "sequence": ["/info", "/block/latest", "/info"],
            },
            "origins": rows,
            "legacy_public_max_height": 105,
        }

    def test_valid_sample_requires_sandwiched_latest_block(self) -> None:
        row = height.sample_origin(
            "nyc",
            "http://127.0.0.1:9090",
            1,
            fetch_sequence([valid_info(122), valid_block(123), valid_info(124)]),
        )
        self.assertEqual(row["latest_block_height"], 123)
        self.assertEqual(row["info_after_height"], 124)

    def test_missing_negative_and_wrong_hash_fail_closed(self) -> None:
        missing = valid_info()
        missing.pop("block_height")
        cases = (
            ([missing, valid_block(), valid_info()], "non-negative integer"),
            ([valid_info(-1), valid_block(), valid_info()], "non-negative integer"),
            ([valid_info(), {"header": {"height": 123}, "hash": "wrong"}, valid_info()], "64 lowercase"),
            ([valid_info(124), valid_block(123), valid_info(124)], "outside"),
        )
        for values, message in cases:
            with self.subTest(message=message), self.assertRaisesRegex(height.HeightReceiptError, message):
                height.sample_origin("nyc", "http://127.0.0.1:9090", 1, fetch_sequence(values))

    def test_non_arc_nonlegacy_and_backward_info_fail_closed(self) -> None:
        other = valid_info()
        other["chain"] = "Other"
        current = valid_info()
        current["version"] = "0.8.0"
        cases = (
            ([other, valid_block(), valid_info()], "not ARC Chain"),
            ([current, valid_block(), valid_info()], "not a v0.7"),
            ([valid_info(123), valid_block(123), valid_info(122)], "backwards"),
        )
        for values, message in cases:
            with self.subTest(message=message), self.assertRaisesRegex(height.HeightReceiptError, message):
                height.sample_origin("nyc", "http://127.0.0.1:9090", 1, fetch_sequence(values))

    def test_freeze_plan_is_canonical_hash_bound_and_exactly_ordered(self) -> None:
        path, digest = self.freeze_plan()
        value = height.load_freeze_plan(path, digest, "1" * 40)
        self.assertEqual(value["source_commit"], "1" * 40)

        duplicate = [
            {"name": node_name, "host": host}
            for node_name, host, _origin in height.FLEET
        ]
        duplicate[-1] = duplicate[0]
        duplicate_path, duplicate_sha = self.freeze_plan(nodes=duplicate, name="duplicate.json")
        with self.assertRaisesRegex(height.HeightReceiptError, "order/topology"):
            height.load_freeze_plan(duplicate_path, duplicate_sha, "1" * 40)
        with self.assertRaisesRegex(height.HeightReceiptError, "approved sha256"):
            height.load_freeze_plan(path, "f" * 64, "1" * 40)

    def test_freeze_plan_symlink_and_mutable_input_are_rejected(self) -> None:
        path, digest = self.freeze_plan()
        symlink = self.root / "freeze-link.json"
        symlink.symlink_to(path)
        with self.assertRaises(OSError):
            height.load_freeze_plan(symlink, digest, "1" * 40)
        path.chmod(0o644)
        with self.assertRaisesRegex(height.HeightReceiptError, "mutable"):
            height.load_freeze_plan(path, digest, "1" * 40)

    def test_receipt_validation_rejects_stale_duplicate_and_inconsistent_max(self) -> None:
        current = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
        stale = self.receipt(current - dt.timedelta(seconds=301))
        with self.assertRaisesRegex(height.HeightReceiptError, "stale"):
            height.validate_receipt(stale, source_main="1" * 40, freeze_sha="2" * 64, now=current)

        duplicate = self.receipt(current)
        duplicate["origins"][-1] = duplicate["origins"][0].copy()
        with self.assertRaisesRegex(height.HeightReceiptError, "origin order/topology"):
            height.validate_receipt(duplicate, source_main="1" * 40, freeze_sha="2" * 64, now=current)

        wrong_max = self.receipt(current)
        wrong_max["legacy_public_max_height"] = 999
        with self.assertRaisesRegex(height.HeightReceiptError, "not the maximum"):
            height.validate_receipt(wrong_max, source_main="1" * 40, freeze_sha="2" * 64, now=current)

    def test_receipt_is_create_only_mode_0400_and_canonical(self) -> None:
        value = self.receipt()
        output = self.root / "height.json"
        height.create_private_receipt(output, value)
        self.assertEqual(output.stat().st_mode & 0o777, 0o400)
        self.assertEqual(output.read_bytes(), height.canonical_bytes(value))
        with self.assertRaisesRegex(height.HeightReceiptError, "already exists"):
            height.create_private_receipt(output, value)

        loaded, maximum = height.load_and_validate_receipt(
            output, source_main="1" * 40, freeze_sha="2" * 64
        )
        self.assertEqual(loaded, value)
        self.assertEqual(maximum, 105)

    def test_receipt_loader_accepts_only_a_pre_stop_fresh_historical_capture(self) -> None:
        completed = dt.datetime(2026, 8, 31, 12, 0, tzinfo=dt.timezone.utc)
        output = self.root / "historical-height.json"
        value = self.receipt(completed)
        height.create_private_receipt(output, value)

        loaded, maximum = height.load_and_validate_receipt(
            output,
            source_main="1" * 40,
            freeze_sha="2" * 64,
            now=completed + dt.timedelta(seconds=300),
        )
        self.assertEqual(loaded, value)
        self.assertEqual(maximum, 105)

        with self.assertRaisesRegex(height.HeightReceiptError, "future"):
            height.load_and_validate_receipt(
                output,
                source_main="1" * 40,
                freeze_sha="2" * 64,
                now=completed - dt.timedelta(seconds=6),
            )

        with self.assertRaisesRegex(height.HeightReceiptError, "stale"):
            height.load_and_validate_receipt(
                output,
                source_main="1" * 40,
                freeze_sha="2" * 64,
                now=completed + dt.timedelta(seconds=301),
            )

    def test_receipt_output_symlink_is_never_followed(self) -> None:
        target = self.root / "target.json"
        target.write_text("untouched", encoding="utf-8")
        output = self.root / "height.json"
        output.symlink_to(target)
        with self.assertRaisesRegex(height.HeightReceiptError, "already exists"):
            height.create_private_receipt(output, self.receipt())
        self.assertEqual(target.read_text(encoding="utf-8"), "untouched")

    def test_target_receipt_accepts_only_fixed_order_live_subset(self) -> None:
        current = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
        full = self.receipt(current)
        targets = ("ams", "lhr", "sgp")
        selected = [row for row in full["origins"] if row["name"] in targets]
        value = {
            **{key: full[key] for key in (
                "source_main_commit", "freeze_plan_sha256", "capture_id", "started_at",
                "completed_at", "duration_ms", "request_policy",
            )},
            "schema": height.TARGET_HEIGHT_SCHEMA,
            "targets": [
                {
                    "node": name,
                    "host": height.ROUND_FLEET_MAP[name],
                    "rpc_origin": next(
                        row["origin"] for row in selected if row["name"] == name
                    ),
                }
                for name in targets
            ],
            "origins": selected,
            "legacy_public_max_height": max(row["info_after_height"] for row in selected),
        }
        maximum = height.validate_target_receipt_live(
            value, source_main="1" * 40, freeze_sha="2" * 64,
            targets=targets, now=current,
        )
        self.assertEqual(maximum, 105)

        reordered = json.loads(json.dumps(value))
        reordered["targets"][0], reordered["targets"][1] = (
            reordered["targets"][1], reordered["targets"][0]
        )
        with self.assertRaisesRegex(height.HeightReceiptError, "order"):
            height.validate_target_receipt_live(
                reordered, source_main="1" * 40, freeze_sha="2" * 64,
                targets=targets, now=current,
            )

    def test_target_parser_rejects_duplicate_unknown_and_reordered_nodes(self) -> None:
        self.assertEqual(height.parse_targets("nyc,ams,sgp"), ("nyc", "ams", "sgp"))
        for raw in ("", "ams,nyc", "nyc,nyc", "nyc,unknown"):
            with self.subTest(raw=raw), self.assertRaises(height.HeightReceiptError):
                height.parse_targets(raw)


class RequestPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.server = QuietServer(("127.0.0.1", 0), ResponseHandler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.origin = f"http://127.0.0.1:{cls.server.server_port}"

    @classmethod
    def tearDownClass(cls) -> None:
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join(timeout=2)

    def test_redirect_and_404_are_rejected(self) -> None:
        for path, message in (("/redirect", "HTTP 302"), ("/missing", "HTTP 404")):
            with self.subTest(path=path), self.assertRaisesRegex(height.HeightReceiptError, message):
                height.request_json(self.origin, path, 1)

    def test_timeout_oversize_and_invalid_json_are_rejected(self) -> None:
        cases = (
            ("/slow", 0.02, "failed"),
            ("/oversize", 1, "exceeded"),
            ("/invalid", 1, "invalid JSON"),
        )
        for path, timeout, message in cases:
            with self.subTest(path=path), self.assertRaisesRegex(height.HeightReceiptError, message):
                height.request_json(self.origin, path, timeout)

    def test_valid_json_returns_body_hash(self) -> None:
        value, digest = height.request_json(self.origin, "/valid", 1)
        self.assertEqual(value, {"ok": True})
        self.assertEqual(digest, hashlib.sha256(b'{"ok":true}').hexdigest())


if __name__ == "__main__":
    unittest.main()
