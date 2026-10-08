#!/usr/bin/env python3
"""Windows job of the Wave 0 desktop updater isolation lab (THROWAWAY LAB FILE, never merged).

Question (ARC-83 audit item 4): with the legacy-bridge launcher release v0.7.12 as GitHub "Latest" (five launchers +
SHA256SUMS, NO latest.json, v0.8.11 present as bait), does the RELEASED v0.7.11 desktop app's native updater path request
only the manifest URL, download no bundle / installer / signature of any release, install nothing, launch no new app
version and write no new files outside its expected app and log state?

Subcommands
  probe --evidence DIR   capability probe (fast, never fails the job): admin, hosts file, port 443, the Root store, WebView2,
                         openssl, the release asset digest, the firewall, the toolchain, and (with the live network blocked)
                         whether the installed released app starts and answers on the DevTools port. Writes probe.json.
  run   --evidence DIR [--tier released_app|native_check|both] [--cases clean,cached-bait]
                         the real thing. Always writes result.json (schema desktop-os-result.v1, consumed by
                         stage_c_summary.py); exit status 0 whenever evidence was produced.

What `run` does (released_app tier, primary)
  1. reads the v0.7.11 release (gh api, read-only), selects the NSIS installer, downloads it with curl.exe and checks its
     sha256 against the digest the release publishes (and against a pin in config.json when one exists) BEFORE using it;
  2. installs it silently (NSIS /S into C:\\arcw0\\app) and records where the files went;
  3. generates a per-run CA (lib/ca.py), adds ONLY its certificate to the machine Root store, maps the GitHub host names to
     127.0.0.1 in the hosts file and starts the recording server (lib/mitm_server.py) on 127.0.0.1:443;
  4. blocks every live ARC address in the Windows firewall (the Stage A rule) and proves it with a connect test;
  5. per case (clean = latest-404, cached-bait = bait-0.8.11 on top of the state the first launch left): snapshots the
     files and processes, starts the file-write poller, launches the installed app with a sandboxed home and a dedicated
     WebView2 profile, drives the page over the Chrome DevTools Protocol (WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS
     --remote-debugging-port) and calls the Tauri command `plugin:updater|check` (what the Install button's JavaScript
     `check()` calls), records the resolved value or the rejection text VERBATIM, then snapshots again;
  6. computes the five criteria (only_manifest_url, no_bundle_download, no_install, no_new_app_launch, no_new_files)
     fail-closed from the recorded request log, file-write log, snapshots and process lists.
native_check tier (supporting): builds wave0-lab-desktop/native-updater-check (the real tauri-plugin-updater 2.10.1 check()
inside tauri's mock runtime), runs it against the same recording server and the same trust store for both scenarios, plus a
positive control (--control-download) that proves the harness sees a bundle request when one is made.

WHAT THE INTERCEPTION COVERS: the updater plugin's check/download/install path (it verifies TLS with the operating system trust
store, so a per-run CA in the machine Root store plus the hosts mapping lets the recording server answer for github.com).
The app's own reqwest calls (the "update available" banner `check_for_update`, and `ensure_binary`) use bundled webpki roots
and are NOT interceptable: their TLS handshake to the recording server fails and is logged as kind tls_failure. They are
covered by the cited source and the Stage A replica, and result.json says so (interception.not_covered).

Safety (by construction): the live ARC addresses are blocked in the firewall before the app starts; only ca.crt and its
sha256 are ever copied into the evidence directory (the private keys stay under the CA's private/ directory, and the evidence
directory is scanned for key material before the run ends); IPv4 addresses are masked in every log and JSON written here.
Cleanup (hosts file, Root certificate, firewall rule, processes) runs in a finally block.

Fail closed: a case with no recorded request log or no recorded file-write log has criteria null (UNPROVED), never PASS.

UNVERIFIED ON CI (nothing below could be exercised on the Mac this file was written on; the first run decides):
  * that the NSIS installer honours `/S /D=C:\\arcw0\\app` (Tauri's template is a standard NSIS script: /D must be the last
    argument and unquoted) and installs `arc-desktop.exe` there; discovery falls back to the registry Uninstall key and the
    per-user default %LOCALAPPDATA%\\ARC Node;
  * that the WebView2 runtime is present on windows-latest (otherwise the installer downloads the bootstrapper from Microsoft);
  * that WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222 opens http://127.0.0.1:9222/json for the app and
    that its page target URL is http(s)://tauri.localhost/ (or tauri://localhost);
  * that Runtime.evaluate can call window.__TAURI_INTERNALS__.invoke('plugin:updater|check') and that the plugin command
    rejects with the Display text of the updater error ("Could not fetch a valid release JSON from the remote");
  * that rustls-platform-verifier on Windows accepts the one-day ECDSA leaf without revocation information (if not, the
    recording server logs tls_failure rows for github.com and the case is UNPROVED, not PASS);
  * that the app honours HOME / USERPROFILE for ~/.arc (the source reads them) but NOT APPDATA / LOCALAPPDATA (Known Folders), so
    those real directories are watched too;
  * the noise level of the watched real directories (%TEMP%, %APPDATA%): the classification rules in FS_POLICY_DOC are the
    first guess and may need an allow-list entry after the first run;
  * that `cargo build --release --locked` of native-updater-check succeeds on windows-latest (RUSTUP_TOOLCHAIN=stable is set
    because the repository root pins a nightly) and how long it takes (started in the background, joined with a 40 minute cap);
  * that the runner's `python` (3.12) is on PATH as `python`; there may be no `python3`.
"""
from __future__ import annotations

import argparse
import base64
import collections
import datetime
import hashlib
import json
import os
import platform
import re
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Callable, Dict, Iterable, List, Optional, Sequence, Tuple

HERE = Path(__file__).resolve().parent            # wave0-lab-desktop/
ROOT = HERE.parent                                # repository root
LIB = HERE / "lib"
for _path in (str(LIB), str(HERE)):
    if _path not in sys.path:
        sys.path.insert(0, _path)

IS_WINDOWS = os.name == "nt"

SCHEMA_RESULT = "arc.legacy-bridge.wave0-lab.desktop-os-result.v1"
SCHEMA_PROBE = "arc.legacy-bridge.wave0-lab.desktop-os-probe.v1"
REPO = "FerrumVir/arc-chain"
TAG = "v0.7.11"
BAIT_TAG = "v0.8.11"
MANIFEST_HOST = "github.com"
MANIFEST_PATH = "/FerrumVir/arc-chain/releases/latest/download/latest.json"
MANIFEST_URL = "https://" + MANIFEST_HOST + MANIFEST_PATH
API_LATEST_PATH = "/repos/FerrumVir/arc-chain/releases/latest"
REDIRECT_TARGET_PATH = "/FerrumVir/arc-chain/releases/download/v0.7.12/latest.json"
RELEASE_NOT_FOUND = "Could not fetch a valid release JSON from the remote"   # tauri-plugin-updater 2.10.1 src/error.rs:25
PLUGIN_NAME = "tauri-plugin-updater"
PLUGIN_VERSION = "2.10.1"
PUBKEY_PREFIX = "dW50cnVzdGVkIGNvbW1lbnQ6"        # base64 of "untrusted comment:", start of every minisign public key
CDP_PORT = 9222
APP_EXE_NAME = "arc-desktop.exe"
NATIVE_EXE_NAME = "native-updater-check.exe"
WEBVIEW2_PROCESS = "msedgewebview2.exe"
FIREWALL_RULE = "arc-live-network-block"
HOSTS_BEGIN = "# arcw0-desktop-lab begin"
HOSTS_END = "# arcw0-desktop-lab end"
BLACKHOLE_HOSTS = ("rsms.me", "huggingface.co", "cdn-lfs.huggingface.co")   # third-party hosts the page would fetch: keep the run hermetic
INSTALLER_NSIS = re.compile(r"^ARC\.Node_[0-9]+\.[0-9]+\.[0-9]+_x64-setup\.exe$")
INSTALLER_MSI = re.compile(r"^ARC\.Node_[0-9]+\.[0-9]+\.[0-9]+_x64_en-US\.msi$")
CASE_NAMES = ("clean", "cached-bait")
CASE_SCENARIO = {"clean": "latest-404", "cached-bait": "bait-0.8.11"}
TIERS = ("released_app", "native_check")
CRITERIA = ("only_manifest_url", "no_bundle_download", "no_install", "no_new_app_launch", "no_new_files")

# Kept equal to stage_c_summary.FORBIDDEN_PATH_PATTERNS (a test compares the two lists).
FORBIDDEN_PATH_PATTERNS = [
    (re.compile(r"^/" + re.escape(REPO) + r"/releases/download/v0\.8\."), "a v0.8 release asset"),
    (re.compile(r"^/" + re.escape(REPO) + r"/releases/download/v0\.7\.11/"), "a v0.7.11 release asset"),
    (re.compile(r"(\.AppImage|\.deb|\.rpm|\.dmg|\.msi|-setup\.exe|\.app\.tar\.gz)$"), "an app bundle"),
    (re.compile(r"\.sig$"), "a signature file"),
]
PAYLOAD_SUFFIXES = (".exe", ".msi", ".msix", ".appx", ".nsis", ".sig", ".zip", ".gz", ".tgz", ".tar", ".7z", ".dmg", ".deb", ".rpm", ".appimage", ".crx")
UPDATER_DIR_MARK = "-updater-"                     # the plugin extracts into <temp>/<app>-<version>-updater-XXXX/

FS_POLICY_DOC = {
    "app-state": "the app's own state: expected, recorded; only payload-like names (installers, archives, signatures, *-updater-* directories) are unexpected",
    "browser-profile": "the dedicated WebView2 profile: browser churn is expected and recorded, not classified",
    "frozen": "must not change at all (the install directory, the real profile's .arc, Downloads, Desktop, Documents, Programs)",
    "update-artifacts-only": "%TEMP%: runner and tool churn is recorded as noise; only payload-like names or *-updater-* directories are unexpected",
}


# ---------------------------------------------------------------------------------------------------------------------
# small helpers
# ---------------------------------------------------------------------------------------------------------------------

_IPV4 = re.compile(r"\b(?:(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\b")


def mask_ips(text: str) -> str:
    """IPv4 addresses other than loopback / unspecified / TEST-NET-1 become a.b.x.x (nothing bound for review shows a live address)."""
    def replace(match: "re.Match[str]") -> str:
        address = match.group(0)
        if address.startswith("127.") or address in ("0.0.0.0", "255.255.255.255") or address.startswith("192.0.2."):
            return address
        parts = address.split(".")
        return "%s.%s.x.x" % (parts[0], parts[1])
    return _IPV4.sub(replace, text)


def now_iso() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with Path(path).open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path: Path, value: Any, sort_keys: bool = True) -> None:
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=sort_keys, default=str) + "\n", encoding="utf-8")     # default=str: a probe detail must never break the write


def read_jsonl(path: Path) -> Optional[List[dict]]:
    """Records of a JSON-lines file; None when the file does not exist; a torn last line is skipped."""
    path = Path(path)
    if not path.is_file():
        return None
    records: List[dict] = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict):
            records.append(value)
    return records


def norm_path(path: str) -> str:
    """Windows path comparison key: backslashes, no trailing separator, case-folded."""
    return path.replace("/", "\\").rstrip("\\").lower()


def path_under(path: str, root: str) -> bool:
    p, r = norm_path(path), norm_path(root)
    return p == r or p.startswith(r + "\\")


def pem_der(pem_text: str) -> bytes:
    match = re.search(r"-----BEGIN CERTIFICATE-----\s*(.+?)\s*-----END CERTIFICATE-----", pem_text, re.DOTALL)
    if not match:
        raise ValueError("no PEM certificate found")
    return base64.b64decode("".join(match.group(1).split()))


def thumbprint_sha1(pem_text: str) -> str:
    """Windows certificate thumbprint (SHA-1 of the DER form), upper-case hex."""
    return hashlib.sha1(pem_der(pem_text)).hexdigest().upper()


# ---------------------------------------------------------------------------------------------------------------------
# running commands, with a log of every command and exit code
# ---------------------------------------------------------------------------------------------------------------------

class CmdResult:
    __slots__ = ("rc", "out", "seconds")

    def __init__(self, rc: int, out: str, seconds: float = 0.0):
        self.rc, self.out, self.seconds = rc, out, seconds

    @property
    def ok(self) -> bool:
        return self.rc == 0


class Shell:
    """Runs commands and records every command and exit code in steps.log (IPv4 addresses masked, never any key material)."""

    def __init__(self, log_path: Optional[Path] = None):
        self.log_path = Path(log_path) if log_path else None
        self.lock = threading.Lock()
        self.history: List[Tuple[List[str], int]] = []

    def log(self, line: str) -> None:
        text = "[%s] %s\n" % (now_iso(), mask_ips(line))
        with self.lock:
            if self.log_path is not None:
                self.log_path.parent.mkdir(parents=True, exist_ok=True)
                with self.log_path.open("a", encoding="utf-8") as handle:
                    handle.write(text)

    def run(self, argv: Sequence[str], timeout: float = 120.0, env: Optional[Dict[str, str]] = None, cwd: Optional[str] = None,
            quiet: bool = False, capture: bool = True) -> CmdResult:
        started = time.time()
        try:
            if capture:
                done = subprocess.run(list(argv), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout, env=env, cwd=cwd, check=False)
                rc, out = done.returncode, done.stdout.decode("utf-8", "replace")
            else:   # installers may leave children that inherit a pipe and keep run() waiting; the exit code is all that is needed
                done = subprocess.run(list(argv), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=timeout, env=env, cwd=cwd, check=False)
                rc, out = done.returncode, ""
        except subprocess.TimeoutExpired as error:
            rc, out = 124, (error.stdout or b"").decode("utf-8", "replace") + "\n[timed out after %.0f s]" % timeout
        except OSError as error:
            rc, out = 127, "%s: %s" % (type(error).__name__, error)
        seconds = time.time() - started
        self.history.append((list(argv), rc))
        if not quiet:
            tail = out.strip()
            limit = 1500 if rc != 0 else 300
            self.log("rc=%d %.1fs $ %s%s" % (rc, seconds, " ".join(argv)[:600], ("\n    " + tail[-limit:].replace("\n", "\n    ")) if tail else ""))
        return CmdResult(rc, out, seconds)


