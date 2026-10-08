#!/usr/bin/env python3
"""Keep the released app away from every ARC address during a desktop lab run (THROWAWAY LAB FILE, never merged).

Rule of the lab: no contact with ARC validators, nodes or arc.ai. The app under test is the released v0.7.11 desktop app; at
start-up it may try to reach the seed addresses and a few named hosts. This module only BUILDS the commands (pure functions,
unit-tested everywhere); the OS jobs run them on the runner, where sudo / administrator rights exist.

Two layers, the same approach the repository CI and the Stage A jobs use:
  1. addresses: every address of the repository's own live-network list (LIVE_NETWORK_IPS of .github/workflows/legacy-bridge.yml,
     cross-checked against live_ips in tests/legacy-bridge/headless-v07-acceptance.sh) is rejected at the firewall:
       Linux   iptables -I OUTPUT -d <ip> -j REJECT
       Windows New-NetFirewallRule -Direction Outbound -Action Block -RemoteAddress <ips>
       macOS   pf rules "block drop out quick to <ip>" loaded with pfctl
  2. names: the ARC names (arc.ai and subdomains seen in the sources), the app's two third-party hosts (huggingface.co for
     model downloads, rsms.me for a web font) and anything that looks like a DNS name in the seeds file or genesis file of
     the released app are mapped to LOOPBACK in the hosts file. A name that resolves to loopback never leaves the machine; if
     the app tries it anyway the attempt lands on the recording server (lib/mitm_server.py), whose certificate does not cover
     the name, so the handshake is refused and logged as a tls_failure with its SNI: evidence of an attempt that went nowhere.
     (At v0.7.11 the seeds file and genesis file hold addresses only, so that part currently finds no names.)

Every builder returns {"commands": [...], "verify": "<one command or script>", "shell": "bash"|"pwsh", "hosts_path": str} and never
runs anything. ``hosts_lines`` produces the lines for BOTH IPv4 and IPv6 loopback, so a resolver cannot fall back to a real
AAAA record of the name (macOS asks DNS for AAAA even when the hosts file has an A record).

UNVERIFIED ON CI: that pfctl accepts the generated rules on the macOS runners exactly as in Stage A (it is the same line);
that the Windows firewall rule blocks a TcpClient connect within five seconds (the Stage A job proves the same on windows-latest).
"""
from __future__ import annotations

import argparse
import json
import re
import shlex
import sys
from pathlib import Path
from typing import Dict, Iterable, List, Optional

IPV4 = re.compile(r"^(?:(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])$")
DNS_NAME = re.compile(r"^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$")
MARKER = "# wave0-lab-desktop"
ARC_NAMES = ("arc.ai", "www.arc.ai", "api.arc.ai", "testnet.arc.ai")
THIRD_PARTY_NAMES = ("huggingface.co", "cdn-lfs.huggingface.co", "rsms.me")
LINUX_HOSTS = "/etc/hosts"
MACOS_HOSTS = "/etc/hosts"
WINDOWS_HOSTS = r"$env:SystemRoot\System32\drivers\etc\hosts"
FIREWALL_RULE_NAME = "arc-live-network-block"


class LiveBlockError(ValueError):
    """The inputs of a builder are not usable."""


def parse_live_ips(workflow_text: str, harness_text: str) -> List[str]:
    """The live address list, from the repository workflow, cross-checked against the acceptance script."""
    found = re.findall(r"^\s*LIVE_NETWORK_IPS:\s*(.+?)\s*$", workflow_text, flags=re.MULTILINE)
    if len(found) != 1:
        raise LiveBlockError("expected exactly one LIVE_NETWORK_IPS line, found %d" % len(found))
    arrays = re.findall(r"^live_ips=\((.*?)\)\s*$", harness_text, flags=re.MULTILINE)
    if len(arrays) != 1:
        raise LiveBlockError("expected exactly one live_ips array, found %d" % len(arrays))
    workflow, harness = found[0].split(), arrays[0].split()
    if workflow != harness:
        raise LiveBlockError("LIVE_NETWORK_IPS differs from live_ips in the acceptance script")
    validate_ips(workflow)
    return workflow


