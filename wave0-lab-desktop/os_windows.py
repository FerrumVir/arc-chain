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
  5. per case: snapshots the files and processes, starts the file-write poller and the connection watcher, launches the
     installed app with a sandboxed home and a dedicated WebView2 profile, and drives it. The strategies, in order (--strategies):
       uia           (default, first) the RELEASED app's own window through Windows UI Automation (PowerShell + System.Windows.Automation)
                     on the CI runner: find the window by process id, walk the UIA tree, click Settings, click "Check for updates"
                     (retried up to 4 times, 20 s apart, while no "Install v... & relaunch" button exists), click Install ONCE (the
                     button's handler calls the plugin check()), read ALL text nodes and join "Update failed: " + the plugin's message.
                     Both cases then run the latest-404 world (as on macOS); the bait scenario stays at plugin level (native tier).
       msedgedriver / cdp-env / cdp-registry   debugging-port strategies, kept as fallbacks for when UIA cannot even find the window
                     (wry's own browser arguments override WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS, so they failed on the first CI runs);
     the plugin path counts as reached through the released app only when Install was clicked AND a manifest request reached the
     recorder; the result records the resolved value or the rejection text VERBATIM, then snapshots again;
  6. computes the five criteria (only_manifest_url, no_bundle_download, no_install, no_new_app_launch, no_new_files)
     fail-closed from the recorded request log, file-write log, snapshots and process lists.
native_check tier (supporting): builds wave0-lab-desktop/native-updater-check (the real tauri-plugin-updater 2.10.1 check()
inside tauri's mock runtime), runs it against the same recording server and the same trust store for both scenarios, plus a
positive control (--control-download) that proves the harness sees a bundle request when one is made.

WHAT THE INTERCEPTION COVERS: the updater plugin's check/download/install path (it verifies TLS with the operating system trust
store, so a per-run CA in the machine Root store plus the hosts mapping lets the recording server answer for github.com).
The app's own reqwest calls (the "update available" banner `check_for_update`, and `ensure_binary`) use bundled webpki roots
and are NOT interceptable. REAL-BANNER MODE (default; --no-real-banner-api is the labelled negative control; approved by work-99 via
the captain, 2026-10-08): the banner's ONE unauthenticated read-only GET https://api.github.com/repos/FerrumVir/arc-chain/releases/latest
is the only request allowed to reach the real Internet. api.github.com is left to the real DNS (NOT in the hosts file), its content is
NOT recorded by this lab, its addresses are resolved at run time and labelled in network-*.json; github.com, the asset hosts and rsms.me
go to the recording server by the hosts file; the ARC addresses stay blocked by the firewall and the real addresses of rsms.me carry the
same block as an independent second line (refreshed before each case); msedgewebview2.exe is blocked from the Internet by program, so
Edge platform traffic cannot be mistaken for the app's. A watcher polls Get-NetTCPConnection for arc-desktop.exe and its msedgewebview2.exe
children about once a second; any other real destination fails the case with the endpoint named (blocked attempts at ARC or rsms.me
addresses are recorded as information, an ESTABLISHED one is a violation). No connection record at all leaves the case UNPROVED.
result.json says all this (interception.not_covered, isolation.real_banner_api).

Safety (by construction): the live ARC addresses are blocked in the firewall before the app starts; only ca.crt and its
sha256 are ever copied into the evidence directory (the private keys stay under the CA's private/ directory, and the evidence
directory is scanned for key material before the run ends); IPv4 addresses are masked in every log written here, and in the connection
evidence the ARC addresses are replaced by live-ip-N (the banner's public addresses and any violating endpoint stay readable on purpose).
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
UNVERIFIED ON CI (UI Automation round; the PowerShell below was never executed on the Mac this file was written on, only parsed by tests
that count brackets; `probe` now runs the real PowerShell parser over the scripts first, check uia_scripts_parse):
  * that Chromium exposes the WebView2 page to a UIA client at all (it builds its accessibility tree lazily when it believes an assistive
    technology runs): the tree walk is retried for ~60 s, SPI_SETSCREENREADER is set for the run and restored, and
    --force-renderer-accessibility is passed in WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS (wry's own arguments probably override it);
  * that the buttons are Button/Hyperlink nodes named exactly 'Settings', 'Check for updates' and 'Install v... & relaunch' (as in the
    macOS accessibility dumps), that they support InvokePattern (otherwise a bounding-rectangle mouse click is used and recorded as
    method 'mouse'; that needs the runner's interactive desktop), and that "Update failed: " and the plugin's message render as two
    adjacent Text nodes (the text search also covers non-adjacent nodes; nothing found = UNPROVED);
  * the output format of `nslookup` on the runner (parse_nslookup_addresses starts at the first "Name:" line) and that a program-scoped
    outbound block of msedgewebview2.exe leaves the page working; the 1 s connection poll is a sample, a connection that opens and
    closes between two polls is not seen;
  * System.Drawing CopyFromScreen in the runner's session (screenshots are best effort and listed only when the file exists).
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
TRIGGER_STRATEGIES = ("uia", "msedgedriver", "cdp-env", "cdp-registry")
BANNER_API_HOST = "api.github.com"
BANNER_API_PATH = "/repos/FerrumVir/arc-chain/releases/latest"
RSMS_HOST = "rsms.me"
APP_SCENARIO = "latest-404"          # both released-app cases when the app is driven through its own UI (bait is exercised at plugin level only)
SETTINGS_PATTERN = r"^\s*Settings\s*$"
CHECK_PATTERN = r"Check for updates"
INSTALL_PATTERN = r"Install\s+v?\d"
RSMS_RULE = "arcw0-rsms-block"
WEBVIEW2_RULE = "arcw0-webview2-egress-block"
REAL_BANNER_SCOPE = (
    "Only one kind of request leaves the sandbox for the real Internet: the banner's unauthenticated read-only GET (one per click on Check for updates) "
    "https://api.github.com/repos/FerrumVir/arc-chain/releases/latest (Settings.tsx:20-26 queryFn api.checkForUpdate; commands.rs:934-957 "
    "check_for_update: reqwest with bundled webpki roots, no Authorization header). Approved by work-99, relayed by the captain, 2026-10-08. "
    "github.com and every other GitHub/asset host and rsms.me stay mapped to the recorder (hosts file), api.github.com is left to the real DNS and is NOT mapped, "
    "the six ARC node addresses stay blocked by the Windows firewall, the real addresses of rsms.me carry the same block as an independent second line, and any "
    "other real destination of the app process tree fails the case with the endpoint named. The CONTENT of that pass-through request was NOT recorded by this lab "
    "(it is not intercepted); its answer is independently checkable: real Latest = v0.7.12 since 2026-10-08T14:18:32Z."
)
EDGE_DRIVER_DEFAULT = "C:\\SeleniumWebDrivers\\EdgeDriver\\msedgedriver.exe"
REGISTRY_KEY = "HKCU\\SOFTWARE\\Policies\\Microsoft\\Edge\\WebView2\\AdditionalBrowserArguments"
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

    def finish(self, min_cycles: int = 2, extra_cycles: int = 2) -> int:
        """Stop only after `extra_cycles` more FULL cycles have completed (the first of them may have started before the process ended)
        and at least `min_cycles` in total, so that even a 0.1 s native check leaves a complete write log."""
        if self.thread is not None:
            target = max(min_cycles, self.cycles + extra_cycles)
            deadline = time.time() + (target - self.cycles + 2) * self.interval + 5.0
            while self.cycles < target and time.time() < deadline and self.thread.is_alive():
                time.sleep(min(0.05, max(0.005, self.interval / 4.0)))
        return self.stop()


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


COMMAND_LINE_ALLOWED = re.compile(r"(?i)^(arc-desktop|msedgewebview2|msedgedriver|native-updater-check|arc-node|.*setup.*|msiexec|.*updater.*)\.exe$")


def redact_processes(processes: Optional[List[dict]]) -> Optional[List[dict]]:
    """Process list for an evidence file: command lines only for the processes this test is about (a runner process may carry a token)."""
    if processes is None:
        return None
    out = []
    for process in processes:
        item = dict(process)
        if not COMMAND_LINE_ALLOWED.match(str(item.get("Name") or "")):
            item["CommandLine"] = "<omitted>"
        item["CommandLine"] = mask_ips(str(item["CommandLine"]))[:2500]      # WebView2 command lines are long; --remote-debugging-port must not be cut off
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
# Processes this script itself (or the runner) starts to look at the machine: recorded, never counted as an app launch or an install.
LAB_HELPER_NAMES = frozenset({
    "powershell.exe", "pwsh.exe", "python.exe", "pythonw.exe", "py.exe", "conhost.exe", "cmd.exe", "bash.exe", "sh.exe", "curl.exe", "certutil.exe", "taskkill.exe",
    "git.exe", "gh.exe", "reg.exe", "ipconfig.exe", "tasklist.exe", "cargo.exe", "rustc.exe", "wmiprvse.exe", "msedgedriver.exe",
    "nslookup.exe", "netsh.exe", "netstat.exe", "csc.exe", "cvtres.exe",       # name resolution, firewall and connection listing, and the C# compiler PowerShell's Add-Type starts
})


def process_violations(before: Optional[List[dict]], after: Optional[List[dict]], app_pid: Optional[int], markers: Sequence[str],
                       ignore_exes: Sequence[str] = (), webview_markers: Sequence[str] = ()) -> Optional[Dict[str, List[str]]]:
    """New processes between the two snapshots that are not the app's own WebView2 helpers or the lab's own helpers. None = a snapshot is missing.

    app_pid is the process of the app under test; 0 means "there is no app process" (the native tier): then nothing counts as the
    app's tree and only a new arc-desktop.exe, an installer or an updater process can be flagged."""
    if before is None or after is None or app_pid is None:
        return None
    old = {process_key(p) for p in before}
    skip = {norm_path(path) for path in ignore_exes if path}
    new = [p for p in after if process_key(p) not in old and norm_path(str(p.get("ExecutablePath") or "")) not in skip]
    tree = descendants(app_pid, after) if app_pid else set()
    low_markers = [m.lower() for m in markers if m]
    own_webview = [m.lower() for m in webview_markers if m]
    result: Dict[str, List[str]] = {"new_app_launches": [], "installers": [], "other_in_app_tree": [], "unrelated": [], "lab_helpers": []}
    for process in new:
        name = str(process.get("Name") or "")
        exe = str(process.get("ExecutablePath") or "")
        command = str(process.get("CommandLine") or "")
        text = "%s | %s | %s (pid %s)" % (name, exe, command[:160], process.get("ProcessId"))
        if name.lower() == WEBVIEW2_PROCESS and (process["ProcessId"] in tree or any(m in command.lower() for m in own_webview)):
            continue
        if name.lower() in LAB_HELPER_NAMES:
            result["lab_helpers"].append(text)
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
                  writes_ok: bool, install_changes: Optional[List[str]], network: Optional[Dict[str, Any]] = None, require_network: bool = False,
                  require_manifest: bool = False) -> Dict[str, Any]:
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
    if require_manifest and ran and requests.get("present") and requests.get("total") and not classes.get("manifest"):
        criteria["only_manifest_url"] = None
        reasons.append("no request for the manifest URL reached the recorder after the Install click: the plugin check was not shown to reach its endpoint")
    # what reached the real Internet (real-banner mode lets exactly one kind of request through; everything else fails the case)
    if network is not None and network.get("violations"):
        criteria["only_manifest_url"] = False
        reasons.append("a real destination other than the allowed ones was contacted: %s" % [
            "%s:%s (%s pid %s, %s)" % (v.get("remote_ip"), v.get("remote_port"), v.get("proc"), v.get("pid"), ",".join(v.get("states") or [])) for v in network["violations"]][:6])
    elif require_network and (network is None or not network.get("recorded")) and criteria["only_manifest_url"] is not False:
        criteria["only_manifest_url"] = None
        reasons.append("a request was allowed to leave the sandbox, but no network record of the app's processes was captured to prove nothing else did")
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
    if value.get("stage") in ("no-ipc", "ui-not-reached"):
        return {"ran": False, "ok": False, "error_text": value.get("error"), "stage": value.get("stage"), "value": None}
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
                            "so their TLS handshake to the recording server fails (logged as tls_failure); covered by the cited source and the Stage A replica. "
                            "In real-banner mode the banner's one read-only GET to api.github.com is passed through to the real Internet, observed but not recorded (see isolation.real_banner_api)"],
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
        self.strategies: List[str] = list(TRIGGER_STRATEGIES)
        self.winning_strategy: Optional[str] = None
        self.registry_override = False
        self.real_banner = True                  # the banner's one read-only GET to api.github.com may reach the real Internet (approved); False = negative control
        self.live_ips: List[str] = []
        self.api_addresses: List[str] = []
        self.rsms_addresses: List[str] = []
        self.rsms_rule_created = False
        self.webview2_rule_created = False
        self.screen_reader_before: Optional[int] = None
        self.extra_block_log: List[Dict[str, Any]] = []
        self.webview2_block = True               # block msedgewebview2.exe's egress by program (UNVERIFIED ON CI that the page keeps working); --no-webview2-block observes only
        self.notes: List[str] = []
        self.native_exe: Optional[Path] = None

    def ui_mode(self) -> bool:
        """The released app is driven through its own window (UI Automation first): both cases then use the latest-404 world, bait stays plugin-level (native tier)."""
        return bool(self.strategies) and self.strategies[0] == "uia"

    def scenario_for(self, name: str) -> str:
        return APP_SCENARIO if self.ui_mode() else CASE_SCENARIO[name]


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


def intercepted_names(ctx: Context, names: Sequence[str]) -> List[str]:
    """The names pointed at the recorder: in real-banner mode api.github.com is the one name left to the real DNS."""
    return [name for name in names if not (ctx.real_banner and name == BANNER_API_HOST)]


def map_hosts(ctx: Context, names: Sequence[str]) -> None:
    names = intercepted_names(ctx, names)
    path = hosts_path()
    with open(str(path), "r", encoding="utf-8", errors="replace", newline="") as handle:
        ctx.hosts_original = handle.read()
    blackholed = [name for name in blackhole_names() if name != BANNER_API_HOST]
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


def sandbox_env(base: Dict[str, str], home: str, wv2: str, port: Optional[int] = CDP_PORT) -> Dict[str, str]:
    """Environment of the app under test. HOME only: the app reads HOME before USERPROFILE for ~/.arc (commands.rs, node_manager.rs), while
    overriding USERPROFILE made Tauri's app_data_dir() fail (the first CI run logged the temp directory as the app data dir), so
    USERPROFILE stays real. port=None leaves WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS alone (msedgedriver sets its own)."""
    env = dict(base)
    for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
        env.pop(name, None)
    env["HOME"] = home
    env["WEBVIEW2_USER_DATA_FOLDER"] = wv2
    if port is not None:
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
    """Stop everything a launch can leave behind: the app tree, its WebView2 helpers (any whose command line carries the profile directory) and msedgedriver."""
    shell = ctx.shell
    if pid:
        shell.run(["taskkill", "/F", "/T", "/PID", str(pid)], timeout=60, quiet=True)
    shell.run(["taskkill", "/F", "/T", "/IM", APP_EXE_NAME], timeout=60, quiet=True)
    shell.run(["taskkill", "/F", "/T", "/IM", "msedgedriver.exe"], timeout=60, quiet=True)
    sweep = ("Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'msedgewebview2.exe' -and $_.CommandLine -like '*%s*' } "
             "| ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }" % wv2_dir.replace("'", "''"))
    shell.run(powershell_argv(sweep), timeout=60, quiet=True)


def leftover_processes(processes: Optional[List[dict]], markers: Sequence[str]) -> List[str]:
    """Processes of an earlier launch that are still alive: the app, msedgedriver, or a WebView2 helper whose command line carries one of the markers."""
    low = [m.lower() for m in markers if m]
    found = []
    for process in processes or []:
        name = str(process.get("Name") or "").lower()
        command = str(process.get("CommandLine") or "").lower()
        exe = str(process.get("ExecutablePath") or "").lower()
        if name in (APP_EXE_NAME, "msedgedriver.exe") or (name == WEBVIEW2_PROCESS and any(m in command for m in low)):
            found.append("%s pid %s %s" % (process.get("Name"), process.get("ProcessId"), exe))
    return found


# --- msedgedriver (W3C WebDriver) ------------------------------------------------------------------------------------

class W3CError(RuntimeError):
    def __init__(self, error: Any, message: Any, status: Optional[int] = None):
        super().__init__("%s: %s" % (error, message))
        self.error, self.message, self.status = error, message, status


def find_edge_driver(env: Dict[str, str], exists: Callable[[str], bool] = os.path.isfile, which: Callable[[str], Optional[str]] = shutil.which) -> Optional[str]:
    """msedgedriver.exe: the image's EdgeWebDriver directory, the usual SeleniumWebDrivers location, then PATH."""
    candidates: List[str] = []
    for key in ("EDGEWEBDRIVER", "EdgeWebDriver"):
        base = env.get(key)
        if base:
            candidates.append(base.rstrip("\\") + "\\msedgedriver.exe")
    candidates.append(EDGE_DRIVER_DEFAULT)
    for candidate in candidates:
        if exists(candidate):
            return candidate
    return which("msedgedriver")


def free_port(bind: Callable[..., Any] = socket.socket) -> int:
    sock = bind()
    try:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])
    finally:
        sock.close()


def w3c_call(base: str, method: str, path: str, body: Optional[dict] = None, timeout: float = 30.0, opener: Optional[Callable[..., Any]] = None) -> Tuple[int, Any]:
    """One W3C WebDriver HTTP call (JSON in, JSON out, never through a proxy). Returns (HTTP status, parsed body)."""
    data = json.dumps(body).encode("utf-8") if body is not None else None
    request = urllib.request.Request(base + path, data=data, method=method, headers={"Content-Type": "application/json; charset=utf-8", "Accept": "application/json"})
    open_url = opener or urllib.request.build_opener(urllib.request.ProxyHandler({})).open
    try:
        with open_url(request, timeout=timeout) as response:
            status, text = response.status, response.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as error:
        status, text = error.code, error.read().decode("utf-8", "replace")
    try:
        payload = json.loads(text)
    except ValueError:
        payload = {"raw": text[:300]}
    return status, payload


def w3c_value(status: int, payload: Any) -> Any:
    """The `value` of a W3C response; raises W3CError for an HTTP error status or an error object."""
    value = payload.get("value") if isinstance(payload, dict) else None
    if status >= 400 or (isinstance(value, dict) and "error" in value and "message" in value):
        detail = value if isinstance(value, dict) else {}
        raise W3CError(detail.get("error") or "http %d" % status, detail.get("message") or json.dumps(payload)[:300], status)
    return value


def edge_capabilities(app_exe: str) -> dict:
    """The capabilities tauri-driver sends to msedgedriver on Windows to drive a WebView2 application."""
    return {"capabilities": {"alwaysMatch": {"browserName": "webview2", "ms:edgeOptions": {"binary": app_exe, "webviewOptions": {}}}}}


def trigger_script_async() -> str:
    """execute/async body: the same plugin:updater|check call as trigger_expression(), callback style, the outcome captured as plain data."""
    return (
        "const done = arguments[arguments.length - 1]; "
        "(async () => { try { "
        "const inv = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke; "
        "if (typeof inv !== 'function') { done({ok: false, stage: 'no-ipc', error: 'window.__TAURI_INTERNALS__.invoke is not available'}); return; } "
        "const value = await inv('plugin:updater|check', {}); "
        "done({ok: true, value: (value === undefined ? null : value)}); "
        "} catch (e) { "
        "let extra = null; try { extra = (typeof e === 'object' && e !== null) ? JSON.stringify(e, Object.getOwnPropertyNames(e)) : null; } catch (_) {} "
        "done({ok: false, stage: 'invoke', error: String(e), error_type: typeof e, error_json: extra}); } })();"
    )


def probe_script_async() -> str:
    return (
        "const done = arguments[arguments.length - 1]; "
        "(async () => { const inv = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke; "
        "let version = null, version_error = null; "
        "if (typeof inv === 'function') { try { version = await inv('plugin:app|version', {}); } catch (e) { version_error = String(e); } } "
        "done({ipc: typeof inv === 'function', href: String(location.href), title: String(document.title), app_version: version, app_version_error: version_error}); })();"
    )


class WebDriverPage:
    """The app's window through msedgedriver (W3C): one session whose `binary` is the released arc-desktop.exe."""
    kind = "msedgedriver"

    def __init__(self, base: str, opener: Optional[Callable[..., Any]] = None):
        self.base, self.opener, self.session_id = base, opener, None

    def call(self, method: str, path: str, body: Optional[dict] = None, timeout: float = 60.0) -> Any:
        status, payload = w3c_call(self.base, method, path, body, timeout, self.opener)
        return w3c_value(status, payload)

    def open(self, app_exe: str, timeout: float = 120.0) -> dict:
        value = self.call("POST", "/session", edge_capabilities(app_exe), timeout=timeout)
        session = value.get("sessionId") if isinstance(value, dict) else None
        if not session:
            raise W3CError("no session id", json.dumps(value)[:300])
        self.session_id = session
        self.call("POST", "/session/%s/timeouts" % session, {"script": 90000, "pageLoad": 60000, "implicit": 0}, timeout=30)
        capabilities = value.get("capabilities") if isinstance(value.get("capabilities"), dict) else {}
        return {key: capabilities.get(key) for key in ("browserName", "browserVersion", "platformName", "msedge", "webview2") if key in capabilities}

    def _run(self, script: str) -> Tuple[bool, Any, Optional[str]]:
        try:
            return True, self.call("POST", "/session/%s/execute/async" % self.session_id, {"script": script, "args": []}, timeout=110), None
        except (W3CError, OSError, ValueError) as error:
            return False, None, "%s: %s" % (type(error).__name__, error)

    def evaluate_probe(self) -> Tuple[bool, Any, Optional[str]]:
        return self._run(probe_script_async())

    def evaluate_trigger(self) -> Tuple[bool, Any, Optional[str]]:
        return self._run(trigger_script_async())

    def close(self) -> None:
        if self.session_id:
            try:
                self.call("DELETE", "/session/%s" % self.session_id, timeout=20)
            except (W3CError, OSError, ValueError):
                pass
            self.session_id = None


class CdpPage:
    """The app's window through the Chrome DevTools Protocol (a websocket to the page target)."""
    kind = "cdp"

    def __init__(self, ws: Any):
        self.ws = ws

    def evaluate_probe(self) -> Tuple[bool, Any, Optional[str]]:
        return cdp_result(cdp_evaluate(self.ws, ipc_probe_expression(), 1, timeout=30))

    def evaluate_trigger(self) -> Tuple[bool, Any, Optional[str]]:
        return cdp_result(cdp_evaluate(self.ws, trigger_expression(), 2, timeout=90))

    def close(self) -> None:
        self.ws.close()


def read_devtools_port(wv2_launch: str, read: Callable[[str], str]) -> Optional[int]:
    """Chromium writes DevToolsActivePort (port on the first line) into its user data directory; WebView2's is <folder>\\EBWebView."""
    for relative in ("EBWebView\\DevToolsActivePort", "DevToolsActivePort"):
        try:
            text = read(wv2_launch.rstrip("\\") + "\\" + relative)
        except OSError:
            continue
        first = text.splitlines()[0].strip() if text.strip() else ""
        if first.isdigit() and int(first) > 0:
            return int(first)
    return None


def wait_for_devtools_port(wv2_launch: str, timeout: float, read: Optional[Callable[[str], str]] = None, sleep: Callable[[float], None] = time.sleep,
                           clock: Callable[[], float] = time.time) -> Tuple[Optional[int], str]:
    reader = read or (lambda path: open(path, "r", encoding="ascii", errors="replace").read())
    deadline = clock() + timeout
    while clock() < deadline:
        port = read_devtools_port(wv2_launch, reader)
        if port:
            return port, "ok"
        sleep(1.0)
    return None, "DevToolsActivePort never appeared under %s" % wv2_launch


def registry_override_argv(value: str) -> List[str]:
    return ["reg", "add", REGISTRY_KEY, "/v", APP_EXE_NAME, "/t", "REG_SZ", "/d", value, "/f"]


def registry_override_remove_argv() -> List[str]:
    return ["reg", "delete", REGISTRY_KEY, "/v", APP_EXE_NAME, "/f"]


def find_app_pid(processes: Optional[List[dict]], app_exe: str) -> Optional[int]:
    """The newest running process of the installed app (it is a child of msedgedriver in the WebDriver strategy)."""
    matches = [p for p in processes or [] if norm_path(str(p.get("ExecutablePath") or "")) == norm_path(app_exe)]
    if not matches:
        return None
    matches.sort(key=lambda p: (str(p.get("CreationDate")), p.get("ProcessId") or 0))
    return int(matches[-1]["ProcessId"])


class Launch:
    """One attempt to get a scripting handle on the app's window with one strategy."""

    def __init__(self, strategy: str, wv2: str):
        self.strategy, self.wv2 = strategy, wv2
        self.page: Any = None
        self.app_proc: Any = None
        self.driver_proc: Any = None
        self.handles: List[Any] = []
        self.error: Optional[str] = None
        self.notes: Dict[str, Any] = {}

    @property
    def ok(self) -> bool:
        return self.error is None and self.page is not None


def start_edge_driver(ctx: Context, launch: Launch, tag: str, home: str, popen: Optional[Callable[..., Any]] = None,
                      call: Optional[Callable[..., Tuple[int, Any]]] = None, sleep: Optional[Callable[[float], None]] = None, clock: Optional[Callable[[], float]] = None) -> str:
    """Start msedgedriver on a free port with the sandboxed environment and wait for /status; returns the base URL."""
    popen = popen or subprocess.Popen             # resolved at call time, so a test (or a wrapper) can replace them
    call = call or w3c_call
    sleep = sleep or time.sleep
    clock = clock or time.time
    exe = find_edge_driver(ctx.env)
    if not exe:
        raise RuntimeError("msedgedriver.exe not found (EdgeWebDriver, %s, PATH)" % EDGE_DRIVER_DEFAULT)
    port = free_port()
    driver_log = ctx.work / ("msedgedriver-%s.log" % tag)
    out = (ctx.work / ("msedgedriver-%s.out" % tag)).open("wb")
    launch.handles.append(out)
    launch.driver_proc = popen([exe, "--port=%d" % port, "--verbose", "--log-path=%s" % driver_log], env=sandbox_env(ctx.env, home, launch.wv2, port=None),
                               stdout=out, stderr=subprocess.STDOUT)
    launch.notes["driver"] = {"exe": exe, "port": port, "log": str(driver_log)}
    base = "http://127.0.0.1:%d" % port
    deadline = clock() + 30
    while clock() < deadline:
        if launch.driver_proc.poll() is not None:
            raise RuntimeError("msedgedriver exited with code %s before it was ready" % launch.driver_proc.poll())
        try:
            status, _payload = call(base, "GET", "/status", None, 3.0)
            if status == 200:
                return base
        except (OSError, ValueError, urllib.error.URLError):
            pass
        sleep(0.5)
    raise RuntimeError("msedgedriver did not answer /status within 30 s")


def try_strategy(ctx: Context, strategy: str, tag: str, home: str, wv2_launch: str, popen: Optional[Callable[..., Any]] = None) -> Launch:
    """Start the app by one strategy and return the Launch; a failure is recorded in launch.error, never raised."""
    popen = popen or subprocess.Popen
    launch = Launch(strategy, wv2_launch)
    try:
        Path(wv2_launch).mkdir(parents=True, exist_ok=True)
        if strategy == "uia":
            parsed = check_ps_scripts(ctx)
            launch.notes["ps_parse"] = parsed
            if parsed["ran"] and not parsed["ok"]:
                raise RuntimeError("the UI Automation PowerShell scripts do not parse on this runner: %s" % json.dumps(parsed["errors"])[:600])
            launch.notes["screen_reader"] = set_screen_reader(ctx)
            env = sandbox_env(ctx.env, home, wv2_launch, port=None)
            env["WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS"] = "--force-renderer-accessibility"      # a hint only: the first CI runs showed wry's own browser arguments win over this variable
            log = (ctx.work / ("app-%s.raw.log" % tag)).open("wb")
            launch.handles.append(log)
            launch.app_proc = popen([ctx.app_exe], env=env, cwd=ctx.install_dir, stdout=log, stderr=subprocess.STDOUT)
            page = UiaPage(ctx, launch.app_proc.pid, ctx.evidence, tag, real_banner=ctx.real_banner)
            launch.page = page
            launch.notes["window_found"] = False
            page.open()
            launch.notes["window_found"] = True
            launch.notes["uia"] = page.ui.get("after_launch")
        elif strategy == "msedgedriver":
            base = start_edge_driver(ctx, launch, tag, home, popen)
            page = WebDriverPage(base)
            launch.page = page          # so a half-open session is still closed
            launch.notes["session"] = page.open(ctx.app_exe, timeout=120)
        elif strategy in ("cdp-env", "cdp-registry"):
            args = "--remote-debugging-port=0 --remote-allow-origins=*"
            if strategy == "cdp-registry":
                done = ctx.shell.run(registry_override_argv(args), timeout=30)
                if not done.ok:
                    raise RuntimeError("reg add failed: %s" % done.out[-200:])
                ctx.registry_override = True
                launch.notes["registry"] = REGISTRY_KEY
            log = (ctx.work / ("app-%s.raw.log" % tag)).open("wb")
            launch.handles.append(log)
            launch.app_proc = popen([ctx.app_exe], env=sandbox_env(ctx.env, home, wv2_launch, port=0), cwd=ctx.install_dir, stdout=log, stderr=subprocess.STDOUT)
            port, why = wait_for_devtools_port(wv2_launch, 60.0)
            launch.notes["devtools_port"] = port
            if port is None:
                raise RuntimeError(why)
            target, why = wait_for_page(port, 60.0)
            if target is None:
                raise RuntimeError("the DevTools page target never appeared (%s)" % why)
            host, wsport, path = parse_ws_url(target["webSocketDebuggerUrl"])
            ws = WebSocketClient(host, wsport, path)
            launch.page = CdpPage(ws)
            ws.connect()
        else:
            raise RuntimeError("unknown strategy %r" % strategy)
    except Exception as error:  # noqa: BLE001 - recorded, the next strategy is tried; launch.page stays so close_launch can close a half-open session
        launch.error = "%s: %s" % (type(error).__name__, str(error)[:600])
        if isinstance(error, UiaContentNotFound):
            launch.notes["window_found"] = True
    return launch


def close_launch(ctx: Context, launch: Launch, wv2_base: str) -> None:
    if launch.page is not None:
        try:
            launch.page.close()
        except Exception as error:  # noqa: BLE001
            ctx.shell.log("closing the %s session failed: %s" % (launch.strategy, error))
    if launch.driver_proc is not None and launch.driver_proc.poll() is None:
        try:
            launch.driver_proc.kill()
        except OSError:
            pass
    pid = launch.app_proc.pid if launch.app_proc is not None else None
    kill_app(ctx, pid, wv2_base)
    if ctx.registry_override:
        ctx.shell.run(registry_override_remove_argv(), timeout=30, quiet=True)
        ctx.registry_override = False
    for handle in launch.handles:
        try:
            handle.close()
        except OSError:
            pass
    launch.handles = []


def webview_command_lines(processes: Optional[List[dict]]) -> List[str]:
    """Command lines of the msedgewebview2.exe processes (whether --remote-debugging-port arrived), IPs masked, trimmed."""
    lines = []
    for process in processes or []:
        if str(process.get("Name") or "").lower() == WEBVIEW2_PROCESS:
            lines.append(mask_ips(str(process.get("CommandLine") or ""))[:2500])
    return lines


def case_summary(requests: Dict[str, Any], classification: Optional[Dict[str, Any]], install_changes: Optional[List[str]], cycles: int) -> Dict[str, Any]:
    return {
        "requests": {"total": requests["total"], "by_host_path": requests["by_host_path"], "tls_failures": requests["tls_failures"], "classes": requests["classes"]},
        "file_changes": {"expected": len((classification or {}).get("expected", [])), "noise": len((classification or {}).get("noise", [])),
                         "unexpected": (classification or {}).get("unexpected", []), "install_dir_changes": install_changes, "poller_cycles": cycles},
    }


def judge_recorded_case(scenario: str, trigger: Dict[str, Any], requests_path: Path, writes_path: Path, before: Optional[Dict[str, dict]], after: Dict[str, dict],
                        policies: Sequence[Dict[str, str]], install_dir: str, procs_before: Optional[List[dict]], procs_after: Optional[List[dict]],
                        app_pid: Optional[int], markers: Sequence[str], ignore_exes: Sequence[str], cycles: int, min_cycles: int, diff_path: Optional[Path] = None,
                        webview_markers: Sequence[str] = (), network: Optional[Dict[str, Any]] = None, require_network: bool = False,
                        require_manifest: bool = False) -> Dict[str, Any]:
    """Everything a case recorded -> the case fields (requests, file_changes, criteria, verdict, reasons, process_changes)."""
    delta = diff_snapshots(before, after) if before is not None else None
    classification = classify_fs_changes(delta, policies) if delta is not None else None
    if delta is not None and diff_path is not None:
        write_json(diff_path, delta)
    install_changes = [p for kind in ("added", "changed", "removed") for p in delta[kind] if path_under(p, install_dir)] if delta is not None else None
    write_records = read_jsonl(writes_path)
    writes_ok = bool(write_records) and cycles >= min_cycles and any(r.get("event") == "stop" for r in write_records)
    requests = summarize_requests(read_jsonl(requests_path))
    violations = process_violations(procs_before, procs_after, app_pid, markers, ignore_exes, webview_markers)
    judgement = evaluate_case(scenario, trigger, requests, classification, violations, writes_ok, install_changes, network, require_network, require_manifest)
    fields = case_summary(requests, classification, install_changes, cycles)
    if network is not None:
        fields["network"] = {"violations": network.get("violations"), "allowed_banner_endpoints": network.get("allowed_banner_endpoints"),
                             "blocked_attempts": len(network.get("blocked_attempts") or []), "recorded": network.get("recorded"), "watcher_polls": network.get("watcher_polls")}
    fields.update(criteria=judgement["criteria"], verdict=judgement["verdict"], reasons=judgement["reasons"], process_changes=violations)
    return fields


def mask_file(src: Path, dest: Path) -> None:
    """Copy a text log with IPv4 addresses masked (the app prints the seed addresses it cannot reach)."""
    try:
        dest.write_text(mask_ips(Path(src).read_text(encoding="utf-8", errors="replace")), encoding="utf-8")
    except OSError:
        pass


# ---------------------------------------------------------------------------------------------------------------------
# UI Automation: drive the RELEASED app through its own window (the WebView2 debugging switches never arrive: wry sets its own browser arguments)
# ---------------------------------------------------------------------------------------------------------------------

UIA_WALK_PS = r"""
param(
  [int]$ProcessId,
  [string]$Out,
  [string]$Pattern = '',
  [string]$Types = 'Button,Hyperlink',
  [int]$MaxNodes = 900,
  [int]$MaxDepth = 40,
  [int]$BudgetMs = 60000,
  [switch]$WindowsOnly,
  [switch]$Click
)
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$ErrorActionPreference = 'Stop'
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$windows = New-Object System.Collections.ArrayList
$nodes = New-Object System.Collections.ArrayList
$result = [ordered]@{ pid = $ProcessId; windows = $windows; nodes = $nodes; truncated = $false; error = $null; elapsed_ms = 0; clicked = $false; method = $null; matched = $null; visited = 0 }
function Get-Info($el, $depth) {
  $c = $el.Current
  return [ordered]@{
    depth = $depth
    type = ([string]$c.ControlType.ProgrammaticName -replace '^ControlType\.', '')
    name = [string]$c.Name
    id = [string]$c.AutomationId
    class = [string]$c.ClassName
    enabled = [bool]$c.IsEnabled
    offscreen = [bool]$c.IsOffscreen
    pid = [int]$c.ProcessId
  }
}
try {
  Add-Type -AssemblyName UIAutomationClient
  Add-Type -AssemblyName UIAutomationTypes
  $root = [System.Windows.Automation.AutomationElement]::RootElement
  $cond = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ProcessIdProperty, $ProcessId)
  $tops = $root.FindAll([System.Windows.Automation.TreeScope]::Children, $cond)
  foreach ($w in $tops) {
    $c = $w.Current
    [void]$windows.Add([ordered]@{ name = [string]$c.Name; class = [string]$c.ClassName; type = ([string]$c.ControlType.ProgrammaticName -replace '^ControlType\.', ''); handle = [int64]$c.NativeWindowHandle; offscreen = [bool]$c.IsOffscreen })
  }
  if (-not $WindowsOnly) {
    $walker = [System.Windows.Automation.TreeWalker]::RawViewWalker
    $wanted = $Types -split ','
    $target = $null
    foreach ($w in $tops) {
      if ($target -ne $null) { break }
      $stack = New-Object System.Collections.Stack
      $stack.Push(@($w, 0))
      while ($stack.Count -gt 0) {
        if ($sw.ElapsedMilliseconds -gt $BudgetMs -or $result.visited -ge $MaxNodes) { $result.truncated = $true; break }
        $item = $stack.Pop()
        $el = $item[0]
        $depth = [int]$item[1]
        try { $info = Get-Info $el $depth } catch { continue }
        $result.visited = $result.visited + 1
        if ($Click) {
          if (($wanted -contains $info.type) -and ($info.name -match $Pattern) -and $info.enabled) { $target = $el; $result.matched = $info; break }
        } else {
          [void]$nodes.Add($info)
        }
        if ($depth -lt $MaxDepth) {
          $kids = New-Object System.Collections.ArrayList
          try {
            $child = $walker.GetFirstChild($el)
            while ($child -ne $null -and $kids.Count -lt 300) {
              [void]$kids.Add($child)
              $child = $walker.GetNextSibling($child)
            }
          } catch { }
          for ($i = $kids.Count - 1; $i -ge 0; $i--) { $stack.Push(@($kids[$i], ($depth + 1))) }
        }
      }
    }
    if ($Click) {
      if ($target -eq $null) {
        $result.error = 'no matching element'
      } else {
        $pat = $null
        if ($target.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$pat)) {
          try { $pat.Invoke(); $result.clicked = $true; $result.method = 'InvokePattern' } catch { $result.invoke_error = [string]$_.Exception.Message }
        }
        if (-not $result.clicked) {
          Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public static class W0Mouse { [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y); [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e); public static void Click(int x, int y) { SetCursorPos(x, y); mouse_event(2, 0, 0, 0, UIntPtr.Zero); mouse_event(4, 0, 0, 0, UIntPtr.Zero); } }'
          $r = $target.Current.BoundingRectangle
          if ((-not $r.IsEmpty) -and ($r.Width -gt 0) -and ($r.Height -gt 0)) {
            [W0Mouse]::Click([int]($r.X + $r.Width / 2), [int]($r.Y + $r.Height / 2))
            $result.clicked = $true
            $result.method = 'mouse'
          } else {
            $result.error = 'Invoke is not supported and the element has no bounding rectangle'
          }
        }
      }
    }
  }
} catch {
  $result.error = ([string]$_.Exception.GetType().Name) + ': ' + ([string]$_.Exception.Message)
}
$result.elapsed_ms = $sw.ElapsedMilliseconds
$json = $result | ConvertTo-Json -Depth 6 -Compress
if ($Out) { [System.IO.File]::WriteAllText($Out, $json, (New-Object System.Text.UTF8Encoding($false))) }
Write-Output ('uia-walk done: windows={0} nodes={1} clicked={2}' -f $windows.Count, $nodes.Count, $result.clicked)
"""

SCREEN_READER_PS = r"""
param([string]$Mode = 'get', [int]$Value = 1, [string]$Out)
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$ErrorActionPreference = 'Stop'
$result = [ordered]@{ mode = $Mode; before = $null; after = $null; ok = $false; error = $null }
try {
  Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public static class W0Spi { [DllImport("user32.dll", SetLastError = true)] public static extern bool SystemParametersInfo(uint action, uint param, ref int value, uint winIni); [DllImport("user32.dll", SetLastError = true)] public static extern bool SystemParametersInfo(uint action, uint param, IntPtr value, uint winIni); }'
  $v = 0
  [void][W0Spi]::SystemParametersInfo(0x46, 0, [ref]$v, 0)
  $result.before = $v
  if ($Mode -eq 'set') {
    [void][W0Spi]::SystemParametersInfo(0x47, [uint32]$Value, [IntPtr]::Zero, 3)
    $w = 0
    [void][W0Spi]::SystemParametersInfo(0x46, 0, [ref]$w, 0)
    $result.after = $w
  } else {
    $result.after = $v
  }
  $result.ok = $true
} catch {
  $result.error = ([string]$_.Exception.GetType().Name) + ': ' + ([string]$_.Exception.Message)
}
$json = $result | ConvertTo-Json -Compress
if ($Out) { [System.IO.File]::WriteAllText($Out, $json, (New-Object System.Text.UTF8Encoding($false))) }
Write-Output $json
"""

SCREENSHOT_PS = r"""
param([string]$Out)
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$ErrorActionPreference = 'Stop'
try {
  Add-Type -AssemblyName System.Windows.Forms
  Add-Type -AssemblyName System.Drawing
  $b = [System.Windows.Forms.SystemInformation]::VirtualScreen
  $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($b.Left, $b.Top, 0, 0, $bmp.Size)
  $bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
  Write-Output ('screenshot {0}x{1}' -f $b.Width, $b.Height)
} catch {
  Write-Output ('screenshot failed: ' + [string]$_.Exception.Message)
  exit 1
}
"""

NET_WATCH_PS = r"""
param([string]$Out, [string]$Stop, [int]$IntervalMs = 1000, [string]$Names = 'arc-desktop,msedgewebview2', [string]$DnsName = 'api.github.com')
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$ErrorActionPreference = 'SilentlyContinue'
$seen = @{}
$polls = 0
$utf8 = New-Object System.Text.UTF8Encoding($false)
function Write-Line($obj) { [System.IO.File]::AppendAllText($Out, (($obj | ConvertTo-Json -Compress) + [Environment]::NewLine), $utf8) }
Write-Line ([ordered]@{ event = 'start'; t = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() / 1000.0; interval_ms = $IntervalMs; names = $Names })
while (-not (Test-Path -LiteralPath $Stop)) {
  $polls = $polls + 1
  $ids = @{}
  foreach ($p in @(Get-Process -Name ($Names -split ',') -ErrorAction SilentlyContinue)) { $ids[[int]$p.Id] = [string]$p.ProcessName }
  if ($ids.Count -gt 0) {
    foreach ($c in @(Get-NetTCPConnection -ErrorAction SilentlyContinue | Where-Object { $ids.ContainsKey([int]$_.OwningProcess) })) {
      $key = '{0}|{1}|{2}|{3}' -f $c.OwningProcess, $c.RemoteAddress, $c.RemotePort, $c.State
      if (-not $seen.ContainsKey($key)) {
        $seen[$key] = $true
        Write-Line ([ordered]@{ event = 'conn'; t = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() / 1000.0; poll = $polls; pid = [int]$c.OwningProcess; proc = $ids[[int]$c.OwningProcess]; local = ('{0}:{1}' -f $c.LocalAddress, $c.LocalPort); remote = [string]$c.RemoteAddress; port = [int]$c.RemotePort; state = [string]$c.State })
      }
    }
  }
  foreach ($d in @(Get-DnsClientCache -ErrorAction SilentlyContinue)) {
    if (([string]$d.Name).ToLower() -ne $DnsName) { continue }
    $dk = 'dns|{0}|{1}' -f $d.Type, $d.Data
    if (-not $seen.ContainsKey($dk)) {
      $seen[$dk] = $true
      Write-Line ([ordered]@{ event = 'dns'; t = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() / 1000.0; poll = $polls; name = [string]$d.Name; type = [string]$d.Type; data = [string]$d.Data; ttl = [int]$d.TimeToLive })
    }
  }
  if (($polls % 5) -eq 1) { Write-Line ([ordered]@{ event = 'poll'; t = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() / 1000.0; poll = $polls; processes = $ids.Count }) }
  Start-Sleep -Milliseconds $IntervalMs
}
Write-Line ([ordered]@{ event = 'stop'; t = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() / 1000.0; polls = $polls })
"""

DNS_CACHE_PS = r"""
param([string]$Out, [string]$Names)
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$ErrorActionPreference = 'SilentlyContinue'
$wanted = $Names -split ','
$rows = New-Object System.Collections.ArrayList
foreach ($e in @(Get-DnsClientCache -ErrorAction SilentlyContinue)) {
  if ($wanted -contains ([string]$e.Name).ToLower()) { [void]$rows.Add([ordered]@{ name = [string]$e.Name; type = [string]$e.Type; data = [string]$e.Data; ttl = [int]$e.TimeToLive }) }
}
$json = ConvertTo-Json -InputObject @($rows) -Compress
[System.IO.File]::WriteAllText($Out, $json, (New-Object System.Text.UTF8Encoding($false)))
Write-Output ('dns cache rows: {0}' -f $rows.Count)
"""

PS_PARSE_PS = r"""
param([string]$Dir, [string]$Out)
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$ErrorActionPreference = 'Stop'
$result = [ordered]@{}
try {
  foreach ($f in @(Get-ChildItem -LiteralPath $Dir -Filter '*.ps1')) {
    $tokens = $null
    $errors = $null
    [void][System.Management.Automation.Language.Parser]::ParseFile($f.FullName, [ref]$tokens, [ref]$errors)
    $messages = New-Object System.Collections.ArrayList
    foreach ($e in @($errors)) { if ($e -ne $null) { [void]$messages.Add(('{0}:{1} {2}' -f $e.Extent.StartLineNumber, $e.Extent.StartColumnNumber, $e.Message)) } }
    $result[$f.Name] = $messages
  }
} catch {
  $result['_error'] = ([string]$_.Exception.GetType().Name) + ': ' + ([string]$_.Exception.Message)
}
$json = $result | ConvertTo-Json -Depth 4 -Compress
[System.IO.File]::WriteAllText($Out, $json, (New-Object System.Text.UTF8Encoding($false)))
Write-Output ('parsed {0} scripts' -f $result.Count)
"""

POWERSHELL_FILE_ARGV = ["powershell", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]


def write_ps_scripts(directory: Path) -> Dict[str, Path]:
    """The PowerShell scripts as files (so no quoting problem can reach them); returns name -> path."""
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    scripts = {"uia-walk": UIA_WALK_PS, "screen-reader": SCREEN_READER_PS, "screenshot": SCREENSHOT_PS, "net-watch": NET_WATCH_PS, "dns-cache": DNS_CACHE_PS}
    paths: Dict[str, Path] = {}
    for name, text in scripts.items():
        path = directory / (name + ".ps1")
        path.write_text(text.lstrip("\n"), encoding="utf-8")
        paths[name] = path
    return paths


def ps_file_argv(script: Path, *args: str) -> List[str]:
    return POWERSHELL_FILE_ARGV + [str(script)] + [str(a) for a in args]


def check_ps_scripts(ctx: "Context") -> Dict[str, Any]:
    """The PowerShell parser (nothing is executed) over every script this module writes: a syntax error shows up in seconds, not after a 90 s wait for a window.
    ran=False means the check itself could not run (that never blocks the strategy); errors is name -> parser messages."""
    directory = ctx.work / "uia"
    scripts = write_ps_scripts(directory)
    parser = directory / "ps-parse.ps1"
    parser.write_text(PS_PARSE_PS.lstrip("\n"), encoding="utf-8")
    out = directory / "ps-parse.json"
    if out.exists():
        out.unlink()
    done = ctx.shell.run(ps_file_argv(parser, "-Dir", str(directory), "-Out", str(out)), timeout=120, quiet=True)
    value = parse_json_file(out)
    if value is None:
        return {"ran": False, "ok": False, "checked": 0, "errors": {}, "detail": mask_ips(done.out.strip()[-300:]) or "no output (rc %d)" % done.rc}
    errors: Dict[str, List[str]] = {}
    for name, messages in value.items():
        found = [messages] if isinstance(messages, str) else [str(m) for m in (messages or [])]
        if found:
            errors[name] = found
    missing = sorted(path.name for path in scripts.values() if path.name not in value)
    if missing:
        errors["_missing"] = ["not seen by the parser: %s" % ", ".join(missing)]
    return {"ran": True, "ok": not errors, "checked": len(value), "errors": errors}


# --- reading the UIA tree ------------------------------------------------------------------------------------------

def parse_json_file(path: Path) -> Optional[dict]:
    try:
        value = json.loads(Path(path).read_text(encoding="utf-8", errors="replace"))
    except (OSError, ValueError):
        return None
    return value if isinstance(value, dict) else None


def uia_matches(node: dict, pattern: str, types: Sequence[str] = ()) -> bool:
    if types and node.get("type") not in types:
        return False
    return re.search(pattern, str(node.get("name") or ""), re.IGNORECASE) is not None


def uia_find(dump: Optional[dict], pattern: str, types: Sequence[str] = ()) -> List[dict]:
    return [n for n in (dump or {}).get("nodes") or [] if isinstance(n, dict) and uia_matches(n, pattern, types)]


def uia_names(dump: Optional[dict], types: Sequence[str] = ()) -> List[str]:
    """Names of the nodes (optionally of the given control types), in document order, empty names dropped, whitespace kept as rendered."""
    return [str(n.get("name")) for n in (dump or {}).get("nodes") or [] if isinstance(n, dict) and str(n.get("name") or "").strip() and (not types or n.get("type") in types)]


CARD_TEXT = re.compile(r"(?i)^updates?$|latest version|is available|update failed|install|^v[0-9A-Za-z.\-]+$|no update|could not|release json")
BUTTON_TYPES = ("Button", "Hyperlink")


def uia_card_texts(dump: Optional[dict]) -> List[str]:
    return [text for text in uia_names(dump) if CARD_TEXT.search(text)][:14]


def summarize_uia(dump: Optional[dict]) -> Dict[str, Any]:
    """What the sequence needs to know about a tree: the three buttons, the Updates card texts, sizes."""
    if not dump:
        return {"available": False, "windows": 0, "nodes": 0, "settings_button": False, "check_for_updates_button": False, "install_button": False, "update_card_texts": []}
    return {
        "available": True,
        "error": dump.get("error"),
        "windows": len(dump.get("windows") or []),
        "nodes": len(dump.get("nodes") or []),
        "truncated": bool(dump.get("truncated")),
        "settings_button": bool(uia_find(dump, SETTINGS_PATTERN, BUTTON_TYPES)),
        "check_for_updates_button": bool(uia_find(dump, CHECK_PATTERN, BUTTON_TYPES)),
        "install_button": bool(uia_find(dump, INSTALL_PATTERN, BUTTON_TYPES)),
        "update_card_texts": uia_card_texts(dump),
    }


def join_update_error(dump: Optional[dict]) -> Dict[str, Any]:
    """The Updates card after the Install click renders "Update failed: " and the plugin's message as two ADJACENT text nodes (the macOS tree showed exactly that).
    Join them; separately search every text value for the exact ReleaseNotFound string. Nothing found => found False (the caller fails closed)."""
    nodes = [n for n in (dump or {}).get("nodes") or [] if isinstance(n, dict)]
    texts = [(i, n) for i, n in enumerate(nodes) if str(n.get("name") or "").strip() and n.get("type") in ("Text", "Custom", "Group", "ListItem", "Document", "Pane", "Edit")]
    joined = ""
    for position, (index, node) in enumerate(texts):
        if re.match(r"^\s*Update failed", str(node.get("name"))):
            parts = [str(node.get("name"))]
            for _i, follower in texts[position + 1:position + 4]:
                if follower.get("depth") == node.get("depth") and follower.get("type") == node.get("type") and not re.search(r"(?i)^(check for updates|install)", str(follower.get("name"))):
                    parts.append(str(follower.get("name")))
                else:
                    break
            joined = "".join(parts) if parts[0].endswith(" ") else " ".join(parts)
            break
    all_text = " ".join(str(n.get("name")) for n in nodes if str(n.get("name") or "").strip())
    release_not_found = RELEASE_NOT_FOUND in all_text or RELEASE_NOT_FOUND in joined
    if release_not_found and not joined:
        joined = RELEASE_NOT_FOUND
    message = None
    match = re.match(r"^\s*Update failed:\s*(.*)$", joined, re.DOTALL)
    if match and match.group(1).strip():
        message = match.group(1).strip()
    elif release_not_found:
        message = RELEASE_NOT_FOUND
    return {"found": bool(joined), "joined": joined.strip(), "message": message, "release_not_found": release_not_found}


class UiaPage:
    """The released app's window through Windows UI Automation (PowerShell + System.Windows.Automation) on the CI runner:
    Settings > Check for updates > (the banner's own result) > Install. kind "uia"; same probe/trigger interface as the other pages."""
    kind = "uia"

    def __init__(self, ctx: "Context", pid: int, evidence: Path, stem: str, sleep: Optional[Callable[[float], None]] = None, clock: Optional[Callable[[], float]] = None,
                 real_banner: bool = True):
        self.ctx, self.pid, self.evidence, self.stem = ctx, pid, Path(evidence), stem
        self.sleep = sleep or time.sleep
        self.clock = clock or time.time
        self.real_banner = real_banner
        self.scripts = write_ps_scripts(ctx.work / "uia")
        self.counter = 0
        self.ui: Dict[str, Any] = {"attempted": True, "trigger": "Windows UI Automation (System.Windows.Automation via PowerShell): Settings > Check for updates > Install",
                                   "steps": [], "pid": pid, "real_banner_api": real_banner, "window_found": False}
        self.files: List[str] = []

    # -- one PowerShell walk ------------------------------------------------------------------------------------
    def walk(self, *extra: str, timeout: float = 150.0) -> Tuple[Optional[dict], CmdResult]:
        self.counter += 1
        out = self.ctx.work / "uia" / ("walk-%s-%d.json" % (self.stem, self.counter))
        if out.exists():
            out.unlink()
        argv = ps_file_argv(self.scripts["uia-walk"], "-ProcessId", str(self.pid), "-Out", str(out), *extra)
        result = self.ctx.shell.run(argv, timeout=timeout)
        return parse_json_file(out), result

    def dump(self, name: Optional[str], max_nodes: int = 900, budget_ms: int = 60000, windows_only: bool = False) -> Optional[dict]:
        extra = ["-MaxNodes", str(max_nodes), "-BudgetMs", str(budget_ms)]
        if windows_only:
            extra.append("-WindowsOnly")
        dump, result = self.walk(*extra, timeout=budget_ms / 1000.0 + 60)
        if name:
            record = dump if dump else {"raw": mask_ips(result.out[-600:]), "rc": result.rc}
            file_name = "uia-%s-%s.json" % (self.stem, name)
            write_json(self.evidence / file_name, record)
            self.files.append(file_name)
        return dump

    def click(self, pattern: str, label: str, types: Sequence[str] = BUTTON_TYPES) -> Dict[str, Any]:
        """Click the first enabled element whose Name matches; InvokePattern first, a bounding-rectangle mouse click only when Invoke is unsupported (the report says which)."""
        report, result = self.walk("-Click", "-Pattern", pattern, "-Types", ",".join(types), "-MaxNodes", "1500", "-BudgetMs", "60000", timeout=150)
        step = {"label": label, "pattern": pattern, "rc": result.rc, "report": {k: report.get(k) for k in ("clicked", "method", "matched", "visited", "error", "invoke_error", "truncated")} if report else None}
        if report is not None and not report.get("clicked") and report.get("error") == "no matching element" and set(types) == set(BUTTON_TYPES):
            report, result = self.walk("-Click", "-Pattern", pattern, "-Types", "Text,Custom,Group,ListItem,TabItem,MenuItem,Pane", "-MaxNodes", "1500", "-BudgetMs", "60000", timeout=150)
            step["retry_report"] = {k: report.get(k) for k in ("clicked", "method", "matched", "visited", "error", "invoke_error")} if report else None
        if report is None:
            step["raw"] = mask_ips(result.out.strip()[-400:])
        self.ui["steps"].append(step)
        return step

    @staticmethod
    def clicked(step: Dict[str, Any]) -> bool:
        return bool((step.get("report") or {}).get("clicked") or (step.get("retry_report") or {}).get("clicked"))

    # -- lifecycle ----------------------------------------------------------------------------------------------
    def open(self, window_timeout: float = 90.0, content_timeout: float = 60.0) -> Dict[str, Any]:
        """Wait for the app window, then for the web content tree (it appears lazily, once the first UIA client asks)."""
        start = self.clock()
        dump = None
        while self.clock() - start < window_timeout:
            dump = self.dump(None, max_nodes=1, budget_ms=15000, windows_only=True)
            if dump and (dump.get("windows") or []):
                self.ui["window_found"] = True
                self.ui["windows"] = dump["windows"]
                break
            self.sleep(3)
        if not self.ui["window_found"]:
            raise UiaWindowNotFound("no window of pid %d appeared within %.0f s" % (self.pid, window_timeout))
        start = self.clock()
        dump = None
        while self.clock() - start < content_timeout:
            dump = self.dump(None, max_nodes=900, budget_ms=60000)
            if dump and summarize_uia(dump)["settings_button"]:
                break
            self.sleep(5)
        self.launch_dump = dump
        write_json(self.evidence / ("uia-%s-1-launch.json" % self.stem), dump or {"error": "no tree"})
        self.files.append("uia-%s-1-launch.json" % self.stem)
        self.ui["after_launch"] = summarize_uia(dump)
        if not self.ui["after_launch"]["settings_button"]:
            raise UiaContentNotFound("the window of pid %d is there but its web content never exposed a Settings button (nodes: %s)" % (self.pid, self.ui["after_launch"].get("nodes")))
        return self.ui["after_launch"]

    def evaluate_probe(self) -> Tuple[bool, Any, Optional[str]]:
        summary = self.ui.get("after_launch") or {}
        windows = self.ui.get("windows") or [{}]
        return True, {"ipc": None, "href": None, "title": windows[0].get("name"), "app_version": None, "nodes": summary.get("nodes"), "path": "uia"}, None

    def evaluate_trigger(self) -> Tuple[bool, Any, Optional[str]]:
        ui = self.ui
        self.click(SETTINGS_PATTERN, "click Settings")
        self.sleep(3)
        settings = self.dump("2-settings")
        ui["settings_page"] = summarize_uia(settings)
        ui["install_button_before_check"] = ui["settings_page"]["install_button"]
        attempts: List[Dict[str, Any]] = []
        dump = None
        for attempt in range(1, 5):          # the first click and up to three more, 20 s apart (the banner call may have been rate limited)
            if attempt > 1:
                self.sleep(20)
            self.click(CHECK_PATTERN, "click Check for updates" if attempt == 1 else "click Check for updates (retry %d)" % (attempt - 1))
            dump = None
            for _ in range(7):               # the banner command has an 8 s timeout; with the real API it answers in well under a second
                self.sleep(4)
                dump = self.dump(None)
                if summarize_uia(dump)["install_button"]:
                    break
            found = summarize_uia(dump)["install_button"]
            attempts.append({"attempt": attempt, "install_button": found, "card_texts": uia_card_texts(dump)})
            if found:
                break
        ui["banner_attempts"] = attempts
        write_json(self.evidence / ("uia-%s-3-after-check.json" % self.stem), dump or {"error": "no tree"})
        self.files.append("uia-%s-3-after-check.json" % self.stem)
        ui["after_check"] = summarize_uia(dump)
        ui["install_button"] = ui["after_check"]["install_button"]
        if not ui["install_button"]:
            return self._not_reached(self._no_install_reason(attempts))
        if ui["install_button_before_check"]:
            return self._not_reached("the Install button was already present BEFORE the check: not clicked (it would not prove the banner's answer)")
        step = self.click(INSTALL_PATTERN, "click Install (calls the plugin check())")
        ui["install_clicked"] = self.clicked(step)
        if not ui["install_clicked"]:
            return self._not_reached("the Install button was rendered but could not be clicked: %s" % json.dumps(step.get("report") or step.get("retry_report") or step.get("raw"))[:300])
        final = None
        error: Dict[str, Any] = {"found": False}
        for _ in range(8):
            self.sleep(4)
            final = self.dump(None)
            error = join_update_error(final)
            if error["found"] and (error["release_not_found"] or error["message"]) or "No update available" in " ".join(uia_names(final)):
                break
        write_json(self.evidence / ("uia-%s-4-after-install.json" % self.stem), final or {"error": "no tree"})
        self.files.append("uia-%s-4-after-install.json" % self.stem)
        ui["after_install"] = summarize_uia(final)
        ui["update_error"] = error
        ui["ui_error_text"] = error.get("joined") or None
        if error["release_not_found"]:
            text = error["joined"] if RELEASE_NOT_FOUND in error["joined"] else ("%s %s" % (error["joined"], RELEASE_NOT_FOUND)).strip()
        else:
            text = error["joined"] or "(no 'Update failed' text found after the Install click; card texts: %s)" % uia_card_texts(final)
        return True, {"ok": False, "stage": "ui", "error": text, "error_type": "ui-text", "error_json": None}, None

    def _no_install_reason(self, attempts: List[Dict[str, Any]]) -> str:
        texts = " ".join((attempts[-1].get("card_texts") or [])) if attempts else ""
        if not self.real_banner:
            return "the Install button was not rendered: Settings.tsx shows it only after check_for_update reports an update, and with real-banner mode off that call cannot succeed (negative control)"
        if re.search(r"(?i)vunknown|\bunknown\b", texts):
            return "the banner's own unintercepted API call returned no release tag (UI: %s) after %d click(s) 20 s apart; not attributable from here" % (texts, len(attempts))
        return "the Install button was not rendered after %d Check for updates click(s) (UI: %s); not attributable from here" % (len(attempts), texts or "no card text found")

    def _not_reached(self, reason: str) -> Tuple[bool, Any, Optional[str]]:
        self.ui["problem"] = reason
        return True, {"ok": False, "stage": "ui-not-reached", "error": reason}, None

    def close(self) -> None:
        pass


class UiaWindowNotFound(RuntimeError):
    """The app has no window at all: the other strategies may still find one."""


class UiaContentNotFound(RuntimeError):
    """The window exists but UI Automation does not see the web content: the other strategies cannot do better (no debugging port), so none is tried."""


# --- what the app's processes talk to ------------------------------------------------------------------------------

def set_screen_reader(ctx: "Context", value: int = 1) -> Dict[str, Any]:
    """SPI_SETSCREENREADER: Chromium builds its accessibility tree when it believes a screen reader runs. Best effort; the previous value is kept for the restore."""
    out = ctx.work / "uia" / "screen-reader.json"
    scripts = write_ps_scripts(ctx.work / "uia")
    done = ctx.shell.run(ps_file_argv(scripts["screen-reader"], "-Mode", "set", "-Value", str(value), "-Out", str(out)), timeout=60, quiet=True)
    info = parse_json_file(out) or {"error": done.out[-200:]}
    if ctx.screen_reader_before is None and isinstance(info.get("before"), int):
        ctx.screen_reader_before = info["before"]
    return info


def restore_screen_reader(ctx: "Context") -> None:
    if ctx.screen_reader_before is None:
        return
    scripts = write_ps_scripts(ctx.work / "uia")
    ctx.shell.run(ps_file_argv(scripts["screen-reader"], "-Mode", "set", "-Value", str(ctx.screen_reader_before), "-Out", str(ctx.work / "uia" / "screen-reader-restore.json")), timeout=60, quiet=True)
    ctx.screen_reader_before = None


def take_screenshot(ctx: "Context", name: str, files: Optional[List[str]] = None) -> Optional[str]:
    """A PNG of the runner's screen (System.Drawing CopyFromScreen); best effort, recorded in the case's evidence list only when it exists."""
    scripts = write_ps_scripts(ctx.work / "uia")
    target = ctx.evidence / name
    ctx.shell.run(ps_file_argv(scripts["screenshot"], "-Out", str(target)), timeout=60, quiet=True)
    if target.is_file() and target.stat().st_size > 0:
        if files is not None and name not in files:
            files.append(name)
        return name
    return None


def parse_nslookup_addresses(text: str) -> List[str]:
    """Addresses from `nslookup NAME` output: only those after the "Name:" line (the lines before it name the DNS server)."""
    lines = text.splitlines()
    start = next((i for i, line in enumerate(lines) if re.match(r"^\s*Name:", line)), None)
    if start is None:
        return []
    found: List[str] = []
    for line in lines[start:]:
        if re.match(r"^\s*(Aliases?:)", line):
            continue
        for token in re.findall(r"[0-9A-Fa-f:.]{3,}", line.split(":", 1)[1] if re.match(r"^\s*(Name|Address|Addresses):", line) else line):
            address = canonical_address(token)
            if address and address not in found:
                found.append(address)
    return found


def canonical_address(value: str) -> Optional[str]:
    """Compressed, scope-less, IPv4-mapped addresses unwrapped; None when it is not an IP address."""
    import ipaddress
    text = str(value).strip().strip("[]").split("%")[0]
    try:
        address = ipaddress.ip_address(text)
    except ValueError:
        return None
    if address.version == 6 and getattr(address, "ipv4_mapped", None):
        return str(address.ipv4_mapped)
    return str(address)


def is_loopback_address(value: str) -> bool:
    import ipaddress
    address = canonical_address(value)
    if not address:
        return False
    return ipaddress.ip_address(address).is_loopback


def is_unspecified_address(value: str) -> bool:
    address = canonical_address(value)
    return address in ("0.0.0.0", "::", None)


def resolve_real(shell: "Shell", name: str) -> List[str]:
    """The addresses a name has in the REAL DNS right now, through nslookup (it does not read the hosts file, so it works after the mapping, too)."""
    done = shell.run(["nslookup", name], timeout=45, quiet=True)
    return parse_nslookup_addresses(done.out) if done.out else []


def rsms_block_script(addresses: Sequence[str], rule: str = RSMS_RULE) -> str:
    """Replace the second-line block rule for the real addresses of rsms.me (the hosts mapping alone is not trusted to keep the webview off it)."""
    remove = "Remove-NetFirewallRule -DisplayName '%s' -ErrorAction SilentlyContinue; " % rule
    if not addresses:
        return remove + "Write-Output 'no addresses to block'"
    quoted = ",".join("'%s'" % a for a in addresses)
    return remove + "New-NetFirewallRule -DisplayName '%s' -Direction Outbound -Action Block -RemoteAddress %s | Out-Null; Write-Output 'rule-created'" % (rule, quoted)


def webview2_block_script(program: str, rule: str = WEBVIEW2_RULE) -> str:
    """Block ALL outbound traffic of the WebView2 runtime's browser binary: the page needs nothing from the Internet, and its own platform services (component
    updater, variations, SmartScreen) must not reach it either. The banner's request is made by arc-desktop.exe itself, not by the webview."""
    quoted = program.replace("'", "''")
    return ("Remove-NetFirewallRule -DisplayName '%s' -ErrorAction SilentlyContinue; "
            "New-NetFirewallRule -DisplayName '%s' -Direction Outbound -Action Block -Program '%s' | Out-Null; Write-Output 'rule-created'" % (rule, rule, quoted))


def find_webview2_binaries(patterns: Sequence[str] = ()) -> List[str]:
    import glob
    roots = list(patterns) or [r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application\*\msedgewebview2.exe", r"C:\Program Files\Microsoft\EdgeWebView\Application\*\msedgewebview2.exe"]
    found: List[str] = []
    for pattern in roots:
        found.extend(sorted(glob.glob(pattern)))
    return found


def refresh_extra_blocks(ctx: "Context") -> Dict[str, Any]:
    """Before every case: the real addresses of rsms.me as the resolver gives them now are added to the block (an independent second line behind the hosts
    mapping), and the WebView2 runtime's browser binary is blocked from the Internet. Returns what was done; never raises."""
    info: Dict[str, Any] = {"rsms_resolved": [], "rsms_rule": None, "webview2_programs": [], "webview2_rule": None}
    try:
        resolved = resolve_real(ctx.shell, RSMS_HOST)
        ctx.rsms_addresses = sorted(set(ctx.rsms_addresses) | set(resolved))
        info["rsms_resolved"] = resolved
        info["rsms_addresses_blocked"] = list(ctx.rsms_addresses)
        done = ctx.shell.run(powershell_argv(rsms_block_script(ctx.rsms_addresses)), timeout=90)
        ctx.rsms_rule_created = ctx.rsms_rule_created or (done.ok and bool(ctx.rsms_addresses))
        info["rsms_rule"] = "created" if (done.ok and ctx.rsms_addresses) else ("no addresses resolved" if done.ok else "failed: %s" % done.out[-200:])
        programs = find_webview2_binaries() if ctx.webview2_block else []
        info["webview2_programs"] = programs
        if not ctx.webview2_block:
            info["webview2_rule"] = "disabled (--no-webview2-block): the runtime's egress is observed, not blocked"
        elif programs:
            made_all = []
            for index, program in enumerate(programs):
                rule = WEBVIEW2_RULE if index == 0 else "%s-%d" % (WEBVIEW2_RULE, index)         # one rule per program: a rule with the same name would replace the earlier one
                made = ctx.shell.run(powershell_argv(webview2_block_script(program, rule)), timeout=90)
                ctx.webview2_rule_created = ctx.webview2_rule_created or made.ok
                made_all.append("created" if made.ok else "failed: %s" % made.out[-200:])
            info["webview2_rule"] = made_all[0] if len(set(made_all)) == 1 else "; ".join(made_all)
        else:
            info["webview2_rule"] = "msedgewebview2.exe not found: its egress is observed, not blocked"
    except Exception as error:  # noqa: BLE001 - recorded
        info["error"] = "%s: %s" % (type(error).__name__, error)
    ctx.extra_block_log.append(info)
    return info


class NetWatcher:
    """A PowerShell process that polls Get-NetTCPConnection for arc-desktop.exe and its msedgewebview2.exe children about once a second and writes one JSON line per new
    (process, remote endpoint, state). One long-lived process: starting PowerShell once a second would be slower than the poll."""

    def __init__(self, ctx: "Context", stem: str, interval_ms: int = 1000, popen: Optional[Callable[..., Any]] = None):
        self.ctx, self.stem, self.interval_ms = ctx, stem, interval_ms
        self.popen = popen
        self.out = ctx.work / ("netwatch-%s.jsonl" % stem)
        self.stop_file = ctx.work / ("netwatch-%s.stop" % stem)
        self.proc: Any = None
        self.handle: Any = None
        self.error: Optional[str] = None

    def start(self) -> None:
        scripts = write_ps_scripts(self.ctx.work / "uia")
        for path in (self.out, self.stop_file):
            if path.exists():
                path.unlink()
        argv = ps_file_argv(scripts["net-watch"], "-Out", str(self.out), "-Stop", str(self.stop_file), "-IntervalMs", str(self.interval_ms))
        try:
            self.handle = (self.ctx.work / ("netwatch-%s.out" % self.stem)).open("wb")
            self.proc = (self.popen or subprocess.Popen)(argv, stdout=self.handle, stderr=subprocess.STDOUT)
        except OSError as error:
            self.error = "%s: %s" % (type(error).__name__, error)

    def stop(self, timeout: float = 20.0) -> List[dict]:
        records: List[dict] = []
        if self.proc is not None:
            try:
                self.stop_file.write_text("stop", encoding="ascii")
                try:
                    self.proc.wait(timeout=timeout)
                except subprocess.TimeoutExpired:
                    self.proc.kill()
            except OSError as error:
                self.error = "%s: %s" % (type(error).__name__, error)
        if self.handle is not None:
            self.handle.close()
        return read_jsonl(self.out) or records


def dns_cache_entries(ctx: "Context", names: Sequence[str], stem: str) -> List[dict]:
    scripts = write_ps_scripts(ctx.work / "uia")
    out = ctx.work / ("dnscache-%s.json" % stem)
    ctx.shell.run(ps_file_argv(scripts["dns-cache"], "-Out", str(out), "-Names", ",".join(n.lower() for n in names)), timeout=60, quiet=True)
    try:
        value = json.loads(out.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    return [row for row in value if isinstance(row, dict)] if isinstance(value, list) else []


def label_live(address: str, live_ips: Sequence[str]) -> str:
    """ARC addresses never appear in evidence: live-ip-N."""
    canon = canonical_address(address) or address
    for index, live in enumerate(sorted(live_ips), 1):
        if canonical_address(live) == canon:
            return "live-ip-%d" % index
    return address


def network_report_win(records: Sequence[dict], api_ips: Sequence[str], live_ips: Sequence[str], rsms_ips: Sequence[str], real_banner: bool,
                       watcher_error: Optional[str] = None) -> Dict[str, Any]:
    """Judge what the app's process tree talked to. Allowed: loopback (the recorder, local IPC) and, in real-banner mode, the api.github.com addresses on 443.
    A non-established attempt at an ARC or rsms.me address is information (the firewall refused it); an ESTABLISHED one means a block failed. Anything else is a violation."""
    api = {canonical_address(a) for a in api_ips if canonical_address(a)}
    observed = {canonical_address(str(r.get("data") or "")) for r in records if r.get("event") == "dns" and canonical_address(str(r.get("data") or ""))}
    api |= observed                    # what the system resolver handed the app, sampled every second (a cache entry lives only as long as its TTL, 60 s)
    live = {canonical_address(a) for a in live_ips if canonical_address(a)}
    rsms = {canonical_address(a) for a in rsms_ips if canonical_address(a)}
    merged: Dict[Tuple[Any, ...], Dict[str, Any]] = {}
    polls = 0
    started = stopped = False
    for record in records:
        event = record.get("event")
        if event == "start":
            started = True
        elif event == "stop":
            stopped = True
            polls = max(polls, int(record.get("polls") or 0))
        elif event == "poll":
            polls = max(polls, int(record.get("poll") or 0))
        elif event == "conn":
            address = canonical_address(str(record.get("remote") or ""))
            if address is None:
                continue
            key = (record.get("pid"), record.get("proc"), address, record.get("port"))
            item = merged.setdefault(key, {"pid": record.get("pid"), "proc": record.get("proc"), "remote_ip": address, "remote_port": record.get("port"), "states": [], "first_seen": record.get("t")})
            if record.get("state") and record["state"] not in item["states"]:
                item["states"].append(record["state"])
    allowed: List[dict] = []
    blocked: List[dict] = []
    violations: List[dict] = []
    loopback = listeners = 0
    for item in merged.values():
        address, port, states = item["remote_ip"], item["remote_port"], item["states"]
        if is_unspecified_address(address) or (states and all(s in ("Listen", "Bound") for s in states)):
            listeners += 1
        elif is_loopback_address(address):
            loopback += 1
        elif real_banner and address in api and port == 443:
            allowed.append(dict(item, label="api.github.com"))
        elif address in live:
            entry = dict(item, remote_ip=label_live(address, live_ips), label="arc-live-node")
            (violations if "Established" in states else blocked).append(entry)
        elif address in rsms:
            entry = dict(item, label="rsms.me")
            (violations if "Established" in states else blocked).append(entry)
        else:
            violations.append(dict(item, label="other real destination"))
    return {
        "recorded": bool(started and (polls > 0 or merged)) and watcher_error is None,
        "watcher_polls": polls, "watcher_started": started, "watcher_stopped": stopped, "watcher_error": watcher_error,
        "real_banner": real_banner,
        "allowed_banner_endpoints": allowed, "blocked_attempts": blocked, "violations": violations,
        "loopback_endpoints": loopback, "listener_entries": listeners,
        "api_addresses": sorted(api), "api_addresses_seen_in_the_resolver_cache": sorted(observed), "rsms_addresses": sorted(rsms),
    }


def selective_mask(text: str, live_ips: Sequence[str]) -> str:
    """Only the ARC addresses are masked (live-ip-N); the addresses that name a violation or label the banner call must stay readable."""
    for index, live in enumerate(sorted(live_ips), 1):
        text = text.replace(live, "live-ip-%d" % index)
    return text


def strategy_order(ctx: Context) -> List[str]:
    """The configured strategies, the one that worked in an earlier case first."""
    order = list(ctx.strategies)
    if ctx.winning_strategy in order:
        order.remove(ctx.winning_strategy)
        order.insert(0, ctx.winning_strategy)
    return order


def acquire_page(ctx: Context, tag: str, home: str, wv2_base: str, evidence: Path, files: List[str],
                 try_one: Optional[Callable[..., Launch]] = None) -> Tuple[Optional[Launch], List[dict]]:
    """Try the strategies in order until one gives a scripting handle on the app's window. Every attempt leaves a launched-process record
    (with the WebView2 command lines, to see whether --remote-debugging-port arrived) and a line in the attempts list."""
    attempts: List[dict] = []
    for strategy in strategy_order(ctx):
        kill_app(ctx, None, wv2_base)                                  # nothing from an earlier attempt or case may survive into this one
        left = leftover_processes(snapshot_processes(ctx.shell), [wv2_base, ctx.install_dir])
        wv2_launch = wv2_base.rstrip("\\") + "\\" + tag + "-" + strategy
        launch = (try_one or try_strategy)(ctx, strategy, tag, home, wv2_launch)
        launched = snapshot_processes(ctx.shell)
        name = "procs-%s-%s-launched.txt" % (tag, strategy)
        write_process_list(evidence / name, launched)
        files.append(name)
        lines = webview_command_lines(launched)
        attempts.append({
            "strategy": strategy, "ok": launch.ok, "error": launch.error, "user_data_folder": wv2_launch, "leftovers_before_launch": left,
            "webview2_process_count": len(lines),
            "remote_debugging_in_webview2_command_line": any("--remote-debugging" in line for line in lines),     # did the flag arrive at the browser process at all?
            "webview2_command_lines": lines, "notes": launch.notes,
        })
        if launch.ok:
            ctx.winning_strategy = strategy
            return launch, attempts
        close_launch(ctx, launch, wv2_base)
        if strategy == "uia" and launch.notes.get("window_found"):
            attempts[-1]["fallbacks_skipped"] = "the window exists but UI Automation does not see the web content; the debugging-port strategies cannot work here (the switches never arrive) and each failed strategy costs minutes"
            break
    return None, attempts


def copy_driver_log(ctx: Context, tag: str, evidence: Path, files: List[str]) -> None:
    """The masked tail of msedgedriver's own log (it records how it started the app) goes into the evidence."""
    for suffix in (".log", ".out"):
        source = ctx.work / ("msedgedriver-%s%s" % (tag, suffix))
        if source.is_file():
            lines = mask_ips(source.read_text(encoding="utf-8", errors="replace")).splitlines()[-300:]
            name = "msedgedriver-%s%s" % (tag, suffix)
            (evidence / name).write_text("\n".join(lines) + "\n", encoding="utf-8")
            files.append(name)


def write_masked_json(path: Path, value: Any, live_ips: Sequence[str]) -> None:
    """JSON evidence with the ARC addresses replaced by live-ip-N (every other address stays readable: it names a violation or labels the banner call)."""
    text = selective_mask(json.dumps(value, indent=2, sort_keys=True, default=str), live_ips)
    Path(path).write_text(text + "\n", encoding="utf-8")


def banner_addresses(ctx: Context, stem: str) -> List[str]:
    """api.github.com's addresses: resolved at run time on the runner (never hard-coded), plus what the system resolver cached for the app."""
    found = set(ctx.api_addresses)
    found.update(resolve_real(ctx.shell, BANNER_API_HOST))
    for row in dns_cache_entries(ctx, [BANNER_API_HOST], stem):
        address = canonical_address(str(row.get("data") or ""))
        if address:
            found.add(address)
    ctx.api_addresses = sorted(found)
    return ctx.api_addresses


def run_released_case(ctx: Context, name: str, state: Dict[str, str], settle_s: float = 10.0) -> Dict[str, Any]:
    """One released-app case: record, launch (UI Automation first, then the debugging strategies when UIA cannot even find the window), drive, record again, evaluate."""
    shell, evidence = ctx.shell, ctx.evidence
    scenario = ctx.scenario_for(name)
    tag = "app-" + name
    files: List[str] = []
    case: Dict[str, Any] = {"name": name, "tier": "released_app", "scenario": scenario, "trigger_outcome": {}, "evidence_files": files}
    requests_path = evidence / ("requests-%s.jsonl" % tag)
    writes_path = evidence / ("writes-%s.jsonl" % tag)
    home, wv2_base = state["home"], state["wv2"]
    policies = fs_policies(home, wv2_base, ctx.install_dir, ctx.env)
    roots = fs_watch_roots(policies)
    mitm = MitmProcess(scenario, ctx.ca_info["server_cert"], ctx.ca_info["server_key"], requests_path, evidence / ("ready-%s" % tag))
    poller = Poller(roots, writes_path)
    watcher = NetWatcher(ctx, tag) if ctx.ui_mode() else None
    launch: Optional[Launch] = None
    trigger: Dict[str, Any] = {"ran": False, "ok": False, "error_text": "not reached", "stage": "setup", "value": None}
    procs_before = procs_after = None
    before_snapshot: Optional[Dict[str, dict]] = None
    app_pid: Optional[int] = None
    attempts: List[dict] = []
    blocks: Dict[str, Any] = {}
    net_records: List[dict] = []
    cycles = 0
    try:
        mitm.start()
        if ctx.ui_mode():
            blocks = refresh_extra_blocks(ctx)                      # the real addresses of rsms.me as the resolver gives them now, and the WebView2 runtime's egress block
            if ctx.real_banner:
                banner_addresses(ctx, tag)
        before_snapshot = take_snapshot(roots)
        write_json(evidence / ("fs-%s-before.json" % tag), before_snapshot)
        prelaunch = snapshot_processes(shell)
        write_process_list(evidence / ("procs-%s-prelaunch.txt" % tag), prelaunch)
        poller.start()
        if watcher is not None:
            watcher.start()
        launch, attempts = acquire_page(ctx, tag, home, wv2_base, evidence, files)
        if launch is None:
            summary = "; ".join("%s: %s" % (a["strategy"], a["error"]) for a in attempts)
            trigger = {"ran": False, "ok": False, "error_text": "no strategy gave a handle on the app window (%s)" % summary[:900], "stage": "launch", "value": None}
        else:
            take_screenshot(ctx, "screenshot-%s-after-launch.png" % name, files)
            time.sleep(5)       # let the page finish its first load before the call
            procs_before = snapshot_processes(shell)
            write_process_list(evidence / ("procs-%s-before.txt" % tag), procs_before)
            app_pid = launch.app_proc.pid if launch.app_proc is not None else find_app_pid(procs_before, ctx.app_exe)
            ok, value, error = launch.page.evaluate_probe()
            case["page_probe"] = {"ok": ok, "value": value, "error": error, "path": launch.page.kind}
            poller.mark("trigger_begin")
            ok, value, error = launch.page.evaluate_trigger()
            poller.mark("trigger_end")
            trigger = trigger_outcome_from(value, error, ok)
            trigger["path"] = launch.page.kind
            if launch.page.kind == "uia":
                trigger["ui"] = {k: v for k, v in launch.page.ui.items() if k != "steps"}
                trigger["ui_steps"] = launch.page.ui.get("steps")
                files.extend(f for f in launch.page.files if f not in files)
                take_screenshot(ctx, "screenshot-%s-final.png" % name, files)
            time.sleep(settle_s)
            procs_after = snapshot_processes(shell)
            write_process_list(evidence / ("procs-%s-after.txt" % tag), procs_after)
    except Exception as error:  # noqa: BLE001 - the case ends UNPROVED with the reason, the run goes on
        trigger = {"ran": False, "ok": False, "error_text": "%s: %s" % (type(error).__name__, error), "stage": "setup", "value": None}
        shell.log("case %s crashed: %s: %s" % (tag, type(error).__name__, error))
    finally:
        if poller.thread is not None:
            cycles = poller.finish()
        after_snapshot = take_snapshot(roots)
        write_json(evidence / ("fs-%s-after.json" % tag), after_snapshot)
        if watcher is not None:
            net_records = watcher.stop()
        if launch is not None:
            close_launch(ctx, launch, wv2_base)
        else:
            kill_app(ctx, None, wv2_base)
        mitm.stop()
        copy_driver_log(ctx, tag, evidence, files)
        for source in sorted(ctx.work.glob("app-%s.raw.log" % tag)):
            mask_file(source, evidence / ("app-%s.log" % tag))
    network: Optional[Dict[str, Any]] = None
    if watcher is not None:
        api_ips = banner_addresses(ctx, tag) if ctx.real_banner else []
        rsms_rows = dns_cache_entries(ctx, [RSMS_HOST], tag + "-rsms")
        rsms = set(ctx.rsms_addresses) | {a for a in (canonical_address(str(r.get("data") or "")) for r in rsms_rows) if a}
        network = network_report_win(net_records, api_ips, ctx.live_ips, sorted(rsms), ctx.real_banner, watcher.error)
        network.update({"extra_blocks": blocks, "statement": REAL_BANNER_SCOPE if ctx.real_banner else "real-banner mode off: no request was allowed to leave the sandbox",
                        "banner_api_addresses_sources": "nslookup on the runner at run time and the system resolver's cache for the app (never hard-coded)",
                        "raw_connection_log_kept": True})
        write_masked_json(evidence / ("network-%s.json" % tag), network, ctx.live_ips)
        write_masked_json(evidence / ("netwatch-%s.jsonl.json" % tag), net_records, ctx.live_ips)
        files.extend(n for n in ("network-%s.json" % tag, "netwatch-%s.jsonl.json" % tag) if n not in files)
    trigger.setdefault("attempts", attempts)
    if ctx.ui_mode():
        trigger["note"] = ("real-banner mode: one unauthenticated read-only GET to api.github.com may reach the real Internet (not recorded by us); everything else stays on the recorder; "
                           "the bait scenario is exercised at plugin level only (the app's Install handler downloads by design when an update exists)") if ctx.real_banner else \
            "NEGATIVE CONTROL: the Install button cannot appear with the banner call blocked"
    case["trigger_outcome"] = trigger
    case["launch_attempts"] = attempts
    case.update(judge_recorded_case(scenario, trigger, requests_path, writes_path, before_snapshot, after_snapshot, policies, ctx.install_dir, procs_before, procs_after, app_pid,
                                    [ctx.install_dir, home, "network.arc.desktop", "ARC.Node_"], [], cycles, 3, evidence / ("fs-%s-diff.json" % tag),
                                    webview_markers=[wv2_base], network=network, require_network=bool(ctx.ui_mode() and ctx.real_banner),
                                    require_manifest=bool(trigger.get("path") == "uia")))
    if trigger.get("path") == "uia":
        write_masked_json(evidence / ("ui-%s.json" % tag), {"trigger": trigger.get("ui"), "steps": trigger.get("ui_steps"), "window_found": True}, ctx.live_ips)
        if "ui-%s.json" % tag not in files:
            files.append("ui-%s.json" % tag)
    for pattern in ("requests-%s.jsonl", "writes-%s.jsonl", "fs-%s-before.json", "fs-%s-after.json", "fs-%s-diff.json", "procs-%s-prelaunch.txt", "procs-%s-before.txt",
                    "procs-%s-after.txt", "app-%s.log"):
        if (evidence / (pattern % tag)).is_file() and (pattern % tag) not in files:
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
            cycles = poller.finish()          # a 0.1 s check still leaves at least two full cycles of the write log
        after = take_snapshot(roots)
        write_json(evidence / ("fs-%s-after.json" % tag), after)
        mitm.stop()
    case["trigger_outcome"] = trigger
    case["native_report"] = report
    case.update(judge_recorded_case(scenario, trigger, requests_path, writes_path, before, after, policies, ctx.install_dir, procs_before, procs_after, 0, [],
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
    if ctx.registry_override:
        shell.run(registry_override_remove_argv(), timeout=30, quiet=True)
        ctx.registry_override = False
    if ctx.screen_reader_before is not None:
        restore_screen_reader(ctx)
    if ctx.rsms_rule_created:
        shell.run(powershell_argv("Remove-NetFirewallRule -DisplayName '%s' -ErrorAction SilentlyContinue" % RSMS_RULE), timeout=60, quiet=True)
        ctx.rsms_rule_created = False
    if ctx.webview2_rule_created:
        shell.run(powershell_argv("Remove-NetFirewallRule -DisplayName '%s*' -ErrorAction SilentlyContinue" % WEBVIEW2_RULE), timeout=60, quiet=True)
        ctx.webview2_rule_created = False
    if ctx.live_block or ctx.rule_created:
        shell.run(powershell_argv(firewall_remove_script()), timeout=60)


def prepare_isolation(ctx: Context, isolation: Dict[str, Any]) -> None:
    """CA, live-network block, trust store, hosts file. Raises when any of them cannot be shown to be in place (nothing runs without it)."""
    from ca import make_ca, DEFAULT_HOSTS, copy_public  # lib/ca.py
    ctx.ca_info = make_ca(ctx.ca_dir, DEFAULT_HOSTS)
    copy_public(ctx.ca_dir, ctx.evidence)                      # ONLY ca.crt and ca.sha256
    isolation["ca_sha256"] = ctx.ca_info["ca_sha256"]
    try:
        ctx.live_ips = load_live_ips(ROOT)
    except (OSError, ValueError):
        ctx.live_ips = []
    if ctx.real_banner:
        ctx.api_addresses = resolve_real(ctx.shell, BANNER_API_HOST)       # at run time, on this runner, never hard-coded; before the mapping
    isolation["real_banner_api"] = {"enabled": ctx.real_banner, "api_addresses": list(ctx.api_addresses), "content_recorded": False,
                                    "scope": REAL_BANNER_SCOPE if ctx.real_banner else "off: every GitHub name, including api.github.com, is mapped to the recorder; the banner call cannot succeed"}
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
    used = sorted({str((c.get("trigger_outcome") or {}).get("path")) for c in cases if (c.get("trigger_outcome") or {}).get("ran")})
    tier["trigger"] = ("Windows UI Automation: Settings > Check for updates > Install (the app's own button calls the plugin check())" if used == ["uia"] else
                       "msedgedriver W3C session (browserName webview2) -> plugin:updater|check" if used == ["msedgedriver"] else
                       "WebView2 DevTools Protocol -> plugin:updater|check" if used == ["cdp"] else
                       "mixed: %s -> plugin:updater|check" % ", ".join(used) if used else "none ran (see launch_attempts in each case)")
    tier["paths_used"] = used
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
    strategies = [name for name in str(getattr(args, "strategies", ",".join(TRIGGER_STRATEGIES))).split(",") if name]
    if not strategies or any(name not in TRIGGER_STRATEGIES for name in strategies):
        print("unknown strategy in %r" % (strategies,), file=sys.stderr)
        return 2
    ctx.strategies = strategies
    ctx.real_banner = bool(getattr(args, "real_banner", True))
    ctx.webview2_block = bool(getattr(args, "webview2_block", True))
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
    if ctx.extra_block_log or ctx.rsms_addresses:
        isolation["extra_block_resolution"] = {"rsms.me": sorted(ctx.rsms_addresses), "refreshes": len(ctx.extra_block_log), "last": ctx.extra_block_log[-1] if ctx.extra_block_log else None,
                                               "why": "the hosts mapping alone is not trusted to keep the webview off rsms.me; an address block does not depend on DNS"}
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
    """Launch the installed released app (live network blocked, GitHub names on the recording server) by the trigger strategies and look at its page. NO updater call."""
    home, wv2_base = ctx.work / "probe-home", ctx.work / "probe-wv2"
    home.mkdir(parents=True, exist_ok=True)
    wv2_base.mkdir(parents=True, exist_ok=True)
    before = len(read_jsonl(mitm.log_path) or []) if mitm else 0
    files: List[str] = []
    launch, attempts = acquire_page(ctx, "probe", str(home), str(wv2_base), ctx.evidence, files)
    result: Dict[str, Any] = {"attempts": attempts, "files": files, "port_hint": port}
    try:
        if launch is not None:
            time.sleep(5)
            ok, value, error = launch.page.evaluate_probe()
            result["ipc_probe"] = {"ok": ok, "value": value, "error": error, "path": launch.page.kind}
            time.sleep(5)
        if mitm:
            records = (read_jsonl(mitm.log_path) or [])[before:]
            result["idle_launch_requests"] = {key: value for key, value in summarize_requests(records).items() if key != "present"}
    finally:
        if launch is not None:
            close_launch(ctx, launch, str(wv2_base))
        else:
            kill_app(ctx, None, str(wv2_base))
        copy_driver_log(ctx, "probe", ctx.evidence, files)
        for source in sorted(ctx.work.glob("app-probe.raw.log")):
            mask_file(source, ctx.evidence / "probe-app.log")
    if launch is None:
        raise RuntimeError("no strategy gave a handle on the app window: %s" % json.dumps(attempts, default=str)[:1200])
    return result


def cmd_probe(args: argparse.Namespace) -> int:
    evidence = Path(args.evidence)
    evidence.mkdir(parents=True, exist_ok=True)
    shell = Shell(evidence / "steps.log")
    work = Path(os.environ.get("ARCW0_WORK", r"C:\arcw0" if IS_WINDOWS else str(evidence / "work")))
    ctx = Context(evidence, work, shell, load_config(None))
    ctx.real_banner = bool(getattr(args, "real_banner", True))
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

            def uia_scripts() -> Any:
                found = check_ps_scripts(ctx)
                if found["errors"]:
                    raise RuntimeError("PowerShell parse errors: %s" % json.dumps(found["errors"])[:700])
                if not found["ran"]:
                    raise RuntimeError("the parse check did not run: %s" % found.get("detail"))
                return found
            check("uia_scripts_parse", uia_scripts)
            check("uia_desktop", lambda: ps("Add-Type -AssemblyName UIAutomationClient; Add-Type -AssemblyName UIAutomationTypes; $r = [System.Windows.Automation.AutomationElement]::RootElement; "
                                            "$k = $r.FindAll([System.Windows.Automation.TreeScope]::Children, [System.Windows.Automation.Condition]::TrueCondition); "
                                            "('desktop root={0} top_level_elements={1}' -f $r.Current.Name, $k.Count)"))

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
    probe.add_argument("--no-real-banner-api", dest="real_banner", action="store_false", default=True, help="map api.github.com to the recorder too (negative control)")
    probe.set_defaults(cargo_check=True)
    run = sub.add_parser("run", help="the isolation test")
    run.add_argument("--evidence", required=True)
    run.add_argument("--tier", choices=("released_app", "native_check", "both"), default="both")
    run.add_argument("--cases", default="clean,cached-bait")
    run.add_argument("--config", default=None)
    run.add_argument("--work", default=None)
    run.add_argument("--native-exe", default=None, help="path of the pre-built native-updater-check.exe (default: the workflow's cargo target directory)")
    run.add_argument("--strategies", default=",".join(TRIGGER_STRATEGIES), help="how to get a handle on the app window, in order: uia, msedgedriver, cdp-env, cdp-registry")
    run.add_argument("--no-real-banner-api", dest="real_banner", action="store_false", default=True,
                     help="map api.github.com to the recorder too: the banner call cannot succeed, the Install button never appears (a labelled negative control)")
    run.add_argument("--no-webview2-block", dest="webview2_block", action="store_false", default=True,
                     help="do not block msedgewebview2.exe's outbound traffic by program (it is then only observed); use when the block disturbs the page")
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
