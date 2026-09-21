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
from typing import Any, Dict, List, Optional, Tuple

from arc_soak import analyze

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
            m = re.match(r"^([a-z_]+):\s*(.*?)\s*$", line)
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

    def args(self, genesis: str) -> List[str]:
        peers = ",".join(f"127.0.0.1:{self.cfg.base_p2p + j}"
                         for j in range(self.cfg.nodes) if j != self.index)
        return [self.cfg.binary, "--rpc", f"127.0.0.1:{self.rpc}",
                "--p2p-port", str(self.p2p), "--data-dir", self.data_dir,
                "--genesis", genesis, "--peers", peers,
                "--insecure-dev-validator-seed", "--validator-seed", f"soak-node-{self.index}",
                "--stake", str(self.cfg.stake),
                "--snapshot-every-blocks", str(self.cfg.snapshot_every)]

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

class Config:
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
        self.binary = os.path.abspath(a.binary)
        self.provenance = os.path.abspath(a.provenance)
        self.nodes = a.nodes
        self.base_rpc = a.base_rpc
        self.base_p2p = a.base_p2p
        self.stake = 6666667
        self.snapshot_every = a.snapshot_every
        self.down_s = 20.0
        self.recovery_budget_s = 180.0
        self.rust_log = a.rust_log
        self.workload = a.workload
        self.faucet_rate = a.faucet_rate
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
        busy = [p for i in range(cfg.nodes)
                for p in (cfg.base_rpc + i, cfg.base_p2p + i) if port_busy(p)]
        if busy:
            raise SystemExit(f"REFUSING: ports in use: {busy}")
        self.run = {
            "schema": "arc-soak-run/1",
            "mode": cfg.mode,
            "completed": False,
            "abort_reason": "the orchestrator has not finished",
            "planned_duration_s": cfg.duration,
            "sample_interval_s": cfg.sample_s,
            "planned_faults": cfg.planned_faults,
            "binary": cfg.binary,
            "binary_sha256": used,
            "provenance": {"file": cfg.provenance, "recorded_binary_sha256": recorded,
                           "source_revision": prov.get("source_revision"),
                           "dirty_files": prov.get("dirty_files"),
                           "input_digest": prov.get("input_digest"),
                           "command": prov.get("command")},
            "host": " ".join(os.uname()),
            "rust_log": cfg.rust_log,
            "workload": {"profile": cfg.workload, "required": cfg.workload != "none",
                         "faucet_rate_per_s": cfg.faucet_rate if cfg.workload == "faucet" else None},
            "thresholds": {"recovery_budget_s": cfg.recovery_budget_s},
            "nodes": [],
        }
        self.write_run()
        self.samples = JsonlWriter(os.path.join(cfg.work, "samples.jsonl"))
        self.agreement = JsonlWriter(os.path.join(cfg.work, "agreement.jsonl"))
        self.faults = JsonlWriter(os.path.join(cfg.work, "faults.jsonl"))
        self.workload_log = JsonlWriter(os.path.join(cfg.work, "workload.jsonl"))
        self.events = open(os.path.join(cfg.work, "events.log"), "a")
        for node in self.nodes:
            node.identity = derive_identity(cfg, node.index)
        time.sleep(2)
        self.run["nodes"] = [{"index": n.index, "identity": n.identity, "rpc": n.rpc,
                              "p2p": n.p2p, "role": "seed" if n.index == 0 else "member"}
                             for n in self.nodes]
        self.write_run()
        with open(self.genesis, "w") as fh:
            fh.write('[chain]\nname = "arc-soak"\nchain_id = "0x415243"\n'
                     "validator_set_complete = false\n\n")
            fh.write(f'[[accounts]]\naddress = "{FAUCET_POOL}"\nbalance = 1_000_000_000_000\n\n')
            for n in self.nodes:
                fh.write(f'[[accounts]]\naddress = "{n.identity}"\nbalance = 1_000_000_000_000\n\n')
            for n in self.nodes:
                fh.write(f'[[validators]]\naddress = "{n.identity}"\nstake = {cfg.stake}\n\n')

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
        rec["disk_bytes"] = dir_bytes(node.data_dir)
        rec["log_bytes"] = sum(os.path.getsize(os.path.join(self.cfg.work, f))
                               for f in os.listdir(self.cfg.work)
                               if f.startswith(f"node-{node.index}.") and f.endswith(".log"))
        return rec

    def sample(self) -> List[Dict[str, Any]]:
        recs = list(self.pool.map(self.probe, self.nodes))
        for r in recs:
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

    def run_fault(self, k: int) -> None:
        victim, roles = self.pick_victim(k)
        f: Dict[str, Any] = {"index": k, "node": victim.index, "identity": victim.identity,
                             "roles": roles}
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
        time.sleep(self.cfg.down_s)
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
                if not f.get("first_new_work_t"):
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


def main(argv: Optional[List[str]] = None) -> int:
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
    p.add_argument("--snapshot-every", type=int, default=500)
    p.add_argument("--workload", choices=["none", "faucet"], default="faucet")
    p.add_argument("--faucet-rate", type=float, default=0.5, help="claims per second offered")
    p.add_argument("--rust-log", default="info")
    p.add_argument("--work")
    a = p.parse_args(argv)
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