def validate_ips(ips: Iterable[str]) -> List[str]:
    values = list(ips)
    if not values or len(set(values)) != len(values):
        raise LiveBlockError("the address list is empty or has duplicates")
    for value in values:
        if not IPV4.match(value):
            raise LiveBlockError("not a dotted IPv4 address: %r" % value)
    return values


def load_live_ips(root) -> List[str]:
    """Read the list from a checkout of the repository (the base tree carries both files)."""
    base = Path(root)
    return parse_live_ips(
        (base / ".github/workflows/legacy-bridge.yml").read_text(encoding="utf-8"),
        (base / "tests/legacy-bridge/headless-v07-acceptance.sh").read_text(encoding="utf-8"),
    )


def names_in_seed_files(seeds_text: str, genesis_text: str) -> List[str]:
    """DNS names (not addresses) found in the released app's seeds file and genesis file: host parts of seed lines and of URLs."""
    names: List[str] = []
    for raw in seeds_text.splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        host = line.rsplit(":", 1)[0] if re.search(r":\d+$", line) else line
        host = host.strip("[]").lower()
        if DNS_NAME.match(host) and not IPV4.match(host):
            names.append(host)
    for match in re.finditer(r"https?://([A-Za-z0-9.-]+)", genesis_text):
        host = match.group(1).lower()
        if DNS_NAME.match(host) and not IPV4.match(host):
            names.append(host)
    return sorted(set(names))


def blocked_names(extra: Iterable[str] = ()) -> List[str]:
    """Every name mapped to loopback: ARC names, the app's third-party hosts, and names found in the seed/genesis files."""
    names: List[str] = []
    for name in list(ARC_NAMES) + list(THIRD_PARTY_NAMES) + list(extra):
        name = name.strip().lower()
        if not DNS_NAME.match(name):
            raise LiveBlockError("not a DNS name: %r" % name)
        if name in ("github.com", "api.github.com") or name.endswith(".githubusercontent.com"):
            raise LiveBlockError("%s is served by the recording server, it must not be blackholed" % name)
        if name not in names:
            names.append(name)
    return names


def hosts_lines(names: Iterable[str], address4: str = "127.0.0.1", address6: str = "::1", marker: str = MARKER) -> List[str]:
    """hosts-file lines for ``names``, IPv4 and IPv6, each tagged with a marker comment so a teardown can find them."""
    lines: List[str] = []
    for name in names:
        name = name.strip().lower()
        if not DNS_NAME.match(name):
            raise LiveBlockError("not a DNS name: %r" % name)
        lines.append("%s %s %s" % (address4, name, marker))
        lines.append("%s %s %s" % (address6, name, marker))
    return lines


def loopback_hosts_lines(names: Iterable[str]) -> List[str]:
    """The lines that make the GitHub names reach the recording server (``names`` from config.json hosts)."""
    return hosts_lines(names)


def linux_commands(ips: Iterable[str], names: Iterable[str], extra_hosts_lines: Iterable[str] = ()) -> Dict[str, object]:
    values = validate_ips(ips)
    commands = ["sudo iptables -I OUTPUT -d %s -j REJECT" % ip for ip in values]
    lines = hosts_lines(names) + list(extra_hosts_lines)
    commands.append("printf '%%s\\n' %s | sudo tee -a %s > /dev/null" % (" ".join(shlex.quote(line) for line in lines), LINUX_HOSTS))
    checks = []
    for ip in values:
        checks.append("sudo iptables -S OUTPUT | grep -q -- '-d %s/32 -j REJECT'" % ip)
    checks.append("if curl -s -m 5 -o /dev/null https://%s/health; then echo 'the live network is still reachable' >&2; exit 1; fi" % values[0])
    return {"commands": commands, "verify": "set -e; " + "; ".join(checks), "shell": "bash", "hosts_path": LINUX_HOSTS}


