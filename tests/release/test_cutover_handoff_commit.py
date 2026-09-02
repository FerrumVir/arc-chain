#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
RECOVERY_DIR = REPO_ROOT / "scripts" / "recovery"
if str(RECOVERY_DIR) not in sys.path:
    sys.path.insert(0, str(RECOVERY_DIR))

import recovery_rollout


COMMIT_VALIDATOR = (
    REPO_ROOT / "scripts" / "release" / "validate-cutover-handoff-commit.py"
)
COMMIT_CREATOR = (
    REPO_ROOT / "scripts" / "release" / "create-cutover-handoff-commit.py"
)
PRODUCER_WORKFLOW = REPO_ROOT / ".github" / "workflows" / "recovery-release-handoff.yml"
FIXTURE_BUILDER = REPO_ROOT / "tests" / "release" / "make_cutover_release_fixture.py"
PUBLIC_FILES = (
    "arc-cutover-policy.json",
    "arc-legacy-maintenance-boundary.json",
    "arc-recovery-checkpoint-descriptor.json",
)


def canonical(value: object) -> bytes:
    return (
        json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
        + "\n"
    ).encode()


def replace_exact(value, old: str, new: str):
    if isinstance(value, dict):
        return {key: replace_exact(item, old, new) for key, item in value.items()}
    if isinstance(value, list):
        return [replace_exact(item, old, new) for item in value]
    return value.replace(old, new) if isinstance(value, str) else value


