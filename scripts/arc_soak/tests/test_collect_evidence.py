"""Tests for the post-soak evidence collector. Synthetic run directories, no nodes."""

import gzip
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(HERE)))

from arc_soak import collect_evidence as ce  # noqa: E402
from arc_soak import growth  # noqa: E402

BINARY = "ab" * 32


def write(path, data):
    mode = "wb" if isinstance(data, bytes) else "w"
    with open(path, mode) as fh:
        fh.write(data)


def write_jsonl(path, recs, extra=()):
    with open(path, "w") as fh:
        for r in recs:
            fh.write(json.dumps(r) + "\n")
        for line in extra:
            fh.write(line + "\n")


def make_run(root):
    d = os.path.join(root, "run")
    os.makedirs(d)
    write(os.path.join(d, "run.json"), json.dumps({
        "completed": True, "binary_sha256": BINARY,
        "provenance": {"file": "/builds/arc-node.provenance", "recorded_binary_sha256": BINARY},
        "nodes": [{"index": 0}, {"index": 1}]}))
    write(os.path.join(d, "verdict.json"), json.dumps(
        {"status": "PASS", "fail": [], "incomplete": [], "facts": {"heights": 12345}}))
    write(os.path.join(d, "report.txt"), "VERDICT: PASS\n")
    write(os.path.join(d, "events.log"), "t=0 start\nt=1 fault 1\n")
    write_jsonl(os.path.join(d, "faults.jsonl"),
                [{"k": 1, "outcome": "recovered"}, {"k": 2, "outcome": "recovered"},
                 {"k": 3, "outcome": "timeout"}])
    write_jsonl(os.path.join(d, "agreement.jsonl"), [{"height": 100 * i, "agree": True} for i in range(5)])
    write_jsonl(os.path.join(d, "workload.jsonl"),
                [{"request_id": f"r{i}", "final_status": "finalized"} for i in range(3)]
                + [{"request_id": "r3", "final_status": "refunded"},
                   {"request_id": "r4", "final_status": "pending"}],
                extra=("not json", ""))
    # 60 samples 30 s apart, so a 700 s warm-up still leaves a steady window.
    diag, samples = [], []
    for i in range(60):
        t, h = 1000.0 + 30 * i, 100 * i
        diag.append({"t": t, "node": 0, "incarnation": 0,
                     "diag": {"height": h, "gauge_flat": 7, "gauge_leaky": 2 * h}})
        samples.append({"t": t, "node": 0, "incarnation": 0, "height": h,
                        "rss_kb": 400_000, "disk_bytes": 1_000_000 + 1000 * h, "log_bytes": 5_000})
    write_jsonl(os.path.join(d, "diag.jsonl"), diag)
    write_jsonl(os.path.join(d, "samples.jsonl"), samples)
    write(os.path.join(d, "activation.json"), json.dumps({"active": True}))
    write(os.path.join(d, "genesis.toml"), "chain_id = 'soak'\n")
    write(os.path.join(d, "load-driver.log"), "driver started\n")
    write(os.path.join(d, "ident-0.txt"), "identity 0\n")
    write(os.path.join(d, "ident-1.txt"), "identity 1\n")
    os.makedirs(os.path.join(d, "ident-0"))
    write(os.path.join(d, "ident-0", "key"), b"\x00secret")
    os.makedirs(os.path.join(d, "node-0", "state"))
    write(os.path.join(d, "node-0", "state", "wal"), b"\x01" * 4096)
    write(os.path.join(d, "node-0.0.log"), "node 0 first incarnation\n" * 50)
    write(os.path.join(d, "node-0.1.log"), "node 0 after restart\n" * 50)
    write(os.path.join(d, "node-1.0.log"), "node 1\n" * 50)
    write(os.path.join(d, "backup-fault-1-node-0.tar.gz"), b"\x1f\x8b not copied")
    return d


def snapshot(root):
    """Every path under root with its bytes' digest (files) and mtime (files and directories)."""
    out = {}
    for base, dirs, files in os.walk(root):
        rel = os.path.relpath(base, root)
        out[("dir", rel)] = os.stat(base).st_mtime_ns
        for f in files:
            p = os.path.join(base, f)
            with open(p, "rb") as fh:
                out[("file", os.path.join(rel, f))] = (hashlib.sha256(fh.read()).hexdigest(),
                                                       os.stat(p).st_mtime_ns)
    return out


