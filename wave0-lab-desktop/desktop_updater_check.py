#!/usr/bin/env python3
"""Desktop updater isolation harness for the Wave 0 lab (THROWAWAY LAB FILE).

Question (ARC-83 reviewer, item 3): with the legacy-bridge launcher release v0.7.12 as GitHub "Latest"
(five launchers + SHA256SUMS, NO latest.json), does the RELEASED v0.7.11 desktop app's "Check for updates"
path find no update, fetch no v0.8 payload and leave no update cache?

The v0.7.11 desktop is a Tauri GUI. Its update-related behaviour is four small constructs, all read from the
tag and cited with file:line (git blob ids and SHA-256 of every file read are recorded):

  Settings.tsx:22-26    "Check for updates" runs the Rust command `check_for_update` (enabled:false => only on click)
  commands.rs:934-957   check_for_update: GET api.github.com/.../releases/latest, has_update = (tag != "0.7.11")
  Settings.tsx:35-52    "Install" runs the Tauri updater plugin `check()`, then downloadAndInstall for an Update object
  tauri.conf.json       ONE endpoint: https://github.com/FerrumVir/arc-chain/releases/latest/download/latest.json
  commands.rs:975-1075  ensure_binary: sidecar refresh from releases/latest/download/<platform asset> (the launcher)

The tauri-plugin-updater crate behind `check()` is pinned to the version found in the strings of the RELEASED
v0.7.11 binary (2.10.1, see PINNED) and its source is read statically (plugin_semantics). `run` replays the
constructs against an in-process fake GitHub (a local HTTP server playing api.github.com and github.com, the
same approach as tests/legacy-bridge/desktop_v07_harness.py), records EVERY request, snapshots the fake HOME
before and after, runs a POSITIVE CONTROL against a Latest that DOES carry a v0.8.11 latest.json, and optionally
drives a REAL tauri-plugin-updater binary (--native-check) against the same fake server.

Usage:
  desktop_updater_check.py check-source --repo . [--tag v0.7.11] --out FILE.json
        [--plugin-crate PATH | --plugin-offline] [--released-binary PATH]
  desktop_updater_check.py run --repo . --evidence DIR --label linux|windows|macos [--tag v0.7.11]
        [--plugin-crate PATH | --plugin-offline] [--native-check PATH] [--live-observation FILE]
        [--released-binary PATH]
Exit status 0 only when the verdict is DESKTOP_UPDATER_ISOLATED (check-source: when every item holds).

`run` writes result-<label>.json (schema arc.legacy-bridge.wave0-lab.stage-c-result.v1, read by stage_c_summary.py) and a copy named
result.json, unless result.json already holds another schema (an OS job's desktop-os-result.v1): that file is never overwritten.
The plugin crate is read from static.crates.io unless --plugin-crate names a local copy (sha256 pinned); a step without network
access must pass --plugin-crate, because a crate that could not be read fails I7 (it must not look like a pass).
Standard library only; Python 3.9+; no symlinks, no deletion commands, nothing but 127.0.0.1 (and the optional crate download).
"""
from __future__ import annotations

import argparse
import hashlib
import http.server
import io
import json
import os
import platform
import re
import shutil
import socketserver
import subprocess
import sys
import tarfile
import tempfile
import threading
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Tuple

SCHEMA_RESULT = "arc.legacy-bridge.wave0-lab.stage-c-result.v1"
SCHEMA_CHECK_SOURCE = "arc.legacy-bridge.wave0-lab.stage-c-check-source.v1"
SCHEMA_NATIVE = "arc.legacy-bridge.wave0-lab.native-updater-check.v1"
VERDICT_ISOLATED = "DESKTOP_UPDATER_ISOLATED"
VERDICT_NOT_ISOLATED = "DESKTOP_UPDATER_NOT_ISOLATED"

REPO = "FerrumVir/arc-chain"
TAG = "v0.7.11"
DESKTOP_VERSION = "0.7.11"
LATEST_TAG = "v0.7.12"
CONTROL_TAG = "v0.8.11"
APP_UA = "arc-desktop/0.1"
TAURI_ENDPOINT = f"https://github.com/{REPO}/releases/latest/download/latest.json"
API_LATEST = f"https://api.github.com/repos/{REPO}/releases/latest"
SIDECAR_TEMPLATE = f"https://github.com/{REPO}/releases/latest/download/{{}}"
LAUNCHER_ASSETS = (
    "arc-node-linux-aarch64",
    "arc-node-linux-x86_64",
    "arc-node-macos-arm64",
    "arc-node-macos-x86_64",
    "arc-node-windows-x86_64.exe",
)
FAKE_SIGNATURE = "ZmFrZSBzaWduYXR1cmU="  # base64("fake signature"): the harness never verifies, it proves the FETCH
# Display text of tauri_plugin_updater::Error::ReleaseNotFound (src/error.rs:25). The plugin analysis READS this text from the
# pinned crate and contradicts the pin when it differs; the install button shows it as "Update failed: <text>".
RELEASE_NOT_FOUND = "Could not fetch a valid release JSON from the remote"

# --------------------------------------------------------------------------------------------------------------
# What the released v0.7.11 binary says about itself (captain's extraction of the GitHub release asset, strings
# re-derived by F4 on 2026-10-08). Nothing here is downloaded in CI; unit tests re-verify it from the local copy.
# --------------------------------------------------------------------------------------------------------------
PINNED: Dict[str, Any] = {
    "provenance": (
        "usr/bin/arc-desktop extracted from the RELEASED v0.7.11 package ARC.Node_0.7.11_amd64.deb (GitHub release asset); "
        "crate versions from `strings -a` of that binary (cargo registry paths in panic/debug strings)"
    ),
    "release": {
        "tag": "v0.7.11",
        "package": "ARC.Node_0.7.11_amd64.deb",
        "package_sha256": "db0df355bb17f23a02b9323adcd7506053a8969ee3d98a23cfe92d1b4219a9c3",
        "package_size": 9168422,
        "binary": "usr/bin/arc-desktop",
        "binary_sha256": "63b1e4032ddc8fbbe3a31670d1182c08ec2e516e07f5d78baa01ab7b6033ab2d",
        "binary_size": 27178864,
    },
    "crates_from_binary_strings": {
        "tauri-plugin-updater": ["2.10.1"],
        "tauri": ["2.11.2"],
        "tauri-utils": ["2.9.2"],
        "wry": ["0.55.1"],
        "tao": ["0.35.3"],
        "reqwest": ["0.12.28", "0.13.4"],
        "minisign-verify": ["0.2.5"],
    },
    "binary_string_counts": {
        "latest.json": 1,
        "https://github.com/FerrumVir/arc-chain/releases/latest/download/latest.json": 1,
        "api.github.com/repos/FerrumVir/arc-chain/releases/latest": 1,
        "releases/latest/download/": 2,
        "githubusercontent": 0,
        "arc-desktop/0.1": 1,
    },
    "plugin_crate": {
        "name": "tauri-plugin-updater",
        "version": "2.10.1",
        "sha256": "806d9dac662c2e4594ff03c647a552f2c9bd544e7d0f683ec58f872f952ce4af",
        "url": "https://static.crates.io/crates/tauri-plugin-updater/tauri-plugin-updater-2.10.1.crate",
        "file_sha256": {
            "src/updater.rs": "d754b8887ad79c5155e4186e758529b320f4a404cbe5c32247e62f33e1fe5a05",
            "src/error.rs": "bd2504aca0b86c0e157d846fd72f110216617f2af29551f245f38b42a1a9884c",
            "src/config.rs": "71cfb43fd484a218422cccb89056bfd32236104f2ba5a2d937d337078cf60f45",
            "src/commands.rs": "a86459f10d3e7c1c82b4517c72d3bbf4ae186b7a1078e5bb25b84350ded76349",
            "src/lib.rs": "dd914a66c150f3bb6f5e8e8bbdc46b1ded00bda351279ecfe1e55491ff429351",
        },
        "crates_io_cross_check": (
            "observed 2026-10-08 via the crates.io API: 2.10.1 (published 2026-04-04) is also the highest 2.x published "
            "before the v0.7.11 release (2026-06-15), so a build without a lockfile would resolve to it as well"
        ),
    },
}

# Expected line numbers of tauri-plugin-updater 2.10.1 (verified by reading the pinned crate; the analyzer finds the
# lines dynamically and the comparison below only runs when the crate hash equals the pin).
PLUGIN_EXPECTED_LINES: Dict[str, int] = {
    "user_agent_const": 44,
    "endpoints_validate": 198,
    "check_fn": 386,
    "accept_header": 390,
    "endpoint_loop": 412,
    "per_endpoint_client": 451,
    "is_success": 483,
    "no_content": 485,
    "no_content_return": 487,
    "non_success_branch": 508,
    "last_error_return": 523,
    "release_not_found": 528,
    "version_compare": 532,
    "get_urls_call": 536,
    "error_release_not_found_text": 25,
    "error_release_not_found_variant": 26,
    "error_serialize_str": 101,
}

# Exhaustive inventories of the v0.7.11 desktop sources (git grep -n -E at the tag). A hidden fallback, a second
# updater call or a new disk write would change one of these sets and fail the check.
SEARCH_DIRS = ["desktop/src", "desktop/src-tauri/src"]
_C = "desktop/src-tauri/src/commands.rs"
_L = "desktop/src-tauri/src/lib.rs"
_S = "desktop/src/screens/Settings.tsx"
_T = "desktop/src/lib/tauri.ts"
EXPECTED_INVENTORIES: List[Tuple[str, str, List[Tuple[str, int]]]] = [
    ("latest.json", r"latest\.json", []),
    ("releases/latest", r"releases/latest", [(_C, 942), (_C, 1018)]),
    ("api.github.com", r"api\.github\.com", [(_C, 942)]),
    ("github.com", r"github\.com", [(_C, 942), (_C, 1018), ("desktop/src/components/Titlebar.tsx", 45)]),
    ("githubusercontent", r"githubusercontent", []),
    ("@tauri-apps/plugin-updater", r"plugin-updater", [(_S, 4)]),
    ("tauri_plugin_updater", r"tauri_plugin_updater", [(_L, 60)]),
    ("downloadAndInstall", r"downloadAndInstall", [(_L, 62), (_S, 45)]),
    ("tauriCheckUpdate", r"tauriCheckUpdate", [(_S, 4), (_S, 39)]),
    ("check( calls", r"(^|[^A-Za-z0-9_.])check\(", []),
    ("installUpdate", r"installUpdate", [(_S, 35), (_S, 206)]),
    (
        "autoUpdate/auto_update",
        r"autoUpdate|auto_update",
        [
            ("desktop/src/lib/types.ts", 75),
            ("desktop/src/screens/Dashboard.tsx", 86),
            ("desktop/src/screens/Onboarding.tsx", 160),
            (_S, 18),
            (_S, 56),
            (_S, 127),
            ("desktop/src-tauri/src/types.rs", 35),
            ("desktop/src-tauri/src/types.rs", 50),
        ],
    ),
    (
        "check_for_update/checkForUpdate",
        r"check_for_update|checkForUpdate",
        [(_C, 934), (_L, 155), (_T, 533), (_T, 931), (_T, 1054), (_T, 1055), (_S, 24)],
    ),
    (
        "ensure_binary/ensureBinary",
        r"ensure_binary|ensureBinary",
        [(_C, 110), (_C, 140), (_C, 975), (_L, 165), ("desktop/src-tauri/src/node_manager.rs", 413), (_T, 535), (_T, 933), (_T, 1056), ("desktop/src/screens/Onboarding.tsx", 167)],
    ),
    ("updater (word)", r"updater", [(_C, 105), (_C, 972), (_L, 60), (_S, 4), (_S, 33)]),
]
EXPECTED_WRITE_SITES: List[Tuple[str, int]] = [
    (_C, 1023), (_C, 1043), (_C, 1045), (_C, 1048),  # ensure_binary: the sidecar launcher download
    (_C, 1314), (_C, 1339), (_C, 1358), (_C, 1379), (_C, 1382),  # model download
    ("desktop/src-tauri/src/node_manager.rs", 108),  # data directory
    ("desktop/src-tauri/src/store.rs", 35), ("desktop/src-tauri/src/store.rs", 36),  # NodeConfig JSON (preferences)
]
WRITE_PATTERN = r"std::fs::(write|rename|create_dir|copy|OpenOptions)|fs::write|File::create|OpenOptions|write_all|create_dir_all|tokio::fs"
EXPECTED_STORAGE_SITES: List[Tuple[str, int]] = [
    ("desktop/src/lib/store.ts", 39), ("desktop/src/lib/store.ts", 48), ("desktop/src/lib/store.ts", 75), ("desktop/src/lib/store.ts", 77),
    (_T, 266), (_T, 267), (_T, 296),
]
STORAGE_PATTERN = r"localStorage|sessionStorage|indexedDB|document\.cookie|caches\."
CHECK_FOR_UPDATE_RANGE = (934, 957)