def powershell_argv(script: str) -> List[str]:
    return ["powershell", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script]


# ---------------------------------------------------------------------------------------------------------------------
# release asset selection, pins
# ---------------------------------------------------------------------------------------------------------------------

class AssetError(RuntimeError):
    pass


def select_installer(release: dict) -> dict:
    """The Windows desktop installer of a release JSON: NSIS setup preferred, MSI as fallback. Returns name, size, digest, url."""
    assets = release.get("assets") if isinstance(release, dict) else None
    if not isinstance(assets, list):
        raise AssetError("the release JSON has no assets list")
    for pattern in (INSTALLER_NSIS, INSTALLER_MSI):
        found = [asset for asset in assets if isinstance(asset, dict) and isinstance(asset.get("name"), str) and pattern.match(asset["name"])]
        if len(found) > 1:
            raise AssetError("more than one asset matches %s: %s" % (pattern.pattern, [a["name"] for a in found]))
        if found:
            asset = found[0]
            digest = asset.get("digest")
            if not (isinstance(digest, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", digest)):
                raise AssetError("asset %s has no sha256 digest in the release JSON (got %r)" % (asset["name"], digest))
            url = asset.get("browser_download_url") or ("https://github.com/%s/releases/download/%s/%s" % (REPO, release.get("tag_name") or TAG, asset["name"]))
            return {"name": asset["name"], "size": asset.get("size"), "digest": digest, "url": url, "kind": "nsis" if pattern is INSTALLER_NSIS else "msi"}
    raise AssetError("no Windows installer (NSIS setup or MSI) among the release assets")


def find_pin(config: Any, name: str) -> Optional[dict]:
    """A pin for asset `name` anywhere in config.json: any object with that name and a sha256 / digest value."""
    if isinstance(config, dict):
        if config.get("name") == name and any(isinstance(config.get(key), str) for key in ("digest", "sha256")):
            return config
        for value in config.values():
            found = find_pin(value, name)
            if found:
                return found
    elif isinstance(config, list):
        for value in config:
            found = find_pin(value, name)
            if found:
                return found
    return None


def pin_digest(pin: dict) -> str:
    raw = pin.get("digest") or pin.get("sha256") or ""
    return raw.lower().replace("sha256:", "")


def digest_problems(asset: dict, computed_sha256: str, pin: Optional[dict]) -> List[str]:
    """Why the downloaded installer must not be used (empty list = it is the released asset)."""
    problems: List[str] = []
    published = asset["digest"].lower().replace("sha256:", "")
    if computed_sha256.lower() != published:
        problems.append("downloaded sha256 %s differs from the release digest %s" % (computed_sha256, published))
    if pin is not None and pin_digest(pin) and pin_digest(pin) != published:
        problems.append("the pinned digest %s in config.json differs from the release digest %s" % (pin_digest(pin), published))
    return problems


def parse_tauri_conf(text: str) -> Dict[str, Any]:
    conf = json.loads(text)
    updater = ((conf.get("plugins") or {}).get("updater")) or {}
    return {"pubkey": updater.get("pubkey"), "endpoints": updater.get("endpoints"), "version": conf.get("version"), "product": conf.get("productName"),
            "identifier": conf.get("identifier")}


def extract_pubkey(data: bytes) -> Optional[str]:
    match = re.search(re.escape(PUBKEY_PREFIX.encode()) + rb"[A-Za-z0-9+/=]{40,200}", data)
    return match.group(0).decode("ascii") if match else None


# ---------------------------------------------------------------------------------------------------------------------
# the isolation: hosts file, trust store, firewall
# ---------------------------------------------------------------------------------------------------------------------

def blackhole_names() -> List[str]:
    """ARC names and the third-party hosts the page would fetch, all mapped to loopback (lib/live_block.py when importable)."""
    try:
        import live_block  # lib/live_block.py
        return list(live_block.blocked_names())
    except Exception:  # noqa: BLE001 - the local list is the fallback, never a reason to stop
        return list(BLACKHOLE_HOSTS)


def hosts_block(mapped: Sequence[str], blackholed: Sequence[str] = ()) -> str:
    """Hosts lines (IPv4 and IPv6 loopback) for the names the recording server plays plus the names that must lead nowhere real."""
    lines = [HOSTS_BEGIN]
    for name in list(mapped) + [n for n in blackholed if n not in mapped]:
        lines.append("127.0.0.1 %s" % name)
        lines.append("::1 %s" % name)
    lines.append(HOSTS_END)
    return "\r\n".join(lines) + "\r\n"


def strip_hosts_block(text: str) -> str:
    out: List[str] = []
    skipping = False
    for line in text.splitlines():
        if line.strip() == HOSTS_BEGIN:
            skipping = True
            continue
        if line.strip() == HOSTS_END:
            skipping = False
            continue
        if not skipping:
            out.append(line)
    return "\r\n".join(out) + ("\r\n" if out else "")


def hosts_with_block(original: str, block: str) -> str:
    base = strip_hosts_block(original)
    if base and not base.endswith("\r\n"):
        base += "\r\n"
    return base + block


def firewall_block_script(ips: Sequence[str], name: str = FIREWALL_RULE) -> str:
    if not ips:
        raise ValueError("no addresses to block")
    quoted = ",".join("'%s'" % ip for ip in ips)
    return ("$ErrorActionPreference = 'Stop'; "
            "New-NetFirewallRule -DisplayName '%s' -Direction Outbound -Action Block -RemoteAddress %s | Out-Null; "
            "Set-NetFirewallProfile -All -Enabled True; Write-Output 'rule-created'" % (name, quoted))


def firewall_verify_script(ip: str, port: int = 443, timeout_ms: int = 5000) -> str:
    """Exit 0 when the address is NOT reachable (blocked), 3 when it is."""
    return ("$c = New-Object System.Net.Sockets.TcpClient; $r = $false; "
            "try { $r = $c.ConnectAsync('%s', %d).Wait(%d) -and $c.Connected } "
            "catch { Write-Output ('blocked: ' + $_.Exception.InnerException.Message) }; "
            "$c.Dispose(); if ($r) { Write-Output 'REACHABLE'; exit 3 } else { Write-Output 'unreachable'; exit 0 }" % (ip, port, timeout_ms))


def firewall_remove_script(name: str = FIREWALL_RULE) -> str:
    return "Remove-NetFirewallRule -DisplayName '%s' -ErrorAction SilentlyContinue" % name


def firewall_list_script(name: str = FIREWALL_RULE) -> str:
    return ("Get-NetFirewallRule -DisplayName '%s' | ForEach-Object { $a = $_ | Get-NetFirewallAddressFilter; "
            "'{0} {1} {2} remote={3}' -f $_.DisplayName, $_.Direction, $_.Action, (($a.RemoteAddress | Measure-Object).Count) }" % name)


def certutil_add_argv(ca_path: str) -> List[str]:
    return ["certutil", "-addstore", "-f", "Root", ca_path]


def certutil_del_argv(thumbprint: str) -> List[str]:
    return ["certutil", "-delstore", "Root", thumbprint]


def root_store_check_script(thumbprint: str) -> str:
    if not re.fullmatch(r"[0-9A-F]{40}", thumbprint):
        raise ValueError("not a SHA-1 thumbprint: %r" % thumbprint)
    return "(Get-ChildItem Cert:\\LocalMachine\\Root | Where-Object { $_.Thumbprint -eq '%s' } | Measure-Object).Count" % thumbprint


def load_live_ips(root: Path) -> List[str]:
    """The live ARC addresses the repository's own CI isolates (same sources and checks as wave0-lab/live_ips.py)."""
    workflow = (root / ".github/workflows/legacy-bridge.yml").read_text(encoding="utf-8")
    harness = (root / "tests/legacy-bridge/headless-v07-acceptance.sh").read_text(encoding="utf-8")
    found = re.findall(r"^\s*LIVE_NETWORK_IPS:\s*(.+?)\s*$", workflow, flags=re.MULTILINE)
    if len(found) != 1:
        raise ValueError("expected exactly one LIVE_NETWORK_IPS line, found %d" % len(found))
    from_workflow = found[0].split()
    arrays = re.findall(r"^live_ips=\((.*?)\)\s*$", harness, flags=re.MULTILINE)
    if len(arrays) != 1:
        raise ValueError("expected exactly one live_ips array, found %d" % len(arrays))
    if from_workflow != arrays[0].split():
        raise ValueError("LIVE_NETWORK_IPS differs from live_ips in the acceptance script")
    if not from_workflow or len(set(from_workflow)) != len(from_workflow):
        raise ValueError("the live address list is empty or has duplicates")
    for address in from_workflow:
        if not _IPV4.fullmatch(address):
            raise ValueError("the live address list holds an entry that is not a dotted IPv4 address")
    return from_workflow


# ---------------------------------------------------------------------------------------------------------------------
# requests: summary and classification
# ---------------------------------------------------------------------------------------------------------------------

def record_host(record: dict) -> str:
    host = str(record.get("host") or record.get("sni") or "").lower()
    return host.split(":")[0]


def payload_reason(path: str, flagged: bool = False) -> Optional[str]:
    if flagged:
        return "flagged payload by the recording server"
    for pattern, label in FORBIDDEN_PATH_PATTERNS:
        if pattern.search(path):
            return label
    low = path.lower().split("?")[0]
    if low.endswith(PAYLOAD_SUFFIXES):
        return "a payload-like file name"
    return None


def classify_request(host: str, path: str, flagged: bool = False) -> str:
    """manifest | manifest-redirect | api-latest | payload | other-latest-json | other."""
    clean_path = path.split("?")[0]
    if host == MANIFEST_HOST and clean_path == MANIFEST_PATH:
        return "manifest"
    if host == MANIFEST_HOST and clean_path == REDIRECT_TARGET_PATH:
        return "manifest-redirect"      # where GitHub resolves ".../releases/latest/download/latest.json" while Latest is v0.7.12
    if host == "api.github.com" and clean_path == API_LATEST_PATH:
        return "api-latest"
    if payload_reason(clean_path, flagged):
        return "payload"
    if clean_path.endswith("latest.json"):
        return "other-latest-json"
    return "other"


def summarize_requests(records: Optional[List[dict]]) -> Dict[str, Any]:
    """The `requests` object of a case plus the classes found. None when there is no log at all."""
    if records is None:
        return {"present": False, "total": 0, "by_host_path": [], "tls_failures": [], "classes": {}}
    requests = [r for r in records if r.get("kind") == "request"]
    tls = [r for r in records if r.get("kind") == "tls_failure"]
    counts: "collections.Counter[Tuple[str, str]]" = collections.Counter()
    classes: Dict[str, List[str]] = collections.defaultdict(list)
    for record in requests:
        host, path = record_host(record), str(record.get("path") or "")
        counts[(host, path)] += 1
        kind = classify_request(host, path, record.get("payload") is True)
        label = "%s%s" % (host, path)
        if label not in classes[kind]:
            classes[kind].append(label)
    tls_counts: "collections.Counter[str]" = collections.Counter(str(r.get("sni") or "") for r in tls)
    return {
        "present": True,
        "total": len(requests),
        "by_host_path": [[host, path, count] for (host, path), count in sorted(counts.items())],
        "tls_failures": [{"sni": sni or None, "count": count} for sni, count in sorted(tls_counts.items())],
        "classes": {kind: sorted(values) for kind, values in classes.items()},
    }


# ---------------------------------------------------------------------------------------------------------------------
# files: snapshots, the write poller, classification
# ---------------------------------------------------------------------------------------------------------------------

def take_snapshot(roots: Sequence[Tuple[str, bool]], hash_limit: int = 1 << 20) -> Dict[str, dict]:
    """{path: {type, size, mtime_ns, sha256?}} for every entry below the roots ((root, hash_files) pairs). Same shape as lib/fswatch.py."""
    entries: Dict[str, dict] = {}
    for root, hashing in roots:
        if not os.path.exists(root):
            continue
        for current, dirnames, filenames in os.walk(root, onerror=lambda _error: None, followlinks=False):
            for name in dirnames:
                full = os.path.join(current, name)
                entries[full] = {"type": "dir", "size": 0, "mtime_ns": 0}
            for name in filenames:
                full = os.path.join(current, name)
                try:
                    info = os.lstat(full)
                except OSError:
                    continue
                entry = {"type": "file", "size": info.st_size, "mtime_ns": info.st_mtime_ns}
                if hashing and info.st_size <= hash_limit:
                    try:
                        entry["sha256"] = sha256_file(Path(full))
                    except OSError:
                        pass
                entries[full] = entry
    return entries


def diff_snapshots(before: Dict[str, dict], after: Dict[str, dict]) -> Dict[str, List[str]]:
    added = sorted(path for path in after if path not in before)
    removed = sorted(path for path in before if path not in after)
    changed = []
    for path in sorted(set(before) & set(after)):
        b, a = before[path], after[path]
        if a.get("type") == "dir" and b.get("type") == "dir":
            continue
        if (a.get("size"), a.get("mtime_ns"), a.get("sha256")) != (b.get("size"), b.get("mtime_ns"), b.get("sha256")):
            changed.append(path)
    return {"added": added, "removed": removed, "changed": changed}


class Poller:
    """Records every new / changed / removed path below the watched roots with the time it was first seen (writes-<case>.jsonl)."""

    def __init__(self, roots: Sequence[Tuple[str, bool]], out_path: Path, interval: float = 2.0):
        self.roots, self.out_path, self.interval = list(roots), Path(out_path), interval
        self.stop_event = threading.Event()
        self.thread: Optional[threading.Thread] = None
        self.cycles = 0
        self.lock = threading.Lock()

    def _write(self, record: dict) -> None:
        with self.lock, self.out_path.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(record, sort_keys=True) + "\n")
            handle.flush()
            os.fsync(handle.fileno())

    def mark(self, label: str) -> None:
        self._write({"t": round(time.time(), 3), "event": "mark", "label": label})

    def _loop(self) -> None:
        previous = take_snapshot(self.roots, hash_limit=0)
        while not self.stop_event.wait(self.interval):
            current = take_snapshot(self.roots, hash_limit=0)
            now = round(time.time(), 3)
            delta = diff_snapshots(previous, current)
            for event, paths in (("new", delta["added"]), ("changed", delta["changed"]), ("removed", delta["removed"])):
                for path in paths:
                    info = current.get(path) or previous.get(path) or {}
                    self._write({"t": now, "event": event, "path": path, "size": info.get("size"), "type": info.get("type")})
            previous = current
            self.cycles += 1

    def start(self) -> None:
        self.out_path.parent.mkdir(parents=True, exist_ok=True)
        self.out_path.write_text("", encoding="utf-8")
        self._write({"t": round(time.time(), 3), "event": "start", "roots": [norm_path(r) for r, _ in self.roots], "interval_s": self.interval})
        self.thread = threading.Thread(target=self._loop, name="fs-poller", daemon=True)
        self.thread.start()

    def stop(self) -> int:
        self.stop_event.set()
        if self.thread is not None:
            self.thread.join(timeout=30)
        self._write({"t": round(time.time(), 3), "event": "stop", "cycles": self.cycles})
        return self.cycles


def payload_like_path(path: str) -> Optional[str]:
    low = norm_path(path)
    base = low.rsplit("\\", 1)[-1]
    if UPDATER_DIR_MARK in low:
        return "an updater working directory"
    if base.endswith(PAYLOAD_SUFFIXES):
        return "a payload-like file name"
    return None


def classify_fs_changes(delta: Dict[str, List[str]], policies: Sequence[Dict[str, str]]) -> Dict[str, Any]:
    """Split a snapshot diff into expected / unexpected / noise by the most specific watched root (see FS_POLICY_DOC)."""
    ordered = sorted(policies, key=lambda item: len(norm_path(item["root"])), reverse=True)
    expected: List[str] = []
    noise: List[str] = []
    unexpected: List[Dict[str, str]] = []
    for kind in ("added", "changed", "removed"):
        for path in delta.get(kind, []):
            policy = next((item for item in ordered if path_under(path, item["root"])), None)
            if policy is None:
                noise.append(path)
                continue
            mode = policy["policy"]
            payload = payload_like_path(path)
            if mode == "frozen":
                unexpected.append({"path": path, "kind": kind, "reason": "frozen location changed (%s)" % policy.get("label", policy["root"])})
            elif mode == "app-state":
                if payload:
                    unexpected.append({"path": path, "kind": kind, "reason": "%s inside app state" % payload})
                else:
                    expected.append(path)
            elif mode == "browser-profile":
                expected.append(path)
            elif mode == "update-artifacts-only":
                if payload:
                    unexpected.append({"path": path, "kind": kind, "reason": "%s in the temp directory" % payload})
                else:
                    noise.append(path)
            else:
                unexpected.append({"path": path, "kind": kind, "reason": "unknown policy %r" % mode})
    return {"expected": sorted(expected), "noise": sorted(noise), "unexpected": unexpected}


def fs_policies(sandbox_home: str, wv2_dir: str, install_dir: str, env: Dict[str, str]) -> List[Dict[str, str]]:
    """Watched roots and their policy. Real profile paths come from the environment of the runner user."""
    profile = env.get("USERPROFILE", "")
    appdata = env.get("APPDATA", "")
    local = env.get("LOCALAPPDATA", "")
    temp = env.get("TEMP", "")
    policies = [
        {"root": sandbox_home, "policy": "app-state", "label": "sandboxed home (~/.arc)"},
        {"root": wv2_dir, "policy": "browser-profile", "label": "WebView2 profile"},
        {"root": install_dir, "policy": "frozen", "label": "install directory"},
    ]
    if appdata:
        policies.append({"root": appdata + "\\network.arc.desktop", "policy": "app-state", "label": "Tauri app data"})
        policies.append({"root": appdata, "policy": "update-artifacts-only", "label": "roaming profile"})
    if local:
        policies.append({"root": local + "\\network.arc.desktop", "policy": "app-state", "label": "Tauri local data"})
        policies.append({"root": local + "\\Programs", "policy": "frozen", "label": "per-user programs"})
    if temp:
        policies.append({"root": temp, "policy": "update-artifacts-only", "label": "temp"})
    if profile:
        for sub in (".arc", "Downloads", "Desktop", "Documents"):
            policies.append({"root": profile + "\\" + sub, "policy": "frozen", "label": "real profile %s" % sub})
    return policies


def fs_watch_roots(policies: Sequence[Dict[str, str]]) -> List[Tuple[str, bool]]:
    """(root, hash files) for the snapshot; hashing only where the content matters (not temp / roaming / browser profile)."""
    seen: Dict[str, bool] = {}
    for item in policies:
        hashing = item["policy"] in ("app-state", "frozen")
        key = norm_path(item["root"])
        if key in seen:
            continue
        seen[key] = hashing
    result: List[Tuple[str, bool]] = []
    # drop roots nested inside another watched root so entries are not walked twice (the outer root carries the policy lookup)
    originals = {norm_path(item["root"]): item["root"] for item in policies}
    for key, hashing in seen.items():
        if any(other != key and key.startswith(other + "\\") for other in seen):
            continue
        result.append((originals[key], hashing))
    return result


# ---------------------------------------------------------------------------------------------------------------------
# processes
# ---------------------------------------------------------------------------------------------------------------------

PS_PROCESSES = ("Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,Name,ExecutablePath,CommandLine,CreationDate "
                "| ConvertTo-Json -Compress -Depth 2")


COMMAND_LINE_ALLOWED = re.compile(r"(?i)^(arc-desktop|msedgewebview2|native-updater-check|arc-node|.*setup.*|msiexec|.*updater.*)\.exe$")


def redact_processes(processes: Optional[List[dict]]) -> Optional[List[dict]]:
    """Process list for an evidence file: command lines only for the processes this test is about (a runner process may carry a token)."""
    if processes is None:
        return None
    out = []
    for process in processes:
        item = dict(process)
        if not COMMAND_LINE_ALLOWED.match(str(item.get("Name") or "")):
            item["CommandLine"] = "<omitted>"
        item["CommandLine"] = mask_ips(str(item["CommandLine"]))[:400]
        out.append(item)
    return out


def write_process_list(path: Path, processes: Optional[List[dict]]) -> None:
    redacted = redact_processes(processes)
    Path(path).write_text(json.dumps(redacted, indent=1, default=str) + "\n" if redacted else "unavailable\n", encoding="utf-8")


def parse_processes(text: str) -> Optional[List[dict]]:
    """Process list from the PowerShell JSON; None when the text is not a usable list (the snapshot is then missing)."""
    text = text.strip()
    if not text:
        return None
    try:
        value = json.loads(text)
    except ValueError:
        return None
    if isinstance(value, dict):
        value = [value]
    if not isinstance(value, list):
        return None
    processes = [item for item in value if isinstance(item, dict) and isinstance(item.get("ProcessId"), int)]
    return processes if processes else None


def process_key(process: dict) -> Tuple[Any, Any]:
    return (process.get("ProcessId"), str(process.get("CreationDate")))


def descendants(root_pid: int, processes: Sequence[dict]) -> set:
    children: Dict[int, List[int]] = collections.defaultdict(list)
    for process in processes:
        children[process.get("ParentProcessId")].append(process["ProcessId"])
    found, stack = set(), [root_pid]
    while stack:
        pid = stack.pop()
        for child in children.get(pid, []):
            if child not in found:
                found.add(child)
                stack.append(child)
    return found


_SUSPECT_NAME = re.compile(r"(?i)(setup|install|updater?|arc-desktop|arc\.node)")


def process_violations(before: Optional[List[dict]], after: Optional[List[dict]], app_pid: Optional[int], markers: Sequence[str],
                       ignore_exes: Sequence[str] = ()) -> Optional[Dict[str, List[str]]]:
    """New processes between the two snapshots that are not the app's own WebView2 helpers. None = a snapshot is missing."""
    if before is None or after is None or app_pid is None:
        return None
    old = {process_key(p) for p in before}
    skip = {norm_path(path) for path in ignore_exes if path}
    new = [p for p in after if process_key(p) not in old and norm_path(str(p.get("ExecutablePath") or "")) not in skip]
    tree = descendants(app_pid, after)
    low_markers = [m.lower() for m in markers if m]
    result: Dict[str, List[str]] = {"new_app_launches": [], "installers": [], "other_in_app_tree": [], "unrelated": []}
    for process in new:
        name = str(process.get("Name") or "")
        exe = str(process.get("ExecutablePath") or "")
        command = str(process.get("CommandLine") or "")
        text = "%s | %s | %s (pid %s)" % (name, exe, command[:160], process.get("ProcessId"))
        if name.lower() == WEBVIEW2_PROCESS and process["ProcessId"] in tree:
            continue
        related = process["ProcessId"] in tree or any(m in (exe + " " + command).lower() for m in low_markers)
        if name.lower() == APP_EXE_NAME:
            result["new_app_launches"].append(text)
        elif re.search(r"(?i)(setup|install|updater?|msiexec)", name) and (related or _SUSPECT_NAME.search(exe)):
            result["installers"].append(text)
        elif related:
            result["other_in_app_tree"].append(text)
        else:
            result["unrelated"].append(text)
    return result


# ---------------------------------------------------------------------------------------------------------------------
# criteria and verdicts (fail closed)
# ---------------------------------------------------------------------------------------------------------------------

def scenario_problem(scenario: str, trigger: Optional[dict]) -> Optional[str]:
    """Why the trigger's outcome does not show the scenario was actually exercised (None = it does)."""
    if not trigger or not trigger.get("ran"):
        return "the trigger did not run"
    if scenario == "latest-404":
        text = trigger.get("error_text")
        if not trigger.get("ok") and isinstance(text, str) and RELEASE_NOT_FOUND in text:
            return None
        return "latest-404: expected the rejection %r, got %r" % (RELEASE_NOT_FOUND, trigger.get("error_text") if not trigger.get("ok") else trigger.get("value"))
    if scenario == "bait-0.8.11":
        value = trigger.get("value")
        if trigger.get("ok") and isinstance(value, dict) and str(value.get("version")) == BAIT_TAG.lstrip("v"):
            return None
        return "bait-0.8.11: expected an update to 0.8.11, got %r" % (trigger.get("error_text") if not trigger.get("ok") else trigger.get("value"))
    return "unknown scenario %r" % scenario


def evaluate_case(scenario: str, trigger: Optional[dict], requests: Dict[str, Any], fs: Optional[Dict[str, Any]], procs: Optional[Dict[str, List[str]]],
                  writes_ok: bool, install_changes: Optional[List[str]]) -> Dict[str, Any]:
    """criteria (true / false / None), verdict and reasons for one case from its recorded evidence."""
    reasons: List[str] = []
    criteria: Dict[str, Optional[bool]] = {key: None for key in CRITERIA}
    ran = bool(trigger and trigger.get("ran"))
    if not ran:
        reasons.append("the trigger did not run, nothing was exercised")
    classes = requests.get("classes", {}) if requests.get("present") else {}
    if requests.get("present") and ran:
        if requests["total"] == 0:
            reasons.append("the recording server logged no request: the updater never reached it (TLS failures: %s)" % (requests.get("tls_failures") or "none"))
        else:
            criteria["only_manifest_url"] = not (classes.get("payload") or classes.get("other-latest-json") or classes.get("other"))
            criteria["no_bundle_download"] = not classes.get("payload")
    elif not requests.get("present"):
        reasons.append("no request log")
    # no_install: nothing changed in the install directory, no payload-like file anywhere, no installer process
    if ran and install_changes is not None and fs is not None and procs is not None:
        payload_files = [item for item in fs.get("unexpected", []) if "payload" in item["reason"] or "updater" in item["reason"]]
        criteria["no_install"] = not install_changes and not payload_files and not procs.get("installers")
    else:
        reasons.append("no_install needs the install directory diff, the file classification and the process lists")
    if ran and procs is not None:
        criteria["no_new_app_launch"] = not procs.get("new_app_launches") and not procs.get("installers") and not procs.get("other_in_app_tree")
    else:
        reasons.append("no_new_app_launch needs the process lists")
    if ran and fs is not None and writes_ok:
        criteria["no_new_files"] = not fs.get("unexpected")
    else:
        reasons.append("no_new_files needs the snapshots and a complete file-write log")
    problem = scenario_problem(scenario, trigger)
    verdict = "PASS"
    if any(value is False for value in criteria.values()):
        verdict = "FAIL"
    elif any(value is None for value in criteria.values()) or problem is not None:
        verdict = "UNPROVED"
    if problem is not None and verdict != "FAIL":
        reasons.append(problem)
    return {"criteria": criteria, "verdict": verdict, "reasons": reasons}


def tier_result(wanted: Sequence[str], cases: Sequence[dict], status: str) -> str:
    """PASS / FAIL / UNPROVED of one tier from its cases."""
    verdicts = {c["name"]: c["verdict"] for c in cases}
    if any(v == "FAIL" for v in verdicts.values()):
        return "FAIL"
    if status == "ran" and wanted and all(verdicts.get(name) == "PASS" for name in wanted) and len(verdicts) == len(wanted):
        return "PASS"
    return "UNPROVED"


def overall_verdict(tiers: Dict[str, dict], cases: Sequence[dict]) -> str:
    attempted = [t for t in tiers.values() if t.get("attempted")]
    if any(t.get("result") == "FAIL" for t in attempted) or any(c["verdict"] == "FAIL" for c in cases):
        return "FAIL"
    if attempted and all(t.get("result") == "PASS" for t in attempted) and cases and all(c["verdict"] == "PASS" for c in cases):
        passing = {c["name"] for c in cases}
        if set(CASE_NAMES) <= passing:
            return "PASS"
    return "UNPROVED"


# ---------------------------------------------------------------------------------------------------------------------
# WebSocket (RFC 6455, client only) and the Chrome DevTools Protocol
# ---------------------------------------------------------------------------------------------------------------------

WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


class WebSocketError(RuntimeError):
    pass


def ws_accept_key(key: str) -> str:
    return base64.b64encode(hashlib.sha1((key + WS_GUID).encode("ascii")).digest()).decode("ascii")


def ws_encode_frame(payload: bytes, opcode: int = 0x1, mask: bool = True, key: Optional[bytes] = None) -> bytes:
    header = bytearray([0x80 | opcode])
    length = len(payload)
    mask_bit = 0x80 if mask else 0
    if length < 126:
        header.append(mask_bit | length)
    elif length < (1 << 16):
        header.append(mask_bit | 126)
        header += struct.pack(">H", length)
    else:
        header.append(mask_bit | 127)
        header += struct.pack(">Q", length)
    if not mask:
        return bytes(header) + payload
    key = key if key is not None else os.urandom(4)
    masked = bytes(byte ^ key[index % 4] for index, byte in enumerate(payload))
    return bytes(header) + key + masked


class WebSocketClient:
    """Minimal text-message client: handshake, masked frames out, fragmentation / ping / close in. No extensions, no TLS."""

    def __init__(self, host: str, port: int, path: str, timeout: float = 10.0):
        self.host, self.port, self.path, self.timeout = host, port, path, timeout
        self.sock: Optional[socket.socket] = None
        self.buffer = b""

    def connect(self) -> None:
        sock = socket.create_connection((self.host, self.port), timeout=self.timeout)
        try:
            self._handshake(sock)
        except Exception:
            sock.close()
            raise

    def _handshake(self, sock: socket.socket) -> None:
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        request = ("GET %s HTTP/1.1\r\nHost: %s:%d\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                   "Sec-WebSocket-Key: %s\r\nSec-WebSocket-Version: 13\r\n\r\n" % (self.path, self.host, self.port, key))
        sock.sendall(request.encode("ascii"))
        data = b""
        while b"\r\n\r\n" not in data:
            chunk = sock.recv(4096)
            if not chunk:
                raise WebSocketError("the server closed the connection during the handshake")
            data += chunk
            if len(data) > 65536:
                raise WebSocketError("handshake response too large")
        head, _, rest = data.partition(b"\r\n\r\n")
        lines = head.decode("latin-1").split("\r\n")
        if not lines[0].startswith("HTTP/1.1 101"):
            raise WebSocketError("handshake refused: %s" % lines[0])
        headers = {}
        for line in lines[1:]:
            name, _, value = line.partition(":")
            headers[name.strip().lower()] = value.strip()
        if headers.get("sec-websocket-accept") != ws_accept_key(key):
            raise WebSocketError("bad Sec-WebSocket-Accept")
        self.sock = sock
        self.buffer = rest

    def _read(self, count: int, deadline: float) -> bytes:
        assert self.sock is not None
        while len(self.buffer) < count:
            remaining = deadline - time.time()
            if remaining <= 0:
                raise WebSocketError("timed out waiting for data")
            self.sock.settimeout(remaining)
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout:
                raise WebSocketError("timed out waiting for data")
            if not chunk:
                raise WebSocketError("connection closed")
            self.buffer += chunk
        data, self.buffer = self.buffer[:count], self.buffer[count:]
        return data

    def send_text(self, text: str) -> None:
        if self.sock is None:
            raise WebSocketError("not connected")
        self.sock.sendall(ws_encode_frame(text.encode("utf-8"), opcode=0x1, mask=True))

    def recv_text(self, timeout: float = 60.0) -> str:
        deadline = time.time() + timeout
        message = b""
        started = False
        while True:
            first = self._read(2, deadline)
            fin, opcode = bool(first[0] & 0x80), first[0] & 0x0F
            masked, length = bool(first[1] & 0x80), first[1] & 0x7F
            if length == 126:
                length = struct.unpack(">H", self._read(2, deadline))[0]
            elif length == 127:
                length = struct.unpack(">Q", self._read(8, deadline))[0]
            key = self._read(4, deadline) if masked else b""
            payload = self._read(length, deadline) if length else b""
            if masked:
                payload = bytes(byte ^ key[index % 4] for index, byte in enumerate(payload))
            if opcode == 0x8:
                raise WebSocketError("the server closed the websocket")
            if opcode == 0x9:
                assert self.sock is not None
                self.sock.sendall(ws_encode_frame(payload, opcode=0xA, mask=True))
                continue
            if opcode == 0xA:
                continue
            if opcode in (0x1, 0x2):
                message, started = payload, True
            elif opcode == 0x0 and started:
                message += payload
            else:
                raise WebSocketError("unexpected opcode %d" % opcode)
            if fin:
                return message.decode("utf-8", "replace")

    def close(self) -> None:
        if self.sock is not None:
            try:
                self.sock.sendall(ws_encode_frame(b"", opcode=0x8, mask=True))
            except OSError:
                pass
            try:
                self.sock.close()
            except OSError:
                pass
            self.sock = None


def trigger_expression() -> str:
    """JavaScript evaluated in the page: what the Install button's `check()` calls, with the outcome captured as plain data."""
    return (
        "(async () => { try { "
        "const inv = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke; "
        "if (typeof inv !== 'function') { return {ok: false, stage: 'no-ipc', error: 'window.__TAURI_INTERNALS__.invoke is not available'}; } "
        "const value = await inv('plugin:updater|check', {}); "
        "return {ok: true, value: (value === undefined ? null : value)}; "
        "} catch (e) { "
        "let extra = null; try { extra = (typeof e === 'object' && e !== null) ? JSON.stringify(e, Object.getOwnPropertyNames(e)) : null; } catch (_) {} "
        "return {ok: false, stage: 'invoke', error: String(e), error_type: typeof e, error_json: extra}; } })()"
    )


def ipc_probe_expression() -> str:
    """Is the Tauri IPC present, where is the page, and which version does the RUNNING app report (plugin:app|version)."""
    return ("(async () => { const inv = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke; "
            "let version = null, version_error = null; "
            "if (typeof inv === 'function') { try { version = await inv('plugin:app|version', {}); } catch (e) { version_error = String(e); } } "
            "return {ipc: typeof inv === 'function', href: String(location.href), title: String(document.title), app_version: version, app_version_error: version_error}; })()")


def pick_page_target(targets: Any) -> Optional[dict]:
    """The page target of the app window from the DevTools /json list."""
    if not isinstance(targets, list):
        return None
    pages = [t for t in targets if isinstance(t, dict) and t.get("type") == "page" and isinstance(t.get("webSocketDebuggerUrl"), str)]
    for target in pages:
        if re.match(r"(?i)^(https?://tauri\.localhost|tauri://localhost)", str(target.get("url"))):
            return target
    for target in pages:
        if not re.match(r"(?i)^(about:|devtools:|chrome|edge)", str(target.get("url"))):
            return target
    return None


def parse_ws_url(url: str) -> Tuple[str, int, str]:
    match = re.match(r"^ws://([^/:]+):(\d+)(/.*)$", url)
    if not match:
        raise WebSocketError("not a ws:// URL: %r" % url)
    return match.group(1), int(match.group(2)), match.group(3)


def cdp_result(message: dict) -> Tuple[bool, Any, Optional[str]]:
    """(ok, value, error text) of a Runtime.evaluate response."""
    if not isinstance(message, dict):
        return False, None, "not an object"
    if "error" in message:
        error = message["error"]
        return False, None, str(error.get("message") if isinstance(error, dict) else error)
    result = message.get("result") or {}
    details = result.get("exceptionDetails")
    if details:
        exception = details.get("exception") or {}
        return False, None, str(exception.get("description") or details.get("text") or "exception")
    inner = result.get("result") or {}
    return True, inner.get("value"), None


def cdp_evaluate(ws: WebSocketClient, expression: str, call_id: int, timeout: float = 60.0) -> dict:
    ws.send_text(json.dumps({"id": call_id, "method": "Runtime.evaluate",
                             "params": {"expression": expression, "awaitPromise": True, "returnByValue": True, "userGesture": True}}))
    deadline = time.time() + timeout
    while True:
        remaining = deadline - time.time()
        if remaining <= 0:
            raise WebSocketError("no response to Runtime.evaluate within %.0f s" % timeout)
        message = json.loads(ws.recv_text(timeout=remaining))
        if message.get("id") == call_id:
            return message


def trigger_outcome_from(value: Any, error: Optional[str], cdp_ok: bool) -> Dict[str, Any]:
    """Normalise what the page returned into the trigger record used by the criteria."""
    if not cdp_ok:
        return {"ran": False, "ok": False, "error_text": error, "stage": "cdp", "value": None}
    if not isinstance(value, dict):
        return {"ran": False, "ok": False, "error_text": "unexpected page result %r" % (value,), "stage": "page", "value": None}
    if value.get("stage") == "no-ipc":
        return {"ran": False, "ok": False, "error_text": value.get("error"), "stage": "no-ipc", "value": None}
    if value.get("ok"):
        return {"ran": True, "ok": True, "error_text": None, "stage": "invoke", "value": value.get("value")}
    return {"ran": True, "ok": False, "error_text": value.get("error"), "stage": "invoke", "error_type": value.get("error_type"),
            "error_json": value.get("error_json"), "value": None}


def http_get_json(url: str, timeout: float = 5.0) -> Any:
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))      # loopback: never through a proxy
    with opener.open(url, timeout=timeout) as response:
        return json.loads(response.read().decode("utf-8", "replace"))


