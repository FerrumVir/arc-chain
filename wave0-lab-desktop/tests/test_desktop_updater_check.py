"""Tests for desktop_updater_check.py (THROWAWAY LAB FILE).

Offline by construction: the only sockets are the in-process fake GitHub on 127.0.0.1 and the stub native checker's
connections to it. Layers:

  * hermetic tests (pure functions, fake GitHub, replicas, a hand-written source fixture, a stub native executable,
    a miniature plugin source) run everywhere, including CI without the v0.7.11 tag;
  * real-tag tests (check-source against the released v0.7.11 sources, synthetic repositories mutated from them) run
    when a checkout holding the tag exists (ARC_V0711_REPO, the repin worktree, or this repository);
  * crate / released-binary tests run when the pinned local copies exist (ARC_PLUGIN_CRATE, ARC_RELEASED_BINARY).
"""
from __future__ import annotations

import contextlib
import copy
import hashlib
import io
import json
import os
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from typing import Callable, Dict, List, Optional
from unittest import mock

import _paths  # noqa: F401
import desktop_updater_check as duc

REPO = duc.REPO
SCRATCH = Path("/private/tmp/claude-501/-Users-excaulibur-work/8af1d49b-2f89-4291-b808-9e795d4789e8/scratchpad/captain-desktop-bin")
KNOWN_BLOBS = {
    "desktop/src-tauri/tauri.conf.json": "d6bb2358b11c9aa52d369df0c9e0566bf0b02895",
    "desktop/src-tauri/src/lib.rs": "39ba6f6fe186767279d18bc722df073c919e8fb6",
    "desktop/src-tauri/src/commands.rs": "8f216fe69e11ca84887da96082f8830b8bc84061",
    "desktop/src/screens/Settings.tsx": "0ebec6f39a3615382204a686b310b37851cbe32a",
    "desktop/src/lib/tauri.ts": "e71c5926cba28e2f6b0d119f6d17f455bff75c8c",
    "desktop/src-tauri/Cargo.toml": "a86363db8c1cb8ba0c5c86cdd916a273214a318e",
}