def run_main(*argv):
    out, err = io.StringIO(), io.StringIO()
    with redirect_stdout(out), redirect_stderr(err):
        rc = ce.main(list(argv))
    return rc, out.getvalue(), err.getvalue()


def read_manifest(out_dir):
    with open(os.path.join(out_dir, "MANIFEST.sha256")) as fh:
        return [line.rstrip("\n").split("  ", 1) for line in fh if line.strip()]


class Collect(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.mkdtemp()
        self.run_dir = make_run(self.root)
        self.out = os.path.join(self.root, "bundle")

    def tearDown(self):
        shutil.rmtree(self.root, ignore_errors=True)

    def test_copies_the_named_records_and_nothing_else(self):
        before = snapshot(self.run_dir)
        rc, _, err = run_main("--run", self.run_dir, "--out", self.out)
        self.assertEqual(rc, 0, err)
        expected = set(ce.RECORDS) | {"ident-0.txt", "ident-1.txt", "growth.txt",
                                      "summary.json", "MANIFEST.sha256"}
        self.assertEqual(set(os.listdir(self.out)), expected)  # no node dirs, logs, backups
        for name in list(ce.RECORDS) + ["ident-0.txt", "ident-1.txt"]:
            with open(os.path.join(self.run_dir, name), "rb") as a, open(os.path.join(self.out, name), "rb") as b:
                self.assertEqual(a.read(), b.read(), name)
        self.assertEqual(snapshot(self.run_dir), before)

    def test_manifest_covers_every_other_file_and_verifies(self):
        self.assertEqual(run_main("--run", self.run_dir, "--out", self.out)[0], 0)
        entries = read_manifest(self.out)
        paths = [p for _, p in entries]
        self.assertEqual(paths, sorted(paths))
        self.assertEqual(set(paths), set(os.listdir(self.out)) - {"MANIFEST.sha256"})
        for digest, rel in entries:
            self.assertEqual(ce.sha256_file(os.path.join(self.out, rel)), digest, rel)

    def test_summary_repeats_what_the_run_recorded(self):
        self.assertEqual(run_main("--run", self.run_dir, "--out", self.out)[0], 0)
        with open(os.path.join(self.out, "summary.json")) as fh:
            s = json.load(fh)
        self.assertEqual(s["verdict"], {"status": "PASS"})
        self.assertIs(s["completed"], True)
        self.assertEqual(s["binary_sha256"], BINARY)
        self.assertEqual(s["provenance"], {"file": "/builds/arc-node.provenance",
                                           "recorded_binary_sha256": BINARY})
        self.assertEqual(s["counts"], {
            "agreement_records": 5, "fault_records": 3, "faults_recovered": 2,
            "fault_outcomes": {"recovered": 2, "timeout": 1},
            "workload_records": 5,
            "workload_by_final_status": {"finalized": 3, "pending": 1, "refunded": 1}})
        self.assertEqual(s["unparsable_lines"], {"workload.jsonl": 1})
        self.assertFalse(s["node_logs_included"])

    def test_verdict_fields_are_never_reinterpreted(self):
        write(os.path.join(self.run_dir, "verdict.json"), json.dumps(
            {"status": "INCOMPLETE", "fail": [], "incomplete": ["orchestrator stopped early"]}))
        self.assertEqual(run_main("--run", self.run_dir, "--out", self.out)[0], 0)
        with open(os.path.join(self.out, "summary.json")) as fh:
            s = json.load(fh)
        self.assertEqual(s["verdict"], {"status": "INCOMPLETE"})  # no exit code synthesized

    def test_an_unreadable_verdict_is_reported_not_judged(self):
        write(os.path.join(self.run_dir, "verdict.json"), "{truncated")
        self.assertEqual(run_main("--run", self.run_dir, "--out", self.out)[0], 0)
        with open(os.path.join(self.out, "summary.json")) as fh:
            self.assertEqual(json.load(fh)["verdict"], {"unreadable": True})

    def test_growth_txt_is_the_growth_report(self):
        self.assertEqual(run_main("--run", self.run_dir, "--out", self.out, "--skip-s", "700")[0], 0)
        with open(os.path.join(self.out, "growth.txt")) as fh:
            text = fh.read()
        self.assertEqual(text, growth.report(self.run_dir, 700.0) + "\n")
        self.assertIn("gauge_leaky", text)
        self.assertIn("skip 700 s", text)

    def test_a_live_orchestrator_is_refused(self):
        before = snapshot(self.run_dir)
        rc, _, err = run_main("--run", self.run_dir, "--out", self.out, "--pid", str(os.getpid()))
        self.assertEqual(rc, 2)
        self.assertIn("still running", err)
        self.assertFalse(os.path.exists(self.out))
        self.assertEqual(snapshot(self.run_dir), before)

    def test_an_exited_orchestrator_is_accepted(self):
        proc = subprocess.Popen([sys.executable, "-c", "pass"])
        proc.wait()
        self.assertEqual(run_main("--run", self.run_dir, "--out", self.out, "--pid", str(proc.pid))[0], 0)

    def test_a_non_positive_pid_is_refused(self):
        self.assertEqual(run_main("--run", self.run_dir, "--out", self.out, "--pid", "0")[0], 2)

    def test_a_missing_verdict_or_report_is_refused(self):
        for name in ("verdict.json", "report.txt"):
            root = tempfile.mkdtemp()
            try:
                run_dir = make_run(root)
                os.remove(os.path.join(run_dir, name))
                before = snapshot(run_dir)
                out = os.path.join(root, "bundle")
                rc, _, err = run_main("--run", run_dir, "--out", out)
                self.assertEqual(rc, 2, name)
                self.assertIn(name, err)
                self.assertFalse(os.path.exists(out))
                self.assertEqual(snapshot(run_dir), before)
            finally:
                shutil.rmtree(root, ignore_errors=True)

    def test_a_non_empty_out_dir_is_refused_and_left_alone(self):
        os.makedirs(self.out)
        write(os.path.join(self.out, "keep.txt"), "earlier bundle\n")
        before = snapshot(self.out)
        rc, _, err = run_main("--run", self.run_dir, "--out", self.out)
        self.assertEqual(rc, 2)
        self.assertIn("not an empty directory", err)
        self.assertEqual(snapshot(self.out), before)

    def test_an_existing_empty_out_dir_is_used(self):
        os.makedirs(self.out)
        self.assertEqual(run_main("--run", self.run_dir, "--out", self.out)[0], 0)
        self.assertTrue(os.path.exists(os.path.join(self.out, "MANIFEST.sha256")))

    def test_an_out_dir_inside_the_run_is_refused(self):
        before = snapshot(self.run_dir)
        rc, _, err = run_main("--run", self.run_dir, "--out", os.path.join(self.run_dir, "bundle"))
        self.assertEqual(rc, 2)
        self.assertIn("inside the run directory", err)
        self.assertEqual(snapshot(self.run_dir), before)

    def test_node_logs_only_with_the_flag_and_gzip_compressed(self):
        before = snapshot(self.run_dir)
        rc, _, err = run_main("--run", self.run_dir, "--out", self.out, "--include-node-logs")
        self.assertEqual(rc, 0, err)
        logs = {"node-0.0.log", "node-0.1.log", "node-1.0.log"}
        self.assertEqual({n for n in os.listdir(self.out) if n.endswith(".log.gz")},
                         {n + ".gz" for n in logs})
        for name in logs:
            with open(os.path.join(self.run_dir, name), "rb") as a, \
                    gzip.open(os.path.join(self.out, name + ".gz"), "rb") as b:
                self.assertEqual(a.read(), b.read(), name)
        self.assertTrue(logs.isdisjoint(os.listdir(self.out)))  # never uncompressed
        self.assertFalse(any(n.startswith("node-") and not n.endswith(".log.gz")
                             for n in os.listdir(self.out)))
        self.assertTrue({n + ".gz" for n in logs} <= {p for _, p in read_manifest(self.out)})
        self.assertEqual(snapshot(self.run_dir), before)


if __name__ == "__main__":
    unittest.main()
