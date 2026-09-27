"""Run a recorded ARC soak. Records everything; judges nothing.

    python3 -m arc_soak.orchestrate --self-test --binary B --provenance P
    python3 -m arc_soak.orchestrate --hours 24  --binary B --provenance P

The verdict and the process exit status come ONLY from `arc_soak.analyze`,
which is run on every exit path - normal completion, an exception, SIGINT or
SIGTERM. An orchestrator that dies part-way leaves `completed: false`, and the
analyzer turns that into INCOMPLETE, never PASS.

What is recorded
----------------
run.json          configuration, identities, provenance binding, thresholds,
                  and - rewritten at the end - completion and duration
samples.jsonl     per node per sample: liveness, identity, height, round,
                  peers, finalized height, RSS, CPU, disk, log bytes
agreement.jsonl   per check: every intended replica's response at one height
faults.jsonl      per fault: kill, restart, and each recovery phase separately
workload.jsonl    per offered item: submission, acceptance, settlement, latency
node-<i>.<k>.log  each process incarnation's own log

Starting it is a deliberate act. Nothing schedules it.
"""

import argparse
import concurrent.futures
import hashlib
import json
import os
import random
import re
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from typing import Any, Dict, List, Optional, Set, Tuple

from arc_soak import analyze
from arc_ops import backup as node_backup

FAUCET_POOL = "2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213"
HEX64 = re.compile(r"^[0-9a-fA-F]{64}$")
ANSI = re.compile(r"\x1b\[[0-9;]*m")


class Abort(Exception):
    """A condition under which the run cannot continue meaningfully."""


# ── small utilities ──────────────────────────────────────────────────────────

def now() -> float:
    return time.time()


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def http_json(port: int, path: str, timeout: float = 5.0,
              body: Optional[Dict[str, Any]] = None) -> Tuple[int, Any]:
    """(status, parsed JSON or text). Status 0 means no HTTP response at all."""
    url = f"http://127.0.0.1:{port}{path}"
    data = None
    headers = {}
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=headers,
                                 method="POST" if body is not None else "GET")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8", "replace")
            status = resp.status
    except urllib.error.HTTPError as exc:
        raw = exc.read().decode("utf-8", "replace")
        status = exc.code
    except (urllib.error.URLError, socket.timeout, ConnectionError, OSError):
        return 0, None
    try:
        return status, json.loads(raw)
    except json.JSONDecodeError:
        return status, raw


def port_busy(port: int) -> bool:
    for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
        s = socket.socket(socket.AF_INET, kind)
        try:
            s.bind(("127.0.0.1", port))
        except OSError:
            return True
        finally:
            s.close()
    return False


def dir_bytes(path: str) -> int:
    total = 0
    for root, _, files in os.walk(path):
        for f in files:
            try:
                total += os.path.getsize(os.path.join(root, f))
            except OSError:
                pass
    return total


def read_provenance(path: str) -> Dict[str, str]:
    """Parse the record written by scripts/arc-build-provenance.sh."""
    out: Dict[str, str] = {}
    with open(path) as fh:
        for line in fh:
            # Keys contain digits (`binary_sha256`); an earlier `[a-z_]+`
            # could not match that key, and the guard refused a correct
            # record - failing closed, but for the wrong reason.
            m = re.match(r"^([a-z0-9_]+):\s*(.*?)\s*$", line)
            if m:
                out[m.group(1)] = m.group(2).split()[0] if m.group(2) else ""
    return out


class JsonlWriter:
    def __init__(self, path: str):
        self.path = path
        self.lock = threading.Lock()
        open(path, "x").close()   # fresh: refuse to append to an old run

    def write(self, obj: Dict[str, Any]) -> None:
        line = json.dumps(obj, sort_keys=True, default=str)
        with self.lock, open(self.path, "a") as fh:
            fh.write(line + "\n")


# ── node processes ───────────────────────────────────────────────────────────

class Node:
    def __init__(self, cfg: "Config", index: int):
        self.cfg = cfg
        self.index = index
        self.rpc = cfg.base_rpc + index
        self.p2p = cfg.base_p2p + index
        self.data_dir = os.path.join(cfg.work, f"node-{index}")
        self.identity: Optional[str] = None
        self.proc: Optional[subprocess.Popen] = None
        self.incarnation = 0
        self.scheduled_down = False
        # The binary this node runs, and its recorded digest. An upgrade
        # drill changes them for one node; everything else runs cfg.binary.
        self.binary = cfg.binary
        self.binary_sha256: Optional[str] = None

    def args(self, genesis: str) -> List[str]:
        peers = ",".join(f"127.0.0.1:{self.cfg.base_p2p + j}"
                         for j in range(self.cfg.nodes) if j != self.index)
        # A single-node chain (a real-model P8 run on one host) has no peers.
        peer_args = ["--peers", peers] if peers else []
        return [self.binary, "--rpc", f"127.0.0.1:{self.rpc}",
                "--p2p-port", str(self.p2p), "--data-dir", self.data_dir,
                "--genesis", genesis] + peer_args + [
                "--insecure-dev-validator-seed", "--validator-seed", f"soak-node-{self.index}",
                "--stake", str(self.cfg.stake),
                "--snapshot-every-blocks", str(self.cfg.snapshot_every)] + (
                    ["--native-inference-activation",
                     os.path.join(self.cfg.work, "activation.json")]
                    if self.cfg.workload == "native" else []) + (
                    # Every node holds the protocol-4 state; only the first
                    # `native_workers` execute and vote (see --native-workers).
                    self.executor_args()
                    if self.cfg.workload == "native"
                    and self.index < getattr(self.cfg, "native_workers", self.cfg.nodes)
                    else [])

    def executor_args(self) -> List[str]:
        real = getattr(self.cfg, "real_model", None)
        if not real:
            return ["--native-inference-runtime", "--enable-native-inference-requests", "--native-inference-test-executor"]
        # The real canonical executor: it verifies the artifact's bytes, the
        # qualification record and the package manifest before it loads.
        return ["--native-inference-runtime", "--enable-native-inference-requests", "--native-inference-artifact", real,
                "--native-inference-qualification", self.cfg.qualification,
                "--native-package-manifest", self.cfg.package_manifest]

    def start(self, genesis: str) -> None:
        os.makedirs(self.data_dir, exist_ok=True)
        log = os.path.join(self.cfg.work, f"node-{self.index}.{self.incarnation}.log")
        self.incarnation += 1
        env = dict(os.environ, RUST_LOG=self.cfg.rust_log)
        with open(log, "ab") as fh:
            self.proc = subprocess.Popen(self.args(genesis), stdout=fh, stderr=subprocess.STDOUT,
                                         env=env, start_new_session=True)

    def alive(self) -> bool:
        return self.proc is not None and self.proc.poll() is None

    def kill9(self) -> None:
        if self.proc is not None and self.proc.poll() is None:
            self.proc.kill()
        if self.proc is not None:
            try:
                self.proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                raise Abort(f"node {self.index} did not die after SIGKILL")

    def stop(self) -> None:
        if self.proc is None or self.proc.poll() is not None:
            return
        self.proc.terminate()
        try:
            self.proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=10)

    def ready_identity(self) -> Optional[str]:
        """The identity this node's RPC reports, if it is up and answering."""
        if not self.alive():
            return None
        code, health = http_json(self.rpc, "/health", timeout=3)
        if code != 200 or not isinstance(health, dict):
            return None
        code, info = http_json(self.rpc, "/node/info", timeout=3)
        if code != 200 or not isinstance(info, dict):
            return None
        ident = str(info.get("validator") or info.get("validator_address") or "")
        ident = ident.lower().replace("0x", "")
        return ident if HEX64.match(ident) else None


