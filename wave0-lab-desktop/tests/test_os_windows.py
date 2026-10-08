"""Offline tests for os_windows.py, the Windows job of the desktop updater isolation lab (THROWAWAY LAB FILE).

Nothing here needs Windows: the machine-touching steps are replaced by fakes, the pure logic (criteria, classification, release
asset selection, the WebSocket / CDP client against a fake server, result assembly) is exercised directly, and the result
document is fed to stage_c_summary.evaluate_os_result so the two files cannot drift apart."""
from __future__ import annotations

import base64
import hashlib
import json
import os
import socket
import struct
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

import _paths  # noqa: F401
import os_windows as ow
import stage_c_summary as ssum

# The asset list of the real v0.7.11 release (gh api repos/FerrumVir/arc-chain/releases/tags/v0.7.11, 2026-10-08), trimmed to names/sizes/digests.
_BEFORE = {}


def _top_level_entries():
    """Top-level entries of the places a stray Windows-style relative path would land (the repo root, the lab directory, the tests directory, the cwd)."""
    found = {}
    for label, directory in (("repository root", _paths.ROOT), ("lab directory", _paths.LAB), ("tests directory", _paths.TESTS), ("working directory", Path.cwd())):
        try:
            found[label] = set(os.listdir(str(directory)))
        except OSError:
            found[label] = set()
    return found


def setUpModule():
    _BEFORE.update(_top_level_entries())


def tearDownModule():
    """Nothing in this module may create files outside a temporary directory: a Windows path used as a POSIX relative path (C:\\arcw0) would show up here."""
    after = _top_level_entries()
    stray = {}
    for label, names in after.items():
        new = sorted(name for name in names - _BEFORE.get(label, set()) if not name.endswith((".py", ".pyc")) and name != "__pycache__")
        if new:
            stray[label] = new
    if stray:
        raise AssertionError("the tests created entries outside temporary directories: %s" % stray)


REAL_ASSETS = [
    ("ARC.Node-0.7.11-1.x86_64.rpm", 9169784, "f800b218bebbd9b13f5f669f4e1c2dafe82d15867ade000d871065b4c7607ddc"),
    ("ARC.Node_0.7.11_aarch64.dmg", 7515660, "49086bc53aacb1c22146554496832cd2421d761a014070ce787de8bf604b4a42"),
    ("ARC.Node_0.7.11_amd64.AppImage", 86108664, "9849f5dba4b9f90be8a0258c23a8c94ccbad0010e5ad8507a1ef2233531ce81b"),
    ("ARC.Node_0.7.11_amd64.deb", 9168422, "db0df355bb17f23a02b9323adcd7506053a8969ee3d98a23cfe92d1b4219a9c3"),
    ("ARC.Node_0.7.11_x64-setup.exe", 4864230, "61a01c0f0253d040110fc9bc14058afdff864d6b9c316115acf8c58ae45d1e15"),
    ("ARC.Node_0.7.11_x64-setup.exe.sig", 420, "eeb928603f38a15ba1c5712c7c5cae13e6c3fe21f7377ab3d46a922ea241cfc5"),
    ("ARC.Node_0.7.11_x64.dmg", 7955313, "78051bfd5a69101475b609e4e3ae6aeb3206a40fe8bedeee493922508e43d26a"),
    ("ARC.Node_0.7.11_x64_en-US.msi", 7241728, "7d9811da71f1fd1e6ba208a2614684ac6d9251c37bfe651e736f4096e8a33979"),
    ("ARC.Node_0.7.11_x64_en-US.msi.sig", 420, "93f9ccebc144b45d6536f8f0be75c237377ac114b2c299616322e5b4d9c57c29"),
    ("latest.json", 2418, "0596969b4fc4622c01989af8e0483f4dc1a650968040df0d863f7485977c12d4"),
]
NSIS_SHA = "61a01c0f0253d040110fc9bc14058afdff864d6b9c316115acf8c58ae45d1e15"
APP_EXE = "C:\\arcw0\\app\\arc-desktop.exe"


def release_json(assets=None):
    return {"tag_name": "v0.7.11", "assets": [{"name": n, "size": s, "digest": "sha256:" + d,
                                                  "browser_download_url": "https://github.com/FerrumVir/arc-chain/releases/download/v0.7.11/" + n}
                                                 for n, s, d in (assets if assets is not None else REAL_ASSETS)]}


def rec(path, host="github.com", kind="request", **extra):
    base = {"t": 1.0, "kind": kind, "host": host, "sni": host, "method": "GET", "path": path, "status": 200}
    base.update(extra)
    return base


MANIFEST = rec(ow.MANIFEST_PATH)
REDIRECT = rec(ow.REDIRECT_TARGET_PATH, status=404)
TRIGGER_404 = {"ran": True, "ok": False, "error_text": ow.RELEASE_NOT_FOUND, "stage": "invoke", "value": None}
TRIGGER_BAIT = {"ran": True, "ok": True, "error_text": None, "stage": "invoke", "value": {"rid": 1, "currentVersion": "0.7.11", "version": "0.8.11"}}
NO_FS = {"expected": ["x"], "noise": [], "unexpected": []}
NO_PROCS = {"new_app_launches": [], "installers": [], "other_in_app_tree": [], "unrelated": []}


def requests_for(*records):
    return ow.summarize_requests(list(records))


class HelperTests(unittest.TestCase):
    def test_mask_ips_keeps_loopback_and_masks_everything_else(self):
        text = "a 149.28.32.76:9090 b 127.0.0.1 c 0.0.0.0 d 192.0.2.123 e 10.0.20348 f 140.82.16.112"
        self.assertEqual(ow.mask_ips(text), "a 149.28.x.x:9090 b 127.0.0.1 c 0.0.0.0 d 192.0.2.123 e 10.0.20348 f 140.82.x.x")

    def test_thumbprint_is_the_upper_case_sha1_of_the_der(self):
        pem = "-----BEGIN CERTIFICATE-----\n" + base64.b64encode(b"abc").decode() + "\n-----END CERTIFICATE-----\n"
        self.assertEqual(ow.thumbprint_sha1(pem), "A9993E364706816ABA3E25717850C26C9CD0D89D")
        with self.assertRaises(ValueError):
            ow.pem_der("not a certificate")

    def test_read_jsonl_tolerates_a_torn_last_line_and_a_missing_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "x.jsonl"
            self.assertIsNone(ow.read_jsonl(path))
            path.write_text('{"a": 1}\n\n[1]\n{"b": 2}\n{"c": ', encoding="utf-8")
            self.assertEqual(ow.read_jsonl(path), [{"a": 1}, {"b": 2}])

    def test_path_comparison_is_windows_style(self):
        self.assertTrue(ow.path_under("C:\\ARCW0\\app\\x.exe", "c:\\arcw0\\app"))
        self.assertTrue(ow.path_under("c:/arcw0/app", "C:\\arcw0\\app\\"))
        self.assertFalse(ow.path_under("C:\\arcw0\\apple\\x", "C:\\arcw0\\app"))

    def test_forbidden_patterns_equal_the_summarys(self):
        mine = [(pattern.pattern, label) for pattern, label in ow.FORBIDDEN_PATH_PATTERNS]
        theirs = [(pattern.pattern, label) for pattern, label in ssum.FORBIDDEN_PATH_PATTERNS]
        self.assertEqual(mine, theirs)

    def test_constants_match_the_summarys(self):
        self.assertEqual(ow.MANIFEST_PATH, ssum.MANIFEST_PATHS[0])
        self.assertEqual(ow.RELEASE_NOT_FOUND, ssum.RELEASE_NOT_FOUND)
        self.assertEqual(ow.PLUGIN_VERSION, ssum.PLUGIN_VERSION)
        self.assertEqual(ow.TAG, ssum.TAG)
        self.assertEqual(tuple(ow.CASE_NAMES), tuple(ssum.CASE_NAMES))
        self.assertEqual(tuple(ow.TIERS), tuple(ssum.TIER_NAMES))
        self.assertEqual(tuple(ow.CRITERIA), tuple(ssum.REQUIRED_CRITERIA))
        self.assertEqual(ow.SCHEMA_RESULT, ssum.OS_SCHEMA)


