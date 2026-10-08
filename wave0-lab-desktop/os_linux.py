#!/usr/bin/env python3
"""Linux job of the Wave 0 desktop updater-isolation lab (THROWAWAY LAB FILE, never merged).

Question (ARC-83 audit item 4): with v0.7.12 as GitHub "Latest" (five launchers + SHA256SUMS, NO latest.json, v0.8.11 present as
bait), does the RELEASED v0.7.11 ARC desktop app's native updater path request only the manifest URL, download no bundle /
installer / .sig / .tar.gz of any release, install nothing, launch no new app version and write no new files outside expected
app/log state? The exact manifest-404 error text is recorded.

This script runs ON a GitHub-hosted ubuntu-24.04 runner (sudo inside the runner is fine; it is never run on a developer Mac).

  os_linux.py probe --evidence DIR
      Capability probe (never fails the job): deps, release asset digest, CA trust, hosts, port 443, WebKitWebDriver session,
      one released-app trigger, `cargo check` of the native crate. Writes DIR/probe.json and DIR/steps.log.
  os_linux.py run --evidence DIR [--tier released_app|native_check|both] [--cases clean,cached-bait]
      The full lab. Writes DIR/result.json (schema arc.legacy-bridge.wave0-lab.desktop-os-result.v1) plus the evidence files
      listed below. Always writes a result.json (verdict UNPROVED with the reason on any failure) and exits 0 when evidence
      exists, so the summary job (stage_c_summary.py) is what judges.

Two tiers, both fail closed:
  released_app   (primary) the released v0.7.11 .deb is downloaded, its sha256 is checked against the release digest BEFORE use,
                 it is extracted into a sandbox (never installed), and run under Xvfb through WebKitWebDriver. The trigger is
                 window.__TAURI_INTERNALS__.invoke('plugin:updater|check') (a programmatic trigger; the Settings "Install"
                 button runs the same plugin check() through the JS wrapper).
  native_check   (supporting) wave0-lab-desktop/native-updater-check, the REAL tauri-plugin-updater 2.10.1 check() built from a
                 reviewed lockfile, run against the same server and CA.

What the interception covers (and says so in result.json): a per-run CA is trusted by the runner, github.com and friends are mapped
to 127.0.0.1, and lib/mitm_server.py answers on 443 and records every request. That covers the tauri-plugin-updater
check / download / install path, which verifies TLS through the operating system trust store. The app's own banner
(check_for_update) and ensure_binary use reqwest with bundled webpki roots: they are NOT interceptable, their handshakes are
rejected by the client and show up as tls_failure rows, and they are covered by the cited sources and the Stage A replica.
Live isolation: every ARC address in .github/workflows/legacy-bridge.yml LIVE_NETWORK_IPS is REJECTed by iptables before any
case, the iptables counters are recorded before and after each case, and the hosts mapping exists only while a case runs.

Evidence files in DIR (never a private key; the CA private key and the server key stay under the runner's work dir):
  result.json probe.json steps.log isolation.json provenance.json provenance-source.json ca.crt ca.sha256 manifest404-error.txt
  requests-<case>.jsonl writes-<case>.jsonl fs-<case>-before.json fs-<case>-after.json procs-<case>-before.txt
  procs-<case>-after.txt outside-writes-<case>.txt trigger-<case>.json app-<case>.log      (released_app; native_check cases use
  the prefix native-<case>), harness/ (the Python replay harness, supporting evidence), selftest-requests.jsonl, build.log.

UNVERIFIED ON CI (nothing below could be exercised on the Mac that wrote this; the first probe run answers them):
  * WebKitWebDriver launches the released wry app from webkitgtk:browserOptions.binary and accepts browserName "wry" (the same
    capabilities tauri-driver sends); the fallback attempt omits browserName. xvfb-run wraps dbus-run-session (when present) which
    wraps the driver; the app inherits that environment.
  * The released binary honours TAURI_WEBVIEW_AUTOMATION=true, and window.__TAURI_INTERNALS__.invoke('plugin:updater|check')
    is allowed from the main window by the updater:default capability inside an automation session.
  * On Linux the plugin (updater.rs:393-401) only defaults SSL_CERT_FILE / SSL_CERT_DIR; its verifier honours them, so the CA
    installed with update-ca-certificates plus SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt makes the TLS handshake succeed.
  * The tray (libayatana-appindicator3 is dlopened, not linked) initialises under Xvfb; tray::install(...)? aborts setup if not.
  * WebKitGTK needs its sandbox disabled on ubuntu-24.04 (AppArmor restricts unprivileged user namespaces): the script sets
    kernel.apparmor_restrict_unprivileged_userns=0 and WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1; neither touches the updater path.
  * apt resolves libgtk-3-0 (provided by libgtk-3-0t64) and libayatana-appindicator3-1 on ubuntu-24.04 images.
  * lib/fswatch.py (read, aligned): Poller scans every 0.5 s, so a file that lives shorter is caught only by the before/after snapshot
    diff; the cost of scanning /tmp, the sandbox and the extracted app on a hosted runner is assumed small.
  * GH_TOKEN (or GITHUB_TOKEN) is exported to the job (the workflow does); without it the release call is anonymous and may be rate
    limited, and the digest recorded in config.json is used instead, labelled as such in result.setup.release_metadata.
  * The native crate compiles (it has never been compiled). The workflow builds it in a step before this script; this script uses
    native-updater-check/target/release/native-updater-check when it exists and otherwise builds into its own work dir.
  * The mock-runtime plugin reads SSL_CERT_FILE exactly as the app's plugin does, and the live-address block (lib/live_block.py: iptables
    REJECT plus hosts blackholes, whose verify script curls one live address and expects it to fail) behaves as in Stage A.
  * Every live address is masked (<live-ip-N>) in steps.log, the app logs and result.json: the evidence never names a validator address.
"""
from __future__ import annotations

import argparse
import collections
import contextlib
import datetime
import hashlib
import http.client
import importlib
import json
import os
import platform
import re
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Callable, Dict, Iterable, List, Optional, Sequence, Tuple

HERE = Path(__file__).resolve().parent
LIB_DIR = HERE / "lib"
REPO_ROOT = HERE.parent

SCHEMA_RESULT = "arc.legacy-bridge.wave0-lab.desktop-os-result.v1"
SCHEMA_PROBE = "arc.legacy-bridge.wave0-lab.desktop-os-probe.v1"
OS_LABEL = "linux"
REPO = "FerrumVir/arc-chain"
TAG = "v0.7.11"
APP_VERSION = "0.7.11"
BAIT_VERSION = "0.8.11"
PLUGIN_VERSION = "2.10.1"
RELEASE_NOT_FOUND = "Could not fetch a valid release JSON from the remote"  # tauri-plugin-updater 2.10.1 src/error.rs:25
MANIFEST_HOST = "github.com"
MANIFEST_PATH = "/%s/releases/latest/download/latest.json" % REPO
REDIRECT_PATH = "/%s/releases/download/v0.7.12/latest.json" % REPO
API_LATEST_PATH = "/repos/%s/releases/latest" % REPO
MANIFEST_URL = "https://%s%s" % (MANIFEST_HOST, MANIFEST_PATH)
UPDATER_PUBKEY = (
    "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDlBOTcwQ0FBQ0U1NjQ3M0IKUldRN1IxYk9xZ3lYbWcrbkhkWnZlc0tmWW1uTTlhcDljLzF4cUZtUUVibTRNa2V4TjBoNHJqY2EK"
)
MITM_HOSTS = ("github.com", "api.github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com")
HOSTS_MARK = "# wave0-lab-desktop-github"   # the GitHub -> recording-server lines; lib/live_block.py tags its blackhole lines "# wave0-lab-desktop"
BLACKHOLE_MARK = "# wave0-lab-desktop"
GUARD_ENV = "WAVE0_DESKTOP_ALLOW_LOCAL"
CA_FILE_NAME = "wave0-lab-desktop-ca.crt"
SYSTEM_BUNDLE = "/etc/ssl/certs/ca-certificates.crt"
SYSTEM_CERT_DIR = "/etc/ssl/certs"
WEBDRIVER_HOST = "127.0.0.1"
WEBDRIVER_PORT = 4444
CASES = ("clean", "cached-bait")
CASE_SCENARIO = {"clean": "latest-404", "cached-bait": "bait-0.8.11"}
TIERS = ("released_app", "native_check")
CRITERIA = ("only_manifest_url", "no_bundle_download", "no_install", "no_new_app_launch", "no_new_files")
PAYLOAD_SUFFIXES = (".sig", ".tar.gz", ".tgz", ".gz", ".zip", ".exe", ".msi", ".dmg", ".deb", ".rpm", ".appimage", ".pkg")
RELEASE_ASSET_RE = re.compile(r"^/%s/releases/download/[^/]+/[^/]+$" % re.escape(REPO))
UPDATE_ARTIFACT_RE = re.compile(
    r"(latest\.json|\.sig|\.tar\.gz|\.tgz|\.appimage|\.deb|\.rpm|\.dmg|\.msi|\.exe|\.partial|\.download|\.crdownload|\.pkg)$", re.IGNORECASE
)
INSTALLER_EXES = frozenset({"dpkg", "dpkg-deb", "apt", "apt-get", "aptitude", "pkexec", "rpm", "dnf", "yum", "snap", "flatpak", "gdebi"})
APP_BINARY_NAMES = frozenset({"arc-desktop", "arc-node-desktop"})
LIVE_WORKFLOW = ".github/workflows/legacy-bridge.yml"
LIVE_ACCEPTANCE = "tests/legacy-bridge/headless-v07-acceptance.sh"
APT_ENV = {"DEBIAN_FRONTEND": "noninteractive", "NEEDRESTART_MODE": "a", "NEEDRESTART_SUSPEND": "1"}
APT_BASE = ["sudo", "-n", "-E", "apt-get", "-o", "DPkg::Lock::Timeout=300", "-o", "Acquire::Retries=3", "-o", "Dpkg::Progress-Fancy=0"]
RUNTIME_PACKAGES = (
    "libwebkit2gtk-4.1-0", "libgtk-3-0", "libayatana-appindicator3-1", "libsoup-3.0-0", "xvfb", "xauth", "webkit2gtk-driver",
    "inotify-tools", "openssl", "ca-certificates", "dbus-x11", "fonts-dejavu-core", "curl", "binutils",
)
BUILD_PACKAGES = ("libwebkit2gtk-4.1-dev", "pkg-config", "build-essential")
EXPECTED_STATE_NOTE = (
    "Expected app/log state under the sandbox HOME: .config (GTK, dconf, the autostart plugin's .desktop entry), .local/share "
    "(Tauri app data dir network.arc.desktop, WebKit storage), .local/state, .cache (WebKit and font caches), .arc/logs, the "
    "sandbox runtime and tmp dirs, and the X server's /tmp/.X* files. Anything else, anything under the extracted app, any "
    "bundle / installer / .sig / .tar.gz / latest.json cache anywhere, is unexpected."
)
INTERCEPTION_COVERAGE = (
    "The interception (per-run CA in the OS trust store, github.com mapped to 127.0.0.1, a recording server on 443) covers the "
    "tauri-plugin-updater check / download / install path, which verifies TLS through the operating system trust store. The "
    "app's banner (check_for_update) and ensure_binary use reqwest 0.12.28 with bundled webpki roots: they are NOT interceptable, "
    "their handshakes to the local server are rejected by the client (tls_failure rows) and the paths they intended are not "
    "visible here. They are covered by the cited v0.7.11 sources and the Stage A replica."
)

# ----------------------------------------------------------------------------------------------------------------------
# JavaScript run inside the app's webview by the WebDriver trigger
# ----------------------------------------------------------------------------------------------------------------------

READY_SCRIPT = "return typeof window.__TAURI_INTERNALS__ !== 'undefined' && typeof window.__TAURI_INTERNALS__.invoke === 'function';"

CHECK_SCRIPT = """
var done = arguments[arguments.length - 1];
try {
  window.__TAURI_INTERNALS__.invoke('plugin:updater|check', {}).then(function (value) {
    done({ok: true, value: (value === undefined ? null : value)});
  }, function (error) {
    var text;
    try { text = (typeof error === 'string') ? error : JSON.stringify(error); } catch (e) { text = String(error); }
    done({ok: false, error: text, error_string: String(error), error_type: typeof error});
  });
} catch (thrown) {
  done({ok: false, thrown: String(thrown)});
}
"""

# POSITIVE CONTROL ONLY (never part of a case): ask the plugin to download the bait update so the request log can be shown to
# see a bundle request. A Tauri Channel crosses the IPC as the string "__CHANNEL__:<callback id>".
DOWNLOAD_CONTROL_SCRIPT = """
var rid = arguments[0];
var done = arguments[arguments.length - 1];
try {
  var internals = window.__TAURI_INTERNALS__;
  var callbackId = internals.transformCallback(function () {});
  internals.invoke('plugin:updater|download', {rid: rid, onEvent: '__CHANNEL__:' + callbackId}).then(function (value) {
    done({ok: true, value: (value === undefined ? null : value)});
  }, function (error) {
    var text;
    try { text = (typeof error === 'string') ? error : JSON.stringify(error); } catch (e) { text = String(error); }
    done({ok: false, error: text, error_string: String(error)});
  });
} catch (thrown) {
  done({ok: false, thrown: String(thrown)});
}
"""


# ----------------------------------------------------------------------------------------------------------------------
# small pure helpers
# ----------------------------------------------------------------------------------------------------------------------

