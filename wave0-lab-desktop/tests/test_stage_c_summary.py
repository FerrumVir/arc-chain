"""Tests for stage_c_summary.py (THROWAWAY LAB FILE). Offline; the end-to-end test feeds it real `desktop_updater_check` results."""
from __future__ import annotations

import contextlib
import copy
import hashlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Dict, Optional
from unittest import mock

import _paths  # noqa: F401
import desktop_updater_check as duc
import stage_c_summary as ssum
from test_desktop_updater_check import fake_source

LABELS = ("linux", "windows", "macos")
IDS = [f"I{n}" for n in range(1, 10)]
CHECKER_OK = {"I1": "PASS", "I2": "PASS", "I3": "PASS", "I4": "PASS", "I5": "PASS", "I6": "PASS", "I7": "PASS", "I8": "SKIP", "I9": "SKIP"}
SUMMARY = _paths.LAB / "stage_c_summary.py"


def make_result(label: str, states: Optional[Dict[str, str]] = None, verdict: Optional[str] = None, **overrides) -> dict:
    states = dict(states or CHECKER_OK)
    if verdict is None:
        verdict = "DESKTOP_UPDATER_NOT_ISOLATED" if "FAIL" in states.values() else "DESKTOP_UPDATER_ISOLATED"
    document = {
        "schema": "arc.legacy-bridge.wave0-lab.stage-c-result.v1",
        "label": label,
        "python": "3.11.9",
        "platform": f"fake-{label}",
        "tag": "v0.7.11",
        "asset": "arc-node-linux-x86_64",
        "verdict": verdict,
        "assertions": [{"id": name, "title": f"title {name}", "result": states[name], "detail": "d"} for name in IDS if name in states],
        "request_count": 7,
        "control_request_count": 6,
        "plugin_semantics": {"status": "confirmed", "reason": None, "crate": "tauri-plugin-updater 2.10.1"},
        "live_observation": {"supplied": False, "note": "not supplied"},
        "sources": {"desktop/src-tauri/tauri.conf.json": {"git_blob": "d" * 40, "sha256": "e" * 64}, "desktop/src/screens/Settings.tsx": {"git_blob": "0" * 40, "sha256": "1" * 64}},
    }
    document.update(overrides)
    return document


class RecordsCase(unittest.TestCase):
    def setUp(self):
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.root = Path(holder.name)
        self.records = self.root / "records"
        self.records.mkdir()
        self.raw: Dict[str, bytes] = {}

    def put(self, label: str, document: Optional[dict] = None, raw: Optional[bytes] = None) -> bytes:
        data = raw if raw is not None else (json.dumps(document if document is not None else make_result(label), indent=2, sort_keys=True) + "\n").encode("utf-8")
        (self.records / f"result-{label}.json").write_bytes(data)
        self.raw[label] = data
        return data

    def put_all(self, **per_label) -> None:
        for label in LABELS:
            self.put(label, per_label.get(label))

    def build(self):
        return ssum.build(self.records)[0]


class VerdictTests(RecordsCase):
    def test_pass_when_all_three_are_isolated_consistent_and_confirmed(self):
        self.put_all()
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_PASS")
        self.assertEqual(summary["schema"], "arc.legacy-bridge.wave0-lab.stage-c.v1")
        self.assertEqual(sorted(summary["labels"]), sorted(LABELS))
        self.assertEqual(summary["missing"], [])
        self.assertEqual(summary["notes"], [])
        self.assertTrue(summary["sources_identical_across_labels"])
        for label in LABELS:
            record = summary["labels"][label]
            self.assertEqual(record["verdict"], "DESKTOP_UPDATER_ISOLATED")
            self.assertEqual(record["request_count"], 7)
            self.assertEqual(record["sha256"], hashlib.sha256(self.raw[label]).hexdigest())
            self.assertEqual(record["native"], "SKIP")
            self.assertEqual(record["plugin_semantics"], "confirmed")
            self.assertEqual(record["failed_assertions"], [])
            self.assertNotIn("sources", record)
        self.assertEqual(summary["native_verified_labels"], [])

    def test_per_label_fields_follow_the_files(self):
        states = dict(CHECKER_OK, I8="PASS", I9="PASS")
        self.put("linux", make_result("linux", states, request_count=9))
        self.put("windows", make_result("windows", python="3.12.1", platform="Windows-2025"))
        self.put("macos", make_result("macos", live_observation={"supplied": True, "sha256": "a" * 64, "content": {}}))
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_PASS")
        self.assertEqual(summary["labels"]["linux"]["request_count"], 9)
        self.assertEqual(summary["labels"]["linux"]["native"], "PASS")
        self.assertEqual(summary["native_verified_labels"], ["linux"])
        self.assertEqual((summary["labels"]["windows"]["python"], summary["labels"]["windows"]["platform"]), ("3.12.1", "Windows-2025"))
        self.assertTrue(summary["labels"]["macos"]["live_observation_supplied"])
        self.assertFalse(summary["labels"]["linux"]["live_observation_supplied"])

    def test_incomplete_when_exactly_one_result_is_missing(self):
        for missing in LABELS:
            self.setUp()
            for label in LABELS:
                if label != missing:
                    self.put(label)
            summary = self.build()
            self.assertEqual(summary["verdict"], "STAGE_C_INCOMPLETE", missing)
            self.assertEqual(summary["missing"], [missing])
            self.assertEqual(sorted(summary["labels"]), sorted(set(LABELS) - {missing}))
            self.assertIn(f"missing results: {missing}", summary["notes"])

    def test_incomplete_when_nothing_is_there_yet(self):
        summary = self.build()
        self.assertEqual((summary["verdict"], summary["missing"], summary["labels"]), ("STAGE_C_INCOMPLETE", list(LABELS), {}))
        self.put("linux")
        self.assertEqual(self.build()["verdict"], "STAGE_C_INCOMPLETE")

    def test_fail_when_one_run_is_not_isolated(self):
        self.put_all(windows=make_result("windows", dict(CHECKER_OK, I3="FAIL")))
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_FAIL")
        self.assertEqual(summary["labels"]["windows"]["verdict"], "DESKTOP_UPDATER_NOT_ISOLATED")
        self.assertEqual(summary["labels"]["windows"]["failed_assertions"], ["I3"])
        self.assertTrue(any("windows: verdict DESKTOP_UPDATER_NOT_ISOLATED (failed: I3)" in note for note in summary["notes"]))
        self.assertEqual(summary["labels"]["linux"]["verdict"], "DESKTOP_UPDATER_ISOLATED")

    def test_a_failure_beats_a_missing_result(self):
        self.put("linux", make_result("linux", dict(CHECKER_OK, I1="FAIL")))
        self.put("macos")
        summary = self.build()
        self.assertEqual((summary["verdict"], summary["missing"]), ("STAGE_C_FAIL", ["windows"]))

    def test_a_native_failure_fails_the_stage(self):
        self.put_all(macos=make_result("macos", dict(CHECKER_OK, I8="FAIL", I9="PASS")))
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_FAIL")
        self.assertEqual(summary["labels"]["macos"]["native"], "FAIL")
        self.assertEqual(summary["labels"]["macos"]["failed_assertions"], ["I8"])

    def test_native_skip_does_not_fail_and_is_never_counted_as_native_proof(self):
        self.put_all(linux=make_result("linux", dict(CHECKER_OK, I8="PASS", I9="PASS")))
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_PASS")
        self.assertEqual(summary["native_verified_labels"], ["linux"])
        self.assertEqual(summary["labels"]["windows"]["native"], "SKIP")

    def test_native_status_mapping(self):
        status = ssum.native_status
        self.assertEqual(status({"I8": "PASS", "I9": "PASS"}), "PASS")
        self.assertEqual(status({"I8": "PASS", "I9": "SKIP"}), "SKIP")
        self.assertEqual(status({"I8": "SKIP", "I9": "SKIP"}), "SKIP")
        self.assertEqual(status({"I8": "PASS", "I9": "FAIL"}), "FAIL")
        self.assertEqual(status({"I8": "FAIL", "I9": "SKIP"}), "FAIL")
        self.assertEqual(status({}), "SKIP")

    def test_plugin_source_must_be_confirmed(self):
        for status in ("unverified", "contradicted", None):
            self.setUp()
            self.put_all(windows=make_result("windows", plugin_semantics={"status": status, "reason": "offline-requested"}))
            summary = self.build()
            self.assertEqual(summary["verdict"], "STAGE_C_FAIL", status)
            self.assertTrue(any("not confirmed" in note and "windows" in note for note in summary["notes"]), summary["notes"])

    def test_extra_files_and_other_labels_are_ignored(self):
        self.put_all()
        (self.records / "result-freebsd.json").write_bytes(b"not even json")
        (self.records / "requests.jsonl").write_bytes(b"")
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_PASS")
        self.assertEqual(sorted(summary["labels"]), sorted(LABELS))


