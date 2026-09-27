#!/usr/bin/env python3
"""Verify authenticated shard discovery and real model coverage (read-only).

Run this before and after each host is reconfigured. It asserts structure and
values, so a wrong result fails rather than printing something that looks fine:

  * TLS is verified strictly. A shard announcement is an authenticated
    security path; verifying it over an unverified transport would be
    theatre, so there is deliberately no --insecure flag.
  * every node must answer /health, /network/info and /shards with the
    expected fields and types;
  * every node's advertised socket_addr must be routable - not 0.0.0.0, not
    loopback, not empty - which is the configuration this checks;
  * validator identities must be distinct, and transaction domain, network
    and chain id must match across the fleet;
  * the union of advertised ranges must cover every layer, and each node's
    registry must contain peers, not only itself, once discovery works.

Exit: 0 PASS, 1 FAIL, 2 INCOMPLETE (a host could not be read).
"""

from __future__ import annotations

import argparse
import json
import ssl
import sys
import urllib.error
import urllib.request

DEFAULT_FLEET = [
    ("NYC", "149.28.32.76"),
    ("LAX", "140.82.16.112"),
    ("AMS", "136.244.109.1"),
    ("LHR", "104.238.171.11"),
    ("NRT", "202.182.107.41"),
    ("SGP", "149.28.153.31"),
]

# Strict verification, explicitly. Not a default that a flag can turn off.
TLS = ssl.create_default_context()
TLS.check_hostname = True
TLS.verify_mode = ssl.CERT_REQUIRED


def fetch(host: str, path: str, timeout: float) -> object:
    url = f"https://{host}{path}"
    with urllib.request.urlopen(url, timeout=timeout, context=TLS) as response:
        if response.status != 200:
            raise ValueError(f"{url}: HTTP {response.status}")
        return json.load(response)


def require(condition: bool, problems: list[str], message: str) -> bool:
    if not condition:
        problems.append(message)
    return condition