# ---------------------------------------------------------------------------------------------------------------------
# provenance of the shipped plugin version, from the installed binary
# ---------------------------------------------------------------------------------------------------------------------

PIN_CRATES = {
    "tauri-plugin-updater": ["2.10.1"],
    "tauri": ["2.11.2"],
    "tauri-utils": ["2.9.2"],
    "minisign-verify": ["0.2.5"],
}


def crates_from_strings(data: bytes, crates: Iterable[str]) -> Dict[str, List[str]]:
    found: Dict[str, List[str]] = {}
    for crate in crates:
        pattern = rb"(?<![A-Za-z0-9_-])" + re.escape(crate.encode()) + rb"-([0-9]+\.[0-9]+\.[0-9]+)"
        found[crate] = sorted({m.group(1).decode() for m in re.finditer(pattern, data)})
    return found


def provenance_of_binary(path: Path, conf_pubkey: Optional[str]) -> Dict[str, Any]:
    data = Path(path).read_bytes()
    strings = crates_from_strings(data, PIN_CRATES)
    pubkey = extract_pubkey(data)
    matches = {crate: strings.get(crate) == versions for crate, versions in PIN_CRATES.items()}
    return {
        "binary": Path(path).name,
        "binary_sha256": sha256_bytes(data),
        "binary_size": len(data),
        "crates_from_binary_strings": strings,
        "pinned": PIN_CRATES,
        "matches_pin": matches,
        "plugin_version_from_binary": strings.get(PLUGIN_NAME, []),
        "updater_endpoint_string_count": data.count(MANIFEST_URL.encode()),
        "pubkey_in_binary": pubkey,
        "pubkey_in_tauri_conf": conf_pubkey,
        "pubkey_matches_conf": bool(pubkey and conf_pubkey and pubkey == conf_pubkey),
    }


