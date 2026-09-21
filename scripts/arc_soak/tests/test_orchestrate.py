"""Fast tests for the orchestrator's pure helpers. No nodes."""

import os
import sys
import tempfile
import types
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(HERE)))

from arc_soak import orchestrate  # noqa: E402

REAL_RECORD = """=== ARC build provenance ===
recorded_utc:     20260921T145417Z
command:          cargo build -p arc-node --locked
workdir:          /Users/excaulibur/work/arc-chain-readiness-20260919
source_revision:  a283e9dfa7c2c1257886eb406165c305c9a15eda
dirty_files:      0
input_digest:     745a7111d2b94ee3891c75f4a8a343e03230606bd3cece800b473ce522078d10   (content of crates/**, scripts/**, Cargo.toml, Cargo.lock)
profile:          debug
features:         <default>
toolchain:        rustc 1.96.0-nightly (1e2183119 2026-03-15) / cargo 1.96.0-nightly (cbb9bb8bd 2026-03-13)
host:             Darwin 24.6.0 arm64
--- building ---
build_exit:       0
binary_source:    target/debug/arc-node
binary_sha256:    429f095c6447cb7885b4d0fc8be80a24c165c12738e4d5f282b47143eaa51a47
binary_bytes:     194738680
immutable_copy:   /tmp/arc-provenance/arc-node-429f095c6447cb78   (read-only; this is what the run executes)
"""


class ProvenanceParsing(unittest.TestCase):
    def test_the_real_build_record_yields_its_binary_digest(self):
        with tempfile.NamedTemporaryFile("w", delete=False, suffix=".txt") as fh:
            fh.write(REAL_RECORD)
        try:
            rec = orchestrate.read_provenance(fh.name)
        finally:
            os.unlink(fh.name)
        self.assertEqual(
            rec["binary_sha256"],
            "429f095c6447cb7885b4d0fc8be80a24c165c12738e4d5f282b47143eaa51a47")
        self.assertEqual(rec["dirty_files"], "0")
        self.assertEqual(rec["source_revision"], "a283e9dfa7c2c1257886eb406165c305c9a15eda")
        # Trailing annotations are not part of the value.
        self.assertEqual(rec["input_digest"],
                         "745a7111d2b94ee3891c75f4a8a343e03230606bd3cece800b473ce522078d10")


class JsonlFreshness(unittest.TestCase):
    def test_a_record_file_that_already_exists_is_refused(self):
        d = tempfile.mkdtemp()
        path = os.path.join(d, "samples.jsonl")
        open(path, "w").write('{"stale": true}\n')
        with self.assertRaises(FileExistsError):
            orchestrate.JsonlWriter(path)


class NativeRequesterFunding(unittest.TestCase):
    """Genesis must fund exactly the accounts the load driver will sign with."""

    def _fake_driver(self, lines):
        path = os.path.join(tempfile.mkdtemp(), "driver")
        with open(path, "w") as fh:
            fh.write("#!/bin/sh\n" + "".join(f"echo {line}\n" for line in lines))
        os.chmod(path, 0o755)
        return path

    def _soak(self, driver, count):
        cfg = types.SimpleNamespace(load_driver=driver, native_requesters=count,
                                    nodes=0, work=tempfile.mkdtemp())
        soak = orchestrate.Soak(cfg)
        soak.run = {"workload": {}}
        return soak

    def test_every_requester_the_driver_reports_is_returned_and_recorded(self):
        addresses = [c * 64 for c in "abc"]
        soak = self._soak(self._fake_driver(addresses), 3)
        self.assertEqual(soak.native_requesters(), addresses)
        self.assertEqual(soak.run["workload"]["requesters"], addresses)

    def test_a_driver_reporting_the_wrong_number_of_requesters_aborts(self):
        soak = self._soak(self._fake_driver(["a" * 64]), 2)
        with self.assertRaises(orchestrate.Abort):
            soak.native_requesters()

    def test_a_driver_reporting_something_that_is_not_an_address_aborts(self):
        soak = self._soak(self._fake_driver(["a" * 64, "not-an-address"]), 2)
        with self.assertRaises(orchestrate.Abort):
            soak.native_requesters()


class BinarySelfReport(unittest.TestCase):
    def test_a_node_reporting_another_build_aborts_the_run(self):
        digest = "ab" * 32
        self.assertEqual(orchestrate.binary_self_report({"binary_sha256": digest.upper()}, digest), "match")
        self.assertEqual(orchestrate.binary_self_report({"status": "ok"}, digest), "absent")
        self.assertEqual(orchestrate.binary_self_report(None, digest), "absent")
        with self.assertRaises(orchestrate.Abort):
            orchestrate.binary_self_report({"binary_sha256": "cd" * 32}, digest)


