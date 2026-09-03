#!/usr/bin/env python3
from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


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
CREATOR_SPEC = importlib.util.spec_from_file_location(
    "arc_create_cutover_handoff_commit_test", COMMIT_CREATOR
)
assert CREATOR_SPEC is not None and CREATOR_SPEC.loader is not None
creator = importlib.util.module_from_spec(CREATOR_SPEC)
sys.modules[CREATOR_SPEC.name] = creator
CREATOR_SPEC.loader.exec_module(creator)
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
    def setUp(self) -> None:
        super().setUp()
        self.full_handoff = self.root / "full-handoff"
        self.fake_binary = self.root / "arc-node"
        self.inspector_binary = self.root / "arc-node-release-inspector"
        subprocess.run(
            [
                sys.executable,
                str(FIXTURE_BUILDER),
                "--handoff-dir",
                str(self.full_handoff),
                "--binary",
                str(self.fake_binary),
                "--genesis",
                str(REPO_ROOT / "genesis.toml"),
            ],
            check=True,
        )
        self.inspector_binary.write_bytes(self.fake_binary.read_bytes())
        self.inspector_binary.chmod(0o755)
        self.fake_binary.chmod(0o600)
        self.fake_binary.write_bytes(
            self.fake_binary.read_bytes() + b"# local platform verifier\n"
        )
        self.fake_binary.chmod(0o755)
        boundary_path = self.full_handoff / "legacy-maintenance-boundary.json"
        manifest_path = self.full_handoff / "arc-recovery-final.lock.json"
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
        sidecar = self.full_handoff / "arc-recovery-final.lock.json.sha256"
        sidecar.chmod(0o600)
        sidecar.write_text(
            f"{hashlib.sha256(manifest_payload).hexdigest()}  arc-recovery-final.lock.json\n",
            encoding="ascii",
        )
        sidecar.chmod(0o444)
        self.ref_name = f"refs/arc-recovery-handoffs/{self.main_commit}"
        self.command = [
            sys.executable,
            str(COMMIT_CREATOR),
            "--repository-root",
            str(self.repository),
            "--full-handoff-dir",
            str(self.full_handoff),
            "--verifier-binary",
            str(self.fake_binary),
            "--inspector-binary",
            str(self.inspector_binary),
            "--genesis",
            str(REPO_ROOT / "genesis.toml"),
            "--main-commit",
            self.main_commit,
            "--tag",
            "v0.8.0",
        ]

    def run_creator(self) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            self.command,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

    def create_local_handoff(self) -> dict[str, object]:
        result = self.run_creator()
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def test_fresh_and_exact_local_retry_are_deterministic_and_worktree_clean(
        self,
    ) -> None:
        before = self.git("status", "--porcelain=v1", "--untracked-files=all")
        first = self.create_local_handoff()
        self.assertEqual(first["local_ref_state"], "created")
        self.assertIsNone(first["remote_ref_state"])
        self.assertIsNone(first["pushed_remote"])
        handoff = str(first["handoff_commit_sha"])
        self.assertEqual(first["handoff_ref"], self.ref_name)
        self.assertEqual(
            self.git("rev-list", "--parents", "-n", "1", handoff),
            f"{handoff} {self.main_commit}",
        )
        self.assertEqual(
            self.git("ls-tree", "-r", "--name-only", handoff).splitlines(),
            list(PUBLIC_FILES),
        )
        listing = self.git("ls-tree", "-r", handoff)
        self.assertNotIn("recovery.arcchkpt", listing)
        self.assertNotIn("arc-recovery-final.lock.json", listing)
        self.assertEqual(
            self.git("status", "--porcelain=v1", "--untracked-files=all"), before
        )

        repeated = self.run_creator()
        self.assertEqual(repeated.returncode, 0, repeated.stderr)
        second = json.loads(repeated.stdout)
        self.assertEqual(second["local_ref_state"], "reused")
        self.assertEqual(second["handoff_commit_sha"], first["handoff_commit_sha"])
        self.assertEqual(second["handoff_tree_sha"], first["handoff_tree_sha"])
        self.assertEqual(self.git("rev-parse", self.ref_name), handoff)
        self.assertEqual(
            self.git("status", "--porcelain=v1", "--untracked-files=all"), before
        )

    def test_local_ref_parent_tree_asset_or_metadata_mismatch_is_never_replaced(
        self,
    ) -> None:
        expected = self.create_local_handoff()
        expected_commit = str(expected["handoff_commit_sha"])
        expected_tree = str(expected["handoff_tree_sha"])
        other_parent = self.commit(self.git("rev-parse", "HEAD^{tree}"), self.main_commit)
        variants = {
            "parent": self.commit(expected_tree, other_parent),
            "tree": self.commit(
                self.tree(
                    {
                        **self.exact_entries(),
                        PUBLIC_FILES[0]: ("100644", b"changed asset\n"),
                    }
                ),
                self.main_commit,
            ),
            "mode": self.commit(
                self.tree(
                    {
                        **self.exact_entries(),
                        PUBLIC_FILES[0]: ("100755", b"changed mode\n"),
                    }
                ),
                self.main_commit,
            ),
            "metadata": self.git(
                "commit-tree",
                expected_tree,
                "-p",
                self.main_commit,
                input_text="tampered handoff metadata\n",
            ),
        }
        for label, alternate in variants.items():
            with self.subTest(label=label):
                self.git("update-ref", self.ref_name, alternate, expected_commit)
                result = self.run_creator()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("differs from the freshly derived commit", result.stderr)
                self.assertEqual(self.git("rev-parse", self.ref_name), alternate)
                self.git("update-ref", self.ref_name, expected_commit, alternate)

    def test_receipt_reports_local_and_remote_created_or_reused_state(self) -> None:
        arguments = self.command[2:] + ["--push-remote", "origin"]
        with mock.patch.object(
            creator, "publish_remote_ref", return_value="created"
        ) as publisher, mock.patch("builtins.print") as output:
            self.assertEqual(creator.main(arguments), 0)
        receipt = json.loads(output.call_args.args[0])
        self.assertEqual(receipt["local_ref_state"], "created")
        self.assertEqual(receipt["remote_ref_state"], "created")
        self.assertEqual(receipt["pushed_remote"], "origin")
        publisher.assert_called_once_with(
            self.repository.resolve(),
            "origin",
            "FerrumVir/arc-chain",
            self.ref_name,
            receipt["handoff_commit_sha"],
        )

        with mock.patch.object(
            creator, "publish_remote_ref", return_value="reused"
        ), mock.patch("builtins.print") as output:
            self.assertEqual(creator.main(arguments), 0)
        retried = json.loads(output.call_args.args[0])
        self.assertEqual(retried["local_ref_state"], "reused")
        self.assertEqual(retried["remote_ref_state"], "reused")
        self.assertEqual(retried["handoff_commit_sha"], receipt["handoff_commit_sha"])

    def test_symbolic_local_handoff_ref_is_rejected_without_mutation(self) -> None:
        self.git("symbolic-ref", self.ref_name, "refs/heads/main")
        result = self.run_creator()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("handoff ref is symbolic", result.stderr)
        self.assertEqual(self.git("symbolic-ref", self.ref_name), "refs/heads/main")