def binary_self_report(health: Any, expected_sha256: str) -> str:
    """Compare a node's self-reported executable digest with the recorded one.

    "match" or "absent" (a binary that predates the field). A different
    digest means the process under test is not the recorded build, and the
    run's evidence would describe the wrong binary: that aborts the run.
    """
    reported = health.get("binary_sha256") if isinstance(health, dict) else None
    if not reported:
        return "absent"
    if str(reported).lower() != expected_sha256.lower():
        raise Abort(f"a node reports binary {str(reported)[:16]}..., "
                    f"not the recorded {expected_sha256[:16]}...")
    return "match"


def derive_identity(cfg: "Config", index: int) -> str:
    """Start the binary briefly and read the validator address it prints."""
    d = os.path.join(cfg.work, f"ident-{index}")
    os.makedirs(d)
    log = os.path.join(cfg.work, f"ident-{index}.txt")
    with open(log, "wb") as fh:
        proc = subprocess.Popen(
            [cfg.binary, "--rpc", f"127.0.0.1:{cfg.base_rpc + index}",
             "--p2p-port", str(cfg.base_p2p + index), "--data-dir", d,
             "--insecure-dev-validator-seed", "--validator-seed", f"soak-node-{index}",
             "--stake", str(cfg.stake)],
            stdout=fh, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        deadline = now() + 60
        while now() < deadline:
            with open(log, errors="replace") as fh:
                for line in fh:
                    m = re.search(r"Validator\s+:\s*(0x)?([0-9a-fA-F]{64})", ANSI.sub("", line))
                    if m:
                        return m.group(2).lower()
            time.sleep(0.5)
    finally:
        proc.kill()
        proc.wait(timeout=15)
        subprocess.run(["rm", "-rf", d], check=False)
    raise Abort(f"could not read node {index}'s validator identity")


# ── configuration ────────────────────────────────────────────────────────────

FAULT_KINDS = ("kill", "restore", "upgrade", "rollback")


def parse_fault_kinds(text: Optional[str]) -> List[str]:
    """`"kill,restore,upgrade,rollback"` -> one kind per fault index (R6 drills).

    kill: SIGKILL and restart (the default for every unlisted index);
    restore: SIGKILL, back the store up, wipe it, restore it, restart;
    upgrade: SIGKILL, back the store up, restart on --upgrade-binary;
    rollback: SIGKILL the upgraded node, restore its pre-upgrade backup, and
    restart it on the original binary.
    """
    if not text:
        return []
    kinds = [part.strip() for part in text.split(",") if part.strip()]
    unknown = [kind for kind in kinds if kind not in FAULT_KINDS]
    if unknown:
        raise SystemExit(f"--fault-kinds: unknown kind(s) {unknown}; use {', '.join(FAULT_KINDS)}")
    for index, kind in enumerate(kinds):
        if kind == "rollback" and "upgrade" not in kinds[:index]:
            raise SystemExit("--fault-kinds: a rollback needs an earlier upgrade")
    return kinds


def real_model_tuple(manifest_path: str) -> List[str]:
    """The activation tuple for the real canonical executor, read from the
    approved package manifest: artifact BLAKE3, profile commitment,
    generation commitment, and the harness's fixed assignment hash. The node
    checks this manifest against the artifact it loads, so a manifest that
    names another artifact stops the run at startup, not later."""
    with open(manifest_path) as fh:
        manifest = json.load(fh)
    try:
        tuple_ = [manifest["artifact"]["blake3"], manifest["execution"]["profile_commitment"],
                  manifest["generation"]["commitment"],
                  hashlib.sha256(b"arc-soak-assignment").hexdigest()]
    except (KeyError, TypeError):
        raise SystemExit(f"{manifest_path} is not an arc.model-package.v1 manifest")
    if manifest.get("schema") != "arc.model-package.v1" or not all(
            isinstance(h, str) and HEX64.match(h) for h in tuple_):
        raise SystemExit(f"{manifest_path} is not an arc.model-package.v1 manifest")
    return tuple_


# 100 ARC in base units: enough for many paid requests at the desktop's defaults.
DEFAULT_FUND_BASE_UNITS = 100_000_000_000


def parse_funding(values: Optional[List[str]]) -> List[Tuple[str, int]]:
    """`--fund ADDRESS[:BASE_UNITS]` accounts genesis also funds - for
    example a desktop wallet for a product journey on a protocol-4 chain,
    which admits no transfers. Refuses anything that is not a 32-byte hex
    address, a non-positive amount, or an address listed twice."""
    out: List[Tuple[str, int]] = []
    for value in values or []:
        address, _, amount = value.partition(":")
        address = address.strip().lower()
        if address.startswith("0x"):
            address = address[2:]
        if not HEX64.match(address):
            raise SystemExit(f"--fund {value}: not a 32-byte hex address")
        try:
            units = int(amount.replace("_", "")) if amount else DEFAULT_FUND_BASE_UNITS
        except ValueError:
            raise SystemExit(f"--fund {value}: the amount is not an integer of base units")
        if units <= 0:
            raise SystemExit(f"--fund {value}: the amount must be positive")
        out.append((address, units))
    if len({address for address, _ in out}) != len(out):
        raise SystemExit("--fund lists an address twice")
    return out


def parse_fault_indices(text: Optional[str]) -> Set[int]:
    """`"1,3"` -> {1, 3}. Empty or None -> no long faults."""
    if not text:
        return set()
    try:
        indices = {int(part) for part in text.split(",") if part.strip()}
    except ValueError:
        raise SystemExit(f"--long-faults must be comma-separated fault indices, not {text!r}")
    if any(i < 0 for i in indices):
        raise SystemExit("--long-faults indices must be non-negative")
    return indices


class Config:
    def down_secs_for(self, k: int) -> float:
        return self.long_down_s if k in self.long_faults else self.down_s

    def fault_kind(self, k: int) -> str:
        return self.fault_kinds[k] if k < len(self.fault_kinds) else "kill"

    def __init__(self, a: argparse.Namespace):
        self.mode = "self-test" if a.self_test else "soak"
        if a.self_test:
            # Long enough that the analyzer's minimum baseline (60 s after a
            # 30 s warm-up) and minimum recovered window (60 s after a 30 s
            # settle) both fit around one fault - otherwise a self-test could
            # only ever come back INCOMPLETE.
            self.duration = 540.0
            self.sample_s = 5.0
            self.warmup_s = 30.0
            self.baseline_s = 150.0
            self.fault_every_s = 1e9      # exactly one fault
            self.planned_faults = 1
        else:
            if not a.hours and not a.minutes:
                raise SystemExit("pass --hours N, --minutes N or --self-test")
            self.duration = float(a.hours or 0) * 3600 + float(a.minutes or 0) * 60
            self.sample_s = float(a.sample_secs)
            self.warmup_s = 60.0
            self.baseline_s = 600.0
            self.fault_every_s = float(a.fault_every_secs)
            # Faults are scheduled only while enough time remains to measure
            # the recovery that follows.
            usable = self.duration - self.warmup_s - self.baseline_s - 300
            self.planned_faults = max(0, int(usable // self.fault_every_s) + 1) if usable > 0 else 0
            if a.no_faults:
                # A measurement run (growth, throughput) - never a soak: the
                # analyzer cannot pass a run that recovered from nothing.
                self.planned_faults = 0
        self.binary = os.path.abspath(a.binary)
        self.provenance = os.path.abspath(a.provenance)
        self.nodes = a.nodes
        self.base_rpc = a.base_rpc
        self.base_p2p = a.base_p2p
        self.stake = 6666667
        self.snapshot_every = a.snapshot_every
        self.down_s = a.down_secs
        # Faults listed here stay down for long_down_s instead: long enough
        # to outlast the peers' DAG retention, so the victim cannot rejoin
        # from history and must adopt an authenticated checkpoint (C11).
        self.long_down_s = a.long_down_secs
        self.long_faults = parse_fault_indices(a.long_faults)
        if self.long_faults and self.long_down_s <= 0:
            raise SystemExit("--long-faults needs --long-down-secs")
        self.recovery_budget_s = a.recovery_budget_secs
        self.fault_kinds = parse_fault_kinds(a.fault_kinds)
        self.upgrade_binary = os.path.abspath(a.upgrade_binary) if a.upgrade_binary else None
        self.upgrade_provenance = (os.path.abspath(a.upgrade_provenance)
                                   if a.upgrade_provenance else None)
        if "upgrade" in self.fault_kinds and not (self.upgrade_binary and self.upgrade_provenance):
            raise SystemExit("an upgrade drill needs --upgrade-binary and --upgrade-provenance")
        self.rust_log = a.rust_log
        self.workload = a.workload
        self.faucet_rate = a.faucet_rate
        self.native_rate = a.native_rate
        self.native_requesters = a.native_requesters
        self.allow_battery = bool(getattr(a, "allow_battery", False))
        self.fund = parse_funding(getattr(a, "fund", None))
        workers = getattr(a, "native_workers", None)
        self.native_workers = a.nodes if workers is None else workers
        if not 0 <= self.native_workers <= a.nodes:
            raise SystemExit("--native-workers must be between 0 and --nodes")
        self.load_driver = os.path.abspath(a.load_driver) if a.load_driver else None
        # A fixed execution tuple the activation allows and every request uses.
        self.native_tuple = [hashlib.sha256(f"arc-soak-{k}".encode()).hexdigest()
                             for k in ("model", "profile", "generation", "assignment")]
        # Real canonical executor (P8): paths the owner supplies; the harness
        # never writes a qualification record.
        self.real_model = os.path.abspath(a.real_model) if getattr(a, "real_model", None) else None
        self.qualification = getattr(a, "qualification", None)
        self.package_manifest = getattr(a, "package_manifest", None)
        self.input_hex = getattr(a, "input_hex", None)
        self.max_tokens = getattr(a, "max_tokens", None) or 8
        if self.real_model:
            missing = [flag for flag, value in (("--qualification", self.qualification),
                                                ("--package-manifest", self.package_manifest),
                                                ("--input-hex", self.input_hex)) if not value]
            if missing:
                raise SystemExit(f"--real-model needs {', '.join(missing)}")
            if a.workload != "native":
                raise SystemExit("--real-model needs --workload native")
            if a.nodes > 1 and not getattr(a, "allow_multiple_real_models", False):
                raise SystemExit("--real-model loads ~8 GB per node; this host holds one. "
                                 "Use --nodes 1, or --allow-multiple-real-models on a larger host")
            self.qualification = os.path.abspath(self.qualification)
            self.package_manifest = os.path.abspath(self.package_manifest)
            self.native_tuple = real_model_tuple(self.package_manifest)
        self.work = os.path.abspath(a.work or f"/tmp/arc-soak-{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}")


# ── the run ──────────────────────────────────────────────────────────────────

class Soak:
    def __init__(self, cfg: Config):
        self.cfg = cfg
        self.nodes = [Node(cfg, i) for i in range(cfg.nodes)]
        self.genesis = os.path.join(cfg.work, "genesis.toml")
        self.stop_event = threading.Event()
        self.run: Dict[str, Any] = {}
        self.started_t = 0.0
        self.samples: Optional[JsonlWriter] = None
        self.agreement: Optional[JsonlWriter] = None
        self.faults: Optional[JsonlWriter] = None
        self.workload_log: Optional[JsonlWriter] = None
        self.fault_state: Optional[Dict[str, Any]] = None   # the fault in flight
        self.fault_lock = threading.Lock()
        self.last_heights: Dict[int, int] = {}
        self.pool = concurrent.futures.ThreadPoolExecutor(max_workers=16)

    # -- run.json --------------------------------------------------------
    def write_run(self) -> None:
        tmp = os.path.join(self.cfg.work, "run.json.tmp")
        with open(tmp, "w") as fh:
            json.dump(self.run, fh, indent=2, sort_keys=True, default=str)
        os.replace(tmp, os.path.join(self.cfg.work, "run.json"))

    # -- setup -----------------------------------------------------------
    def prepare(self) -> None:
        cfg = self.cfg
        if os.path.exists(cfg.work) and os.listdir(cfg.work):
            raise SystemExit(f"REFUSING: {cfg.work} exists and is not empty; every run gets a "
                             "fresh directory so no stale record can be read as this run's")
        os.makedirs(cfg.work, exist_ok=True)
        if not os.access(cfg.binary, os.X_OK):
            raise SystemExit(f"no executable at {cfg.binary}")
        used = sha256_file(cfg.binary)
        prov = read_provenance(cfg.provenance)
        recorded = prov.get("binary_sha256", "").lower()
        if recorded != used:
            raise SystemExit(
                f"REFUSING: the build record at {cfg.provenance} names binary {recorded[:16] or '?'}..., "
                f"but {cfg.binary} is {used[:16]}.... A soak result must be about the binary "
                "the record describes.")
        for node in self.nodes:
            node.binary_sha256 = used
        # An upgrade drill's binary is held to the same rule as the main one.
        self.upgrade_sha256: Optional[str] = None
        self.pre_upgrade: Optional[Tuple[int, str, str, str]] = None
        self.upgraded: Optional[Node] = None
        if cfg.upgrade_binary:
            upgrade_used = sha256_file(cfg.upgrade_binary)
            upgrade_recorded = read_provenance(cfg.upgrade_provenance).get("binary_sha256", "").lower()
            if upgrade_recorded != upgrade_used:
                raise SystemExit(
                    f"REFUSING: the upgrade build record names {upgrade_recorded[:16] or '?'}..., "
                    f"but {cfg.upgrade_binary} is {upgrade_used[:16]}....")
            self.upgrade_sha256 = upgrade_used
        busy = [p for i in range(cfg.nodes)
                for p in (cfg.base_rpc + i, cfg.base_p2p + i) if port_busy(p)]
        if busy:
            raise SystemExit(f"REFUSING: ports in use: {busy}")
        power = host_power()
        refusal = battery_refusal(cfg.duration, power, getattr(cfg, "allow_battery", False))
        if refusal:
            raise SystemExit(refusal)
        self.run = {
            "schema": "arc-soak-run/1",
            "mode": cfg.mode,
            "completed": False,
            "abort_reason": "the orchestrator has not finished",
            "planned_duration_s": cfg.duration,
            "sample_interval_s": cfg.sample_s,
            "host_power_at_start": power,
            "planned_faults": cfg.planned_faults,
            "binary": cfg.binary,
            "binary_sha256": used,
            "provenance": {"file": cfg.provenance, "recorded_binary_sha256": recorded,
                           "source_revision": prov.get("source_revision"),
                           "dirty_files": prov.get("dirty_files"),
                           "input_digest": prov.get("input_digest"),
                           "command": prov.get("command")},
            "host": " ".join(os.uname()),
            # Which harness recorded and judged this run. A verdict is only as
            # good as the code that reached it.
            "harness": {
                "orchestrate_sha256": sha256_file(os.path.abspath(__file__)),
                "analyze_sha256": sha256_file(os.path.abspath(analyze.__file__)),
                # The workload is evidence too: bind the exact driver binary.
                "load_driver": cfg.load_driver,
                "load_driver_sha256": (sha256_file(cfg.load_driver)
                                       if cfg.load_driver else None),
            },
            "rust_log": cfg.rust_log,
            "workload": {"profile": cfg.workload, "required": cfg.workload != "none",
                         "faucet_rate_per_s": cfg.faucet_rate if cfg.workload == "faucet" else None,
                         "native_rate_per_s": cfg.native_rate if cfg.workload == "native" else None,
                         "native_requesters": (cfg.native_requesters
                                               if cfg.workload == "native" else None),
                         # Deterministic-executor evidence exercises protocol and
                         # settlement and qualifies nothing about model quality.
                         # The same label the driver writes into every record.
                         "executor": executor_label_for(cfg)},
            "thresholds": {"recovery_budget_s": cfg.recovery_budget_s},
            "nodes": [],
        }
        self.write_run()
        self.samples = JsonlWriter(os.path.join(cfg.work, "samples.jsonl"))
        self.agreement = JsonlWriter(os.path.join(cfg.work, "agreement.jsonl"))
        self.faults = JsonlWriter(os.path.join(cfg.work, "faults.jsonl"))
        self.workload_log = JsonlWriter(os.path.join(cfg.work, "workload.jsonl"))
        # Bounded per-node consensus counters at every tick, so a throughput
        # question is answered from a timeline rather than from log lines.
        self.diag = JsonlWriter(os.path.join(cfg.work, "diag.jsonl"))
        self.events = open(os.path.join(cfg.work, "events.log"), "a")
        for node in self.nodes:
            node.identity = derive_identity(cfg, node.index)
        time.sleep(2)
        self.run["nodes"] = [{"index": n.index, "identity": n.identity, "rpc": n.rpc,
                              "p2p": n.p2p, "role": "seed" if n.index == 0 else "member"}
                             for n in self.nodes]
        self.write_run()
        # One chain-run identity per soak. It reaches the certificate domain
        # (genesis network hash), so certificates and checkpoints from an
        # earlier soak of this same genesis cannot verify against this one.
        instance = "soak-{}-{}".format(
            time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()), os.getpid())
        self.run["chain_instance_id"] = instance
        self.write_run()
        with open(self.genesis, "w") as fh:
            fh.write('[chain]\nname = "arc-soak"\nchain_id = "0x415243"\n'
                     "validator_set_complete = false\n"
                     f'instance_id = "{instance}"\n\n')
            fh.write(f'[[accounts]]\naddress = "{FAUCET_POOL}"\nbalance = 1_000_000_000_000\n\n')
            for n in self.nodes:
                fh.write(f'[[accounts]]\naddress = "{n.identity}"\nbalance = 1_000_000_000_000\n\n')
            funded = {FAUCET_POOL} | {n.identity for n in self.nodes}
            if cfg.workload == "native":
                for requester in self.native_requesters():
                    fh.write(f'[[accounts]]\naddress = "{requester}"\nbalance = 1_000_000_000_000\n\n')
                    funded.add(requester)
            for address, units in getattr(cfg, "fund", []):
                if address in funded:
                    raise Abort(f"--fund {address} is already an account the harness funds")
                fh.write(f'[[accounts]]\naddress = "{address}"\nbalance = {units}\n\n')
            self.run["workload"]["native_workers"] = getattr(cfg, "native_workers", cfg.nodes)
            self.run["workload"]["extra_funded"] = [
                {"address": address, "base_units": units}
                for address, units in getattr(cfg, "fund", [])]
            for n in self.nodes:
                fh.write(f'[[validators]]\naddress = "{n.identity}"\nstake = {cfg.stake}\n\n')
        if cfg.workload == "native":
            t = cfg.native_tuple
            with open(os.path.join(cfg.work, "activation.json"), "w") as fh:
                json.dump({"recovery_epoch": 0, "allowed_executions": [{
                    "model_hash": t[0], "profile_hash": t[1],
                    "generation_hash": t[2], "assignment_hash": t[3]}]}, fh)

    def driver_executor_args(self) -> List[str]:
        return driver_executor_args_for(self.cfg)

    def native_requesters(self) -> List[str]:
        """The driver's requester accounts, one per concurrent request, which
        genesis must fund. Asked of the driver itself so the two can never
        disagree about the derivation."""
        cfg = self.cfg
        out = subprocess.run(
            [cfg.load_driver, "--print-requester", "--requester-seed", "soak",
             "--requesters", str(cfg.native_requesters)],
            capture_output=True, text=True, timeout=30).stdout.split()
        if len(out) != cfg.native_requesters or not all(HEX64.match(a) for a in out):
            raise Abort(f"load driver did not report {cfg.native_requesters} requester "
                        f"addresses: {out!r}")
        self.run["workload"]["requesters"] = out
        return out

    def note(self, text: str) -> None:
        line = f"{time.strftime('%H:%M:%SZ', time.gmtime())} {text}"
        print(line, flush=True)
        self.events.write(line + "\n")
        self.events.flush()

    def start_all(self) -> None:
        self.note(f"starting {self.cfg.nodes} validators")
        for node in self.nodes:
            node.start(self.genesis)
            deadline = now() + 90
            while now() < deadline:
                if node.ready_identity() == node.identity:
                    break
                time.sleep(1)
            else:
                raise Abort(f"node {node.index} never answered with its own identity")
            code, health = http_json(node.rpc, "/health", timeout=3)
            self.run.setdefault("binary_self_report", {})[str(node.index)] = \
                binary_self_report(health, node.binary_sha256 or self.run["binary_sha256"])
        self.note("all validators answering with their own identities")

    # -- sampling --------------------------------------------------------
    def probe(self, node: Node) -> Dict[str, Any]:
        t = now()
        rec: Dict[str, Any] = {"t": t, "elapsed": t - self.started_t, "node": node.index,
                               "alive": node.alive(), "scheduled_down": node.scheduled_down,
                               "incarnation": node.incarnation}
        if node.alive():
            code, health = http_json(node.rpc, "/health", timeout=4)
            rec["http_ok"] = code == 200
            if isinstance(health, dict):
                rec["height"] = health.get("height")
                rec["dag_round"] = health.get("dag_round")
                rec["peers"] = health.get("peers")
            code, info = http_json(node.rpc, "/node/info", timeout=4)
            if isinstance(info, dict):
                ident = str(info.get("validator") or "").lower().replace("0x", "")
                rec["identity"] = ident or None
            code, fin = http_json(node.rpc, "/finality/latest", timeout=4)
            if isinstance(fin, dict):
                rec["finalized_height"] = fin.get("finalized_height")
                rec["finality_lag"] = fin.get("finality_lag")
            try:
                out = subprocess.run(["ps", "-o", "rss=,pcpu=", "-p", str(node.proc.pid)],
                                     capture_output=True, text=True, timeout=5).stdout.split()
                if len(out) == 2:
                    rec["rss_kb"] = int(out[0])
                    rec["cpu_pct"] = float(out[1])
            except (subprocess.SubprocessError, ValueError):
                pass
            code, diag = http_json(node.rpc, "/consensus/diagnostics", timeout=4)
            if isinstance(diag, dict):
                self.diag.write({"t": t, "node": node.index, "incarnation": node.incarnation,
                                 "diag": diag})
        rec["disk_bytes"] = dir_bytes(node.data_dir)
        rec["log_bytes"] = sum(os.path.getsize(os.path.join(self.cfg.work, f))
                               for f in os.listdir(self.cfg.work)
                               if f.startswith(f"node-{node.index}.") and f.endswith(".log"))
        return rec

    def sample(self) -> List[Dict[str, Any]]:
        tick_t = now()
        self.tick = getattr(self, "tick", -1) + 1
        recs = list(self.pool.map(self.probe, self.nodes))
        for r in recs:
            # One shared tick identity per sample: the analyzer groups by it.
            r["tick"] = self.tick
            r["tick_t"] = tick_t
            self.samples.write(r)
            h = analyze._int(r.get("height"))
            if h is not None:
                self.last_heights[r["node"]] = h
        return recs

    def check_agreement(self, recs: List[Dict[str, Any]]) -> None:
        up = [analyze._int(r.get("height")) for r in recs
              if not r.get("scheduled_down") and analyze._int(r.get("height")) is not None]
        if len(up) < 2 or min(up) < 4:
            return
        target = min(up) - 3

        def ask(node: Node) -> Dict[str, Any]:
            rep: Dict[str, Any] = {"node": node.index, "scheduled_down": node.scheduled_down}
            if node.scheduled_down:
                rep["ok"] = False
                rep["error"] = "scheduled down"
                return rep
            code, info = http_json(node.rpc, "/node/info", timeout=4)
            code_b, block = http_json(node.rpc, f"/block/{target}", timeout=5)
            if code_b != 200 or not isinstance(block, dict):
                rep.update(ok=False, error=f"/block/{target} -> HTTP {code_b}")
                return rep
            header = block.get("header") if isinstance(block.get("header"), dict) else {}
            strip = lambda v: str(v or "").lower().replace("0x", "")
            rep.update(ok=True,
                       identity=strip(info.get("validator")) if isinstance(info, dict) else None,
                       height=header.get("height"),
                       hash=strip(block.get("hash")),
                       parent=strip(header.get("parent_hash")),
                       state_root=strip(header.get("state_root")))
            return rep

        replicas = list(self.pool.map(ask, self.nodes))
        rec = {"t": now(), "target": target, "replicas": replicas}
        self.agreement.write(rec)
        # A full agreement that includes a recovering node is a recovery phase.
        with self.fault_lock:
            f = self.fault_state
            # Only a height ABOVE what the node held before it was killed
            # counts: agreement on blocks it already had shows nothing about
            # whether it recovered.
            if (f and f.get("process_ready_t") and not f.get("agreement_t")
                    and target > (f.get("pre_kill_height") or 0)):
                victim = f["node"]
                valid = [r for r in replicas if r.get("ok")]
                hashes = {r.get("hash") for r in valid}
                if (len(valid) == self.cfg.nodes and len(hashes) == 1
                        and any(r["node"] == victim for r in valid)
                        and all(analyze._int(r.get("height")) == target for r in valid)):
                    f["agreement_t"] = now()

    # -- faults ----------------------------------------------------------
    def pick_victim(self, k: int) -> Tuple[Node, List[str]]:
        """Fault 0 hits the seed; later ones rotate, preferring an existing
        laggard every other time so the seed-plus-laggard shape recurs."""
        if k == 0:
            return self.nodes[0], ["seed"]
        heights = {i: h for i, h in self.last_heights.items()}
        if k % 2 == 1 and heights:
            laggard = min(heights, key=heights.get)
            if max(heights.values()) - heights[laggard] >= 5:
                return self.nodes[laggard], ["laggard"]
        n = self.nodes[k % self.cfg.nodes]
        return n, ["seed" if n.index == 0 else "member"]

    def drill(self, k: int, kind: str, victim: Node, f: Dict[str, Any]) -> None:
        """What happens to a killed node's store and binary before it restarts
        (R6). The node is dead, so its store lock is free for the backup tool."""
        if kind == "kill":
            return
        archive = os.path.join(self.cfg.work, f"backup-fault-{k}-node-{victim.index}.tar.gz")
        node_backup.backup(victim.data_dir, archive, victim.binary)
        f["backup_archive"] = archive
        if kind == "restore":
            os.replace(victim.data_dir, f"{victim.data_dir}.before-restore-{k}")
            node_backup.restore(archive, victim.data_dir)
            self.note(f"fault {k}: node {victim.index} restored from a verified backup")
        elif kind == "upgrade":
            self.pre_upgrade = (victim.index, archive, victim.binary, victim.binary_sha256 or "")
            f["upgrade"] = {"from_sha256": victim.binary_sha256, "to_sha256": self.upgrade_sha256}
            victim.binary = self.cfg.upgrade_binary
            victim.binary_sha256 = self.upgrade_sha256
            self.upgraded = victim
            self.note(f"fault {k}: node {victim.index} upgraded to {self.upgrade_sha256[:16]}...")
        elif kind == "rollback":
            _, pre_archive, binary, sha = self.pre_upgrade
            os.replace(victim.data_dir, f"{victim.data_dir}.before-rollback-{k}")
            node_backup.restore(pre_archive, victim.data_dir)
            f["rollback"] = {"restored": pre_archive, "to_sha256": sha}
            victim.binary = binary
            victim.binary_sha256 = sha
            self.upgraded = None
            self.note(f"fault {k}: node {victim.index} rolled back to its pre-upgrade store and binary")

    def run_fault(self, k: int) -> None:
        kind = self.cfg.fault_kind(k)
        if kind == "rollback":
            if self.upgraded is None or self.pre_upgrade is None:
                raise Abort(f"fault {k}: a rollback with no upgraded node")
            victim, roles = self.upgraded, ["upgraded"]
        else:
            victim, roles = self.pick_victim(k)
        down_s = self.cfg.down_secs_for(k)
        if k in self.cfg.long_faults:
            roles = roles + ["long-downtime"]
        f: Dict[str, Any] = {"index": k, "node": victim.index, "identity": victim.identity,
                             "roles": roles, "down_s": down_s, "kind": kind}
        others = {i: h for i, h in self.last_heights.items() if i != victim.index}
        f["pre_kill_height"] = self.last_heights.get(victim.index, 0)
        f["lag_before_kill"] = (max(others.values()) - f["pre_kill_height"]
                                if others else None)
        self.note(f"fault {k}: SIGKILL node {victim.index} ({', '.join(roles)})")
        victim.scheduled_down = True
        f["kill_t"] = now()
        victim.kill9()
        f["reaped_t"] = now()
        with self.fault_lock:
            self.fault_state = f
        self.drill(k, kind, victim, f)
        self.stop_event.wait(down_s)
        victim.start(self.genesis)
        f["restart_t"] = now()
        self.note(f"fault {k}: node {victim.index} restarted (incarnation {victim.incarnation})")
        deadline = f["restart_t"] + self.cfg.recovery_budget_s
        probe_recipient: Optional[str] = None
        while now() < deadline and not self.stop_event.is_set():
            if not f.get("process_ready_t"):
                ident = victim.ready_identity()
                if ident is not None:
                    f["identity_verified"] = ident == victim.identity
                    f["process_ready_t"] = now()
                    victim.scheduled_down = False
                    code, health = http_json(victim.rpc, "/health", timeout=3)
                    f["binary_self_report"] = binary_self_report(
                        health, victim.binary_sha256 or self.run["binary_sha256"])
            else:
                code, health = http_json(victim.rpc, "/health", timeout=3)
                if isinstance(health, dict):
                    peers = analyze._int(health.get("peers")) or 0
                    if peers >= 1 and not f.get("first_peer_t"):
                        f["first_peer_t"] = now()
                    if peers >= self.cfg.nodes - 1 and not f.get("full_mesh_t"):
                        f["full_mesh_t"] = now()
                    h = analyze._int(health.get("height"))
                    others = [v for i, v in self.last_heights.items() if i != victim.index]
                    if (h is not None and others and not f.get("caught_up_t")
                            and h >= max(others) - 3):
                        f["caught_up_t"] = now()
                # New work, submitted AFTER the restart through another node,
                # seen from the restarted node's own state.
                if not f.get("first_new_work_t") and self.cfg.workload == "native":
                    # A protocol-4 chain refuses the faucet; use the workload's
                    # own requests instead: one submitted after the restart,
                    # finalized, and visible on the restarted node.
                    rid = self._native_request_settled_after(f["restart_t"])
                    if rid is not None:
                        code, receipt = http_json(victim.rpc, f"/native-inference/receipt/{rid}",
                                                  timeout=3)
                        if isinstance(receipt, dict) and receipt.get("observed_status") == "Finalized":
                            f["first_new_work_t"] = now()
                            f["probe_request_id"] = rid
                elif not f.get("first_new_work_t"):
                    if probe_recipient is None:
                        candidate = "".join(random.choice("0123456789abcdef") for _ in range(64))
                        for other in self.nodes:
                            if other.index == victim.index or other.scheduled_down:
                                continue
                            code, resp = http_json(other.rpc, "/faucet/claim",
                                                   body={"address": candidate}, timeout=5)
                            if code == 200:
                                probe_recipient = candidate
                                f["probe_submitted_t"] = now()
                                f["probe_via"] = other.index
                                break
                    else:
                        code, acct = http_json(victim.rpc, f"/account/{probe_recipient}", timeout=3)
                        if isinstance(acct, dict) and (analyze._int(acct.get("balance")) or 0) > 0:
                            f["first_new_work_t"] = now()
            with self.fault_lock:
                done = all(f.get(p) for p in ("process_ready_t", "first_peer_t", "caught_up_t",
                                              "first_new_work_t", "agreement_t"))
            if done:
                break
            time.sleep(1.0)
        victim.scheduled_down = False
        with self.fault_lock:
            self.fault_state = None
            f["outcome"] = "recovered" if all(
                f.get(p) for p in ("process_ready_t", "first_peer_t", "caught_up_t",
                                   "first_new_work_t", "agreement_t")) else "timeout"
        self.faults.write(f)
        phases = ", ".join(f"{p[:-2]}=+{f[p] - f['restart_t']:.0f}s"
                           for p in ("process_ready_t", "first_peer_t", "caught_up_t",
                                     "first_new_work_t", "agreement_t") if f.get(p))
        self.note(f"fault {k}: {f['outcome']} ({phases})")

    def _native_request_settled_after(self, t: float) -> Optional[str]:
        """A native request the driver submitted after `t` that has settled."""
        try:
            with open(os.path.join(self.cfg.work, "workload.jsonl")) as fh:
                for line in fh:
                    try:
                        item = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if (item.get("final_status") == "finalized"
                            and (analyze._finite(item.get("submitted_t")) or 0) > t):
                        return item.get("request_id")
        except OSError:
            return None
        return None

    # -- workload --------------------------------------------------------
    def faucet_workload(self) -> None:
        """State-changing work through the faucet: each claim is a transfer
        signed by the receiving node, mined, and tracked to its receipt."""
        pending: Dict[str, Dict[str, Any]] = {}
        interval = 1.0 / max(self.cfg.faucet_rate, 1e-6)
        next_submit = now()
        seq = 0
        while not self.stop_event.is_set():
            t = now()
            if t >= next_submit:
                next_submit = t + interval
                up = [n for n in self.nodes if not n.scheduled_down and n.alive()]
                if up:
                    node = up[seq % len(up)]
                    seq += 1
                    recipient = "".join(random.choice("0123456789abcdef") for _ in range(64))
                    item = {"id": f"faucet-{seq}", "kind": "faucet_transfer",
                            "submitted_t": t, "submitted_to": node.index, "recipient": recipient}
                    code, resp = http_json(node.rpc, "/faucet/claim",
                                           body={"address": recipient}, timeout=5)
                    if code == 200 and isinstance(resp, dict) and resp.get("tx_hash"):
                        item["accepted_by"] = node.index
                        item["tx_hash"] = str(resp["tx_hash"]).lower().replace("0x", "")
                        pending[item["id"]] = item
                    elif code == 0:
                        item.update(final_status="submit_error", reason="no HTTP response")
                        self.workload_log.write(item)
                    else:
                        item.update(final_status="rejected",
                                    reason=f"HTTP {code}: {str(resp)[:160]}")
                        self.workload_log.write(item)
            self._resolve(pending)
            time.sleep(0.2)
        # Drain: give accepted items a bounded chance to settle, then record
        # whatever is left as unresolved for the analyzer to account.
        deadline = now() + 60
        while pending and now() < deadline:
            self._resolve(pending)
            time.sleep(1.0)
        for item in pending.values():
            item["final_status"] = "pending"
            self.workload_log.write(item)

    def _resolve(self, pending: Dict[str, Dict[str, Any]]) -> None:
        up = [n for n in self.nodes if not n.scheduled_down and n.alive()]
        if not up or not pending:
            return
        for key in list(pending)[:20]:
            item = pending[key]
            node = random.choice(up)
            code, receipt = http_json(node.rpc, f"/tx/{item['tx_hash']}", timeout=3)
            if code == 200 and isinstance(receipt, dict):
                item["included_height"] = receipt.get("block_height")
                item["settled_t"] = now()
                item["latency_s"] = item["settled_t"] - item["submitted_t"]
                if receipt.get("success") is False:
                    item["final_status"] = "failed"
                    item["reason"] = str(receipt.get("error") or receipt.get("reason")
                                         or "execution failed")[:160]
                else:
                    item["final_status"] = "finalized"
                self.workload_log.write(item)
                del pending[key]

    # -- main loop -------------------------------------------------------
    def execute(self) -> None:
        cfg = self.cfg
        self.start_all()
        self.started_t = now()
        self.run["started_t"] = self.started_t
        self.run["started_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(self.started_t))
        self.write_run()
        workload_thread = None
        driver = None
        if cfg.workload == "native":
            ports = ",".join(str(n.rpc) for n in self.nodes)
            # The driver runs for the planned duration and then drains itself,
            # so the last items are settled or explicitly left pending - never
            # cut off mid-flight by the orchestrator.
            driver = subprocess.Popen(
                [cfg.load_driver, "--rpc", ports, "--rate", str(cfg.native_rate),
                 "--duration", str(int(cfg.duration)),
                 "--out", os.path.join(cfg.work, "workload.jsonl"),
                 "--requester-seed", "soak", "--requesters", str(cfg.native_requesters),
                 "--tuple", ",".join(cfg.native_tuple)] + self.driver_executor_args(),
                stdout=open(os.path.join(cfg.work, "load-driver.log"), "ab"),
                stderr=subprocess.STDOUT, start_new_session=True)
        if cfg.workload == "faucet":
            workload_thread = threading.Thread(target=self.faucet_workload, daemon=True)
            workload_thread.start()
        first_fault_at = self.started_t + cfg.warmup_s + cfg.baseline_s
        next_fault_at = first_fault_at
        fault_thread: Optional[threading.Thread] = None
        faults_started = 0
        end_at = self.started_t + cfg.duration
        while now() < end_at and not self.stop_event.is_set():
            tick = now()
            recs = self.sample()
            self.check_agreement(recs)
            for r in recs:
                if not r["alive"] and not r["scheduled_down"]:
                    self.note(f"UNSCHEDULED: node {r['node']} process is not running")
            if (faults_started < cfg.planned_faults and tick >= next_fault_at
                    and (fault_thread is None or not fault_thread.is_alive())):
                fault_thread = threading.Thread(target=self.run_fault, args=(faults_started,),
                                                daemon=True)
                fault_thread.start()
                faults_started += 1
                next_fault_at = tick + cfg.fault_every_s
            time.sleep(max(0.0, cfg.sample_s - (now() - tick)))
        self.stop_event.set()
        if fault_thread is not None:
            fault_thread.join(timeout=cfg.recovery_budget_s + 60)
        if workload_thread is not None:
            workload_thread.join(timeout=120)
        if driver is not None:
            try:
                driver.wait(timeout=150)
            except subprocess.TimeoutExpired:
                driver.kill()
                self.note("load driver did not finish draining; killed")
        self.run["completed"] = True
        self.run["abort_reason"] = None
        self.run["actual_duration_s"] = now() - self.started_t
        self.run["executed_faults"] = faults_started

    def shutdown(self) -> None:
        self.stop_event.set()
        for node in self.nodes:
            try:
                node.stop()
            except Exception:
                pass
        self.pool.shutdown(wait=False)
        self.run["ended_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        if self.started_t and not self.run.get("actual_duration_s"):
            self.run["actual_duration_s"] = now() - self.started_t
        self.write_run()


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description="Recorded ARC soak (checklist R8)")
    p.add_argument("--self-test", action="store_true",
                   help="7 minutes, one fault on the seed: exercises the harness, is not a soak")
    p.add_argument("--hours", type=float)
    p.add_argument("--minutes", type=float)
    p.add_argument("--binary", required=True)
    p.add_argument("--provenance", required=True,
                   help="build record from scripts/arc-build-provenance.sh; must name --binary")
    p.add_argument("--nodes", type=int, default=4)
    p.add_argument("--base-rpc", type=int, default=9960)
    p.add_argument("--base-p2p", type=int, default=9160)
    p.add_argument("--sample-secs", type=float, default=15.0)
    p.add_argument("--fault-every-secs", type=float, default=3600.0)
    p.add_argument("--no-faults", action="store_true",
                   help="schedule no fault at all (measurement runs); otherwise the first fault "
                        "comes after the baseline and one every --fault-every-secs")
    p.add_argument("--snapshot-every", type=int, default=500)
    p.add_argument("--down-secs", type=float, default=20.0,
                   help="how long a killed node stays down before it is restarted")
    p.add_argument("--long-down-secs", type=float, default=0.0,
                   help="downtime for the faults named by --long-faults")
    p.add_argument("--long-faults", default="",
                   help="comma-separated fault indices (0-based) that use --long-down-secs")
    p.add_argument("--recovery-budget-secs", type=float, default=180.0,
                   help="time after a restart within which recovery must complete")
    p.add_argument("--fault-kinds", default="",
                   help="comma-separated kind per fault index: kill, restore, upgrade, rollback "
                        "(R6 drills); unlisted faults are kills")
    p.add_argument("--upgrade-binary", help="binary an upgrade drill restarts its node on")
    p.add_argument("--upgrade-provenance",
                   help="build record for --upgrade-binary (checked like --provenance)")
    p.add_argument("--workload", choices=["none", "faucet", "native"], default="faucet")
    p.add_argument("--native-rate", type=float, default=0.2,
                   help="native-inference requests per second offered (workload=native)")
    p.add_argument("--allow-battery", action="store_true",
                   help="start a run longer than 30 minutes on battery power; a sleep on low "
                        "battery freezes the whole run")
    p.add_argument("--native-requesters", type=int, default=4,
                   help="requester accounts, i.e. the most requests in flight at once")
    p.add_argument("--load-driver",
                   help="soak_native_load binary (workload=native); must be built with the "
                        "native-test-executor feature alongside --binary")
    p.add_argument("--faucet-rate", type=float, default=0.5, help="claims per second offered")
    p.add_argument("--real-model", metavar="GGUF",
                   help="run the REAL canonical executor on this artifact (P8) instead of the "
                        "deterministic test executor; needs --qualification, --package-manifest "
                        "and --input-hex. The harness never writes a qualification record")
    p.add_argument("--qualification", help="the owner's real-execution qualification record")
    p.add_argument("--package-manifest", help="the approved arc.model-package.v1 manifest")
    p.add_argument("--input-hex", help="every request's input: little-endian u32 token ids "
                                       "(e.g. from the node's /native-inference/tokenize)")
    p.add_argument("--max-tokens", type=int, help="tokens each request asks for (default 8)")
    p.add_argument("--allow-multiple-real-models", action="store_true",
                   help="allow --real-model with more than one node (each loads the model)")
    p.add_argument("--native-workers", type=int,
                   help="run the native worker on only the first N nodes (default: all). With "
                        "less than 2/3 of the stake voting no request can be certified, so every "
                        "admitted request expires: how a product journey exercises refunds. "
                        "Such a run is labelled in run.json and is not a stability run")
    p.add_argument("--fund", action="append", default=[], metavar="ADDRESS[:BASE_UNITS]",
                   help="also fund this account in genesis (repeatable; default 100 ARC), e.g. a "
                        "desktop wallet for a product journey")
    p.add_argument("--rust-log", default="info")
    p.add_argument("--work")
    return p


# A laptop that sleeps on low battery freezes every process of a run: on
# 2026-09-22 a 24-hour soak lost 71 minutes that way, and the chain's heights
# still rose across the gap. A run longer than this refuses to start on
# battery power unless the operator accepts that risk.
BATTERY_REFUSAL_S = 1800.0


def parse_power(text: str) -> Optional[Dict[str, Any]]:
    """`pmset -g batt` output: the power source and the charge, when shown."""
    source = re.search(r"Now drawing from '([^']+)'", text)
    if not source:
        return None
    percent = re.search(r"(\d+)%", text)
    return {"source": source.group(1), "percent": int(percent.group(1)) if percent else None}


def host_power() -> Optional[Dict[str, Any]]:
    """This host's power source where the platform reports it (macOS)."""
    if sys.platform != "darwin":
        return None
    try:
        out = subprocess.run(["pmset", "-g", "batt"], capture_output=True, text=True, timeout=5)
    except (OSError, subprocess.SubprocessError):
        return None
    return parse_power(out.stdout)


def battery_refusal(duration_s: float, power: Optional[Dict[str, Any]],
                    allowed: bool) -> Optional[str]:
    """Why a run must not start on this power, or None."""
    if allowed or power is None or duration_s <= BATTERY_REFUSAL_S:
        return None
    if power.get("source") != "Battery Power":
        return None
    return (f"REFUSING: this host is on battery ({power.get('percent')}%) and the run lasts "
            f"{duration_s / 3600:.1f} h. A sleep on low battery freezes the whole run; plug "
            "in, or pass --allow-battery to accept that.")


def executor_label_for(cfg: Any) -> Optional[str]:
    """The executor a native run's nodes execute requests with, as recorded in
    run.json and in every workload record. A real-model run must never carry
    the deterministic executor's label, or the reverse."""
    if getattr(cfg, "workload", None) != "native":
        return None
    if getattr(cfg, "real_model", None):
        return f"canonical-i8-real-model qualification={cfg.qualification}"
    # The load driver's own default label.
    return "deterministic-test-executor"


def driver_executor_args_for(cfg: Any) -> List[str]:
    """The load driver's input for the executor the nodes run."""
    if not getattr(cfg, "real_model", None):
        return []
    return ["--input-hex", cfg.input_hex, "--max-tokens", str(cfg.max_tokens),
            "--executor-label", executor_label_for(cfg)]


def main(argv: Optional[List[str]] = None) -> int:
    a = build_parser().parse_args(argv)
    if a.workload == "native" and not a.load_driver:
        raise SystemExit("--workload native needs --load-driver")
    cfg = Config(a)
    soak = Soak(cfg)
    soak.prepare()

    def on_signal(signum, _frame):
        raise Abort(f"received signal {signum}")
    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGINT, on_signal)
    try:
        soak.execute()
    except BaseException as exc:
        soak.run["completed"] = False
        soak.run["abort_reason"] = f"{type(exc).__name__}: {exc}"
        soak.note(f"ABORTED: {soak.run['abort_reason']}")
    finally:
        soak.shutdown()
    # Rename per-incarnation logs into the analyzer's node-*.log glob? They
    # already match it (node-<i>.<k>.log).
    code = analyze.main([cfg.work])
    print(f"exit status {code} ({[k for k, v in analyze.EXIT_CODES.items() if v == code][0]})")
    return code


if __name__ == "__main__":
    sys.exit(main())
