#!/usr/bin/env python3
"""Offline tests for statecompat_lab.py. Standard library only; no network, no real arc-node.

The end-to-end tests drive the whole tier runner against a FAKE harness and a FAKE node binary (small
scripts written below) so that the orchestration, the judging and the evidence shapes are exercised before
the real run. The fakes serve the same JSON shapes the real nodes serve; they prove the lab's logic, not
the node's behaviour.
"""
import argparse
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
import statecompat_lab as L  # noqa: E402


def h(n):
    return hashlib.sha256(("block%d" % n).encode()).hexdigest()


def r(n):
    return hashlib.sha256(("root%d" % n).encode()).hexdigest()


def chain(upto, first=3):
    rows = []
    for height in range(0, upto + 1):
        if height < first:
            rows.append({"h": height, "missing": True})
        else:
            rows.append({"h": height, "hash": h(height), "state_root": r(height), "parent_hash": h(height - 1),
                         "tx_count": 0, "protocol_major": 3})
    return rows


def node_obs(port, height, sha="a" * 64, version="0.8.10", participation=True):
    return {
        "port": port,
        "health": {"height": height, "version": version, "binary_sha256": sha, "peers": 5,
                   "chain_participation_enabled": participation, "features": []},
        "network_info": {"last_block_height": height, "last_block_hash": "0x" + h(height)},
        "snapshot_info": {"height": height, "state_root": "0x" + r(height)},
        "blocks": chain(height),
    }


def observed_from(expected_height, sha="a" * 64, version="0.8.10", exit_code=0, participation=False, port=9980, samples=3):
    base = node_obs(port, expected_height, sha, version, participation)
    one = {"unix": 1, "health": base["health"], "network_info": base["network_info"], "snapshot_info": base["snapshot_info"]}
    return {"started": True, "port": port, "samples": [dict(one) for _ in range(samples)], "blocks": base["blocks"],
            "exit_code": exit_code}


