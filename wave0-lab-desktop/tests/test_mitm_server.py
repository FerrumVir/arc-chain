"""Tests for lib/mitm_server.py, the recording server that plays github.com (THROWAWAY LAB FILE)."""
from __future__ import annotations

import base64
import http.client
import json
import os
import shutil
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

import _paths  # noqa: F401
import ca
import mitm_server as ms

MANIFEST = ms.MANIFEST_PATH


class Fixture(unittest.TestCase):
    """One CA for the module, one server per test."""

    @classmethod
    def setUpClass(cls):
        if not shutil.which("openssl"):
            raise unittest.SkipTest("needs the openssl command line")
        cls.tmp = tempfile.TemporaryDirectory()
        cls.ca_dir = Path(cls.tmp.name) / "ca"
        cls.info = ca.make_ca(cls.ca_dir, ["github.com", "api.github.com", "objects.githubusercontent.com"])
        cls.trusting = ssl.create_default_context(cafile=cls.info["ca_cert"])

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def start(self, scenario="latest-404", serve_dir=None, listen_host="127.0.0.1"):
        log = Path(self.tmp.name) / ("log-%d.jsonl" % time.time_ns())
        server = ms.MitmServer(self.info["server_cert"], self.info["server_key"], listen_host=listen_host, port=0,
                               scenario=scenario, log_path=str(log), serve_dir=serve_dir)
        server.start()
        self.addCleanup(server.stop)
        server.log = log
        return server

    def request(self, server, path, host="github.com", method="GET", body=None, headers=None, address="127.0.0.1"):
        raw = socket.create_connection((address, server.port), timeout=10)
        tls = self.trusting.wrap_socket(raw, server_hostname=host)
        connection = http.client.HTTPSConnection(host)
        connection.sock = tls
        send = {"Host": host, "User-Agent": "wave0-test/1"}
        send.update(headers or {})
        connection.request(method, path, body=body, headers=send)
        response = connection.getresponse()
        data = response.read()
        result = (response.status, dict((k.lower(), v) for k, v in response.getheaders()), data)
        connection.close()
        return result

    def wait_for(self, server, count, kind=None, timeout=5.0):
        end = time.time() + timeout
        while time.time() < end:
            items = [r for r in server.requests() if kind is None or r["kind"] == kind]
            if len(items) >= count:
                return items
            time.sleep(0.02)
        self.fail("expected %d %s record(s), have %s" % (count, kind, server.requests()))


