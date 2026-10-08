#!/usr/bin/env python3
"""macOS job of the Wave 0 desktop isolation lab (THROWAWAY LAB FILE, never merged).

Question (ARC-83 audit item 4): with v0.7.12 as GitHub "Latest" (five launchers + SHA256SUMS, NO latest.json, v0.8.11 present
as bait) does the RELEASED v0.7.11 ARC Node desktop app request only the manifest URL, download no bundle/installer/.sig/
.tar.gz of any release, install nothing, launch no new app version and write no new files outside expected app/log state?
The exact text of the manifest-404 error is recorded.

Interception (runner only, torn down at the end): a per-run CA (lib/ca.py) is trusted in the System keychain, the GitHub names
point to 127.0.0.1 in /etc/hosts, and lib/mitm_server.py plays github.com on port 443 and records every request and every
refused TLS handshake. The updater plugin (tauri-plugin-updater 2.10.1, rustls-platform-verifier = Security.framework on
macOS) therefore talks to the recorder. The app's own banner (check_for_update) and ensure_binary use reqwest with BUNDLED
webpki roots: their handshake is refused by the client and shows up as kind "tls_failure"; they are NOT intercepted, and the
evidence says so. Every ARC address (the six live nodes) is blocked with pf for the whole run and the rules are verified.

Fixes after the first macOS CI run (37800515512):
  * ONE CA per job: the probe and run phases share it (RUNNER_TEMP/wave0-desktop-macos-shared): generated once, trusted once, removed
    once (the run phase removes it, or `cleanup`; `probe --keep-ca` is the default so the run phase finds it trusted). Every security(1)
    call is bounded; the admin trust-settings right is granted (`authorizationdb write ... allow`) BEFORE every add attempt, a timed
    out call is followed by killing the orphaned security process and any SecurityAgent dialog (killing sudo does not kill root's
    child), and an add gets three attempts of 60/45/45 s. A repeated add and a remove had hung for their full timeouts before.
  * The hosts file did not keep WebKit's network process away from rsms.me (it held ESTABLISHED TLS to Cloudflare addresses): the real
    addresses of rsms.me (system resolver before the mapping, `dig` any time) are added to the pf block next to the six ARC addresses,
    one labelled rule per address, verified the same way, refreshed before every case; a blocked attempt is information, an
    ESTABLISHED connection to any of them fails the case. isolation.json holds, for every mapped name, what dscacheutil and
    getaddrinfo answer after the mapping, to find out who ignores /etc/hosts.
  * The banner's unintercepted API call may answer without a tag (the UI then shows "vUNKNOWN"; most likely the unauthenticated rate
    limit of a shared runner address). Check for updates is clicked up to three more times, 20 s apart, the Updates card text is
    recorded after every click, and an Install button that is present BEFORE the check is never clicked. If it never appears the
    released-app tier is infeasible with the exact UI text; the plugin path is claimed reached through the released app ONLY when
    a manifest request arrived at the recorder after an Install click.

Two tiers, labelled honestly in result.json:

  released_app   the real v0.7.11 .dmg (digest-verified against the release API BEFORE use) is mounted, the .app copied into a
                 sandbox and launched; the UI is driven through System Events (needs the Accessibility permission).
                 FINDING FROM THE v0.7.11 SOURCE (Settings.tsx:20-26,40-50,207-212): the plugin's check() is called ONLY by the
                 "Install v... & relaunch" button, which is rendered ONLY when the banner's Rust command check_for_update
                 (commands.rs:934-957, reqwest + bundled webpki roots, https://api.github.com/.../releases/latest) returned a
                 tag different from 0.7.11. That call cannot be intercepted. Since 2026-10-08T14:18:32Z real GitHub says
                 Latest = v0.7.12, so the button appears if that single read-only GET is allowed to reach the real API.
                 REAL-BANNER MODE (default ON, switch --no-real-banner-api; approved by work-99 via the captain): ONLY
                 api.github.com is left out of the hosts mapping and out of the pf block; github.com, the asset hosts and
                 rsms.me (the webview loads https://rsms.me/inter/inter.css, index.html:14-15) stay on the recorder; the six ARC
                 addresses stay blocked. The pass-through is observed, not recorded: tcpdump (DNS + HTTPS flows with Darwin's
                 per-packet process names) and lsof snapshots of the app's processes; any other real destination fails the
                 case with the endpoint named. The content of the pass-through request is NOT recorded (it is not
                 intercepted). Then Settings > Check for updates > Install drives the real plugin check(), answered by the
                 recorder (scenario latest-404), and the UI error text is recorded verbatim.
                 With the switch OFF the tier is a labelled NEGATIVE CONTROL (the banner call cannot succeed, the Install
                 button never appears, the plugin path is not reached). The bait scenario is NOT driven through the released
                 app: its Install handler calls downloadAndInstall() right after check() when an update exists
                 (Settings.tsx:40-46), so the app itself would download; bait runs at plugin level only (native_check).
                 Nothing is faked.
  native_check   the REAL tauri-plugin-updater 2.10.1 `check()` (wave0-lab-desktop/native-updater-check, built on the runner
                 with --locked) against the same recorder and CA, scenario latest-404 (case "clean") and bait-0.8.11 (case
                 "cached-bait", plus a --control-download POSITIVE CONTROL proving the recorder sees a bundle request). This
                 is the macOS evidence for the plugin path: same plugin version, same OS trust-store verifier.

Subcommands (all refuse to run unless GITHUB_ACTIONS=true: they change /etc/hosts, the keychain and pf and drive the GUI):
  cleanup --evidence DIR   take the per-job CA's trust away once (bounded, never fails); for an `if: always()` step at the end of a job
  probe --evidence DIR [--arch arm64|x86_64] [--no-build] [--build-wait-min 15] [--budget-min 25] [--no-real-banner-api]
        what this runner can do (hosts, port 443, trust store, release download, Accessibility three ways, the app window and its AX
        tree, the whole Settings > Check for updates > Install chain once); writes probe.json, accessibility.json, feasibility.txt
        (the two-line answer, also printed to the log before the optional wait for the background cargo build); never fails the job.
  run --evidence DIR [--arch ...] [--tier released_app|native_check|both] [--cases clean,cached-bait] [--no-real-banner-api]
        [--native-real-home]
        the isolation cases; writes result.json and the per-case evidence; exit 0 whenever evidence was produced.
Released-app cases: both use scenario latest-404 (the world after the flip) in ONE shared sandbox HOME seeded as a finished onboarding
leaves it (store.json, autoStart false); the second case therefore starts from the state the first one left behind.

Evidence files in DIR: result.json, probe.json, accessibility.json, feasibility.txt, steps.log (every command, exit code and
output tail; live addresses and tokens masked; never a private key), isolation.json, provenance.json, ca.crt, ca.sha256,
manifest404-error.txt, hosts.before.txt, hosts.mapped.txt, cargo-build.log, and per case and tier (<case>-<tier>): requests-*.jsonl,
writes-*.jsonl, fs-*-before.json, fs-*-after.json, procs-*-before.txt, procs-*-after.txt, procs-*-seen.txt, and for the released app
ax-*.json (AX trees), ui-*.json (the steps and what the Updates card said), network-*.json (what the app's processes talked to: tcpdump
flows, DNS names, lsof endpoints, violations; the raw capture stays on the runner), app-*.log and screenshot-*.png.

The CA private key and the server key never leave the runner: they stay under <runner temp>/.../ca/private and only ca.crt and
ca.sha256 are copied into DIR.

UNVERIFIED ON CI (nothing below could be run on the author's Mac; the first CI run is the test):
  * that `sudo -n security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain` is non-interactive on the
    hosted image (macOS 15 may demand a GUI authorization; on that error the script grants the admin trust-settings right with
    `security authorizationdb write com.apple.trust-settings.admin allow` and retries once); the probe records the exact outcome;
  * the output formats this script parses without ever having seen them: `tcpdump -k NP` packet lines ("(proc NAME:PID)"),
    `lsof -F pcfnT`, `pfctl -sr`, `dscacheutil -q host`, the JXA AX tree of a WKWebView;
  * that `osascript` multi-line AppleScript given as one -e per line behaves as the man page says and that a JXA file run with
    `osascript -l JavaScript FILE` prints the return value of run();
  * that Security.framework accepts the one-day ECDSA P-256 leaf for github.com and the Rust plugin's verifier agrees
    (`security verify-cert` is run first, the native-check tier is the real test);
  * that /etc/hosts entries are honoured by getaddrinfo for the app and the native binary after dscacheutil -flushcache and
    killall -HUP mDNSResponder (both IPv4 and ::1 are mapped);
  * that `hdiutil attach -nobrowse -readonly -noverify -mountpoint` works on the hosted image and the dmg holds "ARC Node.app";
  * that launching Contents/MacOS/<CFBundleExecutable> directly from the runner session shows a window (the runner must be in
    an Aqua session) and that the app honours HOME for its data directory (store.rs resolves it through Tauri's path
    resolver; WebKit data goes to the real ~/Library, so both are watched);
  * the Accessibility/Automation grants of the hosted image (TCC) for osascript and the process chain, and that the JXA tree
    walk of a WKWebView window returns AXButton elements with usable titles/descriptions;
  * that `pfctl -f` with only block rules keeps the runner connected (the Stage A jobs did exactly this);
  * that `cargo build --release --locked` of native-updater-check finishes inside 25 minutes on macos-15 and macos-15-intel;
  * that Security.framework evaluates trust normally for a process whose HOME is a sandbox directory (the native checker runs with
    HOME=<sandbox> so stray writes are visible; if it fails with a trust or keychain error, rerun with --native-real-home);
  * that `tcpdump -i pktap,all -k NP` is available (the script falls back to `-i any`, then `-i en0`, and says which one ran; without
    process names the lsof records still attribute sockets to processes);
  * that the Updates card shows the plugin error as text the AX tree exposes ("Update failed: ...", data-testid update-error) and that
    the banner's react-query result renders the Install button within about 30 s of the click;
  * that `authorizationdb write com.apple.trust-settings.admin allow` really prevents the dialog that hung the second add/remove, and that
    killing `security`/`SecurityAgent` after a timeout leaves the trust store usable;
  * which resolver path let WebKit reach rsms.me despite the hosts line (HTTPS/SVCB hints, a cached answer, or the mapping not yet
    in effect for that process): the pf block does not depend on the answer, the resolution views in isolation.json should tell;
  * that `dig` (BIND tools) is present on the runner image (without it the system resolver's answers before the mapping are used);
  * that the mitm_server ready file appears within 20 s after `sudo -n python3 lib/mitm_server.py ...`.
"""
from __future__ import annotations

import argparse
import atexit
import contextlib
import hashlib
import json
import os
import platform
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any, Dict, Iterable, Iterator, List, Optional, Sequence, Tuple

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
LIB = HERE / "lib"
if str(LIB) not in sys.path:
    sys.path.insert(0, str(LIB))

try:  # the shared core of the desktop lab (this branch ships both)
    import ca as ca_lib  # type: ignore
    import fswatch  # type: ignore
except ImportError:  # pragma: no cover - only if the lib directory is missing
    ca_lib = None  # type: ignore
    fswatch = None  # type: ignore

SCHEMA_RESULT = "arc.legacy-bridge.wave0-lab.desktop-os-result.v1"
SCHEMA_PROBE = "arc.legacy-bridge.wave0-lab.desktop-macos-probe.v1"
REPO = "FerrumVir/arc-chain"
APP_TAG = "v0.7.11"
APP_VERSION = "0.7.11"
APP_PRODUCT = "ARC Node"
APP_IDENTIFIER = "network.arc.desktop"
PLUGIN_VERSION = "2.10.1"
MANIFEST_HOST = "github.com"
MANIFEST_PATH = "/FerrumVir/arc-chain/releases/latest/download/latest.json"
MANIFEST_URL = "https://" + MANIFEST_HOST + MANIFEST_PATH
# plugins.updater.pubkey of tauri.conf.json at v0.7.11 (public, base64 of the minisign public key)
UPDATER_PUBKEY = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDlBOTcwQ0FBQ0U1NjQ3M0IKUldRN1IxYk9xZ3lYbWcrbkhkWnZlc0tmWW1uTTlhcDljLzF4cUZtUUVibTRNa2V4TjBoNHJqY2EK"
GITHUB_HOSTS = (
    "github.com",
    "api.github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "github-releases.githubusercontent.com",
    "codeload.github.com",
)
# the app's webview loads https://rsms.me/inter/inter.css (desktop/index.html:14-15, CSP style-src/font-src): keep it off the Internet
EXTRA_INTERCEPT_HOSTS = ("rsms.me",)
INTERCEPT_HOSTS = GITHUB_HOSTS + EXTRA_INTERCEPT_HOSTS
BANNER_API_HOST = "api.github.com"
BANNER_API_PATH = "/repos/FerrumVir/arc-chain/releases/latest"
APP_SCENARIO = "latest-404"  # both released-app cases: the world after the flip (Latest = v0.7.12, no latest.json)
REAL_BANNER_SCOPE = (
    "Only one kind of request leaves the sandbox for the real Internet: the banner's unauthenticated read-only GET (one per click on Check for updates) "
    "https://api.github.com/repos/FerrumVir/arc-chain/releases/latest (Settings.tsx:20-26 queryFn api.checkForUpdate; commands.rs:934-957 "
    "check_for_update: reqwest with bundled webpki roots, no Authorization header). Approved by work-99, relayed by the captain, 2026-10-08. "
    "github.com and every other GitHub/asset host stay mapped to the recorder, the six ARC node addresses stay blocked by pf, and any other "
    "real destination fails the case. The CONTENT of that pass-through request was NOT recorded by this lab (it is not intercepted); its answer "
    "is independently checkable: real Latest = v0.7.12 since 2026-10-08T14:18:32Z."
)
UPDATE_ERROR_RE = re.compile(r"Update failed:\s*(.+)", re.DOTALL)
RELEASE_NOT_FOUND_TEXT = "Could not fetch a valid release JSON from the remote"
APP_NAME_HINTS = ("ARC Node", "arc-desktop")
SCENARIO_FOR_CASE = {"clean": "latest-404", "cached-bait": "bait-0.8.11"}
ALL_CASES = ("clean", "cached-bait")
ALL_TIERS = ("released_app", "native_check")
CRITERIA = ("only_manifest_url", "no_bundle_download", "no_install", "no_new_app_launch", "no_new_files")
MANIFEST_ROLES = ("manifest", "manifest_redirect")
HOSTS_BEGIN = "# >>> wave0-desktop-lab begin (throwaway CI runner, removed at the end of the job)"
HOSTS_END = "# <<< wave0-desktop-lab end"
BLACKHOLE_NAMES = ("arc.ai", "www.arc.ai")
LIVE_IP_WORKFLOW = ".github/workflows/legacy-bridge.yml"
LIVE_IP_HARNESS = "tests/legacy-bridge/headless-v07-acceptance.sh"
SETTINGS_PATTERNS = (r"^\s*Settings\s*$",)
CHECK_PATTERNS = (r"Check for updates",)
INSTALL_PATTERNS = (r"Install\s+v?\d",)
AX_CLICK_ROLES = ("AXButton", "AXLink", "AXRadioButton", "AXMenuItem", "AXTab")
AX_ANY_ROLES = AX_CLICK_ROLES + ("AXGroup", "AXStaticText")
# an update artifact or installer by its name: used on every NEW path of a case, expected prefix or not
ARTIFACT_NAME_RE = re.compile(
    r"(?i)(\.(dmg|pkg|mpkg|tar\.gz|tgz|zip|sig|download|partial|part|crdownload|msi|exe|deb|rpm|appimage)$"
    r"|(^|[/\\])latest\.json$|(^|[/\\])(shipit|sparkle|squirrel|updater?)[-_.a-z0-9]*$|[-_.]update[-_.a-z0-9]*$)"
)
PAYLOAD_PATH_RE = re.compile(r"(?i)(\.(sig|tar\.gz|tgz|gz|zip|exe|msi|dmg|deb|rpm|appimage|pkg)$)")
INSTALLER_PROC_RE = re.compile(r"(?i)(^|/)(installer|hdiutil|ditto|unzip|gtar|bsdtar|tar|pkgutil|softwareupdated|installd|shipit|sparkle|squirrel|autoupdate|updater)(\s|$)")
TEMP_NAME_GLOBS = ("*network.arc.desktop*", "*arc-desktop*", "*ARC Node*", "com.apple.WebKit*", "WebKit*", "*.sb-*", "CFNetworkDownload*")
HELPER_COMM_HINTS = ("com.apple.WebKit.", "WebKit.Networking", "WebKit.WebContent", "WebKit.GPU")


# ------------------------------------------------------------------------------------------------------------------
# small pure helpers
# ------------------------------------------------------------------------------------------------------------------

def now_iso() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with Path(path).open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path: Path, value: Any) -> None:
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True, default=str) + "\n", encoding="utf-8")


def read_jsonl(path: Path) -> Optional[List[Dict[str, Any]]]:
    """The records of a JSON-lines file; None when the file does not exist (a missing log is unproved, an empty one is not)."""
    try:
        text = Path(path).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    rows: List[Dict[str, Any]] = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if isinstance(item, dict):
            rows.append(item)
    return rows


def mask_text(text: str, live_ips: Sequence[str] = (), secrets: Sequence[str] = ()) -> str:
    """Hide the live node addresses and any token in text that goes into the evidence."""
    out = text or ""
    for index, address in enumerate(live_ips, 1):
        if address:
            out = out.replace(address, "<live-ip-%d>" % index)
    for secret in secrets:
        if secret and len(secret) >= 8:
            out = out.replace(secret, "<token>")
    return re.sub(r"\b(?:ghs|ghp|gho|ghu|ghr|github_pat)_[A-Za-z0-9_]{16,}\b", "<token>", out)


def tail_lines(text: str, count: int) -> str:
    lines = (text or "").splitlines()
    if len(lines) <= count:
        return "\n".join(lines)
    return "\n".join(["... (%d earlier lines omitted)" % (len(lines) - count)] + lines[-count:])


def parse_live_ips(workflow_text: str, harness_text: str) -> List[str]:
    """The live-network addresses, read from the repository's own CI definition and cross-checked against its harness."""
    found = re.findall(r"^\s*LIVE_NETWORK_IPS:\s*(.+?)\s*$", workflow_text, flags=re.MULTILINE)
    if len(found) != 1:
        raise ValueError("expected exactly one LIVE_NETWORK_IPS line, found %d" % len(found))
    from_workflow = found[0].split()
    arrays = re.findall(r"^live_ips=\((.*?)\)\s*$", harness_text, flags=re.MULTILINE)
    if len(arrays) != 1:
        raise ValueError("expected exactly one live_ips array, found %d" % len(arrays))
    if from_workflow != arrays[0].split():
        raise ValueError("LIVE_NETWORK_IPS differs from the harness live_ips array")
    ipv4 = re.compile(r"^(?:(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])$")
    if not from_workflow or len(set(from_workflow)) != len(from_workflow) or not all(ipv4.match(item) for item in from_workflow):
        raise ValueError("the live address list is empty, has duplicates or holds a non-IPv4 entry")
    return from_workflow


def load_live_ips(root: Path = ROOT) -> List[str]:
    return parse_live_ips(
        (root / LIVE_IP_WORKFLOW).read_text(encoding="utf-8"),
        (root / LIVE_IP_HARNESS).read_text(encoding="utf-8"),
    )


def canonical_ip(address: str) -> str:
    """The textual form pf prints (compressed, lower case); anything that is not an address is returned unchanged."""
    import ipaddress
    try:
        return ipaddress.ip_address(address.strip().strip("[]")).compressed
    except ValueError:
        return address