def windows_commands(ips: Iterable[str], names: Iterable[str], extra_hosts_lines: Iterable[str] = ()) -> Dict[str, object]:
    values = validate_ips(ips)
    quoted = ",".join("'%s'" % ip for ip in values)
    lines = hosts_lines(names) + list(extra_hosts_lines)
    value_array = "@(" + ",".join("'%s'" % line for line in lines) + ")"
    commands = [
        "New-NetFirewallRule -DisplayName '%s' -Direction Outbound -Action Block -RemoteAddress @(%s) | Out-Null" % (FIREWALL_RULE_NAME, quoted),
        "Set-NetFirewallProfile -All -Enabled True",
        "Add-Content -Path \"%s\" -Value %s -Encoding ascii" % (WINDOWS_HOSTS, value_array),
    ]
    verify = " ".join([
        "$ips = @(%s);" % quoted,
        "if (-not (Get-NetFirewallRule -DisplayName '%s' -ErrorAction SilentlyContinue)) { throw 'the live network block is not in place' };" % FIREWALL_RULE_NAME,
        "$client = New-Object System.Net.Sockets.TcpClient; $reachable = $false;",
        "try { $reachable = $client.ConnectAsync($ips[0], 443).Wait(5000) -and $client.Connected }",
        "catch { Write-Host \"blocked as intended: $($_.Exception.InnerException.Message)\" };",
        "$client.Dispose(); if ($reachable) { throw 'the live network is still reachable' }",
    ])
    return {"commands": commands, "verify": verify, "shell": "pwsh", "hosts_path": WINDOWS_HOSTS}


def macos_commands(ips: Iterable[str], names: Iterable[str], pf_conf: str, extra_hosts_lines: Iterable[str] = ()) -> Dict[str, object]:
    values = validate_ips(ips)
    lines = hosts_lines(names) + list(extra_hosts_lines)
    commands = [": > %s" % shlex.quote(pf_conf)]  # create or truncate the rule file
    commands += ["echo 'block drop out quick to %s' >> %s" % (ip, shlex.quote(pf_conf)) for ip in values]
    commands.append("sudo pfctl -f %s" % shlex.quote(pf_conf))
    commands.append("sudo pfctl -e || sudo pfctl -s info | grep -q 'Status: Enabled'")
    commands.append("printf '%%s\\n' %s | sudo tee -a %s > /dev/null" % (" ".join(shlex.quote(line) for line in lines), MACOS_HOSTS))
    # The rules are confirmed BEFORE any connection to a live address is attempted.
    checks = ["sudo pfctl -s rules | grep -q 'block drop out quick to %s'" % ip for ip in values]
    checks.append("if curl -s -m 5 -o /dev/null https://%s/health; then echo 'the live network is still reachable' >&2; exit 1; fi" % values[0])
    return {"commands": commands, "verify": "set -e; " + "; ".join(checks), "shell": "bash", "hosts_path": MACOS_HOSTS}


def build(os_name: str, ips: Iterable[str], names: Iterable[str], pf_conf: str = "arc-pf.conf", extra_hosts_lines: Iterable[str] = ()) -> Dict[str, object]:
    if os_name == "linux":
        return linux_commands(ips, names, extra_hosts_lines)
    if os_name == "windows":
        return windows_commands(ips, names, extra_hosts_lines)
    if os_name in ("macos", "macos-arm64", "macos-intel", "macos_arm64", "macos_intel"):
        return macos_commands(ips, names, pf_conf, extra_hosts_lines)
    raise LiveBlockError("unknown operating system %r" % os_name)


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("os", choices=("linux", "windows", "macos"))
    parser.add_argument("--repo", default=".", help="repository checkout holding the live address list")
    parser.add_argument("--pf-conf", default="arc-pf.conf")
    parser.add_argument("--github-hosts", default="", help="comma separated names to map to the recording server as well")
    args = parser.parse_args(argv)
    ips = load_live_ips(args.repo)
    github = [item for item in args.github_hosts.split(",") if item.strip()]
    plan = build(args.os, ips, blocked_names(), args.pf_conf, loopback_hosts_lines(github))
    print(json.dumps(plan, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