class RouteTests(unittest.TestCase):
    def test_latest_404_redirects_to_the_v0712_path_which_is_404(self):
        status, headers, _ = ms.route("latest-404", "GET", "github.com", MANIFEST)
        self.assertEqual(status, 302)
        self.assertEqual(headers["Location"], "https://github.com" + ms.REDIRECT_TARGET_PATH)
        self.assertTrue(ms.REDIRECT_TARGET_PATH.endswith("/releases/download/v0.7.12/latest.json"))
        self.assertEqual(ms.route("latest-404", "GET", "github.com", ms.REDIRECT_TARGET_PATH)[0], 404)

    def test_latest_404_direct_answers_404_at_the_manifest_url(self):
        self.assertEqual(ms.route("latest-404-direct", "GET", "github.com", MANIFEST)[0], 404)

    def test_bait_manifest_is_a_tauri_manifest_for_0811(self):
        status, headers, body = ms.route("bait-0.8.11", "GET", "github.com", MANIFEST)
        self.assertEqual(status, 200)
        self.assertIn("application/json", headers["Content-Type"])
        manifest = json.loads(body)
        self.assertEqual(manifest["version"], "0.8.11")
        self.assertEqual(sorted(manifest["platforms"]), ["darwin-aarch64", "darwin-x86_64", "linux-x86_64", "windows-x86_64"])
        for key, platform in manifest["platforms"].items():
            self.assertTrue(platform["url"].startswith("https://github.com/FerrumVir/arc-chain/releases/download/v0.8.11/"), key)
            decoded = base64.b64decode(platform["signature"]).decode("ascii")
            self.assertTrue(decoded.startswith("untrusted comment:"), "syntactically a minisign signature file")
            self.assertEqual(len(decoded.strip().splitlines()), 4)
        self.assertEqual(ms.bait_signature(), ms.bait_signature())

    def test_the_bait_signature_can_never_verify(self):
        decoded = base64.b64decode(ms.bait_signature()).decode("ascii").splitlines()
        signature_bytes = base64.b64decode(decoded[1])
        self.assertEqual(signature_bytes[:2], b"ED")
        self.assertEqual(set(signature_bytes[2:]), {0}, "all-zero signature bytes: it parses, it cannot verify")

    def test_api_route_lists_the_five_launchers_and_no_manifest(self):
        for scenario in ("latest-404", "api-latest"):
            status, _, body = ms.route(scenario, "GET", "api.github.com", ms.API_LATEST_PATH)
            release = json.loads(body)
            self.assertEqual(status, 200)
            self.assertEqual(release["tag_name"], "v0.7.12")
            names = [asset["name"] for asset in release["assets"]]
            self.assertEqual(sorted(names), ["SHA256SUMS", "arc-node-linux-aarch64", "arc-node-linux-x86_64", "arc-node-macos-arm64", "arc-node-macos-x86_64", "arc-node-windows-x86_64.exe"])
            self.assertNotIn("latest.json", names)

    def test_other_paths_are_404(self):
        for path in ("/", "/robots.txt", "/FerrumVir/arc-chain/releases/download/v0.8.11/latest.json", "/FerrumVir/arc-chain/releases/download/v0.8.11/ARC.Node_x64.app.tar.gz"):
            self.assertEqual(ms.route("latest-404", "GET", "github.com", path)[0], 404, path)

    def test_unknown_scenario_is_refused(self):
        with self.assertRaises(ValueError):
            ms.route("nonsense", "GET", "github.com", "/")
        with self.assertRaises(ValueError):
            ms.MitmServer("c", "k", scenario="nonsense")

    def test_roles(self):
        self.assertEqual(ms.classify_role("github.com", MANIFEST), "manifest")
        self.assertEqual(ms.classify_role("github.com", ms.REDIRECT_TARGET_PATH), "manifest_redirect")
        self.assertEqual(ms.classify_role("github.com", "/FerrumVir/arc-chain/releases/download/v0.8.11/latest.json"), "other_manifest")
        self.assertEqual(ms.classify_role("github.com", "/FerrumVir/arc-chain/releases/download/v0.7.11/latest.json"), "other_manifest")
        self.assertEqual(ms.classify_role("api.github.com", ms.API_LATEST_PATH), "api_latest")
        self.assertEqual(ms.classify_role("github.com", "/x/y"), "other")
        for name in ("a.AppImage", "a.deb", "a.rpm", "a.dmg", "a.msi", "setup.exe", "a.app.tar.gz", "a.tgz", "a.sig", "a.zip", "a.pkg", "A.APPIMAGE"):
            self.assertTrue(ms.is_payload_path("/dl/" + name), name)
            self.assertEqual(ms.classify_role("github.com", "/dl/" + name), "payload", name)
        self.assertFalse(ms.is_payload_path("/FerrumVir/arc-chain/releases/latest/download/latest.json"))