def pf_conf_text(live_ips: Sequence[str], extra_ips: Sequence[str] = ()) -> str:
    """pf rules that drop every outbound packet to a live node (labels wave0-live-N) and to the extra addresses (wave0-extra-N, the
    second line of defence for a name the hosts file cannot hold back): one rule per address. Each rule carries its own label because
    pf's ruleset optimizer otherwise merges identical rules that differ only in the address into ONE rule on an anonymous table
    (`block drop out quick inet from any to <__automatic_xxx_0>`), which the listing no longer names address by address (the first
    CI run of this lab could not verify the block for that reason and, failing closed, launched nothing)."""
    lines = ['block drop out quick to %s label "wave0-live-%d"\n' % (canonical_ip(address), index) for index, address in enumerate(live_ips, 1)]
    lines += ['block drop out quick to %s label "wave0-extra-%d"\n' % (canonical_ip(address), index) for index, address in enumerate(extra_ips, 1)]
    return "".join(lines)


def automatic_tables(rules_text: str) -> List[str]:
    """Names of the anonymous tables a `pfctl -sr` listing refers to (`<__automatic_227272d_0>`)."""
    return sorted(set(re.findall(r"<(__automatic_[0-9A-Za-z]+_[0-9]+)>", rules_text)))


def addresses_in_tables(table_outputs: Sequence[str], live_ips: Sequence[str]) -> int:
    """How many of the given addresses (IPv4 or IPv6) appear in the `pfctl -t NAME -T show` outputs of the anonymous tables."""
    seen = set()
    for text in table_outputs:
        for token in text.split():
            seen.add(canonical_ip(token))
    return sum(1 for address in live_ips if canonical_ip(address) in seen)


def count_pf_block_rules(rules_text: str, live_ips: Sequence[str]) -> int:
    """How many of the given addresses appear in a `pfctl -sr` listing as block rules."""
    count = 0
    for address in live_ips:
        pattern = re.compile(r"block\s+drop\s+out\s+quick.*\b" + re.escape(canonical_ip(address)) + r"(?![0-9A-Fa-f:.])")
        if any(pattern.search(line) for line in rules_text.splitlines()):
            count += 1
    return count


def parse_dig_addresses(text: str) -> List[str]:
    """Addresses (A and AAAA answers) in `dig +short` output; CNAME targets and other lines are skipped."""
    import ipaddress
    found: List[str] = []
    for line in (text or "").splitlines():
        token = line.strip().split(" ")[0]
        try:
            address = ipaddress.ip_address(token)
        except ValueError:
            continue
        if address.is_loopback or address.is_private or address.is_unspecified or address.is_link_local:
            continue
        if address.compressed not in found:
            found.append(address.compressed)
    return found


def hosts_block(hostnames: Sequence[str], blackholes: Sequence[str] = BLACKHOLE_NAMES) -> str:
    """The text appended to /etc/hosts: the GitHub names to loopback (IPv4 and IPv6), ARC names to nowhere."""
    lines = [HOSTS_BEGIN]
    for name in hostnames:
        lines.append("127.0.0.1 %s" % name)
        lines.append("::1 %s" % name)
    for name in blackholes:
        lines.append("0.0.0.0 %s" % name)
    lines.append(HOSTS_END)
    return "\n".join(lines) + "\n"


def strip_hosts_block(text: str) -> str:
    out: List[str] = []
    skipping = False
    for line in text.splitlines():
        if line.strip() == HOSTS_BEGIN:
            skipping = True
            continue
        if skipping and line.strip() == HOSTS_END:
            skipping = False
            continue
        if not skipping:
            out.append(line)
    return "\n".join(out) + ("\n" if out else "")


class AssetError(RuntimeError):
    pass


def release_digest_hex(value: Any) -> str:
    text = str(value or "").strip().lower()
    return text[len("sha256:"):] if text.startswith("sha256:") else text


def select_mac_assets(release: Dict[str, Any], machine: str) -> Dict[str, Any]:
    """The macOS desktop assets of the v0.7.11 release for this runner's architecture (one build per architecture)."""
    arch = "aarch64" if machine.lower() in ("arm64", "aarch64") else "x64"
    dmg_re = re.compile(r"^ARC\.Node_[0-9]+\.[0-9]+\.[0-9]+_%s\.dmg$" % arch)
    tar_re = re.compile(r"^ARC\.Node_%s\.app\.tar\.gz$" % arch)
    dmg = tar = None
    for asset in release.get("assets") or []:
        name = str(asset.get("name", ""))
        if dmg_re.match(name):
            dmg = asset
        elif tar_re.match(name):
            tar = asset
    if dmg is None and tar is None:
        raise AssetError("the release has no macOS %s desktop asset (.dmg or .app.tar.gz)" % arch)
    return {"arch": arch, "dmg": dmg, "tar": tar}


def parse_json_tail(text: str) -> Optional[Dict[str, Any]]:
    """The last JSON object printed on its own line (osascript and the native checker may add warnings around it)."""
    for line in reversed((text or "").splitlines()):
        line = line.strip()
        if line.startswith("{") and line.endswith("}"):
            try:
                value = json.loads(line)
            except ValueError:
                continue
            if isinstance(value, dict):
                return value
    return None


def arch_matches(requested: Optional[str], machine: str) -> Optional[bool]:
    """--arch is the architecture the workflow job expects; the runner decides what really runs (None = nothing was requested)."""
    if not requested:
        return None
    wanted = {"arm64": "arm64", "aarch64": "arm64", "x86_64": "x86_64", "x64": "x86_64", "amd64": "x86_64"}.get(requested.lower())
    actual = {"arm64": "arm64", "aarch64": "arm64", "x86_64": "x86_64", "amd64": "x86_64"}.get(machine.lower())
    return wanted is not None and wanted == actual


def intercept_hosts(real_banner: bool = True) -> Tuple[str, ...]:
    """The names pointed at the recorder. In real-banner mode api.github.com is the one name left to the real DNS."""
    return tuple(name for name in INTERCEPT_HOSTS if not (real_banner and name == BANNER_API_HOST))


def parse_ps(text: str) -> List[Dict[str, Any]]:
    """`ps -axo pid=,ppid=,comm=` rows."""
    rows = []
    for line in (text or "").splitlines():
        match = re.match(r"^\s*(\d+)\s+(\d+)\s+(.*\S)\s*$", line)
        if match:
            rows.append({"pid": int(match.group(1)), "ppid": int(match.group(2)), "comm": match.group(3)})
    return rows


def descendants(rows: Sequence[Dict[str, Any]], root_pid: int) -> List[Dict[str, Any]]:
    by_parent: Dict[int, List[Dict[str, Any]]] = {}
    for row in rows:
        by_parent.setdefault(row["ppid"], []).append(row)
    found: List[Dict[str, Any]] = []
    stack = [root_pid]
    seen = {root_pid}
    while stack:
        for child in by_parent.get(stack.pop(), []):
            if child["pid"] not in seen:
                seen.add(child["pid"])
                found.append(child)
                stack.append(child["pid"])
    return found


# ------------------------------------------------------------------------------------------------------------------
# what left the sandbox: tcpdump flows and lsof endpoints (pure parsers and the verdict on them)
# ------------------------------------------------------------------------------------------------------------------

TCPDUMP_PACKET = re.compile(r"^(?P<t>\d+\.\d+)\s+(?P<head>.*?)\bIP6?\s+(?P<src>\S+)\s+>\s+(?P<dst>\S+?):(?:\s+|$)(?P<tail>.*)$")
TCPDUMP_PROC = re.compile(r"\bproc\s+(?P<name>[^,):]+?):(?P<pid>\d+)")
TCPDUMP_LENGTH = re.compile(r"\blength\s+(\d+)")
DNS_QUERY = re.compile(r"^(?P<id>\d+)[+*\-%|&]*\s+(?:\[[^\]]*\]\s+)?(?:\w+\s+)?(?:A|AAAA|HTTPS|SVCB)\?\s+(?P<name>[A-Za-z0-9._-]+?)\.?\s+\(\d+\)")
DNS_ANSWER_HEAD = re.compile(r"^(?P<id>\d+)[*|-]*\s+(?P<counts>\d+/\d+/\d+)\s*(?P<records>.*)$")
SERVER_PORTS = (53, 80, 443)


def split_endpoint(text: str) -> Tuple[str, Optional[int]]:
    """`140.82.112.5.443` or `2606:50c0::154.443` (tcpdump's address.port) -> (address, port)."""
    head, _, tail = (text or "").rpartition(".")
    if head and tail.isdigit():
        return head, int(tail)
    return text, None


def is_loopback(address: str) -> bool:
    return address in ("", "::1", "::", "0.0.0.0", "*") or address.startswith("127.") or address.lower().startswith("fe80:")


def parse_tcpdump_text(text: str) -> List[Dict[str, Any]]:
    """Packets of `tcpdump -n -tt -l [-k NP]` text output (lines that are not packets are ignored)."""
    packets: List[Dict[str, Any]] = []
    queries: Dict[str, str] = {}
    for line in (text or "").splitlines():
        match = TCPDUMP_PACKET.match(line.strip())
        if not match:
            continue
        src_ip, src_port = split_endpoint(match.group("src"))
        dst_ip, dst_port = split_endpoint(match.group("dst"))
        tail = match.group("tail")
        proc = TCPDUMP_PROC.search(match.group("head") + " " + tail)
        length = TCPDUMP_LENGTH.search(tail)
        packet: Dict[str, Any] = {
            "t": float(match.group("t")), "src_ip": src_ip, "src_port": src_port, "dst_ip": dst_ip, "dst_port": dst_port,
            "length": int(length.group(1)) if length else 0, "proc": proc.group("name").strip() if proc else None,
            "pid": int(proc.group("pid")) if proc else None, "dns_query": None, "dns_answers": [],
        }
        if dst_port == 53:
            query = DNS_QUERY.match(tail)
            if query:
                packet["dns_query"] = query.group("name")
                queries[query.group("id")] = query.group("name")
                if not packet["length"]:
                    size = re.search(r"\((\d+)\)\s*$", tail)
                    packet["length"] = int(size.group(1)) if size else 0
        elif src_port == 53:
            answer = DNS_ANSWER_HEAD.match(tail)
            if answer:
                packet["dns_query"] = queries.get(answer.group("id"))
                packet["dns_answers"] = re.findall(r"\b(?:A|AAAA)\s+([0-9A-Fa-f:.]+?)[,\s]", answer.group("records") + " ")
                if not packet["length"]:
                    size = re.search(r"\((\d+)\)\s*$", tail)
                    packet["length"] = int(size.group(1)) if size else 0
        packets.append(packet)
    return packets


def summarize_flows(packets: Sequence[Dict[str, Any]], api_ips: Sequence[str] = (), live_ips: Sequence[str] = ()) -> Dict[str, Any]:
    """Per remote (address, port): packets and bytes each way, the processes named by Darwin's packet metadata, a label."""
    flows: Dict[Tuple[str, int], Dict[str, Any]] = {}
    dns_names: Dict[str, List[str]] = {}
    for packet in packets:
        if packet["dst_port"] in SERVER_PORTS:
            remote, port, direction = packet["dst_ip"], packet["dst_port"], "out"
        elif packet["src_port"] in SERVER_PORTS:
            remote, port, direction = packet["src_ip"], packet["src_port"], "in"
        else:
            continue
        flow = flows.setdefault((remote, port), {
            "remote_ip": remote, "remote_port": port, "procs": set(), "packets_out": 0, "packets_in": 0, "bytes_out": 0, "bytes_in": 0,
            "first": packet["t"], "last": packet["t"],
        })
        flow["packets_" + direction] += 1
        flow["bytes_" + direction] += packet["length"]
        flow["first"], flow["last"] = min(flow["first"], packet["t"]), max(flow["last"], packet["t"])
        if packet["proc"]:
            flow["procs"].add("%s:%s" % (packet["proc"], packet["pid"]))
        if packet["dns_query"]:
            names = dns_names.setdefault(packet["dns_query"], [])
            for answer in packet["dns_answers"]:
                if answer not in names:
                    names.append(answer)
    out = []
    for (remote, port), flow in sorted(flows.items(), key=lambda item: item[1]["first"]):
        if port == 53:
            label = "dns"
        elif is_loopback(remote):
            label = "loopback"
        elif remote in live_ips:
            label = "live_node"
        elif remote in api_ips and port == 443:
            label = "banner_api"
        else:
            label = "other"
        out.append(dict(flow, procs=sorted(flow["procs"]), label=label))
    return {"packets": len(packets), "flows": out, "dns_names": {name: sorted(ips) for name, ips in sorted(dns_names.items())}}


LSOF_REMOTE = re.compile(r"->(?P<remote>\[?[0-9A-Fa-f:.]+\]?):(?P<port>\d+)")


def parse_lsof_f(text: str) -> List[Dict[str, Any]]:
    """Network endpoints from `lsof -nP -i -F pcfnT` (process id, command, file descriptor, name, TCP state)."""
    rows: List[Dict[str, Any]] = []
    pid: Optional[int] = None
    command: Optional[str] = None
    current: Optional[Dict[str, Any]] = None
    for line in (text or "").splitlines():
        if not line:
            continue
        tag, value = line[0], line[1:]
        if tag == "p":
            pid = int(value) if value.isdigit() else None
            command, current = None, None
        elif tag == "c":
            command = value
        elif tag == "f":
            current = {"pid": pid, "command": command, "name": None, "state": None}
            rows.append(current)
        elif tag == "n" and current is not None:
            current["name"] = value
        elif tag == "T" and current is not None and value.startswith("ST="):
            current["state"] = value[3:]
    endpoints = []
    for row in rows:
        match = LSOF_REMOTE.search(row.get("name") or "")
        if not match:
            continue
        endpoints.append({"pid": row["pid"], "command": row["command"], "remote_ip": match.group("remote").strip("[]"),
                          "remote_port": int(match.group("port")), "state": row.get("state")})
    return endpoints


def related_pids(seen: Sequence[Dict[str, Any]], app_pid: Optional[int]) -> List[int]:
    """The app, everything it started, and the WebKit XPC helpers that appeared while it ran (launchd starts those)."""
    pids = set()
    if app_pid is not None:
        pids.add(app_pid)
        pids.update(row["pid"] for row in descendants(seen, app_pid))
    for row in seen:
        comm = str(row.get("comm"))
        if "com.apple.WebKit." in comm or "ARC Node.app/" in comm:
            pids.add(row["pid"])
    return sorted(pids)


def network_report(endpoints: Sequence[Dict[str, Any]], flows: Optional[Dict[str, Any]], pids: Sequence[int], api_ips: Sequence[str],
                   live_ips: Sequence[str], real_banner: bool) -> Dict[str, Any]:
    """Judge what the app's processes talked to. Allowed: loopback, and (real-banner mode) the api.github.com addresses on 443.
    A blocked attempt at a live node is information; an ESTABLISHED one means the block failed. Anything else is a violation."""
    related = set(pids)
    allowed: List[Dict[str, Any]] = []
    blocked: List[Dict[str, Any]] = []
    violations: List[Dict[str, Any]] = []
    background = 0
    merged: Dict[Tuple[Any, ...], Dict[str, Any]] = {}
    for endpoint in endpoints:
        key = (endpoint["pid"], endpoint["command"], endpoint["remote_ip"], endpoint["remote_port"])
        item = merged.setdefault(key, dict(endpoint, states=[]))
        if endpoint.get("state") and endpoint["state"] not in item["states"]:
            item["states"].append(endpoint["state"])
    for item in merged.values():
        item.pop("state", None)
        address, port = item["remote_ip"], item["remote_port"]
        if is_loopback(address):
            continue
        if item["pid"] not in related:
            background += 1
            continue
        if real_banner and address in api_ips and port == 443:
            allowed.append(item)
        elif address in live_ips:
            (violations if "ESTABLISHED" in item["states"] else blocked).append(item)
        else:
            violations.append(item)
    capture_violations: List[Dict[str, Any]] = []
    attributed = False
    if flows:
        names = [name for name in APP_NAME_HINTS] + ["com.apple.WebKit"]
        for flow in flows.get("flows", []):
            if flow["procs"]:
                attributed = True
            mine = any(any(hint in proc for hint in names) for proc in flow["procs"])
            if flow["label"] == "other" and mine:
                capture_violations.append(flow)
            elif flow["label"] == "live_node" and mine and flow["packets_in"] > 0:
                capture_violations.append(flow)
    return {
        "recorded": bool(merged) or bool(flows and flows.get("packets")),
        "real_banner": real_banner, "allowed_banner_endpoints": allowed, "blocked_live_node_attempts": blocked,
        "violations": violations + capture_violations, "background_endpoints": background, "capture_attributed": attributed,
    }


def extract_update_error(texts: Sequence[str]) -> Optional[str]:
    """The message after "Update failed: " in the Updates card (Settings.tsx:160-168), verbatim."""
    for text in texts:
        match = UPDATE_ERROR_RE.search(text or "")
        if match:
            return match.group(1).strip()
    return None


def infer_error_kind(message: Optional[str]) -> Optional[str]:
    """The updater error variant behind a UI message (the Display text of tauri-plugin-updater 2.10.1's Error)."""
    if message and RELEASE_NOT_FOUND_TEXT in message:
        return "ReleaseNotFound"
    return None


# ------------------------------------------------------------------------------------------------------------------
# osascript / System Events helpers (pure builders and parsers)
# ------------------------------------------------------------------------------------------------------------------

AS_PROCESS_NAMES = 'tell application "System Events" to get name of every process'
AS_UI_ENABLED = 'tell application "System Events" to get UI elements enabled'
AS_FINDER_CLICK = "\n".join([
    'tell application "Finder" to set idsBefore to id of every Finder window',
    'tell application "System Events" to tell process "Finder" to click menu item "New Finder Window" of menu "File" of menu bar 1',
    "delay 1",
    'tell application "Finder"',
    "  set idsAfter to id of every Finder window",
    "  repeat with w in (every Finder window)",
    "    if (id of w) is not in idsBefore then close w",
    "  end repeat",
    "end tell",
    'return ((count of idsBefore) as string) & "," & ((count of idsAfter) as string)',
])


def osascript_argv(script: str) -> List[str]:
    """AppleScript for osascript as the man page documents it: one -e per line."""
    argv = ["osascript"]
    for line in script.split("\n"):
        argv += ["-e", line]
    return argv


def classify_osascript_error(text: str, rc: int) -> Dict[str, Any]:
    """Name what an osascript failure means. kind: ok, not_allowed_assistive, apple_events_not_authorized, invalid_index,
    application_not_running, timeout, other."""
    body = text or ""
    low = body.lower()
    match = re.search(r"\((-?\d+)\)\s*$", body.strip().splitlines()[-1]) if body.strip() else None
    code = int(match.group(1)) if match else None
    if code is None:
        any_code = re.findall(r"\((-\d+)\)", body)
        code = int(any_code[-1]) if any_code else None
    if rc == 0 and "execution error" not in low and "assistive access" not in low:
        return {"kind": "ok", "code": code}
    if code in (-25211, -25201) or "not allowed assistive access" in low or "assistive access" in low:
        return {"kind": "not_allowed_assistive", "code": code}
    if code == -1743 or "not authorized to send apple events" in low:
        return {"kind": "apple_events_not_authorized", "code": code}
    if code == -1719 or "invalid index" in low:
        return {"kind": "invalid_index", "code": code}
    if code == -600 or "isn't running" in low or "is not running" in low:
        return {"kind": "application_not_running", "code": code}
    if code == -1712 or "timed out" in low or rc == 124:
        return {"kind": "timeout", "code": code}
    return {"kind": "other", "code": code}


def _js(value: Any) -> str:
    return json.dumps(value)


