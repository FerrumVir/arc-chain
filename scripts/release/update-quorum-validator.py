#!/usr/bin/env python3
"""Tracked pure validators for authenticated host config and fresh quorum proof."""
from __future__ import annotations

import re
import time
from pathlib import Path

HOST_SCHEMA = "arc.rollout.host-config.v1"
QUORUM_SCHEMA = "arc.rollout.fresh-quorum.v1"
MAX_PROOF_AGE = 120
MIN_SAMPLE_INTERVAL = 10
SHA_RE = re.compile(r"[0-9a-f]{64}\Z")


class GateError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise GateError(message)


def is_native_arg(value: object) -> bool:
    return str(value).startswith("--native") or str(value).startswith("--enable-native")


def validate_native_pin(native: object, site: str) -> dict:
    """An owner-pinned native inference tail, its file hashes and context commitment.

    Native inference is irreversibly activated on the live chain, so an update proof
    must accept exactly the pinned production tail on every host and nothing else.
    """
    require(isinstance(native, dict) and set(native) == {"argv_tail", "files_sha256", "context_commitment"},
            "native pin schema differs at " + site)
    tail = native["argv_tail"]
    require(isinstance(tail, list) and tail and all(isinstance(value, str) and value for value in tail),
            "native argv tail is malformed at " + site)
    require(is_native_arg(tail[0]) and all(is_native_arg(value) or not value.startswith("-") for value in tail),
            "native argv tail may carry only native flags and their values at " + site)
    files = native["files_sha256"]
    require(isinstance(files, dict) and files and
            all(isinstance(path, str) and path.startswith("/") and ".." not in Path(path).parts and
                SHA_RE.fullmatch(str(digest)) is not None for path, digest in files.items()),
            "native file hash pins are malformed at " + site)
    require(re.fullmatch(r"(?:0x)?[0-9a-f]{64}", str(native["context_commitment"])) is not None,
            "native context commitment is malformed at " + site)
    return native


def validate_host_config(config: dict) -> tuple[dict[str, dict], int, int]:
    require(config.get("schema") == HOST_SCHEMA, "unsupported host config schema")
    total_stake = config.get("total_stake")
    quorum_stake = config.get("quorum_stake")
    require(type(total_stake) is int and total_stake > 0, "invalid total validator stake")
    require(type(quorum_stake) is int and quorum_stake == (2 * total_stake + 2) // 3,
            "quorum stake must be the integer two-thirds threshold")
    hosts = config.get("hosts")
    quorum_sites = config.get("quorum_sites")
    unobserved_stake = config.get("unobserved_stake", 0)
    require(isinstance(hosts, list) and isinstance(quorum_sites, list) and quorum_sites,
            "host list and quorum site list are required")
    require(type(unobserved_stake) is int and unobserved_stake >= 0,
            "invalid unobserved validator stake")
    for field in ("genesis_file_sha256", "genesis_network_hash", "recovery_domain",
                  "checkpoint_manifest_hash"):
        require(SHA_RE.fullmatch(str(config.get(field, ""))) is not None, "invalid network pin: " + field)
    require(type(config.get("validator_set_id")) is int and config["validator_set_id"] > 0,
            "invalid validator set id pin")
    by_site: dict[str, dict] = {}
    ips: set[str] = set()
    identities: set[str] = set()
    for host in hosts:
        require(isinstance(host, dict), "host config row must be an object")
        site = host.get("site")
        require(isinstance(site, str) and re.fullmatch(r"[a-z]{3}", site), "invalid site name")
        require(site not in by_site, "duplicate host site")
        for key in ("ip", "hostname", "validator", "stake", "unit", "data_dir", "genesis_path",
                    "genesis_sha256", "baseline_binary_sha256", "legacy_v3_wire"):
            require(key in host, f"missing {key} for {site}")
        require(isinstance(host["ip"], str) and re.fullmatch(r"(?:[0-9]{1,3}\.){3}[0-9]{1,3}", host["ip"]), "invalid host IP")
        require(host["ip"] not in ips, "duplicate host IP")
        require(host["hostname"] == "arc-node-" + site, "unexpected hostname pin for " + site)
        require(SHA_RE.fullmatch(str(host["validator"])) is not None, "invalid validator identity for " + site)
        require(host["validator"] not in identities, "duplicate validator identity")
        require(type(host["stake"]) is int and host["stake"] > 0, "invalid stake for " + site)
        require(SHA_RE.fullmatch(str(host["baseline_binary_sha256"])) is not None, "invalid baseline binary pin for " + site)
        require(host["genesis_sha256"] == config["genesis_file_sha256"], "host genesis pin differs from network config")
        require(host["unit"] == f"arc-node-v3-{site}.service", "unexpected unit name for " + site)
        require(type(host["legacy_v3_wire"]) is bool, "legacy wire baseline must be explicit for " + site)
        for path_key in ("data_dir", "genesis_path"):
            path = host[path_key]
            require(isinstance(path, str) and path.startswith("/") and ".." not in Path(path).parts,
                    "unsafe " + path_key + " for " + site)
        if "native" in host:
            validate_native_pin(host["native"], site)
        by_site[site] = host
        ips.add(host["ip"])
        identities.add(host["validator"])
    natives = [host.get("native") for host in by_site.values()]
    require(all(native is None for native in natives) or all(native is not None for native in natives),
            "native pins must cover every host or none")
    require(len({str(native["context_commitment"]).removeprefix("0x") for native in natives if native}) <= 1,
            "hosts pin different native context commitments")
    require(len(quorum_sites) == len(set(quorum_sites)), "duplicate quorum site")
    require(set(quorum_sites) == set(by_site), "host config must contain exactly the authenticated quorum scope")
    require(set(quorum_sites) <= set(by_site), "quorum references a missing host")
    require(sum(by_site[s]["stake"] for s in quorum_sites) >= quorum_stake,
            "configured authenticated hosts cannot meet the stake quorum")
    require(sum(h["stake"] for h in by_site.values()) <= total_stake,
            "configured host stakes exceed total validator stake")
    require(sum(h["stake"] for h in by_site.values()) + unobserved_stake == total_stake,
            "observed and explicitly unobserved stake must equal total validator stake")
    return {s: by_site[s] for s in quorum_sites}, total_stake, quorum_stake


