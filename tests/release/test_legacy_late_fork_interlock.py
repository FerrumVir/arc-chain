#!/usr/bin/env python3
from __future__ import annotations

import datetime as dt
import copy
import importlib.util
import io
import json
import os
import pathlib
import socket
import stat
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock


REPO = pathlib.Path(__file__).resolve().parents[2]
TOOL = REPO / "scripts" / "recovery" / "legacy-late-fork-interlock.py"
SPEC = importlib.util.spec_from_file_location("arc_legacy_late_fork_interlock", TOOL)
assert SPEC is not None and SPEC.loader is not None
interlock = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = interlock
SPEC.loader.exec_module(interlock)


def write(path: pathlib.Path, payload: bytes, mode: int = 0o400) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(payload)
        handle.flush()
        os.fsync(handle.fileno())


class LegacyLateForkInterlockTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix=".late-fork-test-", dir=REPO)
        self.runtime_temporary = tempfile.TemporaryDirectory(
            prefix=".arc-lfi-runtime-", dir="/tmp"
        )
        self.root = pathlib.Path(self.temporary.name)
        self.root.chmod(0o700)
        self.runtime = pathlib.Path(self.runtime_temporary.name)
        self.runtime.chmod(0o750)
        if os.name == "posix":
            os.chown(self.runtime, -1, os.getegid())
        self.boundary = self.root / "legacy-maintenance-boundary.json"
        self.output = self.root / "legacy-late-fork-source-set.json"
        self.state = self.root / "state"
        self.commit = "a" * 40
        self.boundary_value = {
            "schema": "arc.recovery.legacy-maintenance-boundary.v1",
            "source_main_commit": self.commit,
            "observed_cutoff_height": 138_236,
            "official_origin_scope": {
                "global_absence_claimed": False,
                "origins": [
                    {"node": name, "host": host, "origin": origin}
                    for name, host, origin in interlock.OFFICIAL
                ],
            },
        }
        write(self.boundary, interlock.canonical(self.boundary_value))
        self.boundary_sha = interlock.digest(self.boundary.read_bytes())
        self.tool_sha = interlock.digest(TOOL.read_bytes())
        self.source_set, self.source_sha = interlock.build_source_set(
            self.boundary, self.boundary_sha, self.output, self.tool_sha
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()
        self.runtime_temporary.cleanup()

    @staticmethod
    def unreachable(source: dict, scope: str) -> dict:
        return {
            "name": source["name"],
            "origin": source["origin"],
            "scope": scope,
            "outcome": "unreachable",
            "height": None,
            "block_hash": None,
            "state_root": None,
            "response_sha256": None,
        }

    @staticmethod
    def observed(source: dict, scope: str, height: int) -> dict:
        return {
            "name": source["name"],
            "origin": source["origin"],
            "scope": scope,
            "outcome": "observed",
            "height": height,
            "block_hash": "b" * 64,
            "state_root": "c" * 64,
            "response_sha256": {
                "info_before": "1" * 64,
                "latest": "2" * 64,
                "exact": "3" * 64,
                "info_after": "4" * 64,
            },
        }

    @staticmethod
    def inconsistent(source: dict, scope: str) -> dict:
        row = LegacyLateForkInterlockTests.observed(source, scope, 1)
        row.update(
            {
                "outcome": "inconsistent",
                "height": None,
                "block_hash": None,
                "state_root": None,
            }
        )
        return row

    def test_build_and_validate_exact_source_set(self) -> None:
        loaded, observed_sha = interlock.load_source_set(
            self.output,
            expected_sha256=self.source_sha,
            expected_boundary_sha256=self.boundary_sha,
            expected_tool_sha256=self.tool_sha,
        )
        self.assertEqual(loaded, self.source_set)
        self.assertEqual(observed_sha, self.source_sha)
        self.assertEqual(len(loaded["monitored_retired_origins"]), 6)
        self.assertEqual(loaded["monitored_community_origins"], [])
        self.assertEqual(
            loaded["validation_mode"],
            "capture-bound-retirement-tripwire-offline-validation-required",
        )
        self.assertFalse(loaded["global_absence_claimed"])
        sidecar = self.output.with_name(self.output.name + ".sha256")
        self.assertEqual(
            sidecar.read_bytes(), f"{self.source_sha}  {self.output.name}\n".encode()
        )
        self.assertEqual(stat.S_IMODE(self.output.stat().st_mode), 0o400)
        self.assertEqual(stat.S_IMODE(sidecar.stat().st_mode), 0o400)

    def test_unreachable_retired_sources_are_honest_healthy_observations(self) -> None:
        with mock.patch.object(interlock, "observe_source", side_effect=self.unreachable):
            status = interlock.sample_once(
                self.source_set, self.source_sha, self.tool_sha, self.state
            )
        self.assertEqual(status["state"], "HEALTHY")
        self.assertEqual(
            status["gate_reason"], "capture-bound-retirement-tripwire-clear"
        )
        self.assertIsNone(status["incident_sha256"])
        self.assertEqual(status["required_community_observations"], 0)
        self.assertEqual(status["healthy_community_observations"], 0)
        self.assertTrue(all(row["outcome"] == "unreachable" for row in status["observations"]))
        self.assertFalse(status["global_absence_claimed"])
        raw = (self.state / "STATUS.json").read_bytes()
        self.assertEqual(
            interlock.validate_status(
                raw,
                source_set=self.source_set,
                source_set_sha=self.source_sha,
                tool_sha=self.tool_sha,
            ),
            status,
        )

    def test_post_cutoff_candidate_is_immutable_and_never_auto_clears(self) -> None:
        calls = 0

        def candidate(source: dict, scope: str) -> dict:
            nonlocal calls
            calls += 1
            row = self.unreachable(source, scope)
            if calls == 1:
                row = self.observed(
                    source,
                    scope,
                    self.source_set["observed_cutoff_height"] + 1,
                )
            return row

        with mock.patch.object(interlock, "observe_source", side_effect=candidate):
            first = interlock.sample_once(
                self.source_set, self.source_sha, self.tool_sha, self.state
            )
        self.assertEqual(first["state"], "MAINTENANCE")
        self.assertEqual(first["gate_reason"], "latched-legacy-source-incident")
        incident_sha = first["incident_sha256"]
        incident = self.state / "incidents" / f"{incident_sha}.json"
        self.assertTrue(incident.is_file())
        self.assertEqual(stat.S_IMODE(incident.stat().st_mode), 0o400)
        incident_bytes = incident.read_bytes()

        with mock.patch.object(interlock, "observe_source", side_effect=self.unreachable):
            second = interlock.sample_once(
                self.source_set, self.source_sha, self.tool_sha, self.state
            )
        self.assertEqual(second["state"], "MAINTENANCE")
        self.assertEqual(second["incident_sha256"], incident_sha)
        self.assertEqual(incident.read_bytes(), incident_bytes)

    def test_inconsistent_retired_response_latches_an_immutable_incident(self) -> None:
        first_name = self.source_set["monitored_retired_origins"][0]["name"]

        def retired_inconsistent(source: dict, scope: str) -> dict:
            if scope == "retired" and source["name"] == first_name:
                return self.inconsistent(source, scope)
            return self.unreachable(source, scope)

        with mock.patch.object(
            interlock, "observe_source", side_effect=retired_inconsistent
        ):
            status = interlock.sample_once(
                self.source_set, self.source_sha, self.tool_sha, self.state
            )
        self.assertEqual(status["state"], "MAINTENANCE")
        self.assertEqual(status["gate_reason"], "latched-legacy-source-incident")
        self.assertIsNotNone(status["incident_sha256"])
        incident = json.loads(
            (
                self.state
                / "incidents"
                / f"{status['incident_sha256']}.json"
            ).read_bytes()
        )
        self.assertEqual(incident["candidate"]["outcome"], "inconsistent")
        self.assertEqual(incident["candidate"]["scope"], "retired")
        self.assertFalse(incident["global_absence_claimed"])

    def test_community_unavailability_is_transient_but_post_cutoff_is_latched(self) -> None:
        source_set = copy.deepcopy(self.source_set)
        source_set["monitored_community_origins"] = [
            {"name": "community-one", "origin": "https://community.example:443"}
        ]
        source_sha = interlock.digest(interlock.canonical(source_set))

        with mock.patch.object(interlock, "observe_source", side_effect=self.unreachable):
            unavailable = interlock.sample_once(
                source_set, source_sha, self.tool_sha, self.state
            )
        self.assertEqual(unavailable["state"], "MAINTENANCE")
        self.assertEqual(
            unavailable["gate_reason"], "community-source-observation-unavailable"
        )
        self.assertIsNone(unavailable["incident_sha256"])
        self.assertEqual(unavailable["required_community_observations"], 1)
        self.assertEqual(unavailable["healthy_community_observations"], 0)

        def community_ready(source: dict, scope: str) -> dict:
            if scope == "community":
                return self.observed(
                    source, scope, source_set["observed_cutoff_height"]
                )
            return self.unreachable(source, scope)

        with mock.patch.object(interlock, "observe_source", side_effect=community_ready):
            recovered = interlock.sample_once(
                source_set, source_sha, self.tool_sha, self.state
            )
        self.assertEqual(recovered["state"], "HEALTHY")
        self.assertEqual(recovered["healthy_community_observations"], 1)
        self.assertIsNone(recovered["incident_sha256"])

        def community_candidate(source: dict, scope: str) -> dict:
            if scope == "community":
                return self.observed(
                    source, scope, source_set["observed_cutoff_height"] + 1
                )
            return self.unreachable(source, scope)

        with mock.patch.object(
            interlock, "observe_source", side_effect=community_candidate
        ):
            candidate = interlock.sample_once(
                source_set, source_sha, self.tool_sha, self.state
            )
        self.assertEqual(candidate["state"], "MAINTENANCE")
        self.assertEqual(candidate["gate_reason"], "latched-legacy-source-incident")
        self.assertIsNotNone(candidate["incident_sha256"])

    def test_retired_source_at_cutoff_is_an_incident_and_expiry_fails_closed(self) -> None:
        def at_cutoff(source: dict, scope: str) -> dict:
            row = self.unreachable(source, scope)
            row.update(
                {
                    "outcome": "observed",
                    "height": self.source_set["observed_cutoff_height"],
                    "block_hash": "d" * 64,
                    "state_root": "e" * 64,
                    "response_sha256": {
                        "info_before": "1" * 64,
                        "latest": "2" * 64,
                        "exact": "3" * 64,
                        "info_after": "4" * 64,
                    },
                }
            )
            return row

        with mock.patch.object(interlock, "observe_source", side_effect=at_cutoff):
            status = interlock.sample_once(
                self.source_set, self.source_sha, self.tool_sha, self.state
            )
        self.assertEqual(status["state"], "MAINTENANCE")
        self.assertEqual(status["gate_reason"], "latched-legacy-source-incident")
        self.assertIsNotNone(status["incident_sha256"])
        raw = (self.state / "STATUS.json").read_bytes()
        expired_now = interlock.parse_utc(status["expires_at"], "test expiry") + dt.timedelta(
            seconds=1
        )
        with self.assertRaisesRegex(interlock.InterlockError, "expired"):
            interlock.validate_status(
                raw,
                source_set=self.source_set,
                source_set_sha=self.source_sha,
                tool_sha=self.tool_sha,
                now=expired_now,
            )

    @unittest.skipUnless(os.name == "posix", "Unix-domain sockets require POSIX")
    def test_unix_listener_is_permission_sealed_and_serves_the_gate(self) -> None:
        listener = self.runtime / "gate.sock"

        stale = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        stale.bind(os.fspath(listener))
        stale.close()
        listener.chmod(0o660)
        with mock.patch.object(interlock, "UNIX_RUNTIME_ROOT", pathlib.Path("/tmp")):
            interlock.prepare_unix_listener(listener)
        self.assertFalse(os.path.lexists(listener))

        captured: dict[str, object] = {}
        errors: list[BaseException] = []
        server_type = interlock.ThreadedUnixServer

        class CapturingServer(server_type):
            def __init__(self, *args, **kwargs):  # type: ignore[no-untyped-def]
                super().__init__(*args, **kwargs)
                captured["server"] = self

        def run() -> None:
            try:
                interlock.serve(
                    listen_unix=listener,
                    source_set=self.source_set,
                    source_set_sha=self.source_sha,
                    tool_sha=self.tool_sha,
                    state_root=self.state,
                )
            except BaseException as error:  # pragma: no cover - surfaced below
                errors.append(error)

        with (
            mock.patch.object(interlock, "UNIX_RUNTIME_ROOT", pathlib.Path("/tmp")),
            mock.patch.object(interlock, "ThreadedUnixServer", CapturingServer),
            mock.patch.object(
                interlock, "observe_source", side_effect=self.unreachable
            ),
        ):
            worker = threading.Thread(target=run, daemon=True)
            worker.start()
            deadline = time.monotonic() + 5
            while (
                not os.path.lexists(listener) or not (self.state / "STATUS.json").exists()
            ) and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(os.path.lexists(listener), errors)
            self.assertEqual(stat.S_IMODE(listener.lstat().st_mode), 0o660)
            with self.assertRaisesRegex(interlock.InterlockError, "still accepting"):
                interlock.prepare_unix_listener(listener)

            client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                client.settimeout(2)
                client.connect(os.fspath(listener))
                client.sendall(b"GET /gate HTTP/1.0\r\nHost: localhost\r\n\r\n")
                response = bytearray()
                while True:
                    chunk = client.recv(4096)
                    if not chunk:
                        break
                    response.extend(chunk)
            finally:
                client.close()
            self.assertIn(b" 204 ", bytes(response).splitlines()[0])

            server = captured.get("server")
            self.assertIsNotNone(server)
            server.shutdown()  # type: ignore[union-attr]
            worker.join(timeout=5)
            self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])
        self.assertFalse(os.path.lexists(listener))

        listener.write_text("not a socket", encoding="utf-8")
        listener.chmod(0o660)
        with mock.patch.object(interlock, "UNIX_RUNTIME_ROOT", pathlib.Path("/tmp")):
            with self.assertRaisesRegex(interlock.InterlockError, "identity"):
                interlock.prepare_unix_listener(listener)

    def test_serve_parser_rejects_the_removed_tcp_listener(self) -> None:
        arguments = [
            "serve",
            "--source-set",
            os.fspath(self.output),
            "--source-set-sha256",
            self.source_sha,
            "--boundary-sha256",
            self.boundary_sha,
            "--tool-sha256",
            self.tool_sha,
            "--state-root",
            os.fspath(self.state),
            "--listen",
            "127.0.0.1:18081",
        ]
        with mock.patch("sys.stderr", new=io.StringIO()):
            with self.assertRaises(SystemExit):
                interlock.parser().parse_args(arguments)

    def test_tamper_symlink_and_wrong_tool_fail_closed(self) -> None:
        with self.assertRaisesRegex(interlock.InterlockError, "selected root"):
            interlock.load_source_set(
                self.output,
                expected_sha256="f" * 64,
                expected_boundary_sha256=self.boundary_sha,
                expected_tool_sha256=self.tool_sha,
            )

        linked = self.root / "linked-source-set.json"
        linked.symlink_to(self.output)
        with self.assertRaisesRegex(interlock.InterlockError, "open|identity"):
            interlock.load_source_set(
                linked,
                expected_sha256=self.source_sha,
                expected_boundary_sha256=self.boundary_sha,
                expected_tool_sha256=self.tool_sha,
            )

        with self.assertRaisesRegex(interlock.InterlockError, "identity/policy"):
            interlock.load_source_set(
                self.output,
                expected_sha256=self.source_sha,
                expected_boundary_sha256=self.boundary_sha,
                expected_tool_sha256="f" * 64,
            )


if __name__ == "__main__":
    unittest.main()
