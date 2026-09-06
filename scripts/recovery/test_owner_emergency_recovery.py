from __future__ import annotations

import argparse
import copy
import datetime as dt
import hashlib
import importlib.util
import io
import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("owner-emergency-recovery.py")
SPEC = importlib.util.spec_from_file_location("arc_owner_emergency_recovery", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
APPROVAL = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(APPROVAL)


def digest(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def utc(value: dt.datetime) -> str:
    return value.astimezone(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


class Fixture:
    def __init__(self, root: Path) -> None:
        self.root = root.resolve()
        self.root.mkdir(parents=True, exist_ok=True)
        os.chmod(self.root, 0o700)
        self.public_keys = self.root / "validator-public-keys.json"
        self.public_rows = [
            {
                "address": address,
                "public_key": f"{ordinal:064x}",
                "stake": stake,
            }
            for ordinal, (_node, address, stake) in enumerate(
                APPROVAL.PRODUCTION_VALIDATORS, start=1
            )
        ]
        self.public_raw = APPROVAL.canonical_json_bytes(self.public_rows)
        self._write_secure(self.public_keys, self.public_raw)
        self.public_sha = digest(self.public_raw)
        self.commit = "a" * 40
        self.checkpoint = "0x" + "b" * 64
        self.run_id = 987654321
        self.run_attempt = 2
        self.workflow_id = 445566
        self.artifact_id = 778899
        self.output = self.root / "OWNER-EMERGENCY-RECOVERY.json"
        now = APPROVAL.utc_now()
        self.started = now - dt.timedelta(seconds=30)
        self.approved = now - dt.timedelta(seconds=20)
        self.completed = now - dt.timedelta(seconds=10)

        validators, _ = APPROVAL.load_validator_public_keys(
            self.public_keys, self.public_sha
        )
        self.receipt_value = {
            "decision": {
                "approver": {
                    "github_login": APPROVAL.OWNER_LOGIN,
                    "github_user_id": APPROVAL.OWNER_USER_ID,
                    "repository_role": "owner",
                },
                "approved_at": utc(self.approved),
                "authority_basis": APPROVAL.AUTHORITY_BASIS,
                "authorization_kind": APPROVAL.AUTHORIZATION_KIND,
                "reason": APPROVAL.REASON,
                "reason_code": APPROVAL.REASON_CODE,
                "risk_acknowledgement": APPROVAL.RISK_ACKNOWLEDGEMENT,
            },
            "github_authentication": {
                "actor": {
                    "login": APPROVAL.OWNER_LOGIN,
                    "user_id": APPROVAL.OWNER_USER_ID,
                },
                "event": "workflow_dispatch",
                "head_branch": APPROVAL.PROTECTED_BRANCH,
                "head_sha": self.commit,
                "repository": APPROVAL.REPOSITORY,
                "run_attempt": self.run_attempt,
                "run_id": self.run_id,
                "triggering_actor": {
                    "login": APPROVAL.OWNER_LOGIN,
                    "user_id": APPROVAL.OWNER_USER_ID,
                },
                "workflow_path": APPROVAL.WORKFLOW_PATH,
            },
            "schema": APPROVAL.RECEIPT_SCHEMA,
            "scope": APPROVAL.scope_value(
                self.commit, self.checkpoint, self.public_sha
            ),
            "signing_policy": APPROVAL.signing_policy(validators),
        }
        self._materialize_inputs()

    def _write_secure(self, path: Path, raw: bytes) -> None:
        if path.exists():
            os.chmod(path, 0o600)
        path.write_bytes(raw)
        os.chmod(path, 0o400)

    def _json_file(self, name: str, value: object) -> Path:
        path = self.root / name
        self._write_secure(path, json.dumps(value, separators=(",", ":")).encode())
        return path

    def _zip_receipt(self, raw: bytes) -> bytes:
        stream = io.BytesIO()
        with zipfile.ZipFile(stream, "w", zipfile.ZIP_DEFLATED) as archive:
            archive.writestr(APPROVAL.ARTIFACT_MEMBER, raw)
        return stream.getvalue()

    def _materialize_inputs(self) -> None:
        receipt_raw = APPROVAL.canonical_json_bytes(self.receipt_value)
        zip_raw = self._zip_receipt(receipt_raw)
        self.artifact_digest = "sha256:" + digest(zip_raw)
        self.workflow = self._json_file(
            "workflow.json", {"id": self.workflow_id, "path": APPROVAL.WORKFLOW_PATH}
        )
        owner = {"login": APPROVAL.OWNER_LOGIN, "id": APPROVAL.OWNER_USER_ID}
        self.run_value = {
            "actor": owner,
            "conclusion": "success",
            "event": "workflow_dispatch",
            "head_branch": APPROVAL.PROTECTED_BRANCH,
            "head_repository": {"full_name": APPROVAL.REPOSITORY},
            "head_sha": self.commit,
            "id": self.run_id,
            "path": APPROVAL.WORKFLOW_PATH,
            "run_attempt": self.run_attempt,
            "run_started_at": utc(self.started),
            "status": "completed",
            "triggering_actor": owner,
            "updated_at": utc(self.completed),
            "workflow_id": self.workflow_id,
        }
        self.run = self._json_file("run.json", self.run_value)
        self.jobs_value = {
            "jobs": [
                {
                    "completed_at": utc(self.completed),
                    "conclusion": "success",
                    "head_sha": self.commit,
                    "name": "authenticate owner and seal exact recovery decision",
                    "run_attempt": self.run_attempt,
                    "run_id": self.run_id,
                    "started_at": utc(self.started),
                    "status": "completed",
                }
            ],
            "total_count": 1,
        }
        self.jobs = self._json_file("jobs.json", self.jobs_value)
        artifact_name = (
            f"arc-owner-emergency-recovery-{self.commit}-{self.run_id}"
            f"-attempt-{self.run_attempt}"
        )
        self.artifact_value = {
            "digest": self.artifact_digest,
            "expired": False,
            "id": self.artifact_id,
            "name": artifact_name,
            "size_in_bytes": len(zip_raw),
            "workflow_run": {"head_sha": self.commit, "id": self.run_id},
        }
        self.artifact = self._json_file("artifact.json", self.artifact_value)
        self.artifact_zip = self.root / "artifact.zip"
        self._write_secure(self.artifact_zip, zip_raw)

    def rewrite_json(self, path: Path, value: object) -> None:
        os.chmod(path, 0o600)
        path.write_bytes(json.dumps(value, separators=(",", ":")).encode())
        os.chmod(path, 0o400)

    def args(self, **overrides: object) -> argparse.Namespace:
        values: dict[str, object] = {
            "artifact_digest": self.artifact_digest,
            "artifact_id": self.artifact_id,
            "artifact_json": self.artifact,
            "artifact_zip": self.artifact_zip,
            "checkpoint_manifest_hash": self.checkpoint,
            "jobs_json": self.jobs,
            "max_age_seconds": 900,
            "output": self.output,
            "run_attempt": self.run_attempt,
            "run_id": self.run_id,
            "run_json": self.run,
            "source_main_sha": self.commit,
            "validator_public_keys": self.public_keys,
            "validator_public_keys_sha256": self.public_sha,
            "workflow_json": self.workflow,
        }
        values.update(overrides)
        return argparse.Namespace(**values)


class OwnerEmergencyRecoveryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.fixture = Fixture(Path(self.temporary.name))

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def test_cli_verifies_exact_owner_attempt_and_materializes_create_only_pair(self) -> None:
        result = subprocess.run(
            [
                sys.executable,
                str(MODULE_PATH),
                "verify-github-artifact",
                "--workflow-json",
                str(self.fixture.workflow),
                "--run-json",
                str(self.fixture.run),
                "--jobs-json",
                str(self.fixture.jobs),
                "--artifact-json",
                str(self.fixture.artifact),
                "--artifact-zip",
                str(self.fixture.artifact_zip),
                "--run-id",
                str(self.fixture.run_id),
                "--run-attempt",
                str(self.fixture.run_attempt),
                "--artifact-id",
                str(self.fixture.artifact_id),
                "--artifact-digest",
                self.fixture.artifact_digest,
                "--source-main-sha",
                self.fixture.commit,
                "--checkpoint-manifest-hash",
                self.fixture.checkpoint,
                "--validator-public-keys",
                str(self.fixture.public_keys),
                "--validator-public-keys-sha256",
                self.fixture.public_sha,
                "--output",
                str(self.fixture.output),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        status = json.loads(result.stdout)
        self.assertEqual(status["status"], "VERIFIED_GITHUB_OWNER_EMERGENCY_RECOVERY")
        self.assertEqual(
            status["sha256"], digest(self.fixture.output.read_bytes())
        )
        self.assertEqual(stat.S_IMODE(self.fixture.output.stat().st_mode), 0o400)
        sidecar = self.fixture.output.with_name(self.fixture.output.name + ".sha256")
        self.assertEqual(stat.S_IMODE(sidecar.stat().st_mode), 0o400)
        self.assertEqual(
            sidecar.read_text(encoding="ascii"),
            f"{status['sha256']}  {self.fixture.output.name}\n",
        )

    def test_exact_run_actor_triggering_actor_attempt_and_job_are_required(self) -> None:
        mutations = (
            ("run", "actor", {"login": "operator", "id": 1}, "pinned repository owner"),
            ("run", "triggering_actor", {"login": "operator", "id": 1}, "pinned repository owner"),
            ("run", "run_attempt", 1, "exact protected-main workflow attempt"),
            ("jobs", "total_count", 2, "exactly one authorization job"),
        )
        for source, field, value, message in mutations:
            with self.subTest(source=source, field=field):
                fixture = Fixture(self.fixture.root / f"case-{source}-{field}")
                target = fixture.run if source == "run" else fixture.jobs
                document = copy.deepcopy(
                    fixture.run_value if source == "run" else fixture.jobs_value
                )
                document[field] = value
                fixture.rewrite_json(target, document)
                with self.assertRaisesRegex(APPROVAL.ApprovalError, message):
                    APPROVAL.verify_github_artifact(fixture.args())

        fixture = Fixture(self.fixture.root / "case-job-attempt")
        document = copy.deepcopy(fixture.jobs_value)
        document["jobs"][0]["run_attempt"] = 1
        fixture.rewrite_json(fixture.jobs, document)
        with self.assertRaisesRegex(APPROVAL.ApprovalError, "exact workflow attempt"):
            APPROVAL.verify_github_artifact(fixture.args())

    def test_receipt_and_artifact_are_bound_to_exact_public_inputs(self) -> None:
        for override in (
            {"source_main_sha": "c" * 40},
            {"checkpoint_manifest_hash": "0x" + "d" * 64},
            {"validator_public_keys_sha256": "e" * 64},
            {"artifact_id": self.fixture.artifact_id + 1},
            {"artifact_digest": "sha256:" + "f" * 64},
        ):
            with self.subTest(override=override):
                with self.assertRaises(APPROVAL.ApprovalError):
                    APPROVAL.verify_github_artifact(self.fixture.args(**override))

    def test_tampered_zip_duplicate_member_and_unsafe_modes_fail_closed(self) -> None:
        os.chmod(self.fixture.artifact_zip, 0o600)
        with self.assertRaisesRegex(APPROVAL.ApprovalError, "mode must be"):
            APPROVAL.verify_github_artifact(self.fixture.args())
        os.chmod(self.fixture.artifact_zip, 0o400)

        fixture = Fixture(self.fixture.root / "duplicate")
        receipt_raw = APPROVAL.canonical_json_bytes(fixture.receipt_value)
        stream = io.BytesIO()
        with zipfile.ZipFile(stream, "w", zipfile.ZIP_DEFLATED) as archive:
            archive.writestr(APPROVAL.ARTIFACT_MEMBER, receipt_raw)
            archive.writestr("unexpected.json", b"{}\n")
        duplicate_raw = stream.getvalue()
        os.chmod(fixture.artifact_zip, 0o600)
        fixture.artifact_zip.write_bytes(duplicate_raw)
        os.chmod(fixture.artifact_zip, 0o400)
        fixture.artifact_digest = "sha256:" + digest(duplicate_raw)
        artifact = copy.deepcopy(fixture.artifact_value)
        artifact["digest"] = fixture.artifact_digest
        artifact["size_in_bytes"] = len(duplicate_raw)
        fixture.rewrite_json(fixture.artifact, artifact)
        with self.assertRaisesRegex(APPROVAL.ApprovalError, "must contain only"):
            APPROVAL.verify_github_artifact(fixture.args())

    def test_stale_future_or_outside_attempt_approval_fails(self) -> None:
        stale = Fixture(self.fixture.root / "stale")
        stale.receipt_value["decision"]["approved_at"] = utc(
            APPROVAL.utc_now() - dt.timedelta(hours=2)
        )
        stale._materialize_inputs()
        with self.assertRaisesRegex(APPROVAL.ApprovalError, "stale"):
            APPROVAL.verify_github_artifact(stale.args())

        outside = Fixture(self.fixture.root / "outside")
        outside.receipt_value["decision"]["approved_at"] = utc(
            outside.started - dt.timedelta(minutes=2)
        )
        outside._materialize_inputs()
        with self.assertRaisesRegex(APPROVAL.ApprovalError, "predates"):
            APPROVAL.verify_github_artifact(outside.args())

    def test_create_only_refuses_existing_receipt_or_sidecar(self) -> None:
        receipt_sha = APPROVAL.verify_github_artifact(self.fixture.args())
        with self.assertRaisesRegex(APPROVAL.ApprovalError, "already exists"):
            APPROVAL.verify_github_artifact(self.fixture.args())
        second = Fixture(self.fixture.root / "sidecar-case")
        sidecar = second.output.with_name(second.output.name + ".sha256")
        sidecar.write_text(f"{receipt_sha}  {second.output.name}\n", encoding="ascii")
        os.chmod(sidecar, 0o400)
        with self.assertRaisesRegex(APPROVAL.ApprovalError, "sidecar already exists"):
            APPROVAL.verify_github_artifact(second.args())
        self.assertFalse(second.output.exists())

    def test_schema_pins_authenticated_owner_and_recovery_boundary(self) -> None:
        schema_path = Path(__file__).with_name("owner-emergency-recovery.schema.json")
        schema = json.loads(schema_path.read_text(encoding="utf-8"))
        properties = schema["properties"]
        self.assertEqual(properties["schema"]["const"], APPROVAL.RECEIPT_SCHEMA)
        self.assertEqual(
            properties["github_authentication"]["properties"]["workflow_path"]["const"],
            APPROVAL.WORKFLOW_PATH,
        )
        self.assertEqual(
            schema["$defs"]["ownerActor"]["properties"]["user_id"]["const"],
            APPROVAL.OWNER_USER_ID,
        )
        self.assertEqual(
            properties["decision"]["properties"]["authority_basis"]["const"],
            APPROVAL.AUTHORITY_BASIS,
        )
        self.assertEqual(properties["scope"]["properties"]["source_height"]["const"], 137145)
        self.assertEqual(properties["scope"]["properties"]["transition_height"]["const"], 137146)
        self.assertEqual(
            properties["signing_policy"]["properties"]["signatures_required"]["const"],
            5,
        )


if __name__ == "__main__":
    unittest.main()