class PureLogicTests(unittest.TestCase):
    def test_norm_hex(self):
        self.assertEqual(L.norm_hex("0xAbC"), "abc")
        self.assertEqual(L.norm_hex("abc"), "abc")
        self.assertIsNone(L.norm_hex(None))

    def test_kind_of_abstracts_nodes_numbers_and_hex(self):
        self.assertEqual(L.kind_of("node-3/generations/gen-000012/wal-00000002.bin"), "node-N/generations/gen-<n>/wal-<n>.bin")
        self.assertEqual(L.kind_of("node-0/pins/" + "ab" * 16 + ".json"), "node-N/pins/<hex>.json")

    def test_manifest_copy_delta(self):
        with tempfile.TemporaryDirectory() as tmp:
            src = Path(tmp) / "src"
            (src / "node-0" / "gen-1").mkdir(parents=True)
            (src / "node-0" / "gen-1" / "state.bin").write_bytes(b"abc")
            (src / "node-0" / "node.log").write_text("log\n")
            (src / "node-0" / "key").write_bytes(b"k")
            os.chmod(str(src / "node-0" / "key"), 0o600)
            before = L.build_manifest(src, "before")
            self.assertEqual({r_["class"] for r_ in before["rows"] if r_["type"] == "file"}, {"state", "log"})
            dst = Path(tmp) / "dst"
            L.copy_tree(src, dst)
            copy = L.build_manifest(dst, "copy")
            self.assertTrue(L.manifests_equal(before, copy))
            with self.assertRaises(L.LabError):
                L.copy_tree(src, dst)
            (dst / "node-0" / "gen-2").mkdir()
            (dst / "node-0" / "gen-2" / "state.bin").write_bytes(b"abcd")
            (dst / "node-0" / "newkind.dat").write_bytes(b"x")
            (dst / "node-0" / "node.log").write_text("log\nmore\n")
            after = L.build_manifest(dst, "after")
            self.assertFalse(L.manifests_equal(before, after))
            delta = L.manifest_delta(before, after)
            self.assertIn("node-0/newkind.dat", delta["added"])
            self.assertEqual(delta["new_file_kinds"], ["node-N/newkind.dat"])
            self.assertEqual(delta["vanished_file_kinds"], [])
            self.assertEqual([m["path"] for m in delta["modified"]], ["node-0/node.log"])
            self.assertEqual(delta["state_files_modified"], [])

    def test_scan_log_signatures(self):
        text = "\n".join([
            "2026-10-07T10:00:00.123456Z  INFO arc_node: started height=12 hash=0x" + "ab" * 32,
            "2026-10-07T10:00:01.000000Z \x1b[31mERROR\x1b[0m arc_node: cannot connect to seed 10.0.0.1:9090",
            "2026-10-07T10:00:02.000000Z  WARN arc_node: failed to decode record at offset 123",
            "2026-10-07T10:00:03.000000Z  INFO arc_consensus: EQUIVOCATION DETECTED - validator produced two blocks",
        ])
        scan = L.scan_log(text)
        self.assertEqual(scan["levels"], {"INFO": 2, "ERROR": 1, "WARN": 1})
        self.assertEqual(len(scan["error_signatures"]), 1)
        self.assertEqual(len(scan["hard_signatures"]), 2)
        later = L.scan_log(text.replace("offset 123", "offset 9999") + "\n2026-10-07T10:00:05Z ERROR x: boom")
        self.assertEqual(L.new_signatures(later, scan), ["ERROR x: boom"])
        self.assertEqual(L.new_signatures(scan, None), sorted(set(scan["error_signatures"]) | set(scan["hard_signatures"])))

    def test_anti_equivocation_info_line_is_not_damage_and_paths_do_not_matter(self):
        a = "2026-10-07T10:51:12Z  INFO arc_node::consensus: Loaded consensus anti-equivocation record path=/home/runner/work/_temp/work/prod/d4-control-validator/node-0/consensus-signing-record.bin absences=1 finality=2"
        b = a.replace("d4-control-validator", "d4-reopen-validator").replace("absences=1", "absences=7")
        scan_a, scan_b = L.scan_log(a), L.scan_log(b)
        self.assertEqual(scan_a["hard_signatures"], [])
        self.assertEqual(scan_a["error_signatures"], [])
        self.assertEqual(L.log_signature(a), L.log_signature(b))
        real = L.scan_log("2026-10-07T10:51:12Z ERROR arc_consensus: EQUIVOCATION DETECTED - validator produced two blocks in the same round\n"
                          "2026-10-07T10:51:13Z  WARN arc_consensus: Slash applied for DAG equivocation validator=ab")
        self.assertEqual(len(real["hard_signatures"]), 2)

    def test_errors_during_an_orderly_shutdown_are_listed_apart(self):
        text = "\n".join([
            "2026-10-07T11:21:34Z  INFO arc_node: lifecycle signal handlers armed before node initialization",
            "2026-10-07T11:21:35.458Z  INFO arc_node: SIGTERM received - stopping HTTP/background admission and draining active work",
            "2026-10-07T11:21:35.460Z ERROR arc_node::consensus: Fatal recovery DAG pre-advance re-broadcast failure round=330 error=recovery DAG outbound transport channel is closed",
            "2026-10-07T11:21:35.537Z  INFO arc_node: RPC handlers drained, node writers joined, WAL durability barrier completed, and the desktop receipt was acknowledged; shutdown is clean",
            "2026-10-07T11:21:35.571Z  INFO arc_node: lifecycle signal handlers armed before node initialization",
            "2026-10-07T11:21:40.000Z ERROR arc_node: a genuine error in the next life",
        ])
        scan = L.scan_log(text)
        self.assertEqual(scan["error_signatures"], ["ERROR arc_node: a genuine error in the next life"])
        self.assertEqual(len(scan["shutdown_error_signatures"]), 1)
        self.assertIn("re-broadcast failure", scan["shutdown_error_signatures"][0])
        self.assertEqual((scan["sigterms"], scan["clean_shutdowns"]), (1, 1))
        # the shutdown-phase error is not "new damage" against a control that lacks it
        control = L.scan_log("2026-10-07T11:00:00Z  INFO ok")
        self.assertEqual(L.new_signatures(scan, control), ["ERROR arc_node: a genuine error in the next life"])

    def test_history_preserved(self):
        a = {"nodes": [node_obs(9980, 10), node_obs(9981, 10)]}
        b = {"nodes": [node_obs(9980, 15), node_obs(9981, 15)]}
        self.assertTrue(L.history_preserved(a, b)["ok"])
        self.assertFalse(L.history_preserved(b, a)["ok"])
        b["nodes"][1]["blocks"][7]["hash"] = "ff" * 32
        self.assertFalse(L.history_preserved(a, b)["ok"])
        self.assertFalse(L.history_preserved({"nodes": []}, b)["ok"])

    def test_node_argv_shapes(self):
        spec = {"index": 2, "mode": "observer", "binary": "/b/arc-node", "data_dir": "/d/node-2", "genesis": "/f/genesis.toml",
                "activation": "/f/activation.json", "key_file": "/f/validator-2.key.json", "native_runtime": True}
        observer = L.node_argv(spec)
        self.assertEqual(observer[observer.index("--stake") + 1], "0")
        for banned in ("--peers", "--validator-key-file", "--recovery-checkpoint", "--native-inference-runtime"):
            self.assertNotIn(banned, observer)
        self.assertIn("--archive", observer)
        validator = L.node_argv(dict(spec, mode="validator"))
        self.assertEqual(validator[validator.index("--stake") + 1], str(L.STAKES[2]))
        self.assertIn("--validator-key-file", validator)
        self.assertIn("--native-inference-test-executor", validator)
        self.assertNotIn("--peers", validator)
        self.assertNotIn("--native-inference-runtime", L.node_argv(dict(spec, mode="validator", native_runtime=False)))
        self.assertEqual(observer[observer.index("--rpc") + 1], "127.0.0.1:%d" % (L.BASE_RPC + 2))