# --------------------------------------------------------------------------------------------------------------
# small helpers
# --------------------------------------------------------------------------------------------------------------

def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def dump(value: Any) -> str:
    return json.dumps(value, indent=2, sort_keys=True) + "\n"


def write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(text.encode("utf-8"))


def write_jsonl(path: Path, rows: List[dict]) -> None:
    write_text(path, "".join(json.dumps(row, sort_keys=True) + "\n" for row in rows))


def cite(file: str, line: Optional[int], text: str) -> dict:
    return {"file": file, "line": line, "text": text.strip()[:170]}


def find_line(lines: List[str], needle: str, start: int = 1) -> Optional[int]:
    """1-based number of the first line at or after `start` containing `needle`."""
    for index in range(max(start, 1) - 1, len(lines)):
        if needle in lines[index]:
            return index + 1
    return None


def find_regex_line(lines: List[str], pattern: str, start: int = 1) -> Optional[int]:
    compiled = re.compile(pattern)
    for index in range(max(start, 1) - 1, len(lines)):
        if compiled.search(lines[index]):
            return index + 1
    return None


def blank_rust(source: str) -> str:
    """Blank comments and the inside of string/char literals, keeping every newline and offset."""
    out: List[str] = []
    i, n = 0, len(source)
    while i < n:
        c = source[i]
        if source.startswith("//", i):
            j = source.find("\n", i)
            j = n if j == -1 else j
            out.append(" " * (j - i))
            i = j
        elif source.startswith("/*", i):
            j = source.find("*/", i + 2)
            j = n if j == -1 else j + 2
            out.append("".join(ch if ch == "\n" else " " for ch in source[i:j]))
            i = j
        elif c == '"':
            j = i + 1
            while j < n and source[j] != '"':
                j += 2 if source[j] == "\\" else 1
            j = min(j + 1, n)
            inner = source[i + 1:j - 1] if j - i >= 2 else ""
            out.append('"' + "".join(ch if ch == "\n" else " " for ch in inner) + '"')
            i = j
        elif c == "'":
            match = re.match(r"'(?:\\.|[^'\\\n])'", source[i:i + 8])
            if match:
                out.append("'" + " " * (len(match.group(0)) - 2) + "'")
                i += len(match.group(0))
            else:
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


def rust_block_range(source: str, start_line: int) -> Tuple[int, int]:
    """(first, last) 1-based lines of the brace block that opens on or after `start_line`."""
    blanked = blank_rust(source).split("\n")
    depth = 0
    opened = False
    for index in range(start_line - 1, len(blanked)):
        for ch in blanked[index]:
            if ch == "{":
                depth += 1
                opened = True
            elif ch == "}":
                depth -= 1
        if opened and depth <= 0:
            return start_line, index + 1
    raise ValueError(f"no closing brace for the block starting at line {start_line}")


# --------------------------------------------------------------------------------------------------------------
# git access (the CI job does a depth-1 fetch of the tag first)
# --------------------------------------------------------------------------------------------------------------

class GitError(RuntimeError):
    pass


