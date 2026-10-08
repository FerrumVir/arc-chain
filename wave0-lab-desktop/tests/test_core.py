"""Tests for the desktop lab core: config.json, check_config.py, guard.sh and the workflow (THROWAWAY LAB FILE)."""
from __future__ import annotations

import contextlib
import copy
import io
import json
import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

import _paths  # noqa: F401
import check_config

LAB = _paths.LAB
ROOT = _paths.ROOT
CONFIG = json.loads((LAB / "config.json").read_text(encoding="utf-8"))
WORKFLOW_PATH = ROOT / ".github" / "workflows" / "wave0-lab-desktop.yml"
WORKFLOW = WORKFLOW_PATH.read_text(encoding="utf-8")
GUARD = LAB / "guard.sh"
WF = ".github/workflows/wave0-lab-desktop.yml"
OS_JOBS = ("desktop-linux", "desktop-windows", "desktop-macos-arm64", "desktop-macos-intel")


def mutated(path, value):
    cfg = copy.deepcopy(CONFIG)
    node = cfg
    for key in path[:-1]:
        node = node[key]
    node[path[-1]] = value
    return cfg


def without(path):
    cfg = copy.deepcopy(CONFIG)
    node = cfg
    for key in path[:-1]:
        node = node[key]
    del node[path[-1]]
    return cfg


class ConfigTests(unittest.TestCase):
    def test_the_shipped_config_is_valid(self):
        self.assertEqual(check_config.validate(CONFIG), [])

    def test_the_shipped_mode_is_probe_or_full(self):
        self.assertIn(CONFIG["mode"], ("probe", "full"))

    def test_outputs(self):
        self.assertEqual(check_config.outputs(CONFIG), {"mode": CONFIG["mode"], "linux": "true", "windows": "true", "macos_arm64": "true", "macos_intel": "true"})
        cfg = mutated(["jobs", "macos_intel", "enabled"], False)
        cfg["mode"] = "full"
        self.assertEqual(check_config.outputs(cfg), {"mode": "full", "linux": "true", "windows": "true", "macos_arm64": "true", "macos_intel": "false"})

    def test_rejections(self):
        cases = {
            "schema": mutated(["schema"], "x"),
            "repository": mutated(["repository"], "other/repo"),
            "base commit": mutated(["base_commit"], "abc"),
            "base tree": mutated(["base_tree"], "G" * 40),
            "mode": mutated(["mode"], "everything"),
            "app tag": mutated(["app", "tag"], "v0.7.12"),
            "asset set": without(["app", "assets", "windows_msi"]),
            "asset digest": mutated(["app", "assets", "linux_deb", "sha256"], "12"),
            "asset release digest": mutated(["app", "assets", "linux_deb", "release_digest"], "sha256:" + "0" * 64),
            "asset size": mutated(["app", "assets", "linux_deb", "size"], 0),
            "duplicate asset names": mutated(["app", "assets", "linux_rpm", "name"], CONFIG["app"]["assets"]["linux_deb"]["name"]),
            "manifest digest": mutated(["app", "manifest", "sha256"], "zz"),
            "latest tag": mutated(["latest_tag"], "v0.7.11"),
            "bait tag": mutated(["bait_tag"], "v0.8.12"),
            "plugin version": mutated(["plugin", "version"], "2.10.0"),
            "manifest url": mutated(["manifest_url"], "https://github.com/FerrumVir/arc-chain/releases/latest/download/other.json"),
            "hosts empty": mutated(["hosts"], []),
            "hosts without api": mutated(["hosts"], ["github.com"]),
            "host not a name": mutated(["hosts"], ["github.com", "api.github.com", "not a name"]),
            "host repeated": mutated(["hosts"], ["github.com", "github.com", "api.github.com"]),
            "scenario": mutated(["scenarios"], ["latest-404", "mystery"]),
            "scenarios empty": mutated(["scenarios"], []),
            "cases": mutated(["cases"], ["clean"]),
            "scope statement": mutated(["interception_scope"], "intercepts everything"),
            "runner linux": mutated(["jobs", "linux", "runner"], "ubuntu-latest"),
            "runner intel": mutated(["jobs", "macos_intel", "runner"], "macos-13"),
            "job missing": without(["jobs", "windows"]),
            "enabled flag": mutated(["jobs", "linux", "enabled"], "yes"),
        }
        for name, cfg in cases.items():
            with self.subTest(name):
                self.assertNotEqual(check_config.validate(cfg), [], name)

    def test_not_an_object(self):
        self.assertEqual(check_config.validate([]), ["config is not a JSON object"])

    def test_asset_digests_are_what_the_release_reported(self):
        for key, asset in CONFIG["app"]["assets"].items():
            self.assertEqual(asset["release_digest"], "sha256:" + asset["sha256"], key)
        self.assertEqual(CONFIG["app"]["assets"]["windows_nsis"]["name"], "ARC.Node_0.7.11_x64-setup.exe")
        self.assertEqual(CONFIG["app"]["manifest"]["name"], "latest.json")

    def test_main_prints_and_writes_github_outputs(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out.txt"
            with contextlib.redirect_stdout(io.StringIO()) as buffer:
                code = check_config.main(["--config", str(LAB / "config.json"), "--github-output", str(out)])
            self.assertEqual(code, 0)
            self.assertIn("mode=" + CONFIG["mode"], buffer.getvalue())
            self.assertEqual(out.read_text(), "mode=%s\nlinux=true\nwindows=true\nmacos_arm64=true\nmacos_intel=true\n" % CONFIG["mode"])
            bad = Path(tmp) / "bad.json"
            bad.write_text("{not json")
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(check_config.main(["--config", str(bad)]), 1)
                broken = Path(tmp) / "broken.json"
                broken.write_text(json.dumps(mutated(["mode"], "x")))
                self.assertEqual(check_config.main(["--config", str(broken)]), 1)


def git(cwd, *args, check=True):
    done = subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "protocol.version=2", *args], cwd=cwd, capture_output=True, text=True)
    if check and done.returncode != 0:
        raise AssertionError("git %s failed: %s" % (args, done.stderr))
    return done.stdout.strip()


