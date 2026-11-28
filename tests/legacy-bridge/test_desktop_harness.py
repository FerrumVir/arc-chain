import http.client
import importlib.util
import io
import ipaddress
import json
import os
from pathlib import Path
import re
import socket
import tempfile
import unittest
from unittest.mock import patch
import urllib.error
from urllib.parse import urlparse
import urllib.request


HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent


def load_module(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


HARNESS = load_module("desktop_v07_harness", "desktop_v07_harness.py")
METADATA = load_module("fetch_release_metadata", "fetch_release_metadata.py")
READINESS = load_module("worker_startup_readiness", "worker_startup_readiness.py")


class FakeHttpResponse:
    def __init__(self, code, raw):
        self.code = code
        self._buffer = io.BytesIO(raw)

    def read(self, size=-1):
        return self._buffer.read(size)

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        return False


class FakeOpener:
    def __init__(self, responses):
        self._responses = list(responses)
        self.calls = []

    def open(self, url, timeout=None):
        self.calls.append((url, timeout))
        item = self._responses.pop(0)
        if isinstance(item, BaseException):
            raise item
        return item


class FakeClock:
    def __init__(self, step=0.25):
        self.now = 100.0
        self.step = step
        self.sleeps = []

    def monotonic(self):
        current = self.now
        self.now += self.step
        return current

    def sleep(self, duration):
        self.sleeps.append(duration)
        self.now += duration


class WorkerStartupReadinessTests(unittest.TestCase):
    model_id = "ab" * 32
    profile = "INT8 integer (per-row, cross-platform deterministic)"

    def snapshot(self, ready):
        return {
            "schema": "arc.inference.readiness.v1",
            "safe_to_dispatch": ready,
            "local_model_ready": ready,
            "community_dispatch_ready": False,
            "sharded_pipeline_ready": False,
            "live_community_workers": 0,
            "model_id": "0x" + self.model_id,
            "required_community_execution_profile": self.profile,
            "mutation_free_observation": True,
        }

    def loading_503_snapshot(self):
        return {
            "schema": "arc.inference.readiness.v1",
            "safe_to_dispatch": False,
            "local_model_ready": False,
            "community_dispatch_ready": False,
            "sharded_pipeline_ready": False,
            "live_community_workers": 0,
            "model_id": None,
            "required_community_execution_profile": self.profile,
            "mutation_free_observation": True,
        }

    def http_error(self, code, raw):
        return urllib.error.HTTPError(
            READINESS.READINESS_URL,
            code,
            f"HTTP {code}",
            {},
            io.BytesIO(raw),
        )

    def test_liveness_cannot_satisfy_model_readiness(self):
        self.assertEqual(
            READINESS.classify_readiness(503, self.loading_503_snapshot(), self.model_id, self.profile),
            "loading",
        )
        self.assertEqual(
            READINESS.classify_readiness(200, self.snapshot(False), self.model_id, self.profile),
            "loading",
        )
        for status in (200, 503):
            with self.subTest(status=status):
                with self.assertRaisesRegex(ValueError, "liveness is not inference readiness"):
                    READINESS.classify_readiness(status, {"status": "loading"}, self.model_id, self.profile)
        self.assertEqual(
            READINESS.classify_readiness(200, self.snapshot(True), self.model_id, self.profile),
            "ready",
        )

    def test_loading_503_requires_exact_schema_false_flags_profile_and_null_model(self):
        with self.assertRaisesRegex(ValueError, "JSON object"):
            READINESS.classify_readiness(503, ["not-an-object"], self.model_id, self.profile)
        for field, wrong in (
            ("schema", "arc.other.v1"),
            ("safe_to_dispatch", True),
            ("local_model_ready", True),
            ("community_dispatch_ready", True),
            ("sharded_pipeline_ready", True),
            ("local_model_ready", 0),
            ("live_community_workers", 1),
            ("live_community_workers", True),
            ("live_community_workers", -1),
            ("model_id", "0x" + self.model_id),
            ("required_community_execution_profile", "another-profile"),
            ("mutation_free_observation", False),
        ):
            with self.subTest(field=field, wrong=wrong):
                snapshot = self.loading_503_snapshot()
                snapshot[field] = wrong
                with self.assertRaises(ValueError):
                    READINESS.classify_readiness(503, snapshot, self.model_id, self.profile)

    def test_ready_requires_pinned_model_profile_and_dispatch(self):
        for field, wrong in (
            ("model_id", None),
            ("model_id", "0x" + "cd" * 32),
            ("required_community_execution_profile", "another-profile"),
            ("safe_to_dispatch", False),
            ("mutation_free_observation", False),
            ("local_model_ready", 1),
            ("community_dispatch_ready", "no"),
            ("sharded_pipeline_ready", 0),
            ("live_community_workers", True),
            ("live_community_workers", -1),
        ):
            with self.subTest(field=field, wrong=wrong):
                snapshot = self.snapshot(True)
                snapshot[field] = wrong
                with self.assertRaises(ValueError):
                    READINESS.classify_readiness(200, snapshot, self.model_id, self.profile)
        mismatched_loading = self.snapshot(False)
        mismatched_loading["model_id"] = "0x" + "cd" * 32
        with self.assertRaisesRegex(ValueError, "different model commitment"):
            READINESS.classify_readiness(200, mismatched_loading, self.model_id, self.profile)

    def test_response_bound_redirect_refusal_and_http_failures(self):
        handler = READINESS.RejectRedirects()
        request = urllib.request.Request(READINESS.READINESS_URL)
        self.assertIsNone(handler.redirect_request(request, None, 302, "redirect", {}, "https://example.invalid"))

        oversized_200 = FakeOpener([FakeHttpResponse(200, b" " * (READINESS.RESPONSE_LIMIT + 1))])
        with self.assertRaisesRegex(ValueError, "exceeds 64 KiB"):
            READINESS.read_snapshot(oversized_200, 1)

        oversized_503 = FakeOpener([self.http_error(503, b" " * (READINESS.RESPONSE_LIMIT + 1))])
        with self.assertRaisesRegex(ValueError, "exceeds 64 KiB"):
            READINESS.read_snapshot(oversized_503, 1)

        valid_503_body = self.loading_503_snapshot()
        valid_503 = FakeOpener([self.http_error(503, json.dumps(valid_503_body).encode("utf-8"))])
        self.assertEqual(READINESS.read_snapshot(valid_503, 1), (503, valid_503_body))

        for status in (301, 302, 401, 403, 404, 500):
            with self.subTest(status=status):
                unexpected = FakeOpener([self.http_error(status, b"")])
                with self.assertRaisesRegex(ValueError, f"unexpected HTTP {status}"):
                    READINESS.read_snapshot(unexpected, 1)
                with self.assertRaises(ValueError):
                    READINESS.classify_readiness(status, {}, self.model_id, self.profile)

        invalid_json_200 = FakeOpener([FakeHttpResponse(200, b"{not-json")])
        with self.assertRaisesRegex(ValueError, "HTTP 200 returned invalid JSON"):
            READINESS.read_snapshot(invalid_json_200, 1)

        invalid_json_503 = FakeOpener([self.http_error(503, b"plain text error")])
        with self.assertRaisesRegex(ValueError, "HTTP 503 returned invalid JSON"):
            READINESS.read_snapshot(invalid_json_503, 1)

        invalid_utf8_503 = FakeOpener([self.http_error(503, b"\xff\xfe")])
        with self.assertRaisesRegex(ValueError, "HTTP 503 returned invalid JSON"):
            READINESS.read_snapshot(invalid_utf8_503, 1)

    def test_wait_for_model_validates_inputs_before_creating_output(self):
        cases = (
            ("", self.profile, 10),
            ("AB" * 32, self.profile, 10),
            ("0x" + self.model_id, self.profile, 10),
            ("ab" * 31, self.profile, 10),
            (123, self.profile, 10),
            (self.model_id, "", 10),
            (self.model_id, None, 10),
            (self.model_id, self.profile, 0),
            (self.model_id, self.profile, -5),
            (self.model_id, self.profile, 601),
            (self.model_id, self.profile, True),
            (self.model_id, self.profile, 1.5),
        )
        with tempfile.TemporaryDirectory() as directory:
            for index, (model_id, profile, seconds) in enumerate(cases):
                with self.subTest(model_id=model_id, profile=profile, seconds=seconds):
                    output = Path(directory) / f"invalid-{index}.jsonl"
                    with self.assertRaises(ValueError):
                        READINESS.wait_for_model(model_id, profile, seconds, output)
                    self.assertFalse(output.exists())

    def test_wait_for_model_retries_transient_errors_and_records_readiness(self):
        opener = FakeOpener(
            [
                urllib.error.URLError("connection refused"),
                ConnectionResetError("peer reset"),
                http.client.RemoteDisconnected("closed before response"),
                TimeoutError("timed out"),
                self.http_error(503, json.dumps(self.loading_503_snapshot()).encode("utf-8")),
                FakeHttpResponse(200, json.dumps(self.snapshot(False)).encode("utf-8")),
                FakeHttpResponse(200, json.dumps(self.snapshot(True)).encode("utf-8")),
            ]
        )
        clock = FakeClock(step=0.25)
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "readiness.jsonl"
            with patch.object(urllib.request, "build_opener", return_value=opener) as build:
                with patch.object(READINESS.time, "monotonic", side_effect=clock.monotonic):
                    with patch.object(READINESS.time, "sleep", side_effect=clock.sleep):
                        READINESS.wait_for_model(self.model_id, self.profile, 30, output)
            self.assertEqual(build.call_count, 1)
            proxy_handler, redirect_handler = build.call_args.args
            self.assertIsInstance(proxy_handler, urllib.request.ProxyHandler)
            self.assertEqual(proxy_handler.proxies, {})
            self.assertIsInstance(redirect_handler, READINESS.RejectRedirects)
            self.assertEqual(len(opener.calls), 7)
            for url, timeout in opener.calls:
                self.assertEqual(url, READINESS.READINESS_URL)
                self.assertGreaterEqual(timeout, 0.01)
                self.assertLessEqual(timeout, 2.0)
            self.assertEqual(len(clock.sleeps), 6)
            for sleep_for in clock.sleeps:
                self.assertGreater(sleep_for, 0)
                self.assertLessEqual(sleep_for, 1.0)
            records = [json.loads(line) for line in output.read_text(encoding="utf-8").splitlines()]
            self.assertEqual(
                [record["state"] for record in records],
                ["unreachable", "unreachable", "unreachable", "unreachable", "loading", "loading", "ready"],
            )
            self.assertEqual([record["http_status"] for record in records[4:]], [503, 200, 200])
            elapsed = [record["elapsed_seconds"] for record in records]
            self.assertTrue(all(value >= 0 for value in elapsed))
            self.assertEqual(elapsed, sorted(elapsed))

    def test_wait_for_model_times_out_and_does_not_retry_protocol_errors(self):
        with tempfile.TemporaryDirectory() as directory:
            timeout_output = Path(directory) / "timeout.jsonl"
            timeout_opener = FakeOpener(
                [self.http_error(503, json.dumps(self.loading_503_snapshot()).encode("utf-8")) for _ in range(10)]
            )
            clock = FakeClock(step=0.5)
            with patch.object(urllib.request, "build_opener", return_value=timeout_opener):
                with patch.object(READINESS.time, "monotonic", side_effect=clock.monotonic):
                    with patch.object(READINESS.time, "sleep", side_effect=clock.sleep):
                        with self.assertRaisesRegex(TimeoutError, "did not load the pinned inference model"):
                            READINESS.wait_for_model(self.model_id, self.profile, 3, timeout_output)
            records = [json.loads(line) for line in timeout_output.read_text(encoding="utf-8").splitlines()]
            self.assertGreaterEqual(len(records), 1)
            self.assertTrue(all(record["state"] == "loading" for record in records))

            redirect_output = Path(directory) / "redirect.jsonl"
            redirect_opener = FakeOpener(
                [
                    self.http_error(302, b""),
                    FakeHttpResponse(200, json.dumps(self.snapshot(True)).encode("utf-8")),
                ]
            )
            with patch.object(urllib.request, "build_opener", return_value=redirect_opener):
                with self.assertRaisesRegex(ValueError, "unexpected HTTP 302"):
                    READINESS.wait_for_model(self.model_id, self.profile, 10, redirect_output)
            self.assertEqual(len(redirect_opener.calls), 1)

            liveness_output = Path(directory) / "liveness.jsonl"
            liveness_opener = FakeOpener(
                [
                    self.http_error(503, json.dumps({"status": "loading"}).encode("utf-8")),
                    FakeHttpResponse(200, json.dumps(self.snapshot(True)).encode("utf-8")),
                ]
            )
            with patch.object(urllib.request, "build_opener", return_value=liveness_opener):
                with self.assertRaisesRegex(ValueError, "liveness is not inference readiness"):
                    READINESS.wait_for_model(self.model_id, self.profile, 10, liveness_output)
            self.assertEqual(len(liveness_opener.calls), 1)


class PortAvailableTests(unittest.TestCase):
    def test_port_available_rejects_active_listener(self):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as active:
            active.bind(("127.0.0.1", 0))
            active.listen(1)
            port = active.getsockname()[1]
            self.assertFalse(HARNESS.port_available(port))
        self.assertTrue(HARNESS.port_available(port))

        if not HARNESS.WINDOWS:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as reused:
                reused.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                reused.bind(("127.0.0.1", 0))
                reused.listen(1)
                reused_port = reused.getsockname()[1]
                self.assertFalse(HARNESS.port_available(reused_port))
            self.assertTrue(HARNESS.port_available(reused_port))

    def test_port_available_accepts_rapid_time_wait_restarts(self):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as seed:
            seed.bind(("127.0.0.1", 0))
            port = seed.getsockname()[1]

        saw_raw_time_wait_refusal = False
        for _ in range(6):
            server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            if not HARNESS.WINDOWS:
                server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            try:
                server.bind(("127.0.0.1", port))
                server.listen(128)
                self.assertFalse(HARNESS.port_available(port))
                with socket.create_connection(("127.0.0.1", port), timeout=5) as client:
                    conn, _ = server.accept()
                    try:
                        client.sendall(b"x")
                        self.assertEqual(conn.recv(1), b"x")
                    finally:
                        conn.close()
                    server.close()
                    self.assertEqual(client.recv(1), b"")
            finally:
                server.close()

            if not HARNESS.WINDOWS:
                with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as raw:
                    try:
                        raw.bind(("127.0.0.1", port))
                    except OSError:
                        saw_raw_time_wait_refusal = True

            self.assertTrue(HARNESS.port_available(port))

        if not HARNESS.WINDOWS:
            self.assertTrue(
                saw_raw_time_wait_refusal,
                "expected rapid server-close cycle to produce TIME_WAIT against non-REUSEADDR bind",
            )

    def test_port_available_matches_rust_tcplistener_socket_options(self):
        for is_windows in (False, True):
            events: list[tuple[object, ...]] = []

            class SpySocket:
                def __enter__(self):
                    return self

                def __exit__(self, *_exc):
                    return False

                def setsockopt(self, level, optname, value):
                    events.append(("setsockopt", level, optname, value))

                def bind(self, address):
                    events.append(("bind", address))

                def listen(self, backlog):
                    events.append(("listen", backlog))

            with patch.object(HARNESS, "WINDOWS", is_windows):
                with patch.object(HARNESS.socket, "socket", return_value=SpySocket()):
                    self.assertTrue(HARNESS.port_available(19944))

            expected = [("bind", ("127.0.0.1", 19944)), ("listen", 128)]
            if not is_windows:
                expected.insert(0, ("setsockopt", socket.SOL_SOCKET, socket.SO_REUSEADDR, 1))
            self.assertEqual(events, expected)

    def test_port_available_returns_false_when_listen_fails(self):
        class FailingListenSocket:
            def __enter__(self):
                return self

            def __exit__(self, *_exc):
                return False

            def setsockopt(self, *_args):
                return None

            def bind(self, _address):
                return None

            def listen(self, _backlog):
                raise OSError("address in use")

        for is_windows in (False, True):
            with patch.object(HARNESS, "WINDOWS", is_windows):
                with patch.object(HARNESS.socket, "socket", return_value=FailingListenSocket()):
                    self.assertFalse(HARNESS.port_available(19944))


class HarnessSafetyHelperTests(unittest.TestCase):
    def test_stop_and_verify_child_exit_waits_for_exit_and_fails_on_leak(self):
        class FakeNode:
            def __init__(self):
                self.stopped = 0

            def stop(self):
                self.stopped += 1

        node = FakeNode()
        with patch.object(HARNESS, "pid_alive", side_effect=[True, False]):
            with patch.object(HARNESS.time, "sleep", return_value=None):
                HARNESS.stop_and_verify_child_exit(node, 4242, timeout_seconds=5.0)
        self.assertEqual(node.stopped, 1)

        leaking_node = FakeNode()
        with patch.object(HARNESS, "pid_alive", return_value=True):
            with patch.object(HARNESS.time, "monotonic", side_effect=[0.0, 0.0, 1.0]):
                with patch.object(HARNESS.time, "sleep", return_value=None):
                    with self.assertRaisesRegex(AssertionError, "4242"):
                        HARNESS.stop_and_verify_child_exit(leaking_node, 4242, timeout_seconds=0.5)
        self.assertEqual(leaking_node.stopped, 1)

    def test_wal_prefix_retention_checks(self):
        with tempfile.TemporaryDirectory() as directory:
            data_dir = Path(directory)
            with self.assertRaises(AssertionError):
                HARNESS.read_wal_snapshot(data_dir)
            wal_path = data_dir / "state.wal"
            wal_path.write_bytes(b"")
            with self.assertRaises(AssertionError):
                HARNESS.read_wal_snapshot(data_dir)
            wal_path.write_bytes(b"arc-wal-prefix-v1")
            snapshot = HARNESS.read_wal_snapshot(data_dir)
            self.assertEqual(snapshot, b"arc-wal-prefix-v1")
            self.assertEqual(
                HARNESS.assert_wal_prefix_retained(data_dir, snapshot, "identical"),
                b"arc-wal-prefix-v1",
            )
            wal_path.write_bytes(b"arc-wal-prefix-v1-appended")
            self.assertEqual(
                HARNESS.assert_wal_prefix_retained(data_dir, snapshot, "appended"),
                b"arc-wal-prefix-v1-appended",
            )
            wal_path.write_bytes(b"arc-wal-prefix")
            with self.assertRaises(AssertionError):
                HARNESS.assert_wal_prefix_retained(data_dir, snapshot, "truncated")
            wal_path.write_bytes(b"mutated-prefix-v1-appended")
            with self.assertRaises(AssertionError):
                HARNESS.assert_wal_prefix_retained(data_dir, snapshot, "mutated")


class FixtureAndHeadlessContractTests(unittest.TestCase):
    def test_previous_release_fixture_is_valid_and_single_source_for_headless(self):
        fixture = json.loads((HERE / "previous-release.json").read_text(encoding="utf-8"))
        self.assertEqual(fixture["schema"], "arc.legacy-bridge.previous-release.v1")
        self.assertTrue(fixture["bridge_tag"].startswith("v"))
        self.assertTrue(fixture["node_tag"].startswith("v"))
        expected_assets = {
            "arc-node-linux-aarch64",
            "arc-node-linux-x86_64",
            "arc-node-macos-arm64",
            "arc-node-macos-x86_64",
            "arc-node-windows-x86_64.exe",
        }
        self.assertEqual(set(fixture["assets"]), expected_assets)
        for name, entry in fixture["assets"].items():
            digest = entry["sha256"]
            self.assertEqual(len(digest), 64, name)
            self.assertTrue(all(ch in "0123456789abcdef" for ch in digest), name)
            self.assertTrue(0 < entry["size"] <= 4 * 1024 * 1024, name)

        headless = (HERE / "headless-v07-acceptance.sh").read_text(encoding="utf-8")
        self.assertIn("previous-release.json", headless)
        self.assertNotIn(fixture["assets"]["arc-node-linux-x86_64"]["sha256"], headless)
        self.assertNotIn("https://api.github.com/", headless)
        self.assertIn("fetch_release_metadata.py", headless)
        self.assertIn("unset GH_TOKEN GITHUB_TOKEN", headless)
        self.assertLess(
            headless.index("unset GH_TOKEN GITHUB_TOKEN"),
            headless.index("releases/download/$legacy_node_tag/$asset"),
        )
        self.assertIn('"$first_bridge_sha"', headless)
        compute_start = headless.index("An opted-in existing worker must pass")
        compute_block = headless[compute_start:]
        self.assertLess(
            compute_block.index("sudo systemctl stop arc-node"),
            compute_block.index("MemAvailable"),
        )
        self.assertLess(
            compute_block.index('"$arc_dir/bin/arc-node" --legacy-bridge-worker-mode full'),
            compute_block.index('install -m 0755 "$state/releases/$prior_tag/$asset" "$arc_dir/bin/arc-node"'),
        )
        self.assertNotIn('"$kept" --legacy-bridge-worker-mode', compute_block)
        self.assertLess(
            compute_block.index('rm -rf "${arc_dir:?}/legacy-bridge/releases/${node_tag:?}"'),
            compute_block.index("sudo systemctl start arc-updater.service"),
        )

    def test_headless_latest_metadata_auth_and_redirect_boundaries(self):
        with patch.dict(os.environ, {"GH_TOKEN": "fixture-headless-credential"}, clear=True):
            with patch.object(urllib.request, "build_opener") as build:
                build.return_value.open.return_value = io.BytesIO(b'{"tag_name": "v0.7.14"}')
                self.assertEqual(
                    METADATA.fetch_latest_release_metadata(attempts=1),
                    {"tag_name": "v0.7.14"},
                )
                request = build.return_value.open.call_args.args[0]
                self.assertEqual(
                    request.full_url,
                    "https://api.github.com/repos/FerrumVir/arc-chain/releases/latest",
                )
                self.assertEqual(request.get_header("Authorization"), "Bearer fixture-headless-credential")
                self.assertIsInstance(build.call_args.args[0], METADATA.RejectMetadataRedirects)

        with patch.dict(os.environ, {}, clear=True):
            with patch.object(urllib.request, "build_opener") as build:
                build.return_value.open.return_value = io.BytesIO(b'{"tag_name": "v0.7.14"}')
                self.assertEqual(
                    METADATA.fetch_latest_release_metadata(attempts=1),
                    {"tag_name": "v0.7.14"},
                )
                self.assertIsNone(build.return_value.open.call_args.args[0].get_header("Authorization"))
                failure = urllib.error.HTTPError("https://api.github.com", 403, "rate limit exceeded", {}, None)
                build.return_value.open.side_effect = failure
                with self.assertRaises(urllib.error.HTTPError) as raised:
                    METADATA.fetch_latest_release_metadata(attempts=3)
                self.assertIs(raised.exception, failure)
                self.assertEqual(build.return_value.open.call_count, 2)

        handler = METADATA.RejectMetadataRedirects()
        request = urllib.request.Request(METADATA.LATEST_RELEASE_URL)
        for url in ("https://api.github.com/redirect", "https://objects.githubusercontent.com/asset"):
            self.assertIsNone(handler.redirect_request(request, None, 302, "Found", {}, url))

        with patch.object(urllib.request, "build_opener") as build:
            build.return_value.open.return_value = io.BytesIO(b" " * (4 * 1024 * 1024 + 1))
            with self.assertRaisesRegex(ValueError, "exceeds 4 MiB"):
                METADATA.fetch_latest_release_metadata(attempts=1)

        with patch.object(urllib.request, "build_opener") as build:
            build.return_value.open.return_value = io.BytesIO(b"{}")
            with self.assertRaisesRegex(ValueError, "tag_name"):
                METADATA.fetch_latest_release_metadata(attempts=1)

    def test_headless_canonical_profile_matches_rust_constant(self):
        transaction_rs = (REPO_ROOT / "crates/arc-types/src/transaction.rs").read_text(encoding="utf-8")
        match = re.search(
            r'pub const CANONICAL_REWARD_INFERENCE_PROFILE:\s*&str\s*=\s*"([^"]+)";',
            transaction_rs,
        )
        self.assertIsNotNone(match)
        rust_profile = match.group(1)
        self.assertEqual(WorkerStartupReadinessTests.profile, rust_profile)
        headless = (HERE / "headless-v07-acceptance.sh").read_text(encoding="utf-8")
        self.assertIn(f"--profile '{rust_profile}'", headless)
        self.assertIn(f"--arg profile '{rust_profile}'", headless)

    def test_headless_prior_node_sha256_pin_contract(self):
        fixture = json.loads((HERE / "previous-release.json").read_text(encoding="utf-8"))
        headless = (HERE / "headless-v07-acceptance.sh").read_text(encoding="utf-8")
        match = re.search(r"^\s*prior_node_sha256=([0-9a-f]{64})\s*$", headless, re.MULTILINE)
        self.assertIsNotNone(match)
        prior_node_sha = match.group(1)
        bridge_digests = {entry["sha256"] for entry in fixture["assets"].values()}
        self.assertNotIn(prior_node_sha, bridge_digests)
        self.assertIn('prior_node_tag="$(jq -er \'.node_tag\' "$previous_release")"', headless)

    def test_headless_on_exit_stops_owned_units_before_removing_firewall(self):
        headless = (HERE / "headless-v07-acceptance.sh").read_text(encoding="utf-8")
        start = headless.index("on_exit() {")
        end = headless.index("\ntrap on_exit EXIT", start)
        body = headless[start:end]
        timer_stop = body.index("sudo systemctl stop arc-updater.timer")
        updater_stop = body.index("sudo systemctl stop arc-updater.service")
        node_stop = body.index("sudo systemctl stop arc-node.service")
        self.assertLess(timer_stop, updater_stop)
        self.assertLess(updater_stop, node_stop)
        self.assertIn("systemctl is-active arc-updater.timer", body)
        self.assertIn("systemctl is-active arc-updater.service", body)
        self.assertIn("systemctl is-active arc-node.service", body)
        self.assertIn("systemctl show -p MainPID --value arc-updater.service", body)
        self.assertIn("systemctl show -p MainPID --value arc-node.service", body)
        self.assertIn('if [ "$updater_pid" != 0 ]; then', body)
        self.assertIn('if [ "$pid" != 0 ]; then', body)
        status_capture = body.index('systemctl status arc-node --no-pager > "$evidence/final/arc-node.status"')
        log_capture = body.index('tail -n 400 "$arc_dir/node.log" > "$evidence/final/node.log.tail"')
        firewall_gate = body.index('if [ "$stop_ok" = true ]; then')
        firewall_remove = body.index('sudo iptables -D OUTPUT -d "$ip" -j REJECT')
        failure_branch = body.index("\n    else\n", firewall_gate)
        self.assertLess(node_stop, status_capture)
        self.assertLess(node_stop, log_capture)
        self.assertLess(log_capture, firewall_gate)
        self.assertLess(firewall_gate, firewall_remove)
        self.assertLess(firewall_remove, failure_branch)
        self.assertNotIn("iptables -D", body[failure_branch:])
        self.assertIn("status=1", body[failure_branch:])

    def test_headless_ip_inventory_covers_shard_coordinator_with_strict_ipv4(self):
        headless = (HERE / "headless-v07-acceptance.sh").read_text(encoding="utf-8")
        live_match = re.search(r"^live_ips=\(([^)]+)\)$", headless, re.MULTILINE)
        self.assertIsNotNone(live_match)
        live_ips = set(live_match.group(1).split())
        self.assertTrue(live_ips)
        for entry in live_ips:
            ipaddress.IPv4Address(entry)

        pins = json.loads((REPO_ROOT / "crates/arc-legacy-bridge/pins/active.json").read_text(encoding="utf-8"))
        for origin in pins["community_rpc_origins"]:
            parsed = urlparse(origin)
            self.assertEqual(parsed.scheme, "https")
            self.assertIn(str(ipaddress.IPv4Address(parsed.hostname)), live_ips)
        shard_worker = pins.get("shard_worker")
        if shard_worker is not None:
            parsed = urlparse(shard_worker["coordinator_url"])
            self.assertEqual(parsed.scheme, "https")
            self.assertIn(str(ipaddress.IPv4Address(parsed.hostname)), live_ips)

        inventory_start = headless.index('seed_ips="$(')
        inventory_end = headless.index('done <<< "$seed_ips"', inventory_start)
        inventory = headless[inventory_start:inventory_end]
        self.assertIn('pins.get("shard_worker")', inventory)
        self.assertIn('shard_worker["coordinator_url"]', inventory)
        self.assertIn("ipaddress.IPv4Address", inventory)
        self.assertIn('[ -n "$seed_ips" ]', inventory)

    def test_headless_local_curl_bounds_wall_clock_deadlines_and_partial_guards(self):
        headless = (HERE / "headless-v07-acceptance.sh").read_text(encoding="utf-8")
        local_curl_start = headless.index("local_curl() {")
        local_curl_end = headless.index("\n}\n", local_curl_start)
        local_curl_body = headless[local_curl_start:local_curl_end]
        for flag in ("--noproxy '*'", "--max-redirs 0", "--connect-timeout 2", '-m "$timeout"'):
            self.assertIn(flag, local_curl_body)
        outside_local_curl = headless[:local_curl_start] + headless[local_curl_end:]
        self.assertIsNone(re.search(r"/usr/bin/curl[^\n]*(?:127\.0\.0\.1|\$url/health)", outside_local_curl))

        for fn_name in ("wait_health", "wait_node_info"):
            fn_start = headless.index(f"{fn_name}() {{")
            fn_end = headless.index("\n}\n", fn_start)
            fn_body = headless[fn_start:fn_end]
            self.assertIn("deadline=$(( $(date +%s) + seconds ))", fn_body)
            self.assertIn("local_curl 2", fn_body)
            self.assertNotIn("seq ", fn_body)
        self.assertIn('seconds="${4:-600}"', headless)
        self.assertIn(
            'wait_node_info "$evidence/node-info.auto-shard.startup.json" "$node_version" "$prior_address" 600',
            headless,
        )

        self.assertIn('if [ -L "$path" ] || [ ! -f "$path" ]; then', headless)
        self.assertIn('if [ -e "$path" ] || [ -L "$path" ]; then', headless)

        clear_needle = 'rm -rf "${arc_dir:?}/legacy-bridge/releases/${node_tag:?}"'
        first_clear = headless.index(clear_needle)
        second_clear = headless.index(clear_needle, first_clear + len(clear_needle))
        for clear_pos in (first_clear, second_clear):
            window = headless[clear_pos : clear_pos + 450]
            self.assertIn('assert_absent_path "$arc_dir/legacy-bridge/releases/$node_tag"', window)
            self.assertIn('assert_regular_file "$arc_dir/legacy-bridge/releases/$prior_node_tag/$asset"', window)
            self.assertIn('assert_regular_file "$expected_model_path"', window)

        auto_start = headless.index("An existing consenting worker without a mode preference automatically qualifies a partial shard")
        auto_block = headless[auto_start:]
        for forbidden in ("--full-integer-worker", "--community-mode", "--community-rpc-url", "--no-community"):
            self.assertIn(forbidden, auto_block)
        self.assertIn('[ "$(argv_value_in "$evidence/updated-auto-shard.argv" --threads)" = 1 ]', auto_block)
        self.assertIn("--community-shard-max-working-set-bytes", auto_block)
        self.assertIn("^[1-9][0-9]*$", auto_block)
        self.assertIn('assert_absent_path "$node_dir/compute-worker-mode"', auto_block)
        for path_expr in (
            '"$expected_model_path"',
            '"$node_dir/compute-consent"',
            '"$node_dir/compute-model.json"',
            '"$prior_keyfile"',
            '"$node_data/genesis.network-hash"',
            '"$arc_dir/legacy-bridge/preserved/arc-node.before-bridge"',
        ):
            self.assertIn(f"assert_regular_file {path_expr}", auto_block)
        for field in (
            '.schema == "arc.inference.readiness.v1"',
            ".safe_to_dispatch == false",
            ".community_dispatch_ready == false",
            ".local_model_ready == false",
            ".sharded_pipeline_ready == false",
            ".live_community_workers == 0",
            ".model_id == null",
            ".required_community_execution_profile == $profile",
            ".mutation_free_observation == true",
        ):
            self.assertIn(field, auto_block)


if __name__ == "__main__":
    unittest.main()