def jxa_ax_dump(pid: int, max_nodes: int = 500, max_depth: int = 16, budget_ms: int = 90000) -> str:
    """JavaScript for Automation: the accessibility tree of every window of the process with this unix id, as one JSON line."""
    return "\n".join([
        "function run() {",
        "  var PID = %s, MAX_NODES = %s, MAX_DEPTH = %s, BUDGET = %s;" % (_js(pid), _js(max_nodes), _js(max_depth), _js(budget_ms)),
        "  var started = Date.now();",
        "  var out = {pid: PID, process_name: null, windows: 0, window_titles: [], nodes: [], truncated: false, error: null};",
        "  function attr(f) { try { var v = f(); if (v === null || v === undefined) { return null; } var s = String(v); return s.length > 200 ? s.substring(0, 200) : s; } catch (e) { return null; } }",
        "  try {",
        "    var se = Application('System Events');",
        "    var procs = se.processes.whose({unixId: PID})();",
        "    if (procs.length === 0) { out.error = 'no process with that unix id'; return JSON.stringify(out); }",
        "    var proc = procs[0];",
        "    out.process_name = attr(function () { return proc.name(); });",
        "    var wins = proc.windows();",
        "    out.windows = wins.length;",
        "    var count = 0;",
        "    var walk = function (el, depth, path) {",
        "      if (count >= MAX_NODES || Date.now() - started > BUDGET) { out.truncated = true; return; }",
        "      count++;",
        "      out.nodes.push({path: path, depth: depth, role: attr(function () { return el.role(); }), subrole: attr(function () { return el.subrole(); }),",
        "        title: attr(function () { return el.title(); }), description: attr(function () { return el.description(); }),",
        "        name: attr(function () { return el.name(); }), value: attr(function () { return el.value(); })});",
        "      if (depth >= MAX_DEPTH) { return; }",
        "      var kids = [];",
        "      try { kids = el.uiElements(); } catch (e) { kids = []; }",
        "      for (var i = 0; i < kids.length; i++) { walk(kids[i], depth + 1, path.concat([i])); if (out.truncated) { return; } }",
        "    };",
        "    for (var w = 0; w < wins.length; w++) {",
        "      out.window_titles.push(attr(function () { return wins[w].name(); }));",
        "      walk(wins[w], 0, [w]);",
        "    }",
        "  } catch (e) { out.error = String(e); }",
        "  return JSON.stringify(out);",
        "}",
    ])


def jxa_click(pid: int, patterns: Sequence[str], roles: Sequence[str] = AX_CLICK_ROLES, max_nodes: int = 700, max_depth: int = 16, budget_ms: int = 90000) -> str:
    """JavaScript for Automation: find the first element of one of the roles whose title/description/name/value matches one of
    the regular expressions and click it (AXPress fallback). Prints one JSON line {clicked, ...}."""
    return "\n".join([
        "function run() {",
        "  var PID = %s, PATTERNS = %s, ROLES = %s, MAX_NODES = %s, MAX_DEPTH = %s, BUDGET = %s;" % (
            _js(pid), _js(list(patterns)), _js(list(roles)), _js(max_nodes), _js(max_depth), _js(budget_ms)),
        "  var started = Date.now();",
        "  var res = {clicked: false, visited: 0, matched: null, error: null};",
        "  var regexes = PATTERNS.map(function (p) { return new RegExp(p, 'i'); });",
        "  function attr(f) { try { var v = f(); if (v === null || v === undefined) { return null; } return String(v); } catch (e) { return null; } }",
        "  try {",
        "    var se = Application('System Events');",
        "    var procs = se.processes.whose({unixId: PID})();",
        "    if (procs.length === 0) { res.error = 'no process with that unix id'; return JSON.stringify(res); }",
        "    var wins = procs[0].windows();",
        "    var found = null;",
        "    var walk = function (el, depth) {",
        "      if (found !== null || res.visited >= MAX_NODES || Date.now() - started > BUDGET) { return; }",
        "      res.visited++;",
        "      var role = attr(function () { return el.role(); });",
        "      if (ROLES.indexOf(role) >= 0) {",
        "        var texts = [attr(function () { return el.title(); }), attr(function () { return el.description(); }), attr(function () { return el.name(); }), attr(function () { return el.value(); })];",
        "        for (var t = 0; t < texts.length; t++) {",
        "          if (texts[t] !== null && regexes.some(function (re) { return re.test(texts[t]); })) { found = el; res.matched = {role: role, text: texts[t].substring(0, 120), depth: depth}; return; }",
        "        }",
        "      }",
        "      if (depth >= MAX_DEPTH) { return; }",
        "      var kids = [];",
        "      try { kids = el.uiElements(); } catch (e) { kids = []; }",
        "      for (var i = 0; i < kids.length && found === null; i++) { walk(kids[i], depth + 1); }",
        "    };",
        "    for (var w = 0; w < wins.length && found === null; w++) { walk(wins[w], 0); }",
        "    if (found === null) { res.error = 'no matching element'; return JSON.stringify(res); }",
        "    try { found.click(); res.clicked = true; }",
        "    catch (e1) {",
        "      try { found.actions.byName('AXPress').perform(); res.clicked = true; res.via = 'AXPress'; }",
        "      catch (e2) { res.error = String(e1) + ' | ' + String(e2); }",
        "    }",
        "  } catch (e) { res.error = String(e); }",
        "  return JSON.stringify(res);",
        "}",
    ])


def ax_find(nodes: Sequence[Dict[str, Any]], patterns: Sequence[str], roles: Sequence[str] = AX_ANY_ROLES) -> List[Dict[str, Any]]:
    """Dumped AX nodes of the given roles whose title/description/name/value match one of the patterns."""
    regexes = [re.compile(pattern, re.IGNORECASE) for pattern in patterns]
    found = []
    for node in nodes or []:
        if node.get("role") not in roles:
            continue
        for key in ("title", "description", "name", "value"):
            text = node.get(key)
            if isinstance(text, str) and any(regex.search(text) for regex in regexes):
                found.append(node)
                break
    return found


def ax_texts(nodes: Sequence[Dict[str, Any]], limit: int = 60) -> List[str]:
    """The visible texts of a dump (static texts and button names), for the report."""
    seen: List[str] = []
    for node in nodes or []:
        for key in ("title", "value", "description", "name"):
            text = node.get(key)
            if isinstance(text, str) and text.strip() and text.strip() not in seen:
                seen.append(text.strip())
                break
        if len(seen) >= limit:
            break
    return seen


def summarize_ax(dump: Optional[Dict[str, Any]]) -> Dict[str, Any]:
    """What a dump says about the update flow: windows, element count, Settings / Check for updates / Install buttons."""
    if not dump:
        return {"available": False}
    nodes = dump.get("nodes") or []
    return {
        "available": dump.get("error") is None,
        "error": dump.get("error"),
        "windows": dump.get("windows"),
        "nodes": len(nodes),
        "truncated": bool(dump.get("truncated")),
        "settings_button": bool(ax_find(nodes, SETTINGS_PATTERNS)),
        "check_for_updates_button": bool(ax_find(nodes, CHECK_PATTERNS)),
        "install_button": bool(ax_find(nodes, INSTALL_PATTERNS)),
        "update_card_texts": [text for text in ax_texts(nodes, 200) if re.search(r"(?i)latest version|is available|update failed|install", text)][:10],
    }


def store_json(sandbox_home: Path) -> Dict[str, Any]:
    """A persisted app state as a finished onboarding leaves it (store.rs: {identity, config}); autoStart is false so the app
    starts no node and calls no ensure_binary. The identity is a throwaway test value, not a secret."""
    return {
        "identity": {
            "address": "0x" + "00" * 19 + "01",
            "publicKey": "00" * 32,
            "seedPhrase": "test test test test test test test test test test test junk",
            "createdAt": 1790000000,
        },
        "config": {
            "role": "worker",
            "modelPath": None,
            "rpcPort": 9090,
            "p2pPort": 9091,
            "autoStart": False,
            "autoUpdate": True,
            "dataDir": str(sandbox_home / ".arc"),
        },
    }


# ------------------------------------------------------------------------------------------------------------------
# fail-closed evaluation of one case (pure)
# ------------------------------------------------------------------------------------------------------------------

def artifact_like(path: str) -> bool:
    return bool(ARTIFACT_NAME_RE.search(path))


def summarize_requests(rows: Optional[Sequence[Dict[str, Any]]]) -> Dict[str, Any]:
    if rows is None:
        return {"total": None, "by_host_path": None, "tls_failures": None, "payload": None}
    counts: Dict[Tuple[str, str], int] = {}
    tls: Dict[str, int] = {}
    payload = []
    total = 0
    for row in rows:
        if row.get("kind") == "tls_failure":
            key = str(row.get("sni") or "(no SNI)")
            tls[key] = tls.get(key, 0) + 1
            continue
        if row.get("kind") != "request":
            continue
        total += 1
        host, path = str(row.get("host") or row.get("sni") or ""), str(row.get("path") or "")
        counts[(host, path)] = counts.get((host, path), 0) + 1
        if row.get("payload") or PAYLOAD_PATH_RE.search(path):
            payload.append([host, path])
    return {
        "total": total,
        "by_host_path": [[host, path, count] for (host, path), count in sorted(counts.items())],
        "tls_failures": [[sni, count] for sni, count in sorted(tls.items())],
        "payload": payload,
    }


def classify_written(path: str, prefixes: Sequence[str], temp_roots: Sequence[str] = ()) -> str:
    """expected / unexpected for a new path: below an expected prefix, or a known WebKit/app temp name under a temp root."""
    if fswatch is not None and fswatch.classify(path, prefixes) == "expected":
        return "expected"
    import fnmatch
    for root in temp_roots:
        root = root.rstrip("/")
        if path.startswith(root + "/"):
            first = path[len(root) + 1:].split("/", 1)[0]
            if any(fnmatch.fnmatch(first, glob) for glob in TEMP_NAME_GLOBS):
                return "expected"
    return "unexpected"


def expected_prefixes(real_home: str, sandbox_home: str, case: str) -> List[str]:
    """Where the released app is expected to write: its data, cache, WebKit, log and preference locations (identifier
    network.arc.desktop) in the real home and in the sandbox HOME, plus the autostart LaunchAgent the plugin registers when the
    saved config asks for it (both cases seed autoStart=false, so none is expected, but the two names it would use are tolerated)."""
    prefixes: List[str] = [sandbox_home]
    for home in (real_home,):
        library = home.rstrip("/") + "/Library"
        for sub in ("Application Support", "Caches", "WebKit", "Logs", "HTTPStorages", "Saved Application State", "Preferences", "Cookies"):
            prefixes.append("%s/%s/%s" % (library, sub, APP_IDENTIFIER))
        prefixes.append("%s/Preferences/%s.plist" % (library, APP_IDENTIFIER))
        prefixes.append("%s/Cookies/%s.binarycookies" % (library, APP_IDENTIFIER))
        prefixes.append("%s/Saved Application State/%s.savedState" % (library, APP_IDENTIFIER))
        prefixes.append("%s/HTTPStorages/%s.binarycookies" % (library, APP_IDENTIFIER))
        prefixes.append("%s/.arc" % home.rstrip("/"))
        # the autostart plugin's LaunchAgent registration (lib.rs setup) is app state, not an update artifact
        prefixes.append("%s/LaunchAgents/%s.plist" % (library, APP_PRODUCT))
        prefixes.append("%s/LaunchAgents/%s.plist" % (library, APP_IDENTIFIER))
    return prefixes


def evaluate_case(tier: str, scenario: str, requests: Optional[Sequence[Dict[str, Any]]], fs: Optional[Dict[str, Any]],
                  procs: Optional[Dict[str, Any]], bundle_diff: Optional[Dict[str, Any]], plugin_check: Dict[str, Any],
                  positive_control: Optional[Dict[str, Any]] = None, network: Optional[Dict[str, Any]] = None,
                  require_network: bool = False) -> Dict[str, Any]:
    """The five pass criteria and the verdict of one case. Every missing record leaves its criteria None (unproved);
    any violated criterion is False; only a complete record without violation is True. verdict: FAIL if any False,
    UNPROVED if any None or the check did not behave as the scenario requires, else PASS.
    Only requests to GitHub hosts are held against only_manifest_url (the recorder also answers rsms.me, the app's font host);
    a real destination other than the allowed ones (network_report) fails it too, and with require_network (real-banner mode,
    where one request is let through) a missing network record leaves it unproved."""
    criteria: Dict[str, Optional[bool]] = {key: None for key in CRITERIA}
    reasons: Dict[str, List[str]] = {key: [] for key in CRITERIA}
    notes: List[str] = []
    info: List[str] = []

    # ---- request log -------------------------------------------------------------------------------
    if requests is None:
        for key in ("only_manifest_url", "no_bundle_download"):
            reasons[key].append("no request log was recorded for this case")
    else:
        req = [row for row in requests if row.get("kind") == "request"]
        manifest = [row for row in req if row.get("role") == "manifest"]
        on_github = [row for row in req if str(row.get("host") or row.get("sni") or "") in GITHUB_HOSTS]
        others = [row for row in on_github if row.get("role") not in MANIFEST_ROLES]
        elsewhere: Dict[str, int] = {}
        for row in req:
            host = str(row.get("host") or row.get("sni") or "(unknown)")
            if host not in GITHUB_HOSTS:
                elsewhere[host] = elsewhere.get(host, 0) + 1
        if elsewhere:
            info.append("non-GitHub traffic answered by the recorder (not updater traffic): %s" % json.dumps(elsewhere, sort_keys=True))
        payload = [row for row in req if row.get("payload") or PAYLOAD_PATH_RE.search(str(row.get("path") or ""))]
        if not plugin_check.get("reached"):
            reasons["only_manifest_url"].append("the updater plugin check was not reached in this tier: %s" % (plugin_check.get("reason") or "no reason recorded"))
        elif not manifest:
            reasons["only_manifest_url"].append("no request for the manifest URL was recorded: the check never reached the endpoint")
        elif others:
            criteria["only_manifest_url"] = False
            reasons["only_manifest_url"].append("requests other than the manifest URL: %s" % [[r.get("host"), r.get("path")] for r in others][:6])
        else:
            criteria["only_manifest_url"] = True
        if payload:
            criteria["no_bundle_download"] = False
            reasons["no_bundle_download"].append("bundle/installer/signature requests: %s" % [[r.get("host"), r.get("path")] for r in payload][:6])
        else:
            criteria["no_bundle_download"] = True

    # ---- what reached the real Internet ------------------------------------------------------------------
    if network is not None and network.get("violations"):
        criteria["only_manifest_url"] = False
        reasons["only_manifest_url"].append("a real destination other than the allowed ones was contacted: %s" % [
            "%s:%s (%s, pid %s)" % (item.get("remote_ip"), item.get("remote_port"), item.get("command") or ",".join(item.get("procs") or []) or "?", item.get("pid", "?"))
            for item in network["violations"]][:6])
    elif require_network and (network is None or not network.get("recorded")) and criteria["only_manifest_url"] is not False:
        criteria["only_manifest_url"] = None
        reasons["only_manifest_url"].append("a request was allowed to leave the sandbox, but no network record (tcpdump/lsof) was captured to prove nothing else did")

    # ---- processes, bundle ---------------------------------------------------------------------------
    install_problems: List[str] = []
    install_unknown = False
    launch_problems: List[str] = []
    launch_unknown = False
    if procs is None:
        install_unknown = launch_unknown = True
        reasons["no_install"].append("no process record")
        reasons["no_new_app_launch"].append("no process record")
    else:
        seen = procs.get("seen") or []
        app_pid = procs.get("app_pid")
        bundle = procs.get("bundle_path") or ""
        app_like = [row for row in seen if ("ARC Node.app/" in str(row.get("comm")) or str(row.get("comm")).endswith("/arc-desktop"))]
        if tier == "released_app":
            if app_pid is None:
                launch_unknown = True
                reasons["no_new_app_launch"].append("the app was not launched, so no launch baseline exists")
            else:
                extra = sorted({row["pid"] for row in app_like if row["pid"] != app_pid})
                if extra:
                    launch_problems.append("process(es) %s from an ARC Node bundle besides the launched pid %s" % (extra, app_pid))
                foreign = sorted({str(row["comm"]) for row in app_like if bundle and not str(row["comm"]).startswith(bundle)})
                if foreign:
                    launch_problems.append("an ARC Node executable outside the sandbox bundle ran: %s" % foreign[:3])
            kids = [row for row in descendants(seen, app_pid)] if app_pid is not None else []
            for row in kids:
                if INSTALLER_PROC_RE.search(str(row.get("comm"))):
                    install_problems.append("the app started an installer-like process: %s" % row.get("comm"))
        else:
            if app_like:
                launch_problems.append("an ARC Node app process ran during the native check: %s" % [row["comm"] for row in app_like][:3])
        if tier == "released_app":
            if bundle_diff is None:
                install_unknown = True
                reasons["no_install"].append("no before/after record of the app bundle")
            elif bundle_diff.get("added") or bundle_diff.get("removed") or bundle_diff.get("changed"):
                install_problems.append("the app bundle changed: %s" % {k: len(v) for k, v in bundle_diff.items()})

    # ---- files -----------------------------------------------------------------------------------
    file_problems: List[str] = []
    file_unknown = False
    if fs is None or fs.get("diff") is None or not fs.get("poller_scans"):
        file_unknown = True
        reasons["no_new_files"].append("no complete file-write record (snapshots and poller scans are both required)")
    else:
        diff = fs["diff"]
        prefixes = fs.get("expected_prefixes") or []
        temp_roots = fs.get("temp_roots") or []
        new_paths = list(diff.get("added") or []) + list(diff.get("changed") or [])
        new_paths += [str(event.get("path")) for event in (fs.get("poller_events") or []) if event.get("event") in ("added", "changed")]
        for path in sorted(set(new_paths)):
            if classify_written(path, prefixes, temp_roots) == "unexpected":
                file_problems.append("unexpected new path: %s" % path)
            if artifact_like(path):
                install_problems.append("update-artifact-like path written: %s" % path)
                file_problems.append("update-artifact-like path written: %s" % path)
    if install_problems:
        criteria["no_install"] = False
        reasons["no_install"].extend(install_problems[:6])
    elif not install_unknown:
        criteria["no_install"] = True
    if launch_problems:
        criteria["no_new_app_launch"] = False
        reasons["no_new_app_launch"].extend(launch_problems[:6])
    elif not launch_unknown:
        criteria["no_new_app_launch"] = True
    if file_problems:
        criteria["no_new_files"] = False
        reasons["no_new_files"].extend(file_problems[:8])
    elif not file_unknown:
        criteria["no_new_files"] = True

    # ---- the check itself behaved as the scenario requires ---------------------------------------------------
    verdict_extra: List[str] = []
    if plugin_check.get("reached"):
        outcome = plugin_check.get("outcome")
        if scenario.startswith("latest-404") and not (outcome == "error" and plugin_check.get("error_kind") == "ReleaseNotFound"):
            verdict_extra.append("the 404 scenario did not end in ReleaseNotFound (outcome %s, kind %s)" % (outcome, plugin_check.get("error_kind")))
        if scenario.startswith("bait") and not (outcome == "update_available" and (plugin_check.get("update") or {}).get("version") == "0.8.11"):
            verdict_extra.append("the bait scenario did not report update 0.8.11 (outcome %s)" % outcome)
        if scenario.startswith("bait") and plugin_check.get("download_attempted"):
            verdict_extra.append("a download was attempted without the control flag")
        if scenario.startswith("bait") and tier == "native_check":
            if positive_control is None or not positive_control.get("observed"):
                verdict_extra.append("positive control missing: the recorder was not shown to see a bundle request")
    notes.extend(verdict_extra)

    if any(value is False for value in criteria.values()):
        verdict = "FAIL"
    elif any(value is None for value in criteria.values()) or verdict_extra:
        verdict = "UNPROVED"
    else:
        verdict = "PASS"
    return {"criteria": criteria, "reasons": {k: v for k, v in reasons.items() if v}, "notes": notes, "info": info, "verdict": verdict}


