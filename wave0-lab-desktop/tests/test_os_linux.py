"""Offline tests of wave0-lab-desktop/os_linux.py (THROWAWAY LAB FILE).

Nothing here touches the network, sudo, iptables, /etc/hosts or the trust store: every command goes through Ctx.run, which the
tests replace, and the module refuses to run outside a CI runner unless WAVE0_DESKTOP_ALLOW_LOCAL=1 is set (the tests set it)."""
from __future__ import annotations

import contextlib
import io
import json
import os
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

import _paths  # noqa: F401
import os_linux as ol
import stage_c_summary as summary_module

RELEASE = {
    "tag_name": "v0.7.11", "draft": False, "prerelease": False, "immutable": True,
    "assets": [
        {"name": "ARC.Node-0.7.11-1.x86_64.rpm", "size": 9169784, "digest": "sha256:f800b218bebbd9b13f5f669f4e1c2dafe82d15867ade000d871065b4c7607ddc"},
        {"name": "ARC.Node_0.7.11_amd64.AppImage", "size": 86108664, "digest": "sha256:9849f5dba4b9f90be8a0258c23a8c94ccbad0010e5ad8507a1ef2233531ce81b"},
        {"name": "ARC.Node_0.7.11_amd64.AppImage.sig", "size": 420, "digest": "sha256:170289eba4bdc2b94e2fbce5ff56bc0b2668894fa56760240c0f27cca01c9a4a"},
        {"name": "ARC.Node_0.7.11_amd64.deb", "size": 9168422, "digest": "sha256:db0df355bb17f23a02b9323adcd7506053a8969ee3d98a23cfe92d1b4219a9c3"},
        {"name": "ARC.Node_0.7.11_x64-setup.exe", "size": 4864230, "digest": "sha256:61a01c0f0253d040110fc9bc14058afdff864d6b9c316115acf8c58ae45d1e15"},
        {"name": "latest.json", "size": 2418, "digest": "sha256:0596969b4fc4622c01989af8e0483f4dc1a650968040df0d863f7485977c12d4"},
    ],
}
DEB_SHA = "db0df355bb17f23a02b9323adcd7506053a8969ee3d98a23cfe92d1b4219a9c3"


def manifest_rows(redirect: bool = True) -> list:
    rows = [{"kind": "request", "t": 1.0, "host": "github.com", "path": ol.MANIFEST_PATH, "status": 302 if redirect else 404, "role": "manifest", "payload": False}]
    if redirect:
        rows.append({"kind": "request", "t": 1.1, "host": "github.com", "path": ol.REDIRECT_PATH, "status": 404, "role": "manifest_redirect", "payload": False})
    return rows


def bait_rows() -> list:
    return [{"kind": "request", "t": 1.0, "host": "github.com", "path": ol.MANIFEST_PATH, "status": 200, "role": "manifest", "payload": False}]


def table(*rows):
    return [dict(pid=pid, ppid=1, uid=1000, exe=exe, cmd=cmd) for pid, exe, cmd in rows]


APP = "/work/app/usr/bin/arc-desktop"
ROOT = "/work/app"
SANDBOX = Path("/work/sandbox/clean")
BASE_PROCS = table((100, "/usr/bin/Xvfb", "Xvfb :99"), (200, APP, APP), (201, "/usr/lib/x86_64-linux-gnu/webkit2gtk-4.1/WebKitWebProcess", "WebKitWebProcess"))


def good_data(scenario: str = "latest-404") -> dict:
    clean = scenario == "latest-404"
    trigger = ol.normalize_webdriver_trigger({"ok": False, "error": ol.RELEASE_NOT_FOUND}) if clean else \
        ol.normalize_webdriver_trigger({"ok": True, "value": {"version": "0.8.11", "currentVersion": "0.7.11", "rid": 7}})
    return {
        "scenario": scenario, "trigger": trigger, "requests": manifest_rows() if clean else bait_rows(),
        "writes_paths": [str(SANDBOX / "home" / ".local" / "share" / "network.arc.desktop" / "store.json"), str(SANDBOX / "tmp" / "xvfb-run.abc" / "Xauthority")],
        "fs_diff": {"added": [str(SANDBOX / "home" / ".cache" / "network.arc.desktop")], "removed": [], "changed": []},
        "outside": {"ok": True, "paths": [], "roots": ["/opt"]},
        "procs_before": BASE_PROCS, "procs_after": BASE_PROCS + table((300, "/usr/bin/sleep", "sleep 1")),
        "binary_before": DEB_SHA, "binary_after": DEB_SHA, "app_binary": APP, "app_root": ROOT, "expect_app": True,
        "expected_prefixes": ol.expected_prefixes(SANDBOX), "uid": 1000, "owner_of": lambda path: 1000,
    }


class FakeWebDriver(BaseHTTPRequestHandler):
    """A tiny W3C WebDriver server: /status, /session, timeouts, execute/sync, execute/async, delete."""
    reject_named = False
    async_value = {"ok": True, "value": None}
    scripts: list = []
    sessions: list = []

    def log_message(self, *args):  # silence
        pass

    def _send(self, status, value):
        data = json.dumps({"value": value}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(length) or b"{}")

    def do_GET(self):
        if self.path == "/status":
            return self._send(200, {"ready": True, "message": "fake"})
        self._send(404, {"error": "unknown command", "message": self.path})

    def do_DELETE(self):
        if self.path == "/session/abc":
            return self._send(200, None)
        self._send(404, {"error": "invalid session id", "message": self.path})

    def do_POST(self):
        body = self._body()
        if self.path == "/session":
            caps = body["capabilities"]["alwaysMatch"]
            FakeWebDriver.sessions.append(caps)
            if FakeWebDriver.reject_named and "browserName" in caps:
                return self._send(500, {"error": "session not created", "message": "browserName does not match"})
            return self._send(200, {"sessionId": "abc", "capabilities": caps})
        if self.path == "/session/abc/timeouts":
            return self._send(200, None)
        if self.path == "/session/abc/execute/sync":
            FakeWebDriver.scripts.append(("sync", body["script"], body["args"]))
            return self._send(200, True)
        if self.path == "/session/abc/execute/async":
            FakeWebDriver.scripts.append(("async", body["script"], body["args"]))
            value = FakeWebDriver.async_value
            if isinstance(value, Exception):
                return self._send(500, {"error": "javascript error", "message": "boom"})
            return self._send(200, value)
        self._send(404, {"error": "unknown command", "message": self.path})


@contextlib.contextmanager
def fake_driver(reject_named=False, async_value=None):
    FakeWebDriver.reject_named = reject_named
    FakeWebDriver.async_value = async_value if async_value is not None else {"ok": True, "value": None}
    FakeWebDriver.scripts, FakeWebDriver.sessions = [], []
    server = ThreadingHTTPServer(("127.0.0.1", 0), FakeWebDriver)
    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)
    thread.start()
    try:
        yield server.server_address[1]
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


