#!/usr/bin/env python3
"""Hermetic contract tests for the macOS pre-tag community canary helper."""

from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import plistlib
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from contextlib import contextmanager
from pathlib import Path
from types import SimpleNamespace
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[2]
HELPER = REPO_ROOT / "scripts/release/macos-community-canary.py"
SPEC = importlib.util.spec_from_file_location("arc_macos_canary", HELPER)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot load {HELPER}")
canary = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = canary
SPEC.loader.exec_module(canary)


def completed(argv: tuple[str, ...], returncode: int = 0, stdout: str = "", stderr: str = ""):
    return subprocess.CompletedProcess(list(argv), returncode, stdout, stderr)


class FakePlatform(canary.PlatformCommands):
    """Model launchd/ps/lsof and the exact preflight CLI without real processes."""

    def __init__(self) -> None:
        self.uid = os.getuid()
        self.loaded = False
        self.alive = False
        self.disabled = False
        self.exit_on_term = True
        self.pid = 4242
        self.expected_command = ""
        self.expected_executable: Path | None = None
        self.reported_command: str | None = None
        self.listener_names = [canary.RPC]
        self.udp_names = ["127.0.0.1:54321"]
        self.start_on_kick = True
        self.commands: list[tuple[str, ...]] = []
        self.sleep_calls = 0
        self.public_address = "b" * 64

    def prove_runner_tools(self) -> None:
        return

    def run(self, argv, *, check: bool = True):
        args = tuple(str(value) for value in argv)
        self.commands.append(args)
        result = self._dispatch(args)
        if check and result.returncode != 0:
            raise canary.CanaryError(
                f"mock platform command failed: {' '.join(args)}: {result.stderr}"
            )
        return result

    def _dispatch(self, args: tuple[str, ...]):
        if args == ("id", "-u"):
            return completed(args, stdout=f"{self.uid}\n")
        if args == ("uname", "-s"):
            return completed(args, stdout="Darwin\n")
        if args == ("uname", "-m"):
            return completed(args, stdout="arm64\n")
        if args[:2] == ("launchctl", "print"):
            target = args[2]
            if target.endswith(f"/{canary.LABEL}"):
                if not self.loaded:
                    return completed(args, 113, stderr="service not found")
                pid = f"\n    pid = {self.pid}\n" if self.alive else "\n"
                return completed(args, stdout=f"service = {{{pid}}}\n")
            return completed(args, stdout="domain = { active = true }\n")
        if args[:2] == ("launchctl", "enable"):
            self.disabled = False
            return completed(args)
        if args[:2] == ("launchctl", "disable"):
            self.disabled = True
            return completed(args)
        if args[:2] == ("launchctl", "bootstrap"):
            if self.loaded:
                return completed(args, 5, stderr="already loaded")
            self.loaded = True
            self.alive = False
            return completed(args)
        if args[:2] == ("launchctl", "kickstart"):
            if not self.loaded:
                return completed(args, 3, stderr="not loaded")
            self.alive = self.start_on_kick
            return completed(args)
        if args[:2] == ("launchctl", "kill"):
            if args[2] != "SIGTERM" or not self.loaded or not self.alive:
                return completed(args, 3, stderr="bad signal target")
            if self.exit_on_term:
                self.alive = False
            return completed(args)
        if args[:2] == ("launchctl", "bootout"):
            if self.alive:
                return completed(args, 5, stderr="mock refuses bootout of live process")
            self.loaded = False
            return completed(args)
        if args[:2] == ("ps", "-p") and args[3:] == ("-o", "pid="):
            if self.alive and int(args[2]) == self.pid:
                return completed(args, stdout=f"{self.pid}\n")
            return completed(args, 1)
        if args[:3] == ("ps", "-ww", "-p") and args[4:] == ("-o", "command="):
            if self.alive and int(args[3]) == self.pid:
                command = self.reported_command or self.expected_command
                return completed(args, stdout=f"{command}\n")
            return completed(args, 1)
        if args[:3] == ("lsof", "-nP", "-a"):
            if self.alive:
                selected = self.udp_names if "-iUDP" in args else self.listener_names
                names = "".join(f"n{value}\n" for value in selected)
                return completed(args, stdout=f"p{self.pid}\n{names}")
            return completed(args, 1, stderr="no listeners")
        if args[:2] == ("lsof", "-a"):
            if self.alive and self.expected_executable is not None:
                return completed(
                    args,
                    stdout=f"p{self.pid}\nftxt\nn{self.expected_executable}\n",
                )
            return completed(args, 1, stderr="no process")
        if len(args) >= 2 and args[0].endswith("arc-cli-macos-arm64"):
            if args[1:3] == ("keygen", "--verify-keyfile"):
                return completed(args, stdout=f"{self.public_address}\n")
            if args[1:4] == ("keygen", "--scheme", "ed25519"):
                if args[4] != "--output":
                    return completed(args, 2, stderr="bad keygen argv")
                output = Path(args[5])
                output.write_text(
                    '{"scheme":"ed25519","test_material":"never-log-this"}\n',
                    encoding="utf-8",
                )
                output.chmod(0o600)
                return completed(args, stdout="generated public test identity\n")
        return completed(args, 127, stderr="unexpected mock command")

    def sleep(self, seconds: float) -> None:
        self.sleep_calls += 1


