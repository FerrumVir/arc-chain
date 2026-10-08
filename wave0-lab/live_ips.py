#!/usr/bin/env python3
"""Print the live-network IPv4 addresses the repository's own CI isolates, space separated.

THROWAWAY LAB FILE. The list is read from .github/workflows/legacy-bridge.yml (the
LIVE_NETWORK_IPS env) and cross-checked against the live_ips array in
tests/legacy-bridge/headless-v07-acceptance.sh, so the lab never carries its own copy.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

IPV4 = re.compile(r"^(?:(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])$")


def from_workflow(text: str) -> list[str]:
    found = re.findall(r"^\s*LIVE_NETWORK_IPS:\s*(.+?)\s*$", text, flags=re.MULTILINE)
    if len(found) != 1:
        raise SystemExit(f"expected exactly one LIVE_NETWORK_IPS line, found {len(found)}")
    return found[0].split()


def from_harness(text: str) -> list[str]:
    found = re.findall(r"^live_ips=\((.*?)\)\s*$", text, flags=re.MULTILINE)
    if len(found) != 1:
        raise SystemExit(f"expected exactly one live_ips array, found {len(found)}")
    return found[0].split()


def load(root: Path) -> list[str]:
    workflow = from_workflow((root / ".github/workflows/legacy-bridge.yml").read_text(encoding="utf-8"))
    harness = from_harness((root / "tests/legacy-bridge/headless-v07-acceptance.sh").read_text(encoding="utf-8"))
    if workflow != harness:
        raise SystemExit("LIVE_NETWORK_IPS in legacy-bridge.yml differs from live_ips in the acceptance script")
    if not workflow or len(set(workflow)) != len(workflow):
        raise SystemExit("the live address list is empty or has duplicates")
    for address in workflow:
        if not IPV4.match(address):
            raise SystemExit("the live address list holds an entry that is not a dotted IPv4 address")
    return workflow


def main() -> int:
    root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(".")
    # bytes, not print(): on Windows a text-mode stdout would end the line with CRLF and poison $GITHUB_ENV
    sys.stdout.buffer.write((" ".join(load(root)) + "\n").encode("ascii"))
    sys.stdout.buffer.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