# ---------------------------------------------------------------------------------------------------------------------
# the recording server process
# ---------------------------------------------------------------------------------------------------------------------

class MitmProcess:
    """lib/mitm_server.py as a child process (CLI contract: --scenario --cert --key --listen --log --ready-file)."""

    def __init__(self, scenario: str, cert: str, key: str, log_path: Path, ready_file: Path, listen: str = "127.0.0.1,::1:443", python: Optional[str] = None):
        self.scenario, self.cert, self.key = scenario, cert, key
        self.log_path, self.ready_file, self.listen = Path(log_path), Path(ready_file), listen
        self.python = python or sys.executable
        self.proc: Optional[subprocess.Popen] = None
        self.stderr_path = Path(str(log_path) + ".stderr")
        self._stderr_handle: Any = None

    def argv(self) -> List[str]:
        return [self.python, str(LIB / "mitm_server.py"), "--scenario", self.scenario, "--cert", self.cert, "--key", self.key,
                "--listen", self.listen, "--log", str(self.log_path), "--ready-file", str(self.ready_file)]

    def start(self, timeout: float = 30.0) -> None:
        self.log_path.parent.mkdir(parents=True, exist_ok=True)
        if self.ready_file.exists():
            self.ready_file.unlink()
        self._stderr_handle = self.stderr_path.open("wb")
        self.proc = subprocess.Popen(self.argv(), stdout=self._stderr_handle, stderr=subprocess.STDOUT)
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.ready_file.exists():
                return
            if self.proc.poll() is not None:
                break
            time.sleep(0.2)
        tail = self.stderr_path.read_text(encoding="utf-8", errors="replace")[-600:] if self.stderr_path.exists() else ""
        self.stop()
        raise RuntimeError("the recording server did not become ready (%s): %s" % (self.scenario, tail.strip()))

    def alive(self) -> bool:
        return self.proc is not None and self.proc.poll() is None

    def stop(self) -> None:
        if self.proc is not None:
            if self.proc.poll() is None:
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
                    self.proc.wait(timeout=10)
            self.proc = None
        if self._stderr_handle is not None:
            self._stderr_handle.close()
            self._stderr_handle = None


# ---------------------------------------------------------------------------------------------------------------------
# result assembly
# ---------------------------------------------------------------------------------------------------------------------

def runner_info() -> Dict[str, str]:
    return {
        "image": ("%s %s" % (os.environ.get("ImageOS", ""), os.environ.get("ImageVersion", ""))).strip() or "unknown",
        "arch": platform.machine() or "unknown",
        "os_version": platform.platform(),
    }


def build_result(*, runner: dict, app: dict, plugin: dict, tiers: Dict[str, dict], cases: List[dict], manifest404: Optional[str], manifest404_source: Optional[str],
                 isolation: dict, controls: List[dict], notes: List[str]) -> Dict[str, Any]:
    verdict = overall_verdict(tiers, cases)
    if app.get("digest_match") is False:
        verdict = "FAIL"            # what was downloaded is not the released asset: nothing that ran can stand for it
    return {
        "schema": SCHEMA_RESULT,
        "os": "windows",
        "runner": runner,
        "app": app,
        "plugin": plugin,
        "tiers": tiers,
        "cases": cases,
        "controls": controls,
        "manifest404_error_text": manifest404,
        "manifest404_error_source": manifest404_source,
        "isolation": isolation,
        "interception": {
            "covers": "the tauri-plugin-updater check / download / install path (it verifies TLS with the operating system trust store)",
            "not_covered": ["check_for_update (the 'update available' banner) and ensure_binary: the app's reqwest uses bundled webpki roots, "
                            "so their TLS handshake to the recording server fails (logged as tls_failure); covered by the cited source and the Stage A replica"],
        },
        "file_policy": FS_POLICY_DOC,
        "generated_at": now_iso(),
        "notes": notes,
        "verdict": verdict,
    }


