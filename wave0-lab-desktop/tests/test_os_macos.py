"""Offline tests of wave0-lab-desktop/os_macos.py (THROWAWAY LAB FILE).

Everything that would touch the machine (sudo, security, pfctl, hosts, hdiutil, osascript, the app) is replaced by a recorder
that only remembers the command lines; nothing here changes the system it runs on."""
from __future__ import annotations

import contextlib
import inspect
import json
import re
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import _paths  # noqa: F401
import os_macos as m

LIVE = ["149.28.32.76", "140.82.16.112", "136.244.109.1", "104.238.171.11", "202.182.107.41", "149.28.153.31"]
RSMS = ["104.21.58.14", "172.67.197.50"]  # what the real resolver answered for rsms.me on the CI runner (test data, never looked up here)

_NETWORK_GUARD = []


def _no_network(*args, **kwargs):
    raise AssertionError("a unit test tried to use the network (name resolution): patch socket.getaddrinfo or the method that calls it")


def setUpModule():
    guard = mock.patch("socket.getaddrinfo", side_effect=_no_network)
    guard.start()
    _NETWORK_GUARD.append(guard)


def tearDownModule():
    while _NETWORK_GUARD:
        _NETWORK_GUARD.pop().stop()
CONFIG = json.loads((_paths.LAB / "config.json").read_text(encoding="utf-8"))


def release_json():
    assets = []
    for key, item in CONFIG["app"]["assets"].items():
        assets.append({"name": item["name"], "size": item["size"], "digest": item["release_digest"], "browser_download_url": "https://github.com/FerrumVir/arc-chain/releases/download/v0.7.11/" + item["name"]})
    return {"tag_name": "v0.7.11", "assets": assets}


def req(role, path, host="github.com", status=200, payload=False, **extra):
    row = {"kind": "request", "role": role, "host": host, "path": path, "status": status, "payload": payload}
    row.update(extra)
    return row


MANIFEST_302 = req("manifest", m.MANIFEST_PATH, status=302)
MANIFEST_404 = req("manifest_redirect", "/FerrumVir/arc-chain/releases/download/v0.7.12/latest.json", status=404)
MANIFEST_200 = req("manifest", m.MANIFEST_PATH, status=200)
BUNDLE = req("payload", "/FerrumVir/arc-chain/releases/download/v0.8.11/ARC.Node_aarch64.app.tar.gz", status=404, payload=True)
CLEAN_FS = {"diff": {"added": [], "removed": [], "changed": []}, "poller_scans": 40, "poller_events": [], "expected_prefixes": [], "temp_roots": []}
CHECK_404 = {"reached": True, "outcome": "error", "error_kind": "ReleaseNotFound", "error": "Could not fetch a valid release JSON from the remote"}
CHECK_BAIT = {"reached": True, "outcome": "update_available", "update": {"version": "0.8.11", "download_url": "https://github.com/x"}, "download_attempted": False}


class FakeRecorder(m.Recorder):
    """Remembers argv, answers from a script, never runs anything."""

    def __init__(self, evidence, answers=None):
        super().__init__(Path(evidence), LIVE)
        self.calls = []
        self.answers = answers or []

    def run(self, argv, label="", timeout=120.0, input_text=None, env=None, cwd=None, sudo=False, quiet=False, max_lines=30):
        full = (["sudo", "-n"] if sudo else []) + [str(item) for item in argv]
        self.calls.append({"argv": full, "input": input_text, "label": label})
        for needle, answer in self.answers:
            if needle in " ".join(full):
                result = answer(full) if callable(answer) else answer
                return m.CmdResult(full, result[0], result[1], result[2] if len(result) > 2 else "", 0.0, timed_out=bool(len(result) > 3 and result[3]))
        return m.CmdResult(full, 0, "", "", 0.0)


class PureHelperTests(unittest.TestCase):
    def test_mask_hides_live_addresses_and_tokens(self):
        text = "connect 149.28.32.76:9090 and 140.82.16.112 with ghs_ABCDEFGHIJKLMNOPQRSTUV and secretvalue123"
        masked = m.mask_text(text, LIVE, ["secretvalue123"])
        self.assertNotIn("149.28.32.76", masked)
        self.assertNotIn("140.82.16.112", masked)
        self.assertNotIn("ghs_", masked)
        self.assertNotIn("secretvalue123", masked)
        self.assertIn("<live-ip-1>", masked)
        self.assertIn("<live-ip-2>", masked)
        self.assertEqual(m.mask_text(masked, LIVE), masked, "masking is idempotent")

    def test_live_addresses_come_from_the_repository_ci_and_agree_with_its_harness(self):
        found = m.load_live_ips(_paths.ROOT)
        self.assertEqual(len(found), 6)
        self.assertEqual(found, LIVE)

    def test_live_address_parsing_refuses_disagreement_duplicates_and_junk(self):
        workflow = "env:\n  LIVE_NETWORK_IPS: 1.2.3.4 5.6.7.8\n"
        harness = "live_ips=(1.2.3.4 5.6.7.8)\n"
        self.assertEqual(m.parse_live_ips(workflow, harness), ["1.2.3.4", "5.6.7.8"])
        with self.assertRaises(ValueError):
            m.parse_live_ips(workflow, "live_ips=(1.2.3.4 9.9.9.9)\n")
        with self.assertRaises(ValueError):
            m.parse_live_ips("  LIVE_NETWORK_IPS: 1.2.3.4 1.2.3.4\n", "live_ips=(1.2.3.4 1.2.3.4)\n")
        with self.assertRaises(ValueError):
            m.parse_live_ips("  LIVE_NETWORK_IPS: 1.2.3.999\n", "live_ips=(1.2.3.999)\n")
        with self.assertRaises(ValueError):
            m.parse_live_ips("nothing\n", harness)

    def test_pf_rules_block_every_live_address_and_nothing_else(self):
        conf = m.pf_conf_text(LIVE)
        self.assertEqual(conf.count("block drop out quick to "), 6)
        self.assertEqual(len(conf.splitlines()), 6)
        self.assertEqual(len(set(re.findall(r'label "(wave0-live-\d+)"', conf))), 6, "a distinct label per rule keeps pf's optimizer from merging them into a table")
        listing = "\n".join("block drop out quick inet from any to %s" % ip for ip in LIVE[:4]) + "\npass all flags S/SA\n"
        self.assertEqual(m.count_pf_block_rules(listing, LIVE), 4)
        self.assertEqual(m.count_pf_block_rules("", LIVE), 0)

    def test_hosts_block_roundtrip_leaves_the_original_untouched(self):
        original = "127.0.0.1 localhost\n255.255.255.255 broadcasthost\n::1 localhost\n"
        block = m.hosts_block(m.INTERCEPT_HOSTS)
        self.assertIn("127.0.0.1 github.com", block)
        self.assertIn("::1 api.github.com", block)
        self.assertIn("0.0.0.0 arc.ai", block)
        self.assertEqual(m.strip_hosts_block(original + block), original)
        self.assertEqual(m.strip_hosts_block(original), original)

    def test_asset_selection_per_architecture(self):
        arm = m.select_mac_assets(release_json(), "arm64")
        self.assertEqual((arm["arch"], arm["dmg"]["name"], arm["tar"]["name"]), ("aarch64", "ARC.Node_0.7.11_aarch64.dmg", "ARC.Node_aarch64.app.tar.gz"))
        intel = m.select_mac_assets(release_json(), "x86_64")
        self.assertEqual((intel["arch"], intel["dmg"]["name"], intel["tar"]["name"]), ("x64", "ARC.Node_0.7.11_x64.dmg", "ARC.Node_x64.app.tar.gz"))
        only_tar = {"assets": [a for a in release_json()["assets"] if a["name"] == "ARC.Node_aarch64.app.tar.gz"]}
        picked = m.select_mac_assets(only_tar, "arm64")
        self.assertIsNone(picked["dmg"])
        self.assertIsNotNone(picked["tar"])
        with self.assertRaises(m.AssetError):
            m.select_mac_assets({"assets": [{"name": "ARC.Node_0.7.11_amd64.deb"}]}, "arm64")
        windows = {"assets": [{"name": "ARC.Node_0.7.11_x64-setup.exe"}, {"name": "ARC.Node_0.7.11_x64_en-US.msi"}]}
        with self.assertRaises(m.AssetError):
            m.select_mac_assets(windows, "x86_64")

    def test_release_digest_hex(self):
        self.assertEqual(m.release_digest_hex("sha256:ABCdef"), "abcdef")
        self.assertEqual(m.release_digest_hex(None), "")

    def test_json_tail_survives_noise(self):
        self.assertEqual(m.parse_json_tail('warning: x\n{"a": 1}\n'), {"a": 1})
        self.assertIsNone(m.parse_json_tail("nothing here"))
        self.assertEqual(m.parse_json_tail('{"a": 1}\n{broken}\n'), {"a": 1})

    def test_ps_parsing_and_descendants(self):
        rows = m.parse_ps("  1     0 /sbin/launchd\n 50     1 /x/ARC Node.app/Contents/MacOS/arc-desktop\n 51    50 /usr/bin/xattr\n 52    51 /usr/bin/tar\n 99     1 /usr/sbin/cfprefsd\nnot a row\n")
        self.assertEqual(len(rows), 5)
        self.assertEqual(rows[1]["comm"], "/x/ARC Node.app/Contents/MacOS/arc-desktop")
        self.assertEqual(sorted(row["pid"] for row in m.descendants(rows, 50)), [51, 52])
        self.assertEqual(m.descendants(rows, 99), [])

    def test_artifact_names(self):
        for path in ("/x/ARC.Node_aarch64.app.tar.gz", "/x/a.dmg", "/x/a.sig", "/x/b.pkg", "/x/latest.json", "/x/arc.partial", "/x/a.download", "/x/ShipIt", "/x/Sparkle", "/x/data-update.bin", "/x/c.exe"):
            self.assertTrue(m.artifact_like(path), path)
        for path in ("/x/store.json", "/x/Cache.db", "/x/WebKit/ServiceWorkers", "/x/Local Storage/leveldb/000003.log", "/x/network.arc.desktop.plist"):
            self.assertFalse(m.artifact_like(path), path)

    def test_store_json_has_the_shape_store_rs_reads(self):
        store = m.store_json(Path("/sandbox/home"))
        self.assertEqual(sorted(store), ["config", "identity"])
        self.assertEqual(sorted(store["identity"]), ["address", "createdAt", "publicKey", "seedPhrase"])
        self.assertEqual(sorted(store["config"]), ["autoStart", "autoUpdate", "dataDir", "modelPath", "p2pPort", "role", "rpcPort"])
        self.assertIs(store["config"]["autoStart"], False, "no node and no ensure_binary download may start by itself")
        self.assertTrue(store["config"]["dataDir"].startswith("/sandbox/home"))
        json.dumps(store)


class OsascriptTests(unittest.TestCase):
    def test_error_classification(self):
        cases = [
            ("execution error: System Events got an error: osascript is not allowed assistive access. (-25211)", 1, "not_allowed_assistive", -25211),
            ("execution error: Not authorized to send Apple events to System Events. (-1743)", 1, "apple_events_not_authorized", -1743),
            ("execution error: System Events got an error: Can’t get window 1 of process \"x\". Invalid index. (-1719)", 1, "invalid_index", -1719),
            ("execution error: Application isn’t running. (-600)", 1, "application_not_running", -600),
            ("execution error: AppleEvent timed out. (-1712)", 1, "timeout", -1712),
            ("", 124, "timeout", None),
            ("weird failure", 1, "other", None),
            ("Safari, Finder, Dock", 0, "ok", None),
        ]
        for text, rc, kind, code in cases:
            with self.subTest(text=text):
                result = m.classify_osascript_error(text, rc)
                self.assertEqual(result["kind"], kind)
                self.assertEqual(result["code"], code)

    def test_scripts_name_what_they_do(self):
        self.assertIn("System Events", m.AS_PROCESS_NAMES)
        self.assertIn("New Finder Window", m.AS_FINDER_CLICK)
        self.assertIn("click menu item", m.AS_FINDER_CLICK)
        dump = m.jxa_ax_dump(4242)
        self.assertIn("4242", dump)
        self.assertIn("unixId", dump)
        click = m.jxa_click(4242, [r"^\s*Settings\s*$"])
        self.assertIn("4242", click)
        self.assertIn("AXButton", click)
        self.assertNotIn("AXStaticText", m.jxa_click(1, ["x"]), "the first pass clicks real buttons only")
        self.assertIn("AXStaticText", m.jxa_click(1, ["x"], roles=m.AX_ANY_ROLES))

    DUMP = {"pid": 7, "windows": 1, "error": None, "truncated": False, "nodes": [
        {"role": "AXWindow", "title": "ARC Node", "description": None, "name": "ARC Node", "value": None},
        {"role": "AXButton", "title": "Settings", "description": None, "name": "Settings", "value": None},
        {"role": "AXButton", "title": " Check for updates", "description": None, "name": None, "value": None},
        {"role": "AXStaticText", "title": None, "description": None, "name": None, "value": "You're running the latest version."},
    ]}

    def test_ax_find_and_summary(self):
        self.assertEqual(len(m.ax_find(self.DUMP["nodes"], m.SETTINGS_PATTERNS)), 1)
        summary = m.summarize_ax(self.DUMP)
        self.assertTrue(summary["settings_button"])
        self.assertTrue(summary["check_for_updates_button"])
        self.assertFalse(summary["install_button"], "the Install button is not rendered while no update is reported")
        self.assertEqual(summary["update_card_texts"], ["You're running the latest version."])
        with_install = json.loads(json.dumps(self.DUMP))
        with_install["nodes"].append({"role": "AXButton", "title": "Install v0.7.12 & relaunch", "description": None, "name": None, "value": None})
        self.assertTrue(m.summarize_ax(with_install)["install_button"])
        self.assertEqual(m.summarize_ax(None), {"available": False})
        self.assertFalse(m.summarize_ax({"error": "no process with that unix id", "nodes": []})["available"])

    @unittest.skipUnless(os.environ.get("WAVE0_TEST_NODE") == "1" and shutil.which("node"),
                         "opt-in (WAVE0_TEST_NODE=1 and node on PATH): the only test that starts a process, node on a temp .js file against a stub System Events tree")
    def test_generated_javascript_walks_and_clicks_a_stub_tree(self):
        harness = r"""
var clicks = [];
function el(role, title, kids, value) {
  return {role: function () { return role; }, subrole: function () { return null; }, title: function () { return title; },
    description: function () { return null; }, name: function () { return title; }, value: function () { return value === undefined ? null : value; },
    uiElements: function () { return kids || []; }, click: function () { clicks.push(title); }, actions: {byName: function () { return {perform: function () {}}; }}};
}
var settings = el('AXButton', 'Settings'), check = el('AXButton', ' Check for updates'), text = el('AXStaticText', null, [], 'You are up to date');
var win = el('AXWindow', 'ARC Node', [el('AXGroup', null, [settings, check, text])]);
var proc = {name: function () { return 'ARC Node'; }, windows: function () { return [win]; }};
function Application() { return {processes: {whose: function (q) { return function () { return q.unixId === 4242 ? [proc] : []; }; }}}; }
"""
        def run_js(script, tail):
            with tempfile.NamedTemporaryFile("w", suffix=".js", delete=False) as handle:
                handle.write(harness + script + "\n" + tail + "\n")
                path = handle.name
            done = subprocess.run(["node", path], capture_output=True, text=True, timeout=30)
            self.assertEqual(done.returncode, 0, done.stderr)
            return done.stdout.strip().splitlines()

        lines = run_js(m.jxa_ax_dump(4242), "console.log(run());")
        dump = json.loads(lines[0])
        self.assertIsNone(dump["error"])
        self.assertEqual(dump["windows"], 1)
        self.assertEqual(len(dump["nodes"]), 5)
        self.assertTrue(m.summarize_ax(dump)["settings_button"])
        self.assertEqual(json.loads(run_js(m.jxa_ax_dump(1), "console.log(run());")[0])["error"], "no process with that unix id")
        lines = run_js(m.jxa_click(4242, [r"^\s*Settings\s*$"]), "console.log(run()); console.log(JSON.stringify(clicks));")
        self.assertTrue(json.loads(lines[0])["clicked"])
        self.assertEqual(json.loads(lines[1]), ["Settings"])
        lines = run_js(m.jxa_click(4242, [r"Install\s+v?\d"]), "console.log(run()); console.log(JSON.stringify(clicks));")
        self.assertFalse(json.loads(lines[0])["clicked"])
        self.assertEqual(json.loads(lines[0])["error"], "no matching element")
        self.assertEqual(json.loads(lines[1]), [], "nothing is clicked when nothing matches")