def overall_verdict(tiers: Dict[str, Dict[str, Any]], cases: Sequence[Dict[str, Any]]) -> Tuple[str, Dict[str, str]]:
    """Per tier and overall. The overall verdict is PASS only when the released app's plugin path ran and every case passed."""
    per_tier: Dict[str, str] = {}
    for tier in ALL_TIERS:
        items = [case for case in cases if case.get("tier") == tier]
        if any(case["verdict"] == "FAIL" for case in items):
            per_tier[tier] = "FAIL"
        elif items and all(case["verdict"] == "PASS" for case in items):
            per_tier[tier] = "PASS"
        elif tiers.get(tier, {}).get("result") == "infeasible":
            per_tier[tier] = "INFEASIBLE"
        else:
            per_tier[tier] = "UNPROVED"
    if "FAIL" in per_tier.values():
        return "FAIL", per_tier
    if per_tier.get("released_app") == "PASS" and per_tier.get("native_check") in ("PASS", "UNPROVED", "INFEASIBLE"):
        return "PASS", per_tier
    return "UNPROVED", per_tier


def build_feasibility(probe: Dict[str, Any]) -> str:
    """The two-line answer the coordinator needs, filled from probe.json (every sentence names its evidence)."""
    acc = probe.get("accessibility", {}).get("summary", {})
    ui = probe.get("released_app_ui", {})
    image = probe.get("system", {}).get("image", "unknown image")
    if acc.get("click_works"):
        first = "YES: UI scripting works on %s (a System Events menu click on Finder succeeded: %s)" % (image, acc.get("click_evidence"))
    elif acc.get("system_events_reachable"):
        first = "PARTLY: System Events answers on %s but a UI click failed with %s (%s)" % (image, acc.get("click_error_kind"), acc.get("click_error"))
    else:
        first = "NO: osascript could not drive System Events on %s: %s" % (image, acc.get("first_error") or "no output")
    if ui.get("window_appeared"):
        first += "; the released app window appeared and its AX tree has %s elements (Settings button %s, Check for updates button %s)." % (
            ui.get("nodes"), "found" if ui.get("settings_button") else "not found", "found" if ui.get("check_button") else "not found")
    elif ui.get("attempted"):
        first += "; the released app window did NOT appear within the wait (%s)." % (ui.get("problem") or "no window")
    else:
        first += "; the released app was not launched in this run (%s)." % (ui.get("problem") or "not attempted")
    real = bool(probe["real_banner_api"]) if "real_banner_api" in probe else bool(ui.get("real_banner_api"))
    second = ("The plugin check() is reachable from the released app only through the Install button, which Settings.tsx renders only after the banner call "
              "check_for_update (bundled webpki roots, not interceptable) reports an update; real-banner mode (one read-only GET to api.github.com allowed through, "
              "approved by work-99) was %s: Install button present after Check for updates = %s" % ("ON" if real else "OFF", "YES" if ui.get("install_button") else "NO"))
    if not ui.get("install_button") and ui.get("banner_ui_text"):
        second += " (UI after %s Check for updates click(s): %s)" % (len(ui.get("banner_attempts") or []), ui.get("banner_ui_text"))
    if ui.get("install_clicked"):
        second += "; clicked, the recorder saw %s manifest request(s) and the UI said: %r" % (ui.get("manifest_requests"), ui.get("ui_error_text"))
    if ui.get("network_violations"):
        second += "; ISOLATION BREACH: another real destination was contacted: %s" % ui.get("network_violations")
    elif real and ui.get("network_recorded"):
        second += "; no other real destination was seen by tcpdump/lsof (the content of the one pass-through request is not recorded)"
    second += ". Native-check tier (same tauri-plugin-updater %s, same OS trust-store verifier): %s." % (PLUGIN_VERSION, probe.get("native_check", {}).get("status", "not run in this probe"))
    return first + "\n" + second + "\n"


# ------------------------------------------------------------------------------------------------------------------
# recorder: every command, exit code and output tail goes to steps.log (masked)
# ------------------------------------------------------------------------------------------------------------------

class CmdResult:
    def __init__(self, argv: Sequence[str], rc: int, out: str, err: str, elapsed: float, timed_out: bool = False):
        self.argv = list(argv)
        self.rc = rc
        self.out = out or ""
        self.err = err or ""
        self.elapsed = elapsed
        self.timed_out = timed_out

    @property
    def ok(self) -> bool:
        return self.rc == 0

    @property
    def text(self) -> str:
        return self.out + self.err


class Recorder:
    def __init__(self, evidence: Path, live_ips: Sequence[str] = ()):
        self.evidence = Path(evidence)
        self.evidence.mkdir(parents=True, exist_ok=True)
        self.log_path = self.evidence / "steps.log"
        self.live_ips = list(live_ips)
        self.secrets = [os.environ.get(name, "") for name in ("GH_TOKEN", "GITHUB_TOKEN")]
        self.lock = threading.Lock()
        self.count = 0

    def mask(self, text: str) -> str:
        return mask_text(text, self.live_ips, self.secrets)

    def _write(self, text: str) -> None:
        with self.lock, self.log_path.open("a", encoding="utf-8") as handle:
            handle.write(self.mask(text))

    def note(self, message: str) -> None:
        self._write("[%s] # %s\n" % (now_iso(), message))
        print("# " + self.mask(message), flush=True)

    def run(self, argv: Sequence[str], label: str = "", timeout: float = 120.0, input_text: Optional[str] = None,
            env: Optional[Dict[str, str]] = None, cwd: Optional[Path] = None, sudo: bool = False, quiet: bool = False,
            max_lines: int = 30) -> CmdResult:
        full = (["sudo", "-n"] if sudo else []) + [str(item) for item in argv]
        started = time.time()
        timed_out = False
        try:
            done = subprocess.run(
                full, input=input_text.encode("utf-8") if input_text is not None else None, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                timeout=timeout, check=False, env=env, cwd=str(cwd) if cwd else None,
            )
            rc, out, err = done.returncode, done.stdout.decode("utf-8", "replace"), done.stderr.decode("utf-8", "replace")
        except subprocess.TimeoutExpired as error:
            rc, timed_out = 124, True
            out = (error.stdout or b"").decode("utf-8", "replace") if isinstance(error.stdout, bytes) else (error.stdout or "")
            err = ((error.stderr or b"").decode("utf-8", "replace") if isinstance(error.stderr, bytes) else (error.stderr or "")) + "\n[timed out after %ss]" % timeout
        except OSError as error:
            rc, out, err = 127, "", "%s: %s" % (type(error).__name__, error)
        elapsed = time.time() - started
        self.count += 1
        header = "[%s] $ %s%s\n  -> rc=%s in %.1fs\n" % (now_iso(), " ".join(shell_quote(item) for item in full), ("   # " + label) if label else "", rc, elapsed)
        body = ""
        if not quiet:
            for name, text in (("stdout", out), ("stderr", err)):
                if text.strip():
                    body += "  %s:\n%s\n" % (name, "\n".join("    " + line for line in tail_lines(text, max_lines).splitlines()))
        self._write(header + body)
        print(self.mask("$ %s -> rc=%s (%.1fs)" % (" ".join(shell_quote(item) for item in full)[:160], rc, elapsed)), flush=True)
        return CmdResult(full, rc, out, err, elapsed, timed_out)


def shell_quote(item: str) -> str:
    return item if re.fullmatch(r"[A-Za-z0-9_@%+=:,./-]+", item) else "'" + item.replace("'", "'\\''") + "'"


class Deadline:
    def __init__(self, seconds: float):
        self.end = time.time() + seconds

    def left(self) -> float:
        return max(0.0, self.end - time.time())


# ------------------------------------------------------------------------------------------------------------------
# machine facts, Accessibility tests
# ------------------------------------------------------------------------------------------------------------------

def system_facts(rec: Recorder) -> Dict[str, Any]:
    facts: Dict[str, Any] = {
        "machine": platform.machine(),
        "mac_ver": platform.mac_ver()[0],
        "image": "%s %s" % (os.environ.get("ImageOS", "unknown ImageOS"), os.environ.get("ImageVersion", "unknown ImageVersion")),
        "runner_name": os.environ.get("RUNNER_NAME"),
        "python": sys.version.split()[0],
    }
    for key, argv in (
        ("sw_vers", ["sw_vers"]),
        ("whoami", ["id", "-un"]),
        ("console_user", ["stat", "-f", "%Su", "/dev/console"]),
        ("launchd_session", ["launchctl", "managername"]),
        ("window_server", ["pgrep", "-lx", "WindowServer"]),
        ("hw_model", ["sysctl", "-n", "hw.model"]),
        ("cpu", ["sysctl", "-n", "machdep.cpu.brand_string"]),
        ("sudo", ["sudo", "-n", "true"]),
        ("openssl", ["openssl", "version"]),
        ("cargo", ["cargo", "--version"]),
        ("gh", ["gh", "--version"]),
    ):
        result = rec.run(argv, label="system facts: " + key, timeout=20)
        facts[key] = {"rc": result.rc, "out": tail_lines(result.text.strip(), 6)}
    return facts


def process_chain(rec: Recorder) -> List[Dict[str, Any]]:
    chain: List[Dict[str, Any]] = []
    pid = os.getpid()
    for _ in range(20):
        result = rec.run(["ps", "-o", "pid=,ppid=,comm=", "-p", str(pid)], timeout=10, quiet=True)
        rows = parse_ps(result.out)
        if not rows:
            break
        chain.append(rows[0])
        if rows[0]["ppid"] <= 1:
            break
        pid = rows[0]["ppid"]
    return chain


def accessibility_tests(rec: Recorder, evidence: Path) -> Dict[str, Any]:
    """Three independent tests of the Accessibility/Automation permission, every output verbatim."""
    tests: Dict[str, Any] = {}

    def record(name: str, result: CmdResult, extra: Optional[Dict[str, Any]] = None) -> None:
        entry = {"argv": [rec.mask(item) for item in result.argv], "rc": result.rc, "stdout": rec.mask(result.out), "stderr": rec.mask(result.err),
                 "classification": classify_osascript_error(result.text, result.rc)}
        if extra:
            entry.update(extra)
        tests[name] = entry

    # (a) System Events answers at all
    record("a_system_events_process_list", rec.run(osascript_argv(AS_PROCESS_NAMES), label="Accessibility (a): System Events process list", timeout=60))
    record("a2_ui_elements_enabled", rec.run(osascript_argv(AS_UI_ENABLED), label="Accessibility (a2): UI elements enabled", timeout=60))
    # (b) the TCC databases and the process chain that would have to hold the grant
    query = "SELECT service, client, client_type, auth_value, auth_reason, datetime(last_modified,'unixepoch') FROM access WHERE service IN ('kTCCServiceAccessibility','kTCCServiceAppleEvents','kTCCServicePostEvent','kTCCServiceScreenCapture');"
    for name, path in (("b_tcc_system_db", "/Library/Application Support/com.apple.TCC/TCC.db"),
                       ("b_tcc_user_db", str(Path.home() / "Library/Application Support/com.apple.TCC/TCC.db"))):
        result = rec.run(["sqlite3", "-readonly", "-separator", "|", path, query], label="Accessibility (b): " + name, timeout=30)
        record(name, result)
    tests["b_process_chain"] = process_chain(rec)
    # (c) a real click through System Events on a harmless app
    click = rec.run(osascript_argv(AS_FINDER_CLICK), label="Accessibility (c): click Finder > File > New Finder Window", timeout=90)
    record("c_finder_menu_click", click)
    counts = (click.out.strip().splitlines() or [""])[-1]
    match = re.fullmatch(r"(\d+),(\d+)", counts)
    click_works = bool(match) and click.ok and int(match.group(2)) > int(match.group(1))
    summary = {
        "system_events_reachable": tests["a_system_events_process_list"]["classification"]["kind"] == "ok",
        "ui_elements_enabled": tests["a2_ui_elements_enabled"]["stdout"].strip(),
        "click_works": click_works,
        "click_evidence": "Finder windows %s -> %s" % (match.group(1), match.group(2)) if match else None,
        "click_error_kind": tests["c_finder_menu_click"]["classification"]["kind"] if not click_works else None,
        "click_error": tail_lines(click.text.strip(), 3) if not click_works else None,
        "first_error": next((tests[name]["stderr"].strip() for name in ("a_system_events_process_list", "c_finder_menu_click") if tests[name]["stderr"].strip()), None),
        "assistive_access": "granted" if click_works else ("denied" if any(tests[n]["classification"]["kind"] == "not_allowed_assistive" for n in ("c_finder_menu_click", "a_system_events_process_list")) else "unknown"),
    }
    result = {"tests": tests, "summary": summary}
    write_json(evidence / "accessibility.json", result)
    return result


# ------------------------------------------------------------------------------------------------------------------
# release assets and the app bundle
# ------------------------------------------------------------------------------------------------------------------

class StepFailed(RuntimeError):
    pass


def fetch_release(rec: Recorder) -> Dict[str, Any]:
    """Read-only GET of the v0.7.11 release (gh with the job token, curl as the fallback)."""
    attempts = [(["gh", "api", "repos/%s/releases/tags/%s" % (REPO, APP_TAG)], "release metadata (gh api)")]
    headers = ["-H", "Accept: application/vnd.github+json"]
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        headers += ["-H", "Authorization: Bearer " + token]
    attempts.append((["curl", "-fsSL", "--proto", "=https", "--tlsv1.2", "--retry", "3"] + headers + ["https://api.github.com/repos/%s/releases/tags/%s" % (REPO, APP_TAG)], "release metadata (curl)"))
    for argv, label in attempts:
        result = rec.run(argv, label=label, timeout=90, quiet=True)
        if result.ok:
            try:
                value = json.loads(result.out)
            except ValueError:
                continue
            if isinstance(value, dict) and value.get("tag_name") == APP_TAG:
                return value
    raise StepFailed("the release metadata of %s could not be read" % APP_TAG)