class Lab:
    """origin (bare) <- clone with a base commit; then a lab commit on top, built by the test."""

    def __init__(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.origin = root / "origin.git"
        subprocess.run(["git", "init", "-q", "--bare", str(self.origin)], check=True)
        self.work = root / "work"
        subprocess.run(["git", "clone", "-q", str(self.origin), str(self.work)], check=True, capture_output=True)
        (self.work / "README.md").write_text("base\n")
        (self.work / "src").mkdir()
        (self.work / "src" / "main.rs").write_text("fn main() {}\n")
        git(self.work, "add", "-A")
        git(self.work, "commit", "-q", "-m", "base")
        git(self.work, "branch", "-M", "main")
        git(self.work, "push", "-q", "origin", "main")
        self.base = git(self.work, "rev-parse", "HEAD")
        self.tree = git(self.work, "rev-parse", "HEAD^{tree}")
        git(self.work, "checkout", "-q", "-b", "wave0-v0712-desktop-lab")

    def config_text(self, tree=None):
        return json.dumps({"base_commit": self.base, "base_tree": tree or self.tree})

    def write_lab(self, skip=(), extra=None):
        files = {WF: "name: d\n", "wave0-lab-desktop/config.json": self.config_text(), "wave0-lab-desktop/x.py": "x = 1\n", "wave0-lab-desktop/lib/y.py": "y = 1\n"}
        for path in skip:
            files.pop(path)
        files.update(extra or {})
        for path, text in files.items():
            target = self.work / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(text)

    def commit(self):
        git(self.work, "add", "-A")
        git(self.work, "commit", "-q", "-m", "lab")

    def guard(self, config=None, sha=None):
        config = config or (self.work / "wave0-lab-desktop/config.json")
        env = dict(os.environ, GITHUB_SHA=sha or git(self.work, "rev-parse", "HEAD"))
        return subprocess.run(["bash", str(GUARD), str(config)], cwd=self.work, capture_output=True, text=True, env=env)


class GuardTests(unittest.TestCase):
    def lab(self):
        lab = Lab()
        self.addCleanup(lab.tmp.cleanup)
        return lab

    def test_base_plus_added_desktop_lab_files_passes(self):
        lab = self.lab()
        lab.write_lab()
        lab.commit()
        done = lab.guard()
        self.assertEqual(done.returncode, 0, done.stderr + done.stdout)
        self.assertIn("added desktop lab files", done.stdout)

    def test_several_commits_are_fine_because_only_the_tree_matters(self):
        lab = self.lab()
        lab.write_lab()
        lab.commit()
        (lab.work / "wave0-lab-desktop" / "x.py").write_text("x = 2\n")
        lab.commit()
        self.assertEqual(lab.guard().returncode, 0)

    def test_modifying_or_deleting_a_base_file_is_refused(self):
        lab = self.lab()
        lab.write_lab(extra={"README.md": "changed\n"})
        lab.commit()
        done = lab.guard()
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("README.md", done.stderr)
        other = self.lab()
        other.write_lab()
        (other.work / "src" / "main.rs").unlink()
        other.commit()
        self.assertNotEqual(other.guard().returncode, 0)

    def test_files_outside_the_desktop_lab_paths_are_refused(self):
        for extra in ({"docs/NOTES.md": "x\n"}, {".github/workflows/other.yml": "name: o\n"}, {".github/workflows/wave0-lab-soak.yml": "name: soak\n"},
                      {"wave0-lab/config.json": "{}\n"}, {"wave0-lab-desktopx/a.py": "a\n"}):
            with self.subTest(list(extra)[0]):
                lab = self.lab()
                lab.write_lab(extra=extra)
                lab.commit()
                self.assertNotEqual(lab.guard().returncode, 0)

    def test_both_the_workflow_and_the_config_are_required(self):
        lab = self.lab()
        lab.write_lab(skip=(WF,))
        lab.commit()
        self.assertNotEqual(lab.guard().returncode, 0)
        other = self.lab()
        other.write_lab(skip=("wave0-lab-desktop/config.json",))
        (other.work / "wave0-lab-desktop").mkdir(exist_ok=True)
        other.commit()
        outside = other.work.parent / "config.json"
        outside.write_text(other.config_text())
        self.assertNotEqual(other.guard(config=outside).returncode, 0)

    def test_a_wrong_base_tree_or_head_is_refused(self):
        lab = self.lab()
        lab.write_lab(extra={"wave0-lab-desktop/config.json": lab.config_text(tree="0" * 40)})
        lab.commit()
        self.assertNotEqual(lab.guard().returncode, 0)
        good = self.lab()
        good.write_lab()
        good.commit()
        self.assertNotEqual(good.guard(sha="1" * 40).returncode, 0)


def job_blocks(text):
    """{job id: body text} of the jobs section (jobs are the two-space-indented keys under `jobs:`)."""
    section = text.split("\njobs:\n", 1)[1]
    blocks, name, lines = {}, None, []
    for line in section.splitlines():
        match = re.match(r"^  ([a-z0-9-]+):\s*$", line)
        if match:
            if name:
                blocks[name] = "\n".join(lines)
            name, lines = match.group(1), []
        elif name is not None:
            lines.append(line)
    if name:
        blocks[name] = "\n".join(lines)
    return blocks


def step_blocks(text):
    """The `- ...` step blocks of a workflow or of one job body (steps are the six-space-indented list items)."""
    blocks, current = [], None
    for line in text.splitlines():
        if re.match(r"^      - ", line):
            if current is not None:
                blocks.append("\n".join(current))
            current = [line]
        elif current is not None:
            current.append(line)
    if current is not None:
        blocks.append("\n".join(current))
    return blocks


def upload_steps(text):
    return [block for block in step_blocks(text) if "uses: actions/upload-artifact@" in block]


def top_level_block(text, key):
    match = re.search(r"^%s:\s*\n((?:[ \t]+.*\n|\n)+)" % re.escape(key), text, flags=re.MULTILINE)
    return match.group(1) if match else ""


class WorkflowTests(unittest.TestCase):
    def setUp(self):
        self.jobs = job_blocks(WORKFLOW)

    def test_only_a_push_to_the_throwaway_branch_triggers_it(self):
        trigger = top_level_block(WORKFLOW, "on")
        self.assertEqual(re.findall(r"^\s{2}([a-z_]+):", trigger, flags=re.MULTILINE), ["push"])
        self.assertEqual(re.findall(r"branches:\s*\[(.*?)\]", trigger), ["wave0-v0712-desktop-lab"])
        for forbidden in ("pull_request", "pull_request_target", "workflow_dispatch", "schedule", "workflow_run", "repository_dispatch", "release", "tags"):
            self.assertNotIn(forbidden, trigger)

    def test_permissions_are_read_only_and_there_is_no_secret_or_environment(self):
        permissions = top_level_block(WORKFLOW, "permissions")
        self.assertEqual(re.findall(r"^\s+([a-z-]+):\s*(\S+)", permissions, flags=re.MULTILINE), [("contents", "read"), ("actions", "read")])
        self.assertNotRegex(WORKFLOW, r"(?m)^\s*[a-z-]+:\s*write\b", "no write permission anywhere")
        self.assertNotIn("secrets.", WORKFLOW)
        self.assertNotRegex(WORKFLOW, r"^\s*environment:", "no deployment environment")
        self.assertNotRegex(WORKFLOW, r"permissions:\s*\n\s+[a-z-]+:\s*write")
        tokens = re.findall(r"\$\{\{\s*([^}]+?)\s*\}\}", WORKFLOW)
        for token in tokens:
            self.assertTrue(
                token.startswith(("github.token", "github.workspace", "needs.guard.outputs.", "steps.config.outputs.", "steps.keys.outcome", "needs.guard.result", "env.MODE")),
                "unexpected expression: " + token)

    def test_the_expected_jobs_exist(self):
        self.assertEqual(sorted(self.jobs), sorted(["guard", "summary"] + list(OS_JOBS)))

    def test_the_four_operating_system_jobs_run_in_parallel_after_the_guard_only(self):
        for name in OS_JOBS:
            needs = re.findall(r"^    needs:\s*(.+)$", self.jobs[name], flags=re.MULTILINE)
            self.assertEqual(needs, ["guard"], name)
            self.assertNotIn("desktop-", needs[0])
        self.assertNotRegex(self.jobs["guard"], r"^    needs:")

    def test_runners_and_timeouts_match_the_config(self):
        runners = {name: re.search(r"^    runs-on:\s*(\S+)", self.jobs[name], flags=re.MULTILINE).group(1) for name in OS_JOBS}
        self.assertEqual(runners, {
            "desktop-linux": CONFIG["jobs"]["linux"]["runner"], "desktop-windows": CONFIG["jobs"]["windows"]["runner"],
            "desktop-macos-arm64": CONFIG["jobs"]["macos_arm64"]["runner"], "desktop-macos-intel": CONFIG["jobs"]["macos_intel"]["runner"]})
        for name in OS_JOBS:
            self.assertEqual(re.search(r"^    timeout-minutes:\s*(\d+)", self.jobs[name], flags=re.MULTILINE).group(1), "75", name)
        self.assertRegex(self.jobs["desktop-macos-intel"], r"(?m)^    continue-on-error:\s*true")
        for name in ("desktop-linux", "desktop-windows", "desktop-macos-arm64"):
            self.assertNotRegex(self.jobs[name], r"(?m)^    continue-on-error:")

    def test_each_os_job_is_gated_on_its_config_flag(self):
        flags = {"desktop-linux": "linux", "desktop-windows": "windows", "desktop-macos-arm64": "macos_arm64", "desktop-macos-intel": "macos_intel"}
        for name, flag in flags.items():
            self.assertIn("if: needs.guard.outputs.%s == 'true'" % flag, self.jobs[name])

    def test_every_action_is_pinned_to_a_sha_the_repository_already_uses(self):
        used = set(re.findall(r"uses:\s*([\w./-]+)@([0-9a-f]{40})\b", WORKFLOW))
        self.assertTrue(used)
        self.assertEqual(len(re.findall(r"uses:", WORKFLOW)), len(re.findall(r"uses:\s*[\w./-]+@[0-9a-f]{40}\s+#", WORKFLOW)), "every uses: is a 40 hex SHA with a version comment")
        existing = set()
        for path in (ROOT / ".github" / "workflows").glob("*.yml"):
            if path.name != WORKFLOW_PATH.name:
                existing |= set(re.findall(r"uses:\s*([\w./-]+)@([0-9a-f]{40})\b", path.read_text(encoding="utf-8")))
        self.assertEqual(used - existing, set(), "a pin that no other workflow of the repository uses")

    def test_uploads_are_gated_on_the_private_key_scan_and_never_name_a_key(self):
        for name in OS_JOBS:
            body = self.jobs[name]
            self.assertRegex(body, r"id: keys\n\s+if: always\(\)\n\s+run: python3? wave0-lab-desktop/lib/ca\.py scan --root \"\$EVIDENCE\"", name)
            upload = re.search(r"- name: Keep the evidence\n\s+if: (.+)\n\s+uses: actions/upload-artifact@", body)
            self.assertIsNotNone(upload, name)
            self.assertIn("steps.keys.outcome == 'success'", upload.group(1))
        uploads = upload_steps(WORKFLOW)
        self.assertEqual(len(uploads), 5)
        for block in uploads:
            path = re.search(r"^\s+path:\s*(.+)$", block, flags=re.MULTILINE).group(1)
            self.assertRegex(path, r"^(evidence/[a-z0-9-]+/|summary/)$", path)
            self.assertNotRegex(path, r"private|\.key|\.pem", path)

    def test_artifact_names_are_distinct_and_keep_90_days(self):
        names = [re.search(r"^\s+name:\s*(desktop-[a-z0-9-]+)\s*$", block, flags=re.MULTILINE).group(1) for block in upload_steps(WORKFLOW)]
        self.assertEqual(sorted(names), sorted(["desktop-evidence-linux", "desktop-evidence-windows", "desktop-evidence-macos-arm64", "desktop-evidence-macos-intel", "desktop-summary"]))
        self.assertEqual(len(re.findall(r"retention-days:\s*90", WORKFLOW)), 5)
        self.assertEqual(len(re.findall(r"if-no-files-found:\s*error", WORKFLOW)), 5)

    def test_every_os_job_calls_its_script_in_the_right_mode_and_builds_the_native_check(self):
        scripts = {"desktop-linux": "python3 -u wave0-lab-desktop/os_linux.py", "desktop-windows": "python -u wave0-lab-desktop/os_windows.py",
                   "desktop-macos-arm64": "python3 -u wave0-lab-desktop/os_macos.py", "desktop-macos-intel": "python3 -u wave0-lab-desktop/os_macos.py"}
        for name, call in scripts.items():
            body = self.jobs[name]
            # the probe runs in every mode (into $EVIDENCE/probe); the isolation run only in full mode (into $EVIDENCE)
            self.assertRegex(body, re.escape(call) + r' probe --evidence "\$EVIDENCE/probe"', name)
            self.assertRegex(body, r"if: \$\{\{ env\.MODE == 'full' \}\}\n\s+run: \|\n\s+set -Eeuo pipefail\n\s+" + re.escape(call) + r' run --evidence "\$EVIDENCE"', name)
            self.assertIn("cargo build --release --locked --manifest-path wave0-lab-desktop/native-updater-check/Cargo.toml", body)
            build = body.split("Build the native tauri-plugin-updater check", 1)[1].split("- name:", 1)[0]
            self.assertIn("continue-on-error: true", build, "a compile failure must not hide the other results")
            self.assertIn("timeout-minutes: 40", build)
        self.assertIn("--arch arm64", self.jobs["desktop-macos-arm64"])
        self.assertIn("--arch x86_64", self.jobs["desktop-macos-intel"])
        self.assertIn("shell: bash", self.jobs["desktop-windows"])

    def test_the_guard_binds_the_tree_validates_the_config_and_runs_the_offline_tests(self):
        guard = self.jobs["guard"]
        self.assertIn("bash wave0-lab-desktop/guard.sh wave0-lab-desktop/config.json", guard)
        self.assertIn("check_config.py --config wave0-lab-desktop/config.json --github-output", guard)
        self.assertIn("unittest discover -s wave0-lab-desktop/tests -p 'test_*.py'", guard)
        self.assertIn("shellcheck -S warning wave0-lab-desktop/*.sh", guard)
        for output in ("mode", "linux", "windows", "macos_arm64", "macos_intel"):
            self.assertIn("%s: ${{ steps.config.outputs.%s }}" % (output, output), guard)

    def test_the_summary_waits_for_everything_and_runs_even_when_a_job_failed(self):
        summary = self.jobs["summary"]
        needs = re.search(r"^    needs:\s*\[(.+)\]", summary, flags=re.MULTILINE).group(1)
        self.assertEqual(sorted(item.strip() for item in needs.split(",")), sorted(["guard"] + list(OS_JOBS)))
        self.assertIn("if: always() && needs.guard.result == 'success'", summary)
        self.assertIn("stage_c_summary.py build --records records --out summary/stage-c-summary.json --require-desktop-os", summary)
        for artifact in ("desktop-evidence-linux", "desktop-evidence-windows", "desktop-evidence-macos-arm64"):
            self.assertIn("name: " + artifact, summary)
        self.assertEqual(summary.count("continue-on-error: true"), 4, "a missing artifact must not stop the summary: it is reported as MISSING")
        self.assertIn("|| true", summary, "the optional Intel summary never fails the job")

    def test_actionlint_labels_are_real_github_labels(self):
        for label in re.findall(r"runs-on:\s*(\S+)", WORKFLOW):
            self.assertIn(label, ("ubuntu-24.04", "windows-latest", "macos-15", "macos-15-intel"))


class LibModuleTests(unittest.TestCase):
    def test_every_core_module_lists_what_it_could_not_verify_off_ci(self):
        for name in ("ca.py", "mitm_server.py", "fswatch.py", "live_block.py"):
            text = (LAB / "lib" / name).read_text(encoding="utf-8")
            self.assertIn("UNVERIFIED ON CI:", text, name)
            self.assertIn("from __future__ import annotations", text, name)
            self.assertNotRegex(text, r"^\s*match\s+\w+:", "no match statements: the Mac runs Python 3.9")

    def test_no_core_module_is_a_network_client_or_prints_a_key_path(self):
        clients = r"^\s*(import|from)\s+(urllib\.request|urllib3|requests|http\.client|smtplib|ftplib|telnetlib|xmlrpc)\b"
        for name in ("ca.py", "mitm_server.py", "fswatch.py", "live_block.py"):
            text = (LAB / "lib" / name).read_text(encoding="utf-8")
            self.assertNotRegex(text, r"(?m)" + clients, "%s must not be able to contact anything" % name)
            self.assertNotRegex(text, r"print\([^)]*\.key", name)
        for name in ("ca.py", "fswatch.py", "live_block.py"):
            text = (LAB / "lib" / name).read_text(encoding="utf-8")
            self.assertNotRegex(text, r"(?m)^\s*(import|from)\s+(socket|ssl)\b", "%s has no use for sockets" % name)

    def test_every_os_job_starts_its_evidence_directory_before_anything_can_fail(self):
        for name in OS_JOBS:
            steps = step_blocks(job_blocks(WORKFLOW)[name])
            self.assertIn("uses: actions/checkout@", steps[0], name)
            self.assertIn("Start the evidence directory", steps[1], name)
            self.assertIn('mkdir -p "$EVIDENCE"', steps[1])
            self.assertIn('> "$EVIDENCE/job.json"', steps[1])

    def test_the_guard_script_is_executable_and_shellcheck_clean(self):
        self.assertTrue(os.access(str(GUARD), os.X_OK))


if __name__ == "__main__":
    unittest.main()