def now_utc() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(str(path), "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_sha256_digest(value: Any) -> Optional[str]:
    """'sha256:<64 hex>' or bare '<64 hex>' -> lowercase hex, else None."""
    if not isinstance(value, str):
        return None
    text = value.strip().lower()
    if text.startswith("sha256:"):
        text = text[len("sha256:"):]
    return text if re.fullmatch(r"[0-9a-f]{64}", text) else None


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def read_jsonl(path: Path) -> Tuple[Optional[List[dict]], int]:
    """(rows, bad_line_count); rows is None when the file does not exist or cannot be read at all."""
    try:
        text = Path(path).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None, 0
    rows: List[dict] = []
    bad = 0
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            item = json.loads(line)
        except ValueError:
            bad += 1
            continue
        if isinstance(item, dict):
            rows.append(item)
        else:
            bad += 1
    return rows, bad


def tail_text(text: str, limit: int = 1200) -> str:
    return text if len(text) <= limit else "...[%d earlier chars]\n%s" % (len(text) - limit, text[-limit:])


def mask_ips(text: str, ips: Sequence[str]) -> str:
    """Replace every ARC live address with <live-ip-N>: nothing bound for review carries the validators' addresses."""
    for index, ip in enumerate(ips, 1):
        text = re.sub(r"(?<![\d.])%s(?![\d])" % re.escape(ip), "<live-ip-%d>" % index, text)
    return text


def case_tag(tier: str, case: str) -> str:
    """File-name tag of a case: plain for the primary tier, native- prefixed for the supporting one."""
    return case if tier == "released_app" else "native-" + case


# ----------------------------------------------------------------------------------------------------------------------
# release asset selection and verification
# ----------------------------------------------------------------------------------------------------------------------

class AssetError(RuntimeError):
    pass


def select_linux_asset(release: dict, tag: str = TAG) -> dict:
    """The Linux desktop asset of the release JSON: the amd64 .deb (preferred) or the amd64 AppImage, with its release digest."""
    if not isinstance(release, dict) or release.get("tag_name") != tag:
        raise AssetError("release JSON is not for %s (tag_name %r)" % (tag, release.get("tag_name") if isinstance(release, dict) else None))
    if release.get("draft"):
        raise AssetError("the release is still a draft")
    assets = release.get("assets")
    if not isinstance(assets, list):
        raise AssetError("release JSON has no assets list")
    named = {}
    for asset in assets:
        if isinstance(asset, dict) and isinstance(asset.get("name"), str):
            named[asset["name"]] = asset
    picks = [name for name in sorted(named) if name.endswith("_amd64.deb")] + [name for name in sorted(named) if name.endswith("_amd64.AppImage")]
    if not picks:
        raise AssetError("no *_amd64.deb or *_amd64.AppImage among the assets: %s" % sorted(named))
    name = picks[0]
    asset = named[name]
    digest = parse_sha256_digest(asset.get("digest"))
    if digest is None:
        raise AssetError("asset %s has no usable sha256 digest (%r)" % (name, asset.get("digest")))
    size = asset.get("size")
    if not isinstance(size, int) or isinstance(size, bool) or size <= 0:
        raise AssetError("asset %s has no usable size (%r)" % (name, size))
    url = "https://github.com/%s/releases/download/%s/%s" % (REPO, tag, name)
    declared = asset.get("browser_download_url")
    if isinstance(declared, str) and declared != url:
        raise AssetError("asset %s declares the download URL %s, expected %s" % (name, declared, url))
    return {"name": name, "size": size, "sha256": digest, "release_digest": "sha256:" + digest, "kind": "deb" if name.endswith(".deb") else "appimage", "url": url}


def fetch_release_json(token: Optional[str] = None, opener: Callable[..., Any] = urllib.request.urlopen, timeout: float = 30.0) -> dict:
    """GET /repos/FerrumVir/arc-chain/releases/tags/v0.7.11 (read only)."""
    headers = {"Accept": "application/vnd.github+json", "User-Agent": "arc-wave0-lab-desktop-linux/1 (read-only)", "X-GitHub-Api-Version": "2022-11-28"}
    if token:
        headers["Authorization"] = "Bearer " + token
    request = urllib.request.Request("https://api.github.com/repos/%s/releases/tags/%s" % (REPO, TAG), headers=headers)
    with opener(request, timeout=timeout) as response:
        return json.loads(response.read(8 << 20).decode("utf-8"))


def config_asset(config: Any) -> Optional[dict]:
    """The Linux asset recorded in wave0-lab-desktop/config.json: app.assets.linux_deb (preferred) or linux_appimage, each
    {name, size, sha256 | release_digest}. Tolerant of a few alternative key names."""
    if not isinstance(config, dict):
        return None
    app = config.get("app")
    assets = app.get("assets") if isinstance(app, dict) else None
    if not isinstance(assets, dict):
        return None
    for key in ("linux_deb", "linux", "linux_x86_64", "linux-x86_64", "linux_appimage"):
        item = assets.get(key)
        if not (isinstance(item, dict) and isinstance(item.get("name"), str) and item["name"].endswith(("_amd64.deb", "_amd64.AppImage"))):
            continue
        digest = parse_sha256_digest(item.get("sha256") or item.get("release_digest") or item.get("digest"))
        if digest:
            return {"name": item["name"], "size": item.get("size"), "sha256": digest, "release_digest": "sha256:" + digest,
                    "kind": "deb" if item["name"].endswith(".deb") else "appimage",
                    "url": "https://github.com/%s/releases/download/%s/%s" % (REPO, TAG, item["name"])}
    return None


def verify_download(path: Path, asset: dict) -> dict:
    """sha256 and size of the downloaded file against the release digest (checked BEFORE the file is used)."""
    actual = sha256_file(path)
    size = path.stat().st_size
    return {
        "asset": asset["name"], "asset_sha256": actual, "release_digest": asset["release_digest"], "size": size,
        "size_match": asset.get("size") in (None, size), "digest_match": actual == asset["sha256"],
    }


def find_app_binary(root: Path) -> Optional[Path]:
    for relative in ("usr/bin/arc-desktop", "squashfs-root/usr/bin/arc-desktop"):
        candidate = Path(root) / relative
        if candidate.is_file():
            return candidate
    return None


def binary_crate_versions(data: bytes, names: Sequence[str] = ("tauri-plugin-updater", "tauri", "tauri-utils", "wry", "tao", "reqwest", "minisign-verify", "rustls-platform-verifier")) -> Dict[str, List[str]]:
    """Versions of crates whose registry paths are embedded in the strings of a released binary (name-X.Y.Z)."""
    found: Dict[str, List[str]] = {}
    for name in names:
        pattern = re.compile(re.escape(name.encode("ascii")) + rb"-(\d+\.\d+\.\d+)(?![\w.-]*\.\d)")
        versions = sorted({match.group(1).decode("ascii") for match in pattern.finditer(data)})
        if versions:
            found[name] = versions
    return found


def parse_ldd_missing(text: str) -> List[str]:
    return sorted({line.split("=>")[0].strip() for line in text.splitlines() if "not found" in line})


# ----------------------------------------------------------------------------------------------------------------------
# command builders
# ----------------------------------------------------------------------------------------------------------------------

def apt_env() -> Dict[str, str]:
    return dict(os.environ, **APT_ENV)


def apt_commands(packages: Sequence[str], update: bool = True) -> List[List[str]]:
    commands = []
    if update:
        commands.append(APT_BASE + ["update", "-q"])
    commands.append(APT_BASE + ["install", "-y", "-q", "--no-install-recommends"] + list(packages))
    return commands


def hosts_lines(hostnames: Iterable[str]) -> List[str]:
    """IPv4 and IPv6 loopback lines (a resolver must not fall back to a real AAAA record of the name)."""
    lines: List[str] = []
    for name in hostnames:
        lines.append("127.0.0.1 %s %s" % (name, HOSTS_MARK))
        lines.append("::1 %s %s" % (name, HOSTS_MARK))
    return lines


def iptables_block_commands(ips: Sequence[str]) -> List[List[str]]:
    return [["sudo", "-n", "iptables", "-I", "OUTPUT", "-d", ip, "-j", "REJECT"] for ip in ips]


def iptables_unblock_commands(ips: Sequence[str]) -> List[List[str]]:
    return [["sudo", "-n", "iptables", "-D", "OUTPUT", "-d", ip, "-j", "REJECT"] for ip in ips]


def parse_iptables_rules(text: str, ips: Sequence[str]) -> Dict[str, bool]:
    """Which live addresses have a REJECT rule in `iptables -S OUTPUT` output."""
    present = {}
    for ip in ips:
        pattern = re.compile(r"^-A OUTPUT\b.*-d %s(?:/32)?\b.*-j REJECT\b" % re.escape(ip), re.MULTILINE)
        present[ip] = bool(pattern.search(text))
    return present


def parse_iptables_counters(text: str, ips: Sequence[str]) -> Dict[str, int]:
    """Packets REJECTed per live address from `iptables -nvxL OUTPUT` (column 1 is the packet count)."""
    counters: Dict[str, int] = {}
    for line in text.splitlines():
        fields = line.split()
        if len(fields) < 9 or not fields[0].isdigit():
            continue
        if "REJECT" not in fields:
            continue
        for ip in ips:
            if ip in fields:
                counters[ip] = counters.get(ip, 0) + int(fields[0])
    return {ip: counters.get(ip, 0) for ip in ips}


def load_live_ips(root: Path) -> List[str]:
    """The ARC live-network addresses the repository's own CI isolates: LIVE_NETWORK_IPS of legacy-bridge.yml, cross-checked
    against the live_ips array of the acceptance script (the same rule as the soak lab's live_ips.py)."""
    workflow = (Path(root) / LIVE_WORKFLOW).read_text(encoding="utf-8")
    found = re.findall(r"^\s*LIVE_NETWORK_IPS:\s*(.+?)\s*$", workflow, flags=re.MULTILINE)
    if len(found) != 1:
        raise ValueError("expected exactly one LIVE_NETWORK_IPS line in %s, found %d" % (LIVE_WORKFLOW, len(found)))
    from_workflow = found[0].split()
    harness = (Path(root) / LIVE_ACCEPTANCE).read_text(encoding="utf-8")
    arrays = re.findall(r"^live_ips=\((.*?)\)\s*$", harness, flags=re.MULTILINE)
    if len(arrays) != 1:
        raise ValueError("expected exactly one live_ips array in %s, found %d" % (LIVE_ACCEPTANCE, len(arrays)))
    if arrays[0].split() != from_workflow:
        raise ValueError("LIVE_NETWORK_IPS differs from the acceptance script's live_ips")
    ipv4 = re.compile(r"^(?:(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])$")
    if not from_workflow or len(set(from_workflow)) != len(from_workflow) or not all(ipv4.match(ip) for ip in from_workflow):
        raise ValueError("the live address list is empty, has duplicates or holds a non-IPv4 entry")
    return from_workflow


def sandbox_env(sandbox: Path, extra: Optional[Dict[str, str]] = None) -> Dict[str, str]:
    """Environment of the app / checker process: everything it writes lands under the sandbox."""
    home = sandbox / "home"
    env = dict(os.environ)
    for key in ("GH_TOKEN", "GITHUB_TOKEN", "ACTIONS_RUNTIME_TOKEN", "ACTIONS_ID_TOKEN_REQUEST_TOKEN"):
        env.pop(key, None)
    env.update({
        "HOME": str(home),
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_DATA_HOME": str(home / ".local" / "share"),
        "XDG_CACHE_HOME": str(home / ".cache"),
        "XDG_STATE_HOME": str(home / ".local" / "state"),
        "XDG_RUNTIME_DIR": str(sandbox / "runtime"),
        "TMPDIR": str(sandbox / "tmp"),
        "SSL_CERT_FILE": SYSTEM_BUNDLE,
        "SSL_CERT_DIR": SYSTEM_CERT_DIR,
        "TAURI_WEBVIEW_AUTOMATION": "true",
        "WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS": "1",
        "WEBKIT_DISABLE_COMPOSITING_MODE": "1",
        "WEBKIT_DISABLE_DMABUF_RENDERER": "1",
        "GDK_BACKEND": "x11",
        "LIBGL_ALWAYS_SOFTWARE": "1",
        "NO_AT_BRIDGE": "1",
        "RUST_LOG": "info,arc_desktop_lib=debug,tauri_plugin_updater=debug,reqwest=debug,hyper_util=debug",
        "RUST_BACKTRACE": "0",
    })
    if extra:
        env.update(extra)
    return env


def expected_prefixes(sandbox: Path) -> List[str]:
    home = sandbox / "home"
    return [
        str(home / ".config"), str(home / ".local" / "share"), str(home / ".local" / "state"), str(home / ".cache"),
        str(home / ".arc" / "logs"), str(sandbox / "runtime"), str(sandbox / "tmp"),
    ]


TRANSIENT_PREFIXES = ("/tmp/.X", "/tmp/.ICE-unix", "/tmp/xvfb-run.", "/tmp/dbus-", "/tmp/.dbus")   # X server files and the session bus socket (dbus-run-session)


# ----------------------------------------------------------------------------------------------------------------------
# W3C WebDriver client (stdlib only)
# ----------------------------------------------------------------------------------------------------------------------

class WebDriverError(RuntimeError):
    def __init__(self, status: int, error: str, message: str, raw: Any = None):
        super().__init__("webdriver %s (HTTP %s): %s" % (error, status, message))
        self.status, self.error, self.message, self.raw = status, error, message, raw


class WebDriverClient:
    """The few W3C WebDriver commands the trigger needs, over http.client. One connection per request."""

    def __init__(self, host: str = WEBDRIVER_HOST, port: int = WEBDRIVER_PORT, timeout: float = 60.0):
        self.host, self.port, self.timeout = host, port, timeout
        self.session_id: Optional[str] = None

    def request(self, method: str, path: str, body: Optional[dict] = None, timeout: Optional[float] = None) -> Any:
        connection = http.client.HTTPConnection(self.host, self.port, timeout=timeout or self.timeout)
        try:
            payload = json.dumps(body if body is not None else {}).encode("utf-8") if method == "POST" else None
            headers = {"Content-Type": "application/json; charset=utf-8", "Accept": "application/json"}
            connection.request(method, path, body=payload, headers=headers)
            response = connection.getresponse()
            raw = response.read().decode("utf-8", "replace")
            status = response.status
        finally:
            connection.close()
        try:
            document = json.loads(raw) if raw.strip() else {}
        except ValueError:
            raise WebDriverError(status, "invalid response", "not JSON: %s" % raw[:200], raw)
        value = document.get("value") if isinstance(document, dict) else None
        if status >= 400:  # W3C errors are HTTP 4xx/5xx; a 200 whose value has an "error" key is just the script's own result
            error = value.get("error") if isinstance(value, dict) else "unknown error"
            message = value.get("message") if isinstance(value, dict) else raw[:200]
            raise WebDriverError(status, str(error), str(message), document)
        return value

    def status(self) -> Any:
        return self.request("GET", "/status", timeout=5)

    def wait_ready(self, total_s: float = 30.0, interval: float = 0.5) -> bool:
        end = time.monotonic() + total_s
        while time.monotonic() < end:
            try:
                self.status()
                return True
            except (OSError, WebDriverError, http.client.HTTPException):
                time.sleep(interval)
        return False

    def new_session(self, capabilities: dict, timeout: float = 180.0) -> str:
        value = self.request("POST", "/session", capabilities, timeout=timeout)
        if not isinstance(value, dict) or not isinstance(value.get("sessionId"), str):
            raise WebDriverError(200, "invalid session response", "no sessionId in %r" % (value,), value)
        self.session_id = value["sessionId"]
        return self.session_id

    def _session_path(self, suffix: str) -> str:
        if not self.session_id:
            raise WebDriverError(0, "no session", "new_session() has not succeeded")
        return "/session/%s%s" % (self.session_id, suffix)

    def set_timeouts(self, script_ms: int = 180000, page_load_ms: int = 60000, implicit_ms: int = 0) -> Any:
        return self.request("POST", self._session_path("/timeouts"), {"script": script_ms, "pageLoad": page_load_ms, "implicit": implicit_ms})

    def execute_sync(self, script: str, args: Optional[list] = None, timeout: float = 60.0) -> Any:
        return self.request("POST", self._session_path("/execute/sync"), {"script": script, "args": args or []}, timeout=timeout)

    def execute_async(self, script: str, args: Optional[list] = None, timeout: float = 180.0) -> Any:
        return self.request("POST", self._session_path("/execute/async"), {"script": script, "args": args or []}, timeout=timeout)

    def delete_session(self) -> None:
        if self.session_id:
            try:
                self.request("DELETE", "/session/%s" % self.session_id, timeout=30)
            finally:
                self.session_id = None


def capability_attempts(binary: str) -> List[dict]:
    """The capabilities tauri-driver sends to WebKitWebDriver (browserName wry + webkitgtk:browserOptions), then the same without
    browserName in case the driver refuses to match the name."""
    options = {"binary": binary, "args": []}
    return [
        {"capabilities": {"alwaysMatch": {"browserName": "wry", "webkitgtk:browserOptions": options}}},
        {"capabilities": {"alwaysMatch": {"webkitgtk:browserOptions": dict(options)}}},
    ]


def open_session(client: WebDriverClient, binary: str) -> Tuple[str, List[str]]:
    """New session with the first capability set the driver accepts. Returns (session id, notes about refused attempts)."""
    notes: List[str] = []
    last: Optional[Exception] = None
    for attempt, capabilities in enumerate(capability_attempts(binary)):
        try:
            return client.new_session(capabilities), notes
        except (WebDriverError, OSError, http.client.HTTPException) as error:
            last = error
            notes.append("attempt %d (%s): %s" % (attempt + 1, "with browserName" if attempt == 0 else "without browserName", error))
    raise WebDriverError(0, "session not created", "; ".join(notes), str(last))


# ----------------------------------------------------------------------------------------------------------------------
# trigger outcomes
# ----------------------------------------------------------------------------------------------------------------------

def normalize_webdriver_trigger(raw: Any) -> dict:
    """The value the CHECK_SCRIPT returned -> {outcome, text, version, current_version}. Outcomes: release_not_found, error,
    update_available, no_update, transport_failure (the script itself did not run)."""
    if not isinstance(raw, dict) or "ok" not in raw:
        return {"outcome": "transport_failure", "text": "unexpected script result: %r" % (raw,), "version": None, "current_version": None}
    if raw.get("ok") is True:
        value = raw.get("value")
        if value is None:
            return {"outcome": "no_update", "text": "check() resolved null (no update)", "version": None, "current_version": None}
        if isinstance(value, dict):
            return {"outcome": "update_available", "text": "update %s available (current %s)" % (value.get("version"), value.get("currentVersion")),
                    "version": value.get("version"), "current_version": value.get("currentVersion"), "rid": value.get("rid")}
        return {"outcome": "transport_failure", "text": "unexpected check() value: %r" % (value,), "version": None, "current_version": None}
    text = raw.get("error")
    if text is None:
        text = raw.get("thrown")
    text = text if isinstance(text, str) else repr(text)
    outcome = "release_not_found" if text.strip() == RELEASE_NOT_FOUND else "error"
    return {"outcome": outcome, "text": text, "version": None, "current_version": None}


def normalize_native_trigger(line: Any) -> dict:
    """The one JSON line native-updater-check printed -> the same shape as normalize_webdriver_trigger."""
    if not isinstance(line, dict) or line.get("schema") != "arc.legacy-bridge.wave0-lab.native-updater-check.v1":
        return {"outcome": "transport_failure", "text": "not a native-updater-check report: %r" % (line,), "version": None, "current_version": None}
    outcome = line.get("outcome")
    if outcome == "update_available" and isinstance(line.get("update"), dict):
        return {"outcome": "update_available", "text": "update %s available (current %s)" % (line["update"].get("version"), line.get("current_version")),
                "version": line["update"].get("version"), "current_version": line.get("current_version"),
                "download_attempted": line.get("download_attempted"), "download_result": line.get("download_result")}
    if outcome == "no_update":
        return {"outcome": "no_update", "text": "check() found no update", "version": None, "current_version": line.get("current_version")}
    if outcome == "error":
        text = line.get("error") if isinstance(line.get("error"), str) else repr(line.get("error"))
        return {"outcome": "release_not_found" if text.strip() == RELEASE_NOT_FOUND else "error", "text": text, "version": None,
                "current_version": line.get("current_version"), "error_kind": line.get("error_kind")}
    return {"outcome": "transport_failure", "text": "unknown outcome %r" % (outcome,), "version": None, "current_version": None}


def trigger_definitive(scenario: str, trigger: Optional[dict]) -> Tuple[bool, str]:
    """Did the trigger do what the case is meant to test? latest-404: check() must fail with ReleaseNotFound (the manifest answered
    404). bait-0.8.11: check() must find the applicable 0.8.11 update. Anything else proves nothing about the criteria."""
    if not trigger:
        return False, "no trigger outcome was recorded"
    outcome = trigger.get("outcome")
    if scenario in ("latest-404", "latest-404-direct", "api-latest"):
        if outcome == "release_not_found":
            return True, "check() failed with the ReleaseNotFound text"
        return False, "latest-404 expected check() to fail with ReleaseNotFound, got %s: %s" % (outcome, tail_text(str(trigger.get("text")), 300))
    if scenario == "bait-0.8.11":
        if outcome == "update_available" and trigger.get("version") == BAIT_VERSION:
            return True, "check() found the applicable %s update" % BAIT_VERSION
        return False, "bait expected check() to find update %s, got %s: %s" % (BAIT_VERSION, outcome, tail_text(str(trigger.get("text")), 300))
    return False, "unknown scenario %r" % scenario


# ----------------------------------------------------------------------------------------------------------------------
# request log analysis
# ----------------------------------------------------------------------------------------------------------------------

def is_payload_row(row: dict) -> bool:
    return row.get("payload") is True or str(row.get("path") or "").lower().endswith(PAYLOAD_SUFFIXES)


def is_manifest_row(row: dict) -> bool:
    path = row.get("path")
    return row.get("role") in ("manifest", "manifest_redirect") or path in (MANIFEST_PATH, REDIRECT_PATH)


def is_release_asset_row(row: dict) -> bool:
    path = str(row.get("path") or "")
    return bool(RELEASE_ASSET_RE.match(path)) and not path.lower().endswith("/latest.json")


def summarize_requests(rows: Sequence[dict]) -> dict:
    """Counts of the recorded rows. `total` counts served HTTP requests only; refused TLS handshakes are listed apart."""
    requests = [row for row in rows if row.get("kind") == "request"]
    failures = [row for row in rows if row.get("kind") == "tls_failure"]
    counter = collections.Counter((str(row.get("host") or row.get("sni") or ""), str(row.get("path") or "")) for row in requests)
    by_host_path = [[host, path, count] for (host, path), count in sorted(counter.items())]
    tls = collections.Counter(str(row.get("sni") or row.get("host") or "(no SNI)") for row in failures)
    payload = sorted({"%s%s" % (row.get("host"), row.get("path")) for row in requests if is_payload_row(row)})
    assets = sorted({"%s%s" % (row.get("host"), row.get("path")) for row in requests if is_release_asset_row(row)})
    non_manifest = sorted({"%s%s" % (row.get("host"), row.get("path")) for row in requests if not is_manifest_row(row)})
    return {
        "total": len(requests), "by_host_path": by_host_path, "tls_failures": [[name, count] for name, count in sorted(tls.items())],
        "manifest": sum(1 for row in requests if is_manifest_row(row)), "payload": payload, "release_assets": assets, "non_manifest": non_manifest,
        "statuses": dict(collections.Counter(str(row.get("status")) for row in requests)),
    }


# ----------------------------------------------------------------------------------------------------------------------
# processes
# ----------------------------------------------------------------------------------------------------------------------

def read_process_table(proc_root: str = "/proc") -> List[dict]:
    """One row per process: pid, ppid, uid, exe (None when unreadable), cmd."""
    table: List[dict] = []
    try:
        entries = os.listdir(proc_root)
    except OSError:
        return table
    for entry in entries:
        if not entry.isdigit():
            continue
        base = os.path.join(proc_root, entry)
        try:
            exe = os.readlink(os.path.join(base, "exe"))
        except OSError:
            exe = None
        try:
            with open(os.path.join(base, "cmdline"), "rb") as handle:
                cmd = handle.read(4096).replace(b"\0", b" ").decode("utf-8", "replace").strip()
        except OSError:
            cmd = ""
        ppid, uid = None, None
        try:
            with open(os.path.join(base, "status"), encoding="utf-8", errors="replace") as handle:
                for line in handle:
                    if line.startswith("PPid:"):
                        ppid = int(line.split()[1])
                    elif line.startswith("Uid:"):
                        uid = int(line.split()[1])
        except (OSError, ValueError, IndexError):
            pass
        table.append({"pid": int(entry), "ppid": ppid, "uid": uid, "exe": (exe or "").replace(" (deleted)", "") or None, "cmd": cmd})
    return sorted(table, key=lambda row: row["pid"])


def format_process_table(table: Sequence[dict]) -> str:
    return "".join("%d %s %s %s | %s\n" % (row["pid"], row.get("ppid"), row.get("uid"), row.get("exe") or "-", row.get("cmd") or "") for row in table)


def diff_processes(before: Sequence[dict], after: Sequence[dict]) -> List[dict]:
    seen = {(row["pid"], row.get("exe"), row.get("cmd")) for row in before}
    return [row for row in after if (row["pid"], row.get("exe"), row.get("cmd")) not in seen]


def process_findings(before: Optional[Sequence[dict]], after: Optional[Sequence[dict]], app_binary: Optional[str], app_root: Optional[str]) -> dict:
    """What the process table says about launches: new processes that are app binaries (anywhere, or anything under the extracted
    app) or installers. `app_before` counts the original instance in the before-table."""
    if before is None or after is None:
        return {"known": False}
    new = diff_processes(before, after)

    def is_app(row: dict) -> bool:
        exe = row.get("exe") or ""
        return bool((app_root and exe.startswith(str(app_root).rstrip("/") + "/")) or os.path.basename(exe) in APP_BINARY_NAMES or exe == app_binary)

    def is_installer(row: dict) -> bool:
        exe_name = os.path.basename(row.get("exe") or "")
        first = (row.get("cmd") or "").split(" ")[0]
        return exe_name in INSTALLER_EXES or os.path.basename(first) in INSTALLER_EXES

    mains_before = [row for row in before if app_binary and row.get("exe") == app_binary]
    mains_after = [row for row in after if app_binary and row.get("exe") == app_binary]
    return {
        "known": True,
        "new_app": [{"pid": row["pid"], "exe": row.get("exe"), "cmd": tail_text(row.get("cmd") or "", 200)} for row in new if is_app(row)],
        "new_installers": [{"pid": row["pid"], "exe": row.get("exe"), "cmd": tail_text(row.get("cmd") or "", 200)} for row in new if is_installer(row)],
        "app_before": len(mains_before), "app_after": len(mains_after), "new_total": len(new),
    }


# ----------------------------------------------------------------------------------------------------------------------
# file writes
# ----------------------------------------------------------------------------------------------------------------------

def classify_write(path: str, app_root: Optional[str], expected: Sequence[str], transient: Sequence[str] = TRANSIENT_PREFIXES) -> str:
    """app_root_write | update_artifact | expected | transient | unexpected."""
    if app_root and (path == str(app_root) or path.startswith(str(app_root).rstrip("/") + "/")):
        return "app_root_write"
    if UPDATE_ARTIFACT_RE.search(path):
        return "update_artifact"
    for prefix in expected:
        if path == prefix or path.startswith(prefix.rstrip("/") + "/"):
            return "expected"
    for prefix in transient:
        if path.startswith(prefix):
            return "transient"
    return "unexpected"


def default_owner(path: str) -> Optional[int]:
    try:
        return os.lstat(path).st_uid
    except OSError:
        return None


def writes_findings(paths: Iterable[str], app_root: Optional[str], expected: Sequence[str], uid: Optional[int], owner_of: Callable[[str], Optional[int]] = default_owner) -> dict:
    """Classify every written path. Paths owned by another user (root's systemd-private dirs in /tmp, for example) are not the app's
    writes and are listed apart; unknown ownership (the file is gone) stays in the classification."""
    classes: Dict[str, List[str]] = {"app_root_write": [], "update_artifact": [], "expected": [], "transient": [], "unexpected": [], "foreign_owner": []}
    for path in sorted(set(paths)):
        if uid is not None:
            owner = owner_of(path)
            if owner is not None and owner != uid:
                classes["foreign_owner"].append(path)
                continue
        classes[classify_write(path, app_root, expected)].append(path)
    bad = classes["app_root_write"] + classes["update_artifact"] + classes["unexpected"]
    return {"classes": classes, "unexpected": sorted(bad)}


# ----------------------------------------------------------------------------------------------------------------------
# case evaluation (pure)
# ----------------------------------------------------------------------------------------------------------------------

def evaluate_case(data: dict) -> dict:
    """Criteria and verdict of one case from its recorded data. Fail closed: a missing log, a trigger that did not do what the
    case needs, a manifest request never seen (the interception is not demonstrated) or any unknown makes a criterion null and the
    verdict UNPROVED; only an explicit violation makes it FAIL. data keys: scenario, trigger, requests (rows or None), writes_paths
    (list or None), fs_diff (dict or None), outside (dict or None), procs_before / procs_after (tables or None), binary_before /
    binary_after (sha256 or None), app_binary, app_root, expect_app, expected_prefixes, uid, owner_of."""
    unproved: List[str] = []
    criteria: Dict[str, Optional[bool]] = {name: None for name in CRITERIA}
    scenario = data.get("scenario")
    trigger = data.get("trigger")
    definitive, trigger_note = trigger_definitive(str(scenario), trigger)
    if not definitive:
        unproved.append("trigger: " + trigger_note)

    # --- requests: only_manifest_url, no_bundle_download
    rows = data.get("requests")
    summary = summarize_requests(rows) if rows is not None else None
    interception = False
    if summary is None:
        unproved.append("no request log: nothing proves what the updater requested (only_manifest_url, no_bundle_download)")
    elif summary["manifest"] == 0:
        unproved.append("the manifest URL was never requested: the interception is not demonstrated (TLS trust, hosts mapping or the trigger failed)")
    else:
        interception = True
        criteria["only_manifest_url"] = not summary["non_manifest"]
        criteria["no_bundle_download"] = not summary["payload"] and not summary["release_assets"]

    # --- processes: no_new_app_launch
    procs = process_findings(data.get("procs_before"), data.get("procs_after"), data.get("app_binary"), data.get("app_root"))
    if not procs["known"]:
        unproved.append("no process snapshots before and after the trigger (no_new_app_launch, no_install)")
    elif data.get("expect_app") and procs["app_before"] < 1:
        unproved.append("the app was not running in the snapshot taken before the trigger (no_new_app_launch)")
    else:
        criteria["no_new_app_launch"] = not procs["new_app"]

    # --- files: no_new_files
    writes_paths = data.get("writes_paths")
    fs_diff = data.get("fs_diff")
    outside = data.get("outside")
    findings = None
    if writes_paths is None:
        unproved.append("no file-write log (no_new_files, no_install)")
    if fs_diff is None:
        unproved.append("no before/after file snapshot diff (no_new_files)")
    if outside is None or not outside.get("ok"):
        unproved.append("the search for writes outside the sandbox did not complete (no_new_files)")
    if writes_paths is not None and fs_diff is not None and outside is not None and outside.get("ok"):
        every = list(writes_paths) + list(fs_diff.get("added", [])) + list(fs_diff.get("changed", [])) + list(outside.get("paths", []))
        findings = writes_findings(every, data.get("app_root"), data.get("expected_prefixes") or [], data.get("uid"), data.get("owner_of") or default_owner)
        criteria["no_new_files"] = not findings["unexpected"]

    # --- no_install: a payload, an installer process, a changed binary or a write under the extracted app is a violation
    binary_before, binary_after = data.get("binary_before"), data.get("binary_after")
    install_signals: List[str] = []
    if summary is not None and summary["payload"]:
        install_signals.append("a payload was requested: %s" % summary["payload"])
    if procs.get("known") and procs["new_installers"]:
        install_signals.append("installer processes started: %s" % [item["cmd"] for item in procs["new_installers"]])
    if binary_before and binary_after and binary_before != binary_after:
        install_signals.append("the app binary changed (%s -> %s)" % (binary_before[:12], binary_after[:12]))
    if findings is not None and findings["classes"]["app_root_write"]:
        install_signals.append("writes under the extracted app: %s" % findings["classes"]["app_root_write"][:5])
    if install_signals:
        criteria["no_install"] = False
    elif interception and procs.get("known") and binary_before and binary_after and findings is not None:
        criteria["no_install"] = True
    else:
        unproved.append("no_install cannot be established (needs the request log with the manifest request, the process snapshots, the binary hashes and the write log)")

    failed = ["criteria.%s is false" % name for name, value in criteria.items() if value is False]
    if failed:
        verdict = "FAIL"
    elif unproved or not definitive or any(value is not True for value in criteria.values()):
        verdict = "UNPROVED"
        for name, value in criteria.items():
            if value is None and not any(name in reason for reason in unproved):
                unproved.append("criteria.%s is unknown" % name)
    else:
        verdict = "PASS"
    return {
        "criteria": criteria, "verdict": verdict, "reasons": failed + unproved,
        "requests": ({key: summary[key] for key in ("total", "by_host_path", "tls_failures")} if summary is not None else {"total": 0, "by_host_path": [], "tls_failures": []}),
        "details": {
            "trigger": trigger_note, "payload_requests": summary["payload"] if summary else None, "non_manifest_requests": summary["non_manifest"] if summary else None,
            "processes": procs, "writes": (findings["classes"] if findings else None), "install_signals": install_signals,
        },
    }


def tier_result(cases: Sequence[dict], names: Sequence[str]) -> str:
    """PASS only when every requested case ran and passed; FAIL when any case failed; UNPROVED otherwise."""
    if any(case.get("verdict") == "FAIL" for case in cases):
        return "FAIL"
    have = {case.get("name"): case.get("verdict") for case in cases}
    if names and all(have.get(name) == "PASS" for name in names):
        return "PASS"
    return "UNPROVED"


def overall_verdict(result: dict) -> Tuple[str, List[str]]:
    """Fail-closed verdict of the whole OS result (what stage_c_summary.py will also compute)."""
    failed: List[str] = []
    unproved: List[str] = []
    tiers = result.get("tiers") or {}
    attempted = [name for name in TIERS if (tiers.get(name) or {}).get("attempted")]
    if not attempted:
        unproved.append("no tier was attempted")
    for name in attempted:
        state = tiers[name].get("result")
        if state == "FAIL":
            failed.append("tier %s failed" % name)
        elif state != "PASS":
            unproved.append("tier %s is %s: %s" % (name, state, tiers[name].get("reason")))
    cases = [case for case in result.get("cases") or [] if isinstance(case, dict)]
    for name in attempted:
        for wanted in CASES:
            mine = [case for case in cases if case.get("tier") == name and case.get("name") == wanted]
            if not mine:
                unproved.append("tier %s has no %s case" % (name, wanted))
            for case in mine:
                if case.get("verdict") == "FAIL" or any(case.get("criteria", {}).get(key) is False for key in CRITERIA):
                    failed.append("%s/%s: a criterion is false or the case failed" % (name, wanted))
                elif case.get("verdict") != "PASS" or any(case.get("criteria", {}).get(key) is not True for key in CRITERIA):
                    unproved.append("%s/%s: not every criterion is proved" % (name, wanted))
    for index, control in enumerate(result.get("controls") or []):
        state = control.get("verdict") if isinstance(control, dict) else None
        if state == "FAIL":
            failed.append("control[%d] failed: the recorder did not see the bundle request the control caused, so its 'no bundle' findings prove nothing" % index)
        elif state != "PASS":
            unproved.append("control[%d] is not proven" % index)
    app = result.get("app") or {}
    if "released_app" in attempted:
        if app.get("digest_match") is False:
            failed.append("the app that ran is not the released asset (digest mismatch)")
        elif app.get("digest_match") is not True:
            unproved.append("app.digest_match is not true")
    if attempted and (result.get("plugin") or {}).get("version") != PLUGIN_VERSION:
        unproved.append("plugin version %s is not proven" % PLUGIN_VERSION)
    if any(case.get("scenario") == "latest-404" for case in result.get("cases") or []):
        if RELEASE_NOT_FOUND not in str(result.get("manifest404_error_text") or ""):
            unproved.append("the ReleaseNotFound error text was not recorded")
    isolation = result.get("isolation") or {}
    if isolation.get("live_block") is not True:
        unproved.append("the live-address block is not proven")
    if not isolation.get("hosts_mapped"):
        unproved.append("no hosts mapping recorded")
    if failed:
        return "FAIL", failed + unproved
    if unproved:
        return "UNPROVED", unproved
    return "PASS", []


# ----------------------------------------------------------------------------------------------------------------------
# runner plumbing
# ----------------------------------------------------------------------------------------------------------------------

class CmdResult:
    __slots__ = ("rc", "out", "seconds")

    def __init__(self, rc: int, out: str, seconds: float = 0.0):
        self.rc, self.out, self.seconds = rc, out, seconds

    @property
    def ok(self) -> bool:
        return self.rc == 0


class StepLog:
    """steps.log: every command with its exit code and a short output tail, plus notes. Never environment values."""

    def __init__(self, path: Optional[Path]):
        self.path = path
        self.started = time.time()
        self.mask_values: List[str] = []
        if path is not None:
            path.parent.mkdir(parents=True, exist_ok=True)
            with open(str(path), "a", encoding="utf-8") as handle:
                handle.write("steps.log of os_linux.py started %s\n" % now_utc())

    def add(self, text: str) -> None:
        text = mask_ips(text, self.mask_values)
        line = "[%7.1fs] %s" % (time.time() - self.started, text)
        print(line, flush=True)
        if self.path is not None:
            with open(str(self.path), "a", encoding="utf-8") as handle:
                handle.write(line + "\n")


class Ctx:
    def __init__(self, evidence: Path, work: Optional[Path] = None):
        self.evidence = Path(evidence)
        self.evidence.mkdir(parents=True, exist_ok=True)
        base = Path(work) if work else Path(os.environ.get("RUNNER_TEMP") or tempfile.gettempdir()) / "wave0-desktop-linux"
        self.work = base
        self.work.mkdir(parents=True, exist_ok=True)
        self.log = StepLog(self.evidence / "steps.log")
        self.uid = os.getuid() if hasattr(os, "getuid") else None
        self.state: Dict[str, Any] = {"hosts_mapped": False, "live_ips": [], "live_block_applied": False, "ca": None, "ca_trusted": False}
        self.started = time.monotonic()

    def run(self, argv: Sequence[str], timeout: float = 300, env: Optional[Dict[str, str]] = None, cwd: Optional[Path] = None,
            input_text: Optional[str] = None, tail: int = 500, log: bool = True) -> CmdResult:
        began = time.time()
        try:
            done = subprocess.run(list(argv), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout, env=env, cwd=str(cwd) if cwd else None,
                                  input=input_text.encode("utf-8") if input_text is not None else None, check=False)
            rc, out = done.returncode, done.stdout.decode("utf-8", "replace")
        except subprocess.TimeoutExpired as error:
            rc, out = 124, (error.stdout or b"").decode("utf-8", "replace") + "\n[timed out after %ss]" % timeout
        except OSError as error:
            rc, out = 127, "%s: %s" % (type(error).__name__, error)
        elapsed = time.time() - began
        if log:
            self.log.add("$ %s -> rc %d (%.1fs)" % (shlex.join([str(a) for a in argv])[:300], rc, elapsed))
            if rc != 0 and out.strip():
                self.log.add("  " + tail_text(out.strip(), tail).replace("\n", "\n  "))
        return CmdResult(rc, out, elapsed)

    def sudo(self, argv: Sequence[str], **kwargs: Any) -> CmdResult:
        return self.run(["sudo", "-n"] + list(argv), **kwargs)


def load_lib(name: str) -> Any:
    """Import one of the CORE fork's lib/ modules (ca, mitm_server, fswatch, live_block)."""
    if str(LIB_DIR) not in sys.path:
        sys.path.insert(0, str(LIB_DIR))
    return importlib.import_module(name)


# ----------------------------------------------------------------------------------------------------------------------
# system setup (CI only)
# ----------------------------------------------------------------------------------------------------------------------

def runner_info() -> dict:
    osr = {}
    try:
        for line in Path("/etc/os-release").read_text(encoding="utf-8").splitlines():
            if "=" in line:
                key, _, value = line.partition("=")
                osr[key] = value.strip('"')
    except OSError:
        pass
    return {
        "image": "%s %s" % (os.environ.get("ImageOS", "unknown"), os.environ.get("ImageVersion", "unknown")),
        "arch": platform.machine(), "os_version": osr.get("PRETTY_NAME") or platform.platform(), "kernel": platform.release(),
        "python": platform.python_version(),
    }


def install_packages(ctx: Ctx, packages: Sequence[str], label: str) -> dict:
    """apt-get install in one go; if that fails, one by one so the log says exactly which package is missing."""
    env = apt_env()
    updated = ctx.state.get("apt_updated")
    commands = apt_commands(packages, update=not updated)
    if not updated:
        first = ctx.run(commands[0], env=env, timeout=600)
        ctx.state["apt_updated"] = first.ok
        commands = commands[1:]
    done = ctx.run(commands[-1], env=env, timeout=1200)
    if done.ok:
        return {"label": label, "installed": list(packages), "failed": []}
    installed, failed = [], []
    for package in packages:
        one = ctx.run(apt_commands([package], update=False)[0], env=env, timeout=600)
        (installed if one.ok else failed).append(package)
    return {"label": label, "installed": installed, "failed": failed}


def relax_userns_restriction(ctx: Ctx) -> dict:
    """ubuntu-24.04 restricts unprivileged user namespaces with AppArmor, which breaks WebKitGTK's bubblewrap sandbox."""
    before = ctx.run(["sysctl", "-n", "kernel.apparmor_restrict_unprivileged_userns"], log=False)
    changed = ctx.sudo(["sysctl", "-w", "kernel.apparmor_restrict_unprivileged_userns=0"])
    return {"before": before.out.strip() if before.ok else None, "set_rc": changed.rc}


def fetch_release_metadata(ctx: Ctx, config: Optional[dict]) -> dict:
    """Release asset facts: live API first, then config.json's recorded digest. Returns {asset, source, notes}."""
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    notes: List[str] = []
    live: Optional[dict] = None
    try:
        live = select_linux_asset(fetch_release_json(token))
        ctx.log.add("release %s asset %s digest %s (live API%s)" % (TAG, live["name"], live["release_digest"], ", authenticated" if token else ", anonymous"))
    except Exception as error:  # noqa: BLE001 - recorded, then the config fallback is tried
        notes.append("live release API failed: %s: %s" % (type(error).__name__, error))
        ctx.log.add(notes[-1])
    recorded = config_asset(config)
    if live and recorded and (live["name"], live["sha256"]) != (recorded["name"], recorded["sha256"]):
        raise AssetError("the live release asset (%s %s) differs from config.json (%s %s)" % (live["name"], live["sha256"][:12], recorded["name"], recorded["sha256"][:12]))
    chosen = live or recorded
    if chosen is None:
        raise AssetError("no release asset facts: the live API failed and config.json records none (%s)" % "; ".join(notes))
    return {"asset": chosen, "source": "live release API" if live else "config.json (live API unavailable)", "notes": notes, "live_ok": live is not None}


def download_and_extract(ctx: Ctx, asset: dict) -> dict:
    """Download, verify the digest BEFORE use, extract (never install). Returns the facts for result.app."""
    downloads = ctx.work / "download"
    downloads.mkdir(parents=True, exist_ok=True)
    target = downloads / asset["name"]
    got = ctx.run(["curl", "-fL", "--proto", "=https", "--tlsv1.2", "--retry", "3", "--retry-delay", "3", "-o", str(target), asset["url"]], timeout=900)
    if not got.ok or not target.is_file():
        raise AssetError("downloading %s failed (rc %d): %s" % (asset["url"], got.rc, tail_text(got.out, 300)))
    facts = verify_download(target, asset)
    ctx.log.add("downloaded %s: sha256 %s, release digest %s, match=%s" % (asset["name"], facts["asset_sha256"], asset["release_digest"], facts["digest_match"]))
    if not facts["digest_match"]:
        return dict(facts, extracted=False, binary=None, reason="the downloaded file does not match the release digest; it is not used")
    root = ctx.work / "app"
    root.mkdir(parents=True, exist_ok=True)
    if asset["kind"] == "deb":
        extracted = ctx.run(["dpkg-deb", "-x", str(target), str(root)], timeout=300)
    else:
        target.chmod(0o755)
        extracted = ctx.run([str(target), "--appimage-extract"], cwd=root, timeout=600)
    binary = find_app_binary(root)
    if not extracted.ok or binary is None:
        return dict(facts, extracted=False, binary=None, reason="extraction failed (rc %d) or usr/bin/arc-desktop is missing" % extracted.rc)
    header = binary.read_bytes()[:4]
    ldd = ctx.run(["ldd", str(binary)], log=False)
    missing = parse_ldd_missing(ldd.out)
    ctx.log.add("extracted %s (ELF=%s, missing libraries: %s)" % (binary, header == b"\x7fELF", missing or "none"))
    return dict(facts, extracted=True, binary=str(binary), root=str(root), elf=header == b"\x7fELF", missing_libraries=missing,
                binary_sha256=sha256_file(binary), reason=None)


def gather_provenance(ctx: Ctx, binary: Path) -> dict:
    """Crate versions from the strings of the released binary + F4's source check. plugin.version is only set when the binary says
    exactly one tauri-plugin-updater version and it is the pinned one."""
    data = binary.read_bytes()
    versions = binary_crate_versions(data)
    plugin_versions = versions.get("tauri-plugin-updater", [])
    provenance = ["strings of %s: %s" % (binary.name, ", ".join("%s-%s" % (name, "/".join(found)) for name, found in sorted(versions.items())) or "no crate versions found")]
    out = ctx.evidence / "provenance-source.json"
    checker = HERE / "desktop_updater_check.py"
    source: dict = {"ran": False}
    if checker.is_file():
        fetched = ctx.run(["git", "-C", str(REPO_ROOT), "fetch", "--no-tags", "--depth=1", "origin", "refs/tags/%s:refs/tags/%s" % (TAG, TAG)], timeout=240)
        done = ctx.run([sys.executable, str(checker), "check-source", "--repo", str(REPO_ROOT), "--tag", TAG, "--out", str(out), "--released-binary", str(binary)], timeout=600, tail=1500)
        source = {"ran": True, "tag_fetch_rc": fetched.rc, "rc": done.rc, "verdict": "SOURCE_VERIFIED" if done.ok else "not verified (rc %d)" % done.rc,
                  "output_tail": tail_text(done.out.strip(), 1500)}
        provenance.append("desktop_updater_check.py check-source: %s" % source["verdict"])
    document = {"binary": binary.name, "binary_sha256": sha256_file(binary), "crate_versions_in_binary": versions, "source_check": source, "provenance": provenance}
    write_json(ctx.evidence / "provenance.json", document)
    proven = plugin_versions == [PLUGIN_VERSION]
    return {"version": PLUGIN_VERSION if proven else (plugin_versions[0] if len(plugin_versions) == 1 else ""), "provenance": provenance, "proven": proven}


def lockfile_plugin_version(lock_path: Optional[Path] = None) -> Optional[str]:
    """tauri-plugin-updater version pinned by the native crate's Cargo.lock (None when absent)."""
    path = lock_path or (HERE / "native-updater-check" / "Cargo.lock")
    try:
        text = Path(path).read_text(encoding="utf-8")
    except OSError:
        return None
    match = re.search(r'name = "tauri-plugin-updater"\nversion = "([0-9.]+)"', text)
    return match.group(1) if match else None


def setup_ca(ctx: Ctx) -> dict:
    """Per-run CA + server certificate (keys stay under the work dir), trusted system-wide; only ca.crt and ca.sha256 are evidence."""
    ca = load_lib("ca")
    info = ca.make_ca(ctx.work / "ca", MITM_HOSTS)
    ca_cert = Path(str(info.get("ca_cert") or info.get("ca_crt")))
    server_cert = str(info.get("server_cert") or info.get("server_crt"))
    server_key = str(info.get("server_key"))
    ca.copy_public(ctx.work / "ca", ctx.evidence)
    ctx.sudo(["cp", str(ca_cert), "/usr/local/share/ca-certificates/" + CA_FILE_NAME])
    updated = ctx.sudo(["update-ca-certificates"], timeout=120)
    verified = ctx.run(["openssl", "verify", "-CAfile", SYSTEM_BUNDLE, server_cert])
    trusted = updated.ok and verified.ok
    ctx.state["ca"] = {"ca_cert": str(ca_cert), "server_cert": server_cert, "server_key": server_key, "ca_sha256": info.get("ca_sha256")}
    ctx.state["ca_trusted"] = trusted
    return {"ca_sha256": info.get("ca_sha256"), "trusted": trusted, "update_ca_certificates_rc": updated.rc, "openssl_verify_rc": verified.rc}


def apply_live_block(ctx: Ctx, resources_dir: Optional[Path] = None) -> dict:
    """REJECT every ARC live address and blackhole the ARC names (lib/live_block.py builds the commands, as in Stage A); verify the
    rules. live_block is True only when every address has a REJECT rule AND the module's own verify script succeeds."""
    block = None
    try:
        block = load_lib("live_block")
        ips = block.load_live_ips(REPO_ROOT)
    except Exception as error:  # noqa: BLE001 - fall back to the local loader (same rule), recorded
        ctx.log.add("lib/live_block.py unavailable (%s); using the local iptables builder" % error)
        block = None
        ips = load_live_ips(REPO_ROOT)
    ctx.state["live_ips"] = ips
    ctx.log.mask_values = list(ips)
    names_note = ""
    verify_rc: Optional[int] = None
    verify_out = ""
    if block is not None:
        extra: List[str] = []
        if resources_dir is not None:
            texts = []
            for name in ("testnet-seeds.txt", "genesis.toml"):
                candidate = Path(resources_dir) / name
                texts.append(candidate.read_text(encoding="utf-8", errors="replace") if candidate.is_file() else "")
            extra = block.names_in_seed_files(texts[0], texts[1])
        built = block.build("linux", ips, block.blocked_names(extra))
        for command in built["commands"]:
            ctx.run(["bash", "-c", command], log=False)
        done = ctx.run(["bash", "-c", built["verify"]], timeout=60, log=True)
        verify_rc, verify_out = done.rc, done.out
        names_note = "ARC and third-party names blackholed to loopback (%d extra from the seeds/genesis files)" % len(extra)
    else:
        for command in iptables_block_commands(ips):
            ctx.run(command, log=False)
    listing = ctx.sudo(["iptables", "-S", "OUTPUT"], log=False)
    present = parse_iptables_rules(listing.out, ips)
    ctx.state["live_block_applied"] = True
    ok = listing.ok and all(present.values()) and (verify_rc in (None, 0))
    ctx.log.add("live-address block: %d addresses, every REJECT rule present=%s, module verify rc=%s" % (len(ips), all(present.values()), verify_rc))
    return {"ok": ok, "ips": len(ips), "missing_rules": ["live-ip-%d" % (index + 1) for index, ip in enumerate(ips) if not present.get(ip)], "verify_rc": verify_rc,
            "verify_output": mask_ips(tail_text(verify_out, 600), ips), "rules": mask_ips(tail_text(listing.out, 2000), ips), "names": names_note}


def live_counters(ctx: Ctx) -> Dict[str, int]:
    """Packets rejected per live address, keyed live-ip-N (the addresses themselves are not written to the evidence)."""
    ips = ctx.state.get("live_ips") or []
    listing = ctx.sudo(["iptables", "-nvxL", "OUTPUT"], log=False)
    counted = parse_iptables_counters(listing.out, ips)
    return {"live-ip-%d" % (index + 1): counted.get(ip, 0) for index, ip in enumerate(ips)}


def remove_live_block(ctx: Ctx) -> None:
    if ctx.state.get("live_block_applied"):
        for command in iptables_unblock_commands(ctx.state.get("live_ips") or []):
            ctx.run(command, log=False)
        ctx.state["live_block_applied"] = False


@contextlib.contextmanager
def hosts_mapped(ctx: Ctx):
    """The GitHub names point at 127.0.0.1 only while a case runs."""
    lines = hosts_lines(MITM_HOSTS)
    ctx.sudo(["tee", "-a", "/etc/hosts"], input_text="\n".join(lines) + "\n", log=False)
    ctx.state["hosts_mapped"] = True
    ctx.state["hosts_mapped_names"] = list(MITM_HOSTS)
    ctx.state["hosts_mapped_lines"] = lines
    try:
        yield lines
    finally:
        ctx.sudo(["sed", "-i", "/%s/d" % HOSTS_MARK, "/etc/hosts"], log=False)
        ctx.state["hosts_mapped"] = False


def resolve_check(ctx: Ctx) -> Dict[str, str]:
    out = {}
    for name in MITM_HOSTS:
        got = ctx.run(["getent", "hosts", name], log=False)
        out[name] = got.out.split()[0] if got.ok and got.out.split() else "unresolved"
    return out


class MitmProcess:
    """lib/mitm_server.py as root on 127.0.0.1:443 (the CLI is the interface: --ready-file, SIGTERM to stop)."""

    def __init__(self, ctx: Ctx, scenario: str, log_path: Path):
        self.ctx, self.scenario, self.log_path = ctx, scenario, Path(log_path)
        self.ready = ctx.work / ("mitm-ready-%s-%d.json" % (scenario, int(time.time() * 1000)))
        self.proc: Optional[subprocess.Popen] = None
        self.pid: Optional[int] = None

    def start(self, timeout: float = 30.0) -> None:
        ca = self.ctx.state["ca"]
        argv = ["sudo", "-n", sys.executable, str(LIB_DIR / "mitm_server.py"), "--scenario", self.scenario, "--cert", ca["server_cert"], "--key", ca["server_key"],
                "--listen", "127.0.0.1,::1:443", "--log", str(self.log_path), "--ready-file", str(self.ready)]
        self.log_path.parent.mkdir(parents=True, exist_ok=True)
        self.proc = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if self.proc.poll() is not None:
                out = (self.proc.stdout.read() if self.proc.stdout else b"").decode("utf-8", "replace")
                raise RuntimeError("mitm_server exited early (rc %s): %s" % (self.proc.returncode, tail_text(out, 600)))
            if self.ready.is_file():
                try:
                    self.pid = int(json.loads(self.ready.read_text(encoding="utf-8")).get("pid"))
                except (ValueError, TypeError, OSError):
                    time.sleep(0.2)
                    continue
                self.ctx.log.add("mitm_server up: scenario %s pid %s log %s" % (self.scenario, self.pid, self.log_path.name))
                return
            time.sleep(0.2)
        self.stop()
        raise RuntimeError("mitm_server did not become ready within %ss" % timeout)

    def stop(self) -> None:
        if self.pid:
            self.ctx.sudo(["kill", "-TERM", str(self.pid)], log=False)
        if self.proc is not None:
            try:
                self.proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                if self.pid:
                    self.ctx.sudo(["kill", "-KILL", str(self.pid)], log=False)
                self.proc.kill()
        if self.ctx.uid is not None:
            self.ctx.sudo(["chown", "%d:%d" % (self.ctx.uid, os.getgid()), str(self.log_path), str(self.ready)], log=False)


def https_selftest(ctx: Ctx) -> dict:
    """The interception end to end with the system trust store: curl for the manifest URL must reach OUR server (header
    Server: wave0-lab-mitm) and get the scenario's answer. Uses its own server instance and log, apart from every case."""
    log_path = ctx.evidence / "selftest-requests.jsonl"
    result: dict = {"ok": False}
    try:
        with hosts_mapped(ctx):
            result["resolves"] = resolve_check(ctx)
            server = MitmProcess(ctx, "latest-404", log_path)
            server.start()
            try:
                got = ctx.run(["curl", "-sS", "-i", "--max-time", "20", MANIFEST_URL], timeout=40, log=True)
                result["curl_rc"] = got.rc
                result["response_head"] = tail_text(got.out, 600)
                first_line = got.out.split("\n", 1)[0]
                result["ok"] = got.ok and "wave0-lab-mitm" in got.out.lower() and (" 302" in first_line or " 404" in first_line)
            finally:
                server.stop()
    except Exception as error:  # noqa: BLE001
        result["error"] = "%s: %s" % (type(error).__name__, error)
    return result


# ----------------------------------------------------------------------------------------------------------------------
# one case
# ----------------------------------------------------------------------------------------------------------------------

def fs_module() -> Any:
    return load_lib("fswatch")


def take_snapshot(roots: Sequence[Path]) -> dict:
    return fs_module().snapshot([str(r) for r in roots])


def snapshot_diff(before: dict, after: dict) -> dict:
    return fs_module().diff(before, after)


def find_outside_writes(ctx: Ctx, marker: Path, roots: Sequence[str]) -> dict:
    """Files newer than the marker under install-related roots outside the sandbox (a best-effort net for stray installs)."""
    existing = [root for root in roots if os.path.exists(root)]
    if not existing:
        return {"ok": True, "paths": [], "roots": []}
    done = ctx.sudo(["find"] + existing + ["-xdev", "-newer", str(marker), "(", "-type", "f", "-o", "-type", "l", ")", "-print"], timeout=240, log=False)
    paths = sorted({line for line in done.out.splitlines() if line.startswith("/")})
    return {"ok": done.rc in (0, 1), "paths": paths, "roots": existing, "rc": done.rc}


OUTSIDE_ROOTS = ("/usr/local/bin", "/usr/local/lib", "/usr/local/share/applications", "/opt", "/usr/share/applications", "/etc/xdg/autostart", "/usr/bin", "/usr/lib/ARC Node")


def writes_paths_from_log(path: Path) -> Optional[List[str]]:
    """Paths the poller saw added or changed (a removal is not a new file). None when there is no log at all."""
    rows, _bad = read_jsonl(path)
    if rows is None:
        return None
    return [str(row["path"]) for row in rows if isinstance(row.get("path"), str) and row.get("event", "added") in ("added", "changed")]


class CaseRunner:
    """Runs one case of one tier and returns the case record for result.json."""

    def __init__(self, ctx: Ctx, tier: str, case: str, scenario: str, tag: Optional[str] = None):
        self.ctx, self.tier, self.case, self.scenario = ctx, tier, case, scenario
        self.tag = tag or case_tag(tier, case)
        self.sandbox = ctx.work / "sandbox" / self.tag
        self.files: List[str] = []

    def evidence_path(self, pattern: str) -> Path:
        name = pattern % self.tag
        self.files.append(name)
        return self.ctx.evidence / name

    def prepare_sandbox(self, seed_home: Optional[Path]) -> None:
        for sub in ("home", "tmp", "runtime"):
            (self.sandbox / sub).mkdir(parents=True, exist_ok=True)
        os.chmod(str(self.sandbox / "runtime"), 0o700)
        if seed_home is not None and Path(seed_home).is_dir():
            shutil.copytree(str(seed_home), str(self.sandbox / "home"), symlinks=True, dirs_exist_ok=True)

    def start_poller(self, roots: Sequence[Path], out_path: Path) -> Any:
        poller = fs_module().Poller([str(r) for r in roots], 0.5, str(out_path))
        poller.start()
        return poller

    def finish(self, trigger: Optional[dict], mitm_log: Path, writes_log: Path, before: Optional[dict], after: Optional[dict], procs_before: Optional[list],
               procs_after: Optional[list], binary_before: Optional[str], binary_after: Optional[str], app_binary: Optional[str], app_root: Optional[str],
               outside: Optional[dict], expect_app: bool, counters: Dict[str, Any]) -> dict:
        rows, bad = read_jsonl(mitm_log)
        writes = writes_paths_from_log(writes_log)
        fs_diff = None
        if before is not None and after is not None:
            try:
                fs_diff = snapshot_diff(before, after)
            except Exception as error:  # noqa: BLE001
                self.ctx.log.add("snapshot diff failed: %s" % error)
        analysis = evaluate_case({
            "scenario": self.scenario, "trigger": trigger, "requests": rows, "writes_paths": writes, "fs_diff": fs_diff, "outside": outside,
            "procs_before": procs_before, "procs_after": procs_after, "binary_before": binary_before, "binary_after": binary_after,
            "app_binary": app_binary, "app_root": app_root, "expect_app": expect_app, "expected_prefixes": expected_prefixes(self.sandbox), "uid": self.ctx.uid,
        })
        if bad:
            analysis["reasons"].append("%d unreadable line(s) in the request log" % bad)
        existing = [name for name in self.files if (self.ctx.evidence / name).is_file()]
        return {
            "name": self.case, "tier": self.tier, "scenario": self.scenario, "trigger_outcome": trigger or {}, "requests": analysis["requests"],
            "criteria": analysis["criteria"], "evidence_files": existing, "verdict": analysis["verdict"], "reasons": analysis["reasons"], "details": analysis["details"],
            "live_counters": counters,
        }


def run_released_case(ctx: Ctx, asset_facts: dict, case: str, scenario: str, seed_home: Optional[Path], control: bool = False) -> dict:
    """Launch the extracted released app under Xvfb through WebKitWebDriver and trigger plugin:updater|check once."""
    runner = CaseRunner(ctx, "released_app", case, scenario, tag=("control-" + case) if control else None)
    runner.prepare_sandbox(seed_home)
    binary = asset_facts["binary"]
    app_root = asset_facts["root"]
    mitm_log = runner.evidence_path("requests-%s.jsonl")
    writes_log = runner.evidence_path("writes-%s.jsonl")
    fs_before_path = runner.evidence_path("fs-%s-before.json")
    fs_after_path = runner.evidence_path("fs-%s-after.json")
    procs_before_path = runner.evidence_path("procs-%s-before.txt")
    procs_after_path = runner.evidence_path("procs-%s-after.txt")
    outside_path = runner.evidence_path("outside-writes-%s.txt")
    trigger_path = runner.evidence_path("trigger-%s.json")
    app_log_path = runner.evidence_path("app-%s.log")
    env = sandbox_env(runner.sandbox)
    roots = [runner.sandbox, Path(app_root), Path("/tmp")]
    marker = ctx.work / ("marker-%s" % runner.tag)
    marker.write_text(now_utc(), encoding="utf-8")
    time.sleep(1.1)  # find -newer compares mtimes at second resolution on some filesystems
    trigger: Optional[dict] = None
    raw_trigger: Any = None
    notes: List[str] = []
    before = after = procs_before = procs_after = None
    outside: Optional[dict] = None
    counters: Dict[str, Any] = {}
    binary_before = sha256_file(Path(binary))
    binary_after: Optional[str] = None
    driver: Optional[subprocess.Popen] = None
    poller = None
    server: Optional[MitmProcess] = None
    port = WEBDRIVER_PORT + int(ctx.state.get("driver_count", 0))
    ctx.state["driver_count"] = int(ctx.state.get("driver_count", 0)) + 1
    client = WebDriverClient(port=port)
    try:
        with hosts_mapped(ctx):
            counters["before"] = live_counters(ctx)
            server = MitmProcess(ctx, scenario, mitm_log)
            server.start()
            poller = runner.start_poller(roots, writes_log)
            driver = start_driver(ctx, env, app_log_path, port)
            if not client.wait_ready(45):
                raise RuntimeError("WebKitWebDriver never answered /status on port %d" % port)
            session, attempts = open_session(client, binary)
            notes.extend(attempts)
            client.set_timeouts()
            ready = wait_tauri_ready(client, 90)
            notes.append("tauri internals ready=%s" % ready)
            time.sleep(10)  # the app settles: first-launch writes happen before the 'before' snapshot
            before = take_snapshot(roots)
            write_json(fs_before_path, before)
            procs_before = read_process_table()
            procs_before_path.write_text(format_process_table(procs_before), encoding="utf-8")
            began = time.time()
            raw_trigger = client.execute_async(CHECK_SCRIPT)
            trigger = normalize_webdriver_trigger(raw_trigger)
            trigger["seconds"] = round(time.time() - began, 2)
            ctx.log.add("trigger %s -> %s: %s" % (runner.tag, trigger["outcome"], tail_text(str(trigger.get("text")), 200)))
            if control and trigger.get("outcome") == "update_available" and trigger.get("rid") is not None:
                try:
                    control_raw = client.execute_async(DOWNLOAD_CONTROL_SCRIPT, [trigger["rid"]])
                    trigger["download_control"] = control_raw
                except WebDriverError as error:
                    trigger["download_control"] = {"ok": False, "error": str(error)}
            time.sleep(20)  # let any follow-up request, download or process start show up
            after = take_snapshot(roots)
            write_json(fs_after_path, after)
            procs_after = read_process_table()
            procs_after_path.write_text(format_process_table(procs_after), encoding="utf-8")
            counters["after"] = live_counters(ctx)
    except Exception as error:  # noqa: BLE001 - recorded; the case stays UNPROVED
        notes.append("case aborted: %s: %s" % (type(error).__name__, error))
        ctx.log.add(notes[-1])
        ctx.log.add(tail_text(traceback.format_exc(), 1500))
    finally:
        with contextlib.suppress(Exception):
            client.delete_session()
        stop_driver(driver)
        with contextlib.suppress(Exception):
            if poller is not None:
                poller.stop()
        with contextlib.suppress(Exception):
            if server is not None:
                server.stop()
        with contextlib.suppress(Exception):
            binary_after = sha256_file(Path(binary))
        with contextlib.suppress(Exception):
            outside = find_outside_writes(ctx, marker, OUTSIDE_ROOTS)
            outside_path.write_text("\n".join(outside["paths"]) + ("\n" if outside["paths"] else ""), encoding="utf-8")
        trim_log(app_log_path, ips=ctx.state.get("live_ips") or [])
    trigger_doc = {"raw": raw_trigger, "normalized": trigger, "notes": notes}
    write_json(trigger_path, trigger_doc)
    record = runner.finish(trigger, mitm_log, writes_log, before, after, procs_before, procs_after, binary_before, binary_after, binary, app_root, outside, True, counters)
    if notes:
        record["notes"] = notes
    return record


def start_driver(ctx: Ctx, env: Dict[str, str], log_path: Path, port: int = WEBDRIVER_PORT) -> subprocess.Popen:
    """xvfb-run [dbus-run-session --] WebKitWebDriver: the driver launches the app itself and the app inherits this environment."""
    driver = shutil.which("WebKitWebDriver") or next(iter(sorted(Path("/usr/lib").glob("*/webkit2gtk-4.*/WebKitWebDriver"))), None)
    xvfb = shutil.which("xvfb-run")
    if not driver or not xvfb:
        raise RuntimeError("WebKitWebDriver (%s) or xvfb-run (%s) is not installed" % (driver, xvfb))
    argv = [xvfb, "-a", "-s", "-screen 0 1280x800x24"]
    session_bus = shutil.which("dbus-run-session")
    if session_bus:
        argv += [session_bus, "--"]
    argv += [str(driver), "--port=%d" % port, "--host=%s" % WEBDRIVER_HOST]
    ctx.log.add("starting the driver: %s" % shlex.join(argv))
    handle = open(str(log_path), "wb")
    proc = subprocess.Popen(argv, env=env, stdout=handle, stderr=subprocess.STDOUT, start_new_session=True)
    proc.log_handle = handle  # type: ignore[attr-defined]
    return proc


def stop_driver(proc: Optional[subprocess.Popen]) -> None:
    if proc is None:
        return
    handle = getattr(proc, "log_handle", None)
    try:
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(proc.pid, sig)
            except (OSError, ProcessLookupError):
                return
            try:
                proc.wait(timeout=10)
                return
            except subprocess.TimeoutExpired:
                continue
    finally:
        if handle is not None:
            with contextlib.suppress(Exception):
                handle.close()


def wait_tauri_ready(client: WebDriverClient, total_s: float) -> bool:
    end = time.monotonic() + total_s
    while time.monotonic() < end:
        try:
            if client.execute_sync(READY_SCRIPT, timeout=15) is True:
                return True
        except (WebDriverError, OSError, http.client.HTTPException):
            pass
        time.sleep(2)
    return False


def trim_log(path: Path, limit: int = 300_000, ips: Sequence[str] = ()) -> None:
    """Keep the tail of an app log and mask the live addresses an attempted connection may have named."""
    try:
        data = path.read_bytes()
        if len(data) > limit:
            data = b"[... %d earlier bytes trimmed ...]\n" % (len(data) - limit) + data[-limit:]
        if ips:
            data = mask_ips(data.decode("utf-8", "replace"), ips).encode("utf-8")
        path.write_bytes(data)
    except OSError:
        pass


def last_json_report(output: str) -> Optional[dict]:
    """The checker prints exactly one JSON line on stdout (stderr diagnostics are merged in here): the last line that parses as an
    object with the checker's schema, else None."""
    for line in reversed(output.splitlines()):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if isinstance(item, dict) and item.get("schema"):
            return item
    return None


def run_native_case(ctx: Ctx, binary: Path, case: str, scenario: str, control: bool = False) -> dict:
    """Run native-updater-check once against the intercepted manifest URL (system trust store + SSL_CERT_FILE)."""
    runner = CaseRunner(ctx, "native_check", case, scenario, tag=("native-control-" + case) if control else None)
    runner.prepare_sandbox(None)
    mitm_log = runner.evidence_path("requests-%s.jsonl")
    writes_log = runner.evidence_path("writes-%s.jsonl")
    fs_before_path = runner.evidence_path("fs-%s-before.json")
    fs_after_path = runner.evidence_path("fs-%s-after.json")
    procs_before_path = runner.evidence_path("procs-%s-before.txt")
    procs_after_path = runner.evidence_path("procs-%s-after.txt")
    outside_path = runner.evidence_path("outside-writes-%s.txt")
    trigger_path = runner.evidence_path("trigger-%s.json")
    env = sandbox_env(runner.sandbox)
    roots = [runner.sandbox, Path("/tmp")]
    marker = ctx.work / ("marker-%s" % runner.tag)
    marker.write_text(now_utc(), encoding="utf-8")
    time.sleep(1.1)
    trigger: Optional[dict] = None
    raw: Any = None
    notes: List[str] = []
    before = after = procs_before = procs_after = None
    outside: Optional[dict] = None
    counters: Dict[str, Any] = {}
    binary_before = sha256_file(binary)
    binary_after: Optional[str] = None
    poller = None
    server: Optional[MitmProcess] = None
    argv = [str(binary), "--endpoint", MANIFEST_URL, "--current-version", APP_VERSION, "--pubkey", UPDATER_PUBKEY]
    if control:
        argv.append("--control-download")
    try:
        with hosts_mapped(ctx):
            counters["before"] = live_counters(ctx)
            server = MitmProcess(ctx, scenario, mitm_log)
            server.start()
            poller = runner.start_poller(roots, writes_log)
            before = take_snapshot(roots)
            write_json(fs_before_path, before)
            procs_before = read_process_table()
            procs_before_path.write_text(format_process_table(procs_before), encoding="utf-8")
            began = time.time()
            done = ctx.run(argv, env=env, cwd=runner.sandbox / "tmp", timeout=180, tail=1200)
            raw = last_json_report(done.out)
            trigger = normalize_native_trigger(raw)
            trigger["seconds"] = round(time.time() - began, 2)
            trigger["rc"] = done.rc
            time.sleep(5)
            after = take_snapshot(roots)
            write_json(fs_after_path, after)
            procs_after = read_process_table()
            procs_after_path.write_text(format_process_table(procs_after), encoding="utf-8")
            counters["after"] = live_counters(ctx)
    except Exception as error:  # noqa: BLE001
        notes.append("case aborted: %s: %s" % (type(error).__name__, error))
        ctx.log.add(notes[-1])
    finally:
        with contextlib.suppress(Exception):
            if poller is not None:
                poller.stop()
        with contextlib.suppress(Exception):
            if server is not None:
                server.stop()
        with contextlib.suppress(Exception):
            binary_after = sha256_file(binary)
        with contextlib.suppress(Exception):
            outside = find_outside_writes(ctx, marker, OUTSIDE_ROOTS)
            outside_path.write_text("\n".join(outside["paths"]) + ("\n" if outside["paths"] else ""), encoding="utf-8")
    write_json(trigger_path, {"raw": raw, "normalized": trigger, "notes": notes})
    record = runner.finish(trigger, mitm_log, writes_log, before, after, procs_before, procs_after, binary_before, binary_after, str(binary), None, outside, False, counters)
    if notes:
        record["notes"] = notes
    return record


# ----------------------------------------------------------------------------------------------------------------------
# native crate build (background)
# ----------------------------------------------------------------------------------------------------------------------

PREBUILT_NATIVE = HERE / "native-updater-check" / "target" / "release" / "native-updater-check"


class BackgroundBuild:
    """cargo build (or check) of the native crate in a thread. If the workflow already built the binary in the crate's own target
    directory (a step before this script), that binary is used and nothing is compiled twice."""

    def __init__(self, ctx: Ctx, mode: str, timeout_s: float):
        self.ctx, self.mode, self.timeout_s = ctx, mode, timeout_s
        self.crate = HERE / "native-updater-check"
        self.target = ctx.work / "native-target"
        self.prebuilt = PREBUILT_NATIVE.is_file()
        self.log_path = ctx.evidence / "build.log"
        self.rc: Optional[int] = None
        self.seconds: Optional[float] = None
        self.thread: Optional[threading.Thread] = None
        self.error: Optional[str] = None

    @property
    def binary(self) -> Path:
        return PREBUILT_NATIVE if self.prebuilt else self.target / "release" / "native-updater-check"

    def _run(self) -> None:
        began = time.time()
        argv = ["cargo", "check" if self.mode == "check" else "build"] + ([] if self.mode == "check" else ["--release"]) + ["--locked", "--manifest-path", str(self.crate / "Cargo.toml")]
        env = dict(os.environ, CARGO_TARGET_DIR=str(self.target), CARGO_TERM_COLOR="never", CARGO_NET_RETRY="5", CARGO_INCREMENTAL="0")
        try:
            with open(str(self.log_path), "wb") as handle:
                handle.write(("$ %s\n" % shlex.join(argv)).encode("utf-8"))
                handle.flush()
                proc = subprocess.Popen(argv, stdout=handle, stderr=subprocess.STDOUT, env=env)
                try:
                    self.rc = proc.wait(timeout=self.timeout_s)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    self.rc = 124
                    self.error = "build timed out after %ds" % self.timeout_s
        except OSError as error:
            self.rc, self.error = 127, "%s: %s" % (type(error).__name__, error)
        self.seconds = round(time.time() - began, 1)
        self.ctx.log.add("native crate %s finished: rc %s in %ss" % (self.mode, self.rc, self.seconds))

    def start(self) -> None:
        if self.prebuilt:
            self.rc, self.seconds = 0, 0.0
            self.ctx.log.add("native crate: using the binary the workflow built (%s)" % PREBUILT_NATIVE)
            return
        self.thread = threading.Thread(target=self._run, name="native-build", daemon=True)
        self.thread.start()

    def wait(self, timeout_s: float) -> bool:
        if self.thread is not None:
            self.thread.join(timeout=timeout_s)
            return not self.thread.is_alive()
        return True

    def status(self) -> dict:
        tail = ""
        try:
            tail = tail_text(self.log_path.read_text(encoding="utf-8", errors="replace"), 1500)
        except OSError:
            pass
        state = "running" if (self.thread is not None and self.thread.is_alive()) else ("ok" if self.rc == 0 else "failed")
        return {"mode": self.mode, "state": state, "rc": self.rc, "seconds": self.seconds, "error": self.error, "prebuilt_by_workflow": self.prebuilt,
                "log_tail": tail if state != "ok" else ""}


# ----------------------------------------------------------------------------------------------------------------------
# F4 replay harness (supporting evidence)
# ----------------------------------------------------------------------------------------------------------------------

def run_replay_harness(ctx: Ctx, native_binary: Optional[Path], released_binary: Optional[str]) -> dict:
    checker = HERE / "desktop_updater_check.py"
    if not checker.is_file():
        return {"ran": False, "reason": "desktop_updater_check.py is not in the tree"}
    out = ctx.evidence / "harness"
    out.mkdir(parents=True, exist_ok=True)
    argv = [sys.executable, str(checker), "run", "--repo", str(REPO_ROOT), "--evidence", str(out), "--label", OS_LABEL, "--tag", TAG]
    if native_binary is not None and native_binary.is_file():
        argv += ["--native-check", str(native_binary)]
    observation = HERE / "live-observation-flip-window.json"
    if observation.is_file():
        argv += ["--live-observation", str(observation)]
    if released_binary:
        argv += ["--released-binary", released_binary]
    done = ctx.run(argv, timeout=900, tail=1500)
    return {"ran": True, "rc": done.rc, "native_check_supplied": bool(native_binary and native_binary.is_file()), "output_tail": tail_text(done.out.strip(), 1500)}


# ----------------------------------------------------------------------------------------------------------------------
# the lab
# ----------------------------------------------------------------------------------------------------------------------

def load_config() -> Optional[dict]:
    path = HERE / "config.json"
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def partial_result(ctx: Ctx, reason: str) -> dict:
    return {
        "schema": SCHEMA_RESULT, "os": OS_LABEL, "runner": runner_info(), "app": {"tag": TAG, "asset": None, "asset_sha256": None, "release_digest": None, "digest_match": None, "version_reported": None},
        "plugin": {"version": "", "provenance": []},
        "tiers": {name: {"attempted": False, "status": "not_attempted", "result": "UNPROVED", "reason": reason, "trigger": None} for name in TIERS},
        "cases": [], "controls": [], "manifest404_error_text": None,
        "isolation": {"hosts_mapped": [], "ca_sha256": None, "live_block": False}, "verdict": "UNPROVED", "reasons": [reason],
        "interception_coverage": INTERCEPTION_COVERAGE, "expected_state": EXPECTED_STATE_NOTE, "started": now_utc(),
    }


def write_result(ctx: Ctx, result: dict) -> None:
    write_json(ctx.evidence / "result.json", result)


def run_lab(ctx: Ctx, tiers: Sequence[str], cases: Sequence[str], skip_native_build_wait: bool = False) -> dict:
    result = partial_result(ctx, "the run has not finished")
    write_result(ctx, result)
    config = load_config()
    want_released = "released_app" in tiers
    want_native = "native_check" in tiers
    asset_facts: dict = {}
    build: Optional[BackgroundBuild] = None
    isolation: Dict[str, Any] = {"hosts_mapped": [], "ca_sha256": None, "live_block": False, "coverage": INTERCEPTION_COVERAGE}
    result["isolation"] = isolation
    try:
        packages = list(RUNTIME_PACKAGES) + (list(BUILD_PACKAGES) if want_native else [])
        result["setup"] = {"apt": install_packages(ctx, packages, "runtime+build" if want_native else "runtime"), "userns": relax_userns_restriction(ctx)}
        if want_released:
            metadata = fetch_release_metadata(ctx, config)
            result["setup"]["release_metadata"] = {"source": metadata["source"], "notes": metadata["notes"]}
            asset_facts = download_and_extract(ctx, metadata["asset"])
            result["app"] = {"tag": TAG, "asset": asset_facts.get("asset"), "asset_sha256": asset_facts.get("asset_sha256"), "release_digest": asset_facts.get("release_digest"),
                             "digest_match": asset_facts.get("digest_match"), "version_reported": None, "binary_sha256": asset_facts.get("binary_sha256"),
                             "missing_libraries": asset_facts.get("missing_libraries")}
            if asset_facts.get("extracted"):
                try:
                    result["plugin"] = gather_provenance(ctx, Path(asset_facts["binary"]))
                except Exception as error:  # noqa: BLE001 - the plugin version stays unproven
                    ctx.log.add("provenance failed: %s: %s" % (type(error).__name__, error))
                    result["plugin"] = {"version": "", "provenance": ["provenance failed: %s: %s" % (type(error).__name__, error)], "proven": False}
            write_result(ctx, result)
        if not want_released:
            locked = lockfile_plugin_version()
            result["plugin"] = {"version": locked or "", "provenance": ["wave0-lab-desktop/native-updater-check/Cargo.lock pins tauri-plugin-updater %s (the released binary was not examined in this run)" % locked], "proven": False}
        if want_native:
            build = BackgroundBuild(ctx, "build", 1800)
            build.start()
        ca_state = setup_ca(ctx)
        isolation["ca_sha256"] = ca_state["ca_sha256"]
        isolation["tls_trust"] = "system CA bundle (update-ca-certificates) with SSL_CERT_FILE=%s" % SYSTEM_BUNDLE
        resources = (Path(asset_facts["root"]) / "usr" / "lib" / "ARC Node" / "resources") if asset_facts.get("root") else None
        block = apply_live_block(ctx, resources)
        isolation["live_block_detail"] = block
        selftest = https_selftest(ctx)
        isolation["selftest"] = selftest
        write_result(ctx, result)
        if not ca_state["trusted"] or not selftest.get("ok"):
            result["reasons"] = ["interception self-test failed: %s" % tail_text(json.dumps(selftest), 400)]

        controls: List[dict] = []
        all_cases: List[dict] = []
        # --- released app tier
        if want_released:
            tier = {"attempted": True, "trigger": "WebKitWebDriver (xvfb) + window.__TAURI_INTERNALS__.invoke('plugin:updater|check')", "status": "not_run", "result": "UNPROVED", "reason": ""}
            result["tiers"]["released_app"] = tier
            if not asset_facts.get("extracted"):
                tier.update(status="infeasible", reason=asset_facts.get("reason") or "the released app was not downloaded or extracted")
            elif not ca_state["trusted"] or not selftest.get("ok"):
                tier.update(status="infeasible", reason="the interception self-test failed, so no case could prove anything")
            else:
                clean_home = ctx.work / "sandbox" / "clean" / "home"
                tier["status"] = "ran"
                tier_cases: List[dict] = []
                for name in cases:
                    seed = clean_home if name == "cached-bait" else None
                    tier_cases.append(run_released_case(ctx, asset_facts, name, CASE_SCENARIO[name], seed))
                    write_result(ctx, dict(result, cases=all_cases + tier_cases))
                version = next((c["trigger_outcome"].get("current_version") for c in tier_cases if c.get("trigger_outcome", {}).get("current_version")), None)
                result["app"]["version_reported"] = version
                if "cached-bait" in cases:
                    control = run_released_case(ctx, asset_facts, "cached-bait", "bait-0.8.11", clean_home, control=True)
                    sent = [row for row in (read_jsonl(ctx.evidence / "requests-control-cached-bait.jsonl")[0] or []) if row.get("kind") == "request" and is_payload_row(row)]
                    controls.append({"name": "released-app-download-control", "purpose": "positive control: the plugin is told to download the bait update; the recorder must see the bundle request",
                                     "payload_requests_seen": [str(row.get("path")) for row in sent], "recorder_sensitive": bool(sent), "verdict": "PASS" if sent else "FAIL",
                                     "evidence_files": control.get("evidence_files", []), "trigger": control.get("trigger_outcome")})
                tier["result"] = tier_result(tier_cases, list(cases))
                if tier["result"] == "PASS" and controls and not controls[-1]["recorder_sensitive"]:
                    tier.update(result="UNPROVED", reason="the download control did not show a bundle request in the request log: the recorder's sensitivity is not proven")
                tier["reason"] = tier["reason"] or "; ".join(sorted({r for c in tier_cases for r in c.get("reasons", [])}))[:600]
                all_cases.extend(tier_cases)
        # --- native tier
        native_binary: Optional[Path] = None
        if want_native and build is not None:
            tier = {"attempted": True, "trigger": "native-updater-check (the real tauri-plugin-updater 2.10.1 check() in tauri's mock runtime)", "status": "not_run", "result": "UNPROVED", "reason": ""}
            result["tiers"]["native_check"] = tier
            if not skip_native_build_wait:
                build.wait(1800)
            status = build.status()
            result["native_build"] = status
            if not ca_state["trusted"] or not selftest.get("ok"):
                tier.update(status="infeasible", reason="the interception self-test failed, so no case could prove anything")
            elif status["state"] == "ok" and build.binary.is_file():
                native_binary = build.binary
                tier["status"] = "ran"
                tier_cases = []
                for name in cases:
                    tier_cases.append(run_native_case(ctx, native_binary, name, CASE_SCENARIO[name]))
                    write_result(ctx, dict(result, cases=all_cases + tier_cases))
                if "cached-bait" in cases:
                    control = run_native_case(ctx, native_binary, "cached-bait", "bait-0.8.11", control=True)
                    sent = [row for row in (read_jsonl(ctx.evidence / "requests-native-control-cached-bait.jsonl")[0] or []) if row.get("kind") == "request" and is_payload_row(row)]
                    controls.append({"name": "native-download-control", "purpose": "positive control: --control-download makes the real plugin fetch the bait bundle; the recorder must see it",
                                     "payload_requests_seen": [str(row.get("path")) for row in sent], "recorder_sensitive": bool(sent), "verdict": "PASS" if sent else "FAIL",
                                     "evidence_files": control.get("evidence_files", []), "trigger": control.get("trigger_outcome")})
                tier["result"] = tier_result(tier_cases, list(cases))
                if tier["result"] == "PASS" and controls and not controls[-1]["recorder_sensitive"]:
                    tier.update(result="UNPROVED", reason="the native download control did not show a bundle request: the recorder's sensitivity is not proven")
                tier["reason"] = tier["reason"] or "; ".join(sorted({r for c in tier_cases for r in c.get("reasons", [])}))[:600]
                all_cases.extend(tier_cases)
            else:
                tier.update(status="infeasible", reason="the native crate did not build (%s): %s" % (status["state"], status.get("error") or tail_text(status.get("log_tail", ""), 300)))
        result["cases"] = all_cases
        result["controls"] = controls
        isolation["hosts_mapped"] = list(ctx.state.get("hosts_mapped_lines") or [])
        isolation["hosts_mapped_names"] = list(ctx.state.get("hosts_mapped_names") or [])
        error_text = next((c["trigger_outcome"].get("text") for c in all_cases if c["scenario"] == "latest-404" and c.get("trigger_outcome", {}).get("outcome") == "release_not_found"), None)
        if error_text is not None:
            result["manifest404_error_text"] = error_text
            (ctx.evidence / "manifest404-error.txt").write_text(error_text + "\n", encoding="utf-8")
        counters_after = live_counters(ctx)
        listing = ctx.sudo(["iptables", "-S", "OUTPUT"], log=False)
        still = parse_iptables_rules(listing.out, ctx.state.get("live_ips") or [])
        isolation["live_block"] = bool(block["ok"] and listing.ok and all(still.values()))
        isolation["live_block_detail"]["counters_end"] = counters_after
        isolation["live_block_detail"]["rejected_packets_total"] = sum(counters_after.values())
        isolation["live_block_detail"]["note"] = "rejected_packets_total counts attempts the local firewall refused: they never left the runner"
        result["supporting"] = {"replay_harness": run_replay_harness(ctx, native_binary, asset_facts.get("binary"))}
        result["verdict"], result["reasons"] = overall_verdict(result)
    except Exception as error:  # noqa: BLE001 - never leave without evidence
        ctx.log.add("lab aborted: %s: %s" % (type(error).__name__, error))
        ctx.log.add(tail_text(traceback.format_exc(), 2000))
        result["verdict"] = "UNPROVED"
        result["reasons"] = ["the lab aborted: %s: %s" % (type(error).__name__, error)]
    finally:
        cleanup(ctx)
        result["finished"] = now_utc()
        write_json(ctx.evidence / "isolation.json", result.get("isolation", {}))
        write_result(ctx, result)
    return result


def cleanup(ctx: Ctx) -> None:
    """Best effort: unmap hosts, drop the iptables rules, untrust the CA. The runner is thrown away anyway."""
    with contextlib.suppress(Exception):
        ctx.sudo(["sed", "-i", "/%s/d" % BLACKHOLE_MARK, "/etc/hosts"], log=False)  # also matches the -github lines
    with contextlib.suppress(Exception):
        remove_live_block(ctx)
    with contextlib.suppress(Exception):
        if ctx.state.get("ca_trusted"):
            ctx.sudo(["rm", "-f", "/usr/local/share/ca-certificates/" + CA_FILE_NAME], log=False)
            ctx.sudo(["update-ca-certificates", "--fresh"], timeout=120, log=False)


# ----------------------------------------------------------------------------------------------------------------------
# the probe
# ----------------------------------------------------------------------------------------------------------------------

def run_probe(ctx: Ctx, build_check_minutes: float = 12.0) -> dict:
    """Fast capability probe: each step is recorded and a failing step never stops the next one."""
    probe: Dict[str, Any] = {"schema": SCHEMA_PROBE, "os": OS_LABEL, "started": now_utc(), "runner": runner_info(), "steps": {}}

    def step(name: str, fn: Callable[[], Any]) -> Any:
        began = time.time()
        try:
            value = fn()
            probe["steps"][name] = {"ok": True, "seconds": round(time.time() - began, 1), "detail": value}
            return value
        except Exception as error:  # noqa: BLE001
            probe["steps"][name] = {"ok": False, "seconds": round(time.time() - began, 1), "detail": "%s: %s" % (type(error).__name__, error)}
            ctx.log.add("probe step %s failed: %s" % (name, error))
            return None
        finally:
            write_json(ctx.evidence / "probe.json", probe)

    config = load_config()
    step("sudo", lambda: ctx.sudo(["true"]).ok)
    step("tools", lambda: {name: shutil.which(name) for name in ("openssl", "curl", "cargo", "rustc", "git", "dpkg-deb", "iptables", "update-ca-certificates", "gh")})
    build = BackgroundBuild(ctx, "check", build_check_minutes * 60)
    apt = step("apt_runtime", lambda: install_packages(ctx, list(RUNTIME_PACKAGES) + list(BUILD_PACKAGES), "runtime+build"))
    step("userns", lambda: relax_userns_restriction(ctx))
    step("webdriver_tools", lambda: {"WebKitWebDriver": shutil.which("WebKitWebDriver") or ([str(p) for p in Path("/usr/lib").glob("*/webkit2gtk-4.*/WebKitWebDriver")] or None),
                                     "xvfb-run": shutil.which("xvfb-run"), "dbus-run-session": shutil.which("dbus-run-session"), "inotifywait": shutil.which("inotifywait")})
    if apt is not None:
        step("native_crate_check_start", lambda: build.start() or "started: cargo check --locked (the crate has never been compiled)")
    asset_facts: dict = {}
    metadata = None
    began = time.time()
    try:
        metadata = fetch_release_metadata(ctx, config)
        probe["steps"]["release_metadata"] = {"ok": bool(metadata["live_ok"]), "seconds": round(time.time() - began, 1),
                                              "detail": {"source": metadata["source"], "asset": metadata["asset"]["name"], "release_digest": metadata["asset"]["release_digest"],
                                                         "notes": metadata["notes"]}}
    except Exception as error:  # noqa: BLE001
        probe["steps"]["release_metadata"] = {"ok": False, "seconds": round(time.time() - began, 1), "detail": "%s: %s" % (type(error).__name__, error)}
    write_json(ctx.evidence / "probe.json", probe)
    if metadata:
        facts = step("download_and_extract", lambda: download_and_extract(ctx, metadata["asset"]))
        asset_facts = facts or {}
        if asset_facts.get("extracted"):
            step("provenance", lambda: gather_provenance(ctx, Path(asset_facts["binary"])))
    step("hosts_writable", lambda: ctx.sudo(["sh", "-c", "echo '127.0.0.1 wave0-probe.invalid %s-probe' >> /etc/hosts; getent hosts wave0-probe.invalid; sed -i '/%s-probe/d' /etc/hosts" % (HOSTS_MARK, HOSTS_MARK)]).out.strip())
    step("port_443", lambda: ctx.sudo([sys.executable, "-c", "import socket; s=socket.socket(); s.bind(('127.0.0.1',443)); s.close(); print('bind ok')"]).out.strip())
    ca_state = step("ca", lambda: setup_ca(ctx))
    step("live_block", lambda: apply_live_block(ctx))
    selftest = step("https_selftest", lambda: https_selftest(ctx)) if ca_state and ca_state.get("trusted") else None
    if asset_facts.get("extracted") and selftest and selftest.get("ok"):
        step("released_app_clean_case", lambda: {k: v for k, v in run_released_case(ctx, asset_facts, "clean", "latest-404", None).items() if k in ("verdict", "reasons", "trigger_outcome", "criteria", "requests", "notes")})
    step("native_crate_check", lambda: (build.wait(build_check_minutes * 60), build.status())[1])
    cleanup(ctx)
    probe["finished"] = now_utc()
    write_json(ctx.evidence / "probe.json", probe)
    return probe


# ----------------------------------------------------------------------------------------------------------------------
# CLI
# ----------------------------------------------------------------------------------------------------------------------

def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    one = sub.add_parser("probe", help="fast capability probe; never fails the job")
    one.add_argument("--evidence", type=Path, required=True)
    one.add_argument("--work", type=Path)
    two = sub.add_parser("run", help="the full lab")
    two.add_argument("--evidence", type=Path, required=True)
    two.add_argument("--work", type=Path)
    two.add_argument("--tier", choices=("released_app", "native_check", "both"), default="both")
    two.add_argument("--cases", default=",".join(CASES))
    return parser.parse_args(list(argv) if argv is not None else None)


def on_runner() -> bool:
    """This script uses sudo, iptables, /etc/hosts and the system trust store: it only runs on a CI runner (GITHUB_ACTIONS=true)
    or when WAVE0_DESKTOP_ALLOW_LOCAL=1 is set deliberately (the unit tests set it and replace every command)."""
    return os.environ.get(GUARD_ENV) == "1" or (os.environ.get("GITHUB_ACTIONS") == "true" and sys.platform.startswith("linux"))


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parse_args(argv)
    if not on_runner():
        print("os_linux.py changes /etc/hosts, the trust store and the firewall: it runs on a GitHub-hosted ubuntu runner only "
              "(GITHUB_ACTIONS=true); refusing to run here", file=sys.stderr)
        return 2
    ctx = Ctx(args.evidence, args.work)
    try:
        if args.command == "probe":
            run_probe(ctx)
            return 0
        tiers = list(TIERS) if args.tier == "both" else [args.tier]
        names = [name.strip() for name in args.cases.split(",") if name.strip()]
        unknown = [name for name in names if name not in CASES]
        if unknown:
            write_result(ctx, partial_result(ctx, "unknown case name(s): %s" % unknown))
            print("unknown case name(s): %s (choose from %s)" % (unknown, ", ".join(CASES)), file=sys.stderr)
            return 2
        result = run_lab(ctx, tiers, names)
        print("verdict: %s" % result.get("verdict"))
        return 0
    except Exception as error:  # noqa: BLE001 - last resort: evidence of the crash, exit 0 so the summary job judges
        ctx.log.add("fatal: %s: %s" % (type(error).__name__, error))
        ctx.log.add(tail_text(traceback.format_exc(), 3000))
        if args.command == "run":
            failure = partial_result(ctx, "fatal error: %s: %s" % (type(error).__name__, error))
            write_result(ctx, failure)
        return 0


if __name__ == "__main__":
    sys.exit(main())