def config_pin(name: str) -> Optional[str]:
    """The digest the committed config.json pins for an asset (cross-check of the live release API)."""
    try:
        config = json.loads((HERE / "config.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    for asset in (config.get("app", {}).get("assets") or {}).values():
        if asset.get("name") == name:
            return release_digest_hex(asset.get("release_digest") or asset.get("sha256"))
    return None


def download_verified(rec: Recorder, asset: Dict[str, Any], dest_dir: Path) -> Tuple[Path, Dict[str, Any]]:
    dest_dir.mkdir(parents=True, exist_ok=True)
    dest = dest_dir / str(asset["name"])
    url = str(asset["browser_download_url"])
    result = rec.run(["curl", "-fL", "--proto", "=https", "--tlsv1.2", "--retry", "3", "--retry-delay", "2", "-o", str(dest), url], label="download " + asset["name"], timeout=300)
    if not result.ok or not dest.exists():
        raise StepFailed("download of %s failed (rc %s)" % (asset["name"], result.rc))
    digest = sha256_file(dest)
    expected = release_digest_hex(asset.get("digest"))
    pinned = config_pin(str(asset["name"]))
    info = {
        "name": asset["name"], "size": dest.stat().st_size, "sha256": digest, "release_digest": asset.get("digest"),
        "digest_match": bool(expected) and digest == expected, "config_pin": pinned,
        "config_pin_match": (pinned == digest) if pinned else None,
    }
    if not info["digest_match"]:
        raise StepFailed("sha256 of %s (%s) does not equal the release digest (%s): not used" % (asset["name"], digest, asset.get("digest")))
    return dest, info


def extract_app(rec: Recorder, assets: Dict[str, Any], downloads: Path, sandbox: Path) -> Tuple[Path, Dict[str, Any]]:
    """Mount the .dmg read-only and copy the .app into the sandbox (the .app.tar.gz is the fallback)."""
    apps_dir = sandbox / "apps"
    apps_dir.mkdir(parents=True, exist_ok=True)
    errors: List[str] = []
    if assets.get("dmg"):
        dmg, info = download_verified(rec, assets["dmg"], downloads)
        mount = sandbox / "mnt"
        mount.mkdir(parents=True, exist_ok=True)
        # a license agreement inside the image would wait for "Y" on stdin: answer it (harmless when there is none)
        attach = rec.run(["hdiutil", "attach", "-nobrowse", "-readonly", "-noverify", "-noautoopen", "-mountpoint", str(mount), str(dmg)], label="mount the dmg",
                         timeout=180, input_text="Y\n")
        if attach.ok:
            try:
                bundles = sorted(item for item in mount.iterdir() if item.suffix == ".app")
                if bundles:
                    dest = apps_dir / bundles[0].name
                    copy = rec.run(["ditto", str(bundles[0]), str(dest)], label="copy the app out of the dmg", timeout=180)
                    if copy.ok and dest.exists():
                        return dest, dict(info, source="dmg")
                    errors.append("ditto failed (rc %s)" % copy.rc)
                else:
                    errors.append("the dmg holds no .app")
            finally:
                rec.run(["hdiutil", "detach", str(mount), "-force"], label="unmount the dmg", timeout=60)
        else:
            errors.append("hdiutil attach failed (rc %s)" % attach.rc)
    if assets.get("tar"):
        tgz, info = download_verified(rec, assets["tar"], downloads)
        unpack = rec.run(["tar", "-xzf", str(tgz), "-C", str(apps_dir)], label="unpack the app.tar.gz (fallback)", timeout=180)
        bundles = sorted(item for item in apps_dir.iterdir() if item.suffix == ".app")
        if unpack.ok and bundles:
            return bundles[0], dict(info, source="app.tar.gz", dmg_errors=errors)
        errors.append("tar failed (rc %s)" % unpack.rc)
    raise StepFailed("the app could not be extracted: " + "; ".join(errors))


def bundle_facts(rec: Recorder, app: Path) -> Dict[str, Any]:
    plist = app / "Contents" / "Info.plist"

    def key(name: str) -> Optional[str]:
        result = rec.run(["/usr/libexec/PlistBuddy", "-c", "Print :" + name, str(plist)], label="Info.plist " + name, timeout=20)
        return result.out.strip() if result.ok else None

    executable = key("CFBundleExecutable")
    binary = app / "Contents" / "MacOS" / (executable or "")
    facts: Dict[str, Any] = {
        "bundle": str(app), "version": key("CFBundleShortVersionString"), "identifier": key("CFBundleIdentifier"),
        "executable": executable, "binary": str(binary),
    }
    for name, argv in (
        ("codesign_verify", ["codesign", "--verify", "--deep", "--strict", "--verbose=2", str(app)]),
        ("codesign_display", ["codesign", "-dv", "--verbose=2", str(app)]),
        ("xattr", ["xattr", "-lr", str(app)]),
        ("lipo", ["lipo", "-archs", str(binary)]),
    ):
        result = rec.run(argv, label="bundle: " + name, timeout=60)
        facts[name] = {"rc": result.rc, "out": tail_lines(result.text.strip(), 12)}
    if "com.apple.quarantine" in facts["xattr"]["out"]:
        cleared = rec.run(["xattr", "-dr", "com.apple.quarantine", str(app)], label="clear quarantine (curl downloads normally carry none)", timeout=60)
        facts["quarantine_cleared"] = cleared.ok
    return facts


def collect_provenance(rec: Recorder, binary: Path, evidence: Path) -> Dict[str, Any]:
    """Crate versions from the strings of the released macOS binary (the v0.7.11 tag has no desktop Cargo.lock), plus F4's report."""
    result = rec.run(["strings", "-a", str(binary)], label="strings of the released binary", timeout=180, quiet=True)
    names = ("tauri-plugin-updater", "tauri-utils", "tauri-runtime", "tauri", "rustls-platform-verifier", "reqwest", "rustls", "webpki-roots", "hyper", "minisign-verify")
    found = sorted(set("%s-%s" % (match.group(1), match.group(2)) for match in re.finditer(r"\b(%s)-(\d+\.\d+\.\d+)\b" % "|".join(re.escape(n) for n in names), result.out)))
    provenance: Dict[str, Any] = {
        "binary": str(binary), "binary_sha256": sha256_file(binary) if binary.exists() else None, "strings_rc": result.rc,
        "crates_in_binary": found,
        "plugin_pinned": "tauri-plugin-updater-%s" % PLUGIN_VERSION in found,
        "bundled_webpki_roots_present": any(item.startswith("webpki-roots-") for item in found),
        "note": "versions read from the strings of the released binary; the tag has no desktop Cargo.lock and the build log expired (HTTP 410)",
    }
    checker = HERE / "desktop_updater_check.py"
    if checker.exists():
        out = evidence / "provenance-check-source.json"
        report = rec.run([sys.executable, str(checker), "check-source", "--repo", str(ROOT), "--tag", APP_TAG, "--plugin-offline",
                          "--released-binary", str(binary), "--out", str(out)], label="check-source (F4)", timeout=180)
        provenance["check_source"] = {"rc": report.rc, "file": out.name if out.exists() else None, "tail": tail_lines(report.text.strip(), 10)}
    write_json(evidence / "provenance.json", provenance)
    return provenance


# ------------------------------------------------------------------------------------------------------------------
# the interception environment: pf block, CA + trust, hosts, recording server
# ------------------------------------------------------------------------------------------------------------------

class Interception:
    """Everything that touches the runner: torn down by teardown() (idempotent, also registered with atexit)."""

    def __init__(self, rec: Recorder, evidence: Path, work: Path, live_ips: Sequence[str], real_banner: bool = True,
                 shared_dir: Optional[Path] = None, keep_ca: bool = False):
        self.rec = rec
        self.evidence = evidence
        self.work = work
        self.live_ips = list(live_ips)
        self.real_banner = real_banner
        self.hosts = intercept_hosts(real_banner)
        self.banner_api_ips: List[str] = []
        # second line of defence: names whose addresses are blocked in pf next to the live nodes (the hosts file did not hold WebKit back from rsms.me)
        self.extra_block_names: Tuple[str, ...] = EXTRA_INTERCEPT_HOSTS
        self.extra_block_ips: List[str] = []
        self.pf_conf = work / "pf-live-block.conf"
        # one CA per JOB, shared by the probe and run phases: generated once, trusted once, removed once (a repeated security(1) call hung)
        self.shared_dir = Path(shared_dir) if shared_dir is not None else work / "shared"
        self.keep_ca = keep_ca
        self.ca_reused = False
        self.ca: Optional[Dict[str, Any]] = None
        self.ca_sha1: Optional[str] = None
        self.trusted = False
        self.hosts_backup: Optional[Path] = None
        self.hosts_on = False
        self.pf_enabled_by_us = False
        self.pf_loaded = False
        self.server_proc: Optional[subprocess.Popen] = None
        self.server_pid: Optional[int] = None
        self._server_out: Any = None
        self.facts: Dict[str, Any] = {
            "hosts_mapped": [], "steps": [],
            "real_banner_api": {"enabled": real_banner, "scope": REAL_BANNER_SCOPE if real_banner else "off: every GitHub name, including api.github.com, is mapped to the recorder; the banner call cannot succeed",
                                "content_recorded": False, "api_addresses": []},
        }
        self.torn_down = False
        atexit.register(self.teardown)

    # ---- live-network block (pf) ----------------------------------------------------------------------------
    @property
    def blocked_ips(self) -> List[str]:
        """Every address pf drops traffic to: the six live nodes and the resolved addresses of the extra names."""
        return [canonical_ip(a) for a in self.live_ips] + list(self.extra_block_ips)

    def resolve_names(self, names: Sequence[str], system: bool = True) -> Dict[str, List[str]]:
        """Real addresses of names, resolved ON THE RUNNER at run time (never hard-coded): the system resolver before the hosts mapping
        and `dig +short` (which does not read /etc/hosts) at any time."""
        import socket
        found: Dict[str, List[str]] = {}
        for name in names:
            addresses: List[str] = []
            if system:
                try:
                    for item in socket.getaddrinfo(name, 443, proto=socket.IPPROTO_TCP):
                        candidate = canonical_ip(item[4][0])
                        if candidate not in addresses and not is_loopback(candidate):
                            addresses.append(candidate)
                except OSError as error:
                    self.rec.note("could not resolve %s: %s" % (name, error))
            for record in ("A", "AAAA"):
                done = self.rec.run(["dig", "+short", "+time=3", "+tries=2", record, name], label="dig %s %s (real resolver, ignores /etc/hosts)" % (record, name), timeout=20)
                for candidate in parse_dig_addresses(done.out):
                    if candidate not in addresses:
                        addresses.append(candidate)
            found[name] = addresses
        return found

    def apply_pf(self) -> Dict[str, Any]:
        """Write the rules (live nodes + extra addresses), load them, make sure pf is on, and verify one rule per address."""
        total = self.blocked_ips
        info: Dict[str, Any] = {"method": "pf", "addresses": len(self.live_ips), "extra_addresses": len(self.extra_block_ips), "verified": False}
        self.pf_conf.write_text(pf_conf_text(self.live_ips, self.extra_block_ips), encoding="utf-8")
        before = self.rec.run(["pfctl", "-s", "info"], label="pf status before", timeout=30, sudo=True)
        was_enabled = "Status: Enabled" in before.text
        load = self.rec.run(["pfctl", "-f", str(self.pf_conf)], label="load the pf block rules (live nodes and extra addresses)", timeout=30, sudo=True)
        self.pf_loaded = load.ok
        enable = self.rec.run(["pfctl", "-e"], label="enable pf", timeout=30, sudo=True)
        status = self.rec.run(["pfctl", "-s", "info"], label="pf status after", timeout=30, sudo=True)
        rules = self.rec.run(["pfctl", "-sr"], label="pf rules loaded", timeout=30, sudo=True)
        if not self.pf_enabled_by_us:
            self.pf_enabled_by_us = not was_enabled and "Status: Enabled" in status.text
        count = count_pf_block_rules(rules.text, total)
        table_count = None
        if count != len(total) and automatic_tables(rules.text):
            # the optimizer merged the rules into an anonymous table: read the table, the block rule must still name it
            outputs = [self.rec.run(["pfctl", "-t", name, "-T", "show"], label="pf anonymous table %s" % name, timeout=30, sudo=True).text
                       for name in automatic_tables(rules.text)]
            table_count = addresses_in_tables(outputs, total)
            if table_count == len(total):
                count = table_count
        info["table_addresses_listed"] = table_count
        info.update({
            "load_rc": load.rc, "enable_rc": enable.rc, "status_enabled": "Status: Enabled" in status.text,
            "block_rules_listed": count, "verified": load.ok and "Status: Enabled" in status.text and count == len(total),
            "rules_masked": self.rec.mask(tail_lines(rules.text.strip(), 14)),
            "extra_block": {"names": list(self.extra_block_names), "addresses": list(self.extra_block_ips),
                            "why": "WebKit's network process reached rsms.me (index.html:14-15) although the hosts file maps it to loopback; an IP block does not depend on DNS"},
        })
        self.facts["live_block"] = info
        return info

    def block_live_network(self) -> Dict[str, Any]:
        """The pf block of the six live nodes plus the real addresses of the extra names, resolved now (before any hosts mapping)."""
        resolved = self.resolve_names(self.extra_block_names, system=True)
        self.extra_block_ips = sorted({a for addresses in resolved.values() for a in addresses if a not in self.blocked_ips})
        self.facts["extra_block_resolution"] = resolved
        return self.apply_pf()

    def refresh_extra_blocks(self) -> Dict[str, Any]:
        """Before a case: resolve the extra names again (dig only; the hosts mapping is in effect) and block any address not blocked yet."""
        fresh = self.resolve_names(self.extra_block_names, system=False)
        new = sorted({a for addresses in fresh.values() for a in addresses if a not in self.blocked_ips})
        if not new:
            return {"added": [], "verified": self.facts.get("live_block", {}).get("verified")}
        self.extra_block_ips = sorted(set(self.extra_block_ips) | set(new))
        info = self.apply_pf()
        info["added"] = new
        return info

    def live_block_counters(self) -> str:
        result = self.rec.run(["pfctl", "-sr", "-v"], label="pf rule counters (packets dropped = attempts to reach a live node)", timeout=30, sudo=True, quiet=True)
        return self.rec.mask(result.out)

    # ---- CA and trust ------------------------------------------------------------------------------------------
    @property
    def ca_state_path(self) -> Path:
        return self.shared_dir / "ca-state.json"

    def read_ca_state(self) -> Optional[Dict[str, Any]]:
        try:
            value = json.loads(self.ca_state_path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return None
        return value if isinstance(value, dict) else None

    def write_ca_state(self) -> None:
        if self.ca is None:
            return
        self.shared_dir.mkdir(parents=True, exist_ok=True)
        write_json(self.ca_state_path, {"ca_sha256": self.ca.get("ca_sha256"), "ca_sha1": self.ca_sha1, "hostnames": sorted(self.hosts), "trusted": self.trusted,
                                        "updated": now_iso()})

    def load_shared_ca(self, state: Dict[str, Any]) -> bool:
        """Reuse the CA a previous phase of this job generated (and trusted) when it covers the same host names. The private keys
        stay where they were created; nothing here copies them."""
        directory = self.shared_dir / "ca"
        paths = {"ca_cert": directory / "ca.crt", "ca_key": directory / "private" / "ca.key", "server_cert": directory / "server.crt",
                 "server_key": directory / "private" / "server.key"}
        if sorted(state.get("hostnames") or []) != sorted(self.hosts) or not all(path.is_file() for path in paths.values()):
            return False
        try:
            digest = (directory / "ca.sha256").read_text(encoding="ascii").strip()
        except OSError:
            return False
        self.ca = {key: str(path) for key, path in paths.items()}
        self.ca.update({"ca_sha256": digest, "hostnames": list(self.hosts)})
        self.ca_sha1 = state.get("ca_sha1")
        self.trusted = bool(state.get("trusted"))
        self.ca_reused = True
        return True

    def make_ca(self) -> Dict[str, Any]:
        if ca_lib is None:
            raise StepFailed("lib/ca.py is missing")
        state = self.read_ca_state()
        if state and self.load_shared_ca(state):
            self.rec.note("reusing the CA of an earlier phase of this job (sha256 %s, trusted=%s)" % (str(self.ca["ca_sha256"])[:16], self.trusted))
        else:
            if state and state.get("trusted") and state.get("ca_sha1"):
                # a CA with a different host list is still trusted from an earlier phase: take its trust away before making a new one
                self.ca_sha1 = state.get("ca_sha1")
                self.trusted = True
                self.untrust_ca(state_ca_cert=str(self.shared_dir / "ca" / "ca.crt"))
            self.ca = ca_lib.make_ca(self.shared_dir / "ca", self.hosts)
            self.ca_reused = False
            self.trusted = False
            finger = self.rec.run(["openssl", "x509", "-in", str(self.ca["ca_cert"]), "-noout", "-fingerprint", "-sha1"], label="CA sha1 (for removal)", timeout=20)
            match = re.search(r"=([0-9A-Fa-f:]{59})", finger.text)
            self.ca_sha1 = match.group(1).replace(":", "").upper() if match else None
            self.write_ca_state()
        shutil.copyfile(str(self.ca["ca_cert"]), str(self.evidence / "ca.crt"))
        (self.evidence / "ca.sha256").write_text(str(self.ca["ca_sha256"]) + "\n", encoding="utf-8")
        self.facts["ca_sha256"] = self.ca["ca_sha256"]
        self.facts["ca_reused_from_an_earlier_phase"] = self.ca_reused
        return self.ca

    def run_security(self, argv: Sequence[str], label: str, timeout: float) -> CmdResult:
        """A security(1) call that cannot hang the job. When it times out, the sudo that started it dies but root's security(1) child
        (and a SecurityAgent authorization dialog nobody can click) survives and blocks the next call: kill both."""
        result = self.rec.run(["security"] + list(argv), label=label, timeout=timeout, sudo=True)
        if result.timed_out:
            for name in ("security", "SecurityAgent", "authorizationhost"):
                self.rec.run(["pkill", "-KILL", "-x", name], label="kill a hung %s after the timeout" % name, timeout=20, sudo=True)
        return result

    def allow_trust_settings(self) -> CmdResult:
        """A throwaway runner only: let the admin trust-settings right be exercised without an authorization dialog."""
        return self.run_security(["authorizationdb", "write", "com.apple.trust-settings.admin", "allow"], "allow the admin trust-settings right without a prompt", 30)

    def trust_ca(self) -> Dict[str, Any]:
        assert self.ca is not None
        verify_argv = ["security", "verify-cert", "-c", str(self.ca["server_cert"]), "-p", "ssl", "-s", "github.com"]
        attempts: List[Dict[str, Any]] = []
        add_ok = False
        allow_rc: Optional[int] = None
        reused_trust = self.trusted
        if self.trusted:  # trusted by an earlier phase of this job: do not touch the trust store again, only check it still holds
            check = self.rec.run(verify_argv, label="Security.framework still trusts the CA of the earlier phase", timeout=60)
            if check.ok:
                add_ok = True
            else:
                self.trusted = False
        if not add_ok:
            argv = ["add-trusted-cert", "-d", "-r", "trustRoot", "-k", "/Library/Keychains/System.keychain", str(self.ca["ca_cert"])]
            for number, timeout in enumerate((60.0, 45.0, 45.0), 1):
                allow = self.allow_trust_settings()  # before EVERY attempt, the first included
                allow_rc = allow.rc
                add = self.run_security(argv, "trust the per-run CA in the System keychain (attempt %d)" % number, timeout)
                attempts.append({"attempt": number, "rc": add.rc, "timed_out": add.timed_out, "allow_rc": allow.rc, "out": tail_lines(add.text.strip(), 4)})
                if add.ok:
                    add_ok = True
                    break
                time.sleep(2)
        verify = self.rec.run(verify_argv, label="Security.framework verifies the github.com leaf", timeout=60)
        api_rc = None
        if not self.real_banner:
            api_rc = self.rec.run(["security", "verify-cert", "-c", str(self.ca["server_cert"]), "-p", "ssl", "-s", "api.github.com"],
                                  label="Security.framework verifies the api.github.com leaf", timeout=60).rc
        self.trusted = bool(add_ok and verify.ok)
        info = {"add_rc": attempts[-1]["rc"] if attempts else 0, "add_out": attempts[-1]["out"] if attempts else "reused: trusted by an earlier phase of this job",
                "attempts": attempts, "trusted_by_an_earlier_phase": reused_trust and not attempts, "authorizationdb_rc": allow_rc,
                "verify_github_rc": verify.rc, "verify_github_out": tail_lines(verify.text.strip(), 6), "verify_api_rc": api_rc, "trusted": self.trusted}
        self.facts["trust"] = info
        self.write_ca_state()
        return info

    def untrust_ca(self, state_ca_cert: Optional[str] = None) -> None:
        """Take the CA's trust away, bounded: a hung call is killed (run_security) and never retried; this runs at the end of a job."""
        cert = state_ca_cert or (str(self.ca["ca_cert"]) if self.ca is not None else None)
        if not self.trusted or cert is None:
            return
        self.run_security(["remove-trusted-cert", "-d", cert], "remove the trust setting of the per-run CA", 45)
        if self.ca_sha1:
            self.run_security(["delete-certificate", "-Z", self.ca_sha1, "/Library/Keychains/System.keychain"], "delete the per-run CA from the System keychain", 45)
        self.trusted = False
        if self.ca is not None:
            self.write_ca_state()

    # ---- hosts -----------------------------------------------------------------------------------------------
    def resolve_banner_api(self) -> List[str]:
        """The addresses api.github.com has RIGHT NOW (resolved on the runner, never hard-coded), to label the pass-through flows."""
        import socket
        found: List[str] = []
        try:
            for item in socket.getaddrinfo(BANNER_API_HOST, 443, proto=socket.IPPROTO_TCP):
                address = item[4][0]
                if address not in found:
                    found.append(address)
        except OSError as error:
            self.rec.note("could not resolve %s: %s" % (BANNER_API_HOST, error))
        self.banner_api_ips = sorted(found)
        self.facts["real_banner_api"]["api_addresses"] = self.banner_api_ips
        return self.banner_api_ips

    def map_hosts(self) -> Dict[str, Any]:
        if self.real_banner:
            self.resolve_banner_api()  # before the mapping; api.github.com is not mapped in this mode, but the order documents it
        backup = self.work / "hosts.before"
        shutil.copyfile("/etc/hosts", str(backup))
        shutil.copyfile("/etc/hosts", str(self.evidence / "hosts.before.txt"))
        self.hosts_backup = backup
        block = hosts_block(self.hosts)
        add = self.rec.run(["tee", "-a", "/etc/hosts"], label="map the GitHub names to loopback, ARC names to nowhere", timeout=30, sudo=True, input_text=block, quiet=True)
        self.hosts_on = add.ok
        self.rec.run(["dscacheutil", "-flushcache"], label="flush the directory-service cache", timeout=30, sudo=True)
        self.rec.run(["killall", "-HUP", "mDNSResponder"], label="flush mDNSResponder", timeout=30, sudo=True)
        shutil.copyfile("/etc/hosts", str(self.evidence / "hosts.mapped.txt"))
        resolved = {}
        for name in ("github.com", BANNER_API_HOST):
            query = self.rec.run(["dscacheutil", "-q", "host", "-a", "name", name], label="resolve " + name, timeout=30)
            resolved[name] = tail_lines(query.text.strip(), 6)

        def loopback(text: str) -> bool:
            return "127.0.0.1" in text or "::1" in text

        api_ok = (not loopback(resolved[BANNER_API_HOST]) and bool(re.search(r"ip(v6)?_address", resolved[BANNER_API_HOST]))) if self.real_banner else loopback(resolved[BANNER_API_HOST])
        views = self.resolution_views(tuple(dict.fromkeys(tuple(self.hosts) + self.extra_block_names + (BANNER_API_HOST,))))
        info = {"hosts_added_rc": add.rc, "resolved": resolved, "ok": add.ok and loopback(resolved["github.com"]) and api_ok,
                "api_github_com_left_to_real_dns": self.real_banner, "resolution_views": views}
        self.facts["hosts_mapped"] = list(self.hosts)
        self.facts["hosts"] = info
        return info

    def resolution_views(self, names: Sequence[str]) -> Dict[str, Any]:
        """For every name two views of what the resolvers answer after the mapping: dscacheutil (Directory Services) and the C library's
        getaddrinfo (what most programs use). Evidence for WHO honours /etc/hosts."""
        import socket
        views: Dict[str, Any] = {}
        for name in names:
            query = self.rec.run(["dscacheutil", "-q", "host", "-a", "name", name], label="resolution view (dscacheutil) " + name, timeout=30, quiet=True)
            libc: List[str] = []
            try:
                libc = sorted({canonical_ip(item[4][0]) for item in socket.getaddrinfo(name, 443, proto=socket.IPPROTO_TCP)})
            except OSError as error:
                libc = ["error: %s" % error]
            views[name] = {"dscacheutil": [canonical_ip(a) for a in re.findall(r"ip(?:v6)?_address:\s*(\S+)", query.text)], "getaddrinfo": libc}
        return views

    def unmap_hosts(self) -> None:
        if not self.hosts_on or self.hosts_backup is None:
            return
        self.rec.run(["cp", str(self.hosts_backup), "/etc/hosts"], label="restore /etc/hosts", timeout=30, sudo=True)
        self.rec.run(["dscacheutil", "-flushcache"], label="flush the directory-service cache", timeout=30, sudo=True)
        self.rec.run(["killall", "-HUP", "mDNSResponder"], label="flush mDNSResponder", timeout=30, sudo=True)
        self.hosts_on = False

    # ---- recording server ---------------------------------------------------------------------------------------
    def start_server(self, scenario: str, log_path: Path) -> None:
        assert self.ca is not None
        ready = self.work / ("ready-%s-%d.json" % (scenario, int(time.time() * 1000)))
        argv = ["sudo", "-n", sys.executable, str(LIB / "mitm_server.py"), "--scenario", scenario, "--cert", str(self.ca["server_cert"]),
                "--key", str(self.ca["server_key"]), "--listen", "127.0.0.1,::1:443", "--log", str(log_path), "--ready-file", str(ready)]
        server_out = open(str(self.work / ("mitm-%s.out" % scenario)), "a", encoding="utf-8")
        self._server_out = server_out
        self.server_proc = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=server_out, stderr=subprocess.STDOUT, start_new_session=True)
        self.rec.note("started the recording server (scenario %s, log %s)" % (scenario, log_path.name))
        deadline = time.time() + 25
        while time.time() < deadline:
            if ready.exists():
                try:
                    self.server_pid = int(json.loads(ready.read_text(encoding="utf-8")).get("pid"))
                except (ValueError, OSError, TypeError):
                    self.server_pid = None
                if self.server_pid:
                    return
            if self.server_proc.poll() is not None:
                break
            time.sleep(0.3)
        out = Path(server_out.name).read_text(encoding="utf-8", errors="replace") if Path(server_out.name).exists() else ""
        raise StepFailed("the recording server did not become ready: " + tail_lines(out.strip(), 8))

    def stop_server(self) -> None:
        if self.server_proc is None:
            return
        if self.server_pid:
            self.rec.run(["kill", "-TERM", str(self.server_pid)], label="stop the recording server", timeout=20, sudo=True)
        try:
            self.server_proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            if self.server_pid:
                self.rec.run(["kill", "-KILL", str(self.server_pid)], label="kill the recording server", timeout=20, sudo=True)
            self.server_proc.kill()
        # the log was written by root; make it readable and keep ownership consistent for the upload
        self.server_proc = None
        self.server_pid = None
        if self._server_out is not None:
            try:
                self._server_out.close()
            except OSError:
                pass
            self._server_out = None

    @contextlib.contextmanager
    def server(self, scenario: str, log_path: Path) -> Iterator[None]:
        self.start_server(scenario, log_path)
        try:
            yield
        finally:
            self.stop_server()
            self.rec.run(["chmod", "a+r", str(log_path)], label="make the server log readable", timeout=20, sudo=True)

    def self_test(self) -> Dict[str, Any]:
        """One request through the hosts mapping proves the names land on OUR server (Server header), not on GitHub."""
        assert self.ca is not None
        log = self.work / "requests-selftest.jsonl"
        info: Dict[str, Any] = {"landed_on_recorder": False}
        with self.server("latest-404", log):
            result = self.rec.run(["curl", "-sS", "--max-time", "15", "--cacert", str(self.ca["ca_cert"]), "-o", "/dev/null", "-D", "-", MANIFEST_URL],
                                  label="self-test: the manifest URL answers from the recorder", timeout=30)
            info.update({"rc": result.rc, "headers": tail_lines(result.text.strip(), 12)})
            info["landed_on_recorder"] = "wave0-lab-mitm" in result.text.lower()
        self.facts["self_test"] = info
        return info

    # ---- teardown ----------------------------------------------------------------------------------------------
    def teardown(self) -> None:
        if self.torn_down:
            return
        self.torn_down = True
        try:
            self.stop_server()
        except Exception as error:  # noqa: BLE001 - teardown must go on
            self.rec.note("teardown: stopping the server failed: %s" % error)
        steps = [self.unmap_hosts] + ([] if self.keep_ca else [self.untrust_ca])
        for step in steps:
            try:
                step()
            except Exception as error:  # noqa: BLE001
                self.rec.note("teardown: %s failed: %s" % (step.__name__, error))
        self.facts["ca_kept_trusted_for_the_next_phase"] = bool(self.keep_ca and self.trusted)
        if self.pf_enabled_by_us:
            self.rec.run(["pfctl", "-d"], label="disable pf again", timeout=30, sudo=True)
        if Path(self.evidence).is_dir():  # an atexit teardown after the evidence directory is gone must not create it again
            write_json(self.evidence / "isolation.json", dict(self.facts, finished=now_iso()))


# ------------------------------------------------------------------------------------------------------------------
# watching files and processes
# ------------------------------------------------------------------------------------------------------------------

class ProcessWatcher:
    """Polls `ps` while a case runs: every (pid, ppid, comm) seen is kept, so a short-lived installer would still show."""

    def __init__(self, rec: Recorder, interval: float = 0.5):
        self.rec = rec
        self.interval = interval
        self.seen: Dict[Tuple[int, int, str], float] = {}
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self.polls = 0

    def snapshot_rows(self) -> List[Dict[str, Any]]:
        try:
            done = subprocess.run(["ps", "-axo", "pid=,ppid=,comm="], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=20, check=False)
            return parse_ps(done.stdout.decode("utf-8", "replace"))
        except (OSError, subprocess.SubprocessError):
            return []

    def _run(self) -> None:
        while not self._stop.is_set():
            self.polls += 1
            for row in self.snapshot_rows():
                self.seen.setdefault((row["pid"], row["ppid"], row["comm"]), time.time())
            self._stop.wait(self.interval)

    def start(self) -> None:
        self._thread = threading.Thread(target=self._run, daemon=True, name="process-watcher")
        self._thread.start()

    def stop(self) -> List[Dict[str, Any]]:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=10)
        return [{"pid": pid, "ppid": ppid, "comm": comm, "first_seen": stamp} for (pid, ppid, comm), stamp in sorted(self.seen.items(), key=lambda item: item[1])]


def write_ps(path: Path, rows: Sequence[Dict[str, Any]]) -> None:
    Path(path).write_text("".join("%d %d %s\n" % (row["pid"], row["ppid"], row["comm"]) for row in rows), encoding="utf-8")


class FileRecorder:
    """Snapshots before/after and a poller during a case (lib/fswatch.py)."""

    def __init__(self, evidence: Path, stem: str, roots: Sequence[str], exclude: Sequence[str] = ()):
        if fswatch is None:
            raise StepFailed("lib/fswatch.py is missing")
        self.evidence = evidence
        self.stem = stem
        self.roots = [r for r in roots]
        self.exclude = tuple(exclude)
        self.before: Optional[Dict[str, Any]] = None
        self.poller = fswatch.Poller(self.roots, interval=0.25, out_path=str(evidence / ("writes-%s.jsonl" % stem)), exclude=self.exclude)

    def start(self) -> None:
        self.before = fswatch.snapshot(self.roots, self.exclude)
        write_json(self.evidence / ("fs-%s-before.json" % self.stem), self.before)
        self.poller.start()

    def stop(self) -> Dict[str, Any]:
        self.poller.stop()
        after = fswatch.snapshot(self.roots, self.exclude)
        write_json(self.evidence / ("fs-%s-after.json" % self.stem), after)
        events = self.poller.events()
        return {"diff": fswatch.diff(self.before or {}, after), "poller_scans": self.poller.scans, "poller_events": events, "roots": self.roots}


# ------------------------------------------------------------------------------------------------------------------
# tier: native check
# ------------------------------------------------------------------------------------------------------------------

def build_native(rec: Recorder, evidence: Path, timeout_s: float = 1500.0, background: bool = False):
    """cargo build --release --locked of wave0-lab-desktop/native-updater-check. Returns the binary path, or (Popen, path) in background mode."""
    crate = HERE / "native-updater-check"
    binary = crate / "target" / "release" / "native-updater-check"
    log = evidence / "cargo-build.log"
    argv = ["cargo", "build", "--release", "--locked", "--manifest-path", str(crate / "Cargo.toml")]
    env = dict(os.environ, CARGO_TERM_COLOR="never", CARGO_INCREMENTAL="0")
    rec.note("cargo build of native-updater-check starts (%s)" % ("in the background" if background else "in the foreground"))
    if background:
        handle = open(str(log), "w", encoding="utf-8")
        proc = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=handle, stderr=subprocess.STDOUT, env=env, start_new_session=True)
        return proc, binary
    result = rec.run(argv, label="build the real plugin checker", timeout=timeout_s, env=env, quiet=True)
    log.write_text(rec.mask(result.text), encoding="utf-8")
    return binary if result.ok and binary.exists() else None