def _has_tag(path: Optional[Path]) -> bool:
    if path is None or not Path(path).is_dir():
        return False
    try:
        done = subprocess.run(["git", "-C", str(path), "rev-parse", "--verify", "--quiet", "v0.7.11^{commit}"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    except OSError:
        return False
    return done.returncode == 0


def _first(*candidates: Optional[Path]) -> Optional[Path]:
    for candidate in candidates:
        if candidate is not None and Path(candidate).exists():
            return Path(candidate)
    return None


def _env_path(name: str) -> Optional[Path]:
    return Path(os.environ[name]) if os.environ.get(name) else None


REAL_REPO: Optional[Path] = next((p for p in (_env_path("ARC_V0711_REPO"), Path("/Users/excaulibur/work/arc-repin-v0811-20261008"), _paths.ROOT) if _has_tag(p)), None)
PLUGIN_CRATE = _first(_env_path("ARC_PLUGIN_CRATE"), SCRATCH / "tauri-plugin-updater-2.10.1.crate")
RELEASED_BINARY = _first(_env_path("ARC_RELEASED_BINARY"), SCRATCH / "deb" / "usr" / "bin" / "arc-desktop")
RELEASED_DEB = _first(_env_path("ARC_RELEASED_DEB"), SCRATCH / "ARC.Node_0.7.11_amd64.deb")
PINNED_VERSIONS_JSON = _first(_env_path("ARC_PINNED_VERSIONS"), SCRATCH / "pinned-versions.json")


def _sha256_file(path: Path) -> str:
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


CRATE_OK = PLUGIN_CRATE is not None and _sha256_file(PLUGIN_CRATE) == duc.PINNED["plugin_crate"]["sha256"]
needs_repo = unittest.skipUnless(REAL_REPO is not None, "no checkout holding the v0.7.11 tag (set ARC_V0711_REPO)")
needs_crate = unittest.skipUnless(CRATE_OK, "pinned tauri-plugin-updater-2.10.1.crate not available locally (set ARC_PLUGIN_CRATE)")
needs_binary = unittest.skipUnless(RELEASED_BINARY is not None, "released v0.7.11 arc-desktop binary not available locally (set ARC_RELEASED_BINARY)")
needs_posix = unittest.skipIf(os.name == "nt", "shebang scripts are not directly executable on Windows")


def fake_source(endpoints: Optional[List[str]] = None, plugin_status: str = "unverified", plugin_reason: Optional[str] = "offline-requested", items_ok: bool = True) -> dict:
    """A hand-written check-source result (same shape) so `execute` can run without any repository."""
    pin = duc.PINNED
    return {
        "schema": duc.SCHEMA_CHECK_SOURCE,
        "tag": duc.TAG,
        "tag_commit": "0" * 40,
        "tag_commit_date": "2026-06-15T00:00:00+00:00",
        "items": [{"id": "x-fixture", "title": "fixture item", "ok": items_ok, "detail": "fixture", "citations": []}],
        "inventories": {},
        "plugin_semantics": {
            "crate": "tauri-plugin-updater 2.10.1", "status": plugin_status, "reason": plugin_reason, "crate_sha256_observed": None,
            "loud_note": None if plugin_status == "confirmed" else "fixture: the plugin source was not read", "lines": {}, "release_not_found_text": None,
        },
        "released_binary": {"binary_sha256": pin["release"]["binary_sha256"], "crates_from_binary_strings": pin["crates_from_binary_strings"], "verified_locally": False, "problems": []},
        "sources": {"desktop/src-tauri/tauri.conf.json": {"git_blob": "d" * 40, "sha256": "e" * 64}},
        "extracted": {
            "api_url": duc.API_LATEST,
            "sidecar_template": duc.SIDECAR_TEMPLATE,
            "platform_assets": {"linux-x86_64": "arc-node-linux-x86_64", "macos-aarch64": "arc-node-macos-arm64", "macos-x86_64": "arc-node-macos-x86_64", "windows-x86_64": "arc-node-windows-x86_64.exe"},
            "pubkey": "dW50cnVzdGVkIGNvbW1lbnQ6IGZpeHR1cmU=",
            "pubkey_sha256": "f" * 64,
            "endpoints": list(endpoints) if endpoints is not None else [duc.TAURI_ENDPOINT],
            "settings_templates": {
                "available": "Version ${update.version} is available. Click below to download, install, and relaunch.",
                "install_button": "Install v${update.version} & relaunch",
            },
        },
        "verdict": "SOURCE_VERIFIED",
    }


def sidecar_asset() -> str:
    return duc.platform_asset(fake_source()["extracted"]["platform_assets"])


def by_id(result: dict) -> Dict[str, dict]:
    return {entry["id"]: entry for entry in result["assertions"]}


def states(result: dict) -> Dict[str, str]:
    return {entry["id"]: entry["result"] for entry in result["assertions"]}


class TempCase(unittest.TestCase):
    def tmpdir(self) -> Path:
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        return Path(holder.name)


class ScriptedGitHub(duc.FakeGitHub):
    """The fake GitHub with canned answers for chosen paths (everything else behaves like the main model)."""

    def __init__(self, routes: Optional[Dict[str, tuple]] = None, model: Optional[duc.ReleaseModel] = None):
        super().__init__(model or duc.main_model())
        self.routes = routes or {}

    def route(self, path: str):
        if path in self.routes:
            status, headers, body = self.routes[path]
            return status, headers, body, "scripted"
        return super().route(path)


ENDPOINT_PATH = f"/{REPO}/releases/latest/download/latest.json"
JSON_HEADERS = {"Content-Type": "application/json"}


def release_json(version: str, platforms: bool = True) -> bytes:
    document = {"version": version, "notes": "x", "pub_date": "2026-10-08T00:00:00Z"}
    if platforms:
        document["platforms"] = {f"{os_}-{arch}": {"signature": duc.FAKE_SIGNATURE, "url": f"http://example.invalid/{os_}-{arch}"} for os_ in ("linux", "darwin", "windows") for arch in ("x86_64", "aarch64", "i686")}
    return json.dumps(document).encode("utf-8")


# --------------------------------------------------------------------------------------------------------------
# pure helpers
# --------------------------------------------------------------------------------------------------------------

class PureFunctionTests(unittest.TestCase):
    def test_trim_start_matches_v_strips_every_leading_v(self):
        self.assertEqual(duc.trim_start_matches_v("v0.7.12"), "0.7.12")
        self.assertEqual(duc.trim_start_matches_v("vv0.7.12"), "0.7.12")
        self.assertEqual(duc.trim_start_matches_v("0.7.12"), "0.7.12")
        self.assertEqual(duc.trim_start_matches_v("v"), "")

    def test_semver_ordering_follows_semver(self):
        key = duc.semver_key
        self.assertGreater(key("0.8.11"), key("0.7.11"))
        self.assertGreater(key("0.10.0"), key("0.9.9"))
        self.assertEqual(key("v1.2.3"), key("1.2.3"))
        self.assertLess(key("1.0.0-rc.1"), key("1.0.0"))
        self.assertGreater(key("1.0.0+build"), key("1.0.0-rc.1"))
        with self.assertRaises(ValueError):
            key("1.2")
        with self.assertRaises(ValueError):
            key("latest")

    def test_rewrite_url_rewrites_only_the_two_github_hosts(self):
        base = "http://127.0.0.1:5555"
        self.assertEqual(duc.rewrite_url("https://github.com/a/b?x=1", base, base), "http://127.0.0.1:5555/a/b?x=1")
        self.assertEqual(duc.rewrite_url("https://api.github.com/repos/a/b", base, "http://127.0.0.1:6666"), "http://127.0.0.1:6666/repos/a/b")
        self.assertEqual(duc.rewrite_url("http://127.0.0.1:5555/already", base, base), "http://127.0.0.1:5555/already")
        for other in ("https://objects.githubusercontent.com/x", "https://example.com/", "https://github.com.evil.example/x", "https://evilgithub.com/x"):
            with self.assertRaises(duc.UnexpectedHost, msg=other):
                duc.rewrite_url(other, base, base)

    def test_intended_host_only_for_the_github_hosts(self):
        self.assertEqual(duc.intended_host("https://github.com/x"), "github.com")
        self.assertEqual(duc.intended_host("https://API.github.com/x"), "API.github.com")
        self.assertIsNone(duc.intended_host("http://127.0.0.1:1/x"))
        self.assertIsNone(duc.intended_host("https://example.com/x"))

    def test_blank_rust_blanks_comments_and_literals_but_keeps_offsets(self):
        source = 'let a = "x // y {"; // note "q" {\n/* block { */ let b = \'}\'; let c = \'a;\nlet d = r;'
        blanked = duc.blank_rust(source)
        self.assertEqual(len(blanked), len(source))
        self.assertEqual(blanked.count("\n"), source.count("\n"))
        for gone in ("x // y", "note", "block", "{", "}"):
            self.assertNotIn(gone, blanked.replace('"', ""), gone)
        self.assertIn("let a =", blanked)
        self.assertIn("let d = r;", blanked)

    def test_rust_block_range_ignores_braces_in_strings_and_comments(self):
        source = 'fn f() {\n    let s = "}";\n    // }\n    if x {\n        y();\n    }\n}\nfn g() {}\n'
        self.assertEqual(duc.rust_block_range(source, 1), (1, 7))
        self.assertEqual(duc.rust_block_range(source, 8), (8, 8))
        with self.assertRaises(ValueError):
            duc.rust_block_range("fn broken() {\n", 1)

    def test_line_finders(self):
        lines = ["alpha", "beta needle", "gamma", "needle again"]
        self.assertEqual(duc.find_line(lines, "needle"), 2)
        self.assertEqual(duc.find_line(lines, "needle", 3), 4)
        self.assertIsNone(duc.find_line(lines, "absent"))
        self.assertEqual(duc.find_regex_line(lines, r"^gam+a$"), 3)
        self.assertIsNone(duc.find_regex_line(lines, r"zzz"))

    def test_cite_shape(self):
        entry = duc.cite("a/b.rs", 7, "   text   ")
        self.assertEqual(entry, {"file": "a/b.rs", "line": 7, "text": "text"})
        self.assertIsNone(duc.cite("a/b.rs", None, "x")["line"])

    def test_changed_paths_lists_new_changed_and_removed(self):
        before = {"a": {"type": "dir"}, "b": {"type": "file", "size": 1, "sha256": "1"}, "gone": {"type": "file", "size": 1, "sha256": "2"}}
        after = {"a": {"type": "dir"}, "b": {"type": "file", "size": 2, "sha256": "3"}, "new": {"type": "file", "size": 1, "sha256": "4"}}
        self.assertEqual(duc.changed_paths(before, after), ["b", "new", "(removed) gone"])
        self.assertEqual(duc.changed_paths(before, before), [])

    def test_snapshot_tree_records_dirs_files_and_hashes(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            (root / "d" / "e").mkdir(parents=True)
            (root / "d" / "e" / "f.txt").write_bytes(b"hello")
            (root / "top").write_bytes(b"")
            snap = duc.snapshot_tree(root)
        self.assertEqual(list(snap), ["d", "d/e", "d/e/f.txt", "top"])
        self.assertEqual(snap["d"], {"type": "dir"})
        self.assertEqual(snap["d/e/f.txt"], {"type": "file", "size": 5, "sha256": hashlib.sha256(b"hello").hexdigest()})

    def test_parse_remote_release(self):
        platforms = duc.parse_remote_release({"version": "v0.8.11", "platforms": {"linux-x86_64": {"url": "u", "signature": "s"}}})
        self.assertEqual((platforms["version"], list(platforms["platforms"])), ("0.8.11", ["linux-x86_64"]))
        flat = duc.parse_remote_release({"name": "1.2.3", "url": "u", "signature": "s"})
        self.assertEqual((flat["version"], flat["url"], flat["platforms"]), ("1.2.3", "u", None))
        for bad, kind in (
            ([], "Json"), ({"platforms": {}}, "Json"), ({"version": "x.y", "platforms": {}}, "Semver"),
            ({"version": "1.0.0"}, "Json"), ({"version": "1.0.0", "platforms": 5}, "Json"), ({"version": 7, "platforms": {}}, "Json"),
        ):
            with self.assertRaises(duc.TauriError, msg=repr(bad)) as raised:
                duc.parse_remote_release(bad)
            self.assertEqual(raised.exception.kind, kind, repr(bad))

    def test_forbidden_requests_patterns(self):
        def row(path, status=404):
            return {"seq": 1, "path": path, "status": status}

        forbidden = [
            f"/{REPO}/releases/download/v0.8.11/anything", f"/{REPO}/releases/download/v0.7.11/arc-node-linux-x86_64",
            f"/{REPO}/releases/download/v0.7.12/ARC.Node_0.7.12_amd64.AppImage", "/x/ARC.Node_aarch64.app.tar.gz", "/x/ARC.Node_0.7.12_x64-setup.exe",
            "/x/a.deb", "/x/a.rpm", "/x/a.dmg", "/x/a.msi", "/x/ARC.Node_x64.app.tar.gz.sig",
        ]
        for path in forbidden:
            self.assertTrue(duc.forbidden_requests([row(path)]), path)
        self.assertTrue(duc.forbidden_requests([row(f"/{REPO}/releases/download/v0.7.12/latest.json", 200)]))
        self.assertFalse(duc.forbidden_requests([row(f"/{REPO}/releases/download/v0.7.12/latest.json", 404)]))
        self.assertFalse(duc.forbidden_requests([row(f"/{REPO}/releases/download/v0.7.12/arc-node-linux-x86_64", 200), row(f"/repos/{REPO}/releases/latest", 200)]))

    def test_plugin_acceptable_truth_table(self):
        accept = duc.plugin_acceptable
        self.assertTrue(accept({"status": "confirmed"}))
        self.assertTrue(accept({"status": "unverified", "reason": "offline-requested"}))
        self.assertFalse(accept({"status": "unverified", "reason": "fetch-failed"}))
        self.assertFalse(accept({"status": "unverified"}))
        self.assertFalse(accept({"status": "contradicted", "reason": "offline-requested"}))

    def test_pinned_constants_name_the_shipped_versions(self):
        pin = duc.PINNED
        self.assertEqual(pin["plugin_crate"]["version"], "2.10.1")
        self.assertEqual(pin["crates_from_binary_strings"]["tauri-plugin-updater"], ["2.10.1"])
        self.assertEqual(pin["crates_from_binary_strings"]["tauri"], ["2.11.2"])
        self.assertEqual(pin["release"]["package_sha256"], "db0df355bb17f23a02b9323adcd7506053a8969ee3d98a23cfe92d1b4219a9c3")
        self.assertEqual(pin["release"]["binary_sha256"], "63b1e4032ddc8fbbe3a31670d1182c08ec2e516e07f5d78baa01ab7b6033ab2d")
        self.assertEqual(pin["binary_string_counts"]["latest.json"], 1)
        self.assertEqual(pin["binary_string_counts"]["githubusercontent"], 0)
        self.assertIn("2.10.1", pin["plugin_crate"]["crates_io_cross_check"])
        self.assertEqual(duc.RELEASE_NOT_FOUND, "Could not fetch a valid release JSON from the remote")

    @unittest.skipUnless(PINNED_VERSIONS_JSON is not None, "pinned-versions.json not available locally")
    def test_pins_agree_with_the_captains_pinned_versions_file(self):
        document = json.loads(Path(PINNED_VERSIONS_JSON).read_text(encoding="utf-8"))
        crates = document["crates"]
        pin = duc.PINNED["crates_from_binary_strings"]
        self.assertEqual(crates["tauri-plugin-updater"], pin["tauri-plugin-updater"][0])
        self.assertEqual(crates["tauri"], pin["tauri"][0])
        self.assertEqual(crates["tauri-utils"], pin["tauri-utils"][0])
        self.assertEqual(crates["wry"], pin["wry"][0])
        self.assertEqual(crates["tao"], pin["tao"][0])
        self.assertEqual(crates["minisign-verify"], pin["minisign-verify"][0])
        self.assertEqual(crates["reqwest (app, commands.rs)"], pin["reqwest"][0])
        self.assertEqual(crates["reqwest (plugin 2.10.1 depends on ^0.13)"], pin["reqwest"][1])
        self.assertEqual(document["plugin_crate"]["sha256"], duc.PINNED["plugin_crate"]["sha256"])
        self.assertIn(duc.PINNED["release"]["package_sha256"], document["source"])
        self.assertIn(duc.PINNED["release"]["binary_sha256"], document["source"])


# --------------------------------------------------------------------------------------------------------------
# the fake GitHub
# --------------------------------------------------------------------------------------------------------------

class FakeGitHubTests(unittest.TestCase):
    def setUp(self):
        self.server = duc.FakeGitHub(duc.main_model()).start()
        self.addCleanup(self.server.stop)

    def fetch(self, path, host="github.com", accept="application/json", user_agent="test-agent/1"):
        return duc.http_fetch(self.server.base + path, user_agent, accept, 10, host=host)

    def test_api_latest_lists_the_five_launchers_and_sums_but_no_latest_json(self):
        self.server.current_step = "api"
        status, body = self.fetch(f"/repos/{REPO}/releases/latest", host="api.github.com")
        document = json.loads(body.decode("utf-8"))
        self.assertEqual(status, 200)
        self.assertEqual(document["tag_name"], "v0.7.12")
        names = sorted(asset["name"] for asset in document["assets"])
        self.assertEqual(names, sorted(list(duc.LAUNCHER_ASSETS) + ["SHA256SUMS"]))
        self.assertNotIn("latest.json", names)
        row = self.server.requests[0]
        self.assertEqual((row["step"], row["host_role"], row["host"], row["status"]), ("api", "api", "api.github.com", 200))

    def test_redirect_pair_is_recorded_once_each_in_order(self):
        self.server.current_step = "pair"
        status, _ = self.fetch(ENDPOINT_PATH)
        self.assertEqual(status, 404)
        rows = self.server.requests
        self.assertEqual([(r["seq"], r["method"], r["path"], r["status"]) for r in rows], [(1, "GET", ENDPOINT_PATH, 302), (2, "GET", f"/{REPO}/releases/download/v0.7.12/latest.json", 404)])
        self.assertTrue(all(r["step"] == "pair" and r["host_role"] == "github" and r["host"] == "github.com" for r in rows))
        self.assertTrue(all(r["user_agent"] == "test-agent/1" and r["accept"] == "application/json" for r in rows))
        self.assertEqual(rows[0]["bytes"], 0)
        self.assertEqual(rows[1]["bytes"], len(b"Not Found"))

    def test_existing_asset_is_served_after_the_redirect(self):
        status, body = self.fetch(f"/{REPO}/releases/latest/download/arc-node-linux-x86_64", accept="*/*")
        self.assertEqual((status, body), (200, duc.fake_bytes("v0.7.12", "arc-node-linux-x86_64")))
        self.assertEqual([r["status"] for r in self.server.requests], [302, 200])

    def test_request_rows_have_the_documented_keys_and_no_secret_headers(self):
        request = urllib.request.Request(self.server.base + f"/repos/{REPO}/releases/latest", headers={"User-Agent": "ua", "Accept": "*/*", "Authorization": "Bearer SECRET-TOKEN", "Cookie": "session=SECRET-COOKIE", "X-Api-Key": "SECRET-KEY"})
        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=10) as response:
            response.read()
        row = self.server.requests[0]
        required = {"seq", "method", "path", "query", "host_role", "user_agent", "accept", "status", "bytes", "note"}
        self.assertTrue(required <= set(row), required - set(row))
        dumped = json.dumps(self.server.requests)
        for secret in ("SECRET-TOKEN", "SECRET-COOKIE", "SECRET-KEY", "Authorization", "Cookie"):
            self.assertNotIn(secret, dumped)

    def test_query_string_is_recorded_separately(self):
        self.fetch(f"/repos/{REPO}/releases/latest?per_page=1&x=y", host="api.github.com")
        self.assertEqual((self.server.requests[0]["path"], self.server.requests[0]["query"]), (f"/repos/{REPO}/releases/latest", "per_page=1&x=y"))

    def test_head_records_zero_bytes_and_other_methods_are_405(self):
        base = self.server.base
        head = urllib.request.Request(base + f"/repos/{REPO}/releases/latest", method="HEAD")
        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(head, timeout=10) as response:
            self.assertEqual(response.status, 200)
        post = urllib.request.Request(base + f"/repos/{REPO}/releases/latest", data=b"{}", method="POST")
        with self.assertRaises(urllib.error.HTTPError) as raised:
            urllib.request.build_opener(urllib.request.ProxyHandler({})).open(post, timeout=10)
        self.assertEqual(raised.exception.code, 405)
        self.assertEqual([(r["method"], r["status"], r["bytes"]) for r in self.server.requests], [("HEAD", 200, 0), ("POST", 405, len(b"Method Not Allowed"))])

    def test_unknown_path_is_404_with_a_note(self):
        status, _ = self.fetch("/nowhere")
        self.assertEqual(status, 404)
        self.assertEqual(self.server.requests[0]["note"], "unknown path")

    def test_main_model_has_no_latest_json_while_the_bait_releases_have_one(self):
        model = duc.main_model()
        self.assertIsNone(model.asset("v0.7.12", "latest.json", "http://x"))
        self.assertEqual(json.loads(model.asset("v0.8.11", "latest.json", "http://x"))["version"], "0.8.11")
        self.assertEqual(json.loads(model.asset("v0.7.11", "latest.json", "http://x"))["version"], "0.7.11")
        mutated = duc.main_model(True)
        document = json.loads(mutated.asset("v0.7.12", "latest.json", "http://fake"))
        self.assertEqual(document["version"], "0.8.11")
        self.assertTrue(all(entry["url"].startswith("http://fake/") and "/v0.8.11/" in entry["url"] for entry in document["platforms"].values()))

    def test_latest_json_covers_every_updater_target_a_runner_can_have(self):
        document = json.loads(duc.latest_json_for("0.8.11", "v0.8.11", "http://fake"))
        self.assertEqual(sorted(document["platforms"]), ["darwin-aarch64", "darwin-x86_64", "linux-aarch64", "linux-x86_64", "windows-aarch64", "windows-x86_64"])
        for entry in document["platforms"].values():
            self.assertEqual(entry["signature"], duc.FAKE_SIGNATURE)

    def test_control_model_variants(self):
        carries = duc.control_model(True)
        self.assertEqual(carries.latest_tag, "v0.8.11")
        self.assertEqual(json.loads(carries.asset("v0.8.11", "latest.json", "http://x"))["version"], "0.8.11")
        without = duc.control_model(False)
        self.assertEqual(without.latest_tag, "v0.7.12")
        self.assertIsNone(without.asset("v0.7.12", "latest.json", "http://x"))

    def test_server_binds_loopback_only(self):
        host, port = self.server.httpd.server_address[:2]
        self.assertEqual(host, "127.0.0.1")
        self.assertNotEqual(port, 0)
        self.assertEqual(self.server.base, f"http://127.0.0.1:{port}")


# --------------------------------------------------------------------------------------------------------------
# replicas of the app / plugin constructs
# --------------------------------------------------------------------------------------------------------------

class ReplicaTests(TempCase):
    PLUGIN_UA = "tauri-plugin-updater/2.10.1"

    def serve(self, routes=None, model=None):
        server = ScriptedGitHub(routes, model).start()
        self.addCleanup(server.stop)
        return server

    def tauri_check(self, server, endpoints=None, version="0.7.11", blocked=None):
        return duc.replica_tauri_check(endpoints or [duc.TAURI_ENDPOINT], server.base, server.base, version, self.PLUGIN_UA, blocked)

    def test_404_after_the_redirect_is_release_not_found_and_nothing_else_is_requested(self):
        server = self.serve()
        outcome = self.tauri_check(server)
        self.assertEqual((outcome["outcome"], outcome["error_kind"], outcome["error"], outcome["update"], outcome["download_attempted"]), ("error", "ReleaseNotFound", duc.RELEASE_NOT_FOUND, None, False))
        self.assertEqual([(r["path"], r["status"]) for r in server.requests], [(ENDPOINT_PATH, 302), (f"/{REPO}/releases/download/v0.7.12/latest.json", 404)])
        self.assertTrue(all(r["user_agent"] == self.PLUGIN_UA and r["accept"] == "application/json" for r in server.requests))

    def test_204_means_no_update(self):
        server = self.serve({ENDPOINT_PATH: (204, {}, b"")})
        outcome = self.tauri_check(server)
        self.assertEqual((outcome["outcome"], outcome["error"]), ("no_update", None))
        self.assertEqual(len(server.requests), 1)

    def test_equal_and_older_versions_are_no_update_newer_is_an_update(self):
        for version, expected in (("0.7.11", "no_update"), ("0.7.0", "no_update"), ("0.8.11", "update_available"), ("v0.7.12", "update_available")):
            server = self.serve({ENDPOINT_PATH: (200, JSON_HEADERS, release_json(version))})
            outcome = self.tauri_check(server)
            self.assertEqual(outcome["outcome"], expected, version)
            if expected == "update_available":
                self.assertTrue(outcome["update"]["download_url"].startswith("http://example.invalid/"))
                self.assertFalse(outcome["download_attempted"])

    def test_non_success_statuses_are_only_logged_and_the_loop_moves_on(self):
        second = "https://github.com/other/repo/releases/latest/download/latest.json"
        server = self.serve({ENDPOINT_PATH: (500, {}, b"boom"), "/other/repo/releases/latest/download/latest.json": (200, JSON_HEADERS, release_json("0.9.0"))})
        outcome = self.tauri_check(server, [duc.TAURI_ENDPOINT, second])
        self.assertEqual(outcome["outcome"], "update_available")
        self.assertEqual([r["status"] for r in server.requests], [500, 200])

    def test_a_second_endpoint_is_what_would_reveal_a_fallback(self):
        bait = f"https://github.com/{REPO}/releases/download/v0.8.11/latest.json"
        server = self.serve()
        outcome = self.tauri_check(server, [duc.TAURI_ENDPOINT, bait])
        self.assertEqual(outcome["outcome"], "update_available")
        self.assertEqual(outcome["update"]["version"], "0.8.11")
        self.assertIn(f"/{REPO}/releases/download/v0.8.11/latest.json", [r["path"] for r in server.requests])

    def test_a_non_json_2xx_body_returns_the_reqwest_error_at_once(self):
        second = "https://github.com/other/repo/releases/latest/download/latest.json"
        server = self.serve({ENDPOINT_PATH: (200, {}, b"<html>not json</html>"), "/other/repo/releases/latest/download/latest.json": (200, JSON_HEADERS, release_json("0.9.0"))})
        outcome = self.tauri_check(server, [duc.TAURI_ENDPOINT, second])
        self.assertEqual((outcome["outcome"], outcome["error_kind"]), ("error", "Reqwest"))
        self.assertEqual(len(server.requests), 1, "the second endpoint must not be tried")

    def test_a_json_that_is_not_a_release_is_recorded_and_the_loop_continues(self):
        second = "https://github.com/other/repo/releases/latest/download/latest.json"
        server = self.serve({ENDPOINT_PATH: (200, JSON_HEADERS, b'{"hello": 1}'), "/other/repo/releases/latest/download/latest.json": (404, {}, b"no")})
        outcome = self.tauri_check(server, [duc.TAURI_ENDPOINT, second])
        self.assertEqual((outcome["outcome"], outcome["error_kind"]), ("error", "Json"))
        self.assertEqual(len(server.requests), 2)

    def test_missing_platform_is_targets_not_found(self):
        document = json.dumps({"version": "0.9.0", "platforms": {"plan9-mips": {"signature": "s", "url": "http://example.invalid/x"}}}).encode()
        server = self.serve({ENDPOINT_PATH: (200, JSON_HEADERS, document)})
        outcome = self.tauri_check(server)
        self.assertEqual((outcome["outcome"], outcome["error_kind"]), ("error", "TargetsNotFound"))
        self.assertIn("None of the fallback platforms", outcome["error"])

    def test_endpoint_template_variables_are_substituted(self):
        server = self.serve({"/x/0.7.11/bundle": (404, {}, b"")})
        self.tauri_check(server, ["https://github.com/x/{{current_version}}/{{bundle_type}}"])
        self.assertEqual(server.requests[0]["path"], "/x/0.7.11/unknown")

    def test_foreign_hosts_are_blocked_never_contacted(self):
        server = self.serve()
        blocked: List[str] = []
        outcome = self.tauri_check(server, ["https://example.invalid/latest.json"], blocked=blocked)
        self.assertEqual(server.requests, [])
        self.assertEqual(len(blocked), 1)
        self.assertIn("example.invalid", blocked[0])
        self.assertEqual(outcome["outcome"], "error")

    def test_check_for_update_replica(self):
        server = self.serve()
        outcome = duc.replica_check_for_update(server.base)
        self.assertEqual((outcome["has_update"], outcome["version"], outcome["http_status"]), (True, "0.7.12", 200))
        self.assertEqual((server.requests[0]["user_agent"], server.requests[0]["accept"], server.requests[0]["host"]), ("arc-desktop/0.1", "*/*", "api.github.com"))
        same = self.serve({f"/repos/{REPO}/releases/latest": (200, JSON_HEADERS, b'{"tag_name": "v0.7.11"}')})
        self.assertFalse(duc.replica_check_for_update(same.base)["has_update"])
        rate_limited = self.serve({f"/repos/{REPO}/releases/latest": (403, JSON_HEADERS, b'{"message": "API rate limit exceeded"}')})
        limited = duc.replica_check_for_update(rate_limited.base)
        self.assertEqual((limited["has_update"], limited["version"], limited["http_status"]), (False, "unknown", 403))
        broken = self.serve({f"/repos/{REPO}/releases/latest": (200, {}, b"<html>")})
        with self.assertRaises(duc.CommandError):
            duc.replica_check_for_update(broken.base)

    def test_install_click_downloads_only_when_the_plugin_returns_an_update(self):
        server = self.serve()
        error = duc.replica_install_click([duc.TAURI_ENDPOINT], server.base, server.base, "0.7.11", self.PLUGIN_UA)
        self.assertEqual((error["downloaded"], error["ui_error_text"]), (False, "Update failed: " + duc.RELEASE_NOT_FOUND))
        none = self.serve({ENDPOINT_PATH: (204, {}, b"")})
        nothing = duc.replica_install_click([duc.TAURI_ENDPOINT], none.base, none.base, "0.7.11", self.PLUGIN_UA)
        self.assertEqual((nothing["downloaded"], nothing["ui_error_text"]), (False, "Update failed: No update available."))
        bait = self.serve(model=duc.control_model(True))
        found = duc.replica_install_click([duc.TAURI_ENDPOINT], bait.base, bait.base, "0.7.11", self.PLUGIN_UA)
        self.assertTrue(found["downloaded"])
        self.assertIsNone(found["ui_error_text"])
        octet = [r for r in bait.requests if r["accept"] == "application/octet-stream"]
        self.assertEqual(len(octet), 1)
        self.assertEqual(octet[0]["status"], 200)

    def test_ensure_binary_replica_writes_only_the_launcher(self):
        server = self.serve()
        home = self.tmpdir()
        asset = "arc-node-linux-x86_64"
        outcome = duc.replica_ensure_binary(home, server.base, duc.SIDECAR_TEMPLATE, asset, windows=False)
        self.assertEqual(outcome["target"], ".arc/bin/arc-node")
        self.assertEqual(sorted(duc.snapshot_tree(home)), [".arc", ".arc/bin", ".arc/bin/arc-node"])
        self.assertEqual((home / ".arc" / "bin" / "arc-node").read_bytes(), duc.fake_bytes("v0.7.12", asset))
        self.assertEqual([(r["user_agent"], r["accept"], r["host"], r["status"]) for r in server.requests], [("arc-desktop/0.1", "*/*", "github.com", 302), ("arc-desktop/0.1", "*/*", "github.com", 200)])

    def test_ensure_binary_replica_windows_name_and_http_error(self):
        server = self.serve()
        home = self.tmpdir()
        outcome = duc.replica_ensure_binary(home, server.base, duc.SIDECAR_TEMPLATE, "arc-node-windows-x86_64.exe", windows=True)
        self.assertEqual(outcome["target"], ".arc/bin/arc-node.exe")
        with self.assertRaises(duc.CommandError):
            duc.replica_ensure_binary(self.tmpdir(), server.base, duc.SIDECAR_TEMPLATE, "arc-node-missing", windows=False)

    def test_platform_asset_lookup(self):
        mapping = fake_source()["extracted"]["platform_assets"]
        self.assertIn(duc.platform_asset(mapping), mapping.values())
        with mock.patch("platform.machine", return_value="riscv64"):
            with self.assertRaises(duc.CommandError):
                duc.platform_asset(mapping)

    def test_updater_targets_for_each_supported_machine(self):
        for machine, arch in (("x86_64", "x86_64"), ("AMD64", "x86_64"), ("arm64", "aarch64"), ("aarch64", "aarch64")):
            with mock.patch("platform.machine", return_value=machine):
                self.assertEqual(duc.updater_targets()[1], arch)
        with mock.patch("platform.machine", return_value="riscv64"):
            with self.assertRaises(duc.TauriError):
                duc.updater_targets()


# --------------------------------------------------------------------------------------------------------------
# miniature plugin source: the analyzer's logic, hermetic
# --------------------------------------------------------------------------------------------------------------

MINI_UPDATER = '''const UPDATER_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

impl UpdaterBuilder {
    fn endpoints(&mut self, endpoints: Vec<Url>) -> Result<&mut Self> {
        crate::config::validate_endpoints(&endpoints, self.config.dangerous_insecure_transport_protocol)?;
        Ok(self)
    }
}

impl Updater {
    pub async fn check(&self) -> Result<Option<Update>> {
        // we want JSON only
        let mut headers = self.headers.clone();
        if !headers.contains_key(ACCEPT) {
            headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        }
        let mut remote_release: Option<RemoteRelease> = None;
        let mut last_error: Option<Error> = None;
        for url in &self.endpoints {
            let mut request = ClientBuilder::new().user_agent(UPDATER_USER_AGENT);
            let response = request.send().await;
            match response {
                Ok(res) => {
                    if res.status().is_success() {
                        // no updates found!
                        if StatusCode::NO_CONTENT == res.status() {
                            return Ok(None);
                        };
                        let update_response: serde_json::Value = res.json().await?;
                        match serde_json::from_value::<RemoteRelease>(update_response) {
                            Ok(release) => {
                                last_error = None;
                                remote_release = Some(release);
                                break;
                            }
                            Err(err) => {
                                last_error = Some(err)
                            }
                        }
                    } else {
                        log::error!(
                            "update endpoint did not respond with a successful status code"
                        );
                    }
                }
                Err(err) => {
                    last_error = Some(err.into())
                }
            }
        }

        if let Some(error) = last_error {
            return Err(error);
        }

        let release = remote_release.ok_or(Error::ReleaseNotFound)?;
        let should_update = match self.version_comparator.as_ref() {
            Some(comparator) => comparator(self.current_version.clone(), release.clone()),
            None => release.version > self.current_version,
        };
        let installer = installer_for_bundle_type(bundle_type());
        let (download_url, signature) = self.get_urls(&release, &installer)?;
        Ok(None)
    }
}

impl Update {
    fn install(&self) -> Result<()> {
        let dir = tempfile::Builder::new().tempdir()?;
        Ok(())
    }
}
'''

MINI_ERROR = '''pub enum Error {
    /// Could not fetch a valid response from the server.
    #[error("Could not fetch a valid release JSON from the remote")]
    ReleaseNotFound,
}

impl Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.to_string().as_ref())
    }
}
'''

MINI_COMMANDS = '''pub(crate) async fn check<R: Runtime>(
    webview: Webview<R>,
    headers: Option<Vec<(String, String)>>,
    timeout: Option<u64>,
    proxy: Option<String>,
    target: Option<String>,
    allow_downgrades: Option<bool>,
) -> Result<Option<Metadata>> {
    let mut builder = webview.updater_builder();
    Ok(None)
}
'''


def mini_files(**replacements: tuple) -> Dict[str, str]:
    files = {"src/updater.rs": MINI_UPDATER, "src/error.rs": MINI_ERROR, "src/commands.rs": MINI_COMMANDS, "src/config.rs": "", "src/lib.rs": ""}
    for name, (old, new) in replacements.items():
        file_name = {"updater": "src/updater.rs", "error": "src/error.rs", "commands": "src/commands.rs"}[name.split("__")[0]]
        assert old in files[file_name], old
        files[file_name] = files[file_name].replace(old, new)
    return files


def failing_findings(analysis: dict) -> List[str]:
    return sorted(text.split(":", 1)[0] for text in analysis["contradictions"])


class MiniPluginAnalyzerTests(unittest.TestCase):
    def test_the_miniature_source_is_confirmed(self):
        analysis = duc.analyze_plugin_source(mini_files())
        self.assertEqual(analysis["status"], "confirmed", analysis["contradictions"])
        ids = [finding["id"] for finding in analysis["findings"]]
        for expected in ("user-agent", "accept-json", "endpoint-iteration", "non-success-continues", "none-left-means-release-not-found", "release-not-found-display-text", "errors-reach-js-as-display-strings", "redirect-default-policy"):
            self.assertIn(expected, ids)
        self.assertEqual(analysis["release_not_found_text"], duc.RELEASE_NOT_FOUND)

    def test_a_404_that_records_an_error_is_contradicted(self):
        files = mini_files(updater__a=('"update endpoint did not respond with a successful status code"\n                        );', '"update endpoint did not respond with a successful status code"\n                        );\n                        last_error = Some(Error::ReleaseNotFound)'))
        self.assertEqual(failing_findings(duc.analyze_plugin_source(files)), ["non-success-continues"])

    def test_a_404_that_returns_is_contradicted(self):
        files = mini_files(updater__a=('"update endpoint did not respond with a successful status code"\n                        );', '"update endpoint did not respond with a successful status code"\n                        );\n                        return Ok(None);'))
        self.assertEqual(failing_findings(duc.analyze_plugin_source(files)), ["non-success-continues"])

    def test_no_release_found_must_map_to_release_not_found(self):
        files = mini_files(updater__a=("remote_release.ok_or(Error::ReleaseNotFound)?", "remote_release.ok_or(Error::EmptyEndpoints)?"))
        self.assertIn("none-left-means-release-not-found", failing_findings(duc.analyze_plugin_source(files)))

    def test_redirect_configuration_anywhere_is_contradicted(self):
        files = mini_files()
        files["src/lib.rs"] = "fn x() { builder.redirect(reqwest::redirect::Policy::none()); }"
        self.assertEqual(failing_findings(duc.analyze_plugin_source(files)), ["redirect-default-policy"])

    def test_file_io_inside_check_is_contradicted(self):
        files = mini_files(updater__a=("// we want JSON only", 'std::fs::write("latest.json", b"x");'))
        self.assertEqual(failing_findings(duc.analyze_plugin_source(files)), ["no-disk-cache-in-check"])

    def test_cache_or_temp_directory_use_inside_check_is_contradicted(self):
        files = mini_files(updater__a=("// we want JSON only", "let dir = dirs::cache_dir();"))
        failing = failing_findings(duc.analyze_plugin_source(files))
        self.assertIn("no-disk-cache-in-check", failing)
        self.assertIn("disk-use-only-after-download", failing)

    def test_a_changed_version_rule_is_contradicted(self):
        files = mini_files(updater__a=("None => release.version > self.current_version,", "None => release.version != self.current_version,"))
        self.assertEqual(failing_findings(duc.analyze_plugin_source(files)), ["semver-compare"])

    def test_204_must_return_no_update_immediately(self):
        files = mini_files(updater__a=("return Ok(None);\n                        };", "return Err(Error::ReleaseNotFound);\n                        };"))
        self.assertIn("204-means-no-update", failing_findings(duc.analyze_plugin_source(files)))

    def test_a_reordered_endpoint_walk_is_contradicted(self):
        files = mini_files(updater__a=("for url in &self.endpoints {", "for url in self.endpoints.iter().rev() {"))
        self.assertIn("endpoint-iteration", failing_findings(duc.analyze_plugin_source(files)))

    def test_missing_accept_header_is_contradicted(self):
        files = mini_files(updater__a=('headers.insert(ACCEPT, HeaderValue::from_static("application/json"));', "headers.clear();"))
        self.assertEqual(failing_findings(duc.analyze_plugin_source(files)), ["accept-json"])

    def test_the_error_text_the_button_shows_is_read_from_the_crate(self):
        files = mini_files(error__a=("Could not fetch a valid release JSON from the remote", "Release not found"))
        analysis = duc.analyze_plugin_source(files)
        self.assertEqual(failing_findings(analysis), ["release-not-found-display-text"])
        self.assertEqual(analysis["release_not_found_text"], "Release not found")

    def test_errors_must_reach_the_js_side_as_display_strings(self):
        files = mini_files(error__a=("serializer.serialize_str(self.to_string().as_ref())", "serializer.serialize_unit()"))
        self.assertEqual(failing_findings(duc.analyze_plugin_source(files)), ["errors-reach-js-as-display-strings"])

    def test_the_js_check_command_must_not_take_endpoints(self):
        files = mini_files(commands__a=("webview: Webview<R>,", "webview: Webview<R>,\n    endpoints: Option<Vec<url::Url>>,"))
        self.assertEqual(failing_findings(duc.analyze_plugin_source(files)), ["js-check-cannot-override-endpoints"])

    def test_a_missing_check_function_returns_early_as_contradicted(self):
        files = mini_files(updater__a=("pub async fn check(&self)", "pub async fn check_all(&self)"))
        analysis = duc.analyze_plugin_source(files)
        self.assertEqual(analysis["status"], "contradicted")
        self.assertTrue(any(text.startswith("check-fn") for text in analysis["contradictions"]))

    def test_pinned_line_numbers_are_enforced_only_when_given(self):
        files = mini_files()
        files["src/updater.rs"] = "\n" + files["src/updater.rs"]
        self.assertEqual(duc.analyze_plugin_source(files)["status"], "confirmed")
        analysis = duc.analyze_plugin_source(files, {"check_fn": 11})
        self.assertEqual(analysis["status"], "contradicted")
        self.assertTrue(any("line numbers differ" in text for text in analysis["contradictions"]))
        self.assertEqual(duc.analyze_plugin_source(files, {"check_fn": 12})["status"], "confirmed")


# --------------------------------------------------------------------------------------------------------------
# plugin_semantics with the real pinned crate (local copy) and the fetch hook (no network)
# --------------------------------------------------------------------------------------------------------------

def crate_files(path: Path) -> Dict[str, str]:
    files = {}
    with tarfile.open(fileobj=io.BytesIO(Path(path).read_bytes()), mode="r:gz") as archive:
        for member in archive.getmembers():
            name = member.name.split("/", 1)[1] if "/" in member.name else member.name
            if member.isfile() and name in duc.PINNED["plugin_crate"]["file_sha256"]:
                files[name] = archive.extractfile(member).read().decode("utf-8")  # type: ignore[union-attr]
    return files


@needs_crate
class RealPluginCrateTests(unittest.TestCase):
    def test_local_crate_is_the_pinned_one_and_confirms_the_isolation_semantics(self):
        semantics = duc.plugin_semantics(PLUGIN_CRATE)
        self.assertEqual(semantics["status"], "confirmed", semantics.get("contradictions"))
        self.assertEqual(semantics["crate"], "tauri-plugin-updater 2.10.1")
        self.assertEqual(semantics["crate_sha256_observed"], duc.PINNED["plugin_crate"]["sha256"])
        self.assertEqual(semantics["file_sha256_observed"], duc.PINNED["plugin_crate"]["file_sha256"])
        self.assertEqual(semantics["lines"], duc.PLUGIN_EXPECTED_LINES)
        self.assertEqual(semantics["release_not_found_text"], duc.RELEASE_NOT_FOUND)
        self.assertIsNone(semantics.get("loud_note"))
        self.assertTrue(all(finding["ok"] for finding in semantics["findings"]))

    def test_the_cited_lines_say_what_the_findings_claim(self):
        files = crate_files(PLUGIN_CRATE)
        updater = files["src/updater.rs"].split("\n")
        lines = duc.PLUGIN_EXPECTED_LINES
        self.assertIn("UPDATER_USER_AGENT", updater[lines["user_agent_const"] - 1])
        self.assertIn("for url in &self.endpoints", updater[lines["endpoint_loop"] - 1])
        self.assertIn("is_success()", updater[lines["is_success"] - 1])
        self.assertIn("NO_CONTENT", updater[lines["no_content"] - 1])
        self.assertEqual(updater[lines["non_success_branch"] - 1].strip(), "} else {")
        self.assertIn("did not respond with a successful status code", "\n".join(updater[lines["non_success_branch"]:lines["non_success_branch"] + 3]))
        self.assertIn("ReleaseNotFound", updater[lines["release_not_found"] - 1])
        self.assertIn("release.version > self.current_version", updater[lines["version_compare"] - 1])
        errors = files["src/error.rs"].split("\n")
        self.assertIn("Could not fetch a valid release JSON from the remote", errors[lines["error_release_not_found_text"] - 1])
        self.assertEqual(errors[lines["error_release_not_found_variant"] - 1].strip(), "ReleaseNotFound,")
        self.assertIn("serialize_str(self.to_string()", errors[lines["error_serialize_str"] - 1])

    def test_mutating_the_real_updater_flips_the_status(self):
        files = crate_files(PLUGIN_CRATE)
        updater = files["src/updater.rs"]
        for old, new, finding in (
            ('log::error!(\n                            "update endpoint did not respond with a successful status code"\n                        );', 'log::error!(\n                            "update endpoint did not respond with a successful status code"\n                        );\n                        last_error = Some(Error::ReleaseNotFound);', "non-success-continues"),
            ("remote_release.ok_or(Error::ReleaseNotFound)?", "remote_release.ok_or(Error::EmptyEndpoints)?", "none-left-means-release-not-found"),
            ("None => release.version > self.current_version,", "None => release.version != self.current_version,", "semver-compare"),
            ("// we want JSON only", 'std::fs::write("latest.json", b"x");', "no-disk-cache-in-check"),
        ):
            self.assertIn(old, updater)
            mutated = dict(files, **{"src/updater.rs": updater.replace(old, new)})
            analysis = duc.analyze_plugin_source(mutated, duc.PLUGIN_EXPECTED_LINES)
            self.assertEqual(analysis["status"], "contradicted", finding)
            self.assertIn(finding, failing_findings(analysis))

    def test_a_shifted_line_breaks_the_pinned_reading_but_not_the_semantics(self):
        files = crate_files(PLUGIN_CRATE)
        shifted = dict(files, **{"src/updater.rs": "\n" + files["src/updater.rs"]})
        self.assertEqual(duc.analyze_plugin_source(shifted)["status"], "confirmed")
        self.assertEqual(duc.analyze_plugin_source(shifted, duc.PLUGIN_EXPECTED_LINES)["status"], "contradicted")

    def test_changed_error_text_in_the_real_crate_is_contradicted(self):
        files = crate_files(PLUGIN_CRATE)
        mutated = dict(files, **{"src/error.rs": files["src/error.rs"].replace("Could not fetch a valid release JSON from the remote", "Nothing found")})
        self.assertIn("release-not-found-display-text", failing_findings(duc.analyze_plugin_source(mutated)))

    def test_fetch_hook_reads_the_crate_without_any_network(self):
        data = Path(PLUGIN_CRATE).read_bytes()
        seen = []

        def fetch(url):
            seen.append(url)
            return data

        semantics = duc.plugin_semantics(fetch=fetch)
        self.assertEqual(seen, [duc.PINNED["plugin_crate"]["url"]])
        self.assertEqual(semantics["status"], "confirmed")
        self.assertEqual(semantics["source"], duc.PINNED["plugin_crate"]["url"])
        self.assertTrue(duc.plugin_acceptable(semantics))


class PluginSemanticsStatusTests(unittest.TestCase):
    def test_offline_is_unverified_but_acceptable_and_loud(self):
        semantics = duc.plugin_semantics(offline=True)
        self.assertEqual((semantics["status"], semantics["reason"]), ("unverified", "offline-requested"))
        self.assertIn("NOT VERIFIED", semantics["loud_note"])
        self.assertTrue(duc.plugin_acceptable(semantics))

    def test_a_fetch_failure_is_unverified_and_not_acceptable(self):
        def fail(url):
            raise urllib.error.URLError("simulated outage")

        semantics = duc.plugin_semantics(fetch=fail)
        self.assertEqual((semantics["status"], semantics["reason"]), ("unverified", "fetch-failed"))
        self.assertIn("simulated outage", semantics["loud_note"])
        self.assertFalse(duc.plugin_acceptable(semantics))

    def test_a_missing_local_crate_file_is_a_fetch_failure(self):
        semantics = duc.plugin_semantics(Path("/nonexistent/crate-file.crate"))
        self.assertEqual((semantics["status"], semantics["reason"]), ("unverified", "fetch-failed"))
        self.assertFalse(duc.plugin_acceptable(semantics))

    def test_a_different_crate_is_contradicted_by_its_digest(self):
        semantics = duc.plugin_semantics(fetch=lambda url: b"not the pinned crate")
        self.assertEqual(semantics["status"], "contradicted")
        self.assertIn("DIGEST MISMATCH", semantics["loud_note"])
        self.assertFalse(duc.plugin_acceptable(semantics))


# --------------------------------------------------------------------------------------------------------------
# a stub native checker (a tiny script standing in for F5's Rust binary)
# --------------------------------------------------------------------------------------------------------------

STUB = r'''
import json
import os
import sys
import urllib.error
import urllib.request

args = sys.argv[1:]


def arg(name):
    return args[args.index(name) + 1]


mode = os.environ.get("STUB_MODE", "normal")
endpoint = arg("--endpoint")
current = arg("--current-version")
ua = "curl/8.0" if mode == "wrong_ua" else "tauri-plugin-updater/2.10.1"
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def get(url, accept):
    request = urllib.request.Request(url, headers={"User-Agent": ua, "Accept": accept})
    try:
        with opener.open(request, timeout=20) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def key(version):
    return tuple(int(part) for part in version.split("."))


if mode == "bad_json":
    print("this is not json")
    sys.exit(0)
if mode == "bad_outcome":
    print(json.dumps({"schema": "arc.legacy-bridge.wave0-lab.native-updater-check.v1", "plugin": "x", "tauri": "x", "current_version": current, "endpoints": [], "outcome": "maybe", "error_kind": None, "error": None, "update": None, "download_attempted": False}))
    sys.exit(0)
if mode == "writes_cache":
    folder = os.path.join(os.environ["XDG_CACHE_HOME"], "updater")
    os.makedirs(folder, exist_ok=True)
    with open(os.path.join(folder, "latest.json"), "w") as handle:
        handle.write("{}")
if mode == "writes_cwd":
    with open("update-cache.json", "w") as handle:
        handle.write(endpoint)
if mode == "writes_hidden_cache":
    with open(os.path.join(os.environ["HOME"], ".state"), "w") as handle:
        handle.write("last endpoint: " + endpoint)
if mode == "writes_noise":
    with open(os.path.join(os.environ["HOME"], ".noise"), "w") as handle:
        handle.write("hello")

report = {
    "schema": "arc.legacy-bridge.wave0-lab.native-updater-check.v1",
    "plugin": "tauri-plugin-updater 2.9.0" if mode == "wrong_plugin" else "tauri-plugin-updater 2.10.1",
    "tauri": "2.11.2",
    "current_version": current,
    "endpoints": [endpoint],
    "outcome": "error",
    "error_kind": None,
    "error": None,
    "update": None,
    "download_attempted": False,
}
status, body = get(endpoint, "application/json")
if mode == "extra_request":
    base = endpoint.split("/FerrumVir")[0]
    get(base + "/FerrumVir/arc-chain/releases/download/v0.8.11/ARC.Node_aarch64.app.tar.gz", "application/octet-stream")
if 200 <= status < 300:
    release = json.loads(body.decode("utf-8"))
    if key(release["version"]) > key(current):
        url = release["platforms"]["darwin-aarch64"]["url"]
        report["outcome"] = "update_available"
        report["update"] = {"version": release["version"], "download_url": url}
        wants = ("--control-download" in args and mode != "no_download_with_flag") or mode == "download_without_flag"
        if wants:
            report["download_attempted"] = True
            get(url, "application/octet-stream")
            report["download_result"] = "error: signature"
    else:
        report["outcome"] = "no_update"
elif mode == "no_update_on_404":
    report["outcome"] = "no_update"
else:
    report["error_kind"] = "Network" if mode == "wrong_kind" else "ReleaseNotFound"
    report["error"] = "something else entirely" if mode == "lying_error_text" else "Could not fetch a valid release JSON from the remote"
print(json.dumps(report))
sys.exit(3 if mode == "exit_code_three" else 0)
'''


class StubCase(TempCase):
    @classmethod
    def setUpClass(cls):
        cls._stub_holder = tempfile.TemporaryDirectory()
        cls.stub_path = Path(cls._stub_holder.name) / "native_stub.py"
        cls.stub_path.write_text(STUB, encoding="utf-8")
        cls.native = [sys.executable, str(cls.stub_path)]

    @classmethod
    def tearDownClass(cls):
        cls._stub_holder.cleanup()

    def run_with(self, mode="normal", native=False, **kwargs):
        evidence = self.tmpdir() / "evidence"
        parent = self.tmpdir()
        kwargs.setdefault("source", fake_source())
        with mock.patch.dict(os.environ, {"STUB_MODE": mode, "PYTHONDONTWRITEBYTECODE": "1"}):  # no bytecode caches in the sandboxed home
            return duc.execute(None, evidence, "linux", native_check=self.native if native else None, home_parent=parent, **kwargs), evidence


# --------------------------------------------------------------------------------------------------------------
# the run: baseline, request log, mutations (hermetic)
# --------------------------------------------------------------------------------------------------------------

class RunBaselineTests(StubCase):
    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        cls.parent = tempfile.TemporaryDirectory()
        cls.evidence = Path(cls.parent.name) / "evidence"
        cls.result = duc.execute(None, cls.evidence, "linux", source=fake_source(), home_parent=Path(cls.parent.name))

    @classmethod
    def tearDownClass(cls):
        cls.parent.cleanup()
        super().tearDownClass()

    def test_verdict_and_assertion_states(self):
        self.assertEqual(self.result["verdict"], "DESKTOP_UPDATER_ISOLATED")
        self.assertEqual(states(self.result), {"I1": "PASS", "I2": "PASS", "I3": "PASS", "I4": "PASS", "I5": "PASS", "I6": "PASS", "I7": "PASS", "I8": "SKIP", "I9": "SKIP"})

    def test_native_assertions_are_skipped_with_a_reason_never_passed(self):
        for name in ("I8", "I9"):
            entry = by_id(self.result)[name]
            self.assertEqual(entry["result"], "SKIP")
            self.assertIn("NOT exercised", entry["detail"])
        self.assertEqual(self.result["native_check"], {"supplied": False, "main": "not supplied", "control": "not supplied", "control_download": "not supplied"})

    def test_result_document_fields(self):
        result = self.result
        self.assertEqual(result["schema"], "arc.legacy-bridge.wave0-lab.stage-c-result.v1")
        self.assertEqual(result["label"], "linux")
        self.assertEqual(result["tag"], "v0.7.11")
        self.assertEqual(result["python"], sys.version.split()[0])
        self.assertTrue(result["platform"])
        self.assertEqual(result["request_count"], 7)
        self.assertEqual(result["asset"], sidecar_asset())
        self.assertEqual(result["blocked_hosts"], [])
        self.assertEqual(result["live_observation"], {"supplied": False, "note": "not supplied"})
        self.assertEqual(result["sources"], fake_source()["sources"])
        self.assertTrue(any("302" in note and "404" in note and "not made by this run" in note for note in result["notes"]))
        self.assertTrue(any("CDN" in note for note in result["notes"]))

    def test_evidence_files_exist_and_parse(self):
        names = sorted(path.name for path in self.evidence.iterdir())
        self.assertEqual(names, sorted(["check-source.json", "control-requests.jsonl", "home-after-check.json", "home-after.json", "home-before.json", "requests.jsonl", "result-linux.json", "result.json", "ui-text.txt"]))
        for name in ("check-source.json", "home-after-check.json", "home-after.json", "home-before.json", "result-linux.json", "result.json"):
            json.loads((self.evidence / name).read_text(encoding="utf-8"))
        self.assertEqual((self.evidence / "result.json").read_bytes(), (self.evidence / "result-linux.json").read_bytes())
        self.assertEqual(json.loads((self.evidence / "result.json").read_text(encoding="utf-8")), self.result)
        self.assertEqual(json.loads((self.evidence / "home-before.json").read_text(encoding="utf-8")), {})
        self.assertNotIn(b"\r\n", (self.evidence / "result.json").read_bytes())

    def test_exact_request_list(self):
        rows = [json.loads(line) for line in (self.evidence / "requests.jsonl").read_text(encoding="utf-8").splitlines()]
        asset = sidecar_asset()
        expected = [
            ("settings_check", "GET", "api", f"/repos/{REPO}/releases/latest", 200, "arc-desktop/0.1"),
            ("tauri_check", "GET", "github", ENDPOINT_PATH, 302, "tauri-plugin-updater/2.10.1"),
            ("tauri_check", "GET", "github", f"/{REPO}/releases/download/v0.7.12/latest.json", 404, "tauri-plugin-updater/2.10.1"),
            ("install_click", "GET", "github", ENDPOINT_PATH, 302, "tauri-plugin-updater/2.10.1"),
            ("install_click", "GET", "github", f"/{REPO}/releases/download/v0.7.12/latest.json", 404, "tauri-plugin-updater/2.10.1"),
            ("sidecar_refresh", "GET", "github", f"/{REPO}/releases/latest/download/{asset}", 302, "arc-desktop/0.1"),
            ("sidecar_refresh", "GET", "github", f"/{REPO}/releases/download/v0.7.12/{asset}", 200, "arc-desktop/0.1"),
        ]
        self.assertEqual([(r["step"], r["method"], r["host_role"], r["path"], r["status"], r["user_agent"]) for r in rows], expected)
        self.assertEqual([r["seq"] for r in rows], list(range(1, 8)))
        self.assertEqual([r["host"] for r in rows], ["api.github.com"] + ["github.com"] * 6)

    def test_request_rows_carry_the_documented_keys_and_no_secrets(self):
        rows = [json.loads(line) for line in (self.evidence / "requests.jsonl").read_text(encoding="utf-8").splitlines()]
        required = {"seq", "method", "path", "query", "host_role", "user_agent", "accept", "status", "bytes", "note"}
        for row in rows:
            self.assertTrue(required <= set(row))
        text = (self.evidence / "requests.jsonl").read_text(encoding="utf-8").lower()
        for secret in ("authorization", "cookie", "token", "bearer"):
            self.assertNotIn(secret, text)

    def test_control_requests_show_the_bait_being_followed(self):
        rows = [json.loads(line) for line in (self.evidence / "control-requests.jsonl").read_text(encoding="utf-8").splitlines()]
        self.assertEqual(self.result["control_request_count"], len(rows))
        paths = [(r["step"], r["path"], r["status"]) for r in rows]
        # the bait manifest lists one bundle per updater target; the harness follows the entry of the HOST it runs on
        os_name, arch = duc.updater_targets()
        bundle = {
            "darwin-aarch64": "ARC.Node_aarch64.app.tar.gz", "darwin-x86_64": "ARC.Node_x64.app.tar.gz",
            "linux-x86_64": "ARC.Node_0.8.11_amd64.AppImage", "linux-aarch64": "ARC.Node_0.8.11_aarch64.AppImage",
            "windows-x86_64": "ARC.Node_0.8.11_x64-setup.exe", "windows-aarch64": "ARC.Node_0.8.11_arm64-setup.exe",
        }["%s-%s" % (os_name, arch)]
        self.assertIn(("control_install_click", f"/{REPO}/releases/download/v0.8.11/{bundle}", 200), paths)
        self.assertIn(("control_tauri_check", f"/{REPO}/releases/download/v0.8.11/latest.json", 200), paths)

    def test_ui_text_has_the_exact_settings_text_and_the_install_error(self):
        text = (self.evidence / "ui-text.txt").read_text(encoding="utf-8")
        self.assertEqual(text, self.result["ui_text"])
        self.assertIn("Version 0.7.12 is available. Click below to download, install, and relaunch.", text)
        self.assertIn("Update failed: Could not fetch a valid release JSON from the remote", text)
        self.assertIn("Install v0.7.12 & relaunch", text)

    def test_home_snapshots(self):
        before = json.loads((self.evidence / "home-before.json").read_text(encoding="utf-8"))
        after_check = json.loads((self.evidence / "home-after-check.json").read_text(encoding="utf-8"))
        after = json.loads((self.evidence / "home-after.json").read_text(encoding="utf-8"))
        self.assertEqual(before, after_check)
        launcher = ".arc/bin/arc-node.exe" if os.name == "nt" else ".arc/bin/arc-node"
        self.assertEqual(sorted(after), [".arc", ".arc/bin", launcher])
        self.assertEqual(after[launcher]["sha256"], hashlib.sha256(duc.fake_bytes("v0.7.12", sidecar_asset())).hexdigest())

    def test_a_result_json_of_another_schema_is_never_overwritten(self):
        evidence = self.tmpdir() / "shared"
        evidence.mkdir()
        theirs = json.dumps({"schema": "arc.legacy-bridge.wave0-lab.desktop-os-result.v1", "os": "linux", "verdict": "PASS"}).encode("utf-8")
        (evidence / "result.json").write_bytes(theirs)
        result = duc.execute(None, evidence, "linux", source=fake_source(), home_parent=self.tmpdir())
        self.assertEqual((evidence / "result.json").read_bytes(), theirs)
        self.assertEqual(json.loads((evidence / "result-linux.json").read_text(encoding="utf-8")), result)
        self.assertTrue(any("left untouched" in note for note in result["notes"]))
        # our own previous result.json (or an unreadable one) is replaced as before
        (evidence / "result.json").write_bytes(b"not json")
        duc.execute(None, evidence, "linux", source=fake_source(), home_parent=self.tmpdir())
        self.assertEqual(json.loads((evidence / "result.json").read_text(encoding="utf-8"))["schema"], duc.SCHEMA_RESULT)
        self.assertIsNone(duc.foreign_schema(evidence / "result.json"))
        self.assertIsNone(duc.foreign_schema(evidence / "missing.json"))

    def test_temporary_trees_are_removed(self):
        parent = self.tmpdir()
        duc.execute(None, self.tmpdir() / "e", "linux", source=fake_source(), home_parent=parent)
        self.assertEqual(list(parent.iterdir()), [])

    def test_two_runs_give_identical_results_and_no_random_port_leaks_in(self):
        again = duc.execute(None, self.tmpdir() / "e2", "linux", source=fake_source(), home_parent=self.tmpdir())
        self.assertEqual(json.dumps(again, sort_keys=True), json.dumps(self.result, sort_keys=True))
        self.assertNotIn("127.0.0.1:", json.dumps(self.result))
        self.assertIn("<fake-github>", json.dumps(self.result["replica_results"]))
        self.assertEqual((self.evidence / "requests.jsonl").read_bytes(), (self.evidence / "requests.jsonl").read_bytes())


class RunOfflineTests(StubCase):
    def test_only_loopback_names_are_ever_resolved_or_connected(self):
        seen: List[str] = []
        real_getaddrinfo = socket.getaddrinfo

        def guarded(host, *args, **kwargs):
            seen.append(str(host))
            if str(host) not in ("127.0.0.1", "localhost"):
                raise OSError("test guard: refusing to resolve %r" % (host,))
            return real_getaddrinfo(host, *args, **kwargs)

        with mock.patch("socket.getaddrinfo", guarded):
            result, _ = self.run_with(native=True)
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_ISOLATED")
        self.assertTrue(seen)
        self.assertEqual(set(seen), {"127.0.0.1"})

    def test_environment_proxies_are_not_used(self):
        with mock.patch.dict(os.environ, {"HTTP_PROXY": "http://127.0.0.1:9", "http_proxy": "http://127.0.0.1:9", "ALL_PROXY": "http://127.0.0.1:9"}):
            result, _ = self.run_with(native=True)
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_ISOLATED")


class RunMutationTests(StubCase):
    def test_latest_carrying_a_v0_8_11_latest_json_flips_the_verdict(self):
        result, evidence = self.run_with(mutate={"latest_has_latest_json": True})
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_NOT_ISOLATED")
        got = states(result)
        self.assertEqual((got["I1"], got["I2"], got["I3"], got["I4"]), ("FAIL", "FAIL", "FAIL", "FAIL"))
        self.assertEqual(got["I5"], "PASS")
        self.assertEqual(got["I6"], "PASS")
        rows = [json.loads(line) for line in (evidence / "requests.jsonl").read_text(encoding="utf-8").splitlines()]
        self.assertTrue(any(r["path"].startswith(f"/{REPO}/releases/download/v0.8.11/") and r["status"] == 200 for r in rows))

    def test_ui_text_does_not_claim_a_clean_outcome_when_the_click_downloaded(self):
        result, evidence = self.run_with(mutate={"latest_has_latest_json": True})
        text = (evidence / "ui-text.txt").read_text(encoding="utf-8")
        self.assertIn("FETCHED a bundle", text)
        self.assertNotIn("Nothing is downloaded", text)
        self.assertNotIn("Update failed", text)

    def test_a_second_fallback_endpoint_flips_the_verdict(self):
        bait = f"https://github.com/{REPO}/releases/download/v0.8.11/latest.json"
        result, _ = self.run_with(source=fake_source(endpoints=[duc.TAURI_ENDPOINT, bait]))
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_NOT_ISOLATED")
        got = states(result)
        self.assertEqual((got["I1"], got["I2"], got["I3"], got["I4"]), ("FAIL", "FAIL", "FAIL", "FAIL"))

    def test_a_foreign_fallback_endpoint_is_blocked_and_fails_the_allowlist(self):
        result, evidence = self.run_with(source=fake_source(endpoints=[duc.TAURI_ENDPOINT, "https://example.invalid/latest.json"]))
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_NOT_ISOLATED")
        self.assertTrue(result["blocked_hosts"])
        self.assertEqual(states(result)["I4"], "FAIL")
        text = (evidence / "requests.jsonl").read_text(encoding="utf-8")
        self.assertNotIn("example.invalid", text)

    def test_an_update_cache_file_flips_i5_only(self):
        result, _ = self.run_with(mutate={"cache_file": ".arc/updater/latest.json"})
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_NOT_ISOLATED")
        got = states(result)
        self.assertEqual(got["I5"], "FAIL")
        self.assertTrue(all(got[name] == "PASS" for name in ("I1", "I2", "I3", "I4", "I6", "I7")))
        self.assertIn("latest.json", by_id(result)["I5"]["detail"] + json.dumps(result["assertions"]))

    def test_a_request_outside_the_allowlist_flips_i4(self):
        result, _ = self.run_with(mutate={"extra_requests": [f"/{REPO}/releases/download/v0.7.12/SHA256SUMS"]})
        got = states(result)
        self.assertEqual((got["I3"], got["I4"]), ("PASS", "FAIL"))
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_NOT_ISOLATED")
        self.assertIn("SHA256SUMS", by_id(result)["I4"]["detail"])

    def test_a_forbidden_request_flips_i3_and_i4(self):
        result, _ = self.run_with(mutate={"extra_requests": [f"/{REPO}/releases/download/v0.8.11/ARC.Node_aarch64.app.tar.gz"]})
        got = states(result)
        self.assertEqual((got["I3"], got["I4"]), ("FAIL", "FAIL"))
        self.assertIn("v0.8", by_id(result)["I3"]["detail"])

    def test_a_control_that_does_not_behave_flips_i6(self):
        result, _ = self.run_with(mutate={"control_carries_latest_json": False})
        got = states(result)
        self.assertEqual(got["I6"], "FAIL")
        self.assertTrue(all(got[name] == "PASS" for name in ("I1", "I2", "I3", "I4", "I5", "I7")))
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_NOT_ISOLATED")

    def test_a_failed_source_item_flips_i7(self):
        result, _ = self.run_with(source=fake_source(items_ok=False))
        self.assertEqual(states(result)["I7"], "FAIL")
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_NOT_ISOLATED")

    def test_released_binary_problems_flip_i7(self):
        source = fake_source()
        source["released_binary"]["problems"] = ["binary digest or size differs from the pin"]
        result, _ = self.run_with(source=source)
        self.assertEqual(states(result)["I7"], "FAIL")

    def test_plugin_semantics_statuses_in_i7(self):
        confirmed, _ = self.run_with(source=fake_source(plugin_status="confirmed", plugin_reason=None))
        self.assertEqual(states(confirmed)["I7"], "PASS")
        offline, _ = self.run_with(source=fake_source(plugin_status="unverified", plugin_reason="offline-requested"))
        self.assertEqual(states(offline)["I7"], "PASS")
        self.assertIn("NOTE", by_id(offline)["I7"]["detail"])
        outage, _ = self.run_with(source=fake_source(plugin_status="unverified", plugin_reason="fetch-failed"))
        self.assertEqual(states(outage)["I7"], "FAIL")
        self.assertIn("FAILS I7", by_id(outage)["I7"]["detail"])
        contradicted, _ = self.run_with(source=fake_source(plugin_status="contradicted", plugin_reason=None))
        self.assertEqual(states(contradicted)["I7"], "FAIL")
        self.assertEqual(contradicted["verdict"], "DESKTOP_UPDATER_NOT_ISOLATED")

    def test_live_observation_is_embedded_verbatim(self):
        path = self.tmpdir() / "live.json"
        document = {"schema": "x", "observations": [{"status": 404, "note": "ü"}]}
        path.write_bytes(json.dumps(document).encode("utf-8"))
        result, _ = self.run_with(live_observation=path)
        self.assertEqual(result["live_observation"]["supplied"], True)
        self.assertEqual(result["live_observation"]["content"], document)
        self.assertEqual(result["live_observation"]["sha256"], hashlib.sha256(path.read_bytes()).hexdigest())
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_ISOLATED")

    def test_unparseable_or_missing_live_observation_is_said_not_hidden(self):
        bad = self.tmpdir() / "bad.json"
        bad.write_bytes(b"{not json")
        result, _ = self.run_with(live_observation=bad)
        self.assertTrue(result["live_observation"]["supplied"])
        self.assertIn("unparseable", result["live_observation"]["note"])
        missing, _ = self.run_with(live_observation=self.tmpdir() / "nope.json")
        self.assertFalse(missing["live_observation"]["supplied"])
        self.assertIn("NOT EMBEDDED", missing["live_observation"]["note"])


# --------------------------------------------------------------------------------------------------------------
# the native checker contract (stub executable)
# --------------------------------------------------------------------------------------------------------------

class NativeCheckTests(StubCase):
    def run_native_with(self, mode="normal", **kwargs):
        return self.run_with(mode, native=True, **kwargs)

    def test_a_well_behaved_native_checker_passes_i8_and_i9(self):
        result, evidence = self.run_native_with("normal")
        got = states(result)
        self.assertEqual((got["I8"], got["I9"], result["verdict"]), ("PASS", "PASS", "DESKTOP_UPDATER_ISOLATED"), by_id(result)["I8"]["detail"] + by_id(result)["I9"]["detail"])
        native = result["native_check"]
        self.assertTrue(native["supplied"])
        self.assertEqual(native["main"]["result"]["outcome"], "error")
        self.assertEqual(native["main"]["result"]["error_kind"], "ReleaseNotFound")
        self.assertEqual((native["main"]["sandbox_delta"], native["main"]["sandbox_delta_total"], native["main"]["sandbox_suspicious"]), ([], 0, []))
        self.assertEqual(native["control"]["result"]["outcome"], "update_available")
        self.assertFalse(native["control"]["result"]["download_attempted"])
        self.assertTrue(native["control_download"]["result"]["download_attempted"])
        self.assertEqual(native["control_download"]["result"]["download_result"], "error: signature")
        rows = [json.loads(line) for line in (evidence / "requests.jsonl").read_text(encoding="utf-8").splitlines()]
        native_rows = [r for r in rows if r["step"] == "native_check"]
        self.assertEqual([(r["path"], r["status"]) for r in native_rows], [(ENDPOINT_PATH, 302), (f"/{REPO}/releases/download/v0.7.12/latest.json", 404)])
        self.assertEqual({r["host"] for r in native_rows}, {"<fake-github>"})
        self.assertEqual({r["user_agent"] for r in native_rows}, {"tauri-plugin-updater/2.10.1"})
        self.assertEqual(result["request_count"], 9)
        control_rows = [json.loads(line) for line in (evidence / "control-requests.jsonl").read_text(encoding="utf-8").splitlines()]
        steps = [r["step"] for r in control_rows]
        self.assertEqual(steps.count("native_control"), 2)  # endpoint 302 + latest.json 200, no bundle without the flag
        self.assertEqual(steps.count("native_control_download"), 3)  # endpoint 302 + latest.json 200 + the bundle
        bundles = [r for r in control_rows if r["step"] == "native_control_download" and "ARC.Node_aarch64.app.tar.gz" in r["path"]]
        self.assertEqual([r["status"] for r in bundles], [200])

    def test_native_command_line_follows_the_contract(self):
        recorder = self.tmpdir() / "argv.json"
        script = self.tmpdir() / "record_argv.py"
        script.write_text("import json, os, sys\nopen(os.environ['ARGV_OUT'], 'a').write(json.dumps(sys.argv[1:]) + '\\n')\nprint(json.dumps({'schema': 'arc.legacy-bridge.wave0-lab.native-updater-check.v1', 'plugin': 'p', 'tauri': 't', 'current_version': '0.7.11', 'endpoints': [], 'outcome': 'error', 'error_kind': 'ReleaseNotFound', 'error': 'e', 'update': None, 'download_attempted': False}))\n", encoding="utf-8")
        source = fake_source()
        with mock.patch.dict(os.environ, {"ARGV_OUT": str(recorder)}):
            duc.execute(None, self.tmpdir() / "e", "linux", native_check=[sys.executable, str(script)], source=source, home_parent=self.tmpdir())
        calls = [json.loads(line) for line in recorder.read_text(encoding="utf-8").splitlines()]
        self.assertEqual(len(calls), 3)
        for call in calls:
            self.assertEqual(call[call.index("--current-version") + 1], "0.7.11")
            self.assertEqual(call[call.index("--pubkey") + 1], source["extracted"]["pubkey"])
            self.assertIn("--insecure-transport", call)
            endpoint = call[call.index("--endpoint") + 1]
            self.assertTrue(endpoint.startswith("http://127.0.0.1:"))
            self.assertTrue(endpoint.endswith(ENDPOINT_PATH))
        self.assertNotIn("--control-download", calls[0])
        self.assertNotIn("--control-download", calls[1])
        self.assertIn("--control-download", calls[2])

    def test_the_native_process_runs_in_a_sandboxed_home_with_proxies_off(self):
        probe = self.tmpdir() / "probe_env.py"
        out = self.tmpdir() / "env.json"
        probe.write_text(
            "import json, os\n"
            "keys = ['HOME', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA', 'XDG_CACHE_HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'TMPDIR', 'TEMP', 'TMP', 'NO_PROXY', 'HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY']\n"
            "json.dump({'cwd': os.getcwd(), **{k: os.environ.get(k) for k in keys}}, open(os.environ['ENV_OUT'], 'w'))\n"
            "print(json.dumps({'schema': 'arc.legacy-bridge.wave0-lab.native-updater-check.v1', 'plugin': 'p', 'tauri': 't', 'current_version': '0.7.11', 'endpoints': [], 'outcome': 'error', 'error_kind': 'ReleaseNotFound', 'error': 'e', 'update': None, 'download_attempted': False}))\n",
            encoding="utf-8",
        )
        sandbox = self.tmpdir() / "sandbox"
        with mock.patch.dict(os.environ, {"ENV_OUT": str(out), "HTTPS_PROXY": "http://proxy.invalid:3128", "PYTHONDONTWRITEBYTECODE": "1"}):
            outcome = duc.run_native([sys.executable, str(probe)], "http://127.0.0.1:1/x", "KEY", "0.7.11", sandbox=sandbox)
        self.assertTrue(outcome["ok"], outcome)
        seen = json.loads(out.read_text(encoding="utf-8"))
        for key in ("HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "XDG_CACHE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "TMPDIR", "TEMP", "TMP"):
            self.assertTrue(Path(seen[key]).resolve().is_relative_to(sandbox.resolve()) if hasattr(Path, "is_relative_to") else str(sandbox.resolve()) in str(Path(seen[key]).resolve()), key)
        self.assertEqual(Path(seen["cwd"]).resolve(), (sandbox / "cwd").resolve())
        self.assertEqual(seen["NO_PROXY"], "127.0.0.1,localhost")
        self.assertIsNone(seen["HTTP_PROXY"])
        self.assertIsNone(seen["HTTPS_PROXY"])
        self.assertIsNone(seen["ALL_PROXY"])
        self.assertEqual((outcome["sandbox_delta"], outcome["sandbox_suspicious"]), ([], []))

    @needs_posix
    def test_a_relative_executable_path_still_resolves_after_the_cwd_change(self):
        script = self.tmpdir() / "relative-stub"
        script.write_text("#!%s\n%s" % (sys.executable, STUB), encoding="utf-8")
        os.chmod(str(script), 0o755)
        server = duc.FakeGitHub(duc.main_model()).start()
        self.addCleanup(server.stop)
        relative = os.path.relpath(str(script), os.getcwd())
        self.assertFalse(os.path.isabs(relative))
        outcome = duc.run_native([relative], server.base + ENDPOINT_PATH, "KEY", "0.7.11", sandbox=self.tmpdir() / "sb")
        self.assertTrue(outcome["ok"], outcome)
        self.assertEqual(outcome["result"]["error_kind"], "ReleaseNotFound")

    def test_extra_native_request_fails_i8_and_i3(self):
        result, _ = self.run_native_with("extra_request")
        got = states(result)
        self.assertEqual((got["I8"], got["I3"], got["I4"]), ("FAIL", "FAIL", "FAIL"))
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_NOT_ISOLATED")
        self.assertIn("the fake server saw", by_id(result)["I8"]["detail"])

    def test_native_update_found_for_the_main_scenario_fails_i8(self):
        result, _ = self.run_native_with("normal", mutate={"latest_has_latest_json": True})
        got = states(result)
        self.assertEqual((got["I8"], got["I1"]), ("FAIL", "FAIL"))
        self.assertIn("update_available", by_id(result)["I8"]["detail"])

    def test_no_update_instead_of_release_not_found_is_accepted(self):
        result, _ = self.run_native_with("no_update_on_404")
        self.assertEqual(states(result)["I8"], "PASS", by_id(result)["I8"]["detail"])

    def test_wrong_error_kind_or_text_fails_i8(self):
        for mode in ("wrong_kind", "lying_error_text"):
            result, _ = self.run_native_with(mode)
            self.assertEqual(states(result)["I8"], "FAIL", mode)

    def test_wrong_pinned_plugin_fails_i8(self):
        result, _ = self.run_native_with("wrong_plugin")
        self.assertEqual(states(result)["I8"], "FAIL")
        self.assertIn("2.9.0", by_id(result)["I8"]["detail"])

    def test_wrong_user_agent_fails_i8(self):
        result, _ = self.run_native_with("wrong_ua")
        self.assertEqual(states(result)["I8"], "FAIL")
        self.assertIn("user agents", by_id(result)["I8"]["detail"])

    def test_native_process_leaving_a_cache_fails_i8(self):
        for mode in ("writes_cache", "writes_cwd", "writes_hidden_cache"):
            result, _ = self.run_native_with(mode)
            self.assertEqual(states(result)["I8"], "FAIL", mode)
            self.assertTrue(result["native_check"]["main"]["sandbox_delta"], mode)
            self.assertTrue(result["native_check"]["main"]["sandbox_suspicious"], mode)
            self.assertIn("an update cache?", by_id(result)["I8"]["detail"])
            self.assertEqual(result["verdict"], "DESKTOP_UPDATER_NOT_ISOLATED")
        self.assertEqual(result["native_check"]["main"]["sandbox_suspicious"], ["home/.state"])  # innocuous name, caught by its content

    def test_unrelated_files_in_the_sandbox_are_reported_but_do_not_fail_i8(self):
        result, _ = self.run_native_with("writes_noise")
        main = result["native_check"]["main"]
        self.assertEqual(states(result)["I8"], "PASS", by_id(result)["I8"]["detail"])
        self.assertEqual((main["sandbox_delta"], main["sandbox_delta_total"], main["sandbox_suspicious"]), (["home/.noise"], 1, []))
        self.assertIn("1 unrelated file/dir entries", by_id(result)["I8"]["detail"])

    def test_suspicious_files_matches_by_path_and_by_content_only_for_files(self):
        sandbox = self.tmpdir()
        (sandbox / "a").mkdir()
        (sandbox / "a" / "Latest.JSON").write_bytes(b"x")
        (sandbox / "a" / "plain.bin").write_bytes(b"... FerrumVir ...")
        (sandbox / "a" / "other.bin").write_bytes(b"nothing to see")
        (sandbox / "ferrumvir-dir").mkdir()
        tokens = duc.watch_tokens("http://127.0.0.1:4242/FerrumVir/arc-chain/releases/latest/download/latest.json")
        delta = ["a", "a/Latest.JSON", "a/plain.bin", "a/other.bin", "ferrumvir-dir", "(removed) a/gone.json"]
        self.assertEqual(duc.suspicious_files(sandbox, delta, tokens), ["a/Latest.JSON", "a/plain.bin"])
        self.assertIn("127.0.0.1:4242", tokens)

    def test_malformed_native_output_fails_i8_and_i9(self):
        for mode in ("bad_json", "bad_outcome"):
            result, _ = self.run_native_with(mode)
            got = states(result)
            self.assertEqual((got["I8"], got["I9"]), ("FAIL", "FAIL"), mode)
            self.assertIn("native checker problem", by_id(result)["I8"]["detail"])

    def test_a_native_checker_that_cannot_be_run_fails_i8_and_i9(self):
        result = duc.execute(None, self.tmpdir() / "e", "linux", native_check=["/nonexistent/native-updater-check"], source=fake_source(), home_parent=self.tmpdir())
        got = states(result)
        self.assertEqual((got["I8"], got["I9"], result["verdict"]), ("FAIL", "FAIL", "DESKTOP_UPDATER_NOT_ISOLATED"))
        self.assertIn("could not run the native checker", by_id(result)["I8"]["detail"])

    def test_control_must_download_only_with_the_flag(self):
        down, _ = self.run_native_with("download_without_flag")
        self.assertEqual(states(down)["I9"], "FAIL")
        self.assertIn("without --control-download", by_id(down)["I9"]["detail"])
        none, _ = self.run_native_with("no_download_with_flag")
        self.assertEqual(states(none)["I9"], "FAIL")

    def test_exit_code_of_the_native_process_is_recorded_not_judged(self):
        result, _ = self.run_native_with("exit_code_three")
        self.assertEqual(result["native_check"]["main"]["exit_code"], 3)
        self.assertEqual(states(result)["I8"], "PASS")

    def test_i9_without_the_bundle_request_would_fail(self):
        # the control server saw no bundle GET: simulate by feeding the evaluator directly
        plain = {"ok": True, "result": {"schema": duc.SCHEMA_NATIVE, "plugin": "tauri-plugin-updater 2.10.1", "tauri": "2.11.2", "current_version": "0.7.11", "endpoints": [], "outcome": "update_available", "error_kind": None, "error": None, "update": {"version": "0.8.11", "download_url": "x"}, "download_attempted": False}}
        with_download = copy.deepcopy(plain)
        with_download["result"]["download_attempted"] = True
        entry = duc._native_control_assertion(plain, with_download, [])
        self.assertEqual(entry["result"], "FAIL")
        self.assertIn("never saw the native bundle GET", entry["detail"])


# --------------------------------------------------------------------------------------------------------------
# released binary
# --------------------------------------------------------------------------------------------------------------

class ReleasedBinaryTests(TempCase):
    def test_a_foreign_file_reports_every_mismatch(self):
        path = self.tmpdir() / "arc-desktop"
        path.write_bytes(b"tauri-plugin-updater-2.0.0 and latest.json twice latest.json")
        outcome = duc.verify_released_binary(path)
        self.assertFalse(outcome["verified_locally"])
        text = " | ".join(outcome["problems"])
        self.assertIn("binary digest or size differs from the pin", text)
        self.assertIn("'latest.json': 2 occurrences, pinned 1", text)
        self.assertIn("tauri-plugin-updater: strings say ['tauri-plugin-updater-2.0.0'], pinned ['tauri-plugin-updater-2.10.1']", text)

    @needs_binary
    def test_the_local_copy_of_the_released_binary_matches_every_embedded_fact(self):
        outcome = duc.verify_released_binary(RELEASED_BINARY)
        self.assertEqual(outcome["problems"], [])
        self.assertTrue(outcome["verified_locally"])

    @unittest.skipUnless(RELEASED_DEB is not None, "released .deb not available locally")
    def test_the_local_deb_matches_the_embedded_package_digest(self):
        data = Path(RELEASED_DEB).read_bytes()
        self.assertEqual(hashlib.sha256(data).hexdigest(), duc.PINNED["release"]["package_sha256"])
        self.assertEqual(len(data), duc.PINNED["release"]["package_size"])


# --------------------------------------------------------------------------------------------------------------
# real v0.7.11 sources
# --------------------------------------------------------------------------------------------------------------

@needs_repo
class RealTagCheckSourceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.source = duc.check_source(REAL_REPO, plugin_crate=PLUGIN_CRATE if CRATE_OK else None, plugin_offline=not CRATE_OK, released_binary=RELEASED_BINARY)

    def test_every_item_is_verified(self):
        self.assertEqual([(item["id"], item["ok"]) for item in self.source["items"]], [
            ("a-tauri-endpoint", True), ("b-plugin-init", True), ("c-check-for-update", True), ("d-ensure-binary", True),
            ("e-settings-flow", True), ("f-inventories", True), ("g-no-update-disk-writes", True),
        ], [item["detail"] for item in self.source["items"] if not item["ok"]])
        self.assertEqual(self.source["verdict"], "SOURCE_VERIFIED")
        self.assertEqual(self.source["schema"], "arc.legacy-bridge.wave0-lab.stage-c-check-source.v1")

    def test_sources_are_pinned_by_git_blob_and_sha256(self):
        self.assertEqual({path: entry["git_blob"] for path, entry in self.source["sources"].items()}, KNOWN_BLOBS)
        for entry in self.source["sources"].values():
            self.assertEqual(len(entry["sha256"]), 64)

    def test_extracted_facts(self):
        extracted = self.source["extracted"]
        self.assertEqual(extracted["endpoints"], [duc.TAURI_ENDPOINT])
        self.assertEqual(extracted["api_url"], duc.API_LATEST)
        self.assertEqual(extracted["sidecar_template"], duc.SIDECAR_TEMPLATE)
        self.assertEqual(extracted["platform_assets"], {"linux-x86_64": "arc-node-linux-x86_64", "macos-aarch64": "arc-node-macos-arm64", "macos-x86_64": "arc-node-macos-x86_64", "windows-x86_64": "arc-node-windows-x86_64.exe"})
        self.assertEqual(extracted["pubkey_sha256"], hashlib.sha256(extracted["pubkey"].encode()).hexdigest())
        self.assertEqual(extracted["settings_templates"]["available"], "Version ${update.version} is available. Click below to download, install, and relaunch.")
        self.assertEqual(extracted["settings_templates"]["install_button"], "Install v${update.version} & relaunch")

    def test_inventories_equal_the_known_sets(self):
        inventories = self.source["inventories"]
        for name in ("latest.json", "githubusercontent", "check( calls"):
            self.assertEqual(inventories[name]["observed"], [], name)
        self.assertEqual([list(x) for x in inventories["downloadAndInstall"]["observed"]], [["desktop/src-tauri/src/lib.rs", 62], ["desktop/src/screens/Settings.tsx", 45]])
        self.assertEqual([list(x) for x in inventories["api.github.com"]["observed"]], [["desktop/src-tauri/src/commands.rs", 942]])
        self.assertEqual(len(inventories["autoUpdate/auto_update"]["observed"]), 8)
        self.assertEqual(inventories["latest.json in tauri.conf.json"]["observed"], [63])
        self.assertTrue(all(entry["ok"] for entry in inventories.values()))

    def test_citations_point_at_real_lines(self):
        for item in self.source["items"]:
            for citation in item["citations"]:
                self.assertTrue(citation["file"].startswith("desktop/"))
                self.assertGreater(citation["line"], 0, citation)
                self.assertTrue(citation["text"])
        c_item = next(item for item in self.source["items"] if item["id"] == "c-check-for-update")
        self.assertIn("lines 934-957", c_item["detail"])
        text = duc.git_text(REAL_REPO, "v0.7.11", "desktop/src-tauri/src/commands.rs").split("\n")
        for citation in c_item["citations"]:
            self.assertIn(citation["text"][:20], text[citation["line"] - 1])

    def test_released_binary_section(self):
        released = self.source["released_binary"]
        self.assertEqual(released["package_sha256"], duc.PINNED["release"]["package_sha256"])
        self.assertEqual(released["binary_sha256"], duc.PINNED["release"]["binary_sha256"])
        self.assertEqual(released["crates_from_binary_strings"]["tauri-plugin-updater"], ["2.10.1"])
        if RELEASED_BINARY is not None:
            self.assertTrue(released["verified_locally"])
            self.assertEqual(released["problems"], [])

    @needs_crate
    def test_plugin_semantics_are_confirmed_at_the_pinned_version(self):
        semantics = self.source["plugin_semantics"]
        self.assertEqual((semantics["status"], semantics["crate"]), ("confirmed", "tauri-plugin-updater 2.10.1"))
        self.assertIn("RELEASED v0.7.11 package", semantics["pinned_by"])
        self.assertIn("strings of the RELEASED binary", semantics["shipped_app_version_note"])

    def test_git_helpers_on_the_real_tag(self):
        self.assertEqual(duc.git_blob_id(REAL_REPO, "v0.7.11", "desktop/src-tauri/tauri.conf.json"), KNOWN_BLOBS["desktop/src-tauri/tauri.conf.json"])
        rows = duc.git_grep(REAL_REPO, "v0.7.11", r"tauri_plugin_updater", ["desktop/src-tauri/src"])
        self.assertEqual([(path, number) for path, number, _ in rows], [("desktop/src-tauri/src/lib.rs", 60)])
        with self.assertRaises(duc.GitError):
            duc.git_text(REAL_REPO, "v0.7.11", "no/such/file")
        self.assertEqual(duc.git_grep(REAL_REPO, "v0.7.11", r"zzz-no-such-token-zzz", ["desktop/src"]), [])

    def test_full_run_against_the_real_tag_is_isolated(self):
        with tempfile.TemporaryDirectory() as raw:
            result = duc.execute(REAL_REPO, Path(raw) / "evidence", "macos", plugin_crate=PLUGIN_CRATE if CRATE_OK else None, plugin_offline=not CRATE_OK, source=self.source, home_parent=Path(raw))
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_ISOLATED", [a for a in result["assertions"] if a["result"] == "FAIL"])
        self.assertEqual(result["request_count"], 7)
        self.assertEqual(result["sources"], self.source["sources"])


# --------------------------------------------------------------------------------------------------------------
# synthetic repositories mutated from the real tag
# --------------------------------------------------------------------------------------------------------------

SYNTHETIC_PATHS = ["desktop/src", "desktop/src-tauri/src", "desktop/src-tauri/tauri.conf.json", "desktop/src-tauri/Cargo.toml"]
_ARCHIVE: Dict[str, bytes] = {}


def _git(root: Path, *args: str) -> None:
    subprocess.run(["git", "-C", str(root), *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)


def build_synthetic_repo(parent: Path, mutations: Optional[Dict[str, Callable[[str], str]]] = None, extra_files: Optional[Dict[str, str]] = None) -> Path:
    if "tar" not in _ARCHIVE:
        done = subprocess.run(["git", "-C", str(REAL_REPO), "archive", "--format=tar", "v0.7.11", *SYNTHETIC_PATHS], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
        _ARCHIVE["tar"] = done.stdout
    root = parent / "synthetic"
    root.mkdir()
    with tarfile.open(fileobj=io.BytesIO(_ARCHIVE["tar"]), mode="r:") as archive:
        try:
            archive.extractall(str(root), filter="data")  # type: ignore[call-arg]
        except TypeError:  # Python before the extraction filters
            archive.extractall(str(root))
    for relative, transform in (mutations or {}).items():
        target = root / relative
        before = target.read_bytes().decode("utf-8")
        after = transform(before)
        assert after != before, f"mutation of {relative} changed nothing"
        target.write_bytes(after.encode("utf-8"))
    for relative, text in (extra_files or {}).items():
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(text.encode("utf-8"))
    _git(root, "init", "-q")
    _git(root, "-c", "core.autocrlf=false", "add", "-A")
    _git(root, "-c", "user.name=lab", "-c", "user.email=lab@example.invalid", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "synthetic v0.7.11 copy")
    _git(root, "tag", "v0.7.11")
    return root


def replace_once(old: str, new: str) -> Callable[[str], str]:
    def transform(text: str) -> str:
        assert text.count(old) == 1, (text.count(old), old)
        return text.replace(old, new)

    return transform


def edit_conf(change: Callable[[dict], None]) -> Callable[[str], str]:
    def transform(text: str) -> str:
        document = json.loads(text)
        change(document)
        return json.dumps(document, indent=2) + "\n"

    return transform


BAIT_ENDPOINT = f"https://github.com/{REPO}/releases/download/v0.8.11/latest.json"
CONF = "desktop/src-tauri/tauri.conf.json"
COMMANDS = "desktop/src-tauri/src/commands.rs"
LIB = "desktop/src-tauri/src/lib.rs"
SETTINGS = "desktop/src/screens/Settings.tsx"


@needs_repo
class SyntheticRepoMutationTests(TempCase):
    def source_of(self, mutations=None, extra_files=None) -> dict:
        repo = build_synthetic_repo(self.tmpdir(), mutations, extra_files)
        return duc.check_source(repo, plugin_offline=True)

    def failing(self, source: dict) -> List[str]:
        return sorted(item["id"] for item in source["items"] if not item["ok"])

    def test_an_unmutated_copy_is_verified_with_identical_blob_ids(self):
        source = self.source_of()
        self.assertEqual(source["verdict"], "SOURCE_VERIFIED", [i["detail"] for i in source["items"] if not i["ok"]])
        self.assertEqual({path: entry["git_blob"] for path, entry in source["sources"].items()}, KNOWN_BLOBS)

    def test_a_second_endpoint_in_tauri_conf_is_caught(self):
        source = self.source_of({CONF: edit_conf(lambda d: d["plugins"]["updater"]["endpoints"].append(BAIT_ENDPOINT))})
        self.assertEqual(source["verdict"], "SOURCE_NOT_VERIFIED")
        self.assertIn("a-tauri-endpoint", self.failing(source))
        self.assertFalse(source["inventories"]["latest.json in tauri.conf.json"]["ok"])
        self.assertEqual(source["extracted"]["endpoints"], [duc.TAURI_ENDPOINT, BAIT_ENDPOINT])

    def test_a_dangerous_transport_flag_is_caught(self):
        source = self.source_of({CONF: edit_conf(lambda d: d["plugins"]["updater"].__setitem__("dangerousInsecureTransportProtocol", True))})
        self.assertIn("a-tauri-endpoint", self.failing(source))

    def test_a_missing_or_inactive_updater_config_is_caught(self):
        source = self.source_of({CONF: edit_conf(lambda d: d["plugins"]["updater"].__setitem__("active", False))})
        self.assertIn("a-tauri-endpoint", self.failing(source))
        broken = self.source_of({CONF: lambda text: "{ not json"})
        self.assertIn("a-tauri-endpoint", self.failing(broken))
        self.assertEqual(broken["extracted"]["endpoints"], [])

    def test_an_extra_updater_call_in_settings_is_caught(self):
        source = self.source_of({SETTINGS: replace_once("const u = await tauriCheckUpdate();", "const u = await tauriCheckUpdate();\n      const spare = await tauriCheckUpdate();")})
        self.assertEqual(source["verdict"], "SOURCE_NOT_VERIFIED")
        self.assertIn("f-inventories", self.failing(source))
        self.assertFalse(source["inventories"]["tauriCheckUpdate"]["ok"])

    def test_a_new_check_call_in_another_file_is_caught(self):
        source = self.source_of(extra_files={"desktop/src/lib/extra_updater.ts": 'import { check } from "@tauri-apps/plugin-updater";\nexport const probe = () => check();\n'})
        self.assertIn("f-inventories", self.failing(source))
        self.assertFalse(source["inventories"]["check( calls"]["ok"])
        self.assertFalse(source["inventories"]["@tauri-apps/plugin-updater"]["ok"])

    def test_a_new_auto_update_consumer_is_caught(self):
        source = self.source_of(extra_files={"desktop/src/lib/autoupdate_probe.ts": "export const run = (config: { autoUpdate: boolean }) => config.autoUpdate;\n"})
        self.assertIn("f-inventories", self.failing(source))
        self.assertFalse(source["inventories"]["autoUpdate/auto_update"]["ok"])

    def test_a_file_write_inside_check_for_update_is_caught(self):
        source = self.source_of({COMMANDS: replace_once('    let current = env!("CARGO_PKG_VERSION");', '    let current = env!("CARGO_PKG_VERSION");\n    let _ = std::fs::write("latest.json", &version);')})
        failing = self.failing(source)
        self.assertIn("c-check-for-update", failing)
        self.assertIn("g-no-update-disk-writes", failing)
        self.assertIn("f-inventories", failing)

    def test_a_duplicated_plugin_init_is_caught(self):
        line = "        .plugin(tauri_plugin_updater::Builder::new().build())"
        source = self.source_of({LIB: replace_once(line, line + "\n" + line)})
        self.assertIn("b-plugin-init", self.failing(source))

    def test_a_plugin_init_with_overrides_is_caught(self):
        source = self.source_of({LIB: replace_once("tauri_plugin_updater::Builder::new().build()", 'tauri_plugin_updater::Builder::new().target("x").build()')})
        self.assertIn("b-plugin-init", self.failing(source))

    def test_a_changed_sidecar_url_is_caught(self):
        source = self.source_of({COMMANDS: replace_once("https://github.com/FerrumVir/arc-chain/releases/latest/download/{}", "https://github.com/FerrumVir/arc-chain/releases/download/v0.8.11/{}")})
        failing = self.failing(source)
        self.assertIn("d-ensure-binary", failing)
        self.assertIn("f-inventories", failing)

    def test_a_changed_api_url_is_caught(self):
        source = self.source_of({COMMANDS: replace_once("https://api.github.com/repos/FerrumVir/arc-chain/releases/latest", "https://api.github.com/repos/FerrumVir/arc-chain/releases")})
        self.assertIn("c-check-for-update", self.failing(source))

    def test_a_changed_has_update_rule_is_caught(self):
        source = self.source_of({COMMANDS: replace_once('has_update: version != current && version != "unknown",', "has_update: true,")})
        self.assertIn("c-check-for-update", self.failing(source))

    def test_a_reordered_settings_flow_is_caught(self):
        source = self.source_of({SETTINGS: replace_once("await u.downloadAndInstall();", "await tauriRelaunch();")})
        self.assertIn("e-settings-flow", self.failing(source))

    def test_automatic_checking_is_caught(self):
        source = self.source_of({SETTINGS: replace_once("enabled: false,", "enabled: true,")})
        self.assertIn("e-settings-flow", self.failing(source))

    def test_a_new_write_site_that_mentions_updates_is_caught(self):
        source = self.source_of(extra_files={"desktop/src-tauri/src/update_cache.rs": 'pub fn remember(v: &str) { let _ = std::fs::write("update-cache.json", v); }\n'})
        self.assertIn("g-no-update-disk-writes", self.failing(source))

    def test_browser_storage_for_updates_is_caught(self):
        source = self.source_of(extra_files={"desktop/src/lib/remember.ts": 'export const remember = (v: string) => localStorage.setItem("latest-update", v);\n'})
        self.assertIn("g-no-update-disk-writes", self.failing(source))

    def test_a_missing_tag_or_repo_is_a_clean_git_error(self):
        repo = build_synthetic_repo(self.tmpdir())
        with self.assertRaises(duc.GitError):
            duc.check_source(repo, tag="v9.9.9", plugin_offline=True)
        with self.assertRaises(duc.GitError):
            duc.check_source(self.tmpdir() / "not-a-repo", plugin_offline=True)

    def test_a_run_on_a_repo_with_a_second_endpoint_is_not_isolated(self):
        repo = build_synthetic_repo(self.tmpdir(), {CONF: edit_conf(lambda d: d["plugins"]["updater"]["endpoints"].append(BAIT_ENDPOINT))})
        result = duc.execute(repo, self.tmpdir() / "e", "linux", plugin_offline=True, home_parent=self.tmpdir())
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_NOT_ISOLATED")
        got = states(result)
        self.assertEqual((got["I1"], got["I2"], got["I3"], got["I7"]), ("FAIL", "FAIL", "FAIL", "FAIL"))

    def test_a_run_on_a_repo_with_an_extra_updater_call_is_not_isolated_by_i7(self):
        repo = build_synthetic_repo(self.tmpdir(), {SETTINGS: replace_once("const u = await tauriCheckUpdate();", "const u = await tauriCheckUpdate();\n      const spare = await tauriCheckUpdate();")})
        result = duc.execute(repo, self.tmpdir() / "e", "linux", plugin_offline=True, home_parent=self.tmpdir())
        got = states(result)
        self.assertEqual(got["I7"], "FAIL")
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_NOT_ISOLATED")


# --------------------------------------------------------------------------------------------------------------
# command line
# --------------------------------------------------------------------------------------------------------------

CHECKER = _paths.LAB / "desktop_updater_check.py"


def run_cli(*args: str, env: Optional[Dict[str, str]] = None, timeout: float = 180.0) -> "subprocess.CompletedProcess[str]":
    full_env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1", PYTHONIOENCODING="utf-8")
    full_env.update(env or {})
    return subprocess.run([sys.executable, "-B", str(CHECKER), *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True, encoding="utf-8", timeout=timeout, env=full_env)


class CliTests(TempCase):
    def test_a_missing_repository_is_exit_2_with_a_clean_message(self):
        out = self.tmpdir() / "out.json"
        outcome = run_cli("check-source", "--repo", str(self.tmpdir() / "nope"), "--out", str(out), "--plugin-offline")
        self.assertEqual(outcome.returncode, 2, outcome.stderr)
        self.assertIn("ERROR: cannot read the v0.7.11 sources", outcome.stderr)
        self.assertNotIn("Traceback", outcome.stderr)
        self.assertFalse(out.exists())

    def test_run_requires_a_known_label(self):
        outcome = run_cli("run", "--repo", ".", "--evidence", str(self.tmpdir()), "--label", "freebsd")
        self.assertEqual(outcome.returncode, 2)
        self.assertIn("invalid choice", outcome.stderr)

    def test_plugin_crate_and_plugin_offline_are_mutually_exclusive(self):
        outcome = run_cli("check-source", "--repo", ".", "--out", str(self.tmpdir() / "o.json"), "--plugin-crate", "x", "--plugin-offline")
        self.assertEqual(outcome.returncode, 2)
        self.assertIn("not allowed with argument", outcome.stderr)

    @needs_repo
    def test_check_source_cli_writes_the_report_and_exits_zero(self):
        out = self.tmpdir() / "check-source.json"
        args = ["check-source", "--repo", str(REAL_REPO), "--out", str(out)]
        args += ["--plugin-crate", str(PLUGIN_CRATE)] if CRATE_OK else ["--plugin-offline"]
        outcome = run_cli(*args)
        self.assertEqual(outcome.returncode, 0, outcome.stdout + outcome.stderr)
        self.assertIn("SOURCE_VERIFIED", outcome.stdout)
        self.assertEqual(outcome.stdout.count("OK   "), 7)
        document = json.loads(out.read_text(encoding="utf-8"))
        self.assertEqual(document["verdict"], "SOURCE_VERIFIED")

    @needs_repo
    def test_run_cli_exit_zero_when_isolated_and_writes_the_evidence(self):
        evidence = self.tmpdir() / "evidence"
        args = ["run", "--repo", str(REAL_REPO), "--evidence", str(evidence), "--label", "linux"]
        args += ["--plugin-crate", str(PLUGIN_CRATE)] if CRATE_OK else ["--plugin-offline"]
        outcome = run_cli(*args)
        self.assertEqual(outcome.returncode, 0, outcome.stdout + outcome.stderr)
        self.assertTrue(outcome.stdout.rstrip().endswith("DESKTOP_UPDATER_ISOLATED"))
        self.assertIn("SKIP I8", outcome.stdout)
        self.assertEqual(json.loads((evidence / "result-linux.json").read_text(encoding="utf-8"))["verdict"], "DESKTOP_UPDATER_ISOLATED")
        self.assertEqual(len((evidence / "requests.jsonl").read_text(encoding="utf-8").splitlines()), 7)

    @needs_repo
    @needs_posix
    def test_run_cli_with_a_native_stub_and_live_observation(self):
        stub = self.tmpdir() / "native-updater-check"
        stub.write_text("#!%s\n%s" % (sys.executable, STUB), encoding="utf-8")
        os.chmod(str(stub), 0o755)
        live = self.tmpdir() / "live.json"
        live.write_text(json.dumps({"observations": [{"status": 404}]}), encoding="utf-8")
        evidence = self.tmpdir() / "evidence"
        args = ["run", "--repo", str(REAL_REPO), "--evidence", str(evidence), "--label", "macos", "--native-check", str(stub), "--live-observation", str(live)]
        args += ["--plugin-crate", str(PLUGIN_CRATE)] if CRATE_OK else ["--plugin-offline"]
        outcome = run_cli(*args)
        self.assertEqual(outcome.returncode, 0, outcome.stdout + outcome.stderr)
        result = json.loads((evidence / "result.json").read_text(encoding="utf-8"))
        got = states(result)
        self.assertEqual((got["I8"], got["I9"]), ("PASS", "PASS"))
        self.assertEqual(result["live_observation"]["content"], {"observations": [{"status": 404}]})
        self.assertEqual(result["request_count"], 9)

    @needs_repo
    def test_run_cli_exit_one_when_not_isolated(self):
        repo = build_synthetic_repo(self.tmpdir(), {CONF: edit_conf(lambda d: d["plugins"]["updater"]["endpoints"].append(BAIT_ENDPOINT))})
        outcome = run_cli("run", "--repo", str(repo), "--evidence", str(self.tmpdir() / "e"), "--label", "windows", "--plugin-offline")
        self.assertEqual(outcome.returncode, 1, outcome.stdout + outcome.stderr)
        self.assertTrue(outcome.stdout.rstrip().endswith("DESKTOP_UPDATER_NOT_ISOLATED"))
        self.assertIn("FAIL I1", outcome.stdout)

    def test_main_returns_codes_in_process_too(self):
        out = io.StringIO()
        err = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = duc.main(["check-source", "--repo", str(self.tmpdir() / "missing"), "--out", str(self.tmpdir() / "o.json"), "--plugin-offline"])
        self.assertEqual(code, 2)
        self.assertIn("ERROR", err.getvalue())


class ModuleHygieneTests(unittest.TestCase):
    def test_module_is_stdlib_only(self):
        import ast

        tree = ast.parse(Path(duc.__file__).read_text(encoding="utf-8"))
        stdlib = set(sys.stdlib_module_names) if hasattr(sys, "stdlib_module_names") else None
        allowed = {"argparse", "hashlib", "http", "io", "json", "os", "platform", "re", "shutil", "socketserver", "subprocess", "sys", "tarfile", "tempfile", "threading", "urllib", "pathlib", "typing", "__future__"}
        imported = set()
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                imported.update(alias.name.split(".")[0] for alias in node.names)
            elif isinstance(node, ast.ImportFrom) and node.module:
                imported.add(node.module.split(".")[0])
        self.assertEqual(imported - allowed, set())
        if stdlib is not None:
            self.assertTrue(imported <= stdlib)

    def test_no_symlinks_no_fork_tricks_no_deletion_commands(self):
        text = Path(duc.__file__).read_text(encoding="utf-8")
        for forbidden in ("os.symlink", "os.fork", "os.exec", "os.system", "sudo", '"rm"', "'rm'", "rmdir", "subprocess.Popen"):
            self.assertNotIn(forbidden, text, forbidden)

    def test_the_only_urls_in_the_module_are_github_loopback_and_the_pinned_crate_host(self):
        import re

        text = Path(duc.__file__).read_text(encoding="utf-8").replace("\\.", ".")  # regex escapes such as api\.github\.com
        hosts = set(re.findall(r"https?://([A-Za-z0-9.\-]+)", text))
        self.assertEqual(hosts - {"github.com", "api.github.com", "static.crates.io", "127.0.0.1", "example.invalid"}, set())
        self.assertEqual(text.count("urlopen("), 0)
        self.assertEqual(duc.PINNED["plugin_crate"]["url"], "https://static.crates.io/crates/tauri-plugin-updater/tauri-plugin-updater-2.10.1.crate")


if __name__ == "__main__":
    unittest.main()