def assert_no_private_keys(evidence: Path) -> List[str]:
    """Paths under the evidence directory that hold key material (must be empty before anything is uploaded)."""
    try:
        import ca as ca_module  # lib/ca.py
        return ca_module.scan_for_private_keys(evidence)
    except ImportError:
        leaks = []
        for current, _dirs, files in os.walk(str(evidence)):
            for name in files:
                full = os.path.join(current, name)
                try:
                    head = open(full, "rb").read(1 << 20)
                except OSError:
                    continue
                if re.search(rb"-----BEGIN (?:[A-Z0-9 ]+ )?PRIVATE KEY-----", head):
                    leaks.append(full)
        return leaks


# ---------------------------------------------------------------------------------------------------------------------
# Windows orchestration (everything below touches the machine; none of it runs in the offline tests except through fakes)
# ---------------------------------------------------------------------------------------------------------------------

class Context:
    """State shared by the steps of one `run`."""

    def __init__(self, evidence: Path, work: Path, shell: Shell, config: dict, env: Optional[Dict[str, str]] = None):
        self.evidence, self.work, self.shell, self.config = Path(evidence), Path(work), shell, config
        self.env = dict(env if env is not None else os.environ)
        self.install_dir = str(self.work / "app")
        self.ca_dir = self.work / "ca"
        self.ca_info: Optional[dict] = None
        self.thumbprint: Optional[str] = None
        self.hosts_original: Optional[str] = None
        self.hosts_mapped: List[str] = []
        self.hosts_blackholed: List[str] = []
        self.live_block = False
        self.rule_created = False           # the firewall rule exists, whether or not the connect test proved it effective
        self.live_block_detail: Dict[str, Any] = {}
        self.app_exe: Optional[str] = None
        self.notes: List[str] = []
        self.native_exe: Optional[Path] = None


def hosts_path() -> Path:
    return Path(os.environ.get("SystemRoot", r"C:\Windows")) / "System32" / "drivers" / "etc" / "hosts"


def write_hosts(path: Path, text: str) -> None:
    with open(str(path), "w", encoding="utf-8", newline="") as handle:     # newline="" keeps the CRLFs exactly as built
        handle.write(text)


def gh_release(shell: Shell, tag: str) -> dict:
    result = shell.run(["gh", "api", "repos/%s/releases/tags/%s" % (REPO, tag)], timeout=60)
    if not result.ok:
        raise AssetError("gh api release %s failed: %s" % (tag, result.out[-300:]))
    return json.loads(result.out)


def ensure_tag_source(ctx: Context) -> Dict[str, Any]:
    """The v0.7.11 tag's tauri.conf.json (pubkey, endpoint); fetches the tag first when the shallow checkout lacks it."""
    shell = ctx.shell
    probe = shell.run(["git", "-C", str(ROOT), "rev-parse", "-q", "--verify", "refs/tags/%s" % TAG], quiet=True)
    if not probe.ok:
        fetched = shell.run(["git", "-C", str(ROOT), "fetch", "--no-tags", "--depth=1", "origin", "refs/tags/%s:refs/tags/%s" % (TAG, TAG)], timeout=180)
        if not fetched.ok:
            return {"error": "cannot fetch the %s tag: %s" % (TAG, fetched.out[-200:])}
    shown = shell.run(["git", "-C", str(ROOT), "show", "%s:desktop/src-tauri/tauri.conf.json" % TAG], timeout=60)
    if not shown.ok:
        return {"error": "cannot read tauri.conf.json at %s: %s" % (TAG, shown.out[-200:])}
    try:
        return parse_tauri_conf(shown.out)
    except ValueError as error:
        return {"error": "tauri.conf.json is not JSON: %s" % error}


def download_installer(ctx: Context, asset: dict, dest: Path) -> Tuple[str, List[str]]:
    dest.parent.mkdir(parents=True, exist_ok=True)
    got = ctx.shell.run(["curl.exe", "-fL", "--proto", "=https", "--tlsv1.2", "--retry", "3", "-o", str(dest), asset["url"]], timeout=600)
    if not got.ok or not dest.is_file():
        return "", ["download failed (rc %d): %s" % (got.rc, got.out[-200:])]
    computed = sha256_file(dest)
    pin = find_pin(ctx.config, asset["name"])
    return computed, digest_problems(asset, computed, pin)


def nsis_install_argv(installer: str, install_dir: str) -> List[str]:
    # /D must be the last argument and must not be quoted, even if the path had spaces (it has none)
    return [installer, "/S", "/D=" + install_dir]


def msi_install_argv(installer: str, install_dir: str, log_path: str) -> List[str]:
    return ["msiexec", "/i", installer, "/qn", "/norestart", "INSTALLDIR=" + install_dir, "/L*v", log_path]


def discover_app_exe(candidates: Sequence[str], exists: Callable[[str], bool], listdir: Callable[[str], List[str]]) -> Optional[str]:
    """Where the installer put arc-desktop.exe: the candidate directories in order, then any non-uninstall exe in them."""
    for directory in candidates:
        exe = directory.rstrip("\\") + "\\" + APP_EXE_NAME
        if exists(exe):
            return exe
    for directory in candidates:
        try:
            names = listdir(directory)
        except OSError:
            continue
        for name in sorted(names):
            if name.lower().endswith(".exe") and not re.search(r"(?i)unins|uninstall|setup", name):
                return directory.rstrip("\\") + "\\" + name
    return None