def plugin_check_from_json(report: Optional[Dict[str, Any]]) -> Dict[str, Any]:
    if not report:
        return {"reached": False, "reason": "the checker printed no JSON report"}
    return {
        "reached": True, "outcome": report.get("outcome"), "error_kind": report.get("error_kind"), "error": report.get("error"),
        "update": report.get("update"), "download_attempted": bool(report.get("download_attempted")), "download_result": report.get("download_result"),
        "plugin": report.get("plugin"), "tauri": report.get("tauri"),
    }


def run_native_case(rec: Recorder, env: Interception, evidence: Path, work: Path, binary: Path, case: str, real_home: bool = False) -> Dict[str, Any]:
    scenario = SCENARIO_FOR_CASE[case]
    stem = "%s-native_check" % case
    sandbox = work / ("native-" + case)
    home, tmp = sandbox / "home", sandbox / "tmp"
    home.mkdir(parents=True, exist_ok=True)
    tmp.mkdir(parents=True, exist_ok=True)
    child_env = {"HOME": str(Path.home()) if real_home else str(home), "TMPDIR": str(tmp) + "/", "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "RUST_BACKTRACE": "0"}
    base_args = [str(binary), "--endpoint", MANIFEST_URL, "--current-version", APP_VERSION, "--pubkey", UPDATER_PUBKEY]
    files = FileRecorder(evidence, stem, [str(home), str(tmp)])
    watcher = ProcessWatcher(rec)
    log = evidence / ("requests-%s.jsonl" % stem)
    before_rows = watcher.snapshot_rows()
    write_ps(evidence / ("procs-%s-before.txt" % stem), before_rows)
    files.start()
    watcher.start()
    report: Optional[Dict[str, Any]] = None
    run = None
    try:
        with env.server(scenario, log):
            run = rec.run(base_args, label="native check, scenario %s" % scenario, timeout=120, env=child_env)
            report = parse_json_tail(run.out)
    finally:
        seen = watcher.stop()
        fs_result = files.stop()
    write_ps(evidence / ("procs-%s-after.txt" % stem), watcher.snapshot_rows())
    write_ps(evidence / ("procs-%s-seen.txt" % stem), seen)
    plugin_check = plugin_check_from_json(report)
    positive = None
    control_files: List[str] = []
    if scenario.startswith("bait"):
        control_log = evidence / ("requests-%s-control.jsonl" % stem)
        with env.server(scenario, control_log):
            control = rec.run(base_args + ["--control-download"], label="POSITIVE CONTROL: the same check with --control-download", timeout=120, env=child_env)
        control_report = parse_json_tail(control.out)
        rows = read_jsonl(control_log) or []
        seen_payload = [row for row in rows if row.get("kind") == "request" and (row.get("payload") or PAYLOAD_PATH_RE.search(str(row.get("path") or "")))]
        positive = {"observed": bool(seen_payload) and bool(control_report and control_report.get("download_attempted")),
                    "payload_requests": [[row.get("host"), row.get("path")] for row in seen_payload][:4], "report": control_report}
        control_files.append(control_log.name)
    rows = read_jsonl(log)
    fs_record = dict(fs_result, expected_prefixes=[], temp_roots=[str(tmp)])
    procs = {"seen": seen, "app_pid": None, "bundle_path": ""}
    evaluated = evaluate_case("native_check", scenario, rows, fs_record, procs, None, plugin_check, positive)
    entry = {
        "name": case, "tier": "native_check", "scenario": scenario,
        "trigger_outcome": dict(plugin_check, trigger="native-updater-check (real tauri-plugin-updater %s check(), Security.framework trust)" % PLUGIN_VERSION, rc=run.rc if run else None,
                                home="real HOME (--native-real-home)" if real_home else "sandbox HOME"),
        "requests": summarize_requests(rows), "criteria": evaluated["criteria"], "criteria_reasons": evaluated["reasons"], "notes": evaluated["notes"],
        "positive_control": positive, "file_writes": {"expected_prefixes": [], "poller_scans": fs_result["poller_scans"], "poller_events": len(fs_result["poller_events"]),
                                                      "added": len(fs_result["diff"]["added"]), "changed": len(fs_result["diff"]["changed"])},
        "evidence_files": sorted([log.name, "writes-%s.jsonl" % stem, "fs-%s-before.json" % stem, "fs-%s-after.json" % stem,
                                  "procs-%s-before.txt" % stem, "procs-%s-after.txt" % stem, "procs-%s-seen.txt" % stem] + control_files),
        "verdict": evaluated["verdict"],
    }
    return entry


# ------------------------------------------------------------------------------------------------------------------
# tier: released app
# ------------------------------------------------------------------------------------------------------------------

def osascript_json(rec: Recorder, script: str, label: str, timeout: float = 150.0) -> Tuple[Optional[Dict[str, Any]], CmdResult]:
    """Run a JXA script from a file (osascript -l JavaScript FILE) and parse the JSON line it prints."""
    handle = tempfile.NamedTemporaryFile("w", suffix=".js", prefix="wave0-jxa-", delete=False, encoding="utf-8")
    try:
        handle.write(script + "\n")
    finally:
        handle.close()
    result = rec.run(["osascript", "-l", "JavaScript", handle.name], label=label, timeout=timeout, quiet=True)
    return parse_json_tail(result.out), result


def screenshot(rec: Recorder, evidence: Path, name: str) -> None:
    rec.run(["screencapture", "-x", "-t", "png", str(evidence / ("screenshot-%s.png" % name))], label="screenshot " + name, timeout=30)


class CaptureWatcher:
    """A text capture of DNS and HTTPS packets (tcpdump -n -tt -l [-k NP]) while a case runs. Darwin's pktap interface names the
    process of every packet. The raw capture stays on the runner; only the per-flow summary goes into the evidence."""

    def __init__(self, rec: Recorder, path: Path):
        self.rec = rec
        self.path = path
        self.proc: Optional[subprocess.Popen] = None
        self.handle: Any = None
        self.iface: Optional[str] = None
        self.error: Optional[str] = None

    def start(self) -> bool:
        for iface, extra in (("pktap,all", ["-k", "NP"]), ("any", []), ("en0", [])):
            argv = ["sudo", "-n", "tcpdump", "-i", iface, "-n", "-tt", "-l"] + extra + ["port 53 or port 443 or port 80"]
            try:
                handle = open(str(self.path), "w", encoding="utf-8")
                proc = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=handle, stderr=subprocess.STDOUT, start_new_session=True)
            except OSError as error:
                self.error = "%s: %s" % (type(error).__name__, error)
                continue
            time.sleep(1.5)
            if proc.poll() is None:
                self.proc, self.handle, self.iface = proc, handle, iface
                self.rec.note("tcpdump capture started on %s" % iface)
                return True
            handle.close()
            self.error = tail_lines(self.path.read_text(encoding="utf-8", errors="replace").strip(), 3) if self.path.exists() else "tcpdump exited"
        self.rec.note("tcpdump could not be started: %s" % self.error)
        return False

    def stop(self) -> str:
        if self.proc is not None:
            self.rec.run(["pkill", "-INT", "-x", "tcpdump"], label="stop the tcpdump capture", timeout=20, sudo=True)
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.rec.run(["pkill", "-KILL", "-x", "tcpdump"], label="kill the tcpdump capture", timeout=20, sudo=True)
            if self.handle is not None:
                self.handle.close()
        try:
            return self.path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return ""


class LsofWatcher:
    """`lsof -nP -i` once a second while a case runs: every remote endpoint any process held, with its pid, command and TCP state."""

    def __init__(self, interval: float = 1.0, resolve_host: Optional[str] = None):
        self.interval = interval
        self.resolve_host = resolve_host
        self.resolved: List[str] = []
        self.rows: List[Dict[str, Any]] = []
        self.polls = 0
        self.errors: List[str] = []
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None

    def poll_once(self) -> None:
        try:
            done = subprocess.run(["lsof", "-nP", "-i", "-F", "pcfnT"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=20, check=False)
            text = done.stdout.decode("utf-8", "replace")
            if done.returncode not in (0, 1):
                self.errors.append("lsof rc %s: %s" % (done.returncode, done.stderr.decode("utf-8", "replace")[:120]))
        except (OSError, subprocess.SubprocessError) as error:
            self.errors.append("%s: %s" % (type(error).__name__, error))
            text = ""
        self.polls += 1
        stamp = time.time()
        for endpoint in parse_lsof_f(text):
            endpoint["t"] = stamp
            self.rows.append(endpoint)
        if self.resolve_host and self.polls % 5 == 1:  # the address pool of api.github.com rotates: keep every address the resolver hands out
            import socket
            try:
                for item in socket.getaddrinfo(self.resolve_host, 443, proto=socket.IPPROTO_TCP):
                    if item[4][0] not in self.resolved:
                        self.resolved.append(item[4][0])
            except OSError:
                pass

    def _run(self) -> None:
        while not self._stop.is_set():
            self.poll_once()
            self._stop.wait(self.interval)

    def start(self) -> None:
        self._thread = threading.Thread(target=self._run, daemon=True, name="lsof-watcher")
        self._thread.start()

    def stop(self) -> List[Dict[str, Any]]:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=25)
        return list(self.rows)


def write_masked_json(rec: Recorder, path: Path, value: Any) -> None:
    Path(path).write_text(rec.mask(json.dumps(value, indent=2, sort_keys=True, default=str)) + "\n", encoding="utf-8")


def ax_visible_texts(dump: Optional[Dict[str, Any]]) -> List[str]:
    return ax_texts((dump or {}).get("nodes") or [], 400)


CARD_TEXT = re.compile(r"(?i)^updates?$|latest version|is available|update failed|install|^v[0-9A-Za-z.\-]+$|no update")


def banner_card_texts(dump: Optional[Dict[str, Any]]) -> List[str]:
    """The texts of the Updates card the banner call drives: the card title, the version pill ("v0.7.12", or "vUNKNOWN" when the
    API answered without a tag) and the sentence below it, plus any button or error text."""
    return [text for text in ax_visible_texts(dump) if CARD_TEXT.search(text)][:14]


