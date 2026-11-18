"""Offline audit-policy tests using saved npm audit --json reports."""

from contextlib import redirect_stderr, redirect_stdout
from datetime import datetime, timezone
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/release/npm-audit-gate.py"
FIXTURES = Path(__file__).parent / "fixtures/npm-audit"
SPEC = importlib.util.spec_from_file_location("npm_audit_gate", SCRIPT)
GATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GATE)
ACCEPTED = "GHSA-fc5q-39xc-vjpf"
OTHER = "GHSA-2345-6789-cfgh"


class AuditGateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.package = self.root / "package with spaces"
        self.package.mkdir()
        for name in ("package.json", "package-lock.json"):
            (self.package / name).write_text("{}")
        self.policy = json.loads((ROOT / "npm-audit-exceptions.json").read_text())
        self.policy_path = self.root / "npm-audit-exceptions.json"
        self.write_policy()

    def write_policy(self):
        self.policy_path.write_text(json.dumps(self.policy))

    def run_gate(self, fixture="excepted", *, report=None, raw=None,
                 code=None, flags=(), error=None):
        if raw is None:
            raw = json.dumps(report) if report is not None else (FIXTURES / (fixture + ".json")).read_text()
        if code is None:
            parsed = json.loads(raw)
            level = flags[flags.index("--level") + 1] if "--level" in flags else "low"
            code = int(any(GATE.LEVELS[f["severity"]] >= GATE.LEVELS[level]
                           for f in parsed["vulnerabilities"].values()))
        stdout, stderr = io.StringIO(), io.StringIO()
        with patch.object(GATE, "ROOT", self.root), \
                patch.object(GATE, "datetime") as clock, \
                patch.object(GATE.shutil, "which", return_value="/mock/npm"), \
                patch.object(GATE.subprocess, "run") as npm, \
                redirect_stdout(stdout), redirect_stderr(stderr):
            clock.now.return_value = datetime(2026, 11, 18, tzinfo=timezone.utc)
            npm.return_value = subprocess.CompletedProcess([], code, raw, "")
            npm.side_effect = error
            result = GATE.main(["--package", str(self.package), "--level", "low", *flags])
        output = stdout.getvalue() + stderr.getvalue()
        self.assertEqual(output.count("npm-audit-gate package="), 1, output)
        return result, output, npm

    def report(self, name="excepted"):
        return json.loads((FIXTURES / (name + ".json")).read_text())

    def test_clean_warns_unused_exception(self):
        code, output, _ = self.run_gate("clean")
        self.assertEqual(code, 0, output)
        self.assertIn("findings=0 excepted=0 blocking=0 exceptions_used=none", output)
        self.assertIn("WARNING", output)

    def test_only_exact_advisory_passes_and_lists_exception(self):
        code, output, _ = self.run_gate()
        self.assertEqual(code, 0, output)
        self.assertIn("findings=1 excepted=1 blocking=0 exceptions_used=" + ACCEPTED, output)

    def test_mixed_advisories_block_other_and_print_path(self):
        code, output, _ = self.run_gate("mixed")
        self.assertEqual(code, 1, output)
        self.assertIn("BLOCK advisory=" + OTHER + " package=other path=node_modules/other", output)
        self.assertNotIn("BLOCK advisory=" + ACCEPTED, output)
        self.assertIn("findings=2 excepted=1 blocking=1", output)

    def test_dependent_with_only_excepted_cause_passes(self):
        code, output, _ = self.run_gate("dependent")
        self.assertEqual(code, 0, output)
        self.assertIn("findings=2 excepted=2 blocking=0", output)

    def test_dependent_with_two_causes_blocks(self):
        code, output, _ = self.run_gate("dependent-mixed")
        self.assertEqual(code, 1, output)
        self.assertIn("findings=3 excepted=1 blocking=2", output)
        self.assertIn("finding=vite finding_paths=node_modules/vite", output)

    def test_other_advisory_on_same_package_still_blocks(self):
        report = self.report()
        other = dict(report["vulnerabilities"]["picomatch"]["via"][0])
        other["url"] = "https://github.com/advisories/" + OTHER
        report["vulnerabilities"]["picomatch"]["via"].append(other)
        code, output, _ = self.run_gate(report=report)
        self.assertEqual(code, 1, output)
        self.assertIn("BLOCK advisory=" + OTHER + " package=picomatch", output)

    def test_real_reports_resolve_every_dependent(self):
        for name, count in [("desktop", 5), ("compatibility-sdk", 35)]:
            with self.subTest(name=name):
                code, output, _ = self.run_gate(name)
                self.assertEqual(code, 0, output)
                self.assertIn(f"findings={count} excepted={count} blocking=0", output)

    def test_expired_exception_fails_even_for_clean_report(self):
        self.policy["exceptions"][0].update(added="2026-11-01", expires="2026-11-17")
        self.write_policy()
        code, output, npm = self.run_gate("clean")
        self.assertEqual(code, 2, output)
        self.assertIn("expired exception", output)
        npm.assert_not_called()

    def test_expiry_day_is_inclusive(self):
        self.policy["exceptions"][0]["expires"] = "2026-11-18"
        self.write_policy()
        code, output, _ = self.run_gate()
        self.assertEqual(code, 0, output)

    def test_malformed_report_fails(self):
        code, output, _ = self.run_gate("malformed", code=1)
        self.assertEqual(code, 2, output)

    def test_malformed_exception_file_fails(self):
        self.policy_path.write_text('{"exceptions":')
        code, output, npm = self.run_gate()
        self.assertEqual(code, 2, output)
        npm.assert_not_called()

    def test_wrong_exception_package_does_not_cover(self):
        self.policy["exceptions"][0]["package"] = "vite"
        self.write_policy()
        code, output, _ = self.run_gate()
        self.assertEqual(code, 1, output)
        self.assertIn("blocking=1 exceptions_used=none", output)
        self.assertIn("WARNING", output)

    def test_flags_cwd_and_timeout_forwarded(self):
        code, output, npm = self.run_gate(flags=("--omit-dev", "--lock-only", "--level", "high"))
        self.assertEqual(code, 0, output)
        args, kwargs = npm.call_args
        self.assertEqual(args[0], ["/mock/npm", "audit", "--json", "--audit-level=high",
                                   "--omit=dev", "--package-lock-only"])
        self.assertEqual(kwargs["cwd"], self.package)
        self.assertEqual(kwargs["timeout"], 120)
        self.assertNotIn("shell", kwargs)
        self.assertIn("level=high omit_dev=1", output)

    def test_thresholds(self):
        for level, expected in [("low", 1), ("moderate", 1), ("high", 1), ("critical", 0)]:
            with self.subTest(level=level):
                code, output, _ = self.run_gate("mixed", flags=("--level", level))
                self.assertEqual(code, expected, output)

    def test_execution_errors_are_not_a_pass(self):
        for error in (OSError("not found"), subprocess.TimeoutExpired("npm", 120)):
            with self.subTest(error=error):
                code, output, _ = self.run_gate(error=error)
                self.assertEqual(code, 2, output)
        for fixture, npm_code in [("clean", 1), ("excepted", 0), ("clean", 2)]:
            code, output, _ = self.run_gate(fixture, code=npm_code)
            self.assertEqual(code, 2, output)

    def test_invalid_policy_schema_dates_and_duplicates(self):
        original = json.loads(json.dumps(self.policy))
        for field, value in [("owner", ""), ("expires", "2026-02-30"),
                             ("added", "2026-11-19"), ("expires", "20261217"),
                             ("advisory", ACCEPTED + "-extra"), ("reason", None)]:
            with self.subTest(field=field, value=value):
                self.policy = json.loads(json.dumps(original))
                self.policy["exceptions"][0][field] = value
                self.write_policy()
                code, output, _ = self.run_gate()
                self.assertEqual(code, 2, output)
        self.policy = original
        self.policy["exceptions"].append(dict(self.policy["exceptions"][0]))
        self.write_policy()
        code, output, _ = self.run_gate()
        self.assertEqual(code, 2, output)

    def test_unknown_schema_and_contradictory_metadata_fail(self):
        reports = [{}, {"error": {"code": "ENOAUDIT"}}]
        for mutate in (
            lambda r: r.update(auditReportVersion=3),
            lambda r: r["metadata"]["vulnerabilities"].update(total=0),
            lambda r: r["metadata"]["vulnerabilities"].update(high=0),
            lambda r: r["metadata"].pop("dependencies"),
            lambda r: r["vulnerabilities"]["picomatch"].update(nodes=[]),
            lambda r: r["vulnerabilities"]["picomatch"].update(via=[]),
            lambda r: r["vulnerabilities"]["picomatch"].update(via=["missing"]),
            lambda r: r["vulnerabilities"]["picomatch"].update(severity="unknown"),
            lambda r: r["vulnerabilities"]["picomatch"]["via"][0].update(url="https://example.com/" + ACCEPTED),
            lambda r: r["vulnerabilities"]["picomatch"]["via"][0].update(dependency="other"),
        ):
            report = self.report()
            mutate(report)
            reports.append(report)
        for report in reports:
            with self.subTest(report=report):
                code, output, _ = self.run_gate(report=report, code=1)
                self.assertEqual(code, 2, output)

    def test_cycles_require_reachable_root_for_every_member(self):
        report = self.report("dependent")
        report["vulnerabilities"]["picomatch"]["via"].append("vite")
        code, output, _ = self.run_gate(report=report)
        self.assertEqual(code, 0, output)
        report["vulnerabilities"]["picomatch"]["via"] = ["vite"]
        code, output, _ = self.run_gate(report=report)
        self.assertEqual(code, 2, output)
        self.assertIn("no root advisory reachable", output)

    def test_duplicate_keys_and_non_json_constants_fail(self):
        for raw in ('{"vulnerabilities":{},"vulnerabilities":{}}', '{"x":NaN}'):
            code, output, _ = self.run_gate(raw=raw, code=1)
            self.assertEqual(code, 2, output)

    def test_missing_policy_and_lock_fail(self):
        self.policy_path.rename(self.root / "saved.json")
        code, output, _ = self.run_gate()
        self.assertEqual(code, 2, output)
        self.write_policy()
        (self.package / "package-lock.json").rename(self.package / "saved-lock.json")
        code, output, _ = self.run_gate()
        self.assertEqual(code, 2, output)

    def test_cli_usage_failure_has_one_summary(self):
        result = subprocess.run([sys.executable, "-I", "-B", str(SCRIPT), "--unknown"],
                                text=True, capture_output=True, check=False)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertEqual(result.stdout.count("npm-audit-gate package="), 1)
        self.assertEqual(GATE.ROOT, ROOT)

    @unittest.skipIf(sys.platform == "win32", "POSIX fake npm executable")
    def test_cli_uses_script_policy_root_and_package_cwd(self):
        script = self.root / "scripts/release/npm-audit-gate.py"
        script.parent.mkdir(parents=True)
        shutil.copyfile(SCRIPT, script)
        # Policy dates follow today for this real subprocess; unit tests freeze
        # the clock to keep expiry cases deterministic after the policy expires.
        today = datetime.now(timezone.utc).date().isoformat()
        self.policy["exceptions"][0].update(added=today, expires=today)
        self.write_policy()
        binary_dir = self.root / "bin"
        binary_dir.mkdir()
        npm = binary_dir / "npm"
        npm.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "Path(os.environ['AUDIT_CAPTURE']).write_text(json.dumps([sys.argv[1:], os.getcwd()]))\n"
            "print(Path(os.environ['AUDIT_FIXTURE']).read_text())\n"
            "sys.exit(1)\n")
        npm.chmod(0o755)
        unrelated = self.root / "unrelated"
        unrelated.mkdir()
        (unrelated / "npm-audit-exceptions.json").write_text("invalid")
        capture = self.root / "capture.json"
        result = subprocess.run(
            [sys.executable, "-I", "-B", str(script), "--package", "../package with spaces",
             "--level", "high", "--omit-dev", "--lock-only"], cwd=unrelated,
            env={**os.environ, "PATH": str(binary_dir) + os.pathsep + os.environ["PATH"],
                 "AUDIT_CAPTURE": str(capture), "AUDIT_FIXTURE": str(FIXTURES / "excepted.json")},
            text=True, capture_output=True, check=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("exceptions_used=" + ACCEPTED, result.stdout)
        argv, cwd = json.loads(capture.read_text())
        self.assertEqual(argv, ["audit", "--json", "--audit-level=high", "--omit=dev", "--package-lock-only"])
        self.assertEqual(Path(cwd).resolve(), self.package.resolve())


if __name__ == "__main__":
    unittest.main(verbosity=2)