class ReleaseAssetTests(unittest.TestCase):
    def test_the_real_asset_list_selects_the_nsis_installer(self):
        asset = ow.select_installer(release_json())
        self.assertEqual(asset["name"], "ARC.Node_0.7.11_x64-setup.exe")
        self.assertEqual(asset["kind"], "nsis")
        self.assertEqual(asset["digest"], "sha256:" + NSIS_SHA)
        self.assertEqual(asset["size"], 4864230)
        self.assertTrue(asset["url"].endswith("/releases/download/v0.7.11/ARC.Node_0.7.11_x64-setup.exe"))

    def test_msi_is_the_fallback_and_signatures_are_never_selected(self):
        assets = [a for a in REAL_ASSETS if "setup.exe" not in a[0]]
        asset = ow.select_installer(release_json(assets))
        self.assertEqual((asset["name"], asset["kind"]), ("ARC.Node_0.7.11_x64_en-US.msi", "msi"))
        only_sigs = [a for a in REAL_ASSETS if a[0].endswith(".sig")]
        with self.assertRaises(ow.AssetError):
            ow.select_installer(release_json(only_sigs))

    def test_missing_digest_duplicates_and_non_lists_are_refused(self):
        broken = release_json()
        broken["assets"][4].pop("digest")
        with self.assertRaises(ow.AssetError):
            ow.select_installer(broken)
        with self.assertRaises(ow.AssetError):
            ow.select_installer(release_json(REAL_ASSETS + [REAL_ASSETS[4]]))
        with self.assertRaises(ow.AssetError):
            ow.select_installer({"assets": None})

    def test_pins_are_found_anywhere_in_the_config_and_compared(self):
        config = {"app": {"assets": {"windows": [{"name": "ARC.Node_0.7.11_x64-setup.exe", "digest": "sha256:" + NSIS_SHA, "size": 4864230}]}}}
        pin = ow.find_pin(config, "ARC.Node_0.7.11_x64-setup.exe")
        self.assertEqual(ow.pin_digest(pin), NSIS_SHA)
        asset = ow.select_installer(release_json())
        self.assertEqual(ow.digest_problems(asset, NSIS_SHA, pin), [])
        self.assertTrue(ow.digest_problems(asset, "0" * 64, pin))
        wrong_pin = {"name": "x", "sha256": "1" * 64}
        self.assertTrue(any("pinned digest" in p for p in ow.digest_problems(asset, NSIS_SHA, wrong_pin)))
        self.assertIsNone(ow.find_pin(config, "nope"))
        self.assertEqual(ow.digest_problems(asset, NSIS_SHA, None), [])

    def test_tauri_conf_and_pubkey_extraction(self):
        conf = json.dumps({"version": "0.7.11", "productName": "ARC Node", "identifier": "network.arc.desktop",
                           "plugins": {"updater": {"pubkey": ow.PUBKEY_PREFIX + "AAAA" * 20, "endpoints": [ow.MANIFEST_URL]}}})
        parsed = ow.parse_tauri_conf(conf)
        self.assertEqual(parsed["endpoints"], [ow.MANIFEST_URL])
        self.assertEqual(parsed["version"], "0.7.11")
        data = b"\x00junk" + parsed["pubkey"].encode() + b"\x00more"
        self.assertEqual(ow.extract_pubkey(data), parsed["pubkey"])
        self.assertIsNone(ow.extract_pubkey(b"nothing here"))

    def test_provenance_reads_crate_versions_from_binary_strings(self):
        pubkey = ow.PUBKEY_PREFIX + "B" * 60
        data = (b"\x00tauri-plugin-updater-2.10.1\x00tauri-2.11.2\x00tauri-utils-2.9.2\x00minisign-verify-0.2.5\x00" + pubkey.encode() +
                b"\x00" + ow.MANIFEST_URL.encode() + b"\x00xtauri-plugin-updater-9.9.9")
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "arc-desktop.exe"
            path.write_bytes(data)
            info = ow.provenance_of_binary(path, pubkey)
        self.assertEqual(info["plugin_version_from_binary"], ["2.10.1"], "a version glued to other letters is not a crate string")
        self.assertTrue(all(info["matches_pin"].values()))
        self.assertTrue(info["pubkey_matches_conf"])
        self.assertEqual(info["updater_endpoint_string_count"], 1)
        self.assertEqual(info["binary_sha256"], hashlib.sha256(data).hexdigest())

    def test_a_different_plugin_version_is_reported_not_hidden(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "a.exe"
            path.write_bytes(b"tauri-plugin-updater-2.9.0 tauri-plugin-updater-2.10.1")
            info = ow.provenance_of_binary(path, None)
        self.assertEqual(info["plugin_version_from_binary"], ["2.10.1", "2.9.0"])
        self.assertFalse(info["matches_pin"]["tauri-plugin-updater"])
        self.assertFalse(info["pubkey_matches_conf"])


class IsolationScriptTests(unittest.TestCase):
    def test_hosts_block_maps_both_families_and_is_idempotent(self):
        block = ow.hosts_block(["github.com", "api.github.com"], ["rsms.me"])
        self.assertIn("127.0.0.1 github.com\r\n::1 github.com\r\n", block)
        self.assertIn("::1 rsms.me\r\n", block)
        original = "127.0.0.1 localhost\r\n# keep me\r\n"
        once = ow.hosts_with_block(original, block)
        twice = ow.hosts_with_block(once, block)
        self.assertEqual(once, twice)
        self.assertEqual(ow.strip_hosts_block(once), original)
        self.assertTrue(once.startswith(original))

    def test_strip_removes_only_the_marked_block(self):
        text = "a\r\n" + ow.HOSTS_BEGIN + "\r\n127.0.0.1 x\r\n" + ow.HOSTS_END + "\r\nb\r\n"
        self.assertEqual(ow.strip_hosts_block(text), "a\r\nb\r\n")

    def test_apply_and_restore_the_hosts_file_byte_for_byte(self):
        with tempfile.TemporaryDirectory() as tmp:
            hosts = Path(tmp) / "hosts"
            original = b"127.0.0.1 localhost\r\n# comment\r\n"
            hosts.write_bytes(original)
            ctx = ow.Context(Path(tmp) / "ev", Path(tmp) / "w", ow.Shell(), {})
            with mock.patch.object(ow, "hosts_path", return_value=hosts), mock.patch.object(ow.Shell, "run", return_value=ow.CmdResult(0, "")):
                with mock.patch.object(ow, "blackhole_names", return_value=["arc.ai"]):
                    ow.map_hosts(ctx, ["github.com"])
                mapped = hosts.read_bytes()
                self.assertIn(b"127.0.0.1 github.com\r\n", mapped)
                self.assertIn(b"::1 arc.ai\r\n", mapped)
                self.assertTrue(mapped.startswith(original))
                ow.cleanup(ctx)
            self.assertEqual(hosts.read_bytes(), original)

    def test_firewall_scripts(self):
        script = ow.firewall_block_script(["149.28.32.76", "140.82.16.112"])
        self.assertIn("New-NetFirewallRule -DisplayName 'arc-live-network-block' -Direction Outbound -Action Block -RemoteAddress '149.28.32.76','140.82.16.112'", script)
        self.assertIn("Set-NetFirewallProfile -All -Enabled True", script)
        with self.assertRaises(ValueError):
            ow.firewall_block_script([])
        verify = ow.firewall_verify_script("149.28.32.76")
        self.assertIn("ConnectAsync('149.28.32.76', 443).Wait(5000)", verify)
        self.assertIn("exit 3", verify)
        self.assertNotIn('"', script + verify, "no double quotes: the script travels as one command-line argument")
        self.assertIn("Remove-NetFirewallRule -DisplayName 'arc-live-network-block'", ow.firewall_remove_script())
        self.assertEqual(ow.powershell_argv("x")[:4], ["powershell", "-NoProfile", "-NonInteractive", "-ExecutionPolicy"])

    def test_certutil_and_store_check(self):
        self.assertEqual(ow.certutil_add_argv("C:\\x\\ca.crt"), ["certutil", "-addstore", "-f", "Root", "C:\\x\\ca.crt"])
        self.assertEqual(ow.certutil_del_argv("AB" * 20), ["certutil", "-delstore", "Root", "AB" * 20])
        self.assertIn("Cert:\\LocalMachine\\Root", ow.root_store_check_script("AB" * 20))
        with self.assertRaises(ValueError):
            ow.root_store_check_script("not-a-thumbprint'; Remove-Item *")

    def test_live_ips_come_from_the_repository_and_must_agree(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / ".github/workflows").mkdir(parents=True)
            (root / "tests/legacy-bridge").mkdir(parents=True)
            (root / ".github/workflows/legacy-bridge.yml").write_text("env:\n  LIVE_NETWORK_IPS: 203.0.113.1 203.0.113.2\n", encoding="utf-8")
            (root / "tests/legacy-bridge/headless-v07-acceptance.sh").write_text("live_ips=(203.0.113.1 203.0.113.2)\n", encoding="utf-8")
            self.assertEqual(ow.load_live_ips(root), ["203.0.113.1", "203.0.113.2"])
            (root / "tests/legacy-bridge/headless-v07-acceptance.sh").write_text("live_ips=(203.0.113.1)\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                ow.load_live_ips(root)
            (root / "tests/legacy-bridge/headless-v07-acceptance.sh").write_text("live_ips=(203.0.113.1 203.0.113.1)\n", encoding="utf-8")
            (root / ".github/workflows/legacy-bridge.yml").write_text("LIVE_NETWORK_IPS: 203.0.113.1 203.0.113.1\n", encoding="utf-8")
            with self.assertRaises(ValueError):
                ow.load_live_ips(root)

    def test_the_repositorys_own_list_loads(self):
        ips = ow.load_live_ips(_paths.ROOT)
        self.assertGreaterEqual(len(ips), 6)
        self.assertEqual(len(set(ips)), len(ips))

    def test_installer_command_lines(self):
        argv = ow.nsis_install_argv("C:\\arcw0\\dl\\ARC.Node_0.7.11_x64-setup.exe", "C:\\arcw0\\app")
        self.assertEqual(argv[1:], ["/S", "/D=C:\\arcw0\\app"])
        self.assertEqual(argv[-1][:3], "/D=", "NSIS requires /D to be the last argument")
        msi = ow.msi_install_argv("a.msi", "C:\\arcw0\\app", "log.txt")
        self.assertEqual(msi[:3], ["msiexec", "/i", "a.msi"])
        self.assertIn("INSTALLDIR=C:\\arcw0\\app", msi)


class RequestTests(unittest.TestCase):
    def test_classification(self):
        cases = [
            ("github.com", ow.MANIFEST_PATH, False, "manifest"),
            ("github.com", ow.MANIFEST_PATH + "?x=1", False, "manifest"),
            ("github.com", ow.REDIRECT_TARGET_PATH, False, "manifest-redirect"),
            ("api.github.com", ow.API_LATEST_PATH, False, "api-latest"),
            ("github.com", "/FerrumVir/arc-chain/releases/download/v0.8.11/latest.json", False, "payload"),
            ("github.com", "/FerrumVir/arc-chain/releases/download/v0.8.11/ARC.Node_0.8.11_x64-setup.exe", False, "payload"),
            ("github.com", "/FerrumVir/arc-chain/releases/download/v0.7.12/arc-node-linux-x86_64", False, "other"),
            ("github.com", "/FerrumVir/arc-chain/releases/download/v0.7.11/latest.json", False, "payload"),
            ("github.com", "/x/y/other/latest.json", False, "other-latest-json"),
            ("github.com", "/FerrumVir/arc-chain/releases/download/v0.7.12/latest.json.sig", False, "payload"),
            ("github.com", "/anything", True, "payload"),
            ("evil.example", ow.MANIFEST_PATH, False, "other-latest-json"),
            ("github.com", "/x", False, "other"),
        ]
        for host, path, flagged, expected in cases:
            with self.subTest(host=host, path=path):
                self.assertEqual(ow.classify_request(host, path, flagged), expected)

    def test_summary_counts_hosts_without_ports_and_keeps_tls_failures_apart(self):
        records = [MANIFEST, rec(ow.MANIFEST_PATH, host="GitHub.com:443"), REDIRECT,
                   {"kind": "tls_failure", "sni": "api.github.com", "t": 2.0, "error": "handshake"},
                   {"kind": "tls_failure", "sni": "api.github.com", "t": 3.0}]
        summary = ow.summarize_requests(records)
        self.assertTrue(summary["present"])
        self.assertEqual(summary["total"], 3)
        self.assertEqual(summary["by_host_path"], [["github.com", ow.REDIRECT_TARGET_PATH, 1], ["github.com", ow.MANIFEST_PATH, 2]])
        self.assertEqual(sum(row[2] for row in summary["by_host_path"]), summary["total"])
        self.assertEqual(summary["tls_failures"], [{"sni": "api.github.com", "count": 2}])
        self.assertEqual(sorted(summary["classes"]), ["manifest", "manifest-redirect"])

    def test_no_log_is_not_an_empty_log(self):
        self.assertFalse(ow.summarize_requests(None)["present"])
        self.assertTrue(ow.summarize_requests([])["present"])
        self.assertEqual(ow.summarize_requests([])["total"], 0)


class FileTests(unittest.TestCase):
    def test_snapshot_and_diff(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "a.txt").write_text("one", encoding="utf-8")
            (root / "sub").mkdir()
            (root / "sub" / "b.txt").write_text("two", encoding="utf-8")
            before = ow.take_snapshot([(str(root), True)])
            self.assertEqual(before[str(root / "a.txt")]["sha256"], hashlib.sha256(b"one").hexdigest())
            (root / "a.txt").write_text("ONE!", encoding="utf-8")
            (root / "c.exe").write_text("new", encoding="utf-8")
            (root / "sub" / "b.txt").unlink()
            after = ow.take_snapshot([(str(root), True)])
            delta = ow.diff_snapshots(before, after)
            self.assertEqual(delta["added"], [str(root / "c.exe")])
            self.assertEqual(delta["removed"], [str(root / "sub" / "b.txt")])
            self.assertEqual(delta["changed"], [str(root / "a.txt")])
            self.assertEqual(ow.take_snapshot([(str(root / "missing"), True)]), {})

    def test_poller_records_new_files_marks_and_cycles(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "watched"
            root.mkdir()
            log = Path(tmp) / "writes.jsonl"
            poller = ow.Poller([(str(root), False)], log, interval=0.05)
            poller.start()
            time.sleep(0.15)
            poller.mark("trigger_begin")
            (root / "dropped.bin").write_bytes(b"x")
            deadline = time.time() + 10
            while time.time() < deadline and not any(r.get("event") == "new" for r in (ow.read_jsonl(log) or [])):
                time.sleep(0.05)
            while time.time() < deadline and poller.cycles < 3:
                time.sleep(0.05)
            cycles = poller.stop()
            records = ow.read_jsonl(log)
        self.assertGreaterEqual(cycles, 3)
        events = [r["event"] for r in records]
        self.assertEqual(events[0], "start")
        self.assertEqual(events[-1], "stop")
        self.assertIn("mark", events)
        new = [r for r in records if r["event"] == "new"]
        self.assertEqual([os.path.basename(r["path"]) for r in new], ["dropped.bin"])
        self.assertEqual(records[-1]["cycles"], cycles)

    def policies(self):
        env = {"USERPROFILE": "C:\\Users\\runneradmin", "APPDATA": "C:\\Users\\runneradmin\\AppData\\Roaming",
               "LOCALAPPDATA": "C:\\Users\\runneradmin\\AppData\\Local", "TEMP": "C:\\Users\\runneradmin\\AppData\\Local\\Temp"}
        return ow.fs_policies("C:\\arcw0\\home", "C:\\arcw0\\wv2", "C:\\arcw0\\app", env)

    def test_policies_cover_state_install_profile_and_temp(self):
        by_root = {item["root"]: item["policy"] for item in self.policies()}
        self.assertEqual(by_root["C:\\arcw0\\home"], "app-state")
        self.assertEqual(by_root["C:\\arcw0\\wv2"], "browser-profile")
        self.assertEqual(by_root["C:\\arcw0\\app"], "frozen")
        self.assertEqual(by_root["C:\\Users\\runneradmin\\AppData\\Local\\Temp"], "update-artifacts-only")
        self.assertEqual(by_root["C:\\Users\\runneradmin\\.arc"], "frozen")
        self.assertEqual(by_root["C:\\Users\\runneradmin\\AppData\\Roaming\\network.arc.desktop"], "app-state")

    def test_roots_nested_in_another_watched_root_are_not_walked_twice(self):
        roots = [root for root, _hash in ow.fs_watch_roots(self.policies())]
        self.assertNotIn("C:\\Users\\runneradmin\\AppData\\Roaming\\network.arc.desktop", roots)
        self.assertIn("C:\\Users\\runneradmin\\AppData\\Roaming", roots)
        self.assertEqual(len({ow.norm_path(r) for r in roots}), len(roots))

    def test_classification(self):
        delta = {
            "added": [
                "C:\\arcw0\\home\\.arc\\settings.json",                                  # app state: expected
                "C:\\arcw0\\wv2\\EBWebView\\Default\\Cache\\f_000001",                    # browser profile: expected
                "C:\\Users\\runneradmin\\AppData\\Local\\Temp\\tmp1234.tmp",             # temp churn: noise
                "C:\\Users\\runneradmin\\AppData\\Local\\Temp\\ARC Node-0.8.11-updater-AbC\\ARC.Node_0.8.11_x64-setup.exe",   # update artifact
                "C:\\arcw0\\app\\arc-desktop.exe.new",                                   # frozen install dir
                "C:\\Users\\runneradmin\\Downloads\\x.msi",                              # frozen profile folder
                "C:\\arcw0\\home\\.arc\\bin\\arc-node.exe",                              # payload-like name inside app state
            ],
            "changed": ["C:\\Users\\runneradmin\\AppData\\Roaming\\network.arc.desktop\\store.json"],
            "removed": [],
        }
        result = ow.classify_fs_changes(delta, self.policies())
        unexpected = {item["path"]: item["reason"] for item in result["unexpected"]}
        self.assertEqual(sorted(unexpected), sorted([
            "C:\\Users\\runneradmin\\AppData\\Local\\Temp\\ARC Node-0.8.11-updater-AbC\\ARC.Node_0.8.11_x64-setup.exe",
            "C:\\arcw0\\app\\arc-desktop.exe.new",
            "C:\\Users\\runneradmin\\Downloads\\x.msi",
            "C:\\arcw0\\home\\.arc\\bin\\arc-node.exe",
        ]))
        self.assertIn("updater working directory", unexpected["C:\\Users\\runneradmin\\AppData\\Local\\Temp\\ARC Node-0.8.11-updater-AbC\\ARC.Node_0.8.11_x64-setup.exe"])
        self.assertIn("frozen", unexpected["C:\\arcw0\\app\\arc-desktop.exe.new"])
        self.assertEqual(result["noise"], ["C:\\Users\\runneradmin\\AppData\\Local\\Temp\\tmp1234.tmp"])
        self.assertIn("C:\\arcw0\\home\\.arc\\settings.json", result["expected"])
        self.assertIn("C:\\Users\\runneradmin\\AppData\\Roaming\\network.arc.desktop\\store.json", result["expected"])

    def test_a_removed_file_in_the_install_dir_is_unexpected(self):
        result = ow.classify_fs_changes({"added": [], "changed": [], "removed": ["C:\\arcw0\\app\\arc-desktop.exe"]}, self.policies())
        self.assertEqual(len(result["unexpected"]), 1)
        self.assertEqual(result["unexpected"][0]["kind"], "removed")


class ProcessTests(unittest.TestCase):
    @staticmethod
    def proc(pid, ppid, name, exe="", cmd="", created="1"):
        return {"ProcessId": pid, "ParentProcessId": ppid, "Name": name, "ExecutablePath": exe, "CommandLine": cmd, "CreationDate": created}

    def base(self):
        return [self.proc(1, 0, "System"), self.proc(100, 1, "arc-desktop.exe", APP_EXE), self.proc(101, 100, "msedgewebview2.exe", "C:\\wv2\\msedgewebview2.exe")]

    def test_parse(self):
        self.assertEqual(len(ow.parse_processes(json.dumps(self.base()))), 3)
        self.assertEqual(len(ow.parse_processes(json.dumps(self.base()[0]))), 1)
        for text in ("", "not json", "[]", '"str"', '[{"ProcessId": "x"}]'):
            self.assertIsNone(ow.parse_processes(text), text)

    def test_descendants(self):
        procs = self.base() + [self.proc(102, 101, "msedgewebview2.exe"), self.proc(200, 1, "svchost.exe")]
        self.assertEqual(ow.descendants(100, procs), {101, 102})

    def violations(self, extra, ignore=()):
        before = self.base()
        after = self.base() + extra
        return ow.process_violations(before, after, 100, [ow.norm_path("C:\\arcw0\\app"), "network.arc.desktop"], ignore)

    def test_webview2_helpers_are_the_apps_own(self):
        found = self.violations([self.proc(110, 101, "msedgewebview2.exe", "C:\\wv2\\msedgewebview2.exe", created="2")])
        self.assertEqual(found, {"new_app_launches": [], "installers": [], "other_in_app_tree": [], "unrelated": [], "lab_helpers": []})

    def test_a_second_app_instance_is_a_new_launch(self):
        found = self.violations([self.proc(120, 1, "arc-desktop.exe", "C:\\arcw0\\app2\\arc-desktop.exe", created="3")])
        self.assertEqual(len(found["new_app_launches"]), 1)

    def test_an_installer_in_the_apps_tree_or_from_its_directories_is_flagged(self):
        found = self.violations([self.proc(130, 100, "ARC.Node_0.8.11_x64-setup.exe", "C:\\Temp\\ARC Node-0.8.11-updater-x\\ARC.Node_0.8.11_x64-setup.exe", created="4")])
        self.assertEqual(len(found["installers"]), 1)
        found = self.violations([self.proc(131, 1, "msiexec.exe", "C:\\Windows\\System32\\msiexec.exe", "msiexec /i C:\\arcw0\\app\\x.msi", created="5")])
        self.assertEqual(len(found["installers"]), 1)

    def test_unrelated_system_processes_are_recorded_but_do_not_count(self):
        found = self.violations([self.proc(140, 1, "svchost.exe", "C:\\Windows\\System32\\svchost.exe", created="6"),
                                 self.proc(141, 1, "msiexec.exe", "C:\\Windows\\System32\\msiexec.exe", "msiexec /V", created="7")])
        self.assertEqual(found["new_app_launches"] + found["installers"] + found["other_in_app_tree"], [])
        self.assertEqual(len(found["unrelated"]), 2)

    def test_another_executable_started_by_the_app_counts(self):
        found = self.violations([self.proc(150, 100, "arc-node.exe", "C:\\arcw0\\home\\.arc\\bin\\arc-node.exe", created="8")])
        self.assertEqual(len(found["other_in_app_tree"]), 1)

    def test_the_checkers_own_process_is_ignored(self):
        exe = "C:\\native\\native-updater-check.exe"
        extra = [self.proc(160, 1, "native-updater-check.exe", exe, created="9")]
        self.assertEqual(len(self.violations(extra)["installers"]), 1, "it would look like an updater without the ignore list")
        self.assertEqual(self.violations(extra, ignore=[exe]), {"new_app_launches": [], "installers": [], "other_in_app_tree": [], "unrelated": [], "lab_helpers": []})

    def test_a_missing_snapshot_is_unknown_not_clean(self):
        self.assertIsNone(ow.process_violations(None, self.base(), 100, []))
        self.assertIsNone(ow.process_violations(self.base(), None, 100, []))
        self.assertIsNone(ow.process_violations(self.base(), self.base(), None, []))


class RedactionTests(unittest.TestCase):
    def test_command_lines_are_kept_only_for_the_processes_under_test_and_ips_are_masked(self):
        procs = [ProcessTests.proc(1, 0, "Runner.Worker.exe", cmd="Runner.Worker.exe --jitconfig SECRETTOKEN"),
                 ProcessTests.proc(2, 1, "arc-desktop.exe", APP_EXE, "arc-desktop.exe --minimized http://149.28.32.76:9090"),
                 ProcessTests.proc(3, 2, "msedgewebview2.exe", cmd="--remote-debugging-port=9222")]
        redacted = ow.redact_processes(procs)
        self.assertEqual(redacted[0]["CommandLine"], "<omitted>")
        self.assertIn("149.28.x.x", redacted[1]["CommandLine"])
        self.assertNotIn("149.28.32.76", redacted[1]["CommandLine"])
        self.assertIn("9222", redacted[2]["CommandLine"])
        self.assertEqual(procs[0]["CommandLine"], "Runner.Worker.exe --jitconfig SECRETTOKEN", "the in-memory list used for the criteria is untouched")
        self.assertIsNone(ow.redact_processes(None))

    def test_long_webview2_command_lines_keep_the_debugging_flag(self):
        flag = "--remote-debugging-port=0"
        command = "msedgewebview2.exe --webview-exe-name=arc-desktop.exe " + "--enable-features=" + ("a" * 900) + " " + flag + " --lang=en"
        redacted = ow.redact_processes([ProcessTests.proc(3, 2, "msedgewebview2.exe", cmd=command)])
        self.assertIn(flag, redacted[0]["CommandLine"])
        self.assertIn(flag, ow.webview_command_lines([ProcessTests.proc(3, 2, "msedgewebview2.exe", cmd=command)])[0])

    def test_the_written_list_never_contains_the_omitted_secret(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "procs.txt"
            ow.write_process_list(path, [ProcessTests.proc(1, 0, "Runner.Listener.exe", cmd="--jitconfig SECRETTOKEN")])
            self.assertNotIn("SECRETTOKEN", path.read_text(encoding="utf-8"))
            ow.write_process_list(path, None)
            self.assertEqual(path.read_text(encoding="utf-8"), "unavailable\n")


class CriteriaTests(unittest.TestCase):
    def judge(self, scenario="latest-404", trigger=None, requests=None, fs=NO_FS, procs=NO_PROCS, writes_ok=True, install_changes=()):
        trigger = TRIGGER_404 if trigger is None and scenario == "latest-404" else (TRIGGER_BAIT if trigger is None else trigger)
        requests = requests if requests is not None else requests_for(MANIFEST, REDIRECT)
        return ow.evaluate_case(scenario, trigger, requests, fs, procs, writes_ok, None if install_changes is None else list(install_changes))

    def test_a_clean_latest_404_run_passes(self):
        result = self.judge()
        self.assertEqual(result["verdict"], "PASS", result["reasons"])
        self.assertEqual(result["criteria"], {key: True for key in ow.CRITERIA})

    def test_a_clean_bait_run_passes(self):
        result = self.judge("bait-0.8.11", requests=requests_for(MANIFEST))
        self.assertEqual(result["verdict"], "PASS", result["reasons"])

    def test_every_missing_piece_of_evidence_makes_it_unproved_never_pass(self):
        cases = {
            "no request log": dict(requests=ow.summarize_requests(None)),
            "no request at all": dict(requests=requests_for()),
            "trigger did not run": dict(trigger={"ran": False, "ok": False, "error_text": "cdp never appeared", "stage": "cdp", "value": None}),
            "no file classification": dict(fs=None),
            "no process comparison": dict(procs=None),
            "no complete file-write log": dict(writes_ok=False),
            "no install-dir diff": dict(install_changes=None),
        }
        for name, kwargs in cases.items():
            with self.subTest(name):
                result = self.judge(**kwargs)
                self.assertEqual(result["verdict"], "UNPROVED", result)
                self.assertTrue(result["reasons"])
                self.assertFalse(any(value is False for value in result["criteria"].values()), "missing evidence must not read as a failure either")

    def test_each_violation_fails_exactly_its_criterion(self):
        cases = [
            ("a bundle request", requests_for(MANIFEST, rec("/FerrumVir/arc-chain/releases/download/v0.8.11/ARC.Node_0.8.11_x64-setup.exe")), "no_bundle_download"),
            ("a payload flagged by the server", requests_for(MANIFEST, rec("/innocent", payload=True)), "no_bundle_download"),
            ("another latest.json", requests_for(MANIFEST, rec("/other/latest.json")), "only_manifest_url"),
            ("an unknown path", requests_for(MANIFEST, rec("/FerrumVir/arc-chain/releases")), "only_manifest_url"),
        ]
        for name, requests, criterion in cases:
            with self.subTest(name):
                result = self.judge(requests=requests)
                self.assertEqual(result["verdict"], "FAIL")
                self.assertFalse(result["criteria"][criterion], result["criteria"])

    def test_the_apis_latest_endpoint_is_not_a_violation(self):
        result = self.judge(requests=requests_for(MANIFEST, rec(ow.API_LATEST_PATH, host="api.github.com")))
        self.assertEqual(result["verdict"], "PASS", result["reasons"])

    def test_file_and_process_violations(self):
        fs_bad = {"expected": [], "noise": [], "unexpected": [{"path": "C:\\x", "kind": "added", "reason": "a payload-like file name in the temp directory"}]}
        result = self.judge(fs=fs_bad)
        self.assertEqual((result["verdict"], result["criteria"]["no_new_files"], result["criteria"]["no_install"]), ("FAIL", False, False))
        fs_other = {"expected": [], "noise": [], "unexpected": [{"path": "C:\\y", "kind": "added", "reason": "frozen location changed (real profile Downloads)"}]}
        result = self.judge(fs=fs_other)
        self.assertEqual((result["criteria"]["no_new_files"], result["criteria"]["no_install"]), (False, True))
        installer = dict(NO_PROCS, installers=["setup.exe"])
        result = self.judge(procs=installer)
        self.assertEqual((result["criteria"]["no_install"], result["criteria"]["no_new_app_launch"]), (False, False))
        launched = dict(NO_PROCS, new_app_launches=["arc-desktop.exe"])
        self.assertEqual(self.judge(procs=launched)["criteria"]["no_new_app_launch"], False)
        self.assertEqual(self.judge(install_changes=["C:\\arcw0\\app\\arc-desktop.exe"])["criteria"]["no_install"], False)

    def test_the_scenario_must_actually_have_happened(self):
        wrong_text = dict(TRIGGER_404, error_text="error sending request for url (https://github.com/...)")
        result = self.judge(trigger=wrong_text)
        self.assertEqual(result["verdict"], "UNPROVED")
        self.assertTrue(any("latest-404" in reason for reason in result["reasons"]))
        resolved = {"ran": True, "ok": True, "error_text": None, "stage": "invoke", "value": None}
        self.assertEqual(self.judge(trigger=resolved)["verdict"], "UNPROVED")
        bait_without_update = {"ran": True, "ok": True, "error_text": None, "stage": "invoke", "value": None}
        self.assertEqual(self.judge("bait-0.8.11", trigger=bait_without_update, requests=requests_for(MANIFEST))["verdict"], "UNPROVED")
        self.assertEqual(self.judge("nonsense")["verdict"], "UNPROVED")

    def test_a_failure_is_never_softened_by_an_unproved_scenario(self):
        wrong_text = dict(TRIGGER_404, error_text="something else")
        result = self.judge(trigger=wrong_text, requests=requests_for(MANIFEST, rec("/x/y/z.exe")))
        self.assertEqual(result["verdict"], "FAIL")

    def test_tier_and_overall_verdicts(self):
        passing = [{"name": "clean", "verdict": "PASS"}, {"name": "cached-bait", "verdict": "PASS"}]
        self.assertEqual(ow.tier_result(["clean", "cached-bait"], passing, "ran"), "PASS")
        self.assertEqual(ow.tier_result(["clean", "cached-bait"], passing[:1], "ran"), "UNPROVED")
        self.assertEqual(ow.tier_result(["clean", "cached-bait"], passing, "infeasible"), "UNPROVED")
        self.assertEqual(ow.tier_result(["clean", "cached-bait"], [passing[0], {"name": "cached-bait", "verdict": "FAIL"}], "ran"), "FAIL")
        tiers = {"released_app": {"attempted": True, "result": "PASS"}, "native_check": {"attempted": False, "result": "UNPROVED"}}
        self.assertEqual(ow.overall_verdict(tiers, passing), "PASS")
        tiers["native_check"] = {"attempted": True, "result": "UNPROVED"}
        self.assertEqual(ow.overall_verdict(tiers, passing), "UNPROVED", "an attempted tier that proved nothing keeps the OS from PASS")
        tiers["native_check"] = {"attempted": True, "result": "FAIL"}
        self.assertEqual(ow.overall_verdict(tiers, passing), "FAIL")
        self.assertEqual(ow.overall_verdict({"released_app": {"attempted": False}, "native_check": {"attempted": False}}, []), "UNPROVED")
        self.assertEqual(ow.overall_verdict({"released_app": {"attempted": True, "result": "PASS"}}, passing[:1]), "UNPROVED")


class FakeWebSocketServer:
    """A one-connection RFC 6455 server: handshake, then a script of ('recv'|'send'|...) steps run on the accepted socket."""

    def __init__(self, script, accept_override=None):
        self.script, self.accept_override = script, accept_override
        self.server = socket.socket()
        self.server.bind(("127.0.0.1", 0))
        self.server.listen(1)
        self.port = self.server.getsockname()[1]
        self.received = []
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    @staticmethod
    def read_exact(sock, count):
        data = b""
        while len(data) < count:
            chunk = sock.recv(count - len(data))
            if not chunk:
                raise EOFError
            data += chunk
        return data

    def read_frame(self, sock):
        first = self.read_exact(sock, 2)
        opcode, masked, length = first[0] & 0x0F, bool(first[1] & 0x80), first[1] & 0x7F
        if length == 126:
            length = struct.unpack(">H", self.read_exact(sock, 2))[0]
        elif length == 127:
            length = struct.unpack(">Q", self.read_exact(sock, 8))[0]
        key = self.read_exact(sock, 4) if masked else b""
        payload = self.read_exact(sock, length) if length else b""
        if masked:
            payload = bytes(b ^ key[i % 4] for i, b in enumerate(payload))
        return opcode, masked, payload

    def run(self):
        conn, _ = self.server.accept()
        try:
            request = b""
            while b"\r\n\r\n" not in request:
                request += conn.recv(4096)
            key = [line.split(b":", 1)[1].strip() for line in request.split(b"\r\n") if line.lower().startswith(b"sec-websocket-key")][0].decode()
            self.path = request.split(b" ")[1].decode()
            accept = self.accept_override or ow.ws_accept_key(key)
            conn.sendall(("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: %s\r\n\r\n" % accept).encode())
            self.script(self, conn)
        except (EOFError, OSError):
            pass
        finally:
            conn.close()
            self.server.close()


class WebSocketTests(unittest.TestCase):
    def client(self, server, timeout=5.0):
        client = ow.WebSocketClient("127.0.0.1", server.port, "/devtools/page/ABC", timeout=timeout)
        self.addCleanup(client.close)
        return client

    def test_handshake_masked_text_out_and_text_in(self):
        def script(server, conn):
            opcode, masked, payload = server.read_frame(conn)
            server.received.append((opcode, masked, payload))
            conn.sendall(ow.ws_encode_frame(b"echo:" + payload, mask=False))

        server = FakeWebSocketServer(script)
        client = self.client(server)
        client.connect()
        client.send_text("héllo")
        self.assertEqual(client.recv_text(3), "echo:héllo")
        self.assertEqual(server.received, [(0x1, True, "héllo".encode())])
        self.assertEqual(server.path, "/devtools/page/ABC")

    def test_a_wrong_accept_key_is_refused(self):
        server = FakeWebSocketServer(lambda s, c: None, accept_override="AAAA")
        with self.assertRaises(ow.WebSocketError):
            self.client(server).connect()

    def test_large_frames_use_the_extended_lengths(self):
        sizes = [125, 126, 65535, 65536, 200000]

        def script(server, conn):
            for _ in sizes:
                opcode, masked, payload = server.read_frame(conn)
                server.received.append(len(payload))
                conn.sendall(ow.ws_encode_frame(payload, mask=False))

        server = FakeWebSocketServer(script)
        client = self.client(server, timeout=10)
        client.connect()
        for size in sizes:
            text = "x" * size
            client.send_text(text)
            self.assertEqual(client.recv_text(5), text)
        self.assertEqual(server.received, sizes)

    def test_fragmented_messages_are_joined_and_pings_are_answered(self):
        def script(server, conn):
            conn.sendall(bytes([0x01, 0x03]) + b"abc")                 # text, not final
            conn.sendall(bytes([0x89, 0x02]) + b"pp")                  # ping in the middle
            conn.sendall(bytes([0x80, 0x03]) + b"def")                 # continuation, final
            opcode, masked, payload = server.read_frame(conn)           # the client's pong
            server.received.append((opcode, payload))

        server = FakeWebSocketServer(script)
        client = self.client(server)
        client.connect()
        self.assertEqual(client.recv_text(3), "abcdef")
        server.thread.join(timeout=3)
        self.assertEqual(server.received, [(0xA, b"pp")])

    def test_close_and_timeout_raise(self):
        def closing(server, conn):
            conn.sendall(bytes([0x88, 0x00]))

        server = FakeWebSocketServer(closing)
        client = self.client(server)
        client.connect()
        with self.assertRaises(ow.WebSocketError):
            client.recv_text(3)

        silent = FakeWebSocketServer(lambda s, c: time.sleep(1.5))
        quiet = self.client(silent)
        quiet.connect()
        started = time.time()
        with self.assertRaises(ow.WebSocketError):
            quiet.recv_text(0.3)
        self.assertLess(time.time() - started, 1.2)

    def test_connect_refused_is_an_oserror(self):
        sock = socket.socket()
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
        sock.close()
        with self.assertRaises(OSError):
            ow.WebSocketClient("127.0.0.1", port, "/", timeout=1).connect()


class FakeWs:
    """Stands in for WebSocketClient in the CDP tests: records sends, replays scripted incoming messages."""

    def __init__(self, incoming):
        self.sent, self.incoming = [], list(incoming)

    def send_text(self, text):
        self.sent.append(json.loads(text))

    def recv_text(self, timeout=60.0):
        if not self.incoming:
            raise ow.WebSocketError("nothing more to read")
        return json.dumps(self.incoming.pop(0))


class CdpTests(unittest.TestCase):
    def test_evaluate_skips_events_and_other_ids_and_sends_the_right_request(self):
        ws = FakeWs([{"method": "Runtime.consoleAPICalled", "params": {}}, {"id": 99, "result": {}},
                     {"id": 7, "result": {"result": {"type": "object", "value": {"ok": True, "value": None}}}}])
        message = ow.cdp_evaluate(ws, "1+1", 7, timeout=5)
        self.assertEqual(message["id"], 7)
        sent = ws.sent[0]
        self.assertEqual((sent["id"], sent["method"], sent["params"]["expression"]), (7, "Runtime.evaluate", "1+1"))
        self.assertTrue(sent["params"]["awaitPromise"] and sent["params"]["returnByValue"])

    def test_evaluate_gives_up_when_the_answer_never_comes(self):
        with self.assertRaises(ow.WebSocketError):
            ow.cdp_evaluate(FakeWs([]), "x", 1, timeout=1)

    def test_result_parsing(self):
        self.assertEqual(ow.cdp_result({"id": 1, "result": {"result": {"type": "string", "value": "v"}}}), (True, "v", None))
        self.assertEqual(ow.cdp_result({"id": 1, "result": {"exceptionDetails": {"exception": {"description": "ReferenceError: x"}}}}), (False, None, "ReferenceError: x"))
        self.assertEqual(ow.cdp_result({"id": 1, "result": {"exceptionDetails": {"text": "Uncaught"}}}), (False, None, "Uncaught"))
        self.assertEqual(ow.cdp_result({"id": 1, "error": {"code": -32000, "message": "No target"}}), (False, None, "No target"))
        self.assertFalse(ow.cdp_result("garbage")[0])

    def test_trigger_outcomes(self):
        self.assertEqual(ow.trigger_outcome_from({"ok": False, "stage": "invoke", "error": ow.RELEASE_NOT_FOUND, "error_type": "string"}, None, True),
                         {"ran": True, "ok": False, "error_text": ow.RELEASE_NOT_FOUND, "stage": "invoke", "error_type": "string", "error_json": None, "value": None})
        ok = ow.trigger_outcome_from({"ok": True, "value": {"version": "0.8.11"}}, None, True)
        self.assertEqual((ok["ran"], ok["ok"], ok["value"]), (True, True, {"version": "0.8.11"}))
        no_ipc = ow.trigger_outcome_from({"ok": False, "stage": "no-ipc", "error": "no invoke"}, None, True)
        self.assertEqual((no_ipc["ran"], no_ipc["stage"]), (False, "no-ipc"))
        self.assertFalse(ow.trigger_outcome_from(None, "Runtime exception", False)["ran"])
        self.assertFalse(ow.trigger_outcome_from("weird", None, True)["ran"])

    def test_the_expressions_call_the_updaters_check_command_and_nothing_else(self):
        expression = ow.trigger_expression()
        self.assertIn("plugin:updater|check", expression)
        self.assertIn("__TAURI_INTERNALS__", expression)
        for forbidden in ("download", "install", "relaunch", "fetch(", "XMLHttpRequest"):
            self.assertNotIn(forbidden, expression)
        probe = ow.ipc_probe_expression()
        self.assertIn("plugin:app|version", probe)
        self.assertNotIn("updater", probe)

    def test_page_target_selection(self):
        targets = [
            {"type": "background_page", "url": "x", "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/B"},
            {"type": "page", "url": "about:blank", "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/C"},
            {"type": "page", "url": "http://tauri.localhost/", "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/A"},
        ]
        self.assertTrue(ow.pick_page_target(targets)["webSocketDebuggerUrl"].endswith("/A"))
        self.assertTrue(ow.pick_page_target([{"type": "page", "url": "tauri://localhost", "webSocketDebuggerUrl": "ws://127.0.0.1:9222/x/1"}]))
        self.assertIsNone(ow.pick_page_target(targets[:2]))
        self.assertIsNone(ow.pick_page_target("not a list"))
        self.assertEqual(ow.parse_ws_url("ws://127.0.0.1:9222/devtools/page/A"), ("127.0.0.1", 9222, "/devtools/page/A"))
        with self.assertRaises(ow.WebSocketError):
            ow.parse_ws_url("wss://example/x")

    def test_wait_for_page_polls_until_a_page_appears(self):
        answers = iter([OSError("refused"), ValueError("bad json"), [], [{"type": "page", "url": "http://tauri.localhost/", "webSocketDebuggerUrl": "ws://127.0.0.1:9222/p"}]])
        clock = {"t": 0.0}

        def fetch(url):
            value = next(answers)
            if isinstance(value, Exception):
                raise value
            return value

        def sleep(seconds):
            clock["t"] += seconds

        target, why = ow.wait_for_page(9222, 60, fetch=fetch, sleep=sleep, clock=lambda: clock["t"])
        self.assertEqual((bool(target), why), (True, "ok"))
        target, why = ow.wait_for_page(9222, 5, fetch=lambda url: (_ for _ in ()).throw(OSError("refused")), sleep=sleep, clock=lambda: clock["t"])
        self.assertIsNone(target)
        self.assertIn("OSError", why)


class ShellAndInstallTests(unittest.TestCase):
    def test_shell_logs_every_command_with_ips_masked_and_reports_failures(self):
        with tempfile.TemporaryDirectory() as tmp:
            shell = ow.Shell(Path(tmp) / "steps.log")
            ok = shell.run([sys.executable, "-c", "print('seen 149.28.32.76')"])
            self.assertEqual((ok.rc, ok.out.strip()), (0, "seen 149.28.32.76"))
            bad = shell.run([sys.executable, "-c", "import sys; print('boom'); sys.exit(3)"])
            self.assertEqual(bad.rc, 3)
            self.assertEqual(shell.run(["definitely-not-a-command-xyz"]).rc, 127)
            slow = shell.run([sys.executable, "-c", "import time; time.sleep(5)"], timeout=0.5)
            self.assertEqual(slow.rc, 124)
            ignored = shell.run([sys.executable, "-c", "print('x')"], capture=False)
            self.assertEqual((ignored.rc, ignored.out), (0, ""))
            text = (Path(tmp) / "steps.log").read_text(encoding="utf-8")
        self.assertIn("149.28.x.x", text)
        self.assertNotIn("149.28.32.76", text)
        self.assertIn("rc=3", text)
        self.assertIn("rc=124", text)
        self.assertIn("rc=127", text)
        self.assertEqual(len(shell.history), 5)

    def test_discover_app_exe(self):
        present = {"C:\\arcw0\\app\\arc-desktop.exe"}
        self.assertEqual(ow.discover_app_exe(["C:\\arcw0\\app"], lambda p: p in present, lambda d: []), "C:\\arcw0\\app\\arc-desktop.exe")
        listing = {"C:\\Users\\r\\AppData\\Local\\ARC Node": ["uninstall.exe", "ARC Node.exe", "x.dll"]}

        def listdir(directory):
            if directory in listing:
                return listing[directory]
            raise OSError

        self.assertEqual(ow.discover_app_exe(["C:\\arcw0\\app", "C:\\Users\\r\\AppData\\Local\\ARC Node"], lambda p: False, listdir),
                         "C:\\Users\\r\\AppData\\Local\\ARC Node\\ARC Node.exe")
        self.assertIsNone(ow.discover_app_exe(["C:\\nowhere"], lambda p: False, listdir))

    def make_ctx(self, tmp):
        ctx = ow.Context(Path(tmp) / "ev", Path(tmp) / "w", ow.Shell(), {}, env={"LOCALAPPDATA": "C:\\L", "ProgramFiles": "C:\\PF"})
        ctx.install_dir = "C:\\arcw0\\app"
        return ctx

    def test_install_records_where_the_exe_landed_and_waits_for_it(self):
        with tempfile.TemporaryDirectory() as tmp:
            ctx = self.make_ctx(tmp)
            calls = {"n": 0}

            def exists(path):
                calls["n"] += 1
                return calls["n"] >= 4 and path == APP_EXE

            clock = {"t": 0.0}
            with mock.patch.object(ow.Shell, "run", return_value=ow.CmdResult(0, "Uninstall key for 149.28.32.76")) as run:
                detail = ow.install_app(ctx, ow.select_installer(release_json()), Path("C:\\arcw0\\dl\\setup.exe"), exists=exists, listdir=lambda d: [],
                                        sleep=lambda s: clock.__setitem__("t", clock["t"] + s), clock=lambda: clock["t"], wait_s=60)
            self.assertEqual(detail["exe"], APP_EXE)
            self.assertEqual(ctx.app_exe, APP_EXE)
            self.assertEqual(ctx.install_dir, "C:\\arcw0\\app")
            self.assertIn("149.28.x.x", detail["uninstall_registry"])
            first = run.call_args_list[0]
            self.assertEqual(first[0][0][1:], ["/S", "/D=C:\\arcw0\\app"])
            self.assertFalse(first[1]["capture"], "the installer's children must not keep a pipe open")

    def test_install_without_an_exe_gives_up_after_the_wait(self):
        with tempfile.TemporaryDirectory() as tmp:
            ctx = self.make_ctx(tmp)
            clock = {"t": 0.0}
            with mock.patch.object(ow.Shell, "run", return_value=ow.CmdResult(1, "")):
                detail = ow.install_app(ctx, ow.select_installer(release_json()), Path("x.exe"), exists=lambda p: False, listdir=lambda d: [],
                                        sleep=lambda s: clock.__setitem__("t", clock["t"] + s), clock=lambda: clock["t"], wait_s=30)
            self.assertIsNone(detail["exe"])
            self.assertIsNone(ctx.app_exe)
            self.assertEqual(detail["installer_rc"], 1)

    def test_sandbox_environment(self):
        env = ow.sandbox_env({"PATH": "p", "HTTPS_PROXY": "http://proxy", "USERPROFILE": "C:\\Users\\r"}, "C:\\arcw0\\home", "C:\\arcw0\\wv2")
        self.assertEqual((env["HOME"], env["USERPROFILE"], env["WEBVIEW2_USER_DATA_FOLDER"]), ("C:\\arcw0\\home", "C:\\Users\\r", "C:\\arcw0\\wv2"),
                         "HOME is the sandbox; USERPROFILE stays real so Tauri's app_data_dir() still resolves")
        self.assertIn("--remote-debugging-port=9222", env["WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS"])
        self.assertNotIn("HTTPS_PROXY", env)
        self.assertEqual(env["PATH"], "p")

    def test_kill_app_issues_the_tree_kill_and_the_profile_sweep(self):
        shell = mock.Mock()
        shell.run.return_value = ow.CmdResult(0, "")
        ctx = ow.Context(Path("e"), Path("w"), shell, {})
        ow.kill_app(ctx, 4321, "C:\\arcw0\\wv2")
        argvs = [call[0][0] for call in shell.run.call_args_list]
        self.assertEqual(argvs[0], ["taskkill", "/F", "/T", "/PID", "4321"])
        self.assertEqual(argvs[1], ["taskkill", "/F", "/T", "/IM", "arc-desktop.exe"])
        self.assertEqual(argvs[2], ["taskkill", "/F", "/T", "/IM", "msedgedriver.exe"])
        self.assertIn("C:\\arcw0\\wv2", argvs[3][-1])
        self.assertNotIn('"', argvs[3][-1])

    def test_mitm_process_start_waits_for_the_ready_file_and_detects_an_early_exit(self):
        with tempfile.TemporaryDirectory() as tmp:
            stub_dir = Path(tmp) / "lib"
            stub_dir.mkdir()
            good = ("import sys, time, pathlib\nargs = sys.argv\nready = args[args.index('--ready-file') + 1]\ntime.sleep(0.3)\n"
                    "pathlib.Path(ready).write_text('{}')\ntime.sleep(30)\n")
            (stub_dir / "mitm_server.py").write_text(good, encoding="utf-8")
            with mock.patch.object(ow, "LIB", stub_dir):
                server = ow.MitmProcess("latest-404", "c.crt", "k.key", Path(tmp) / "req.jsonl", Path(tmp) / "ready")
                self.assertIn("--scenario", server.argv())
                self.assertEqual(server.argv()[server.argv().index("--listen") + 1], "127.0.0.1,::1:443")
                server.start(timeout=10)
                self.assertTrue(server.alive())
                server.stop()
                self.assertFalse(server.alive())
                (stub_dir / "mitm_server.py").write_text("import sys\nprint('cannot bind 443', file=sys.stderr)\nsys.exit(2)\n", encoding="utf-8")
                failing = ow.MitmProcess("latest-404", "c.crt", "k.key", Path(tmp) / "req2.jsonl", Path(tmp) / "ready2")
                with self.assertRaises(RuntimeError) as caught:
                    failing.start(timeout=10)
                self.assertIn("cannot bind 443", str(caught.exception))


class NativeCheckTests(unittest.TestCase):
    def test_json_line_is_found_among_noise(self):
        line = json.dumps({"schema": "arc.legacy-bridge.wave0-lab.native-updater-check.v1", "outcome": "error", "error_kind": "ReleaseNotFound",
                           "error": ow.RELEASE_NOT_FOUND, "update": None, "download_attempted": False})
        self.assertEqual(ow.parse_native_json("warning: x\n" + line + "\n")["outcome"], "error")
        self.assertIsNone(ow.parse_native_json("{\"schema\": \"other\"}\nnothing"))
        self.assertIsNone(ow.parse_native_json(""))

    def test_trigger_mapping(self):
        error = {"outcome": "error", "error": ow.RELEASE_NOT_FOUND, "error_kind": "ReleaseNotFound"}
        trigger = ow.native_trigger(error, 0, "latest-404")
        self.assertEqual((trigger["ran"], trigger["ok"], trigger["error_text"]), (True, False, ow.RELEASE_NOT_FOUND))
        update = {"outcome": "update_available", "update": {"version": "0.8.11", "download_url": "https://github.com/x"}, "download_attempted": False}
        trigger = ow.native_trigger(update, 0, "bait-0.8.11")
        self.assertEqual((trigger["ok"], trigger["value"]["version"]), (True, "0.8.11"))
        self.assertFalse(ow.native_trigger(None, 0, "latest-404")["ran"])
        self.assertFalse(ow.native_trigger(error, 2, "latest-404")["ran"])

    def test_the_native_cases_judge_like_the_app_cases(self):
        error = {"outcome": "error", "error": ow.RELEASE_NOT_FOUND}
        trigger = ow.native_trigger(error, 0, "latest-404")
        result = ow.evaluate_case("latest-404", trigger, requests_for(MANIFEST, REDIRECT), NO_FS, NO_PROCS, True, [])
        self.assertEqual(result["verdict"], "PASS", result["reasons"])

    def test_the_positive_control_must_be_seen_by_the_harness(self):
        seen = {"requests": {"classes": {"payload": ["github.com/FerrumVir/arc-chain/releases/download/v0.8.11/ARC.Node_0.8.11_x64-setup.exe"]}},
                "native_report": {"download_attempted": True, "download_result": "error: invalid signature"}}
        self.assertEqual(ow.control_judgement(seen)["verdict"], "PASS")
        blind = {"requests": {"classes": {}}, "native_report": {"download_attempted": True}}
        self.assertEqual(ow.control_judgement(blind)["verdict"], "FAIL", "a download the harness did not see would make every PASS worthless")
        none = {"requests": {"classes": {}}, "native_report": {"download_attempted": False}}
        self.assertEqual(ow.control_judgement(none)["verdict"], "UNPROVED")

    def test_the_prebuilt_binary_is_found_where_the_workflow_builds_it(self):
        default = str(ow.HERE / "native-updater-check" / "target" / "release" / ow.NATIVE_EXE_NAME)
        found, tried = ow.find_native_exe(None, {}, exists=lambda path: path == default)
        self.assertEqual((found, tried), (default, [default]))
        found, tried = ow.find_native_exe("D:\\x\\n.exe", {"NATIVE_UPDATER_CHECK": "E:\\y\\n.exe", "CARGO_TARGET_DIR": "F:\\t"}, exists=lambda path: path == "E:\\y\\n.exe")
        self.assertEqual(found, "E:\\y\\n.exe")
        self.assertEqual(tried[0], "D:\\x\\n.exe", "an explicit path is tried first")
        self.assertEqual(len(tried), 4)
        self.assertEqual(ow.find_native_exe(None, {}, exists=lambda path: False)[0], None)

    def test_binary_info_survives_a_missing_file(self):
        self.assertIn("error", ow.binary_info(Path("C:\\nope\\n.exe")))
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "n.exe"
            path.write_bytes(b"abc")
            info = ow.binary_info(path)
        self.assertEqual((info["size"], info["sha256"]), (3, hashlib.sha256(b"abc").hexdigest()))


def case_document(name, tier, scenario, tmp, verdict="PASS", criteria=None, rows=None):
    rows = rows if rows is not None else ([["github.com", ow.REDIRECT_TARGET_PATH, 1], ["github.com", ow.MANIFEST_PATH, 1]] if scenario == "latest-404" else [["github.com", ow.MANIFEST_PATH, 1]])
    evidence = "requests-%s-%s.jsonl" % (tier, name)
    (Path(tmp) / evidence).write_text("{}\n", encoding="utf-8")
    return {"name": name, "tier": tier, "scenario": scenario, "trigger_outcome": {}, "criteria": criteria if criteria is not None else {k: True for k in ow.CRITERIA},
            "requests": {"total": sum(r[2] for r in rows), "by_host_path": rows}, "verdict": verdict, "evidence_files": [evidence]}


class ResultAssemblyTests(unittest.TestCase):
    def build(self, tmp, **overrides):
        cases = overrides.pop("cases", None)
        if cases is None:
            cases = [case_document(n, t, ow.CASE_SCENARIO[n], tmp) for t in ow.TIERS for n in ow.CASE_NAMES]
        tiers = overrides.pop("tiers", {t: {"attempted": True, "status": "ran", "result": "PASS", "reason": "", "trigger": "x"} for t in ow.TIERS})
        kwargs = dict(
            runner={"image": "win25 1", "arch": "AMD64", "os_version": "Windows-2025"},
            app={"tag": "v0.7.11", "asset": "ARC.Node_0.7.11_x64-setup.exe", "asset_sha256": "sha256:" + NSIS_SHA, "release_digest": "sha256:" + NSIS_SHA,
                 "digest_match": True, "version_reported": "0.7.11"},
            plugin={"name": "tauri-plugin-updater", "version": "2.10.1", "provenance": []},
            tiers=tiers, cases=cases, manifest404=ow.RELEASE_NOT_FOUND, manifest404_source="released_app",
            isolation={"hosts_mapped": ["github.com"], "ca_sha256": "ab" * 32, "live_block": True},
            controls=[], notes=[])
        kwargs.update(overrides)
        return ow.build_result(**kwargs)

    def summarize(self, tmp, document):
        path = Path(tmp) / "result.json"
        raw = json.dumps(document).encode()
        path.write_bytes(raw)
        return ssum.evaluate_os_result(document, raw, path, Path(tmp))

    def test_a_complete_result_is_pass_for_the_summary_too(self):
        with tempfile.TemporaryDirectory() as tmp:
            document = self.build(tmp)
            self.assertEqual(document["schema"], ssum.OS_SCHEMA)
            self.assertEqual(document["verdict"], "PASS")
            record = self.summarize(tmp, document)
            self.assertEqual(record["verdict"], "PASS", record["reasons"])
            self.assertEqual(record["evidence"]["missing_from_records"], [])

    def test_released_app_alone_is_pass_when_the_native_tier_was_not_requested(self):
        with tempfile.TemporaryDirectory() as tmp:
            cases = [case_document(n, "released_app", ow.CASE_SCENARIO[n], tmp) for n in ow.CASE_NAMES]
            tiers = {"released_app": {"attempted": True, "status": "ran", "result": "PASS", "reason": ""}, "native_check": {"attempted": False, "status": "not_attempted", "result": "UNPROVED", "reason": "not requested"}}
            document = self.build(tmp, cases=cases, tiers=tiers)
            self.assertEqual(document["verdict"], "PASS")
            self.assertEqual(self.summarize(tmp, document)["verdict"], "PASS")

    def test_an_attempted_but_infeasible_tier_keeps_the_os_unproved_in_both_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            cases = [case_document(n, "released_app", ow.CASE_SCENARIO[n], tmp) for n in ow.CASE_NAMES]
            tiers = {"released_app": {"attempted": True, "status": "ran", "result": "PASS", "reason": ""},
                     "native_check": {"attempted": True, "status": "infeasible", "result": "UNPROVED", "reason": "cargo build failed"}}
            document = self.build(tmp, cases=cases, tiers=tiers)
            self.assertEqual(document["verdict"], "UNPROVED")
            self.assertEqual(self.summarize(tmp, document)["verdict"], "UNPROVED")

    def test_mutations_are_caught_by_the_summary(self):
        with tempfile.TemporaryDirectory() as tmp:
            for criterion in ow.CRITERIA:
                bad = {k: True for k in ow.CRITERIA}
                bad[criterion] = None
                cases = [case_document(n, t, ow.CASE_SCENARIO[n], tmp) for t in ow.TIERS for n in ow.CASE_NAMES]
                cases[0]["criteria"] = bad
                cases[0]["verdict"] = "UNPROVED"
                document = self.build(tmp, cases=cases)
                self.assertEqual(document["verdict"], "UNPROVED", criterion)
                self.assertEqual(self.summarize(tmp, document)["verdict"], "UNPROVED", criterion)
                bad[criterion] = False
                cases[0]["verdict"] = "FAIL"
                document = self.build(tmp, cases=cases)
                self.assertEqual(document["verdict"], "FAIL", criterion)
                self.assertEqual(self.summarize(tmp, document)["verdict"], "FAIL", criterion)

    def test_summary_demands_the_recorded_error_text_the_live_block_and_the_digest_match(self):
        with tempfile.TemporaryDirectory() as tmp:
            for field, value in (("manifest404", "some other error"), ("manifest404", None)):
                self.assertEqual(self.summarize(tmp, self.build(tmp, **{field: value}))["verdict"], "UNPROVED")
            self.assertEqual(self.summarize(tmp, self.build(tmp, isolation={"hosts_mapped": ["github.com"], "ca_sha256": "ab" * 32, "live_block": False}))["verdict"], "UNPROVED")
            app = {"tag": "v0.7.11", "asset": "x", "asset_sha256": "sha256:" + "0" * 64, "release_digest": "sha256:" + NSIS_SHA, "digest_match": True, "version_reported": None}
            self.assertEqual(self.summarize(tmp, self.build(tmp, app=app))["verdict"], "FAIL")
            plugin = {"name": "tauri-plugin-updater", "version": "2.9.0", "provenance": []}
            self.assertEqual(self.summarize(tmp, self.build(tmp, plugin=plugin))["verdict"], "FAIL")

    def test_request_tables_must_add_up(self):
        with tempfile.TemporaryDirectory() as tmp:
            cases = [case_document(n, t, ow.CASE_SCENARIO[n], tmp) for t in ow.TIERS for n in ow.CASE_NAMES]
            cases[1]["requests"]["total"] = 99
            self.assertEqual(self.summarize(tmp, self.build(tmp, cases=cases))["verdict"], "UNPROVED")

    def test_evidence_the_cases_list_exists_in_the_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            document = self.build(tmp)
            (Path(tmp) / document["cases"][0]["evidence_files"][0]).unlink()
            self.assertTrue(self.summarize(tmp, document)["evidence"]["missing_from_records"])


class OrchestrationTests(unittest.TestCase):
    """cmd_run with every machine-touching step replaced by a fake: result.json is always written, partial evidence survives, nothing hides a failure."""

    def args(self, tmp, tier="both", cases="clean,cached-bait"):
        import argparse
        return argparse.Namespace(evidence=str(Path(tmp) / "evidence"), tier=tier, cases=cases, config=None, work=str(Path(tmp) / "work"), native_exe=None)

    def fake_ca_module(self, tmp):
        module = mock.Mock()

        def make_ca(outdir, hostnames):
            out = Path(outdir)
            (out / "private").mkdir(parents=True, exist_ok=True)
            (out / "ca.crt").write_text("-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n", encoding="utf-8")
            (out / "ca.sha256").write_text("ab" * 32 + "\n", encoding="utf-8")
            return {"ca_cert": str(out / "ca.crt"), "ca_key": str(out / "private/ca.key"), "server_cert": str(out / "server.crt"),
                    "server_key": str(out / "private/server.key"), "ca_sha256": "ab" * 32, "hostnames": list(hostnames)}

        def copy_public(outdir, dest):
            Path(dest).mkdir(parents=True, exist_ok=True)
            for name in ("ca.crt", "ca.sha256"):
                (Path(dest) / name).write_bytes((Path(outdir) / name).read_bytes())
            return []

        module.make_ca = make_ca
        module.DEFAULT_HOSTS = ("github.com", "api.github.com")
        module.copy_public = copy_public
        module.scan_for_private_keys = lambda root: []
        return module

    def patched(self, tmp, **overrides):
        evidence = Path(tmp) / "evidence"

        def fake_case(ctx, name, state, settle_s=10.0):
            document = case_document(name, "released_app", ow.CASE_SCENARIO[name], evidence)
            document["trigger_outcome"] = {"error_text": ow.RELEASE_NOT_FOUND if name == "clean" else None}
            document["page_probe"] = {"value": {"app_version": "0.7.11"}}
            return document

        def fake_native_case(ctx, name, exe, pubkey, control=False):
            document = case_document(name, "native_check", ow.CASE_SCENARIO[name], evidence)
            document["trigger_outcome"] = {"error_text": ow.RELEASE_NOT_FOUND if name == "clean" else None}
            document["requests"]["classes"] = {"payload": ["github.com/x.exe"]} if control else {}
            document["native_report"] = {"download_attempted": control}
            return document

        installed = {"exe": APP_EXE}

        def fake_install(ctx, asset, installer, **kwargs):
            ctx.app_exe = APP_EXE
            return {"exe": APP_EXE}

        def fake_provenance(path, pubkey):
            return {"plugin_version_from_binary": ["2.10.1"], "pubkey_in_binary": "KEY", "binary_sha256": "00"}

        patches = {
            "IS_WINDOWS": True,
            "ensure_tag_source": lambda ctx: {"pubkey": "PUBKEY", "endpoints": [ow.MANIFEST_URL], "version": "0.7.11"},
            "gh_release": lambda shell, tag: release_json(),
            "download_installer": lambda ctx, asset, dest: (NSIS_SHA, []),
            "block_live_network": lambda ctx: setattr(ctx, "live_block", True) or setattr(ctx, "live_block_detail", {"rule": "x"}),
            "trust_ca": lambda ctx: setattr(ctx, "thumbprint", "AB" * 20),
            "map_hosts": lambda ctx, names: (setattr(ctx, "hosts_mapped", list(names)), setattr(ctx, "hosts_blackholed", ["arc.ai"])) and None,
            "cleanup": lambda ctx: None,
            "install_app": fake_install,
            "provenance_of_binary": fake_provenance,
            "run_released_case": fake_case,
            "find_native_exe": lambda explicit=None, env=None, exists=None: ("C:\\native\\native-updater-check.exe", ["C:\\native\\native-updater-check.exe"]),
            "run_native_case": fake_native_case,
        }
        patches.update(overrides)
        return patches

    def run_with(self, tmp, tier="both", cases="clean,cached-bait", **overrides):
        stack = [mock.patch.object(ow, name, value) for name, value in self.patched(tmp, **overrides).items()]
        stack.append(mock.patch.dict(sys.modules, {"ca": self.fake_ca_module(tmp)}))
        for patch in stack:
            patch.start()
            self.addCleanup(patch.stop)
        code = ow.cmd_run(self.args(tmp, tier, cases))
        self.assertEqual(code, 0)
        return json.loads((Path(tmp) / "evidence" / "result.json").read_text(encoding="utf-8"))

    def test_the_happy_path(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_with(tmp)
            self.assertEqual(result["verdict"], "PASS", result["notes"])
            self.assertEqual([t["result"] for t in result["tiers"].values()], ["PASS", "PASS"])
            self.assertEqual(len(result["cases"]), 4)
            self.assertEqual(result["manifest404_error_text"], ow.RELEASE_NOT_FOUND)
            self.assertEqual(result["manifest404_error_source"], "released_app")
            self.assertEqual(result["app"]["digest_match"], True)
            self.assertEqual(result["app"]["version_reported"], "0.7.11")
            self.assertEqual(result["plugin"]["version"], "2.10.1")
            self.assertEqual(result["isolation"]["hosts_mapped"], ["github.com", "api.github.com"])
            self.assertEqual(result["isolation"]["hosts_blackholed"], ["arc.ai"])
            self.assertTrue(result["isolation"]["live_block"])
            self.assertEqual(result["controls"][0]["verdict"], "PASS")
            self.assertTrue((Path(tmp) / "evidence" / "ca.crt").is_file())
            self.assertFalse(list((Path(tmp) / "evidence").rglob("*.key")))
            self.assertIn("ca.crt", result["shared_evidence_files"])
            self.assertEqual((Path(tmp) / "evidence" / "manifest404-error.txt").read_text(encoding="utf-8").strip(), ow.RELEASE_NOT_FOUND)
            self.assertIn("manifest404-error.txt", result["shared_evidence_files"])
            for case in result["cases"]:
                self.assertTrue(any(name.startswith("trigger-") for name in case["evidence_files"]), case["evidence_files"])
            self.assertTrue((Path(tmp) / "evidence" / "trigger-app-clean.json").is_file())
            self.assertEqual(result["tiers"]["native_check"]["binary"].get("path"), "C:\\native\\native-updater-check.exe")
            record = ssum.evaluate_os_result(result, (Path(tmp) / "evidence" / "result.json").read_bytes(), Path(tmp) / "evidence" / "result.json", Path(tmp) / "evidence")
            self.assertEqual(record["verdict"], "PASS", record["reasons"])

    def test_not_windows_writes_an_unproved_result_and_exits_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_with(tmp, IS_WINDOWS=False)
            self.assertEqual(result["verdict"], "UNPROVED")
            self.assertTrue(all(t["attempted"] and t["result"] == "UNPROVED" for t in result["tiers"].values()))
            self.assertTrue(any("Windows only" in note for note in result["notes"]))
            self.assertEqual(result["cases"], [])

    def test_a_digest_mismatch_stops_everything_before_the_isolation(self):
        with tempfile.TemporaryDirectory() as tmp:
            touched = []
            result = self.run_with(tmp, download_installer=lambda ctx, asset, dest: ("0" * 64, ["downloaded sha256 differs from the release digest"]),
                                   map_hosts=lambda ctx, names: touched.append(names), block_live_network=lambda ctx: touched.append("fw"))
            self.assertEqual(touched, [], "nothing is touched when the installer is not the released asset")
            self.assertIs(result["app"]["digest_match"], False)
            self.assertEqual(result["verdict"], "FAIL", "the downloaded installer is not the released asset")
            self.assertEqual(ssum.evaluate_os_result(result, b"{}", Path(tmp) / "r.json", Path(tmp))["verdict"], "FAIL")

    def test_cleanup_runs_even_when_a_step_blows_up(self):
        with tempfile.TemporaryDirectory() as tmp:
            cleaned = []
            result = self.run_with(tmp, trust_ca=lambda ctx: (_ for _ in ()).throw(RuntimeError("certutil refused")), cleanup=lambda ctx: cleaned.append(True))
            self.assertEqual(cleaned, [True])
            self.assertEqual(result["verdict"], "UNPROVED")
            self.assertIn("certutil refused", json.dumps(result["tiers"]))

    def test_the_released_tier_crashing_does_not_stop_the_native_tier(self):
        with tempfile.TemporaryDirectory() as tmp:
            def boom(ctx, asset, installer, **kwargs):
                raise OSError("installer vanished")

            result = self.run_with(tmp, install_app=boom)
            self.assertEqual(result["tiers"]["released_app"]["status"], "infeasible")
            self.assertIn("installer vanished", result["tiers"]["released_app"]["reason"])
            self.assertEqual(result["tiers"]["native_check"]["result"], "PASS")
            self.assertEqual(result["verdict"], "UNPROVED")
            self.assertEqual(result["manifest404_error_source"], "native_check")
            self.assertEqual(result["plugin"]["version"][:6], "2.10.1")

    def test_a_missing_exe_makes_the_released_tier_infeasible_and_says_why(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_with(tmp, install_app=lambda ctx, asset, installer, **kw: {"exe": None})
            self.assertEqual(result["tiers"]["released_app"]["status"], "infeasible")
            self.assertIn("arc-desktop.exe", result["tiers"]["released_app"]["reason"])
            self.assertEqual(result["verdict"], "UNPROVED")

    def test_a_missing_prebuilt_binary_is_reported_and_keeps_the_os_unproved(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_with(tmp, find_native_exe=lambda explicit=None, env=None, exists=None: (None, ["X:\\target\\release\\native-updater-check.exe"]))
            self.assertEqual(result["tiers"]["native_check"]["status"], "infeasible")
            self.assertIn("cargo build step failed", result["tiers"]["native_check"]["reason"])
            self.assertIn("target", result["tiers"]["native_check"]["reason"])
            self.assertEqual(result["tiers"]["released_app"]["result"], "PASS")
            self.assertEqual(result["verdict"], "UNPROVED")

    def test_released_only_run_is_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_with(tmp, tier="released_app")
            self.assertEqual(result["verdict"], "PASS")
            self.assertFalse(result["tiers"]["native_check"]["attempted"])

    def test_a_failing_case_makes_the_result_fail(self):
        with tempfile.TemporaryDirectory() as tmp:
            evidence = Path(tmp) / "evidence"

            def failing(ctx, name, state, settle_s=10.0):
                document = case_document(name, "released_app", ow.CASE_SCENARIO[name], evidence, verdict="FAIL" if name == "cached-bait" else "PASS")
                document["trigger_outcome"] = {"error_text": ow.RELEASE_NOT_FOUND}
                return document

            result = self.run_with(tmp, tier="released_app", run_released_case=failing)
            self.assertEqual(result["tiers"]["released_app"]["result"], "FAIL")
            self.assertEqual(result["verdict"], "FAIL")

    def test_key_material_in_the_evidence_directory_fails_the_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            def leaky(ctx, asset, installer, **kwargs):
                (ctx.evidence / "oops.pem").write_text("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n", encoding="utf-8")
                ctx.app_exe = APP_EXE
                return {"exe": APP_EXE}

            module = self.fake_ca_module(tmp)
            module.scan_for_private_keys = lambda root: [str(Path(root) / "oops.pem")]
            patches = self.patched(tmp, install_app=leaky)
            stack = [mock.patch.object(ow, name, value) for name, value in patches.items()] + [mock.patch.dict(sys.modules, {"ca": module})]
            for patch in stack:
                patch.start()
                self.addCleanup(patch.stop)
            self.assertEqual(ow.cmd_run(self.args(tmp)), 0)
            result = json.loads((Path(tmp) / "evidence" / "result.json").read_text(encoding="utf-8"))
            self.assertEqual(result["verdict"], "FAIL")
            self.assertTrue(any("PRIVATE KEY" in note for note in result["notes"]))

    def test_unknown_case_names_are_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(ow.cmd_run(self.args(tmp, cases="clean,bogus")), 2)

    def test_cached_bait_alone_warms_up_with_an_unreported_clean_launch(self):
        with tempfile.TemporaryDirectory() as tmp:
            launched = []
            evidence = Path(tmp) / "evidence"

            def tracking(ctx, name, state, settle_s=10.0):
                launched.append(name)
                return case_document(name, "released_app", ow.CASE_SCENARIO[name], evidence)

            result = self.run_with(tmp, tier="released_app", cases="cached-bait", run_released_case=tracking)
            self.assertEqual(launched, ["clean", "cached-bait"])
            self.assertEqual([c["name"] for c in result["cases"]], ["cached-bait"])
            self.assertTrue(any("warm-up" in note for note in result["notes"]))
            self.assertEqual(result["verdict"], "UNPROVED", "a run without the clean case cannot prove the clean case")


class FakeProc:
    def __init__(self, pid=4242, rc=None):
        self.pid, self.rc = pid, rc

    def poll(self):
        return self.rc


class FakePage:
    """Stands in for WebDriverPage / CdpPage: answers the probe and the trigger like a Tauri page would, and records what was asked."""

    def __init__(self, trigger_value, kind="msedgedriver", probe_value=None, on_trigger=None):
        self.kind = kind
        self.trigger_value = trigger_value
        self.probe_value = probe_value if probe_value is not None else {"ipc": True, "href": "http://tauri.localhost/", "title": "ARC Node", "app_version": "0.7.11", "app_version_error": None}
        self.on_trigger = on_trigger
        self.calls = []
        self.closed = False

    def evaluate_probe(self):
        self.calls.append("probe")
        return True, self.probe_value, None

    def evaluate_trigger(self):
        self.calls.append("trigger")
        if self.on_trigger:
            self.on_trigger()
        return True, self.trigger_value, None

    def close(self):
        self.closed = True


class FakeMitm:
    """Replaces MitmProcess: writes the request log the scenario would produce."""
    records = {"latest-404": [MANIFEST, REDIRECT], "bait-0.8.11": [MANIFEST]}
    fail_start = False
    instances = []

    def __init__(self, scenario, cert, key, log_path, ready_file, **kwargs):
        self.scenario, self.log_path = scenario, Path(log_path)
        FakeMitm.instances.append(self)
        self.stopped = False

    def start(self, timeout=30.0):
        if FakeMitm.fail_start:
            raise RuntimeError("cannot bind 443")
        self.log_path.write_text("".join(json.dumps(r) + "\n" for r in FakeMitm.records[self.scenario]), encoding="utf-8")

    def stop(self):
        self.stopped = True


class CaseRunnerTests(unittest.TestCase):
    ENV = {"USERPROFILE": "C:\\Users\\runneradmin", "APPDATA": "C:\\Users\\runneradmin\\AppData\\Roaming",
           "LOCALAPPDATA": "C:\\Users\\runneradmin\\AppData\\Local", "TEMP": "C:\\Users\\runneradmin\\AppData\\Local\\Temp"}
    HOME, WV2 = "C:\\arcw0\\home", "C:\\arcw0\\wv2"

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        FakeMitm.fail_start = False
        FakeMitm.instances = []
        self.evidence = Path(self.tmp.name) / "ev"
        self.evidence.mkdir()
        self.shell = ow.Shell(self.evidence / "steps.log")
        self.ctx = ow.Context(self.evidence, Path(self.tmp.name) / "w", self.shell, {}, env=self.ENV)
        self.ctx.work.mkdir()
        self.ctx.app_exe = APP_EXE
        self.ctx.install_dir = "C:\\arcw0\\app"
        self.ctx.ca_info = {"server_cert": "s.crt", "server_key": "s.key", "ca_cert": "ca.crt", "ca_sha256": "ab" * 32}
        self.killed = []
        self.launched = []

    def procs(self, extra=()):
        base = [ProcessTests.proc(1, 0, "System"), ProcessTests.proc(4242, 1, "arc-desktop.exe", APP_EXE), ProcessTests.proc(4300, 4242, "msedgewebview2.exe", "C:\\wv2\\msedgewebview2.exe")]
        return base + list(extra)

    def run_case(self, name="clean", trigger_value=None, procs_after=None, fs_after=None, outcomes=None, strategies=None, kind="msedgedriver", app_proc=False):
        """outcomes: strategy -> error text (the attempt fails) or None (it works)."""
        scenario = ow.CASE_SCENARIO[name]
        if trigger_value is None:
            trigger_value = ({"ok": False, "stage": "invoke", "error": ow.RELEASE_NOT_FOUND, "error_type": "string", "error_json": None} if name == "clean"
                             else {"ok": True, "value": {"rid": 3, "currentVersion": "0.7.11", "version": "0.8.11"}})
        outcomes = outcomes if outcomes is not None else {"msedgedriver": None}
        if strategies:
            self.ctx.strategies = list(strategies)
        state = {"triggered": False}
        pages = []

        def fake_try(ctx, strategy, tag, home, wv2_launch):
            self.launched.append((strategy, tag, wv2_launch))
            launch = ow.Launch(strategy, wv2_launch)
            error = outcomes.get(strategy, "not configured")
            if error:
                launch.error = error
                return launch
            page = FakePage(trigger_value, kind=kind if strategy == "msedgedriver" else "cdp", on_trigger=lambda: state.__setitem__("triggered", True))
            pages.append(page)
            launch.page = page
            launch.notes["fake"] = True
            if app_proc:
                launch.app_proc = FakeProc(4242)
            return launch

        after_list = procs_after if procs_after is not None else self.procs([ProcessTests.proc(4400, 4300, "msedgewebview2.exe", created="9")])

        def fake_snapshot(shell):
            if state["triggered"]:
                return None if after_list == "missing" else after_list
            return self.procs()

        snapshots = iter([{}, fs_after or {}])
        real_sleep = time.sleep
        real_poller = ow.Poller
        raw = self.ctx.work / ("app-app-%s.raw.log" % name)
        raw.write_bytes(b"connecting to 149.28.32.76:9090 failed\n")
        with mock.patch.object(ow, "MitmProcess", FakeMitm), mock.patch.object(ow, "try_strategy", fake_try), \
                mock.patch.object(ow, "snapshot_processes", side_effect=fake_snapshot), \
                mock.patch.object(ow, "take_snapshot", side_effect=lambda roots, hash_limit=1 << 20: {} if hash_limit == 0 else next(snapshots)), \
                mock.patch.object(ow, "kill_app", side_effect=lambda ctx, pid, wv2: self.killed.append((pid, wv2))), \
                mock.patch.object(ow, "Poller", lambda roots, out, interval=1.0: real_poller(roots, out, interval=0.01)), \
                mock.patch.object(ow.time, "sleep", lambda seconds: real_sleep(0.1)):
            case = ow.run_released_case(self.ctx, name, {"home": self.HOME, "wv2": self.WV2})
        return case, pages

    def test_a_clean_run_records_everything_and_passes(self):
        case, pages = self.run_case("clean")
        self.assertEqual(case["verdict"], "PASS", case["reasons"])
        self.assertEqual(case["criteria"], {k: True for k in ow.CRITERIA})
        self.assertEqual(case["trigger_outcome"]["error_text"], ow.RELEASE_NOT_FOUND)
        self.assertEqual(case["trigger_outcome"]["path"], "msedgedriver", "the result says which path ran")
        self.assertEqual(case["requests"]["total"], 2)
        self.assertEqual(case["page_probe"]["value"]["app_version"], "0.7.11")
        self.assertEqual(pages[0].calls, ["probe", "trigger"])
        self.assertTrue(pages[0].closed)
        for name in case["evidence_files"]:
            self.assertTrue((self.evidence / name).is_file(), name)
        self.assertEqual(sorted(case["evidence_files"]), sorted([
            "procs-app-clean-msedgedriver-launched.txt", "requests-app-clean.jsonl", "writes-app-clean.jsonl", "fs-app-clean-before.json", "fs-app-clean-after.json",
            "fs-app-clean-diff.json", "procs-app-clean-prelaunch.txt", "procs-app-clean-before.txt", "procs-app-clean-after.txt", "app-app-clean.log"]))
        self.assertTrue(FakeMitm.instances[0].stopped)
        writes = ow.read_jsonl(self.evidence / "writes-app-clean.jsonl")
        self.assertEqual([r["event"] for r in writes if r["event"] in ("start", "stop")], ["start", "stop"])
        self.assertEqual([r["label"] for r in writes if r["event"] == "mark"], ["trigger_begin", "trigger_end"])
        self.assertGreaterEqual(case["file_changes"]["poller_cycles"], 2)
        app_log = (self.evidence / "app-app-clean.log").read_text(encoding="utf-8")
        self.assertIn("149.28.x.x", app_log)
        self.assertNotIn("149.28.32.76", app_log)
        self.assertEqual(case["launch_attempts"][0]["strategy"], "msedgedriver")
        self.assertTrue(case["launch_attempts"][0]["ok"])
        self.assertFalse(case["launch_attempts"][0]["remote_debugging_in_webview2_command_line"], "recorded, so a refused flag is visible without reading the lists")
        self.assertEqual(case["launch_attempts"][0]["webview2_process_count"], 1)

    def test_the_bait_case_passes_only_when_the_update_was_really_offered(self):
        case, _ = self.run_case("cached-bait")
        self.assertEqual(case["verdict"], "PASS", case["reasons"])
        self.assertEqual(case["requests"]["total"], 1)
        case, _ = self.run_case("cached-bait", trigger_value={"ok": False, "stage": "invoke", "error": "no update"})
        self.assertEqual(case["verdict"], "UNPROVED")

    def test_the_driver_apps_pid_is_found_from_the_process_list(self):
        case, _ = self.run_case("clean", app_proc=False)
        self.assertEqual(case["verdict"], "PASS", case["reasons"])
        case, _ = self.run_case("clean", app_proc=True, outcomes={"msedgedriver": None})
        self.assertEqual(case["verdict"], "PASS", case["reasons"])

    def test_the_cdp_fallback_runs_when_the_driver_fails_and_the_attempts_are_recorded(self):
        case, pages = self.run_case("clean", outcomes={"msedgedriver": "RuntimeError: session not created: this version of Microsoft Edge WebDriver only supports", "cdp-env": None})
        self.assertEqual(case["verdict"], "PASS", case["reasons"])
        self.assertEqual(case["trigger_outcome"]["path"], "cdp")
        self.assertEqual([a["strategy"] for a in case["launch_attempts"]], ["msedgedriver", "cdp-env"])
        self.assertFalse(case["launch_attempts"][0]["ok"])
        self.assertIn("session not created", case["launch_attempts"][0]["error"])
        self.assertEqual(self.ctx.winning_strategy, "cdp-env")
        self.assertIn("procs-app-clean-msedgedriver-launched.txt", case["evidence_files"])
        self.assertIn("procs-app-clean-cdp-env-launched.txt", case["evidence_files"])
        user_data = [a["user_data_folder"] for a in case["launch_attempts"]]
        self.assertEqual(len(set(user_data)), 2, "a distinct WebView2 user data folder per launch")

    def test_no_strategy_working_is_unproved_with_every_reason(self):
        case, _ = self.run_case("clean", outcomes={"msedgedriver": "no driver", "cdp-env": "no port", "cdp-registry": "no port either"})
        self.assertEqual(case["verdict"], "UNPROVED")
        self.assertFalse(case["trigger_outcome"]["ran"])
        for reason in ("no driver", "no port", "no port either"):
            self.assertIn(reason, case["trigger_outcome"]["error_text"])
        self.assertEqual(len(case["launch_attempts"]), 3)
        self.assertTrue((self.evidence / "fs-app-clean-after.json").is_file())
        self.assertGreaterEqual(len([k for k in self.killed if k[0] is None]), 3, "everything is cleaned between attempts and at the end")

    def test_the_winning_strategy_is_tried_first_in_the_next_case(self):
        self.run_case("clean", outcomes={"msedgedriver": "no driver", "cdp-env": None})
        self.launched.clear()
        self.run_case("cached-bait", outcomes={"msedgedriver": "no driver", "cdp-env": None})
        self.assertEqual([entry[0] for entry in self.launched][0], "cdp-env")

    def test_a_server_that_cannot_start_is_unproved_and_cleans_up(self):
        FakeMitm.fail_start = True
        case, _ = self.run_case("clean")
        self.assertEqual(case["verdict"], "UNPROVED")
        self.assertIn("cannot bind 443", case["trigger_outcome"]["error_text"])
        self.assertTrue(self.killed)

    def test_a_payload_dropped_in_temp_fails_no_new_files_and_no_install(self):
        dropped = "C:\\Users\\runneradmin\\AppData\\Local\\Temp\\ARC Node-0.8.11-updater-x\\ARC.Node_0.8.11_x64-setup.exe"
        case, _ = self.run_case("clean", fs_after={dropped: {"type": "file", "size": 5, "mtime_ns": 1}})
        self.assertEqual(case["verdict"], "FAIL")
        self.assertFalse(case["criteria"]["no_new_files"])
        self.assertFalse(case["criteria"]["no_install"])
        self.assertEqual(case["file_changes"]["unexpected"][0]["path"], dropped)

    def test_a_changed_install_directory_fails_no_install(self):
        changed = {"C:\\arcw0\\app\\arc-desktop.exe": {"type": "file", "size": 9, "mtime_ns": 2}}
        case, _ = self.run_case("clean", fs_after=changed)
        self.assertEqual(case["verdict"], "FAIL")
        self.assertFalse(case["criteria"]["no_install"])
        self.assertEqual(case["file_changes"]["install_dir_changes"], list(changed))

    def test_an_installer_process_fails_no_new_app_launch(self):
        installer = ProcessTests.proc(5000, 4242, "ARC.Node_0.8.11_x64-setup.exe", "C:\\Temp\\ARC.Node_0.8.11_x64-setup.exe", created="7")
        case, _ = self.run_case("clean", procs_after=self.procs([installer]))
        self.assertEqual(case["verdict"], "FAIL")
        self.assertFalse(case["criteria"]["no_new_app_launch"])

    def test_the_labs_own_helpers_are_not_a_launch(self):
        helper = ProcessTests.proc(6000, 1, "powershell.exe", "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe", "powershell -Command Get-CimInstance", created="8")
        case, _ = self.run_case("clean", procs_after=self.procs([helper]))
        self.assertEqual(case["verdict"], "PASS", case["reasons"])
        self.assertEqual(len(case["process_changes"]["lab_helpers"]), 1)

    def test_missing_process_lists_leave_it_unproved(self):
        case, _ = self.run_case("clean", procs_after="missing")
        self.assertEqual(case["verdict"], "UNPROVED")
        self.assertIsNone(case["criteria"]["no_new_app_launch"])
        self.assertIsNone(case["criteria"]["no_install"])


class FakeResponse:
    def __init__(self, status, body):
        self.status, self._body = status, body

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False

    def read(self):
        return self._body if isinstance(self._body, bytes) else json.dumps(self._body).encode()


class W3CTests(unittest.TestCase):
    """The WebDriver client against fake openers: nothing here touches a network or starts a process."""

    def opener(self, answers):
        calls = []

        def open_url(request, timeout=None):
            calls.append((request.get_method(), request.full_url, json.loads(request.data.decode()) if request.data else None, timeout))
            answer = answers.pop(0)
            if isinstance(answer, Exception):
                raise answer
            return FakeResponse(*answer)

        open_url.calls = calls
        return open_url

    def test_call_sends_json_and_parses_the_answer(self):
        opener = self.opener([(200, {"value": {"sessionId": "S1", "capabilities": {"browserName": "webview2"}}})])
        status, payload = ow.w3c_call("http://127.0.0.1:9515", "POST", "/session", {"a": 1}, 12.0, opener)
        self.assertEqual((status, payload["value"]["sessionId"]), (200, "S1"))
        self.assertEqual(opener.calls[0], ("POST", "http://127.0.0.1:9515/session", {"a": 1}, 12.0))

    def test_http_errors_and_non_json_bodies_are_returned_not_raised(self):
        import io
        import urllib.error
        error = urllib.error.HTTPError("http://x", 500, "boom", {}, io.BytesIO(json.dumps({"value": {"error": "unknown error", "message": "bad"}}).encode()))
        status, payload = ow.w3c_call("http://x", "GET", "/status", opener=self.opener([error]))
        self.assertEqual(status, 500)
        self.assertEqual(payload["value"]["error"], "unknown error")
        status, payload = ow.w3c_call("http://x", "GET", "/status", opener=self.opener([(200, b"<html>")]))
        self.assertEqual((status, payload), (200, {"raw": "<html>"}))

    def test_value_unwraps_success_and_raises_w3c_errors(self):
        self.assertEqual(ow.w3c_value(200, {"value": {"ready": True}}), {"ready": True})
        self.assertIsNone(ow.w3c_value(200, {"value": None}))
        with self.assertRaises(ow.W3CError) as caught:
            ow.w3c_value(500, {"value": {"error": "session not created", "message": "This version of Microsoft Edge WebDriver only supports Microsoft Edge version 152"}})
        self.assertIn("session not created", str(caught.exception))
        self.assertIn("only supports", str(caught.exception))
        with self.assertRaises(ow.W3CError):
            ow.w3c_value(200, {"value": {"error": "javascript error", "message": "x is not defined"}})
        with self.assertRaises(ow.W3CError):
            ow.w3c_value(404, {"raw": "not found"})

    def test_capabilities_are_what_tauri_driver_sends_on_windows(self):
        caps = ow.edge_capabilities("C:\\arcw0\\app\\arc-desktop.exe")["capabilities"]["alwaysMatch"]
        self.assertEqual(caps["browserName"], "webview2")
        self.assertEqual(caps["ms:edgeOptions"], {"binary": "C:\\arcw0\\app\\arc-desktop.exe", "webviewOptions": {}})

    def test_the_async_scripts_only_call_the_updater_check_and_the_version(self):
        script = ow.trigger_script_async()
        self.assertIn("arguments[arguments.length - 1]", script)
        self.assertIn("plugin:updater|check", script)
        for forbidden in ("download", "install", "relaunch", "fetch(", "XMLHttpRequest"):
            self.assertNotIn(forbidden, script)
        probe = ow.probe_script_async()
        self.assertIn("plugin:app|version", probe)
        self.assertNotIn("updater", probe)

    def test_the_page_session_lifecycle(self):
        opener = self.opener([
            (200, {"value": {"sessionId": "S9", "capabilities": {"browserName": "webview2", "browserVersion": "153.0", "msedge": {"x": 1}}}}),
            (200, {"value": None}),                                                                       # timeouts
            (200, {"value": {"ipc": True, "app_version": "0.7.11"}}),                                    # probe
            (200, {"value": {"ok": False, "stage": "invoke", "error": ow.RELEASE_NOT_FOUND}}),           # trigger
            (200, {"value": None}),                                                                       # delete
        ])
        page = ow.WebDriverPage("http://127.0.0.1:9515", opener)
        caps = page.open("C:\\app\\arc-desktop.exe", timeout=99)
        self.assertEqual(caps["browserVersion"], "153.0")
        self.assertEqual(page.session_id, "S9")
        self.assertEqual(opener.calls[1][1], "http://127.0.0.1:9515/session/S9/timeouts")
        self.assertGreaterEqual(opener.calls[1][2]["script"], 60000)
        self.assertEqual(page.evaluate_probe(), (True, {"ipc": True, "app_version": "0.7.11"}, None))
        ok, value, error = page.evaluate_trigger()
        self.assertEqual((ok, value["error"], error), (True, ow.RELEASE_NOT_FOUND, None))
        execute = opener.calls[3]
        self.assertEqual(execute[1], "http://127.0.0.1:9515/session/S9/execute/async")
        self.assertIn("plugin:updater|check", execute[2]["script"])
        self.assertEqual(execute[2]["args"], [])
        page.close()
        self.assertEqual((opener.calls[4][0], opener.calls[4][1]), ("DELETE", "http://127.0.0.1:9515/session/S9"))
        self.assertIsNone(page.session_id)
        page.close()
        self.assertEqual(len(opener.calls), 5, "closing twice does nothing")

    def test_session_creation_failures_surface_with_the_drivers_message(self):
        opener = self.opener([(500, {"value": {"error": "session not created", "message": "version mismatch 153 vs 152"}})])
        page = ow.WebDriverPage("http://x", opener)
        with self.assertRaises(ow.W3CError) as caught:
            page.open("app.exe")
        self.assertIn("version mismatch", str(caught.exception))
        self.assertIsNone(page.session_id)
        page.close()                         # nothing to close, and no call is made
        self.assertEqual(len(opener.calls), 1)

    def test_a_missing_session_id_is_an_error(self):
        page = ow.WebDriverPage("http://x", self.opener([(200, {"value": {"capabilities": {}}})]))
        with self.assertRaises(ow.W3CError):
            page.open("app.exe")

    def test_script_errors_become_a_failed_evaluation_not_an_exception(self):
        opener = self.opener([(500, {"value": {"error": "script timeout", "message": "script timeout after 90000 ms"}})])
        page = ow.WebDriverPage("http://x", opener)
        page.session_id = "S"
        ok, value, error = page.evaluate_trigger()
        self.assertEqual((ok, value), (False, None))
        self.assertIn("script timeout", error)
        opener = self.opener([OSError("connection reset")])
        page = ow.WebDriverPage("http://x", opener)
        page.session_id = "S"
        self.assertFalse(page.evaluate_trigger()[0])

    def test_driver_discovery(self):
        default = ow.EDGE_DRIVER_DEFAULT
        self.assertEqual(ow.find_edge_driver({}, exists=lambda p: p == default, which=lambda n: None), default)
        found = ow.find_edge_driver({"EdgeWebDriver": "D:\\drivers\\"}, exists=lambda p: p == "D:\\drivers\\msedgedriver.exe", which=lambda n: None)
        self.assertEqual(found, "D:\\drivers\\msedgedriver.exe")
        self.assertEqual(ow.find_edge_driver({}, exists=lambda p: False, which=lambda n: "C:\\path\\msedgedriver.exe"), "C:\\path\\msedgedriver.exe")
        self.assertIsNone(ow.find_edge_driver({}, exists=lambda p: False, which=lambda n: None))

    def test_free_port_uses_an_os_chosen_port_and_closes_the_socket(self):
        class Sock:
            closed = False

            def bind(self, address):
                self.address = address

            def getsockname(self):
                return ("127.0.0.1", 54321)

            def close(self):
                Sock.closed = True

        self.assertEqual(ow.free_port(bind=Sock), 54321)
        self.assertTrue(Sock.closed)


class DevToolsPortTests(unittest.TestCase):
    def test_the_active_port_file_is_read_from_the_webview2_folder(self):
        files = {"C:\\wv\\EBWebView\\DevToolsActivePort": "40123\n/devtools/browser/abc\n"}

        def read(path):
            if path in files:
                return files[path]
            raise FileNotFoundError(path)

        self.assertEqual(ow.read_devtools_port("C:\\wv\\", read), 40123)
        files.clear()
        files["C:\\wv\\DevToolsActivePort"] = "9333\n"
        self.assertEqual(ow.read_devtools_port("C:\\wv", read), 9333)
        files.clear()
        self.assertIsNone(ow.read_devtools_port("C:\\wv", read))
        files["C:\\wv\\DevToolsActivePort"] = "garbage"
        self.assertIsNone(ow.read_devtools_port("C:\\wv", read))
        files["C:\\wv\\DevToolsActivePort"] = "0\n"
        self.assertIsNone(ow.read_devtools_port("C:\\wv", read), "port 0 is a request, not an answer")

    def test_waiting_gives_up_with_the_reason(self):
        clock = {"t": 0.0}
        port, why = ow.wait_for_devtools_port("C:\\wv", 5, read=lambda p: (_ for _ in ()).throw(OSError()), sleep=lambda s: clock.__setitem__("t", clock["t"] + s), clock=lambda: clock["t"])
        self.assertIsNone(port)
        self.assertIn("DevToolsActivePort never appeared", why)
        answers = iter([OSError(), OSError(), "55555\n"])

        def read(path):
            value = next(answers)
            if isinstance(value, Exception):
                raise value
            return value

        clock["t"] = 0.0
        port, why = ow.wait_for_devtools_port("C:\\wv", 60, read=read, sleep=lambda s: clock.__setitem__("t", clock["t"] + s), clock=lambda: clock["t"])
        self.assertEqual((port, why), (55555, "ok"))

    def test_registry_override_commands(self):
        argv = ow.registry_override_argv("--remote-debugging-port=0 --remote-allow-origins=*")
        self.assertEqual(argv[:2], ["reg", "add"])
        self.assertIn("HKCU\\SOFTWARE\\Policies\\Microsoft\\Edge\\WebView2\\AdditionalBrowserArguments", argv)
        self.assertEqual(argv[argv.index("/v") + 1], "arc-desktop.exe")
        self.assertEqual(argv[argv.index("/d") + 1], "--remote-debugging-port=0 --remote-allow-origins=*")
        self.assertEqual(ow.registry_override_remove_argv()[:2], ["reg", "delete"])
        self.assertIn("/f", ow.registry_override_remove_argv())

    def test_app_pid_and_leftovers_and_command_lines(self):
        procs = [ProcessTests.proc(10, 1, "msedgedriver.exe", "C:\\SeleniumWebDrivers\\EdgeDriver\\msedgedriver.exe"),
                 ProcessTests.proc(20, 10, "arc-desktop.exe", APP_EXE, created="2"), ProcessTests.proc(21, 10, "arc-desktop.exe", APP_EXE, created="5"),
                 ProcessTests.proc(30, 21, "msedgewebview2.exe", "C:\\wv\\msedgewebview2.exe", "msedgewebview2.exe --user-data-dir=C:\\arcw0\\wv2\\app-clean\\EBWebView --remote-debugging-port=0"),
                 ProcessTests.proc(31, 1, "msedgewebview2.exe", "C:\\wv\\msedgewebview2.exe", "msedgewebview2.exe --user-data-dir=C:\\other"),
                 ProcessTests.proc(40, 1, "svchost.exe")]
        self.assertEqual(ow.find_app_pid(procs, "c:/arcw0/app/ARC-DESKTOP.exe".replace("/", "\\")), 21, "the newest instance")
        self.assertIsNone(ow.find_app_pid(procs, "C:\\nowhere\\arc-desktop.exe"))
        self.assertIsNone(ow.find_app_pid(None, APP_EXE))
        left = ow.leftover_processes(procs, ["C:\\arcw0\\wv2"])
        self.assertEqual(len(left), 4, left)          # driver, two app instances, the one webview2 helper with our folder
        self.assertEqual(ow.leftover_processes(None, ["x"]), [])
        lines = ow.webview_command_lines(procs)
        self.assertEqual(len(lines), 2)
        self.assertIn("--remote-debugging-port=0", lines[0])


class TryStrategyTests(unittest.TestCase):
    ENV = {"USERPROFILE": "C:\\Users\\runneradmin", "PATH": "p"}

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.shell = mock.Mock()
        self.shell.run.return_value = ow.CmdResult(0, "")
        self.shell.log = lambda line: None
        self.ctx = ow.Context(Path(self.tmp.name) / "ev", Path(self.tmp.name) / "w", self.shell, {}, env=self.ENV)
        self.ctx.work.mkdir(parents=True)
        self.ctx.app_exe = APP_EXE
        self.ctx.install_dir = "C:\\arcw0\\app"
        self.wv2 = str(Path(self.tmp.name) / "wv2" / "app-clean-x")

    def test_msedgedriver_strategy_starts_the_driver_with_the_sandbox_environment_and_opens_a_session(self):
        popen_calls = []

        class Driver:
            def poll(self):
                return None

            def kill(self):
                pass

        def popen(argv, **kwargs):
            popen_calls.append((argv, kwargs))
            return Driver()

        statuses = iter([(503, {}), (200, {"value": {"ready": True}})])
        sessions = []

        class Page:
            session_id = None

            def __init__(self, base):
                self.base = base

            def open(self, app_exe, timeout=120.0):
                sessions.append((self.base, app_exe, timeout))
                self.session_id = "S"
                return {"browserVersion": "153"}

            def close(self):
                pass

        with mock.patch.object(ow, "find_edge_driver", return_value="C:\\drv\\msedgedriver.exe"), mock.patch.object(ow, "free_port", return_value=45678), \
                mock.patch.object(ow, "WebDriverPage", Page), mock.patch.object(ow, "w3c_call", side_effect=lambda *a, **k: next(statuses)), \
                mock.patch.object(ow.time, "sleep", lambda s: None):
            launch = ow.try_strategy(self.ctx, "msedgedriver", "app-clean", "C:\\arcw0\\home", self.wv2, popen=popen)
        self.addCleanup(ow.close_launch, self.ctx, launch, "C:\\arcw0\\wv2")
        self.assertTrue(launch.ok, launch.error)
        self.assertEqual(sessions, [("http://127.0.0.1:45678", APP_EXE, 120)])
        argv, kwargs = popen_calls[0]
        self.assertEqual(argv[0], "C:\\drv\\msedgedriver.exe")
        self.assertIn("--port=45678", argv)
        env = kwargs["env"]
        self.assertEqual((env["HOME"], env["WEBVIEW2_USER_DATA_FOLDER"], env["USERPROFILE"]), ("C:\\arcw0\\home", self.wv2, "C:\\Users\\runneradmin"))
        self.assertNotIn("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", env, "msedgedriver sets its own debugging arguments")
        ow.close_launch(self.ctx, launch, "C:\\arcw0\\wv2")

    def test_a_session_that_will_not_open_is_an_error_with_the_message_and_is_cleaned_up(self):
        class Driver:
            def poll(self):
                return None

            def kill(self):
                self.killed = True

        driver = Driver()

        class Page:
            session_id = None

            def __init__(self, base):
                pass

            def open(self, app_exe, timeout=120.0):
                raise ow.W3CError("session not created", "This version of Microsoft Edge WebDriver only supports Edge 152")

            def close(self):
                pass

        with mock.patch.object(ow, "find_edge_driver", return_value="d.exe"), mock.patch.object(ow, "free_port", return_value=1), mock.patch.object(ow, "WebDriverPage", Page), \
                mock.patch.object(ow, "w3c_call", return_value=(200, {})), mock.patch.object(ow.time, "sleep", lambda s: None):
            launch = ow.try_strategy(self.ctx, "msedgedriver", "t", "h", self.wv2, popen=lambda argv, **kw: driver)
        self.addCleanup(ow.close_launch, self.ctx, launch, "C:\\arcw0\\wv2")
        self.assertFalse(launch.ok)
        self.assertIn("session not created", launch.error)
        ow.close_launch(self.ctx, launch, "C:\\arcw0\\wv2")
        self.assertTrue(driver.killed)

    def test_the_driver_exiting_or_missing_is_reported(self):
        with mock.patch.object(ow, "find_edge_driver", return_value=None):
            launch = ow.try_strategy(self.ctx, "msedgedriver", "t", "h", self.wv2)
        self.assertIn("msedgedriver.exe not found", launch.error)

        class Dead:
            def poll(self):
                return 3

            def kill(self):
                pass

        with mock.patch.object(ow, "find_edge_driver", return_value="d.exe"), mock.patch.object(ow, "free_port", return_value=1):
            launch = ow.try_strategy(self.ctx, "msedgedriver", "t", "h", self.wv2, popen=lambda argv, **kw: Dead())
        self.addCleanup(ow.close_launch, self.ctx, launch, "C:\\arcw0\\wv2")
        self.assertIn("exited with code 3", launch.error)

    def test_driver_never_ready_is_reported(self):
        class Alive:
            def poll(self):
                return None

            def kill(self):
                pass

        clock = {"t": 0.0}
        with mock.patch.object(ow, "find_edge_driver", return_value="d.exe"), mock.patch.object(ow, "free_port", return_value=1), \
                mock.patch.object(ow, "w3c_call", side_effect=OSError("refused")), mock.patch.object(ow.time, "sleep", lambda s: clock.__setitem__("t", clock["t"] + s)), \
                mock.patch.object(ow.time, "time", lambda: clock["t"]):
            launch = ow.try_strategy(self.ctx, "msedgedriver", "t", "h", self.wv2, popen=lambda argv, **kw: Alive())
        self.addCleanup(ow.close_launch, self.ctx, launch, "C:\\arcw0\\wv2")
        self.assertIn("did not answer /status", launch.error)

    def cdp_launch(self, strategy, port=40001, devtools_port=None, target=True):
        popen_calls = []

        class App:
            pid = 777

            def poll(self):
                return None

        def popen(argv, **kwargs):
            popen_calls.append((argv, kwargs))
            return App()

        class Ws:
            def __init__(self, host, port, path, timeout=10.0):
                self.target = (host, port, path)

            def connect(self):
                pass

            def close(self):
                pass

        page_target = {"webSocketDebuggerUrl": "ws://127.0.0.1:40001/devtools/page/A", "url": "http://tauri.localhost/"} if target else None
        with mock.patch.object(ow, "wait_for_devtools_port", return_value=(devtools_port if devtools_port is not None else port, "ok") if devtools_port != 0 else (None, "never appeared")), \
                mock.patch.object(ow, "wait_for_page", return_value=(page_target, "ok" if target else "no page")), mock.patch.object(ow, "WebSocketClient", Ws):
            launch = ow.try_strategy(self.ctx, strategy, "app-clean", "C:\\arcw0\\home", self.wv2, popen=popen)
        self.addCleanup(ow.close_launch, self.ctx, launch, "C:\\arcw0\\wv2")
        return launch, popen_calls

    def test_the_env_strategy_asks_for_an_os_chosen_debugging_port_and_finds_it_in_the_user_data_folder(self):
        launch, calls = self.cdp_launch("cdp-env")
        self.assertTrue(launch.ok, launch.error)
        self.assertEqual(launch.page.kind, "cdp")
        env = calls[0][1]["env"]
        self.assertEqual(env["WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS"], "--remote-debugging-port=0 --remote-allow-origins=*")
        self.assertEqual((env["HOME"], env["WEBVIEW2_USER_DATA_FOLDER"], env["USERPROFILE"]), ("C:\\arcw0\\home", self.wv2, "C:\\Users\\runneradmin"))
        self.assertEqual(launch.notes["devtools_port"], 40001)
        self.assertFalse(self.ctx.registry_override)
        ow.close_launch(self.ctx, launch, "C:\\arcw0\\wv2")

    def test_the_registry_strategy_sets_the_override_and_close_removes_it(self):
        launch, calls = self.cdp_launch("cdp-registry")
        self.assertTrue(launch.ok, launch.error)
        self.assertTrue(self.ctx.registry_override)
        added = [c[0][0] for c in self.shell.run.call_args_list if c[0][0][:2] == ["reg", "add"]]
        self.assertEqual(len(added), 1)
        ow.close_launch(self.ctx, launch, "C:\\arcw0\\wv2")
        removed = [c[0][0] for c in self.shell.run.call_args_list if c[0][0][:2] == ["reg", "delete"]]
        self.assertEqual(len(removed), 1)
        self.assertFalse(self.ctx.registry_override)
        ow.cleanup(self.ctx)
        self.assertEqual(len([c for c in self.shell.run.call_args_list if c[0][0][:2] == ["reg", "delete"]]), 1, "nothing left to remove")

    def test_a_registry_write_that_fails_stops_that_strategy(self):
        self.shell.run.return_value = ow.CmdResult(1, "Access is denied")
        launch, calls = self.cdp_launch("cdp-registry")
        self.assertFalse(launch.ok)
        self.assertIn("reg add failed", launch.error)
        self.assertEqual(calls, [], "the app is not started when the override could not be set")

    def test_no_debugging_port_is_a_recorded_failure(self):
        launch, _ = self.cdp_launch("cdp-env", devtools_port=0)
        self.assertFalse(launch.ok)
        self.assertIn("never appeared", launch.error)
        launch, _ = self.cdp_launch("cdp-env", target=False)
        self.assertIn("page target never appeared", launch.error)

    def test_unknown_strategy(self):
        launch = ow.try_strategy(self.ctx, "telepathy", "t", "h", self.wv2)
        self.assertIn("unknown strategy", launch.error)

    def test_closing_a_launch_stops_everything_it_started(self):
        class Proc:
            def __init__(self, pid):
                self.pid, self.killed = pid, False

            def poll(self):
                return None

            def kill(self):
                self.killed = True

        launch = ow.Launch("cdp-env", self.wv2)
        launch.app_proc, launch.driver_proc = Proc(55), Proc(56)
        page = mock.Mock()
        launch.page = page
        handle = mock.Mock()
        launch.handles = [handle]
        ow.close_launch(self.ctx, launch, "C:\\arcw0\\wv2")
        page.close.assert_called_once()
        self.assertTrue(launch.driver_proc.killed)
        handle.close.assert_called_once()
        taskkills = [c[0][0] for c in self.shell.run.call_args_list if c[0][0][0] == "taskkill"]
        self.assertIn(["taskkill", "/F", "/T", "/PID", "55"], taskkills)


class NativeTierCriteriaTests(unittest.TestCase):
    """The first CI run failed no_new_app_launch in the native tier only because the lab's own PowerShell snapshot was counted."""

    def base(self):
        return [ProcessTests.proc(1, 0, "System"), ProcessTests.proc(900, 1, "python.exe", "C:\\hostedtoolcache\\python.exe")]

    def native(self, extra, **kwargs):
        return ow.process_violations(self.base(), self.base() + extra, 0, [], ["C:\\native\\native-updater-check.exe"], **kwargs)

    def test_the_labs_helpers_are_listed_but_do_not_count(self):
        helpers = [ProcessTests.proc(901, 900, "powershell.exe", "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe", "powershell -Command Get-CimInstance", created="2"),
                   ProcessTests.proc(902, 901, "conhost.exe", "C:\\Windows\\System32\\conhost.exe", created="3"),
                   ProcessTests.proc(903, 900, "python.exe", created="4"), ProcessTests.proc(904, 900, "native-updater-check.exe", "C:\\native\\native-updater-check.exe", created="5"),
                   ProcessTests.proc(905, 900, "curl.exe", created="6"), ProcessTests.proc(906, 900, "certutil.exe", created="7")]
        found = self.native(helpers)
        self.assertEqual(found["new_app_launches"] + found["installers"] + found["other_in_app_tree"], [])
        self.assertEqual(len(found["lab_helpers"]), 5, "the checker itself is ignored by path, the other five are recorded as lab helpers")

    def test_no_app_tree_exists_in_the_native_tier(self):
        everything = [ProcessTests.proc(910, 0, "SomethingElse.exe", "C:\\x\\something.exe", created="2")]
        found = self.native(everything)
        self.assertEqual(found["other_in_app_tree"], [], "pid 0 means there is no app; children of the idle process are not an app tree")
        self.assertEqual(len(found["unrelated"]), 1)

    def test_a_new_app_an_installer_or_an_updater_is_still_flagged(self):
        flagged = self.native([ProcessTests.proc(920, 1, "arc-desktop.exe", APP_EXE, created="2")])
        self.assertEqual(len(flagged["new_app_launches"]), 1)
        flagged = self.native([ProcessTests.proc(921, 1, "ARC.Node_0.8.11_x64-setup.exe", "C:\\Temp\\ARC.Node_0.8.11_x64-setup.exe", created="3")])
        self.assertEqual(len(flagged["installers"]), 1)
        flagged = self.native([ProcessTests.proc(922, 1, "Update.exe", "C:\\Users\\x\\AppData\\Local\\ARC Node\\updater\\Update.exe", created="4")])
        self.assertEqual(len(flagged["installers"]), 1)

    def test_webview_helpers_carrying_the_launch_folder_are_the_apps_own_even_outside_its_tree(self):
        helper = ProcessTests.proc(930, 77, "msedgewebview2.exe", "C:\\wv\\msedgewebview2.exe", "msedgewebview2.exe --user-data-dir=C:\\arcw0\\wv2\\app-clean-cdp-env\\EBWebView", created="2")
        before = [ProcessTests.proc(1, 0, "System"), ProcessTests.proc(100, 1, "arc-desktop.exe", APP_EXE)]
        found = ow.process_violations(before, before + [helper], 100, [], webview_markers=["C:\\arcw0\\wv2"])
        self.assertEqual(found["other_in_app_tree"] + found["unrelated"], [])
        found = ow.process_violations(before, before + [helper], 100, [])
        self.assertEqual(len(found["unrelated"]), 1, "without the marker an orphan helper is only recorded")

    def test_the_native_case_with_a_powershell_snapshot_passes(self):
        error = {"outcome": "error", "error": ow.RELEASE_NOT_FOUND}
        trigger = ow.native_trigger(error, 0, "latest-404")
        procs = ow.process_violations(self.base(), self.base() + [ProcessTests.proc(901, 900, "powershell.exe", created="2")], 0, [], ["C:\\native\\n.exe"])
        result = ow.evaluate_case("latest-404", trigger, requests_for(MANIFEST, REDIRECT), NO_FS, procs, True, [])
        self.assertEqual(result["verdict"], "PASS", result["reasons"])


class PollerFinishTests(unittest.TestCase):
    def test_a_fast_check_still_gets_at_least_two_full_cycles(self):
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "writes.jsonl"
            poller = ow.Poller([(str(Path(tmp) / "watched"), False)], log, interval=0.05)
            poller.start()
            cycles = poller.finish(min_cycles=2, extra_cycles=2)
            records = ow.read_jsonl(log)
        self.assertGreaterEqual(cycles, 2)
        self.assertEqual(records[-1]["event"], "stop")
        self.assertEqual(records[-1]["cycles"], cycles)

    def test_finish_waits_for_extra_cycles_after_the_call(self):
        with tempfile.TemporaryDirectory() as tmp:
            poller = ow.Poller([(str(Path(tmp) / "watched"), False)], Path(tmp) / "w.jsonl", interval=0.03)
            poller.start()
            deadline = time.time() + 10
            while poller.cycles < 3 and time.time() < deadline:
                time.sleep(0.02)
            before = poller.cycles
            cycles = poller.finish(min_cycles=2, extra_cycles=2)
        self.assertGreaterEqual(cycles, before + 2)

    def test_finish_without_a_started_poller_just_stops(self):
        with tempfile.TemporaryDirectory() as tmp:
            poller = ow.Poller([], Path(tmp) / "w.jsonl")
            self.assertEqual(poller.finish(), 0)


class ProbeAppLaunchTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.shell = mock.Mock()
        self.shell.log = lambda line: None
        self.ctx = ow.Context(Path(self.tmp.name) / "ev", Path(self.tmp.name) / "w", self.shell, {}, env={})
        self.ctx.evidence.mkdir()
        self.ctx.app_exe = APP_EXE

    def mitm(self, records):
        log = Path(self.tmp.name) / "requests-probe.jsonl"
        log.write_text("".join(json.dumps(r) + "\n" for r in records), encoding="utf-8")
        return mock.Mock(log_path=log)

    def test_the_idle_launch_goes_through_the_strategies_and_reports_only_its_own_traffic(self):
        mitm = self.mitm([{"kind": "request", "host": "github.com", "path": "/earlier"}])
        page = FakePage({}, kind="msedgedriver")
        launch = ow.Launch("msedgedriver", "C:\\wv")
        launch.page = page

        def acquire(ctx, tag, home, wv2_base, evidence, files, try_one=None):
            self.assertEqual(tag, "probe")
            files.append("procs-probe-msedgedriver-launched.txt")
            with mitm.log_path.open("a", encoding="utf-8") as handle:
                handle.write(json.dumps({"kind": "tls_failure", "sni": "rsms.me"}) + "\n")
            return launch, [{"strategy": "msedgedriver", "ok": True, "error": None}]

        with mock.patch.object(ow, "acquire_page", acquire), mock.patch.object(ow, "kill_app"), mock.patch.object(ow.time, "sleep", lambda s: None):
            result = ow.probe_app_launch(self.ctx, mitm)
        self.assertEqual(result["ipc_probe"]["path"], "msedgedriver")
        self.assertTrue(result["ipc_probe"]["value"]["ipc"])
        self.assertEqual(result["idle_launch_requests"]["total"], 0, "the earlier request was before the launch")
        self.assertEqual(result["idle_launch_requests"]["tls_failures"], [{"sni": "rsms.me", "count": 1}])
        self.assertEqual(page.calls, ["probe"], "the probe never calls the updater")
        self.assertTrue(page.closed)

    def test_no_strategy_working_is_an_error_carrying_every_attempt(self):
        attempts = [{"strategy": "msedgedriver", "ok": False, "error": "session not created"}, {"strategy": "cdp-env", "ok": False, "error": "no port"}]
        with mock.patch.object(ow, "acquire_page", lambda *a, **k: (None, attempts)), mock.patch.object(ow, "kill_app") as killed:
            with self.assertRaises(RuntimeError) as caught:
                ow.probe_app_launch(self.ctx, None)
        self.assertIn("session not created", str(caught.exception))
        self.assertIn("no port", str(caught.exception))
        killed.assert_called()


class StrategyOptionTests(unittest.TestCase):
    def args(self, tmp, strategies):
        import argparse
        return argparse.Namespace(evidence=str(Path(tmp) / "evidence"), tier="released_app", cases="clean,cached-bait", config=None, work=str(Path(tmp) / "work"), native_exe=None,
                                  strategies=strategies)

    def test_unknown_strategies_are_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(ow.cmd_run(self.args(tmp, "msedgedriver,telepathy")), 2)
            self.assertEqual(ow.cmd_run(self.args(tmp, "")), 2)

    def test_the_strategy_order_is_carried_into_the_context_and_the_winner_goes_first(self):
        seen = []
        original = ow.Context

        class Spy(original):
            def __init__(self, *a, **k):
                super().__init__(*a, **k)
                seen.append(self)

        with tempfile.TemporaryDirectory() as tmp, mock.patch.object(ow, "Context", Spy):
            ow.cmd_run(self.args(tmp, "cdp-registry,msedgedriver"))
        self.assertEqual(seen[0].strategies, ["cdp-registry", "msedgedriver"])
        seen[0].winning_strategy = "msedgedriver"
        self.assertEqual(ow.strategy_order(seen[0]), ["msedgedriver", "cdp-registry"])
        seen[0].winning_strategy = "something-not-configured"
        self.assertEqual(ow.strategy_order(seen[0]), ["cdp-registry", "msedgedriver"])

    def test_the_default_order_puts_msedgedriver_first(self):
        self.assertEqual(ow.TRIGGER_STRATEGIES[0], "msedgedriver")
        self.assertEqual(set(ow.TRIGGER_STRATEGIES), {"msedgedriver", "cdp-env", "cdp-registry"})


class StepTests(unittest.TestCase):
    """The machine-touching steps against a fake shell: command shapes and the refusal to go on when something is not in place."""

    def make(self, responses):
        calls = []

        def run(argv, timeout=120.0, env=None, cwd=None, quiet=False, capture=True):
            calls.append(list(argv))
            for needle, result in responses:
                if needle in " ".join(argv):
                    return result(argv) if callable(result) else result
            return ow.CmdResult(0, "")

        shell = mock.Mock()
        shell.run.side_effect = run
        shell.log = lambda line: None
        return shell, calls

    def ctx(self, shell, tmp):
        return ow.Context(Path(tmp) / "ev", Path(tmp) / "w", shell, {}, env={})

    def test_live_block_requires_the_rule_and_a_blocked_connect(self):
        with tempfile.TemporaryDirectory() as tmp:
            shell, calls = self.make([("ConnectAsync", ow.CmdResult(0, "unreachable")), ("Get-NetFirewallRule", ow.CmdResult(0, "arc-live-network-block Outbound Block remote=6"))])
            ctx = self.ctx(shell, tmp)
            with mock.patch.object(ow, "load_live_ips", return_value=["203.0.113.1", "203.0.113.2"]):
                ow.block_live_network(ctx)
            self.assertTrue(ctx.live_block)
            self.assertEqual(ctx.live_block_detail["addresses"], 2)
            self.assertNotIn("203.0.113.1", json.dumps(ctx.live_block_detail), "the addresses themselves are not recorded, only their count and digest")
            self.assertEqual(len(ctx.live_block_detail["addresses_sha256"]), 64)
            self.assertIn("New-NetFirewallRule", calls[0][-1])
            shell, _ = self.make([("ConnectAsync", ow.CmdResult(3, "REACHABLE"))])
            ctx = self.ctx(shell, tmp)
            with mock.patch.object(ow, "load_live_ips", return_value=["203.0.113.1"]), self.assertRaises(RuntimeError):
                ow.block_live_network(ctx)
            self.assertFalse(ctx.live_block)
            shell, _ = self.make([("New-NetFirewallRule", ow.CmdResult(1, "access denied")), ("ConnectAsync", ow.CmdResult(0, "unreachable"))])
            ctx = self.ctx(shell, tmp)
            with mock.patch.object(ow, "load_live_ips", return_value=["203.0.113.1"]), self.assertRaises(RuntimeError):
                ow.block_live_network(ctx)

    def test_trust_ca_adds_to_the_machine_root_store_and_checks_it_is_there(self):
        with tempfile.TemporaryDirectory() as tmp:
            ca = Path(tmp) / "ca.crt"
            ca.write_text("-----BEGIN CERTIFICATE-----\n" + base64.b64encode(b"abc").decode() + "\n-----END CERTIFICATE-----\n", encoding="ascii")
            shell, calls = self.make([("Get-ChildItem", ow.CmdResult(0, "1\n"))])
            ctx = self.ctx(shell, tmp)
            ctx.ca_info = {"ca_cert": str(ca)}
            ow.trust_ca(ctx)
            self.assertEqual(ctx.thumbprint, "A9993E364706816ABA3E25717850C26C9CD0D89D")
            self.assertEqual(calls[0], ["certutil", "-addstore", "-f", "Root", str(ca)])
            shell, _ = self.make([("Get-ChildItem", ow.CmdResult(0, "0\n"))])
            ctx = self.ctx(shell, tmp)
            ctx.ca_info = {"ca_cert": str(ca)}
            with self.assertRaises(RuntimeError):
                ow.trust_ca(ctx)

    def test_download_checks_the_digest_after_the_download(self):
        with tempfile.TemporaryDirectory() as tmp:
            payload = b"installer bytes"

            def curl(argv):
                Path(argv[argv.index("-o") + 1]).write_bytes(payload)
                return ow.CmdResult(0, "")

            shell, calls = self.make([("curl.exe", curl)])
            ctx = self.ctx(shell, tmp)
            asset = {"name": "setup.exe", "digest": "sha256:" + hashlib.sha256(payload).hexdigest(), "url": "https://github.com/x/setup.exe"}
            computed, problems = ow.download_installer(ctx, asset, Path(tmp) / "dl" / "setup.exe")
            self.assertEqual((computed, problems), (hashlib.sha256(payload).hexdigest(), []))
            self.assertEqual(calls[0][:6], ["curl.exe", "-fL", "--proto", "=https", "--tlsv1.2", "--retry"])
            asset["digest"] = "sha256:" + "0" * 64
            computed, problems = ow.download_installer(ctx, asset, Path(tmp) / "dl" / "setup.exe")
            self.assertTrue(problems and "differs" in problems[0])
            shell, _ = self.make([("curl.exe", ow.CmdResult(22, "404"))])
            ctx = self.ctx(shell, tmp)
            computed, problems = ow.download_installer(ctx, dict(asset, name="missing.exe"), Path(tmp) / "dl2" / "missing.exe")
            self.assertEqual(computed, "")
            self.assertIn("download failed", problems[0])

    def test_tag_source_fetches_the_tag_only_when_it_is_missing(self):
        conf = json.dumps({"version": "0.7.11", "plugins": {"updater": {"pubkey": "KEY", "endpoints": [ow.MANIFEST_URL]}}})
        with tempfile.TemporaryDirectory() as tmp:
            shell, calls = self.make([("rev-parse", ow.CmdResult(1, "")), ("fetch", ow.CmdResult(0, "")), ("show", ow.CmdResult(0, conf))])
            info = ow.ensure_tag_source(self.ctx(shell, tmp))
            self.assertEqual(info["pubkey"], "KEY")
            self.assertTrue(any("fetch" in c for c in calls))
            self.assertTrue(any(c[-1].startswith("refs/tags/v0.7.11:refs/tags/v0.7.11") for c in calls if "fetch" in c))
            shell, calls = self.make([("rev-parse", ow.CmdResult(0, "abc")), ("show", ow.CmdResult(0, conf))])
            ow.ensure_tag_source(self.ctx(shell, tmp))
            self.assertFalse(any("fetch" in c for c in calls))
            shell, _ = self.make([("rev-parse", ow.CmdResult(1, "")), ("fetch", ow.CmdResult(128, "no network"))])
            self.assertIn("error", ow.ensure_tag_source(self.ctx(shell, tmp)))
            shell, _ = self.make([("rev-parse", ow.CmdResult(0, "")), ("show", ow.CmdResult(0, "not json"))])
            self.assertIn("error", ow.ensure_tag_source(self.ctx(shell, tmp)))

    def test_gh_release(self):
        shell, calls = self.make([("gh", ow.CmdResult(0, json.dumps(release_json())))])
        self.assertEqual(ow.gh_release(shell, "v0.7.11")["tag_name"], "v0.7.11")
        self.assertEqual(calls[0][:3], ["gh", "api", "repos/FerrumVir/arc-chain/releases/tags/v0.7.11"])
        shell, _ = self.make([("gh", ow.CmdResult(1, "HTTP 404"))])
        with self.assertRaises(ow.AssetError):
            ow.gh_release(shell, "v0.7.11")

    def test_snapshot_processes_returns_none_when_powershell_fails(self):
        shell, _ = self.make([("Win32_Process", ow.CmdResult(1, "boom"))])
        self.assertIsNone(ow.snapshot_processes(shell))
        shell, _ = self.make([("Win32_Process", ow.CmdResult(0, json.dumps([ProcessTests.proc(1, 0, "System")])))])
        self.assertEqual(len(ow.snapshot_processes(shell)), 1)

    def test_a_rule_created_but_not_proven_effective_is_still_removed(self):
        with tempfile.TemporaryDirectory() as tmp:
            shell, calls = self.make([("ConnectAsync", ow.CmdResult(3, "REACHABLE")), ("New-NetFirewallRule", ow.CmdResult(0, "rule-created"))])
            ctx = self.ctx(shell, tmp)
            with mock.patch.object(ow, "load_live_ips", return_value=["203.0.113.1"]), self.assertRaises(RuntimeError):
                ow.block_live_network(ctx)
            self.assertFalse(ctx.live_block)
            self.assertTrue(ctx.rule_created)
            ow.cleanup(ctx)
            self.assertTrue(any("Remove-NetFirewallRule" in " ".join(c) for c in calls))

    def test_cleanup_removes_what_was_added(self):
        with tempfile.TemporaryDirectory() as tmp:
            shell, calls = self.make([])
            ctx = self.ctx(shell, tmp)
            ctx.thumbprint, ctx.live_block = "AB" * 20, True
            ow.cleanup(ctx)
            flat = [" ".join(c) for c in calls]
            self.assertTrue(any(c.startswith("certutil -delstore Root " + "AB" * 20) for c in flat))
            self.assertTrue(any("Remove-NetFirewallRule" in c for c in flat))
            shell, calls = self.make([])
            ow.cleanup(self.ctx(shell, tmp))
            self.assertFalse(any("certutil" in " ".join(c) or "Remove-NetFirewallRule" in " ".join(c) for c in calls), "nothing is removed that was never added")


class NativeCaseRunnerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.evidence = Path(self.tmp.name) / "ev"
        self.evidence.mkdir()
        self.shell = mock.Mock()
        self.shell.log = lambda line: None
        self.ctx = ow.Context(self.evidence, Path(self.tmp.name) / "w", self.shell, {}, env=CaseRunnerTests.ENV)
        self.ctx.ca_info = {"server_cert": "s.crt", "server_key": "s.key"}
        self.ctx.install_dir = "C:\\arcw0\\app"
        FakeMitm.fail_start = False

    def run_native(self, name, report, rc=0, control=False):
        out = "warning: something\n" + (json.dumps(report) + "\n" if report is not None else "")
        self.shell.run.return_value = ow.CmdResult(rc, out)
        real_sleep, real_poller = time.sleep, ow.Poller
        processes = iter([[ProcessTests.proc(1, 0, "System")], [ProcessTests.proc(1, 0, "System")]])
        with mock.patch.object(ow, "MitmProcess", FakeMitm), mock.patch.object(ow, "snapshot_processes", side_effect=lambda shell: next(processes)), \
                mock.patch.object(ow, "take_snapshot", return_value={}), mock.patch.object(ow, "Poller", lambda roots, out, interval=1.0: real_poller(roots, out, interval=0.01)), \
                mock.patch.object(ow.time, "sleep", lambda seconds: real_sleep(0.1)):
            return ow.run_native_case(self.ctx, name, Path("C:\\native\\native-updater-check.exe"), "PUBKEY", control=control)

    def report(self, **kw):
        base = {"schema": "arc.legacy-bridge.wave0-lab.native-updater-check.v1", "outcome": "error", "error": ow.RELEASE_NOT_FOUND, "error_kind": "ReleaseNotFound",
                "update": None, "download_attempted": False}
        base.update(kw)
        return base

    def test_the_clean_case_passes_and_uses_the_shipped_endpoint_and_version(self):
        case = self.run_native("clean", self.report())
        self.assertEqual(case["verdict"], "PASS", case["reasons"])
        argv = self.shell.run.call_args[0][0]
        self.assertEqual(argv[1:7], ["--endpoint", ow.MANIFEST_URL, "--current-version", "0.7.11", "--pubkey", "PUBKEY"])
        self.assertNotIn("--insecure-transport", argv, "the shipped transport is exercised, TLS against the trust store")
        self.assertNotIn("--control-download", argv)
        self.assertIn("native-clean.json", case["evidence_files"])
        saved = json.loads((self.evidence / "native-clean.json").read_text(encoding="utf-8"))
        self.assertEqual(saved["error"], ow.RELEASE_NOT_FOUND)

    def test_the_bait_case_and_the_control(self):
        bait = self.report(outcome="update_available", error=None, error_kind=None, update={"version": "0.8.11", "download_url": "https://github.com/x"})
        case = self.run_native("cached-bait", bait)
        self.assertEqual(case["verdict"], "PASS", case["reasons"])
        FakeMitm.records = dict(FakeMitm.records, **{"bait-0.8.11": [MANIFEST, rec("/FerrumVir/arc-chain/releases/download/v0.8.11/ARC.Node_0.8.11_x64-setup.exe", status=404, payload=True)]})
        self.addCleanup(lambda: FakeMitm.records.update({"bait-0.8.11": [MANIFEST]}))
        control = self.run_native("cached-bait", dict(bait, download_attempted=True, download_result="error: bad signature"), control=True)
        self.assertEqual(control["verdict"], "FAIL", "the control case itself sees a bundle request")
        self.assertIn("--control-download", self.shell.run.call_args[0][0])
        self.assertEqual(ow.control_judgement(control)["verdict"], "PASS")
        self.assertIn("native-control.json", control["evidence_files"])

    def test_no_json_report_is_unproved(self):
        case = self.run_native("clean", None, rc=2)
        self.assertEqual(case["verdict"], "UNPROVED")
        self.assertFalse(case["trigger_outcome"]["ran"])


class ProbeTests(unittest.TestCase):
    def test_probe_never_fails_and_writes_probe_json_even_off_windows(self):
        import argparse
        with tempfile.TemporaryDirectory() as tmp:
            args = argparse.Namespace(evidence=str(Path(tmp) / "ev"), launch=True, cargo_check=False)
            with mock.patch.object(ow.Shell, "run", return_value=ow.CmdResult(1, "no")):
                self.assertEqual(ow.cmd_probe(args), 0)
            probe = json.loads((Path(tmp) / "ev" / "probe.json").read_text(encoding="utf-8"))
        self.assertEqual(probe["schema"], ow.SCHEMA_PROBE)
        self.assertEqual(probe["checks"]["is_windows"]["detail"], False)
        self.assertFalse(probe["checks"]["release_asset_digest"]["ok"])
        self.assertIn("skipped", probe["checks"]["installer_download"]["detail"])
        self.assertIn("finished", probe)

    def test_main_catches_a_crash_and_keeps_the_checks_recorded_so_far(self):
        with tempfile.TemporaryDirectory() as tmp:
            evidence = Path(tmp) / "ev"
            evidence.mkdir()
            (evidence / "probe.json").write_text(json.dumps({"schema": ow.SCHEMA_PROBE, "checks": {"python": {"ok": True, "detail": "3.12"}}}), encoding="utf-8")
            with mock.patch.object(ow, "cmd_probe", side_effect=RuntimeError("kaboom")):
                self.assertEqual(ow.main(["probe", "--evidence", str(evidence)]), 0)
            probe = json.loads((evidence / "probe.json").read_text(encoding="utf-8"))
            self.assertIn("kaboom", probe["error"])
            self.assertEqual(probe["checks"]["python"]["detail"], "3.12")
            (evidence / "probe.json").unlink()
            with mock.patch.object(ow, "cmd_probe", side_effect=RuntimeError("again")):
                self.assertEqual(ow.main(["probe", "--evidence", str(evidence)]), 0)
            self.assertIn("again", json.loads((evidence / "probe.json").read_text(encoding="utf-8"))["error"])

    def test_probe_is_written_after_every_check(self):
        import argparse
        with tempfile.TemporaryDirectory() as tmp:
            evidence = Path(tmp) / "ev"
            seen = []
            real_write = ow.write_json

            def spying(path, value, sort_keys=True):
                real_write(path, value, sort_keys=sort_keys)
                if Path(path).name == "probe.json":
                    seen.append(len(value["checks"]))

            with mock.patch.object(ow, "write_json", spying), mock.patch.object(ow.Shell, "run", return_value=ow.CmdResult(0, "ok")):
                ow.cmd_probe(argparse.Namespace(evidence=str(evidence), launch=False, cargo_check=False))
        self.assertEqual(seen, sorted(seen))
        self.assertGreater(len(set(seen)), 5, "the file grows check by check, so a killed job still leaves what was learned")


class FakeCargo:
    def __init__(self, rc=0, hang=False):
        self.rc, self.hang, self.killed, self.waits = rc, hang, False, []

    def wait(self, timeout=None):
        self.waits.append(timeout)
        if self.hang and not self.killed:
            raise ow.subprocess.TimeoutExpired("cargo", timeout)
        return -9 if self.killed else self.rc

    def kill(self):
        self.killed = True


class CargoCheckTests(unittest.TestCase):
    def ctx(self, tmp):
        return ow.Context(Path(tmp) / "ev", Path(tmp) / "w", ow.Shell(), {}, env={"PATH": "p", "TEMP": "C:\\Temp"})

    def test_the_command_and_environment(self):
        with tempfile.TemporaryDirectory() as tmp:
            seen = {}

            def popen(argv, **kwargs):
                seen["argv"], seen["kwargs"] = argv, kwargs
                return FakeCargo()

            job = ow.start_cargo_check(self.ctx(tmp), popen=popen, clock=lambda: 100.0)
            job["handle"].close()
            self.assertEqual(seen["argv"][:4], ["cargo", "check", "--locked", "--manifest-path"])
            self.assertTrue(seen["argv"][4].replace("\\", "/").endswith("wave0-lab-desktop/native-updater-check/Cargo.toml"))
            env = seen["kwargs"]["env"]
            self.assertEqual(env["RUSTUP_TOOLCHAIN"], "stable")
            self.assertTrue(env["CARGO_TARGET_DIR"].startswith(tmp))
            self.assertEqual(env["TEMP"], env["TMP"])
            self.assertNotEqual(env["TEMP"], "C:\\Temp", "the compiler's temp files stay out of the watched temp directory")
            self.assertEqual(seen["kwargs"]["cwd"], str(ow.HERE / "native-updater-check"))
            self.assertEqual(job["started"], 100.0)
            self.assertIsNone(job["error"])

    def test_a_missing_cargo_is_recorded_not_raised(self):
        with tempfile.TemporaryDirectory() as tmp:
            def popen(argv, **kwargs):
                raise FileNotFoundError("cargo")

            job = ow.start_cargo_check(self.ctx(tmp), popen=popen)
            self.assertIn("FileNotFoundError", job["error"])
            result = ow.finish_cargo_check(job)
            self.assertIsNone(result["rc"])
            self.assertIn("FileNotFoundError", result["error"])

    def test_finish_reports_rc_elapsed_and_the_last_forty_lines(self):
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "cargo.log"
            log.write_text("".join("line %d 149.28.32.76\n" % n for n in range(100)), encoding="utf-8")
            proc = FakeCargo(rc=0)
            clock = iter([130.0, 421.5])
            job = {"argv": ["cargo", "check"], "started": 100.0, "proc": proc, "handle": None, "error": None, "log": log}
            result = ow.finish_cargo_check(job, timeout=1200, clock=lambda: next(clock))
            self.assertEqual((result["rc"], result["timed_out"], result["elapsed_s"]), (0, False, 321.5))
            self.assertEqual(len(result["tail"]), 40)
            self.assertEqual(result["tail"][-1], "line 99 149.28.x.x")
            self.assertEqual(result["log_lines"], 100)
            self.assertEqual(proc.waits, [1200 - 30.0], "the wait is what is left of the 20 minutes counted from the start")
            self.assertTrue(job["tail_text"].endswith("line 99 149.28.x.x\n"))

    def test_an_overrunning_check_is_killed_and_marked(self):
        with tempfile.TemporaryDirectory() as tmp:
            proc = FakeCargo(hang=True)
            job = {"argv": ["cargo", "check"], "started": 0.0, "proc": proc, "handle": None, "error": None, "log": None}
            result = ow.finish_cargo_check(job, timeout=1200, clock=lambda: 1300.0)
            self.assertTrue(proc.killed)
            self.assertTrue(result["timed_out"])
            self.assertIsNone(result["rc"])
            self.assertEqual(proc.waits[0], 1.0, "never a zero or negative wait")

    def test_a_compile_error_is_a_nonzero_rc(self):
        proc = FakeCargo(rc=101)
        result = ow.finish_cargo_check({"argv": ["cargo"], "started": 0.0, "proc": proc, "handle": None, "error": None, "log": None}, clock=lambda: 5.0)
        self.assertEqual((result["rc"], result["timed_out"]), (101, False))


class WindowsProbeTests(unittest.TestCase):
    """The rehearsal chain of `probe` on a simulated Windows runner: every machine step faked, the order and the teardown are what is under test."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.evidence = Path(self.tmp.name) / "ev"
        self.events = []
        self.cargo_outcome = {"command": "cargo check --locked", "rc": 0, "timed_out": False, "elapsed_s": 321.0, "tail": ["Finished dev profile"], "error": None}
        FakeMitm.fail_start = False
        FakeMitm.instances = []

    def run_probe(self, launch=True, cargo=True, **overrides):
        import argparse
        fake_ca = OrchestrationTests.fake_ca_module(self, self.tmp.name)
        record = self.events.append

        class SocketStub:
            def __init__(self, *args, **kwargs):
                pass

            def bind(self, address):
                record(("bind", address))

            def close(self):
                pass

        def shell_run(self, argv, timeout=120.0, env=None, cwd=None, quiet=False, capture=True):
            record(("run", " ".join(argv)[:80]))
            return ow.CmdResult(0, "True" if "IsInRole" in " ".join(argv) else "ok")

        def fake_install(ctx, asset, installer, **kwargs):
            ctx.app_exe = APP_EXE
            return {"exe": APP_EXE}

        patches = {
            "IS_WINDOWS": True,
            "hosts_path": lambda: Path(self.tmp.name) / "hosts",
            "gh_release": lambda shell, tag: release_json(),
            "ensure_tag_source": lambda ctx: {"pubkey": "PUBKEY", "endpoints": [ow.MANIFEST_URL], "version": "0.7.11"},
            "download_installer": lambda ctx, asset, dest: (NSIS_SHA, []),
            "find_native_exe": lambda explicit=None, env=None, exists=None: ("C:\\native\\n.exe", []),
            "binary_info": lambda path: {"path": str(path), "sha256": "00"},
            "load_live_ips": lambda root: ["203.0.113.1", "203.0.113.2"],
            "block_live_network": lambda ctx: (record(("block", None)), setattr(ctx, "live_block", True), setattr(ctx, "live_block_detail", {"rule": "x"})),
            "trust_ca": lambda ctx: (record(("trust", None)), setattr(ctx, "thumbprint", "AB" * 20)),
            "map_hosts": lambda ctx, names: (record(("hosts", None)), setattr(ctx, "hosts_mapped", list(names)), setattr(ctx, "hosts_blackholed", ["arc.ai"])),
            "MitmProcess": FakeMitm,
            "curl_selftest": lambda shell, extra=(): {"rc": 0, "http_code": "302", "recording_server_header": True, "output_tail": ""},
            "native_selftest": lambda shell, exe, pubkey: {"rc": 0, "report": {"outcome": "error"}, "release_not_found": True, "output_tail": ""},
            "install_app": fake_install,
            "provenance_of_binary": lambda path, pubkey: {"binary_sha256": "00", "binary_size": 1, "plugin_version_from_binary": ["2.10.1"], "matches_pin": {}, "pubkey_matches_conf": True},
            "probe_app_launch": lambda ctx, mitm, port=ow.CDP_PORT: {"page_target": {"found": True}, "idle_launch_requests": {"total": 0}},
            "cleanup": lambda ctx: record(("cleanup", None)),
            "start_cargo_check": lambda ctx, **kw: (record(("cargo-start", None)), {"argv": ["cargo", "check"], "started": 0.0, "proc": object(), "handle": None, "error": None, "log": None})[1],
            "finish_cargo_check": lambda job, **kw: (record(("cargo-finish", None)), self.cargo_outcome)[1],
        }
        patches.update(overrides)
        stack = [mock.patch.object(ow, name, value) for name, value in patches.items()]
        stack += [mock.patch.object(ow.Shell, "run", shell_run), mock.patch.object(ow.socket, "socket", SocketStub), mock.patch.dict(sys.modules, {"ca": fake_ca})]
        for patch in stack:
            patch.start()
            self.addCleanup(patch.stop)
        (Path(self.tmp.name) / "hosts").write_text("127.0.0.1 localhost\r\n", encoding="utf-8")
        env = mock.patch.dict(os.environ, {"ARCW0_WORK": str(Path(self.tmp.name) / "work")})
        env.start()
        self.addCleanup(env.stop)
        code = ow.cmd_probe(argparse.Namespace(evidence=str(self.evidence), launch=launch, cargo_check=cargo))
        self.assertEqual(code, 0)
        return json.loads((self.evidence / "probe.json").read_text(encoding="utf-8"))

    def test_the_whole_chain_runs_in_order_and_is_torn_down(self):
        probe = self.run_probe()
        failed = {name: value["detail"] for name, value in probe["checks"].items() if not value["ok"] and name not in ("curl", "gh", "cargo", "rustup_toolchains", "edge_driver", "seven_zip", "disk")}
        self.assertEqual(failed, {}, failed)
        order = [name for name in probe["checks"]]
        for earlier, later in (("release_asset_digest", "installer_download"), ("installer_download", "live_block"), ("live_block", "root_store"), ("root_store", "hosts"),
                               ("hosts", "recording_server"), ("recording_server", "tls_selftest_curl"), ("tls_selftest_curl", "app_install"), ("app_install", "app_launch_idle")):
            self.assertLess(order.index(earlier), order.index(later))
        kinds = [event[0] for event in self.events]
        self.assertLess(kinds.index("block"), kinds.index("trust"))
        self.assertLess(kinds.index("trust"), kinds.index("hosts"))
        self.assertEqual(kinds[-2:], ["cleanup", "cargo-finish"], "the isolation is torn down first, the compiler is awaited last")
        self.assertEqual(kinds.count("cleanup"), 1)
        self.assertTrue(FakeMitm.instances[0].stopped)
        self.assertTrue((self.evidence / "ca.crt").is_file())
        self.assertEqual(probe["isolation_snapshot"]["live_block"], True)
        self.assertIn("cleanup", probe)

    def test_cargo_check_runs_alongside_the_probe_and_is_awaited_after_the_isolation_is_gone(self):
        probe = self.run_probe()
        kinds = [event[0] for event in self.events]
        self.assertLess(kinds.index("cargo-start"), kinds.index("block"), "started at the beginning, before anything is isolated")
        self.assertLess(kinds.index("cleanup"), kinds.index("cargo-finish"), "the hosts block, the Root certificate and the firewall rule are gone while the compiler is awaited")
        self.assertEqual(kinds.count("cargo-start"), 1)
        self.assertEqual(kinds.count("cargo-finish"), 1)
        self.assertEqual(probe["native_check"]["rc"], 0)
        self.assertTrue(probe["checks"]["native_crate_compiles"]["ok"])
        self.assertIn("rc 0", probe["checks"]["native_crate_compiles"]["detail"])

    def test_a_failing_or_overrunning_cargo_check_is_recorded_as_a_failed_check(self):
        self.cargo_outcome = {"command": "cargo check --locked", "rc": 101, "timed_out": False, "elapsed_s": 40.0, "tail": ["error[E0432]: unresolved import"], "error": None}
        probe = self.run_probe()
        self.assertFalse(probe["checks"]["native_crate_compiles"]["ok"])
        self.assertEqual(probe["native_check"]["tail"], ["error[E0432]: unresolved import"])
        self.setUp()
        self.cargo_outcome = {"command": "cargo check --locked", "rc": None, "timed_out": True, "elapsed_s": 1201.0, "tail": [], "error": None}
        probe = self.run_probe()
        self.assertFalse(probe["checks"]["native_crate_compiles"]["ok"])
        self.assertIn("killed", probe["checks"]["native_crate_compiles"]["detail"])

    def test_no_cargo_check_flag_skips_it(self):
        probe = self.run_probe(cargo=False)
        self.assertNotIn("native_check", probe)
        self.assertNotIn("cargo-start", [event[0] for event in self.events])

    def test_cargo_is_awaited_even_when_a_check_blew_up_before_the_end(self):
        def broken(ctx, asset, installer, **kwargs):
            raise OSError("installer vanished")

        self.run_probe(install_app=broken)
        kinds = [event[0] for event in self.events]
        self.assertEqual(kinds[-1], "cargo-finish")
        self.assertLess(kinds.index("cleanup"), kinds.index("cargo-finish"))

    def test_the_hosts_are_only_mapped_after_every_github_read(self):
        self.run_probe()
        reads = [i for i, e in enumerate(self.events) if e[0] == "run" and e[1].startswith("curl.exe --version")]
        self.assertTrue(reads)
        kinds = [event[0] for event in self.events]
        self.assertLess(kinds.index("run"), kinds.index("hosts"))

    def test_a_failing_install_skips_the_launch_and_still_cleans_up(self):
        def broken(ctx, asset, installer, **kwargs):
            raise OSError("installer vanished")

        probe = self.run_probe(install_app=broken)
        self.assertFalse(probe["checks"]["app_install"]["ok"])
        self.assertIn("installer vanished", probe["checks"]["app_install"]["detail"])
        self.assertIn("skipped", probe["checks"]["app_launch_idle"]["detail"])
        self.assertTrue(probe["checks"]["tls_selftest_curl"]["ok"], "what was learned before the failure stays in the file")
        kinds = [e[0] for e in self.events]
        self.assertEqual(kinds.count("cleanup"), 1)
        self.assertLess(kinds.index("cleanup"), kinds.index("cargo-finish"))

    def test_a_tls_failure_is_a_failed_check_not_a_crash(self):
        probe = self.run_probe(curl_selftest=lambda shell, extra=(): {"rc": 35, "http_code": "000", "recording_server_header": False, "output_tail": "schannel: CRYPT_E_NO_REVOCATION_CHECK"})
        self.assertFalse(probe["checks"]["tls_selftest_curl"]["ok"])
        self.assertIn("CRYPT_E_NO_REVOCATION_CHECK", probe["checks"]["tls_selftest_curl"]["detail"])

    def test_a_native_plugin_that_does_not_reach_release_not_found_is_reported(self):
        probe = self.run_probe(native_selftest=lambda shell, exe, pubkey: {"rc": 0, "report": {"outcome": "error", "error": "invalid peer certificate"}, "release_not_found": False, "output_tail": ""})
        self.assertFalse(probe["checks"]["tls_selftest_native_plugin"]["ok"])
        self.assertIn("invalid peer certificate", probe["checks"]["tls_selftest_native_plugin"]["detail"])

    def test_no_launch_skips_only_the_app_steps(self):
        probe = self.run_probe(launch=False)
        self.assertNotIn("app_install", probe["checks"])
        self.assertTrue(probe["checks"]["tls_selftest_native_plugin"]["ok"])

    def test_a_missing_native_binary_does_not_stop_the_curl_selftest(self):
        probe = self.run_probe(find_native_exe=lambda explicit=None, env=None, exists=None: (None, ["X"]))
        self.assertFalse(probe["checks"]["native_binary"]["ok"])
        self.assertIn("skipped", probe["checks"]["tls_selftest_native_plugin"]["detail"])
        self.assertTrue(probe["checks"]["tls_selftest_curl"]["ok"])

    def test_curl_selftest_parsing(self):
        shell = mock.Mock()
        shell.run.return_value = ow.CmdResult(0, "HTTP/1.1 302 Found\r\nServer: wave0-lab-mitm\r\nX-Wave0-Lab-Scenario: latest-404\r\n\r\n\nHTTPCODE:302\n")
        result = ow.curl_selftest(shell, ["--ssl-no-revoke"])
        self.assertEqual((result["http_code"], result["recording_server_header"], result["rc"]), ("302", True, 0))
        argv = shell.run.call_args[0][0]
        self.assertIn("--ssl-no-revoke", argv)
        self.assertEqual(argv[-1], ow.MANIFEST_URL)
        shell.run.return_value = ow.CmdResult(60, "curl: (60) SSL certificate problem\n\nHTTPCODE:000\n")
        result = ow.curl_selftest(shell)
        self.assertEqual((result["http_code"], result["recording_server_header"]), ("000", False))

    def test_native_selftest_parsing(self):
        shell = mock.Mock()
        line = json.dumps({"schema": "arc.legacy-bridge.wave0-lab.native-updater-check.v1", "outcome": "error", "error": ow.RELEASE_NOT_FOUND})
        shell.run.return_value = ow.CmdResult(0, line + "\n")
        self.assertTrue(ow.native_selftest(shell, "n.exe", "KEY")["release_not_found"])
        argv = shell.run.call_args[0][0]
        self.assertEqual(argv[1:], ["--endpoint", ow.MANIFEST_URL, "--current-version", "0.7.11", "--pubkey", "KEY"])
        shell.run.return_value = ow.CmdResult(2, "usage")
        self.assertFalse(ow.native_selftest(shell, "n.exe", "KEY")["release_not_found"])


if __name__ == "__main__":
    unittest.main()
