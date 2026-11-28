import importlib.util
import io
import json
import os
from pathlib import Path
import socket
import tempfile
import unittest
from unittest.mock import patch
import urllib.error
import urllib.request


HERE = Path(__file__).resolve().parent


def load_module(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


HARNESS = load_module("desktop_v07_harness", "desktop_v07_harness.py")
METADATA = load_module("fetch_release_metadata", "fetch_release_metadata.py")


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


if __name__ == "__main__":
    unittest.main()