class RequestAndPathTests(unittest.TestCase):
    def test_request_summary(self):
        rows = [MANIFEST_302, MANIFEST_404, BUNDLE, {"kind": "tls_failure", "sni": "api.github.com"}, {"kind": "tls_failure", "sni": "api.github.com"}, {"kind": "tls_failure", "sni": None}]
        summary = m.summarize_requests(rows)
        self.assertEqual(summary["total"], 3)
        self.assertEqual(summary["tls_failures"], [["(no SNI)", 1], ["api.github.com", 2]])
        self.assertEqual(len(summary["payload"]), 1)
        self.assertIsNone(m.summarize_requests(None)["total"])
        self.assertEqual(m.summarize_requests([])["total"], 0)

    def test_expected_prefixes_and_classification(self):
        prefixes = m.expected_prefixes("/Users/runner", "/tmp/sbx/home", "cached-bait")
        self.assertIn("/tmp/sbx/home", prefixes)
        self.assertIn("/Users/runner/Library/Application Support/network.arc.desktop", prefixes)
        self.assertNotIn("/Users/runner/Library/LaunchAgents", prefixes, "the whole LaunchAgents folder is never expected")
        self.assertIn("/Users/runner/Library/LaunchAgents/ARC Node.plist", prefixes, "the autostart plugin's own registration is app state")
        self.assertEqual(m.classify_written("/Users/runner/Library/LaunchAgents/evil-updater.plist", prefixes), "unexpected")
        self.assertEqual(m.classify_written("/tmp/sbx/home/Library/Application Support/network.arc.desktop/store.json", prefixes), "expected")
        self.assertEqual(m.classify_written("/Users/runner/Library/Caches/network.arc.desktop/WebKit/x", prefixes), "expected")
        self.assertEqual(m.classify_written("/Users/runner/Downloads/ARC.Node_aarch64.app.tar.gz", prefixes), "unexpected")
        self.assertEqual(m.classify_written("/Applications/ARC Node.app", prefixes), "unexpected")
        self.assertEqual(m.classify_written("/tmp/sbx/tmp/com.apple.WebKit.WebContent+x/y", prefixes, ["/tmp/sbx/tmp"]), "expected")
        self.assertEqual(m.classify_written("/tmp/sbx/tmp/mystery.bin", prefixes, ["/tmp/sbx/tmp"]), "unexpected")


class EvaluateCaseTests(unittest.TestCase):
    def native(self, rows, check=CHECK_404, scenario="latest-404", fs=CLEAN_FS, positive=None, procs=None):
        procs = procs if procs is not None else {"seen": [{"pid": 3, "ppid": 1, "comm": "/usr/libexec/xpcproxy"}], "app_pid": None, "bundle_path": ""}
        return m.evaluate_case("native_check", scenario, rows, fs, procs, None, check, positive)

    def test_native_404_case_passes_when_everything_is_recorded(self):
        result = self.native([MANIFEST_302, MANIFEST_404])
        self.assertEqual(result["verdict"], "PASS", result)
        self.assertEqual(result["criteria"], {key: True for key in m.CRITERIA})

    def test_bait_case_needs_the_positive_control(self):
        rows = [MANIFEST_200]
        without = self.native(rows, CHECK_BAIT, "bait-0.8.11")
        self.assertEqual(without["verdict"], "UNPROVED")
        self.assertTrue(any("positive control" in note for note in without["notes"]))
        failed_control = self.native(rows, CHECK_BAIT, "bait-0.8.11", positive={"observed": False})
        self.assertEqual(failed_control["verdict"], "UNPROVED")
        proven = self.native(rows, CHECK_BAIT, "bait-0.8.11", positive={"observed": True})
        self.assertEqual(proven["verdict"], "PASS", proven)

    def test_a_bundle_request_fails_the_case(self):
        result = self.native([MANIFEST_200, BUNDLE], CHECK_BAIT, "bait-0.8.11", positive={"observed": True})
        self.assertEqual(result["verdict"], "FAIL")
        self.assertIs(result["criteria"]["no_bundle_download"], False)
        by_extension = self.native([MANIFEST_200, req("other", "/x/y.sig", payload=False)], CHECK_BAIT, "bait-0.8.11", positive={"observed": True})
        self.assertIs(by_extension["criteria"]["no_bundle_download"], False, "the path pattern counts even if the server did not flag it")

    def test_any_other_latest_json_or_path_fails_only_manifest_url(self):
        for extra in (req("other_manifest", "/FerrumVir/arc-chain/releases/download/v0.8.11/latest.json"), req("api_latest", "/repos/FerrumVir/arc-chain/releases/latest", host="api.github.com"), req("other", "/somewhere")):
            with self.subTest(role=extra["role"]):
                result = self.native([MANIFEST_302, MANIFEST_404, extra])
                self.assertIs(result["criteria"]["only_manifest_url"], False)
                self.assertEqual(result["verdict"], "FAIL")

    def test_no_request_log_is_unproved_not_pass(self):
        result = self.native(None)
        self.assertIsNone(result["criteria"]["only_manifest_url"])
        self.assertIsNone(result["criteria"]["no_bundle_download"])
        self.assertEqual(result["verdict"], "UNPROVED")

    def test_an_empty_log_means_the_check_never_reached_the_endpoint(self):
        result = self.native([])
        self.assertIsNone(result["criteria"]["only_manifest_url"])
        self.assertEqual(result["verdict"], "UNPROVED")
        self.assertIs(result["criteria"]["no_bundle_download"], True, "an empty but present log does prove that no bundle was requested")

    def test_tls_failures_alone_do_not_count_as_manifest_evidence(self):
        result = self.native([{"kind": "tls_failure", "sni": "api.github.com"}])
        self.assertIsNone(result["criteria"]["only_manifest_url"])
        self.assertEqual(result["verdict"], "UNPROVED")

    def test_missing_or_incomplete_file_record_is_unproved(self):
        rows = [MANIFEST_302, MANIFEST_404]
        for fs in (None, {"diff": None, "poller_scans": 5}, {"diff": {"added": [], "removed": [], "changed": []}, "poller_scans": 0}):
            with self.subTest(fs=fs):
                result = self.native(rows, fs=fs)
                self.assertIsNone(result["criteria"]["no_new_files"])
                self.assertEqual(result["verdict"], "UNPROVED")

    def test_new_files_outside_the_expected_state_fail(self):
        fs = dict(CLEAN_FS, diff={"added": ["/Users/runner/Downloads/notes.txt"], "removed": [], "changed": []}, expected_prefixes=["/sandbox"])
        result = self.native([MANIFEST_302, MANIFEST_404], fs=fs)
        self.assertIs(result["criteria"]["no_new_files"], False)
        self.assertEqual(result["verdict"], "FAIL")
        poller_only = dict(CLEAN_FS, poller_events=[{"event": "added", "path": "/elsewhere/tmp.bin"}], expected_prefixes=["/sandbox"])
        self.assertIs(self.native([MANIFEST_302, MANIFEST_404], fs=poller_only)["criteria"]["no_new_files"], False, "a file that came and went is caught by the poller")

    def test_update_artifact_in_expected_state_still_fails_install_and_files(self):
        fs = dict(CLEAN_FS, diff={"added": ["/sandbox/home/Library/Caches/network.arc.desktop/update-0.8.11.tar.gz"], "removed": [], "changed": []}, expected_prefixes=["/sandbox"])
        result = self.native([MANIFEST_302, MANIFEST_404], fs=fs)
        self.assertIs(result["criteria"]["no_install"], False)
        self.assertIs(result["criteria"]["no_new_files"], False)
        self.assertEqual(result["verdict"], "FAIL")

    def test_native_check_unexpected_outcome_is_unproved(self):
        wrong = dict(CHECK_404, outcome="update_available", error_kind=None)
        result = self.native([MANIFEST_302, MANIFEST_404], wrong)
        self.assertEqual(result["verdict"], "UNPROVED")
        tls = dict(CHECK_404, error_kind="Reqwest")
        self.assertEqual(self.native([MANIFEST_302, MANIFEST_404], tls)["verdict"], "UNPROVED")

    def test_an_app_process_during_a_native_check_fails_the_launch_criterion(self):
        procs = {"seen": [{"pid": 9, "ppid": 1, "comm": "/Applications/ARC Node.app/Contents/MacOS/arc-desktop"}], "app_pid": None, "bundle_path": ""}
        result = self.native([MANIFEST_302, MANIFEST_404], procs=procs)
        self.assertIs(result["criteria"]["no_new_app_launch"], False)

    def test_missing_process_record_is_unproved(self):
        result = m.evaluate_case("native_check", "latest-404", [MANIFEST_302, MANIFEST_404], CLEAN_FS, None, None, CHECK_404)
        self.assertIsNone(result["criteria"]["no_install"])
        self.assertIsNone(result["criteria"]["no_new_app_launch"])
        self.assertEqual(result["verdict"], "UNPROVED")

    BUNDLE_PATH = "/sbx/apps/ARC Node.app"
    MAIN = "/sbx/apps/ARC Node.app/Contents/MacOS/arc-desktop"

    def app(self, seen, bundle_diff=None, check=None, app_pid=50, rows=None):
        check = check if check is not None else {"reached": False, "reason": "the Install button was not rendered"}
        procs = {"seen": seen, "app_pid": app_pid, "bundle_path": self.BUNDLE_PATH}
        diff = {"added": [], "removed": [], "changed": []} if bundle_diff is None else bundle_diff
        return m.evaluate_case("released_app", "latest-404", rows if rows is not None else [{"kind": "tls_failure", "sni": "api.github.com"}], CLEAN_FS, procs, diff, check)

    def test_released_app_negative_control_is_never_a_pass_for_the_plugin_path(self):
        seen = [{"pid": 50, "ppid": 1, "comm": self.MAIN}, {"pid": 60, "ppid": 1, "comm": "/System/Library/com.apple.WebKit.WebContent"}]
        result = self.app(seen)
        self.assertEqual(result["verdict"], "UNPROVED")
        self.assertIsNone(result["criteria"]["only_manifest_url"])
        self.assertIs(result["criteria"]["no_bundle_download"], True)
        self.assertIs(result["criteria"]["no_install"], True)
        self.assertIs(result["criteria"]["no_new_app_launch"], True)
        self.assertIn("not reached", result["reasons"]["only_manifest_url"][0])

    def test_released_app_second_launch_or_foreign_bundle_fails(self):
        self.assertIs(self.app([{"pid": 50, "ppid": 1, "comm": self.MAIN}, {"pid": 77, "ppid": 1, "comm": self.MAIN}])["criteria"]["no_new_app_launch"], False)
        foreign = [{"pid": 50, "ppid": 1, "comm": self.MAIN}, {"pid": 78, "ppid": 1, "comm": "/Applications/ARC Node.app/Contents/MacOS/arc-desktop"}]
        self.assertIs(self.app(foreign)["criteria"]["no_new_app_launch"], False)
        self.assertIsNone(self.app([{"pid": 50, "ppid": 1, "comm": self.MAIN}], app_pid=None)["criteria"]["no_new_app_launch"])

    def test_released_app_installer_child_or_changed_bundle_fails_install(self):
        seen = [{"pid": 50, "ppid": 1, "comm": self.MAIN}, {"pid": 51, "ppid": 50, "comm": "/usr/bin/hdiutil"}]
        self.assertIs(self.app(seen)["criteria"]["no_install"], False)
        quiet = [{"pid": 50, "ppid": 1, "comm": self.MAIN}, {"pid": 99, "ppid": 1, "comm": "/usr/libexec/installd"}]
        self.assertIs(self.app(quiet)["criteria"]["no_install"], True, "an unrelated system installer daemon is not a child of the app")
        changed = self.app([{"pid": 50, "ppid": 1, "comm": self.MAIN}], bundle_diff={"added": [], "removed": [], "changed": [self.MAIN]})
        self.assertIs(changed["criteria"]["no_install"], False)
        no_record = m.evaluate_case("released_app", "latest-404", [], CLEAN_FS, {"seen": [], "app_pid": 50, "bundle_path": self.BUNDLE_PATH}, None, {"reached": False})
        self.assertIsNone(no_record["criteria"]["no_install"])

    def test_verdict_priority_fail_over_unproved(self):
        result = self.native([MANIFEST_302, MANIFEST_404, BUNDLE], fs=None)
        self.assertEqual(result["verdict"], "FAIL")


class OverallVerdictTests(unittest.TestCase):
    def case(self, tier, verdict):
        return {"tier": tier, "verdict": verdict}

    def test_pass_needs_the_released_app_path_to_have_run(self):
        tiers = {"released_app": {"result": "infeasible"}, "native_check": {"result": "ran"}}
        verdict, per_tier = m.overall_verdict(tiers, [self.case("native_check", "PASS"), self.case("native_check", "PASS"), self.case("released_app", "UNPROVED")])
        self.assertEqual(verdict, "UNPROVED")
        self.assertEqual(per_tier["native_check"], "PASS")
        self.assertEqual(per_tier["released_app"], "INFEASIBLE")

    def test_a_failure_anywhere_is_a_failure(self):
        verdict, per_tier = m.overall_verdict({"released_app": {"result": "ran"}, "native_check": {"result": "ran"}}, [self.case("native_check", "FAIL"), self.case("released_app", "PASS")])
        self.assertEqual((verdict, per_tier["native_check"]), ("FAIL", "FAIL"))

    def test_both_tiers_passing_is_a_pass(self):
        verdict, _ = m.overall_verdict({"released_app": {"result": "ran"}, "native_check": {"result": "ran"}}, [self.case("native_check", "PASS"), self.case("released_app", "PASS")])
        self.assertEqual(verdict, "PASS")

    def test_nothing_run_is_unproved(self):
        verdict, per_tier = m.overall_verdict({"released_app": m.not_run_tier("x"), "native_check": m.not_run_tier("y")}, [])
        self.assertEqual(verdict, "UNPROVED")
        self.assertEqual(set(per_tier.values()), {"UNPROVED"})


class FeasibilityTests(unittest.TestCase):
    def probe(self, **overrides):
        base = {
            "system": {"image": "macos-15 20261001.1"},
            "accessibility": {"summary": {"system_events_reachable": True, "click_works": True, "click_evidence": "Finder windows 0 -> 1"}},
            "released_app_ui": {"attempted": True, "window_appeared": True, "nodes": 120, "settings_button": True, "check_button": True, "install_button": False},
            "native_check": {"status": "cargo build succeeded (rc 0)"},
        }
        base.update(overrides)
        return base

    def test_yes_answer_names_its_evidence(self):
        text = m.build_feasibility(self.probe())
        first, second = text.strip().splitlines()
        self.assertTrue(first.startswith("YES: UI scripting works on macos-15 20261001.1"))
        self.assertIn("Finder windows 0 -> 1", first)
        self.assertIn("120 elements", first)
        self.assertIn("Install button present after Check for updates = NO", second)
        self.assertIn("Native-check tier", second)

    def test_the_real_banner_switch_in_the_text_is_the_probes_own_option_even_when_the_app_part_did_not_run(self):
        off = self.probe(real_banner_api=False, released_app_ui={"attempted": False, "problem": "x"})
        on = self.probe(real_banner_api=True, released_app_ui={"attempted": False, "problem": "x"})
        self.assertIn("was OFF", m.build_feasibility(off))
        self.assertIn("was ON", m.build_feasibility(on), "ON in the probe's options means ON, whatever the UI dict says")
        self.assertIn("was ON", m.build_feasibility(self.probe(released_app_ui={"attempted": True, "real_banner_api": True})))

    def test_the_text_quotes_what_the_updates_card_said_when_no_install_button_appeared(self):
        ui = {"attempted": True, "window_appeared": True, "nodes": 133, "settings_button": True, "check_button": True, "install_button": False, "real_banner_api": True,
              "banner_attempts": [{"attempt": n, "install_button": False, "card_texts": ["Updates", "vUNKNOWN"]} for n in range(1, 5)], "banner_ui_text": "Updates vUNKNOWN You're running the latest version."}
        text = m.build_feasibility(self.probe(released_app_ui=ui, real_banner_api=True))
        self.assertIn("UI after 4 Check for updates click(s): Updates vUNKNOWN You're running the latest version.", text)

    def test_denied_answer_quotes_the_error(self):
        acc = {"summary": {"system_events_reachable": False, "click_works": False, "first_error": "osascript is not allowed assistive access. (-25211)"}}
        text = m.build_feasibility(self.probe(accessibility=acc, released_app_ui={"attempted": False, "problem": "isolation not ready"}))
        self.assertTrue(text.startswith("NO: osascript could not drive System Events"))
        self.assertIn("-25211", text)
        self.assertIn("isolation not ready", text)

    def test_partial_answer_when_events_answer_but_the_click_fails(self):
        acc = {"summary": {"system_events_reachable": True, "click_works": False, "click_error_kind": "not_allowed_assistive", "click_error": "not allowed"}}
        self.assertTrue(m.build_feasibility(self.probe(accessibility=acc)).startswith("PARTLY"))