class CanaryFixture:
    COMMIT = "a" * 40
    RUN_ID = 123456
    RUN_ATTEMPT = 2
    ARTIFACT_ID = 987654
    ARTIFACT_DIGEST = "sha256:" + "d" * 64
    ARCHIVE_SHA256 = "c" * 64
    ARTIFACT_SIZE = 123456

    def __init__(self, root: Path) -> None:
        self.root = root
        self.home = root / "home"
        self.home.mkdir(mode=0o700)
        self.candidate = root / "candidate"
        self.candidate.mkdir(mode=0o700)
        self.node = self.candidate / "arc-node-macos-arm64"
        self.cli = self.candidate / "arc-cli-macos-arm64"
        self.genesis = self.candidate / "genesis.toml"
        self.model = root / "canonical.gguf"
        self.actions_zip = root / "artifact.zip"
        self.node.write_bytes(b"exact preflight node fixture\n")
        self.cli.write_bytes(b"exact preflight cli fixture\n")
        self.genesis.write_text(self._genesis(), encoding="utf-8")
        self.model.write_bytes(b"exact canonical model fixture\n")
        self.actions_zip.write_bytes(b"shared verifier fixture raw Actions ZIP\n")
        self.actions_zip.chmod(0o400)
        self.ARTIFACT_DIGEST = "sha256:" + hashlib.sha256(
            self.actions_zip.read_bytes()
        ).hexdigest()
        self.ARTIFACT_SIZE = self.actions_zip.stat().st_size
        files = {
            path.name: hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (self.node, self.cli, self.genesis)
        }
        metadata = {
            "schema": "arc.pretag.artifact.v1",
            "kind": "headless",
            "repository": canary.REPOSITORY,
            "commit": self.COMMIT,
            "platform": canary.PLATFORM,
            "rust_target": canary.RUST_TARGET,
            "version": "0.8.0",
            "workflow_run_id": self.RUN_ID,
            "workflow_run_attempt": self.RUN_ATTEMPT,
            "files": files,
        }
        metadata_path = self.candidate / "BUILD-METADATA.json"
        metadata_path.write_text(
            json.dumps(metadata, sort_keys=True, indent=2) + "\n", encoding="utf-8"
        )
        self.provenance = {
            "schema": canary.artifact_provenance.PROVENANCE_SCHEMA,
            "live": {
                "repository": canary.REPOSITORY,
                "protected_branch": "main",
                "commit": self.COMMIT,
                "workflow_id": 42,
                "workflow_path": canary.artifact_provenance.WORKFLOW_PATH,
                "run_id": self.RUN_ID,
                "run_attempt": self.RUN_ATTEMPT,
                "artifact_id": self.ARTIFACT_ID,
                "artifact_name": (
                    f"arc-pretag-headless-{canary.PLATFORM}-{self.COMMIT}-"
                    f"{self.RUN_ID}-{self.RUN_ATTEMPT}-{self.ARCHIVE_SHA256}"
                ),
                "artifact_digest": self.ARTIFACT_DIGEST,
                "artifact_size_in_bytes": self.ARTIFACT_SIZE,
                "api_verified_at_unix": 1_800_000_000,
            },
            "api": {
                "origin": canary.artifact_provenance.API_ORIGIN,
                "anonymous": True,
                "redirects_followed": False,
                "max_age_seconds": 300,
                "curl_sha256": "e" * 64,
                "ca_bundle_sha256": "f" * 64,
                "responses": [
                    {
                        "label": label,
                        "body_sha256": hashlib.sha256(label.encode()).hexdigest(),
                        "response_unix": 1_800_000_000 + index,
                        "request_id": f"ABCD:{index:04X}:1234:5678",
                        "cache_control": "public, max-age=60",
                        "age": 0,
                    }
                    for index, label in enumerate(
                        ("workflow", "run", "artifact", "protected_main")
                    )
                ],
            },
            "artifact": {
                "kind": "headless",
                "platform": canary.PLATFORM,
                "version": "0.8.0",
                "raw_actions_zip_sha256": hashlib.sha256(
                    self.actions_zip.read_bytes()
                ).hexdigest(),
                "raw_actions_zip_size": self.actions_zip.stat().st_size,
                "archive_sha256": self.ARCHIVE_SHA256,
                "build_metadata_sha256": hashlib.sha256(
                    metadata_path.read_bytes()
                ).hexdigest(),
                "files": files,
            },
        }
        self.provenance_bytes = canary.canonical_json(self.provenance)
        self.recheck_count = 0
        self.proof_count = 0
        self.initial_proof_hashes: list[str] = []
        self.node.chmod(0o500)
        self.cli.chmod(0o500)
        self.genesis.chmod(0o400)
        metadata_path.chmod(0o400)
        self.genesis_sha256 = files["genesis.toml"]
        self.model_sha256 = hashlib.sha256(self.model.read_bytes()).hexdigest()

    @contextmanager
    def live_proof(self, **_kwargs):
        self.proof_count += 1
        initial_provenance = json.loads(self.provenance_bytes)
        initial_offset = (self.proof_count - 1) * 100
        initial_prefix = f"{0xA000 + self.proof_count:04X}"
        for response in initial_provenance["api"]["responses"]:
            response["response_unix"] += initial_offset
            response["request_id"] = response["request_id"].replace(
                "ABCD", initial_prefix
            )
        initial_provenance["live"]["api_verified_at_unix"] += initial_offset
        initial_bytes = canary.canonical_json(initial_provenance)
        self.initial_proof_hashes.append(hashlib.sha256(initial_bytes).hexdigest())

        final_provenance = json.loads(initial_bytes)
        final_provenance["live"]["api_verified_at_unix"] += 10
        final_prefix = f"{0xD000 + self.proof_count:04X}"
        for response in final_provenance["api"]["responses"]:
            response["response_unix"] += 10
            response["request_id"] = response["request_id"].replace(
                initial_prefix, final_prefix
            )
        final_bytes = canary.canonical_json(final_provenance)
        proof = SimpleNamespace(
            payload_root=self.candidate,
            build_metadata_path=self.candidate / "BUILD-METADATA.json",
            payloads={
                "arc-node-macos-arm64": self.node,
                "arc-cli-macos-arm64": self.cli,
                "genesis.toml": self.genesis,
            },
            provenance=initial_provenance,
            provenance_bytes=initial_bytes,
        )
        def recheck():
            self.recheck_count += 1
            return SimpleNamespace(
                value=final_provenance,
                canonical_bytes=final_bytes,
                path=None,
            )

        proof.recheck = recheck
        yield proof

    @staticmethod
    def _genesis() -> str:
        lines = [
            "[chain]",
            'name = "arc-testnet"',
            'chain_id = "0x415243"',
            "validator_set_complete = true",
            "community_rewards_v1_activation_height = 137146",
            "",
        ]
        for index in range(1, 7):
            lines.extend(
                (
                    "[[accounts]]",
                    f'address = "{index:064x}"',
                    "balance = 0",
                    "",
                )
            )
        for index in range(1, 7):
            lines.extend(
                (
                    "[[validators]]",
                    f'address = "{index:064x}"',
                    "stake = 6666667",
                    "",
                )
            )
        return "\n".join(lines)


