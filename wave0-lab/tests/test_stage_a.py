"""Tests for stage_a_record.py and stage_a_summary.py (THROWAWAY LAB FILE)."""
from __future__ import annotations

import copy
import hashlib
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

import _paths  # noqa: F401
import stage_a_record as rec
import stage_a_summary as summ

ASSETS = list(json.loads((_paths.LAB / "config.json").read_text())["handoff"]["launchers"])
RELEASE_LIB = Path("/Users/excaulibur/work/outputs/arc-proof-sprint-20261006/v0712-release/v0712_release_lib.py")


class Env:
    """A temp dir with five launcher files whose digests the config pins."""

    def __init__(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.cfg = json.loads((_paths.LAB / "config.json").read_text())
        self.files = {}
        for asset in ASSETS:
            path = self.dir / asset
            path.write_bytes((asset + "\n").encode() * 10)
            self.files[asset] = path
            self.cfg["handoff"]["launchers"][asset] = rec.sha256_file(path)
        self.cfg_path = self.dir / "config.json"
        self.cfg_path.write_text(json.dumps(self.cfg))
        self.records = self.dir / "records"
        self.records.mkdir()

    def consumed(self, asset, job, exit_code=0, runner="ubuntu-24.04", before=None):
        record = rec.build_consumed(self.cfg, asset, self.files[asset], before or rec.sha256_file(self.files[asset]), "harness.sh", job, runner, exit_code, None)
        return record

    def smoke(self, exit_code=0, runner="ubuntu-24.04-arm"):
        asset = "arc-node-linux-aarch64"
        return rec.build_smoke(self.cfg, self.files[asset], rec.sha256_file(self.files[asset]), "stage-a-linux-aarch64-smoke", runner, exit_code, "ELF aarch64", "arc-node 0.7.12")

    def full_set(self):
        out = [self.consumed(asset, job, runner="x") for asset, job in summ.CONSUMED_REQUIRED]
        out.append(self.smoke())
        return out

    def lab(self):
        return {"commit": "c" * 40, "run_id": 99, "run_attempt": 1, "branch": "wave0-v0712-lab"}


class RecordTests(unittest.TestCase):
    def test_consumed_pass(self):
        env = Env()
        record = env.consumed("arc-node-linux-x86_64", "stage-a-headless-linux")
        self.assertEqual(record["result"], "PASS")
        self.assertTrue(record["consumed"])
        self.assertEqual(record["sha256_before_run"], record["sha256_after_run"])

    def test_consumed_fails_on_exit_code(self):
        env = Env()
        self.assertEqual(env.consumed("arc-node-linux-x86_64", "j", exit_code=1)["result"], "FAIL")

    def test_consumed_fails_when_the_bytes_changed_during_the_run(self):
        env = Env()
        self.assertEqual(env.consumed("arc-node-linux-x86_64", "j", before="0" * 64)["result"], "FAIL")

    def test_consumed_fails_on_other_bytes_than_pinned(self):
        env = Env()
        env.cfg["handoff"]["launchers"]["arc-node-linux-x86_64"] = "1" * 64
        self.assertEqual(env.consumed("arc-node-linux-x86_64", "j")["result"], "FAIL")

    def test_smoke_pass_and_fail(self):
        env = Env()
        self.assertEqual(env.smoke()["smoke_result"], "PASS")
        self.assertEqual(env.smoke(exit_code=1)["smoke_result"], "FAIL")
        self.assertEqual(env.smoke(runner="ubuntu-24.04")["smoke_result"], "FAIL")

    def test_cli_writes_files_and_refuses_aarch64_as_consumed(self):
        env = Env()
        out = env.dir / "r.json"
        code = rec.main(["consumed", "--config", str(env.cfg_path), "--asset", "arc-node-linux-x86_64", "--launcher", str(env.files["arc-node-linux-x86_64"]),
                         "--before", rec.sha256_file(env.files["arc-node-linux-x86_64"]), "--harness", "h", "--job", "j", "--runner", "r", "--exit-code", "0", "--out", str(out)])
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out.read_text())["result"], "PASS")
        code = rec.main(["consumed", "--config", str(env.cfg_path), "--asset", "arc-node-linux-aarch64", "--launcher", str(env.files["arc-node-linux-aarch64"]),
                         "--before", "0" * 64, "--harness", "h", "--job", "j", "--runner", "r", "--exit-code", "0", "--out", str(env.dir / "x.json")])
        self.assertEqual(code, 2)

    def test_the_statement_is_the_approved_text(self):
        self.assertEqual(
            rec.AARCH64_EXCEPTION_STATEMENT,
            "NOT CONSUMED: no v0.7 linux-aarch64 baseline exists (v0.7.7 shipped none), so no stranded v0.7 install can ever fetch this asset; EXECUTED as a smoke on ubuntu-24.04-arm.",
        )

    @unittest.skipUnless(RELEASE_LIB.exists(), "the release scripts are not on this machine")
    def test_the_statement_equals_the_release_library_constant(self):
        spec = importlib.util.spec_from_file_location("v0712_release_lib", RELEASE_LIB)
        module = importlib.util.module_from_spec(spec)
        try:
            spec.loader.exec_module(module)
        except Exception:  # noqa: BLE001
            self.skipTest("the release library does not import here")
        constant = getattr(module, "AARCH64_EXCEPTION_STATEMENT", None)
        if constant is None:
            self.skipTest("the release library has no AARCH64_EXCEPTION_STATEMENT yet")
        self.assertEqual(constant, rec.AARCH64_EXCEPTION_STATEMENT)