class CutoverHandoffRemotePublicationTests(GitFixture):
    def setUp(self) -> None:
        super().setUp()
        self.ref_name = f"refs/arc-recovery-handoffs/{self.main_commit}"
        self.handoff = self.commit(self.tree(self.exact_entries()), self.main_commit)
        self.remote = self.root / "remote.git"
        subprocess.run(
            ["git", "init", "--bare", str(self.remote)],
            check=True,
            capture_output=True,
        )
        self.git("remote", "add", "origin", str(self.remote))

    @contextlib.contextmanager
    def local_remote_contract(self):
        identity = (str(self.remote), str(self.remote))
        with mock.patch.object(
            creator, "verify_remote_identity", return_value=identity
        ), mock.patch.object(
            creator,
            "remote_push_environment",
            side_effect=lambda _url: contextlib.nullcontext({}),
        ):
            yield

    def publish(self) -> str:
        return creator.publish_remote_ref(
            self.repository,
            "origin",
            "FerrumVir/arc-chain",
            self.ref_name,
            self.handoff,
        )

    def remote_target(self) -> str | None:
        completed = subprocess.run(
            ["git", "--git-dir", str(self.remote), "rev-parse", "--verify", self.ref_name],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        return completed.stdout.strip() if completed.returncode == 0 else None

    def test_fresh_remote_create_then_exact_remote_reuse(self) -> None:
        with self.local_remote_contract():
            self.assertEqual(self.publish(), "created")
            self.assertEqual(self.remote_target(), self.handoff)
            self.assertEqual(self.publish(), "reused")
            self.assertEqual(self.remote_target(), self.handoff)

    def test_existing_remote_mismatch_is_an_incident_and_never_replaced(self) -> None:
        alternate = self.commit(self.git("rev-parse", "HEAD^{tree}"), self.main_commit)
        self.git("push", "origin", f"{alternate}:{self.ref_name}")
        with self.local_remote_contract(), self.assertRaises(
            creator.HandoffCommitError
        ) as raised:
            self.publish()
        self.assertIn("different commit", str(raised.exception))
        self.assertEqual(self.remote_target(), alternate)

    def test_unavailable_initial_probe_is_not_mistaken_for_absence(self) -> None:
        unavailable = self.root / "remote-unavailable.git"
        self.git("remote", "set-url", "origin", str(unavailable))
        identity = (str(unavailable), str(unavailable))
        with mock.patch.object(
            creator, "verify_remote_identity", return_value=identity
        ), self.assertRaises(creator.RemoteProbeError) as raised:
            self.publish()
        self.assertIn("absence was not proven", str(raised.exception))

    def test_created_then_transport_error_is_reprobed_as_success(self) -> None:
        original_run_git = creator.run_git

        def lose_push_response(repository, arguments, **kwargs):
            if arguments and arguments[0] == "push":
                original_run_git(repository, arguments, **kwargs)
                raise creator.GitCommandError("simulated lost push response")
            return original_run_git(repository, arguments, **kwargs)

        with self.local_remote_contract(), mock.patch.object(
            creator, "run_git", side_effect=lose_push_response
        ):
            self.assertEqual(self.publish(), "created")
        self.assertEqual(self.remote_target(), self.handoff)

    def test_absent_after_push_error_fails_safe_and_retry_succeeds(self) -> None:
        original_run_git = creator.run_git

        def fail_before_push(repository, arguments, **kwargs):
            if arguments and arguments[0] == "push":
                raise creator.GitCommandError("simulated authentication failure")
            return original_run_git(repository, arguments, **kwargs)

        with self.local_remote_contract(), mock.patch.object(
            creator, "run_git", side_effect=fail_before_push
        ), self.assertRaises(creator.HandoffCommitError) as raised:
            self.publish()
        self.assertIn("safe to retry", str(raised.exception))
        self.assertIsNone(self.remote_target())
        with self.local_remote_contract():
            self.assertEqual(self.publish(), "created")
        self.assertEqual(self.remote_target(), self.handoff)

    def test_different_ref_appearing_during_push_is_an_incident(self) -> None:
        alternate = self.commit(self.git("rev-parse", "HEAD^{tree}"), self.main_commit)
        original_run_git = creator.run_git

        def race_different_ref(repository, arguments, **kwargs):
            if arguments and arguments[0] == "push":
                self.git("push", "origin", f"{alternate}:{self.ref_name}")
                return ""
            return original_run_git(repository, arguments, **kwargs)

        with self.local_remote_contract(), mock.patch.object(
            creator, "run_git", side_effect=race_different_ref
        ), self.assertRaises(creator.HandoffCommitError) as raised:
            self.publish()
        self.assertIn("treat this as an incident", str(raised.exception))
        self.assertEqual(self.remote_target(), alternate)

    def test_remote_identity_requires_one_exact_credential_free_github_url(self) -> None:
        with self.assertRaises(creator.HandoffCommitError):
            creator.verify_remote_identity(
                self.repository, "origin", "FerrumVir/arc-chain"
            )
        exact = "https://github.com/FerrumVir/arc-chain.git"
        self.git("remote", "set-url", "origin", exact)
        self.assertEqual(
            creator.verify_remote_identity(
                self.repository, "origin", "FerrumVir/arc-chain"
            ),
            (exact, exact),
        )
        credentialed = "https://secret-token@github.com/FerrumVir/arc-chain.git"
        self.git("remote", "set-url", "origin", credentialed)
        with self.assertRaises(creator.HandoffCommitError) as raised:
            creator.verify_remote_identity(
                self.repository, "origin", "FerrumVir/arc-chain"
            )
        self.assertNotIn("secret-token", str(raised.exception))

    def test_remote_name_and_https_askpass_are_bounded_and_token_free_on_disk(
        self,
    ) -> None:
        self.git(
            "remote", "set-url", "origin", "https://github.com/FerrumVir/arc-chain.git"
        )
        with self.assertRaises(creator.HandoffCommitError):
            creator.verify_remote_identity(
                self.repository, "--upload-pack=owned", "FerrumVir/arc-chain"
            )
        secret = "ghp_SENTINEL_ASKPASS_TOKEN_1234567890"
        with mock.patch.dict(os.environ, {"GH_TOKEN": secret}, clear=False):
            with creator.remote_push_environment(
                "https://github.com/FerrumVir/arc-chain.git"
            ) as environment:
                askpass = Path(environment["GIT_ASKPASS"])
                self.assertTrue(askpass.is_file())
                self.assertEqual(askpass.stat().st_mode & 0o777, 0o700)
                self.assertNotIn(secret.encode(), askpass.read_bytes())
                self.assertEqual(environment["ARC_HANDOFF_PUSH_TOKEN"], secret)
                self.assertNotIn("GH_TOKEN", environment)
                self.assertNotIn("GITHUB_TOKEN", environment)
                self.assertIn("credential.helper", environment.values())
                self.assertIn("http.extraHeader", environment.values())
                askpass_path = askpass
            self.assertFalse(askpass_path.exists())

        with mock.patch.dict(os.environ, {"GH_TOKEN": "bad token"}, clear=False):
            with self.assertRaises(creator.HandoffCommitError):
                with creator.remote_push_environment(
                    "https://github.com/FerrumVir/arc-chain.git"
                ):
                    pass


class CutoverHandoffIsolationTests(unittest.TestCase):
    def test_pythonpath_sitecustomize_and_tokens_do_not_reach_derived_tools(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            malicious = root / "malicious"
            malicious.mkdir()
            sitecustomize_marker = root / "sitecustomize-ran"
            (malicious / "sitecustomize.py").write_text(
                "from pathlib import Path\n"
                f"Path({str(sitecustomize_marker)!r}).write_text('executed')\n",
                encoding="utf-8",
            )
            observed = root / "observed.json"
            probe = root / "probe.py"
            probe.write_text(
                "import json, os, sys\n"
                "from pathlib import Path\n"
                "Path(sys.argv[1]).write_text(json.dumps({"
                "'argv': sys.argv, 'environment': dict(os.environ)"
                "}, sort_keys=True))\n",
                encoding="utf-8",
            )
            safe_home = root / "safe-home"
            safe_home.mkdir(mode=0o700)
            secret = "ghp_SENTINEL_TOKEN_MUST_NOT_ESCAPE_123456789"
            hostile_environment = {
                "PYTHONPATH": str(malicious),
                "PYTHONHOME": str(malicious),
                "GH_TOKEN": secret,
                "GITHUB_TOKEN": secret,
                "SSH_AUTH_SOCK": str(root / "agent.sock"),
                "GOOGLE_APPLICATION_CREDENTIALS": str(root / "cloud.json"),
            }
            with mock.patch.dict(os.environ, hostile_environment, clear=False):
                creator.run_isolated_python(
                    probe,
                    [str(observed)],
                    "hostile isolation probe",
                    safe_home=safe_home,
                )
            self.assertFalse(sitecustomize_marker.exists())
            payload = json.loads(observed.read_text(encoding="utf-8"))
            expected_environment = {
                "HOME": str(safe_home),
                "PATH": "/usr/bin:/bin",
                "LANG": "C",
                "LC_ALL": "C",
                "TZ": "UTC",
            }
            for name, value in expected_environment.items():
                self.assertEqual(payload["environment"].get(name), value)
            self.assertLessEqual(
                set(payload["environment"]) - set(expected_environment),
                {"__CF_USER_TEXT_ENCODING"},
            )
            serialized = json.dumps(payload, sort_keys=True)
            self.assertNotIn(secret, serialized)
            self.assertNotIn("PYTHONPATH", serialized)
            self.assertEqual(payload["argv"], [str(probe), str(observed)])

    def test_isolated_tool_failure_diagnostic_cannot_echo_parent_token(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            safe_home = root / "safe-home"
            safe_home.mkdir(mode=0o700)
            probe = root / "fail.py"
            probe.write_text(
                "import os, sys\nprint(dict(os.environ), file=sys.stderr)\nraise SystemExit(7)\n",
                encoding="utf-8",
            )
            secret = "ghp_SENTINEL_TOKEN_MUST_NOT_ESCAPE_987654321"
            with mock.patch.dict(os.environ, {"GH_TOKEN": secret}, clear=False), self.assertRaises(
                creator.HandoffCommitError
            ) as raised:
                creator.run_isolated_python(
                    probe, [], "failing isolation probe", safe_home=safe_home
                )
            self.assertNotIn(secret, str(raised.exception))


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