def install_app(ctx: Context, asset: dict, installer: Path, exists: Callable[[str], bool] = os.path.exists, listdir: Callable[[str], List[str]] = os.listdir,
                sleep: Callable[[float], None] = time.sleep, clock: Callable[[], float] = time.time, wait_s: float = 120.0) -> Dict[str, Any]:
    shell = ctx.shell
    if asset["kind"] == "nsis":
        argv = nsis_install_argv(str(installer), ctx.install_dir)
    else:
        argv = msi_install_argv(str(installer), ctx.install_dir, str(ctx.evidence / "msi-install.log"))
    done = shell.run(argv, timeout=600, capture=False)
    local = ctx.env.get("LOCALAPPDATA", "")
    pf = ctx.env.get("ProgramFiles", r"C:\Program Files")
    candidates = [ctx.install_dir, local + "\\ARC Node", pf + "\\ARC Node"]
    deadline = clock() + wait_s
    exe = None
    while True:
        exe = discover_app_exe(candidates, exists, listdir)
        if exe or clock() >= deadline:
            break
        sleep(3)
    uninstall = shell.run(["reg", "query", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall", "/s", "/f", "ARC Node"], timeout=60)
    detail = {"installer_rc": done.rc, "kind": asset["kind"], "install_dir_requested": ctx.install_dir, "exe": exe, "candidates": candidates,
              "uninstall_registry": mask_ips(uninstall.out[-800:])}
    if exe:
        ctx.app_exe = exe
        ctx.install_dir = exe.rsplit("\\", 1)[0]
    return detail


def trust_ca(ctx: Context) -> None:
    ca_crt = Path(ctx.ca_info["ca_cert"])
    ctx.thumbprint = thumbprint_sha1(ca_crt.read_text(encoding="ascii"))
    added = ctx.shell.run(certutil_add_argv(str(ca_crt)), timeout=60)
    check = ctx.shell.run(powershell_argv(root_store_check_script(ctx.thumbprint)), timeout=60)
    if not added.ok or check.out.strip() != "1":
        raise RuntimeError("the CA is not in the machine Root store (certutil rc %d, store count %r)" % (added.rc, check.out.strip()))


def map_hosts(ctx: Context, names: Sequence[str]) -> None:
    path = hosts_path()
    with open(str(path), "r", encoding="utf-8", errors="replace", newline="") as handle:
        ctx.hosts_original = handle.read()
    blackholed = blackhole_names()
    write_hosts(path, hosts_with_block(ctx.hosts_original, hosts_block(names, blackholed)))
    ctx.hosts_mapped = list(names)
    ctx.hosts_blackholed = blackholed
    ctx.shell.run(["ipconfig", "/flushdns"], timeout=30, quiet=True)


def block_live_network(ctx: Context) -> None:
    ips = load_live_ips(ROOT)
    shell = ctx.shell
    created = shell.run(powershell_argv(firewall_block_script(ips)), timeout=120)
    ctx.rule_created = created.ok
    verify = shell.run(powershell_argv(firewall_verify_script(ips[0])), timeout=60)
    listed = shell.run(powershell_argv(firewall_list_script()), timeout=60)
    ctx.live_block = created.ok and verify.rc == 0 and "REACHABLE" not in verify.out
    ctx.live_block_detail = {
        "rule": FIREWALL_RULE, "addresses": len(ips), "addresses_sha256": sha256_bytes(("\n".join(sorted(ips)) + "\n").encode()),
        "created": created.ok, "connect_test_rc": verify.rc, "connect_test": mask_ips(verify.out.strip()[-200:]), "rule_listing": mask_ips(listed.out.strip()[-300:]),
    }
    if not ctx.live_block:
        raise RuntimeError("the live network is not blocked: %s" % ctx.live_block_detail)


def snapshot_processes(shell: Shell) -> Optional[List[dict]]:
    out = shell.run(powershell_argv(PS_PROCESSES), timeout=120, quiet=True)
    return parse_processes(out.out) if out.ok else None


def sandbox_env(base: Dict[str, str], home: str, wv2: str, port: int = CDP_PORT) -> Dict[str, str]:
    env = dict(base)
    for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
        env.pop(name, None)
    env["HOME"] = home
    env["USERPROFILE"] = home
    env["WEBVIEW2_USER_DATA_FOLDER"] = wv2
    env["WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS"] = "--remote-debugging-port=%d --remote-allow-origins=*" % port
    return env


def wait_for_page(port: int, timeout: float, fetch: Callable[[str], Any] = http_get_json, sleep: Callable[[float], None] = time.sleep, clock: Callable[[], float] = time.time) -> Tuple[Optional[dict], str]:
    deadline = clock() + timeout
    last = "no answer"
    while clock() < deadline:
        try:
            target = pick_page_target(fetch("http://127.0.0.1:%d/json" % port))
            if target:
                return target, "ok"
            last = "no page target yet"
        except (OSError, ValueError, urllib.error.URLError) as error:
            last = "%s: %s" % (type(error).__name__, error)
        sleep(1.5)
    return None, last


def kill_app(ctx: Context, pid: Optional[int], wv2_dir: str) -> None:
    shell = ctx.shell
    if pid:
        shell.run(["taskkill", "/F", "/T", "/PID", str(pid)], timeout=60, quiet=True)
    shell.run(["taskkill", "/F", "/T", "/IM", APP_EXE_NAME], timeout=60, quiet=True)
    sweep = ("Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'msedgewebview2.exe' -and $_.CommandLine -like '*%s*' } "
             "| ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }" % wv2_dir.replace("'", "''"))
    shell.run(powershell_argv(sweep), timeout=60, quiet=True)


def case_summary(requests: Dict[str, Any], classification: Optional[Dict[str, Any]], install_changes: Optional[List[str]], cycles: int) -> Dict[str, Any]:
    return {
        "requests": {"total": requests["total"], "by_host_path": requests["by_host_path"], "tls_failures": requests["tls_failures"], "classes": requests["classes"]},
        "file_changes": {"expected": len((classification or {}).get("expected", [])), "noise": len((classification or {}).get("noise", [])),
                         "unexpected": (classification or {}).get("unexpected", []), "install_dir_changes": install_changes, "poller_cycles": cycles},
    }


def judge_recorded_case(scenario: str, trigger: Dict[str, Any], requests_path: Path, writes_path: Path, before: Optional[Dict[str, dict]], after: Dict[str, dict],
                        policies: Sequence[Dict[str, str]], install_dir: str, procs_before: Optional[List[dict]], procs_after: Optional[List[dict]],
                        app_pid: Optional[int], markers: Sequence[str], ignore_exes: Sequence[str], cycles: int, min_cycles: int, diff_path: Optional[Path] = None) -> Dict[str, Any]:
    """Everything a case recorded -> the case fields (requests, file_changes, criteria, verdict, reasons, process_changes)."""
    delta = diff_snapshots(before, after) if before is not None else None
    classification = classify_fs_changes(delta, policies) if delta is not None else None
    if delta is not None and diff_path is not None:
        write_json(diff_path, delta)
    install_changes = [p for kind in ("added", "changed", "removed") for p in delta[kind] if path_under(p, install_dir)] if delta is not None else None
    write_records = read_jsonl(writes_path)
    writes_ok = bool(write_records) and cycles >= min_cycles and any(r.get("event") == "stop" for r in write_records)
    requests = summarize_requests(read_jsonl(requests_path))
    violations = process_violations(procs_before, procs_after, app_pid, markers, ignore_exes)
    judgement = evaluate_case(scenario, trigger, requests, classification, violations, writes_ok, install_changes)
    fields = case_summary(requests, classification, install_changes, cycles)
    fields.update(criteria=judgement["criteria"], verdict=judgement["verdict"], reasons=judgement["reasons"], process_changes=violations)
    return fields


def mask_file(src: Path, dest: Path) -> None:
    """Copy a text log with IPv4 addresses masked (the app prints the seed addresses it cannot reach)."""
    try:
        dest.write_text(mask_ips(Path(src).read_text(encoding="utf-8", errors="replace")), encoding="utf-8")
    except OSError:
        pass


def run_released_case(ctx: Context, name: str, state: Dict[str, str], settle_s: float = 10.0) -> Dict[str, Any]:
    """One released-app case: record, launch, trigger over CDP, record again, evaluate."""
    shell, evidence = ctx.shell, ctx.evidence
    scenario = CASE_SCENARIO[name]
    tag = "app-" + name
    files: List[str] = []
    case: Dict[str, Any] = {"name": name, "tier": "released_app", "scenario": scenario, "trigger_outcome": {}, "evidence_files": files}
    requests_path = evidence / ("requests-%s.jsonl" % tag)
    writes_path = evidence / ("writes-%s.jsonl" % tag)
    raw_log = ctx.work / ("app-%s.raw.log" % tag)
    home, wv2 = state["home"], state["wv2"]
    policies = fs_policies(home, wv2, ctx.install_dir, ctx.env)
    roots = fs_watch_roots(policies)
    mitm = MitmProcess(scenario, ctx.ca_info["server_cert"], ctx.ca_info["server_key"], requests_path, evidence / ("ready-%s" % tag))
    poller = Poller(roots, writes_path)
    pid: Optional[int] = None
    trigger: Dict[str, Any] = {"ran": False, "ok": False, "error_text": "not reached", "stage": "setup", "value": None}
    procs_before = procs_after = None
    before_snapshot: Optional[Dict[str, dict]] = None
    cycles = 0
    app_log = None
    try:
        mitm.start()
        before_snapshot = take_snapshot(roots)
        write_json(evidence / ("fs-%s-before.json" % tag), before_snapshot)
        prelaunch = snapshot_processes(shell)
        write_process_list(evidence / ("procs-%s-prelaunch.txt" % tag), prelaunch)
        poller.start()
        env = sandbox_env(ctx.env, home, wv2)
        raw_log.parent.mkdir(parents=True, exist_ok=True)
        app_log = raw_log.open("wb")
        proc = subprocess.Popen([ctx.app_exe], env=env, cwd=ctx.install_dir, stdout=app_log, stderr=subprocess.STDOUT)
        pid = proc.pid
        shell.log("launched %s pid=%d case=%s" % (ctx.app_exe, pid, tag))
        target, why = wait_for_page(CDP_PORT, 120.0)
        if target is None:
            trigger = {"ran": False, "ok": False, "error_text": "the DevTools page target never appeared (%s); app exit code %s" % (why, proc.poll()), "stage": "cdp", "value": None}
        else:
            time.sleep(5)       # let the page finish its first load before the call
            procs_before = snapshot_processes(shell)
            write_process_list(evidence / ("procs-%s-before.txt" % tag), procs_before)
            host, port, path = parse_ws_url(target["webSocketDebuggerUrl"])
            ws = WebSocketClient(host, port, path)
            try:
                ws.connect()
                ok, value, error = cdp_result(cdp_evaluate(ws, ipc_probe_expression(), 1, timeout=30))
                case["page_probe"] = {"ok": ok, "value": value, "error": error, "target_url": target.get("url")}
                poller.mark("trigger_begin")
                ok, value, error = cdp_result(cdp_evaluate(ws, trigger_expression(), 2, timeout=90))
                poller.mark("trigger_end")
                trigger = trigger_outcome_from(value, error, ok)
            except (WebSocketError, OSError, ValueError) as error:
                trigger = {"ran": False, "ok": False, "error_text": "%s: %s" % (type(error).__name__, error), "stage": "cdp", "value": None}
            finally:
                ws.close()
            time.sleep(settle_s)
            procs_after = snapshot_processes(shell)
            write_process_list(evidence / ("procs-%s-after.txt" % tag), procs_after)
    except Exception as error:  # noqa: BLE001 - the case ends UNPROVED with the reason, the run goes on
        trigger = {"ran": False, "ok": False, "error_text": "%s: %s" % (type(error).__name__, error), "stage": "setup", "value": None}
        shell.log("case %s crashed: %s: %s" % (tag, type(error).__name__, error))
    finally:
        if poller.thread is not None:
            cycles = poller.stop()
        after_snapshot = take_snapshot(roots)
        write_json(evidence / ("fs-%s-after.json" % tag), after_snapshot)
        kill_app(ctx, pid, wv2)
        mitm.stop()
        if app_log is not None:
            app_log.close()
        mask_file(raw_log, evidence / ("app-%s.log" % tag))
    case["trigger_outcome"] = trigger
    case.update(judge_recorded_case(scenario, trigger, requests_path, writes_path, before_snapshot, after_snapshot, policies, ctx.install_dir, procs_before, procs_after, pid,
                                    [ctx.install_dir, home, "network.arc.desktop", "ARC.Node_"], [], cycles, 3, evidence / ("fs-%s-diff.json" % tag)))
    for pattern in ("requests-%s.jsonl", "writes-%s.jsonl", "fs-%s-before.json", "fs-%s-after.json", "fs-%s-diff.json", "procs-%s-prelaunch.txt", "procs-%s-before.txt",
                    "procs-%s-after.txt", "app-%s.log"):
        if (evidence / (pattern % tag)).is_file():
            files.append(pattern % tag)
    return case


def find_native_exe(explicit: Optional[str] = None, env: Optional[Dict[str, str]] = None,
                    exists: Callable[[str], bool] = os.path.isfile) -> Tuple[Optional[str], List[str]]:
    """The native-updater-check.exe the workflow's build step produced (or one named explicitly). Returns (path, candidates tried)."""
    env = os.environ if env is None else env
    candidates = [explicit, env.get("NATIVE_UPDATER_CHECK"), str(HERE / "native-updater-check" / "target" / "release" / NATIVE_EXE_NAME),
                  str(Path(env.get("CARGO_TARGET_DIR", "")) / "release" / NATIVE_EXE_NAME) if env.get("CARGO_TARGET_DIR") else None]
    tried = [c for c in candidates if c]
    for candidate in tried:
        if exists(candidate):
            return candidate, tried
    return None, tried


def parse_native_json(text: str) -> Optional[dict]:
    for line in reversed(text.strip().splitlines()):
        line = line.strip()
        if line.startswith("{"):
            try:
                value = json.loads(line)
            except ValueError:
                continue
            if isinstance(value, dict) and value.get("schema", "").startswith("arc.legacy-bridge.wave0-lab.native-updater-check"):
                return value
    return None


def native_trigger(report: Optional[dict], rc: int, scenario: str) -> Dict[str, Any]:
    """Map the checker's JSON line onto the trigger record used by evaluate_case."""
    if report is None or rc != 0:
        return {"ran": False, "ok": False, "error_text": "native-updater-check exit %s without a JSON report" % rc, "stage": "native", "value": None}
    if report.get("outcome") == "error":
        return {"ran": True, "ok": False, "error_text": report.get("error"), "error_kind": report.get("error_kind"), "stage": "native", "value": None}
    if report.get("outcome") == "update_available":
        update = report.get("update") or {}
        return {"ran": True, "ok": True, "error_text": None, "stage": "native", "value": {"version": update.get("version"), "download_url": update.get("download_url")},
                "download_attempted": bool(report.get("download_attempted"))}
    return {"ran": True, "ok": True, "error_text": None, "stage": "native", "value": None}


def run_native_case(ctx: Context, name: str, exe: Path, pubkey: str, control: bool = False) -> Dict[str, Any]:
    shell, evidence = ctx.shell, ctx.evidence
    scenario = CASE_SCENARIO[name] if not control else "bait-0.8.11"
    tag = "native-" + ("control" if control else name)
    files: List[str] = []
    case: Dict[str, Any] = {"name": name, "tier": "native_check", "scenario": scenario, "evidence_files": files}
    requests_path = evidence / ("requests-%s.jsonl" % tag)
    writes_path = evidence / ("writes-%s.jsonl" % tag)
    home = str(ctx.work / "native-home" / tag)
    Path(home).mkdir(parents=True, exist_ok=True)
    wv2 = str(ctx.work / "native-wv2")
    policies = fs_policies(home, wv2, ctx.install_dir, ctx.env)
    roots = fs_watch_roots(policies)
    mitm = MitmProcess(scenario, ctx.ca_info["server_cert"], ctx.ca_info["server_key"], requests_path, evidence / ("ready-%s" % tag))
    poller = Poller(roots, writes_path)
    trigger: Dict[str, Any] = {"ran": False, "ok": False, "error_text": "not reached", "stage": "setup", "value": None}
    report = None
    before: Optional[Dict[str, dict]] = None
    procs_before = procs_after = None
    cycles = 0
    report_name = "native-%s.json" % ("control" if control else name)
    try:
        mitm.start()
        before = take_snapshot(roots)
        write_json(evidence / ("fs-%s-before.json" % tag), before)
        procs_before = snapshot_processes(shell)
        write_process_list(evidence / ("procs-%s-before.txt" % tag), procs_before)
        poller.start()
        argv = [str(exe), "--endpoint", MANIFEST_URL, "--current-version", TAG.lstrip("v"), "--pubkey", pubkey]
        if control:
            argv.append("--control-download")
        poller.mark("trigger_begin")
        done = shell.run(argv, timeout=180, env=sandbox_env(ctx.env, home, wv2))
        poller.mark("trigger_end")
        report = parse_native_json(done.out)
        trigger = native_trigger(report, done.rc, scenario)
        (evidence / report_name).write_text(json.dumps(report, indent=2, sort_keys=True) + "\n" if report else mask_ips(done.out), encoding="utf-8")
        time.sleep(3)
        procs_after = snapshot_processes(shell)
        write_process_list(evidence / ("procs-%s-after.txt" % tag), procs_after)
    except Exception as error:  # noqa: BLE001
        trigger = {"ran": False, "ok": False, "error_text": "%s: %s" % (type(error).__name__, error), "stage": "setup", "value": None}
        shell.log("native case %s crashed: %s: %s" % (tag, type(error).__name__, error))
    finally:
        if poller.thread is not None:
            cycles = poller.stop()
        after = take_snapshot(roots)
        write_json(evidence / ("fs-%s-after.json" % tag), after)
        mitm.stop()
    case["trigger_outcome"] = trigger
    case["native_report"] = report
    case.update(judge_recorded_case(scenario, trigger, requests_path, writes_path, before, after, policies, ctx.install_dir, procs_before, procs_after, os.getpid(), [],
                                    [str(exe)], cycles, 2, evidence / ("fs-%s-diff.json" % tag)))
    for pattern in ("requests-%s.jsonl", "writes-%s.jsonl", "fs-%s-before.json", "fs-%s-after.json", "fs-%s-diff.json", "procs-%s-before.txt", "procs-%s-after.txt"):
        if (evidence / (pattern % tag)).is_file():
            files.append(pattern % tag)
    if (evidence / report_name).is_file():
        files.append(report_name)
    return case


def control_judgement(case: dict) -> Dict[str, Any]:
    """The positive control passes when the harness SAW the bundle request the control download made."""
    classes = (case.get("requests") or {}).get("classes", {})
    saw = bool(classes.get("payload"))
    report = case.get("native_report") or {}
    return {"name": "native-download-control", "seen_by_harness": saw, "download_attempted": report.get("download_attempted"),
            "download_result": report.get("download_result"), "payload_requests": classes.get("payload", []),
            "verdict": "PASS" if saw and report.get("download_attempted") else "FAIL" if report.get("download_attempted") and not saw else "UNPROVED"}


# ---------------------------------------------------------------------------------------------------------------------
# subcommands
# ---------------------------------------------------------------------------------------------------------------------

def load_config(path: Optional[Path]) -> dict:
    candidate = Path(path) if path else HERE / "config.json"
    if candidate.is_file():
        try:
            value = json.loads(candidate.read_text(encoding="utf-8"))
            return value if isinstance(value, dict) else {}
        except ValueError:
            return {}
    return {}


def cleanup(ctx: Context) -> None:
    shell = ctx.shell
    if ctx.hosts_original is not None:
        try:
            write_hosts(hosts_path(), ctx.hosts_original)
            shell.log("hosts file restored")
        except OSError as error:
            shell.log("hosts file NOT restored: %s" % error)
    shell.run(["ipconfig", "/flushdns"], timeout=30, quiet=True)
    if ctx.thumbprint:
        shell.run(certutil_del_argv(ctx.thumbprint), timeout=60)
    if ctx.live_block or ctx.rule_created:
        shell.run(powershell_argv(firewall_remove_script()), timeout=60)


def prepare_isolation(ctx: Context, isolation: Dict[str, Any]) -> None:
    """CA, live-network block, trust store, hosts file. Raises when any of them cannot be shown to be in place (nothing runs without it)."""
    from ca import make_ca, DEFAULT_HOSTS, copy_public  # lib/ca.py
    ctx.ca_info = make_ca(ctx.ca_dir, DEFAULT_HOSTS)
    copy_public(ctx.ca_dir, ctx.evidence)                      # ONLY ca.crt and ca.sha256
    isolation["ca_sha256"] = ctx.ca_info["ca_sha256"]
    block_live_network(ctx)
    isolation["live_block"] = ctx.live_block
    isolation["live_block_detail"] = ctx.live_block_detail
    trust_ca(ctx)
    isolation["root_store_thumbprint_sha1"] = ctx.thumbprint
    map_hosts(ctx, list(ctx.ca_info["hostnames"]))
    isolation["hosts_mapped"] = list(ctx.hosts_mapped)
    isolation["hosts_blackholed"] = list(ctx.hosts_blackholed)


def released_app_tier(ctx: Context, asset: dict, installer: Path, pubkey: Optional[str], cases_wanted: Sequence[str], app: Dict[str, Any], plugin: Dict[str, Any],
                      notes: List[str]) -> Tuple[Dict[str, Any], List[dict], Optional[str], Optional[str]]:
    """(tier record, cases, pubkey to use for the native tier, manifest-404 text)."""
    tier = {"attempted": True, "status": "ran", "result": "UNPROVED", "reason": "", "trigger": "WebView2 DevTools Protocol -> plugin:updater|check (Tauri IPC)"}
    install = install_app(ctx, asset, installer)
    write_json(ctx.evidence / "install.json", install)
    if not ctx.app_exe:
        tier.update(status="infeasible", reason="the installer left no arc-desktop.exe (see install.json and steps.log)")
        return tier, [], pubkey, None
    provenance = provenance_of_binary(Path(ctx.app_exe), pubkey)
    write_json(ctx.evidence / "provenance.json", provenance)
    plugin["provenance"] = [provenance]
    plugin["version"] = ",".join(provenance["plugin_version_from_binary"]) or None
    pubkey = pubkey or provenance.get("pubkey_in_binary")
    state = {"home": str(ctx.work / "home"), "wv2": str(ctx.work / "wv2")}
    Path(state["home"]).mkdir(parents=True, exist_ok=True)
    Path(state["wv2"]).mkdir(parents=True, exist_ok=True)
    ordered = [name for name in CASE_NAMES if name in cases_wanted]
    if "cached-bait" in ordered and "clean" not in ordered:
        notes.append("cached-bait requested without clean: the first launch's state was created by an unreported warm-up case")
        run_released_case(ctx, "clean", state)
    cases = [run_released_case(ctx, name, state) for name in ordered]
    tier["result"] = tier_result(cases_wanted, cases, "ran")
    clean = next((c for c in cases if c["name"] == "clean"), None)
    if clean:
        reported = (((clean.get("page_probe") or {}).get("value")) or {}).get("app_version")
        app["version_reported"] = reported if isinstance(reported, str) else None
        if isinstance(reported, str) and reported != TAG.lstrip("v"):
            notes.append("the running app reports version %s, not %s" % (reported, TAG.lstrip("v")))
    text = clean["trigger_outcome"].get("error_text") if clean else None
    return tier, cases, pubkey, text if isinstance(text, str) and text else None


def binary_info(path: Path) -> Dict[str, Any]:
    try:
        return {"path": str(path), "sha256": sha256_file(Path(path)), "size": Path(path).stat().st_size}
    except OSError as error:
        return {"path": str(path), "error": "%s: %s" % (type(error).__name__, error)}


def native_check_tier(ctx: Context, pubkey: Optional[str], cases_wanted: Sequence[str], explicit_exe: Optional[str] = None) -> Tuple[Dict[str, Any], List[dict], List[dict], Optional[str]]:
    """(tier record, cases, controls, manifest-404 text)."""
    tier = {"attempted": True, "status": "ran", "result": "UNPROVED", "reason": "",
            "trigger": "native-updater-check (tauri-plugin-updater 2.10.1 check() in tauri's mock runtime, OS trust store)"}
    found, tried = find_native_exe(explicit_exe, ctx.env)
    if found is None:
        tier.update(status="infeasible", reason="no pre-built %s (the workflow's cargo build step failed or was skipped); looked at %s" % (NATIVE_EXE_NAME, tried))
        return tier, [], [], None
    if not pubkey:
        tier.update(status="infeasible", reason="no updater public key could be read (tauri.conf.json at %s, installed binary)" % TAG)
        return tier, [], [], None
    exe = Path(found)
    ctx.native_exe = exe
    tier["binary"] = binary_info(exe)
    cases = [run_native_case(ctx, name, exe, pubkey) for name in CASE_NAMES if name in cases_wanted]
    controls = [control_judgement(run_native_case(ctx, "cached-bait", exe, pubkey, control=True))]
    tier["result"] = tier_result(cases_wanted, cases, "ran")
    clean = next((c for c in cases if c["name"] == "clean"), None)
    text = clean["trigger_outcome"].get("error_text") if clean else None
    return tier, cases, controls, text if isinstance(text, str) and text else None


SHARED_EVIDENCE = ("ca.crt", "ca.sha256", "provenance.json", "install.json", "isolation.json", "manifest404-error.txt", "steps.log")


def cmd_run(args: argparse.Namespace) -> int:
    evidence = Path(args.evidence)
    evidence.mkdir(parents=True, exist_ok=True)
    shell = Shell(evidence / "steps.log")
    work = Path(args.work) if args.work else Path(os.environ.get("ARCW0_WORK", r"C:\arcw0" if IS_WINDOWS else str(evidence / "work")))
    ctx = Context(evidence, work, shell, load_config(args.config))
    wanted_tiers = list(TIERS) if args.tier == "both" else [args.tier]
    cases_wanted = [name for name in args.cases.split(",") if name]
    for name in cases_wanted:
        if name not in CASE_NAMES:
            print("unknown case %r" % name, file=sys.stderr)
            return 2
    notes: List[str] = []
    tiers: Dict[str, dict] = {t: {"attempted": False, "status": "not_attempted", "result": "UNPROVED", "reason": "not requested", "trigger": None} for t in TIERS}
    cases: List[dict] = []
    controls: List[dict] = []
    app: Dict[str, Any] = {"tag": TAG, "asset": None, "asset_sha256": None, "release_digest": None, "digest_match": None, "version_reported": None}
    plugin: Dict[str, Any] = {"name": PLUGIN_NAME, "version": None, "provenance": []}
    isolation: Dict[str, Any] = {"hosts_mapped": [], "ca_sha256": None, "live_block": False}
    manifest404: Optional[str] = None
    manifest404_source: Optional[str] = None
    pubkey: Optional[str] = None

    def fail_tiers(which: Sequence[str], reason: str) -> None:
        for tier in which:
            if tiers[tier]["status"] in ("not_attempted", "starting"):
                tiers[tier].update(attempted=True, status="infeasible", result="UNPROVED", reason=reason)

    try:
        if not IS_WINDOWS:
            raise RuntimeError("this job runs on Windows only (os.name=%s)" % os.name)
        for tier in wanted_tiers:
            tiers[tier].update(attempted=True, status="starting", reason="")
        # --- inputs, all read BEFORE the hosts file is touched ---------------------------------------------------------
        conf = ensure_tag_source(ctx)
        if "error" in conf:
            notes.append(conf["error"])
        else:
            pubkey = conf.get("pubkey")
            if conf.get("endpoints") != [MANIFEST_URL]:
                notes.append("tauri.conf.json endpoints differ from the expected single manifest URL: %r" % (conf.get("endpoints"),))
        release = gh_release(shell, TAG)
        asset = select_installer(release)
        app.update(asset=asset["name"], release_digest=asset["digest"])
        installer = work / "dl" / asset["name"]
        computed, problems = download_installer(ctx, asset, installer)
        app["asset_sha256"] = "sha256:" + computed if computed else None
        app["digest_match"] = (not problems) and bool(computed)
        if problems:
            raise AssetError("; ".join(problems))
        prepare_isolation(ctx, isolation)
    except Exception as error:  # noqa: BLE001 - evidence is still written, the verdict stays UNPROVED
        reason = "%s: %s" % (type(error).__name__, error)
        notes.append("setup stopped: " + reason)
        shell.log("setup stopped: " + reason)
        fail_tiers(wanted_tiers, reason)
    else:
        if "released_app" in wanted_tiers:
            try:
                tier, tier_cases, pubkey, text = released_app_tier(ctx, asset, installer, pubkey, cases_wanted, app, plugin, notes)
                tiers["released_app"].update(tier)
                cases.extend(tier_cases)
                if text:
                    manifest404, manifest404_source = text, "released_app"
            except Exception as error:  # noqa: BLE001
                reason = "%s: %s" % (type(error).__name__, error)
                shell.log("released_app tier stopped: " + reason)
                tiers["released_app"].update(status="infeasible", result="UNPROVED", reason=reason)
        if "native_check" in wanted_tiers:
            try:
                tier, tier_cases, tier_controls, text = native_check_tier(ctx, pubkey, cases_wanted, args.native_exe)
                tiers["native_check"].update(tier)
                cases.extend(tier_cases)
                controls.extend(tier_controls)
                if text and manifest404 is None:
                    manifest404, manifest404_source = text, "native_check"
                if plugin["version"] is None and tier["status"] == "ran":
                    plugin["version"] = PLUGIN_VERSION + " (native-check build, pinned; the released binary's own strings were not read)"
            except Exception as error:  # noqa: BLE001
                reason = "%s: %s" % (type(error).__name__, error)
                shell.log("native_check tier stopped: " + reason)
                tiers["native_check"].update(status="infeasible", result="UNPROVED", reason=reason)
    finally:
        cleanup(ctx)
    fail_tiers(wanted_tiers, "the tier did not complete")
    write_json(evidence / "isolation.json", isolation)
    result = build_result(runner=runner_info(), app=app, plugin=plugin, tiers=tiers, cases=cases, manifest404=manifest404, manifest404_source=manifest404_source,
                          isolation=isolation, controls=controls, notes=notes)
    if manifest404:
        (evidence / "manifest404-error.txt").write_text(manifest404 + "\n", encoding="utf-8")
    for case in cases:
        name = "trigger-%s-%s.json" % ("app" if case["tier"] == "released_app" else "native", case["name"])
        write_json(evidence / name, {"case": case["name"], "tier": case["tier"], "scenario": case["scenario"], "trigger_outcome": case.get("trigger_outcome"),
                                     "page_probe": case.get("page_probe")})
        case["evidence_files"].append(name)
    result["shared_evidence_files"] = [name for name in SHARED_EVIDENCE if (evidence / name).is_file()]
    leaks = assert_no_private_keys(evidence)
    if leaks:
        result["verdict"] = "FAIL"
        result["notes"].append("PRIVATE KEY MATERIAL under the evidence directory: %s" % [os.path.basename(path) for path in leaks])
    write_json(evidence / "result.json", result)
    print("verdict %s (tiers: %s)" % (result["verdict"], ", ".join("%s %s" % (t, tiers[t]["result"]) for t in TIERS)))
    return 0


CARGO_CHECK_TIMEOUT_S = 1200.0


def cargo_check_env(base: Dict[str, str], work: Path) -> Dict[str, str]:
    """RUSTUP_TOOLCHAIN=stable because the repository root pins a nightly (the crate needs >= 1.77.2); build output and temp files stay out of the watched directories."""
    env = dict(base)
    env["RUSTUP_TOOLCHAIN"] = "stable"
    env["CARGO_TARGET_DIR"] = str(work / "native-check-target")
    tmp = work / "native-check-tmp"
    tmp.mkdir(parents=True, exist_ok=True)
    env["TEMP"] = env["TMP"] = str(tmp)
    return env


def start_cargo_check(ctx: Context, popen: Callable[..., Any] = subprocess.Popen, clock: Callable[[], float] = time.time) -> Dict[str, Any]:
    """Start `cargo check --locked` of the native-updater-check crate in the background (the crate has never been compiled anywhere)."""
    crate = HERE / "native-updater-check"
    log_path = ctx.work / "cargo-check.log"
    argv = ["cargo", "check", "--locked", "--manifest-path", str(crate / "Cargo.toml")]
    job: Dict[str, Any] = {"argv": argv, "started": clock(), "log": log_path, "proc": None, "handle": None, "error": None}
    try:
        log_path.parent.mkdir(parents=True, exist_ok=True)
        job["handle"] = log_path.open("wb")
        job["proc"] = popen(argv, env=cargo_check_env(ctx.env, ctx.work), stdout=job["handle"], stderr=subprocess.STDOUT, cwd=str(crate))
    except OSError as error:
        job["error"] = "%s: %s" % (type(error).__name__, error)
    return job


def finish_cargo_check(job: Dict[str, Any], timeout: float = CARGO_CHECK_TIMEOUT_S, clock: Callable[[], float] = time.time) -> Dict[str, Any]:
    """Wait for the background check (at most `timeout` seconds after it started), kill it if it overruns, and describe the outcome."""
    proc = job.get("proc")
    result: Dict[str, Any] = {"command": " ".join(job["argv"]), "toolchain": "RUSTUP_TOOLCHAIN=stable", "rc": None, "timed_out": False, "elapsed_s": None, "tail": [], "error": job.get("error")}
    if proc is not None:
        remaining = max(1.0, timeout - (clock() - job["started"]))
        try:
            result["rc"] = proc.wait(timeout=remaining)
        except subprocess.TimeoutExpired:
            result["timed_out"] = True
            proc.kill()
            try:
                proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                pass
    result["elapsed_s"] = round(clock() - job["started"], 1)
    handle = job.get("handle")
    if handle is not None:
        handle.close()
    log_path = job.get("log")
    if log_path is not None and Path(log_path).is_file():
        lines = mask_ips(Path(log_path).read_text(encoding="utf-8", errors="replace")).splitlines()
        result["tail"] = lines[-40:]
        result["log_lines"] = len(lines)
        result["log_tail_file"] = "cargo-check.log"
        job["tail_text"] = "\n".join(lines[-400:]) + "\n"
    return result


def curl_selftest(shell: Shell, extra: Sequence[str] = ()) -> Dict[str, Any]:
    """One request to the manifest URL through the OS trust store (Schannel); the recording server names itself in a response header."""
    done = shell.run(["curl.exe", "-sS", "--max-time", "20", "-D", "-", "-o", "NUL", "-w", "\nHTTPCODE:%{http_code}\n"] + list(extra) + [MANIFEST_URL], timeout=60)
    text = done.out
    code = re.search(r"HTTPCODE:(\d+)", text)
    return {"rc": done.rc, "http_code": code.group(1) if code else None, "recording_server_header": bool(re.search(r"(?im)^server:\s*wave0-lab-mitm", text)),
            "output_tail": mask_ips(text.strip()[-400:])}


def native_selftest(shell: Shell, exe: str, pubkey: str) -> Dict[str, Any]:
    done = shell.run([exe, "--endpoint", MANIFEST_URL, "--current-version", TAG.lstrip("v"), "--pubkey", pubkey], timeout=120)
    report = parse_native_json(done.out)
    return {"rc": done.rc, "report": report, "release_not_found": bool(report and report.get("outcome") == "error" and RELEASE_NOT_FOUND in str(report.get("error"))),
            "output_tail": mask_ips(done.out.strip()[-400:])}


def probe_app_launch(ctx: Context, mitm: Optional[MitmProcess], port: int = CDP_PORT) -> Dict[str, Any]:
    """Launch the installed released app (live network blocked, GitHub names on the recording server) and look at its page. NO updater call."""
    home, wv2 = ctx.work / "probe-home", ctx.work / "probe-wv2"
    home.mkdir(parents=True, exist_ok=True)
    wv2.mkdir(parents=True, exist_ok=True)
    before = len(read_jsonl(mitm.log_path) or []) if mitm else 0
    log_path = ctx.work / "probe-app.raw.log"
    handle = log_path.open("wb")
    proc = subprocess.Popen([ctx.app_exe], env=sandbox_env(ctx.env, str(home), str(wv2), port), cwd=ctx.install_dir, stdout=handle, stderr=subprocess.STDOUT)
    result: Dict[str, Any] = {"pid": proc.pid}
    try:
        target, why = wait_for_page(port, 90.0)
        result["page_target"] = {"found": bool(target), "why": why, "url": (target or {}).get("url")}
        if target:
            time.sleep(5)
            host, wsport, path = parse_ws_url(target["webSocketDebuggerUrl"])
            ws = WebSocketClient(host, wsport, path)
            try:
                ws.connect()
                ok, value, error = cdp_result(cdp_evaluate(ws, ipc_probe_expression(), 1, timeout=30))
                result["ipc_probe"] = {"ok": ok, "value": value, "error": error}
            finally:
                ws.close()
            time.sleep(5)
        if mitm:
            records = (read_jsonl(mitm.log_path) or [])[before:]
            result["idle_launch_requests"] = {key: value for key, value in summarize_requests(records).items() if key != "present"}
        result["exit_code_while_probing"] = proc.poll()
    finally:
        kill_app(ctx, proc.pid, str(wv2))
        handle.close()
        mask_file(log_path, ctx.evidence / "probe-app.log")
    if not result["page_target"]["found"]:
        raise RuntimeError("the DevTools page target never appeared: %s" % json.dumps(result, default=str)[:600])
    return result


def cmd_probe(args: argparse.Namespace) -> int:
    evidence = Path(args.evidence)
    evidence.mkdir(parents=True, exist_ok=True)
    shell = Shell(evidence / "steps.log")
    work = Path(os.environ.get("ARCW0_WORK", r"C:\arcw0" if IS_WINDOWS else str(evidence / "work")))
    ctx = Context(evidence, work, shell, load_config(None))
    probe: Dict[str, Any] = {"schema": SCHEMA_PROBE, "os": "windows", "runner": runner_info(), "started": now_iso(), "checks": {}}
    checks = probe["checks"]
    state: Dict[str, Any] = {}
    cargo_job: Optional[Dict[str, Any]] = None
    torn_down = {"done": False}

    def teardown() -> None:
        """Stop the recording server and remove the hosts block, the Root certificate and the firewall rule (once)."""
        if torn_down["done"]:
            return
        torn_down["done"] = True
        server = state.pop("mitm", None)
        if server is not None:
            try:
                server.stop()
            except Exception as error:  # noqa: BLE001
                shell.log("recording server stop failed: %s" % error)
        try:
            cleanup(ctx)
            probe["cleanup"] = "hosts file restored, Root certificate removed, firewall rule removed (where they had been added)"
        except Exception as error:  # noqa: BLE001
            probe["cleanup"] = "cleanup failed: %s: %s" % (type(error).__name__, error)

    def flush() -> None:
        try:
            write_json(evidence / "probe.json", probe, sort_keys=False)        # chronological: the order the checks ran in is information
        except OSError:
            pass

    def check(name: str, fn: Callable[[], Any], requires: Sequence[str] = ()) -> bool:
        missing = [need for need in requires if not (checks.get(need) or {}).get("ok")]
        if missing:
            checks[name] = {"ok": False, "detail": "skipped: needs %s" % ", ".join(missing)}
        else:
            try:
                checks[name] = {"ok": True, "detail": fn()}
            except Exception as error:  # noqa: BLE001 - a probe never fails the job
                checks[name] = {"ok": False, "detail": "%s: %s" % (type(error).__name__, str(error)[:900])}
                shell.log("probe check %s failed: %s: %s" % (name, type(error).__name__, error))
        flush()
        return checks[name]["ok"]

    def ps(script: str, timeout: float = 60.0) -> str:
        done = shell.run(powershell_argv(script), timeout=timeout)
        return mask_ips(done.out.strip()[-600:]) + ("" if done.ok else " [rc %d]" % done.rc)

    flush()
    try:
        if args.cargo_check:
            cargo_job = start_cargo_check(ctx)
        check("python", lambda: sys.version.split()[0])
        check("is_windows", lambda: IS_WINDOWS)
        check("openssl", lambda: __import__("ca").openssl_binary())
        check("curl", lambda: shell.run(["curl.exe", "--version"], timeout=30).out.splitlines()[0])
        check("gh", lambda: shell.run(["gh", "--version"], timeout=30).out.splitlines()[0])
        check("cargo", lambda: shell.run(["cargo", "--version"], timeout=30).out.strip())
        check("rustup_toolchains", lambda: shell.run(["rustup", "toolchain", "list"], timeout=30).out.strip())
        check("disk", lambda: shutil.disk_usage(os.environ.get("SystemDrive", "C:") + "\\" if IS_WINDOWS else "/")._asdict())
        if IS_WINDOWS:
            check("admin", lambda: ps("([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)"))
            check("os_caption", lambda: ps("(Get-CimInstance Win32_OperatingSystem | Select-Object Caption,Version,BuildNumber | ConvertTo-Json -Compress)"))
            check("webview2_runtime", lambda: ps("(Get-ItemProperty 'HKLM:\\SOFTWARE\\WOW6432Node\\Microsoft\\EdgeUpdate\\Clients\\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}' -ErrorAction SilentlyContinue).pv"))
            check("edge_driver", lambda: ps("Get-Command msedgedriver -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source"))
            check("firewall_cmdlets", lambda: ps("Get-Command New-NetFirewallRule | Select-Object -ExpandProperty Name"))
            check("seven_zip", lambda: "present" if shell.run(["7z"], timeout=30).ok else "absent")

            def hosts_writable() -> str:
                path = hosts_path()
                with open(str(path), "r", encoding="utf-8", errors="replace", newline="") as handle:
                    original = handle.read()
                write_hosts(path, original + "# arcw0 probe\r\n")
                write_hosts(path, original)
                return "append and restore worked (%d bytes)" % len(original)
            check("hosts_writable", hosts_writable)

            def port443() -> str:
                sock = socket.socket()
                try:
                    sock.bind(("127.0.0.1", 443))
                    return "127.0.0.1:443 bindable"
                finally:
                    sock.close()
            check("port_443", port443)

            def firewall_roundtrip() -> str:
                rule = "arcw0-probe-block"
                made = shell.run(powershell_argv(firewall_block_script(["192.0.2.123"], rule)), timeout=60)
                gone = shell.run(powershell_argv(firewall_remove_script(rule)), timeout=60)
                if made.rc != 0:
                    raise RuntimeError("New-NetFirewallRule failed: %s" % made.out[-200:])
                return "create rc %d, remove rc %d" % (made.rc, gone.rc)
            check("firewall_roundtrip", firewall_roundtrip)

        # --- everything that reads real GitHub happens before any isolation is applied ---------------------------------
        def release_digest() -> Dict[str, Any]:
            asset = select_installer(gh_release(shell, TAG))
            state["asset"] = asset
            pin = find_pin(ctx.config, asset["name"])
            return {"asset": asset["name"], "size": asset["size"], "digest": asset["digest"], "kind": asset["kind"], "config_pin": pin_digest(pin) if pin else None,
                    "pin_matches": (pin_digest(pin) == asset["digest"].replace("sha256:", "")) if pin else None}
        check("release_asset_digest", release_digest)

        def tag_source() -> Dict[str, Any]:
            conf = ensure_tag_source(ctx)
            if "error" in conf:
                raise RuntimeError(conf["error"])
            state["pubkey"] = conf.get("pubkey")
            return {"version": conf.get("version"), "endpoints": conf.get("endpoints"), "pubkey_prefix": (conf.get("pubkey") or "")[:24] + "..."}
        check("tag_source", tag_source)

        def download() -> Dict[str, Any]:
            installer = work / "dl" / state["asset"]["name"]
            computed, problems = download_installer(ctx, state["asset"], installer)
            if problems:
                raise RuntimeError("; ".join(problems))
            state["installer"] = installer
            return {"sha256": computed, "matches_release_digest": True}
        check("installer_download", download, requires=("release_asset_digest",))

        def native_binary() -> Dict[str, Any]:
            found, tried = find_native_exe(None, ctx.env)
            if found is None:
                raise RuntimeError("no pre-built %s; looked at %s" % (NATIVE_EXE_NAME, tried))
            state["native_exe"] = found
            return binary_info(Path(found))
        check("native_binary", native_binary)
        if IS_WINDOWS:
            def live_list() -> str:
                ips = load_live_ips(ROOT)
                return "%d live addresses, list sha256 %s" % (len(ips), sha256_bytes(("\n".join(sorted(ips)) + "\n").encode()))
            check("live_ips", live_list)

            def ca() -> Dict[str, Any]:
                from ca import make_ca, DEFAULT_HOSTS, copy_public  # lib/ca.py
                ctx.ca_info = make_ca(ctx.ca_dir, DEFAULT_HOSTS)
                copy_public(ctx.ca_dir, evidence)
                return {"ca_sha256": ctx.ca_info["ca_sha256"], "hostnames": ctx.ca_info["hostnames"]}
            check("ca", ca)
            check("live_block", lambda: (block_live_network(ctx), ctx.live_block_detail)[1], requires=("live_ips",))
            check("root_store", lambda: (trust_ca(ctx), {"thumbprint_sha1": ctx.thumbprint})[1], requires=("ca",))

            def hosts() -> Dict[str, Any]:
                map_hosts(ctx, list(ctx.ca_info["hostnames"]))
                return {"mapped": ctx.hosts_mapped, "blackholed": ctx.hosts_blackholed}
            check("hosts", hosts, requires=("ca",))

            def recording_server() -> str:
                server = MitmProcess("latest-404", ctx.ca_info["server_cert"], ctx.ca_info["server_key"], evidence / "requests-probe.jsonl", evidence / "ready-probe")
                server.start()
                state["mitm"] = server
                return "listening (127.0.0.1 and ::1, port 443), scenario latest-404"
            check("recording_server", recording_server, requires=("ca", "hosts", "port_443"))

            def tls_curl() -> Dict[str, Any]:
                plain = curl_selftest(shell)
                relaxed = curl_selftest(shell, ["--ssl-no-revoke"]) if not plain["recording_server_header"] else None
                result = {"default": plain, "ssl_no_revoke": relaxed}
                if not (plain["recording_server_header"] or (relaxed and relaxed["recording_server_header"])):
                    raise RuntimeError("curl.exe (Schannel, OS trust store) did not reach the recording server: %s" % json.dumps(result)[:700])
                return result
            check("tls_selftest_curl", tls_curl, requires=("recording_server", "root_store"))

            def tls_native() -> Dict[str, Any]:
                result = native_selftest(shell, state["native_exe"], state["pubkey"])
                if not result["release_not_found"]:
                    raise RuntimeError("the real plugin did not end in ReleaseNotFound through the recording server: %s" % json.dumps(result, default=str)[:700])
                return result
            check("tls_selftest_native_plugin", tls_native, requires=("recording_server", "native_binary", "tag_source"))

            if args.launch:
                def app_install() -> Dict[str, Any]:
                    detail = install_app(ctx, state["asset"], state["installer"])
                    if not ctx.app_exe:
                        raise RuntimeError("no arc-desktop.exe after the installer: %s" % json.dumps(detail, default=str)[:600])
                    provenance = provenance_of_binary(Path(ctx.app_exe), state.get("pubkey"))
                    write_json(evidence / "provenance.json", provenance)
                    detail["provenance"] = {key: provenance[key] for key in ("binary_sha256", "binary_size", "plugin_version_from_binary", "matches_pin", "pubkey_matches_conf")}
                    return detail
                check("app_install", app_install, requires=("installer_download",))
                check("app_launch_idle", lambda: probe_app_launch(ctx, state.get("mitm")), requires=("app_install", "recording_server"))
        probe["isolation_snapshot"] = {"live_block": ctx.live_block, "live_block_detail": ctx.live_block_detail, "hosts_mapped": ctx.hosts_mapped, "hosts_blackholed": ctx.hosts_blackholed,
                                       "ca_sha256": (ctx.ca_info or {}).get("ca_sha256")}
        teardown()            # the isolation is not kept while the compiler runs
        flush()
    finally:
        teardown()
        if cargo_job is not None:
            try:
                outcome = finish_cargo_check(cargo_job)
                probe["native_check"] = outcome
                ok = outcome["rc"] == 0 and not outcome["timed_out"] and not outcome["error"]
                checks["native_crate_compiles"] = {"ok": ok, "detail": "cargo check --locked: rc %s%s after %s s" % (outcome["rc"], " (killed after the %d s cap)" % CARGO_CHECK_TIMEOUT_S if outcome["timed_out"] else "", outcome["elapsed_s"])
                                                                    if not outcome["error"] else outcome["error"]}
                if cargo_job.get("tail_text"):
                    (evidence / "cargo-check.log").write_text(cargo_job["tail_text"], encoding="utf-8")
            except Exception as error:  # noqa: BLE001 - a probe never fails the job
                probe["native_check"] = {"error": "%s: %s" % (type(error).__name__, error)}
        probe["finished"] = now_iso()
        flush()
    print("probe done: %s" % ", ".join("%s=%s" % (name, "ok" if value["ok"] else "FAIL") for name, value in checks.items()))
    return 0


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    probe = sub.add_parser("probe", help="capability probe; never fails the job")
    probe.add_argument("--evidence", required=True)
    probe.add_argument("--no-launch", dest="launch", action="store_false", help="skip installing and launching the released app (live network blocked) to look at its page")
    probe.set_defaults(launch=True)
    probe.add_argument("--no-cargo-check", dest="cargo_check", action="store_false", help="skip the background cargo check of the native-updater-check crate")
    probe.set_defaults(cargo_check=True)
    run = sub.add_parser("run", help="the isolation test")
    run.add_argument("--evidence", required=True)
    run.add_argument("--tier", choices=("released_app", "native_check", "both"), default="both")
    run.add_argument("--cases", default="clean,cached-bait")
    run.add_argument("--config", default=None)
    run.add_argument("--work", default=None)
    run.add_argument("--native-exe", default=None, help="path of the pre-built native-updater-check.exe (default: the workflow's cargo target directory)")
    args = parser.parse_args(argv)
    if args.command == "probe":
        try:
            return cmd_probe(args)
        except Exception as error:  # noqa: BLE001 - a probe never fails the job
            path = Path(args.evidence) / "probe.json"
            path.parent.mkdir(parents=True, exist_ok=True)
            try:
                document = json.loads(path.read_text(encoding="utf-8"))
                if not isinstance(document, dict):
                    document = {}
            except (OSError, ValueError):
                document = {}
            document.setdefault("schema", SCHEMA_PROBE)
            document.setdefault("os", "windows")
            document["error"] = "%s: %s" % (type(error).__name__, error)       # the checks recorded so far stay in the file
            write_json(path, document)
            print("probe crashed: %s" % error)
            return 0
    return cmd_run(args)


if __name__ == "__main__":
    sys.exit(main())