class MalformedResultTests(RecordsCase):
    def assert_fails(self, label: str, expect_in_notes: str, **put_kwargs) -> dict:
        self.put(label, **put_kwargs)
        for other in LABELS:
            if other != label:
                self.put(other)
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_FAIL", summary["notes"])
        self.assertTrue(any(expect_in_notes in note for note in summary["notes"]), (expect_in_notes, summary["notes"]))
        return summary

    def test_not_json(self):
        summary = self.assert_fails("linux", "not JSON", raw=b"{ broken")
        self.assertNotIn("linux", summary["labels"])

    def test_not_an_object(self):
        self.assert_fails("linux", "not a JSON object", raw=b"[1, 2]")

    def test_not_utf8(self):
        self.assert_fails("linux", "not JSON", raw=b"\xff\xfe\x00")

    def test_wrong_schema(self):
        self.assert_fails("linux", "schema", document=make_result("linux", schema="arc.legacy-bridge.wave0-lab.stage-c-result.v0"))

    def test_label_inside_the_file_must_match_the_file_name(self):
        self.assert_fails("windows", "label 'linux'", document=make_result("linux"))

    def test_missing_assertions(self):
        document = make_result("linux")
        del document["assertions"]
        self.assert_fails("linux", "assertions missing", document=document)

    def test_each_of_i1_to_i9_must_be_present(self):
        for name in IDS:
            self.setUp()
            states = {k: v for k, v in CHECKER_OK.items() if k != name}
            self.assert_fails("linux", f"'{name}'", document=make_result("linux", states))

    def test_malformed_assertion_entries(self):
        document = make_result("linux")
        document["assertions"].append({"id": "I10", "result": "MAYBE"})
        document["assertions"].append("I11")
        self.assert_fails("linux", "malformed assertion", document=document)

    def test_skip_is_only_legal_for_i8_and_i9(self):
        for name in ("I1", "I4", "I7"):
            self.setUp()
            self.assert_fails("linux", "only I8 and I9 may be skipped", document=make_result("linux", dict(CHECKER_OK, **{name: "SKIP"}), verdict="DESKTOP_UPDATER_ISOLATED"))

    def test_verdict_must_follow_from_the_assertions(self):
        self.assert_fails("linux", "does not follow from its assertions", document=make_result("linux", dict(CHECKER_OK, I3="FAIL"), verdict="DESKTOP_UPDATER_ISOLATED"))
        self.setUp()
        self.assert_fails("linux", "does not follow from its assertions", document=make_result("linux", CHECKER_OK, verdict="DESKTOP_UPDATER_NOT_ISOLATED"))

    def test_unknown_verdict(self):
        self.assert_fails("linux", "verdict 'GREEN'", document=make_result("linux", verdict="GREEN"))

    def test_request_count_must_be_a_positive_integer(self):
        for bad in (0, -3, "7", None, True, 7.0):
            self.setUp()
            self.assert_fails("linux", "request_count", document=make_result("linux", request_count=bad))
        self.setUp()
        document = make_result("linux")
        del document["request_count"]
        self.assert_fails("linux", "request_count", document=document)

    def test_a_malformed_file_never_yields_a_pass_even_if_the_rest_is_perfect(self):
        self.put_all()
        self.put("macos", raw=b"")
        self.assertEqual(self.build()["verdict"], "STAGE_C_FAIL")

    def test_missing_plugin_semantics_block_is_not_a_confirmation(self):
        document = make_result("linux")
        del document["plugin_semantics"]
        self.assert_fails("linux", "not confirmed", document=document)


class SourceConsistencyTests(RecordsCase):
    def test_all_runs_must_read_the_same_source_blobs(self):
        different = make_result("windows")
        different["sources"]["desktop/src/screens/Settings.tsx"]["git_blob"] = "9" * 40
        self.put_all(windows=different)
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_FAIL")
        self.assertFalse(summary["sources_identical_across_labels"])
        self.assertTrue(any("different source bytes" in note for note in summary["notes"]))

    def test_a_missing_source_entry_counts_as_different(self):
        other = make_result("macos")
        del other["sources"]["desktop/src/screens/Settings.tsx"]
        self.put_all(macos=other)
        self.assertEqual(self.build()["verdict"], "STAGE_C_FAIL")

    def test_other_differences_between_runs_are_fine(self):
        self.put("linux", make_result("linux", python="3.9.6", platform="Linux-6.8", asset="arc-node-linux-x86_64", request_count=7))
        self.put("windows", make_result("windows", python="3.12.1", platform="Windows-2025", asset="arc-node-windows-x86_64.exe", request_count=7))
        self.put("macos", make_result("macos", python="3.11.5", platform="macOS-15-arm64", asset="arc-node-macos-arm64", request_count=9))
        self.assertEqual(self.build()["verdict"], "STAGE_C_PASS")

    def test_two_runs_alone_are_compared_too(self):
        different = make_result("windows")
        different["sources"]["desktop/src-tauri/tauri.conf.json"]["git_blob"] = "9" * 40
        self.put("linux")
        self.put("windows", different)
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_FAIL")
        self.assertEqual(summary["missing"], ["macos"])