class AssetTests(unittest.TestCase):
    def test_the_deb_is_chosen_with_its_release_digest_and_the_exact_url(self):
        asset = ol.select_linux_asset(RELEASE)
        self.assertEqual(asset["name"], "ARC.Node_0.7.11_amd64.deb")
        self.assertEqual((asset["sha256"], asset["release_digest"], asset["size"], asset["kind"]), (DEB_SHA, "sha256:" + DEB_SHA, 9168422, "deb"))
        self.assertEqual(asset["url"], "https://github.com/FerrumVir/arc-chain/releases/download/v0.7.11/ARC.Node_0.7.11_amd64.deb")

    def test_the_appimage_is_the_fallback(self):
        release = dict(RELEASE, assets=[a for a in RELEASE["assets"] if not a["name"].endswith(".deb")])
        asset = ol.select_linux_asset(release)
        self.assertEqual((asset["name"], asset["kind"]), ("ARC.Node_0.7.11_amd64.AppImage", "appimage"))

    def test_nothing_unsafe_is_accepted(self):
        assets = RELEASE["assets"]
        bad = {
            "wrong tag": dict(RELEASE, tag_name="v0.7.12"),
            "draft": dict(RELEASE, draft=True),
            "no assets": {"tag_name": "v0.7.11"},
            "no linux asset": dict(RELEASE, assets=[a for a in assets if "amd64" not in a["name"]]),
            "no digest": dict(RELEASE, assets=[dict(a, digest=None) if a["name"].endswith(".deb") else a for a in assets if not a["name"].endswith("AppImage")]),
            "short digest": dict(RELEASE, assets=[dict(a, digest="sha256:abc") if a["name"].endswith(".deb") else a for a in assets if not a["name"].endswith("AppImage")]),
            "no size": dict(RELEASE, assets=[dict(a, size=0) if a["name"].endswith(".deb") else a for a in assets if not a["name"].endswith("AppImage")]),
            "foreign url": dict(RELEASE, assets=[dict(a, browser_download_url="https://example.com/x.deb") if a["name"].endswith(".deb") else a for a in assets]),
        }
        for name, release in bad.items():
            with self.subTest(name), self.assertRaises(ol.AssetError):
                ol.select_linux_asset(release)
        with self.assertRaises(ol.AssetError):
            ol.select_linux_asset("not a dict")

    def test_digest_parsing(self):
        self.assertEqual(ol.parse_sha256_digest("SHA256:" + DEB_SHA.upper()), DEB_SHA)
        self.assertEqual(ol.parse_sha256_digest(DEB_SHA), DEB_SHA)
        for value in (None, 5, "", "sha256:", "sha1:" + "a" * 40, "g" * 64, "a" * 63):
            self.assertIsNone(ol.parse_sha256_digest(value))

    def test_config_asset_reads_the_core_config_shape(self):
        config = {"app": {"assets": {"linux_deb": {"name": "ARC.Node_0.7.11_amd64.deb", "size": 9168422, "sha256": DEB_SHA, "release_digest": "sha256:" + DEB_SHA}}}}
        self.assertEqual(ol.config_asset(config)["sha256"], DEB_SHA)
        self.assertIsNone(ol.config_asset({"app": {"assets": {"linux_deb": {"name": "x.deb", "sha256": DEB_SHA}}}}))
        self.assertIsNone(ol.config_asset({}))
        self.assertIsNone(ol.config_asset(None))
        real = json.loads((_paths.LAB / "config.json").read_text(encoding="utf-8")) if (_paths.LAB / "config.json").is_file() else None
        if real is not None:
            self.assertEqual(ol.config_asset(real)["sha256"], DEB_SHA, "config.json must record the same digest the release shows")

    def test_release_fetch_sends_the_token_only_in_the_header_and_parses(self):
        seen = {}

        class Response:
            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

            def read(self, n=-1):
                return json.dumps(RELEASE).encode()

        def opener(request, timeout=None):
            seen["url"] = request.full_url
            seen["auth"] = request.get_header("Authorization")
            return Response()

        self.assertEqual(ol.fetch_release_json("tok", opener=opener)["tag_name"], "v0.7.11")
        self.assertEqual(seen["url"], "https://api.github.com/repos/FerrumVir/arc-chain/releases/tags/v0.7.11")
        self.assertEqual(seen["auth"], "Bearer tok")
        ol.fetch_release_json(None, opener=opener)
        self.assertIsNone(seen["auth"])

    def test_the_download_is_checked_against_the_release_digest(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "x.deb"
            path.write_bytes(b"not the package")
            asset = dict(ol.select_linux_asset(RELEASE))
            facts = ol.verify_download(path, asset)
            self.assertFalse(facts["digest_match"])
            self.assertFalse(facts["size_match"])
            asset["sha256"], asset["size"] = ol.sha256_file(path), path.stat().st_size
            self.assertTrue(ol.verify_download(path, asset)["digest_match"])

    def test_binary_strings_and_ldd_and_lockfile(self):
        blob = b"\0/registry/src/index/tauri-plugin-updater-2.10.1/src/updater.rs\0tauri-2.11.2/src\0tauri-utils-2.9.2/\0reqwest-0.12.28/\0reqwest-0.13.4/\0"
        versions = ol.binary_crate_versions(blob)
        self.assertEqual(versions["tauri-plugin-updater"], ["2.10.1"])
        self.assertEqual(versions["tauri"], ["2.11.2"])
        self.assertEqual(versions["reqwest"], ["0.12.28", "0.13.4"])
        self.assertEqual(ol.parse_ldd_missing("\tlibfoo.so.1 => not found\n\tlibc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x1)\n"), ["libfoo.so.1"])
        self.assertEqual(ol.lockfile_plugin_version(), "2.10.1", "the native crate's lockfile pins the shipped plugin version")
        self.assertIsNone(ol.lockfile_plugin_version(Path("/nonexistent/Cargo.lock")))


class WebDriverTests(unittest.TestCase):
    def test_session_flow_and_scripts(self):
        with fake_driver(async_value={"ok": False, "error": ol.RELEASE_NOT_FOUND}) as port:
            client = ol.WebDriverClient(port=port, timeout=5)
            self.assertTrue(client.wait_ready(5))
            session, notes = ol.open_session(client, "/work/app/usr/bin/arc-desktop")
            self.assertEqual((session, notes), ("abc", []))
            client.set_timeouts()
            self.assertTrue(ol.wait_tauri_ready(client, 5))
            raw = client.execute_async(ol.CHECK_SCRIPT)
            self.assertEqual(ol.normalize_webdriver_trigger(raw)["outcome"], "release_not_found")
            client.delete_session()
            self.assertIsNone(client.session_id)
        kinds = [item[0] for item in FakeWebDriver.scripts]
        self.assertEqual(kinds, ["sync", "async"])
        self.assertEqual(FakeWebDriver.sessions[0]["webkitgtk:browserOptions"]["binary"], "/work/app/usr/bin/arc-desktop")
        self.assertEqual(FakeWebDriver.sessions[0]["browserName"], "wry")

    def test_the_second_capability_set_drops_browser_name(self):
        with fake_driver(reject_named=True) as port:
            client = ol.WebDriverClient(port=port, timeout=5)
            session, notes = ol.open_session(client, "/app")
            self.assertEqual(session, "abc")
            self.assertEqual(len(notes), 1)
            self.assertIn("browserName", notes[0])
            self.assertNotIn("browserName", FakeWebDriver.sessions[-1])

    def test_a_driver_that_refuses_both_attempts_raises_with_both_reasons(self):
        with fake_driver(reject_named=True) as port:
            client = ol.WebDriverClient(port=port, timeout=5)
            with mock.patch.object(ol, "capability_attempts", lambda binary: [{"capabilities": {"alwaysMatch": {"browserName": "x"}}}] * 2):
                with self.assertRaises(ol.WebDriverError) as caught:
                    ol.open_session(client, "/app")
        self.assertIn("attempt 1", caught.exception.message)
        self.assertIn("attempt 2", caught.exception.message)

    def test_webdriver_errors_carry_the_w3c_error_code(self):
        with fake_driver(async_value=RuntimeError("x")) as port:
            client = ol.WebDriverClient(port=port, timeout=5)
            client.new_session(ol.capability_attempts("/app")[0])
            with self.assertRaises(ol.WebDriverError) as caught:
                client.execute_async("return 1")
        self.assertEqual(caught.exception.error, "javascript error")
        self.assertEqual(caught.exception.status, 500)

    def test_no_driver_listening_means_not_ready_and_no_session(self):
        client = ol.WebDriverClient(port=1, timeout=1)  # nothing listens on port 1
        self.assertFalse(client.wait_ready(0.6, interval=0.1))
        with self.assertRaises(ol.WebDriverError):
            client.execute_async("return 1")  # no session yet

    def test_the_scripts_call_the_plugin_commands(self):
        self.assertIn("plugin:updater|check", ol.CHECK_SCRIPT)
        self.assertIn("__TAURI_INTERNALS__", ol.CHECK_SCRIPT)
        self.assertIn("arguments[arguments.length - 1]", ol.CHECK_SCRIPT)
        self.assertIn("plugin:updater|download", ol.DOWNLOAD_CONTROL_SCRIPT)
        self.assertIn("__CHANNEL__:", ol.DOWNLOAD_CONTROL_SCRIPT)
        self.assertNotIn("download", ol.CHECK_SCRIPT.replace("plugin:updater|check", ""), "the check trigger must not mention a download")
        self.assertNotIn("install", ol.CHECK_SCRIPT)


class TriggerTests(unittest.TestCase):
    def test_webdriver_outcomes(self):
        n = ol.normalize_webdriver_trigger
        self.assertEqual(n({"ok": True, "value": None})["outcome"], "no_update")
        found = n({"ok": True, "value": {"version": "0.8.11", "currentVersion": "0.7.11", "rid": 3}})
        self.assertEqual((found["outcome"], found["version"], found["current_version"], found["rid"]), ("update_available", "0.8.11", "0.7.11", 3))
        self.assertEqual(n({"ok": False, "error": ol.RELEASE_NOT_FOUND})["outcome"], "release_not_found")
        self.assertEqual(n({"ok": False, "error": ol.RELEASE_NOT_FOUND})["text"], ol.RELEASE_NOT_FOUND)
        self.assertEqual(n({"ok": False, "error": "error sending request for url"})["outcome"], "error")
        self.assertEqual(n({"ok": False, "thrown": "TypeError: x"})["text"], "TypeError: x")
        for junk in (None, "x", {"value": 1}, {"ok": True, "value": 5}):
            self.assertEqual(n(junk)["outcome"], "transport_failure")

    def test_native_outcomes(self):
        schema = "arc.legacy-bridge.wave0-lab.native-updater-check.v1"
        n = ol.normalize_native_trigger
        error = n({"schema": schema, "outcome": "error", "error_kind": "ReleaseNotFound", "error": ol.RELEASE_NOT_FOUND, "current_version": "0.7.11"})
        self.assertEqual((error["outcome"], error["text"]), ("release_not_found", ol.RELEASE_NOT_FOUND))
        found = n({"schema": schema, "outcome": "update_available", "update": {"version": "0.8.11", "download_url": "u"}, "current_version": "0.7.11", "download_attempted": False})
        self.assertEqual((found["outcome"], found["version"]), ("update_available", "0.8.11"))
        self.assertEqual(n({"schema": schema, "outcome": "no_update"})["outcome"], "no_update")
        self.assertEqual(n({"schema": "other"})["outcome"], "transport_failure")
        self.assertEqual(n(None)["outcome"], "transport_failure")
        self.assertEqual(n({"schema": schema, "outcome": "weird"})["outcome"], "transport_failure")

    def test_a_trigger_is_only_definitive_when_it_tests_what_the_case_needs(self):
        release_not_found = ol.normalize_webdriver_trigger({"ok": False, "error": ol.RELEASE_NOT_FOUND})
        bait = ol.normalize_webdriver_trigger({"ok": True, "value": {"version": "0.8.11", "currentVersion": "0.7.11"}})
        no_update = ol.normalize_webdriver_trigger({"ok": True, "value": None})
        tls_error = ol.normalize_webdriver_trigger({"ok": False, "error": "error sending request: invalid peer certificate"})
        self.assertTrue(ol.trigger_definitive("latest-404", release_not_found)[0])
        self.assertTrue(ol.trigger_definitive("bait-0.8.11", bait)[0])
        for scenario, trigger in (("latest-404", no_update), ("latest-404", tls_error), ("latest-404", bait), ("bait-0.8.11", release_not_found),
                                  ("bait-0.8.11", no_update), ("bait-0.8.11", None), ("latest-404", None), ("nonsense", bait)):
            self.assertFalse(ol.trigger_definitive(scenario, trigger)[0], (scenario, trigger))
        wrong = ol.normalize_webdriver_trigger({"ok": True, "value": {"version": "0.9.0", "currentVersion": "0.7.11"}})
        self.assertFalse(ol.trigger_definitive("bait-0.8.11", wrong)[0])


class RequestSummaryTests(unittest.TestCase):
    def test_counts_and_separation_of_tls_failures(self):
        rows = manifest_rows() + [{"kind": "tls_failure", "sni": "api.github.com", "host": "api.github.com", "error": "unknown ca"}, {"kind": "tls_failure", "sni": None, "host": None}]
        summary = ol.summarize_requests(rows)
        self.assertEqual(summary["total"], 2)
        self.assertEqual(sum(count for _h, _p, count in summary["by_host_path"]), summary["total"], "the summary judge needs total == sum of the counts")
        self.assertEqual(summary["manifest"], 2)
        self.assertEqual(summary["non_manifest"], [])
        self.assertEqual(summary["tls_failures"], [["(no SNI)", 1], ["api.github.com", 1]])
        self.assertTrue(all(len(row) == 3 and row[0] == "github.com" for row in summary["by_host_path"]))

    def test_payloads_assets_and_other_paths_are_found(self):
        base = "/FerrumVir/arc-chain/releases/download/v0.8.11/"
        rows = bait_rows() + [
            {"kind": "request", "host": "github.com", "path": base + "ARC.Node_0.8.11_amd64.AppImage", "status": 404, "payload": True},
            {"kind": "request", "host": "github.com", "path": base + "ARC.Node_aarch64.app.tar.gz.sig", "status": 404, "role": "payload"},
            {"kind": "request", "host": "github.com", "path": "/FerrumVir/arc-chain/releases/download/v0.7.12/arc-node-linux-x86_64", "status": 404},
            {"kind": "request", "host": "api.github.com", "path": ol.API_LATEST_PATH, "status": 200, "role": "api_latest"},
            {"kind": "request", "host": "github.com", "path": "/FerrumVir/arc-chain/releases/download/v0.8.11/latest.json", "status": 404, "role": "other_manifest"},
        ]
        summary = ol.summarize_requests(rows)
        self.assertEqual(len(summary["payload"]), 2)
        self.assertEqual(len(summary["release_assets"]), 3, "any release asset counts, latest.json does not")
        self.assertIn("api.github.com" + ol.API_LATEST_PATH, summary["non_manifest"])
        self.assertIn("github.com/FerrumVir/arc-chain/releases/download/v0.8.11/latest.json", summary["non_manifest"], "a latest.json other than the manifest chain is not the manifest URL")

    def test_a_payload_suffix_is_enough_even_without_the_servers_flag(self):
        for suffix in (".sig", ".tar.gz", ".deb", ".AppImage", ".exe", ".msi", ".dmg", ".rpm"):
            self.assertTrue(ol.is_payload_row({"path": "/x/y" + suffix}), suffix)
        self.assertFalse(ol.is_payload_row({"path": ol.MANIFEST_PATH}))


class ProcessTests(unittest.TestCase):
    def test_read_process_table_from_a_fake_proc(self):
        with tempfile.TemporaryDirectory() as tmp:
            for pid, exe, cmd, ppid in ((10, "/usr/bin/a", b"a\0--x\0", 1), (20, "/work/app/usr/bin/arc-desktop (deleted)", b"arc-desktop\0", 10)):
                base = Path(tmp) / str(pid)
                base.mkdir()
                os.symlink(exe, str(base / "exe"))
                (base / "cmdline").write_bytes(cmd)
                (base / "status").write_text("Name:\tx\nPPid:\t%d\nUid:\t1000\t1000\t1000\t1000\n" % ppid)
            (Path(tmp) / "self").mkdir()
            rows = ol.read_process_table(tmp)
        self.assertEqual([row["pid"] for row in rows], [10, 20])
        self.assertEqual(rows[1]["exe"], "/work/app/usr/bin/arc-desktop")
        self.assertEqual((rows[1]["ppid"], rows[1]["uid"]), (10, 1000))
        self.assertEqual(rows[0]["cmd"], "a --x")
        self.assertEqual(ol.read_process_table("/nonexistent-proc"), [])
        self.assertIn("20 10 1000 /work/app/usr/bin/arc-desktop", ol.format_process_table(rows))

    def test_findings(self):
        quiet = ol.process_findings(BASE_PROCS, BASE_PROCS + table((300, "/usr/bin/sleep", "sleep 1")), APP, ROOT)
        self.assertTrue(quiet["known"])
        self.assertEqual((quiet["new_app"], quiet["new_installers"], quiet["app_before"], quiet["app_after"]), ([], [], 1, 1))
        relaunch = ol.process_findings(BASE_PROCS, BASE_PROCS + table((301, APP, APP + " --minimized")), APP, ROOT)
        self.assertEqual(len(relaunch["new_app"]), 1)
        under_root = ol.process_findings(BASE_PROCS, BASE_PROCS + table((302, ROOT + "/usr/bin/other", "other")), APP, ROOT)
        self.assertEqual(len(under_root["new_app"]), 1)
        other_name = ol.process_findings(BASE_PROCS, BASE_PROCS + table((303, "/tmp/x/arc-desktop", "x")), APP, ROOT)
        self.assertEqual(len(other_name["new_app"]), 1, "an app binary anywhere counts")
        installer = ol.process_findings(BASE_PROCS, BASE_PROCS + table((304, "/usr/bin/dpkg", "dpkg -i x.deb"), (305, None, "apt-get install x")), APP, ROOT)
        self.assertEqual(len(installer["new_installers"]), 2)
        self.assertEqual(ol.process_findings(None, BASE_PROCS, APP, ROOT), {"known": False})
        self.assertEqual(ol.process_findings(BASE_PROCS, None, APP, ROOT), {"known": False})


class WriteClassificationTests(unittest.TestCase):
    def setUp(self):
        self.expected = ol.expected_prefixes(SANDBOX)
        self.home = SANDBOX / "home"

    def classify(self, path):
        return ol.classify_write(str(path), ROOT, self.expected)

    def test_classes(self):
        self.assertEqual(self.classify(self.home / ".config" / "autostart" / "ARC Node.desktop"), "expected")
        self.assertEqual(self.classify(self.home / ".local" / "share" / "network.arc.desktop" / "store.json"), "expected")
        self.assertEqual(self.classify(self.home / ".cache" / "network.arc.desktop"), "expected")
        self.assertEqual(self.classify(SANDBOX / "tmp" / "xvfb-run.1" / "Xauthority"), "expected")
        self.assertEqual(self.classify("/tmp/.X99-lock"), "transient")
        self.assertEqual(self.classify("/tmp/.X11-unix/X99"), "transient")
        self.assertEqual(self.classify("/tmp/dbus-AbCdEf1234"), "transient")
        self.assertEqual(self.classify(ROOT + "/usr/bin/arc-desktop"), "app_root_write")
        self.assertEqual(self.classify(self.home / ".local" / "share" / "network.arc.desktop" / "ARC.Node_0.8.11_amd64.AppImage"), "update_artifact", "an update artifact is never expected, even in an expected folder")
        self.assertEqual(self.classify(self.home / ".cache" / "latest.json"), "update_artifact")
        self.assertEqual(self.classify(self.home / ".arc" / "bin" / "arc-node-linux-x86_64.partial"), "update_artifact")
        self.assertEqual(self.classify(self.home / ".arc" / "bin" / "arc-node"), "unexpected")
        self.assertEqual(self.classify("/usr/local/bin/arc"), "unexpected")
        self.assertEqual(self.classify(self.home / "Downloads" / "x"), "unexpected")

    def test_findings_ignore_other_users_but_not_unknown_owners(self):
        owners = {"/tmp/systemd-private-x": 0, "/gone": None, "/usr/local/bin/arc": 1000}
        found = ol.writes_findings(["/tmp/systemd-private-x", "/gone", "/usr/local/bin/arc", str(self.home / ".cache" / "c")], ROOT, self.expected, 1000, owner_of=lambda p: owners.get(p, 1000))
        self.assertEqual(found["classes"]["foreign_owner"], ["/tmp/systemd-private-x"])
        self.assertEqual(found["unexpected"], ["/gone", "/usr/local/bin/arc"])
        self.assertEqual(found["classes"]["expected"], [str(self.home / ".cache" / "c")])
        everything = ol.writes_findings(["/tmp/systemd-private-x"], ROOT, self.expected, None, owner_of=lambda p: 0)
        self.assertEqual(everything["unexpected"], ["/tmp/systemd-private-x"], "without a known uid nothing is filtered")


class EvaluateCaseTests(unittest.TestCase):
    def test_the_clean_case_passes_only_with_every_proof(self):
        result = ol.evaluate_case(good_data())
        self.assertEqual(result["verdict"], "PASS", result["reasons"])
        self.assertEqual(result["criteria"], {name: True for name in ol.CRITERIA})
        self.assertEqual(result["requests"]["total"], 2)
        self.assertEqual(result["reasons"], [])

    def test_the_bait_case_passes_with_an_applicable_update_and_no_download(self):
        result = ol.evaluate_case(good_data("bait-0.8.11"))
        self.assertEqual(result["verdict"], "PASS", result["reasons"])

    def mutate(self, **changes):
        data = good_data(changes.pop("scenario", "latest-404"))
        data.update(changes)
        return ol.evaluate_case(data)

    def test_missing_evidence_is_unproved_never_pass(self):
        cases = {
            "no request log": ({"requests": None}, ("only_manifest_url", "no_bundle_download")),
            "no manifest request (interception not shown)": ({"requests": []}, ("only_manifest_url", "no_bundle_download", "no_install")),
            "only tls failures": ({"requests": [{"kind": "tls_failure", "sni": "github.com"}]}, ("only_manifest_url",)),
            "no write log": ({"writes_paths": None}, ("no_new_files",)),
            "no snapshot diff": ({"fs_diff": None}, ("no_new_files",)),
            "outside search missing": ({"outside": None}, ("no_new_files",)),
            "outside search failed": ({"outside": {"ok": False, "paths": []}}, ("no_new_files",)),
            "no process snapshots": ({"procs_before": None}, ("no_new_app_launch", "no_install")),
            "no process snapshot after": ({"procs_after": None}, ("no_new_app_launch",)),
            "app not running before the trigger": ({"procs_before": table((100, "/usr/bin/Xvfb", "Xvfb"))}, ("no_new_app_launch",)),
            "no binary hash": ({"binary_after": None}, ("no_install",)),
            "no trigger": ({"trigger": None}, ()),
        }
        for name, (changes, nulls) in cases.items():
            with self.subTest(name):
                result = self.mutate(**changes)
                self.assertEqual(result["verdict"], "UNPROVED", result["reasons"])
                for criterion in nulls:
                    self.assertIsNone(result["criteria"][criterion], criterion)

    def test_the_trigger_must_do_what_the_case_needs(self):
        result = self.mutate(trigger=ol.normalize_webdriver_trigger({"ok": False, "error": "error sending request: invalid peer certificate"}))
        self.assertEqual(result["verdict"], "UNPROVED")
        self.assertTrue(any("invalid peer certificate" in reason for reason in result["reasons"]))
        result = self.mutate(scenario="bait-0.8.11", trigger=ol.normalize_webdriver_trigger({"ok": True, "value": None}))
        self.assertEqual(result["verdict"], "UNPROVED", "an update must be applicable for the bait case")

    def test_violations_are_failures(self):
        base = "/FerrumVir/arc-chain/releases/download/v0.8.11/"
        cases = {
            "a bundle was requested": ({"requests": manifest_rows() + [{"kind": "request", "host": "github.com", "path": base + "ARC.Node_0.8.11_amd64.AppImage", "status": 404, "payload": True}]},
                                       ("no_bundle_download", "no_install", "only_manifest_url")),
            "a signature was requested": ({"requests": manifest_rows() + [{"kind": "request", "host": "github.com", "path": base + "x.sig", "status": 404}]}, ("no_bundle_download",)),
            "a launcher asset was requested": ({"requests": manifest_rows() + [{"kind": "request", "host": "github.com", "path": "/FerrumVir/arc-chain/releases/download/v0.7.12/arc-node-linux-x86_64", "status": 404}]},
                                                ("no_bundle_download", "only_manifest_url")),
            "another URL was requested": ({"requests": manifest_rows() + [{"kind": "request", "host": "github.com", "path": "/FerrumVir/arc-chain/releases", "status": 404}]}, ("only_manifest_url",)),
            "a new app process": ({"procs_after": BASE_PROCS + table((400, APP, APP))}, ("no_new_app_launch",)),
            "an installer process": ({"procs_after": BASE_PROCS + table((401, "/usr/bin/dpkg", "dpkg -i x.deb"))}, ("no_install",)),
            "the binary changed": ({"binary_after": "0" * 64}, ("no_install",)),
            "an unexpected file": ({"writes_paths": good_data()["writes_paths"] + ["/home/runner/Downloads/update.bin"]}, ("no_new_files",)),
            "a write in the install dir": ({"fs_diff": {"added": [ROOT + "/usr/bin/arc-desktop.new"], "removed": [], "changed": []}}, ("no_new_files", "no_install")),
            "a stray install outside": ({"outside": {"ok": True, "paths": ["/usr/local/bin/arc-desktop"]}}, ("no_new_files",)),
            "an update artifact in an expected folder": ({"writes_paths": [str(SANDBOX / "home" / ".cache" / "update.tar.gz")]}, ("no_new_files",)),
        }
        for name, (changes, falses) in cases.items():
            with self.subTest(name):
                result = self.mutate(**changes)
                self.assertEqual(result["verdict"], "FAIL", result["reasons"])
                for criterion in falses:
                    self.assertIs(result["criteria"][criterion], False, criterion)

    def test_a_removal_is_not_a_new_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "writes.jsonl"
            log.write_text("\n".join(json.dumps(item) for item in (
                {"event": "added", "path": "/a/new"}, {"event": "changed", "path": "/a/chg"}, {"event": "removed", "path": "/a/gone"}, {"path": "/a/legacy"})) + "\nnot json\n")
            self.assertEqual(ol.writes_paths_from_log(log), ["/a/new", "/a/chg", "/a/legacy"])
            self.assertIsNone(ol.writes_paths_from_log(Path(tmp) / "missing.jsonl"))

    def test_tier_and_overall_verdicts(self):
        passing = {"name": "clean", "verdict": "PASS"}
        both = [passing, {"name": "cached-bait", "verdict": "PASS"}]
        self.assertEqual(ol.tier_result(both, ["clean", "cached-bait"]), "PASS")
        self.assertEqual(ol.tier_result([passing], ["clean", "cached-bait"]), "UNPROVED")
        self.assertEqual(ol.tier_result([], ["clean"]), "UNPROVED")
        self.assertEqual(ol.tier_result(both + [{"name": "x", "verdict": "FAIL"}], ["clean"]), "FAIL")
        self.assertEqual(ol.tier_result([{"name": "clean", "verdict": "UNPROVED"}], ["clean"]), "UNPROVED")


def good_result() -> dict:
    cases = []
    for name, scenario in ol.CASE_SCENARIO.items():
        analysis = ol.evaluate_case(good_data(scenario))
        cases.append({"name": name, "tier": "released_app", "scenario": scenario, "trigger_outcome": good_data(scenario)["trigger"], "requests": analysis["requests"],
                      "criteria": analysis["criteria"], "evidence_files": ["requests-%s.jsonl" % name], "verdict": analysis["verdict"]})
    return {
        "schema": ol.SCHEMA_RESULT, "os": "linux", "runner": {"image": "ubuntu24 x", "arch": "x86_64", "os_version": "Ubuntu 24.04"},
        "app": {"tag": "v0.7.11", "asset": "ARC.Node_0.7.11_amd64.deb", "asset_sha256": DEB_SHA, "release_digest": "sha256:" + DEB_SHA, "digest_match": True, "version_reported": "0.7.11"},
        "plugin": {"version": "2.10.1", "provenance": ["strings"]},
        "tiers": {"released_app": {"attempted": True, "status": "ran", "result": "PASS", "reason": "", "trigger": "x"},
                  "native_check": {"attempted": False, "status": "not_attempted", "result": "UNPROVED", "reason": "not requested", "trigger": None}},
        "cases": cases, "controls": [], "manifest404_error_text": ol.RELEASE_NOT_FOUND,
        "isolation": {"hosts_mapped": ol.hosts_lines(ol.MITM_HOSTS), "ca_sha256": "ab" * 32, "live_block": True},
        "verdict": "PASS",
    }


class ContractWithTheSummaryJudgeTests(unittest.TestCase):
    """The per-OS result must be judged PASS by stage_c_summary.py exactly when every proof is present."""

    def judge(self, document):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "result.json"
            raw = json.dumps(document).encode()
            path.write_bytes(raw)
            return summary_module.evaluate_os_result(document, raw, path, Path(tmp))

    def test_a_complete_result_is_pass(self):
        document = good_result()
        self.assertEqual(ol.overall_verdict(document), ("PASS", []))
        record = self.judge(document)
        self.assertEqual(record["verdict"], "PASS", record["reasons"])

    def test_every_missing_proof_is_not_pass_in_both_judges(self):
        def tweak(change):
            document = good_result()
            change(document)
            return document

        cases = {
            "digest unknown": lambda d: d["app"].update(digest_match=None),
            "digest mismatch": lambda d: d["app"].update(digest_match=False),
            "live block not proven": lambda d: d["isolation"].update(live_block=False),
            "no hosts mapping": lambda d: d["isolation"].update(hosts_mapped=[]),
            "plugin version unproven": lambda d: d["plugin"].update(version=""),
            "404 text missing": lambda d: d.update(manifest404_error_text=None),
            "a criterion unknown": lambda d: d["cases"][0]["criteria"].update(no_new_files=None),
            "a criterion false": lambda d: d["cases"][1]["criteria"].update(no_bundle_download=False),
            "a case missing": lambda d: d.update(cases=d["cases"][:1]),
            "tier unproved": lambda d: d["tiers"]["released_app"].update(result="UNPROVED"),
            "no tier attempted": lambda d: d["tiers"]["released_app"].update(attempted=False),
        }
        for name, change in cases.items():
            with self.subTest(name):
                document = tweak(change)
                self.assertNotEqual(ol.overall_verdict(document)[0], "PASS", name)
                self.assertNotEqual(self.judge(document)["verdict"], "PASS", name)

    def test_my_failure_modes_agree_with_the_judge(self):
        document = good_result()
        document["app"]["digest_match"] = False
        self.assertEqual(ol.overall_verdict(document)[0], "FAIL")
        self.assertEqual(self.judge(document)["verdict"], "FAIL")
        document = good_result()
        document["cases"][0]["criteria"]["only_manifest_url"] = False
        document["cases"][0]["verdict"] = "FAIL"
        document["tiers"]["released_app"]["result"] = "FAIL"
        self.assertEqual(ol.overall_verdict(document)[0], "FAIL")
        self.assertEqual(self.judge(document)["verdict"], "FAIL")


class MaskingTests(unittest.TestCase):
    IPS = ["192.0.2.1", "192.0.2.10"]

    def test_live_addresses_are_masked_exactly(self):
        text = "connect to 192.0.2.1:9090 failed; also 192.0.2.10 and 1192.0.2.1 and 192.0.2.100 and 10.0.0.2"
        masked = ol.mask_ips(text, self.IPS)
        self.assertEqual(masked, "connect to <live-ip-1>:9090 failed; also <live-ip-2> and 1192.0.2.1 and 192.0.2.100 and 10.0.0.2")

    def test_the_step_log_and_app_logs_never_carry_the_addresses(self):
        with tempfile.TemporaryDirectory() as tmp:
            log = ol.StepLog(Path(tmp) / "steps.log")
            log.mask_values = list(self.IPS)
            log.add("$ curl https://192.0.2.1/health -> rc 7")
            self.assertNotIn("192.0.2.1", (Path(tmp) / "steps.log").read_text().replace("192.0.2.10", ""))
            app = Path(tmp) / "app.log"
            app.write_text("WARN connecting to 192.0.2.10:9090\n" * 3)
            ol.trim_log(app, ips=self.IPS)
            self.assertNotIn("192.0.2", app.read_text())
            self.assertIn("<live-ip-2>", app.read_text())

    def test_counters_are_keyed_by_position(self):
        ctx_state = {"live_ips": self.IPS}

        class Fake:
            state = ctx_state

            def sudo(self, argv, **kw):
                return ol.CmdResult(0, "       4      240 REJECT     all  --  *      *       0.0.0.0/0            192.0.2.10           reject-with icmp-port-unreachable\n")

        self.assertEqual(ol.live_counters(Fake()), {"live-ip-1": 0, "live-ip-2": 4})


class NativeReportTests(unittest.TestCase):
    def test_the_report_is_found_among_diagnostics(self):
        report = {"schema": "arc.legacy-bridge.wave0-lab.native-updater-check.v1", "outcome": "no_update"}
        output = "2026-10-08T14:00:00Z DEBUG checking for updates https://github.com/x\n{not json}\n" + json.dumps(report) + "\ntrailing diagnostics\n"
        self.assertEqual(ol.last_json_report(output), report)
        self.assertIsNone(ol.last_json_report("no json here\n{\"no_schema\": 1}\n"))
        self.assertIsNone(ol.last_json_report(""))


class BuilderTests(unittest.TestCase):
    def test_apt_commands_are_non_interactive(self):
        commands = ol.apt_commands(["a", "b"])
        self.assertEqual(len(commands), 2)
        self.assertIn("update", commands[0])
        self.assertEqual(commands[1][-2:], ["a", "b"])
        for command in commands:
            self.assertEqual(command[:4], ["sudo", "-n", "-E", "apt-get"])
            self.assertIn("DPkg::Lock::Timeout=300", command)
        self.assertEqual(len(ol.apt_commands(["a"], update=False)), 1)
        env = ol.apt_env()
        self.assertEqual((env["DEBIAN_FRONTEND"], env["NEEDRESTART_MODE"]), ("noninteractive", "a"))
        for package in ("libwebkit2gtk-4.1-0", "libayatana-appindicator3-1", "xvfb", "xauth", "webkit2gtk-driver", "inotify-tools", "openssl"):
            self.assertIn(package, ol.RUNTIME_PACKAGES)
        self.assertIn("libwebkit2gtk-4.1-dev", ol.BUILD_PACKAGES)

    def test_hosts_lines_are_loopback_for_both_families_and_marked(self):
        lines = ol.hosts_lines(["github.com", "api.github.com"])
        self.assertEqual(len(lines), 4)
        self.assertIn("127.0.0.1 github.com " + ol.HOSTS_MARK, lines)
        self.assertIn("::1 api.github.com " + ol.HOSTS_MARK, lines)
        self.assertTrue(all(line.endswith(ol.HOSTS_MARK) for line in lines))
        self.assertTrue(ol.HOSTS_MARK.startswith(ol.BLACKHOLE_MARK), "one sed on the blackhole mark removes both kinds of lines at the end")
        self.assertNotEqual(ol.HOSTS_MARK, ol.BLACKHOLE_MARK, "removing the GitHub lines after a case must not remove the blackhole lines")

    def test_iptables_builders_and_parsers(self):
        ips = ["192.0.2.1", "192.0.2.2"]
        self.assertEqual(ol.iptables_block_commands(ips)[0], ["sudo", "-n", "iptables", "-I", "OUTPUT", "-d", "192.0.2.1", "-j", "REJECT"])
        self.assertEqual(ol.iptables_unblock_commands(ips)[1][4:], ["OUTPUT", "-d", "192.0.2.2", "-j", "REJECT"])
        listing = "-P OUTPUT ACCEPT\n-A OUTPUT -d 192.0.2.2/32 -j REJECT --reject-with icmp-port-unreachable\n-A OUTPUT -d 192.0.2.1/32 -j REJECT --reject-with icmp-port-unreachable\n"
        self.assertEqual(ol.parse_iptables_rules(listing, ips), {"192.0.2.1": True, "192.0.2.2": True})
        self.assertEqual(ol.parse_iptables_rules("-P OUTPUT ACCEPT\n-A OUTPUT -d 192.0.2.1/32 -j ACCEPT\n", ips), {"192.0.2.1": False, "192.0.2.2": False})
        self.assertEqual(ol.parse_iptables_rules("-A OUTPUT -d 192.0.2.11/32 -j REJECT\n", ["192.0.2.1"]), {"192.0.2.1": False}, "192.0.2.1 is not a prefix match of 192.0.2.11")
        counters = "Chain OUTPUT (policy ACCEPT 0 packets, 0 bytes)\n    pkts      bytes target     prot opt in     out     source               destination\n" \
                   "       3      180 REJECT     all  --  *      *       0.0.0.0/0            192.0.2.2            reject-with icmp-port-unreachable\n" \
                   "       0        0 REJECT     all  --  *      *       0.0.0.0/0            192.0.2.1            reject-with icmp-port-unreachable\n"
        self.assertEqual(ol.parse_iptables_counters(counters, ips), {"192.0.2.1": 0, "192.0.2.2": 3})

    def test_the_live_address_list_matches_the_repository_and_the_core_module(self):
        ips = ol.load_live_ips(_paths.ROOT)
        self.assertTrue(ips and all(ip.count(".") == 3 for ip in ips))
        import live_block
        self.assertEqual(ips, live_block.load_live_ips(_paths.ROOT), "both loaders read the same list")

    def test_the_live_address_list_is_cross_checked(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / ".github" / "workflows").mkdir(parents=True)
            (root / "tests" / "legacy-bridge").mkdir(parents=True)
            (root / ol.LIVE_WORKFLOW).write_text("env:\n  LIVE_NETWORK_IPS: 192.0.2.1 192.0.2.2\n")
            (root / ol.LIVE_ACCEPTANCE).write_text("live_ips=(192.0.2.1 192.0.2.2)\n")
            self.assertEqual(ol.load_live_ips(root), ["192.0.2.1", "192.0.2.2"])
            (root / ol.LIVE_ACCEPTANCE).write_text("live_ips=(192.0.2.1 192.0.2.3)\n")
            with self.assertRaises(ValueError):
                ol.load_live_ips(root)
            (root / ol.LIVE_ACCEPTANCE).write_text("live_ips=(192.0.2.1 192.0.2.1)\n")
            (root / ol.LIVE_WORKFLOW).write_text("env:\n  LIVE_NETWORK_IPS: 192.0.2.1 192.0.2.1\n")
            with self.assertRaises(ValueError):
                ol.load_live_ips(root)

    def test_the_sandbox_environment_confines_the_app_and_carries_no_secret(self):
        with mock.patch.dict(os.environ, {"GH_TOKEN": "secret", "GITHUB_TOKEN": "secret2", "ACTIONS_RUNTIME_TOKEN": "secret3", "HOME": "/home/runner"}):
            env = ol.sandbox_env(Path("/work/sandbox/clean"))
        self.assertEqual(env["HOME"], "/work/sandbox/clean/home")
        for key in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR", "TMPDIR"):
            self.assertTrue(env[key].startswith("/work/sandbox/clean/"), key)
        self.assertNotIn("secret", " ".join(env.values()))
        self.assertEqual(env["TAURI_WEBVIEW_AUTOMATION"], "true")
        self.assertEqual((env["SSL_CERT_FILE"], env["SSL_CERT_DIR"]), (ol.SYSTEM_BUNDLE, ol.SYSTEM_CERT_DIR))
        self.assertIn("tauri_plugin_updater=debug", env["RUST_LOG"])
        self.assertEqual(ol.sandbox_env(Path("/s"), {"X": "1"})["X"], "1")

    def test_case_tags_and_expected_state(self):
        self.assertEqual(ol.case_tag("released_app", "clean"), "clean")
        self.assertEqual(ol.case_tag("native_check", "cached-bait"), "native-cached-bait")
        prefixes = ol.expected_prefixes(Path("/s"))
        self.assertIn("/s/home/.config", prefixes)
        self.assertNotIn("/s/home", prefixes, "the whole home is not expected state")

    def test_the_updater_pubkey_is_the_one_in_the_shipped_config(self):
        import subprocess
        shown = subprocess.run(["git", "-C", str(_paths.ROOT), "show", "v0.7.11:desktop/src-tauri/tauri.conf.json"], capture_output=True, text=True)
        if shown.returncode != 0:
            self.skipTest("the v0.7.11 tag is not in this checkout")
        config = json.loads(shown.stdout)
        self.assertEqual(config["plugins"]["updater"]["pubkey"], ol.UPDATER_PUBKEY)
        self.assertEqual(config["plugins"]["updater"]["endpoints"], [ol.MANIFEST_URL])
        self.assertEqual(config["version"], ol.APP_VERSION)


class MocksForTheLab(unittest.TestCase):
    """The lab and the probe with every command replaced: evidence is always written and nothing real runs."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.evidence = Path(self.tmp.name) / "evidence"
        self.work = Path(self.tmp.name) / "work"
        patcher = mock.patch.dict(os.environ, {ol.GUARD_ENV: "1"})
        patcher.start()
        self.addCleanup(patcher.stop)
        self.commands = []

        def fake_run(ctx_self, argv, **kwargs):
            self.commands.append(list(argv))
            return ol.CmdResult(1, "mocked failure")

        for target in (mock.patch.object(ol.Ctx, "run", fake_run), mock.patch.object(ol.subprocess, "Popen", side_effect=OSError("no processes in tests")),
                       mock.patch.object(ol, "fetch_release_json", side_effect=OSError("offline"))):
            target.start()
            self.addCleanup(target.stop)

    def main(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = ol.main(list(argv) + ["--evidence", str(self.evidence), "--work", str(self.work)])
        return code, out.getvalue(), err.getvalue()

    def test_refuses_to_run_off_a_runner(self):
        with mock.patch.dict(os.environ, {ol.GUARD_ENV: "0", "GITHUB_ACTIONS": "false"}):
            code, _out, err = self.main("run")
        self.assertEqual(code, 2)
        self.assertIn("GITHUB_ACTIONS=true", err)
        self.assertFalse(self.evidence.exists())
        self.assertEqual(self.commands, [])

    def test_a_failing_lab_still_writes_a_result_and_exits_zero(self):
        code, out, _err = self.main("run")
        self.assertEqual(code, 0)
        result = json.loads((self.evidence / "result.json").read_text())
        self.assertEqual(result["schema"], ol.SCHEMA_OS_RESULT if hasattr(ol, "SCHEMA_OS_RESULT") else ol.SCHEMA_RESULT)
        self.assertEqual(result["verdict"], "UNPROVED")
        self.assertTrue(result["reasons"])
        self.assertIn("verdict: UNPROVED", out)
        self.assertTrue((self.evidence / "steps.log").is_file())
        self.assertTrue((self.evidence / "isolation.json").is_file())
        self.assertEqual(ol.overall_verdict(result)[0], "UNPROVED")
        for name in ol.TIERS:
            self.assertIn(result["tiers"][name]["result"], ("UNPROVED",))

    def test_a_native_only_run_with_no_build_is_unproved_not_a_crash(self):
        code, _out, _err = self.main("run", "--tier", "native_check")
        self.assertEqual(code, 0)
        result = json.loads((self.evidence / "result.json").read_text())
        self.assertEqual(result["verdict"], "UNPROVED")
        self.assertEqual(result["plugin"]["version"], "2.10.1", "the lockfile pin is recorded when the released binary was not examined")
        self.assertFalse(result["plugin"]["proven"])

    def test_unknown_case_names_are_a_usage_error(self):
        code, _out, err = self.main("run", "--cases", "clean,bogus")
        self.assertEqual(code, 2)
        self.assertIn("bogus", err)
        self.assertEqual(json.loads((self.evidence / "result.json").read_text())["verdict"], "UNPROVED")

    def test_the_probe_records_every_failed_step_and_never_raises(self):
        code, _out, _err = self.main("probe")
        self.assertEqual(code, 0)
        probe = json.loads((self.evidence / "probe.json").read_text())
        self.assertEqual(probe["schema"], ol.SCHEMA_PROBE)
        for name in ("apt_runtime", "release_metadata", "hosts_writable", "port_443", "ca", "live_block"):
            self.assertIn(name, probe["steps"], name)
        self.assertIn("finished", probe)
        release = probe["steps"]["release_metadata"]
        self.assertFalse(release["ok"], "ok means the LIVE release read worked; a fallback to config.json is reported but is not ok")
        self.assertEqual(release["detail"]["source"], "config.json (live API unavailable)")
        self.assertFalse(probe["steps"]["download_and_extract"]["ok"], "the (mocked) download failed")

    def test_no_command_in_a_failing_run_touches_the_network_or_the_machine_for_real(self):
        self.main("run")
        flat = [" ".join(command) for command in self.commands]
        self.assertTrue(any("apt-get" in line for line in flat), "the lab did try to install its dependencies (mocked)")
        # every one of those commands went through the mocked Ctx.run: if one had run for real this test would not be offline
        self.assertTrue(all(isinstance(command, list) for command in self.commands))


class FullLabWithMocksTests(unittest.TestCase):
    """run_lab end to end with both tiers 'working': the result is what the summary judge needs for PASS, and an insensitive recorder is not PASS."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.evidence = Path(self.tmp.name) / "evidence"
        self.work = Path(self.tmp.name) / "work"
        self.patches = []

    def start(self, target, **kwargs):
        patcher = target if hasattr(target, "start") else mock.patch.object(*target, **kwargs)
        patcher.start()
        self.addCleanup(patcher.stop)

    def run_lab(self, control_sensitive=True, tiers=("released_app", "native_check")):
        ctx = ol.Ctx(self.evidence, self.work)
        facts = {"asset": "ARC.Node_0.7.11_amd64.deb", "asset_sha256": DEB_SHA, "release_digest": "sha256:" + DEB_SHA, "digest_match": True, "extracted": True,
                 "binary": APP, "root": ROOT, "binary_sha256": "63" * 32, "missing_libraries": [], "reason": None}

        def released_case(ctx_, asset_facts, case, scenario, seed, control=False):
            analysis = ol.evaluate_case(good_data(scenario))
            name = "control-" + case if control else case
            if control:
                payload = [{"kind": "request", "host": "github.com", "path": "/FerrumVir/arc-chain/releases/download/v0.8.11/x.AppImage", "payload": True}] if control_sensitive else []
                (ctx_.evidence / ("requests-%s.jsonl" % name)).write_text("".join(json.dumps(row) + "\n" for row in bait_rows() + payload))
            return {"name": case, "tier": "released_app", "scenario": scenario, "trigger_outcome": good_data(scenario)["trigger"], "requests": analysis["requests"], "criteria": analysis["criteria"],
                    "evidence_files": ["requests-%s.jsonl" % name], "verdict": analysis["verdict"], "reasons": analysis["reasons"]}

        def native_case(ctx_, binary, case, scenario, control=False):
            record = released_case(ctx_, facts, case, scenario, None, control)
            record["tier"] = "native_check"
            name = "native-control-" + case if control else "native-" + case
            if control:
                payload = [{"kind": "request", "host": "github.com", "path": "/FerrumVir/arc-chain/releases/download/v0.8.11/x.app.tar.gz", "payload": True}] if control_sensitive else []
                (ctx_.evidence / ("requests-%s.jsonl" % name)).write_text("".join(json.dumps(row) + "\n" for row in bait_rows() + payload))
            record["evidence_files"] = ["requests-%s.jsonl" % name]
            return record

        native = Path(self.tmp.name) / "native-updater-check"
        native.write_bytes(b"not a real binary; only its existence matters to the mocked lab")

        class FakeBuild:
            def __init__(self, *args, **kwargs):
                self.binary = native

            def start(self):
                pass

            def wait(self, timeout):
                return True

            def status(self):
                return {"mode": "build", "state": "ok", "rc": 0, "seconds": 1.0, "error": None, "log_tail": ""}

        listing = "-A OUTPUT -d 192.0.2.1/32 -j REJECT\n"
        self.start((ol, "install_packages"), return_value={"installed": [], "failed": []})
        self.start((ol, "relax_userns_restriction"), return_value={})
        self.start((ol, "fetch_release_metadata"), return_value={"asset": ol.select_linux_asset(RELEASE), "source": "test", "notes": []})
        self.start((ol, "download_and_extract"), return_value=facts)
        self.start((ol, "gather_provenance"), return_value={"version": "2.10.1", "provenance": ["strings"], "proven": True})
        self.start((ol, "setup_ca"), side_effect=lambda ctx_: ctx_.state.update(ca_trusted=True) or {"ca_sha256": "ab" * 32, "trusted": True})
        self.start((ol, "apply_live_block"), side_effect=lambda ctx_, resources=None: ctx_.state.update(live_ips=["192.0.2.1"], live_block_applied=True) or {"ok": True, "ips": 1, "verify_rc": 0})
        self.start((ol, "https_selftest"), return_value={"ok": True})
        self.start((ol, "run_released_case"), side_effect=released_case)
        self.start((ol, "run_native_case"), side_effect=native_case)
        self.start((ol, "BackgroundBuild"), new=FakeBuild)
        self.start((ol, "run_replay_harness"), return_value={"ran": False})
        self.start((ol, "live_counters"), return_value={"live-ip-1": 0})
        self.start((ol, "cleanup"))
        self.start(mock.patch.object(ol.Ctx, "sudo", lambda self_, argv, **kw: ol.CmdResult(0, listing)))
        self.start((ol, "load_config"), return_value=None)
        ctx.state["hosts_mapped_lines"] = ol.hosts_lines(ol.MITM_HOSTS)
        ctx.state["hosts_mapped_names"] = list(ol.MITM_HOSTS)
        return ol.run_lab(ctx, list(tiers), list(ol.CASES), skip_native_build_wait=True)

    def judge(self, result):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "result.json"
            raw = json.dumps(result).encode()
            path.write_bytes(raw)
            return summary_module.evaluate_os_result(result, raw, path, Path(tmp))

    def test_both_tiers_working_is_pass_and_agrees_with_the_judge(self):
        result = self.run_lab()
        self.assertEqual(result["verdict"], "PASS", result["reasons"])
        self.assertEqual(self.judge(json.loads((self.evidence / "result.json").read_text()))["verdict"], "PASS")
        self.assertEqual(sorted(case["tier"] for case in result["cases"]), ["native_check", "native_check", "released_app", "released_app"])
        self.assertEqual(len(result["controls"]), 2)
        self.assertTrue(all(control["recorder_sensitive"] for control in result["controls"]))
        self.assertEqual((self.evidence / "manifest404-error.txt").read_text(), ol.RELEASE_NOT_FOUND + "\n")
        self.assertEqual(result["manifest404_error_text"], ol.RELEASE_NOT_FOUND)
        self.assertEqual(result["app"]["version_reported"], "0.7.11")
        self.assertTrue(result["isolation"]["live_block"])
        self.assertEqual(json.loads((self.evidence / "isolation.json").read_text())["ca_sha256"], "ab" * 32)

    def test_an_insensitive_recorder_is_never_pass(self):
        result = self.run_lab(control_sensitive=False)
        self.assertEqual(result["verdict"], "FAIL", "a control that did not see its own bundle request fails the OS, exactly like the summary judge")
        self.assertTrue(any("sensitivity" in str(result["tiers"][name]["reason"]) for name in ol.TIERS))
        self.assertTrue(all(control["verdict"] == "FAIL" for control in result["controls"]))
        self.assertEqual(self.judge(json.loads((self.evidence / "result.json").read_text()))["verdict"], "FAIL")

    def test_released_tier_alone(self):
        result = self.run_lab(tiers=("released_app",))
        self.assertEqual(result["verdict"], "PASS", result["reasons"])
        self.assertFalse(result["tiers"]["native_check"]["attempted"])

    def test_no_private_key_ever_lands_in_the_evidence(self):
        self.run_lab()
        import ca
        self.assertEqual(ca.scan_for_private_keys(self.evidence), [])
        names = {path.name for path in self.evidence.iterdir()}
        self.assertTrue({"result.json", "isolation.json", "steps.log", "manifest404-error.txt"} <= names, names)


if __name__ == "__main__":
    unittest.main()