def validate_quorum_proof(proof: dict, hosts: dict[str, dict], total_stake: int,
                          quorum_stake: int, *, expected_network: dict | None = None,
                          now: float | None = None) -> dict:
    now = time.time() if now is None else now
    require(proof.get("schema") == QUORUM_SCHEMA, "unsupported fresh quorum proof schema")
    require("StrictHostKeyChecking=yes" in proof.get("transport", "") and "IdentitiesOnly=yes" in proof.get("transport", ""),
            "quorum proof must use strict pinned SSH identities")
    require(proof.get("total_stake") == total_stake and proof.get("quorum_stake") == quorum_stake,
            "fresh proof voting weights differ from host config")
    network_fields = ("genesis_file_sha256", "genesis_network_hash", "recovery_domain",
                      "checkpoint_manifest_hash", "validator_set_id")
    if expected_network is not None:
        for field in network_fields:
            require(proof.get(field) == expected_network.get(field), "fresh network pin differs from host config: " + field)
    captured = proof.get("captured_at_unix")
    require(type(captured) in (int, float) and 0 <= now - captured < MAX_PROOF_AGE, "quorum proof is stale or future-dated")
    samples = proof.get("samples")
    require(isinstance(samples, list) and len(samples) == 2, "two fresh quorum samples are required")
    require(all(isinstance(sample, dict) for sample in samples), "quorum sample must be an object")
    times = [sample.get("captured_at_unix") for sample in samples]
    require(all(type(t) in (int, float) for t in times) and MIN_SAMPLE_INTERVAL <= times[1] - times[0] <= MAX_PROOF_AGE,
            "quorum samples must be separated by 10–120 seconds")
    require(times[0] <= times[1] <= captured <= now, "quorum proof timestamps are not ordered")
    expected_sites = set(hosts)
    prior: dict[str, tuple] = {}
    for sample in samples:
        rows = sample.get("nodes")
        require(isinstance(rows, list) and len(rows) == len(expected_sites), "sample missing or duplicating a quorum host")
        current: dict[str, dict] = {}
        for row in rows:
            require(isinstance(row, dict), "quorum host sample row must be an object")
            site = row.get("site")
            require(site in expected_sites and site not in current, "sample contains missing/duplicate or unconfigured host")
            pin = hosts[site]
            require((row.get("ip"), row.get("hostname"), row.get("validator"), row.get("stake")) ==
                    (pin["ip"], pin["hostname"], pin["validator"], pin["stake"]), "host identity/stake mismatch at " + site)
            require(row.get("active") == "active", "host unit is not active at " + site)
            require(row.get("binary_sha256") == pin["baseline_binary_sha256"], "baseline binary hash mismatch at " + site)
            argv = row.get("argv_redacted")
            require(isinstance(argv, list) and argv and Path(argv[0]).name.startswith("arc-node."),
                    "running argv is absent or not an immutable versioned binary at " + site)
            native = pin.get("native")
            if native is None:
                require(not any(is_native_arg(value) for value in argv) and row.get("native") is None and
                        not row.get("native_files_sha256"),
                        "native activation is forbidden without pinned native evidence at " + site)
            else:
                tail = native["argv_tail"]
                require(len(argv) > len(tail) and argv[-len(tail):] == tail and
                        not any(is_native_arg(value) for value in argv[:-len(tail)]),
                        "running argv differs from the exact pinned native tail at " + site)
                require(row.get("native") == native and row.get("native_files_sha256") == native["files_sha256"],
                        "native pin or observed native file hashes differ at " + site)
            require("--data-dir" in argv and argv[argv.index("--data-dir") + 1] == pin["data_dir"] and
                    "--genesis" in argv and argv[argv.index("--genesis") + 1] == pin["genesis_path"],
                    "data/genesis argv differs from host pin at " + site)
            require(argv.count("--legacy-v3-wire") == (1 if pin["legacy_v3_wire"] else 0),
                    "legacy-v3-wire mode differs from baseline at " + site)
            require(type(row.get("network_total_stake")) is int and row.get("network_total_stake") == total_stake and
                    type(row.get("active_stake")) is int and row.get("active_stake") >= quorum_stake,
                    "live voting weights differ from host config at " + site)
            for row_field, proof_field in (("genesis_sha256", "genesis_file_sha256"),
                                           ("network_genesis_hash", "genesis_network_hash"),
                                           ("recovery_domain", "recovery_domain"),
                                           ("checkpoint_manifest_hash", "checkpoint_manifest_hash"),
                                           ("validator_set_id", "validator_set_id")):
                require(row.get(row_field) == proof.get(proof_field), "network recovery pin differs at " + site + ":" + row_field)
            for field in ("pid", "restarts", "invocation_id", "height", "last_block_height"):
                require(field in row, "missing process/height baseline field " + field + " at " + site)
            require(type(row["pid"]) is int and row["pid"] > 1 and type(row["restarts"]) is int and row["restarts"] >= 0,
                    "invalid process identity at " + site)
            require(re.fullmatch(r"[0-9a-f]{32}", str(row["invocation_id"])) is not None, "invalid invocation id at " + site)
            health = row.get("health") or {}
            require(type(row.get("height")) is int and type(row.get("last_block_height")) is int,
                    "host height fields are malformed at " + site)
            require(health.get("status") == "ok" and health.get("chain_advancing") is True and
                    health.get("peers", 0) >= 4 and health.get("validators") == 6 and
                    health.get("last_block_age_secs", 999999) <= 120,
                    "host health/peer/progress gate failed at " + site)
            current[site] = row
        require(set(current) == expected_sites, "sample site set differs from quorum config")
        for site, row in current.items():
            key = (row["pid"], row["restarts"], row["invocation_id"], row["binary_sha256"])
            if site in prior:
                require(prior[site] == key, "host process or binary changed between quorum samples at " + site)
            prior[site] = key
        if sample is samples[1]:
            for site, row in current.items():
                first = next(x for x in samples[0]["nodes"] if x["site"] == site)
                require(row["height"] > first["height"] and row["last_block_height"] > first["last_block_height"],
                        "host did not advance between quorum samples at " + site)
    common = proof.get("common_block")
    require(isinstance(common, dict), "explicit common block evidence is required")
    height, block_hash, state_root = common.get("height"), common.get("hash"), common.get("state_root")
    require(type(height) is int and height > 0 and SHA_RE.fullmatch(str(block_hash)) and SHA_RE.fullmatch(str(state_root)),
            "malformed common block evidence")
    require(common.get("protocol_major") == 3, "common block protocol major differs from the compatibility baseline")
    block_rows = common.get("hosts")
    require(isinstance(block_rows, list) and len(block_rows) == len(expected_sites), "common block must cover every quorum host")
    hashes: set[tuple] = set()
    seen: set[str] = set()
    for row in block_rows:
        require(isinstance(row, dict), "common block host row must be an object")
        site = row.get("site")
        require(site in expected_sites and site not in seen, "common block has missing/duplicate host")
        seen.add(site)
        require(row.get("height") == height and row.get("hash") == block_hash and row.get("state_root") == state_root,
                "common block disagreement at " + site)
        require(row.get("protocol_major") == 3, "common block protocol major mismatch at " + site)
        hashes.add((row["hash"], row["state_root"]))
    require(seen == expected_sites and len(hashes) == 1, "common block quorum incomplete or divergent")
    online_stake = sum(hosts[s]["stake"] for s in expected_sites)
    require(online_stake >= quorum_stake, "authenticated quorum stake is below threshold")
    heights = [row["last_block_height"] for row in samples[1]["nodes"]]
    require(height <= min(heights), "common block is ahead of a sampled host")
    return {"captured_at_unix": captured, "host_count": len(expected_sites), "online_stake": online_stake,
            "total_stake": total_stake, "quorum_stake": quorum_stake, "common_height": height,
            "common_hash": block_hash, "common_state_root": state_root}