class JudgeTests(unittest.TestCase):
    SHA = "b" * 64

    def expected(self, height=470, port=9980):
        return L.expected_from_obs(node_obs(port, height))

    def judge(self, observed, expected=None, scan_new=(), stable=True, sha=None, version="0.8.10"):
        return L.judge_node(expected or self.expected(), observed, sha or self.SHA, version, stable, list(scan_new))

    def good(self, height=470, **kw):
        return observed_from(height, sha=self.SHA, **kw)

    def names_failed(self, verdict):
        return {c["check"] for c in verdict["checks"] if not c["ok"]}

    def test_pass(self):
        verdict = self.judge(self.good())
        self.assertTrue(verdict["ok"], self.names_failed(verdict))

    def test_pass_with_extra_blocks_and_validator_mode(self):
        verdict = self.judge(self.good(height=475), stable=False)
        self.assertTrue(verdict["ok"], self.names_failed(verdict))
        # a validator may differ between samples (not stable-required): add a differing sample
        observed = self.good(height=475)
        observed["samples"][0]["health"] = dict(observed["samples"][0]["health"], height=474)
        self.assertTrue(self.judge(observed, stable=False)["ok"])
        self.assertFalse(self.judge(observed, stable=True)["ok"])

    def test_stale_copy_fails_freshness_and_history(self):
        verdict = self.judge(self.good(height=400))
        failed = self.names_failed(verdict)
        self.assertIn(L.FRESHNESS, failed)
        self.assertIn("every block the candidate served is served identically (hash and state root)", failed)

    def test_each_mutation_is_caught(self):
        base = self.good()

        def mutate(fn):
            observed = json.loads(json.dumps(base))
            fn(observed)
            return self.judge(observed)

        cases = {
            "block hash altered": lambda o: o["blocks"][100].update(hash="00" * 32),
            "state root altered": lambda o: o["blocks"][100].update(state_root="11" * 32),
            "block missing": lambda o: o["blocks"][100].update(missing=True),
            "wrong binary": lambda o: o["samples"][-1]["health"].update(binary_sha256="c" * 64),
            "wrong version": lambda o: o["samples"][-1]["health"].update(version="0.8.11"),
            "not started": lambda o: o.update(started=False, start_error="boom"),
            "unclean exit": lambda o: o.update(exit_code=1),
            "killed on term": lambda o: o.update(exit_code="killed after 60 s without exiting on SIGTERM"),
            "live root differs from header": lambda o: o["samples"][-1]["snapshot_info"].update(state_root="0x" + "22" * 32),
            "tip hash differs": lambda o: o["samples"][-1]["network_info"].update(last_block_hash="0x" + "33" * 32),
            "consensus running (observer)": lambda o: o["samples"][-1]["health"].update(chain_participation_enabled=True),
            "height below": lambda o: o["samples"][-1]["health"].update(height=469),
            "unstable samples": lambda o: o["samples"][0]["health"].update(height=469),
        }
        for name, fn in cases.items():
            self.assertFalse(mutate(fn)["ok"], name)
        self.assertFalse(self.judge(base, scan_new=["ERROR new thing"])["ok"])
        self.assertTrue(self.judge(base)["ok"])

    def test_reopen_aggregate_and_agreement(self):
        expected = [self.expected(470 + i, 9980 + i) for i in range(3)]
        observed = [observed_from(470 + i, sha=self.SHA, port=9980 + i) for i in range(3)]
        scans = {9980 + i: L.scan_log("INFO ok") for i in range(3)}
        verdict = L.judge_reopen(expected, observed, self.SHA, "0.8.10", True, scans, scans)
        self.assertTrue(verdict["ok"], L.failed_checks(verdict))
        observed[1]["blocks"][50]["state_root"] = "99" * 32
        verdict = L.judge_reopen(expected, observed, self.SHA, "0.8.10", True, scans, scans)
        self.assertFalse(verdict["ok"])
        self.assertFalse(verdict["agreement"]["ok"])
        # one node alone: agreement not applicable
        single = L.judge_reopen(expected[:1], observed[:1], self.SHA, "0.8.10", True, scans, scans)
        self.assertTrue(single["ok"])
        self.assertEqual(L.failed_checks(single), [])


