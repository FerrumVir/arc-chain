"""Tests for wave0-lab-desktop/native-updater-check, the REAL tauri-plugin-updater 2.10.1 check() driver (THROWAWAY LAB FILE).

Static tests always run (pins, lockfile, gitignore, no install call). Dynamic tests run the compiled binary against a
local fake server and are skipped when the binary has not been built (set WAVE0_NATIVE_UPDATER_CHECK to its path, or
build it into wave0-lab-desktop/native-updater-check/target/release/)."""
from __future__ import annotations

import json
import os
import platform
import re
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import _paths  # noqa: F401

CRATE = _paths.LAB / "native-updater-check"
EXE = "native-updater-check.exe" if os.name == "nt" else "native-updater-check"
BINARY = Path(os.environ.get("WAVE0_NATIVE_UPDATER_CHECK") or (CRATE / "target" / "release" / EXE))
SCHEMA = "arc.legacy-bridge.wave0-lab.native-updater-check.v1"
# plugins.updater.pubkey of the released v0.7.11 desktop app (desktop/src-tauri/tauri.conf.json at the tag; a PUBLIC key).
PUBKEY = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDlBOTcwQ0FBQ0U1NjQ3M0IKUldRN1IxYk9xZ3lYbWcrbkhkWnZlc0tmWW1uTTlhcDljLzF4cUZtUUVibTRNa2V4TjBoNHJqY2EK"
ENDPOINT_PATH = "/FerrumVir/arc-chain/releases/latest/download/latest.json"
REDIRECT_TARGET = "/FerrumVir/arc-chain/releases/download/v0.7.12/latest.json"
# Versions read from the strings of the released v0.7.11 Linux binary (see Cargo.toml): the lockfile must hold them.
SHIPPED = {
    "tauri": "2.11.2",
    "tauri-plugin-updater": "2.10.1",
    "tauri-utils": "2.9.2",
    "tauri-runtime": "2.11.2",
    "reqwest": "0.13.4",
    "hyper": "1.10.1",
    "hyper-util": "0.1.20",
    "http": "1.4.2",
    "rustls": "0.23.40",
    "rustls-webpki": "0.103.13",
    "rustls-pki-types": "1.14.1",
    "tokio": "1.52.3",
    "tokio-rustls": "0.26.4",
    "tower": "0.5.3",
    "url": "2.5.8",
    "ring": "0.17.14",
    "semver": "1.0.28",
    "minisign-verify": "0.2.5",
    "percent-encoding": "2.3.2",
    "idna": "1.1.0",
    "tar": "0.4.46",
}


def lock_packages() -> dict:
    text = (CRATE / "Cargo.lock").read_text(encoding="utf-8")
    found: dict = {}
    for block in text.split("[[package]]")[1:]:
        name = re.search(r'^name = "([^"]+)"', block, re.MULTILINE)
        version = re.search(r'^version = "([^"]+)"', block, re.MULTILINE)
        if name and version:
            found.setdefault(name.group(1), []).append(version.group(1))
    return found


class StaticTests(unittest.TestCase):
    def test_manifest_pins_the_shipped_versions_exactly(self):
        manifest = (CRATE / "Cargo.toml").read_text(encoding="utf-8")
        self.assertRegex(manifest, r'tauri = \{ version = "=2\.11\.2", default-features = false, features = \["test"\] \}')
        self.assertRegex(manifest, r'(?m)^tauri-plugin-updater = "=2\.10\.1"$')
        self.assertNotRegex(manifest, r"tauri-plugin-updater = \{[^}]*default-features\s*=\s*false", "the shipped app used the plugin's default features (rustls-tls + zip)")
        self.assertRegex(manifest, r'(?m)^tauri-utils = "=2\.9\.2"$')
        self.assertIn("\n[workspace]\n", manifest, "the crate must stay independent of the application workspace")
        code = "\n".join(line for line in manifest.splitlines() if not line.lstrip().startswith("#"))
        self.assertNotRegex(code, r"\bwry\b", "no webview feature or dependency")

    def test_lockfile_holds_the_shipped_versions(self):
        packages = lock_packages()
        for name, version in SHIPPED.items():
            with self.subTest(name):
                self.assertIn(name, packages, f"{name} is not in Cargo.lock")
                self.assertIn(version, packages[name], f"{name} {version} is not locked (locked: {packages[name]})")
        for name in ("tauri", "tauri-plugin-updater"):
            self.assertEqual(packages[name], [SHIPPED[name]], f"exactly one {name}, at the shipped version")

    def test_lockfile_has_no_webview_runtime(self):
        """webkit2gtk / webview2-com are only the binding crates tauri-runtime itself depends on (Linux needs
        libwebkit2gtk-4.1-dev to build them); the wry runtime that would create a webview must be absent."""
        packages = lock_packages()
        for name in ("wry", "tauri-runtime-wry", "tao"):
            self.assertNotIn(name, packages, f"{name} must not be built: this is the mock runtime")

    def test_gitignore_ignores_target(self):
        lines = (CRATE / ".gitignore").read_text(encoding="utf-8").splitlines()
        self.assertIn("target/", [line.strip() for line in lines])

    def test_the_source_never_installs_anything(self):
        source = (CRATE / "src" / "main.rs").read_text(encoding="utf-8")
        code = "\n".join(line for line in source.splitlines() if not line.lstrip().startswith("//"))
        self.assertNotIn("download_and_install", source, "not even in a comment: the tool must never install")
        self.assertNotRegex(code, r"\.install\s*\(")
        self.assertNotIn("std::fs", code)
        self.assertNotIn("Command::new", code)
        self.assertIn(SCHEMA, source)
        self.assertIn("tauri-plugin-updater 2.10.1", source)
        self.assertRegex(code, r"app\s*\.updater\(\)", "the updater must come from the plugin configuration (UpdaterExt::updater)")
        self.assertIn("--control-download", source)

    def test_only_the_download_step_is_behind_the_control_flag(self):
        code = (CRATE / "src" / "main.rs").read_text(encoding="utf-8")
        self.assertEqual(len(re.findall(r"\.download\(", code)), 1)
        index = code.index(".download(")
        self.assertIn("args.control_download", code[max(0, index - 400):index])


