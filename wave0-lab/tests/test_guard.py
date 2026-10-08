"""wave0-lab/guard.sh against throwaway git repositories (THROWAWAY LAB FILE): the lab tree must be the base tree plus ADDED lab files."""
from __future__ import annotations

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

import _paths  # noqa: F401

GUARD = _paths.LAB / "guard.sh"
A = ".github/workflows/wave0-lab.yml"
B = ".github/workflows/wave0-lab-soak.yml"


def git(cwd, *args, check=True):
    done = subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "protocol.version=2", *args], cwd=cwd, capture_output=True, text=True)
    if check and done.returncode != 0:
        raise AssertionError(f"git {args} failed: {done.stderr}")
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
        git(self.work, "checkout", "-q", "-b", "wave0-v0712-lab")

    def write_lab(self, skip=(), extra=None):
        files = {A: "name: a\n", B: "name: b\n", "wave0-lab/config.json": json.dumps({"base_commit": self.base, "base_tree": self.tree}), "wave0-lab/x.py": "x = 1\n"}
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

    def guard(self, config=None):
        config = config or (self.work / "wave0-lab/config.json")
        env = dict(os.environ, GITHUB_SHA=git(self.work, "rev-parse", "HEAD"))
        return subprocess.run(["bash", str(GUARD), str(config)], cwd=self.work, capture_output=True, text=True, env=env)


class GuardTests(unittest.TestCase):
    def lab(self):
        lab = Lab()
        self.addCleanup(lab.tmp.cleanup)
        return lab

    def test_base_plus_added_lab_files_passes(self):
        lab = self.lab()
        lab.write_lab()
        lab.commit()
        done = lab.guard()
        self.assertEqual(done.returncode, 0, done.stderr + done.stdout)
        self.assertIn("added lab files", done.stdout)

    def test_several_lab_commits_are_fine_because_only_the_tree_matters(self):
        lab = self.lab()
        lab.write_lab()
        lab.commit()
        (lab.work / "wave0-lab" / "x.py").write_text("x = 2\n")
        lab.commit()
        self.assertEqual(lab.guard().returncode, 0)

    def test_modifying_a_base_file_is_refused(self):
        lab = self.lab()
        lab.write_lab(extra={"README.md": "changed\n"})
        lab.commit()
        done = lab.guard()
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("README.md", done.stderr)

    def test_deleting_a_base_file_is_refused(self):
        lab = self.lab()
        lab.write_lab()
        (lab.work / "src" / "main.rs").unlink()
        lab.commit()
        self.assertNotEqual(lab.guard().returncode, 0)

    def test_adding_a_file_outside_the_lab_paths_is_refused(self):
        lab = self.lab()
        lab.write_lab(extra={"docs/NOTES.md": "x\n"})
        lab.commit()
        self.assertNotEqual(lab.guard().returncode, 0)
        lab2 = self.lab()
        lab2.write_lab(extra={".github/workflows/other.yml": "name: o\n"})
        lab2.commit()
        self.assertNotEqual(lab2.guard().returncode, 0, "another workflow file is not a lab file")

    def test_a_missing_required_lab_file_is_refused(self):
        for missing in (A, B, "wave0-lab/config.json"):
            with self.subTest(missing):
                lab = self.lab()
                lab.write_lab(skip=(missing,), extra={"wave0-lab/config.json": json.dumps({"base_commit": lab.base, "base_tree": lab.tree})} if missing != "wave0-lab/config.json" else {})
                if missing == "wave0-lab/config.json":
                    (lab.work / "wave0-lab").mkdir(exist_ok=True)
                    cfg = lab.work.parent / "config.json"
                    cfg.write_text(json.dumps({"base_commit": lab.base, "base_tree": lab.tree}))
                lab.commit()
                done = lab.guard(config=(lab.work.parent / "config.json") if missing == "wave0-lab/config.json" else None)
                self.assertNotEqual(done.returncode, 0, done.stdout)

    def test_a_wrong_base_tree_is_refused(self):
        lab = self.lab()
        lab.write_lab(extra={"wave0-lab/config.json": json.dumps({"base_commit": lab.base, "base_tree": "0" * 40})})
        lab.commit()
        self.assertNotEqual(lab.guard().returncode, 0)

    def test_head_must_be_the_workflow_commit(self):
        lab = self.lab()
        lab.write_lab()
        lab.commit()
        env = dict(os.environ, GITHUB_SHA="1" * 40)
        done = subprocess.run(["bash", str(GUARD), str(lab.work / "wave0-lab/config.json")], cwd=lab.work, capture_output=True, text=True, env=env)
        self.assertNotEqual(done.returncode, 0)


if __name__ == "__main__":
    unittest.main()