def is_routable(addr: object) -> bool:
    """A stub cannot be dialled by a peer, which is the whole failure."""
    if not isinstance(addr, str) or not addr.strip():
        return False
    host = addr.split("//")[-1].rsplit(":", 1)[0].strip("[]")
    if not host or host in {"0.0.0.0", "::", "localhost"}:
        return False
    return not host.startswith("127.")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--fleet", help="name=host,name=host (default: the six validators)")
    parser.add_argument("--timeout", type=float, default=10.0)
    parser.add_argument("--json-out")
    parser.add_argument(
        "--expect-peers",
        action="store_true",
        help="also require each node's registry to hold a peer's entry, which "
        "is only true once announcements are being accepted",
    )
    args = parser.parse_args(argv)

    fleet = DEFAULT_FLEET
    if args.fleet:
        fleet = [tuple(entry.split("=", 1)) for entry in args.fleet.split(",")]

    problems: list[str] = []
    unreadable: list[str] = []
    report: dict[str, object] = {"schema": "arc.shard-discovery.v1", "nodes": {}}
    identities: dict[str, str] = {}
    domains: set[str] = set()
    networks: set[str] = set()
    chain_ids: set[str] = set()
    covered: set[int] = set()
    total_layers: set[int] = set()
    model_ids: set[str] = set()

    for name, host in fleet:
        node: dict[str, object] = {"host": host}
        try:
            health = fetch(host, "/health", args.timeout)
            info = fetch(host, "/network/info", args.timeout)
            shards = fetch(host, "/shards", args.timeout)
        except (urllib.error.URLError, ssl.SSLError, ValueError, TimeoutError) as error:
            unreadable.append(f"{name} ({host}): {type(error).__name__}: {error}")
            report["nodes"][name] = {"host": host, "unreadable": str(error)}
            continue

        for label, payload in (("health", health), ("network/info", info), ("shards", shards)):
            require(isinstance(payload, dict), problems, f"{name} {label} is not a JSON object")

        node["height"] = health.get("height")
        node["peers"] = health.get("peers")
        node["advancing"] = health.get("chain_advancing")
        require(
            isinstance(node["height"], int) and node["height"] > 0,
            problems,
            f"{name} health.height is not a positive integer: {node['height']!r}",
        )
        require(
            node["advancing"] is True,
            problems,
            f"{name} is not advancing (chain_advancing={node['advancing']!r})",
        )

        validator = info.get("validator_address")
        require(
            isinstance(validator, str) and len(validator.removeprefix("0x")) == 64,
            problems,
            f"{name} network/info.validator_address is not a 32-byte hex id: {validator!r}",
        )
        if isinstance(validator, str):
            if validator in identities:
                problems.append(
                    f"{name} shares validator_address {validator[:18]}… with {identities[validator]}"
                )
            identities[validator] = name
            node["validator_address"] = validator
        domains.add(str(info.get("transaction_domain") or info.get("recovery_domain")))
        networks.add(str(info.get("network")))
        chain_ids.add(str(info.get("chain_id")))

        # `self_shards` is what this node holds; `shards` is its registry.
        # While announcements are being refused the two are identical, which
        # is precisely the failure, so peers are counted as registry entries
        # that are NOT this node's own ranges. `node_name` is a display label,
        # not an identity, and must never be used for this.
        own = shards.get("self_shards")
        registry = shards.get("shards")
        require(isinstance(own, list), problems, f"{name} /shards omitted self_shards")
        require(isinstance(registry, list), problems, f"{name} /shards omitted shards")
        own = own if isinstance(own, list) else []
        registry = registry if isinstance(registry, list) else []

        def key(entry: object) -> tuple:
            if not isinstance(entry, dict):
                return ()
            return (entry.get("start_layer"), entry.get("end_layer"), entry.get("socket_addr"))

        own_keys = {key(entry) for entry in own}
        node["own_ranges"] = len(own)
        node["registry_entries"] = len(registry)
        node["peer_entries"] = sum(1 for entry in registry if key(entry) not in own_keys)

        model_ids.update(
            str(entry.get("model_id")) for entry in own if isinstance(entry, dict)
        )

        entries = own
        node["ranges"] = []
        node["stub_addresses"] = 0
        for entry in entries:
            start, end = entry.get("start_layer"), entry.get("end_layer")
            if isinstance(start, int) and isinstance(end, int) and start < end:
                node["ranges"].append([start, end])
                covered.update(range(start, end))
            if isinstance(entry.get("total_layers"), int):
                total_layers.add(entry["total_layers"])
            addr = entry.get("socket_addr")
            if not is_routable(addr):
                node["stub_addresses"] += 1
                problems.append(
                    f"{name} advertises a non-routable shard address {addr!r}: a peer cannot dial "
                    f"it, so the announcement is refused. Set ARC_PUBLIC_SOCKET on this host."
                )

        node["fully_covered"] = shards.get("fully_covered")
        if args.expect_peers:
            require(
                node["peer_entries"] > 0,
                problems,
                f"{name} registry holds no peer entry: announcements are still being refused",
            )
            require(
                node["fully_covered"] is True,
                problems,
                f"{name} reports fully_covered={node['fully_covered']!r}",
            )
        report["nodes"][name] = node

    require(len(domains) == 1, problems, f"transaction domains differ across the fleet: {domains}")
    require(len(networks) == 1, problems, f"networks differ across the fleet: {networks}")
    require(len(chain_ids) == 1, problems, f"chain ids differ across the fleet: {chain_ids}")
    require(
        len(model_ids) == 1,
        problems,
        f"nodes hold shards of different models: {sorted(model_ids)}",
    )
    report["model_id"] = sorted(model_ids)[0] if len(model_ids) == 1 else sorted(model_ids)

    if total_layers:
        require(
            len(total_layers) == 1,
            problems,
            f"nodes disagree on total_layers: {sorted(total_layers)}",
        )
        total = max(total_layers)
        missing = sorted(set(range(total)) - covered)
        report["coverage"] = {
            "total_layers": total,
            "covered_layers": len(covered),
            "missing_layers": missing,
        }
        require(
            not missing,
            problems,
            f"the fleet's advertised ranges leave {len(missing)} layer(s) uncovered: {missing[:8]}",
        )

    report["problems"] = problems
    report["unreadable"] = unreadable
    status = "INCOMPLETE" if unreadable else ("FAIL" if problems else "PASS")
    report["status"] = status
    text = json.dumps(report, indent=1, sort_keys=True)
    if args.json_out:
        with open(args.json_out, "w") as handle:
            handle.write(text + "\n")
    print(text)
    print(f"SHARD DISCOVERY: {status}", file=sys.stderr)
    return {"PASS": 0, "FAIL": 1, "INCOMPLETE": 2}[status]


if __name__ == "__main__":
    raise SystemExit(main())