def git(repo: Path, *args: str, check: bool = True) -> "subprocess.CompletedProcess[bytes]":
    done = subprocess.run(["git", "-C", str(repo), *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if check and done.returncode != 0:
        raise GitError(f"git {' '.join(args)} failed ({done.returncode}): {done.stderr.decode('utf-8', 'replace').strip()}")
    return done


def git_text(repo: Path, tag: str, path: str) -> str:
    return git(repo, "show", f"{tag}:{path}").stdout.decode("utf-8").replace("\r\n", "\n")


def git_blob_id(repo: Path, tag: str, path: str) -> str:
    return git(repo, "rev-parse", f"{tag}:{path}").stdout.decode().strip()


def git_grep(repo: Path, tag: str, pattern: str, paths: List[str]) -> List[Tuple[str, int, str]]:
    done = git(repo, "grep", "-n", "-I", "-E", pattern, tag, "--", *paths, check=False)
    if done.returncode not in (0, 1):
        raise GitError(f"git grep failed ({done.returncode}): {done.stderr.decode('utf-8', 'replace').strip()}")
    rows: List[Tuple[str, int, str]] = []
    prefix = f"{tag}:"
    for raw in done.stdout.decode("utf-8", "replace").splitlines():
        rest = raw[len(prefix):] if raw.startswith(prefix) else raw
        path, number, text = rest.split(":", 2)
        rows.append((path, int(number), text.rstrip("\r")))
    rows.sort()
    return rows


# --------------------------------------------------------------------------------------------------------------
# static plugin semantics (tauri-plugin-updater, pinned version)
# --------------------------------------------------------------------------------------------------------------

def analyze_plugin_source(files: Dict[str, str], pinned_lines: Optional[Dict[str, int]] = None) -> dict:
    """Read the plugin sources and test every assumption the isolation argument rests on.

    Returns {"status": "confirmed"|"contradicted", "findings": [...], "contradictions": [...], "lines": {...}}."""
    findings: List[dict] = []
    contradictions: List[str] = []
    lines_found: Dict[str, int] = {}

    def need(item: str, ok: bool, detail: str, cites: List[dict]) -> None:
        findings.append({"id": item, "ok": bool(ok), "detail": detail, "citations": cites})
        if not ok:
            contradictions.append(f"{item}: {detail}")

    updater = files.get("src/updater.rs", "")
    commands = files.get("src/commands.rs", "")
    ulines = updater.split("\n")

    def at(name: str, line: Optional[int], text: str = "") -> List[dict]:
        if line is None:
            return []
        lines_found[name] = line
        return [cite("src/updater.rs", line, text or ulines[line - 1])]

    ua_line = find_regex_line(ulines, r'const UPDATER_USER_AGENT: &str = concat!\(env!\("CARGO_PKG_NAME"\), "/", env!\("CARGO_PKG_VERSION"\)')
    need("user-agent", ua_line is not None, "the updater identifies itself as <crate name>/<crate version>", at("user_agent_const", ua_line))

    validate_line = find_line(ulines, "crate::config::validate_endpoints(")
    need("endpoint-transport-gate", validate_line is not None, "endpoints() validates the transport (https unless dangerousInsecureTransportProtocol)", at("endpoints_validate", validate_line))

    check_line = find_regex_line(ulines, r"pub async fn check\(&self\) -> Result<Option<Update>>")
    if check_line is None:
        need("check-fn", False, "pub async fn check(&self) -> Result<Option<Update>> not found", [])
        return {"status": "contradicted", "findings": findings, "contradictions": contradictions, "lines": lines_found}
    first, last = rust_block_range(updater, check_line)
    lines_found["check_fn"] = first
    body = "\n".join(ulines[first - 1:last])
    blanked_body = "\n".join(blank_rust(updater).split("\n")[first - 1:last])

    accept_line = find_regex_line(ulines, r'headers\.insert\(ACCEPT, HeaderValue::from_static\("application/json"\)\)', first)
    need("accept-json", accept_line is not None and accept_line < last, "check() asks for application/json unless the caller set Accept", at("accept_header", accept_line))

    loop_line = find_line(ulines, "for url in &self.endpoints {", first)
    need("endpoint-iteration", loop_line is not None and loop_line < last, "check() walks self.endpoints in order, one request per endpoint", at("endpoint_loop", loop_line))

    client_line = find_regex_line(ulines, r"ClientBuilder::new\(\)\.user_agent\(UPDATER_USER_AGENT\)", first)
    need("client-per-endpoint", client_line is not None and loop_line is not None and client_line > loop_line, "a fresh client with the updater user agent is built inside the endpoint loop", at("per_endpoint_client", client_line))

    success_line = find_line(ulines, "if res.status().is_success() {", first)
    need("success-branch", success_line is not None, "only a 2xx response is read as a release", at("is_success", success_line))

    nc_line = find_line(ulines, "if StatusCode::NO_CONTENT == res.status() {", first)
    ret_line = find_line(ulines, "return Ok(None);", nc_line or first)
    need(
        "204-means-no-update",
        nc_line is not None and ret_line is not None and ret_line - (nc_line or 0) <= 3,
        "a 204 No Content answer returns Ok(None) immediately: no update",
        at("no_content", nc_line) + at("no_content_return", ret_line),
    )

    else_line = None
    if success_line is not None:
        indent = len(ulines[success_line - 1]) - len(ulines[success_line - 1].lstrip())
        for number in range(success_line + 1, last + 1):
            text = ulines[number - 1]
            if text.strip() == "} else {" and len(text) - len(text.lstrip()) == indent:
                else_line = number
                break
    else_end = None
    if else_line is not None:
        indent = len(ulines[else_line - 1]) - len(ulines[else_line - 1].lstrip())
        for number in range(else_line + 1, last + 1):
            text = ulines[number - 1]
            if text.strip() == "}" and len(text) - len(text.lstrip()) == indent:
                else_end = number
                break
    else_text = "\n".join(ulines[else_line:else_end - 1]) if else_line and else_end else ""
    need(
        "non-success-continues",
        bool(else_text) and "log::error!" in else_text and "last_error" not in else_text and "return" not in else_text and "break" not in else_text,
        "a non-success status (404 included) is only logged: no error is recorded, no return; the loop moves to the next endpoint",
        at("non_success_branch", else_line, ("} else { ... log::error! ... } (lines %s-%s)" % (else_line, else_end)) if else_line else ""),
    )

    if_err_line = find_line(ulines, "if let Some(error) = last_error {", first)
    err_ret_line = find_line(ulines, "return Err(error);", if_err_line or first)
    need("last-error-returned", if_err_line is not None and err_ret_line is not None, "after the loop a recorded error is returned", at("last_error_return", if_err_line))

    rnf_line = find_line(ulines, "let release = remote_release.ok_or(Error::ReleaseNotFound)?;", first)
    need(
        "none-left-means-release-not-found",
        rnf_line is not None and if_err_line is not None and rnf_line > if_err_line,
        "with no release found and no recorded error, check() returns Err(Error::ReleaseNotFound)",
        at("release_not_found", rnf_line),
    )

    cmp_line = find_line(ulines, "None => release.version > self.current_version,", first)
    need("semver-compare", cmp_line is not None, "by default only a strictly greater semver counts as an update", at("version_compare", cmp_line))

    urls_line = find_line(ulines, "self.get_urls(&release, &installer)?;", first)
    findings.append({"id": "platform-lookup-before-compare", "ok": True, "detail": "info: the platform entry is resolved (and can fail with TargetsNotFound) before the version comparison", "citations": at("get_urls_call", urls_line)})

    redirect_hits = [name for name, text in files.items() if name.endswith(".rs") and re.search(r"redirect", text, re.IGNORECASE)]
    need(
        "redirect-default-policy",
        not redirect_hits,
        "no redirect configuration anywhere in the plugin: reqwest's default policy (follow up to 10 redirects) applies, so releases/latest/download/... redirects are followed",
        [cite(name, None, "searched for 'redirect' (case-insensitive): no occurrence") for name in sorted(k for k in files if k.endswith(".rs"))],
    )

    cache_tokens = [token for token in ("std::fs", "fs::", "File::", "cache", "tempfile", "dirs::", "write(", "persist") if token in blanked_body]
    need(
        "no-disk-cache-in-check",
        not cache_tokens,
        "check() performs no file I/O and keeps no cache (tokens searched: std::fs, fs::, File::, cache, tempfile, dirs::, write(, persist)",
        [cite("src/updater.rs", first, f"check() spans lines {first}-{last}")],
    )
    disk_lines = [i + 1 for i, text in enumerate(ulines) if "dirs::cache_dir" in text or "tempfile::Builder" in text]
    outside = [number for number in disk_lines if first <= number <= last]
    need(
        "disk-use-only-after-download",
        bool(disk_lines) and not outside,
        f"temp/cache directories appear only in install paths after a downloaded bundle (lines {disk_lines[:8]}...), never inside check()",
        [cite("src/updater.rs", number, ulines[number - 1]) for number in disk_lines[:4]],
    )

    cmd_lines = commands.split("\n")
    cmd_check = find_regex_line(cmd_lines, r"pub\(crate\) async fn check<R: Runtime>\(")
    signature = "\n".join(cmd_lines[cmd_check - 1:cmd_check + 8]) if cmd_check else ""
    need(
        "js-check-cannot-override-endpoints",
        bool(cmd_check) and "endpoints" not in signature.split("->")[0],
        "the JS-facing check command takes headers/timeout/proxy/target/allow_downgrades and NO endpoints: the endpoint list comes only from tauri.conf.json",
        [cite("src/commands.rs", cmd_check or 0, "pub(crate) async fn check<R: Runtime>(...)")],
    )

    errors = files.get("src/error.rs", "").split("\n")
    variant_line = find_regex_line(errors, r"^\s*ReleaseNotFound,\s*$")
    shown = None
    if variant_line is not None and variant_line >= 2:
        attribute = re.match(r'^\s*#\[error\("(.*)"\)\]\s*$', errors[variant_line - 2])
        shown = attribute.group(1) if attribute else None
        lines_found["error_release_not_found_text"] = variant_line - 1
        lines_found["error_release_not_found_variant"] = variant_line
    need(
        "release-not-found-display-text",
        shown == RELEASE_NOT_FOUND,
        f"Error::ReleaseNotFound displays as {shown!r}: the text the install button shows after 'Update failed: '",
        [cite("src/error.rs", variant_line - 1, errors[variant_line - 2]), cite("src/error.rs", variant_line, errors[variant_line - 1])] if variant_line and variant_line >= 2 else [],
    )
    serialize_line = find_line(errors, "serializer.serialize_str(self.to_string().as_ref())")
    if serialize_line is not None:
        lines_found["error_serialize_str"] = serialize_line
    need(
        "errors-reach-js-as-display-strings",
        serialize_line is not None,
        "plugin errors are serialised to the JS side as their Display string, so Settings.tsx catch shows String(e) = the text above",
        [cite("src/error.rs", serialize_line, errors[serialize_line - 1])] if serialize_line else [],
    )

    status = "confirmed" if not contradictions else "contradicted"
    if pinned_lines is not None and status == "confirmed":
        mismatched = {name: (pinned_lines[name], lines_found.get(name)) for name in pinned_lines if lines_found.get(name) != pinned_lines[name]}
        if mismatched:
            contradictions.append(f"line numbers differ from the pinned reading: {mismatched}")
            status = "contradicted"
    return {"status": status, "findings": findings, "contradictions": contradictions, "lines": lines_found, "release_not_found_text": shown}


def plugin_semantics(
    plugin_crate: Optional[Path] = None,
    offline: bool = False,
    fetch: Optional[Callable[[str], bytes]] = None,
) -> dict:
    """Pinned-crate analysis. status: confirmed | contradicted | unverified (reason offline-requested | fetch-failed, said loudly)."""
    pin = PINNED["plugin_crate"]
    base = {
        "crate": f"{pin['name']} {pin['version']}",
        "pinned_by": PINNED["provenance"],
        "crate_sha256_pinned": pin["sha256"],
        "crates_io_cross_check": pin["crates_io_cross_check"],
        "shipped_app_version_note": (
            "the desktop has no Cargo.lock at the v0.7.11 tag; the version is read from the strings of the RELEASED binary "
            "(tauri-plugin-updater-2.10.1, tauri-2.11.2), so it is pinned by the shipped artifact, not by the source tag"
        ),
    }
    data: Optional[bytes] = None
    if plugin_crate is not None:
        try:
            data = Path(plugin_crate).read_bytes()
        except OSError as error:
            return dict(base, status="unverified", reason="fetch-failed", loud_note=f"PLUGIN SEMANTICS NOT VERIFIED: could not read the plugin crate file ({type(error).__name__}: {error})", findings=[], contradictions=[], lines={})
        base["source"] = f"local file {Path(plugin_crate).name}"
    elif offline:
        return dict(base, status="unverified", reason="offline-requested", loud_note="PLUGIN SEMANTICS NOT VERIFIED: offline requested; the static reading of tauri-plugin-updater was skipped", findings=[], contradictions=[], lines={})
    else:
        try:
            data = (fetch or _http_get)(pin["url"])
            base["source"] = pin["url"]
        except Exception as error:  # noqa: BLE001 - a crates.io outage must not look like a pass
            return dict(base, status="unverified", reason="fetch-failed", loud_note=f"PLUGIN SEMANTICS NOT VERIFIED: could not fetch the pinned crate ({type(error).__name__}: {error})", findings=[], contradictions=[], lines={})
    digest = sha256_bytes(data)
    base["crate_sha256_observed"] = digest
    if digest != pin["sha256"]:
        return dict(base, status="contradicted", loud_note=f"PLUGIN CRATE DIGEST MISMATCH: {digest} != pinned {pin['sha256']}", findings=[], contradictions=["crate digest differs from the pin"], lines={})
    files: Dict[str, str] = {}
    hashes: Dict[str, str] = {}
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        for member in archive.getmembers():
            name = member.name.split("/", 1)[1] if "/" in member.name else member.name
            if member.isfile() and name in pin["file_sha256"]:
                content = archive.extractfile(member).read()  # type: ignore[union-attr]
                files[name] = content.decode("utf-8")
                hashes[name] = sha256_bytes(content)
    base["file_sha256_observed"] = hashes
    wrong = {name: (hashes.get(name), want) for name, want in pin["file_sha256"].items() if hashes.get(name) != want}
    analysis = analyze_plugin_source(files, PLUGIN_EXPECTED_LINES)
    if wrong:
        analysis["contradictions"].append(f"source file digests differ from the pin: {wrong}")
        analysis["status"] = "contradicted"
    result = dict(base, **analysis)
    if result["status"] == "contradicted":
        result["loud_note"] = "THE PLUGIN SOURCE CONTRADICTS THE ISOLATION ASSUMPTIONS: " + "; ".join(analysis["contradictions"])
    return result


def _http_get(url: str, timeout: float = 60.0) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": "arc-wave0-lab-desktop-updater-check/1 (read-only)"})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(request, timeout=timeout) as response:
        return response.read()


def plugin_acceptable(plugin: dict) -> bool:
    """A crates.io outage (fetch-failed) must not look like a pass; an explicit offline request is the operator's choice and stays loud."""
    return plugin["status"] == "confirmed" or (plugin["status"] == "unverified" and plugin.get("reason") == "offline-requested")


def verify_released_binary(path: Path) -> dict:
    """Re-verify the embedded facts about the released binary from a local copy (unit tests; never run in CI)."""
    data = Path(path).read_bytes()
    release = PINNED["release"]
    problems: List[str] = []
    if sha256_bytes(data) != release["binary_sha256"] or len(data) != release["binary_size"]:
        problems.append("binary digest or size differs from the pin")
    for needle, want in PINNED["binary_string_counts"].items():
        got = data.count(needle.encode())
        if got != want:
            problems.append(f"{needle!r}: {got} occurrences, pinned {want}")
    for crate, versions in PINNED["crates_from_binary_strings"].items():
        found = sorted({m.group(0).decode() for m in re.finditer(rb"(?<![A-Za-z0-9_-])" + re.escape(crate.encode()) + rb"-[0-9]+\.[0-9]+\.[0-9]+", data)})
        want = sorted(f"{crate}-{v}" for v in versions)
        if found != want:
            problems.append(f"{crate}: strings say {found}, pinned {want}")
    return {"verified_locally": not problems, "problems": problems, "file": Path(path).name}


# --------------------------------------------------------------------------------------------------------------
# check-source
# --------------------------------------------------------------------------------------------------------------

def _item(item_id: str, title: str, ok: bool, detail: str, cites: Optional[List[dict]] = None) -> dict:
    return {"id": item_id, "title": title, "ok": bool(ok), "detail": detail, "citations": cites or []}


def _expect_all(text: str, required: List[str]) -> List[str]:
    return [needle for needle in required if needle not in text]


def check_source(
    repo: Path,
    tag: str = TAG,
    plugin_crate: Optional[Path] = None,
    plugin_offline: bool = False,
    released_binary: Optional[Path] = None,
    fetch: Optional[Callable[[str], bytes]] = None,
) -> dict:
    repo = Path(repo)
    sources: Dict[str, dict] = {}

    def read(path: str) -> str:
        text = git_text(repo, tag, path)
        sources[path] = {"git_blob": git_blob_id(repo, tag, path), "sha256": sha256_bytes(text.encode("utf-8"))}
        return text

    conf_text = read("desktop/src-tauri/tauri.conf.json")
    lib_text = read("desktop/src-tauri/src/lib.rs")
    cmd_text = read("desktop/src-tauri/src/commands.rs")
    settings_text = read("desktop/src/screens/Settings.tsx")
    read("desktop/src/lib/tauri.ts")
    cargo_text = read("desktop/src-tauri/Cargo.toml")
    info = {
        "commit": git(repo, "rev-parse", f"{tag}^{{commit}}").stdout.decode().strip(),
        "commit_date": git(repo, "log", "-1", "--format=%cI", tag).stdout.decode().strip(),
    }
    items: List[dict] = []
    cmd_lines = cmd_text.split("\n")
    lib_lines = lib_text.split("\n")
    set_lines = settings_text.split("\n")
    conf_lines = conf_text.split("\n")

    # (a) the single Tauri endpoint
    endpoints: List[str] = []
    try:
        conf = json.loads(conf_text)
        updater = conf["plugins"]["updater"]
        endpoints = [e for e in updater.get("endpoints", []) if isinstance(e, str)] if isinstance(updater.get("endpoints"), list) else []
        conf_ok = (
            updater.get("endpoints") == [TAURI_ENDPOINT]
            and updater.get("active") is True
            and isinstance(updater.get("pubkey"), str)
            and bool(updater["pubkey"])
            and conf["bundle"].get("createUpdaterArtifacts") is True
            and not any(key.startswith("dangerous") for key in updater)
        )
        pubkey = updater.get("pubkey", "")
        detail = f"endpoints={updater.get('endpoints')}; active={updater.get('active')}; createUpdaterArtifacts={conf['bundle'].get('createUpdaterArtifacts')}; pubkey sha256 {sha256_bytes(pubkey.encode())[:16]}"
    except (ValueError, KeyError, AttributeError) as error:
        conf_ok, pubkey, detail = False, "", f"tauri.conf.json unreadable: {error}"
    ep_line = find_line(conf_lines, TAURI_ENDPOINT) or 0
    items.append(_item("a-tauri-endpoint", "exactly ONE Tauri updater endpoint (releases/latest/download/latest.json), no dangerous flags", conf_ok, detail,
                       [cite("desktop/src-tauri/tauri.conf.json", find_line(conf_lines, '"endpoints"') or 0, '"endpoints": [...]'), cite("desktop/src-tauri/tauri.conf.json", ep_line, TAURI_ENDPOINT)]))

    # (b) plugin init without overrides
    init_re = re.compile(r"^\s*\.plugin\(tauri_plugin_updater::Builder::new\(\)\.build\(\)\)\s*$")
    init_lines = [i + 1 for i, text in enumerate(lib_lines) if init_re.match(text)]
    rust_hits = git_grep(repo, tag, r"tauri_plugin_updater", ["desktop/src-tauri/src"])
    items.append(_item("b-plugin-init", "the updater plugin is initialised once with default Builder (no endpoint override)", len(init_lines) == 1 and len(rust_hits) == 1,
                       f"init lines {init_lines}; every tauri_plugin_updater mention: {[(p, n) for p, n, _ in rust_hits]}",
                       [cite("desktop/src-tauri/src/lib.rs", n, lib_lines[n - 1]) for n in init_lines]))

    # (c) check_for_update
    start = find_regex_line(cmd_lines, r"pub async fn check_for_update\(\)")
    api_url = ""
    c_ok, c_detail, c_cites = False, "check_for_update not found", []
    if start:
        first, last = rust_block_range(cmd_text, start)
        body = "\n".join(cmd_lines[first - 1:last])
        required = [
            '.user_agent("arc-desktop/0.1")',
            ".timeout(std::time::Duration::from_secs(8))",
            ".trim_start_matches('v')",
            'has_update: version != current && version != "unknown",',
            'env!("CARGO_PKG_VERSION")',
        ]
        missing = _expect_all(body, required)
        match = re.search(r'\.get\("(https://api\.github\.com/[^"]+)"\)', body)
        api_url = match.group(1) if match else ""
        forbidden = [token for token in ("latest.json", "std::fs", "File::", "write", ".download") if token in blank_rust(body)]
        pkg_version = re.search(r'^version = "([^"]+)"', "\n".join(cargo_text.split("[dependencies]")[0].split("\n")), re.MULTILINE)
        c_ok = not missing and api_url == API_LATEST and not forbidden and (first, last) == CHECK_FOR_UPDATE_RANGE and bool(pkg_version) and pkg_version.group(1) == DESKTOP_VERSION
        c_detail = f"lines {first}-{last}; url {api_url}; missing {missing}; forbidden tokens {forbidden}; CARGO_PKG_VERSION {pkg_version.group(1) if pkg_version else None}"
        c_cites = [cite("desktop/src-tauri/src/commands.rs", find_line(cmd_lines, token, first) or 0, token) for token in (".get(\"https://api.github.com", "has_update: version != current")]
    items.append(_item("c-check-for-update", "check_for_update: one GET of api.github.com releases/latest; has_update = (tag != 0.7.11); no file I/O", c_ok, c_detail, c_cites))

    # (d) ensure_binary
    d_ok, d_detail, d_cites, template, asset_map = False, "ensure_binary not found", [], "", {}
    start = find_regex_line(cmd_lines, r"pub async fn ensure_binary\(app: AppHandle\)")
    if start:
        first, last = rust_block_range(cmd_text, start)
        body = "\n".join(cmd_lines[first - 1:last])
        required = [
            '.user_agent("arc-desktop/0.1")',
            ".timeout(std::time::Duration::from_secs(600))",
            "if !resp.status().is_success() {",
            'let tmp = target.with_extension("download");',
            "std::fs::rename(&tmp, &target)",
        ]
        missing = _expect_all(body, required)
        match = re.search(r'"(https://github\.com/[^"]*releases/latest/download/\{\})"', body)
        template = match.group(1) if match else ""
        asset_map = {f"{os_}-{arch}": asset for os_, arch, asset in re.findall(r'\("(\w+)", "(\w+)"\) => Some\("([^"]+)"\)', cmd_text)}
        calls = [n for n, text in enumerate(cmd_lines, 1) if "ensure_binary(app.clone()).await?;" in text]
        d_ok = not missing and template == SIDECAR_TEMPLATE and len(asset_map) == 4 and calls == [110, 140]
        d_detail = f"lines {first}-{last}; template {template}; missing {missing}; platform assets {asset_map}; callers {calls}"
        d_cites = [cite("desktop/src-tauri/src/commands.rs", find_line(cmd_lines, "releases/latest/download/{}", first) or 0, "releases/latest/download/{}")]
        d_cites += [cite("desktop/src-tauri/src/commands.rs", n, cmd_lines[n - 1]) for n in calls]
    items.append(_item("d-ensure-binary", "ensure_binary: sidecar launcher download from releases/latest/download/<platform asset>; called by start_node and restart_node only", d_ok, d_detail, d_cites))

    # (e) Settings.tsx flow
    e_missing: List[str] = []
    numbers: Dict[str, int] = {}
    for name, needle in (
        ("import", 'import { check as tauriCheckUpdate } from "@tauri-apps/plugin-updater";'),
        ("queryFn", "queryFn: api.checkForUpdate,"),
        ("enabled", "enabled: false,"),
        ("try", "try {"),
        ("check", "const u = await tauriCheckUpdate();"),
        ("guard", "if (!u) {"),
        ("noupdate", 'setInstallError("No update available.");'),
        ("download", "await u.downloadAndInstall();"),
        ("relaunch", "await tauriRelaunch();"),
        ("catch", "} catch (e) {"),
        ("seterror", "setInstallError(e instanceof Error ? e.message : String(e));"),
        ("available", "is available. Click below to download, install, and relaunch."),
        ("failed", "Update failed: {installError}"),
        ("button", "`Install v${update.version} & relaunch`"),
        ("click", "onClick={() => checkUpdate()}"),
    ):
        line = find_line(set_lines, needle, numbers.get("try", 1) if name in ("check", "guard", "noupdate", "download", "relaunch", "catch", "seterror") else 1)
        if line is None:
            e_missing.append(needle)
        else:
            numbers[name] = line
    order_ok = not e_missing and numbers["try"] < numbers["check"] < numbers["guard"] < numbers["download"] < numbers["relaunch"] < numbers["catch"] < numbers["seterror"]
    items.append(_item("e-settings-flow", "Settings: the Rust check only on click; the plugin check() then downloadAndInstall only for an Update object, errors shown, never auto-run", order_ok,
                       f"line numbers {numbers}; missing {e_missing}",
                       [cite("desktop/src/screens/Settings.tsx", n, set_lines[n - 1]) for n in sorted(numbers.values())][:10]))

    # (f) inventories
    inventory_report: Dict[str, dict] = {}
    inventory_ok = True
    for name, pattern, expected in EXPECTED_INVENTORIES:
        observed = [(p, n) for p, n, _ in git_grep(repo, tag, pattern, SEARCH_DIRS)]
        ok = sorted(observed) == sorted(expected)
        inventory_ok = inventory_ok and ok
        inventory_report[name] = {"pattern": pattern, "ok": ok, "observed": observed, "expected": expected}
    conf_hits = [n for n, text in enumerate(conf_lines, 1) if "latest.json" in text]
    inventory_report["latest.json in tauri.conf.json"] = {"pattern": "latest.json", "ok": len(conf_hits) == 1, "observed": conf_hits, "expected": [ep_line]}
    inventory_ok = inventory_ok and len(conf_hits) == 1
    bad = [name for name, entry in inventory_report.items() if not entry["ok"]]
    items.append(_item("f-inventories", "exhaustive inventories (latest.json, releases/latest, api.github.com, plugin-updater, downloadAndInstall, check( calls, autoUpdate, ...) equal the known sets", inventory_ok,
                       "all inventories equal the known sets" if inventory_ok else f"DIFFERENT: {bad}"))

    # (g) no update metadata written to disk (heuristic)
    writes = [(p, n, t) for p, n, t in git_grep(repo, tag, WRITE_PATTERN, ["desktop/src-tauri/src"])]
    storage = [(p, n, t) for p, n, t in git_grep(repo, tag, STORAGE_PATTERN, ["desktop/src"])]
    in_check = [(p, n) for p, n, _ in writes if p == _C and CHECK_FOR_UPDATE_RANGE[0] <= n <= CHECK_FOR_UPDATE_RANGE[1]]
    updaty = [(p, n) for p, n, t in writes + storage if re.search(r"update|latest|etag|release", t, re.IGNORECASE)]
    g_ok = (
        sorted((p, n) for p, n, _ in writes) == sorted(EXPECTED_WRITE_SITES)
        and sorted((p, n) for p, n, _ in storage) == sorted(EXPECTED_STORAGE_SITES)
        and not in_check
        and not updaty
    )
    items.append(_item(
        "g-no-update-disk-writes",
        "no code path writes update metadata to disk (heuristic)",
        g_ok,
        f"{len(writes)} Rust write sites (sidecar download, model download, data dir, NodeConfig preferences) and {len(storage)} browser-storage sites all match the known sets; inside check_for_update: {in_check}; lines mentioning update/latest/etag/release: {updaty}. "
        "HEURISTIC: a text-level inventory of write calls and browser-storage APIs; it cannot see writes built from dynamic strings and does not cover the plugin's own behaviour (see plugin_semantics) or the OS.",
    ))

    plugin = plugin_semantics(plugin_crate, plugin_offline, fetch)
    released = {
        "package": PINNED["release"]["package"],
        "package_sha256": PINNED["release"]["package_sha256"],
        "binary": PINNED["release"]["binary"],
        "binary_sha256": PINNED["release"]["binary_sha256"],
        "binary_size": PINNED["release"]["binary_size"],
        "crates_from_binary_strings": PINNED["crates_from_binary_strings"],
        "binary_string_counts": PINNED["binary_string_counts"],
        "reading": (
            "the released binary holds exactly one latest.json string (the single endpoint, immediately followed by the base64 pubkey), one "
            "api.github.com releases/latest URL and no githubusercontent string"
        ),
        "verified_locally": False,
        "note": "values embedded from the captain's extraction of the GitHub release asset (the 9 MB package is not downloaded in CI); re-verified in the unit tests from the local copy",
    }
    if released_binary is not None:
        released.update(verify_released_binary(released_binary))
    ok_items = all(item["ok"] for item in items)
    verdict = "SOURCE_VERIFIED" if ok_items and plugin_acceptable(plugin) and released.get("problems", []) == [] else "SOURCE_NOT_VERIFIED"
    return {
        "schema": SCHEMA_CHECK_SOURCE,
        "tag": tag,
        "tag_commit": info["commit"],
        "tag_commit_date": info["commit_date"],
        "items": items,
        "inventories": inventory_report,
        "plugin_semantics": plugin,
        "released_binary": released,
        "sources": sources,
        "extracted": {
            "api_url": api_url,
            "sidecar_template": template,
            "platform_assets": asset_map,
            "pubkey_sha256": sha256_bytes(pubkey.encode()) if pubkey else "",
            "pubkey": pubkey,
            "endpoints": endpoints,
            "settings_templates": {
                "available": settings_template(settings_text, SETTINGS_AVAILABLE),
                "install_button": settings_template(settings_text, SETTINGS_BUTTON),
            },
        },
        "verdict": verdict,
    }


# --------------------------------------------------------------------------------------------------------------
# the fake GitHub
# --------------------------------------------------------------------------------------------------------------

def bundle_names(version: str) -> List[str]:
    return [
        f"ARC.Node_{version}_amd64.AppImage", f"ARC.Node_{version}_amd64.AppImage.sig", f"ARC.Node_{version}_amd64.deb",
        f"ARC.Node-{version}-1.x86_64.rpm", f"ARC.Node_{version}_x64-setup.exe", f"ARC.Node_{version}_x64-setup.exe.sig",
        f"ARC.Node_{version}_x64_en-US.msi", f"ARC.Node_{version}_x64_en-US.msi.sig", f"ARC.Node_{version}_aarch64.dmg",
        f"ARC.Node_{version}_x64.dmg", "ARC.Node_aarch64.app.tar.gz", "ARC.Node_aarch64.app.tar.gz.sig",
        "ARC.Node_x64.app.tar.gz", "ARC.Node_x64.app.tar.gz.sig",
        f"ARC.Node_{version}_aarch64.AppImage", f"ARC.Node_{version}_arm64-setup.exe",  # only so every updater target of a runner resolves
    ]


def fake_bytes(tag: str, name: str) -> bytes:
    return (f"fake {tag} {name}\n").encode("utf-8") * 40


def latest_json_for(version: str, tag: str, base: str) -> bytes:
    def url(name: str) -> str:
        return f"{base}/{REPO}/releases/download/{tag}/{name}"

    document = {
        "version": version,
        "notes": f"fake desktop release {version}",
        "pub_date": "2026-10-08T05:34:45Z",
        "platforms": {
            "darwin-aarch64": {"signature": FAKE_SIGNATURE, "url": url("ARC.Node_aarch64.app.tar.gz")},
            "darwin-x86_64": {"signature": FAKE_SIGNATURE, "url": url("ARC.Node_x64.app.tar.gz")},
            "linux-x86_64": {"signature": FAKE_SIGNATURE, "url": url(f"ARC.Node_{version}_amd64.AppImage")},
            "windows-x86_64": {"signature": FAKE_SIGNATURE, "url": url(f"ARC.Node_{version}_x64-setup.exe")},
            "linux-aarch64": {"signature": FAKE_SIGNATURE, "url": url(f"ARC.Node_{version}_aarch64.AppImage")},
            "windows-aarch64": {"signature": FAKE_SIGNATURE, "url": url(f"ARC.Node_{version}_arm64-setup.exe")},
        },
    }
    return (json.dumps(document, indent=2) + "\n").encode("utf-8")


class ReleaseModel:
    """tag -> asset name -> bytes (latest.json is generated per request because it embeds the server address)."""

    def __init__(self, latest_tag: str, releases: Dict[str, Dict[str, bytes]], latest_json: Dict[str, Tuple[str, str]]):
        self.latest_tag = latest_tag
        self.releases = releases
        self.latest_json = latest_json  # tag -> (version served as latest.json, tag whose bundles its platform URLs name)

    def asset(self, tag: str, name: str, base: str) -> Optional[bytes]:
        if name == "latest.json" and tag in self.latest_json:
            version, assets_tag = self.latest_json[tag]
            return latest_json_for(version, assets_tag, base)
        return self.releases.get(tag, {}).get(name)

    def api_latest(self, base: str) -> bytes:
        tag = self.latest_tag
        names = list(self.releases.get(tag, {}))
        if tag in self.latest_json:
            names.append("latest.json")
        document = {
            "tag_name": tag, "name": tag, "draft": False, "prerelease": False, "immutable": True,
            "html_url": f"{base}/{REPO}/releases/tag/{tag}",
            "assets": [
                {"name": name, "size": len(self.asset(tag, name, base) or b""), "browser_download_url": f"{base}/{REPO}/releases/download/{tag}/{name}"}
                for name in sorted(names)
            ],
        }
        return (json.dumps(document, indent=2) + "\n").encode("utf-8")


def main_model(latest_has_latest_json: bool = False) -> ReleaseModel:
    launchers = {name: fake_bytes(LATEST_TAG, name) for name in LAUNCHER_ASSETS}
    launchers["SHA256SUMS"] = b"fake sums\n"
    releases = {
        LATEST_TAG: launchers,
        "v0.7.11": {name: fake_bytes("v0.7.11", name) for name in bundle_names("0.7.11")},
        "v0.8.11": {name: fake_bytes("v0.8.11", name) for name in bundle_names("0.8.11")},
    }
    overrides = {"v0.7.11": ("0.7.11", "v0.7.11"), "v0.8.11": ("0.8.11", "v0.8.11")}
    if latest_has_latest_json:
        overrides[LATEST_TAG] = ("0.8.11", "v0.8.11")  # mutation: Latest republishes the v0.8.11 latest.json, as a careless bridge release would
    return ReleaseModel(LATEST_TAG, releases, overrides)


def control_model(carries_latest_json: bool = True) -> ReleaseModel:
    """The bait scenario: Latest IS a v0.8.11-style desktop release with latest.json and bundles."""
    if carries_latest_json:
        releases = {CONTROL_TAG: {name: fake_bytes(CONTROL_TAG, name) for name in bundle_names("0.8.11")}}
        return ReleaseModel(CONTROL_TAG, releases, {CONTROL_TAG: ("0.8.11", CONTROL_TAG)})
    return main_model(False)


class _LoopbackServer(http.server.ThreadingHTTPServer):
    def server_bind(self) -> None:
        socketserver.TCPServer.server_bind(self)  # HTTPServer.server_bind would call socket.getfqdn(): a DNS lookup we do not want
        self.server_name, self.server_port = self.server_address[0], self.server_address[1]


class FakeGitHub:
    """A local HTTP server playing BOTH api.github.com and github.com and recording every request."""

    def __init__(self, model: ReleaseModel):
        self.model = model
        self.requests: List[dict] = []
        self.current_step = "setup"
        self._lock = threading.Lock()
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            server_version = "FakeGitHub/1"

            def log_message(self, *args: Any) -> None:  # silence
                pass

            def do_GET(self) -> None:
                self._serve(False)

            def do_HEAD(self) -> None:
                self._serve(True)

            def do_POST(self) -> None:
                self._serve(False, method_not_allowed=True)

            do_PUT = do_DELETE = do_PATCH = do_POST

            def _serve(self, head_only: bool, method_not_allowed: bool = False) -> None:
                parsed = urllib.parse.urlsplit(self.path)
                path = urllib.parse.unquote(parsed.path)
                status, headers, body, note = (405, {}, b"Method Not Allowed", "method not allowed") if method_not_allowed else outer.route(path)
                outer.record(self.command, parsed.path, parsed.query, self.headers.get("Host"), self.headers.get("User-Agent"), self.headers.get("Accept"), status, 0 if head_only else len(body), note)
                self.send_response(status)
                for key, value in headers.items():
                    self.send_header(key, value)
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Connection", "close")
                self.end_headers()
                if not head_only:
                    self.wfile.write(body)

        self.httpd = _LoopbackServer(("127.0.0.1", 0), Handler)
        self.httpd.daemon_threads = True
        self.base = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        self.netloc = f"127.0.0.1:{self.httpd.server_address[1]}"
        self.thread = threading.Thread(target=self.httpd.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)  # shutdown() waits one poll interval

    def start(self) -> "FakeGitHub":
        self.thread.start()
        return self

    def stop(self) -> None:
        self.httpd.shutdown()
        self.httpd.server_close()
        self.thread.join(timeout=10)

    def route(self, path: str) -> Tuple[int, Dict[str, str], bytes, str]:
        if path == f"/repos/{REPO}/releases/latest":
            return 200, {"Content-Type": "application/json"}, self.model.api_latest(self.base), f"api: latest release {self.model.latest_tag}"
        match = re.fullmatch(rf"/{re.escape(REPO)}/releases/latest/download/(.+)", path)
        if match:
            target = f"{self.base}/{REPO}/releases/download/{self.model.latest_tag}/{match.group(1)}"
            return 302, {"Location": target}, b"", f"redirect to the Latest tag {self.model.latest_tag}"
        match = re.fullmatch(rf"/{re.escape(REPO)}/releases/download/([^/]+)/(.+)", path)
        if match:
            body = self.model.asset(match.group(1), match.group(2), self.base)
            if body is None:
                return 404, {"Content-Type": "text/plain"}, b"Not Found", f"no asset {match.group(2)} in {match.group(1)}"
            return 200, {"Content-Type": "application/octet-stream"}, body, f"asset {match.group(2)} of {match.group(1)}"
        return 404, {"Content-Type": "text/plain"}, b"Not Found", "unknown path"

    def record(self, method: str, path: str, query: str, host: Optional[str], user_agent: Optional[str], accept: Optional[str], status: int, size: int, note: str) -> None:
        """host_role: which GitHub host the client addressed. The replicas send the Host header of the URL in the app's code
        (api.github.com / github.com) to the fake server; a client that talks to the fake directly (the native checker) is classified by path."""
        name = (host or "").split(":")[0].lower()
        role = "api" if name == "api.github.com" else "github" if name == "github.com" else ("api" if path.startswith("/repos/") else "github")
        if host == self.netloc:
            host = "<fake-github>"  # a client that talked to the fake directly; the port is random and would make logs differ per run
        with self._lock:
            self.requests.append({
                "seq": len(self.requests) + 1, "step": self.current_step, "method": method, "path": path, "query": query,
                "host": host, "host_role": role,
                "user_agent": user_agent, "accept": accept, "status": status, "bytes": size, "note": note,
            })


# --------------------------------------------------------------------------------------------------------------
# client replicas (the Rust and TypeScript constructs, cited)
# --------------------------------------------------------------------------------------------------------------

class NetworkError(RuntimeError):
    pass


class UnexpectedHost(RuntimeError):
    pass


class CommandError(RuntimeError):
    pass


class TauriError(RuntimeError):
    def __init__(self, kind: str, message: str):
        super().__init__(message)
        self.kind = kind


def rewrite_url(url: str, github_base: str, api_base: str) -> str:
    """Replace ONLY scheme and host of the two GitHub hosts by the fake server; any other host is refused."""
    parts = urllib.parse.urlsplit(url)
    host = (parts.hostname or "").lower()
    if parts.netloc in (urllib.parse.urlsplit(github_base).netloc, urllib.parse.urlsplit(api_base).netloc):
        return url  # already the fake server (a latest.json the fake server generated points at itself)
    if host == "github.com":
        base = urllib.parse.urlsplit(github_base)
    elif host == "api.github.com":
        base = urllib.parse.urlsplit(api_base)
    else:
        raise UnexpectedHost(f"the replica refuses to contact {host!r} ({url})")
    return urllib.parse.urlunsplit((base.scheme, base.netloc, parts.path, parts.query, parts.fragment))


_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def intended_host(url: str) -> Optional[str]:
    """The GitHub host named by the URL in the app's code (None for anything else, e.g. the fake server's own address)."""
    parts = urllib.parse.urlsplit(url)
    return parts.netloc if (parts.hostname or "").lower() in ("github.com", "api.github.com") else None


def http_fetch(url: str, user_agent: str, accept: str, timeout: float, host: Optional[str] = None) -> Tuple[int, bytes]:
    """GET following redirects (up to 10, like reqwest's default); returns the FINAL status and body.
    `host` is sent as the Host header (the fake server plays whichever GitHub host the app's URL names).
    The app's own timeouts (8 s, 600 s) are capped at 60 s here: the server is local, a hang would be a harness bug."""
    headers = {"User-Agent": user_agent, "Accept": accept}
    if host:
        headers["Host"] = host
    request = urllib.request.Request(url, headers=headers)
    try:
        with _OPENER.open(request, timeout=min(timeout, 60.0)) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()
    except (urllib.error.URLError, OSError) as error:
        raise NetworkError(str(error)) from None


def semver_key(version: str) -> Tuple[int, int, int, int, str]:
    match = re.fullmatch(r"v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+.*)?", version.strip())
    if not match:
        raise ValueError(f"not a semver version: {version!r}")
    major, minor, patch, pre = int(match.group(1)), int(match.group(2)), int(match.group(3)), match.group(4)
    return major, minor, patch, 0 if pre else 1, pre or ""


def trim_start_matches_v(tag: str) -> str:
    """Rust `str::trim_start_matches('v')` strips EVERY leading 'v'."""
    return tag.lstrip("v")


def replica_check_for_update(api_base: str, desktop_version: str = DESKTOP_VERSION, api_url: str = API_LATEST) -> dict:
    """commands.rs:934-957 (reqwest, user agent arc-desktop/0.1, 8 s timeout, default Accept */*, no status check)."""
    url = rewrite_url(api_url, api_base, api_base)
    status, body = http_fetch(url, APP_UA, "*/*", 8, host=intended_host(api_url))
    try:
        document = json.loads(body.decode("utf-8"))
    except ValueError as error:
        raise CommandError(f"error decoding response body: {error}") from None
    tag = document.get("tag_name") if isinstance(document, dict) else None
    version = trim_start_matches_v(tag) if isinstance(tag, str) else "unknown"
    return {"has_update": version != desktop_version and version != "unknown", "version": version, "http_status": status}


def updater_targets() -> Tuple[str, str]:
    """tauri-plugin-updater updater_os()/updater_arch() (updater.rs:1324-1350)."""
    if sys.platform.startswith("linux"):
        os_name = "linux"
    elif sys.platform == "darwin":
        os_name = "darwin"
    elif os.name == "nt":
        os_name = "windows"
    else:
        raise TauriError("UnsupportedOs", "Unsupported operating system")
    machine = platform.machine().lower()
    arch = {"x86_64": "x86_64", "amd64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64", "i686": "i686", "x86": "i686"}.get(machine)
    if arch is None:
        raise TauriError("UnsupportedArch", "Unsupported application architecture")
    return os_name, arch


def replica_tauri_check(
    endpoints: List[str],
    github_base: str,
    api_base: str,
    current_version: str,
    plugin_ua: str,
    blocked: Optional[List[str]] = None,
) -> dict:
    """tauri-plugin-updater 2.10.1 Updater::check (src/updater.rs:386-560), same decisions in the same order.

    Result keys mirror the native checker's JSON line: outcome, error_kind, error, update, download_attempted."""
    os_name, arch = updater_targets()
    release: Optional[dict] = None
    last_error: Optional[TauriError] = None
    for template in endpoints:
        url = (template.replace("{{current_version}}", urllib.parse.quote(current_version)).replace("{{target}}", os_name)
               .replace("{{arch}}", arch).replace("{{bundle_type}}", "unknown"))
        try:
            real = rewrite_url(url, github_base, api_base)
        except UnexpectedHost as error:
            if blocked is not None:
                blocked.append(str(error))
            last_error = TauriError("Network", f"blocked by the harness: {error}")
            continue
        try:
            status, body = http_fetch(real, plugin_ua, "application/json", 30, host=intended_host(url))
        except NetworkError as error:
            last_error = TauriError("Network", str(error))
            continue
        if 200 <= status < 300:
            if status == 204:
                return {"outcome": "no_update", "error_kind": None, "error": None, "update": None, "download_attempted": False}
            try:
                document = json.loads(body.decode("utf-8"))  # `res.json().await?`: a non-JSON 2xx body returns the error at once
            except ValueError as error:
                raise_error = TauriError("Reqwest", f"error decoding response body: {error}")
                return {"outcome": "error", "error_kind": raise_error.kind, "error": str(raise_error), "update": None, "download_attempted": False}
            try:
                release = parse_remote_release(document)
                last_error = None
                break
            except TauriError as error:
                last_error = error  # deserialisation failure: recorded, loop continues
        # non-success: only logged, no last_error, the loop moves on
    if last_error is not None:
        return {"outcome": "error", "error_kind": last_error.kind, "error": str(last_error), "update": None, "download_attempted": False}
    if release is None:
        return {"outcome": "error", "error_kind": "ReleaseNotFound", "error": RELEASE_NOT_FOUND, "update": None, "download_attempted": False}
    targets = [f"{os_name}-{arch}"]
    entry = None
    if release["platforms"] is not None:
        for target in targets:
            if target in release["platforms"]:
                entry = release["platforms"][target]
                break
        if entry is None:
            names = ", ".join(f'"{t}"' for t in targets)
            return {"outcome": "error", "error_kind": "TargetsNotFound", "error": f"None of the fallback platforms `[{names}]` were found in the response `platforms` object", "update": None, "download_attempted": False}
    else:
        entry = {"url": release["url"], "signature": release["signature"]}
    if semver_key(release["version"]) > semver_key(current_version):
        return {"outcome": "update_available", "error_kind": None, "error": None, "update": {"version": release["version"], "download_url": entry["url"]}, "download_attempted": False}
    return {"outcome": "no_update", "error_kind": None, "error": None, "update": None, "download_attempted": False}


def parse_remote_release(document: Any) -> dict:
    """RemoteRelease deserialisation (updater.rs:1383-1429): `version` (alias `name`, leading v trimmed) + platforms OR url+signature."""
    if not isinstance(document, dict):
        raise TauriError("Json", "invalid type: the update response must be a JSON object")
    raw = document.get("version", document.get("name"))
    if not isinstance(raw, str):
        raise TauriError("Json", "missing field `version`")
    version = raw.lstrip("v")
    try:
        semver_key(version)
    except ValueError as error:
        raise TauriError("Semver", str(error)) from None
    platforms = document.get("platforms")
    if platforms is not None:
        if not isinstance(platforms, dict):
            raise TauriError("Json", "invalid type for `platforms`")
        return {"version": version, "platforms": platforms, "url": None, "signature": None}
    if not isinstance(document.get("url"), str) or not isinstance(document.get("signature"), str):
        raise TauriError("Json", "the `url` field was not set on the updater response")
    return {"version": version, "platforms": None, "url": document["url"], "signature": document["signature"]}


def replica_download_and_install(update: dict, github_base: str, api_base: str, plugin_ua: str) -> dict:
    """Update::download (updater.rs:~650-700): GET the bundle with Accept application/octet-stream. The signature check
    that follows in the real plugin is NOT performed (the harness bundles carry a fake signature): only the FETCH matters."""
    target = rewrite_url(update["download_url"], github_base, api_base)
    status, body = http_fetch(target, plugin_ua, "application/octet-stream", 600, host=intended_host(update["download_url"]))
    return {"download_attempted": True, "status": status, "bytes": len(body), "signature_verification": "not performed by the harness"}


def replica_install_click(
    endpoints: List[str],
    github_base: str,
    api_base: str,
    current_version: str,
    plugin_ua: str,
    blocked: Optional[List[str]] = None,
) -> dict:
    """Settings.tsx:35-52. Plugin errors reach JS as strings (error.rs Serialize => serialize_str), so the catch shows String(e)."""
    outcome = replica_tauri_check(endpoints, github_base, api_base, current_version, plugin_ua, blocked)
    result = {"check": outcome, "install_error": None, "downloaded": False, "relaunched": False}
    if outcome["outcome"] == "error":
        result["install_error"] = outcome["error"]
    elif outcome["outcome"] == "no_update":
        result["install_error"] = "No update available."
    else:
        download = replica_download_and_install(outcome["update"], github_base, api_base, plugin_ua)
        result["downloaded"] = True
        result["download"] = download
    result["ui_error_text"] = f"Update failed: {result['install_error']}" if result["install_error"] else None
    return result


def platform_asset(asset_map: Dict[str, str]) -> str:
    """commands.rs:1120-1128 platform_release_asset()."""
    machine = platform.machine().lower()
    arch = "aarch64" if machine in ("arm64", "aarch64") else "x86_64" if machine in ("x86_64", "amd64") else machine
    os_name = "macos" if sys.platform == "darwin" else "windows" if os.name == "nt" else "linux" if sys.platform.startswith("linux") else sys.platform
    asset = asset_map.get(f"{os_name}-{arch}")
    if asset is None:
        raise CommandError(f"no prebuilt arc-node binary for platform {os_name}-{arch}")
    return asset


def replica_ensure_binary(home: Path, github_base: str, template: str, asset: str, windows: Optional[bool] = None) -> dict:
    """commands.rs:975-1075 for a fresh install (no managed binary yet): GET releases/latest/download/<asset> (user agent
    arc-desktop/0.1, 600 s), error unless success, write arc-node.download, rename over ~/.arc/bin/arc-node, chmod 0755."""
    is_windows = os.name == "nt" if windows is None else windows
    target = home / ".arc" / "bin" / ("arc-node.exe" if is_windows else "arc-node")
    app_url = template.format(asset)
    url = rewrite_url(app_url, github_base, github_base)
    status, body = http_fetch(url, APP_UA, "*/*", 600, host=intended_host(app_url))
    if not 200 <= status < 300:
        raise CommandError(f"release asset {asset} returned HTTP {status}")
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = target.with_suffix(".download")
    temporary.write_bytes(body)
    for attempt in range(20):
        try:
            os.replace(temporary, target)
            break
        except PermissionError:
            if attempt == 19:
                raise
            threading.Event().wait(0.5)
    if not is_windows:
        os.chmod(target, 0o755)
    return {"already_installed": False, "downloaded_bytes": len(body), "url": url, "target": str(target.relative_to(home)).replace("\\", "/")}


# --------------------------------------------------------------------------------------------------------------
# tree snapshots and the native checker
# --------------------------------------------------------------------------------------------------------------

def snapshot_tree(root: Path) -> Dict[str, dict]:
    entries: Dict[str, dict] = {}
    for current, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in sorted(dirnames) + sorted(filenames):
            path = Path(current) / name
            relative = path.relative_to(root).as_posix()
            if path.is_dir():
                entries[relative] = {"type": "dir"}
            else:
                data = path.read_bytes()
                entries[relative] = {"type": "file", "size": len(data), "sha256": sha256_bytes(data)}
    return dict(sorted(entries.items()))


PROXY_VARIABLES = ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY")
SANDBOX_VARIABLES = {
    "HOME": "home", "USERPROFILE": "home", "APPDATA": "appdata", "LOCALAPPDATA": "localappdata",
    "XDG_CACHE_HOME": "xdg-cache", "XDG_CONFIG_HOME": "xdg-config", "XDG_DATA_HOME": "xdg-data", "XDG_STATE_HOME": "xdg-state",
    "TMPDIR": "tmp", "TEMP": "tmp", "TMP": "tmp",
}


def sandbox_environment(sandbox: Path) -> Dict[str, str]:
    """Environment of the native checker: home, app-data, XDG and temp directories all live inside `sandbox`, so any file the
    process writes (an update cache, a download, a log) shows up in the sandbox snapshot. Proxies are switched off."""
    env = {key: value for key, value in os.environ.items() if key.upper() not in PROXY_VARIABLES}
    for variable, folder in SANDBOX_VARIABLES.items():
        (sandbox / folder).mkdir(parents=True, exist_ok=True)
        env[variable] = str(sandbox / folder)
    (sandbox / "cwd").mkdir(parents=True, exist_ok=True)
    env["NO_PROXY"] = "127.0.0.1,localhost"
    if os.name != "nt":
        env["no_proxy"] = env["NO_PROXY"]
    return env


def changed_paths(before: Dict[str, dict], after: Dict[str, dict]) -> List[str]:
    return sorted(path for path in after if before.get(path) != after[path]) + sorted("(removed) " + path for path in before if path not in after)


def watch_tokens(endpoint: str) -> List[str]:
    """What a cache of the update check would have to mention: the repository, the release tags, the endpoint's file or its address."""
    netloc = urllib.parse.urlsplit(endpoint).netloc
    return [token.lower() for token in ("FerrumVir", "arc-chain", "latest.json", "releases/latest", "releases/download", "0.8.11", "v0.7.12", netloc) if token]


def suspicious_files(sandbox: Path, delta: List[str], tokens: List[str]) -> List[str]:
    """New or changed FILES of the sandbox whose path or content (first 2 MB) mentions one of the tokens. Files that mention none
    of them (interpreter bytecode caches, OS bookkeeping) are reported in the delta but are not update data."""
    found: List[str] = []
    for relative in delta:
        if relative.startswith("(removed) "):
            continue
        path = sandbox / relative
        if not path.is_file():
            continue
        if any(token in relative.lower() for token in tokens):
            found.append(relative)
            continue
        try:
            with open(str(path), "rb") as handle:
                blob = handle.read(2_000_000).lower()
        except OSError:
            continue
        if any(token.encode("utf-8") in blob for token in tokens):
            found.append(relative)
    return found


def run_native(
    command: List[str],
    endpoint: str,
    pubkey: str,
    current_version: str,
    control_download: bool = False,
    timeout: float = 180.0,
    sandbox: Optional[Path] = None,
) -> dict:
    """Invoke the real tauri-plugin-updater checker and parse ONE JSON line (schema SCHEMA_NATIVE) from its stdout.

    With a `sandbox` directory the process runs inside it (see sandbox_environment). Afterwards `sandbox_delta` lists the paths
    that are new or changed (first 40, `sandbox_delta_total` is the count) and `sandbox_suspicious` the new or changed FILES that
    mention the endpoint, the repository or the release tags (watch_tokens): an update cache would have to."""
    command = list(command)
    if os.path.dirname(command[0]) and not os.path.isabs(command[0]):
        command[0] = os.path.abspath(command[0])  # the child runs with another working directory
    argv = command + ["--endpoint", endpoint, "--current-version", current_version, "--pubkey", pubkey, "--insecure-transport"]
    if control_download:
        argv.append("--control-download")
    env = sandbox_environment(sandbox) if sandbox is not None else None
    before = snapshot_tree(sandbox) if sandbox is not None else {}

    tokens = watch_tokens(endpoint)

    def finish(entry: dict) -> dict:
        if sandbox is not None:
            delta = changed_paths(before, snapshot_tree(sandbox))
            entry["sandbox_delta"] = delta[:40]
            entry["sandbox_delta_total"] = len(delta)
            entry["sandbox_suspicious"] = suspicious_files(sandbox, delta, tokens)
        return entry

    try:
        done = subprocess.run(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout, check=False, env=env, cwd=str(sandbox / "cwd") if sandbox is not None else None)
    except (OSError, subprocess.TimeoutExpired) as error:
        return finish({"ok": False, "problem": f"could not run the native checker: {type(error).__name__}: {error}", "exit_code": None})
    lines = [line.strip() for line in done.stdout.decode("utf-8", "replace").splitlines() if line.strip().startswith("{")]
    if not lines:
        return finish({"ok": False, "problem": f"no JSON line on stdout; stderr tail: {done.stderr.decode('utf-8', 'replace')[-300:]}", "exit_code": done.returncode})
    try:
        document = json.loads(lines[-1])
    except ValueError as error:
        return finish({"ok": False, "problem": f"the last stdout JSON line is invalid: {error}", "exit_code": done.returncode})
    required = ("schema", "plugin", "tauri", "current_version", "endpoints", "outcome", "error_kind", "error", "update", "download_attempted")
    missing = [key for key in required if key not in document]
    if document.get("schema") != SCHEMA_NATIVE or missing:
        return finish({"ok": False, "problem": f"schema {document.get('schema')!r}, missing keys {missing}", "exit_code": done.returncode})
    if document.get("outcome") not in ("no_update", "update_available", "error"):
        return finish({"ok": False, "problem": f"outcome {document.get('outcome')!r} is not one of no_update, update_available, error", "exit_code": done.returncode})
    return finish({"ok": True, "result": document, "exit_code": done.returncode})


# --------------------------------------------------------------------------------------------------------------
# the run
# --------------------------------------------------------------------------------------------------------------

FORBIDDEN_PATH_PATTERNS = [
    (re.compile(rf"^/{re.escape(REPO)}/releases/download/v0\.8\."), "a v0.8 release asset"),
    (re.compile(rf"^/{re.escape(REPO)}/releases/download/v0\.7\.11/"), "a v0.7.11 release asset"),
    (re.compile(r"(\.AppImage|\.deb|\.rpm|\.dmg|\.msi|-setup\.exe|\.app\.tar\.gz)$"), "an app bundle"),
    (re.compile(r"\.sig$"), "a signature file"),
]


def forbidden_requests(requests: List[dict]) -> List[dict]:
    found = []
    for request in requests:
        reasons = [label for pattern, label in FORBIDDEN_PATH_PATTERNS if pattern.search(request["path"])]
        if request["path"].endswith("/latest.json") and request["status"] == 200:
            reasons.append("a served latest.json")
        if reasons:
            found.append({"seq": request["seq"], "path": request["path"], "status": request["status"], "why": reasons})
    return found


def _assertion(assertion_id: str, title: str, result: str, detail: str) -> dict:
    assert result in ("PASS", "FAIL", "SKIP")
    return {"id": assertion_id, "title": title, "result": result, "detail": detail}


def execute(
    repo: Path,
    evidence: Path,
    label: str,
    tag: str = TAG,
    plugin_crate: Optional[Path] = None,
    plugin_offline: bool = False,
    native_check: Optional[List[str]] = None,
    live_observation: Optional[Path] = None,
    released_binary: Optional[Path] = None,
    mutate: Optional[dict] = None,
    source: Optional[dict] = None,
    home_parent: Optional[Path] = None,
) -> dict:
    """Run everything and return the result dict (also written under `evidence`). `mutate` is for tests only."""
    mutate = mutate or {}
    evidence = Path(evidence)
    evidence.mkdir(parents=True, exist_ok=True)
    source = source if source is not None else check_source(repo, tag, plugin_crate, plugin_offline, released_binary)
    extracted = source["extracted"]
    endpoints = list(extracted["endpoints"])
    pins = PINNED["plugin_crate"]
    plugin_ua = f"{pins['name']}/{pins['version']}"
    asset = platform_asset(extracted["platform_assets"])
    home = Path(tempfile.mkdtemp(prefix="wave0-desktop-home-", dir=str(home_parent) if home_parent else None))
    native_root = Path(tempfile.mkdtemp(prefix="wave0-native-sandbox-", dir=str(home_parent) if home_parent else None))
    endpoint_path = urllib.parse.urlsplit(endpoints[0]).path
    blocked: List[str] = []

    main = FakeGitHub(main_model(bool(mutate.get("latest_has_latest_json")))).start()
    results: Dict[str, Any] = {}
    snapshots: Dict[str, Dict[str, dict]] = {}
    native_main: Optional[dict] = None
    try:
        snapshots["before"] = snapshot_tree(home)
        main.current_step = "settings_check"
        results["check_for_update"] = replica_check_for_update(main.base, DESKTOP_VERSION, extracted["api_url"])
        main.current_step = "tauri_check"
        results["tauri_check"] = replica_tauri_check(endpoints, main.base, main.base, DESKTOP_VERSION, plugin_ua, blocked)
        main.current_step = "install_click"
        results["install_click"] = replica_install_click(endpoints, main.base, main.base, DESKTOP_VERSION, plugin_ua, blocked)
        for path in mutate.get("extra_requests", []):
            main.current_step = "injected"
            http_fetch(main.base + path, APP_UA, "*/*", 10)
        if mutate.get("cache_file"):
            target = home / mutate["cache_file"]
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(b"cached latest.json\n")
        if native_check:
            main.current_step = "native_check"
            native_main = run_native(native_check, main.base + endpoint_path, extracted["pubkey"], DESKTOP_VERSION, sandbox=native_root / "main")
        snapshots["after_check"] = snapshot_tree(home)
        main.current_step = "sidecar_refresh"
        results["ensure_binary"] = replica_ensure_binary(home, main.base, extracted["sidecar_template"], asset)
        snapshots["after"] = snapshot_tree(home)
    finally:
        main.stop()

    control = FakeGitHub(control_model(mutate.get("control_carries_latest_json", True))).start()
    native_control: Optional[dict] = None
    native_control_download: Optional[dict] = None
    try:
        control.current_step = "control_settings_check"
        results["control_check_for_update"] = replica_check_for_update(control.base, DESKTOP_VERSION, extracted["api_url"])
        control.current_step = "control_tauri_check"
        results["control_tauri_check"] = replica_tauri_check(endpoints, control.base, control.base, DESKTOP_VERSION, plugin_ua)
        control.current_step = "control_install_click"
        results["control_install_click"] = replica_install_click(endpoints, control.base, control.base, DESKTOP_VERSION, plugin_ua)
        if native_check:
            control.current_step = "native_control"
            native_control = run_native(native_check, control.base + endpoint_path, extracted["pubkey"], DESKTOP_VERSION, sandbox=native_root / "control")
            control.current_step = "native_control_download"
            native_control_download = run_native(native_check, control.base + endpoint_path, extracted["pubkey"], DESKTOP_VERSION, control_download=True, sandbox=native_root / "control-download")
    finally:
        control.stop()

    assertions = evaluate_assertions(
        main.requests, control.requests, results, snapshots, source, asset, plugin_ua, native_check is not None,
        native_main, native_control, native_control_download, blocked,
    )
    verdict = VERDICT_ISOLATED if all(a["result"] != "FAIL" for a in assertions) else VERDICT_NOT_ISOLATED

    update = results["check_for_update"]
    available = extracted["settings_templates"]["available"].replace("${update.version}", update["version"])
    button = extracted["settings_templates"]["install_button"].replace("${update.version}", update["version"])
    page_text = available if update["has_update"] else "You're running the latest version."
    click = results["install_click"]
    if click["downloaded"]:
        reading = "Reading: the install click found an update and FETCHED a bundle. This is the failure the harness exists to catch."
    elif click["ui_error_text"]:
        reading = ("Reading: the page claims an update because check_for_update only compares tags; the install click asks the Tauri updater, "
                   "which found no usable release JSON and shows the error above. Nothing is downloaded.")
    else:
        reading = "Reading: the install click finished without an error and without a download."
    ui_lines = [
        'Settings > Updates after "Check for updates" (hasUpdate=%s): %s' % (str(update["has_update"]).lower(), page_text),
        "Install button shown: %s" % (button if update["has_update"] else "(none)"),
        "After clicking the install button: %s" % (click["ui_error_text"] or "(no error shown)"),
        reading,
    ]
    ui_text = "\n".join(ui_lines) + "\n"

    live = {"supplied": False, "note": "not supplied"}
    if live_observation is not None:
        try:
            raw = Path(live_observation).read_bytes()
        except OSError as error:
            live = {"supplied": False, "note": f"NOT EMBEDDED: could not read {Path(live_observation).name} ({type(error).__name__})"}
        else:
            try:
                live = {"supplied": True, "sha256": sha256_bytes(raw), "content": json.loads(raw.decode("utf-8"))}
            except ValueError as error:
                live = {"supplied": True, "sha256": sha256_bytes(raw), "note": f"unparseable: {error}"}

    addresses = {main.base: "<fake-github>", control.base: "<fake-github>"}
    foreign = foreign_schema(evidence / "result.json")
    result = {
        "schema": SCHEMA_RESULT,
        "label": label,
        "python": sys.version.split()[0],
        "platform": platform.platform(),
        "tag": tag,
        "asset": asset,
        "verdict": verdict,
        "assertions": assertions,
        "request_count": len(main.requests),
        "control_request_count": len(control.requests),
        "ui_text": ui_text,
        "replica_results": scrub(results, addresses),
        "native_check": scrub({
            "supplied": native_check is not None,
            "main": native_main if native_main is not None else "not supplied",
            "control": native_control if native_control is not None else "not supplied",
            "control_download": native_control_download if native_control_download is not None else "not supplied",
        }, addresses),
        "plugin_semantics": {k: source["plugin_semantics"].get(k) for k in ("status", "reason", "crate", "crate_sha256_observed", "loud_note", "lines", "release_not_found_text")},
        "released_binary": {k: source["released_binary"].get(k) for k in ("binary_sha256", "crates_from_binary_strings", "verified_locally")},
        "live_observation": live,
        "sources": source["sources"],
        "blocked_hosts": blocked,
        "notes": [
            "AUTHOR'S OBSERVATION of the real github.com on 2026-10-08 (curl, read-only; not made by this run): GET releases/latest/download/<asset the Latest release lacks> answers 302 to releases/download/<LatestTag>/<asset>, which answers 404. The fake server reproduces that chain.",
            "NOT MODELLED: for an asset that exists, the real github.com adds one more 302 to a signed CDN URL; the fake serves the asset at the first release URL. That hop cannot add a request the first one did not already select.",
            *([f"result.json in this directory carries schema {foreign} and was left untouched; this result is in result-{label}.json"] if foreign else []),
            "REPLICA vs NATIVE: the replicas re-implement the cited Rust/TypeScript decisions in Python with the pinned plugin's request headers; I8/I9 run the real tauri-plugin-updater when a native checker binary is supplied.",
            "The harness never verifies minisign signatures and never installs anything: the control stops at the bundle FETCH, which is what must never happen on the main scenario.",
        ],
    }
    if not foreign:
        write_text(evidence / "result.json", dump(result))
    write_text(evidence / f"result-{label}.json", dump(result))
    write_jsonl(evidence / "requests.jsonl", main.requests)
    write_jsonl(evidence / "control-requests.jsonl", control.requests)
    write_text(evidence / "home-before.json", dump(snapshots["before"]))
    write_text(evidence / "home-after-check.json", dump(snapshots["after_check"]))
    write_text(evidence / "home-after.json", dump(snapshots["after"]))
    write_text(evidence / "ui-text.txt", ui_text)
    write_text(evidence / "check-source.json", dump(source))
    for temporary in (home, native_root):  # the program's own temp trees (their contents are in the snapshots above)
        shutil.rmtree(str(temporary), ignore_errors=True)
    return result


SETTINGS_AVAILABLE = "is available. Click below to download, install, and relaunch."
SETTINGS_BUTTON = "Install v${update.version} & relaunch"


def foreign_schema(path: Path) -> Optional[str]:
    """The schema of an existing JSON file at `path` when it is NOT ours (e.g. an OS job's desktop-os-result.v1 result.json)."""
    try:
        document = json.loads(Path(path).read_bytes().decode("utf-8"))
    except (OSError, ValueError):
        return None
    schema = document.get("schema") if isinstance(document, dict) else None
    return schema if isinstance(schema, str) and schema != SCHEMA_RESULT else None


def scrub(value: Any, replacements: Dict[str, str]) -> Any:
    """Deep copy with the fake servers' (random-port) addresses replaced, so results compare across runs and operating systems."""
    if isinstance(value, str):
        for old, new in replacements.items():
            value = value.replace(old, new)
        return value
    if isinstance(value, list):
        return [scrub(item, replacements) for item in value]
    if isinstance(value, dict):
        return {key: scrub(item, replacements) for key, item in value.items()}
    return value


def settings_template(settings_text: str, needle: str) -> str:
    """The backtick template literal of Settings.tsx containing `needle`, without the backticks."""
    for match in re.finditer(r"`([^`]*)`", settings_text):
        if needle in match.group(1):
            return match.group(1)
    return needle


def evaluate_assertions(
    requests: List[dict],
    control_requests: List[dict],
    results: Dict[str, Any],
    snapshots: Dict[str, Dict[str, dict]],
    source: dict,
    asset: str,
    plugin_ua: str,
    native_supplied: bool,
    native_main: Optional[dict],
    native_control: Optional[dict],
    native_control_download: Optional[dict],
    blocked: List[str],
) -> List[dict]:
    out: List[dict] = []
    endpoint_path = f"/{REPO}/releases/latest/download/latest.json"
    target_path = f"/{REPO}/releases/download/{LATEST_TAG}/latest.json"

    def step(name: str) -> List[dict]:
        return [r for r in requests if r["step"] == name]

    def pairs(rows: List[dict]) -> List[Tuple[str, int]]:
        return [(r["path"], r["status"]) for r in rows]

    # I1
    tauri = results["tauri_check"]
    tauri_pairs = pairs(step("tauri_check"))
    ok1 = tauri_pairs == [(endpoint_path, 302), (target_path, 404)] and tauri["outcome"] == "error" and tauri["error_kind"] == "ReleaseNotFound" and tauri["update"] is None
    out.append(_assertion("I1", "the Tauri endpoint request ends in 404 after the redirect and check() yields no update", "PASS" if ok1 else "FAIL",
                          f"requests {tauri_pairs}; outcome {tauri['outcome']}/{tauri['error_kind']}; update {tauri['update']}"))
    # I2
    click = results["install_click"]
    click_rows = step("install_click")
    octet = [r for r in click_rows if r["accept"] == "application/octet-stream"]
    ok2 = bool(click["install_error"]) and click["ui_error_text"] == f"Update failed: {RELEASE_NOT_FOUND}" and not click["downloaded"] and not octet and pairs(click_rows) == [(endpoint_path, 302), (target_path, 404)]
    out.append(_assertion("I2", "the install flow shows an error and downloaded nothing", "PASS" if ok2 else "FAIL",
                          f"UI shows {click['ui_error_text']!r}; downloaded={click['downloaded']}; requests {pairs(click_rows)}"))
    # I3
    bad = forbidden_requests(requests)
    out.append(_assertion("I3", "no request to a v0.8 asset, a v0.7.11 asset, any app bundle or .sig, and no served latest.json", "PASS" if not bad else "FAIL",
                          "none of the %d requests is forbidden" % len(requests) if not bad else f"FORBIDDEN: {bad}"))
    # I4
    allowed = {
        ("GET", "api", f"/repos/{REPO}/releases/latest"), ("GET", "github", endpoint_path), ("GET", "github", target_path),
        ("GET", "github", f"/{REPO}/releases/latest/download/{asset}"), ("GET", "github", f"/{REPO}/releases/download/{LATEST_TAG}/{asset}"),
    }
    observed = {(r["method"], r["host_role"], r["path"]) for r in requests}
    ok4 = observed == allowed and not blocked
    out.append(_assertion("I4", "the observed set of (method, host, path) requests equals the allowlist exactly", "PASS" if ok4 else "FAIL",
                          f"{len(observed)} distinct paths; extra {sorted(observed - allowed)}; missing {sorted(allowed - observed)}; blocked hosts {blocked}"))
    # I5
    before, after_check, after = snapshots["before"], snapshots["after_check"], snapshots["after"]
    new = sorted(set(after) - set(after_check))
    expected_new = [".arc", ".arc/bin", ".arc/bin/arc-node" if ".arc/bin/arc-node" in after else ".arc/bin/arc-node.exe"]
    launcher = after.get(expected_new[-1], {})
    launcher_ok = launcher.get("sha256") == sha256_bytes(fake_bytes(LATEST_TAG, asset)) and not any(name.endswith(".download") for name in after)
    ok5 = before == after_check and new == expected_new and launcher_ok and all(after[key] == after_check[key] for key in after_check)
    out.append(_assertion("I5", "no update cache: the tree is identical after the check flows; the sidecar step adds only .arc/bin/arc-node", "PASS" if ok5 else "FAIL",
                          f"after the check flows: {sorted(set(after_check) - set(before)) or 'nothing new'}; sidecar step added {new}; launcher digest ok={launcher_ok}"))
    # I6 (control, excluded from I3/I4)
    ck = results["control_check_for_update"]
    ct = results["control_tauri_check"]
    ci = results["control_install_click"]
    bundle_gets = [r for r in control_requests if r["status"] == 200 and r["path"].startswith(f"/{REPO}/releases/download/{CONTROL_TAG}/") and not r["path"].endswith("latest.json")]
    ok6 = ck["has_update"] is True and ck["version"] == "0.8.11" and ct["outcome"] == "update_available" and ci["downloaded"] is True and bool(bundle_gets)
    out.append(_assertion("I6", "POSITIVE CONTROL: with a v0.8.11 latest.json as Latest the same replica finds the update and requests the bundle", "PASS" if ok6 else "FAIL",
                          f"check_for_update {ck['has_update']}/{ck['version']}; tauri check {ct['outcome']}; bundle GETs {[r['path'] for r in bundle_gets]}"))
    # I7
    items_ok = all(item["ok"] for item in source["items"])
    plugin = source["plugin_semantics"]
    released_problems = source["released_binary"].get("problems", [])
    ok7 = items_ok and plugin_acceptable(plugin) and not released_problems
    detail7 = f"{sum(1 for i in source['items'] if i['ok'])}/{len(source['items'])} check-source items verified; plugin semantics {plugin['status']} ({plugin['crate']})"
    if plugin["status"] == "unverified":
        detail7 += f"; {'NOTE' if plugin_acceptable(plugin) else 'FAILS I7'}: {plugin.get('loud_note')}"
    if plugin["status"] == "contradicted":
        detail7 += f"; LOUD: {plugin.get('loud_note')}"
    out.append(_assertion("I7", "every check-source item verified; the plugin semantics section confirmed or loudly noted", "PASS" if ok7 else "FAIL", detail7))
    # I8 / I9 (native)
    if not native_supplied:
        reason = "no --native-check binary supplied: the real tauri-plugin-updater was NOT exercised (replica evidence only)"
        out.append(_assertion("I8", "native tauri-plugin-updater check against Latest v0.7.12 without latest.json", "SKIP", reason))
        out.append(_assertion("I9", "native tauri-plugin-updater control against the v0.8.11 latest.json", "SKIP", reason))
        return out
    out.append(_native_main_assertion(native_main, step("native_check"), endpoint_path, target_path, plugin_ua))
    out.append(_native_control_assertion(native_control, native_control_download, control_requests))
    return out


def _native_pin_problems(document: dict) -> List[str]:
    problems = []
    if document.get("plugin") != f"tauri-plugin-updater {PINNED['plugin_crate']['version']}":
        problems.append(f"plugin {document.get('plugin')!r} is not the pinned tauri-plugin-updater {PINNED['plugin_crate']['version']}")
    if document.get("tauri") != PINNED["crates_from_binary_strings"]["tauri"][0]:
        problems.append(f"tauri {document.get('tauri')!r} is not the pinned {PINNED['crates_from_binary_strings']['tauri'][0]}")
    if document.get("current_version") != DESKTOP_VERSION:
        problems.append(f"current_version {document.get('current_version')!r}")
    return problems


def _native_main_assertion(native: Optional[dict], rows: List[dict], endpoint_path: str, target_path: str, plugin_ua: str) -> dict:
    title = "native tauri-plugin-updater check against Latest v0.7.12 without latest.json: no update, no download, endpoint + redirect only, no files left"
    if native is None or not native.get("ok"):
        return _assertion("I8", title, "FAIL", f"native checker problem: {(native or {}).get('problem')}")
    document = native["result"]
    seen = [(r["method"], r["path"], r["status"]) for r in rows]
    uas = sorted({r["user_agent"] for r in rows})
    problems = _native_pin_problems(document)
    if document["outcome"] == "error":
        outcome_ok = document["error_kind"] == "ReleaseNotFound" and document["error"] == RELEASE_NOT_FOUND  # the text the install button shows
    else:
        outcome_ok = document["outcome"] == "no_update"
    if not outcome_ok:
        problems.append(f"outcome {document['outcome']}/{document['error_kind']}")
    if document["download_attempted"] is not False or document["update"] is not None:
        problems.append("a download was attempted or an update was reported")
    if seen != [("GET", endpoint_path, 302), ("GET", target_path, 404)]:
        problems.append(f"the fake server saw {seen} from the native process")
    if uas != [plugin_ua]:
        problems.append(f"user agents {uas}, expected [{plugin_ua!r}]")
    suspicious = native.get("sandbox_suspicious")
    if suspicious is None:
        problems.append("the native run was not sandboxed (no sandbox_suspicious)")
    elif suspicious:
        problems.append(f"the native process left files mentioning the endpoint, repository or release tags (an update cache?) in its sandboxed home/app-data/XDG/temp/cwd: {suspicious}")
    total = native.get("sandbox_delta_total", 0)
    left = "none" if not total else f"{total} unrelated file/dir entries (none mention the endpoint, repository or release tags)"
    return _assertion("I8", title, "FAIL" if problems else "PASS", "; ".join(problems) if problems else f"{document['plugin']} / tauri {document['tauri']}: outcome {document['outcome']}/{document['error_kind']}; requests {seen}; sandboxed home/temp/cwd afterwards: {left}")


def _native_control_assertion(native: Optional[dict], native_download: Optional[dict], control_requests: List[dict]) -> dict:
    title = "native control: the real plugin sees the v0.8.11 latest.json update and, with --control-download, the bundle GET shows up"
    problems = []
    for name, entry in (("without --control-download", native), ("with --control-download", native_download)):
        if entry is None or not entry.get("ok"):
            problems.append(f"{name}: {(entry or {}).get('problem')}")
    if problems:
        return _assertion("I9", title, "FAIL", "; ".join(problems))
    plain, with_download = native["result"], native_download["result"]  # type: ignore[index]
    problems += _native_pin_problems(plain)
    if plain["outcome"] != "update_available" or not plain.get("update") or plain["update"].get("version") != "0.8.11":
        problems.append(f"without the flag: outcome {plain['outcome']} update {plain.get('update')}")
    if plain["download_attempted"] is not False:
        problems.append("a download happened without --control-download")
    if with_download["outcome"] != "update_available" or with_download["download_attempted"] is not True:
        problems.append(f"with the flag: outcome {with_download['outcome']} download_attempted {with_download['download_attempted']}")
    native_rows = [r for r in control_requests if r["step"] == "native_control_download" and r["status"] == 200 and r["path"].startswith(f"/{REPO}/releases/download/{CONTROL_TAG}/") and not r["path"].endswith("latest.json")]
    plain_bundle = [r for r in control_requests if r["step"] == "native_control" and not r["path"].endswith("latest.json") and r["path"].startswith(f"/{REPO}/releases/download/")]
    if not native_rows:
        problems.append("the control server never saw the native bundle GET")
    if plain_bundle:
        problems.append("the native check fetched a bundle without --control-download")
    return _assertion("I9", title, "FAIL" if problems else "PASS", "; ".join(problems) if problems else f"update {plain['update']}; bundle GET {[r['path'] for r in native_rows]}")


# --------------------------------------------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------------------------------------------

def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--repo", type=Path, default=Path("."))
    common.add_argument("--tag", default=TAG)
    group = common.add_mutually_exclusive_group()
    group.add_argument("--plugin-crate", type=Path, help="local tauri-plugin-updater-2.10.1.crate (otherwise fetched from static.crates.io)")
    group.add_argument("--plugin-offline", action="store_true", help="skip the plugin source reading (status unverified, said loudly)")
    common.add_argument("--released-binary", type=Path, help="local usr/bin/arc-desktop of the released .deb: re-verify the embedded facts (unit tests)")
    one = sub.add_parser("check-source", parents=[common])
    one.add_argument("--out", type=Path, required=True)
    two = sub.add_parser("run", parents=[common])
    two.add_argument("--evidence", type=Path, required=True)
    two.add_argument("--label", required=True, choices=("linux", "windows", "macos"))
    two.add_argument("--native-check", type=Path, help="the real tauri-plugin-updater checker (the binary built from wave0-lab-desktop/native-updater-check)")
    two.add_argument("--live-observation", type=Path, help="JSON with the flip-window surface record, embedded verbatim")
    args = parser.parse_args(argv)
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):  # Windows consoles default to a legacy code page
            stream.reconfigure(encoding="utf-8", errors="replace")
    try:
        return _dispatch(args)
    except GitError as error:
        print(f"ERROR: cannot read the {args.tag} sources from {args.repo}: {error}", file=sys.stderr)
        return 2


def _dispatch(args: argparse.Namespace) -> int:
    if args.command == "check-source":
        report = check_source(args.repo, args.tag, args.plugin_crate, args.plugin_offline, args.released_binary)
        write_text(args.out, dump(report))
        for item in report["items"]:
            print(f"{'OK  ' if item['ok'] else 'FAIL'} {item['id']}: {item['title']}")
        print(f"plugin semantics: {report['plugin_semantics']['status']} ({report['plugin_semantics']['crate']})")
        if report["plugin_semantics"].get("loud_note"):
            print(report["plugin_semantics"]["loud_note"])
        print(report["verdict"])
        return 0 if report["verdict"] == "SOURCE_VERIFIED" else 1
    command = [str(args.native_check.resolve())] if args.native_check else None
    result = execute(args.repo, args.evidence, args.label, args.tag, args.plugin_crate, args.plugin_offline, command, args.live_observation, args.released_binary)
    for entry in result["assertions"]:
        print(f"{entry['result']:4} {entry['id']}: {entry['title']}\n       {entry['detail'][:300]}")
    print(result["ui_text"], end="")
    print(result["verdict"])
    return 0 if result["verdict"] == VERDICT_ISOLATED else 1


if __name__ == "__main__":
    sys.exit(main())