STUB_NODE = textwrap.dedent('''
    import hashlib, json, os, signal, sys, threading
    from http.server import BaseHTTPRequestHandler, HTTPServer

    def digest(n, tag):
        return hashlib.sha256(("%s%d" % (tag, n)).encode()).hexdigest()

    version = sys.argv[1]
    args = sys.argv[2:]
    if "--version" in args:
        print("arc-node " + version)
        sys.exit(0)
    def opt(name, default=None):
        return args[args.index(name) + 1] if name in args else default
    rpc = opt("--rpc")
    data = opt("--data-dir")
    stake = int(opt("--stake", "0"))
    open(os.path.join(data, "argv.txt"), "a").write(" ".join(args) + "\\n")
    state = json.load(open(os.path.join(data, "chain.json")))
    # damage detection: the records file must match the digest chain.json recorded; a validator also checks
    # that the signing record belongs to it.
    records = os.path.join(data, "dag-wal", "records.bin")
    if os.path.exists(records) and hashlib.sha256(open(records, "rb").read()).hexdigest() != state["records_sha256"]:
        sys.stderr.write("ERROR arc_node: dag-wal records.bin checksum mismatch\\n")
        sys.exit(3)
    index = int(opt("--rpc").rsplit(":", 1)[1]) - 9980
    if stake > 0:
        signing = open(os.path.join(data, "consensus-signing-record.bin"), "rb").read()
        if not signing.startswith(("NODE%d-" % index).encode()):
            sys.stderr.write("ERROR arc_node: signing record belongs to another validator\\n")
            sys.exit(4)
    if stake > 0:
        sys.stderr.write("2026-10-07T10:51:12Z  INFO arc_node::consensus: Loaded consensus anti-equivocation record path=%s absences=1 finality=2\\n"
                         % os.path.join(data, "consensus-signing-record.bin"))
        sys.stderr.flush()
    height = state["height"]
    sha = hashlib.sha256(open(sys.argv[0] if False else os.environ["STUB_SELF"], "rb").read()).hexdigest()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass
        def send(self, code, obj):
            body = json.dumps(obj).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        def do_GET(self):
            path = self.path
            if path == "/health":
                self.send(200, {"status": "ok", "version": version, "height": height, "peers": 0 if stake == 0 else 5,
                                "chain_participation_enabled": stake > 0, "binary_sha256": sha,
                                "features": json.loads(os.environ.get("STUB_FEATURES", "[]"))})
            elif path == "/network/info":
                self.send(200, {"height": height, "last_block_height": height, "last_block_hash": "0x" + digest(height, "block")})
            elif path == "/sync/snapshot/info":
                self.send(200, {"height": height, "state_root": "0x" + digest(height, "root")})
            elif path.startswith("/block/"):
                n = int(path.rsplit("/", 1)[1])
                if n < 3 or n > height:
                    self.send(404, {"error": "no"})
                else:
                    self.send(200, {"hash": digest(n, "block"), "header": {"state_root": digest(n, "root"),
                                "parent_hash": digest(n - 1, "block"), "tx_count": 0, "protocol_version": {"major": 3}},
                                "tx_hashes": []})
            else:
                self.send(404, {"error": "no"})

    server = HTTPServer(("127.0.0.1", int(rpc.rsplit(":", 1)[1])), Handler)
    def stop(*_):
        threading.Thread(target=server.shutdown).start()
    signal.signal(signal.SIGTERM, stop)
    server.serve_forever()
    sys.exit(0)
''')