class ServerTests(Fixture):
    def test_manifest_url_in_the_clean_world_is_a_redirect_then_a_404_and_both_are_recorded(self):
        server = self.start("latest-404")
        status, headers, _ = self.request(server, MANIFEST)
        self.assertEqual(status, 302)
        self.assertEqual(headers["server"], "wave0-lab-mitm")
        self.assertEqual(headers["x-wave0-lab-scenario"], "latest-404")
        status, _, body = self.request(server, ms.REDIRECT_TARGET_PATH)
        self.assertEqual((status, body), (404, b"Not Found"))
        records = self.wait_for(server, 2, "request")
        self.assertEqual([r["role"] for r in records], ["manifest", "manifest_redirect"])
        self.assertEqual([r["status"] for r in records], [302, 404])
        first = records[0]
        self.assertEqual((first["sni"], first["host"], first["method"], first["path"], first["query"]), ("github.com", "github.com", "GET", MANIFEST, ""))
        self.assertEqual(first["user_agent"], "wave0-test/1")
        self.assertEqual(first["location"], "https://github.com" + ms.REDIRECT_TARGET_PATH)
        self.assertFalse(first["payload"])
        self.assertEqual([r["seq"] for r in records], [1, 2])
        self.assertTrue(all(r["scenario"] == "latest-404" for r in records))
        self.assertGreater(first["t"], 1.7e9)

    def test_the_direct_variant_has_no_redirect_hop(self):
        server = self.start("latest-404-direct")
        self.assertEqual(self.request(server, MANIFEST)[0], 404)
        self.assertEqual(self.wait_for(server, 1, "request")[0]["status"], 404)

    def test_bait_world_serves_the_manifest_and_never_the_bundle(self):
        server = self.start("bait-0.8.11")
        status, headers, body = self.request(server, MANIFEST)
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(body)["version"], "0.8.11")
        bundle = "/FerrumVir/arc-chain/releases/download/v0.8.11/ARC.Node_x64.app.tar.gz"
        status, _, _ = self.request(server, bundle)
        self.assertEqual(status, 404)
        records = self.wait_for(server, 2, "request")
        self.assertEqual([r["role"] for r in records], ["manifest", "payload"])
        self.assertTrue(records[1]["payload"], "a bundle request is what the pass criteria call a download")
        self.assertFalse(records[0]["payload"])

    def test_every_payload_shape_is_flagged_and_stays_404_even_when_a_file_exists(self):
        with tempfile.TemporaryDirectory() as serve:
            for name in ("ARC.Node_0.7.11_amd64.deb", "setup.exe", "bundle.tar.gz", "x.sig", "fixture.json"):
                Path(serve, name).write_bytes(b"real bytes")
            server = self.start("latest-404", serve_dir=serve)
            for name in ("ARC.Node_0.7.11_amd64.deb", "setup.exe", "bundle.tar.gz", "x.sig"):
                status, _, body = self.request(server, "/dl/" + name)
                self.assertEqual((status, body), (404, b"Not Found"), name)
            status, _, body = self.request(server, "/anything/fixture.json")
            self.assertEqual((status, body), (200, b"real bytes"), "a non-payload fixture is served by basename")
            records = self.wait_for(server, 5, "request")
            self.assertEqual([r["payload"] for r in records], [True, True, True, True, False])

    def test_unknown_paths_and_methods_are_answered_and_recorded(self):
        server = self.start()
        self.assertEqual(self.request(server, "/x?y=1&z=2")[0], 404)
        status, _, _ = self.request(server, "/upload", method="POST", body=b"abc" * 100, headers={"Content-Type": "application/octet-stream"})
        self.assertEqual(status, 404)
        status, _, body = self.request(server, MANIFEST, method="HEAD")
        self.assertEqual((status, body), (302, b""))
        records = self.wait_for(server, 3, "request")
        self.assertEqual([(r["method"], r["path"], r["query"]) for r in records], [("GET", "/x", "y=1&z=2"), ("POST", "/upload", ""), ("HEAD", MANIFEST, "")])
        self.assertEqual(records[2]["bytes_out"], 0)
        self.assertEqual(records[0]["role"], "other")

    def test_api_host_is_served_with_its_own_sni(self):
        server = self.start()
        status, _, body = self.request(server, ms.API_LATEST_PATH, host="api.github.com")
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(body)["tag_name"], "v0.7.12")
        record = self.wait_for(server, 1, "request")[0]
        self.assertEqual((record["sni"], record["host"], record["role"]), ("api.github.com", "api.github.com", "api_latest"))

    def test_a_client_that_refuses_our_ca_is_logged_as_a_tls_failure_with_its_sni(self):
        server = self.start()
        raw = socket.create_connection(("127.0.0.1", server.port), timeout=10)
        with self.assertRaises(ssl.SSLError):
            ssl.create_default_context().wrap_socket(raw, server_hostname="api.github.com")
        record = self.wait_for(server, 1, "tls_failure")[0]
        self.assertEqual(record["sni"], "api.github.com")
        self.assertEqual(record["host"], "api.github.com")
        self.assertIn("error", record)
        self.assertNotIn("path", record)
        self.assertEqual([r for r in server.requests() if r["kind"] == "request"], [])

    def test_a_client_that_connects_and_hangs_up_is_a_tls_failure_without_sni(self):
        server = self.start()
        socket.create_connection(("127.0.0.1", server.port), timeout=10).close()
        record = self.wait_for(server, 1, "tls_failure")[0]
        self.assertIsNone(record["sni"])

    def test_the_log_file_has_one_fsynced_json_line_per_record_in_arrival_order(self):
        server = self.start()
        self.request(server, MANIFEST)
        self.request(server, "/a")
        raw = socket.create_connection(("127.0.0.1", server.port), timeout=10)
        with self.assertRaises(ssl.SSLError):
            ssl.create_default_context().wrap_socket(raw, server_hostname="github.com")
        self.wait_for(server, 3)
        lines = [json.loads(line) for line in Path(server.log).read_text().splitlines()]
        self.assertEqual(lines, server.requests())
        self.assertEqual([item["seq"] for item in lines], [1, 2, 3])
        self.assertEqual(sorted(item["kind"] for item in lines), ["request", "request", "tls_failure"])
        for item in lines:
            for key in ("t", "kind", "sni", "host", "scenario", "seq"):
                self.assertIn(key, item)
        for item in (line for line in lines if line["kind"] == "request"):
            for key in ("method", "path", "query", "status", "bytes_out", "user_agent", "remote", "role", "payload"):
                self.assertIn(key, item)

    def test_requests_returns_copies(self):
        server = self.start()
        self.request(server, "/a")
        items = self.wait_for(server, 1)
        items[0]["path"] = "tampered"
        self.assertEqual(server.requests()[0]["path"], "/a")

    def test_the_secondary_address_is_optional(self):
        server = self.start(listen_host="127.0.0.1,::1")
        self.assertEqual(self.request(server, "/a")[0], 404)
        if not server.bind_warnings:
            self.assertEqual(self.request(server, "/b", address="::1")[0], 404)

    def test_stop_releases_the_port(self):
        server = ms.MitmServer(self.info["server_cert"], self.info["server_key"], port=0)
        server.start()
        port = server.port
        server.stop()
        with self.assertRaises(OSError):
            socket.create_connection(("127.0.0.1", port), timeout=1)

    def test_binding_a_taken_port_is_an_error(self):
        first = self.start()
        second = ms.MitmServer(self.info["server_cert"], self.info["server_key"], port=first.port)
        with self.assertRaises(OSError):
            second.start()


