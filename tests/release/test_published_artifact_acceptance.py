#!/usr/bin/env python3
"""Adversarial tests for the public post-release artifact boundary."""

from __future__ import annotations

import copy
import hashlib
import importlib.util
import json
import tempfile
import unittest
import zipfile
from argparse import Namespace
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = REPO_ROOT / "scripts" / "release" / "published-artifact-acceptance.py"
SPEC = importlib.util.spec_from_file_location("published_artifact_acceptance", MODULE_PATH)
assert SPEC and SPEC.loader
acceptance = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(acceptance)


class PublishedArtifactAcceptanceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.commit = "a" * 40
        self.repository = "FerrumVir/arc-chain"
        self.run_id = 987654
        self.run_attempt = 3
        self.original_legacy = copy.deepcopy(acceptance.LEGACY_SOURCE)
        self.addCleanup(self.restore_legacy)

        self.asset_bytes = {
            name: f"public bytes for {name}\n".encode()
            for name in acceptance.EXPECTED_RELEASE_ASSETS
        }
        self.release = {
            "id": 777,
            "tag_name": "v0.8.0",
            "target_commitish": self.commit,
            "draft": False,
            "prerelease": False,
            "immutable": True,
            "author": {"login": "github-actions[bot]"},
            "assets": [
                {
                    "id": index + 1000,
                    "name": name,
                    "size": len(self.asset_bytes[name]),
                    "digest": "sha256:"
                    + hashlib.sha256(self.asset_bytes[name]).hexdigest(),
                    "state": "uploaded",
                    "uploader": {"login": "github-actions[bot]"},
                    "browser_download_url": (
                        f"https://github.com/{self.repository}/releases/download/"
                        f"v0.8.0/{name}"
                    ),
                }
                for index, name in enumerate(sorted(self.asset_bytes))
            ],
        }

        legacy_bytes = b"real fixture stand-in selected by immutable server id\n"
        acceptance.LEGACY_SOURCE = copy.deepcopy(acceptance.LEGACY_SOURCE)
        acceptance.LEGACY_SOURCE["asset"] = {
            "id": 432306066,
            "name": "arc-node-linux-x86_64",
            "size": len(legacy_bytes),
            "sha256": hashlib.sha256(legacy_bytes).hexdigest(),
        }
        self.legacy_bytes = legacy_bytes
        self.legacy_release = {
            "id": acceptance.LEGACY_SOURCE["release_id"],
            "tag_name": "v0.7.7",
            "assets": [
                {
                    "id": acceptance.LEGACY_SOURCE["asset"]["id"],
                    "name": "arc-node-linux-x86_64",
                    "size": len(legacy_bytes),
                    "digest": "sha256:" + hashlib.sha256(legacy_bytes).hexdigest(),
                    "state": "uploaded",
                    "browser_download_url": (
                        f"https://github.com/{self.repository}/releases/download/"
                        "v0.7.7/arc-node-linux-x86_64"
                    ),
                }
            ],
        }
        self.jobs = [
            {
                "id": 5000 + index,
                "name": name,
                "run_id": self.run_id,
                "run_attempt": self.run_attempt,
                "head_sha": self.commit,
                "status": "completed",
                "conclusion": (
                    "skipped" if name == acceptance.SKIPPED_RELEASE_JOB else "success"
                ),
            }
            for index, name in enumerate(
                sorted(
                    set(acceptance.EXPECTED_RELEASE_JOBS)
                    | {acceptance.SKIPPED_RELEASE_JOB}
                )
            )
        ]
        self.published_evidence_bytes = json.dumps(
            self.release, sort_keys=True, separators=(",", ":")
        ).encode("utf-8")
        self.published_download = self.root / "published-download"
        self.published_download.mkdir()
        self.published_zip = self.published_download / "published-evidence.zip"
        self.write_published_zip()
        self.write_api_documents()

    def restore_legacy(self) -> None:
        acceptance.LEGACY_SOURCE = self.original_legacy

    def write_json(self, name: str, value: object) -> Path:
        path = self.root / name
        path.write_text(json.dumps(value), encoding="utf-8")
        return path

    def write_published_zip(self, extra_member: bool = False) -> None:
        with zipfile.ZipFile(self.published_zip, "w", zipfile.ZIP_STORED) as archive:
            archive.writestr(
                acceptance.PUBLISHED_EVIDENCE_MEMBER, self.published_evidence_bytes
            )
            if extra_member:
                archive.writestr("unexpected.txt", b"unexpected")

    def published_artifact(self) -> dict[str, object]:
        evidence_sha = hashlib.sha256(self.published_evidence_bytes).hexdigest()
        return {
            "id": 5555,
            "name": (
                f"arc-release-published-evidence-{self.commit}-{self.run_id}-"
                f"{self.run_attempt}-{self.release['id']}-{evidence_sha}"
            ),
            "digest": "sha256:" + hashlib.sha256(self.published_zip.read_bytes()).hexdigest(),
            "size_in_bytes": self.published_zip.stat().st_size,
            "expired": False,
            "workflow_run": {"id": self.run_id, "head_sha": self.commit},
        }

    def write_api_documents(self) -> None:
        self.workflow_json = self.write_json(
            "workflow.json",
            {"id": 123, "path": ".github/workflows/release.yml", "state": "active"},
        )
        self.run_json = self.write_json(
            "run.json",
            {
                "id": self.run_id,
                "run_attempt": self.run_attempt,
                "workflow_id": 123,
                "head_repository": {"full_name": self.repository},
                "path": ".github/workflows/release.yml",
                "event": "workflow_dispatch",
                "head_branch": "main",
                "head_sha": self.commit,
                "status": "completed",
                "conclusion": "success",
            },
        )
        self.jobs_json = self.write_json(
            "jobs.json", {"total_count": len(self.jobs), "jobs": self.jobs}
        )
        self.tag_ref_json = self.write_json(
            "tag-ref.json",
            {
                "ref": "refs/tags/v0.8.0",
                "object": {"type": "commit", "sha": self.commit},
            },
        )
        self.release_json = self.write_json("release.json", self.release)
        self.legacy_ref_json = self.write_json(
            "legacy-ref.json",
            {
                "ref": "refs/tags/v0.7.7",
                "object": {
                    "type": "commit",
                    "sha": acceptance.LEGACY_SOURCE["commit"],
                },
            },
        )
        self.legacy_release_json = self.write_json(
            "legacy-release.json", self.legacy_release
        )
        self.published_artifact_json = self.write_json(
            "published-evidence-artifact.json", self.published_artifact()
        )

    def bind(self) -> Path:
        output = self.root / "binding.json"
        self.jobs_output = self.root / "release-attempt-jobs.json"
        self.published_evidence_output = self.root / "release-published.json"
        self.published_evidence_zip_output = self.root / "retained-published-evidence.zip"
        self.published_artifact_output = self.root / "published-evidence-selection.json"
        acceptance.command_bind(
            Namespace(
                repository=self.repository,
                tag="v0.8.0",
                commit=self.commit,
                release_run_id=self.run_id,
                release_run_attempt=self.run_attempt,
                workflow_json=self.workflow_json,
                run_json=self.run_json,
                jobs_json=self.jobs_json,
                tag_ref_json=self.tag_ref_json,
                release_json=self.release_json,
                published_evidence_artifact_json=self.published_artifact_json,
                published_evidence_download_root=self.published_download,
                legacy_tag_ref_json=self.legacy_ref_json,
                legacy_release_json=self.legacy_release_json,
                jobs_output=self.jobs_output,
                published_evidence_output=self.published_evidence_output,
                published_evidence_zip_output=self.published_evidence_zip_output,
                published_evidence_artifact_output=self.published_artifact_output,
                output=output,
            )
        )
        return output

    def test_binds_exact_successful_attempt_immutable_release_and_legacy_id(self) -> None:
        binding = json.loads(self.bind().read_text(encoding="utf-8"))
        self.assertEqual(binding["release_workflow"]["run_id"], self.run_id)
        self.assertEqual(binding["release_workflow"]["run_attempt"], 3)
        self.assertEqual(
            binding["release_workflow"]["jobs_sha256"],
            acceptance.sha256_file(self.jobs_output),
        )
        self.assertEqual(binding["release"], {"id": 777, "immutable": True})
        self.assertEqual(binding["published_evidence"]["artifact_id"], 5555)
        self.assertEqual(
            binding["published_evidence"]["artifact_digest"],
            self.published_artifact()["digest"],
        )
        self.assertEqual(
            binding["published_evidence"]["release_published_sha256"],
            hashlib.sha256(self.published_evidence_bytes).hexdigest(),
        )
        self.assertEqual(
            self.published_evidence_zip_output.read_bytes(), self.published_zip.read_bytes()
        )
        self.assertEqual(
            binding["legacy_source"]["asset"]["id"],
            acceptance.LEGACY_SOURCE["asset"]["id"],
        )
        self.assertEqual(set(binding["assets"]), acceptance.EXPECTED_RELEASE_ASSETS)

    def test_selects_only_one_exact_attempt_publication_evidence_artifact(self) -> None:
        artifacts_json = self.write_json(
            "artifacts.json",
            {
                "artifacts": [
                    {**self.published_artifact(), "id": 5554, "name": "unrelated"},
                    self.published_artifact(),
                ]
            },
        )
        output = self.root / "selected-published-evidence.json"
        arguments = Namespace(
            commit=self.commit,
            release_run_id=self.run_id,
            release_run_attempt=self.run_attempt,
            release_json=self.release_json,
            artifacts_json=artifacts_json,
            output=output,
        )
        acceptance.command_select_published_evidence(arguments)
        self.assertEqual(json.loads(output.read_text())["id"], 5555)

        duplicate = self.published_artifact()
        duplicate["id"] = 5556
        artifacts_json.write_text(
            json.dumps({"artifacts": [self.published_artifact(), duplicate]}),
            encoding="utf-8",
        )
        with self.assertRaisesRegex(acceptance.AcceptanceError, "exactly one"):
            acceptance.command_select_published_evidence(arguments)

    def test_rejects_rerun_release_replacement_and_moved_legacy_tag(self) -> None:
        cases = (
            (self.run_json, "run_attempt", 4, "release run attempt mismatch"),
            (self.release_json, "immutable", False, "release immutable state mismatch"),
        )
        for path, field, replacement, message in cases:
            value = json.loads(path.read_text(encoding="utf-8"))
            original = value[field]
            value[field] = replacement
            path.write_text(json.dumps(value), encoding="utf-8")
            with self.assertRaisesRegex(acceptance.AcceptanceError, message):
                self.bind()
            value[field] = original
            path.write_text(json.dumps(value), encoding="utf-8")

        ref = json.loads(self.legacy_ref_json.read_text(encoding="utf-8"))
        ref["object"]["sha"] = "b" * 40
        self.legacy_ref_json.write_text(json.dumps(ref), encoding="utf-8")
        with self.assertRaisesRegex(acceptance.AcceptanceError, "v0.7.7 tag commit"):
            self.bind()

    def test_rejects_partial_or_nonterminal_exact_release_attempt_jobs(self) -> None:
        original = {"total_count": len(self.jobs), "jobs": copy.deepcopy(self.jobs)}
        mutations = []

        missing = copy.deepcopy(original)
        missing["jobs"].pop()
        mutations.append(("missing job", missing, "captured count"))

        duplicate = copy.deepcopy(original)
        duplicate["jobs"][1]["name"] = duplicate["jobs"][0]["name"]
        mutations.append(("duplicate job", duplicate, "duplicate release-attempt job"))

        failed = copy.deepcopy(original)
        required = next(
            job
            for job in failed["jobs"]
            if job["name"] != acceptance.SKIPPED_RELEASE_JOB
        )
        required["conclusion"] = "failure"
        mutations.append(("failed required job", failed, "conclusion mismatch"))

        cleanup_success = copy.deepcopy(original)
        cleanup = next(
            job
            for job in cleanup_success["jobs"]
            if job["name"] == acceptance.SKIPPED_RELEASE_JOB
        )
        cleanup["conclusion"] = "success"
        mutations.append(("cleanup ran", cleanup_success, "conclusion mismatch"))

        wrong_attempt = copy.deepcopy(original)
        wrong_attempt["jobs"][0]["run_attempt"] = self.run_attempt - 1
        mutations.append(("wrong attempt", wrong_attempt, "run attempt mismatch"))

        wrong_sha = copy.deepcopy(original)
        wrong_sha["jobs"][0]["head_sha"] = "b" * 40
        mutations.append(("wrong SHA", wrong_sha, "commit mismatch"))

        truncated = copy.deepcopy(original)
        truncated["total_count"] += 1
        mutations.append(("truncated page", truncated, "total_count mismatch"))

        for label, value, message in mutations:
            with self.subTest(label=label):
                self.jobs_json.write_text(json.dumps(value), encoding="utf-8")
                with self.assertRaisesRegex(acceptance.AcceptanceError, message):
                    self.bind()
        self.jobs_json.write_text(json.dumps(original), encoding="utf-8")

    def test_rejects_unbound_or_tampered_published_evidence_artifact(self) -> None:
        original = self.published_artifact()
        mutations = []

        expired = copy.deepcopy(original)
        expired["expired"] = True
        mutations.append(("expired", expired, "expiration mismatch"))

        wrong_run = copy.deepcopy(original)
        wrong_run["workflow_run"]["id"] = self.run_id + 1
        mutations.append(("wrong run", wrong_run, "run id mismatch"))

        wrong_sha = copy.deepcopy(original)
        wrong_sha["workflow_run"]["head_sha"] = "b" * 40
        mutations.append(("wrong SHA", wrong_sha, "commit mismatch"))

        wrong_digest = copy.deepcopy(original)
        wrong_digest["digest"] = "sha256:" + "f" * 64
        mutations.append(("wrong ZIP digest", wrong_digest, "ZIP digest mismatch"))

        wrong_name = copy.deepcopy(original)
        wrong_name["name"] = str(wrong_name["name"]).replace(
            f"-{self.run_attempt}-", f"-{self.run_attempt + 1}-", 1
        )
        mutations.append(("wrong attempt name", wrong_name, "not exact-attempt bound"))

        for label, value, message in mutations:
            with self.subTest(label=label):
                self.published_artifact_json.write_text(
                    json.dumps(value), encoding="utf-8"
                )
                with self.assertRaisesRegex(acceptance.AcceptanceError, message):
                    self.bind()
        self.published_artifact_json.write_text(json.dumps(original), encoding="utf-8")

    def test_rejects_unsafe_or_noncausal_published_evidence_zip(self) -> None:
        self.write_published_zip(extra_member=True)
        self.published_artifact_json.write_text(
            json.dumps(self.published_artifact()), encoding="utf-8"
        )
        with self.assertRaisesRegex(acceptance.AcceptanceError, "exactly"):
            self.bind()

        with zipfile.ZipFile(self.published_zip, "w", zipfile.ZIP_STORED) as archive:
            archive.writestr("../release-published.json", self.published_evidence_bytes)
        self.published_artifact_json.write_text(
            json.dumps(self.published_artifact()), encoding="utf-8"
        )
        with self.assertRaisesRegex(acceptance.AcceptanceError, "unsafe or invalid"):
            self.bind()

        self.published_evidence_bytes = json.dumps(
            {**self.release, "name": "different captured evidence"},
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
        self.write_published_zip()
        stale_name = self.published_artifact()
        stale_name["name"] = self.published_artifact()["name"].rsplit("-", 1)[0] + "-" + "0" * 64
        self.published_artifact_json.write_text(json.dumps(stale_name), encoding="utf-8")
        with self.assertRaisesRegex(acceptance.AcceptanceError, "content-name digest"):
            self.bind()

    def test_rejects_asset_digest_id_url_and_name_set_tampering(self) -> None:
        mutations = (
            ("digest", "sha256:not-a-digest", "malformed SHA-256"),
            ("id", 0, "positive integer"),
            ("browser_download_url", "https://example.invalid/a", "URL mismatch"),
        )
        for field, replacement, message in mutations:
            release = copy.deepcopy(self.release)
            original = release["assets"][0][field]
            release["assets"][0][field] = replacement
            self.release_json.write_text(json.dumps(release), encoding="utf-8")
            with self.assertRaisesRegex(acceptance.AcceptanceError, message):
                self.bind()
            release["assets"][0][field] = original
        self.release_json.write_text(json.dumps(self.release), encoding="utf-8")
        release = copy.deepcopy(self.release)
        release["assets"].pop()
        self.release_json.write_text(json.dumps(release), encoding="utf-8")
        with self.assertRaisesRegex(acceptance.AcceptanceError, "asset set mismatch"):
            self.bind()

    def test_windows_receipt_distinguishes_exact_msi_and_embedded_versions(self) -> None:
        checks = dict(acceptance.REQUIRED_CHECKS["windows-x86_64"])
        checks["embedded_app_product_version"] = "0.8.0.0"
        acceptance.validate_platform_checks("windows-x86_64", checks)
        for replacement in ("0.8.0-beta", "0.8.0.1"):
            with self.subTest(embedded=replacement):
                checks["embedded_app_product_version"] = replacement
                with self.assertRaisesRegex(
                    acceptance.AcceptanceError, "embedded_app_product_version"
                ):
                    acceptance.validate_platform_checks("windows-x86_64", checks)
        checks["embedded_app_product_version"] = "0.8.0"
        checks["msi_product_version"] = "0.8.0.1"
        with self.assertRaisesRegex(acceptance.AcceptanceError, "msi_product_version"):
            acceptance.validate_platform_checks("windows-x86_64", checks)

    def test_download_verifier_and_aggregate_fail_closed(self) -> None:
        binding_path = self.bind()
        binding = json.loads(binding_path.read_text(encoding="utf-8"))
        downloads = self.root / "downloads"
        downloads.mkdir()
        for name in acceptance.EXPECTED_COMPONENTS["linux-x86_64"]:
            (downloads / name).write_bytes(self.asset_bytes[name])
        files_receipt = self.root / "files.json"
        acceptance.command_verify_files(
            Namespace(
                binding=binding_path,
                directory=downloads,
                asset=sorted(acceptance.EXPECTED_COMPONENTS["linux-x86_64"]),
                output=files_receipt,
            )
        )
        self.assertEqual(
            set(json.loads(files_receipt.read_text())["assets"]),
            acceptance.EXPECTED_COMPONENTS["linux-x86_64"],
        )
        (downloads / "install.sh").write_bytes(b"tampered")
        with self.assertRaisesRegex(acceptance.AcceptanceError, "downloaded size"):
            acceptance.command_verify_files(
                Namespace(
                    binding=binding_path,
                    directory=downloads,
                    asset=["install.sh"],
                    output=files_receipt,
                )
            )

        component_dir = self.root / "components"
        component_dir.mkdir()
        binding_sha = acceptance.sha256_file(binding_path)
        for platform, names in acceptance.EXPECTED_COMPONENTS.items():
            checks = dict(acceptance.REQUIRED_CHECKS[platform])
            if platform == "windows-x86_64":
                checks["embedded_app_product_version"] = "0.8.0"
            component = {
                "acceptance_run_attempt": 2,
                "acceptance_run_id": 2468,
                "assets": {name: binding["assets"][name] for name in sorted(names)},
                "binding_sha256": binding_sha,
                "checks": checks,
                "commit": binding["commit"],
                "platform": platform,
                "release_id": binding["release"]["id"],
                "release_run_attempt": binding["release_workflow"]["run_attempt"],
                "release_run_id": binding["release_workflow"]["run_id"],
                "repository": binding["repository"],
                "schema": "arc.published-artifact-acceptance-component.v1",
                "tag": binding["tag"],
            }
            (component_dir / f"{platform}.json").write_text(
                json.dumps(component), encoding="utf-8"
            )
        component_artifacts = self.root / "component-artifacts.json"
        component_artifacts.write_text(
            json.dumps(
                {
                    "schema": "arc.published-artifact-component-binding.v1",
                    "repository": self.repository,
                    "acceptance_run_id": 2468,
                    "acceptance_run_attempt": 2,
                    "artifacts": {
                        platform: {
                            "name": (
                                f"arc-published-acceptance-{platform}-2468-attempt-2"
                            ),
                            "id": 9000 + index,
                            "digest": "sha256:" + f"{index + 1:064x}",
                            "size": 100 + index,
                            "expired": False,
                            "workflow_run_id": 2468,
                            "head_sha": binding["commit"],
                        }
                        for index, platform in enumerate(
                            sorted(acceptance.EXPECTED_COMPONENTS)
                        )
                    },
                }
            ),
            encoding="utf-8",
        )
        evidence_root = self.root / "evidence"
        for relative in acceptance.EXPECTED_EVIDENCE_FILES:
            path = evidence_root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(f"retained evidence: {relative}\n".encode("utf-8"))
        (evidence_root / "release/release-attempt-jobs.json").write_bytes(
            self.jobs_output.read_bytes()
        )
        (evidence_root / "release/release-published.json").write_bytes(
            self.published_evidence_output.read_bytes()
        )
        (evidence_root / "release/published-evidence.zip").write_bytes(
            self.published_evidence_zip_output.read_bytes()
        )
        (evidence_root / "release/published-evidence-artifact.json").write_bytes(
            self.published_artifact_output.read_bytes()
        )
        evidence_manifest = self.root / "EVIDENCE-MANIFEST.json"
        acceptance.command_evidence_manifest(
            Namespace(
                binding=binding_path,
                component_artifacts=component_artifacts,
                evidence_root=evidence_root,
                acceptance_run_id=2468,
                acceptance_run_attempt=2,
                output=evidence_manifest,
            )
        )
        output = self.root / "accepted.json"
        acceptance.command_aggregate(
            Namespace(
                binding=binding_path,
                component_artifacts=component_artifacts,
                components=component_dir,
                evidence_manifest=evidence_manifest,
                evidence_root=evidence_root,
                acceptance_run_id=2468,
                acceptance_run_attempt=2,
                output=output,
            )
        )
        final = json.loads(output.read_text(encoding="utf-8"))
        self.assertEqual(final["schema"], "arc.published-artifact-acceptance.v1")
        self.assertEqual(final["verified_platforms"], sorted(acceptance.EXPECTED_COMPONENTS))
        self.assertEqual(final["evidence_file_count"], len(acceptance.EXPECTED_EVIDENCE_FILES))
        self.assertEqual(
            final["evidence_manifest_sha256"],
            acceptance.sha256_file(evidence_manifest),
        )

        identities = json.loads(component_artifacts.read_text(encoding="utf-8"))
        identities["artifacts"]["macos-arm64"]["digest"] = "sha256:mutable"
        component_artifacts.write_text(json.dumps(identities), encoding="utf-8")
        with self.assertRaisesRegex(acceptance.AcceptanceError, "artifact digest"):
            acceptance.command_aggregate(
                Namespace(
                    binding=binding_path,
                    component_artifacts=component_artifacts,
                    components=component_dir,
                    evidence_manifest=evidence_manifest,
                    evidence_root=evidence_root,
                    acceptance_run_id=2468,
                    acceptance_run_attempt=2,
                    output=output,
                )
            )
        identities["artifacts"]["macos-arm64"]["digest"] = "sha256:" + f"{2:064x}"
        component_artifacts.write_text(json.dumps(identities), encoding="utf-8")

        linux_path = component_dir / "linux-x86_64.json"
        linux = json.loads(linux_path.read_text(encoding="utf-8"))
        linux["checks"]["legacy_history_preserved"] = False
        linux_path.write_text(json.dumps(linux), encoding="utf-8")
        with self.assertRaisesRegex(acceptance.AcceptanceError, "legacy_history_preserved"):
            acceptance.command_aggregate(
                Namespace(
                    binding=binding_path,
                    component_artifacts=component_artifacts,
                    components=component_dir,
                    evidence_manifest=evidence_manifest,
                    evidence_root=evidence_root,
                    acceptance_run_id=2468,
                    acceptance_run_attempt=2,
                    output=output,
                )
            )

        linux["checks"]["legacy_history_preserved"] = True
        linux_path.write_text(json.dumps(linux), encoding="utf-8")
        evidence_path = evidence_root / "macos-arm64/desktop.stdout"
        original_evidence = evidence_path.read_bytes()
        evidence_path.write_bytes(b"tampered after the evidence manifest was sealed")
        with self.assertRaisesRegex(acceptance.AcceptanceError, "file manifest mismatch"):
            acceptance.command_aggregate(
                Namespace(
                    binding=binding_path,
                    component_artifacts=component_artifacts,
                    components=component_dir,
                    evidence_manifest=evidence_manifest,
                    evidence_root=evidence_root,
                    acceptance_run_id=2468,
                    acceptance_run_attempt=2,
                    output=output,
                )
            )

        evidence_path.write_bytes(original_evidence)
        symlink = evidence_root / "macos-arm64/unexpected-link"
        symlink.symlink_to(evidence_path)
        with self.assertRaisesRegex(acceptance.AcceptanceError, "only regular files"):
            acceptance.collect_evidence_files(evidence_root)


if __name__ == "__main__":
    unittest.main(verbosity=2)
