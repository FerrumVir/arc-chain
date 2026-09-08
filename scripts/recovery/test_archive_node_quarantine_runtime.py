#!/usr/bin/env python3
"""Focused contract/model tests for archive-node quarantine-round runtime.

These tests intentionally never touch nft, systemd, /proc, or production hosts.
They compile both embedded Python programs, assert the production ordering
guards, and exercise the crash/deadline/reboot/concurrency state machine with
a deterministic fake runtime.
"""

from __future__ import annotations

import ast
import dataclasses
import hashlib
import os
import pathlib
import re
import socket
import sys
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).with_name("archive-node.sh")
FLEET_SCRIPT = SCRIPT.with_name("archive-fleet-to-drive.sh")
MANIFEST_BUILDER = SCRIPT.with_name("build-production-manifest.py")
ROLLOUT = SCRIPT.with_name("recovery_rollout.py")


def round_source() -> str:
    text = SCRIPT.read_text(encoding="utf-8")
    start = text.index("\nimport ctypes\n", text.index("quarantine_round_entry()")) + 1
    end = text.index("\nPY\n}", start)
    return text[start:end]


def helper_source(source: str) -> str:
    tree = ast.parse(source)
    template = next(
        node.value.value
        for node in ast.walk(tree)
        if isinstance(node, ast.Assign)
        and any(isinstance(target, ast.Name) and target.id == "helper_template"
                for target in node.targets)
    )
    return (template.replace("@@PYTHON@@", "/usr/bin/python3")
            .replace("@@STATE@@", repr(
                "/etc/arc-recovery/network-fence-rounds/" + "a" * 64 + "/" + "b" * 64
            )))


def live_capture_source() -> str:
    text = SCRIPT.read_text(encoding="utf-8")
    function = text.index("capture_live_legacy_source()")
    start = text.index("\nimport base64\n", function) + 1
    end = text.index("\nPY\n}", start)
    return text[start:end]


@dataclasses.dataclass
class FakeCaptureJournal:
    request_count: int = 0
    attempt_prefix: str | None = None
    receipt: bool = False
    selector: bool = False
    pair_valid: bool = True

    def crash_after(self, prefix: str) -> None:
        order = ("request", "snapshot", "fixed-pair", "receipt", "selector")
        self.request_count += 1
        self.attempt_prefix = prefix
        if order.index(prefix) >= order.index("receipt"):
            self.receipt = True
        if order.index(prefix) >= order.index("selector"):
            self.selector = True

    def resume(self) -> None:
        if self.selector:
            if not self.receipt or not self.pair_valid:
                raise RuntimeError("selected capture is not a valid completed attempt")
            return
        if self.receipt:
            if not self.pair_valid:
                raise RuntimeError("completed capture cannot be selected")
            self.selector = True
            return
        self.request_count += 1
        self.attempt_prefix = "selector"
        self.receipt = True
        self.selector = True


@dataclasses.dataclass
class FakeRuntime:
    authorized: bool = False
    ready: bool = False
    now: int = 0
    deadline: int = 300
    writer_live: bool = True
    boot: int = 1
    sealed_boot: int = 1
    intent: bool = False
    persistence_plan: bool = False
    persistence_files: int = 0
    persistence_file_total: int = 6
    supervisor_barrier: bool = False
    supervisor_live: bool = True
    alternatives_live: bool = False
    pending_jobs: bool = False
    stable_absence_samples: int = 0
    writer_exit_cause: str | None = None
    writer_exit_signal: int | None = None
    barrier: bool = False
    gate: bool = False
    table: bool = False
    commit: bool = False
    selector: bool = False
    service: bool = False
    roots_exact: bool = True

    def authorize_and_ready(self) -> None:
        self.authorized = True
        self.ready = True

    def prefix(self, name: str) -> None:
        if not self.authorized or not self.ready or not self.roots_exact:
            raise RuntimeError("local exact authorization/readiness required")
        if self.now > self.deadline and not self.table and not self.commit:
            raise TimeoutError("expired before mutation")
        order = (
            "intent", "persistence-plan", "supervisor-dropin", "dropin-2",
            "dropin-3", "dropin-4", "dispatcher", "unit", "daemon-reload",
            "enable", "sync", "barrier", "gate", "nft", "commit",
            "selector", "unit-start",
        )
        target = order.index(name)
        for index, step in enumerate(order[: target + 1]):
            if step == "intent":
                self.intent = True
            elif step == "persistence-plan":
                self.persistence_plan = True
            elif step in {"supervisor-dropin", "dropin-2", "dropin-3", "dropin-4",
                          "dispatcher", "unit"}:
                self.persistence_files = max(self.persistence_files, index - 1)
                if step == "supervisor-dropin":
                    self.supervisor_barrier = True
            elif step == "barrier":
                self.barrier = True
            elif step == "gate":
                if not self.writer_live or self.boot != self.sealed_boot or self.now > self.deadline:
                    raise RuntimeError("writer/deadline gate failed")
                self.gate = True
            elif step == "nft":
                if not self.gate or self.now > self.deadline:
                    raise RuntimeError("nft cannot cross a missing/late gate")
                self.table = True
            elif step == "commit":
                if not self.table:
                    raise RuntimeError("commit cannot predate table")
                self.commit = True
            elif step == "selector":
                if not self.commit:
                    raise RuntimeError("selector cannot predate commit")
                self.selector = True
            elif step == "unit-start":
                if not self.selector:
                    raise RuntimeError("service cannot predate selector")
                self.service = True

    def natural_writer_exit(self) -> None:
        self.writer_live = False
        self.supervisor_live = False
        self.writer_exit_cause = "unknown"
        self.writer_exit_signal = None

    def observe_stable_absence(self) -> None:
        if (not self.writer_live and not self.supervisor_live
                and not self.alternatives_live and not self.pending_jobs):
            self.stable_absence_samples += 1

    def reboot(self) -> None:
        self.boot += 1
        self.table = False
        # The exact selector lives under /run and therefore never survives a
        # reboot, regardless of whether it was published before the crash.
        self.selector = False
        self.service = False
        self.writer_live = not self.supervisor_barrier
        self.supervisor_live = not self.supervisor_barrier
        if not self.writer_live:
            self.writer_exit_cause = "unknown"
            self.writer_exit_signal = None

    def ensure(self) -> bool:
        if not self.commit:
            return False
        self.table = True
        return True

    def reconcile_same_boot(self) -> bool:
        if self.table and self.gate and not self.commit and self.boot == self.sealed_boot:
            self.commit = True
            return True
        return False

    def stopped_candidate(self) -> bool:
        return (
            self.intent and self.supervisor_barrier and not self.writer_live
            and not self.supervisor_live and not self.alternatives_live
            and not self.pending_jobs and self.stable_absence_samples >= 2
            and self.now > self.deadline and not self.table and not self.commit
            and not self.selector and self.writer_exit_cause == "unknown"
            and self.writer_exit_signal is None
        )

    def pre_barrier_absence_is_restart_eligible(self) -> bool:
        return (
            self.intent and not self.supervisor_barrier and not self.writer_live
            and self.writer_exit_cause == "unknown" and self.writer_exit_signal is None
        )


class EmbeddedProgramTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.shell = SCRIPT.read_text(encoding="utf-8")
        cls.outer = round_source()
        cls.helper = helper_source(cls.outer)
        cls.live_capture = live_capture_source()
        cls.fleet = FLEET_SCRIPT.read_text(encoding="utf-8")

    def test_embedded_programs_compile(self) -> None:
        compile(self.outer, "archive-node-quarantine-round", "exec")
        compile(self.helper, "archive-node-pinned-helper", "exec")
        compile(self.live_capture, "archive-node-live-source-capture", "exec")

    def test_prefix_reproof_rejects_path_rotation_after_held_fd_open(self) -> None:
        def fail(message: str) -> None:
            raise RuntimeError(message)

        sources = {
            "current_append_only_prefix": self.live_capture,
            "reprove_capture_prefix": self.outer,
            "reprove_final_prefix": self.outer,
        }
        for name, source in sources.items():
            with self.subTest(function=name), tempfile.TemporaryDirectory() as raw:
                function = next(
                    node for node in ast.walk(ast.parse(source))
                    if isinstance(node, ast.FunctionDef) and node.name == name
                )
                root = pathlib.Path(raw)
                path = root / "state.wal"
                moved = root / "state.wal.rotated"
                content = b"durable-prefix" * 64
                path.write_bytes(content)
                details = path.stat()
                expected = {
                    "device": details.st_dev, "inode": details.st_ino,
                    "mode": details.st_mode, "uid": details.st_uid,
                    "gid": details.st_gid, "nlink": details.st_nlink,
                    "size": len(content), "mtime_ns": details.st_mtime_ns,
                    "ctime_ns": details.st_ctime_ns,
                    "sha256": hashlib.sha256(content).hexdigest(),
                }

                class RotatingOs:
                    O_RDONLY = os.O_RDONLY
                    O_NOFOLLOW = getattr(os, "O_NOFOLLOW", 0)

                    def __init__(self) -> None:
                        self.rotated = False

                    def __getattr__(self, attribute: str):
                        return getattr(os, attribute)

                    def pread(self, descriptor: int, size: int, offset: int) -> bytes:
                        chunk = os.pread(descriptor, size, offset)
                        if not self.rotated:
                            self.rotated = True
                            os.replace(path, moved)
                            path.write_bytes(content)
                            os.chmod(path, details.st_mode & 0o777)
                        return chunk

                namespace = {
                    "os": RotatingOs(), "pathlib": pathlib,
                    "stat": __import__("stat"), "hashlib": hashlib,
                    "HASH_RE": re.compile(r"[0-9a-f]{64}"), "fail": fail,
                }
                exec(
                    compile(
                        ast.Module(body=[function], type_ignores=[]),
                        f"{name}-rotation", "exec",
                    ),
                    namespace,
                )
                arguments = (
                    (path, expected, details.st_mode & 0o777, "test WAL")
                    if name == "reprove_capture_prefix"
                    else (path, expected, "test WAL")
                )
                with self.assertRaisesRegex(RuntimeError, "reviewed prefix changed"):
                    namespace[name](*arguments)

    def test_normalized_capture_precedes_rust_inspection_and_binds_both_wals(self) -> None:
        source = self.live_capture
        normalize_at = source.index(
            'normalized=run_recorded(normalize_command,"wal-normalizer",600)'
        )
        rust_capture_at = source.index(
            'result=run_recorded(capture_command,"legacy-source-inspector",240)',
            normalize_at,
        )
        seal_at = source.index('create(attempt/"receipt.json",raw)', rust_capture_at)
        self.assertLess(normalize_at, rust_capture_at)
        self.assertLess(rust_capture_at, seal_at)
        for required in (
            'capture_data_dir=normalized_source',
            'current_append_only_prefix(data_dir/"state.wal",receipt.get("source_wal")',
            'current_identity(normalized/"state.wal",receipt.get("derivative_wal")',
            'tail_bytes!=loader_bytes-accepted_bytes',
            '"arc.recovery.legacy-wal-normalization.v2"',
        ):
            self.assertIn(required, source)

    def test_v3_v4_and_v5_persisted_heads_bind_the_durable_dag_cursor(self) -> None:
        self.assertIn("capture-live-source)", self.shell)
        self.assertIn("capture-normalized-live-source)", self.shell)
        self.assertIn("capture-durable-wal-live-source)", self.shell)
        self.assertIn("wal-normalizer) filename=normalize-legacy-wal.py; mode=500", self.shell)
        self.assertIn(
            "wal-normalization-plan) filename=legacy-wal-normalization-plan.json; mode=400",
            self.shell,
        )
        self.assertIn("arc.recovery.persisted-legacy-head.v3", self.shell)
        self.assertIn("arc.recovery.persisted-legacy-head.v4", self.shell)
        self.assertIn("arc.recovery.persisted-legacy-head.v5", self.shell)
        self.assertEqual(
            self.fleet.count("rounds.validate_sgp_persisted_head_v5("), 2
        )
        self.assertIn(
            "durable-wal-boundary-plan) filename=durable-wal-boundary-plan.json; mode=400",
            self.shell,
        )
        self.assertIn("recovery inspect-legacy-dag-round", self.shell)
        self.assertIn('"legacy_dag_round"', self.shell)
        self.assertIn('"inspection": dag_inspection', self.shell)
        self.assertIn('sha(canonical(dag_inspection))', self.shell)
        self.assertIn('"trusted_anchor_ancestry"', self.shell)
        self.assertIn('"valid_anchor_descendant"', self.shell)
        self.assertIn('"below_trusted_anchor"', self.shell)
        self.assertIn(
            "arc.recovery.persisted-legacy-head-stopped-precommit.v2", self.shell
        )
        self.assertIn('"legacy_dag_wal_dir"', self.shell)
        self.assertIn("exact-content-pinned-normalization-source", self.shell)

    def test_persistently_stopped_source_projection_reproves_normal_and_sgp_inputs(
        self,
    ) -> None:
        function = next(
            node for node in ast.walk(ast.parse(self.outer))
            if isinstance(node, ast.FunctionDef)
            and node.name == "current_source_projection"
        )

        class ProjectionError(RuntimeError):
            pass

        def fail(message: str) -> None:
            raise ProjectionError(message)

        namespace = {
            "os": os, "pathlib": pathlib, "hashlib": hashlib,
            "HASH_RE": re.compile(r"[0-9a-f]{64}"), "fail": fail,
            "node": "nyc",
        }
        exec(
            compile(
                ast.Module(body=[function], type_ignores=[]),
                "persistently-stopped-source-projection", "exec",
            ),
            namespace,
        )
        project = namespace["current_source_projection"]

        def file_identity(path: pathlib.Path) -> dict[str, object]:
            details = path.stat()
            return {
                "path": str(path), "device": details.st_dev,
                "inode": details.st_ino, "mode": details.st_mode,
                "uid": details.st_uid, "gid": details.st_gid,
                "nlink": details.st_nlink, "size": details.st_size,
                "mtime_ns": details.st_mtime_ns, "ctime_ns": details.st_ctime_ns,
                "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            }

        def directory_identity(path: pathlib.Path) -> dict[str, object]:
            details = path.stat()
            return {
                "path": str(path), "device": details.st_dev,
                "inode": details.st_ino, "mode": details.st_mode,
                "uid": details.st_uid, "gid": details.st_gid,
                "nlink": details.st_nlink, "size": details.st_size,
                "mtime_ns": details.st_mtime_ns, "ctime_ns": details.st_ctime_ns,
            }

        def source_inputs(root: pathlib.Path, *, durable: bool) -> tuple[
            dict[str, object], dict[str, object]
        ]:
            original = root / "original"
            dag_wal = original / "dag-wal"
            fixed = root / "fixed-source"
            dag_wal.mkdir(parents=True)
            fixed.mkdir()
            final_wal = original / "state.wal"
            fixed_wal = fixed / "state.wal"
            snapshot = fixed / "state.snapshot.lz4"
            genesis = fixed / "genesis.network-hash"
            final_wal.write_bytes(b"original-wal")
            fixed_wal.write_bytes(b"fixed-wal")
            snapshot.write_bytes(b"replayed-snapshot")
            genesis.write_bytes(b"genesis-binding")
            head = {
                "height": 97591, "block_hash": "b" * 64,
                "state_root": "c" * 64,
            }
            durable_value = None
            if durable:
                fixed_plan = fixed / "durable-wal-boundary.plan.json"
                preserved = fixed / "source.snapshot.inconsistent.lz4"
                fixed_plan.write_bytes(b"durable-plan")
                preserved.write_bytes(b"observed-snapshot")
                fixed_plan_identity = file_identity(fixed_plan)
                durable_value = {
                    "plan_sha256": fixed_plan_identity["sha256"],
                    "selected_boundary": {
                        **head, "checkpoint_sequence": 17,
                    },
                    "fixed_plan": fixed_plan_identity,
                    "preserved_source_snapshot": file_identity(preserved),
                    "replay_derived_snapshot": file_identity(snapshot),
                }
            value = {
                "original_data_dir": directory_identity(original),
                "legacy_dag_wal_dir": directory_identity(dag_wal),
                "final_state_wal": file_identity(final_wal),
                "fixed_data_dir": directory_identity(fixed),
                "fixed_state_wal": file_identity(fixed_wal),
                "fixed_snapshot": file_identity(snapshot),
                "fixed_genesis_binding": file_identity(genesis),
                "live_source_capture_sha256": "d" * 64,
                "rust_live_source_capture_sha256": "e" * 64,
                "source_pair_role": "preauthorization-boundary",
            }
            if durable_value is not None:
                value["durable_wal_boundary"] = durable_value
            return value, head

        with tempfile.TemporaryDirectory() as raw:
            normal, head = source_inputs(pathlib.Path(raw), durable=False)
            self.assertEqual(project(normal, head), normal)
            self.assertIn("legacy_dag_wal_dir", normal)
            changed = dict(normal)
            changed["legacy_dag_wal_dir"] = dict(normal["legacy_dag_wal_dir"])
            changed["legacy_dag_wal_dir"]["inode"] += 1
            with self.assertRaisesRegex(ProjectionError, "data directory changed"):
                project(changed, head)

        with tempfile.TemporaryDirectory() as raw:
            durable, head = source_inputs(pathlib.Path(raw), durable=True)
            namespace["node"] = "sgp"
            self.assertEqual(project(durable, head), durable)
            evidence = durable["durable_wal_boundary"]
            self.assertEqual(evidence["selected_boundary"]["height"], head["height"])
            changed = dict(durable)
            changed_evidence = dict(evidence)
            changed_evidence["plan_sha256"] = "f" * 64
            changed["durable_wal_boundary"] = changed_evidence
            with self.assertRaisesRegex(ProjectionError, "durable WAL boundary roots"):
                project(changed, head)
            namespace["node"] = "nyc"
            with self.assertRaisesRegex(ProjectionError, "durable WAL boundary evidence"):
                project(durable, head)

    def test_live_capture_persists_bounded_child_process_diagnostics(self) -> None:
        source = self.live_capture
        self.assertIn(
            '"schema":"arc.recovery.live-source-child-process.v1"', source
        )
        self.assertIn('prefix=raw[:64*1024]', source)
        self.assertIn('"prefix_base64":base64.b64encode(prefix)', source)
        self.assertIn('create(attempt/f"{stage}.process.json",canonical(evidence))', source)
        self.assertIn(
            'run_recorded(inspect_command,f"ancestry-{label}-inspector",240)', source
        )
        self.assertIn('except subprocess.TimeoutExpired as error:', source)

    def test_fleet_stages_normalization_only_for_lax_and_ams(self) -> None:
        self.assertIn('case "$node" in\n        lax)', self.fleet)
        self.assertIn('case "$node" in\n                lax)', self.fleet)
        self.assertEqual(self.fleet.count('capture_action="capture-normalized-live-source"'), 4)
        self.assertEqual(self.fleet.count('stage_file "$node" "$freeze_sha" wal-normalizer'), 2)
        self.assertEqual(
            self.fleet.count('stage_file "$node" "$freeze_sha" wal-normalization-plan'), 2
        )
        self.assertIn('"${normalization_args[@]}" > "$temporary"', self.fleet)

    def test_normalization_inputs_are_retained_by_manifest_and_archive(self) -> None:
        builder = MANIFEST_BUILDER.read_text(encoding="utf-8")
        rollout = ROLLOUT.read_text(encoding="utf-8")
        for name in (
            "legacy_wal_normalizer",
            "legacy_wal_normalization_lax",
            "legacy_wal_normalization_ams",
        ):
            self.assertIn(name, builder)
            self.assertIn(name, rollout)
        self.assertIn("normalize-legacy-wal.py", self.fleet)
        self.assertIn("legacy-wal-normalization-lax.json", self.fleet)
        self.assertIn("legacy-wal-normalization-ams.json", self.fleet)

    def test_live_capture_accepts_only_loopback_reachable_ipv4_listeners(self) -> None:
        tree = ast.parse(self.live_capture)
        function_names = {
            "ipv4_loopback_reachable_listener_inodes",
            "listener_inodes_on_port",
            "unique_owned_listener_inode",
        }
        functions = [
            node
            for node in tree.body
            if isinstance(node, ast.FunctionDef)
            and node.name in function_names
        ]

        def fail(message: str) -> None:
            raise RuntimeError(message)

        namespace: dict[str, object] = {"fail": fail}
        exec(
            compile(ast.Module(body=functions, type_ignores=[]), "listener-parser", "exec"),
            namespace,
        )
        parse = namespace["ipv4_loopback_reachable_listener_inodes"]
        parse_all = namespace["listener_inodes_on_port"]
        select = namespace["unique_owned_listener_inode"]
        header = "  sl  local_address rem_address st tx_queue rx_queue tr tm->when retrnsmt uid timeout inode\n"

        exact_loopback = header + "0: 0100007F:2382 00000000:0000 0A 0:0 0:0 0 0 0 4242\n"
        wildcard = header + "0: 00000000:2382 00000000:0000 0A 0:0 0:0 0 0 0 4343\n"
        public_specific = header + "0: 4C201C95:2382 00000000:0000 0A 0:0 0:0 0 0 0 4444\n"
        wrong_port = header + "0: 00000000:2383 00000000:0000 0A 0:0 0:0 0 0 0 4545\n"
        non_listener = header + "0: 00000000:2382 00000000:0000 01 0:0 0:0 0 0 0 4646\n"

        self.assertEqual(parse(exact_loopback, 9090), [4242])
        self.assertEqual(parse(wildcard, 9090), [4343])
        self.assertEqual(parse(public_specific, 9090), [])
        self.assertEqual(parse(wrong_port, 9090), [])
        self.assertEqual(parse(non_listener, 9090), [])
        self.assertEqual(parse_all(public_specific, 9090), [4444])
        self.assertEqual(select([4242], {4242}, []), 4242)
        self.assertEqual(select([4343], {4343}, []), 4343)
        for listeners, owned, ipv6 in (
            ([], set(), []),
            ([4242], set(), []),
            ([4242, 4343], {4242, 4343}, []),
            ([4242], {4242}, [4747]),
        ):
            with self.assertRaises(RuntimeError):
                select(listeners, owned, ipv6)

    @unittest.skipUnless(sys.platform.startswith("linux"), "requires Linux /proc")
    def test_live_capture_matches_real_owned_ipv4_proc_listener(self) -> None:
        tree = ast.parse(self.live_capture)
        functions = [
            node
            for node in tree.body
            if isinstance(node, ast.FunctionDef)
            and node.name in {
                "ipv4_loopback_reachable_listener_inodes",
                "listener_inodes_on_port",
                "unique_owned_listener_inode",
            }
        ]

        def fail(message: str) -> None:
            raise RuntimeError(message)

        namespace: dict[str, object] = {"fail": fail}
        exec(
            compile(ast.Module(body=functions, type_ignores=[]), "listener-proc", "exec"),
            namespace,
        )
        parse = namespace["ipv4_loopback_reachable_listener_inodes"]
        select = namespace["unique_owned_listener_inode"]

        for bind_address in ("127.0.0.1", "0.0.0.0"):
            with self.subTest(bind_address=bind_address):
                with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
                    listener.bind((bind_address, 0))
                    listener.listen(1)
                    port = listener.getsockname()[1]
                    target = os.readlink(f"/proc/{os.getpid()}/fd/{listener.fileno()}")
                    match = re.fullmatch(r"socket:\[([0-9]+)\]", target)
                    self.assertIsNotNone(match)
                    inode = int(match.group(1))
                    rows = parse(pathlib.Path("/proc/net/tcp").read_text(), port)
                    self.assertEqual(select(rows, {inode}, []), inode)
                    with self.assertRaises(RuntimeError):
                        select(rows, set(), [])

        if hasattr(socket, "SO_REUSEPORT"):
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as first:
                first.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
                first.bind(("127.0.0.1", 0))
                first.listen(1)
                port = first.getsockname()[1]
                with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as second:
                    second.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
                    second.bind(("127.0.0.1", port))
                    second.listen(1)
                    rows = parse(pathlib.Path("/proc/net/tcp").read_text(), port)
                    owned = {
                        int(re.fullmatch(
                            r"socket:\[([0-9]+)\]",
                            os.readlink(f"/proc/{os.getpid()}/fd/{item.fileno()}"),
                        ).group(1))
                        for item in (first, second)
                    }
                    self.assertEqual(len(set(rows)), 2)
                    with self.assertRaises(RuntimeError):
                        select(rows, owned, [])

    def test_live_capture_receipt_is_durable_before_its_selector_and_reconciled_before_http(self) -> None:
        receipt = 'create(attempt/"receipt.json",raw)'
        selector = "create(selected,raw)"
        completed_scan = 'for receipt_path in sorted(attempts.glob("*/receipt.json")):'
        request_loop = "for _attempt_number in range(MAX_ATTEMPTS):"
        self.assertLess(self.live_capture.index(receipt), self.live_capture.rindex(selector))
        self.assertLess(
            self.live_capture.index(completed_scan), self.live_capture.index(request_loop)
        )
        self.assertIn(
            'validate_selected_capture(raw,"recovered live source attempt")',
            self.live_capture,
        )
        self.assertIn(
            'if str(parsed_attempt_id)!=attempt_id:', self.live_capture
        )

    def test_old_direct_mutation_dispatch_is_retired(self) -> None:
        retired = (
            "fence-stop|quarantine|quarantine-starter|quarantine-authority|",
            "obsolete global quarantine authority is retired",
        )
        for marker in retired:
            self.assertIn(marker, self.shell)
        self.assertNotRegex(self.shell, r"\n    quarantine\)\n.*fence_stop")

    def test_all_target_readiness_not_singleton(self) -> None:
        self.assertIn("full ordered set", self.shell)
        self.assertIn("len(rows) != len(auth_target_names)", self.outer)
        self.assertNotIn("len(targets) != 1", self.outer)

    def test_supervisor_writer_topology_matches_each_sealed_production_shape(self) -> None:
        tree = ast.parse(self.outer)
        topology_function = next(
            node
            for node in tree.body
            if isinstance(node, ast.FunctionDef)
            and node.name == "verify_supervisor_writer_topology"
        )

        class TopologyError(RuntimeError):
            pass

        def fail(message: str) -> None:
            raise TopologyError(message)

        namespace: dict[str, object] = {"fail": fail}
        exec(
            compile(
                ast.Module(body=[topology_function], type_ignores=[]),
                "supervisor-writer-topology",
                "exec",
            ),
            namespace,
        )
        verify = namespace["verify_supervisor_writer_topology"]
        systemd_cgroup = ["/system.slice/arc-self-heal.service"]
        writer_stat = ["S", "1", "200", "200"]

        # The five production self-heal units own an exact writer child in the
        # same sealed systemd cgroup; their MainPID is intentionally distinct.
        verify(
            {"mode": "systemd-unit", "unit": "arc-self-heal.service"},
            100,
            systemd_cgroup,
            200,
            systemd_cgroup,
            writer_stat,
        )
        with self.assertRaisesRegex(TopologyError, "systemd writer topology"):
            verify(
                {"mode": "systemd-unit", "unit": "arc-self-heal.service"},
                100,
                systemd_cgroup,
                200,
                ["/user.slice/user-0.slice/session-1.scope"],
                writer_stat,
            )

        # A direct arc-node.service remains stricter: its sealed MainPID must
        # be the writer as well as sharing the exact cgroup.
        direct = ["/system.slice/arc-node.service"]
        with self.assertRaisesRegex(TopologyError, "systemd writer topology"):
            verify(
                {"mode": "systemd-unit", "unit": "arc-node.service"},
                100,
                direct,
                200,
                direct,
                writer_stat,
            )
        verify(
            {"mode": "systemd-unit", "unit": "arc-node.service"},
            100,
            direct,
            100,
            direct,
            ["S", "1", "100", "100"],
        )

        # The detached production shape remains a disjoint PID-1 child and
        # session leader supervised by arc-self-heal.service.
        verify(
            {"mode": "detached-root-session", "unit": "arc-self-heal.service"},
            100,
            systemd_cgroup,
            200,
            ["/user.slice/user-0.slice/session-1.scope"],
            writer_stat,
        )
        for invalid in (
            ({"mode": "detached-root-session", "unit": "arc-node.service"}, 200, writer_stat),
            ({"mode": "detached-root-session", "unit": "arc-self-heal.service"}, 100, writer_stat),
            ({"mode": "detached-root-session", "unit": "arc-self-heal.service"}, 200, ["S", "9", "200", "200"]),
            ({"mode": "detached-root-session", "unit": "arc-self-heal.service"}, 200, ["S", "1", "200", "201"]),
        ):
            sealed, writer_pid, fields = invalid
            with self.assertRaisesRegex(TopologyError, "detached root-session"):
                verify(
                    sealed,
                    100,
                    systemd_cgroup,
                    writer_pid,
                    ["/user.slice/user-0.slice/session-1.scope"],
                    fields,
                )

        self.assertIn(
            "verify_supervisor_writer_topology(\n"
            "        sealed, pid, unified, writer_pid, writer_unified, fields,\n"
            "    )",
            self.outer,
        )

    def test_no_runtime_mutation_precedes_readiness(self) -> None:
        ready = self.outer.index("readiness = validate_readiness(readiness_raw")
        for token in (
            "secure_dir(state_base.parent, 0o700, create=True)",
            "systemctl\", \"daemon-reload",
            "subprocess.check_output([str(state / \"apply\"), \"initial\"])",
        ):
            self.assertGreater(self.outer.index(token), ready)

    def test_intent_precedes_restart_affecting_paths(self) -> None:
        intent = self.outer.index("publish(intent_path, intent_raw, 0o400)")
        # The embedded maintenance helper has its own idempotent persistence
        # projection.  Scope these searches to the outer initial-apply path so
        # that the assertion proves the mutation ordering it names rather than
        # accidentally comparing against the helper template definition.
        dispatcher = self.outer.index(
            "publish(dispatcher_path, dispatcher_raw_value, 0o500)", intent
        )
        unit = self.outer.index("publish(unit_path, unit_value, 0o400)", intent)
        enable = self.outer.index(
            "\"enable\", \"arc-legacy-maintenance-fence.service\"", intent
        )
        self.assertLess(intent, dispatcher)
        self.assertLess(intent, unit)
        self.assertLess(intent, enable)

    def test_selected_supervisor_dependency_is_the_first_restart_effective_write(self) -> None:
        payloads = self.outer[self.outer.index("def persistence_payloads():"):
                              self.outer.index("def persistence_file_roots(")]
        self.assertLess(
            payloads.index('legacy_units = [frozen["supervisor_unit"]]'),
            payloads.index('legacy_units.extend('),
        )
        apply_path = self.outer[self.outer.index(
            "# The selected frozen supervisor dependency is first in insertion order."
        ):self.outer.index("barrier_fixed = {", self.outer.index(
            "# The selected frozen supervisor dependency is first in insertion order."
        ))]
        self.assertLess(
            apply_path.index("for dependency, dependency_value in dependencies.items():"),
            apply_path.index("publish(dispatcher_path, dispatcher_raw_value, 0o500)"),
        )
        self.assertLess(
            apply_path.index("publish(dispatcher_path, dispatcher_raw_value, 0o500)"),
            apply_path.index("publish(unit_path, unit_value, 0o400)"),
        )

    def test_same_boot_stopped_terminal_requires_expiry_and_stable_fail_closed_absence(self) -> None:
        precommit = self.outer[self.outer.index('if mode in {"precommit-status", "stopped-precommit"}:'):
                               self.outer.index("def validate_stopped_transition(")]
        for marker in (
            'accepted_elapsed = acceptance.get("accepted_monotonic_ns")',
            'current_elapsed - accepted_elapsed <= lease_ns',
            'fail("precommit stopped status is not after monotonic lease expiry")',
            '"Job", "MainPID", "Requires", "After", "DropInPaths"',
            'properties["ActiveState"] not in {"inactive", "failed"}',
            'properties["Job"] not in {"", "0"}',
            'absence_samples = [stable_absence_sample()]',
            'absence_samples.append(stable_absence_sample())',
            '"writer_exit_cause": "unknown", "writer_exit_signal": None',
            '"reboot_after_intent": reboot_after_intent',
        ):
            self.assertIn(marker, precommit)

    def test_helper_deadline_and_writer_checks_immediately_precede_nft(self) -> None:
        initial = self.helper.index("# initial:")
        lease_check = self.helper.index("enforce_monotonic_lease()", initial)
        nft = self.helper.index(
            'subprocess.run([str(nft),"-f",str(STATE/"rendered-policy.nft")],check=True)',
            lease_check,
        )
        prefix = self.helper[initial:nft]
        self.assertIn("verify_writer(contract)", prefix)
        self.assertIn("enforce_monotonic_lease()", prefix)
        self.assertLess(
            prefix.rindex("verify_writer(contract)"),
            prefix.rindex("enforce_monotonic_lease()"),
        )
        self.assertIn("nft-deadline-gate.json", self.helper)

    def test_ensure_requires_durable_commit(self) -> None:
        ensure = self.helper[self.helper.index('if MODE=="ensure":'):
                             self.helper.index("# initial:")]
        self.assertLess(ensure.index("load_commit()"), ensure.index("-f"))
        self.assertNotIn("load_gate()", ensure)

    def test_binding_covers_every_authority_root(self) -> None:
        for field in (
            "round_authorization_sha256", "round_readiness_sha256",
            "authorization_deadline", "apply_helper_sha256", "policy_sha256",
            "writer", "table_binding_sha256", "table_comment",
        ):
            self.assertIn(field, self.outer)
        self.assertIn("arc-recovery:round=", self.outer)
        self.assertIn(":bind=", self.outer)

    def test_no_replace_and_post_mode_fsync(self) -> None:
        for source in (self.outer, self.helper):
            self.assertIn("renameat2", source)
            self.assertIn("os.fchmod", source)
            self.assertRegex(source, r"os\.fchmod\([^\n]+\)[^\n]*os\.fsync")


class FakeCrashMatrixTests(unittest.TestCase):
    def runtime(self) -> FakeRuntime:
        value = FakeRuntime(now=100)
        value.authorize_and_ready()
        return value

    def test_direct_entry_without_readiness_never_mutates(self) -> None:
        value = FakeRuntime(authorized=True, ready=False, now=100)
        with self.assertRaises(RuntimeError):
            value.prefix("intent")
        self.assertFalse(value.intent)
        self.assertFalse(value.table)

    def test_expired_absent_table_never_applies(self) -> None:
        value = self.runtime()
        value.now = 301
        with self.assertRaises(TimeoutError):
            value.prefix("gate")
        self.assertFalse(value.table)
        self.assertFalse(value.commit)

    def test_post_kernel_precommit_same_boot_recovers_exactly(self) -> None:
        value = self.runtime()
        value.prefix("nft")
        self.assertTrue(value.reconcile_same_boot())
        self.assertTrue(value.commit)

    def test_post_kernel_precommit_reboot_never_late_applies(self) -> None:
        value = self.runtime()
        value.prefix("nft")
        value.reboot()
        value.now = 1000
        self.assertFalse(value.ensure())
        self.assertFalse(value.table)
        self.assertTrue(value.commit is False)

    def test_pre_gate_reboot_is_classifiable_by_intent(self) -> None:
        value = self.runtime()
        value.prefix("enable")
        value.reboot()
        value.now = 1000
        value.observe_stable_absence()
        value.observe_stable_absence()
        self.assertTrue(value.stopped_candidate())
        self.assertFalse(value.ensure())

    def test_commit_before_selector_reboot_clears_selector_but_keeps_recovery_commit(self) -> None:
        value = self.runtime()
        value.prefix("commit")
        self.assertFalse(value.selector)
        value.reboot()
        value.now = 1000
        self.assertFalse(value.selector)
        self.assertFalse(value.writer_live)
        self.assertTrue(value.ensure())
        self.assertTrue(value.table)

    def test_commit_plus_selector_reboot_also_clears_only_the_boot_scoped_selector(self) -> None:
        value = self.runtime()
        value.prefix("selector")
        self.assertTrue(value.commit)
        self.assertTrue(value.selector)
        value.reboot()
        value.now = 1000
        self.assertTrue(value.commit)
        self.assertFalse(value.selector)
        self.assertFalse(value.writer_live)
        self.assertTrue(value.ensure())
        self.assertTrue(value.table)

    def test_wrong_root_fails_before_mutation(self) -> None:
        value = self.runtime()
        value.roots_exact = False
        with self.assertRaises(RuntimeError):
            value.prefix("intent")
        self.assertFalse(value.intent)

    def test_same_attempt_concurrency_has_one_transition(self) -> None:
        first = self.runtime()
        first.prefix("commit")
        second = dataclasses.replace(first)
        self.assertTrue(first.commit and second.commit)
        self.assertEqual(
            hashlib.sha256(repr(first).encode()).hexdigest(),
            hashlib.sha256(repr(second).encode()).hexdigest(),
        )

    def test_every_crash_prefix_is_live_or_stopped_or_committed(self) -> None:
        prefixes = (
            "intent", "persistence-plan", "supervisor-dropin", "dropin-2",
            "dropin-3", "dropin-4", "dispatcher", "unit", "daemon-reload",
            "enable", "sync", "barrier", "gate", "nft", "commit", "selector",
            "unit-start",
        )
        for prefix in prefixes:
            with self.subTest(prefix=prefix):
                value = self.runtime()
                value.prefix(prefix)
                value.reboot()
                value.now = 1000
                value.observe_stable_absence()
                value.observe_stable_absence()
                outcome = (
                    value.writer_live,
                    value.stopped_candidate(),
                    value.commit,
                )
                self.assertEqual(sum(bool(item) for item in outcome), 1)

    def test_same_boot_natural_writer_exit_at_every_prefix_is_restart_eligible_stopped_or_committed(self) -> None:
        prefixes = (
            "intent", "persistence-plan", "supervisor-dropin", "dropin-2",
            "dropin-3", "dropin-4", "dispatcher", "unit", "daemon-reload",
            "enable", "sync", "barrier", "gate", "nft", "commit", "selector",
            "unit-start",
        )
        for prefix in prefixes:
            with self.subTest(prefix=prefix):
                value = self.runtime()
                value.prefix(prefix)
                value.natural_writer_exit()
                value.now = 1000
                if value.table and not value.commit:
                    self.assertTrue(value.reconcile_same_boot())
                value.observe_stable_absence()
                value.observe_stable_absence()
                outcomes = (
                    value.pre_barrier_absence_is_restart_eligible(),
                    value.stopped_candidate(),
                    value.commit,
                )
                self.assertEqual(sum(bool(item) for item in outcomes), 1)
                if value.stopped_candidate():
                    self.assertEqual(value.boot, value.sealed_boot)
                    self.assertEqual(value.writer_exit_cause, "unknown")
                    self.assertIsNone(value.writer_exit_signal)


class FakeCaptureResumeTests(unittest.TestCase):
    def test_receipt_before_selector_crash_reuses_the_completed_attempt(self) -> None:
        journal = FakeCaptureJournal()
        journal.crash_after("receipt")
        journal.resume()
        self.assertEqual(journal.request_count, 1)
        self.assertTrue(journal.receipt and journal.selector)

    def test_receipt_plus_selector_crash_revalidates_without_a_new_request(self) -> None:
        journal = FakeCaptureJournal()
        journal.crash_after("selector")
        journal.resume()
        self.assertEqual(journal.request_count, 1)
        self.assertTrue(journal.receipt and journal.selector)

    def test_incomplete_attempt_is_retained_and_a_new_attempt_is_selected(self) -> None:
        for prefix in ("request", "snapshot", "fixed-pair"):
            with self.subTest(prefix=prefix):
                journal = FakeCaptureJournal()
                journal.crash_after(prefix)
                journal.resume()
                self.assertEqual(journal.request_count, 2)
                self.assertTrue(journal.receipt and journal.selector)

    def test_invalid_completed_attempt_fails_closed_instead_of_reissuing(self) -> None:
        journal = FakeCaptureJournal()
        journal.crash_after("receipt")
        journal.pair_valid = False
        with self.assertRaises(RuntimeError):
            journal.resume()
        self.assertEqual(journal.request_count, 1)


if __name__ == "__main__":
    unittest.main()