class ResultAssemblyTests(unittest.TestCase):
    def test_result_has_the_documented_skeleton(self):
        system = {"machine": "arm64", "mac_ver": "15.0", "image": "macos-15 1"}
        cases = [{"name": "clean", "tier": "native_check", "verdict": "PASS"}, {"name": "cached-bait", "tier": "native_check", "verdict": "PASS"}]
        tiers = {"released_app": m.not_run_tier("unreachable", "infeasible", "ui"), "native_check": m.not_run_tier("ran", "ran", "native")}
        result = m.assemble_result(system, {"name": "a.dmg", "sha256": "1" * 64, "release_digest": "sha256:" + "1" * 64, "digest_match": True, "source": "dmg"},
                                   {"version": "0.7.11", "identifier": "network.arc.desktop"}, {"crates_in_binary": ["tauri-plugin-updater-2.10.1"], "plugin_pinned": True},
                                   tiers, cases, "Could not fetch a valid release JSON from the remote", "native_check tier", {"hosts_mapped": ["github.com"], "ca_sha256": "ab" * 32, "live_block": {"verified": True}}, [])
        for key in ("schema", "os", "runner", "app", "plugin", "tiers", "cases", "manifest404_error_text", "isolation", "verdict"):
            self.assertIn(key, result)
        self.assertEqual(result["schema"], "arc.legacy-bridge.wave0-lab.desktop-os-result.v1")
        self.assertEqual(result["os"], "macos-arm64")
        self.assertEqual(result["app"]["version_reported"], "0.7.11")
        self.assertTrue(result["app"]["digest_match"])
        self.assertEqual(result["verdict"], "UNPROVED")
        self.assertEqual(result["tier_verdicts"], {"released_app": "INFEASIBLE", "native_check": "PASS"})
        self.assertIn("NOT interceptable", result["isolation"]["interception_scope"])
        json.dumps(result)
        self.assertEqual(m.assemble_result({"machine": "x86_64"}, None, None, None, tiers, [], None, None, {}, ["x"])["os"], "macos-intel")


class RecorderTests(unittest.TestCase):
    """The recorder's own behavior, with subprocess.run replaced: no process is started."""

    def test_commands_are_logged_masked_and_timed_out(self):
        calls = []

        def fake_run(argv, **kwargs):
            calls.append((list(argv), kwargs))
            if argv[0] == "echo":
                return subprocess.CompletedProcess(argv, 0, b"reach 149.28.32.76 now\n", b"")
            if argv[0] == "fail":
                return subprocess.CompletedProcess(argv, 3, b"", b"it broke\n")
            if argv[0] == "slow":
                raise subprocess.TimeoutExpired(argv, kwargs["timeout"], output=b"partial output", stderr=b"")
            raise FileNotFoundError(argv[0])

        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(m.subprocess, "run", side_effect=fake_run):
            rec = m.Recorder(Path(tmp), LIVE)
            done = rec.run(["echo", "reach 149.28.32.76 now"], label="mask test")
            self.assertEqual(done.rc, 0)
            self.assertIn("149.28.32.76", done.out, "the caller still sees the real output")
            failed = rec.run(["fail"])
            self.assertEqual(failed.rc, 3)
            slow = rec.run(["slow"], timeout=1)
            self.assertEqual(slow.rc, 124)
            self.assertTrue(slow.timed_out)
            self.assertIn("partial output", slow.out)
            self.assertIn("timed out after 1", slow.err)
            missing = rec.run(["/no/such/binary"])
            self.assertEqual(missing.rc, 127)
            log = (Path(tmp) / "steps.log").read_text()
            self.assertNotIn("149.28.32.76", log)
            self.assertIn("<live-ip-1>", log)
            self.assertIn("rc=3", log)
            self.assertIn("rc=124", log)
            self.assertIn("rc=127", log)
            self.assertIn("# mask test", log)
            self.assertIn("it broke", log)
        self.assertEqual(calls[2][1]["timeout"], 1)

    def test_sudo_prefix_is_non_interactive(self):
        with tempfile.TemporaryDirectory() as tmp:
            rec = FakeRecorder(tmp)
            rec.run(["pfctl", "-s", "info"], sudo=True)
            self.assertEqual(rec.calls[0]["argv"][:3], ["sudo", "-n", "pfctl"])

    def test_the_real_recorder_prefixes_sudo_with_dash_n_and_passes_input(self):
        seen = {}

        def fake_run(argv, **kwargs):
            seen["argv"], seen["input"] = list(argv), kwargs.get("input")
            return subprocess.CompletedProcess(argv, 0, b"", b"")

        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(m.subprocess, "run", side_effect=fake_run):
            m.Recorder(Path(tmp)).run(["tee", "-a", "/etc/hosts"], sudo=True, input_text="127.0.0.1 x\n", quiet=True)
        self.assertEqual(seen["argv"], ["sudo", "-n", "tee", "-a", "/etc/hosts"])
        self.assertEqual(seen["input"], b"127.0.0.1 x\n")


