#!/usr/bin/env python3
"""Adversarial tests for live protected-main raw Actions artifact staging."""

from __future__ import annotations

import email.utils
import hashlib
import io
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from contextlib import contextmanager
from pathlib import Path
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[2]
HELPER = REPO_ROOT / "scripts/release/protected_pretag_artifact.py"
PACKAGER = REPO_ROOT / "scripts/release/package-pretag-artifact.py"
SPEC = importlib.util.spec_from_file_location("arc_protected_pretag_test", HELPER)
assert SPEC is not None and SPEC.loader is not None
provenance = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = provenance
SPEC.loader.exec_module(provenance)

COMMIT = "a" * 40
RUN_ID = 123_456
RUN_ATTEMPT = 2
ARTIFACT_ID = 987_654
VERSION = "0.8.0"
KIND = "headless"
PLATFORM = "linux-x86_64"
NOW = 1_800_000_000


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def protected_runtime() -> tuple[Path, str, Path, str]:
    curl_candidates = (Path("/usr/bin/curl"),)
    ca_candidates = (
        Path("/etc/ssl/cert.pem"),
        Path("/etc/ssl/certs/ca-certificates.crt"),
        Path("/etc/pki/tls/certs/ca-bundle.crt"),
    )
    curl = next(path for path in curl_candidates if path.is_file() and not path.is_symlink()).resolve()
    ca = next(path for path in ca_candidates if path.is_file() and not path.is_symlink()).resolve()
    return curl, digest(curl), ca, digest(ca)


