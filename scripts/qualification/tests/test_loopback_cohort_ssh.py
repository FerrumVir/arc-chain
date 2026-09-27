from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock

from scripts.qualification.loopback_cohort_ssh import LoopbackCohortSsh


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
        helper._sshd, helper._agent = sshd, agent
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


if __name__ == "__main__":
    unittest.main()