class Drills(unittest.TestCase):
    """The R6 drill step on a real (stopped) store, without starting nodes."""

    def setUp(self):
        import tempfile
        self.tmp = tempfile.TemporaryDirectory()
        self.work = self.tmp.name
        self.soak = object.__new__(orchestrate.Soak)
        self.soak.cfg = orchestrate.Config(orchestrate.build_parser().parse_args(
            ["--binary", "/bin/sh", "--provenance", "/dev/null", "--minutes", "20",
             "--work", os.path.join(self.work, "run"),
             "--upgrade-binary", "/bin/echo", "--upgrade-provenance", "/dev/null",
             "--fault-kinds", "restore,upgrade,rollback"]))
        self.soak.cfg.work = self.work
        self.soak.note = lambda text: None
        self.soak.upgrade_sha256 = "b" * 64
        self.soak.pre_upgrade = None
        self.soak.upgraded = None
        self.node = orchestrate.Node(self.soak.cfg, 0)
        self.node.binary_sha256 = "a" * 64
        os.makedirs(self.node.data_dir)
        with open(os.path.join(self.node.data_dir, "state.wal"), "wb") as fh:
            fh.write(b"wal-bytes-v1")

    def tearDown(self):
        self.tmp.cleanup()

    def wal(self):
        with open(os.path.join(self.node.data_dir, "state.wal"), "rb") as fh:
            return fh.read()

    def test_restore_upgrade_and_rollback_move_store_and_binary_as_recorded(self):
        f = {}
        self.soak.drill(0, "restore", self.node, f)
        self.assertEqual(self.wal(), b"wal-bytes-v1", "a restore brings back the same bytes")
        self.assertTrue(os.path.exists(f["backup_archive"]))

        f = {}
        self.soak.drill(1, "upgrade", self.node, f)
        self.assertEqual(self.node.binary, "/bin/echo")
        self.assertEqual(self.node.binary_sha256, "b" * 64)
        self.assertIs(self.soak.upgraded, self.node)
        # The upgraded binary writes to the store...
        with open(os.path.join(self.node.data_dir, "state.wal"), "ab") as fh:
            fh.write(b"+written-by-v2")

        f = {}
        self.soak.drill(2, "rollback", self.node, f)
        self.assertEqual(self.wal(), b"wal-bytes-v1", "rollback restores the pre-upgrade store")
        self.assertEqual((self.node.binary, self.node.binary_sha256), ("/bin/sh", "a" * 64))
        self.assertIsNone(self.soak.upgraded)
        self.assertTrue(os.path.isdir(self.node.data_dir + ".before-rollback-2"),
                        "the upgraded store is kept aside as evidence")


class FaultPlanning(unittest.TestCase):
    ARGS = ["--binary", "/bin/sh", "--provenance", "/dev/null", "--minutes", "20"]

    def config(self, *extra):
        return orchestrate.Config(orchestrate.build_parser().parse_args(self.ARGS + list(extra)))

    def test_a_short_run_still_gets_one_fault_after_its_baseline(self):
        # A huge interval does not mean "no faults": the first one always
        # comes once the baseline is measured, if there is time to recover.
        self.assertEqual(self.config("--fault-every-secs", "100000").planned_faults, 1)

    def test_no_faults_means_none(self):
        self.assertEqual(self.config("--no-faults").planned_faults, 0)

    def test_named_faults_outlast_retention_and_the_rest_keep_the_short_downtime(self):
        cfg = self.config("--long-down-secs", "900", "--long-faults", "1,3")
        self.assertEqual([cfg.down_secs_for(k) for k in range(5)], [20.0, 900.0, 20.0, 900.0, 20.0])
        self.assertEqual(self.config("--down-secs", "5").down_secs_for(0), 5.0)

    def test_drill_kinds_are_per_fault_and_validated(self):
        cfg = self.config("--fault-kinds", "restore,kill", "--minutes", "30")
        self.assertEqual([cfg.fault_kind(k) for k in range(4)], ["restore", "kill", "kill", "kill"])
        with self.assertRaises(SystemExit):
            self.config("--fault-kinds", "upgrade")  # no upgrade binary
        with self.assertRaises(SystemExit):
            self.config("--fault-kinds", "rollback,upgrade",
                        "--upgrade-binary", "/bin/sh", "--upgrade-provenance", "/dev/null")
        with self.assertRaises(SystemExit):
            self.config("--fault-kinds", "kill,explode")
        ok = self.config("--fault-kinds", "upgrade,rollback",
                         "--upgrade-binary", "/bin/sh", "--upgrade-provenance", "/dev/null")
        self.assertEqual(ok.fault_kind(1), "rollback")

    def test_long_faults_without_a_long_downtime_are_refused(self):
        with self.assertRaises(SystemExit):
            self.config("--long-faults", "1")
        with self.assertRaises(SystemExit):
            self.config("--long-down-secs", "900", "--long-faults", "one")


if __name__ == "__main__":
    unittest.main(verbosity=2)