class InterceptionTests(unittest.TestCase):
    def make(self, tmp, answers=None):
        rec = FakeRecorder(Path(tmp) / "evidence", answers)
        env = m.Interception(rec, Path(tmp) / "evidence", Path(tmp) / "work", LIVE, shared_dir=Path(tmp) / "shared")
        (Path(tmp) / "work").mkdir(parents=True, exist_ok=True)
        return rec, env

    def joined(self, rec):
        return [" ".join(call["argv"]) for call in rec.calls]

    @staticmethod
    def rsms_resolves(addresses=RSMS):
        """The real resolver's answers for rsms.me, scripted: getaddrinfo (before the mapping) and dig (any time)."""
        infos = [(2, 1, 6, "", (a, 443)) for a in addresses]
        return mock.patch("socket.getaddrinfo", return_value=infos)

    def test_live_block_writes_only_block_rules_and_verifies_them(self):
        with tempfile.TemporaryDirectory() as tmp, self.rsms_resolves():
            listing = "\n".join("block drop out quick inet from any to %s" % ip for ip in LIVE + RSMS)
            rec, env = self.make(tmp, [("pfctl -sr", (0, listing)), ("pfctl -s info", (0, "Status: Enabled for 0 days"))])
            info = env.block_live_network()
            conf = (Path(tmp) / "work" / "pf-live-block.conf").read_text()
            self.assertEqual(conf, m.pf_conf_text(LIVE, RSMS))
            self.assertEqual(conf.count("wave0-live-"), 6)
            self.assertEqual(conf.count("wave0-extra-"), 2)
            self.assertTrue(info["verified"])
            self.assertEqual(info["block_rules_listed"], 8)
            self.assertEqual(info["extra_addresses"], 2)
            self.assertEqual(env.extra_block_ips, sorted(RSMS))
            commands = self.joined(rec)
            self.assertTrue(any(c.startswith("sudo -n pfctl -f ") for c in commands))
            self.assertTrue(any(c.startswith("dig +short") and "rsms.me" in c for c in commands), "the real resolver is asked directly, /etc/hosts cannot answer dig")
            self.assertNotIn("149.28.32.76", json.dumps(info), "the masked listing never carries a live address")

    def test_one_missing_extra_rule_is_enough_for_the_block_to_be_unverified(self):
        with tempfile.TemporaryDirectory() as tmp, self.rsms_resolves():
            listing = "\n".join("block drop out quick inet from any to %s" % ip for ip in LIVE + RSMS[:1])
            rec, env = self.make(tmp, [("pfctl -sr", (0, listing)), ("pfctl -s info", (0, "Status: Enabled"))])
            self.assertFalse(env.block_live_network()["verified"])

    def test_the_extra_addresses_include_ipv6_in_the_form_pf_prints(self):
        with tempfile.TemporaryDirectory() as tmp, self.rsms_resolves(["2606:4700:3037:0:0:0:6815:3A0E", "104.21.58.14"]):
            listing = "\n".join("block drop out quick inet from any to %s" % ip for ip in LIVE) + "\nblock drop out quick inet6 from any to 2606:4700:3037::6815:3a0e\nblock drop out quick inet from any to 104.21.58.14\n"
            rec, env = self.make(tmp, [("pfctl -sr", (0, listing)), ("pfctl -s info", (0, "Status: Enabled"))])
            info = env.block_live_network()
            self.assertTrue(info["verified"], info)
            self.assertIn("2606:4700:3037::6815:3a0e", env.extra_block_ips, "compressed lower case, exactly as pf lists it")

    def test_addresses_that_appear_later_are_blocked_before_the_next_case(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.rsms_resolves():
                listing = "\n".join("block drop out quick inet from any to %s" % ip for ip in LIVE + RSMS + ["104.21.99.9"])
                dig_calls = {"n": 0}

                def dig(argv):  # the first resolution sees the usual pair, a later one a third address
                    dig_calls["n"] += 1
                    return (0, "104.21.58.14\n172.67.197.50\n") if dig_calls["n"] == 1 else (0, "104.21.58.14\n172.67.197.50\n104.21.99.9\n")

                rec, env = self.make(tmp, [("pfctl -sr", (0, listing)), ("pfctl -s info", (0, "Status: Enabled")), ("dig +short +time=3 +tries=2 A rsms.me", dig)])
                env.block_live_network()
                loads = len([c for c in self.joined(rec) if c.startswith("sudo -n pfctl -f")])
                refreshed = env.refresh_extra_blocks()
            self.assertEqual(refreshed["added"], ["104.21.99.9"])
            self.assertTrue(refreshed["verified"])
            self.assertEqual(len([c for c in self.joined(rec) if c.startswith("sudo -n pfctl -f")]), loads + 1, "the rules are reloaded once, with the new address")
            self.assertIn("104.21.99.9", (Path(tmp) / "work" / "pf-live-block.conf").read_text())
            again = env.refresh_extra_blocks()
            self.assertEqual(again["added"], [])
            self.assertEqual(len([c for c in self.joined(rec) if c.startswith("sudo -n pfctl -f")]), loads + 1, "nothing new, no reload")

    def test_dig_output_parsing_skips_cnames_loopback_and_private_addresses(self):
        text = "alias.example.\n104.21.58.14\n127.0.0.1\n10.0.0.1\n2606:4700:3037::6815:3A0E\n::1\n104.21.58.14\nnot an address\n"
        self.assertEqual(m.parse_dig_addresses(text), ["104.21.58.14", "2606:4700:3037::6815:3a0e"])
        self.assertEqual(m.parse_dig_addresses(""), [])

    def test_a_merged_anonymous_table_is_read_back_and_verifies_the_block(self):
        with tempfile.TemporaryDirectory() as tmp, self.rsms_resolves([]):
            listing = "block drop out quick inet from any to <__automatic_227272d_0>\nNo ALTQ support in kernel\n"
            table = "\n".join("   %s" % ip for ip in LIVE) + "\n"
            rec, env = self.make(tmp, [("pfctl -t __automatic_227272d_0 -T show", (0, table)), ("pfctl -sr", (0, listing)), ("pfctl -s info", (0, "Status: Enabled"))])
            info = env.block_live_network()
            self.assertTrue(info["verified"])
            self.assertEqual(info["table_addresses_listed"], 6)
            self.assertNotIn("149.28.32.76", json.dumps(info))
            partial = "\n".join("   %s" % ip for ip in LIVE[:5]) + "\n"
            rec2, env2 = self.make(tmp, [("pfctl -t __automatic_227272d_0 -T show", (0, partial)), ("pfctl -sr", (0, listing)), ("pfctl -s info", (0, "Status: Enabled"))])
            self.assertFalse(env2.block_live_network()["verified"], "five of six addresses in the table is not a verified block")
            rec3, env3 = self.make(tmp, [("pfctl -t __automatic_227272d_0 -T show", (1, "", "pfctl: Table does not exist")), ("pfctl -sr", (0, listing)), ("pfctl -s info", (0, "Status: Enabled"))])
            self.assertFalse(env3.block_live_network()["verified"], "an unreadable table never verifies")

    def test_live_block_is_not_verified_when_a_rule_is_missing(self):
        with tempfile.TemporaryDirectory() as tmp, self.rsms_resolves([]):
            listing = "\n".join("block drop out quick inet from any to %s" % ip for ip in LIVE[:5])
            rec, env = self.make(tmp, [("pfctl -sr", (0, listing)), ("pfctl -s info", (0, "Status: Enabled"))])
            self.assertFalse(env.block_live_network()["verified"])
            rec2, env2 = self.make(tmp, [("pfctl -sr", (0, "")), ("pfctl -f", (1, "", "pfctl: syntax error"))])
            self.assertFalse(env2.block_live_network()["verified"])

    def test_trust_grants_the_right_first_then_adds_non_interactively_and_verifies_with_security_framework(self):
        with tempfile.TemporaryDirectory() as tmp:
            rec, env = self.make(tmp)
            env.ca = {"ca_cert": "/w/ca/ca.crt", "server_cert": "/w/ca/server.crt", "ca_sha256": "ab" * 32}
            info = env.trust_ca()
            commands = self.joined(rec)
            self.assertEqual(commands[0], "sudo -n security authorizationdb write com.apple.trust-settings.admin allow", "the right is granted BEFORE the first add")
            self.assertEqual(commands[1], "sudo -n security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain /w/ca/ca.crt")
            self.assertIn("security verify-cert -c /w/ca/server.crt -p ssl -s github.com", commands[2])
            self.assertTrue(info["trusted"])
            self.assertEqual(len(info["attempts"]), 1)
            env.ca_sha1 = "AA" * 20
            env.untrust_ca()
            commands = self.joined(rec)
            self.assertTrue(any("security remove-trusted-cert -d /w/ca/ca.crt" in c for c in commands))
            self.assertTrue(any("security delete-certificate -Z %s /Library/Keychains/System.keychain" % ("AA" * 20) in c for c in commands))

    def test_a_hung_trust_call_is_killed_and_retried_with_the_right_granted_again(self):
        calls = {"add": 0}

        def add(argv):
            calls["add"] += 1
            return (124, "", "[timed out after 60s]", True) if calls["add"] == 1 else (0, "")

        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(m.time, "sleep"):
            rec, env = self.make(tmp, [("add-trusted-cert", add)])
            env.ca = {"ca_cert": "/w/ca.crt", "server_cert": "/w/s.crt"}
            info = env.trust_ca()
            commands = self.joined(rec)
            self.assertTrue(info["trusted"])
            self.assertEqual([a["timed_out"] for a in info["attempts"]], [True, False])
            for victim in ("security", "SecurityAgent", "authorizationhost"):
                self.assertIn("sudo -n pkill -KILL -x %s" % victim, commands, "the hung process (root's child of the killed sudo) and the dialog are killed")
            self.assertEqual(len([c for c in commands if "authorizationdb write" in c]), 2, "the right is granted again before the retry")
            first_kill = commands.index("sudo -n pkill -KILL -x security")
            second_add = [i for i, c in enumerate(commands) if "add-trusted-cert" in c][1]
            self.assertLess(first_kill, second_add, "the kill comes before the next attempt")

    def test_the_trust_attempts_use_bounded_timeouts_and_give_up_after_three(self):
        timeouts = []

        class Rec(FakeRecorder):
            def run(self, argv, label="", timeout=120.0, **kw):
                if "add-trusted-cert" in argv:
                    timeouts.append(timeout)
                return super().run(argv, label=label, timeout=timeout, **kw)

        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(m.time, "sleep"):
            rec = Rec(Path(tmp) / "evidence", [("add-trusted-cert", (124, "", "[timed out]", True))])
            env = m.Interception(rec, Path(tmp) / "evidence", Path(tmp) / "work", LIVE, shared_dir=Path(tmp) / "shared")
            env.ca = {"ca_cert": "/w/ca.crt", "server_cert": "/w/s.crt"}
            info = env.trust_ca()
        self.assertEqual(timeouts, [60.0, 45.0, 45.0])
        self.assertTrue(all(t <= 60 for t in timeouts))
        self.assertFalse(info["trusted"], "fail closed")
        self.assertEqual(len(info["attempts"]), 3)

    def test_failed_trust_is_reported_not_trusted(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(m.time, "sleep"):
            rec, env = self.make(tmp, [("add-trusted-cert", (1, "", "SecTrustSettingsSetTrustSettings: authorization denied"))])
            env.ca = {"ca_cert": "/w/ca.crt", "server_cert": "/w/s.crt"}
            self.assertFalse(env.trust_ca()["trusted"])
            env.untrust_ca()
            self.assertFalse(any("remove-trusted-cert" in c for c in self.joined(rec)), "nothing was trusted, so nothing is removed")

    def fake_ca(self, tmp, hostnames=None):
        """What lib/ca.make_ca leaves on disk, without running openssl."""
        def make(outdir, hosts):
            out = Path(outdir)
            (out / "private").mkdir(parents=True, exist_ok=True)
            for name in ("ca.crt", "server.crt", "private/ca.key", "private/server.key"):
                (out / name).write_text("x", encoding="ascii")
            (out / "ca.sha256").write_text("cd" * 32 + "\n", encoding="ascii")
            return {"ca_cert": str(out / "ca.crt"), "ca_key": str(out / "private" / "ca.key"), "server_cert": str(out / "server.crt"), "server_key": str(out / "private" / "server.key"),
                    "ca_sha256": "cd" * 32, "hostnames": list(hosts)}
        return mock.patch.object(m.ca_lib, "make_ca", side_effect=make)

    def test_one_ca_per_job_generated_once_trusted_once_and_kept_for_the_next_phase(self):
        with tempfile.TemporaryDirectory() as tmp:
            fingerprint = "SHA1 Fingerprint=" + ":".join(["AB"] * 20)
            with self.fake_ca(tmp) as make:
                rec1, env1 = self.make(tmp, [("openssl x509", (0, fingerprint))])
                env1.keep_ca = True
                env1.make_ca()
                env1.trust_ca()
                state = json.loads((Path(tmp) / "shared" / "ca-state.json").read_text())
                self.assertTrue(state["trusted"])
                self.assertEqual(state["ca_sha1"], "AB" * 20)
                self.assertEqual(state["hostnames"], sorted(env1.hosts))
                env1.teardown()
                self.assertFalse(any("remove-trusted-cert" in c for c in self.joined(rec1)), "the probe phase leaves the CA trusted for the run phase")
                self.assertTrue(env1.facts["ca_kept_trusted_for_the_next_phase"])
                # the run phase of the same job
                rec2, env2 = self.make(tmp)
                env2.make_ca()
                self.assertTrue(env2.ca_reused)
                self.assertTrue(env2.trusted)
                self.assertEqual(make.call_count, 1, "the CA is generated once")
                info = env2.trust_ca()
                commands = self.joined(rec2)
                self.assertTrue(info["trusted"])
                self.assertTrue(info["trusted_by_an_earlier_phase"])
                self.assertFalse(any("add-trusted-cert" in c or "authorizationdb" in c for c in commands), "trusted once: no second trust call, no second prompt")
                env2.keep_ca = False
                env2.teardown()
                self.assertTrue(any("remove-trusted-cert" in c for c in self.joined(rec2)), "removed once, at the end of the job")
                self.assertFalse(json.loads((Path(tmp) / "shared" / "ca-state.json").read_text())["trusted"])

    def test_a_ca_for_other_host_names_is_replaced_and_the_old_trust_removed(self):
        with tempfile.TemporaryDirectory() as tmp, self.fake_ca(tmp) as make:
            rec1, env1 = self.make(tmp)
            env1.make_ca()
            env1.ca_sha1 = "AB" * 20
            env1.trust_ca()
            rec2 = FakeRecorder(Path(tmp) / "evidence")
            env2 = m.Interception(rec2, Path(tmp) / "evidence", Path(tmp) / "work", LIVE, real_banner=False, shared_dir=Path(tmp) / "shared")
            env2.make_ca()
            self.assertFalse(env2.ca_reused, "api.github.com is in the SAN list only when the switch is off: a different CA is needed")
            self.assertEqual(make.call_count, 2)
            self.assertTrue(any("remove-trusted-cert" in c for c in self.joined(rec2)), "the earlier CA is untrusted first")

    def test_cleanup_removes_the_job_ca_once_and_never_fails(self):
        with tempfile.TemporaryDirectory() as tmp, mock.patch.dict(m.os.environ, {"GITHUB_ACTIONS": "true", "RUNNER_TEMP": str(Path(tmp) / "rt")}):
            (Path(tmp) / "rt").mkdir()
            shared = m.shared_dir()
            (shared / "ca").mkdir(parents=True, exist_ok=True)
            (shared / "ca-state.json").write_text(json.dumps({"ca_sha256": "ab" * 32, "ca_sha1": "CD" * 20, "hostnames": ["github.com"], "trusted": True}))
            commands = []

            def fake_run(argv, **kwargs):
                commands.append(list(argv))
                return subprocess.CompletedProcess(argv, 0, b"", b"")

            with mock.patch.object(m.subprocess, "run", side_effect=fake_run):
                self.assertEqual(m.main(["cleanup", "--evidence", str(Path(tmp) / "ev")]), 0)
                self.assertEqual(m.main(["cleanup", "--evidence", str(Path(tmp) / "ev")]), 0)
            removals = [c for c in commands if "remove-trusted-cert" in c]
            self.assertEqual(len(removals), 1, "the second cleanup finds nothing trusted any more")
            self.assertFalse(json.loads((shared / "ca-state.json").read_text())["trusted"])

    def make_with(self, tmp, answers, real_banner):
        rec = FakeRecorder(Path(tmp) / "evidence", answers)
        env = m.Interception(rec, Path(tmp) / "evidence", Path(tmp) / "work", LIVE, real_banner=real_banner)
        (Path(tmp) / "work").mkdir(parents=True, exist_ok=True)
        return rec, env

    def run_map_hosts(self, env):
        loopback = [(2, 1, 6, "", ("127.0.0.1", 443))]
        with mock.patch.object(m.shutil, "copyfile", side_effect=lambda src, dst: Path(dst).write_text("127.0.0.1 localhost\n")), \
                mock.patch.object(env, "resolve_banner_api", return_value=["140.82.112.5"]), mock.patch("socket.getaddrinfo", return_value=loopback):
            return env.map_hosts()

    def test_hosts_are_appended_flushed_and_restored_from_the_backup(self):
        with tempfile.TemporaryDirectory() as tmp:
            rec, env = self.make_with(tmp, [("dscacheutil -q host", (0, "name: x\nipv6_address: ::1\nip_address: 127.0.0.1\n"))], real_banner=False)
            info = self.run_map_hosts(env)
            self.assertTrue(info["ok"])
            appended = [call for call in rec.calls if call["argv"][:4] == ["sudo", "-n", "tee", "-a"]]
            self.assertEqual(len(appended), 1)
            self.assertEqual(appended[0]["input"], m.hosts_block(env.hosts))
            self.assertIn("127.0.0.1 api.github.com", appended[0]["input"], "with the switch off every GitHub name goes to the recorder")
            commands = self.joined(rec)
            self.assertTrue(any("dscacheutil -flushcache" in c for c in commands))
            self.assertTrue(any("killall -HUP mDNSResponder" in c for c in commands))
            env.unmap_hosts()
            self.assertTrue(any(c.startswith("sudo -n cp ") and c.endswith(" /etc/hosts") for c in self.joined(rec)))
            self.assertFalse(env.hosts_on)

    def test_real_banner_mode_leaves_exactly_api_github_com_to_the_real_dns(self):
        self.assertNotIn("api.github.com", m.intercept_hosts(True))
        self.assertIn("api.github.com", m.intercept_hosts(False))
        for name in ("github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com", "codeload.github.com", "rsms.me"):
            self.assertIn(name, m.intercept_hosts(True), name)
        self.assertEqual(set(m.INTERCEPT_HOSTS) - set(m.intercept_hosts(True)), {"api.github.com"})

        def dscache(argv):
            name = argv[-1]
            return (0, "name: %s\nip_address: %s\n" % (name, "127.0.0.1" if name == "github.com" else "140.82.112.5"))

        with tempfile.TemporaryDirectory() as tmp:
            rec, env = self.make_with(tmp, [("dscacheutil -q host", dscache)], real_banner=True)
            info = self.run_map_hosts(env)
            self.assertTrue(info["ok"], info)
            block = [call for call in rec.calls if call["argv"][:4] == ["sudo", "-n", "tee", "-a"]][0]["input"]
            self.assertNotIn("api.github.com", block)
            self.assertIn("127.0.0.1 rsms.me", block)
            self.assertTrue(info["api_github_com_left_to_real_dns"])
            self.assertEqual(env.facts["hosts_mapped"], list(m.intercept_hosts(True)))

    def test_real_banner_mode_is_not_ok_when_api_github_com_resolves_to_loopback(self):
        with tempfile.TemporaryDirectory() as tmp:
            rec, env = self.make_with(tmp, [("dscacheutil -q host", (0, "ip_address: 127.0.0.1\n"))], real_banner=True)
            self.assertFalse(self.run_map_hosts(env)["ok"])

    def test_a_hosts_name_that_does_not_resolve_to_loopback_is_not_ok(self):
        with tempfile.TemporaryDirectory() as tmp:
            rec, env = self.make_with(tmp, [("dscacheutil -q host", (0, "name: github.com\nip_address: 140.82.112.3\n"))], real_banner=False)
            self.assertFalse(self.run_map_hosts(env)["ok"])

    def test_the_real_banner_statement_is_recorded_in_the_isolation_facts(self):
        with tempfile.TemporaryDirectory() as tmp:
            rec, env = self.make_with(tmp, [], real_banner=True)
            facts = env.facts["real_banner_api"]
            self.assertTrue(facts["enabled"])
            self.assertFalse(facts["content_recorded"])
            for needle in ("api.github.com", "read-only", "unauthenticated", "one per click", "NOT recorded", "ARC node addresses stay blocked", "14:18:32Z"):
                self.assertIn(needle, facts["scope"])
            off_rec, off = self.make_with(tmp, [], real_banner=False)
            self.assertFalse(off.facts["real_banner_api"]["enabled"])

    def test_teardown_is_idempotent_and_restores_everything(self):
        with tempfile.TemporaryDirectory() as tmp:
            rec, env = self.make(tmp)
            env.ca = {"ca_cert": "/w/ca.crt"}
            env.trusted = True
            env.ca_sha1 = "BB" * 20
            env.hosts_on = True
            env.hosts_backup = Path(tmp) / "hosts.before"
            env.pf_enabled_by_us = True
            env.teardown()
            first = len(rec.calls)
            env.teardown()
            self.assertEqual(len(rec.calls), first, "a second teardown does nothing")
            commands = self.joined(rec)
            self.assertTrue(any("/etc/hosts" in c for c in commands))
            self.assertTrue(any("remove-trusted-cert" in c for c in commands))
            self.assertTrue(any(c == "sudo -n pfctl -d" for c in commands))
            self.assertTrue((Path(tmp) / "evidence" / "isolation.json").exists())

    def test_the_server_runs_as_root_with_the_private_key_on_the_runner_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            rec, env = self.make(tmp)
            env.ca = {"server_cert": "/w/ca/server.crt", "server_key": "/w/ca/private/server.key", "ca_cert": "/w/ca/ca.crt"}
            captured = {}

            class FakeProc:
                def poll(self):
                    return None

                def wait(self, timeout=None):
                    return 0

                def kill(self):
                    pass

            def fake_popen(argv, **kwargs):
                captured["argv"] = argv
                ready = Path(argv[argv.index("--ready-file") + 1])
                ready.write_text(json.dumps({"pid": 4321, "port": 443}))
                return FakeProc()

            with mock.patch.object(m.subprocess, "Popen", side_effect=fake_popen):
                env.start_server("bait-0.8.11", Path(tmp) / "evidence" / "requests-x.jsonl")
            argv = captured["argv"]
            self.assertEqual(argv[:3], ["sudo", "-n", m.sys.executable])
            self.assertIn("--scenario", argv)
            self.assertEqual(argv[argv.index("--scenario") + 1], "bait-0.8.11")
            self.assertEqual(argv[argv.index("--listen") + 1], "127.0.0.1,::1:443")
            self.assertEqual(env.server_pid, 4321)
            env.stop_server()
            self.assertTrue(any(c == "sudo -n kill -TERM 4321" for c in self.joined(rec)))
            for call in rec.calls:
                self.assertNotIn("PRIVATE KEY", json.dumps(call))


def forbid(*args, **kwargs):
    raise AssertionError("a unit test tried to start a real process: %r" % (args[:1],))


class FakeEnv:
    """Stands in for Interception: no hosts, keychain, pf or server is touched; the 'server' writes a scripted request log."""

    DEFAULT_ROWS = {
        "latest-404": [dict(MANIFEST_302), dict(MANIFEST_404), req("other", "/inter/inter.css", host="rsms.me", status=404)],
        "bait-0.8.11": [dict(MANIFEST_200)],
    }

    def __init__(self, rec=None, evidence=None, work=None, live_ips=(), real_banner=True, rows=None, shared_dir=None, keep_ca=False, refresh=None):
        self.real_banner = real_banner
        self.keep_ca = keep_ca
        self.refresh = refresh if refresh is not None else {"added": [], "verified": True}
        self.banner_api_ips = ["140.82.112.5"] if real_banner else []
        self.extra_block_ips = list(RSMS)
        self.live_ips = list(LIVE)
        self.hosts = m.intercept_hosts(real_banner)
        self.facts = {"hosts_mapped": list(self.hosts), "ca_sha256": "ab" * 32, "live_block": {"verified": True}, "real_banner_api": {"enabled": real_banner}}
        self.rows = rows or {}
        self.torn = False
        self.served = []

    @property
    def blocked_ips(self):
        return list(LIVE) + list(self.extra_block_ips)

    def refresh_extra_blocks(self):
        return dict(self.refresh)

    def block_live_network(self):
        return {"verified": True}

    def make_ca(self):
        return {"ca_sha256": "ab" * 32}

    def trust_ca(self):
        return {"trusted": True}

    def map_hosts(self):
        return {"ok": True}

    def self_test(self):
        return {"landed_on_recorder": True}

    def live_block_counters(self):
        return "block drop out quick inet from any to %s [ Evaluations: 3 Packets: 6 ]" % LIVE[0]

    def teardown(self):
        self.torn = True

    def server(self, scenario, log_path):
        env = self

        class Context:
            def __enter__(self):
                rows = env.rows.get(scenario, FakeEnv.DEFAULT_ROWS.get(scenario, []))
                Path(log_path).write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")
                env.served.append((scenario, Path(log_path).name))

            def __exit__(self, *args):
                return False

        return Context()


class FakeWatcher:
    SEEN = [{"pid": 3, "ppid": 1, "comm": "/usr/libexec/xpcproxy", "first_seen": 1.0}]

    def __init__(self, rec=None, interval=0.5):
        pass

    def snapshot_rows(self):
        return []

    def start(self):
        pass

    def stop(self):
        return list(self.SEEN)


class World:
    """Every heavy function of os_macos replaced; anything that would start a real process fails the test."""

    def __init__(self, tmp, **overrides):
        self.tmp = Path(tmp)
        (self.tmp / "work").mkdir(exist_ok=True)
        self.stack = contextlib.ExitStack()
        self.overrides = overrides

    def defaults(self):
        return {
            "accessibility_tests": lambda rec, evidence: self.accessibility(evidence),
            "ensure_tag": lambda rec: {"fetched": False},
            "prepare_app": lambda rec, evidence, work, system: (self.tmp / "ARC Node.app", {"name": "a.dmg", "sha256": "1" * 64, "release_digest": "sha256:" + "1" * 64, "digest_match": True},
                                                               {"version": "0.7.11", "identifier": "network.arc.desktop", "binary": "/x/arc-desktop", "executable": "arc-desktop"},
                                                               {"crates_in_binary": ["tauri-plugin-updater-2.10.1"], "plugin_pinned": True}),
            "build_native": lambda rec, evidence, timeout_s=1500.0, background=False: Path("/x/native"),
            "Interception": FakeEnv,
        }

    def __enter__(self):
        patch = self.stack.enter_context
        patch(mock.patch.dict(m.os.environ, {"GITHUB_ACTIONS": "true"}))
        patch(mock.patch.object(m.subprocess, "run", side_effect=forbid))
        patch(mock.patch.object(m.subprocess, "Popen", side_effect=forbid))
        patch(mock.patch.object(m, "work_dir", return_value=self.tmp / "work"))
        patch(mock.patch.object(m, "system_facts", return_value={"machine": "arm64", "mac_ver": "15.0", "image": "macos-15 test", "sudo": {"rc": 0, "out": ""}}))
        patch(mock.patch.object(m, "load_live_ips", return_value=list(LIVE)))
        defaults = self.defaults()
        defaults.update(self.overrides)
        for name, value in defaults.items():
            patch(mock.patch.object(m, name, value))
        return self

    def __exit__(self, *args):
        self.stack.close()
        return False

    @staticmethod
    def accessibility(evidence, click=True):
        result = {"tests": {}, "summary": {"system_events_reachable": True, "click_works": click, "click_evidence": "Finder windows 0 -> 1" if click else None,
                                           "click_error_kind": None if click else "not_allowed_assistive", "click_error": None if click else "-25211"}}
        m.write_json(Path(evidence) / "accessibility.json", result)
        return result


class FlowTests(unittest.TestCase):
    """cmd_run / cmd_probe write their evidence even when everything goes wrong, and never touch the machine here."""

    def run_cmd(self, argv):
        return m.main(argv)

    def test_run_and_probe_refuse_outside_a_github_actions_runner(self):
        with mock.patch.dict(m.os.environ, {"GITHUB_ACTIONS": "false"}), tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(self.run_cmd(["run", "--evidence", str(Path(tmp) / "ev")]), 2)
            self.assertEqual(self.run_cmd(["probe", "--evidence", str(Path(tmp) / "ev")]), 2)
            self.assertFalse((Path(tmp) / "ev").exists(), "nothing is created, nothing is run")

    def test_run_writes_an_unproved_result_when_nothing_can_be_done(self):
        with tempfile.TemporaryDirectory() as tmp, World(tmp, **{}) as world, mock.patch.object(m, "load_live_ips", side_effect=ValueError("no live list")):
            code = self.run_cmd(["run", "--evidence", str(Path(tmp) / "ev")])
            self.assertEqual(code, 0)
            result = json.loads((Path(tmp) / "ev" / "result.json").read_text())
            self.assertEqual(result["verdict"], "UNPROVED")
            self.assertTrue(any("no live list" in item for item in result["problems"]))
            self.assertEqual(result["cases"], [])

    def test_run_refuses_to_run_a_case_when_the_block_cannot_be_verified(self):
        class Blocked(FakeEnv):
            def block_live_network(self):
                return {"verified": False}

        created = {}

        def factory(*args, **kwargs):
            created["env"] = Blocked(*args, **kwargs)
            return created["env"]

        with tempfile.TemporaryDirectory() as tmp, World(tmp, Interception=factory, prepare_app=mock.Mock(side_effect=m.StepFailed("offline"))) as world, \
                mock.patch.object(m, "run_native_case") as native_case, mock.patch.object(m, "run_app_case") as app_case:
            self.assertEqual(self.run_cmd(["run", "--evidence", str(Path(tmp) / "ev")]), 0)
            native_case.assert_not_called()
            app_case.assert_not_called()
            result = json.loads((Path(tmp) / "ev" / "result.json").read_text())
            self.assertEqual(result["verdict"], "UNPROVED")
            self.assertTrue(any("live-network block" in item for item in result["problems"]))
            self.assertTrue(created["env"].torn, "the environment is torn down even when the run stopped early")

    def native_case(self, rec, env, evidence, work, binary, case, real_home=False):
        outcome = {"error": "Could not fetch a valid release JSON from the remote"} if case == "clean" else {}
        return {"name": case, "tier": "native_check", "scenario": m.SCENARIO_FOR_CASE[case], "trigger_outcome": outcome, "verdict": "PASS"}

    def app_case(self, reached, message=None):
        def run(rec, env, evidence, work, app, facts, case, probe=False):
            check = {"reached": True, "outcome": "error", "error_kind": "ReleaseNotFound", "error": message} if reached else {"reached": False, "reason": "the Install button was not rendered"}
            return {"name": case, "tier": "released_app", "scenario": "latest-404", "trigger_outcome": {"plugin_check": check}, "verdict": "PASS" if reached else "UNPROVED"}
        return run

    def test_happy_path_with_the_released_app_reaching_the_plugin(self):
        text = "Could not fetch a valid release JSON from the remote"
        with tempfile.TemporaryDirectory() as tmp, World(tmp, run_native_case=self.native_case, run_app_case=self.app_case(True, text)) as world:
            self.assertEqual(self.run_cmd(["run", "--evidence", str(Path(tmp) / "ev")]), 0)
            ev = Path(tmp) / "ev"
            result = json.loads((ev / "result.json").read_text())
            self.assertEqual(result["tiers"]["native_check"]["result"], "ran")
            self.assertEqual(result["tiers"]["released_app"]["result"], "ran")
            self.assertEqual([c["name"] for c in result["cases"]], ["clean", "cached-bait", "clean", "cached-bait"])
            self.assertEqual(result["verdict"], "PASS")
            self.assertEqual((ev / "manifest404-error.txt").read_text().strip(), text)
            self.assertIn("released_app tier", result["manifest404_error_source"], "the released app's own UI text wins")
            self.assertEqual(result["manifest404_error_texts"]["native_check"], text)
            self.assertTrue(result["real_banner_api"]["enabled"])
            self.assertEqual(result["isolation"]["hosts_mapped"], list(m.intercept_hosts(True)))
            self.assertTrue(result["isolation"]["real_banner_api"]["enabled"])

    def test_released_app_without_the_install_button_is_infeasible_and_the_overall_verdict_unproved(self):
        with tempfile.TemporaryDirectory() as tmp, World(tmp, run_native_case=self.native_case, run_app_case=self.app_case(False)) as world:
            self.assertEqual(self.run_cmd(["run", "--evidence", str(Path(tmp) / "ev"), "--no-real-banner-api"]), 0)
            ev = Path(tmp) / "ev"
            result = json.loads((ev / "result.json").read_text())
            self.assertEqual(result["tiers"]["released_app"]["result"], "infeasible")
            self.assertIn("Install button was not rendered", result["tiers"]["released_app"]["reason"])
            self.assertFalse(result["tiers"]["released_app"]["real_banner_api"])
            self.assertEqual(result["verdict"], "UNPROVED")
            self.assertEqual(result["tier_verdicts"], {"released_app": "INFEASIBLE", "native_check": "PASS"})
            self.assertEqual((ev / "manifest404-error.txt").read_text().strip(), "Could not fetch a valid release JSON from the remote", "falls back to the native checker's text")
            self.assertIn("native_check tier", result["manifest404_error_source"])

    def test_the_real_banner_switch_reaches_the_environment(self):
        seen = {}

        def factory(*args, **kwargs):
            seen["real_banner"] = kwargs.get("real_banner")
            return FakeEnv(*args, **kwargs)

        for flag, expected in ((None, True), ("--no-real-banner-api", False), ("--real-banner-api", True)):
            with self.subTest(flag=flag), tempfile.TemporaryDirectory() as tmp, World(tmp, Interception=factory, run_native_case=self.native_case, run_app_case=self.app_case(False)):
                argv = ["run", "--evidence", str(Path(tmp) / "ev"), "--tier", "native_check"] + ([flag] if flag else [])
                self.assertEqual(self.run_cmd(argv), 0)
                self.assertIs(seen["real_banner"], expected)

    def test_probe_always_writes_its_three_files_even_when_every_step_fails(self):
        def boom(*args, **kwargs):
            raise RuntimeError("boom")

        with tempfile.TemporaryDirectory() as tmp, World(tmp, accessibility_tests=boom, ensure_tag=boom, prepare_app=boom) as world, \
                mock.patch.object(m, "system_facts", side_effect=RuntimeError("no facts")):
            self.assertEqual(self.run_cmd(["probe", "--evidence", str(Path(tmp) / "ev"), "--no-build"]), 0)
            ev = Path(tmp) / "ev"
            probe = json.loads((ev / "probe.json").read_text())
            self.assertIn("boom", json.dumps(probe))
            self.assertFalse(probe["steps"]["release_download"]["ok"])
            self.assertEqual(len((ev / "feasibility.txt").read_text().strip().splitlines()), 2)
            self.assertTrue((ev / "accessibility.json").exists(), "accessibility.json exists even when the tests could not run")

    def test_probe_survives_a_crash_outside_its_guards(self):
        with tempfile.TemporaryDirectory() as tmp, World(tmp) as world, mock.patch.object(m, "cmd_probe", side_effect=RuntimeError("outside")):
            self.assertEqual(self.run_cmd(["probe", "--evidence", str(Path(tmp) / "ev")]), 0)
            ev = Path(tmp) / "ev"
            self.assertIn("outside", json.loads((ev / "probe.json").read_text())["fatal"])
            self.assertTrue((ev / "feasibility.txt").read_text().startswith("NO:"))
            self.assertTrue((ev / "accessibility.json").exists())

    def test_probe_full_path_writes_the_answer_before_it_waits_for_the_build(self):
        def app_case(rec, env, evidence, work, app, facts, case, probe=False):
            return {
                "name": case, "tier": "released_app", "scenario": "latest-404", "verdict": "PASS", "criteria": {}, "requests": {"by_host_path": [["github.com", m.MANIFEST_PATH, 2]]},
                "network": {"recorded": True, "violations": []},
                "trigger_outcome": {"ui": {"real_banner_api": True, "window_appeared": True, "nodes": 90, "settings_button": True, "check_button": True, "install_button": True,
                                           "install_clicked": True, "ui_error_text": "Could not fetch a valid release JSON from the remote"}},
            }

        class Proc:
            returncode = 0

            def __init__(self):
                self.polls = 0

            def poll(self):
                self.polls += 1
                return 0

        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "native"
            binary.write_bytes(b"built")
            world = World(tmp, run_app_case=app_case, build_native=lambda rec, evidence, timeout_s=1500.0, background=False: (Proc(), binary))
            with world:
                self.assertEqual(self.run_cmd(["probe", "--evidence", str(Path(tmp) / "ev"), "--build-wait-min", "0.01"]), 0)
            probe = json.loads((Path(tmp) / "ev" / "probe.json").read_text())
            ui = probe["released_app_ui"]
            self.assertTrue(ui["install_clicked"])
            self.assertEqual(ui["manifest_requests"], 2)
            self.assertIn("Install button present after Check for updates = YES", probe["feasibility"])
            self.assertIn("Could not fetch a valid release JSON", probe["feasibility"])
            self.assertEqual(probe["native_check"]["status"], "cargo build succeeded (rc 0)")
            self.assertTrue(probe["isolation"]["real_banner_api"]["enabled"])

    def test_the_workflow_call_with_arch_is_accepted_and_a_mismatch_is_recorded_not_fatal(self):
        for flag, machine, expected in (("arm64", "arm64", True), ("x86_64", "x86_64", True), ("x86_64", "arm64", False)):
            with self.subTest(flag=flag, machine=machine), tempfile.TemporaryDirectory() as tmp, World(tmp, run_native_case=self.native_case, run_app_case=self.app_case(False)), \
                    mock.patch.object(m.platform, "machine", return_value=machine):
                self.assertEqual(self.run_cmd(["probe", "--evidence", str(Path(tmp) / "pev"), "--no-build", "--arch", flag]), 0)
                self.assertIs(json.loads((Path(tmp) / "pev" / "probe.json").read_text())["arch_matches"], expected)
                self.assertEqual(self.run_cmd(["run", "--evidence", str(Path(tmp) / "rev"), "--arch", flag]), 0)
                problems = json.loads((Path(tmp) / "rev" / "result.json").read_text())["problems"]
                self.assertEqual(any("the job asked for" in item for item in problems), not expected)
        self.assertIsNone(m.arch_matches(None, "arm64"))
        self.assertTrue(m.arch_matches("aarch64", "arm64"))
        self.assertTrue(m.arch_matches("x64", "x86_64"))
        self.assertFalse(m.arch_matches("bogus", "arm64"))

    @staticmethod
    def accepted_by(double, real):
        """Names of the real function's parameters that the double cannot take (drift between a double and the function it replaces)."""
        d, r = inspect.signature(double), inspect.signature(real)
        takes_any = any(p.kind is inspect.Parameter.VAR_KEYWORD for p in d.parameters.values())
        wanted = [p.name for p in r.parameters.values() if p.name != "self" and p.kind not in (inspect.Parameter.VAR_POSITIONAL, inspect.Parameter.VAR_KEYWORD)]
        return [name for name in wanted if name not in d.parameters and not takes_any]

    def test_every_test_double_accepts_every_parameter_of_the_function_it_replaces(self):
        flow = FlowTests("test_tier_option_is_validated")
        with tempfile.TemporaryDirectory() as tmp:
            doubles = World(tmp).defaults()
            doubles["run_native_case"] = flow.native_case
            doubles["run_app_case"] = flow.app_case(True)
            for name, double in doubles.items():
                real = m.Interception.__init__ if name == "Interception" else getattr(m, name)
                double = FakeEnv.__init__ if name == "Interception" else double
                with self.subTest(function=name):
                    self.assertEqual(self.accepted_by(double, real), [], "the double of %s lost a parameter the real function has" % name)
            self.assertEqual(self.accepted_by(FakeWatcher.__init__, m.ProcessWatcher.__init__), [])

    def test_nothing_in_the_flows_depends_on_the_host_platform(self):
        for machine in ("arm64", "x86_64", "AMD64"):
            for plat in ("linux", "darwin", "win32"):
                with self.subTest(machine=machine, platform=plat), tempfile.TemporaryDirectory() as tmp, mock.patch.object(m.platform, "machine", return_value=machine), \
                        mock.patch.object(m.sys, "platform", plat), World(tmp, run_native_case=self.native_case, run_app_case=self.app_case(True, "Could not fetch a valid release JSON from the remote")):
                    self.assertEqual(self.run_cmd(["run", "--evidence", str(Path(tmp) / "ev")]), 0)
                    result = json.loads((Path(tmp) / "ev" / "result.json").read_text())
                    self.assertEqual(result["verdict"], "PASS")
                    self.assertEqual(result["tiers"]["released_app"]["result"], "ran")
                    self.assertEqual(result["tiers"]["native_check"]["result"], "ran")
                    self.assertEqual(self.run_cmd(["probe", "--evidence", str(Path(tmp) / "pev"), "--no-build"]), 0)
                    self.assertTrue((Path(tmp) / "pev" / "feasibility.txt").exists())

    def test_unknown_case_is_refused_before_anything_runs(self):
        with tempfile.TemporaryDirectory() as tmp, World(tmp) as world:
            with self.assertRaises(SystemExit):
                self.run_cmd(["run", "--evidence", str(Path(tmp) / "ev"), "--cases", "bogus"])

    def test_tier_option_is_validated(self):
        with self.assertRaises(SystemExit):
            m.main(["run", "--evidence", "/nonexistent", "--tier", "windows"])


class NetworkParsingTests(unittest.TestCase):
    TCPDUMP = """tcpdump: data link type PKTAP
1760000000.100000 (proc ARC Node:4242) IP 10.0.0.2.51000 > 140.82.112.5.443: Flags [S], seq 1, win 65535, length 0
1760000000.150000 (proc ARC Node:4242) IP 140.82.112.5.443 > 10.0.0.2.51000: Flags [S.], seq 2, ack 2, win 65535, length 0
1760000000.200000 (proc ARC Node:4242) IP 10.0.0.2.51000 > 140.82.112.5.443: Flags [P.], seq 2:519, ack 2, win 2048, length 517
1760000000.300000 IP 10.0.0.2.60000 > 10.0.0.1.53: 12345+ A? api.github.com. (32)
1760000000.320000 IP 10.0.0.1.53 > 10.0.0.2.60000: 12345 2/0/1 A 140.82.112.5, A 140.82.112.6 (80)
1760000001.000000 (proc com.apple.WebKit.Networking:555) IP 10.0.0.2.52000 > 203.0.113.9.443: Flags [S], seq 1, win 65535, length 0
1760000001.500000 (proc ARC Node:4242) IP6 fe80::1.51111 > 2606:50c0:8000::154.443: Flags [S], seq 1, length 0
1760000002.000000 (proc ARC Node:4242) IP 10.0.0.2.52100 > %s.9090: Flags [S], seq 1, length 0
"""

    def test_tcpdump_lines_become_packets_with_process_names_and_dns(self):
        packets = m.parse_tcpdump_text(self.TCPDUMP)
        self.assertEqual(len(packets), 8)
        first = packets[0]
        self.assertEqual((first["src_ip"], first["src_port"], first["dst_ip"], first["dst_port"], first["proc"], first["pid"]), ("10.0.0.2", 51000, "140.82.112.5", 443, "ARC Node", 4242))
        self.assertEqual(packets[2]["length"], 517)
        self.assertEqual(packets[3]["dns_query"], "api.github.com")
        self.assertEqual(packets[4]["dns_answers"], ["140.82.112.5", "140.82.112.6"])
        self.assertEqual(packets[4]["dns_query"], "api.github.com", "the answer is tied to its query by transaction id")
        self.assertEqual(packets[6]["dst_ip"], "2606:50c0:8000::154", "IPv6 addresses keep their colons")
        self.assertEqual(m.parse_tcpdump_text(""), [])
        self.assertEqual(m.parse_tcpdump_text("garbage\nmore garbage\n"), [])

    def test_flows_are_summarized_per_remote_with_labels(self):
        packets = m.parse_tcpdump_text(self.TCPDUMP % LIVE[0])
        summary = m.summarize_flows(packets, ["140.82.112.5"], LIVE)
        labels = {(flow["remote_ip"], flow["remote_port"]): flow for flow in summary["flows"]}
        banner = labels[("140.82.112.5", 443)]
        self.assertEqual((banner["label"], banner["packets_out"], banner["packets_in"], banner["bytes_out"]), ("banner_api", 2, 1, 517))
        self.assertEqual(banner["procs"], ["ARC Node:4242"])
        self.assertEqual(labels[("10.0.0.1", 53)]["label"], "dns")
        self.assertEqual(labels[("203.0.113.9", 443)]["label"], "other")
        self.assertEqual(summary["dns_names"], {"api.github.com": ["140.82.112.5", "140.82.112.6"]})
        self.assertNotIn(LIVE[0], [flow["remote_ip"] for flow in summary["flows"] if flow["remote_port"] == 443], "port 9090 is outside the capture filter ports")
        self.assertEqual(m.summarize_flows([], [], [])["flows"], [])

    LSOF = (
        "p4242\ncARC Node\nf20\ntIPv4\nn10.0.0.2:51000->140.82.112.5:443\nTST=ESTABLISHED\nTQR=0\n"
        "f21\ntIPv4\nn10.0.0.2:51001->%(live)s:9090\nTST=SYN_SENT\n"
        "f22\ntIPv4\nn*:5353\n"
        "p555\ncWebKit Networking\nf5\ntIPv4\nn10.0.0.2:52000->203.0.113.9:443\nTST=ESTABLISHED\n"
        "p777\ncSafari\nf5\ntIPv6\nn[fe80::1]:50000->[2606:4700::1]:443\nTST=ESTABLISHED\n"
        "p4242\ncARC Node\nf30\ntIPv4\nn127.0.0.1:9000->127.0.0.1:9090\nTST=ESTABLISHED\n"
    )

    def test_lsof_field_output_becomes_endpoints(self):
        endpoints = m.parse_lsof_f(self.LSOF % {"live": LIVE[1]})
        self.assertEqual(len(endpoints), 5, "the listening socket has no remote and is skipped")
        self.assertEqual((endpoints[0]["pid"], endpoints[0]["command"], endpoints[0]["remote_ip"], endpoints[0]["remote_port"], endpoints[0]["state"]), (4242, "ARC Node", "140.82.112.5", 443, "ESTABLISHED"))
        self.assertEqual(endpoints[3]["remote_ip"], "2606:4700::1")
        self.assertEqual(m.parse_lsof_f(""), [])

    def test_network_report_allows_the_banner_blocks_live_nodes_and_names_everything_else(self):
        endpoints = m.parse_lsof_f(self.LSOF % {"live": LIVE[1]})
        report = m.network_report(endpoints, None, [4242, 555], ["140.82.112.5"], LIVE, real_banner=True)
        self.assertEqual([item["remote_ip"] for item in report["allowed_banner_endpoints"]], ["140.82.112.5"])
        self.assertEqual([item["remote_ip"] for item in report["blocked_live_node_attempts"]], [LIVE[1]])
        self.assertEqual([item["remote_ip"] for item in report["violations"]], ["203.0.113.9"], "the WebKit helper reached an address nobody allowed")
        self.assertEqual(report["background_endpoints"], 1, "Safari is not part of the app")
        self.assertTrue(report["recorded"])
        off = m.network_report(endpoints, None, [4242, 555], ["140.82.112.5"], LIVE, real_banner=False)
        self.assertEqual(sorted(item["remote_ip"] for item in off["violations"]), ["140.82.112.5", "203.0.113.9"], "with the switch off even the banner address is a violation")

    def test_an_established_connection_to_a_live_node_means_the_block_failed(self):
        endpoints = [{"pid": 4242, "command": "ARC Node", "remote_ip": LIVE[2], "remote_port": 9090, "state": "ESTABLISHED"}]
        report = m.network_report(endpoints, None, [4242], [], LIVE, real_banner=False)
        self.assertEqual(len(report["violations"]), 1)
        self.assertEqual(report["blocked_live_node_attempts"], [])

    def test_capture_flows_with_process_names_also_count(self):
        flows = m.summarize_flows(m.parse_tcpdump_text(self.TCPDUMP % LIVE[0]), ["140.82.112.5"], LIVE)
        report = m.network_report([], flows, [4242, 555], ["140.82.112.5"], LIVE, real_banner=True)
        self.assertEqual(sorted(item["remote_ip"] for item in report["violations"]), ["203.0.113.9", "2606:50c0:8000::154"])
        self.assertTrue(report["capture_attributed"])
        self.assertTrue(report["recorded"])
        empty = m.network_report([], None, [4242], [], LIVE, real_banner=True)
        self.assertFalse(empty["recorded"])

    def test_related_pids_follow_the_app_its_children_and_the_webkit_helpers(self):
        seen = [{"pid": 4242, "ppid": 1, "comm": "/sbx/ARC Node.app/Contents/MacOS/arc-desktop"}, {"pid": 4300, "ppid": 4242, "comm": "/usr/bin/xattr"},
                {"pid": 555, "ppid": 1, "comm": "/System/Library/com.apple.WebKit.Networking"}, {"pid": 9, "ppid": 1, "comm": "/usr/sbin/cfprefsd"}]
        self.assertEqual(m.related_pids(seen, 4242), [555, 4242, 4300])
        self.assertEqual(m.related_pids(seen, None), [555, 4242])

    def test_update_error_text_is_taken_verbatim_from_the_updates_card(self):
        texts = ["You're running the latest version.", "Update failed: Could not fetch a valid release JSON from the remote", "Check for updates"]
        self.assertEqual(m.extract_update_error(texts), "Could not fetch a valid release JSON from the remote")
        self.assertIsNone(m.extract_update_error(["Version 0.7.12 is available."]))
        self.assertEqual(m.infer_error_kind("Could not fetch a valid release JSON from the remote"), "ReleaseNotFound")
        self.assertIsNone(m.infer_error_kind("error sending request for url"))
        self.assertIsNone(m.infer_error_kind(None))


class EvaluateWithNetworkTests(unittest.TestCase):
    BUNDLE_PATH = "/sbx/apps/ARC Node.app"
    MAIN = "/sbx/apps/ARC Node.app/Contents/MacOS/arc-desktop"
    ROWS = [dict(MANIFEST_302), dict(MANIFEST_404), req("other", "/inter/inter.css", host="rsms.me", status=404), {"kind": "tls_failure", "sni": "rsms.me"}]
    PLUGIN = {"reached": True, "outcome": "error", "error_kind": "ReleaseNotFound", "error": "Could not fetch a valid release JSON from the remote"}
    NETWORK_OK = {"recorded": True, "violations": [], "allowed_banner_endpoints": [{"remote_ip": "140.82.112.5"}]}
    FS = {"diff": {"added": [], "removed": [], "changed": []}, "poller_scans": 9, "poller_events": [], "expected_prefixes": ["/sbx/home"], "temp_roots": []}
    PROCS = {"seen": [{"pid": 50, "ppid": 1, "comm": MAIN}, {"pid": 60, "ppid": 1, "comm": "/System/com.apple.WebKit.WebContent"}], "app_pid": 50, "bundle_path": BUNDLE_PATH}

    def evaluate(self, rows=None, plugin=None, network=NETWORK_OK, require=True, fs=None, procs=None):
        return m.evaluate_case("released_app", "latest-404", self.ROWS if rows is None else rows, fs or self.FS, procs or self.PROCS,
                               {"added": [], "removed": [], "changed": []}, plugin or self.PLUGIN, None, network, require)

    def test_the_full_chain_through_the_released_app_passes(self):
        result = self.evaluate()
        self.assertEqual(result["verdict"], "PASS", result)
        self.assertEqual(result["criteria"], {key: True for key in m.CRITERIA})
        self.assertTrue(any("rsms.me" in line for line in result["info"]), "the font host is reported, not held against the updater")

    def test_requests_to_the_font_host_do_not_count_as_updater_requests(self):
        self.assertIs(self.evaluate()["criteria"]["only_manifest_url"], True)
        self.assertIs(self.evaluate(rows=self.ROWS + [req("other", "/FerrumVir/arc-chain/releases/download/v0.8.11/latest.json", status=404)])["criteria"]["only_manifest_url"], False)

    def test_another_real_destination_fails_and_is_named(self):
        network = {"recorded": True, "violations": [{"remote_ip": "203.0.113.9", "remote_port": 443, "command": "WebKit Networking", "pid": 555}], "allowed_banner_endpoints": []}
        result = self.evaluate(network=network)
        self.assertIs(result["criteria"]["only_manifest_url"], False)
        self.assertEqual(result["verdict"], "FAIL")
        self.assertIn("203.0.113.9:443", json.dumps(result["reasons"]))

    def test_a_missing_network_record_leaves_it_unproved_when_a_request_was_let_through(self):
        for network in (None, {"recorded": False, "violations": []}):
            with self.subTest(network=network):
                result = self.evaluate(network=network)
                self.assertIsNone(result["criteria"]["only_manifest_url"])
                self.assertEqual(result["verdict"], "UNPROVED")
        self.assertEqual(self.evaluate(network=None, require=False)["verdict"], "PASS", "with the switch off no network record is required")

    def test_the_ui_message_must_be_release_not_found_in_the_404_scenario(self):
        other = dict(self.PLUGIN, error="error sending request for url", error_kind=None)
        self.assertEqual(self.evaluate(plugin=other)["verdict"], "UNPROVED")
        nothing = dict(self.PLUGIN, outcome=None, error=None, error_kind=None)
        self.assertEqual(self.evaluate(plugin=nothing)["verdict"], "UNPROVED")

    def test_the_install_click_without_a_manifest_request_is_not_a_pass(self):
        result = self.evaluate(rows=[req("other", "/inter/inter.css", host="rsms.me", status=404)])
        self.assertIsNone(result["criteria"]["only_manifest_url"])
        self.assertEqual(result["verdict"], "UNPROVED")


class TierFlowTests(unittest.TestCase):
    """run_native_case and run_app_case end to end on scripted pieces: real file recorder, fake processes, fake osascript."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.ev = Path(self.tmp.name) / "ev"
        self.ev.mkdir()
        self.work = Path(self.tmp.name) / "work"
        self.work.mkdir()
        self.home = Path(self.tmp.name) / "realhome"
        self.home.mkdir()

    def native(self, case, report, control_report=None, env=None):
        def answer(argv):
            return (0, json.dumps(control_report if "--control-download" in argv else report) + "\n")

        rec = FakeRecorder(self.ev, [("/x/native", answer)])
        env = env or FakeEnv()
        with mock.patch.object(m, "ProcessWatcher", FakeWatcher), mock.patch.object(m.subprocess, "run", side_effect=forbid), mock.patch.object(m.subprocess, "Popen", side_effect=forbid):
            return rec, m.run_native_case(rec, env, self.ev, self.work, Path("/x/native"), case)

    REPORT_404 = {"outcome": "error", "error_kind": "ReleaseNotFound", "error": "Could not fetch a valid release JSON from the remote", "update": None, "download_attempted": False}
    REPORT_BAIT = {"outcome": "update_available", "error_kind": None, "error": None, "update": {"version": "0.8.11", "download_url": "https://github.com/x"}, "download_attempted": False}
    REPORT_CONTROL = dict(REPORT_BAIT, download_attempted=True, download_result="error: signature")

    def test_the_native_checker_runs_in_a_sandbox_home_unless_the_debug_switch_says_otherwise(self):
        homes = []

        def answer(argv):
            return (0, json.dumps(self.REPORT_404) + "\n")

        class Rec(FakeRecorder):
            def run(self, argv, label="", timeout=120.0, input_text=None, env=None, **kw):
                homes.append((env or {}).get("HOME"))
                return super().run(argv, label=label, timeout=timeout, input_text=input_text, env=env, **kw)

        for flag in (False, True):
            rec = Rec(self.ev, [("/x/native", answer)])
            with mock.patch.object(m, "ProcessWatcher", FakeWatcher), mock.patch("pathlib.Path.home", return_value=self.home):
                entry = m.run_native_case(rec, FakeEnv(), self.ev, self.work, Path("/x/native"), "clean", real_home=flag)
            self.assertEqual(entry["trigger_outcome"]["home"], "real HOME (--native-real-home)" if flag else "sandbox HOME")
        self.assertIn(str(self.home), homes)
        self.assertTrue(any(h and h.startswith(str(self.work)) for h in homes))

    def test_native_clean_case_passes_and_leaves_its_evidence_files(self):
        rec, entry = self.native("clean", self.REPORT_404)
        self.assertEqual(entry["verdict"], "PASS", entry)
        self.assertEqual(entry["trigger_outcome"]["error"], "Could not fetch a valid release JSON from the remote")
        self.assertEqual(entry["requests"]["total"], 3)
        for name in entry["evidence_files"]:
            self.assertTrue((self.ev / name).exists(), name)
        self.assertIn("requests-clean-native_check.jsonl", entry["evidence_files"])
        call = rec.calls[0]["argv"]
        self.assertEqual(call[:2], ["/x/native", "--endpoint"])
        self.assertEqual(call[call.index("--endpoint") + 1], m.MANIFEST_URL)
        self.assertEqual(call[call.index("--current-version") + 1], "0.7.11")
        self.assertEqual(call[call.index("--pubkey") + 1], m.UPDATER_PUBKEY)
        self.assertNotIn("--insecure-transport", call, "the shipped https configuration is exercised, not the test shortcut")

    def test_native_bait_case_runs_the_positive_control_with_its_own_log(self):
        env = FakeEnv(rows={"bait-0.8.11": [dict(MANIFEST_200), BUNDLE]})
        # the main run is clean (the first server session serves only the manifest), the control session also serves the bundle request
        sessions = []
        original = env.server

        def server(scenario, log_path):
            sessions.append(Path(log_path).name)
            env.rows["bait-0.8.11"] = [dict(MANIFEST_200)] if len(sessions) == 1 else [dict(MANIFEST_200), BUNDLE]
            return original(scenario, log_path)

        env.server = server
        rec, entry = self.native("cached-bait", self.REPORT_BAIT, self.REPORT_CONTROL, env)
        self.assertEqual(sessions, ["requests-cached-bait-native_check.jsonl", "requests-cached-bait-native_check-control.jsonl"])
        self.assertTrue(entry["positive_control"]["observed"])
        self.assertEqual(entry["verdict"], "PASS", entry)
        self.assertIn("requests-cached-bait-native_check-control.jsonl", entry["evidence_files"])
        self.assertTrue(any("--control-download" in call["argv"] for call in rec.calls))

    def test_native_bait_case_is_unproved_when_the_control_cannot_see_a_download(self):
        env = FakeEnv(rows={"bait-0.8.11": [dict(MANIFEST_200)]})
        rec, entry = self.native("cached-bait", self.REPORT_BAIT, self.REPORT_CONTROL, env)
        self.assertFalse(entry["positive_control"]["observed"])
        self.assertEqual(entry["verdict"], "UNPROVED")

    def test_native_case_with_a_bundle_request_in_the_main_run_fails(self):
        env = FakeEnv(rows={"bait-0.8.11": [dict(MANIFEST_200), BUNDLE]})
        rec, entry = self.native("cached-bait", self.REPORT_BAIT, self.REPORT_CONTROL, env)
        self.assertEqual(entry["verdict"], "FAIL")
        self.assertIs(entry["criteria"]["no_bundle_download"], False)

    def test_native_case_without_a_report_is_unproved(self):
        rec = FakeRecorder(self.ev, [("/x/native", (2, "", "usage"))])
        with mock.patch.object(m, "ProcessWatcher", FakeWatcher):
            entry = m.run_native_case(rec, FakeEnv(), self.ev, self.work, Path("/x/native"), "clean")
        self.assertEqual(entry["verdict"], "UNPROVED")
        self.assertFalse(entry["trigger_outcome"]["reached"])

    # ---- released app -------------------------------------------------------------------------------
    @staticmethod
    def dump(buttons=(), texts=()):
        nodes = [{"role": "AXWindow", "title": "ARC Node", "description": None, "name": "ARC Node", "value": None}]
        nodes += [{"role": "AXButton", "title": label, "description": None, "name": label, "value": None} for label in buttons]
        nodes += [{"role": "AXStaticText", "title": None, "description": None, "name": None, "value": text} for text in texts]
        return {"pid": 4242, "windows": 1, "error": None, "truncated": False, "nodes": nodes}

    def run_app(self, install=True, error_text="Update failed: Could not fetch a valid release JSON from the remote", lsof_rows=None, env=None, click_ok=True,
                install_from_attempt=None, pill="v0.7.12", sentence=None, install_before_check=False, sleeps=None):
        class FakeProc:
            pid = 4242
            terminated = False

            def poll(self):
                return 0 if self.terminated else None

            def terminate(self):
                self.terminated = True

            def wait(self, timeout=None):
                return 0

            def kill(self):
                self.terminated = True

        checks = {"dumps": 0}

        def osascript(rec, script, label, timeout=150.0):
            result = m.CmdResult(["osascript"], 0, "", "", 0.0)
            if label == "wait for the app window":
                return {"windows": 1, "nodes": []}, result
            if label == "AX dump after launch":
                return self.dump(["Settings", " Check for updates"], ["Configure your node and app preferences."]), result
            if label == "AX dump on the Settings page":
                return self.dump(["Settings", " Check for updates"] + (["Install v0.7.12 & relaunch"] if install_before_check else []), ["Configure your node and app preferences."]), result
            if label == "AX dump after Check for updates":
                attempt = checks["dumps"] // 7 + 1  # seven polls per click while no Install button shows up
                checks["dumps"] += 1
                shows = install_from_attempt is not None and attempt >= install_from_attempt if install_from_attempt is not None else install
                texts = [sentence or ("Version 0.7.12 is available. Click below to download, install, and relaunch." if shows else "You're running the latest version.")]
                return self.dump(["Settings", " Check for updates"] + (["Install v0.7.12 & relaunch"] if shows else []), ["Updates", pill] + texts), result
            if label == "AX dump after Install":
                return self.dump(["Settings"], [error_text] if error_text else []), result
            raise AssertionError("unexpected osascript label " + label)

        class FakeCapture:
            def __init__(self, rec, path):
                self.proc, self.iface, self.error = object(), "pktap,all", None

            def start(self):
                return True

            def stop(self):
                return "1760000000.100000 (proc ARC Node:4242) IP 10.0.0.2.51000 > 140.82.112.5.443: Flags [P.], seq 1:518, length 517\n"

        rows = lsof_rows if lsof_rows is not None else [
            {"pid": 4242, "command": "ARC Node", "remote_ip": "140.82.112.5", "remote_port": 443, "state": "ESTABLISHED", "t": 1.0},
            {"pid": 4242, "command": "ARC Node", "remote_ip": LIVE[0], "remote_port": 9090, "state": "SYN_SENT", "t": 1.0},
        ]

        class FakeLsof:
            polls = 7
            errors: list = []
            resolved = ["140.82.112.6"]

            def __init__(self, interval=1.0, resolve_host=None):
                pass

            def start(self):
                pass

            def stop(self):
                return list(rows)

        class SeenWatcher(FakeWatcher):
            SEEN = [{"pid": 4242, "ppid": 1, "comm": "%s/apps/ARC Node.app/Contents/MacOS/arc-desktop" % self.work / "sandbox" if False else str(self.work / "ARC Node.app/Contents/MacOS/arc-desktop"), "first_seen": 2.0},
                    {"pid": 555, "ppid": 1, "comm": "/System/Library/com.apple.WebKit.Networking", "first_seen": 3.0}]

        app = self.work / "ARC Node.app"
        (app / "Contents" / "MacOS").mkdir(parents=True, exist_ok=True)
        (app / "Contents" / "MacOS" / "arc-desktop").write_bytes(b"binary")
        rec = FakeRecorder(self.ev)
        env = env or FakeEnv()
        facts = {"binary": str(app / "Contents" / "MacOS" / "arc-desktop")}
        click = mock.Mock(side_effect=lambda rec, pid, patterns, label: {"label": label, "report": {"clicked": click_ok}})
        with mock.patch("pathlib.Path.home", return_value=self.home), mock.patch.object(m, "osascript_json", osascript), mock.patch.object(m, "click_step", click), \
                mock.patch.object(m, "screenshot"), mock.patch.object(m, "ProcessWatcher", SeenWatcher), mock.patch.object(m, "CaptureWatcher", FakeCapture), \
                mock.patch.object(m, "LsofWatcher", FakeLsof), mock.patch.object(m.subprocess, "Popen", return_value=FakeProc()), \
                mock.patch.object(m.time, "sleep", side_effect=(sleeps.append if sleeps is not None else (lambda seconds: None))):
            entry = m.run_app_case(rec, env, self.ev, self.work, app, facts, "clean")
        return entry, click, env

    def test_released_app_case_drives_settings_check_install_and_passes(self):
        entry, click, env = self.run_app()
        self.assertEqual(entry["verdict"], "PASS", json.dumps(entry["criteria_reasons"]))
        labels = [call.args[3] for call in click.call_args_list]
        self.assertEqual(labels, ["click Settings", "click Check for updates", "click Install (calls the plugin check())"])
        plugin = entry["trigger_outcome"]["plugin_check"]
        self.assertTrue(plugin["reached"])
        self.assertEqual(plugin["error"], "Could not fetch a valid release JSON from the remote")
        self.assertEqual(plugin["error_kind"], "ReleaseNotFound")
        self.assertEqual(entry["scenario"], "latest-404")
        self.assertEqual(entry["network"]["blocked_live_node_attempts"], 1)
        self.assertEqual(entry["network"]["violations"], [])
        for name in ("network-clean-released_app.json", "ui-clean-released_app.json", "ax-clean-released_app-1-launch.json", "ax-clean-released_app-4-after-install.json",
                     "requests-clean-released_app.jsonl", "writes-clean-released_app.jsonl", "procs-clean-released_app-seen.txt"):
            self.assertTrue((self.ev / name).exists(), name)
        network_file = (self.ev / "network-clean-released_app.json").read_text()
        self.assertNotIn(LIVE[0], network_file, "live node addresses are masked in the evidence")
        self.assertIn("NOT recorded", network_file)
        self.assertIn("140.82.112.5", network_file, "the pass-through flow is labelled with the address resolved at run time")
        self.assertIn(("latest-404", "requests-clean-released_app.jsonl"), env.served)

    def test_a_banner_without_a_tag_is_retried_three_times_20_seconds_apart_then_the_tier_is_infeasible_with_the_exact_ui_text(self):
        sleeps = []
        entry, click, env = self.run_app(install=False, pill="vUNKNOWN", sleeps=sleeps)
        attempts = entry["trigger_outcome"]["ui"]["banner_attempts"]
        self.assertEqual([a["attempt"] for a in attempts], [1, 2, 3, 4])
        self.assertTrue(all(not a["install_button"] for a in attempts))
        self.assertEqual(sleeps.count(20), 3, "20 s between the clicks")
        self.assertIn("vUNKNOWN", " ".join(attempts[-1]["card_texts"]), "the UI text after every click is recorded")
        reason = entry["trigger_outcome"]["plugin_check"]["reason"]
        self.assertIn("the banner's own unintercepted API call returned no release tag (UI: ", reason)
        self.assertIn("vUNKNOWN", reason)
        self.assertIn("not attributable from here", reason)
        self.assertFalse(entry["trigger_outcome"]["plugin_check"]["reached"], "never claim the plugin path was reached")
        self.assertEqual(entry["verdict"], "UNPROVED")
        self.assertFalse(any(call.args[3].startswith("click Install") for call in click.call_args_list))

    def test_an_install_button_that_shows_up_on_a_retry_is_clicked(self):
        entry, click, env = self.run_app(install_from_attempt=3, pill="v0.7.12")
        labels = [call.args[3] for call in click.call_args_list]
        self.assertEqual(labels, ["click Settings", "click Check for updates", "click Check for updates (retry 1)", "click Check for updates (retry 2)", "click Install (calls the plugin check())"])
        ui = entry["trigger_outcome"]["ui"]
        self.assertEqual([a["install_button"] for a in ui["banner_attempts"]], [False, False, True])
        self.assertTrue(entry["trigger_outcome"]["plugin_check"]["reached"])
        self.assertEqual(entry["verdict"], "PASS", json.dumps(entry["criteria_reasons"]))

    def test_an_install_button_present_before_the_check_is_not_clicked(self):
        entry, click, env = self.run_app(install_before_check=True, install_from_attempt=1)
        self.assertFalse(any(call.args[3].startswith("click Install") for call in click.call_args_list))
        ui = entry["trigger_outcome"]["ui"]
        self.assertTrue(ui["install_button_before_check"])
        self.assertIn("BEFORE the check", ui["problem"])
        self.assertFalse(entry["trigger_outcome"]["plugin_check"]["reached"])
        self.assertIn("already present before the check", entry["trigger_outcome"]["plugin_check"]["reason"])

    def test_the_rsms_addresses_are_blocked_before_the_app_starts_and_a_failed_block_prevents_the_launch(self):
        launched = []
        entry, click, env = self.run_app(env=FakeEnv(refresh={"added": ["104.21.99.9"], "verified": True}))
        self.assertEqual(entry["trigger_outcome"]["ui"]["extra_block_refresh"]["added"], ["104.21.99.9"])
        broken = FakeEnv(refresh={"added": ["104.21.99.9"], "verified": False})
        entry, click, env = self.run_app(env=broken)
        self.assertNotIn("pid", entry["trigger_outcome"]["ui"], "the app was not launched")
        self.assertIn("pf block could not be verified", entry["trigger_outcome"]["ui"]["problem"])
        self.assertEqual(entry["verdict"], "UNPROVED")
        self.assertEqual(click.call_count, 0)

    def test_attempts_to_a_blocked_rsms_address_are_information_but_an_established_connection_fails(self):
        syn = [{"pid": 555, "command": "com.apple.WebKit.Networking", "remote_ip": RSMS[0], "remote_port": 443, "state": "SYN_SENT", "t": 1.0}]
        entry, click, env = self.run_app(lsof_rows=syn)
        self.assertEqual(entry["network"]["violations"], [])
        self.assertEqual(entry["network"]["blocked_live_node_attempts"], 1)
        up = [dict(syn[0], state="ESTABLISHED")]
        entry, click, env = self.run_app(lsof_rows=up)
        self.assertEqual(entry["verdict"], "FAIL")
        self.assertIn("%s:443" % RSMS[0], json.dumps(entry["criteria_reasons"]))

    def test_a_rotated_api_address_seen_by_the_resolver_or_by_tcpdump_is_allowed(self):
        rows = [{"pid": 4242, "command": "ARC Node", "remote_ip": "140.82.112.6", "remote_port": 443, "state": "ESTABLISHED", "t": 1.0}]
        entry, click, env = self.run_app(lsof_rows=rows)
        self.assertEqual(entry["network"]["violations"], [], "140.82.112.6 was handed out by the resolver during the case")
        self.assertEqual(entry["verdict"], "PASS", json.dumps(entry["criteria_reasons"]))
        network = json.loads((self.ev / "network-clean-released_app.json").read_text())
        self.assertIn("140.82.112.6", network["banner_api_addresses"])
        self.assertIn("never hard-coded", network["banner_api_addresses_sources"])

    def test_released_app_case_fails_and_names_an_unexpected_destination(self):
        rows = [{"pid": 4242, "command": "ARC Node", "remote_ip": "140.82.112.5", "remote_port": 443, "state": "ESTABLISHED", "t": 1.0},
                {"pid": 555, "command": "com.apple.WebKit.Networking", "remote_ip": "198.51.100.7", "remote_port": 443, "state": "ESTABLISHED", "t": 1.0}]
        entry, click, env = self.run_app(lsof_rows=rows)
        self.assertEqual(entry["verdict"], "FAIL")
        self.assertIs(entry["criteria"]["only_manifest_url"], False)
        self.assertIn("198.51.100.7:443", json.dumps(entry["criteria_reasons"]))

    def test_without_the_install_button_the_case_is_a_labelled_negative_control(self):
        entry, click, env = self.run_app(install=False)
        self.assertEqual(entry["verdict"], "UNPROVED")
        plugin = entry["trigger_outcome"]["plugin_check"]
        self.assertFalse(plugin["reached"])
        self.assertIn("Install button was not rendered", plugin["reason"])
        self.assertEqual([call.args[3] for call in click.call_args_list],
                         ["click Settings", "click Check for updates", "click Check for updates (retry 1)", "click Check for updates (retry 2)", "click Check for updates (retry 3)"],
                         "the check is clicked again up to three times; nothing is clicked that is not there")

    def test_switch_off_means_no_network_record_is_required_and_the_banner_address_is_not_allowed(self):
        env = FakeEnv(real_banner=False)
        entry, click, _ = self.run_app(install=False, env=env, lsof_rows=[{"pid": 4242, "command": "ARC Node", "remote_ip": "140.82.112.5", "remote_port": 443, "state": "ESTABLISHED", "t": 1.0}])
        self.assertEqual(entry["verdict"], "FAIL", "with the switch off nothing may leave the sandbox")
        self.assertIn("140.82.112.5:443", json.dumps(entry["criteria_reasons"]))

    def test_second_case_starts_from_the_state_the_first_left(self):
        self.run_app()
        store = self.work / "app-shared" / "home" / "Library" / "Application Support" / "network.arc.desktop" / "store.json"
        self.assertTrue(store.exists())
        store.write_text(json.dumps({"marker": "left behind by the first case"}))
        class Stop(Exception):
            pass
        entry, click, env = self.run_app()
        self.assertIn("left behind", store.read_text(), "an existing store.json is not overwritten by the second case")


class EnsureTagTests(unittest.TestCase):
    def test_an_existing_tag_is_left_alone(self):
        with tempfile.TemporaryDirectory() as tmp:
            rec = FakeRecorder(tmp, [("rev-parse -q --verify", (0, "d60632af" + "0" * 32 + "\n"))])
            result = m.ensure_tag(rec)
            self.assertFalse(result["fetched"])
            self.assertFalse(any("fetch" in " ".join(call["argv"]) for call in rec.calls), "no fetch, so no history change")

    def test_depth_one_is_used_only_in_a_shallow_checkout(self):
        for shallow, expect_depth in (("true\n", True), ("false\n", False)):
            with self.subTest(shallow=shallow), tempfile.TemporaryDirectory() as tmp:
                rec = FakeRecorder(tmp, [("rev-parse -q --verify", (1, "")), ("--is-shallow-repository", (0, shallow))])
                result = m.ensure_tag(rec)
                fetch = next(call["argv"] for call in rec.calls if "fetch" in call["argv"])
                self.assertEqual("--depth=1" in fetch, expect_depth)
                self.assertIn("--no-tags", fetch)
                self.assertTrue(result["fetched"])


class CiHelperTests(unittest.TestCase):
    """The helpers that only run on the runner, driven by scripted command answers."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "ev").mkdir()

    def test_applescript_goes_to_osascript_one_dash_e_per_line(self):
        argv = m.osascript_argv("tell application \"Finder\"\n  activate\nend tell")
        self.assertEqual(argv, ["osascript", "-e", 'tell application "Finder"', "-e", "  activate", "-e", "end tell"])

    def accessibility(self, click_answer):
        answers = [
            ("get name of every process", (0, "Finder, Dock, SystemUIServer\n")),
            ("UI elements enabled", (0, "true\n")),
            ("New Finder Window", click_answer),
            ("sqlite3", (1, "", "Error: unable to open database file")),
        ]
        rec = FakeRecorder(self.root / "ev", answers)
        return m.accessibility_tests(rec, self.root / "ev")

    def test_accessibility_granted_is_proved_by_a_click_that_opened_a_window(self):
        result = self.accessibility((0, "0,1\n"))
        summary = result["summary"]
        self.assertTrue(summary["system_events_reachable"])
        self.assertTrue(summary["click_works"])
        self.assertEqual(summary["click_evidence"], "Finder windows 0 -> 1")
        self.assertEqual(summary["assistive_access"], "granted")
        self.assertEqual(result["tests"]["b_tcc_system_db"]["rc"], 1, "an unreadable TCC database is recorded, not hidden")
        self.assertIn("unable to open database file", result["tests"]["b_tcc_system_db"]["stderr"])
        saved = json.loads((self.root / "ev" / "accessibility.json").read_text())
        self.assertEqual(saved["summary"]["click_works"], True)
        self.assertEqual(set(saved["tests"]), {"a_system_events_process_list", "a2_ui_elements_enabled", "b_tcc_system_db", "b_tcc_user_db", "b_process_chain", "c_finder_menu_click"})

    def test_accessibility_denied_keeps_the_exact_error(self):
        error = "execution error: System Events got an error: osascript is not allowed assistive access. (-25211)"
        result = self.accessibility((1, "", error))
        summary = result["summary"]
        self.assertFalse(summary["click_works"])
        self.assertEqual(summary["assistive_access"], "denied")
        self.assertEqual(summary["click_error_kind"], "not_allowed_assistive")
        self.assertIn("-25211", summary["click_error"])
        self.assertIn("-25211", summary["first_error"])
        self.assertEqual(result["tests"]["c_finder_menu_click"]["classification"]["code"], -25211)

    def test_a_click_that_changes_nothing_is_not_a_working_click(self):
        self.assertFalse(self.accessibility((0, "1,1\n"))["summary"]["click_works"])
        self.assertFalse(self.accessibility((0, "garbage\n"))["summary"]["click_works"])

    def test_release_metadata_uses_gh_then_curl_and_insists_on_the_tag(self):
        rec = FakeRecorder(self.root / "ev", [("gh api", (4, "", "gh: To use GitHub CLI in a GitHub Actions workflow, set the GH_TOKEN environment variable")),
                                              ("curl", (0, json.dumps(release_json())))])
        self.assertEqual(m.fetch_release(rec)["tag_name"], "v0.7.11")
        self.assertTrue(any(call["argv"][0] == "curl" for call in rec.calls))
        wrong = FakeRecorder(self.root / "ev", [("gh api", (0, json.dumps({"tag_name": "v0.8.0"}))), ("curl", (0, "not json"))])
        with self.assertRaises(m.StepFailed):
            m.fetch_release(wrong)
        broken = FakeRecorder(self.root / "ev", [("gh api", (1, "", "x")), ("curl", (22, "", "x"))])
        with self.assertRaises(m.StepFailed):
            m.fetch_release(broken)

    def curl_writing(self, content, destinations=None):
        def answer(argv):
            target = Path(argv[argv.index("-o") + 1])
            target.write_bytes(content)
            if destinations is not None:
                destinations.append(target)
            return (0, "")
        return answer

    def test_a_download_is_used_only_when_its_sha256_equals_the_release_digest(self):
        import hashlib
        data = b"pretend dmg"
        asset = {"name": "ARC.Node_0.7.11_aarch64.dmg", "browser_download_url": "https://github.com/x/y.dmg", "digest": "sha256:" + hashlib.sha256(data).hexdigest()}
        rec = FakeRecorder(self.root / "ev", [("curl -fL", self.curl_writing(data))])
        path, info = m.download_verified(rec, asset, self.root / "dl")
        self.assertTrue(info["digest_match"])
        self.assertEqual(path.read_bytes(), data)
        curl = next(call["argv"] for call in rec.calls if call["argv"][0] == "curl")
        self.assertIn("--proto", curl)
        self.assertIn("=https", curl)
        self.assertIn("--tlsv1.2", curl)
        bad = dict(asset, digest="sha256:" + "0" * 64)
        with self.assertRaises(m.StepFailed):
            m.download_verified(FakeRecorder(self.root / "ev", [("curl -fL", self.curl_writing(data))]), bad, self.root / "dl2")
        with self.assertRaises(m.StepFailed):
            m.download_verified(FakeRecorder(self.root / "ev", [("curl -fL", (22, "", "404"))]), asset, self.root / "dl3")

    def test_the_app_is_taken_from_the_tar_when_the_dmg_cannot_be_mounted(self):
        import hashlib
        dmg, tgz = b"dmg bytes", b"tgz bytes"
        assets = {"arch": "aarch64",
                  "dmg": {"name": "a.dmg", "browser_download_url": "https://github.com/a.dmg", "digest": "sha256:" + hashlib.sha256(dmg).hexdigest()},
                  "tar": {"name": "a.app.tar.gz", "browser_download_url": "https://github.com/a.tgz", "digest": "sha256:" + hashlib.sha256(tgz).hexdigest()}}
        sandbox = self.root / "sandbox"

        def curl(argv):
            target = Path(argv[argv.index("-o") + 1])
            target.write_bytes(dmg if target.name.endswith(".dmg") else tgz)
            return (0, "")

        def untar(argv):
            (sandbox / "apps" / "ARC Node.app").mkdir(parents=True, exist_ok=True)
            return (0, "")

        rec = FakeRecorder(self.root / "ev", [("curl -fL", curl), ("hdiutil attach", (1, "", "hdiutil: attach failed - no mountable file systems")), ("tar -xzf", untar)])
        app, info = m.extract_app(rec, assets, self.root / "dl", sandbox)
        self.assertEqual(app.name, "ARC Node.app")
        self.assertEqual(info["source"], "app.tar.gz")
        self.assertTrue(any("hdiutil attach" in " ".join(call["argv"]) for call in rec.calls))
        self.assertFalse(any("detach" in " ".join(call["argv"]) for call in rec.calls), "nothing was mounted, so nothing is unmounted")

    def test_the_dmg_is_mounted_read_only_without_browsing_copied_and_always_unmounted(self):
        import hashlib
        dmg = b"dmg bytes"
        assets = {"arch": "x64", "dmg": {"name": "a.dmg", "browser_download_url": "https://github.com/a.dmg", "digest": "sha256:" + hashlib.sha256(dmg).hexdigest()}, "tar": None}
        sandbox = self.root / "sandbox"

        def attach(argv):
            mount = Path(argv[argv.index("-mountpoint") + 1])
            (mount / "ARC Node.app").mkdir(parents=True, exist_ok=True)
            return (0, "/dev/disk9s1 Apple_HFS")

        def ditto(argv):
            Path(argv[-1]).mkdir(parents=True, exist_ok=True)
            return (0, "")

        rec = FakeRecorder(self.root / "ev", [("curl -fL", self.curl_writing(dmg)), ("hdiutil attach", attach), ("ditto", ditto)])
        app, info = m.extract_app(rec, assets, self.root / "dl", sandbox)
        self.assertEqual(info["source"], "dmg")
        attach_call = next(call["argv"] for call in rec.calls if call["argv"][:2] == ["hdiutil", "attach"])
        for flag in ("-nobrowse", "-readonly", "-noverify", "-noautoopen", "-mountpoint"):
            self.assertIn(flag, attach_call)
        self.assertEqual(next(call["input"] for call in rec.calls if call["argv"][:2] == ["hdiutil", "attach"]), "Y\n", "a license agreement in the image is answered")
        self.assertTrue(any(call["argv"][:2] == ["hdiutil", "detach"] for call in rec.calls))

    def test_provenance_reads_crate_versions_from_the_strings_of_the_binary(self):
        binary = self.root / "arc-desktop"
        binary.write_bytes(b"x")
        strings = "\n".join([
            "/Users/runner/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tauri-plugin-updater-2.10.1/src/updater.rs",
            "/registry/src/tauri-2.11.2/src/lib.rs", "/registry/src/reqwest-0.13.4/src/lib.rs", "/registry/src/reqwest-0.12.28/src/lib.rs",
            "/registry/src/rustls-platform-verifier-0.6.2/src/lib.rs", "/registry/src/webpki-roots-1.0.2/src/lib.rs", "unrelated-1.2.3",
        ])
        rec = FakeRecorder(self.root / "ev", [("strings -a", (0, strings))])
        provenance = m.collect_provenance(rec, binary, self.root / "ev")
        self.assertTrue(provenance["plugin_pinned"])
        self.assertTrue(provenance["bundled_webpki_roots_present"])
        for item in ("tauri-plugin-updater-2.10.1", "tauri-2.11.2", "reqwest-0.12.28", "reqwest-0.13.4", "rustls-platform-verifier-0.6.2", "webpki-roots-1.0.2"):
            self.assertIn(item, provenance["crates_in_binary"])
        self.assertNotIn("unrelated-1.2.3", provenance["crates_in_binary"])
        self.assertTrue((self.root / "ev" / "provenance.json").exists())
        other = FakeRecorder(self.root / "ev", [("strings -a", (0, "tauri-plugin-updater-2.9.0/src/x.rs"))])
        self.assertFalse(m.collect_provenance(other, binary, self.root / "ev")["plugin_pinned"])

    def test_system_facts_records_every_probe_command(self):
        rec = FakeRecorder(self.root / "ev", [("sudo -n true", (0, ""))])
        facts = m.system_facts(rec)
        for key in ("sw_vers", "whoami", "console_user", "launchd_session", "window_server", "sudo", "openssl", "cargo", "gh"):
            self.assertIn(key, facts)
        self.assertEqual(facts["sudo"]["rc"], 0)

    def test_tcpdump_start_falls_back_from_pktap_to_any_and_reports_failures(self):
        class Proc:
            def __init__(self, alive):
                self.alive = alive

            def poll(self):
                return None if self.alive else 1

            def wait(self, timeout=None):
                return 0

        started = []

        def popen(argv, **kwargs):
            started.append(argv)
            if "pktap,all" in argv:
                kwargs["stdout"].write("tcpdump: pktap,all: No such device exists\n")
                kwargs["stdout"].flush()
                return Proc(False)
            return Proc(True)

        rec = FakeRecorder(self.root / "ev")
        watcher = m.CaptureWatcher(rec, self.root / "dump.txt")
        with mock.patch.object(m.subprocess, "Popen", side_effect=popen), mock.patch.object(m.time, "sleep"):
            self.assertTrue(watcher.start())
        self.assertEqual(watcher.iface, "any")
        self.assertIn("-k", started[0])
        self.assertEqual(started[0][:5], ["sudo", "-n", "tcpdump", "-i", "pktap,all"])
        self.assertNotIn("-k", started[1])
        text = watcher.stop()
        self.assertTrue(any(call["argv"][:4] == ["sudo", "-n", "pkill", "-INT"] for call in rec.calls))
        self.assertIsInstance(text, str)
        refuse = m.CaptureWatcher(FakeRecorder(self.root / "ev"), self.root / "dump2.txt")
        with mock.patch.object(m.subprocess, "Popen", side_effect=lambda argv, **kw: Proc(False)), mock.patch.object(m.time, "sleep"):
            self.assertFalse(refuse.start())
        self.assertIsNone(refuse.proc)

    def test_lsof_watcher_collects_endpoints_and_survives_errors(self):
        watcher = m.LsofWatcher()
        text = "p4242\ncARC Node\nf20\ntIPv4\nn10.0.0.2:51000->140.82.112.5:443\nTST=ESTABLISHED\n"
        with mock.patch.object(m.subprocess, "run", return_value=subprocess.CompletedProcess(["lsof"], 0, text.encode(), b"")):
            watcher.poll_once()
        self.assertEqual(len(watcher.rows), 1)
        self.assertEqual(watcher.rows[0]["remote_ip"], "140.82.112.5")
        self.assertEqual(watcher.polls, 1)
        with mock.patch.object(m.subprocess, "run", side_effect=FileNotFoundError("lsof")):
            watcher.poll_once()
        self.assertEqual(watcher.polls, 2)
        self.assertTrue(watcher.errors)

    def test_process_watcher_keeps_every_process_it_ever_saw(self):
        outputs = iter(["1 0 /sbin/launchd\n50 1 /x/app\n", "1 0 /sbin/launchd\n50 1 /x/app\n51 50 /usr/bin/hdiutil\n", "1 0 /sbin/launchd\n"])

        def run(argv, **kwargs):
            return subprocess.CompletedProcess(argv, 0, next(outputs, "").encode(), b"")

        watcher = m.ProcessWatcher(FakeRecorder(self.root / "ev"), interval=0.01)
        with mock.patch.object(m.subprocess, "run", side_effect=run):
            for _ in range(3):
                for row in watcher.snapshot_rows():
                    watcher.seen.setdefault((row["pid"], row["ppid"], row["comm"]), 1.0)
        seen = watcher.stop()
        self.assertEqual({row["comm"] for row in seen}, {"/sbin/launchd", "/x/app", "/usr/bin/hdiutil"}, "a process that came and went stays in the record")


class EvidenceHygieneTests(unittest.TestCase):
    def test_nothing_in_the_script_uploads_or_prints_a_private_key(self):
        source = (_paths.LAB / "os_macos.py").read_text(encoding="utf-8")
        self.assertNotIn("copy_public", source, "only ca.crt and ca.sha256 are written into the evidence, by name")
        for line in source.splitlines():
            if "private" in line and ("evidence" in line and "/" in line):
                self.assertNotIn("private/", line, line)
        self.assertIn('shutil.copyfile(str(self.ca["ca_cert"]), str(self.evidence / "ca.crt"))', source)
        for number, line in enumerate(source.splitlines(), 1):
            if "ca.key" in line or "server.key" in line:
                self.assertNotIn("evidence", line, "line %d: a private key file is named next to the evidence directory" % number)
                self.assertNotIn("copyfile", line, "line %d: a private key file is copied" % number)

    def test_python_39_syntax(self):
        compile((_paths.LAB / "os_macos.py").read_text(encoding="utf-8"), "os_macos.py", "exec")


if __name__ == "__main__":
    unittest.main()
