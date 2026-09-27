from __future__ import annotations

import tempfile
import os
import shutil
import signal
import subprocess
import time
import unittest
from pathlib import Path
from unittest.mock import Mock

from scripts.qualification.loopback_cohort_ssh import LoopbackCohortSsh
from scripts.qualification.run_low_residency_conformance import require_clean_exits


EXPECTED_AGENT_SHUTDOWN = {
    "signal": "SIGTERM",
    "requested_by_helper_while_alive": True,
    "authentication_succeeded_before_shutdown": True,
    "forced_kill": False,
    "agent_socket_removed": True,
    "private_key_directory_removed": True,
    "raw_exit_code": 2,
}


class LoopbackCohortSshTests(unittest.TestCase):
    def test_known_hosts_is_exact_pinned_loopback_key(self):
        record = LoopbackCohortSsh.known_hosts_record(43210, "ssh-ed25519 AQID comment")
        self.assertEqual(record, "[127.0.0.1]:43210 ssh-ed25519 AQID\n")
        self.assertNotIn("comment", record)

    def test_known_hosts_rejects_invalid_ports_and_key_types(self):
        for port in (0, 65536):
            with self.subTest(port=port), self.assertRaises(ValueError):
                LoopbackCohortSsh.known_hosts_record(port, "ssh-ed25519 AQID")
        with self.assertRaises(ValueError):
            LoopbackCohortSsh.known_hosts_record(22, "ssh-rsa AQID")
        with self.assertRaises(ValueError):
            LoopbackCohortSsh.known_hosts_record(22, "ssh-ed25519\nmalicious AQID")

    def test_evidence_contains_public_endpoint_only(self):
        helper = LoopbackCohortSsh(Mock(), "/qualification", {"SSH_AUTH_SOCK": "/private/agent"})
        helper._port = 42317
        helper.target = "ssh://runner@127.0.0.1:42317"
        helper.known_hosts = Path("/qualification/loopback_known_hosts")
        evidence = helper.evidence()
        self.assertEqual(evidence["bind_address"], "127.0.0.1")
        self.assertFalse(evidence["private_keys_written_to_output"])
        self.assertFalse(evidence["private_key_material_recorded"])
        self.assertNotIn("SSH_AUTH_SOCK", repr(evidence))
        self.assertNotIn("/private/agent", repr(evidence))

    def test_close_stops_owned_daemons_and_removes_private_directory(self):
        class Process:
            returncode = None

            def poll(self):
                return self.returncode

        class Children:
            def __init__(self):
                self.stopped = None

            def stop(self, processes, timeout):
                self.stopped = (processes, timeout)
                for process in processes:
                    process.returncode = 0

        children = Children()
        helper = LoopbackCohortSsh(children, "/unused", {})
        helper._private_dir = Path(tempfile.mkdtemp(prefix="loopback-helper-test-"))
        sshd, agent = Process(), Process()
        helper._sshd = sshd
        helper._processes = [sshd, agent]
        helper.close()
        self.assertEqual(children.stopped, ([sshd, agent], 5))
        self.assertFalse(helper._private_dir)
        helper.close()  # idempotent

    def test_close_fails_if_owned_daemon_was_not_reaped(self):
        class Process:
            def poll(self):
                return None

        class Children:
            def stop(self, processes, timeout):
                self.timeout = timeout

        helper = LoopbackCohortSsh(Children(), "/unused", {})
        helper._private_dir = Path(tempfile.mkdtemp(prefix="loopback-helper-test-"))
        helper._sshd = Process()
        helper._processes = [helper._sshd]
        with self.assertRaisesRegex(RuntimeError, "remained alive"):
            helper.close()
        self.assertFalse(helper._private_dir)

    def test_runner_accepts_only_marked_agent_sigterm_exit_two(self):
        require_clean_exits([{"name": "loopback-ssh-agent", "returncode": 2,
                              "expected_shutdown": EXPECTED_AGENT_SHUTDOWN}])

    def test_runner_rejects_early_unrelated_forced_or_incomplete_exit_two(self):
        invalid = [
            {"name": "loopback-ssh-agent", "returncode": 2},
            {"name": "unrelated-child", "returncode": 2, "expected_shutdown": EXPECTED_AGENT_SHUTDOWN},
            {"name": "loopback-ssh-agent", "returncode": 2,
             "expected_shutdown": dict(EXPECTED_AGENT_SHUTDOWN, forced_kill=True)},
            {"name": "loopback-ssh-agent", "returncode": 2,
             "expected_shutdown": dict(EXPECTED_AGENT_SHUTDOWN, agent_socket_removed=False)},
            {"name": "loopback-ssh-agent", "returncode": -9,
             "expected_shutdown": EXPECTED_AGENT_SHUTDOWN},
        ]
        for record in invalid:
            with self.subTest(record=record), self.assertRaisesRegex(RuntimeError, "nonzero or unreaped"):
                require_clean_exits([record])

    @unittest.skipUnless(os.name == "posix" and shutil.which("ssh-agent"), "OpenSSH agent unavailable")
    def test_local_openssh_agent_expected_exit_is_recorded_only_after_auth_and_cleanup(self):
        with tempfile.TemporaryDirectory(prefix="loopback-agent-exit-test-") as directory:
            socket_path = Path(directory) / "agent.sock"
            process = subprocess.Popen(
                [shutil.which("ssh-agent"), "-D", "-a", str(socket_path)],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True,
            )
            try:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline and not socket_path.exists() and process.poll() is None:
                    time.sleep(0.02)
                self.assertTrue(socket_path.exists(), "ssh-agent did not create its private socket")
                self.assertIsNone(process.poll(), "ssh-agent exited before SIGTERM")
                record = {"name": "loopback-ssh-agent", "returncode": None}

                class ChildSupervisor:
                    items = [(process, record)]

                    def record_exit(self, child):
                        record["returncode"] = child.returncode

                    def stop(self, processes, timeout):
                        raise AssertionError("agent shutdown must use the explicit SIGTERM contract")

                helper = LoopbackCohortSsh(ChildSupervisor(), directory, {})
                helper._private_dir = Path(directory)
                helper._agent = process
                helper._agent_socket = socket_path
                helper._authenticated = True
                helper._processes = [process]
                helper.close()
                self.assertEqual(process.returncode, 2)
                self.assertFalse(socket_path.exists(), "ssh-agent left its socket behind")
                self.assertEqual(record["returncode"], 2)
                self.assertEqual(record["expected_shutdown"], EXPECTED_AGENT_SHUTDOWN)
                require_clean_exits([record])
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)


if __name__ == "__main__":
    unittest.main()