FAKE_HARNESS = textwrap.dedent('''
    import hashlib, json, os, shutil, subprocess, sys, time
    from pathlib import Path

    def digest(n, tag):
        return hashlib.sha256(("%s%d" % (tag, n)).encode()).hexdigest()

    step = os.environ["LAB_STEP"]
    tier = os.environ["LAB_TIER"]
    out = Path(os.environ["LAB_OUT"])
    root = Path(os.environ["LAB_ROOT"])

    def chain(upto):
        return [({"h": h, "missing": True} if h < 3 else {"h": h, "hash": digest(h, "block"), "state_root": digest(h, "root"),
                 "parent_hash": digest(h - 1, "block"), "tx_count": 0, "protocol_major": 3}) for h in range(upto + 1)]

    def node(port, height, sha, version, features):
        return {"port": port, "health": {"height": height, "version": version, "binary_sha256": sha, "peers": 5,
                                         "chain_participation_enabled": True, "features": features},
                "network_info": {"last_block_height": height, "last_block_hash": "0x" + digest(height, "block")},
                "snapshot_info": {"height": height, "state_root": "0x" + digest(height, "root")},
                "blocks": chain(height)}

    if step == "fixture":
        fx = root / "fixture"
        fx.mkdir(parents=True)
        (fx / "genesis.toml").write_text("[chain]\\nname = 'fake'\\n")
        (fx / "activation.json").write_text("{}")
        (fx / "approved.arcchkpt").write_bytes(b"checkpoint")
        for i in range(6):
            (fx / ("validator-%d.key.json" % i)).write_text('{"secret_key": "00"}')
            os.chmod(str(fx / ("validator-%d.key.json" % i)), 0o600)
        (fx / "fixture.json").write_text(json.dumps({"manifest": "ab" * 32, "activation_height": 270}))
        out.write_text(json.dumps({"step": "fixture", "complete": True}))
        sys.exit(0)

    binary = os.environ["LAB_BIN"]
    expect = os.environ["LAB_EXPECT_SHA256"]
    actual = hashlib.sha256(open(binary, "rb").read()).hexdigest()
    if expect and actual != expect:
        sys.stderr.write("wrong binary\\n")
        sys.exit(9)
    version = subprocess.run([binary, "--version"], capture_output=True, text=True).stdout.split()[-1]
    features = ["native-test-executor"] if tier == "native" else ["candle"]
    data = Path(os.environ["LAB_DATA"])
    heights = {"old_run": (None, 400), "new_run": (400, 470), "resume": (470, 500)}
    start_h, end_h = heights[step]

    def sign_record(i, who):
        return ("NODE%d-%s" % (i, who)).encode()

    events = []
    def report(label):
        events.append({"event": "started", "detail": {"label": label, "binary_report": [
            {"port": 9980 + i, "binary_sha256": actual, "features": features, "version": version} for i in range(6)]}})

    observations = []
    if step == "old_run":
        for i in range(6):
            d = data / ("node-%d" % i)
            (d / "generations" / "gen-1").mkdir(parents=True)
            (d / "dag-wal").mkdir()
            (d / "generations" / "gen-1" / "state.bin").write_bytes(("state-%d" % i).encode() * 40)
            rec = ("records-%d" % i).encode() * 100
            (d / "dag-wal" / "records.bin").write_bytes(rec)
            (d / "recovery.active").write_text("pinned")
            (d / ".arc-node.lock").write_text("")
            (d / "consensus-signing-record.bin").write_bytes(sign_record(i, "old"))
            (d / "node.log").write_text("INFO started old\\n")
            (d / "chain.json").write_text(json.dumps({"height": end_h, "records_sha256": hashlib.sha256(rec).hexdigest()}))
        report("old")
        observations.append({"label": "start", "nodes": [node(9980 + i, 12, actual, version, features) for i in range(6)]})
    else:
        for i in range(6):
            d = data / ("node-%d" % i)
            rec = (d / "dag-wal" / "records.bin").read_bytes() + (b"more-%d" % i) * 30
            (d / "dag-wal" / "records.bin").write_bytes(rec)
            (d / "generations" / "gen-2").mkdir(exist_ok=True)
            (d / "generations" / "gen-2" / "state.bin").write_bytes(("state2-%d" % i).encode() * 40)
            (d / "consensus-signing-record.bin").write_bytes(sign_record(i, step))
            with open(str(d / "node.log"), "a") as stream:
                stream.write("INFO started %s\\nWARN peer slow\\nINFO arc_node::consensus: Loaded consensus anti-equivocation record path=%s absences=1 finality=2\\n" % (step, d / "consensus-signing-record.bin"))
            (d / "chain.json").write_text(json.dumps({"height": end_h, "records_sha256": hashlib.sha256(rec).hexdigest()}))
        report(step)
        observations.append({"label": "start", "nodes": [node(9980 + i, start_h, actual, version, features) for i in range(6)]})
    observations.append({"label": "final", "nodes": [node(9980 + i, end_h, actual, version, features) for i in range(6)]})
    workload = {"transfers": [{"tx": "aa", "height": 5}]}
    if tier == "native":
        workload["paid_requests"] = [{"phase": step, "request_id": "bb" * 32, "settlement": ["Finalized", 5, "cc", []]}]
    out.write_text(json.dumps({"step": step, "tier": tier, "complete": True, "events": events, "observations": observations,
                               "workload": workload}))
    sys.exit(0)
''')