L6_RESULT = Path("/Users/excaulibur/work/outputs/arc-proof-sprint-20261006/bridge-v0712/l6-result.json")


class ReleaseLibraryAgreementTests(unittest.TestCase):
    """The summary this lab writes must be accepted by the validator the L6/L8 scripts use (when both are on this machine)."""

    @unittest.skipUnless(RELEASE_LIB.exists() and L6_RESULT.exists(), "the release scripts or the real L6 result are not on this machine")
    def test_summary_is_accepted_by_check_stage_a_summary(self):
        spec = importlib.util.spec_from_file_location("v0712_release_lib", RELEASE_LIB)
        lib = importlib.util.module_from_spec(spec)
        import sys
        sys.modules["v0712_release_lib"] = lib
        spec.loader.exec_module(lib)
        cfg = json.loads((_paths.LAB / "config.json").read_text())
        l6 = json.loads(L6_RESULT.read_text())
        pinned = cfg["handoff"]["launchers"]
        records = []
        for asset, job in summ.CONSUMED_REQUIRED:
            digest = pinned[asset]
            records.append({"schema": rec.SCHEMA, "kind": "consumed", "asset": asset, "sha256": digest, "consumed": True, "result": "PASS", "harness": "h", "job": job,
                            "runner": "r", "sha256_before_run": digest, "sha256_after_run": digest, "exit_code": 0})
        digest = pinned["arc-node-linux-aarch64"]
        records.append({"schema": rec.SCHEMA, "kind": "exception", "asset": "arc-node-linux-aarch64", "sha256": digest, "exception_kind": rec.EXCEPTION_KIND,
                        "statement": rec.AARCH64_EXCEPTION_STATEMENT, "executed": True, "smoke_result": "PASS", "runner": "ubuntu-24.04-arm",
                        "job": summ.SMOKE_JOB, "sha256_before_run": digest, "sha256_after_run": digest})
        summary, notes = summ.build(cfg, records, {"commit": "c" * 40, "run_id": 123456789, "run_attempt": 1, "branch": "wave0-v0712-lab"})
        self.assertEqual(notes, [])
        problems, per_asset, exceptions = lib.check_stage_a_summary(summary, l6)
        self.assertEqual(problems, [])
        self.assertEqual(sorted(per_asset), sorted(pinned))
        self.assertEqual(len(exceptions), 1)