class GitFixture(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.repository = self.root / "repository"
        self.repository.mkdir()
        self.git("init", "-b", "main")
        self.git("config", "user.name", "ARC Test")
        self.git("config", "user.email", "arc-test@example.invalid")
        (self.repository / "README").write_text("main\n", encoding="utf-8")
        self.git("add", "README")
        self.git("commit", "-m", "main")
        self.main_commit = self.git("rev-parse", "HEAD")

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def git(
        self, *arguments: str, input_text: str | None = None, cwd: Path | None = None
    ) -> str:
        completed = subprocess.run(
            ["git", "-C", str(cwd or self.repository), *arguments],
            input=input_text,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        return completed.stdout.strip()

    def tree(self, entries: dict[str, tuple[str, bytes]]) -> str:
        records = []
        for name, (mode, payload) in sorted(entries.items()):
            blob = self.git("hash-object", "-w", "--stdin", input_text=payload.decode())
            records.append(f"{mode} blob {blob}\t{name}\n")
        return self.git("mktree", input_text="".join(records))

    def commit(self, tree: str, parent: str) -> str:
        return self.git("commit-tree", tree, "-p", parent, input_text="handoff\n")

    def run_validator(
        self, handoff: str, *, output_name: str
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                sys.executable,
                str(COMMIT_VALIDATOR),
                "--repository-root",
                str(self.repository),
                "--handoff-commit",
                handoff,
                "--main-commit",
                self.main_commit,
                "--output-dir",
                str(self.root / output_name),
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

    @staticmethod
    def exact_entries() -> dict[str, tuple[str, bytes]]:
        return {name: ("100644", f"{name}\n".encode()) for name in PUBLIC_FILES}


class CutoverHandoffCommitValidationTests(GitFixture):
    def test_exact_parent_tree_and_modes_materialize_create_only(self) -> None:
        handoff = self.commit(self.tree(self.exact_entries()), self.main_commit)
        result = self.run_validator(handoff, output_name="materialized")
        self.assertEqual(result.returncode, 0, result.stderr)
        output = self.root / "materialized"
        self.assertEqual(tuple(sorted(path.name for path in output.iterdir())), PUBLIC_FILES)
        for name in PUBLIC_FILES:
            self.assertEqual((output / name).stat().st_mode & 0o777, 0o444)
            self.assertEqual((output / name).read_bytes(), f"{name}\n".encode())

    def test_wrong_parent_is_rejected(self) -> None:
        other_parent = self.commit(self.git("rev-parse", "HEAD^{tree}"), self.main_commit)
        handoff = self.commit(self.tree(self.exact_entries()), other_parent)
        result = self.run_validator(handoff, output_name="wrong-parent")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("sole parent", result.stderr)

    def test_extra_tree_entry_is_rejected(self) -> None:
        entries = self.exact_entries()
        entries["unexpected.json"] = ("100644", b"unexpected\n")
        handoff = self.commit(self.tree(entries), self.main_commit)
        result = self.run_validator(handoff, output_name="extra")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exact three-file contract", result.stderr)

    def test_non_0644_mode_is_rejected(self) -> None:
        entries = self.exact_entries()
        entries[PUBLIC_FILES[0]] = ("100755", b"executable\n")
        handoff = self.commit(self.tree(entries), self.main_commit)
        result = self.run_validator(handoff, output_name="mode")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("non-regular or non-0644", result.stderr)


class CutoverHandoffCreationCommandTests(GitFixture):
    def test_local_command_creates_and_pushes_only_compact_assets_without_worktree_edits(
        self,
    ) -> None:
        full_handoff = self.root / "full-handoff"
        fake_binary = self.root / "arc-node"
        inspector_binary = self.root / "arc-node-release-inspector"
        subprocess.run(
            [
                sys.executable,
                str(FIXTURE_BUILDER),
                "--handoff-dir",
                str(full_handoff),
                "--binary",
                str(fake_binary),
                "--genesis",
                str(REPO_ROOT / "genesis.toml"),
            ],
            check=True,
        )
        inspector_binary.write_bytes(fake_binary.read_bytes())
        inspector_binary.chmod(0o755)
        fake_binary.chmod(0o600)
        fake_binary.write_bytes(fake_binary.read_bytes() + b"# local platform verifier\n")
        fake_binary.chmod(0o755)
        boundary_path = full_handoff / "legacy-maintenance-boundary.json"
        manifest_path = full_handoff / "arc-recovery-final.lock.json"
        boundary = json.loads(boundary_path.read_bytes())
        manifest = json.loads(manifest_path.read_bytes())
        original_source_commit = manifest["provenance"]["source_main_commit"]
        manifest = replace_exact(manifest, original_source_commit, self.main_commit)
        boundary["source_main_commit"] = self.main_commit
        boundary_payload = canonical(boundary)
        boundary_path.chmod(0o600)
        boundary_path.write_bytes(boundary_payload)
        boundary_path.chmod(0o444)
        boundary_sha256 = hashlib.sha256(boundary_payload).hexdigest()
        manifest["artifacts"]["legacy_maintenance_boundary"]["sha256"] = boundary_sha256
        manifest["chain"]["legacy_maintenance_boundary_sha256"] = boundary_sha256
        manifest["archive"]["prearchive_rollout_sha256"] = "0" * 64
        manifest["archive"]["prearchive_rollout_sha256"] = (
            recovery_rollout.prearchive_projection_digest(manifest)
        )
        manifest_payload = canonical(manifest)
        manifest_path.chmod(0o600)
        manifest_path.write_bytes(manifest_payload)
        manifest_path.chmod(0o444)
        sidecar = full_handoff / "arc-recovery-final.lock.json.sha256"
        sidecar.chmod(0o600)
        sidecar.write_text(
            f"{hashlib.sha256(manifest_payload).hexdigest()}  arc-recovery-final.lock.json\n",
            encoding="ascii",
        )
        sidecar.chmod(0o444)

        remote = self.root / "remote.git"
        subprocess.run(["git", "init", "--bare", str(remote)], check=True, capture_output=True)
        self.git("remote", "add", "origin", str(remote))
        before = self.git("status", "--porcelain=v1", "--untracked-files=all")
        command = [
            sys.executable,
            str(COMMIT_CREATOR),
            "--repository-root",
            str(self.repository),
            "--full-handoff-dir",
            str(full_handoff),
            "--verifier-binary",
            str(fake_binary),
            "--inspector-binary",
            str(inspector_binary),
            "--genesis",
            str(REPO_ROOT / "genesis.toml"),
            "--main-commit",
            self.main_commit,
            "--tag",
            "v0.8.0",
            "--push-remote",
            "origin",
        ]
        result = subprocess.run(
            command,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        receipt = json.loads(result.stdout)
        handoff = receipt["handoff_commit_sha"]
        ref_name = f"refs/arc-recovery-handoffs/{self.main_commit}"
        self.assertEqual(receipt["handoff_ref"], ref_name)
        self.assertEqual(
            self.git("rev-list", "--parents", "-n", "1", handoff),
            f"{handoff} {self.main_commit}",
        )
        self.assertEqual(self.git("ls-tree", "-r", "--name-only", handoff).splitlines(), list(PUBLIC_FILES))
        self.assertEqual(self.git("status", "--porcelain=v1", "--untracked-files=all"), before)
        self.assertEqual(
            self.git("--git-dir", str(remote), "rev-parse", ref_name, cwd=self.root),
            handoff,
        )
        listing = self.git("ls-tree", "-r", "--name-only", handoff)
        self.assertNotIn("recovery.arcchkpt", listing)
        self.assertNotIn("arc-recovery-final.lock.json", listing)

        repeated = subprocess.run(
            command,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertNotEqual(repeated.returncode, 0)
        self.assertIn("create-only handoff ref already exists", repeated.stderr)


class RecoveryHandoffWorkflowContractTests(unittest.TestCase):
    def test_producer_is_exact_main_protected_compact_and_full_handoff_free(self) -> None:
        workflow = PRODUCER_WORKFLOW.read_text(encoding="utf-8")
        for required in (
            "workflow_dispatch:",
            "handoff_commit_sha:",
            "if: github.ref == 'refs/heads/main'",
            "environment: release",
            'LIVE_MAIN_SHA="$(gh api',
            'HANDOFF_REF="refs/arc-recovery-handoffs/$GITHUB_SHA"',
            "validate-cutover-handoff-commit.py",
            "validate-cutover-derived-assets.py",
            "--verifier-binary target/release/arc-node",
            "arc-recovery-release-handoff-${{ github.sha }}",
            "derived-cutover-handoff/*",
            "overwrite: false",
        ):
            self.assertIn(required, workflow)
        self.assertNotIn("recovery.arcchkpt", workflow)
        self.assertNotIn("arc-recovery-final.lock.json", workflow)
        self.assertNotIn("drive", workflow.lower())


if __name__ == "__main__":
    unittest.main()