def run_app_case(rec: Recorder, env: Interception, evidence: Path, work: Path, app: Path, facts: Dict[str, Any], case: str, probe: bool = False) -> Dict[str, Any]:
    """Launch the released app under interception and drive Settings > Check for updates > Install. See the module docstring:
    in real-banner mode the one banner request reaches api.github.com and the Install button drives the plugin check(); with the
    mode off the case is a labelled negative control. Both cases use scenario latest-404 (the world after the flip); the second
    case starts from the state the first one left in the shared sandbox HOME."""
    scenario = APP_SCENARIO
    stem = "%s-released_app" % case
    sandbox = work / "app-shared"
    home, tmp = sandbox / "home", sandbox / ("tmp-" + case)
    support = home / "Library" / "Application Support" / APP_IDENTIFIER
    support.mkdir(parents=True, exist_ok=True)
    tmp.mkdir(parents=True, exist_ok=True)
    seeded = not (support / "store.json").exists()
    if seeded:  # as a finished onboarding leaves it; the second case finds the first case's state instead
        (support / "store.json").write_text(json.dumps(store_json(home), indent=2), encoding="utf-8")
    real_home = str(Path.home())
    library = Path(real_home) / "Library"
    # Only places the app (or its autostart registration) may write: the sandbox HOME and TMPDIR the process was given, the app's
    # own identifier directories in the real ~/Library (WebKit ignores HOME), the LaunchAgents folder, ~/.arc and the user folders
    # an installer would use. System-wide trees (/tmp, $TMPDIR of the runner, ~/Library/Preferences, ~/Library/Logs) are not
    # watched: other processes write there all the time and would drown the signal.
    roots = [str(home), str(tmp), str(app)]
    roots += [str(library / sub / APP_IDENTIFIER) for sub in ("Application Support", "Caches", "WebKit", "Logs", "HTTPStorages", "Saved Application State")]
    roots += [str(library / "LaunchAgents"), str(Path(real_home) / ".arc"), str(Path(real_home) / "Downloads"), str(Path(real_home) / "Desktop")]
    roots = [r for r in dict.fromkeys(roots)]
    bundle_files = FileRecorder(evidence, stem + "-bundle", [str(app)])
    files = FileRecorder(evidence, stem, [r for r in roots if r != str(app)])
    watcher = ProcessWatcher(rec)
    capture = CaptureWatcher(rec, work / ("tcpdump-%s.txt" % stem))
    lsof = LsofWatcher(resolve_host=BANNER_API_HOST if env.real_banner else None)
    log = evidence / ("requests-%s.jsonl" % stem)
    before_rows = watcher.snapshot_rows()
    write_ps(evidence / ("procs-%s-before.txt" % stem), before_rows)
    ui: Dict[str, Any] = {"attempted": True, "trigger": "System Events (JXA) UI scripting: Settings > Check for updates > Install", "steps": [], "state_seeded": seeded,
                          "real_banner_api": env.real_banner}
    proc: Optional[subprocess.Popen] = None
    app_handle: Any = None
    app_log = evidence / ("app-%s.log" % stem)
    bundle_result: Dict[str, Any] = {}
    fs_result: Dict[str, Any] = {}
    seen: List[Dict[str, Any]] = []
    capture_text = ""
    endpoints: List[Dict[str, Any]] = []
    launched_at = time.time()

    def dump_step(label: str, name: str, key: str) -> Optional[Dict[str, Any]]:
        dump, result = osascript_json(rec, jxa_ax_dump(proc.pid), label)  # type: ignore[union-attr]
        write_json(evidence / ("ax-%s-%s.json" % (stem, name)), dump if dump else {"raw": rec.mask(result.text)})
        ui[key] = summarize_ax(dump)
        return dump

    try:
        bundle_files.start()
        files.start()
        watcher.start()
        capture.start()
        lsof.start()
        with env.server(scenario, log):
            try:
                refresh = env.refresh_extra_blocks()  # the addresses of rsms.me as the real resolver gives them now
                ui["extra_block_refresh"] = refresh
                if refresh.get("verified") is False:
                    raise StepFailed("the pf block could not be verified after adding addresses: the app is not launched")
                binary = Path(facts["binary"])
                child_env = {"HOME": str(home), "TMPDIR": str(tmp) + "/", "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "RUST_LOG": "info", "LANG": "en_US.UTF-8"}
                app_handle = open(str(app_log), "w", encoding="utf-8")
                launched_at = time.time()
                proc = subprocess.Popen([str(binary)], env=child_env, stdin=subprocess.DEVNULL, stdout=app_handle, stderr=subprocess.STDOUT, cwd=str(home), start_new_session=True)
                rec.note("launched the released app (pid %d) with HOME=%s" % (proc.pid, home))
                ui["pid"] = proc.pid
                window = None
                end = time.time() + 90
                while time.time() < end and proc.poll() is None:
                    dump, result = osascript_json(rec, jxa_ax_dump(proc.pid, max_nodes=1, budget_ms=15000), "wait for the app window", timeout=40)
                    if dump and dump.get("windows", 0) >= 1:
                        window = dump
                        break
                    time.sleep(3)
                ui["window_appeared"] = window is not None
                if proc.poll() is not None:
                    ui["problem"] = "the app exited with rc %s before a window appeared" % proc.poll()
                elif window is None:
                    ui["problem"] = "no window appeared within 90 s (System Events saw none)"
                if probe or window is not None:
                    screenshot(rec, evidence, "%s-after-launch" % case)
                if window is not None:
                    time.sleep(4)
                    dump_step("AX dump after launch", "1-launch", "after_launch")
                    ui["steps"].append(click_step(rec, proc.pid, SETTINGS_PATTERNS, "click Settings"))
                    time.sleep(3)
                    dump_step("AX dump on the Settings page", "2-settings", "settings_page")
                    ui["install_button_before_check"] = bool((ui.get("settings_page") or {}).get("install_button"))
                    attempts: List[Dict[str, Any]] = []
                    dump = None
                    result = CmdResult(["osascript"], 0, "", "", 0.0)
                    for attempt in range(1, 5):  # the first click and up to three more, 20 s apart: the banner call may have been rate limited
                        if attempt > 1:
                            time.sleep(20)
                        ui["steps"].append(click_step(rec, proc.pid, CHECK_PATTERNS, "click Check for updates" if attempt == 1 else "click Check for updates (retry %d)" % (attempt - 1)))
                        dump = None
                        for _ in range(7):  # the banner command has an 8 s timeout; with the real API it answers in well under a second
                            time.sleep(4)
                            dump, result = osascript_json(rec, jxa_ax_dump(proc.pid), "AX dump after Check for updates")
                            if summarize_ax(dump).get("install_button"):
                                break
                        found = bool(summarize_ax(dump).get("install_button"))
                        attempts.append({"attempt": attempt, "install_button": found, "card_texts": banner_card_texts(dump)})
                        if found:
                            break
                    ui["banner_attempts"] = attempts
                    write_json(evidence / ("ax-%s-3-after-check.json" % stem), dump if dump else {"raw": rec.mask(result.text)})
                    ui["after_check"] = summarize_ax(dump)
                    ui["install_button"] = bool(ui["after_check"].get("install_button"))
                    if ui["install_button"] and ui["install_button_before_check"]:
                        ui["problem"] = "the Install button was already present BEFORE the check: not clicked (it would not prove the banner's answer)"
                    elif ui["install_button"]:
                        ui["steps"].append(click_step(rec, proc.pid, INSTALL_PATTERNS, "click Install (calls the plugin check())"))
                        ui["install_clicked"] = bool((ui["steps"][-1].get("report") or {}).get("clicked") or (ui["steps"][-1].get("retry_report") or {}).get("clicked"))
                        final = None
                        for _ in range(8):
                            time.sleep(4)
                            final, result = osascript_json(rec, jxa_ax_dump(proc.pid), "AX dump after Install")
                            if extract_update_error(ax_visible_texts(final)) or "No update available" in " ".join(ax_visible_texts(final)):
                                break
                        write_json(evidence / ("ax-%s-4-after-install.json" % stem), final if final else {"raw": rec.mask(result.text)})
                        ui["after_install"] = summarize_ax(final)
                        ui["ui_error_text"] = extract_update_error(ax_visible_texts(final))
                    screenshot(rec, evidence, "%s-final" % case)
            finally:
                if proc is not None and proc.poll() is None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                if app_handle is not None:
                    app_handle.close()
    except Exception as error:  # noqa: BLE001 - partial evidence is still evidence
        ui["problem"] = "%s: %s" % (type(error).__name__, error)
        rec.note("released-app case %s stopped: %s" % (case, ui["problem"]))
    finally:
        seen = watcher.stop()
        endpoints = lsof.stop()
        capture_text = capture.stop()
        for stopper, holder in ((bundle_files, "bundle"), (files, "fs")):
            try:
                result_part = stopper.stop()
            except Exception as error:  # noqa: BLE001
                result_part = {"diff": None, "poller_scans": 0, "poller_events": [], "error": str(error)}
            if holder == "bundle":
                bundle_result = result_part
            else:
                fs_result = result_part
    write_ps(evidence / ("procs-%s-after.txt" % stem), watcher.snapshot_rows())
    write_ps(evidence / ("procs-%s-seen.txt" % stem), seen)
    rows = read_jsonl(log)

    # ---- what the processes talked to ------------------------------------------------------------------
    packets = parse_tcpdump_text(capture_text)
    api_ips = set(env.banner_api_ips) | set(lsof.resolved)
    for packet in packets:
        if packet.get("dns_query") == BANNER_API_HOST:
            api_ips.update(packet.get("dns_answers") or [])
    api_ips = sorted(api_ips) if env.real_banner else []
    flows = summarize_flows(packets, api_ips, env.blocked_ips) if (capture.proc is not None or packets) else None
    pids = related_pids(seen, ui.get("pid"))
    network = network_report(endpoints, flows, pids, api_ips, env.blocked_ips, env.real_banner)
    network.update({
        "capture": {"interface": capture.iface, "error": capture.error if capture.proc is None else None, "packets": len(packets), "flows": (flows or {}).get("flows", []),
                    "dns_names": (flows or {}).get("dns_names", {}), "raw_capture_kept": False},
        "lsof": {"polls": lsof.polls, "errors": lsof.errors[:3]}, "banner_api_addresses": api_ips,
        "banner_api_addresses_sources": "resolved on the runner before the mapping, re-resolved every few seconds during the case, and the DNS answers tcpdump saw (never hard-coded)",
        "statement": REAL_BANNER_SCOPE if env.real_banner else "real-banner mode off: no request was allowed to leave the sandbox",
    })
    write_masked_json(rec, evidence / ("network-%s.json" % stem), network)

    after_check = ui.get("after_check") or {}
    ui["install_button"] = bool(after_check.get("install_button"))
    ui["check_button"] = bool((ui.get("settings_page") or {}).get("check_for_updates_button"))
    ui["settings_button"] = bool((ui.get("after_launch") or {}).get("settings_button"))
    ui["nodes"] = (ui.get("after_launch") or {}).get("nodes")
    ui["pf_counters"] = env.live_block_counters()
    manifest_rows = [row for row in (rows or []) if row.get("kind") == "request" and row.get("role") == "manifest"]
    message = ui.get("ui_error_text")
    if ui.get("install_clicked") and manifest_rows:
        plugin_check: Dict[str, Any] = {"reached": True, "outcome": "error" if message else None, "error_kind": infer_error_kind(message),
                                        "error": message, "source": "UI text of the Updates card (data-testid update-error)"}
    else:
        last_texts = ((ui.get("banner_attempts") or [{}])[-1]).get("card_texts") or []
        banner_text = " ".join(last_texts)
        clicks = len(ui.get("banner_attempts") or [])
        if ui["install_button"] and ui.get("install_button_before_check"):
            reason = "the Install button was already present before the check; it was not clicked"
        elif ui["install_button"]:
            reason = "the Install button was rendered but not clicked, or the recorder saw no manifest request"
        elif not env.real_banner:
            reason = "the Install button was not rendered: Settings.tsx shows it only after check_for_update reports an update, and with real-banner mode off that call cannot succeed"
        elif re.search(r"(?i)vunknown|\bunknown\b", banner_text):
            reason = ("the banner's own unintercepted API call returned no release tag (UI: %s); not attributable from here (after %d Check for updates click(s) 20 s apart)"
                      % (banner_text, clicks))
        else:
            reason = "the Install button was not rendered after %d Check for updates click(s) (UI: %s); not attributable from here" % (clicks, banner_text or "no card text found")
        plugin_check = {"reached": False, "reason": reason}
    expected = expected_prefixes(real_home, str(home), case)
    fs_record = dict(fs_result, expected_prefixes=expected, temp_roots=[str(tmp)])
    procs = {"seen": seen, "app_pid": ui.get("pid"), "bundle_path": str(app)}
    evaluated = evaluate_case("released_app", scenario, rows, fs_record, procs, bundle_result.get("diff"), plugin_check, None, network, require_network=env.real_banner)
    entry = {
        "name": case, "tier": "released_app", "scenario": scenario,
        "trigger_outcome": {"trigger": ui["trigger"], "ui": {k: v for k, v in ui.items() if k != "pf_counters"}, "plugin_check": plugin_check,
                            "note": ("real-banner mode: one unauthenticated read-only GET to api.github.com reached the real Internet (not recorded by us); everything else stayed on the recorder"
                                     if env.real_banner else "NEGATIVE CONTROL: the plugin check() is not reachable from the released macOS app without the non-interceptable banner call succeeding")
                            + "; the bait scenario is exercised at plugin level only (the app's Install handler downloads by design when an update exists)"},
        "requests": summarize_requests(rows), "criteria": evaluated["criteria"], "criteria_reasons": evaluated["reasons"], "notes": evaluated["notes"] + evaluated.get("info", []),
        "network": {"violations": network["violations"], "allowed_banner_endpoints": network["allowed_banner_endpoints"], "blocked_live_node_attempts": len(network["blocked_live_node_attempts"]),
                    "recorded": network["recorded"], "file": "network-%s.json" % stem},
        "file_writes": {"expected_prefixes": expected, "poller_scans": fs_result.get("poller_scans"), "poller_events": len(fs_result.get("poller_events") or []),
                        "added": len((fs_result.get("diff") or {}).get("added") or []), "changed": len((fs_result.get("diff") or {}).get("changed") or [])},
        "evidence_files": sorted([log.name, "writes-%s.jsonl" % stem, "fs-%s-before.json" % stem, "fs-%s-after.json" % stem, "network-%s.json" % stem,
                                  "procs-%s-before.txt" % stem, "procs-%s-after.txt" % stem, "procs-%s-seen.txt" % stem, app_log.name]),
        "verdict": evaluated["verdict"],
    }
    write_masked_json(rec, evidence / ("ui-%s.json" % stem), ui)
    return entry


def click_step(rec: Recorder, pid: int, patterns: Sequence[str], label: str) -> Dict[str, Any]:
    """Click the first matching button; if there is none, retry once on any element role (a label inside a button)."""
    report, result = osascript_json(rec, jxa_click(pid, patterns), label, timeout=150)
    step = {"label": label, "report": report, "rc": result.rc, "classification": classify_osascript_error(result.text, result.rc)}
    if report is not None and not report.get("clicked") and report.get("error") == "no matching element":
        report, result = osascript_json(rec, jxa_click(pid, patterns, roles=AX_ANY_ROLES), label + " (any element role)", timeout=150)
        step.update({"retry_report": report, "retry_rc": result.rc})
    if report is None:
        step["raw"] = rec.mask(tail_lines(result.text.strip(), 6))
    return step


# ------------------------------------------------------------------------------------------------------------------
# result assembly
# ------------------------------------------------------------------------------------------------------------------

def runner_facts(system: Dict[str, Any]) -> Dict[str, Any]:
    return {"image": system.get("image"), "arch": system.get("machine"), "os_version": system.get("mac_ver")}


def assemble_result(system: Dict[str, Any], app_info: Optional[Dict[str, Any]], bundle: Optional[Dict[str, Any]], provenance: Optional[Dict[str, Any]],
                    tiers: Dict[str, Dict[str, Any]], cases: List[Dict[str, Any]], manifest_error: Optional[str], manifest_source: Optional[str],
                    isolation: Dict[str, Any], problems: List[str]) -> Dict[str, Any]:
    machine = str(system.get("machine") or "")
    verdict, per_tier = overall_verdict(tiers, cases)
    return {
        "schema": SCHEMA_RESULT,
        "os": "macos-arm64" if machine in ("arm64", "aarch64") else "macos-intel",
        "runner": runner_facts(system),
        "app": {
            "tag": APP_TAG, "asset": (app_info or {}).get("name"), "asset_sha256": (app_info or {}).get("sha256"),
            "release_digest": (app_info or {}).get("release_digest"), "digest_match": (app_info or {}).get("digest_match"),
            "version_reported": (bundle or {}).get("version"), "bundle_identifier": (bundle or {}).get("identifier"), "source": (app_info or {}).get("source"),
        },
        "plugin": {"version": PLUGIN_VERSION, "provenance": (provenance or {}).get("crates_in_binary", []), "plugin_pinned": (provenance or {}).get("plugin_pinned")},
        "tiers": tiers,
        "tier_verdicts": per_tier,
        "cases": cases,
        "manifest404_error_text": manifest_error,
        "manifest404_error_source": manifest_source,
        "isolation": {
            "hosts_mapped": isolation.get("hosts_mapped", []), "ca_sha256": isolation.get("ca_sha256"),
            "live_block": isolation.get("live_block"), "trust": isolation.get("trust"), "self_test": isolation.get("self_test"),
            "real_banner_api": isolation.get("real_banner_api"),
            "interception_scope": "the updater plugin's check/download/install path (OS trust store); the app's banner check_for_update and ensure_binary use bundled webpki roots and are NOT interceptable: their handshakes show as tls_failure rows",
        },
        "problems": problems,
        "verdict": verdict,
        "finished": now_iso(),
    }


def not_run_tier(reason: str, result: str = "not_attempted", trigger: Optional[str] = None) -> Dict[str, Any]:
    return {"attempted": result != "not_attempted", "trigger": trigger, "result": result, "reason": reason}


# ------------------------------------------------------------------------------------------------------------------
# subcommands
# ------------------------------------------------------------------------------------------------------------------

def shared_dir() -> Path:
    """Where the probe and run phases of one job share their CA (its private keys never leave this directory)."""
    base = Path(os.environ.get("RUNNER_TEMP") or tempfile.gettempdir())
    path = base / "wave0-desktop-macos-shared"
    path.mkdir(parents=True, exist_ok=True)
    return path


def work_dir() -> Path:
    base = Path(os.environ.get("RUNNER_TEMP") or tempfile.gettempdir())
    path = base / ("wave0-desktop-macos-%d" % os.getpid())
    path.mkdir(parents=True, exist_ok=True)
    return path


def prepare_app(rec: Recorder, evidence: Path, work: Path, system: Dict[str, Any]) -> Tuple[Path, Dict[str, Any], Dict[str, Any], Dict[str, Any]]:
    """Real-GitHub reads (BEFORE the hosts mapping): release metadata, the digest-verified download, extraction, provenance."""
    release = fetch_release(rec)
    assets = select_mac_assets(release, str(system["machine"]))
    app, info = extract_app(rec, assets, work / "downloads", work / "sandbox")
    facts = bundle_facts(rec, app)
    provenance = collect_provenance(rec, Path(facts["binary"]), evidence)
    return app, info, facts, provenance


def ensure_tag(rec: Recorder) -> Dict[str, Any]:
    """Make the v0.7.11 tag readable for the source citations. A tag that is already there is left alone; --depth=1 is used ONLY
    when the checkout is already shallow (the CI checkout is): in a complete repository --depth would make it shallow."""
    have = rec.run(["git", "-C", str(ROOT), "rev-parse", "-q", "--verify", "refs/tags/%s^{commit}" % APP_TAG], label="is the %s tag already here" % APP_TAG, timeout=30)
    if have.ok and have.out.strip():
        return {"fetched": False, "commit": have.out.strip()}
    shallow = rec.run(["git", "-C", str(ROOT), "rev-parse", "--is-shallow-repository"], label="is this checkout shallow", timeout=30).out.strip() == "true"
    argv = ["git", "-C", str(ROOT), "fetch", "--no-tags"] + (["--depth=1"] if shallow else []) + ["origin", "refs/tags/%s:refs/tags/%s" % (APP_TAG, APP_TAG)]
    done = rec.run(argv, label="fetch the %s tag (read-only; for the source citations)" % APP_TAG, timeout=120)
    return {"fetched": done.ok, "shallow_checkout": shallow, "rc": done.rc}


def refuse_outside_ci(command: str) -> Optional[int]:
    """probe and run change /etc/hosts, the System keychain, pf and drive the GUI: they belong on a throwaway runner only."""
    if os.environ.get("GITHUB_ACTIONS") == "true":
        return None
    print("os_macos.py %s refuses to run outside a GitHub Actions runner (it changes /etc/hosts, the keychain and pf and drives the GUI); "
          "GITHUB_ACTIONS is not 'true'" % command, file=sys.stderr, flush=True)
    return 2


def guarded(label: str, fn, rec: Optional[Recorder] = None, default: Any = None) -> Any:
    """Run one probe step; whatever it raises is recorded as {"ok": False, "error": ...} and the next step still runs."""
    try:
        return fn()
    except Exception as error:  # noqa: BLE001 - the probe never fails the job
        if rec is not None:
            rec.note("%s failed: %s: %s" % (label, type(error).__name__, error))
        return default if default is not None else {"ok": False, "error": "%s: %s" % (type(error).__name__, error)}


def cmd_probe(args: argparse.Namespace) -> int:
    """Capability and Accessibility probe. NEVER raises: probe.json, accessibility.json and feasibility.txt are always written."""
    evidence = Path(args.evidence)
    evidence.mkdir(parents=True, exist_ok=True)
    rec = Recorder(evidence)
    deadline = Deadline(float(args.budget_min) * 60)
    probe: Dict[str, Any] = {"schema": SCHEMA_PROBE, "started": now_iso(), "steps": {}, "real_banner_api": bool(args.real_banner_api),
                             "arch_requested": getattr(args, "arch", None), "arch_actual": platform.machine(),
                             "arch_matches": arch_matches(getattr(args, "arch", None), platform.machine())}
    env: Optional[Interception] = None
    native_proc = None
    native_binary: Optional[Path] = None
    work: Optional[Path] = None

    def write_outputs() -> None:
        probe["finished"] = now_iso()
        probe["feasibility"] = guarded("feasibility", lambda: build_feasibility(probe), rec, default="feasibility could not be built (see probe.json)\n")
        write_json(evidence / "probe.json", probe)
        (evidence / "feasibility.txt").write_text(str(probe["feasibility"]), encoding="utf-8")

    try:
        work = work_dir()
        probe["system"] = guarded("system facts", lambda: system_facts(rec), rec)
        live_ips: List[str] = []
        try:
            live_ips = load_live_ips()
            rec.live_ips = live_ips
            probe["steps"]["live_addresses"] = {"ok": True, "count": len(live_ips)}
        except (OSError, ValueError) as error:
            probe["steps"]["live_addresses"] = {"ok": False, "error": str(error)}
        probe["accessibility"] = guarded("accessibility tests", lambda: accessibility_tests(rec, evidence), rec, default={"summary": {}, "tests": {}})
        if not (evidence / "accessibility.json").exists():
            write_json(evidence / "accessibility.json", probe["accessibility"])
        if not args.no_build:
            try:
                native_proc, native_binary = build_native(rec, evidence, background=True)
            except OSError as error:
                probe["native_check"] = {"status": "build not started: %s" % error}
        env = Interception(rec, evidence, work, live_ips, real_banner=bool(args.real_banner_api), shared_dir=shared_dir(), keep_ca=bool(args.keep_ca))
        sudo = isinstance(probe["system"], dict) and (probe["system"].get("sudo") or {}).get("rc") == 0
        probe["steps"]["sudo"] = {"ok": bool(sudo)}
        if sudo and live_ips:
            probe["steps"]["pf"] = guarded("pf block", env.block_live_network, rec)
        app = None
        facts: Dict[str, Any] = {}
        try:
            ensure_tag(rec)
            app, info, facts, provenance = prepare_app(rec, evidence, work, probe["system"] if isinstance(probe["system"], dict) else {"machine": platform.machine()})
            probe["steps"]["release_download"] = {"ok": True, "asset": info, "bundle": {k: facts.get(k) for k in ("version", "identifier", "executable")},
                                                  "provenance_plugin_pinned": provenance.get("plugin_pinned")}
        except (StepFailed, AssetError, OSError, KeyError) as error:
            probe["steps"]["release_download"] = {"ok": False, "error": str(error)}
            app, facts = None, {}
        except Exception as error:  # noqa: BLE001
            probe["steps"]["release_download"] = {"ok": False, "error": "%s: %s" % (type(error).__name__, error)}
            app, facts = None, {}
        if sudo:
            ca = guarded("CA", lambda: env.make_ca() and {"ok": True}, rec)
            probe["steps"]["ca"] = ca
            if ca.get("ok"):
                probe["steps"]["trust"] = guarded("trust", env.trust_ca, rec)
                probe["steps"]["hosts"] = guarded("hosts", env.map_hosts, rec)
                probe["steps"]["self_test"] = guarded("self test", env.self_test, rec)
                probe["steps"]["port_443"] = {"ok": bool((probe["steps"]["self_test"] or {}).get("landed_on_recorder"))}
        steps = probe["steps"]
        ready = bool(app is not None and sudo and live_ips and (steps.get("pf") or {}).get("verified") and (steps.get("trust") or {}).get("trusted")
                     and (steps.get("hosts") or {}).get("ok") and (steps.get("self_test") or {}).get("landed_on_recorder"))
        if ready and deadline.left() > 120:
            entry = guarded("released app", lambda: run_app_case(rec, env, evidence, work, app, facts, "clean", probe=True), rec, default={"error": True})
            if "trigger_outcome" in entry:
                ui = (entry.get("trigger_outcome") or {}).get("ui") or {}
                net = entry.get("network") or {}
                requests_summary = entry.get("requests") or {}
                probe["released_app_ui"] = {
                    "attempted": True, "real_banner_api": ui.get("real_banner_api"), "window_appeared": ui.get("window_appeared"), "nodes": ui.get("nodes"),
                    "settings_button": ui.get("settings_button"), "check_button": ui.get("check_button"), "install_button": ui.get("install_button"),
                    "install_clicked": ui.get("install_clicked"), "ui_error_text": ui.get("ui_error_text"), "problem": ui.get("problem"),
                    "banner_attempts": ui.get("banner_attempts"),
                    "banner_ui_text": " ".join(((ui.get("banner_attempts") or [{}])[-1]).get("card_texts") or []),
                    "install_button_before_check": ui.get("install_button_before_check"),
                    "manifest_requests": sum(count for host, path, count in (requests_summary.get("by_host_path") or []) if path == MANIFEST_PATH),
                    "network_recorded": net.get("recorded"), "network_violations": [
                        "%s:%s" % (item.get("remote_ip"), item.get("remote_port")) for item in net.get("violations", [])],
                    "requests": requests_summary, "criteria": entry.get("criteria"), "case_verdict": entry.get("verdict"),
                }
            else:
                probe["released_app_ui"] = {"attempted": True, "problem": "the released-app case crashed: %s" % entry.get("error")}
        else:
            probe["released_app_ui"] = {"attempted": False, "problem": "isolation or the app download was not ready: %s" % json.dumps(
                {k: v for k, v in steps.items() if isinstance(v, dict)}, default=str)[:300]}
    except Exception as error:  # noqa: BLE001 - the probe never fails the job
        probe["fatal"] = "%s: %s" % (type(error).__name__, error)
        rec.note("probe stopped: " + probe["fatal"])
    finally:
        if env is not None:
            guarded("teardown", env.teardown, rec)
            probe["isolation"] = dict(env.facts)
        probe.setdefault("native_check", {"status": "not run in this probe (--no-build)" if args.no_build else "cargo build running in the background"})
        guarded("write probe outputs", write_outputs, rec)
        print(probe.get("feasibility", ""), flush=True)
        if native_proc is not None:
            end = time.time() + float(args.build_wait_min) * 60
            while native_proc.poll() is None and time.time() < end:
                time.sleep(5)
            if native_proc.poll() is None:
                native_proc.kill()
                probe["native_check"] = {"status": "cargo build still running after %.0f min of waiting; killed (cargo-build.log shows how far it got)" % float(args.build_wait_min)}
            else:
                ok = native_proc.returncode == 0 and native_binary is not None and native_binary.exists()
                probe["native_check"] = {"status": "cargo build %s (rc %s)" % ("succeeded" if ok else "FAILED", native_proc.returncode), "built": ok}
            guarded("write probe outputs", write_outputs, rec)
            print("native check: %s" % probe["native_check"]["status"], flush=True)
    return 0


def cmd_run(args: argparse.Namespace) -> int:
    evidence = Path(args.evidence)
    evidence.mkdir(parents=True, exist_ok=True)
    rec = Recorder(evidence)
    work = work_dir()
    tiers_wanted = list(ALL_TIERS) if args.tier == "both" else [args.tier]
    cases_wanted = [item.strip() for item in args.cases.split(",") if item.strip()]
    for case in cases_wanted:
        if case not in SCENARIO_FOR_CASE:
            raise SystemExit("unknown case %s" % case)
    problems: List[str] = []
    system: Dict[str, Any] = {}
    app_info = bundle = provenance = None
    tiers: Dict[str, Dict[str, Any]] = {name: not_run_tier("not requested") for name in ALL_TIERS}
    cases: List[Dict[str, Any]] = []
    error_texts: Dict[str, Optional[str]] = {"released_app_ui": None, "native_check": None}
    env: Optional[Interception] = None
    native_binary: Optional[Path] = None
    isolation: Dict[str, Any] = {}
    try:
        system = system_facts(rec)
        if arch_matches(getattr(args, "arch", None), platform.machine()) is False:
            problems.append("the job asked for %s but this runner is %s: the assets of the RUNNER's architecture are used" % (args.arch, platform.machine()))
        live_ips = load_live_ips()
        rec.live_ips = live_ips
        app = None
        facts: Dict[str, Any] = {}
        # 1. everything that reads the real GitHub, before the hosts mapping
        if "released_app" in tiers_wanted or "native_check" in tiers_wanted:
            try:
                ensure_tag(rec)
                app, app_info, facts, provenance = prepare_app(rec, evidence, work, system)
                bundle = facts
            except (StepFailed, AssetError, OSError, KeyError) as error:
                problems.append("released app unavailable: %s" % error)
                tiers["released_app"] = not_run_tier("the released app could not be prepared: %s" % error, "infeasible")
        if "native_check" in tiers_wanted:
            native_binary = build_native(rec, evidence)
            if native_binary is None:
                tiers["native_check"] = not_run_tier("cargo build --release --locked failed or timed out (see cargo-build.log)", "infeasible", "native-updater-check")
                problems.append("native-updater-check did not build")
        # 2. isolation
        env = Interception(rec, evidence, work, live_ips, real_banner=bool(args.real_banner_api), shared_dir=shared_dir(), keep_ca=bool(args.keep_ca))
        block = env.block_live_network()
        if not block["verified"]:
            raise StepFailed("the live-network block could not be verified: no case may run without it")
        env.make_ca()
        trust = env.trust_ca()
        if not trust["trusted"]:
            raise StepFailed("the per-run CA is not trusted by Security.framework: %s" % json.dumps(trust)[:300])
        hosts = env.map_hosts()
        if not hosts["ok"]:
            raise StepFailed("the hosts mapping does not resolve as intended: %s" % json.dumps(hosts)[:300])
        test = env.self_test()
        if not test["landed_on_recorder"]:
            raise StepFailed("a request to the manifest URL did not land on the recorder: %s" % json.dumps(test)[:300])
        # 3. the cases
        if "native_check" in tiers_wanted and native_binary is not None:
            tiers["native_check"] = not_run_tier("ran", "ran", "native-updater-check (real tauri-plugin-updater %s check())" % PLUGIN_VERSION)
            tiers["native_check"]["note"] = "same plugin version and OS trust-store verifier as the released app; not the released app binary"
            for case in cases_wanted:
                entry = run_native_case(rec, env, evidence, work, native_binary, case, real_home=bool(getattr(args, "native_real_home", False)))
                cases.append(entry)
                if case == "clean" and entry["trigger_outcome"].get("error") and error_texts["native_check"] is None:
                    error_texts["native_check"] = str(entry["trigger_outcome"]["error"])
        if "released_app" in tiers_wanted and app is not None:
            for case in cases_wanted:
                entry = run_app_case(rec, env, evidence, work, app, facts, case)
                cases.append(entry)
                message = entry["trigger_outcome"]["plugin_check"].get("error")
                if case == "clean" and message and error_texts["released_app_ui"] is None:
                    error_texts["released_app_ui"] = str(message)
            app_cases = [c for c in cases if c["tier"] == "released_app"]
            reached = [c for c in app_cases if c["trigger_outcome"]["plugin_check"].get("reached")]
            if reached:
                tiers["released_app"] = not_run_tier("the released app's own Install button drove the real plugin check() against the recorder", "ran",
                                                     "System Events (JXA) UI scripting: Settings > Check for updates > Install")
                tiers["released_app"]["real_banner_api"] = bool(args.real_banner_api)
            else:
                first = app_cases[0]["trigger_outcome"]["plugin_check"].get("reason") if app_cases else "no case ran"
                tiers["released_app"] = not_run_tier("the plugin check() was not reached from the released app: %s" % first, "infeasible",
                                                     "System Events (JXA) UI scripting: Settings > Check for updates")
                tiers["released_app"]["real_banner_api"] = bool(args.real_banner_api)
    except StepFailed as error:
        problems.append(str(error))
        rec.note("run stopped: %s" % error)
    except SystemExit:
        raise
    except Exception as error:  # noqa: BLE001 - partial evidence beats none
        problems.append("%s: %s" % (type(error).__name__, error))
        rec.note("run crashed: %s: %s" % (type(error).__name__, error))
    finally:
        if env is not None:
            isolation = dict(env.facts)
            try:
                env.teardown()
            except Exception as error:  # noqa: BLE001
                rec.note("teardown failed: %s" % error)
        # the exact 404 text: the released app's own UI text wins; the native checker's text is the fallback (and is kept next to it)
        manifest_error = error_texts["released_app_ui"] or error_texts["native_check"]
        manifest_source = ("released_app tier: UI text of the Updates card (Settings.tsx update-error), scenario latest-404" if error_texts["released_app_ui"] else
                           ("native_check tier (real tauri-plugin-updater %s check(), scenario latest-404)" % PLUGIN_VERSION if error_texts["native_check"] else None))
        if manifest_error is not None:
            (evidence / "manifest404-error.txt").write_text(manifest_error + "\n", encoding="utf-8")
        result = assemble_result(system, app_info, bundle, provenance, tiers, cases, manifest_error, manifest_source, isolation, problems)
        result["manifest404_error_texts"] = error_texts
        result["real_banner_api"] = {"enabled": bool(args.real_banner_api), "scope": REAL_BANNER_SCOPE if args.real_banner_api else "off"}
        write_json(evidence / "result.json", result)
        print("verdict: %s (tiers %s)" % (result["verdict"], json.dumps(result["tier_verdicts"])), flush=True)
    return 0


def cmd_cleanup(args: argparse.Namespace) -> int:
    """End of the job: take the shared CA's trust away once. Never fails; every call is bounded."""
    evidence = Path(args.evidence)
    evidence.mkdir(parents=True, exist_ok=True)
    rec = Recorder(evidence)
    try:
        env = Interception(rec, evidence, work_dir(), [], real_banner=True, shared_dir=shared_dir(), keep_ca=False)
        state = env.read_ca_state()
        if state and state.get("trusted") and state.get("ca_sha1"):
            env.ca_sha1 = state.get("ca_sha1")
            env.trusted = True
            env.untrust_ca(state_ca_cert=str(env.shared_dir / "ca" / "ca.crt"))
            state["trusted"] = env.trusted
            write_json(env.ca_state_path, state)
            rec.note("cleanup: the per-job CA trust was removed (still trusted: %s)" % env.trusted)
        else:
            rec.note("cleanup: no trusted per-job CA recorded")
        env.torn_down = True
    except Exception as error:  # noqa: BLE001 - cleanup never fails the job
        rec.note("cleanup failed: %s: %s" % (type(error).__name__, error))
    return 0


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    probe = sub.add_parser("probe", help="capability and Accessibility probe; never fails the job")
    probe.add_argument("--evidence", required=True)
    probe.add_argument("--arch", choices=("arm64", "aarch64", "x86_64", "x64"), help="the architecture the workflow job expects (cross-checked against the runner)")
    probe.add_argument("--no-build", action="store_true", help="do not start the background cargo build of the native checker")
    probe.add_argument("--build-wait-min", type=float, default=15.0, help="how long to wait for the background build at the end (the feasibility answer is written and printed BEFORE this wait)")
    probe.add_argument("--budget-min", type=float, default=25.0, help="soft budget: the released-app part is skipped if less than 2 minutes of it are left")
    probe.add_argument("--real-banner-api", action=argparse.BooleanOptionalAction, default=True,
                       help="let the banner's one read-only GET reach the real api.github.com (default on; approved); off = every GitHub name goes to the recorder")
    probe.add_argument("--keep-ca", action=argparse.BooleanOptionalAction, default=True,
                       help="leave the per-job CA trusted for the run phase that follows in the same job (default on; `cleanup` or the run phase removes it)")
    run = sub.add_parser("run", help="the isolation cases")
    run.add_argument("--evidence", required=True)
    run.add_argument("--arch", choices=("arm64", "aarch64", "x86_64", "x64"), help="the architecture the workflow job expects (cross-checked against the runner)")
    run.add_argument("--tier", choices=("released_app", "native_check", "both"), default="both")
    run.add_argument("--cases", default=",".join(ALL_CASES))
    run.add_argument("--native-real-home", action="store_true", help="run the native checker with the runner's real HOME (debug switch: use it if Security.framework misbehaves with the sandbox HOME)")
    run.add_argument("--real-banner-api", action=argparse.BooleanOptionalAction, default=True,
                     help="let the banner's one read-only GET reach the real api.github.com (default on; approved); off = negative control")
    run.add_argument("--keep-ca", action=argparse.BooleanOptionalAction, default=False,
                     help="leave the per-job CA trusted at the end (default off: the run phase is the last one and removes it, once)")
    cleanup = sub.add_parser("cleanup", help="remove the per-job CA's trust (bounded, best effort); for an `if: always()` step at the end of a job")
    cleanup.add_argument("--evidence", required=True)
    args = parser.parse_args(argv)
    refused = refuse_outside_ci(args.command)
    if refused is not None:
        return refused
    if args.command == "cleanup":
        return cmd_cleanup(args)
    if args.command == "probe":
        try:
            return cmd_probe(args)
        except Exception as error:  # noqa: BLE001 - the probe must not fail the job whatever happens
            print("probe crashed outside its guards: %s: %s" % (type(error).__name__, error), flush=True)
            evidence = Path(args.evidence)
            evidence.mkdir(parents=True, exist_ok=True)
            write_json(evidence / "probe.json", {"schema": SCHEMA_PROBE, "fatal": "%s: %s" % (type(error).__name__, error), "finished": now_iso()})
            (evidence / "feasibility.txt").write_text("NO: the probe crashed before it could answer: %s: %s\n" % (type(error).__name__, error), encoding="utf-8")
            if not (evidence / "accessibility.json").exists():
                write_json(evidence / "accessibility.json", {"error": "the probe crashed before the Accessibility tests: %s" % error})
            return 0
    return cmd_run(args)


if __name__ == "__main__":
    sys.exit(main())