class MacosCommunityCanaryTests(unittest.TestCase):
    def setUp(self) -> None:
        # Linux normally places TemporaryDirectory under world-writable /tmp,
        # which the production canary deliberately rejects for operator inputs.
        # Keep the hermetic fixture under the canonical operator home instead;
        # TemporaryDirectory still owns and removes the isolated 0700 leaf.
        fixture_parent = Path.home().resolve(strict=True)
        self.temporary = tempfile.TemporaryDirectory(
            prefix=".arc-canary-contract-", dir=fixture_parent
        )
        self.fixture = CanaryFixture(Path(self.temporary.name).resolve())
        self.patches = (
            mock.patch.object(
                canary, "CANONICAL_GENESIS_SHA256", self.fixture.genesis_sha256
            ),
            mock.patch.object(canary, "CANONICAL_MODEL_SHA256", self.fixture.model_sha256),
            mock.patch.object(
                canary,
                "CANONICAL_MODEL_SIZE_BYTES",
                self.fixture.model.stat().st_size,
            ),
        )
        for patcher in self.patches:
            patcher.start()
        self.live_proof_patcher = mock.patch.object(
            canary.artifact_provenance,
            "pretag_actions_proof",
            side_effect=self.fixture.live_proof,
        )
        self.live_proof_mock = self.live_proof_patcher.start()
        self.fake = FakePlatform()
        self.controller = canary.CanaryController(
            self.fixture.home / "canary",
            self.fake,
            home=self.fixture.home,
            stop_budget_seconds=2,
            start_proof_seconds=2,
        )

    def tearDown(self) -> None:
        self.live_proof_patcher.stop()
        for patcher in reversed(self.patches):
            patcher.stop()
        self.temporary.cleanup()

    def install(self) -> None:
        self.controller.install(*self.live_args())
        config = self.controller._load_config()
        self.fake.expected_command = " ".join(config["runtime"]["argv"])
        self.fake.expected_executable = self.controller.paths.node

    def live_args(
        self,
        *,
        model: Path | None = None,
        commit: str | None = None,
        artifact_id: int | None = None,
    ) -> tuple:
        return (
            self.fixture.actions_zip,
            self.fixture.model if model is None else model,
            self.fixture.COMMIT if commit is None else commit,
            self.fixture.RUN_ID,
            self.fixture.RUN_ATTEMPT,
            self.fixture.ARTIFACT_ID if artifact_id is None else artifact_id,
            Path("/usr/bin/curl"),
            "e" * 64,
            Path("/private/etc/ssl/cert.pem"),
            "f" * 64,
        )

    def test_full_lifecycle_is_exact_and_cleanup_preserves_operator_state(self) -> None:
        self.install()
        self.assertEqual(1, self.fixture.recheck_count)
        paths = self.controller.paths
        config = self.controller._load_config()
        argv = config["runtime"]["argv"]
        self.assertEqual(6, argv.count("--community-rpc-url"))
        self.assertEqual(list(canary.COMMUNITY_RPC_URLS), config["runtime"]["community_rpc_urls"])
        self.assertEqual("127.0.0.1:19944", config["runtime"]["rpc"])
        self.assertEqual(0, config["runtime"]["stake"])
        self.assertEqual([], config["runtime"]["p2p_peers"])
        self.assertIn("--full-integer-worker", argv)
        self.assertNotIn("--peers", argv)
        self.assertNotIn("--seeds-file", argv)
        self.assertIn(str(paths.key), argv)
        self.assertNotIn("never-log-this", paths.runner.read_text(encoding="utf-8"))
        self.assertNotIn("never-log-this", paths.config.read_text(encoding="utf-8"))
        self.assertNotIn("never-log-this", paths.launch_agent.read_text(encoding="utf-8"))
        self.assertFalse(
            any(
                "never-log-this" in path.read_text(encoding="utf-8")
                for path in paths.evidence_dir.glob("*.json")
            )
        )

        data_marker = paths.data_dir / "preserve.data"
        data_marker.write_text("persistent chain state\n", encoding="utf-8")
        self.controller.start()
        self.assertEqual(0, self.controller.status())
        self.controller.stop()
        self.assertFalse(self.fake.loaded)
        self.controller.cleanup()

        self.assertFalse(paths.launch_agent.exists())
        self.assertTrue(paths.key.exists())
        self.assertTrue(data_marker.exists())
        self.assertTrue(self.fixture.model.exists())
        self.assertTrue(paths.model.exists())
        self.assertEqual(self.fixture.model.read_bytes(), paths.model.read_bytes())
        self.assertTrue(any(paths.evidence_dir.glob("*.json")))
        signals = [args for args in self.fake.commands if args[:2] == ("launchctl", "kill")]
        self.assertEqual(1, len(signals))
        self.assertEqual("SIGTERM", signals[0][2])
        self.assertFalse(any("SIGKILL" in part for args in self.fake.commands for part in args))

    def test_argv_mismatch_fails_before_any_signal_or_disable(self) -> None:
        self.install()
        self.controller.start()
        command_count = len(self.fake.commands)
        self.fake.reported_command = "/tmp/not-the-canary --stake 0"
        with self.assertRaisesRegex(canary.CanaryError, "argv differs"):
            self.controller.stop()
        new_commands = self.fake.commands[command_count:]
        self.assertFalse(any(args[:2] == ("launchctl", "kill") for args in new_commands))
        self.assertFalse(any(args[:2] == ("launchctl", "disable") for args in new_commands))
        self.assertTrue(self.fake.alive)

    def test_external_or_additional_listener_fails_process_proof(self) -> None:
        self.install()
        self.controller.start()
        self.fake.listener_names = [canary.RPC, "*:19945"]
        with self.assertRaisesRegex(canary.CanaryError, "sole loopback RPC"):
            self.controller.status()
        self.fake.listener_names = ["*:19944"]
        with self.assertRaisesRegex(canary.CanaryError, "sole loopback RPC"):
            self.controller.status()

    def test_udp_socket_proof_rejects_wildcard_multiple_and_malformed(self) -> None:
        self.install()
        self.controller.start()
        for udp_names in (
            ["*:54321"],
            ["0.0.0.0:54321"],
            ["127.0.0.1:54321", "127.0.0.1:54322"],
            ["127.0.0.1:not-a-port"],
            ["127.0.0.1:0"],
            ["127.0.0.1:65536"],
            [],
        ):
            with self.subTest(udp_names=udp_names):
                self.fake.udp_names = udp_names
                with self.assertRaisesRegex(canary.CanaryError, "loopback QUIC"):
                    self.controller.status()

    def test_start_quarantines_exact_process_with_external_listener(self) -> None:
        self.install()
        self.fake.listener_names = ["*:19944"]
        command_count = len(self.fake.commands)
        with self.assertRaisesRegex(canary.CanaryError, "sole loopback RPC"):
            self.controller.start()
        new_commands = self.fake.commands[command_count:]
        self.assertTrue(self.fake.disabled)
        self.assertFalse(self.fake.alive)
        self.assertFalse(self.fake.loaded)
        self.assertIn(
            ("launchctl", "kill", "SIGTERM", self.controller._service_target()),
            new_commands,
        )
        self.assertFalse(any("SIGKILL" in part for args in new_commands for part in args))

    def test_start_timeout_disables_but_never_boots_out_no_pid_job(self) -> None:
        self.install()
        self.fake.start_on_kick = False
        command_count = len(self.fake.commands)
        with self.assertRaisesRegex(canary.CanaryError, "racy no-PID observation"):
            self.controller.start()
        new_commands = self.fake.commands[command_count:]
        self.assertTrue(self.fake.disabled)
        self.assertTrue(self.fake.loaded)
        self.assertFalse(self.fake.alive)
        self.assertFalse(
            any(args[:2] == ("launchctl", "bootout") for args in new_commands)
        )
        self.assertFalse(any("SIGKILL" in part for args in new_commands for part in args))

    def test_stop_no_pid_disables_but_never_signals_or_boots_out_job(self) -> None:
        self.install()
        self.fake.loaded = True
        self.fake.alive = False
        command_count = len(self.fake.commands)
        with self.assertRaisesRegex(canary.CanaryError, "loaded canary has no provable PID"):
            self.controller.stop()
        new_commands = self.fake.commands[command_count:]
        self.assertTrue(self.fake.disabled)
        self.assertTrue(self.fake.loaded)
        self.assertFalse(self.fake.alive)
        self.assertFalse(
            any(args[:2] == ("launchctl", "bootout") for args in new_commands)
        )
        self.assertFalse(
            any(args[:2] == ("launchctl", "kill") for args in new_commands)
        )
        self.assertFalse(any("SIGKILL" in part for args in new_commands for part in args))

    def test_graceful_timeout_never_escalates_or_boots_out_live_process(self) -> None:
        self.install()
        self.controller.start()
        self.fake.exit_on_term = False
        with self.assertRaisesRegex(canary.CanaryError, "4420-second graceful budget"):
            self.controller.stop()
        self.assertTrue(self.fake.loaded)
        self.assertTrue(self.fake.alive)
        self.assertTrue(self.fake.disabled)
        self.assertFalse(
            any(args[:2] == ("launchctl", "bootout") for args in self.fake.commands)
        )
        self.assertFalse(any("SIGKILL" in part for args in self.fake.commands for part in args))

    def test_install_is_create_only_and_refuses_tampered_plist(self) -> None:
        self.install()
        path = self.controller.paths.launch_agent
        path.write_bytes(b"tampered plist\n")
        path.chmod(0o600)
        with self.assertRaisesRegex(canary.CanaryError, "refusing to replace mismatched"):
            self.controller.install(*self.live_args())
        self.assertEqual(b"tampered plist\n", path.read_bytes())

    def test_idempotent_install_preserves_identity_and_existing_log(self) -> None:
        self.install()
        paths = self.controller.paths
        key_before = paths.key.read_bytes()
        initial_receipt = paths.provenance_receipt.read_bytes()
        final_receipt = paths.provenance_recheck.read_bytes()
        config_before = paths.config.read_bytes()
        paths.log.write_text("existing non-secret runtime log\n", encoding="utf-8")
        paths.log.chmod(0o600)
        self.controller.install(*self.live_args())
        self.assertEqual(2, self.fixture.proof_count)
        self.assertEqual(2, len(set(self.fixture.initial_proof_hashes)))
        self.assertEqual(2, self.fixture.recheck_count)
        self.assertEqual(key_before, paths.key.read_bytes())
        self.assertEqual(initial_receipt, paths.provenance_receipt.read_bytes())
        self.assertEqual(final_receipt, paths.provenance_recheck.read_bytes())
        self.assertEqual(config_before, paths.config.read_bytes())
        self.assertEqual(
            "existing non-secret runtime log\n", paths.log.read_text(encoding="utf-8")
        )

    def test_retry_preserves_initial_receipt_after_interrupted_install(self) -> None:
        with mock.patch.object(
            self.controller,
            "_ensure_key",
            side_effect=canary.CanaryError("injected interruption after initial receipt"),
        ):
            with self.assertRaisesRegex(canary.CanaryError, "injected interruption"):
                self.controller.install(*self.live_args())
        paths = self.controller.paths
        retained_initial = paths.provenance_receipt.read_bytes()
        self.assertFalse(paths.provenance_recheck.exists())

        self.controller.install(*self.live_args())
        self.assertEqual(2, self.fixture.proof_count)
        self.assertEqual(2, len(set(self.fixture.initial_proof_hashes)))
        self.assertEqual(retained_initial, paths.provenance_receipt.read_bytes())
        self.controller._validate_installation(require_launch_agent=True)

    def test_retry_preserves_both_receipts_after_recheck_publication(self) -> None:
        original_publish = canary.publish_bytes_create_only
        injected = False

        def interrupt_after_recheck(path, content, mode, uid):
            nonlocal injected
            original_publish(path, content, mode, uid)
            if path == self.controller.paths.provenance_recheck and not injected:
                injected = True
                raise canary.CanaryError("injected interruption after final receipt")

        with mock.patch.object(
            canary, "publish_bytes_create_only", side_effect=interrupt_after_recheck
        ):
            with self.assertRaisesRegex(canary.CanaryError, "injected interruption"):
                self.controller.install(*self.live_args())
        paths = self.controller.paths
        retained_initial = paths.provenance_receipt.read_bytes()
        retained_final = paths.provenance_recheck.read_bytes()

        self.controller.install(*self.live_args())
        self.assertEqual(2, self.fixture.proof_count)
        self.assertEqual(2, len(set(self.fixture.initial_proof_hashes)))
        self.assertEqual(retained_initial, paths.provenance_receipt.read_bytes())
        self.assertEqual(retained_final, paths.provenance_recheck.read_bytes())
        self.controller._validate_installation(require_launch_agent=True)

    def test_live_pair_rejects_reordered_or_reused_api_requests(self) -> None:
        with self.fixture.live_proof() as proof:
            initial = proof.provenance
            final = proof.recheck().value
        with self.assertRaisesRegex(canary.CanaryError, "predates the initial"):
            canary.require_ordered_fresh_live_provenance_pair(final, initial)

        reused = json.loads(canary.canonical_json(final))
        for current, prior in zip(
            reused["api"]["responses"], initial["api"]["responses"]
        ):
            current["request_id"] = prior["request_id"]
        with self.assertRaisesRegex(canary.CanaryError, "fresh API requests"):
            canary.require_ordered_fresh_live_provenance_pair(initial, reused)

        same_second = json.loads(canary.canonical_json(final))
        boundary = max(row["response_unix"] for row in initial["api"]["responses"])
        for response in same_second["api"]["responses"]:
            response["response_unix"] = boundary
        same_second["live"]["api_verified_at_unix"] = boundary
        canary.require_ordered_fresh_live_provenance_pair(initial, same_second)

    def test_wrong_model_and_wrong_preflight_pin_fail_before_install(self) -> None:
        self.fixture.model.write_bytes(b"wrong model\n")
        with self.assertRaisesRegex(canary.CanaryError, "size mismatch"):
            self.controller.plan(*self.live_args())
        self.assertFalse(self.controller.paths.root.exists())

        self.fixture.model.write_bytes(b"exact canonical model fixture\n")
        with self.assertRaisesRegex(canary.CanaryError, "commit differs"):
            self.controller.plan(*self.live_args(commit="c" * 40))
        with self.assertRaisesRegex(canary.CanaryError, "live protected preflight proof differs"):
            self.controller.plan(*self.live_args(artifact_id=self.fixture.ARTIFACT_ID + 1))

    def test_external_model_is_copied_private_and_runtime_rejects_managed_mutation(self) -> None:
        self.install()
        paths = self.controller.paths
        config = self.controller._load_config()
        self.assertEqual(str(paths.model), config["model"]["path"])
        self.assertIn(str(paths.model), config["runtime"]["argv"])
        self.assertNotIn(str(self.fixture.model), config["runtime"]["argv"])
        self.assertEqual(0o400, stat.S_IMODE(paths.model.stat().st_mode))
        self.assertEqual(1, paths.model.stat().st_nlink)

        # The external operator input is no longer a runtime dependency.
        self.fixture.model.chmod(0o600)
        self.fixture.model.write_bytes(b"external source changed after materialization\n")
        self.controller.start()
        self.controller.stop()

        # Managed bytes are private/create-only and are re-proved before use.
        paths.model.chmod(0o600)
        paths.model.write_bytes(b"managed model mutation\n")
        paths.model.chmod(0o400)
        with self.assertRaisesRegex(canary.CanaryError, "managed canonical GGUF failed"):
            self.controller.start()

    def test_model_symlink_hardlink_and_writable_ancestry_are_rejected(self) -> None:
        linked = self.fixture.root / "linked.gguf"
        os.link(self.fixture.model, linked)
        with self.assertRaisesRegex(canary.CanaryError, "exactly one hard link"):
            self.controller.plan(*self.live_args())
        linked.unlink()

        symlink = self.fixture.root / "model-link.gguf"
        symlink.symlink_to(self.fixture.model)
        with self.assertRaisesRegex(canary.CanaryError, "non-symlink regular file"):
            self.controller.plan(*self.live_args(model=symlink))

        unsafe = self.fixture.root / "unsafe-model-parent"
        unsafe.mkdir(mode=0o700)
        unsafe_model = unsafe / "canonical.gguf"
        unsafe_model.write_bytes(self.fixture.model.read_bytes())
        unsafe.chmod(0o777)
        with self.assertRaisesRegex(canary.CanaryError, "non-group/world-writable"):
            self.controller.plan(*self.live_args(model=unsafe_model))

    def test_zero_progress_publication_fails_and_removes_staging(self) -> None:
        destination = self.fixture.home / "zero-progress.json"
        with mock.patch.object(canary.os, "write", return_value=0):
            with self.assertRaisesRegex(canary.CanaryError, "no write progress"):
                canary.publish_bytes_create_only(
                    destination, b"must not truncate\n", 0o600, os.getuid()
                )
        self.assertFalse(destination.exists())
        self.assertEqual([], list(destination.parent.glob(f".{destination.name}.new.*")))

    def test_platform_commands_ignore_path_and_loader_injection(self) -> None:
        platform = canary.PlatformCommands()
        fake_result = completed(("/usr/bin/id", "-u"), stdout=f"{os.getuid()}\n")
        with mock.patch.dict(
            os.environ,
            {
                "PATH": "/attacker/bin",
                "DYLD_INSERT_LIBRARIES": "/attacker/lib.dylib",
                "LD_PRELOAD": "/attacker/lib.so",
            },
            clear=False,
        ), mock.patch.object(canary.subprocess, "run", return_value=fake_result) as invoked:
            platform.run(("id", "-u"))
        command = invoked.call_args.args[0]
        environment = invoked.call_args.kwargs["env"]
        self.assertEqual(["/usr/bin/id", "-u"], command)
        self.assertEqual("/usr/bin:/bin:/usr/sbin:/sbin", environment["PATH"])
        self.assertNotIn("DYLD_INSERT_LIBRARIES", environment)
        self.assertNotIn("LD_PRELOAD", environment)
        with self.assertRaisesRegex(canary.CanaryError, "unmapped non-absolute"):
            platform.run(("attacker-launchctl", "print"))

    @unittest.skipUnless(sys.platform == "darwin", "executes the exact macOS runner")
    def test_launchd_hostile_environment_is_cleared_before_candidate_exec(self) -> None:
        self.install()
        paths = self.controller.paths
        capture = paths.root / "captured-runtime-environment"
        hook_marker = paths.root / "shell-hook-ran"
        hook = paths.root / "hostile-shell-hook"
        hook.write_text(
            f"#!/bin/sh\n/usr/bin/touch {hook_marker}\n", encoding="utf-8"
        )
        hook.chmod(0o700)
        node_script = (
            f"#!/bin/sh\n/usr/bin/env > {capture}\n"
        ).encode()
        paths.node.write_bytes(node_script)
        paths.node.chmod(0o700)
        runner = canary.runner_bytes(
            paths,
            [str(paths.node)],
            uid=os.getuid(),
            node_sha256=hashlib.sha256(node_script).hexdigest(),
            node_size=len(node_script),
            genesis_sha256=hashlib.sha256(paths.genesis.read_bytes()).hexdigest(),
            genesis_size=paths.genesis.stat().st_size,
        )
        paths.runner.write_bytes(runner)
        paths.runner.chmod(0o700)
        plist = plistlib.loads(canary.plist_bytes(paths))
        program = plist["ProgramArguments"]
        self.assertEqual("/usr/bin/env", program[0])
        self.assertEqual("-i", program[1])
        self.assertNotIn("EnvironmentVariables", plist)
        hostile = {
            "PATH": "/attacker/bin",
            "HOME": "/attacker/home",
            "TMPDIR": "/attacker/tmp",
            "BASH_ENV": str(hook),
            "ENV": str(hook),
            "DYLD_INSERT_LIBRARIES": "/attacker/libinject.dylib",
            "DYLD_LIBRARY_PATH": "/attacker/lib",
            "LD_LIBRARY_PATH": "/attacker/elf-lib",
            "PYTHONPATH": "/attacker/python",
            "NODE_OPTIONS": "--require=/attacker/hook.js",
            "HTTPS_PROXY": "http://attacker.invalid:8080",
            "SSH_AUTH_SOCK": "/attacker/agent.sock",
        }
        result = subprocess.run(
            program,
            cwd=paths.root,
            env=hostile,
            text=True,
            capture_output=True,
            timeout=10,
            check=False,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertFalse(hook_marker.exists())
        captured = dict(
            line.split("=", 1)
            for line in capture.read_text(encoding="utf-8").splitlines()
            if "=" in line
        )
        self.assertEqual(str(paths.root.parent), captured["HOME"])
        self.assertEqual(str(paths.tmp_dir), captured["TMPDIR"])
        self.assertEqual(canary.FIXED_RUNTIME_PATH, captured["PATH"])
        self.assertEqual("C", captured["LANG"])
        self.assertEqual("C", captured["LC_ALL"])
        self.assertEqual("arc=info", captured["RUST_LOG"])
        for name in hostile:
            if name not in {"HOME", "TMPDIR", "PATH"}:
                self.assertNotIn(name, captured)

    def test_lifecycle_lock_serializes_cleanup_against_concurrent_start(self) -> None:
        self.install()
        self.controller.start()
        second = canary.CanaryController(
            self.controller.paths.root,
            self.fake,
            home=self.fixture.home,
            stop_budget_seconds=2,
            start_proof_seconds=2,
        )
        entered = threading.Event()
        release = threading.Event()
        original_stop = self.controller.stop

        def held_stop():
            entered.set()
            if not release.wait(3):
                raise AssertionError("test did not release held cleanup")
            return original_stop()

        self.controller.stop = held_stop  # type: ignore[method-assign]
        cleanup_errors: list[BaseException] = []
        start_errors: list[BaseException] = []

        def cleanup_worker():
            try:
                self.controller.cleanup()
            except BaseException as error:  # pragma: no cover - assertion aid
                cleanup_errors.append(error)

        def start_worker():
            try:
                second.start()
            except BaseException as error:
                start_errors.append(error)

        cleanup_thread = threading.Thread(target=cleanup_worker)
        cleanup_thread.start()
        self.assertTrue(entered.wait(2))
        start_thread = threading.Thread(target=start_worker)
        start_thread.start()
        time.sleep(0.05)
        self.assertTrue(start_thread.is_alive(), "concurrent start bypassed lifecycle lock")
        release.set()
        cleanup_thread.join(3)
        start_thread.join(3)
        self.assertFalse(cleanup_thread.is_alive())
        self.assertFalse(start_thread.is_alive())
        self.assertEqual([], cleanup_errors)
        self.assertEqual(1, len(start_errors))
        self.assertIn("LaunchAgent", str(start_errors[0]))
        self.assertFalse(self.fake.loaded)

    def test_lifecycle_lock_rejects_hardlink_without_mutating_target(self) -> None:
        victim = self.fixture.home / "operator-evidence.json"
        victim.write_bytes(b"preserve me\n")
        victim.chmod(0o640)
        os.link(victim, self.controller.paths.lifecycle_lock)

        with self.assertRaisesRegex(canary.CanaryError, "unsafe owner/mode/type/link"):
            with self.controller._lifecycle_transaction():
                self.fail("unsafe hard-linked lifecycle lock was accepted")

        self.assertEqual(b"preserve me\n", victim.read_bytes())
        self.assertEqual(0o640, stat.S_IMODE(victim.stat().st_mode))
        self.assertEqual(2, victim.stat().st_nlink)

    def test_lifecycle_lock_serializes_stop_before_concurrent_start(self) -> None:
        self.install()
        self.controller.start()
        second = canary.CanaryController(
            self.controller.paths.root,
            self.fake,
            home=self.fixture.home,
            stop_budget_seconds=2,
            start_proof_seconds=2,
        )
        entered = threading.Event()
        release = threading.Event()
        original_validate = self.controller._validate_installation

        def held_validate(*, require_launch_agent: bool):
            entered.set()
            if not release.wait(3):
                raise AssertionError("test did not release held stop")
            return original_validate(require_launch_agent=require_launch_agent)

        self.controller._validate_installation = held_validate  # type: ignore[method-assign]
        stop_errors: list[BaseException] = []
        start_errors: list[BaseException] = []

        def stop_worker():
            try:
                self.controller.stop()
            except BaseException as error:  # pragma: no cover - assertion aid
                stop_errors.append(error)

        def start_worker():
            try:
                second.start()
            except BaseException as error:  # pragma: no cover - assertion aid
                start_errors.append(error)

        stop_thread = threading.Thread(target=stop_worker)
        stop_thread.start()
        self.assertTrue(entered.wait(2))
        start_thread = threading.Thread(target=start_worker)
        start_thread.start()
        time.sleep(0.05)
        self.assertTrue(start_thread.is_alive(), "concurrent start bypassed lifecycle lock")
        self.assertTrue(self.fake.loaded)
        release.set()
        stop_thread.join(3)
        start_thread.join(3)
        self.assertFalse(stop_thread.is_alive())
        self.assertFalse(start_thread.is_alive())
        self.assertEqual([], stop_errors)
        self.assertEqual([], start_errors)
        self.assertTrue(self.fake.loaded)
        self.assertTrue(self.fake.alive)
        lifecycle = [args[:2] for args in self.fake.commands if args[:2] in {
            ("launchctl", "bootout"),
            ("launchctl", "bootstrap"),
        }]
        self.assertEqual(("launchctl", "bootout"), lifecycle[-2])
        self.assertEqual(("launchctl", "bootstrap"), lifecycle[-1])

    def test_managed_modes_are_private(self) -> None:
        self.install()
        paths = self.controller.paths
        for path in (
            paths.key,
            paths.genesis,
            paths.metadata,
            paths.provenance_receipt,
            paths.provenance_recheck,
            paths.config,
            paths.config_checksum,
            paths.launch_agent,
        ):
            self.assertEqual(0o600, stat.S_IMODE(path.stat().st_mode), path)
        for path in (paths.node, paths.cli, paths.runner):
            self.assertEqual(0o700, stat.S_IMODE(path.stat().st_mode), path)
        self.assertEqual(0o400, stat.S_IMODE(paths.model.stat().st_mode), paths.model)
        self.assertEqual(
            0o600,
            stat.S_IMODE(paths.lifecycle_lock.stat().st_mode),
            paths.lifecycle_lock,
        )
        self.assertEqual(0o700, stat.S_IMODE(paths.tmp_dir.stat().st_mode), paths.tmp_dir)

    def test_retained_final_provenance_rejects_cached_api_even_if_local_hashes_are_rewritten(self) -> None:
        self.install()
        paths = self.controller.paths
        recheck = json.loads(paths.provenance_recheck.read_text())
        recheck["api"]["responses"][-1]["age"] = 1
        recheck_bytes = canary.canonical_json(recheck)
        paths.provenance_recheck.write_bytes(recheck_bytes)

        config = json.loads(paths.config.read_text())
        config["pretag"]["live_recheck_sha256"] = hashlib.sha256(
            recheck_bytes
        ).hexdigest()
        config_bytes = canary.canonical_json(config)
        paths.config.write_bytes(config_bytes)
        paths.config_checksum.write_bytes(
            f"{hashlib.sha256(config_bytes).hexdigest()}  canary.json\n".encode()
        )
        with self.assertRaisesRegex(canary.CanaryError, "malformed or cached"):
            self.controller._validate_installation(require_launch_agent=True)


if __name__ == "__main__":
    unittest.main(verbosity=2)