class CliTests(Fixture):
    def test_cli_serves_writes_the_ready_file_and_stops_on_sigterm(self):
        if not hasattr(signal, "SIGTERM"):
            self.skipTest("needs POSIX signals")
        log = Path(self.tmp.name) / "cli-log.jsonl"
        ready = Path(self.tmp.name) / "cli-ready.json"
        if ready.exists():
            ready.unlink()
        process = subprocess.Popen(
            [sys.executable, "-B", str(_paths.LAB / "lib" / "mitm_server.py"), "--scenario", "bait-0.8.11", "--cert", self.info["server_cert"],
             "--key", self.info["server_key"], "--listen", "127.0.0.1:0", "--log", str(log), "--ready-file", str(ready)],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        try:
            end = time.time() + 15
            while time.time() < end and not ready.exists():
                time.sleep(0.05)
            self.assertTrue(ready.exists(), "no ready file")
            info = json.loads(ready.read_text())
            self.assertEqual(info["scenario"], "bait-0.8.11")
            self.assertGreater(info["port"], 0)
            raw = socket.create_connection(("127.0.0.1", info["port"]), timeout=10)
            tls = self.trusting.wrap_socket(raw, server_hostname="github.com")
            connection = http.client.HTTPSConnection("github.com")
            connection.sock = tls
            connection.request("GET", MANIFEST, headers={"Host": "github.com"})
            response = connection.getresponse()
            self.assertEqual(json.loads(response.read())["version"], "0.8.11")
            process.send_signal(signal.SIGTERM)
            self.assertEqual(process.wait(timeout=15), 0)
        finally:
            if process.poll() is None:
                process.kill()
            process.stdout.close()
        lines = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual([item["role"] for item in lines], ["manifest"])

    def test_cli_rejects_a_bad_listen_value(self):
        import contextlib
        import io

        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            ms.main(["--cert", "c", "--key", "k", "--log", "l", "--listen", "nonsense"])

    def test_parse_listen(self):
        self.assertEqual(ms.parse_listen("127.0.0.1:443"), ("127.0.0.1", 443))
        self.assertEqual(ms.parse_listen("127.0.0.1,::1:8443"), ("127.0.0.1,::1", 8443))


if __name__ == "__main__":
    unittest.main()
