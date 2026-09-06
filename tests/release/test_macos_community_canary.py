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
        self.disable_takes_effect = True
        self.exit_on_term = True
        self.pid = 4242
        self.expected_command = ""
        self.expected_executable: Path | None = None
        self.reported_command: str | None = None
        self.drain_reported_command: str | None = None
        self.drain_executable: Path | None = None
        self.listener_names = [canary.RPC]
        self.all_tcp_names: list[str] | None = None
        self.udp_names: list[str] = []
        self.drain_listener_names: list[str] | None = None
        self.drain_all_tcp_names: list[str] | None = None
        self.drain_udp_names: list[str] | None = None
        self.extra_text_paths: list[str] = []
        self.lsof_pid: int | None = None
        self.empty_socket_rc1 = False
        self.startup_phases: list[dict[str, object]] | None = None
        self.startup_phase = 0
        self.pid_changes_on_sleep: list[int] = []
        self.start_on_kick = True
        self.term_sent = False
        self.exit_after_term_sleeps: int | None = None
        self.term_sleep_calls = 0
        self.orphan_processes: dict[int, str] = {}
        self.ps_missing_returncode = 1
        self.ps_missing_stdout = ""
        self.ps_missing_stderr = ""
        self.launch_agent_path: Path | None = None
        self.working_directory: Path | None = None
        self.log_path: Path | None = None
        self.launch_arguments: list[str] = []
        self.launch_runs = 1
        self.launch_last_exit_code = 0
        self.launch_properties = "inferred program"
        self.unloaded_print_returncode = 113
        self.unloaded_print_stderr: str | None = None
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
                    stderr = self.unloaded_print_stderr or (
                        "Bad request.\n"
                        f'Could not find service "{canary.LABEL}" in domain for '
                        f"user gui: {self.uid}\n"
                    )
                    return completed(
                        args,
                        self.unloaded_print_returncode,
                        stderr=stderr,
                    )
                active_count = 1 if self.alive else 0
                state = "running" if self.alive else "not running"
                pid = f"    pid = {self.pid}\n" if self.alive else ""
                arguments = "".join(
                    f"        {value}\n" for value in self.launch_arguments
                )
                return completed(
                    args,
                    stdout=(
                        f"{target} = {{\n"
                        f"    active count = {active_count}\n"
                        f"    path = {self.launch_agent_path}\n"
                        "    type = LaunchAgent\n"
                        f"    state = {state}\n"
                        "    program = /usr/bin/env\n"
                        "    arguments = {\n"
                        f"{arguments}"
                        "    }\n"
                        f"    working directory = {self.working_directory}\n"
                        f"    stdout path = {self.log_path}\n"
                        f"    stderr path = {self.log_path}\n"
                        "    umask = 77\n"
                        f"    runs = {self.launch_runs}\n"
                        f"    last exit code = {self.launch_last_exit_code}\n"
                        f"    properties = {self.launch_properties}\n"
                        f"{pid}"
                        "}\n"
                    ),
                )
            return completed(args, stdout="domain = { active = true }\n")
        if args[:2] == ("launchctl", "print-disabled"):
            state = "disabled" if self.disabled else "enabled"
            return completed(
                args,
                stdout=(
                    "disabled services = {\n"
                    f'    "{canary.LABEL}" => {state}\n'
                    "}\n"
                ),
            )
        if args[:2] == ("launchctl", "enable"):
            self.disabled = False
            return completed(args)
        if args[:2] == ("launchctl", "disable"):
            if self.disable_takes_effect:
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
            self.startup_phase = 0
            return completed(args)
        if args[:2] == ("launchctl", "kill"):
            if args[2] != "SIGTERM" or not self.loaded or not self.alive:
                return completed(args, 3, stderr="bad signal target")
            if self.exit_on_term:
                self.alive = False
            self.term_sent = True
            self.term_sleep_calls = 0
            return completed(args)
        if args[:2] == ("launchctl", "bootout"):
            if self.alive:
                return completed(args, 5, stderr="mock refuses bootout of live process")
            self.loaded = False
            return completed(args)
        if args[:2] == ("ps", "-p") and args[3:] == ("-o", "pid="):
            if self.alive and int(args[2]) == self.pid:
                return completed(args, stdout=f"{self.pid:5d}\n")
            return completed(
                args,
                self.ps_missing_returncode,
                stdout=self.ps_missing_stdout,
                stderr=self.ps_missing_stderr,
            )
        if args == ("ps", "-ww", "-axo", "pid=,command="):
            rows = dict(self.orphan_processes)
            if self.alive:
                rows[self.pid] = self.reported_command or str(
                    self._phase().get("command", self.expected_command)
                )
            return completed(
                args,
                stdout="".join(f"{pid:6d} {command}\n" for pid, command in sorted(rows.items())),
            )
        if args[:3] == ("ps", "-ww", "-p") and args[4:] == ("-o", "command="):
            if self.alive and int(args[3]) == self.pid:
                phase = self._phase()
                command = (
                    self.drain_reported_command
                    if self.term_sent and self.drain_reported_command is not None
                    else self.reported_command
                    or str(phase.get("command", self.expected_command))
                )
                return completed(args, stdout=f"{command}\n")
            return completed(args, 1)
        if args[:3] == ("ps", "-ww", "-p") and args[4:] == ("-o", "comm="):
            if self.alive and int(args[3]) == self.pid:
                phase = self._phase()
                executable = (
                    self.drain_executable
                    if self.term_sent and self.drain_executable is not None
                    else phase.get("executable", self.expected_executable)
                )
                if executable is None:
                    return completed(args, 1, stderr="no executable")
                return completed(args, stdout=f"{executable}\n")
            return completed(args, 1)
        if args[:3] == ("lsof", "-nP", "-a"):
            if self.alive:
                phase = self._phase()
                if "-iUDP" in args:
                    selected = (
                        self.drain_udp_names
                        if self.term_sent and self.drain_udp_names is not None
                        else phase.get("udp", self.udp_names)
                    )
                elif "-sTCP:LISTEN" in args:
                    selected = (
                        self.drain_listener_names
                        if self.term_sent and self.drain_listener_names is not None
                        else phase.get("tcp", self.listener_names)
                    )
                else:
                    selected = (
                        self.drain_all_tcp_names
                        if self.term_sent and self.drain_all_tcp_names is not None
                        else phase.get(
                            "all_tcp",
                            self.all_tcp_names
                            if self.all_tcp_names is not None
                            else phase.get("tcp", self.listener_names),
                        )
                    )
                if self.empty_socket_rc1 and not selected:
                    return completed(args, 1)
                names = "".join(f"n{value}\n" for value in selected)
                return completed(args, stdout=f"p{self.pid}\n{names}")
            return completed(args, 1, stderr="no listeners")
        if args[:2] == ("lsof", "-a"):
            if self.alive and self.expected_executable is not None:
                phase = self._phase()
                executable = (
                    self.drain_executable
                    if self.term_sent and self.drain_executable is not None
                    else phase.get("executable", self.expected_executable)
                )
                extras = "".join(
                    f"ftxt\nn{value}\n" for value in self.extra_text_paths
                )
                return completed(
                    args,
                    stdout=(
                        f"p{self.pid if self.lsof_pid is None else self.lsof_pid}\n"
                        f"ftxt\nn{executable}\n{extras}"
                    ),
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

    def _phase(self) -> dict[str, object]:
        if not self.startup_phases:
            return {}
        return self.startup_phases[self.startup_phase]

    def sleep(self, seconds: float) -> None:
        self.sleep_calls += 1
        if self.term_sent:
            self.term_sleep_calls += 1
            if self.exit_after_term_sleeps == self.term_sleep_calls:
                self.alive = False
        if self.pid_changes_on_sleep:
            self.pid = self.pid_changes_on_sleep.pop(0)
        if self.startup_phases and self.startup_phase + 1 < len(self.startup_phases):
            self.startup_phase += 1


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
        self.fake.launch_agent_path = self.controller.paths.launch_agent
        self.fake.working_directory = self.controller.paths.root
        self.fake.log_path = self.controller.paths.log
        self.fake.launch_arguments = plistlib.loads(
            self.controller.paths.launch_agent.read_bytes()
        )["ProgramArguments"]

    def exact_startup_phases(self) -> list[dict[str, object]]:
        launch_arguments = plistlib.loads(
            self.controller.paths.launch_agent.read_bytes()
        )["ProgramArguments"]
        return [
            {
                "command": " ".join(launch_arguments),
                "executable": Path("/usr/bin/env"),
                "tcp": [],
                "udp": [],
            },
            {
                "command": f"/bin/sh {self.controller.paths.runner}",
                "executable": Path("/bin/sh"),
                "tcp": [],
                "udp": [],
            },
            {
                "command": self.fake.expected_command,
                "executable": self.controller.paths.node,
                "tcp": [],
                "udp": [],
            },
            {
                "command": self.fake.expected_command,
                "executable": self.controller.paths.node,
                "tcp": [canary.RPC],
                "udp": [],
            },
        ]

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

    def test_acceptance_receipt_binds_exact_running_worker_and_is_create_only(self) -> None:
        self.install()
        self.controller.start()
        self.controller.accept()
        path = self.controller.paths.acceptance_receipt
        first = path.read_bytes()
        receipt = json.loads(first)
        config = self.controller._load_config()
        self.assertEqual(
            "arc.macos.pretag-community-canary.acceptance.v1",
            receipt["schema"],
        )
        self.assertEqual("0x" + self.fake.public_address, receipt["worker"])
        self.assertEqual(canary.sha256(self.controller.paths.config), receipt["config_sha256"])
        self.assertEqual(config["pretag"]["artifact_id"], receipt["pretag"]["artifact_id"])
        self.assertTrue(receipt["process"]["exact_executable_argv_and_listeners_proved"])
        self.controller.accept()
        self.assertEqual(first, path.read_bytes())

        hostile = json.loads(first)
        hostile["worker"] = "0x" + "9" * 64
        path.write_bytes(canary.canonical_json(hostile))
        path.chmod(0o600)
        with self.assertRaisesRegex(canary.CanaryError, "validated canary config"):
            self.controller.status()

    def test_start_allows_only_exact_same_pid_env_shell_node_chain(self) -> None:
        self.install()
        self.controller.start_proof_seconds = 6
        self.fake.startup_phases = self.exact_startup_phases()
        original_pid = self.fake.pid
        command_count = len(self.fake.commands)

        self.controller.start()

        new_commands = self.fake.commands[command_count:]
        self.assertEqual(original_pid, self.fake.pid)
        self.assertEqual(3, self.fake.sleep_calls)
        self.assertFalse(self.fake.disabled)
        self.assertTrue(self.fake.loaded)
        self.assertTrue(self.fake.alive)
        self.assertFalse(
            any(args[:2] == ("launchctl", "kill") for args in new_commands)
        )
        self.assertFalse(
            any(args[:2] == ("launchctl", "bootout") for args in new_commands)
        )
        start_evidence = [
            json.loads(path.read_text(encoding="utf-8"))
            for path in self.controller.paths.evidence_dir.glob("*-start-*.json")
        ]
        self.assertEqual(1, len(start_evidence))
        self.assertFalse(start_evidence[0]["recovered_loaded_disabled"])

    def test_start_rejects_near_miss_bootstrap_identity_without_signal(self) -> None:
        self.install()
        cases = (
            ("env argv", 0, "command", " --unexpected"),
            ("shell argv", 1, "command", ".wrong"),
            ("env executable", 0, "executable", Path("/bin/sh")),
        )
        for description, phase_index, field, alteration in cases:
            with self.subTest(description=description):
                phases = self.exact_startup_phases()
                if field == "command":
                    phases[phase_index][field] = str(phases[phase_index][field]) + str(
                        alteration
                    )
                else:
                    phases[phase_index][field] = alteration
                self.fake.startup_phases = [phases[phase_index]]
                self.fake.loaded = False
                self.fake.alive = False
                self.fake.disabled = False
                command_count = len(self.fake.commands)
                with self.assertRaises(canary.CanaryError):
                    self.controller.start()
                new_commands = self.fake.commands[command_count:]
                self.assertTrue(self.fake.disabled)
                self.assertTrue(self.fake.loaded)
                self.assertTrue(self.fake.alive)
                self.assertFalse(
                    any(args[:2] == ("launchctl", "kill") for args in new_commands)
                )
                self.assertFalse(
                    any(args[:2] == ("launchctl", "bootout") for args in new_commands)
                )

    def test_transient_runner_identity_is_never_accepted_by_status_or_stop(self) -> None:
        self.install()
        self.fake.loaded = True
        self.fake.alive = True
        self.fake.startup_phases = [self.exact_startup_phases()[1]]
        with self.assertRaisesRegex(canary.CanaryError, "argv differs"):
            self.controller.status()
        with self.assertRaisesRegex(canary.CanaryError, "argv differs"):
            self.controller.stop()
        self.assertTrue(self.fake.alive)
        self.assertFalse(self.fake.disabled)

    def test_exact_runner_timeout_disables_without_signal_bootout_or_start_evidence(self) -> None:
        self.install()
        self.fake.startup_phases = [self.exact_startup_phases()[1]]
        command_count = len(self.fake.commands)
        with self.assertRaisesRegex(canary.CanaryError, "last exact phase: runner"):
            self.controller.start()
        new_commands = self.fake.commands[command_count:]
        self.assertTrue(self.fake.disabled)
        self.assertTrue(self.fake.loaded)
        self.assertTrue(self.fake.alive)
        self.assertFalse(
            any(args[:2] == ("launchctl", "kill") for args in new_commands)
        )
        self.assertFalse(
            any(args[:2] == ("launchctl", "bootout") for args in new_commands)
        )
        self.assertFalse(
            any(self.controller.paths.evidence_dir.glob("*-start-*.json"))
        )

    def test_start_rejects_runner_with_established_tcp_without_signaling(self) -> None:
        self.install()
        runner = self.exact_startup_phases()[1]
        runner["tcp"] = ["127.0.0.1:49152->127.0.0.1:443"]
        self.fake.startup_phases = [runner]
        command_count = len(self.fake.commands)
        with self.assertRaisesRegex(canary.CanaryError, "runner unexpectedly owns"):
            self.controller.start()
        new_commands = self.fake.commands[command_count:]
        self.assertTrue(self.fake.disabled)
        self.assertTrue(self.fake.loaded)
        self.assertTrue(self.fake.alive)
        self.assertFalse(
            any(args[:2] == ("launchctl", "kill") for args in new_commands)
        )
        self.assertFalse(
            any(args[:2] == ("launchctl", "bootout") for args in new_commands)
        )

    def test_start_rejects_pid_change_across_exec_only_chain(self) -> None:
        self.install()
        self.controller.start_proof_seconds = 4
        self.fake.startup_phases = self.exact_startup_phases()
        self.fake.pid_changes_on_sleep = [self.fake.pid + 1]
        command_count = len(self.fake.commands)
        with self.assertRaisesRegex(canary.CanaryError, "PID changed"):
            self.controller.start()
        new_commands = self.fake.commands[command_count:]
        self.assertTrue(self.fake.disabled)
        self.assertFalse(
            any(args[:2] == ("launchctl", "kill") for args in new_commands)
        )
        self.assertFalse(
            any(args[:2] == ("launchctl", "bootout") for args in new_commands)
        )

    def test_loaded_disabled_ready_node_is_reenabled_reproved_and_evidenced(self) -> None:
        self.install()
        self.fake.loaded = True
        self.fake.alive = True
        self.fake.disabled = True
        command_count = len(self.fake.commands)

        self.controller.start()

        new_commands = self.fake.commands[command_count:]
        self.assertFalse(self.fake.disabled)
        self.assertFalse(
            any(args[:2] == ("launchctl", "kickstart") for args in new_commands)
        )
        self.assertIn(
            ("launchctl", "enable", self.controller._service_target()), new_commands
        )
        start_evidence = [
            json.loads(path.read_text(encoding="utf-8"))
            for path in self.controller.paths.evidence_dir.glob("*-start-*.json")
        ]
        self.assertEqual(1, len(start_evidence))
        self.assertTrue(start_evidence[0]["recovered_loaded_disabled"])

    def test_loaded_disabled_recovery_failure_restores_disabled_state(self) -> None:
        self.install()
        self.fake.loaded = True
        self.fake.alive = True
        self.fake.disabled = True
        with mock.patch.object(
            self.controller,
            "_write_evidence",
            side_effect=canary.CanaryError("injected evidence failure"),
        ):
            with self.assertRaisesRegex(canary.CanaryError, "evidence failure"):
                self.controller.start()
        self.assertTrue(self.fake.disabled)
        self.assertTrue(self.fake.loaded)
        self.assertTrue(self.fake.alive)

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

    def test_stake_zero_socket_proof_accepts_no_udp_and_rejects_every_udp_socket(self) -> None:
        self.install()
        self.controller.start()
        for udp_names in (
            ["*:54321"],
            ["0.0.0.0:54321"],
            ["127.0.0.1:54321", "127.0.0.1:54322"],
            ["127.0.0.1:not-a-port"],
            ["127.0.0.1:0"],
            ["127.0.0.1:65536"],
        ):
            with self.subTest(udp_names=udp_names):
                self.fake.udp_names = udp_names
                with self.assertRaisesRegex(canary.CanaryError, "must own no UDP"):
                    self.controller.status()
        self.fake.udp_names = []
        self.assertEqual(0, self.controller.status())

    def test_extra_darwin_txt_mapping_does_not_make_executable_ambiguous(self) -> None:
        self.install()
        self.controller.start()
        self.fake.extra_text_paths = ["/Library/Preferences/Logging/.plist-cache.test"]
        self.assertEqual(0, self.controller.status())

    def test_lsof_executable_proof_requires_the_exact_pid_row(self) -> None:
        self.install()
        self.controller.start()
        self.fake.lsof_pid = self.fake.pid + 1
        with self.assertRaisesRegex(canary.CanaryError, "bind exactly the canary PID"):
            self.controller.status()

    def test_lsof_rc1_with_empty_output_means_no_socket_matches(self) -> None:
        self.install()
        self.controller.start_proof_seconds = 6
        self.fake.startup_phases = self.exact_startup_phases()
        self.fake.empty_socket_rc1 = True
        self.controller.start()
        self.assertEqual(0, self.controller.status())

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

    def test_stop_allows_exact_rpc_to_zero_socket_drain_before_clean_exit(self) -> None:
        self.install()
        self.controller.start()
        self.fake.exit_on_term = False
        self.fake.drain_listener_names = []
        self.fake.drain_all_tcp_names = [
            "127.0.0.1:53123->149.28.32.76:443"
        ]
        self.fake.drain_udp_names = []
        self.fake.exit_after_term_sleeps = 1
        command_count = len(self.fake.commands)

        self.controller.stop()

        new_commands = self.fake.commands[command_count:]
        self.assertFalse(self.fake.loaded)
        self.assertFalse(self.fake.alive)
        self.assertIn(
            ("launchctl", "kill", "SIGTERM", self.controller._service_target()),
            new_commands,
        )
        self.assertIn(
            ("launchctl", "bootout", self.controller._service_target()),
            new_commands,
        )
        evidence = [
            json.loads(path.read_text(encoding="utf-8"))
            for path in self.controller.paths.evidence_dir.glob("*-stop-*.json")
        ]
        self.assertEqual(1, len(evidence))
        self.assertFalse(evidence[0]["recovered_loaded_disabled_no_pid"])
        self.assertRegex(evidence[0]["service_snapshot_sha256"], r"^[0-9a-f]{64}$")

    def test_graceful_drain_rejects_additional_tcp_listener(self) -> None:
        self.install()
        self.controller.start()
        self.fake.exit_on_term = False
        self.fake.drain_listener_names = [canary.RPC, "*:19945"]
        self.fake.drain_all_tcp_names = [
            canary.RPC,
            "*:19945",
        ]
        command_count = len(self.fake.commands)

        with self.assertRaisesRegex(canary.CanaryError, "RPC-to-zero transition"):
            self.controller.stop()

        new_commands = self.fake.commands[command_count:]
        self.assertTrue(self.fake.loaded)
        self.assertTrue(self.fake.alive)
        self.assertTrue(self.fake.disabled)
        self.assertFalse(
            any(args[:2] == ("launchctl", "bootout") for args in new_commands)
        )

    def test_graceful_drain_rejects_udp_socket(self) -> None:
        self.install()
        self.controller.start()
        self.fake.exit_on_term = False
        self.fake.drain_listener_names = []
        self.fake.drain_all_tcp_names = []
        self.fake.drain_udp_names = ["127.0.0.1:55331"]

        with self.assertRaisesRegex(canary.CanaryError, "must own no UDP"):
            self.controller.stop()

        self.assertTrue(self.fake.loaded)
        self.assertTrue(self.fake.alive)
        self.assertTrue(self.fake.disabled)

    def test_graceful_drain_rejects_rpc_reopen_after_zero_socket_state(self) -> None:
        self.install()
        self.controller.start()
        config = self.controller._load_config()
        self.fake.term_sent = True
        self.fake.drain_listener_names = []
        self.fake.drain_all_tcp_names = []
        closed = self.controller._prove_pid_graceful_drain(
            config,
            self.fake.pid,
            rpc_listener_closed=False,
        )
        self.assertTrue(closed)
        self.fake.drain_listener_names = [canary.RPC]
        self.fake.drain_all_tcp_names = [canary.RPC]
        with self.assertRaisesRegex(canary.CanaryError, "reopened"):
            self.controller._prove_pid_graceful_drain(
                config,
                self.fake.pid,
                rpc_listener_closed=closed,
            )

    def test_graceful_drain_rejects_argv_or_pid_change_without_bootout(self) -> None:
        self.install()
        self.controller.start()
        self.fake.exit_on_term = False
        self.fake.drain_listener_names = []
        self.fake.drain_all_tcp_names = []
        self.fake.drain_reported_command = "/tmp/not-the-canary --stake 0"
        command_count = len(self.fake.commands)
        with self.assertRaisesRegex(canary.CanaryError, "argv differs"):
            self.controller.stop()
        self.assertTrue(self.fake.loaded)
        self.assertTrue(self.fake.alive)
        self.assertTrue(self.fake.disabled)
        self.assertFalse(
            any(
                args[:2] == ("launchctl", "bootout")
                for args in self.fake.commands[command_count:]
            )
        )

    def test_graceful_drain_rejects_executable_change(self) -> None:
        self.install()
        self.controller.start()
        config = self.controller._load_config()
        self.fake.term_sent = True
        self.fake.drain_listener_names = []
        self.fake.drain_executable = Path("/bin/sh")

        with self.assertRaisesRegex(canary.CanaryError, "executable differs"):
            self.controller._prove_pid_graceful_drain(
                config,
                self.fake.pid,
                rpc_listener_closed=False,
            )

    def test_graceful_drain_rejects_launchd_pid_change(self) -> None:
        self.install()
        self.controller.start()
        self.fake.exit_on_term = False
        self.fake.drain_listener_names = []
        self.fake.drain_all_tcp_names = []
        self.fake.pid_changes_on_sleep = [self.fake.pid + 1]
        command_count = len(self.fake.commands)

        with self.assertRaisesRegex(canary.CanaryError, "PID changed"):
            self.controller.stop()

        self.assertTrue(self.fake.loaded)
        self.assertTrue(self.fake.disabled)
        self.assertFalse(
            any(
                args[:2] == ("launchctl", "bootout")
                for args in self.fake.commands[command_count:]
            )
        )

    def test_graceful_drain_requires_clean_launchd_exit_before_bootout(self) -> None:
        self.install()
        self.controller.start()
        self.fake.launch_last_exit_code = 9
        command_count = len(self.fake.commands)

        with self.assertRaisesRegex(canary.CanaryError, "clean-exit contract"):
            self.controller.stop()

        new_commands = self.fake.commands[command_count:]
        self.assertTrue(self.fake.loaded)
        self.assertFalse(self.fake.alive)
        self.assertTrue(self.fake.disabled)
        self.assertFalse(
            any(args[:2] == ("launchctl", "bootout") for args in new_commands)
        )
        self.assertFalse(
            any(self.controller.paths.evidence_dir.glob("*-stop-*.json"))
        )

    def test_post_bootout_unexpected_launchctl_error_cannot_seal_stop(self) -> None:
        self.install()
        self.controller.start()
        self.fake.unloaded_print_returncode = 1
        self.fake.unloaded_print_stderr = "operation not permitted\n"

        with self.assertRaisesRegex(canary.CanaryError, "unexpected error"):
            self.controller.stop()

        self.assertFalse(self.fake.loaded)
        self.assertFalse(
            any(self.controller.paths.evidence_dir.glob("*-stop-*.json"))
        )

    def test_unexpected_ps_absence_error_cannot_bootout_or_seal_stop(self) -> None:
        self.install()
        self.controller.start()
        self.fake.ps_missing_returncode = 2
        self.fake.ps_missing_stderr = "operation not permitted\n"
        command_count = len(self.fake.commands)

        with self.assertRaisesRegex(canary.CanaryError, "process-existence proof"):
            self.controller.stop()

        new_commands = self.fake.commands[command_count:]
        self.assertTrue(self.fake.loaded)
        self.assertFalse(self.fake.alive)
        self.assertTrue(self.fake.disabled)
        self.assertFalse(
            any(args[:2] == ("launchctl", "bootout") for args in new_commands)
        )
        self.assertFalse(
            any(self.controller.paths.evidence_dir.glob("*-stop-*.json"))
        )

    def test_malformed_ps_live_result_cannot_bootout_or_seal_stop(self) -> None:
        self.install()
        self.controller.start()
        self.fake.ps_missing_returncode = 0
        self.fake.ps_missing_stdout = "9999\n"
        command_count = len(self.fake.commands)

        with self.assertRaisesRegex(canary.CanaryError, "ambiguous live-process"):
            self.controller.stop()

        new_commands = self.fake.commands[command_count:]
        self.assertTrue(self.fake.loaded)
        self.assertFalse(self.fake.alive)
        self.assertTrue(self.fake.disabled)
        self.assertFalse(
            any(args[:2] == ("launchctl", "bootout") for args in new_commands)
        )
        self.assertFalse(
            any(self.controller.paths.evidence_dir.glob("*-stop-*.json"))
        )

    def test_process_exists_accepts_one_native_padded_exact_pid_row(self) -> None:
        self.fake.pid = 1
        self.fake.alive = True
        self.assertTrue(self.controller._process_exists(1))
        self.fake.alive = False
        self.assertFalse(self.controller._process_exists(1))

    def test_only_exact_launchctl_missing_service_result_means_unloaded(self) -> None:
        self.install()
        self.fake.loaded = False
        self.assertFalse(self.controller.is_loaded())
        for returncode, stderr in (
            (1, "operation not permitted\n"),
            (113, "service not found\n"),
            (
                113,
                "Bad request.\n"
                f'Could not find service "{canary.LABEL}" in domain for user gui: 999\n',
            ),
        ):
            with self.subTest(returncode=returncode, stderr=stderr):
                self.fake.unloaded_print_returncode = returncode
                self.fake.unloaded_print_stderr = stderr
                with self.assertRaisesRegex(canary.CanaryError, "unexpected error"):
                    self.controller._service_pid()

    def test_stop_requires_disable_to_take_effect_before_sigterm(self) -> None:
        self.install()
        self.controller.start()
        self.fake.disable_takes_effect = False
        command_count = len(self.fake.commands)

        with self.assertRaisesRegex(canary.CanaryError, "before graceful stop"):
            self.controller.stop()

        new_commands = self.fake.commands[command_count:]
        self.assertTrue(self.fake.loaded)
        self.assertTrue(self.fake.alive)
        self.assertFalse(self.fake.disabled)
        self.assertFalse(any(args[:2] == ("launchctl", "kill") for args in new_commands))
        self.assertFalse(
            any(args[:2] == ("launchctl", "bootout") for args in new_commands)
        )

    def test_loaded_disabled_clean_no_pid_recovery_is_stable_and_signal_free(self) -> None:
        self.install()
        self.controller.start()
        self.fake.disabled = True
        self.fake.alive = False
        command_count = len(self.fake.commands)

        self.controller.stop()

        new_commands = self.fake.commands[command_count:]
        self.assertFalse(self.fake.loaded)
        self.assertFalse(any(args[:2] == ("launchctl", "kill") for args in new_commands))
        self.assertFalse(any(args[:2] == ("launchctl", "enable") for args in new_commands))
        self.assertFalse(any(args[:2] == ("launchctl", "kickstart") for args in new_commands))
        self.assertIn(
            ("launchctl", "bootout", self.controller._service_target()),
            new_commands,
        )
        evidence = [
            json.loads(path.read_text(encoding="utf-8"))
            for path in self.controller.paths.evidence_dir.glob("*-stop-*.json")
        ]
        self.assertEqual(1, len(evidence))
        self.assertTrue(evidence[0]["recovered_loaded_disabled_no_pid"])
        self.assertFalse(evidence[0]["signal_sent"])
        self.assertEqual(
            canary.NO_PID_STABILITY_OBSERVATIONS + 1,
            evidence[0]["stable_no_pid_observations"],
        )

    def test_loaded_disabled_no_pid_recovery_rejects_definition_change(self) -> None:
        self.install()
        self.controller.start()
        self.fake.disabled = True
        self.fake.alive = False
        original_sleep = self.fake.sleep

        def mutate_definition(seconds: float) -> None:
            original_sleep(seconds)
            self.fake.launch_runs += 1

        command_count = len(self.fake.commands)
        with mock.patch.object(self.fake, "sleep", side_effect=mutate_definition):
            with self.assertRaisesRegex(canary.CanaryError, "definition changed"):
                self.controller.stop()
        self.assertTrue(self.fake.loaded)
        self.assertFalse(
            any(
                args[:2] == ("launchctl", "bootout")
                for args in self.fake.commands[command_count:]
            )
        )

    def test_loaded_disabled_no_pid_recovery_rejects_matching_orphan_process(self) -> None:
        self.install()
        self.controller.start()
        self.fake.disabled = True
        self.fake.alive = False
        self.fake.orphan_processes[9999] = self.fake.expected_command

        with self.assertRaisesRegex(canary.CanaryError, "matching runner/node"):
            self.controller.stop()

        self.assertTrue(self.fake.loaded)
        self.assertFalse(
            any(args[:2] == ("launchctl", "bootout") for args in self.fake.commands)
        )

    def test_loaded_disabled_no_pid_recovery_aborts_if_pid_appears(self) -> None:
        self.install()
        self.controller.start()
        self.fake.disabled = True
        self.fake.alive = False
        original_sleep = self.fake.sleep

        def revive_process(seconds: float) -> None:
            original_sleep(seconds)
            self.fake.alive = True

        command_count = len(self.fake.commands)
        with mock.patch.object(self.fake, "sleep", side_effect=revive_process):
            with self.assertRaisesRegex(canary.CanaryError, "PID appeared"):
                self.controller.stop()
        self.assertTrue(self.fake.loaded)
        self.assertTrue(self.fake.alive)
        self.assertFalse(
            any(
                args[:2] == ("launchctl", "bootout")
                for args in self.fake.commands[command_count:]
            )
        )

    def test_loaded_disabled_no_pid_recovery_rejects_nonzero_exit(self) -> None:
        self.install()
        self.controller.start()
        self.fake.disabled = True
        self.fake.alive = False
        self.fake.launch_last_exit_code = 1

        with self.assertRaisesRegex(canary.CanaryError, "clean-exit contract"):
            self.controller.stop()

        self.assertTrue(self.fake.loaded)

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
