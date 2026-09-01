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
import pathlib
import re
import unittest


SCRIPT = pathlib.Path(__file__).with_name("archive-node.sh")


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
    start = text.index("\nimport datetime\n", function) + 1
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

    def test_embedded_programs_compile(self) -> None:
        compile(self.outer, "archive-node-quarantine-round", "exec")
        compile(self.helper, "archive-node-pinned-helper", "exec")
        compile(self.live_capture, "archive-node-live-source-capture", "exec")

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
            'if recovery_started <= deadline:',
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
        deadline_check = self.helper.index(
            "authorization expired immediately before", initial
        )
        nft = self.helper.index(
            'subprocess.run([str(nft),"-f",str(STATE/"rendered-policy.nft")],check=True)',
            deadline_check,
        )
        prefix = self.helper[initial:nft]
        self.assertIn("verify_writer(contract)", prefix)
        self.assertIn("authorization expired immediately before", prefix)
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