class Fixture:
    def __init__(
        self,
        root: Path,
        *,
        kind: str = KIND,
        platform: str = PLATFORM,
        artifact_id: int = ARTIFACT_ID,
    ) -> None:
        self.root = root
        self.kind = kind
        self.platform = platform
        self.artifact_id = artifact_id
        payload = root / "source-payload"
        payload.mkdir()
        for name in provenance.expected_files(kind, platform):
            (payload / name).write_bytes(f"fixture:{name}\n".encode())
        stage = root / "packaged"
        stage.mkdir()
        result = subprocess.run(
            [
                "python3",
                str(PACKAGER),
                "--kind",
                kind,
                "--platform",
                platform,
                "--repository",
                provenance.REPOSITORY,
                "--commit",
                COMMIT,
                "--run-id",
                str(RUN_ID),
                "--run-attempt",
                str(RUN_ATTEMPT),
                "--version",
                VERSION,
                "--rust-target",
                provenance.RUST_TARGETS[platform],
                "--payload-dir",
                str(payload),
                "--stage-dir",
                str(stage),
            ],
            text=True,
            capture_output=True,
            check=True,
        )
        self.artifact_name = result.stdout.strip()
        self.archive_sha256 = self.artifact_name.rsplit("-", 1)[1]
        self.zip = root / "artifact.zip"
        archive = next(stage.glob("*.tar.gz"))
        with zipfile.ZipFile(self.zip, "w", compression=zipfile.ZIP_STORED) as outer:
            outer.write(archive, archive.name)
            outer.write(stage / "SHA256SUMS", "SHA256SUMS")
        self.zip.chmod(0o400)
        self.zip_sha256 = digest(self.zip)
        self.zip_size = self.zip.stat().st_size
        self.documents = self.api_documents()

    def api_documents(self) -> dict[str, provenance.ApiDocument]:
        workflow_id = 44_321
        values = {
            "preflight workflow": {
                "id": workflow_id,
                "name": provenance.WORKFLOW_NAME,
                "path": provenance.WORKFLOW_PATH,
                "state": "active",
            },
            "protected main": {
                "name": "main",
                "protected": True,
                "commit": {"sha": COMMIT},
            },
            "preflight run": {
                "id": RUN_ID,
                "workflow_id": workflow_id,
                "run_attempt": RUN_ATTEMPT,
                "head_branch": "main",
                "head_sha": COMMIT,
                "event": "workflow_dispatch",
                "status": "completed",
                "conclusion": "success",
                "path": provenance.WORKFLOW_PATH,
                "head_repository": {"full_name": provenance.REPOSITORY},
            },
            "Actions artifact": {
                "id": self.artifact_id,
                "name": self.artifact_name,
                "expired": False,
                "size_in_bytes": self.zip_size,
                "digest": f"sha256:{self.zip_sha256}",
                "workflow_run": {
                    "id": RUN_ID,
                    "head_branch": "main",
                    "head_sha": COMMIT,
                },
            },
        }
        return {
            label: provenance.ApiDocument(
                value,
                hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest(),
                NOW - index,
                f"ABCD:{index:04X}:1234:5678",
            )
            for index, (label, value) in enumerate(values.items(), start=1)
        }

    def replace_build_metadata(self, replacement: bytes) -> None:
        with zipfile.ZipFile(self.zip, "r") as outer:
            archive_name = next(
                name for name in outer.namelist() if name.endswith(".tar.gz")
            )
            archive_bytes = outer.read(archive_name)

        rebuilt = io.BytesIO()
        with tarfile.open(fileobj=io.BytesIO(archive_bytes), mode="r:gz") as source:
            with tarfile.open(
                fileobj=rebuilt, mode="w:gz", format=tarfile.PAX_FORMAT
            ) as target:
                for member in source.getmembers():
                    extracted = source.extractfile(member)
                    payload = extracted.read() if extracted is not None else b""
                    if member.name == "BUILD-METADATA.json":
                        payload = replacement
                    member.size = len(payload)
                    target.addfile(member, io.BytesIO(payload))

        archive_bytes = rebuilt.getvalue()
        self.archive_sha256 = hashlib.sha256(archive_bytes).hexdigest()
        stem = (
            f"arc-pretag-{self.kind}-{self.platform}-{COMMIT}-"
            f"{RUN_ID}-{RUN_ATTEMPT}"
        )
        archive_name = f"{stem}.tar.gz"
        checksums = "\n".join(
            (
                "# ARC pre-tag artifact v1",
                f"# kind={self.kind}",
                f"# repository={provenance.REPOSITORY}",
                f"# commit={COMMIT}",
                f"# run_id={RUN_ID}",
                f"# run_attempt={RUN_ATTEMPT}",
                f"# platform={self.platform}",
                f"{self.archive_sha256}  {archive_name}",
                "",
            )
        ).encode()
        self.artifact_name = f"{stem}-{self.archive_sha256}"
        self.zip.chmod(0o600)
        with zipfile.ZipFile(self.zip, "w", compression=zipfile.ZIP_STORED) as outer:
            outer.writestr(archive_name, archive_bytes)
            outer.writestr("SHA256SUMS", checksums)
        self.zip.chmod(0o400)
        self.zip_sha256 = digest(self.zip)
        self.zip_size = self.zip.stat().st_size
        self.documents = self.api_documents()

    @contextmanager
    def verify(self):
        curl, curl_sha, ca, ca_sha = protected_runtime()
        with mock.patch.object(
            provenance.CurlApiClient,
            "get_json",
            side_effect=lambda _endpoint, *, label: self.documents[label],
        ):
            with provenance.verified_protected_pretag_artifact(
                raw_actions_zip=self.zip,
                expected_commit=COMMIT,
                expected_run_id=RUN_ID,
                expected_run_attempt=RUN_ATTEMPT,
                expected_artifact_id=self.artifact_id,
                kind=self.kind,
                platform=self.platform,
                expected_version=VERSION,
                curl=curl,
                curl_sha256=curl_sha,
                ca_bundle=ca,
                ca_bundle_sha256=ca_sha,
                now=NOW,
            ) as verified:
                yield verified


class ProtectedArtifactTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="arc-live-artifact-test.")
        self.addCleanup(self.temporary.cleanup)
        self.fixture = Fixture(Path(self.temporary.name))

    def test_exact_live_tuple_raw_zip_and_payloads_are_privately_staged(self) -> None:
        with self.fixture.verify() as verified:
            self.assertEqual(provenance.PROVENANCE_SCHEMA, verified.provenance["schema"])
            self.assertEqual(COMMIT, verified.provenance["live"]["commit"])
            self.assertEqual(ARTIFACT_ID, verified.provenance["live"]["artifact_id"])
            self.assertEqual(self.fixture.zip_sha256, verified.provenance["artifact"]["raw_actions_zip_sha256"])
            self.assertEqual(self.fixture.archive_sha256, verified.provenance["artifact"]["archive_sha256"])
            self.assertEqual(
                set(provenance.expected_files(KIND, PLATFORM)), set(verified.payloads)
            )
            self.assertEqual(0o700, verified.transaction_root.stat().st_mode & 0o777)
            self.assertEqual(0o400, verified.provenance_path.stat().st_mode & 0o777)
            self.assertEqual(verified.provenance_bytes, provenance.canonical_json(verified.provenance))
            final = verified.recheck()
            self.assertEqual(provenance.PROVENANCE_SCHEMA, final.value["schema"])
            self.assertEqual(verified.provenance["artifact"], final.value["artifact"])
            self.assertEqual(verified.provenance["live"]["artifact_id"], final.value["live"]["artifact_id"])
            self.assertIsNotNone(final.path)
            assert final.path is not None
            self.assertEqual(0o400, final.path.stat().st_mode & 0o777)
            root = verified.transaction_root
        self.assertFalse(root.exists())

    def test_rehashed_semantic_build_metadata_must_still_be_canonical(self) -> None:
        with zipfile.ZipFile(self.fixture.zip, "r") as outer:
            archive_name = next(
                name for name in outer.namelist() if name.endswith(".tar.gz")
            )
            with tarfile.open(
                fileobj=io.BytesIO(outer.read(archive_name)), mode="r:gz"
            ) as archive:
                metadata_file = archive.extractfile("BUILD-METADATA.json")
                assert metadata_file is not None
                metadata = json.loads(metadata_file.read())
        noncanonical = (
            json.dumps(metadata, sort_keys=True, indent=2) + "\n"
        ).encode()
        self.assertNotEqual(noncanonical, provenance.canonical_json(metadata))
        self.fixture.replace_build_metadata(noncanonical)

        with self.assertRaisesRegex(
            provenance.ProvenanceError,
            "pre-tag BUILD-METADATA is not canonical JSON",
        ):
            with self.fixture.verify():
                pass

    def test_forged_local_receipt_is_not_an_authorization_input(self) -> None:
        forged = self.fixture.root / "MATERIALIZATION-RECEIPT.json"
        forged.write_text('{"schema":"forged"}\n')
        forged.chmod(0o400)
        with self.fixture.verify() as verified:
            self.assertNotIn("MATERIALIZATION-RECEIPT.json", verified.payloads)
            self.assertNotIn(str(forged), verified.provenance_bytes.decode())

    def test_api_rejects_unprotected_main_wrong_event_expiry_and_cross_run(self) -> None:
        cases = (
            ("protected main", "protected", False, "current protected main"),
            ("preflight run", "event", "push", "completed successful"),
            ("Actions artifact", "expired", True, "expiry/run binding"),
            ("Actions artifact", "workflow_run", {"id": RUN_ID + 1}, "expiry/run binding"),
        )
        for label, field, value, expected in cases:
            with self.subTest(label=label, field=field):
                original = self.fixture.documents[label]
                changed = dict(original.value)
                changed[field] = value
                self.fixture.documents[label] = provenance.ApiDocument(
                    changed, original.body_sha256, original.response_unix, original.request_id
                )
                with self.assertRaisesRegex(provenance.ProvenanceError, expected):
                    with self.fixture.verify():
                        pass
                self.fixture.documents[label] = original

    def test_raw_zip_digest_size_symlink_and_hardlink_are_rejected(self) -> None:
        self.fixture.zip.chmod(0o600)
        with self.fixture.zip.open("ab") as handle:
            handle.write(b"tamper")
        self.fixture.zip.chmod(0o400)
        with self.assertRaisesRegex(provenance.ProvenanceError, "server digest or size"):
            with self.fixture.verify():
                pass

        self.tearDown()
        self.setUp()
        linked = self.fixture.root / "artifact-second-link.zip"
        os.link(self.fixture.zip, linked)
        with self.assertRaisesRegex(provenance.ProvenanceError, "one protected"):
            with self.fixture.verify():
                pass

        linked.unlink()
        symlink = self.fixture.root / "artifact-symlink.zip"
        symlink.symlink_to(self.fixture.zip)
        original = self.fixture.zip
        self.fixture.zip = symlink
        with self.assertRaises(provenance.ProvenanceError):
            with self.fixture.verify():
                pass
        self.fixture.zip = original

    def test_unknown_outer_membership_and_payload_hash_tamper_are_rejected(self) -> None:
        self.fixture.zip.chmod(0o600)
        with zipfile.ZipFile(self.fixture.zip, "a", compression=zipfile.ZIP_STORED) as outer:
            outer.writestr("LOCAL-RECEIPT.json", b"forged\n")
        self.fixture.zip.chmod(0o400)
        artifact = self.fixture.documents["Actions artifact"]
        value = dict(artifact.value)
        value["size_in_bytes"] = self.fixture.zip.stat().st_size
        value["digest"] = f"sha256:{digest(self.fixture.zip)}"
        self.fixture.documents["Actions artifact"] = provenance.ApiDocument(
            value, artifact.body_sha256, artifact.response_unix, artifact.request_id
        )
        with self.assertRaisesRegex(provenance.ProvenanceError, "exactly SHA256SUMS"):
            with self.fixture.verify():
                pass

    def test_curl_client_is_config_proxy_redirect_token_free_and_bounded(self) -> None:
        curl, curl_sha, ca, ca_sha = protected_runtime()
        root = self.fixture.root / "api-root"
        root.mkdir(mode=0o700)
        captured: dict[str, object] = {}

        def fake_run(command: list[str], **kwargs):
            captured["command"] = command
            captured["kwargs"] = kwargs
            header = Path(command[command.index("--dump-header") + 1])
            body = Path(command[command.index("--output") + 1])
            response_date = email.utils.formatdate(NOW, usegmt=True)
            header.write_bytes(
                f"HTTP/2 200\r\ndate: {response_date}\r\ncache-control: public, max-age=60\r\nage: 0\r\nx-github-request-id: ABCD:1234:5678:9ABC\r\n\r\n".encode()
            )
            body.write_bytes(b'{"id":1}\n')
            return subprocess.CompletedProcess(command, 0, b"", b"")

        client = provenance.CurlApiClient(
            curl, curl_sha, ca, ca_sha, root, now=NOW
        )
        endpoint = (
            f"/repos/{provenance.REPOSITORY}/actions/workflows/"
            "release-signing-preflight.yml"
        )
        commands: list[list[str]] = []

        def capture_run(command: list[str], **kwargs):
            result = fake_run(command, **kwargs)
            commands.append(command)
            return result

        with mock.patch.object(
            provenance.os,
            "urandom",
            side_effect=(b"a" * 32, b"b" * 32),
        ), mock.patch.object(provenance.subprocess, "run", side_effect=capture_run):
            document = client.get_json(endpoint, label="fixture")
            client.get_json(endpoint, label="fixture")
        self.assertEqual({"id": 1}, document.value)
        command = captured["command"]
        assert isinstance(command, list)
        self.assertEqual("-q", command[1])
        self.assertIn("/dev/null", command)
        self.assertIn("--max-redirs", command)
        self.assertNotIn("--location", command)
        self.assertNotIn("-L", command)
        self.assertIn("Authorization:", command)
        self.assertIn("Cache-Control: no-cache, no-store, max-age=0", command)
        self.assertIn("Pragma: no-cache", command)
        self.assertFalse(any("token " in value.lower() for value in command))
        urls = [item[-1] for item in commands]
        self.assertEqual(2, len(set(urls)))
        for url in urls:
            self.assertRegex(
                url,
                rf"^{re.escape(provenance.API_ORIGIN + endpoint)}\?_arc_proof=[0-9a-f]{{64}}$",
            )
        kwargs = captured["kwargs"]
        assert isinstance(kwargs, dict)
        environment = kwargs["env"]
        self.assertEqual({"HOME", "PATH", "LANG", "LC_ALL"}, set(environment))
        self.assertNotIn("HTTP_PROXY", environment)

    def test_curl_client_preserves_existing_query_and_reserves_proof_nonce(self) -> None:
        curl, curl_sha, ca, ca_sha = protected_runtime()
        root = self.fixture.root / "api-query-root"
        root.mkdir(mode=0o700)
        captured_url: list[str] = []

        def fake_run(command: list[str], **_kwargs):
            captured_url.append(command[-1])
            header = Path(command[command.index("--dump-header") + 1])
            body = Path(command[command.index("--output") + 1])
            response_date = email.utils.formatdate(NOW, usegmt=True)
            header.write_bytes(
                f"HTTP/2 200\r\ndate: {response_date}\r\n"
                "cache-control: public, max-age=60, s-maxage=60\r\nage: 0\r\n"
                "x-github-request-id: ABCD:1234:5678:9ABC\r\n\r\n".encode()
            )
            body.write_bytes(b'{"total_count":0,"artifacts":[]}\n')
            return subprocess.CompletedProcess(command, 0, b"", b"")

        client = provenance.CurlApiClient(
            curl, curl_sha, ca, ca_sha, root, now=NOW
        )
        endpoint = f"/repos/{provenance.REPOSITORY}/actions/runs/1/artifacts?per_page=100"
        with mock.patch.object(
            provenance.os, "urandom", return_value=b"c" * 32
        ), mock.patch.object(provenance.subprocess, "run", side_effect=fake_run):
            client.get_json(endpoint, label="fixture")
        self.assertRegex(
            captured_url[0],
            rf"^{re.escape(provenance.API_ORIGIN + endpoint)}&_arc_proof=[0-9a-f]{{64}}$",
        )
        with self.assertRaisesRegex(provenance.ProvenanceError, "reserved proof nonce"):
            client.get_json(
                f"/repos/{provenance.REPOSITORY}/branches/main?_arc_proof={'a' * 64}",
                label="fixture",
            )
        with mock.patch.object(
            provenance.os, "urandom", side_effect=OSError("entropy unavailable")
        ), self.assertRaisesRegex(provenance.ProvenanceError, "entropy is unavailable"):
            client.get_json(
                f"/repos/{provenance.REPOSITORY}/branches/main",
                label="fixture",
            )

    def test_curl_client_rejects_stale_or_multiple_http_responses(self) -> None:
        stale = (
            "HTTP/2 200\r\n"
            f"date: {email.utils.formatdate(NOW - 301, usegmt=True)}\r\n"
            "x-github-request-id: ABCD:1234:5678:9ABC\r\n\r\n"
        ).encode()
        with self.assertRaisesRegex(provenance.ProvenanceError, "stale"):
            provenance.CurlApiClient._parse_headers(stale, now=NOW)
        multiple = (
            "HTTP/1.1 200 Connection established\r\n\r\n"
            f"HTTP/2 200\r\ndate: {email.utils.formatdate(NOW, usegmt=True)}\r\n"
            "x-github-request-id: ABCD:1234:5678:9ABC\r\n\r\n"
        ).encode()
        with self.assertRaisesRegex(provenance.ProvenanceError, "multiple"):
            provenance.CurlApiClient._parse_headers(multiple, now=NOW)
        aged = (
            "HTTP/2 200\r\n"
            f"date: {email.utils.formatdate(NOW, usegmt=True)}\r\n"
            "cache-control: public, max-age=60\r\nage: 1\r\n"
            "x-github-request-id: ABCD:1234:5678:9ABC\r\n\r\n"
        ).encode()
        with self.assertRaisesRegex(provenance.ProvenanceError, "positively aged cache"):
            provenance.CurlApiClient._parse_headers(aged, now=NOW)

    def test_final_recheck_fails_if_protected_main_or_artifact_tuple_moves(self) -> None:
        with self.fixture.verify() as verified:
            original = self.fixture.documents["protected main"]
            moved = dict(original.value)
            moved["commit"] = {"sha": "b" * 40}
            self.fixture.documents["protected main"] = provenance.ApiDocument(
                moved, original.body_sha256, original.response_unix, original.request_id
            )
            with self.assertRaisesRegex(provenance.ProvenanceError, "current protected main"):
                verified.recheck()

    def test_standalone_final_live_reproof_returns_second_full_canonical_provenance(self) -> None:
        curl, curl_sha, ca, ca_sha = protected_runtime()
        with self.fixture.verify() as verified:
            initial = verified.provenance_bytes
        with mock.patch.object(
            provenance.CurlApiClient,
            "get_json",
            side_effect=lambda _endpoint, *, label: self.fixture.documents[label],
        ):
            final = provenance.final_live_reproof(
                initial_provenance_bytes=initial,
                expected_commit=COMMIT,
                expected_run_id=RUN_ID,
                expected_run_attempt=RUN_ATTEMPT,
                expected_artifact_id=ARTIFACT_ID,
                kind=KIND,
                platform=PLATFORM,
                expected_version=VERSION,
                curl=curl,
                curl_sha256=curl_sha,
                ca_bundle=ca,
                ca_bundle_sha256=ca_sha,
                now=NOW,
            )
        self.assertEqual(provenance.PROVENANCE_SCHEMA, final.value["schema"])
        self.assertEqual(verified.provenance["artifact"], final.value["artifact"])
        self.assertEqual(final.canonical_bytes, provenance.canonical_json(final.value))

    def test_exact_nine_artifact_set_uses_four_plus_four_requests_with_branch_last(self) -> None:
        set_parent = self.fixture.root / "set-fixtures"
        set_parent.mkdir()
        fixtures: list[Fixture] = []
        rows: list[dict[str, object]] = []
        artifact_ids: list[int] = []
        for index, (kind, platform) in enumerate(provenance.PRETAG_GROUPS):
            group_root = set_parent / f"{index:02d}"
            group_root.mkdir()
            artifact_id = ARTIFACT_ID + index
            fixture = Fixture(
                group_root,
                kind=kind,
                platform=platform,
                artifact_id=artifact_id,
            )
            fixtures.append(fixture)
            artifact_ids.append(artifact_id)
            rows.append(
                {
                    "raw_actions_zip": fixture.zip,
                    "expected_artifact_id": artifact_id,
                    "kind": kind,
                    "platform": platform,
                }
            )

        first = fixtures[0].documents
        listing_value = {
            "total_count": len(fixtures),
            "artifacts": [fixture.documents["Actions artifact"].value for fixture in fixtures],
        }
        documents = {
            "preflight workflow": first["preflight workflow"],
            "preflight run": first["preflight run"],
            "Actions artifact set": provenance.ApiDocument(
                listing_value,
                hashlib.sha256(
                    json.dumps(listing_value, sort_keys=True).encode()
                ).hexdigest(),
                NOW - 3,
                "ABCD:0003:1234:5678",
            ),
            "protected main": first["protected main"],
        }
        calls: list[tuple[str, str]] = []

        def get_json(client, endpoint: str, *, label: str):
            calls.append((endpoint, label))
            if len(calls) > 60:
                raise AssertionError("anonymous GitHub rate limit exceeded")
            client.counter += 1
            return documents[label]

        curl, curl_sha, ca, ca_sha = protected_runtime()
        with mock.patch.object(
            provenance.CurlApiClient, "get_json", autospec=True, side_effect=get_json
        ):
            with provenance.pretag_actions_set_proof(
                rows=rows,
                expected_commit=COMMIT,
                expected_run_id=RUN_ID,
                expected_run_attempt=RUN_ATTEMPT,
                expected_version=VERSION,
                curl=curl,
                curl_sha256=curl_sha,
                ca_bundle=ca,
                ca_bundle_sha256=ca_sha,
                now=NOW,
            ) as verified_set:
                self.assertEqual(4, verified_set.api_request_count)
                self.assertEqual(9, len(verified_set.artifacts))
                self.assertEqual(
                    ["preflight workflow", "preflight run", "Actions artifact set", "protected main"],
                    [label for _, label in calls],
                )
                self.assertEqual(
                    f"/repos/{provenance.REPOSITORY}/actions/runs/{RUN_ID}/attempts/{RUN_ATTEMPT}",
                    calls[1][0],
                )
                self.assertIn("?per_page=100", calls[2][0])
                initial_bytes = tuple(
                    verified.provenance_bytes for verified in verified_set.artifacts
                )
                shared_api = verified_set.artifacts[0].provenance["api"]
                self.assertTrue(
                    all(
                        verified.provenance["api"] == shared_api
                        for verified in verified_set.artifacts
                    )
                )
                self.assertEqual(
                    ["workflow", "run", "artifact_set", "protected_main"],
                    [row["label"] for row in shared_api["responses"]],
                )
                final = verified_set.recheck()
                self.assertEqual(8, final.api_request_count)
                self.assertEqual(9, len(final.proofs))
                self.assertEqual(
                    [
                        "preflight workflow",
                        "preflight run",
                        "Actions artifact set",
                        "protected main",
                    ]
                    * 2,
                    [label for _, label in calls],
                )
                self.assertTrue(
                    all(
                        proof.value["artifact"]
                        == verified.provenance["artifact"]
                        for proof, verified in zip(final.proofs, verified_set.artifacts)
                    )
                )
                transaction_root = verified_set.transaction_root
            self.assertFalse(transaction_root.exists())

            calls.clear()
            standalone = provenance.final_live_set_reproof(
                initial_provenance_bytes_list=initial_bytes,
                expected_commit=COMMIT,
                expected_run_id=RUN_ID,
                expected_run_attempt=RUN_ATTEMPT,
                expected_artifact_ids=artifact_ids,
                expected_version=VERSION,
                curl=curl,
                curl_sha256=curl_sha,
                ca_bundle=ca,
                ca_bundle_sha256=ca_sha,
                now=NOW,
            )
        self.assertEqual(4, standalone.api_request_count)
        self.assertEqual(9, len(standalone.proofs))
        self.assertEqual("protected main", calls[-1][1])
        self.assertEqual(4, len(calls))

    def test_set_rejects_noncanonical_rows_and_incomplete_or_extra_listing(self) -> None:
        rows = [
            {"artifact_id": ARTIFACT_ID + index, "kind": kind, "platform": platform}
            for index, (kind, platform) in enumerate(provenance.PRETAG_GROUPS)
        ]
        reversed_rows = list(reversed(rows))
        with self.assertRaisesRegex(provenance.ProvenanceError, "order, group"):
            provenance._normalize_live_set_rows(reversed_rows)

        documents = self.fixture.documents
        listing = provenance.ApiDocument(
            {
                "total_count": 10,
                "artifacts": [documents["Actions artifact"].value] * 9,
            },
            "f" * 64,
            NOW,
            "ABCD:9999:1234:5678",
        )
        client = mock.Mock()
        client.get_json.side_effect = lambda _endpoint, *, label: {
            "preflight workflow": documents["preflight workflow"],
            "preflight run": documents["preflight run"],
            "Actions artifact set": listing,
            "protected main": documents["protected main"],
        }[label]
        with self.assertRaisesRegex(provenance.ProvenanceError, "exactly nine"):
            provenance.prove_live_api_set(
                client,
                commit=COMMIT,
                run_id=RUN_ID,
                run_attempt=RUN_ATTEMPT,
                rows=rows,
            )
        self.assertEqual(3, client.get_json.call_count)


if __name__ == "__main__":
    unittest.main(verbosity=2)