def make_exec(path, text):
    Path(path).write_text(text)
    os.chmod(str(path), 0o755)


class EndToEndTests(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="statecompat-test-"))
        os.environ["STATECOMPAT_ALLOW_TEST_OVERRIDES"] = "1"
        (self.tmp / "stub_node.py").write_text(STUB_NODE)
        (self.tmp / "fake_harness.py").write_text(FAKE_HARNESS)
        self.harness = self.tmp / "harness"
        make_exec(self.harness, "#!/bin/sh\nexec %s %s \"$@\"\n" % (sys.executable, self.tmp / "fake_harness.py"))
        self.old = self.make_node("old", "0.8.10")
        self.new = self.make_node("new", "0.8.11")

    def tearDown(self):
        os.environ.pop("STATECOMPAT_ALLOW_TEST_OVERRIDES", None)
        shutil.rmtree(str(self.tmp), ignore_errors=True)

    def make_node(self, name, version, features="[]"):
        path = self.tmp / ("arc-node-" + name)
        make_exec(path, "#!/bin/sh\nSTUB_SELF=%s STUB_FEATURES='%s' exec %s %s %s \"$@\"\n"
                  % (path, features, sys.executable, self.tmp / "stub_node.py", version))
        return path

    def runner(self, tier, old=None, new=None):
        old = old or self.old
        new = new or self.new
        args = argparse.Namespace(
            tier=tier, harness=str(self.harness), old_bin=str(old), new_bin=str(new), old_sha=L.sha256_file(old),
            new_sha=L.sha256_file(new), work=str(self.tmp / "work"), out=str(self.tmp / ("out-" + tier)), step_timeout=120)
        runner = L.TierRunner(args)
        runner.sample_gap = 0.2
        runner.start_timeout = 30
        runner.test_isolation_override = {"isolated": True, "netns": "test", "interfaces": ["lo"]}
        original = L.pick_isolation
        L.pick_isolation = lambda: ("test-only (no namespace)", [], {"isolated": True, "netns": "test"})
        self.addCleanup(lambda: setattr(L, "pick_isolation", original))
        return runner

    def test_prod_tier_passes_and_writes_evidence(self):
        runner = self.runner("prod")
        result = runner.run()
        self.assertEqual(result["verdict"], "pass", json.dumps({"failing": result["failing_checks"], "steps": [
            (s["step"], s.get("status"), s.get("error")) for s in result["steps"]]}, indent=1))
        out = self.tmp / "out-prod"
        for name in ("tier-result.json", "reopen-evidence.json", "delta-pre-to-post-candidate.json",
                     "reopen/evidence-observer.json", "reopen/evidence-validator.json", "reopen/negative-controls.json",
                     "reopen/inspectors.json", "observations/old_run.json", "observations/new_run.json",
                     "observations/resume.json", "copies/pre-candidate-copy.tar.gz", "copies/post-candidate-copy.tar.gz"):
            self.assertTrue((out / name).exists(), name)
        # lab keys are neither copied nor hashed into the evidence
        self.assertEqual([p.name for p in (out / "fixture").iterdir() if p.name.endswith(".key.json")], [])
        # the observer was started without peers, key or stake
        argv_lines = (self.tmp / "work" / "prod" / "d4-reopen-observer" / "node-0" / "argv.txt").read_text().splitlines()
        self.assertIn("--stake 0", argv_lines[-1])
        self.assertNotIn("--peers", argv_lines[-1])
        self.assertNotIn("--validator-key-file", argv_lines[-1])
        # negative controls were detected
        neg = L.read_json(out / "reopen" / "negative-controls.json")
        self.assertTrue(all(row["detected"] for row in neg.values()), neg)
        # the control discriminates
        evidence = L.read_json(out / "reopen" / "evidence-observer.json")
        self.assertTrue(evidence["control_fails_freshness"])
        self.assertTrue(evidence["control_vs_old_state_ok"])
        self.assertEqual(evidence["signing_record_changed"], [])
        self.assertTrue(result["test_override_used"])

    def test_new_file_kinds_are_attributed_by_the_old_binary_lineage_control(self):
        runner = self.runner("prod")
        original = runner.s05_copy_post

        def with_new_kind():
            # the "candidate" creates a new file kind; the old lineage control will not -> attributable
            outcome = original()
            runner.delta["new_file_kinds"] = ["node-N/only-the-candidate.dat"]
            return outcome

        runner.s05_copy_post = with_new_kind
        result = runner.run()
        evidence = L.read_json(self.tmp / "out-prod" / "delta-pre-to-post-candidate.json")
        self.assertEqual(evidence["new_file_kinds_attributable_to_candidate"], ["node-N/only-the-candidate.dat"])
        self.assertTrue(any("old binary's own run" in f for f in result["flags"]), result["flags"])
        self.assertTrue((self.tmp / "out-prod" / "delta-pre-to-old-binary-lineage-control.json").exists())

    def test_native_tier_requires_the_feature(self):
        runner = self.runner("native")
        result = runner.run()
        # the fake harness reports the native feature for the native tier, so this passes
        self.assertEqual(result["verdict"], "pass", result["failing_checks"])

    def test_wrong_old_binary_stops_the_foundation(self):
        runner = self.runner("prod")
        runner.old_sha = "0" * 64
        result = runner.run()
        self.assertEqual(result["verdict"], "fail")
        self.assertTrue(any(s["status"] == "skipped" for s in result["steps"]))
        self.assertIn("binary digests and versions are as expected", result["failing_checks"])

    def test_a_candidate_that_rewrites_history_fails(self):
        # the fake harness writes the same chain; make the "start" observation of new_run disagree by patching it
        runner = self.runner("prod")
        original = runner.run_step

        def patched(step, data_dir, binary, sha, prev, label):
            rc, record, complete = original(step, data_dir, binary, sha, prev, label)
            if step == "new_run" and record:
                record["observations"][0]["nodes"][2]["blocks"][100]["hash"] = "ee" * 32
                L.write_json(self.tmp / "out-prod" / "observations" / (label + ".json"), record)
            return rc, record, complete

        runner.run_step = patched
        result = runner.run()
        self.assertEqual(result["verdict"], "fail")
        self.assertIn("the candidate serves exactly the history the old binary wrote", result["failing_checks"])

    def test_candidate_damage_visible_to_the_old_binary_fails_the_reopen(self):
        runner = self.runner("prod")
        original = runner.s05_copy_post

        def damaging():
            outcome = original()
            # corrupt the post-candidate copy the way a format change would look to the old binary
            victim = runner.d3 / "node-3" / "dag-wal" / "records.bin"
            victim.write_bytes(victim.read_bytes() + b"x")
            runner.m_post = L.build_manifest(runner.d3, "d3-post-candidate-copy")
            return outcome

        runner.s05_copy_post = damaging
        result = runner.run()
        self.assertEqual(result["verdict"], "fail")
        self.assertTrue(any("reopen" in name for name in result["failing_checks"]), result["failing_checks"])

    def test_probe_runs_a_stub_node(self):
        data = self.tmp / "d" / "node-0"
        (data / "dag-wal").mkdir(parents=True)
        rec = b"r" * 50
        (data / "dag-wal" / "records.bin").write_bytes(rec)
        (data / "chain.json").write_text(json.dumps({"height": 20, "records_sha256": hashlib.sha256(rec).hexdigest()}))
        spec = {"index": 0, "mode": "observer", "binary": str(self.old), "data_dir": str(data), "genesis": "g", "activation": "a",
                "key_file": "k", "native_runtime": False, "log": str(self.tmp / "p.log"), "start_timeout": 30, "samples": 2,
                "sample_gap": 0.1, "isolation_override": {"isolated": True}}
        L.write_json(self.tmp / "spec.json", spec)
        code = subprocess.call([sys.executable, str(Path(L.__file__)), "probe-reopen", "--spec", str(self.tmp / "spec.json"),
                                "--out", str(self.tmp / "res.json")])
        self.assertEqual(code, 0)
        result = L.read_json(self.tmp / "res.json")
        self.assertTrue(result["started"])
        self.assertEqual(result["exit_code"], 0)
        self.assertEqual(len(result["samples"]), 2)
        self.assertEqual(result["samples"][-1]["health"]["height"], 20)
        self.assertEqual([b for b in result["blocks"] if not b.get("missing")][-1]["h"], 20)


class AssembleTests(unittest.TestCase):
    def test_summary_requires_both_tiers_to_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            for tier, verdict in (("prod", "pass"), ("native", "fail")):
                d = tmp / ("tier-" + tier)
                (d / "reopen").mkdir(parents=True)
                L.write_json(d / "tier-result.json", {"tier": tier, "verdict": verdict, "failing_checks": [], "caveats": [],
                                                      "not_reproduced": [], "isolation": {"method": "m"}})
            args = argparse.Namespace(out=str(tmp / "out"), tier_dir=[str(tmp / "tier-prod"), str(tmp / "tier-native")],
                                      build_record=None, prod_binaries=None)
            self.assertEqual(L.cmd_assemble(args), 1)
            summary = L.read_json(tmp / "out" / "lab-summary.json")
            self.assertEqual(summary["verdict"], "fail")
            L.write_json(tmp / "tier-native" / "tier-result.json", {"tier": "native", "verdict": "pass", "failing_checks": [],
                                                                    "caveats": [], "not_reproduced": [], "isolation": {}})
            self.assertEqual(L.cmd_assemble(args), 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