def updater_target() -> str:
    system, machine = platform.system(), platform.machine().lower()
    arch = {"x86_64": "x86_64", "amd64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64"}.get(machine, machine)
    os_name = {"Linux": "linux", "Darwin": "darwin", "Windows": "windows"}[system]
    return f"{os_name}-{arch}"


class FakeServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, behaviour):
        self.behaviour = behaviour
        self.requests: list = []
        outer = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):  # silence
                pass

            def do_GET(self):  # noqa: N802
                outer.requests.append(self.path)
                status, headers, body = outer.behaviour(self.path, outer)
                self.send_response(status)
                for key, value in headers.items():
                    self.send_header(key, value)
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Connection", "close")
                self.end_headers()
                if status != 204:
                    self.wfile.write(body)

        super().__init__(("127.0.0.1", 0), Handler)

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.server_address[1]}"


def run_binary(args, timeout=120):
    env = {key: value for key, value in os.environ.items() if "proxy" not in key.lower()}
    env["NO_PROXY"] = "127.0.0.1,localhost"
    return subprocess.run([str(BINARY), *args], capture_output=True, text=True, timeout=timeout, env=env)


@unittest.skipUnless(BINARY.is_file(), f"the native binary is not built ({BINARY}); set WAVE0_NATIVE_UPDATER_CHECK or build the crate")
class DynamicTests(unittest.TestCase):
    def serve(self, behaviour):
        server = FakeServer(behaviour)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(lambda: (server.shutdown(), server.server_close()))
        return server

    def check(self, server, *extra, endpoint_path=ENDPOINT_PATH):
        done = run_binary(["--endpoint", server.base + endpoint_path, "--current-version", "0.7.11", "--pubkey", PUBKEY, "--insecure-transport", *extra])
        self.assertEqual(done.returncode, 0, done.stderr)
        lines = done.stdout.splitlines()
        self.assertEqual(len(lines), 1, f"stdout must be exactly one JSON line: {done.stdout!r}")
        report = json.loads(lines[0])
        self.assertEqual(report["schema"], SCHEMA)
        self.assertEqual(report["plugin"], "tauri-plugin-updater 2.10.1")
        self.assertEqual(report["tauri"], "2.11.2")
        self.assertEqual(report["current_version"], "0.7.11")
        self.assertEqual(report["endpoints"], [server.base + endpoint_path])
        return report

    def test_404_is_release_not_found_and_nothing_else_is_requested(self):
        server = self.serve(lambda path, srv: (404, {"Content-Type": "text/plain"}, b"Not Found"))
        report = self.check(server)
        self.assertEqual(report["outcome"], "error")
        self.assertEqual(report["error_kind"], "ReleaseNotFound")
        self.assertEqual(report["error"], "Could not fetch a valid release JSON from the remote")
        self.assertIsNone(report["update"])
        self.assertFalse(report["download_attempted"])
        self.assertNotIn("download_result", report)
        self.assertEqual(server.requests, [ENDPOINT_PATH], "exactly one request: the configured endpoint")

    def test_204_is_no_update(self):
        server = self.serve(lambda path, srv: (204, {}, b""))
        report = self.check(server)
        self.assertEqual(report["outcome"], "no_update")
        self.assertIsNone(report["error_kind"])
        self.assertIsNone(report["update"])
        self.assertEqual(server.requests, [ENDPOINT_PATH])

    def test_github_style_redirect_to_a_missing_latest_json_is_release_not_found(self):
        def behaviour(path, srv):
            if path == ENDPOINT_PATH:
                return 302, {"Location": REDIRECT_TARGET}, b""
            return 404, {}, b"Not Found"

        server = self.serve(behaviour)
        report = self.check(server)
        self.assertEqual(report["outcome"], "error")
        self.assertEqual(report["error_kind"], "ReleaseNotFound")
        self.assertEqual(server.requests, [ENDPOINT_PATH, REDIRECT_TARGET], "the endpoint and its redirect target, nothing else")

    def bait_behaviour(self):
        target = updater_target()

        def behaviour(path, srv):
            if path == ENDPOINT_PATH:
                manifest = {
                    "version": "0.8.11",
                    "notes": "x",
                    "pub_date": "2026-01-01T00:00:00Z",
                    "platforms": {target: {"url": f"{srv.base}/bundle.tar.gz", "signature": "dGhpcyBpcyBub3QgYSBzaWduYXR1cmU="}},
                }
                return 200, {"Content-Type": "application/json"}, json.dumps(manifest).encode()
            if path == "/bundle.tar.gz":
                return 200, {"Content-Type": "application/octet-stream"}, b"not a bundle"
            return 404, {}, b"Not Found"

        return behaviour

    def test_a_v08_latest_json_is_seen_as_an_update_without_downloading(self):
        server = self.serve(self.bait_behaviour())
        report = self.check(server)
        self.assertEqual(report["outcome"], "update_available")
        self.assertEqual(report["update"]["version"], "0.8.11")
        self.assertEqual(report["update"]["download_url"], server.base + "/bundle.tar.gz")
        self.assertFalse(report["download_attempted"])
        self.assertEqual(server.requests, [ENDPOINT_PATH], "check() alone never touches the bundle")

    def test_control_download_makes_the_bundle_request_visible(self):
        server = self.serve(self.bait_behaviour())
        report = self.check(server, "--control-download")
        self.assertEqual(report["outcome"], "update_available")
        self.assertTrue(report["download_attempted"])
        self.assertTrue(report["download_result"].startswith("error:"), report["download_result"])
        self.assertEqual(server.requests, [ENDPOINT_PATH, "/bundle.tar.gz"])

    def test_the_same_version_is_not_an_update(self):
        target = updater_target()

        def behaviour(path, srv):
            manifest = {"version": "0.7.11", "platforms": {target: {"url": f"{srv.base}/b", "signature": "c2ln"}}}
            return 200, {"Content-Type": "application/json"}, json.dumps(manifest).encode()

        server = self.serve(behaviour)
        report = self.check(server)
        self.assertEqual(report["outcome"], "no_update")
        self.assertEqual(server.requests, [ENDPOINT_PATH])

    def test_an_unreachable_endpoint_is_a_reported_error_not_a_crash(self):
        server = self.serve(lambda path, srv: (404, {}, b""))
        base = server.base
        server.shutdown()
        server.server_close()
        done = run_binary(["--endpoint", base + ENDPOINT_PATH, "--current-version", "0.7.11", "--pubkey", PUBKEY, "--insecure-transport"])
        self.assertEqual(done.returncode, 0, done.stderr)
        report = json.loads(done.stdout.splitlines()[0])
        self.assertEqual(report["outcome"], "error")
        self.assertEqual(report["error_kind"], "Reqwest")

    def test_usage_errors_exit_2_with_one_line(self):
        for args in ([], ["--endpoint", "http://127.0.0.1:1/x"], ["--bogus"], ["--endpoint", "http://127.0.0.1:1/x", "--current-version", "not-a-version", "--pubkey", PUBKEY, "--insecure-transport"]):
            with self.subTest(args=args):
                done = run_binary(args)
                self.assertEqual(done.returncode, 2)
                self.assertEqual(done.stdout, "")
                self.assertTrue(done.stderr.strip().startswith("native-updater-check:"))

    @unittest.skipUnless("release" in str(BINARY), "debug builds only warn about http endpoints")
    def test_an_http_endpoint_without_the_flag_is_refused_like_the_shipped_release_build(self):
        done = run_binary(["--endpoint", "http://127.0.0.1:1" + ENDPOINT_PATH, "--current-version", "0.7.11", "--pubkey", PUBKEY])
        self.assertEqual(done.returncode, 2)
        self.assertIn("secure protocol", done.stderr)


if __name__ == "__main__":
    unittest.main()