class CliTests(RecordsCase):
    def run_main(self, *argv: str, env: Optional[Dict[str, str]] = None):
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.dict(os.environ, env or {}), contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            if env is None:
                os.environ.pop("GITHUB_STEP_SUMMARY", None)
            code = ssum.main(list(argv))
        return code, out.getvalue(), err.getvalue()

    def test_exit_zero_only_for_a_pass_and_the_file_is_written_every_time(self):
        out = self.root / "bundle" / "stage-c-summary.json"
        self.put_all()
        code, stdout, _ = self.run_main("build", "--records", str(self.records), "--out", str(out))
        self.assertEqual(code, 0)
        self.assertIn("Stage C verdict: STAGE_C_PASS", stdout)
        self.assertEqual(json.loads(out.read_text(encoding="utf-8"))["verdict"], "STAGE_C_PASS")
        self.assertTrue(out.read_bytes().endswith(b"\n"))
        self.assertNotIn(b"\r", out.read_bytes())

        self.setUp()
        self.put("linux")
        code, stdout, _ = self.run_main("build", "--records", str(self.records), "--out", str(out))
        self.assertEqual(code, 1)
        self.assertIn("STAGE_C_INCOMPLETE", stdout)
        self.assertEqual(json.loads(out.read_text(encoding="utf-8"))["verdict"], "STAGE_C_INCOMPLETE")

        self.setUp()
        self.put_all(macos=make_result("macos", dict(CHECKER_OK, I5="FAIL")))
        code, stdout, _ = self.run_main("build", "--records", str(self.records), "--out", str(out))
        self.assertEqual(code, 1)
        self.assertIn("STAGE_C_FAIL", stdout)

    def test_table_lines_per_label(self):
        self.put_all(linux=make_result("linux", dict(CHECKER_OK, I8="PASS", I9="PASS")))
        _, stdout, _ = self.run_main("build", "--records", str(self.records), "--out", str(self.root / "o.json"))
        self.assertIn("DESKTOP_UPDATER_ISOLATED", stdout)
        self.assertIn("native PASS", stdout)
        self.assertIn("native SKIP", stdout)
        self.assertIn("native tauri-plugin-updater verified on: ['linux']", stdout)

    def test_a_missing_records_directory_is_incomplete_not_a_crash(self):
        code, stdout, _ = self.run_main("build", "--records", str(self.root / "nope"), "--out", str(self.root / "o.json"))
        self.assertEqual(code, 1)
        self.assertIn("STAGE_C_INCOMPLETE", stdout)

    def test_step_summary_is_appended_when_github_provides_one(self):
        self.put_all()
        summary_file = self.root / "step-summary.md"
        summary_file.write_text("existing\n", encoding="utf-8")
        code, _, _ = self.run_main("build", "--records", str(self.records), "--out", str(self.root / "o.json"), env={"GITHUB_STEP_SUMMARY": str(summary_file)})
        self.assertEqual(code, 0)
        text = summary_file.read_text(encoding="utf-8")
        self.assertTrue(text.startswith("existing\n```\nStage C verdict: STAGE_C_PASS"))
        self.assertTrue(text.endswith("```\n"))

    def test_subprocess_invocation_and_usage_errors(self):
        self.put_all()
        out = self.root / "sub" / "summary.json"
        env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1", PYTHONIOENCODING="utf-8")
        env.pop("GITHUB_STEP_SUMMARY", None)
        done = subprocess.run([sys.executable, "-B", str(SUMMARY), "build", "--records", str(self.records), "--out", str(out)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True, encoding="utf-8", env=env, timeout=60)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(out.read_text(encoding="utf-8"))["verdict"], "STAGE_C_PASS")
        usage = subprocess.run([sys.executable, "-B", str(SUMMARY), "build"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True, encoding="utf-8", env=env, timeout=60)
        self.assertEqual(usage.returncode, 2)
        self.assertIn("--records", usage.stderr)
        nothing = subprocess.run([sys.executable, "-B", str(SUMMARY)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True, encoding="utf-8", env=env, timeout=60)
        self.assertEqual(nothing.returncode, 2)

    def test_summary_is_deterministic(self):
        self.put_all()
        first = json.dumps(self.build(), sort_keys=True)
        self.assertEqual(first, json.dumps(self.build(), sort_keys=True))

    def test_module_is_stdlib_only(self):
        import ast

        tree = ast.parse(SUMMARY.read_text(encoding="utf-8"))
        imported = set()
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                imported.update(alias.name.split(".")[0] for alias in node.names)
            elif isinstance(node, ast.ImportFrom) and node.module:
                imported.add(node.module.split(".")[0])
        self.assertEqual(imported - {"argparse", "hashlib", "json", "os", "re", "sys", "pathlib", "typing", "__future__"}, set())


class EndToEndContractTests(RecordsCase):
    """Real results written by desktop_updater_check.execute, read by stage_c_summary."""

    def run_label(self, label: str, **kwargs) -> dict:
        evidence = self.root / "evidence" / label
        parent = self.root / "tmp"
        parent.mkdir(exist_ok=True)
        kwargs.setdefault("source", fake_source(plugin_status="confirmed", plugin_reason=None))
        result = duc.execute(None, evidence, label, home_parent=parent, **kwargs)
        (self.records / f"result-{label}.json").write_bytes((evidence / f"result-{label}.json").read_bytes())
        return result

    def test_three_real_results_give_a_pass(self):
        for label in LABELS:
            self.assertEqual(self.run_label(label)["verdict"], "DESKTOP_UPDATER_ISOLATED")
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_PASS", summary["notes"])
        self.assertEqual({label: record["request_count"] for label, record in summary["labels"].items()}, {label: 7 for label in LABELS})
        self.assertEqual({record["native"] for record in summary["labels"].values()}, {"SKIP"})
        for label in LABELS:
            self.assertEqual(summary["labels"][label]["sha256"], hashlib.sha256((self.records / f"result-{label}.json").read_bytes()).hexdigest())

    def test_a_real_not_isolated_result_gives_a_fail(self):
        self.run_label("linux")
        self.run_label("windows", mutate={"latest_has_latest_json": True})
        self.run_label("macos")
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_FAIL")
        self.assertEqual(summary["labels"]["windows"]["failed_assertions"], ["I1", "I2", "I3", "I4"])

    def test_a_real_offline_plugin_run_is_not_a_stage_pass(self):
        for label in LABELS:
            self.run_label(label, source=fake_source(plugin_status="unverified", plugin_reason="offline-requested"))
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_FAIL")
        self.assertTrue(all(record["verdict"] == "DESKTOP_UPDATER_ISOLATED" for record in summary["labels"].values()))
        self.assertTrue(any("not confirmed" in note for note in summary["notes"]))

    def test_a_missing_real_result_is_incomplete(self):
        self.run_label("linux")
        self.run_label("macos")
        self.assertEqual(self.build()["verdict"], "STAGE_C_INCOMPLETE")

    def test_every_assertion_the_checker_writes_is_one_the_summary_requires(self):
        result = self.run_label("linux")
        self.assertEqual([entry["id"] for entry in result["assertions"]], list(ssum.REQUIRED_ASSERTIONS))
        self.assertEqual(ssum.RESULT_SCHEMA, duc.SCHEMA_RESULT)
        self.assertEqual(ssum.ISOLATED, duc.VERDICT_ISOLATED)
        self.assertEqual(ssum.NOT_ISOLATED, duc.VERDICT_NOT_ISOLATED)

    def test_a_real_result_with_a_native_stub_is_reported_as_native_verified(self):
        stub_dir = self.root / "stub"
        stub_dir.mkdir()
        stub = stub_dir / "stub.py"
        # the same contract stub the checker tests use
        from test_desktop_updater_check import STUB

        stub.write_text(STUB, encoding="utf-8")
        with mock.patch.dict(os.environ, {"STUB_MODE": "normal", "PYTHONDONTWRITEBYTECODE": "1"}):
            result = self.run_label("linux", native_check=[sys.executable, str(stub)])
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_ISOLATED", [a for a in result["assertions"] if a["result"] == "FAIL"])
        self.run_label("windows")
        self.run_label("macos")
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_PASS")
        self.assertEqual(summary["native_verified_labels"], ["linux"])
        self.assertEqual(summary["labels"]["linux"]["request_count"], 9)



# --------------------------------------------------------------------------------------------------------------
# desktop-os-result.v1 (written by the OS jobs): PASS only on explicit proof
# --------------------------------------------------------------------------------------------------------------

REPO = "FerrumVir/arc-chain"
MANIFEST = f"/{REPO}/releases/latest/download/latest.json"
REDIRECT_TARGET = f"/{REPO}/releases/download/v0.7.12/latest.json"
API_LATEST = f"/repos/{REPO}/releases/latest"
CRITERIA = ("only_manifest_url", "no_bundle_download", "no_install", "no_new_app_launch", "no_new_files")


def make_case(name: str = "clean", tier: str = "released_app", scenario: str = "latest-404", **overrides) -> dict:
    case = {
        "name": name,
        "tier": tier,
        "scenario": scenario,
        "trigger_outcome": {"error": "Could not fetch a valid release JSON from the remote"},
        "requests": {"total": 2, "by_host_path": [["github.com", MANIFEST, 1], ["github.com", REDIRECT_TARGET, 1]]},
        "criteria": {key: True for key in CRITERIA},
        "evidence_files": [],
        "verdict": "PASS",
    }
    case.update(overrides)
    return case


def make_os_result(label: str = "linux", **overrides) -> dict:
    document = {
        "schema": "arc.legacy-bridge.wave0-lab.desktop-os-result.v1",
        "os": label,
        "runner": {"image": "ubuntu-24.04", "arch": "x86_64", "os_version": "24.04"},
        "app": {"tag": "v0.7.11", "asset": "ARC.Node_0.7.11_amd64.AppImage", "asset_sha256": "a" * 64, "release_digest": "a" * 64, "digest_match": True, "version_reported": "0.7.11"},
        "plugin": {"version": "2.10.1", "provenance": ["strings of the released binary"]},
        "tiers": {
            "released_app": {"attempted": True, "trigger": "install button", "result": "PASS", "reason": ""},
            "native_check": {"attempted": True, "trigger": "native-updater-check", "result": "PASS", "reason": ""},
        },
        "cases": [
            make_case("clean", "released_app"), make_case("cached-bait", "released_app", "bait-0.8.11"),
            make_case("clean", "native_check"), make_case("cached-bait", "native_check", "bait-0.8.11"),
        ],
        "manifest404_error_text": "Update failed: Could not fetch a valid release JSON from the remote",
        "isolation": {"hosts_mapped": ["github.com", "api.github.com"], "ca_sha256": "b" * 64, "live_block": True},
        "verdict": "PASS",
    }
    document.update(overrides)
    return document


class OsCase(RecordsCase):
    def put_os(self, document: dict, where: str = "") -> Path:
        directory = self.records / where if where else self.records
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / "result.json"
        path.write_bytes((json.dumps(document, indent=2, sort_keys=True) + "\n").encode("utf-8"))
        return path

    def judge(self, document: dict) -> dict:
        path = self.put_os(document, "judged")
        return ssum.evaluate_os_result(document, path.read_bytes(), path, self.records)

    def mutated(self, mutate) -> dict:
        document = make_os_result()
        mutate(document)
        return self.judge(document)


class OsNormTests(unittest.TestCase):
    def test_only_explicit_values_decide(self):
        for value in (True, "PASS", "pass", " Pass ", "OK", "PROVED", "true", "ISOLATED", "DESKTOP_UPDATER_ISOLATED"):
            self.assertEqual(ssum.norm(value), "PASS", repr(value))
        for value in (False, "FAIL", "failed", "false", "NOT_ISOLATED", "DESKTOP_UPDATER_NOT_ISOLATED", "violated"):
            self.assertEqual(ssum.norm(value), "FAIL", repr(value))
        for value in (None, "", "UNPROVED", "unknown", "SKIPPED", "n/a", 1, 0, 1.0, [], {}, "passed-ish"):
            self.assertEqual(ssum.norm(value), "UNPROVED", repr(value))


class OsResultTests(OsCase):
    def test_a_fully_proven_result_passes(self):
        record = self.judge(make_os_result())
        self.assertEqual((record["verdict"], record["reasons"]), ("PASS", []))
        self.assertEqual(record["tiers"], {"released_app": "PASS", "native_check": "PASS"})
        self.assertEqual([case["verdict"] for case in record["cases"]], ["PASS"] * 4)
        self.assertEqual(record["plugin_version"], "2.10.1")
        self.assertIs(record["digest_match"], True)
        self.assertIs(record["live_block"], True)
        self.assertEqual(record["sha256"], hashlib.sha256((self.records / "judged" / "result.json").read_bytes()).hexdigest())

    def test_each_criterion_must_be_explicitly_true(self):
        for key in CRITERIA:
            for value, expected in ((False, "FAIL"), ("FAIL", "FAIL"), ("UNPROVED", "UNPROVED"), (None, "UNPROVED"), (1, "UNPROVED"), ("maybe", "UNPROVED")):
                def change(document, key=key, value=value):
                    document["cases"][0]["criteria"][key] = value

                record = self.mutated(change)
                self.assertEqual(record["verdict"], expected, (key, value))
                self.assertTrue(any(key in reason for reason in record["reasons"]), (key, value, record["reasons"]))

    def test_a_missing_criterion_or_criteria_block_is_unproved_never_pass(self):
        def drop_key(document):
            del document["cases"][1]["criteria"]["no_new_files"]

        record = self.mutated(drop_key)
        self.assertEqual(record["verdict"], "UNPROVED")
        self.assertTrue(any("criteria.no_new_files is missing" in reason for reason in record["reasons"]))

        def drop_block(document):
            del document["cases"][2]["criteria"]

        self.assertEqual(self.mutated(drop_block)["verdict"], "UNPROVED")

    def test_a_case_that_says_fail_or_says_nothing(self):
        self.assertEqual(self.mutated(lambda d: d["cases"][0].__setitem__("verdict", "FAIL"))["verdict"], "FAIL")
        self.assertEqual(self.mutated(lambda d: d["cases"][0].__delitem__("verdict"))["verdict"], "UNPROVED")
        self.assertEqual(self.mutated(lambda d: d["cases"][0].__setitem__("verdict", "UNPROVED"))["verdict"], "UNPROVED")

    def test_the_overall_verdict_must_be_explicit_too(self):
        self.assertEqual(self.mutated(lambda d: d.__setitem__("verdict", "FAIL"))["verdict"], "FAIL")
        self.assertEqual(self.mutated(lambda d: d.__delitem__("verdict"))["verdict"], "UNPROVED")
        self.assertEqual(self.mutated(lambda d: d.__setitem__("verdict", "INCONCLUSIVE"))["verdict"], "UNPROVED")

    def test_tiers(self):
        def none_attempted(document):
            for tier in document["tiers"].values():
                tier["attempted"] = False
            document["cases"] = []

        record = self.mutated(none_attempted)
        self.assertEqual(record["verdict"], "UNPROVED")
        self.assertEqual(record["tiers"], {"released_app": "NOT_ATTEMPTED", "native_check": "NOT_ATTEMPTED"})
        self.assertEqual(self.mutated(lambda d: d["tiers"]["released_app"].__setitem__("result", None))["verdict"], "UNPROVED")
        self.assertEqual(self.mutated(lambda d: d["tiers"]["native_check"].__setitem__("result", "FAIL"))["verdict"], "FAIL")
        self.assertEqual(self.mutated(lambda d: d["tiers"]["native_check"].__setitem__("attempted", "yes"))["tiers"]["native_check"], "NOT_ATTEMPTED")

    def test_one_tier_alone_is_enough_when_its_cases_pass(self):
        def native_only(document):
            document["tiers"]["released_app"] = {"attempted": False, "trigger": "", "result": "SKIPPED", "reason": "no display on this runner"}
            document["cases"] = [case for case in document["cases"] if case["tier"] == "native_check"]
            document["app"] = {}

        record = self.mutated(native_only)
        self.assertEqual((record["verdict"], record["reasons"]), ("PASS", []))
        self.assertEqual(record["tiers"], {"released_app": "NOT_ATTEMPTED", "native_check": "PASS"})

    def test_a_case_on_a_tier_that_did_not_run_proves_nothing(self):
        def stray(document):
            document["tiers"]["released_app"]["attempted"] = False
            document["app"] = {}

        record = self.mutated(stray)
        self.assertEqual(record["verdict"], "UNPROVED")
        self.assertTrue(any("tier released_app was not attempted" in reason for reason in record["reasons"]))

    def test_both_named_cases_must_pass(self):
        def only_clean(document):
            document["cases"] = [case for case in document["cases"] if case["name"] == "clean"]

        record = self.mutated(only_clean)
        self.assertEqual(record["verdict"], "UNPROVED")
        self.assertIn("no passing case named 'cached-bait'", " ".join(record["reasons"]))
        self.assertEqual(self.mutated(lambda d: d.__setitem__("cases", []))["verdict"], "UNPROVED")
        self.assertEqual(self.mutated(lambda d: d.__delitem__("cases"))["verdict"], "UNPROVED")
        self.assertEqual(self.mutated(lambda d: d["cases"].append("garbage"))["verdict"], "UNPROVED")
        renamed = self.mutated(lambda d: [c.__setitem__("name", "dirty") for c in d["cases"] if c["name"] == "cached-bait"])
        self.assertEqual(renamed["verdict"], "UNPROVED")
        self.assertTrue(any("unknown case name 'dirty'" in reason for reason in renamed["reasons"]))

    def test_a_failing_extra_case_fails_the_os_even_if_the_named_cases_pass(self):
        record = self.mutated(lambda d: d["cases"].append(make_case("clean", "native_check", criteria={**{k: True for k in CRITERIA}, "no_install": False})))
        self.assertEqual(record["verdict"], "FAIL")

    def test_request_logs_must_be_consistent(self):
        def total(document):
            document["cases"][0]["requests"]["total"] = 5

        record = self.mutated(total)
        self.assertEqual(record["verdict"], "UNPROVED")
        self.assertTrue(any("not the sum of the counts (2)" in reason for reason in record["reasons"]))
        self.assertEqual(self.mutated(lambda d: d["cases"][0]["requests"].__setitem__("by_host_path", []))["verdict"], "UNPROVED")
        self.assertEqual(self.mutated(lambda d: d["cases"][0].__delitem__("requests"))["verdict"], "UNPROVED")
        self.assertEqual(self.mutated(lambda d: d["cases"][0]["requests"].__setitem__("by_host_path", [["github.com", MANIFEST]]))["verdict"], "UNPROVED")
        self.assertEqual(self.mutated(lambda d: d["cases"][0]["requests"].__setitem__("by_host_path", [["github.com", MANIFEST, 0]]))["verdict"], "UNPROVED")

    def test_requests_the_criteria_deny_are_a_failure_even_when_declared_clean(self):
        denied = (
            f"/{REPO}/releases/download/v0.8.11/ARC.Node_aarch64.app.tar.gz", f"/{REPO}/releases/download/v0.7.12/ARC.Node_0.7.12_amd64.AppImage",
            f"/{REPO}/releases/download/v0.7.12/ARC.Node_0.7.12_x64-setup.exe.sig", f"/{REPO}/releases/download/v0.7.11/arc-node-linux-x86_64",
            "/x/a.deb", "/x/a.rpm", "/x/a.dmg", "/x/a.msi",
        )
        for path in denied:
            def add(document, path=path):
                document["cases"][0]["requests"]["by_host_path"].append(["github.com", path, 1])
                document["cases"][0]["requests"]["total"] = 3

            record = self.mutated(add)
            self.assertEqual(record["verdict"], "FAIL", path)
            self.assertTrue(any("the criteria deny" in reason for reason in record["reasons"]), path)

    def test_a_declared_no_bundle_download_is_contradicted_by_a_bundle_in_any_scenario(self):
        def bait(document):
            case = document["cases"][1]
            self.assertEqual(case["scenario"], "bait-0.8.11")
            case["requests"]["by_host_path"].append(["github.com", f"/{REPO}/releases/download/v0.8.11/ARC.Node_aarch64.app.tar.gz", 1])
            case["requests"]["total"] = 3

        self.assertEqual(self.mutated(bait)["verdict"], "FAIL")

        def v0711_asset_in_bait(document):
            case = document["cases"][1]
            case["requests"]["by_host_path"].append(["github.com", f"/{REPO}/releases/download/v0.7.11/arc-node-linux-x86_64", 1])
            case["requests"]["total"] = 3

        self.assertEqual(self.mutated(v0711_asset_in_bait)["verdict"], "PASS", "only the latest-404 scenario denies v0.7.11 assets")

    def test_other_paths_make_only_manifest_url_unproved_but_the_api_and_redirect_are_fine(self):
        def api(document):
            document["cases"][0]["requests"]["by_host_path"].append(["api.github.com", API_LATEST, 1])
            document["cases"][0]["requests"]["total"] = 3

        self.assertEqual(self.mutated(api)["verdict"], "PASS")

        def other(document):
            document["cases"][0]["requests"]["by_host_path"].append(["github.com", f"/{REPO}/releases/download/v0.7.12/SHA256SUMS", 1])
            document["cases"][0]["requests"]["total"] = 3

        record = self.mutated(other)
        self.assertEqual(record["verdict"], "UNPROVED")
        self.assertTrue(any("only_manifest_url is declared true but the log also holds" in reason for reason in record["reasons"]))

    def test_the_released_app_must_be_the_released_asset(self):
        self.assertEqual(self.mutated(lambda d: d["app"].__setitem__("digest_match", False))["verdict"], "FAIL")
        self.assertEqual(self.mutated(lambda d: d["app"].__delitem__("digest_match"))["verdict"], "UNPROVED")
        self.assertEqual(self.mutated(lambda d: d["app"].__setitem__("digest_match", "true"))["verdict"], "UNPROVED")
        self.assertEqual(self.mutated(lambda d: d["app"].__setitem__("release_digest", "c" * 64))["verdict"], "FAIL")
        self.assertEqual(self.mutated(lambda d: d["app"].__setitem__("release_digest", "sha256:" + "a" * 64))["verdict"], "PASS")
        self.assertEqual(self.mutated(lambda d: d["app"].__setitem__("tag", "v0.7.10"))["verdict"], "FAIL")
        self.assertEqual(self.mutated(lambda d: d["app"].__delitem__("tag"))["verdict"], "UNPROVED")
        self.assertEqual(self.mutated(lambda d: d.__delitem__("app"))["verdict"], "UNPROVED")

    def test_the_plugin_version_must_be_the_shipped_one(self):
        self.assertEqual(self.mutated(lambda d: d["plugin"].__setitem__("version", "tauri-plugin-updater 2.10.1"))["verdict"], "PASS")
        failed = self.mutated(lambda d: d["plugin"].__setitem__("version", "2.9.0"))
        self.assertEqual(failed["verdict"], "FAIL")
        self.assertTrue(any("not the shipped tauri-plugin-updater 2.10.1" in reason for reason in failed["reasons"]))
        self.assertEqual(self.mutated(lambda d: d["plugin"].__setitem__("version", "unknown"))["verdict"], "UNPROVED")
        self.assertEqual(self.mutated(lambda d: d["plugin"].__setitem__("version", ""))["verdict"], "UNPROVED")
        self.assertEqual(self.mutated(lambda d: d.__delitem__("plugin"))["verdict"], "UNPROVED")

    def test_the_404_text_the_app_showed(self):
        self.assertEqual(self.mutated(lambda d: d.__setitem__("manifest404_error_text", "Could not fetch a valid release JSON from the remote"))["verdict"], "PASS")
        for value in (None, "", "Update failed: certificate verify failed", 404):
            def change(document, value=value):
                document["manifest404_error_text"] = value

            self.assertEqual(self.mutated(change)["verdict"], "UNPROVED", value)
        self.assertEqual(self.mutated(lambda d: d.__delitem__("manifest404_error_text"))["verdict"], "UNPROVED")

        def no_latest_404_case(document):
            document["cases"] = [dict(case, scenario="bait-0.8.11") for case in document["cases"]]
            document["manifest404_error_text"] = None

        self.assertEqual(self.mutated(no_latest_404_case)["verdict"], "PASS", "the text is only demanded when a latest-404 case ran")

    def test_isolation_from_the_live_network_must_be_shown(self):
        for change in (
            lambda d: d["isolation"].__setitem__("live_block", False), lambda d: d["isolation"].__setitem__("live_block", "yes"),
            lambda d: d["isolation"].__delitem__("live_block"), lambda d: d.__delitem__("isolation"),
            lambda d: d["isolation"].__setitem__("hosts_mapped", []), lambda d: d["isolation"].__delitem__("hosts_mapped"),
        ):
            self.assertEqual(self.mutated(change)["verdict"], "UNPROVED")

    def test_listed_evidence_files_are_hashed_when_next_to_the_result(self):
        document = make_os_result()
        directory = self.records / "withfiles"
        directory.mkdir()
        (directory / "requests.jsonl").write_bytes(b"{}\n")
        (self.records / "outside.txt").write_bytes(b"secret")
        document["cases"][0]["evidence_files"] = ["requests.jsonl", "gone.png", "../outside.txt"]
        document["cases"][1]["evidence_files"] = ["requests.jsonl"]
        path = self.put_os(document, "withfiles")
        record = ssum.evaluate_os_result(document, path.read_bytes(), path, self.records)
        self.assertEqual(record["verdict"], "PASS", "missing evidence is reported, it does not change the verdict")
        self.assertEqual(record["evidence"]["listed"], 3)
        self.assertEqual(record["evidence"]["present"], 1)
        self.assertEqual(record["evidence"]["missing_from_records"], ["../outside.txt", "gone.png"])
        self.assertEqual(record["evidence"]["sha256"], {"requests.jsonl": hashlib.sha256(b"{}\n").hexdigest()})


class OsAggregationTests(OsCase):
    def put_everything(self, **os_overrides) -> None:
        self.put_all()
        for label in LABELS:
            self.put_os(make_os_result(label, **os_overrides.get(label, {})), f"desktop-os-{label}")

    def test_everything_proven_is_a_pass_with_one_verdict_per_os(self):
        self.put_everything()
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_PASS", summary["notes"])
        self.assertEqual(summary["os_verdicts"], {"linux": "PASS", "windows": "PASS", "macos": "PASS"})
        self.assertEqual(sorted(summary["desktop_os"]), sorted(LABELS))
        self.assertEqual(summary["desktop_os_absent"], [])
        self.assertEqual(summary["desktop_os"]["windows"]["file"], "desktop-os-windows/result.json")
        text = ssum.render(summary)
        for label in LABELS:
            line = next(row for row in text.splitlines() if row.split()[1:2] == [label])
            self.assertTrue(line.strip().startswith("PASS"), line)
            self.assertIn("desktop-os PASS", line)
            self.assertIn("harness DESKTOP_UPDATER_ISOLATED", line)

    def test_without_any_desktop_os_result_the_harness_alone_is_judged(self):
        self.put_all()
        summary = self.build()
        self.assertEqual((summary["verdict"], summary["desktop_os"], summary["desktop_os_absent"]), ("STAGE_C_PASS", {}, list(LABELS)))
        self.assertIn("desktop-os (none)", ssum.render(summary))

    def test_requiring_them_makes_an_absent_desktop_os_result_missing(self):
        self.put_all()
        self.put_os(make_os_result("linux"), "desktop-os-linux")
        summary = ssum.build(self.records, require_desktop_os=True)[0]
        self.assertEqual(summary["verdict"], "STAGE_C_INCOMPLETE")
        self.assertEqual(summary["os_verdicts"], {"linux": "PASS", "windows": "MISSING", "macos": "MISSING"})
        self.assertTrue(summary["require_desktop_os"])
        self.assertTrue(any("windows: no desktop-os result (--require-desktop-os)" in note for note in summary["notes"]))

    def test_with_the_flag_the_desktop_os_results_alone_decide_and_the_harness_result_is_optional(self):
        for label in LABELS:
            self.put_os(make_os_result(label), f"records/{label}")  # no harness result anywhere: the desktop workflow's layout
        summary = ssum.build(self.records, require_desktop_os=True)[0]
        self.assertEqual(summary["verdict"], "STAGE_C_PASS", summary["notes"])
        self.assertEqual(summary["os_verdicts"], {"linux": "PASS", "windows": "PASS", "macos": "PASS"})
        self.assertEqual(summary["labels"], {})
        self.assertEqual(summary["missing"], list(LABELS))  # informational
        self.assertNotIn("missing results", " ".join(summary["notes"]))
        # without the flag the same records are INCOMPLETE: the harness result is required there
        plain = ssum.build(self.records)[0]
        self.assertEqual((plain["verdict"], plain["os_verdicts"]), ("STAGE_C_INCOMPLETE", {"linux": "MISSING", "windows": "MISSING", "macos": "MISSING"}))

    def test_with_the_flag_a_present_but_bad_harness_result_still_fails_the_os(self):
        for label in LABELS:
            self.put_os(make_os_result(label), f"records/{label}")
        self.put("windows", make_result("windows", dict(CHECKER_OK, I5="FAIL")))
        summary = ssum.build(self.records, require_desktop_os=True)[0]
        self.assertEqual((summary["verdict"], summary["os_verdicts"]["windows"]), ("STAGE_C_FAIL", "FAIL"))

    def test_with_the_flag_one_unproved_or_missing_os_keeps_the_stage_from_passing(self):
        broken = make_os_result("macos")
        broken["tiers"]["native_check"] = {"attempted": True, "status": "infeasible", "result": "UNPROVED", "reason": "cargo build failed"}
        broken["cases"] = [case for case in broken["cases"] if case["tier"] == "released_app"]
        self.put_os(make_os_result("linux"), "records/linux")
        self.put_os(broken, "records/macos-arm64")
        summary = ssum.build(self.records, require_desktop_os=True)[0]
        self.assertEqual(summary["os_verdicts"], {"linux": "PASS", "windows": "MISSING", "macos": "UNPROVED"})
        self.assertEqual(summary["verdict"], "STAGE_C_INCOMPLETE")
        self.assertTrue(any("status 'infeasible': cargo build failed" in note for note in summary["notes"]), summary["notes"])

    def test_the_os_value_is_read_tolerantly(self):
        for spelling, expected in (("macos", "macos"), ("macOS-arm64", "macos"), ("macos-x86_64", "macos"), ("darwin", "macos"), ("Windows", "windows"), ("windows-latest", "windows"), ("ubuntu-24.04", "linux"), ("Linux", "linux")):
            self.assertEqual(ssum.canonical_os(spelling), expected, spelling)
        for bad in ("freebsd", "", None, 7, ["linux"]):
            self.assertIsNone(ssum.canonical_os(bad), repr(bad))
        self.put_os(make_os_result("macos-arm64"), "records/macos-arm64")
        summary = ssum.build(self.records, require_desktop_os=True)[0]
        self.assertEqual(summary["os_verdicts"]["macos"], "PASS")
        self.assertEqual(summary["desktop_os"]["macos"]["os_reported"], "macos-arm64")

    def test_positive_controls_must_be_proven_when_the_result_lists_them(self):
        def with_controls(*verdicts):
            def change(document):
                document["controls"] = [{"name": "bait-download", "verdict": verdict} for verdict in verdicts]

            return self.mutated(change)

        self.assertEqual(with_controls("PASS")["verdict"], "PASS")
        self.assertEqual(with_controls("PASS", "PASS")["verdict"], "PASS")
        failed = with_controls("PASS", "FAIL")
        self.assertEqual(failed["verdict"], "FAIL")
        self.assertTrue(any("control[1] FAILED" in reason for reason in failed["reasons"]))
        self.assertEqual(with_controls("UNPROVED")["verdict"], "UNPROVED")
        self.assertEqual(with_controls("PASS", None)["verdict"], "UNPROVED")
        self.assertEqual(self.mutated(lambda d: d.__setitem__("controls", ["junk"]))["verdict"], "UNPROVED")
        self.assertEqual(self.judge(make_os_result())["controls"], 0)

    def test_native_verified_labels_include_the_desktop_os_native_tier(self):
        self.put_all()
        self.put_os(make_os_result("windows"), "w")
        summary = self.build()
        self.assertEqual(summary["native_verified_labels"], ["windows"])
        self.assertFalse(summary["require_desktop_os"])

    def test_an_unproved_os_makes_the_stage_incomplete_and_says_why(self):
        broken = make_os_result("macos")
        del broken["cases"][0]["criteria"]["no_new_files"]
        self.put_all()
        self.put_os(make_os_result("linux"), "a")
        self.put_os(make_os_result("windows"), "b")
        self.put_os(broken, "c")
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_INCOMPLETE")
        self.assertEqual(summary["os_verdicts"], {"linux": "PASS", "windows": "PASS", "macos": "UNPROVED"})
        self.assertTrue(any(note.startswith("macos: desktop-os UNPROVED: ") and "no_new_files" in note for note in summary["notes"]))
        line = next(row for row in ssum.render(summary).splitlines() if row.split()[1:2] == ["macos"])
        self.assertTrue(line.strip().startswith("UNPROVED"))

    def test_an_explicit_failure_wins_over_unproved_and_missing(self):
        failing = make_os_result("windows")
        failing["cases"][0]["criteria"]["no_install"] = False
        unproved = make_os_result("macos", verdict=None)
        self.put("linux")
        self.put("windows")
        self.put_os(failing, "w")
        self.put_os(unproved, "m")
        summary = self.build()
        self.assertEqual(summary["os_verdicts"], {"linux": "PASS", "windows": "FAIL", "macos": "MISSING"})  # macos: its harness result is absent and its desktop-os result is unproved
        self.assertEqual(summary["verdict"], "STAGE_C_FAIL")

    def test_an_explicit_os_failure_is_a_failure_even_when_the_harness_result_is_missing(self):
        failing = make_os_result("macos")
        failing["cases"][0]["criteria"]["no_new_files"] = False
        self.put("linux")
        self.put("windows")
        self.put_os(failing, "m")
        summary = self.build()
        self.assertEqual(summary["os_verdicts"]["macos"], "FAIL")
        self.assertEqual((summary["verdict"], summary["missing"]), ("STAGE_C_FAIL", ["macos"]))

    def test_the_harness_failing_is_a_failure_even_if_the_os_result_passes(self):
        self.put_all(windows=make_result("windows", dict(CHECKER_OK, I3="FAIL")))
        for label in LABELS:
            self.put_os(make_os_result(label), label)
        summary = self.build()
        self.assertEqual(summary["os_verdicts"]["windows"], "FAIL")
        self.assertEqual(summary["verdict"], "STAGE_C_FAIL")

    def test_a_desktop_os_result_without_the_harness_result_is_missing_not_pass(self):
        self.put("linux")
        self.put("windows")
        for label in LABELS:
            self.put_os(make_os_result(label), label)
        summary = self.build()
        self.assertEqual(summary["os_verdicts"], {"linux": "PASS", "windows": "PASS", "macos": "MISSING"})
        self.assertEqual(summary["verdict"], "STAGE_C_INCOMPLETE")

    def test_the_harness_result_json_next_to_a_desktop_os_result_is_not_double_counted(self):
        self.put_all()
        for label in LABELS:
            directory = self.records / "evidence" / label
            directory.mkdir(parents=True)
            (directory / "result.json").write_bytes(self.raw[label])  # the harness's own result.json copy
            (directory / "home-before.json").write_bytes(b"{}")
            (directory / "requests.jsonl").write_bytes(b"")
            (directory / "check-source.json").write_bytes(b'{"schema": "arc.legacy-bridge.wave0-lab.stage-c-check-source.v1"}')
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_PASS", summary["notes"])
        self.assertEqual(summary["desktop_os"], {})

    def test_nested_artifact_layouts_are_found(self):
        for label in LABELS:
            directory = self.records / f"stage-c-{label}"
            directory.mkdir()
            data = (json.dumps(make_result(label), indent=2, sort_keys=True) + "\n").encode("utf-8")
            (directory / f"result-{label}.json").write_bytes(data)
            self.raw[label] = data
            self.put_os(make_os_result(label), f"desktop-os/{label}")
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_PASS", summary["notes"])
        self.assertEqual(summary["labels"]["linux"]["sha256"], hashlib.sha256(self.raw["linux"]).hexdigest())

    def test_the_same_document_delivered_twice_is_one_document(self):
        self.put_all()
        document = make_os_result("linux")
        self.put_os(document, "one")
        self.put_os(document, "two")
        (self.records / "dup").mkdir()
        (self.records / "dup" / "result-linux.json").write_bytes(self.raw["linux"])
        summary = self.build()
        self.assertEqual(summary["os_verdicts"]["linux"], "PASS")
        self.assertEqual(summary["verdict"], "STAGE_C_PASS", summary["notes"])

    def test_two_different_documents_for_one_os_are_a_failure(self):
        self.put_all()
        self.put_os(make_os_result("linux"), "one")
        self.put_os(make_os_result("linux", manifest404_error_text="Update failed: Could not fetch a valid release JSON from the remote (second)"), "two")
        summary = self.build()
        self.assertEqual((summary["verdict"], summary["os_verdicts"]["linux"]), ("STAGE_C_FAIL", "FAIL"))
        self.assertTrue(any("two different desktop-os results" in note for note in summary["notes"]))

    def test_two_different_harness_results_for_one_label_are_a_failure(self):
        self.put_all()
        (self.records / "again").mkdir()
        (self.records / "again" / "result-macos.json").write_bytes(json.dumps(make_result("macos", request_count=8)).encode("utf-8"))
        summary = self.build()
        self.assertEqual((summary["verdict"], summary["os_verdicts"]["macos"]), ("STAGE_C_FAIL", "FAIL"))
        self.assertTrue(any("exists 2 times with different contents" in note for note in summary["notes"]))

    def test_an_unknown_os_value_is_a_failure(self):
        self.put_all()
        self.put_os(make_os_result("freebsd"), "x")
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_FAIL")
        self.assertTrue(any("os 'freebsd'" in note for note in summary["notes"]))

    def test_a_broken_result_json_is_a_failure_but_other_broken_json_is_ignored(self):
        self.put_all()
        for label in LABELS:
            self.put_os(make_os_result(label), label)
        (self.records / "linux" / "home-before.json").write_bytes(b"{ broken")
        self.assertEqual(self.build()["verdict"], "STAGE_C_PASS")
        (self.records / "linux" / "result.json").write_bytes(b"{ broken")
        summary = self.build()
        self.assertEqual(summary["verdict"], "STAGE_C_FAIL")
        self.assertTrue(any("result.json in linux: not JSON" in note for note in summary["notes"]))

    def test_a_desktop_os_document_with_a_harness_style_name_is_still_read_as_desktop_os(self):
        self.put_all()
        (self.records / "result-linux.json").write_bytes(json.dumps(make_os_result("linux")).encode("utf-8"))
        summary = self.build()
        self.assertEqual(summary["os_verdicts"]["linux"], "MISSING")
        self.assertEqual(summary["desktop_os"]["linux"]["verdict"], "PASS")
        self.assertEqual(summary["verdict"], "STAGE_C_INCOMPLETE")

    def test_other_schemas_and_non_objects_are_ignored(self):
        self.put_all()
        (self.records / "a.json").write_bytes(json.dumps({"schema": "something.else", "os": "linux"}).encode("utf-8"))
        (self.records / "b.json").write_bytes(b"[1, 2, 3]")
        (self.records / "c.json").write_bytes(b'"text"')
        self.assertEqual(self.build()["verdict"], "STAGE_C_PASS")

    def test_cli_flag_and_json_output(self):
        self.put_all()
        for label in LABELS:
            self.put_os(make_os_result(label), label)
        out = self.root / "o.json"
        quiet = io.StringIO()
        with contextlib.redirect_stdout(quiet):
            code = ssum.main(["build", "--records", str(self.records), "--out", str(out)])
        self.assertEqual(code, 0)
        written = json.loads(out.read_text(encoding="utf-8"))
        self.assertEqual(written["os_verdicts"], {"linux": "PASS", "windows": "PASS", "macos": "PASS"})
        self.assertEqual(sorted(written["desktop_os"]["linux"]), sorted(["file", "sha256", "verdict", "reasons", "tiers", "cases", "plugin_version", "digest_match", "live_block", "runner", "evidence", "os_reported", "controls"]))
        # the same records, one OS result removed, with and without the flag
        (self.root / "trash").mkdir()
        os.replace(str(self.records / "windows" / "result.json"), str(self.root / "trash" / "windows-result.json"))
        with contextlib.redirect_stdout(quiet):
            self.assertEqual(ssum.main(["build", "--records", str(self.records), "--out", str(out)]), 0)
            self.assertEqual(ssum.main(["build", "--records", str(self.records), "--out", str(out), "--require-desktop-os"]), 1)
        self.assertEqual(json.loads(out.read_text(encoding="utf-8"))["os_verdicts"]["windows"], "MISSING")


class ContractWithTheCheckerTests(unittest.TestCase):
    def test_the_constants_equal_the_checkers(self):
        self.assertEqual(ssum.REPO, duc.REPO)
        self.assertEqual(ssum.TAG, duc.TAG)
        self.assertEqual(ssum.PLUGIN_VERSION, duc.PINNED["plugin_crate"]["version"])
        self.assertEqual(ssum.RELEASE_NOT_FOUND, duc.RELEASE_NOT_FOUND)
        self.assertEqual(ssum.MANIFEST_PATHS[0], duc.TAURI_ENDPOINT.replace("https://github.com", ""))
        self.assertEqual(ssum.MANIFEST_PATHS[1], f"/repos/{duc.REPO}/releases/latest")

    def test_the_forbidden_path_patterns_are_the_checkers(self):
        self.assertEqual([(pattern.pattern, label) for pattern, label in ssum.FORBIDDEN_PATH_PATTERNS], [(pattern.pattern, label) for pattern, label in duc.FORBIDDEN_PATH_PATTERNS])

    def test_the_checkers_own_request_log_satisfies_the_summarys_denials(self):
        # every request the harness makes in the main scenario is allowed by the summary's rules (no bundle, no v0.8, only the manifest)
        from test_desktop_updater_check import fake_source

        with tempfile.TemporaryDirectory() as raw:
            result = duc.execute(None, Path(raw) / "e", "linux", source=fake_source(), home_parent=Path(raw))
            rows = [json.loads(line) for line in (Path(raw) / "e" / "requests.jsonl").read_text(encoding="utf-8").splitlines()]
        manifest_rows = [row for row in rows if row["step"] in ("tauri_check", "install_click")]
        self.assertEqual(result["verdict"], "DESKTOP_UPDATER_ISOLATED")
        for row in manifest_rows:
            self.assertTrue(row["path"] in ssum.MANIFEST_PATHS or (row["path"].startswith(ssum.MANIFEST_PREFIX) and row["path"].endswith("/latest.json")), row["path"])
        for row in rows:
            for pattern, label in ssum.FORBIDDEN_PATH_PATTERNS:
                self.assertIsNone(pattern.search(row["path"]), (row["path"], label))


if __name__ == "__main__":
    unittest.main()