class SummaryTests(unittest.TestCase):
    def test_full_set_passes_and_has_exactly_the_contract_keys(self):
        env = Env()
        summary, notes = summ.build(env.cfg, env.full_set(), env.lab())
        self.assertEqual(summary["verdict"], "STAGE_A_PASS", notes)
        self.assertEqual(notes, [])
        self.assertEqual(sorted(summary), sorted(["schema", "repository", "workflow_path", "lab_branch", "lab_commit", "lab_run_id", "lab_run_attempt", "base_commit", "handoff", "results", "exceptions", "verdict"]))
        self.assertEqual(len(summary["results"]), 5)
        for entry in summary["results"]:
            self.assertEqual(sorted(entry), sorted(summ.RESULT_KEYS))
            self.assertTrue(entry["consumed"])
            self.assertEqual(entry["result"], "PASS")
        self.assertEqual(len(summary["exceptions"]), 1)
        exception = summary["exceptions"][0]
        self.assertEqual(sorted(exception), sorted(summ.EXCEPTION_KEYS))
        self.assertEqual(exception["asset"], "arc-node-linux-aarch64")
        self.assertEqual(exception["kind"], "NOT_CONSUMED_NO_V07_BASELINE")
        self.assertEqual(exception["statement"], rec.AARCH64_EXCEPTION_STATEMENT)
        self.assertEqual(summary["handoff"]["launchers"], env.cfg["handoff"]["launchers"])
        self.assertEqual(summary["workflow_path"], ".github/workflows/wave0-lab.yml")

    def test_missing_record_is_incomplete(self):
        env = Env()
        records = [r for r in env.full_set() if r.get("job") != "stage-a-desktop-macos-intel"]
        summary, notes = summ.build(env.cfg, records, env.lab())
        self.assertEqual(summary["verdict"], "STAGE_A_INCOMPLETE")
        self.assertTrue(any("macos-intel" in note for note in notes))

    def test_missing_exception_is_incomplete(self):
        env = Env()
        records = [r for r in env.full_set() if r["kind"] != "exception"]
        summary, _ = summ.build(env.cfg, records, env.lab())
        self.assertEqual(summary["verdict"], "STAGE_A_INCOMPLETE")

    def test_failed_harness_fails(self):
        env = Env()
        records = env.full_set()
        records[0] = env.consumed(records[0]["asset"], records[0]["job"], exit_code=1)
        summary, notes = summ.build(env.cfg, records, env.lab())
        self.assertEqual(summary["verdict"], "STAGE_A_FAIL")
        self.assertTrue(notes)

    def test_other_bytes_fail(self):
        env = Env()
        records = env.full_set()
        records[1]["sha256_after_run"] = "0" * 64
        summary, _ = summ.build(env.cfg, records, env.lab())
        self.assertEqual(summary["verdict"], "STAGE_A_FAIL")

    def test_exception_for_another_asset_fails(self):
        env = Env()
        records = env.full_set()
        bad = copy.deepcopy(records[-1])
        bad["asset"] = "arc-node-windows-x86_64.exe"
        bad["job"] = "other"
        summary, notes = summ.build(env.cfg, records[:-1] + [bad], env.lab())
        self.assertEqual(summary["verdict"], "STAGE_A_FAIL")
        self.assertTrue(any("only valid for" in note for note in notes))

    def test_altered_statement_fails(self):
        env = Env()
        records = env.full_set()
        records[-1]["statement"] = records[-1]["statement"].replace("NOT CONSUMED", "Not consumed")
        summary, _ = summ.build(env.cfg, records, env.lab())
        self.assertEqual(summary["verdict"], "STAGE_A_FAIL")

    def test_aarch64_as_consumed_fails(self):
        env = Env()
        records = env.full_set()
        fake = env.consumed("arc-node-linux-x86_64", "stage-a-x")
        fake["asset"] = "arc-node-linux-aarch64"
        fake["sha256"] = fake["sha256_before_run"] = fake["sha256_after_run"] = env.cfg["handoff"]["launchers"]["arc-node-linux-aarch64"]
        summary, _ = summ.build(env.cfg, records + [fake], env.lab())
        self.assertEqual(summary["verdict"], "STAGE_A_FAIL")

    def test_duplicate_record_fails(self):
        env = Env()
        records = env.full_set()
        summary, _ = summ.build(env.cfg, records + [copy.deepcopy(records[0])], env.lab())
        self.assertEqual(summary["verdict"], "STAGE_A_FAIL")

    def test_unknown_asset_fails(self):
        env = Env()
        records = env.full_set()
        odd = copy.deepcopy(records[0])
        odd["asset"] = "arc-node-freebsd"
        odd["job"] = "x"
        summary, _ = summ.build(env.cfg, records + [odd], env.lab())
        self.assertEqual(summary["verdict"], "STAGE_A_FAIL")

    def test_cli_end_to_end(self):
        env = Env()
        for index, record in enumerate(env.full_set()):
            (env.records / f"record-{index}.json").write_text(json.dumps(record))
        (env.records / "junk.json").write_text("{not json")
        out = env.dir / "bundle" / "stage-a-summary.json"
        code = summ.main(["build", "--config", str(env.cfg_path), "--records", str(env.records), "--out", str(out),
                          "--lab-commit", "c" * 40, "--lab-run-id", "5", "--lab-run-attempt", "1", "--lab-branch", "wave0-v0712-lab"])
        self.assertEqual(code, 1, "an unreadable record must not yield a PASS")
        self.assertEqual(json.loads(out.read_text())["verdict"], "STAGE_A_FAIL")
        (env.records / "junk.json").write_text(json.dumps({"hello": 1}))
        code = summ.main(["build", "--config", str(env.cfg_path), "--records", str(env.records), "--out", str(out),
                          "--lab-commit", "c" * 40, "--lab-run-id", "5", "--lab-run-attempt", "1", "--lab-branch", "wave0-v0712-lab"])
        self.assertEqual(code, 1, "a file that is not a Stage A record must not yield a PASS")


if __name__ == "__main__":
    unittest.main()
