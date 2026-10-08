"""Structural guarantees of the two lab workflows (THROWAWAY LAB FILE)."""
from __future__ import annotations

import re
import unittest

import _paths  # noqa: F401
import stage_a_record as rec
import stage_a_summary as summ

try:
    import yaml
except ImportError:  # pragma: no cover
    yaml = None

WORKFLOWS = _paths.ROOT / ".github" / "workflows"
A = WORKFLOWS / "wave0-lab.yml"
B = WORKFLOWS / "wave0-lab-soak.yml"
ALLOWED_ACTIONS = {
    "actions/checkout": "3d3c42e5aac5ba805825da76410c181273ba90b1",
    "actions/upload-artifact": "043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
    "actions/download-artifact": "3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c",
}


def load(path):
    return yaml.safe_load(path.read_text(encoding="utf-8"))


def triggers(doc):
    return doc.get("on") if "on" in doc else doc.get(True)


@unittest.skipIf(yaml is None, "PyYAML is not installed")
class WorkflowTests(unittest.TestCase):
    def test_both_files_exist(self):
        self.assertTrue(A.is_file() and B.is_file())

    def test_triggers_are_the_lab_branch_push_only(self):
        for path in (A, B):
            with self.subTest(path.name):
                on = triggers(load(path))
                self.assertEqual(set(on), {"push"}, "no pull_request, no workflow_dispatch, no schedule")
                self.assertEqual(on["push"], {"branches": ["wave0-v0712-lab"]})

    def test_permissions_are_read_only(self):
        for path in (A, B):
            with self.subTest(path.name):
                self.assertEqual(load(path)["permissions"], {"contents": "read", "actions": "read"})

    def test_no_secrets_no_environment_no_oidc_no_release_commands(self):
        for path in (A, B):
            text = path.read_text(encoding="utf-8")
            with self.subTest(path.name):
                self.assertNotIn("secrets.", text)
                self.assertNotIn("environment:", text)
                self.assertNotIn("id-token", text)
                self.assertNotRegex(text, r"gh (release|workflow|api -X|pr )")
                self.assertNotRegex(text, r"git push|git tag|--force")

    def test_every_action_is_sha_pinned_and_already_used_by_the_repository(self):
        for path in (A, B):
            for match in re.finditer(r"uses:\s*([\w./-]+)@([0-9a-f]{40})\s+#", path.read_text(encoding="utf-8")):
                with self.subTest(path.name, action=match.group(1)):
                    self.assertEqual(ALLOWED_ACTIONS.get(match.group(1)), match.group(2))
            self.assertEqual(len(re.findall(r"uses:", path.read_text(encoding="utf-8"))), len(re.findall(r"uses:\s*[\w./-]+@[0-9a-f]{40}\s+#", path.read_text(encoding="utf-8"))), "every uses: must be pinned")

    def test_pinned_shas_match_the_repository_own_workflow(self):
        reference = (WORKFLOWS / "legacy-bridge.yml").read_text(encoding="utf-8")
        for action, sha in ALLOWED_ACTIONS.items():
            self.assertIn(f"{action}@{sha}", reference)

    def test_timeouts_fit_the_hosted_limit(self):
        for path in (A, B):
            for name, job in load(path)["jobs"].items():
                with self.subTest(path.name, job=name):
                    self.assertIn("timeout-minutes", job)
                    self.assertLessEqual(job["timeout-minutes"], 350)
        stage_b = load(B)["jobs"]["stage-b"]
        self.assertEqual(stage_b["timeout-minutes"], 350)
        run_step = next(step for step in stage_b["steps"] if step.get("name", "").startswith("Run Wave 0"))
        self.assertLess(run_step["timeout-minutes"], 350)
        import json
        cfg = json.loads((_paths.LAB / "config.json").read_text())
        for name, profile in cfg["stage_b"]["profiles"].items():
            with self.subTest(profile=name):
                self.assertLess(profile["deadline_min"], run_step["timeout-minutes"], "the orchestrator must stop before its step is killed")

    def test_every_stage_b_step_before_the_soak_has_its_own_timeout(self):
        steps = load(B)["jobs"]["stage-b"]["steps"]
        for step in steps:
            name = step.get("name", "")
            if name.startswith("KVM gate") or name.startswith("Run Wave 0"):
                with self.subTest(name):
                    self.assertIn("timeout-minutes", step)
        gate = next(step for step in steps if step.get("name", "").startswith("KVM gate"))
        self.assertLessEqual(gate["timeout-minutes"], 30, "a hung apt must not hold the Stage B concurrency group for hours")

    def test_independent_concurrency_groups(self):
        soak = load(B)["jobs"]["stage-b"]["concurrency"]
        self.assertNotEqual(load(A)["concurrency"]["group"], soak["group"])
        self.assertFalse(load(A)["concurrency"]["cancel-in-progress"])
        self.assertFalse(soak["cancel-in-progress"])
        self.assertNotIn("concurrency", load(B), "the soak lock is job-level so guard-only pushes never queue behind a running soak")
        self.assertNotIn("concurrency", load(B)["jobs"]["guard"])

    def test_stage_a_summary_needs_every_stage_a_leg(self):
        jobs = load(A)["jobs"]
        legs = {name for name in jobs if name.startswith("stage-a-") and name != "stage-a-summary"}
        self.assertEqual(set(jobs["stage-a-summary"]["needs"]), legs | {"guard"})
        self.assertIn("always()", jobs["stage-a-summary"]["if"])

    def test_job_names_used_in_records_match_the_summary_contract(self):
        text = A.read_text(encoding="utf-8")
        used = set(re.findall(r"--job (stage-a-[a-z0-9-]+)", text))
        expected = {job for _, job in summ.CONSUMED_REQUIRED} | {summ.SMOKE_JOB}
        self.assertEqual(used, expected)
        for job in expected:
            self.assertIn(job, load(A)["jobs"], "the --job value must be the workflow job id")

    def test_runner_labels_in_records_match_runs_on(self):
        jobs = load(A)["jobs"]
        for match in re.finditer(r"--job (stage-a-[a-z0-9-]+)[^\n]*\n[^\n]*--runner ([\w.-]+)", A.read_text(encoding="utf-8")):
            job, runner = match.groups()
            with self.subTest(job):
                self.assertEqual(jobs[job]["runs-on"], runner)
        self.assertEqual(jobs[summ.SMOKE_JOB]["runs-on"], rec.AARCH64_RUNNER)

    def test_each_leg_records_before_and_after_and_digest_checks_its_own_artifact(self):
        jobs = load(A)["jobs"]
        for name, job in jobs.items():
            if not name.startswith("stage-a-") or name == "stage-a-summary":
                continue
            text = "\n".join(str(step.get("run", "")) for step in job["steps"])
            with self.subTest(name):
                self.assertIn("fetch_handoff.py", text, "every leg must digest-check the artifact itself")
                self.assertIn("stage_a_record.py", text)
                self.assertIn("--before", text)
                self.assertEqual(job["env"]["GH_TOKEN"], "${{ github.token }}")

    def test_the_summary_artifact_has_the_contract_name(self):
        text = A.read_text(encoding="utf-8")
        self.assertIn("name: wave0-stage-a-evidence", text)
        self.assertIn("bundle/stage-a-summary.json", text)

    def test_checkouts_do_not_persist_credentials(self):
        for path in (A, B):
            for name, job in load(path)["jobs"].items():
                for step in job["steps"]:
                    if str(step.get("uses", "")).startswith("actions/checkout@"):
                        with self.subTest(path.name, job=name):
                            self.assertIs(step["with"]["persist-credentials"], False)

    def test_stage_b_uses_the_documented_scripts(self):
        text = B.read_text(encoding="utf-8")
        for needle in ("host_run.py preflight", "host_run.py run", "host_run.py collect", "evaluate_stage_b.py --evidence", "name: wave0-stage-b-evidence"):
            self.assertIn(needle, text)

    def test_guards_run_the_tree_binding_and_the_offline_tests(self):
        for path in (A, B):
            text = "\n".join(str(step.get("run", "")) for step in load(path)["jobs"]["guard"]["steps"])
            with self.subTest(path.name):
                self.assertIn("wave0-lab/guard.sh", text)
                self.assertIn("check_config.py", text)
                self.assertIn("unittest discover", text)

    def test_the_workflows_only_name_files_the_lab_branch_adds(self):
        for path in (A, B):
            for ref in set(re.findall(r"wave0-lab/[\w./-]+", path.read_text(encoding="utf-8"))):
                if ref.endswith("/") or "*" in ref:
                    continue
                with self.subTest(path.name, ref=ref):
                    self.assertTrue((_paths.ROOT / ref).exists(), ref)


if __name__ == "__main__":
    unittest.main()
